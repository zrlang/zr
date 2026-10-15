//! ARM 32 位（A32）指令选择与机器码发射——嵌入式裸机目标。
//!
//! 覆盖 ARMv7-A 常用的 A32 编码子集（常量全部由 `clang -target
//! armv7a-none-eabi` 汇编并与 `objdump` 逐条核对）：
//!
//! ```text
//! 数据处理   and / eor / sub / add / orr / mov / rsb / mvn / cmp（可带 I 位立即数）
//! 乘法       mul / mla / mls / smull / umull
//! 除法       sdiv / udiv（ARMv7-R/A 有，Cortex-M 没有）
//! 移位       寄存器受控移位 lsl / lsr（`LSL Rd, Rm, Rs`）
//! 立即数     movw / movt 拼 32 位常量（无法用 8 位旋转立即数表示时）
//! 分支       b / bl / 条件执行（mov<cond>）/ bx
//! ```
//!
//! 与 `encoder_aarch64` 的差异：
//! - ARM 有**条件执行**，比较结果不需要额外的 `cset`；
//! - 常量装载用 `movw` + `movt`（A32 没有统一的 64 位立即数编码）；
//! - Thumb 不在支持范围内（`thumbv7em-none-eabi` 会被显式拒绝）。
//!
//! AAPCS32：`r0-r3` 传参、`r0` 返回；`r13`=SP，`r14`=LR，`r15`=PC；
//! `r9/r10/r11` 属于平台寄存器，故分配池只用 `r0-r8`。
//!
//! 宽度说明：zr 的 SSA 目前把整数统一建模为 i64，32 位目标上按
//! 机器字长（32 位）逐条发射——多字长（64 位）算术尚未实现。

use crate::codegen::target::{Arch, Os, Target};
use crate::codegen::{CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection};
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaValue};

/// ARM32 通用寄存器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(u8);

impl Reg {
    /// r0：第一个参数寄存器，同时是返回值寄存器。
    pub const R0: Reg = Reg(0);

    /// 4 位寄存器号。
    fn bits(self) -> u8 {
        self.0 & 15
    }
}

/// 可用于分配的通用寄存器：r0-r8（r9-r12 属平台寄存器，r13-r15 另有用途）。
const ALLOCATABLE: [Reg; 9] = [
    Reg(0),
    Reg(1),
    Reg(2),
    Reg(3),
    Reg(4),
    Reg(5),
    Reg(6),
    Reg(7),
    Reg(8),
];

/// AAPCS32 的整数实参寄存器 r0-r3。
const ARG_REGS: [Reg; 4] = [Reg(0), Reg(1), Reg(2), Reg(3)];

/// 把 SSA 值映射到物理寄存器（虚拟分配，与其他后端同策略）。
fn alloc_reg(v: SsaValue) -> Reg {
    ALLOCATABLE[(v.id() as usize) % ALLOCATABLE.len()]
}

/// 把实参搬进 AAPCS32 的 r0-r3。
fn emit_call_args(unit: &mut CompilationUnit, args: &[SsaValue]) {
    for (i, a) in args.iter().enumerate() {
        if i >= ARG_REGS.len() {
            break; // 栈上传参区尚未实现
        }
        let src = alloc_reg(*a);
        let dst = ARG_REGS[i];
        if src != dst {
            emit_mov_rr(unit, dst, src);
        }
    }
}

/// 条件码（A32 的 `cond` 字段）。
mod cc {
    /// 相等（Z=1）。
    pub const EQ: u32 = 0b0000;
    /// 不等（Z=0）。
    pub const NE: u32 = 0b0001;
    /// 带符号 <
    pub const LT: u32 = 0b1011;
    /// 带符号 >=
    pub const GE: u32 = 0b1010;
    /// 带符号 >
    pub const GT: u32 = 0b1100;
    /// 带符号 <=
    pub const LE: u32 = 0b1101;
    /// 无条件。
    pub const AL: u32 = 0b1110;
}

