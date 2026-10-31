# zr Future Features

This directory holds design documents and unimplemented features, organized by
subsystem. Each file uses the following status legend:

- ⬜ **Not started** — no code yet.
- 🔧 **Partial** — scaffolding exists, core algorithm missing.
- 📐 **Designed** — approved design, ready for implementation.
- ✅ **Done** — shipped in the current release.

> ⚠️ **2026-10-29 核对**：这张表**严重失真**——几乎所有条目都标 ✅ Done，
> 但源码里对应特性大多没接线或只是空壳。状态按 `AGENTS.md` / `AGENTS.md`
> 与 `grep` 源码的实际结论重排：只有确实实现/接线的才留 ✅，其余降为
> 🔧（有骨架没算法）或 ⬜（连骨架都没有）。

## Index

### async_await
| File                    | Status | Summary                          |
|-------------------------|--------|----------------------------------|
| pinning_and_move.md     | 🔧     | `Pin<P>` / `!Unpin` 有类型（`Ty::Pin`），语义完整度未验证 |
| waker_integration.md    | 🔧     | `Waker` / `Context` 接线只有骨架 |
| nested_await.md         | 🔧     | `.await` 在闭包/match 中受限 |
| async_traits.md         | ⬜     | trait 中的 `async fn`（RPIT）未实现 |

### trait_system
| File                     | Status | Summary                       |
|--------------------------|--------|-------------------------------|
| assoc_type_projection.md | ⬜     | `<T as Tr>::Item` 归一化未实现（`Self::Output` 可用但无投影求解） |
| higher_ranked.md         | ⬜     | `for<'a>` 绑定器未实现 |
| specialization.md        | ⬜     | 重叠 impl 排序未实现 |
| coherence_rules.md       | ⬜     | 孤儿规则与重叠检查未实现 |
| impl_trait.md            | ⬜     | RPIT / APIT 未实现（解析失败） |
| negative_bounds.md       | ⬜     | `where T: !Trait` 未实现 |

### bootstrap
| File               | Status | Summary                          |
|--------------------|--------|----------------------------------|
| README.md          | 📐     | Three-stage self-hosting plan.   |
| reduced_syntax.md  | 🔧     | `zr0` 语法子集有设计文档，`src/zr0/` 未 `mod` 声明 |
| c_backend.md       | 🔧     | `src/codegen/c_backend.rs` 存在，CLI 无 `--emit-c` 入口 |

### zpm
| File                   | Status | Summary                     |
|------------------------|--------|-----------------------------|
| registry_protocol.md   | 🔧     | `src/zpm/registry` 有代码 |
| lockfile_format.md     | 🔧     | `zpm.lock` 有代码           |
| dependency_resolution.md| 🔧     | 有代码                      |
| build_scripts.md       | 🔧     | 有代码                      |

### codegen
| File              | Status | Summary                        |
|-------------------|--------|-------------------------------|
| dwarf_debug.md    | ✅     | DWARFv5 已实现（`src/codegen/dwarf.rs`，`-g`） |
| lto.md            | 🔧     | `src/opt/lto.rs` 有完整序列化/反序列化 + `cross_module_inline`/`run_lto`，但 `opt/mod.rs` 未声明、CLI 无开关 |
| pgo.md            | ⬜     | `src/opt/pgo.rs` 未接线（`opt/mod.rs` 未声明） |
| inline_asm.md     | ✅     | `asm!` 已实现（`27_inline_asm.zr`） |

### misc
| File                | Status | Summary                    |
|---------------------|--------|----------------------------|
| const_generics.md   | ⬜     | `<const N: usize>` 解析接受但检查时报错 |
| macro_system.rs     | 🔧     | `macro_rules!` 已走 `src/macros.rs`（`58_macros.zr`）；proc-macro 未接线 |
| simd.md             | 🔧     | `std/simd` 目标相关，非全平台 |
| wasm_backend.md     | ✅     | wasm32/wasm64 目标已实现（`src/codegen/wasm.rs`） |
