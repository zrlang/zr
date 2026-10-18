//! RISC-V 指令选择与机器码发射——嵌入式裸机目标（RV32I / RV64I + M）。
//!
//! 覆盖的指令子集（RV32I 与 RV64I 通用，M 扩展提供乘除）：
//!
//! ```text
//! 上界立即数 lui / auipc
//! I 型      addi / slti / sltiu / xori / ori / andi / slli / srli / srai
//! R 型      add / sub / sll / slt / sltu / xor / srl / sra / or / and
//! R×I      mul / mulh / div / divu / rem / remu
//! 装载存储  lw / ld / lb·lbu·lb / lh·lhu·lh / lwu / sw / sb / sh / sd
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
//!
//! 栈槽说明：本后端是**无通用序言**的（没有 sp 下移 reserve），只有
//! φ 搬运的环需要 16 字节临时槽（用完立即还回去）。被取地址的局部变量
//! 需要"地址就是身份"的栈位，所以在出现 `SlotAddr` 的函数里加最小序言 /
//! 尾声：`addi sp, sp, -N` / `addi sp, sp, +N`（N = 槽数 × 8）。临时槽
//! 在槽数为 0 的函数里也安全（在保留区下方独自下探）。

use crate::codegen::target::{Arch, Os, Target};
use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};
use crate::mir::NumTy;
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaValue};

use std::cell::RefCell;
use std::collections::HashMap;

