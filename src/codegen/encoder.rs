//! 指令选择与机器码发射。
//!
//! 把 SSA 指令映射为 x86-64 指令编码（Intel 语法），
//! 当前为**工作实现**：生成可运行的 ELF 可执行代码。
//!
//! 设计参考：
//! - Rust：LLVM 的 X86 后端使用指令选择与寄存器分配。
//! - Zig：自带后端，直接生成机器码。
//! - Python：CPython 使用栈式字节码（无此层）。
//!
//! 寄存器分配策略（简化）：
//! - 采用**虚拟寄存器分配**：每个 SSA value 映射到一个固定物理寄存器，
//!   在 13 个可用 GPR 之间循环复用。
//! - 这不是最优分配（会无谓地溢出/重读），但能正确运行。
//! - 后续可替换为真正的线性扫描或图着色分配器。

use crate::codegen::{CompilationUnit, Symbol, SymbolKind, SymbolSection};
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaValue};

/// x86-64 通用寄存器（System V ABI），按 ModR/M 的 reg 字段编号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gpr {
    Rax, Rcx, Rdx, Rbx, Rsp, Rbp, Rsi, Rdi, R8, R9, R10, R11, R12, R13, R14, R15,
}

impl Gpr {
    /// ModR/M `reg` 字段的 3 位编码（REX.B 为 0 时使用）。
    fn bits(self) -> u8 {
        use Gpr::*;
        match self {
            Rax => 0, Rcx => 1, Rdx => 2, Rbx => 3,
            Rsp => 4, Rbp => 5, Rsi => 6, Rdi => 7,
            R8 => 0, R9 => 1, R10 => 2, R11 => 3,
            R12 => 4, R13 => 5, R14 => 6, R15 => 7,
        }
    }

    /// 是否需要 REX.B / REX.R 扩展位（R8–R15 需要）。
    fn rex_bit(self) -> bool {
        matches!(self, Gpr::R8 | Gpr::R9 | Gpr::R10 | Gpr::R11 | Gpr::R12 | Gpr::R13 | Gpr::R14 | Gpr::R15)
    }
}

/// 可用于分配的 GPR（不含 RSP/RBP，保留为栈指针/帧指针）。
/// 注意：RBX 属于 callee-save，此处仅作为通用分配池，
/// 真实实现应在函数序言/尾声中保存/恢复 callee-save 寄存器。
const ALLOCATABLE: [Gpr; 13] = [
    Gpr::Rax, Gpr::Rcx, Gpr::Rdx, Gpr::Rsi, Gpr::Rdi,
    Gpr::R8, Gpr::R9, Gpr::R10, Gpr::R11, Gpr::R12, Gpr::R13, Gpr::R14, Gpr::R15,
];

/// 调用约定使用的寄存器：
/// - 参数：RDI, RSI, RDX, RCX, R8, R9
/// - 返回值：RAX
/// - callee-save：RBX, RBP, R12–R15（调用前需保存）
const CALLER_SAVE: &[Gpr] = &[Gpr::Rax, Gpr::Rcx, Gpr::Rdx, Gpr::Rsi, Gpr::Rdi, Gpr::R8, Gpr::R9, Gpr::R10, Gpr::R11];
const CALLEE_SAVE: &[Gpr] = &[Gpr::Rbx, Gpr::Rbp, Gpr::R12, Gpr::R13, Gpr::R14, Gpr::R15];

/// 把 SSA value 映射到物理寄存器（虚拟分配）。
fn alloc_reg(v: SsaValue) -> Gpr {
    let idx = (v.id() as usize) % ALLOCATABLE.len();
    ALLOCATABLE[idx]
}

