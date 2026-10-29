//! `#[derive(..)]` 的方法体生成。
//!
//! 早先 `#[derive(PartialEq/Eq/Hash/Ord/Default)]` 只往 `impl_blocks` 里登记
//! 一个"trait 标记"：`fn needs_eq<T: Eq>(x: T)` 这类边界校验能过，但
//! `.clone()` / `.hash()` / `.default()` 这些**方法不存在**，`==` 更是直接
//! 走到 MIR 的标量比较上——聚合值在运行期是地址，两个内容相同的值比较的
//! 却是两个不同的地址，于是 `a == b` 恒假（静默算错，不报错）。
//!
//! 这里在 **AST 层**补出真正的方法体。放在 AST 而不是 HIR 的理由：
//! impl 方法本来就是在 `check_program` 开头被展开成普通函数 `Type__method`
//! 再走后续全部阶段的（`impl_targets` 记录目标类型），derive 只需要产出
//! 同样形状的 `ast::Function`，类型检查 / MIR / 后端都无需知道它来自 derive。
//!
//! 生成的函数与 `std/core.zr` 里的 trait 声明签名一致（注意那些 trait 的
//! receiver 是**按值**的 `self`，不是 `&self`）：
//!
//! | derive | 方法 |
//! |---|---|
//! | `Clone` | `fn Type__clone(self) -> Self` |
//! | `Default` | `fn Type__default() -> Self`（仅结构体） |
//! | `PartialEq` / `Eq` | `fn Type__eq(self, other: Self) -> bool` |
//! | `PartialOrd` / `Ord` | `lt` / `le` / `gt` / `ge`（仅结构体） |
//! | `Hash` | `fn Type__hash(self) -> u64` |
//!
//! `Copy` 是纯标记 trait，不生成方法。`Debug` 仍是标记：它要先把值渲染成
//! 字符串，缺一套运行期格式化设施（见 `docs/rust-parity.md`）。
//!
//! ## 已知限定（都选择"显式报错"而不是静默编出错的代码）
//!
//! - 泛型类型上的 derive 不生成方法（方法体里的字段类型要靠单态化才知道），
//!   报错让调用方知道；
//! - 枚举不支持 `Default`（Rust 要求用 `#[default]` 标出变体，zr 没有这个
//!   属性）与 `PartialOrd` / `Ord`（要按判别式排序，判别式可以是显式且非
//!   单调的表达式）；
//! - 结构体更新语法之外，`Hash` 对 `str` / 切片 / 数组 / 元组 / 指针字段一律
//!   按 `0` 计入：只保证"相等 ⇒ 同哈希"，不保证分布。这几类字段在 zr 里还
//!   没有按内容比较的语义，硬编一个哈希反而会在 eq/hash 之间制造不一致。

use crate::error::{CompileError, Result};
use crate::parser::ast::{
    self, Argument, AssignOp, BinaryOp, Block, Expression, FieldPat, Function, Literal, Mutability,
    Param, Pattern, Receiver, Span, Statement, Type, VariantPayload,
};

/// 生成代码没有源码位置。所有派生方法的诊断都落在同一点上，但错误文案里
/// 会带上类型名与方法名，足够定位。
const SPAN: Span = Span::unknown();

/// 字段名与字段类型。
struct FieldInfo {
    name: String,
    ty: Type,
    /// 字段名所在源码区间。生成 `self.field` 访问时用它做诊断位置，让
    /// "某字段缺 trait"的报错指向**那个字段**而不是整个类型定义。
    span: Span,
}

