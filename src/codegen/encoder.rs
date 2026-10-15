//! x86-64 指令选择与机器码发射。
//!
//! 把 SSA 指令映射为 x86-64 指令编码（AT&T 语义、Intel 字节序），
//! 生成 ELF / Mach-O / PE 三种容器共用的 `.text` 字节流。
//!
//! 设计参考：
//! - Rust：LLVM 的 X86 后端使用指令选择与寄存器分配。
//! - Zig：自带后端，直接生成机器码。
//! - Python：CPython 使用栈式字节码（无此层）。
//!
//! 寄存器分配策略（简化）：
//! - 采用**虚拟寄存器分配**：每个 SSA value 映射到一个固定物理寄存器，
//!   在 10 个可用 GPR 之间循环复用。
//! - `rax` / `rcx` / `rdx` 保留为**隐式暂存器**：`div`、`idiv` 与移位计数
//!   分别固定使用 rdx:rax 与 cl，若参与分配会让这些指令读到被破坏的值。
//! - 这不是最优分配（会无谓地溢出/重读），但能正确运行。
//! - 后续可替换为真正的线性扫描或图着色分配器。
//!
//! 启动约定随平台变化（见 [`emit_start_wrapper`]）：Linux 直接用系统调用
//! 退出进程；macOS 与 Windows 复用运行时的 `zr_exit`。

use crate::codegen::target::{Arch, Os, Target};
use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaValue};

/// x86-64 通用寄存器（System V ABI / Microsoft x64 ABI 共用编号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gpr {
    Rax, Rcx, Rdx, Rsi, Rdi, R8, R9, R10, R11, R12, R13, R14, R15,
}

impl Gpr {
    /// ModR/M `reg` 字段的 3 位编码（REX.B 为 0 时使用）。
    fn bits(self) -> u8 {
        use Gpr::*;
        match self {
            Rax => 0, Rcx => 1, Rdx => 2, Rsi => 6, Rdi => 7,
            R8 => 0, R9 => 1, R10 => 2, R11 => 3,
            R12 => 4, R13 => 5, R14 => 6, R15 => 7,
        }
    }

    /// 是否需要 REX.B / REX.R 扩展位（R8–R15 需要）。
    fn rex_bit(self) -> bool {
        matches!(self, Gpr::R8 | Gpr::R9 | Gpr::R10 | Gpr::R11 | Gpr::R12 | Gpr::R13 | Gpr::R14 | Gpr::R15)
    }
}

/// 隐式暂存寄存器（不参与分配）。
const SCRATCH_AX: Gpr = Gpr::Rax;
const SCRATCH_CX: Gpr = Gpr::Rcx;
const SCRATCH_DX: Gpr = Gpr::Rdx;

/// 可用于分配的 GPR（不含 RSP/RBP 栈指针/帧指针，也不含三个暂存器）。
/// 注意：R12–R15 属于 callee-save，此处仅作为通用分配池，
/// 真实实现应在函数序言/尾声中保存/恢复 callee-save 寄存器。
/// SysV AMD64 调用约定的整数实参寄存器（前 6 个）。
const ARG_REGS: [Gpr; 6] = [Gpr::Rdi, Gpr::Rsi, SCRATCH_DX, SCRATCH_CX, Gpr::R8, Gpr::R9];

const ALLOCATABLE: [Gpr; 10] = [
    Gpr::Rsi, Gpr::Rdi, Gpr::R8, Gpr::R9, Gpr::R10, Gpr::R11, Gpr::R12, Gpr::R13, Gpr::R14, Gpr::R15,
];

/// 把 SSA value 映射到物理寄存器（虚拟分配）。
fn alloc_reg(v: SsaValue) -> Gpr {
    ALLOCATABLE[(v.id() as usize) % ALLOCATABLE.len()]
}

