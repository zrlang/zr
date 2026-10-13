# Trait Coherence Rules

## Status: 📐 Designed

## Problem
Two independent crates must never be able to define overlapping impls that
compile together. This is the "orphan rule".

## Rules
1. **Orphan rule**: an impl is valid only if the trait or at least one type in
   the impl header is local to the current crate.
2. **Overlap check**: no two impls may apply to the same concrete type.
3. **Marker traits** (`Send`, `Sync`, `Unpin`) are auto-derived; manual impls
   are allowed but coherence still applies.

## Tasks
- [ ] Implement overlap detection (trivial vs non-trivial overlaps).
- [ ] Add `#[marker]` trait attribute.
- [ ] Test cross-crate coherence with a fixture.