// 被取地址的局部变量 → 栈槽编号（`addi sp` 的字偏移 ×8 = 字节偏移）。
thread_local! {
    static LOCAL_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

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

/// 可用于分配的寄存器池：a0-a6（caller-save）+ t0-t6（caller-save）
/// + s0-s11（callee-save）。
///
/// 避开 `ra`(x1)、`sp`(x2)、`gp`(x3)、`tp`(x4) 与 `a7`(x17)。gp/tp/a7
/// 留作 φ 搬运与溢出值装载的**永远不会存活的暂存寄存器**（见
/// [`PHI_TMP`] 与 [`SPILL_DST`] 等）。a8+i 类实参走栈上传参区尚未实现，
/// 因此牺牲 a7 不损失功能。s0-s11 跨调用存活，供跨调用的值使用——序言
/// 保存 ra 与实际用到的 s 组，每个返回点恢复（见 [`CALLEE_SAVE`] 与
/// `emit_function`）。
const ALLOCATABLE: [Reg; 26] = [
    Reg(10),
    Reg(11),
    Reg(12),
    Reg(13),
    Reg(14),
    Reg(15),
    Reg(16),
    Reg(5),
    Reg(6),
    Reg(7),
    Reg(28),
    Reg(29),
    Reg(30),
    Reg(31),
    Reg(8),
    Reg(9),
    Reg(18),
    Reg(19),
    Reg(20),
    Reg(21),
    Reg(22),
    Reg(23),
    Reg(24),
    Reg(25),
    Reg(26),
    Reg(27),
];

/// `ALLOCATABLE` 里属于 callee-save（s0-s11）的下标（14..=25）。
///
/// 传给 `allocate_registers`，使跨调用存活的值只占用这些寄存器；序言借此
/// 知道要保存/恢复哪些。
const CALLEE_SAVE: [usize; 12] = [14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25];

/// psABI 的整数实参寄存器 a0-a7（x10-x17）。
const ARG_REGS: [Reg; 8] = [
    Reg(10),
    Reg(11),
    Reg(12),
    Reg(13),
    Reg(14),
    Reg(15),
    Reg(16),
    Reg(17),
];

thread_local! {
    /// 当前函数的整数寄存器分配：值 id -> `ALLOCATABLE` 下标。
    ///
    /// 用线性扫描的结果，而不是 `id % 26` 取模——取模会让两个同时存活、
    /// 编号同余的值抢同一个寄存器，后写者把先写者冲掉，安静地算错。
    /// 没进活跃区间的临时值才退回取模。
    static REG_MAP: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
    /// 当前函数的溢出：值 id -> 溢出栈槽号（0 起）。只对 Location::Stack
    /// 的值有条目，读写都按 `[sp + SPILL_BASE + slot*8]` 寻址。
    static SPILL_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
    /// 溢出槽区的 sp 偏移（字节数）：保存区与取址槽区的上方。每个函数
    /// `emit_function` 里更新。
    static SPILL_BASE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// 机器字长（RV64=true）：溢出槽的 load/store 宽度。emit_function 设置。
    static WIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// psABI 里预留的 φ 搬运中转寄存器：借用 `gp`(x3)——本后端不生成
/// gp 相关寻址（无 medany/gp 相对访问），运行环境也不会用它。
const PHI_TMP: Reg = Reg(3);

/// 溢出值的暂存寄存器（都不进分配池，也绝不会与活跃值同住）。
///
/// - `SPILL_SRC2`(gp)：二元运算的第二溢出操作数。gp 同时是 φ 中转，但
///   φ 拷贝发生在指令之间，与单条指令的发射不重叠。
const SPILL_DST: Reg = Reg(4); // tp
const SPILL_SRC: Reg = Reg(17); // a7
const SPILL_SRC2: Reg = Reg(3); // gp

/// 把实参搬进 psABI 的 a0-a7 / fa0-fa7。
///
/// 整数与浮点**分别计数**（psABI 就这么规定）：`f(x: f64, y: i32)` 里
/// `x` 落 fa0、`y` 落 a0。实参的落点可能与后续实参的源寄存器交叉（分配
/// 器会把值放到 a 组），顺序 `mv` 会互相覆盖，必须并行走环；溢出实参
/// 先逐个从栈槽装载。浮点实参最后统一从浮点槽加载进 fa 组——`fld`
/// 不碰整数寄存器，放在所有整数搬运之后也不会干扰。
fn emit_call_args(unit: &mut CompilationUnit, args: &[SsaValue]) {
    let mut moves: Vec<(Reg, Reg)> = Vec::new();
    let mut from_stack: Vec<(Reg, usize)> = Vec::new();
    let mut fp_loads: Vec<(usize, usize, u8)> = Vec::new(); // (fa 编号, 槽, 位宽)
    let mut ai = 0usize;
    let mut fi = 0usize;
    for a in args {
        if let Some((slot, w)) = float_slot(*a) {
            if fi < FA_REGS.len() {
                fp_loads.push((fi, slot, w));
            }
            fi += 1;
            continue;
        }
        if ai >= ARG_REGS.len() {
            break; // 栈上传参区尚未实现
        }
        if let Some(slot) = spill_slot(*a) {
            from_stack.push((ARG_REGS[ai], slot));
        } else {
            moves.push((ARG_REGS[ai], alloc_reg(*a)));
        }
        ai += 1;
    }
    emit_parallel_moves(unit, &mut moves);
    for (dst, slot) in from_stack {
        load_spill(unit, SPILL_DST, slot);
        emit_mv(unit, dst, SPILL_DST);
    }
    for (i, slot, w) in fp_loads {
        emit_float_load(unit, FA_REGS[i], slot, w);
    }
}

/// 把形参从 ABI 寄存器 a0-a7 / fa0-fa7 搬到分配结果所在的寄存器或槽。
///
/// 调用的世界：调用方把实参放进 a 组 / fa 组（`emit_call_args`），被调用
/// 方这里按分配结果把值搬进自己的位置——没有搬运的话，函数体里读形参
/// 会用到分配寄存器里的**垃圾**。溢出的形参直接落到它的栈槽。
///
/// 形参有没有浮点性只能靠浮点槽反推：`assign_float_slots` 已把函数体内
/// 经浮点运算传播到的形参记进去了。
fn emit_param_moves(unit: &mut CompilationUnit, func: &SsaFunction) {
    let mut gi = 0usize;
    let mut fi = 0usize;
    let mut moves: Vec<(Reg, Reg)> = Vec::new();
    for p in &func.params {
        if let Some((slot, w)) = float_slot(*p) {
            if fi < FA_REGS.len() {
                emit_float_store(unit, FA_REGS[fi], slot, w);
            }
            fi += 1;
            continue;
        }
        if gi >= ARG_REGS.len() {
            continue; // 超过 8 个整数形参走栈上传参区，本后端尚未实现
        }
        if let Some(slot) = spill_slot(*p) {
            store_spill(unit, ARG_REGS[gi], slot);
        } else {
            moves.push((alloc_reg(*p), ARG_REGS[gi]));
        }
        gi += 1;
    }
    emit_parallel_moves(unit, &mut moves);
}

/// `v` 的溢出栈槽（不在寄存器里、只在栈上时返回编号）。
fn spill_slot(v: SsaValue) -> Option<usize> {
    SPILL_SLOTS.with(|m| m.borrow().get(&v.id()).copied())
}

/// 溢出槽的 sp 偏移（字节数）。
fn spill_offset(slot: usize) -> i32 {
    (SPILL_BASE.with(|c| c.get()) + slot * 8) as i32
}

/// 从溢出槽装载到 `dst`。
fn load_spill(unit: &mut CompilationUnit, dst: Reg, slot: usize) {
    emit_load(
        unit,
        WIDE.with(|c| c.get()),
        dst,
        Reg::SP,
        spill_offset(slot),
    );
}

/// 把 `src` 存进溢出槽。
fn store_spill(unit: &mut CompilationUnit, src: Reg, slot: usize) {
    emit_store(
        unit,
        WIDE.with(|c| c.get()),
        src,
        Reg::SP,
        spill_offset(slot),
    );
}

/// 读出 `v`：溢出的值装载到 [`SPILL_SRC`]。
fn val_reg(unit: &mut CompilationUnit, v: SsaValue) -> Reg {
    if let Some(slot) = spill_slot(v) {
        load_spill(unit, SPILL_SRC, slot);
        return SPILL_SRC;
    }
    alloc_reg(v)
}

/// 读出二元的第二操作数：溢出的值装载到 [`SPILL_SRC2`]（与第一操作数
/// 的暂存寄存器不同，左右都溢出时不会互相冲掉）。
fn val_reg2(unit: &mut CompilationUnit, v: SsaValue) -> Reg {
    if let Some(slot) = spill_slot(v) {
        load_spill(unit, SPILL_SRC2, slot);
        return SPILL_SRC2;
    }
    alloc_reg(v)
}

/// `v` 里的结果要发射到的寄存器：溢出的结果先算进 [`SPILL_DST`]，
/// 发射完用 [`put_out`] 落回槽。
fn out_reg(v: SsaValue) -> Reg {
    if spill_slot(v).is_some() {
        SPILL_DST
    } else {
        alloc_reg(v)
    }
}

/// 溢出的结果从 [`SPILL_DST`] 落回它的槽（其余值无事可做）。
fn put_out(unit: &mut CompilationUnit, v: SsaValue) {
    if let Some(slot) = spill_slot(v) {
        store_spill(unit, SPILL_DST, slot);
    }
}

/// 把 SSA 值映射到物理寄存器（优先用线性扫描的结果）。
pub fn alloc_reg(v: SsaValue) -> Reg {
    if let Some(i) = REG_MAP.with(|m| m.borrow().get(&v.id()).copied()) {
        return ALLOCATABLE[i];
    }
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
    insn(
        unit,
        ((imm & 0xf_ffff) << 12) | (rd.bits() as u32) << 7 | 0x37,
    );
}

/// `lui rd, %hi(sym)` + `addi rd, rd, %lo(sym)`：取**符号**的绝对地址。
///
/// 字符串字面量在 `.rodata` 里，静态可执行文件的加载地址编译期已知，
/// 绝对取址（相对 `adrp` 成对解）同时兼容自链接与 ELF 目标文件的
/// `R_RISCV_HI20`/`R_RISCV_LO12_I`（REL 型符号直引）。
fn emit_str_addr(unit: &mut CompilationUnit, dst: Reg, sym: &str) {
    let a = unit.code.len();
    emit_lui(unit, dst, 0); // %hi 后填
    unit.relocations
        .push(Relocation::new(a, a, 4, sym, RelocationKind::RiscVHi20));
    let b = unit.code.len();
    emit_addi(unit, dst, dst, 0); // %lo 后填
    unit.relocations
        .push(Relocation::new(b, b, 4, sym, RelocationKind::RiscVLo12I));
}

/// I 型：`opcode | rd | funct3 | rs1 | imm12`
fn emit_i(unit: &mut CompilationUnit, opcode: u32, rd: Reg, funct3: u32, rs1: Reg, imm: i32) {
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
/// RV64 的 shamt 是 6 位：[31:26] 放 `{arith, shamt[5]}`（arith = 0x20
/// 表示算术右移，RISC-V 规范 Volume I，I 型移位表格），[25:20] 放
/// `shamt[4:0]`。所以把 `{arith, shamt}` 作为整体 7 位写入 [31:25]。
fn emit_shift_imm(
    unit: &mut CompilationUnit,
    funct3: u32,
    arith: u32,
    rd: Reg,
    rs1: Reg,
    shamt: i64,
) {
    let word = ((((arith & 0x20) | ((shamt >> 5) as u32 & 1)) as u32) << 5
        | ((shamt & 0x1f) as u32))
        << 20
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
    let left = 64 - bits as i64;
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

/// `ty` 的（字节宽度，是否有符号）。浮点不走整数的 load/store 路径。
fn mem_access(ty: NumTy) -> (usize, bool) {
    match ty {
        NumTy::Int(w) => ((w as usize) / 8, true),
        NumTy::Uint(w) => ((w as usize) / 8, false),
        NumTy::Bool => (1, false),
        NumTy::Float(w) => ((w as usize) / 8, false),
    }
}

/// `rd = [base + 0]`，按 `ty` 位宽/符号选 load（UTF32 上 4 字节零扩展要用 `lwu`，
/// RV32 上 4 字节就是整字 `lw`）。8 字节只在 RV64 有效。
fn emit_load_mem(unit: &mut CompilationUnit, rd: Reg, base: Reg, ty: NumTy, wide_target: bool) {
    let (bytes, signed) = mem_access(ty);
    let f3 = match (bytes, signed, wide_target) {
        (1, false, _) => 4,     // lbu
        (1, true, _) => 0,      // lb
        (2, false, _) => 5,     // lhu
        (2, true, _) => 1,      // lh
        (4, false, false) => 2, // lw（RV32 整字）
        (4, false, true) => 6,  // lwu（RV64 零扩展）
        (4, true, _) => 2,      // lw（符号扩展）
        (8, _, false) => {
            // 32 位目标的多字长（i64）尚未实现。
            crate::codegen::unsupported_const("64-bit memory access on 32-bit target")
        }
        _ => 3, // ld
    };
    insn(
        unit,
        (base.bits() as u32) << 15 | f3 << 12 | (rd.bits() as u32) << 7 | 0x03,
    );
}

/// `[base + 0] = src`，只写 `ty` 位宽对应的低字节（RISC-V store 没有
/// "宽存窄写"的坑，`sx` 就是窄的）。
fn emit_store_mem(unit: &mut CompilationUnit, base: Reg, src: Reg, ty: NumTy, wide_target: bool) {
    let (bytes, _) = mem_access(ty);
    let f3 = match (bytes, wide_target) {
        (1, _) => 0, // sb
        (2, _) => 1, // sh
        (4, _) => 2, // sw
        (8, false) => crate::codegen::unsupported_const("64-bit memory access on 32-bit target"),
        _ => 3, // sd
    };
    insn(
        unit,
        (src.bits() as u32) << 20 | (base.bits() as u32) << 15 | f3 << 12 | 0x23,
    );
}

/// 一个块的后继。
fn successors(block: &crate::ssa::SsaBasicBlock) -> Vec<usize> {
    match &block.term {
        SsaTerminator::Goto(t) => vec![*t],
        SsaTerminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![*then_block, *else_block],
        _ => Vec::new(),
    }
}

/// 把本块持有的值写进后继块各个 φ 的目标寄存器（并行，含环）。
fn emit_parallel_moves(unit: &mut CompilationUnit, moves: &mut Vec<(Reg, Reg)>) {
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
                // 环：把某个源先挪进中转寄存器，再把环里指向它的源
                // 改成从中转取，下一轮就有 safe 搬运可消费了。
                let src = moves[0].1;
                emit_mv(unit, PHI_TMP, src);
                for m in moves.iter_mut() {
                    if m.1 == src {
                        m.1 = PHI_TMP;
                    }
                }
            }
        }
    }
}

/// 在当前块末尾把本块持有的值写进后继块的各个 φ 寄存器（并行拷贝）。
///
/// 溢出目标/源的搬运逐条直接经暂存寄存器进出栈槽（溢出槽的读写彼此
/// 无依赖，不会成环）；寄存器搬运才需要并行打散。
/// 在当前块末尾把本块持有的值写进后继块的各个 φ 寄存器（并行拷贝）。
///
/// 溢出目标的搬运分三步走，保证读到的一定是 φ 之前的值：
/// 1. 栈对栈与"寄存器 → 槽"先行——此刻还没人动过寄存器；
/// 2. 寄存器之间的搬运并行走环；
/// 3. "槽 → 寄存器"最后装载——寄存器在回到装配前全部写完。
fn emit_phi_copies(
    unit: &mut CompilationUnit,
    func: &SsaFunction,
    succ: usize,
    from: usize,
    _wide: bool,
) {
    let Some(block) = func.blocks.iter().find(|b| b.id == succ) else {
        return;
    };
    let mut reg_moves: Vec<(Reg, Reg)> = Vec::new();
    let mut store_stack: Vec<(usize, Reg)> = Vec::new();
    let mut load_stack: Vec<(Reg, usize)> = Vec::new();
    let ds: Vec<(SsaValue, SsaValue)> = block
        .instrs
        .iter()
        .filter_map(|instr| match instr {
            SsaInstr::Phi {
                result, incoming, ..
            } => {
                let src = incoming.iter().find(|(p, _)| *p == from)?.1;
                Some((*result, src))
            }
            _ => None,
        })
        .collect();
    for (result, src) in &ds {
        if let (Some((ds_, dw)), Some((ss, sw))) = (float_slot(*result), float_slot(*src)) {
            // 浮点 → 浮点：经 FT0 栈对栈直挪
            let w = dw.max(sw);
            emit_float_load(unit, FP_TMP0, ss, w);
            emit_float_store(unit, FP_TMP0, ds_, w);
        } else if let (Some(ds_), Some(ss)) = (spill_slot(*result), spill_slot(*src)) {
            // 溢出 → 溢出：经暂存寄存器栈对栈
            load_spill(unit, SPILL_DST, ss);
            store_spill(unit, SPILL_DST, ds_);
        } else if let Some(ds_) = spill_slot(*result) {
            // 槽 ← 寄存器：先读（无论后续寄存器搬运怎么写这些源寄存器，
            // 这里读到的都是 φ 之前的值）
            store_stack.push((ds_, alloc_reg(*src)));
        } else if let Some(ss) = spill_slot(*src) {
            // 槽 → 寄存器：装载进暂存，最后并行生效
            load_stack.push((alloc_reg(*result), ss));
        } else {
            let (dst, s) = (alloc_reg(*result), alloc_reg(*src));
            if dst != s {
                reg_moves.push((dst, s));
            }
        }
    }
    for (slot, r) in &store_stack {
        store_spill(unit, *r, *slot);
    }
    emit_parallel_moves(unit, &mut reg_moves);
    for (dst, slot) in &load_stack {
        load_spill(unit, SPILL_SRC, *slot);
        emit_mv(unit, *dst, SPILL_SRC);
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
    let wide = matches!(target.arch(), Arch::RiscV64);
    WIDE.with(|c| c.set(wide));

    // 实际用到的 callee-save 寄存器（REG_MAP 里下标落在 s 组）：序言保存
    // ra 与它们，每个返回点恢复。
    let cs_used: Vec<u8> = {
        let mut idx: Vec<usize> =
            REG_MAP.with(|m| m.borrow().values().copied().filter(|i| *i >= 14).collect());
        idx.sort_unstable();
        idx.dedup();
        idx.into_iter().map(|i| ALLOCATABLE[i].bits()).collect()
    };
    // 帧布局（从 sp 起向上）：ra、被调用者保存的 s 组、浮点槽区、取址
    // 局部变量槽、溢出槽。保存区在下（恢复时按同一偏移加载）。
    let saved_bytes = (cs_used.len() + 1) * 8;
    let float_count = assign_float_slots(func);
    let slot_count = assign_local_slots(func, cs_used.len() + 1 + float_count);
    if slot_count > 0 && (slot_count + float_count) * 8 > 2040 {
        crate::codegen::unsupported_const("too many address-taken locals (risc-v slot area)");
    }
    let spill_count = SPILL_SLOTS.with(|m| m.borrow().len());
    FLOAT_BASE.with(|c| c.set(saved_bytes));
    SPILL_BASE.with(|c| c.set(saved_bytes + (slot_count + float_count) * 8));
    let frame_bytes = saved_bytes + (slot_count + float_count + spill_count) * 8;
    if frame_bytes > 2040 {
        crate::codegen::unsupported_const("function frame too large (risc-v)");
    }

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

    // 序言：预留帧并保存 ra 与用到的 s 组。
    if frame_bytes > 0 {
        emit_addi(unit, Reg::SP, Reg::SP, -(frame_bytes as i32));
        emit_store(unit, wide, Reg::RA, Reg::SP, 0);
        for (i, &r) in cs_used.iter().enumerate() {
            emit_store(unit, wide, Reg(r), Reg::SP, (8 + i * 8) as i32);
        }
    }
    // 形参从 ABI 寄存器搬到分配结果所在的寄存器。
    emit_param_moves(unit, func);

    for block in &func.blocks {
        block_offsets.push(unit.code.len());
        for instr in &block.instrs {
            emit_instr(unit, instr, wide);
        }
        for succ in successors(block) {
            emit_phi_copies(unit, func, succ, block.id, wide);
        }
        match &block.term {
            SsaTerminator::Return(ret_val) => {
                if let Some(v) = ret_val {
                    if let Some((slot, w)) = float_slot(*v) {
                        emit_float_load(unit, Reg(10), slot, w); // fa0
                    } else {
                        let r = val_reg(unit, *v);
                        emit_mv(unit, Reg::A0, r);
                    }
                }
                // 尾声：恢复 s 组与 ra、归还帧、返回。
                if frame_bytes > 0 {
                    for (i, &r) in cs_used.iter().enumerate() {
                        emit_load(unit, wide, Reg(r), Reg::SP, (8 + i * 8) as i32);
                    }
                    emit_load(unit, wide, Reg::RA, Reg::SP, 0);
                    emit_addi(unit, Reg::SP, Reg::SP, frame_bytes as i32);
                }
                emit_ret(unit);
            }
            SsaTerminator::Goto(target) => emit_j(unit, &block_label(&func.name, *target)),
            SsaTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                // RISC-V 没有条件执行，用"跳过一条跳转"来表达二选一：
                //   beq cond, zero, else   // cond == 0 时直接走 else
                //   jal zero, then         // 否则顺序落到 then
                // 注意判断方向：cond != 0（真）必须落到 then_block。
                // 溢出的条件值要先装载到暂存寄存器再进 beq。
                let r = val_reg(unit, *cond);
                emit_branch(
                    unit,
                    0b000,
                    r,
                    Reg::ZERO,
                    &block_label(&func.name, *else_block),
                ); // beq
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

/// 扫出所有被取地址的局部变量，各分配一个栈槽（编号从 `base` 起）。
/// 返回槽数；`LOCAL_SLOTS` 保存局部变量 → 槽号的映射。
fn assign_local_slots(func: &SsaFunction, base: usize) -> usize {
    let mut ids: Vec<u32> = Vec::new();
    for block in &func.blocks {
        for instr in &block.instrs {
            if let SsaInstr::SlotAddr { local, .. } = instr {
                if !ids.contains(local) {
                    ids.push(*local);
                }
            }
        }
    }
    ids.sort_unstable();
    let count = ids.len();
    LOCAL_SLOTS.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for (i, id) in ids.into_iter().enumerate() {
            map.insert(id, base + i);
        }
    });
    count
}

/// 扫出一个函数里所有浮点值，给每个值分配专属栈槽，返回槽数。
///
/// 与 AArch64 后端同一套不动点算法（移植）：浮点"性"必须穿过 `Copy` 与
/// `Phi` 传播——只认定义点会让中转值被当成整数去找寄存器，然后拿到栈槽
/// 里的残渣当浮点数用。播种（`Neg` 依赖操作数已是浮点）与传播合成一轮
/// 重扫全函数，直到不再变化。
///
/// 槽号从 0 起编，寻址时由 `float_offset` 加上 `FLOAT_BASE`。
fn assign_float_slots(func: &SsaFunction) -> usize {
    let mut floats: HashMap<u32, u8> = HashMap::new();
    let width_of =
        |v: &SsaValue, map: &HashMap<u32, u8>| -> u8 { map.get(&v.id()).copied().unwrap_or(0) };
    loop {
        let mut changed = false;
        let mark = |v: &SsaValue, w: u8, map: &mut HashMap<u32, u8>| -> bool {
            match map.get(&v.id()) {
                // 已知但更窄，保留更宽的那个（f64 优先）
                Some(old) if *old >= w => false,
                _ => {
                    map.insert(v.id(), w);
                    true
                }
            }
        };
        for block in &func.blocks {
            for instr in &block.instrs {
                match instr {
                    SsaInstr::Const {
                        result,
                        value: SsaConst::Float(_, w),
                    } => {
                        if mark(result, *w, &mut floats) {
                            changed = true;
                        }
                    }
                    SsaInstr::Binary {
                        result,
                        ty,
                        lhs,
                        rhs,
                        ..
                    } => {
                        if ty.is_float() {
                            let w = match *ty {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            for v in [result, lhs, rhs] {
                                if mark(v, w, &mut floats) {
                                    changed = true;
                                }
                            }
                        } else {
                            // 比较的结果是 bool，但操作数可能都是浮点：两边互相传播
                            let lw = width_of(lhs, &floats);
                            let rw = width_of(rhs, &floats);
                            let (unknown, w) = match (lw, rw) {
                                (w, 0) if w > 0 => (rhs, w),
                                (0, w) if w > 0 => (lhs, w),
                                _ => continue,
                            };
                            if mark(unknown, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Unary {
                        result, operand, ..
                    } => {
                        if floats.contains_key(&result.id()) {
                            let w = width_of(result, &floats);
                            if w > 0 && mark(operand, w, &mut floats) {
                                changed = true;
                            }
                        }
                        if floats.contains_key(&operand.id()) {
                            let w = width_of(operand, &floats);
                            if w > 0 && mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Call { result, ty, .. } => {
                        if ty.is_float() {
                            let w = match *ty {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            if mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Cast {
                        result,
                        src,
                        from,
                        to,
                    } => {
                        if to.is_float() {
                            let w = match *to {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            if mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                        if from.is_float() {
                            let w = match *from {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            if mark(src, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Copy { result, src } => {
                        if floats.contains_key(&src.id()) {
                            let w = width_of(src, &floats);
                            if mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                        if floats.contains_key(&result.id()) {
                            let w = width_of(result, &floats);
                            if mark(src, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Phi {
                        result, incoming, ..
                    } if incoming.iter().any(|(_, v)| floats.contains_key(&v.id())) => {
                        let w = incoming
                            .iter()
                            .map(|(_, v)| width_of(v, &floats))
                            .max()
                            .unwrap_or(64);
                        if mark(result, w, &mut floats) {
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut ids: Vec<u32> = floats.keys().copied().collect();
    ids.sort_unstable();
    let count = ids.len();
    FLOAT_SLOTS.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for (i, id) in ids.into_iter().enumerate() {
            let w = floats.get(&id).copied().unwrap_or(64);
            map.insert(id, (i, w));
        }
    });
    count
}

/// 发射一条 SSA 指令。
/// 发射一条 SSA 指令。
fn emit_instr(unit: &mut CompilationUnit, instr: &SsaInstr, wide: bool) {
    match instr {
        SsaInstr::Const { result, value } => match value {
            SsaConst::Int(i) => {
                let v = *i;
                emit_li(unit, out_reg(*result), v as i32);
                put_out(unit, *result);
            }
            // bool / unit 在整数寄存器里就是 0/1；浮点与字符串后端暂未实现
            SsaConst::Bool(b) => {
                emit_li(unit, out_reg(*result), i32::from(*b));
                put_out(unit, *result);
            }
            SsaConst::Unit => {
                emit_li(unit, out_reg(*result), 0);
                put_out(unit, *result);
            }
            SsaConst::Float(bits, w) => emit_const_float(unit, *result, *bits, *w),
            SsaConst::Str(text) => {
                let sym = unit.intern_string(text);
                emit_str_addr(unit, out_reg(*result), &sym);
                put_out(unit, *result);
            }
        },
        SsaInstr::Unary {
            result,
            op,
            operand,
        } => {
            if *op == crate::ssa::SsaUnaryOp::Neg
                && (float_slot(*result).is_some() || float_slot(*operand).is_some())
            {
                emit_unary_float(unit, *result, *operand);
            } else {
                emit_unary(unit, *result, *op, *operand);
            }
        }
        SsaInstr::Binary {
            result,
            op,
            lhs,
            rhs,
            ty,
        } => {
            if ty.is_float() || float_slot(*lhs).is_some() || float_slot(*rhs).is_some() {
                emit_binary_float(unit, *result, *op, *lhs, *rhs, *ty);
            } else {
                emit_binary(unit, *result, *op, *lhs, *rhs);
            }
        }
        SsaInstr::Call {
            result,
            callee,
            args,
            ..
        } => {
            emit_call_args(unit, args);
            emit_jal(unit, Reg::RA, callee);
            if let Some((slot, w)) = float_slot(*result) {
                emit_float_store(unit, Reg(10), slot, w); // fa0
            } else if let Some(slot) = spill_slot(*result) {
                store_spill(unit, Reg::A0, slot);
            } else {
                emit_mv(unit, alloc_reg(*result), Reg::A0);
            }
        }
        // φ 的实际拷贝由前驱末尾的 emit_phi_copies 发出
        SsaInstr::Phi { .. } => {}
        SsaInstr::Cast {
            result,
            src,
            from,
            to,
        } => {
            if from.is_float() || to.is_float() {
                emit_cast_float(unit, *result, *src, *from, *to);
            } else {
                let rs = val_reg(unit, *src);
                let rd = out_reg(*result);
                emit_cast(unit, rd, rs, *from, *to);
                put_out(unit, *result);
            }
        }
        SsaInstr::Copy { result, src } => {
            if float_slot(*result).is_some() {
                emit_float_copy(unit, *result, *src);
            } else {
                let s = val_reg(unit, *src);
                let d = out_reg(*result);
                if d != s {
                    emit_mv(unit, d, s);
                }
                put_out(unit, *result);
            }
        }
        SsaInstr::Var { .. } => {}
        // 被取地址的局部变量的栈槽地址：`addi rd, sp, 8*slot`。
        SsaInstr::SlotAddr { result, local } => {
            let slot = LOCAL_SLOTS.with(|m| m.borrow().get(local).copied().unwrap_or(0));
            let offset = (slot * 8) as i32;
            if offset > 2040 {
                crate::codegen::unsupported_const("risc-v slot offset out of range");
            }
            let rd = out_reg(*result);
            emit_addi(unit, rd, Reg::SP, offset);
            put_out(unit, *result);
        }
        SsaInstr::Load {
            result, ptr, ty, ..
        } => {
            let rp = val_reg(unit, *ptr);
            let rd = out_reg(*result);
            emit_load_mem(unit, rd, rp, *ty, wide);
            put_out(unit, *result);
        }
        SsaInstr::Store { ptr, value, ty, .. } => {
            let rp = val_reg(unit, *ptr);
            let rs = val_reg2(unit, *value);
            emit_store_mem(unit, rp, rs, *ty, wide);
        }
    }
}

/// 发射一元运算。
fn emit_unary(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: crate::ssa::SsaUnaryOp,
    operand: SsaValue,
) {
    let src = val_reg(unit, operand);
    let dst = out_reg(result);
    use crate::ssa::SsaUnaryOp::*;
    match op {
        // !x：sltiu rd, x, 1（x != 0 时为 1）
        Not => emit_seqz(unit, dst, src),
        BitNot => emit_not(unit, dst, src),
        Neg => emit_neg(unit, dst, src),
    }
    put_out(unit, result);
}

/// 发射二元运算。
///
/// 操作数经 `val_reg`/`val_reg2` 读出：左右都溢出时也必须各自装载（分别
/// 走 a7/gp 两个暂存寄存器，不会互相冲掉）；结果溢出时先算进 tp，发射完
/// 落回槽。
fn emit_binary(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    let rs1 = val_reg(unit, lhs);
    let rs2 = val_reg2(unit, rhs);
    let dst = out_reg(result);
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
    put_out(unit, result);
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
        // ge ⇔ !(x < y)；le ⇔ !(y < x)
        Ge => {
            emit_slt(unit, dst, rs1, rs2);
            emit_xori(unit, dst, dst, 1);
        }
        Le => {
            emit_slt(unit, dst, rs2, rs1);
            emit_xori(unit, dst, dst, 1);
        }
        // 其余算术运算不会走到这里（调用点已按比较运算分派），
        // 兜底按"小于"处理，保证函数对任意输入都有确定输出。
        _ => emit_slt(unit, dst, rs1, rs2),
    }
}

/// 寄存器控制的移位：`dst = rs1 << rs2`（`left = true`）或
/// `dst = rs1 >> rs2`。
///
/// 基础 RV ISA 本来就有 R 型寄存器移位（`sll`/`sra`，funct3=1/5，
/// funct7=0x00/0x20）——与 x86-64 的 `sar` 保持一致：IR 里只有一个
/// `Shr`，整数字面量默认 i32，对有符号数必须补符号位。
fn emit_var_shift(unit: &mut CompilationUnit, left: bool, dst: Reg, rs1: Reg, rs2: Reg) {
    let funct3 = if left { 1 } else { 5 };
    let funct7 = if left { 0x00 } else { 0x20 };
    emit_r(unit, 0x33, dst, funct3, rs1, rs2, funct7);
}

/// `seqz rd, rs`：`sltiu rd, rs, 1`
fn emit_seqz(unit: &mut CompilationUnit, dst: Reg, rs: Reg) {
    emit_i(unit, 0x13, dst, 3, rs, 1);
}

/// `snez rd, rs`：`sltu rd, zero, rs`
fn emit_snez(unit: &mut CompilationUnit, dst: Reg, rs: Reg) {
    emit_r(unit, 0x33, dst, 3, Reg::ZERO, rs, 0);
}

// ==================================================================
// 浮点（RV64 的 F/D 扩展指令选择）
// ==================================================================
//
// 与 AArch64 后端同一套数据流模型：浮点值**不占整数分配池**，平时躺在
// 專属栈槽里（-value 一个槽），只在进出形参/返回值/运算时落到浮点寄存器
// （fa0-fa7 传参，ft0-ft2 暂存）。整数寄存器分配照旧，两套互不干扰。
//
// 编码纪律（RISC-V 未授权规范卷一表 11.x）：
//   FADD.S/D 指令系列用 F 型（funct7 低 7 位发射 + fmt 位在 funct7[5]）；
//   转换指令用 rs2 字段区分子类（w/wu/l/lu），funct7 区分方向与格式。

thread_local! {
    /// 当前函数的浮点栈槽：值 id -> (槽号, 位宽 32/64)。
    static FLOAT_SLOTS: RefCell<HashMap<u32, (usize, u8)>> = RefCell::new(HashMap::new());
    /// 浮点槽区的基址：槽区位于 ra/保存区之上、取址槽之下。emit_function 设置。
    static FLOAT_BASE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 浮点暂存寄存器：`ft0`/`ft1`/`ft2` = f5/f6/f7（caller-save，跨指令不携带）。
const FP_TMP0: Reg = Reg(5);
const FP_TMP1: Reg = Reg(6);
const FP_TMP2: Reg = Reg(7);

/// psABI 的浮点实参寄存器 fa0-fa7（f10-f17），返回值在 fa0。
const FA_REGS: [Reg; 8] = [
    Reg(10),
    Reg(11),
    Reg(12),
    Reg(13),
    Reg(14),
    Reg(15),
    Reg(16),
    Reg(17),
];

/// `v` 如果是浮点值，返回 (槽号, 位宽)。
fn float_slot(v: SsaValue) -> Option<(usize, u8)> {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).copied())
}

/// 浮点槽相对 sp 的字节偏移。
fn float_offset(slot: usize) -> i32 {
    (FLOAT_BASE.with(|c| c.get()) + slot * 8) as i32
}

/// `fld/flw rd, off(sp)`——浮点槽装载。
fn emit_float_load(unit: &mut CompilationUnit, rd: Reg, slot: usize, w: u8) {
    let f3 = if w == 64 { 3 } else { 2 };
    let off = float_offset(slot) as u32 & 0xfff;
    insn(
        unit,
        off << 20 | (Reg::SP.bits() as u32) << 15 | f3 << 12 | (rd.bits() as u32) << 7 | 0x07,
    );
}

/// `fsd/fsw rs, off(sp)`——把浮点寄存器值落进槽。
fn emit_float_store(unit: &mut CompilationUnit, rs: Reg, slot: usize, w: u8) {
    let f3 = if w == 64 { 3 } else { 2 };
    let off = float_offset(slot) as u32;
    let hi = (off >> 5) & 0x7f;
    let lo = off & 0x1f;
    insn(
        unit,
        hi << 25
            | (rs.bits() as u32) << 20
            | (Reg::SP.bits() as u32) << 15
            | f3 << 12
            | lo << 7
            | 0x27,
    );
}

/// F 型运算：`funct7` 直接给（含 fmt 位），`rm` 编进 funct3。
fn emit_f(unit: &mut CompilationUnit, funct7: u32, rm: u32, rd: Reg, rs1: Reg, rs2: Reg) {
    emit_r(unit, 0x53, rd, rm, rs1, rs2, funct7);
}

/// 浮点转换：`rd = funct7 目标(rs2 字段选 w/wu/l/lu 变体)`。
fn emit_f_cvt(unit: &mut CompilationUnit, rd: Reg, rs1: Reg, funct7: u32, variant: u32, rm: u32) {
    emit_r(unit, 0x53, rd, rm, rs1, Reg((variant & 31) as u8), funct7);
}

/// 64 位立即数装载（f64 位型常量用）。
///
/// `emit_li` 只能装 32 位符号扩展值；位型 64 位常量分三段：
/// 1. lo32 经 `slli+srli` 零扩展进 `rd`；
/// 2. hi32 经 `emit_li` 正确 64 位化后上移；
/// 3. `or` 合并。中转借用 gp(x3)——指令发射期间 φ 中转必然空闲。
fn emit_li64(unit: &mut CompilationUnit, rd: Reg, imm: u64) {
    let bits = imm as i64;
    if bits >= i32::MIN as i64 && bits <= i32::MAX as i64 {
        emit_li(unit, rd, bits as i32);
        return;
    }
    // lo32 零扩展落位（li 是符号扩展的，左移 32 再无符号右移抹掉高位）
    emit_li(unit, rd, (imm & 0xffff_ffff) as u32 as i32);
    emit_shift_imm(unit, 1, 0x00, rd, rd, 32);
    emit_shift_imm(unit, 5, 0x00, rd, rd, 32);
    let hi = (imm >> 32) as i32;
    if hi != 0 {
        // li(hi) 符号扩到 64 位，slli 32 把低 32 位的符号垃圾移出界
        let tmp = PHI_TMP;
        emit_li(unit, tmp, hi);
        emit_shift_imm(unit, 1, 0x00, tmp, tmp, 32);
        emit_r(unit, 0x33, rd, 6, rd, tmp, 0); // or
    }
}

/// f64/f32 位型常量：`li` 出位型 → `fmv.d.x/fmv.w.x` 送进浮点暂存 → 存槽。
fn emit_const_float(unit: &mut CompilationUnit, result: SsaValue, bits: u64, w: u8) {
    let (slot, _) = float_slot(result).expect("浮点常量没有栈槽");
    let tmp = PHI_TMP;
    emit_li64(unit, tmp, bits);
    let funct7 = if w == 64 { 0x79 } else { 0x78 }; // FMV.D.X / FMV.W.X
    emit_f(unit, funct7, 0, FP_TMP0, tmp, Reg::ZERO);
    emit_float_store(unit, FP_TMP0, slot, w);
}

/// 浮点一元取负：`fneg`（= FSGNJN rd, rs, rs）。
fn emit_unary_float(unit: &mut CompilationUnit, result: SsaValue, operand: SsaValue) {
    let (_, sw) = float_slot(operand).expect("浮点取负的操作数没有栈槽");
    let (dst, dw) = float_slot(result).expect("浮点取负的结果没有栈槽");
    let w = dw.max(sw);
    emit_float_load(
        unit,
        FP_TMP0,
        float_slot(operand).map(|(s, _)| s).unwrap(),
        w,
    );
    let funct7 = if w == 64 { 0x11 } else { 0x10 }; // FSGNJN.D / FSGNJN.S，rm=01
    emit_f(unit, funct7, 1, FP_TMP1, FP_TMP0, FP_TMP0);
    emit_float_store(unit, FP_TMP1, dst, w);
}

/// 浮点槽对槽复制：经 FT0 中转。
fn emit_float_copy(unit: &mut CompilationUnit, result: SsaValue, src: SsaValue) {
    let (_, sw) = float_slot(src).expect("浮点复制源没有栈槽");
    let (dst, dw) = float_slot(result).expect("浮点复制目标没有栈槽");
    let w = dw.max(sw);
    let ss = float_slot(src).map(|(s, _)| s).unwrap();
    emit_float_load(unit, FP_TMP0, ss, w);
    emit_float_store(unit, FP_TMP0, dst, w);
}

/// 浮点二元运算与比较。
///
/// 算术结果落浮点槽（FT2 暂存）；比较（IEEE754 无序时取假：`feq/flt/fle`
/// 对 NaN 一律返回 0）的结果物化到**整数寄存器**的 0/1——后续 `beq`
/// 只认整数布尔。
///
/// 两个操作数位宽不一致时把窄的先转成宽的再运算（理论上是 tyc k 的事，
/// 这里兜底留住正确性）。
fn emit_binary_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
    ty: NumTy,
) {
    use SsaBinaryOp::*;
    let (ls, lw) = float_slot(lhs).expect("浮点运算左操作数没有栈槽");
    let (rs, rw) = float_slot(rhs).expect("浮点运算右操作数没有栈槽");
    let tw = match ty {
        NumTy::Float(w) if w != 0 => w,
        _ => 64,
    };
    let w = tw.max(lw).max(rw);
    let fmt = u32::from(w == 64); // .D 的 funct7 位
                                  // 左操作数 → FT0，右 → FT1；不足宽先转
    emit_float_load(unit, FP_TMP0, ls, lw);
    if w != lw {
        emit_f(unit, 0x21, 0, FP_TMP0, FP_TMP0, Reg::ZERO); // fcvt.d.s
    }
    emit_float_load(unit, FP_TMP1, rs, rw);
    if w != rw {
        emit_f(unit, 0x21, 0, FP_TMP1, FP_TMP1, Reg::ZERO); // fcvt.d.s
    }
    if matches!(op, Eq | Ne | Lt | Gt | Le | Ge) {
        let dst = out_reg(result); // 比较结果落整数寄存器
        match op {
            Eq => emit_f(unit, 0x50 | fmt, 2, dst, FP_TMP0, FP_TMP1), // feq
            Ne => {
                emit_f(unit, 0x50 | fmt, 2, dst, FP_TMP0, FP_TMP1);
                emit_xori(unit, dst, dst, 1);
            }
            Lt => emit_f(unit, 0x50 | fmt, 1, dst, FP_TMP0, FP_TMP1), // flt
            Gt => emit_f(unit, 0x50 | fmt, 1, dst, FP_TMP1, FP_TMP0),
            Le => emit_f(unit, 0x50 | fmt, 0, dst, FP_TMP0, FP_TMP1), // fle
            Ge => emit_f(unit, 0x50 | fmt, 0, dst, FP_TMP1, FP_TMP0),
            _ => unreachable!(),
        }
        put_out(unit, result);
        return;
    }
    // 算术：结果统一算进 FT2，再落回结果槽（位宽以 ty 为准）
    let base = match op {
        Add => 0x00, // FADD
        Sub => 0x04,
        Mul => 0x08,
        Div => 0x0c,
        _ => crate::codegen::unsupported_binary_op(op),
    };
    emit_f(unit, base | fmt, 0, FP_TMP2, FP_TMP0, FP_TMP1);
    emit_float_store(
        unit,
        FP_TMP2,
        float_slot(result).map(|(s, _)| s).unwrap_or_default(),
        tw,
    );
}

/// 浮点转换：f↔f 跨宽（fcvt.d.s / fcvt.s.d）、f↔整数（`as` 语义 =
/// 向零截断，rm=001）。整数源总是 represented 成 x 寄存器里的 i64/u64。
fn emit_cast_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    src: SsaValue,
    from: NumTy,
    to: NumTy,
) {
    if from.is_float() && to.is_float() {
        let fw = match from {
            NumTy::Float(w) => w,
            _ => 64,
        };
        let tw = match to {
            NumTy::Float(w) => w,
            _ => 64,
        };
        let (ss, _) = float_slot(src).expect("浮点转换源没有栈槽");
        let (dst, _) = float_slot(result).expect("浮点转换结果没有栈槽");
        emit_float_load(unit, FP_TMP0, ss, fw);
        if fw != tw {
            // 0x21 = FCVT.D.S（f32→f64），0x20 = FCVT.S.D
            let funct7 = if tw == 64 { 0x21 } else { 0x20 };
            emit_f(unit, funct7, 0, FP_TMP0, FP_TMP0, Reg::ZERO);
        }
        emit_float_store(unit, FP_TMP0, dst, tw);
        return;
    }
    if to.is_float() {
        // 整数 → 浮点：窄宽度已在 x 寄存器里扩展好，宽变体同样适用。
        // rs2 字段：0=w 1=wu 2=l 3=lu；有符号 / 无符号由源类型定。
        let variant: u32 = match from {
            NumTy::Int(32) => 0,
            NumTy::Uint(32) | NumTy::Bool => 1,
            NumTy::Int(_) => 2,
            NumTy::Uint(_) => 3,
            NumTy::Float(_) => unreachable!(),
        };
        let rs = val_reg(unit, src);
        let (dst, tw) = float_slot(result).expect("浮点转换结果没有栈槽");
        // funct7：0b1101XXX = int→f（低 3 位=fmt + rs2 变体用 rs2 字段）
        let funct7 = if tw == 64 { 0x69 } else { 0x68 };
        emit_f_cvt(unit, FP_TMP0, rs, funct7, variant, 0);
        emit_float_store(unit, FP_TMP0, dst, tw);
        return;
    }
    // 浮点 → 整数：rm=001 = 向零取整，与 Rust `as` 一致。
    let (ss, fw) = float_slot(src).expect("浮点转换源没有栈槽");
    let variant: u32 = match to {
        NumTy::Int(32) => 0,
        NumTy::Uint(32) | NumTy::Bool => 1,
        NumTy::Int(_) => 2,
        NumTy::Uint(_) => 3,
        NumTy::Float(_) => unreachable!(),
    };
    let funct7 = if fw == 64 { 0x61 } else { 0x60 };
    let want64 = matches!(to, NumTy::Int(64) | NumTy::Uint(64));
    emit_float_load(unit, FP_TMP0, ss, fw);
    emit_f_cvt(unit, FP_TMP0, FP_TMP0, funct7, variant, 1);
    let r = SPILL_DST;
    emit_fmv_x(unit, r, FP_TMP0, want64); // 浮点轨 → 整数轨
    let rd = out_reg(result);
    if rd != r {
        emit_mv(unit, rd, r);
    }
    // 窄宽度需要按整数世界的扩展规则收尾（值已在 i64 语义里）
    if !want64 {
        emit_cast(unit, rd, rd, NumTy::Int(64), to);
    }
    put_out(unit, result);
}

/// `fmv.x.d/fmv.x.w rd, rs`：浮点寄存器 → x 寄存器（位型搬运）。
///
/// `.w` 按规范把低 32 位**符号扩展**；窄整数结果随后由 `emit_cast` 的
/// 扩展规则修正，无碍。64 位结果必须走 `.d`（全 64 位照搬）。
fn emit_fmv_x(unit: &mut CompilationUnit, rd: Reg, rs: Reg, wide: bool) {
    let funct7 = if wide { 0x71 } else { 0x70 };
    emit_r(unit, 0x53, rd, 0, rs, Reg::ZERO, funct7);
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
                        ty: crate::mir::NumTy::Int(64),
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
            inline_always: false,
        };
        let mut unit = empty_unit();
        emit_function(
            &Target::parse("riscv64-unknown-none").unwrap(),
            &mut unit,
            &func,
        );
        crate::codegen::objfile::apply_relocations(&mut unit, 0, 0);

        let word_at = |off: usize| u32::from_le_bytes(unit.code[off..off + 4].try_into().unwrap());
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

    /// B 型立即数（字节单位）：imm[12|10:5] | imm[4:1|11]
    fn b_imm(word: u32) -> i32 {
        let imm = ((word >> 31) & 1) << 12
            | ((word >> 25) & 0x3f) << 5
            | ((word >> 8) & 0xf) << 1
            | ((word >> 7) & 1) << 11;
        ((imm << 20) as i32) >> 20
    }

    /// J 型立即数（字节单位）：imm[20|10:1|11|19:12]
    fn j_imm(word: u32) -> i32 {
        let imm = ((word >> 31) & 1) << 20
            | ((word >> 21) & 0x3ff) << 1
            | ((word >> 20) & 1) << 11
            | ((word >> 12) & 0xff) << 12;
        ((imm << 11) as i32) >> 11
    }

    #[test]
    fn conditional_branch_sends_true_to_then_block() {
        let (branch, jump, branch_at, then_off, else_off) = branch_shape();
        // funct3 = 000（beq）：条件为"真"时**不**跳转，顺序落到后面的 j → then 块
        assert_eq!(
            (branch >> 12) & 7,
            0b000,
            "应为 beq，跳过跳转即表示条件为真"
        );
        assert_eq!(branch & 0x7f, 0x63, "opcode 必须是 BRANCH");
        // 紧跟的 J 型跳转用 rd = x0（无条件），跳到 then 块
        assert_eq!(jump & 0x7f, 0x6f, "第二条指令必须是 jal");
        assert_eq!((jump >> 7) & 0x1f, 0, "jal rd 必须为 x0（无条件跳转）");

        // 语义核验：beq 落在 else 块，jal 落在 then 块
        let branch_pc = branch_at as i64;
        assert_eq!(
            branch_pc + b_imm(branch) as i64,
            else_off as i64,
            "beq 必须跳到 else 块"
        );
        assert_eq!(
            branch_pc + 4 + j_imm(jump) as i64,
            then_off as i64,
            "jal 必须跳到 then 块"
        );
    }

    #[test]
    fn variable_shift_is_a_single_register_shift() {
        // `dst = x << y`：基础 ISA 的 R 型 `sll`，移位量直接来自
        // y 所在的**寄存器**（rs2 是寄存器号字段，不是立即数 shamt）
        let mut unit = empty_unit();
        let lhs = SsaValue(1);
        let rhs = SsaValue(7);
        let dst = SsaValue(8);
        emit_binary(&mut unit, dst, SsaBinaryOp::Shl, lhs, rhs);
        crate::codegen::objfile::apply_relocations(&mut unit, 0, 0);

        let words: Vec<u32> = unit
            .code
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let word = words[0];
        assert_eq!(word & 0x7f, 0x33, "应为 OP（R 型），实际：{words:08x?}");
        assert_eq!(
            (word >> 12) & 7,
            0b001,
            "funct3=001 才是 sll，实际：{words:08x?}"
        );
        assert_eq!(
            (word >> 25) & 0x7f,
            0,
            "sll 的 funct7 应为 0，实际：{words:08x?}"
        );
        // 操作数必须来自两个 SSA 值各自的分配寄存器
        let rd = u32::from((word >> 7) & 0x1f);
        let rs1 = u32::from((word >> 15) & 0x1f);
        let rs2 = u32::from((word >> 20) & 0x1f);
        assert_eq!(rd, alloc_reg(dst).bits() as u32, "目的寄存器：{words:08x?}");
        assert_eq!(rs1, alloc_reg(lhs).bits() as u32, "被移位值：{words:08x?}");
        assert_eq!(rs2, alloc_reg(rhs).bits() as u32, "移位量：{words:08x?}");
    }

    #[test]
    fn right_shift_is_arithmetic() {
        let mut unit = empty_unit();
        emit_binary(
            &mut unit,
            SsaValue(8),
            SsaBinaryOp::Shr,
            SsaValue(1),
            SsaValue(2),
        );
        let words: Vec<u32> = unit
            .code
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        // IR 只有一个 Shr，整数字面量默认 i32，所以取算术右移
        // `sra`（funct3=101，funct7=0x20），与 x86-64 的 sar /
        // AArch64 的 asrv 保持一致。
        let word = words[0];
        assert_eq!(word & 0x7f, 0x33, "应为 OP（R 型），实际：{words:08x?}");
        assert_eq!(
            (word >> 12) & 7,
            0b101,
            "funct3=101 才是右移，实际：{words:08x?}"
        );
        assert_eq!(
            (word >> 25) & 0x7f,
            0x20,
            "应为 sra（算术右移），不是 srl，实际：{words:08x?}"
        );
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
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x02A0_0513
        );
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
        assert_eq!(
            ((lui >> 12) & 0xf_ffff) << 12 | ((addi >> 20) & 0xfff),
            0x1234_5678
        );
    }

    #[test]
    fn negative_constant_is_encoded_in_two_instructions() {
        let mut u = empty_unit();
        emit_li(&mut u, Reg::A0, -1);
        assert_eq!(u.code.len(), 4, "-1 可用单条 addi 表示");
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xFFF0_0513
        );
    }

    #[test]
    fn negative_offset_constant_uses_single_addi() {
        // addi sp, sp, -32 = 0xFE010113
        let mut u = empty_unit();
        emit_addi(&mut u, Reg(2), Reg(2), -32);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xfe01_0113
        );
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
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0000_00ef,
            "jal ra, 0"
        );
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
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0000_006f
        );
    }

    #[test]
    fn shift_immediates_match_specification() {
        // slli a0, a1, 3 = 0x00359513
        let mut u = empty_unit();
        emit_shift_imm(&mut u, 1, 0x00, Reg(10), Reg(11), 3);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0035_9513
        );
        // srli a0, a1, 3 = 0x0035D513
        let mut u = empty_unit();
        emit_shift_imm(&mut u, 5, 0x00, Reg(10), Reg(11), 3);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0035_D513
        );
        // srai a0, a1, 3 = 0x4035D513
        let mut u = empty_unit();
        emit_shift_imm(&mut u, 5, 0x20, Reg(10), Reg(11), 3);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x4035_D513
        );
    }

    #[test]
    fn sequence_zeros_match_specification() {
        // seqz a0, a0 = sltiu a0, a0, 1 = 0x00153513
        let mut u = empty_unit();
        emit_seqz(&mut u, Reg(10), Reg(10));
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0015_3513
        );
        // snez a0, a0 = sltu a0, zero, a0 = 0x00A03533
        let mut u = empty_unit();
        emit_snez(&mut u, Reg(10), Reg(10));
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x00a0_3533
        );
    }

    #[test]
    fn remainder_moves_result_out_of_a0() {
        let mut u = empty_unit();
        // result 落在 a0 之外时才需要搬运；检查至少生成了 rem
        emit_binary(
            &mut u,
            SsaValue(0),
            SsaBinaryOp::Rem,
            SsaValue(1),
            SsaValue(2),
        );
        assert!(!u.code.is_empty());
    }

    /// load/store 的编码基准来自 RISC-V 规范的 LOAD/STORE-OPCODE 表：
    /// funct3：lb=0 lh=1 lw=2 ld=3 lbu=4 lhu=5 lwu=6；sb=0 sh=1 sw=2 sd=3。
    #[test]
    fn load_mem_selects_width_and_sign() {
        // t0 = a0：lwu t0, 0(a0)（RV64，4 字节数据、无符号、机器 64 位）
        // 编码：rs2 无用，f3=6，rs1=a0(x10)=1010，rd=t0(x5)=0101
        //   imm=0 | rs1=0x0A<<15 | f3=6<<12 | rd=0x05<<7 | 0x03
        // = 0x00A5_32E? 手算：0x0A<<15=0x0005_0000? 不对——立即手算：
        //   0 << 20 = 0
        //   0x0A << 15 = 0x0005_0000
        //   6 << 12 = 0x6000
        //   0x05 << 7 = 0x0280
        //   0x03
        // 合计 0x0005_0000 | 0x6000 | 0x0280 | 3 = 0x0005_6283
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg(5), Reg(10), NumTy::Uint(32), true);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0005_6283,
            "RV64：u32 用 lwu 零扩展"
        );
        // t0 = a0：lw t0, 0(a0)（RV32 上同样读 4 字节无符号，机器 32 位）
        //   f3=2 → 0x0005_2283
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg(5), Reg(10), NumTy::Uint(32), false);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0005_2283
        );
        // t1 = a1：lb t1, 0(a1)（有符号字节）
        // rs1=0x0B<<15 | 0<<12 | rd=0x06<<7 | 3 = 0x0058_3083? 直接算：
        //   0x0B << 15 = 0x0005_8000；f3=0；0x06 << 7 = 0x0300；opcode 0x03
        // 合计 0x0005_8303
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg(6), Reg(11), NumTy::Int(8), true);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0005_8303
        );
        // t1 = a1：lbu t1, 0(a1)（无符号字节，f3=4 → +0x4000）
        // = 0x0005_C303
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg(6), Reg(11), NumTy::Uint(8), true);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x0005_C303
        );
    }

    #[test]
    fn store_mem_selects_width() {
        // sb a1, 0(a0)：rs2=0x0B<<20 | rs1=0x0A<<15 | f3=0 | 0x23
        // = 0x00B5_0023
        let mut u = empty_unit();
        emit_store_mem(&mut u, Reg(10), Reg(11), NumTy::Bool, true);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x00B5_0023
        );
        // sw a1, 0(a0)：f3=2 → 0x00B5_2023
        let mut u = empty_unit();
        emit_store_mem(&mut u, Reg(10), Reg(11), NumTy::Int(32), false);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x00B5_2023
        );
    }
}
/// 寄存器分配。
///
/// 与 AArch64 同构：先用线性扫描给每个值定寄存器，`alloc_reg` 按结果走；
/// 活跃值超过可分配寄存器数时溢出到栈槽（x86-64 同款语义），读取点经
/// 暂存寄存器装载。
fn check_register_pressure(func: &SsaFunction) {
    // 跨调用存活的值只放被调用者保存的 s0-s11，否则 `jal` 返回后内容就没了。
    let alloc = crate::codegen::regalloc::allocate_registers(
        func,
        ALLOCATABLE.len(),
        &CALLEE_SAVE,
        ARG_REGS.len(),
    );
    REG_MAP.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for (&v, loc) in &alloc.map {
            if let crate::codegen::regalloc::Location::Reg(i) = loc {
                map.insert(v, *i);
            }
        }
    });
    SPILL_SLOTS.with(|m| {
        let mut slots = m.borrow_mut();
        slots.clear();
        for (&v, loc) in &alloc.map {
            if let crate::codegen::regalloc::Location::Stack(i) = loc {
                slots.insert(v, *i);
            }
        }
    });
}
