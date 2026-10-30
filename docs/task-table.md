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

> P0 = 会产出错误结果但不报错。借用检查是目前唯一剩的 P0；**混合宽度元组
> 字段访问是另一个新发现的 P0**（见 P0-2）。

### P0-2 混合宽度元组字段访问读垃圾（新发现）

现状：`(i32, i64)` / `(i64, i32)` 等**混合宽度**元组的字段访问读的是未初始化
栈内存——`t.1`（第二个元素）读到地址型垃圾；`(i64, i32)` 连 `t.0` 都错。
同宽元组 `(i32, i32)` 正常（`55_tuple_pattern` 只覆盖同宽，所以没被测出）。
本地元组、按值传参、结构体字段三种位置都复现；`t.0`（首元素）在 `(i32, i64)`
上正常，说明首元素偏移对、后续偏移或整体拷贝尺寸错。属代码生成器（
`type_size_align`/`tuple_layout` 看起来对，错在下游 MIR→SSA→codegen 的
元组拷贝或字段偏移）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P0-2a | 钉住最小复现：`(i32, i64)` 的 `t.1` 读垃圾、`(i64, i32)` 的 `t.0` 读垃圾，`(i32, i32)` 正常 | ✅ `29fc950` |
| P0-2b | 定位根因：元组字面量元素用独立推断变量，期望元组类型不传播 → 字面量落成 i32，跨对齐元素读垃圾 | ✅ `29fc950` |
| P0-2c | 修复 + 回归：`tests/cases/93_mixed_width_tuple.zr`（含 `(i32, i64)` / `(i64, i32)` / 结构体字段 / 按值传参）全档通过 | ✅ `29fc950` |

**验收**：`/tmp/tup_scope.zr` 手工确认三个结构体的元组字段都读对；用例进套件全绿。

### P0-3 数组字段访问读垃圾（新发现，`caab7a0` 已修）

现状：早先 `is_aggregate` 漏了数组，`s.a`（`a: [T; N]`）在字段访问处被当成
**标量加载**，`s.a[0]` 读到垃圾（`/tmp/arr_idx.zr`：`x=0 y=0`，应 `x=1 y=3`）。
`Index`/`copy_struct_into` 早已用 `array_info` 特判数组，唯独字段访问没对齐。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P0-3a | 复现：`s.a[i]` 读垃圾、局部数组 `a[i]` 正常 | ✅ `caab7a0` |
| P0-3b | 修复：`is_aggregate` 对 `Ty::Array(..)` 返回 true（数组是地址表示的值聚合） | ✅ `caab7a0` |
| P0-3c | 回归：`tests/cases/94_array_field_hash.zr`（数组字段访问 + 元组数组嵌套） | ✅ `caab7a0` |

### P0-4 str/切片字段布局错位（`1161b65` 已修）

现状：早先 `type_size_align` 把 `str`/切片当 **8 字节**（瘦指针），`struct_layout`
却按 16 字节（`{ptr, len}`）算。结构体里 `str`/切片字段后面的字段落在偏移 8，
盖住 `len`——`S { name: str, x: i64 }` 的 `name.len` 读到 `x` 的值（`str3.zr`：
len=7 应 3）。元组/数组/枚举载荷含 `str`/切片时同样错位。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P0-4a | 复现：`S { name: str, x: i64 }` 的 `name.len` 读到 `x` | ✅ `1161b65` |
| P0-4b | 修复：`type_size_align` 对 `str`/切片返回 `(16, 8)` | ✅ `1161b65` |
| P0-4c | 回归：`tests/cases/95_str_slice_layout.zr`（str/切片字段布局） | ✅ `1161b65` |

### P0-5 切片字段初始化静默存坏数据（`273d6f4` 已修）