/// 为某个带 `#[derive(..)]` 的类型生成方法体。
///
/// 只生成**能在本类型上正确实现**的方法；不能实现又确实被 `derive` 要求的
/// 组合直接报错（见模块文档的"已知限定"）。不认识的 trait 名不在这里报错，
/// 交给 `generate_derive_impls` 统一报 "unknown derive trait"。
pub fn derive_methods(type_name: &str, type_def: &ast::TypeDef, traits: &[String]) -> Result<Vec<Function>> {
    // derive 生成的比较/哈希方法在诊断里用类型定义的位置指出"哪个类型缺
    // trait"，避免报 `0:0:0`（`SPAN`）。
    let span = match type_def {
        ast::TypeDef::Struct(s) => s.span,
        ast::TypeDef::Enum(e) => e.span,
        _ => SPAN,
    };
    let mut out = Vec::new();
    // 先按 trait 名推出这次要生成哪几个方法，避免重复生成（`PartialEq` 与
    // `Eq` 都要求 `eq`，`PartialOrd` 与 `Ord` 都要求 `lt/le/gt/ge`）。
    let want_clone = traits.iter().any(|t| t == "Clone");
    let want_default = traits.iter().any(|t| t == "Default");
    let want_eq = traits.iter().any(|t| t == "PartialEq" || t == "Eq");
    let want_ord = traits.iter().any(|t| t == "PartialOrd" || t == "Ord");
    let want_hash = traits.iter().any(|t| t == "Hash");
    if !(want_clone || want_default || want_eq || want_ord || want_hash) {
        return Ok(out);
    }

    // 泛型类型上的 derive：方法体里每个字段的具体类型都要等单态化才知道，
    // 生成的 `Self` 也不唯一。明确报错，别生成一堆按推断变量检查的假代码。
    let generics = match type_def {
        ast::TypeDef::Struct(s) => &s.generics,
        ast::TypeDef::Enum(e) => &e.generics,
        _ => return Ok(out),
    };
    if !generics.is_empty() {
        return Err(CompileError::type_error(format!(
            "`#[derive(..)]` on generic type `{type_name}` is not supported yet"
        ))
        .with_span(span));
    }

    let self_ty = named(type_name);

    // 结构体：拿字段表。元组结构体的字段名就是下标字符串（`"0"` / `"1"`），
    // 与 HIR 的 `FieldIndex::Pos` 一致，字段访问 `self.0` 直接可用。
    // 枚举：拿变体表（判别式在这里用不到，见模块文档）。
    let (fields, variants) = match type_def {
        ast::TypeDef::Struct(s) => (
            Some(
                s.fields
                    .iter()
                    .enumerate()
                    .map(|(idx, f)| FieldInfo {
                        // 元组结构体的字段在 AST 里没有名字（空串），HIR 会补成
                        // 下标字符串——派生方法也必须用同一套名字，`.0` 才找得到。
                        name: if f.name.is_empty() {
                            idx.to_string()
                        } else {
                            f.name.clone()
                        },
                        ty: f.ty.clone(),
                        span: f.span,
                    })
                    .collect::<Vec<_>>(),
            ),
            None,
        ),
        ast::TypeDef::Enum(e) => (None, Some(e.variants.clone())),
        _ => return Ok(out),
    };

    if want_default {
        let Some(fields) = &fields else {
            return Err(CompileError::type_error(format!(
                "cannot derive `Default` for enum `{type_name}`: zr has no `#[default]` variant attribute"
            ))
            .with_span(span));
        };
        out.push(default_fn(type_name, &self_ty, fields)?);
    }
    if want_ord {
        if variants.is_some() {
            return Err(CompileError::type_error(format!(
                "cannot derive `Ord`/`PartialOrd` for enum `{type_name}` yet: \
                 ordering needs discriminant comparison, which derive does not implement"
            ))
            .with_span(span));
        }
    }
    if want_clone {
        out.push(clone_fn(type_name, &self_ty, fields.as_deref(), variants.as_deref(), span)?);
    }
    if want_eq {
        out.push(eq_fn(type_name, &self_ty, fields.as_deref(), variants.as_deref(), span)?);
    }
    if want_ord {
        let fields = fields.as_deref().unwrap_or(&[]);
        out.push(lt_fn(type_name, &self_ty, fields, span));
        out.push(le_fn(type_name, &self_ty, span));
        out.push(gt_fn(type_name, &self_ty, span));
        out.push(ge_fn(type_name, &self_ty, span));
    }
    if want_hash {
        out.push(hash_fn(type_name, &self_ty, fields.as_deref(), variants.as_deref(), span)?);
    }
    Ok(out)
}

