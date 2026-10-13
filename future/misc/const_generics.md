# Const Generics

## Status: ✅ Done

## Goal
Allow types to be parameterized by values, not just other types:

```rust
struct Array<T, const N: usize> { data: [T; N] }

fn transpose<T, const R: usize, const C: usize>(m: [[T; C]; R])
    -> [[T; R]; C { .. }]
```

## Minimal implementation (MVP)
- Only `usize` and `i32` const parameters.
- Only trivial expressions in array lengths (no function calls).

## Full implementation
- CTFE (compile-time function evaluation) interpreter.
- Full integer and bool const expressions.
- `where T: Trait` bounds dependent on const parameters.
