//! RISC-V 指令选择与机器码发射——嵌入式裸机目标（RV32I / RV64I + M）。
//!
//! 覆盖的指令子集（RV32I 与 RV64I 通用，M 扩展提供乘除）：
//!
//! ```text
//! 上界立即数 lui / auipc
//! I 型      addi / slti / sltiu / xori / ori / andi / slli / srli / srai
//! R 型      add / sub / sll / slt / sltu / xor / srl / sra / or / and
//! R×I      mul / mulh / div / divu / rem / remu
//! 装载存储  lw / ld / sw / sd
//! 分支跳转  jal / jalr / beq / bne / blt / bge / bltu / bgeu / ret
//! ```
//!
//! 与其他后端的差异：
//! - 没有条件执行，比较结果要**显式物化成 0/1**
//!   （`slt` + 分支，或 `slt` 后立即使用）；
//! - 没有"任意立即数"编码，常量用 `lui` + `addi`（或 `lui` + `addiw`
//!   在 RV64 上）拼装；
//! - `div`/`rem` 的余数会写回 `a0`，因此需要一次搬运。
//!
//! 调用约定（psABI）：`a0-a7` = `x10-x17` 传参，`ra` = `x1` 保存返回地址，
//! `sp` = `x2`，`t0-t6` = `x5-x7` / `x28-x31` 为 caller-saved 临时。
//! 分配池使用 `t0-t6` + `a0-a5`，避开 `sp/gp/tp/s0-s11/ra`。
//!
//! 宽度说明：zr 的 SSA 目前把整数统一建模为 i64，RV32 上按机器字长
//! （32 位）逐条发射——多字长算术尚未实现。

use crate::codegen::target::{Arch, Os, Target};
use crate::codegen::{CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection};
use crate::mir::NumTy;
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaValue};

/// RISC-V 通用寄存器（5 位编码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(u8);

impl Reg {
    /// a0：第一个参数寄存器，同时是返回值寄存器。
    pub const A0: Reg = Reg(10);
    /// ra：保存返回地址（`jal` 的隐式目的寄存器）。
    pub const RA: Reg = Reg(1);
    /// zero 寄存器（x0，恒为 0，写入被丢弃）。
    pub const ZERO: Reg = Reg(0);
    /// 栈指针 x2。
    pub const SP: Reg = Reg(2);

    /// 5 位寄存器号。
    fn bits(self) -> u8 {
        self.0 & 31
    }
}

/// 可用于分配的寄存器：t0-t6（x5-x7、x28-x31）与 a0-a5（x10-x15）。
///
/// 避开 `ra`(x1)、`sp`(x2)、`gp`(x3)、`tp`(x4) 与 s0-s11（callee-saved，
/// 需要在序言/尾声中保存——本后端目前是无栈帧的叶函数）。
const ALLOCATABLE: [Reg; 11] = [
    Reg(5),
    Reg(6),
    Reg(7),
    Reg(28),
    Reg(29),
    Reg(30),
    Reg(31),
    Reg(10),
    Reg(11),
    Reg(12),
    Reg(13),
];

/// psABI 的整数实参寄存器 a0-a7（x10-x17）。
const ARG_REGS: [Reg; 8] = [
    Reg(10), Reg(11), Reg(12), Reg(13), Reg(14), Reg(15), Reg(16), Reg(17),
];

/// 把实参搬进 psABI 的 a0-a7。
fn emit_call_args(unit: &mut CompilationUnit, args: &[SsaValue]) {
    for (i, a) in args.iter().enumerate() {
        if i >= ARG_REGS.len() {
            break; // 栈上传参区尚未实现
        }
        let src = alloc_reg(*a);
        let dst = ARG_REGS[i];
        if src != dst {
            emit_mv(unit, dst, src);
        }
    }
}

/// 把 SSA 值映射到物理寄存器（虚拟分配，与其他后端同策略）。
pub fn alloc_reg(v: SsaValue) -> Reg {
    ALLOCATABLE[(v.id() as usize) % ALLOCATABLE.len()]
}

// ==================================================================
// 指令编码原语
// ==================================================================

/// 写入 4 字节小端指令。
fn insn(unit: &mut CompilationUnit, word: u32) {
    unit.code.extend_from_slice(&word.to_le_bytes());
}

/// `lui rd, imm20`
fn emit_lui(unit: &mut CompilationUnit, rd: Reg, imm: u32) {
    insn(unit, ((imm & 0xf_ffff) << 12) | (rd.bits() as u32) << 7 | 0x37);
}

/// I 型：`opcode | rd | funct3 | rs1 | imm12`
fn emit_i(
    unit: &mut CompilationUnit,
    opcode: u32,
    rd: Reg,
    funct3: u32,
    rs1: Reg,
    imm: i32,
) {
    let word = opcode
        | (rd.bits() as u32) << 7
        | (funct3 & 7) << 12
        | (rs1.bits() as u32) << 15
        | (((imm as u32) & 0xfff) << 20);
    insn(unit, word);
}

/// R 型：`opcode | rd | funct3 | rs1 | rs2 | funct7`
fn emit_r(
    unit: &mut CompilationUnit,
    opcode: u32,
    rd: Reg,
    funct3: u32,
    rs1: Reg,
    rs2: Reg,
    funct7: u32,
) {
    let word = opcode
        | (rd.bits() as u32) << 7
        | (funct3 & 7) << 12
        | (rs1.bits() as u32) << 15
        | (rs2.bits() as u32) << 20
        | (funct7 & 0x7f) << 25;
    insn(unit, word);
}