// ----------------------------------------------------------------------
// 各方法的生成
// ----------------------------------------------------------------------

/// `fn Type__default() -> Type`：每个字段取零值 / 递归取 `__default()`。
fn default_fn(type_name: &str, self_ty: &Type, fields: &[FieldInfo]) -> Result<Function> {
    let mut args = Vec::with_capacity(fields.len());
    for f in fields {
        args.push(default_value(type_name, f)?);
    }
    Ok(make_fn(
        type_name,
        "default",
        None,
        Vec::new(),
        self_ty.clone(),
        construct(type_name, fields, args),
    ))
}

/// 单个字段的默认值表达式。
fn default_value(type_name: &str, f: &FieldInfo) -> Result<Expression> {
    let e = match &f.ty {
        Type::I(_) | Type::U(_) => int(0),
        Type::F(_) => Expression::Literal(Literal::Float(0.0, None), SPAN),
        Type::Bool => Expression::Literal(Literal::Bool(false), SPAN),
        Type::Char => Expression::Literal(Literal::Char('\0'), SPAN),
        Type::Str => Expression::Literal(Literal::String(String::new()), SPAN),
        // 命名类型（含 `T?` / `T!E` 这两个 `Option` / `Result` 的糖）递归取
        // 它自己的 `__default`。目标类型没 derive Default 时这里会报
        // "cannot find value `Inner__default`"——与 Rust 的
        // "the trait bound `Inner: Default` is not satisfied" 位置一致。
        Type::Named { path, .. } => {
            let last = path.last().cloned().unwrap_or_default();
            call_path_at(&[format!("{last}__default")], Vec::new(), f.span)
        }
        Type::Optional(inner) => {
            // `T?` 就是 `Option<T>`；Option 的 Default 是 `None`，而 zr 的
            // `Option` 是 std 里的枚举，没有 `#[default]` 可言——直接报错，
            // 别生成一个找不到的符号。
            let _ = inner;
            return Err(unsupported_default(type_name, f));
        }
        Type::ErrorUnion(..) => return Err(unsupported_default(type_name, f)),
        _ => return Err(unsupported_default(type_name, f)),
    };
    Ok(e)
}

fn unsupported_default(type_name: &str, f: &FieldInfo) -> CompileError {
    CompileError::type_error(format!(
        "cannot derive `Default` for `{type_name}`: field `{}` has no default value",
        f.name
    ))
    .with_span(f.span)
}

/// `fn Type__clone(self) -> Type`：字段逐个 `.clone()`（标量按位复制）。
fn clone_fn(
    type_name: &str,
    self_ty: &Type,
    fields: Option<&[FieldInfo]>,
    variants: Option<&[ast::Variant]>,
    _span: Span,
) -> Result<Function> {
    if let Some(fields) = fields {
        let args = fields
            .iter()
            .map(|f| clone_value(field_at(self_expr(), &f.name, f.span), &f.ty))
            .collect();
        return Ok(make_fn(
            type_name,
            "clone",
            Some(receiver(self_ty)),
            Vec::new(),
            self_ty.clone(),
            construct(type_name, fields, args),
        ));
    }
    let variants = variants.unwrap_or(&[]);
    Ok(make_fn(
        type_name,
        "clone",
        Some(receiver(self_ty)),
        Vec::new(),
        self_ty.clone(),
        variant_match(type_name, variants, |v, binds| {
            let args = binds
                .iter()
                .zip(payload_types(v))
                .map(|(b, ty)| clone_value(var(b), &ty))
                .collect();
            variant_build(type_name, v, args)
        }),
    ))
}

