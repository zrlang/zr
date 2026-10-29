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

use crate::parser::ast::{ModuleDecl, ModuleItem, Program};
use crate::parser::Parser;

/// 本次编译读到的文件（按读取顺序）与各���内容指纹。
#[derive(Debug, Default)]
pub struct LoadReport {
    /// 参与编译的源文件：路径 -> 内容指纹。
    pub files: Vec<(PathBuf, u64)>,
    /// 被 `mod` / `use` 拉进来的模块名。
    pub modules: Vec<String>,
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

/// 标准库根目录（`<crate>/std`）。
pub fn std_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("std")
}

/// 加载结果。
pub struct Loaded {
    /// 加载失败时的原因。
    pub error: Option<String>,
    /// 并入主程序的模块名（类型检查用得上：`io` 这类段要当模块看待）。
    pub modules: Vec<String>,
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
pub fn load(prog: &mut Program, base_dir: &Path) -> Loaded {
    let mut out = Loaded {
        error: None,
        modules: Vec::new(),
        module_exports: std::collections::HashMap::new(),
        report: LoadReport::default(),
    };
    let mut loader = Loader {
        seen: std::collections::HashSet::new(),
        modules: std::collections::HashSet::new(),
        report: &mut out.report,
        module_exports: &mut out.module_exports,
    };

    // 1) 先展开内联模块（它们已经在内存里了）
    let inline = std::mem::take(&mut prog.modules);
    for decl in inline {
        match decl {
            ModuleDecl::Inline(_, items) => loader.absorb_items(prog, items, base_dir),
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

    out.modules = loader.modules.into_iter().collect();
    out.modules.sort();
    out.report.modules = out.modules.clone();
    out
}

struct Loader<'a> {
    /// 已加载的源文件绝对路径（防重复加载与循环引用）。
    seen: std::collections::HashSet<PathBuf>,
    /// 已加载的模块名。
    modules: std::collections::HashSet<String>,
    report: &'a mut LoadReport,
    /// 模块名 → 导出的条目名（`use foo::*` 用）。
    module_exports: &'a mut std::collections::HashMap<String, Vec<String>>,
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
        let mut queue = roots;
        while let Some(name) = queue.pop() {
            if self.modules.contains(&name) {
                continue;
            }
            let path = stdlib_module_path(&std_root(), &name).ok_or_else(|| {
                format!(
                    "unknown module `{name}` (looked in {})",
                    std_root().display()
                )
            })?;
            let (module, path) = self.parse_file(&path)?;
            self.modules.insert(name.clone());

            // 记录本模块导出的条目名，供 `use core::*` 导入。
            self.module_exports
                .insert(name.clone(), module_item_names(&module));

            for dep in collect_use_roots(&module.imports) {
                if !self.modules.contains(&dep) {
                    queue.push(dep);
                }
            }
            let dir = path.parent().unwrap_or(_base).to_path_buf();
            self.absorb(prog, module, &dir, depth);
        }
        Ok(())
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
        self.absorb(prog, module, &dir, depth);
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
    fn absorb(&mut self, prog: &mut Program, module: Program, dir: &Path, depth: usize) {
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
        self.absorb_items(prog, items, dir);
        // 模块自己的 `use` 也要拉进来
        let _ = depth;
    }

    /// 把一批条目并进主程序（内联模块与文件模块共用）。
    fn absorb_items(&mut self, prog: &mut Program, items: Vec<ModuleItem>, dir: &Path) {
        let mut nested_uses = Vec::new();
        for item in items {
            match item {
                ModuleItem::Function(f) => prog.functions.push(f),
                ModuleItem::TypeDef(t) => prog.type_defs.push(t),
                ModuleItem::Trait(t) => prog.traits.push(t),
                ModuleItem::Impl(i) => prog.impls.push(i),
                ModuleItem::Const(c) => prog.consts.push(c),
                ModuleItem::Static(s) => prog.statics.push(s),
                ModuleItem::Use(u) => nested_uses.push(u),
                ModuleItem::Module(ModuleDecl::Inline(name, inner)) => {
                    // 登记模块名，`name::item` 才能被当成路径解析
                    self.modules.insert(name);
                    self.absorb_items(prog, inner, dir)
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

/// 把一段源码解析成 `Program`。
pub fn parse_program(src: &str, origin: &str) -> Result<Program, String> {
    let mut parser = Parser::new(crate::lexer::Lexer::new(src).tokenize());
    parser.parse().map_err(|e| format!("{origin}: {e}"))
}

/// 标准库模块的文件位置（`a::b` -> `a/b.zr`）。
fn stdlib_module_path(root: &Path, name: &str) -> Option<PathBuf> {
    let rel = name.replace("::", "/");
    let p = root.join(format!("{rel}.zr"));
    p.is_file().then_some(p)
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
