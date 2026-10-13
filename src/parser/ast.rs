//! 抽象语法树（AST）定义。
//!
//! 设计参考：
//! - Rust：语句 vs 表达式区分、`match` 必须穷尽、块返回最后表达式。
//! - Zig：switch 支持范围 + `else`、标签 + `break :label`。
//! - Python：列表/字典/集合推导式、装饰器（`@` 属性语法）。
//!
//! 节点都携带 `Span`，便于后续阶段定位错误。

pub use crate::lexer::Span;

// ======================================================================
// 顶层
// ======================================================================

/// 整个编译单元（一个源文件）。
#[derive(Debug, Clone, Default)]
pub struct Program {
    /// 模块级属性（文件头 `#![...]`）。
    pub attrs: Vec<Attribute>,
    /// 模块声明（`mod foo;` / `mod foo {}`）。
    pub modules: Vec<ModuleDecl>,
    /// 导入声明（`use foo::bar;`）。
    pub imports: Vec<UseDecl>,
    /// 外部声明（`extern "C" { ... }`）。
    pub externs: Vec<ExternBlock>,
    /// 函数定义。
    pub functions: Vec<Function>,
    /// 类型定义（`struct` / `enum` / `union`）。
    pub type_defs: Vec<TypeDef>,
    /// 全局常量（`const FOO: i32 = 42;`）。
    pub consts: Vec<ConstDecl>,
    /// 全局静态变量（`static mut COUNTER: i32 = 0;`）。
    pub statics: Vec<StaticDecl>,
    /// trait 定义。
    pub traits: Vec<TraitDef>,
    /// impl 块。
    pub impls: Vec<ImplBlock>,
    /// 声明式宏定义。
    pub macros: Vec<MacroDef>,
}

// ======================================================================
// 属性
// ======================================================================

/// 属性，如 `#[inline]`、`#[link(name = "c")]`、`#![no_std]`。
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub args: Vec<AttributeArg>,
    pub inner: bool,
    pub span: Span,
}

/// 属性参数：`name = value` 或位置参数。
#[derive(Debug, Clone)]
pub enum AttributeArg {
    /// 位置参数：`#[foo(bar)]`。
    Pos(Expression),
    /// 命名参数：`#[foo(name = bar)]`。
    Named(String, Expression),
}

// ======================================================================
// 模块系统
// ======================================================================

/// 模块声明。
#[derive(Debug, Clone)]
pub enum ModuleDecl {
    /// `mod foo;` —— 从 `foo.zr` 加载。
    Load {
        name: String,
        visibility: Visibility,
        span: Span,
    },
    /// `mod foo { ... }` —— 内联模块。
    Inline {
        name: String,
        visibility: Visibility,
        body: Program,
        span: Span,
    },
}

/// 可见性修饰符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// 私有（默认）。
    Private,
    /// `pub`
    Public,
    /// `pub(crate)`
    Crate,
    /// `pub(self)`
    SelfOnly,
    /// `pub(super)`
    Super,
}

/// `use` 声明。
#[derive(Debug, Clone)]
pub struct UseDecl {
    pub tree: UseTree,
    pub span: Span,
}

/// `use` 路径树，支持 `use a::{b, c::{d}};`。
#[derive(Debug, Clone)]
pub enum UseTree {
    /// 单个名称：`use foo;`
    Name(String),
    /// 重命名：`use foo as bar;`
    Rename(Box<UseTree>, String),
    /// 通配符：`use foo::*;`
    Glob,
    /// 分组：`use foo::{a, b};`
    Group(Vec<UseTree>),
}

