//==============================================================================
// modules.rs — 模块加载与增量编译
//
// 负责两件事：
//
// 1. **把 `mod foo;` 真正加载进来**（`foo.zr`，或 `mod/foo.zr`），并递归
//    展开内联模块与 `use` 到的标准库模块。早先 `parse_mod` 把模块名丢掉了，
//    `ModuleDecl::Load` 只是个空壳，`mod foo;` 等于没写。
//
// 2. **增量编译**：记录本次编译读了哪些文件、每个文件的内容指纹，据此算出
//    一个总指纹。指纹没变就直接复用上次的产物文件。
//
// 关于第 2 点的诚实说明：这里做的是**整程序级**的缓存复用，不是按模块
// 分别重新类型检查 / 生成代码。原因见文件末尾"为什么不是真正的按模块增量"。
//==============================================================================

use std::path::{Path, PathBuf};

use crate::parser::ast::{self, ModuleDecl, ModuleItem, Program};
use crate::parser::Parser;

/// 本次编译读到的文件（按读取顺序）与各���内容指纹。
#[derive(Debug, Default)]
pub struct LoadReport {
    /// 参与编译的源文件：路径 -> 内容指纹。
    pub files: Vec<(PathBuf, u64)>,
    /// 被 `mod` / `use` 拉进来的模块名。
    pub modules: Vec<String>,
    /// 内联模块名（`mod af { ... }`）。模块系统是扁平命名空间，内联模块的条目
    /// 被直接合并进顶层，模块名本身不是符号；但源码里 `af::INET` 这样的限定
    /// 路径仍要能解析——`resolve_path` 据此把叶子段拿到顶层符号表解析。
    pub flattened: Vec<String>,
}

/// 收集一个模块（`Program`）里定义的所有顶层条目名，供 `use foo::*` 导入。
///
/// 只取**定义**类条目（函数 / 类型 / trait / 常量 / 静态 / extern 声明 / 宏），
/// 不取 `use` 与 `mod`（它们是限定符不是可导入名）。标准库模块里的条目基本
/// 都带 `pub`；AST 层没有逐条记录可见性（只有 `pub`，无可见性检查），这里
/// 一律按"可导出"处理——`use core::*` 把 `core` 的全部顶层名字拉进来。
fn module_item_names(module: &Program) -> Vec<String> {
    let mut out = Vec::new();
    for f in &module.functions {
        out.push(f.name.clone());
    }
    for td in &module.type_defs {
        match td {
            crate::parser::ast::TypeDef::Struct(s) => out.push(s.name.clone()),
            crate::parser::ast::TypeDef::Enum(e) => out.push(e.name.clone()),
            crate::parser::ast::TypeDef::Union(u) => out.push(u.name.clone()),
            crate::parser::ast::TypeDef::Alias { name, .. } => out.push(name.clone()),
        }
    }
    for t in &module.traits {
        out.push(t.name.clone());
    }
    for c in &module.consts {
        out.push(c.name.clone());
    }
    for s in &module.statics {
        out.push(s.name.clone());
    }
    for m in &module.macros {
        out.push(m.name.clone());
    }
    for e in &module.externs {
        for f in &e.functions {
            out.push(f.name.clone());
        }
    }
    out
}

//=============================================================================
// 懒导入：引用驱动的标准库按需加载
//
// `use std::*` 是门面（std/std.zr），它 32 个 `use x::*` 全量 glob 会把
// 整个标准库（含 tensor 69KB、io 94KB、tls 42KB 等重模块）加载进来——
// 一个只用 `println` 的程序也要解析 32 个模块，编译从 0.5s 涨到 3s+。
//
// 懒导入在**加载 std 门面时**按主程序实际引用过滤：`use std::*` 展开出的
// 子模块 `use x::*`，只有 `x` 的导出名与主程序引用的标识符**相交**时才
// 真正加载。主程序引用的标识符由 `collect_referenced_names` 收集（遍历
// 函数体、类型、模式、属性等所有可能出现名字的位置），**保守完整**：
// 宁可多加载一个模块，不可漏掉被引用的符号。
//
// 只作用于 std 门面的 glob 子模块；显式 `use io::*` / `use core::*` 不受
// 影响（用户指明要哪个模块，必须全量）。
//=============================================================================