/// 字段/载荷的克隆表达式：命名类型递归 `.clone()`，其余按位复制。
///
/// 只对命名类型递归的理由：标量 / `str` / 指针 / 切片 / 元组都没有
/// `clone` 方法，写 `.clone()` 会报"字段上找不到方法"；而按位复制对它们就是
/// 正确的语义（`str` 是 `{ptr, len}`，切片是 `{ptr, len}`）。
fn clone_value(e: Expression, ty: &Type) -> Expression {
    if is_named_like(ty) {
        method_call(e, "clone", Vec::new())
    } else {
        e
    }
}

/// `fn Type__eq(self, other: Type) -> bool`。
fn eq_fn(
    type_name: &str,
    self_ty: &Type,
    fields: Option<&[FieldInfo]>,
    variants: Option<&[ast::Variant]>,
    span: Span,
) -> Result<Function> {
    let body = if let Some(fields) = fields {
        // `f1 == g1 && f2 == g2 && ...`（零字段恒等）。
        let mut acc: Option<Expression> = None;
        for f in fields {
            let cmp = bin(
                field_at(self_expr(), &f.name, f.span),
                BinaryOp::Eq,
                field_at(var("other"), &f.name, f.span),
                f.span,
            );
            acc = Some(match acc {
                Some(prev) => bin(prev, BinaryOp::And, cmp, span),
                None => cmp,
            });
        }
        acc.unwrap_or_else(|| Expression::Literal(Literal::Bool(true), span))
    } else {
        let variants = variants.unwrap_or(&[]);
        // 第一层按 `self` 的变体分派，第二层在同一个变体内比较载荷：
        // 变体不同直接 false。载荷字段用 `==`（命名类型会被类型检查器改写
        // 成递归的 `__eq` 调用，见 `check_binary`）。
        variant_match(type_name, variants, |v, sbinds| {
            let arms = variants
                .iter()
                .map(|w| {
                    let pattern = variant_pattern(type_name, w, "o");
                    let body = if v.name == w.name {
                        let obinds = payload_binds(w, "o");
                        let ospans = payload_bind_spans(w, "o");
                        let mut acc: Option<Expression> = None;
                        for (k, (a, b)) in sbinds.iter().zip(obinds.iter()).enumerate() {
                            // 结构体载荷的字段 span 让报错指向那个字段；元组
                            // 载荷没有字段名，回退到枚举定义的位置。
                            let fspan = ospans
                                .get(k)
                                .map(|(_, s)| *s)
                                .filter(|s| *s != SPAN)
                                .unwrap_or(span);
                            let cmp = bin(var(a), BinaryOp::Eq, var(b), fspan);
                            acc = Some(match acc {
                                Some(prev) => bin(prev, BinaryOp::And, cmp, fspan),
                                None => cmp,
                            });
                        }
                        acc.unwrap_or_else(|| Expression::Literal(Literal::Bool(true), span))
                    } else {
                        Expression::Literal(Literal::Bool(false), span)
                    };
                    ast::Arm {
                        pattern,
                        guard: None,
                        body: Box::new(body),
                    }
                })
                .collect();
            // 变体全覆盖，不需要 `_` 臂（穷尽性检查认这个）。
            Expression::Match(Box::new(var("other")), arms, SPAN)
        })
    };
    Ok(make_fn(
        type_name,
        "eq",
        Some(receiver(self_ty)),
        vec![param("other", self_ty)],
        Type::Bool,
        body,
    ))
}

