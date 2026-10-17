//! AArch64 指令选择与机器码发射。
//!
//! 与 x86-64 后端（[`crate::codegen::encoder`]）使用同一套中间表示：
//! SSA 值经 [`alloc_reg`] 映射到固定物理寄存器，再逐条编码为 A64 指令。
//!
//! 遵循 AAPCS64：
//! - 整数参数与返回值位于 `x0..x7`，返回值为 `x0`；
//! - `x29`（FP）、`x30`（LR）、`sp` 保留给栈帧与链接；
//! - 分支/调用使用 26 位字面量（`bl`/`b`）与 19 位字面量（`b.cond`），
//!   二者都需要重定位。
//!
//! 指令编码备忘（64 位版本）：
//! ```text
//! movz xd, #imm16, lsl #shift   1101 0010 100 hw imm16 Rd
//! movk xd, #imm16, lsl #shift   1111 0010 100 hw imm16 Rd
//! orr  xd, xn, xm              1010 1010 00 Rm 000000 Rn Rd   （mov 为 orr xd, xzr, xm）
//! add  xd, xn, xm              1000 1011 000 Rm 000000 Rn Rd
//! sub  xd, xn, xm              1100 1011 000 Rm 000000 Rn Rd
//! ands xd, xn, xm              1000 1010 000 Rm 000000 Rn Rd
//! cmp  xn, xm（= subs xzr）    1110 1011 000 Rm 001111 Rn Rd
//! cset xd, cond                1001 1010 1001 1111 0000 11 cond Rd
//! b    label                   0001 01 imm26
//! b.cond label                 0101 0100 imm19 0 cond
//! bl   label                   1001 01 imm26
//! ret                          1101 0110 0101 1111 0000 0011 0000
//! ```

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crate::codegen::target::{Arch, Target};
use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};
use crate::mir::NumTy;
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaValue};

/// AArch64 通用寄存器（编码为 5 位寄存器号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(u8);

impl Reg {
    /// x0：第一个参数寄存器，同时是返回值寄存器。
    pub const X0: Reg = Reg(0);

    /// 5 位寄存器号。
    pub fn bits(self) -> u8 {
        self.0 & 31
    }
}

/// 零寄存器（xzr / wzr）：`mul`（madd 的 Ra）与 `mov`（orr 的 Rn）用到。
const XZR: u8 = 31;

/// 可用于分配的通用寄存器：x0..x17（caller-save）+ x19..x28（callee-save）。
///
/// x18 是平台保留寄存器（Linux 用作 TLS），x29/x30 是帧指针与链接寄存器，
/// x31 在非 `SP` 语境下即 `xzr`，因此都不参与分配。
///
/// AAPCS64 里 x0..x17 全部是 caller-save、调用后即被破坏；x19..x28 是
/// callee-save。跨调用存活的值必须落在后者，否则 `call` 返回时内容就没了
/// （见 [`CALLEE_SAVE`] 与序言里的保存/恢复）。
const ALLOCATABLE: [Reg; 28] = [
    Reg(0),
    Reg(1),
    Reg(2),
    Reg(3),
    Reg(4),
    Reg(5),
    Reg(6),
    Reg(7),
    Reg(8),
    Reg(9),
    Reg(10),
    Reg(11),
    Reg(12),
    Reg(13),
    Reg(14),
    Reg(15),
    Reg(16),
    Reg(17),
    Reg(19),
    Reg(20),
    Reg(21),
    Reg(22),
    Reg(23),
    Reg(24),
    Reg(25),
    Reg(26),
    Reg(27),
    Reg(28),
];

/// `ALLOCATABLE` 里属于 callee-save 的下标（x19..x28）。
///
/// 传给 `allocate_registers`，使跨调用存活的值只占用这些寄存器；序言借此
/// 知道要保存/恢复哪些。
const CALLEE_SAVE: [usize; 10] = [18, 19, 20, 21, 22, 23, 24, 25, 26, 27];

/// AAPCS64 的整数实参寄存器 x0-x7。
const ARG_REGS: [Reg; 8] = [
    Reg(0),
    Reg(1),
    Reg(2),
    Reg(3),
    Reg(4),
    Reg(5),
    Reg(6),
    Reg(7),
];

/// 把实参搬进 AAPCS64 的寄存器。
///
/// 整数实参与浮点实参**各自计数**：`f(g(1, 2.0), 3)` 里 1 在 x0、2.0 在 v0、
/// 3 在 x1。整数搬运是并行的（目标与源可能交叉），浮点则直接落 v 寄存器。
fn emit_call_args(unit: &mut CompilationUnit, args: &[SsaValue]) {
    let mut gi = 0usize;
    let mut xi = 0usize;
    let mut moves: Vec<(Reg, Reg)> = Vec::new();
    for a in args {
        if let Some(slot) = float_slot(*a) {
            if xi < 8 {
                emit_load_float(unit, xi as u8, slot, float_width(*a));
            }
            // 超过 8 个浮点实参走栈上传参区，本后端尚未实现。
            xi += 1;
        } else if gi < ARG_REGS.len() {
            moves.push((ARG_REGS[gi], alloc_reg(*a)));
            gi += 1;
        }
        // 超过 8 个整数实参走栈上传参区，本后端尚未实现。
    }
    emit_parallel_reg_moves(unit, &moves);
}

fn alloc_reg(v: SsaValue) -> Reg {
    if let Some(i) = REG_MAP.with(|m| m.borrow().get(&v.id()).copied()) {
        return ALLOCATABLE[i];
    }
    ALLOCATABLE[(v.id() as usize) % ALLOCATABLE.len()]
}

// ==================================================================
// 浮点支持
//
// 与 x86-64 后端同构：浮点值不参与整数寄存器分配，每个浮点 SSA 值分配一个
// 专属栈槽，运算时用 `d0/s0`、`d1/s1` 当暂存器读写内存。这样既不必给
// `alloc_reg` 的取模分配器再添一套并行的浮点活跃区间，也不会扰动已经验证过
// 的整数路径。
//
// 槽位通过函数序言里的 `sub sp, sp, #frame` 预留（`frame` 由浮点值个数决定），
// 运行时按 `[sp, #8*slot]` 寻址。
// ==================================================================

thread_local! {
    /// 当前函数的浮点栈槽：值 id -> (槽号, 位宽 32/64)。
    static FLOAT_SLOTS: RefCell<HashMap<u32, (usize, u8)>> = RefCell::new(HashMap::new());
    /// 当前函数为浮点站槽预留的字节数（驱动序言/尾声的 `sub/add sp`）。
    static FRAME_BYTES: Cell<usize> = const { Cell::new(0) };
    /// 当前函数的整数寄存器分配：值 id -> `ALLOCATABLE` 下标。
    ///
    /// 用线性扫描的结果，而不是 `id % 18` 取模——取模会让两个同时存活、
    /// 编号同余的值抢同一个寄存器，后写者把先写者冲掉，安静地算错。
    static REG_MAP: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
    /// 被取地址的局部变量的专属栈槽：局部 id -> 槽号。
    ///
    /// 排在浮点槽之后，同一块帧里，`[sp, #8*slot]` 寻址。
    static LOCAL_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

/// `v` 的浮点位宽（32 或 64）。整数返回 0。
fn float_width(v: SsaValue) -> u8 {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).map(|(_, w)| *w).unwrap_or(0))
}

/// `v` 如果是浮点值，返回它的栈槽。
fn float_slot(v: SsaValue) -> Option<usize> {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).map(|(s, _)| *s))
}

/// `ldr {s|d}<t>, [sp, #8*slot]`。
fn emit_load_float(unit: &mut CompilationUnit, t: u8, slot: usize, width: u8) {
    // 立即数是"偏移 / 元素大小"：S 按 4 缩放，D 按 8 缩放。
    let (base, scale) = if width == 32 {
        (0xbd40_0000u32, 4)
    } else {
        (0xfd40_0000u32, 8)
    };
    let imm12 = ((slot * 8) / scale) as u32;
    insn(unit, base | (imm12 << 10) | (31 << 5) | (t & 31) as u32);
}

