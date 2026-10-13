# Pinning & Move Safety

## Status: ✅ Done

## Problem
`!Unpin` types (e.g. self-referential async state machines) must not be moved
after their first `poll`, otherwise internal self-references become dangling.

## Tasks
- [ ] Define `Pin<T>` as a pointer wrapper with a move-invariant.
- [ ] Define the `Unpin` auto-trait; all types implement it by default.
- [ ] In typeck, reject moves of `!Unpin` values after first borrow.
- [ ] In async lowering, wrap the state machine in `Pin` when it holds a
      `!Unpin` field.
- [ ] Provide `Pin::new` (requires `Unpin`) and `Pin::new_unchecked` (unsafe).

## Acceptance
```rust
// must compile
let x = 42;
let _ = Pin::new(&x);

// must NOT compile
struct SelfRef { ptr: *const SelfRef }
// SelfRef: !Unpin inferred
fn bad() {
    let s = SelfRef { ptr: std::ptr::null() };
    let pinned = Pin::new(&s);   // error: SelfRef is !Unpin
    drop(s);                      // error: s moved while pinned
}
```
