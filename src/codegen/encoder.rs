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
use crate::mir::NumTy;
use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaTerminator, SsaValue};
use std::collections::{HashMap, HashSet};

/// x86-64 通用寄存器（System V ABI / Microsoft x64 ABI 共用编号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gpr {
    Rax,
    Rcx,
    Rdx,
    Rsi,
    Rdi,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
}

impl Gpr {
    /// ModR/M `reg` 字段的 3 位编码（REX.B 为 0 时使用）。
    fn bits(self) -> u8 {
        use Gpr::*;
        match self {
            Rax => 0,
            Rcx => 1,
            Rdx => 2,
            Rsi => 6,
            Rdi => 7,
            R8 => 0,
            R9 => 1,
            R10 => 2,
            R11 => 3,
            R12 => 4,
            R13 => 5,
            R14 => 6,
            R15 => 7,
        }
    }

    /// 是否需要 REX.B / REX.R 扩展位（R8–R15 需要）。
    fn rex_bit(self) -> bool {
        matches!(
            self,
            Gpr::R8 | Gpr::R9 | Gpr::R10 | Gpr::R11 | Gpr::R12 | Gpr::R13 | Gpr::R14 | Gpr::R15
        )
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
    Gpr::Rsi,
    Gpr::Rdi,
    Gpr::R8,
    Gpr::R9,
    Gpr::R10,
    Gpr::R11,
    Gpr::R12,
    Gpr::R13,
    Gpr::R14,
    Gpr::R15,
];

/// 把 SSA value 映射到物理寄存器（虚拟分配）。
/// 基本块标签：`<函数名>.bb<编号>`。
///
/// 早先只用 `bb<编号>`，于是**不同函数的同名块会共用一个符号**——`main`
/// 的 `bb1` 和 `f` 的 `bb1` 撞在一起，`main` 里的分支会跳进别的函数。
/// 加上函数名前缀后每个块的标签全局唯一。
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
        let SsaInstr::Phi {
            result, incoming, ..
        } = instr
        else {
            continue;
        };
        let Some((_, src)) = incoming.iter().find(|(p, _)| *p == from) else {
            continue;
        };
        // 浮点 φ 的两侧都是浮点栈槽：xmm0 中转，不碰 GPR
        if let (Some(rs), Some(ss)) = (float_slot(*result), float_slot(*src)) {
            let w = float_width(*result).max(float_width(*src));
            emit_load_float(unit, 0, ss, w);
            emit_store_float(unit, 0, rs, w);
            continue;
        }
        if let Location::Stack(slot) = location_of(*result) {
            emit_mov_src(unit, SCRATCH_AX, src_of(*src));
            emit_store_slot(unit, SCRATCH_AX, slot);
        }
    }
    // 剩下的都是寄存器到寄存器，统一走并行搬运处理环
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
        // 浮点 φ 已在上面按栈槽处理完，这里跳过
        if float_slot(*result).is_some() || float_slot(*src).is_some() {
            continue;
        }
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

/// 登记一个只在函数内部使用的局部标签（条件分支的 `else` 副本）。
///
/// 与基本块标签不同：它不对应任何块，只是让 `jz` 能跳过 then 分支的代码。
/// 放在符号表里（而不是单独一张表）是为了能复用现有的重定位解析路径。
fn bind_local_label(unit: &mut CompilationUnit, name: &str, offset: usize) {
    unit.symbols.push(Symbol {
        name: name.to_string(),
        section: SymbolSection::Text,
        offset,
        size: 0,
        global: false,
        kind: SymbolKind::Section,
    });
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
    /// 间接调用保存函数指针的专用栈槽。
    static CALL_SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// 浮点值专用的栈槽。
    ///
    /// 浮点走 **XMM**，而整数分配器只认 GPR。与其给分配器加一整套并行的
    /// 浮点活跃区间（收益很小，因为 SSA 里每个浮点值只定义一次），不如给
    /// 每个浮点值一个专属栈槽，运算时用 `xmm0/xmm1` 当暂存器读写内存。
    /// 慢一点，但绝对正确，而且不碰已经验证过的整数分配器。
    static FLOAT_SLOTS: std::cell::RefCell<HashMap<u32, (usize, u8)>> =
        std::cell::RefCell::new(HashMap::new());
    /// 被取地址的局部变量的专属栈槽：局部 id -> 绝对槽号。
    ///
    /// 与浮点槽同一块帧，只是排在浮点槽之后，所以 `[rsp + 8*slot]`
    /// 寻址方式完全一样。`&x` 就是这块槽的地址。
    static LOCAL_SLOTS: std::cell::RefCell<HashMap<u32, usize>> =
        std::cell::RefCell::new(HashMap::new());
}

/// `v` 的浮点位宽（32 或 64）。整数返回 0。
fn float_width(v: SsaValue) -> u8 {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).map(|(_, w)| *w).unwrap_or(0))
}

/// `v` 如果是浮点值，返回它的栈槽。
fn float_slot(v: SsaValue) -> Option<usize> {
    FLOAT_SLOTS.with(|m| m.borrow().get(&v.id()).map(|(s, _)| *s))
}

/// 浮点标量移动 / 运算的前缀：f32 用 F3（`scalar single`），f64 用 F2（`scalar double`）。
fn sse_prefix(width: u8) -> u8 {
    if width == 32 {
        0xF3
    } else {
        0xF2
    }
}

/// `movss/movsd xmm, [rsp + 8*slot]`
fn emit_load_float(unit: &mut CompilationUnit, xmm: u8, slot: usize, width: u8) {
    emit1(unit, sse_prefix(width));
    emit1(unit, 0x0F);
    emit1(unit, 0x10);
    // mod=10(disp32), reg=xmm, rm=100(SIB)
    emit1(unit, 0b10_000_100 | (xmm & 7) << 3);
    emit1(unit, 0b00_100_100);
    emit4(unit, (slot * 8) as u32);
}

/// `movss/movsd [rsp + 8*slot], xmm`
fn emit_store_float(unit: &mut CompilationUnit, xmm: u8, slot: usize, width: u8) {
    emit1(unit, sse_prefix(width));
    emit1(unit, 0x0F);
    emit1(unit, 0x11);
    emit1(unit, 0b10_000_100 | (xmm & 7) << 3);
    emit1(unit, 0b00_100_100);
    emit4(unit, (slot * 8) as u32);
}

/// `movq xmm, r64`：把 64 位位模式搬进 XMM。`66 REX.W 0F 6E /r`
fn emit_movq_gpr_to_xmm(unit: &mut CompilationUnit, xmm: u8, gpr: Gpr) {
    emit1(unit, 0x66);
    emit1(unit, 0x48 | if gpr.rex_bit() { 0x01 } else { 0 });
    emit1(unit, 0x0F);
    emit1(unit, 0x6E);
    modrm(unit, 3, xmm & 7, gpr.bits());
}

/// `movd xmm, r32`：把 32 位位模式搬进 XMM 低双字。`66 0F 6E /r`
fn emit_movd_gpr_to_xmm(unit: &mut CompilationUnit, xmm: u8, gpr: Gpr) {
    emit1(unit, 0x66);
    if gpr.rex_bit() {
        emit1(unit, 0x41);
    }
    emit1(unit, 0x0F);
    emit1(unit, 0x6E);
    modrm(unit, 3, xmm & 7, gpr.bits());
}

/// 浮点算术：`opss/opsd xmm1, xmm2`。opcode 0F 5x /r（58 add / 5C sub / 59 mul / 5E div）。
fn emit_sse_arith(unit: &mut CompilationUnit, width: u8, opcode: u8, dst: u8, src: u8) {
    emit1(unit, sse_prefix(width));
    emit1(unit, 0x0F);
    emit1(unit, opcode);
    modrm(unit, 3, dst & 7, src & 7);
}

/// `xorps/xorpd xmm1, xmm2`（0F 57 /r）——用来给浮点取负（翻符号位）。
fn emit_xor_sign(unit: &mut CompilationUnit, width: u8, dst: u8, src: u8) {
    if width != 32 {
        emit1(unit, 0x66);
    }
    emit1(unit, 0x0F);
    emit1(unit, 0x57);
    modrm(unit, 3, dst & 7, src & 7);
}

/// `ucomiss/ucomisd xmm1, xmm2`（0F 2E /r，f64 加 66 前缀）——浮点比较。
fn emit_ucomis(unit: &mut CompilationUnit, width: u8, a: u8, b: u8) {
    if width != 32 {
        emit1(unit, 0x66);
    }
    emit1(unit, 0x0F);
    emit1(unit, 0x2E);
    modrm(unit, 3, a & 7, b & 7);
}

/// 单双精度互转（`cvtss2sd` 用 F3 前缀，`cvtsd2ss` 用 F2 前缀，都是 0F 5A /r）。
fn emit_cvt_ss_dbl(unit: &mut CompilationUnit, src_is_ss: bool, dst: u8, src: u8) {
    emit1(unit, if src_is_ss { 0xF3 } else { 0xF2 });
    emit1(unit, 0x0F);
    emit1(unit, 0x5A);
    modrm(unit, 3, dst & 7, src & 7);
}

/// `cvtsi2ss/cvtsi2sd xmm, r64`：F3/F2 REX.W 0F 2A /r
fn emit_cvtsi2f(unit: &mut CompilationUnit, width: u8, xmm: u8, gpr: Gpr) {
    emit1(unit, sse_prefix(width));
    emit1(unit, 0x48 | if gpr.rex_bit() { 0x01 } else { 0 });
    emit1(unit, 0x0F);
    emit1(unit, 0x2A);
    modrm(unit, 3, xmm & 7, gpr.bits());
}

/// `cvttss2si/cvttsd2si r64, xmm`：F3/F2 REX.W 0F 2C /r（截断向零）
fn emit_cvttf2si(unit: &mut CompilationUnit, width: u8, gpr: Gpr, xmm: u8) {
    emit1(unit, sse_prefix(width));
    emit1(unit, 0x48 | if gpr.rex_bit() { 0x01 } else { 0 });
    emit1(unit, 0x0F);
    emit1(unit, 0x2C);
    modrm(unit, 3, gpr.bits(), xmm & 7);
}

/// 把一个浮点常量装进 `xmm`：位模式经 GPR / 立即数中转。
fn emit_float_const(unit: &mut CompilationUnit, xmm: u8, bits: u64, width: u8) {
    if width == 32 {
        // mov eax, imm32；movd xmm, eax
        emit1(unit, 0xb8);
        emit4(unit, bits as u32);
        emit_movd_gpr_to_xmm(unit, xmm, SCRATCH_AX);
    } else {
        // movabs rax, imm64；movq xmm, rax
        emit1(unit, 0x48);
        emit1(unit, 0xb8);
        emit8(unit, bits);
        emit_movq_gpr_to_xmm(unit, xmm, SCRATCH_AX);
    }
}

