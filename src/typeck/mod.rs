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

/// 把数组长度写成一个 HIR 整数字面量，便于类型统一与打印。
fn const_array_len(n: i128, span: Span) -> hir::ArrayLen {
    hir::ArrayLen::Const(Box::new(HirExpr {
        kind: ExprKind::Literal(HirLiteral::Int(n)),
        ty: Ty::Uint(64),
        span,
    }))
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
    /// 闭包还没有运行期表示：捕获分析与匿名结构体的生成都没做，MIR 侧只能
    /// 把闭包降成 unit——调用它就是拿 unit 当函数指针，跑起来直接崩。这里
    /// 先记下来，等整个程序检查完再统一报错（这样闭包**里面**的类型错误
    /// 仍然会先报出来）。
    closure_span: Option<ast::Span>,
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
            pointer_bits: 64,
            symbols: HashMap::new(),
            def_names: HashMap::new(),
            type_defs: HashMap::new(),
            functions: HashMap::new(),
            async_outputs: HashMap::new(),
            trait_defs: HashMap::new(),
            impl_blocks: Vec::new(),
            impl_targets: HashMap::new(),
            drop_impls: HashMap::new(),
            consts: HashMap::new(),
            statics: HashMap::new(),
            locals: HashMap::new(),
            mutable_locals: HashSet::new(),
            type_params: Vec::new(),
            impl_generics: HashMap::new(),
            generic_fn_span: None,
            closure_span: None,
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

    /// 设置目标指针位宽，供 `usize` / `isize` 解析使用。
    pub fn set_pointer_bits(&mut self, bits: u16) {
        self.pointer_bits = bits;
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
        // impl 方法先展开成普通函数（名字 mangle 成 `Type__method`），这样
        // 现有签名/函数体检查、MIR 降级都能复用同一张函数表。
        let mut expanded = prog.clone();
        for ib in &prog.impls {
            let Some(target_name) = ast_type_name(&ib.target) else {
                continue;
            };
            for method in &ib.methods {
                let mut m = method.clone();
                m.name = format!("{target_name}__{}", method.name);
                self.impl_targets.insert(m.name.clone(), ib.target.clone());
                self.impl_generics.insert(m.name.clone(), ib.generics.clone());
                expanded.functions.push(m);
            }
        }
        let prog = &expanded;

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
        //
        // 捕获分析（哪些外部变量被捕获）与匿名结构体的生成都还没做，MIR 侧
        // 只能把闭包降成 unit——调用它就是拿 unit 当函数指针，跑起来直接崩。
        // 这里明确报错，不编出会崩的程序。
        if let Some(span) = self.closure_span {
            return Err(CompileError::type_error(
                "closures are not implemented yet (captures are not lowered)",
            )
            .with_span(span));
        }
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
            // trait 里的方法也要登记成定义——后面按名字取它们的签名。
            //
            // 名字带 trait 前缀，免得两个 trait 的同名方法（`Eq::eq` 与
            // `PartialEq::eq`）撞成同一个定义。
            for m in &t.methods {
                let mid = self.alloc_def(format!("{}::{}", t.name, m.name));
                self.ast_fns.insert(format!("{}::{}", t.name, m.name), m.clone());
                self.functions.insert(mid, self.fn_to_hir(m, mid));
            }
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
        // 泛型参数先绑上：字段里的 `T` 是类型变量。
        self.push_type_params(&s.generics, ast::Span::unknown());
        let def_id = self.find_def(&s.name)?;
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
        Ok(())
    }

    fn check_enum_def(&mut self, e: &ast::EnumDef) -> Result<()> {
        // 泛型参数先绑上：变体载荷里的 `T` 是类型变量。
        self.push_type_params(&e.generics, ast::Span::unknown());
        let def_id = self.find_def(&e.name)?;
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
            // 载荷的字段必须是标量。
            //
            // 枚举的载荷是**按值内联**在判别式后面的，字段的类型若是聚合
            // （结构体 / 元组 / 数组 / 枚举），它的值在运行期是"指向存储的
            // 地址"——把地址内联进枚举里，指的东西随时会失效。
            for ty in &payload_tys {
                if false && self.is_aggregate_ty(ty) {
                    return Err(CompileError::type_error(
                        "enum variant payloads must be scalars \
                         (aggregate types are address-represented and cannot be inlined)",
                    )
                    .with_span(ast::Span::unknown()));
                }
            }
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
                continue;
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
            if let Some(hs) = self.statics.get_mut(&id) {
                hs.ty = ty;
                *hs.init = init;
            }
        }
        Ok(())
    }

    fn check_function_signatures(&mut self, prog: &Program) -> Result<()> {
        for f in &prog.functions {
            let id = self.find_def(&f.name)?;
            // 签名里的泛型参数 / `Self` 要先绑上——它们是类型变量。
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
        for f in &prog.functions {
            let id = self.find_def(&f.name)?;
            // 泛型函数的体跳过：类型参数是推断变量，按它检查只会报一堆
            // 假错。函数体留空，用到它时再报"未实现"。
            if !f.generics.is_empty() {
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
                    self.check_partial_move(&h, e.span())?;
                    self.infer
                        .unify(&h.ty, &ty, e.span())
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
                // 无载荷的枚举变体可以直接当值用：`Shape::Circle`。
                if let Some((def_id, index)) = self.enum_variant_target_path(path, span) {
                    return self.check_enum_variant_init(def_id, index, &[], span);
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
            Expression::For(pat, iter, body, _) => {
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
                    let body_hir = self.check_block(body, &Ty::Unit)?;
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
                        kind: ExprKind::While(Box::new(cond), loop_body),
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
                    kind: ExprKind::While(Box::new(cond), loop_body),
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
                let elem = self.infer.fresh_infer(span);
                // 声明类型（`let b: [u8; 4] = [1, 2, ..]`）先定下元素类型，
                // 否则字面量的默认类型（i32）会顶掉它。
                if let Ty::Array(decl_elem, _) = expected {
                    let _ = self.infer.unify(&elem, decl_elem, span);
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
                for e in elems {
                    let h = self.check_expr(e, &Ty::Infer(InferId(0)))?;
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
                let ty = {
                    let t = then_hir
                        .tail
                        .as_ref()
                        .map(|t| t.ty.clone())
                        .unwrap_or(Ty::Unit);
                    let e = else_hir
                        .as_ref()
                        .and_then(|b| b.tail.as_ref())
                        .map(|t| t.ty.clone())
                        .unwrap_or(Ty::Unit);
                    self.unify_or(&t, &e)
                };
                let ty = if then_hir.tail.is_none()
                    && else_hir.as_ref().is_some_and(|b| b.tail.is_none())
                {
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

    /// 若 `callee` 是元组结构体类型名，返回其 DefId。
    fn tuple_struct_target(&self, callee: &Expression) -> Option<DefId> {
        let id = match callee {
            Expression::Variable(name, _) => self.symbols.get(name).copied()?,
            Expression::Path(path, _) if path.len() == 1 => self.symbols.get(&path[0]).copied()?,
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
        let ast::Expression::Path(path, span) = callee else {
            return None;
        };
        self.enum_variant_target_path(path, *span)
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
        span: ast::Span,
    ) -> Result<HirExpr> {
        let field_tys = self.variant_field_types(def_id, index).ok_or_else(|| {
            CompileError::internal("enum variant missing type definition").with_span(span)
        })?;
        if args.len() != field_tys.len() {
            return Err(CompileError::type_error(format!(
                "enum variant constructor expects {} arguments, found {}",
                field_tys.len(),
                args.len()
            ))
            .with_span(span));
        }
        let mut hir_args = Vec::with_capacity(args.len());
        for (arg, ty) in args.iter().zip(&field_tys) {
            let ast::Argument::Pos(expr) = arg else {
                return Err(CompileError::type_error(
                    "enum variant constructor does not accept named arguments",
                )
                .with_span(span));
            };
            let checked = self.check_expr(expr, ty)?;
            self.check_partial_move(&checked, expr.span())?;
            hir_args.push(checked);
        }
        Ok(HirExpr {
            kind: ExprKind::EnumVariant {
                def_id,
                index,
                args: hir_args,
            },
            ty: Ty::Named(def_id, Vec::new()),
            span,
        })
    }

    fn check_tuple_struct_init(
        &mut self,
        type_id: DefId,
        args: &[ast::Argument],
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
        let mut fields = Vec::with_capacity(args.len());
        for (field, arg) in s.fields.iter().zip(args) {
            let ast::Argument::Pos(expr) = arg else {
                return Err(CompileError::type_error(
                    "tuple struct constructor does not accept named arguments",
                )
                .with_span(span));
            };
            let arg = self.check_expr(expr, &field.ty)?;
            self.check_partial_move(&arg, expr.span())?;
            fields.push((field.name.clone(), arg));
        }
        Ok(HirExpr {
            kind: ExprKind::Struct(fields),
            ty: Ty::Named(type_id, Vec::new()),
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
            pats[idx] = Some(self.lower_pattern(&fp.pattern, &var_fields[idx].ty)?);
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
            let checked = self.check_expr(expr, &var_fields[idx].ty)?;
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
            ty: Ty::Named(enum_id, Vec::new()),
            span,
        })
    }

    /// 查字段（不报错）：用于区分 `expr.name(...)` 是字段函数指针调用
    /// 还是 `impl` 方法调用。引用/指针自动穿透。
    fn lookup_field(&self, ty: &Ty, name: &str) -> Option<(Ty, FieldIndex)> {
        match ty {
            Ty::Named(id, _) => {
                let hir::HirTypeDef::Struct(s) = self.type_defs.get(id)? else {
                    return None;
                };
                s.fields
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| (f.ty.clone(), FieldIndex::Named(f.index)))
            }
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
        let Ty::Named(id, _) = ty else {
            if let Ty::Reference(inner, _) | Ty::Pointer(inner, _) = ty {
                return self.lookup_method(inner, name);
            }
            return None;
        };
        let target = self.def_names.get(id)?;
        let mangled = format!("{target}__{name}");
        if !self.impl_targets.contains_key(&mangled) {
            return None;
        }
        let method = self.symbols.get(&mangled).copied()?;
        self.functions.get(&method)?;
        Some(method)
    }

    fn check_method_call(
        &mut self,
        method: hir::DefId,
        receiver: HirExpr,
        args: &[ast::Argument],
        span: ast::Span,
    ) -> Result<HirExpr> {
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

    fn check_call(
        &mut self,
        callee: &Expression,
        args: &[ast::Argument],
        _expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        // 元组结构体构造：`Point(1, 2)` 语法上是把类型名当函数调用。
        if let Some(type_id) = self.tuple_struct_target(callee) {
            return self.check_tuple_struct_init(type_id, args, span);
        }

        // 枚举变体构造：`Shape::Circle(5)` 同样是"把变体名当函数调用"。
        if let Some((def_id, index)) = self.enum_variant_target(callee) {
            return self.check_enum_variant_init(def_id, index, args, span);
        }

        // `expr.method(args)` 在语法上就是 `Call(Field(expr, method), args)`。
        // 字段优先：`ops.f(1, 2)` 里的 `f` 是函数指针字段；字段不存在时才
        // 到 `impl` 方法表里找 `Type__method`。
        let mut prepared_callee = None;
        if let Expression::Field(receiver, name, field_span) = callee {
            let recv_ty = self.infer_expr_ty(receiver)?;
            let recv = self.check_expr(receiver, &recv_ty)?;
            let recv_ty = self.infer.resolve(&recv.ty);
            if self.lookup_field(&recv_ty, name).is_none() {
                if let Some(method_id) = self.lookup_method(&recv_ty, name) {
                    return self.check_method_call(method_id, recv, args, span);
                }
            } else if let Some((ty, _)) = self.lookup_field(&recv_ty, name) {
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
            let param_ty = param_tys.get(i).cloned().unwrap_or(Ty::Infer(InferId(0)));
            let arg = self.check_expr(a, &param_ty)?;
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
                // 模式的条件与绑定替换。绑定臂把它绑的局部换写成 scrutinee
                // 本身：`v if v > 0 => v` 的 v 两处都替换进去。结构体模式的
                // 绑定落在字段上（`P { x: v }` 的 v 是 p.x），逐个换写。
                // 臂绑定没有"先赋值再读"的独立存储，直接内联取值表达式是
                // 唯一等价的降级（scrutinee 重复求值是本实现已知的限定，
                // 见下）。
                let (cond, binds) = self.pat_match(&s, &pat, span)?;
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
        let last_unconditional = matches!(
            lowered.last(),
            Some(arm) if arm.cond.is_none() && arm.guard.is_none() && pat_irrefutable(&arm.pattern)
        );
        if !last_unconditional {
            // bool 的字面量覆盖（true / false 各一条）也算穷尽。
            let bool_covered = matches!(self.infer.resolve(&s.ty), Ty::Bool) && {
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
            let scrutinee_ty = match &s.ty {
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
            .map(|arm| arm.body.ty.clone())
            .unwrap_or(Ty::Never);
        let n = lowered.len();
        let mut chain: Option<hir::HirBlock> = None;
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
                let def_id = match &scrutinee.ty {
                    Ty::Named(id, _) => *id,
                    Ty::Reference(inner, _) | Ty::Pointer(inner, _) => match inner.as_ref() {
                        Ty::Named(id, _) => *id,
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
                let field_tys = self.variant_field_types(def_id, *index).unwrap_or_default();
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
        _expected: &Ty,
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
                        enum_id, path, fields, base, span,
                    );
                }
            }
        }
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
        let target_ty = Ty::Named(def_id, Vec::new());
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

    fn check_field_access(&self, ty: &Ty, name: &str, span: ast::Span) -> Result<(Ty, FieldIndex)> {
        match ty {
            Ty::Named(id, _) => {
                if let Some(hir::HirTypeDef::Struct(s)) = self.type_defs.get(id) {
                    for f in &s.fields {
                        if f.name == name {
                            return Ok((f.ty.clone(), FieldIndex::Named(f.index)));
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
            Ty::Str | Ty::Slice(..) => match name {
                "ptr" => Ok((Ty::Pointer(Box::new(Ty::Uint(8)), hir::Mutability::Immutable), FieldIndex::Named(0))),
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
                                enum_id, path, fields, *rest, *span,
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
                let def_id = self.resolve_path(&path[..path.len() - 1], *span)?;
                let variant_name = path.last().unwrap();
                let variant_idx = self.variant_index(def_id, variant_name);
                let payload_tys = self.variant_payload_tys(def_id, variant_idx, *span)?;
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

    /// 类型是不是"聚合"（运行期用地址表示）。
    ///
    /// 结构体、元组、数组、枚举的值在运行期都是"指向存储的地址"，不能按值
    /// 内联进别的聚合里。
    fn is_aggregate_ty(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Tuple(_) | Ty::Array(..) | Ty::Slice(..) => true,
            Ty::Named(id, _) => matches!(
                self.type_defs.get(id),
                Some(hir::HirTypeDef::Struct(_) | hir::HirTypeDef::Enum(_))
            ),
            _ => false,
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

    /// 绑定一组泛型参数（类型参数 → 新的推断变量）。
    ///
    /// 泛型还没有单态化，声明里的类型参数只能当成推断变量：声明本身能过
    /// 类型检查，但按它布局 / 生成代码是不对的（`Option<T>` 的大小取决于
    /// `T`）。用到泛型的地方会另行报错。
    fn push_type_params(&mut self, generics: &[ast::GenericParam], span: ast::Span) {
        let mut scope = HashMap::new();
        for g in generics {
            if let ast::GenericParam::Type(name) = g {
                let ty = self.infer.fresh_infer(span);
                scope.insert(name.clone(), ty);
            }
        }
        self.type_params.push(scope);
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
    /// 类型照常检查（闭包体里的类型错误要先报出来），但闭包本身还没有运行期
    /// 表示：捕获分析与匿名结构体的生成都没做，MIR 侧只能把闭包降成 unit
    /// ——调用它就是拿 unit 当函数指针，跑起来直接崩。所以记下位置，等整个
    /// 程序检查完再统一报错。
    fn check_lambda(
        &mut self,
        params: &[ast::LambdaParam],
        ret: Option<&AstType>,
        body: &AstBlock,
        _expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
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
        let _ = self.check_block(body, &ret_ty)?;
        self.pop_scope();
        let fn_ty = Ty::Function {
            params: hir_params.iter().map(|(_, t)| t.clone()).collect(),
            ret: Box::new(ret_ty),
            is_async: false,
            abi: Abi::Zr,
        };
        if self.closure_span.is_none() {
            self.closure_span = Some(span);
        }
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
                Ty::Named(id, Vec::new())
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

/// 语法类型里最末一级名字（`a::b::Foo` → `Foo`）。
fn ast_type_name(ty: &AstType) -> Option<&str> {
    match ty {
        AstType::Named { path, .. } => path.last().map(String::as_str),
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
        ExprKind::Continue if depth == 0 => {
            let cont = HirExpr {
                kind: ExprKind::Continue,
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
        ExprKind::While(c, b) => {
            rewrite_for_continue_expr(c, inc, depth + 1);
            rewrite_for_continue(b, inc, depth + 1);
        }
        ExprKind::Loop(b) => rewrite_for_continue(b, inc, depth + 1),
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
        | ExprKind::Cast(v)
        | ExprKind::EnumField { value: v, .. }
        | ExprKind::EnumTag(v)
        | ExprKind::Spawn(v) => substitute_local(v, target, with),
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
        | ExprKind::Break
        | ExprKind::Continue
        | ExprKind::Defer
        | ExprKind::Try
        | ExprKind::Throw
        | ExprKind::Array(..)
        | ExprKind::Tuple(..)
        | ExprKind::Closure
        | ExprKind::Asm { .. }
        | ExprKind::Yield
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
