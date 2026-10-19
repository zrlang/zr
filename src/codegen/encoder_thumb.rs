//==============================================================================
// encoder_thumb.rs — ARM Thumb-2（thumbv7em）后端
//
// 与 `encoder_arm.rs`（A32）生成同一套 SSA IR，区别只在指令编码。
//
// 为什么单独一个后端而不是"给 A32 加个开关"：Thumb-2 和 A32 是两套完全
// 不同的编码规则——16 位指令寄存器受限、部分算术要用 `movw`/`movt` 拼
// 32 位常量、条件执行从 A32 的条件后缀变成了 **IT 块**。混在一个文件里
// 会让两套编码互相干扰。
//
// 编码的来源与验证：每条指令的参考半字都由 `clang --target=thumbv7em-none-eabi`
// 汇编同一句汇编得到（见 `tests::encodings_match_clang_reference`）。
// IT 块 clang 的内置汇编器不接受，其编码按 ARM ARM 的固定格式给出
// （`1011 1111 cond(4) mask(4)`，单条指令时 mask = 0b1000）。
//
// 宽度说明：zr 的 SSA 把整数统一建模为 i64，32 位目标上按机器字长（32 位）
// 逐条发射，超出 32 位的值走"双 32 位 + 栈槽"（见 `WIDE_VALUES`）；浮点值
// 不占整数寄存器池，每个 SSA 浮点值一个 8 字节栈槽，用 VFP `s0/s1`、`d0/d1`
// 当暂存。
//==============================================================================

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};
use crate::mir::NumTy;
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaUnaryOp, SsaValue};

/// 通用寄存器 r0–r15。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(pub u32);

impl Reg {
    /// 位域值（ARM 的寄存器字段统一 4 位）。
    fn bits(self) -> u16 {
        (self.0 & 0xf) as u16
    }
}

/// 可分配寄存器。池首与 AAPCS 的实参寄存器 r0-r3 对齐（前 4 个形参会钉在
/// 这里），后 4 个是被调用者保存的 r4-r7（跨调用存活的值只能落在这里）。
const ALLOCATABLE: [Reg; 8] = [
    Reg(0),
    Reg(1),
    Reg(2),
    Reg(3),
    Reg(4),
    Reg(5),
    Reg(6),
    Reg(7),
];

/// AAPCS32 的整数实参寄存器 r0-r3。
const ARG_REGS: [Reg; 4] = [Reg(0), Reg(1), Reg(2), Reg(3)];

/// 被调用者保存的寄存器——分配池下标（池首 4 个是 r0-r3，故 4..8 对应 r4-r7）。
const CALLEE_SAVE: [usize; 4] = [4, 5, 6, 7];

/// 溢出 / 宽整数运算的暂存寄存器（都在分配池之外）。
const SPILL_SRC: Reg = Reg(9);
const SPILL_SRC2: Reg = Reg(10);
const MUL_TMP: Reg = Reg(11);
const SPILL_DST: Reg = Reg(12);
const PHI_TMP: Reg = Reg(9);

thread_local! {
    /// 当前函数的整数寄存器分配：值 id -> 物理寄存器。
    static REG_MAP: RefCell<HashMap<u32, Reg>> = RefCell::new(HashMap::new());
    /// 溢出值 / 宽整数值的栈槽：值 id -> 槽号（0 起）。
    static SPILL_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
    /// 溢出槽区的 sp 偏移（取址槽区基址要抬升它）。
    static SPILL_BASE: Cell<usize> = const { Cell::new(0) };
    /// 当前函数的多字长（i64）值 id 集合。
    static WIDE_VALUES: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    /// 浮点值栈槽：值 id -> (槽号, 位宽 32/64)。
    static FLOAT_SLOTS: RefCell<HashMap<u32, (usize, u8)>> = RefCell::new(HashMap::new());
    /// 被取地址的局部变量 -> 栈槽编号。
    static LOCAL_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
    /// 当前函数里的整数常量：值 id -> 常量值（用于 i64 除/取余的常量强度削减）。
    static CONSTS: RefCell<HashMap<u32, i128>> = RefCell::new(HashMap::new());
    /// 函数内局部标签：所属函数名前缀 + 计数器。
    ///
    /// Thumb 没有 A32 的条件执行后缀，宽移位要按 `cnt < 32` 分两段，只能靠
    /// 条件跳转绕开。编码器原先只有块级标签（每块一个 `{func}.bN`），这里
    /// 补一个函数内任意位置的标签设施。名字带**函数名前缀**、计数器在
    /// `emit_function` 开头复位：既保证同一份产物内唯一（`symbol_addresses`
    /// 按名字建表），又不受并行代码生成时的调度顺序影响（确定性输出）。
    static LOCAL_LABEL_PREFIX: RefCell<String> = const { RefCell::new(String::new()) };
    static LOCAL_LABEL: Cell<u32> = const { Cell::new(0) };
}

/// 造一个函数内唯一的标签，并把它的符号放在当前代码位置。
fn define_label(unit: &mut CompilationUnit, name: &str) {
    unit.symbols.push(Symbol {
        name: name.to_string(),
        section: SymbolSection::Text,
        offset: unit.code.len(),
        size: 0,
        global: false,
        kind: SymbolKind::Section,
    });
}

/// 申请一个函数内唯一的局部标签名（尚未落点）。
fn fresh_label() -> String {
    let prefix = LOCAL_LABEL_PREFIX.with(|p| p.borrow().clone());
    let n = LOCAL_LABEL.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    format!("{prefix}.L{n}")
}

fn alloc_reg(v: SsaValue) -> Reg {
    if let Some(r) = REG_MAP.with(|m| m.borrow().get(&v.id()).copied()) {
        return r;
    }
    ALLOCATABLE[(v.id() as usize) % ALLOCATABLE.len()]
}

fn spill_slot(v: SsaValue) -> Option<usize> {
    SPILL_SLOTS.with(|m| m.borrow().get(&v.id()).copied())
}

fn is_wide(v: SsaValue) -> bool {
    WIDE_VALUES.with(|w| w.borrow().contains(&v.id()))
}

fn float_width(v: SsaValue) -> u8 {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).map(|(_, w)| *w).unwrap_or(0))
}

fn float_slot(v: SsaValue) -> Option<usize> {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).map(|(s, _)| *s))
}

// ==================================================================
// 编码原语
// ==================================================================

/// 写一个 16 位半字。
fn hw(unit: &mut CompilationUnit, v: u16) {
    unit.code.extend_from_slice(&v.to_le_bytes());
}

/// 写一个 32 位 Thumb-2 指令（两个半字，低位在前）。
fn w32(unit: &mut CompilationUnit, lo: u16, hi: u16) {
    hw(unit, lo);
    hw(unit, hi);
}

/// 写一个 32 位字（VFP 协处理器指令与 A32 同编码，按小端落两个半字）。
fn word(unit: &mut CompilationUnit, w: u32) {
    // Thumb-2 的 32 位指令在内存里**高半字在前**。
    hw(unit, (w >> 16) as u16);
    hw(unit, w as u16);
}

/// `movw rd, #imm16`（T1）。
fn movw(unit: &mut CompilationUnit, rd: Reg, imm: u16) {
    let imm4 = ((imm >> 12) & 0xf) as u16;
    let i = ((imm >> 11) & 1) as u16;
    let imm3 = ((imm >> 8) & 0x7) as u16;
    let imm8 = imm & 0xffu16;
    w32(
        unit,
        0xf240 | (i << 10) | imm4,
        (imm3 << 12) | (rd.bits() << 8) | imm8,
    );
}

/// `movt rd, #imm16`（T1）。与 `movw` 只差 hw1 的一个常量位。
fn movt(unit: &mut CompilationUnit, rd: Reg, imm: u16) {
    let imm4 = ((imm >> 12) & 0xf) as u16;
    let i = ((imm >> 11) & 1) as u16;
    let imm3 = ((imm >> 8) & 0x7) as u16;
    let imm8 = imm & 0xffu16;
    w32(
        unit,
        0xf2c0 | (i << 10) | imm4,
        (imm3 << 12) | (rd.bits() << 8) | imm8,
    );
}

/// 32 位常量装载：`movw` + `movt`。
fn mov_imm32(unit: &mut CompilationUnit, rd: Reg, imm: u32) {
    movw(unit, rd, (imm & 0xffff) as u16);
    let hi = (imm >> 16) as u16;
    if hi != 0 {
        movt(unit, rd, hi);
    }
}

/// 把 `.rodata` 符号的**绝对地址**装进 `rd`：`movw` + `movt`，各附一条
/// 重定位（下半 / 上半）。目标文件交给链接器；自构造映像由
/// `apply_relocations` 回填。
fn emit_str_addr(unit: &mut CompilationUnit, rd: Reg, sym: &str) {
    let start = unit.code.len();
    movw(unit, rd, 0);
    unit.relocations.push(Relocation::new(
        start,
        start,
        4,
        sym,
        RelocationKind::ThumbMovwAbs,
    ));
    let hi_start = unit.code.len();
    movt(unit, rd, 0);
    unit.relocations.push(Relocation::new(
        hi_start,
        hi_start,
        4,
        sym,
        RelocationKind::ThumbMovtAbs,
    ));
}

/// 数据处理（3 个寄存器）T2。
fn dp3(unit: &mut CompilationUnit, prefix: u16, rd: Reg, rn: Reg, rm: Reg) {
    w32(unit, prefix | rn.bits(), (rd.bits() << 8) | rm.bits());
}

/// 各三寄存器逻辑/算术运算的 hw1 前缀。
fn dp_prefix(op: SsaBinaryOp) -> u16 {
    use SsaBinaryOp::*;
    match op {
        Add => 0xeb00,
        Sub => 0xeba0,
        BitAnd | And => 0xea00,
        BitOr | Or => 0xea40,
        BitXor => 0xea80,
        _ => unreachable!(),
    }
}

/// 三寄存器乘除（MUL/SDIV/UDIV）。
fn muldiv(unit: &mut CompilationUnit, op: u16, rd: Reg, rn: Reg, rm: Reg) {
    let hw1 = 0xfb00 | (op << 4) | rn.bits();
    let nib = if op == 0 { 0 } else { 0xf };
    let hw2 = 0xf000 | (nib << 4) | (rd.bits() << 8) | rm.bits();
    w32(unit, hw1, hw2);
}

/// `mls rd, rn, rm, ra` → `rd = ra - rn * rm`。
fn mls(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
    let hw1 = 0xfb00 | rn.bits();
    let hw2 = (ra.bits() << 12) | (rd.bits() << 8) | 0x10 | rm.bits();
    w32(unit, hw1, hw2);
}

/// 移位（寄存器控制）T2。
fn shift_reg(unit: &mut CompilationUnit, op: u16, rd: Reg, rn: Reg, rm: Reg) {
    let hw1 = 0xfa00 | (op << 5) | rn.bits();
    let hw2 = 0xf000 | (rd.bits() << 8) | rm.bits();
    w32(unit, hw1, hw2);
}