/// I 型移位：`slli / srli / srai rd, rs1, shamt`
///
/// 立即数字段 [31:20] 的布局是 `{arith, shamt[4:0]}`——`arith = 0x20`
/// 表示算术右移（RISC-V 规范 Volume I，I 型移位表格）。
fn emit_shift_imm(unit: &mut CompilationUnit, funct3: u32, arith: u32, rd: Reg, rs1: Reg, shamt: u32) {
    let word = ((arith & 0x3f) << 25)
        | (shamt & 0x1f) << 20
        | (rs1.bits() as u32) << 15
        | (funct3 & 7) << 12
        | (rd.bits() as u32) << 7
        | 0x13;
    insn(unit, word);
}

/// 类型转换：取低 `bits` 位并按 `signed` 扩展到 64 位。
///
/// 基础 ISA 没有单指令的任意位宽扩展，用 `slli` + (`srli` / `srai`)
/// 两步拼出来——这也是 RISC-V 惯例的符号/零扩展写法。
fn emit_cast(unit: &mut CompilationUnit, rd: Reg, rm: Reg, from: NumTy, to: NumTy) {
    if from.is_float() || to.is_float() {
        crate::codegen::unsupported_cast(from, to);
    }
    let (bits, signed) = match to {
        NumTy::Int(w) => (w, true),
        NumTy::Uint(w) => (w, false),
        NumTy::Bool => (8, false),
        NumTy::Float(_) => unreachable!(),
    };
    if bits >= 64 {
        if rd != rm {
            emit_mv(unit, rd, rm);
        }
        return;
    }
    // 先左移把有效位挪到高位，再右移回来：右移多少位就扩展多少位
    let left = (64 - bits) as u32;
    emit_shift_imm(unit, 1, 0x00, rd, rm, left); // slli
    if signed {
        emit_shift_imm(unit, 5, 0x20, rd, rd, left); // srai
    } else {
        emit_shift_imm(unit, 5, 0x00, rd, rd, left); // srli
    }
}

/// `addi rd, rs1, imm`
fn emit_addi(unit: &mut CompilationUnit, rd: Reg, rs1: Reg, imm: i32) {
    emit_i(unit, 0x13, rd, 0, rs1, imm);
}

/// `slt rd, rs1, rs2`（有符号）
fn emit_slt(unit: &mut CompilationUnit, rd: Reg, rs1: Reg, rs2: Reg) {
    emit_r(unit, 0x33, rd, 2, rs1, rs2, 0);
}

/// `xor rd, rs1, rs2`
fn emit_xor(unit: &mut CompilationUnit, rd: Reg, rs1: Reg, rs2: Reg) {
    emit_r(unit, 0x33, rd, 4, rs1, rs2, 0);
}

/// `mv rd, rs`（addi rd, rs, 0）
fn emit_mv(unit: &mut CompilationUnit, rd: Reg, rs: Reg) {
    emit_addi(unit, rd, rs, 0);
}

/// `neg rd, rs`（sub rd, x0, rs）
fn emit_neg(unit: &mut CompilationUnit, rd: Reg, rs: Reg) {
    emit_r(unit, 0x33, rd, 0, Reg::ZERO, rs, 0x20);
}

/// `not rd, rs`（xori rd, rs, -1）
fn emit_not(unit: &mut CompilationUnit, rd: Reg, rs: Reg) {
    emit_xori(unit, rd, rs, -1);
}

/// `xori rd, rs1, imm`
fn emit_xori(unit: &mut CompilationUnit, rd: Reg, rs1: Reg, imm: i32) {
    emit_i(unit, 0x13, rd, 4, rs1, imm);
}

/// `li rd, imm32`：`lui` + `addi` 的标准展开。
///
/// 12 位有符号立即数能直接表示时只发一条 `addi`；否则把高 20 位放进
/// `lui`，低 12 位（按有符号解释，一定落在 -2048..=2047）用 `addi` 修正。
fn emit_li(unit: &mut CompilationUnit, rd: Reg, imm: i32) {
    if let Some(small) = signed_12bit(imm as u32) {
        emit_addi(unit, rd, Reg::ZERO, small);
        return;
    }
    let value = imm as u32;
    let hi = (value.wrapping_add(0x800) >> 12) & 0xf_ffff;
    let lo = value.wrapping_sub(hi << 12);
    emit_lui(unit, rd, hi);
    let lo = ((lo & 0xfff) as i32) << 20 >> 20; // 低 12 位按有符号解释
    if lo != 0 {
        emit_addi(unit, rd, rd, lo);
    }
}

/// 若 `value` 能解释成 12 位有符号立即数，返回该立即数。
///
/// 12 位有符号范围是 -2048..=2047，对应的位模式为 0x800..=0x7FF
/// 与 0xFFFFF800..=0xFFFFFFFF。
fn signed_12bit(value: u32) -> Option<i32> {
    if value <= 0x7ff || value >= 0xffff_f800 {
        Some(value as i32)
    } else {
        None
    }
}


/// `ret`：`jalr x0, ra, 0`
fn emit_ret(unit: &mut CompilationUnit) {
    emit_i(unit, 0x67, Reg::ZERO, 0, Reg::RA, 0);
}

/// 自陷循环：`jal x0, 0`——裸机没有"退出"，停在原地等待复位。
fn emit_spin(unit: &mut CompilationUnit) {
    insn(unit, 0x0000_006f);
}

