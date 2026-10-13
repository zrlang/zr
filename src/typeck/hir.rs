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

use crate::parser::ast::{Abi, Attribute, GenericParam, Pattern, Span, Visibility};
use crate::typeck::Ty as HirType;

/// 一个 HIR 程序（已通过类型检查）。
#[derive(Debug, Clone, Default)]
pub struct HirModule {
    pub functions: Vec<HirFunction>,
    pub type_defs: Vec<HirTypeDef>,
    pub consts: Vec<HirConst>,
    pub statics: Vec<HirStatic>,
    pub trait_defs: Vec<HirTrait>,
    pub impl_blocks: Vec<HirImpl>,
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
    /// 泛型参数（类型变量）。
    Param(GenericId),
    /// 推断类型（待 unify）。
    Infer(InferId),
    /// 类型别名（已展开）。
    Alias(Box<Ty>),
    /// 不透明类型（前向声明）。
    Opaque(DefId),
    /// `Pin<T>`：固定指针，阻止 pointee 被移动。
    Pin(Box<Ty>),
    /// 关联类型投影 `<T as Trait>::Assoc`。
    Projection {
        /// 限定路径：`(self_type, trait_def_id, assoc_name)`。
        self_ty: Box<Ty>,
        trait_id: DefId,
        assoc_name: String,
    },
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
            (ArrayLen::Const(a), ArrayLen::Const(b)) => {
                format!("{:?}", a) == format!("{:?}", b)
            }
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

/// 类型参数 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenericId(pub u32);

/// 推断变量 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InferId(pub u32);

/// 局部变量 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalId(pub u32);

/// 基本块 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BasicBlockId(pub u32);

/// 生命周期变量 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegionVarId(pub u32);

/// 生命周期。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Region {
    /// 静态生命周期 `'static`。
    Static,
    /// 命名生命周期 `'a`。
    Named(String),
    /// 生命周期变量（推断用）。
    Var(RegionVarId),
    /// 匿名生命周期（省略的）。
    Anonymous,
}

/// 全称量化绑定器 `for<'a> T`。
/// 表示 T 中的生命周期变量被全称量化，对所有生命周期都成立。
#[derive(Debug, Clone)]
pub struct Binder<T> {
    /// 绑定的生命周期变量列表。
    pub bound_regions: Vec<Region>,
    /// 被量化的内部值。
    pub value: T,
}

impl<T> Binder<T> {
    pub fn new(value: T) -> Self {
        Self {
            bound_regions: Vec::new(),
            value,
        }
    }
}

/// 类型方案（带量化）。
#[derive(Debug, Clone)]
pub struct TypeScheme {
    pub generics: Vec<GenericParam>,
    pub where_clause: Vec<WherePred>,
    pub ty: Ty,
}

/// Where 谓词。
#[derive(Debug, Clone)]
pub struct WherePred {
    pub lhs: Ty,
    pub trait_id: DefId,
    /// 高阶 trait 边界：`for<'a> T: Trait`。
    /// 存储绑定的生命周期变量列表（空 = 普通边界）。
    pub hrtb: Vec<Region>,
    /// 是否为负边界：`T: !Trait`。
    pub negated: bool,
    pub span: Span,
}

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
    Call(Box<HirExpr>, Vec<HirExpr>),
    /// 方法调用。
    MethodCall(Box<HirExpr>, DefId, Vec<HirExpr>),
    /// 字段访问（含元组索引）。
    Field(Box<HirExpr>, FieldIndex),
    /// 解引用。
    Deref(Box<HirExpr>),
    /// 取地址。
    AddrOf(Box<HirExpr>, Mutability),
    /// 索引访问。
    Index(Box<HirExpr>, Box<HirExpr>),
    /// 类型转换（已确定源类型与目标类型）。
    Cast(Box<HirExpr>, Ty),
    /// 块表达式。
    Block(HirBlock),
    /// `if` 表达式。
    If(Box<HirExpr>, HirBlock, Option<HirBlock>),
    /// `match` 表达式（已验证穷尽性）。
    Match(Box<HirExpr>, Vec<HirArm>),
    /// `while` 循环。
    While(Box<HirExpr>, HirBlock, Option<HirBlock>),
    /// `for` 循环（脱糖后保留 for-specific 语义）。
    For(ForLoop),
    /// `loop` 无限循环。
    Loop(HirBlock),
    /// `break` / `continue`（含标签与值）。
    Break(BreakValue),
    /// `return`。
    Return(Option<Box<HirExpr>>),
    /// `defer` 延迟表达式。
    Defer(Box<HirExpr>),
    /// `try` / `catch`。
    Try(Box<HirExpr>, Option<Box<HirExpr>>),
    /// `throw`。
    Throw(Box<HirExpr>),
    /// 数组字面量。
    Array(Vec<HirExpr>, Ty),
    /// 元组字面量。
    Tuple(Vec<HirExpr>),
    /// 结构体字面量（字段顺序已按定义重排）。
    Struct(DefId, Vec<(usize, HirExpr)>),
    /// Lambda / 闭包（已确定捕获列表与类型）。
    Closure(Box<ClosureExpr>),
    /// `comptime` 块。
    Comptime(Box<HirExpr>),
    /// `await`。
    Await(Box<HirExpr>),
    /// `yield`。
    Yield(Box<HirExpr>),
    /// `spawn`。
    Spawn(Box<HirExpr>),
    /// 编译期错误。
    CompileError(String),
    /// `sizeof` / `alignof` / `typeof` / `offsetof` 内置。
    Intrinsic(Intrinsic, Vec<Ty>),
    /// `is` 类型测试（脱糖为判别 + 转换）。
    Is(Box<HirExpr>, Ty),
    /// 字符串插值。
    Interpolated(Vec<InterpPart>),
    /// 永不求值的表达式（用于 `!` 类型传播）。
    Never(Box<HirExpr>),
    /// 错误传播 `expr?`（脱糖为 match）。
    TryOperator(Box<HirExpr>),
    /// 范围字面量。
    Range(Box<HirExpr>, Box<HirExpr>, RangeInclusivity),
}