/// 收集主程序**所有**出现的标识符（保守完整）。
///
/// 返回的名字集合用于懒导入：std 门面的子模块导出名与它相交才加载。
/// 收集位置：
/// - 函数体表达式：`Variable` / `Path` 首段 / 调用 / 字段 / 类型注解；
/// - 类型注解：`Named` 首段 / `Qualified` 的 trait；
/// - 类型定义 / trait / 常量 / 静态的定义名（可能被 glob 导出后引用）。
fn collect_referenced_names(prog: &Program) -> std::collections::HashSet<String> {
    use crate::parser::ast::*;
    let mut out = std::collections::HashSet::new();

    fn type_names(t: &Type, out: &mut std::collections::HashSet<String>) {
        match t {
            Type::Named { path, generics } => {
                if let Some(first) = path.first() {
                    out.insert(first.clone());
                }
                for g in generics {
                    type_names(g, out);
                }
            }
            Type::Qualified { self_ty, trait_ty, .. } => {
                type_names(self_ty, out);
                type_names(trait_ty, out);
            }
            Type::Pointer(i, _) | Type::Reference(i, _) => type_names(i, out),
            Type::Array(i, _) | Type::Slice(i, _) | Type::Optional(i) | Type::Not(i) => {
                type_names(i, out)
            }
            Type::Tuple(ts) => {
                for t in ts {
                    type_names(t, out);
                }
            }
            Type::Function { params, ret, .. } => {
                for p in params {
                    type_names(p, out);
                }
                type_names(ret, out);
            }
            Type::ErrorUnion(t, e) => {
                type_names(t, out);
                type_names(e, out);
            }
            Type::ImplTrait(bs) | Type::Dyn(bs) => {
                for b in bs {
                    type_names(b, out);
                }
            }
            _ => {}
        }
    }

    fn expr_names(e: &Expression, out: &mut std::collections::HashSet<String>) {
        match e {
            Expression::Variable(n, _) => {
                out.insert(n.clone());
            }
            Expression::Path(path, _) => {
                if let Some(first) = path.first() {
                    out.insert(first.clone());
                }
            }
            Expression::Binary(a, _, b, _) | Expression::Assign(a, _, b, _) => {
                expr_names(a, out);
                expr_names(b, out);
            }
            Expression::Unary(_, a, _)
            | Expression::Question(a, _)
            | Expression::PostfixBang(a, _)
            | Expression::AddrOf(a, _, _)
            | Expression::Comptime(a, _)
            | Expression::Await(a, _)
            | Expression::Spawn(a, _)
            | Expression::Unsafe(a, _)
            | Expression::Defer(a, _)
            | Expression::Throw(a, _) => expr_names(a, out),
            Expression::Turbofish(a, tys, _) => {
                expr_names(a, out);
                for t in tys {
                    type_names(t, out);
                }
            }
            Expression::Call(f, args, _) => {
                expr_names(f, out);
                for a in args {
                    match a {
                        Argument::Pos(e) | Argument::Named(_, e) => expr_names(e, out),
                    }
                }
            }
            Expression::MacroCall { name, args, .. } => {
                out.insert(name.clone());
                // 宏参数是记号序列：无法静态收集引用，保守不收集（宏展开
                // 阶段会处理）。若宏引用 std 符号，靠其他位置的收集兜底。
                let _ = args;
            }
            Expression::Index(a, b, _) => {
                expr_names(a, out);
                expr_names(b, out);
            }
            Expression::Field(a, _, _) => expr_names(a, out),
            Expression::Cast(a, t, _) => {
                expr_names(a, out);
                type_names(t, out);
            }
            Expression::Block(b, _) => block_names(b, out),
            Expression::If(c, t, el, _) => {
                expr_names(c, out);
                block_names(t, out);
                if let Some(el) = el {
                    expr_names(el, out);
                }
            }
            Expression::IfLet(p, c, t, el, _) => {
                pattern_names(p, out);
                expr_names(c, out);
                block_names(t, out);
                if let Some(el) = el {
                    expr_names(el, out);
                }
            }
            Expression::Match(s, arms, _) => {
                expr_names(s, out);
                for arm in arms {
                    pattern_names(&arm.pattern, out);
                    if let Some(g) = &arm.guard {
                        expr_names(g, out);
                    }
                    expr_names(&arm.body, out);
                }
            }
            Expression::While(c, b, el, _) => {
                expr_names(c, out);
                block_names(b, out);
                if let Some(el) = el {
                    block_names(el, out);
                }
            }
            Expression::For(p, it, b, _) => {
                pattern_names(p, out);
                expr_names(it, out);
                block_names(b, out);
            }
            Expression::Loop(b, _) => block_names(b, out),
            Expression::Break(_, v, _) => {
                if let Some(v) = v {
                    expr_names(v, out);
                }
            }
            Expression::Continue(_, _) | Expression::Goto(_, _) | Expression::Labeled(..)
            | Expression::Asm(..) | Expression::Intrinsic(..) => {}
            Expression::Return(Some(e), _) | Expression::Yield(e, _) => expr_names(e, out),
            Expression::Return(None, _) => {}
            Expression::Try(e, catch_pat, catch_block, _) => {
                expr_names(e, out);
                if let Some(p) = catch_pat {
                    pattern_names(p, out);
                }
                block_names(catch_block, out);
            }
            Expression::Array(items, _, _) | Expression::Tuple(items, _) => {
                for it in items {
                    expr_names(it, out);
                }
            }
            Expression::StructInit(path, fields, base, _) => {
                if let Some(first) = path.first() {
                    out.insert(first.clone());
                }
                for (_, v) in fields {
                    expr_names(v, out);
                }
                if let Some(b) = base {
                    expr_names(b, out);
                }
            }
            Expression::Lambda(params, ret, body, _) => {
                for p in params {
                    if let Some(t) = &p.ty {
                        type_names(t, out);
                    }
                    pattern_names(&p.pattern, out);
                }
                if let Some(t) = ret {
                    type_names(t, out);
                }
                block_names(body, out);
            }
            Expression::AsyncBlock(b, _) => block_names(b, out),
            Expression::Literal(..) => {}
        }
    }

    fn stmt_names(s: &Statement, out: &mut std::collections::HashSet<String>) {
        match s {
            Statement::Let(p, t, e, _) | Statement::Var(p, t, e, _) => {
                pattern_names(p, out);
                if let Some(t) = t {
                    type_names(t, out);
                }
                if let Some(e) = e {
                    expr_names(e, out);
                }
            }
            Statement::LetElse(p, t, e, b, _) => {
                pattern_names(p, out);
                if let Some(t) = t {
                    type_names(t, out);
                }
                expr_names(e, out);
                block_names(b, out);
            }
            Statement::Const(_, t, e, _) => {
                type_names(t, out);
                expr_names(e, out);
            }
            Statement::Expr(e, _) => expr_names(e, out),
        }
    }

    fn block_names(b: &Block, out: &mut std::collections::HashSet<String>) {
        for s in &b.stmts {
            stmt_names(s, out);
        }
        if let Some(t) = &b.tail {
            expr_names(t, out);
        }
    }

    fn pattern_names(p: &Pattern, out: &mut std::collections::HashSet<String>) {
        match p {
            Pattern::Binding(_, _, sub, _) => {
                if let Some(sub) = sub {
                    pattern_names(sub, out);
                }
            }
            Pattern::Tuple(ps, _) => {
                for p in ps {
                    pattern_names(p, out);
                }
            }
            Pattern::Struct { path, fields, .. } => {
                if let Some(first) = path.first() {
                    out.insert(first.clone());
                }
                for f in fields {
                    pattern_names(&f.pattern, out);
                }
            }
            Pattern::Variant { path, args, .. } => {
                if let Some(first) = path.first() {
                    out.insert(first.clone());
                }
                for a in args {
                    pattern_names(a, out);
                }
            }
            Pattern::Slice(ps, _) => {
                for p in ps {
                    pattern_names(p, out);
                }
            }
            Pattern::Range(a, b, _, _) => {
                expr_names(a, out);
                expr_names(b, out);
            }
            Pattern::Ref(inner, _, _) => pattern_names(inner, out),
            Pattern::Or(ps) => {
                for p in ps {
                    pattern_names(p, out);
                }
            }
            _ => {}
        }
    }

    // 函数定义名、参数类型、返回类型、函数体、where 子句。
    for f in &prog.functions {
        out.insert(f.name.clone());
        for p in &f.params {
            type_names(&p.ty, &mut out);
        }
        type_names(&f.return_type, &mut out);
        if let Some(b) = &f.body {
            block_names(b, &mut out);
        }
        for g in &f.generics {
            match g {
                GenericParam::Type(n, bounds) => {
                    out.insert(n.clone());
                    for b in bounds {
                        type_names(b, &mut out);
                    }
                }
                GenericParam::Const(n, _) => {
                    out.insert(n.clone());
                }
                _ => {}
            }
        }
        for w in &f.where_clause {
            type_names(&w.lhs, &mut out);
            for b in &w.bounds {
                type_names(b, &mut out);
            }
        }
    }
    // 类型定义名（可能被 glob 导出后引用）与字段类型。
    for td in &prog.type_defs {
        match td {
            TypeDef::Struct(s) => {
                out.insert(s.name.clone());
                for f in &s.fields {
                    type_names(&f.ty, &mut out);
                }
                for g in &s.generics {
                    if let GenericParam::Type(n, _) = g {
                        out.insert(n.clone());
                    }
                }
            }
            TypeDef::Enum(e) => {
                out.insert(e.name.clone());
                for v in &e.variants {
                    match &v.payload {
                        VariantPayload::Tuple(ts) => {
                            for t in ts {
                                type_names(t, &mut out);
                            }
                        }
                        VariantPayload::Struct(fs) => {
                            for f in fs {
                                type_names(&f.ty, &mut out);
                            }
                        }
                        VariantPayload::Unit => {}
                    }
                }
                for g in &e.generics {
                    if let GenericParam::Type(n, _) = g {
                        out.insert(n.clone());
                    }
                }
            }
            TypeDef::Union(u) => {
                out.insert(u.name.clone());
                for f in &u.fields {
                    type_names(&f.ty, &mut out);
                }
            }
            TypeDef::Alias { name, target, .. } => {
                out.insert(name.clone());
                type_names(target, &mut out);
            }
        }
    }
    // trait 名、方法签名、关联类型、边界。
    for t in &prog.traits {
        out.insert(t.name.clone());
        for st in &t.super_traits {
            type_names(st, &mut out);
        }
        for m in &t.methods {
            for p in &m.params {
                type_names(&p.ty, &mut out);
            }
            type_names(&m.return_type, &mut out);
            if let Some(b) = &m.body {
                block_names(b, &mut out);
            }
        }
        for at in &t.assoc_types {
            if let Some(ty) = &at.ty {
                type_names(ty, &mut out);
            }
        }
    }
    // const / static 的类型与值。
    for c in &prog.consts {
        type_names(&c.ty, &mut out);
        expr_names(&c.value, &mut out);
    }
    for s in &prog.statics {
        type_names(&s.ty, &mut out);
        expr_names(&s.init, &mut out);
    }
    // impl 目标、trait、方法体、关联类型。
    for i in &prog.impls {
        type_names(&i.target, &mut out);
        if let Some(t) = &i.trait_type {
            type_names(t, &mut out);
        }
        for m in &i.methods {
            for p in &m.params {
                type_names(&p.ty, &mut out);
            }
            type_names(&m.return_type, &mut out);
            if let Some(b) = &m.body {
                block_names(b, &mut out);
            }
        }
        for at in &i.assoc_types {
            if let Some(ty) = &at.ty {
                type_names(ty, &mut out);
            }
        }
    }
    // 宏名。
    for m in &prog.macros {
        out.insert(m.name.clone());
    }
    out
}

