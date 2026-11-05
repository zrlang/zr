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

仍**明确报错**（不是静默出错）的两条路径，由 P1-2c 解决（函数值 = 16 字节
记录 `{code, env}`）：

- ~~带捕获闭包作为**实参**传给 `fn(..)` 形参 → 报 `captured closure cannot be
  passed as a plain `fn` value`（负向 `07_closure_capture.zr`）~~：已支持，
  `coerce_closure_to_fn` 把环境值搬进记录（env 复制到堆）、`code` 指向特化
  thunk。原负向用例因此删除。
- ~~带捕获闭包作为 `fn(..)` 的**返回值** → 早先**编译通过、运行 SIGSEGV**
  （环境结构体被 MIR 当函数地址 `call`），现报 `captured closure cannot be
  returned as a plain `fn` value`（负向 `25_closure_return.zr`）~~：已支持，
  同一套 `ClosureToFn` 降级。原负向用例删除。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P1-2a | 带捕获闭包作为**函数实参**传递给**泛型**形参（env 打包成栈上结构体按值传入，被调方内联展开）——✅。连带修掉 `substitute_local` 的 `Index` 分支只替换下标、不替换数组基址的静默 bug（数组捕获跨函数内联读别的栈帧、SIGSEGV；`Array`/`Tuple` 字面量子表达式同因），回归 `109_closure_env_cross_fn.zr` | ✅ |
| P1-2b | 带捕获闭包作为**返回值** / 存进结构体字段——✅（泛型路径）。环境值就是普通值：`fn idf<F>(f: F) -> F { return f; }` 透传返回、`struct Holder<F> { f: F }` 存字段，单态化把 `F` 定到 `__env_N`，之后 `f(x)` / `h.f(x)` 照常按环境 DefId 内联展开；捕获是值拷贝（改外层不影响闭包）。回归 `110_closure_env_generic_passthrough.zr`。**裸 `fn(..)` 返回值**这条仍是明确报错（负向 `25_closure_return.zr`），归 P1-2c | ✅ |
| P1-2c | `fn(..)` 形参 / 返回值接收带捕获闭包——✅ 已实现：函数值统一是 **16 字节记录 `{code, env}`**（MIR 里按聚合值处理，表达式的值是它的存储地址），`code` 恒指向一个"末尾多收一个 `env`"的 shim（普通函数是转发 shim，env 忽略；带捕获闭包是特化 thunk，体内捕获 local 换成 `env.__cN`，环境在 MIR 降级时用 `zr_alloc` 复制到堆），间接调用统一按 `(用户实参..., env, [隐藏返回槽])` 传参。`extern "C"` 边界由 `FnToCodePtr` 退化回裸代码地址（普通函数直接取真实符号，绕开多一个形参的 shim）。回归 `125_closure_to_fn.zr`；负向 `38_generic_fn_as_value.zr` / `39_async_fn_as_value.zr`（泛型 / 异步函数当值明确报错）。**仍不支持**：带捕获闭包经 `extern "C"` 传给 C 侧回调（env 无处安放，除非 C 侧原样回传环境指针） | ✅ |


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

现状：`std/std/io.zr` 格式引擎支持 `{}` `{:?}` `{:x}` …、位置参数、动态宽度/精度；
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
| P2-2a | 评估是否接线（先确认 pipeline 是否需要多模块，再决定）；当前单模块下 LTO 是 no-op | ✅ 评估完成：**不接线**。理由：(1) `modules.rs` 把全部源文件扁平成**一个**编译单元，整个程序只有一个 `SsaModule`，`run_lto(Vec<SsaModule>)` 永远只收一个模块，跨模块内联无对象可并——要让 LTO 有意义得先把流水线改成"每模块一个 codegen unit + 链接时合并"，属 v0.13 的架构改动；(2) PGO 需要 `-C profile-generate` 插桩、运行时计数器落盘、`pgomerge` 合并、`-C profile-use` 二次编译的完整闭环，CLI 连 `-C` 开关都不存在，插桩还要改 MIR 降级路径；(3) roadmap 明确把 LTO/PGO 排在 v0.13（性能阶段），当前语言特性阶段提前接线只会多出两条无人验证的代码路径。`src/opt/lto.rs` / `src/opt/pgo.rs` 保持不声明，留待 v0.13 重新评估 |

### P2-3 `--emit-llvm` 真 IR / 删伪 IR

