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

use crate::error::{CompileError, Result};
use crate::parser::ast::{
    self, Abi, Arm, AttributeArg, Block as AstBlock, Expression, Literal as AstLiteral,
    Pattern as AstPattern, Program, Spanned, Span, Statement, StructDef as AstStruct,
    Type as AstType, UseTree,
};
use crate::typeck::hir::{
    BasicBlock, BasicBlockId, BreakValue, Capture, ClosureExpr, Cfg, DefId,
    ExprKind, FieldIndex, ForLoop, ForProtocol, HirArm, HirExpr, HirField, HirStmt,
    InferId, LocalId, Ty, WherePred,
};
use crate::typeck::infer::InferCtxt;

/// 类型检查器。
#[derive(Debug)]
pub struct TypeChecker {
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
    /// 当前函数 ID（用于 return 检查）。
    current_function: Option<DefId>,
    /// 错误集合（延迟报告，允许错误恢复）。
    errors: Vec<CompileError>,
    /// 导入解析缓存。
    use_aliases: HashMap<String, DefId>,
}

impl Default for TypeChecker {
    fn default() -> Self {
        // InferCtxt 借用 TypeChecker 的字段，无法在构造时直接填入。
        // 先用 empty() 占位（指向 'static 空集合），实际检查时再按需重建。
        Self {
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
            current_function: None,
            errors: Vec::new(),
            use_aliases: HashMap::new(),
        }
    }
}

impl TypeChecker {
    /// 创建类型检查器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 执行完整类型检查，返回 HIR 模块。
    pub fn check_program(&mut self, prog: &Program) -> Result<hir::HirModule> {
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

        if !self.errors.is_empty() {
            // 报告第一个错误
            return Err(self.errors.remove(0));
        }

        Ok(hir::HirModule {
            functions,
            type_defs: self.type_defs.values().cloned().collect(),
            consts: self.consts.values().cloned().collect(),
            statics: self.statics.values().cloned().collect(),
            trait_defs: self.trait_defs.values().cloned().collect(),
            impl_blocks: self.impl_blocks.clone(),
        })
    }

    // ==================================================================
    // 阶段 1：声明收集
    // ==================================================================