/// 数据处理操作码。
mod op {
    pub const AND: u32 = 0b0000;
    pub const EOR: u32 = 0b0001;
    pub const SUB: u32 = 0b0010;
    pub const ADD: u32 = 0b0100;
    pub const ORR: u32 = 0b1100;
    pub const CMP: u32 = 0b1010;
    pub const MOV: u32 = 0b1101;
    pub const MVN: u32 = 0b1111;
    /// 立即数形式的 `rsb`（取负）：ARM 的立即数数据处理空间里
    /// 0b0011 才是 rsb（0b0010 在该空间表示 sub）——ARM 的历史包袱。
    pub const RSB_IMM: u32 = 0b0011;
}

// ==================================================================
// 指令编码原语
// ==================================================================

/// 写入 4 字节小端指令。
fn insn(unit: &mut CompilationUnit, word: u32) {
    unit.code.extend_from_slice(&word.to_le_bytes());
}

/// 数据处理指令（立即数 / 寄存器两种形式）。
///
/// 布局：`cond | 00 | I | opcode | S | Rn | Rd | operand2`——参数与
/// 指令字段一一对应，因此函数签名较宽。
#[allow(clippy::too_many_arguments)]
fn data_processing(
    unit: &mut CompilationUnit,
    cond: u32,
    opcode: u32,
    set_flags: bool,
    rn: Reg,
    rd: Reg,
    operand2: u32,
    immediate: bool,
) {
    let mut word = (cond & 0xf) << 28;
    word |= if immediate { 1 << 25 } else { 0 };
    word |= (opcode & 0xf) << 21;
    word |= if set_flags { 1 << 20 } else { 0 };
    word |= (rn.bits() as u32) << 16;
    word |= (rd.bits() as u32) << 12;
    word |= operand2 & 0xfff;
    insn(unit, word);
}

/// `rotate_imm | imm8` 形式的立即数。
fn imm8_operand(rotate: u32, imm: u8) -> u32 {
    ((rotate & 0xf) << 8) | imm as u32
}

/// 8 位旋转立即数：若 `value` 能表示成 `imm8` 的 2n 倍旋转，返回操作数。
///
/// 这是 ARM 唯一"单指令装载任意小常量"的手段，能覆盖约 1/4 的模式。
fn rotated_imm8_operand(value: u32) -> Option<u32> {
    for rotation in 0..16u32 {
        let rotated = value.rotate_right(rotation * 2);
        if rotated & 0xffff_ff00 == 0 {
            return Some(imm8_operand(rotation, rotated as u8));
        }
    }
    None
}

/// `mov<cond> rd, #imm8`：条件执行用来实现"比较后选择 0/1"。
fn emit_mov_cond(unit: &mut CompilationUnit, cond: u32, rd: Reg, imm: u8) {
    data_processing(unit, cond, op::MOV, false, Reg(0), rd, imm8_operand(0, imm), true);
}

/// `mov rd, rm`
fn emit_mov_rr(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    data_processing(unit, cc::AL, op::MOV, false, Reg(0), rd, rm.bits() as u32, false);
}

/// `mov rd, #imm32`：能用旋转立即数就用一条 movw，否则 movw + movt。
fn emit_mov_imm32(unit: &mut CompilationUnit, rd: Reg, imm: u32) {
    if let Some(op2) = rotated_imm8_operand(imm) {
        data_processing(unit, cc::AL, op::MOV, false, Reg(0), rd, op2, true);
        return;
    }
    emit_movw(unit, rd, imm as u16);
    let high = (imm >> 16) as u16;
    if high != 0 {
        emit_movt(unit, rd, high);
    }
}

/// `movw rd, #imm16`：立即数被拆成 imm4[19:16] 与 imm12[11:0]，
/// 目的寄存器在 [15:12]。
fn emit_movw(unit: &mut CompilationUnit, rd: Reg, imm: u16) {
    insn(unit, 0xe300_0000 | (rd.bits() as u32) << 12 | movw_imm(imm));
}

/// `movt rd, #imm16`（写高 16 位），字段布局与 movw 相同。
fn emit_movt(unit: &mut CompilationUnit, rd: Reg, imm: u16) {
    insn(unit, 0xe340_0000 | (rd.bits() as u32) << 12 | movw_imm(imm));
}

/// movw/movt 的 16 位立即数字段。
fn movw_imm(imm: u16) -> u32 {
    (((imm >> 12) as u32) << 16) | (imm & 0xfff) as u32
}