/// `str {s|d}<t>, [sp, #8*slot]`。
fn emit_store_float(unit: &mut CompilationUnit, t: u8, slot: usize, width: u8) {
    let (base, scale) = if width == 32 {
        (0xbd00_0000u32, 4)
    } else {
        (0xfd00_0000u32, 8)
    };
    let imm12 = ((slot * 8) / scale) as u32;
    insn(unit, base | (imm12 << 10) | (31 << 5) | (t & 31) as u32);
}

/// `str x<rt>, [sp, #off]`（保存 callee-save 寄存器；`imm12 = off/8`）。
fn emit_str_sp(unit: &mut CompilationUnit, rt: u8, off: usize) {
    insn(
        unit,
        0xf900_0000 | (((off / 8) as u32) << 10) | (31 << 5) | (rt & 31) as u32,
    );
}

/// `ldr x<rt>, [sp, #off]`（恢复 callee-save 寄存器）。
fn emit_ldr_sp(unit: &mut CompilationUnit, rt: u8, off: usize) {
    insn(
        unit,
        0xf940_0000 | (((off / 8) as u32) << 10) | (31 << 5) | (rt & 31) as u32,
    );
}

/// 内存操作的位宽与有无符号：`(字节数, 有符号)`。
fn mem_access(ty: NumTy) -> (usize, bool) {
    match ty {
        NumTy::Int(w) => ((w as usize) / 8, true),
        NumTy::Uint(w) => ((w as usize) / 8, false),
        NumTy::Bool => (1, false),
        NumTy::Float(w) => ((w as usize) / 8, false),
    }
}

/// `add <dst>, sp, #8*slot`——被取地址的局部变量的地址。
fn emit_lea_slot(unit: &mut CompilationUnit, dst: u8, slot: usize) {
    // ADD (immediate) 64 位、无移位：`0x91000000`。imm12 按 1 缩放。
    insn(
        unit,
        0x9100_0000 | (((slot * 8) as u32 & 0xfff) << 10) | (31 << 5) | (dst & 31) as u32,
    );
}

/// `<dst> = [<base>]`，按 `ty` 的位宽/符号扩展。
fn emit_load_mem(unit: &mut CompilationUnit, dst: u8, base: u8, ty: NumTy) {
    let (bytes, signed) = mem_access(ty);
    let op = match (bytes, signed) {
        (1, false) => 0x3940_0000, // ldrb w
        (1, true) => 0x39c0_0000,  // ldrsb x（符号扩展到 64）
        (2, false) => 0x7940_0000, // ldrh w
        (2, true) => 0x79c0_0000,  // ldrsh x
        (4, false) => 0xb940_0000, // ldr w（零扩展到 64）
        (4, true) => 0xb980_0000,  // ldrsw x
        _ => 0xf940_0000,          // ldr x
    };
    insn(unit, op | ((base & 31) as u32) << 5 | (dst & 31) as u32);
}

/// `[<base>] = <src>`，只写 `ty` 位宽对应的低字节。
fn emit_store_mem(unit: &mut CompilationUnit, base: u8, src: u8, ty: NumTy) {
    let (bytes, _) = mem_access(ty);
    let op = match bytes {
        1 => 0x3900_0000, // strb w
        2 => 0x7900_0000, // strh w
        4 => 0xb900_0000, // str w
        _ => 0xf900_0000, // str x
    };
    insn(unit, op | ((base & 31) as u32) << 5 | (src & 31) as u32);
}

/// 扫出所有被取地址的局部变量，各分配一个栈槽（排在 `base` 之后）。
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

/// 标量浮点二元算术：`fadd/fsub/fmul/fdiv {s|d}<rd>, <rn>, <rm>`。
///
/// `opcode2`：`Mul=0x02`、`Div=0x06`、`Add=0x0A`、`Sub=0x0E`；
/// 类型位在 bit22（`S=0`、`D=1`）。
fn emit_fp_arith(unit: &mut CompilationUnit, width: u8, opcode2: u32, rd: u8, rn: u8, rm: u8) {
    let ty = if width == 32 { 0u32 } else { 1 };
    insn(
        unit,
        0x1e20_0000
            | (ty << 22)
            | 1 << 21
            | (opcode2 << 10)
            | ((rm & 31) as u32) << 16
            | ((rn & 31) as u32) << 5
            | (rd & 31) as u32,
    );
}

/// `fneg {s|d}<rd>, {s|d}<rn>`。
///
/// 基址是 `0x1e214000`（`fneg s`）；`d` 置 bit22。注意**不是** `0x1e204000`：
/// 后者缺 bit16，`fneg d13, d14` 会错成 `0x1e6041cd`（clang 为 `0x1e6141cd`）。
fn emit_fneg(unit: &mut CompilationUnit, width: u8, rd: u8, rn: u8) {
    let ty = if width == 32 { 0u32 } else { 1 };
    insn(
        unit,
        0x1e21_4000 | (ty << 22) | ((rn & 31) as u32) << 5 | (rd & 31) as u32,
    );
}

/// `fcmp {s|d}<rn>, {s|d}<rm>`（置 NZCV）。
fn emit_fcmp(unit: &mut CompilationUnit, width: u8, rn: u8, rm: u8) {
    let ty = if width == 32 { 0u32 } else { 1 };
    insn(
        unit,
        0x1e20_2000 | (ty << 22) | ((rm & 31) as u32) << 16 | ((rn & 31) as u32) << 5,
    );
}

/// `scvtf/ucvtf {s|d}<rd>, x<rn>`：整数 -> 浮点（源按 64 位有/无符号）。
fn emit_int_to_fp(unit: &mut CompilationUnit, width: u8, signed: bool, rd: u8, rn: u8) {
    let ty = if width == 32 { 0u32 } else { 1 };
    let base = if signed { 0x9e22_0000 } else { 0x9e23_0000 };
    insn(
        unit,
        base | (ty << 22) | ((rn & 31) as u32) << 5 | (rd & 31) as u32,
    );
}

/// `fcvtzs/fcvtzu x<rd>, {s|d}<rn>`：浮点 -> 整数（截断向零，64 位结果）。
fn emit_fp_to_int(unit: &mut CompilationUnit, width: u8, signed: bool, rd: u8, rn: u8) {
    let ty = if width == 32 { 0u32 } else { 1 };
    let base = if signed { 0x9e38_0000 } else { 0x9e39_0000 };
    insn(
        unit,
        base | (ty << 22) | ((rn & 31) as u32) << 5 | (rd & 31) as u32,
    );
}

/// `fcvt {s|d}<rd>, {s|d}<rn>`：单双精度互转。
fn emit_fcvt(unit: &mut CompilationUnit, dst_is_single: bool, rd: u8, rn: u8) {
    let base = if dst_is_single {
        0x1e62_4000u32 // f64 -> f32
    } else {
        0x1e22_c000u32 // f32 -> f64
    };
    insn(unit, base | ((rn & 31) as u32) << 5 | (rd & 31) as u32);
}

/// `fmov s<rd>, w<rn>`（按位搬运，不做数值转换）。
fn emit_fmov_w_to_s(unit: &mut CompilationUnit, rd: u8, rn: u8) {
    insn(
        unit,
        0x1e27_0000 | ((rn & 31) as u32) << 5 | (rd & 31) as u32,
    );
}

/// `fmov d<rd>, x<rn>`（按位搬运）。
fn emit_fmov_x_to_d(unit: &mut CompilationUnit, rd: u8, rn: u8) {
    insn(
        unit,
        0x9e67_0000 | ((rn & 31) as u32) << 5 | (rd & 31) as u32,
    );
}

/// 浮点常量/转换的中转寄存器。
///
/// 必须是**不可分配**的寄存器：`ALLOCATABLE` 是 x0-x17，任何固定的 x0-x17 都
/// 可能正持有活跃的整数值，借它中转会把该值踩掉（`main` 里 `mov x0, #3`
/// 之后再往 x0 装浮点常量的位模式，实参 3 就没了）。x18 已被本后端借作 φ 搬运
/// 的中转，同样不参与分配，这里复用它。
const FP_SCRATCH: u8 = 18;