/// 一元（`mvn` 等）T2：hw2 = Rd | Rm。
fn un_op(unit: &mut CompilationUnit, hw1: u16, rd: Reg, rm: Reg) {
    w32(unit, hw1, (rd.bits() << 8) | rm.bits());
}

/// 扩展（`sxtb`/`sxth`/`uxtb`/`uxth`）16 位。只编码 Rd，源被隐含为 Rd。
fn extend16(unit: &mut CompilationUnit, kind: u16, rd: Reg, rm: Reg) {
    let field: u16 = match kind {
        0b00 => 0b01, // sxtb
        0b01 => 0b00, // sxth
        0b10 => 0b11, // uxtb
        0b11 => 0b10, // uxth
        _ => unreachable!(),
    };
    if rd != rm {
        mov_low(unit, rd, rm);
    }
    hw(unit, 0xb200 | (field << 6) | 0x10 | rd.bits());
}

/// 扩展（`sxtb.w`/`sxth.w`/`uxtb.w`/`uxth.w`）32 位，寄存器不限。
/// kind: 0b00=sxtb 0b01=sxth 0b10=uxtb 0b11=uxth。
fn extend_any(unit: &mut CompilationUnit, kind: u16, rd: Reg, rm: Reg) {
    let hw1 = match kind {
        0b00 => 0xfa4f,
        0b01 => 0xfa0f,
        0b10 => 0xfa5f,
        0b11 => 0xfa1f,
        _ => unreachable!(),
    };
    w32(unit, hw1, 0xf080 | (rd.bits() << 8) | rm.bits());
}

/// `mov rd, rm`（16 位，仅低 8 个寄存器）。
fn mov_low(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    debug_assert!(rd.0 < 8, "mov_low 的目标只能是低 8 个寄存器");
    hw(unit, 0x4600 | ((rm.bits() & 7) << 3) | (rd.bits() & 7));
}

/// `cmp rn, #imm`，imm 需能放进 8 位。
fn cmp_imm8(unit: &mut CompilationUnit, rn: Reg, imm: u8) {
    if rn.0 < 8 {
        hw(unit, 0x2800 | (rn.bits() << 8) | imm as u16);
    } else {
        w32(unit, 0xf1b0 | rn.bits(), 0x0f00 | imm as u16);
    }
}

/// `sub.w rd, rn, #imm8`（32 位 T2，`imm8` 需能放进 8 位立即数）。
fn sub_imm8(unit: &mut CompilationUnit, rd: Reg, rn: Reg, imm: u8) {
    w32(unit, 0xf1a0 | rn.bits(), (rd.bits() << 8) | imm as u16);
}

/// `rsb.w rd, rn, #imm8`（32 位，`rd = imm - rn`）。
fn rsb_imm8(unit: &mut CompilationUnit, rd: Reg, rn: Reg, imm: u8) {
    w32(unit, 0xf1c0 | rn.bits(), (rd.bits() << 8) | imm as u16);
}

/// `mov.w rd, rm`（32 位 T3），寄存器范围不限。
fn mov_wide(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    w32(unit, 0xea4f, (rd.bits() << 8) | rm.bits());
}

/// `mov rd, #imm`（16 位，仅 0–255、rd<8）。
fn mov_imm8(unit: &mut CompilationUnit, rd: Reg, imm: u8) {
    hw(unit, 0x2000 | ((rd.bits() & 7) << 8) | imm as u16);
}

/// `mov.w rd, #imm8`（32 位，寄存器不限）。
fn mov_imm8_wide(unit: &mut CompilationUnit, rd: Reg, imm: u8) {
    w32(unit, 0xf04f, (rd.bits() << 8) | imm as u16);
}

/// `mov rd, #imm8`：按寄存器自动选 16/32 位形式。
fn mov_reg_imm8(unit: &mut CompilationUnit, rd: Reg, imm: u8) {
    if rd.0 < 8 {
        mov_imm8(unit, rd, imm);
    } else {
        mov_imm8_wide(unit, rd, imm);
    }
}

/// `cmp rn, rm`（16 位，仅低 8 个寄存器）。
fn cmp_low(unit: &mut CompilationUnit, rn: Reg, rm: Reg) {
    hw(unit, 0x4280 | ((rm.bits() & 7) << 3) | (rn.bits() & 7));
}

/// `cmp.w rn, rm`（32 位 T2）。
fn cmp_wide(unit: &mut CompilationUnit, rn: Reg, rm: Reg) {
    w32(unit, 0xebb0 | rn.bits(), 0x0f00 | rm.bits());
}

/// 寄存器不限的 `cmp`。
fn cmp_any(unit: &mut CompilationUnit, rn: Reg, rm: Reg) {
    if rn.0 < 8 && rm.0 < 8 {
        cmp_low(unit, rn, rm);
    } else {
        cmp_wide(unit, rn, rm);
    }
}

// 带进位/借位的加减（用于 i64 多字长）。
fn adds(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    w32(unit, 0xeb10 | rn.bits(), (rd.bits() << 8) | rm.bits());
}
fn adcs(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    w32(unit, 0xeb50 | rn.bits(), (rd.bits() << 8) | rm.bits());
}
fn subs(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    w32(unit, 0xebb0 | rn.bits(), (rd.bits() << 8) | rm.bits());
}
fn sbcs(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    w32(unit, 0xeb70 | rn.bits(), (rd.bits() << 8) | rm.bits());
}

/// `orr.w rd, rn, rm`（32 位 T2，不置标志）。
fn orr(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    dp3(unit, 0xea40, rd, rn, rm);
}

/// `umull rdlo, rdhi, rn, rm`（无符号 32x32->64）。
fn umull(unit: &mut CompilationUnit, rdlo: Reg, rdhi: Reg, rn: Reg, rm: Reg) {
    w32(
        unit,
        0xfba0 | rn.bits(),
        (rdlo.bits() << 12) | (rdhi.bits() << 8) | rm.bits(),
    );
}

/// IT 块：让紧跟其后的一条指令按条件执行。
fn it(unit: &mut CompilationUnit, cond: u16) {
    hw(unit, 0xbf00 | (cond << 4) | 0b1000);
}

/// 比较运算对应的 ARM 条件码。
fn cond_of(op: SsaBinaryOp) -> u16 {
    use SsaBinaryOp::*;
    match op {
        Eq => 0,  // Z
        Ne => 1,  // NZ
        Lt => 11, // LT
        Ge => 10, // GE
        Gt => 12, // GT
        Le => 13, // LE
        _ => 1,
    }
}

fn invert(cond: u16) -> u16 {
    cond ^ 1
}

/// 浮点比较的条件码：NaN（无序）时只有 `!=` 为真，其余全为假。
///
/// VFP 的 `vcmp` 在无序时置 C=1、V=1、Z=0、N=0，所以
/// `<` 用 CC（C==0）、`<=` 用 LS（C==0 或 Z==1）、`>` 用 GT、`>=` 用 GE，
/// 这些在无序时都不成立。
fn float_cond(op: SsaBinaryOp) -> u16 {
    use SsaBinaryOp::*;
    match op {
        Eq => 0,  // EQ
        Ne => 1,  // NE
        Lt => 3,  // CC（C 清）
        Ge => 10, // GE
        Gt => 12, // GT
        Le => 9,  // LS（C 清或 Z 置）
        _ => 1,
    }
}

// ==================================================================
// 溢出 / 帧
// ==================================================================

/// 读出 `v`：溢出的值装载到 [`SPILL_SRC`]。
fn val_reg(unit: &mut CompilationUnit, v: SsaValue) -> Reg {
    if let Some(slot) = spill_slot(v) {
        load_spill(unit, SPILL_SRC, slot);
        return SPILL_SRC;
    }
    alloc_reg(v)
}

/// 读出二元的第二操作数（溢出时装到 [`SPILL_SRC2`]）。
fn val_reg2(unit: &mut CompilationUnit, v: SsaValue) -> Reg {
    if let Some(slot) = spill_slot(v) {
        load_spill(unit, SPILL_SRC2, slot);
        return SPILL_SRC2;
    }
    alloc_reg(v)
}

/// 结果的发射目标：溢出的结果先算进 [`SPILL_DST`]。
fn out_reg(v: SsaValue) -> Reg {
    if spill_slot(v).is_some() {
        SPILL_DST
    } else {
        alloc_reg(v)
    }
}

/// 溢出的结果从 [`SPILL_DST`] 落回槽。
fn put_out(unit: &mut CompilationUnit, v: SsaValue) {
    if let Some(slot) = spill_slot(v) {
        store_spill(unit, SPILL_DST, slot);
    }
}

/// `ldr.w rt, [sp, #off]`（off 必须 4 字节对齐且 < 4096）。
fn ldr_sp_imm(unit: &mut CompilationUnit, rt: Reg, off: u32) {
    debug_assert!(off % 4 == 0 && off < 4096, "sp 相对装载偏移越界: {off}");
    w32(unit, 0xf8d0 | 13, (rt.bits() << 12) | (off as u16 & 0xfff));
}

/// `str.w rt, [sp, #off]`。
fn str_sp_imm(unit: &mut CompilationUnit, rt: Reg, off: u32) {
    debug_assert!(off % 4 == 0 && off < 4096, "sp 相对存储偏移越界: {off}");
    w32(unit, 0xf8c0 | 13, (rt.bits() << 12) | (off as u16 & 0xfff));
}

fn load_spill(unit: &mut CompilationUnit, dst: Reg, slot: usize) {
    ldr_sp_imm(unit, dst, (slot * 8) as u32);
}

fn store_spill(unit: &mut CompilationUnit, src: Reg, slot: usize) {
    str_sp_imm(unit, src, (slot * 8) as u32);
}

fn load_spill_hi(unit: &mut CompilationUnit, dst: Reg, slot: usize) {
    ldr_sp_imm(unit, dst, (slot * 8 + 4) as u32);
}

fn store_spill_hi(unit: &mut CompilationUnit, src: Reg, slot: usize) {
    str_sp_imm(unit, src, (slot * 8 + 4) as u32);
}

/// `sub sp, #bytes`。
fn sub_sp_imm(unit: &mut CompilationUnit, bytes: u32) {
    if bytes == 0 {
        return;
    }
    if bytes % 4 == 0 && bytes <= 508 {
        hw(unit, 0xb080 | (bytes / 4) as u16);
    } else {
        mov_imm32(unit, SPILL_DST, bytes);
        w32(unit, 0xebad, 0x0d00 | SPILL_DST.bits()); // sub.w sp, sp, r12
    }
}

/// `add sp, #bytes`。
fn add_sp_imm(unit: &mut CompilationUnit, bytes: u32) {
    if bytes == 0 {
        return;
    }
    if bytes % 4 == 0 && bytes <= 508 {
        hw(unit, 0xb000 | (bytes / 4) as u16);
    } else {
        mov_imm32(unit, SPILL_DST, bytes);
        w32(unit, 0xeb0d, 0x0d00 | SPILL_DST.bits()); // add.w sp, sp, r12
    }
}

