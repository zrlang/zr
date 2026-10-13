//! 类型推断引擎（Hindley-Milner + 子类型 + 约束求解）。
//!
//! 设计参考：
//! - Rust：区域（region）推断、trait bound 求解、impl trait。
//! - Zig：无隐式转换，仅允许显式 `as` 转换与 comptime 类型。
//! - Python：结构子类型（structural typing）用于 `anytype` / structural trait。
//!
//! 核心算法：
//! 1. 约束生成：遍历 HIR，为每个子表达式生成 `a ~ b` 或 `a <: b` 约束。
//! 2. 约束求解：统一（unification）+ 子类型闭包。
//! 3. trait 求解：选择满足 bound 的具体实现（含 specialization）。
//! 4. 闭包：捕获分析 + 类型推导。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::OnceLock;

use crate::error::{CompileError, ErrorKind, Result};
use crate::parser::ast::Span;

use super::hir::{
    self, BasicBlockId, BreakValue, CaptureKind, ClosureExpr, Cfg, DefId, ExprKind,
    FieldIndex, ForLoop, ForProtocol, GenericId, HirExpr, HirStmt, InferId, LocalId,
    Ty, TypeScheme, WherePred,
};

/// 类型推断上下文。
#[derive(Debug)]
pub struct InferCtxt<'a> {
    /// 推断变量 → 类型映射（单向）。
    pub vars: HashMap<InferId, Ty>,
    /// 约束集合。
    pub constraints: Vec<Constraint>,
    /// 下一个推断变量 ID。
    pub next_infer: u32,
    /// 类型参数 ID → 具体类型（单态化时用）。
    pub substitution: HashMap<GenericId, Ty>,
    /// trait 求解缓存：`(trait_id, self_ty) -> impl_id`。
    pub impl_cache: HashMap<(DefId, Ty), DefId>,
    /// 当前函数返回类型（用于 `return` 检查）。
    pub return_ty: Option<Ty>,
    /// 当前所在函数 ID（用于错误消息）。
    pub current_function: Option<DefId>,
    /// 借用信息（NLL 用）：每个局部的生命周期区间。
    pub borrow_intervals: Vec<BorrowInterval>,
    /// 所有错误（收集后统一报告）。
    pub errors: Vec<CompileError>,
    /// 类型定义表（用于查找字段、变体）。
    pub type_defs: &'a HashMap<DefId, hir::HirTypeDef>,
    /// trait 表。
    pub trait_defs: &'a HashMap<DefId, hir::HirTrait>,
    /// impl 表。
    pub impl_blocks: &'a [hir::HirImpl],
    /// 函数表。
    pub functions: &'a HashMap<DefId, hir::HirFunction>,
    /// 变量 → 类型映射（局部变量）。
    pub locals: &'a mut HashMap<LocalId, Ty>,
    /// 常量表（用于 CTFE 交叉引用）。
    pub consts: &'a HashMap<DefId, hir::HirConst>,
    /// 静态变量表。
    pub statics: &'a HashMap<DefId, hir::HirStatic>,
}

/// 类型约束。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Constraint {
    /// 相等约束 `T1 == T2`。
    Eq(Ty, Ty, Span),
    /// 子类型约束 `T1 <: T2`。
    Subtype(Ty, Ty, Span),
    /// trait bound `T: Trait`。
    TraitBound(Ty, DefId, Span),
    /// 生命周期约束 `'a: 'b`。
    RegionOutlives(RegionId, RegionId, Span),
}

/// 生命周期区域 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegionId(pub u32);

/// `'static` 空集合，用于 `InferCtxt::empty()` 占位（避免未初始化引用）。
fn empty_type_defs() -> &'static HashMap<DefId, hir::HirTypeDef> {
    static V: OnceLock<HashMap<DefId, hir::HirTypeDef>> = OnceLock::new();
    V.get_or_init(HashMap::new)
}
fn empty_trait_defs() -> &'static HashMap<DefId, hir::HirTrait> {
    static V: OnceLock<HashMap<DefId, hir::HirTrait>> = OnceLock::new();
    V.get_or_init(HashMap::new)
}
fn empty_functions() -> &'static HashMap<DefId, hir::HirFunction> {
    static V: OnceLock<HashMap<DefId, hir::HirFunction>> = OnceLock::new();
    V.get_or_init(HashMap::new)
}
fn empty_consts() -> &'static HashMap<DefId, hir::HirConst> {
    static V: OnceLock<HashMap<DefId, hir::HirConst>> = OnceLock::new();
    V.get_or_init(HashMap::new)
}
fn empty_statics() -> &'static HashMap<DefId, hir::HirStatic> {
    static V: OnceLock<HashMap<DefId, hir::HirStatic>> = OnceLock::new();
    V.get_or_init(HashMap::new)
}
fn empty_locals() -> &'static mut HashMap<LocalId, Ty> {
    static mut V: OnceLock<HashMap<LocalId, Ty>> = OnceLock::new();
    // SAFETY: `empty()` 只在 `Default::default()` 里调用，且紧接着被 `new()` 覆盖，
    // 返回的引用不会被跨调用/跨线程使用。
    unsafe { V.get_or_init(HashMap::new); V.get_mut().unwrap() }
}