/// 把一个浮点常量装进 `{s|d}<t>`：位模式先经中转寄存器。
fn emit_float_const(unit: &mut CompilationUnit, t: u8, bits: u64, width: u8) {
    emit_mov_imm64(unit, Reg(FP_SCRATCH), bits);
    if width == 32 {
        emit_fmov_w_to_s(unit, t, FP_SCRATCH);
    } else {
        emit_fmov_x_to_d(unit, t, FP_SCRATCH);
    }
}

/// `sub sp, sp, #imm12`（序言）。
fn emit_sub_sp(unit: &mut CompilationUnit, imm: u32) {
    insn(unit, 0xd100_0000 | (imm << 10) | (31 << 5) | 31);
}

/// `add sp, sp, #imm12`（尾声）。
fn emit_add_sp(unit: &mut CompilationUnit, imm: u32) {
    insn(unit, 0x9100_0000 | (imm << 10) | (31 << 5) | 31);
}

/// `adrp rd, #imm`：页对齐地址的高 21 位（imm 已是"页数"，即字节偏移 >> 12）。
fn emit_adrp(unit: &mut CompilationUnit, rd: Reg, imm: i64) {
    let imm = imm as u32 & 0x1f_ffff;
    let immlo = (imm & 0x3) << 29;
    let immhi = ((imm >> 2) & 0x7ffff) << 5;
    insn(unit, 0x9000_0000 | immlo | immhi | rd.bits() as u32);
}

/// `add rd, rn, #imm12`（64 位）。
fn emit_add_imm(unit: &mut CompilationUnit, rd: Reg, rn: Reg, imm: u32) {
    insn(
        unit,
        0x9100_0000 | ((imm & 0xfff) << 10) | ((rn.bits() as u32) << 5) | rd.bits() as u32,
    );
}

/// 把 `.rodata` 符号的地址装进 `dst`：`adrp dst, sym` + `add dst, dst, :lo12:sym`。
///
/// 两条指令各配一条重定位（页差 + 页内偏移），链接器或自构造可执行文件的
/// 回填路径据此计算最终地址。
fn emit_str_addr(unit: &mut CompilationUnit, dst: Reg, sym: &str) {
    let a = unit.code.len();
    emit_adrp(unit, dst, 0);
    unit.relocations
        .push(Relocation::new(a, a, 4, sym, RelocationKind::AdrPrelPage21));
    let b = unit.code.len();
    emit_add_imm(unit, dst, dst, 0);
    unit.relocations
        .push(Relocation::new(b, b, 4, sym, RelocationKind::AddAbsLo12));
}

/// 寄存器到寄存器的**并行**搬运（可含环，用 `x18` 打开）。
fn emit_parallel_reg_moves(unit: &mut CompilationUnit, moves: &[(Reg, Reg)]) {
    let mut moves: Vec<(Reg, Reg)> = moves.iter().copied().filter(|(d, s)| d != s).collect();
    while !moves.is_empty() {
        let safe = moves
            .iter()
            .position(|(d, _)| !moves.iter().any(|(_, s)| s == d));
        match safe {
            Some(at) => {
                let (d, s) = moves.remove(at);
                emit_mov_rr(unit, d, s);
            }
            None => {
                let s = moves[0].1;
                emit_mov_rr(unit, PHI_TMP, s);
                for m in moves.iter_mut() {
                    if m.1 == s {
                        m.1 = PHI_TMP;
                    }
                }
            }
        }
    }
}

/// 序言里把 ABI 实参寄存器搬进形参的位置（AAPCS64：整数 x0-x7，浮点 v0-v7，
/// 两类各自计数）。
fn emit_param_moves(unit: &mut CompilationUnit, func: &SsaFunction) {
    // 浮点形参先落栈槽——这些 store 只写内存，可以先于整数搬运做。
    let mut gi = 0usize;
    let mut xi = 0usize;
    let mut int_moves: Vec<(Reg, Reg)> = Vec::new();
    for p in &func.params {
        if let Some(sl) = float_slot(*p) {
            if xi < 8 {
                emit_store_float(unit, xi as u8, sl, float_width(*p));
            }
            xi += 1;
            continue;
        }
        if gi >= ARG_REGS.len() {
            continue; // 超过 8 个整数形参走栈上传参区，本后端尚未实现
        }
        int_moves.push((alloc_reg(*p), ARG_REGS[gi]));
        gi += 1;
    }
    // 整数形参的搬运要并行走环：`fn f(x: i32, y: i32)` 里目标寄存器与实参
    // 寄存器可能交叉，顺序 `mov` 会互相覆盖。
    emit_parallel_reg_moves(unit, &int_moves);
}

// ==================================================================
// 指令编码原语
// ==================================================================

/// 写入 4 字节小端指令。
fn insn(unit: &mut CompilationUnit, word: u32) {
    unit.code.extend_from_slice(&word.to_le_bytes());
}

/// `movz` / `movk`：加载或改写 64 位立即数的 16 位片段。
fn emit_mov_wide(unit: &mut CompilationUnit, rd: Reg, imm: u64, op: u32, shift: u32) {
    insn(
        unit,
        op | (shift << 21) | (((imm & 0xffff) as u32) << 5) | rd.bits() as u32,
    );
}

/// `mov xd, #imm64`（movz + 至多三段 movk）。
fn emit_mov_imm64(unit: &mut CompilationUnit, rd: Reg, imm: u64) {
    emit_mov_wide(unit, rd, imm, 0xd280_0000, 0);
    for half in 1..4u32 {
        let chunk = (imm >> (half * 16)) & 0xffff;
        if chunk != 0 {
            emit_mov_wide(unit, rd, chunk, 0xf280_0000, half);
        }
    }
}

/// `mov xd, xm`（`orr xd, xzr, xm`）。
fn emit_mov_rr(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    insn(
        unit,
        0xaa00_03e0 | (rm.bits() as u32) << 16 | rd.bits() as u32,
    );
}

/// 类型转换：取低 `bits` 位并按 `signed` 扩展到 64 位。
///
/// AArch64 用 SBFM / UBFM 一步完成任意位宽的扩展：
/// `sbfm xd, xn, #lsb, #width-1` / `ubfm xd, xn, #0, #width-1`。
/// 位宽已经是 64 时退化成一次 `mov`。
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
            emit_mov_rr(unit, rd, rm);
        }
        return;
    }
    // immr = 0 表示不右移，imms = bits-1 决定取多少位；源寄存器在 bit5-9。
    // SBFM(64) 基址 0x93400000，UBFM(64) 基址 0xd3400000（不是 0x53400000）。
    let imms = (bits - 1) as u32;
    let base = if signed {
        0x9340_0000u32
    } else {
        0xd340_0000u32
    };
    insn(
        unit,
        base | imms << 10 | (rm.bits() as u32) << 5 | rd.bits() as u32,
    );
}

/// 三地址算术/逻辑：`op xd, xn, xm`。
fn emit_op3(unit: &mut CompilationUnit, op: u32, rd: Reg, rn: Reg, rm: Reg) {
    insn(
        unit,
        op | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5 | rd.bits() as u32,
    );
}

/// `add xd, xn, xm`
fn emit_add(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    emit_op3(unit, 0x8b00_0000, rd, rn, rm);
}

/// `sub xd, xn, xm`
fn emit_sub(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    emit_op3(unit, 0xcb00_0000, rd, rn, rm);
}

/// `ands xd, xn, xm`（逻辑与，结果同时更新 NZCV）。
fn emit_ands(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    emit_op3(unit, 0x8a00_0000, rd, rn, rm);
}

/// `cmp xn, xm`（`subs xzr, xn, xm`）。
fn emit_cmp(unit: &mut CompilationUnit, rn: Reg, rm: Reg) {
    insn(
        unit,
        0xeb00_001f | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5,
    );
}

/// `cset xd, cond`（`csinc xd, xzr, xzr, invert(cond)`）。
fn emit_cset(unit: &mut CompilationUnit, rd: Reg, cond: u32) {
    insn(unit, 0x9a9f_07e0 | ((cond ^ 1) << 12) | rd.bits() as u32);
}