/// `fn Type__lt(self, other: Type) -> bool`：字段按字典序比较。
///
/// `f1 < g1 || (f1 == g1 && (f2 < g2 || (f2 == g2 && ...)))`
fn lt_fn(type_name: &str, self_ty: &Type, fields: &[FieldInfo], span: Span) -> Function {
    let mut acc: Option<Expression> = None;
    for (i, f) in fields.iter().enumerate().rev() {
        let lt = bin(
            field_at(self_expr(), &f.name, f.span),
            BinaryOp::Lt,
            field_at(var("other"), &f.name, f.span),
            f.span,
        );
        acc = Some(if i == fields.len() - 1 {
            // 最后一个字段：没有"后面"要比较了，不必再拼 `==`。
            lt
        } else {
            let eq = bin(
                field_at(self_expr(), &f.name, f.span),
                BinaryOp::Eq,
                field_at(var("other"), &f.name, f.span),
                f.span,
            );
            let tail = acc.expect("非末字段一定有后续比较");
            bin(lt, BinaryOp::Or, bin(eq, BinaryOp::And, tail, span), span)
        });
    }
    let body = acc.unwrap_or_else(|| Expression::Literal(Literal::Bool(false), span));
    make_fn(
        type_name,
        "lt",
        Some(receiver(self_ty)),
        vec![param("other", self_ty)],
        Type::Bool,
        body,
    )
}

/// `fn Type__le(self, other) -> bool { self.lt(other) || self.eq(other) }`
///
/// 用组合而不是自己重写一遍字段比较：`lt` / `eq` 已经在别处生成，两者
/// 都必须与它们**完全**一致，否则 `a <= b` 与 `a < b || a == b` 会分家。
fn le_fn(type_name: &str, self_ty: &Type, span: Span) -> Function {
    let lt = method_call(self_expr(), "lt", vec![var("other")]);
    let eq = method_call(self_expr(), "eq", vec![var("other")]);
    make_fn(
        type_name,
        "le",
        Some(receiver(self_ty)),
        vec![param("other", self_ty)],
        Type::Bool,
        bin(lt, BinaryOp::Or, eq, span),
    )
}

/// `fn Type__gt(self, other) -> bool { !self.le(other) }`
fn gt_fn(type_name: &str, self_ty: &Type, span: Span) -> Function {
    let le = method_call(self_expr(), "le", vec![var("other")]);
    make_fn(
        type_name,
        "gt",
        Some(receiver(self_ty)),
        vec![param("other", self_ty)],
        Type::Bool,
        Expression::Unary(ast::UnaryOp::Not, Box::new(le), span),
    )
}

/// `fn Type__ge(self, other) -> bool { !self.lt(other) }`
fn ge_fn(type_name: &str, self_ty: &Type, span: Span) -> Function {
    let lt = method_call(self_expr(), "lt", vec![var("other")]);
    make_fn(
        type_name,
        "ge",
        Some(receiver(self_ty)),
        vec![param("other", self_ty)],
        Type::Bool,
        Expression::Unary(ast::UnaryOp::Not, Box::new(lt), span),
    )
}

/// `fn Type__hash(self) -> u64`。
///
/// 累积式：`h = h * 31 + 字段哈希`。乘法因子 31 与 Java 的 `String.hashCode`
/// 同源，小、奇数、字节友好；这里唯一要的保证是"相等 ⇒ 同哈希"。
fn hash_fn(
    type_name: &str,
    self_ty: &Type,
    fields: Option<&[FieldInfo]>,
    variants: Option<&[ast::Variant]>,
    span: Span,
) -> Result<Function> {
    // 枚举：把变体号也混进去，不同变体即使载荷都是零值也不同哈希。
    let mut stmts = vec![let_mut("h", Type::U(64), int(0))];
    if let Some(fields) = fields {
        for f in fields {
            let hv = hash_value(field_at(self_expr(), &f.name, f.span), &f.ty);
            stmts.push(accumulate_hash(hv, f.span));
        }
    } else {
        let variants = variants.unwrap_or(&[]);
        // 每个变体：`h = <变体下标>`，再逐个混入载荷字段。
        //
        // 这里不用 `match` 表达式返回值，而是让每个臂**写同一个 h**：
        // 载荷字段的个数随变体而变，用表达式返回就得为每个臂拼一个长度不同
        // 的式子，再在 `match` 之后继续累积——反而更绕。
        let arms = variants
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut arm_stmts = vec![Statement::Expr(
                    Expression::Assign(
                        Box::new(var("h")),
                        AssignOp::Assign,
                        Box::new(int(i as i128)),
                        span,
                    ),
                    span,
                )];
                for (k, ty) in payload_types(v).into_iter().enumerate() {
                    let binds = payload_binds(v, "p");
                    arm_stmts.push(accumulate_hash(hash_value(var(&binds[k]), &ty), span));
                }
                let block = Block {
                    stmts: arm_stmts,
                    tail: None,
                    span,
                };
                ast::Arm {
                    pattern: variant_pattern(type_name, v, "p"),
                    guard: None,
                    body: Box::new(Expression::Block(block, span)),
                }
            })
            .collect();
        stmts.push(Statement::Expr(
            Expression::Match(Box::new(self_expr()), arms, span),
            span,
        ));
    }
    let block = Block {
        stmts,
        tail: Some(Box::new(var("h"))),
        span,
    };
    Ok(make_fn(
        type_name,
        "hash",
        Some(receiver(self_ty)),
        Vec::new(),
        Type::U(64),
        Expression::Block(block, span),
    ))
}

