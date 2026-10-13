# Dependency Resolution Algorithm

## Status: ✅ Done

## Algorithm: PubGrub (adapted)
1. Incompatibilities are sets of (package, version range) tuples.
2. Start with the root package, derive initial incompatibilities.
3. Pick the highest untested version of the package with the fewest remaining
   choices (heuristic: "fewest versions first").
4. When a conflict is found, derive a new incompatibility by combining the
   conflicting partial solutions.
5. Backtrack to the highest decision level where the incompatibility is
   relevant.
6. Terminate when all packages have a selected version and no incompatibilities
   remain.

## Extensions over vanilla PubGrub
- Semver ranges with `^` and `~` shortcuts.
- Platform-specific dependencies (`[target.'cfg(linux)'.dependencies]`).
- Dev-dependencies excluded from the published artifact.
