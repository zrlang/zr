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
    /// 类型 DefId → 用户实现的 Drop 析构函数名。
    pub drop_impls: HashMap<DefId, String>,
    /// 泛型类型定义的类型参数占位符（按声明顺序），MIR 布局按实参替换。
    pub generic_params: HashMap<DefId, Vec<Ty>>,
    /// 全局 `static` 变量（按 DefId 排序）。只有标量、且初值能折成字面量
    /// 的静态量才会生成数据符号。
    pub statics: Vec<HirStaticDef>,
}

/// 一个全局静态量。
#[derive(Debug, Clone)]
pub struct HirStaticDef {
    pub id: DefId,
    pub name: String,
    pub ty: Ty,
    /// 初值折叠后的字面量；折不出来就是 `None`（读取时会报错）。
    pub const_value: Option<HirLiteral>,
}

// ======================================================================
// 类型（HIR 层的类型，带归一化信息）
// ======================================================================

/// HIR 类型 = 语法类型 + 类型参数替换后的实际类型。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
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
    #[default]
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
    /// `dyn Trait`：trait 对象（动态分发）。运行时表示是**胖指针**
    /// `{ data: *mut void, vtable: *const VTable }`——数据指针 + 指向
    /// 虚方法表的指针。目前类型检查接受该类型（`std/error` 的
    /// `&'static dyn Error` 可解析），方法调用经 vtable 分派的完整实现
    /// 尚在途，见 `docs/rust-parity.md`。
    Dyn(DefId),
}

/// 数组长度。
#[derive(Debug, Clone)]
pub enum ArrayLen {
    /// 编译期常量。
    Const(Box<HirExpr>),
    /// 动态长度（切片 / 未定长数组）。
    Dynamic,
}

/// 取数组长度的编译期整数值（只认字面量；复杂常量表达式留给后续版本）。
pub fn array_len_value(len: &ArrayLen) -> Option<i128> {
    match len {
        ArrayLen::Const(e) => match &e.kind {
            ExprKind::Literal(HirLiteral::Int(n)) => Some(*n),
            ExprKind::Comptime(lit) => match &**lit {
                HirLiteral::Int(n) => Some(*n),
                _ => None,
            },
            _ => None,
        },
        ArrayLen::Dynamic => None,
    }
}

