//! 中级中间表示（MIR，Mid-level IR）。
//!
//! 设计参考：
//! - Rust：MIR（`rustc_middle::mir`）是 borrow checker 与代码生成的核心 IR，
//!   显式表达控制流、drop 作用域与借用区间。
//! - Zig：Air 作为语义分析与代码生成之间的 IR，保留调试信息。
//! - Python：无对应层。
//!
//! MIR 特性：
//! - 控制流以基本块 + 终结器显式表达。
//! - 所有控制结构脱糖为 `Goto`/`Branch`/`Return`。
//! - 临时值有显式编号，便于后续 SSA 化。
//! - 保留 drop 语义与借用区间（骨架阶段为占位）。

pub mod builder;

use crate::typeck::hir::{self, HirModule};

/// 一个 MIR 模块。
#[derive(Debug, Clone, Default)]
pub struct MirModule {
    pub functions: Vec<MirFunction>,
}

/// 一个 MIR 函数。
#[derive(Debug, Clone)]
pub struct MirFunction {
    pub name: String,
    pub params: Vec<MirParam>,
    pub blocks: Vec<MirBasicBlock>,
}

/// 函数参数。
#[derive(Debug, Clone)]
pub struct MirParam;

/// MIR 基本块。
#[derive(Debug, Clone)]
pub struct MirBasicBlock {
    pub id: usize,
    pub instrs: Vec<MirInstr>,
    pub term: MirTerminator,
}

/// MIR 指令。
#[derive(Debug, Clone)]
pub enum MirInstr {
    /// 赋值 `name = value`。
    Assign(String, MirValue),
    /// 二元运算 `name = a op b`。
    Binary(String, MirValue, MirBinaryOp, MirValue),
    /// 一元运算 `name = op x`。
    Unary(String, MirUnaryOp, MirValue),
    /// await 点：暂停当前协程，等待 future 就绪。
    /// `name = await future`。
    Await(String, MirValue),
}

/// MIR 终结器。
#[derive(Debug, Clone)]
pub enum MirTerminator {
    /// 无条件跳转。
    Goto(usize),
    /// 条件分支。
    Branch {
        then_block: usize,
        else_block: usize,
    },
    /// 函数返回。
    Return,
    /// 不可达（发散调用 / `!` 类型）。
    Unreachable,
}

/// MIR 值。
#[derive(Debug, Clone, PartialEq)]
pub enum MirValue {
    /// 整数常量。
    Int(i128),
    /// 浮点常量（按位存储）。
    Float(u64),
    /// 布尔常量。
    Bool(bool),
    /// 单元值 `()`。
    Unit,
    /// 命名局部变量 / 参数。
    Var(String),
    /// 编译期字符串。
    Str(String),
}

/// MIR 二元运算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirBinaryOp {
    Add, Sub, Mul, Div, Rem,
    BitAnd, BitOr, BitXor, Shl, Shr,
    Eq, Ne, Lt, Gt, Le, Ge,
    And, Or,
}

/// MIR 一元运算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirUnaryOp {
    Neg,
    Not,
    BitNot,
    /// 解引用（需配合内存模型）。
    Deref,
}

/// 从 HIR 模块降级到 MIR。
///
/// 降级过程：
/// 1. 为每个函数建立基本块（入口块 + 控制流拆分的后续块）。
/// 2. 把 HIR 表达式逐条降级为 MIR 指令，控制结构脱糖为终结器。
pub fn lower(hir: &HirModule) -> MirModule {
    let mut functions = Vec::with_capacity(hir.functions.len());

    for func in &hir.functions {
        if let Some(body) = &func.body {
            functions.push(builder::lower_function(func, body));
        }
    }

    MirModule { functions }
}