/// `add.w rd, sp, #imm`：经 r12 拼大立即数。
fn add_rd_sp_imm(unit: &mut CompilationUnit, rd: Reg, imm: u32) {
    mov_imm32(unit, SPILL_DST, imm);
    w32(unit, 0xeb0d, (rd.bits() << 8) | SPILL_DST.bits()); // add.w rd, sp, r12
}

// ==================================================================
// 内存访问
// ==================================================================

/// `ty` 的（字节宽度，是否有符号）。
fn mem_access(ty: NumTy) -> (usize, bool) {
    match ty {
        NumTy::Int(w) => ((w as usize) / 8, true),
        NumTy::Uint(w) => ((w as usize) / 8, false),
        NumTy::Bool => (1, false),
        NumTy::Float(w) => ((w as usize) / 8, false),
    }
}

/// `rd = [rn]`，按 `ty` 位宽/符号选 32 位 load（寄存器不限）。
fn emit_load_mem(unit: &mut CompilationUnit, rd: Reg, rn: Reg, ty: NumTy) {
    let (bytes, signed) = mem_access(ty);
    let hw1 = match (bytes, signed) {
        (1, false) => 0xf890, // ldrb.w
        (1, true) => 0xf990,  // ldrsb.w
        (2, false) => 0xf8b0, // ldrh.w
        (2, true) => 0xf9b0,  // ldrsh.w
        (4, _) => 0xf8d0,     // ldr.w
        _ => crate::codegen::unsupported_const("64-bit memory access on 32-bit target"),
    };
    w32(unit, hw1 | rn.bits(), rd.bits() << 12);
}

/// `[rn] = rm`，只写 `ty` 位宽对应的低字节。
fn emit_store_mem(unit: &mut CompilationUnit, rn: Reg, rm: Reg, ty: NumTy) {
    let (bytes, _) = mem_access(ty);
    let hw1 = match bytes {
        1 => 0xf880, // strb.w
        2 => 0xf8a0, // strh.w
        4 => 0xf8c0, // str.w
        _ => crate::codegen::unsupported_const("64-bit memory access on 32-bit target"),
    };
    w32(unit, hw1 | rn.bits(), rm.bits() << 12);
}

// ==================================================================
// VFP（浮点）
// ==================================================================

/// `vldr s/d, [sp, #slot*8]`。
fn emit_vldr(unit: &mut CompilationUnit, is_single: bool, reg: u8, slot: usize) {
    let imm = (slot * 2) as u32; // 偏移 = slot*8，imm8 缩放 4
    let base = if is_single { 0xED9D_0A00u32 } else { 0xED9D_0B00u32 };
    word(unit, base | ((reg as u32) << 12) | (imm & 0xFF));
}

/// `vstr s/d, [sp, #slot*8]`。
fn emit_vstr(unit: &mut CompilationUnit, is_single: bool, reg: u8, slot: usize) {
    let imm = (slot * 2) as u32;
    let base = if is_single { 0xED8D_0A00u32 } else { 0xED8D_0B00u32 };
    word(unit, base | ((reg as u32) << 12) | (imm & 0xFF));
}

#[derive(Clone, Copy)]
enum VfpArith {
    Add,
    Sub,
    Mul,
    Div,
}

/// VFP 算术：固定用 s0/s1（或 d0/d1），结果落 s0/d0。
fn emit_vfp_arith(unit: &mut CompilationUnit, is_single: bool, kind: VfpArith) {
    let w = match (is_single, kind) {
        (true, VfpArith::Add) => 0xEE30_0A20,
        (true, VfpArith::Sub) => 0xEE30_0A60,
        (true, VfpArith::Mul) => 0xEE20_0A20,
        (true, VfpArith::Div) => 0xEE80_0A20,
        (false, VfpArith::Add) => 0xEE30_0B01,
        (false, VfpArith::Sub) => 0xEE30_0B41,
        (false, VfpArith::Mul) => 0xEE20_0B01,
        (false, VfpArith::Div) => 0xEE80_0B01,
    };
    word(unit, w);
}

/// `vneg s0/s0` 或 `vneg d0/d0`。
fn emit_vneg(unit: &mut CompilationUnit, is_single: bool) {
    word(unit, if is_single { 0xEEB1_0A40 } else { 0xEEB1_0B40 });
}

/// `vcmp s0, s1` / `vcmp d0, d1` + `vmrs apsr_nzcv, fpscr`。
fn emit_vcmp(unit: &mut CompilationUnit, is_single: bool) {
    word(unit, if is_single { 0xEEB4_0A60 } else { 0xEEB4_0B41 });
    word(unit, 0xEEF1_FA10); // vmrs apsr_nzcv, fpscr
}

/// 整数 -> 浮点：`vmov s0, rCore` 再 `vcvt.f{32,64}.{s,u}32`。
fn emit_vcvt_int_to_fp(unit: &mut CompilationUnit, is_single: bool, signed: bool, core: Reg) {
    word(unit, 0xEE00_0A10 | ((core.0 as u32) << 16)); // vmov s0, rCore
    let w = match (is_single, signed) {
        (true, true) => 0xEEB8_0AC0,
        (true, false) => 0xEEB8_0A40,
        (false, true) => 0xEEB8_0BC0,
        (false, false) => 0xEEB8_0B40,
    };
    word(unit, w);
}

/// 浮点 -> 整数：`vcvt.{s,u}32.f{32,64}` 再 `vmov rCore, s0`。
fn emit_vcvt_fp_to_int(unit: &mut CompilationUnit, is_single: bool, signed: bool, core: Reg) {
    let w = match (is_single, signed) {
        (true, true) => 0xEEBD_0AC0,
        (true, false) => 0xEEBC_0AC0,
        (false, true) => 0xEEBD_0BC0,
        (false, false) => 0xEEBC_0BC0,
    };
    word(unit, w);
    word(unit, 0xEE10_0A10 | ((core.0 as u32) << 16)); // vmov rCore, s0
}

/// 单双精度互转：`vcvt.f32.f64` 或 `vcvt.f64.f32`。
fn emit_vcvt_fp_to_fp(unit: &mut CompilationUnit, dst_is_single: bool) {
    word(unit, if dst_is_single { 0xEEB7_0BC0 } else { 0xEEB7_0AC0 });
}

/// 把浮点常量装进 s0/d0（经整数寄存器中转）。
fn emit_float_const(unit: &mut CompilationUnit, bits: u64, width: u8) {
    if width == 32 {
        mov_imm32(unit, SPILL_DST, bits as u32);
        word(unit, 0xEE00_0A10 | ((SPILL_DST.0 as u32) << 16)); // vmov s0, r12
    } else {
        mov_imm32(unit, SPILL_DST, bits as u32);
        mov_imm32(unit, MUL_TMP, (bits >> 32) as u32);
        // vmov d0, r12(low), r11(high)
        word(
            unit,
            0xEC40_0B10 | ((MUL_TMP.0 as u32) << 16) | ((SPILL_DST.0 as u32) << 12),
        );
    }
}

// ==================================================================
// 指令发射
// ==================================================================

fn emit_mov_rr(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    if rd == rm {
        return;
    }
    if rd.0 < 8 && rm.0 < 8 {
        mov_low(unit, rd, rm);
    } else {
        mov_wide(unit, rd, rm);
    }
}

fn emit_binary(unit: &mut CompilationUnit, dst: Reg, op: SsaBinaryOp, rn: Reg, rm: Reg) {
    use SsaBinaryOp::*;
    match op {
        Add | Sub | BitAnd | And | BitOr | Or | BitXor => dp3(unit, dp_prefix(op), dst, rn, rm),
        Mul => muldiv(unit, 0b0000, dst, rn, rm),
        Div => muldiv(unit, 0b1001, dst, rn, rm),
        Rem => {
            muldiv(unit, 0b1001, MUL_TMP, rn, rm);
            mls(unit, SPILL_DST, MUL_TMP, rm, rn);
            emit_mov_rr(unit, dst, SPILL_DST);
        }
        Shl => shift_reg(unit, 0b0000, dst, rn, rm),
        Shr => shift_reg(unit, 0b0010, dst, rn, rm),
        Eq | Ne | Lt | Gt | Le | Ge => emit_compare(unit, dst, op, rn, rm),
    }
}

fn emit_compare(unit: &mut CompilationUnit, dst: Reg, op: SsaBinaryOp, rn: Reg, rm: Reg) {
    use SsaBinaryOp::*;
    match op {
        Eq | Ne => {
            dp3(unit, 0xea80, dst, rn, rm);
            cmp_imm8(unit, dst, 0);
        }
        _ => cmp_any(unit, rn, rm),
    }
    it(unit, cond_of(op));
    mov_reg_imm8(unit, dst, 1);
    it(unit, invert(cond_of(op)));
    mov_reg_imm8(unit, dst, 0);
}

/// 比较后按浮点条件码置 0/1（把 `cond` 传进来，以区分 NaN 语义）。
fn emit_set_cc(unit: &mut CompilationUnit, dst: Reg, cond: u16) {
    it(unit, cond);
    mov_reg_imm8(unit, dst, 1);
    it(unit, invert(cond));
    mov_reg_imm8(unit, dst, 0);
}

fn emit_unary(unit: &mut CompilationUnit, result: SsaValue, op: SsaUnaryOp, operand: SsaValue) {
    // 浮点取负
    if let Some(slot) = float_slot(operand) {
        if !matches!(op, SsaUnaryOp::Neg) {
            crate::codegen::unsupported_const("非 Neg 的一元浮点运算");
        }
        let w = float_width(operand);
        let is_single = w == 32;
        emit_vldr(unit, is_single, 0, slot);
        emit_vneg(unit, is_single);
        if let Some(rs) = float_slot(result) {
            emit_vstr(unit, is_single, 0, rs);
        }
        return;
    }
    let dst = out_reg(result);
    let src = val_reg(unit, operand);
    match op {
        SsaUnaryOp::Not => emit_compare(unit, dst, SsaBinaryOp::Eq, src, src),
        SsaUnaryOp::BitNot => un_op(unit, 0xea6f, dst, src),
        SsaUnaryOp::Neg => {
            // `rsb dst, src, #0`
            w32(unit, 0xf1c0 | src.bits(), dst.bits() << 8);
        }
    }
    put_out(unit, result);
}

/// 类型转换：取低 w 位并按有无符号扩展。
fn emit_cast(unit: &mut CompilationUnit, dst: Reg, rm: Reg, from: NumTy, to: NumTy) {
    if from.is_float() || to.is_float() {
        crate::codegen::unsupported_cast(from, to);
    }
    let (bits, signed) = match to {
        NumTy::Int(w) => (w, true),
        NumTy::Uint(w) => (w, false),
        NumTy::Bool => (8, false),
        NumTy::Float(_) => unreachable!(),
    };
    match bits {
        8 => extend_any(unit, if signed { 0b00 } else { 0b10 }, dst, rm),
        16 => extend_any(unit, if signed { 0b01 } else { 0b11 }, dst, rm),
        32 => emit_mov_rr(unit, dst, rm),
        _ => crate::codegen::unsupported_cast(from, to),
    }
}

// ==================================================================
// i64（多字长）
// ==================================================================

