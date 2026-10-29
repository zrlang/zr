# zr 剩余功能任务表（可执行清单）

> 创建：2026-10-29。这是**可执行的任务跟踪表**，不是设计文档——设计细节见
> `docs/roadmap.md`（阶段 A–L、v0.9–v1.1 排期）。本表只列**源码实测确认存在**的
> 缺口，按"影响面 × 静默出错 × 能否独立完成"排序，每行是一个**可独立完成**的
> 子任务，附验收标准。大型缺口拆成多个子任务跨轮推进。
>
> 状态图例：⬜ 未开始 / 🔧 进行中 / ✅ 已提交。
>
> 基线（2026-10-29）：`feat/lang-features`，92 用例 × 5 等级 + 3 宿主端对照全绿，
> 负向 17 全绿，`--bin zr` 265 全绿。工作树干净。

---

## 阶段 0 — P0 静默错误（最高优先级，先修）

> P0 = 会产出错误结果但不报错。借用检查是目前唯一剩的 P0。

### P0-1 借用检查（NLL）——拆 4 个子任务

现状：`src/typeck/infer.rs::check_borrows` 返回空 `errors`（`borrow_intervals`
是 `Vec<BorrowInterval>`，`BorrowInterval` 是空结构体，`record_borrow()` 不传
任何参数）。这是"静默接受悬垂引用 / 重叠可变借用"的根源。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P0-1a | `BorrowInterval` 填充字段：`local`（DefId）、`is_mut`（bool）、`span`、`start`/`end` 程序点；`record_borrow` 改为接收这些参数，所有调用点传入真实借用信息 | ⬜ |
| P0-1b | 借用区间收集：扫 MIR 基本块 + 支配树（`src/ssa/domtree.rs` 已有支配树），算每个借用的活跃区间 | ⬜ |
| P0-1c | 冲突检测：`&mut T` × 任意重叠 → `E0499 cannot borrow as mutable more than once`；`&T` × `&mut T` 重叠 → `E0502 cannot borrow as immutable while mutable` | ⬜ |
| P0-1d | 回归：`tests/negative/` 加借用冲突负向用例 + `tests/cases/` 加合法借用用例 | ⬜ |

**验收**：`/tmp/borrow_*.zr` 手工确认冲突报错、合法借用不报；负向用例进套件。

---

## 阶段 1 — P1 明确缺失（其次）

### P1-1 `#[derive]` 剩余缺口——拆 4 个子任务

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-1a | **字段级诊断**：derive 生成的 `Type__eq`/`__lt` 等失败时，报错应指向**出问题的那个字段**并说清缺什么 trait，而不是 `0:0:0`（实测：`Outer { b: Inner }` 上 `derive(PartialEq)` 报 `cannot compare values of type Inner` 但位置是 `0:0:0`）。已修：`StructDef`/`EnumDef`/`Field` 带真实 span，生成的字段访问用字段 span；结构体字段精准到列（`b: Inner` → `3:24`），枚举结构体载荷也精准到字段（`B { x: Inner }` → `2:22`），元组载荷回退到枚举定义。新增宿主端测试 `derive_field_error_has_span` | ✅ |
| P1-1b | **`Hash` 分布**：`str`/切片/数组/元组/指针字段当前按 `0` 计入（只保证"相等⇒同哈希"，碰撞严重）；等这些类型有按内容比较语义后换真实哈希 | ⬜ |
| P1-1c | **`Debug` 方法体**：目前是标记 trait，`{:?}` 不经过它；需要 `std/fmt` 的 `Formatter` 与字符串拼接齐备（受阻于格式化设施） | ⬜ |
| P1-1d | 泛型类型上的 derive / 枚举 `Default` / 枚举 `Ord` 显式报错——已实现（负向 13–15），报错位置也已从 `0:0:0` 修到类型定义（`derive_methods` 用 `span` 传参） | ✅ |

### P1-2 闭包捕获 env 跨函数

现状：带捕获闭包已可存进变量 / 嵌套 / 多次调用（`84_closure_capture_value.zr`），
但**跨函数传 env** 仍报 `closures can only be invoked immediately where they are defined`。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-2a | 带捕获闭包作为**函数实参**传递（env 打包成栈上结构体，按引用/值传给被调函数） | ⬜ |
| P1-2b | 带捕获闭包作为**返回值** / 存进结构体字段（env 生命周期管理） | ⬜ |

### P1-3 `i128` / `u128` 64 位目标——拆 2 个子任务

现状：64 位目标上 `i128`/`u128` **编译期报错**（`check_int_width` 拒绝 >64 位），
曾静默截断、现改为报错。需要寄存器对拆分。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-3a | 类型检查 + MIR/SSA：`Ty::Int(128)`/`Uint(128)` 通过，算术按高低 64 位拆分 | ⬜ |
| P1-3b | x86-64 后端：寄存器对（`rdx:rax` / `rcx:rbx`）传参与返回值，基本 add/sub/mul/compare | ⬜ |

### P1-4 泛型推断从用法体推断

现状：`let v = Vec::new(); v.push(1)` 报 `cannot infer type parameter T`（实测），
`Vec::new()` 的 `T` 无实参可推、需要延迟单态化。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-4a | 泛型函数调用返回 `Vec<?T>`（`?T` 为推断变量）不立即报错，延迟到后续使用点约束 | ⬜ |
| P1-4b | 方法调用（`v.push(1)`）把接收者的 `?T` 与实参类型 unify | ⬜ |

### P1-5 格式串 `{name}` 与 `{:#?}` 聚合

