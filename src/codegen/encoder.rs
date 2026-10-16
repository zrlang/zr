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

use crate::codegen::regalloc::{Allocation, Location, X86_64_CALLEE_SAVE};
use crate::codegen::target::{Arch, Os, Target};
use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};
use std::collections::HashMap;
use crate::mir::NumTy;
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
/// 基本块标签：`<函数名>.bb<编号>`。
///
/// 早先只用 `bb<编号>`，于是**不同函数的同名块会共用一个符号**——`main`
/// 的 `bb1` 和 `f` 的 `bb1` 撞在一起，`main` 里的分支会跳进别的函数。
/// 加上函数名前缀后每个块的标签全局唯一。
/// 一个块的后继。
fn successors(block: &crate::ssa::SsaBasicBlock) -> Vec<usize> {
    match &block.term {
        SsaTerminator::Goto(t) => vec![*t],
        SsaTerminator::Branch { then_block, else_block, .. } => vec![*then_block, *else_block],
        _ => Vec::new(),
    }
}

/// 在当前块（`from`）末尾，把本块持有的值写进后继块 `succ` 的各个 φ 寄存器。
///
/// 多个 φ 的写入彼此独立但必须**同时**读源：直接按顺序 `mov` 会在存在
/// 环（`x = y; y = x`）时读到已被改写的值。做法和实参搬运一样——反复挑
/// "目标不再是任何源"的先执行，都挑不出就先用 rax 暂存把环打开。
fn emit_phi_copies(unit: &mut CompilationUnit, func: &SsaFunction, succ: usize, from: usize) {
    let Some(block) = func.blocks.iter().find(|b| b.id == succ) else {
        return;
    };
    let mut moves: Vec<(Gpr, Src)> = Vec::new();
    // 溢出结果先单独处理：它们没有固定寄存器，只能经 `rax` 中转。
    // 放在最前面做是安全的——`rax` 不参与分配，不是任何搬运的源。
    for instr in &block.instrs {
        let SsaInstr::Phi { result, incoming, .. } = instr else {
            continue;
        };
        let Some((_, src)) = incoming.iter().find(|(p, _)| *p == from) else {
            continue;
        };
        if let Location::Stack(slot) = location_of(*result) {
            emit_mov_src(unit, SCRATCH_AX, src_of(*src));
            emit_store_slot(unit, SCRATCH_AX, slot);
        }
    }
    // 剩下的都是寄存器到寄存器，统一走并行搬运处理环
    for instr in &block.instrs {
        let SsaInstr::Phi { result, incoming, .. } = instr else {
            continue;
        };
        let Some((_, src)) = incoming.iter().find(|(p, _)| *p == from) else {
            continue;
        };
        let Location::Reg(d) = location_of(*result) else {
            continue;
        };
        moves.push((ALLOCATABLE[d], src_of(*src)));
    }
    emit_parallel_moves(unit, &moves);
}

/// 本函数实际用到的 callee-save 寄存器（按 push 顺序）。
fn callee_saved_regs(map: &HashMap<u32, Location>) -> Vec<Gpr> {
    ALLOCATABLE
        .iter()
        .copied()
        .filter(|r| matches!(r, Gpr::R12 | Gpr::R13 | Gpr::R14 | Gpr::R15))
        .filter(|r| {
            map.values()
                .any(|loc| matches!(loc, Location::Reg(i) if ALLOCATABLE[*i] == *r))
        })
        .collect()
}

/// `push r64`：`41 5<8+reg>`。
fn emit_push_r(unit: &mut CompilationUnit, r: Gpr) {
    emit2(unit, 0x41, 0x50 + r.bits());
}

/// `pop r64`：`41 58<8+reg>`。
fn emit_pop_r(unit: &mut CompilationUnit, r: Gpr) {
    emit2(unit, 0x41, 0x58 + r.bits());
}

fn block_label(func: &str, id: usize) -> String {
    format!("{func}.bb{id}")
}