/// 发射一个函数的机器码。
pub fn emit_function(target: &Target, unit: &mut CompilationUnit, func: &SsaFunction) {
    debug_assert_eq!(target.arch(), Arch::X86_64, "x86-64 后端只能用于 x86_64 目标");
    let start = unit.code.len();

    // SSA 基本块是只读的，采用两遍策略：先为每个块占位一个符号，
    // 发射时记录块起始偏移，发射结束后统一回填。
    let sym_base = unit.symbols.len();
    for block in &func.blocks {
        unit.symbols.push(Symbol {
            name: format!("bb{}", block.id),
            section: SymbolSection::Text,
            offset: 0, // 稍后回填
            size: 0,
            global: false,
            kind: SymbolKind::Section,
        });
    }

    let mut block_offsets: Vec<usize> = Vec::with_capacity(func.blocks.len());

    // 函数序言：push rbp; mov rbp, rsp
    emit1(unit, 0x55);
    emit3(unit, 0x48, 0x89, 0xe5);

    for block in &func.blocks {
        block_offsets.push(unit.code.len());
        for instr in &block.instrs {
            emit_instr(unit, instr);
        }
        match &block.term {
            SsaTerminator::Return(ret_val) => {
                // 若有返回值，先 mov rax, <reg>
                if let Some(v) = ret_val {
                    emit_mov_rr(unit, SCRATCH_AX, alloc_reg(*v));
                }
                // 尾声：mov rsp, rbp; pop rbp; ret
                emit3(unit, 0x48, 0x89, 0xec);
                emit1(unit, 0x5d);
                emit1(unit, 0xc3);
            }
            SsaTerminator::Goto(target) => emit_jmp(unit, &format!("bb{target}")),
            SsaTerminator::Branch { cond, then_block, else_block } => {
                // test cond, cond; jz else; jmp then
                let r = alloc_reg(*cond);
                emit_test_rr(unit, r, r);
                emit_jcc_rel32(unit, 0x84, &format!("bb{else_block}")); // jz
                emit_jmp(unit, &format!("bb{then_block}"));
            }
            SsaTerminator::Unreachable => emit_ud2(unit),
        }
    }

    // 回填每个基本块符号的偏移。
    for (i, &off) in block_offsets.iter().enumerate() {
        if let Some(sym) = unit.symbols.get_mut(sym_base + i) {
            sym.offset = off;
        }
    }

    unit.symbols.push(Symbol {
        name: func.name.clone(),
        section: SymbolSection::Text,
        offset: start,
        size: unit.code.len() - start,
        global: true,
        kind: SymbolKind::Function,
    });
}

/// 发射一条 SSA 指令。
fn emit_instr(unit: &mut CompilationUnit, instr: &SsaInstr) {
    match instr {
        SsaInstr::Const { result, value } => emit_const(unit, *result, value.clone()),
        SsaInstr::Binary { result, op, lhs, rhs } => emit_binary(unit, *result, *op, *lhs, *rhs),
        SsaInstr::Unary { result, op, operand } => emit_unary(unit, *result, *op, *operand),
        SsaInstr::Call { result, callee, args } => {
            emit_call_args(unit, args);
            emit_call(unit, callee);
            emit_mov_rr(unit, alloc_reg(*result), SCRATCH_AX);
        }
        SsaInstr::Var { .. } => {
            // Var 是 φ 合并前的引用，运行时无需动作
        }
    }
}

/// 发射常量加载。
fn emit_const(unit: &mut CompilationUnit, result: SsaValue, value: SsaConst) {
    let dst = alloc_reg(result);
    match value {
        SsaConst::Int(i) => {
            // movabs（REX.W + 0xB8 + rd + imm64）加载 64 位立即数
            let rex = 0x48u8 | if dst.rex_bit() { 0x01 } else { 0 };
            emit1(unit, rex);
            emit1(unit, 0xb8 + dst.bits());
            emit8(unit, i as u64);
        }
        // bool / unit 在通用寄存器里就是 0/1；浮点与字符串后端暂未实现
        SsaConst::Bool(b) => emit_const_bool(unit, dst.bits(), b),
        SsaConst::Unit => emit_const_int(unit, dst.bits(), 0),
        SsaConst::Float(_) | SsaConst::Str(_) => emit_const_int(unit, dst.bits(), 0),
    }
}

/// 用 mov 立即数加载一个小整数（0/1 之类）。
fn emit_const_int(unit: &mut CompilationUnit, dst: u8, value: u64) {
    if value == 0 {
        // xor reg, reg —— 比 mov 短，且不破坏标志位
        emit1(unit, 0x31);
        emit1(unit, 0xc0 | (dst << 3) | dst);
    } else {
        emit1(unit, 0x48 | if dst >= 8 { 0x04 } else { 0x00 });
        emit1(unit, 0xb8 + (dst & 7));
        emit8(unit, value);
    }
}

