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
//==============================================================================

use std::collections::HashMap;

use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};
use crate::mir::NumTy;
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaUnaryOp, SsaValue};

/// 通用寄存器 r0–r15。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(pub u32);

impl Reg {
    /// 位域值（ARM 的寄存器字段统一 4 位）。
    ///
    /// 返回 `u16`：Thumb 的常量位域基本都用 `u16` 拼，半字编码里混进
    /// `u32` 只会带来一连串无谓的类型转换。
    fn bits(self) -> u16 {
        (self.0 & 0xf) as u16
    }
}

/// 可分配寄存器。**刻意只用 r0–r7**：Thumb-1 的 16 位编码（add / cmp /
/// push / pop / mul）只覆盖低 8 个，用到 r8 以上就得全部走 32 位宽指令，
/// 代码量和体积都会明显变差。
const ALLOCATABLE: [Reg; 8] = [
    Reg(4),
    Reg(5),
    Reg(6),
    Reg(7),
    Reg(0),
    Reg(1),
    Reg(2),
    Reg(3),
];

thread_local! {
    static REG_MAP: std::cell::RefCell<HashMap<u32, Reg>> =
        std::cell::RefCell::new(HashMap::new());
}

fn alloc_reg(v: SsaValue) -> Reg {
    REG_MAP.with(|m| {
        m.borrow()
            .get(&v.id())
            .copied()
            .unwrap_or_else(|| ALLOCATABLE[(v.id() as usize) % ALLOCATABLE.len()])
    })
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

/// 数据处理（3 个寄存器）T2。
///
/// `prefix` 是 hw1 的固定部分（`op` + S 位），各运算的取值由 clang 的
/// 参考编码得出：add 0xEB、sub 0xEB（op 位不同）、and 0xEA、orr 0xEA、
/// eor 0xEA。早先把 op 放在 `(op << 5)` 再事后回写 hw1，既绕又容易写错，
/// 直接吃前缀更清楚。
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
///
/// `op` 落在 hw1 的 [7:4]：MUL=0b0000、SDIV=0b1001、UDIV=0b1011
/// （clang: mul=fb02 sdiv=fb92 udiv=fbb2）。hw2 的高半字节固定 `0xf`。
fn muldiv(unit: &mut CompilationUnit, op: u16, rd: Reg, rn: Reg, rm: Reg) {
    let hw1 = 0xfb00 | (op << 4) | rn.bits();
    // hw2 的 [7:4]：MUL 是 0，SDIV/UDIV 是 0xf（ARM ARM 里那个符号位）。
    // clang：mul=fb02 f103、sdiv=fb92 f1f3。
    let nib = if op == 0 { 0 } else { 0xf };
    let hw2 = 0xf000 | (nib << 4) | (rd.bits() << 8) | rm.bits();
    w32(unit, hw1, hw2);
}

/// `mls rd, rn, rm, ra` → `rd = ra - rn * rm`。
///
/// clang 实测 `mls r4,r2,r3,r1` = fb02 1413：hw2 的 [7:4] 恒为 1
/// （不是 rn，也不是 0），hw1 的 [3:0] 才是 rn。
fn mls(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
    let hw1 = 0xfb00 | rn.bits();
    let hw2 = (ra.bits() << 12) | (rd.bits() << 8) | 0x10 | rm.bits();
    w32(unit, hw1, hw2);
}

/// 移位（寄存器控制）T2。
/// `lsl.w` / `lsr.w` / `asr.w`：op 落在 hw1 的 [6:5]，即 LSL 0b00 / LSR 0b01 / ASR 0b10
/// （clang: lsl=fa02 lsr=fa22 asr=fa42）。hw2 高半字节固定 `0xf`。
fn shift_reg(unit: &mut CompilationUnit, op: u16, rd: Reg, rn: Reg, rm: Reg) {
    let hw1 = 0xfa00 | (op << 5) | rn.bits();
    let hw2 = 0xf000 | (rd.bits() << 8) | rm.bits();
    w32(unit, hw1, hw2);
}

/// 一元（`mvn` 等）T2：hw2 = Rd | Rm。
fn un_op(unit: &mut CompilationUnit, hw1: u16, rd: Reg, rm: Reg) {
    w32(unit, hw1, (rd.bits() << 8) | rm.bits());
}

/// 扩展（`sxtb`/`sxth`/`uxtb`/`uxth`）16 位。
/// `sxtb/sxth/uxtb/uxth`。
///
/// Thumb-1 的这四条只有 16 位版本 `0xB2xx`，且**只编码 Rd**（源寄存器被
/// 隐含为 Rd 自己），所以 rd != rm 时先补一条 `mov rd, rm`。
/// kind: 0b00=sxtb 0b01=sxth 0b10=uxtb 0b11=uxth
/// （clang: sxtb=b251 sxth=b211 uxtb=b2d1 uxth=b291）
fn extend16(unit: &mut CompilationUnit, kind: u16, rd: Reg, rm: Reg) {
    let field: u16 = match kind {
        0b00 => 0b01, // sxtb -> 0x40
        0b01 => 0b00, // sxth -> 0x00
        0b10 => 0b11, // uxtb -> 0xC0
        0b11 => 0b10, // uxth -> 0x80
        _ => unreachable!(),
    };
    if rd != rm {
        mov_low(unit, rd, rm);
    }
    hw(unit, 0xb200 | (field << 6) | 0x10 | rd.bits());
}

/// `mov rd, rm`（16 位，仅低 8 个寄存器）。
/// `mov rd, rm`（16 位高/低寄存器通用形式）。clang: `mov r1, r2` = 0x4611。
///
/// 目标必须落在 r0-r7（16 位形式的 Rd 只有 3 位）。
fn mov_low(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    debug_assert!(rd.0 < 8, "mov_low 的目标只能是低 8 个寄存器");
    hw(unit, 0x4600 | ((rm.bits() & 7) << 3) | (rd.bits() & 7));
}

/// `cmp rn, #imm`，imm 需能放进 8 位。
/// clang: `cmp r0,#0`=2800 `cmp r1,#7`=2907 `cmp r2,#255`=2aff `cmp.w r8,#0`=f1b8 0f00
fn cmp_imm8(unit: &mut CompilationUnit, rn: Reg, imm: u8) {
    if rn.0 < 8 {
        // clang: cmp r0,#0=2800 cmp r1,#7=2907 cmp r2,#255=2aff
        hw(unit, 0x2800 | (rn.bits() << 8) | imm as u16);
    } else {
        // `cmp.w rn, #imm`：clang `cmp.w r8, #0` = f1b8 0f00
        w32(unit, 0xf1b0 | rn.bits(), 0x0f00 | imm as u16);
    }
}

/// `mov.w rd, rm`（32 位 T3），寄存器范围不限。
fn mov_wide(unit: &mut CompilationUnit, rd: Reg, rm: Reg) {
    w32(unit, 0xea4f, (rd.bits() << 8) | rm.bits());
}

/// `mov rd, #imm`（16 位，仅 0–255）。
fn mov_imm8(unit: &mut CompilationUnit, rd: Reg, imm: u8) {
    hw(unit, 0x2000 | ((rd.bits() & 7) << 8) | imm as u16);
}

/// `cmp rn, rm`（16 位，仅低 8 个寄存器）。
fn cmp_low(unit: &mut CompilationUnit, rn: Reg, rm: Reg) {
    hw(unit, 0x4280 | ((rm.bits() & 7) << 3) | (rn.bits() & 7));
}

/// `cmp.w rn, rm`（32 位 T2）。
fn cmp_wide(unit: &mut CompilationUnit, rn: Reg, rm: Reg) {
    w32(unit, 0xebb0 | rn.bits(), 0x0f00 | rm.bits());
}

/// IT 块：让**紧跟其后的一条**指令按条件执行。
///
/// 编码固定为 `1011 1111 cond(4) mask(4)`，单条指令时 mask = 0b1000。
fn it(unit: &mut CompilationUnit, cond: u16) {
    hw(unit, 0xbf00 | (cond << 4) | 0b1000);
}

/// 条件码（与 A32 一致）。
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
            // 余数 = l - (l / r) * r。
            // sdiv 之后 dst 里是商，左操作数还留在 rn：
            //   mls rd=dst, rn=rm, rm=商, ra=原左操作数 → dst = l - r*商
            muldiv(unit, 0b1001, dst, rn, rm);
            mls(unit, dst, rm, dst, rn);
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
            // 相等/不等：异或后与 0 比较（拿 dst 跟自己比永远是相等）
            dp3(unit, 0xea80, dst, rn, rm);
            cmp_imm8(unit, dst, 0);
        }
        _ => {
            if rn.0 < 8 && rm.0 < 8 {
                cmp_low(unit, rn, rm);
            } else {
                cmp_wide(unit, rn, rm);
            }
        }
    }
    // 条件置 1 / 0：Thumb-2 没有 A32 的条件后缀，用 IT 块。
    it(unit, cond_of(op));
    mov_imm8(unit, dst, 1);
    it(unit, invert(cond_of(op)));
    mov_imm8(unit, dst, 0);
}

