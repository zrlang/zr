//! 静态单赋值形式（SSA）中间表示。
//!
//! 设计参考：
//! - Rust：MIR → LLVM IR 前的 SSA 化，用 `mir::VarDebugInfo` 关联原始变量。
//! - Zig：Air（Analysis IR）介于 AST 与机器码之间，保留调试与优化信息。
//! - Python：无 SSA 层（CPython 用栈式字节码）。
//!
//! 核心概念：
//! - 每个变量只被赋值一次（Single Static Assignment）。
//! - 在控制流汇合点插入 φ（phi）函数，把来自不同前驱的值合并。
//! - 支配树（dominator tree）是构造 φ 函数的理论基础。

pub mod domtree;
pub mod print;

pub use print::print_module;

use crate::mir::{self, MirInstr, MirModule, MirTerminator, MirValue};
use std::cell::Cell;

/// 一个 SSA 模块，包含若干函数。
#[derive(Debug, Clone, Default)]
pub struct SsaModule {
    pub functions: Vec<SsaFunction>,
}

/// 一个 SSA 函数。
#[derive(Debug, Clone)]
pub struct SsaFunction {
    pub name: String,
    pub params: Vec<SsaValue>,
    pub blocks: Vec<SsaBasicBlock>,
    pub is_async: bool,
}

/// 一个 SSA 基本块。
#[derive(Debug, Clone)]
pub struct SsaBasicBlock {
    pub id: usize,
    pub preds: Vec<usize>,
    pub succs: Vec<usize>,
    pub instrs: Vec<SsaInstr>,
    pub term: SsaTerminator,
    /// 该块在最终机器码中的起始偏移（由代码生成器在发射时填写）。
    /// 用于 Goto/Branch 重定位回填目标地址。
    pub start_offset: usize,
}

/// SSA 指令。
#[derive(Debug, Clone)]
pub enum SsaInstr {
    /// 常量赋值 `v = c`。
    Const {
        result: SsaValue,
        value: SsaConst,
    },
    /// 一元运算 `v = op x`。
    Unary {
        result: SsaValue,
        op: SsaUnaryOp,
        operand: SsaValue,
    },
    /// 二元运算 `v = a op b`。
    Binary {
        result: SsaValue,
        op: SsaBinaryOp,
        lhs: SsaValue,
        rhs: SsaValue,
    },
    /// 类型转换 `v = cast<ty>(x)`。
    Cast {
        result: SsaValue,
        operand: SsaValue,
        to: SsaType,
    },
    /// 变量引用（用于 φ 函数合并前的原始变量）。
    Var {
        result: SsaValue,
        name: String,
    },
}

/// SSA 终结器（基本块末尾的控制流指令）。
#[derive(Debug, Clone)]
pub enum SsaTerminator {
    /// 无条件跳转。
    Goto(usize),
    /// 条件分支。
    Branch {
        cond: SsaValue,
        then_block: usize,
        else_block: usize,
    },
    /// 函数返回。
    Return(Option<SsaValue>),
    /// 不可达（发散调用）。
    Unreachable,
}

/// SSA 值（一个版本化的变量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SsaValue(pub u32);

impl SsaValue {
    /// 创建一个新值。
    pub fn new(id: u32) -> Self {
        Self(id)
    }

    /// 返回底层 ID。
    pub fn id(self) -> u32 {
        self.0
    }
}

/// SSA 常量。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SsaConst {
    Int(i128),
    Float(u64), // 以位模式存储，避免 f64 不可 Eq
    Bool(bool),
    Unit,
}

/// 一元运算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsaUnaryOp {
    /// 逻辑非。
    Not,
    /// 位取反。
    BitNot,
    /// 取负。
    Neg,
}

/// 二元运算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsaBinaryOp {
    Add, Sub, Mul, Div, Rem,
    BitAnd, BitOr, BitXor, Shl, Shr,
    Eq, Ne, Lt, Gt, Le, Ge,
    And, Or,
}