/// 扫出一个函数里所有浮点值，给每个值分配专属栈槽。
///
/// 浮点的"性"要穿过 `Copy` 与 `Phi` 传播：优化会把 `a + b` 的结果复制
/// 走（`x + 0 -> x`），循环里也会有 `φ`。只认"定义点"会让这些中转值
/// 被当成整数去 GPR 里找，然后拿到栈槽里残渣当浮点数用。
fn assign_float_slots(func: &SsaFunction, base: usize) -> usize {
    // 一遍"先播种、再传播"是不够的：`Neg` 的播种条件是"操作数已是浮点"，
    // 而操作数的浮点性要等 `Copy` 传播完才知道。所以把播种和传播合成
    // 一个不动点，每轮都重新扫全部指令。
    // (值 id -> 是否是浮点, 位宽)。位宽 0 表示还没推断出来。
    let mut floats: HashMap<u32, u8> = HashMap::new();
    let width_of =
        |v: &SsaValue, map: &HashMap<u32, u8>| -> u8 { map.get(&v.id()).copied().unwrap_or(0) };
    loop {
        let mut changed = false;
        {
            let mark = |v: &SsaValue, w: u8, map: &mut HashMap<u32, u8>| -> bool {
                match map.get(&v.id()) {
                    // 已知但更窄的位宽，保留更宽的那个（f64 优先）
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
                            // 常量自带位宽，直接播种（f32 的 `1.5` 就该按 32 位搬）。
                            if mark(result, *w, &mut floats) {
                                changed = true;
                            }
                        }
                        // 比较的结果是布尔，即使两个操作数都是浮点
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
                                if mark(result, w, &mut floats) {
                                    changed = true;
                                }
                                // 形参没有类型标注（`MirParam` 是空结构），只能从
                                // "谁在拿它做浮点运算"反推：浮点二元运算的操作数
                                // 一定是浮点。少了这一步，`fn f(a: f64, b: f64)`
                                // 的形参会按整数走 rdi/rsi，返回值也按位模式搬。
                                if mark(lhs, w, &mut floats) {
                                    changed = true;
                                }
                                if mark(rhs, w, &mut floats) {
                                    changed = true;
                                }
                            } else {
                                // 比较 / 逻辑运算的结果是 bool，但操作数可能都是
                                // 浮点。两边互相传播：`f < g` 里只要有一边已知是
                                // 浮点，另一边必然也是。
                                let lw = width_of(lhs, &floats);
                                let rw = width_of(rhs, &floats);
                                // 只有一边已知时，把已知的位宽推给另一边
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
                            // 结果是浮点 -> 操作数也是；反过来同理。
                            // 位宽原样传播——早先这里 `.max(64)` 把 f32 的
                            // 取负强行拔到 64 位，于是 `-x` 会按 f64 读 f32 的
                            // 槽，拿到完全不同的数。
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
                        SsaInstr::Call {
                            result, args, ty, ..
                        } => {
                            if ty.is_float() {
                                let w = match *ty {
                                    NumTy::Float(w) => w,
                                    _ => 64,
                                };
                                if mark(result, w, &mut floats) {
                                    changed = true;
                                }
                            }
                            // 被调函数的形参类型 IR 里没有，只能按"这个实参
                            // 已知是浮点"来传。调用方的浮点值会通过这里推给
                            // 形参，但形参自己若是浮点而调用方没传浮点，IR
                            // 里就无从得知——这是 `MirParam` 不带类型的代价。
                            let _ = args;
                        }
                        // 浮点 Load/Store 的结果/值也必须落进 XMM 栈槽，
                        // 否则状态机恢复浮点局部量时会把它当整数搬。
                        SsaInstr::Load { result, ty, .. } if ty.is_float() => {
                            let w = match *ty {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            if mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                        SsaInstr::Store { value, ty, .. } if ty.is_float() => {
                            let w = match *ty {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            if mark(value, w, &mut floats) {
                                changed = true;
                            }
                        }
                        SsaInstr::Cast {
                            result,
                            src,
                            from,
                            to,
                        } => {
                            // 结果是浮点 -> 目标类型就是位宽
                            if to.is_float() {
                                let w = match *to {
                                    NumTy::Float(w) => w,
                                    _ => 64,
                                };
                                if mark(result, w, &mut floats) {
                                    changed = true;
                                }
                            }
                            // 源是浮点 -> 结果是整数，但源本身要占槽
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
                            // 反向：结果是浮点 -> 源也是。
                            // 形参本身没有类型标注，只能靠这种反向推断：
                            // `fn f(a: f64, b: f64) { a + b }` 里 `%2 = copy %0`
                            // 的 %0 是形参，它的浮点性只能从 %2 反推出来。
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
    // REX 位必须分清 ModRM 的两个字段：`8B /r` 的 **reg 字段是目的**，
    // **r/m 字段是源**——而这里的 r/m 是 SIB，所以真正被扩展的是 SIB 的
    // **base**（`rsp`）。于是 REX.R 属于 dst，而 REX.B **必须留 0**。
    //
    // 把 REX.B 当成"目的寄存器需要扩展"来设（早先的写法）会在目的寄存器是
    // R8–R15 时把 SIB 的 base 从 `rsp` 换成 `r12`：装载溢出值时读的根本不是
    // 栈帧，而是某个被分配给活跃值的寄存器指向的内存 —— 直接段错误。
    let rex = 0x48u8 | if dst.rex_bit() { 0x04 } else { 0 };
    emit1(unit, rex);
    emit1(unit, 0x8B);
    // mod=10 (disp32), reg=dst, rm=100 (SIB)
    emit1(unit, 0b10_000_100 | dst.bits() << 3);
    // SIB: scale=00, index=100 (无索引), base=100 (rsp)
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

/// `lea dst, [rsp + 8*slot]`——被取地址的局部变量的地址。
fn emit_lea_slot(unit: &mut CompilationUnit, dst: Gpr, slot: usize) {
    emit1(unit, 0x48 | if dst.rex_bit() { 0x04 } else { 0 });
    emit1(unit, 0x8D);
    // mod=10 (disp32), reg=dst, r/m=100 (SIB)
    emit1(unit, 0b10_000_100 | dst.bits() << 3);
    emit1(unit, 0b00_100_100);
    emit4(unit, (slot * 8) as u32);
}

/// 内存操作的位宽与有无符号：`(字节数, 有符号)`。
fn mem_access(ty: NumTy) -> (usize, bool) {
    match ty {
        NumTy::Int(w) => ((w as usize) / 8, true),
        NumTy::Uint(w) => ((w as usize) / 8, false),
        // bool 只有一个字节；浮点不走内存 load/store（未实现）。
        NumTy::Bool => (1, false),
        NumTy::Float(w) => ((w as usize) / 8, false),
    }
}

/// `dst = [base]`，按 `ty` 的位宽/符号扩展。地址用 `[base + 0]`（SIB）。
fn emit_load_mem(unit: &mut CompilationUnit, dst: Gpr, base: Gpr, ty: NumTy) {
    let (bytes, signed) = mem_access(ty);
    let rex = 0x40 | if dst.rex_bit() { 0x04 } else { 0 } | if base.rex_bit() { 0x01 } else { 0 };
    match (bytes, signed) {
        (1, false) => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x0F);
            emit1(unit, 0xB6);
        }
        (1, true) => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x0F);
            emit1(unit, 0xBE);
        }
        (2, false) => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x0F);
            emit1(unit, 0xB7);
        }
        (2, true) => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x0F);
            emit1(unit, 0xBF);
        }
        // 32 位无符号：写 r32 自动零扩展到 64 位。
        (4, false) => {
            emit1(unit, rex);
            emit1(unit, 0x8B);
        }
        // 32 位有符号：`movsxd r64, dword`.
        (4, true) => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x63);
        }
        // 64 位。
        _ => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x8B);
        }
    }
    // mod=10 (disp32=0), reg=dst, r/m=100 (SIB)；SIB base=base、无 index。
    emit1(unit, 0b10_000_100 | dst.bits() << 3);
    emit1(unit, 0b00_100_000 | base.bits());
    emit4(unit, 0);
}

/// `[base] = rax`（只写低 `bytes` 字节）。
fn emit_store_mem(unit: &mut CompilationUnit, base: Gpr, ty: NumTy) {
    let (bytes, _) = mem_access(ty);
    let rex = 0x40 | if base.rex_bit() { 0x01 } else { 0 };
    match bytes {
        // byte：`mov [base], al`。
        1 => {
            if rex != 0x40 {
                emit1(unit, rex);
            }
            emit1(unit, 0x88);
        }
        // word：`mov [base], ax`。
        2 => {
            emit1(unit, 0x66);
            if rex != 0x40 {
                emit1(unit, rex);
            }
            emit1(unit, 0x89);
        }
        // dword：`mov [base], eax`。
        4 => {
            if rex != 0x40 {
                emit1(unit, rex);
            }
            emit1(unit, 0x89);
        }
        // qword：`mov [base], rax`。
        _ => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x89);
        }
    }
    // reg=0 (al/ax/eax/rax)，r/m=100 (SIB)。
    emit1(unit, 0b10_000_100);
    emit1(unit, 0b00_100_000 | base.bits());
    emit4(unit, 0);
}

/// 把 `v` 的 64 位表示搬进 `dst`（寄存器间 mov，或从栈槽装载）。
fn reg_into(unit: &mut CompilationUnit, v: SsaValue, dst: Gpr) {
    match location_of(v) {
        Location::Reg(i) => {
            let s = ALLOCATABLE[i];
            if s != dst {
                emit_mov_rr(unit, dst, s);
            }
        }
        Location::Stack(slot) => emit_load_slot(unit, dst, slot),
    }
}

/// `result = *ptr`。
fn emit_load(unit: &mut CompilationUnit, result: SsaValue, ptr: SsaValue, ty: NumTy) {
    let p = use_reg(unit, ptr);
    if let NumTy::Float(width) = ty {
        let Some(slot) = float_slot(result) else {
            crate::codegen::unsupported_cast(ty, ty);
        };
        emit_sse_mem_op(unit, Some(sse_prefix(width)), 0x10, 0, p);
        emit_store_float(unit, 0, slot, width);
        return;
    }
    let spilled = matches!(location_of(result), Location::Stack(_));
    let d = if spilled {
        SCRATCH_AX
    } else {
        alloc_reg(result)
    };
    emit_load_mem(unit, d, p, ty);
    if spilled {
        if let Location::Stack(slot) = location_of(result) {
            emit_store_slot(unit, d, slot);
        }
    }
}

/// `*ptr = value`。
fn emit_store(unit: &mut CompilationUnit, ptr: SsaValue, value: SsaValue, ty: NumTy) {
    // 先把地址放进一个稳定寄存器：溢出值或 rax 会经 `rcx` 中转（rcx 不参与
    // 分配，任何时刻都是空的）。否则直接用它的寄存器。
    let p = match location_of(ptr) {
        Location::Reg(i) => {
            let r = ALLOCATABLE[i];
            if r == SCRATCH_AX {
                emit_mov_rr(unit, SCRATCH_CX, SCRATCH_AX);
                SCRATCH_CX
            } else {
                r
            }
        }
        Location::Stack(slot) => {
            emit_load_slot(unit, SCRATCH_CX, slot);
            SCRATCH_CX
        }
    };
    if let NumTy::Float(width) = ty {
        let Some(slot) = float_slot(value) else {
            crate::codegen::unsupported_cast(ty, ty);
        };
        emit_load_float(unit, 0, slot, width);
        emit_sse_mem_op(unit, Some(sse_prefix(width)), 0x11, 0, p);
        return;
    }
    // 值搬进 rax（完整 64 位搬即可——小端下低字节就是要写的值）。
    reg_into(unit, value, SCRATCH_AX);
    emit_store_mem(unit, p, ty);
}