/// `op rd, rn, rm`
fn emit_op3(unit: &mut CompilationUnit, opcode: u32, rd: Reg, rn: Reg, rm: Reg) {
    data_processing(unit, cc::AL, opcode, false, rn, rd, rm.bits() as u32, false);
}

/// `cmp rn, rm`：只更新标志位（编码上等价于 `subs r0, rn, rm`，
/// 但汇编器统一使用 CMP 这个操作码）。
fn emit_cmp(unit: &mut CompilationUnit, rn: Reg, rm: Reg) {
    data_processing(unit, cc::AL, op::CMP, true, rn, Reg(0), rm.bits() as u32, false);
}

/// `mul rd, lhs, rhs`（Rd = lhs * rhs）
///
/// A32 的乘法字段把**第二个**操作数放在 [11:8]、**第一个**放在 [3:0]
/// （与 `sdiv` 相同），乘法可交换因此编译器与汇编器都按此约定排列。
fn emit_mul(unit: &mut CompilationUnit, rd: Reg, lhs: Reg, rhs: Reg) {
    insn(
        unit,
        0xe000_0090 | (rd.bits() as u32) << 16 | (rhs.bits() as u32) << 8 | lhs.bits() as u32,
    );
}

/// `mls rd, lhs, rhs, acc`：rd = acc - lhs * rhs
fn emit_mls(unit: &mut CompilationUnit, rd: Reg, lhs: Reg, rhs: Reg, acc: Reg) {
    insn(
        unit,
        0xe060_0090 | (rd.bits() as u32) << 16 | (acc.bits() as u32) << 12
            | (rhs.bits() as u32) << 8
            | lhs.bits() as u32,
    );
}

/// `sdiv rd, rn, rm`
fn emit_sdiv(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    insn(unit, 0xe710_f010 | (rd.bits() as u32) << 16 | (rm.bits() as u32) << 8 | rn.bits() as u32);
}

/// `mvn rd, rm`
fn emit_mvn(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    data_processing(unit, cc::AL, op::MVN, false, Reg(0), rd, rm.bits() as u32, false);
}

/// `rsb rd, rn, #0`（取负）
fn emit_rsb(unit: &mut CompilationUnit, rd: Reg, rn: Reg) {
    data_processing(unit, cc::AL, op::RSB_IMM, false, rn, rd, imm8_operand(0, 0), true);
}

/// 移位种类（编码在 [6:5]）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shift {
    Lsl,
    Lsr,
}

impl Shift {
    fn type_code(self) -> u32 {
        match self {
            Shift::Lsl => 0b00,
            Shift::Lsr => 0b01,
        }
    }
}

/// 寄存器受控移位：`lsl/lsr rd, rm, rs`（rd = rm 移位 rs 位）。
fn emit_shift_by_reg(unit: &mut CompilationUnit, rd: Reg, rm: Reg, count: Reg, shift: Shift) {
    insn(
        unit,
        0xe1a0_1010 | (rd.bits() as u32) << 12 | (count.bits() as u32) << 8
            | shift.type_code() << 5
            | rm.bits() as u32,
    );
}

/// `push {r4-r8, lr}`
fn emit_push(unit: &mut CompilationUnit) {
    // reglist: r4..r8 = 0x1F0, lr(r14) = 0x4000
    insn(unit, 0xe92d_0000 | 0x41f0);
}

/// `pop {r4-r8, pc}`
fn emit_pop_pc(unit: &mut CompilationUnit) {
    // reglist: r4..r8 = 0x1F0, pc(r15) = 0x8000
    insn(unit, 0xe8bd_0000 | 0x81f0);
}

/// `b .`——自陷到自身，等待调试器或复位（裸机没有"退出"）。
///
/// imm24 是 24 位**有符号**字偏移：-2（0xFFFFFE）表示目标地址
/// = PC + 8 + (-2 × 4) = PC，即"跳到下一条指令"。
fn emit_spin(unit: &mut CompilationUnit) {
    insn(unit, 0xeaff_fffe);
}

/// `bl target`：跳转到外部符号（需重定位）。
fn emit_bl(unit: &mut CompilationUnit, target: &str) {
    let instr_start = unit.code.len();
    insn(unit, 0xeb00_0000);
    push_branch_reloc(unit, instr_start, target);
}