/// 外部块（`extern "C" { fn ...; }`）。
#[derive(Debug, Clone)]
pub struct ExternBlock {
    pub abi: Abi,
    pub items: Vec<ExternItem>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Abi {
    C,
    Cdecl,
    Stdcall,
    Fastcall,
    Zr,
    System,
    Other(String),
}

#[derive(Debug, Clone)]
pub enum ExternItem {
    Function(Function),
    /// 不透明类型声明：`extern "C" { type FILE; }`
    Type(String, Span),
}

// ======================================================================
// 类型
// ======================================================================

/// 类型表达式。
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    /// `i8`..`i128`
    I(usize),
    /// `u8`..`u128`
    U(usize),
    /// `f32` / `f64`
    F(usize),
    /// `bool`
    Bool,
    /// `char`
    Char,
    /// `str`（UTF-8 字符串切片）
    Str,
    /// `void` / `()`
    Void,
    /// `!`（never）
    Never,
    /// 命名类型：`Foo::Bar`
    Named { path: Vec<String>, generics: Vec<Type> },
    /// 指针：`*T` / `*const T` / `*mut T`
    Pointer(Box<Type>, Mutability),
    /// 引用：`&T` / `&mut T`
    Reference(Box<Type>, Mutability),
    /// 数组：`[T; N]`
    Array(Box<Type>, ArrayLen),
    /// 切片：`[]T` / `[]const T`
    Slice(Box<Type>, Mutability),
    /// 元组：`(T1, T2, ...)`
    Tuple(Vec<Type>),
    /// 函数指针：`fn(i32, i32) -> i32`
    Function {
        params: Vec<Type>,
        ret: Box<Type>,
        is_async: bool,
        abi: Abi,
    },
    /// 可选类型：`T?`（语法糖：`Option<T>`）
    Optional(Box<Type>),
    /// 错误联合：`T!E`（语法糖：`Result<T, E>`）
    ErrorUnion(Box<Type>, Box<Type>),
    /// 推断类型（留给类型推导）：`_`
    Infer,
    /// 编译期类型：`anytype`
    Anytype,
    /// `impl Trait`：存在性类型（返回位置 RPIT / 参数位置 APIT）。
    ImplTrait(Vec<Type>),
    /// `Pin<T>`：固定指针类型。
    Pin(Box<Type>),
}

/// 数组长度（可以是编译期常量表达式）。
#[derive(Debug, Clone)]
pub enum ArrayLen {
    /// 编译期已知长度。
    Const(Box<Expression>),
    /// 动态长度（仅在 `&[T]` 切片中使用，或作为未定长数组）。
    Dynamic,
}

// Expression 未实现 PartialEq，这里用 Debug 输出做近似比较。
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

/// 可变性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mutability {
    Immutable,
    Mutable,
}

// ======================================================================
// 模式
// ======================================================================

/// 模式（用于 `let` / `match` / 函数参数解构）。
#[derive(Debug, Clone)]
pub enum Pattern {
    /// 通配符 `_`。
    Wildcard(Span),
    /// 字面量模式。
    Literal(Literal),
    /// 绑定：`x` 或 `ref mut x`。
    Binding(String, Mutability, Option<Box<Pattern>>, Span),
    /// 元组模式：`(a, b)`。
    Tuple(Vec<Pattern>, Span),
    /// 结构体模式：`Point { x, y: 0 }`。
    Struct {
        path: Vec<String>,
        fields: Vec<FieldPat>,
        rest: bool, // `..`
        span: Span,
    },
    /// 枚举变体模式：`Option::Some(x)`。
    Variant {
        path: Vec<String>,
        args: Vec<Pattern>,
        span: Span,
    },
    /// 切片模式：`[first, .., last]`。
    Slice(Vec<Pattern>, Span),
    /// 范围模式：`1..=5`。
    Range(Box<Expression>, Box<Expression>, bool, Span),
    /// `ref mut p`（显式引用绑定）。
    Ref(Box<Pattern>, Mutability, Span),
    /// 带守卫的 pattern：`p if cond`。
    Guard(Box<Pattern>, Box<Expression>, Span),
}

/// 结构体模式中的字段。
#[derive(Debug, Clone)]
pub struct FieldPat {
    pub name: String,
    pub pattern: Pattern,
}

/// 字面量。
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i128, Option<String>),  // 值 + 可选后缀
    Float(f64, Option<String>),
    String(String),
    Char(char),
    Bool(bool),
}

// ======================================================================
// 表达式
// ======================================================================

/// 内联汇编表达式。
#[derive(Debug, Clone)]
pub struct AsmExpr {
    /// 汇编模板字符串。
    pub template: String,
    /// 输出操作数：`out(reg) name`。
    pub outputs: Vec<AsmOperand>,
    /// 输入操作数：`in(reg) expr`。
    pub inputs: Vec<AsmOperand>,
    /// 选项：`pure`, `nomem`, `nostack` 等。
    pub options: Vec<String>,
}

/// 汇编操作数。
#[derive(Debug, Clone)]
pub struct AsmOperand {
    /// 寄存器约束，如 `reg`, `"rax"`。
    pub constraint: String,
    /// 关联的表达式（输入）或名称（输出）。
    pub expr: Expression,
}