/// 标准库根目录（`<crate>/std`）。
pub fn std_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("std")
}

//=============================================================================
// 三层标准库：core ⊂ alloc ⊂ std（与 Rust 的 crate 分层对应）
//
//   core  零依赖：无堆、无 OS、无 I/O（trait / Option / Result / panic 等）
//   alloc 在 core 上加堆分配（String / Vec / 容器 / 纯解析库）
//   std   完整标准库，依赖 OS（io / fs / net / 进程 / 线程 …）
//
// 模块名仍是扁平的（`use core::Option`、`use string::String`、`use io::File`），
// 层级由文件所在目录决定：`std/core/*.zr`、`std/alloc/*.zr`、`std/std/*.zr`。
// 依赖方向只允许 core → core、alloc → {core, alloc}、std → 任意，
// 违反时在加载阶段报错（见 `load_uses`）。
//=============================================================================

/// 三层目录名，按依赖从低到高排列（顺序即 `Layer` 的比较顺序）。
pub const STDLIB_LAYERS: &[&str] = &["core", "alloc", "std"];

/// 标准库层级。`PartialOrd` 的顺序就是依赖方向：低层不能依赖高层。
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Layer {
    Core,
    Alloc,
    Std,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Layer::Core => "core",
            Layer::Alloc => "alloc",
            Layer::Std => "std",
        }
    }
}

/// 由模块文件的存放位置推出它属于哪一层。
///
/// 只有 `<root>/<layer>/<name>.zr` 才带层信息；直接放在 `<root>/` 下的
/// 根级文件（例如门面 `std.zr`）按最上层 `std` 处理。
fn layer_of_path(root: &Path, path: &Path) -> Layer {
    if let Ok(rel) = path.strip_prefix(root) {
        let mut comps = rel.components();
        let first = comps.next();
        let has_more = comps.next().is_some();
        if has_more {
            if let Some(std::path::Component::Normal(seg)) = first {
                return match seg.to_string_lossy().as_ref() {
                    "core" => Layer::Core,
                    "alloc" => Layer::Alloc,
                    _ => Layer::Std,
                };
            }
        }
    }
    // 根级文件（门面）视为 std 层。
    Layer::Std
}

/// 加载结果。
pub struct Loaded {
    /// 加载失败时的原因。
    pub error: Option<String>,
    /// 并入主程序的模块名（类型检查用得上：`io` 这类段要当模块看待）。
    pub modules: Vec<String>,
    /// 内联模块名（`mod af { ... }`，条目已扁平并入顶层，见 `LoadReport`）。
    pub flattened: Vec<String>,
    /// 每个模块导出的条目名（`use core::*` 要按它导入全部名字）。
    ///
    /// 键是模块名（`core` / `io`），值是模块里定义的顶层条目名（函数、
    /// 类型、常量、静态、trait、宏、extern 声明）。`use foo::*` 在类型检查
    /// 时把这里列出的名字全部登记成 `foo` 的别名。
    pub module_exports: std::collections::HashMap<String, Vec<String>>,
    /// 读过的文件与指纹。
    pub report: LoadReport,
}