现状：`src/main.rs::emit_llvm_ir` 手写伪 IR（整数统一 i64、指针 i8*），`clang`
消费不了。roadmap §8.5 决策：**删**（或 v1.0 后真做）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-3a | 按 roadmap 决策删除伪 IR 或标 `--emit-llvm` 为实验性（未定） | ✅ 已标实验性：help 文案改 "Emit pseudo LLVM IR (experimental; not consumable by clang)"，发射分支向 stderr 打实验性警告；`toolchain_subcommands_emit` 断言警告存在（选"标"而非"删"：删除会打破工具链回归，伪 IR 对人眼检查 SSA 形状仍有价值） |

### P2-4 C 后端接 CLI / 自举

现状：`src/codegen/c_backend.rs` 已由 `--emit-c` 接线（`src/main.rs` CLI 解析 + 从
HIR 直译成一个 C 翻译单元）；`src/zr0/` 仍未 `mod` 声明。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-4a | `--emit-c` CLI 入口接 `c_backend.rs`（自举关键路径，roadmap §8.6 决策：真做） | ✅ 已接线（`src/main.rs` 的 `--emit-c` 分支） |

### P2-5 宏系统 / `yield` / const 泛型 / trait 高级特性

现状：`src/macro_system/` 未 `mod` 声明（`macro_rules!` 走 `src/macros.rs`）；`yield`
**已从类型占位升级为可执行的协作让出点**（async fn 内的 `Suspend` 由既有状态机
拆成 Pending + 续体，`zr_run_tasks` 轮询恢复；同步函数与闭包体里的 `yield` 报错）；
`const` 泛型**已实现**（`<const N: usize>` 进类型作用域，数组长度
按参数名延迟替换，实例化后支持结构体 / 函数 / `impl` 方法与值位置使用，
**长度表达式 `[T; N + 1]` / `[0; N + 1]` 也已支持**）；trait 高级特性里**关联类型投影
（含 `<T as Tr>::Item` 限定写法）、`impl Trait` 参数位置脱糖（含 trait 方法签名）、
孤儿规则/重叠检查、trait 默认方法（带方法体的方法 / `default fn` 拼写）、
**负边界 `where T: !Trait`、HRTB `for<'a>`、RPIT（`opaque_hidden` 驱动字段/方法访问）**
已全部实现。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P2-5a | 关联类型投影（`Self::Output`/`P::Output`/`<T as Tr>::Item`）：`resolve_assoc_type` + `generic_impl_assocs`，限定写法带 trait 校验 | ✅ 已实现（回归 `118` / `119`，负向 `29`） |
| P2-5b | `yield` 协程：从类型占位升级为可执行（frame 状态机） | ✅ 已实现（回归 `123`，负向 `35` / `36`） |
| P2-5c | `const` 泛型 `<const N: usize>`：检查期接受并支持数组长度参数 | ✅ 已实现（回归 `122`，负向 `32` / `33` / `34`） |
| P2-5d | 负边界 / 特化 / 孤儿规则 / `impl Trait` 参数 | ✅ 孤儿规则 + 重叠检查已接线（`check_orphan_rule` / `check_overlap`）；特化只有 impl 选择（更 specific 的 impl 优先，回归 `spec1` 手工探针）。**trait 默认方法已实现**（2026-11-02）：带方法体的 trait 方法在 impl 未覆盖时合成到目标类型，`default fn` 拼写（trait / impl 两侧）可解析；缺失必需方法在 impl 块报 `not all trait items implemented`（回归 `124`，负向 `37`；局限：泛型 impl 的毯覆方法解析仍不支持）。**`impl Trait` 参数（APIT）已脱糖成匿名泛型参数并真校验边界**（`120`，负向 `30`；trait 方法签名也已脱糖，`170`）；**返回位置（RPIT）已可用**（`167_rpit_field.zr`，负向 `47`/`48`）：`opaque_hidden` 记录隐藏类型，字段/方法访问都按它解析、MIR 布局按它传/返回。**负边界已实现**（`168_negative_bounds.zr`，负向 `38`）：`Type::Not` + `WherePred` 存储，按 `(trait_id, negated)` 求解，违反报 `unexpectedly implements trait`。**HRTB 已实现**（`169_hrtb.zr`）：`for<'a> T: Trait<'a>` 解析完整，生命周期参数整体跳过（无语义但语法可用）。**仍不支持**：`impl Fn(T) -> T`（无 `Fn` trait 语法）、泛型 impl 的毯覆方法解析、真正的 HRTB 语义（`'a` 未参与类型检查） |