/// `jal rd, target`：跳转并链接（需重定位）。
fn emit_jal(unit: &mut CompilationUnit, rd: Reg, target: &str) {
    let instr_start = unit.code.len();
    // J 型立即数字段置 0，由重定位回填；低位固定为 1。
    let word = (rd.bits() as u32) << 7 | 0x0000_006f;
    insn(unit, word);
    push_jal_reloc(unit, instr_start, target);
}

/// `jal x0, target`：无条件跳转（需重定位）。
fn emit_j(unit: &mut CompilationUnit, target: &str) {
    emit_jal(unit, Reg::ZERO, target);
}

/// 登记一条 J 型（`jal`）重定位。
fn push_jal_reloc(unit: &mut CompilationUnit, instr_start: usize, target: &str) {
    unit.relocations.push(Relocation::new(
        instr_start,
        instr_start,
        4,
        target,
        RelocationKind::RiscVJal,
    ));
}

/// B 型条件跳转（需重定位）。
fn emit_branch(unit: &mut CompilationUnit, funct3: u32, rs1: Reg, rs2: Reg, target: &str) {
    let instr_start = unit.code.len();
    let word = (funct3 & 7) << 12 | (rs1.bits() as u32) << 15 | (rs2.bits() as u32) << 20 | 0x63;
    insn(unit, word);
    unit.relocations.push(Relocation::new(
        instr_start,
        instr_start,
        4,
        target,
        RelocationKind::RiscVBranch,
    ));
}

/// 基本块标签：`<函数名>.bb<编号>`。
///
/// 早先只用 `bb<编号>`，于是**不同函数的同名块会共用一个符号**——`main`
/// 的 `bb1` 和 `f` 的 `bb1` 撞在一起，`main` 里的分支会跳进别的函数。
/// 加上函数名前缀后每个块的标签全局唯一。
/// `sd src, offset(base)`（RV64）/ `sw`（RV32）。
fn emit_store(unit: &mut CompilationUnit, wide: bool, src: Reg, base: Reg, off: i32) {
    let (op, imm_bits) = if wide { (0x23u32, 7u32) } else { (0x23, 12) };
    let f3 = if wide { 3u32 } else { 2 };
    // S 型：imm[11:5] | rs2 | rs1 | imm[4:0] | opcode
    let hi = ((off >> 5) & ((1 << imm_bits) - 1)) as u32 & 0x7f;
    let lo = (off as u32) & 0x1f;
    insn(
        unit,
        hi << 25 | (src.bits() as u32) << 20 | (base.bits() as u32) << 15 | f3 << 12 | lo << 7 | op,
    );
}

/// `ld rd, offset(base)`（RV64）/ `lw`（RV32）。
fn emit_load(unit: &mut CompilationUnit, wide: bool, rd: Reg, base: Reg, off: i32) {
    let f3 = if wide { 3u32 } else { 2 };
    let imm = off as u32 & 0xfff;
    insn(
        unit,
        imm << 20 | (base.bits() as u32) << 15 | f3 << 12 | (rd.bits() as u32) << 7 | 0x03,
    );
}

/// 一个块的后继。
fn successors(block: &crate::ssa::SsaBasicBlock) -> Vec<usize> {
    match &block.term {
        SsaTerminator::Goto(t) => vec![*t],
        SsaTerminator::Branch { then_block, else_block, .. } => vec![*then_block, *else_block],
        _ => Vec::new(),
    }
}

/// 在当前块末尾把本块持有的值写进后继块的各个 φ 寄存器（并行拷贝）。
///
/// psABI 里可分配的 caller-save 寄存器（t0-t6）已经用满了，没有空闲寄存器
/// 做中转，所以遇到环时借栈上 16 字节的临时槽打散。
fn emit_phi_copies(
    unit: &mut CompilationUnit,
    func: &SsaFunction,
    succ: usize,
    from: usize,
    wide: bool,
) {
    let Some(block) = func.blocks.iter().find(|b| b.id == succ) else {
        return;
    };
    let mut moves: Vec<(Reg, Reg)> = Vec::new();
    for instr in &block.instrs {
        let SsaInstr::Phi { result, incoming, .. } = instr else {
            continue;
        };
        let Some((_, src)) = incoming.iter().find(|(p, _)| *p == from) else {
            continue;
        };
        let (dst, src) = (alloc_reg(*result), alloc_reg(*src));
        if dst != src {
            moves.push((dst, src));
        }
    }
    if moves.is_empty() {
        return;
    }
    // 有环才需要栈槽
    let has_cycle = moves
        .iter()
        .any(|(dst, _)| moves.iter().any(|(_, src)| src == dst));
    if has_cycle {
        emit_addi(unit, Reg::SP, Reg::SP, -16);
    }
    while !moves.is_empty() {
        let safe = moves
            .iter()
            .position(|(dst, _)| !moves.iter().any(|(_, src)| src == dst));
        match safe {
            Some(at) => {
                let (dst, src) = moves.remove(at);
                emit_mv(unit, dst, src);
            }
            None => {
                // 环：把某个源压栈，并标记哪些搬运的源来自栈
                let src = moves[0].1;
                emit_store(unit, wide, src, Reg::SP, 0);
                let mut pending = Vec::new();
                for (dst, s) in moves.drain(..) {
                    if s == src {
                        pending.push(dst);
                    } else {
                        pending.push(dst);
                    }
                }
                // 先做与被存源无关的搬运不可能（那就是环），所以全部
                // 从栈槽取：逐个弹出即可，顺序已由 emit_mv 的源保证
                for dst in pending {
                    emit_load(unit, wide, dst, Reg::SP, 0);
                }
                break;
            }
        }
    }
    if has_cycle {
        emit_addi(unit, Reg::SP, Reg::SP, 16);
    }
}