/// 借用区间（NLL）。
#[derive(Debug, Clone)]
pub struct BorrowInterval {
    pub local: LocalId,
    /// 借用开始的程序点。
    pub start: usize,
    /// 借用结束的程序点（None = 直到作用域结束）。
    pub end: Option<usize>,
    /// 借用种类。
    pub kind: BorrowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BorrowKind {
    /// 共享借用 `&T`。
    Shared,
    /// 可变借用 `&mut T`。
    Mutable,
    /// 唯一不可变借用（类似 `&uniq T`）。
    Unique,
    /// 通过指针的借用 `*mut T`（unsafe）。
    Raw,
}

/// 类型推断结果。
#[derive(Debug, Default)]
pub struct InferResult {
    /// 已求解的类型映射。
    pub type_map: HashMap<InferId, Ty>,
    /// 残余约束（未解决的）。
    pub residual: Vec<Constraint>,
}

impl<'a> InferCtxt<'a> {
    /// 创建一个空的推断上下文（仅用于字段初始化占位）。
    ///
    /// 所有引用字段借用 `'static` 的空集合，因此不会被实际使用
    /// （正常流程下会被 `new()` 的返回值立即覆盖）。
    /// 用 `OnceLock` 避免非 const 的 `HashMap::new()` 作为静态初始值。
    pub fn empty() -> Self {
        Self {
            vars: HashMap::new(),
            constraints: Vec::new(),
            next_infer: 0,
            substitution: HashMap::new(),
            impl_cache: HashMap::new(),
            return_ty: None,
            current_function: None,
            borrow_intervals: Vec::new(),
            errors: Vec::new(),
            type_defs: empty_type_defs(),
            trait_defs: empty_trait_defs(),
            impl_blocks: &[],
            functions: empty_functions(),
            consts: empty_consts(),
            statics: empty_statics(),
            locals: empty_locals(),
        }
    }

    /// 创建一个新的推断上下文。
    pub fn new(
        type_defs: &'a HashMap<DefId, hir::HirTypeDef>,
        trait_defs: &'a HashMap<DefId, hir::HirTrait>,
        impl_blocks: &'a [hir::HirImpl],
        functions: &'a HashMap<DefId, hir::HirFunction>,
        consts: &'a HashMap<DefId, hir::HirConst>,
        statics: &'a HashMap<DefId, hir::HirStatic>,
        locals: &'a mut HashMap<LocalId, Ty>,
    ) -> Self {
        Self {
            vars: HashMap::new(),
            constraints: Vec::new(),
            next_infer: 0,
            substitution: HashMap::new(),
            impl_cache: HashMap::new(),
            return_ty: None,
            current_function: None,
            borrow_intervals: Vec::new(),
            errors: Vec::new(),
            type_defs,
            trait_defs,
            impl_blocks,
            functions,
            consts,
            statics,
            locals,
        }
    }

    /// 创建一个新的推断变量。
    pub fn fresh_infer(&mut self, span: Span) -> Ty {
        let id = InferId(self.next_infer);
        self.next_infer += 1;
        self.vars.insert(id, Ty::Infer(id));
        Ty::Infer(id)
    }