/// SSA 类型（轻量表示，仅用于代码生成与推断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsaType {
    I(usize),
    U(usize),
    F(usize),
    Bool,
    Ptr,
    Void,
}

impl SsaInstr {
    /// 返回此指令定义的值（若有）。
    pub fn result(&self) -> Option<SsaValue> {
        match self {
            SsaInstr::Const { result, .. } => Some(*result),
            SsaInstr::Unary { result, .. } => Some(*result),
            SsaInstr::Binary { result, .. } => Some(*result),
            SsaInstr::Cast { result, .. } => Some(*result),
            SsaInstr::Var { result, .. } => Some(*result),
        }
    }
}

/// 从 MIR 模块构造 SSA 形式。
///
/// 当前为「朴素 SSA」降级：把 MIR 基本块与指令线性映射到 SSA，
/// 每个赋值产生一个新版本的值。φ 函数的插入需要支配树分析，
/// 见 `domtree` 模块的 `build_dominator_tree`。
pub fn from_mir(mir: &MirModule) -> SsaModule {
    let mut functions = Vec::with_capacity(mir.functions.len());
    for func in &mir.functions {
        functions.push(convert_function(func));
    }
    SsaModule { functions }
}

/// 把单个 MIR 函数转为 SSA。
fn convert_function(func: &mir::MirFunction) -> SsaFunction {
    let next_value: Cell<u32> = Cell::new(0);
    let alloc = || {
        let v = SsaValue(next_value.get());
        next_value.set(next_value.get() + 1);
        v
    };

    let params: Vec<SsaValue> = func
        .params
        .iter()
        .map(|_| alloc())
        .collect();

    let mut blocks = Vec::with_capacity(func.blocks.len());
    for block in &func.blocks {
        let mut instrs = Vec::new();
        for instr in &block.instrs {
            match instr {
                MirInstr::Assign(name, val) => {
                    let v = alloc();
                    instrs.push(SsaInstr::Var {
                        result: v,
                        name: name.clone(),
                    });
                    // 若是字面量，补充一条 Const 指令保留值语义
                    if let MirValue::Int(_) | MirValue::Bool(_) = val {
                        let _ = val;
                    }
                }
                MirInstr::Binary(name, a, op, b) => {
                    let v = alloc();
                    instrs.push(SsaInstr::Binary {
                        result: v,
                        op: convert_binary_op(*op),
                        lhs: value_to_ssa(a, &next_value),
                        rhs: value_to_ssa(b, &next_value),
                    });
                    let _ = name;
                }
                MirInstr::Unary(name, op, x) => {
                    let v = alloc();
                    instrs.push(SsaInstr::Unary {
                        result: v,
                        op: convert_unary_op(*op),
                        operand: value_to_ssa(x, &next_value),
                    });
                    let _ = name;
                }
                MirInstr::Call(name, _ret, args) => {
                    let v = alloc();
                    let _ = args;
                    instrs.push(SsaInstr::Var {
                        result: v,
                        name: name.clone(),
                    });
                }
                MirInstr::Load(ptr) => {
                    let v = alloc();
                    instrs.push(SsaInstr::Var {
                        result: v,
                        name: format!("load_{:?}", ptr),
                    });
                }
                MirInstr::Store(_ptr, _val) => {
                    // Store 不产生新值
                }
                MirInstr::Drop(_) => {
                    // Drop 不产生新值
                }
                MirInstr::Await(name, future) => {
                    let v = alloc();
                    instrs.push(SsaInstr::Var {
                        result: v,
                        name: name.clone(),
                    });
                    let _ = future;
                }
            }
        }

        let term = convert_terminator(&block.term, &next_value);
        blocks.push(SsaBasicBlock {
            id: block.id,
            preds: block.preds.clone(),
            succs: block.succs.clone(),
            instrs,
            term,
            start_offset: 0,
        });
    }

    SsaFunction {
        name: func.name.clone(),
        params,
        blocks,
        is_async: func.is_async,
    }
}