---

## 阶段 3 — 标准库平台能力（argparse / env / json / xml / toml）

> 2026-11-02 新增。五项能力全部落在 `std/`：命令行参数、环境变量、JSON、XML、TOML。
> 不标"暂不做"——每一项都拆到可独立实现、可回归的子任务。
>
> 约束（来自扁平符号表）：新增模块的**公开自由函数名必须全局唯一**，因此
> JSON/XML/TOML 的入口统一带模块前缀（`json_parse` / `xml_parse` / `toml_parse`），
> 其余 API 用 `impl` 方法（按类型解析，不会撞名）。每个模块都必须能被
> `use <m>::*;` + 空 `main` 全链路加载。

### P3-1 命令行参数（std/std/argparse.zr，原 std/cli.zr）

现状：✅ `_start` 在调用 `main` 之前把 `(argc, argv)` 交给 `zr_args_init`，
运行时把二者存进 `__DATA/__bss`，`std/std/argparse.zr` 只做薄封装（把 C 字符串复制成
`String`；**2026-11-02 已由 `std/cli.zr` 重命名为 `argparse`**，并补上按参数名
读取，见阶段 4 的 P4-1）。各平台入口约定：

| 平台 | 入口处参数位置 | 入口桩动作 |
|---|---|---|
| Linux x86-64 | `[rsp]`=argc、`rsp+8`=argv | `mov rdi,[rsp]` / `lea rsi,[rsp+8]` 后 `call zr_args_init` |
| macOS x86-64 | `rdi`=argc、`rsi`=argv（dyld 传入） | 直接 `call zr_args_init` |
| Linux aarch64 | `[sp]`=argc、`sp+8`=argv | `ldr x0,[sp]` / `add x1,sp,#8` 后 `bl zr_args_init` |
| macOS arm64 | `x0`=argc、`x1`=argv | 直接 `bl zr_args_init` |
| Windows | 无 CRT 参数；运行时用 `GetCommandLineA` 解析 | `zr_args_init` 忽略实参，惰性解析命令行 |

`envp` 不需要单独传递：按 ABI 它紧跟 `argv`（`argv + (argc+1)*8`），
由 `zr_args_init` 一并记下，供 P3-2 使用。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P3-1a | 运行时就绪：`zr_args_init(argc, argv)` + `zr_args_count()` / `zr_args_ptr(i)`（越界返回 null） | ✅ |
| P3-1b | 六个 `_start` 入口桩按上表捕获参数（x86-64 / aarch64，Linux / macOS / Windows） | ✅ |
| P3-1c | `std/std/argparse.zr`（现 `argparse`，原 `std/cli.zr`）：`arg_count()`、`arg(i) -> String`、`arg_opt(i) -> Option<String>`、`args() -> Vec<String>`、`program_name() -> String` | ✅ |
| P3-1d | 回归 `tests/cases/126_cli_args.zr`：`arg_count() >= 1`、`arg(0)` 非空、越界返回空串 / `None`（运行器传入额外参数时不崩溃） | ✅ |

**验收**：编译出的二进制被运行时，`arg_count()` 与真实 `argc` 一致；
`arg(0)` 是程序路径；`use argparse::*; fn main(){}` 可全链路编译链接。

### P3-2 环境变量（std/std/env.zr）

现状：✅ 环境变量初值来自 P3-1a 一并记下的入口 `envp`，读取经运行时
`zr_env_count()` / `zr_env_entry(i)` 返回只读条目；写操作不碰运行时，
统一落在 `std/std/env.zr` 自己的**覆盖表**里。

实现：覆盖表是堆上的 `*const u8` 数组，每项一条 C 字符串——`"KEY=VALUE"`
表示设置，只有 `"KEY"`（没有 `=`）表示删除（墓碑）。查找**从后往前**扫
覆盖表（后写优先），命中墓碑即视为未设置；没有覆盖时才扫入口 `envp`。
`std/std/env.zr` 负责 `str` → NUL 结尾缓冲与结果 → `String` 的复制。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P3-2a | 运行时 `zr_env_count() -> usize`、`zr_env_entry(i) -> *const u8`（六平台汇编）；覆盖表在 `std/std/env.zr` 内实现 | ✅ |
| P3-2b | `std/std/env.zr`：`var(key) -> Option<String>`、`var_or(key, default) -> String`、`set_var(key, value) -> bool`、`remove_var(key) -> bool`、`has_var(key) -> bool` | ✅ |
| P3-2c | 回归 `tests/cases/127_env_vars.zr`：设置 → 读取 → 覆盖 → 删除；不存在的键返回 `None` / 默认值 | ✅ |