/// 加载布尔常量。
fn emit_const_bool(unit: &mut CompilationUnit, dst: u8, value: bool) {
    emit_const_int(unit, dst, u64::from(value));
}

/// 发射二元运算。
fn emit_binary(unit: &mut CompilationUnit, result: SsaValue, op: SsaBinaryOp, lhs: SsaValue, rhs: SsaValue) {
    let dst = alloc_reg(result);
    let l = alloc_reg(lhs);
    let r = alloc_reg(rhs);

    use SsaBinaryOp::*;
    match op {
        // 算术与位运算：dst = l OP r
        Add | Sub | BitAnd | BitOr | BitXor | And | Or => {
            if dst != l {
                emit_mov_rr(unit, dst, l);
            }
            if let Some(code) = alu_opcode(op) {
                emit_full_alu(unit, code, dst, r);
            }
        }
        // 乘法 / 除法 / 取模 / 移位：需要专用指令编码
        Mul => emit_mul(unit, dst, l, r),
        Div => emit_divrem(unit, dst, l, r, false),
        Rem => emit_divrem(unit, dst, l, r, true),
        Shl => emit_shift(unit, 4, dst, l, r), // shl r/m64, cl
        Shr => emit_shift(unit, 5, dst, l, r), // shr r/m64, cl
        // 比较运算：cmp l, r; setcc al; movzx dst, al
        Eq | Ne | Lt | Gt | Le | Ge => {
            emit_cmp_rr(unit, l, r);
            emit_setcc(unit, condition_code(op), SCRATCH_AX);
            emit_movzx(unit, dst, SCRATCH_AX);
        }
    }
}

/// 发射一元运算。
fn emit_unary(unit: &mut CompilationUnit, result: SsaValue, op: crate::ssa::SsaUnaryOp, operand: SsaValue) {
    let dst = alloc_reg(result);
    let r = alloc_reg(operand);

    use crate::ssa::SsaUnaryOp::*;
    match op {
        Not => {
            // test r, r; sete al; movzx dst, al
            emit_test_rr(unit, r, r);
            emit_setcc(unit, 0x94, SCRATCH_AX); // sete
            emit_movzx(unit, dst, SCRATCH_AX);
        }
        BitNot => {
            // mov dst, r; not dst
            if dst != r {
                emit_mov_rr(unit, dst, r);
            }
            emit1(unit, 0x48);
            emit1(unit, 0xf7);
            modrm(unit, 3, 2, dst.bits()); // /2 = NOT
        }
        Neg => {
            // mov dst, r; neg dst
            if dst != r {
                emit_mov_rr(unit, dst, r);
            }
            emit1(unit, 0x48);
            emit1(unit, 0xf7);
            modrm(unit, 3, 3, dst.bits()); // /3 = NEG
        }
    }
}

/// 返回 ALU 指令的编码分类（对应 `emit_full_alu`）：
/// 0=add, 1=or, 4=and, 5=sub, 6=xor。
/// 对于 mul/div/rem/shl/shr 这类不能用简单 MR 形式编码的运算，
/// 返回 `None`，由调用者走专用路径。
fn alu_opcode(op: SsaBinaryOp) -> Option<u8> {
    use SsaBinaryOp::*;
    Some(match op {
        Add => 0,     // add
        Sub => 5,     // sub
        BitAnd | And => 4, // and
        BitOr | Or => 1,  // or
        BitXor => 6,  // xor
        Mul | Div | Rem | Shl | Shr => return None,
        Eq | Ne | Lt | Gt | Le | Ge => return None,
    })
}

/// 返回条件码（用于 setcc）。
fn condition_code(op: SsaBinaryOp) -> u8 {
    use SsaBinaryOp::*;
    match op {
        Eq => 0x94,  // sete
        Ne => 0x95,  // setne
        Lt => 0x9c,  // setl（有符号小于）
        Gt => 0x9f,  // setg
        Le => 0x9e,  // setle
        Ge => 0x9d,  // setge
        _ => 0x94,
    }
}

// ==================================================================
// 指令编码原语
// ==================================================================

/// 写入一个字节。
fn emit1(unit: &mut CompilationUnit, b: u8) {
    unit.code.push(b);
}

