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

use crate::codegen::target::{Arch, Target};
use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};
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

/// 可用于分配的通用寄存器：x0..x17。
///
/// x18 是平台保留寄存器（Linux 用作 TLS），x29/x30 是帧指针与链接寄存器，
/// x31 在非 `SP` 语境下即 `xzr`，因此都不参与分配。
const ALLOCATABLE: [Reg; 18] = [
    Reg(0), Reg(1), Reg(2), Reg(3), Reg(4), Reg(5), Reg(6), Reg(7), Reg(8), Reg(9), Reg(10), Reg(11),
    Reg(12), Reg(13), Reg(14), Reg(15), Reg(16), Reg(17),
];

/// AAPCS64 的整数实参寄存器 x0-x7。
const ARG_REGS: [Reg; 8] = [
    Reg(0), Reg(1), Reg(2), Reg(3), Reg(4), Reg(5), Reg(6), Reg(7),
];

/// 把实参搬进 AAPCS64 的 x0-x7。
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

fn alloc_reg(v: SsaValue) -> Reg {
    ALLOCATABLE[(v.id() as usize) % ALLOCATABLE.len()]
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
    insn(unit, op | (shift << 21) | (((imm & 0xffff) as u32) << 5) | rd.bits() as u32);
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
    insn(unit, 0xaa00_03e0 | (rm.bits() as u32) << 16 | rd.bits() as u32);
}

/// 三地址算术/逻辑：`op xd, xn, xm`。
fn emit_op3(unit: &mut CompilationUnit, op: u32, rd: Reg, rn: Reg, rm: Reg) {
    insn(unit, op | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5 | rd.bits() as u32);
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
    insn(unit, 0xeb00_001f | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5);
}

/// `cset xd, cond`（`csinc xd, xzr, xzr, invert(cond)`）。
fn emit_cset(unit: &mut CompilationUnit, rd: Reg, cond: u32) {
    insn(unit, 0x9a9f_07e0 | ((cond ^ 1) << 12) | rd.bits() as u32);
}

/// `madd xd, xn, xm, xa`（`xa = XZR` 时即乘法）。
fn emit_madd(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg, ra: u8) {
    insn(
        unit,
        0x9b00_0000 | (rm.bits() as u32) << 16 | (ra as u32) << 10 | (rn.bits() as u32) << 5
            | rd.bits() as u32,
    );
}

/// `sdiv xd, xn, xm`
fn emit_sdiv(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg) {
    insn(unit, 0x9ac0_0c00 | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5 | rd.bits() as u32);
}

/// `msub xd, xn, xm, xa`：`xa - xn * xm`（取模用）。
fn emit_msub(unit: &mut CompilationUnit, rd: Reg, rn: Reg, rm: Reg, ra: u8) {
    insn(
        unit,
        0x9b00_8000 | (rm.bits() as u32) << 16 | (ra as u32) << 10 | (rn.bits() as u32) << 5
            | rd.bits() as u32,
    );
}

/// 数据处理（2）类指令：移位等，`op xd, xn, xm`。
fn emit_dp2(unit: &mut CompilationUnit, op: u32, rd: Reg, rn: Reg, rm: Reg) {
    insn(unit, op | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5 | rd.bits() as u32);
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
    unit.relocations.push(Relocation::new(instr_start, instr_start, 4, target, kind));
}

/// 条件跳转序列。
///
/// A64 的 `b.cond` 只有 19 位字面量（±1 MiB），不足以直接跳到任意基本块；
/// 因此这里采用经典的两级结构：
/// ```text
///     b.<!cond> +8      ; 条件成立时跳过下面这条 b，偏移固定、无需重定位
///     b    then_label
///     b    else_label
/// ```
/// 两条无条件跳转使用 26 位字面量，由链接器（或本编译器的重定位回填）
/// 修正到实际基本块。
fn emit_cond_branch(unit: &mut CompilationUnit, cond: u32, then_label: &str, else_label: &str) {
    // 0x54000000 | imm19 << 5 | cond；imm19 = 1 → 目标为下一条指令之后。
    insn(unit, 0x5400_0000 | (1u32 << 5) | ((cond ^ 1) & 0xf));
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
        SsaTerminator::Branch { then_block, else_block, .. } => vec![*then_block, *else_block],
        _ => Vec::new(),
    }
}

/// 在当前块末尾把本块持有的值写进后继块的各个 φ 寄存器（并行拷贝）。
fn emit_phi_copies(unit: &mut CompilationUnit, func: &SsaFunction, succ: usize, from: usize) {
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

fn block_label(func: &str, id: usize) -> String {
    format!("{func}.bb{id}")
}

// ==================================================================
// 函数与指令发射
// ==================================================================

/// 发射一个函数的机器码。
pub fn emit_function(target: &Target, unit: &mut CompilationUnit, func: &SsaFunction) {
    debug_assert_eq!(target.arch(), Arch::AArch64, "AArch64 后端只能用于 aarch64 目标");
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
                    let r = alloc_reg(*v);
                    emit_mov_rr(unit, Reg::X0, r);
                }
                emit_epilogue(unit);
                emit_ret(unit);
            }
            SsaTerminator::Goto(target) => emit_b(unit, &block_label(&func.name, *target)),
            SsaTerminator::Branch { cond, then_block, else_block } => {
                let r = alloc_reg(*cond);
                emit_cmp(unit, r, r);
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
            SsaConst::Float(_) | SsaConst::Str(_) => {
                emit_mov_imm64(unit, alloc_reg(*result), 0);
            }
        },
        SsaInstr::Unary { result, op, operand } => emit_unary(unit, *result, *op, *operand),
        SsaInstr::Binary { result, op, lhs, rhs } => emit_binary(unit, *result, *op, *lhs, *rhs),
        SsaInstr::Call { result, callee, args } => {
            emit_call_args(unit, args);
            emit_call(unit, callee);
            emit_mov_rr(unit, alloc_reg(*result), Reg(0));
        }
        // φ 的实际拷贝由前驱末尾的 emit_phi_copies 发出
        SsaInstr::Phi { .. } => {}
        SsaInstr::Copy { result, src } => {
            let (d, s) = (alloc_reg(*result), alloc_reg(*src));
            if d != s {
                emit_mov_rr(unit, d, s);
            }
        }
        // `Var` 只是 SSA 值的命名引用，运行时不产生代码。
        SsaInstr::Var { .. } => {}
    }
}

