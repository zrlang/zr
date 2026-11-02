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

### P0-8 `let` 解构模式静默未初始化（已修）

现状：`Statement::Let` 早先算完 `hir_pat` 后只取 `first_binding(...)` 当**唯一**
绑定，而它只认简单绑定 `x`——元组 / 结构体 / 切片模式的子绑定由
`lower_pattern` 分配却**永不写值**：`let (a, b) = (3, 4); a + b` 编译通过、
运行 exit 192（应 7）；`let [a, b] = arr;` 同理（exit 208）。切片模式此前在
`match` 里也直接报 `match on slice patterns is not implemented yet`。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P0-8a | 复现：`let (a, b)` / `let [a, b]` 均静默读栈上垃圾 | ✅ |
| P0-8b | 修复：`let` 非简单绑定时展开成"临时 scrutinee + 逐子值 `HirStmt::Let`"，复用 `pat_match`；可反驳模式（字面量/范围/变体/切片长度）报 `refutable pattern in local binding; use \`let ... else\``；`HirPattern::Slice` 携带子模式，`pat_match` 生成长度条件（定长数组按编译期长度、切片按运行期 `.len`）与 `scrut[i]` 绑定 | ✅ |
| P0-8c | 回归：`tests/cases/121_let_destructure.zr`（元组/嵌套/结构体/数组切片/切片 `let ... else`/match 切片模式）、负向 `tests/negative/31_let_refutable.zr` | ✅ |

### P0-1 借用检查（NLL）——拆 4 个子任务

现状：**已实现最小切片**（`src/typeck/mod.rs`——注意 `src/typeck/infer.rs::check_borrows`
仍是未接线的空壳残留，真正的检查不在那里）。同位置借用重叠报
`cannot borrow ... while already borrowed`；写被借用位置报
`cannot assign to ... while it is borrowed`；读被可变借用位置报
`cannot read ... while it is mutably borrowed`；NLL 按 last-use 近似
（`finish_borrow_accesses` 在函数体检查完后统一判定）。已知局限：多绑定 /
未关联借用按作用域结束、跨作用域重名可能混淆 last-use。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P0-1a | 借用记录：位置（DefId）、可变性、span、起止程序点 | ✅ `ff5ff77` |
| P0-1b | 借用区间 + last-use 近似（写/读被借用位置检测） | ✅ `14154ba` `a53a750` |
| P0-1c | 冲突检测：`&mut` × 任意重叠、`&` × `&mut` 重叠 | ✅ `ff5ff77` |
| P0-1d | 回归：`tests/negative/20_borrow_conflict.zr`、`21_borrow_write.zr`、`22_borrow_read.zr` + `tests/cases/105_borrow_nll.zr` | ✅ |

---

## 阶段 1 — P1 明确缺失（其次）

### P1-1 `#[derive]` 剩余缺口——拆 4 个子任务

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-1a | **字段级诊断**：derive 生成的 `Type__eq`/`__lt` 等失败时，报错应指向**出问题的那个字段**并说清缺什么 trait，而不是 `0:0:0`（实测：`Outer { b: Inner }` 上 `derive(PartialEq)` 报 `cannot compare values of type Inner` 但位置是 `0:0:0`）。已修：`StructDef`/`EnumDef`/`Field` 带真实 span，生成的字段访问用字段 span；结构体字段精准到列（`b: Inner` → `3:24`），枚举结构体载荷也精准到字段（`B { x: Inner }` → `2:22`），元组载荷回退到枚举定义。新增宿主端测试 `derive_field_error_has_span` | ✅ |
| P1-1b | **`Hash` 分布**：`str`/切片/指针/元组/数组字段当前按 `0` 计入（只保证"相等⇒同哈希"，碰撞严重）；**指针/引用字段已改为按地址计入**（`b5f5a56`：相等⇒地址相同⇒同哈希，且给出真实分布）；**元组字段已改为按内容计**（`29fc950` 修好混合宽度元组字段访问后：逐元素折叠 `a0*31 + a1`，相等⇒同哈希、不同元组大概率不同）；**数组字段已改为按内容计**（`caab7a0`：定长展开 `a[0]*31 + a[1]`，长度 ≤ 32；更大/动态长度仍按 `0`）；**`str`/切片已改为按内容计**（`41ec8f4`：`content_hash_loop` 用 `while` + 累加器折叠 `h*31 + byte`，`"abc"`=96354、`""`=0；新增 `tests/cases/96_str_slice_hash.zr`） | ✅ `41ec8f4` |
| P1-1c | **`Debug` 方法体**：`#[derive(Debug)]` 生成 `Type__debug(self) -> String`（Rust `{:?}` 风格）：命名/元组结构体（空结构体只打名字）、单元/元组/结构体变体枚举、嵌套类型递归 `Inner__debug`（偏移 0 已对齐）；整数/无符号/`bool`/`char`/`str`/`[]const u8` 直接渲染；浮点/指针/数组/元组/`String` 字段显式报错（负向 `19_debug_float.zr`）；新增 `tests/cases/100_debug_derive.zr`（长度 + 立即字节断言）。**两处组合错也已对齐 Rust**：结构体变体载荷里的 `str` 字段、命名结构体里 `str` 字段之后的嵌套结构体字段（都是 `EnumField` 聚合特判一并修好的）——见 `AGENTS.md` §7 | ✅ |
| P1-1d | 泛型类型上的 derive / 枚举 `Default` / 枚举 `Ord` 显式报错——已实现（负向 13–15），报错位置也已从 `0:0:0` 修到类型定义（`derive_methods` 用 `span` 传参） | ✅ |

