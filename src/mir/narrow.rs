//! 窄浮点（`f8` / `f16`）的位模式转换。
//!
//! 窄浮点在**寄存器里以 f32 形态存放**——算术与比较都按 f32 做，只有落
//! 内存时才按窄位宽打包。所以这里只负责两件事：
//!
//! - [`f32_to_narrow_bits`]：把 f32 的值舍入成窄格式，返回窄位模式；
//! - [`narrow_bits_to_f32`]：把窄位模式还原成 f32 的值。
//!
//! 两个方向都在**整数域**完成（位搬运 + 舍入），因此对所有后端都是同一
//! 个结果，不依赖任何一条浮点指令。f32 与整数之间的按位重解释借用了
//! [`crate::mir::builder`] 里的内存往返：把值先存进一个临时栈槽，再按另
//! 一种宽度读回来——字节完全一样，解释方式变了。
//!
//! 格式定义：
//!
//! | 类型 | 位宽 | 符号 | 指数 | 尾数 | 偏置 |
//! |------|------|------|------|------|------|
//! | `f16` | 16 | 1 | 5 | 10 | 15（IEEE 754 binary16）|
//! | `f8` | 8 | 1 | 4 | 3 | 7（本语言自定义）|
//!
//! 规格化数的取值范围是 `2^(1-bias)` 到 `2^(max_exp-1-bias)`；次正规数按
//! "隐含位为 0"解释；指数全 1 表示 Inf / NaN。

use std::sync::atomic::{AtomicU8, Ordering};

use crate::mir::{
    MirBasicBlock, MirBinaryOp, MirFunction, MirInstr, MirParam, MirTerminator, MirValue, NumTy,
};

use super::builder::alloc_synthetic_slot;

/// 用到了窄浮点的位宽集合（位 0 = f8，位 1 = f16）。
///
/// 转换辅助函数只在真的用到时才加进模块，免得每个程序都白背一份。
static USED_WIDTHS: AtomicU8 = AtomicU8::new(0);

/// 记下某个位宽的窄浮点被用到了。
pub(super) fn mark_used(bits: u8) {
    let bit = if bits == 8 { 1 } else { 2 };
    USED_WIDTHS.fetch_or(bit, Ordering::Relaxed);
}

/// 用到的窄浮点位宽。
pub(super) fn used_widths() -> Vec<u8> {
    let mask = USED_WIDTHS.load(Ordering::Relaxed);
    let mut out = Vec::new();
    if mask & 1 != 0 {
        out.push(8);
    }
    if mask & 2 != 0 {
        out.push(16);
    }
    out
}

/// 转换函数的发射上下文：指令列表 + 临时量编号。
///
/// 转换逻辑被编成**独立的辅助函数**（见 [`helper_functions`]），调用点只留
/// 一条 `call`。这样每个调用点的栈帧不会随转换的指令条数膨胀——窄浮点的
/// 转换有上百条指令，放在调用点里会把 32 位目标的栈帧撑爆。
pub(super) struct Emitter {
    instrs: Vec<MirInstr>,
    next: u32,
}

impl Emitter {
    /// 新建一个发射上下文。
    pub(super) fn new() -> Self {
        Emitter {
            instrs: Vec::new(),
            next: 0,
        }
    }

    /// 新的临时量名。
    fn temp(&mut self) -> String {
        let t = self.next;
        self.next += 1;
        format!("t{t}")
    }

    /// 追加一条指令。
    fn push(&mut self, instr: MirInstr) {
        self.instrs.push(instr);
    }
}

/// 窄浮点格式参数。
#[derive(Debug, Clone, Copy)]
pub(super) struct NarrowFormat {
    /// 指数位宽。
    pub(super) exp_bits: u8,
    /// 尾数位宽。
    pub(super) mant_bits: u8,
    /// 指数偏置。
    pub(super) bias: i32,
}

impl NarrowFormat {
    /// 指数掩码（`2^exp_bits - 1`）。
    fn exp_mask(self) -> i128 {
        (1i128 << self.exp_bits) - 1
    }

    /// 尾数掩码（`2^mant_bits - 1`）。
    fn mant_mask(self) -> i128 {
        (1i128 << self.mant_bits) - 1
    }