/// 表达式。
#[derive(Debug, Clone)]
pub enum Expression {
    /// 字面量。
    Literal(Literal, Span),
    /// 变量引用。
    Variable(String, Span),
    /// 限定名：`a::b::c`。
    Path(Vec<String>, Span),
    /// 二元运算。
    Binary(Box<Expression>, BinaryOp, Box<Expression>, Span),
    /// 一元运算。
    Unary(UnaryOp, Box<Expression>, Span),
    /// 赋值（含复合赋值 `+=` 等）。
    Assign(Box<Expression>, AssignOp, Box<Expression>, Span),
    /// 调用。
    Call(Box<Expression>, Vec<Argument>, Span),
    /// 方法调用：`obj.method(args)`。
    MethodCall(Box<Expression>, String, Vec<Argument>, Span),
    /// 索引：`a[i]`。
    Index(Box<Expression>, Box<Expression>, Span),
    /// 字段访问：`a.b`。
    Field(Box<Expression>, String, Span),
    /// 解引用：`*p`。
    Deref(Box<Expression>, Span),
    /// 取地址：`&x` / `&mut x`。
    AddrOf(Box<Expression>, Mutability, Span),
    /// 类型转换：`value as Type`。
    Cast(Box<Expression>, Type, Span),
    /// `value is Type`（类型测试）。
    Is(Box<Expression>, Type, Span),
    /// `value in range`（范围测试）。
    In(Box<Expression>, Box<Expression>, Span),
    /// 块 `{ ... }`，最后一个表达式作为块的值。
    Block(Block, Span),
    /// `if` 表达式（带可选 `else`）。
    If(Box<Expression>, Block, Option<Box<Expression>>, Span),
    /// `match` 表达式。
    Match(Box<Expression>, Vec<Arm>, Span),
    /// `while` 循环（带可选 `else`）。
    While(Box<Expression>, Block, Option<Block>, Span),
    /// `for` 循环：`for pat in iter { ... }`。
    For(Pattern, Box<Expression>, Block, Span),
    /// `loop` 无限循环。
    Loop(Block, Span),
    /// `break` / `break :label value`。
    Break(Option<String>, Option<Box<Expression>>, Span),
    /// `continue` / `continue :label`。
    Continue(Option<String>, Span),
    /// `return` / `return value`。
    Return(Option<Box<Expression>>, Span),
    /// `defer` 延迟执行。
    Defer(Box<Expression>, Span),
    /// `try` / `catch`：`try { ... } catch (e) { ... }`。
    Try(Box<Expression>, Option<Pattern>, Block, Span),
    /// `throw` 抛出异常。
    Throw(Box<Expression>, Span),
    /// 数组字面量：`[1, 2, 3]` 或 `[0; 10]`。
    Array(Vec<Expression>, Option<Box<Expression>>, Span),
    /// 元组字面量：`(1, "a")`。
    Tuple(Vec<Expression>, Span),
    /// 结构体字面量：`Point { x: 1, y: 2 }`。
    StructInit(Vec<String>, Vec<(String, Expression)>, Span),
    /// Lambda / 闭包：`|a, b| a + b`。
    Lambda(Vec<Pattern>, Option<Type>, Block, Span),
    /// `comptime` 块 / 表达式（编译期求值）。
    Comptime(Box<Expression>, Span),
    /// `await` 异步表达式。
    Await(Box<Expression>, Span),
    /// 内联汇编 `asm!(...)`。
    Asm(AsmExpr, Span),
    /// `yield` 协程产出值。
    Yield(Box<Expression>, Span),
    /// `spawn` 派生协程。
    Spawn(Box<Expression>, Span),
    /// 有标签的块：`'label: { ... }`。
    Labeled(String, Box<Expression>, Span),
    /// `goto label;`
    Goto(String, Span),
    /// `sizeof(T)` / `typeof(e)` / `alignof(T)`
    Intrinsic(IntrinsicKind, Vec<Type>, Span),
    /// 编译错误内置：`@compileError("msg")`
    CompileError(String, Span),
    /// 函数指针取址：`&func`
    FuncRef(String, Span),
    /// 字符串插值：`f"hello {name}"`
    Interpolated(Vec<InterpPart>, Span),
}