// 当前函数的「值 id -> 寄存器下标」映射。寄存器分配要跨整个函数做
// 活跃区间分析，结果没法用纯函数 `alloc_reg(v)` 表达，所以放在
// thread_local 里：`emit_function` 开头写入、结尾清空。编译是单线程的，
// 这样够用，也免得把映射逐层传进每个发射函数。
thread_local! {
    static REG_MAP: std::cell::RefCell<Allocation> =
        std::cell::RefCell::new(Allocation::default());
    /// 当前函数预留的栈槽数（驱动序言的 `sub rsp`）。
    static FRAME_SLOTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 取 `v` 的位置；没有分配记录时退回取模。
fn location_of(v: SsaValue) -> Location {
    REG_MAP.with(|m| {
        m.borrow()
            .map
            .get(&v.id())
            .copied()
            .unwrap_or(Location::Reg((v.id() as usize) % ALLOCATABLE.len()))
    })
}

/// 把 SSA value 映射到物理寄存器（线性扫描的结果）。
///
/// 溢出到内存的值没有寄存器——调用方必须先用 [`use_reg`] 把它们从栈槽
/// 装回寄存器再取。直接调用本函数等于把一个错误编码进去，所以它只在
/// 调用方已经保证该值有寄存器时使用。
fn alloc_reg(v: SsaValue) -> Gpr {
    match location_of(v) {
        Location::Reg(i) => ALLOCATABLE[i],
        Location::Stack(_) => unreachable!("值溢出到了栈槽，必须先经 use_reg 取回"),
    }
}

/// 取 `v` 所在的寄存器，必要时从栈槽装载。
///
/// 溢出值没有固定寄存器，用 `rax` 装载——它不参与分配，随时可用。
fn use_reg(unit: &mut CompilationUnit, v: SsaValue) -> Gpr {
    match location_of(v) {
        Location::Reg(i) => ALLOCATABLE[i],
        Location::Stack(slot) => {
            emit_load_slot(unit, SCRATCH_AX, slot);
            SCRATCH_AX
        }
    }
}

/// 并行搬运的来源：寄存器，或者某个溢出值的栈槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Src {
    Reg(Gpr),
    Mem(usize),
}

/// `mov dst, [rsp + 8*slot]`
fn emit_load_slot(unit: &mut CompilationUnit, dst: Gpr, slot: usize) {
    emit1(unit, 0x48 | if dst.rex_bit() { 0x01 } else { 0 });
    emit1(unit, 0x8B);
    // mod=10 (disp32), reg=dst, r/m=100 (SIB) -> SIB: base=rsp(100), index=none
    emit1(unit, 0b10_000_100 | dst.bits() << 3);
    emit1(unit, 0b00_100_100);
    emit4(unit, (slot * 8) as u32);
}

/// `mov [rsp + 8*slot], src`
fn emit_store_slot(unit: &mut CompilationUnit, src: Gpr, slot: usize) {
    emit1(unit, 0x48 | if src.rex_bit() { 0x04 } else { 0 });
    emit1(unit, 0x89);
    // mod=10 (disp32), reg=src, r/m=100 (SIB)
    emit1(unit, 0b10_000_100 | src.bits() << 3);
    emit1(unit, 0b00_100_100);
    emit4(unit, (slot * 8) as u32);
}