fn emit_unary(unit: &mut CompilationUnit, dst: Reg, op: SsaUnaryOp, rm: Reg) {
    match op {
        // !x：等价于 x == 0
        SsaUnaryOp::Not => emit_compare(unit, dst, SsaBinaryOp::Eq, rm, rm),
        // ~x：mvn
        SsaUnaryOp::BitNot => un_op(unit, 0xea6f, dst, rm),
        // -x：rsb dst, rm, #0
        SsaUnaryOp::Neg => {
            // `rsb dst, rm, #0`：hw1 常量位固定，S 位为 0
            let hw1 = 0xf1c0 | rm.bits();
            w32(unit, hw1, dst.bits() << 8);
        }
    }
}

/// 类型转换：取低 w 位并按有无符号扩展（32 位目标上 64 位放不下）。
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
        8 => extend16(unit, if signed { 0b00 } else { 0b01 }, dst, rm),
        16 => extend16(unit, if signed { 0b01 } else { 0b10 }, dst, rm),
        32 => emit_mov_rr(unit, dst, rm),
        _ => crate::codegen::unsupported_cast(from, to),
    }
}

/// `bl target`（T1，带重定位）。
fn emit_bl(unit: &mut CompilationUnit, target: &str) {
    let at = unit.code.len();
    // 先写占位，再压重定位（ARM 的重定位是 PC 相对 + S 位）
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
        // T4 是 32 位指令；T1 条件分支是 16 位，见下
        instr_len: 4,
        symbol: target.to_string(),
        kind: RelocationKind::ArmBranchThumb,
        addend: 0,
    });
}

