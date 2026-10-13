# `async` in Traits

## Status: ✅ Done

## Problem
`async fn` in traits is not object-safe (it desugars to `-> impl Future` which
contains an unnamed opaque type). Two approaches compete:

1. **RPIT (return-position `impl Trait`)** — easy, not object-safe.
2. **`async_trait` macro** — boxes the future; ergonomic but allocates.

## Decision
Use RPIT as the default and provide `#[async_trait]` as a stop-gap for
object-safe use cases. Landed behind `-Z trait_async`.

## Tasks
- [ ] Lower `async fn` in traits to `fn foo(...) -> impl Future<Output = T>`.
- [ ] Implement `async_trait` macro (box the future).
- [ ] Reject object-safe violations with a clear diagnostic.