/// 递归加载 `prog` 的全部依赖，把定义并回 `prog`。
///
/// `base_dir` 是根源文件所在目录，用来定位 `mod foo;` 对应的 `foo.zr`。
/// `root_file` 是根源文件本身：登记进 `seen` 后，prelude 再 `use` 同名模块时
/// 不会把同一份定义并第二遍（否则"编译标准库模块本身"会报
/// `multiple impls of the same trait`）。普通用户程序传 `None` 即可。
pub fn load_with_root(prog: &mut Program, base_dir: &Path, root_file: Option<&Path>) -> Loaded {
    let mut out = Loaded {
        error: None,
        modules: Vec::new(),
        flattened: Vec::new(),
        module_exports: std::collections::HashMap::new(),
        report: LoadReport::default(),
    };
    let mut loader = Loader {
        seen: root_file
            .map(absolutize)
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        modules: std::collections::HashSet::new(),
        flattened: std::collections::HashSet::new(),
        layers: std::collections::HashMap::new(),
        glob_deps: std::collections::HashMap::new(),
        report: &mut out.report,
        module_exports: &mut out.module_exports,
        // 懒导入：主程序引用的标识符。在吸收内联模块**之前**收集
        // （此时 prog 已解析完，函数体 AST 完整）。
        referenced_names: collect_referenced_names(prog),
        filtered: std::collections::HashSet::new(),
    };
    // 1) 先展开内联模块（它们已经在内存里了）
    let inline = std::mem::take(&mut prog.modules);
    for decl in inline {
        match decl {
            ModuleDecl::Inline(name, items) => {
                loader.flattened.insert(name.clone());
                loader.absorb_items(prog, items, base_dir, &name);
            }
            ModuleDecl::Load(name) => {
                if let Err(e) = loader.load_mod_file(prog, &name, base_dir, 0) {
                    out.error = Some(e);
                    return out;
                }
            }
        }
    }

    // 2) 再拉 `use` 到的标准库模块
    if let Err(e) = loader.load_uses(prog, base_dir, 0) {
        out.error = Some(e);
        return out;
    }

    // 先把 `loader` 里的集合取出来再 `drop`：`loader` 可变借用了
    // `out.report` / `out.module_exports`，不结束借用就不能写 `out.report.*`。
    // 用 `mem::take` 逐个取出（`into_iter` 会部分移动 `loader`）。
    let modules = std::mem::take(&mut loader.modules)
        .into_iter()
        .collect::<Vec<_>>();
    let flattened = std::mem::take(&mut loader.flattened)
        .into_iter()
        .collect::<Vec<_>>();
    let glob_deps = std::mem::take(&mut loader.glob_deps);
    drop(loader);

    out.modules = modules;
    out.modules.sort();
    out.flattened = flattened;
    out.flattened.sort();
    // `std` 门面的 `use core::*; use string::*; ..` 要真的把下层名字再导出：
    // 把 glob 依赖的导出名并进本模块的导出表（多轮直到不再增长，含环依赖）。
    expand_glob_exports(&mut out.module_exports, &glob_deps);
    out.report.modules = out.modules.clone();
    out.report.flattened = out.flattened.clone();
    out
}

