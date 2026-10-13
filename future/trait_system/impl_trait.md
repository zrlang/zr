# `impl Trait` (RPIT & APIT)

## Status: ✅ Done

## Problem
`impl Trait` in return position (RPIT) hides the concrete type; in argument
position (APIT) it is existential. Both desugar to opaque types but with
different variance and capture rules.

## Tasks
- [ ] Lower RPIT to `OpaqueTy` with inferred hidden type.
- [ ] Lower APIT to a fresh generic parameter.
- [ ] Capture only the needed generic parameters (not all in scope).
- [ ] Reject opaque types crossing function boundaries unsoundly.

## Acceptance
```rust
fn iter() -> impl Iterator<Item = i32> { (0..10).map(|x| x * 2) }
fn apply(f: impl Fn(i32) -> i32) -> i32 { f(1) }
```