/// 字面量（HIR 层已类型化）。
#[derive(Debug, Clone)]
pub enum HirLiteral {
    Int(i128),
    Float(f64),
    String(String),
    Char(char),
    Bool(bool),
    /// 复合字面量（数组 / 结构体 / 元组）。
    Composite(Vec<HirExpr>),
    /// 零初始化（`std::mem::zeroed()`）。
    Zero(Ty),
}

impl PartialEq for HirLiteral {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (HirLiteral::Int(a), HirLiteral::Int(b)) => a == b,
            (HirLiteral::Float(a), HirLiteral::Float(b)) => a == b,
            (HirLiteral::String(a), HirLiteral::String(b)) => a == b,
            (HirLiteral::Char(a), HirLiteral::Char(b)) => a == b,
            (HirLiteral::Bool(a), HirLiteral::Bool(b)) => a == b,
            (HirLiteral::Zero(a), HirLiteral::Zero(b)) => a == b,
            (HirLiteral::Composite(a), HirLiteral::Composite(b)) => {
                a.len() == b.len()
                    && a.iter().zip(b.iter()).all(|(x, y)| {
                        format!("{:?}", x) == format!("{:?}", y)
                    })
            }
            _ => false,
        }
    }
}

impl Eq for HirLiteral {}

/// 二元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Add, Sub, Mul, Div, Rem,
    BitAnd, BitOr, BitXor, Shl, Shr,
    Eq, Ne, Lt, Gt, Le, Ge,
    And, Or,
    Range, RangeInclusive,
    Concat,
}

/// 一元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Neg, Not, BitNot, Deref, AddrOf,
}

/// 字段索引（已解析为定义顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldIndex {
    /// 命名字段。
    Named(usize),
    /// 元组/结构体位置字段。
    Pos(usize),
}

/// 范围包含性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeInclusivity {
    Exclusive,
    Inclusive,
}

/// 字符串插值片段。
#[derive(Debug, Clone)]
pub enum InterpPart {
    Text(String),
    Expr(HirExpr),
}

/// 内置函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intrinsic {
    SizeOf,
    AlignOf,
    TypeOf,
    OffsetOf,
    BitCast,
    IntCast,
    /// 帧地址（用于 error return trace / frame 内建）。
    FrameAddress,
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
    /// `intrinsic!(call ...)` —— 强制内联调用。
    Call,
}

/// `break` / `continue` 的值。
#[derive(Debug, Clone)]
pub enum BreakValue {
    /// 无值 `break`。
    None,
    /// 带值 `break value`。
    Value(Box<HirExpr>),
    /// 带标签 `break 'label`。
    Label(String),
    /// 带标签和值。
    Labeled(String, Box<HirExpr>),
}

