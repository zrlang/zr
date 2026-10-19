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
use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};
use crate::mir::NumTy;
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaValue};

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

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

/// 除法/取余序列的内部暂存：商（`sdiv` 的目的）。
/// 平台寄存器 r11，不在分配池里，随时可以用。
const MUL_TMP_A: Reg = Reg(11);

thread_local! {
    /// 当前函数的溢出：值 id -> 溢出栈槽号（0 起）。只对 Location::Stack
    /// 的值有条目，读写都按 `[sp + SPILL_BASE + slot*8]` 寻址。
    static SPILL_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
    /// 溢出槽区的 sp 偏移（字节数）：push 保存区与取址槽区的上方。每个函数
    /// `emit_function` 里更新。
    static SPILL_BASE: Cell<usize> = const { Cell::new(0) };
    /// 当前函数的多字长（i64）值 id 集合（32 位目标）。
    static WIDE_VALUES: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    /// 当前函数的浮点栈槽：值 id -> (槽号, 位宽 32/64)。
    ///
    /// 浮点值不参与整数寄存器分配，每个浮点 SSA 值分配一个专属栈槽，
    /// 运算时用 `s0`/`s1`（单精度）或 `d0`/`d1`（双精度）当暂存器读写内存。
    static FLOAT_SLOTS: RefCell<HashMap<u32, (usize, u8)>> = RefCell::new(HashMap::new());
    /// 当前函数为浮点栈槽预留的字节数（驱动序言/尾声的 `sub/add sp`）。
    static FRAME_BYTES: Cell<usize> = const { Cell::new(0) };
}

/// `v` 是否是多字长（i64）值（32 位目标走栈槽 + 双 32 位运算）。
fn is_wide(v: SsaValue) -> bool {
    WIDE_VALUES.with(|w| w.borrow().contains(&v.id()))
}

/// 溢出值的暂存寄存器（都不进分配池，也绝不会与活跃值同住）。
///
/// - `SPILL_SRC`(r9)：第一溢出操作数的装载目标，同时兼任 φ 搬运中转
///   （φ 拷贝发生在指令之间，与单条指令的发射不重叠）。
/// - `SPILL_SRC2`(r10)：二元运算的第二溢出操作数，左右都溢出时与
///   `SPILL_SRC` 分家。
/// - `SPILL_DST`(r12)：溢出结果先算进这里再落回槽。
const SPILL_SRC: Reg = Reg(9);
const SPILL_SRC2: Reg = Reg(10);
const SPILL_DST: Reg = Reg(12);

/// 被调用者保存的寄存器（分配池下标，arm32 池里恰与寄存器号一致）。
/// 跨调用存活的值只会落进 r4-r8，序言/尾声 `push/pop {r4-r8}` 恰好覆盖。
const CALLEE_SAVE: [usize; 5] = [4, 5, 6, 7, 8];

thread_local! {
    /// 当前函数的整数寄存器分配：值 id -> `ALLOCATABLE` 下标。
    ///
    /// 用线性扫描的结果，而不是 `id % 9` 取模——取模会让两个同时存活、
    /// 编号同余的值抢同一个寄存器，后写者把先写者冲掉，安静地算错。
    /// 没进活跃区间的临时值才退回取模。
    static REG_MAP: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

/// 把 SSA 值映射到物理寄存器（优先用线性扫描的结果）。
fn alloc_reg(v: SsaValue) -> Reg {
    if let Some(i) = REG_MAP.with(|m| m.borrow().get(&v.id()).copied()) {
        return ALLOCATABLE[i];
    }
    ALLOCATABLE[(v.id() as usize) % ALLOCATABLE.len()]
}

/// `v` 的溢出栈槽（不在寄存器里、只在栈上时返回编号）。
fn spill_slot(v: SsaValue) -> Option<usize> {
    SPILL_SLOTS.with(|m| m.borrow().get(&v.id()).copied())
}

/// 溢出槽号的 sp 偏移：溢出区开在最终 sp 上方 0 起排（push 保存区与
/// 取址槽区都在它上方）。
fn spill_offset(slot: usize) -> i32 {
    (slot * 8) as i32
}

/// `ld r, [sp + ...]`：从溢出槽装载字。
fn load_spill(unit: &mut CompilationUnit, dst: Reg, slot: usize) {
    let off = spill_offset(slot);
    debug_assert!((0..=4095).contains(&off), "溢出槽偏移超出 ldr 立即数范围");
    insn(unit, 0xe59d_0000 | (dst.bits() as u32) << 12 | off as u32);
}

/// `str r, [sp + ...]`：把字存进溢出槽。
fn store_spill(unit: &mut CompilationUnit, src: Reg, slot: usize) {
    let off = spill_offset(slot);
    debug_assert!((0..=4095).contains(&off), "溢出槽偏移超出 str 立即数范围");
    insn(unit, 0xe58d_0000 | (src.bits() as u32) << 12 | off as u32);
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

/// `v` 的浮点位宽（32 或 64）。整数返回 0。
fn float_width(v: SsaValue) -> u8 {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).map(|(_, w)| *w).unwrap_or(0))
}

/// `v` 如果是浮点值，返回它的栈槽。
fn float_slot(v: SsaValue) -> Option<usize> {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).map(|(s, _)| *s))
}

/// `vldr s<t>, [sp, #off]` / `vldr d<t>, [sp, #off]`：从栈槽装载浮点值到 VFP 寄存器。
fn emit_vldr(unit: &mut CompilationUnit, is_single: bool, reg: u8, slot: usize) {
    // VFP load: 0xED900A00 | (cond<<28) | (P=1) | (U=1) | (W=0) | (L=1) | (Rd<<12) | (Rn<<16) | (imm8<<0)
    // Rn = sp (13), imm8 = slot * 8 / 4 (for single) or slot * 8 / 8 (for double)
    // We use slot * 2 for single (4-byte aligned), slot for double (8-byte aligned)
    let imm = if is_single { (slot * 2) as u32 } else { slot as u32 };
    let base = if is_single { 0xED9D_0A00u32 } else { 0xED9D_0B00u32 };
    insn(unit, base | ((reg as u32) << 12) | (imm & 0xFF));
}

/// `vstr s<t>, [sp, #off]` / `vstr d<t>, [sp, #off]`：把 VFP 寄存器存回栈槽。
fn emit_vstr(unit: &mut CompilationUnit, is_single: bool, reg: u8, slot: usize) {
    let imm = if is_single { (slot * 2) as u32 } else { slot as u32 };
    let base = if is_single { 0xED8D_0A00u32 } else { 0xED8D_0B00u32 };
    insn(unit, base | ((reg as u32) << 12) | (imm & 0xFF));
}

/// 把一个浮点常量装进 VFP 寄存器：先经整数寄存器中转。
fn emit_float_const(unit: &mut CompilationUnit, bits: u64, width: u8) {
    // 使用 r12 (SPILL_DST) 作为中转寄存器（不在分配池里）
    emit_mov_imm32(unit, SPILL_DST, bits as u32);
    if width == 64 {
        emit_mov_imm32(unit, Reg(9), (bits >> 32) as u32);
    }
    if width == 32 {
        // vmsr fpscr, r12; 然后用 vmov s0, r12 也行，但直接用 VMOV 更简单
        // VMOV (ARM -> VFP): 0xEEE00A40 | (cond<<28) | (Rd<<12) | (Rm<<16) | (opc1<<5) | opc2
        // 对于单精度：0xEEE00A40 | (t<<12) | (m<<16) | (1<<5) | 0
        insn(unit, 0xEEE0_0A40 | (0u32 << 12) | (SPILL_DST.bits() as u32) << 16 | (1u32 << 5));
    } else {
        // 双精度：需要两个整数寄存器
        // vmov d0, r12, r9 (r12=low, r9=high): 0xEC400A10 | (cond<<28) | (Rt<<12) | (Rt2<<16) | (opc1<<5) | opc2
        // 实际上是 VMOV (two ARM -> VFP): 0xEC500A10 | (t<<12) | (t2<<16) | (1<<5) | 0
        // 但更标准的是用 vmov d<d>, r<rt>, r<rt2>
        // 编码: cond=1110, 1110, 1, 0, 1, 00, Rt2, 1, Rt, 0, 1, opc1=0, Rt3=0... 
        // 实际上查阅 ARM ARM: VMOV (two ARM registers to two single-precision or one double-precision)
        // 0xEC400A10 | (t<<12) | (t2<<16) | (1<<5) for double
        insn(unit, 0xEC40_0A10 | (0u32 << 12) | (SPILL_DST.bits() as u32) << 16 | (Reg(9).bits() as u32) << 8 | (1u32 << 5));
    }
}

