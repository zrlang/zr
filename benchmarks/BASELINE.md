# zr benchmark baseline

- Date: 2026-10-20
- Host: macOS x86_64 (`Darwin-x86_64`, Darwin 24.6.0)
- Compiler build: debug (`cargo build --quiet`)
- Command: `./scripts/bench.sh`

`cargo run --quiet -- -V` on this machine:

```text
zr 0.8.0
host: x86_64-apple-darwin (Mach-O)
targets: arm-none-eabi, thumbv7em-none-eabi, riscv32-unknown-none, riscv64-unknown-none, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, x86_64-apple-darwin, aarch64-apple-darwin, x86_64-pc-windows-msvc, aarch64-pc-windows-msvc, wasm32-unknown-unknown, wasm32-wasi, wasm64-unknown-unknown, x86_64-unknown-freebsd, aarch64-unknown-freebsd, x86_64-linux-android, aarch64-linux-android, aarch64-apple-ios
```

Observed best-of-3 wall times, in seconds:

| benchmark | -O0 | -O1 | -O2 | -O3 | -Oz |
|---|---:|---:|---:|---:|---:|
| bench_fib | 0.020 | 0.020 | 0.020 | 0.020 | 0.020 |
| bench_loop | 0.040 | 0.040 | 0.040 | 0.040 | 0.040 |
| bench_sieve | 0.080 | 0.080 | 0.080 | 0.080 | 0.080 |
| bench_string | 0.090 | 0.090 | 0.090 | 0.090 | 0.090 |

These numbers are best-of-3 on this machine only; do not compare them across
hosts, CPU load, or compiler build profiles. `/usr/bin/time -p` reports
wall-clock time with 0.01 s resolution, so cells are rounded accordingly.

Notes:

- `bench_sieve` uses the language-level `[u8; 256]` array directly.
- Each benchmark checks its final result and returns 0 only on success.