impl PartialEq for ArrayLen {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ArrayLen::Dynamic, ArrayLen::Dynamic) => true,
            (ArrayLen::Const(a), ArrayLen::Const(b)) => {
                match (
                    array_len_value(&ArrayLen::Const(a.clone())),
                    array_len_value(&ArrayLen::Const(b.clone())),
                ) {
                    (Some(x), Some(y)) => x == y,
                    // 不可求值时才退回 Debug 近似比较。
                    _ => format!("{:?}", a) == format!("{:?}", b),
                }
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
            match array_len_value(self) {
                Some(n) => n.hash(state),
                None => format!("{:?}", e).hash(state),
            }
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
    /// 通过函数指针调用：`callee(args...)`，`callee` 是运行期地址。
    CallIndirect {
        callee: Box<HirExpr>,
        args: Vec<HirExpr>,
        result: LocalId,
    },
    /// 字段访问（含元组索引）。字段名在类型检查时解析，编译期求值靠它取值。
    Field(Box<HirExpr>, String),
    /// 解引用。
    Deref(Box<HirExpr>),
    /// 取地址（`&place`）。操作数必须保留：MIR 要靠它算出被取址的位置。
    AddrOf(Box<HirExpr>),
    /// 索引访问 `receiver[index]`。
    ///
    /// 索引表达式必须保留。早先只存了 receiver，`s[i]` 的 `i` 在类型检查
    /// 之后就被丢弃，MIR 侧无从计算地址。
    Index(Box<HirExpr>, Box<HirExpr>),
    /// 定长数组到切片的隐式转换：`[T; N]` -> `[]T`。
    ///
    /// 运行期要补出 `(ptr, len)` 这对胖指针，不是空操作。
    ArrayToSlice(Box<HirExpr>),
    /// 切片：`receiver[start..end]` / `[..end]` / `[start..]` / `[..]`。
    ///
    /// 结果在运行期是 `(ptr, len)` 一对。缺失的边界是 `None`；
    /// `inclusive` 表示源语法用了 `..=`。
    IndexRange(
        Box<HirExpr>,
        Option<Box<HirExpr>>,
        Option<Box<HirExpr>>,
        bool,
    ),
    /// 块表达式。
    Block(HirBlock),
    /// `if` 表达式。
    If(Box<HirExpr>, HirBlock, Option<HirBlock>),
    /// `match` 表达式（已验证穷尽性）。
    Match,
    /// `while` 循环。第一个字段是可选标签（`outer: while ..`）。
    While(Option<String>, Box<HirExpr>, HirBlock),
    /// `loop` 无限循环。第一个字段是可选标签（`outer: loop ..`）。
    Loop(Option<String>, HirBlock),
    /// `break`：跳到对应循环的出口。
    ///
    /// 携带可选标签：`break 'outer` 跳指定循环，无标签跳最内层。
    Break(Option<String>),
    /// `continue`：跳回对应循环的回边。
    ///
    /// 必须和 `Break` 分开。之前两者共用 `Break`，于是 `continue` 会跳出
    /// 循环而不是回到循环头。携带可选标签，语义同 `Break`。
    Continue(Option<String>),
    /// `return`。
    Return(Option<Box<HirExpr>>),
    /// 不可达点：类型是 `!`，运行期不该被执行到。
    ///
    /// 目前唯一的构造者是 `match` 的脱糖：穷尽的 `match`（所有枚举变体 /
    /// bool 两个值都覆盖）脱糖成 if/else 链之后，链尾那个"没匹配上"的
    /// 分支其实**永远走不到**。早先那里留的是 `None`（没有 else），
    /// 于是 MIR 把它当成"if 没有 else 分支 ⇒ 值是 `()`"，join 块无条件往
    /// 结果临时名里写 `Unit`——把最后一个变体臂刚算出来的值**覆盖**掉。
    /// 症状：`match e { A => 10, B => 20, C => 50 }` 对 C 返回 51 之类的垃圾
    /// （测试 57 的臂体全都用 `return`，从入口就离开了，所以一直没暴露）。
    ///
    /// 补一个显式的发散尾分支，join 块就只剩 then 一个前驱，φ 自然正确。
    Unreachable,
    /// `defer` 延迟表达式（块内每个 `defer` 都排入栈，退出时按逆序执行）。
    Defer(HirBlock),
    /// `try { ... } catch (pat) { ... }`。
    ///
    /// **预留变体**：parser 已支持 `try`/`catch` 语法，但 typeck 在没有栈展开表
    /// 之前直接拒绝（见 `check_expr` 的 `Expression::Try` 分支），所以这个
    /// HIR 变体当前无人构造。保留它以待将来实现异常机制时直接落地。
    #[allow(dead_code)]
    Try {
        expr: Box<HirExpr>,
        catch_pat: Option<HirPattern>,
        catch_block: HirBlock,
    },
    /// `throw value`。同 `Try`，预留变体。
    #[allow(dead_code)]
    Throw(Box<HirExpr>),
    /// 数组字面量（元素按源码顺序保存）。
    Array(Vec<HirExpr>),
    /// 元组字面量：各元素按源码顺序保存。
    Tuple(Vec<HirExpr>),
    /// 枚举变体构造：`Shape::Circle(5)`。
    ///
    /// `index` 是变体下标（判别式的值），`args` 是载荷的实参。
    EnumVariant {
        def_id: DefId,
        index: usize,
        args: Vec<HirExpr>,
    },
    /// 枚举的判别式（tag）：判断当前是哪个变体。
    EnumTag(Box<HirExpr>),
    /// 枚举载荷的字段访问：变体 `variant` 的第 `index` 个载荷字段。
    ///
    /// 载荷的字段偏移随变体而变（不同变体的载荷类型不同），所以访问要
    /// 带上变体下标。
    EnumField {
        value: Box<HirExpr>,
        variant: usize,
        index: usize,
    },
    /// 结构体字面量：字段按源码书写顺序保存。
    Struct(Vec<(String, HirExpr)>),
    /// Lambda / 闭包。
    ///
    /// 闭包。
    ///
    /// 两条出路：
    /// - **无捕获**：`check_lambda` 直接提升成 `__closure_N` 顶层函数
    ///   （表达式变成 `ExprKind::Def`），本变体不会出现在 HIR 里。
    /// - **带捕获**：闭包作为**值**（存进 `let`、传参、返回）时，表达式类型
    ///   是环境结构体 `__env_N`（字段 = 捕获局部，按发现顺序），`body` 保留
    ///   内联体（捕获引用仍指向定义处外层 local）。调用点（`f(a)`，`f` 持有
    ///   环境值）由 `apply_capture_body` 把内联体里捕获 local 的引用替换成
    ///   `c.__cN` 后展开。`params` / `ret` 保持原样供签名核对。
    Closure {
        params: Vec<(HirPattern, Ty)>,
        ret: Box<Ty>,
        body: Option<HirBlock>,
        /// 被闭包体引用、且定义在闭包之外的外层局部变量（去重，按发现顺序）。
        /// `Ty` 是该 local 在闭包处的类型（用于将来 by-ref / by-value 捕获决策）。
        captures: Vec<(LocalId, Ty)>,
    },
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
    Asm {
        /// 模板字符串，如 `"add {0}, {1}"`。
        template: String,
        /// 输出操作数：`out(reg) expr`，约束与已降级的表达式。
        outputs: Vec<(String, HirExpr)>,
        /// 输入操作数：`in(reg) expr`，约束与已降级的表达式。
        inputs: Vec<(String, HirExpr)>,
    },
    /// `yield`。
    Yield,
    /// `spawn`：把内层 future 交给执行器。
    Spawn(Box<HirExpr>),
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
    /// 聚合值（编译期结构体字面量）：字段按书写顺序的 `(名字, 值)`。
    ///
    /// 只在编译期求值内部存在；若它逃逸到 MIR，说明结构体没有被折叠掉。
    Composite(Vec<(String, HirLiteral)>),
}