    /// 最大的规格化指数（无偏置）。
    fn max_unbiased(self) -> i32 {
        // 指数全 1 留给 Inf / NaN，所以最大的规格化指数字段是 `2^E - 2`。
        (self.exp_mask() - 1) as i32 - self.bias
    }

    /// 最小的规格化指数（无偏置）。
    fn min_unbiased(self) -> i32 {
        1 - self.bias
    }

    /// 规格化数要丢弃的尾数位数。
    fn mant_shift(self) -> i32 {
        23 - self.mant_bits as i32
    }

    /// 窄格式里符号位所在的位置。
    fn sign_shift(self) -> i32 {
        self.exp_bits as i32 + self.mant_bits as i32
    }
}

/// 取指定位宽的窄浮点格式。
pub(super) fn format_for(bits: u8) -> NarrowFormat {
    if bits == 16 {
        NarrowFormat {
            exp_bits: 5,
            mant_bits: 10,
            bias: 15,
        }
    } else {
        NarrowFormat {
            exp_bits: 4,
            mant_bits: 3,
            bias: 7,
        }
    }
}

// ==================================================================
// 整数域的小工具
// ==================================================================

/// 发一条整数二元运算。
fn op(e: &mut Emitter, l: MirValue, o: MirBinaryOp, r: MirValue, ty: NumTy) -> MirValue {
    let name = e.temp();
    e.push(MirInstr::Binary(name.clone(), l, o, r, ty));
    MirValue::Var(name)
}

/// 发一条"右操作数是常量"的整数二元运算。
fn op_imm(e: &mut Emitter, l: MirValue, o: MirBinaryOp, r: i128, ty: NumTy) -> MirValue {
    op(e, l, o, MirValue::Int(r), ty)
}

/// 无符号 32 位运算。
fn uop(e: &mut Emitter, l: MirValue, o: MirBinaryOp, r: MirValue) -> MirValue {
    op(e, l, o, r, NumTy::Uint(32))
}

/// 有符号 32 位运算。
fn sop(e: &mut Emitter, l: MirValue, o: MirBinaryOp, r: MirValue) -> MirValue {
    op(e, l, o, r, NumTy::Int(32))
}

/// 有符号 32 位运算（右操作数是常量）。
fn sop_imm(e: &mut Emitter, l: MirValue, o: MirBinaryOp, r: i128) -> MirValue {
    sop(e, l, o, MirValue::Int(r))
}

/// 发一条整数转浮点的**值**转换（`cvtsi2ss` 那一类）。
fn cast_int_to_float(e: &mut Emitter, value: MirValue, from: NumTy, to: NumTy) -> MirValue {
    let name = e.temp();
    e.push(MirInstr::Cast(name.clone(), value, from, to));
    MirValue::Var(name)
}

/// 发一条浮点二元运算。
fn float_binary(e: &mut Emitter, l: MirValue, o: MirBinaryOp, r: MirValue, ty: NumTy) -> MirValue {
    let name = e.temp();
    e.push(MirInstr::Binary(name.clone(), l, o, r, ty));
    MirValue::Var(name)
}

/// `cond ? a : other`。
///
/// `cond` 是比较运算的结果（0 或 1）。做法是把它展开成"全 0 / 全 1"的
/// 掩码，再用掩码挑值：
///
/// ```text
/// mask = cond * (-1)          // cond 为 1 时是 0xFFFF…FFFF
/// pick = (a & mask) | (other & ~mask)
/// ```
///
/// 掩码展开用的是 32 位运算：转换里的量都是 32 位（窄浮点的位宽本来就
/// 不超过 16 位），32 位目标上也不会因此走进"多字长"那条路径。
fn select(e: &mut Emitter, cond: MirValue, a: MirValue, other: MirValue) -> MirValue {
    let mask = op_imm(e, cond, MirBinaryOp::Mul, -1, NumTy::Int(32));
    let inv = op_imm(e, mask.clone(), MirBinaryOp::BitXor, -1, NumTy::Int(32));
    let hi = op(e, a, MirBinaryOp::BitAnd, mask, NumTy::Int(32));
    let lo = op(e, other, MirBinaryOp::BitAnd, inv, NumTy::Int(32));
    op(e, hi, MirBinaryOp::BitOr, lo, NumTy::Int(32))
}