fn wide_load(unit: &mut CompilationUnit, v: SsaValue, lo: Reg, hi: Reg) {
    let slot = spill_slot(v).expect("i64 值没有宽槽");
    load_spill(unit, lo, slot);
    load_spill_hi(unit, hi, slot);
}

/// `asr.w rd, rm, #amount`（移位立即数 T2）。
fn asr_imm(unit: &mut CompilationUnit, rd: Reg, rm: Reg, amount: u8) {
    shift_imm(unit, 0b10, rd, rm, amount);
}

/// 移位立即数 T2：`typ` = 0b00 lsl / 0b01 lsr / 0b10 asr / 0b11 ror。
fn shift_imm(unit: &mut CompilationUnit, typ: u16, rd: Reg, rm: Reg, amount: u8) {
    let a = (amount & 0x1f) as u16;
    let imm3 = (a >> 2) & 0x7;
    let imm2 = a & 0x3;
    w32(
        unit,
        0xea4f,
        (imm3 << 12) | (imm2 << 6) | (typ << 4) | (rd.bits() << 8) | rm.bits(),
    );
}

/// 读出一个"当作 i64 用"的操作数：本身是 i64 就从宽槽读低/高字；
/// 只是 32 位值（指针、`as i64` 之前的中间值等）就放进 `lo` 并符号扩展
/// 出 `hi`。二元 i64 运算的两侧未必都被标成宽——例如指针相减的结果是
/// i64，但操作数是指针（32 位）。
fn wide_load_any(unit: &mut CompilationUnit, v: SsaValue, lo: Reg, hi: Reg) {
    if is_wide(v) {
        let slot = spill_slot(v).expect("i64 值没有宽槽");
        load_spill(unit, lo, slot);
        load_spill_hi(unit, hi, slot);
    } else if let Some(slot) = spill_slot(v) {
        load_spill(unit, lo, slot);
        asr_imm(unit, hi, lo, 31);
    } else {
        let r = alloc_reg(v);
        if r != lo {
            emit_mov_rr(unit, lo, r);
        }
        asr_imm(unit, hi, lo, 31);
    }
}

fn wide_store(unit: &mut CompilationUnit, v: SsaValue, lo: Reg, hi: Reg) {
    let slot = spill_slot(v).expect("i64 值没有宽槽");
    store_spill(unit, lo, slot);
    store_spill_hi(unit, hi, slot);
}

fn emit_const_wide(unit: &mut CompilationUnit, result: SsaValue, i: i128) {
    let slot = spill_slot(result).expect("i64 常量没有宽槽");
    let lo = (i as u64 & 0xffff_ffff) as u32;
    let hi = ((i as u64) >> 32) as u32;
    mov_imm32(unit, SPILL_SRC, lo);
    store_spill(unit, SPILL_SRC, slot);
    mov_imm32(unit, SPILL_SRC2, hi);
    store_spill_hi(unit, SPILL_SRC2, slot);
}

fn emit_addsub_wide(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    wide_load_any(unit, lhs, SPILL_SRC, SPILL_SRC2);
    wide_load_any(unit, rhs, MUL_TMP, SPILL_DST);
    if op == SsaBinaryOp::Add {
        adds(unit, SPILL_SRC, SPILL_SRC, MUL_TMP);
        adcs(unit, SPILL_SRC2, SPILL_SRC2, SPILL_DST);
    } else {
        subs(unit, SPILL_SRC, SPILL_SRC, MUL_TMP);
        sbcs(unit, SPILL_SRC2, SPILL_SRC2, SPILL_DST);
    }
    wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
}

fn emit_mul_wide(unit: &mut CompilationUnit, result: SsaValue, lhs: SsaValue, rhs: SsaValue) {
    //   lo = a_lo * b_lo（低 32）
    //   hi = high(a_lo*b_lo) + a_lo*b_hi + a_hi*b_lo   （各取低 32）
    // 低 64 位与有/无符号无关（二补码乘法低位一致），用 umull 即可。
    let slot = spill_slot(result).expect("i64 乘法没有宽槽");
    wide_load_any(unit, lhs, SPILL_SRC, SPILL_SRC2); // a_lo=r9, a_hi=r10
    wide_load_any(unit, rhs, MUL_TMP, SPILL_DST); // b_lo=r11, b_hi=r12
    // 先把 a_lo 暂存到结果槽（后面 lo 乘积会覆写 r9）。
    store_spill(unit, SPILL_SRC, slot);
    // r9 = a_lo * b_hi
    muldiv(unit, 0b0000, SPILL_SRC, SPILL_SRC, SPILL_DST);
    // r12 = a_hi * b_lo
    muldiv(unit, 0b0000, SPILL_DST, SPILL_SRC2, MUL_TMP);
    // r10 = 交叉项之和
    dp3(unit, 0xeb00, SPILL_SRC2, SPILL_SRC, SPILL_DST);
    // 重载 a_lo，做 lo 乘积（r9=lo, r11=high）
    load_spill(unit, SPILL_SRC, slot);
    umull(unit, SPILL_SRC, MUL_TMP, SPILL_SRC, MUL_TMP);
    // r10 = 交叉项 + high
    dp3(unit, 0xeb00, SPILL_SRC2, MUL_TMP, SPILL_SRC2);
    wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
}

fn emit_compare_wide(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    wide_load_any(unit, lhs, SPILL_SRC, SPILL_SRC2);
    wide_load_any(unit, rhs, MUL_TMP, SPILL_DST);
    cmp_any(unit, SPILL_SRC, MUL_TMP);
    it(unit, cond_of(SsaBinaryOp::Eq));
    cmp_any(unit, SPILL_SRC2, SPILL_DST);
    let dst = out_reg(result);
    emit_set_cc(unit, dst, cond_of(op));
    put_out(unit, result);
}

/// 变量移位的 i64 逻辑（`lhs << rhs` / `lhs >> rhs`，算术右移）。
///
/// Thumb 没有 A32 的条件执行后缀，故用 `cmp cnt, #32` + 条件跳转把移位量
/// 分成 `< 32` 与 `>= 32` 两段：前段做双字拼接（跨字进位用 `32 - cnt` 的
/// 反向移位补进来），后段整字搬移并补符号/零。Thumb-2 的寄存器移位指令
/// (`LSL.W`/`LSR.W`/`ASR.W Rd, Rn, Rm`) 允许 Rd == Rm，因此拼进位不需要
/// 额外的暂存寄存器（r9=lo、r10=hi、r11=cnt、r12=scratch 恰好够用）。
fn emit_shift_wide(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    wide_load_any(unit, lhs, SPILL_SRC, SPILL_SRC2);
    wide_load_any(unit, rhs, MUL_TMP, SPILL_DST);
    cmp_imm8(unit, MUL_TMP, 32);
    let less = fresh_label();
    let end = fresh_label();
    emit_b_cond(unit, 0b0011, &less); // CC / LO：cnt < 32

    // --- cnt >= 32：整字搬移 ---
    sub_imm8(unit, MUL_TMP, MUL_TMP, 32); // cnt -= 32
    match op {
        SsaBinaryOp::Shl => {
            shift_reg(unit, 0b0000, SPILL_SRC, SPILL_SRC, MUL_TMP); // lo = lo << (cnt-32)
            mov_reg_imm8(unit, SPILL_SRC2, 0); // hi = 0
        }
        SsaBinaryOp::Shr => {
            shift_reg(unit, 0b0010, SPILL_SRC, SPILL_SRC2, MUL_TMP); // lo = asr(hi, cnt-32)
            asr_imm(unit, SPILL_SRC2, SPILL_SRC2, 31); // hi = 符号位
        }
        _ => unreachable!(),
    }
    emit_b(unit, &end);

    // --- cnt < 32：双字拼接 ---
    define_label(unit, &less);
    match op {
        SsaBinaryOp::Shl => {
            // lo' = lo << cnt;  hi' = (hi << cnt) | (lo >> (32-cnt))
            rsb_imm8(unit, SPILL_DST, MUL_TMP, 32); // scratch = 32 - cnt
            shift_reg(unit, 0b0001, SPILL_DST, SPILL_SRC, SPILL_DST); // scratch = lo >> (32-cnt)
            shift_reg(unit, 0b0000, SPILL_SRC2, SPILL_SRC2, MUL_TMP); // hi <<= cnt
            shift_reg(unit, 0b0000, SPILL_SRC, SPILL_SRC, MUL_TMP); // lo <<= cnt
            orr(unit, SPILL_SRC2, SPILL_SRC2, SPILL_DST); // hi |= 进位
        }
        SsaBinaryOp::Shr => {
            // hi' = hi >> cnt;  lo' = (lo >> cnt) | (hi << (32-cnt))
            rsb_imm8(unit, SPILL_DST, MUL_TMP, 32); // scratch = 32 - cnt
            shift_reg(unit, 0b0000, SPILL_DST, SPILL_SRC2, SPILL_DST); // scratch = hi << (32-cnt)
            shift_reg(unit, 0b0010, SPILL_SRC2, SPILL_SRC2, MUL_TMP); // hi >>= cnt（算术）
            shift_reg(unit, 0b0001, SPILL_SRC, SPILL_SRC, MUL_TMP); // lo >>= cnt
            orr(unit, SPILL_SRC, SPILL_SRC, SPILL_DST); // lo |= 进位
        }
        _ => unreachable!(),
    }
    define_label(unit, &end);
    wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
}

/// i64 除以/取余一个 2 的幂常量：通用 64 位除法代价过高，而元素大小、
/// 对齐等场景的除数几乎都是 2 的幂，用移位/掩码即可（除数须为正）。
fn emit_divrem_wide_const(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    divisor: i128,
) {
    let mag = divisor.unsigned_abs();
    let k = mag.trailing_zeros();
    if divisor <= 0 || mag != 1u128 << k || k > 31 {
        crate::codegen::unsupported_binary_op(op);
    }
    wide_load_any(unit, lhs, SPILL_SRC, SPILL_SRC2);
    if op == SsaBinaryOp::Div {
        if k > 0 {
            let k = k as u8;
            shift_imm(unit, 0b01, SPILL_DST, SPILL_SRC, k); // lsr lo, k
            shift_imm(unit, 0b00, MUL_TMP, SPILL_SRC2, 32 - k); // lsl hi, 32-k
            dp3(unit, 0xea40, SPILL_SRC, SPILL_DST, MUL_TMP); // orr → lo
            shift_imm(unit, 0b10, SPILL_SRC2, SPILL_SRC2, k); // asr hi, k
        }
    } else {
        if k > 0 {
            mov_imm32(unit, SPILL_DST, mag as u32 - 1);
            dp3(unit, 0xea00, SPILL_SRC, SPILL_SRC, SPILL_DST); // and → lo
        } else {
            mov_reg_imm8(unit, SPILL_SRC, 0);
        }
        mov_reg_imm8(unit, SPILL_SRC2, 0);
    }
    wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
}