/// `for` 循环（脱糖信息保留）。
#[derive(Debug, Clone)]
pub struct ForLoop {
    /// 循环变量（已声明为局部）。
    pub var: LocalId,
    /// 迭代器表达式。
    pub iter: Box<HirExpr>,
    /// 循环体。
    pub body: HirBlock,
    /// 迭代器协议（by-value / by-ref）。
    pub protocol: ForProtocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForProtocol {
    /// `for x in iter`（按值消费）
    ByValue,
    /// `for x in &iter`（按引用）
    ByRef,
}

/// 闭包表达式。
#[derive(Debug, Clone)]
pub struct ClosureExpr {
    /// 参数列表（已声明为匿名结构体字段）。
    pub params: Vec<(HirPattern, Ty)>,
    /// 捕获列表（按引用 / 按值）。
    pub captures: Vec<Capture>,
    /// 函数指针类型。
    pub ty: Ty,
    /// 函数体。
    pub body: HirExpr,
}

/// 捕获方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureKind {
    /// 按引用（共享）。
    ByRef,
    /// 按可变引用。
    ByRefMut,
    /// 按值（move）。
    ByValue,
}

/// 单个捕获。
#[derive(Debug, Clone)]
pub struct Capture {
    pub local: LocalId,
    pub kind: CaptureKind,
    pub ty: Ty,
}

/// match 分支（已验证穷尽性）。
#[derive(Debug, Clone)]
pub struct HirArm {
    pub pattern: HirPattern,
    pub guard: Option<Box<HirExpr>>,
    pub body: Box<HirExpr>,
}

// ======================================================================
// HIR 模式（带类型，已解析为绑定）
// ======================================================================

/// HIR 模式（每个绑定都有明确的类型与 LocalId）。
#[derive(Debug, Clone)]
pub enum HirPattern {
    Wildcard,
    /// 字面量模式。
    Literal(HirLiteral),
    /// 绑定（绑定到局部变量）。
    Binding(LocalId, Ty),
    /// 元组模式。
    Tuple(Vec<HirPattern>, Ty),
    /// 结构体模式。
    Struct {
        def_id: DefId,
        fields: Vec<(usize, HirPattern)>,
        /// 是否含 `..`（剩余字段）。
        rest: bool,
    },
    /// 枚举变体模式。
    Variant {
        enum_id: DefId,
        variant: usize,
        args: Vec<HirPattern>,
    },
    /// 切片模式。
    Slice(Vec<HirPattern>, Option<Box<HirPattern>>, Vec<HirPattern>),
    /// 范围模式。
    Range(Box<HirExpr>, Box<HirExpr>, RangeInclusivity),
    /// 引用模式 `ref mut? p`。
    Ref(Box<HirPattern>, Mutability),
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
        ty: Ty,
        init: Option<Box<HirExpr>>,
        span: Span,
    },
    /// `var` / `mut let` 绑定（可变）。
    Var {
        local: LocalId,
        ty: Ty,
        init: Option<Box<HirExpr>>,
        span: Span,
    },
    /// `const`（编译期常量，要求 const-foldable）。
    Const {
        def: DefId,
        ty: Ty,
        value: Box<HirExpr>,
        span: Span,
    },
    /// 表达式语句（结果被丢弃，除非是尾表达式）。
    Expr(Box<HirExpr>, Span),
    /// 语义项（函数 / 类型定义等，已在 HIR 顶层处理）。
    Item,
}

/// HIR 块。
#[derive(Debug, Clone, Default)]
pub struct HirBlock {
    pub stmts: Vec<HirStmt>,
    pub tail: Option<Box<HirExpr>>,
    pub ty: Ty,
    pub span: Span,
}

// ======================================================================
// HIR 函数 / 类型定义
// ======================================================================

/// HIR 函数。
#[derive(Debug, Clone)]
pub struct HirFunction {
    pub id: DefId,
    pub name: String,
    pub visibility: Visibility,
    pub attrs: Vec<Attribute>,
    pub is_async: bool,
    pub is_noreturn: bool,
    pub is_const: bool,
    /// 是否为 `default fn`（可被特化覆盖）。
    pub is_default: bool,
    pub abi: Abi,
    pub generics: Vec<GenericParam>,
    pub params: Vec<HirParam>,
    pub receiver: Option<HirReceiver>,
    pub return_type: Ty,
    pub body: Option<HirBlock>,
    pub span: Span,
}

/// HIR 参数。
#[derive(Debug, Clone)]
pub struct HirParam {
    pub local: LocalId,
    pub pattern: HirPattern,
    pub ty: Ty,
    pub span: Span,
}

/// 方法接收者。
#[derive(Debug, Clone)]
pub struct HirReceiver {
    pub local: LocalId,
    pub mutability: Mutability,
    pub ty: Ty,
    pub span: Span,
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
    Alias {
        def_id: DefId,
        target: Ty,
        visibility: Visibility,
        span: Span,
    },
}