现状：`std/io.zr` 格式引擎支持 `{}` `{:?}` `{:x}` …、位置参数、动态宽度/精度；
缺**命名参数 `{name}`** 与 **`{:#?}` 多行聚合 Debug**（聚合 Debug 未实现，受阻）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-5a | 命名参数 `{name}`：格式引擎支持按名字查值（需要调用方传名字表，或编译期改写） | ⬜ |
| P1-5b | `{:#?}` 聚合多行：递归 pretty-print（需要聚合 Debug 先落地，受阻于 P1-1c） | ⬜ |

### P1-6 元数据 / tempfile API

现状：`std/fs` 无权限、时间戳；无 `tempfile`。需要运行时 syscall（跨平台）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-6a | `fs::metadata` → `{ len, mode, mtime, ctime }`（`stat` syscall） | ⬜ |
| P1-6b | `tempfile::temp_dir()` / `temp_dir_file()` | ⬜ |

---

## 阶段 2 — P2 工具链与生态

### P2-1 可见性检查

现状：`Visibility`（`ast.rs:84`：`Public/Private/Crate/SelfOnly/Super`）解析后**全部丢弃**，
`typeck/mod.rs` 不读任何 `.vis`；`pub(crate)`/`pub(super)` 无检查。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-1a | 每个 def 记录其 `Visibility` + 所属模块（需要从 `modules.rs` 的 `absorb` 把模块信息带到 typeck） | ⬜ |
| P2-1b | 路径解析时：非 `pub` 项被**不同模块**引用 → `cannot find` / `private item` 报错 | ⬜ |

### P2-2 LTO / PGO 接线

现状：`src/opt/lto.rs`（`run_lto(Vec<SsaModule>, LtoMode)` + `cross_module_inline`）、
`src/opt/pgo.rs` 存在但 `opt/mod.rs` 只声明 `pass_manager`；单模块流水线，无独立
`SsaModule`。LTO 在单模块下是无操作合并器。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-2a | 评估是否接线（先确认 pipeline 是否需要多模块，再决定）；当前单模块下 LTO 是 no-op | ⬜ |

### P2-3 `--emit-llvm` 真 IR / 删伪 IR

现状：`src/main.rs:907 emit_llvm_ir` 手写伪 IR（整数统一 i64、指针 i8*），`clang`
消费不了。roadmap §8.5 决策：**删**（或 v1.0 后真做）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-3a | 按 roadmap 决策删除伪 IR 或标 `--emit-llvm` 为实验性（未定） | ⬜ |

### P2-4 C 后端接 CLI / 自举

现状：`src/codegen/c_backend.rs` 无 `--emit-c` 入口；`src/zr0/` 未 `mod` 声明。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-4a | `--emit-c` CLI 入口接 `c_backend.rs`（自举关键路径，roadmap §8.6 决策：真做） | ⬜ |

### P2-5 宏系统 / `yield` / const 泛型 / trait 高级特性

现状：`src/macro_system/` 未 `mod` 声明（`macro_rules!` 走 `src/macros.rs`）；`yield`
是类型占位；`const` 泛型解析接受但检查报错；trait 高级特性（关联类型投影、HRTB、
特化、孤儿规则、负边界、`impl Trait`）全缺。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-5a | 关联类型投影（`Self::Output`/`P::Output`）：`HirAssocType` 从空结构体升级为记录名字 + impl 绑定，`resolve_type_path` 支持投影（roadmap v0.11） | ⬜ |
| P2-5b | `yield` 协程：从类型占位升级为可执行（frame 状态机） | ⬜ |
| P2-5c | `const` 泛型 `<const N: usize>`：检查期接受并支持数组长度参数 | ⬜ |
| P2-5d | 负边界 / 特化 / 孤儿规则 / `impl Trait` 参数 | ⬜ |

---

## 阶段 3 — 已完成的缺口（核对后归档）

> 以下为已提交并全绿验证的项，留档便于查证。

| 项 | 提交 | 状态 |
|---|---|---|
| P1 #5 穷尽性措辞 | `a526953` | ✅ |
| P1 #7 Option/Result prelude | `4ed15d7` | ✅ |
| P1-1a derive 字段级诊断 | `bcf29f6` `9238875` | ✅ |
| P1-1d derive 拒绝文案 + 位置 | `60174ca` | ✅ |
| P2 #8 glob 导入 `use foo::*` | `40b5b45` | ✅ |
| P3 文档债（全部 6 项） | `fb4dfb2` `1d38ca3` | ✅ |
| §4 多边界泛型 | `8d68ae3` | ✅ |
| §4 cfg 过滤 | `60b4997` | ✅ |
| §4 工具链子命令/发射开关 | `6ea5ed5` `4331dee` | ✅ |
| §4 static mut 盲区核实 | `4ab802d` | ✅ |
| §4 f8/f16 盲区核实 | `03fe185` | ✅ |
| §3.4/§4 关联类型投影声明修正 | `0a7895d` | ✅ |

---

## 使用约定

- 每完成一个子任务：更新状态为 ✅、跑全绿（`/tmp/run_all2.sh` + `cargo test --test
  zr_negative` + `cargo test --bin zr`）、提交（中文 Conventional Commits）。
- 每轮（自动 continuation）至少推进**一个子任务**；大型子任务（如 P0-1a–d）无法
  一次完成时，按行继续拆。
- 文档同步：落地任何改动都要更新 `README.md` / `docs/rust-parity.md` /
  `future/README.md` / `tests/README.md` / `CONTRIBUTING.md`（见 `docs/roadmap.md` §9）。
- 红线：不在 `src/zr0/`、`src/macro_system/`、`src/opt/lto.rs`、`src/opt/pgo.rs`、
  `src/codegen/c_backend.rs` 里"修 bug"，先确认接线。
