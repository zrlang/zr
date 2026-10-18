//! 高层中间表示（HIR）。
//!
//! HIR 是类型检查后的 IR，相比 AST 的主要变化：
//! - 所有表达式都带类型（`ty: Type` 字段）。
//! - 名称已解析为 DefId（通过符号表映射）。
//! - 语法糖已脱糖（如 `for` → `while` + iterator、`a += b` → `a = a + b`）。
//! - 控制流为结构化形式（无 goto、无 label）。
//! - 所有块都有明确的返回类型。
//!
//! HIR 是后续 MIR 降级、借用检查、模式穷尽性检查、闭包转换的输入。

pub use crate::parser::ast::{Abi, Span};
use std::collections::HashMap;

/// 一个 HIR 程序（已通过类型检查）。
#[derive(Debug, Clone, Default)]
pub struct HirModule {
    pub functions: Vec<HirFunction>,
    /// DefId → 定义名（MIR 降级时用来把调用还原成符号名）。
    pub def_names: HashMap<DefId, String>,
    /// 类型定义表（结构体、枚举等）。
    pub type_defs: HashMap<DefId, HirTypeDef>,
    /// 全局常量表（DefId → 定义）。
    ///
    /// MIR 降级需要它：引用一个 `const` 时要把**值**内联进指令流
    /// （常量没有存储，不存在"读一个全局"这种运行时操作）。少了这张表，
    /// `const LIMIT: i32 = 40; if LIMIT != 40 { .. }` 会拿 unit 去比较。
    pub consts: HashMap<DefId, HirConst>,
}

// ======================================================================
// 类型（HIR 层的类型，带归一化信息）
// ======================================================================

/// HIR 类型 = 语法类型 + 类型参数替换后的实际类型。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    /// 原始整数类型。
    Int(u16),
    /// 原始无符号整数类型。
    Uint(u16),
    /// 浮点类型。
    Float(u16),
    /// 布尔类型。
    Bool,
    /// 字符类型（Unicode 标量值）。
    Char,
    /// 字符串切片 `[]const u8`。
    Str,
    /// 空元组 `()`。
    Unit,
    /// Never 类型 `!`。
    Never,
    /// 命名类型（经过名称解析，指向 DefId）。
    Named(DefId, Vec<Ty>),
    /// 指针类型。
    Pointer(Box<Ty>, Mutability),
    /// 引用类型。
    Reference(Box<Ty>, Mutability),
    /// 数组 `[T; N]`。
    Array(Box<Ty>, ArrayLen),
    /// 切片 `[]T`。
    Slice(Box<Ty>, Mutability),
    /// 元组 `(T1, T2, ...)`。
    Tuple(Vec<Ty>),
    /// 函数类型（已柯里化）。
    Function {
        params: Vec<Ty>,
        ret: Box<Ty>,
        is_async: bool,
        abi: Abi,
    },
    /// 可选类型 `T?`。
    Optional(Box<Ty>),
    /// 错误联合 `T!E`。
    ErrorUnion(Box<Ty>, Box<Ty>),
    /// 推断类型（待 unify）。
    Infer(InferId),
    /// 不透明类型（前向声明）。
    Opaque(DefId),
    /// `Pin<T>`：固定指针，阻止 pointee 被移动。
    Pin(Box<Ty>),
}

impl Default for Ty {
    fn default() -> Self {
        Ty::Unit
    }
}

/// 数组长度。
#[derive(Debug, Clone)]
pub enum ArrayLen {
    /// 编译期常量。
    Const(Box<HirExpr>),
    /// 动态长度（切片 / 未定长数组）。
    Dynamic,
}

// HirExpr 未实现 PartialEq/Eq/Hash，这里用 Debug 输出做近似实现。
impl PartialEq for ArrayLen {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ArrayLen::Dynamic, ArrayLen::Dynamic) => true,
            (ArrayLen::Const(a), ArrayLen::Const(b)) => format!("{:?}", a) == format!("{:?}", b),
            _ => false,
        }
    }
}

impl Eq for ArrayLen {}

impl std::hash::Hash for ArrayLen {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        if let ArrayLen::Const(e) = self {
            format!("{:?}", e).hash(state);
        }
    }
}

/// 可变性。
/// 可变性（直接复用 AST 定义，避免类型分裂）。
pub use crate::parser::ast::Mutability;

/// 定义 ID（指向符号表中的某个定义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefId(pub u32);

/// 推断变量 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InferId(pub u32);

/// 局部变量 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalId(pub u32);

/// 基本块 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BasicBlockId;

