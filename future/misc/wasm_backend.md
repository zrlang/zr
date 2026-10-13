# WebAssembly Backend

## Status: ✅ Done

## Goal
`zr compile --target wasm32-unknown-unknown` produces a valid `.wasm` module
that can run in a browser or on `wasmtime`.

## Milestones
1. Emit a minimal WAT/WASM module (one function, no imports).
2. Support `i32`/`i64`/`f32`/`f64` locals and arithmetic.
3. Add import/export tables for host calls (e.g. `wasi_snapshot_preview1`).
4. Port `std` to `wasi` (alloc, console output).
5. Interop with JS via `wasm-bindgen`-style glue.

## Open question
Should the wasm backend reuse the x86 SSA pipeline (with a wasm-specific
instruction selector) or emit WASM text directly from MIR?
