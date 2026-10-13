# Higher-Ranked Trait Bounds (HRTB)

## Status: ⬜ Not implemented

## Problem
`for<'a> fn(&'a T)` requires the trait bound to hold for *all* lifetimes `'a`.
This needs a separate "universe" system in the solver.

## Tasks
- [ ] Add `Binder<T>` to represent universally-quantified lifetimes.
- [ ] Skolemize placeholders when entering a `for<>` binder.
- [ ] Fold/unfold lifetimes when exiting the binder.
- [ ] Diagnose "borrowed value does not live long enough" via placeholder leak.

## Acceptance
```rust
fn apply<F>(f: F) -> i32 where F: for<'a> Fn(&'a i32) -> i32 { f(&42) }
```