/// `h = h * 31 + (值)`。
fn accumulate_hash(value: Expression, span: Span) -> Statement {
    let mixed = bin(
        bin(var("h"), BinaryOp::Mul, int(31), span),
        BinaryOp::Add,
        value,
        span,
    );
    Statement::Expr(
        Expression::Assign(Box::new(var("h")), AssignOp::Assign, Box::new(mixed), span),
        span,
    )
}

/// 单个字段/载荷的哈希值表达式。
fn hash_value(e: Expression, ty: &Type) -> Expression {
    match ty {
        // 标量与浮点：`as u64`。"相等 ⇒ 同哈希"仍然成立：取整是**值**的函数，
        // 相等的两个浮点取整结果相同。唯一一组"相等但位模式不同"的值是
        // `0.0` / `-0.0`，两者取整都是 0，正好不会分家（早先的注释说"位模式
        // 相同"是错的，结论对但理由不成立）。取整会丢小数部分，哈希有碰撞，
        // 这是允许的——只要求相等必同哈希。
        Type::I(_) | Type::U(_) | Type::F(_) | Type::Bool | Type::Char => cast(e, Type::U(64)),
        Type::Named { .. } => method_call(e, "hash", Vec::new()),
        // `str` / 切片 / 数组 / 元组 / 指针 / 引用：按 0 计（见模块文档）。
        _ => int(0),
    }
}

// ----------------------------------------------------------------------
// 结构体 / 枚举的公共骨架
// ----------------------------------------------------------------------

/// 结构体字面量（元组结构体走调用形式）：
/// `Type { f1: a1, f2: a2 }` / `Type(a0, a1)`。
fn construct(type_name: &str, fields: &[FieldInfo], args: Vec<Expression>) -> Expression {
    if is_tuple_struct(fields) {
        return call(type_name, args);
    }
    Expression::StructInit(
        vec![type_name.to_string()],
        fields
            .iter()
            .map(|f| f.name.clone())
            .zip(args)
            .collect(),
        None,
        SPAN,
    )
}

/// 字段名是 `0, 1, 2, ...` 且与下标一一对应 ⇒ 元组结构体。
fn is_tuple_struct(fields: &[FieldInfo]) -> bool {
    fields
        .iter()
        .enumerate()
        .all(|(i, f)| f.name == i.to_string())
}

/// 枚举变体模式：`E::V(b0, b1)` / `E::V { f: b0 }` / `E::V`。
fn variant_pattern(type_name: &str, v: &ast::Variant, prefix: &str) -> Pattern {
    let path = vec![type_name.to_string(), v.name.clone()];
    match &v.payload {
        VariantPayload::Unit => Pattern::Variant {
            path,
            args: Vec::new(),
            span: SPAN,
        },
        VariantPayload::Tuple(_) => Pattern::Variant {
            path,
            args: payload_binds(v, prefix).into_iter().map(|b| binding(&b)).collect(),
            span: SPAN,
        },
        VariantPayload::Struct(fields) => Pattern::Struct {
            path,
            fields: fields
                .iter()
                .map(|f| FieldPat {
                    name: f.name.clone(),
                    pattern: binding(&format!("{prefix}_{}", f.name)),
                })
                .collect(),
            rest: false,
            span: SPAN,
        },
    }
}

