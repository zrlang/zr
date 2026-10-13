# Build Scripts (`build.zr`)

## Purpose
Some packages need to generate code at compile time (e.g. bindgen, lexer
generators). `zpm` runs a sandboxed `build.zr` before compiling the package.

## Format
```rust
fn main() {
    zpm::rerun_if_changed("src/proto/defs.proto");
    protoc::compile(&["src/proto/defs.proto"], &["src/gen"]);
}
```

## Sandbox guarantees
- No network access by default (opt-in via `[package.build.network]`).
- Filesystem access limited to the package root and a scratch dir.
- CPU and wall-clock time limits (configurable, default 60s).

## Tasks
- [ ] Define the `zpm` build-API crate (a reduced std).
- [ ] Implement the sandbox (Linux namespaces, macOS seatbelt).
- [ ] Cache build script outputs by input hash.