/// 插值字符串片段。
#[derive(Debug, Clone)]
pub enum InterpPart {
    Text(String),
    Expr(Expression),
}

/// 内置编译器内置函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrinsicKind {
    /// `sizeof(T)`
    SizeOf,
    /// `alignof(T)`
    AlignOf,
    /// `typeof(e)`
    TypeOf,
    /// `offsetof(T, field)`
    OffsetOf,
    /// `@compileLog(args...)`
    CompileLog,
    /// `@bitCast(T, value)`
    BitCast,
    /// `@intCast(T, value)`
    IntCast,
    /// `@errorReturnTrace()`
    ErrorReturnTrace,
    /// `@frame()`
    Frame,
    /// `@call(.always_inline, f, args)`
    Call,
    /// `intrinsic!(yield)` —— 挂起当前协程
    Yield,
    /// `intrinsic!(now_ms)` —— 当前时间戳（毫秒）
    NowMs,
    /// `intrinsic!(capture_backtrace)` —— 捕获调用栈
    CaptureBacktrace,
    /// `intrinsic!(sum s)` —— 切片求和
    Sum,
    /// `intrinsic!(drop value)` —— 丢弃值（调用析构）
    Drop,
    /// `intrinsic!(transmute src -> Dst)` —— 位转换
    Transmute,
    /// `intrinsic!(forget value)` —— 抑制析构
    Forget,
    /// `intrinsic!(zeroed T)` —— 零初始化
    Zeroed,
}

/// 实参：位置参数或命名参数。
#[derive(Debug, Clone)]
pub enum Argument {
    /// 位置参数。
    Pos(Expression),
    /// 命名参数：`name: value`。
    Named(String, Expression),
}

/// 二元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    // 算术
    Add, Sub, Mul, Div, Rem,
    // 位运算
    BitAnd, BitOr, BitXor, Shl, Shr,
    // 比较
    Eq, Ne, Lt, Gt, Le, Ge,
    // 逻辑
    And, Or,
    // 范围
    Range, RangeInclusive,
    // 字符串/切片连接
    Concat,
}

/// 一元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// `-`
    Neg,
    /// `!`（逻辑非）
    Not,
    /// `~`（位非）
    BitNot,
    /// `*` 解引用
    Deref,
    /// `&` / `&mut`
    AddrOf,
}

/// 赋值运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssignOp {
    /// `=`
    Assign,
    /// `+=`
    AddAssign,
    /// `-=`
    SubAssign,
    /// `*=`
    MulAssign,
    /// `/=`
    DivAssign,
    /// `%=`
    RemAssign,
    /// `&=`
    AndAssign,
    /// `|=`
    OrAssign,
    /// `^=`
    XorAssign,
    /// `<<=`
    ShlAssign,
    /// `>>=`
    ShrAssign,
}

// ======================================================================
// 语句（Zig/Rust 风格：多数"语句"实为表达式）
// ======================================================================

/// 语句。
#[derive(Debug, Clone)]
pub enum Statement {
    /// `let` 绑定（不可变）。
    Let(Pattern, Option<Type>, Option<Expression>, Span),
    /// `var` / `let mut` 可变绑定。
    Var(Pattern, Option<Type>, Option<Expression>, Span),
    /// `const` 编译期常量。
    Const(String, Type, Expression, Span),
    /// 表达式语句（结果被丢弃，除非是 `;` 结尾的块）。
    Expr(Expression, Span),
    /// 宏调用语句（以 `;` 结尾的 `expr!()` 等）。
    Item(ModuleItem),
}

/// 一个模块级条目（函数 / 类型 / trait / impl / const / static / use / mod）。
#[derive(Debug, Clone)]
pub enum ModuleItem {
    Function(Function),
    TypeDef(TypeDef),
    Trait(TraitDef),
    Impl(ImplBlock),
    Const(ConstDecl),
    Static(StaticDecl),
    Use(UseDecl),
    Module(ModuleDecl),
    Extern(ExternBlock),
    Attribute(Attribute),
    /// `macro_rules! name { ... }` 声明式宏定义。
    Macro(MacroDef),
}

/// 声明式宏定义。
#[derive(Debug, Clone)]
pub struct MacroDef {
    pub name: String,
    pub arms: Vec<MacroArm>,
    pub span: Span,
}