**验收**：同一进程内 `set_var("ZR_TEST_X","1")` 后 `var` 立即读到 `"1"`，
覆盖后读到新值，`remove_var` 后回到 `None`；未设置时 `var_or` 返回默认值。

### P3-3 JSON（std/alloc/json.zr）

现状：✅ `std/alloc/json.zr` 已实现，解析 / 访问 / 生成全部可用。

类型：`Json`（`Null` / `Bool` / `Int` / `Float` / `Str` / `Array` / `Object`，
递归经 `Vec` 破环）、`JsonPair { key, value }`、`JsonError { pos, msg }`。
解析是手写游标，出错带字节位置；生成分紧凑与两空格缩进两种。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P3-3a | 类型 + 访问器：`as_bool` / `as_int` / `as_float` / `as_str` / `at(i)` / `get(key)` / `len` / `type_name` | ✅ |
| P3-3b | `json_parse(str) -> Result<Json, JsonError>`：空白、`null`/`true`/`false`、整数与浮点（含指数）、字符串转义（`\" \\ \/ \b \f \n \r \t` 与 `\uXXXX`，代理对）、数组、对象；重复键保留后写 | ✅ |
| P3-3c | `json_stringify(&Json) -> String`、`json_stringify_pretty(&Json, indent) -> String`（转义 `"` `\` 与 <0x20 控制字符） | ✅ |
| P3-3d | 回归 `tests/cases/128_json.zr`：解析嵌套对象/数组/转义/数字，再生成并逐字节断言；负向不合法输入返回 `Err` | ✅ |

**验收**：`json_parse` 对嵌套样例返回正确树；`json_stringify` 的输出可被
再次 `json_parse` 解析且内容一致；空输入 / 截断输入返回 `Err` 而非崩溃。

### P3-4 XML（std/alloc/xml.zr）

现状：✅ `std/alloc/xml.zr` 已实现，解析 / 访问 / 生成全部可用。

类型：`XmlNode { name, attrs, children, text }`、`XmlAttr { name, value }`、
`XmlError { pos, msg }`。解析支持 XML 声明、注释、DOCTYPE、CDATA、自闭合
标签、属性（单/双引号）、实体（`&amp; &lt; &gt; &quot; &apos;` 与
`&#NN;` / `&#xHH;`）；生成时转义 `& < > " '`。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P3-4a | 类型 + 访问器：`attr(name)` / `child(i)` / `child_named(name)` / `text_of(name)` | ✅ |
| P3-4b | `xml_parse(str) -> Result<XmlNode, XmlError>`：单根校验、标签配对、属性、文本、CDATA、注释、DOCTYPE、实体解码 | ✅ |
| P3-4c | `xml_stringify(&XmlNode) -> String`（只序列化节点，不带 XML 声明）与 `xml_stringify_pretty(&XmlNode, indent) -> String` | ✅ |
| P3-4d | 回归 `tests/cases/129_xml.zr`：带属性/嵌套/实体/CDATA 的样例；生成后重新解析一致；未闭合标签返回 `Err` | ✅ |

**验收**：解析 → 生成 → 再解析得到同样的树；自闭合标签与文本混排正确；
不匹配的结束标签返回带位置的 `Err`。

### P3-5 TOML（std/alloc/toml.zr）

现状：✅ `std/alloc/toml.zr` 已实现，解析 / 访问 / 生成全部可用。

类型：`Toml`（`Integer` / `Float` / `Bool` / `Str` / `Array` / `Table`）+ 
`TomlEntry { key, value }` + `TomlError { pos, msg }`。解析覆盖配置文件的
常用子集：注释、`[table]` 与点分键（`a.b = 1`）、`[[array-of-tables]]`、
基本字符串（含转义）与字面字符串（`'...'`）、整数（`+/-`、下划线、
`0x/0o/0b`）、浮点（小数点 / 指数）、布尔、数组（可跨行、可尾逗号）、
内联表 `{ a = 1 }`。已知限制：不解析日期时间；浮点输出最多 6 位小数；
重复键后写覆盖（不报错）。