fn emit_cast_wide(
    unit: &mut CompilationUnit,
    result: SsaValue,
    src: SsaValue,
    from: NumTy,
    to: NumTy,
) {
    let to_wide = matches!(to, NumTy::Int(64) | NumTy::Uint(64));
    let from_wide = matches!(from, NumTy::Int(64) | NumTy::Uint(64));
    if from_wide && to_wide {
        wide_load(unit, src, SPILL_SRC, SPILL_SRC2);
        wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
    } else if !from_wide && to_wide {
        let s = val_reg(unit, src);
        let signed = matches!(from, NumTy::Int(_));
        if s != SPILL_SRC {
            emit_mov_rr(unit, SPILL_SRC, s);
        }
        if signed {
            // hi = asr(lo, 31)
            asr_imm(unit, SPILL_SRC2, SPILL_SRC, 31);
        } else {
            mov_reg_imm8(unit, SPILL_SRC2, 0);
        }
        wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
    } else {
        wide_load(unit, src, SPILL_SRC, SPILL_SRC2);
        let dst = out_reg(result);
        emit_mov_rr(unit, dst, SPILL_SRC);
        put_out(unit, result);
    }
}

// ==================================================================
// 浮点发射
// ==================================================================

fn emit_binary_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
    ty: NumTy,
) {
    use SsaBinaryOp::*;
    let slot = |v: SsaValue| float_slot(v).unwrap_or_else(|| panic!("浮点操作数没有栈槽: {v:?}"));
    let mut w = float_width(lhs).max(float_width(rhs));
    if w == 0 {
        w = match ty {
            NumTy::Float(w) => w,
            _ => 64,
        };
    }
    let is_single = w == 32;
    emit_vldr(unit, is_single, 0, slot(lhs));
    emit_vldr(unit, is_single, 1, slot(rhs));

    match op {
        Add => {
            emit_vfp_arith(unit, is_single, VfpArith::Add);
            if let Some(rs) = float_slot(result) {
                emit_vstr(unit, is_single, 0, rs);
            }
        }
        Sub => {
            emit_vfp_arith(unit, is_single, VfpArith::Sub);
            if let Some(rs) = float_slot(result) {
                emit_vstr(unit, is_single, 0, rs);
            }
        }
        Mul => {
            emit_vfp_arith(unit, is_single, VfpArith::Mul);
            if let Some(rs) = float_slot(result) {
                emit_vstr(unit, is_single, 0, rs);
            }
        }
        Div => {
            emit_vfp_arith(unit, is_single, VfpArith::Div);
            if let Some(rs) = float_slot(result) {
                emit_vstr(unit, is_single, 0, rs);
            }
        }
        Eq | Ne | Lt | Gt | Le | Ge => {
            emit_vcmp(unit, is_single);
            let dst = out_reg(result);
            emit_set_cc(unit, dst, float_cond(op));
            put_out(unit, result);
        }
        _ => crate::codegen::unsupported_binary_op(op),
    }
}

fn emit_cast_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    src: SsaValue,
    from: NumTy,
    to: NumTy,
) {
    match (from, to) {
        (NumTy::Int(_) | NumTy::Uint(_), NumTy::Float(w)) => {
            let is_single = w == 32;
            let signed = matches!(from, NumTy::Int(_));
            let src_reg = val_reg(unit, src);
            emit_vcvt_int_to_fp(unit, is_single, signed, src_reg);
            if let Some(slot) = float_slot(result) {
                emit_vstr(unit, is_single, 0, slot);
            }
        }
        (NumTy::Float(_), NumTy::Int(_) | NumTy::Uint(_)) => {
            let src_w = float_width(src);
            let is_single = src_w == 32;
            let signed = matches!(to, NumTy::Int(_));
            emit_vldr(unit, is_single, 0, float_slot(src).unwrap());
            let dst = out_reg(result);
            emit_vcvt_fp_to_int(unit, is_single, signed, dst);
            put_out(unit, result);
        }
        (NumTy::Float(from_w), NumTy::Float(to_w)) => {
            let src_is_single = from_w == 32;
            let dst_is_single = to_w == 32;
            emit_vldr(unit, src_is_single, 0, float_slot(src).unwrap());
            emit_vcvt_fp_to_fp(unit, dst_is_single);
            if let Some(slot) = float_slot(result) {
                emit_vstr(unit, dst_is_single, 0, slot);
            }
        }
        _ => crate::codegen::unsupported_cast(from, to),
    }
}

/// `bl target`（T1，带重定位）。
fn emit_bl(unit: &mut CompilationUnit, target: &str) {
    let at = unit.code.len();
    w32(unit, 0xf7ff, 0xfffe);
    unit.relocations.push(Relocation {
        offset: at,
        instr_start: at,
        instr_len: 4,
        symbol: target.to_string(),
        kind: RelocationKind::ArmCallThumb,
        addend: 0,
    });
}

/// `b label`（T4）。
fn emit_b(unit: &mut CompilationUnit, target: &str) {
    let at = unit.code.len();
    w32(unit, 0xf000, 0xbffe);
    unit.relocations.push(Relocation {
        offset: at,
        instr_start: at,
        instr_len: 4,
        symbol: target.to_string(),
        kind: RelocationKind::ArmBranchThumb,
        addend: 0,
    });
}

/// `b<cond> label`（T1，16 位条件分支，范围 ±256 字节）。
///
/// Thumb 没有 A32 的条件执行后缀，宽移位按 `cnt < 32` 分两段时用它绕开。
/// 立即数落在 [7:0]（8 位半字偏移），由 `ArmBranchThumb`（`instr_len == 2`）回填。
fn emit_b_cond(unit: &mut CompilationUnit, cond: u16, target: &str) {
    let at = unit.code.len();
    hw(unit, 0xd000 | ((cond & 0xf) << 8));
    unit.relocations.push(Relocation {
        offset: at,
        instr_start: at,
        instr_len: 2,
        symbol: target.to_string(),
        kind: RelocationKind::ArmBranchThumb,
        addend: 0,
    });
}

/// `cbz` / `cbnz rn, label`（16 位），高寄存器退化为 `cmp #0` + 条件跳。
fn emit_branch_if_nonzero(
    unit: &mut CompilationUnit,
    rn: Reg,
    true_target: &str,
    false_target: &str,
) {
    if rn.0 <= 3 {
        let at = unit.code.len();
        hw(unit, 0xb900 | rn.bits());
        unit.relocations.push(Relocation {
            offset: at + 2,
            instr_start: at,
            instr_len: 2,
            symbol: true_target.to_string(),
            kind: RelocationKind::ArmCbzThumb,
            addend: 0,
        });
    } else {
        cmp_imm8(unit, rn, 0);
        it(unit, cond_of(SsaBinaryOp::Ne));
        emit_b(unit, true_target);
        emit_b(unit, false_target);
    }
}

/// 把实参搬进 AAPCS 寄存器；第 5+ 个整型实参写进栈传参区，返回其字节数。
fn emit_call_args(unit: &mut CompilationUnit, args: &[SsaValue]) -> u32 {
    let float_args: Vec<SsaValue> = args.iter().copied().filter(|a| float_slot(*a).is_some()).collect();
    let int_args: Vec<SsaValue> = args.iter().copied().filter(|a| float_slot(*a).is_none()).collect();

    // 栈上传参：每个占 8 字节。
    let mut stack = 0u32;
    let mut gi = 0usize;
    let mut reg_ints: Vec<SsaValue> = Vec::new();
    for a in &int_args {
        let need = if is_wide(*a) { 2 } else { 1 };
        if gi + need <= ARG_REGS.len() {
            reg_ints.push(*a);
            gi += need;
        } else {
            stack += 8;
        }
    }
    if stack > 0 {
        sub_sp_imm(unit, stack);
        let mut off = 0u32;
        let mut gi2 = 0usize;
        for a in &int_args {
            let need = if is_wide(*a) { 2 } else { 1 };
            if gi2 + need <= ARG_REGS.len() {
                gi2 += need;
                continue;
            }
            if is_wide(*a) {
                wide_load(unit, *a, SPILL_DST, SPILL_SRC2);
                str_sp_imm(unit, SPILL_DST, off);
                str_sp_imm(unit, SPILL_SRC2, off + 4);
            } else {
                let r = val_reg(unit, *a);
                str_sp_imm(unit, r, off);
            }
            off += 8;
            gi2 += need;
        }
    }

    // 浮点实参进 s0-s7/d0-d7。
    for (i, a) in float_args.iter().enumerate() {
        if i >= 8 {
            crate::codegen::unsupported_const("超过 8 个浮点实参（栈上传参尚未实现）");
        }
        let w = float_width(*a);
        let is_single = w == 32;
        emit_vldr(unit, is_single, i as u8, float_slot(*a).unwrap());
    }

    // 整数实参进 r0-r3（i64 占两个连续寄存器）。
    let mut gi3 = 0usize;
    for a in &int_args {
        if gi3 >= ARG_REGS.len() {
            break;
        }
        if is_wide(*a) {
            if gi3 + 2 > ARG_REGS.len() {
                break;
            }
            wide_load(unit, *a, SPILL_SRC, SPILL_SRC2);
            emit_mov_rr(unit, ARG_REGS[gi3], SPILL_SRC);
            emit_mov_rr(unit, ARG_REGS[gi3 + 1], SPILL_SRC2);
            gi3 += 2;
        } else {
            let src = val_reg(unit, *a);
            emit_mov_rr(unit, ARG_REGS[gi3], src);
            gi3 += 1;
        }
    }
    stack
}