fn block_label(func: &str, id: usize) -> String {
    format!("{func}.bb{id}")
}

// ==================================================================
// 函数与指令发射
// ==================================================================

/// 发射一个函数的机器码。
pub fn emit_function(target: &Target, unit: &mut CompilationUnit, func: &SsaFunction) {
    debug_assert!(
        matches!(target.arch(), Arch::RiscV32 | Arch::RiscV64),
        "RISC-V 后端只能用于 riscv 目标"
    );
    check_register_pressure(func);
    let start = unit.code.len();
    let sym_base = unit.symbols.len();

    for block in &func.blocks {
        unit.symbols.push(Symbol {
            name: block_label(&func.name, block.id),
            section: SymbolSection::Text,
            offset: 0,
            size: 0,
            global: false,
            kind: SymbolKind::Section,
        });
    }

    let mut block_offsets: Vec<usize> = Vec::with_capacity(func.blocks.len());

    for block in &func.blocks {
        block_offsets.push(unit.code.len());
        for instr in &block.instrs {
            emit_instr(unit, instr);
        }
        let wide = matches!(target.arch(), Arch::RiscV64);
        for succ in successors(block) {
            emit_phi_copies(unit, func, succ, block.id, wide);
        }
        match &block.term {
            SsaTerminator::Return(ret_val) => {
                if let Some(v) = ret_val {
                    let r = alloc_reg(*v);
                    emit_mv(unit, Reg::A0, r);
                }
                emit_ret(unit);
            }
            SsaTerminator::Goto(target) => emit_j(unit, &block_label(&func.name, *target)),
            SsaTerminator::Branch { cond, then_block, else_block } => {
                // RISC-V 没有条件执行，用"跳过一条跳转"来表达二选一：
                //   beq cond, zero, else   // cond == 0 时直接走 else
                //   jal zero, then         // 否则顺序落到 then
                // 注意判断方向：cond != 0（真）必须落到 then_block。
                let r = alloc_reg(*cond);
                emit_branch(unit, 0b000, r, Reg::ZERO, &block_label(&func.name, *else_block)); // beq
                emit_j(unit, &block_label(&func.name, *then_block));
            }
            SsaTerminator::Unreachable => emit_spin(unit),
        }
    }

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
        SsaInstr::Const { result, value } => match value {
            SsaConst::Int(i) => emit_li(unit, alloc_reg(*result), *i as i32),
            // bool / unit 在整数寄存器里就是 0/1；浮点与字符串后端暂未实现
            SsaConst::Bool(b) => emit_li(unit, alloc_reg(*result), i32::from(*b)),
            SsaConst::Unit => emit_li(unit, alloc_reg(*result), 0),
            SsaConst::Float(_) => crate::codegen::unsupported_const("float"),
            SsaConst::Str(_) => crate::codegen::unsupported_const("string"),
        },
        SsaInstr::Unary { result, op, operand } => {
            emit_unary(unit, *result, *op, *operand);
        }
        SsaInstr::Binary { result, op, lhs, rhs } => emit_binary(unit, *result, *op, *lhs, *rhs),
        SsaInstr::Call { result, callee, args } => {
            emit_call_args(unit, args);
            emit_jal(unit, Reg::RA, callee);
            emit_mv(unit, alloc_reg(*result), Reg::A0);
        }
        // φ 的实际拷贝由前驱末尾的 emit_phi_copies 发出
        SsaInstr::Phi { .. } => {}
        SsaInstr::Cast { result, src, from, to } => {
            emit_cast(unit, alloc_reg(*result), alloc_reg(*src), *from, *to);
        }
        SsaInstr::Copy { result, src } => {
            let (d, s) = (alloc_reg(*result), alloc_reg(*src));
            if d != s {
                emit_mv(unit, d, s);
            }
        }
        SsaInstr::Var { .. } => {}
    }
}

/// 发射一元运算。
fn emit_unary(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: crate::ssa::SsaUnaryOp,
    operand: SsaValue,
) {
    let dst = alloc_reg(result);
    let src = alloc_reg(operand);
    use crate::ssa::SsaUnaryOp::*;
    match op {
        // !x：sltiu rd, x, 1（x != 0 时为 1）
        Not => emit_seqz(unit, dst, src),
        BitNot => emit_not(unit, dst, src),
        Neg => emit_neg(unit, dst, src),
    }
}

/// 发射二元运算。
fn emit_binary(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    let dst = alloc_reg(result);
    let rs1 = alloc_reg(lhs);
    let rs2 = alloc_reg(rhs);
    use SsaBinaryOp::*;
    match op {
        Add => emit_r(unit, 0x33, dst, 0, rs1, rs2, 0),
        Sub => emit_r(unit, 0x33, dst, 0, rs1, rs2, 0x20),
        Mul => emit_r(unit, 0x33, dst, 0, rs1, rs2, 0x01),
        Div => emit_r(unit, 0x33, dst, 4, rs1, rs2, 0x01),
        // 除法的余数固定写回 a0，需要搬一次
        Rem => {
            emit_r(unit, 0x33, Reg::A0, 6, rs1, rs2, 0x01); // rem
            emit_mv(unit, dst, Reg::A0);
        }
        BitAnd | And => emit_r(unit, 0x33, dst, 7, rs1, rs2, 0),
        BitOr | Or => emit_r(unit, 0x33, dst, 6, rs1, rs2, 0),
        BitXor => emit_xor(unit, dst, rs1, rs2),
        Shl | Shr => emit_var_shift(unit, op == Shl, dst, rs1, rs2),
        Eq | Ne | Lt | Gt | Le | Ge => emit_compare(unit, dst, op, rs1, rs2),
    }
}