/// 扫出所有被取地址的局部变量，各分配一个栈槽（排在 `base` 之后）。
fn assign_local_slots(func: &SsaFunction, base: usize) -> usize {
    // 同一个 local id 可能出现多次；取最大的槽需求。
    let mut locals: Vec<(u32, u32)> = Vec::new();
    for block in &func.blocks {
        for instr in &block.instrs {
            if let SsaInstr::SlotAddr { local, size, .. } = instr {
                match locals.iter_mut().find(|(id, _)| id == local) {
                    Some((_, s)) => *s = (*s).max(*size),
                    None => locals.push((*local, *size)),
                }
            }
        }
    }
    locals.sort_unstable_by_key(|(id, _)| *id);
    let mut slots = 0usize;
    LOCAL_SLOTS.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for (id, size) in locals {
            map.insert(id, base + slots);
            slots += (size.max(8) as usize).div_ceil(8);
        }
    });
    slots
}
/// 发射一个函数的机器码。
pub fn emit_function(target: &Target, unit: &mut CompilationUnit, func: &SsaFunction) {
    debug_assert_eq!(
        target.arch(),
        Arch::X86_64,
        "x86-64 后端只能用于 x86_64 目标"
    );
    // 先做活跃区间分析 + 线性扫描，确定每个值落在哪个寄存器
    let alloc = crate::codegen::regalloc::allocate_registers(
        func,
        ALLOCATABLE.len(),
        &X86_64_CALLEE_SAVE,
        0,
        &HashSet::new(),
    );
    // callee-save 集合要在发射尾声时再用，所以这里留一份
    let callee_saved = callee_saved_regs(&alloc.map);
    // 整数溢出的槽之后接着排浮点槽，最后是被取地址的局部变量的槽：
    // 三类槽共用一个帧，序言只需一次 sub rsp。
    let float_slots = assign_float_slots(func, alloc.spilled);
    let local_slots = assign_local_slots(func, alloc.spilled + float_slots);
    let call_slot = alloc.spilled + float_slots + local_slots;
    CALL_SLOT.with(|c| c.set(call_slot));
    FRAME_SLOTS.with(|c| c.set(call_slot + 1));
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
        //
        // **条件分支块里，拷贝必须排在 `test` 之后**（见下面 Branch 分支）。
        // 拷贝写的是后继 φ 的目标寄存器，而条件值所在的寄存器在拷贝那一刻
        // 已经"用完"了（按活跃区间算它到这里就死了），于是两者可以分到同一个
        // 寄存器。拷贝在前的话，`mov` 会把条件覆盖掉，分支判的是错的值——
        // `while k < 10 { k += 1; if k == 5 { break } }` 在 k 非零时直接跳出。
        match &block.term {
            SsaTerminator::Return(ret_val) => {
                // 若有返回值，先 mov rax, <reg>。
                // 必须走 `use_reg`：返回值本身可能是溢出的值，它没有固定
                // 寄存器，直接 `alloc_reg` 会 panic。
                if let Some(v) = ret_val {
                    // 浮点返回值按 ABI 走 xmm0，不是 rax
                    if let Some(slot) = float_slot(*v) {
                        emit_load_float(unit, 0, slot, float_width(*v));
                    } else {
                        let r = use_reg(unit, *v);
                        emit_mov_rr(unit, SCRATCH_AX, r);
                    }
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
            SsaTerminator::Goto(target) => {
                emit_phi_copies(unit, func, *target, block.id);
                emit_jmp(unit, &block_label(&func.name, *target));
            }
            SsaTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                // 条件可能溢出在栈上，先装回 rax 再测。
                let r = use_reg(unit, *cond);
                emit_test_rr(unit, r, r);

                // φ 拷贝必须**按边**发射，不能在分支之前一股脑全发。
                //
                // 每个后继只在自己的前驱边上做拷贝：`while c { ... }` 展开成
                // `bb1 -> bb2`（真）与 `bb1 -> bb3`（假），而只有 bb3 有 φ。
                // 提前在分支前统一发射，拷贝会在**两条边上都执行**——包括
                // 根本不经过 bb3 的那条。于是 bb2 读到的循环变量寄存器已经被
                // 覆盖成别的值，累加器从某一轮起就不再正确。
                //
                //   test cond, cond
                //   jz   L_else
                //   <then 边的 φ 拷贝>      ; 条件为真
                //   jmp  then
                // L_else:
                //   <else 边的 φ 拷贝>      ; 条件为假
                //   jmp  else
                let else_label = format!("{}.else{}", func.name, block.id);
                emit_jcc_rel32(unit, 0x84, &else_label); // jz L_else
                emit_phi_copies(unit, func, *then_block, block.id);
                emit_jmp(unit, &block_label(&func.name, *then_block));
                // `else` 副本登记成一个局部标签（不出现在符号表里）。
                let else_start = unit.code.len();
                emit_phi_copies(unit, func, *else_block, block.id);
                emit_jmp(unit, &block_label(&func.name, *else_block));
                bind_local_label(unit, &else_label, else_start);
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
        SsaInstr::Binary {
            result,
            op,
            lhs,
            rhs,
            ty,
        } => emit_binary(unit, *result, *op, *lhs, *rhs, *ty),
        SsaInstr::Unary {
            result,
            op,
            operand,
        } => {
            // 一元运算的结果类型与操作数一致；用它核对浮点位宽
            let operand_w = float_width(*operand);
            emit_unary(unit, *result, *op, *operand, operand_w);
        }
        SsaInstr::Call {
            result,
            callee,
            args,
            ..
        } => {
            emit_call_args(unit, args);
            emit_call(unit, callee);
            // 浮点返回值按 ABI 在 xmm0，不是 rax
            if let Some(slot) = float_slot(*result) {
                emit_store_float(unit, 0, slot, float_width(*result));
            } else {
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
        }
        // φ 的实际拷贝由前驱末尾的 emit_phi_copies 发出
        SsaInstr::Phi { .. } => {}
        SsaInstr::Copy { result, src } => {
            // 浮点之间经 xmm0 中转
            if let (Some(rs), Some(ss)) = (float_slot(*result), float_slot(*src)) {
                let w = float_width(*result).max(float_width(*src));
                emit_load_float(unit, 0, ss, w);
                emit_store_float(unit, 0, rs, w);
                return;
            }
            let spilled = matches!(location_of(*result), Location::Stack(_));
            let d = if spilled {
                SCRATCH_AX
            } else {
                alloc_reg(*result)
            };
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
        SsaInstr::Cast {
            result,
            src,
            from,
            to,
        } => {
            // 任一侧是浮点就走 XMM 路径
            if from.is_float() || to.is_float() || float_slot(*src).is_some() {
                emit_cast_float(unit, *result, *src, *from, *to);
                return;
            }
            let spilled = matches!(location_of(*result), Location::Stack(_));
            let d = if spilled {
                SCRATCH_AX
            } else {
                alloc_reg(*result)
            };
            let s = use_reg(unit, *src);
            emit_cast(unit, d, s, *from, *to);
            if spilled {
                if let Location::Stack(slot) = location_of(*result) {
                    emit_store_slot(unit, d, slot);
                }
            }
        }
        SsaInstr::Load { result, ptr, ty } => emit_load(unit, *result, *ptr, *ty),
        SsaInstr::Store { ptr, value, ty } => emit_store(unit, *ptr, *value, *ty),
        SsaInstr::SlotAddr { result, local, .. } => {
            let slot = LOCAL_SLOTS.with(|m| m.borrow().get(local).copied().unwrap_or(0));
            // 结果溢出的值经 rax 写回栈槽。
            if let Location::Stack(s) = location_of(*result) {
                emit_lea_slot(unit, SCRATCH_AX, slot);
                emit_store_slot(unit, SCRATCH_AX, s);
            } else {
                emit_lea_slot(unit, alloc_reg(*result), slot);
            }
        }
        SsaInstr::SimdBinary {
            lhs,
            op,
            rhs,
            out,
            ty,
            lanes,
        } => emit_simd_binary(unit, *lhs, *op, *rhs, *out, *ty, *lanes),
        SsaInstr::Asm {
            template,
            outputs,
            inputs,
            ..
        } => emit_inline_asm(unit, template, outputs, inputs),
        SsaInstr::FuncAddr { result, target } => {
            let spilled = matches!(location_of(*result), Location::Stack(_));
            let d = if spilled {
                SCRATCH_AX
            } else {
                alloc_reg(*result)
            };
            emit_str_addr(unit, d, target);
            if spilled {
                if let Location::Stack(slot) = location_of(*result) {
                    emit_store_slot(unit, d, slot);
                }
            }
        }
        SsaInstr::CallIndirect {
            result,
            callee,
            args,
            ..
        } => {
            // 函数指针先落进专用栈槽：实参搬运可能覆盖它所在的寄存器。
            let c = use_reg(unit, *callee);
            let call_slot = CALL_SLOT.with(|c| c.get());
            emit_store_slot(unit, c, call_slot);
            emit_call_args(unit, args);
            emit_load_slot(unit, SCRATCH_AX, call_slot);
            emit_call_reg(unit, SCRATCH_AX);
            if let Some(slot) = float_slot(*result) {
                emit_store_float(unit, 0, slot, float_width(*result));
            } else {
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
        }
        SsaInstr::Var { .. } => {
            // Var 是 φ 合并前的引用，运行时无需动作
        }
    }
}

/// 浮点参与的类型转换。
///
/// 浮点值住在专用栈槽里，整数值住在 GPR 里，所以四条方向各是一套指令：
/// `cvtsi2sd` / `cvttsd2si`（都支持 64 位）与 `cvtss2sd` / `cvtsd2ss`。
fn emit_cast_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    src: SsaValue,
    from: NumTy,
    to: NumTy,
) {
    let dst_is_float = to.is_float();
    let src_is_float = from.is_float() || float_slot(src).is_some();
    // 源 / 目标的位宽。优先信 NumTy；它没给（如 `from` 是整数类型但值其实
    // 住在浮点槽里，或反之）就退回槽的实际位宽。
    let fw = match from {
        NumTy::Float(w) => w,
        _ => float_width(src),
    };
    let tw = match to {
        NumTy::Float(w) => w,
        _ => float_width(result),
    };

    // 源进 xmm0（浮点）或 rax（整数）
    let (src_xmm, src_gpr): (Option<u8>, Option<Gpr>) = if src_is_float {
        let sl = float_slot(src).expect("浮点源没有栈槽");
        emit_load_float(unit, 0, sl, if fw == 0 { 64 } else { fw });
        (Some(0), None)
    } else {
        let r = use_reg(unit, src);
        if r != SCRATCH_AX {
            emit_mov_rr(unit, SCRATCH_AX, r);
        }
        (None, Some(SCRATCH_AX))
    };

    if dst_is_float {
        let sl = float_slot(result).expect("浮点目的没有栈槽");
        if src_is_float {
            // 同宽直接照搬；跨宽用 cvtss2sd / cvtsd2ss（都是 0F 5A /r）
            if tw == 32 && fw != 32 {
                emit_cvt_ss_dbl(unit, false, 0, src_xmm.unwrap());
            } else if tw != 32 && fw == 32 {
                emit_cvt_ss_dbl(unit, true, 0, src_xmm.unwrap());
            }
        } else {
            // int -> f32/f64：cvtsi2ss / cvtsi2sd
            emit_cvtsi2f(unit, tw, 0, src_gpr.unwrap());
        }
        emit_store_float(unit, 0, sl, tw);
    } else if src_is_float {
        // f32/f64 -> int：cvttss2si / cvttsd2si（截断向零，与 Rust `as` 一致）
        let gpr = match location_of(result) {
            Location::Stack(_) => SCRATCH_AX,
            Location::Reg(i) => ALLOCATABLE[i],
        };
        emit_cvttf2si(unit, fw, gpr, 0);
        if let Location::Stack(slot) = location_of(result) {
            emit_store_slot(unit, SCRATCH_AX, slot);
        }
    }
}

/// 类型转换。
///
/// 值一律放在 64 位寄存器里，所以转换就是"取低 w 位再按目标的有无符号
/// 重新符号/零扩展到 64 位"。宽度已经是 64 时什么都不用做。
fn emit_cast(unit: &mut CompilationUnit, dst: Gpr, src: Gpr, from: NumTy, to: NumTy) {
    // 浮点参与的转换有专门路径（要用 XMM），这里只处理整数之间。
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
    let dst = if spilled {
        SCRATCH_AX
    } else {
        alloc_reg(result)
    };
    match value {
        SsaConst::Int(i) => {
            // movabs（REX.W + 0xB8 + rd + imm64）加载 64 位立即数
            let rex = 0x48u8 | if dst.rex_bit() { 0x01 } else { 0 };
            emit1(unit, rex);
            emit1(unit, 0xb8 + dst.bits());
            emit8(unit, i as u64);
        }
        // bool / unit 在通用寄存器里就是 0/1；字符串后端暂未实现
        SsaConst::Bool(b) => emit_const_bool(unit, dst, b),
        SsaConst::Unit => emit_const_int(unit, dst, 0),
        // 浮点根本不进 GPR：位模式直接搬进 XMM，再落到浮点专用栈槽
        SsaConst::Float(f, w) => {
            if let Some(slot) = float_slot(result) {
                emit_float_const(unit, 0, f, w);
                emit_store_float(unit, 0, slot, w);
            } else {
                crate::codegen::unsupported_const("float");
            }
        }
        // 字符串字面量：值是 `.rodata` 里那份字节的地址（指针）。
        SsaConst::Str(text) => {
            let sym = unit.intern_string(&text);
            emit_str_addr(unit, dst, &sym);
        }
    }
    if spilled {
        if let Location::Stack(slot) = location_of(result) {
            emit_store_slot(unit, dst, slot);
        }
    }
}

/// `lea dst, [rip + symbol]`：把 `.rodata` 符号的地址装进 `dst`。
///
/// 7 字节：`REX.W 8D /r`，modrm 的 mod=00、rm=101 表示 RIP 相对 disp32。
/// 位移字段附一条 `RipRel32` 重定位（附加数 -4），链接器与自构造可执行
/// 文件的回填路径都据此计算地址。
fn emit_str_addr(unit: &mut CompilationUnit, dst: Gpr, sym: &str) {
    let start = unit.code.len();
    // `8D /r` 的 **reg 字段是目的**，因此 R8–R15 要靠 REX.R（0x04）扩展；
    // 若误用 REX.B（0x01），低 3 位的 4/5 会被解码成 `rsp`/`rbp`，把栈指针
    // 与帧指针直接写坏。
    emit1(unit, 0x48 | if dst.rex_bit() { 0x04 } else { 0 });
    emit1(unit, 0x8d);
    emit1(unit, 0x05 | ((dst.bits() & 7) << 3));
    let field = unit.code.len();
    emit4(unit, 0);
    let mut reloc = Relocation::new(field, start, 7, sym, RelocationKind::RipRel32);
    reloc.addend = -4;
    unit.relocations.push(reloc);
}

/// 用 mov 立即数加载一个小整数（0/1 之类）。
fn emit_const_int(unit: &mut CompilationUnit, dst: Gpr, value: u64) {
    if value == 0 {
        // xor reg, reg —— 比 mov 短，且不破坏标志位。
        // reg==r/m 都是 dst：REX 要同时带上 REX.R 和 REX.B（R8+ 时得 0x09）。
        let rex = 0x48u8 | if dst.rex_bit() { 0x09 } else { 0x00 };
        emit1(unit, rex);
        emit1(unit, 0x31);
        emit1(unit, 0xc0 | (dst.bits() << 3) | dst.bits());
    } else {
        emit1(unit, 0x48u8 | if dst.rex_bit() { 0x01 } else { 0x00 });
        emit1(unit, 0xb8 + dst.bits());
        emit8(unit, value);
    }
}

/// 加载布尔常量。
fn emit_const_bool(unit: &mut CompilationUnit, dst: Gpr, value: bool) {
    emit_const_int(unit, dst, u64::from(value));
}

/// 发射二元运算。
fn emit_binary(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
    ty: NumTy,
) {
    use SsaBinaryOp::*;

    // 任何一个操作数是浮点，整条运算就都走 XMM。
    //
    // 判据是**操作数**而不是结果类型：比较的结果是整数，但 `f < g` 必须
    // 用 `ucomisd` 而不是 `cmp`——后者会比较两个 IEEE754 位模式当整数比，
    // 负数与 NaN 的序完全错掉。
    let lhs_is_f = float_slot(lhs).is_some();
    let rhs_is_f = float_slot(rhs).is_some();
    if lhs_is_f || rhs_is_f {
        emit_binary_float(unit, result, op, lhs, rhs, ty);
        return;
    }

    let result_spilled = matches!(location_of(result), Location::Stack(_));
    let dst = if result_spilled {
        SCRATCH_AX
    } else {
        alloc_reg(result)
    };

    match op {
        // -------------------------------------------------------------
        // 除法 / 取模：`rax` 放被除数，`rdx` 放余数（`cqo` 写 rdx）
        // -------------------------------------------------------------
        Div | Rem => {
            // 被除数 → rax
            match location_of(lhs) {
                Location::Reg(_) => emit_mov_rr(unit, SCRATCH_AX, alloc_reg(lhs)),
                Location::Stack(slot) => emit_load_slot(unit, SCRATCH_AX, slot),
            }
            // 除数：溢出时直接用内存操作数 `idiv [rsp+disp32]`。
            //
            // 不装进寄存器：rax 被除数占用，rdx 会被 `cqo` 写掉，而借一个
            // 分配池里的寄存器就得先 push 保护——push 让 rsp 偏移 8，之后
            // 所有 `[rsp+disp]` 形式的栈槽访问全部读错地址。
            let divisor = match location_of(rhs) {
                Location::Reg(_) => emit_divrem_reg(unit, dst, alloc_reg(rhs), op == Rem),
                Location::Stack(slot) => emit_divrem_mem(unit, dst, slot, op == Rem),
            };
            if result_spilled {
                if let Location::Stack(slot) = location_of(result) {
                    emit_store_slot(unit, divisor, slot);
                }
            }
        }

        // -------------------------------------------------------------
        // 移位：计数固定走 `cl`，被移位值进 dst
        // -------------------------------------------------------------
        Shl | Shr => {
            // 左操作数直接搬进 dst：`mov dst, l` 本来就要发，省一次搬运。
            match location_of(lhs) {
                Location::Reg(i) => {
                    let l = ALLOCATABLE[i];
                    if l != dst {
                        emit_mov_rr(unit, dst, l);
                    }
                }
                Location::Stack(slot) => emit_load_slot(unit, dst, slot),
            }
            // 计数：x86 的移位计数只取 `cl`，固定用 rcx。
            match location_of(rhs) {
                Location::Reg(i) => {
                    let r = ALLOCATABLE[i];
                    if r != SCRATCH_CX {
                        emit_mov_rr(unit, SCRATCH_CX, r);
                    }
                }
                Location::Stack(slot) => emit_load_slot(unit, SCRATCH_CX, slot),
            }
            // 4 = shl, 7 = sar（算术右移：IR 的 Shr 是有符号的）
            let code = if op == Shl { 4 } else { 7 };
            let mut rex = 0x48u8;
            if dst.rex_bit() {
                rex |= 0x01;
            }
            emit1(unit, rex);
            emit1(unit, 0xd3);
            modrm(unit, 3, code, dst.bits());
            if result_spilled {
                if let Location::Stack(slot) = location_of(result) {
                    emit_store_slot(unit, dst, slot);
                }
            }
        }

        // -------------------------------------------------------------
        // 比较：`cmp l, r; setcc al; movzx dst, al`
        // -------------------------------------------------------------
        Eq | Ne | Lt | Gt | Le | Ge => {
            // 两个操作数都装进暂存器。它们在 `cmp` 之后就没用了，所以结果
            // 暂存器可以与它们重合（`setcc` 写的是 al，`movzx dst, al` 在
            // `cmp` 之后才执行）。
            let l = load_scratch(unit, lhs, SCRATCH_AX);
            let r = load_scratch(unit, rhs, SCRATCH_DX);
            emit_cmp_rr(unit, l, r);
            emit_setcc(unit, condition_code(op), SCRATCH_AX);
            emit_movzx(unit, dst, SCRATCH_AX);
            if result_spilled {
                if let Location::Stack(slot) = location_of(result) {
                    emit_store_slot(unit, dst, slot);
                }
            }
        }

        // -------------------------------------------------------------
        // 其余 ALU：add / sub / mul / and / or / xor
        // -------------------------------------------------------------
        Add | Sub | Mul | BitAnd | BitOr | BitXor | And | Or => {
            // 左操作数进 dst（`mov dst, l` 本来就要发）。
            match location_of(lhs) {
                Location::Reg(i) => {
                    let l = ALLOCATABLE[i];
                    if l != dst {
                        emit_mov_rr(unit, dst, l);
                    }
                }
                Location::Stack(slot) => emit_load_slot(unit, dst, slot),
            }
            // 右操作数需要一个**不等于 dst** 的暂存器：等于 dst 时会发成
            // `add rax, rax`（把左操作数加了两次），而不是加上真正的右操作数。
            let scratch = if dst == SCRATCH_AX {
                SCRATCH_DX
            } else {
                SCRATCH_AX
            };
            let r = load_scratch(unit, rhs, scratch);
            if op == Mul {
                emit_mul(unit, dst, dst, r);
            } else if let Some(code) = alu_opcode(op) {
                emit_full_alu(unit, code, dst, r);
            }
            if result_spilled {
                if let Location::Stack(slot) = location_of(result) {
                    emit_store_slot(unit, dst, slot);
                }
            }
        }
    }
}

/// 取一个值的所在寄存器；溢出值先从栈槽装进 `scratch`。
///
/// `scratch` 必须与本条指令里的其它暂存器错开（见 [`emit_binary`]）。
fn load_scratch(unit: &mut CompilationUnit, v: SsaValue, scratch: Gpr) -> Gpr {
    match location_of(v) {
        Location::Reg(i) => ALLOCATABLE[i],
        Location::Stack(slot) => {
            emit_load_slot(unit, scratch, slot);
            scratch
        }
    }
}

/// 浮点二元运算：算术结果落浮点栈槽，比较结果落 GPR/整数槽。
fn emit_binary_float(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: SsaBinaryOp,
    lhs: SsaValue,
    rhs: SsaValue,
    _ty: NumTy,
) {
    use SsaBinaryOp::*;
    let slot = |v: SsaValue| float_slot(v).expect("浮点操作数没有栈槽");
    // 操作数位宽：比较的结果类型是 bool，所以只从两个操作数推。
    // 两边都推不出来时退回 _ty。
    let w = {
        let lw = float_width(lhs);
        let rw = float_width(rhs);
        let mut w = lw.max(rw);
        if w == 0 {
            w = match _ty {
                NumTy::Float(w) => w,
                _ => 64,
            };
        }
        w
    };
    // xmm0 = 左，xmm1 = 右
    emit_load_float(unit, 0, slot(lhs), w);
    emit_load_float(unit, 1, slot(rhs), w);

    match op {
        // 比较：结果是整数，写回整数一侧的位置
        Eq | Ne | Lt | Gt | Le | Ge => {
            // `ucomisd a, b` 设 CF/ZF/PF，NaN 时 CF=ZF=1。直接 `setcc` 会把
            // 任何与 NaN 的比较判成"小于"（CF=1），而 IEEE754 要求全是 false。
            // 解法是交换操作数后用"反向"的 setcc：
            //   a <  b  <=>  b >  a （seta：ZF=0 且 CF=0）
            //   a <= b  <=>  b >= a （setae：CF=0）
            // 这样 NaN 落在 CF=1 一侧，被两个 setcc 一致地判 false。
            // 相等 / 不等要额外看 PF：NaN 参与时 `ucomisd` 会把 ZF 和 PF
            // 一起置 1，光用 `sete` 会把 `NaN == NaN` 判成真（IEEE754 要求
            // 为假）。所以 Eq 是 `sete && setnp`，Ne 是 `setne || setp`。
            //
            // 小于 / 大于方向已经用"交换操作数 + 反向 setcc"绕开了：
            // `a < b` 编成 `ucomisd b, a; seta`，NaN 时 CF=1 使 seta 为假。
            let (left, right) = match op {
                Eq | Ne => (0u8, 1u8),
                Lt | Le => (1, 0),
                Gt | Ge => (0, 1),
                _ => unreachable!(),
            };
            emit_ucomis(unit, w, left, right);
            // al 与 dl 都不参与分配（整数比较路径也用这两个做暂存器），
            // 此刻没有待发射的实参搬运，所以取它们是安全的。
            let al = SCRATCH_AX.bits();
            let dl = SCRATCH_DX.bits();
            match op {
                Eq => {
                    emit_setcc(unit, 0x94, SCRATCH_AX); // sete  al
                    emit_setcc(unit, 0x9B, SCRATCH_DX); // setnp dl
                    emit1(unit, 0x20); // and r/m8, r8  ->  and al, dl
                    emit1(unit, 0xC0 | (dl << 3) | al);
                }
                Ne => {
                    emit_setcc(unit, 0x95, SCRATCH_AX); // setne al
                    emit_setcc(unit, 0x9A, SCRATCH_DX); // setp  dl
                    emit1(unit, 0x08); // or r/m8, r8   ->  or al, dl
                    emit1(unit, 0xC0 | (dl << 3) | al);
                }
                Lt | Le | Gt | Ge => {
                    let cc = match op {
                        Lt => 0x97u8, // seta（交换后：b > a）
                        Gt => 0x97,   // seta
                        Le => 0x93,   // setae（交换后：b >= a）
                        Ge => 0x93,   // setae
                        _ => unreachable!(),
                    };
                    emit_setcc(unit, cc, SCRATCH_AX);
                }
                _ => unreachable!(),
            }
            if let Location::Stack(sl) = location_of(result) {
                emit_store_slot(unit, SCRATCH_AX, sl);
                return;
            }
            let dst = alloc_reg(result);
            if dst != SCRATCH_AX {
                emit_movzx(unit, dst, SCRATCH_AX);
            }
        }
        Add | Sub | Mul | Div => {
            // F2 0F 58 addsd / 5C subsd / 59 mulsd / 5E divsd
            let opcode = match op {
                Add => 0x58,
                Sub => 0x5C,
                Mul => 0x59,
                Div => 0x5E,
                _ => unreachable!(),
            };
            emit_sse_arith(unit, w, opcode, 0, 1);
            if let Some(sl) = float_slot(result) {
                emit_store_float(unit, 0, sl, w);
            } else {
                crate::codegen::unsupported_const("float result");
            }
        }
        // 浮点没有整数的位运算 / 逻辑运算；`Rem` 也先不做。
        // 宁可报错也不能静默发出 0。
        Shl | Shr | BitAnd | BitOr | BitXor | And | Or | Rem => {
            crate::codegen::unsupported_binary_op(op)
        }
    }
}

/// `mov rax, l; cqo; idiv r`（除数在寄存器里），结果写到 `dst`。
fn emit_divrem_reg(unit: &mut CompilationUnit, dst: Gpr, r: Gpr, want_rem: bool) -> Gpr {
    // 必须用 `cqo`（rax 的符号扩展进 rdx），**不能**用 `xor edx, edx`：
    // 后者只在被除数为非负时等价。负数除法会算出完全错误的商和余数。
    emit2(unit, 0x48, 0x99); // cqo
    let mut rex = 0x48u8;
    if r.rex_bit() {
        rex |= 0x01;
    }
    emit1(unit, rex);
    emit1(unit, 0xf7);
    modrm(unit, 3, 7, r.bits()); // /7 = IDIV
    let src = if want_rem { SCRATCH_DX } else { SCRATCH_AX };
    if dst != src {
        emit_mov_rr(unit, dst, src);
    }
    dst
}

/// `mov rax, l; cqo; idiv qword ptr [rsp+disp32]`（除数溢出到栈上）。
fn emit_divrem_mem(unit: &mut CompilationUnit, dst: Gpr, slot: usize, want_rem: bool) -> Gpr {
    emit2(unit, 0x48, 0x99); // cqo
    emit1(unit, 0x48); // REX.W
    emit1(unit, 0xf7);
    // mod=10 (disp32), reg=111 (/7 = IDIV), rm=100 (SIB)
    modrm(unit, 2, 7, 4);
    // SIB: scale=00, index=100 (无索引), base=100 (rsp)
    emit1(unit, 0b00_100_100);
    emit4(unit, (slot * 8) as u32);
    let src = if want_rem { SCRATCH_DX } else { SCRATCH_AX };
    if dst != src {
        emit_mov_rr(unit, dst, src);
    }
    dst
}

/// 发射一元运算。
fn emit_unary(
    unit: &mut CompilationUnit,
    result: SsaValue,
    op: crate::ssa::SsaUnaryOp,
    operand: SsaValue,
    _operand_width: u8,
) {
    use crate::ssa::SsaUnaryOp::*;

    // 浮点取负：没有 `neg` 指令，翻符号位即可。
    //
    // 只有 `Neg` 有浮点语义。`Not` / `BitNot` 在浮点上没有定义——如果它们
    // 走到这里，说明上游把一个布尔/整数误标成了浮点；翻符号位会静默给出
    // 一个"能跑"的错值，所以直接报错。
    if let Some(slot) = float_slot(operand) {
        if !matches!(op, Neg) {
            crate::codegen::unsupported_const("非 Neg 的一元浮点运算");
        }
        let w = float_width(operand);
        emit_load_float(unit, 0, slot, w);
        // 符号位掩码（f32 取低 32 位的 0x8000_0000）经 GPR 搬进 xmm1，再异或
        let mask = if w == 32 {
            0x8000_0000u64
        } else {
            0x8000_0000_0000_0000u64
        };
        emit_float_const(unit, 1, mask, w);
        emit_xor_sign(unit, w, 0, 1);
        if let Some(rs) = float_slot(result) {
            emit_store_float(unit, 0, rs, w);
        }
        return;
    }

    let spilled = matches!(location_of(result), Location::Stack(_));
    let dst = if spilled {
        SCRATCH_AX
    } else {
        alloc_reg(result)
    };
    let r = match location_of(operand) {
        Location::Reg(i) => ALLOCATABLE[i],
        Location::Stack(slot) => {
            emit_load_slot(unit, SCRATCH_DX, slot);
            SCRATCH_DX
        }
    };

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
        Add => 0,          // add
        Sub => 5,          // sub
        BitAnd | And => 4, // and
        BitOr | Or => 1,   // or
        BitXor => 6,       // xor
        Mul | Div | Rem | Shl | Shr => return None,
        Eq | Ne | Lt | Gt | Le | Ge => return None,
    })
}

/// 返回条件码（用于 setcc）。
fn condition_code(op: SsaBinaryOp) -> u8 {
    use SsaBinaryOp::*;
    match op {
        Eq => 0x94, // sete
        Ne => 0x95, // setne
        Lt => 0x9c, // setl（有符号小于）
        Gt => 0x9f, // setg
        Le => 0x9e, // setle
        Ge => 0x9d, // setge
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
    if src.rex_bit() {
        rex |= 0x04;
    } // REX.R
    if dst.rex_bit() {
        rex |= 0x01;
    } // REX.B
    emit1(unit, rex);
    emit1(unit, 0x89);
    modrm(unit, 3, src.bits(), dst.bits());
}

/// `test r64, r64`：REX.W + 0x85 /r  (85 = MR, reg=src, r/m=dst; 计算 src & dst)
fn emit_test_rr(unit: &mut CompilationUnit, a: Gpr, b: Gpr) {
    let mut rex = 0x48u8;
    if a.rex_bit() {
        rex |= 0x04;
    }
    if b.rex_bit() {
        rex |= 0x01;
    }
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
    if b.rex_bit() {
        rex |= 0x04;
    } // REX.R <- reg 字段（b）
    if a.rex_bit() {
        rex |= 0x01;
    } // REX.B <- rm  字段（a）
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
    if dst.rex_bit() {
        rex |= 0x01;
    }
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
    if dst.rex_bit() {
        rex |= 0x04;
    } // REX.R <- reg 字段（dst）
    if src.rex_bit() {
        rex |= 0x01;
    } // REX.B <- rm  字段（src）
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
    // SysV：整数实参与浮点实参各自占一套寄存器、各自计数。
    // `f(g(1, 2.0), 3)` 里 1 在 rdi、2.0 在 xmm0、3 在 rsi。
    let mut moves: Vec<(Gpr, Src)> = Vec::new();
    let mut gi = 0usize; // 整数形参下标
    let mut xi = 0usize; // 浮点形参下标
    for a in args {
        if let Some(slot) = float_slot(*a) {
            if xi < 8 {
                emit_load_float(unit, xi as u8, slot, float_width(*a));
            }
            // 超过 8 个浮点实参走栈上传参区，本后端尚未实现。
            xi += 1;
        } else {
            if gi < ARG_REGS.len() {
                let src = src_of(*a);
                if !matches!(src, Src::Reg(r) if r == ARG_REGS[gi]) {
                    moves.push((ARG_REGS[gi], src));
                }
                gi += 1;
            }
            // 超过 6 个整数实参走栈上传参区，本后端尚未实现。
        }
    }
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
    // 浮点形参走 xmm0..、整数形参走 rdi..，两类各自计数（SysV ABI）。
    let mut gi = 0usize;
    let mut xi = 0usize;
    let mut moves: Vec<(Gpr, Src)> = Vec::new();
    for p in &func.params {
        if let Some(sl) = float_slot(*p) {
            if xi < 8 {
                emit_store_float(unit, xi as u8, sl, float_width(*p));
            }
            xi += 1;
            continue;
        }
        if gi >= ARG_REGS.len() {
            // 超过 6 个整数形参走栈上传参区，本后端尚未实现。
            continue;
        }
        match location_of(*p) {
            // 溢出形参没有固定寄存器：从 ABI 寄存器直接落进栈槽。这些
            // store 只写内存、不碰寄存器，可以先于寄存器搬运做。
            Location::Stack(slot) => emit_store_slot(unit, ARG_REGS[gi], slot),
            Location::Reg(d) if ALLOCATABLE[d] != ARG_REGS[gi] => {
                moves.push((ALLOCATABLE[d], Src::Reg(ARG_REGS[gi])));
            }
            Location::Reg(_) => {}
        }
        gi += 1;
    }
    // 寄存器搬运必须并行：落点可能覆盖后面还要读的 ABI 源寄存器（例如第
    // 5、6 个形参在 r8/r9，而中间某步的落点正好也是 r8/r9）。顺序搬运会
    // 踩掉尚未读取的源。
    emit_parallel_moves(unit, &moves);
}

/// `call rel32`：0xE8 + imm32（需重定位）。
fn emit_call(unit: &mut CompilationUnit, target: &str) {
    let instr_start = unit.code.len();
    emit1(unit, 0xe8);
    emit4(unit, 0);
    push_reloc(unit, instr_start, 5, target);
}

/// `call r64`：`FF /2`。
fn emit_call_reg(unit: &mut CompilationUnit, target: Gpr) {
    if target.rex_bit() {
        emit1(unit, 0x41);
    }
    emit1(unit, 0xff);
    emit1(unit, 0b11_010_000 | target.bits());
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
pub fn emit_start_wrapper(target: &Target, unit: &mut CompilationUnit, run_tasks: bool) {
    debug_assert_eq!(target.arch(), Arch::X86_64);
    let start = unit.code.len();

    emit_call(unit, "main");
    if run_tasks {
        // 主函数返回后把剩余任务跑完；r12 是跨调用保留寄存器。
        emit_mov_rr(unit, Gpr::R12, Gpr::Rax);
        emit_call(unit, "zr_run_tasks");
        emit_mov_rr(unit, Gpr::Rax, Gpr::R12);
    }

    match target.os() {
        Os::Linux => {
            // mov edi, eax（main 的返回值作为退出码）
            emit2(unit, 0x89, 0xc7);
            // mov eax, __NR_exit（60）
            emit1(unit, 0xb8);
            emit4(unit, target.os().exit_syscall(Arch::X86_64));
            // syscall
            emit2(unit, 0x0f, 0x05);
        }
        Os::FreeBsd => {
            // FreeBSD 的系统调用约定与 Linux 相同（号在 rax，参数在
            // rdi/rsi/rdx/r10/r8/r9），`SYS_exit` 也正好是 60。
            emit2(unit, 0x89, 0xc7);
            emit1(unit, 0xb8);
            emit4(unit, target.os().exit_syscall(Arch::X86_64));
            emit2(unit, 0x0f, 0x05);
        }
        Os::Android => {
            // Android 走 Bionic，x86_64 上和 Linux 一样用 `syscall`，
            // 但要用 `exit_group`（231）：只调 `exit`(60) 的话线程不会被
            // 回收，Bionic 的运行时会在退出时挂住。
            emit2(unit, 0x89, 0xc7);
            emit1(unit, 0xb8);
            emit4(unit, target.os().exit_syscall(Arch::X86_64));
            emit2(unit, 0x0f, 0x05);
        }
        Os::Darwin | Os::Ios => {
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
    if dst.rex_bit() {
        rex |= 0x04;
    }
    if r.rex_bit() {
        rex |= 0x01;
    }
    emit1(unit, rex);
    emit2(unit, 0x0f, 0xaf);
    modrm(unit, 3, dst.bits(), r.bits());
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
    if src.rex_bit() {
        rex |= 0x04;
    }
    if dst.rex_bit() {
        rex |= 0x01;
    }
    emit1(unit, rex);
    emit1(unit, opcode);
    modrm(unit, 3, src.bits(), dst.bits());
}

// ======================================================================
// 内联汇编（`asm!`）
// ======================================================================

/// 汇编操作数可用的寄存器池（顺序固定，保证产物确定）。
const ASM_REG_POOL: [Gpr; 13] = [
    Gpr::R10,
    Gpr::R11,
    Gpr::R12,
    Gpr::R13,
    Gpr::R14,
    Gpr::R15,
    Gpr::R8,
    Gpr::R9,
    Gpr::Rsi,
    Gpr::Rdi,
    Gpr::Rax,
    Gpr::Rcx,
    Gpr::Rdx,
];

/// 内联汇编期间需要保存/恢复的寄存器（编译器可能存放活跃值）。
const ASM_SAVED: [Gpr; 13] = [
    Gpr::Rax,
    Gpr::Rcx,
    Gpr::Rdx,
    Gpr::Rsi,
    Gpr::Rdi,
    Gpr::R8,
    Gpr::R9,
    Gpr::R10,
    Gpr::R11,
    Gpr::R12,
    Gpr::R13,
    Gpr::R14,
    Gpr::R15,
];

fn gpr_by_name(name: &str) -> Option<Gpr> {
    let n = name.trim().to_ascii_lowercase();
    Some(match n.as_str() {
        "rax" | "eax" | "ax" | "al" => Gpr::Rax,
        "rcx" | "ecx" | "cx" | "cl" => Gpr::Rcx,
        "rdx" | "edx" | "dx" | "dl" => Gpr::Rdx,
        "rsi" | "esi" | "si" | "sil" => Gpr::Rsi,
        "rdi" | "edi" | "di" | "dil" => Gpr::Rdi,
        "r8" | "r8d" | "r8w" | "r8b" => Gpr::R8,
        "r9" | "r9d" | "r9w" | "r9b" => Gpr::R9,
        "r10" | "r10d" | "r10w" | "r10b" => Gpr::R10,
        "r11" | "r11d" | "r11w" | "r11b" => Gpr::R11,
        "r12" | "r12d" | "r12w" | "r12b" => Gpr::R12,
        "r13" | "r13d" | "r13w" | "r13b" => Gpr::R13,
        "r14" | "r14d" | "r14w" | "r14b" => Gpr::R14,
        "r15" | "r15d" | "r15w" | "r15b" => Gpr::R15,
        _ => return None,
    })
}

fn xmm_by_name(name: &str) -> Option<u8> {
    let n = name.trim().to_ascii_lowercase();
    let rest = n.strip_prefix("xmm")?;
    rest.parse::<u8>().ok().filter(|v| *v < 16)
}

fn gpr_name(r: Gpr) -> &'static str {
    match r {
        Gpr::Rax => "rax",
        Gpr::Rcx => "rcx",
        Gpr::Rdx => "rdx",
        Gpr::Rsi => "rsi",
        Gpr::Rdi => "rdi",
        Gpr::R8 => "r8",
        Gpr::R9 => "r9",
        Gpr::R10 => "r10",
        Gpr::R11 => "r11",
        Gpr::R12 => "r12",
        Gpr::R13 => "r13",
        Gpr::R14 => "r14",
        Gpr::R15 => "r15",
    }
}

/// `push r64`（低 8 个寄存器不带 REX.B）。
fn emit_push64(unit: &mut CompilationUnit, r: Gpr) {
    if r.rex_bit() {
        emit1(unit, 0x41);
    }
    emit1(unit, 0x50 + r.bits());
}

/// `pop r64`。
fn emit_pop64(unit: &mut CompilationUnit, r: Gpr) {
    if r.rex_bit() {
        emit1(unit, 0x41);
    }
    emit1(unit, 0x58 + r.bits());
}

/// `mov r64, imm64`。
fn emit_mov_imm64(unit: &mut CompilationUnit, dst: Gpr, imm: i64) {
    let rex = 0x48u8 | if dst.rex_bit() { 0x01 } else { 0 };
    emit1(unit, rex);
    emit1(unit, 0xb8 + dst.bits());
    emit8(unit, imm as u64);
}

/// `mov [rsp + disp32], src`，按 `ty` 位宽写入。
fn emit_store_mem_reg(unit: &mut CompilationUnit, base: Gpr, src: Gpr, ty: NumTy) {
    let (bytes, _) = mem_access(ty);
    let rex = 0x40 | if src.rex_bit() { 0x04 } else { 0 } | if base.rex_bit() { 0x01 } else { 0 };
    match bytes {
        1 => {
            if rex != 0x40 {
                emit1(unit, rex);
            }
            emit1(unit, 0x88);
        }
        2 => {
            emit1(unit, 0x66);
            if rex != 0x40 {
                emit1(unit, rex);
            }
            emit1(unit, 0x89);
        }
        4 => {
            if rex != 0x40 {
                emit1(unit, rex);
            }
            emit1(unit, 0x89);
        }
        _ => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x89);
        }
    }
    // mod=10 (disp32)，rm=100 (SIB)；SIB base=base、无 index。
    emit1(unit, 0b10_000_100 | (src.bits() << 3));
    emit1(unit, 0b00_100_000 | base.bits());
    emit4(unit, 0);
}

/// `mov dst, [rsp + disp]`，按 `ty` 位宽/符号扩展。
fn emit_load_mem_rsp(unit: &mut CompilationUnit, dst: Gpr, disp: i32, ty: NumTy) {
    let (bytes, signed) = mem_access(ty);
    let rex = 0x40 | if dst.rex_bit() { 0x04 } else { 0 };
    match (bytes, signed) {
        (1, false) => {
            emit1(unit, rex | 0x08);
            emit2(unit, 0x0f, 0xb6);
        }
        (1, true) => {
            emit1(unit, rex | 0x08);
            emit2(unit, 0x0f, 0xbe);
        }
        (2, false) => {
            emit1(unit, rex | 0x08);
            emit2(unit, 0x0f, 0xb7);
        }
        (2, true) => {
            emit1(unit, rex | 0x08);
            emit2(unit, 0x0f, 0xbf);
        }
        (4, false) => {
            emit1(unit, rex);
            emit1(unit, 0x8b);
        }
        (4, true) => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x63);
        }
        _ => {
            emit1(unit, rex | 0x08);
            emit1(unit, 0x8b);
        }
    }
    emit1(unit, 0b10_000_100 | (dst.bits() << 3));
    emit1(unit, 0b00_100_100);
    emit4(unit, disp as u32);
}

/// `op dst, imm32`（`add` / `sub` / `and` / `or` / `xor` / `cmp`）。
fn emit_alu_imm(unit: &mut CompilationUnit, ext: u8, dst: Gpr, imm: i64, _cmp: bool) {
    let rex = 0x48u8 | if dst.rex_bit() { 0x01 } else { 0 };
    emit1(unit, rex);
    emit1(unit, 0x81);
    modrm(unit, 3, ext, dst.bits());
    emit4(unit, imm as u32);
}

/// `inc` / `dec` / `not` / `neg`（`FF` 或 `F7` 组）。
fn emit_group3(unit: &mut CompilationUnit, opcode: u8, ext: u8, dst: Gpr) {
    let rex = 0x48u8 | if dst.rex_bit() { 0x01 } else { 0 };
    emit1(unit, rex);
    emit1(unit, opcode);
    modrm(unit, 3, ext, dst.bits());
}

/// 解析汇编立即数：十进制、`0x` 十六进制、可选负号。
fn parse_imm(text: &str) -> Option<i64> {
    let t = text.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let value = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()?
    } else {
        t.parse::<i64>().ok()?
    };
    Some(if neg { -value } else { value })
}