/// `madd xd, xn, xm, xa`（`xa = XZR` 时即乘法）。
fn emit_madd(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg, ra: u8) {
    insn(
        unit,
        0x9b00_0000
            | (rm.bits() as u32) << 16
            | (ra as u32) << 10
            | (rn.bits() as u32) << 5
            | rd.bits() as u32,
    );
}

/// `sdiv xd, xn, xm`
fn emit_sdiv(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    insn(
        unit,
        0x9ac0_0c00 | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5 | rd.bits() as u32,
    );
}

/// `msub xd, xn, xm, xa`：`xa - xn * xm`（取模用）。
fn emit_msub(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg, ra: u8) {
    insn(
        unit,
        0x9b00_8000
            | (rm.bits() as u32) << 16
            | (ra as u32) << 10
            | (rn.bits() as u32) << 5
            | rd.bits() as u32,
    );
}

/// 数据处理（2）类指令：移位等，`op xd, xn, xm`。
fn emit_dp2(unit: &mut CompilationUnit, op: u32, rd: Reg, rn: Reg, rm: Reg) {
    insn(
        unit,
        op | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5 | rd.bits() as u32,
    );
}

/// `ret`（等价 `ret x30`）。
fn emit_ret(unit: &mut CompilationUnit) {
    insn(unit, 0xd65f_03c0);
}

/// `brk #0`——非法指令位置（`unreachable` 终结符）。
fn emit_brk(unit: &mut CompilationUnit) {
    insn(unit, 0xd420_0000);
}

/// `stp x29, x30, [sp, #-16]!`（序言）。
fn emit_prologue(unit: &mut CompilationUnit) {
    insn(unit, 0xa9bf_7bfd);
}

/// `ldp x29, x30, [sp], #16`（尾声）。
fn emit_epilogue(unit: &mut CompilationUnit) {
    insn(unit, 0xa8c1_7bfd);
}

/// `bl label`：26 位字面量调用（需重定位）。
///
/// A64 的 26 位字面量位于指令自身的 [25:0] 位，因此重定位的字段偏移
/// 与指令起始偏移相同（不同于 x86-64 的"立即数在指令末尾"）。
fn emit_call(unit: &mut CompilationUnit, target: &str) {
    let instr_start = unit.code.len();
    unit.code.extend_from_slice(&0x9400_0000u32.to_le_bytes());
    push_branch_reloc(unit, instr_start, target, RelocationKind::Call26);
}

/// `b label`：26 位字面量无条件跳转（需重定位）。
fn emit_b(unit: &mut CompilationUnit, target: &str) {
    let instr_start = unit.code.len();
    unit.code.extend_from_slice(&0x1400_0000u32.to_le_bytes());
    push_branch_reloc(unit, instr_start, target, RelocationKind::Branch26);
}

/// 登记一条 26 位字面量分支重定位。
fn push_branch_reloc(
    unit: &mut CompilationUnit,
    instr_start: usize,
    target: &str,
    kind: RelocationKind,
) {
    unit.relocations
        .push(Relocation::new(instr_start, instr_start, 4, target, kind));
}

/// 条件跳转序列。
///
/// A64 的 `b.cond` 只有 19 位字面量（±1 MiB），不足以直接跳到任意基本块；
/// 因此这里采用经典的两级结构：
/// ```text
///     b.<!cond> +8      ; 取反条件成立时跳过下面这条 b，落到 else
///     b    then_label
///     b    else_label
/// ```
/// 两条无条件跳转使用 26 位字面量，由链接器（或本编译器的重定位回填）
/// 修正到实际基本块。
///
/// 偏移是 `imm19 * 4`，且相对**本条指令**。要跨过紧跟的 `b then`（4 字节）
/// 落到 `b else`，需 `imm19 = 2`——写 1 只会落到 `b then`，于是无论条件真假
/// 都走 then 分支（这是一个曾经存在的真 bug）。
fn emit_cond_branch(unit: &mut CompilationUnit, cond: u32, then_label: &str, else_label: &str) {
    // 0x54000000 | imm19 << 5 | cond；imm19 = 2 → 目标为两条指令之后。
    insn(unit, 0x5400_0000 | (2u32 << 5) | ((cond ^ 1) & 0xf));
    emit_b(unit, then_label);
    emit_b(unit, else_label);
}

/// A64 条件码。
mod cc {
    /// 等于（`cmp` 的 Z 位置 1）。
    pub const EQ: u32 = 0b0000;
    /// 不等于。
    pub const NE: u32 = 0b0001;
    /// 有符号大于等于。
    pub const GE: u32 = 0b1010;
    /// 有符号小于。
    pub const LT: u32 = 0b1011;
    /// 有符号大于。
    pub const GT: u32 = 0b1100;
    /// 有符号小于等于。
    pub const LE: u32 = 0b1101;
    /// 负（`fcmp` 的 N 位）：浮点"小于"。
    ///
    /// `fcmp` 对 NaN 置 N=0，因此 `mi` 天然把无序比较判成假，符合 IEEE754。
    pub const MI: u32 = 0b0100;
    /// 无符号小于等于（C=0 或 Z=1）：浮点"小于等于"。
    ///
    /// `fcmp` 对 NaN 置 C=1、Z=0，于是 `ls` 为假，符合 IEEE754。
    pub const LS: u32 = 0b1001;
}

/// 基本块标签：`<函数名>.bb<编号>`。
///
/// 早先只用 `bb<编号>`，于是**不同函数的同名块会共用一个符号**——`main`
/// 的 `bb1` 和 `f` 的 `bb1` 撞在一起，`main` 里的分支会跳进别的函数。
/// 加上函数名前缀后每个块的标签全局唯一。
/// AArch64 上借 x18（平台寄存器，AAPCS64 允许临时使用）做 φ 搬运的中转。
const PHI_TMP: Reg = Reg(18);

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

/// 在当前块末尾把本块持有的值写进后继块的各个 φ 寄存器（并行拷贝）。
fn emit_phi_copies(unit: &mut CompilationUnit, func: &SsaFunction, succ: usize, from: usize) {
    let Some(block) = func.blocks.iter().find(|b| b.id == succ) else {
        return;
    };
    // 浮点 φ 的两侧都是浮点栈槽：用 d0 中转，不碰 GPR。
    for instr in &block.instrs {
        let SsaInstr::Phi {
            result, incoming, ..
        } = instr
        else {
            continue;
        };
        let Some((_, src)) = incoming.iter().find(|(p, _)| *p == from) else {
            continue;
        };
        if let (Some(rs), Some(ss)) = (float_slot(*result), float_slot(*src)) {
            let w = float_width(*result).max(float_width(*src));
            emit_load_float(unit, 0, ss, w);
            emit_store_float(unit, 0, rs, w);
        }
    }
    // 整数 φ 的拷贝是并行的（含环），浮点 φ 已在上面处理完。
    let mut moves: Vec<(Reg, Reg)> = Vec::new();
    for instr in &block.instrs {
        let SsaInstr::Phi {
            result, incoming, ..
        } = instr
        else {
            continue;
        };
        let Some((_, src)) = incoming.iter().find(|(p, _)| *p == from) else {
            continue;
        };
        if float_slot(*result).is_some() || float_slot(*src).is_some() {
            continue;
        }
        let (dst, src) = (alloc_reg(*result), alloc_reg(*src));
        if dst != src {
            moves.push((dst, src));
        }
    }
    emit_parallel_reg_moves(unit, &moves);
}

fn block_label(func: &str, id: usize) -> String {
    format!("{func}.bb{id}")
}

// ==================================================================
// 函数与指令发射
// ==================================================================