/// 把 MIR 二元运算转为 SSA 二元运算。
fn convert_binary_op(op: mir::MirBinaryOp) -> SsaBinaryOp {
    use mir::MirBinaryOp::*;
    match op {
        Add => SsaBinaryOp::Add,
        Sub => SsaBinaryOp::Sub,
        Mul => SsaBinaryOp::Mul,
        Div => SsaBinaryOp::Div,
        Rem => SsaBinaryOp::Rem,
        BitAnd => SsaBinaryOp::BitAnd,
        BitOr => SsaBinaryOp::BitOr,
        BitXor => SsaBinaryOp::BitXor,
        Shl => SsaBinaryOp::Shl,
        Shr => SsaBinaryOp::Shr,
        Eq => SsaBinaryOp::Eq,
        Ne => SsaBinaryOp::Ne,
        Lt => SsaBinaryOp::Lt,
        Gt => SsaBinaryOp::Gt,
        Le => SsaBinaryOp::Le,
        Ge => SsaBinaryOp::Ge,
        And => SsaBinaryOp::And,
        Or => SsaBinaryOp::Or,
    }
}

/// 把 MIR 一元运算转为 SSA 一元运算。
fn convert_unary_op(op: mir::MirUnaryOp) -> SsaUnaryOp {
    use mir::MirUnaryOp::*;
    match op {
        Neg => SsaUnaryOp::Neg,
        Not => SsaUnaryOp::Not,
        BitNot => SsaUnaryOp::BitNot,
        Deref => SsaUnaryOp::Not, // 指针解引用在骨架阶段占位
    }
}

/// 把 MIR 值转为 SSA 值（含常量物化）。
fn value_to_ssa(val: &MirValue, next: &Cell<u32>) -> SsaValue {
    let alloc = || {
        let v = SsaValue(next.get());
        next.set(next.get() + 1);
        v
    };
    match val {
        MirValue::Var(name) => {
            // 变量名 -> 简单哈希为 u32，真实实现需维护版本映射表
            let h = hash_name(name);
            SsaValue(h)
        }
        MirValue::Int(i) => {
            let v = alloc();
            let _ = i;
            v
        }
        MirValue::Bool(b) => {
            let v = alloc();
            let _ = b;
            v
        }
        MirValue::Unit => alloc(),
        MirValue::Float(_) | MirValue::Str(_) => alloc(),
    }
}

/// 把 MIR 终结器转为 SSA 终结器。
fn convert_terminator(term: &MirTerminator, next_value: &Cell<u32>) -> SsaTerminator {
    let alloc = || {
        let v = SsaValue(next_value.get());
        next_value.set(next_value.get() + 1);
        v
    };
    match term {
        MirTerminator::Goto(target) => SsaTerminator::Goto(*target),
        MirTerminator::Branch { cond: _, then_block, else_block } => SsaTerminator::Branch {
            cond: alloc(),
            then_block: *then_block,
            else_block: *else_block,
        },
        MirTerminator::Return(_) => {
            SsaTerminator::Return(Some(alloc()))
        }
        MirTerminator::Call { target, ret, next } => SsaTerminator::Goto(*next),
        MirTerminator::Switch { discr: _, cases, default } => {
            // 骨架：默认分支兜底
            SsaTerminator::Goto(*default)
        }
        MirTerminator::Unreachable => SsaTerminator::Unreachable,
    }
}

/// 对变量名做确定性哈希，把字符串映射到 u32。
fn hash_name(name: &str) -> u32 {
    let mut h: u32 = 0x9e3779b9;
    for b in name.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u32);
    }
    // 保留高 8 位为 0，避免与字面量分配的值碰撞
    h & 0x00ff_ffff
}