/// 写入两个字节。
fn emit2(unit: &mut CompilationUnit, a: u8, b: u8) {
    unit.code.push(a);
    unit.code.push(b);
}

/// 写入三个字节。
fn emit3(unit: &mut CompilationUnit, a: u8, b: u8, c: u8) {
    unit.code.push(a);
    unit.code.push(b);
    unit.code.push(c);
}

/// 写入 4 字节占位（立即数字段，稍后由重定位回填）。
fn emit4(unit: &mut CompilationUnit, v: u32) {
    unit.code.extend_from_slice(&v.to_le_bytes());
}

/// 写入 8 字节（小端）。
fn emit8(unit: &mut CompilationUnit, v: u64) {
    unit.code.extend_from_slice(&v.to_le_bytes());
}

/// 生成 ModR/M 字节：mod=mm, reg=reg, rm=rm。
fn modrm(unit: &mut CompilationUnit, mm: u8, reg: u8, rm: u8) {
    unit.code.push((mm << 6) | ((reg & 7) << 3) | (rm & 7));
}

/// `mov r64, r64`：REX.W + 0x89 /r  (89 = MR, reg=src, r/m=dst)
/// 实际用 0x89 modrm(3, src, dst)：dst = src
fn emit_mov_rr(unit: &mut CompilationUnit, dst: Gpr, src: Gpr) {
    let mut rex = 0x48u8;
    if src.rex_bit() { rex |= 0x04; } // REX.R
    if dst.rex_bit() { rex |= 0x01; } // REX.B
    emit1(unit, rex);
    emit1(unit, 0x89);
    modrm(unit, 3, src.bits(), dst.bits());
}

/// `test r64, r64`：REX.W + 0x85 /r  (85 = MR, reg=src, r/m=dst; 计算 src & dst)
fn emit_test_rr(unit: &mut CompilationUnit, a: Gpr, b: Gpr) {
    let mut rex = 0x48u8;
    if a.rex_bit() { rex |= 0x04; }
    if b.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit1(unit, 0x85);
    modrm(unit, 3, a.bits(), b.bits());
}

/// `cmp r64, r64`：REX.W + 0x39 /r  (39 = MR, reg=src, r/m=dst; 计算 dst - src)
/// `cmp a, b`（比较 `a - b`，结果只体现在标志位里）。
///
/// 0x39 是 Intel 的 `cmp r/m64, r64`，算的是 `r/m - reg`；AT&T 又把
/// 操作数反过来打印。所以要让 `a` 作被减数，ModRM 的 **rm 字段必须放 a、
/// reg 字段放 b**——写反就变成 `b - a`，`>` / `<` 会整体反掉。
fn emit_cmp_rr(unit: &mut CompilationUnit, a: Gpr, b: Gpr) {
    let mut rex = 0x48u8;
    if b.rex_bit() { rex |= 0x04; } // REX.R <- reg 字段（b）
    if a.rex_bit() { rex |= 0x01; } // REX.B <- rm  字段（a）
    emit1(unit, rex);
    emit1(unit, 0x39);
    modrm(unit, 3, b.bits(), a.bits());
}

/// `setcc r8`：0x0F 0x9x + ModR/M(r/m=reg, mod=3)
/// `cc` 是 0x90 + 条件码。
fn emit_setcc(unit: &mut CompilationUnit, cc: u8, dst: Gpr) {
    // 0x0F 0x90+cc; ModR/M: mod=3, reg=0, r/m=dst
    //
    // REX 前缀是**必须**的：没有 REX 时 0F 90-9F 这一段被解释成
    // `xchg` / `nop` / `pause` 等等，根本不是 setcc。加上 REX.B 才能
    // 正确扩展到 r8-r15。
    let mut rex = 0x40u8;
    if dst.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit2(unit, 0x0f, cc);
    modrm(unit, 3, 0, dst.bits());
}

