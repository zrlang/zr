# Associated Type Projection

## Status: 🔧 Partial

## Problem
Given `<T as Iterator>::Item`, the compiler must normalize the projection to a
concrete type or leave it as an inference variable. This is needed for
`impl Trait`, `dyn Trait`, and `where` clauses.

## Tasks
- [ ] Add `TyKind::Projection(<QPath, assoc>)` to the type repr.
- [ ] Implement `normalize_projection` in the trait solver.
- [ ] Cache normalizations to avoid exponential blow-up.
- [ ] Report "ambiguous associated type" when multiple impls match.

## Acceptance
```rust
fn first<T: IntoIterator>(t: T) -> T::Item { t.into_iter().next().unwrap() }
```