/// 发射一个函数的机器码。
pub fn emit_function(target: &Target, unit: &mut CompilationUnit, func: &SsaFunction) {
    debug_assert_eq!(target.arch(), Arch::X86_64, "x86-64 后端只能用于 x86_64 目标");
    // 先做活跃区间分析 + 线性扫描，确定每个值落在哪个寄存器
    let alloc = crate::codegen::regalloc::allocate_registers(
        func,
        ALLOCATABLE.len(),
        &X86_64_CALLEE_SAVE,
    );
    // callee-save 集合要在发射尾声时再用，所以这里留一份
    let callee_saved = callee_saved_regs(&alloc.map);
    FRAME_SLOTS.with(|c| c.set(alloc.spilled));
    REG_MAP.with(|m| {
        *m.borrow_mut() = alloc;
    });
    let start = unit.code.len();

    // SSA 基本块是只读的，采用两遍策略：先为每个块占位一个符号，
    // 发射时记录块起始偏移，发射结束后统一回填。
    let sym_base = unit.symbols.len();
    for block in &func.blocks {
        unit.symbols.push(Symbol {
            name: block_label(&func.name, block.id),
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
    // R12–R15 是 callee-save：分配器可能用到，用到就存，返回前还原
    for r in &callee_saved {
        emit_push_r(unit, *r);
    }
    // 溢出值的栈槽。SysV 要求调用时 rsp 16 字节对齐，所以向上取整。
    let frame = FRAME_SLOTS.with(|c| c.get()) * 8;
    let frame = (frame + 15) & !15;
    if frame > 0 {
        // sub rsp, imm32
        emit1(unit, 0x48);
        emit1(unit, 0x81);
        emit1(unit, 0xEC);
        emit4(unit, frame as u32);
    }
    emit_param_moves(unit, func);

    for block in &func.blocks {
        block_offsets.push(unit.code.len());
        for instr in &block.instrs {
            emit_instr(unit, instr);
        }
        // φ 是"在每个前驱末尾"的并行拷贝：跳转之前必须把本块持有的值
        // 写进后继块各个 φ 的目标寄存器。
        for succ in successors(block) {
            emit_phi_copies(unit, func, succ, block.id);
        }
        match &block.term {
            SsaTerminator::Return(ret_val) => {
                // 若有返回值，先 mov rax, <reg>
                if let Some(v) = ret_val {
                    emit_mov_rr(unit, SCRATCH_AX, alloc_reg(*v));
                }
                // 尾声：add rsp, frame; pop callee-save; mov rsp, rbp; pop rbp; ret
                if frame > 0 {
                    emit1(unit, 0x48);
                    emit1(unit, 0x81);
                    emit1(unit, 0xC4);
                    emit4(unit, frame as u32);
                }
                for r in callee_saved.iter().rev() {
                    emit_pop_r(unit, *r);
                }
                emit3(unit, 0x48, 0x89, 0xec);
                emit1(unit, 0x5d);
                emit1(unit, 0xc3);
            }
            SsaTerminator::Goto(target) => emit_jmp(unit, &block_label(&func.name, *target)),
            SsaTerminator::Branch { cond, then_block, else_block } => {
                // test cond, cond; jz else; jmp then
                // 条件可能溢出在栈上，先装回 rax 再测
                let r = use_reg(unit, *cond);
                emit_test_rr(unit, r, r);
                emit_jcc_rel32(unit, 0x84, &block_label(&func.name, *else_block)); // jz
                emit_jmp(unit, &block_label(&func.name, *then_block));
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
            match location_of(*result) {
                Location::Reg(i) => {
                    let d = ALLOCATABLE[i];
                    if d != SCRATCH_AX {
                        emit_mov_rr(unit, d, SCRATCH_AX);
                    }
                }
                Location::Stack(slot) => emit_store_slot(unit, SCRATCH_AX, slot),
            }
        }
        // φ 的实际拷贝由前驱末尾的 emit_phi_copies 发出
        SsaInstr::Phi { .. } => {}
        SsaInstr::Copy { result, src } => {
            let spilled = matches!(location_of(*result), Location::Stack(_));
            let d = if spilled { SCRATCH_AX } else { alloc_reg(*result) };
            match location_of(*src) {
                Location::Reg(i) => {
                    let s = ALLOCATABLE[i];
                    if d != s {
                        emit_mov_rr(unit, d, s);
                    }
                }
                Location::Stack(slot) => emit_load_slot(unit, d, slot),
            }
            if spilled {
                if let Location::Stack(slot) = location_of(*result) {
                    emit_store_slot(unit, d, slot);
                }
            }
        }
        SsaInstr::Cast { result, src, from, to } => {
            let spilled = matches!(location_of(*result), Location::Stack(_));
            let d = if spilled { SCRATCH_AX } else { alloc_reg(*result) };
            let s = use_reg(unit, *src);
            emit_cast(unit, d, s, *from, *to);
            if spilled {
                if let Location::Stack(slot) = location_of(*result) {
                    emit_store_slot(unit, d, slot);
                }
            }
        }
        SsaInstr::Var { .. } => {
            // Var 是 φ 合并前的引用，运行时无需动作
        }
    }
}

/// 类型转换。
///
/// 值一律放在 64 位寄存器里，所以转换就是"取低 w 位再按目标的有无符号
/// 重新符号/零扩展到 64 位"。宽度已经是 64 时什么都不用做。
fn emit_cast(
    unit: &mut CompilationUnit,
    dst: Gpr,
    src: Gpr,
    from: NumTy,
    to: NumTy,
) {
    // 浮点之间的转换、浮点与整数之间的转换目前后端还没有实现。
    // 宁可报错也不能静默发出 0 —— 那比编译失败更难查。
    if from.is_float() || to.is_float() {
        crate::codegen::unsupported_cast(from, to);
    }
    let (bits, signed) = match to {
        NumTy::Int(w) => (w, true),
        NumTy::Uint(w) => (w, false),
        // bool 和 64 位整数之间转换只是取低 1 位 / 归一化
        NumTy::Bool => (8, false),
        NumTy::Float(_) => unreachable!("上面已经拦下了浮点"),
    };
    if bits >= 64 {
        if dst != src {
            emit_mov_rr(unit, dst, src);
        }
        return;
    }
    emit_extend_to64(unit, dst, src, bits, signed);
}

/// `movsx` / `movzx`：把 `src` 的低 `bits` 位按 `signed` 解释后扩展到 64 位。
fn emit_extend_to64(unit: &mut CompilationUnit, dst: Gpr, src: Gpr, bits: u8, signed: bool) {
    // 这些指令的 ModRM 都是 **reg = 目的(dst), r/m = 源(src)**，
    // 和 `mov r/m, reg`（0x89）正好相反，REX 位也跟着换：
    // REX.R(0x04) 扩展 reg 字段（dst），REX.B(0x01) 扩展 r/m 字段（src）。
    // 写反的话 R8 会被当成 RAX 读。
    let mut rex = 0x48u8;
    if dst.rex_bit() {
        rex |= 0x04;
    }
    if src.rex_bit() {
        rex |= 0x01;
    }
    let reg_rm = dst.bits();
    let rm = src.bits();
    match (bits, signed) {
        // movsx：0F BE /r 取字节、0F BF /r 取字
        (8, true) => {
            emit1(unit, rex);
            emit1(unit, 0x0F);
            emit1(unit, 0xBE);
            modrm(unit, 3, reg_rm, rm);
        }
        (16, true) => {
            emit1(unit, rex);
            emit1(unit, 0x0F);
            emit1(unit, 0xBF);
            modrm(unit, 3, reg_rm, rm);
        }
        // movzx：0F B6 /r 取字节、0F B7 /r 取字
        (8, false) => {
            emit1(unit, rex);
            emit1(unit, 0x0F);
            emit1(unit, 0xB6);
            modrm(unit, 3, reg_rm, rm);
        }
        (16, false) => {
            emit1(unit, rex);
            emit1(unit, 0x0F);
            emit1(unit, 0xB7);
            modrm(unit, 3, reg_rm, rm);
        }
        // movsxd r64, r/m32
        (32, true) => {
            emit1(unit, rex);
            emit1(unit, 0x63);
            modrm(unit, 3, reg_rm, rm);
        }
        // mov r32, r/m32：写 32 位子寄存器会把高 32 位清零，等于零扩展
        (32, false) => {
            emit1(unit, rex);
            emit1(unit, 0x8B);
            modrm(unit, 3, reg_rm, rm);
        }
        _ => {}
    }
}

/// 发射常量加载。
fn emit_const(unit: &mut CompilationUnit, result: SsaValue, value: SsaConst) {
    // 溢出的结果没有寄存器：先装进 rax，再写回栈槽
    let spilled = matches!(location_of(result), Location::Stack(_));
    let dst = if spilled { SCRATCH_AX } else { alloc_reg(result) };
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
        SsaConst::Float(_) => crate::codegen::unsupported_const("float"),
        SsaConst::Str(_) => crate::codegen::unsupported_const("string"),
    }
    if spilled {
        if let Location::Stack(slot) = location_of(result) {
            emit_store_slot(unit, dst, slot);
        }
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
    use SsaBinaryOp::*;

    // 操作数可能是溢出的值：先按需从栈槽装进临时寄存器。
    // 分配器保证溢出值没有固定寄存器，所以这里用的是**不参与分配**的
    // `rax` / `rdx`。
    let lhs_loc = location_of(lhs);
    let rhs_loc = location_of(rhs);
    let l_temp = match lhs_loc {
        Location::Stack(slot) => {
            emit_load_slot(unit, SCRATCH_AX, slot);
            Some(SCRATCH_AX)
        }
        Location::Reg(_) => None,
    };
    let r_temp = match rhs_loc {
        Location::Stack(_) => {
            // `div`/`rem` 内部要用 `rax:rdx` 放被除数，`cqo` 还会把 rdx
            // 写掉，所以除数不能借用这两个寄存器——借一个分配池里的，
            // 用 push/pop 保护它当下的内容。
            if matches!(op, Div | Rem) {
                let slot = match rhs_loc {
                    Location::Stack(slot) => slot,
                    Location::Reg(_) => unreachable!(),
                };
                let protect = ALLOCATABLE[X86_64_CALLEE_SAVE[0]];
                emit_push_r(unit, protect);
                emit_load_slot(unit, protect, slot);
                Some(protect)
            } else {
                // 两个操作数都溢出时只能用第二个临时寄存器
                let tmp = if l_temp.is_some() { SCRATCH_DX } else { SCRATCH_AX };
                let slot = match rhs_loc {
                    Location::Stack(slot) => slot,
                    Location::Reg(_) => unreachable!(),
                };
                emit_load_slot(unit, tmp, slot);
                Some(tmp)
            }
        }
        Location::Reg(_) => None,
    };

    // 结果寄存器：溢出的结果先算进一个临时寄存器，最后再存回栈槽。
    // 除法/取模要占用 rax:rdx，比较要用 setcc al，移位要占 cl ——
    // 这几种运算的结果临时寄存器换用 rcx，免得和它们打架。
    let result_spilled = matches!(location_of(result), Location::Stack(_));
    let scratch = match op {
        Div | Rem | Eq | Ne | Lt | Gt | Le | Ge => SCRATCH_CX,
        _ => SCRATCH_AX,
    };
    let dst = if result_spilled { scratch } else { alloc_reg(result) };
    let l = l_temp.unwrap_or_else(|| alloc_reg(lhs));
    let r = r_temp.unwrap_or_else(|| alloc_reg(rhs));

    // 运算体
    let _ = &mut ();
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
        Mul => {
            emit_mul(unit, dst, l, r);
            if r_temp.is_some() {
                emit_pop_r(unit, r);
            }
        }
        Div => {
            emit_divrem(unit, dst, l, r, false);
            if r_temp.is_some() {
                emit_pop_r(unit, r);
            }
        }
        Rem => {
            emit_divrem(unit, dst, l, r, true);
            if r_temp.is_some() {
                emit_pop_r(unit, r);
            }
        }
        Shl => emit_shift(unit, 4, dst, l, r), // shl r/m64, cl
        Shr => emit_shift(unit, 7, dst, l, r), // sar r/m64, cl（算术右移）
        // 比较运算：cmp l, r; setcc al; movzx dst, al
        Eq | Ne | Lt | Gt | Le | Ge => {
            emit_cmp_rr(unit, l, r);
            emit_setcc(unit, condition_code(op), SCRATCH_AX);
            emit_movzx(unit, dst, SCRATCH_AX);
        }
    }

    // 溢出结果写回栈槽
    if result_spilled {
        emit_store_slot(unit, dst, match location_of(result) {
            Location::Stack(slot) => slot,
            Location::Reg(_) => unreachable!(),
        });
    }
}

/// 发射一元运算。
fn emit_unary(unit: &mut CompilationUnit, result: SsaValue, op: crate::ssa::SsaUnaryOp, operand: SsaValue) {
    let spilled = matches!(location_of(result), Location::Stack(_));
    let dst = if spilled { SCRATCH_AX } else { alloc_reg(result) };
    let r = match location_of(operand) {
        Location::Reg(i) => ALLOCATABLE[i],
        Location::Stack(slot) => {
            emit_load_slot(unit, SCRATCH_DX, slot);
            SCRATCH_DX
        }
    };

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
            // REX.B 扩展的是 ModRM 的 **r/m** 字段，而这里是目的寄存器。
            // 漏掉它的话，`not r9` 会被编码成 `not rcx`——写错寄存器，
            // 而且不报错。
            emit1(unit, 0x48 | if dst.rex_bit() { 0x01 } else { 0 });
            emit1(unit, 0xf7);
            modrm(unit, 3, 2, dst.bits()); // /2 = NOT
        }
        Neg => {
            // mov dst, r; neg dst
            if dst != r {
                emit_mov_rr(unit, dst, r);
            }
            emit1(unit, 0x48 | if dst.rex_bit() { 0x01 } else { 0 });
            emit1(unit, 0xf7);
            modrm(unit, 3, 3, dst.bits()); // /3 = NEG
        }
    }

    // 溢出结果写回栈槽
    if spilled {
        if let Location::Stack(slot) = location_of(result) {
            emit_store_slot(unit, dst, slot);
        }
    }
}

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
    let mut moves: Vec<(Gpr, Src)> = Vec::new();
    for i in 0..n {
        let src = src_of(args[i]);
        if !matches!(src, Src::Reg(r) if r == ARG_REGS[i]) {
            moves.push((ARG_REGS[i], src));
        }
    }
    // 超过 6 个的实参走栈上传参区，本后端尚未实现。

    emit_parallel_moves(unit, &moves);
}

