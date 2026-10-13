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
    /// 全局常量（CTFE 结果）。
    pub constants: Vec<MirConstant>,
}

/// 一个 MIR 函数。
#[derive(Debug, Clone)]
pub struct MirFunction {
    pub name: String,
    pub params: Vec<MirParam>,
    pub blocks: Vec<MirBasicBlock>,
    pub is_async: bool,
    /// 局部变量表（含类型，供栈帧布局用）。
    pub locals: Vec<MirLocal>,
}

/// 函数参数。
#[derive(Debug, Clone)]
pub struct MirParam {
    pub name: String,
    pub ty: MirType,
}

/// MIR 局部变量。
#[derive(Debug, Clone)]
pub struct MirLocal {
    pub name: String,
    pub ty: MirType,
    /// 是否可变。
    pub mutable: bool,
}

/// MIR 基本块。
#[derive(Debug, Clone)]
pub struct MirBasicBlock {
    pub id: usize,
    pub preds: Vec<usize>,
    pub succs: Vec<usize>,
    pub instrs: Vec<MirInstr>,
    pub term: MirTerminator,
    /// 本块的 drop 作用域（骨架：占位）。
    pub drop_scope: Option<usize>,
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
    /// 函数调用（结果绑定到 name）。
    Call(String, MirValue, Vec<MirValue>),
    /// 内存读取 `name = *ptr`。
    Load(MirValue),
    /// 内存写入 `*ptr = value`。
    Store(MirValue, MirValue),
    /// 显式 drop（对应 RAII）。
    Drop(MirValue),
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
        cond: MirValue,
        then_block: usize,
        else_block: usize,
    },
    /// 函数返回。
    Return(Option<MirValue>),
    /// 调用终结器（用于分离调用点/返回点，方便栈展开）。
    Call {
        target: usize,
        ret: Option<String>,
        next: usize,
    },
    /// 多路分支（对应 `match`）。
    Switch {
        discr: MirValue,
        cases: Vec<(MirValue, usize)>,
        default: usize,
    },
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

/// MIR 类型（轻量表示）。
#[derive(Debug, Clone, PartialEq)]
pub enum MirType {
    I(usize),
    U(usize),
    F(usize),
    Bool,
    /// 指针类型（指向的类型）。
    Ptr(Box<MirType>),
    /// 切片。
    Slice(Box<MirType>),
    /// 元组。
    Tuple(Vec<MirType>),
    /// 命名类型（按定义 ID 引用）。
    Named(u32, Vec<MirType>),
    /// `()`。
    Unit,
}

/// MIR 全局常量。
#[derive(Debug, Clone)]
pub struct MirConstant {
    pub name: String,
    pub ty: MirType,
    pub value: MirValue,
}

/// 从 HIR 模块降级到 MIR。
///
/// 降级过程：
/// 1. 为每个函数建立基本块（入口块 + 控制流拆分的后续块）。
/// 2. 把 HIR 表达式逐条降级为 MIR 指令，控制结构脱糖为终结器。
/// 3. 收集全局常量。
pub fn lower(hir: &HirModule) -> MirModule {
    let mut functions = Vec::with_capacity(hir.functions.len());
    let mut constants = Vec::new();

    for func in &hir.functions {
        if let Some(body) = &func.body {
            functions.push(builder::lower_function(func, body));
        }
    }

    // 收集顶层常量
    for c in &hir.consts {
        constants.push(MirConstant {
            name: c.name.clone(),
            ty: lower_type(&c.ty),
            value: MirValue::Unit, // 骨架：CTFE 结果待填充
        });
    }

    MirModule { functions, constants }
}

/// 把 HIR 类型降级为 MIR 类型。
pub fn lower_type(ty: &hir::Ty) -> MirType {
    use hir::Ty::*;
    match ty {
        Int(w) => MirType::I(*w as usize),
        Uint(w) => MirType::U(*w as usize),
        Float(w) => MirType::F(*w as usize),
        Bool => MirType::Bool,
        Unit | Never => MirType::Unit,
        Pointer(inner, _) => MirType::Ptr(Box::new(lower_type(inner))),
        Slice(inner, _) => MirType::Slice(Box::new(lower_type(inner))),
        Tuple(ts) => MirType::Tuple(ts.iter().map(lower_type).collect()),
        Named(id, args) => MirType::Named(id.0, args.iter().map(lower_type).collect()),
        _ => MirType::Unit,
    }
}
