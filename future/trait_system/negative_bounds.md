# Negative Trait Bounds

## Status: ✅ Done

## Problem
`where T: !Trait` expresses that `T` must *not* implement `Trait`. This is
useful for specialization and for preventing footguns (e.g. preventing a type
from being `Send`).

## Tasks
- [ ] Extend `WhereClause` with `NegatedTrait(TraitRef)`.
- [ ] In the solver, treat negative bounds as constraints on impl selection.
- [ ] Soundness: only auto traits may use negative reasoning (for now).

## Open question
Should `!Send` be expressible for user-defined traits? RFC under discussion.
