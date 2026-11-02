# `impl Trait` (RPIT & APIT)

## Status: 🔧 部分完成（APIT ✅ / RPIT ⬜）

## Problem
`impl Trait` in return position (RPIT) hides the concrete type; in argument
position (APIT) it is existential. Both desugar to opaque types but with
different variance and capture rules.

## Tasks
- [x] Lower APIT to a fresh generic parameter.
      `src/typeck/mod.rs` 的 `check_program` 在声明收集**之前**对
      `expanded.functions` 做一遍 `desugar_apit`：参数类型里每个
      `impl Trait` 就地换成 `Named{path:["__apit_N"]}`，边界作为
      `GenericParam::Type("__apit_N", bounds)` 追加到 `f.generics`。
      递归进引用 / 指针 / 泛型实参 / 元组 / 数组 / 切片 / 可选 / 错误联合，
      因此 `&impl Trait` 绑**内层**类型（`&T`）、`Vec<impl Trait>` 是 `Vec<T>`。
      `fn(..)` 函数指针类型里的 `impl Trait` 不处理（Rust 也不允许）。
      边界校验复用 `check_trait_bounds`（单态化点），报错措辞对匿名参数
      特判成 `required by a bound in `f``。回归 `tests/cases/120_apit.zr`，
      负向 `tests/negative/30_apit_bound.zr`。
- [ ] Lower RPIT to `OpaqueTy` with inferred hidden type.
      现状：`-> impl Trait` 能解析、能构造返回值，但类型是
      `opaque(impl_trait_N)`，任何字段 / 方法访问都报
      `cannot access field ... on type opaque(impl_trait_N)`。
      异步用例（`29`/`49`/`50`）只把它当 Future 句柄轮询，不走字段访问。
- [ ] Capture only the needed generic parameters (not all in scope).
- [ ] Reject opaque types crossing function boundaries unsoundly.
- [ ] trait **声明**里的方法签名不做 APIT 脱糖（声明本身不参与单态化，
      只有 impl 方法 / 自由函数会走 `desugar_apit`）。

## Acceptance
```rust
fn iter() -> impl Iterator<Item = i32> { (0..10).map(|x| x * 2) }  // ⬜ RPIT
fn apply(f: impl Fn(i32) -> i32) -> i32 { f(1) }                   // 🟡 APIT 已脱糖，但 `impl Fn(i32) -> i32` 这种带括号的 trait 路径仍解析失败
```