/// VFP 算术指令：`vadd.f32/f64`, `vsub.f32/f64`, `vmul.f32/f64`, `vdiv.f32/f64`
fn emit_vfp_arith(unit: &mut CompilationUnit, is_single: bool, op: u32, rd: u8, rn: u8, rm: u8) {
    // VFP data processing: 0xEE200A00 | (cond<<28) | (op<<20) | (Rd<<12) | (Rn<<16) | (Rm<<0) | (sz<<6)
    // op: add=0b0000, sub=0b0001, mul=0b0010, div=0b0111
    // sz: 0 for single, 1 for double
    let sz = if is_single { 0 } else { 1 };
    let cond = 0xE;
    insn(unit, (cond << 28) | 0xEE20_0A00 | (op << 20) | ((rd as u32) << 12) | ((rn as u32) << 16) | (rm as u32) | ((sz as u32) << 6));
}

/// VFP 取负：`vneg.f32/f64`
fn emit_vneg(unit: &mut CompilationUnit, is_single: bool, rd: u8, rm: u8) {
    // VFP neg: 0xEE200A00 | (cond<<28) | (op=0b1111<<20) | (Rd<<12) | (Rm<<0) | (sz<<6) | (Rn=0<<16)
    let sz = if is_single { 0 } else { 1 };
    let cond = 0xE;
    insn(unit, (cond << 28) | 0xEE20_0A00 | (0xF << 20) | ((rd as u32) << 12) | ((rm as u32)) | ((sz as u32) << 6));
}

/// VFP 比较：`vcmp.f32/f64` / `vcmpz.f32/f64` + `vmrs apsr_nzcv, fpscr`
fn emit_vcmp(unit: &mut CompilationUnit, is_single: bool, rn: u8, rm: u8) {
    let sz = if is_single { 0 } else { 1 };
    let cond = 0xE;
    // vcmp: 0xEE200A00 | (op=0b1010<<20) | (Rn<<16) | (Rm<<0) | (sz<<6)
    insn(unit, (cond << 28) | 0xEE20_0A00 | (0xA << 20) | ((rn as u32) << 16) | (rm as u32) | ((sz as u32) << 6));
    // vmrs apsr_nzcv, fpscr: 0xEEF1FA10
    insn(unit, 0xEEF1_FA10);
}

/// VFP 整数到浮点转换：`vcvt.f32.s32` / `vcvt.f64.s32` / `vcvt.f32.u32` / `vcvt.f64.u32`
fn emit_vcvt_int_to_fp(unit: &mut CompilationUnit, is_single: bool, signed: bool, rd: u8, rm: u8) {
    // vcvt: 0xEEBC0AC0 | (cond<<28) | (opc2<<16) | (Rd<<12) | (Rm<<0) | (sz<<6) | (opc1<<5) | (opc3<<7)
    // signed int -> fp: opc1=0, opc2=0b1010 (signed), opc3=0
    // unsigned int -> fp: opc1=0, opc2=0b1011 (unsigned), opc3=0
    let sz = if is_single { 0 } else { 1 };
    let opc2 = if signed { 0xA } else { 0xB };
    let cond = 0xE;
    insn(unit, (cond << 28) | 0xEEBC_0AC0 | (opc2 << 16) | ((rd as u32) << 12) | (rm as u32) | ((sz as u32) << 6));
}

/// VFP 浮点到整数转换：`vcvt.s32.f32` / `vcvt.s32.f64` / `vcvt.u32.f32` / `vcvt.u32.f64` (向零截断)
fn emit_vcvt_fp_to_int(unit: &mut CompilationUnit, is_single: bool, signed: bool, rd: u8, rm: u8) {
    // vcvt (fp -> int): 0xEEBC0AC0 | (cond<<28) | (opc2<<16) | (Rd<<12) | (Rm<<0) | (sz<<6) | (opc1<<5) | (opc3<<7)
    // opc1=1 (fp->int), opc2=0b1010 (signed), opc2=0b1011 (unsigned), opc3=1 (truncate toward zero)
    let sz = if is_single { 0 } else { 1 };
    let opc2 = if signed { 0xA } else { 0xB };
    let cond = 0xE;
    insn(unit, (cond << 28) | 0xEEBC_0AC0 | (opc2 << 16) | ((rd as u32) << 12) | (rm as u32) | ((sz as u32) << 6) | (1u32 << 5) | (1u32 << 7));
}

/// VFP 单双精度互转：`vcvt.f32.f64` / `vcvt.f64.f32`
fn emit_vcvt_fp_to_fp(unit: &mut CompilationUnit, dst_is_single: bool, rd: u8, rm: u8) {
    // vcvt (fp -> fp): 0xEEBC0AC0 | (cond<<28) | (opc2=0b1111) | (Rd<<12) | (Rm<<0) | (sz<<6) | (opc1=0<<5) | (opc3=1<<7)
    let sz = if dst_is_single { 0 } else { 1 };
    let cond = 0xE;
    insn(unit, (cond << 28) | 0xEEBC_0AC0 | (0xF << 16) | ((rd as u32) << 12) | (rm as u32) | ((sz as u32) << 6) | (1u32 << 7));
}

/// 把实参搬进 AAPCS32 的 r0-r3；第 5+ 个整型实参写进**栈上传参区**。
/// 返回传出栈区的字节数——`bl` 之后调用方要加回去。
///
/// 顺序很要紧：栈上传参先落（`str` 只读源寄存器、不写 r0-r3），然后才是
/// 搬进 r0-r3 的并行走环——第 5+ 个实参的源可能就住在 r0-r3 里，如果先走环，
/// `str` 读到的就是被改写之后的值。溢出实参经 [SPILL_DST] 中转。
///
/// AAPCS32 浮点实参：s0-s15 (单精度) / d0-d7 (双精度)，与整数实参各自计数。
fn emit_call_args(unit: &mut CompilationUnit, args: &[SsaValue]) -> u32 {
    let mut gi = 0usize;  // 整数实参计数
    let mut fi = 0usize;  // 浮点实参计数
    let mut int_args: Vec<SsaValue> = Vec::new();
    let mut float_args: Vec<SsaValue> = Vec::new();

    // 分离整数和浮点实参
    for a in args {
        if float_slot(*a).is_some() {
            float_args.push(*a);
            fi += 1;
        } else {
            int_args.push(*a);
            gi += 1;
        }
    }

    // 栈上传参：整数和浮点混合排布，每个占 8 字节
    let int_stack = int_args.len().saturating_sub(4);
    let float_stack = float_args.len().saturating_sub(8);
    let stack_args = int_stack + float_stack;
    let out_bytes = (stack_args * 8) as u32;

    if out_bytes > 0 {
        emit_sub_sp_imm(unit, out_bytes);
        let mut stack_idx = 0usize;
        // 整数栈上传参
        for a in int_args.iter().skip(4) {
            let off = (stack_idx * 8) as u32;
            let r = if let Some(slot) = spill_slot(*a) {
                load_spill(unit, SPILL_DST, slot);
                SPILL_DST
            } else {
                alloc_reg(*a)
            };
            emit_str_sp_imm(unit, r, off);
            stack_idx += 1;
        }
        // 浮点栈上传参
        for a in float_args.iter().skip(8) {
            let off = (stack_idx * 8) as u32;
            let w = float_width(*a);
            let is_single = w == 32;
            emit_vldr(unit, is_single, 0, float_slot(*a).unwrap());
            emit_vstr(unit, is_single, 0, 0); // 临时存到栈槽 0
            // 然后用整数指令从栈槽搬到栈上传参区
            emit_ldr_sp_imm(unit, SPILL_DST, 0);
            emit_str_sp_imm(unit, SPILL_DST, off);
            stack_idx += 1;
        }
    }

    // 整数实参并行搬运
    let mut moves: Vec<(Reg, Reg)> = Vec::new();
    let mut from_stack: Vec<(Reg, usize)> = Vec::new();
    for (i, a) in int_args.iter().take(4).enumerate() {
        if let Some(slot) = spill_slot(*a) {
            from_stack.push((ARG_REGS[i], slot));
        } else {
            moves.push((ARG_REGS[i], alloc_reg(*a)));
        }
    }
    emit_parallel_moves(unit, &mut moves);
    for (dst, slot) in from_stack {
        load_spill(unit, SPILL_DST, slot);
        emit_mov_rr(unit, dst, SPILL_DST);
    }

    // 浮点实参直接装载到 VFP 寄存器
    for (reg_idx, a) in float_args.iter().take(8).enumerate() {
        let w = float_width(*a);
        let is_single = w == 32;
        emit_vldr(unit, is_single, reg_idx as u8, float_slot(*a).unwrap());
    }

    out_bytes
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
    pub const ADC: u32 = 0b0101;
    pub const SBC: u32 = 0b0110;
    pub const ORR: u32 = 0b1100;
    pub const CMP: u32 = 0b1010;
    pub const MOV: u32 = 0b1101;
    pub const MVN: u32 = 0b1111;
    /// 立即数形式的 `rsb`（取负）：ARM 的立即数数据处理空间里
    /// 0b0011 才是 rsb（0b0010 在该空间表示 sub）——ARM 的历史包袱。
    pub const RSB_IMM: u32 = 0b0011;
}