/// `max(a, other)`（有符号 32 位比较）。
fn max_s(e: &mut Emitter, a: MirValue, other: MirValue) -> MirValue {
    let gt = op(e, a.clone(), MirBinaryOp::Gt, other.clone(), NumTy::Int(32));
    select(e, gt, a, other)
}

/// `min(a, other)`（有符号 32 位比较）。
fn min_s(e: &mut Emitter, a: MirValue, other: MirValue) -> MirValue {
    let lt = op(e, a.clone(), MirBinaryOp::Lt, other.clone(), NumTy::Int(32));
    select(e, lt, a, other)
}

/// 把 f32 的位模式读成整数。
///
/// 位模式重解释在 IR 里没有对应指令，借内存往返实现：先按 f32 存进一个
/// 临时栈槽，再按 u32 读回来——读到的字节就是那个 f32 的位模式。
pub(super) fn f32_to_bits(e: &mut Emitter, value: MirValue) -> MirValue {
    let slot = alloc_slot(e);
    let raw = e.temp();
    e.push(MirInstr::Store(
        MirValue::Var(slot.clone()),
        value,
        NumTy::Float(32),
    ));
    e.push(MirInstr::Load(
        raw.clone(),
        MirValue::Var(slot),
        NumTy::Uint(32),
    ));
    MirValue::Var(raw)
}

/// 把 32 位整数按位重解释成 f32。
pub(super) fn bits_to_f32(e: &mut Emitter, bits: MirValue) -> MirValue {
    let slot = alloc_slot(e);
    let out = e.temp();
    e.push(MirInstr::Store(
        MirValue::Var(slot.clone()),
        bits,
        NumTy::Uint(32),
    ));
    e.push(MirInstr::Load(
        out.clone(),
        MirValue::Var(slot),
        NumTy::Float(32),
    ));
    MirValue::Var(out)
}

/// 分配一块合成栈槽，返回它的地址临时名。
fn alloc_slot(e: &mut Emitter) -> String {
    let id = alloc_synthetic_slot();
    let name = format!("s{id}");
    e.push(MirInstr::SlotAddr(name.clone(), id, 8));
    name
}

// ==================================================================
// f32 → 窄位模式
// ==================================================================

