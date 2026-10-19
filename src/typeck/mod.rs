//! 类型检查器主模块。
//!
//! 类型检查流程：
//! ```text
//! AST → Name Resolution → HIR 构建 → 类型推断/统一 → trait 求解
//!     → 借用检查（NLL） → 模式穷尽性检查 → 闭包分析 → 编译期求值验证
//! ```
//!
//! 所有阶段共享一个 `TypeChecker`，以便跨阶段传递符号表、错误集合等状态。

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

/// 整数字面量后缀 → 具体整型。无后缀或未知后缀返回 `None`。
///
/// `usize`/`isize` 目前按 64 位平台字长处理。
fn int_suffix_ty(suffix: &str) -> Option<Ty> {
    Some(match suffix {
        "i8" => Ty::Int(8),
        "i16" => Ty::Int(16),
        "i32" => Ty::Int(32),
        "i64" => Ty::Int(64),
        "i128" => Ty::Int(128),
        "isize" => Ty::Int(64),
        "u8" => Ty::Uint(8),
        "u16" => Ty::Uint(16),
        "u32" => Ty::Uint(32),
        "u64" => Ty::Uint(64),
        "u128" => Ty::Uint(128),
        "usize" => Ty::Uint(64),
        _ => return None,
    })
}

/// 类型检查器。
#[derive(Debug)]
pub struct TypeChecker {
    /// 目标允许的最大整数位宽（默认 128，即不额外限制）。
    max_int_bits: u16,
    /// 符号表：名称 → 定义 ID。
    symbols: HashMap<String, DefId>,
    /// 反向：DefId → 名称（用于错误报告）。
    def_names: HashMap<DefId, String>,
    /// 类型定义表。
    type_defs: HashMap<DefId, hir::HirTypeDef>,
    /// 函数表。
    functions: HashMap<DefId, hir::HirFunction>,
    /// trait 表。
    trait_defs: HashMap<DefId, hir::HirTrait>,
    /// impl 块列表。
    impl_blocks: Vec<hir::HirImpl>,
    /// 常量表。
    consts: HashMap<DefId, hir::HirConst>,
    /// 静态变量表。
    statics: HashMap<DefId, hir::HirStatic>,
    /// 局部变量表（当前作用域链）。
    locals: HashMap<LocalId, Ty>,
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
    /// 错误集合（延迟报告，允许错误恢复）。
    errors: Vec<CompileError>,
    /// 导入解析缓存。
    use_aliases: HashMap<String, DefId>,
    /// 已加载的标准库模块名。
    ///
    /// `use io::{println}` 里的 `io` 不是一个可调用的符号，而是模块
    /// 限定符——标准库的定义已经被并进当前编译单元，所以这里只需要
    /// 知道"这个名字是模块"，不用再解析成 DefId。
    known_modules: HashSet<String>,
    /// 警告集合（deprecated 使用等）。
    warnings: Vec<CompileError>,
    /// 已处理的 cfg 条件（用于过滤条目）。
    cfg_enabled: bool,
    /// AST 层的函数表（名字 → 函数）。
    ///
    /// comptime 函数需要在**类型检查函数体之前**就能被求值（`comptime fact(4)`
    /// 出现在某个函数体里，而 `fact` 自己可能还没检查到）。HIR 这时候还不存在，
    /// 所以必须留一份 AST。HIR 版求值器用于阶段 12 的折叠，AST 版用于
    /// `check_expr` 里的即时求值。
    ast_fns: HashMap<String, ast::Function>,
}

