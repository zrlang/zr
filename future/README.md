# zr Future Features

This directory holds design documents and unimplemented features, organized by
subsystem. Each file uses the following status legend:

- ⬜ **Not started** — no code yet.
- 🔧 **Partial** — scaffolding exists, core algorithm missing.
- 📐 **Designed** — approved design, ready for implementation.
- ✅ **Done** — shipped in the current release.

> ⚠️ **2026-11-05 核对**：这张表在 2026-10-29 曾严重失真（几乎所有条目都标
> ✅ Done，但源码里对应特性大多没接线或只是空壳）。2026-10-29 / 2026-11-01
> 已按源码实际结论重排；2026-11-05 又核对一次：`higher_ranked` /
> `negative_bounds` / `const_generics`（含长度表达式）/ RPIT 字段访问 /
> 关联类型消歧 / `zr0` 接线已按新实现更新为 ✅ 或 🔧（部分）。

## Index

### async_await
| File                    | Status | Summary                          |
|-------------------------|--------|----------------------------------|
| pinning_and_move.md     | 🔧     | `Pin<P>` / `!Unpin` 有类型（`Ty::Pin`），语义完整度未验证 |
| waker_integration.md    | 🔧     | `trait Wake`（`wake(self: Box<Self>)` / `wake_by_ref(&self)`）、`Waker`（`new` / `wake` / `wake_by_ref`）、`Context`（`new` / `waker`）都在 `std/alloc/async.zr` 实现；`Box<dyn Wake>` vtable 分派可用（`169_box_dyn.zr`）。仍缺：async 降级生成的 `poll` 签名没有 `cx: &mut Context`——`.await` 的 Context 传递、timer 驱动的 readiness 通知未接线 |
| nested_await.md         | 🔧     | `.await` 在闭包/match 中受限 |
| async_traits.md         | ⬜     | trait 中的 `async fn`（RPIT）未实现 |

### trait_system
| File                     | Status | Summary                       |
|--------------------------|--------|-------------------------------|
| assoc_type_projection.md | ✅     | `Self::Output` / `F::Output`（`F: Trait`）已支持（含泛型 impl 的 `impl<T> Tr for S<T> { type Item = T; }` 投影）；`<T as Tr>::Item` 限定写法也已实现并带 trait 校验（`119_qualified_assoc.zr`，负向 `29`）；**同名关联类型消歧已实现**（`163_assoc_disambig.zr`）——`<Both as HasA>::Item` 与 `<Both as HasB>::Item` 按限定路径里的 trait 过滤 impl 表，不再取先收集者 |
| higher_ranked.md         | ✅     | `for<'a> T: Trait<'a>` 已支持（2026-11-05）：`parse_lifetime_name` 认 `TokenKind::Lifetime`（`'a`/`'static`），`for<'a>` 的收尾 `>` 正确消费；zr 生命周期参数整体跳过（类型检查丢弃），`'a` 绑定器无语义但语法完整——单/多生命周期、`impl<'a> Tr<'a> for Type`、`&'a i32` 实参都可用（回归 `169_hrtb.zr`）。真正的 HRTB 语义（对所有 `'a` 成立）仍无——`'a` 未参与类型检查 |
| specialization.md        | 🔧     | 重叠 impl 的**选择**已按"更 specific 优先"（`is_more_specific`，`impl<T> Tr for T` 与 `impl Tr for A` 同时存在时 `a.method()` 走具体实现）。**trait 默认方法已实现**（`124`，负向 `37`）：trait 里带方法体的方法在 impl 未覆盖时合成到目标类型，`default fn` 拼写在 trait / impl 两侧都接受；但"具体 impl 只覆盖部分默认方法、其余从毯覆 impl 继承"的两级继承仍无（泛型 impl 的毯覆方法解析本身未做） |
| coherence_rules.md       | ✅     | 孤儿规则（`check_orphan_rule`）与重叠检查（`check_overlap`）已在 `src/typeck/mod.rs:1812-1814` 接线，报 `coherence_error`；marker trait 允许重叠 |
| impl_trait.md            | 🔧     | **参数位置（APIT）已实现**：`x: impl Trait` 脱糖成匿名泛型参数，按实参单态化、边界真校验（`120_apit.zr`，负向 `30`）；`&impl Trait` / `Vec<impl Trait>` 绑内层类型；**trait 声明方法签名里的 APIT 也已脱糖**（`170_trait_apit.zr`，含默认方法与关联函数）。**返回位置（RPIT）已可用**（`167_rpit_field.zr`，负向 `47`/`48`）：`-> impl Tr` 是 `opaque(impl_trait_N)`，隐藏类型经 `opaque_hidden` 记录，字段访问（`r.x`）与方法访问（`r.area()`）都按隐藏类型解析、MIR 布局按隐藏类型传/返回。**仍不支持**：`impl Fn(T) -> T`（无 `Fn` trait 语法）；trait 方法签名里的 RPIT（async_traits 条目） |
| negative_bounds.md       | ✅     | `where T: !Trait` 已实现（`168_negative_bounds.zr`，负向 `38`）：`Type::Not(inner)` 解析 + `WherePred { lhs, bounds }` 存储，`check_impl_where_bounds`（impl 方法）与泛型函数 where 校验按 `(trait_id, negated)` 求解——positive 要求实现、negative 要求不实现，违反报 `unexpectedly implements trait`。多边界混合（`T: Clone + !Marker`）可用；`<T: !Tr>` 泛型参数位置也过 |