| 子任务 | 内容 | 状态 |
|---|---|---|
| P3-5a | 类型 + 访问器：`get(key)` / `get_path("a.b")` / `at(i)` / `len` / `is_table` / `is_array` / `as_int` / `as_float` / `as_bool` / `as_str` | ✅ |
| P3-5b | `toml_parse(str) -> Result<Toml, TomlError>`：注释 / 表头 / 点分键 / 字符串 / 数字 / 布尔 / 数组 / 内联表；非法值返回 `Err` | ✅ |
| P3-5c | `toml_stringify(&Toml) -> String`：先输出标量键，再按层输出 `[table]` 与 `[[array-of-tables]]`；字符串转义 | ✅ |
| P3-5d | 回归 `tests/cases/130_toml.zr`：多表 + 点分键 + 数组 + 内联表 + 数组表；生成后重新解析一致；语法错误返回 `Err` | ✅ |

**验收**：解析 → 生成 → 再解析得到同样的树；`a.b = 1` 等价于 `[a] b = 1`；
未闭合数组 / 缺值返回带位置的 `Err`。

---

## 阶段 4 — 标准库扩展与 AI 运行时（2026-11-02）

> 目标：把通用工具能力与 AI 开发所需的数据类型补进 `std/`，全部用 **zr 语言**
> 实现（不引第三方依赖）。每项都必须有正向回归用例并在 `-O0/-O1/-O2/-O3/-Oz`
> 五档通过；**不设"暂不做"状态**——列出的每一条都是本轮要落地的真实任务。
>
> 状态：⬜ 未开始 / 🔧 进行中 / ✅ 已提交。