/// 把 f32 的值舍入成窄格式，返回窄位模式（整数）。
///
/// 做法：
/// 1. 拆出符号 / 指数 / 尾数；
/// 2. 指数换算到目标格式（加上偏置差），尾数按目标宽度右移；
/// 3. 丢掉的部分做"就近偶数"舍入；
/// 4. 越界的按 Inf / ±0 处理，指数全 1 的按 Inf / NaN 处理。
///
/// 规格化数与次正规数用**同一个移位量**：`max(23-M, 24-bias-M-e)`。
/// 次正规数的尾数要左移才能规格化，等价于把移位量调大；两者在最小规格化
/// 指数处恰好相等，所以一个 `max` 就够了。
pub(super) fn f32_to_narrow_bits(em: &mut Emitter, value: MirValue, bits: u8) -> MirValue {
    let f = format_for(bits);
    let raw = f32_to_bits(em, value);

    // 拆位。移位是算术右移（IR 的 `Shr` 是有符号的），所以拆出来的字段
    // 一律再掩一次——取低位不受符号扩展影响。
    let sign = uop(em, raw.clone(), MirBinaryOp::Shr, MirValue::Int(31));
    let sign = op_imm(em, sign, MirBinaryOp::BitAnd, 1, NumTy::Uint(32));
    let exp = uop(em, raw.clone(), MirBinaryOp::Shr, MirValue::Int(23));
    let exp = op_imm(em, exp, MirBinaryOp::BitAnd, 0xff, NumTy::Uint(32));
    let mant = op_imm(em, raw, MirBinaryOp::BitAnd, 0x7f_ffff, NumTy::Uint(32));

    // 无偏置指数。`exp` 只有 0..255，作为有符号数解释没问题。
    let unbiased = sop_imm(em, exp.clone(), MirBinaryOp::Sub, 127);

    // 指数下界：再小就彻底下溢成 ±0。取这个下界是为了让移位量不超过 31
    // ——x86 的移位计数只取低 5 位，移 32 位会变成移 0 位。
    let e_min = -7 - f.bias - f.mant_bits as i32;
    let e_c = max_s(em, unbiased.clone(), MirValue::Int(e_min as i128));

    // 移位量：规格化用 `23-M`，次正规用 `24-bias-M-e`。
    let shift_sub = sop(
        em,
        MirValue::Int((24 - f.bias - f.mant_bits as i32) as i128),
        MirBinaryOp::Sub,
        e_c.clone(),
    );
    let shift = max_s(em, MirValue::Int(f.mant_shift() as i128), shift_sub);

    // 尾数：带上隐含位一起右移。`full` 落在 2^24 以内，是正数，所以算术
    // 右移与逻辑右移等价。
    let full = op_imm(em, mant, MirBinaryOp::BitOr, 0x80_0000, NumTy::Uint(32));
    let shifted = uop(em, full.clone(), MirBinaryOp::Shr, shift.clone());

    // 规格化数的尾数里带着隐含位，次正规数不带——隐含位在不在，取决于
    // 目标格式里这个数算不算规格化。
    let is_normal = sop(em, e_c.clone(), MirBinaryOp::Ge, MirValue::Int(f.min_unbiased() as i128));
    let implicit = op_imm(
        em,
        is_normal,
        MirBinaryOp::Mul,
        1i128 << f.mant_bits as i128,
        NumTy::Int(32),
    );
    let frac = op(em, shifted, MirBinaryOp::Sub, implicit, NumTy::Int(32));

    // 就近偶数舍入：看被丢掉的最高位（round bit）与更低位是否非零（sticky）。
    let drop = op_imm(em, shift.clone(), MirBinaryOp::Sub, 1, NumTy::Uint(32));
    let round_bit = uop(em, full.clone(), MirBinaryOp::Shr, drop.clone());
    let round_bit = op_imm(em, round_bit, MirBinaryOp::BitAnd, 1, NumTy::Uint(32));
    let sticky_mask = op(
        em,
        MirValue::Int(1),
        MirBinaryOp::Shl,
        drop,
        NumTy::Uint(32),
    );
    let sticky_mask = op_imm(em, sticky_mask, MirBinaryOp::Sub, 1, NumTy::Uint(32));
    let sticky = uop(em, full, MirBinaryOp::BitAnd, sticky_mask);
    let sticky = op(em, sticky, MirBinaryOp::Ne, MirValue::Int(0), NumTy::Uint(32));
    let lsb = op_imm(em, frac.clone(), MirBinaryOp::BitAnd, 1, NumTy::Int(32));
    let tie = op_imm(em, round_bit, MirBinaryOp::BitAnd, 1, NumTy::Uint(32));
    let cond = op(em, sticky, MirBinaryOp::BitOr, lsb, NumTy::Int(32));
    let inc = op(em, cond, MirBinaryOp::BitAnd, tie, NumTy::Int(32));
    let frac = op(em, frac, MirBinaryOp::Add, inc, NumTy::Int(32));

    // 舍入进位：尾数装不下了就进到指数位（"变成下一个数"，或者从最大的
    // 规格化数进成 Inf）。
    let carry = uop(em, frac.clone(), MirBinaryOp::Shr, MirValue::Int(f.mant_bits as i128));
    let carry = op_imm(em, carry, MirBinaryOp::BitAnd, 1, NumTy::Uint(32));
    let frac = op_imm(em, frac, MirBinaryOp::BitAnd, f.mant_mask(), NumTy::Int(32));

    // 指数字段：加偏置差，负数归零（次正规数），过大归到全 1（Inf）。
    let e_field = sop_imm(em, e_c, MirBinaryOp::Add, f.bias as i128);
    let e_field = max_s(em, e_field, MirValue::Int(0));
    let e_field = min_s(em, e_field, MirValue::Int(f.exp_mask()));
    let e_field = op(em, e_field, MirBinaryOp::Add, carry, NumTy::Int(32));
    let e_field = min_s(em, e_field, MirValue::Int(f.exp_mask()));

    // 上溢：比目标格式能表示的最大指数还大 —— 结果是 Inf，尾数要清零。
    // 指数全 1（源就是 Inf / NaN）不算上溢，NaN 要保留载荷。
    let too_big = sop(em, unbiased, MirBinaryOp::Gt, MirValue::Int(f.max_unbiased() as i128));
    let not_special = op(em, exp, MirBinaryOp::Ne, MirValue::Int(0xff), NumTy::Uint(32));
    let overflow = op(em, too_big, MirBinaryOp::BitAnd, not_special, NumTy::Uint(32));
    let frac = select(em, overflow, MirValue::Int(0), frac);

    // 合成：符号 | 指数 | 尾数。
    let sign = uop(em, sign, MirBinaryOp::Shl, MirValue::Int(f.sign_shift() as i128));
    let e_field = uop(
        em,
        e_field,
        MirBinaryOp::Shl,
        MirValue::Int(f.mant_bits as i128),
    );
    let out = op(em, sign, MirBinaryOp::BitOr, e_field, NumTy::Uint(32));
    op(em, out, MirBinaryOp::BitOr, frac, NumTy::Int(32))
}