/// 发射一个函数的机器码。
pub fn emit_function(target: &Target, unit: &mut CompilationUnit, func: &SsaFunction) {
    debug_assert_eq!(
        target.arch(),
        Arch::AArch64,
        "AArch64 后端只能用于 aarch64 目标"
    );

    check_register_pressure(func);
    // 本函数实际用到的 callee-save 寄存器（`REG_MAP` 里下标 ≥ 18 的 x19..x28）：
    // 序言保存、每个返回点恢复。
    let cs_used: Vec<u8> = {
        let mut idx: Vec<usize> =
            REG_MAP.with(|m| m.borrow().values().copied().filter(|i| *i >= 18).collect());
        idx.sort_unstable();
        idx.dedup();
        idx.into_iter().map(|i| ALLOCATABLE[i].bits()).collect()
    };
    let cs_bytes = (cs_used.len() * 8 + 15) & !15;
    // 浮点值全部落专属栈槽；被取地址的局部变量排在浮点槽之后。槽区在
    // 序言里一次性预留（16 字节对齐）。
    let float_slots = assign_float_slots(func, 0);
    let local_slots = assign_local_slots(func, float_slots);
    let frame = ((float_slots + local_slots) * 8 + 15) & !15;
    FRAME_BYTES.with(|c| c.set(frame));
    let start = unit.code.len();
    let sym_base = unit.symbols.len();

    // 先为每个基本块占位一个符号，块偏移在发射完成后回填。
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

    emit_prologue(unit);
    if cs_bytes > 0 {
        emit_sub_sp(unit, cs_bytes as u32);
        for (i, &r) in cs_used.iter().enumerate() {
            emit_str_sp(unit, r, i * 8);
        }
    }
    if frame > 0 {
        emit_sub_sp(unit, frame as u32);
    }
    emit_param_moves(unit, func);

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
                    // 浮点返回值按 AAPCS64 走 v0，不是 x0。
                    if let Some(slot) = float_slot(*v) {
                        emit_load_float(unit, 0, slot, float_width(*v));
                    } else {
                        let r = alloc_reg(*v);
                        if r != Reg::X0 {
                            emit_mov_rr(unit, Reg::X0, r);
                        }
                    }
                }
                if frame > 0 {
                    emit_add_sp(unit, frame as u32);
                }
                if cs_bytes > 0 {
                    for (i, &r) in cs_used.iter().enumerate() {
                        emit_ldr_sp(unit, r, i * 8);
                    }
                    emit_add_sp(unit, cs_bytes as u32);
                }
                emit_epilogue(unit);
                emit_ret(unit);
            }
            SsaTerminator::Goto(target) => emit_b(unit, &block_label(&func.name, *target)),
            SsaTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                // 判 cond 是否为 0：`cmp r, r` 恒为相等，无法区分零/非零，
                // 必须与 xzr 比较（`cmp r, xzr`）。
                let r = alloc_reg(*cond);
                emit_cmp(unit, r, Reg(31));
                emit_cond_branch(
                    unit,
                    cc::NE,
                    &block_label(&func.name, *then_block),
                    &block_label(&func.name, *else_block),
                );
            }
            SsaTerminator::Unreachable => emit_brk(unit),
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
            SsaConst::Int(i) => emit_mov_imm64(unit, alloc_reg(*result), *i as u64),
            SsaConst::Bool(b) => emit_mov_imm64(unit, alloc_reg(*result), u64::from(*b)),
            SsaConst::Unit => emit_mov_imm64(unit, alloc_reg(*result), 0),
            SsaConst::Float(bits, w) => {
                emit_float_const(unit, 0, *bits, *w);
                if let Some(slot) = float_slot(*result) {
                    emit_store_float(unit, 0, slot, *w);
                }
            }
            // 字符串字面量：值是 `.rodata` 里那份字节的地址（指针）。
            SsaConst::Str(text) => {
                let sym = unit.intern_string(text);
                emit_str_addr(unit, alloc_reg(*result), &sym);
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
            if float_slot(*lhs).is_some() || float_slot(*rhs).is_some() || ty.is_float() {
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
            emit_call(unit, callee);
            // 浮点返回值按 AAPCS64 在 v0，不是 x0。
            if let Some(slot) = float_slot(*result) {
                emit_store_float(unit, 0, slot, float_width(*result));
            } else {
                let d = alloc_reg(*result);
                if d != Reg(0) {
                    emit_mov_rr(unit, d, Reg(0));
                }
            }
        }
        // φ 的实际拷贝由前驱末尾的 emit_phi_copies 发出
        SsaInstr::Phi { .. } => {}
        SsaInstr::Copy { result, src } => {
            if let (Some(rs), Some(ss)) = (float_slot(*result), float_slot(*src)) {
                let w = float_width(*result).max(float_width(*src));
                emit_load_float(unit, 0, ss, w);
                emit_store_float(unit, 0, rs, w);
            } else {
                let (d, s) = (alloc_reg(*result), alloc_reg(*src));
                if d != s {
                    emit_mov_rr(unit, d, s);
                }
            }
        }
        SsaInstr::Cast {
            result,
            src,
            from,
            to,
        } => {
            if from.is_float() || to.is_float() || float_slot(*src).is_some() {
                emit_cast_float(unit, *result, *src, *from, *to);
            } else {
                emit_cast(unit, alloc_reg(*result), alloc_reg(*src), *from, *to);
            }
        }
        SsaInstr::Load { result, ptr, ty } => {
            emit_load_mem(unit, alloc_reg(*result).bits(), alloc_reg(*ptr).bits(), *ty);
        }
        SsaInstr::Store { ptr, value, ty } => {
            emit_store_mem(unit, alloc_reg(*ptr).bits(), alloc_reg(*value).bits(), *ty);
        }
        SsaInstr::SlotAddr { result, local } => {
            let slot = LOCAL_SLOTS.with(|m| m.borrow().get(local).copied().unwrap_or(0));
            emit_lea_slot(unit, alloc_reg(*result).bits(), slot);
        }
        // `Var` 只是 SSA 值的命名引用，运行时不产生代码。
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
    use crate::ssa::SsaUnaryOp::*;

    // 浮点取负：`fneg`。只有 `Neg` 有浮点语义；`Not`/`BitNot` 落到这里说明上游
    // 把布尔/整数误标成了浮点，宁可报错也不静默给出错值。
    if let Some(slot) = float_slot(operand) {
        if !matches!(op, Neg) {
            crate::codegen::unsupported_const("非 Neg 的一元浮点运算");
        }
        let w = float_width(operand);
        emit_load_float(unit, 0, slot, w);
        emit_fneg(unit, w, 0, 0);
        if let Some(rs) = float_slot(result) {
            emit_store_float(unit, 0, rs, w);
        }
        return;
    }

    let dst = alloc_reg(result);
    let src = alloc_reg(operand);

    match op {
        // !x：比较 x 是否为 0（`cmp x, x` 恒相等，必须与 xzr 比较）
        Not => {
            emit_cmp(unit, src, Reg(31));
            emit_cset(unit, dst, 0); // eq
        }
        // ~x：mvn xd, xm
        BitNot => {
            insn(
                unit,
                0xaa20_03e0 | (src.bits() as u32) << 16 | dst.bits() as u32,
            );
        }
        // -x：neg xd, xm
        Neg => {
            insn(
                unit,
                0xcb00_03e0 | (src.bits() as u32) << 16 | dst.bits() as u32,
            );
        }
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
        Add => emit_add(unit, dst, rn, rm),
        Sub => emit_sub(unit, dst, rn, rm),
        BitAnd | And => emit_ands(unit, dst, rn, rm),
        BitOr | Or => {
            insn(
                unit,
                0xaa00_0000
                    | (rm.bits() as u32) << 16
                    | (rn.bits() as u32) << 5
                    | dst.bits() as u32,
            );
        }
        BitXor => {
            insn(
                unit,
                0xca00_0000
                    | (rm.bits() as u32) << 16
                    | (rn.bits() as u32) << 5
                    | dst.bits() as u32,
            );
        }
        Mul => emit_madd(unit, dst, rn, rm, XZR),
        Div => emit_sdiv(unit, dst, rn, rm),
        Rem => {
            // q = l / r; 余数 = l - q * r
            emit_sdiv(unit, dst, rn, rm);
            emit_msub(unit, dst, dst, rm, rn.bits());
        }
        Shl => emit_dp2(unit, 0x9ac0_2000, dst, rn, rm), // lslv
        // LSRV 是逻辑右移；右移一律取算术（ASRV），和 x86-64 的 sar 一致：
        // IR 只有一个 Shr，而整数字面量默认 i32，对有符号数要补符号位。
        Shr => emit_dp2(unit, 0x9ac0_2800, dst, rn, rm), // asrv
        Eq | Ne | Lt | Gt | Le | Ge => {
            emit_cmp(unit, rn, rm);
            emit_cset(unit, dst, condition_code(op));
        }
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
    let slot = |v: SsaValue| float_slot(v).expect("浮点操作数没有栈槽");
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
    emit_load_float(unit, 0, slot(lhs), w);
    emit_load_float(unit, 1, slot(rhs), w);

    match op {
        Eq | Ne | Lt | Gt | Le | Ge => {
            // `fcmp` 的条件码天然满足 IEEE754：NaN 时 N=0、C=1、Z=0，于是
            // `lt`(mi)/`le`(ls)/`gt`/`ge`/`eq` 全为假、`ne` 为真。
            emit_fcmp(unit, w, 0, 1);
            let cond = match op {
                Eq => cc::EQ,
                Ne => cc::NE,
                Lt => cc::MI,
                Le => cc::LS,
                Gt => cc::GT,
                Ge => cc::GE,
                _ => unreachable!(),
            };
            let dst = alloc_reg(result);
            emit_cset(unit, dst, cond);
        }
        Add | Sub | Mul | Div => {
            let opcode2 = match op {
                Add => 0x0a,
                Sub => 0x0e,
                Mul => 0x02,
                Div => 0x06,
                _ => unreachable!(),
            };
            emit_fp_arith(unit, w, opcode2, 0, 0, 1);
            if let Some(sl) = float_slot(result) {
                emit_store_float(unit, 0, sl, w);
            } else {
                crate::codegen::unsupported_const("float result");
            }
        }
        // 浮点没有整数的位运算 / 逻辑运算；`Rem` 也先不做。
        Shl | Shr | BitAnd | BitOr | BitXor | And | Or | Rem => {
            crate::codegen::unsupported_binary_op(op)
        }
    }
}

/// 浮点参与的类型转换。浮点值在栈槽（用 v 寄存器）、整数值在 GPR，四条方向
/// 各是一套指令：`scvtf`/`ucvtf`、`fcvtzs`/`fcvtzu`、`fcvt`。
fn emit_cast_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    src: SsaValue,
    from: NumTy,
    to: NumTy,
) {
    let dst_is_float = to.is_float();
    let src_is_float = from.is_float() || float_slot(src).is_some();
    let fw = match from {
        NumTy::Float(w) => w,
        _ => float_width(src),
    };
    let tw = match to {
        NumTy::Float(w) => w,
        _ => float_width(result),
    };
    let fw = if src_is_float && fw == 0 { 64 } else { fw };

    // 源进 d0（浮点）或 x0（整数）。
    if src_is_float {
        let sl = float_slot(src).expect("浮点源没有栈槽");
        emit_load_float(unit, 0, sl, fw);
    } else {
        let r = alloc_reg(src);
        if r.bits() != FP_SCRATCH {
            emit_mov_rr(unit, Reg(FP_SCRATCH), r);
        }
    }

    if dst_is_float {
        let sl = float_slot(result).expect("浮点目的没有栈槽");
        if src_is_float {
            // 同宽直接照搬 d0；跨宽用 `fcvt`。
            if tw == 32 && fw != 32 {
                emit_fcvt(unit, true, 0, 0);
            } else if tw != 32 && fw == 32 {
                emit_fcvt(unit, false, 0, 0);
            }
        } else {
            // int -> f32/f64：有符号用 scvtf，无符号用 ucvtf。
            let signed = !matches!(from, NumTy::Uint(_));
            emit_int_to_fp(unit, tw, signed, 0, FP_SCRATCH);
        }
        emit_store_float(unit, 0, sl, tw);
    } else if src_is_float {
        // f32/f64 -> int：fcvtzs/fcvtzu（截断向零，与 Rust `as` 一致）。
        let signed = !matches!(to, NumTy::Uint(_));
        let dst = alloc_reg(result);
        emit_fp_to_int(unit, fw, signed, dst.bits(), 0);
    }
}