/// Where 谓词。
#[derive(Debug, Clone)]
pub struct WherePred;

// ======================================================================
// HIR 表达式
// ======================================================================

/// HIR 表达式（带类型）。
#[derive(Debug, Clone)]
pub struct HirExpr {
    pub kind: ExprKind,
    pub ty: Ty,
    pub span: Span,
}

/// 表达式种类。
#[derive(Debug, Clone)]
pub enum ExprKind {
    /// 字面量（类型已归一化，如 `1` → `i32`）。
    Literal(HirLiteral),
    /// 局部变量引用。
    Local(LocalId),
    /// 全局定义引用（函数 / 常量 / 静态）。
    Def(DefId),
    /// 二元运算（运算符已确定、操作数已类型检查）。
    Binary(BinaryOp, Box<HirExpr>, Box<HirExpr>),
    /// 一元运算。
    Unary(UnaryOp, Box<HirExpr>),
    /// 赋值（含复合赋值，已脱糖为普通赋值）。
    Assign(Box<HirExpr>, Box<HirExpr>),
    /// 函数调用（含方法调用，已确定 receiver 与方法）。
    /// 函数调用：`callee(args...)`，`callee` 是 `ExprKind::Def`。
    Call {
        callee: DefId,
        args: Vec<HirExpr>,
        result: LocalId,
    },
    /// 字段访问（含元组索引）。
    Field(Box<HirExpr>),
    /// 解引用。
    Deref(Box<HirExpr>),
    /// 取地址（`&place`）。操作数必须保留：MIR 要靠它算出被取址的位置。
    AddrOf(Box<HirExpr>),
    /// 索引访问 `receiver[index]`。
    ///
    /// 索引表达式必须保留。早先只存了 receiver，`s[i]` 的 `i` 在类型检查
    /// 之后就被丢弃，MIR 侧无从计算地址。
    Index(Box<HirExpr>, Box<HirExpr>),
    /// 块表达式。
    Block(HirBlock),
    /// `if` 表达式。
    If(Box<HirExpr>, HirBlock, Option<HirBlock>),
    /// `match` 表达式（已验证穷尽性）。
    Match,
    /// `while` 循环。
    While(Box<HirExpr>, HirBlock),
    /// `for` 循环（脱糖后保留 for-specific 语义）。
    For,
    /// `loop` 无限循环。
    Loop(HirBlock),
    /// `break`（含标签与值）：跳到最内层循环的出口。
    Break,
    /// `continue`：跳回最内层循环的回边。
    ///
    /// 必须和 `Break` 分开。之前两者共用 `Break`，于是 `continue` 会跳出
    /// 循环而不是回到循环头。
    Continue,
    /// `return`。
    Return(Option<Box<HirExpr>>),
    /// `defer` 延迟表达式。
    Defer,
    /// `try` / `catch`。
    Try,
    /// `throw`。
    Throw,
    /// 数组字面量。
    Array,
    /// 元组字面量。
    Tuple,
    /// 结构体字面量（字段顺序已按定义重排）。
    Struct,
    /// Lambda / 闭包（已确定捕获列表与类型）。
    Closure,
    /// 类型转换 `Cast(操作数, 目标类型)`。
    ///
    /// 操作数必须留下来：丢掉它的话 MIR 侧无从生成转换指令。
    Cast(Box<HirExpr>),
    /// `comptime` 表达式，已经在编译期折成字面量。
    ///
    /// 只保留折叠结果，不保留内层表达式：求值成功后内层表达式不会再被
    /// 任何阶段读取（MIR 直接用这个字面量），留着只是白占内存。
    Comptime(Box<HirLiteral>),
    /// `await`。
    Await(Box<HirExpr>),
    /// 内联汇编。
    Asm,
    /// `yield`。
    Yield,
    /// `spawn`。
    Spawn,
    /// `sizeof` / `alignof` / `typeof` / `offsetof` 内置。
    Intrinsic,
}

/// 字面量（HIR 层已类型化）。
#[derive(Debug, Clone)]
pub enum HirLiteral {
    Int(i128),
    Float(f64),
    String(String),
    Char(char),
    Bool(bool),
}

