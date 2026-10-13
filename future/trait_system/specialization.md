# Specialization

## Status: ⬜ Not implemented

## Problem
Allow `impl<T> Trait for T` to be specialized by a more specific impl
`impl Trait for MyType`. Requires a coherence rule based on specificity.

## Tasks
- [ ] Implement the "more specific" partial order.
- [ ] Reject overlapping impls that are not ordered by specificity.
- [ ] Decide default semantics (`default fn`) vs final methods.

## Open question
Should specialization be sound with negative reasoning (e.g. `?Sized`)?
Track RFC #1210.