fn emit_instr(unit: &mut CompilationUnit, instr: &SsaInstr) {
    match instr {
        SsaInstr::Const { result, value } => match value {
            SsaConst::Int(i) => {
                if is_wide(*result) {
                    emit_const_wide(unit, *result, *i);
                } else {
                    mov_imm32(unit, out_reg(*result), *i as u32);
                    put_out(unit, *result);
                }
            }
            SsaConst::Bool(b) => {
                mov_reg_imm8(unit, out_reg(*result), u8::from(*b));
                put_out(unit, *result);
            }
            SsaConst::Unit => {
                mov_reg_imm8(unit, out_reg(*result), 0);
                put_out(unit, *result);
            }
            SsaConst::Float(bits, w) => {
                emit_float_const(unit, *bits, *w);
                if let Some(slot) = float_slot(*result) {
                    emit_vstr(unit, *w == 32, 0, slot);
                }
            }
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
        } => emit_unary(unit, *result, *op, *operand),
        SsaInstr::Binary {
            result,
            op,
            lhs,
            rhs,
            ty,
        } => {
            if is_wide(*lhs) || is_wide(*rhs) {
                match op {
                    SsaBinaryOp::Add | SsaBinaryOp::Sub => {
                        emit_addsub_wide(unit, *result, *op, *lhs, *rhs)
                    }
                    SsaBinaryOp::Mul => emit_mul_wide(unit, *result, *lhs, *rhs),
                    SsaBinaryOp::Shl | SsaBinaryOp::Shr => {
                        emit_shift_wide(unit, *result, *op, *lhs, *rhs)
                    }
                    SsaBinaryOp::Eq
                    | SsaBinaryOp::Ne
                    | SsaBinaryOp::Lt
                    | SsaBinaryOp::Gt
                    | SsaBinaryOp::Le
                    | SsaBinaryOp::Ge => emit_compare_wide(unit, *result, *op, *lhs, *rhs),
                    SsaBinaryOp::Div | SsaBinaryOp::Rem => {
                        let d = CONSTS.with(|c| c.borrow().get(&rhs.id()).copied());
                        match d {
                            Some(d) => emit_divrem_wide_const(unit, *result, *op, *lhs, d),
                            None => crate::codegen::unsupported_binary_op(*op),
                        }
                    }
                    _ => crate::codegen::unsupported_binary_op(*op),
                }
            } else if float_slot(*lhs).is_some() || float_slot(*rhs).is_some() || ty.is_float() {
                emit_binary_float(unit, *result, *op, *lhs, *rhs, *ty);
            } else {
                let dst = out_reg(*result);
                let rn = val_reg(unit, *lhs);
                let rm = if rn == out_reg(*result) {
                    val_reg2(unit, *rhs)
                } else {
                    // 结果寄存器可能与左操作数相同，先读左再读右
                    val_reg2(unit, *rhs)
                };
                emit_binary(unit, dst, *op, rn, rm);
                put_out(unit, *result);
            }
        }
        SsaInstr::Call {
            result,
            callee,
            args,
            ..
        } => {
            let out_bytes = emit_call_args(unit, args);
            emit_bl(unit, callee);
            if out_bytes > 0 {
                add_sp_imm(unit, out_bytes);
            }
            if is_wide(*result) {
                wide_store(unit, *result, Reg(0), Reg(1));
            } else if let Some(slot) = float_slot(*result) {
                let is_single = float_width(*result) == 32;
                emit_vstr(unit, is_single, 0, slot);
            } else if let Some(slot) = spill_slot(*result) {
                store_spill(unit, Reg(0), slot);
            } else {
                emit_mov_rr(unit, alloc_reg(*result), Reg(0));
            }
        }
        SsaInstr::Phi { .. } => {}
        SsaInstr::Cast {
            result,
            src,
            from,
            to,
        } => {
            if matches!(from, NumTy::Int(64) | NumTy::Uint(64))
                || matches!(to, NumTy::Int(64) | NumTy::Uint(64))
            {
                emit_cast_wide(unit, *result, *src, *from, *to);
            } else if from.is_float() || to.is_float() || float_slot(*src).is_some() {
                emit_cast_float(unit, *result, *src, *from, *to);
            } else {
                let s = val_reg(unit, *src);
                emit_cast(unit, out_reg(*result), s, *from, *to);
                put_out(unit, *result);
            }
        }
        SsaInstr::Copy { result, src } => {
            if is_wide(*result) || is_wide(*src) {
                wide_load_any(unit, *src, SPILL_SRC, SPILL_SRC2);
                if is_wide(*result) {
                    wide_store(unit, *result, SPILL_SRC, SPILL_SRC2);
                } else {
                    let dst = out_reg(*result);
                    emit_mov_rr(unit, dst, SPILL_SRC);
                    put_out(unit, *result);
                }
            } else if let (Some(rs), Some(ss)) = (float_slot(*result), float_slot(*src)) {
                let w = float_width(*result).max(float_width(*src));
                emit_vldr(unit, w == 32, 0, ss);
                emit_vstr(unit, w == 32, 0, rs);
            } else {
                match (spill_slot(*result), spill_slot(*src)) {
                    (Some(ds), Some(ss)) => {
                        load_spill(unit, SPILL_SRC, ss);
                        store_spill(unit, SPILL_SRC, ds);
                    }
                    (Some(ds), None) => {
                        let r = val_reg(unit, *src);
                        store_spill(unit, r, ds);
                    }
                    (None, Some(ss)) => {
                        load_spill(unit, out_reg(*result), ss);
                    }
                    (None, None) => {
                        let (d, s) = (alloc_reg(*result), alloc_reg(*src));
                        if d != s {
                            emit_mov_rr(unit, d, s);
                        }
                    }
                }
            }
        }
        SsaInstr::Var { .. } => {}
        SsaInstr::SlotAddr { result, local } => {
            let slot = LOCAL_SLOTS.with(|m| m.borrow().get(local).copied().unwrap_or(0));
            let off = slot * 8 + SPILL_BASE.get();
            add_rd_sp_imm(unit, out_reg(*result), off as u32);
            put_out(unit, *result);
        }
        SsaInstr::Load {
            result, ptr, ty, ..
        } => {
            if is_wide(*result) || matches!(ty, NumTy::Int(64) | NumTy::Uint(64)) {
                let p = val_reg(unit, *ptr);
                emit_load_mem(unit, SPILL_SRC, p, NumTy::Int(32));
                mov_reg_imm8(unit, SPILL_DST, 4);
                dp3(unit, 0xeb00, SPILL_DST, p, SPILL_DST); // add sp dst= p+4
                emit_load_mem(unit, SPILL_SRC2, SPILL_DST, NumTy::Int(32));
                wide_store(unit, *result, SPILL_SRC, SPILL_SRC2);
            } else {
                let d = out_reg(*result);
                let p = val_reg(unit, *ptr);
                emit_load_mem(unit, d, p, *ty);
                put_out(unit, *result);
            }
        }
        SsaInstr::Store { ptr, value, ty, .. } => {
            if is_wide(*value) || matches!(ty, NumTy::Int(64) | NumTy::Uint(64)) {
                let p = val_reg(unit, *ptr);
                wide_load_any(unit, *value, SPILL_SRC, SPILL_SRC2);
                emit_store_mem(unit, p, SPILL_SRC, NumTy::Int(32));
                mov_reg_imm8(unit, SPILL_DST, 4);
                dp3(unit, 0xeb00, SPILL_DST, p, SPILL_DST);
                emit_store_mem(unit, SPILL_DST, SPILL_SRC2, NumTy::Int(32));
            } else {
                let p = val_reg(unit, *ptr);
                let v = val_reg2(unit, *value);
                emit_store_mem(unit, p, v, *ty);
            }
        }
    }
}