/// 并行搬运 `(dst, src)`：允许目标重叠，靠环检测保证正确。
///
/// 直接按顺序发 `mov` 会在有环时互相覆盖（例如
/// `rdi->rsi` 与 `rsi->rdi`），所以先发那些"目标没被别的搬运当源读"的，
/// 剩下的环用 `rax`（不参与分配）中转打开。
///
/// 源可以是栈槽（溢出值）。内存源不会被别的搬运读寄存器时踩到，但它
/// 自己仍然可能覆盖某个源寄存器——所以它也要参与同一套环检测。
fn emit_parallel_moves(unit: &mut CompilationUnit, moves: &[(Gpr, Src)]) {
    let mut moves: Vec<(Gpr, Src)> = moves
        .iter()
        .copied()
        .filter(|(dst, src)| !matches!(src, Src::Reg(r) if *r == *dst))
        .collect();
    while !moves.is_empty() {
        // 目标不再被任何待执行的搬运读取 -> 可以安全执行
        let safe = moves.iter().position(|(dst, _)| {
            !moves
                .iter()
                .any(|(_, src)| matches!(src, Src::Reg(r) if *r == *dst))
        });
        match safe {
            Some(at) => {
                let (dst, src) = moves.remove(at);
                emit_mov_src(unit, dst, src);
            }
            None => {
                // 存在环：把第一个搬运的源暂存到 rax，把环打开
                if let Src::Reg(src) = moves[0].1 {
                    emit_mov_rr(unit, SCRATCH_AX, src);
                    for m in moves.iter_mut() {
                        if m.1 == Src::Reg(src) {
                            m.1 = Src::Reg(SCRATCH_AX);
                        }
                    }
                } else {
                    // 没有纯寄存器的环可打破（正常不会走到这里）
                    let (dst, src) = moves.remove(0);
                    emit_mov_src(unit, dst, src);
                }
            }
        }
    }
}