### P1-2 闭包捕获 env 跨函数

现状：带捕获闭包已可存进变量 / 嵌套 / 多次调用（`84_closure_capture_value.zr`）；
**跨函数传环境**（把环境结构体按值交给另一个函数、被调方再调用它）也已支持——
zr 的做法是让被调方是**泛型**函数（`fn apply<F>(f: F, ..)`）：`F` 单态化到环境
结构体 `__env_N`，被调函数体里的 `f(x)` 按环境 DefId 查 `closures_by_env`、
`apply_capture_body` 把捕获 local 替换成形参 `f.__cN` 后内联展开
（`109_closure_env_cross_fn.zr`）。

仍**明确报错**（不是静默出错）的两条路径，都是"环境值不是函数指针"的直接后果：

- 带捕获闭包作为**实参**传给 `fn(..)` 形参 → 报
  `captured closure cannot be passed as a plain `fn` value`（负向 `07_closure_capture.zr`）。
  要支持它需要 `Fn` 边界 / 胖指针（`{code, env}`）与配套调用约定，属**独立大工程**，
  与 zr 的"单态化优先、不做默认动态分发"取向相悖，暂不做；泛型形参是当前正解。
- 带捕获闭包作为 `fn(..)` 的**返回值** → 早先**编译通过、运行 SIGSEGV**（环境结构体
  被 MIR 当函数地址 `call`），现报
  `captured closure cannot be returned as a plain `fn` value`（负向 `25_closure_return.zr`）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-2a | 带捕获闭包作为**函数实参**传递给**泛型**形参（env 打包成栈上结构体按值传入，被调方内联展开）——✅。连带修掉 `substitute_local` 的 `Index` 分支只替换下标、不替换数组基址的静默 bug（数组捕获跨函数内联读别的栈帧、SIGSEGV；`Array`/`Tuple` 字面量子表达式同因），回归 `109_closure_env_cross_fn.zr` | ✅ |
| P1-2b | 带捕获闭包作为**返回值** / 存进结构体字段——✅（泛型路径）。环境值就是普通值：`fn idf<F>(f: F) -> F { return f; }` 透传返回、`struct Holder<F> { f: F }` 存字段，单态化把 `F` 定到 `__env_N`，之后 `f(x)` / `h.f(x)` 照常按环境 DefId 内联展开；捕获是值拷贝（改外层不影响闭包）。回归 `110_closure_env_generic_passthrough.zr`。**裸 `fn(..)` 返回值**这条仍是明确报错（负向 `25_closure_return.zr`），归 P1-2c | ✅ |
| P1-2c | `fn(..)` 形参 / 返回值接收带捕获闭包（`Fn` 边界 / 胖指针 `{code, env}` + 调用约定）——需独立设计，暂不做（与 zr"单态化优先、不做默认动态分发"取向相悖） | ⬜ |


### P1-3 `i128` / `u128` 64 位目标——拆 2 个子任务