/// `movzx r64, r8`：REX.W + 0x0F 0xB6 /r
/// 把源寄存器的 8 位结果零扩展到 64 位。
fn emit_movzx(unit: &mut CompilationUnit, dst: Gpr, src: Gpr) {
    // Intel `movzx r32, r/m8`（0F B6 /r）：**reg 字段是 32 位目的，
    // r/m 字段是 8 位源**。写反会变成把 dst 的低字节搬进 src。
    let mut rex = 0x48u8;
    if dst.rex_bit() { rex |= 0x04; } // REX.R <- reg 字段（dst）
    if src.rex_bit() { rex |= 0x01; } // REX.B <- rm  字段（src）
    emit1(unit, rex);
    emit2(unit, 0x0f, 0xb6);
    modrm(unit, 3, dst.bits(), src.bits());
}

/// 按 SysV AMD64 约定把实参搬进 rdi/rsi/rdx/rcx/r8/r9。
///
/// 这是一次**并行搬运**：目标集合和源集合可能重叠，顺序执行会互相
/// 覆盖（先 `mov rdi, rsi` 再 `mov rsi, rdi`，两步读到的都是已被改写
/// 的值，调用方会拿到 `add(20, 20)` 而不是 `add(20, 22)`）。
///
/// 做法是标准的平行搬运消环：反复挑一个"目标不再被任何人读取"的
/// 搬运先执行；如果一个都挑不出来（说明存在环），就把某个环上的源
/// 暂存到 rax（rax 不是实参寄存器），把环打开后继续。
fn emit_call_args(unit: &mut CompilationUnit, args: &[SsaValue]) {
    let n = args.len().min(ARG_REGS.len());
    let mut moves: Vec<(Gpr, Gpr)> = Vec::new();
    for i in 0..n {
        let src = alloc_reg(args[i]);
        if src != ARG_REGS[i] {
            moves.push((ARG_REGS[i], src));
        }
    }
    // 超过 6 个的实参走栈上传参区，本后端尚未实现。

    while !moves.is_empty() {
        // 目标不再被任何待执行的搬运读取 -> 可以安全执行
        let safe = moves.iter().position(|(dst, _)| {
            !moves.iter().any(|(_, src)| src == dst)
        });
        match safe {
            Some(at) => {
                let (dst, src) = moves.remove(at);
                emit_mov_rr(unit, dst, src);
            }
            None => {
                // 存在环：把第一个搬运的源暂存到 rax，把环打开
                let src = moves[0].1;
                emit_mov_rr(unit, SCRATCH_AX, src);
                for m in moves.iter_mut() {
                    if m.1 == src {
                        m.1 = SCRATCH_AX;
                    }
                }
            }
        }
    }
}

/// `call rel32`：0xE8 + imm32（需重定位）。
fn emit_call(unit: &mut CompilationUnit, target: &str) {
    let instr_start = unit.code.len();
    emit1(unit, 0xe8);
    emit4(unit, 0);
    push_reloc(unit, instr_start, 5, target);
}

/// `jmp rel32`：0xE9 + imm32（需重定位）。
fn emit_jmp(unit: &mut CompilationUnit, target: &str) {
    let instr_start = unit.code.len();
    emit1(unit, 0xe9);
    emit4(unit, 0);
    push_reloc(unit, instr_start, 5, target);
}

/// `jcc rel32`：0x0F 0x8x + imm32（需重定位）。
/// `cc` 为条件码低 4 位，例如 0x4 对应 `jz`(je)、0x5 对应 `jnz`(jne)。
fn emit_jcc_rel32(unit: &mut CompilationUnit, cc: u8, target: &str) {
    let instr_start = unit.code.len();
    emit2(unit, 0x0f, 0x80 | (cc & 0x0f));
    emit4(unit, 0);
    push_reloc(unit, instr_start, 6, target);
}

/// `ud2`：触发非法指令异常（`unreachable` 终结符）。
fn emit_ud2(unit: &mut CompilationUnit) {
    emit2(unit, 0x0f, 0x0b);
}

/// 登记一条 PC 相对重定位。
fn push_reloc(unit: &mut CompilationUnit, instr_start: usize, instr_len: usize, target: &str) {
    unit.relocations.push(Relocation::new(
        instr_start + instr_len - 4, // 立即数字段偏移
        instr_start,
        instr_len,
        target,
        RelocationKind::PcRel32,
    ));
}

// ==================================================================
// 程序入口
// ==================================================================

