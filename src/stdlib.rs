//==============================================================================
// stdlib.rs — 标准库加载
//
// `std/*.zr` 里的模块不是靠 `mod` 声明找到的，而是按**需要**加载：
// 源文件里写了 `use io::{println}`，就去 `std/io.zr` 找 `io`，把它
// 的定义并进当前编译单元，再递归处理它自己的 `use`。
//
// 这样做的理由：
//   * 只加载真正用到的模块——把全部 9 个模块（约 3900 行）无脑并进来，
//     会把没人用的代码也发射出去，还容易撞上跨模块重名的 `pub fn`；
//   * `net.zr` 依赖尚未实现的 `zr_socket`，用不到就不该编译它。
//
// 已知限制：并进来的是**扁平命名空间**，所以同时加载的两个模块不能有
// 同名 `pub fn`（例如 `collections` 和 `slice` 都有 `len`）。真要支持
// 得把符号表改成 `module::name` 的两层结构。
//==============================================================================

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::parser::ast::{Program, UseTree};
use crate::parser::Parser;

/// 标准库根目录（`<crate>/std`）。
pub fn std_root() -> PathBuf {
    // 可执行文件在 target/{profile}/zr，回退到当前工作目录。
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("std")
}

/// 加载结果：并进主程序的模块名（供名称解析把 `io` 这样的段当模块看待）。
#[derive(Debug, Default)]
pub struct Loaded {
    /// 成功加载的模块名。
    pub modules: HashSet<String>,
}

/// 把源文件（及其传递依赖的）`use` 到的标准库模块并进 `prog`。
pub fn load(prog: &mut Program) -> Result<Loaded, String> {
    let root = std_root();
    let mut loaded = Loaded::default();
    let mut pending: Vec<String> = collect_roots(&prog.imports);

    while let Some(name) = pending.pop() {
        if loaded.modules.contains(&name) {
            continue;
        }
        // 先标记，避免互相引用时无限递归
        loaded.modules.insert(name.clone());

        let path = module_path(&root, &name);
        let Some(path) = path else {
            return Err(format!(
                "unknown module `{name}` (looked for {})",
                module_path(&root, &name)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| format!("{}/{}.zr", root.display(), name))
            ));
        };

        let src = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
        let module = parse_program(&src, &path.display().to_string())?;

        // 先把它的 use 目标也排进队列，再合并它的定义
        for dep in collect_roots(&module.imports) {
            if !loaded.modules.contains(&dep) {
                pending.push(dep);
            }
        }
        merge(prog, module);
    }

    Ok(loaded)
}

/// 把一段源码解析成 `Program`。
fn parse_program(src: &str, origin: &str) -> Result<Program, String> {
    let tokens = crate::lexer::Lexer::new(src).tokenize();
    let mut parser = Parser::new(tokens);
    parser.parse().map_err(|e| format!("{origin}: {e}"))
}

/// 解析 `use` 声明，取出每条语句的**最顶层模块名**。
fn collect_roots(imports: &[crate::parser::ast::UseDecl]) -> Vec<String> {
    imports.iter().filter_map(|imp| module_root(&imp.tree)).collect()
}

/// 取 `use` 树里最左边的那个名字——也就是模块名。
///
/// 解析器把 `use io::{a, b}` 构造成 `Group([Name("io"), Group([...])])`，
/// 把 `use a::b::c` 构造成 `Group([Name("a"), Group([Name("b"), ...])])`，
/// 所以模块名永远是树的**最左叶子**；`{...}` 里的项名不是模块，不能收。
fn module_root(tree: &UseTree) -> Option<String> {
    match tree {
        UseTree::Name(n) => Some(n.clone()),
        UseTree::Rename(inner, _) => module_root(inner),
        UseTree::Group(items) => items.first().and_then(module_root),
        UseTree::Glob => None,
    }
}

/// 模块名 -> 文件路径。支持 `a/b` 形式的下划线路径。
fn module_path(root: &Path, name: &str) -> Option<PathBuf> {
    let rel = name.replace("::", "/");
    let p = root.join(format!("{rel}.zr"));
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// 把一个模块的定义并进主程序。
///
/// 词法/语法阶段把 `pub` 修饰符消费掉了（没有存进 AST），这里没法按可见性
/// 过滤，只能整份并入。因为是**按需加载**，没被 `use` 到的模块不会进来，
/// 泄漏面有限。
fn merge(prog: &mut Program, module: Program) {
    prog.functions.extend(module.functions);
    prog.type_defs.extend(module.type_defs);
    prog.consts.extend(module.consts);
    prog.statics.extend(module.statics);
}