### bootstrap
| File               | Status | Summary                          |
|--------------------|--------|----------------------------------|
| README.md          | 📐     | Three-stage self-hosting plan.   |
| reduced_syntax.md  | 🔧     | `zr0` 语法子集已**接线**（`bfaf849`：`main.rs` `mod zr0` + `zr zr0-check <file>` / `zr zr0-c <file>` 子命令，`zr0::check_zr0` / `parse_zr0` / `zr0_to_c`）。仍只有检查与 C 骨架翻译，完整自举闭环（zr0 → C → 编译 zr）未验证 |
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

### std
| File                | Status | Summary                    |
|---------------------|--------|----------------------------|
| argparse.md         | ✅     | 命令行参数：入口桩捕获 `(argc, argv)` → `zr_args_init`，`std/std/argparse.zr` 提供索引读取 `arg_count` / `arg` / `arg_opt` / `args` / `get` / `program_name`，按名读取 `value` / `value_or` / `has` / `flag` / `positionals`，以及子命令与帮助 `subcommand` / `print_help` / `help_requested`（回归 `126` / `131` / `145`；原 `std/cli.zr` 已重命名） |
| env.md              | ✅     | 环境变量：初值来自入口 `envp`（`zr_env_count` / `zr_env_entry`），写操作落在 `std/std/env.zr` 的覆盖表（墓碑删除，后写优先），提供 `var` / `var_or` / `set_var` / `remove_var` / `has_var`（回归 `127`） |
| json.md             | ✅     | JSON 解析 / 生成：`Json` 递归值树，`json_parse` / `json_stringify` / `json_stringify_pretty`（回归 `128`） |
| xml.md              | ✅     | XML 解析 / 生成：`XmlNode`（属性 / 子节点 / 文本），`xml_parse` / `xml_stringify` / `xml_stringify_pretty`（回归 `129`） |
| toml.md             | ✅     | TOML 解析 / 生成：`Toml` 表 / 数组 / 标量，支持 `[table]`、`[[array-of-tables]]`、点分键、内联表（回归 `130`） |
| os / posix / time / tz | ✅ | `std/std/os.zr`（`pid` / `cwd_len` / `temp_dir` / `home_dir` / `os_name` / `pointer_bits` / `exit`）、`std/std/posix.zr`（`isatty` / `getpid` / `kill` / `getcwd` / `SIG*` / 等待状态解码）、`std/std/time.zr`（纳秒时钟 + `DateTime` / ISO 解析）、`std/std/tz.zr`（内置九时区 + 真实 DST）（回归 `136` / `137` / `138`） |
| md5 / sha256 / base64 / http / pdb / tensor | ✅ | 纯 zr 实现的摘要与编码（`std/alloc/md5.zr` / `std/alloc/sha256.zr` / `std/alloc/base64.zr`）、HTTP 客户端（`std/std/http.zr`：GET / POST / 重定向）、PDB 结构解析（`std/alloc/pdb.zr`）、面向 AI 的 `Tensor`（`std/alloc/tensor.zr`：广播 / `conv2d` / autograd / GPU 桩）（回归 `133` / `134` / `135` / `140` / `141` / `142` / `143` / `146`；API 一览见 `docs/task-table.md` 阶段 4） |
| tls.md              | 🟢     | `std/std/tls.zr`：**纯 zr** TLS 1.2 客户端（rustls 式 `ClientConfig` / `ClientConnection`），零外部依赖；完整 PSK 握手中实现（0x00B2）、AES-128-CTR+HMAC-SHA256 加密 record 层、PRF 密钥导出；非 PSK 密码套件走骨架模式（仅 ClientHello/ServerHello 交换）；RSA/ECDH 证书验证留 TODO（受限于无 u128/BigInt） |
| crypto.md           | ✅     | `std/alloc/crypto.zr`：纯 zr 密码学原语——AES-128-ECB + AES-128-CTR + SHA-256 流式（`init` / `update` / `digest`）+ HMAC-SHA256 + 简易 PRNG；三个 NIST 测试向量 + RFC 4231 HMAC 向量全部通过 |

### misc
| File                | Status | Summary                    |
|---------------------|--------|----------------------------|
| const_generics.md   | ✅     | `<const N: usize>` 已全链路实现（`7bebe08` + `bfaf849`）：类型作用域登记、数组长度按参数名延迟替换、实例化期 `subst_const_expr` 按名换值并折叠（回归 `122`，负向 `32`/`33`/`34`）；**长度表达式** `[T; N + 1]` / `[0; N + 1]` / `[i32; 2 * N]` / `[i32; N - 1]` 也已支持，数组 `.len` 发射编译期常量（回归 `167_const_len_expr.zr`）。**仍不支持**：`[T; N + M]` 这类含多个参数的表达式（长度表达式按编译期常量求值，只认 `ConstParamRef` 换值的单参数形式） |
| macro_system.rs     | 🔧     | `macro_rules!` 已走 `src/macros.rs`（`58_macros.zr`）；proc-macro 未接线 |
| simd.md             | ✅     | `std/core/simd.zr`：`f32x4` / `i32x4` / `f64x2` / `i8x8` / `i64x2` / `f32x8` / `f64x4` 的 `from_array` / `to_array` / `splat` / 逐 lane 运算 / `sum` / `dot`，以及 `best_width()`（回归 `139` / `144`） |
| wasm_backend.md     | ✅     | wasm32/wasm64 目标已实现（`src/codegen/wasm.rs`） |
