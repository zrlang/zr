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
}

/// 属性参数：`name = value` 或位置参数。
#[derive(Debug, Clone)]
pub enum AttributeArg {
    /// 位置参数：`#[foo(bar)]`、`#[inline(always)]`。
    Pos(Expression),
    /// 具名参数：`#[repr(align = 8)]`。
    NameValue(String, Expression),
}

// ======================================================================
// 模块系统
// ======================================================================

/// 模块声明。
#[derive(Debug, Clone)]
pub enum ModuleDecl {
    /// `mod foo;` —— 从 `foo.zr`（或 `mod/foo.zr`）加载。
    ///
    /// 必须带上名字：早先这个变体只记"是个 mod"，名字被丢在解析器里，
    /// 加载阶段根本不知道要去读哪个文件，`mod foo;` 等于空声明。
    Load(String),
    /// `mod foo { ... }` —— 内联模块。
    ///
    /// 存的是**内部条目本身**而不是块：模块系统是扁平命名空间，内联模块的
    /// `fn`/`struct` 会被直接合并进顶层，转成语句反而会丢掉定义。
    Inline(String, Vec<ModuleItem>),
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
///
/// 块里的函数声明会被并入 `Program::functions`（`body` 为 `None`），使其
/// 能被类型检查与名称解析当作普通可调用函数；代码生成不产出函数体，调用点
/// 以未定义符号交给平台链接器绑定到运行时 / C 库。
#[derive(Debug, Clone)]
pub struct ExternBlock {
    /// 块中声明的外部函数（`body` 恒为 `None`）。
    pub functions: Vec<Function>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Abi {
    Zr,
    /// C ABI（`extern "C"`）。
    C,
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
    Named {
        path: Vec<String>,
        generics: Vec<Type>,
    },
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
    /// 编译期类型：`anytype`
    Anytype,
    /// `impl Trait`：存在性类型（返回位置 RPIT / 参数位置 APIT）。
    ImplTrait(Vec<Type>),
    /// `dyn Trait`：trait 对象（动态分发）。`Vec<Type>` 是 trait 边界列表
    /// （`dyn Error`、`dyn Error + Send`）。
    Dyn(Vec<Type>),
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
            (ArrayLen::Const(a), ArrayLen::Const(b)) => format!("{:?}", a) == format!("{:?}", b),
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
    /// 或模式 `p1 | p2`（只在 match 位置有效）。
    Or(Vec<Pattern>),
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
    Int(i128, Option<String>), // 值 + 可选后缀
    Float(f64, Option<String>),
    String(String),
    Char(char),
    Bool(bool),
    /// 字节字面量：`b'x'` / `b"str"`。
    Byte(u8),
    /// 字节串字面量：`b"str"`，按**原始字节**保存（不做 UTF-8 转义）。
    ByteString(Vec<u8>),
}

// ======================================================================
// 表达式
// ======================================================================

/// 内联汇编表达式。
#[derive(Debug, Clone)]
pub struct AsmExpr {
    /// 模板字符串，如 `"add {0}, {1}"`。
    pub template: String,
    /// 输出操作数：`out(reg) expr`。
    pub outputs: Vec<AsmOperand>,
    /// 输入操作数：`in(reg) expr`。
    pub inputs: Vec<AsmOperand>,
}

/// 汇编操作数。
#[derive(Debug, Clone)]
pub struct AsmOperand {
    /// 寄存器约束（`reg`、`rax` 等）。
    pub constraint: String,
    /// 关联的表达式（输入值或输出位置）。
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
    /// 后缀 `?`：错误传播（`Result` / `Option`）。
    Question(Box<Expression>, Span),
    /// 赋值（含复合赋值 `+=` 等）。
    Assign(Box<Expression>, AssignOp, Box<Expression>, Span),
    /// 调用。
    Call(Box<Expression>, Vec<Argument>, Span),
    /// 带 turbofish 的泛型引用：`size_of::<T>` / `Foo::<i32>::new`。
    Turbofish(Box<Expression>, Vec<Type>, Span),
    /// 宏调用：`name!(...)`。
    ///
    /// 参数先按**记号序列**存着，等整份程序解析完再由宏展开阶段
    /// 匹配 `macro_rules!` 的臂并把展开结果解析成真正的表达式。
    MacroCall {
        /// 宏名（不含 `!`）。
        name: String,
        /// 括号内的记号序列（已去掉最外层括号）。
        args: Vec<crate::lexer::TokenKind>,
        span: Span,
    },
    /// 索引：`a[i]`。
    Index(Box<Expression>, Box<Expression>, Span),
    /// 字段访问：`a.b`。
    Field(Box<Expression>, String, Span),
    /// 取地址：`&x` / `&mut x`。
    AddrOf(Box<Expression>, Mutability, Span),
    /// 类型转换：`value as Type`。
    Cast(Box<Expression>, Type, Span),
    /// 块 `{ ... }`，最后一个表达式作为块的值。
    Block(Block, Span),
    /// `if` 表达式（带可选 `else`）。
    If(Box<Expression>, Block, Option<Box<Expression>>, Span),
    /// `if let` 表达式：`if let pat = expr { ... } else { ... }`。
    IfLet(Pattern, Box<Expression>, Block, Option<Box<Expression>>, Span),
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
    /// 结构体字面量：`Point { x: 1, y: 2 }`；最后一个字段可以是
    /// 结构体更新语法 `..base`。
    StructInit(
        Vec<String>,
        Vec<(String, Expression)>,
        Option<Box<Expression>>,
        Span,
    ),
    /// Lambda / 闭包：`|a, b| a + b`。
    Lambda(Vec<LambdaParam>, Option<Type>, Block, Span),
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
    /// `unsafe { ... }`：标记需开发者自行保证安全的操作（原始指针解引用、
    /// 外部函数调用等）。zr 目前无借用检查，`unsafe` 是显式边界标记，检查
    /// 阶段按块表达式透传（块的值即 unsafe 块的值）。
    Unsafe(Box<Expression>, Span),
    /// `sizeof(T)` / `typeof(e)` / `alignof(T)`
    /// `intrinsic!(op types... args...)`
    ///
    /// 同时保存类型实参和**表达式**实参——`intrinsic!(transmute x -> u32)`
    /// 的 `x` 是表达式，`intrinsic!(size_of T)` 的 `T` 是类型。
    Intrinsic(IntrinsicKind, Vec<Type>, Vec<Expression>, Span),
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
    /// `@intCast(T, value)`
    IntCast,
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
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    // 位运算
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    // 比较
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    // 逻辑
    And,
    Or,
    // 范围
    Range,
    RangeInclusive,
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
    /// `let mut` 可变绑定。
    Var(Pattern, Option<Type>, Option<Expression>, Span),
    /// `let PAT = EXPR else { .. };` —— 模式不匹配时走 else 分支。
    ///
    /// else 分支必须发散（`return` / `break` / `panic` 等），
    /// 这样后续语句里模式的绑定才一定已经初始化。
    LetElse(Pattern, Option<Type>, Expression, Block, Span),
    /// `const` 编译期常量。
    Const(String, Type, Expression, Span),
    /// 表达式语句（结果被丢弃，除非是 `;` 结尾的块）。
    Expr(Expression, Span),
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
    /// `macro_rules! name { ... }` 声明式宏定义。
    Macro(MacroDef),
}

/// 声明式宏定义。
///
/// `macro_rules! name { (pattern) => { body }; ... }`
/// 宏体是**记号序列**（token 序列），不是 AST：展开要在语法分析之前
/// 把记号拼回去，因此这里保留原始 token。`TokenKind` 自带文本
/// （`Ident(String)` / `IntLiteral(..)` 等），所以只存 kind 就够了。
#[derive(Debug, Clone)]
pub struct MacroDef {
    /// 宏名（不含 `!`）。
    pub name: String,
    /// 各条展开臂。调用时自上而下匹配第一条命中的。
    pub arms: Vec<MacroArm>,
    pub span: Span,
}

/// 宏的一条展开臂。
#[derive(Debug, Clone)]
pub struct MacroArm {
    /// 左侧匹配模式（记号序列）。
    pub pattern: Vec<crate::lexer::TokenKind>,
    /// 右侧展开体（记号序列）。
    pub body: Vec<crate::lexer::TokenKind>,
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
}

// ======================================================================
// 函数
// ======================================================================

/// 函数定义。
#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    /// 可见性（`pub` / `pub(crate)` / `pub(super)`，默认私有）。
    ///
    /// 解析阶段保留（早先被丢弃），供类型检查的可见性检查用。
    pub visibility: Visibility,
    /// 声明所在的模块路径（`mod a { mod b { .. } }` 里的函数是 `a::b`）。
    ///
    /// 由模块加载器在扁平化时打上；顶层条目为空串（crate root）。
    pub module: String,
    /// 泛型参数（`fn swap<T>(..)` 里的 `T`）。
    pub generics: Vec<GenericParam>,
    /// 函数上的属性（`#[inline]` / `#[test]` / `#[no_mangle]` …）。
    pub attrs: Vec<Attribute>,
    pub is_async: bool,
    /// `comptime fn`：只能在编译期调用，结果被折叠成常量。
    pub is_comptime: bool,
    pub params: Vec<Param>,
    pub receiver: Option<Receiver>, // `self` / `&self` / `&mut self`
    pub return_type: Type,
    pub body: Option<Block>, // `None` = 声明（extern）
}

/// 泛型参数。
#[derive(Debug, Clone)]
pub enum GenericParam {
    /// 生命周期参数：`<'a>`。
    Lifetime(String),
    /// 类型参数：`<T>` / `<T: Trait>`。
    ///
    /// 第二个元素是 trait 边界（`<T: Ord>` 里的 `Ord`）。解析阶段**不能**丢掉：
    /// 丢掉后 `T: Ord` 就成了没写，单态化时没有任何东西去验证实参真的实现了
    /// `Ord`——`needs_clone(SomeStructWithoutClone)` 静默通过，是类型安全漏洞。
    Type(String, Vec<Type>),
    /// 常量参数：`<const N: usize>`。
    Const(String),
}

/// `where` 子句谓词。
#[derive(Debug, Clone)]
pub struct WherePred;

/// 函数参数。
#[derive(Debug, Clone)]
pub struct Param {
    pub pattern: Pattern,
    pub ty: Type,
}

/// 方法接收者。
#[derive(Debug, Clone)]
pub struct Receiver {
    pub mutability: Mutability,
    /// 显式写出的接收者类型：`self: &mut Self` 里的 `&mut Self`。
    ///
    /// 省略时为 `None`，按 `Self` 处理。
    pub ty: Option<Type>,
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
        /// 可见性（默认私有）。
        visibility: Visibility,
        /// 声明所在的模块路径（模块加载器扁平化时打上；顶层为空串）。
        module: String,
        target: Type,
        /// 泛型参数（`type MyBox<T> = Option<T>` 里的 `<T>`）。
        ///
        /// 解析阶段不能丢：别名目标里要用到 `T`，没有它连"这是不是别名"
        /// 都判断不出来（见 `typeck::collect_declarations` 的别名分支）。
        generics: Vec<GenericParam>,
    },
}