// ==================================================================
// 窄位模式 → f32
// ==================================================================

/// 把窄位模式还原成 f32 的值。
///
/// 做法比反向转换简单，因为可以借助浮点乘法：
///
/// ```text
/// 规格化数： (2^M + mant) * 2^(exp - bias - M)
/// 次正规数： mant * 2^(1 - bias - M)
/// ```
///
/// 两者都是"整数 × 2 的幂"，而 `2^k` 可以直接按位模式拼出来（`k + 127`
/// 放在指数字段里），所以整个转换只需要一次整数转浮点、一次浮点乘法。
/// 次正规数不必规格化，也就不用数前导零。
///
/// Inf / NaN 同样走这条路：`2^128` 在 f32 里就是 Inf，于是
/// `1 * Inf = Inf` 是 Inf，`0 * Inf = NaN` 是 NaN。
pub(super) fn narrow_bits_to_f32(em: &mut Emitter, bits: MirValue, width: u8) -> MirValue {
    let f = format_for(width);

    // 拆位。
    let sign = uop(em, bits.clone(), MirBinaryOp::Shr, MirValue::Int(f.sign_shift() as i128));
    let sign = op_imm(em, sign, MirBinaryOp::BitAnd, 1, NumTy::Uint(32));
    let exp = uop(
        em,
        bits.clone(),
        MirBinaryOp::Shr,
        MirValue::Int(f.mant_bits as i128),
    );
    let exp = op_imm(em, exp, MirBinaryOp::BitAnd, f.exp_mask(), NumTy::Uint(32));
    let mant = op_imm(em, bits, MirBinaryOp::BitAnd, f.mant_mask(), NumTy::Uint(32));

    // 指数分类：全 0 是零 / 次正规，全 1 是 Inf / NaN。
    let exp_is_zero = op(em, exp.clone(), MirBinaryOp::Eq, MirValue::Int(0), NumTy::Uint(32));
    let exp_is_max = op(
        em,
        exp.clone(),
        MirBinaryOp::Eq,
        MirValue::Int(f.exp_mask()),
        NumTy::Uint(32),
    );
    let mant_is_zero = op(em, mant.clone(), MirBinaryOp::Eq, MirValue::Int(0), NumTy::Uint(32));

    // 被乘的那个整数：
    //   指数全 0（零 / 次正规）：mant
    //   规格化：2^M + mant（隐含位补上）
    //   Inf：1（1 * Inf = Inf）
    //   NaN：0（0 * Inf = NaN）
    let normal_sig = op_imm(
        em,
        mant.clone(),
        MirBinaryOp::BitOr,
        1i128 << f.mant_bits as i128,
        NumTy::Uint(32),
    );
    // 指数全 0（零 / 次正规）：被乘的整数就是尾数本身。
    let sig = select(em, exp_is_zero.clone(), mant.clone(), normal_sig);
    // Inf：尾数为 0；NaN：尾数非 0。
    let mant_nz = op(
        em,
        mant_is_zero.clone(),
        MirBinaryOp::Eq,
        MirValue::Int(0),
        NumTy::Uint(32),
    );
    let is_inf = op(
        em,
        exp_is_max.clone(),
        MirBinaryOp::BitAnd,
        mant_is_zero.clone(),
        NumTy::Uint(32),
    );
    let is_nan = op(
        em,
        exp_is_max.clone(),
        MirBinaryOp::BitAnd,
        mant_nz,
        NumTy::Uint(32),
    );
    // Inf：1 * Inf = Inf。NaN：0 * Inf = NaN。
    let sig = select(em, is_inf, MirValue::Int(1), sig);
    let sig = select(em, is_nan, MirValue::Int(0), sig);

    // 2 的幂的指数：
    //   指数全 0：1 - bias - M（次正规数；零取什么都无所谓，被乘的是 0）
    //   规格化：exp - bias - M
    //   Inf / NaN：128（2^128 在 f32 里就是 Inf）
    let normal_scale = sop_imm(
        em,
        exp.clone(),
        MirBinaryOp::Sub,
        (f.bias + f.mant_bits as i32) as i128,
    );
    let sub_scale = sop_imm(
        em,
        exp,
        MirBinaryOp::Sub,
        (f.bias + f.mant_bits as i32 - 1) as i128,
    );
    let scale = select(em, exp_is_zero, sub_scale, normal_scale);
    let scale = select(em, exp_is_max, MirValue::Int(128), scale);

    // 2^scale 的位模式：指数字段放 `scale + 127`，符号位直接带上——
    // 负的 2 的幂会把结果的符号翻过来。
    let scale_bits = sop_imm(em, scale, MirBinaryOp::Add, 127);
    let scale_bits = uop(em, scale_bits, MirBinaryOp::Shl, MirValue::Int(23));
    let sign = uop(em, sign, MirBinaryOp::Shl, MirValue::Int(31));
    let scale_bits = op(em, scale_bits, MirBinaryOp::BitOr, sign, NumTy::Uint(32));
    let scale_f32 = bits_to_f32(em, scale_bits);

    // 整数尾数转成 f32，再乘上 2 的幂。
    let sig_f32 = cast_int_to_float(em, sig, NumTy::Int(32), NumTy::Float(32));
    float_binary(em, sig_f32, MirBinaryOp::Mul, scale_f32, NumTy::Float(32))
}