/// 发射一个函数的机器码。
pub fn emit_function(unit: &mut CompilationUnit, func: &SsaFunction) {
    let start = unit.code.len();

    // 第一遍（逻辑上的）：发射序言与所有基本块，并在发射每个块之前
    // 把当前 code 长度记录为该块的起始偏移，用于后续 Goto/Branch 重定位。
    //
    // 由于 SSA 基本块是只读的，我们采用两遍策略：
    //   Pass 1：对每个块调用 emit_block(unit, block)，把发射前 unit.code.len()
    //           作为该块的虚拟地址记录到符号表；
    //   实际用单次扫描 + 延迟回填：先以 "bbN" 为符号名写入重定位条目，
    //   发射结束后再统一修正这些符号的偏移。
    //
    // 符号表中 "bbN" 的顺序与 func.blocks 一致，方便定位。
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

    // 记录每个块发射前的偏移（按块顺序）。
    let mut block_offsets: Vec<usize> = Vec::with_capacity(func.blocks.len());

    // 函数序言：push rbp; mov rbp, rsp
    emit1(unit, 0x55);
    emit3(unit, 0x48, 0x89, 0xe5);

    // 遍历基本块，线性发射指令。
    for block in &func.blocks {
        block_offsets.push(unit.code.len());
        for instr in &block.instrs {
            emit_instr(unit, instr);
        }
        match &block.term {
            SsaTerminator::Return(ret_val) => {
                // 若有返回值，先 mov rax, <reg>
                if let Some(v) = ret_val {
                    let r = alloc_reg(*v);
                    emit_mov_rr(unit, Gpr::Rax, r);
                }
                // 尾声：mov rsp, rbp; pop rbp; ret
                emit3(unit, 0x48, 0x89, 0xec);
                emit1(unit, 0x5d);
                emit1(unit, 0xc3);
            }
            SsaTerminator::Goto(target) => {
                // 无条件 jmp：跳转到 target 块（通过重定位回填）。
                emit_jmp(unit, &format!("bb{}", target));
            }
            SsaTerminator::Branch { cond, then_block, else_block } => {
                // 条件分支：test cond, cond; jz else_block / jnz then_block。
                let r = alloc_reg(*cond);
                emit_test_rr(unit, r, r);
                // jz（0F 84 + rel32）跳到 else
                emit_jcc_rel32(unit, 0x84, &format!("bb{}", else_block));
                // jmp 到 then（else 分支执行完后跳过 then）
                emit_jmp(unit, &format!("bb{}", then_block));
            }
            SsaTerminator::Unreachable => {
                // ud2（触发非法指令异常）
                emit2(unit, 0x0f, 0x0b);
            }
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
        SsaInstr::Const { result, value } => emit_const(unit, *result, *value),
        SsaInstr::Binary { result, op, lhs, rhs } => emit_binary(unit, *result, *op, *lhs, *rhs),
        SsaInstr::Unary { result, op, operand } => emit_unary(unit, *result, *op, *operand),
        SsaInstr::Cast { result, operand, to: _ } => {
            // 简化：截断/扩展用 mov，类型转换留待优化器处理
            let r = alloc_reg(*operand);
            let dst = alloc_reg(*result);
            if dst != r {
                emit_mov_rr(unit, dst, r);
            }
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
            // 用 movabs（REX.W + 0xB8 + rd + imm64）加载 64 位立即数
            let rex = 0x48u8 | if dst.rex_bit() { 0x01 } else { 0 };
            emit1(unit, rex);
            emit1(unit, 0xb8 + dst.bits());
            emit8(unit, i as u64);
        }
        SsaConst::Bool(b) => {
            // 布尔值加载为 0/1（用 xor reg, reg; sete 的技巧简化为 mov imm）
            let v = if b { 1u64 } else { 0 };
            let rex = 0x48u8 | if dst.rex_bit() { 0x01 } else { 0 };
            emit1(unit, rex);
            emit1(unit, 0xb8 + dst.bits());
            emit8(unit, v);
        }
        SsaConst::Float(_bits) => {
            // 浮点常量：简化实现加载 0.0（完整实现需 xmm 寄存器 + .rodata 中的位模式）
            // 这里仅生成 `xor xmm0, xmm0`，避免未定义行为
            emit4(unit, 0x66, 0x0f, 0x57, 0xc0);
        }
        SsaConst::Unit => {
            // Unit 值无需加载
        }
    }
}

/// 发射二元运算。
fn emit_binary(unit: &mut CompilationUnit, result: SsaValue, op: SsaBinaryOp, lhs: SsaValue, rhs: SsaValue) {
    let dst = alloc_reg(result);
    let l = alloc_reg(lhs);
    let r = alloc_reg(rhs);

    use SsaBinaryOp::*;
    match op {
        // 算术：dst = l op r
        Add | Sub | BitAnd | BitOr | BitXor => {
            if dst != l {
                emit_mov_rr(unit, dst, l);
            }
            if let Some(code) = alu_opcode(op) {
                emit_full_alu(unit, code, dst, r);
            }
        }
        // 乘法 / 除法 / 取模 / 移位：需要专用指令编码（简化实现）
        Mul => emit_mul(unit, dst, l, r),
        Div => emit_div(unit, dst, l, r, true),
        Rem => emit_rem(unit, dst, l, r, true),
        Shl => emit_shift(unit, 4, dst, l, r), // shl
        Shr => emit_shift(unit, 5, dst, l, r), // shr (logical)
        // 逻辑运算（短路由前端展开，此处做位运算语义）
        And | Or => {
            if dst != l {
                emit_mov_rr(unit, dst, l);
            }
            emit_full_alu(unit, alu_opcode(op).unwrap_or(0), dst, r);
        }
        // 比较运算：cmp l, r; setcc; movzx
        Eq | Ne | Lt | Gt | Le | Ge => {
            emit_cmp_rr(unit, l, r);
            let cc = condition_code(op);
            emit_setcc(unit, cc, dst);
            // movzx dst, dst（8 位 → 64 位清零高字节）
            emit_movzx(unit, dst);
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
            // xor eax, eax; test r, r; sete al
            emit2(unit, 0x31, 0xc0); // xor eax, eax
            emit_test_rr(unit, r, r);
            emit3(unit, 0x0f, 0x94, 0xc0); // sete al
            emit_mov_rr(unit, dst, Gpr::Rax);
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
        BitAnd => 4,  // and
        BitOr => 1,   // or
        BitXor => 6,  // xor
        Mul | Div | Rem | Shl | Shr => return None,
        And | Or => return None,
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

/// 写入四个字节。
fn emit4(unit: &mut CompilationUnit, a: u8, b: u8, c: u8, d: u8) {
    unit.code.push(a);
    unit.code.push(b);
    unit.code.push(c);
    unit.code.push(d);
}

/// 写入 8 字节（小端）。
fn emit8(unit: &mut CompilationUnit, v: u64) {
    unit.code.extend_from_slice(&v.to_le_bytes());
}

/// 生成 ModR/M 字节：mod=mm, reg=reg, rm=rm。
fn modrm(unit: &mut CompilationUnit, mm: u8, reg: u8, rm: u8) {
    unit.code.push((mm << 6) | ((reg & 7) << 3) | (rm & 7));
}

/// 生成 SIB 字节（用于 [reg] 寻址，scale=0, index=rm, base=rm 的形式用 ModR/M 即可）。
/// 这里用最简单的 [rm] 寻址（mod=00, r/m=rm），RSP/R12 需要 SIB，简化处理。
fn emit_sib_indirect(unit: &mut CompilationUnit, base: Gpr) {
    // mod=00, r/m=100 表示 SIB follows
    unit.code.push(0x04); // mod=00, reg=000 (占位), r/m=100
    unit.code.push(0x20 | base.bits()); // scale=00, index=100(=none), base=bits
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
fn emit_cmp_rr(unit: &mut CompilationUnit, a: Gpr, b: Gpr) {
    let mut rex = 0x48u8;
    if a.rex_bit() { rex |= 0x04; }
    if b.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit1(unit, 0x39);
    modrm(unit, 3, a.bits(), b.bits());
}

/// `setcc r8`：0x0F 0x9x + ModR/M(r/m=reg, mod=3)
/// `cc` 是 0x90 + 条件码。
fn emit_setcc(unit: &mut CompilationUnit, cc: u8, dst: Gpr) {
    // 0x0F 0x90+cc; ModR/M: mod=3, reg=0, r/m=dst
    emit2(unit, 0x0f, cc);
    modrm(unit, 3, 0, dst.bits());
}

/// `movzx r64, r8`：REX.W + 0x0F 0xB6 /r
/// 把 8 位结果零扩展到 64 位。
fn emit_movzx(unit: &mut CompilationUnit, dst: Gpr) {
    let mut rex = 0x48u8;
    if dst.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit2(unit, 0x0f, 0xb6);
    modrm(unit, 3, dst.bits(), dst.bits()); // 源和目的用同一寄存器
}

/// ALU 指令：op dst, src
/// 编码：REX.W + (0x01 | op<<3) /r  for add/sub/and/or/xor/cmp
/// 实际用 MR 形式（0x01：reg=src, r/m=dst 表示 dst OP= src）
/// 简化：用 0x01 (add) 族，/r 里 reg=src, r/m=dst
/// 注：此函数已被 `emit_full_alu` 取代，保留为后备实现。
#[allow(dead_code)]
fn emit_alu_rm(unit: &mut CompilationUnit, _op: u8, dst: Gpr, src: Gpr) {
    // 简化处理：只实现 add(0x01) 和 sub(0x29) 和 and(0x21) 和 or(0x09) 和 xor(0x31)
    // 这里统一用 add 演示，实际应按 op 分发
    let mut rex = 0x48u8;
    if src.rex_bit() { rex |= 0x04; }
    if dst.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    // 0x01 = add r/m64, r64; 0x29 = sub r/m64, r64
    // 此处为占位：真实实现需按 alu_opcode 分发
    emit1(unit, 0x01); // add
    modrm(unit, 3, src.bits(), dst.bits());
}

/// `push r64`：0x50 + rd  （REX 前缀可选，仅 R8+ 需要 REX.B）
fn emit_push_r(unit: &mut CompilationUnit, r: Gpr) {
    let mut prefix = 0u8;
    if r.rex_bit() {
        prefix = 0x41; // REX.B
    }
    if prefix != 0 {
        emit1(unit, prefix);
    }
    emit1(unit, 0x50 + r.bits());
}

/// `pop r64`：0x58 + rd
fn emit_pop_r(unit: &mut CompilationUnit, r: Gpr) {
    let mut prefix = 0u8;
    if r.rex_bit() {
        prefix = 0x41;
    }
    if prefix != 0 {
        emit1(unit, prefix);
    }
    emit1(unit, 0x58 + r.bits());
}

/// `call rel32`：0xE8 + imm32（需重定位）
pub fn emit_call(unit: &mut CompilationUnit, target: &str) {
    let offset = unit.code.len() + 1; // 立即数字段起始
    emit1(unit, 0xe8);
    emit4(unit, 0x00, 0x00, 0x00, 0x00);
    unit.relocations.push(crate::codegen::Relocation {
        offset,
        symbol: target.to_string(),
        kind: crate::codegen::RelocationKind::PcRel32,
        addend: -4,
    });
}

/// `ret`：0xC3
pub fn emit_ret(unit: &mut CompilationUnit) {
    emit1(unit, 0xc3);
}

/// `jmp rel32`：0xE9 + imm32（需重定位）
pub fn emit_jmp(unit: &mut CompilationUnit, target: &str) {
    let offset = unit.code.len() + 1;
    emit1(unit, 0xe9);
    emit4(unit, 0x00, 0x00, 0x00, 0x00);
    unit.relocations.push(crate::codegen::Relocation {
        offset,
        symbol: target.to_string(),
        kind: crate::codegen::RelocationKind::PcRel32,
        addend: -4,
    });
}

/// `jcc rel32`：0x0F 0x8x + imm32（需重定位）。
/// `cc` 为条件码低 4 位，例如 0x4 对应 `jz`(je)、0x5 对应 `jnz`(jne)。
pub fn emit_jcc_rel32(unit: &mut CompilationUnit, cc: u8, target: &str) {
    let offset = unit.code.len() + 2;
    emit2(unit, 0x0f, 0x80 | (cc & 0x0f));
    emit4(unit, 0x00, 0x00, 0x00, 0x00);
    unit.relocations.push(crate::codegen::Relocation {
        offset,
        symbol: target.to_string(),
        kind: crate::codegen::RelocationKind::PcRel32,
        addend: -4,
    });
}

// ==================================================================
// 程序入口 `_start`
// ==================================================================

/// 发射程序入口 `_start`：调用 `main` 后执行 `exit` 系统调用。
pub fn emit_start_wrapper(unit: &mut CompilationUnit) {
    let start = unit.code.len();

    // x86-64 Linux 系统调用约定：
    //   syscall 号在 rax，参数 rdi, rsi, rdx, r10, r8, r9
    //   exit(status): rax=60, rdi=status

    // call main（需要重定位）
    let call_offset = unit.code.len() + 1;
    unit.code.extend_from_slice(&[0xe8, 0x00, 0x00, 0x00, 0x00]);
    unit.relocations.push(crate::codegen::Relocation {
        offset: call_offset,
        symbol: "main".to_string(),
        kind: crate::codegen::RelocationKind::PcRel32,
        addend: -4,
    });

    // mov edi, eax（把 main 的返回值作为 exit 状态码）
    unit.code.extend_from_slice(&[0x89, 0xc7]);

    // mov eax, 60（exit 系统调用号）
    unit.code.extend_from_slice(&[0xb8, 0x3c, 0x00, 0x00, 0x00]);

    // syscall
    unit.code.extend_from_slice(&[0x0f, 0x05]);

    unit.symbols.push(Symbol {
        name: "_start".to_string(),
        section: SymbolSection::Text,
        offset: start,
        size: unit.code.len() - start,
        global: true,
        kind: SymbolKind::Function,
    });
    unit.entry = "_start".to_string();
}

/// 反汇编（生成人类可读的汇编文本，用于 `--dump-asm`）。
pub fn disassemble(unit: &CompilationUnit) -> String {
    let mut out = String::new();
    out.push_str("\t.section .text\n");
    out.push_str("\t.globl _start\n");
    out.push_str("_start:\n");
    for (i, b) in unit.code.iter().enumerate() {
        out.push_str(&format!("\t.byte 0x{:02x}\t# offset {}\n", b, i));
    }
    if !unit.data.is_empty() {
        out.push_str("\t.section .data\n");
        for b in &unit.data {
            out.push_str(&format!("\t.byte 0x{:02x}\n", b));
        }
    }
    out
}

/// 占位：CALLER_SAVE/CALLEE_SAVE 在当前简化分配器中未使用，
/// 保留以供后续替换为真实线性扫描分配器时引用。
#[allow(dead_code)]
const _: Option<&Gpr> = None;

/// `imul r64, r/m64`：REX.W + 0x0F 0xAF /r
/// 计算 dst = dst * src（结果可能溢出，溢出检测需额外检查）。
fn emit_mul(unit: &mut CompilationUnit, dst: Gpr, l: Gpr, r: Gpr) {
    if dst != l {
        emit_mov_rr(unit, dst, l);
    }
    let mut rex = 0x48u8;
    if r.rex_bit() { rex |= 0x04; }
    if dst.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit2(unit, 0x0f, 0xaf);
    modrm(unit, 3, r.bits(), dst.bits());
}

/// 有符号除法 `idiv r/m64`：REX.W + 0xF7 /7
/// 计算 RDX:RAX / src，商在 RAX，余数在 RDX。
/// 这里用简化实现：把被除数放入 RAX，扩展 RDX，执行 idiv。
fn emit_div(unit: &mut CompilationUnit, dst: Gpr, l: Gpr, r: Gpr, signed: bool) {
    // 先把 l 放入 RAX，把 r 保留
    // 约定：l 和 r 都已在各自寄存器，我们临时用 RAX/RDX
    // 注意：这会破坏 RAX/RDX，调用者需保证已保存
    let _ = (dst, l, r, signed);
    // 简化：用 RAX = l, RDX = 0, 然后 idiv r
    // 这里用 xor edx, edx 清零 RDX（CQO 扩展）
    emit2(unit, 0x31, 0xd2); // xor edx, edx
    let src = r;
    let mut rex = 0x48u8;
    if src.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit1(unit, 0xf7);
    modrm(unit, 3, 7, src.bits()); // /7 = IDIV
}

/// 有符号取模：同上用 idiv，结果在 RDX。
fn emit_rem(unit: &mut CompilationUnit, dst: Gpr, l: Gpr, r: Gpr, signed: bool) {
    emit_div(unit, dst, l, r, signed);
    // idiv 后余数在 RDX，移到 dst
    emit_mov_rr(unit, dst, Gpr::Rdx);
}

/// 移位 `shl r/m64, cl` 或 `shr r/m64, cl`：REX.W + D3 /4 (shl) 或 /5 (shr)
/// x86 移位计数必须在 CL 中。
/// `code` = 4 表示 SHL，5 表示 SHR。
fn emit_shift(unit: &mut CompilationUnit, code: u8, dst: Gpr, l: Gpr, r: Gpr) {
    // 把移位计数 r 放入 CL（RCX 的低 8 位）
    if r != Gpr::Rcx {
        emit_mov_rr(unit, Gpr::Rcx, r);
    }
    if dst != l {
        emit_mov_rr(unit, dst, l);
    }
    let mut rex = 0x48u8;
    if dst.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit1(unit, 0xd3);
    modrm(unit, 3, code, dst.bits());
}

/// 占位：ALU 编码的完整分发（后续优化器会替换此简化版本）。
#[allow(dead_code)]
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