/// 宏分支：匹配模式 token 树 → 展开 token 树。
#[derive(Debug, Clone)]
pub struct MacroArm {
    pub lhs: Vec<crate::lexer::Token>,
    pub rhs: Vec<crate::lexer::Token>,
    pub span: Span,
}

/// 块 = 一系列语句，最后一个表达式可选。
#[derive(Debug, Clone, Default)]
pub struct Block {
    pub stmts: Vec<Statement>,
    pub tail: Option<Box<Expression>>,
    pub span: Span,
}

/// match 分支。
#[derive(Debug, Clone)]
pub struct Arm {
    pub pattern: Pattern,
    pub guard: Option<Box<Expression>>,
    pub body: Box<Expression>,
    pub span: Span,
}

// ======================================================================
// 函数
// ======================================================================

/// 函数定义。
#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub visibility: Visibility,
    pub attrs: Vec<Attribute>,
    pub is_async: bool,
    /// 是否不返回（如 `fn abort() -> never`）。
    pub is_noreturn: bool,
    pub is_const: bool, // 编译期可执行
    /// 是否为 `default fn`（可被特化覆盖）。
    pub is_default: bool,
    pub abi: Abi,
    pub generics: Vec<GenericParam>,
    pub where_clause: Vec<WherePred>,
    pub params: Vec<Param>,
    pub receiver: Option<Receiver>, // `self` / `&self` / `&mut self`
    pub return_type: Type,
    pub body: Option<Block>, // `None` = 声明（extern）
    pub span: Span,
}

/// 泛型参数。
#[derive(Debug, Clone)]
pub enum GenericParam {
    /// 类型参数：`<T>` / `<T: Trait>`。
    Type { name: String, bounds: Vec<Type>, default: Option<Type> },
    /// 常量参数：`<const N: usize>`。
    Const { name: String, ty: Type, default: Option<Expression> },
}

/// `where` 子句谓词。
#[derive(Debug, Clone)]
pub struct WherePred {
    pub lhs: Type,
    pub trait_type: Type,
    /// `for<'a, 'b>` 绑定的生命周期（HRTB）。
    pub for_lifetimes: Vec<String>,
    /// 是否为负边界：`T: !Trait`。
    pub negated: bool,
    pub span: Span,
}

/// 函数参数。
#[derive(Debug, Clone)]
pub struct Param {
    pub pattern: Pattern,
    pub ty: Type,
    pub span: Span,
}

/// 方法接收者。
#[derive(Debug, Clone)]
pub struct Receiver {
    pub mutability: Mutability,
    pub ty: Option<Type>, // `self: Box<Self>` 等显式形式
    pub span: Span,
}

// ======================================================================
// 类型定义
// ======================================================================

/// 类型定义。
#[derive(Debug, Clone)]
pub enum TypeDef {
    /// `struct`。
    Struct(StructDef),
    /// `enum`（代数数据类型，带 payload）。
    Enum(EnumDef),
    /// `union`（仅最后一个字段活跃，unsafe）。
    Union(UnionDef),
    /// `type` 别名。
    Alias {
        name: String,
        generics: Vec<GenericParam>,
        target: Type,
        visibility: Visibility,
        span: Span,
    },
}

/// 结构体字段。
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub visibility: Visibility,
    pub default: Option<Expression>,
    pub span: Span,
}

/// 结构体定义。
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub visibility: Visibility,
    pub attrs: Vec<Attribute>,
    pub generics: Vec<GenericParam>,
    pub fields: Vec<Field>,
    /// 是否元组结构体：`struct Point(u32, u32)`。
    pub tuple_like: bool,
    pub span: Span,
}

/// 枚举变体。
#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    /// payload：无 / 元组 / 结构体。
    pub payload: VariantPayload,
    pub discriminant: Option<Expression>, // 显式赋值
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum VariantPayload {
    Unit,
    Tuple(Vec<Type>),
    Struct(Vec<Field>),
}

/// 枚举定义。
#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    /// 底层整数类型（默认 `i32`）。
    pub repr: Option<Type>,
    pub variants: Vec<Variant>,
    pub span: Span,
}

/// 联合体定义。
#[derive(Debug, Clone)]
pub struct UnionDef {
    pub name: String,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub fields: Vec<Field>,
    pub span: Span,
}

// ======================================================================
// const / static
// ======================================================================