impl Default for TypeChecker {
    fn default() -> Self {
        // InferCtxt 借用 TypeChecker 的字段，无法在构造时直接填入。
        // 先用 empty() 占位（指向 'static 空集合），实际检查时再按需重建。
        Self {
            max_int_bits: 128,
            symbols: HashMap::new(),
            def_names: HashMap::new(),
            type_defs: HashMap::new(),
            functions: HashMap::new(),
            trait_defs: HashMap::new(),
            impl_blocks: Vec::new(),
            consts: HashMap::new(),
            statics: HashMap::new(),
            locals: HashMap::new(),
            infer: InferCtxt::empty(),
            next_def: 1,
            next_local: 1,
            next_bb: 0,
            scope_depth: 0,
            scope_stack: Vec::new(),
            local_scopes: Vec::new(),
            locals_by_def: HashMap::new(),
            current_function: None,
            errors: Vec::new(),
            use_aliases: HashMap::new(),
            known_modules: HashSet::new(),
            warnings: Vec::new(),
            cfg_enabled: true,
            ast_fns: HashMap::new(),
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

    /// 设置目标允许的最大整数位宽。
    ///
    /// 32 位目标目前没有多字长寄存器对，写出来的 i64 只是**静默截断**成
    /// 32 位——这种"能编过但结果是错的"比报错糟糕得多，所以在类型检查阶段
    /// 就把超宽的显式标注挡掉。
    pub fn set_max_int_bits(&mut self, bits: u16) {
        self.max_int_bits = bits;
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
        // ===== 阶段 0：属性预处理 =====
        self.process_attributes(prog)?;

        // ===== 阶段 1：声明收集（pass 1）=====
        self.collect_declarations(prog)?;

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

        // ===== 阶段 10：闭包捕获分析 =====
        self.analyze_closures(&functions)?;

        // ===== 阶段 11：未使用变量 / 未使用导入警告 =====
        self.check_unused(&functions)?;

        // ===== 阶段 12：comptime 函数调用的编译期折叠 =====
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

        Ok(hir::HirModule {
            functions,
            def_names: self.def_names.clone(),
            type_defs: self.type_defs.clone(),
            consts: self.consts.clone(),
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
        // cfg 过滤：检查模块级属性
        for attr in &prog.attrs {
            if attr.name == "cfg" {
                // 模块级 cfg：如果条件不满足，整个模块被过滤
                // 简化实现：模块级 cfg 暂不处理（需要目标信息）
            }
        }

        // 收集所有条目的属性信息
        for item in &prog.functions {
            self.process_fn_attrs(item)?;
        }
        for td in &prog.type_defs {
            self.process_type_attrs(td)?;
        }
        for t in &prog.traits {
            self.process_trait_attrs(t)?;
        }
        Ok(())
    }

    /// 处理函数属性。
    fn process_fn_attrs(&mut self, f: &ast::Function) -> Result<()> {
        // cfg 过滤
        if !self.cfg_enabled {
            return Ok(());
        }

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
        let attrs: &[ast::Attribute] = match td {
            ast::TypeDef::Struct(s) => &s.attrs,
            _ => &[],
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
                "Debug" | "Clone" | "Copy" => {
                    // 生成 impl 块：在类型检查器中注册为已实现的 trait
                    // 实际的 trait 求解在 infer.rs 的 check_trait_impl 中处理
                    let impl_block = hir::HirImpl {
                        target: Ty::Named(def_id, Vec::new()),
                        trait_id: None, // 将在后续解析
                        where_clause: Vec::new(),
                        span: Span::unknown(),
                    };
                    self.impl_blocks.push(impl_block);
                }
                _ => {
                    // 未知的 derive trait：报错
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
            let id = self.alloc_def(item.name.clone());
            self.ast_fns.insert(item.name.clone(), item.clone());
            self.functions.insert(
                id,
                hir::HirFunction {
                    name: item.name.clone(),
                    params: Vec::new(),
                    return_type: Ty::Infer(InferId(0)),
                    body: None,
                    is_async: item.is_async,
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
        for td in &prog.type_defs {
            let id = self.alloc_def(match td {
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
                ast::TypeDef::Alias {
                    name: _, target, ..
                } => {
                    let target_ty = self.ast_to_hir_type(target)?;
                    self.type_defs.insert(
                        id,
                        hir::HirTypeDef::Alias {
                            def_id: id,
                            target: target_ty,
                        },
                    );
                }
            }
        }
        for t in &prog.traits {
            let id = self.alloc_def(t.name.clone());
            let (is_marker, _) = classify_trait(t);
            self.trait_defs.insert(
                id,
                hir::HirTrait {
                    assoc_types: Vec::new(),
                    is_marker,
                },
            );
        }
        for c in &prog.consts {
            let id = self.alloc_def(c.name.clone());
            self.consts.insert(
                id,
                hir::HirConst {
                    ty: Ty::Infer(InferId(0)),
                    value: Box::new(self.dummy_expr()),
                },
            );
        }
        for s in &prog.statics {
            let id = self.alloc_def(s.name.clone());
            self.statics.insert(
                id,
                hir::HirStatic {
                    ty: Ty::Infer(InferId(0)),
                    init: Box::new(self.dummy_expr()),
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
                } else {
                    self.errors.push(
                        CompileError::name_error(format!("cannot resolve `{}` in import", name))
                            .with_span(*span),
                    );
                }
            }
            UseTree::Rename(inner, alias) => {
                if let UseTree::Name(name) = inner.as_ref() {
                    if let Some(id) = self.symbols.get(name).copied() {
                        self.use_aliases.insert(alias.clone(), id);
                    } else {
                        self.errors.push(
                            CompileError::name_error(format!(
                                "cannot resolve `{}` in import",
                                name
                            ))
                            .with_span(*span),
                        );
                    }
                }
            }
            UseTree::Group(trees) => {
                for t in trees {
                    self.resolve_use_tree(t, span)?;
                }
            }
            UseTree::Glob => {
                // `use foo::*` —— 需要模块系统支持，此处记录为 future feature
                self.errors.push(CompileError::internal(
                    "glob imports (`use foo::*`) require module system support (not yet implemented)",
                )
                .with_span(*span));
            }
        }
        Ok(())
    }

    // ==================================================================
    // 阶段 3：类型定义检查
    // ==================================================================

    fn check_type_defs(&mut self, prog: &Program) -> Result<()> {
        for td in &prog.type_defs {
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
        let def_id = self.find_def(&s.name)?;
        let mut fields = Vec::new();
        let mut offsets = OffsetTracker::new();

        for (idx, f) in s.fields.iter().enumerate() {
            let ty = self.ast_to_hir_type(&f.ty)?;
            offsets.alloc(self.infer.size_of(&ty), self.infer.align_of(&ty));
            fields.push(hir::HirField {
                name: f.name.clone(),
                ty: ty.clone(),
                index: idx,
            });
        }

        // `#[repr(..)]` 写错了必须报错，不能默默回退到默认布局——
        // 那会让 FFI 结构体的实际布局和调用方以为的不一致。
        let repr = hir_repr(&convert_attrs(&s.attrs))?;
        if let Some(hir_td) = self.type_defs.get_mut(&def_id) {
            if let hir::HirTypeDef::Struct(hs) = hir_td {
                hs.fields = fields;
                hs.repr = repr;
            }
        }
        Ok(())
    }

    fn check_enum_def(&mut self, e: &ast::EnumDef) -> Result<()> {
        let def_id = self.find_def(&e.name)?;
        let mut variants = Vec::new();

        for v in &e.variants {
            if let Some(d) = &v.discriminant {
                // 编译期求值判别式（用于错误检查）
                self.eval_const_expr(d)?;
            }
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
            variants.push(hir::HirVariant { payload });
        }

        // `#[repr(i64)]` 走的是另一条路（直接存 IntegerRepr），同样要挡住
        // 超宽标注。必须在 `get_mut` 之前算——闭包里再借用 `self` 会和
        // 可变借用冲突。
        if let Some(AstType::I(w)) = e.repr.as_ref() {
            self.check_int_width(*w as u16)?;
        }

        if let Some(hir_td) = self.type_defs.get_mut(&def_id) {
            if let hir::HirTypeDef::Enum(he) = hir_td {
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
        }
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
        if let Some(hir_td) = self.type_defs.get_mut(&def_id) {
            if let hir::HirTypeDef::Union(hu) = hir_td {
                hu.fields = fields;
            }
        }
        Ok(())
    }

    // ==================================================================
    // 阶段 4：trait / impl 检查
    // ==================================================================

    fn check_traits(&mut self, prog: &Program) -> Result<()> {
        for t in &prog.traits {
            let id = self.find_def(&t.name)?;
            // 先验证 super trait 存在（不可变借用），再取可变引用
            for st in &t.super_traits {
                if let AstType::Named { path, .. } = st {
                    let _ = self.resolve_type_path(path)?;
                }
            }
            // 先解析关联类型的默认值（避免与 get_mut 借用冲突）
            let assoc_types: Vec<_> = t.assoc_types.iter().map(|_| hir::HirAssocType).collect();
            let hir_t = self.trait_defs.get_mut(&id).unwrap();
            hir_t.assoc_types = assoc_types;
            for m in &t.methods {
                let fid = self.find_def(&m.name)?;
                // trait 中的方法是声明，不要求有 body
                self.functions.insert(fid, self.fn_to_hir(m, fid));
            }
        }
        Ok(())
    }

    fn check_impls(&mut self, prog: &Program) -> Result<()> {
        for ib in &prog.impls {
            let target = self.ast_to_hir_type(&ib.target)?;
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
                hc.value = Box::new(value);
            }
        }
        Ok(())
    }

    fn check_statics(&mut self, prog: &Program) -> Result<()> {
        for s in &prog.statics {
            let id = self.find_def(&s.name)?;
            let ty = self.ast_to_hir_type(&s.ty)?;
            let init = self.check_expr(&s.init, &ty)?;
            if let Some(hs) = self.statics.get_mut(&id) {
                hs.ty = ty;
                hs.init = Box::new(init);
            }
        }
        Ok(())
    }

    fn check_function_signatures(&mut self, prog: &Program) -> Result<()> {
        for f in &prog.functions {
            let id = self.find_def(&f.name)?;
            let mut ret = self.ast_to_hir_type(&f.return_type)?;
            // async fn 的返回类型降级为 `impl Future<Output = T>`（不透明类型）
            if f.is_async && !matches!(ret, Ty::Opaque(_)) {
                let opaque_id = self.alloc_def(format!("async_future_{}", self.next_def));
                ret = Ty::Opaque(opaque_id);
            }
            if let Some(hf) = self.functions.get_mut(&id) {
                hf.return_type = ret;
            }
        }
        Ok(())
    }

    // ==================================================================
    // 阶段 7：函数体类型检查
    // ==================================================================

    fn check_function_bodies(&mut self, prog: &Program) -> Result<Vec<hir::HirFunction>> {
        let _result: Vec<hir::HirFunction> = Vec::new();
        for f in &prog.functions {
            let id = self.find_def(&f.name)?;
            self.current_function = Some(id);
            self.scope_stack.clear();
            self.push_scope();

            let ret_ty = self.functions[&id].return_type.clone();
            self.infer.return_ty = Some(ret_ty.clone());

            // 检查 receiver
            if let Some(recv) = &f.receiver {
                let local = self.alloc_local();
                // 显式接收者类型优先（`self: &mut Self`），否则退回 impl 目标
                let self_ty = match &recv.ty {
                    Some(t) => self.ast_to_hir_type(t)?,
                    None => Ty::Named(id, Vec::new()),
                };
                let ty = match recv.mutability {
                    ast::Mutability::Mutable => {
                        Ty::Reference(Box::new(self_ty), hir::Mutability::Mutable)
                    }
                    ast::Mutability::Immutable => {
                        Ty::Reference(Box::new(self_ty), hir::Mutability::Immutable)
                    }
                };
                self.locals.insert(local, ty.clone());
                self.infer.locals.insert(local, ty);
            }

            // 检查参数
            let mut params = Vec::new();
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
            }
            self.pop_scope();
        }
        // 函数顺序必须确定：按 DefId 排序（保证可复现产物）。
        let mut funcs: Vec<_> = self.functions.iter().collect();
        funcs.sort_by_key(|(id, _)| id.0);
        Ok(funcs.into_iter().map(|(_, f)| f.clone()).collect())
    }

    fn check_block(&mut self, b: &AstBlock, expected: &Ty) -> Result<hir::HirBlock> {
        self.push_scope();
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
        self.pop_scope();
        Ok(hir::HirBlock { stmts, tail })
    }

    fn check_stmt(&mut self, s: &Statement) -> Result<HirStmt> {
        match s {
            Statement::Let(_pat, ty_ann, init, span) => {
                let ty = if let Some(t) = ty_ann {
                    self.ast_to_hir_type(t)?
                } else if let Some(e) = init {
                    self.infer_expr_ty(e)?
                } else {
                    return Err(CompileError::type_error(
                        "type annotations needed for let-binding without initializer",
                    )
                    .with_span(*span));
                };
                let init_hir = if let Some(e) = init {
                    let h = self.check_expr(e, &ty)?;
                    self.infer
                        .unify(&h.ty, &ty, e.span())
                        .map_err(|e| self.errors.push(e))
                        .ok();
                    Some(Box::new(h))
                } else {
                    None
                };
                let hir_pat = self.lower_pattern(&_pat, &ty)?;
                // 非绑定模式（通配 / 字面量 / 元组）没有单个可引用的名字，
                // 用一个匿名局部占位，保证 `HirStmt::Let` 的形状不变。
                let local = first_binding(&hir_pat).unwrap_or_else(|| self.alloc_local());
                Ok(HirStmt::Let {
                    local,
                    init: init_hir,
                })
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
                    self.infer
                        .unify(&h.ty, &ty, e.span())
                        .map_err(|e| self.errors.push(e))
                        .ok();
                    Some(Box::new(h))
                } else {
                    None
                };
                // 别像以前那样只登记 LocalId：`var y` 的名字必须进作用域，
                // 否则任何使用都报 "cannot find value y"（var 语句实际上没
                // 法用）。走 lower_pattern / bind_local，和 let 同一条路。
                let hir_pat = self.lower_pattern(&_pat, &ty)?;
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
        let span = e.span();
        let kind = match e {
            Expression::Literal(lit, _) => {
                let (kind, ty) = self.check_literal(lit, expected)?;
                return Ok(HirExpr { kind, ty, span });
            }
            Expression::Variable(name, _) => {
                let id = self.resolve_name(name, span)?;
                if let Some(local) = self.as_local(id) {
                    ExprKind::Local(local)
                } else {
                    ExprKind::Def(id)
                }
            }
            Expression::Path(path, _) => {
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
                self.check_assignable(&l)?;
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
            Expression::Index(receiver, index, _) => {
                let recv = self.check_expr(receiver, &Ty::Infer(InferId(0)))?;
                // 索引表达式本身要保留下来（MIR 用它算地址）。
                let idx = self.check_expr(index, &Ty::Int(64))?;
                // 元素类型 = receiver 的元素类型。`str` 按字节索引得到 `u8`。
                let elem_ty = match self.infer.resolve(&recv.ty) {
                    Ty::Array(t, _) | Ty::Slice(t, _) => (*t).clone(),
                    Ty::Str => Ty::Uint(8),
                    Ty::Pointer(t, _) | Ty::Reference(t, _) => (*t).clone(),
                    _ => {
                        let elem = self.infer.fresh_infer(span);
                        self.infer
                            .unify(
                                &recv.ty,
                                &Ty::Pointer(Box::new(elem.clone()), hir::Mutability::Immutable),
                                span,
                            )
                            .ok();
                        elem
                    }
                };
                if matches!(self.infer.resolve(&elem_ty), Ty::Float(_)) {
                    return Err(CompileError::type_error(
                        "indexing into floating-point elements is not supported",
                    )
                    .with_span(span));
                }
                return Ok(HirExpr {
                    kind: ExprKind::Index(Box::new(recv), Box::new(idx)),
                    ty: elem_ty,
                    span,
                });
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
                let ty = inner
                    .tail
                    .as_ref()
                    .map(|t| t.ty.clone())
                    .unwrap_or(Ty::Unit);
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
                let ty = if let Some(ref el) = else_hir {
                    // 合并 then/else 类型
                    let t = then_hir
                        .tail
                        .as_ref()
                        .map(|t| t.ty.clone())
                        .unwrap_or(Ty::Unit);
                    let e = el.tail.as_ref().map(|t| t.ty.clone()).unwrap_or(Ty::Unit);
                    self.unify_or(&t, &e)
                } else {
                    Ty::Unit
                };
                // 两个分支都是发散的（都以 `return` / `throw` 结束）时，
                // `if` 本身的值是 never：它既不产生值也不正常返回。
                // 没有这一条，`fn f() -> i32 { if c { return 1; } else { return 2; } }`
                // 里的 `if` 作为尾表达式会被拿去和 i32 统一，报类型不匹配。
                let ty = if then_hir.tail.is_none()
                    && else_hir.as_ref().is_some_and(|b| b.tail.is_none())
                {
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
                let c = self.check_expr(cond, &Ty::Bool)?;
                let b = self.check_block(body, &Ty::Unit)?;
                let _else_hir = if let Some(eb) = else_b {
                    Some(self.check_block(eb, &Ty::Unit)?)
                } else {
                    None
                };
                return Ok(HirExpr {
                    kind: ExprKind::While(Box::new(c), b),
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::For(_pat, iter, body, _) => {
                let _it = self.check_expr(iter, &Ty::Infer(InferId(0)))?;
                let var = self.alloc_local();
                let var_ty = self.infer.fresh_infer(span);
                self.locals.insert(var, var_ty.clone());
                self.infer.locals.insert(var, var_ty.clone());
                self.scope_stack.last_mut().unwrap().push(var);
                let _b = self.check_block(body, &Ty::Unit)?;
                return Ok(HirExpr {
                    kind: ExprKind::For,
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::Loop(body, _) => {
                let b = self.check_block(body, &Ty::Unit)?;
                return Ok(HirExpr {
                    kind: ExprKind::Loop(b),
                    ty: Ty::Never,
                    span,
                });
            }
            Expression::Break(label, value, _) => {
                if let Some(val) = value {
                    let _ = self.check_expr(val, &Ty::Infer(InferId(0)))?;
                }
                let _ = label;
                return Ok(HirExpr {
                    kind: ExprKind::Break,
                    ty: Ty::Never,
                    span,
                });
            }
            Expression::Continue(label, _) => {
                let _ = label;
                return Ok(HirExpr {
                    kind: ExprKind::Continue,
                    ty: Ty::Never,
                    span,
                });
            }
            Expression::Return(value, _) => {
                let ret_ty = self.infer.return_ty.clone().unwrap_or(Ty::Unit);
                let v = if let Some(val) = value {
                    let e = self.check_expr(val, &ret_ty)?;
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
                let inner = self.check_expr(e, &Ty::Unit)?;
                let _ = inner;
                return Ok(HirExpr {
                    kind: ExprKind::Defer,
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::Try(e, pat, catch, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                if let Some(p) = pat {
                    let _ = self.lower_pattern(p, &inner.ty)?;
                    let _ = self.check_block(catch, &Ty::Infer(InferId(0)))?;
                }
                return Ok(HirExpr {
                    kind: ExprKind::Try,
                    ty: Ty::Infer(InferId(0)),
                    span,
                });
            }
            Expression::Throw(e, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                let _ = inner;
                return Ok(HirExpr {
                    kind: ExprKind::Throw,
                    ty: Ty::Never,
                    span,
                });
            }
            Expression::Array(elems, repeat, _) => {
                if let Some(rep) = repeat {
                    let elem = self.infer.fresh_infer(span);
                    let _ = self.check_expr(rep, &Ty::Uint(64));
                    let first = self.check_expr(&elems[0], &elem)?;
                    let _ = first;
                    return Ok(HirExpr {
                        kind: ExprKind::Array,
                        ty: Ty::Array(Box::new(elem), hir::ArrayLen::Dynamic),
                        span,
                    });
                }
                let elem = self.infer.fresh_infer(span);
                for e in elems {
                    let _ = self.check_expr(e, &elem)?;
                }
                return Ok(HirExpr {
                    kind: ExprKind::Array,
                    ty: Ty::Array(Box::new(elem), hir::ArrayLen::Dynamic),
                    span,
                });
            }
            Expression::Tuple(elems, _) => {
                let mut tys = Vec::new();
                for e in elems {
                    let h = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                    tys.push(h.ty.clone());
                }
                return Ok(HirExpr {
                    kind: ExprKind::Tuple,
                    ty: Ty::Tuple(tys),
                    span,
                });
            }
            Expression::StructInit(path, fields, _) => {
                return self.check_struct_init(path, fields, expected, span);
            }
            Expression::Lambda(params, ret, body, _) => {
                return self.check_lambda(params, ret.as_ref(), body, expected, span);
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
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                return Ok(HirExpr {
                    kind: ExprKind::Await(Box::new(inner)),
                    ty: Ty::Infer(InferId(0)),
                    span,
                });
            }
            Expression::Asm(asm, _) => {
                // 保留对 asm 操作数的类型检查（副作用）
                for o in &asm.outputs {
                    let _ = self.check_expr(&o.expr, &Ty::Infer(InferId(0)));
                }
                for i in &asm.inputs {
                    let _ = self.check_expr(&i.expr, &Ty::Infer(InferId(0)));
                }
                return Ok(HirExpr {
                    kind: ExprKind::Asm {
                        template: asm.template.clone(),
                        outputs: asm.outputs.clone(),
                        inputs: asm.inputs.clone(),
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
                let _ = inner;
                return Ok(HirExpr {
                    kind: ExprKind::Spawn,
                    ty: Ty::Infer(InferId(0)),
                    span,
                });
            }
            Expression::Labeled(_label, e, _) => {
                let inner = self.check_expr(e, expected)?;
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

    fn check_call(
        &mut self,
        callee: &Expression,
        args: &[ast::Argument],
        _expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let callee_ty = self.infer_expr_ty(callee)?;
        let callee_hir = self.check_expr(callee, &callee_ty)?;
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
            let param_ty = param_tys
                .get(i)
                .cloned()
                .unwrap_or_else(|| Ty::Infer(InferId(0)));
            hir_args.push(self.check_expr(a, &param_ty)?);
        }
        // 被调函数用一个 DefId 指明；间接调用（通过函数指针）暂不支持。
        let callee_def = match callee_hir.kind {
            ExprKind::Def(id) => id,
            _ => {
                return Err(CompileError::type_error(
                    "only direct calls to named functions are supported",
                )
                .with_span(span));
            }
        };
        Ok(HirExpr {
            kind: ExprKind::Call {
                callee: callee_def,
                args: hir_args,
                result: self.alloc_local(),
            },
            ty: ret_ty,
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
        // 或模式先在臂级展开：`3 | 4 => b` 变成两条"3 => b"、"4 => b"。
        let mut lowered: Vec<(hir::HirPattern, Option<HirExpr>, HirExpr)> = Vec::new();
        for arm in arms {
            // 先展开模式（里的绑定要在此注册），再看 guard / 臂体——
            // `v if v > 0 => v` 的两处 v 都得能解析到臂绑定。
            self.push_scope();
            let mut alts_hir: Vec<hir::HirPattern> = Vec::new();
            let mut alts_ast: Vec<&AstPattern> = Vec::new();
            push_or_alts(&arm.pattern, &mut alts_ast);
            for alt in &alts_ast {
                match self.lower_pattern(alt, &s.ty) {
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
                // 绑定臂把它绑的局部换写成 scrutinee 本身：`v if v > 0 => v`
                // 的 v 两处都替换进去。臂绑定没有"先赋值再读"的独立存储，
                // 直接内联 scrutinee 是唯一等价的降级（scrutinee 重复求值
                // 是本实现已知的限定，见下）。
                let mut g = guard.clone();
                let mut b2 = body.clone();
                if let hir::HirPattern::Binding(local) = pat {
                    if let Some(g2) = &mut g {
                        substitute_local(g2, local, &s);
                    }
                    substitute_local(&mut b2, local, &s);
                }
                lowered.push((pat, g, b2));
            }
        }
        // 模式支持面：只译出"字面量 + 通配符 + 绑定臂"。枚举 payload / tuple / 结构体
        // 模式需要聚合值表达，代码生成还没有那套——宁报编译错误，不静默算错。
        for (pat, _, _) in &lowered {
            if !matches!(
                pat,
                hir::HirPattern::Literal(_)
                    | hir::HirPattern::Wildcard
                    | hir::HirPattern::Binding(_)
            ) {
                return Err(CompileError::type_error(
                    "match on this pattern kind is not implemented yet (only literal and `_` patterns)",
                )
                .with_span(span));
            }
        }
        // 最后一条必须是"无条件匹配臂"（`_`），或穷尽 bool 的两条字面量臂：
        // if/else 链缺 else 会把"没匹配上"变成取到 Unit 继续跑——另一种静默算错。
        let last_unconditional = matches!(
            lowered.last(),
            Some((
                hir::HirPattern::Wildcard | hir::HirPattern::Binding(_),
                None,
                _
            ))
        );
        if !last_unconditional {
            // bool 的字面量覆盖（true / false 各一条）也算穷尽。
            let bool_covered = matches!(self.infer.resolve(&s.ty), Ty::Bool) && {
                let mut seen = [false, false];
                for (pat, _, _) in &lowered {
                    if let hir::HirPattern::Literal(v) = pat {
                        if *v == 0 {
                            seen[0] = true;
                        } else if *v == 1 {
                            seen[1] = true;
                        }
                    }
                }
                seen[0] && seen[1]
            };
            if !bool_covered {
                return Err(
                    CompileError::type_error("match must have a trailing `_` arm").with_span(span),
                );
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
            .map(|(_, _, b)| b.ty.clone())
            .unwrap_or(Ty::Never);
        let n = lowered.len();
        let mut chain: Option<hir::HirBlock> = None;
        for k in (0..n).rev() {
            let (pat, guard, body) = &lowered[k];
            let unconditional = guard.is_none()
                && matches!(pat, hir::HirPattern::Wildcard | hir::HirPattern::Binding(_));
            if unconditional {
                // 无条件臂就是链尾的 else；可前面的臂永不可达（首匹配胜出）。
                chain = Some(arm_body_block(body));
                continue;
            }
            let cond = match_arm_cond(&s, pat, guard, span);
            let else_block = chain.take();
            let node = hir::HirExpr {
                kind: ExprKind::If(Box::new(cond), arm_body_block(body), else_block),
                ty: ty.clone(),
                span,
            };
            chain = Some(hir::HirBlock {
                stmts: Vec::new(),
                tail: Some(Box::new(node)),
            });
        }
        match chain {
            Some(mut last) => match last.tail.take() {
                Some(node) => Ok(*node),
                // 整条链就是单个有返回块的臂体（如 `_ => { return 3; }`）。
                None => Ok(hir::HirExpr {
                    kind: ExprKind::Block(last),
                    ty,
                    span,
                }),
            },
            None => Ok(hir::HirExpr {
                kind: ExprKind::Match,
                ty,
                span,
            }),
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
        _expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let def_id = self.resolve_path(path, span)?;
        let field_tys: Vec<(String, Ty)> = match self.type_defs.get(&def_id) {
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
            checked.push((name.clone(), checked_expr));
        }
        // 检查是否有未初始化的字段（结构体字面量要求全部初始化）
        for (name, _) in &field_tys {
            if !seen.contains(name) {
                return Err(CompileError::type_error(format!(
                    "missing field `{}` in struct literal for `{}`",
                    name,
                    path.join("::")
                ))
                .with_span(span));
            }
        }
        Ok(HirExpr {
            kind: ExprKind::Struct(checked),
            ty: Ty::Named(def_id, Vec::new()),
            span,
        })
    }

    fn check_field_access(&self, ty: &Ty, name: &str, span: ast::Span) -> Result<(Ty, FieldIndex)> {
        match ty {
            Ty::Named(id, _) => {
                if let Some(hir_td) = self.type_defs.get(id) {
                    if let hir::HirTypeDef::Struct(s) = hir_td {
                        for f in &s.fields {
                            if f.name == name {
                                return Ok((f.ty.clone(), FieldIndex::Named(f.index)));
                            }
                        }
                    }
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
                let ty = match suffix.as_deref().and_then(int_suffix_ty) {
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
                let _ = mutability;
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
                let _ = hir_pats?;
                Ok(hir::HirPattern::Tuple)
            }
            AstPattern::Struct {
                path,
                fields,
                rest,
                span,
            } => {
                let def_id = self.resolve_path(path, *span)?;
                let hir_fields: Result<Vec<_>> = fields
                    .iter()
                    .map(|fp| {
                        let fty = self.field_type(def_id, &fp.name, *span)?;
                        Ok((0, self.lower_pattern(&fp.pattern, &fty)?))
                    })
                    .collect();
                let _ = hir_fields?;
                let _ = rest;
                Ok(hir::HirPattern::Struct)
            }
            AstPattern::Variant { path, args, span } => {
                let def_id = self.resolve_path(&path[..path.len() - 1], *span)?;
                let variant_name = path.last().unwrap();
                let variant_idx = self.variant_index(def_id, variant_name);
                let payload_tys = self.variant_payload_tys(def_id, variant_idx, *span)?;
                let hir_args: Result<Vec<_>> = args
                    .iter()
                    .enumerate()
                    .map(|(i, p)| self.lower_pattern(p, payload_tys.get(i).unwrap_or(ty)))
                    .collect();
                let _ = hir_args?;
                Ok(hir::HirPattern::Variant)
            }
            AstPattern::Slice(pats, _) => {
                let _ = pats
                    .iter()
                    .map(|p| self.lower_pattern(p, ty))
                    .collect::<Result<Vec<_>>>()?;
                Ok(hir::HirPattern::Slice)
            }
            AstPattern::Range(start, end, inclusive, _) => {
                let s = self.check_expr(start, &Ty::Infer(InferId(0)))?;
                let en = self.check_expr(end, &Ty::Infer(InferId(0)))?;
                let _ = (s, en, inclusive);
                Ok(hir::HirPattern::Range)
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
        if let Some(hir_td) = self.type_defs.get(&def_id) {
            if let hir::HirTypeDef::Struct(s) = hir_td {
                if let Some(f) = s.fields.iter().find(|f| f.name == name) {
                    return Ok(f.ty.clone());
                }
            }
        }
        Err(CompileError::name_error(format!("no field `{}`", name)).with_span(span))
    }

    fn variant_index(&self, _def_id: DefId, _name: &str) -> usize {
        0
    }

    fn variant_payload_tys(
        &self,
        _def_id: DefId,
        _idx: usize,
        _span: ast::Span,
    ) -> Result<Vec<Ty>> {
        Ok(Vec::new())
    }

    // ==================================================================
    // 闭包
    // ==================================================================

    fn check_lambda(
        &mut self,
        params: &[AstPattern],
        ret: Option<&AstType>,
        body: &AstBlock,
        _expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        self.push_scope();
        let mut hir_params = Vec::new();
        for p in params {
            let ty = self.infer.fresh_infer(span);
            let local = self.alloc_local();
            self.locals.insert(local, ty.clone());
            self.infer.locals.insert(local, ty.clone());
            self.scope_stack.last_mut().unwrap().push(local);
            hir_params.push((self.lower_pattern(p, &ty)?, ty.clone()));
        }
        let ret_ty = ret
            .as_ref()
            .map(|t| self.ast_to_hir_type(t))
            .unwrap_or_else(|| Ok(self.infer.fresh_infer(span)))?;
        let _ = self.check_block(body, &ret_ty)?;
        self.pop_scope();
        let fn_ty = Ty::Function {
            params: hir_params.iter().map(|(_, t)| t.clone()).collect(),
            ret: Box::new(ret_ty),
            is_async: false,
            abi: Abi::Zr,
        };
        Ok(HirExpr {
            kind: ExprKind::Closure,
            ty: fn_ty,
            span,
        })
    }

    fn analyze_closures(&self, _functions: &[hir::HirFunction]) -> Result<()> {
        // 为每个闭包确定捕获列表，并生成对应的匿名结构体
        Ok(())
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
        self.def_names.insert(id, name);
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
        let first = path[0].clone();
        let id = self.resolve_name(&first, span)?;
        for seg in &path[1..] {
            // 路径继续：查找 id 的成员
            if let Some(hir_td) = self.type_defs.get(&id) {
                if let hir::HirTypeDef::Struct(s) = hir_td {
                    if let Some(f) = s.fields.iter().find(|f| f.name == *seg) {
                        // 字段不是 DefId，这里仅作占位
                        let _ = f;
                    }
                }
            }
            let _ = seg;
        }
        Ok(id)
    }

    fn resolve_type_path(&self, path: &[String]) -> Result<DefId> {
        if path.len() == 1 {
            let name = &path[0];
            if let Some(w) = parse_int_type(name) {
                return Ok(DefId(w as u32));
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
                // 特殊处理 Pin<T>
                if path.len() == 1 && path[0] == "Pin" && generics.len() == 1 {
                    return Ok(Ty::Pin(Box::new(self.ast_to_hir_type(&generics[0])?)));
                }
                let id = self.resolve_type_path(path)?;
                Ty::Named(
                    id,
                    generics
                        .iter()
                        .map(|g| self.ast_to_hir_type(g))
                        .collect::<Result<_>>()?,
                )
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
        })
    }

    fn infer_expr_ty(&mut self, e: &Expression) -> Result<Ty> {
        // 快速类型推断（用于递归调用，避免无限递归）
        Ok(match e {
            Expression::Literal(AstLiteral::Int(_, suffix), _) => suffix
                .as_deref()
                .and_then(int_suffix_ty)
                .unwrap_or(Ty::Int(32)),
            Expression::Literal(AstLiteral::Float(_, suffix), _) => match suffix.as_deref() {
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
            Expression::Binary(_, op, _, _) => match op {
                ast::BinaryOp::Eq
                | ast::BinaryOp::Ne
                | ast::BinaryOp::Lt
                | ast::BinaryOp::Gt
                | ast::BinaryOp::Le
                | ast::BinaryOp::Ge
                | ast::BinaryOp::And
                | ast::BinaryOp::Or => Ty::Bool,
                _ => self.infer.fresh_infer(e.span()),
            },
            Expression::Unary(op, _, _) => match op {
                ast::UnaryOp::Not => Ty::Bool,
                _ => self.infer.fresh_infer(e.span()),
            },
            // `x as T` 的结果类型就是 T，不需要推断。
            //
            // 漏掉这一条会让 `let y = x as f64;` 的 y 落到一个未解决的
            // 推断变量上：后续 `y as i32` 直接报 "cannot cast `_` to i32"。
            Expression::Cast(_, target, _) => self.ast_to_hir_type(target)?,
            Expression::Call(callee, _, _) => {
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
            Expression::StructInit(path, _, _) => {
                let id = self
                    .resolve_path(path, ast::Span::unknown())
                    .unwrap_or(DefId(0));
                Ty::Named(id, Vec::new())
            }
            Expression::Tuple(elems, _) => Ty::Tuple(
                elems
                    .iter()
                    .map(|e| self.infer_expr_ty(e))
                    .collect::<Result<_>>()?,
            ),
            Expression::Array(elems, _, _) => {
                let elem = if let Some(first) = elems.first() {
                    self.infer_expr_ty(first)?
                } else {
                    self.infer.fresh_infer(e.span())
                };
                Ty::Array(Box::new(elem), hir::ArrayLen::Dynamic)
            }
            Expression::Lambda(_, ret, _, _) => {
                let params = Vec::new();
                let ret_ty = ret
                    .as_ref()
                    .map(|t| self.ast_to_hir_type(t))
                    .unwrap_or_else(|| Ok(self.infer.fresh_infer(e.span())))?;
                Ty::Function {
                    params,
                    ret: Box::new(ret_ty),
                    is_async: false,
                    abi: Abi::Zr,
                }
            }
            Expression::Await(_, _) => self.infer.fresh_infer(e.span()),
            Expression::Yield(_, _) => self.infer.fresh_infer(e.span()),
            Expression::Spawn(_, _) => self.infer.fresh_infer(e.span()),
            _ => self.infer.fresh_infer(e.span()),
        })
    }

    fn validate_cast(&self, from: &Ty, to: &Ty, span: ast::Span) -> Result<()> {
        match (from, to) {
            // 数值类型之间可以 cast
            (Ty::Int(_), Ty::Int(_))
            | (Ty::Int(_), Ty::Uint(_))
            | (Ty::Int(_), Ty::Float(_))
            | (Ty::Uint(_), Ty::Int(_))
            | (Ty::Uint(_), Ty::Uint(_))
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

            // `comptime Point { x: 1, y: 2 }`：字段按书写顺序求值。
            Expression::StructInit(_, fields, _) => {
                let mut items = Vec::with_capacity(fields.len());
                for (name, e) in fields {
                    items.push((name.clone(), *self.eval_const_expr(e)?));
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

    fn unify_or(&self, a: &Ty, b: &Ty) -> Ty {
        if a == b {
            a.clone()
        } else {
            Ty::Infer(InferId(0))
        }
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
    }
}

/// 取模式中第一个绑定变量的 `LocalId`。
fn first_binding(p: &hir::HirPattern) -> Option<LocalId> {
    match p {
        hir::HirPattern::Binding(local) => Some(*local),
        _ => None,
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
        ExprKind::Unary(_, v)
        | ExprKind::Assign(v, _)
        | ExprKind::Field(v, _)
        | ExprKind::Deref(v)
        | ExprKind::AddrOf(v)
        | ExprKind::Await(v)
        | ExprKind::Cast(v) => substitute_local(v, target, with),
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
        ExprKind::If(cond, then, els) => {
            substitute_local(cond, target, with);
            walk_block(then, target, with);
            if let Some(els) = els {
                walk_block(els, target, with);
            }
        }
        ExprKind::While(cond, body) => {
            substitute_local(cond, target, with);
            walk_block(body, target, with);
        }
        ExprKind::Loop(body) => walk_block(body, target, with),
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
        | ExprKind::For
        | ExprKind::Break
        | ExprKind::Continue
        | ExprKind::Defer
        | ExprKind::Try
        | ExprKind::Throw
        | ExprKind::Array
        | ExprKind::Tuple
        | ExprKind::Closure
        | ExprKind::Asm
        | ExprKind::Yield
        | ExprKind::Spawn
        | ExprKind::Intrinsic => {}
    }
}

/// `substitute_local` 的块部分（stmts 里的 Expr 语句 + tail）。
fn walk_block(b: &mut hir::HirBlock, target: LocalId, with: &hir::HirExpr) {
    for stmt in &mut b.stmts {
        if let HirStmt::Expr(expr) = stmt {
            substitute_local(expr, target, with);
        }
    }
    if let Some(tail) = &mut b.tail {
        substitute_local(tail, target, with);
    }
}

/// match 臂的分支条件：`scrutinee == 字面量`，有 guard 再与它对半。
///
/// 字面量的 HIR 形态按 scrutinee 的类型回填（bool → Bool 字面量、char → Char），
/// 否则 `match b { 1 => .., }` 对 bool scrutinee 会比较错类型的字面量。
fn match_arm_cond(
    scrutinee: &HirExpr,
    pat: &hir::HirPattern,
    guard: &Option<HirExpr>,
    span: ast::Span,
) -> HirExpr {
    // 字面量臂：条件是 ` scrutinee == 字面量`；`_`/绑定臂无条件匹配——
    // 条件就是 guard（没 guard 时调用方已经把它当链尾处理，这里也造恒真）。
    let unpick = match pat {
        hir::HirPattern::Literal(v) => Some(*v),
        _ => None,
    };
    let mut cond = match unpick {
        Some(val) => {
            let lit = hir::HirExpr {
                kind: ExprKind::Literal(match &scrutinee.ty {
                    Ty::Bool => hir::HirLiteral::Bool(val != 0),
                    Ty::Char => hir::HirLiteral::Char(
                        u32::try_from(val)
                            .ok()
                            .and_then(char::from_u32)
                            .unwrap_or('\0'),
                    ),
                    _ => hir::HirLiteral::Int(val),
                }),
                ty: scrutinee.ty.clone(),
                span,
            };
            hir::HirExpr {
                kind: ExprKind::Binary(
                    hir::BinaryOp::Eq,
                    Box::new(scrutinee.clone()),
                    Box::new(lit),
                ),
                ty: Ty::Bool,
                span,
            }
        }
        None => hir::HirExpr {
            kind: ExprKind::Literal(hir::HirLiteral::Bool(true)),
            ty: Ty::Bool,
            span,
        },
    };
    if let Some(g) = guard {
        cond = hir::HirExpr {
            kind: ExprKind::Binary(hir::BinaryOp::And, Box::new(cond), Box::new(g.clone())),
            ty: Ty::Bool,
            span,
        };
    }
    cond
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