现状：✅ 已在 64 位 x86-64 目标上打通。`i128`/`u128` 拆成两个连续的 8 字节
栈槽（lo / hi），加减用 `adc`/`sbb` 接进位、移位用 `shld`/`shrd`、比较按
"先比高位、高位相等再比低位"定序，传参 / 返回走寄存器对 `rdx:rax`。
乘 / 除 / 取模需要更长的算法（乘拆四段、除走软件长除），暂不支持——显式报错，
绝不静默给错值。回归 `tests/cases/111_i128.zr`。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-3a | 类型检查 + MIR/SSA：`Ty::Int(128)`/`Uint(128)` 通过，算术按高低 64 位拆分 | ✅ |
| P1-3b | x86-64 后端：寄存器对传参 / 返回 / add/sub/位运算/移位/比较/转换；mul/div/rem 显式报错（任务表原写 `rdx:rax` / `rcx:rbx`，本后端无 `rbx`，实际用 `rdx:rax`；两个 8 字节栈槽而非寄存器对） | ✅ |

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
**命名参数 `{name}` 已实现**（`io::println_fmt_named(fmt, &[NamedArg], n)`，
`0d40ab1`）。**`{:#?}` 多行聚合 Debug 已全线打通**：derive 侧 `#[derive(Debug)]`
生成 `.debug_pretty()` / `.debug_pretty_at(e, ind)`（输出对齐 Rust `{:#?}`）；
格式引擎侧新增**类型化值通道**——`FormatArg` 加 `kind = 7`（custom）+ `obj_ptr`
+ `debug_fn` 函数指针，调用方写 `fmt_debug(&v as *const void, Type__debug_fmt)`
即可把自定义类型交给 `{:?}` / `{:#?}`（`#` 在运行期决定走 `.debug()` 还是
`.debug_pretty()`）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-5a | 命名参数 `{name}`：格式引擎按名字查值（`NamedArg { name, value }`，名字与值同结构体） | ✅ `0d40ab1` |
| P1-5b | `{:#?}` 多行 derive 侧：`#[derive(Debug)]` 生成 `Type__debug_pretty`（入口）/ `Type__debug_pretty_at(e, ind)`（递归 worker；缩进是**运行期**参数，同一类型不同深度都对齐 Rust）；`String::push_indent` 做缩进。`108_debug_pretty.zr` 逐个对照 `rustc {:#?}` | ✅ `13acff2` |
| P1-5c | 格式引擎接 `{:?}` / `{:#?}`：`FormatArg` 加类型化值通道（`kind = 7` + `obj_ptr` + `debug_fn`），`derive` 生成 `Type__debug_fmt(*const void, bool, *mut u8, usize) -> usize`，`io::fmt_debug` 组装参数。`tests/cases/112_fmt_debug.zr` + `io_print_matches_rust` 逐字节对照 Rust | ✅ |

### P1-6 元数据 / tempfile API

现状：✅ 已补（`db0b01f`；ctime 于 2026-11-01 补）。`std/fs` 新增 `temp_file()`/
`temp_file_raw()`（`/tmp/zr_tmp_<纳秒>`，用已有 `zr_open` + `zr_time_unix_ns`）、
`file_mtime(path)`（Unix 秒）、`file_mode(path)`（`st_mode` 位）、`file_ctime(path)`
（状态变更时间，Unix 秒）；运行时新增 `zr_file_mtime` / `zr_file_mode` / `zr_file_ctime`
（macOS x86-64 用 `stat$INODE64` 偏移 48/4/64，Linux x86-64 用
`newfstatat` 偏移 88/24/104，其它平台暂返回 -1）；`tests/cases/99_fs_tempfile_metadata.zr`
（ctime 值已对照系统 `stat -f %c` 验证）。
早先标注的 `fs::file_size(str)`"变量路径返回 -1"实为**命名冲突**：`io::file_size(fd)`
与其同名、撞成一个 `z_file_size` 符号、MIR 去重只留首个，调用点可能走到 fd 版。
已修：`io::file_size` 改名 `file_size_fd`，`fs::file_size` 现在是唯一路径版。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-6a | `fs::metadata` → `{ len, mode, mtime, ctime }`（`stat` syscall） | ✅ `file_size`/`file_mode`/`file_mtime`/`file_ctime` + `_raw` 版齐 |
| P1-6b | `tempfile::temp_dir()` / `temp_dir_file()` | ✅ `temp_file()`/`temp_file_raw()` |

### P1-7 `String::push_bytes` 扩容算错导致越界写堆

