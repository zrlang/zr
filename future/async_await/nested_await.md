# Nested `.await`

## Status: ⬜ Not implemented

## Problem
`.await` inside closures, `match` arms, `if` expressions and blocks must all
desugar correctly. The state machine must thread the `waker` through every
nested await point.

## Tasks
- [ ] Extend the await-desugaring CFG to handle closures.
- [ ] Support `.await` in `match` scrutinees and guards.
- [ ] Ensure `?` inside async blocks interacts correctly with `Poll::Ready`.

## Acceptance
```rust
async fn inner() -> i32 { 1 }
async fn outer() -> i32 {
    let v = (|| async { inner().await + 1 })().await;
    match v { 2 => inner().await, _ => 0 }
}
```