impl PartialEq for HirLiteral {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (HirLiteral::Int(a), HirLiteral::Int(b)) => a == b,
            (HirLiteral::Float(a), HirLiteral::Float(b)) => a == b,
            (HirLiteral::String(a), HirLiteral::String(b)) => a == b,
            (HirLiteral::Char(a), HirLiteral::Char(b)) => a == b,
            (HirLiteral::Bool(a), HirLiteral::Bool(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for HirLiteral {}

/// 二元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
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
    Range,
    RangeInclusive,
}

/// 一元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Neg,
    Not,
    BitNot,
    Deref,
    AddrOf,
}

/// 字段索引（已解析为定义顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldIndex {
    /// 命名字段。
    Named(usize),
    /// 元组/结构体位置字段。
    Pos(usize),
}

/// 内置函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intrinsic {
    SizeOf,
    AlignOf,
    TypeOf,
    OffsetOf,
    IntCast,
    /// `intrinsic!(yield)` —— 挂起当前协程。
    Yield,
    /// `intrinsic!(now_ms)` —— 当前时间戳（毫秒）。
    NowMs,
    /// `intrinsic!(capture_backtrace)` —— 捕获调用栈。
    CaptureBacktrace,
    /// `intrinsic!(sum s)` —— 切片求和。
    Sum,
    /// `intrinsic!(drop value)` —— 丢弃值（调用析构）。
    Drop,
    /// `intrinsic!(forget value)` —— 抑制析构。
    Forget,
    /// `intrinsic!(zeroed T)` —— 零初始化。
    Zeroed,
    /// `intrinsic!(transmute src -> Dst)` —— 位转换。
    Transmute,
}

// ======================================================================
// HIR 模式（带类型，已解析为绑定）
// ======================================================================

/// HIR 模式（每个绑定都有明确的类型与 LocalId）。
#[derive(Debug, Clone)]
pub enum HirPattern {
    Wildcard,
    /// 字面量模式：保留匹配值（int / char / bool 统一成 i128，bool 为 0/1）。
    ///
    /// 早先这里是 unit 变体，模式值在类型检查被丢掉——即便运行期实现也
    /// 无从比较，这正是 `match` 静默算错的根源之一。
    Literal(i128),
    /// 绑定（绑定到局部变量，携带它对应的 LocalId）。
    Binding(LocalId),
    /// 元组模式。
    Tuple,
    /// 结构体模式。
    Struct,
    /// 枚举变体模式。
    Variant,
    /// 切片模式。
    Slice,
    /// 范围模式。
    Range,
    /// 引用模式 `ref mut? p`。
    Ref,
}

// ======================================================================
// HIR 语句
// ======================================================================

/// HIR 语句。
#[derive(Debug, Clone)]
pub enum HirStmt {
    /// `let` 绑定（不可变，初始化可选，未初始化 = 要求后续赋值）。
    Let {
        local: LocalId,
        init: Option<Box<HirExpr>>,
    },
    /// `var` / `mut let` 绑定（可变）。
    Var {
        local: LocalId,
        init: Option<Box<HirExpr>>,
    },
    /// 表达式语句（结果被丢弃，除非是尾表达式）。
    Expr(Box<HirExpr>),
    /// 语义项（函数 / 类型定义等，已在 HIR 顶层处理）。
    Item,
}

/// HIR 块。
#[derive(Debug, Clone, Default)]
pub struct HirBlock {
    pub stmts: Vec<HirStmt>,
    pub tail: Option<Box<HirExpr>>,
}

// ======================================================================
// HIR 函数 / 类型定义
// ======================================================================

/// HIR 层的属性。
///
/// 语法树那边的 `ast::Attribute` 装的是完整表达式，而优化器、MIR 这些
/// 下游只关心"属性叫什么、带什么字面量参数"，所以这里压成字符串参数：
/// `#[inline(always)]` -> `{ name: "inline", args: ["always"] }`，
/// `#[repr(align = 8)]` -> `{ name: "repr", named: [("align", "8")] }`。
#[derive(Debug, Clone)]
pub struct HirAttr {
    pub name: String,
    /// 位置参数。
    pub args: Vec<String>,
    /// 具名参数 `key = value`。
    pub named: Vec<(String, String)>,
}

impl HirAttr {
    /// 属性是否带某个位置参数（`#[inline(never)]`）。
    pub fn has_arg(&self, arg: &str) -> bool {
        self.args.iter().any(|a| a == arg)
    }
}

/// HIR 函数。
#[derive(Debug, Clone)]
pub struct HirFunction {
    pub name: String,
    pub params: Vec<HirParam>,
    pub return_type: Ty,
    pub body: Option<HirBlock>,
    /// 是否 `async fn`。
    pub is_async: bool,
    /// `comptime fn`：只能在编译期求值（调用点折叠成常量）。
    pub is_comptime: bool,
    /// 调用约定 / ABI（保留字段，后端按目标选择调用约定时使用）。
    #[allow(dead_code)]
    pub abi: Abi,
    /// 函数上的属性（`#[inline]` / `#[test]` / `#[no_mangle]` …）。
    pub attrs: Vec<HirAttr>,
}