现状：✅ 已修。`push_bytes` 需要扩容时传的是 `reserve(new_len - capacity)`
（`new_len = len + bytes.len`），而 `reserve(additional)` 只保证
`capacity >= len + additional`——因此只有当 `len == capacity` 时才够，其余
情况一律少 reserve，紧接着的拷贝循环越界写堆。症状：循环里 `push_byte`
反复 realloc 后 `push_bytes` 多字节字面量，越界写覆写相邻对象（字面量 `str`
的 `ptr` 被写成 `s.ptr`），拷贝出 `ABCD`+NUL，最坏 `SIGABRT`/`SIGSEGV`。
**早先被误判为代码生成器缺陷**（寄存器定错 / 局部槽别名）——实为标准库
`reserve` 算错，代码生成器无缺陷；这也是 P1-5b `{:#?}` 被"阻塞"的真因。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-7a | 复现：循环 realloc + 之后 `push_bytes` 多字节字面量（`/tmp/hand17.zr`，未修版 exit 134/139） | ✅ |
| P1-7b | 修复：改传 `reserve(bytes.len)` | ✅ |
| P1-7c | 回归：`tests/cases/107_string_push_bytes_reserve.zr` 五档全绿（未修版全红） | ✅ |

---

## 阶段 2 — P2 工具链与生态

### P2-1 可见性检查

现状：✅ 已实现（`e0c5541`）。解析阶段保留 `pub` / `pub(crate)` / `pub(super)`
（早先 `_visibility` 直接丢弃），模块加载器给每个条目打上声明模块路径；类型检查
在 `resolve_name` / `resolve_path` 里按"条目可见性 × 引用方模块"判定
（`check_visibility` / `visibility_allows`）：`pub` / `pub(crate)` 单 crate 内处处
可见；`pub(super)` 对父模块及其后代可见；私有 / `pub(self)` 对声明模块及其后代
可见。负向用例 `23_visibility_private.zr`、`24_visibility_super.zr`。
已知局限：`use m::item` 导入私有条目的检查在 `use` 点未做；`resolve_name` 的
`use_aliases` 分支绕过可见性检查。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-1a | 每个 def 记录其 `Visibility` + 所属模块（需要从 `modules.rs` 的 `absorb` 把模块信息带到 typeck） | ✅ `e0c5541` |
| P2-1b | 路径解析时：非 `pub` 项被**不同模块**引用 → `cannot find` / `private item` 报错 | ✅ `e0c5541` |

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

现状：`src/codegen/c_backend.rs` 已由 `--emit-c` 接线（`src/main.rs` CLI 解析 + 从
HIR 直译成一个 C 翻译单元）；`src/zr0/` 仍未 `mod` 声明。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-4a | `--emit-c` CLI 入口接 `c_backend.rs`（自举关键路径，roadmap §8.6 决策：真做） | ✅ 已接线（`src/main.rs` 的 `--emit-c` 分支） |

### P2-5 宏系统 / `yield` / const 泛型 / trait 高级特性

现状：`src/macro_system/` 未 `mod` 声明（`macro_rules!` 走 `src/macros.rs`）；`yield`
是类型占位；`const` 泛型解析接受但检查报错；trait 高级特性里**关联类型投影
（含 `<T as Tr>::Item` 限定写法）、`impl Trait` 参数位置脱糖、孤儿规则/重叠检查
已实现**，仍缺 HRTB、负边界、`default fn` 特化、RPIT。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-5a | 关联类型投影（`Self::Output`/`P::Output`/`<T as Tr>::Item`）：`resolve_assoc_type` + `generic_impl_assocs`，限定写法带 trait 校验 | ✅ 已实现（回归 `118` / `119`，负向 `29`） |
| P2-5b | `yield` 协程：从类型占位升级为可执行（frame 状态机） | ⬜ |
| P2-5c | `const` 泛型 `<const N: usize>`：检查期接受并支持数组长度参数 | ⬜ |
| P2-5d | 负边界 / 特化 / 孤儿规则 / `impl Trait` 参数 | 🟡 孤儿规则 + 重叠检查已接线（`check_orphan_rule` / `check_overlap`）；特化只有 impl 选择、无 `default fn`。**`impl Trait` 参数（APIT）已脱糖成匿名泛型参数并真校验边界**（`120`，负向 `30`）；返回位置（RPIT）仍是 `opaque` 占位。负边界 / HRTB 仍缺 |

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
| P1-1c Debug 方法体 | `01ae31d` | ✅ |
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