// ==================================================================
// 辅助函数
// ==================================================================

/// 窄位模式在 MIR 里的类型（位模式是整数）。
pub(super) fn bits_ty(bits: u8) -> NumTy {
    if bits == 8 {
        NumTy::Uint(8)
    } else {
        NumTy::Uint(16)
    }
}

/// `f32 -> 窄位模式` 辅助函数的名字。
pub(super) fn f32_to_narrow_name(bits: u8) -> String {
    format!("__zr_f32_to_f{bits}")
}

/// `窄位模式 -> f32` 辅助函数的名字。
pub(super) fn narrow_to_f32_name(bits: u8) -> String {
    format!("__zr_f{bits}_to_f32")
}

/// `f32 -> 窄位模式` 辅助函数：形参是 f32，返回窄位模式（整数）。
///
/// 位模式是整数，所以返回类型是 `Uint(8)` / `Uint(16)`。
pub(super) fn f32_to_narrow_fn(bits: u8) -> MirFunction {
    let mut em = Emitter::new();
    let input = MirValue::Var("arg_0".to_string());
    let packed = f32_to_narrow_bits(&mut em, input, bits);
    MirFunction {
        name: format!("__zr_f32_to_f{bits}"),
        is_async: false,
        param_types: vec![NumTy::Float(32)],
        // 转换有上百条指令，内联回调用点会把调用方的栈帧撑爆。
        inline_never: true,
        inline_always: false,
        aggregate_ret: None,
        returns_future: false,
        params: vec![MirParam],
        blocks: vec![MirBasicBlock {
            id: 0,
            instrs: em.instrs,
            term: Some(MirTerminator::Return(Some(packed))),
        }],
    }
}