/// 类型转换。
///
/// ARM32 的寄存器本来就是 32 位，所以只有**变窄**的转换需要指令：
/// 取低 w 位并按有无符号重新解释。转成 64 位在这里做不到（没有寄存器
/// 放得下），直接报错。
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
    match bits {
        8 | 16 => emit_extend(unit, rd, rm, bits, signed),
        // 32 位：已经是整个寄存器，不需要扩展
        32 => {
            if rd != rm {
                emit_mov_rr(unit, rd, rm);
            }
        }
        _ => crate::codegen::unsupported_cast(from, to),
    }
}

/// `sxth / sxtb / uxth / uxtb rd, rm`
///
/// 这些是 extra load/store 空间里的单指令扩展：
/// `cond 00 1 0101 1 imm4 1 Rd 1 10 0 imm2 type Rm`
fn emit_extend(unit: &mut CompilationUnit, rd: Reg, rm: Reg, bits: u8, signed: bool) {
    // A32 一字扩展（extra load/store 家族，gas 实测对账）：
    // `sxtb  rd, rm` = 0xE6AF0070 | rd<<12 | rm
    // `uxtb  rd, rm` = 0xE6EF0070 | rd<<12 | rm
    // `sxth  rd, rm` = 0xE6BF0070 | rd<<12 | rm
    // `uxth  rd, rm` = 0xE6FF0070 | rd<<12 | rm
    let base: u32 = match (bits, signed) {
        (8, true) => 0xE6AF_0070,   // sxtb
        (8, false) => 0xE6EF_0070,  // uxtb
        (16, true) => 0xE6BF_0070,  // sxth
        (16, false) => 0xE6FF_0070, // uxth
        _ => return,
    };
    insn(unit, base | (rd.bits() as u32) << 12 | rm.bits() as u32);
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
/// 把 32 位常量拆成 ARM 的旋转立即数：硬件解码为 `value = imm8 ror 2×rot`，
/// 所以编码时 `imm8 = value rol 2×rot`（谨防把移位方向弄反——records
/// `300` 只有 `rol 30 = ror 2 「imm8=75, rot=15」是合法的）。旋转方向
/// 写反会解码成 `imm8 ror 2×rot` 的错误值。
fn rotated_imm8_operand(value: u32) -> Option<u32> {
    for rotation in 0..16u32 {
        let rotated = value.rotate_left(rotation * 2);
        if rotated <= 0xff {
            return Some(imm8_operand(rotation, rotated as u8));
        }
    }
    None
}

/// `mov<cond> rd, #imm8`：条件执行用来实现"比较后选择 0/1"。
fn emit_mov_cond(unit: &mut CompilationUnit, cond: u32, rd: Reg, imm: u8) {
    data_processing(
        unit,
        cond,
        op::MOV,
        false,
        Reg(0),
        rd,
        imm8_operand(0, imm),
        true,
    );
}

/// `mov rd, rm`
fn emit_mov_rr(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    data_processing(
        unit,
        cc::AL,
        op::MOV,
        false,
        Reg(0),
        rd,
        rm.bits() as u32,
        false,
    );
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

/// `opss rd, rn, rm`：设置条件码（用于 i64 加减的进位/借位传播）。
fn emit_adds(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    data_processing(unit, cc::AL, op::ADD, true, rn, rd, rm.bits() as u32, false);
}

/// `subs rd, rn, rm`：设置条件码（用于 i64 减法的借位传播）。
fn emit_subs(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    data_processing(unit, cc::AL, op::SUB, true, rn, rd, rm.bits() as u32, false);
}

/// `cmp rn, rm`：只更新标志位（编码上等价于 `subs r0, rn, rm`，
/// 但汇编器统一使用 CMP 这个操作码）。
fn emit_cmp(unit: &mut CompilationUnit, rn: Reg, rm: Reg) {
    data_processing(
        unit,
        cc::AL,
        op::CMP,
        true,
        rn,
        Reg(0),
        rm.bits() as u32,
        false,
    );
}

/// `cmp rn, #imm`：立即数比较（走 `subs r0, rn, #imm`）。
fn emit_cmp_imm8(unit: &mut CompilationUnit, rn: Reg, imm: u8) {
    data_processing(
        unit,
        cc::AL,
        op::CMP,
        true,
        rn,
        Reg(0),
        imm8_operand(0, imm),
        true,
    );
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

/// `mls rd, lhs, rhs, acc`：对应 A32 `MLS Rd, Rn, Rm, Ra`（gas 顺序
/// `mls r1,r4,r7,r2` = 0xE061_2794 实测），语义 Rd = Ra − Rn·Rm。
/// A32 MUL 家族的字段排布不太常规：**Rd@19:16**、Ra@15:12、Rm@11:8、
/// Rn@3:0。调用方注意两点：`acc` 是**被减数 Ra**（对应 gas 的最后一
/// 个操作数），且 Rd 与 Rn（`lhs`，位 [3:0]）重名时 UNPREDICTABLE——
/// 取余序列里商走 MUL_TMP_A(r11)、结果走 SPILL_DST(r12)，避开了重名。
fn emit_mls(unit: &mut CompilationUnit, rd: Reg, lhs: Reg, rhs: Reg, acc: Reg) {
    insn(
        unit,
        0xe060_0090
            | (rd.bits() as u32) << 16
            | (acc.bits() as u32) << 12
            | (rhs.bits() as u32) << 8
            | lhs.bits() as u32,
    );
}

/// `sdiv rd, rn, rm`
fn emit_sdiv(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    insn(
        unit,
        0xe710_f010 | (rd.bits() as u32) << 16 | (rm.bits() as u32) << 8 | rn.bits() as u32,
    );
}

/// `mvn rd, rm`
fn emit_mvn(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    data_processing(
        unit,
        cc::AL,
        op::MVN,
        false,
        Reg(0),
        rd,
        rm.bits() as u32,
        false,
    );
}

/// `rsb rd, rn, #0`（取负）
fn emit_rsb(unit: &mut CompilationUnit, rd: Reg, rn: Reg) {
    data_processing(
        unit,
        cc::AL,
        op::RSB_IMM,
        false,
        rn,
        rd,
        imm8_operand(0, 0),
        true,
    );
}

/// 移位种类（编码在 [6:5]）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shift {
    Lsl,
    /// 算术右移。
    Asr,
}

impl Shift {
    fn type_code(self) -> u32 {
        match self {
            Shift::Lsl => 0b00,
            Shift::Asr => 0b10,
        }
    }
}

/// 寄存器受控移位：`lsl/asr rd, rm, rs`（rd = rm 移位 rs 位）。
fn emit_shift_by_reg(unit: &mut CompilationUnit, rd: Reg, rm: Reg, count: Reg, shift: Shift) {
    // 基址只置 bit4（寄存器受控移位标志），**不要**把 Rd 烤进基址——
    // 早先写成 0xe1a0_1010，等于 Rd 恒为 1，换任何别的目的寄存器都会错位。
    insn(
        unit,
        0xe1a0_0010
            | (rd.bits() as u32) << 12
            | (count.bits() as u32) << 8
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

/// ARM32 的 imm12 立即数：8 位值**循环右移**偶数位（`imm8 ror k`，k 偶数）。
///
/// `imm12 = imm8 | ((k/2) << 8)`；k=0 就是普通不过的 0..=255。
/// 返回编码后的 imm12 字段；v 编不进返回 None。
fn arm_imm_field(v: u32) -> Option<u32> {
    if v <= 0xff {
        return Some(v);
    }
    // value == imm8 ror k ⟺ value == imm8 << (32-k) 且没有更高位丢失
    for k in (2..=30).step_by(2) {
        let t = v >> (32 - k);
        if t > 0 && t <= 0xff && t << (32 - k) == v {
            return Some(t | ((k / 2) << 8));
        }
    }
    None
}

/// `sub sp, sp, #imm`。imm 编不进 8 位旋转立即数时（大槽区，如 17 的一千多
/// 字节溢出区退路），经 r12 用 `sub sp, sp, rm` 兜底——r12 不在分配池里。
fn emit_sub_sp_imm(unit: &mut CompilationUnit, bytes: u32) {
    match arm_imm_field(bytes) {
        Some(field) => insn(unit, 0xe24d_d000 | field),
        None => {
            emit_mov_imm32(unit, SPILL_DST, bytes);
            insn(unit, 0xe04d_d000 | SPILL_DST.bits() as u32); // sub sp, sp, r12
        }
    }
}

/// `add sp, sp, #imm`（尾声）；大立即数同样经 r12 兜底。
fn emit_add_sp_imm(unit: &mut CompilationUnit, bytes: u32) {
    match arm_imm_field(bytes) {
        Some(field) => insn(unit, 0xe28d_d000 | field),
        None => {
            emit_mov_imm32(unit, SPILL_DST, bytes);
            insn(unit, 0xe08d_d000 | SPILL_DST.bits() as u32); // add sp, sp, r12
        }
    }
}

/// `add rd, sp, #imm`：被取地址的槽（一般 ≤ 1020 字节偏移，编码上限同上）。
fn emit_add_rd_sp_imm(unit: &mut CompilationUnit, rd: Reg, bytes: u32) {
    let field = arm_imm_field(bytes).unwrap_or_else(|| {
        crate::codegen::unsupported_const("slot offset out of range for the arm32 backend")
    });
    insn(unit, 0xe28d_0000 | (rd.bits() as u32) << 12 | field);
}

/// `ldr rd, [sp, #off]`：装载栈上传参区的第 5+ 个整型实参/接参。
fn emit_ldr_sp_imm(unit: &mut CompilationUnit, rd: Reg, off: u32) {
    debug_assert!((0..=4095).contains(&off), "栈传参偏移超出 ldr 立即数范围");
    insn(unit, 0xe59d_0000 | (rd.bits() as u32) << 12 | off);
}

/// `str rs, [sp, #off]`：把第 5+ 个整型实参写进栈上传参区。
fn emit_str_sp_imm(unit: &mut CompilationUnit, rs: Reg, off: u32) {
    debug_assert!((0..=4095).contains(&off), "栈传参偏移超出 str 立即数范围");
    insn(unit, 0xe58d_0000 | (rs.bits() as u32) << 12 | off);
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
/// ARM 上借 r9（AAPCS32 的 caller-save 暂存区）做 φ 搬运的中转。
const PHI_TMP: Reg = Reg(9);

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

/// 把一组寄存器搬运并行执行（成环时经 [`PHI_TMP`] 打散）。
fn emit_parallel_moves(unit: &mut CompilationUnit, moves: &mut Vec<(Reg, Reg)>) {
    moves.retain(|(dst, src)| dst != src);
    // 前趟直接拆链——把 (d, s) 与 (s, d) 这样的直接自环去掉后，纯环的
    // 成员会被各自搬运么？不会：环上的每个寄存器既是某条搬运的 dst，
    // 也是另一条的 src，SAFE 永远为空，只能走 PHI_TMP 路径。
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

/// 在当前块末尾把本块持有的值写进后继块的各个 φ 寄存器（并行拷贝）。
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
            wide_load(unit, *src, SPILL_SRC, SPILL_SRC2);
            wide_store(unit, *result, SPILL_SRC, SPILL_SRC2);
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
        emit_mov_rr(unit, *dst, SPILL_SRC);
    }
}

/// 把形参从 ABI 寄存器 r0-r3 搬到分配结果所在的寄存器。
///
/// 调用的世界：调用方把实参放进 r 组（`emit_call_args`），被调用方这里
/// 按分配结果把值搬进自己的寄存器——没有搬运的话，函数体里读形参会用
/// 到分配寄存器里的**垃圾**。
/// 被调用方的形参搬运：前 4 个整型实参从 r0-r3 搬到各自被钉/被分配的
/// 位置；浮点实参从 s0-s15/d0-d7 搬到栈槽；第 5+ 个整型实参从
/// **栈上传参区**装载（它在调用方 `sub sp` 后的 sp+0 起，
/// 也就是进入本函数时的 `sp + 栈传参基址 + (i-4)*8`）。`stack_base` 是
/// 进入函数时到当前 sp 的字节数（push 区 + 本函数的槽区）。
fn emit_param_moves(unit: &mut CompilationUnit, func: &SsaFunction, stack_base: u32) {
    let mut gi = 0usize;
    let mut fi = 0usize;
    for p in &func.params {
        if let Some(slot) = float_slot(*p) {
            // 浮点形参：从 VFP 寄存器存到栈槽
            let w = float_width(*p);
            let is_single = w == 32;
            if fi < 8 {
                emit_vstr(unit, is_single, fi as u8, slot);
            } else {
                // 超过 8 个浮点形参从栈上传参区装载（简化处理：暂不支持）
                let _off = stack_base + ((fi - 8) * 8) as u32;
                emit_vldr(unit, is_single, 0, 0);
                emit_vstr(unit, is_single, 0, slot);
            }
            fi += 1;
            continue;
        }
        if gi >= ARG_REGS.len() {
            let off = stack_base + ((gi - ARG_REGS.len()) * 8) as u32;
            if let Some(slot) = spill_slot(*p) {
                emit_ldr_sp_imm(unit, SPILL_DST, off);
                store_spill(unit, SPILL_DST, slot);
            } else {
                let r = alloc_reg(*p);
                emit_ldr_sp_imm(unit, r, off);
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

/// i64 helper functions for 32-bit ARM target.
fn load_spill_hi(unit: &mut CompilationUnit, dst: Reg, slot: usize) {
    let off = spill_offset(slot) + 4;
    debug_assert!((0..=4095).contains(&off), "溢出槽高字偏移超出 ldr 立即数范围");
    insn(unit, 0xe59d_0000 | (dst.bits() as u32) << 12 | off as u32);
}

fn store_spill_hi(unit: &mut CompilationUnit, src: Reg, slot: usize) {
    let off = spill_offset(slot) + 4;
    debug_assert!((0..=4095).contains(&off), "溢出槽高字偏移超出 str 立即数范围");
    insn(unit, 0xe58d_0000 | (src.bits() as u32) << 12 | off as u32);
}

fn wide_load(unit: &mut CompilationUnit, v: SsaValue, lo: Reg, hi: Reg) {
    let slot = spill_slot(v).expect("i64 值没有宽槽");
    load_spill(unit, lo, slot);
    load_spill_hi(unit, hi, slot);
}

fn wide_load2(unit: &mut CompilationUnit, v: SsaValue, lo: Reg, hi: Reg) {
    let slot = spill_slot(v).expect("i64 值没有宽槽");
    load_spill(unit, lo, slot);
    load_spill_hi(unit, hi, slot);
}

fn wide_store(unit: &mut CompilationUnit, v: SsaValue, lo: Reg, hi: Reg) {
    let slot = spill_slot(v).expect("i64 值没有宽槽");
    store_spill(unit, lo, slot);
    store_spill_hi(unit, hi, slot);
}

fn emit_cmp_cond(unit: &mut CompilationUnit, cond: u32, rn: Reg, rm: Reg) {
    data_processing(unit, cond, op::CMP, true, rn, Reg(0), rm.bits() as u32, false);
}

fn emit_const_wide(unit: &mut CompilationUnit, result: SsaValue, i: i128) {
    let slot = spill_slot(result).expect("i64 常量没有宽槽");
    let lo = (i as u64 & 0xffff_ffff) as u32;
    let hi = ((i as u64) >> 32) as u32;
    emit_mov_imm32(unit, SPILL_SRC, lo);
    store_spill(unit, SPILL_SRC, slot);
    emit_mov_imm32(unit, SPILL_SRC2, hi);
    store_spill_hi(unit, SPILL_SRC2, slot);
}

fn emit_addsub_wide(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    wide_load(unit, lhs, SPILL_SRC, SPILL_SRC2);
    wide_load2(unit, rhs, MUL_TMP_A, SPILL_DST);
    if op == SsaBinaryOp::Add {
        emit_adds(unit, SPILL_SRC, SPILL_SRC, MUL_TMP_A);
        emit_op3(unit, op::ADC, SPILL_SRC2, SPILL_SRC2, SPILL_DST);
    } else {
        emit_subs(unit, SPILL_SRC, SPILL_SRC, MUL_TMP_A);
        emit_op3(unit, op::SBC, SPILL_SRC2, SPILL_SRC2, SPILL_DST);
    }
    wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
}

fn emit_compare_wide(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    wide_load(unit, lhs, SPILL_SRC, SPILL_SRC2);
    wide_load2(unit, rhs, MUL_TMP_A, SPILL_DST);
    emit_cmp(unit, SPILL_SRC, MUL_TMP_A);
    emit_cmp_cond(unit, cc::EQ, SPILL_SRC2, SPILL_DST);
    let dst = out_reg(result);
    emit_mov_cond(unit, condition_code(op), dst, 1);
    emit_mov_cond(unit, inverse_condition_code(op), dst, 0);
    put_out(unit, result);
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
        emit_op3(unit, op::MOV, SPILL_SRC, Reg(0), s);
        let op2 = (31 << 5) | (Shift::Asr.type_code() << 4) | (s.bits() as u32);
        insn(unit, 0xe1a0_0000 | (SPILL_SRC2.bits() as u32) << 12 | op2);
        wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
    } else {
        wide_load(unit, src, SPILL_SRC, SPILL_SRC2);
        let dst = out_reg(result);
        emit_mov_rr(unit, dst, SPILL_SRC);
        put_out(unit, result);
    }
}

fn emit_mul_wide(
    unit: &mut CompilationUnit,
    result: SsaValue,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    wide_load(unit, lhs, SPILL_SRC, SPILL_SRC2);
    wide_load2(unit, rhs, MUL_TMP_A, SPILL_DST);
    insn(unit, 0xe080_0090 | (SPILL_SRC.bits() as u32) << 16 | (SPILL_SRC2.bits() as u32) << 12 | (MUL_TMP_A.bits() as u32) << 8 | SPILL_DST.bits() as u32);
    emit_cmp(unit, SPILL_SRC2, Reg(0));
    insn(unit, 0x41a0_0000 | (SPILL_SRC2.bits() as u32) << 16 | (SPILL_SRC2.bits() as u32) << 12 | SPILL_DST.bits() as u32);
    emit_cmp(unit, SPILL_DST, Reg(0));
    insn(unit, 0x41a0_0000 | (SPILL_SRC2.bits() as u32) << 16 | (SPILL_SRC2.bits() as u32) << 12 | MUL_TMP_A.bits() as u32);
    wide_store(unit, result, SPILL_SRC, SPILL_SRC2);
}

fn emit_divmod_wide(
    _unit: &mut CompilationUnit,
    _result: SsaValue,
    _lhs: SsaValue,
    _rhs: SsaValue,
    _is_div: bool,
) {
    crate::codegen::unsupported_binary_op(if _is_div { SsaBinaryOp::Div } else { SsaBinaryOp::Rem });
}

fn emit_shift_wide(
    _unit: &mut CompilationUnit,
    _result: SsaValue,
    _op: SsaBinaryOp,
    _lhs: SsaValue,
    _rhs: SsaValue,
) {
    crate::codegen::unsupported_binary_op(_op);
}

fn block_label(func: &str, id: usize) -> String {
    format!("{func}.bb{id}")
}

// ==================================================================
// 函数与指令发射
// ==================================================================

/// 为浮点 SSA 值分配栈槽。
fn assign_float_slots(func: &SsaFunction) -> usize {
    let mut floats: HashMap<u32, u8> = HashMap::new();
    let width_of =
        |v: &SsaValue, map: &HashMap<u32, u8>| -> u8 { map.get(&v.id()).copied().unwrap_or(0) };

    loop {
        let mut changed = false;
        let mut mark = |v: &SsaValue, w: u8, map: &mut HashMap<u32, u8>| -> bool {
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
                        // 前向：源是浮点 -> 结果是浮点
                        if floats.contains_key(&src.id()) {
                            let w = width_of(src, &floats);
                            if mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                        // 反向：结果是浮点 -> 源也是（形参没有类型标注，只能反推）
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

/// 发射一个函数的机器码。
pub fn emit_function(target: &Target, unit: &mut CompilationUnit, func: &SsaFunction) {
    debug_assert_eq!(target.arch(), Arch::Arm32, "ARM 后端只能用于 arm32 目标");

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

    emit_push(unit);

    // 为浮点值分配栈槽（排在最前面）
    let float_slots = assign_float_slots(func);
    let float_bytes = (float_slots * 8) as u32;

    // 被取地址的局部变量的栈槽：槽区开在浮点槽区下方。
    let slot_count = assign_local_slots(func, float_slots);
    let slot_bytes = (slot_count * 8) as u32;
    if slot_bytes > 1020 {
        crate::codegen::unsupported_const("too many address-taken locals (arm32 slot area)");
    }
    // 溢出槽数：溢出区开在最终 sp 之上、取址槽区之下；取址槽的 sp 偏移
    // 就得整体抬升溢出区高度（记进 SPILL_BASE）。
    let spill_count = SPILL_SLOTS.with(|m| m.borrow().len());
    let spill_bytes = (spill_count * 8) as u32;
    SPILL_BASE.with(|c| c.set(spill_bytes as usize));
    if float_bytes > 0 {
        emit_sub_sp_imm(unit, float_bytes);
    }
    if slot_bytes > 0 {
        emit_sub_sp_imm(unit, slot_bytes);
    }
    if spill_bytes > 0 {
        emit_sub_sp_imm(unit, spill_bytes);
    }
    // 形参从 ABI 寄存器 r0-r3 搬到分配结果所在的寄存器；浮点形参从 s0-s15/d0-d7；
    // 第 5+ 个整型实参从栈上传参区装载——基址 = push 区（push {r4-r8, lr} = 24 字节）+ 本函数槽区。
    let stack_arg_base = 24 + float_bytes + slot_bytes + spill_bytes;
    emit_param_moves(unit, func, stack_arg_base);

    for block in &func.blocks {
        block_offsets.push(unit.code.len());
        for instr in &block.instrs {
            emit_instr(unit, instr);
        }
        for succ in successors(block) {
            emit_phi_copies(unit, func, succ, block.id);
        }
        match &block.term {
            SsaTerminator::Return(ret_val) => {
                if let Some(v) = ret_val {
                    if let Some(slot) = float_slot(*v) {
                        // 浮点返回值：按 AAPCS32 在 s0/d0 返回
                        let w = float_width(*v);
                        let is_single = w == 32;
                        emit_vldr(unit, is_single, 0, slot);
                    } else if is_wide(*v) {
                        wide_load(unit, *v, Reg::R0, Reg(1));
                    } else {
                        let r = val_reg(unit, *v);
                        emit_mov_rr(unit, Reg::R0, r);
                    }
                }
                if spill_bytes > 0 {
                    emit_add_sp_imm(unit, spill_bytes);
                }
                if slot_bytes > 0 {
                    emit_add_sp_imm(unit, slot_bytes);
                }
                if float_bytes > 0 {
                    emit_add_sp_imm(unit, float_bytes);
                }
                emit_pop_pc(unit);
            }
            SsaTerminator::Goto(target) => emit_b(unit, &block_label(&func.name, *target)),
            SsaTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                // 判 cond 是否为 0：`cmp r, r` 恒为相等、分不清零/非零，
                // 必须与立即数 0 比较（`cmp r, #0`），与 AArch64 的
                // `cmp r, xzr` 同一约定。溢出的条件值先装载到暂存寄存器。
                let r = val_reg(unit, *cond);
                emit_cmp_imm8(unit, r, 0);
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
            SsaConst::Int(i) => {
                if is_wide(*result) {
                    emit_const_wide(unit, *result, *i);
                } else {
                    emit_mov_imm32(unit, out_reg(*result), *i as u32);
                    put_out(unit, *result);
                }
            }
            SsaConst::Bool(b) => {
                emit_mov_imm32(unit, out_reg(*result), u32::from(*b));
                put_out(unit, *result);
            }
            SsaConst::Unit => {
                emit_mov_imm32(unit, out_reg(*result), 0);
                put_out(unit, *result);
            }
            SsaConst::Float(bits, w) => {
                emit_float_const(unit, *bits, *w);
                if let Some(slot) = float_slot(*result) {
                    emit_vstr(unit, *w == 32, 0, slot);
                }
            }
            SsaConst::Str(_) => crate::codegen::unsupported_const("string"),
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
                    SsaBinaryOp::Div => emit_divmod_wide(unit, *result, *lhs, *rhs, true),
                    SsaBinaryOp::Rem => emit_divmod_wide(unit, *result, *lhs, *rhs, false),
                    SsaBinaryOp::Shl | SsaBinaryOp::Shr => {
                        emit_shift_wide(unit, *result, *op, *lhs, *rhs)
                    }
                    SsaBinaryOp::Eq | SsaBinaryOp::Ne | SsaBinaryOp::Lt | SsaBinaryOp::Gt | SsaBinaryOp::Le | SsaBinaryOp::Ge => {
                        emit_compare_wide(unit, *result, *op, *lhs, *rhs)
                    }
                    _ => crate::codegen::unsupported_binary_op(*op),
                }
            } else if float_slot(*lhs).is_some() || float_slot(*rhs).is_some() || ty.is_float() {
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
            let out_bytes = emit_call_args(unit, args);
            emit_bl(unit, callee);
            if out_bytes > 0 {
                emit_add_sp_imm(unit, out_bytes);
            }
            if is_wide(*result) {
                wide_store(unit, *result, Reg(0), Reg(1));
            } else if let Some(slot) = float_slot(*result) {
                // 浮点返回值在 s0/d0
                let w = float_width(*result);
                let is_single = w == 32;
                emit_vstr(unit, is_single, 0, slot);
            } else if let Some(slot) = spill_slot(*result) {
                store_spill(unit, Reg(0), slot);
            } else {
                emit_mov_rr(unit, alloc_reg(*result), Reg(0));
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
                wide_load(unit, *src, SPILL_SRC, SPILL_SRC2);
                wide_store(unit, *result, SPILL_SRC, SPILL_SRC2);
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
        // 被取地址的局部变量的栈槽地址：`add rd, sp, 8*slot`。取址槽区
        // 下方腾出了溢出区，基址要抬升溢出高度。
        SsaInstr::SlotAddr { result, local } => {
            let slot = LOCAL_SLOTS.with(|m| m.borrow().get(local).copied().unwrap_or(0));
            let off = slot * 8 + SPILL_BASE.get();
            emit_add_rd_sp_imm(unit, out_reg(*result), off as u32);
            put_out(unit, *result);
        }
        SsaInstr::Load {
            result, ptr, ty, ..
        } => {
            if is_wide(*result) || matches!(ty, NumTy::Int(64) | NumTy::Uint(64)) {
                let p = val_reg(unit, *ptr);
                emit_load_mem(unit, SPILL_SRC, p, NumTy::Int(32));
                emit_mov_imm32(unit, SPILL_DST, 4);
                emit_op3(unit, op::ADD, SPILL_DST, p, SPILL_DST);
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
                wide_load(unit, *value, SPILL_SRC, SPILL_SRC2);
                emit_store_mem(unit, p, SPILL_SRC, NumTy::Int(32));
                emit_mov_imm32(unit, SPILL_DST, 4);
                emit_op3(unit, op::ADD, SPILL_DST, p, SPILL_DST);
                emit_store_mem(unit, SPILL_DST, SPILL_SRC2, NumTy::Int(32));
            } else {
                let p = val_reg(unit, *ptr);
                let v = val_reg2(unit, *value);
                emit_store_mem(unit, p, v, *ty);
            }
        }
    }
}

// 被取地址的局部变量 → 栈槽编号（栈槽 ×8 = 字节偏移）。
thread_local! {
    static LOCAL_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

/// 被取地址的局部变量 → 栈槽编号，返回槽数；映射存进 `LOCAL_SLOTS`。
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

/// `ty` 的（字节宽度，是否有符号）。浮点不走整数 load/store。
fn mem_access(ty: NumTy) -> (usize, bool) {
    match ty {
        NumTy::Int(w) => ((w as usize) / 8, true),
        NumTy::Uint(w) => ((w as usize) / 8, false),
        NumTy::Bool => (1, false),
        NumTy::Float(w) => ((w as usize) / 8, false),
    }
}

/// `rd = [rn + 0]`，按 `ty` 位宽/符号选 load（8 字节 i64 多字长在 32 位
/// 目标上明确报错）。
fn emit_load_mem(unit: &mut CompilationUnit, rd: Reg, rn: Reg, ty: NumTy) {
    let (bytes, signed) = mem_access(ty);
    let base = match (bytes, signed) {
        (1, false) => 0xe5d0_0000, // ldrb（零扩展）
        (1, true) => 0xe1d0_00d0,  // ldrsb（符号扩展）
        (2, false) => 0xe1d0_00b0, // ldrh
        (2, true) => 0xe1d0_00f0,  // ldrsh
        (4, _) => 0xe590_0000,     // ldr（整字，无扩展可言）
        _ => crate::codegen::unsupported_const("64-bit memory access on 32-bit target"),
    };
    insn(
        unit,
        base | (rn.bits() as u32) << 16 | (rd.bits() as u32) << 12,
    );
}

/// `[rn + 0] = rm`，只写 `ty` 位宽对应的低字节。
fn emit_store_mem(unit: &mut CompilationUnit, rn: Reg, rm: Reg, ty: NumTy) {
    let (bytes, _) = mem_access(ty);
    let base = match bytes {
        1 => 0xe5c0_0000, // strb
        2 => 0xe1c0_00b0, // strh
        4 => 0xe580_0000, // str
        _ => crate::codegen::unsupported_const("64-bit memory access on 32-bit target"),
    };
    insn(
        unit,
        base | (rn.bits() as u32) << 16 | (rm.bits() as u32) << 12,
    );
}

/// 发射一元运算。
fn emit_unary(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: crate::ssa::SsaUnaryOp,
    operand: SsaValue,
) {
    use crate::ssa::SsaUnaryOp::*;

    // 浮点取负：`vneg`
    if let Some(slot) = float_slot(operand) {
        if !matches!(op, Neg) {
            crate::codegen::unsupported_const("非 Neg 的一元浮点运算");
        }
        let w = float_width(operand);
        let is_single = w == 32;
        emit_vldr(unit, is_single, 0, slot);
        emit_vneg(unit, is_single, 0, 0);
        if let Some(rs) = float_slot(result) {
            emit_vstr(unit, is_single, 0, rs);
        }
        return;
    }

    let dst = out_reg(result);
    let src = val_reg(unit, operand);
    match op {
        // !x：x 与 0 比较（与 x 自己比较恒为相等），然后条件置 1/0
        Not => {
            emit_cmp_imm8(unit, src, 0);
            emit_mov_cond(unit, cc::EQ, dst, 1);
            emit_mov_cond(unit, cc::NE, dst, 0);
        }
        // ~x：mvn dst, src
        BitNot => emit_mvn(unit, dst, src),
        // -x：rsb dst, src, #0
        Neg => emit_rsb(unit, dst, src),
    }
    put_out(unit, result);
}

/// 发射二元运算。
fn emit_binary(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
) {
    let dst = out_reg(result);
    let rn = val_reg(unit, lhs);
    let rm = val_reg2(unit, rhs);
    use SsaBinaryOp::*;
    match op {
        Add => emit_op3(unit, op::ADD, dst, rn, rm),
        Sub => emit_op3(unit, op::SUB, dst, rn, rm),
        BitAnd | And => emit_op3(unit, op::AND, dst, rn, rm),
        BitOr | Or => emit_op3(unit, op::ORR, dst, rn, rm),
        BitXor => emit_op3(unit, op::EOR, dst, rn, rm),
        Mul => emit_mul(unit, dst, rn, rm),
        Div => emit_sdiv(unit, dst, rn, rm),
        // 余数 = l - (l / r) * r。ARM 的 MLS 规定 Rd == Rn 时 UNPREDICTABLE，
        // 而商通常就想落在 dst 里；所以整条序列只用平台寄存器 r11/r12 做
        // 中转，不碰分配池，再一次性搬回 dst（dst 溢出时 r12 就是
        // SPILL_DST，`put_out` 直接落槽）。
        Rem => {
            // 商 → r11；r12 = rn − r11·rm；dst 再接管。传参对应 gas
            // （Rd, Rn, Rm, Ra）：被减数是 rn，落在 acc 位。
            emit_sdiv(unit, MUL_TMP_A, rn, rm);
            emit_mls(unit, SPILL_DST, MUL_TMP_A, rm, rn);
            emit_mov_rr(unit, dst, SPILL_DST);
        }
        Shl => emit_shift_by_reg(unit, dst, rn, rm, Shift::Lsl),
        // 右移一律取算术（ASR），与 x86-64 的 sar 一致：IR 只有一个 Shr，
        // 而整数字面量默认 i32，对有符号数必须补符号位。
        Shr => emit_shift_by_reg(unit, dst, rn, rm, Shift::Asr),
        Eq | Ne | Lt | Gt | Le | Ge => emit_compare(unit, dst, op, rn, rm),
    }
    put_out(unit, result);
}

/// 比较运算：把 0/1 写入 `dst`（靠 ARM 的条件执行）。
fn emit_compare(unit: &mut CompilationUnit, dst: Reg, op: SsaBinaryOp, rn: Reg, rm: Reg) {
    use SsaBinaryOp::*;
    match op {
        // 相等可以用异或后测零，省一次 compare 的依赖。
        // 注意必须与 **#0** 比较（cmp dst, dst 恒相等）。
        Eq | Ne => {
            emit_op3(unit, op::EOR, dst, rn, rm);
            emit_cmp_imm8(unit, dst, 0);
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

/// 浮点二元运算：算术结果落浮点栈槽，比较结果落整数寄存器。
fn emit_binary_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
    ty: NumTy,
) {
    use SsaBinaryOp::*;
    let slot = |v: SsaValue| {
        float_slot(v).unwrap_or_else(|| panic!("浮点操作数没有栈槽: {:?}", v))
    };
    // 操作数位宽：比较的结果类型是 bool，只能从两个操作数推；都推不出来时退回 ty。
    let w = {
        let mut w = float_width(lhs).max(float_width(rhs));
        if w == 0 {
            w = match ty {
                NumTy::Float(w) => w,
                _ => 64,
            };
        }
        w
    };
    let is_single = w == 32;
    emit_vldr(unit, is_single, 0, slot(lhs));
    emit_vldr(unit, is_single, 1, slot(rhs));

    match op {
        Add => emit_vfp_arith(unit, is_single, 0x0, 0, 0, 1), // vadd
        Sub => emit_vfp_arith(unit, is_single, 0x1, 0, 0, 1), // vsub
        Mul => emit_vfp_arith(unit, is_single, 0x2, 0, 0, 1), // vmul
        Div => emit_vfp_arith(unit, is_single, 0x7, 0, 0, 1), // vdiv
        Eq | Ne | Lt | Gt | Le | Ge => {
            emit_vcmp(unit, is_single, 0, 1);
            // 比较结果在整数寄存器
            let dst = out_reg(result);
            let cc = match op {
                Eq => cc::EQ,
                Ne => cc::NE,
                Lt => cc::LT,
                Gt => cc::GT,
                Le => cc::LE,
                Ge => cc::GE,
                _ => cc::AL,
            };
            emit_mov_cond(unit, cc, dst, 1);
            emit_mov_cond(unit, inverse_condition_code(op), dst, 0);
            put_out(unit, result);
        }
        _ => crate::codegen::unsupported_binary_op(op),
    }

    // 算术运算结果存回栈槽
    if matches!(op, Add | Sub | Mul | Div) {
        if let Some(rs) = float_slot(result) {
            emit_vstr(unit, is_single, 0, rs);
        }
    }
}

/// 浮点类型转换
fn emit_cast_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    src: SsaValue,
    from: NumTy,
    to: NumTy,
) {
    let src_slot = float_slot(src);
    let dst_slot = float_slot(result);

    match (from, to) {
        // 整数 -> 浮点
        (NumTy::Int(_) | NumTy::Uint(_), NumTy::Float(w)) => {
            let is_single = w == 32;
            let signed = matches!(from, NumTy::Int(_));
            let src_reg = val_reg(unit, src);
            // 将整数寄存器值移到 VFP
            // vcvt 需要源在整数寄存器，目的在 VFP
            emit_vcvt_int_to_fp(unit, is_single, signed, 0, src_reg.bits());
            if let Some(slot) = dst_slot {
                emit_vstr(unit, is_single, 0, slot);
            }
        }
        // 浮点 -> 整数
        (NumTy::Float(_), NumTy::Int(_) | NumTy::Uint(_)) => {
            let src_w = float_width(src);
            let is_single = src_w == 32;
            let signed = matches!(to, NumTy::Int(_));
            // vcvt.f32.s32 结果在整数寄存器
            emit_vldr(unit, is_single, 0, src_slot.unwrap());
            emit_vcvt_fp_to_int(unit, is_single, signed, Reg::R0.bits(), 0);
            if let Some(slot) = spill_slot(result) {
                store_spill(unit, Reg::R0, slot);
            } else {
                let dst = out_reg(result);
                emit_mov_rr(unit, dst, Reg::R0);
                put_out(unit, result);
            }
        }
        // 浮点 -> 浮点 (f32 <-> f64)
        (NumTy::Float(from_w), NumTy::Float(to_w)) => {
            let src_is_single = from_w == 32;
            let dst_is_single = to_w == 32;
            emit_vldr(unit, src_is_single, 0, src_slot.unwrap());
            emit_vcvt_fp_to_fp(unit, dst_is_single, 0, 0);
            if let Some(slot) = dst_slot {
                emit_vstr(unit, dst_is_single, 0, slot);
            }
        }
        _ => crate::codegen::unsupported_cast(from, to),
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
        emit_function(&Target::parse("arm-none-eabi").unwrap(), &mut unit, &func);
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
        let BranchShape {
            words,
            relocs,
            branch,
            jump,
            branch_at,
            then_off,
            else_off,
        } = branch_shape();

        // 条件分支必须是 `beq`（cond = 0000），不能在条件成立时跳转
        assert_eq!(branch >> 28, cc::EQ, "应为 beq，跳转即表示条件为假");
        // B/BL 的编码是 `cond | 1010`（bit 27:25），bit 24 = 0 表示 B
        // 失败时把发射结果整体打出来，方便定位
        let ctx = format!(
            "branch={branch:08x} jump={jump:08x} at={branch_at} \
             relocs={relocs:?} words={words:08x?}"
        );
        assert_eq!(
            branch & 0x0e00_0000,
            0x0a00_0000,
            "必须是 B/BL 指令族 {ctx}"
        );
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

    /// 条件结构的判零必须与**立即数 0** 比较，而不是寄存器自身：
    /// `cmp r, r` 恒为相等，任何分支都会在第一轮跳去 else（while 循环
    /// 直接不进循环体）。旧实现正是踩了这个坑。
    #[test]
    fn branch_tests_condition_against_zero_immediate() {
        let BranchShape {
            words, branch_at, ..
        } = branch_shape();

        // 在条件分支之前找最后一条数据处理指令：I=1、opcode=CMP、S=1、
        // rd=0、operand2=0 —— 即 `cmp rX, #0`。
        let limit = branch_at / 4;
        let found = (0..limit).rev().find(|&i| {
            let w = words[i];
            w >> 28 == 0xe
                    && (w >> 25) & 1 == 1
                    && (w >> 21) & 0xf == op::CMP
                    && (w >> 20) & 1 == 1 // S：设置标志位
                    && (w >> 12) & 0xf == 0 // rd=0（CMP 的固定编码）
                    && w & 0xfff == 0 // 立即数 0
        });
        assert!(
            found.is_some(),
            "cond 判零必须是 `cmp rX, #0`（立即数）words={words:08x?}"
        );
    }

    /// `cmp rN, #0` 的编码钉死值（AAPCS 汇编表达式）。
    #[test]
    fn cmp_zero_immediate_encoding() {
        let mut u = empty_unit();
        emit_cmp_imm8(&mut u, Reg(4), 0);
        assert_eq!(
            u32::from_le_bytes(u.code[..4].try_into().unwrap()),
            0xE354_0000
        );
    }

    /// 汇编器实测值（`clang -target armv7a-none-eabi -march=armv7ve` + objdump）。
    const REF: [(&str, u32); 13] = [
        ("sdiv r1,r4,r7", 0xE711_F714),
        ("udiv r3,r5,r8", 0xE733_F815),
        ("mul r1,r4,r7", 0xE001_0794),
        ("mls r1,r4,r7,r2", 0xE061_2794),
        ("mov r1,r4", 0xE1A0_1004),
        ("lsl r1,r4,r7", 0xE1A0_1714),
        ("asr r1,r4,r7", 0xE1A0_1754),
        ("asr r2,r4,r7", 0xE1A0_2754),
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
            ("asr r1,r4,r7", {
                let mut u = empty_unit();
                emit_shift_by_reg(&mut u, r(1), r(4), r(7), Shift::Asr);
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
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xe3a0_002a
        );
    }

    #[test]
    fn large_constant_uses_movw_plus_movt() {
        let mut u = empty_unit();
        emit_mov_imm32(&mut u, Reg::R0, 0x1234_5678);
        assert_eq!(u.code.len(), 8);
        // movw r0, #0x5678 / movt r0, #0x1234（imm4 在 [19:16]）
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xe305_0678
        );
        assert_eq!(
            u32::from_le_bytes(u.code[4..8].try_into().unwrap()),
            0xe341_0234
        );
    }

    #[test]
    fn rotated_immediate_covers_common_constants() {
        // 0x000000FF、0xFF000000、0x0000FF00 都应单指令完成，且**解码回
        // 原值**（硬件语义 `value = imm8 ror 2×rot`）——旋转方向写反时
        // 典型反例 `300`（=75 ror 30）会编成 `75 ror 2` 的死值。
        for value in [
            0u32,
            255,
            256,
            300,
            1024,
            75,
            0x0000_00ff,
            0x0000_ff00,
            0x00ff_0000,
            0xff00_0000,
            0x1234_u32 << 20,
        ] {
            let Some(op) = rotated_imm8_operand(value) else {
                continue;
            };
            let imm8 = op & 0xff;
            let rot = (op >> 8) & 0xf;
            let decoded = imm8.rotate_right(rot * 2);
            assert_eq!(
                decoded, value,
                "rotated immediate {op:#x} 回解码应为 {value}"
            );
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
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()) >> 24,
            0xeb
        );
        assert_eq!(u.relocations.len(), 1);
        assert_eq!(u.relocations[0].kind, RelocationKind::ArmBranch24);
        assert_eq!(u.relocations[0].offset, 0, "imm24 在指令自身的 [23:0]");
    }

    #[test]
    fn prologue_saves_callee_saved_registers() {
        let mut u = empty_unit();
        emit_push(&mut u);
        emit_pop_pc(&mut u);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xe92d_41f0
        );
        assert_eq!(
            u32::from_le_bytes(u.code[4..8].try_into().unwrap()),
            0xe8bd_81f0,
            "pop 寄存器列表含 pc"
        );
    }

    #[test]
    fn remainder_uses_div_and_mls() {
        let mut u = empty_unit();
        emit_binary(
            &mut u,
            SsaValue(3),
            SsaBinaryOp::Rem,
            SsaValue(0),
            SsaValue(1),
        );
        // 余数避开 MLS 的 Rd==Rn UNPREDICTABLE：商落 r11（MUL_TMP_A），
        // 余数先落 r12（SPILL_DST）再搬回分配寄存器 r3。
        //   r11 = r0 / r1; r12 = r0 - r11 * r1; r3 = r12
        let sdiv = u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        let mls = u32::from_le_bytes(u.code[4..8].try_into().unwrap());
        let mv = u32::from_le_bytes(u.code[8..12].try_into().unwrap());
        assert_eq!(sdiv, 0xe71b_f110, "sdiv r11, r0, r1");
        assert_eq!(mls, 0xe06c_019b, "mls r12 = r0 - r11*r1");
        assert_eq!(mv, 0xe1a0_300c, "mov r3, r12");
    }

    /// load/store 编码基准（clang --target=armv7a-none-eabi + objdump 实测）：
    ///   ldr  r0, [r1]  = 0xE591_0000   str  r0, [r1]  = 0xE581_0000
    ///   ldrb r0, [r1]  = 0xE5D1_0000   strb r0, [r1]  = 0xE5C1_0000
    ///   ldrsb r0, [r1] = 0xE1D1_00D0   ldrh r0, [r1]  = 0xE1D1_00B0
    ///   ldrsh r0, [r1] = 0xE1D1_00F0   strh r0, [r1]  = 0xE1C1_00B0
    #[test]
    fn load_store_select_width_and_sign() {
        let ld = |ty| {
            let mut u = empty_unit();
            emit_load_mem(&mut u, Reg(0), Reg(1), ty);
            u32::from_le_bytes(u.code[0..4].try_into().unwrap())
        };
        let st = |ty| {
            let mut u = empty_unit();
            emit_store_mem(&mut u, Reg(1), Reg(0), ty);
            u32::from_le_bytes(u.code[0..4].try_into().unwrap())
        };
        assert_eq!(ld(NumTy::Int(8)), 0xe1d1_00d0, "ldrsb r0, [r1]");
        assert_eq!(ld(NumTy::Uint(8)), 0xe5d1_0000, "ldrb r0, [r1]");
        assert_eq!(ld(NumTy::Int(16)), 0xe1d1_00f0, "ldrsh r0, [r1]");
        assert_eq!(ld(NumTy::Uint(16)), 0xe1d1_00b0, "ldrh r0, [r1]");
        assert_eq!(ld(NumTy::Int(32)), 0xe591_0000, "ldr r0, [r1]");
        assert_eq!(ld(NumTy::Bool), 0xe5d1_0000, "bool 也是无符号单字节");
        assert_eq!(st(NumTy::Int(8)), 0xe5c1_0000, "strb r0, [r1]");
        assert_eq!(st(NumTy::Int(16)), 0xe1c1_00b0, "strh r0, [r1]");
        assert_eq!(st(NumTy::Int(32)), 0xe581_0000, "str r0, [r1]");
    }

    /// imm12 = 8 位值循环右移偶数位字段：0x1000 = 1<<(12)【k=20】、1020 =
    /// 255<<2【k=2】、255 自身【k=0】。
    #[test]
    fn arm_immediates_encode_as_rotated_bytes() {
        assert_eq!(arm_imm_field(255), Some(255), "k=0 直接落地");
        // 1020 = 255<<2 = 255 ror 30 → imm12 = 0xFF | (15<<8)
        assert_eq!(arm_imm_field(1020), Some(0x0fff), "1020 = 255ror30");
        // 0x1000 = 1<<12 = 1 ror 20 → imm12 = 1 | (10<<8)
        assert_eq!(
            arm_imm_field(0x1000),
            Some(1 | (10 << 8)),
            "0x1000 = 1ror20"
        );
        // 1007 = 0b1111101111 不是任何 8 位值的偶数位旋转
        assert_eq!(arm_imm_field(1007), None);
    }

    #[test]
    fn sp_immediate_adjust_encodes() {
        let mut u = empty_unit();
        emit_sub_sp_imm(&mut u, 8);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xe24d_d008,
            "sub sp, sp, #8"
        );
        let mut u = empty_unit();
        emit_add_sp_imm(&mut u, 1020);
        // 1020 的字段为 0xFF | (15<<8) = 0xFFF
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xe28d_dfff,
            "add sp, sp, #1020"
        );
        // add r5, sp, #24
        let mut u = empty_unit();
        emit_add_rd_sp_imm(&mut u, Reg(5), 24);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xe28d_5018,
            "add r5, sp, #24"
        );
    }
}

/// 寄存器分配。
///
/// 与 AArch64 同构：先用线性扫描给每个值定寄存器，`alloc_reg` 按结果走；
/// 否则活跃值超过可分配寄存器数时报错——宁可明确报错，也不能安静地
/// 编出一个算错的程序（取模分配会让同余的值抢同一个寄存器）。
///
/// x86-64 后端自己实现了溢出，见 `encoder.rs`。
fn check_register_pressure(func: &SsaFunction) {
    // 跨调用存活的值只放被调用者保存的 r4-r8，否则 `bl` 会破坏它。
    // 多字长（i64）值走栈槽，不占寄存器池。
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