| 子任务 | 内容 | 验收 | 状态 |
|---|---|---|---|
| P4-1 argparse | `std/std/argparse.zr` 重命名为 `std/std/argparse.zr`；保留索引读取（`arg_count` / `arg` / `arg_opt` / `args` / `get` / `program_name`），新增参数名读取：`value(name)` / `value_or(name, default)` / `flag(name)` / `has(name)` / `positionals()`，支持 `--name=value`、`--name value`、`-n value`；另给 `value_in` / `value_or_in` / `has_in` / `flag_in` / `positionals_in` 可注入参数表的版本便于单测 | `126` 更新、新增 `131` | ✅ |
| P4-2 字符串格式化 | `io::format(fmt, args, nargs) -> String`：把格式串与参数拼成**新的字符串**（不写 stdout）；支持 `{}` `{:?}` `{:#?}` `{:x}` `{:X}` `{:b}` `{:o}`、位置参数 `{0}`、`{{` / `}}`；`string::join_parts(parts, count, sep)` | 新增 `132` | ✅ |
| P4-3 simd | 在现有 `f32x4` / `i32x4` / `f64x2` / `i8x8` 上补逐 lane 运算与归约：`from_array` / `to_array` / `splat` / `add` / `sub` / `mul`（浮点另有 `div`）/ `neg` / `min` / `max`（浮点另有 `abs`）/ `sum` / `dot` | 新增 `139` | ✅ |
| P4-4 os | `std/std/os.zr`：`pid` / `cwd_len(buf, cap)` / `temp_dir` / `home_dir` / `os_name`（运行时探测 macos/linux/windows）/ `pointer_bits` / `exit` | 新增 `138` | ✅ |
| P4-5 posix | `std/std/posix.zr`：`isatty` / `getpid` / `kill` / `getcwd`；`SIG*` 与 `*_FILENO` 常量；`wifexited` / `wexitstatus` / `wifsignaled` / `wtermsig` 状态解码 | 新增 `138` | ✅ |
| P4-6 time | `std/std/time.zr`：`monotonic_ns` / `unix_ns` / `unix_secs` / `now_ms` / `sleep_ms`；`DateTime` + `civil_from_unix` / `unix_from_civil`（含负秒、闰年）/ `format_iso` / `parse_iso` | 新增 `136` | ✅ |
| P4-7 tz | `std/std/tz.zr`：`zone_exists` / `utc_offset_seconds(name, unix_secs)` / `to_local` / `from_local`；内置 UTC、Asia/Shanghai、Asia/Tokyo、Asia/Kolkata、Europe/London、Europe/Berlin、America/New_York、America/Los_Angeles、Australia/Sydney，并按真实 EU/US/AU DST 规则计算 | 新增 `137` | ✅ |
| P4-8 md5 | `std/md5.zr`：RFC 1321，`md5_hex(bytes)` / `md5_hex_str(s)` | 新增 `134` | ✅ |
| P4-9 sha256 | `std/sha256.zr`：FIPS 180-4，`sha256_hex(bytes)` / `sha256_hex_str(s)` | 新增 `135` | ✅ |
| P4-10 base64 | `std/base64.zr`：标准与 URL-safe 编解码，带 `=` padding，严格解码（非法字符 / 长度 / 缺 padding 返回 `None`） | 新增 `133` | ✅ |
| P4-11 http | `std/std/http.zr`：`url_parse`、`http_build_request`、`http_parse_response`（状态行 / 全部头 / Content-Length / chunked / 无长度）、`http_get`、`http_post`、`http_request(method, url, body, headers, follow)`（走运行时 `zr_*` 套接字，避免与 `net` 的同名类型冲突）；自动跟随 301/302/303/307/308（303 强制 GET、307/308 保留方法与 body、相对 Location 按原 URL 拼绝对地址）；仅点分 IPv4，不支持 HTTP/2 与代理；`https://` 可解析但明文模块返回 `TlsNotSupported`（TLS 在可选模块 `std/std/tls.zr`） | 新增 `140` | ✅ |
| P4-12 pdb | `std/alloc/pdb.zr`：PDB（Protein Data Bank）结构文件解析——HEADER / TITLE / 固定列宽 ATOM / HETATM，`Pdb::atom_count` / `atom` / `centroid`；坏行跳过 | 新增 `141` | ✅ |
| P4-13 tensor | `std/alloc/tensor.zr`：`Tensor` 值类型（`dims: [usize; 4]` + `ndim` + `len` + `data: *mut f64` + `grad` + `ctx`，行主序，最多 4 维）：`zeros` / `ones` / `full` / `scalar` / `from_slice` / `from_vec` / `from_matrix` / `identity`，`get_flat` / `set_flat` / `get2` / `set2` / `get3` / `set3`，逐元素 `add` / `sub` / `mul` / `div` / `neg`、标量 `add_scalar` / `mul_scalar`，`relu` / `leaky_relu`，`matmul` / `transpose` / `reshape`，`sum` / `mean` / `max` / `min` / `argmax`，`square` / `mse` / `same_shape` / `close_to` / `copy` / `fill` / `destroy`；浮点数学用 zr 自身实现（无外部依赖） | 新增 `142` / `143` | ✅ |
| P4-14 tensor 广播 | `Tensor::broadcast(target_dims, target_ndim)`（NumPy 规则：末维对齐、1 扩到更大维度）、`broadcast_add` / `broadcast_mul` | 新增 `146` | ✅ |
| P4-15 tensor conv2d | `Tensor::conv2d(weight, bias, stride_h, stride_w, pad_h, pad_w)`——NCHW 输入、`[OC, IC, KH, KW]` 权重、`[OC]` 偏置，直接卷积（无 im2col），输出 `[N, OC, OH, OW]` | 新增 `146` | ✅ |
| P4-16 tensor autograd | 极简反向模式自动微分（`TensorCtx` 计算图 + 拓扑后序）：`track_inplace` / `backward` / `zero_grad` / `sum_tensor`；前向 `add` / `mul` / `matmul` / `relu` 自动挂图，反向按链式法则累积 `grad` | 新增 `146` | ✅ |
| P4-17 tensor GPU 桩 | `gpu_available() -> bool`（当前恒 false）+ `GpuHandle` / `gpu_to()` 占位（`panic("not implemented")`），为将来 CUDA/Metal 后端预留接口 | 新增 `146` | ✅ |
| P4-18 tls | `std/std/tls.zr`：**纯 zr** TLS 1.2 客户端骨架（rustls 式分层 `ClientConfig` / `ClientConnection`，`handshake()` / `write()` / `read()` / `close()`；ClientHello 编码 + ServerHello 解析 + 记录层收发 + close_notify；支持 4 种密码套件（0x002F / 0x003C / 0xC02F / 0xC030）；**无外部依赖**（底层密码学由 `std/alloc/crypto.zr` 提供）；密钥交换（RSA/ECDH）和证书验证留 TODO | 无（Python TLS server 端到端验证通过） | ✅ |
| P4-21 crypto | `std/alloc/crypto.zr`：纯 zr 密码学原语——AES-128-ECB（keyexp + encrypt_block + decrypt_block）、AES-128-CTR 模式、SHA-256 流式（`Sha256State` + `init` / `update` / `digest`）、HMAC-SHA256、简易 PRNG（`PrngState` + `seed` / `bytes`）。三个 NIST AES-128 测试向量 + RFC 4231 HMAC-SHA256 向量全部通过 | 无（单文件自验证） | ✅ |
| P4-19 simd 扩展 | `std/core/simd.zr` 新增 `f32x8` / `f64x4` / `i64x2`（逐 lane `add`/`sub`/`mul`/`div`/`neg`/`min`/`max`/`abs`、归约 `sum`/`dot`）+ `best_width()`（64 位平台 32 字节、32 位 16 字节） | 新增 `144` | ✅ |
| P4-20 argparse 子命令 | `std/std/argparse.zr` 新增 `subcommand()` / `subcommand_in(argv)`（跳过选项取第一个位置参数）、`print_help(name, desc, cmds, descs, ncmd)` 生成 USAGE / SUBCOMMANDS 文本、`help_requested()` / `help_requested_in(argv)` 识别 `--help` / `-h` | 新增 `145` | ✅ |