/// `窄位模式 -> f32` 辅助函数：形参是窄位模式，返回 f32。
pub(super) fn narrow_to_f32_fn(bits: u8) -> MirFunction {
    let mut em = Emitter::new();
    let input = MirValue::Var("arg_0".to_string());
    let value = narrow_bits_to_f32(&mut em, input, bits);
    MirFunction {
        name: format!("__zr_f{bits}_to_f32"),
        is_async: false,
        param_types: vec![if bits == 8 {
            NumTy::Uint(8)
        } else {
            NumTy::Uint(16)
        }],
        inline_never: true,
        inline_always: false,
        aggregate_ret: None,
        returns_future: false,
        params: vec![MirParam],
        blocks: vec![MirBasicBlock {
            id: 0,
            instrs: em.instrs,
            term: Some(MirTerminator::Return(Some(value))),
        }],
    }
}

// ==================================================================
// 纯 Rust 版本（字面量折叠 / 单元测试用）
// ==================================================================

/// f32 位模式 → 窄位模式（纯 Rust 版，与 [`f32_to_narrow_bits`] 同语义）。
pub(super) fn f32_to_narrow_bits_ref(bits: u32, width: u8) -> u32 {
    let f = format_for(width);
    let sign = (bits >> 31) & 1;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    let sign_bit = sign << f.sign_shift();

    if exp == 0xff {
        // Inf / NaN：指数全 1，NaN 保留一位尾数载荷。
        let payload = if mant != 0 { 1u32 << (f.mant_bits - 1) } else { 0 };
        return sign_bit | ((f.exp_mask() as u32) << f.mant_bits) | payload;
    }
    if exp == 0 {
        // f32 的次正规数在窄格式里直接下溢成 ±0。
        return sign_bit;
    }

    let e = exp - 127;
    if e > f.max_unbiased() {
        // 上溢 → Inf。
        return sign_bit | ((f.exp_mask() as u32) << f.mant_bits);
    }

    let e_min = -7 - f.bias - f.mant_bits as i32;
    let e_clamped = e.max(e_min);
    let shift = f
        .mant_shift()
        .max(24 - f.bias - f.mant_bits as i32 - e_clamped) as u32;
    let full = mant | 0x80_0000;
    let shifted = full >> shift;
    // 规格化数的尾数带隐含位，次正规数不带。
    let implicit = if e_clamped >= f.min_unbiased() {
        1u32 << f.mant_bits
    } else {
        0
    };
    let mut frac = shifted - implicit;
    let round_bit = (full >> (shift - 1)) & 1;
    let sticky = full & ((1u32 << (shift - 1)) - 1);
    if round_bit == 1 && (sticky != 0 || (frac & 1) == 1) {
        frac = frac.wrapping_add(1);
    }
    let carry = (frac >> f.mant_bits) & 1;
    let frac = frac & f.mant_mask() as u32;
    let e_field = (e_clamped + f.bias).clamp(0, f.exp_mask() as i32) as u32;
    let e_field = (e_field + carry).min(f.exp_mask() as u32);
    sign_bit | (e_field << f.mant_bits) | frac
}