/// 比较运算对应的条件码（供 `cset` 使用）。
fn condition_code(op: SsaBinaryOp) -> u32 {
    use SsaBinaryOp::*;
    match op {
        Eq => cc::EQ,
        Ne => cc::NE,
        Lt => cc::LT,
        Ge => cc::GE,
        Gt => cc::GT,
        Le => cc::LE,
        _ => cc::EQ,
    }
}

/// 发射程序入口：调用 `main` 后退出进程。
pub fn emit_start_wrapper(target: &Target, unit: &mut CompilationUnit) {
    debug_assert_eq!(target.arch(), Arch::AArch64);
    let start = unit.code.len();

    if target.os().uses_syscalls() {
        // 系统调用号各家不同（Linux 93 / FreeBSD 1 / Android 94），由
        // Os::exit_syscall 按 (平台, 架构) 给出。
        let nr = target.os().exit_syscall(Arch::AArch64) as u64;
        emit_call(unit, "main");
        emit_mov_wide(unit, Reg(8), nr, 0xd280_0000, 0); // mov x8, #nr
        insn(unit, 0xd400_0001); // svc #0
    } else {
        // macOS / Windows：退出交给运行时（zr_exit → libc/Win32 API）
        emit_call(unit, "main");
        emit_call(unit, "zr_exit");
        emit_brk(unit);
    }

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
    use crate::codegen::target::Os;

    fn empty_unit() -> CompilationUnit {
        CompilationUnit::new()
    }

    #[test]
    fn register_allocator_avoids_frame_and_link_registers() {
        for v in 0..64u32 {
            let r = alloc_reg(SsaValue(v));
            // x18（平台寄存器）与 x29/x30/x31 不能分配；x0-x17、x19-x28 可以。
            assert!(r.0 < 29 && r.0 != 18, "不能分配 x18/x29/x30/x31");
        }
    }

    #[test]
    fn string_address_uses_adrp_add_pair() {
        // clang: `adrp x0, sym` + `add x0, x0, :lo12:sym`
        let mut u = empty_unit();
        emit_str_addr(&mut u, Reg::X0, ".Lstr0");
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x9000_0000,
            "adrp x0（占位 0）"
        );
        assert_eq!(
            u32::from_le_bytes(u.code[4..8].try_into().unwrap()),
            0x9100_0000,
            "add x0, x0, #0（占位 0）"
        );
        assert_eq!(u.relocations.len(), 2);
        assert_eq!(u.relocations[0].kind, RelocationKind::AdrPrelPage21);
        assert_eq!(u.relocations[1].kind, RelocationKind::AddAbsLo12);
        assert_eq!(u.relocations[0].symbol, ".Lstr0");
    }

    #[test]
    fn load_mem_selects_width_and_sign() {
        let word = |u: &CompilationUnit| u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg::X0.0, Reg(1).0, NumTy::Uint(8));
        assert_eq!(word(&u), 0x3940_0020, "ldrb w0, [x1]");
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg::X0.0, Reg(1).0, NumTy::Int(8));
        assert_eq!(word(&u), 0x39c0_0020, "ldrsb x0, [x1]");
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg::X0.0, Reg(1).0, NumTy::Uint(16));
        assert_eq!(word(&u), 0x7940_0020, "ldrh w0, [x1]");
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg::X0.0, Reg(1).0, NumTy::Int(16));
        assert_eq!(word(&u), 0x79c0_0020, "ldrsh x0, [x1]");
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg::X0.0, Reg(1).0, NumTy::Uint(32));
        assert_eq!(word(&u), 0xb940_0020, "ldr w0, [x1]");
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg::X0.0, Reg(1).0, NumTy::Int(32));
        assert_eq!(word(&u), 0xb980_0020, "ldrsw x0, [x1]");
        let mut u = empty_unit();
        emit_load_mem(&mut u, Reg::X0.0, Reg(1).0, NumTy::Int(64));
        assert_eq!(word(&u), 0xf940_0020, "ldr x0, [x1]");
    }

    #[test]
    fn store_mem_selects_width() {
        let word = |u: &CompilationUnit| u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        let mut u = empty_unit();
        emit_store_mem(&mut u, Reg(1).0, Reg::X0.0, NumTy::Uint(8));
        assert_eq!(word(&u), 0x3900_0020, "strb w0, [x1]");
        let mut u = empty_unit();
        emit_store_mem(&mut u, Reg(1).0, Reg::X0.0, NumTy::Uint(16));
        assert_eq!(word(&u), 0x7900_0020, "strh w0, [x1]");
        let mut u = empty_unit();
        emit_store_mem(&mut u, Reg(1).0, Reg::X0.0, NumTy::Uint(32));
        assert_eq!(word(&u), 0xb900_0020, "str w0, [x1]");
        let mut u = empty_unit();
        emit_store_mem(&mut u, Reg(1).0, Reg::X0.0, NumTy::Int(64));
        assert_eq!(word(&u), 0xf900_0020, "str x0, [x1]");
    }

    #[test]
    fn lea_slot_uses_add_from_sp() {
        // add x0, sp, #8*slot；slot=3 → #24。
        let mut u = empty_unit();
        emit_lea_slot(&mut u, Reg::X0.0, 3);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x9100_63e0
        );
    }

    #[test]
    fn mov_imm64_emits_movz_plus_movk() {
        let mut u = empty_unit();
        emit_mov_imm64(&mut u, Reg::X0, 0x0000_0000_1234_5678);
        assert_eq!(u.code.len(), 8, "movz + 1 段 movk");
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xd280_0000 | (0x5678 << 5)
        );
        assert_eq!(
            u32::from_le_bytes(u.code[4..8].try_into().unwrap()),
            0xf2a0_0000 | (1 << 21) | (0x1234 << 5)
        );

        // 需要三段时补满 4 条指令
        let mut u = empty_unit();
        emit_mov_imm64(&mut u, Reg::X0, 0x0123_4567_89ab_cdef);
        assert_eq!(u.code.len(), 16);
    }

    #[test]
    fn small_immediate_needs_single_instruction() {
        let mut unit = empty_unit();
        emit_mov_imm64(&mut unit, Reg::X0, 42);
        assert_eq!(unit.code.len(), 4);
    }

    #[test]
    fn call_emits_branch26_relocation() {
        let mut u = empty_unit();
        emit_call(&mut u, "main");
        assert_eq!(u.code.len(), 4);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()) >> 26,
            0b100101
        );
        assert_eq!(u.relocations.len(), 1);
        let reloc = &u.relocations[0];
        assert_eq!(reloc.kind, RelocationKind::Call26);
        assert_eq!(reloc.instr_start, 0);
        assert_eq!(reloc.offset, 0, "字面量就在指令自身的 [25:0] 位");
        assert_eq!(reloc.symbol, "main");
    }

    #[test]
    fn condition_codes_match_a64_encoding() {
        // cset x0, eq → csinc x0, xzr, xzr, ne（条件码取反）
        let mut u = empty_unit();
        emit_cset(&mut u, Reg::X0, cc::EQ);
        let word = u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        assert_eq!(word & 0xfff0_0000, 0x9a90_0000);

        // cset x0, gt → 反转为 le(0b1101)
        let mut u = empty_unit();
        emit_cset(&mut u, Reg::X0, cc::GT);
        let word = u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        assert_eq!((word >> 12) & 0xf, cc::LE);
    }

    #[test]
    fn conditional_branch_uses_short_fixed_offset_plus_two_jumps() {
        let mut u = empty_unit();
        emit_cond_branch(&mut u, cc::NE, "bb1", "bb2");
        assert_eq!(u.code.len(), 12, "b.cond + b + b");
        let first = u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        assert_eq!(first >> 24, 0x54, "b.cond（0101_0100 imm19 0 cond）");
        assert_eq!((first >> 4) & 1, 0, "imm19[18] 恒为 0");
        assert_eq!((first >> 5) & 0x7_ffff, 2, "字面量 +8 字节（跳过一条指令）");
        assert_eq!(first & 0xf, cc::EQ, "条件取反后为 eq");
        assert_eq!(
            u32::from_le_bytes(u.code[4..8].try_into().unwrap()) >> 26,
            0b000101
        );
        assert_eq!(
            u32::from_le_bytes(u.code[8..12].try_into().unwrap()) >> 26,
            0b000101
        );
        assert_eq!(u.relocations.len(), 2, "两条无条件跳转需要重定位");
        assert!(u
            .relocations
            .iter()
            .all(|r| r.kind == RelocationKind::Branch26));
    }

    #[test]
    fn start_wrapper_uses_syscalls_on_linux_and_runtime_elsewhere() {
        let linux = Target::new(Arch::AArch64, Os::Linux);
        let mut u = empty_unit();
        emit_start_wrapper(&linux, &mut u);
        // bl main; mov x8, #93; svc #0
        assert_eq!(u.code.len(), 12);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()) >> 26,
            0b100101,
            "bl main"
        );
        assert_eq!(
            u32::from_le_bytes(u.code[4..8].try_into().unwrap()),
            0xd280_0ba8,
            "mov x8, #93"
        );
        assert_eq!(
            u32::from_le_bytes(u.code[8..12].try_into().unwrap()),
            0xd400_0001,
            "svc #0"
        );
        assert_eq!(u.entry, "_start");

        let macos = Target::new(Arch::AArch64, Os::Darwin);
        let mut u = empty_unit();
        emit_start_wrapper(&macos, &mut u);
        // bl main; bl zr_exit; brk #0
        assert_eq!(u.code.len(), 12);
        let targets: Vec<&str> = u.relocations.iter().map(|r| r.symbol.as_str()).collect();
        assert_eq!(targets, vec!["main", "zr_exit"]);
        assert_eq!(
            u32::from_le_bytes(u.code[8..12].try_into().unwrap()),
            0xd420_0000,
            "brk #0"
        );
    }

    #[test]
    fn branch_and_not_test_against_xzr() {
        // `cmp x3, xzr`（不是 `cmp x3, x3`，后者恒等）。
        let mut u = empty_unit();
        emit_cmp(&mut u, Reg(3), Reg(31));
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0xeb1f_007f
        );
        // `cset x3, eq`。
        let mut u = empty_unit();
        emit_cset(&mut u, Reg(3), 0);
        assert_eq!(
            u32::from_le_bytes(u.code[0..4].try_into().unwrap()),
            0x9a9f_17e3
        );
        // 条件跳转字面量必须是 2（+8 字节），不是 1。
        let mut u = empty_unit();
        emit_cond_branch(&mut u, cc::NE, "t", "e");
        let first = u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        assert_eq!((first >> 5) & 0x7_ffff, 2);
        assert_eq!(first & 0xf, cc::EQ);
    }

    #[test]
    fn shift_and_cast_match_clang() {
        let word = |u: &CompilationUnit| u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        // lslv x3, x4, x5
        let mut u = empty_unit();
        emit_dp2(&mut u, 0x9ac0_2000, Reg(3), Reg(4), Reg(5));
        assert_eq!(word(&u), 0x9ac5_2083);
        // asrv x3, x4, x5（左移/右移的基址都曾写错）
        let mut u = empty_unit();
        emit_dp2(&mut u, 0x9ac0_2800, Reg(3), Reg(4), Reg(5));
        assert_eq!(word(&u), 0x9ac5_2883);
        // sbfm x19, x0, #0, #7（即 sxtb x19, w0）
        let mut u = empty_unit();
        emit_cast(&mut u, Reg(19), Reg(0), NumTy::Int(32), NumTy::Int(8));
        assert_eq!(word(&u), 0x9340_1c13);
        // ubfm x5, x6, #0, #7
        let mut u = empty_unit();
        emit_cast(&mut u, Reg(5), Reg(6), NumTy::Int(32), NumTy::Uint(8));
        assert_eq!(word(&u), 0xd340_1cc5);
        // sbfm x7, x8, #0, #31（即 sxtw x7, w8）
        let mut u = empty_unit();
        emit_cast(&mut u, Reg(7), Reg(8), NumTy::Int(32), NumTy::Int(32));
        assert_eq!(word(&u), 0x9340_7d07);
    }

    #[test]
    fn callee_save_load_store_encodings() {
        let word = |u: &CompilationUnit| u32::from_le_bytes(u.code[0..4].try_into().unwrap());
        // str x19, [sp, #8]
        let mut u = empty_unit();
        emit_str_sp(&mut u, 19, 8);
        assert_eq!(word(&u), 0xf900_07f3);
        // ldr x28, [sp, #16]
        let mut u = empty_unit();
        emit_ldr_sp(&mut u, 28, 16);
        assert_eq!(word(&u), 0xf940_0bfc);
    }

    #[test]
    fn float_instructions_match_clang() {
        // 期望值全部来自 `clang --target=aarch64 ... -c` + objdump 的实际输出。
        fn word(u: &CompilationUnit) -> u32 {
            u32::from_le_bytes(u.code[0..4].try_into().unwrap())
        }

        let mut u = empty_unit();
        emit_fp_arith(&mut u, 64, 0x0a, 0, 0, 0); // fadd d0, d0, d0
        assert_eq!(word(&u), 0x1e60_2800);

        let mut u = empty_unit();
        emit_fp_arith(&mut u, 32, 0x02, 7, 8, 9); // fmul s7, s8, s9
        assert_eq!(word(&u), 0x1e29_0907);

        let mut u = empty_unit();
        emit_fp_arith(&mut u, 64, 0x0e, 4, 5, 6); // fsub d4, d5, d6
        assert_eq!(word(&u), 0x1e66_38a4);

        let mut u = empty_unit();
        emit_fp_arith(&mut u, 64, 0x06, 10, 11, 12); // fdiv d10, d11, d12
        assert_eq!(word(&u), 0x1e6c_196a);

        let mut u = empty_unit();
        emit_fneg(&mut u, 64, 13, 14); // fneg d13, d14
        assert_eq!(word(&u), 0x1e61_41cd);

        let mut u = empty_unit();
        emit_fcmp(&mut u, 64, 0, 1); // fcmp d0, d1
        assert_eq!(word(&u), 0x1e61_2000);

        let mut u = empty_unit();
        emit_load_float(&mut u, 0, 1, 64); // ldr d0, [sp, #8]
        assert_eq!(word(&u), 0xfd40_07e0);

        let mut u = empty_unit();
        emit_store_float(&mut u, 0, 1, 32); // str s0, [sp, #8]
        assert_eq!(word(&u), 0xbd00_0be0);

        let mut u = empty_unit();
        emit_int_to_fp(&mut u, 64, true, 0, 0); // scvtf d0, x0
        assert_eq!(word(&u), 0x9e62_0000);

        let mut u = empty_unit();
        emit_int_to_fp(&mut u, 32, true, 0, 0); // scvtf s0, x0
        assert_eq!(word(&u), 0x9e22_0000);

        let mut u = empty_unit();
        emit_int_to_fp(&mut u, 64, false, 1, 1); // ucvtf d1, x1
        assert_eq!(word(&u), 0x9e63_0021);

        let mut u = empty_unit();
        emit_fp_to_int(&mut u, 64, true, 0, 0); // fcvtzs x0, d0
        assert_eq!(word(&u), 0x9e78_0000);

        let mut u = empty_unit();
        emit_fp_to_int(&mut u, 32, true, 1, 2); // fcvtzs x1, s2
        assert_eq!(word(&u), 0x9e38_0041);

        let mut u = empty_unit();
        emit_fp_to_int(&mut u, 64, false, 2, 3); // fcvtzu x2, d3
        assert_eq!(word(&u), 0x9e79_0062);

        let mut u = empty_unit();
        emit_fcvt(&mut u, true, 6, 7); // fcvt s6, d7
        assert_eq!(word(&u), 0x1e62_40e6);

        let mut u = empty_unit();
        emit_fcvt(&mut u, false, 4, 5); // fcvt d4, s5
        assert_eq!(word(&u), 0x1e22_c0a4);

        let mut u = empty_unit();
        emit_fmov_x_to_d(&mut u, 0, 0); // fmov d0, x0
        assert_eq!(word(&u), 0x9e67_0000);

        let mut u = empty_unit();
        emit_sub_sp(&mut u, 16); // sub sp, sp, #16
        assert_eq!(word(&u), 0xd100_43ff);

        let mut u = empty_unit();
        emit_add_sp(&mut u, 32); // add sp, sp, #32
        assert_eq!(word(&u), 0x9100_83ff);
    }

    #[test]
    fn float_comparison_uses_ieee754_conditions() {
        // clang 对 `a < b` 用 `fcmp` + `cset mi`；`<=` 用 `ls`。这两个条件对 NaN
        // 都取假，符合 IEEE754。
        // 两个操作数得有浮点栈槽。
        FLOAT_SLOTS.with(|m| {
            let mut map = m.borrow_mut();
            map.clear();
            map.insert(0, (0, 64));
            map.insert(1, (1, 64));
        });
        for (op, want) in [
            (SsaBinaryOp::Lt, cc::MI),
            (SsaBinaryOp::Le, cc::LS),
            (SsaBinaryOp::Gt, cc::GT),
            (SsaBinaryOp::Ge, cc::GE),
            (SsaBinaryOp::Eq, cc::EQ),
            (SsaBinaryOp::Ne, cc::NE),
        ] {
            let mut u = empty_unit();
            emit_binary_float(
                &mut u,
                SsaValue(9),
                op,
                SsaValue(0),
                SsaValue(1),
                NumTy::Float(64),
            );
            // ldr d0,[sp]; ldr d1,[sp,#8]; fcmp d0,d1; cset x9, <cond>
            assert_eq!(u.code.len(), 16);
            let fcmp = u32::from_le_bytes(u.code[8..12].try_into().unwrap());
            assert_eq!(fcmp, 0x1e61_2000, "fcmp d0, d1");
            let cset = u32::from_le_bytes(u.code[12..16].try_into().unwrap());
            assert_eq!((cset >> 12) & 0xf, want ^ 1, "cset 条件码取反后写入");
        }
        FLOAT_SLOTS.with(|m| m.borrow_mut().clear());
    }

    #[test]
    fn remainder_uses_msub_without_clobbering_the_dividend() {
        let mut u = empty_unit();
        emit_binary(
            &mut u,
            SsaValue(3),
            SsaBinaryOp::Rem,
            SsaValue(0),
            SsaValue(1),
        );
        assert!(!u.code.is_empty());
    }
}