/// 把 `{0}` / `{1}` 换成操作数寄存器名，并处理 `{{` / `}}` 转义。
fn substitute_template(template: &str, regs: &[Gpr]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                    continue;
                }
                let mut digits = String::new();
                for d in chars.by_ref() {
                    if d == '}' {
                        break;
                    }
                    digits.push(d);
                }
                if let Ok(idx) = digits.parse::<usize>() {
                    if let Some(r) = regs.get(idx) {
                        out.push_str(gpr_name(*r));
                        continue;
                    }
                }
                out.push('{');
                out.push_str(&digits);
                out.push('}');
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                out.push('}');
            }
            _ => out.push(c),
        }
    }
    out
}

/// 发射一条 SSE 寄存器-寄存器指令。`prefix` 为 `66` / `F3` / `F2`。
/// `movss/movsd/movups/movdqa/movdqu` 的内存形式：`op xmm, [base]` / `op [base], xmm`。
///
/// `prefix` 是 legacy 前缀（`66`/`F3`/`F2`），无前缀传 `None`。`base` 为
/// `rsp/r12` 时补 SIB；为 `rbp/r13` 时补 disp8=0。
fn emit_sse_mem_op(unit: &mut CompilationUnit, prefix: Option<u8>, opcode: u8, xmm: u8, base: Gpr) {
    let mut rex = 0x40u8;
    if xmm >= 8 {
        rex |= 0x04;
    }
    if base.rex_bit() {
        rex |= 0x01;
    }
    if let Some(p) = prefix {
        emit1(unit, p);
    }
    if rex != 0x40 {
        emit1(unit, rex);
    }
    emit2(unit, 0x0f, opcode);
    let b = base.bits();
    // mod=00 且 rm=5 表示 RIP 相对，`rbp/r13` 都必须改成 disp8=0。
    let (mod_bits, rm) = if b == 5 { (1, 5) } else { (0, b) };
    modrm(unit, mod_bits, xmm & 7, rm);
    if b == 4 {
        emit1(unit, 0b00_100_100);
    }
    if mod_bits == 1 {
        emit1(unit, 0);
    }
}