/// 编译期常量。
#[derive(Debug, Clone)]
pub struct ConstDecl {
    pub name: String,
    pub visibility: Visibility,
    pub ty: Type,
    pub value: Expression,
    pub span: Span,
}

/// 全局静态变量。
#[derive(Debug, Clone)]
pub struct StaticDecl {
    pub name: String,
    pub visibility: Visibility,
    pub mutable: bool,
    pub ty: Type,
    pub init: Expression,
    pub span: Span,
}

// ======================================================================
// trait / impl
// ======================================================================

/// trait 定义。
#[derive(Debug, Clone)]
pub struct TraitDef {
    pub name: String,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub super_traits: Vec<Type>,
    pub assoc_types: Vec<AssocType>,
    pub methods: Vec<Function>,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// 关联类型：`type Item;` / `type Item: Iterator;`。
#[derive(Debug, Clone)]
pub struct AssocType {
    pub name: String,
    pub bounds: Vec<Type>,
    pub default: Option<Type>,
    pub span: Span,
}

/// impl 块。
#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub generics: Vec<GenericParam>,
    pub target: Type, // 实现的目标类型
    pub trait_type: Option<Type>, // `impl Trait for T` 中的 Trait
    pub where_clause: Vec<WherePred>,
    pub items: Vec<Function>,
    pub assoc_types: Vec<AssocType>,
    pub span: Span,
}

// ======================================================================
// Span 便捷实现
// ======================================================================

/// 从表达式提取 span 的便捷 trait。
pub trait Spanned {
    fn span(&self) -> Span;
}

impl Spanned for Expression {
    fn span(&self) -> Span {
        match self {
            Expression::Literal(_, s) => *s,
            Expression::Variable(_, s) => *s,
            Expression::Path(_, s) => *s,
            Expression::Binary(_, _, _, s) => *s,
            Expression::Unary(_, _, s) => *s,
            Expression::Assign(_, _, _, s) => *s,
            Expression::Call(_, _, s) => *s,
            Expression::MethodCall(_, _, _, s) => *s,
            Expression::Index(_, _, s) => *s,
            Expression::Field(_, _, s) => *s,
            Expression::Deref(_, s) => *s,
            Expression::AddrOf(_, _, s) => *s,
            Expression::Cast(_, _, s) => *s,
            Expression::Is(_, _, s) => *s,
            Expression::In(_, _, s) => *s,
            Expression::Block(_b, s) => *s,
            Expression::If(_, _, _, s) => *s,
            Expression::Match(_, _, s) => *s,
            Expression::While(_, _, _, s) => *s,
            Expression::For(_, _, _, s) => *s,
            Expression::Loop(_, s) => *s,
            Expression::Break(_, _, s) => *s,
            Expression::Continue(_, s) => *s,
            Expression::Return(_, s) => *s,
            Expression::Defer(_, s) => *s,
            Expression::Try(_, _, _, s) => *s,
            Expression::Throw(_, s) => *s,
            Expression::Array(_, _, s) => *s,
            Expression::Tuple(_, s) => *s,
            Expression::StructInit(_, _, s) => *s,
            Expression::Lambda(_, _, _, s) => *s,
            Expression::Comptime(_, s) => *s,
            Expression::Await(_, s) => *s,
            Expression::Asm(_, s) => *s,
            Expression::Yield(_, s) => *s,
            Expression::Spawn(_, s) => *s,
            Expression::Labeled(_, _, s) => *s,
            Expression::Goto(_, s) => *s,
            Expression::Intrinsic(_, _, s) => *s,
            Expression::CompileError(_, s) => *s,
            Expression::FuncRef(_, s) => *s,
            Expression::Interpolated(_, s) => *s,
        }
    }
}

impl Spanned for Pattern {
    fn span(&self) -> Span {
        match self {
            Pattern::Wildcard(s) => *s,
            Pattern::Literal(Literal::Int(_, _)) => Span::unknown(),
            Pattern::Literal(_) => Span::unknown(),
            Pattern::Binding(_, _, _, s) => *s,
            Pattern::Tuple(_, s) => *s,
            Pattern::Struct { span, .. } => *span,
            Pattern::Variant { span, .. } => *span,
            Pattern::Slice(_, s) => *s,
            Pattern::Range(_, _, _, s) => *s,
            Pattern::Ref(_, _, s) => *s,
            Pattern::Guard(_, _, s) => *s,
        }
    }
}