impl HirFunction {
    /// 是否带某个属性。
    pub fn has_attr(&self, name: &str) -> bool {
        self.attrs.iter().any(|a| a.name == name)
    }

    /// 第一个同名属性。
    pub fn attr(&self, name: &str) -> Option<&HirAttr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

/// HIR 参数。
#[derive(Debug, Clone)]
pub struct HirParam {
    /// 参数对应的局部变量。
    pub local: LocalId,
    /// 参数类型（用于构造函数的类型）。
    pub ty: Ty,
}

/// HIR 类型定义。
#[derive(Debug, Clone)]
pub enum HirTypeDef {
    /// 结构体。
    Struct(HirStruct),
    /// 枚举（带 payload 的代数数据类型）。
    Enum(HirEnum),
    /// 联合体（仅最后一个字段活跃）。
    Union(HirUnion),
    /// 类型别名（已展开为 Target 类型）。
    Alias { def_id: DefId, target: Ty },
}

/// HIR 结构体。
#[derive(Debug, Clone)]
pub struct HirStruct {
    pub name: String,
    pub attrs: Vec<HirAttr>,
    pub fields: Vec<HirField>,
    pub repr: Repr,
}

/// HIR 枚举。
#[derive(Debug, Clone)]
pub struct HirEnum {
    pub name: String,
    pub repr: IntegerRepr,
    pub variants: Vec<HirVariant>,
}

/// HIR 联合体。
#[derive(Debug, Clone)]
pub struct HirUnion {
    pub name: String,
    pub fields: Vec<HirField>,
}

/// HIR 字段。
#[derive(Debug, Clone)]
pub struct HirField {
    pub name: String,
    pub ty: Ty,
    pub index: usize,
}

/// HIR 枚举变体。
#[derive(Debug, Clone)]
pub struct HirVariant {
    pub payload: HirVariantPayload,
}

#[derive(Debug, Clone)]
pub enum HirVariantPayload {
    Unit,
    Tuple(Vec<Ty>),
    Struct(Vec<HirField>),
}

/// 整数表示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegerRepr {
    I8,
    I16,
    I32,
    I64,
    I128,
    /// 由编译器选择最小表示（默认）。
    Auto,
}

/// 内存布局 repr。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Repr {
    /// Rust 式自动布局（带填充、字段重排优化）。
    Rust,
    /// C 兼容布局（按声明顺序、保留填充）。
    C,
    /// 紧凑布局（无填充）。
    Packed,
    /// 透明布局（与第一个字段相同，用于 FFI 新类型）。
    Transparent,
    /// SIMD 向量布局（`#[repr(simd)]`），同质向量类型。
    Simd,
}

/// HIR 常量。
#[derive(Debug, Clone)]
pub struct HirConst {
    pub ty: Ty,
    pub value: Box<HirExpr>,
}

/// HIR 静态变量。
#[derive(Debug, Clone)]
pub struct HirStatic {
    pub ty: Ty,
    pub init: Box<HirExpr>,
}

/// HIR trait 定义。
#[derive(Debug, Clone)]
pub struct HirTrait {
    pub assoc_types: Vec<HirAssocType>,
    /// 是否为 marker trait（`#[marker]`），如 `Send`、`Sync`、`Unpin`。
    /// marker trait 没有方法，且可以有多个 impl（只要不重叠）。
    pub is_marker: bool,
}

/// 关联类型。
#[derive(Debug, Clone)]
pub struct HirAssocType;

/// HIR impl 块。
#[derive(Debug, Clone)]
pub struct HirImpl {
    pub target: Ty,
    pub trait_id: Option<DefId>,
    pub where_clause: Vec<WherePred>,
    pub span: Span,
}

// ======================================================================
// 控制流图（CFG）构建结果
// ======================================================================

/// 控制流图（从 HIR 函数构建，供 MIR 使用）。
#[derive(Debug, Clone, Default)]
pub struct Cfg {
    pub blocks: Vec<BasicBlock>,
    pub start: BasicBlockId,
}

/// 基本块。
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub stmts: Vec<HirStmt>,
    pub term: Terminator,
}

/// 终结器。
#[derive(Debug, Clone)]
pub enum Terminator {
    /// 函数返回。
    Return,
    /// 不可达（`!` 类型）。
    Unreachable,
}