/// 发射程序入口 `_start`：调用 `main` 后退出进程。
///
/// 平台差异：
/// - **Linux**：自备启动桩，`main` 的返回值直接作为 `exit` 的状态码
///   （x86-64: syscall 号 60；aarch64: `x8 = 93` + `svc #0`），不依赖运行时；
/// - **macOS / Windows**：把返回值交给运行时的 `zr_exit`
///   （分别转发到 libc 的 `_exit` 与 kernel32 的 `ExitProcess`）。
pub fn emit_start_wrapper(target: &Target, unit: &mut CompilationUnit) {
    debug_assert_eq!(target.arch(), Arch::X86_64);
    let start = unit.code.len();

    emit_call(unit, "main");

    match target.os() {
        Os::Linux => {
            // mov edi, eax（main 的返回值作为退出码）
            emit2(unit, 0x89, 0xc7);
            // mov eax, 60（__NR_exit）
            emit1(unit, 0xb8);
            emit4(unit, 60);
            // syscall
            emit2(unit, 0x0f, 0x05);
        }
        Os::Darwin => {
            // SysV ABI：`main` 的返回值在 rdi，`zr_exit` 直接取用。
            emit_mov_rr(unit, Gpr::Rdi, Gpr::Rax);
            emit_call(unit, "zr_exit");
            emit_ud2(unit);
        }
        Os::Win => {
            // Microsoft x64 ABI：第一个参数在 rcx，需要搬一次。
            emit_mov_rr(unit, Gpr::Rcx, Gpr::Rax);
            emit_call(unit, "zr_exit");
            emit_ud2(unit);
        }
        // 裸机目标由各自的嵌入式后端生成入口桩
        Os::Bare | Os::Wasi => emit_ud2(unit),
    }

    let entry = target.entry_symbol();
    unit.symbols.push(Symbol {
        name: entry.to_string(),
        section: SymbolSection::Text,
        offset: start,
        size: unit.code.len() - start,
        global: true,
        kind: SymbolKind::Function,
    });
    unit.entry = entry.to_string();
}

// ==================================================================
// 乘除与移位
// ==================================================================

/// `imul r64, r/m64`：REX.W + 0x0F 0xAF /r（dst = dst * src）
fn emit_mul(unit: &mut CompilationUnit, dst: Gpr, l: Gpr, r: Gpr) {
    if dst != l {
        emit_mov_rr(unit, dst, l);
    }
    let mut rex = 0x48u8;
    // `imul r/m64, reg64`（REX.W 0F AF /r）的 reg 字段是**目的**、
    // r/m 字段是源，所以 REX.R 取自 dst、REX.B 取自 r——和 `mov r64,r64`
    // （89 /r，reg=源）的 REX 归属正好相反，弄反会读到错误的寄存器。
    if dst.rex_bit() { rex |= 0x04; }
    if r.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit2(unit, 0x0f, 0xaf);
    modrm(unit, 3, dst.bits(), r.bits());
}

/// 有符号除法 / 取模：`idiv r/m64`（REX.W + 0xF7 /7）。
///
/// 约定使用 rax 作为被除数、rdx 保存余数——这正是 `rax`/`rdx` 不参与
/// 寄存器分配的原因。
fn emit_divrem(unit: &mut CompilationUnit, dst: Gpr, l: Gpr, r: Gpr, want_rem: bool) {
    emit_mov_rr(unit, SCRATCH_AX, l);
    emit2(unit, 0x31, 0xd2); // xor edx, edx（等价 cqo 的符号扩展）
    let mut rex = 0x48u8;
    if r.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit1(unit, 0xf7);
    modrm(unit, 3, 7, r.bits()); // /7 = IDIV
    emit_mov_rr(unit, dst, if want_rem { SCRATCH_DX } else { SCRATCH_AX });
}

/// 移位 `shl r/m64, cl` 或 `shr r/m64, cl`：REX.W + D3 /4 (shl) 或 /5 (shr)。
/// x86 移位计数固定使用 CL——这是 `rcx` 不参与分配的原因。
fn emit_shift(unit: &mut CompilationUnit, code: u8, dst: Gpr, l: Gpr, r: Gpr) {
    if dst != l {
        emit_mov_rr(unit, dst, l);
    }
    if r != SCRATCH_CX {
        emit_mov_rr(unit, SCRATCH_CX, r);
    }
    let mut rex = 0x48u8;
    if dst.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit1(unit, 0xd3);
    modrm(unit, 3, code, dst.bits());
}

