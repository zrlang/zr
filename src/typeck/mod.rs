//! 类型检查器主模块。
//!
//! 类型检查流程：
//! ```text
//! AST → Name Resolution → HIR 构建 → 类型推断/统一 → trait 求解
//!     → 借用检查（NLL） → 模式穷尽性检查 → 闭包分析 → 编译期求值验证
//! ```
//!
//! 所有阶段共享一个 `TypeChecker`，以便跨阶段传递符号表、错误集合等状态。

pub mod derive;
pub mod hir;
pub mod infer;

use std::collections::{HashMap, HashSet};

use crate::attributes;
use crate::error::{CompileError, Result};
use crate::parser::ast::{
    self, Abi, Arm, Block as AstBlock, Expression, Literal as AstLiteral, Pattern as AstPattern,
    Program, Span, Spanned, Statement, StructDef as AstStruct, Type as AstType, UseTree,
};
use crate::typeck::hir::{
    BasicBlock, BasicBlockId, Cfg, DefId, ExprKind, FieldIndex, HirExpr, HirField, HirLiteral,
    HirStmt, InferId, LocalId, Ty, WherePred,
};
use crate::typeck::infer::InferCtxt;

/// 把数组长度写成一个 HIR 整数字面量，便于类型统一与打印。
fn const_array_len(n: i128, span: Span) -> hir::ArrayLen {
    hir::ArrayLen::Const(Box::new(HirExpr {
        kind: ExprKind::Literal(HirLiteral::Int(n)),
        ty: Ty::Uint(64),
        span,
    }))
}

/// 一个被提升成顶层函数的无捕获闭包。
///
/// `check_lambda` 在捕获集为空时登记；`finalize_lifted_closures` 在
/// `check_function_bodies` 收尾时统一解析推断变量、构造 `HirFunction` 并
/// 挂进 `self.functions`。这样 MIR / SSA / codegen 完全按普通函数处理它，
/// 不需要为"函数指针"新开后端路径。
#[derive(Debug, Clone)]
struct LiftedClosure {
    /// 提升后的 DefId（`ExprKind::Def(def_id)` 指向它）。
    def_id: DefId,
    /// 形参（模式 + 类型）。类型可能是推断变量，收尾时 resolve。
    params: Vec<(hir::HirPattern, Ty)>,
    /// 返回类型（推断变量，收尾时 resolve）。
    ret: Ty,
    /// 闭包体（局部量 id 与调用方共享编号，收尾时重编号）。
    body: hir::HirBlock,
    /// 闭包的源码 span（错误报告用）。
    span: Span,
}

/// 带捕获闭包的值模型登记项（环境结构体 DefId → 签名与内联体）。
///
/// 捕获闭包作为值传递时，表达式类型是环境结构体 `__env_N`；这张表让
/// 调用点能从环境值反查出要内联展开的闭包体。
#[derive(Debug, Clone)]
struct CapturedClosure {
    /// 形参（模式 + 类型）。
    params: Vec<(hir::HirPattern, Ty)>,
    /// 返回类型。
    ret: Ty,
    /// 内联体（捕获引用仍指向定义处外层 local，调用点替换成环境字段）。
    body: hir::HirBlock,
    /// 捕获顺序（与环境结构体字段 `__cN` 一一对应）。
    captures: Vec<(LocalId, Ty)>,
    /// 闭包源码 span（报错用）。
    span: Span,
}

/// `loop` / `while` / `for` 的 break 上下文（`TypeChecker::loop_ctxs` 栈项）。
#[derive(Debug)]
struct LoopBreakCtx {
    /// 循环标签（`outer: loop`）。`break 'outer` 按它匹配；无标签取最内层。
    label: Option<String>,
    /// break 值类型。`loop` 用新鲜推断变量（`break v` 与它统一）；`while` /
    /// `for` 是 `Unit`（break 作语句）。
    ty: Ty,
    /// 该循环是否是 `loop`（可带值 break）。`while` / `for` 为 `false`。
    is_value: bool,
    /// 见过带值 `break` 吗？`loop` 检查完据此定类型。
    value_seen: bool,
    /// 见过不带值 `break` 吗？
    plain_seen: bool,
}

/// 在 break 上下文栈里找 `break` 的目标：`None`（无标签）取最内层，
/// `Some(name)` 从内向外找第一个标签匹配的项。
fn find_loop_ctx(ctxs: &[LoopBreakCtx], label: Option<&str>) -> Option<usize> {
    match label {
        None => ctxs.len().checked_sub(1),
        Some(name) => ctxs
            .iter()
            .rposition(|c| c.label.as_deref() == Some(name)),
    }
}

/// 类型检查器。
#[derive(Debug)]
pub struct TypeChecker {
    /// 目标允许的最大整数位宽（默认 128，即不额外限制）。
    max_int_bits: u16,
    /// 目标指针位宽（默认 64）；`usize` / `isize` 按它展开。
    pointer_bits: u16,
    /// 符号表：名称 → 定义 ID。
    symbols: HashMap<String, DefId>,
    /// 反向：DefId → 名称（用于错误报告）。
    def_names: HashMap<DefId, String>,
    /// 类型定义表。
    type_defs: HashMap<DefId, hir::HirTypeDef>,
    /// 函数表。
    functions: HashMap<DefId, hir::HirFunction>,
    /// 异步函数的 Future 不透明类型 → `Output` 类型。
    async_outputs: HashMap<DefId, Ty>,
    /// trait 表。
    trait_defs: HashMap<DefId, hir::HirTrait>,
    /// trait 名 → DefId，供 `#[derive(Clone)]` 按**名字**声明的实现做求解。
    ///
    /// derive 生成的 impl 块 `trait_id` 是 `None`（属性 args 里只有字符串），
    /// 所以求解时得靠这张表把名字解析回 DefId。没有它，`#[derive(Clone)]`
    /// 只能被 `check_trait_impl` 里那段 attr 扫描"认名字"，一旦求解搬到
    /// 有真实 impl 表的地方就丢失了。
    trait_defs_by_name: HashMap<String, DefId>,
    /// impl 块列表。
    impl_blocks: Vec<hir::HirImpl>,
    /// impl 方法（名字已被 mangle 成 `Type__method`）→ 目标类型。
    impl_targets: HashMap<String, AstType>,
    /// 目标类型 DefId → Drop 析构函数 DefId。
    drop_impls: HashMap<DefId, DefId>,
    /// 常量表。
    consts: HashMap<DefId, hir::HirConst>,
    /// 静态变量表。
    statics: HashMap<DefId, hir::HirStatic>,
    /// 局部变量表（当前作用域链）。
    locals: HashMap<LocalId, Ty>,
    /// 可被赋值的局部变量（`let mut` / `mut` 参数 / `mut` 模式绑定）。
    mutable_locals: HashSet<LocalId>,
    /// 已被移动的局部变量（非 `Copy` / 含 drop glue 的类型，经 `let b = a`
    /// 或按值传参后源不再有效）。在**当前块**里收集：`let b = a`（`a` 为
    /// 移动类型的局部）后 `a` 进入这里，后续对 `a` 的**读**报 use-after-move。
    ///
    /// 这是移动检查的最小实现：只覆盖"整个局部被移动"（不做字段级 / 分支
    /// 敏感）。借用检查（`&mut` 独占、别名）仍是 `infer.rs::check_borrows`
    /// 的空壳，不在本集合里。
    moved_locals: HashSet<LocalId>,
    /// 当前块里"正在被移动"的局部（`let b = a` 的 `a` 在被检查的瞬间算
    /// **移动读**而不是**普通读**，避免 `let b = a` 自己触发 use-after-move）。
    ///
    /// 用 `HashSet` 而非单个 `Option`：一个表达式里可能同时移动多个局部
    /// （元组解构 `let (b, c) = (a, d)`）。
    moving_now: HashSet<LocalId>,
    /// 泛型参数的类型（按作用域栈）。
    ///
    /// 泛型参数在声明里是一个类型变量（推断变量），用到的地方按它解析。
    type_params: Vec<HashMap<String, Ty>>,
    /// impl 块里的方法 → 该 impl 的泛型参数。
    ///
    /// 方法被展开成独立的函数后，impl 的类型参数就没人绑了——`impl<T>
    /// Option<T>` 里的 `T` 会变成"找不到的名字"。
    impl_generics: HashMap<String, Vec<ast::GenericParam>>,
    /// 泛型函数（第一个的位置）。
    ///
    /// 泛型还没有单态化：泛型函数的体按推断变量检查会报一堆假错（`T` 的
    /// 运算无从判断），而真按具体类型生成代码又需要实例化。这里把泛型函数
    /// 的体跳过，等用到它时再明确报"未实现"。
    generic_fn_span: Option<ast::Span>,
    /// 程序里出现过闭包（第一个的位置）。
    ///
    /// `closure_consumed`：被 `inline_closure_call` 内联展开的**立即调用**
    /// 个数。带捕获闭包被当值取走（存变量 / 传参 / 返回）时靠环境结构体
    /// `__env_N` 走 MIR，不再计数。
    closure_consumed: u32,
    /// 闭包捕获上下文栈。每层一项：进入闭包时该闭包**之外**所有可见的
    /// `LocalId`（即外层作用域里的局部变量，不含闭包自己的形参）。闭包体里
    /// 引用到这些 local 即记为捕获。嵌套闭包时内层把外层 local 也当捕获。
    closure_ctx: Vec<std::collections::HashSet<LocalId>>,
    /// 当前最内层闭包正在收集的捕获（`LocalId` → 类型）。离开闭包时取出，
    /// 挂到对应 `ExprKind::Closure` 上。`Vec` 层与 `closure_ctx` 一一对应。
    closure_captures: Vec<Vec<(LocalId, Ty)>>,
    /// 无捕获闭包被提升成的顶层函数（DefId → 闭包体与签名）。
    ///
    /// 无捕获闭包本来就是一个普通函数，没有理由拒绝存进变量 / 当实参 /
    /// 返回（见 `tests/cases/83_closure_fnptr.zr` 的头注释）。`check_lambda`

    /// 带捕获闭包的值模型：环境结构体 DefId → 闭包签名与内联体。
    ///
    /// 捕获闭包存进变量 / 传参 / 返回时，表达式类型是环境结构体
    /// `__env_N`（字段 = 捕获局部，值拷贝）。调用点 `f(a)`（`f` 持有环境值）
    /// 按 `Ty::Named(env_id)` 查这张表拿回闭包体与签名，`apply_capture_body`
    /// 把内联体里捕获 local 的引用替换成 `f.__cN` 后在调用点展开。环境值按
    /// 普通结构体传递，因此**同函数内**有效（调用方作用域里的捕获 local
    /// 仍可达）；跨函数传值时捕获 local 生命周期不保证，暂不校验。
    closures_by_env: HashMap<DefId, CapturedClosure>,
    /// 在捕获集为空、且闭包不是在"立即调用"位置时，登记到这里并返回
    /// `ExprKind::Def(def_id)`（函数指针）。函数体在 `finalize_lifted_closures`
    /// 里统一解析推断变量并挂进 `self.functions`。
    lifted_closures: Vec<LiftedClosure>,
    /// 推断上下文。
    infer: InferCtxt<'static>,
    /// 下一个定义 ID。
    next_def: u32,
    /// 下一个局部 ID。
    next_local: u32,
    /// 下一个基本块 ID。
    next_bb: u32,
    /// 当前作用域深度（用于实现 shadowing）。
    scope_depth: usize,
    /// 作用域栈：每层记录在该层引入的局部变量。
    scope_stack: Vec<Vec<LocalId>>,
    /// 词法作用域栈：每层是"名字 → DefId"的绑定表。
    ///
    /// 与 `scope_stack` 同步 push/pop，用于把源码里的标识符解析到
    /// 对应的局部变量（shadowing 时内层遮蔽外层）。
    local_scopes: Vec<HashMap<String, DefId>>,
    /// DefId → LocalId：区分"这个 DefId 其实是局部变量"。
    locals_by_def: HashMap<DefId, LocalId>,
    /// 当前函数 ID（用于 return 检查）。
    current_function: Option<DefId>,
    /// 当前函数是否是 `async fn`（`.await` 只能在异步上下文中使用）。
    current_is_async: bool,
    /// 函数 DefId 的登记顺序（与 `prog.functions` 的 cfg 过滤后顺序一致）。
    ///
    /// 同一名字可能同时有用户定义与标准库定义（如用户 `fn swap` 与
    /// `core` 的 `swap<T>`）。`find_def(name)` 只按名字查、且只认首个
    /// （用户）定义，但阶段 6/7 逐个处理 `prog.functions` 时**每个函数**
    /// 都要拿到它自己的 DefId——否则标准库 `swap<T>` 的签名会覆盖用户
    /// `swap` 的条目，用户元组返回类型被静默丢掉（`55_tuple_pattern`）。
    function_def_ids: Vec<DefId>,
    /// 错误集合（延迟报告，允许错误恢复）。
    errors: Vec<CompileError>,
    /// 导入解析缓存。
    use_aliases: HashMap<String, DefId>,
    /// 裸枚举变体导入（`use core::Option::Some`）：叶子变体名 → 宿主枚举的
    /// DefId。`use_tree_root` 只加载模块，类型检查器另行把叶子段登记到这里；
    /// 调用点 `Some(7)` 走 `enum_variant_target` 的 `Variable` 分支查表。
    /// 同名变体（两个枚举都有 `Some`）后注册的覆盖先注册的——与
    /// `use_aliases` 的"后导入遮蔽先导入"语义一致。
    variant_aliases: HashMap<String, DefId>,
    /// 已加载的标准库模块名。
    ///
    /// `use io::{println}` 里的 `io` 不是一个可调用的符号，而是模块
    /// 限定符——标准库的定义已经被并进当前编译单元，所以这里只需要
    /// 知道"这个名字是模块"，不用再解析成 DefId。
    known_modules: HashSet<String>,
    /// 每个模块导出的条目名（`use foo::*` 按它把模块的全部顶层名字导入）。
    module_exports: std::collections::HashMap<String, Vec<String>>,
    /// 警告集合（deprecated 使用等）。
    warnings: Vec<CompileError>,
    /// cfg 求值用的目标平台信息。
    ///
    /// 条目的 `#[cfg]` 条件要按**编译目标**求值：`#[cfg(target_os =
    /// "linux")]` 在交叉编译到 Linux 时成立，在 macOS 上不成立。
    cfg_target: attributes::CfgTarget,
    /// AST 层的函数表（名字 → 函数）。
    ///
    /// comptime 函数需要在**类型检查函数体之前**就能被求值（`comptime fact(4)`
    /// 出现在某个函数体里，而 `fact` 自己可能还没检查到）。HIR 这时候还不存在，
    /// 所以必须留一份 AST。HIR 版求值器用于阶段 12 的折叠，AST 版用于
    /// `check_expr` 里的即时求值。
    ast_fns: HashMap<String, ast::Function>,
    /// 待绑定的循环标签。
    ///
    /// `Expression::Labeled` 先把标签放在这里，紧接着检查的
    /// `while` / `loop` / `for` 会取走它并写进 HIR。
    pending_loop_label: Option<String>,
    /// 活动循环的 break 上下文栈（`loop` / `while` / `for` 各占一项）。
    ///
    /// `Expression::Loop` 压入 `is_value: true` 的项（`break v` 把 `v` 的类型
    /// 与该循环的 break 类型统一），`while` / `for` 压入 `is_value: false` 的项
    /// （`break` 作语句，值只检查后丢弃——Rust 里 `while`/`for` 的 `break` 不能
    /// 带值，zr 沿用"检查但不产生表达式值"）。`break` 取最内层（无标签）或按
    /// 标签匹配的项。早先只记 `loop` 的 break 类型，`let x = loop { break 7; }`
    /// 静默得 0。
    loop_ctxs: Vec<LoopBreakCtx>,
    /// 可单态化的泛型函数 / 泛型 `impl` 方法的 AST。
    ///
    /// 声明阶段只把类型参数当作推断变量，函数体按具体类型重新检查才能真正
    /// 生成代码。键是展开后的函数名（`Type__method`）。
    generic_asts: HashMap<DefId, ast::Function>,
    /// 泛型参数名（按声明顺序）：自由函数来自 `fn<T>`，方法来自 `impl<T>`。
    generic_names: HashMap<DefId, Vec<String>>,
    /// 泛型函数的 trait 边界（`fn f<T: Ord>` 里的 `Ord`），按 `generic_names`
    /// 里类型参数的顺序对齐。
    ///
    /// 单独存，不塞进 `generic_names`（那里只有名字）：单态化时必须拿具体实参
    /// 去验每个边界。没存的话 `T: Ord` 解析完就被丢掉，边界等于不存在。
    generic_bounds: HashMap<DefId, Vec<Vec<DefId>>>,
    /// 每个类型参数对应的 trait 边界，已解析成 trait DefId（按声明顺序）。
    ///
    /// 存解析好的 DefId 而不是 AST 类型：单态化时已经过了 trait 检查阶段，
    /// 名字都能解析；存 DefId 让 `check_trait_bounds` 变成纯 `&self`，
    /// 不必再借 `ast_to_hir_type` 的可变引用。
    /// 单态实例去重：`(基础函数, 实参类型键)` → 实例 DefId。
    mono_cache: HashMap<(DefId, String), DefId>,
    /// 已登记、等待检查函数体的单态实例。
    mono_pending: Vec<MonoInstance>,
    /// 泛型类型定义（结构体 / 枚举）的类型参数占位符，按声明顺序。
    ///
    /// 字段 / 载荷类型里出现的就是这些 `Ty::Infer`；`Ty::Named(id, args)`
    /// 取值时用 `args` 替换它们。
    type_generic_params: HashMap<DefId, Vec<Ty>>,
}

/// 一次泛型单态化：用 `subst` 里的具体类型替换泛型参数。
#[derive(Clone, Debug)]
struct MonoInstance {
    /// 泛型函数本身。
    base: DefId,
    /// 生成的实例函数。
    id: DefId,
    /// 泛型参数名 → 具体类型。
    subst: Vec<(String, Ty)>,
    /// 实例化发生处的源码位置（`cannot infer type parameter` 报在这）。
    span: ast::Span,
}

impl Default for TypeChecker {
    fn default() -> Self {
        // InferCtxt 借用 TypeChecker 的字段，无法在构造时直接填入。
        // 先用 empty() 占位（指向 'static 空集合），实际检查时再按需重建。
        Self {
            max_int_bits: 128,
            pointer_bits: 64,
            symbols: HashMap::new(),
            def_names: HashMap::new(),
            type_defs: HashMap::new(),
            functions: HashMap::new(),
            async_outputs: HashMap::new(),
            trait_defs: HashMap::new(),
            trait_defs_by_name: HashMap::new(),
            impl_blocks: Vec::new(),
            impl_targets: HashMap::new(),
            drop_impls: HashMap::new(),
            consts: HashMap::new(),
            statics: HashMap::new(),
            locals: HashMap::new(),
            mutable_locals: HashSet::new(),
            moved_locals: HashSet::new(),
            moving_now: HashSet::new(),
            type_params: Vec::new(),
            impl_generics: HashMap::new(),
            generic_fn_span: None,
            closure_consumed: 0,
            closure_ctx: Vec::new(),
            closure_captures: Vec::new(),
            lifted_closures: Vec::new(),
            closures_by_env: HashMap::new(),
            infer: InferCtxt::empty(),
            next_def: 1,
            next_local: 1,
            next_bb: 0,
            scope_depth: 0,
            scope_stack: Vec::new(),
            local_scopes: Vec::new(),
            locals_by_def: HashMap::new(),
            current_function: None,
            current_is_async: false,
            function_def_ids: Vec::new(),
            errors: Vec::new(),
            use_aliases: HashMap::new(),
            variant_aliases: HashMap::new(),
            known_modules: HashSet::new(),
            module_exports: std::collections::HashMap::new(),
            warnings: Vec::new(),
            cfg_target: attributes::CfgTarget::host(),
            ast_fns: HashMap::new(),
            pending_loop_label: None,
            loop_ctxs: Vec::new(),
            generic_asts: HashMap::new(),
            generic_names: HashMap::new(),
            generic_bounds: HashMap::new(),
            mono_cache: HashMap::new(),
            mono_pending: Vec::new(),
            type_generic_params: HashMap::new(),
        }
    }
}

impl TypeChecker {
    /// 创建类型检查器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 执行完整类型检查，返回 HIR 模块。
    /// 告知类型检查器哪些名字是已加载的标准库模块。
    pub fn set_known_modules(&mut self, modules: HashSet<String>) {
        self.known_modules = modules;
    }

    /// 告知类型检查器每个模块导出的条目名，供 `use foo::*` 导入。
    pub fn set_module_exports(
        &mut self,
        exports: std::collections::HashMap<String, Vec<String>>,
    ) {
        self.module_exports = exports;
    }

    /// 设置目标允许的最大整数位宽。
    ///
    /// 32 位目标目前没有多字长寄存器对，写出来的 i64 只是**静默截断**成
    /// 32 位——这种"能编过但结果是错的"比报错糟糕得多，所以在类型检查阶段
    /// 就把超宽的显式标注挡掉。
    pub fn set_max_int_bits(&mut self, bits: u16) {
        self.max_int_bits = bits;
    }

    /// 设置目标指针位宽，供 `usize` / `isize` 解析使用。
    pub fn set_pointer_bits(&mut self, bits: u16) {
        self.pointer_bits = bits;
    }

    /// 设置 cfg 求值用的目标平台信息。
    ///
    /// 必须按**编译目标**设置，而不是宿主平台：交叉编译到 Linux 时，
    /// `#[cfg(target_os = "linux")]` 的条目应当存在。
    pub fn set_cfg_target(&mut self, target: attributes::CfgTarget) {
        self.cfg_target = target;
    }

    /// 条目的 cfg 条件是否成立（成立则保留该条目）。
    fn cfg_enabled(&self, attrs: &[ast::Attribute]) -> bool {
        attributes::is_cfg_enabled(attrs, &self.cfg_target)
    }

    /// 整数字面量后缀 → 具体整型。无后缀或未知后缀返回 `None`。
    ///
    /// `usize`/`isize` 按目标指针位宽处理。
    fn int_suffix_ty(&self, suffix: &str) -> Option<Ty> {
        Some(match suffix {
            "i8" => Ty::Int(8),
            "i16" => Ty::Int(16),
            "i32" => Ty::Int(32),
            "i64" => Ty::Int(64),
            "i128" => Ty::Int(128),
            "isize" => Ty::Int(self.pointer_bits as u16),
            "u8" => Ty::Uint(8),
            "u16" => Ty::Uint(16),
            "u32" => Ty::Uint(32),
            "u64" => Ty::Uint(64),
            "u128" => Ty::Uint(128),
            "usize" => Ty::Uint(self.pointer_bits as u16),
            _ => return None,
        })
    }

    fn check_int_width(&self, w: u16) -> Result<()> {
        if w > self.max_int_bits {
            return Err(CompileError::type_error(format!(
                "{} 位整数在当前目标上尚未实现：该目标只有 {} 位寄存器，\
                 编译器暂时还不能把宽整数拆成寄存器对。现在生成的是静默截断的代码，\
                 所以直接报错。",
                w, self.max_int_bits
            )));
        }
        Ok(())
    }

    pub fn check_program(&mut self, prog: &Program) -> Result<hir::HirModule> {
        // 诊断名字表按程序重建：`DefId` 从 1 重新分配，残留会让新程序里的
        // 类型在错误信息里显示成上一次检查的某个同名编号的类型。
        infer::clear_def_names();
        // impl 方法先展开成普通函数（名字 mangle 成 `Type__method`），这样
        // 现有签名/函数体检查、MIR 降级都能复用同一张函数表。
        let mut expanded = prog.clone();
        for ib in &prog.impls {
            let Some(target_name) = ast_type_name(&ib.target) else {
                continue;
            };
            for method in &ib.methods {
                // cfg 过滤：被 cfg 掉的方法不存在。
                //
                // impl 块本身在解析阶段就被整体剔除了（属性没有留在 AST
                // 上），但**方法**的属性留下来了，所以这里要再过滤一次。
                if !self.cfg_enabled(&method.attrs) {
                    continue;
                }
                let mut m = method.clone();
                m.name = format!("{target_name}__{}", method.name);
                self.impl_targets.insert(m.name.clone(), ib.target.clone());
                self.impl_generics.insert(m.name.clone(), ib.generics.clone());
                expanded.functions.push(m);
            }
        }
        // `#[derive(..)]` 的方法体：同样在声明收集之前补进函数表。
        //
        // 与上面的 impl 展开分工一致——`generate_derive_impls`（阶段 0）只
        // 登记"实现了哪个 trait"（供 `T: Eq` 边界校验），方法体在这里生成，
        // 这样后续的阶段（签名、函数体、MIR）完全按普通方法处理。
        //
        // 先收集再 push：`expanded.type_defs` 与 `expanded.functions` 同属
        // 一个结构，边遍历边改借用检查过不去。
        let mut derived: Vec<ast::Function> = Vec::new();
        for td in &expanded.type_defs {
            let (name, attrs) = match td {
                ast::TypeDef::Struct(s) => (&s.name, &s.attrs),
                ast::TypeDef::Enum(e) => (&e.name, &e.attrs),
                // 联合体没有 attrs 槽位，别名不产生方法（与
                // `process_type_attrs` 的 `_ => &[]` 一致）。
                ast::TypeDef::Union(_) | ast::TypeDef::Alias { .. } => continue,
            };
            let derives = attributes::derive_traits(attrs);
            if derives.is_empty() {
                continue;
            }
            let target = ast::Type::Named {
                path: vec![name.clone()],
                generics: Vec::new(),
            };
            for m in derive::derive_methods(name, td, &derives)? {
                // `impl_targets` 是方法表的路由键：`lookup_method` 先查它再看
                // 符号表，缺了这一步 `a.clone()` 会报 "no field `clone`"。
                self.impl_targets.insert(m.name.clone(), target.clone());
                derived.push(m);
            }
        }
        expanded.functions.extend(derived);
        let prog = &expanded;

        // ===== 阶段 1：声明收集（pass 1）=====
        self.collect_declarations(prog)?;

        // ===== 阶段 0：属性预处理 =====
        //
        // 放在**声明收集之后**：`#[derive(..)]` 要调 `generate_derive_impls`
        // → `find_def` 查刚声明的类型，收集之前符号表是空的，于是整个结构体
        // 变成 "undefined name `P`"（见 `tests/cases/71_derive.zr` 的头注释）。
        // `#[cfg]` 过滤与 deprecated 警告不依赖符号表，挪到这里没有影响。
        self.process_attributes(prog)?;

        // ===== 阶段 2：名称解析 =====
        self.resolve_imports(prog)?;

        // ===== 阶段 3：类型定义检查 =====
        self.check_type_defs(prog)?;

        // ===== 阶段 4：trait / impl 检查 =====
        self.check_traits(prog)?;
        self.check_impls(prog)?;

        // ===== 阶段 5：常量与静态初始化检查（含 CTFE）=====
        self.check_consts(prog)?;
        self.check_statics(prog)?;

        // ===== 阶段 6：函数签名检查 =====
        self.check_function_signatures(prog)?;

        // ===== 阶段 7：函数体类型检查（含推断）=====
        let functions = self.check_function_bodies(prog)?;

        // ===== 阶段 8：借用检查（NLL）=====
        self.borrow_check(&functions)?;

        // ===== 阶段 9：模式穷尽性检查 =====
        self.check_exhaustiveness(&functions)?;

        // ===== 阶段 10：未使用变量 / 未使用导入警告 =====
        self.check_unused(&functions)?;

        // ===== 阶段 11：comptime 函数调用的编译期折叠 =====
        //
        // 必须排在类型检查与借用检查之后：求值器解释的是 HIR，需要名称
        // 解析与类型检查都已经完成。同时也排在最后——这样 const /
        // static 的初始化式里若调用了 comptime 函数，折叠后的常量会一并
        // 生效，不会出现"先按未折叠的 AST 求值一遍"的两遍求值。
        let mut functions = functions;
        self.fold_comptime_functions(&mut functions);

        // ===== 阶段 13：常量定义的最终求值 =====
        //
        // 阶段 5 的 `check_consts` 只能折出纯字面量的初始化式：那会儿函数体
        // 还没检查，comptime 函数根本不存在。`const SIZE: i32 = fact(5)`
        // 要等到这里，comptime 调用折叠完，才能确定 `SIZE` 的值。
        //
        // 两遍求值不是重复劳动：第一遍负责把 `const LIMIT: i32 = 40` 这类
        // 简单常量在函数体检查**之前**变成已知值（`comptime LIMIT + 2` 依赖
        // 它），第二遍才补上需要 comptime 函数的部分。
        self.finalize_consts();

        if !self.errors.is_empty() {
            // 报告第一个错误
            return Err(self.errors.remove(0));
        }

        let drop_impls = self
            .drop_impls
            .iter()
            .filter_map(|(ty, func)| self.def_names.get(func).map(|name| (*ty, name.clone())))
            .collect();

        Ok(hir::HirModule {
            functions,
            def_names: self.def_names.clone(),
            type_defs: self.type_defs.clone(),
            consts: self.consts.clone(),
            drop_impls,
            generic_params: self.type_generic_params.clone(),
            statics: self
                .statics
                .iter()
                .map(|(id, st)| hir::HirStaticDef {
                    id: *id,
                    name: self.def_names.get(id).cloned().unwrap_or_default(),
                    ty: st.ty.clone(),
                    const_value: st.const_value.clone(),
                })
                .collect(),
        })
    }

    /// 常量定义的最终求值：补上阶段 5 折不出来的那些。
    ///
    /// 折不出常量的定义会在这里报错——`const` 的右值必须在编译期可求值，
    /// 这条约束正是 `const` 与 `static` 的区别，不能等到运行期。
    fn finalize_consts(&mut self) {
        // 先把常量表整体取出：求值过程中要读别的常量，直接持借用会撞上
        // `&mut self` 与 `&self.consts` 的双重借用。
        let names: Vec<(DefId, String)> = self
            .consts
            .keys()
            .map(|&id| (id, self.def_names.get(&id).cloned().unwrap_or_default()))
            .collect();
        for (id, name) in names {
            let Some(value) = self.consts.get(&id).map(|c| c.value.clone()) else {
                continue;
            };
            match self.eval_const_from_hir(&value) {
                Ok(_) => {
                    // 值可求值。`hc.value` 已经在阶段 12 折叠过，保持原样。
                }
                Err(_) => {
                    // 阶段 12 的折叠已经处理了 comptime 调用；到这里仍然折不出
                    // 常量，说明右值真的不是编译期常量。
                    self.errors.push(
                        CompileError::internal(format!(
                            "`const {name}` initializer is not a compile-time constant"
                        ))
                        .with_span(value.span),
                    );
                }
            }
        }
    }

    /// 把「comptime fn + 全常量实参」的调用点折成字面量。
    ///
    /// 折叠之后运行期不再有对应的调用：`comptime fn fact(5)` 直接变成
    /// `Const(120)`，`const SIZE: usize = fact(5)` 的值也随之确定。
    fn fold_comptime_functions(&mut self, functions: &mut [hir::HirFunction]) {
        // 只有存在 comptime 函数时才值得走一趟遍历。
        if !functions.iter().any(|f| f.is_comptime) {
            return;
        }
        let table = &self.functions;
        let mut total = 0usize;
        let mut steps = 0u64;

        // 常量定义的右值也要折叠：`const SIZE: i32 = fact(5)` 的求值发生在
        // 阶段 13，折叠必须在那之前完成。
        let const_ids: Vec<DefId> = self.consts.keys().copied().collect();
        for id in const_ids {
            if let Some(c) = self.consts.get_mut(&id) {
                total +=
                    crate::comptime::fold_comptime_calls(c.value.as_mut(), table, 0, &mut steps);
            }
        }

        for func in functions.iter_mut() {
            if let Some(body) = &mut func.body {
                for stmt in &mut body.stmts {
                    let expr = match stmt {
                        hir::HirStmt::Let { init: Some(e), .. }
                        | hir::HirStmt::Var { init: Some(e), .. } => e.as_mut(),
                        hir::HirStmt::Expr(e) => e.as_mut(),
                        _ => continue,
                    };
                    total += crate::comptime::fold_comptime_calls(expr, table, 0, &mut steps);
                }
                if let Some(tail) = &mut body.tail {
                    total +=
                        crate::comptime::fold_comptime_calls(tail.as_mut(), table, 0, &mut steps);
                }
            }
        }
        if total > 0 && self.warnings.is_empty() && std::env::var_os("ZR_VERBOSE").is_some() {
            eprintln!("zr: folded {total} comptime call(s)");
        }
    }

    // ==================================================================
    // 阶段 0：属性预处理
    // ==================================================================

    /// 处理所有属性：cfg 过滤、derive 生成、deprecated 标记。
    fn process_attributes(&mut self, prog: &Program) -> Result<()> {
        // 收集所有条目的属性信息
        //
        // cfg 过滤在这里先做一遍：被 cfg 掉的条目不该产生 deprecated 警告，
        // 更不该进入符号表（引用它要报 "cannot find value"）。
        //
        // 注意：模块级条目（`const` / `static` / `enum` / `impl` / `type`）
        // 的属性没有留在 AST 上，所以它们的 cfg 过滤是在**解析阶段**完成的
        // （见 `parser::parse_module_item_opt`）；这里处理的是属性还在
        // AST 上的条目：`fn` 与 `struct`。
        for item in &prog.functions {
            if !self.cfg_enabled(&item.attrs) {
                continue;
            }
            self.process_fn_attrs(item)?;
        }
        for td in &prog.type_defs {
            if let ast::TypeDef::Struct(s) = td {
                if !self.cfg_enabled(&s.attrs) {
                    continue;
                }
            }
            self.process_type_attrs(td)?;
        }
        for t in &prog.traits {
            if !self.cfg_enabled(&t.attrs) {
                continue;
            }
            self.process_trait_attrs(t)?;
        }
        Ok(())
    }

    /// 处理函数属性。
    fn process_fn_attrs(&mut self, f: &ast::Function) -> Result<()> {
        // deprecated 标记
        if attributes::is_deprecated(&f.attrs) {
            let msg = attributes::deprecated_message(&f.attrs)
                .unwrap_or_else(|| format!("`{}` is deprecated", f.name));
            self.warnings.push(CompileError::new(format!(
                "use of deprecated function `{}`: {}",
                f.name, msg
            )));
        }

        // lint 级别检查
        let lint = attributes::lint_level(&f.attrs, "deprecated");
        if lint == attributes::LintLevel::Deny {
            // deny 级别：deprecated 使用直接报错
            if attributes::is_deprecated(&f.attrs) {
                return Err(CompileError::type_error(format!(
                    "use of deprecated function `{}`",
                    f.name
                )));
            }
        }

        Ok(())
    }

    /// 处理类型定义属性。
    fn process_type_attrs(&mut self, td: &ast::TypeDef) -> Result<()> {
        let name = match td {
            ast::TypeDef::Struct(s) => &s.name,
            ast::TypeDef::Enum(e) => &e.name,
            ast::TypeDef::Union(u) => &u.name,
            ast::TypeDef::Alias { name, .. } => name,
        };
        // 结构体和枚举都保留属性。早先只有 `Struct` 取到 `attrs`，枚举落到
        // `_ => &[]` 分支，`#[derive(..)]`、`#[deprecated]` 在 `enum` 上被
        // **静默丢弃**——`#[derive(Clone)] enum E` 编译通过，可 `T: Clone`
        // 却报"不实现"。属性在声明收集阶段（`collect_declarations`）就已经
        // 写进 HIR 的 `HirEnum.attrs`，这里丢掉纯属遗漏。
        //
        // 联合体走 `&[]`：`ast::UnionDef` 根本没有 `attrs` 字段（联合体的
        // derive 目前无从谈起），要支持得先在 AST 上加槽位。
        let attrs: &[ast::Attribute] = match td {
            ast::TypeDef::Struct(s) => &s.attrs,
            ast::TypeDef::Enum(e) => &e.attrs,
            ast::TypeDef::Union(_) | ast::TypeDef::Alias { .. } => &[],
        };

        // derive 处理：生成默认 impl
        let derives = attributes::derive_traits(attrs);
        if !derives.is_empty() {
            self.generate_derive_impls(name, &derives, attrs)?;
        }

        // deprecated 标记
        if attributes::is_deprecated(attrs) {
            let msg = attributes::deprecated_message(attrs)
                .unwrap_or_else(|| format!("`{}` is deprecated", name));
            self.warnings.push(CompileError::new(format!(
                "use of deprecated type `{}`: {}",
                name, msg
            )));
        }

        Ok(())
    }

    /// 处理 trait 属性。
    fn process_trait_attrs(&mut self, t: &ast::TraitDef) -> Result<()> {
        // deprecated 标记
        if attributes::is_deprecated(&t.attrs) {
            let msg = attributes::deprecated_message(&t.attrs)
                .unwrap_or_else(|| format!("`{}` is deprecated", t.name));
            self.warnings.push(CompileError::new(format!(
                "use of deprecated trait `{}`: {}",
                t.name, msg
            )));
        }
        Ok(())
    }

    /// 为 `#[derive(...)]` 生成默认 impl 块。
    ///
    /// 支持的 derive：
    /// - `Debug` — 生成 `impl Debug for T { fn fmt(&self) -> str { ... } }`
    /// - `Clone` — 生成 `impl Clone for T { fn clone(&self) -> Self { ... } }`
    /// - `Copy` — 生成 `impl Copy for T {}`（marker trait）
    fn generate_derive_impls(
        &mut self,
        type_name: &str,
        traits: &[String],
        attrs: &[ast::Attribute],
    ) -> Result<()> {
        let def_id = self.find_def(type_name)?;

        for trait_name in traits {
            match trait_name.as_str() {
                "Debug" | "Clone" | "Copy"
                | "PartialEq" | "Eq" | "Hash"
                | "PartialOrd" | "Ord" | "Default" => {
                    // 生成 impl 块：在类型检查器中注册为已实现的 trait
                    // 实际的 trait 求解在 infer.rs 的 check_trait_impl 中处理
                    //
                    // 比较类（`PartialEq` 等）与 `Default` 目前**只登记
                    // trait 标记，不生成方法体**：`fn needs_eq<T: Eq>` 这类
                    // 边界校验能通过，但 `.eq()` / `==` 方法不存在（与
                    // `Clone` 现状一致）。真生成方法体的工作见
                    // `generate_derive_impls` 的 TODO 与测试 `71_derive`。
                    let impl_block = hir::HirImpl {
                        target: Ty::Named(def_id, Vec::new()),
                        trait_id: None, // derive 的具体 trait 在检查时按名字比对
                        where_clause: Vec::new(),
                        span: Span::unknown(),
                    };
                    self.impl_blocks.push(impl_block);
                }
                _ => {
                    return Err(CompileError::type_error(format!(
                        "cannot derive `{}`: unknown derive trait",
                        trait_name
                    )));
                }
            }
        }

        // 检查 derive 属性上的 lint 级别
        let lint = attributes::lint_level(attrs, "deprecated");
        if lint == attributes::LintLevel::Deny {
            // deny 级别：检查 derive 的 trait 是否 deprecated
            for trait_name in traits {
                // 简化：假设所有 derive trait 都不 deprecated
                let _ = trait_name;
            }
        }

        Ok(())
    }

    // ==================================================================
    // 阶段 1：声明收集
    // ==================================================================

    fn collect_declarations(&mut self, prog: &Program) -> Result<()> {
        for item in &prog.functions {
            // cfg 过滤：条件不成立的函数**不存在**——不进符号表，引用它
            // 会报 "cannot find value"。
            if !self.cfg_enabled(&item.attrs) {
                continue;
            }
            let id = self.alloc_def_top(item.name.clone());
            // 记录本函数自己的 DefId：同名函数（用户定义 + 标准库定义）在
            // `prog.functions` 里按顺序各占一条，`find_def(name)` 只认首个，
            // 阶段 6/7 必须用这里的逐条 DefId 而不是按名查找。
            self.function_def_ids.push(id);
            self.ast_fns.insert(item.name.clone(), item.clone());
            // 参数类型这里不解析，`check_function_signatures`（阶段 6）只解析
            // 返回类型；参数要等阶段 7 `check_function_bodies` 逐个函数补上。
            // 占位的空参数列表 + `Infer` 返回类型是故意的。
            self.functions.insert(
                id,
                hir::HirFunction {
                    name: item.name.clone(),
                    params: Vec::new(),
                    return_type: Ty::Infer(InferId(0)),
                    body: None,
                    is_async: item.is_async,
                    async_output: None,
                    is_comptime: item.is_comptime,
                    // 以 `;` 结尾的声明（`extern "C"` 块里的函数）按 C ABI 处理。
                    abi: if item.body.is_none() {
                        hir::Abi::C
                    } else {
                        hir::Abi::Zr
                    },
                    attrs: convert_attrs(&item.attrs),
                },
            );
        }
        // 别名目标先记下、整个循环跑完之后再转换：`type Forward = Later;` 里
        // `Later` 可能定义在后面（前向引用），而它的名字要到循环结束才全部进
        // 符号表——当时机解析会报 `cannot find value Later`。只延后别名：
        // 结构体 / 枚举 / union 的目标都是具名类型或字面量，无需等待。
        // （见 `tests/cases/70_type_alias.zr` 的头注释。）
        //
        // `generics` 一并记下：泛型别名（`type Ref<T> = *const T`）的目标里
        // 有 `T`，得靠它建占位符；不记的话 `T` 在第二遍解析时报
        // `cannot find value T`，而且解析器直接丢掉 `<T>` 更糟——静默把
        // `Ref<i32>` 当成 `*const <推断类型>`。
        let mut alias_pending: Vec<(DefId, ast::Type, Vec<ast::GenericParam>)> = Vec::new();
        for td in &prog.type_defs {
            // cfg 过滤：结构体的属性留在 AST 上，可以在这里判断。
            //
            // `enum` / `union` / `type` 别名的属性没有留在 AST 上，它们在
            // 解析阶段就被剔除了（见 `parser::parse_module_item_opt`）。
            if let ast::TypeDef::Struct(s) = td {
                if !self.cfg_enabled(&s.attrs) {
                    continue;
                }
            }
            let id = self.alloc_def_top(match td {
                ast::TypeDef::Struct(s) => s.name.clone(),
                ast::TypeDef::Enum(e) => e.name.clone(),
                ast::TypeDef::Union(u) => u.name.clone(),
                ast::TypeDef::Alias { name, .. } => name.clone(),
            });
            match td {
                ast::TypeDef::Struct(s) => {
                    self.type_defs.insert(
                        id,
                        hir::HirTypeDef::Struct(hir::HirStruct {
                            name: s.name.clone(),
                            attrs: convert_attrs(&s.attrs),
                            fields: Vec::new(),
                            repr: hir::Repr::Rust,
                        }),
                    );
                }
                ast::TypeDef::Enum(e) => {
                    self.type_defs.insert(
                        id,
                        hir::HirTypeDef::Enum(hir::HirEnum {
                            name: e.name.clone(),
                            attrs: convert_attrs(&e.attrs),
                            repr: hir::IntegerRepr::Auto,
                            variants: Vec::new(),
                        }),
                    );
                }
                ast::TypeDef::Union(u) => {
                    self.type_defs.insert(
                        id,
                        hir::HirTypeDef::Union(hir::HirUnion {
                            name: u.name.clone(),
                            fields: Vec::new(),
                        }),
                    );
                }
                ast::TypeDef::Alias { target, generics, .. } => {
                    alias_pending.push((id, target.clone(), generics.clone()));
                }
            }
        }
        // 第二个 pass：现在所有类型名都进了符号表，可以安全转换别名目标。
        //
        // 泛型别名要先 push 类型参数作用域再转换：目标里的 `T` 是类型变量，
        // 靠 `lookup_type_param` 解析。转换出的目标里 `T` 是 `Infer` 占位符，
        // 用法点（`inline_alias_ty`）按实参做替换。作用域必须 pop 掉，否则
        // 残留会让后面无关定义里的 `T` 误解析到这个别名上。
        for (id, target, generics) in alias_pending {
            let scope = self.push_type_params(&generics, ast::Span::unknown());
            self.record_type_generics(id, &generics, &scope);
            let target_ty = self.ast_to_hir_type(&target)?;
            self.type_params.pop();
            self.type_defs.insert(
                id,
                hir::HirTypeDef::Alias {
                    def_id: id,
                    target: target_ty,
                },
            );
        }
        for t in &prog.traits {
            let id = self.alloc_def_top(t.name.clone());
            let (is_marker, _) = classify_trait(t);
            self.trait_defs.insert(
                id,
                hir::HirTrait {
                    assoc_types: Vec::new(),
                    is_marker,
                    super_traits: Vec::new(),
                },
            );
            // 名字表：`#[derive(Clone)]` 生成的 impl 块 `trait_id` 是 `None`
            // （属性 args 里只有字符串），求解得靠名字反查。
            self.trait_defs_by_name.insert(t.name.clone(), id);
            // trait 里的方法也要登记成定义——后面按名字取它们的签名。
            //
            // 名字带 trait 前缀，免得两个 trait 的同名方法（`Eq::eq` 与
            // `PartialEq::eq`）撞成同一个定义。
            for m in &t.methods {
                // cfg 过滤：被 cfg 掉的 trait 方法不存在。
                if !self.cfg_enabled(&m.attrs) {
                    continue;
                }
                let mid = self.alloc_def(format!("{}::{}", t.name, m.name));
                self.ast_fns.insert(format!("{}::{}", t.name, m.name), m.clone());
                self.functions.insert(mid, self.fn_to_hir(m, mid));
            }
        }
        for c in &prog.consts {
            let id = self.alloc_def_top(c.name.clone());
            self.consts.insert(
                id,
                hir::HirConst {
                    ty: Ty::Infer(InferId(0)),
                    value: Box::new(self.dummy_expr()),
                },
            );
        }
        for s in &prog.statics {
            let id = self.alloc_def_top(s.name.clone());
            self.statics.insert(
                id,
                hir::HirStatic {
                    ty: Ty::Infer(InferId(0)),
                    init: Box::new(self.dummy_expr()),
                    is_mut: s.is_mut,
                    const_value: None,
                },
            );
        }
        Ok(())
    }

    // ==================================================================
    // 阶段 2：导入解析
    // ==================================================================

    fn resolve_imports(&mut self, prog: &Program) -> Result<()> {
        for imp in &prog.imports {
            // 先登记变体别名（`use core::Option::Some` 的 `Some` → 宿主
            // `Option` 的 DefId），再跑普通符号解析——这样 `Name` 分支遇到
            // 变体叶子时能从 `variant_aliases` 查到，不报"无法解析"。
            self.register_variant_aliases(&imp.tree, &imp.span);
            self.resolve_use_tree(&imp.tree, &imp.span)?;
        }
        Ok(())
    }

    fn resolve_use_tree(&mut self, tree: &UseTree, span: &ast::Span) -> Result<()> {
        match tree {
            UseTree::Name(name) => {
                if self.known_modules.contains(name) {
                    // 模块限定符：定义已经被并进当前编译单元，无需再解析
                    return Ok(());
                }
                if let Some(id) = self.symbols.get(name).copied() {
                    self.use_aliases.insert(name.clone(), id);
                } else if self.variant_aliases.contains_key(name) {
                    // 枚举变体导入（`use core::Option::Some`）：变体名本身不
                    // 进符号表，宿主枚举在 `variant_aliases` 里（注册在
                    // `resolve_imports` 的预处理）。这里静默放行，裸 `Some(7)`
                    // 的解析由 `enum_variant_target` 的 `Variable` 分支完成。
                } else {
                    self.errors.push(
                        CompileError::name_error(format!("cannot resolve `{}` in import", name))
                            .with_span(*span),
                    );
                }
            }
            UseTree::Rename(inner, alias) => {
                // 解析出的 `Rename` 内层**不一定**是 `Name`：单段路径 `use make as mkr`
                // 是 `Rename(Name("make"), ..)`，但多段路径 `use core::Option as Opt` 经
                // `parse_use_tree` 包成了 `Rename(Group([Name("core"), Name("Option")]), ..)`。
                // 早先这里只匹配 `Name` 内层，于是多段路径的重命名被**静默忽略**
                // ——既不登记别名、也不报错，使用点只剩一句无头无尾的
                // `cannot find value `Opt` in this scope`（P0 级静默失败）。
                //
                // 取的是叶子段（路径的最后一段）：模块段只起限定作用，
                // 真正的名字在叶子。注意叶子**不是**模块名——
                // `use core::Option as Opt` 的叶子是 `Option`，所以不能走
                // `Name` 分支那个 `known_modules` 早退（会把别名丢掉）。
                let leaf = Self::use_tree_leaf_name(inner);
                match leaf {
                    Some(name) if self.symbols.contains_key(&name) => {
                        self.use_aliases.insert(alias.clone(), self.symbols[&name]);
                    }
                    Some(name) => {
                        self.errors.push(
                            CompileError::name_error(format!(
                                "cannot resolve `{}` in import",
                                name
                            ))
                            .with_span(*span),
                        );
                    }
                    None => {}
                }
            }
            UseTree::Group(trees) => {
                // `use core::*` 解析成 `Group([Name("core"), Glob])`：Glob 是
                // 兄弟节点，模块段是它前面的路径。先处理 glob（它要拿到整个
                // 组的上下文），再处理其余兄弟。
                if trees.iter().any(|t| matches!(t, UseTree::Glob)) {
                    let module_name = Self::glob_module_name(trees);
                    match module_name {
                        Some(mod_name) => {
                            if let Some(exports) = self.module_exports.get(&mod_name) {
                                for item in exports {
                                    if let Some(id) = self.symbols.get(item).copied() {
                                        self.use_aliases.insert(item.clone(), id);
                                    }
                                }
                            } else {
                                self.errors.push(
                                    CompileError::name_error(format!(
                                        "cannot glob-import `{}`: unknown module",
                                        mod_name
                                    ))
                                    .with_span(*span),
                                );
                            }
                        }
                        None => {
                            self.errors.push(
                                CompileError::name_error(
                                    "glob import must name a module (e.g. `use core::*`)",
                                )
                                .with_span(*span),
                            );
                        }
                    }
                    // 其余兄弟（如果有）正常递归。
                    for t in trees {
                        if !matches!(t, UseTree::Glob) {
                            self.resolve_use_tree(t, span)?;
                        }
                    }
                    return Ok(());
                }
                for t in trees {
                    self.resolve_use_tree(t, span)?;
                }
            }
            UseTree::Glob => {
                // 到达这里的单独 `Glob`（不在 Group 里）：`use *`，非法。
                self.errors.push(
                    CompileError::name_error("glob import must name a module (e.g. `use core::*`)")
                        .with_span(*span),
                );
            }
        }
        Ok(())
    }

    /// 拍平 use 树路径，把形如 `use core::Option::Some` 的**枚举变体导入**
    /// 登记进 `variant_aliases`（叶子名 → 宿主枚举 DefId）。
    ///
    /// 两条结构都要处理（`parse_use_tree` 与 prelude 注入各造一种）：
    /// - 解析器形式 `Group([Group([core, Option]), Some])`（链式段包在左侧）
    /// - 注入形式 `Group([core, Group([Option, Some])])`（模块段在左侧）
    ///
    /// 统一规则：`Group([i0, i1, ..., ik])` 里**第一个**项是其余项的公共
    /// 前缀上下文，前缀段数 = `segments(i0)`（Name 1 段、Group 递归拼段）；
    /// 第 j 项（j≥1）的路径 = 前缀上下文 ++ 它自身展开的段。`i0` 自身也
    /// 贡献路径（如 `use core::{Option, Result}` 的 `Option`）。
    ///
    /// 对每个**深度 ≥ 2** 的段序列，倒数第二段是枚举的符号名时，把最后
    /// 一段登记为该枚举的变体别名。同名变体（两个枚举都有 `Some`）后登记
    /// 的覆盖先登记的，与 `use_aliases` 的"后导入遮蔽先导入"语义一致。
    fn register_variant_aliases(&mut self, tree: &UseTree, _span: &ast::Span) {
        // 子树贡献的路径段（顺序拼接）。
        fn segments_of(tree: &UseTree, out: &mut Vec<String>) {
            match tree {
                UseTree::Name(n) => out.push(n.clone()),
                UseTree::Group(items) => {
                    for it in items {
                        segments_of(it, out);
                    }
                }
                UseTree::Rename(inner, _) => segments_of(inner, out),
                UseTree::Glob => {}
            }
        }

        // 收集所有叶子路径（前缀 ++ 自身段）。
        fn paths_of(tree: &UseTree, prefix: &[String], out: &mut Vec<Vec<String>>) {
            match tree {
                UseTree::Name(n) => {
                    let mut p = prefix.to_vec();
                    p.push(n.clone());
                    out.push(p);
                }
                UseTree::Group(items) => {
                    if items.is_empty() {
                        return;
                    }
                    // 第一项：自身贡献路径 + 作为其余项的公共前缀。
                    paths_of(&items[0], prefix, out);
                    let mut ctx = prefix.to_vec();
                    segments_of(&items[0], &mut ctx);
                    // 注意：`i0` 自己的路径已经用原 `prefix` 收过了；
                    // 从 `i1` 起用扩展上下文。
                    for it in items.iter().skip(1) {
                        paths_of(it, &ctx, out);
                    }
                }
                UseTree::Rename(inner, _) => paths_of(inner, prefix, out),
                UseTree::Glob => {}
            }
        }

        let mut paths = Vec::new();
        paths_of(tree, &[], &mut paths);
        for p in paths {
            if p.len() < 2 {
                continue;
            }
            let parent = &p[p.len() - 2];
            let leaf = &p[p.len() - 1];
            let Some(enum_id) = self.symbols.get(parent).copied() else {
                continue;
            };
            if !matches!(self.type_defs.get(&enum_id), Some(hir::HirTypeDef::Enum(_))) {
                continue;
            }
            // 叶子自身已是符号（如同名函数/类型）时不覆盖：普通符号优先。
            if self.symbols.contains_key(leaf) {
                continue;
            }
            self.variant_aliases.insert(leaf.clone(), enum_id);
        }
    }

    /// 带路径上下文的 use 树解析。`path` 记录从根到当前节点的中间段
    /// （不含当前节点自身）：`use core::Option::Some` 解析到叶子 `Some` 时
    /// `path == ["core", "Option"]`。
    /// 取 `use` 路径的**叶子**名字。
    ///
    /// `parse_use_tree` 把每个路径段包成一个扁平 Group：`use core::Option`
    /// 变成 `Group([Name("core"), Name("Option")])`，再套一层 `Rename`。
    /// 所以叶子是沿着 `Group` 一路往下取**最后**一个元素（`parse_use_tree`
    /// 把新段往后追加）——`Name` 就是叶子，`Group` 继续往里走，`Glob`
    /// 取不到名字。早先 `resolve_use_tree` 只匹配 `Rename(Name(..), ..)`，
    /// 多段路径的重命名于是被静默忽略。
    fn use_tree_leaf_name(tree: &UseTree) -> Option<String> {
        match tree {
            UseTree::Name(name) => Some(name.clone()),
            UseTree::Group(trees) => {
                let last = trees.last()?;
                Self::use_tree_leaf_name(last)
            }
            UseTree::Rename(inner, _) => Self::use_tree_leaf_name(inner),
            UseTree::Glob => None,
        }
    }

    /// `use foo::*` 里 `*` 所属的模块名。
    ///
    /// 树形是 `Group([Name("foo"), Glob])`（解析器把 `*` 包在路径外面，
    /// 路径段 `foo` 是兄弟节点），模块名就是 Glob 前面的那段路径。纯
    /// `use *`（无模块段）返回 `None`——那在 Rust 里也是非法写法。
    fn glob_module_name(trees: &[UseTree]) -> Option<String> {
        // 找 Glob 在哪个兄弟位置，取其前面的路径段作为模块名。
        for (i, t) in trees.iter().enumerate() {
            if matches!(t, UseTree::Glob) {
                if i == 0 {
                    return None;
                }
                // 前面是模块路径（`Group([Name("a"), Name("b"), Glob])`
                // 代表 `use a::b::*`），取最后一段名字。zr 的标准库模块
                // 都是单段（`core` / `io`），但多段也能取到叶子。
                return Self::last_segment_name(&trees[..i]);
            }
        }
        // 没有直接 Glob 兄弟，可能在嵌套里（`use foo::{bar::*}`）。
        for t in trees {
            if let UseTree::Group(inner) = t {
                if let Some(name) = Self::glob_module_name(inner) {
                    return Some(name);
                }
            }
        }
        None
    }

    /// 取一段 use 路径的最后一段名字（`Group([Name("a"), Name("b")])` → `b`）。
    fn last_segment_name(trees: &[UseTree]) -> Option<String> {
        match trees.last()? {
            UseTree::Name(n) => Some(n.clone()),
            UseTree::Group(items) => Self::last_segment_name(items),
            UseTree::Rename(inner, _) => Self::last_segment_name(std::slice::from_ref(&**inner)),
            UseTree::Glob => None,
        }
    }

    // ==================================================================
    // 阶段 3：类型定义检查
    // ==================================================================

    fn check_type_defs(&mut self, prog: &Program) -> Result<()> {
        for td in &prog.type_defs {
            // cfg 过滤：被 cfg 掉的类型定义没有符号表条目，直接跳过。
            if let ast::TypeDef::Struct(s) = td {
                if !self.cfg_enabled(&s.attrs) {
                    continue;
                }
            }
            match td {
                ast::TypeDef::Struct(s) => self.check_struct_def(s)?,
                ast::TypeDef::Enum(e) => self.check_enum_def(e)?,
                ast::TypeDef::Union(u) => self.check_union_def(u)?,
                ast::TypeDef::Alias { .. } => {} // 已在 collect 阶段处理
            }
        }
        Ok(())
    }

    fn check_struct_def(&mut self, s: &AstStruct) -> Result<()> {
        // 泛型参数先绑上：字段里的 `T` 是类型变量。
        let scope = self.push_type_params(&s.generics, ast::Span::unknown());
        let def_id = self.find_def(&s.name)?;
        self.record_type_generics(def_id, &s.generics, &scope);
        let mut fields = Vec::new();
        let mut offsets = OffsetTracker::new();

        for (idx, f) in s.fields.iter().enumerate() {
            let ty = self.ast_to_hir_type(&f.ty)?;
            offsets.alloc(self.infer.size_of(&ty), self.infer.align_of(&ty));
            // 元组结构体的字段在 AST 里没有名字，这里补成 `"0"`、`"1"`，
            // 与 `.0` / `.1` 的字段名一致。
            let name = if f.name.is_empty() {
                idx.to_string()
            } else {
                f.name.clone()
            };
            fields.push(hir::HirField {
                name,
                ty: ty.clone(),
                index: idx,
            });
        }

        // `#[repr(..)]` 写错了必须报错，不能默默回退到默认布局——
        // 那会让 FFI 结构体的实际布局和调用方以为的不一致。
        let repr = hir_repr(&convert_attrs(&s.attrs))?;
        if let Some(hir::HirTypeDef::Struct(hs)) = self.type_defs.get_mut(&def_id) {
            hs.fields = fields;
            hs.repr = repr;
        }
        // 泛型参数的占位符不能泄漏到后续定义：以前这里不 pop，`impl<T>`
        // 方法的 `T` 靠"上一次 struct 检查残留的作用域"碰巧解析。
        self.type_params.pop();
        Ok(())
    }

    fn check_enum_def(&mut self, e: &ast::EnumDef) -> Result<()> {
        // 泛型参数先绑上：变体载荷里的 `T` 是类型变量。
        let scope = self.push_type_params(&e.generics, ast::Span::unknown());
        let def_id = self.find_def(&e.name)?;
        self.record_type_generics(def_id, &e.generics, &scope);
        let mut variants = Vec::new();

        // 判别式：显式写的用那个值，没写的沿用上一个 + 1（和 Rust 一样）。
        let mut next_discriminant: i128 = 0;
        for v in &e.variants {
            let discriminant = match &v.discriminant {
                Some(d) => {
                    // 编译期求值判别式。
                    let value = self.eval_const_expr(d)?;
                    match *value {
                        hir::HirLiteral::Int(i) => i,
                        _ => {
                            return Err(CompileError::type_error(
                                "enum discriminant must be an integer literal",
                            )
                            .with_span(ast::Span::unknown()))
                        }
                    }
                }
                None => next_discriminant,
            };
            next_discriminant = discriminant + 1;
            let payload = match &v.payload {
                ast::VariantPayload::Unit => hir::HirVariantPayload::Unit,
                ast::VariantPayload::Tuple(types) => {
                    let tys: Result<Vec<Ty>> =
                        types.iter().map(|t| self.ast_to_hir_type(t)).collect();
                    hir::HirVariantPayload::Tuple(tys?)
                }
                ast::VariantPayload::Struct(fields) => {
                    let hir_fields: Result<Vec<HirField>> = fields
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let ty = self.ast_to_hir_type(&f.ty)?;
                            Ok(hir::HirField {
                                name: f.name.clone(),
                                ty,
                                index: i,
                            })
                        })
                        .collect();
                    hir::HirVariantPayload::Struct(hir_fields?)
                }
            };
            // 载荷里有需要析构的资源时直接报错：析构要按"当前是哪个变体"
            // 决定丢哪些字段，枚举的析构胶还没实现——让它悄悄泄漏比报错糟。
            let payload_tys: Vec<Ty> = match &payload {
                hir::HirVariantPayload::Unit => Vec::new(),
                hir::HirVariantPayload::Tuple(ts) => ts.clone(),
                hir::HirVariantPayload::Struct(fields) => {
                    fields.iter().map(|f| f.ty.clone()).collect()
                }
            };
            // 载荷可以是聚合。
            //
            // 枚举的载荷是**按值内联**在判别式后面的，聚合类型在运行期是
            // "指向存储的地址"。地址拷贝是否**安全**只取决于一件事：载荷
            // 需不需要析构（drop glue）。需要析构时，内联地址指的东西随时会
            // 失效，且析构要按"当前是哪个变体"决定丢哪些字段（胶还没实现）；
            // 不需要析构时全字段可拷贝，地址拷贝无损。
            //
            // 早先这里还有一道"载荷必须是标量"的 ban（`is_aggregate_ty` 判定），
            // 它把不需要析构的聚合（如 `SocketAddr::V4(Ipv4Addr, u16)` 这类
            // std 里的全拷贝结构）也一并挡住，过度保守，被 `if false &&` 永久
            // 关掉了。现在把死代码整个删掉：真正的防线就是下面这条 drop glue
            // 检查——全拷贝聚合放行，带析构的聚合明确报错。
            for ty in &payload_tys {
                if self.has_drop_glue(ty, 0) {
                    return Err(CompileError::type_error(
                        "enums with droppable payloads are not supported yet",
                    )
                    .with_span(ast::Span::unknown()));
                }
            }
            variants.push(hir::HirVariant {
                name: v.name.clone(),
                payload,
                discriminant,
            });
        }

        // `#[repr(i64)]` 走的是另一条路（直接存 IntegerRepr），同样要挡住
        // 超宽标注。必须在 `get_mut` 之前算——闭包里再借用 `self` 会和
        // 可变借用冲突。
        if let Some(AstType::I(w)) = e.repr.as_ref() {
            self.check_int_width(*w as u16)?;
        }

        if let Some(hir::HirTypeDef::Enum(he)) = self.type_defs.get_mut(&def_id) {
            he.variants = variants;
            he.repr = e.repr.as_ref().map_or(hir::IntegerRepr::Auto, |t| {
                if let AstType::I(w) = t {
                    match *w {
                        8 => hir::IntegerRepr::I8,
                        16 => hir::IntegerRepr::I16,
                        32 => hir::IntegerRepr::I32,
                        64 => hir::IntegerRepr::I64,
                        128 => hir::IntegerRepr::I128,
                        _ => hir::IntegerRepr::Auto,
                    }
                } else {
                    hir::IntegerRepr::Auto
                }
            });
        }
        self.type_params.pop();
        Ok(())
    }

    fn check_union_def(&mut self, u: &ast::UnionDef) -> Result<()> {
        let def_id = self.find_def(&u.name)?;
        let mut max_size = 0usize;
        let mut max_align = 1usize;
        let mut fields = Vec::new();
        for (idx, f) in u.fields.iter().enumerate() {
            let ty = self.ast_to_hir_type(&f.ty)?;
            let sz = self.infer.size_of(&ty);
            let al = self.infer.align_of(&ty);
            max_size = max_size.max(sz);
            max_align = max_align.max(al);
            fields.push(hir::HirField {
                name: f.name.clone(),
                ty,
                index: idx,
            });
        }
        if let Some(hir::HirTypeDef::Union(hu)) = self.type_defs.get_mut(&def_id) {
            hu.fields = fields;
        }
        Ok(())
    }

    // ==================================================================
    // 阶段 4：trait / impl 检查
    // ==================================================================

    fn check_traits(&mut self, prog: &Program) -> Result<()> {
        for t in &prog.traits {
            let id = self.find_def(&t.name)?;
            // 先验证 super trait 存在（不可变借用），再取可变引用。
            //
            // 边界要存下来，不是"验证了就丢"：`trait Ord: Eq` 意味着实现
            // `Ord` 蕴含实现 `Eq`，trait 求解得靠它把 `Ord` 展开成
            // `{Ord, Eq}`。早先这里 `let _ = ...` 只做了存在性检查，
            // `Ord` 的超类型 `Eq` 永远进不了求解图。
            let super_ids: Vec<DefId> = t
                .super_traits
                .iter()
                .filter_map(|st| match st {
                    AstType::Named { path, .. } => self.resolve_type_path(path).ok(),
                    _ => None,
                })
                .collect();
            // 先解析关联类型的默认值（避免与 get_mut 借用冲突）
            let assoc_types: Vec<_> = t.assoc_types.iter().map(|_| hir::HirAssocType).collect();
            let hir_t = self.trait_defs.get_mut(&id).unwrap();
            hir_t.assoc_types = assoc_types;
            hir_t.super_traits = super_ids;
            for m in &t.methods {
                // cfg 过滤：被 cfg 掉的 trait 方法没有签名条目。
                if !self.cfg_enabled(&m.attrs) {
                    continue;
                }
                // trait 中的方法是声明，不要求有 body
                let key = format!("{}::{}", t.name, m.name);
                let fid = self.find_def(&key)?;
                self.functions.insert(fid, self.fn_to_hir(m, fid));
            }
        }
        Ok(())
    }

    fn check_impls(&mut self, prog: &Program) -> Result<()> {
        for ib in &prog.impls {
            // `impl<T> Option<T>` 的目标类型里有泛型参数，转换前要先绑上。
            // 以前这里靠 `check_type_defs` 里残留的作用域碰巧能解析，把
            // 泄漏修掉之后必须显式 push。
            if !ib.generics.is_empty() {
                self.push_type_params(&ib.generics, ib.span);
            }
            let result = self.check_impl_block(ib);
            if !ib.generics.is_empty() {
                self.type_params.pop();
            }
            result?;
        }
        Ok(())
    }

    fn check_impl_block(&mut self, ib: &ast::ImplBlock) -> Result<()> {
        {
            let target = self.ast_to_hir_type(&ib.target)?;
            // `Drop` 是编译器内建 trait，不要求用户先声明它。
            let is_drop = ib
                .trait_type
                .as_ref()
                .and_then(ast_type_name)
                .is_some_and(|name| name == "Drop");
            if is_drop {
                let Some(target_name) = ast_type_name(&ib.target) else {
                    return Err(CompileError::type_error(
                        "`impl Drop` 的目标必须是命名结构体".to_string(),
                    )
                    .with_span(ib.span));
                };
                let Ty::Named(type_id, _) = target else {
                    return Err(CompileError::type_error(
                        "`impl Drop` 的目标必须是命名结构体".to_string(),
                    )
                    .with_span(ib.span));
                };
                if !matches!(
                    self.type_defs.get(&type_id),
                    Some(hir::HirTypeDef::Struct(_))
                ) {
                    return Err(CompileError::type_error(
                        "`impl Drop` 的目标必须是命名结构体".to_string(),
                    )
                    .with_span(ib.span));
                }
                let Some(method) = ib.methods.iter().find(|m| m.name == "drop") else {
                    return Err(CompileError::type_error(format!(
                        "`impl Drop for {target_name}` 缺少 `fn drop(&mut self)`"
                    ))
                    .with_span(ib.span));
                };
                if method.receiver.is_none() || !method.params.is_empty() {
                    return Err(CompileError::type_error(
                        "`Drop::drop` 的签名必须是 `fn drop(&mut self)`".to_string(),
                    )
                    .with_span(ib.span));
                }
                let mangled = format!("{target_name}__drop");
                let fn_id = self.find_def(&mangled)?;
                if self.drop_impls.insert(type_id, fn_id).is_some() {
                    return Err(CompileError::type_error(format!(
                        "`{target_name}` 已经实现过 Drop"
                    ))
                    .with_span(ib.span));
                }
                self.impl_blocks.push(hir::HirImpl {
                    target,
                    trait_id: None,
                    where_clause: Vec::new(),
                    span: ib.span,
                });
                return Ok(());
            }
            let trait_id = if let Some(tt) = &ib.trait_type {
                match self.ast_to_hir_type(tt)? {
                    Ty::Named(id, _) => Some(id),
                    _ => None,
                }
            } else {
                None
            };
            let where_clause: Vec<_> = ib.where_clause.iter().map(|_| WherePred).collect();
            let hir_impl = hir::HirImpl {
                target: target.clone(),
                trait_id,
                where_clause,
                span: ib.span,
            };
            // 一致性检查：孤儿规则
            check_orphan_rule(&hir_impl, &self.type_defs, &self.trait_defs, ib.span)?;
            // 一致性检查：重叠检测
            check_overlap(&hir_impl, &self.impl_blocks, &self.trait_defs)?;
            self.impl_blocks.push(hir_impl);
        }
        Ok(())
    }

    // ==================================================================
    // 阶段 5 & 6：常量 / 静态 / 函数签名
    // ==================================================================

    fn check_consts(&mut self, prog: &Program) -> Result<()> {
        for c in &prog.consts {
            let id = self.find_def(&c.name)?;
            let ty = self.ast_to_hir_type(&c.ty)?;
            let value = self.check_const_expr(&c.value, &ty)?;
            // 第一遍求值：只折得出纯字面量的初始化式。
            //
            // 这里**不能**因为折不出来就报错——`const SIZE: i32 = fact(5)`
            // 需要 comptime 函数，而函数体要到阶段 7 才检查完。真正的
            // "必须是编译期常量" 校验在 `finalize_consts`（阶段 13）。
            let _ = self.eval_const_expr(&c.value);
            if let Some(hc) = self.consts.get_mut(&id) {
                hc.ty = ty;
                *hc.value = value;
            }
        }
        Ok(())
    }

    fn check_statics(&mut self, prog: &Program) -> Result<()> {
        for s in &prog.statics {
            let id = self.find_def(&s.name)?;
            let ty = self.ast_to_hir_type(&s.ty)?;
            let init = self.check_expr(&s.init, &ty)?;
            // 初值折叠成字面量：`static X: i32 = 1 + 2;` 也得折出 `3`，
            // 否则 MIR 侧没有可编码的字节，数据段就是空的、符号也不生成，
            // 链接期才会报 "undefined symbol"。折不出来不算错——那类
            // 静态量本来就没有可落地的存储（读取时已经报错）。
            let const_value = self.eval_const_from_hir(&init).ok().map(|b| *b);
            if let Some(hs) = self.statics.get_mut(&id) {
                hs.ty = ty;
                *hs.init = init;
                hs.const_value = const_value;
            }
        }
        Ok(())
    }

    fn check_function_signatures(&mut self, prog: &Program) -> Result<()> {
        // `function_def_ids` 只登记 cfg 通过的函数，与 `prog.functions` 的
        // 下标不一致（cfg 掉的函数占位但未登记），所以要独立计数。
        let mut di = 0usize;
        for f in &prog.functions {
            // cfg 过滤：被 cfg 掉的函数没有符号表条目。
            if !self.cfg_enabled(&f.attrs) {
                continue;
            }
            // 用本函数自己的 DefId（按登记顺序），不是 `find_def(&f.name)`——
            // 同名时 `find_def` 只认首个（用户）定义，标准库的重复定义拿不到
            // 自己的条目，签名会覆盖用户的。
            let id = self.function_def_ids.get(di).copied().unwrap_or(self.find_def(&f.name)?);
            di += 1;
            // 签名里的泛型参数 / `Self` 要先绑上——它们是类型变量。
            //
            // `impl<T>` 的类型参数不在方法自己身上（方法被展开成普通函数后
            // `f.generics` 是空的），必须从 impl 块补上，否则 `Vec<T>` 里的
            // `T` 解析不到。
            let impl_gens = self
                .impl_generics
                .get(&f.name)
                .cloned()
                .unwrap_or_default();
            let generic_names: Vec<String> = impl_gens
                .iter()
                .chain(f.generics.iter())
                .filter_map(|g| match g {
                    ast::GenericParam::Type(n, _) => Some(n.clone()),
                    _ => None,
                })
                .collect();
            if !generic_names.is_empty() {
                // 边界按和 `generic_names` 完全相同的顺序收集：先 `impl<T>`、
                // 再 `fn<T>`，且只收类型参数。顺序错位会让 `T: Ord` 的边界
                // 落到另一个参数头上——静默放错约束比不检查更糟。
                let generic_bounds: Vec<Vec<DefId>> = impl_gens
                    .iter()
                    .chain(f.generics.iter())
                    // 必须和 `generic_names` 一样**只收类型参数**，不能给
                    // 生命周期 / `const` 参数留占位空 vec：否则
                    // `fn f<'a, T: Ord>` 的 `names` 是 `["T"]` 而 `bounds` 是
                    // `[[], [Ord]]`，`T` 索引到空 vec，边界静默丢失。
                    .filter_map(|g| match g {
                        ast::GenericParam::Type(_, b) => Some(
                            b.iter()
                                .filter_map(|t| match t {
                                    AstType::Named { path, .. } => {
                                        self.resolve_type_path(path).ok()
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>(),
                        ),
                        _ => None,
                    })
                    .collect();
                self.generic_asts.insert(id, f.clone());
                self.generic_names.insert(id, generic_names);
                self.generic_bounds.insert(id, generic_bounds);
            }
            if !impl_gens.is_empty() {
                self.push_type_params(&impl_gens, ast::Span::unknown());
            }
            self.push_type_params(&f.generics, ast::Span::unknown());
            if let Some(target) = self.impl_targets.get(&f.name).cloned() {
                if let Ok(ty) = self.ast_to_hir_type(&target) {
                    self.push_named_type("Self", ty);
                }
            }
            let mut ret = self.ast_to_hir_type(&f.return_type)?;
            // async fn 的返回类型降级为 `impl Future<Output = T>`（不透明类型）
            if f.is_async && !matches!(ret, Ty::Opaque(_)) {
                let opaque_id = self.alloc_def(format!("async_future_{}", self.next_def));
                self.async_outputs.insert(opaque_id, ret.clone());
                ret = Ty::Opaque(opaque_id);
            }
            let mut params = self
                .functions
                .get(&id)
                .map(|f| f.params.clone())
                .unwrap_or_default();
            if let Some(recv_ty) = self.receiver_ty(f)? {
                params.insert(
                    0,
                    hir::HirParam {
                        local: self.alloc_local(),
                        ty: recv_ty,
                    },
                );
            }
            if let Some(hf) = self.functions.get_mut(&id) {
                hf.return_type = ret;
                hf.params = params;
            }
            if self.impl_targets.contains_key(&f.name) {
                self.type_params.pop();
            }
            if !impl_gens.is_empty() {
                self.type_params.pop();
            }
            self.type_params.pop();
        }
        Ok(())
    }

    /// `impl` 方法的 receiver 类型。关联函数（没有 receiver）返回 `None`。
    fn receiver_ty(&mut self, f: &ast::Function) -> Result<Option<Ty>> {
        let Some(recv) = &f.receiver else {
            return Ok(None);
        };
        let self_ty = match self.impl_targets.get(&f.name).cloned() {
            Some(t) => self.ast_to_hir_type(&t)?,
            None => Ty::Unit,
        };
        Ok(Some(match &recv.ty {
            Some(t) => self.ast_to_hir_type(t)?,
            None => Ty::Reference(
                Box::new(self_ty),
                match recv.mutability {
                    ast::Mutability::Mutable => hir::Mutability::Mutable,
                    ast::Mutability::Immutable => hir::Mutability::Immutable,
                },
            ),
        }))
    }

    // ==================================================================
    // 阶段 7：函数体类型检查
    // ==================================================================

    fn check_function_bodies(&mut self, prog: &Program) -> Result<Vec<hir::HirFunction>> {
        let _result: Vec<hir::HirFunction> = Vec::new();
        // 与 `check_function_signatures` 相同：cfg 过滤后独立计数，逐条取
        // 本函数自己的 DefId（同名用户/标准库定义不能按名查）。
        let mut di = 0usize;
        for f in &prog.functions {
            // cfg 过滤：被 cfg 掉的函数没有符号表条目。
            if !self.cfg_enabled(&f.attrs) {
                continue;
            }
            let id = self.function_def_ids.get(di).copied().unwrap_or(self.find_def(&f.name)?);
            di += 1;
            // 泛型函数的体跳过：类型参数是推断变量，按它检查只会报一堆
            // 假错。函数体留空，用到它时再按具体类型单态化检查。
            // `impl<T>` 里的方法同理（`impl_generics` 由展开阶段记录）。
            let impl_gens = self
                .impl_generics
                .get(&f.name)
                .cloned()
                .unwrap_or_default();
            if !f.generics.is_empty() || !impl_gens.is_empty() {
                if self.generic_fn_span.is_none() {
                    self.generic_fn_span = Some(ast::Span::unknown());
                }
                if let Some(hf) = self.functions.get_mut(&id) {
                    hf.body = None;
                }
                continue;
            }
            self.current_function = Some(id);
            self.current_is_async = f.is_async;
            self.scope_stack.clear();
            self.push_scope();

            // 函数体按 async fn 的真实输出类型检查；对外签名仍是 Future。
            let declared = self.functions[&id].return_type.clone();
            let ret_ty = match &declared {
                Ty::Opaque(opaque) => self.async_outputs.get(opaque).cloned().unwrap_or(declared),
                _ => declared,
            };
            self.infer.return_ty = Some(ret_ty.clone());

            // impl 方法里的 `Self` 是实现的目标类型。
            if let Some(target) = self.impl_targets.get(&f.name).cloned() {
                if let Ok(ty) = self.ast_to_hir_type(&target) {
                    self.push_named_type("Self", ty);
                }
            }

            // 检查 receiver。impl 方法的目标类型在展开阶段记录；
            // 方法体里的 `self` 按目标类型绑定，结构体值本身以指针表示。
            let mut params = Vec::new();
            if let Some(ty) = self.receiver_ty(f)? {
                let (_, local) = self.bind_local("self", &ty);
                params.push(hir::HirParam { local, ty });
            }

            // 检查参数
            for p in &f.params {
                let ty = self.ast_to_hir_type(&p.ty)?;
                // 参数模式本身会负责绑定名字（`n` / `mut n` / 复杂模式）
                let pat = self.lower_pattern(&p.pattern, &ty)?;
                let local = first_binding(&pat).unwrap_or_else(|| self.alloc_local());
                params.push(hir::HirParam {
                    local,
                    ty: ty.clone(),
                });
            }

            // 检查函数体
            let body = if let Some(b) = &f.body {
                Some(self.check_block(b, &ret_ty)?)
            } else {
                None
            };

            if let Some(hf) = self.functions.get_mut(&id) {
                hf.params = params;
                hf.body = body;
                if f.is_async {
                    hf.async_output = Some(ret_ty);
                }
            }
            self.pop_scope();
            if self.impl_targets.contains_key(&f.name) {
                self.type_params.pop();
            }
            if !impl_gens.is_empty() {
                self.type_params.pop();
            }
        }
        // 泛型单态实例：签名在调用点已登记，这里补上函数体。实例体里可能
        // 又调用新的泛型函数，所以用下标循环直到队列不再增长。
        let mut i = 0;
        while i < self.mono_pending.len() {
            let inst = self.mono_pending[i].clone();
            self.check_instance_body(&inst)?;
            i += 1;
        }
        // 提升闭包：无捕获闭包登记时形参/返回类型可能还是推断变量
        // （`let id = |x| x; id(7)` 里 `x` 靠调用反推）。所有函数体检查完
        // 再统一 resolve，把仍停着的推断变量明确报错，并回填函数体。
        self.finalize_lifted_closures()?;
        // 函数顺序必须确定：按 DefId 排序（保证可复现产物）。
        let mut funcs: Vec<_> = self.functions.iter().collect();
        funcs.sort_by_key(|(id, _)| id.0);
        Ok(funcs.into_iter().map(|(_, f)| f.clone()).collect())
    }

    fn check_block(&mut self, b: &AstBlock, expected: &Ty) -> Result<hir::HirBlock> {
        self.push_scope();
        // 移动检查按块收集：进入块时保存外层已移动的局部，离开时恢复。
        // 块内 `let b = a` 移动的 `a` 只在块内生效（出了块 `a` 已经不存在，
        // 或按作用域规则被遮蔽）；跨分支的移动状态不在这里合并——那需要
        // 数据流分析，属借用检查的完整实现，本最小版不覆盖。
        let saved_moved = self.moved_locals.clone();
        self.moved_locals.clear();
        self.moving_now.clear();
        let mut stmts = Vec::new();
        for s in &b.stmts {
            stmts.push(self.check_stmt(s)?);
        }
        let diverges = block_diverges(b);
        let tail = if let Some(t) = &b.tail {
            let e = self.check_expr(t, expected)?;
            self.infer
                .subtype(&e.ty, expected, t.span())
                .map_err(|e| self.errors.push(e))
                .ok();
            Some(Box::new(e))
        } else if diverges {
            None
        } else if !matches!(expected, Ty::Unit | Ty::Never) && !self.current_returns() {
            // 块缺少尾部表达式，但期望非空类型
            self.errors.push(
                CompileError::type_error(format!(
                    "block missing value: expected `{}`, found `()`",
                    self.infer.type_name(expected)
                ))
                .with_span(b.span),
            );
            None
        } else {
            None
        };
        self.moved_locals = saved_moved;
        self.pop_scope();
        Ok(hir::HirBlock { stmts, tail })
    }

    fn check_stmt(&mut self, s: &Statement) -> Result<HirStmt> {
        match s {
            Statement::Let(_pat, ty_ann, init, span) => {
                // 带捕获闭包作为值（`let cap = |x| x + n;`）：绑定类型是环境
                // 结构体，不是函数类型。`infer_expr_ty` 对 Lambda 返回 `fn(..)`
                // （那是"调用点视角"），但存下来的值实际是 `__env_N`。没有
                // 标注时**只检查一次** Lambda、用其结果类型作绑定类型，并在
                // 下方复用同一份 HIR——否则两次 `check_lambda` 会生成两个
                // 不同的 `__env_N`，绑定类型与初值类型对不上。
                let pre_checked: Option<HirExpr> = match (ty_ann, init) {
                    (None, Some(Expression::Lambda(..))) => {
                        let h = self.check_expr(init.as_ref().unwrap(), &Ty::Infer(InferId(0)))?;
                        Some(h)
                    }
                    _ => None,
                };
                let ty = if let Some(t) = ty_ann {
                    self.ast_to_hir_type(t)?
                } else if let Some(e) = init {
                    pre_checked
                        .as_ref()
                        .map(|h| self.infer.resolve(&h.ty).clone())
                        .unwrap_or(self.infer_expr_ty(e)?)
                } else {
                    return Err(CompileError::type_error(
                        "type annotations needed for let-binding without initializer",
                    )
                    .with_span(*span));
                };
                let init_hir = if let Some(e) = init {
                    // 移动检查：`let b = a` 对非 `Copy`（含 drop glue）的 `a`
                    // 是移动不是复制。把 `a` 放进 `moving_now`，使检查 `a`
                    // 时按"移动读"对待（不报 use-after-move）；检查完再从
                    // `moving_now` 取出并登记进 `moved_locals`——此后对 `a`
                    // 的任何**读**都报 use-after-move。
                    //
                    // 只认裸 `Variable`：`let b = a` 这种整值移动是当前最
                    // 常见的坑。`let b = a.field`（字段级部分移动）、`f(a)`
                    // （按值传参）暂不覆盖，留待借用检查的完整实现。
                    if let Expression::Variable(name, _) = e {
                        if let Ok(id) = self.resolve_name(name, e.span()) {
                            if let Some(local) = self.as_local(id) {
                                if !self.moved_locals.contains(&local)
                                    && self.is_move_ty(&self.locals[&local])
                                {
                                    self.moving_now.insert(local);
                                }
                            }
                        }
                    }
                    let h = match (pre_checked, ty_ann) {
                        (Some(h), _) => h,
                        (None, _) => self.check_expr(e, &ty)?,
                    };
                    // 从 `moving_now` 取出并登记为已移动。
                    if let Expression::Variable(name, _) = e {
                        if let Ok(id) = self.resolve_name(name, e.span()) {
                            if let Some(local) = self.as_local(id) {
                                self.moving_now.remove(&local);
                                if self.is_move_ty(&self.locals[&local]) {
                                    self.moved_locals.insert(local);
                                }
                            }
                        }
                    }
                    self.check_partial_move(&h, e.span())?;
                    // `unify` 两参对称，顺序只影响文案：传反了 `let x: i32 = "hi"`
                    // 会报成 "expected `str`, found `i32`"。
                    self.infer
                        .unify(&ty, &h.ty, e.span())
                        .map_err(|e| self.errors.push(e))
                        .ok();
                    Some(Box::new(h))
                } else {
                    None
                };
                let hir_pat = self.lower_pattern(_pat, &ty)?;
                // 非绑定模式（通配 / 字面量 / 元组）没有单个可引用的名字，
                // 用一个匿名局部占位，保证 `HirStmt::Let` 的形状不变。
                let local = first_binding(&hir_pat).unwrap_or_else(|| self.alloc_local());
                Ok(HirStmt::Let {
                    local,
                    init: init_hir,
                })
            }
            Statement::LetElse(pat, ty_ann, init, else_blk, span) => {
                // `let PAT = EXPR else { .. };`
                //
                // 先求出被匹配值，落到一个临时局部量里；再按模式算条件，
                // 不匹配时执行 else（必须发散），匹配时把各绑定写到
                // 当前作用域的局部量上。后续语句用到的名字在
                // `lower_pattern` 阶段就已经解析到这些局部量，所以
                // 只要能走到后续语句，绑定一定已经初始化。
                // else 分支先检查：这时候模式里的名字还不存在，
                // 在 else 里用它们会报"找不到名字"（和 Rust 一致）。
                let else_hir = self.check_block(else_blk, &Ty::Never)?;
                // 没有类型标注时按推断变量检查初始值，再取回实际类型
                // （`let d = Step::Done;` 这类无载荷变体构造才推得出来）。
                let (ty, init_hir) = if let Some(t) = ty_ann {
                    let ty = self.ast_to_hir_type(t)?;
                    let h = self.check_expr(init, &ty)?;
                    (ty, h)
                } else {
                    let h = self.check_expr(init, &Ty::Infer(InferId(0)))?;
                    let ty = self.infer.resolve(&h.ty);
                    (ty, h)
                };
                self.check_partial_move(&init_hir, init.span())?;
                let scrut_local = self.alloc_local();
                self.locals.insert(scrut_local, ty.clone());
                self.infer.locals.insert(scrut_local, ty.clone());
                let scrutinee = HirExpr {
                    kind: ExprKind::Local(scrut_local),
                    ty: ty.clone(),
                    span: *span,
                };
                let hir_pat = self.lower_pattern(pat, &ty)?;
                let (pat_cond, binds) = self.pat_match(&scrutinee, &hir_pat, *span)?;
                let mut then_stmts: Vec<hir::HirStmt> = Vec::new();
                for (local, value) in binds {
                    then_stmts.push(hir::HirStmt::Let {
                        local,
                        init: Some(Box::new(value)),
                    });
                }
                let then_block = hir::HirBlock {
                    stmts: then_stmts,
                    tail: None,
                };
                let cond = pat_cond.unwrap_or_else(|| bool_lit(true, *span));
                let if_expr = HirExpr {
                    kind: ExprKind::If(Box::new(cond), then_block, Some(else_hir)),
                    ty: Ty::Unit,
                    span: *span,
                };
                let block = hir::HirBlock {
                    stmts: vec![
                        hir::HirStmt::Let {
                            local: scrut_local,
                            init: Some(Box::new(init_hir)),
                        },
                        hir::HirStmt::Expr(Box::new(if_expr)),
                    ],
                    tail: None,
                };
                Ok(HirStmt::Expr(Box::new(HirExpr {
                    kind: ExprKind::Block(block),
                    ty: Ty::Unit,
                    span: *span,
                })))
            }
            Statement::Var(_pat, ty_ann, init, span) => {
                let ty = if let Some(t) = ty_ann {
                    self.ast_to_hir_type(t)?
                } else if let Some(e) = init {
                    self.infer_expr_ty(e)?
                } else {
                    return Err(CompileError::type_error(
                        "type annotations needed for var-binding without initializer",
                    )
                    .with_span(*span));
                };
                let init_hir = if let Some(e) = init {
                    let h = self.check_expr(e, &ty)?;
                    self.check_partial_move(&h, e.span())?;
                    // `unify` 两参对称，顺序只影响文案：传反了 `let x: i32 = "hi"`
                    // 会报成 "expected `str`, found `i32`"。
                    self.infer
                        .unify(&ty, &h.ty, e.span())
                        .map_err(|e| self.errors.push(e))
                        .ok();
                    Some(Box::new(h))
                } else {
                    None
                };
                // 别像以前那样只登记 LocalId：`var y` 的名字必须进作用域，
                // 否则任何使用都报 "cannot find value y"（var 语句实际上没
                // 法用）。走 lower_pattern / bind_local，和 let 同一条路。
                let hir_pat = self.lower_pattern(_pat, &ty)?;
                let local = first_binding(&hir_pat).unwrap_or_else(|| self.alloc_local());
                Ok(HirStmt::Var {
                    local,
                    init: init_hir,
                })
            }
            Statement::Const(name, ast_ty, value_expr, _span) => {
                let ty = self.ast_to_hir_type(ast_ty)?;
                let value = self.check_const_expr(value_expr, &ty)?;
                let def = self.alloc_def(name.clone());
                self.consts.insert(
                    def,
                    hir::HirConst {
                        ty: ty.clone(),
                        value: Box::new(value),
                    },
                );
                Ok(HirStmt::Item)
            }
            Statement::Expr(e, _span) => {
                Ok(HirStmt::Expr(Box::new(self.check_expr(e, &Ty::Unit)?)))
            }
        }
    }

    fn check_expr(&mut self, e: &Expression, expected: &Ty) -> Result<HirExpr> {
        let h = self.check_expr_raw(e, expected)?;
        Ok(self.coerce_array_to_slice(h, expected))
    }

    /// 隐式把定长数组转成切片。只在期望类型已经是 `[]T` 时生效。
    fn coerce_array_to_slice(&self, e: HirExpr, expected: &Ty) -> HirExpr {
        let Ty::Slice(exp_elem, mutability) = self.infer.resolve(expected) else {
            return e;
        };
        let Ty::Array(act_elem, _) = self.infer.resolve(&e.ty) else {
            return e;
        };
        if self.infer.resolve(&act_elem) != self.infer.resolve(&exp_elem) {
            return e;
        }
        let span = e.span;
        HirExpr {
            kind: ExprKind::ArrayToSlice(Box::new(e)),
            ty: Ty::Slice(act_elem, mutability),
            span,
        }
    }

    fn check_expr_raw(&mut self, e: &Expression, expected: &Ty) -> Result<HirExpr> {
        let span = e.span();
        let kind = match e {
            Expression::Literal(lit, _) => {
                let (kind, ty) = self.check_literal(lit, expected)?;
                return Ok(HirExpr { kind, ty, span });
            }
            Expression::Variable(name, _) => {
                let id = match self.resolve_name(name, span) {
                    Ok(id) => id,
                    // 裸无载荷枚举变体（`use core::Option::None` 导入的 `None`
                    // 当值用）：不是名字，是"0 实参的变体构造"。
                    Err(_) => {
                        if let Some(enum_id) = self.variant_aliases.get(name).copied() {
                            let Some(hir::HirTypeDef::Enum(he)) =
                                self.type_defs.get(&enum_id)
                            else {
                                return Err(CompileError::name_error(
                                    format!("cannot find value `{name}` in this scope"),
                                )
                                .with_span(span));
                            };
                            let idx = he
                                .variants
                                .iter()
                                .position(|v| v.name == *name)
                                .unwrap_or(0);
                            if !he
                                .variants
                                .get(idx)
                                .map_or(false, |v| matches!(v.payload, hir::HirVariantPayload::Unit))
                            {
                                return Err(CompileError::name_error(
                                    format!(
                                        "`{name}` is a payload variant, use `{name}(...)` to construct it"
                                    ),
                                )
                                .with_span(span));
                            }
                            return self.check_enum_variant_init(
                                enum_id,
                                idx,
                                &[],
                                expected,
                                span,
                            );
                        }
                        return Err(CompileError::name_error(
                            format!("cannot find value `{name}` in this scope"),
                        )
                        .with_span(span));
                    }
                };
                if let Some(local) = self.as_local(id) {
                    // 移动检查：读一个已被移动的局部（且不是"正在被移动"的
                    // 那个，后者是 `let b = a` 的合法移动读）报 use-after-move。
                    if self.moved_locals.contains(&local)
                        && !self.moving_now.contains(&local)
                        && self
                            .locals
                            .get(&local)
                            .map(|t| self.is_move_ty(t))
                            .unwrap_or(false)
                    {
                        return Err(CompileError::type_error(format!(
                            "use of moved value `{name}`",
                        ))
                        .with_span(span));
                    }
                    ExprKind::Local(local)
                } else {
                    ExprKind::Def(id)
                }
            }
            Expression::Turbofish(inner, targs, _) => {
                // 无载荷变体带显式类型实参：`Option::<i32>::None`。
                if let Expression::Path(path, pspan) = inner.as_ref() {
                    if let Some((def_id, index)) = self.enum_variant_target_path(path, *pspan) {
                        let tys = self.turbofish_targs(targs)?;
                        let want = Ty::Named(def_id, tys);
                        return self.check_enum_variant_init(def_id, index, &[], &want, span);
                    }
                }
                // 结构体字面量带显式类型实参：`Shape::<u8>::Circle { r: 7 }`。
                // 解析器把 `{ .. }` 整个包在 `Turbofish` 里；这里把类型实参
                // 还原成期望类型，`check_struct_init` 据此定下泛型实参。
                if let Expression::StructInit(path, _, _, _) = inner.as_ref() {
                    // 枚举结构体载荷（`Shape::<u8>::Circle { r: 7 }`）的路径最后
                    // 一段是变体名，类型定义在前缀上；普通结构体就是整条路径。
                    let def_id = match path.split_last() {
                        Some((_, prefix)) if !prefix.is_empty() => {
                            match self.resolve_path(prefix, span) {
                                Ok(id)
                                    if matches!(
                                        self.type_defs.get(&id),
                                        Some(hir::HirTypeDef::Enum(_))
                                    ) =>
                                {
                                    id
                                }
                                _ => self.resolve_path(path, span)?,
                            }
                        }
                        _ => self.resolve_path(path, span)?,
                    };
                    let tys = self.turbofish_targs(targs)?;
                    let want = Ty::Named(def_id, tys);
                    return self.check_expr_raw(inner, &want);
                }
                return self.check_expr_raw(inner, expected);
            }
            Expression::Question(inner, _) => {
                return self.check_question(inner, expected, span);
            }
            Expression::Path(path, _) => {
                // 无载荷的枚举变体可以直接当值用：`Shape::Circle`。
                if let Some((def_id, index)) = self.enum_variant_target_path(path, span) {
                    return self.check_enum_variant_init(def_id, index, &[], expected, span);
                }
                let id = self.resolve_path(path, span)?;
                ExprKind::Def(id)
            }
            Expression::Binary(lhs, op, rhs, _) => {
                return self.check_binary(lhs, op, rhs, expected, span);
            }
            Expression::Unary(op, operand, _) => {
                match op {
                    ast::UnaryOp::Deref => {
                        // *p : p 是指针 `*T` 或引用 `&T`，结果是 `T`。
                        let operand = self.check_expr(operand, &Ty::Infer(InferId(0)))?;
                        let inner = pointee_of(&self.infer, &operand.ty).unwrap_or_else(|| {
                            let i = self.infer.fresh_infer(span);
                            self.infer
                                .unify(
                                    &operand.ty,
                                    &Ty::Pointer(Box::new(i.clone()), hir::Mutability::Immutable),
                                    span,
                                )
                                .ok();
                            i
                        });
                        if matches!(self.infer.resolve(&inner), Ty::Float(_)) {
                            return Err(CompileError::type_error(
                                "floating-point dereference is not supported",
                            )
                            .with_span(span));
                        }
                        return Ok(HirExpr {
                            kind: ExprKind::Deref(Box::new(operand)),
                            ty: inner,
                            span,
                        });
                    }
                    ast::UnaryOp::AddrOf => {
                        let operand = self.check_expr(operand, &Ty::Infer(InferId(0)))?;
                        reject_float_address(&self.infer, &operand.ty, span)?;
                        let ty =
                            Ty::Pointer(Box::new(operand.ty.clone()), hir::Mutability::Immutable);
                        return Ok(HirExpr {
                            kind: ExprKind::AddrOf(Box::new(operand)),
                            ty,
                            span,
                        });
                    }
                    ast::UnaryOp::Not => {
                        let operand = self.check_expr(operand, &Ty::Bool)?;
                        // 结果留成推断变量（而不是写死 `bool`）：本语言的 `!`
                        // 在整数上取真值，`let n: i32 = !x;` 依赖结果能与 i32
                        // 统一。写死 bool 会把这类用法判成类型错误。
                        return Ok(HirExpr {
                            kind: ExprKind::Unary(self.ast_unary_op(*op), Box::new(operand)),
                            ty: self.infer.fresh_infer(span),
                            span,
                        });
                    }
                    ast::UnaryOp::Neg | ast::UnaryOp::BitNot => {
                        // 类型**跟随操作数**，不是硬编码的 i32。早先 `Neg`
                        // 一律给 `Int(32)` 当期望，于是 `-30.0` 里的 `30.0`
                        // 推断成默认 f64，与外层的 f32 混算读错位宽。
                        // 把外层期望传下去，让字面量跟着上下文走。
                        let want = if matches!(expected, Ty::Infer(_)) {
                            Ty::Infer(InferId(0))
                        } else {
                            expected.clone()
                        };
                        let operand = self.check_expr(operand, &want)?;
                        let ty = operand.ty.clone();
                        return Ok(HirExpr {
                            kind: ExprKind::Unary(self.ast_unary_op(*op), Box::new(operand)),
                            ty,
                            span,
                        });
                    }
                }
            }
            Expression::Assign(lhs, op, rhs, _) => {
                let l = self.check_expr(lhs, &Ty::Infer(InferId(0)))?;
                let r = self.check_expr(rhs, &l.ty)?;
                self.check_partial_move(&r, rhs.span())?;
                self.check_assignable(&l)?;
                self.require_mutable_binding(&l)?;
                let (kind, ty) = if matches!(op, ast::AssignOp::Assign) {
                    (ExprKind::Assign(Box::new(l), Box::new(r)), Ty::Unit)
                } else {
                    // 复合赋值脱糖：l = l op r
                    let bin_op = self.assign_to_binary(*op);
                    let bin = HirExpr {
                        kind: ExprKind::Binary(bin_op, Box::new(l.clone()), Box::new(r)),
                        ty: l.ty.clone(),
                        span,
                    };
                    (ExprKind::Assign(Box::new(l), Box::new(bin)), Ty::Unit)
                };
                return Ok(HirExpr { kind, ty, span });
            }
            Expression::Call(callee, args, _) => {
                return self.check_call(callee, args, expected, span);
            }
            Expression::MacroCall { name, .. } => {
                // 宏调用在类型检查之前就被展开了（见 `macros::expand`）。
                // 能走到这里说明展开阶段漏掉了这个调用点。
                return Err(CompileError::type_error(format!(
                    "macro `{name}!` was not expanded"
                )));
            }
            Expression::Index(receiver, index, _) => {
                let recv = self.check_expr(receiver, &Ty::Infer(InferId(0)))?;
                // 切片：`s[a..b]` / `s[..b]` / `s[a..]` / `s[..]`。
                // 定长数组退化成 `[]T`，`str` 仍是 `str`，切片保持原可变性。
                if let Expression::Binary(l, op, r, _) = index.as_ref() {
                    let inclusive = match op {
                        ast::BinaryOp::Range => false,
                        ast::BinaryOp::RangeInclusive => true,
                        _ => {
                            return self.check_index_scalar(recv, index, span);
                        }
                    };
                    let base = self.infer.resolve(&recv.ty);
                    let result_ty = match &base {
                        Ty::Array(t, _) => {
                            Ty::Slice(t.clone(), hir::Mutability::Immutable)
                        }
                        Ty::Str => Ty::Str,
                        Ty::Slice(t, m) => Ty::Slice(t.clone(), *m),
                        Ty::Pointer(t, m) | Ty::Reference(t, m) => {
                            Ty::Slice(t.clone(), *m)
                        }
                        other => {
                            return Err(CompileError::type_error(format!(
                                "cannot slice a value of type `{}`",
                                ty_name(other)
                            ))
                            .with_span(span))
                        }
                    };
                    let start = if is_missing_bound(l) {
                        None
                    } else {
                        Some(Box::new(self.check_expr(l, &Ty::Uint(64))?))
                    };
                    let end = if is_missing_bound(r) {
                        None
                    } else {
                        Some(Box::new(self.check_expr(r, &Ty::Uint(64))?))
                    };
                    return Ok(HirExpr {
                        kind: ExprKind::IndexRange(Box::new(recv), start, end, inclusive),
                        ty: result_ty,
                        span,
                    });
                }
                return self.check_index_scalar(recv, index, span);
            }
            Expression::Field(receiver, name, _) => {
                let recv = self.check_expr(receiver, &Ty::Infer(InferId(0)))?;
                let (ty, _idx) = self.check_field_access(&recv.ty, name, span)?;
                return Ok(HirExpr {
                    kind: ExprKind::Field(Box::new(recv), name.clone()),
                    ty,
                    span,
                });
            }
            Expression::AddrOf(e, m, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                reject_float_address(&self.infer, &inner.ty, span)?;
                let ty = Ty::Pointer(Box::new(inner.ty.clone()), *m);
                return Ok(HirExpr {
                    kind: ExprKind::AddrOf(Box::new(inner)),
                    ty,
                    span,
                });
            }
            Expression::Cast(e, target, _) => {
                let target_ty = self.ast_to_hir_type(target)?;
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                self.validate_cast(&inner.ty, &target_ty, span)?;
                // 操作数必须保留。早先这里只做类型检查就把表达式扔了，
                // HIR 里只剩下目标类型，`x as i32` 到 MIR 就变成了 Unit。
                return Ok(HirExpr {
                    kind: ExprKind::Cast(Box::new(inner)),
                    ty: target_ty,
                    span,
                });
            }
            Expression::Block(b, _) => {
                let inner = self.check_block(b, expected)?;
                // 块以 `return` / `throw` 结尾时**发散**：类型是 `never`，
                // 不是 `()`。之前一律当 `()`，于是
                // `if let x = e { return 1 } else { return 0 }` 里
                // match 表达式被当成 `()` 用，语句位置就报
                // "expected `()`, found `i32`"。
                let diverges = inner.tail.is_none() && block_diverges(b);
                let ty = match inner.tail.as_ref() {
                    Some(t) => t.ty.clone(),
                    None if diverges => Ty::Never,
                    None => Ty::Unit,
                };
                return Ok(HirExpr {
                    kind: ExprKind::Block(inner),
                    ty,
                    span,
                });
            }
            Expression::If(cond, then_b, else_b, _) => {
                let c = self.check_expr(cond, &Ty::Bool)?;
                let then_hir = self.check_block(then_b, expected)?;
                let else_hir = if let Some(e) = else_b {
                    let e_expr = if let Expression::Block(b, _) = e.as_ref() {
                        self.check_block(b, expected)?
                    } else {
                        // `else` 后面直接跟一个表达式（`else if ...` / `else 3`），
                        // 把它当尾表达式包成单表达式块。结果必须放进 `tail`，
                        // 否则这里会把它丢掉，MIR 侧看到的就是 `Unit`。
                        let checked = self.check_expr(e, expected)?;
                        hir::HirBlock {
                            stmts: Vec::new(),
                            tail: Some(Box::new(checked)),
                        }
                    };
                    Some(e_expr)
                } else {
                    None
                };
                let then_diverges = block_diverges(then_b);
                let else_diverges = else_b.as_ref().is_some_and(|e| expr_diverges(e));
                let t = if then_diverges {
                    Ty::Never
                } else {
                    then_hir
                        .tail
                        .as_ref()
                        .map(|t| t.ty.clone())
                        .unwrap_or(Ty::Unit)
                };
                let e = match else_hir.as_ref() {
                    None => Ty::Unit,
                    Some(_) if else_diverges => Ty::Never,
                    Some(el) => el
                        .tail
                        .as_ref()
                        .map(|t| t.ty.clone())
                        .unwrap_or(Ty::Unit),
                };
                let ty = self.unify_or(&t, &e);
                // 两个分支都真正**发散**（都以 `return` / `throw` 结束）时，
                // `if` 本身的值是 never：它既不产生值也不正常返回。
                // 没有这一条，`fn f() -> i32 { if c { return 1; } else { return 2; } }`
                // 里的 `if` 作为尾表达式会被拿去和 i32 统一，报类型不匹配。
                //
                // 发散必须按 AST 形状判（`block_diverges`），**不能**看
                // `tail.is_none()`：尾表达式为 `None` 有两种情况——块以 `return;`
                // 结尾（发散），或块最后一句带了分号（`write_fill(...);` 之后直接
                // `}`，不产生值但**不**发散，类型是 `()`）。早先混为一谈，后者被误判
                // 成发散，`if` 于是拿到 `Never`，再经 `unify_or` 的"不一致返回推断
                // 变量"兜底落到 `InferId(0)`——而 `InferId(0)` 是个**活的**推断变量，
                // 长编译里可能早已被绑定（`std/io.zr` 里绑成了 `u8`），`subtype` 一
                // resolve 就解析出 `u8` 去和 `()` 比，报出一条完全无中生有的
                // "expected `()`, found `u8`"。短文件不绑定 `InferId(0)`，所以碰不到。
                let ty = if then_diverges && else_diverges {
                    Ty::Never
                } else {
                    ty
                };
                return Ok(HirExpr {
                    kind: ExprKind::If(Box::new(c), then_hir, else_hir),
                    ty,
                    span,
                });
            }
            Expression::Match(scrutinee, arms, _) => {
                return self.check_match(scrutinee, arms, expected, span);
            }
            Expression::While(cond, body, else_b, _) => {
                let label = self.pending_loop_label.take();
                let c = self.check_expr(cond, &Ty::Bool)?;
                // `while` 的 break 是语句（不产生表达式值），压一个非值上下文，
                // 让 `break v` 在这里被当作语句 break（值只检查后丢弃），不会
                // 误写入外层 `loop` 的结果槽。
                self.loop_ctxs.push(LoopBreakCtx {
                    label: label.clone(),
                    ty: Ty::Unit,
                    is_value: false,
                    value_seen: false,
                    plain_seen: false,
                });
                let b = self.check_block(body, &Ty::Unit)?;
                self.loop_ctxs.pop();
                let _else_hir = if let Some(eb) = else_b {
                    Some(self.check_block(eb, &Ty::Unit)?)
                } else {
                    None
                };
                return Ok(HirExpr {
                    kind: ExprKind::While(label, Box::new(c), b),
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::For(pat, iter, body, _) => {
                // 标签属于整个 `for` 循环（它的 `break` / `continue` 在
                // 降级后仍然作用于这里生成的 `while`）。
                let label = self.pending_loop_label.take();
                // 数组按值遍历：元素逐个绑定到循环变量（与 Rust 的
                // `[T; N]` IntoIterator 一致）。源数组先绑定到局部量，
                // 保证表达式只求值一次；元素移动由 MIR 侧的下标移动
                // 跟踪负责，提前 break 时未遍历的元素仍会析构。
                let raw_iter_ty = self.infer_expr_ty(iter)?;
                let iter_ty = self.infer.resolve(&raw_iter_ty);
                if let Ty::Array(elem_ty, len) = &iter_ty {
                    let Some(n) = hir::array_len_value(len) else {
                        return Err(CompileError::type_error(
                            "cannot iterate over an array with a dynamic length",
                        )
                        .with_span(span));
                    };
                    let elem_ty = (**elem_ty).clone();

                    self.push_scope();
                    let iter_hir = self.check_expr(iter, &iter_ty)?;
                    let hir_pat = self.lower_pattern(pat, &elem_ty)?;
                    let var = first_binding(&hir_pat).unwrap_or_else(|| {
                        let id = self.alloc_local();
                        self.locals.insert(id, elem_ty.clone());
                        self.infer.locals.insert(id, elem_ty.clone());
                        id
                    });
                    let body_hir = {
                        // `for` 的 break 是语句（不产生表达式值），同上。
                        self.loop_ctxs.push(LoopBreakCtx {
                            label: label.clone(),
                            ty: Ty::Unit,
                            is_value: false,
                            value_seen: false,
                            plain_seen: false,
                        });
                        let b = self.check_block(body, &Ty::Unit)?;
                        self.loop_ctxs.pop();
                        b
                    };
                    self.pop_scope();

                    let src_local = self.alloc_local();
                    let idx_local = self.alloc_local();
                    self.locals.insert(src_local, iter_ty.clone());
                    self.infer.locals.insert(src_local, iter_ty.clone());
                    self.locals.insert(idx_local, Ty::Uint(64));
                    self.infer.locals.insert(idx_local, Ty::Uint(64));

                    let idx_ref = || HirExpr {
                        kind: ExprKind::Local(idx_local),
                        ty: Ty::Uint(64),
                        span,
                    };
                    let item_init = HirExpr {
                        kind: ExprKind::Index(
                            Box::new(HirExpr {
                                kind: ExprKind::Local(src_local),
                                ty: iter_ty.clone(),
                                span,
                            }),
                            Box::new(idx_ref()),
                        ),
                        ty: elem_ty.clone(),
                        span,
                    };

                    let mut inner_stmts = vec![HirStmt::Let {
                        local: var,
                        init: Some(Box::new(item_init)),
                    }];
                    inner_stmts.extend(body_hir.stmts);
                    if let Some(tail) = body_hir.tail {
                        inner_stmts.push(HirStmt::Expr(tail));
                    }
                    let inc = HirExpr {
                        kind: ExprKind::Assign(
                            Box::new(idx_ref()),
                            Box::new(HirExpr {
                                kind: ExprKind::Binary(
                                    hir::BinaryOp::Add,
                                    Box::new(idx_ref()),
                                    Box::new(HirExpr {
                                        kind: ExprKind::Literal(HirLiteral::Int(1)),
                                        ty: Ty::Uint(64),
                                        span,
                                    }),
                                ),
                                ty: Ty::Uint(64),
                                span,
                            }),
                        ),
                        ty: Ty::Unit,
                        span,
                    };
                    inner_stmts.push(HirStmt::Expr(Box::new(inc.clone())));
                    let mut loop_body = hir::HirBlock {
                        stmts: inner_stmts,
                        tail: None,
                    };
                    rewrite_for_continue(&mut loop_body, &inc, 0);
                    let cond = HirExpr {
                        kind: ExprKind::Binary(
                            hir::BinaryOp::Lt,
                            Box::new(idx_ref()),
                            Box::new(HirExpr {
                                kind: ExprKind::Literal(HirLiteral::Int(n)),
                                ty: Ty::Uint(64),
                                span,
                            }),
                        ),
                        ty: Ty::Bool,
                        span,
                    };
                    let while_expr = HirExpr {
                        kind: ExprKind::While(label.clone(), Box::new(cond), loop_body),
                        ty: Ty::Unit,
                        span,
                    };
                    return Ok(HirExpr {
                        kind: ExprKind::Block(hir::HirBlock {
                            stmts: vec![
                                HirStmt::Let {
                                    local: src_local,
                                    init: Some(Box::new(iter_hir)),
                                },
                                HirStmt::Var {
                                    local: idx_local,
                                    init: Some(Box::new(HirExpr {
                                        kind: ExprKind::Literal(HirLiteral::Int(0)),
                                        ty: Ty::Uint(64),
                                        span,
                                    })),
                                },
                                HirStmt::Expr(Box::new(while_expr)),
                            ],
                            tail: None,
                        }),
                        ty: Ty::Unit,
                        span,
                    });
                }

                let (start, end, inclusive) = match iter.as_ref() {
                    Expression::Binary(lhs, ast::BinaryOp::Range, rhs, _) => {
                        (lhs.as_ref(), rhs.as_ref(), false)
                    }
                    Expression::Binary(lhs, ast::BinaryOp::RangeInclusive, rhs, _) => {
                        (lhs.as_ref(), rhs.as_ref(), true)
                    }
                    _ => {
                        return Err(CompileError::type_error(
                            "only `for x in start..end` range loops are supported",
                        )
                        .with_span(span))
                    }
                };

                self.push_scope();
                // 先确定终点类型，这样 `for i in 0..n_u8` 里的起点字面量
                // 会跟着 n_u8 推断，而不是先默认成 i32 再报类型不匹配。
                let end_hir = self.check_expr(end, &Ty::Infer(InferId(0)))?;
                let elem_ty = self.infer.resolve(&end_hir.ty);
                if !matches!(elem_ty, Ty::Int(_) | Ty::Uint(_)) {
                    return Err(
                        CompileError::type_error("`for` range bounds must be integers")
                            .with_span(span),
                    );
                }
                let start_hir = self.check_expr(start, &elem_ty)?;
                self.infer
                    .unify(&start_hir.ty, &elem_ty, span)
                    .map_err(|e| self.errors.push(e))
                    .ok();
                let hir_pat = self.lower_pattern(pat, &elem_ty)?;
                let var = first_binding(&hir_pat).unwrap_or_else(|| {
                    let id = self.alloc_local();
                    self.locals.insert(id, elem_ty.clone());
                    self.infer.locals.insert(id, elem_ty.clone());
                    id
                });
                let body_hir = self.check_block(body, &Ty::Unit)?;
                self.pop_scope();

                let end_local = self.alloc_local();
                self.locals.insert(end_local, elem_ty.clone());
                self.infer.locals.insert(end_local, elem_ty.clone());

                let var_ref = || HirExpr {
                    kind: ExprKind::Local(var),
                    ty: elem_ty.clone(),
                    span,
                };
                let cond = HirExpr {
                    kind: ExprKind::Binary(
                        if inclusive {
                            hir::BinaryOp::Le
                        } else {
                            hir::BinaryOp::Lt
                        },
                        Box::new(var_ref()),
                        Box::new(HirExpr {
                            kind: ExprKind::Local(end_local),
                            ty: elem_ty.clone(),
                            span,
                        }),
                    ),
                    ty: Ty::Bool,
                    span,
                };
                let inc = HirExpr {
                    kind: ExprKind::Assign(
                        Box::new(var_ref()),
                        Box::new(HirExpr {
                            kind: ExprKind::Binary(
                                hir::BinaryOp::Add,
                                Box::new(var_ref()),
                                Box::new(HirExpr {
                                    kind: ExprKind::Literal(hir::HirLiteral::Int(1)),
                                    ty: elem_ty.clone(),
                                    span,
                                }),
                            ),
                            ty: elem_ty.clone(),
                            span,
                        }),
                    ),
                    ty: Ty::Unit,
                    span,
                };

                let mut loop_stmts = body_hir.stmts;
                if let Some(tail) = body_hir.tail {
                    loop_stmts.push(hir::HirStmt::Expr(tail));
                }
                loop_stmts.push(hir::HirStmt::Expr(Box::new(inc.clone())));
                let mut loop_body = hir::HirBlock {
                    stmts: loop_stmts,
                    tail: None,
                };
                // `continue` 必须仍然执行自增，否则循环不会前进。把循环体
                // 里（不跨越内层循环）的 continue 改写成“自增后 continue”。
                rewrite_for_continue(&mut loop_body, &inc, 0);
                let while_expr = HirExpr {
                    kind: ExprKind::While(label.clone(), Box::new(cond), loop_body),
                    ty: Ty::Unit,
                    span,
                };

                return Ok(HirExpr {
                    kind: ExprKind::Block(hir::HirBlock {
                        stmts: vec![
                            hir::HirStmt::Var {
                                local: var,
                                init: Some(Box::new(start_hir)),
                            },
                            hir::HirStmt::Let {
                                local: end_local,
                                init: Some(Box::new(end_hir)),
                            },
                            hir::HirStmt::Expr(Box::new(while_expr)),
                        ],
                        tail: None,
                    }),
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::Loop(body, _) => {
                let label = self.pending_loop_label.take();
                // `loop` 表达式的类型 = 所有带值 `break` 的统一类型（没有带值
                // break 就按"有没有 break"定 `()`/`Never`）。压入一个新鲜推断
                // 变量，`break v` 把 `v` 与它统一；检查完弹出来解析。
                let break_ty = self.infer.fresh_infer(span);
                self.loop_ctxs.push(LoopBreakCtx {
                    label: label.clone(),
                    ty: break_ty.clone(),
                    is_value: true,
                    value_seen: false,
                    plain_seen: false,
                });
                let b = self.check_block(body, &Ty::Unit)?;
                let ctx = self.loop_ctxs.pop().unwrap();
                let ty = if ctx.value_seen {
                    self.infer.resolve(&ctx.ty)
                } else if ctx.plain_seen {
                    Ty::Unit
                } else {
                    // 循环体没有 `break`：死循环，发散。
                    Ty::Never
                };
                return Ok(HirExpr {
                    kind: ExprKind::Loop(label, b),
                    ty,
                    span,
                });
            }
            Expression::Break(label, value, _) => {
                // 带值 break：把值与目标循环（最内层，或按标签匹配）的 break
                // 类型统一。目标必须是 `loop`（`is_value`）；`while`/`for` 的
                // break 是语句，值只检查后丢弃（Rust 里 `while`/`for` 不能带值
                // break）。值**保留**在 HIR 里供 MIR 写入结果槽——早先只检查
                // 后丢弃，`let x = loop { break 7; }` 静默得 0。
                let hir_val = if let Some(val) = value {
                    let ctx_idx = find_loop_ctx(&self.loop_ctxs, label.as_deref());
                    match ctx_idx {
                        Some(idx) if self.loop_ctxs[idx].is_value => {
                            let ty = self.loop_ctxs[idx].ty.clone();
                            let hv = self.check_expr(val, &ty)?;
                            self.loop_ctxs[idx].value_seen = true;
                            Some(Box::new(hv))
                        }
                        _ => {
                            let _ = self.check_expr(val, &Ty::Infer(InferId(0)))?;
                            None
                        }
                    }
                } else {
                    if let Some(idx) = find_loop_ctx(&self.loop_ctxs, label.as_deref()) {
                        self.loop_ctxs[idx].plain_seen = true;
                    }
                    None
                };
                return Ok(HirExpr {
                    kind: ExprKind::Break(label.clone(), hir_val),
                    ty: Ty::Never,
                    span,
                });
            }
            Expression::Continue(label, _) => {
                return Ok(HirExpr {
                    kind: ExprKind::Continue(label.clone()),
                    ty: Ty::Never,
                    span,
                });
            }
            Expression::Return(value, _) => {
                let ret_ty = self.infer.return_ty.clone().unwrap_or(Ty::Unit);
                let v = if let Some(val) = value {
                    let e = self.check_expr(val, &ret_ty)?;
                    self.check_partial_move(&e, val.span())?;
                    self.infer.unify(&e.ty, &ret_ty, span).ok();
                    Some(Box::new(e))
                } else {
                    None
                };
                return Ok(HirExpr {
                    kind: ExprKind::Return(v),
                    ty: Ty::Never,
                    span,
                });
            }
            Expression::Defer(e, _) => {
                // `defer` 只允许包住一个块（与 Zig 一致）：退出作用域时按声明
                // 顺序入栈、逆序执行。这里把块检查成 `()`，块本身的值无关。
                match e.as_ref() {
                    Expression::Block(b, _) => {
                        let block = self.check_block(b, &Ty::Unit)?;
                        return Ok(HirExpr {
                            kind: ExprKind::Defer(block),
                            ty: Ty::Unit,
                            span,
                        });
                    }
                    _ => {
                        return Err(CompileError::type_error(
                            "`defer` must be followed by a block `{ ... }`",
                        )
                        .with_span(span));
                    }
                }
            }
            Expression::Try(_e, _pat, _catch, _) => {
                // try/catch 是 Zig 风格的异常语法。当前编译器没有异常表与
                // 栈展开机制，无法在 MIR/SSA 阶段生成正确的跳转，所以这里
                // 明确报错，而不是偷偷把 `catch` 编译成永远不会进的死代码。
                return Err(CompileError::type_error(
                    "`try` / `catch` is not implemented yet: the compiler has no exception tables or stack unwinding"
                        .to_string(),
                )
                .with_span(span));
            }
            Expression::Throw(_e, _) => {
                return Err(CompileError::type_error(
                    "`throw` is not implemented yet: the compiler has no exception tables or stack unwinding",
                )
                .with_span(span));
            }
            Expression::Array(elems, repeat, _) => {
                let elem = self.infer.fresh_infer(span);
                // 声明类型（`let b: [u8; 4] = [1, 2, ..]`）先定下元素类型，
                // 否则字面量的默认类型（i32）会顶掉它。**切片**也要定下元素
                // 类型：`S { arr: [10, 20, 30] }`（`arr: []const i64`）早先
                // 元素默认成 i32，`[i32; 3]` 又被字段初始化静默强转成
                // `[]const i64`——切片指向 i32 数据却被当 i64 读，运行期
                // 读越界（`/tmp/sl_bug.zr` 直接 SIGSEGV）。
                match self.infer.resolve(expected) {
                    Ty::Array(decl_elem, _) | Ty::Slice(decl_elem, _) => {
                        let _ = self.infer.unify(&elem, &decl_elem, span);
                    }
                    _ => {}
                }
                let mut hir_elems = Vec::new();
                if let Some(rep) = repeat {
                    let n = match &**rep {
                        Expression::Literal(AstLiteral::Int(n, _), _) => *n,
                        _ => {
                            return Err(CompileError::type_error(
                                "array repeat length must be an integer literal".to_string(),
                            ))
                        }
                    };
                    if !(0..=1_000_000).contains(&n) {
                        return Err(CompileError::type_error(
                            "array repeat length out of supported range".to_string(),
                        ));
                    }
                    let first_expr = elems.first().ok_or_else(|| {
                        CompileError::type_error("array repeat needs an element".to_string())
                    })?;
                    let first = self.check_expr(first_expr, &elem)?;
                    self.check_partial_move(&first, first_expr.span())?;
                    for _ in 0..n {
                        hir_elems.push(first.clone());
                    }
                } else if self.infer.resolve(&elem) != Ty::Infer(InferId(0)) {
                    // 元素类型已由声明定下：直接按它检查。
                    let elem_ty = self.infer.resolve(&elem);
                    for e in elems {
                        let h = self.check_expr(e, &elem_ty)?;
                        self.check_partial_move(&h, e.span())?;
                        hir_elems.push(h);
                    }
                } else if let Some(first) = elems.first() {
                    // 第一个元素定下元素类型：先检查它，再把推断变量写回
                    // 去的类型。剩下的元素按已定的类型检查——字面量会跟着收窄
                    // （`[f32; 4] = [1.5, ..]` 里的 1.5 默认是 f64）。
                    let h = self.check_expr(first, &elem)?;
                    self.check_partial_move(&h, first.span())?;
                    let _ = self.infer.unify(&elem, &h.ty, span);
                    hir_elems.push(h);
                    let elem_ty = self.infer.resolve(&elem);
                    for e in &elems[1..] {
                        let h = self.check_expr(e, &elem_ty)?;
                        self.check_partial_move(&h, e.span())?;
                        hir_elems.push(h);
                    }
                }
                let len = hir_elems.len() as i128;
                // 元素类型要落到具体类型：字面量的默认类型（i32）不会自动写回
                // 推断变量。元素类型停在 Infer 时，MIR 按 1 字节算元素大小
                // ——数组的元素会挤在一起，整个数组的布局就错了。
                if let Some(first_ty) = hir_elems.first().map(|e| e.ty.clone()) {
                    let _ = self.infer.unify(&elem, &first_ty, span);
                }
                let arr_ty = Ty::Array(Box::new(elem), const_array_len(len, span));
                // 数组字面量自身没有别的地方会把它和标注类型统一，这里必须
                // 主动 unify：否则 `let a: [i32; 4] = [...]` 里的元素类型
                // 会停在 Infer，MIR 按 64 位而不是 32 位存元素。
                let _ = self.infer.unify(&arr_ty, expected, span);
                let ty = self.infer.resolve(&arr_ty);
                let resolved_elem = match &ty {
                    Ty::Array(elem, _) => Some(self.infer.resolve(elem)),
                    _ => None,
                };
                // 元素类型也要解出来：`resolve` 只解一层，数组的元素类型
                // 停在 Infer 时，MIR 按 1 字节算元素大小——数组的元素会
                // 挤在一起，整个数组的布局就错了。
                let ty = match &ty {
                    Ty::Array(elem, len) => {
                        Ty::Array(Box::new(self.infer.resolve(elem)), len.clone())
                    }
                    other => other.clone(),
                };
                for e in &mut hir_elems {
                    e.ty = self.infer.resolve(&e.ty);
                    // 数值字面量要随数组标注类型收窄：`[f32; 4] = [1.5, ..]`
                    // 里的字面量默认是 f64，不收窄就会按 64 位写进 4 字节槽。
                    if let (
                        Some(elem),
                        ExprKind::Literal(HirLiteral::Int(_) | HirLiteral::Float(_)),
                    ) = (&resolved_elem, &e.kind)
                    {
                        if !matches!(elem, Ty::Infer(_)) {
                            e.ty = elem.clone();
                        }
                    }
                }
                return Ok(HirExpr {
                    kind: ExprKind::Array(hir_elems),
                    ty,
                    span,
                });
            }
            Expression::Tuple(elems, _) => {
                let mut tys = Vec::new();
                let mut hir_elems = Vec::with_capacity(elems.len());
                // 期望类型是元组时，按对应元素类型检查每个元素——否则元组里的
                // 整数字面量（`(1, 5)` 在 `(i32, i64)` 位置）会各自落成默认的
                // `i32`，存进 i64 槽只写低 4 字节，`t.1` 读到"值 | 高位垃圾"。
                // 期望类型还是推断变量（未知）时退回各自独立推断（原行为）。
                let exp_tys: Option<Vec<Ty>> = match self.infer.resolve(expected) {
                    Ty::Tuple(ts) => Some(ts),
                    _ => None,
                };
                for (i, e) in elems.iter().enumerate() {
                    let elem_exp = exp_tys
                        .as_ref()
                        .and_then(|ts| ts.get(i).cloned())
                        .unwrap_or_else(|| Ty::Infer(InferId(0)));
                    let h = self.check_expr(e, &elem_exp)?;
                    tys.push(h.ty.clone());
                    hir_elems.push(h);
                }
                return Ok(HirExpr {
                    kind: ExprKind::Tuple(hir_elems),
                    ty: Ty::Tuple(tys),
                    span,
                });
            }
            Expression::StructInit(path, fields, base, _) => {
                return self.check_struct_init(path, fields, base.as_deref(), expected, span);
            }
            Expression::Lambda(params, ret, body, _) => {
                // 非"立即调用"位置的闭包允许提升：无捕获时变成顶层函数指针。
                return self.check_lambda(params, ret.as_ref(), body, expected, span, true);
            }
            Expression::Comptime(e, _) => {
                // comptime 块：表达式必须在编译期可求值
                let inner = self.check_expr(e, expected)?;
                // 求值结果必须带进 HIR：丢掉的话 MIR 侧只能拿一个占位值，
                // `comptime 40 + 2` 会在运行期变成 Unit。
                let folded = self.eval_const_expr(e)?;
                // 类型优先沿用内层表达式推断出来的结果（例如 `comptime ~0`
                // 应当是 i32）。类型检查器还没有真正的推断上下文，`Ty::Infer`
                // 占位不会被解析掉，直接透传会让 `comptime { 40 + 2 } != 42`
                // 报 "found `_`"，所以落回按折叠出的字面量反推。
                let ty = match &inner.ty {
                    Ty::Infer(_) => const_literal_ty(&folded),
                    concrete => concrete.clone(),
                };
                return Ok(HirExpr {
                    kind: ExprKind::Comptime(folded),
                    ty,
                    span,
                });
            }
            Expression::Await(e, _) => {
                if !self.current_is_async {
                    return Err(CompileError::type_error(
                        "`.await` 只能出现在 `async fn` 内部".to_string(),
                    ));
                }
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                // `await` 的结果类型就是 Future 的 `Output`。异步函数签名
                // 已经把返回类型换成了不透明 Future，这里通过映射找回它。
                let ty = match &inner.ty {
                    Ty::Opaque(id) => self
                        .async_outputs
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| expected.clone()),
                    // `impl Future` 类型的 Output 可能没有登记（例如返回不同
                    // 异步函数分支的动态来源）；给调用方一个用期望类型补全
                    // 的机会，`let y: i32 = f.await` 因此可以确定结果宽度。
                    _ => expected.clone(),
                };
                return Ok(HirExpr {
                    kind: ExprKind::Await(Box::new(inner)),
                    ty,
                    span,
                });
            }
            Expression::Asm(asm, _) => {
                // 操作数逐个降级到 HIR，并保留寄存器约束。
                let mut outputs = Vec::with_capacity(asm.outputs.len());
                for o in &asm.outputs {
                    let e = self.check_expr(&o.expr, &Ty::Infer(InferId(0)))?;
                    outputs.push((o.constraint.clone(), e));
                }
                let mut inputs = Vec::with_capacity(asm.inputs.len());
                for i in &asm.inputs {
                    let e = self.check_expr(&i.expr, &Ty::Infer(InferId(0)))?;
                    inputs.push((i.constraint.clone(), e));
                }
                return Ok(HirExpr {
                    kind: ExprKind::Asm {
                        template: asm.template.clone(),
                        outputs,
                        inputs,
                    },
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::Yield(e, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                let _ = inner;
                return Ok(HirExpr {
                    kind: ExprKind::Yield,
                    ty: Ty::Infer(InferId(0)),
                    span,
                });
            }
            Expression::Spawn(e, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                if !matches!(inner.ty, Ty::Opaque(_)) {
                    return Err(CompileError::type_error(
                        "`spawn` expects a future from an `async fn` or `-> impl Future`",
                    ));
                }
                return Ok(HirExpr {
                    kind: ExprKind::Spawn(Box::new(inner)),
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::Labeled(label, e, _) => {
                // 标签只对循环有意义：`outer: loop { break outer; }`。
                // 先把标签挂到待绑定槽，紧接着检查的内层 `while` / `loop`
                // / `for` 会取走它。非循环上的标签（块标签）目前忽略，
                // 但必须清空槽位以免泄漏到后面无关的循环上。
                let is_loop = matches!(
                    **e,
                    Expression::While(..) | Expression::Loop(..) | Expression::For(..)
                );
                if is_loop {
                    self.pending_loop_label = Some(label.clone());
                }
                let inner = self.check_expr(e, expected)?;
                self.pending_loop_label = None;
                self.infer.record_borrow();
                return Ok(HirExpr {
                    kind: inner.kind,
                    ty: inner.ty,
                    span,
                });
            }
            Expression::Goto(label, _) => {
                return Err(CompileError::internal(format!(
                    "`goto` to label '{}' requires basic block construction (not yet implemented)",
                    label
                ))
                .with_span(span));
            }
            Expression::Unsafe(inner, _) => {
                // `unsafe { ... }`：目前 zr 没有借用检查，`unsafe` 是显式
                // 边界标记，不改变语义——按块表达式透传（块的值即 unsafe
                // 块的值）。将来有了借用检查，原始指针解引用等危险操作会被
                // 要求包在 `unsafe` 里，这里只透传不降级。
                return self.check_expr(inner, expected);
            }
            Expression::IfLet(pat, expr, then_b, else_b, span) => {
                // `if let pat = expr { then } else { else }` 直接降成
                // `if <pat 匹配> { then } else { else }`。
                //
                // 不走 match 那条路：match 的脱糖把"绑定臂"当**无条件**臂
                // （`v => ...` 必然匹配），于是 `if let` 脱糖出来的
                // `match e { v => then, _ => else }` 里，`_` 臂先占住链尾、
                // `v` 臂再把链尾覆盖掉——else 分支整个丢了。
                let scrutinee = self.check_expr(expr, &Ty::Infer(InferId(0)))?;
                // 模式里的绑定名在 then 分支可见。
                self.push_scope();
                let hir_pat = self.lower_pattern(pat, &scrutinee.ty)?;
                let then_hir = self.check_block(then_b, expected)?;
                self.pop_scope();
                // 模式的条件与绑定替换。绑定臂的绑定没有独立存储：把 then
                // 分支里对它的引用整体换成取值表达式（与 match 的绑定臂同一
                // 套降级）。结构体模式的绑定落在字段上（`P { x: v }` 的 v
                // 是 `p.x`）。
                let (pat_cond, binds) = self.pat_match(&scrutinee, &hir_pat, *span)?;
                let mut then_hir = then_hir;
                if !binds.is_empty() {
                    // 替换要走整块——`return y` 这类引用在语句里，不在尾表达式。
                    let mut wrapper = HirExpr {
                        kind: ExprKind::Block(then_hir),
                        ty: Ty::Unit,
                        span: *span,
                    };
                    for (local, value) in &binds {
                        substitute_local(&mut wrapper, *local, value);
                    }
                    then_hir = match wrapper.kind {
                        ExprKind::Block(b) => b,
                        _ => unreachable!("wrapper 保持是块表达式"),
                    };
                }
                let else_hir: Option<hir::HirBlock> = if let Some(e) = else_b {
                    let e_expr = if let Expression::Block(b, _) = e.as_ref() {
                        self.check_block(b, expected)?
                    } else {
                        let checked = self.check_expr(e, expected)?;
                        hir::HirBlock {
                            stmts: Vec::new(),
                            tail: Some(Box::new(checked)),
                        }
                    };
                    Some(e_expr)
                } else {
                    // 没有 else：不匹配就什么也不做（值恒为 `()`）。
                    Some(hir::HirBlock {
                        stmts: Vec::new(),
                        tail: None,
                    })
                };
                // 条件：绑定 / 通配模式不可反驳，恒匹配；字面量模式比较相等；
                // 结构体模式是各字段条件之合。
                let cond = pat_cond.unwrap_or_else(|| bool_lit(true, *span));
                // 发散按 **AST 形状** 判（`then_b` 是 `Block`，`else_b` 是
                // `Expression`，与 `expr_diverges` 里的区分一致）。不能用
                // `then_hir.tail.is_none()`：绑定替换把 `then_hir` 包进 wrapper
                // 再解包时，发散块的 `tail` 会丢，嵌套 `if let` 的 then 分支
                // 于是被误判成类型 `()`，报 `expected i32, found ()`。
                let then_diverges = block_diverges(then_b);
                let else_diverges = else_b.as_ref().is_some_and(|e| expr_diverges(e));
                let ty = {
                    let t = if then_diverges {
                        Ty::Never
                    } else {
                        then_hir
                            .tail
                            .as_ref()
                            .map(|t| t.ty.clone())
                            .unwrap_or(Ty::Unit)
                    };
                    let e = match else_hir.as_ref() {
                        None => Ty::Unit,
                        Some(_) if else_diverges => Ty::Never,
                        Some(el) => el
                            .tail
                            .as_ref()
                            .map(|t| t.ty.clone())
                            .unwrap_or(Ty::Unit),
                    };
                    self.unify_or(&t, &e)
                };
                let ty = if then_diverges && else_diverges {
                    Ty::Never
                } else {
                    ty
                };
                return Ok(HirExpr {
                    kind: ExprKind::If(Box::new(cond), then_hir, else_hir),
                    ty,
                    span: *span,
                });
            }
            Expression::Intrinsic(kind, types, operands, _) => {
                let hir_types: Result<Vec<Ty>> =
                    types.iter().map(|t| self.ast_to_hir_type(t)).collect();
                let mut hir_types = hir_types?;
                let _ = self.lower_intrinsic(*kind);
                // 表达式实参也要检查（`intrinsic!(drop x)` 里的 `x`）
                for op in operands {
                    let expected = Ty::Infer(InferId(0));
                    let _ = self.check_expr(op, &expected)?;
                }
                let ty = match kind {
                    ast::IntrinsicKind::SizeOf | ast::IntrinsicKind::AlignOf => Ty::Uint(64),
                    // `transmute src -> Dst` 的结果类型就是 Dst
                    ast::IntrinsicKind::Transmute => {
                        hir_types.pop().unwrap_or(Ty::Infer(InferId(0)))
                    }
                    ast::IntrinsicKind::TypeOf => Ty::Infer(InferId(0)),
                    _ => Ty::Infer(InferId(0)),
                };
                return Ok(HirExpr {
                    kind: ExprKind::Intrinsic,
                    ty,
                    span,
                });
            }
        };
        // 走到这里的表达式大多已经自己算好了类型；只有引用（局部变量 /
        // 全局定义）需要在这里补上——之前统一发一个全新的推断变量，导致
        // `fn f(n: i64) -> i64 { n }` 里的 `n` 退化成 `_`。
        let ty = match &kind {
            // 局部变量的类型要走一遍推断替换：`let x = f();` 把 `x` 记成
            // 一个推断变量，真正的类型在 `let` 处才 unify 进去。不 resolve
            // 的话读 `x` 永远是 `_`，后续 `x * 100.0` 里的字面量会回落到
            // f64 默认值，与 x 的实际位宽对不上。
            ExprKind::Local(local) => {
                // 闭包捕获记录：若当前处在闭包检查中，且这个 local 是闭包
                // 之外的局部变量（在最内层闭包的外层集合里），就把它记为捕获。
                // 闭包形参与体内新建的 local 不在外层集合里，不会误记。
                if let Some(outer) = self.closure_ctx.last() {
                    if outer.contains(local) {
                        let ty = self
                            .locals
                            .get(local)
                            .cloned()
                            .unwrap_or(Ty::Infer(InferId(0)));
                        let ty = self.infer.resolve(&ty);
                        if let Some(cap) = self.closure_captures.last_mut() {
                            cap.push((*local, ty));
                        }
                    }
                }
                let t = self
                    .locals
                    .get(local)
                    .cloned()
                    .unwrap_or(Ty::Infer(InferId(0)));
                self.infer.resolve(&t)
            }
            ExprKind::Def(id) => self.def_type(*id).unwrap_or(Ty::Infer(InferId(0))),
            _ => self.infer.fresh_infer(span),
        };
        Ok(HirExpr { kind, ty, span })
    }

    /// 全局定义（函数 / 常量 / 静态 / 类型别名）的类型。
    fn def_type(&self, id: DefId) -> Option<Ty> {
        // 函数名的类型是**函数类型**而不是返回类型——否则
        // `add(1, 2)` 会报 "cannot call non-function type `i32`"。
        if let Some(f) = self.functions.get(&id) {
            return Some(Ty::Function {
                params: f.params.iter().map(|p| p.ty.clone()).collect(),
                ret: Box::new(f.return_type.clone()),
                is_async: f.is_async,
                abi: hir::Abi::Zr,
            });
        }
        if let Some(c) = self.consts.get(&id) {
            return Some(c.ty.clone());
        }
        if let Some(s) = self.statics.get(&id) {
            return Some(s.ty.clone());
        }
        None
    }

    // ==================================================================
    // 二元 / 调用 / 方法调用
    // ==================================================================

    fn check_binary(
        &mut self,
        lhs: &Expression,
        op: &ast::BinaryOp,
        rhs: &Expression,
        _expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let bin_op = self.ast_binary_op(*op);
        let l_ty = self.infer_expr_ty(lhs)?;
        let l = self.check_expr(lhs, &l_ty)?;

        // 指针算术：`p + n` / `n + p` / `p - n` 结果还是指针，`p - q` 结果是
        // 元素个数（isize）。必须在这里拦下——否则会走下面的整数统一逻辑，
        // `n + p` 甚至会被期望类型挡成类型错误。
        if matches!(op, ast::BinaryOp::Add | ast::BinaryOp::Sub) {
            let l_ptr = matches!(
                self.infer.resolve(&l.ty),
                Ty::Pointer(..) | Ty::Reference(..)
            );
            let r_hint = self.infer_expr_ty(rhs)?;
            let r_ptr = matches!(
                self.infer.resolve(&r_hint),
                Ty::Pointer(..) | Ty::Reference(..)
            );
            match (op, l_ptr, r_ptr) {
                // 指针相减：结果是以元素为单位的差值。
                (ast::BinaryOp::Sub, true, true) => {
                    let r = self.check_expr(rhs, &l.ty)?;
                    self.infer.unify(&l.ty, &r.ty, span).ok();
                    return Ok(HirExpr {
                        kind: ExprKind::Binary(bin_op, Box::new(l), Box::new(r)),
                        ty: Ty::Int(64),
                        span,
                    });
                }
                // `p + n` / `p - n`：右操作数是整数偏移，结果仍是指针。
                (ast::BinaryOp::Add | ast::BinaryOp::Sub, true, false) => {
                    let r = self.check_expr(rhs, &Ty::Int(64))?;
                    let ty = l.ty.clone();
                    return Ok(HirExpr {
                        kind: ExprKind::Binary(bin_op, Box::new(l), Box::new(r)),
                        ty,
                        span,
                    });
                }
                // `n + p`：加法可交换，结果仍是指针。
                (ast::BinaryOp::Add, false, true) => {
                    let r = self.check_expr(rhs, &r_hint)?;
                    let ty = r.ty.clone();
                    return Ok(HirExpr {
                        kind: ExprKind::Binary(bin_op, Box::new(l), Box::new(r)),
                        ty,
                        span,
                    });
                }
                // 两个指针相加没有意义。
                (ast::BinaryOp::Add, true, true) => {
                    return Err(CompileError::type_error("cannot add two pointers").with_span(span));
                }
                _ => {}
            }
        }

        // 右操作数用左操作数**已解出**的类型当期望：`x_f32 * 100.0` 里
        // 100.0 默认推断成 f64，与 x 的 f32 混算会按错位宽读写。传 `l.ty`
        // 让字面量跟着左操作数走（与 `let y: f32 = 1.5` 是同一条规则）。
        let r_ty = if matches!(l.ty, Ty::Infer(_)) {
            self.infer_expr_ty(rhs)?
        } else {
            l.ty.clone()
        };
        let r = self.check_expr(rhs, &r_ty)?;
        self.infer.unify(&l.ty, &r.ty, span).ok();
        // 聚合类型（结构体 / 枚举）的比较走 trait 方法，不走 MIR 的标量比较。
        //
        // 聚合值在运行期是**地址**：早先这里直接产出 `Binary(Eq)`，后端拿两个
        // 地址当标量比，于是内容相同的两个值也恒不相等（`a == b` 恒假，
        // `a != b` 恒真，编译期一声不响）。`#[derive(PartialEq/Eq)]` 现在会
        // 生成 `Type__eq`，`#[derive(PartialOrd/Ord)]` 生成 `lt/le/gt/ge`，
        // 这里改写过去即可。
        //
        // 没实现对应方法就明确报错——让 `==` 退回标量比较等于把"静默算错"
        // 留在原地。
        if matches!(
            op,
            ast::BinaryOp::Eq
                | ast::BinaryOp::Ne
                | ast::BinaryOp::Lt
                | ast::BinaryOp::Le
                | ast::BinaryOp::Gt
                | ast::BinaryOp::Ge
        ) {
            let cmp_ty = self.infer.resolve(&l.ty);
            if matches!(cmp_ty, Ty::Named(..)) {
                let (method, negate) = match op {
                    ast::BinaryOp::Eq => ("eq", false),
                    ast::BinaryOp::Ne => ("eq", true),
                    ast::BinaryOp::Lt => ("lt", false),
                    ast::BinaryOp::Le => ("le", false),
                    ast::BinaryOp::Gt => ("gt", false),
                    _ => ("ge", false),
                };
                let Some(id) = self.lookup_method(&cmp_ty, method) else {
                    let what = if method == "eq" {
                        "`PartialEq`"
                    } else {
                        "`Ord`"
                    };
                    return Err(CompileError::type_error(format!(
                        "cannot compare values of type `{}`: it does not derive or implement {what}",
                        self.infer.type_name(&cmp_ty)
                    ))
                    .with_span(span));
                };
                let call = HirExpr {
                    kind: ExprKind::Call {
                        callee: id,
                        args: vec![l, r],
                        result: self.alloc_local(),
                    },
                    ty: Ty::Bool,
                    span,
                };
                return Ok(if negate {
                    HirExpr {
                        kind: ExprKind::Unary(hir::UnaryOp::Not, Box::new(call)),
                        ty: Ty::Bool,
                        span,
                    }
                } else {
                    call
                });
            }
        }
        let ty = match op {
            ast::BinaryOp::Add
            | ast::BinaryOp::Sub
            | ast::BinaryOp::Mul
            | ast::BinaryOp::Div
            | ast::BinaryOp::Rem
            | ast::BinaryOp::BitAnd
            | ast::BinaryOp::BitOr
            | ast::BinaryOp::BitXor
            | ast::BinaryOp::Shl
            | ast::BinaryOp::Shr => l.ty.clone(),
            ast::BinaryOp::Eq
            | ast::BinaryOp::Ne
            | ast::BinaryOp::Lt
            | ast::BinaryOp::Gt
            | ast::BinaryOp::Le
            | ast::BinaryOp::Ge => Ty::Bool,
            ast::BinaryOp::And | ast::BinaryOp::Or => Ty::Bool,
            ast::BinaryOp::Range | ast::BinaryOp::RangeInclusive => {
                Ty::Reference(Box::new(l.ty.clone()), hir::Mutability::Immutable)
            }
        };
        Ok(HirExpr {
            kind: ExprKind::Binary(bin_op, Box::new(l), Box::new(r)),
            ty,
            span,
        })
    }

    /// 若 `callee` 是元组结构体类型名，返回其 DefId。
    fn tuple_struct_target(&self, callee: &Expression) -> Option<DefId> {
        let id = match callee {
            Expression::Variable(name, _) => self.symbols.get(name).copied()?,
            Expression::Path(path, _) if path.len() == 1 => self.symbols.get(&path[0]).copied()?,
            // `Wrap::<i32>(7)`：turbofish 外面还包了一层
            Expression::Turbofish(inner, _, _) => return self.tuple_struct_target(inner),
            _ => return None,
        };
        let hir::HirTypeDef::Struct(s) = self.type_defs.get(&id)? else {
            return None;
        };
        if s.fields
            .iter()
            .enumerate()
            .all(|(i, f)| f.name == i.to_string())
        {
            Some(id)
        } else {
            None
        }
    }

    /// 路径指向枚举变体时返回 (枚举的 DefId, 变体下标)。
    ///
    /// 路径的最后一段是变体名，前面的是枚举的类型路径。
    fn enum_variant_target_path(
        &self,
        path: &[String],
        span: ast::Span,
    ) -> Option<(DefId, usize)> {
        if path.len() < 2 {
            return None;
        }
        let def_id = self.resolve_path(&path[..path.len() - 1], span).ok()?;
        let hir::HirTypeDef::Enum(he) = self.type_defs.get(&def_id)? else {
            return None;
        };
        let name = path.last()?;
        let index = he.variants.iter().position(|v| &v.name == name)?;
        Some((def_id, index))
    }

    /// 调用目标是枚举变体时返回 (枚举的 DefId, 变体下标)。
    fn enum_variant_target(&self, callee: &Expression) -> Option<(DefId, usize)> {
        // `Option::<i32>::None` / `Option::<i32>::Some(7)`：turbofish
        // 外面还包了一层，看到它之后继续往里看。
        if let ast::Expression::Turbofish(inner, _, _) = callee {
            return self.enum_variant_target(inner);
        }
        match callee {
            ast::Expression::Path(path, span) => {
                self.enum_variant_target_path(path, *span)
            }
            // 通过 `use core::Option::Some` 导入的裸变体名（`Some(7)`）：
            // 变量名经 `variant_aliases` 指向宿主枚举定义（登记见
            // `resolve_use_tree`）。按 (枚举 DefId, 变体名) 查变体下标。
            ast::Expression::Variable(name, _span) => {
                let alias_id = self.variant_aliases.get(name).copied()?;
                let hir::HirTypeDef::Enum(he) = self.type_defs.get(&alias_id)? else {
                    return None;
                };
                let index = he.variants.iter().position(|v| &v.name == name)?;
                Some((alias_id, index))
            }
            _ => None,
        }
    }

    /// 显式类型实参（turbofish）当成期望类型：`Option::<i32>::Some(7)` 的
    /// `i32` 必须喂给构造器，否则 `T` 只能靠上下文推——`let a = ...` 这种
    /// 没有上下文的地方就推不出来。
    fn turbofish_expected(&mut self, callee: &Expression, def_id: DefId) -> Result<Option<Ty>> {
        let ast::Expression::Turbofish(_, targs, _) = callee else {
            return Ok(None);
        };
        let tys = self.turbofish_targs(targs)?;
        Ok(Some(Ty::Named(def_id, tys)))
    }

    /// 把 turbofish 里的 AST 类型实参转成 HIR 类型。
    fn turbofish_targs(&mut self, targs: &[AstType]) -> Result<Vec<Ty>> {
        let mut tys = Vec::with_capacity(targs.len());
        for t in targs {
            tys.push(self.ast_to_hir_type(t)?);
        }
        Ok(tys)
    }

    /// 枚举变体的载荷字段类型（按声明顺序）。
    fn variant_field_types(&self, def_id: DefId, index: usize) -> Option<Vec<Ty>> {
        let hir::HirTypeDef::Enum(he) = self.type_defs.get(&def_id)? else {
            return None;
        };
        let v = he.variants.get(index)?;
        Some(match &v.payload {
            hir::HirVariantPayload::Unit => Vec::new(),
            hir::HirVariantPayload::Tuple(ts) => ts.clone(),
            hir::HirVariantPayload::Struct(fields) => {
                fields.iter().map(|f| f.ty.clone()).collect()
            }
        })
    }

    /// 枚举变体构造：`Shape::Circle(5)`。
    fn check_enum_variant_init(
        &mut self,
        def_id: DefId,
        index: usize,
        args: &[ast::Argument],
        expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let raw_tys = self.variant_field_types(def_id, index).ok_or_else(|| {
            CompileError::internal("enum variant missing type definition").with_span(span)
        })?;
        if args.len() != raw_tys.len() {
            return Err(CompileError::type_error(format!(
                "enum variant constructor expects {} arguments, found {}",
                raw_tys.len(),
                args.len()
            ))
            .with_span(span));
        }
        let (targs, hir_args) =
            self.check_ctor_args(def_id, &raw_tys, args, expected, span)?;
        Ok(HirExpr {
            kind: ExprKind::EnumVariant {
                def_id,
                index,
                args: hir_args,
            },
            ty: Ty::Named(def_id, targs),
            span,
        })
    }

    /// 构造器实参检查：先定下泛型实参，再按替换后的字段类型检查每个实参。
    fn check_ctor_args(
        &mut self,
        def_id: DefId,
        raw_tys: &[Ty],
        args: &[ast::Argument],
        expected: &Ty,
        span: ast::Span,
    ) -> Result<(Vec<Ty>, Vec<HirExpr>)> {
        let provided: Vec<Ty> = match self.infer.resolve(expected) {
            Ty::Named(id, a) if id == def_id => a,
            _ => Vec::new(),
        };
        let mut checked = Vec::with_capacity(args.len());
        let mut actuals = Vec::with_capacity(args.len());
        for (arg, ty) in args.iter().zip(raw_tys) {
            let ast::Argument::Pos(expr) = arg else {
                return Err(CompileError::type_error(
                    "constructor does not accept named arguments",
                )
                .with_span(span));
            };
            let aty = if provided.is_empty() {
                self.infer.fresh_infer(span)
            } else {
                ty.clone()
            };
            let h = self.check_expr(expr, &aty)?;
            self.check_partial_move(&h, expr.span())?;
            actuals.push(h.ty.clone());
            checked.push(h);
        }
        let targs = self.collect_type_args(def_id, &provided, raw_tys, &actuals, span)?;
        if provided.is_empty() {
            // 未标注时第一遍是按推断变量检查的，现在有了具体实参，重新
            // 按替换后的类型检查一遍（数组→切片之类的强转需要它）。
            let mut rechecked = Vec::with_capacity(args.len());
            for (arg, ty) in args.iter().zip(raw_tys) {
                let ast::Argument::Pos(expr) = arg else {
                    continue;
                };
                let want = self.subst_def_ty(def_id, &targs, ty);
                let h = self.check_expr(expr, &want)?;
                rechecked.push(h);
            }
            if rechecked.len() == checked.len() {
                checked = rechecked;
            }
        }
        Ok((targs, checked))
    }

    fn check_tuple_struct_init(
        &mut self,
        type_id: DefId,
        args: &[ast::Argument],
        expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let hir::HirTypeDef::Struct(s) = self
            .type_defs
            .get(&type_id)
            .cloned()
            .ok_or_else(|| CompileError::internal("tuple struct missing type definition"))?
        else {
            return Err(CompileError::internal("tuple struct is not a struct"));
        };
        if args.len() != s.fields.len() {
            return Err(CompileError::type_error(format!(
                "tuple struct constructor expects {} arguments, found {}",
                s.fields.len(),
                args.len()
            ))
            .with_span(span));
        }
        let raw_tys: Vec<Ty> = s.fields.iter().map(|f| f.ty.clone()).collect();
        let (targs, checked) = self.check_ctor_args(type_id, &raw_tys, args, expected, span)?;
        let fields: Vec<(String, HirExpr)> = s
            .fields
            .iter()
            .zip(checked)
            .map(|(f, arg)| (f.name.clone(), arg))
            .collect();
        Ok(HirExpr {
            kind: ExprKind::Struct(fields),
            ty: Ty::Named(type_id, targs),
            span,
        })
    }

    /// 枚举变体的结构体载荷模式：`Shape::Circle { r: 3 }`。
    fn lower_enum_struct_variant_pattern(
        &mut self,
        enum_id: DefId,
        path: &[String],
        fields: &[ast::FieldPat],
        rest: bool,
        scrut_ty: &Ty,
        span: ast::Span,
    ) -> Result<hir::HirPattern> {
        let variant_name = path.last().cloned().unwrap_or_default();
        let index = self.variant_index(enum_id, &variant_name);
        let Some(hir::HirTypeDef::Enum(he)) = self.type_defs.get(&enum_id).cloned() else {
            return Err(
                CompileError::internal("enum missing type definition").with_span(span)
            );
        };
        let Some(v) = he.variants.get(index) else {
            return Err(CompileError::name_error(format!(
                "no variant `{}` on enum `{}`",
                variant_name,
                path[..path.len() - 1].join("::")
            ))
            .with_span(span));
        };
        let hir::HirVariantPayload::Struct(var_fields) = &v.payload else {
            return Err(CompileError::type_error(format!(
                "variant `{}` does not take struct fields",
                variant_name
            ))
            .with_span(span));
        };
        // 模式按字段的**声明顺序**排（写出来的顺序不作数）；没写到的字段
        // 要么是 `..` 补上的通配，要么直接报错。
        let mut pats: Vec<Option<hir::HirPattern>> = vec![None; var_fields.len()];
        for fp in fields {
            let Some(idx) = var_fields.iter().position(|f| &f.name == &fp.name) else {
                return Err(CompileError::name_error(format!(
                    "no field `{}` on variant `{}`",
                    fp.name, variant_name
                ))
                .with_span(span));
            };
            if pats[idx].is_some() {
                return Err(CompileError::name_error(format!(
                    "field `{}` is given twice",
                    fp.name
                ))
                .with_span(span));
            }
            // 载荷字段类型来自**声明**，泛型参数还是占位推断变量；必须按
            // scrutinee 的实际类型实参替换。不替换的话 `Opt::<u8>::Some { v }`
            // 里的 `v` 拿到的是声明中的 `T`（一个没人绑过的 `_`），后面
            // `v as i32` 报 `cannot cast '_' to 'i32'`。
            let field_ty = self.subst_variant_field_ty(enum_id, scrut_ty, &var_fields[idx].ty);
            pats[idx] = Some(self.lower_pattern(&fp.pattern, &field_ty)?);
        }
        let mut args = Vec::with_capacity(pats.len());
        for (i, pat) in pats.into_iter().enumerate() {
            match pat {
                Some(p) => args.push(p),
                None if rest => args.push(hir::HirPattern::Wildcard),
                None => {
                    return Err(CompileError::name_error(format!(
                        "missing field `{}` in pattern for variant `{}`",
                        var_fields[i].name, variant_name
                    ))
                    .with_span(span))
                }
            }
        }
        Ok(hir::HirPattern::Variant { index, args })
    }

    /// 枚举变体的结构体载荷构造：`Shape::Circle { r: 3 }`。
    fn check_enum_struct_variant_init(
        &mut self,
        enum_id: DefId,
        path: &[String],
        fields: &[(String, Expression)],
        base: Option<&Expression>,
        expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let variant_name = path.last().cloned().unwrap_or_default();
        let index = self.variant_index(enum_id, &variant_name);
        let Some(hir::HirTypeDef::Enum(he)) = self.type_defs.get(&enum_id).cloned() else {
            return Err(
                CompileError::internal("enum missing type definition").with_span(span)
            );
        };
        let v = he.variants.get(index).ok_or_else(|| {
            CompileError::name_error(format!(
                "no variant `{}` on enum `{}`",
                variant_name,
                path[..path.len() - 1].join("::")
            ))
            .with_span(span)
        })?;
        let hir::HirVariantPayload::Struct(var_fields) = &v.payload else {
            return Err(CompileError::type_error(format!(
                "variant `{}` does not take struct fields",
                variant_name
            ))
            .with_span(span));
        };
        if base.is_some() {
            return Err(CompileError::type_error(
                "struct update syntax is not supported for enum variants",
            )
            .with_span(span));
        }
        let raw_tys: Vec<Ty> = var_fields.iter().map(|f| f.ty.clone()).collect();
        let provided: Vec<Ty> = match self.infer.resolve(expected) {
            Ty::Named(id, a) if id == enum_id => a,
            _ => Vec::new(),
        };
        let targs = if provided.len() == raw_tys.len() && !raw_tys.is_empty() {
            provided
        } else {
            // 从各字段的实参类型推断。
            let mut actuals = Vec::new();
            for (name, expr) in fields {
                let Some(idx) = var_fields.iter().position(|f| &f.name == name) else {
                    return Err(CompileError::name_error(format!(
                        "no field `{}` on variant `{}`",
                        name, variant_name
                    ))
                    .with_span(span));
                };
                let aty = self.infer.fresh_infer(span);
                let h = self.check_expr(expr, &aty)?;
                actuals.push((idx, self.infer.resolve(&h.ty)));
            }
            let params = self
                .type_generic_params
                .get(&enum_id)
                .cloned()
                .unwrap_or_default();
            let mut out: Vec<Option<Ty>> = vec![None; params.len()];
            for (idx, aty) in &actuals {
                if let Some(raw) = raw_tys.get(*idx) {
                    self.bind_type_args(raw, aty, &params, &mut out, span)?;
                }
            }
            out.into_iter()
                .map(|o| o.unwrap_or(Ty::Infer(InferId(0))))
                .collect()
        };
        // 实参按字段的**声明顺序**排（写出来的顺序不作数）。
        let mut args: Vec<Option<HirExpr>> = vec![None; var_fields.len()];
        for (name, expr) in fields {
            let Some(idx) = var_fields.iter().position(|f| &f.name == name) else {
                return Err(CompileError::name_error(format!(
                    "no field `{}` on variant `{}`",
                    name, variant_name
                ))
                .with_span(span));
            };
            if args[idx].is_some() {
                return Err(CompileError::name_error(format!(
                    "field `{}` is given twice",
                    name
                ))
                .with_span(span));
            }
            let want = self.subst_def_ty(enum_id, &targs, &var_fields[idx].ty);
            let checked = self.check_expr(expr, &want)?;
            self.check_partial_move(&checked, expr.span())?;
            args[idx] = Some(checked);
        }
        let mut hir_args = Vec::with_capacity(args.len());
        for (i, arg) in args.into_iter().enumerate() {
            let Some(arg) = arg else {
                return Err(CompileError::name_error(format!(
                    "missing field `{}` in variant `{}`",
                    var_fields[i].name, variant_name
                ))
                .with_span(span));
            };
            hir_args.push(arg);
        }
        Ok(HirExpr {
            kind: ExprKind::EnumVariant {
                def_id: enum_id,
                index,
                args: hir_args,
            },
            ty: Ty::Named(enum_id, targs),
            span,
        })
    }

    /// 查字段（不报错）：用于区分 `expr.name(...)` 是字段函数指针调用
    /// 还是 `impl` 方法调用。引用/指针自动穿透。
    fn lookup_field(&self, ty: &Ty, name: &str) -> Option<(Ty, FieldIndex)> {
        match ty {
            Ty::Named(id, args) => self
                .struct_field_tys(*id, args)
                .into_iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, ty, index)| (ty, FieldIndex::Named(index))),
            Ty::Tuple(tys) => {
                let idx = name.parse::<usize>().ok()?;
                tys.get(idx).cloned().map(|t| (t, FieldIndex::Pos(idx)))
            }
            Ty::Reference(inner, _) | Ty::Pointer(inner, _) => self.lookup_field(inner, name),
            _ => None,
        }
    }

    /// 查 `impl` 方法。方法已展开成普通函数 `Type__method`。
    fn lookup_method(&self, ty: &Ty, name: &str) -> Option<hir::DefId> {
        // 基本类型也能有 `impl` 方法（`impl i32 { fn lt .. }`）：目标类型
        // 名字就是类型名 `i32` / `u8` / `f64` / `bool` / `str`。
        let target = match ty {
            Ty::Named(id, _) => self.def_names.get(id)?.clone(),
            Ty::Int(w) => format!("i{w}"),
            Ty::Uint(w) => format!("u{w}"),
            Ty::Float(w) => format!("f{w}"),
            Ty::Bool => "bool".to_string(),
            Ty::Char => "char".to_string(),
            Ty::Str => "str".to_string(),
            Ty::Reference(inner, _) | Ty::Pointer(inner, _) => {
                return self.lookup_method(inner, name);
            }
            _ => return None,
        };
        let mangled = format!("{target}__{name}");
        if !self.impl_targets.contains_key(&mangled) {
            return None;
        }
        let method = self.symbols.get(&mangled).copied()?;
        self.functions.get(&method)?;
        Some(method)
    }

    /// 显式 turbofish 的泛型调用：`size_of::<T>()`、`Foo::<i32>::new(..)`。
    ///
    /// 返回 `None` 表示不是泛型函数，交回普通调用路径处理。
    fn check_turbofish_call(
        &mut self,
        inner: &Expression,
        targs: &[ast::Type],
        args: &[ast::Argument],
        span: ast::Span,
    ) -> Result<Option<HirExpr>> {
        let base = match inner {
            Expression::Variable(name, _) => match self.resolve_name(name, span) {
                Ok(id) => id,
                Err(_) => return Ok(None),
            },
            Expression::Path(path, _) => match self.resolve_path(path, span) {
                Ok(id) => id,
                Err(_) => return Ok(None),
            },
            _ => return Ok(None),
        };
        let Some(names) = self.generic_names.get(&base).cloned() else {
            return Ok(None);
        };
        let Some(ast_fn) = self.generic_asts.get(&base).cloned() else {
            return Ok(None);
        };
        let mut ty_args = Vec::with_capacity(targs.len());
        for t in targs {
            ty_args.push(self.ast_to_hir_type(t)?);
        }
        if ty_args.len() != names.len() {
            return Err(CompileError::type_error(format!(
                "expected {} type argument(s) for `{}`, found {}",
                names.len(),
                ast_fn.name,
                ty_args.len()
            ))
            .with_span(span));
        }
        // 编译期内建：`size_of` / `align_of` 直接算成字面量。
        if ast_fn.body.is_none() && matches!(ast_fn.name.as_str(), "size_of" | "align_of") {
            let ty = &ty_args[0];
            let value = if ast_fn.name == "size_of" {
                self.infer.size_of(ty)
            } else {
                self.infer.align_of(ty)
            };
            return Ok(Some(HirExpr {
                kind: ExprKind::Literal(hir::HirLiteral::Int(value as i128)),
                ty: Ty::Uint(self.pointer_bits as u16),
                span,
            }));
        }
        let id = self.instantiate_generic(base, &ast_fn, &names, &ty_args, span)?;
        let sig = self
            .functions
            .get(&id)
            .cloned()
            .ok_or_else(|| CompileError::internal("instantiated signature missing"))?;
        let mut hir_args = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            let e = match arg {
                ast::Argument::Pos(e) | ast::Argument::Named(_, e) => e,
            };
            let pty = match sig.params.get(i) {
                Some(p) => p.ty.clone(),
                None => self.infer.fresh_infer(span),
            };
            let h = self.check_expr(e, &pty)?;
            self.check_partial_move(&h, e.span())?;
            hir_args.push(h);
        }
        Ok(Some(HirExpr {
            kind: ExprKind::Call {
                callee: id,
                args: hir_args,
                result: self.alloc_local(),
            },
            ty: sig.return_type,
            span,
        }))
    }

    fn check_method_call(
        &mut self,
        method: hir::DefId,
        receiver: HirExpr,
        args: &[ast::Argument],
        span: ast::Span,
    ) -> Result<HirExpr> {
        if self.generic_names.contains_key(&method) {
            return self.check_generic_method_call(method, receiver, args, span);
        }
        let sig = self.functions.get(&method).cloned().ok_or_else(|| {
            CompileError::internal(format!(
                "impl method {:?} missing from function table",
                method
            ))
        })?;
        if sig.params.is_empty() {
            return Err(
                CompileError::type_error("impl method is missing its receiver parameter")
                    .with_span(span),
            );
        }
        let mut hir_args = Vec::with_capacity(args.len() + 1);
        if !matches!(
            self.infer.resolve(&sig.params[0].ty),
            Ty::Reference(..) | Ty::Pointer(..)
        ) {
            self.check_partial_move(&receiver, span)?;
        }
        hir_args.push(receiver);
        for (i, arg) in args.iter().enumerate() {
            let a = match arg {
                ast::Argument::Pos(e) | ast::Argument::Named(_, e) => e,
            };
            let param_ty = sig
                .params
                .get(i + 1)
                .map(|p| p.ty.clone())
                .unwrap_or(Ty::Infer(InferId(0)));
            let arg = self.check_expr(a, &param_ty)?;
            if !matches!(
                self.infer.resolve(&param_ty),
                Ty::Reference(..) | Ty::Pointer(..)
            ) {
                self.check_partial_move(&arg, a.span())?;
            }
            hir_args.push(arg);
        }
        Ok(HirExpr {
            kind: ExprKind::Call {
                callee: method,
                args: hir_args,
                result: self.alloc_local(),
            },
            ty: sig.return_type.clone(),
            span,
        })
    }

    /// 泛型 `impl` 方法的调用：按 receiver 的具体类型单态化。
    fn check_generic_method_call(
        &mut self,
        base: hir::DefId,
        receiver: HirExpr,
        args: &[ast::Argument],
        span: ast::Span,
    ) -> Result<HirExpr> {
        let ast_fn = self
            .generic_asts
            .get(&base)
            .cloned()
            .ok_or_else(|| CompileError::internal("generic method missing AST"))?;
        let names = self.generic_names.get(&base).cloned().unwrap_or_default();
        let mut subst: HashMap<String, Ty> = HashMap::new();
        // receiver 的具体类型决定 impl 的泛型实参。
        let recv_ty = self.infer.resolve(&receiver.ty);
        let recv_inner = match &recv_ty {
            Ty::Reference(inner, _) | Ty::Pointer(inner, _) => (**inner).clone(),
            _ => recv_ty.clone(),
        };
        if let Some(target) = self.impl_targets.get(&ast_fn.name).cloned() {
            self.match_generic_ast(&target, &recv_inner, &names, &mut subst, span)?;
        }
        let mut hir_args = Vec::with_capacity(args.len() + 1);
        hir_args.push(receiver);
        for (i, arg) in args.iter().enumerate() {
            let e = match arg {
                ast::Argument::Pos(e) | ast::Argument::Named(_, e) => e,
            };
            let aty = self.infer.fresh_infer(span);
            let h = self.check_expr(e, &aty)?;
            self.check_partial_move(&h, e.span())?;
            if let Some(p) = ast_fn.params.get(i) {
                self.match_generic_ast(&p.ty, &h.ty, &names, &mut subst, span)?;
            }
            hir_args.push(h);
        }
        let mut ordered = Vec::with_capacity(names.len());
        for n in &names {
            match subst.get(n) {
                Some(t) => ordered.push(self.infer.resolve(t)),
                None => {
                    return Err(CompileError::type_error(format!(
                        "cannot infer type parameter `{n}` for `{}`",
                        ast_fn.name
                    ))
                    .with_span(span))
                }
            }
        }
        let id = self.instantiate_generic(base, &ast_fn, &names, &ordered, span)?;
        let sig = self.functions.get(&id).cloned();
        let ret = sig
            .as_ref()
            .map(|f| f.return_type.clone())
            .unwrap_or(Ty::Unit);
        let mut final_args = Vec::with_capacity(hir_args.len());
        final_args.push(hir_args.remove(0));
        for (i, arg) in args.iter().enumerate() {
            let e = match arg {
                ast::Argument::Pos(e) | ast::Argument::Named(_, e) => e,
            };
            let pty = match sig.as_ref().and_then(|s| s.params.get(i + 1)) {
                Some(p) => p.ty.clone(),
                None => self.infer.fresh_infer(span),
            };
            let h = self.check_expr(e, &pty)?;
            self.check_partial_move(&h, e.span())?;
            final_args.push(h);
        }
        Ok(HirExpr {
            kind: ExprKind::Call {
                callee: id,
                args: final_args,
                result: self.alloc_local(),
            },
            ty: ret,
            span,
        })
    }

    /// 标量索引 `x[i]`：结果是元素类型。`str` 按字节索引得到 `u8`。
    fn check_index_scalar(
        &mut self,
        recv: HirExpr,
        index: &Expression,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let idx = self.check_expr(index, &Ty::Int(64))?;
        let recv_ty = self.infer.resolve(&recv.ty);
        // `Vec` 之类的"data: *mut T"结构体支持下标：`v[i]` 脱糖成 `v.data[i]`
        // （指针下标，读/写都走指针算术，元素类型 = `data` 指向的类型）。这样
        // `Vec<u8>` 就能像 Rust 一样 `v[i]`（读）与 `v[i] = x`（写）。
        // `net.zr` 靠它加载（`buf[i]` 读 `Vec<u8>`）。
        if let Ty::Named(id, args) = &recv_ty {
            let fields = self.struct_field_tys(*id, args);
            if let Some((_, data_ty, _)) = fields.iter().find(|(n, _, _)| n == "data") {
                let data_ty = self.infer.resolve(data_ty);
                if matches!(data_ty, Ty::Pointer(..) | Ty::Reference(..)) {
                    let data_expr = HirExpr {
                        kind: ExprKind::Field(Box::new(recv), "data".to_string()),
                        ty: data_ty.clone(),
                        span,
                    };
                    return self.check_index_scalar(data_expr, index, span);
                }
            }
        }
        // 指针/引用指向聚合时，元素是聚合的**成员**而不是聚合本身：
        // `p: *mut [u8; 16]` 的 `p[k]` 是 `u8`。早先这里直接把 pointee 当
        // 元素类型，于是 `p[k]` 被推断成 `[u8; 16]`，下游按 16 字节步长缩放、
        // 还走聚合拷贝存储路径，写出的值和内存布局全都不对。
        let elem_ty = match &recv_ty {
            Ty::Array(t, _) | Ty::Slice(t, _) => (**t).clone(),
            Ty::Str => Ty::Uint(8),
            Ty::Pointer(inner, _) | Ty::Reference(inner, _) => {
                let pointee = (**inner).clone();
                match pointee {
                    Ty::Array(t, _) | Ty::Slice(t, _) => (*t).clone(),
                    other => other,
                }
            }
            _ => {
                // 其它类型：只有当它确实是（或能推断成）指针时才允许下标。
                //
                // 早先这里把 unify 的错误 `.ok()` 掉了，于是 `(*p)[i]`
                // （p: *const u8）这种"对 u8 下标"也照样编过：元素类型取
                // 一个新鲜的推断变量，MIR 把 `*p` 的**值**当成地址去做
                // 指针算术，运行时直接段错误。类型不对就明确报错。
                let elem = self.infer.fresh_infer(span);
                if self
                    .infer
                    .unify(
                        &recv.ty,
                        &Ty::Pointer(Box::new(elem.clone()), hir::Mutability::Immutable),
                        span,
                    )
                    .is_err()
                {
                    return Err(CompileError::type_error(format!(
                        "cannot index a value of type `{}`",
                        ty_name(&recv_ty)
                    ))
                    .with_span(span));
                }
                elem
            }
        };
        // 越界检查（编译期常量下标）：数组/切片的长度在编译期已知时，对
        // **字面量下标**（`arr[5]`，`arr[k]` 的 `k` 是 const 局部）做边界
        // 检查。运行期变量的下标留给 MIR 的越界检查（尚未接线），这里先
        // 堵住"明显越界"的静默错读——`arr[5]` 在 3 元素数组上会读到相邻
        // 内存，`arr[-1]` 更直接踩到数组前头。
        //
        // 只对 `Array` 与 `Slice`（长度已知或运行时长度）做；裸指针没有
        // 长度信息，`p[k]` 是 unsafe 的原始内存访问，不检查（与 C 一致）。
        if let Some(blen) = array_len_of(&recv_ty) {
            if let Some(i) = const_index_of(index) {
                if i < 0 {
                    return Err(CompileError::type_error(format!(
                        "index out of bounds: the len is {blen} but the index is {i}",
                    ))
                    .with_span(index.span()));
                }
                if i >= blen {
                    return Err(CompileError::type_error(format!(
                        "index out of bounds: the len is {blen} but the index is {i}",
                    ))
                    .with_span(index.span()));
                }
            }
        }
        Ok(HirExpr {
            kind: ExprKind::Index(Box::new(recv), Box::new(idx)),
            ty: elem_ty,
            span,
        })
    }

    fn check_call(
        &mut self,
        callee: &Expression,
        args: &[ast::Argument],
        expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        // `size_of::<T>()` 之类显式给出类型实参的调用。
        if let Expression::Turbofish(inner, targs, _) = callee {
            if let Some(call) = self.check_turbofish_call(inner, targs, args, span)? {
                return Ok(call);
            }
        }

        // 元组结构体构造：`Point(1, 2)` 语法上是把类型名当函数调用。
        if let Some(type_id) = self.tuple_struct_target(callee) {
            let want = match self.turbofish_expected(callee, type_id)? {
                Some(t) => t,
                None => expected.clone(),
            };
            return self.check_tuple_struct_init(type_id, args, &want, span);
        }

        // 枚举变体构造：`Shape::Circle(5)` 同样是"把变体名当函数调用"。
        if let Some((def_id, index)) = self.enum_variant_target(callee) {
            let want = match self.turbofish_expected(callee, def_id)? {
                Some(t) => t,
                None => expected.clone(),
            };
            return self.check_enum_variant_init(def_id, index, args, &want, span);
        }

        // 闭包立即调用：`(|x| x + 1)(5)`、`(|x| x + n)(5)`。走单态化内联：
        // 在调用点把闭包体展开，形参引用替换成实参（先 let 绑定一次，避免
        // 带副作用的实参被重复求值），捕获变量复用外层 local 的存储。
        if let Expression::Lambda(lam_params, lam_ret, lam_body, lam_span) = callee {
            return self.inline_closure_call(
                lam_params,
                lam_ret.as_ref(),
                lam_body,
                args,
                expected,
                span,
                *lam_span,
            );
        }

        // `expr.method(args)` 在语法上就是 `Call(Field(expr, method), args)`。
        // 字段优先：`ops.f(1, 2)` 里的 `f` 是函数指针字段；字段不存在时才
        // 到 `impl` 方法表里找 `Type__method`。
        let mut prepared_callee = None;
        if let Expression::Field(receiver, name, field_span) = callee {
            let recv_ty = self.infer_expr_ty(receiver)?;
            let recv = self.check_expr(receiver, &recv_ty)?;
            let recv_ty = self.infer.resolve(&recv.ty);
            // `v.len()` 与 `v.len` 的区分：带括号时方法优先（Rust 语义）。
            // 只有当同名字段本身是函数指针时才按字段调用。
            let field = self.lookup_field(&recv_ty, name);
            let field_is_fn = field
                .as_ref()
                .is_some_and(|(t, _)| matches!(self.infer.resolve(t), Ty::Function { .. }));
            if !field_is_fn {
                if let Some(method_id) = self.lookup_method(&recv_ty, name) {
                    return self.check_method_call(method_id, recv, args, span);
                }
            }
            if let Some((ty, _)) = field {
                prepared_callee = Some(HirExpr {
                    kind: ExprKind::Field(Box::new(recv), name.clone()),
                    ty,
                    span: *field_span,
                });
            }
        }

        let callee_ty = match &prepared_callee {
            Some(c) => c.ty.clone(),
            None => self.infer_expr_ty(callee)?,
        };
        let callee_hir = match prepared_callee {
            Some(c) => c,
            None => self.check_expr(callee, &callee_ty)?,
        };
        // 带捕获闭包的值调用：被调方是一个环境结构体（`__env_N`，字段 =
        // 捕获局部），不是普通函数指针。按环境 DefId 查 `closures_by_env`
        // 拿回闭包签名与内联体，`apply_capture_body` 把捕获 local 替换成
        // 环境字段后在调用点展开。`f(a)` / `f.f(a)`（`f` 是结构体字段）都
        // 走这条路。
        if let Ty::Named(env_id, _) = self.infer.resolve(&callee_hir.ty) {
            if let Some(rec) = self.closures_by_env.get(&env_id).cloned() {
                return self.apply_capture_body(callee_hir, &rec, args, expected, span);
            }
        }
        // 泛型函数：按实参类型生成单态实例，调用点直接指向实例。
        if let ExprKind::Def(id) = &callee_hir.kind {
            if let Some(call) = self.try_instantiate_call(*id, args, expected, span)? {
                return Ok(call);
            }
        }
        let (param_tys, ret_ty) = match &callee_hir.ty {
            Ty::Function { params, ret, .. } => (params.clone(), ret.as_ref().clone()),
            Ty::Infer(_) => {
                // 推断：创建参数类型变量
                let params: Vec<Ty> = (0..args.len())
                    .map(|_| self.infer.fresh_infer(span))
                    .collect();
                let ret = self.infer.fresh_infer(span);
                (params, ret)
            }
            _ => {
                return Err(CompileError::type_error(format!(
                    "cannot call non-function type `{}`",
                    self.infer.type_name(&callee_hir.ty)
                ))
                .with_span(span));
            }
        };
        let mut hir_args = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            let a = match arg {
                ast::Argument::Pos(e) => e,
                ast::Argument::Named(name, e) => {
                    // 命名参数：查找对应参数
                    let _ = name;
                    e
                }
            };
            // 参数表比实参少的唯一情形是"前向调用"：被调函数在源码里排在
            // 调用点后面，`functions` 表里它的 params 还是 `collect_declarations`
            // 留下的空占位（参数要等阶段 7 逐个函数检查时才补上），于是 `def_type`
            // 给出的 `Ty::Function { params: [] }` 查不到形参类型。
            //
            // 早先这里 `unwrap_or(Ty::Infer(InferId(0)))` 拿 `InferId(0)` 当
            // "未知类型"哨兵——但它是**活的推断变量**，大文件编译到一半它早
            // 已被 unify 到别的类型上（曾绑成某个 `Ty::Named`），`resolve` 出旧值
            // 后 `subtype` 报出与真实问题无关的 `type#N` 错（`std/io.zr` 的
            // `prefix_of` 前向调用就踩过）。改回新建推断变量：未知形参就让它
            // 独立推断、被实参绑上，不与全局共享的 `InferId(0)` 纠缠。
            let param_ty = param_tys
                .get(i)
                .cloned()
                .unwrap_or_else(|| self.infer.fresh_infer(span));
            let arg = self.check_expr(a, &param_ty)?;
            // 带捕获闭包（环境结构体 `__env_N`）传给 `fn(..)` 形参：环境值
            // 不是函数指针，被调方拿不到捕获 local，跨函数环境传递机制
            // 不存在，明确报错而不是把环境值当函数地址调用（运行必崩）。
            if let Ty::Named(arg_id, _) = self.infer.resolve(&arg.ty) {
                if self.closures_by_env.contains_key(&arg_id) {
                    let param = self.infer.resolve(&param_ty);
                    // 环境结构体传给 `fn(..)` 形参：被调方拿不到捕获 local，
                    // 跨函数环境传递不存在，运行必崩。形参停在 Infer 也一样
                    // 拒绝——它要么被绑成函数类型（上面），要么被绑成环境类型
                    // 却仍被当函数指针调用，两者都错。
                    if matches!(param, Ty::Function { .. } | Ty::Infer(_)) {
                        return Err(CompileError::type_error(
                        "captured closure cannot be passed as a plain `fn` value; \
                         a closure that captures variables carries its environment and \
                         cannot be converted to a function pointer",
                    )
                    .with_span(a.span()));
                    }
                }
            }
            // 形参类型是推断变量（提升闭包的未标注形参，如 `let id = |x| x;`
            // 里 `x` 的类型要靠 `id(7)` 反推）：`check_literal` 对 `Infer`
            // 期望只给字面量默认类型（`Int(32)`），不会主动绑定推断变量。
            // 反过来，实参类型停在推断变量而形参是具体类型（把 `id` 传给
            // `fn(..)` 形参，`id` 的形参类型还没被任何调用定下）：`check_expr`
            // 也不主动把两者统一。两种情况都要在这里补一次 unify，否则提升
            // 闭包的形参永远停在 Infer，收尾报 `type annotations needed`。
            let param_is_infer = matches!(self.infer.resolve(&param_ty), Ty::Infer(_));
            let arg_has_infer = ty_has_unresolved_infer(&arg.ty, &self.infer.vars);
            if param_is_infer || arg_has_infer {
                self.infer.unify(&param_ty, &arg.ty, span).ok();
            }
            // C 风格隐式拓宽：`f32`/`i32`/`u32` 实参传给 `f64` 形参时插入显式
            // 转换。只放行"数值同族、宽度不减"这一类；`unify` 要求宽度相等，
            // 所以 `check_expr` 本身既不拒绝也不改写跨宽实参——若不补这一步，
            // `f32` 的 4 字节位模式会被当 `f64` 直接读出来（结果是 0.0），
            // 整数更根本不在 XMM 里、形参拿到的是寄存器残留。
            let arg = self.widen_arg_for_call(arg, &param_ty)?;
            if !matches!(
                self.infer.resolve(&param_ty),
                Ty::Reference(..) | Ty::Pointer(..)
            ) {
                self.check_partial_move(&arg, a.span())?;
            }
            hir_args.push(arg);
        }
        // 具名函数走直接调用；其余函数类型表达式走运行期函数指针调用。
        let kind = match callee_hir.kind {
            ExprKind::Def(id) => ExprKind::Call {
                callee: id,
                args: hir_args,
                result: self.alloc_local(),
            },
            _ => ExprKind::CallIndirect {
                callee: Box::new(callee_hir),
                args: hir_args,
                result: self.alloc_local(),
            },
        };
        Ok(HirExpr {
            kind,
            ty: ret_ty,
            span,
        })
    }

    /// 把数值实参隐式拓宽成形参类型，返回带 `Cast` 的 HIR。
    ///
    /// `unify` 只接受等宽类型，所以 `check_expr` 对 `f32` 实参配 `f64` 形参
    /// 既不报错也不改写——原样推进调用点，`f32` 的 4 字节位模式就被当 `f64`
    /// 读出来（恒为 0.0），整数更根本不在 XMM 里、形参拿到寄存器残留。这里补
    /// 上一次显式转换。
    ///
    /// 只放行两类**加宽**：同族加宽（`i8→i32`、`f32→f64`、`u32→u64`）和整数转
    /// 浮点（`i32→f64`、`u32→f64`）。收窄（`f64→f32`、`i64→i32`）、跨族换符号
    /// （`u32→i32`）和浮点取整（`f64→i32`）这里一律**不插入转换**、原样放过。
    ///
    /// 注意：原样放过 ≠ 报错。调用点没有别的地方校验实参类型（`check_expr`
    /// 对变量实参只发 `Local(id)`、忽略 `expected`），所以 `takes_f32(f64)`
    /// 目前仍能编译，并把 f64 的位模式当 f32 读出来（恒为 0.0）。要真正禁止
    /// 窄化得在调用点补一次类型校验，但那是独立的、更大的改动，不是这个
    /// 函数的职责——这里只负责把该插的转换插对。
    fn widen_arg_for_call(
        &self,
        arg: HirExpr,
        param_ty: &Ty,
    ) -> Result<HirExpr> {
        #[derive(Clone, Copy, PartialEq)]
        enum Family {
            Signed(u16),
            Unsigned(u16),
            Float(u16),
        }
        let family = |t: &Ty| -> Option<Family> {
            match t {
                Ty::Int(w) => Some(Family::Signed(*w)),
                Ty::Uint(w) => Some(Family::Unsigned(*w)),
                Ty::Float(w) => Some(Family::Float(*w)),
                _ => None,
            }
        };
        let (have, want) = (
            self.infer.resolve(&arg.ty),
            self.infer.resolve(param_ty),
        );
        let (sh, sw) = match (family(&have), family(&want)) {
            (Some(s), Some(w)) if s != w => (s, w),
            _ => return Ok(arg),
        };
        let widen = match (sh, sw) {
            (Family::Signed(a), Family::Signed(b))
            | (Family::Unsigned(a), Family::Unsigned(b))
            | (Family::Float(a), Family::Float(b)) => b > a,
            (Family::Signed(_) | Family::Unsigned(_), Family::Float(_)) => true,
            _ => false,
        };
        if !widen {
            return Ok(arg);
        }
        let span = arg.span;
        Ok(HirExpr {
            kind: ExprKind::Cast(Box::new(arg)),
            ty: want.clone(),
            span,
        })
    }

    // ==================================================================
    // match / 穷尽性
    // ==================================================================

    fn check_match(
        &mut self,
        scrutinee: &Expression,
        arms: &[Arm],
        expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let s_ty = self.infer_expr_ty(scrutinee)?;
        let s = self.check_expr(scrutinee, &s_ty)?;
        // scrutinee 只求值一次：先绑到一个匿名局部，各臂都读这个局部。
        // 否则带副作用的 scrutinee（`match v.pop() { .. }`）会被执行多次。
        let scrut_ty0 = self.infer.resolve(&s.ty);
        let scrut_local = self.alloc_local();
        self.locals.insert(scrut_local, scrut_ty0.clone());
        self.infer.locals.insert(scrut_local, scrut_ty0.clone());
        let s_local = HirExpr {
            kind: ExprKind::Local(scrut_local),
            ty: scrut_ty0.clone(),
            span,
        };
        // 模式匹配 `&T` / `*const T` 时自动穿透一层：聚合值在运行期本来
        // 就是地址，引用可以直接当值；标量才需要显式解引用。
        let (scrut_ty, s_local) = match &scrut_ty0 {
            Ty::Reference(inner, _) | Ty::Pointer(inner, _) => {
                let inner = self.infer.resolve(inner);
                if self.ty_is_aggregate(&inner) {
                    (
                        inner.clone(),
                        HirExpr {
                            kind: ExprKind::Local(scrut_local),
                            ty: inner,
                            span,
                        },
                    )
                } else {
                    let d = HirExpr {
                        kind: ExprKind::Deref(Box::new(s_local)),
                        ty: inner.clone(),
                        span,
                    };
                    (inner, d)
                }
            }
            _ => (scrut_ty0, s_local),
        };
        // 或模式先在臂级展开：`3 | 4 => b` 变成两条"3 => b"、"4 => b"。
        //
        // 每条臂记四样东西：模式（穷尽性检查用）、模式自身的匹配条件、
        // guard、臂体。条件与 guard 分开是因为"模式恒匹配"（`_` / 绑定臂）
        // 和"模式有条件"（字面量 / 结构体模式）要区别对待。
        let mut lowered: Vec<MatchArm> = Vec::new();
        for arm in arms {
            // 先展开模式（里的绑定要在此注册），再看 guard / 臂体——
            // `v if v > 0 => v` 的两处 v 都得能解析到臂绑定。
            self.push_scope();
            let mut alts_hir: Vec<hir::HirPattern> = Vec::new();
            let mut alts_ast: Vec<&AstPattern> = Vec::new();
            push_or_alts(&arm.pattern, &mut alts_ast);
            for alt in &alts_ast {
                match self.lower_pattern(alt, &scrut_ty) {
                    Ok(pat) => alts_hir.push(pat),
                    Err(e) => {
                        self.pop_scope();
                        return Err(e);
                    }
                }
            }
            let guard = match &arm.guard {
                Some(g) => Some(self.check_expr(g, &Ty::Bool)?),
                None => None,
            };
            let body = self.check_expr(&arm.body, expected)?;
            self.pop_scope();
            for pat in alts_hir {
                // 模式的条件与绑定替换。绑定臂把它绑的局部换写成 scrutinee
                // 本身：`v if v > 0 => v` 的 v 两处都替换进去。结构体模式的
                // 绑定落在字段上（`P { x: v }` 的 v 是 p.x），逐个换写。
                // 臂绑定没有"先赋值再读"的独立存储，直接内联取值表达式是
                // 唯一等价的降级（scrutinee 重复求值是本实现已知的限定，
                // 见下）。
                let (cond, binds) = self.pat_match(&s_local, &pat, span)?;
                let mut g = guard.clone();
                let mut b2 = body.clone();
                for (local, value) in &binds {
                    if let Some(g2) = &mut g {
                        substitute_local(g2, *local, value);
                    }
                    substitute_local(&mut b2, *local, value);
                }
                lowered.push(MatchArm {
                    pattern: pat,
                    cond,
                    guard: g,
                    body: b2,
                });
            }
        }
        // 最后一条必须是"无条件匹配臂"（`_`），或穷尽 bool 的两条字面量臂：
        // if/else 链缺 else 会把"没匹配上"变成取到 Unit 继续跑——另一种静默算错。
        //
        // 这个标志在下面脱糖时还要用：**最后一条不是无条件臂**意味着穷尽性
        // 是靠"枚举变体/bool 值全覆盖"证明的，链尾的 else 永远走不到，得补
        // 一个发散尾分支（否则 join 块会把最后一个臂的值覆盖成 `()`）。
        let last_unconditional = matches!(
            lowered.last(),
            Some(arm) if arm.cond.is_none() && arm.guard.is_none() && pat_irrefutable(&arm.pattern)
        );
        if !last_unconditional {
            // bool 的字面量覆盖（true / false 各一条）也算穷尽。
            let bool_covered = matches!(scrut_ty, Ty::Bool) && {
                let mut seen = [false, false];
                for arm in &lowered {
                    if let hir::HirPattern::Literal(v) = &arm.pattern {
                        if *v == 0 {
                            seen[0] = true;
                        } else if *v == 1 {
                            seen[1] = true;
                        }
                    }
                }
                seen[0] && seen[1]
            };
            // 枚举：所有变体都被覆盖也算穷尽（和 Rust 一样，不用再写 `_` 臂）。
            // 被匹配的值可能是引用 / 指针——穿透一层看它的目标类型。
            let scrutinee_ty = match &scrut_ty {
                Ty::Reference(inner, _) | Ty::Pointer(inner, _) => inner.as_ref(),
                other => other,
            };
            let enum_covered = match scrutinee_ty {
                Ty::Named(def_id, _) => {
                    let variants = match self.type_defs.get(def_id) {
                        Some(hir::HirTypeDef::Enum(he)) => he.variants.len(),
                        _ => 0,
                    };
                    if variants == 0 {
                        false
                    } else {
                        let mut seen = vec![false; variants];
                        for arm in &lowered {
                            if let hir::HirPattern::Variant { index, .. } = &arm.pattern {
                                if let Some(flag) = seen.get_mut(*index) {
                                    *flag = true;
                                }
                            }
                        }
                        seen.iter().all(|x| *x)
                    }
                }
                _ => false,
            };
            if !bool_covered && !enum_covered {
                // 改进报错文案：区分"缺 `_` 通配臂"和"缺具体值/变体"。
                // - 整型/标量：需要 `_` 臂捕获所有未列出的值
                // - 枚举/bool：如果已经全覆盖则不会走到这里
                let kind_name = match scrutinee_ty {
                    Ty::Int(_) | Ty::Uint(_) | Ty::Float(_) => "integer",
                    Ty::Named(id, _) => {
                        // 尝试显示类型名
                        match self.type_defs.get(id) {
                            Some(hir::HirTypeDef::Enum(e)) => &e.name,
                            Some(hir::HirTypeDef::Struct(s)) => &s.name,
                            _ => "type",
                        }
                    }
                    _ => "value",
                };
                // 对枚举，报出**缺失的变体名**而不是笼统的"需要 `_` 臂"。
                // Rust 在枚举缺臂时提示的是具体变体（`non-exhaustive patterns:
                // `C` not covered`），早先这里说 "must have a trailing `_` arm
                // or cover all values" 会误导人以为补一个 `_` 就行。
                let mut missing: Vec<&str> = Vec::new();
                if let Ty::Named(id, _) = scrutinee_ty {
                    if let Some(hir::HirTypeDef::Enum(he)) = self.type_defs.get(id) {
                        let mut seen = vec![false; he.variants.len()];
                        for arm in &lowered {
                            if let hir::HirPattern::Variant { index, .. } = &arm.pattern {
                                if let Some(flag) = seen.get_mut(*index) {
                                    *flag = true;
                                }
                            }
                        }
                        for (idx, v) in he.variants.iter().enumerate() {
                            if !seen[idx] {
                                missing.push(v.name.as_str());
                            }
                        }
                    }
                }
                let msg = if !missing.is_empty() {
                    format!(
                        "match is not exhaustive: `{}` variant{} not covered: {}",
                        kind_name,
                        if missing.len() > 1 { "s" } else { "" },
                        missing.join(", ")
                    )
                } else {
                    format!(
                        "match on `{kind_name}` must have a trailing `_` arm or cover all values"
                    )
                };
                return Err(CompileError::type_error(msg).with_span(span));
            }
        }
        // 把
        //     match s { v1 => b1, v2 | guard => b2, _ => b3 }
        // 脱糖成
        //     if s == v1 { b1 } else if s == v2 && guard { b2 } else { b3 }
        //
        // scrutinee 在每个臂上重新求值一次：对字面量 / 局部变量无感，但带
        // 副作用的 scrutinee 会被重复执行——HIR 表达式形态下的已知限定。
        let ty = lowered
            .first()
            .map(|arm| arm.body.ty.clone())
            .unwrap_or(Ty::Never);
        let n = lowered.len();
        // 链尾的 else。最后一条臂是无条件臂时它自己就是 else（循环里会被
        // 覆盖）；否则穷尽性来自"变体/值全覆盖"，走到这里是不可达的——
        // 用一个发散分支而不是 `None`。
        //
        // 用 `None` 会开启 MIR 的"if 没有 else ⇒ 值是 Unit"路径：join 块
        // 无条件往结果临时名写 `Unit`，把最后一个变体臂算出来的值覆盖掉。
        // 症状是 `match e { A => 10, B => 20, C => 50 }` 对 C 返回 51。
        let mut chain: Option<hir::HirBlock> = if last_unconditional {
            None
        } else {
            Some(hir::HirBlock {
                stmts: Vec::new(),
                tail: Some(Box::new(hir::HirExpr {
                    kind: ExprKind::Unreachable,
                    ty: Ty::Never,
                    span,
                })),
            })
        };
        for k in (0..n).rev() {
            let arm = &lowered[k];
            let unconditional =
                arm.cond.is_none() && arm.guard.is_none() && pat_irrefutable(&arm.pattern);
            if unconditional {
                // 无条件臂就是链尾的 else；可前面的臂永不可达（首匹配胜出）。
                chain = Some(arm_body_block(&arm.body));
                continue;
            }
            // 条件 = 模式自身的条件 && guard。
            let cond = match (&arm.cond, &arm.guard) {
                (Some(c), Some(g)) => and_expr(c.clone(), g.clone(), span),
                (Some(c), None) => c.clone(),
                (None, Some(g)) => g.clone(),
                (None, None) => bool_lit(true, span),
            };
            let else_block = chain.take();
            let node = hir::HirExpr {
                kind: ExprKind::If(
                    Box::new(cond),
                    arm_body_block(&arm.body),
                    else_block,
                ),
                ty: ty.clone(),
                span,
            };
            chain = Some(hir::HirBlock {
                stmts: Vec::new(),
                tail: Some(Box::new(node)),
            });
        }
        let node = match chain {
            Some(mut last) => match last.tail.take() {
                Some(node) => *node,
                // 整条链就是单个有返回块的臂体（如 `_ => { return 3; }`）。
                None => hir::HirExpr {
                    kind: ExprKind::Block(last),
                    ty: ty.clone(),
                    span,
                },
            },
            None => hir::HirExpr {
                kind: ExprKind::Match,
                ty: ty.clone(),
                span,
            },
        };
        Ok(hir::HirExpr {
            kind: ExprKind::Block(hir::HirBlock {
                stmts: vec![hir::HirStmt::Let {
                    local: scrut_local,
                    init: Some(Box::new(s)),
                }],
                tail: Some(Box::new(node)),
            }),
            ty,
            span,
        })
    }

    /// 模式与某个值的匹配：返回 (匹配条件, 绑定替换)。
    ///
    /// - 条件：`None` 表示恒匹配（绑定 / 通配模式）；`Some(e)` 表示匹配成立
    ///   当且仅当 `e` 为真。
    /// - 绑定替换：模式里绑定的局部量 → 取值表达式。结构化模式的绑定落在
    ///   字段上（`P { x: v }` 的 v 是 `p.x`），不是整个 scrutinee。
    ///
    /// 目前支持字面量、绑定、通配与**结构体模式**（字段模式可以是字面量 /
    /// 绑定 / 通配）。元组 / 枚举变体 / 切片 / 范围模式还没有对应的运行期
    /// 表示，遇到时直接报错——不静默当恒真。
    fn pat_match(
        &self,
        scrutinee: &HirExpr,
        pat: &hir::HirPattern,
        span: ast::Span,
    ) -> Result<(Option<HirExpr>, Vec<(hir::LocalId, HirExpr)>)> {
        match pat {
            hir::HirPattern::Wildcard => Ok((None, Vec::new())),
            hir::HirPattern::Binding(local) => Ok((None, vec![(*local, scrutinee.clone())])),
            hir::HirPattern::Literal(v) => {
                let lit = hir::HirExpr {
                    kind: ExprKind::Literal(match &scrutinee.ty {
                        Ty::Bool => hir::HirLiteral::Bool(*v != 0),
                        Ty::Char => hir::HirLiteral::Char(
                            u32::try_from(*v)
                                .ok()
                                .and_then(char::from_u32)
                                .unwrap_or('\0'),
                        ),
                        _ => hir::HirLiteral::Int(*v),
                    }),
                    ty: scrutinee.ty.clone(),
                    span,
                };
                let eq = hir::HirExpr {
                    kind: ExprKind::Binary(
                        hir::BinaryOp::Eq,
                        Box::new(scrutinee.clone()),
                        Box::new(lit),
                    ),
                    ty: Ty::Bool,
                    span,
                };
                Ok((Some(eq), Vec::new()))
            }
            hir::HirPattern::Struct(fields) => {
                let mut conds: Vec<HirExpr> = Vec::new();
                let mut binds: Vec<(hir::LocalId, HirExpr)> = Vec::new();
                for (name, fty, sub) in fields {
                    // 子值 = `scrutinee.<字段>`。
                    let sub_value = hir::HirExpr {
                        kind: ExprKind::Field(Box::new(scrutinee.clone()), name.clone()),
                        ty: fty.clone(),
                        span,
                    };
                    let (cond, sub_binds) = self.pat_match(&sub_value, sub, span)?;
                    conds.extend(cond);
                    binds.extend(sub_binds);
                }
                // 条件 = 各字段条件之合。
                let cond = conds.into_iter().reduce(|a, b| and_expr(a, b, span));
                Ok((cond, binds))
            }
            hir::HirPattern::Tuple(elems) => {
                let mut conds: Vec<HirExpr> = Vec::new();
                let mut binds: Vec<(hir::LocalId, HirExpr)> = Vec::new();
                let elem_tys = match &scrutinee.ty {
                    Ty::Tuple(ts) => ts.clone(),
                    _ => Vec::new(),
                };
                for (idx, sub) in elems.iter().enumerate() {
                    // 子值 = `scrutinee.<下标>`。
                    let sub_value = hir::HirExpr {
                        kind: ExprKind::Field(
                            Box::new(scrutinee.clone()),
                            idx.to_string(),
                        ),
                        ty: elem_tys.get(idx).cloned().unwrap_or_else(|| scrutinee.ty.clone()),
                        span,
                    };
                    let (cond, sub_binds) = self.pat_match(&sub_value, sub, span)?;
                    conds.extend(cond);
                    binds.extend(sub_binds);
                }
                let cond = conds.into_iter().reduce(|a, b| and_expr(a, b, span));
                Ok((cond, binds))
            }
            hir::HirPattern::Variant { index, args } => {
                // 条件：判别式等于变体下标。
                // 被匹配的值可能是引用 / 指针——穿透一层看它的目标类型。
                let (def_id, targs) = match &scrutinee.ty {
                    Ty::Named(id, a) => (*id, a.clone()),
                    Ty::Reference(inner, _) | Ty::Pointer(inner, _) => match inner.as_ref() {
                        Ty::Named(id, a) => (*id, a.clone()),
                        _ => {
                            return Err(CompileError::type_error(
                                "enum variant pattern matched against a non-enum value",
                            )
                            .with_span(span));
                        }
                    },
                    _ => {
                        return Err(CompileError::type_error(
                            "enum variant pattern matched against a non-enum value",
                        )
                        .with_span(span));
                    }
                };
                let tag_ty = self.enum_tag_type(def_id);
                let tag = hir::HirExpr {
                    kind: ExprKind::EnumTag(Box::new(scrutinee.clone())),
                    ty: tag_ty.clone(),
                    span,
                };
                let eq = hir::HirExpr {
                    kind: ExprKind::Binary(
                        hir::BinaryOp::Eq,
                        Box::new(tag),
                        Box::new(hir::HirExpr {
                            kind: ExprKind::Literal(hir::HirLiteral::Int(
                                self.variant_discriminant(def_id, *index),
                            )),
                            ty: tag_ty,
                            span,
                        }),
                    ),
                    ty: Ty::Bool,
                    span,
                };
                // 载荷的绑定：变体 `index` 的第 i 个载荷字段。
                let field_tys = self
                    .variant_field_types_args(def_id, *index, &targs)
                    .unwrap_or_default();
                let mut binds: Vec<(hir::LocalId, HirExpr)> = Vec::new();
                let mut conds: Vec<HirExpr> = vec![eq];
                for (i, sub) in args.iter().enumerate() {
                    let sub_value = hir::HirExpr {
                        kind: ExprKind::EnumField {
                            value: Box::new(scrutinee.clone()),
                            variant: *index,
                            index: i,
                        },
                        ty: field_tys.get(i).cloned().unwrap_or_else(|| scrutinee.ty.clone()),
                        span,
                    };
                    let (cond, sub_binds) = self.pat_match(&sub_value, sub, span)?;
                    // 载荷字段的条件也要算进来——`Shape::Circle(1)` 要同时
                    // 对上判别式与载荷。
                    conds.extend(cond);
                    binds.extend(sub_binds);
                }
                let cond = conds.into_iter().reduce(|a, b| and_expr(a, b, span));
                Ok((cond, binds))
            }
            hir::HirPattern::Slice => Err(CompileError::type_error(
                "match on slice patterns is not implemented yet",
            )
            .with_span(span)),
            hir::HirPattern::Range {
                start,
                end,
                inclusive,
            } => {
                // 条件：start <= scrutinee <= end（`..` 的右端不含）。
                let mut conds: Vec<HirExpr> = Vec::new();
                // 下端总是含的（`11..20` 含 11），只有上端分含与不含。
                let lo = hir::HirExpr {
                    kind: ExprKind::Binary(
                        hir::BinaryOp::Le,
                        Box::new(start.as_ref().clone()),
                        Box::new(scrutinee.clone()),
                    ),
                    ty: Ty::Bool,
                    span,
                };
                let hi = hir::HirExpr {
                    kind: ExprKind::Binary(
                        if *inclusive {
                            hir::BinaryOp::Le
                        } else {
                            hir::BinaryOp::Lt
                        },
                        Box::new(scrutinee.clone()),
                        Box::new(end.as_ref().clone()),
                    ),
                    ty: Ty::Bool,
                    span,
                };
                conds.push(lo);
                conds.push(hi);
                let cond = conds.into_iter().reduce(|a, b| and_expr(a, b, span));
                Ok((cond, Vec::new()))
            }
            hir::HirPattern::Ref => Err(CompileError::type_error(
                "match on reference patterns is not implemented yet",
            )
            .with_span(span)),
        }
    }

    fn check_exhaustiveness(&self, _functions: &[hir::HirFunction]) -> Result<()> {
        // match 表达式的穷尽性直接在 check_match 里强制（尾 `_` 臂），
        // 这里其余的表达式形态暂无可校验项。
        Ok(())
    }

    // ==================================================================
    // 结构体字面量 / 字段访问
    // ==================================================================

    fn check_struct_init(
        &mut self,
        path: &[String],
        fields: &[(String, Expression)],
        base: Option<&Expression>,
        expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        // 枚举变体的结构体载荷：`Shape::Circle { r: 3 }`。
        //
        // 路径的最后一段是变体名，前面的是枚举的类型路径。
        if path.len() >= 2 {
            if let Ok(enum_id) = self.resolve_path(&path[..path.len() - 1], span) {
                if matches!(
                    self.type_defs.get(&enum_id),
                    Some(hir::HirTypeDef::Enum(_))
                ) {
                    return self.check_enum_struct_variant_init(
                        enum_id, path, fields, base, expected, span,
                    );
                }
            }
        }
        let def_id = self.resolve_path(path, span)?;
        let raw_field_tys: Vec<(String, Ty)> = match self.type_defs.get(&def_id) {
            Some(hir::HirTypeDef::Struct(s)) => s
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.ty.clone()))
                .collect(),
            _ => {
                return Err(CompileError::type_error(format!(
                    "`{}` is not a struct",
                    path.join("::")
                ))
                .with_span(span))
            }
        };
        // 泛型实参：优先用注解 / 上下文给出的类型，否则从字段值推。
        let provided: Vec<Ty> = match self.infer.resolve(expected) {
            Ty::Named(id, a) if id == def_id => a,
            _ => Vec::new(),
        };
        let params = self
            .type_generic_params
            .get(&def_id)
            .cloned()
            .unwrap_or_default();
        let targs: Vec<Ty> = if params.is_empty() {
            Vec::new()
        } else if provided.len() == params.len() {
            provided
        } else {
            let mut out: Vec<Option<Ty>> = vec![None; params.len()];
            for (name, expr) in fields {
                if let Some(idx) = raw_field_tys.iter().position(|(n, _)| n == name) {
                    let aty = self.infer.fresh_infer(span);
                    let h = self.check_expr(expr, &aty)?;
                    // 用表达式**实际**的类型：`aty` 是期望类型，字面量检查
                    // 不一定把它绑定成 i32，拿它推泛型会得到未解推断变量。
                    let actual = self.infer.resolve(&h.ty);
                    self.bind_type_args(&raw_field_tys[idx].1, &actual, &params, &mut out, span)?;
                }
            }
            let mut args = Vec::with_capacity(params.len());
            for (i, o) in out.into_iter().enumerate() {
                match o {
                    Some(t) => args.push(self.infer.resolve(&t)),
                    None => {
                        return Err(CompileError::type_error(format!(
                            "cannot infer type parameter #{} for `{}`",
                            i,
                            path.join("::")
                        ))
                        .with_span(span))
                    }
                }
            }
            args
        };
        let field_tys: Vec<(String, Ty)> = raw_field_tys
            .iter()
            .map(|(n, t)| (n.clone(), self.subst_def_ty(def_id, &targs, t)))
            .collect();
        let target_ty = Ty::Named(def_id, targs);
        // 结构体更新语法 `..base`：为了不让 base 表达式被重复求值，目前要求
        // 它是位置表达式（局部量 / 字段 / 索引 / 解引用）。Drop 类型禁止
        // 更新——逐字段复制会重复析构同一份资源。
        let base_hir = if let Some(base) = base {
            if !is_struct_update_place(base) {
                return Err(CompileError::type_error(
                    "struct update base must be a place expression \
                     (a local, field, index, or dereference)",
                )
                .with_span(span));
            }
            if self.has_drop_glue(&target_ty, 0) {
                return Err(CompileError::type_error(
                    "struct update syntax is not supported for types with Drop glue",
                )
                .with_span(span));
            }
            let base = self.check_expr(base, &target_ty)?;
            self.infer.unify(&base.ty, &target_ty, span).ok();
            Some(base)
        } else {
            None
        };
        let mut seen = HashSet::new();
        let mut checked: Vec<(String, HirExpr)> = Vec::with_capacity(fields.len());
        for (name, expr) in fields {
            let field_idx = field_tys.iter().position(|(n, _)| n == name);
            let idx = match field_idx {
                Some(i) => i,
                None => {
                    return Err(CompileError::name_error(format!(
                        "no field `{}` on struct `{}`",
                        name,
                        path.join("::")
                    ))
                    .with_span(span));
                }
            };
            if seen.contains(name) {
                return Err(CompileError::name_error(format!(
                    "field `{}` specified more than once",
                    name
                ))
                .with_span(span));
            }
            seen.insert(name.clone());
            let fty = &field_tys[idx].1;
            let checked_expr = self.check_expr(expr, fty)?;
            self.check_partial_move(&checked_expr, expr.span())?;
            checked.push((name.clone(), checked_expr));
        }
        // 未写出的字段：普通字面量要求全部初始化，更新语法则从 base 读取。
        for (name, fty) in &field_tys {
            if seen.contains(name) {
                continue;
            }
            let Some(base) = &base_hir else {
                return Err(CompileError::type_error(format!(
                    "missing field `{}` in struct literal for `{}`",
                    name,
                    path.join("::")
                ))
                .with_span(span));
            };
            checked.push((
                name.clone(),
                HirExpr {
                    kind: ExprKind::Field(Box::new(base.clone()), name.clone()),
                    ty: fty.clone(),
                    span,
                },
            ));
        }
        Ok(HirExpr {
            kind: ExprKind::Struct(checked),
            ty: target_ty,
            span,
        })
    }

    /// 后缀 `?` 错误传播：在类型检查期脱糖成 `if 判别式 == 成功 { 载荷 }
    /// else { return 同型失败值 }`，MIR 层无需理解"错误传播"。
    ///
    /// 成功载荷走 `EnumField`（值型读取）——因为当前编译器对枚举载荷只有
    /// 一条取值路径，而这条路径是纯读取、不搬走判别式，所以不触发部分移动。
    /// 失败分支直接 `return` 一个与函数返回类型同型的失败值（不保留
    /// `Err(e)` 里的 `e`：错误类型目前不做 `From` 转换）。
    ///
    /// 载荷类型 `T` 以调用点的期望类型为准，没有注解时用推断变量。
    fn check_question(&mut self, inner: &Expression, expected: &Ty, span: ast::Span) -> Result<HirExpr> {
        // `infer_expr_ty` 是启发式的，看不到「枚举变体构造器调用」的类型：
        // `Result::Ok(1)?` 里外层会被推成一个未解的推断变量（路径没有函数
        // 签名可查）。但 `?` 按定义要求当前函数返回同一个枚举——错误类型不做
        // `From` 转换——所以下一行那个约束本就成立，用返回类型兜底，比在这里
        // 报「found `_`」有用得多（见 `tests/cases/69_question.zr` 的
        // `inferred_err`）。
        let inferred_ty = self.infer_expr_ty(inner)?;
        let src_ty = if matches!(self.infer.resolve(&inferred_ty), Ty::Infer(_)) {
            self.infer.return_ty.clone().unwrap_or(Ty::Unit)
        } else {
            inferred_ty
        };
        let src_ty = self.infer.resolve(&src_ty);
        let (enum_id, he) = match &src_ty {
            Ty::Named(id, _) => {
                let Some(hir::HirTypeDef::Enum(he)) = self.type_defs.get(id) else {
                    return Err(CompileError::internal("enum missing type definition")
                        .with_span(span));
                };
                (*id, he)
            }
            other => {
                return Err(CompileError::type_error(format!(
                    "`?` can only be used on `Option<T>` or `Result<T, E>`, found `{}`",
                    self.infer.type_name(other)
                ))
                .with_span(span));
            }
        };
        let Some((ok_idx, ok_name)) = he.variants.iter().enumerate().find_map(|(i, v)| {
            matches!(v.name.as_str(), "Some" | "Ok").then_some((i, v.name.clone()))
        }) else {
            return Err(CompileError::type_error(format!(
                "`{}` is not `Option` or `Result` (no `Some`/`Ok` variant)",
                he.name
            ))
            .with_span(span));
        };
        let fail_name = if ok_name == "Some" { "None" } else { "Err" };
        // 只需确认失败变体存在（`?` 依赖「存在成功/失败两种分支」这件事）；
        // 分支下标本身用不到——失败路径把整个值原样 `return`，不重建变体。
        let has_fail = he.variants.iter().any(|v| v.name == fail_name);
        if !has_fail {
            return Err(CompileError::type_error(format!(
                "`{}` has `{}` but no matching failure variant",
                he.name, ok_name
            ))
            .with_span(span));
        }
        // 当前函数必须返回同一个枚举（错误类型不做 `From` 转换）。
        let ret_ty = self.infer.return_ty.clone().unwrap_or(Ty::Unit);
        match &ret_ty {
            Ty::Named(id, _) if *id == enum_id => {}
            _ => {
                return Err(CompileError::type_error(format!(
                    "`?` requires the enclosing function to return `{}` (found `{}`)",
                    he.name,
                    self.infer.type_name(&ret_ty)
                ))
                .with_span(span));
            }
        }

        // 载荷类型：以期望类型为准（`let x: i32 = f()?`）。没有具体注解时，从
        // 成功变体的**真实字段类型**推出来——按当前枚举的类型实参做替换。
        //
        // 这里不能退回一个新的推断变量。在泛型函数里，`T` 自己的占位符是已经
        // 存在的、会被单态化替换的类型；新造的推断变量和它无关。同一个泛型
        // 函数里第二个 `?` 就拿到一个与 `T` 无关的类型，代码生成把值算错
        // （见 `tests/cases/69_question.zr` 的 `chain`：`id(x)?` 之后 `id(a)?`
        // 的结果不随 `a` 变；把 `let a` 注解成 `T` 就正常了，能证明就是这里）。
        let value_ty = match expected {
            Ty::Infer(_) => {
                let args = match &src_ty {
                    Ty::Named(_, a) => a.clone(),
                    _ => Vec::new(),
                };
                match self
                    .variant_field_types(enum_id, ok_idx)
                    .as_deref()
                    .and_then(|ts| ts.first())
                {
                    Some(t) => self.subst_def_ty(enum_id, &args, t),
                    None => Ty::Unit,
                }
            }
            other => other.clone(),
        };

        let expr = self.check_expr(inner, &src_ty.clone())?;
        let tag = HirExpr {
            kind: ExprKind::EnumTag(Box::new(expr.clone())),
            ty: self.enum_tag_type(enum_id),
            span,
        };
        let cond = HirExpr {
            kind: ExprKind::Binary(
                hir::BinaryOp::Eq,
                Box::new(tag),
                Box::new(HirExpr {
                    kind: ExprKind::Literal(hir::HirLiteral::Int(
                        self.variant_discriminant(enum_id, ok_idx),
                    )),
                    ty: self.enum_tag_type(enum_id),
                    span,
                }),
            ),
            ty: Ty::Bool,
            span,
        };

        // 成功分支：读载荷字段（不搬走判别式，不触发部分移动）。
        let payload = HirExpr {
            kind: ExprKind::EnumField {
                value: Box::new(expr.clone()),
                variant: ok_idx,
                index: 0,
            },
            ty: value_ty.clone(),
            span,
        };
        let then_block = hir::HirBlock {
            stmts: Vec::new(),
            tail: Some(Box::new(payload)),
        };

        // 失败分支：把**整个值**原样 `return` 出去（短路的错误传播）。
        //
        // 这里不能构造一个新的失败值，也不能只返回失败载荷：`Option::None`
        // 无载荷、`Result::Err(E)` 带一参，写 `Err()` 会在后者上直接报
        // 「构造器期望 1 个参数」；而返回载荷（`E`）又和函数返回类型
        // （`Result<T, E>`）不符。值本身就是失败值，直接传出去
        // （见 `tests/cases/69_question.zr` 的 `parse(x)?`）。
        let else_block = hir::HirBlock {
            stmts: Vec::new(),
            tail: Some(Box::new(HirExpr {
                kind: ExprKind::Return(Some(Box::new(expr.clone()))),
                ty: Ty::Never,
                span,
            })),
        };

        self.infer.unify(&value_ty, expected, span).ok();
        Ok(HirExpr {
            kind: ExprKind::If(Box::new(cond), then_block, Some(else_block)),
            ty: value_ty,
            span,
        })
    }

    /// 移动一个 Drop 字段时，禁止从直接实现 Drop 的类型（或引用/指针后面）
    /// 移出。Rust 在 E0509/E0507 中同样禁止这类部分移动。
    fn check_partial_move(&self, expr: &HirExpr, span: ast::Span) -> Result<()> {
        let ty = self.infer.resolve(&expr.ty);
        if !self.has_drop_glue(&ty, 0) {
            return Ok(());
        }
        let hir::ExprKind::Field(base, _) = &expr.kind else {
            return Ok(());
        };
        let base_ty = self.infer.resolve(&base.ty);
        let forbidden = match base_ty {
            Ty::Reference(..) | Ty::Pointer(..) => true,
            Ty::Named(id, _) => self.drop_impls.contains_key(&id),
            _ => false,
        };
        if forbidden {
            return Err(CompileError::type_error(
                "cannot move a Drop field out of a type that implements Drop \
                 (or out of borrowed content)",
            )
            .with_span(span));
        }
        Ok(())
    }

    /// 类型是否含任何 Drop glue：自身实现 Drop，或成员递归含 Drop。
    fn has_drop_glue(&self, ty: &Ty, depth: usize) -> bool {
        if depth > 16 {
            return true;
        }
        match ty {
            Ty::Named(id, _) => {
                if self.drop_impls.contains_key(id) {
                    return true;
                }
                let Some(hir::HirTypeDef::Struct(s)) = self.type_defs.get(id) else {
                    return false;
                };
                s.fields
                    .iter()
                    .any(|f| self.has_drop_glue(&f.ty, depth + 1))
            }
            Ty::Array(elem, _) => self.has_drop_glue(elem, depth + 1),
            _ => false,
        }
    }

    /// 类型是否"移动语义"（非 `Copy`）：含 drop glue 的类型赋值/传参是移动
    /// 不是复制。不含 drop glue 的类型（标量、纯值结构体）按复制语义，重复
    /// 使用合法。
    ///
    /// 这里以"是否含析构"为近似：zr 的 `Copy` 标记 trait 只登记不参与
    /// 单态化决策（`src/typeck/derive.rs` 的 `generate_derive_impls` 对
    /// `Copy` 只插标记 impl），而 MIR 的 `is_drop_ty` 正是用 drop glue 判
    /// 移动/复制（`builder.rs` `let b = a` 对 Drop 类型 `mark_moved`）。与
    /// MIR 保持同一判据，移动检查才不会跟后端生成对不上。
    fn is_move_ty(&self, ty: &Ty) -> bool {
        self.has_drop_glue(&self.infer.resolve(ty), 0)
    }

    fn check_field_access(&self, ty: &Ty, name: &str, span: ast::Span) -> Result<(Ty, FieldIndex)> {
        match ty {
            Ty::Named(id, args) => {
                // 字段类型必须按结构体的类型实参替换（`Vec<T>.data` 在
                // `Vec<i32>` 上是 `*mut i32`，不是 `*mut T`）。
                if let Some((f, index)) = self
                    .struct_field_tys(*id, args)
                    .into_iter()
                    .find(|(n, _, _)| n == name)
                    .map(|(_, f, index)| (f, index))
                {
                    return Ok((f, FieldIndex::Named(index)));
                }
                Err(CompileError::name_error(format!(
                    "no field `{}` on type `{}`",
                    name,
                    self.infer.type_name(ty)
                ))
                .with_span(span))
            }
            Ty::Tuple(tys) => {
                if let Ok(idx) = name.parse::<usize>() {
                    if idx < tys.len() {
                        return Ok((tys[idx].clone(), FieldIndex::Pos(idx)));
                    }
                }
                Err(
                    CompileError::type_error(format!("tuple index out of range: {}", name))
                        .with_span(span),
                )
            }
            Ty::Reference(inner, _) | Ty::Pointer(inner, _) => {
                self.check_field_access(inner, name, span)
            }
            // 数组的长度是编译期常量。
            Ty::Array(..) => match name {
                "len" => Ok((Ty::Uint(64), FieldIndex::Named(0))),
                _ => Err(CompileError::name_error(format!(
                    "no field `{}` on array (only `len`)",
                    name
                ))
                .with_span(span)),
            },
            // 切片 / 字符串在运行期是 (ptr, len) 一对，可以取这两个字段。
            Ty::Str => match name {
                "ptr" => Ok((
                    Ty::Pointer(Box::new(Ty::Uint(8)), hir::Mutability::Immutable),
                    FieldIndex::Named(0),
                )),
                "len" => Ok((Ty::Uint(64), FieldIndex::Named(1))),
                _ => Err(CompileError::name_error(format!(
                    "no field `{}` on slice (only `ptr` and `len`)",
                    name
                ))
                .with_span(span)),
            },
            Ty::Slice(elem, mutability) => match name {
                "ptr" => Ok((
                    Ty::Pointer(elem.clone(), *mutability),
                    FieldIndex::Named(0),
                )),
                "len" => Ok((Ty::Uint(64), FieldIndex::Named(1))),
                _ => Err(CompileError::name_error(format!(
                    "no field `{}` on slice (only `ptr` and `len`)",
                    name
                ))
                .with_span(span)),
            },
            _ => Err(CompileError::type_error(format!(
                "cannot access field `{}` on type `{}`",
                name,
                self.infer.type_name(ty)
            ))
            .with_span(span)),
        }
    }

    // ==================================================================
    // 字面量 / 模式
    // ==================================================================

    fn check_literal(&self, lit: &AstLiteral, expected: &Ty) -> Result<(ExprKind, Ty)> {
        Ok(match lit {
            AstLiteral::Int(v, suffix) => {
                let ty = match suffix.as_deref().and_then(|s| self.int_suffix_ty(s)) {
                    Some(t) => t,
                    None => {
                        // 默认推断：尝试从 expected 推断
                        if let Ty::Int(w) = expected {
                            Ty::Int(*w)
                        } else if let Ty::Uint(w) = expected {
                            Ty::Uint(*w)
                        } else {
                            Ty::Int(32)
                        }
                    }
                };
                (ExprKind::Literal(hir::HirLiteral::Int(*v)), ty)
            }
            AstLiteral::Float(v, suffix) => {
                let ty = match suffix.as_deref() {
                    Some("f8") => Ty::Float(8),
                    Some("f16") => Ty::Float(16),
                    Some("f32") => Ty::Float(32),
                    Some("f64") => Ty::Float(64),
                    // 无后缀时从 expected 推断——与整数字面量一致。
                    // 少了这一条，`let x: f32 = 1.5;` 会把 1.5 判成 f64，
                    // 然后与注解 f32 冲突，f32 在这个语言里就用不了。
                    _ => match expected {
                        Ty::Float(w) => Ty::Float(*w),
                        _ => Ty::Float(64),
                    },
                };
                (ExprKind::Literal(hir::HirLiteral::Float(*v)), ty)
            }
            ast::Literal::String(s) => (
                ExprKind::Literal(hir::HirLiteral::String(s.clone())),
                Ty::Str,
            ),
            ast::Literal::Char(c) => (ExprKind::Literal(hir::HirLiteral::Char(*c)), Ty::Char),
            ast::Literal::Bool(b) => (ExprKind::Literal(hir::HirLiteral::Bool(*b)), Ty::Bool),
            AstLiteral::Byte(v) => (
                ExprKind::Literal(hir::HirLiteral::Int(*v as i128)),
                Ty::Uint(8),
            ),
            AstLiteral::ByteString(bytes) => (
                ExprKind::Literal(hir::HirLiteral::String(
                    bytes.iter().map(|b| *b as char).collect(),
                )),
                Ty::Slice(Box::new(Ty::Uint(8)), ast::Mutability::Immutable),
            ),
        })
    }

    fn lower_pattern(&mut self, p: &AstPattern, ty: &Ty) -> Result<hir::HirPattern> {
        match p {
            AstPattern::Wildcard(_) => Ok(hir::HirPattern::Wildcard),
            AstPattern::Literal(lit) => {
                let (kind, _) = self.check_literal(lit, ty)?;
                // 只支持能用一个 i128 表示的模式值（整数 / bool / char）。
                // 浮点模式不缩窄成 i128（位模式与值语义不同），字符串字面量
                // 模式是地址——都报"未支持"，不能落在通配符里装作匹配。
                match kind {
                    ExprKind::Literal(hir::HirLiteral::Int(v)) => Ok(hir::HirPattern::Literal(v)),
                    ExprKind::Literal(hir::HirLiteral::Bool(b)) => {
                        Ok(hir::HirPattern::Literal(b as i128))
                    }
                    ExprKind::Literal(hir::HirLiteral::Char(c)) => {
                        Ok(hir::HirPattern::Literal(c as i128))
                    }
                    ExprKind::Literal(hir::HirLiteral::Float(_))
                    | ExprKind::Literal(hir::HirLiteral::String(_)) => {
                        // 浮点模式不能用位模式比较，字符串模式是地址。
                        // 报"未支持"，不能落进通配符里装作匹配成功。
                        Err(CompileError::type_error(
                            "match on a float / string pattern is not supported (only integer / char / bool literals)",
                        )
                        .with_span(ast::Span::unknown()))
                    }
                    _ => Err(CompileError::type_error(
                        "unsupported literal kind in a match pattern",
                    )
                    .with_span(ast::Span::unknown())),
                }
            }
            AstPattern::Binding(name, mutability, sub, _) => {
                let (_, local) = self.bind_local(name, ty);
                if matches!(mutability, ast::Mutability::Mutable) {
                    self.mutable_locals.insert(local);
                }
                if let Some(s) = sub {
                    let _ = self.lower_pattern(s, ty)?;
                    Ok(hir::HirPattern::Ref)
                } else {
                    Ok(hir::HirPattern::Binding(local))
                }
            }
            AstPattern::Tuple(pats, _) => {
                let elem_tys = match ty {
                    Ty::Tuple(ts) => ts.clone(),
                    _ => Vec::new(),
                };
                let hir_pats: Result<Vec<_>> = pats
                    .iter()
                    .enumerate()
                    .map(|(i, p)| self.lower_pattern(p, elem_tys.get(i).unwrap_or(ty)))
                    .collect();
                Ok(hir::HirPattern::Tuple(hir_pats?))
            }
            AstPattern::Struct {
                path,
                fields,
                rest,
                span,
            } => {
                // 枚举变体的结构体载荷模式：`Shape::Circle { r: 3 }`。
                if path.len() >= 2 {
                    if let Ok(enum_id) = self.resolve_path(&path[..path.len() - 1], *span) {
                        if matches!(
                            self.type_defs.get(&enum_id),
                            Some(hir::HirTypeDef::Enum(_))
                        ) {
                            return self.lower_enum_struct_variant_pattern(
                                enum_id, path, fields, *rest, ty, *span,
                            );
                        }
                    }
                }
                let def_id = self.resolve_path(path, *span)?;
                let hir_fields: Result<Vec<_>> = fields
                    .iter()
                    .map(|fp| {
                        let fty = self.field_type(def_id, &fp.name, *span)?;
                        let sub = self.lower_pattern(&fp.pattern, &fty)?;
                        Ok((fp.name.clone(), fty, sub))
                    })
                    .collect();
                let _ = rest;
                Ok(hir::HirPattern::Struct(hir_fields?))
            }
            AstPattern::Variant { path, args, span } => {
                // 裸变体模式（`Some(v)`）：路径只有变体名本身，宿主枚举靠
                // `variant_aliases`（`use core::Option::Some` 时登记）找。
                // 限定路径（`Option::Some(v)` / `Opt::<i32>::Some(v)`）照旧
                // 走 `resolve_path`。
                let def_id = if path.len() == 1 {
                    match self.variant_aliases.get(&path[0]) {
                        Some(id) => *id,
                        None => {
                            return Err(CompileError::name_error(format!(
                                "cannot find enum variant `{}` in this scope (import it via its enum type, e.g. `use core::Option::Some`)",
                                path[0]
                            ))
                            .with_span(*span));
                        }
                    }
                } else {
                    self.resolve_path(&path[..path.len() - 1], *span)?
                };
                let variant_name = path.last().unwrap();
                let variant_idx = self.variant_index(def_id, variant_name);
                let payload_tys = self.variant_payload_tys(def_id, variant_idx, *span)?;
                // 同上：载荷类型是声明里的，泛型参数要按 scrutinee 的实际
                // 类型实参替换，否则 `Opt::<u8>::Some(v)` 的 `v` 是 `_`。
                let payload_tys: Vec<Ty> = payload_tys
                    .iter()
                    .map(|t| self.subst_variant_field_ty(def_id, ty, t))
                    .collect();
                let hir_args: Result<Vec<_>> = args
                    .iter()
                    .enumerate()
                    .map(|(i, p)| self.lower_pattern(p, payload_tys.get(i).unwrap_or(ty)))
                    .collect();
                Ok(hir::HirPattern::Variant {
                    index: variant_idx,
                    args: hir_args?,
                })
            }
            AstPattern::Slice(pats, _) => {
                let hir_pats: Result<Vec<_>> = pats
                    .iter()
                    .map(|p| self.lower_pattern(p, ty))
                    .collect();
                let _ = hir_pats?;
                Ok(hir::HirPattern::Slice)
            }
            AstPattern::Range(start, end, inclusive, _) => {
                // 范围两端的类型要与被匹配的值一致（字面量也因此拿到正确的
                // 类型，而不是各自推断成默认的 i32 / f64）。
                let s = self.check_expr(start, ty)?;
                let en = self.check_expr(end, ty)?;
                Ok(hir::HirPattern::Range {
                    start: Box::new(s),
                    end: Box::new(en),
                    inclusive: *inclusive,
                })
            }
            AstPattern::Ref(inner, mutability, _) => {
                let _ = self.lower_pattern(inner, ty)?;
                let _ = mutability;
                Ok(hir::HirPattern::Ref)
            }
            AstPattern::Or(_) => {
                // 或模式需要拆成多条分支条件，只有 match 能做到；其他位置
                // （let / 函数参数）没有"多选一"的语义容量。
                Err(
                    CompileError::type_error("or-patterns are only supported in match arms")
                        .with_span(ast::Span::unknown()),
                )
            }
        }
    }

    fn field_type(&self, def_id: DefId, name: &str, span: ast::Span) -> Result<Ty> {
        if let Some(hir::HirTypeDef::Struct(s)) = self.type_defs.get(&def_id) {
            if let Some(f) = s.fields.iter().find(|f| f.name == name) {
                return Ok(f.ty.clone());
            }
        }
        Err(CompileError::name_error(format!("no field `{}`", name)).with_span(span))
    }

    /// 枚举的判别式类型。
    ///
    /// `Auto` 按变体个数挑最小的够用的整数（和 Rust 一样）。
    fn enum_tag_type(&self, def_id: DefId) -> Ty {
        let Some(hir::HirTypeDef::Enum(he)) = self.type_defs.get(&def_id) else {
            return Ty::Int(32);
        };
        match he.repr {
            hir::IntegerRepr::I8 => Ty::Int(8),
            hir::IntegerRepr::I16 => Ty::Int(16),
            hir::IntegerRepr::I32 => Ty::Int(32),
            hir::IntegerRepr::I64 | hir::IntegerRepr::I128 => Ty::Int(64),
            hir::IntegerRepr::Auto => match he.variants.len() {
                0..=256 => Ty::Int(8),
                257..=65536 => Ty::Int(16),
                _ => Ty::Int(32),
            },
        }
    }

    /// 查一个泛型参数的类型。
    fn lookup_type_param(&self, name: &str) -> Option<Ty> {
        for scope in self.type_params.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty.clone());
            }
        }
        None
    }

    /// 绑定一个具名类型（`Self` → 实现的目标类型）。
    fn push_named_type(&mut self, name: &str, ty: Ty) {
        let mut scope = HashMap::new();
        scope.insert(name.to_string(), ty);
        self.type_params.push(scope);
    }

    /// 调用表达式是不是直接指向一个可单态化的泛型函数。
    fn generic_call_target(&self, callee: &Expression) -> Option<DefId> {
        if let Expression::Variable(name, _) = callee {
            let id = self.symbols.get(name)?;
            if self.generic_names.contains_key(id) {
                return Some(*id);
            }
        }
        // `Vec::new` 这类**路径**调用：早先只认 `Variable`，`Vec::new()` 的
        // 快速推断会走 `infer_expr_ty(Path)` 拿到**未替换**的泛型签名
        // （`Vec<T>` 里的 `T` 是共享占位符），`let req = Vec::new()` 就绑到
        // 共享占位符上；同文件里另一个 `Vec<SocketAddr>` 实例化把占位符绑成
        // `SocketAddr`，`req` 泄漏成 `Vec<SocketAddr>`（`net.zr` 的
        // `req.push(buf[m])` 报「T inferred as both SocketAddr and u8」）。
        if let Expression::Path(path, _) = callee {
            if let Ok(id) = self.resolve_path(path, ast::Span::unknown()) {
                if self.generic_names.contains_key(&id) {
                    return Some(id);
                }
            }
        }
        None
    }

    /// 记录一个泛型类型定义的类型参数占位符（按声明顺序）。
    fn record_type_generics(
        &mut self,
        def_id: DefId,
        generics: &[ast::GenericParam],
        scope: &HashMap<String, Ty>,
    ) {
        let params: Vec<Ty> = generics
            .iter()
            .filter_map(|g| match g {
                ast::GenericParam::Type(n, _) => scope.get(n).cloned(),
                _ => None,
            })
            .collect();
        if !params.is_empty() {
            self.type_generic_params.insert(def_id, params);
        }
    }

    /// 用 `args` 替换类型定义 `def_id` 里的泛型占位符。
    fn subst_def_ty(&self, def_id: DefId, args: &[Ty], ty: &Ty) -> Ty {
        let Some(params) = self.type_generic_params.get(&def_id) else {
            return ty.clone();
        };
        if params.len() != args.len() {
            return ty.clone();
        }
        let map: HashMap<InferId, Ty> = params
            .iter()
            .zip(args)
            .filter_map(|(p, a)| match p {
                Ty::Infer(id) => Some((*id, a.clone())),
                _ => None,
            })
            .collect();
        self.subst_infer(ty, &map)
    }

    /// 把枚举变体载荷字段的**声明**类型按 scrutinee 的实际类型实参替换。
    ///
    /// `variant_field_types` 返回的是声明里写的东西：泛型枚举的载荷是
    /// `Ty::Infer(T 的占位符)`。模式绑定必须用具体化之后的类型——
    /// `Opt::<u8>::Some(v)` 的 `v` 是 `u8`，不是那个占位符。
    ///
    /// 早先这里直接用声明类型：不加类型标注时 `if d != 9` 之类的比较会
    /// 顺手把声明里的 `Ty::Infer` 绑到 `i32` 上（AGENTS 红线 2 的污染），
    /// 于是"碰巧能跑"；一旦 scrutinee 是 `Opt<u8>` 或者显式 turbofish，
    /// `v` 就留成一个没人绑过的 `_`，报 `cannot cast '_' to 'i32'`。
    fn subst_variant_field_ty(&self, def_id: DefId, scrut_ty: &Ty, ty: &Ty) -> Ty {
        match self.infer.resolve(scrut_ty) {
            Ty::Named(id, targs) if id == def_id => self.subst_def_ty(def_id, &targs, ty),
            _ => ty.clone(),
        }
    }

    /// 把类型里出现的占位符 `InferId` 替换成具体类型。
    fn subst_infer(&self, ty: &Ty, map: &HashMap<InferId, Ty>) -> Ty {
        match ty {
            Ty::Infer(id) => map.get(id).cloned().unwrap_or_else(|| ty.clone()),
            Ty::Named(id, args) => Ty::Named(
                *id,
                args.iter().map(|t| self.subst_infer(t, map)).collect(),
            ),
            Ty::Pointer(t, m) => Ty::Pointer(Box::new(self.subst_infer(t, map)), *m),
            Ty::Reference(t, m) => Ty::Reference(Box::new(self.subst_infer(t, map)), *m),
            Ty::Array(t, len) => Ty::Array(Box::new(self.subst_infer(t, map)), len.clone()),
            Ty::Slice(t, m) => Ty::Slice(Box::new(self.subst_infer(t, map)), *m),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.subst_infer(t, map)).collect()),
            Ty::Function {
                params,
                ret,
                is_async,
                abi,
            } => Ty::Function {
                params: params.iter().map(|t| self.subst_infer(t, map)).collect(),
                ret: Box::new(self.subst_infer(ret, map)),
                is_async: *is_async,
                abi: abi.clone(),
            },
            Ty::Optional(t) => Ty::Optional(Box::new(self.subst_infer(t, map))),
            Ty::ErrorUnion(t, e) => Ty::ErrorUnion(
                Box::new(self.subst_infer(t, map)),
                Box::new(self.subst_infer(e, map)),
            ),
            Ty::Pin(t) => Ty::Pin(Box::new(self.subst_infer(t, map))),
            _ => ty.clone(),
        }
    }

    /// 把某个 trait 展开成它自己 + 全部超类型（含传递）。
    ///
    /// `trait Ord: Eq` 意味着实现 `Ord` 蕴含实现 `Eq`。查 `T: Eq` 时，
    /// `T` 实现了 `Ord` 也算满足——所以求解必须把超类型算进来，否则
    /// `max3<T: Ord>` 里对 `Ord` 的检查会漏掉 `Ord: Eq` 那层。
    fn expand_trait(&self, trait_id: DefId) -> Vec<DefId> {
        let mut out = Vec::new();
        let mut stack = vec![trait_id];
        while let Some(id) = stack.pop() {
            if out.contains(&id) {
                continue;
            }
            out.push(id);
            if let Some(hir::HirTrait { super_traits, .. }) = self.trait_defs.get(&id) {
                for st in super_traits {
                    if !out.contains(st) {
                        stack.push(*st);
                    }
                }
            }
        }
        out
    }

    /// 某个类型是否实现了某个 trait（含超类型传递、含 `#[derive]`）。
    ///
    /// 必须做在 `TypeChecker` 上，不能放在 `InferCtxt`：`self.infer` 是
    /// `InferCtxt<'static>`，它的 `impl_blocks` / `type_defs` 一直是空静态表
    /// （见字段定义处的注释），`check_trait_impl` 里那段扫 impl 的代码从来没
    /// 拿到过真实数据——这正是 `needs_clone<Nope>` 能编过的原因。
    ///
    /// 方向是「每个直接实现的 trait 往上找超类型」，不能反过来。
    /// `trait Ord: Eq` 下，实现了 `Ord` 的类型满足 `T: Eq`（`Eq` 是 `Ord` 的
    /// 超类型）；反过来 `T: Ord` 绝不能被只有 `Eq` 的类型满足。早先这里把
    /// **目标** trait 往上展开成 `{Ord, Eq}` 再判"实现其中任意一个"，于是
    /// `fn id<T: Ord>` 配一个只实现了 `Eq` 的类型能静默编过——边界被削弱，
    /// 是类型安全漏洞。
    fn implements_trait(&self, ty: &Ty, trait_id: DefId) -> bool {
        for id in self.direct_impls(ty) {
            if self.expand_trait(id).contains(&trait_id) {
                return true;
            }
        }
        false
    }

    /// 某个类型**直接**实现了哪些 trait：手写 `impl` 块 + `#[derive]`。
    ///
    /// 不含超类型传递（那是调用方按需要展开的），否则"实现了什么"和"边界算
    /// 不算满足"会混在一起。
    fn direct_impls(&self, ty: &Ty) -> Vec<DefId> {
        let mut out = Vec::new();
        for ib in &self.impl_blocks {
            if !self.impl_target_matches(ib, ty) {
                continue;
            }
            // 手写 impl：`trait_id` 直接可用。
            if let Some(tid) = ib.trait_id {
                if !out.contains(&tid) {
                    out.push(tid);
                }
                continue;
            }
            // `#[derive(Clone)]` 生成的 impl 块 `trait_id` 是 `None`（属性 args
            // 里只有字符串），得按名字认。只认真正写出来的 trait，不能一见
            // `derive` 属性就算满足——那样 derive 能满足**任何** trait。
            let Ty::Named(tid, _) = self.resolve_ty(&ib.target) else {
                continue;
            };
            let attrs = match self.type_defs.get(&tid) {
                Some(hir::HirTypeDef::Struct(s)) => &s.attrs,
                Some(hir::HirTypeDef::Enum(e)) => &e.attrs,
                _ => continue,
            };
            for attr in attrs.iter().filter(|a| a.name == "derive") {
                for name in &attr.args {
                    if let Some(def) = self.trait_defs_by_name.get(name) {
                        if !out.contains(def) {
                            out.push(*def);
                        }
                    }
                }
            }
        }
        out
    }

    /// 判断某个 impl 块的目标类型是否适用于给定类型。
    ///
    /// 不能无条件返回 `true`：那样"trait 有没有被实现过"和"这个类型实现了它"
    /// 混为一谈——`impl Ord for i32` 存在，于是任何类型都"实现"了 `Ord`，
    /// `max3(SomeTypeWithoutOrd)` 静默通过，是类型安全漏洞。
    ///
    /// 两条路径：
    /// - **单态 impl**（`impl Ord for i32`、`impl Eq for Pair<i32>`）：目标
    ///   解析完就是最终类型，直接比结构即可。基元类型（`i32` / `u64` / `f64`）
    ///   是 `Ty::Int` / `Ty::Uint` / `Ty::Float`，**不是** `Ty::Named`，所以
    ///   不能先拆 `Ty::Named` 再比——必须先解析、再比整体，否则
    ///   `impl Ord for i32` 永远匹配不上，`max3(3, 9, 5)` 反而编不过。
    /// - **泛型 impl**（`impl<T> Future for Ready<T>`）：目标里是未绑定的
    ///   `Infer` 占位符（`fresh_infer` 自指，解析不出来）。按位置绑定到实参的
    ///   一级实参，替换后比对剩余结构。
    fn impl_target_matches(&self, ib: &hir::HirImpl, ty: &Ty) -> bool {
        let ty = self.resolve_ty(ty);
        let impl_ty = self.resolve_ty(&ib.target);
        // 单态 impl（含 `impl Ord for i32` 这类基元目标）：解析后结构相等即匹配。
        // 基元是 `Ty::Int` / `Ty::Uint` / `Ty::Float`，不是 `Ty::Named`，所以必须
        // 先比整体结构，不能先拆 `Ty::Named` 再比——否则 `impl Ord for i32`
        // 永远匹配不上，`max3(3, 9, 5)` 反而编不过。
        if impl_ty == ty {
            return true;
        }
        // 泛型 impl（`impl<T> Future for Ready<T>`）：目标里留着未绑定的 `Infer`
        // 占位符，上面整体比必然不等，改按位置绑定一级实参后重比。
        let Ty::Named(impl_id, ref impl_args) = impl_ty else {
            return false;
        };
        let Ty::Named(ty_id, ref ty_args) = ty else {
            return false;
        };
        if ty_id != impl_id {
            return false;
        }
        let params = self
            .type_generic_params
            .get(&impl_id)
            .cloned()
            .unwrap_or_default();
        if params.len() != ty_args.len() {
            return false;
        }
        let map: HashMap<InferId, Ty> = params
            .iter()
            .zip(ty_args.iter())
            .filter_map(|(p, a)| match p {
                Ty::Infer(id) => Some((*id, a.clone())),
                _ => None,
            })
            .collect();
        impl_args.iter().zip(ty_args.iter()).all(|(exp, got)| {
            self.subst_infer(exp, &map) == *got
        })
    }

    /// 解析推断占位符（这里只做结构解析，不查外部表）。
    ///
    /// 只对已经绑定的推断变量做解引用，**解不开的原样保留**。
    /// `fresh_infer` 建出的 `Ty::Infer(id)` 在 `vars` 里自指
    /// （`vars[id] = Ty::Infer(id)`），无脑递归会顺着这条自环无限下去
    /// 直到栈溢出——见 `InferCtxt::resolve` 里的同类判断。
    fn resolve_ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Infer(id) => match self.infer.vars.get(id) {
                Some(v) if !matches!(v, Ty::Infer(_)) => self.resolve_ty(v),
                _ => ty.clone(),
            },
            Ty::Named(id, args) => Ty::Named(
                *id,
                args.iter().map(|t| self.resolve_ty(t)).collect(),
            ),
            _ => ty.clone(),
        }
    }

    /// 校验泛型函数被单态化时的 trait 边界。
    ///
    /// 单态化点拿到全部具体实参，是唯一能验证"实参真的满足 `T: Ord`"的时刻。
    /// 边界没被验证过，`fn f<T: Ord>` 里的 `Ord` 就等于没写。
    fn check_trait_bounds(
        &self,
        base: DefId,
        ordered: &[Ty],
        span: ast::Span,
    ) -> Result<()> {
        let names = match self.generic_names.get(&base) {
            Some(n) => n.clone(),
            None => return Ok(()),
        };
        let bounds = match self.generic_bounds.get(&base) {
            Some(b) => b.clone(),
            None => return Ok(()),
        };
        for (i, param_name) in names.iter().enumerate() {
            if i >= ordered.len() {
                continue;
            }
            for trait_id in &bounds[i] {
                if !self.implements_trait(&ordered[i], *trait_id) {
                    return Err(CompileError::type_error(format!(
                        "`{}` does not implement trait `{}` (required by `{param_name}`)",
                        self.infer.type_name(&ordered[i]),
                        self.def_names.get(trait_id).cloned().unwrap_or_default()
                    ))
                    .with_span(span));
                }
            }
        }
        Ok(())
    }

    /// 结构体的字段类型（按 `args` 替换泛型参数）。
    fn struct_field_tys(&self, def_id: DefId, args: &[Ty]) -> Vec<(String, Ty, usize)> {
        let Some(hir::HirTypeDef::Struct(s)) = self.type_defs.get(&def_id) else {
            return Vec::new();
        };
        s.fields
            .iter()
            .map(|f| (f.name.clone(), self.subst_def_ty(def_id, args, &f.ty), f.index))
            .collect()
    }

    /// 类型在运行期是否以"指向存储的地址"表示。
    fn ty_is_aggregate(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Tuple(_) | Ty::Array(..) | Ty::Slice(..) | Ty::Str => true,
            Ty::Named(id, _) => matches!(
                self.type_defs.get(id),
                Some(hir::HirTypeDef::Struct(_)) | Some(hir::HirTypeDef::Enum(_))
            ),
            _ => false,
        }
    }

    /// 枚举变体的载荷类型（按 `args` 替换泛型参数）。
    fn variant_field_types_args(
        &self,
        def_id: DefId,
        index: usize,
        args: &[Ty],
    ) -> Option<Vec<Ty>> {
        self.variant_field_types(def_id, index)
            .map(|ts| ts.iter().map(|t| self.subst_def_ty(def_id, args, t)).collect())
    }
    /// 把"含占位符的字段类型"和实际值类型做结构匹配，填 `out`。
    fn bind_type_args(
        &mut self,
        pat: &Ty,
        actual: &Ty,
        params: &[Ty],
        out: &mut Vec<Option<Ty>>,
        span: ast::Span,
    ) -> Result<()> {
        let actual = self.infer.resolve(actual);
        if let Ty::Infer(id) = pat {
            if let Some(i) = params
                .iter()
                .position(|p| matches!(p, Ty::Infer(pid) if pid == id))
            {
                match out[i].clone() {
                    Some(prev) => {
                        self.infer.unify(&prev, &actual, span)?;
                    }
                    None => out[i] = Some(actual),
                }
            }
            return Ok(());
        }
        match (pat, &actual) {
            (Ty::Pointer(p, _), Ty::Pointer(a, _))
            | (Ty::Pointer(p, _), Ty::Reference(a, _))
            | (Ty::Reference(p, _), Ty::Pointer(a, _))
            | (Ty::Reference(p, _), Ty::Reference(a, _))
            | (Ty::Slice(p, _), Ty::Slice(a, _))
            | (Ty::Array(p, _), Ty::Array(a, _))
            | (Ty::Optional(p), Ty::Optional(a))
            | (Ty::Pin(p), Ty::Pin(a)) => self.bind_type_args(p, a, params, out, span),
            (Ty::ErrorUnion(p1, p2), Ty::ErrorUnion(a1, a2)) => {
                self.bind_type_args(p1, a1, params, out, span)?;
                self.bind_type_args(p2, a2, params, out, span)
            }
            (Ty::Tuple(ps), Ty::Tuple(as_)) => {
                for (p, a) in ps.iter().zip(as_) {
                    self.bind_type_args(p, a, params, out, span)?;
                }
                Ok(())
            }
            (Ty::Named(pd, pa), Ty::Named(ad, aa)) if pd == ad => {
                for (p, a) in pa.iter().zip(aa) {
                    self.bind_type_args(p, a, params, out, span)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// 由字段 / 载荷的实参类型推断类型定义的泛型实参。
    ///
    /// `provided` 是显式给出的（比如 `let x: Option<i32>` 的注解）；为空时
    /// 从 `pats[i]` 与 `actuals[i]` 的结构匹配里推。
    fn collect_type_args(
        &mut self,
        def_id: DefId,
        provided: &[Ty],
        pats: &[Ty],
        actuals: &[Ty],
        span: ast::Span,
    ) -> Result<Vec<Ty>> {
        let params = self
            .type_generic_params
            .get(&def_id)
            .cloned()
            .unwrap_or_default();
        if params.is_empty() {
            return Ok(Vec::new());
        }
        if provided.len() == params.len() {
            return Ok(provided.to_vec());
        }
        let mut out: Vec<Option<Ty>> = vec![None; params.len()];
        for (p, a) in pats.iter().zip(actuals) {
            self.bind_type_args(p, a, &params, &mut out, span)?;
        }
        let mut args = Vec::with_capacity(params.len());
        for o in out {
            match o {
                Some(t) => args.push(self.infer.resolve(&t)),
                // 推不出来的参数先留一个推断变量：`Option::None` 自己没有
                // 载荷，`T` 要靠上下文（同一调用的其它实参 / 返回值）定。
                None => args.push(self.infer.fresh_infer(span)),
            }
        }
        Ok(args)
    }

    /// 把一组"泛型参数名 → 具体类型"压进类型作用域。
    fn push_type_scope(&mut self, subst: &[(String, Ty)]) {
        let scope: HashMap<String, Ty> = subst.iter().cloned().collect();
        self.type_params.push(scope);
    }

    /// 用实参类型去匹配声明里的参数类型，把泛型参数填进 `subst`。
    ///
    /// 只做结构匹配：`T` 绑定实参类型，`*mut T` / `[]T` / `Option<T>` 这类
    /// 复合类型递归下去。非泛型部分只要求形如即可，不在这里做真正的类型
    /// 相等检查（交给后续 `check_expr` / `unify`）。
    fn match_generic_ast(
        &mut self,
        pat: &AstType,
        actual: &Ty,
        names: &[String],
        subst: &mut HashMap<String, Ty>,
        span: ast::Span,
    ) -> Result<()> {
        let actual = self.infer.resolve(actual);
        match pat {
            AstType::Named { path, generics }
                if path.len() == 1 && generics.is_empty() && names.iter().any(|n| n == &path[0]) =>
            {
                let name = &path[0];
                match subst.get(name).cloned() {
                    // 重复出现的参数（`min(a: T, b: T)`）：两次实参必须一致。
                    Some(prev) => {
                        let prev = self.infer.resolve(&prev);
                        self.infer.unify(&prev, &actual, span).map_err(|_| {
                            CompileError::type_error(format!(
                                "type parameter `{name}` inferred as both `{}` and `{}`",
                                self.infer.type_name(&prev),
                                self.infer.type_name(&actual)
                            ))
                            .with_span(span)
                        })?;
                    }
                    None => {
                        subst.insert(name.clone(), actual);
                    }
                }
                Ok(())
            }
            AstType::Named { generics, .. } => {
                if let Ty::Named(_, args) = &actual {
                    for (g, a) in generics.iter().zip(args) {
                        self.match_generic_ast(g, a, names, subst, span)?;
                    }
                }
                Ok(())
            }
            AstType::Pointer(inner, _) => {
                if let Ty::Pointer(a, _) = &actual {
                    self.match_generic_ast(inner, a, names, subst, span)?;
                }
                Ok(())
            }
            AstType::Reference(inner, _) => {
                // `&mut x` 表达式的类型是 `Ty::Pointer`，而参数类型 `&mut T`
                // 是 `Ty::Reference` —— 两者按同一形状匹配（`subtype` 也是
                // 这么放宽的）。
                match &actual {
                    Ty::Reference(a, _) | Ty::Pointer(a, _) => {
                        self.match_generic_ast(inner, a, names, subst, span)?;
                    }
                    _ => {}
                }
                Ok(())
            }
            AstType::Array(inner, _) => {
                if let Ty::Array(a, _) = &actual {
                    self.match_generic_ast(inner, a, names, subst, span)?;
                }
                Ok(())
            }
            AstType::Slice(inner, _) => {
                match &actual {
                    Ty::Slice(a, _) | Ty::Array(a, _) => {
                        self.match_generic_ast(inner, a, names, subst, span)?;
                    }
                    _ => {}
                }
                Ok(())
            }
            AstType::Optional(inner) => {
                if let Ty::Optional(a) = &actual {
                    self.match_generic_ast(inner, a, names, subst, span)?;
                }
                Ok(())
            }
            AstType::ErrorUnion(t, e) => {
                if let Ty::ErrorUnion(a, b) = &actual {
                    self.match_generic_ast(t, a, names, subst, span)?;
                    self.match_generic_ast(e, b, names, subst, span)?;
                }
                Ok(())
            }
            AstType::Tuple(tys) => {
                if let Ty::Tuple(args) = &actual {
                    for (t, a) in tys.iter().zip(args) {
                        self.match_generic_ast(t, a, names, subst, span)?;
                    }
                }
                Ok(())
            }
            AstType::Function { params, ret, .. } => {
                if let Ty::Function {
                    params: aparams,
                    ret: aret,
                    ..
                } = &actual
                {
                    for (p, a) in params.iter().zip(aparams) {
                        self.match_generic_ast(p, a, names, subst, span)?;
                    }
                    self.match_generic_ast(ret, aret, names, subst, span)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// 若 `base` 是泛型函数，按调用点实参类型生成（或复用）一个单态实例，
    /// 并返回指向实例的调用表达式。非泛型函数返回 `None`。
    fn try_instantiate_call(
        &mut self,
        base: DefId,
        args: &[ast::Argument],
        expected: &Ty,
        span: ast::Span,
    ) -> Result<Option<HirExpr>> {
        let Some(ast_fn) = self.generic_asts.get(&base).cloned() else {
            return Ok(None);
        };
        // 带 receiver 的方法走 `check_method_call` 的单态化路径。
        if ast_fn.receiver.is_some() {
            return Ok(None);
        }
        let names = self.generic_names.get(&base).cloned().unwrap_or_default();
        if names.is_empty() || ast_fn.params.len() != args.len() {
            return Ok(None);
        }
        let mut hir_args = Vec::with_capacity(args.len());
        let mut subst: HashMap<String, Ty> = HashMap::new();
        for (p, arg) in ast_fn.params.iter().zip(args) {
            let e = match arg {
                ast::Argument::Pos(e) | ast::Argument::Named(_, e) => e,
            };
            // 不要用 `infer_expr_ty` 预判实参类型：它对 `&mut x` 这类
            // 借用表达式只给一个未解推断变量。让 `check_expr` 真正检查一遍，
            // 拿到的是具体类型（`&mut i32`）。
            let aty = self.infer.fresh_infer(span);
            let h = self.check_expr(e, &aty)?;
            self.check_partial_move(&h, e.span())?;
            self.match_generic_ast(&p.ty, &h.ty, &names, &mut subst, span)?;
            hir_args.push(h);
        }
        // 返回值 / 上下文里没出现在参数上的类型参数（`Vec::new()` 的 `T`
        // 来自 `let v: Vec<i32>`）。
        if subst.len() < names.len() {
            let want = self.infer.resolve(expected);
            if !matches!(want, Ty::Infer(_) | Ty::Unit) {
                self.match_generic_ast(&ast_fn.return_type, &want, &names, &mut subst, span)
                    .ok();
            }
        }
        let mut ordered = Vec::with_capacity(names.len());
        for n in &names {
            match subst.get(n) {
                Some(t) => ordered.push(self.infer.resolve(t)),
                None => {
                    // P1-4：从实参/期望类型推不出该类型参数时，先留一个推断
                    // 变量、按它实例化，等用法体（`let v = Vec::new();
                    // v.push(1)` 里的 `push` 把 `T` 约束成 `i32`）把它定下来。
                    // 早先这里直接报 `cannot infer type parameter`，让常见
                    // Rust 写法（先 `new()` 再 `push`）编译失败。
                    ordered.push(self.infer.fresh_infer(span));
                }
            }
        }
        let inst_id = self.instantiate_generic(base, &ast_fn, &names, &ordered, span)?;
        let sig = self.functions.get(&inst_id).cloned();
        let ret = sig
            .as_ref()
            .map(|f| f.return_type.clone())
            .unwrap_or(Ty::Unit);
        // 第二遍：按具体参数类型重新检查实参，让数组→切片、`&mut` 之类的
        // 隐式转换真正发生（第一遍用的还是推断变量）。
        let mut final_args = Vec::with_capacity(hir_args.len());
        for (i, arg) in args.iter().enumerate() {
            let e = match arg {
                ast::Argument::Pos(e) | ast::Argument::Named(_, e) => e,
            };
            let pty = match sig.as_ref().and_then(|s| s.params.get(i)) {
                Some(p) => p.ty.clone(),
                None => self.infer.fresh_infer(span),
            };
            let h = self.check_expr(e, &pty)?;
            self.check_partial_move(&h, e.span())?;
            final_args.push(h);
        }
        Ok(Some(HirExpr {
            kind: ExprKind::Call {
                callee: inst_id,
                args: final_args,
                result: self.alloc_local(),
            },
            ty: ret,
            span,
        }))
    }

    /// 取得（或新建）一个单态实例的 DefId，并登记待检查的函数体。
    fn instantiate_generic(
        &mut self,
        base: DefId,
        ast_fn: &ast::Function,
        names: &[String],
        ordered: &[Ty],
        span: ast::Span,
    ) -> Result<DefId> {
        let key = format!("{:?}", ordered);
        if let Some(id) = self.mono_cache.get(&(base, key.clone())).copied() {
            // 命中缓存说明同一个实参组合早就验过边界了，不必重验。
            return Ok(id);
        }
        // 单态化是验证 trait 边界的唯一时刻：这里才拿到全部具体实参。
        // 不查的话 `fn f<T: Ord>` 里的 `Ord` 等于没写，`f<Nope>` 静默通过。
        self.check_trait_bounds(base, ordered, span)?;
        if self.mono_cache.len() > 512 {
            return Err(
                CompileError::type_error("monomorphization recursion limit reached")
                    .with_span(span),
            );
        }
        let name = format!("{}__mono{}", ast_fn.name, self.next_def);
        let id = self.alloc_def(name);
        let subst: Vec<(String, Ty)> = names.iter().cloned().zip(ordered.iter().cloned()).collect();
        self.mono_cache.insert((base, key), id);
        self.mono_pending.push(MonoInstance {
            base,
            id,
            subst: subst.clone(),
            span,
        });
        let sig = self.build_instance_signature(ast_fn, id, &subst)?;
        self.functions.insert(id, sig);
        Ok(id)
    }

    /// 按具体类型生成单态实例的签名（参数类型 / 返回类型）。
    fn build_instance_signature(
        &mut self,
        ast_fn: &ast::Function,
        id: DefId,
        subst: &[(String, Ty)],
    ) -> Result<hir::HirFunction> {
        let saved = std::mem::take(&mut self.type_params);
        self.push_type_scope(subst);
        let ret = self.ast_to_hir_type(&ast_fn.return_type)?;
        let mut params = Vec::with_capacity(ast_fn.params.len() + 1);
        if let Some(ty) = self.receiver_ty(ast_fn)? {
            params.push(hir::HirParam {
                local: self.alloc_local(),
                ty,
            });
        }
        for p in &ast_fn.params {
            let ty = self.ast_to_hir_type(&p.ty)?;
            params.push(hir::HirParam {
                local: self.alloc_local(),
                ty,
            });
        }
        self.type_params = saved;
        Ok(hir::HirFunction {
            name: self.def_names.get(&id).cloned().unwrap_or_default(),
            params,
            return_type: ret,
            body: None,
            is_async: ast_fn.is_async,
            async_output: None,
            is_comptime: ast_fn.is_comptime,
            abi: hir::Abi::Zr,
            attrs: Vec::new(),
        })
    }

    /// 用具体类型重新检查一个单态实例的函数体。
    fn check_instance_body(&mut self, inst: &MonoInstance) -> Result<()> {
        let Some(ast_fn) = self.generic_asts.get(&inst.base).cloned() else {
            return Ok(());
        };
        // P1-4：泛型实例的类型参数若仍是未解析的推断变量，说明调用点既没从
        // 实参/期望类型推出、也没被用法体约束（`let v = Vec::new();` 之后 `v`
        // 再没被用过，`T` 无从定下）。早先把 `Vec::new()` 直接报
        // `cannot infer type parameter`；现在留推断变量等用法体，到这里仍是
        // 未解析就明确报错，别让后端把 `_` 当 i64 编出错误程序。
        for (_, ty) in &inst.subst {
            if ty_has_unresolved_infer(ty, &self.infer.vars) {
                let name = self.generic_names.get(&inst.base).map(|ns| ns.join(", ")).unwrap_or_default();
                return Err(CompileError::type_error(format!(
                    "cannot infer type parameter{} for `{}`",
                    if name.is_empty() { String::new() } else { format!(" `{name}`") },
                    ast_fn.name
                ))
                .with_span(inst.span));
            }
        }
        let saved_types = std::mem::take(&mut self.type_params);
        let saved_scopes = std::mem::take(&mut self.local_scopes);
        let saved_stack = std::mem::take(&mut self.scope_stack);
        let saved_depth = self.scope_depth;
        self.scope_depth = 0;

        self.current_function = Some(inst.id);
        self.current_is_async = ast_fn.is_async;
        self.push_scope();
        self.push_type_scope(&inst.subst);
        if let Some(target) = self.impl_targets.get(&ast_fn.name).cloned() {
            if let Ok(ty) = self.ast_to_hir_type(&target) {
                self.push_named_type("Self", ty);
            }
        }

        let declared = self
            .functions
            .get(&inst.id)
            .map(|f| f.return_type.clone())
            .unwrap_or(Ty::Unit);
        let ret_ty = match &declared {
            Ty::Opaque(opaque) => self.async_outputs.get(opaque).cloned().unwrap_or(declared),
            _ => declared,
        };
        self.infer.return_ty = Some(ret_ty.clone());

        let mut params = Vec::new();
        if let Some(ty) = self.receiver_ty(&ast_fn)? {
            let (_, local) = self.bind_local("self", &ty);
            params.push(hir::HirParam { local, ty });
        }
        for p in &ast_fn.params {
            let ty = self.ast_to_hir_type(&p.ty)?;
            let pat = self.lower_pattern(&p.pattern, &ty)?;
            let local = first_binding(&pat).unwrap_or_else(|| self.alloc_local());
            params.push(hir::HirParam {
                local,
                ty: ty.clone(),
            });
        }
        let body = match &ast_fn.body {
            Some(b) => Some(self.check_block(b, &ret_ty)?),
            None => None,
        };
        if let Some(hf) = self.functions.get_mut(&inst.id) {
            hf.params = params;
            hf.body = body;
            if ast_fn.is_async {
                hf.async_output = Some(ret_ty);
            }
        }
        self.pop_scope();
        if self.impl_targets.contains_key(&ast_fn.name) {
            self.type_params.pop();
        }
        self.type_params.pop();
        self.current_function = None;

        self.scope_depth = saved_depth;
        self.scope_stack = saved_stack;
        self.local_scopes = saved_scopes;
        self.type_params = saved_types;
        Ok(())
    }

    /// 绑定一组泛型参数（类型参数 → 新的推断变量）。
    ///
    /// 泛型还没有单态化，声明里的类型参数只能当成推断变量：声明本身能过
    /// 类型检查，但按它布局 / 生成代码是不对的（`Option<T>` 的大小取决于
    /// `T`）。用到泛型的地方会另行报错。
    fn push_type_params(
        &mut self,
        generics: &[ast::GenericParam],
        span: ast::Span,
    ) -> HashMap<String, Ty> {
        let mut scope = HashMap::new();
        // 所有参数（类型 / 生命周期 / const）共用一个名字空间做重名检查：
        // 重名是笔误，早点报出来比在用到的地方冒出一堆推断错误强。
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for g in generics {
            let name = match g {
                ast::GenericParam::Type(name, _) => name.as_str(),
                ast::GenericParam::Lifetime(name) => name.as_str(),
                ast::GenericParam::Const(name) => name.as_str(),
            };
            if !seen.insert(name) {
                self.errors.push(
                    CompileError::type_error(format!("duplicate generic parameter `{name}`"))
                        .with_span(span),
                );
            }
            // 生命周期 / const 参数不进入类型环境：生命周期不影响代码生成，
            // const 泛型还没实现（用到时另行报错）。
            if let ast::GenericParam::Type(name, _) = g {
                let ty = self.infer.fresh_infer(span);
                scope.insert(name.clone(), ty);
            }
        }
        self.type_params.push(scope.clone());
        scope
    }

    /// 变体的判别式值（显式写的那个，或上一个 + 1）。
    fn variant_discriminant(&self, def_id: DefId, index: usize) -> i128 {
        let Some(hir::HirTypeDef::Enum(he)) = self.type_defs.get(&def_id) else {
            return index as i128;
        };
        he.variants
            .get(index)
            .map(|v| v.discriminant)
            .unwrap_or(index as i128)
    }

    /// 变体下标：按声明顺序。
    fn variant_index(&self, def_id: DefId, name: &str) -> usize {
        let Some(hir::HirTypeDef::Enum(he)) = self.type_defs.get(&def_id) else {
            return 0;
        };
        he.variants
            .iter()
            .position(|v| v.name == name)
            .unwrap_or(0)
    }

    /// 变体载荷的字段类型（按声明顺序）。
    fn variant_payload_tys(
        &self,
        def_id: DefId,
        idx: usize,
        _span: ast::Span,
    ) -> Result<Vec<Ty>> {
        Ok(self.variant_field_types(def_id, idx).unwrap_or_default())
    }

    // ==================================================================
    // 闭包
    // ==================================================================

    /// 闭包表达式。
    ///
    /// 第一版闭包走**单态化内联**：闭包体在调用点展开，捕获变量直接复用外层
    /// local 的存储。这里只负责把闭包体类型检查出来、收集捕获，不再无条件
    /// 报错——只有在闭包被当成值"取走"（存进变量 / 当实参 / 返回）时，MIR
    /// 阶段才会拒绝，因为那超出了同函数内联的范围。
    fn check_lambda(
        &mut self,
        params: &[ast::LambdaParam],
        ret: Option<&AstType>,
        body: &AstBlock,
        _expected: &Ty,
        span: ast::Span,
        liftable: bool,
    ) -> Result<HirExpr> {
        // 进入闭包**之前**抓一份当前所有可见 local：它们就是"外层局部"，
        // 闭包形参与体内新建的 local 都不在这个集合里，借此区分捕获。
        let outer = self.locals.keys().copied().collect();
        self.closure_ctx.push(outer);
        self.closure_captures.push(Vec::new());
        self.push_scope();
        let mut hir_params = Vec::new();
        for p in params {
            // 写了类型标注就用它，没写就靠推断。
            let ty = match &p.ty {
                Some(t) => self.ast_to_hir_type(t)?,
                None => self.infer.fresh_infer(span),
            };
            let local = self.alloc_local();
            self.locals.insert(local, ty.clone());
            self.infer.locals.insert(local, ty.clone());
            self.scope_stack.last_mut().unwrap().push(local);
            hir_params.push((self.lower_pattern(&p.pattern, &ty)?, ty.clone()));
        }
        let ret_ty = ret
            .as_ref()
            .map(|t| self.ast_to_hir_type(t))
            .unwrap_or_else(|| Ok(self.infer.fresh_infer(span)))?;
        // 闭包体里的 `return` 返回的是**闭包**的值，不是外层函数的值。
        // 不切换的话 `let r = |x: i32| { return x + 1; };` 里的 `return x + 1`
        // 会被拿去和外层函数的返回类型对（碰巧都是 i32 才没炸）。切换后
        // 立即恢复：闭包体检查完，外层函数的 `return` 语义要原样保留。
        let saved_ret = self.infer.return_ty.take();
        self.infer.return_ty = Some(ret_ty.clone());
        let checked_body = self.check_block(body, &ret_ty)?;
        self.infer.return_ty = saved_ret;
        // 返回类型是推断变量（`let f = |x| x + 1;`）：`check_block` 的 tail
        // 走 `subtype`，而 `subtype` 对 `Infer` 期望类型直接放行**不绑定**，
        // 于是 `ret_ty` 永远停在 Infer，收尾报 `type annotations needed`。
        // 这里用尾表达式的实际类型把推断变量绑上——但只有真正存在尾表达式
        // 才绑：`|x: i32| { f(x); }`（最后一句带分号）的类型是 `()`，不能
        // 拿 `()` 去污染一个本该由 `return` 或调用点反推的返回类型。
        if matches!(self.infer.resolve(&ret_ty), Ty::Infer(_)) {
            if let Some(tail) = &checked_body.tail {
                self.infer.unify(&ret_ty, &tail.ty, span).ok();
            }
        }
        self.pop_scope();
        // 取出本层捕获（去重，保留发现顺序；`check_expr` 在解析到外层 local
        // 时已记入）。`closure_ctx` 与 `closure_captures` 同步 pop。
        let mut captures = self.closure_captures.pop().unwrap();
        self.closure_ctx.pop();
        captures.dedup_by_key(|(l, _)| *l);
        // 嵌套闭包捕获**向外传播**：内层闭包捕获了 `L`，而 `L` 对外层闭包
        // 也是自由变量（在外层闭包的外层集合里），外层闭包必须一并捕获 `L`。
        // 否则外层闭包会被提升成顶层函数（`lift_closure`），内层闭包的环境
        // 字面量 `__env_N { __c0: L, .. }` 里引用的 `L` 就悬空了——顶层函数
        // 没有外层局部，MIR 拿 `Local(L)` 读到未初始化的栈槽。例：
        // `let a = 5; let outer = |b| { let inner = |c| c + a + b; inner(10) };`
        // `inner` 捕获 `a` 与 `b`，`b` 是 `outer` 的形参（不在 `outer` 的外层
        // 集合，不传播），`a` 是 `outer` 外层的局部（传播给 `outer`）。
        if let Some(outer_ctx) = self.closure_ctx.last() {
            if let Some(enclosing) = self.closure_captures.last_mut() {
                for (l, ty) in &captures {
                    if outer_ctx.contains(l) {
                        enclosing.push((*l, ty.clone()));
                    }
                }
            }
        }
        let fn_ty = Ty::Function {
            params: hir_params.iter().map(|(_, t)| t.clone()).collect(),
            ret: Box::new(ret_ty.clone()),
            is_async: false,
            abi: Abi::Zr,
        };
        // 上下文给了函数类型时（`let g: fn(i32) -> i32 = |x| ..`、把闭包
        // 当实参传给 `fn(..)` 形参），把期望类型里的形参/返回类型统一进来。
        // 这样没写标注的形参/返回类型能从上下文拿到，而不是停在推断变量上。
        // 只在期望类型本身是 `Ty::Function` 时生效——`let f = |x| x` 的期望
        // 是 Infer，统一进去会制造无意义的链。
        if let Ty::Function {
            params: exp_params,
            ret: exp_ret,
            ..
        } = self.infer.resolve(_expected)
        {
            if exp_params.len() == hir_params.len() {
                for (p, ep) in hir_params.iter().zip(exp_params.iter()) {
                    if matches!(self.infer.resolve(&p.1), Ty::Infer(_)) {
                        self.infer.unify(&p.1, ep, span).ok();
                    }
                }
            }
            if matches!(self.infer.resolve(&ret_ty), Ty::Infer(_)) {
                self.infer.unify(&ret_ty, &exp_ret, span).ok();
            }
        }
        // 无捕获闭包提升成顶层函数：闭包不引用任何外层局部，本来就是普通
        // 函数（见 `tests/cases/83_closure_fnptr.zr` 头注释）。只有不在
        // "立即调用"位置时才提升——立即调用走单态化内联的老路子。
        if liftable && captures.is_empty() {
            return self.lift_closure(hir_params, ret_ty, checked_body, fn_ty, span);
        }
        // 两条出路：
        // 1. **立即调用**（`liftable=false`，由 `inline_closure_call` 调用）：
        //    保留捕获列表与内联体，调用点直接把闭包体在定义处展开，捕获
        //    local 复用外层 local（同函数内联，MIR 用得到）。
        // 2. **非立即调用**（`liftable=true`，`let f = |..|`、传参、返回）：
        //    无捕获时已 `lift_closure` 成顶层函数；带捕获时登记环境结构体
        //    `__env_N`（字段 = 捕获局部，值拷贝），表达式类型是环境结构体。
        //    调用点 `f(a)` 按环境 DefId 查 `closures_by_env` 拿回闭包体，
        //    `apply_capture_body` 把内联体里捕获 local 替换成 `f.__cN` 后
        //    在调用点展开。环境值按普通结构体传递，同函数内有效。
        if !liftable {
            // 立即调用路径：保留捕获，交给 `inline_closure_call` 展开。
            return Ok(HirExpr {
                kind: ExprKind::Closure {
                    params: hir_params,
                    ret: Box::new(ret_ty),
                    body: Some(checked_body),
                    captures,
                },
                ty: fn_ty,
                span,
            });
        }
        // 非立即调用、带捕获：登记环境结构体 + 闭包记录，返回**环境结构体
        // 字面量**（不是 `ExprKind::Closure`——MIR 不会降级 Closure 值，它只
        // 在立即调用里被内联展开掉）。字段 = 捕获局部，按发现顺序 `__cN`。
        if !captures.is_empty() {
            let env_id = self.closure_env_id(&captures)?;
            self.closures_by_env.insert(
                env_id,
                CapturedClosure {
                    params: hir_params.clone(),
                    ret: ret_ty.clone(),
                    body: checked_body.clone(),
                    captures: captures.clone(),
                    span,
                },
            );
            let env_ty = Ty::Named(env_id, Vec::new());
            // 环境结构体字面量：`__env_N { __c0: cap0, __c1: cap1, .. }`。
            // 捕获 local 是外层局部，值拷贝进环境；MIR 按普通结构体降级。
            let fields: Vec<(String, HirExpr)> = captures
                .iter()
                .enumerate()
                .map(|(i, (cap_local, cap_ty))| {
                    let cap_ty = self.infer.resolve(cap_ty);
                    (
                        format!("__c{i}"),
                        HirExpr {
                            kind: ExprKind::Local(*cap_local),
                            ty: cap_ty.clone(),
                            span,
                        },
                    )
                })
                .collect();
            return Ok(HirExpr {
                kind: ExprKind::Struct(fields),
                ty: env_ty,
                span,
            });
        }
        // 非立即调用、无捕获：不应该走到这里（上面 `lift_closure` 已返回）。
        Ok(HirExpr {
            kind: ExprKind::Closure {
                params: hir_params,
                ret: Box::new(ret_ty),
                body: Some(checked_body),
                captures: Vec::new(),
            },
            ty: fn_ty,
            span,
        })
    }

    /// 带捕获闭包的环境结构体：`Ty::Named(env_id, [])`。
    ///
    /// 非泛型结构体：`Ty::Named` 的类型参数列表必须为空。`type_defs` 里
    /// 登记 `HirStruct`（字段 = 捕获局部，按发现顺序），让 `lookup_field` /
    /// `struct_layout` / `copy_struct_value` 能按名取字段。返回环境 DefId。
    fn closure_env_id(&mut self, captures: &[(LocalId, Ty)]) -> Result<DefId> {
        let id = self.alloc_def(format!("__env_{}", self.next_def));
        let name = format!("__env_{}", id.0);
        self.def_names.insert(id, name.clone());
        self.type_defs.insert(
            id,
            hir::HirTypeDef::Struct(hir::HirStruct {
                name,
                attrs: Vec::new(),
                repr: hir::Repr::Rust,
                fields: captures
                    .iter()
                    .enumerate()
                    .map(|(i, (_, t))| hir::HirField {
                        name: format!("__c{i}"),
                        ty: t.clone(),
                        index: i,
                    })
                    .collect(),
            }),
        );
        Ok(id)
    }

    /// 把无捕获闭包提升成顶层函数，表达式变成函数指针（`ExprKind::Def`）。
    ///
    /// 登记到 `self.lifted_closures` 与 `self.functions`（占位，body 暂空）：
    /// 后续调用点 `def_type` 能直接拿到 `fn(..)` 类型。形参 / 返回类型可能
    /// 还是推断变量（`let id = |x| x; id(7)` 里 `x` 要靠调用反推），所以
    /// 函数体在 `finalize_lifted_closures` 里统一 resolve 并回填。
    fn lift_closure(
        &mut self,
        params: Vec<(hir::HirPattern, Ty)>,
        ret: Ty,
        body: hir::HirBlock,
        fn_ty: Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let name = format!("__closure_{}", self.next_def);
        let def_id = self.alloc_def(name);
        let hir_params: Vec<hir::HirParam> = params
            .iter()
            .map(|(pat, ty)| hir::HirParam {
                local: first_binding(pat).unwrap_or_else(|| self.alloc_local()),
                ty: ty.clone(),
            })
            .collect();
        self.functions.insert(
            def_id,
            hir::HirFunction {
                name: format!("__closure_{}", def_id.0),
                params: hir_params.clone(),
                return_type: ret.clone(),
                body: None,
                is_async: false,
                async_output: None,
                is_comptime: false,
                abi: Abi::Zr,
                attrs: Vec::new(),
            },
        );
        self.lifted_closures.push(LiftedClosure {
            def_id,
            params,
            ret,
            body,
            span,
        });
        Ok(HirExpr {
            kind: ExprKind::Def(def_id),
            ty: fn_ty,
            span,
        })
    }

    /// 把带捕获闭包的环境值在调用点内联展开：`f(a)`（`f` 是 `__env_N`）。
    ///
    /// 环境值是普通结构体（字段 = 捕获局部，按发现顺序 `__cN`）。展开时：
    /// 1. 为每个形参新建局部绑定实参（避免带副作用的实参被重复求值）；
    /// 2. 把内联体里捕获 local 的引用替换成 `f.__cN`（`f` 是环境值 local，
    ///    MIR 走 `Field(Local(f))`）。
    /// 形参 local 是闭包体检查时分配的，与调用方局部不冲突，直接保留。
    fn apply_capture_body(
        &mut self,
        env_expr: HirExpr,
        rec: &CapturedClosure,
        args: &[ast::Argument],
        expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        // 环境值必须是一个 local（`let f = |..|` 存下来的结构体值），或一个
        // 结构体字段（`ops.f`）。取其 base local 作替换目标；非 local 直接报错。
        let env_base: LocalId = match &env_expr.kind {
            ExprKind::Local(l) => *l,
            ExprKind::Field(v, _) => match &v.kind {
                ExprKind::Local(l) => *l,
                _ => {
                    return Err(CompileError::type_error(
                        "closure environment must be a local variable",
                    )
                    .with_span(span));
                }
            },
            _ => {
                return Err(CompileError::type_error(
                    "closure environment must be a local variable",
                )
                .with_span(span));
            }
        };
        let env_ty = env_expr.ty.clone();
        // 形参绑定：复用闭包体检查时分配的形参 local（与调用方局部不冲突）。
        let mut prelude: Vec<hir::HirStmt> = Vec::with_capacity(args.len());
        let mut lam_body = rec.body.clone();
        for ((pat, ty), arg) in rec.params.iter().zip(args.iter()) {
            let local = match pat {
                hir::HirPattern::Binding(l) => *l,
                _ => {
                    return Err(CompileError::type_error(
                        "closure destructuring parameters are not yet supported",
                    )
                    .with_span(span));
                }
            };
            let arg_ast = match arg {
                ast::Argument::Pos(e) => e,
                ast::Argument::Named(_, e) => e,
            };
            let arg_hir = self.check_expr(arg_ast, ty)?;
            prelude.push(hir::HirStmt::Let {
                local,
                init: Some(Box::new(arg_hir)),
            });
        }
        // 把内联体里捕获 local 的引用替换成 `env_base.__cN`。
        for (i, (cap_local, cap_ty)) in rec.captures.iter().enumerate() {
            let cap_ty = self.infer.resolve(cap_ty);
            let with = HirExpr {
                kind: ExprKind::Field(
                    Box::new(HirExpr {
                        kind: ExprKind::Local(env_base),
                        ty: env_ty.clone(),
                        span: rec.span,
                    }),
                    format!("__c{i}"),
                ),
                ty: cap_ty,
                span: rec.span,
            };
            walk_block(&mut lam_body, *cap_local, &with);
        }
        // 闭包体里的 `return` 属于**闭包**，不是外层函数。内联到调用点后，
        // `Return(Some(e))` 会被 MIR 当成"从外层函数返回"，把调用点之后的
        // 代码全部跳过——`let r = |x| { return x + 1; }; r(41)` 的 `r(41)`
        // 展开成 `return 42;`，`main` 直接返回 42，后续断言全不跑。
        //
        // 处理：闭包体最后一句若是 `Return(Some(e))`（`{ return e; }` 这种
        // 常见形状），把这一句从 stmts 里摘出来，换成块尾表达式 `e`——闭包
        // 的返回值本来就是 `e`，调用点消费这个块的值，语义一致。体内中间的
        // `return` 需要更复杂的基本块变换，暂不支持（会报"unreachable"，
        // 与 Rust 的 `unreachable` 语义接近，但不静默返回错值）。
        if lam_body.tail.is_none() {
            let tail_ret = match lam_body.stmts.last() {
                Some(HirStmt::Expr(last)) => match &last.kind {
                    ExprKind::Return(Some(ret_val)) => Some(ret_val.clone()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(ret_val) = tail_ret {
                lam_body.stmts.pop();
                lam_body.tail = Some(ret_val);
            }
        }
        let result_ty = match self.infer.resolve(&rec.ret) {
            Ty::Infer(_) => self.infer.resolve(expected).clone(),
            other => other,
        };
        let mut inline_stmts = prelude;
        inline_stmts.extend(lam_body.stmts);
        Ok(HirExpr {
            kind: ExprKind::Block(hir::HirBlock {
                stmts: inline_stmts,
                tail: lam_body.tail,
            }),
            ty: result_ty,
            span,
        })
    }

    /// 收尾解析所有提升闭包并回填函数体。
    ///
    /// `check_function_bodies` 处理完所有函数（含泛型单态实例）后调用。
    /// 每个提升闭包：
    /// 1. 解析形参 / 返回类型里的推断变量（`let id = |x| x; id(7)` 里 `x`
    ///    要靠调用点反推，此时已经统一完）；
    /// 2. 仍停在 `Infer` 的明确报 `type annotations needed`（Rust 同样要求
    ///    这种闭包写标注，不能让后端把 `_` 当 i64 编）；
    /// 3. 把闭包体的局部量 id 重编号成提升函数的独立编号空间（闭包体里的
    ///    `Local(id)` 原本和调用方共享 `next_local` 计数器，两个函数若用
    ///    同一个 `v{id}` 名字，MIR 会串味）；
    /// 4. 把 `HirFunction`（签名 + 体）挂进 `self.functions`，MIR 按普通
    ///    函数降级。
    fn finalize_lifted_closures(&mut self) -> Result<()> {
        if self.lifted_closures.is_empty() {
            return Ok(());
        }
        let lifted = std::mem::take(&mut self.lifted_closures);
        for lc in lifted {
            // 解析形参 / 返回类型。参数类型停在推断变量时明确报错——没有
            // 任何上下文能定下它，写出来也只会让后端把 `_` 当 i64 编。
            let mut params = Vec::with_capacity(lc.params.len());
            for (pat, ty) in lc.params {
                let ty = ty_resolve_deep(&ty, &self.infer.vars);
                if ty_has_unresolved_infer(&ty, &self.infer.vars) {
                    return Err(CompileError::type_error(
                        "type annotations needed for closure parameter type",
                    )
                    .with_span(lc.span));
                }
                params.push((pat, ty));
            }
            let ret = ty_resolve_deep(&lc.ret, &self.infer.vars);
            if ty_has_unresolved_infer(&ret, &self.infer.vars) {
                return Err(CompileError::type_error(
                    "type annotations needed for closure return type",
                )
                .with_span(lc.span));
            }
            // 重编号：闭包体里的局部量 id 原本从全局计数器分配，与调用方
            // 共享。单独成一个函数后必须有自己的编号空间，否则 MIR 里
            // `v{id}` 会和调用方的临时名撞车。形参 local 也要重编号，因为
            // `HirParam.local` 与函数体里 `ExprKind::Local` 必须一致。
            let mut map: HashMap<LocalId, LocalId> = HashMap::new();
            let hir_params: Vec<hir::HirParam> = params
                .iter()
                .map(|(pat, ty)| {
                    let local = match first_binding(pat) {
                        Some(id) => {
                            self.next_local += 1;
                            let nid = LocalId(self.next_local - 1);
                            map.insert(id, nid);
                            nid
                        }
                        None => {
                            self.next_local += 1;
                            LocalId(self.next_local - 1)
                        }
                    };
                    hir::HirParam { local, ty: ty.clone() }
                })
                .collect();
            let mut body = lc.body;
            remap_lifted_body(&mut body, &mut map, &mut self.next_local);
            // 回填签名：`functions` 里登记的占位签名用解析后的类型。
            if let Some(hf) = self.functions.get_mut(&lc.def_id) {
                hf.params = hir_params.clone();
                hf.return_type = ret.clone();
                hf.body = Some(body);
            }
        }
        Ok(())
    }

    /// 闭包立即调用的单态化内联：`(|x| x + 1)(5)`、`(|x| x + n)(5)`。
    ///
    /// 在调用点把闭包体展开：为每个形参新建一个局部变量绑定实参（避免带
    /// 副作用的实参被重复求值），然后把闭包体里对形参的引用替换成该新局部。
    ///
    /// 带捕获的闭包（`(|x| x + n)(2)`，`n` 是外层 local）：闭包体里对捕获
    /// local 的引用**直接复用调用方的 local**（同函数内联，MIR 用得到），
    /// 不引入环境结构体。形参绑定走新 local，替换进闭包体。
    ///
    /// 与 `apply_capture_body`（带捕获闭包**当值**取走的调用）分工：这里只
    /// 处理"定义处立即调用"（`liftable=false`），捕获引用在调用方作用域里
    /// 有效；值调用走环境结构体替换成 `f.__cN`。
    fn inline_closure_call(
        &mut self,
        params: &[ast::LambdaParam],
        ret: Option<&AstType>,
        body: &AstBlock,
        args: &[ast::Argument],
        expected: &Ty,
        span: ast::Span,
        lam_span: ast::Span,
    ) -> Result<HirExpr> {
        let lam = self.check_lambda(params, ret, body, &Ty::Unit, lam_span, false)?;
        self.closure_consumed += 1;
        let (hir_params, ret_ty, lam_body_opt, _captures, _lam_ty) = match &lam.kind {
            ExprKind::Closure {
                params,
                ret,
                body,
                captures,
            } => (
                params.clone(),
                ret.as_ref().clone(),
                body.clone(),
                captures.clone(),
                lam.ty.clone(),
            ),
            _ => unreachable!("check_lambda always yields ExprKind::Closure"),
        };
        let lam_body = lam_body_opt.ok_or_else(|| {
            CompileError::type_error(
                "closure body is not available for inlining",
            )
            .with_span(lam_span)
        })?;
        if hir_params.len() != args.len() {
            return Err(CompileError::type_error(format!(
                "closure expects {} argument(s), but {} were supplied",
                hir_params.len(),
                args.len()
            ))
            .with_span(span));
        }
        // 只支持简单绑定形参 `|x|`、`|x, y|`：复杂模式（元组 / 结构体解构）
        // 的内联替换需要把模式拆到字段上，超出第一版范围。
        let mut param_locals: Vec<(LocalId, Ty)> = Vec::with_capacity(hir_params.len());
        for (pat, ty) in &hir_params {
            match pat {
                hir::HirPattern::Binding(local) => param_locals.push((*local, ty.clone())),
                _ => {
                    return Err(CompileError::type_error(
                        "closure destructuring parameters are not yet supported",
                    )
                    .with_span(span));
                }
            }
        }
        self.push_scope();
        let mut prelude: Vec<hir::HirStmt> = Vec::with_capacity(args.len());
        // 带捕获立即调用（`(|x| x + n)(2)`，`n` 是外层 local）：闭包体里对
        // 捕获 local 的引用直接复用调用方的 local（同函数内联，MIR 用得到），
        // 不引入环境结构体。形参绑定走新 local（避免带副作用的实参被重复
        // 求值），替换进闭包体。
        let mut lam_body = lam_body;
        for ((orig, ty), arg) in param_locals.iter().zip(args.iter()) {
            let arg_ast = match arg {
                ast::Argument::Pos(e) => e,
                ast::Argument::Named(_, e) => e,
            };
            let arg_hir = self.check_expr(arg_ast, ty)?;
            let new_local = self.alloc_local();
            self.locals.insert(new_local, ty.clone());
            self.infer.locals.insert(new_local, ty.clone());
            self.scope_stack.last_mut().unwrap().push(new_local);
            prelude.push(hir::HirStmt::Let {
                local: new_local,
                init: Some(Box::new(arg_hir.clone())),
            });
            // 闭包体里对 `orig` 的引用替换成对新 local 的引用。
            let with = HirExpr {
                kind: ExprKind::Local(new_local),
                ty: ty.clone(),
                span,
            };
            walk_block(&mut lam_body, *orig, &with);
        }
        self.pop_scope();
        // 内联块：形参绑定前导 + 闭包体（语句 + 尾表达式）。结果类型是闭包返回类型，
        // 若与期望不符由后续统一处理；返回类型若是推断变量则按期望收窄。
        let result_ty = match self.infer.resolve(&ret_ty) {
            Ty::Infer(_) => self.infer.resolve(expected).clone(),
            other => other,
        };
        let mut inline_stmts = prelude;
        inline_stmts.extend(lam_body.stmts);
        Ok(HirExpr {
            kind: ExprKind::Block(hir::HirBlock {
                stmts: inline_stmts,
                tail: lam_body.tail,
            }),
            ty: result_ty,
            span,
        })
    }

    // ==================================================================
    // 借用检查
    // ==================================================================

    fn borrow_check(&mut self, functions: &[hir::HirFunction]) -> Result<()> {
        for f in functions {
            if let Some(b) = &f.body {
                let cfg = self.build_cfg(b)?;
                let errors = self.infer.check_borrows(&cfg);
                if !errors.is_empty() {
                    return Err(errors.into_iter().next().unwrap());
                }
            }
        }
        Ok(())
    }

    fn build_cfg(&mut self, block: &hir::HirBlock) -> Result<Cfg> {
        let mut cfg = Cfg::default();
        let start = self.alloc_bb();
        cfg.start = start;
        let mut bb = BasicBlock {
            stmts: Vec::new(),
            term: hir::Terminator::Unreachable,
        };
        for s in &block.stmts {
            bb.stmts.push(s.clone());
        }
        let _ = &block.tail;
        bb.term = hir::Terminator::Return;
        cfg.blocks.push(bb);
        Ok(cfg)
    }

    // ==================================================================
    // 名称解析辅助
    // ==================================================================

    fn alloc_def(&mut self, name: String) -> DefId {
        let id = DefId(self.next_def);
        self.next_def += 1;
        self.symbols.insert(name.clone(), id);
        self.def_names.insert(id, name.clone());
        // 诊断用的名字表：`InferCtxt` 看不到 `self.def_names`（`'static` 借用），
        // 没有这一步所有具名类型在 `unify` 的错误里都会打成 `type#N`。
        infer::register_def_name(id, &name);
        id
    }

    /// 顶层声明专用的 DefId 登记：**用户定义优先**，同名时保留先注册的。
    ///
    /// `collect_declarations` 先处理用户源码里的函数/类型/常量/静态，后处理
    /// 被 `use` 拉进来的标准库模块条目。同名时（用户自己写 `fn swap`，标准库
    /// `core` 也有 `swap<T>`）**先注册的（用户）要赢**——不能后注册的（std）
    /// 覆盖，否则 `main` 里调 `swap((12,13))` 会解析到泛型 std `swap<T>`
    /// （返回 `()`），用户的元组返回类型被静默丢掉，报
    /// "cannot access field `0` on type `()`"（`55_tuple_pattern`）。
    ///
    /// 这里保留首次注册：std 的重复条目仍拿到独立 DefId（进 `functions` /
    /// `generic_asts`，限定路径 `core::swap` 仍可解析），只是不再改写
    /// `symbols` 里用户的名字绑定。真正的重复定义（用户写两个同名函数）
    /// 是另一个错误，不由这里管。
    fn alloc_def_top(&mut self, name: String) -> DefId {
        let id = DefId(self.next_def);
        self.next_def += 1;
        if !self.symbols.contains_key(&name) {
            self.symbols.insert(name.clone(), id);
        }
        self.def_names.insert(id, name.clone());
        infer::register_def_name(id, &name);
        id
    }

    fn alloc_local(&mut self) -> LocalId {
        let id = LocalId(self.next_local);
        self.next_local += 1;
        id
    }

    fn alloc_bb(&mut self) -> BasicBlockId {
        self.next_bb += 1;
        BasicBlockId
    }

    fn find_def(&self, name: &str) -> Result<DefId> {
        self.symbols
            .get(name)
            .copied()
            .ok_or_else(|| CompileError::name_error(format!("undefined name `{}`", name)))
    }

    fn resolve_name(&self, name: &str, span: ast::Span) -> Result<DefId> {
        // 先查局部变量（词法作用域，内层优先），再查导入与全局符号
        if let Some(id) = self.resolve_local(name) {
            return Ok(id);
        }
        if let Some(id) = self.use_aliases.get(name).copied() {
            return Ok(id);
        }
        self.symbols.get(name).copied().ok_or_else(|| {
            CompileError::name_error(format!("cannot find value `{}` in this scope", name))
                .with_span(span)
        })
    }

    fn resolve_path(&self, path: &[String], span: ast::Span) -> Result<DefId> {
        if path.is_empty() {
            return Err(CompileError::name_error("empty path").with_span(span));
        }
        // 关联函数 `Type::method` 在展开阶段被 mangle 成 `Type__method`。
        if path.len() > 1 {
            let mangled = path.join("__");
            if let Some(id) = self.symbols.get(&mangled).copied() {
                return Ok(id);
            }
        }
        let first = path[0].clone();
        let id = self.resolve_name(&first, span)?;
        for seg in &path[1..] {
            // 路径继续：查找 id 的成员
            if let Some(hir::HirTypeDef::Struct(s)) = self.type_defs.get(&id) {
                if let Some(f) = s.fields.iter().find(|f| f.name == *seg) {
                    // 字段不是 DefId，这里仅作占位
                    let _ = f;
                }
            }
            let _ = seg;
        }
        Ok(id)
    }

    fn resolve_type_path(&self, path: &[String]) -> Result<DefId> {
        if path.len() == 1 {
            if let Some(id) = primitive_def_id(&path[0]) {
                return Ok(id);
            }
        }
        self.resolve_path(path, ast::Span::unknown())
    }

    fn as_local(&self, id: DefId) -> Option<LocalId> {
        // 若 id 对应某个局部变量则返回
        self.locals_by_def.get(&id).copied()
    }

    /// 按名字查当前作用域里局部变量的类型。
    fn local_var_ty(&self, name: &str) -> Option<Ty> {
        for layer in self.local_scopes.iter().rev() {
            if let Some(def) = layer.get(name) {
                if let Some(local) = self.locals_by_def.get(def) {
                    if let Some(ty) = self.locals.get(local) {
                        return Some(ty.clone());
                    }
                }
            }
        }
        None
    }

    // ==================================================================
    // 类型转换辅助
    // ==================================================================

    fn ast_to_hir_type(&mut self, t: &AstType) -> Result<Ty> {
        Ok(match t {
            AstType::I(w) => {
                self.check_int_width(*w as u16)?;
                Ty::Int(*w as u16)
            }
            AstType::U(w) => {
                self.check_int_width(*w as u16)?;
                Ty::Uint(*w as u16)
            }
            AstType::F(w) => Ty::Float(*w as u16),
            AstType::Bool => Ty::Bool,
            AstType::Char => Ty::Char,
            AstType::Str => Ty::Str,
            AstType::Void => Ty::Unit,
            AstType::Never => Ty::Never,
            AstType::Named { path, generics } => {
                // `usize` / `isize` 是目标相关的整数类型，不是用户类型。
                if path.len() == 1 && generics.is_empty() {
                    match path[0].as_str() {
                        "usize" => return Ok(Ty::Uint(self.pointer_bits as u16)),
                        "isize" => return Ok(Ty::Int(self.pointer_bits as u16)),
                        _ => {}
                    }
                }
                // 特殊处理 Pin<T>
                if path.len() == 1 && path[0] == "Pin" && generics.len() == 1 {
                    return Ok(Ty::Pin(Box::new(self.ast_to_hir_type(&generics[0])?)));
                }
                // 泛型参数优先：声明里的 `T` 是类型变量，不是用户类型。
                if path.len() == 1 && generics.is_empty() {
                    if let Some(ty) = self.lookup_type_param(&path[0]) {
                        return Ok(ty);
                    }
                }
                let id = self.resolve_type_path(path)?;
                let ty = Ty::Named(
                    id,
                    generics
                        .iter()
                        .map(|g| self.ast_to_hir_type(g))
                        .collect::<Result<_>>()?,
                );
                // 别名不是独立的名义类型：`type MyInt = i32` 之后 `Vec<MyInt>`
                // 必须能与 `Vec<i32>` 统一，否则泛型推断把 `T` 绑成 `MyInt`，
                // 再遇 `i32` 就报「推断成两个不同的类型」。所以在这里展平别名。
                //
                // 别名目标本身可能又是别名（`type A = B; type B = i32`），
                // 于是递归——深度上限挡掉 `type A = B; type B = A` 的循环。
                self.inline_alias_ty(&ty, 0)
            }
            AstType::Pointer(inner, m) => Ty::Pointer(Box::new(self.ast_to_hir_type(inner)?), *m),
            AstType::Reference(inner, m) => {
                Ty::Reference(Box::new(self.ast_to_hir_type(inner)?), *m)
            }
            AstType::Array(inner, len) => Ty::Array(
                Box::new(self.ast_to_hir_type(inner)?),
                match len {
                    ast::ArrayLen::Const(e) => {
                        hir::ArrayLen::Const(Box::new(self.lower_const_expr(e)?))
                    }
                    ast::ArrayLen::Dynamic => hir::ArrayLen::Dynamic,
                },
            ),
            AstType::Slice(inner, m) => Ty::Slice(Box::new(self.ast_to_hir_type(inner)?), *m),
            AstType::Tuple(tys) => Ty::Tuple(
                tys.iter()
                    .map(|t| self.ast_to_hir_type(t))
                    .collect::<Result<_>>()?,
            ),
            AstType::Function {
                params,
                ret,
                is_async,
                abi,
            } => Ty::Function {
                params: params
                    .iter()
                    .map(|p| self.ast_to_hir_type(p))
                    .collect::<Result<_>>()?,
                ret: Box::new(self.ast_to_hir_type(ret)?),
                is_async: *is_async,
                abi: abi.clone(),
            },
            AstType::Optional(inner) => Ty::Optional(Box::new(self.ast_to_hir_type(inner)?)),
            AstType::ErrorUnion(t, e) => Ty::ErrorUnion(
                Box::new(self.ast_to_hir_type(t)?),
                Box::new(self.ast_to_hir_type(e)?),
            ),
            AstType::Anytype => Ty::Infer(InferId(0)),
            AstType::ImplTrait(bounds) => {
                // `impl Trait` 降级为不透明类型：
                // - RPIT（返回位置）：隐藏类型由函数体推断，外部不可见
                // - APIT（参数位置）：等价于一个新的泛型参数
                // 这里统一创建一个 OpaqueTy，其隐藏类型在后续推断中确定
                let opaque_id = self.alloc_def(format!("impl_trait_{}", self.next_def));
                // 记录 trait 边界到约束集合
                for bound in bounds {
                    if let AstType::Named { path, .. } = bound {
                        if let Ok(trait_id) = self.resolve_type_path(path) {
                            // 约束：opaque 类型必须实现该 trait
                            // 这里简化为 Infer，实际的 trait 约束在推断阶段检查
                            let _ = trait_id;
                        }
                    }
                }
                Ty::Opaque(opaque_id)
            }
            AstType::Dyn(bounds) => {
                // `dyn Trait`：trait 对象。运行时是 `{ data, vtable }` 胖指针。
                // 取第一个 trait 边界作为主 trait（`dyn Error + Send` 的
                // `Send` 是 auto trait，主 trait 是 `Error`）。
                let mut trait_id = None;
                for bound in bounds {
                    if let AstType::Named { path, .. } = bound {
                        if let Ok(id) = self.resolve_type_path(path) {
                            trait_id = Some(id);
                            break;
                        }
                    }
                }
                let Some(id) = trait_id else {
                    return Err(CompileError::type_error(
                        "`dyn` requires at least one trait bound",
                    ));
                };
                Ty::Dyn(id)
            }
        })
    }

    /// 展平别名类型，返回别名目标。
    ///
    /// 别名不是独立的名义类型：`type MyInt = i32` 之后 `MyInt` 与 `i32` 必须
    /// 完全等价，`Vec<MyInt>` 也得能被当成 `Vec<i32>` 统一，否则泛型推断会把
    /// `T` 绑成 `MyInt`、再遇 `i32` 就报「推断成两个不同的类型」
    /// （见 `tests/cases/70_type_alias.zr`）。
    ///
    /// 别名目标本身可能又是别名（`type A = B; type B = i32`），要一路走到
    /// 叶子；深度上限挡掉 `type A = B; type B = A` 的循环定义。
    fn inline_alias_ty(&self, ty: &Ty, depth: usize) -> Ty {
        const MAX_ALIAS_DEPTH: usize = 64;
        if depth >= MAX_ALIAS_DEPTH {
            return ty.clone();
        }
        let Ty::Named(id, args) = ty else {
            return ty.clone();
        };
        match self.type_defs.get(id) {
            // 泛型别名：目标里是 `Infer` 占位符，必须按实参替换。
            //
            // 直接展开不替换会让 `type Ref<T> = *const T; let r: Ref<i32> = ..`
            // 退化成 `*const <推断类型>`——推断到别处之后类型就飘了，而这里
            // 完全不会报错。占位符是按声明顺序排的，和实参一一对应。
            Some(hir::HirTypeDef::Alias { target, .. }) => {
                let substituted = self.subst_def_ty(*id, args, target);
                self.inline_alias_ty(&substituted, depth + 1)
            }
            _ => ty.clone(),
        }
    }

    fn infer_expr_ty(&mut self, e: &Expression) -> Result<Ty> {
        // 快速类型推断（用于递归调用，避免无限递归）
        Ok(match e {
            Expression::Literal(AstLiteral::Int(_, suffix), _) => suffix
                .as_deref()
                .and_then(|s| self.int_suffix_ty(s))
                .unwrap_or(Ty::Int(32)),
            Expression::Literal(AstLiteral::Float(_, suffix), _) => match suffix.as_deref() {
                Some("f8") => Ty::Float(8),
                Some("f16") => Ty::Float(16),
                Some("f32") => Ty::Float(32),
                _ => Ty::Float(64),
            },
            Expression::Literal(ast::Literal::String(_), _) => Ty::Str,
            Expression::Literal(ast::Literal::Char(_), _) => Ty::Char,
            Expression::Literal(ast::Literal::Bool(_), _) => Ty::Bool,
            Expression::Literal(AstLiteral::Byte(_), _) => Ty::Uint(8),
            Expression::Literal(AstLiteral::ByteString(_), _) => Ty::Slice(
                Box::new(Ty::Uint(8)),
                crate::parser::ast::Mutability::Immutable,
            ),
            Expression::Variable(name, _) => {
                // 局部变量：从当前作用域查它的类型。早先这里只查模块级符号，
                // 局部变量一律退化成未解推断变量——于是 `1 + p`（p 是局部
                // 指针）在右侧检测不到指针类型。
                if let Some(ty) = self.local_var_ty(name) {
                    ty
                } else if let Some(id) = self.symbols.get(name).copied() {
                    if let Some(t) = self.consts.get(&id) {
                        t.ty.clone()
                    } else if let Some(t) = self.statics.get(&id) {
                        t.ty.clone()
                    } else if let Some(f) = self.functions.get(&id) {
                        // 函数名的类型是**函数类型**，不是返回类型。返回
                        // 类型会让上游 `Call` 分支取不到签名，`let x = f();`
                        // 的 `x` 就退化成 `_`（f32 返回值尤其致命）。
                        Ty::Function {
                            params: f.params.iter().map(|p| p.ty.clone()).collect(),
                            ret: Box::new(f.return_type.clone()),
                            is_async: f.is_async,
                            abi: hir::Abi::Zr,
                        }
                    } else {
                        self.infer.fresh_infer(e.span())
                    }
                } else {
                    self.infer.fresh_infer(e.span())
                }
            }
            Expression::Path(path, _) => {
                // `TcpListener::bind` 这类**路径**调用：早先 `infer_expr_ty`
                // 没有 `Path` 分支，`TcpListener::bind(addr)?` 里 `?` 拿不到
                // 调用返回类型，退化成"当前函数返回类型"兜底——`serve_echo`
                // 返回 `Result<(), NetError>`，于是 `listener` 被推断成 `()`
                // （`Tuple([])`），`.accept` 报「tuple index out of range:
                // accept」。与 `Variable` 分支一样把路径解析成函数类型，让
                // `?`（及其它快速推断）拿到真实返回载荷。
                if let Ok(id) = self.resolve_path(path, e.span()) {
                    if let Some(f) = self.functions.get(&id) {
                        Ty::Function {
                            params: f.params.iter().map(|p| p.ty.clone()).collect(),
                            ret: Box::new(f.return_type.clone()),
                            is_async: f.is_async,
                            abi: hir::Abi::Zr,
                        }
                    } else {
                        self.infer.fresh_infer(e.span())
                    }
                } else {
                    self.infer.fresh_infer(e.span())
                }
            }
            Expression::Binary(
                _,
                ast::BinaryOp::Eq
                | ast::BinaryOp::Ne
                | ast::BinaryOp::Lt
                | ast::BinaryOp::Gt
                | ast::BinaryOp::Le
                | ast::BinaryOp::Ge
                | ast::BinaryOp::And
                | ast::BinaryOp::Or,
                _,
                _,
            ) => Ty::Bool,
            Expression::Binary(..) => self.infer.fresh_infer(e.span()),
            Expression::Unary(ast::UnaryOp::Not, _, _) => Ty::Bool,
            Expression::Unary(..) => self.infer.fresh_infer(e.span()),
            // `x as T` 的结果类型就是 T，不需要推断。
            //
            // 漏掉这一条会让 `let y = x as f64;` 的 y 落到一个未解决的
            // 推断变量上：后续 `y as i32` 直接报 "cannot cast `_` to i32"。
            Expression::Cast(_, target, _) => self.ast_to_hir_type(target)?,
            Expression::Call(callee, _, _) => {
                // 泛型函数的签名里有共享的推断变量（`T` 的占位符）。把它的
                // 返回类型直接交给 `let` 去 unify，会让第一次实例化（比如
                // `identity(7)` 的 i32）把占位符**永久**绑死，后续的
                // `identity(9i64)` 就报 i64/i32 不匹配。返回一个全新的推断
                // 变量，由真正的调用检查去决定类型。
                if self.generic_call_target(callee).is_some() {
                    return Ok(self.infer.fresh_infer(e.span()));
                }
                let cty = self.infer_expr_ty(callee)?;
                match cty {
                    Ty::Function { ret, .. } => *ret,
                    _ => self.infer.fresh_infer(e.span()),
                }
            }
            Expression::Block(b, _) => b
                .tail
                .as_ref()
                .map(|t| self.infer_expr_ty(t))
                .unwrap_or(Ok(Ty::Unit))?,
            Expression::If(_, then_b, else_b, _) => {
                if let Some(eb) = else_b {
                    let t = self
                        .infer_expr_ty(&Expression::Block(then_b.clone(), ast::Span::unknown()))?;
                    self.infer_expr_ty(eb).unwrap_or(t)
                } else {
                    Ty::Unit
                }
            }
            Expression::Match(_, arms, _) => {
                if let Some(first) = arms.first() {
                    self.infer_expr_ty(&first.body)?
                } else {
                    Ty::Never
                }
            }
            Expression::Return(_, _) => Ty::Never,
            Expression::Break(_, _, _) => Ty::Never,
            Expression::Continue(_, _) => Ty::Never,
            Expression::Loop(_, _) => Ty::Never,
            Expression::Throw(_, _) => Ty::Never,
            Expression::StructInit(path, _, _, _) => {
                let id = self
                    .resolve_path(path, ast::Span::unknown())
                    .unwrap_or(DefId(0));
                // 泛型结构体字面量的类型实参要从字段值推，先给个推断变量，
                // 不能直接写 `Named(id, [])`（那会和 `Named(id, [i32])`
                // 统一失败）。
                if self.type_generic_params.contains_key(&id) {
                    self.infer.fresh_infer(e.span())
                } else {
                    Ty::Named(id, Vec::new())
                }
            }
            Expression::Tuple(elems, _) => Ty::Tuple(
                elems
                    .iter()
                    .map(|e| self.infer_expr_ty(e))
                    .collect::<Result<_>>()?,
            ),
            Expression::Array(elems, repeat, _) => {
                let elem = if let Some(first) = elems.first() {
                    self.infer_expr_ty(first)?
                } else {
                    self.infer.fresh_infer(e.span())
                };
                let len = repeat
                    .as_ref()
                    .and_then(|r| match &**r {
                        Expression::Literal(AstLiteral::Int(n, _), _) => Some(*n),
                        _ => None,
                    })
                    .unwrap_or(elems.len() as i128);
                Ty::Array(Box::new(elem), const_array_len(len, e.span()))
            }
            Expression::Lambda(params, ret, _, _) => {
                // 形参的类型：写了标注就用标注，没写就新建一个推断变量。
                //
                // 早先这里一律用空列表，推断出来的闭包类型是 `fn() -> _`——
                // 参数全丢了，于是 `let f = |x: i32| ...` 报"类型不匹配"。
                let mut param_tys = Vec::with_capacity(params.len());
                for p in params {
                    param_tys.push(match &p.ty {
                        Some(t) => self.ast_to_hir_type(t)?,
                        None => self.infer.fresh_infer(e.span()),
                    });
                }
                let ret_ty = ret
                    .as_ref()
                    .map(|t| self.ast_to_hir_type(t))
                    .unwrap_or_else(|| Ok(self.infer.fresh_infer(e.span())))?;
                Ty::Function {
                    params: param_tys,
                    ret: Box::new(ret_ty),
                    is_async: false,
                    abi: Abi::Zr,
                }
            }
            Expression::Await(_, _) => self.infer.fresh_infer(e.span()),
            Expression::Yield(_, _) => self.infer.fresh_infer(e.span()),
            Expression::Spawn(_, _) => self.infer.fresh_infer(e.span()),
            Expression::Unsafe(inner, _) => self.infer_expr_ty(inner)?,
            _ => self.infer.fresh_infer(e.span()),
        })
    }

    fn validate_cast(&self, from: &Ty, to: &Ty, span: ast::Span) -> Result<()> {
        match (from, to) {
            // 数值类型之间可以 cast
            (Ty::Int(_), Ty::Int(_))
            | (Ty::Uint(_), Ty::Uint(_))
            | (Ty::Int(_), Ty::Uint(_))
            | (Ty::Int(_), Ty::Float(_))
            | (Ty::Uint(_), Ty::Int(_))
            | (Ty::Uint(_), Ty::Float(_))
            | (Ty::Float(_), Ty::Int(_))
            | (Ty::Float(_), Ty::Uint(_))
            | (Ty::Float(_), Ty::Float(_)) => Ok(()),
            // 指针 ↔ 整数
            (Ty::Pointer(_, _), Ty::Uint(_) | Ty::Int(_))
            | (Ty::Uint(_) | Ty::Int(_), Ty::Pointer(_, _)) => Ok(()),
            // 指针类型之间（unsafe）
            (Ty::Pointer(_, _), Ty::Pointer(_, _)) => Ok(()),
            // bool ↔ 整数
            (Ty::Bool, Ty::Int(_) | Ty::Uint(_)) | (Ty::Int(_) | Ty::Uint(_), Ty::Bool) => Ok(()),
            // 字符可以转成整数（取它的码点），也可以从整数转回来。
            (Ty::Char, Ty::Int(_) | Ty::Uint(_)) => Ok(()),
            (Ty::Int(_) | Ty::Uint(_), Ty::Char) => Ok(()),
            _ => Err(CompileError::type_error(format!(
                "cannot cast `{}` to `{}`",
                self.infer.type_name(from),
                self.infer.type_name(to)
            ))
            .with_span(span)),
        }
    }

    fn check_assignable(&self, expr: &HirExpr) -> Result<()> {
        match &expr.kind {
            ExprKind::Local(_) => Ok(()),
            // 全局 `static mut` 有真实的数据符号（落 `.data`），可以当赋值目标。
            // 只读 `static` 同样有存储，但下面的 `require_mutable_binding` 会
            // 挡掉对它写——这里只管「是不是一个能写的位置」，不管可写性。
            ExprKind::Def(id) => {
                if self.statics.contains_key(id) {
                    Ok(())
                } else {
                    Err(CompileError::type_error(
                        "expression is not assignable (expected variable, field, deref, or index)",
                    )
                    .with_span(expr.span))
                }
            }
            ExprKind::Field(inner, _) => self.check_assignable(inner),
            // `*p = v` / `p[i] = v`：先验证写入目标本身可写。
            ExprKind::Deref(inner) => {
                self.require_mutable_place(inner)?;
                self.check_assignable(inner)
            }
            ExprKind::Index(inner, _) => {
                self.require_mutable_place(inner)?;
                self.check_assignable(inner)
            }
            _ => Err(CompileError::type_error(
                "expression is not assignable (expected variable, field, deref, or index)",
            )
            .with_span(expr.span)),
        }
    }

    /// 赋值目标必须是 `let mut` 声明过的绑定。
    ///
    /// 字段 / 数组元素沿用根变量的可变性；通过 `*mut T` / `&mut T` 写入
    /// 时，可变性由指针类型决定（`require_mutable_place` 已经查过），
    /// 指针变量本身不需要 `mut`。
    fn require_mutable_binding(&self, place: &HirExpr) -> Result<()> {
        match &place.kind {
            ExprKind::Local(id) => {
                if self.mutable_locals.contains(id) {
                    Ok(())
                } else {
                    Err(CompileError::type_error(
                        "cannot assign to immutable binding (declare it with `let mut`)",
                    )
                    .with_span(place.span))
                }
            }
            // `static mut` 有数据符号、可以写；只读 `static` 同样有存储但不能写。
            // 注意这条必须显式列出：下面的通配分支是 `Ok(())`，不加就会把
            // `LIMIT = 41` 这种对只读静态量的赋值放行（见 `tests/cases/72_statics.zr`）。
            ExprKind::Def(id) => {
                match self.statics.get(id) {
                    Some(s) if s.is_mut => Ok(()),
                    _ => Err(CompileError::type_error(format!(
                        "cannot assign to immutable static `{}`",
                        self.def_names.get(id).cloned().unwrap_or_default()
                    ))
                    .with_span(place.span)),
                }
            }
            ExprKind::Field(inner, _) => {
                if matches!(
                    self.infer.resolve(&inner.ty),
                    Ty::Pointer(..) | Ty::Reference(..)
                ) {
                    Ok(())
                } else {
                    self.require_mutable_binding(inner)
                }
            }
            ExprKind::Deref(_) => Ok(()),
            ExprKind::Index(inner, _) => {
                if matches!(
                    self.infer.resolve(&inner.ty),
                    Ty::Pointer(..) | Ty::Reference(..) | Ty::Slice(..)
                ) {
                    Ok(())
                } else {
                    self.require_mutable_binding(inner)
                }
            }
            _ => Ok(()),
        }
    }

    /// 写入目标必须"可写"：
    ///
    /// - 字符串字面量住在 `.rodata`（只读段），对 `s[i]` 赋值在运行期是
    ///   段错误——在类型检查直接挡住；
    /// - 通过 `*const T` / `&T` 写入同理：地址是只读的，`*p = v` 会安静地
    ///   写坏只读页。要写就得拿 `*mut T` / `&mut T`。
    fn require_mutable_place(&self, place: &HirExpr) -> Result<()> {
        match self.infer.resolve(&place.ty) {
            Ty::Str => Err(CompileError::type_error(
                "cannot assign into a string (string literals live in read-only memory)",
            )
            .with_span(place.span)),
            Ty::Pointer(_, hir::Mutability::Immutable)
            | Ty::Reference(_, hir::Mutability::Immutable) => Err(CompileError::type_error(
                "cannot assign through an immutable pointer (use `&mut` and a `*mut` type)",
            )
            .with_span(place.span)),
            _ => Ok(()),
        }
    }

    // ==================================================================
    // AST → HIR 转换辅助
    // ==================================================================

    fn fn_to_hir(&self, f: &ast::Function, _id: DefId) -> hir::HirFunction {
        hir::HirFunction {
            name: f.name.clone(),
            params: Vec::new(),
            return_type: Ty::Infer(InferId(0)),
            body: None,
            is_async: f.is_async,
            async_output: None,
            is_comptime: f.is_comptime,
            abi: hir::Abi::Zr,
            attrs: convert_attrs(&f.attrs),
        }
    }

    fn lower_const_expr(&mut self, e: &Expression) -> Result<HirExpr> {
        self.check_expr(e, &Ty::Infer(InferId(0)))
    }

    fn check_const_expr(&mut self, e: &Expression, expected: &Ty) -> Result<HirExpr> {
        self.check_expr(e, expected)
    }

    // ==================================================================
    // 编译期求值（CTFE）
    // ==================================================================

    /// 编译期求值：把表达式折成一个字面量。
    ///
    /// 折不出来时返回 `Err`，调用方据此报"不是编译期常量"。
    fn eval_const_expr(&self, e: &Expression) -> Result<Box<HirLiteral>> {
        match e {
            Expression::Literal(lit, _) => Ok(Box::new(self.ast_literal_to_hir(lit)?)),

            // `comptime { 40 + 2 }` 里的 `{ ... }` 在语法上就是一个块，
            // 值取块的最后一条表达式。不处理块的话 `comptime { .. }`
            // 这个最自然的写法会直接被判成"不是常量"。
            Expression::Block(b, _) => {
                // 尾表达式优先；没有尾表达式就退而取最后一条 `let` 的初值
                if let Some(tail) = &b.tail {
                    return self.eval_const_expr(tail);
                }
                match b.stmts.last() {
                    Some(ast::Statement::Expr(inner, _)) => self.eval_const_expr(inner),
                    Some(ast::Statement::Let(_, _, init, _))
                    | Some(ast::Statement::Var(_, _, init, _)) => {
                        let init = init.as_ref().ok_or_else(|| {
                            CompileError::internal("comptime binding without initializer")
                        })?;
                        self.eval_const_expr(init)
                    }
                    _ => Err(CompileError::internal(
                        "comptime block has no constant tail expression",
                    )),
                }
            }

            Expression::Binary(lhs, op, rhs, span) => {
                let l = self.eval_const_expr(lhs)?;
                let r = self.eval_const_expr(rhs)?;
                self.const_binary(*op, &l, &r, *span)
            }

            Expression::Unary(op, operand, span) => {
                let v = self.eval_const_expr(operand)?;
                self.const_unary(*op, &v, *span)
            }

            // `comptime comptime 6 * 7`：内层已经是常量，外层直接透传
            Expression::Comptime(inner, _) => self.eval_const_expr(inner),

            // `comptime fact(4)`：调用 comptime 函数并在编译期求值。
            //
            // 走 AST 而不是 HIR：函数体是顺序检查的，用到 `fact` 的那个函数
            // 完全可能排在 `fact` 自己前面，这时它的 HIR 还不存在。
            Expression::Call(callee, args, span) => {
                let name = match callee.as_ref() {
                    ast::Expression::Variable(n, _) => n.clone(),
                    _ => {
                        return Err(CompileError::internal(
                            "only direct calls to `comptime fn` are supported \
                             in compile-time contexts",
                        )
                        .with_span(*span))
                    }
                };
                let mut ev = crate::comptime::AstEvaluator::new(&self.ast_fns);
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    match a {
                        ast::Argument::Pos(e) => vals.push(*self.eval_const_expr(e)?),
                        ast::Argument::Named(_, _) => {
                            return Err(CompileError::internal(
                                "named arguments are not supported in compile-time contexts",
                            )
                            .with_span(*span))
                        }
                    }
                }
                let value = ev
                    .call(&name, &vals)
                    .map_err(|e| CompileError::internal(e.message).with_span(*span))?;
                Ok(Box::new(value))
            }

            // 块里的 `let` 绑定要让后续表达式看得到，否则
            // `comptime { let x = 6; x * 7 }` 会在 `x` 上报错。
            // 这里不做作用域弹出：整个 `eval_const_expr` 的求值过程中
            // 名字到值的绑定是单调累积的，与运行期的遮蔽规则差异只在
            // 同名遮蔽的极端情形。
            Expression::Variable(name, span) => {
                // 查找常量定义并求值
                if let Some(id) = self.symbols.get(name).copied() {
                    if let Some(c) = self.consts.get(&id) {
                        return self.eval_const_from_hir(&c.value);
                    }
                }
                Err(CompileError::internal(format!(
                    "value `{name}` cannot be used in compile-time context"
                ))
                .with_span(*span))
            }

            // `comptime Point { x: 1, y: 2, ..base }`：字段按书写顺序求值，
            // 未写出的字段从编译期 base 聚合值继承。
            Expression::StructInit(_, fields, base, _) => {
                let mut items = if let Some(base) = base {
                    match *self.eval_const_expr(base)? {
                        HirLiteral::Composite(items) => items,
                        _ => {
                            return Err(CompileError::internal(
                                "struct update base is not a compile-time struct value",
                            ))
                        }
                    }
                } else {
                    Vec::new()
                };
                for (name, e) in fields {
                    let value = *self.eval_const_expr(e)?;
                    if let Some(slot) = items.iter_mut().find(|(n, _)| n == name) {
                        slot.1 = value;
                    } else {
                        items.push((name.clone(), value));
                    }
                }
                Ok(Box::new(HirLiteral::Composite(items)))
            }

            // `comptime p.x`：对编译期聚合值取字段。
            Expression::Field(base, name, span) => {
                let value = *self.eval_const_expr(base)?;
                match value {
                    HirLiteral::Composite(items) => items
                        .into_iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| Box::new(v))
                        .ok_or_else(|| {
                            CompileError::internal(format!(
                                "no field `{name}` in compile-time struct value"
                            ))
                            .with_span(*span)
                        }),
                    _ => Err(CompileError::internal(
                        "field access on a non-struct compile-time value",
                    )
                    .with_span(*span)),
                }
            }

            _ => Err(CompileError::internal(
                "expression is not a compile-time constant",
            )),
        }
    }

    /// AST 字面量 -> HIR 字面量。
    fn ast_literal_to_hir(&self, lit: &ast::Literal) -> Result<HirLiteral> {
        Ok(match lit {
            ast::Literal::Int(v, _) => HirLiteral::Int(*v),
            ast::Literal::Float(v, _) => HirLiteral::Float(*v),
            ast::Literal::String(v) => HirLiteral::String(v.clone()),
            ast::Literal::Char(c) => HirLiteral::Char(*c),
            ast::Literal::Bool(b) => HirLiteral::Bool(*b),
            ast::Literal::Byte(b) => HirLiteral::Int(*b as i128),
            ast::Literal::ByteString(b) => {
                HirLiteral::String(String::from_utf8_lossy(b).into_owned())
            }
        })
    }

    /// 编译期常量相等比较。
    fn const_eq(&self, l: &HirLiteral, r: &HirLiteral) -> bool {
        match (l, r) {
            (HirLiteral::Int(a), HirLiteral::Int(b)) => a == b,
            (HirLiteral::Float(a), HirLiteral::Float(b)) => a == b,
            (HirLiteral::Bool(a), HirLiteral::Bool(b)) => a == b,
            (HirLiteral::String(a), HirLiteral::String(b)) => a == b,
            (HirLiteral::Char(a), HirLiteral::Char(b)) => a == b,
            // 跨类型（`1 == 1.0`）在编译期不算常量
            _ => false,
        }
    }

    /// 编译期折叠一个二元运算。
    fn const_binary(
        &self,
        op: ast::BinaryOp,
        l: &HirLiteral,
        r: &HirLiteral,
        span: Span,
    ) -> Result<Box<HirLiteral>> {
        use HirLiteral::*;
        match op {
            ast::BinaryOp::Add
            | ast::BinaryOp::Sub
            | ast::BinaryOp::Mul
            | ast::BinaryOp::Div
            | ast::BinaryOp::Rem
            | ast::BinaryOp::BitAnd
            | ast::BinaryOp::BitOr
            | ast::BinaryOp::BitXor
            | ast::BinaryOp::Shl
            | ast::BinaryOp::Shr => match (l, r) {
                (Float(l), Float(r))
                    if !matches!(
                        op,
                        ast::BinaryOp::Rem
                            | ast::BinaryOp::BitAnd
                            | ast::BinaryOp::BitOr
                            | ast::BinaryOp::BitXor
                            | ast::BinaryOp::Shl
                            | ast::BinaryOp::Shr
                    ) =>
                {
                    if matches!(op, ast::BinaryOp::Div) && *r == 0.0 {
                        return Err(CompileError::internal("division by zero").with_span(span));
                    }
                    Ok(Box::new(Float(match op {
                        ast::BinaryOp::Add => l + r,
                        ast::BinaryOp::Sub => l - r,
                        ast::BinaryOp::Mul => l * r,
                        _ => l / r,
                    })))
                }
                (Int(l), Int(r)) => {
                    // 除以 0 必须在编译期报错，不能留到运行期
                    if matches!(op, ast::BinaryOp::Div | ast::BinaryOp::Rem) && *r == 0 {
                        return Err(CompileError::internal("division by zero").with_span(span));
                    }
                    Ok(Box::new(Int(match op {
                        ast::BinaryOp::Add => l.wrapping_add(*r),
                        ast::BinaryOp::Sub => l.wrapping_sub(*r),
                        ast::BinaryOp::Mul => l.wrapping_mul(*r),
                        ast::BinaryOp::Div => l.wrapping_div(*r),
                        ast::BinaryOp::Rem => l.wrapping_rem(*r),
                        ast::BinaryOp::BitAnd => l & r,
                        ast::BinaryOp::BitOr => l | r,
                        ast::BinaryOp::BitXor => l ^ r,
                        ast::BinaryOp::Shl => l.wrapping_shl(*r as u32),
                        _ => l.wrapping_shr(*r as u32),
                    })))
                }
                _ => Err(CompileError::internal(
                    "mismatched operand types in compile-time expression",
                )
                .with_span(span)),
            },
            ast::BinaryOp::Eq => Ok(Box::new(Bool(self.const_eq(l, r)))),
            ast::BinaryOp::Ne => Ok(Box::new(Bool(!self.const_eq(l, r)))),
            ast::BinaryOp::Lt => Ok(Box::new(Bool(self.const_ord(l, r, span)?.is_lt()))),
            ast::BinaryOp::Le => Ok(Box::new(Bool(self.const_ord(l, r, span)?.is_le()))),
            ast::BinaryOp::Gt => Ok(Box::new(Bool(self.const_ord(l, r, span)?.is_gt()))),
            ast::BinaryOp::Ge => Ok(Box::new(Bool(self.const_ord(l, r, span)?.is_ge()))),
            // 逻辑运算可短路：左边定了就不看右边，但仍要求右边能求值
            ast::BinaryOp::And => match l {
                Bool(false) => Ok(Box::new(Bool(false))),
                Bool(true) => match r {
                    Bool(b) => Ok(Box::new(Bool(*b))),
                    _ => Err(CompileError::internal("'&&' needs booleans").with_span(span)),
                },
                _ => Err(CompileError::internal("'&&' needs booleans").with_span(span)),
            },
            ast::BinaryOp::Or => match l {
                Bool(true) => Ok(Box::new(Bool(true))),
                Bool(false) => match r {
                    Bool(b) => Ok(Box::new(Bool(*b))),
                    _ => Err(CompileError::internal("'||' needs booleans").with_span(span)),
                },
                _ => Err(CompileError::internal("'||' needs booleans").with_span(span)),
            },
            _ => Err(
                CompileError::internal("operator not allowed in compile-time context")
                    .with_span(span),
            ),
        }
    }

    /// 编译期折叠一个一元运算。
    fn const_unary(&self, op: ast::UnaryOp, v: &HirLiteral, span: Span) -> Result<Box<HirLiteral>> {
        use HirLiteral::*;
        match op {
            ast::UnaryOp::Neg => match v {
                Int(i) => Ok(Box::new(Int(i.wrapping_neg()))),
                Float(f) => Ok(Box::new(Float(-f))),
                _ => Err(
                    CompileError::internal("cannot negate this value at compile time")
                        .with_span(span),
                ),
            },
            // `!` 按类型分派：bool 是逻辑非，整数是位非
            ast::UnaryOp::Not => match v {
                Bool(b) => Ok(Box::new(Bool(!b))),
                Int(i) => Ok(Box::new(Int(!i))),
                _ => {
                    Err(CompileError::internal("cannot apply '!' at compile time").with_span(span))
                }
            },
            ast::UnaryOp::BitNot => match v {
                Int(i) => Ok(Box::new(Int(!i))),
                _ => Err(
                    CompileError::internal("'~' needs an integer at compile time").with_span(span),
                ),
            },
            _ => Err(
                CompileError::internal("unary operator not allowed in compile-time context")
                    .with_span(span),
            ),
        }
    }

    /// 编译期常量大小比较；类型不可比时报错。
    fn const_ord(&self, l: &HirLiteral, r: &HirLiteral, span: Span) -> Result<std::cmp::Ordering> {
        match (l, r) {
            (HirLiteral::Int(a), HirLiteral::Int(b)) => Ok(a.cmp(b)),
            (HirLiteral::Float(a), HirLiteral::Float(b)) => a.partial_cmp(b).ok_or_else(|| {
                CompileError::internal("cannot compare NaN at compile time").with_span(span)
            }),
            (HirLiteral::Char(a), HirLiteral::Char(b)) => Ok(a.cmp(b)),
            _ => Err(
                CompileError::internal("mismatched operand types in compile-time comparison")
                    .with_span(span),
            ),
        }
    }

    /// 从 HIR 表达式求值（常量定义的右值）。
    ///
    /// HIR 的运算符与 AST 同名同形，直接复用同一套折叠逻辑。
    fn eval_const_from_hir(&self, e: &HirExpr) -> Result<Box<HirLiteral>> {
        match &e.kind {
            hir::ExprKind::Literal(l) => Ok(Box::new(l.clone())),
            hir::ExprKind::Binary(op, lhs, rhs) => {
                let l = self.eval_const_from_hir(lhs)?;
                let r = self.eval_const_from_hir(rhs)?;
                self.const_binary(hir_binary_to_ast(*op), &l, &r, e.span)
            }
            hir::ExprKind::Unary(op, operand) => {
                let v = self.eval_const_from_hir(operand)?;
                self.const_unary(hir_unary_to_ast(*op), &v, e.span)
            }
            hir::ExprKind::Comptime(folded) => Ok(folded.clone()),
            hir::ExprKind::Cast(inner) => self.eval_const_from_hir(inner),
            _ => Err(
                CompileError::internal("const definition is not a compile-time constant")
                    .with_span(e.span),
            ),
        }
    }

    // ==================================================================
    // 作用域管理
    // ==================================================================

    fn push_scope(&mut self) {
        self.scope_depth += 1;
        self.scope_stack.push(Vec::new());
        self.local_scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        if let Some(locals) = self.scope_stack.pop() {
            for l in locals {
                self.locals.remove(&l);
                self.mutable_locals.remove(&l);
                self.infer.locals.remove(&l);
            }
        }
        self.local_scopes.pop();
        self.scope_depth = self.scope_depth.saturating_sub(1);
    }

    /// 在当前作用域里绑定一个局部变量名，返回它的 `DefId`。
    ///
    /// 刻意**不**写入全局 `symbols`——局部名不应该泄漏到文件作用域，
    /// 否则不同函数里的同名变量会互相覆盖。
    fn bind_local(&mut self, name: &str, ty: &Ty) -> (DefId, LocalId) {
        let def = DefId(self.next_def);
        self.next_def += 1;
        self.def_names.insert(def, name.to_string());

        let local = self.alloc_local();
        self.locals.insert(local, ty.clone());
        self.infer.locals.insert(local, ty.clone());
        self.locals_by_def.insert(def, local);

        if let Some(layer) = self.scope_stack.last_mut() {
            layer.push(local);
        }
        if let Some(layer) = self.local_scopes.last_mut() {
            layer.insert(name.to_string(), def);
        }
        (def, local)
    }

    /// 从最内层作用域向外查找局部变量名。
    fn resolve_local(&self, name: &str) -> Option<DefId> {
        for layer in self.local_scopes.iter().rev() {
            if let Some(&def) = layer.get(name) {
                return Some(def);
            }
        }
        None
    }

    fn current_returns(&self) -> bool {
        false
    }

    fn check_unused(&self, _functions: &[hir::HirFunction]) -> Result<()> {
        // 检查未使用的变量 / 导入 / 参数
        Ok(())
    }

    /// 合并 `if` / `match` 各分支的结果类型。
    ///
    /// `Never` 是底类型：发散分支（以 `return` / `throw` 结尾）没有值可交出去，
    /// 应当直接取另一支——`unify_or(a, Never) == a`。Rust 也是这么做的。
    ///
    /// 分支类型真的不一致时返回 `Unit`，而不是一个"未知"的推断变量：不一致
    /// 本身已经是一条应当报出来的错误，不该用一个占位类型继续往下糊。
    ///
    /// 早先这里对任何不一致都返回 `Ty::Infer(InferId(0))`，那是一颗定时炸弹：
    /// `InferId(0)` 不是哨兵，是个**活的**推断变量，长编译里可能早就被绑定了
    /// （`std/io.zr` 里绑成了 `u8`）。下游 `subtype` 一 `resolve` 就解出那个旧值，
    /// 凭空报出一条 "expected `()`, found `u8`" 这类毫无根据的错，而且报错指向
    /// 的位置跟真正的问题毫无关系。
    fn unify_or(&self, a: &Ty, b: &Ty) -> Ty {
        if a == b {
            return a.clone();
        }
        if matches!(a, Ty::Never) {
            return b.clone();
        }
        if matches!(b, Ty::Never) {
            return a.clone();
        }
        Ty::Unit
    }

    fn dummy_expr(&self) -> HirExpr {
        HirExpr {
            kind: ExprKind::Literal(hir::HirLiteral::Int(0)),
            ty: Ty::Int(32),
            span: ast::Span::unknown(),
        }
    }

    fn ast_unary_op(&self, op: ast::UnaryOp) -> hir::UnaryOp {
        match op {
            ast::UnaryOp::Neg => hir::UnaryOp::Neg,
            ast::UnaryOp::Not => hir::UnaryOp::Not,
            ast::UnaryOp::BitNot => hir::UnaryOp::BitNot,
            ast::UnaryOp::Deref => hir::UnaryOp::Deref,
            ast::UnaryOp::AddrOf => hir::UnaryOp::AddrOf,
        }
    }

    fn ast_binary_op(&self, op: ast::BinaryOp) -> hir::BinaryOp {
        match op {
            ast::BinaryOp::Add => hir::BinaryOp::Add,
            ast::BinaryOp::Sub => hir::BinaryOp::Sub,
            ast::BinaryOp::Mul => hir::BinaryOp::Mul,
            ast::BinaryOp::Div => hir::BinaryOp::Div,
            ast::BinaryOp::Rem => hir::BinaryOp::Rem,
            ast::BinaryOp::BitAnd => hir::BinaryOp::BitAnd,
            ast::BinaryOp::BitOr => hir::BinaryOp::BitOr,
            ast::BinaryOp::BitXor => hir::BinaryOp::BitXor,
            ast::BinaryOp::Shl => hir::BinaryOp::Shl,
            ast::BinaryOp::Shr => hir::BinaryOp::Shr,
            ast::BinaryOp::Eq => hir::BinaryOp::Eq,
            ast::BinaryOp::Ne => hir::BinaryOp::Ne,
            ast::BinaryOp::Lt => hir::BinaryOp::Lt,
            ast::BinaryOp::Gt => hir::BinaryOp::Gt,
            ast::BinaryOp::Le => hir::BinaryOp::Le,
            ast::BinaryOp::Ge => hir::BinaryOp::Ge,
            ast::BinaryOp::And => hir::BinaryOp::And,
            ast::BinaryOp::Or => hir::BinaryOp::Or,
            ast::BinaryOp::Range => hir::BinaryOp::Range,
            ast::BinaryOp::RangeInclusive => hir::BinaryOp::RangeInclusive,
        }
    }

    fn assign_to_binary(&self, op: ast::AssignOp) -> hir::BinaryOp {
        match op {
            ast::AssignOp::AddAssign => hir::BinaryOp::Add,
            ast::AssignOp::SubAssign => hir::BinaryOp::Sub,
            ast::AssignOp::MulAssign => hir::BinaryOp::Mul,
            ast::AssignOp::DivAssign => hir::BinaryOp::Div,
            ast::AssignOp::RemAssign => hir::BinaryOp::Rem,
            ast::AssignOp::AndAssign => hir::BinaryOp::BitAnd,
            ast::AssignOp::OrAssign => hir::BinaryOp::BitOr,
            ast::AssignOp::XorAssign => hir::BinaryOp::BitXor,
            ast::AssignOp::ShlAssign => hir::BinaryOp::Shl,
            ast::AssignOp::ShrAssign => hir::BinaryOp::Shr,
            ast::AssignOp::Assign => hir::BinaryOp::Add,
        }
    }

    fn lower_intrinsic(&self, k: ast::IntrinsicKind) -> hir::Intrinsic {
        match k {
            ast::IntrinsicKind::SizeOf => hir::Intrinsic::SizeOf,
            ast::IntrinsicKind::AlignOf => hir::Intrinsic::AlignOf,
            ast::IntrinsicKind::TypeOf => hir::Intrinsic::TypeOf,
            ast::IntrinsicKind::OffsetOf => hir::Intrinsic::OffsetOf,
            ast::IntrinsicKind::IntCast => hir::Intrinsic::IntCast,
            // 运行时内建：编译器处理为对应的 HIR 内建操作。
            ast::IntrinsicKind::Yield => hir::Intrinsic::Yield,
            ast::IntrinsicKind::NowMs => hir::Intrinsic::NowMs,
            ast::IntrinsicKind::CaptureBacktrace => hir::Intrinsic::CaptureBacktrace,
            ast::IntrinsicKind::Sum => hir::Intrinsic::Sum,
            ast::IntrinsicKind::Drop => hir::Intrinsic::Drop,
            ast::IntrinsicKind::Transmute => hir::Intrinsic::Transmute,
            ast::IntrinsicKind::Forget => hir::Intrinsic::Forget,
            ast::IntrinsicKind::Zeroed => hir::Intrinsic::Zeroed,
        }
    }
}

/// 计算结构体内存布局的辅助。
#[derive(Debug, Default)]
struct OffsetTracker {
    current: usize,
    max_align: usize,
}

/// 语法类型里最末一级名字（`a::b::Foo` → `Foo`）。
fn ast_type_name(ty: &AstType) -> Option<String> {
    match ty {
        AstType::Named { path, .. } => path.last().cloned(),
        // 基本类型也能写 `impl i32 { fn lt ... }`。以前这里只认命名类型，
        // 于是所有基本类型的 `impl` 块被整体丢掉：`min` / `max` 里的
        // `a.lt(b)` 报 "cannot access field `lt` on type `i32`"。
        AstType::I(w) => Some(format!("i{w}")),
        AstType::U(w) => Some(format!("u{w}")),
        AstType::F(w) => Some(format!("f{w}")),
        AstType::Bool => Some("bool".to_string()),
        AstType::Char => Some("char".to_string()),
        AstType::Str => Some("str".to_string()),
        _ => None,
    }
}

impl OffsetTracker {
    fn new() -> Self {
        Self::default()
    }

    fn alloc(&mut self, size: usize, align: usize) -> usize {
        self.current = align_up(self.current, align);
        let offset = self.current;
        self.current = offset + size;
        self.max_align = self.max_align.max(align);
        offset
    }
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

fn parse_int_type(name: &str) -> Option<u16> {
    match name {
        "i8" => Some(8),
        "i16" => Some(16),
        "i32" => Some(32),
        "i64" => Some(64),
        "i128" => Some(128),
        "u8" => Some(8),
        "u16" => Some(16),
        "u32" => Some(32),
        "u64" => Some(64),
        "u128" => Some(128),
        _ => None,
    }
}

/// 内建整数类型的 DefId。
///
/// 早先只用位宽当 DefId，于是 `i32` 与 `u32`（乃至 `i64` 与 `u64`）拿到
/// 同一个 DefId——两者被当成同一个类型。`impl Eq for i32` 与
/// `impl Eq for u32` 于是被一致性检查判成"同一个类型的两个实现"，直接
/// 编译不过；`isize` / `usize` 也一样。
///
/// 现在按「有符号 / 无符号 + 位宽」编码，最高位置 1 表示内建类型，与用户
/// 类型的 DefId（从小 1 开始发号）不会撞。
fn primitive_def_id(name: &str) -> Option<DefId> {
    let width = parse_int_type(name)?;
    let unsigned = name.starts_with('u');
    Some(DefId(
        0x8000_0000 | ((!unsigned as u32) << 16) | (width as u32 & 0xffff),
    ))
}

// ======================================================================
// Trait 一致性检查（Coherence）
// ======================================================================

/// 根据属性分类 trait：是否为 marker trait、是否为 auto trait。
fn classify_trait(t: &ast::TraitDef) -> (bool, bool) {
    let mut is_marker = false;
    let mut is_auto = false;
    for attr in &t.attrs {
        match attr.name.as_str() {
            "marker" => is_marker = true,
            "auto" => is_auto = true,
            _ => {}
        }
    }
    // 内建 auto traits：Send、Sync、Unpin
    if matches!(t.name.as_str(), "Send" | "Sync" | "Unpin" | "Sized") {
        is_auto = true;
        is_marker = true;
    }
    (is_marker, is_auto)
}

/// 孤儿规则：impl 只有在 trait 或至少一个类型参数是本地定义时才有效。
/// 在单 crate 编译器中，所有定义都是本地的，因此此规则主要为多 crate 扩展预留。
fn check_orphan_rule(
    impl_block: &hir::HirImpl,
    type_defs: &HashMap<DefId, hir::HirTypeDef>,
    trait_defs: &HashMap<DefId, hir::HirTrait>,
    span: Span,
) -> Result<()> {
    // 检查 trait 是否本地
    if let Some(trait_id) = impl_block.trait_id {
        if trait_defs.contains_key(&trait_id) {
            return Ok(()); // trait 是本地的
        }
    }
    // 检查目标类型是否本地
    match &impl_block.target {
        Ty::Named(def_id, _) => {
            if type_defs.contains_key(def_id) {
                return Ok(()); // 目标类型是本地的
            }
        }
        Ty::Pointer(_, _) | Ty::Reference(_, _) | Ty::Slice(_, _) | Ty::Array(_, _) => {
            // 指针/引用/切片/数组类型：检查内部类型是否本地
            return Ok(()); // 简化：允许对这些类型 impl
        }
        Ty::Tuple(_) | Ty::Function { .. } => {
            return Ok(()); // 元组/函数类型允许
        }
        _ => {}
    }
    Err(CompileError::coherence_error(format!(
        "cannot implement trait for type `{}`: either the trait or the type must be defined in the current crate",
        ty_name(&impl_block.target)
    ))
    .with_span(span))
}

/// 重叠检查：两个 impl 不能应用到同一个具体类型。
/// 对于 marker trait，允许重叠（多个 impl 可以共存）。
fn check_overlap(
    new_impl: &hir::HirImpl,
    existing_impls: &[hir::HirImpl],
    trait_defs: &HashMap<DefId, hir::HirTrait>,
) -> Result<()> {
    let new_trait = new_impl.trait_id;
    // 固有 impl（`impl Type`）不是 trait 实现，彼此不构成 coherence 冲突。
    // 方法重名由函数表/调用解析负责；这里只管 `impl Trait for Type`。
    if new_trait.is_none() {
        return Ok(());
    }
    // 只比较相同 trait 的 impl
    for existing in existing_impls {
        if existing.trait_id != new_trait {
            continue;
        }
        // 如果是 marker trait，允许多个 impl（不检查重叠）
        if let Some(tid) = new_trait {
            if let Some(td) = trait_defs.get(&tid) {
                if td.is_marker {
                    continue;
                }
            }
        }
        // 检查目标类型是否可能重叠
        if types_may_overlap(&new_impl.target, &existing.target) {
            // 特化：如果一个 impl 比另一个更 specific，则允许重叠
            let new_more_specific = is_more_specific(new_impl, existing);
            let existing_more_specific = is_more_specific(existing, new_impl);
            if new_more_specific || existing_more_specific {
                continue;
            }
            return Err(CompileError::coherence_error(format!(
                "conflicting implementations: type `{}` has multiple impls of the same trait",
                ty_name(&new_impl.target)
            ))
            .with_span(new_impl.span));
        }
    }
    Ok(())
}

/// 判断 `a` 是否比 `b` 更 specific（用于特化）。
/// 更 specific 的 impl 可以覆盖较不 specific 的 impl。
/// 规则：
///   - 具体类型 > 泛型参数
///   - 有更多 where 约束的更 specific
fn is_more_specific(a: &hir::HirImpl, b: &hir::HirImpl) -> bool {
    // 具体类型比泛型参数更 specific
    let a_is_concrete = !matches!(a.target, Ty::Infer(_));
    let b_is_concrete = !matches!(b.target, Ty::Infer(_));
    if a_is_concrete && !b_is_concrete {
        return true;
    }
    if !a_is_concrete && b_is_concrete {
        return false;
    }
    // 约束更多的更 specific
    a.where_clause.len() > b.where_clause.len()
}

/// 若 `ty`（解析后）是指针或引用，返回其指向的类型。
///
/// 解引用 / 索引都要拿它定结果类型。这里同时接受 `Pointer` 与
/// `Reference`：语法上 `&x` 产出引用，而用户显式写的 `*T` 产指针，
/// 两者在运行期都是同一个"地址"。
fn is_struct_update_place(e: &Expression) -> bool {
    match e {
        Expression::Variable(..) | Expression::Path(..) => true,
        Expression::Field(base, ..) | Expression::Index(base, ..) => is_struct_update_place(base),
        Expression::Unary(ast::UnaryOp::Deref, base, _) => is_struct_update_place(base),
        _ => false,
    }
}

fn pointee_of(infer: &InferCtxt<'_>, ty: &Ty) -> Option<Ty> {
    match infer.resolve(ty) {
        Ty::Pointer(t, _) | Ty::Reference(t, _) => Some((*t).clone()),
        _ => None,
    }
}

/// 拒绝对浮点值取地址。
///
/// 浮点值住在专属栈槽、只经 XMM 读写，而指针走整数寄存器与整数 load/store。
/// 让两者交叉（`&f` / `*pf`）在当前后端只会安静地产生错值，所以这里直接
/// 报编译错误，而不是留一个"能编过但算错"的坑。
fn reject_float_address(infer: &InferCtxt<'_>, ty: &Ty, span: Span) -> Result<()> {
    if matches!(infer.resolve(ty), Ty::Float(_)) {
        return Err(CompileError::type_error(
            "taking the address of a floating-point value is not supported",
        )
        .with_span(span));
    }
    Ok(())
}

/// 判断两个类型是否可能重叠（即是否存在一个具体类型同时匹配两者）。
fn types_may_overlap(a: &Ty, b: &Ty) -> bool {
    match (a, b) {
        // 相同的命名类型可能重叠
        (Ty::Named(id1, _), Ty::Named(id2, _)) => id1 == id2,
        // 推断变量可能与任何类型重叠
        (Ty::Infer(_), _) | (_, Ty::Infer(_)) => true,
        // 指针/引用：检查内部类型
        (Ty::Pointer(t1, _), Ty::Pointer(t2, _)) | (Ty::Reference(t1, _), Ty::Reference(t2, _)) => {
            types_may_overlap(t1, t2)
        }
        // 数组：检查内部类型
        (Ty::Array(t1, _), Ty::Array(t2, _)) => types_may_overlap(t1, t2),
        // 元组：检查长度和元素
        (Ty::Tuple(ts1), Ty::Tuple(ts2)) => {
            ts1.len() == ts2.len()
                && ts1
                    .iter()
                    .zip(ts2.iter())
                    .all(|(x, y)| types_may_overlap(x, y))
        }
        // 其他组合：保守地认为不重叠
        _ => a == b,
    }
}

/// 解析器用空元组 `()` 表示切片语法里缺失的边界。
fn is_missing_bound(e: &Expression) -> bool {
    matches!(e, Expression::Tuple(items, _) if items.is_empty())
}

/// 数组/切片类型的长度（编译期已知时返回 `Some`；裸指针 / 引用指向的
/// 定长数组也解一层。动态切片长度运行期才有，这里返回 `None`）。
fn array_len_of(ty: &Ty) -> Option<i128> {
    match ty {
        Ty::Array(_, len) => hir::array_len_value(len),
        Ty::Pointer(inner, _) | Ty::Reference(inner, _) => match &**inner {
            Ty::Array(_, len) => hir::array_len_value(len),
            _ => None,
        },
        _ => None,
    }
}

/// 下标表达式的编译期整数值（只认字面量；`arr[k]` 的 `k` 是 const 局部
/// 时后续版本再展开，这里先堵住最常见的 `arr[5]` 越界）。
fn const_index_of(expr: &Expression) -> Option<i128> {
    match expr {
        Expression::Literal(ast::Literal::Int(n, _), _) => Some(*n),
        _ => None,
    }
}

/// 格式化类型名为字符串（用于错误消息）。
fn ty_name(ty: &Ty) -> String {
    match ty {
        Ty::Int(w) => format!("i{}", w),
        Ty::Uint(w) => format!("u{}", w),
        Ty::Float(w) => format!("f{}", w),
        Ty::Bool => "bool".into(),
        Ty::Char => "char".into(),
        Ty::Str => "str".into(),
        Ty::Unit => "()".into(),
        Ty::Never => "!".into(),
        Ty::Named(id, _) => format!("type#{}", id.0),
        Ty::Pointer(t, _) => format!("*{}", ty_name(t)),
        Ty::Reference(t, _) => format!("&{}", ty_name(t)),
        Ty::Array(t, _) => format!("[{}; _]", ty_name(t)),
        Ty::Slice(t, _) => format!("[]{}", ty_name(t)),
        Ty::Tuple(ts) => format!(
            "({})",
            ts.iter().map(ty_name).collect::<Vec<_>>().join(", ")
        ),
        Ty::Function { .. } => "fn(...)".into(),
        Ty::Optional(t) => format!("{}?", ty_name(t)),
        Ty::ErrorUnion(t, e) => format!("{}!{}", ty_name(t), ty_name(e)),
        Ty::Infer(_) => "_".into(),
        Ty::Opaque(id) => format!("opaque({})", id.0),
        Ty::Pin(inner) => format!("Pin<{}>", ty_name(inner)),
        Ty::Dyn(trait_id) => format!("dyn trait#{}", trait_id.0),
    }
}

/// 把 `for` 循环体里的 `continue` 改写成“先执行自增，再 continue”。
/// `depth > 0` 表示位于内层循环中，那里的 continue 属于内层循环，不能改。
fn rewrite_for_continue(block: &mut hir::HirBlock, inc: &HirExpr, depth: usize) {
    for stmt in &mut block.stmts {
        match stmt {
            HirStmt::Expr(e) => rewrite_for_continue_expr(e, inc, depth),
            HirStmt::Let { init, .. } | HirStmt::Var { init, .. } => {
                if let Some(e) = init {
                    rewrite_for_continue_expr(e, inc, depth);
                }
            }
            HirStmt::Item => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        rewrite_for_continue_expr(tail, inc, depth);
    }
}

fn rewrite_for_continue_expr(e: &mut HirExpr, inc: &HirExpr, depth: usize) {
    match &mut e.kind {
        ExprKind::Continue(label) if depth == 0 => {
            let cont = HirExpr {
                kind: ExprKind::Continue(label.clone()),
                ty: Ty::Never,
                span: e.span,
            };
            e.kind = ExprKind::Block(hir::HirBlock {
                stmts: vec![HirStmt::Expr(Box::new(inc.clone()))],
                tail: Some(Box::new(cont)),
            });
            e.ty = Ty::Never;
        }
        ExprKind::Binary(_, l, r) | ExprKind::Assign(l, r) | ExprKind::Index(l, r) => {
            rewrite_for_continue_expr(l, inc, depth);
            rewrite_for_continue_expr(r, inc, depth);
        }
        ExprKind::Unary(_, x)
        | ExprKind::Field(x, _)
        | ExprKind::Deref(x)
        | ExprKind::AddrOf(x)
        | ExprKind::Cast(x)
        | ExprKind::Spawn(x) => rewrite_for_continue_expr(x, inc, depth),
        ExprKind::Call { args, .. } => {
            for arg in args {
                rewrite_for_continue_expr(arg, inc, depth);
            }
        }
        ExprKind::CallIndirect { callee, args, .. } => {
            rewrite_for_continue_expr(callee, inc, depth);
            for arg in args {
                rewrite_for_continue_expr(arg, inc, depth);
            }
        }
        ExprKind::Block(b) => rewrite_for_continue(b, inc, depth),
        ExprKind::If(c, t, el) => {
            rewrite_for_continue_expr(c, inc, depth);
            rewrite_for_continue(t, inc, depth);
            if let Some(el) = el {
                rewrite_for_continue(el, inc, depth);
            }
        }
        ExprKind::While(_, c, b) => {
            rewrite_for_continue_expr(c, inc, depth + 1);
            rewrite_for_continue(b, inc, depth + 1);
        }
        ExprKind::Loop(_, b) => rewrite_for_continue(b, inc, depth + 1),
        ExprKind::Return(Some(x)) => rewrite_for_continue_expr(x, inc, depth),
        ExprKind::Array(elems) => {
            for elem in elems {
                rewrite_for_continue_expr(elem, inc, depth);
            }
        }
        ExprKind::Struct(fields) => {
            for (_, value) in fields {
                rewrite_for_continue_expr(value, inc, depth);
            }
        }
        _ => {}
    }
}

/// 取模式中第一个绑定变量的 `LocalId`。
fn first_binding(p: &hir::HirPattern) -> Option<LocalId> {
    match p {
        hir::HirPattern::Binding(local) => Some(*local),
        _ => None,
    }
}

/// 类型里是否还残留**未解析**的推断变量（深层扫描）。
///
/// 提升闭包时形参 / 返回类型可能是推断变量（`let f = |x| x;` 里 `x` 的类型
/// 要靠后面的调用 `f(7)` 反推）。收尾时 `resolve` 过一遍，若仍停在 `Infer`，
/// 说明没有任何上下文能定下它——Rust 同样要求这种闭包写标注，这里明确报错
/// 而不是让后端把 `_` 当 i64 编（见 `tests/cases/83_closure_fnptr.zr` 注释）。
fn ty_has_unresolved_infer(ty: &Ty, vars: &HashMap<InferId, Ty>) -> bool {
    let resolved = ty_resolve_deep(ty, vars);
    ty_contains_infer(&resolved)
}

/// 深解析：递归地把 `Ty::Infer` 沿绑定链解到底。
///
/// `InferCtxt::resolve` 只跟**一层**（绑定到另一个推断变量时直接停住返回原样），
/// 但 `unify` 的"两个推断变量"路径会让 `vars[a] = Infer(b)`，于是链可能
/// 是 `a -> b -> i32` 两跳以上。这里跟着链走到头；链可能成环（`a -> b` 后又
/// `b -> a`，`unify` 不查发生性），所以带 visited 集合防死循环。
fn ty_resolve_deep(ty: &Ty, vars: &HashMap<InferId, Ty>) -> Ty {
    fn go(ty: &Ty, vars: &HashMap<InferId, Ty>, seen: &mut HashSet<InferId>) -> Ty {
        match ty {
            Ty::Infer(id) => {
                if seen.contains(id) {
                    return ty.clone();
                }
                seen.insert(*id);
                match vars.get(id) {
                    Some(bound) => go(bound, vars, seen),
                    None => ty.clone(),
                }
            }
            Ty::Pointer(t, m) => Ty::Pointer(Box::new(go(t, vars, seen)), *m),
            Ty::Reference(t, m) => Ty::Reference(Box::new(go(t, vars, seen)), *m),
            Ty::Array(t, len) => Ty::Array(Box::new(go(t, vars, seen)), len.clone()),
            Ty::Slice(t, m) => Ty::Slice(Box::new(go(t, vars, seen)), *m),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| go(t, vars, seen)).collect()),
            Ty::Function {
                params,
                ret,
                is_async,
                abi,
            } => Ty::Function {
                params: params.iter().map(|t| go(t, vars, seen)).collect(),
                ret: Box::new(go(ret, vars, seen)),
                is_async: *is_async,
                abi: abi.clone(),
            },
            Ty::Optional(t) => Ty::Optional(Box::new(go(t, vars, seen))),
            Ty::ErrorUnion(t, e) => Ty::ErrorUnion(
                Box::new(go(t, vars, seen)),
                Box::new(go(e, vars, seen)),
            ),
            Ty::Named(id, args) => Ty::Named(*id, args.iter().map(|a| go(a, vars, seen)).collect()),
            Ty::Pin(t) => Ty::Pin(Box::new(go(t, vars, seen))),
            _ => ty.clone(),
        }
    }
    go(ty, vars, &mut HashSet::new())
}

/// 类型里是否出现任何 `Ty::Infer`（深层）。
fn ty_contains_infer(ty: &Ty) -> bool {
    match ty {
        Ty::Infer(_) => true,
        Ty::Pointer(t, _) | Ty::Reference(t, _) | Ty::Slice(t, _) | Ty::Optional(t)
        | Ty::Pin(t) => ty_contains_infer(t),
        Ty::Array(t, _) => ty_contains_infer(t),
        Ty::Tuple(ts) => ts.iter().any(ty_contains_infer),
        Ty::Function { params, ret, .. } => {
            params.iter().any(ty_contains_infer) || ty_contains_infer(ret)
        }
        Ty::ErrorUnion(t, e) => ty_contains_infer(t) || ty_contains_infer(e),
        Ty::Named(_, args) => args.iter().any(ty_contains_infer),
        _ => false,
    }
}

/// 把（可能嵌套的）或模式展开成分支条件列表：`3 | 4 | _` → [3, 4, _]。
fn push_or_alts<'a>(p: &'a AstPattern, out: &mut Vec<&'a AstPattern>) {
    if let AstPattern::Or(alts) = p {
        for alt in alts {
            push_or_alts(alt, out);
        }
    } else {
        out.push(p);
    }
}

/// 把 match 臂体包成分支块：块表达式原样使用，值表达式放进尾表达式位。
fn arm_body_block(body: &HirExpr) -> hir::HirBlock {
    match &body.kind {
        ExprKind::Block(b) => b.clone(),
        _ => hir::HirBlock {
            stmts: Vec::new(),
            tail: Some(Box::new(body.clone())),
        },
    }
}

/// 在表达式树里把对局部 `target` 的引用整体替换成 `with` 的拷贝。
///
/// 匹配臂的绑定（如 `v if v > 0 => v`）没有独立存储，guard / 臂体里对它的
/// 读都内联成 scrutinee 表达式。树是已检查过的，等价替换不需要重新推断。
fn substitute_local(e: &mut hir::HirExpr, target: LocalId, with: &hir::HirExpr) {
    if matches!(&e.kind, ExprKind::Local(id) if *id == target) {
        *e = with.clone();
        return;
    }
    match &mut e.kind {
        ExprKind::Binary(_, l, r) => {
            substitute_local(l, target, with);
            substitute_local(r, target, with);
        }
        // 赋值的**左右两侧**都要替换：`n = v` 里的绑定在右值上。
        // 早先这里只替换了左侧（被赋值的位置），于是
        // `if let Some(v) = x { n = v; }` 的 `v` 保持未初始化。
        ExprKind::Assign(l, r) => {
            substitute_local(l, target, with);
            substitute_local(r, target, with);
        }
        ExprKind::Unary(_, v)
        | ExprKind::Field(v, _)
        | ExprKind::Deref(v)
        | ExprKind::AddrOf(v)
        | ExprKind::Await(v)
        | ExprKind::Cast(v)
        | ExprKind::EnumField { value: v, .. }
        | ExprKind::EnumTag(v)
        | ExprKind::ArrayToSlice(v)
        | ExprKind::Spawn(v) => substitute_local(v, target, with),
        ExprKind::IndexRange(v, start, end, _) => {
            substitute_local(v, target, with);
            if let Some(s) = start {
                substitute_local(s, target, with);
            }
            if let Some(e) = end {
                substitute_local(e, target, with);
            }
        }
        ExprKind::Return(v) => {
            if let Some(v) = v {
                substitute_local(v, target, with);
            }
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                substitute_local(arg, target, with);
            }
        }
        ExprKind::EnumVariant { args, .. } => {
            for arg in args {
                substitute_local(arg, target, with);
            }
        }
        ExprKind::CallIndirect { callee, args, .. } => {
            substitute_local(callee, target, with);
            for arg in args {
                substitute_local(arg, target, with);
            }
        }
        ExprKind::If(cond, then, els) => {
            substitute_local(cond, target, with);
            walk_block(then, target, with);
            if let Some(els) = els {
                walk_block(els, target, with);
            }
        }
        ExprKind::While(_, cond, body) => {
            substitute_local(cond, target, with);
            walk_block(body, target, with);
        }
        ExprKind::Loop(_, body) => walk_block(body, target, with),
        ExprKind::Block(b) => walk_block(b, target, with),
        ExprKind::Index(_, v) => substitute_local(v, target, with),
        ExprKind::Struct(fields) => {
            for (_, f) in fields {
                substitute_local(f, target, with);
            }
        }
        ExprKind::Comptime(_)
        | ExprKind::Literal(_)
        | ExprKind::Local(_)
        | ExprKind::Def(_)
        | ExprKind::Match
        | ExprKind::Break(_, _)
        | ExprKind::Continue(_)
        | ExprKind::Unreachable
        | ExprKind::Defer(_)
        | ExprKind::Try { .. }
        | ExprKind::Throw(_)
        | ExprKind::Array(..)
        | ExprKind::Tuple(..)
        | ExprKind::Closure { .. }
        | ExprKind::Asm { .. }
        | ExprKind::Yield
        | ExprKind::Intrinsic => {}
    }
}

/// `substitute_local` 的块部分（stmts 里的 Expr 语句 + tail）。
fn walk_block(b: &mut hir::HirBlock, target: LocalId, with: &hir::HirExpr) {
    for stmt in &mut b.stmts {
        match stmt {
            HirStmt::Expr(expr) => substitute_local(expr, target, with),
            // `let y = v;` / `let mut y = v;`：初始化表达式里的绑定引用也要替换，
            // 否则 `let y = v;` 里的 `v` 保持未替换（与臂体尾表达式不一致）。
            HirStmt::Let { init, .. } | HirStmt::Var { init, .. } => {
                if let Some(init) = init {
                    substitute_local(init, target, with);
                }
            }
            HirStmt::Item => {}
        }
    }
    if let Some(tail) = &mut b.tail {
        substitute_local(tail, target, with);
    }
}

/// 把提升闭包体的局部量 id 重编号成独立编号空间。
///
/// 闭包体在 `check_lambda` 里检查时，局部量（形参 + 体内 `let`）从全局
/// `next_local` 计数器分配，和调用方函数共享编号。单独提升成一个函数后，
/// 若沿用原编号，MIR 里 `v{id}` 会和调用方的临时名撞车（`lower_function`
/// 用 `v{local_id}` 作寄存器名）。这里把每个局部量重新分配到提升函数自己的
/// 编号空间，`map` 记录旧 id → 新 id。
fn remap_lifted_body(
    b: &mut hir::HirBlock,
    map: &mut HashMap<LocalId, LocalId>,
    next_local: &mut u32,
) {
    // 为局部量 id 分配新编号：已有映射直接复用，否则从 `*next` 递增。
    fn fresh(map: &mut HashMap<LocalId, LocalId>, next: &mut u32, id: LocalId) -> LocalId {
        if let Some(&n) = map.get(&id) {
            return n;
        }
        *next += 1;
        let n = LocalId(*next - 1);
        map.insert(id, n);
        n
    }
    // `Local(id)` 的 id 换成新编号；`Call` / `CallIndirect` 的 `result`
    // 局部量也要重编号（它是被调结果的存储位置，同样按函数独立）。
    fn remap_expr(e: &mut HirExpr, map: &mut HashMap<LocalId, LocalId>, next: &mut u32) {
        // 每个表达式的类型字段里若有局部量无关的东西不用动；类型本身
        // 不含 LocalId，跳过。只需处理表达式结构与 `ExprKind::Local`。
        match &mut e.kind {
            ExprKind::Local(id) => {
                *id = fresh(map, next, *id);
            }
            ExprKind::Binary(_, l, r) => {
                remap_expr(l, map, next);
                remap_expr(r, map, next);
            }
            ExprKind::Assign(l, r) => {
                remap_expr(l, map, next);
                remap_expr(r, map, next);
            }
            ExprKind::Unary(_, v)
            | ExprKind::Field(v, _)
            | ExprKind::Deref(v)
            | ExprKind::AddrOf(v)
            | ExprKind::Await(v)
            | ExprKind::Cast(v)
            | ExprKind::EnumField { value: v, .. }
            | ExprKind::EnumTag(v)
            | ExprKind::ArrayToSlice(v)
            | ExprKind::Spawn(v) => remap_expr(v, map, next),
            ExprKind::IndexRange(v, start, end, _) => {
                remap_expr(v, map, next);
                if let Some(s) = start {
                    remap_expr(s, map, next);
                }
                if let Some(e) = end {
                    remap_expr(e, map, next);
                }
            }
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    remap_expr(v, map, next);
                }
            }
            ExprKind::Call { args, result, .. } => {
                let n = fresh(map, next, *result);
                *result = n;
                for arg in args {
                    remap_expr(arg, map, next);
                }
            }
            ExprKind::EnumVariant { args, .. } => {
                for arg in args {
                    remap_expr(arg, map, next);
                }
            }
            ExprKind::CallIndirect { callee, args, result, .. } => {
                let n = fresh(map, next, *result);
                *result = n;
                remap_expr(callee, map, next);
                for arg in args {
                    remap_expr(arg, map, next);
                }
            }
            ExprKind::If(cond, then, els) => {
                remap_expr(cond, map, next);
                remap_block(then, map, next);
                if let Some(els) = els {
                    remap_block(els, map, next);
                }
            }
            ExprKind::While(_, cond, body) => {
                remap_expr(cond, map, next);
                remap_block(body, map, next);
            }
            ExprKind::Loop(_, body) => remap_block(body, map, next),
            ExprKind::Block(blk) => remap_block(blk, map, next),
            ExprKind::Index(recv, idx) => {
                remap_expr(recv, map, next);
                remap_expr(idx, map, next);
            }
            ExprKind::Struct(fields) => {
                for (_, f) in fields {
                    remap_expr(f, map, next);
                }
            }
            ExprKind::Defer(d) => remap_block(d, map, next),
            ExprKind::Array(elems) => {
                for el in elems {
                    remap_expr(el, map, next);
                }
            }
            ExprKind::Tuple(elems) => {
                for el in elems {
                    remap_expr(el, map, next);
                }
            }
            ExprKind::Comptime(_)
            | ExprKind::Literal(_)
            | ExprKind::Def(_)
            | ExprKind::Match
            | ExprKind::Break(_, _)
            | ExprKind::Continue(_)
            | ExprKind::Unreachable
            | ExprKind::Try { .. }
            | ExprKind::Throw(_)
            | ExprKind::Closure { .. }
            | ExprKind::Asm { .. }
            | ExprKind::Yield
            | ExprKind::Intrinsic => {}
        }
    }
    fn remap_block(
        blk: &mut hir::HirBlock,
        map: &mut HashMap<LocalId, LocalId>,
        next: &mut u32,
    ) {
        for stmt in &mut blk.stmts {
            match stmt {
                HirStmt::Expr(expr) => remap_expr(expr, map, next),
                HirStmt::Let { local, init } | HirStmt::Var { local, init } => {
                    let n = fresh(map, next, *local);
                    *local = n;
                    if let Some(init) = init {
                        remap_expr(init, map, next);
                    }
                }
                HirStmt::Item => {}
            }
        }
        if let Some(tail) = &mut blk.tail {
            remap_expr(tail, map, next);
        }
    }
    remap_block(b, map, next_local);
}

/// match 臂的分支条件：`scrutinee == 字面量`，有 guard 再与它对半。
///
/// 字面量的 HIR 形态按 scrutinee 的类型回填（bool → Bool 字面量、char → Char），
/// 否则 `match b { 1 => .., }` 对 bool scrutinee 会比较错类型的字面量。
/// 脱糖后的一条 match 臂。
struct MatchArm {
    /// 模式（穷尽性检查用）。
    pattern: hir::HirPattern,
    /// 模式自身的匹配条件：`None` 表示恒匹配。
    cond: Option<HirExpr>,
    /// guard（已按臂绑定替换）。
    guard: Option<HirExpr>,
    /// 臂体（已按臂绑定替换）。
    body: HirExpr,
}

/// 模式是否不可反驳（一定能匹配上）。
///
/// `_` 与绑定臂永远匹配；元组 / 结构体模式则要求每个子模式都不可反驳
/// ——`(_, _)` 与 `_` 一样能兜底，`(1, _)` 不行。
fn pat_irrefutable(pat: &hir::HirPattern) -> bool {
    match pat {
        hir::HirPattern::Wildcard | hir::HirPattern::Binding(_) => true,
        hir::HirPattern::Tuple(elems) => elems.iter().all(pat_irrefutable),
        hir::HirPattern::Struct(fields) => {
            fields.iter().all(|(_, _, sub)| pat_irrefutable(sub))
        }
        _ => false,
    }
}

/// `a && b`。
fn and_expr(a: HirExpr, b: HirExpr, span: ast::Span) -> HirExpr {
    hir::HirExpr {
        kind: ExprKind::Binary(hir::BinaryOp::And, Box::new(a), Box::new(b)),
        ty: Ty::Bool,
        span,
    }
}

/// 布尔字面量。
fn bool_lit(v: bool, span: ast::Span) -> HirExpr {
    hir::HirExpr {
        kind: ExprKind::Literal(hir::HirLiteral::Bool(v)),
        ty: Ty::Bool,
        span,
    }
}

/// 块是否以发散语句结束（`return` / `throw`）。
///
/// 这类块不会把值交给调用方，所以类型检查不应要求它提供尾部表达式。
fn block_diverges(b: &AstBlock) -> bool {
    match (b.stmts.last(), &b.tail) {
        (Some(Statement::Expr(e, _)), None) => {
            matches!(e, Expression::Return(..) | Expression::Throw(..))
        }
        _ => false,
    }
}

/// 一个表达式是否**发散**（控制流不会从它正常返回）。
///
/// 只用于判定 `if` 的分支：`else` 分支在 AST 里是一个表达式而不是块
/// （`else { .. }` 会先脱成 `Expression::Block`，`else if` 是嵌套 `If`），
/// 所以要顺着表达式形状递归，而不是直接套 `block_diverges`。
///
/// 注意 `tail == None` 和"发散"是两码事：`{ write_fill(...); }` 的 tail 是
/// `None`（最后一句带了分号），但它正常返回一个 `()`。把这两者混同会让
/// `if` 分支的类型推断整个走偏，详见 `check_expr_raw` 里 `Expression::If` 的注释。
fn expr_diverges(e: &Expression) -> bool {
    match e {
        Expression::Return(..) | Expression::Throw(..) => true,
        Expression::Block(b, _) => block_diverges(b),
        Expression::If(_, then_b, else_b, _) => {
            let then_d = block_diverges(then_b);
            let else_d = else_b.as_ref().is_some_and(|e| expr_diverges(e));
            then_d && else_d
        }
        Expression::IfLet(_, _, then_b, else_b, _) => {
            let then_d = block_diverges(then_b);
            let else_d = else_b.as_ref().is_some_and(|e| expr_diverges(e));
            then_d && else_d
        }
        // `while` / `loop` / `break` / `continue` 都不构成发散：它们要么正常
        // 退出，要么被 `break` 带出，控制流最终会回到外层。
        _ => false,
    }
}

/// HIR 二元运算符 -> AST 同名运算符（两者变体一一对应）。
fn hir_binary_to_ast(op: hir::BinaryOp) -> ast::BinaryOp {
    use hir::BinaryOp::*;
    match op {
        Add => ast::BinaryOp::Add,
        Sub => ast::BinaryOp::Sub,
        Mul => ast::BinaryOp::Mul,
        Div => ast::BinaryOp::Div,
        Rem => ast::BinaryOp::Rem,
        BitAnd => ast::BinaryOp::BitAnd,
        BitOr => ast::BinaryOp::BitOr,
        BitXor => ast::BinaryOp::BitXor,
        Shl => ast::BinaryOp::Shl,
        Shr => ast::BinaryOp::Shr,
        Eq => ast::BinaryOp::Eq,
        Ne => ast::BinaryOp::Ne,
        Lt => ast::BinaryOp::Lt,
        Gt => ast::BinaryOp::Gt,
        Le => ast::BinaryOp::Le,
        Ge => ast::BinaryOp::Ge,
        And => ast::BinaryOp::And,
        Or => ast::BinaryOp::Or,
        Range => ast::BinaryOp::Range,
        RangeInclusive => ast::BinaryOp::RangeInclusive,
    }
}

/// HIR 一元运算符 -> AST 同名运算符。
fn hir_unary_to_ast(op: hir::UnaryOp) -> ast::UnaryOp {
    use hir::UnaryOp::*;
    match op {
        Neg => ast::UnaryOp::Neg,
        Not => ast::UnaryOp::Not,
        BitNot => ast::UnaryOp::BitNot,
        Deref => ast::UnaryOp::Deref,
        AddrOf => ast::UnaryOp::AddrOf,
    }
}

/// 编译期常量字面量对应的类型。
fn const_literal_ty(lit: &HirLiteral) -> Ty {
    match lit {
        // 宽度必须和类型检查器给整数字面量的默认宽度一致（i32），
        // 否则 `comptime ~0 != -1` 会拿 i64 去比 i32。
        HirLiteral::Int(_) => Ty::Int(32),
        HirLiteral::Float(_) => Ty::Float(64),
        HirLiteral::Bool(_) => Ty::Bool,
        HirLiteral::Char(_) => Ty::Char,
        HirLiteral::String(_) => Ty::Str,
        // 聚合值没有可反推的命名类型；正常路径下结构体字面量的类型
        // 由 `check_struct_init` 给出，不会走到这里。
        HirLiteral::Composite(_) => Ty::Unit,
    }
}

/// AST 属性 -> HIR 属性。
///
/// 参数压成字符串：下游（优化器 / MIR）只按名字和字面量参数做判断，
/// 完整表达式留在这里没有用处。无法静态渲染的表达式记成空串，
/// 等价于"没给这个参数"。
fn convert_attrs(attrs: &[ast::Attribute]) -> Vec<hir::HirAttr> {
    attrs
        .iter()
        .map(|a| hir::HirAttr {
            name: a.name.clone(),
            args: a
                .args
                .iter()
                .filter_map(|arg| match arg {
                    ast::AttributeArg::Pos(e) => render_attr_arg(e),
                    ast::AttributeArg::NameValue(_, _) => None,
                })
                .collect(),
            named: a
                .args
                .iter()
                .filter_map(|arg| match arg {
                    ast::AttributeArg::NameValue(n, v) => {
                        render_attr_arg(v).map(|v| (n.clone(), v))
                    }
                    ast::AttributeArg::Pos(_) => None,
                })
                .collect(),
        })
        .collect()
}

/// 把属性参数表达式渲染成字符串。
fn render_attr_arg(e: &ast::Expression) -> Option<String> {
    Some(match e {
        ast::Expression::Literal(ast::Literal::Int(v, _), _) => v.to_string(),
        ast::Expression::Literal(ast::Literal::Float(v, _), _) => v.to_string(),
        ast::Expression::Literal(ast::Literal::String(s), _) => s.clone(),
        ast::Expression::Literal(ast::Literal::Char(c), _) => c.to_string(),
        ast::Expression::Literal(ast::Literal::Bool(b), _) => b.to_string(),
        ast::Expression::Variable(name, _) => name.clone(),
        ast::Expression::Path(segments, _) => segments.join("::"),
        _ => return None,
    })
}

/// 从属性列表推出内存布局。
///
/// `#[repr(..)]` 显式给一个就行；`#[repr(C, align = 8)]` 这种组合里，
/// `C` / `packed` / `transparent` / `simd` 决定布局，`align = N` 是对齐
/// 约束——当前后端还没消费对齐，所以这里只校验它是个合法数字，
/// 非法写法直接报错，而不是像早先那样默默回退到默认布局。
fn hir_repr(attrs: &[hir::HirAttr]) -> Result<hir::Repr> {
    for attr in attrs {
        if attr.name != "repr" {
            continue;
        }
        for (key, value) in &attr.named {
            match key.as_str() {
                "align" => {
                    if value.parse::<u64>().is_err() {
                        return Err(CompileError::internal(format!(
                            "invalid #[repr(align = {value})]: expected a power-of-two integer"
                        )));
                    }
                }
                other => {
                    return Err(CompileError::internal(format!(
                        "unknown repr attribute argument `{other}`"
                    )));
                }
            }
        }
        // 逐个校验参数，取第一个决定布局的那个。`align = 8` 的简写形式
        // 只施加对齐约束，不决定布局，所以遇到它继续往后看。
        let mut layout = hir::Repr::Rust;
        for arg in &attr.args {
            match arg.as_str() {
                "C" => layout = hir::Repr::C,
                "packed" => layout = hir::Repr::Packed,
                "transparent" => layout = hir::Repr::Transparent,
                "simd" => layout = hir::Repr::Simd,
                other if other.starts_with("align") => {}
                other => {
                    return Err(CompileError::internal(format!(
                        "unknown representation `{other}`"
                    )))
                }
            }
        }
        return Ok(layout);
    }
    Ok(hir::Repr::Rust)
}

#[cfg(test)]
mod width_tests {
    use super::*;

    fn checker_for(max_bits: u16) -> TypeChecker {
        let mut c = TypeChecker::new();
        c.set_max_int_bits(max_bits);
        c
    }

    /// 64 位目标：i64 / u64 / i128 一律放行。
    #[test]
    fn wide_ints_are_allowed_on_64_bit_targets() {
        let c = checker_for(64);
        assert!(c.check_int_width(64).is_ok());
        assert!(c.check_int_width(32).is_ok());
    }

    /// 32 位目标：宽整数必须在类型检查阶段就报错。
    ///
    /// 之前不报错，后端把 i64 直接当 32 位写：`let a: i64 = 4294967296`
    /// 编出来是 `movw r4, #0`，`a + b` 得到 1 而不是 4294967297。
    /// 那种"能编过但结果是错的"必须挡在类型检查这一步。
    #[test]
    fn wide_ints_are_rejected_on_32_bit_targets() {
        let c = checker_for(32);
        assert!(c.check_int_width(32).is_ok());
        assert!(c.check_int_width(16).is_ok());
        let err = c.check_int_width(64).unwrap_err().to_string();
        assert!(err.contains("64"), "error should name the width: {err}");
        assert!(c.check_int_width(128).is_err());
    }

    /// 默认构造（宿主 x86-64）不该被限制。
    #[test]
    fn default_checker_allows_128_bit_ints() {
        assert!(TypeChecker::new().check_int_width(128).is_ok());
    }
}

#[cfg(test)]
mod mutability_tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn check(src: &str) -> Result<hir::HirModule> {
        let tokens = Lexer::new(src).tokenize();
        let prog = Parser::new(tokens).parse().expect("应能解析成功");
        TypeChecker::new().check_program(&prog)
    }

    /// 字符串字面量在 `.rodata`（只读段），`s[i] = v` 是段错误。
    #[test]
    fn rejects_writing_into_a_string() {
        let err = check(r#"fn main() -> i32 { let s = "ABC"; s[0] = 65; return 0; }"#)
            .expect_err("应拒绝写入字符串");
        assert!(
            err.message.contains("cannot assign into a string"),
            "actual: {err}"
        );
    }

    /// `*T` 默认是 `*const T`，写入要 `*mut T`。
    #[test]
    fn rejects_write_through_const_pointer() {
        let err = check(
            "fn main() -> i32 { let x: i32 = 1; let p: *const i32 = &x; *p = 42; return 0; }",
        )
        .expect_err("应拒绝通过只读指针写入");
        assert!(
            err.message
                .contains("cannot assign through an immutable pointer"),
            "actual: {err}"
        );
        let err = check(
            "fn main() -> i32 { let x: i32 = 1; let p: *const i32 = &x; p[0] = 42; return 0; }",
        )
        .expect_err("应拒绝通过只读指针索引写入");
        assert!(
            err.message
                .contains("cannot assign through an immutable pointer"),
            "actual: {err}"
        );
    }

    /// `let p: *i32 = &x;` 里的 `&x` 是只读指针，写入同样拒绝。
    #[test]
    fn rejects_write_through_address_of() {
        let err =
            check("fn main() -> i32 { let x: i32 = 1; let p: *i32 = &x; *p = 42; return 0; }")
                .expect_err("应拒绝通过 `&x` 写入");
        assert!(
            err.message
                .contains("cannot assign through an immutable pointer"),
            "actual: {err}"
        );
    }

    /// 可变指针（`&mut x` + `*mut i32`）读写都放行。
    #[test]
    fn allows_write_through_mut_pointer() {
        check("fn main() -> i32 { let x: i32 = 1; let p: *mut i32 = &mut x; *p = 42; if *p != 42 { return 1; } return 0; }")
            .expect("可变指针写入应通过类型检查");
        check("fn main() -> i32 { let x: i32 = 1; let p: *mut i32 = &mut x; p[0] = 42; if p[0] != 42 { return 1; } return 0; }")
            .expect("可变指针索引写入应通过类型检查");
    }
}

#[cfg(test)]
mod drop_move_tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn check(src: &str) -> Result<hir::HirModule> {
        let tokens = Lexer::new(src).tokenize();
        let prog = Parser::new(tokens).parse().expect("应能解析成功");
        TypeChecker::new().check_program(&prog)
    }

    /// 不能从直接实现 Drop 的类型里移出 Drop 字段（Rust E0509）。
    #[test]
    fn rejects_moving_drop_field_out_of_drop_type() {
        let err = check(
            r#"
            struct Leaf { v: i32 }
            impl Drop for Leaf { fn drop(&mut self) { } }
            struct Wrap { leaf: Leaf }
            impl Drop for Wrap { fn drop(&mut self) { } }
            fn main() -> i32 {
                let w = Wrap { leaf: Leaf { v: 1 } };
                let l = w.leaf;
                return 0;
            }
            "#,
        )
        .expect_err("应拒绝从 Drop 类型中部分移动 Drop 字段");
        assert!(err.message.contains("implements Drop"), "actual: {err}");
    }

    /// 外层没有直接实现 Drop 时，移出 Drop 字段是允许的。
    #[test]
    fn allows_moving_drop_field_out_of_plain_struct() {
        check(
            r#"
            struct Leaf { v: i32 }
            impl Drop for Leaf { fn drop(&mut self) { } }
            struct Wrap { leaf: Leaf }
            fn main() -> i32 {
                let w = Wrap { leaf: Leaf { v: 1 } };
                let l = w.leaf;
                return 0;
            }
            "#,
        )
        .expect("普通结构体字段应可移出");
    }
}
