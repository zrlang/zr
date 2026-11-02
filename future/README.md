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
| assoc_type_projection.md | ✅     | `Self::Output` / `F::Output`（`F: Trait`）已支持（含泛型 impl 的 `impl<T> Tr for S<T> { type Item = T; }` 投影）；`<T as Tr>::Item` 限定写法也已实现并带 trait 校验（`119_qualified_assoc.zr`，负向 `29`）。投影仍按类型查 impl 表、不按 trait 过滤，两个 trait 同名关联类型时无法消歧 |
| higher_ranked.md         | ⬜     | `for<'a>` 绑定器未实现 |
| specialization.md        | 🔧     | 重叠 impl 的**选择**已按"更 specific 优先"（`is_more_specific`，`impl<T> Tr for T` 与 `impl Tr for A` 同时存在时 `a.method()` 走具体实现）。**trait 默认方法已实现**（`124`，负向 `37`）：trait 里带方法体的方法在 impl 未覆盖时合成到目标类型，`default fn` 拼写在 trait / impl 两侧都接受；但"具体 impl 只覆盖部分默认方法、其余从毯覆 impl 继承"的两级继承仍无（泛型 impl 的毯覆方法解析本身未做） |
| coherence_rules.md       | ✅     | 孤儿规则（`check_orphan_rule`）与重叠检查（`check_overlap`）已在 `src/typeck/mod.rs:1812-1814` 接线，报 `coherence_error`；marker trait 允许重叠 |
| impl_trait.md            | 🔧     | **参数位置（APIT）已实现**：`x: impl Trait` 脱糖成匿名泛型参数，按实参单态化、边界真校验（`120_apit.zr`，负向 `30`）；`&impl Trait` / `Vec<impl Trait>` 绑内层类型。**返回位置（RPIT）仍只是骨架**：值是 `opaque(impl_trait_N)`，任何字段 / 方法访问都报错；`impl Fn(T) -> T` 仍解析失败 |
| negative_bounds.md       | ⬜     | `where T: !Trait` 解析后被丢弃（`WherePred` 是空结构，整个 `where` 子句都没有语义）；`<T: !Tr>` 泛型参数位置连解析都过不去 |

### bootstrap
| File               | Status | Summary                          |
|--------------------|--------|----------------------------------|
| README.md          | 📐     | Three-stage self-hosting plan.   |
| reduced_syntax.md  | 🔧     | `zr0` 语法子集有设计文档，`src/zr0/` 未 `mod` 声明 |
| c_backend.md       | ✅     | `src/codegen/c_backend.rs` 已由 `--emit-c` 接线（HIR → 单个 C 翻译单元，自举路径；`main.rs:762-766`） |

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