/// 变体载荷的绑定名。模式（`variant_pattern`）与臂体（读载荷）必须用同一套
/// 名字——元组载荷用 `前缀+下标`，结构体载荷用 `前缀_字段名`。
fn payload_binds(v: &ast::Variant, prefix: &str) -> Vec<String> {
    match &v.payload {
        VariantPayload::Unit => Vec::new(),
        VariantPayload::Tuple(tys) => (0..tys.len()).map(|i| format!("{prefix}{i}")).collect(),
        VariantPayload::Struct(fields) => fields
            .iter()
            .map(|f| format!("{prefix}_{}", f.name))
            .collect(),
    }
}

/// 变体载荷的绑定名 + 对应字段的 span。元组载荷没有字段名（fallback 到
/// `SPAN`，即枚举定义的位置）；结构体载荷用字段名所在位置。诊断时让
/// "某载荷字段缺 trait"的报错指向**那个字段**，而不是枚举定义整体。
fn payload_bind_spans(v: &ast::Variant, prefix: &str) -> Vec<(String, Span)> {
    match &v.payload {
        VariantPayload::Unit => Vec::new(),
        VariantPayload::Tuple(tys) => (0..tys.len())
            .map(|i| (format!("{prefix}{i}"), SPAN))
            .collect(),
        VariantPayload::Struct(fields) => fields
            .iter()
            .map(|f| (format!("{prefix}_{}", f.name), f.span))
            .collect(),
    }
}

/// 枚举变体构造：`E::V(a0)` / `E::V { f: a0 }` / `E::V`。
fn variant_build(type_name: &str, v: &ast::Variant, args: Vec<Expression>) -> Expression {
    let path = vec![type_name.to_string(), v.name.clone()];
    match &v.payload {
        VariantPayload::Unit => Expression::Path(path, SPAN),
        VariantPayload::Tuple(_) => call_path(&path, args),
        VariantPayload::Struct(fields) => Expression::StructInit(
            path,
            fields.iter().map(|f| f.name.clone()).zip(args).collect(),
            None,
            SPAN,
        ),
    }
}

/// 变体载荷的类型列表（Unit 变体为空）。
fn payload_types(v: &ast::Variant) -> Vec<Type> {
    match &v.payload {
        VariantPayload::Unit => Vec::new(),
        VariantPayload::Tuple(tys) => tys.clone(),
        VariantPayload::Struct(fields) => fields.iter().map(|f| f.ty.clone()).collect(),
    }
}

/// 对 `self` 按变体分派，每个臂由 `build` 给出臂体。
///
/// `build(variant, bind_names)` 拿到的绑定名与 `variant_pattern` 用同一套
/// 前缀（`s`），臂体里直接用这些名字读载荷。
fn variant_match<F>(type_name: &str, variants: &[ast::Variant], build: F) -> Expression
where
    F: Fn(&ast::Variant, &[String]) -> Expression,
{
    let arms = variants
        .iter()
        .map(|v| {
            let binds = payload_binds(v, "s");
            ast::Arm {
                pattern: variant_pattern(type_name, v, "s"),
                guard: None,
                body: Box::new(build(v, &binds)),
            }
        })
        .collect();
    Expression::Match(Box::new(self_expr()), arms, SPAN)
}

// ----------------------------------------------------------------------
// AST 构造小工具
// ----------------------------------------------------------------------