fn emit_sse_rr(unit: &mut CompilationUnit, prefix: Option<u8>, opcode: u8, dst: u8, src: u8) {
    let mut rex = 0x40u8;
    if dst >= 8 {
        rex |= 0x04;
    }
    if src >= 8 {
        rex |= 0x01;
    }
    if let Some(p) = prefix {
        emit1(unit, p);
    }
    if rex != 0x40 {
        emit1(unit, rex);
    }
    emit2(unit, 0x0f, opcode);
    modrm(unit, 3, dst, src);
}

/// 发射带一个 8 位立即数的 SSE 指令（如 `pslld xmm, imm8`）。
fn emit_sse_imm(
    unit: &mut CompilationUnit,
    prefix: Option<u8>,
    opcode: u8,
    ext: u8,
    dst: u8,
    imm: u8,
) {
    let mut rex = 0x40u8;
    if dst >= 8 {
        rex |= 0x01;
    }
    if let Some(p) = prefix {
        emit1(unit, p);
    }
    if rex != 0x40 {
        emit1(unit, rex);
    }
    emit2(unit, 0x0f, opcode);
    modrm(unit, 3, ext, dst);
    emit1(unit, imm);
}

/// `movd` / `movq`：GPR <-> XMM。
fn emit_mov_gpr_xmm(unit: &mut CompilationUnit, xmm: u8, gpr: Gpr, qword: bool, to_xmm: bool) {
    let mut rex = 0x40u8;
    if qword {
        rex |= 0x08;
    }
    if xmm >= 8 {
        rex |= 0x04;
    }
    if gpr.rex_bit() {
        rex |= 0x01;
    }
    emit1(unit, 0x66);
    if rex != 0x40 {
        emit1(unit, rex);
    }
    emit1(unit, 0x0f);
    // 6E: movd/movq xmm, r/m；7E: movd/movq r/m, xmm。
    emit1(unit, if to_xmm { 0x6e } else { 0x7e });
    modrm(unit, 3, xmm, gpr.bits());
}

