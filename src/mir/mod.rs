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
pub mod test_harness;

use crate::typeck::hir::HirModule;

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
    /// `#[inline(never)]`：优化器不许把这个函数自动内联进调用点。
    pub inline_never: bool,
    /// `#[inline(always)]`：优化器必须把这个函数内联进调用点。
    pub inline_always: bool,
}

/// 函数参数。
#[derive(Debug, Clone)]
pub struct MirParam;

/// MIR 基本块。
#[derive(Debug, Clone)]
pub struct MirBasicBlock {
    /// 块号（降级时分配，SSA 层用下标而非块号寻址）。
    #[allow(dead_code)]
    pub id: usize,
    pub instrs: Vec<MirInstr>,
    /// 终结符。`None` 表示"还没有终结"——降级收尾时会补一个默认的
    /// `Return`。必须能区分"未终结"和"显式的 `return`"，否则函数体里
    /// 写 `return x;` 会被收尾逻辑覆盖成返回空值。
    pub term: Option<MirTerminator>,
}

/// MIR 指令。
#[derive(Debug, Clone)]
pub enum MirInstr {
    /// 赋值 `name = value`。
    Assign(String, MirValue),
    /// 二元运算 `name = a op b`。
    ///
    /// 末位是**运算的结果类型**。它不是可有可无的元信息：后端必须靠它区分
    /// 整数运算与浮点运算——`a + b` 在 GPR 里是 `add`，在 XMM 里是 `addsd`，
    /// 两者编码完全不同。缺了它，浮点会被当成整数做加法（位模式直接相加）。
    Binary(String, MirValue, MirBinaryOp, MirValue, NumTy),
    /// 一元运算 `name = op x`（结果类型与操作数一致，所以不必另带）。
    Unary(String, MirUnaryOp, MirValue),
    /// await 点：暂停当前协程，等待 future 就绪。
    /// `name = await future`。
    Await(String, MirValue),
    /// 函数调用 `name = callee(args...)`。
    /// `name = callee(args...)`，末位是返回值的类型。
    ///
    /// 返回类型必须显式带上：后端要靠它判断返回值是不是浮点（走 xmm0 还是
    /// rax）。IR 的浮点常量不带位宽，没法事后从值反推。
    Call(String, String, Vec<MirValue>, NumTy),
    /// 类型转换 `name = value as to`。
    ///
    /// 之前 MIR 里根本没有这个指令：`x as i32` 在类型检查阶段就被丢掉
    /// 了操作数，MIR 侧只能降级成 `Unit`，于是运行期读到的是 0。
    Cast(String, MirValue, NumTy, NumTy),
}

/// 数值类型（只保留后端真正需要的部分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumTy {
    /// 有符号整数，位宽 8 / 16 / 32 / 64。
    Int(u8),
    /// 无符号整数，位宽同上。
    Uint(u8),
    /// 浮点，位宽 32 / 64。
    Float(u8),
    /// 布尔。
    Bool,
}

impl NumTy {
    pub fn is_float(self) -> bool {
        matches!(self, NumTy::Float(_))
    }
}

/// MIR 终结器。
#[derive(Debug, Clone)]
pub enum MirTerminator {
    /// 无条件跳转。
    Goto(usize),
    /// 条件分支（条件为真走 then，否则走 else）。
    Branch {
        cond: MirValue,
        then_block: usize,
        else_block: usize,
    },
    /// 函数返回（携带返回值；`None` 表示返回单元值）。
    Return(Option<MirValue>),
    /// 不可达（发散调用 / `!` 类型）。
    #[allow(dead_code)]
    Unreachable,
}

/// MIR 值。
#[derive(Debug, Clone, PartialEq)]
pub enum MirValue {
    /// 整数常量。
    Int(i128),
    /// 浮点常量（按位存储 + 位宽）。
    ///
    /// 位宽（32 / 64）必须随值携带：常量本身不含类型信息，后端只有拿到
    /// 位宽才知道该按 `movss/addss` 还是 `movsd/addsd` 编码，否则 f32 会读到
    /// 错误的字节。
    Float(u64, u8),
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
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
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
            functions.push(builder::lower_function(
                func,
                body,
                &hir.def_names,
                &hir.consts,
            ));
        }
    }

    let mut module = MirModule { functions };
    // `#[test]`：用合成的测试桩替换掉用户写的 `main`
    let names: Vec<String> = hir
        .functions
        .iter()
        .filter(|f| f.has_attr("test"))
        .map(|f| f.name.clone())
        .collect();
    test_harness::install(&mut module, &names);
    module
}