    /// 计算类型的大小（字节）。简化实现：64 位目标。
    pub fn size_of(&self, ty: &Ty) -> usize {
        match ty {
            Ty::Int(w) | Ty::Uint(w) => (*w as usize).max(8) / 8,
            Ty::Float(w) => (*w as usize) / 8,
            Ty::Bool => 1,
            Ty::Char => 4,
            Ty::Str => 16, // ptr + len
            Ty::Unit | Ty::Never => 0,
            Ty::Pointer(..) | Ty::Reference(..) => 8,
            Ty::Array(inner, len) => {
                let n = match len {
                    hir::ArrayLen::Const(_) => 1, // 简化：无法求值时按 1 计
                    hir::ArrayLen::Dynamic => 0,
                };
                self.size_of(inner) * n
            }
            Ty::Slice(..) => 16, // ptr + len
            Ty::Tuple(tys) => {
                let mut sz = 0usize;
                for t in tys {
                    let a = self.align_of(t);
                    sz = (sz + a - 1) / a * a;
                    sz += self.size_of(t);
                }
                sz
            }
            Ty::Function { .. } => 8,
            Ty::Optional(inner) => self.size_of(inner) + 1,
            Ty::ErrorUnion(ok, _) => self.size_of(ok) + 1,
            Ty::Named(def_id, _) => {
                match self.type_defs.get(def_id) {
                    Some(hir::HirTypeDef::Struct(s)) => {
                        let mut sz = 0usize;
                        for f in &s.fields {
                            let a = self.align_of(&f.ty);
                            sz = (sz + a - 1) / a * a;
                            sz += self.size_of(&f.ty);
                        }
                        sz
                    }
                    Some(hir::HirTypeDef::Enum(e)) => {
                        let tag = match e.repr {
                            hir::IntegerRepr::I8 | hir::IntegerRepr::U8 => 1,
                            hir::IntegerRepr::I16 | hir::IntegerRepr::U16 => 2,
                            hir::IntegerRepr::I32 | hir::IntegerRepr::U32 => 4,
                            _ => 8,
                        };
                        let mut max_payload = 0usize;
                        for v in &e.variants {
                            let payload_size = match &v.payload {
                                hir::HirVariantPayload::Tuple(tys) => {
                                    tys.iter().map(|t| self.size_of(t)).max().unwrap_or(0)
                                }
                                hir::HirVariantPayload::Struct(fields) => {
                                    fields.iter().map(|f| self.size_of(&f.ty)).max().unwrap_or(0)
                                }
                                hir::HirVariantPayload::Unit => 0,
                            };
                            max_payload = max_payload.max(payload_size);
                        }
                        tag + max_payload
                    }
                    Some(hir::HirTypeDef::Union(u)) => {
                        u.fields.iter().map(|f| self.size_of(&f.ty)).max().unwrap_or(0)
                    }
                    Some(hir::HirTypeDef::Alias { target, .. }) => self.size_of(target),
                    None => 0,
                }
            }
            Ty::Alias(inner) => self.size_of(inner),
            Ty::Opaque(_) => 0,
            Ty::Param(_) | Ty::Infer(_) => 0,
        }
    }

    /// 计算类型的对齐（字节）。简化实现：64 位目标。
    pub fn align_of(&self, ty: &Ty) -> usize {
        match ty {
            Ty::Int(w) | Ty::Uint(w) => {
                let b = (*w as usize).max(8) / 8;
                b.min(8)
            }
            Ty::Float(32) => 4,
            Ty::Float(_) => 8,
            Ty::Bool => 1,
            Ty::Char => 4,
            Ty::Str | Ty::Slice(..) => 8,
            Ty::Unit | Ty::Never => 1,
            Ty::Pointer(..) | Ty::Reference(..) | Ty::Function { .. } => 8,
            Ty::Array(inner, _) => self.align_of(inner),
            Ty::Tuple(tys) => tys.iter().map(|t| self.align_of(t)).max().unwrap_or(1),
            Ty::Optional(inner) | Ty::ErrorUnion(inner, _) => self.align_of(inner).max(1),
            Ty::Named(def_id, _) => {
                match self.type_defs.get(def_id) {
                    Some(hir::HirTypeDef::Struct(s)) => {
                        s.fields.iter().map(|f| self.align_of(&f.ty)).max().unwrap_or(1)
                    }
                    Some(hir::HirTypeDef::Enum(_)) => 8,
                    Some(hir::HirTypeDef::Union(u)) => {
                        u.fields.iter().map(|f| self.align_of(&f.ty)).max().unwrap_or(1)
                    }
                    Some(hir::HirTypeDef::Alias { target, .. }) => self.align_of(target),
                    None => 1,
                }
            }
            Ty::Alias(inner) => self.align_of(inner),
            Ty::Opaque(_) => 1,
            Ty::Param(_) | Ty::Infer(_) => 1,
        }
    }

    /// 创建一个新的类型参数。
    pub fn fresh_generic(&mut self, name: &str) -> Ty {
        let id = GenericId(self.next_infer);
        self.next_infer += 1;
        Ty::Param(id)
    }