/// ALU 编码的完整分发：op dst, src（dst OP= src）。
fn emit_full_alu(unit: &mut CompilationUnit, op: u8, dst: Gpr, src: Gpr) {
    // op: 0=add, 1=or, 4=and, 5=sub, 6=xor
    let opcode = match op {
        0 => 0x01, // add r/m64, r64
        1 => 0x09, // or  r/m64, r64
        4 => 0x21, // and r/m64, r64
        5 => 0x29, // sub r/m64, r64
        6 => 0x31, // xor r/m64, r64
        _ => 0x01,
    };
    let mut rex = 0x48u8;
    if src.rex_bit() { rex |= 0x04; }
    if dst.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit1(unit, opcode);
    modrm(unit, 3, src.bits(), dst.bits());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> CompilationUnit {
        CompilationUnit::new()
    }

    #[test]
    fn scratch_registers_are_not_allocatable() {
        for v in 0..256u32 {
            let r = alloc_reg(SsaValue(v));
            assert!(
                !matches!(r, SCRATCH_AX | SCRATCH_CX | SCRATCH_DX),
                "暂存器不应出现在分配池中"
            );
        }
    }

    #[test]
    fn call_records_five_byte_relocation() {
        let mut u = unit();
        emit_call(&mut u, "main");
        assert_eq!(u.code.len(), 5);
        let reloc = &u.relocations[0];
        assert_eq!(reloc.offset, 1, "立即数紧随 0xE8");
        assert_eq!(reloc.instr_start, 0);
        assert_eq!(reloc.instr_len, 5);
        assert_eq!(reloc.kind, RelocationKind::PcRel32);
    }

    #[test]
    fn jcc_relocation_points_past_two_byte_opcode() {
        let mut u = unit();
        emit_jcc_rel32(&mut u, 0x84, "bb1");
        assert_eq!(u.code.len(), 6);
        let reloc = &u.relocations[0];
        assert_eq!(reloc.offset, 2, "立即数紧随 0x0F 0x84");
        assert_eq!(reloc.instr_len, 6);
    }

    #[test]
    fn division_uses_rax_and_rdx() {
        let mut u = unit();
        emit_divrem(&mut u, ALLOCATABLE[0], ALLOCATABLE[1], ALLOCATABLE[2], false);
        // 被除数 rdi（ALLOCATABLE[1]）→ rax，除数 r8（ALLOCATABLE[2]）
        assert_eq!(u.code.len(), 11);
        assert_eq!(&u.code[0..3], &[0x48, 0x89, 0xf8], "mov rax, rdi");
        assert_eq!(&u.code[3..5], &[0x31, 0xd2], "xor edx, edx");
        assert_eq!(&u.code[5..8], &[0x49, 0xf7, 0xf8], "REX.WB + idiv r8");
    }

    #[test]
    fn linux_start_wrapper_calls_main_then_syscall() {
        let mut u = unit();
        let target = Target::new(Arch::X86_64, Os::Linux);
        emit_start_wrapper(&target, &mut u);
        assert_eq!(u.code[0], 0xe8, "call main");
        assert!(u.code.ends_with(&[0x0f, 0x05]), "以 syscall 结束");
        assert_eq!(u.entry, "_start");
    }

    #[test]
    fn macos_start_wrapper_delegates_to_runtime_exit() {
        let mut u = unit();
        let target = Target::new(Arch::X86_64, Os::Darwin);
        emit_start_wrapper(&target, &mut u);
        assert_eq!(u.code[0], 0xe8, "call main");
        assert_eq!(&u.code[5..8], &[0x48, 0x89, 0xc7], "mov rdi, rax");
        assert_eq!(u.code[8], 0xe8, "call zr_exit");
        let targets: Vec<&str> = u.relocations.iter().map(|r| r.symbol.as_str()).collect();
        assert_eq!(targets, vec!["main", "zr_exit"]);
    }

    #[test]
    fn windows_start_wrapper_moves_return_into_rcx() {
        let mut u = unit();
        let target = Target::new(Arch::X86_64, Os::Win);
        emit_start_wrapper(&target, &mut u);
        assert_eq!(&u.code[5..8], &[0x48, 0x89, 0xc1], "mov rcx, rax");
        assert_eq!(u.code[8], 0xe8, "call zr_exit");
    }
}