/// `mov dst, src`，src 可以是寄存器或栈槽。
fn emit_mov_src(unit: &mut CompilationUnit, dst: Gpr, src: Src) {
    match src {
        Src::Reg(r) => emit_mov_rr(unit, dst, r),
        Src::Mem(slot) => emit_load_slot(unit, dst, slot),
    }
}

/// 某个值的搬运来源。
fn src_of(v: SsaValue) -> Src {
    match location_of(v) {
        Location::Reg(i) => Src::Reg(ALLOCATABLE[i]),
        Location::Stack(slot) => Src::Mem(slot),
    }
}

/// 函数序言里搬运形参：ABI 实参寄存器 -> 分配器给形参选的寄存器。
///
/// 之前完全没有这一步，形参直接从"分配器选的寄存器"读，而调用方是按
/// SysV 约定（rdi/rsi/rdx/...）放的实参，两者对不上就会读到垃圾。
fn emit_param_moves(unit: &mut CompilationUnit, func: &SsaFunction) {
    let n = func.params.len().min(ARG_REGS.len());
    // 溢出形参没有固定寄存器：从 ABI 寄存器直接落进栈槽。这些 store
    // 只写内存、不碰寄存器，所以可以先于寄存器搬运做。
    for i in 0..n {
        if let Location::Stack(slot) = location_of(func.params[i]) {
            emit_store_slot(unit, ARG_REGS[i], slot);
        }
    }
    let moves: Vec<(Gpr, Src)> = (0..n)
        .filter_map(|i| match location_of(func.params[i]) {
            Location::Reg(d) => Some((ALLOCATABLE[d], Src::Reg(ARG_REGS[i]))),
            Location::Stack(_) => None,
        })
        .collect();
    emit_parallel_moves(unit, &moves);
    // 超过 6 个的形参走栈上传参区，本后端尚未实现。
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
    // 必须用 `cqo`（rax 的符号扩展进 rdx），**不能**用 `xor edx, edx`：
    // 后者只在被除数为非负时等价。负数除法会算出完全错误的商和余数。
    emit2(unit, 0x48, 0x99); // cqo
    let mut rex = 0x48u8;
    if r.rex_bit() { rex |= 0x01; }
    emit1(unit, rex);
    emit1(unit, 0xf7);
    modrm(unit, 3, 7, r.bits()); // /7 = IDIV
    emit_mov_rr(unit, dst, if want_rem { SCRATCH_DX } else { SCRATCH_AX });
}

/// 移位 `shl r/m64, cl` 或 `sar r/m64, cl`：REX.W + D3 /4 (shl) 或 /7 (sar)。
/// x86 移位计数固定使用 CL——这是 `rcx` 不参与分配的原因。
///
/// 右移一律用**算术**右移：IR 只有一个 `Shr`，没有区分有无符号，而
/// 整数字面量默认是 i32——对有符号数 `>>` 必须补符号位（`sar`）。
/// 无符号右移需要 IR 里另加一个操作码。
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
        // 必须是 `cqo`（rax 的符号扩展进 rdx:rax），不能是 `xor edx, edx`——
        // 后者只在被除数非负时等价，负数除法会算错。
        assert_eq!(&u.code[3..5], &[0x48, 0x99], "cqo");
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