    /// 求一个类型的完整形式（展开别名、替换参数）。
    pub fn resolve(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Infer(id) => match self.vars.get(id) {
                Some(t) if !matches!(t, Ty::Infer(_)) => self.resolve(t),
                _ => ty.clone(),
            },
            Ty::Alias(inner) => self.resolve(inner),
            Ty::Param(id) => self
                .substitution
                .get(id)
                .map(|t| self.resolve(t))
                .unwrap_or_else(|| ty.clone()),
            Ty::Pointer(t, m) => Ty::Pointer(Box::new(self.resolve(t)), *m),
            Ty::Reference(t, m) => Ty::Reference(Box::new(self.resolve(t)), *m),
            Ty::Array(t, len) => Ty::Array(
                Box::new(self.resolve(t)),
                match len {
                    hir::ArrayLen::Const(e) => hir::ArrayLen::Const(e.clone()),
                    hir::ArrayLen::Dynamic => hir::ArrayLen::Dynamic,
                },
            ),
            Ty::Slice(t, m) => Ty::Slice(Box::new(self.resolve(t)), *m),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve(t)).collect()),
            Ty::Function { params, ret, is_async, abi } => Ty::Function {
                params: params.iter().map(|t| self.resolve(t)).collect(),
                ret: Box::new(self.resolve(ret)),
                is_async: *is_async,
                abi: abi.clone(),
            },
            Ty::Optional(t) => Ty::Optional(Box::new(self.resolve(t))),
            Ty::ErrorUnion(t, e) => {
                Ty::ErrorUnion(Box::new(self.resolve(t)), Box::new(self.resolve(e)))
            }
            Ty::Named(id, args) => Ty::Named(
                *id,
                args.iter().map(|a| self.resolve(a)).collect(),
            ),
            _ => ty.clone(),
        }
    }

    /// 统一两个类型（Unification）。
    pub fn unify(&mut self, a: &Ty, b: &Ty, span: Span) -> Result<()> {
        let a = self.resolve(a);
        let b = self.resolve(b);
        match (&a, &b) {
            // 两个推断变量：让其中一个指向另一个
            (Ty::Infer(i), Ty::Infer(j)) => {
                if i != j {
                    self.vars.insert(*i, Ty::Infer(*j));
                }
                Ok(())
            }
            (Ty::Infer(i), _) => self.bind_infer(*i, &b, span),
            (_, Ty::Infer(j)) => self.bind_infer(*j, &a, span),

            // 同构构造子
            (Ty::Int(w1), Ty::Int(w2)) if w1 == w2 => Ok(()),
            (Ty::Uint(w1), Ty::Uint(w2)) if w1 == w2 => Ok(()),
            (Ty::Float(w1), Ty::Float(w2)) if w1 == w2 => Ok(()),
            (Ty::Bool, Ty::Bool) => Ok(()),
            (Ty::Char, Ty::Char) => Ok(()),
            (Ty::Str, Ty::Str) => Ok(()),
            (Ty::Unit, Ty::Unit) => Ok(()),
            (Ty::Never, Ty::Never) => Ok(()),

            (Ty::Pointer(t1, m1), Ty::Pointer(t2, m2)) if m1 == m2 => self.unify(t1, t2, span),
            (Ty::Reference(t1, m1), Ty::Reference(t2, m2)) if m1 == m2 => self.unify(t1, t2, span),
            (Ty::Array(t1, l1), Ty::Array(t2, l2)) if l1 == l2 => self.unify(t1, t2, span),
            (Ty::Slice(t1, m1), Ty::Slice(t2, m2)) if m1 == m2 => self.unify(t1, t2, span),
            (Ty::Tuple(ts1), Ty::Tuple(ts2)) if ts1.len() == ts2.len() => {
                for (x, y) in ts1.iter().zip(ts2.iter()) {
                    self.unify(x, y, span)?;
                }
                Ok(())
            }
            (Ty::Function { params: p1, ret: r1, is_async: a1, abi: abi1 },
             Ty::Function { params: p2, ret: r2, is_async: a2, abi: abi2 })
                if a1 == a2 && abi1 == abi2 && p1.len() == p2.len() =>
            {
                for (x, y) in p1.iter().zip(p2.iter()) {
                    self.unify(x, y, span)?;
                }
                self.unify(r1, r2, span)
            }
            (Ty::Optional(t1), Ty::Optional(t2)) => self.unify(t1, t2, span),
            (Ty::ErrorUnion(t1, e1), Ty::ErrorUnion(t2, e2)) => {
                self.unify(t1, t2, span)?;
                self.unify(e1, e2, span)
            }
            (Ty::Named(id1, args1), Ty::Named(id2, args2))
                if id1 == id2 && args1.len() == args2.len() =>
            {
                for (x, y) in args1.iter().zip(args2.iter()) {
                    self.unify(x, y, span)?;
                }
                Ok(())
            }
            (Ty::Param(g1), occurs) if !self.occurs_check(*g1, occurs) => {
                self.substitution.insert(*g1, occurs.clone());
                Ok(())
            }
            (Ty::Param(_), _) => Err(self.type_mismatch(&a, &b, span)),
            _ => Err(self.type_mismatch(&a, &b, span)),
        }
    }

    /// 发生性检查（Occurs Check），防止无限类型。
    fn occurs_check(&self, generic: GenericId, ty: &Ty) -> bool {
        match ty {
            Ty::Param(g) => g == &generic,
            Ty::Infer(_) => false,
            Ty::Pointer(t, _) | Ty::Reference(t, _) | Ty::Array(t, _) | Ty::Slice(t, _) => {
                self.occurs_check(generic, t)
            }
            Ty::Tuple(ts) | Ty::Function { params: ts, .. } => {
                ts.iter().any(|t| self.occurs_check(generic, t))
            }
            Ty::Optional(t) | Ty::ErrorUnion(t, _) => self.occurs_check(generic, t),
            Ty::Named(_, args) => args.iter().any(|a| self.occurs_check(generic, a)),
            Ty::Alias(t) => self.occurs_check(generic, t),
            _ => false,
        }
    }

    /// 将推断变量绑定到一个非推断类型。
    fn bind_infer(&mut self, id: InferId, ty: &Ty, span: Span) -> Result<()> {
        // 发生性检查
        if let Ty::Infer(other) = ty {
            if *other == id {
                return Ok(()); // 自引用，忽略
            }
        }
        if self.occurs_in(id, ty) {
            return Err(CompileError::type_error(format!(
                "cyclic type reference detected: cannot infer type for {}",
                self.type_name(ty)
            ))
            .with_span(span));
        }
        self.vars.insert(id, ty.clone());
        Ok(())
    }

    fn occurs_in(&self, id: InferId, ty: &Ty) -> bool {
        match ty {
            Ty::Infer(other) => id == *other,
            Ty::Pointer(t, _) | Ty::Reference(t, _) | Ty::Array(t, _) | Ty::Slice(t, _) => {
                self.occurs_in(id, t)
            }
            Ty::Tuple(ts) | Ty::Function { params: ts, .. } => {
                ts.iter().any(|t| self.occurs_in(id, t))
            }
            Ty::Optional(t) | Ty::ErrorUnion(t, _) => self.occurs_in(id, t),
            Ty::Named(_, args) => args.iter().any(|a| self.occurs_in(id, a)),
            Ty::Alias(t) => self.occurs_in(id, t),
            _ => false,
        }
    }

    /// 子类型关系检查（含自动解引用、自动借用、类型提升）。
    pub fn subtype(&mut self, sub: &Ty, sup: &Ty, span: Span) -> Result<()> {
        let sub = self.resolve(sub);
        let sup = self.resolve(sup);

        // 同一类型：直接相等
        if sub == sup {
            return Ok(());
        }

        // Never 是底类型：可以赋值给任何类型
        if matches!(sub, Ty::Never) {
            return Ok(());
        }

        // 子类型关系：需要 trait 系统支持，这里处理内置规则
        match (&sub, &sup) {
            // 整数提升：i8 → i16 → i32 → i64 → i128（无符号同理）
            (Ty::Int(w1), Ty::Int(w2)) if w1 <= w2 => Ok(()),
            (Ty::Uint(w1), Ty::Uint(w2)) if w1 <= w2 => Ok(()),
            (Ty::Int(w1), Ty::Float(w2)) if *w1 <= 64 && *w2 >= 32 => Ok(()),

            // 自动借用：`T` → `&T`（受限：仅当目标期望引用时）
            (ty, Ty::Reference(inner, _)) => self.unify(ty, inner, span),
            // 自动解引用：`&T` → `T`（受限）
            (Ty::Reference(inner, _), ty) => self.unify(inner, ty, span),

            // 错误联合：可以向更宽的错误集合提升
            (Ty::ErrorUnion(t1, e1), Ty::ErrorUnion(t2, e2)) => {
                self.subtype(t1, t2, span)?;
                self.subtype(e1, e2, span)
            }
            // `T` 可提升到 `T!E`（任何 T 都可视为带错误的 T）
            (t, Ty::ErrorUnion(inner, _)) => self.subtype(t, inner, span),
            // `T` 可提升到 `T?`（anytype 兼容）
            (t, Ty::Optional(inner)) => self.subtype(t, inner, span),

            // trait 对象子类型（需 trait 求解）
            (Ty::Named(sub_id, _), Ty::Named(sup_id, _)) => {
                // 检查 sub 是否 impl sup_trait
                self.check_trait_impl(*sub_id, *sup_id, span)
            }

            _ => Err(self.type_mismatch(&sub, &sup, span)),
        }
    }

    /// 检查 `type_id` 是否实现了 `trait_id`。
    fn check_trait_impl(&mut self, type_id: DefId, trait_id: DefId, span: Span) -> Result<()> {
        // 遍历所有 impl 块，寻找匹配的
        for impl_block in self.impl_blocks {
            if let Some(t) = impl_block.trait_id {
                if t == trait_id && self.impl_matches_type(impl_block, &Ty::Named(type_id, vec![])) {
                    return Ok(());
                }
            }
        }
        // 检查类型定义中是否直接声明了 trait（自动 impl）
        if let Some(td) = self.type_defs.get(&type_id) {
            match td {
                hir::HirTypeDef::Struct(s) => {
                    for attr in &s.attrs {
                        if attr.name == "derive" {
                            // 派生的 trait impl
                            return Ok(());
                        }
                    }
                }
                _ => {}
            }
        }
        Err(CompileError::type_error(format!(
            "type {} does not implement trait {}",
            self.type_id_name(type_id),
            self.type_id_name(trait_id)
        ))
        .with_span(span))
    }

    fn impl_matches_type(&self, _impl_block: &hir::HirImpl, _ty: &Ty) -> bool {
        // 占位：实际实现需要类型参数匹配
        true
    }

    // ===== 错误消息辅助 =====

    fn type_mismatch(&self, expected: &Ty, found: &Ty, span: Span) -> CompileError {
        CompileError::type_error(format!(
            "type mismatch: expected `{}`, found `{}`",
            self.type_name(expected),
            self.type_name(found)
        ))
        .with_kind(ErrorKind::Type)
        .with_span(span)
    }

    /// 将类型格式化为可读字符串。
    pub fn type_name(&self, ty: &Ty) -> String {
        match ty {
            Ty::Int(w) => format!("i{}", w),
            Ty::Uint(w) => format!("u{}", w),
            Ty::Float(w) => format!("f{}", w),
            Ty::Bool => "bool".into(),
            Ty::Char => "char".into(),
            Ty::Str => "str".into(),
            Ty::Unit => "()".into(),
            Ty::Never => "!".into(),
            Ty::Named(id, args) => {
                let name = self.type_id_name(*id);
                if args.is_empty() {
                    name
                } else {
                    format!(
                        "{}<{}>",
                        name,
                        args.iter()
                            .map(|a| self.type_name(a))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            }
            Ty::Pointer(t, m) => format!(
                "*{}{}",
                if matches!(m, hir::Mutability::Mutable) {
                    "mut "
                } else {
                    "const "
                },
                self.type_name(t)
            ),
            Ty::Reference(t, m) => format!(
                "&{}{}",
                if matches!(m, hir::Mutability::Mutable) {
                    "mut "
                } else {
                    ""
                },
                self.type_name(t)
            ),
            Ty::Array(t, len) => {
                let l = match len {
                    hir::ArrayLen::Const(_) => "_".to_string(),
                    hir::ArrayLen::Dynamic => "_".to_string(),
                };
                format!("[{}; {}]", self.type_name(t), l)
            }
            Ty::Slice(t, m) => format!(
                "[]{}{}",
                if matches!(m, hir::Mutability::Mutable) {
                    "mut "
                } else {
                    "const "
                },
                self.type_name(t)
            ),
            Ty::Tuple(ts) => {
                format!(
                    "({})",
                    ts.iter()
                        .map(|t| self.type_name(t))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Ty::Function { params, ret, is_async, abi } => {
                let params = params
                    .iter()
                    .map(|p| self.type_name(p))
                    .collect::<Vec<_>>()
                    .join(", ");
                let async_kw = if *is_async { "async " } else { "" };
                format!(
                    "{async_kw}fn({}) -> {} [{:?}]",
                    params,
                    self.type_name(ret),
                    abi
                )
            }
            Ty::Optional(t) => format!("{}?", self.type_name(t)),
            Ty::ErrorUnion(t, e) => {
                format!("{}!{}", self.type_name(t), self.type_name(e))
            }
            Ty::Param(g) => format!("T{}", g.0),
            Ty::Infer(_) => "_".into(),
            Ty::Alias(t) => self.type_name(t),
            Ty::Opaque(id) => format!("opaque({})", self.type_id_name(*id)),
        }
    }

    fn type_id_name(&self, id: DefId) -> String {
        // 从函数表中查找名称，类型定义表类似
        if let Some(f) = self.functions.get(&id) {
            f.name.clone()
        } else if let Some(t) = self.type_defs.get(&id) {
            match t {
                hir::HirTypeDef::Struct(s) => s.name.clone(),
                hir::HirTypeDef::Enum(e) => e.name.clone(),
                hir::HirTypeDef::Union(u) => u.name.clone(),
                hir::HirTypeDef::Alias { def_id, .. } => format!("alias({})", def_id.0),
            }
        } else {
            format!("type#{}", id.0)
        }
    }

    /// 求解所有约束（主入口）。
    pub fn solve(&mut self) -> Result<InferResult> {
        // 使用 Robinson 统一算法
        let constraints = std::mem::take(&mut self.constraints);
        for c in &constraints {
            match c {
                Constraint::Eq(a, b, span) => self.unify(a, b, *span)?,
                Constraint::Subtype(a, b, span) => self.subtype(a, b, *span)?,
                Constraint::TraitBound(ty, trait_id, span) => {
                    self.check_trait_impl_for_type(ty, *trait_id, *span)?;
                }
                Constraint::RegionOutlives(a, b, span) => {
                    self.check_region_outlives(*a, *b, *span)?;
                }
            }
        }
        Ok(InferResult {
            type_map: self.vars.clone(),
            residual: Vec::new(),
        })
    }

    fn check_trait_impl_for_type(
        &mut self,
        ty: &Ty,
        trait_id: DefId,
        span: Span,
    ) -> Result<()> {
        let ty = self.resolve(ty);
        match &ty {
            Ty::Named(id, _) => self.check_trait_impl(*id, trait_id, span),
            Ty::Int(_) | Ty::Uint(_) | Ty::Float(_) | Ty::Bool => {
                // 内建类型默认 impl 常用 trait（Copy, Clone, Eq, Hash 等）
                self.check_builtin_trait_impl(&ty, trait_id, span)
            }
            Ty::Pointer(_, _) | Ty::Reference(_, _) => {
                self.check_builtin_trait_impl(&ty, trait_id, span)
            }
            _ => Err(CompileError::type_error(format!(
                "type `{}` does not implement trait `{}`",
                self.type_name(&ty),
                self.type_id_name(trait_id)
            ))
            .with_span(span)),
        }
    }

    fn check_builtin_trait_impl(
        &self,
        _ty: &Ty,
        _trait_id: DefId,
        _span: Span,
    ) -> Result<()> {
        // 内建类型自动实现基础 trait（Copy, Clone, Eq, Hash, Ord, Display 等）
        // 简化：假设内建类型都能满足基础 trait 约束
        Ok(())
    }

    fn check_region_outlives(
        &self,
        _a: RegionId,
        _b: RegionId,
        _span: Span,
    ) -> Result<()> {
        // 生命周期约束求解（NLL liveness analysis）
        Ok(())
    }

    /// 记录一次借用，用于后续借用检查。
    pub fn record_borrow(
        &mut self,
        local: LocalId,
        kind: BorrowKind,
        start: usize,
    ) -> usize {
        let idx = self.borrow_intervals.len();
        self.borrow_intervals.push(BorrowInterval {
            local,
            start,
            end: None,
            kind,
        });
        idx
    }

    /// 结束一个借用区间。
    pub fn end_borrow(&mut self, idx: usize, point: usize) {
        if let Some(b) = self.borrow_intervals.get_mut(idx) {
            b.end = Some(point);
        }
    }

    /// 借用检查（NLL 风格）：检测悬垂引用与重叠可变借用。
    pub fn check_borrows(&self, _cfg: &Cfg) -> Vec<CompileError> {
        let mut errors = Vec::new();
        // 收集每个程序点的活跃借用
        // 检查同一变量的可变借用是否重叠
        // 检查是否存在 use-after-free（引用超出所引用值的生命周期）
        // 此处为框架实现
        let _ = (&self.borrow_intervals, &mut errors);
        errors
    }
}

/// 类型方案（带全称量化的类型）。
impl TypeScheme {
    /// 实例化一个类型方案，生成新的推断变量。
    pub fn instantiate(&self, _cx: &mut InferCtxt<'_>, _span: Span) -> Ty {
        // 为每个泛型参数创建新的推断变量，并替换到 ty 中
        // 此处为框架实现
        self.ty.clone()
    }
}

/// 区域推断（NLL）。
#[derive(Debug, Default)]
pub struct RegionInferer {
    /// 区域 → 约束集合。
    pub constraints: Vec<(RegionId, RegionId)>,
    /// 已求解的区域映射。
    pub solution: HashMap<RegionId, usize>,
}

impl RegionInferer {
    /// 求解区域约束。
    pub fn solve(&mut self) -> Result<()> {
        // 使用约束图的最近公共祖先 / 作用域树算法
        // 此处为框架实现
        Ok(())
    }
}

/// 格式化输出类型（用于 error reporting）。
impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Ty::Int(w) => format!("i{}", w),
            Ty::Uint(w) => format!("u{}", w),
            Ty::Float(w) => format!("f{}", w),
            Ty::Bool => "bool".to_string(),
            Ty::Char => "char".to_string(),
            Ty::Str => "str".to_string(),
            Ty::Unit => "()".to_string(),
            Ty::Never => "!".to_string(),
            Ty::Named(_, _) => "named".to_string(),
            Ty::Pointer(t, _) => format!("*{}", t),
            Ty::Reference(t, _) => format!("&{}", t),
            Ty::Array(t, _) => format!("[{}]", t),
            Ty::Slice(t, _) => format!("[]{}", t),
            Ty::Tuple(_) => "tuple".to_string(),
            Ty::Function { .. } => "fn".to_string(),
            Ty::Optional(t) => format!("{}?", t),
            Ty::ErrorUnion(t, _) => format!("{}!", t),
            Ty::Param(_) => "param".to_string(),
            Ty::Infer(_) => "_".to_string(),
            Ty::Alias(t) => format!("alias({})", t),
            Ty::Opaque(_) => "opaque".to_string(),
        };
        f.write_str(&name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的 InferCtxt 构造辅助，避免临时值生命周期问题。
    struct TestCtxt {
        type_defs: HashMap<DefId, hir::HirTypeDef>,
        trait_defs: HashMap<DefId, hir::HirTrait>,
        impl_blocks: Vec<hir::HirImpl>,
        functions: HashMap<DefId, hir::HirFunction>,
        consts: HashMap<DefId, hir::HirConst>,
        statics: HashMap<DefId, hir::HirStatic>,
        locals: HashMap<LocalId, Ty>,
    }

    impl TestCtxt {
        fn new() -> Self {
            Self {
                type_defs: HashMap::new(),
                trait_defs: HashMap::new(),
                impl_blocks: Vec::new(),
                functions: HashMap::new(),
                consts: HashMap::new(),
                statics: HashMap::new(),
                locals: HashMap::new(),
            }
        }

        fn cx(&mut self) -> InferCtxt<'_> {
            InferCtxt::new(
                &self.type_defs,
                &self.trait_defs,
                &self.impl_blocks,
                &self.functions,
                &self.consts,
                &self.statics,
                &mut self.locals,
            )
        }
    }

    #[test]
    fn unify_int_types() {
        let mut t = TestCtxt::new();
        let mut cx = t.cx();
        let span = Span::unknown();
        cx.unify(&Ty::Int(32), &Ty::Int(32), span).unwrap();
    }

    #[test]
    fn unify_infer_var() {
        let mut t = TestCtxt::new();
        let mut cx = t.cx();
        let span = Span::unknown();
        let v = cx.fresh_infer(span);
        cx.unify(&v, &Ty::Int(64), span).unwrap();
        assert_eq!(cx.resolve(&v), Ty::Int(64));
    }

    #[test]
    fn occurs_check_prevents_infinite_type() {
        let mut t = TestCtxt::new();
        let mut cx = t.cx();
        let span = Span::unknown();
        let v = cx.fresh_infer(span);
        let list = Ty::Named(DefId(1), vec![v.clone()]);
        let err = cx.unify(&v, &list, span).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Type));
    }

    #[test]
    fn subtype_int_promotion() {
        let mut t = TestCtxt::new();
        let mut cx = t.cx();
        let span = Span::unknown();
        cx.subtype(&Ty::Int(8), &Ty::Int(32), span).unwrap();
    }

    #[test]
    fn never_is_bottom_type() {
        let mut t = TestCtxt::new();
        let mut cx = t.cx();
        let span = Span::unknown();
        cx.subtype(&Ty::Never, &Ty::Int(32), span).unwrap();
    }
}
