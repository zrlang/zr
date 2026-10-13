# Inline Assembly (`asm!`)

## Status: ✅ Done

## Goal
Provide a stable inline-asm syntax so systems programmers can drop to
assembly without writing a separate `.s` file.

## Proposed syntax
```rust
let result: i32;
unsafe {
    asm!(
        "mov {0}, 42",
        out(reg) result,
        options(pure, nomem, nostack),
    );
}
```

## Design questions
- Follow Rust's `asm!` syntax or LLVM's `__asm__`? Leaning Rust for ergonomics.
- How to validate register constraints at compile time?
- What target features are required for which registers?