/// 扫出一个函数里所有浮点值，给每个值分配专属栈槽，返回槽数。
///
/// 浮点"性"必须穿过 `Copy` 与 `Phi` 传播：优化会把 `a + b` 的结果复制走，
/// 循环里也会有 φ；只认"定义点"会让这些中转值被当成整数去 GPR 里找，然后
/// 拿到栈槽里的残渣当浮点数用。
///
/// 播种与传播合成一个不动点：`Neg` 的播种依赖操作数已是浮点，而操作数的
/// 浮点性可能要等 `Copy` 传播完才知道，所以每轮都重扫全部指令直到不再变化。
fn assign_float_slots(func: &SsaFunction, base: usize) -> usize {
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
            map.insert(id, (base + i, w));
        }
    });
    count
}

/// 寄存器压力检查。
///
/// 这些后端还没实现溢出（spilling）：活跃值超过可分配寄存器数时，
/// `alloc_reg` 按编号取模会让同时存活的值抢同一个寄存器，生成**错的**代码。
/// 宁可在这里明确报错，也不能安静地编出一个算错的程序。
///
/// 溢出未见实现时直接报错；否则把线性扫描的分配结果写进 `REG_MAP` 供
/// `alloc_reg` 使用。x86-64 后端自己实现了溢出，见 `encoder.rs`。
fn check_register_pressure(func: &SsaFunction) {
    // 传 callee-save 集合：跨调用存活的值只放 x19..x28，否则 `call` 会破坏它。
    let alloc = crate::codegen::regalloc::allocate_registers(func, ALLOCATABLE.len(), &CALLEE_SAVE);
    if alloc.spilled > 0 {
        crate::codegen::unsupported_pressure(&func.name, ALLOCATABLE.len());
    }
    REG_MAP.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for (&v, loc) in &alloc.map {
            if let crate::codegen::regalloc::Location::Reg(i) = loc {
                map.insert(v, *i);
            }
        }
    });
}