/// SSE 二元/一元指令表：`(前缀, 操作码)`。第二个操作数既可以是 XMM，也可以是
/// 显式 GPR（`movd`/`movq` 单独处理）。
fn sse_rr_opcode(name: &str) -> Option<(Option<u8>, u8)> {
    Some(match name {
        "movaps" => (None, 0x28),
        "movapd" => (Some(0x66), 0x28),
        "movups" => (None, 0x10),
        "movupd" => (Some(0x66), 0x10),
        "movdqa" => (Some(0x66), 0x6f),
        "movdqu" => (Some(0xf3), 0x6f),
        "addps" => (None, 0x58),
        "addpd" => (Some(0x66), 0x58),
        "addss" => (Some(0xf3), 0x58),
        "addsd" => (Some(0xf2), 0x58),
        "subps" => (None, 0x5c),
        "subpd" => (Some(0x66), 0x5c),
        "subss" => (Some(0xf3), 0x5c),
        "subsd" => (Some(0xf2), 0x5c),
        "mulps" => (None, 0x59),
        "mulpd" => (Some(0x66), 0x59),
        "mulss" => (Some(0xf3), 0x59),
        "mulsd" => (Some(0xf2), 0x59),
        "divps" => (None, 0x5e),
        "divpd" => (Some(0x66), 0x5e),
        "divss" => (Some(0xf3), 0x5e),
        "divsd" => (Some(0xf2), 0x5e),
        "minps" => (None, 0x5d),
        "minpd" => (Some(0x66), 0x5d),
        "maxps" => (None, 0x5f),
        "maxpd" => (Some(0x66), 0x5f),
        "sqrtps" => (None, 0x51),
        "sqrtpd" => (Some(0x66), 0x51),
        "andps" => (None, 0x54),
        "andpd" => (Some(0x66), 0x54),
        "andnps" => (None, 0x55),
        "andnpd" => (Some(0x66), 0x55),
        "orps" => (None, 0x56),
        "orpd" => (Some(0x66), 0x56),
        "xorps" => (None, 0x57),
        "xorpd" => (Some(0x66), 0x57),
        "paddb" => (Some(0x66), 0xfc),
        "paddw" => (Some(0x66), 0xfd),
        "paddd" => (Some(0x66), 0xfe),
        "paddq" => (Some(0x66), 0xd4),
        "psubb" => (Some(0x66), 0xf8),
        "psubw" => (Some(0x66), 0xf9),
        "psubd" => (Some(0x66), 0xfa),
        "psubq" => (Some(0x66), 0xfb),
        "pmullw" => (Some(0x66), 0xd5),
        "pmuludq" => (Some(0x66), 0xf4),
        "pxor" => (Some(0x66), 0xef),
        "pand" => (Some(0x66), 0xdb),
        "pandn" => (Some(0x66), 0xdf),
        "por" => (Some(0x66), 0xeb),
        "pcmpeqb" => (Some(0x66), 0x74),
        "pcmpeqw" => (Some(0x66), 0x75),
        "pcmpeqd" => (Some(0x66), 0x76),
        "pcmpgtb" => (Some(0x66), 0x64),
        "pcmpgtw" => (Some(0x66), 0x65),
        "pcmpgtd" => (Some(0x66), 0x66),
        "punpcklbw" => (Some(0x66), 0x60),
        "punpcklwd" => (Some(0x66), 0x61),
        "punpckldq" => (Some(0x66), 0x62),
        "punpckhbw" => (Some(0x66), 0x68),
        "punpckhwd" => (Some(0x66), 0x69),
        "punpckhdq" => (Some(0x66), 0x6a),
        _ => return None,
    })
}

/// SSE 移位指令：`(ext, 操作码)`（`66 0F 72/73` + imm8）。
fn sse_shift_opcode(name: &str) -> Option<(u8, u8)> {
    Some(match name {
        "pslld" => (6, 0x72),
        "psrld" => (2, 0x72),
        "psrad" => (4, 0x72),
        "psllq" => (6, 0x73),
        "psrlq" => (2, 0x73),
        "pslldq" => (7, 0x73),
        "psrldq" => (3, 0x73),
        _ => return None,
    })
}

