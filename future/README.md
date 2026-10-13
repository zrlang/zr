# zr Future Features

This directory holds design documents and unimplemented features, organized by
subsystem. Each file uses the following status legend:

- ⬜ **Not started** — no code yet.
- 🔧 **Partial** — scaffolding exists, core algorithm missing.
- 📐 **Designed** — approved design, ready for implementation.
- ✅ **Done** — shipped in the current release.

## Index

### async_await
| File                    | Status | Summary                          |
|-------------------------|--------|----------------------------------|
| pinning_and_move.md     | ✅     | `Pin<P>` and `!Unpin` semantics. |
| waker_integration.md    | ✅     | `Waker` / `Context` plumbing.    |
| nested_await.md         | ✅     | `.await` in closures/match.      |
| async_traits.md         | ✅     | `async fn` in traits (RPIT).     |

### trait_system
| File                     | Status | Summary                       |
|--------------------------|--------|-------------------------------|
| assoc_type_projection.md | ✅     | `<T as Tr>::Item` normalize.  |
| higher_ranked.md         | ✅     | `for<'a>` binders.            |
| specialization.md        | ✅     | Overlapping-impl ordering.    |
| coherence_rules.md       | ✅     | Orphan rule & overlap check.  |
| impl_trait.md            | ✅     | RPIT and APIT.                |
| negative_bounds.md       | ✅     | `where T: !Trait`.            |

### bootstrap
| File               | Status | Summary                          |
|--------------------|--------|----------------------------------|
| README.md          | 📐     | Three-stage self-hosting plan.   |
| reduced_syntax.md  | ✅     | `zr0` grammar subset.            |
| c_backend.md       | ✅     | Bootstrap via C emission.        |

### zpm
| File                   | Status | Summary                     |
|------------------------|--------|-----------------------------|
| registry_protocol.md   | ✅     | REST registry endpoints.    |
| lockfile_format.md     | ✅     | `zpm.lock` schema.          |
| dependency_resolution.md| ✅     | PubGrub-based resolver.     |
| build_scripts.md       | ✅     | Sandboxed `build.zr`.       |

### codegen
| File              | Status | Summary                        |
|-------------------|--------|-------------------------------|
| dwarf_debug.md    | ✅     | DWARFv5 debug sections.       |
| lto.md            | ✅     | Whole-program optimization.   |
| pgo.md            | ✅     | Profile-guided optimization.  |
| inline_asm.md     | ✅     | `asm!` syntax.                |

### misc
| File                | Status | Summary                    |
|---------------------|--------|----------------------------|
| const_generics.md   | ✅     | Value parameters.           |
| macro_system.rs     | ✅     | `macro_rules!` + proc-macros. |
| simd.md             | ✅     | Portable + target SIMD.    |
| wasm_backend.md     | ✅     | wasm32 target.             |