---

## 阶段 5 — 已完成的缺口（核对后归档）

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

## 阶段 6 — 标准库三层重组（core ⊂ alloc ⊂ std）

对齐 Rust 的 crate 分层，把扁平 `std/*.zr` 按依赖方向落到目录里：

| 层 | 目录 | 模块 |
|---|---|---|
| **core**（零依赖，无堆/OS/I/O） | `std/core/` | `core` `simd` |
| **alloc**（只依赖 core，有堆分配） | `std/alloc/` | `mem` `slice` `string` `collections` `json` `xml` `toml` `base64` `md5` `sha256` `crypto` `tensor` `pdb` `async` |
| **std**（依赖 OS） | `std/std/` | `sys` `io` `fs` `net` `http` `tls` `env` `os` `posix` `process` `thread` `time` `tz` `path` `argparse` `error` |
| **门面** | `std/std.zr` | `use core::*; use string::*; …` 传递性再导出，业务代码可 `use std::*` / `use std::io::File` |

编译器配套（`src/modules.rs`）：
- `stdlib_module_path`：按 `std/<name>.zr` → `core/` → `alloc/` → `std/` 顺序查找，旧的扁平 `use io::File` 不受影响。
- `layer_of_path` + `Layer`：层级由目录推出；依赖方向 `core → core`、`alloc → {core, alloc}`、`std → 任意`，越界报 `layering violation`。
- `expand_glob_exports`：门面的 `use x::*` 做传递性再导出（多轮直到收敛，可处理环）。
- `load_with_root`：把根源文件登记进 `seen`，避免"编译标准库模块本身"时被 prelude 重复并入 → `multiple impls of the same trait`。
- 拆分：`core.zr` 只留 trait/Option/Result/panic/assert/size_of；分配器接口移到 `alloc/mem.zr`；`write/read/exit` 移到 `std/std/sys.zr`。
- 清理重名：删除 `net.zr` 里与 `http.zr` 冲突的旧 HTTP 客户端；`http` 复用 `net::SockAddrStorage`。

回归：`tests/cases/148_std_layers.zr`（`use std::*` 同时取三层名字，五档全绿）；`modules::layer_tests`（层级分类 + glob 传递性，2 个单测）；`cargo test --bin zr` 269 通过；`zr_language` / `zr_negative` 全绿。

---

## 阶段 7 — 按 Rust 的 core / alloc / std 补齐 API

| 层 | 新增 |
|---|---|
| **core** | `cmp.zr`（`Ordering` / `cmp` / `cmp_min` / `cmp_max`）、`char.zr`（ASCII 判定 / 大小写 / 数字转换，自由函数）、`num.zr`（`abs` / `sign` / `pow` / 饱和 / 检查 / 回绕 / 位计数 / `gcd` / `lcm` / `clamp` / `i64_min()` / `i64_max()`）；`core.zr` 的 `Option` / `Result` 补 `expect` / `unwrap_or_else` / `map_or` / `ok_or` / `filter` / `or` / `ok` / `err` / `map_err`，标量补 `Eq` / `Ord` / `Hash` |
| **alloc** | 新增 `boxed.zr`（`Box<T>`）、`vec_deque.zr`（`VecDeque<T>`）、`hashmap.zr`（`HashMap<K, V>`，开放寻址 + rehash）；`collections.zr` 的 `Vec` 补 `first` / `last` / `truncate` / `retain` / `reverse` / `swap` / `extend_from_slice` / `dedup`；`string.zr` 的 `String` 补 `trim*` / `to_lowercase` / `to_uppercase` / `find_str` / `rfind_byte` / `split` / `split_whitespace` / `replace` / `parse_i64` / `parse_i32` 与 `Eq` / `Ord` / `Hash` / `Clone` / `Default`；`mem.zr` 的原子 `Ordering` 改名 `AtomicOrdering` |
| **std** | `io.zr` 补 `stdout()` / `stderr()` / `stdin()`、`File::read_line` / `File::read_to_string` / `File::write_string`；`fs.zr` 补 `create_dir_all` / `read_to_string` / `read_bytes` / `write_string` / `append_string` / `copy_file` / `read_dir`；`time.zr` 补 `Duration` / `Instant` / `sleep` |