    fn collect_declarations(&mut self, prog: &Program) -> Result<()> {
        for item in &prog.functions {
            let id = self.alloc_def(item.name.clone());
            self.functions.insert(
                id,
                hir::HirFunction {
                    id,
                    name: item.name.clone(),
                    visibility: item.visibility,
                    attrs: item.attrs.clone(),
                    is_async: item.is_async,
                    is_noreturn: item.is_noreturn,
                    is_const: item.is_const,
                    is_default: item.is_default,
                    abi: item.abi.clone(),
                    generics: item.generics.clone(),
                    params: Vec::new(),
                    receiver: None,
                    return_type: Ty::Infer(InferId(0)),
                    body: None,
                    span: item.span,
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
                    self.type_defs.insert(id, hir::HirTypeDef::Struct(hir::HirStruct {
                        def_id: id,
                        name: s.name.clone(),
                        visibility: s.visibility,
                        generics: s.generics.clone(),
                        attrs: s.attrs.clone(),
                        fields: Vec::new(),
                        repr: hir::Repr::Rust,
                        span: s.span,
                    }));
                }
                ast::TypeDef::Enum(e) => {
                    self.type_defs.insert(id, hir::HirTypeDef::Enum(hir::HirEnum {
                        def_id: id,
                        name: e.name.clone(),
                        visibility: e.visibility,
                        generics: e.generics.clone(),
                        repr: hir::IntegerRepr::Auto,
                        variants: Vec::new(),
                        span: e.span,
                    }));
                }
                ast::TypeDef::Union(u) => {
                    self.type_defs.insert(id, hir::HirTypeDef::Union(hir::HirUnion {
                        def_id: id,
                        name: u.name.clone(),
                        visibility: u.visibility,
                        generics: u.generics.clone(),
                        fields: Vec::new(),
                        span: u.span,
                    }));
                }
                ast::TypeDef::Alias { name: _, target, visibility, span, .. } => {
                    let target_ty = self.ast_to_hir_type(target)?;
                    self.type_defs.insert(
                        id,
                        hir::HirTypeDef::Alias {
                            def_id: id,
                            target: target_ty,
                            visibility: *visibility,
                            span: *span,
                        },
                    );
                }
            }
        }
        for t in &prog.traits {
            let id = self.alloc_def(t.name.clone());
            let (is_marker, is_auto) = classify_trait(t);
            self.trait_defs.insert(
                id,
                hir::HirTrait {
                    def_id: id,
                    name: t.name.clone(),
                    visibility: t.visibility,
                    generics: t.generics.clone(),
                    super_traits: Vec::new(),
                    assoc_types: Vec::new(),
                    methods: Vec::new(),
                    is_marker,
                    is_auto,
                    span: t.span,
                },
            );
        }
        for c in &prog.consts {
            let id = self.alloc_def(c.name.clone());
            self.consts.insert(
                id,
                hir::HirConst {
                    def_id: id,
                    name: c.name.clone(),
                    visibility: c.visibility,
                    ty: Ty::Infer(InferId(0)),
                    value: Box::new(self.dummy_expr()),
                    span: c.span,
                },
            );
        }
        for s in &prog.statics {
            let id = self.alloc_def(s.name.clone());
            self.statics.insert(
                id,
                hir::HirStatic {
                    def_id: id,
                    name: s.name.clone(),
                    visibility: s.visibility,
                    mutable: s.mutable,
                    ty: Ty::Infer(InferId(0)),
                    init: Box::new(self.dummy_expr()),
                    span: s.span,
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
                if let Some(id) = self.symbols.get(name).copied() {
                    self.use_aliases.insert(name.clone(), id);
                } else {
                    self.errors.push(CompileError::name_error(format!(
                        "cannot resolve `{}` in import",
                        name
                    ))
                    .with_span(*span));
                }
            }
            UseTree::Rename(inner, alias) => {
                if let UseTree::Name(name) = inner.as_ref() {
                    if let Some(id) = self.symbols.get(name).copied() {
                        self.use_aliases.insert(alias.clone(), id);
                    } else {
                        self.errors.push(CompileError::name_error(format!(
                            "cannot resolve `{}` in import",
                            name
                        ))
                        .with_span(*span));
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
            let offset = offsets.alloc(self.infer.size_of(&ty), self.infer.align_of(&ty));
            fields.push(hir::HirField {
                name: f.name.clone(),
                ty: ty.clone(),
                visibility: f.visibility,
                index: idx,
                offset,
                align: self.infer.align_of(&ty),
                span: f.span,
            });
        }

        let repr = self.compute_repr(s);
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
        let mut next_disc = 0i128;

        for v in &e.variants {
            let disc = match &v.discriminant {
                Some(d) => {
                    // 编译期求值判别式
                    let val = self.eval_const_expr(d)?;
                    next_disc = val;
                    val
                }
                None => {
                    let d = next_disc;
                    next_disc += 1;
                    d
                }
            };
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
                                visibility: f.visibility,
                                index: i,
                                offset: 0,
                                align: 0,
                                span: f.span,
                            })
                        })
                        .collect();
                    hir::HirVariantPayload::Struct(hir_fields?)
                }
            };
            variants.push(hir::HirVariant {
                name: v.name.clone(),
                payload,
                discriminant: Some(disc),
                span: v.span,
            });
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
                    } else { hir::IntegerRepr::Auto }
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
                visibility: f.visibility,
                index: idx,
                offset: 0, // 所有字段从 0 开始
                align: al,
                span: f.span,
            });
        }
        if let Some(hir_td) = self.type_defs.get_mut(&def_id) {
            if let hir::HirTypeDef::Union(hu) = hir_td {
                hu.fields = fields;
            }
        }
        Ok(())
    }

    fn compute_repr(&self, s: &AstStruct) -> hir::Repr {
        for attr in &s.attrs {
            match attr.name.as_str() {
                "repr" => {
                    if let Some(AttributeArg::Pos(Expression::Variable(name, _))) = attr.args.first() {
                        return match name.as_str() {
                            "C" => hir::Repr::C,
                            "packed" => hir::Repr::Packed,
                            "transparent" => hir::Repr::Transparent,
                            "simd" => hir::Repr::Simd,
                            _ => hir::Repr::Rust,
                        };
                    }
                }
                _ => {}
            }
        }
        hir::Repr::Rust
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
            let assoc_types: Vec<_> = t.assoc_types.iter().map(|at| hir::HirAssocType {
                name: at.name.clone(),
                bounds: Vec::new(),
                default: at.default.as_ref().and_then(|t| self.ast_to_hir_type(t).ok()),
                span: at.span,
            }).collect();
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
            let mut items = Vec::new();
            for f in &ib.items {
                let fid = self.find_def(&f.name)?;
                items.push(self.fn_to_hir(f, fid));
            }
            let where_clause: Vec<_> = ib.where_clause.iter().map(|w| WherePred {
                lhs: self.ast_to_hir_type(&w.lhs).unwrap_or(Ty::Infer(InferId(0))),
                trait_id: DefId(0),
                hrtb: w.for_lifetimes.iter().map(|l| hir::Region::Named(l.clone())).collect(),
                negated: w.negated,
                span: w.span,
            }).collect();
            let hir_impl = hir::HirImpl {
                generics: ib.generics.clone(),
                target: target.clone(),
                trait_id,
                where_clause,
                items,
                assoc_types: ib.assoc_types.iter().map(|at| hir::HirAssocType {
                    name: at.name.clone(),
                    bounds: Vec::new(),
                    default: at.default.as_ref().and_then(|t| self.ast_to_hir_type(t).ok()),
                    span: at.span,
                }).collect(),
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
            // CTFE：验证值在编译期可求值
            self.eval_const_expr(&c.value)?;
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

    fn check_function_bodies(
        &mut self,
        prog: &Program,
    ) -> Result<Vec<hir::HirFunction>> {
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
                let self_ty = Ty::Named(id, Vec::new()); // 占位：实际应是 impl 的目标类型
                let ty = match recv.mutability {
                    ast::Mutability::Mutable => Ty::Reference(Box::new(self_ty), hir::Mutability::Mutable),
                    ast::Mutability::Immutable => Ty::Reference(Box::new(self_ty), hir::Mutability::Immutable),
                };
                self.locals.insert(local, ty.clone());
                self.infer.locals.insert(local, ty);
            }

            // 检查参数
            let mut params = Vec::new();
            for p in &f.params {
                let ty = self.ast_to_hir_type(&p.ty)?;
                let local = self.alloc_local();
                self.locals.insert(local, ty.clone());
                self.infer.locals.insert(local, ty.clone());
                self.scope_stack.last_mut().unwrap().push(local);
                params.push(hir::HirParam {
                    local,
                    pattern: self.lower_pattern(&p.pattern, &ty)?,
                    ty,
                    span: p.span,
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
        Ok(self.functions.values().cloned().collect())
    }

    fn check_block(&mut self, b: &AstBlock, expected: &Ty) -> Result<hir::HirBlock> {
        self.push_scope();
        let mut stmts = Vec::new();
        for s in &b.stmts {
            stmts.push(self.check_stmt(s)?);
        }
        let tail = if let Some(t) = &b.tail {
            let e = self.check_expr(t, expected)?;
            self.infer.subtype(&e.ty, expected, t.span())
                .map_err(|e| self.errors.push(e)).ok();
            Some(Box::new(e))
        } else if !matches!(expected, Ty::Unit | Ty::Never) && !self.current_returns() {
            // 块缺少尾部表达式，但期望非空类型
            self.errors.push(CompileError::type_error(format!(
                "block missing value: expected `{}`, found `()`",
                self.infer.type_name(expected)
            ))
            .with_span(b.span));
            None
        } else {
            None
        };
        self.pop_scope();
        Ok(hir::HirBlock {
            stmts,
            tail,
            ty: expected.clone(),
            span: b.span,
        })
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
                    self.infer.unify(&h.ty, &ty, e.span())
                        .map_err(|e| self.errors.push(e)).ok();
                    Some(Box::new(h))
                } else {
                    None
                };
                let local = self.alloc_local();
                self.locals.insert(local, ty.clone());
                self.infer.locals.insert(local, ty.clone());
                self.scope_stack.last_mut().unwrap().push(local);
                Ok(HirStmt::Let {
                    local,
                    ty,
                    init: init_hir,
                    span: *span,
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
                    Some(Box::new(self.check_expr(e, &ty)?))
                } else {
                    None
                };
                let local = self.alloc_local();
                self.locals.insert(local, ty.clone());
                self.infer.locals.insert(local, ty.clone());
                self.scope_stack.last_mut().unwrap().push(local);
                Ok(HirStmt::Var {
                    local,
                    ty,
                    init: init_hir,
                    span: *span,
                })
            }
            Statement::Const(name, ast_ty, value_expr, span) => {
                let ty = self.ast_to_hir_type(ast_ty)?;
                let value = self.check_const_expr(value_expr, &ty)?;
                let def = self.alloc_def(name.clone());
                self.consts.insert(def, hir::HirConst {
                    def_id: def,
                    name: name.clone(),
                    visibility: crate::parser::ast::Visibility::Private,
                    ty: ty.clone(),
                    value: Box::new(value),
                    span: *span,
                });
                Ok(HirStmt::Item)
            }
            Statement::Expr(e, span) => Ok(HirStmt::Expr(Box::new(self.check_expr(e, &Ty::Unit)?), *span)),
            Statement::Item(_) => Ok(HirStmt::Item),
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
                    let _ty = self.locals.get(&local).cloned().unwrap_or(Ty::Infer(InferId(0)));
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
                let ty = match op {
                    ast::UnaryOp::Neg => Ty::Int(32),
                    ast::UnaryOp::Not => Ty::Bool,
                    ast::UnaryOp::BitNot => Ty::Int(32),
                    ast::UnaryOp::Deref => {
                        // 需要 infer
                        self.infer.fresh_infer(span)
                    }
                    ast::UnaryOp::AddrOf => {
                        self.infer.fresh_infer(span)
                    }
                };
                let operand = self.check_expr(operand, &ty)?;
                match op {
                    ast::UnaryOp::Deref => {
                        // *p : 若 p: &T 则结果为 T
                        let inner = self.infer.fresh_infer(span);
                        self.infer.unify(
                            &operand.ty,
                            &Ty::Reference(Box::new(inner.clone()), hir::Mutability::Immutable),
                            span,
                        ).ok();
                        return Ok(HirExpr { kind: ExprKind::Deref(Box::new(operand)), ty: inner, span });
                    }
                    ast::UnaryOp::AddrOf => {
                        let inner = self.infer.fresh_infer(span);
                        let ty = Ty::Reference(Box::new(inner.clone()), hir::Mutability::Immutable);
                        return Ok(HirExpr { kind: ExprKind::AddrOf(Box::new(operand), hir::Mutability::Immutable), ty, span });
                    }
                    _ => ExprKind::Unary(self.ast_unary_op(*op), Box::new(operand)),
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
            Expression::MethodCall(receiver, name, args, _) => {
                return self.check_method_call(receiver, name, args, expected, span);
            }
            Expression::Index(receiver, index, _) => {
                let recv = self.check_expr(receiver, &Ty::Infer(InferId(0)))?;
                let idx = self.check_expr(index, &Ty::Int(64))?;
                let elem_ty = self.infer.fresh_infer(span);
                // 推断 receiver 为 array/slice/pointer
                match &recv.ty {
                    Ty::Array(t, _) | Ty::Slice(t, _) => {
                        self.infer.unify(t, &elem_ty, span).ok();
                    }
                    _ => {
                        // 尝试索引为指针
                        self.infer.unify(
                            &recv.ty,
                            &Ty::Pointer(Box::new(elem_ty.clone()), hir::Mutability::Immutable),
                            span,
                        ).ok();
                    }
                }
                return Ok(HirExpr { kind: ExprKind::Index(Box::new(recv), Box::new(idx)), ty: elem_ty, span });
            }
            Expression::Field(receiver, name, _) => {
                let recv = self.check_expr(receiver, &Ty::Infer(InferId(0)))?;
                let (ty, idx) = self.check_field_access(&recv.ty, name, span)?;
                return Ok(HirExpr { kind: ExprKind::Field(Box::new(recv), idx), ty, span });
            }
            Expression::Deref(e, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                let elem = self.infer.fresh_infer(span);
                self.infer.unify(
                    &inner.ty,
                    &Ty::Reference(Box::new(elem.clone()), hir::Mutability::Immutable),
                    span,
                ).ok();
                return Ok(HirExpr { kind: ExprKind::Deref(Box::new(inner)), ty: elem, span });
            }
            Expression::AddrOf(e, m, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                let ty = Ty::Reference(Box::new(inner.ty.clone()), *m);
                return Ok(HirExpr { kind: ExprKind::AddrOf(Box::new(inner), *m), ty, span });
            }
            Expression::Cast(e, target, _) => {
                let target_ty = self.ast_to_hir_type(target)?;
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                self.validate_cast(&inner.ty, &target_ty, span)?;
                return Ok(HirExpr { kind: ExprKind::Cast(Box::new(inner), target_ty.clone()), ty: target_ty, span });
            }
            Expression::Is(e, target, _) => {
                let _inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                let _target = self.ast_to_hir_type(target)?;
                return Ok(HirExpr { kind: ExprKind::Is(Box::new(_inner), _target.clone()), ty: Ty::Bool, span });
            }
            Expression::In(e, target, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                let _target = self.check_expr(target, &Ty::Infer(InferId(0)))?;
                return Ok(HirExpr { kind: ExprKind::Is(Box::new(inner), Ty::Bool), ty: Ty::Bool, span });
            }
            Expression::Block(b, _) => {
                let inner = self.check_block(b, expected)?;
                let ty = inner.tail.as_ref().map(|t| t.ty.clone()).unwrap_or(Ty::Unit);
                return Ok(HirExpr { kind: ExprKind::Block(inner), ty, span });
            }
            Expression::If(cond, then_b, else_b, _) => {
                let c = self.check_expr(cond, &Ty::Bool)?;
                let then_hir = self.check_block(then_b, expected)?;
                let else_hir = if let Some(e) = else_b {
                    let e_expr = if let Expression::Block(b, _) = e.as_ref() {
                        self.check_block(b, expected)?
                    } else {
                        self.check_expr(e, expected)?;
                        // 包装为块
                        hir::HirBlock { stmts: Vec::new(), tail: None, ty: expected.clone(), span: e.span() }
                    };
                    Some(e_expr)
                } else {
                    None
                };
                let ty = if let Some(ref el) = else_hir {
                    // 合并 then/else 类型
                    let t = then_hir.tail.as_ref().map(|t| t.ty.clone()).unwrap_or(Ty::Unit);
                    let e = el.tail.as_ref().map(|t| t.ty.clone()).unwrap_or(Ty::Unit);
                    self.unify_or(&t, &e)
                } else {
                    Ty::Unit
                };
                return Ok(HirExpr { kind: ExprKind::If(Box::new(c), then_hir, else_hir), ty, span });
            }
            Expression::Match(scrutinee, arms, _) => {
                return self.check_match(scrutinee, arms, expected, span);
            }
            Expression::While(cond, body, else_b, _) => {
                let c = self.check_expr(cond, &Ty::Bool)?;
                let b = self.check_block(body, &Ty::Unit)?;
                let else_hir = if let Some(eb) = else_b {
                    Some(self.check_block(eb, &Ty::Unit)?)
                } else {
                    None
                };
                return Ok(HirExpr { kind: ExprKind::While(Box::new(c), b, else_hir), ty: Ty::Unit, span });
            }
            Expression::For(_pat, iter, body, _) => {
                let it = self.check_expr(iter, &Ty::Infer(InferId(0)))?;
                let var = self.alloc_local();
                let var_ty = self.infer.fresh_infer(span);
                self.locals.insert(var, var_ty.clone());
                self.infer.locals.insert(var, var_ty.clone());
                self.scope_stack.last_mut().unwrap().push(var);
                let b = self.check_block(body, &Ty::Unit)?;
                return Ok(HirExpr {
                    kind: ExprKind::For(ForLoop {
                        var,
                        iter: Box::new(it),
                        body: b,
                        protocol: ForProtocol::ByValue,
                    }),
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::Loop(body, _) => {
                let b = self.check_block(body, &Ty::Unit)?;
                return Ok(HirExpr { kind: ExprKind::Loop(b), ty: Ty::Never, span });
            }
            Expression::Break(label, value, _) => {
                let v = if let Some(val) = value {
                    Some(Box::new(self.check_expr(val, &Ty::Infer(InferId(0)))?))
                } else {
                    None
                };
                let bv = match (label, v) {
                    (Some(l), Some(v)) => BreakValue::Labeled(l.clone(), v),
                    (Some(l), None) => BreakValue::Label(l.clone()),
                    (None, Some(v)) => BreakValue::Value(v),
                    (None, None) => BreakValue::None,
                };
                return Ok(HirExpr { kind: ExprKind::Break(bv), ty: Ty::Never, span });
            }
            Expression::Continue(label, _) => {
                return Ok(HirExpr { kind: ExprKind::Break(BreakValue::Label(label.clone().unwrap_or_default())), ty: Ty::Never, span });
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
                return Ok(HirExpr { kind: ExprKind::Return(v), ty: Ty::Never, span });
            }
            Expression::Defer(e, _) => {
                let inner = self.check_expr(e, &Ty::Unit)?;
                return Ok(HirExpr { kind: ExprKind::Defer(Box::new(inner)), ty: Ty::Unit, span });
            }
            Expression::Try(e, pat, catch, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                let handler = if let Some(p) = pat {
                    let _ = self.lower_pattern(p, &inner.ty)?;
                    Some(Box::new(HirExpr {
                        kind: ExprKind::Block(self.check_block(catch, &Ty::Infer(InferId(0)))?),
                        ty: Ty::Infer(InferId(0)),
                        span,
                    }))
                } else {
                    None
                };
                return Ok(HirExpr { kind: ExprKind::Try(Box::new(inner), handler), ty: Ty::Infer(InferId(0)), span });
            }
            Expression::Throw(e, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                return Ok(HirExpr { kind: ExprKind::Throw(Box::new(inner)), ty: Ty::Never, span });
            }
            Expression::Array(elems, repeat, _) => {
                if let Some(rep) = repeat {
                    let elem = self.infer.fresh_infer(span);
                    let _ = self.check_expr(rep, &Ty::Uint(64));
                    let first = self.check_expr(&elems[0], &elem)?;
                    return Ok(HirExpr {
                        kind: ExprKind::Array(vec![first], elem.clone()),
                        ty: Ty::Array(Box::new(elem), hir::ArrayLen::Dynamic),
                        span,
                    });
                }
                let elem = self.infer.fresh_infer(span);
                let mut hir_elems = Vec::new();
                for e in elems {
                    hir_elems.push(self.check_expr(e, &elem)?);
                }
                return Ok(HirExpr {
                    kind: ExprKind::Array(hir_elems, elem.clone()),
                    ty: Ty::Array(Box::new(elem), hir::ArrayLen::Dynamic),
                    span,
                });
            }
            Expression::Tuple(elems, _) => {
                let mut hir_elems = Vec::new();
                let mut tys = Vec::new();
                for e in elems {
                    let h = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                    tys.push(h.ty.clone());
                    hir_elems.push(h);
                }
                return Ok(HirExpr { kind: ExprKind::Tuple(hir_elems), ty: Ty::Tuple(tys), span });
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
                let inner_ty = inner.ty.clone();
                let _ = self.eval_const_expr(e)?;
                return Ok(HirExpr { kind: ExprKind::Comptime(Box::new(inner)), ty: inner_ty, span });
            }
            Expression::Await(e, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                return Ok(HirExpr { kind: ExprKind::Await(Box::new(inner)), ty: Ty::Infer(InferId(0)), span });
            }
            Expression::Asm(asm, _) => {
                let outputs: Vec<_> = asm.outputs.iter().map(|o| hir::HirAsmOperand {
                    constraint: o.constraint.clone(),
                    expr: Box::new(self.check_expr(&o.expr, &Ty::Infer(InferId(0))).unwrap_or(HirExpr {
                        kind: hir::ExprKind::Literal(hir::HirLiteral::Composite(Vec::new())),
                        ty: Ty::Unit,
                        span,
                    })),
                }).collect();
                let inputs: Vec<_> = asm.inputs.iter().map(|i| hir::HirAsmOperand {
                    constraint: i.constraint.clone(),
                    expr: Box::new(self.check_expr(&i.expr, &Ty::Infer(InferId(0))).unwrap_or(HirExpr {
                        kind: hir::ExprKind::Literal(hir::HirLiteral::Composite(Vec::new())),
                        ty: Ty::Unit,
                        span,
                    })),
                }).collect();
                return Ok(HirExpr {
                    kind: ExprKind::Asm(hir::HirAsmExpr {
                        template: asm.template.clone(),
                        outputs,
                        inputs,
                        options: asm.options.clone(),
                    }),
                    ty: Ty::Unit,
                    span,
                });
            }
            Expression::Yield(e, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                return Ok(HirExpr { kind: ExprKind::Yield(Box::new(inner)), ty: Ty::Infer(InferId(0)), span });
            }
            Expression::Spawn(e, _) => {
                let inner = self.check_expr(e, &Ty::Infer(InferId(0)))?;
                return Ok(HirExpr { kind: ExprKind::Spawn(Box::new(inner)), ty: Ty::Infer(InferId(0)), span });
            }
            Expression::Labeled(_label, e, _) => {
                let inner = self.check_expr(e, expected)?;
                self.infer.record_borrow(LocalId(0), infer::BorrowKind::Shared, 0);
                return Ok(HirExpr { kind: inner.kind, ty: inner.ty, span });
            }
            Expression::Goto(label, _) => {
                return Err(CompileError::internal(format!(
                    "`goto` to label '{}' requires basic block construction (not yet implemented)",
                    label
                ))
                .with_span(span));
            }
            Expression::Intrinsic(kind, types, _) => {
                let hir_types: Result<Vec<Ty>> = types.iter()
                    .map(|t| self.ast_to_hir_type(t))
                    .collect();
                let ty = match kind {
                    ast::IntrinsicKind::SizeOf | ast::IntrinsicKind::AlignOf => Ty::Uint(64),
                    ast::IntrinsicKind::TypeOf => Ty::Infer(InferId(0)),
                    _ => Ty::Infer(InferId(0)),
                };
                return Ok(HirExpr {
                    kind: ExprKind::Intrinsic(self.lower_intrinsic(*kind), hir_types?),
                    ty,
                    span,
                });
            }
            Expression::CompileError(msg, _) => {
                return Err(CompileError::internal(format!(
                    "@compileError({:?})",
                    msg
                ))
                .with_span(span));
            }
            Expression::FuncRef(name, _) => {
                let id = self.resolve_name(name, span)?;
                return Ok(HirExpr { kind: ExprKind::Def(id), ty: Ty::Infer(InferId(0)), span });
            }
            Expression::Interpolated(parts, _) => {
                let hir_parts: Result<Vec<_>> = parts.iter().map(|p| match p {
                    ast::InterpPart::Text(s) => Ok(hir::InterpPart::Text(s.clone())),
                    ast::InterpPart::Expr(e) => Ok(hir::InterpPart::Expr(self.check_expr(e, &Ty::Infer(InferId(0)))?)),
                }).collect();
                return Ok(HirExpr { kind: ExprKind::Interpolated(hir_parts?), ty: Ty::Str, span });
            }
        };
        let ty = self.infer.fresh_infer(span);
        Ok(HirExpr { kind, ty, span })
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
        let r_ty = self.infer_expr_ty(rhs)?;
        let l = self.check_expr(lhs, &l_ty)?;
        let r = self.check_expr(rhs, &r_ty)?;
        self.infer.unify(&l.ty, &r.ty, span).ok();
        let ty = match op {
            ast::BinaryOp::Add | ast::BinaryOp::Sub | ast::BinaryOp::Mul
            | ast::BinaryOp::Div | ast::BinaryOp::Rem
            | ast::BinaryOp::BitAnd | ast::BinaryOp::BitOr | ast::BinaryOp::BitXor
            | ast::BinaryOp::Shl | ast::BinaryOp::Shr => l.ty.clone(),
            ast::BinaryOp::Eq | ast::BinaryOp::Ne | ast::BinaryOp::Lt
            | ast::BinaryOp::Gt | ast::BinaryOp::Le | ast::BinaryOp::Ge => Ty::Bool,
            ast::BinaryOp::And | ast::BinaryOp::Or => Ty::Bool,
            ast::BinaryOp::Range | ast::BinaryOp::RangeInclusive => {
                Ty::Reference(Box::new(l.ty.clone()), hir::Mutability::Immutable)
            }
            ast::BinaryOp::Concat => Ty::Str,
        };
        Ok(HirExpr { kind: ExprKind::Binary(bin_op, Box::new(l), Box::new(r)), ty, span })
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
                let params: Vec<Ty> = (0..args.len()).map(|_| self.infer.fresh_infer(span)).collect();
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
        let mut hir_args = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            let a = match arg {
                ast::Argument::Pos(e) => e,
                ast::Argument::Named(name, e) => {
                    // 命名参数：查找对应参数
                    let _ = name;
                    e
                }
            };
            let param_ty = param_tys.get(i).cloned().unwrap_or_else(|| Ty::Infer(InferId(0)));
            hir_args.push(self.check_expr(a, &param_ty)?);
        }
        Ok(HirExpr { kind: ExprKind::Call(Box::new(callee_hir), hir_args), ty: ret_ty, span })
    }

    fn check_method_call(
        &mut self,
        receiver: &Expression,
        name: &str,
        args: &[ast::Argument],
        _expected: &Ty,
        span: ast::Span,
    ) -> Result<HirExpr> {
        let recv_ty = self.infer_expr_ty(receiver)?;
        let recv = self.check_expr(receiver, &recv_ty)?;
        // 查找方法：先在 receiver 的 impl 中找，再在 trait 中找
        let method_id = self.lookup_method(&recv.ty, name, span)?;
        let mut hir_args = Vec::new();
        for arg in args {
            let e = match arg {
                ast::Argument::Pos(e) => e,
                ast::Argument::Named(_, e) => e,
            };
            hir_args.push(self.check_expr(e, &Ty::Infer(InferId(0)))?);
        }
        Ok(HirExpr {
            kind: ExprKind::MethodCall(Box::new(recv), method_id, hir_args),
            ty: Ty::Infer(InferId(0)),
            span,
        })
    }

    fn lookup_method(&self, _recv_ty: &Ty, name: &str, span: ast::Span) -> Result<DefId> {
        // 遍历所有 impl 块，查找匹配的方法
        for impl_block in &self.impl_blocks {
            for item in &impl_block.items {
                if item.name == name {
                    return Ok(item.id);
                }
            }
        }
        Err(CompileError::name_error(format!("no method named `{}` found", name)).with_span(span))
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
        let mut hir_arms = Vec::new();
        for arm in arms {
            let pat = self.lower_pattern(&arm.pattern, &s.ty)?;
            let guard = if let Some(g) = &arm.guard {
                Some(Box::new(self.check_expr(g, &Ty::Bool)?))
            } else {
                None
            };
            let body = self.check_expr(&arm.body, expected)?;
            hir_arms.push(HirArm { pattern: pat, guard, body: Box::new(body) });
        }
        // 记录模式用于穷尽性检查
        self.check_exhaustiveness_arm(&s.ty, &hir_arms, span)?;
        let ty = if let Some(first) = hir_arms.first() {
            first.body.ty.clone()
        } else {
            Ty::Never
        };
        Ok(HirExpr { kind: ExprKind::Match(Box::new(s), hir_arms), ty, span })
    }

    fn check_exhaustiveness_arm(
        &self,
        _scrutinee_ty: &Ty,
        arms: &[HirArm],
        span: ast::Span,
    ) -> Result<()> {
        // 简化检查：若最后一个模式不是通配符，且 scrutinee 类型不是 bool/数值，
        // 则警告可能不穷尽。
        if let Some(last) = arms.last() {
            if !matches!(last.pattern, hir::HirPattern::Wildcard) {
                // 对 bool / 单变体枚举 / 数值类型，至少需要覆盖所有可能值
                // 此处为框架实现
                let _ = span;
            }
        }
        Ok(())
    }

    fn check_exhaustiveness(&self, _functions: &[hir::HirFunction]) -> Result<()> {
        // 遍历所有 match 表达式，验证穷尽性
        // 使用算法：对每个 match，构造 "witness" —— 一个不在任何模式中的值
        // 若存在 witness 且类型非可空，则报错
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
            Some(hir::HirTypeDef::Struct(s)) => s.fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect(),
            _ => return Err(CompileError::type_error(format!(
                "`{}` is not a struct",
                path.join("::")
            ))
            .with_span(span)),
        };
        let mut hir_fields = Vec::new();
        let mut seen = HashSet::new();
        for (name, expr) in fields {
            let field_idx = field_tys.iter().position(|(n, _)| n == name);
            let idx = match field_idx {
                Some(i) => i,
                None => {
                    return Err(CompileError::name_error(format!(
                        "no field `{}` on struct `{}`",
                        name, path.join("::")
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
            let hir_expr = self.check_expr(expr, fty)?;
            hir_fields.push((idx, hir_expr));
        }
        // 检查是否有未初始化的字段（结构体字面量要求全部初始化）
        for (name, _) in &field_tys {
            if !seen.contains(name) {
                return Err(CompileError::type_error(format!(
                    "missing field `{}` in struct literal for `{}`",
                    name, path.join("::")
                ))
                .with_span(span));
            }
        }
        Ok(HirExpr {
            kind: ExprKind::Struct(def_id, hir_fields),
            ty: Ty::Named(def_id, Vec::new()),
            span,
        })
    }

    fn check_field_access(
        &self,
        ty: &Ty,
        name: &str,
        span: ast::Span,
    ) -> Result<(Ty, FieldIndex)> {
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
                Err(CompileError::type_error(format!(
                    "tuple index out of range: {}",
                    name
                ))
                .with_span(span))
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

    fn check_literal(
        &self,
        lit: &AstLiteral,
        expected: &Ty,
    ) -> Result<(ExprKind, Ty)> {
        Ok(match lit {
            AstLiteral::Int(v, suffix) => {
                let ty = match suffix.as_deref() {
                    Some("i8") => Ty::Int(8),
                    Some("i16") => Ty::Int(16),
                    Some("i32") => Ty::Int(32),
                    Some("i64") => Ty::Int(64),
                    Some("u8") => Ty::Uint(8),
                    Some("u16") => Ty::Uint(16),
                    Some("u32") => Ty::Uint(32),
                    Some("u64") => Ty::Uint(64),
                    _ => {
                        // 默认推断：尝试从 expected 推断
                        if let Ty::Int(w) = expected { Ty::Int(*w) }
                        else if let Ty::Uint(w) = expected { Ty::Uint(*w) }
                        else { Ty::Int(32) }
                    }
                };
                (ExprKind::Literal(hir::HirLiteral::Int(*v)), ty)
            }
            AstLiteral::Float(v, suffix) => {
                let ty = match suffix.as_deref() {
                    Some("f32") => Ty::Float(32),
                    Some("f64") => Ty::Float(64),
                    _ => Ty::Float(64),
                };
                (ExprKind::Literal(hir::HirLiteral::Float(*v)), ty)
            }
            AstLiteral::String(s) => {
                (ExprKind::Literal(hir::HirLiteral::String(s.clone())), Ty::Str)
            }
            AstLiteral::Char(c) => {
                (ExprKind::Literal(hir::HirLiteral::Char(*c)), Ty::Char)
            }
            AstLiteral::Bool(b) => {
                (ExprKind::Literal(hir::HirLiteral::Bool(*b)), Ty::Bool)
            }
        })
    }

    fn lower_pattern(&mut self, p: &AstPattern, ty: &Ty) -> Result<hir::HirPattern> {
        Ok(match p {
            AstPattern::Wildcard(_) => hir::HirPattern::Wildcard,
            AstPattern::Literal(lit) => {
                let (kind, _) = self.check_literal(lit, ty)?;
                match kind {
                    ExprKind::Literal(l) => hir::HirPattern::Literal(l),
                    _ => hir::HirPattern::Wildcard,
                }
            }
            AstPattern::Binding(_name, mutability, sub, _) => {
                let local = self.alloc_local();
                self.locals.insert(local, ty.clone());
                self.infer.locals.insert(local, ty.clone());
                self.scope_stack.last_mut().unwrap().push(local);
                let _ = mutability;
                if let Some(s) = sub {
                    let inner = self.lower_pattern(s, ty)?;
                    hir::HirPattern::Ref(Box::new(inner), hir::Mutability::Immutable)
                } else {
                    hir::HirPattern::Binding(local, ty.clone())
                }
            }
            AstPattern::Tuple(pats, _) => {
                let elem_tys = match ty {
                    Ty::Tuple(ts) => ts.clone(),
                    _ => Vec::new(),
                };
                let hir_pats: Result<Vec<_>> = pats.iter().enumerate()
                    .map(|(i, p)| self.lower_pattern(p, elem_tys.get(i).unwrap_or(ty)))
                    .collect();
                hir::HirPattern::Tuple(hir_pats?, ty.clone())
            }
            AstPattern::Struct { path, fields, rest, span } => {
                let def_id = self.resolve_path(path, *span)?;
                let hir_fields: Result<Vec<_>> = fields.iter()
                    .map(|fp| {
                        let fty = self.field_type(def_id, &fp.name, *span)?;
                        Ok((0, self.lower_pattern(&fp.pattern, &fty)?))
                    })
                    .collect();
                hir::HirPattern::Struct { def_id, fields: hir_fields?, rest: *rest }
            }
            AstPattern::Variant { path, args, span } => {
                let def_id = self.resolve_path(&path[..path.len()-1], *span)?;
                let variant_name = path.last().unwrap();
                let variant_idx = self.variant_index(def_id, variant_name);
                let payload_tys = self.variant_payload_tys(def_id, variant_idx, *span)?;
                let hir_args: Result<Vec<_>> = args.iter().enumerate()
                    .map(|(i, p)| self.lower_pattern(p, payload_tys.get(i).unwrap_or(ty)))
                    .collect();
                hir::HirPattern::Variant { enum_id: def_id, variant: variant_idx, args: hir_args? }
            }
            AstPattern::Slice(pats, _) => {
                hir::HirPattern::Slice(pats.iter().map(|p| self.lower_pattern(p, ty)).collect::<Result<_>>()?, None, Vec::new())
            }
            AstPattern::Range(start, end, inclusive, _) => {
                let s = self.check_expr(start, &Ty::Infer(InferId(0)))?;
                let en = self.check_expr(end, &Ty::Infer(InferId(0)))?;
                hir::HirPattern::Range(Box::new(s), Box::new(en), if *inclusive { hir::RangeInclusivity::Inclusive } else { hir::RangeInclusivity::Exclusive })
            }
            AstPattern::Ref(inner, mutability, _) => {
                hir::HirPattern::Ref(Box::new(self.lower_pattern(inner, ty)?), *mutability)
            }
            AstPattern::Guard(pat, _guard, _) => self.lower_pattern(pat, ty)?,
        })
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

    fn variant_payload_tys(&self, _def_id: DefId, _idx: usize, _span: ast::Span) -> Result<Vec<Ty>> {
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
        let ret_ty = ret.as_ref().map(|t| self.ast_to_hir_type(t)).unwrap_or_else(|| Ok(self.infer.fresh_infer(span)))?;
        let body_expr = HirExpr {
            kind: ExprKind::Block(self.check_block(body, &ret_ty)?),
            ty: ret_ty.clone(),
            span,
        };
        self.pop_scope();
        // 捕获分析
        let captures = self.compute_captures(&body_expr);
        let fn_ty = Ty::Function {
            params: hir_params.iter().map(|(_, t)| t.clone()).collect(),
            ret: Box::new(ret_ty),
            is_async: false,
            abi: Abi::Zr,
        };
        Ok(HirExpr {
            kind: ExprKind::Closure(Box::new(ClosureExpr {
                params: hir_params,
                captures,
                ty: fn_ty.clone(),
                body: body_expr,
            })),
            ty: fn_ty,
            span,
        })
    }

    fn compute_captures(&self, _body: &HirExpr) -> Vec<Capture> {
        // 分析闭包体引用了哪些外部局部变量，
        // 按使用方式（读 / 写）决定按值还是按引用捕获。
        Vec::new()
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
            id: start,
            stmts: Vec::new(),
            term: hir::Terminator::Unreachable,
            preds: Vec::new(),
            succs: Vec::new(),
            dominators: Vec::new(),
            defs: Vec::new(),
        };
        for s in &block.stmts {
            bb.stmts.push(s.clone());
        }
        if let Some(tail) = &block.tail {
            bb.term = hir::Terminator::Return(Some(tail.clone()));
        } else {
            bb.term = hir::Terminator::Return(None);
        }
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
        let id = BasicBlockId(self.next_bb);
        self.next_bb += 1;
        id
    }

    fn find_def(&self, name: &str) -> Result<DefId> {
        self.symbols.get(name).copied().ok_or_else(|| {
            CompileError::name_error(format!("undefined name `{}`", name))
        })
    }

    fn resolve_name(&self, name: &str, span: ast::Span) -> Result<DefId> {
        // 先查局部变量，再查全局符号
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

    fn as_local(&self, _id: DefId) -> Option<LocalId> {
        // 若 id 对应某个局部变量则返回
        None
    }

    // ==================================================================
    // 类型转换辅助
    // ==================================================================

    fn ast_to_hir_type(&mut self, t: &AstType) -> Result<Ty> {
        Ok(match t {
            AstType::I(w) => Ty::Int(*w as u16),
            AstType::U(w) => Ty::Uint(*w as u16),
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
                Ty::Named(id, generics.iter().map(|g| self.ast_to_hir_type(g)).collect::<Result<_>>()?)
            }
            AstType::Pointer(inner, m) => Ty::Pointer(Box::new(self.ast_to_hir_type(inner)?), *m),
            AstType::Reference(inner, m) => Ty::Reference(Box::new(self.ast_to_hir_type(inner)?), *m),
            AstType::Array(inner, len) => Ty::Array(
                Box::new(self.ast_to_hir_type(inner)?),
                match len {
                    ast::ArrayLen::Const(e) => hir::ArrayLen::Const(Box::new(self.lower_const_expr(e)?)),
                    ast::ArrayLen::Dynamic => hir::ArrayLen::Dynamic,
                },
            ),
            AstType::Slice(inner, m) => Ty::Slice(Box::new(self.ast_to_hir_type(inner)?), *m),
            AstType::Tuple(tys) => Ty::Tuple(tys.iter().map(|t| self.ast_to_hir_type(t)).collect::<Result<_>>()?),
            AstType::Function { params, ret, is_async, abi } => Ty::Function {
                params: params.iter().map(|p| self.ast_to_hir_type(p)).collect::<Result<_>>()?,
                ret: Box::new(self.ast_to_hir_type(ret)?),
                is_async: *is_async,
                abi: abi.clone(),
            },
            AstType::Optional(inner) => Ty::Optional(Box::new(self.ast_to_hir_type(inner)?)),
            AstType::ErrorUnion(t, e) => Ty::ErrorUnion(
                Box::new(self.ast_to_hir_type(t)?),
                Box::new(self.ast_to_hir_type(e)?),
            ),
            AstType::Infer => Ty::Infer(InferId(0)),
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
            AstType::Pin(inner) => Ty::Pin(Box::new(self.ast_to_hir_type(inner)?)),
        })
    }

    fn infer_expr_ty(&mut self, e: &Expression) -> Result<Ty> {
        // 快速类型推断（用于递归调用，避免无限递归）
        Ok(match e {
            Expression::Literal(AstLiteral::Int(_, suffix), _) => match suffix.as_deref() {
                Some("i64") => Ty::Int(64),
                Some("u64") => Ty::Uint(64),
                Some("i32") => Ty::Int(32),
                _ => Ty::Int(32),
            },
            Expression::Literal(AstLiteral::Float(_, suffix), _) => match suffix.as_deref() {
                Some("f32") => Ty::Float(32),
                _ => Ty::Float(64),
            },
            Expression::Literal(AstLiteral::String(_), _) => Ty::Str,
            Expression::Literal(AstLiteral::Char(_), _) => Ty::Char,
            Expression::Literal(AstLiteral::Bool(_), _) => Ty::Bool,
            Expression::Variable(name, _) => {
                if let Some(id) = self.symbols.get(name).copied() {
                    if let Some(t) = self.consts.get(&id) { t.ty.clone() }
                    else if let Some(t) = self.statics.get(&id) { t.ty.clone() }
                    else if let Some(t) = self.functions.get(&id) { t.return_type.clone() }
                    else { self.infer.fresh_infer(e.span()) }
                } else {
                    self.infer.fresh_infer(e.span())
                }
            }
            Expression::Binary(_, op, _, _) => match op {
                ast::BinaryOp::Eq | ast::BinaryOp::Ne | ast::BinaryOp::Lt
                | ast::BinaryOp::Gt | ast::BinaryOp::Le | ast::BinaryOp::Ge
                | ast::BinaryOp::And | ast::BinaryOp::Or => Ty::Bool,
                _ => self.infer.fresh_infer(e.span()),
            },
            Expression::Unary(op, _, _) => match op {
                ast::UnaryOp::Not => Ty::Bool,
                _ => self.infer.fresh_infer(e.span()),
            },
            Expression::Call(callee, _, _) => {
                let cty = self.infer_expr_ty(callee)?;
                match cty {
                    Ty::Function { ret, .. } => *ret,
                    _ => self.infer.fresh_infer(e.span()),
                }
            }
            Expression::Block(b, _) => b.tail.as_ref().map(|t| self.infer_expr_ty(t)).unwrap_or(Ok(Ty::Unit))?,
            Expression::If(_, then_b, else_b, _) => {
                if let Some(eb) = else_b {
                    let t = self.infer_expr_ty(&Expression::Block(then_b.clone(), ast::Span::unknown()))?;
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
                let id = self.resolve_path(path, ast::Span::unknown()).unwrap_or(DefId(0));
                Ty::Named(id, Vec::new())
            }
            Expression::Tuple(elems, _) => Ty::Tuple(
                elems.iter().map(|e| self.infer_expr_ty(e)).collect::<Result<_>>()?
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
                let ret_ty = ret.as_ref().map(|t| self.ast_to_hir_type(t)).unwrap_or_else(|| Ok(self.infer.fresh_infer(e.span())))?;
                Ty::Function { params, ret: Box::new(ret_ty), is_async: false, abi: Abi::Zr }
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
            ExprKind::Deref(inner) => self.check_assignable(inner),
            ExprKind::Index(inner, _) => self.check_assignable(inner),
            _ => Err(CompileError::type_error(
                "expression is not assignable (expected variable, field, deref, or index)",
            )
            .with_span(expr.span)),
        }
    }

    // ==================================================================
    // AST → HIR 转换辅助
    // ==================================================================

    fn fn_to_hir(&self, f: &ast::Function, id: DefId) -> hir::HirFunction {
        hir::HirFunction {
            id,
            name: f.name.clone(),
            visibility: f.visibility,
            attrs: f.attrs.clone(),
            is_async: f.is_async,
            is_noreturn: f.is_noreturn,
            is_const: f.is_const,
            is_default: f.is_default,
            abi: f.abi.clone(),
            generics: f.generics.clone(),
            params: Vec::new(),
            receiver: None,
            return_type: Ty::Infer(InferId(0)),
            body: None,
            span: f.span,
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

    fn eval_const_expr(&self, e: &Expression) -> Result<i128> {
        // 编译期求值引擎（简化版）
        match e {
            Expression::Literal(AstLiteral::Int(v, _), _) => Ok(*v),
            Expression::Binary(lhs, op, rhs, _) => {
                let l = self.eval_const_expr(lhs)?;
                let r = self.eval_const_expr(rhs)?;
                Ok(match op {
                    ast::BinaryOp::Add => l + r,
                    ast::BinaryOp::Sub => l - r,
                    ast::BinaryOp::Mul => l * r,
                    ast::BinaryOp::Div => l / r,
                    ast::BinaryOp::Rem => l % r,
                    ast::BinaryOp::BitAnd => l & r,
                    ast::BinaryOp::BitOr => l | r,
                    ast::BinaryOp::BitXor => l ^ r,
                    ast::BinaryOp::Shl => l << (r as u32),
                    ast::BinaryOp::Shr => l >> (r as u32),
                    _ => return Err(CompileError::internal(
                        "operator not allowed in compile-time context",
                    )),
                })
            }
            Expression::Unary(op, operand, _) => {
                let v = self.eval_const_expr(operand)?;
                Ok(match op {
                    ast::UnaryOp::Neg => -v,
                    ast::UnaryOp::Not => (v == 0) as i128,
                    ast::UnaryOp::BitNot => !v,
                    _ => return Err(CompileError::internal(
                        "unary operator not allowed in compile-time context",
                    )),
                })
            }
            Expression::Variable(name, span) => {
                // 查找常量定义并求值
                if let Some(id) = self.symbols.get(name).copied() {
                    if let Some(c) = self.consts.get(&id) {
                        return self.eval_const_expr_from_hir(&c.value);
                    }
                }
                Err(CompileError::internal(format!(
                    "value `{}` cannot be used in compile-time context",
                    name
                ))
                .with_span(*span))
            }
            _ => Err(CompileError::internal(
                "expression is not a compile-time constant",
            )),
        }
    }

    fn eval_const_expr_from_hir(&self, _e: &HirExpr) -> Result<i128> {
        // 从 HIR 表达式求值（需要遍历 HIR 字面量）
        Ok(0)
    }

    // ==================================================================
    // 作用域管理
    // ==================================================================

    fn push_scope(&mut self) {
        self.scope_depth += 1;
        self.scope_stack.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        if let Some(locals) = self.scope_stack.pop() {
            for l in locals {
                self.locals.remove(&l);
                self.infer.locals.remove(&l);
            }
        }
        self.scope_depth = self.scope_depth.saturating_sub(1);
    }

    fn current_returns(&self) -> bool {
        false
    }

    fn check_unused(&self, _functions: &[hir::HirFunction]) -> Result<()> {
        // 检查未使用的变量 / 导入 / 参数
        Ok(())
    }

    fn unify_or(&self, a: &Ty, b: &Ty) -> Ty {
        if a == b { a.clone() } else { Ty::Infer(InferId(0)) }
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
            ast::BinaryOp::Concat => hir::BinaryOp::Concat,
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
            ast::IntrinsicKind::CompileLog => hir::Intrinsic::TypeOf, // 降级为 no-op
            ast::IntrinsicKind::BitCast => hir::Intrinsic::BitCast,
            ast::IntrinsicKind::IntCast => hir::Intrinsic::IntCast,
            ast::IntrinsicKind::ErrorReturnTrace => hir::Intrinsic::FrameAddress,
            ast::IntrinsicKind::Frame => hir::Intrinsic::FrameAddress,
            ast::IntrinsicKind::Call => hir::Intrinsic::Call,
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
        "i8" => Some(8), "i16" => Some(16), "i32" => Some(32), "i64" => Some(64), "i128" => Some(128),
        "u8" => Some(8), "u16" => Some(16), "u32" => Some(32), "u64" => Some(64), "u128" => Some(128),
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
    let a_is_concrete = !matches!(a.target, Ty::Param(_) | Ty::Infer(_));
    let b_is_concrete = !matches!(b.target, Ty::Param(_) | Ty::Infer(_));
    if a_is_concrete && !b_is_concrete {
        return true;
    }
    if !a_is_concrete && b_is_concrete {
        return false;
    }
    // 约束更多的更 specific
    a.where_clause.len() > b.where_clause.len()
}

/// 判断两个类型是否可能重叠（即是否存在一个具体类型同时匹配两者）。
fn types_may_overlap(a: &Ty, b: &Ty) -> bool {
    match (a, b) {
        // 相同的命名类型可能重叠
        (Ty::Named(id1, _), Ty::Named(id2, _)) => id1 == id2,
        // 泛型参数可能与任何类型重叠
        (Ty::Param(_), _) | (_, Ty::Param(_)) => true,
        // 推断变量可能与任何类型重叠
        (Ty::Infer(_), _) | (_, Ty::Infer(_)) => true,
        // 指针/引用：检查内部类型
        (Ty::Pointer(t1, _), Ty::Pointer(t2, _))
        | (Ty::Reference(t1, _), Ty::Reference(t2, _)) => types_may_overlap(t1, t2),
        // 数组：检查内部类型
        (Ty::Array(t1, _), Ty::Array(t2, _)) => types_may_overlap(t1, t2),
        // 元组：检查长度和元素
        (Ty::Tuple(ts1), Ty::Tuple(ts2)) => {
            ts1.len() == ts2.len()
                && ts1.iter().zip(ts2.iter()).all(|(x, y)| types_may_overlap(x, y))
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
        Ty::Tuple(ts) => format!("({})", ts.iter().map(ty_name).collect::<Vec<_>>().join(", ")),
        Ty::Function { .. } => "fn(...)".into(),
        Ty::Optional(t) => format!("{}?", ty_name(t)),
        Ty::ErrorUnion(t, e) => format!("{}!{}", ty_name(t), ty_name(e)),
        Ty::Param(g) => format!("T{}", g.0),
        Ty::Infer(_) => "_".into(),
        Ty::Alias(t) => ty_name(t),
        Ty::Opaque(id) => format!("opaque({})", id.0),
        Ty::Pin(inner) => format!("Pin<{}>", ty_name(inner)),
        Ty::Projection { self_ty, trait_id, assoc_name } => {
            format!("<{} as trait#{}>::{}", ty_name(self_ty), trait_id.0, assoc_name)
        }
    }
}
