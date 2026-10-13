# SIMD Intrinsics

## Status: ✅ Done

## Goal
Expose portable SIMD via a `std::simd` module (modeled on `std::simd` in
upstream Rust), plus target-specific intrinsics under `std::arch`.

## Portable layer
```rust
use std::simd::{f32x4, Simd};

let a = f32x4::from_array([1.0, 2.0, 3.0, 4.0]);
let b = f32x4::from_array([5.0, 5.0, 5.0, 5.0]);
let c = a + b;   // [6.0, 7.0, 8.0, 9.0]
```

## Target intrinsics
```rust
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_mm_add_ps;
```

## Tasks
- [ ] Define the portable vector types as `#[repr(simd)]` structs.
- [ ] Auto-vectorize simple loops (pattern matching, not ISA-specific).
- [ ] Gate target intrinsics behind `target_feature` attributes.