fn make_fn(
    type_name: &str,
    method: &str,
    receiver: Option<Receiver>,
    params: Vec<Param>,
    ret: Type,
    body: Expression,
) -> Function {
    Function {
        name: format!("{type_name}__{method}"),
        generics: Vec::new(),
        attrs: Vec::new(),
        is_async: false,
        is_comptime: false,
        params,
        receiver,
        return_type: ret,
        body: Some(Block {
            stmts: Vec::new(),
            tail: Some(Box::new(body)),
            span: SPAN,
        }),
    }
}

/// 按值接收者。
///
/// `Receiver::ty` 为 `None` 在 `receiver_ty` 里表示 `&self`（自动套一层引用），
/// 而 `std/core.zr` 的 trait 方法要的是按值的 `self`，所以这里**必须**显式
/// 写出 `Some(Self类型)`。
fn receiver(self_ty: &Type) -> Receiver {
    Receiver {
        mutability: Mutability::Immutable,
        ty: Some(self_ty.clone()),
    }
}

fn param(name: &str, ty: &Type) -> Param {
    Param {
        pattern: binding(name),
        ty: ty.clone(),
    }
}

fn binding(name: &str) -> Pattern {
    Pattern::Binding(name.to_string(), Mutability::Immutable, None, SPAN)
}

fn let_mut(name: &str, ty: Type, init: Expression) -> Statement {
    // 可变性记在**模式**上：类型检查器看的是绑定模式的 `Mutability`，`Var`
    // 只是"这个 let 带 mut"的语法归类。写成 Immutable 会得到
    // "cannot assign to immutable binding"。
    Statement::Var(
        Pattern::Binding(name.to_string(), Mutability::Mutable, None, SPAN),
        Some(ty),
        Some(init),
        SPAN,
    )
}

fn self_expr() -> Expression {
    var("self")
}

fn var(name: &str) -> Expression {
    Expression::Variable(name.to_string(), SPAN)
}

fn field(e: Expression, name: &str) -> Expression {
    Expression::Field(Box::new(e), name.to_string(), SPAN)
}

/// 带位置版本的 `field`：用于 derive 生成的字段访问，让"某字段缺 trait"的
/// 报错指向**那个字段**（`span`）而不是 `0:0:0`。
fn field_at(e: Expression, name: &str, span: Span) -> Expression {
    Expression::Field(Box::new(e), name.to_string(), span)
}

fn method_call(recv: Expression, name: &str, args: Vec<Expression>) -> Expression {
    Expression::Call(
        Box::new(field(recv, name)),
        args.into_iter().map(Argument::Pos).collect(),
        SPAN,
    )
}

fn call(name: &str, args: Vec<Expression>) -> Expression {
    call_path(&[name.to_string()], args)
}

fn call_path(path: &[String], args: Vec<Expression>) -> Expression {
    Expression::Call(
        Box::new(Expression::Path(path.to_vec(), SPAN)),
        args.into_iter().map(Argument::Pos).collect(),
        SPAN,
    )
}

/// 带位置版本的 `call_path`：递归取 `Type__default` 等时用字段的 span，让
/// "目标类型没 derive Default"的报错指向**那个字段**而不是 `0:0:0`。
fn call_path_at(path: &[String], args: Vec<Expression>, span: Span) -> Expression {
    Expression::Call(
        Box::new(Expression::Path(path.to_vec(), span)),
        args.into_iter().map(Argument::Pos).collect(),
        span,
    )
}

fn bin(l: Expression, op: BinaryOp, r: Expression, span: Span) -> Expression {
    Expression::Binary(Box::new(l), op, Box::new(r), span)
}

fn cast(e: Expression, ty: Type) -> Expression {
    Expression::Cast(Box::new(e), ty, SPAN)
}

fn int(v: i128) -> Expression {
    Expression::Literal(Literal::Int(v, None), SPAN)
}

fn named(name: &str) -> Type {
    Type::Named {
        path: vec![name.to_string()],
        generics: Vec::new(),
    }
}

/// `T?` / `T!E` 这类语法糖在 HIR 里都是命名类型，递归调用代表方法。
fn is_named_like(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Named { .. } | Type::Optional(_) | Type::ErrorUnion(..)
    )
}