/// 结构体字段。
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    /// 字段名所在源码区间。derive 生成的 `self.field` 访问用它做诊断位置，
    /// 让"某字段缺 trait"的报错指向**那个字段**而不是整个类型定义。
    pub span: Span,
}

/// 结构体定义。
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    /// 可见性（默认私有）。
    pub visibility: Visibility,
    /// 声明所在的模块路径（模块加载器扁平化时打上；顶层为空串）。
    pub module: String,
    /// 泛型参数（`struct Pair<T> { .. }` 里的 `T`）。
    pub generics: Vec<GenericParam>,
    pub attrs: Vec<Attribute>,
    pub fields: Vec<Field>,
    /// 定义所在源码区间。derive 生成的比较/哈希方法用它在诊断里指出
    /// "哪个类型缺 trait"，避免报 `0:0:0`。
    pub span: Span,
}

/// 闭包的一个参数：模式 + 可选的类型标注。
///
/// `|x: i32| ...` 的 `x: i32`——标注是可选的，不写就靠推断。
#[derive(Debug, Clone)]
pub struct LambdaParam {
    pub pattern: Pattern,
    pub ty: Option<Type>,
}

/// 枚举变体。
#[derive(Debug, Clone)]
pub struct Variant {
    /// 变体名（`Shape::Circle` 里的 `Circle`）。
    ///
    /// 早先解析完就丢掉了，变体只能按位置编号——`Shape::Circle(1)` 这样的
    /// 变体构造根本没法按名字找到对应的变体。
    pub name: String,
    /// payload：无 / 元组 / 结构体。
    pub payload: VariantPayload,
    pub discriminant: Option<Expression>, // 显式赋值
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
    /// 可见性（默认私有）。
    pub visibility: Visibility,
    /// 声明所在的模块路径（模块加载器扁平化时打上；顶层为空串）。
    pub module: String,
    /// 属性（`#[derive(..)]` 等）。
    pub attrs: Vec<Attribute>,
    /// 泛型参数（`enum Option<T> { .. }` 里的 `T`）。
    pub generics: Vec<GenericParam>,
    /// 底层整数类型（默认 `i32`）。
    pub repr: Option<Type>,
    pub variants: Vec<Variant>,
    /// 定义所在源码区间。derive 生成的比较/哈希方法用它在诊断里指出
    /// "哪个类型缺 trait"，避免报 `0:0:0`。
    pub span: Span,
}