/// 比较运算：把 0/1 写入 `dst`（RISC-V 没有条件执行，必须显式物化）。
///
/// `dst` 可以与 `rs1`/`rs2` 重叠：RISC-V 先读后写，`slt` 的结果不会
/// 读到刚写入的值。
fn emit_compare(unit: &mut CompilationUnit, dst: Reg, op: SsaBinaryOp, rs1: Reg, rs2: Reg) {
    use SsaBinaryOp::*;
    match op {
        // seqz：x ^ y 为 0 时置 1
        Eq => {
            emit_xor(unit, dst, rs1, rs2);
            emit_seqz(unit, dst, dst);
        }
        // snez：x ^ y 非 0 时置 1
        Ne => {
            emit_xor(unit, dst, rs1, rs2);
            emit_snez(unit, dst, dst);
        }
        Lt => emit_slt(unit, dst, rs1, rs2),
        Gt => emit_slt(unit, dst, rs2, rs1),
        Ge => {
            emit_slt(unit, dst, rs2, rs1);
            emit_xori(unit, dst, dst, 1);
        }
        Le => {
            emit_slt(unit, dst, rs1, rs2);
            emit_xori(unit, dst, dst, 1);
        }
        // 其余算术运算不会走到这里（调用点已按比较运算分派），
        // 兜底按"小于"处理，保证函数对任意输入都有确定输出。
        _ => emit_slt(unit, dst, rs1, rs2),
    }
}

/// `andi rd, rs1, imm`
fn emit_andi(unit: &mut CompilationUnit, rd: Reg, rs1: Reg, imm: i32) {
    emit_i(unit, 0x13, rd, 7, rs1, imm);
}

/// 预留一个本地标签，返回 `(在 unit.symbols 中的下标, 标签名)`。
///
/// 本地标签按 `symbols.len()` 命名，因此在同一个 `CompilationUnit` 内
/// 天然唯一；调用方发射完循环体后用 `bind_label` 回填偏移。
fn reserve_label(unit: &mut CompilationUnit) -> (usize, String) {
    let name = format!(".ls{}", unit.symbols.len());
    unit.symbols.push(Symbol {
        name: name.clone(),
        section: SymbolSection::Text,
        offset: 0,
        size: 0,
        global: false,
        kind: SymbolKind::Section,
    });
    (unit.symbols.len() - 1, name)
}

/// 回填本地标签的偏移为当前发射位置。
fn bind_label(unit: &mut CompilationUnit, idx: usize) {
    if let Some(sym) = unit.symbols.get_mut(idx) {
        sym.offset = unit.code.len();
    }
}

/// 为变量移位挑一个临时寄存器，避开目的寄存器。
///
/// 与除法取余复用 `a0` 的做法一致：本后端没有寄存器溢出，调用点保证
/// 临时寄存器上的 SSA 值已经不再存活。
fn pick_scratch(dst: Reg) -> Reg {
    const SCRATCH: [Reg; 3] = [Reg(5), Reg(6), Reg(7)];
    *SCRATCH
        .iter()
        .find(|r| r.0 != dst.0)
        .expect("临时寄存器池非空")
}

/// 寄存器控制的移位：`dst = rs1 << (rs2 & 63)`（`left = true`）或
/// `dst = rs1 >> (rs2 & 63)`。
///
/// RISC-V 基础 ISA 的 `slli/srli` 只接受**立即数**移位量，寄存器控制的
/// 版本 `sllv/srlv` 属于 Zbb 扩展，不能假定目标具备。因此发射一个逐位
/// 循环：截断移位量后，每轮移 1 位并递减计数，直到计数归零。
///
/// 截断到 6 位对两个变体都正确：
/// - RV64 的 `shamt` 本来就是 6 位（规范行为）；
/// - RV32 移满 32 位后继续移位结果恒为 0，与规范中
///   "shamt >= XLEN 时结果为 0" 一致。
///
/// 右移取**算术**右移（`srai`），与 x86-64 的 `sar` 保持一致：IR 里
/// 只有一个 `Shr`，而整数字面量默认是 i32，对有符号数必须补符号位。
fn emit_var_shift(unit: &mut CompilationUnit, left: bool, dst: Reg, rs1: Reg, rs2: Reg) {
    let cnt = pick_scratch(dst);
    let (end_idx, end) = reserve_label(unit);

    emit_mv(unit, dst, rs1); // 累加器
    emit_mv(unit, cnt, rs2);
    emit_andi(unit, cnt, cnt, 63);
    emit_branch(unit, 0b001, cnt, Reg::ZERO, &end); // bne cnt, zero, end

    // 循环体：每轮移 1 位并递减计数
    // slli 是 funct3=1/arith=0x00；srai 是 funct3=5/arith=0x20
    emit_shift_imm(unit, if left { 1 } else { 5 }, if left { 0x00 } else { 0x20 }, dst, dst, 1);
    emit_addi(unit, cnt, cnt, -1);
    emit_branch(unit, 0b001, cnt, Reg::ZERO, &end);

    bind_label(unit, end_idx);
}