顺带修的编译器缺陷：
- `size_of` / `align_of` 对泛型具名类型未按实参实例化（`Entry<K,V>` 算成 1 字节）→ `InferCtxt::type_params` + `subst_ty`。
- `size_of` 的结构体 / 元组 / 枚举未按对齐补齐（29 vs 真实 32）→ 与 `mir/builder` 的布局同口径；定长数组改用 `hir::array_len_value`；`char` 改为 1 字节。
- `io.zr` 私有助手 `align_of` 遮蔽内建 `align_of::<T>()` → 改名 `io_align_of`。
- 文档记录 `&&` / `||` 不短路（`HashMap::probe` 改为显式 `if used` 守住求值）。

回归：`tests/cases/149_std_core_ext.zr`、`150_std_alloc_ext.zr`、`151_std_fs_time.zr`（五档全绿）；`cargo test --bin zr` / `zr_language` / `zr_negative` 全绿。

---

## 阶段 8 — HTTP/2 客户端（2026-11-03）

| 项 | 内容 |
|---|---|
| **新增模块** | `std/std/http2.zr`：RFC 7540 明文 h2c 客户端。连接前导 + SETTINGS 协商、9 字节帧编解码（DATA / HEADERS / RST_STREAM / SETTINGS / PING / GOAWAY / WINDOW_UPDATE / CONTINUATION）、HEADERS + CONTINUATION 头块拆帧、16 KiB DATA 分帧、响应侧流控（每个非末 DATA 帧回流 + 连接各一个 WINDOW_UPDATE） |
| **HPACK（RFC 7541）** | 静态表 61 项 + 动态表（4096 / 可配置，条目 = name + value + 32，LRU 淘汰）、三种字面量编码（增量索引 / 不索引 / 动态表容量更新）、7 位前缀变长整数、Huffman **解码**（附录 B 257 符号，规范 `count`/`first_code`/`first_index`/`syms` 表，逐位解码）；编码侧不做 Huffman |
| **公开 API** | `http2_request` / `http2_get` / `http2_post`（返回既有 `HttpResponse`），以及 `HpackDecoder`、`h2_encode_headers`、`h2_encode_frame` 纯函数（便于离线测试）；`std.zr` 门面新增 `use http2::*;` |
| **配套改动** | `HttpResponse` 新增 `strings: Vec<String>` 所有权池（HPACK 解码出的 `str` 指向池内堆缓冲，避免悬垂）；`http.zr` 的 `http_find_header_value` / `http_resolve_redirect` 改 `pub` 供 http2 复用重定向策略 |
| **顺带修的缺陷** | `net.zr::tcp_connect(host, port)` 早先忽略 `port`，把 host 直接交给 `parse_socket_addr`（要求 `host:port`），所以 `tcp_connect("127.0.0.1", 18081)` 一律 `InvalidAddr`；**聚合字面量移动不登记**导致 `TcpStream::connect` 返回前就 drop 掉 `Socket`、fd 被关（详见 `docs/rust-parity.md` 与 `AGENTS.md` 的缺陷表） |
| **限制** | 仅明文 `http://`（`https://` 返回 `TlsNotSupported`，`tls.zr` 没有 ALPN）；不支持服务端推送 / PRIORITY；请求体 ≤ 65535；不严格校验帧序 |
| **回归** | `tests/cases/152_http2.zr`（五档全绿）：RFC 7541 附录 C.4.1–C.4.3 / C.6.1–C.6.3 六组向量、编码器字面量格式、长值多字节长度前缀、9 字节帧头逐字节断言。真实 h2c 往返（GET / POST）用 `/tmp` 探针 + 手写 Python 服务端人工验证 |

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