/// 发射一条内联汇编语句（x86-64 Intel 语法子集）。
///
/// 支持的操作数约束：`reg`（由编译器分配寄存器）或显式 64 位寄存器名。
/// 支持 `nop` / `ret` / `int3` / `ud2` / `hlt` / `pause` / `leave` /
/// `syscall` / `rdtsc` / `cpuid`，以及 `mov` / `lea` 之外的常用整数
/// 指令（`add` / `sub` / `and` / `or` / `xor` / `cmp` / `test` /
/// `inc` / `dec` / `neg` / `not` / `imul` / `shl` / `shr` / `sar` /
/// `push` / `pop` / `int`）。
/// 数组逐元素 SIMD：从两个地址读 128 位，执行 packed 运算，写回第三个地址。
fn emit_simd_binary(
    unit: &mut CompilationUnit,
    lhs: SsaValue,
    op: SsaBinaryOp,
    rhs: SsaValue,
    out: SsaValue,
    ty: NumTy,
    lanes: u32,
) {
    let Some((op_prefix, mem_prefix, load_op, store_op, opcode)) = simd_encoding(ty, op) else {
        crate::codegen::unsupported_simd("x86-64");
    };
    let _ = lanes;

    let saved = [Gpr::R8, Gpr::R9, Gpr::R10];
    for r in saved {
        emit_push64(unit, r);
    }
    // 先把三个地址值压到栈上再统一装载：源寄存器可能正好是 R8/R9/R10，
    // 直接逐个 mov 会互相覆盖（lhs 写到 R8 时可能踩掉还没读的 rhs）。
    let mut shift = (saved.len() * 8) as i32;
    for v in [lhs, rhs, out] {
        match location_of(v) {
            Location::Reg(i) => emit_push64(unit, ALLOCATABLE[i]),
            Location::Stack(slot) => {
                emit_load_mem_rsp(unit, SCRATCH_CX, shift + (slot as i32) * 8, NumTy::Uint(64));
                emit_push64(unit, SCRATCH_CX);
            }
        }
        shift += 8;
    }
    emit_load_mem_rsp(unit, Gpr::R8, 16, NumTy::Uint(64));
    emit_load_mem_rsp(unit, Gpr::R9, 8, NumTy::Uint(64));
    emit_load_mem_rsp(unit, Gpr::R10, 0, NumTy::Uint(64));

    emit_sse_mem_op(unit, mem_prefix, load_op, 0, Gpr::R8);
    emit_sse_mem_op(unit, mem_prefix, load_op, 1, Gpr::R9);
    emit_sse_rr(unit, op_prefix, opcode, 0, 1);
    emit_sse_mem_op(unit, mem_prefix, store_op, 0, Gpr::R10);

    for _ in 0..3 {
        emit_pop64(unit, SCRATCH_AX);
    }
    for r in saved.into_iter().rev() {
        emit_pop64(unit, r);
    }
}

/// `(运算前缀, 访存前缀, 加载 opcode, 存储 opcode, packed 运算 opcode)`。
type SimdEncoding = (Option<u8>, Option<u8>, u8, u8, u8);

/// 选择 packed 运算对应的指令编码。
fn simd_encoding(ty: NumTy, op: SsaBinaryOp) -> Option<SimdEncoding> {
    use SsaBinaryOp::*;
    match ty {
        NumTy::Float(32) => {
            let opcode = match op {
                Add => 0x58,
                Sub => 0x5c,
                Mul => 0x59,
                Div => 0x5e,
                _ => return None,
            };
            Some((None, None, 0x10, 0x11, opcode))
        }
        NumTy::Float(64) => {
            let opcode = match op {
                Add => 0x58,
                Sub => 0x5c,
                Mul => 0x59,
                Div => 0x5e,
                _ => return None,
            };
            Some((Some(0x66), Some(0x66), 0x10, 0x11, opcode))
        }
        NumTy::Int(32) | NumTy::Uint(32) => {
            let opcode = match op {
                Add => 0xfe,
                Sub => 0xfa,
                BitAnd => 0xdb,
                BitOr => 0xeb,
                BitXor => 0xef,
                _ => return None,
            };
            Some((Some(0x66), Some(0xf3), 0x6f, 0x7f, opcode))
        }
        NumTy::Int(64) | NumTy::Uint(64) => {
            let opcode = match op {
                Add => 0xd4,
                Sub => 0xfb,
                BitAnd => 0xdb,
                BitOr => 0xeb,
                BitXor => 0xef,
                _ => return None,
            };
            Some((Some(0x66), Some(0xf3), 0x6f, 0x7f, opcode))
        }
        _ => None,
    }
}