/// 窄位模式 → f32 位模式（纯 Rust 版，与 [`narrow_bits_to_f32`] 同语义）。
pub(super) fn narrow_bits_to_f32_ref(bits: u32, width: u8) -> u32 {
    let f = format_for(width);
    let sign = ((bits >> f.sign_shift()) & 1) << 31;
    let exp = ((bits >> f.mant_bits) & f.exp_mask() as u32) as i32;
    let mant = bits & f.mant_mask() as u32;

    let (sig, scale) = if exp == 0 {
        (mant as f32, 1 - f.bias - f.mant_bits as i32)
    } else if exp == f.exp_mask() as i32 {
        if mant == 0 {
            (1.0f32, 128)
        } else {
            (0.0f32, 128)
        }
    } else {
        (
            (1u32 << f.mant_bits) as f32 + mant as f32,
            exp - f.bias - f.mant_bits as i32,
        )
    };
    let scale_bits = (((scale + 127) as u32) << 23) | sign;
    let value = sig * f32::from_bits(scale_bits);
    value.to_bits()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_bits(v: f32) -> u32 {
        v.to_bits()
    }

    #[test]
    fn format_matches_spec() {
        let f16 = format_for(16);
        assert_eq!((f16.exp_bits, f16.mant_bits, f16.bias), (5, 10, 15));
        let f8 = format_for(8);
        assert_eq!((f8.exp_bits, f8.mant_bits, f8.bias), (4, 3, 7));
    }

    #[test]
    fn zero_and_sign() {
        for width in [8u8, 16] {
            let f = format_for(width);
            assert_eq!(f32_to_narrow_bits_ref(f32_bits(0.0), width), 0);
            assert_eq!(
                f32_to_narrow_bits_ref(f32_bits(-0.0), width),
                1 << f.sign_shift()
            );
        }
    }

    #[test]
    fn one_round_trips() {
        for width in [8u8, 16] {
            let f = format_for(width);
            let narrow = f32_to_narrow_bits_ref(f32_bits(1.0), width);
            // 1.0 的指数字段就是偏置，尾数全 0。
            let expect = (f.bias as u32) << f.mant_bits;
            assert_eq!(narrow, expect);
            assert_eq!(narrow_bits_to_f32_ref(narrow, width), f32_bits(1.0));
        }
    }

    #[test]
    fn values_round_trip_exactly() {
        // 在窄格式里精确可表示的值，转换必须原样回来。
        for width in [8u8, 16] {
            for v in [0.5f32, 1.0, 2.0, 4.0, 0.25, 3.0, -2.5] {
                let narrow = f32_to_narrow_bits_ref(f32_bits(v), width);
                let back = f32::from_bits(narrow_bits_to_f32_ref(narrow, width));
                assert_eq!(back, v, "width={width} v={v}");
            }
        }
    }

    #[test]
    fn infinity_and_nan() {
        for width in [8u8, 16] {
            let f = format_for(width);
            let inf = f32_to_narrow_bits_ref(f32_bits(f32::INFINITY), width);
            assert_eq!(inf, (f.exp_mask() as u32) << f.mant_bits);
            assert!(f32::from_bits(narrow_bits_to_f32_ref(inf, width)).is_infinite());

            let nan = f32_to_narrow_bits_ref(f32_bits(f32::NAN), width);
            assert!(f32::from_bits(narrow_bits_to_f32_ref(nan, width)).is_nan());
        }
    }

    #[test]
    fn overflow_becomes_infinity() {
        for width in [8u8, 16] {
            let f = format_for(width);
            let big = f32_to_narrow_bits_ref(f32_bits(1.0e30), width);
            assert_eq!(big, (f.exp_mask() as u32) << f.mant_bits);
            assert!(f32::from_bits(narrow_bits_to_f32_ref(big, width)).is_infinite());
        }
    }

    #[test]
    fn underflow_becomes_zero() {
        for width in [8u8, 16] {
            assert_eq!(f32_to_narrow_bits_ref(f32_bits(1.0e-30), width), 0);
        }
    }

    #[test]
    fn rounding_is_nearest_even() {
        // 1.0 在 f8 下尾数只有 3 位：1 + 2^-7 恰好落在两个可表示值中间，
        // 就近偶数舍入到 1.0（尾数最低位是 0）。
        let f = format_for(8);
        let v = 1.0f32 + 1.0 / 128.0;
        let narrow = f32_to_narrow_bits_ref(f32_bits(v), 8);
        let expect = (f.bias as u32) << f.mant_bits;
        assert_eq!(narrow, expect);
    }

    #[test]
    fn subnormal_narrow_values_round_trip() {
        // f16 的次正规数（2^-24）必须能原样还原。
        let f = format_for(16);
        let narrow = 1u32; // 指数全 0，尾数 1 → 2^-24
        let back = f32::from_bits(narrow_bits_to_f32_ref(narrow, 16));
        assert_eq!(back, 2.0f32.powi(-24));
        let _ = f;
    }
}
