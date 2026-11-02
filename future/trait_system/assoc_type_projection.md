# Associated Type Projection

## Status: ✅ Done（限定写法已实现，投影仍不按 trait 过滤）

## Problem
Given `<T as Iterator>::Item`, the compiler must normalize the projection to a
concrete type or leave it as an inference variable. This is needed for
`impl Trait`, `dyn Trait`, and `where` clauses.

## What actually exists

三种写法都走同一条投影路径（`src/typeck/mod.rs::resolve_assoc_type`）：

| 写法 | 入口 | 说明 |
|---|---|---|
| `Self::Output` | `ast_to_hir_type` 的关联类型分支 | impl 方法签名里；`build_instance_signature` 会推 `Self` |
| `F::Output` | 同上（基类型是泛型参数） | 声明期 `F` 是推断变量 → 返回新推断变量占位，实例化期再算 |
| `Type::Output` | 同上（基类型是具体类型） | 非泛型 impl 走 `impl_assoc_types`（按类型 DefId 索引） |
| `<T as Trait>::Item` | `AstType::Qualified` | 多一步 trait 校验：`direct_impls(&base)` 不含该 trait 就报 `the trait `X` is not implemented for `Y`` |

泛型 impl（`impl<T> Tr for Ready<T> { type Output = T; }`）的关联类型不能提前
算好：`generic_impl_assocs` 保留目标 AST + 泛型参数名 + 关联类型 AST，投影时用
`match_generic_ast` 把 `T` 绑到实参，再把定义体 AST 在新作用域里重降。

## Tasks
- [x] 类型表示：不新增 `TyKind::Projection`，投影在 `ast_to_hir_type` 阶段就
      归一化成具体 `Ty`（或声明期的新推断变量）。
- [x] 实现 projection 归一化（`resolve_assoc_type`：非泛型 impl 表 / 泛型 impl
      逐个 `impl_target_matches`）。
- [x] 限定路径 `<T as Trait>::Item` 解析（`Type::Qualified`）与 trait 校验。
- [x] 顺带修掉类型位置的 `<<` / `>>` 拆分（`Parser::split_angle`），否则嵌套
      泛型 `Vec<Vec<i32>>` 与 `Option<<T as Tr>::Item>` 解析不了。
- [ ] 投影按 trait 过滤 impl 表（两个 trait 同名关联类型时仍无法真正消歧）。
- [ ] 归一化缓存（当前每次投影都遍历 impl 表 / 重降 AST，无缓存）。

## Acceptance
已有回归：`tests/cases/118_assoc_projection.zr`、`tests/cases/119_qualified_assoc.zr`；
负向 `tests/negative/29_qualified_trait_not_impl.zr`。

```rust
// 泛型函数返回关联类型投影（zr 已支持，见 119）
fn qualified<F: Future>(f: F) -> <F as Future>::Output { .. }
```