/// 联合体定义。
#[derive(Debug, Clone)]
pub struct UnionDef {
    pub name: String,
    pub fields: Vec<Field>,
    /// 条目可见性（`pub` / `pub(crate)` / `pub(super)` / 私有）。
    pub visibility: Visibility,
    /// 声明所在的模块路径（模块加载器扁平化时打上；顶层为空串）。
    pub module: String,
}

// ======================================================================
// const / static
// ======================================================================

/// 编译期常量。
#[derive(Debug, Clone)]
pub struct ConstDecl {
    pub name: String,
    /// 可见性（默认私有）。
    pub visibility: Visibility,
    /// 声明所在的模块路径（模块加载器扁平化时打上；顶层为空串）。
    pub module: String,
    pub ty: Type,
    pub value: Expression,
}

/// 全局静态变量。
#[derive(Debug, Clone)]
pub struct StaticDecl {
    pub name: String,
    /// 可见性（默认私有）。
    pub visibility: Visibility,
    /// 声明所在的模块路径（模块加载器扁平化时打上；顶层为空串）。
    pub module: String,
    /// 是否 `static mut`。
    pub is_mut: bool,
    pub ty: Type,
    pub init: Expression,
}

// ======================================================================
// trait / impl
// ======================================================================

/// trait 定义。
#[derive(Debug, Clone)]
pub struct TraitDef {
    pub name: String,
    /// 可见性（默认私有）。
    pub visibility: Visibility,
    /// 声明所在的模块路径（模块加载器扁平化时打上；顶层为空串）。
    pub module: String,
    pub super_traits: Vec<Type>,
    pub assoc_types: Vec<AssocType>,
    pub methods: Vec<Function>,
    pub attrs: Vec<Attribute>,
}

