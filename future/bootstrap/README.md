# Bootstrap Plan

The goal is for `zr` to compile itself. This happens in three stages:

## Stage 0 — Reduced subset (`zr0`)
- Freeze the C ABI and the core syntax subset.
- Write a minimal compiler (let/fn/if/while, i32, pointers) that targets C.
- Deliverable: `zr0` can compile `zr0` itself.

## Stage 1 — Self-hosting subset (`zr1`)
- Implement `zr1` (a superset of `zr0`) *in* `zr0`.
- Add structs, enums, generics, trait basics.
- Deliverable: `zr1` compiled by `zr0` can recompile `zr1`.

## Stage 2 — Full self-hosting
- Port the complete Rust-based bootstrap compiler to `zr1`.
- Drop the Rust codebase; the compiler is now written in `zr`.
- Deliverable: `zr` compiled by `zr` produces a byte-identical binary.

## Verification
```
rustc zr.zr -o stage0
./stage0 zr.zr -o stage1
./stage1 zr.zr -o stage2
sha256sum stage1 stage2   # must match
```

## Timeline
| Quarter   | Milestone                          |
|-----------|------------------------------------|
| 2026 Q4   | ABI freeze, C backend prototype    |
| 2027 Q1   | `zr0` self-compiles                |
| 2027 Q2   | `zr1` subset implemented in `zr0`  |
| 2027 Q3   | Full language features land        |
| 2027 Q4   | Complete self-hosting              |
| 2028 Q1   | Release v1.0 "Self-Hosted"         |