/// 为浮点 SSA 值分配栈槽，返回槽数（每槽 8 字节）。
fn assign_float_slots(func: &SsaFunction) -> usize {
    let mut floats: HashMap<u32, u8> = HashMap::new();
    let width_of =
        |v: &SsaValue, map: &HashMap<u32, u8>| -> u8 { map.get(&v.id()).copied().unwrap_or(0) };
    loop {
        let mut changed = false;
        let mut mark = |v: &SsaValue, w: u8, map: &mut HashMap<u32, u8>| -> bool {
            match map.get(&v.id()) {
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

/// 被取地址的局部变量 -> 栈槽编号，返回槽数；映射存进 `LOCAL_SLOTS`。
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

fn block_label(func: &str, id: usize) -> String {
    format!("{func}.bb{id}")
}

/// 把一组寄存器搬运并行执行（成环时经 [`PHI_TMP`] 打散）。
fn emit_parallel_moves(unit: &mut CompilationUnit, moves: &mut Vec<(Reg, Reg)>) {
    moves.retain(|(dst, src)| dst != src);
    while !moves.is_empty() {
        let safe = moves
            .iter()
            .position(|(dst, _)| !moves.iter().any(|(_, src)| src == dst));
        match safe {
            Some(at) => {
                let (dst, src) = moves.remove(at);
                emit_mov_rr(unit, dst, src);
            }
            None => {
                let src = moves[0].1;
                emit_mov_rr(unit, PHI_TMP, src);
                for m in moves.iter_mut() {
                    if m.1 == src {
                        m.1 = PHI_TMP;
                    }
                }
            }
        }
    }
}

fn emit_phi_copies(unit: &mut CompilationUnit, func: &SsaFunction, succ: usize, from: usize) {
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
        if is_wide(*result) || is_wide(*src) {
            wide_load_any(unit, *src, SPILL_SRC, SPILL_SRC2);
            if is_wide(*result) {
                wide_store(unit, *result, SPILL_SRC, SPILL_SRC2);
            } else if let Some(dst_slot) = spill_slot(*result) {
                store_spill(unit, SPILL_SRC, dst_slot);
            } else {
                emit_mov_rr(unit, alloc_reg(*result), SPILL_SRC);
            }
        } else if let (Some(dst_slot), Some(src_slot)) = (spill_slot(*result), spill_slot(*src)) {
            load_spill(unit, SPILL_DST, src_slot);
            store_spill(unit, SPILL_DST, dst_slot);
        } else if let Some(dst_slot) = spill_slot(*result) {
            store_stack.push((dst_slot, alloc_reg(*src)));
        } else if let Some(src_slot) = spill_slot(*src) {
            load_stack.push((alloc_reg(*result), src_slot));
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
        emit_mov_rr(unit, *dst, SPILL_SRC);
    }
}

/// 把形参从 ABI 寄存器搬到分配结果 / 栈槽。
fn emit_param_moves(unit: &mut CompilationUnit, func: &SsaFunction, stack_base: u32) {
    let mut gi = 0usize;
    let mut fi = 0usize;
    for p in &func.params {
        if let Some(slot) = float_slot(*p) {
            let is_single = float_width(*p) == 32;
            if fi < 8 {
                emit_vstr(unit, is_single, fi as u8, slot);
            } else {
                crate::codegen::unsupported_const("超过 8 个浮点形参（栈上传参尚未实现）");
            }
            fi += 1;
            continue;
        }
        if is_wide(*p) {
            if gi + 2 <= ARG_REGS.len() {
                if let Some(slot) = spill_slot(*p) {
                    store_spill(unit, ARG_REGS[gi], slot);
                    store_spill_hi(unit, ARG_REGS[gi + 1], slot);
                }
                gi += 2;
            } else {
                let off = stack_base + ((gi - ARG_REGS.len()) * 8) as u32;
                if let Some(slot) = spill_slot(*p) {
                    ldr_sp_imm(unit, SPILL_DST, off);
                    store_spill(unit, SPILL_DST, slot);
                    ldr_sp_imm(unit, SPILL_DST, off + 4);
                    store_spill_hi(unit, SPILL_DST, slot);
                }
                gi += 2;
            }
            continue;
        }
        if gi >= ARG_REGS.len() {
            let off = stack_base + ((gi - ARG_REGS.len()) * 8) as u32;
            if let Some(slot) = spill_slot(*p) {
                ldr_sp_imm(unit, SPILL_DST, off);
                store_spill(unit, SPILL_DST, slot);
            } else {
                let r = alloc_reg(*p);
                ldr_sp_imm(unit, r, off);
            }
            gi += 1;
            continue;
        }
        if let Some(slot) = spill_slot(*p) {
            store_spill(unit, ARG_REGS[gi], slot);
        } else {
            let r = alloc_reg(*p);
            if r != ARG_REGS[gi] {
                emit_mov_rr(unit, r, ARG_REGS[gi]);
            }
        }
        gi += 1;
    }
}

/// 发射一个函数。
pub fn emit_function(
    target: &crate::codegen::target::Target,
    unit: &mut CompilationUnit,
    func: &SsaFunction,
) {
    debug_assert!(target.is_arm32_thumb(), "Thumb-2 后端只用于 thumb 目标");

    // 函数内局部标签用函数名前缀 + 每函数复位的计数器命名（确定性 + 唯一）。
    LOCAL_LABEL_PREFIX.with(|p| *p.borrow_mut() = func.name.clone());
    LOCAL_LABEL.with(|c| c.set(0));

    let wide = crate::codegen::regalloc::assign_int_widths(func);
    let alloc = crate::codegen::regalloc::allocate_registers(
        func,
        ALLOCATABLE.len(),
        &CALLEE_SAVE,
        ARG_REGS.len(),
        &wide,
    );
    WIDE_VALUES.with(|w| {
        w.borrow_mut().clear();
        w.borrow_mut().extend(wide.iter().copied());
    });
    CONSTS.with(|c| {
        let mut map = c.borrow_mut();
        map.clear();
        for block in &func.blocks {
            for instr in &block.instrs {
                if let SsaInstr::Const {
                    result,
                    value: SsaConst::Int(i),
                } = instr
                {
                    map.insert(result.id(), *i);
                }
            }
        }
    });
    REG_MAP.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for (&v, loc) in &alloc.map {
            if let crate::codegen::regalloc::Location::Reg(i) = loc {
                map.insert(v, ALLOCATABLE[*i]);
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

    // 序言：`push {r4-r7, lr}`（5 个寄存器 = 20 字节）。
    push_regs(unit, &[Reg(4), Reg(5), Reg(6), Reg(7), Reg(14)]);

    let float_slots = assign_float_slots(func);
    let float_bytes = (float_slots * 8) as u32;
    let slot_count = assign_local_slots(func, float_slots);
    let slot_bytes = (slot_count * 8) as u32;
    let spill_count = SPILL_SLOTS.with(|m| m.borrow().len());
    let spill_bytes = (spill_count * 8) as u32;
    SPILL_BASE.with(|c| c.set(spill_bytes as usize));

    if float_bytes > 0 {
        sub_sp_imm(unit, float_bytes);
    }
    if slot_bytes > 0 {
        sub_sp_imm(unit, slot_bytes);
    }
    if spill_bytes > 0 {
        sub_sp_imm(unit, spill_bytes);
    }

    let stack_arg_base = 20 + float_bytes + slot_bytes + spill_bytes;
    emit_param_moves(unit, func, stack_arg_base);

    let mut block_offsets = Vec::with_capacity(func.blocks.len());
    for block in &func.blocks {
        block_offsets.push(unit.code.len());
        for instr in &block.instrs {
            emit_instr(unit, instr);
        }
        for succ in successors(block) {
            emit_phi_copies(unit, func, succ, block.id);
        }
        match &block.term {
            SsaTerminator::Return(ret) => {
                if let Some(v) = ret {
                    if let Some(slot) = float_slot(*v) {
                        emit_vldr(unit, float_width(*v) == 32, 0, slot);
                    } else if is_wide(*v) {
                        wide_load(unit, *v, Reg(0), Reg(1));
                    } else {
                        let r = val_reg(unit, *v);
                        emit_mov_rr(unit, Reg(0), r);
                    }
                }
                if spill_bytes > 0 {
                    add_sp_imm(unit, spill_bytes);
                }
                if slot_bytes > 0 {
                    add_sp_imm(unit, slot_bytes);
                }
                if float_bytes > 0 {
                    add_sp_imm(unit, float_bytes);
                }
                pop_regs(unit, &[Reg(4), Reg(5), Reg(6), Reg(7), Reg(15)]);
            }
            SsaTerminator::Goto(t) => emit_b(unit, &block_label(&func.name, *t)),
            SsaTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                let r = val_reg(unit, *cond);
                emit_branch_if_nonzero(
                    unit,
                    r,
                    &block_label(&func.name, *then_block),
                    &block_label(&func.name, *else_block),
                );
            }
            SsaTerminator::Unreachable => hw(unit, 0xbf00), // nop
        }
    }

    for (i, &off) in block_offsets.iter().enumerate() {
        if let Some(sym) = unit.symbols.get_mut(sym_base + i) {
            sym.offset = off;
        }
    }
    let end = unit.code.len();
    if let Some(sym) = unit.symbols.get_mut(sym_base) {
        sym.size = end - start;
    }
    unit.symbols.push(Symbol {
        name: func.name.clone(),
        section: SymbolSection::Text,
        offset: start,
        size: end - start,
        global: true,
        kind: SymbolKind::Function,
    });
}

/// `push {reglist}`（16 位，只支持低 8 个寄存器 + lr）。
fn push_regs(unit: &mut CompilationUnit, regs: &[Reg]) {
    let mut mask: u16 = 0;
    for r in regs {
        match r.0 {
            0..=7 => mask |= 1 << r.0,
            14 | 8 => mask |= 1u16 << 8,
            _ => {}
        }
    }
    if mask != 0 {
        hw(unit, 0xb400 | mask);
    }
}

/// `pop {reglist}`（16 位）。
fn pop_regs(unit: &mut CompilationUnit, regs: &[Reg]) {
    let mut mask: u16 = 0;
    for r in regs {
        match r.0 {
            0..=7 => mask |= 1u16 << r.0,
            8 | 15 => mask |= 1u16 << 8,
            _ => {}
        }
    }
    if mask != 0 {
        hw(unit, 0xbc00 | mask);
    }
}

/// 入口桩：调用 `main` 后退出。
pub fn emit_start_wrapper(target: &crate::codegen::target::Target, unit: &mut CompilationUnit) {
    debug_assert!(target.is_arm32_thumb());
    let start = unit.code.len();
    emit_bl(unit, "main");
    emit_bl(unit, "zr_exit");
    hw(unit, 0xbf00); // nop

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

    fn empty_unit() -> CompilationUnit {
        CompilationUnit::default()
    }

    fn halfwords(u: &CompilationUnit) -> Vec<u16> {
        u.code
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)]))
            .collect()
    }

    /// 每条指令的参考半字由 `clang --target=thumbv7em-none-eabi` 汇编
    /// 同一句汇编得到。
    #[test]
    fn encodings_match_clang_reference() {
        let mut u = empty_unit();
        movw(&mut u, Reg(1), 0x1234);
        assert_eq!(halfwords(&u), [0xf241, 0x2134], "movw r1, #0x1234");

        let mut u = empty_unit();
        movt(&mut u, Reg(1), 0x5678);
        assert_eq!(halfwords(&u), [0xf2c5, 0x6178], "movt r1, #0x5678");

        let mut u = empty_unit();
        dp3(&mut u, 0xeb00, Reg(1), Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0xeb01, 0x0102], "add.w r1, r1, r2");

        let mut u = empty_unit();
        dp3(&mut u, 0xeba0, Reg(1), Reg(2), Reg(3));
        assert_eq!(halfwords(&u), [0xeba2, 0x0103]);

        let mut u = empty_unit();
        muldiv(&mut u, 0b0000, Reg(1), Reg(2), Reg(3));
        assert_eq!(halfwords(&u), [0xfb02, 0xf103], "mul r1, r2, r3");

        let mut u = empty_unit();
        muldiv(&mut u, 0b1001, Reg(1), Reg(2), Reg(3));
        assert_eq!(halfwords(&u), [0xfb92, 0xf1f3], "sdiv r1, r2, r3");

        let mut u = empty_unit();
        mls(&mut u, Reg(1), Reg(2), Reg(3), Reg(4));
        assert_eq!(halfwords(&u), [0xfb02, 0x4113], "mls r1, r2, r3, r4");

        let mut u = empty_unit();
        shift_reg(&mut u, 0b0000, Reg(1), Reg(2), Reg(3));
        assert_eq!(halfwords(&u), [0xfa02, 0xf103], "lsl.w r1, r2, r3");

        let mut u = empty_unit();
        shift_reg(&mut u, 0b0010, Reg(1), Reg(2), Reg(3));
        assert_eq!(halfwords(&u), [0xfa42, 0xf103], "asr.w r1, r2, r3");

        let mut u = empty_unit();
        un_op(&mut u, 0xea6f, Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0xea6f, 0x0102], "mvn.w r1, r2");

        let mut u = empty_unit();
        extend16(&mut u, 0b00, Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0x4611, 0xb251], "sxtb r1, r2");
        let mut u = empty_unit();
        extend16(&mut u, 0b00, Reg(1), Reg(1));
        assert_eq!(halfwords(&u), [0xb251], "sxtb r1, r1");

        let mut u = empty_unit();
        extend_any(&mut u, 0b00, Reg(0), Reg(9));
        assert_eq!(halfwords(&u), [0xfa4f, 0xf089], "sxtb.w r0, r9");
        let mut u = empty_unit();
        extend_any(&mut u, 0b11, Reg(9), Reg(9));
        assert_eq!(halfwords(&u), [0xfa1f, 0xf989], "uxth.w r9, r9");

        let mut u = empty_unit();
        mov_low(&mut u, Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0x4611], "mov r1, r2");

        let mut u = empty_unit();
        cmp_imm8(&mut u, Reg(4), 0);
        assert_eq!(halfwords(&u), [0x2c00], "cmp r4, #0");
        cmp_imm8(&mut u, Reg(8), 0);
        assert_eq!(halfwords(&u), [0x2c00, 0xf1b8, 0x0f00], "cmp.w r8, #0");

        let mut u = empty_unit();
        cmp_low(&mut u, Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0x4291], "cmp r1, r2");

        let mut u = empty_unit();
        mov_imm8(&mut u, Reg(1), 5);
        assert_eq!(halfwords(&u), [0x2105], "mov r1, #5");

        let mut u = empty_unit();
        push_regs(&mut u, &[Reg(4), Reg(5), Reg(6), Reg(7), Reg(14)]);
        assert_eq!(halfwords(&u), [0xb5f0], "push {{r4-r7, lr}}");

        let mut u = empty_unit();
        pop_regs(&mut u, &[Reg(4), Reg(5), Reg(6), Reg(7)]);
        assert_eq!(halfwords(&u), [0xbcf0], "pop {{r4-r7}}");
    }

    /// 溢出与内存访问的编码对照 clang。
    #[test]
    fn spill_and_memory_encodings() {
        let mut u = empty_unit();
        ldr_sp_imm(&mut u, Reg(9), 8);
        assert_eq!(halfwords(&u), [0xf8dd, 0x9008], "ldr.w r9, [sp, #8]");

        let mut u = empty_unit();
        str_sp_imm(&mut u, Reg(10), 12);
        assert_eq!(halfwords(&u), [0xf8cd, 0xa00c], "str.w r10, [sp, #12]");

        let mut u = empty_unit();
        sub_sp_imm(&mut u, 16);
        assert_eq!(halfwords(&u), [0xb084], "sub sp, #16");

        let mut u = empty_unit();
        sub_sp_imm(&mut u, 1024);
        // >508 走 `movw r12, #imm` + `sub.w sp, sp, r12` 的回退序列。
        assert_eq!(
            halfwords(&u),
            [0xf240, 0x4c00, 0xebad, 0x0d0c],
            "movw r12, #0x400; sub.w sp, sp, r12"
        );

        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg(0), Reg(1), NumTy::Int(8));
        assert_eq!(halfwords(&u), [0xf991, 0x0000], "ldrsb.w r0, [r1]");

        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg(9), Reg(1), NumTy::Uint(8));
        assert_eq!(halfwords(&u), [0xf891, 0x9000], "ldrb.w r9, [r1]");

        let mut u = empty_unit();
        emit_store_mem(&mut u, Reg(1), Reg(9), NumTy::Uint(8));
        assert_eq!(halfwords(&u), [0xf881, 0x9000], "strb.w r9, [r1]");

        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg(0), Reg(1), NumTy::Int(32));
        assert_eq!(halfwords(&u), [0xf8d1, 0x0000], "ldr.w r0, [r1]");

        let mut u = empty_unit();
        adds(&mut u, Reg(9), Reg(10), Reg(11));
        assert_eq!(halfwords(&u), [0xeb1a, 0x090b], "adds.w r9, r10, r11");
        let mut u = empty_unit();
        adcs(&mut u, Reg(9), Reg(10), Reg(11));
        assert_eq!(halfwords(&u), [0xeb5a, 0x090b], "adcs.w r9, r10, r11");
        let mut u = empty_unit();
        sbcs(&mut u, Reg(9), Reg(10), Reg(11));
        assert_eq!(halfwords(&u), [0xeb7a, 0x090b], "sbcs.w r9, r10, r11");
        let mut u = empty_unit();
        umull(&mut u, Reg(9), Reg(10), Reg(11), Reg(12));
        assert_eq!(halfwords(&u), [0xfbab, 0x9a0c], "umull r9, r10, r11, r12");
    }

    /// VFP 编码对照 clang（Thumb-2 与 A32 的 VFP 编码一致）。
    #[test]
    fn vfp_encodings_match_clang() {
        let mut u = empty_unit();
        emit_vfp_arith(&mut u, false, VfpArith::Add);
        assert_eq!(halfwords(&u), [0xee30, 0x0b01], "vadd.f64 d0,d0,d1");
        let mut u = empty_unit();
        emit_vfp_arith(&mut u, false, VfpArith::Sub);
        assert_eq!(halfwords(&u), [0xee30, 0x0b41], "vsub.f64 d0,d0,d1");
        let mut u = empty_unit();
        emit_vfp_arith(&mut u, false, VfpArith::Mul);
        assert_eq!(halfwords(&u), [0xee20, 0x0b01], "vmul.f64 d0,d0,d1");
        let mut u = empty_unit();
        emit_vfp_arith(&mut u, false, VfpArith::Div);
        assert_eq!(halfwords(&u), [0xee80, 0x0b01], "vdiv.f64 d0,d0,d1");
        let mut u = empty_unit();
        emit_vfp_arith(&mut u, true, VfpArith::Add);
        assert_eq!(halfwords(&u), [0xee30, 0x0a20], "vadd.f32 s0,s0,s1");
        let mut u = empty_unit();
        emit_vfp_arith(&mut u, true, VfpArith::Mul);
        assert_eq!(halfwords(&u), [0xee20, 0x0a20], "vmul.f32 s0,s0,s1");

        let mut u = empty_unit();
        emit_vldr(&mut u, false, 0, 1);
        assert_eq!(halfwords(&u), [0xed9d, 0x0b02], "vldr d0,[sp,#8]");
        let mut u = empty_unit();
        emit_vstr(&mut u, true, 0, 1);
        assert_eq!(halfwords(&u), [0xed8d, 0x0a02], "vstr s0,[sp,#8]");

        let mut u = empty_unit();
        emit_vcvt_int_to_fp(&mut u, false, true, Reg(0));
        assert_eq!(halfwords(&u), [0xee00, 0x0a10, 0xeeb8, 0x0bc0]);
    }

    /// IT 块：clang 的内置汇编器不接受，按 ARM ARM 的固定格式验证。
    #[test]
    fn it_block_encoding() {
        for (cond, expected) in [(0u16, 0xbf08u16), (1, 0xbf18), (10, 0xbfa8), (12, 0xbfc8)] {
            let mut u = empty_unit();
            it(&mut u, cond);
            assert_eq!(halfwords(&u), [expected], "it cond={cond}");
        }
    }

    #[test]
    fn conditional_mov_matches_reference_shape() {
        let mut u = empty_unit();
        it(&mut u, 1);
        mov_imm8(&mut u, Reg(0), 0);
        assert_eq!(halfwords(&u), [0xbf18, 0x2000]);
    }

    #[test]
    fn all_emitted_instructions_are_aligned() {
        let mut u = empty_unit();
        for _ in 0..5 {
            dp3(&mut u, 0xeb00, Reg(1), Reg(2), Reg(3));
        }
        assert_eq!(u.code.len() % 2, 0, "代码长度必须是偶数（Thumb 指令对齐）");
        assert_eq!(u.code.len(), 20, "5 条 32 位指令");
    }

    #[test]
    fn mov_imm32_uses_two_instructions_for_large_constants() {
        let mut u = empty_unit();
        mov_imm32(&mut u, Reg(1), 0x1234_5678);
        assert_eq!(
            halfwords(&u),
            [0xf245, 0x6178, 0xf2c1, 0x2134],
            "movw 取低 16 位、movt 取高 16 位"
        );

        let mut u = empty_unit();
        mov_imm32(&mut u, Reg(1), 0x42);
        assert_eq!(halfwords(&u), [0xf240, 0x0142], "小常量只用一条 movw");
    }

    /// 宽移位用到的算术/移位/分支编码，逐条对照 `clang --target=thumbv7em-none-eabi`。
    #[test]
    fn wide_shift_helper_encodings_match_gnu_assembler_output() {
        let mut u = empty_unit();
        sub_imm8(&mut u, Reg(12), Reg(11), 32);
        assert_eq!(halfwords(&u), [0xf1ab, 0x0c20], "sub.w r12, r11, #32");

        let mut u = empty_unit();
        rsb_imm8(&mut u, Reg(12), Reg(11), 32);
        assert_eq!(halfwords(&u), [0xf1cb, 0x0c20], "rsb.w r12, r11, #32");

        let mut u = empty_unit();
        orr(&mut u, Reg(10), Reg(10), Reg(11));
        assert_eq!(halfwords(&u), [0xea4a, 0x0a0b], "orr.w r10, r10, r11");

        let mut u = empty_unit();
        shift_reg(&mut u, 0b0001, Reg(12), Reg(9), Reg(12));
        assert_eq!(halfwords(&u), [0xfa29, 0xfc0c], "lsr.w r12, r9, r12");

        let mut u = empty_unit();
        emit_b_cond(&mut u, 0b0011, "x");
        assert_eq!(halfwords(&u), [0xd300], "blo 待回填");
    }

    /// 函数内局部标签 + T1 条件分支的重定位（`b<cond>` 立即数落在 [7:0]）。
    #[test]
    fn local_label_conditional_branch_resolves() {
        let mut u = empty_unit();
        let end = fresh_label();
        emit_b_cond(&mut u, 0b0001, &end); // 偏移 0：bne end
        mov_imm8(&mut u, Reg(0), 0); // 偏移 2
        mov_imm8(&mut u, Reg(0), 0); // 偏移 4
        mov_imm8(&mut u, Reg(0), 0); // 偏移 6
        define_label(&mut u, &end); // 偏移 8
        crate::codegen::objfile::apply_relocations(&mut u, 0, 0);
        let hw = u16::from_le_bytes([u.code[0], u.code[1]]);
        // pc = 0 + 4，delta = 8 - 4 = 4，off = 2 → 0xd100 | 2
        assert_eq!(hw, 0xd102, "bne 到偏移 8");
    }

    /// i64 变量移位的整段发射序列（分 `cnt < 32` / `cnt >= 32` 两段）。
    #[test]
    fn wide_shift_emits_split_sequence() {
        let setup = || {
            WIDE_VALUES.with(|w| {
                let mut w = w.borrow_mut();
                w.insert(1);
                w.insert(2);
                w.insert(3);
            });
            SPILL_SLOTS.with(|m| {
                let mut m = m.borrow_mut();
                m.insert(1, 0);
                m.insert(2, 1);
                m.insert(3, 2);
            });
        };

        setup();
        let mut u = empty_unit();
        emit_shift_wide(&mut u, SsaValue(3), SsaBinaryOp::Shl, SsaValue(1), SsaValue(2));
        assert_eq!(
            halfwords(&u),
            [
                0xf8dd, 0x9000, // ldr.w r9, [sp, #0]    ; lo
                0xf8dd, 0xa004, // ldr.w r10, [sp, #4]   ; hi
                0xf8dd, 0xb008, // ldr.w r11, [sp, #8]   ; cnt
                0xf8dd, 0xc00c, // ldr.w r12, [sp, #12]
                0xf1bb, 0x0f20, // cmp.w r11, #32
                0xd300, // blo <cnt<32>
                0xf1ab, 0x0b20, // sub.w r11, r11, #32
                0xfa09, 0xf90b, // lsl.w r9, r9, r11
                0xf04f, 0x0a00, // mov.w r10, #0
                0xf000, 0xbffe, // b <end>
                0xf1cb, 0x0c20, // rsb.w r12, r11, #32
                0xfa29, 0xfc0c, // lsr.w r12, r9, r12
                0xfa0a, 0xfa0b, // lsl.w r10, r10, r11
                0xfa09, 0xf90b, // lsl.w r9, r9, r11
                0xea4a, 0x0a0c, // orr.w r10, r10, r12
                0xf8cd, 0x9010, // str.w r9, [sp, #16]
                0xf8cd, 0xa014, // str.w r10, [sp, #20]
            ],
            "i64 shl"
        );

        setup();
        let mut u = empty_unit();
        emit_shift_wide(&mut u, SsaValue(3), SsaBinaryOp::Shr, SsaValue(1), SsaValue(2));
        assert_eq!(
            halfwords(&u)[..13],
            [
                0xf8dd, 0x9000, // lo
                0xf8dd, 0xa004, // hi
                0xf8dd, 0xb008, // cnt
                0xf8dd, 0xc00c, //
                0xf1bb, 0x0f20, // cmp.w r11, #32
                0xd300, // blo
                0xf1ab, 0x0b20, // sub.w r11, r11, #32
            ],
            "i64 shr 头部"
        );
        assert_eq!(
            halfwords(&u)[13..],
            [
                0xfa4a, 0xf90b, // asr.w r9, r10, r11
                0xea4f, 0x7aea, // asr.w r10, r10, #31
                0xf000, 0xbffe, // b <end>
                0xf1cb, 0x0c20, // rsb.w r12, r11, #32
                0xfa0a, 0xfc0c, // lsl.w r12, r10, r12
                0xfa4a, 0xfa0b, // asr.w r10, r10, r11
                0xfa29, 0xf90b, // lsr.w r9, r9, r11
                0xea49, 0x090c, // orr.w r9, r9, r12
                0xf8cd, 0x9010, // str.w r9, [sp, #16]
                0xf8cd, 0xa014, // str.w r10, [sp, #20]
            ],
            "i64 shr 尾部"
        );
    }
}