现状：早先 `Expression::Array` 只按**数组**期望类型传播元素类型，**切片**期望
没处理。`S { arr: []const i64 }` 用 `[10, 20, 30]` 初始化时元素默认成 i32，
`[i32; 3]` 又被字段初始化静默强转成 `[]const i64`——切片指向 i32 数据却被当
i64 读，运行期越界 SIGSEGV（`/tmp/sl_bug.zr`）。`let a: []const i64 = [10,20,30]`
更在类型检查报 `type mismatch`（let 与字段初始化不一致）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P0-5a | 复现：切片字段 i32 字面量数组 → SIGSEGV | ✅ `273d6f4` |
| P0-5b | 修复：`resolve(expected)` 为数组/切片时把元素类型 unify 给字面量 | ✅ `273d6f4` |
| P0-5c | 回归：`tests/cases/95_str_slice_layout.zr`（切片字段按期望收窄） | ✅ `273d6f4` |
| P0-6a | 复现：`let x = loop { break 7; }` 静默得 0（`ExprKind::Break` 只记标签丢弃值、`Loop` 恒 `Never`） | ✅ `87ef09a` |
| P0-6b | 修复：typeck 压入 `LoopBreakCtx` 栈（`loop`/`while`/`for` 各一项，`while`/`for` 非值），`Break(v)` 携带值、与目标循环 break 类型统一；MIR `LoopCtx.result` + `assign_if_open` 写结果临时名 | ✅ `87ef09a` |
| P0-6c | 回归：`tests/cases/97_loop_value.zr`（标量 break 值 + 累加 + 嵌套 + 语句 break + 发散循环） | ✅ `87ef09a` |
| P0-7 | **已修** `daf9763`：聚合（结构体/元组/数组）作为 `loop` break 值在 `-O3`/`-Oz` 触发寄存器分配 bug——`live_intervals` 线性位置按数组顺序排块（非 RPO），定义在 break 块、用在出口块的值区间倒挂、不分配寄存器，codegen 拿被常量 `8` 覆盖的寄存器当基址，`load [r8]` 读到地址 8 段错误。修复：线性位置改按 RPO 编号（`src/codegen/regalloc.rs`，`mir_rpo` 改 `pub(crate)`） | ✅ `daf9763` |

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
| P1-1b | **`Hash` 分布**：`str`/切片/指针/元组/数组字段当前按 `0` 计入（只保证"相等⇒同哈希"，碰撞严重）；**指针/引用字段已改为按地址计入**（`b5f5a56`：相等⇒地址相同⇒同哈希，且给出真实分布）；**元组字段已改为按内容计**（`29fc950` 修好混合宽度元组字段访问后：逐元素折叠 `a0*31 + a1`，相等⇒同哈希、不同元组大概率不同）；**数组字段已改为按内容计**（`caab7a0`：定长展开 `a[0]*31 + a[1]`，长度 ≤ 32；更大/动态长度仍按 `0`）；**`str`/切片已改为按内容计**（`41ec8f4`：`content_hash_loop` 用 `while` + 累加器折叠 `h*31 + byte`，`"abc"`=96354、`""`=0；新增 `tests/cases/96_str_slice_hash.zr`） | ✅ `41ec8f4` |
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

现状：✅ 已修（`bab298c`）。`try_instantiate_call` 推不出类型参数时留推断变量、
按它实例化，用法体（`v.push(1)` 把 `Vec<T>` 约束成 `Vec<i32>`）随后把类型定下；
`check_instance_body` 里若仍停在未解析推断变量（`let v = Vec::new();` 后 `v` 再没
被用过）才明确报 `cannot infer type parameter`（报在真实调用点）。
`tests/cases/98_generic_infer_usage.zr`、负向 `tests/negative/18_generic_infer_unused.zr`。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-4a | 泛型函数调用返回 `Vec<?T>`（`?T` 为推断变量）不立即报错，延迟到后续使用点约束 | ✅ `bab298c` |
| P1-4b | 方法调用（`v.push(1)`）把接收者的 `?T` 与实参类型 unify | ✅ `bab298c` |

### P1-5 格式串 `{name}` 与 `{:#?}` 聚合

现状：`std/io.zr` 格式引擎支持 `{}` `{:?}` `{:x}` …、位置参数、动态宽度/精度；
缺**命名参数 `{name}`** 与 **`{:#?}` 多行聚合 Debug**（聚合 Debug 未实现，受阻）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-5a | 命名参数 `{name}`：格式引擎支持按名字查值（需要调用方传名字表，或编译期改写） | ⬜ |
| P1-5b | `{:#?}` 聚合多行：递归 pretty-print（需要聚合 Debug 先落地，受阻于 P1-1c） | ⬜ |

### P1-6 元数据 / tempfile API

现状：✅ 已补（`db0b01f`）。`std/fs` 新增 `temp_file()`/`temp_file_raw()`
（`/tmp/zr_tmp_<纳秒>`，用已有 `zr_open` + `zr_time_unix_ns`）、`file_mtime(path)`
（Unix 秒）、`file_mode(path)`（`st_mode` 位）；运行时新增 `zr_file_mtime` /
`zr_file_mode`（macOS x86-64 用 `stat$INODE64` 偏移 48/4，Linux x86-64 用
`newfstatat` 偏移 88/24，其它平台暂返回 -1）；`tests/cases/99_fs_tempfile_metadata.zr`。
注：`fs::file_size(str)` 有**既有 bug**（变量路径返回 -1，字面量/`_raw` 正常），未修。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-6a | `fs::metadata` → `{ len, mode, mtime, ctime }`（`stat` syscall） | 🔧 部分（mtime/mode 已出，len 走 `file_size_raw`，ctime 未出） |
| P1-6b | `tempfile::temp_dir()` / `temp_dir_file()` | ✅ `temp_file()`/`temp_file_raw()` |

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
| P1-1b Hash 指针/引用按地址 | `b5f5a56` | ✅ |
| P1-1b Hash 元组按内容 | `29fc950` 后 | ✅ |
| P1-1b Hash 数组按内容 | `caab7a0` | ✅ |
| P1-1b Hash `str`/切片按内容 | `41ec8f4` | ✅ |
| P0-4 str/切片字段布局 | `1161b65` | ✅ |
| P0-5 切片字段按期望收窄 | `273d6f4` | ✅ |
| P0-6 `loop` 带值 break | `87ef09a` | ✅ |
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