/// HIR 结构体。
#[derive(Debug, Clone)]
pub struct HirStruct {
    pub def_id: DefId,
    pub name: String,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub attrs: Vec<Attribute>,
    pub fields: Vec<HirField>,
    pub repr: Repr,
    pub span: Span,
}

/// HIR 枚举。
#[derive(Debug, Clone)]
pub struct HirEnum {
    pub def_id: DefId,
    pub name: String,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub repr: IntegerRepr,
    pub variants: Vec<HirVariant>,
    pub span: Span,
}

/// HIR 联合体。
#[derive(Debug, Clone)]
pub struct HirUnion {
    pub def_id: DefId,
    pub name: String,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub fields: Vec<HirField>,
    pub span: Span,
}

/// HIR 字段。
#[derive(Debug, Clone)]
pub struct HirField {
    pub name: String,
    pub ty: Ty,
    pub visibility: Visibility,
    pub index: usize,
    pub offset: usize,
    pub align: usize,
    pub span: Span,
}

/// HIR 枚举变体。
#[derive(Debug, Clone)]
pub struct HirVariant {
    pub name: String,
    pub payload: HirVariantPayload,
    pub discriminant: Option<i128>,
    pub span: Span,
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
    I8, I16, I32, I64, I128, ISize,
    U8, U16, U32, U64, U128, USize,
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
    /// 显式对齐。
    Align(usize),
    /// 透明布局（与第一个字段相同，用于 FFI 新类型）。
    Transparent,
}

/// HIR 常量。
#[derive(Debug, Clone)]
pub struct HirConst {
    pub def_id: DefId,
    pub name: String,
    pub visibility: Visibility,
    pub ty: Ty,
    pub value: Box<HirExpr>,
    pub span: Span,
}

/// HIR 静态变量。
#[derive(Debug, Clone)]
pub struct HirStatic {
    pub def_id: DefId,
    pub name: String,
    pub visibility: Visibility,
    pub mutable: bool,
    pub ty: Ty,
    pub init: Box<HirExpr>,
    pub span: Span,
}

/// HIR trait 定义。
#[derive(Debug, Clone)]
pub struct HirTrait {
    pub def_id: DefId,
    pub name: String,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub super_traits: Vec<DefId>,
    pub assoc_types: Vec<HirAssocType>,
    pub methods: Vec<HirFunction>,
    /// 是否为 marker trait（`#[marker]`），如 `Send`、`Sync`、`Unpin`。
    /// marker trait 没有方法，且可以有多个 impl（只要不重叠）。
    pub is_marker: bool,
    /// auto trait（自动为所有类型实现，除非有显式 `!Trait` impl）。
    pub is_auto: bool,
    pub span: Span,
}

/// 关联类型。
#[derive(Debug, Clone)]
pub struct HirAssocType {
    pub name: String,
    pub bounds: Vec<DefId>,
    pub default: Option<Ty>,
    pub span: Span,
}

/// HIR impl 块。
#[derive(Debug, Clone)]
pub struct HirImpl {
    pub generics: Vec<GenericParam>,
    pub target: Ty,
    pub trait_id: Option<DefId>,
    pub where_clause: Vec<WherePred>,
    pub items: Vec<HirFunction>,
    pub assoc_types: Vec<HirAssocType>,
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
    /// 循环信息（用于 borrow checker 的 "NLL region" 分析）。
    pub loops: Vec<LoopInfo>,
    /// 标签 → 块映射。
    pub labels: std::collections::HashMap<String, BasicBlockId>,
}

/// 基本块。
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub id: BasicBlockId,
    pub stmts: Vec<HirStmt>,
    pub term: Terminator,
    pub preds: Vec<BasicBlockId>,
    pub succs: Vec<BasicBlockId>,
    /// 支配者集合（用于 SSA 构造）。
    pub dominators: Vec<BasicBlockId>,
    /// 此块内定义的局部变量。
    pub defs: Vec<LocalId>,
}

/// 终结器。
#[derive(Debug, Clone)]
pub enum Terminator {
    /// 顺序执行下一块。
    Goto(BasicBlockId),
    /// 条件分支。
    Branch(Box<HirExpr>, BasicBlockId, BasicBlockId),
    /// 函数返回。
    Return(Option<Box<HirExpr>>),
    /// 不可达（`!` 类型）。
    Unreachable,
    /// 发散调用（如 `abort()`）。
    Diverge,
}

/// 循环信息。
#[derive(Debug, Clone)]
pub struct LoopInfo {
    pub header: BasicBlockId,
    pub body: Vec<BasicBlockId>,
    /// 循环变量（用于归纳变量优化）。
    pub induction_vars: Vec<LocalId>,
}