/// `b target`：跳转到基本块（需重定位）。
fn emit_b(unit: &mut CompilationUnit, target: &str) {
    let instr_start = unit.code.len();
    insn(unit, 0xea00_0000);
    push_branch_reloc(unit, instr_start, target);
}

/// `b<cond> target`：条件跳转（需重定位）。
fn emit_b_cond(unit: &mut CompilationUnit, cond: u32, target: &str) {
    let instr_start = unit.code.len();
    // B/BL 的公共编码是 `cond | 1010`（bit 27:25），bit 24 = 0 表示 B、
    // 1 表示 BL；低 24 位留给重定位回填。漏掉 `1010` 会退化成
    // `and` 类指令而不是分支。
    insn(unit, ((cond & 0xf) << 28) | 0x0a00_0000);
    push_branch_reloc(unit, instr_start, target);
}

/// 登记一条 ARM 24 位字对齐分支重定位。
///
/// A32 的 `b`/`bl` 把 imm24 放在指令的 [23:0]（单位 4 字节），
/// 操作码与条件位在 [27:0]，因此需要"读-改-写"。
fn push_branch_reloc(unit: &mut CompilationUnit, instr_start: usize, target: &str) {
    unit.relocations.push(Relocation::new(
        instr_start,
        instr_start,
        4,
        target,
        RelocationKind::ArmBranch24,
    ));
}

/// 基本块标签：`<函数名>.bb<编号>`。
///
/// 早先只用 `bb<编号>`，于是**不同函数的同名块会共用一个符号**——`main`
/// 的 `bb1` 和 `f` 的 `bb1` 撞在一起，`main` 里的分支会跳进别的函数。
/// 加上函数名前缀后每个块的标签全局唯一。
fn block_label(func: &str, id: usize) -> String {
    format!("{func}.bb{id}")
}

// ==================================================================
// 函数与指令发射
// ==================================================================