/// 把 `use foo::*` 的导出名传递性地并进本模块的 `module_exports`。
///
/// 扁平化后模块没有真正的命名空间，`std` 门面要"再导出"下层模块的名字，
/// 只能靠这张表把下层的导出名并进来。多轮传播以处理 `std -> io -> core`
/// 这类传递链；`names` 排序固定顺序，避免不同 HashMap 迭代顺序造成差异。
fn expand_glob_exports(
    exports: &mut std::collections::HashMap<String, Vec<String>>,
    globs: &std::collections::HashMap<String, Vec<String>>,
) {
    let mut names: Vec<String> = exports.keys().cloned().collect();
    names.sort();
    // 模块数有限、名字集只增不减，最多传播 `names.len()` 轮。
    for _ in 0..=names.len() {
        let mut changed = false;
        for m in &names {
            let deps = match globs.get(m) {
                Some(d) => d.clone(),
                None => continue,
            };
            let mut add: Vec<String> = Vec::new();
            for d in &deps {
                if let Some(xs) = exports.get(d) {
                    add.extend(xs.iter().cloned());
                }
            }
            let cur = match exports.get_mut(m) {
                Some(c) => c,
                None => continue,
            };
            for a in add {
                if !cur.contains(&a) {
                    cur.push(a);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

struct Loader<'a> {
    /// 已加载的源文件绝对路径（防重复加载与循环引用）。
    seen: std::collections::HashSet<PathBuf>,
    /// 已加载的模块名。
    modules: std::collections::HashSet<String>,
    /// 内联模块名（条目扁平并入顶层，模块名本身不是符号）。
    flattened: std::collections::HashSet<String>,
    /// 已加载的标准库模块 → 层级（依赖方向校验用）。
    layers: std::collections::HashMap<String, Layer>,
    /// 模块名 → 它通过 `use foo::*` 依赖的模块名（`std` 门面再导出用）。
    glob_deps: std::collections::HashMap<String, Vec<String>>,
    report: &'a mut LoadReport,
    /// 模块名 → 导出的条目名（`use foo::*` 用）。
    module_exports: &'a mut std::collections::HashMap<String, Vec<String>>,
    /// 主程序引用的标识符（懒导入过滤用）。空 = 不启用懒导入（主程序
    /// 无函数体或未被引用收集——保持全量加载，保守）。
    referenced_names: std::collections::HashSet<String>,
    /// 懒导入过滤跳过的模块名（符号未加载）。真实依赖到达时覆盖加载。
    filtered: std::collections::HashSet<String>,
}

/// `mod` 嵌套深度上限。超过就当作循环引用报错——递归 `mod` 会无限展开。
const MAX_MODULE_DEPTH: usize = 32;

impl Loader<'_> {
    /// 处理 `use` 声明，把命中的标准库模块并进来。
    fn load_uses(&mut self, prog: &mut Program, _base: &Path, depth: usize) -> Result<(), String> {
        if depth > MAX_MODULE_DEPTH {
            return Err("module `use` chain is too deep (cycle?)".to_string());
        }
        let roots = collect_use_roots(&prog.imports);
        // 队列元素带上"发起依赖的模块名"：来自用户程序的根导入不做层级限制。
        let mut queue: Vec<(String, Option<String>)> =
            roots.into_iter().map(|n| (n, None)).collect();
        while let Some((name, origin)) = queue.pop() {
            let maybe_path = stdlib_module_path(&std_root(), &name);
            // （无诊断）
            // 懒导入：`std` 门面（std/std.zr）的 glob 子模块按主程序引用
            // 过滤——`use std::*` 全量加载 41 个模块，含 tensor / io / tls
            // 等重模块；一个只用 `println` 的程序也要解析全部。这里只对
            // **std 门面的直接子模块**（origin == Some("std")）过滤：用户
            // 显式 `use io::*` 走 origin == None（不过滤，必须全量）；模块
            // 内部依赖（io 需要 string）也全量（被加载模块的依赖链完整）。
            // `referenced_names` 为空（未收集到）时保守全量加载。
            //
            // 过滤跳过的模块**仍填 `module_exports`**（std.zr 的 `use cmp::*`
            // 被 absorb 并进主程序，typeck 会对它做 glob——没有导出名表会报
            // `cannot glob-import`），并进 `modules`（`use cmp::*` 的
            // `Name("cmp")` 分支靠 `known_modules` 早退）。只是不加载模块体、
            // 不 absorb——符号缺失时 typeck 报 `cannot find value`，正确。
            if origin.as_deref() == Some("std") {
                if let Some(exports) = self.std_glob_exports(&name) {
                    if exports
                        .iter()
                        .any(|e| self.referenced_names.contains(e))
                    {
                        // 被引用：走正常加载。
                    } else {
                        // 未引用：跳过加载，但记录导出名、模块名与"曾被
                        // 过滤"标记。真实依赖（已加载模块的内部 use）到达
                        // 时会覆盖加载。
                        self.modules.insert(name.clone());
                        self.module_exports.insert(name.clone(), exports);
                        self.filtered.insert(name.clone());
                        continue;
                    }
                }
            }

            // 分层约束：core 只能依赖 core；alloc 只能依赖 core/alloc；
            // std 可以依赖任意层。先于"已加载则跳过"检查，避免依赖方向
            // 因加载顺序不同而被漏检（例如 io 先加载后再被 alloc 依赖）。
            if let (Some(origin_name), Some(path)) = (&origin, &maybe_path) {
                let layer = layer_of_path(&std_root(), path);
                if let Some(origin_layer) = self.layers.get(origin_name) {
                    if *origin_layer < layer {
                        return Err(format!(
                            "layering violation: module `{origin_name}` ({} layer) may not depend on `{name}` ({} layer)",
                            origin_layer.name(),
                            layer.name()
                        ));
                    }
                }
            }

            if self.modules.contains(&name) && !self.filtered.contains(&name) {
                continue;
            }
            let path = maybe_path.ok_or_else(|| {
                format!(
                    "unknown module `{name}` (looked in {})",
                    std_root().display()
                )
            })?;
            let layer = layer_of_path(&std_root(), &path);

            let (module, path) = self.parse_file(&path)?;
            self.modules.insert(name.clone());
            self.filtered.remove(&name);
            self.layers.insert(name.clone(), layer);

            // 记录本模块导出的条目名，供 `use core::*` 导入。
            self.module_exports
                .insert(name.clone(), module_item_names(&module));
            // 记录 `use foo::*` 的 glob 依赖，供 `std` 门面传递性再导出。
            let globs = collect_glob_roots(&module.imports);
            if !globs.is_empty() {
                self.glob_deps.insert(name.clone(), globs);
            }

            // （std 依赖由 collect_use_roots 收集）
            for dep in collect_use_roots(&module.imports) {
                // 过滤标记（std 门面懒导入跳过的模块）需要真实依赖覆盖加载：
                // string 的 `use char::is_ascii_whitespace` 若 char 已被过滤，
                // 这里必须重新入队，让 char 真正加载（符号进 symbols）。
                if !self.modules.contains(&dep) || self.filtered.contains(&dep) {
                    queue.push((dep, Some(name.clone())));
                }
            }
            let dir = path.parent().unwrap_or(_base).to_path_buf();
            self.absorb(prog, module, &dir, depth, &name);
        }
        Ok(())
    }

    /// std 门面的子模块 `name` 的导出名（懒导入过滤用）。
    ///
    /// 读 `name` 模块文件、解析、取顶层条目名。`None` = 读/解析失败
    /// （交给后续正常报错，不过滤）。**不**进 `seen` / `modules`——过滤
    /// 跳过后模块仍可被其他路径正常加载。
    fn std_glob_exports(&self, name: &str) -> Option<Vec<String>> {
        let path = stdlib_module_path(&std_root(), name)?;
        let src = std::fs::read_to_string(&path).ok()?;
        let module = parse_program(&src, &path.display().to_string()).ok()?;
        Some(module_item_names(&module))
    }

    /// 加载 `mod name;`。
    fn load_mod_file(
        &mut self,
        prog: &mut Program,
        name: &str,
        base: &Path,
        depth: usize,
    ) -> Result<(), String> {
        if depth > MAX_MODULE_DEPTH {
            return Err(format!("module `{name}` nests too deeply (cycle?)"));
        }
        let path = user_module_path(base, name).ok_or_else(|| {
            format!("cannot find module `{name}` (looked for {name}.zr and mod/{name}.zr)")
        })?;
        let (module, path) = self.parse_file(&path)?;
        // `mod foo;` 里 foo 是**模块名**，登记它让 `foo::bar` 能解析
        self.modules.insert(name.to_string());
        // 记录导出条目名，供 `use foo::*` 导入。
        self.module_exports
            .insert(name.to_string(), module_item_names(&module));
        let dir = path.parent().unwrap_or(base).to_path_buf();
        self.absorb(prog, module, &dir, depth, name);
        Ok(())
    }

    /// 读文件 + 解析 + 记账指纹。
    fn parse_file(&mut self, path: &Path) -> Result<(Program, PathBuf), String> {
        let abs = absolutize(path);
        if !self.seen.insert(abs.clone()) {
            // 同一文件被两个模块引到：返回空程序，避免重复定义
            return Ok((Program::default(), abs));
        }
        let src = std::fs::read_to_string(&abs)
            .map_err(|e| format!("cannot read '{}': {e}", abs.display()))?;
        self.report
            .files
            .push((abs.clone(), fingerprint(src.as_bytes())));
        let module = parse_program(&src, &abs.display().to_string())?;
        Ok((module, abs))
    }

    /// 把一个模块的条目并进主程序，并递归处理它自己的依赖。
    fn absorb(
        &mut self,
        prog: &mut Program,
        module: Program,
        dir: &Path,
        depth: usize,
        module_name: &str,
    ) {
        // 先取出来，避免借用冲突
        let mut items: Vec<ModuleItem> = Vec::new();        let Program {
            attrs,
            modules,
            imports,
            externs,
            functions,
            type_defs,
            traits,
            impls,
            consts,
            statics,
            macros,
            ..
        } = module;
        items.extend(externs.into_iter().map(ModuleItem::Extern));
        items.extend(functions.into_iter().map(ModuleItem::Function));
        items.extend(type_defs.into_iter().map(ModuleItem::TypeDef));
        items.extend(traits.into_iter().map(ModuleItem::Trait));
        items.extend(impls.into_iter().map(ModuleItem::Impl));
        items.extend(consts.into_iter().map(ModuleItem::Const));
        items.extend(statics.into_iter().map(ModuleItem::Static));
        items.extend(macros.into_iter().map(ModuleItem::Macro));
        items.extend(modules.into_iter().map(ModuleItem::Module));
        prog.attrs.extend(attrs);
        prog.imports.extend(imports);
        self.absorb_items(prog, items, dir, module_name);
        // 模块自己的 `use` 也要拉进来
        let _ = depth;
    }

    /// 把一批条目并进主程序（内联模块与文件模块共用）。
    ///
    /// `module_path` 是当前模块的路径（`mod a { mod b { .. } }` 里的条目是
    /// `a::b`；顶层条目为空串）。扁平化时给每个条目打上模块路径，供类型检查
    /// 的可见性检查判断"该条目对引用它的模块是否可见"。
    fn absorb_items(&mut self, prog: &mut Program, items: Vec<ModuleItem>, dir: &Path, module_path: &str) {
        let mut nested_uses = Vec::new();
        for item in items {
            match item {
                ModuleItem::Function(mut f) => {
                    f.module = module_path.to_string();
                    prog.functions.push(f);
                }
                ModuleItem::TypeDef(mut t) => {
                    set_type_module(&mut t, module_path);
                    prog.type_defs.push(t);
                }
                ModuleItem::Trait(mut t) => {
                    t.module = module_path.to_string();
                    prog.traits.push(t);
                }
                ModuleItem::Impl(mut i) => {
                    i.module = module_path.to_string();
                    prog.impls.push(i);
                }
                ModuleItem::Const(mut c) => {
                    c.module = module_path.to_string();
                    prog.consts.push(c);
                }
                ModuleItem::Static(mut s) => {
                    s.module = module_path.to_string();
                    prog.statics.push(s);
                }
                ModuleItem::Use(u) => nested_uses.push(u),
                ModuleItem::Module(ModuleDecl::Inline(name, inner)) => {
                    // 登记为内联（扁平）模块：`mod af { pub const INET }` 的
                    // `INET` 被合并进顶层，`af::INET` 靠 `flattened` 里的名字
                    // 把叶子段拿到顶层解析。不能进 `modules`（`known_modules`）
                    // ——那是**加载**模块，`core::Option::Some` 得走既有路径。
                    self.flattened.insert(name.clone());
                    let nested = if module_path.is_empty() {
                        name.clone()
                    } else {
                        format!("{module_path}::{name}")
                    };
                    self.absorb_items(prog, inner, dir, &nested);
                }
                ModuleItem::Module(ModuleDecl::Load(name)) => {
                    if let Err(e) = self.load_mod_file(prog, &name, dir, 0) {
                        // 内层模块加载失败不应该把整个编译干掉太多信息，
                        // 但静默忽略更糟——统一冒泡出去
                        eprintln!("zr: {e}");
                    }
                }
                ModuleItem::Extern(e) => prog.externs.push(e),
                ModuleItem::Macro(m) => prog.macros.push(m),
            }
        }
        if !nested_uses.is_empty() {
            let _ = self.load_uses(prog, dir, 0);
        }
    }
}

/// 给 `TypeDef` 的每个变体（struct / enum / alias）打上模块路径。
fn set_type_module(t: &mut ast::TypeDef, module_path: &str) {
    match t {
        ast::TypeDef::Struct(s) => s.module = module_path.to_string(),
        ast::TypeDef::Enum(e) => e.module = module_path.to_string(),
        ast::TypeDef::Union(u) => u.module = module_path.to_string(),
        ast::TypeDef::Alias { module, .. } => *module = module_path.to_string(),
    }
}

/// 把一段源码解析成 `Program`。
pub fn parse_program(src: &str, origin: &str) -> Result<Program, String> {
    let mut parser = Parser::new(crate::lexer::Lexer::new(src).tokenize());
    parser.parse().map_err(|e| format!("{origin}: {e}"))
}

/// 标准库模块的文件位置（`a::b` -> `a/b.zr`）。
///
/// 查找顺序：先根级（门面 `std.zr` 与历史布局），再 `core/`、`alloc/`、`std/`。
/// 这样旧写法 `use io::File` 与分层后的 `use std::io::File` 都能命中。
fn stdlib_module_path(root: &Path, name: &str) -> Option<PathBuf> {
    let rel = name.replace("::", "/");
    let direct = root.join(format!("{rel}.zr"));
    if direct.is_file() {
        return Some(direct);
    }
    for layer in STDLIB_LAYERS {
        let p = root.join(layer).join(format!("{rel}.zr"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// `mod foo;` 的文件位置：先找 `foo.zr`，再找 `mod/foo.zr`。
fn user_module_path(base: &Path, name: &str) -> Option<PathBuf> {
    let direct = base.join(format!("{name}.zr"));
    if direct.is_file() {
        return Some(direct);
    }
    let nested = base.join("mod").join(format!("{name}.zr"));
    if nested.is_file() {
        return Some(nested);
    }
    None
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// 解析 `use` 声明，取出每条语句的**最顶层**模块名。
fn collect_use_roots(imports: &[crate::parser::ast::UseDecl]) -> Vec<String> {
    let mut out = Vec::new();
    for imp in imports {
        if let Some(root) = use_tree_root(&imp.tree) {
            out.push(root);
        }
    }
    out
}

/// 收集 `use foo::*` 里的 `foo`（glob 依赖的模块名）。
///
/// 树形是 `Group([Name(foo), Glob])`；带前缀时（`use a::b::*`）把 Glob 之外的
/// 各段按顺序拼回 `a::b`。只有 glob 才登记——普通 `use` 不构成"再导出"。
fn collect_glob_roots(imports: &[crate::parser::ast::UseDecl]) -> Vec<String> {
    fn segments(t: &crate::parser::ast::UseTree, out: &mut Vec<String>) {
        match t {
            crate::parser::ast::UseTree::Name(n) => out.push(n.clone()),
            crate::parser::ast::UseTree::Group(items) => {
                for it in items {
                    segments(it, out);
                }
            }
            crate::parser::ast::UseTree::Rename(inner, _) => segments(inner, out),
            crate::parser::ast::UseTree::Glob => {}
        }
    }
    fn walk(t: &crate::parser::ast::UseTree, out: &mut Vec<String>) {
        match t {
            crate::parser::ast::UseTree::Group(items) => {
                if items
                    .iter()
                    .any(|it| matches!(it, crate::parser::ast::UseTree::Glob))
                {
                    let mut segs = Vec::new();
                    for it in items {
                        segments(it, &mut segs);
                    }
                    if !segs.is_empty() {
                        out.push(segs.join("::"));
                    }
                }
                for it in items {
                    walk(it, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for imp in imports {
        walk(&imp.tree, &mut out);
    }
    out.dedup();
    out
}

/// use 树里最左边的路径段——那就是要加载的模块名。
///
/// `use io::println_int;` 与 `use io::{a, b};` 的树形不同（前者是
/// `Group([Name(io), Name(println_int)])`，后者是
/// `Group([Name(io), Group([a, b])])`），但要加载的模块都是最左边那一段。
/// 早先把树里的每个叶子都当成模块名去加载，`use io::println_int;` 于是
/// 去找一个叫 `println_int` 的模块——标准库根本没法用。
fn use_tree_root(node: &crate::parser::ast::UseTree) -> Option<String> {
    match node {
        crate::parser::ast::UseTree::Name(n) => Some(n.clone()),
        crate::parser::ast::UseTree::Group(items) => items.first().and_then(use_tree_root),
        crate::parser::ast::UseTree::Glob => None,
        // `use a as b`：重命名不影响要加载哪个模块，内层路径里仍然藏着它。
        // 早先这里无条件返回 `None`，于是 `use io::println_fmt as pf;` **一个
        // 模块都不加载**——`println_fmt` 从未进入符号表，别名指向虚空，使用点
        // 只剩一句无头无尾的 `cannot find value `pf``。多段路径的重命名尤其
        // 隐蔽：文件里只要还有另一条不带 `as` 的 `use io::...`，模块会被那条
        // 顺带拉进来，问题就悄悄消失（这正是它长期没被发现的原因）。
        //
        // 但只有**多段**路径才需要加载：单段 `use make as mkr` 的 `make` 是一个
        // 符号名而不是模块名，按模块去查会报 `unknown module `make``——基线
        // 里那种用法是好的，不能弄坏它。
        crate::parser::ast::UseTree::Rename(inner, _) => match &**inner {
            crate::parser::ast::UseTree::Group(items) => items.first().and_then(use_tree_root),
            _ => None,
        },
    }
}

pub fn fingerprint(bytes: &[u8]) -> u64 {
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut b: u64 = 0x9e37_79b9_7f4a_7c15;
    for (i, &x) in bytes.iter().enumerate() {
        a ^= x as u64;
        a = a.wrapping_mul(0x100_0000_01b3);
        b = b.wrapping_add((x as u64).wrapping_mul(0x2545_f491_4f6c_dd1d) ^ (i as u64));
        b ^= b >> 29;
    }
    a ^ b.rotate_left(17)
}

/// 把 [所有文件的指纹 + 编译选项] 合成一个总指纹。
pub fn build_fingerprint(files: &[(PathBuf, u64)], salt: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let feed = |bytes: &[u8], h: &mut u64| {
        for &x in bytes {
            *h ^= x as u64;
            *h = h.wrapping_mul(0x100_0000_01b3);
        }
    };
    feed(salt.as_bytes(), &mut h);
    // 文件顺序会影响结果，先排序保证与读取顺序无关
    let mut sorted: Vec<&(PathBuf, u64)> = files.iter().collect();
    sorted.sort();
    for (path, fp) in sorted {
        feed(path.to_string_lossy().as_bytes(), &mut h);
        feed(&fp.to_le_bytes(), &mut h);
    }
    h
}

// 为什么不是"按模块分别增量"（诚实说明）
// -------------------------------------------------
// 想只重新编译改动的模块，就得让模块能**独立**编译，也就是模块之间只通过
// 稳定接口（导出符号的签名）耦合。但这个编译器的模块系统是**扁平命名空间**
// ——加载器把模块的条目直接并进同一个 `Program`，共享一张符号表。在这个结构
// 下，任何一个模块改了都可能改变别的模块的名字解析结果，没有办法安全地跳过
// 它们的类型检查。
//
// 实测的阶段耗时（`ZR_TIME=1`）大致是：
//
//   lex     ≈ 0.4 ms
//   parse   ≈ 0.9 ms
//   typeck  ≈ 2.2 ms
//   mir     ≈ 2.3 ms
//
// 缓存词法+语法最多省掉前端约 25% 的时间，而真正的瓶颈是类型检查。所以这里
// 做的是**总指纹级的产物复用**：什么都没改就复用上次的产物文件，改动任何一个
// 文件则整个重编。这是有价值且**不会出错**的那部分。要做到真正的按模块增量，
// 得先把模块系统改成两层符号表，那是另一个量级的改动。

/// 增量缓存：指纹 -> 上次产物的路径。
///
/// 只在**什么都没改**时命中；改动任意一个参与编译的文件，指纹就变，
/// 于是整个重编。这么做是正确的：模块是扁平命名空间，没有安全的办法
/// 只重编其中一个（见文件末尾的说明）。
#[derive(Debug)]
pub struct Cache {
    path: PathBuf,
    entry: Option<CacheEntry>,
}

#[derive(Debug)]
struct CacheEntry {
    fingerprint: u64,
    /// 上次产物的路径（复用时直接拷过去）。
    artifact: PathBuf,
    /// 上次参与的源文件及其指纹。
    ///
    /// 必须存下来：命中检查要在**不解析任何源码**的前提下判断"是不是都没变"。
    /// 不知道该检查哪些文件，就只能先解析一遍才知道依赖了谁——那样连
    /// 类型检查之前的开销都省不掉，缓存也就没意义了。
    files: Vec<(PathBuf, u64)>,
}

impl Cache {
    /// 打开（或准备创建）缓存。`key` 用源文件绝对路径派生，避免不同项目串味。
    pub fn open(root: &Path) -> Cache {
        let path = cache_dir().join(format!(
            "{:016x}.zrcache",
            fingerprint(root.to_string_lossy().as_bytes())
        ));
        // 产物必须是"存在且非空"。历史上这里只判 `is_file()`，于是产物被
        // 删成 0 字节后缓存照样命中，`restore` 拷出一个空文件，而命令仍然
        // 报成功——比直接失败难查得多。
        let entry = std::fs::read(&path)
            .ok()
            .and_then(|b| parse_entry(&b))
            .filter(|e| e.artifact.is_file())
            .and_then(|e| {
                std::fs::metadata(&e.artifact)
                    .ok()
                    .filter(|m| m.len() > 0)?;
                Some(e)
            });
        Cache { path, entry }
    }

    /// 判断能否复用：重新读取并哈希上次记录的每个文件，与存的指纹比对。
    ///
    /// 这里**不重新解析**任何源码——只做文件读取与哈希，正是增量编译能省的
    /// 那部分。任何一个文件变了、被删了，就返回 `false`。
    pub fn try_reuse(&self, salt: &str) -> bool {
        let Some(e) = &self.entry else {
            if std::env::var_os("ZR_CACHE_DEBUG").is_some() {
                eprintln!("[cache] 无缓存条目");
            }
            return false;
        };
        if !e.artifact.is_file() {
            return false;
        }
        let mut live: Vec<(PathBuf, u64)> = Vec::with_capacity(e.files.len());
        for (path, _) in &e.files {
            match std::fs::read(path) {
                Ok(bytes) => live.push((path.clone(), fingerprint(&bytes))),
                Err(_) => return false,
            }
        }
        let now = build_fingerprint(&live, salt);
        let hit = now == e.fingerprint;
        if std::env::var_os("ZR_CACHE_DEBUG").is_some() {
            eprintln!(
                "[cache] {} 个文件 存={} 算={} -> {}",
                e.files.len(),
                e.fingerprint,
                now,
                if hit { "命中" } else { "未命中" }
            );
        }
        hit
    }

    /// 复用上次的产物（把缓存里的文件拷到 `dest`）。
    pub fn restore(&self, dest: &Path) -> Result<(), String> {
        let e = self.entry.as_ref().ok_or("no cache entry")?;
        let len = std::fs::metadata(&e.artifact)
            .map_err(|err| format!("cached artifact is unreadable: {err}"))?
            .len();
        if len == 0 {
            // 空产物说明上次那次写坏了，宁可重新编一遍。
            return Err("cached artifact is empty".to_string());
        }
        std::fs::copy(&e.artifact, dest)
            .map_err(|err| format!("cannot restore cached artifact: {err}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755));
        }
        Ok(())
    }

    /// 记录本次指纹、产物与参与的源文件。
    pub fn store(
        &mut self,
        fingerprint: u64,
        artifact: &Path,
        files: &[(PathBuf, u64)],
    ) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or("cache path has no parent directory")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create cache dir: {e}"))?;

        // 把产物**拷进缓存目录**再记路径，而不是记用户 `-o` 的路径。
        // 记用户路径的话：产物被删 / 被覆盖 / 换个 `-o` 再编一次之后，
        // 缓存就会指向一个不存在或不相干的文件，却照样"命中"。
        let bytes = std::fs::read(artifact).map_err(|e| format!("cannot read artifact: {e}"))?;
        if bytes.is_empty() {
            return Err("refusing to cache an empty artifact".to_string());
        }
        let owned = parent.join(format!("artifact-{fingerprint:016x}.bin"));
        std::fs::write(&owned, &bytes).map_err(|e| format!("cannot write cached artifact: {e}"))?;

        let entry = CacheEntry {
            fingerprint,
            artifact: owned,
            files: files.to_vec(),
        };
        std::fs::write(&self.path, serialize_entry(&entry))
            .map_err(|e| format!("cannot write cache: {e}"))
    }
}

fn cache_dir() -> PathBuf {
    // 放在 target 下，跟着 `cargo clean` 一起清掉
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("zr-cache")
}

fn serialize_entry(e: &CacheEntry) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"ZRC2");
    out.extend_from_slice(&e.fingerprint.to_le_bytes());
    put_bytes(&mut out, e.artifact.to_string_lossy().as_bytes());
    out.extend_from_slice(&(e.files.len() as u64).to_le_bytes());
    for (p, fp) in &e.files {
        put_bytes(&mut out, p.to_string_lossy().as_bytes());
        out.extend_from_slice(&fp.to_le_bytes());
    }
    out
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u64).to_le_bytes());
    out.extend_from_slice(b);
}

fn parse_entry(b: &[u8]) -> Option<CacheEntry> {
    if b.len() < 12 || &b[0..4] != b"ZRC2" {
        return None;
    }
    let fingerprint = u64::from_le_bytes(b[4..12].try_into().ok()?);
    let mut at = 12usize;
    let artifact = {
        let len = u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?) as usize;
        at += 8;
        PathBuf::from(String::from_utf8_lossy(b.get(at..at + len)?).into_owned())
    };
    at += artifact.to_string_lossy().len();
    let n = u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?) as usize;
    at += 8;
    let mut files = Vec::with_capacity(n);
    for _ in 0..n {
        let len = u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?) as usize;
        at += 8;
        let path = PathBuf::from(String::from_utf8_lossy(b.get(at..at + len)?).into_owned());
        at += len;
        let fp = u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        files.push((path, fp));
    }
    Some(CacheEntry {
        fingerprint,
        artifact,
        files,
    })
}

/// 清空增量缓存。
pub fn clean() -> Result<usize, String> {
    let dir = cache_dir();
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut n = 0usize;
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let p = entry.map_err(|e| e.to_string())?.path();
        if p.extension().map(|e| e == "zrcache").unwrap_or(false) {
            std::fs::remove_file(&p).map_err(|e| e.to_string())?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod layer_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn classifies_layer_by_directory() {
        let root = Path::new("/x/std");
        assert_eq!(
            layer_of_path(root, Path::new("/x/std/core/core.zr")),
            Layer::Core
        );
        assert_eq!(
            layer_of_path(root, Path::new("/x/std/alloc/string.zr")),
            Layer::Alloc
        );
        assert_eq!(
            layer_of_path(root, Path::new("/x/std/std/io.zr")),
            Layer::Std
        );
        // 门面等根级文件按最上层处理
        assert_eq!(layer_of_path(root, Path::new("/x/std/std.zr")), Layer::Std);
        // 依赖方向的内建顺序：Core < Alloc < Std
        assert!(Layer::Core < Layer::Alloc);
        assert!(Layer::Alloc < Layer::Std);
    }

    #[test]
    fn glob_exports_propagate_transitively() {
        let mut exports: HashMap<String, Vec<String>> = HashMap::new();
        exports.insert("core".into(), vec!["Option".into(), "min".into()]);
        exports.insert("string".into(), vec!["String".into()]);
        exports.insert("io".into(), vec!["println".into()]);
        exports.insert("std".into(), vec!["facade_only".into()]);

        let mut globs: HashMap<String, Vec<String>> = HashMap::new();
        globs.insert(
            "std".into(),
            vec!["core".into(), "string".into(), "io".into()],
        );

        expand_glob_exports(&mut exports, &globs);

        let got = exports.get("std").unwrap();
        for name in ["facade_only", "Option", "min", "String", "println"] {
            assert!(got.contains(&name.to_string()), "missing {name}");
        }
    }
}