/// 关联类型：`type Item;` / `type Item: Iterator;`。
#[derive(Debug, Clone)]
pub struct AssocType;

/// impl 块。
#[derive(Debug, Clone)]
pub struct ImplBlock {
    /// 泛型参数（`impl<T> Option<T> { .. }` 里的 `T`）。
    pub generics: Vec<GenericParam>,
    pub target: Type,             // 实现的目标类型
    pub trait_type: Option<Type>, // `impl Trait for T` 中的 Trait
    pub methods: Vec<Function>,   // impl 中定义的方法
    pub where_clause: Vec<WherePred>,
    /// 声明所在的模块路径（模块加载器扁平化时打上；顶层为空串）。
    pub module: String,
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
            Expression::Question(_, s) => *s,
            Expression::Assign(_, _, _, s) => *s,
            Expression::Call(_, _, s) => *s,
            Expression::Turbofish(_, _, s) => *s,
            Expression::MacroCall { span, .. } => *span,
            Expression::Index(_, _, s) => *s,
            Expression::Field(_, _, s) => *s,
            Expression::AddrOf(_, _, s) => *s,
            Expression::Cast(_, _, s) => *s,
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
            Expression::StructInit(_, _, _, s) => *s,
            Expression::Lambda(_, _, _, s) => *s,
            Expression::Comptime(_, s) => *s,
            Expression::Await(_, s) => *s,
            Expression::Asm(_, s) => *s,
            Expression::Yield(_, s) => *s,
            Expression::Spawn(_, s) => *s,
            Expression::Labeled(_, _, s) => *s,
            Expression::Goto(_, s) => *s,
            Expression::Unsafe(_, s) => *s,
            Expression::Intrinsic(_, _, _, s) => *s,
            Expression::IfLet(_, _, _, _, s) => *s,
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
            // 或模式没有自己的 token 位置——取第一个分支的位置。
            Pattern::Or(alts) => match alts.first() {
                Some(first) => first.span(),
                None => Span::unknown(),
            },
        }
    }
}