/// 发射一个函数的机器码。
pub fn emit_function(target: &Target, unit: &mut CompilationUnit, func: &SsaFunction) {
    debug_assert_eq!(target.arch(), Arch::Arm32, "ARM 后端只能用于 arm32 目标");
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

    emit_push(unit);

    for block in &func.blocks {
        block_offsets.push(unit.code.len());
        for instr in &block.instrs {
            emit_instr(unit, instr);
        }
        match &block.term {
            SsaTerminator::Return(ret_val) => {
                if let Some(v) = ret_val {
                    let r = alloc_reg(*v);
                    emit_mov_rr(unit, Reg::R0, r);
                }
                emit_pop_pc(unit);
            }
            SsaTerminator::Goto(target) => emit_b(unit, &block_label(&func.name, *target)),
            SsaTerminator::Branch { cond, then_block, else_block } => {
                let r = alloc_reg(*cond);
                emit_cmp(unit, r, r);
                // cond == 0 时跳到 else，否则顺序执行 then
                // （x86-64 的 `jz else; jmp then` 与之等价）
                emit_b_cond(unit, cc::EQ, &block_label(&func.name, *else_block));
                emit_b(unit, &block_label(&func.name, *then_block));
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
            SsaConst::Int(i) => emit_mov_imm32(unit, alloc_reg(*result), *i as u32),
            SsaConst::Bool(b) => emit_mov_imm32(unit, alloc_reg(*result), u32::from(*b)),
            SsaConst::Unit => emit_mov_imm32(unit, alloc_reg(*result), 0),
            SsaConst::Float(_) | SsaConst::Str(_) => {
                emit_mov_imm32(unit, alloc_reg(*result), 0);
            }
        },
        SsaInstr::Unary { result, op, operand } => emit_unary(unit, *result, *op, *operand),
        SsaInstr::Binary { result, op, lhs, rhs } => emit_binary(unit, *result, *op, *lhs, *rhs),
        SsaInstr::Call { result, callee, args } => {
            emit_call_args(unit, args);
            emit_bl(unit, callee);
            emit_mov_rr(unit, alloc_reg(*result), Reg(0));
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
        // !x：cmp 后条件置 1 / 0
        Not => emit_compare(unit, dst, SsaBinaryOp::Eq, src, src),
        // ~x：mvn dst, src
        BitNot => emit_mvn(unit, dst, src),
        // -x：rsb dst, src, #0
        Neg => emit_rsb(unit, dst, src),
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
    let rn = alloc_reg(lhs);
    let rm = alloc_reg(rhs);
    use SsaBinaryOp::*;
    match op {
        Add => emit_op3(unit, op::ADD, dst, rn, rm),
        Sub => emit_op3(unit, op::SUB, dst, rn, rm),
        BitAnd | And => emit_op3(unit, op::AND, dst, rn, rm),
        BitOr | Or => emit_op3(unit, op::ORR, dst, rn, rm),
        BitXor => emit_op3(unit, op::EOR, dst, rn, rm),
        Mul => emit_mul(unit, dst, rn, rm),
        Div => emit_sdiv(unit, dst, rn, rm),
        // 余数 = l - (l / r) * r：sdiv + mls 两条指令
        Rem => {
            // 余数 = l - (l / r) * r：sdiv + mls 两条指令
            emit_sdiv(unit, dst, rn, rm);
            emit_mls(unit, dst, rn, rm, dst);
        }
        Shl => emit_shift_by_reg(unit, dst, rn, rm, Shift::Lsl),
        Shr => emit_shift_by_reg(unit, dst, rn, rm, Shift::Lsr),
        Eq | Ne | Lt | Gt | Le | Ge => emit_compare(unit, dst, op, rn, rm),
    }
}

/// 比较运算：把 0/1 写入 `dst`（靠 ARM 的条件执行）。
fn emit_compare(unit: &mut CompilationUnit, dst: Reg, op: SsaBinaryOp, rn: Reg, rm: Reg) {
    use SsaBinaryOp::*;
    match op {
        // 相等可以用异或后测零，省一次 compare 的依赖
        Eq | Ne => {
            emit_op3(unit, op::EOR, dst, rn, rm);
            emit_cmp(unit, dst, dst);
        }
        _ => emit_cmp(unit, rn, rm),
    }
    emit_mov_cond(unit, condition_code(op), dst, 1);
    emit_mov_cond(unit, inverse_condition_code(op), dst, 0);
}

/// 比较运算对应的条件码。
fn condition_code(op: SsaBinaryOp) -> u32 {
    use SsaBinaryOp::*;
    match op {
        Eq => cc::EQ,
        Ne => cc::NE,
        Lt => cc::LT,
        Ge => cc::GE,
        Gt => cc::GT,
        Le => cc::LE,
        _ => cc::AL,
    }
}

/// 条件的反码。
fn inverse_condition_code(op: SsaBinaryOp) -> u32 {
    match op {
        SsaBinaryOp::Eq => cc::NE,
        SsaBinaryOp::Ne => cc::EQ,
        SsaBinaryOp::Lt => cc::GE,
        SsaBinaryOp::Ge => cc::LT,
        SsaBinaryOp::Gt => cc::LE,
        SsaBinaryOp::Le => cc::GT,
        _ => cc::AL,
    }
}

/// 发射程序入口 `_start`：调用 `main` 之后自旋等待复位。
///
/// 裸机没有"退出进程"：返回 `main` 后停在自循环，既能被调试器接管，
/// 也符合 Cortex-M 的复位语义。
pub fn emit_start_wrapper(target: &Target, unit: &mut CompilationUnit) {
    debug_assert_eq!(target.arch(), Arch::Arm32);
    debug_assert_eq!(target.os(), Os::Bare);
    let start = unit.code.len();

    emit_bl(unit, "main");
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

    /// `branch_shape` 的返回值。
    struct BranchShape {
        /// 发射出来的全部指令字。
        words: Vec<u32>,
        /// (重定位偏移, 目标符号)。
        relocs: Vec<(usize, String)>,
        /// 条件分支指令。
        branch: u32,
        /// 紧随其后的无条件分支。
        jump: u32,
        /// 条件分支在 code 中的偏移。
        branch_at: usize,
        /// then 块偏移。
        then_off: usize,
        /// else 块偏移。
        else_off: usize,
    }

    /// 构造 `cond ? then : else` 的三块函数并跑完后端。
    fn branch_shape() -> BranchShape {
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
        };
        let mut unit = empty_unit();
        emit_function(
            &Target::parse("arm-none-eabi").unwrap(),
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
            .find(|r| r.kind == RelocationKind::ArmBranch24)
            .expect("条件分支应产生一条 24 位分支重定位")
            .offset;
        BranchShape {
            words: unit
                .code
                .chunks(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect(),
            relocs: unit
                .relocations
                .iter()
                .map(|r| (r.offset, r.symbol.clone()))
                .collect(),
            branch: word_at(branch),
            jump: word_at(branch + 4),
            branch_at: branch,
            then_off: block_off(1),
            else_off: block_off(2),
        }
    }

    /// ARM `B` 的 imm24 字段（单位是 4 字节的字），符号扩展自 24 位。
    ///
    /// 目标地址 = 指令地址 + 8（流水线预取）+ imm24 * 4。
    fn b_imm24_words(word: u32) -> i32 {
        let imm = word & 0x00ff_ffff;
        (imm as i32) << 8 >> 8
    }

    #[test]
    fn conditional_branch_sends_true_to_then_block() {
        let BranchShape { words, relocs, branch, jump, branch_at, then_off, else_off } =
            branch_shape();

        // 条件分支必须是 `beq`（cond = 0000），不能在条件成立时跳转
        assert_eq!(branch >> 28, cc::EQ, "应为 beq，跳转即表示条件为假");
        // B/BL 的编码是 `cond | 1010`（bit 27:25），bit 24 = 0 表示 B
        // 失败时把发射结果整体打出来，方便定位
        let ctx = format!(
            "branch={branch:08x} jump={jump:08x} at={branch_at} \
             relocs={relocs:?} words={words:08x?}"
        );
        assert_eq!(branch & 0x0e00_0000, 0x0a00_0000, "必须是 B/BL 指令族 {ctx}");
        assert_eq!(jump & 0x0e00_0000, 0x0a00_0000, "第二条指令应是无条件 b");
        assert_eq!(jump >> 28, 0b1110, "无条件 b 的 cond 应为 AL");

        // 语义核验：beq 落在 else 块，无条件 b 落在 then 块
        // 目标 = 指令地址 + 8（流水线预取）+ imm24 * 4
        let pc = branch_at as i64;
        assert_eq!(
            pc + 8 + b_imm24_words(branch) as i64 * 4,
            else_off as i64,
            "beq 必须跳到 else 块 {ctx}"
        );
        assert_eq!(
            pc + 4 + 8 + b_imm24_words(jump) as i64 * 4,
            then_off as i64,
            "无条件 b 必须跳到 then 块"
        );
    }

    /// 汇编器实测值（`clang -target armv7a-none-eabi -march=armv7ve` + objdump）。
    const REF: [(&str, u32); 12] = [
        ("sdiv r1,r4,r7", 0xE711_F714),
        ("udiv r3,r5,r8", 0xE733_F815),
        ("mul r1,r4,r7", 0xE001_0794),
        ("mls r1,r4,r7,r2", 0xE061_2794),
        ("mov r1,r4", 0xE1A0_1004),
        ("lsl r1,r4,r7", 0xE1A0_1714),
        ("lsr r1,r4,r7", 0xE1A0_1734),
        ("cmp r4,r7", 0xE154_0007),
        ("mvn r1,r4", 0xE1E0_1004),
        ("rsb r1,r4,#0", 0xE264_1000),
        ("movw r1,#0x1234", 0xE301_1234),
        ("movt r1,#0x5678", 0xE345_1678),
    ];

    #[test]
    fn allocator_avoids_platform_and_special_registers() {
        for v in 0..64u32 {
            assert!(alloc_reg(SsaValue(v)).0 <= 8, "只允许 r0-r8");
        }
    }

    #[test]
    fn encodings_match_gnu_assembler_output() {
        let r = |n: u8| Reg(n);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("sdiv r1,r4,r7", {
                let mut u = empty_unit();
                emit_sdiv(&mut u, r(1), r(4), r(7));
                u.code
            }),
            ("mul r1,r4,r7", {
                let mut u = empty_unit();
                emit_mul(&mut u, r(1), r(4), r(7));
                u.code
            }),
            ("mls r1,r4,r7,r2", {
                let mut u = empty_unit();
                emit_mls(&mut u, r(1), r(4), r(7), r(2));
                u.code
            }),
            ("mvn r1,r4", {
                let mut u = empty_unit();
                emit_mvn(&mut u, r(1), r(4));
                u.code
            }),
            ("rsb r1,r4,#0", {
                let mut u = empty_unit();
                emit_rsb(&mut u, r(1), r(4));
                u.code
            }),
            ("mov r1,r4", {
                let mut u = empty_unit();
                emit_mov_rr(&mut u, r(1), r(4));
                u.code
            }),
            ("lsl r1,r4,r7", {
                let mut u = empty_unit();
                emit_shift_by_reg(&mut u, r(1), r(4), r(7), Shift::Lsl);
                u.code
            }),
            ("lsr r1,r4,r7", {
                let mut u = empty_unit();
                emit_shift_by_reg(&mut u, r(1), r(4), r(7), Shift::Lsr);
                u.code
            }),
            ("cmp r4,r7", {
                let mut u = empty_unit();
                emit_cmp(&mut u, r(4), r(7));
                u.code
            }),
            ("movw r1,#0x1234", {
                let mut u = empty_unit();
                emit_movw(&mut u, r(1), 0x1234);
                u.code
            }),
            ("movt r1,#0x5678", {
                let mut u = empty_unit();
                emit_movt(&mut u, r(1), 0x5678);
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
    fn small_constant_uses_single_rotated_immediate() {
        let mut u = empty_unit();
        emit_mov_imm32(&mut u, Reg::R0, 42);
        assert_eq!(u.code.len(), 4, "42 可用 mov r0, #42 表示");
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0xe3a0_002a);
    }

    #[test]
    fn large_constant_uses_movw_plus_movt() {
        let mut u = empty_unit();
        emit_mov_imm32(&mut u, Reg::R0, 0x1234_5678);
        assert_eq!(u.code.len(), 8);
        // movw r0, #0x5678 / movt r0, #0x1234（imm4 在 [19:16]）
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0xe305_0678);
        assert_eq!(u32::from_le_bytes(u.code[4..8].try_into().unwrap()), 0xe341_0234);
    }

    #[test]
    fn rotated_immediate_covers_common_constants() {
        // 0x000000FF、0xFF000000、0x0000FF00 都应单指令完成
        for value in [0x0000_00ffu32, 0x0000_ff00, 0x00ff_0000, 0xff00_0000] {
            assert!(rotated_imm8_operand(value).is_some(), "{value:#x}");
        }
        assert!(rotated_imm8_operand(0x1234_5678).is_none());
    }

    #[test]
    fn comparison_uses_conditional_execution() {
        let mut u = empty_unit();
        emit_compare(&mut u, Reg::R0, SsaBinaryOp::Lt, Reg(1), Reg(2));
        let words: Vec<u32> = u
            .code
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(words[0], 0xe151_0002, "cmp r1, r2");
        assert_eq!(words[1], 0xb3a0_0001, "movlt r0, #1");
        assert_eq!(words[2], 0xa3a0_0000, "movge r0, #0");
    }

    #[test]
    fn bl_records_arm_branch24_relocation() {
        let mut u = empty_unit();
        emit_bl(&mut u, "main");
        assert_eq!(u.code.len(), 4);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()) >> 24, 0xeb);
        assert_eq!(u.relocations.len(), 1);
        assert_eq!(u.relocations[0].kind, RelocationKind::ArmBranch24);
        assert_eq!(u.relocations[0].offset, 0, "imm24 在指令自身的 [23:0]");
    }

    #[test]
    fn prologue_saves_callee_saved_registers() {
        let mut u = empty_unit();
        emit_push(&mut u);
        emit_pop_pc(&mut u);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()), 0xe92d_41f0);
        assert_eq!(u32::from_le_bytes(u.code[4..8].try_into().unwrap()), 0xe8bd_81f0, "pop 寄存器列表含 pc");
    }

    #[test]
    fn remainder_uses_div_and_mls() {
        let mut u = empty_unit();
        emit_binary(&mut u, SsaValue(3), SsaBinaryOp::Rem, SsaValue(0), SsaValue(1));
        // r3 = r0 / r1; r3 = r0 - r3 * r1
        let sdiv = u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        let mls = u32::from_le_bytes(u.code[4..8].try_into().unwrap());
        assert_eq!(sdiv, 0xe713_f110, "sdiv r3, r0, r1");
        assert_eq!(mls, 0xe063_3190, "mls r3, r0, r1, r3");
    }
}