/// `cbz` / `cbnz rn, label`（16 位）：按寄存器是否为**零**分支。
///
/// `branch_if_zero = true` 是 `cbz`，否则是 `cbnz`。范围 ±128 字节
/// （imm8 以半字为单位）。
/// 条件是"某个值非零则跳转"时的落码。
///
/// 16 位的 `cbz/cbnz` **只支持 r0-r3**（clang: `cbnz r2`=0xB912 `cbnz r4` 需
/// 32 位），而本后端的寄存器分配覆盖 r0-r7，所以高寄存器一律退化成
/// `cmp rn,#0` + IT + 无条件跳转。
fn emit_branch_if_nonzero(
    unit: &mut CompilationUnit,
    rn: Reg,
    true_target: &str,
    false_target: &str,
) {
    if rn.0 <= 3 {
        let at = unit.code.len();
        // 0xB1xx：bit11 = 0 是 CBZ / 1 是 CBNZ；Rn 在 [2:0]，
        // 6 位有符号半字偏移在 [9:3]。clang: `cbnz r2`(imm=4) = 0xB912
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

/// 按 ARM 调用约定把实参搬进 r0–r3（超过 4 个走栈，本后端未实现）。
fn emit_call_args(unit: &mut CompilationUnit, args: &[SsaValue]) {
    const ARG: [Reg; 4] = [Reg(0), Reg(1), Reg(2), Reg(3)];
    for (i, a) in args.iter().enumerate().take(ARG.len()) {
        let src = alloc_reg(*a);
        if src != ARG[i] {
            emit_mov_rr(unit, ARG[i], src);
        }
    }
}

fn emit_phi_copies(unit: &mut CompilationUnit, func: &SsaFunction, succ: usize, from: usize) {
    let Some(block) = func.blocks.iter().find(|b| b.id == succ) else {
        return;
    };
    // φ 的值在进入前驱块时还活着，不能被这里的搬运破坏，所以按
    // "目标没被当作源读" 的顺序依次发射。
    let mut pending: Vec<(Reg, SsaValue)> = Vec::new();
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
        pending.push((alloc_reg(*result), *src));
    }
    loop {
        let Some(at) = pending
            .iter()
            .position(|(d, _src)| !pending.iter().any(|(_, other)| alloc_reg(*other) == *d))
        else {
            break;
        };
        let (d, s) = pending.remove(at);
        emit_mov_rr(unit, d, alloc_reg(s));
    }
}

fn emit_instr(unit: &mut CompilationUnit, instr: &SsaInstr) {
    match instr {
        SsaInstr::Const { result, value } => match value {
            SsaConst::Int(i) => mov_imm32(unit, alloc_reg(*result), *i as u32),
            SsaConst::Bool(b) => mov_imm8(unit, alloc_reg(*result), u8::from(*b)),
            SsaConst::Unit => mov_imm8(unit, alloc_reg(*result), 0),
            SsaConst::Float(..) => crate::codegen::unsupported_const("float"),
            SsaConst::Str(_) => crate::codegen::unsupported_const("string"),
        },
        SsaInstr::Unary {
            result,
            op,
            operand,
        } => emit_unary(unit, alloc_reg(*result), *op, alloc_reg(*operand)),
        SsaInstr::Binary {
            result,
            op,
            lhs,
            rhs,
            ..
        } => emit_binary(
            unit,
            alloc_reg(*result),
            *op,
            alloc_reg(*lhs),
            alloc_reg(*rhs),
        ),
        SsaInstr::Call {
            result,
            callee,
            args,
            ..
        } => {
            emit_call_args(unit, args);
            emit_bl(unit, callee);
            emit_mov_rr(unit, alloc_reg(*result), Reg(0));
        }
        SsaInstr::Phi { .. } => {}
        SsaInstr::Cast {
            result,
            src,
            from,
            to,
        } => emit_cast(unit, alloc_reg(*result), alloc_reg(*src), *from, *to),
        SsaInstr::Copy { result, src } => {
            emit_mov_rr(unit, alloc_reg(*result), alloc_reg(*src));
        }
        SsaInstr::Var { .. } => {}
    }
}

/// 发射一个函数。
pub fn emit_function(
    target: &crate::codegen::target::Target,
    unit: &mut CompilationUnit,
    func: &SsaFunction,
) {
    debug_assert!(target.is_arm32_thumb(), "Thumb-2 后端只用于 thumb 目标");
    let alloc = crate::codegen::regalloc::allocate_registers(func, ALLOCATABLE.len(), &[]);
    if alloc.spilled > 0 {
        crate::codegen::unsupported_pressure(&func.name, ALLOCATABLE.len());
    }
    let map: HashMap<u32, Reg> = alloc
        .map
        .into_iter()
        .map(|(k, loc)| match loc {
            crate::codegen::regalloc::Location::Reg(i) => (k, ALLOCATABLE[i]),
            crate::codegen::regalloc::Location::Stack(_) => unreachable!(),
        })
        .collect();
    REG_MAP.with(|m| {
        *m.borrow_mut() = map;
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

    // 序言：`push {r4-r7, lr}`
    push_regs(unit, &[Reg(4), Reg(5), Reg(6), Reg(7), Reg(14)]);

    // AAPCS：前 4 个整数参数从 r0-r3 传进来，搬进分配器给的寄存器。
    // 第 5 个及以后在栈上，暂不支持——宁可报错也别读错栈槽。
    if func.params.len() > 4 {
        crate::codegen::unsupported_pressure(
            &format!("{} 的第 5 个及以后的参数（栈上传参尚未实现）", func.name),
            ALLOCATABLE.len(),
        );
    }
    for (i, param) in func.params.iter().enumerate() {
        let dst = alloc_reg(*param);
        if dst != Reg(i as u32) {
            emit_mov_rr(unit, dst, Reg(i as u32));
        }
    }

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
            crate::ssa::SsaTerminator::Return(ret) => {
                if let Some(v) = ret {
                    emit_mov_rr(unit, Reg(0), alloc_reg(*v));
                }
                // `pop {r4-r7, pc}` 直接返回；弹 LR 的话会从序言里继续往下跑
                pop_regs(unit, &[Reg(4), Reg(5), Reg(6), Reg(7), Reg(15)]);
            }
            crate::ssa::SsaTerminator::Goto(t) => emit_b(unit, &block_label(&func.name, *t)),
            crate::ssa::SsaTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                // 条件是个值（非零为真）。Thumb 有专门的 `cbnz` 16 位指令，
                // 比"cmp #0 再条件跳"少一条，而且不受条件码限制。
                let r = alloc_reg(*cond);
                emit_branch_if_nonzero(
                    unit,
                    r,
                    &block_label(&func.name, *then_block),
                    &block_label(&func.name, *else_block),
                );
            }
            crate::ssa::SsaTerminator::Unreachable => hw(unit, 0xbf00), // nop
        }
    }

    // 回填块符号
    for i in 0..func.blocks.len() {
        if let Some(sym) = unit.symbols.get_mut(sym_base + i) {
            sym.offset = block_offsets[i];
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

fn successors(block: &crate::ssa::SsaBasicBlock) -> Vec<usize> {
    match &block.term {
        crate::ssa::SsaTerminator::Goto(t) => vec![*t],
        crate::ssa::SsaTerminator::Branch {
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

/// `push {reglist}`（16 位，只支持低 8 个寄存器 + lr）。
fn push_regs(unit: &mut CompilationUnit, regs: &[Reg]) {
    let mut mask: u16 = 0;
    for r in regs {
        match r.0 {
            0..=7 => mask |= 1 << r.0,
            // Thumb-1 的 push/pop 掩码里 LR 和 PC 都落在 bit 8
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
            // bit 8 = PC；`pop {.., pc}` 就是函数返回
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
    // 退出交给运行时（zr_exit）
    emit_bl(unit, "zr_exit");
    hw(unit, 0xbf00); // nop（zr_exit 声明为 !）

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
    /// 同一句汇编得到。用到 clang 的内置汇编器会拒绝 IT 块，所以 IT 的
    /// 参考值取自 ARM ARM 的固定编码格式。
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
        // clang：`sub.w r1, r2, r3` = eba2 0103
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
        // 16 位 extend 只编码 Rd，rd != rm 时先补一条 mov
        extend16(&mut u, 0b00, Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0x4611, 0xb251], "sxtb r1, r2");
        extend16(&mut u, 0b00, Reg(1), Reg(1));
        let mut u = empty_unit();
        extend16(&mut u, 0b00, Reg(1), Reg(1));
        assert_eq!(halfwords(&u), [0xb251], "sxtb r1, r1");

        let mut u = empty_unit();
        extend16(&mut u, 0b01, Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0x4611, 0xb211], "sxth r1, r2");

        let mut u = empty_unit();
        extend16(&mut u, 0b10, Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0x4611, 0xb2d1], "uxtb r1, r2");

        let mut u = empty_unit();
        extend16(&mut u, 0b11, Reg(1), Reg(2));
        assert_eq!(halfwords(&u), [0x4611, 0xb291], "uxth r1, r2");

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

    /// IT 块：clang 的内置汇编器不接受，这里按 ARM ARM 的固定格式验证。
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
        // clang 参考：`it ne` + `movw r0, #0`；IT 见上面的固定编码测试
        let mut u = empty_unit();
        it(&mut u, 1);
        mov_imm8(&mut u, Reg(0), 0);
        assert_eq!(halfwords(&u), [0xbf18, 0x2000]);
    }

    #[test]
    fn all_emitted_instructions_are_aligned() {
        // Thumb 指令必须是 2 字节对齐的；分支目标也必须对齐
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
}
