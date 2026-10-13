# Link-Time Optimization (LTO)

## Status: ✅ Done

## Why
Whole-program optimization (inlining across crate boundaries, dead-code
elimination of unused generic monomorphizations) can yield 10–30% size and
speed improvements.

## Approach
1. Emit bitcode (LLVM IR or our own SSA) into a dedicated section/archive.
2. At link time, deserialize all modules and run the optimization pipeline
   before the final codegen pass.
3. Support `lto = "thin"` (parallel, per-call-graph partition) and
   `lto = "fat"` (whole-program).

## Tasks
- [ ] Define the serialized SSA format (versioned).
- [ ] Implement cross-module inlining.
- [ ] Add `-C lto={off,thin,fat}` flag.