impl PartialEq for HirLiteral {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (HirLiteral::Int(a), HirLiteral::Int(b)) => a == b,
            (HirLiteral::Float(a), HirLiteral::Float(b)) => a == b,
            (HirLiteral::String(a), HirLiteral::String(b)) => a == b,
            (HirLiteral::Char(a), HirLiteral::Char(b)) => a == b,
            (HirLiteral::Bool(a), HirLiteral::Bool(b)) => a == b,
            (HirLiteral::Composite(a), HirLiteral::Composite(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((na, va), (nb, vb))| na == nb && va == vb)
            }
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
    /// 元组模式：逐个元素的模式。
    Tuple(Vec<HirPattern>),
    /// 结构体模式：`(字段名, 字段类型, 字段模式)`。
    ///
    /// 字段模式要留下来：结构体模式的匹配条件是各字段条件之合，绑定也
    /// 落在字段上（不是整个 scrutinee）。早先把子模式丢掉，模式就只剩
    /// "是个结构体"这一条信息，`if let P { x: 1 } = p` 于是恒成立。
    Struct(Vec<(String, Ty, HirPattern)>),
    /// 枚举变体模式：变体下标 + 各个载荷的模式。
    Variant {
        index: usize,
        args: Vec<HirPattern>,
    },
    /// 切片模式。
    Slice,
    /// 范围模式：`start..=end`（`inclusive` 为真）或 `start..end`。
    Range {
        start: Box<HirExpr>,
        end: Box<HirExpr>,
        inclusive: bool,
    },
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
    /// `let mut` 绑定（可变）。
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
    /// `async fn` 的真实输出类型（`return_type` 对外是 `Future`）。
    pub async_output: Option<Ty>,
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
    pub attrs: Vec<HirAttr>,
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
    /// 变体名（`Shape::Circle` 里的 `Circle`）。
    pub name: String,
    pub payload: HirVariantPayload,
    /// 判别式的值：显式写的那个，没写就是上一个 + 1。
    ///
    /// 判别式才是运行期的"是哪个变体"标记——`enum E { A = 5, B }` 里
    /// A 的判别式是 5、B 是 6，不是 0 / 1。
    pub discriminant: i128,
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
    /// `static mut` 标记。`require_mutable_binding` 用它挡掉对只读静态量的
    /// 赋值（`LIMIT = 41` 报错）；后端目前不区分可变性、静态量统一落 `.data`。
    pub is_mut: bool,
    /// 初值折叠后的字面量（`static X: i32 = 1 + 2;` 也能折出来）。
    pub const_value: Option<HirLiteral>,
}

/// HIR trait 定义。
#[derive(Debug, Clone)]
pub struct HirTrait {
    pub assoc_types: Vec<HirAssocType>,
    /// 是否为 marker trait（`#[marker]`），如 `Send`、`Sync`、`Unpin`。
    /// marker trait 没有方法，且可以有多个 impl（只要不重叠）。
    pub is_marker: bool,
    /// 超类型 trait（`trait Ord: Eq` 里的 `Eq`）。
    ///
    /// 必须有：实现 `Ord` 蕴含实现 `Eq`，所以查 `T: Eq` 时，`T` 实现了
    /// `Ord` 也算数。没有这个字段，`Ord: Eq` 那层继承关系就断了。
    pub super_traits: Vec<DefId>,
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