fn emit_inline_asm(
    unit: &mut CompilationUnit,
    template: &str,
    outputs: &[(String, SsaValue, NumTy)],
    inputs: &[(String, SsaValue, NumTy)],
) {
    let mut used: Vec<Gpr> = Vec::new();
    let pick = |constraint: &str, used: &mut Vec<Gpr>| -> Gpr {
        if let Some(r) = gpr_by_name(constraint) {
            if !used.contains(&r) {
                used.push(r);
            }
            return r;
        }
        for r in ASM_REG_POOL {
            if !used.contains(&r) {
                used.push(r);
                return r;
            }
        }
        crate::codegen::unsupported_inline_asm("x86-64");
    };

    let mut operand_regs: Vec<Gpr> = Vec::with_capacity(outputs.len() + inputs.len());
    for (constraint, _, _) in outputs {
        operand_regs.push(pick(constraint, &mut used));
    }
    for (constraint, _, _) in inputs {
        operand_regs.push(pick(constraint, &mut used));
    }
    let mut addr_regs: Vec<Gpr> = Vec::with_capacity(outputs.len());
    for _ in outputs {
        addr_regs.push(pick("reg", &mut used));
    }

    // 保存所有可能被编译器占用、且会被本段汇编触碰的寄存器。
    let saved: Vec<Gpr> = ASM_SAVED
        .iter()
        .copied()
        .filter(|r| used.contains(r))
        .collect();
    for r in &saved {
        emit_push64(unit, *r);
    }

    // 栈槽在 push 之后整体下移 `saved.len() * 8`。
    let frame_shift = (saved.len() * 8) as i32;
    let load_operand =
        |unit: &mut CompilationUnit, dst: Gpr, v: SsaValue, ty: NumTy| match location_of(v) {
            Location::Reg(i) => {
                let src = ALLOCATABLE[i];
                if src != dst {
                    emit_mov_rr(unit, dst, src);
                }
            }
            Location::Stack(slot) => {
                emit_load_mem_rsp(unit, dst, frame_shift + (slot as i32) * 8, ty)
            }
        };

    for (i, (_, v, _)) in outputs.iter().enumerate() {
        load_operand(unit, addr_regs[i], *v, NumTy::Uint(64));
    }
    let out_base = outputs.len();
    for (i, (_, v, ty)) in inputs.iter().enumerate() {
        load_operand(unit, operand_regs[out_base + i], *v, *ty);
    }

    let text = substitute_template(template, &operand_regs);
    for line in text.lines() {
        let line = line
            .split(';')
            .next()
            .unwrap_or("")
            .split('#')
            .next()
            .unwrap_or("");
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (mnemonic, rest) = match line.split_once(char::is_whitespace) {
            Some((m, r)) => (m.to_ascii_lowercase(), r.trim()),
            None => (line.to_ascii_lowercase(), ""),
        };
        let args: Vec<&str> = if rest.is_empty() {
            Vec::new()
        } else {
            rest.split(',').map(str::trim).collect()
        };

        let reg_arg = |s: &str| -> Option<Gpr> { gpr_by_name(s) };
        let unsupported = || -> ! {
            eprintln!("zr: error: unsupported inline assembly instruction: `{line}`");
            std::process::exit(1);
        };

        match mnemonic.as_str() {
            "nop" => emit1(unit, 0x90),
            "ret" => emit1(unit, 0xc3),
            "int3" => emit1(unit, 0xcc),
            "ud2" => emit2(unit, 0x0f, 0x0b),
            "hlt" => emit1(unit, 0xf4),
            "pause" => emit2(unit, 0xf3, 0x90),
            "leave" => emit1(unit, 0xc9),
            "syscall" => emit2(unit, 0x0f, 0x05),
            "rdtsc" => emit2(unit, 0x0f, 0x31),
            "cpuid" => emit2(unit, 0x0f, 0xa2),
            "int" => {
                if let Some(imm) = args.first().and_then(|a| parse_imm(a)) {
                    emit1(unit, 0xcd);
                    emit1(unit, imm as u8);
                } else {
                    unsupported();
                }
            }
            "mov" => {
                if args.len() != 2 {
                    unsupported();
                }
                let dst = reg_arg(args[0]);
                let src = reg_arg(args[1]);
                match (dst, src) {
                    (Some(d), Some(s)) => emit_mov_rr(unit, d, s),
                    (Some(d), None) => {
                        let imm = parse_imm(args[1]).unwrap_or_else(|| unsupported());
                        emit_mov_imm64(unit, d, imm);
                    }
                    _ => unsupported(),
                }
            }
            "add" | "sub" | "and" | "or" | "xor" | "cmp" => {
                if args.len() != 2 {
                    unsupported();
                }
                let dst = reg_arg(args[0]).unwrap_or_else(|| unsupported());
                let ext = match mnemonic.as_str() {
                    "add" => 0,
                    "or" => 1,
                    "and" => 4,
                    "sub" => 5,
                    "xor" => 6,
                    "cmp" => 7,
                    _ => unreachable!(),
                };
                match reg_arg(args[1]) {
                    Some(src) => {
                        if mnemonic == "cmp" {
                            emit_cmp_rr(unit, dst, src);
                        } else {
                            emit_full_alu(unit, ext, dst, src);
                        }
                    }
                    None => {
                        let imm = parse_imm(args[1]).unwrap_or_else(|| unsupported());
                        emit_alu_imm(unit, ext, dst, imm, mnemonic == "cmp");
                    }
                }
            }
            "test" => {
                if args.len() != 2 {
                    unsupported();
                }
                let dst = reg_arg(args[0]).unwrap_or_else(|| unsupported());
                match reg_arg(args[1]) {
                    Some(src) => emit_test_rr(unit, dst, src),
                    None => {
                        let imm = parse_imm(args[1]).unwrap_or_else(|| unsupported());
                        emit_alu_imm(unit, 0, dst, imm, false);
                    }
                }
            }
            "inc" | "dec" | "neg" | "not" => {
                let dst = args
                    .first()
                    .and_then(|a| reg_arg(a))
                    .unwrap_or_else(|| unsupported());
                match mnemonic.as_str() {
                    "inc" => emit_group3(unit, 0xff, 0, dst),
                    "dec" => emit_group3(unit, 0xff, 1, dst),
                    "not" => emit_group3(unit, 0xf7, 2, dst),
                    "neg" => emit_group3(unit, 0xf7, 3, dst),
                    _ => unreachable!(),
                }
            }
            "imul" if args.len() == 2 => {
                let dst = reg_arg(args[0]).unwrap_or_else(|| unsupported());
                let src = reg_arg(args[1]).unwrap_or_else(|| unsupported());
                emit_mul(unit, dst, dst, src);
            }
            "shl" | "shr" | "sar" => {
                if args.len() != 2 {
                    unsupported();
                }
                let dst = reg_arg(args[0]).unwrap_or_else(|| unsupported());
                let imm = parse_imm(args[1]).unwrap_or_else(|| unsupported());
                let ext = match mnemonic.as_str() {
                    "shl" => 4,
                    "shr" => 5,
                    "sar" => 7,
                    _ => unreachable!(),
                };
                let rex = 0x48u8 | if dst.rex_bit() { 0x01 } else { 0 };
                emit1(unit, rex);
                emit1(unit, 0xc1);
                modrm(unit, 3, ext, dst.bits());
                emit1(unit, imm as u8);
            }
            "push" | "pop" => {
                let r = args
                    .first()
                    .and_then(|a| reg_arg(a))
                    .unwrap_or_else(|| unsupported());
                if mnemonic == "push" {
                    emit_push64(unit, r);
                } else {
                    emit_pop64(unit, r);
                }
            }
            "movd" | "movq" if args.len() == 2 => {
                let (qdst, qsrc) = (xmm_by_name(args[0]), xmm_by_name(args[1]));
                match (qdst, qsrc, reg_arg(args[0]), reg_arg(args[1])) {
                    (Some(x), None, _, Some(g)) => {
                        emit_mov_gpr_xmm(unit, x, g, mnemonic == "movq", true)
                    }
                    (None, Some(x), Some(g), _) => {
                        emit_mov_gpr_xmm(unit, x, g, mnemonic == "movq", false)
                    }
                    (Some(dst), Some(src), _, _) => {
                        // 寄存器间复制：用 `movaps` 编码（位模式等价）。
                        emit_sse_rr(unit, None, 0x28, dst, src)
                    }
                    _ => unsupported(),
                }
            }
            _ if sse_shift_opcode(&mnemonic).is_some() => {
                let (ext, opc) = sse_shift_opcode(&mnemonic).unwrap();
                let dst = args
                    .first()
                    .and_then(|a| xmm_by_name(a))
                    .unwrap_or_else(|| unsupported());
                let imm = args
                    .get(1)
                    .and_then(|a| parse_imm(a))
                    .unwrap_or_else(|| unsupported());
                emit_sse_imm(unit, Some(0x66), opc, ext, dst, imm as u8);
            }
            _ if sse_rr_opcode(&mnemonic).is_some() => {
                let (prefix, opc) = sse_rr_opcode(&mnemonic).unwrap();
                let dst = args
                    .first()
                    .and_then(|a| xmm_by_name(a))
                    .unwrap_or_else(|| unsupported());
                let src = args
                    .get(1)
                    .and_then(|a| xmm_by_name(a))
                    .unwrap_or_else(|| unsupported());
                emit_sse_rr(unit, prefix, opc, dst, src);
            }
            _ => unsupported(),
        }
    }

    for (i, (_, _, ty)) in outputs.iter().enumerate() {
        emit_store_mem_reg(unit, addr_regs[i], operand_regs[i], *ty);
    }

    for r in saved.iter().rev() {
        emit_pop64(unit, *r);
    }
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
    fn string_address_uses_rip_relative_lea() {
        // clang: `lea rsi, [rip+sym]` = 48 8d 35 <disp32>
        let mut u = unit();
        emit_str_addr(&mut u, Gpr::Rsi, ".Lstr0");
        assert_eq!(u.code, vec![0x48, 0x8d, 0x35, 0, 0, 0, 0]);
        let reloc = &u.relocations[0];
        assert_eq!(reloc.kind, RelocationKind::RipRel32);
        assert_eq!(reloc.offset, 3, "disp32 紧随 REX+opcode+modrm");
        assert_eq!(reloc.instr_start, 0);
        assert_eq!(reloc.instr_len, 7);
        assert_eq!(reloc.addend, -4, "R_X86_64_PC32 加载符号地址用 -4");

        // 扩展寄存器：目的在 modrm 的 reg 字段，需 REX.R（0x04）而非 REX.B。
        // 用错会让 r12 被解码成 rsp。
        let mut u = unit();
        emit_str_addr(&mut u, Gpr::R12, ".Lstr1");
        assert_eq!(u.code, vec![0x4c, 0x8d, 0x25, 0, 0, 0, 0]);
    }

    #[test]
    fn mem_access_widths_and_signedness() {
        assert_eq!(mem_access(NumTy::Int(8)), (1, true));
        assert_eq!(mem_access(NumTy::Uint(8)), (1, false));
        assert_eq!(mem_access(NumTy::Int(16)), (2, true));
        assert_eq!(mem_access(NumTy::Uint(16)), (2, false));
        assert_eq!(mem_access(NumTy::Int(32)), (4, true));
        assert_eq!(mem_access(NumTy::Uint(32)), (4, false));
        assert_eq!(mem_access(NumTy::Int(64)), (8, true));
        assert_eq!(mem_access(NumTy::Bool), (1, false));
    }

    #[test]
    fn load_mem_uses_sib_disp32_form() {
        // 读 1 字节无符号：movzx eax, byte ptr [rdi]；地址用 [rdi+disp32=0]。
        let mut u = unit();
        emit_load_mem(&mut u, Gpr::Rax, Gpr::Rdi, NumTy::Uint(8));
        assert_eq!(u.code, vec![0x48, 0x0F, 0xB6, 0x84, 0x27, 0, 0, 0, 0]);

        // 读 1 字节有符号：movsx eax, byte ptr [rdi]。
        let mut u = unit();
        emit_load_mem(&mut u, Gpr::Rax, Gpr::Rdi, NumTy::Int(8));
        assert_eq!(u.code, vec![0x48, 0x0F, 0xBE, 0x84, 0x27, 0, 0, 0, 0]);

        // 读 4 字节有符号：movsxd rax, dword ptr [rdi]。
        let mut u = unit();
        emit_load_mem(&mut u, Gpr::Rax, Gpr::Rdi, NumTy::Int(32));
        assert_eq!(u.code, vec![0x48, 0x63, 0x84, 0x27, 0, 0, 0, 0]);

        // 读 8 字节：mov rax, qword ptr [rdi]。
        let mut u = unit();
        emit_load_mem(&mut u, Gpr::Rax, Gpr::Rdi, NumTy::Int(64));
        assert_eq!(u.code, vec![0x48, 0x8B, 0x84, 0x27, 0, 0, 0, 0]);
    }

    #[test]
    fn store_mem_writes_low_bytes_of_rax() {
        // 写 1 字节：mov byte ptr [rdi], al（无 REX）。
        let mut u = unit();
        emit_store_mem(&mut u, Gpr::Rdi, NumTy::Uint(8));
        assert_eq!(u.code, vec![0x88, 0x84, 0x27, 0, 0, 0, 0]);

        // 写 2 字节：66 mov word ptr [rdi], ax。
        let mut u = unit();
        emit_store_mem(&mut u, Gpr::Rdi, NumTy::Uint(16));
        assert_eq!(u.code, vec![0x66, 0x89, 0x84, 0x27, 0, 0, 0, 0]);

        // 写 8 字节：mov qword ptr [rdi], rax。
        let mut u = unit();
        emit_store_mem(&mut u, Gpr::Rdi, NumTy::Int(64));
        assert_eq!(u.code, vec![0x48, 0x89, 0x84, 0x27, 0, 0, 0, 0]);
    }

    #[test]
    fn lea_slot_addresses_stack_slot() {
        // lea rax, [rsp + 8*slot]，slot=3 → disp32=24。
        let mut u = unit();
        emit_lea_slot(&mut u, Gpr::Rax, 3);
        assert_eq!(u.code, vec![0x48, 0x8D, 0x84, 0x24, 24, 0, 0, 0]);
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
    fn division_sign_extends_before_idiv() {
        let mut u = unit();
        // 除数在 r8（ALLOCATABLE[2]），被除数由调用方预先放进 rax。
        emit_divrem_reg(&mut u, ALLOCATABLE[0], ALLOCATABLE[2], false);
        // 必须是 `cqo`（rax 的符号扩展进 rdx:rax），不能是 `xor edx, edx`——
        // 后者只在被除数非负时等价，负数除法会算错。
        assert_eq!(&u.code[0..2], &[0x48, 0x99], "cqo");
        assert_eq!(&u.code[2..5], &[0x49, 0xf7, 0xf8], "REX.WB + idiv r8");
    }

    #[test]
    fn spilled_divisor_uses_the_memory_operand() {
        let mut u = unit();
        emit_divrem_mem(&mut u, ALLOCATABLE[0], 3, false);
        assert_eq!(&u.code[0..2], &[0x48, 0x99], "cqo");
        // REX.W + idiv /7 + modrm(mod=10,reg=111,rm=SIB) + SIB(rsp) + disp32
        assert_eq!(&u.code[2..6], &[0x48, 0xf7, 0b10_111_100, 0b00_100_100]);
        assert_eq!(&u.code[6..10], &24u32.to_le_bytes(), "槽 3 -> rsp+24");
        // SIB 的 base 是 rsp（没有 REX.B）：早先的版本按目的寄存器设 REX.B，
        // 目的落在 R8–R15 时会把 base 换成 r12，装载溢出值时直接段错误。
        assert_eq!(u.code[2] & 0x01, 0, "REX.B 必须为 0（base 是 rsp）");
    }

    #[test]
    fn add_with_two_spilled_operands_targets_distinct_registers() {
        // 两个操作数都溢出：左操作数装进 dst(=rax)，右操作数装进 **rdx**。
        // 两者撞在一起会发成 `add rax, rax`（把左操作数加了两次），
        // 程序能跑，值却是错的。
        let mut u = unit();
        emit_load_slot(&mut u, SCRATCH_AX, 0);
        emit_load_slot(&mut u, SCRATCH_DX, 1);
        emit_full_alu(&mut u, 0, SCRATCH_AX, SCRATCH_DX);

        // 期望字节：两条 load 加一条 `add rdx -> rax`。
        // `mov rax, [rsp+0]` / `mov rdx, [rsp+8]` / `add rdx, rax`
        let modrm_reg_sib = |reg_bits: u8| (2 << 6) | (reg_bits << 3) | 4;
        let mut expect = Vec::new();
        for (dst, slot) in [(SCRATCH_AX, 0u32), (SCRATCH_DX, 8)] {
            expect.extend_from_slice(&[0x48, 0x8b]);
            expect.push(modrm_reg_sib(dst.bits()));
            expect.extend_from_slice(&[0b00_100_100]); // SIB base=rsp
            expect.extend_from_slice(&slot.to_le_bytes());
        }
        expect.extend_from_slice(&[0x48, 0x01]); // add r/m64, r64
        expect.push((3 << 6) | (SCRATCH_DX.bits() << 3) | SCRATCH_AX.bits());

        assert_eq!(u.code, expect, "溢出操作数必须落在不同的暂存器");
    }

    #[test]
    fn load_and_store_use_rsp_as_the_base_register() {
        // `8B /r` 的 reg 字段是**目的**，r/m 是 SIB——真正被 REX.B 扩展的是
        // SIB 的 **base**（rsp）。早先按目的寄存器设 REX.B，目的落在 R8–R15
        // 时会把 base 换成 r12，装载溢出值时读的根本不是栈帧。
        let mut u = unit();
        emit_load_slot(&mut u, Gpr::R10, 5);
        // REX.W + REX.R（R10 的 reg 字段需要扩展），REX.B 留 0
        assert_eq!(u.code[0], 0x48 | 0x04, "只应设 REX.R");
        assert_eq!(u.code[1], 0x8b, "mov r64, r/m64");
        assert_eq!(u.code[2] >> 6, 0b10, "mod=10 (disp32)");
        assert_eq!(u.code[2] & 0b111, 0b100, "rm=100 (SIB)");
        assert_eq!(u.code[2] >> 3 & 0b111, Gpr::R10.bits(), "reg=目的");
        assert_eq!(u.code[3], 0b00_100_100, "SIB base=rsp, 无索引");
        assert_eq!(&u.code[4..8], &40u32.to_le_bytes(), "槽 5 -> rsp+40");

        let mut u2 = unit();
        emit_store_slot(&mut u2, Gpr::R9, 2);
        assert_eq!(u2.code[0], 0x48 | 0x04, "只应设 REX.R");
        assert_eq!(u2.code[1], 0x89, "mov r/m64, r64");
        assert_eq!(u2.code[3], 0b00_100_100, "SIB base=rsp");
    }

    #[test]
    fn linux_start_wrapper_calls_main_then_syscall() {
        let mut u = unit();
        let target = Target::new(Arch::X86_64, Os::Linux);
        emit_start_wrapper(&target, &mut u, false);
        assert_eq!(u.code[0], 0xe8, "call main");
        assert!(u.code.ends_with(&[0x0f, 0x05]), "以 syscall 结束");
        assert_eq!(u.entry, "_start");
    }

    #[test]
    fn macos_start_wrapper_delegates_to_runtime_exit() {
        let mut u = unit();
        let target = Target::new(Arch::X86_64, Os::Darwin);
        emit_start_wrapper(&target, &mut u, false);
        assert_eq!(u.code[0], 0xe8, "call main");
        assert_eq!(&u.code[5..8], &[0x48, 0x89, 0xc7], "mov rdi, rax");
        assert_eq!(u.code[8], 0xe8, "call zr_exit");
        let targets: Vec<&str> = u.relocations.iter().map(|r| r.symbol.as_str()).collect();
        assert_eq!(targets, vec!["main", "zr_exit"]);
    }

    #[test]
    fn start_wrapper_drains_spawned_tasks() {
        let mut u = unit();
        let target = Target::new(Arch::X86_64, Os::Linux);
        emit_start_wrapper(&target, &mut u, true);
        let targets: Vec<&str> = u.relocations.iter().map(|r| r.symbol.as_str()).collect();
        assert_eq!(targets, vec!["main", "zr_run_tasks"]);
        assert!(u.code.ends_with(&[0x0f, 0x05]), "仍以 syscall 结束");
    }

    #[test]
    fn windows_start_wrapper_moves_return_into_rcx() {
        let mut u = unit();
        let target = Target::new(Arch::X86_64, Os::Win);
        emit_start_wrapper(&target, &mut u, false);
        assert_eq!(&u.code[5..8], &[0x48, 0x89, 0xc1], "mov rcx, rax");
        assert_eq!(u.code[8], 0xe8, "call zr_exit");
    }
}