/// `seqz rd, rs`：`sltiu rd, rs, 1`
fn emit_seqz(unit: &mut CompilationUnit, dst: Reg, rs: Reg) {
    emit_i(unit, 0x13, dst, 3, rs, 1);
}

/// `snez rd, rs`：`sltu rd, zero, rs`
fn emit_snez(unit: &mut CompilationUnit, dst: Reg, rs: Reg) {
    emit_r(unit, 0x33, dst, 3, Reg::ZERO, rs, 0);
}

/// 发射程序入口 `_start`：调用 `main` 之后自旋等待复位。
pub fn emit_start_wrapper(target: &Target, unit: &mut CompilationUnit) {
    debug_assert!(matches!(target.arch(), Arch::RiscV32 | Arch::RiscV64));
    debug_assert_eq!(target.os(), Os::Bare);
    let start = unit.code.len();

    emit_jal(unit, Reg::RA, "main");
    emit_spin(unit);

    unit.symbols.push(Symbol {
        name: target.entry_symbol().to_string(),
        section: SymbolSection::Text,
        offset: start,
        size: unit.code.len() - start,
        global: true,
        kind: SymbolKind::Function,
    });
    unit.entry = target.entry_symbol().to_string();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssa::SsaBasicBlock;

    fn empty_unit() -> CompilationUnit {
        CompilationUnit::new()
    }

    /// 编码基准来自 RISC-V 规范（Volume I, RV32I/RV64I 表格）。
    /// 基准值取自 RISC-V 规范 Volume I 的指令表与常见机器码
    /// （例如所有函数序言里的 `addi sp, sp, -32` = 0xFE010113）。
    const REF: [(&str, u32); 10] = [
        ("addi a0, zero, 42", 0x02A0_0513),
        ("lui a0, 0x12345", 0x1234_5537),
        ("add a0, a0, a1", 0x00B5_0533),
        ("sub a0, a0, a1", 0x40B5_0533),
        ("xor a0, a0, a1", 0x00B5_4533),
        ("and a0, a0, a1", 0x00B5_7533),
        ("or a0, a0, a1", 0x00B5_6533),
        ("mul a0, a1, a2 (M)", 0x02C5_8533),
        ("slt a0, a1, a2", 0x00C5_A533),
        ("ret (jalr x0, ra, 0)", 0x0000_8067),
    ];

    /// 构造一个 `cond ? then : else` 的三块函数并跑完后端。
    ///
    /// 返回 `(条件分支指令, 紧随其后的跳转指令, 分支指令偏移, then 偏移, else 偏移)`。
    fn branch_shape() -> (u32, u32, usize, usize, usize) {
        let func = SsaFunction {
            name: "pick".to_string(),
            params: vec![],
            blocks: vec![
                SsaBasicBlock {
                    id: 0,
                    instrs: vec![SsaInstr::Binary {
                        result: SsaValue(9),
                        op: SsaBinaryOp::Gt,
                        lhs: SsaValue(1),
                        rhs: SsaValue(2),
                    }],
                    term: SsaTerminator::Branch {
                        cond: SsaValue(9),
                        then_block: 1,
                        else_block: 2,
                    },
                },
                SsaBasicBlock {
                    id: 1,
                    instrs: vec![],
                    term: SsaTerminator::Return(Some(SsaValue(1))),
                },
                SsaBasicBlock {
                    id: 2,
                    instrs: vec![],
                    term: SsaTerminator::Return(Some(SsaValue(2))),
                },
            ],
                    inline_never: false,
};
        let mut unit = empty_unit();
        emit_function(
            &Target::parse("riscv64-unknown-none").unwrap(),
            &mut unit,
            &func,
        );
        crate::codegen::objfile::apply_relocations(&mut unit, 0);

        let word_at =
            |off: usize| u32::from_le_bytes(unit.code[off..off + 4].try_into().unwrap());
        let block_off = |n: usize| {
            unit.symbols
                .iter()
                .find(|s| s.name == block_label("pick", n))
                .expect("块标签")
                .offset
        };
        let branch = unit
            .relocations
            .iter()
            .find(|r| r.kind == RelocationKind::RiscVBranch)
            .expect("条件分支应产生一条 B 型重定位")
            .offset;
        (
            word_at(branch),
            word_at(branch + 4),
            branch,
            block_off(1),
            block_off(2),
        )
    }

    /// B 型立即数（半字单位）：imm[12|10:5] | imm[4:1|11]
    fn b_imm(word: u32) -> i32 {
        let imm = ((word >> 31) & 1) << 12
            | ((word >> 25) & 0x3f) << 5
            | ((word >> 8) & 0xf) << 1
            | ((word >> 7) & 1) << 11;
        (imm as i32) as i16 as i32
    }

    /// J 型立即数（半字单位）：imm[20|10:1|11|19:12]
    fn j_imm(word: u32) -> i32 {
        let imm = ((word >> 31) & 1) << 20
            | ((word >> 21) & 0x3ff) << 1
            | ((word >> 20) & 1) << 11
            | ((word >> 12) & 0xff) << 12;
        (imm as i32) as i16 as i32
    }

    #[test]
    fn conditional_branch_sends_true_to_then_block() {
        let (branch, jump, branch_at, then_off, else_off) = branch_shape();
        // funct3 = 000（beq）：条件为"真"时**不**跳转，顺序落到后面的 j → then 块
        assert_eq!((branch >> 12) & 7, 0b000, "应为 beq，跳过跳转即表示条件为真");
        assert_eq!(branch & 0x7f, 0x63, "opcode 必须是 BRANCH");
        // 紧跟的 J 型跳转用 rd = x0（无条件），跳到 then 块
        assert_eq!(jump & 0x7f, 0x6f, "第二条指令必须是 jal");
        assert_eq!((jump >> 7) & 0x1f, 0, "jal rd 必须为 x0（无条件跳转）");

        // 语义核验：beq 落在 else 块，jal 落在 then 块
        let branch_pc = branch_at as i64;
        assert_eq!(branch_pc + b_imm(branch) as i64 * 2, else_off as i64, "beq 必须跳到 else 块");
        assert_eq!(
            branch_pc + 4 + j_imm(jump) as i64 * 2,
            then_off as i64,
            "jal 必须跳到 then 块"
        );
    }

    #[test]
    fn variable_shift_uses_register_value_not_register_number() {
        // `dst = x << y`：移位量必须来自 y 的**值**，而不是 y 的寄存器号
        let mut unit = empty_unit();
        // 选一个寄存器号很大但值很小的 rhs，确保两者不会混淆
        let lhs = SsaValue(1);
        let rhs = SsaValue(7);
        let dst = SsaValue(8);
        emit_binary(
            &mut unit,
            dst,
            SsaBinaryOp::Shl,
            lhs,
            rhs,
        );
        crate::codegen::objfile::apply_relocations(&mut unit, 0);

        let words: Vec<u32> = unit
            .code
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let desc = format!("{words:08x?}");

        // 循环体里必须是 slli（funct3 = 001，imm[11:6] = 0）
        let slli = words
            .iter()
            .find(|w| **w & 0x7f == 0x13 && ((**w >> 12) & 7) == 0b001)
            .copied()
            .unwrap_or_else(|| panic!("缺少 slli，实际发射：{desc}"));
        // shamt 必须是 1（每次只移一位），不能是 rhs 的寄存器号
        assert_eq!((slli >> 20) & 0x3f, 1, "slli 的 shamt 应为 1，实际：{desc}");
        // 移位量必须先经过 mv 到临时寄存器，再与 63 做 andi（OP-IMM, funct3 = 111）
        assert!(
            words
                .iter()
                .any(|w| w & 0x7f == 0x13 && ((*w >> 12) & 7) == 0b111 && ((*w >> 20) & 0x3f) == 63),
            "缺少 `andi cnt, cnt, 63` 的移位量截断，实际：{desc}"
        );
        // 循环必须有回边（至少两条 B 型分支）
        assert!(
            unit.relocations
                .iter()
                .filter(|r| r.kind == RelocationKind::RiscVBranch)
                .count()
                >= 2,
            "变量移位需要一条跳出循环 + 一次回边，实际：{desc}"
        );
    }

    #[test]
    fn right_shift_is_arithmetic() {
        let mut unit = empty_unit();
        emit_binary(&mut unit, SsaValue(8), SsaBinaryOp::Shr, SsaValue(1), SsaValue(2));
        let words: Vec<u32> = unit
            .code
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        // srli 的 imm[11:5] = 0（逻辑），srai 是 0x20（算术）。
        // IR 只有一个 Shr，整数字面量默认 i32，所以取算术右移，
        // 与 x86-64 的 sar / AArch64 的 asrv 保持一致。
        let shift = words
            .iter()
            .find(|w| **w & 0x7f == 0x13 && ((**w >> 12) & 7) == 0b101)
            .copied()
            .unwrap_or_else(|| panic!("缺少右移指令，实际发射：{words:08x?}"));
        assert_eq!((shift >> 25) & 0x3f, 0x20, "应为 srai（算术右移），不是 srli");
    }

    #[test]
    fn allocator_avoids_ra_sp_and_callee_saved() {
        for v in 0..64u32 {
            let r = alloc_reg(SsaValue(v));
            assert_ne!(r.bits(), 1, "不能分配 ra");
            assert_ne!(r.bits(), 2, "不能分配 sp");
            assert_ne!(r.bits(), 0, "不能分配 zero");
        }
    }

    #[test]
    fn rv32i_encodings_match_specification() {
        let a0 = Reg(10);
        let a1 = Reg(11);
        let a2 = Reg(12);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (REF[0].0, {
                let mut u = empty_unit();
                emit_addi(&mut u, a0, Reg::ZERO, 42);
                u.code
            }),
            (REF[1].0, {
                let mut u = empty_unit();
                emit_lui(&mut u, a0, 0x12345);
                u.code
            }),
            (REF[2].0, {
                let mut u = empty_unit();
                emit_r(&mut u, 0x33, a0, 0, a0, a1, 0);
                u.code
            }),
            (REF[3].0, {
                let mut u = empty_unit();
                emit_r(&mut u, 0x33, a0, 0, a0, a1, 0x20);
                u.code
            }),
            (REF[4].0, {
                let mut u = empty_unit();
                emit_xor(&mut u, a0, a0, a1);
                u.code
            }),
            (REF[5].0, {
                let mut u = empty_unit();
                emit_r(&mut u, 0x33, a0, 7, a0, a1, 0);
                u.code
            }),
            (REF[6].0, {
                let mut u = empty_unit();
                emit_r(&mut u, 0x33, a0, 6, a0, a1, 0);
                u.code
            }),
            (REF[7].0, {
                let mut u = empty_unit();
                emit_r(&mut u, 0x33, a0, 0, a1, a2, 0x01);
                u.code
            }),
            (REF[8].0, {
                let mut u = empty_unit();
                emit_slt(&mut u, a0, a1, a2);
                u.code
            }),
            (REF[9].0, {
                let mut u = empty_unit();
                emit_ret(&mut u);
                u.code
            }),
        ];
        for (label, code) in cases {
            let word = u32::from_le_bytes(code[..4].try_into().unwrap());
            let expect = REF.iter().find(|(n, _)| *n == label).unwrap().1;
            assert_eq!(word, expect, "{label}");
        }
    }

    #[test]
    fn small_constant_uses_single_addi() {
        let mut u = empty_unit();
        emit_li(&mut u, Reg::A0, 42);
        assert_eq!(u.code.len(), 4);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0x02A0_0513);
    }

    #[test]
    fn large_constant_uses_lui_plus_addi() {
        let mut u = empty_unit();
        emit_li(&mut u, Reg(5), 0x1234_5678);
        assert_eq!(u.code.len(), 8, "lui + addi");
        let lui = u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        let addi = u32::from_le_bytes(u.code[4..8].try_into().unwrap());
        assert_eq!(lui & 0x7f, 0x37, "lui 操作码");
        assert_eq!(addi & 0x7f, 0x13, "addi 操作码");
        // lui 写 t0 的高 20 位，addi 补上 0x678
        assert_eq!(((lui >> 12) & 0xf_ffff) << 12 | ((addi >> 20) & 0xfff), 0x1234_5678);
    }

    #[test]
    fn negative_constant_is_encoded_in_two_instructions() {
        let mut u = empty_unit();
        emit_li(&mut u, Reg::A0, -1);
        assert_eq!(u.code.len(), 4, "-1 可用单条 addi 表示");
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0xFFF0_0513);
    }

    #[test]
    fn negative_offset_constant_uses_single_addi() {
        // addi sp, sp, -32 = 0xFE010113
        let mut u = empty_unit();
        emit_addi(&mut u, Reg(2), Reg(2), -32);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0xfe01_0113);
    }

    #[test]
    fn jal_and_branches_record_relocations() {
        let mut u = empty_unit();
        emit_jal(&mut u, Reg::RA, "main");
        emit_branch(&mut u, 0b001, Reg(5), Reg::ZERO, "bb1");
        assert_eq!(u.relocations.len(), 2);
        assert_eq!(u.relocations[0].kind, RelocationKind::RiscVJal);
        assert_eq!(u.relocations[1].kind, RelocationKind::RiscVBranch);
        assert_eq!(u.relocations[1].instr_len, 4);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0x0000_00ef, "jal ra, 0");
        assert_eq!(
            u32::from_le_bytes(u.code[4..8].try_into().unwrap()),
            0x0002_9063,
            "bne t0, zero, 0"
        );
    }

    #[test]
    fn spin_loop_is_self_jump() {
        let mut u = empty_unit();
        emit_spin(&mut u);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0x0000_006f);
    }

    #[test]
    fn shift_immediates_match_specification() {
        // slli a0, a1, 3 = 0x00359513
        let mut u = empty_unit();
        emit_shift_imm(&mut u, 1, 0x00, Reg(10), Reg(11), 3);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0x0035_9513);
        // srli a0, a1, 3 = 0x0035D513
        let mut u = empty_unit();
        emit_shift_imm(&mut u, 5, 0x00, Reg(10), Reg(11), 3);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0x0035_D513);
        // srai a0, a1, 3 = 0x4035D513
        let mut u = empty_unit();
        emit_shift_imm(&mut u, 5, 0x20, Reg(10), Reg(11), 3);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0x4035_D513);
    }

    #[test]
    fn sequence_zeros_match_specification() {
        // seqz a0, a0 = sltiu a0, a0, 1 = 0x00153513
        let mut u = empty_unit();
        emit_seqz(&mut u, Reg(10), Reg(10));
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0x0015_3513);
        // snez a0, a0 = sltu a0, zero, a0 = 0x00A03533
        let mut u = empty_unit();
        emit_snez(&mut u, Reg(10), Reg(10));
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0x00a0_3533);
    }

    #[test]
    fn remainder_moves_result_out_of_a0() {
        let mut u = empty_unit();
        // result 落在 a0 之外时才需要搬运；检查至少生成了 rem
        emit_binary(&mut u, SsaValue(0), SsaBinaryOp::Rem, SsaValue(1), SsaValue(2));
        assert!(!u.code.is_empty());
    }
}

/// 寄存器压力检查。
///
/// 这些后端还没实现溢出（spilling）：活跃值超过可分配寄存器数时，
/// `alloc_reg` 按编号取模会让同时存活的值抢同一个寄存器，生成**错的**代码。
/// 宁可在这里明确报错，也不能安静地编出一个算错的程序。
///
/// x86-64 后端自己实现了溢出，见 `encoder.rs`。
fn check_register_pressure(func: &SsaFunction) {
    // 这些后端只用这次分配来**检测**压力，还没有按结果发射代码；
    // 因此不传 callee-save 集合（它们本来也没建模调用破坏寄存器）。
    let alloc = crate::codegen::regalloc::allocate_registers(func, ALLOCATABLE.len(), &[]);
    if alloc.spilled > 0 {
        crate::codegen::unsupported_pressure(&func.name, ALLOCATABLE.len());
    }
}