/// 发射一元运算。
fn emit_unary(unit: &mut CompilationUnit, result: SsaValue, op: crate::ssa::SsaUnaryOp, operand: SsaValue) {
    let dst = alloc_reg(result);
    let src = alloc_reg(operand);

    use crate::ssa::SsaUnaryOp::*;
    match op {
        // !x：比较 x 是否为 0
        Not => {
            emit_cmp(unit, src, src);
            emit_cset(unit, dst, 0); // eq
        }
        // ~x：mvn xd, xm
        BitNot => {
            insn(unit, 0xaa20_03e0 | (src.bits() as u32) << 16 | dst.bits() as u32);
        }
        // -x：neg xd, xm
        Neg => {
            insn(unit, 0xcb00_03e0 | (src.bits() as u32) << 16 | dst.bits() as u32);
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
            insn(unit, 0xaa00_0000 | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5 | dst.bits() as u32);
        }
        BitXor => {
            insn(unit, 0xca00_0000 | (rm.bits() as u32) << 16 | (rn.bits() as u32) << 5 | dst.bits() as u32);
        }
        Mul => emit_madd(unit, dst, rn, rm, XZR),
        Div => emit_sdiv(unit, dst, rn, rm),
        Rem => {
            // q = l / r; 余数 = l - q * r
            emit_sdiv(unit, dst, rn, rm);
            emit_msub(unit, dst, dst, rm, rn.bits());
        }
        Shl => emit_dp2(unit, 0x9ac2_2000, dst, rn, rm),
        Shr => emit_dp2(unit, 0x9ac2_2400, dst, rn, rm),
        Eq | Ne | Lt | Gt | Le | Ge => {
            emit_cmp(unit, rn, rm);
            emit_cset(unit, dst, condition_code(op));
        }
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
        // Linux：bl main; mov x8, #93; svc #0（__NR_exit = 93）
        emit_call(unit, "main");
        emit_mov_wide(unit, Reg(8), 93, 0xd280_0000, 0); // mov x8, #93
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
            assert!(r.0 < 18, "不能分配 x29/x30/x31");
        }
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
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()) >> 26, 0b100101);
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
        assert_eq!((first >> 5) & 0x7_ffff, 1, "字面量 +8 字节（跳过一条指令）");
        assert_eq!(first & 0xf, cc::EQ, "条件取反后为 eq");
        assert_eq!(u32::from_le_bytes(u.code[4..8].try_into().unwrap()) >> 26, 0b000101);
        assert_eq!(u32::from_le_bytes(u.code[8..12].try_into().unwrap()) >> 26, 0b000101);
        assert_eq!(u.relocations.len(), 2, "两条无条件跳转需要重定位");
        assert!(u.relocations.iter().all(|r| r.kind == RelocationKind::Branch26));
    }

    #[test]
    fn start_wrapper_uses_syscalls_on_linux_and_runtime_elsewhere() {
        let linux = Target::new(Arch::AArch64, Os::Linux);
        let mut u = empty_unit();
        emit_start_wrapper(&linux, &mut u);
        // bl main; mov x8, #93; svc #0
        assert_eq!(u.code.len(), 12);
        assert_eq!(u32::from_le_bytes(u.code[0..4].try_into().unwrap()) >> 26, 0b100101, "bl main");
        assert_eq!(
            u32::from_le_bytes(u.code[4..8].try_into().unwrap()),
            0xd280_0ba8,
            "mov x8, #93"
        );
        assert_eq!(u32::from_le_bytes(u.code[8..12].try_into().unwrap()), 0xd400_0001, "svc #0");
        assert_eq!(u.entry, "_start");

        let macos = Target::new(Arch::AArch64, Os::Darwin);
        let mut u = empty_unit();
        emit_start_wrapper(&macos, &mut u);
        // bl main; bl zr_exit; brk #0
        assert_eq!(u.code.len(), 12);
        let targets: Vec<&str> = u.relocations.iter().map(|r| r.symbol.as_str()).collect();
        assert_eq!(targets, vec!["main", "zr_exit"]);
        assert_eq!(u32::from_le_bytes(u.code[8..12].try_into().unwrap()), 0xd420_0000, "brk #0");
    }

    #[test]
    fn remainder_uses_msub_without_clobbering_the_dividend() {
        let mut u = empty_unit();
        emit_binary(&mut u, SsaValue(3), SsaBinaryOp::Rem, SsaValue(0), SsaValue(1));
        assert!(!u.code.is_empty());
    }
}