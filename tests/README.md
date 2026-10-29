# zr 语言测试用例

测试用例按《Rust 程序设计语言》（The Rust Book）的章节顺序组织，每个
`cases/*.zr` 是一个自检程序：

- 全部断言通过时 `main` 返回 `0`；
- 失败时返回一个**非 0 的编号**，直接指向失败的断言（见文件内的注释）。

宿主侧运行器是 `tests/zr_language.rs`，`cargo test` 会自动执行：

```sh
cargo test --test zr_language
```

之所以用宿主运行器而不是语言内的 `#[test]` + 断言宏，是因为 `#[test]`
属性、断言内建这些设施编译器目前还不支持（闭包已可用，见 `83_closure_fnptr.zr`
与 `84_closure_capture_value.zr`；见下方"尚未覆盖"）。

## 已覆盖

| 用例 | 对应 Rust Book 章节 |
|------|--------------------|
| `01_variables.zr` | 变量与可变性：`let` / `mut` / shadowing |
| `02_scalars.zr`   | 数据类型：整数标量与 `+ - * / %` |
| `03_precedence.zr`| 运算符：优先级、括号、结合性 |
| `04_comparisons.zr`| 运算符：`== != > >=` |
| `05_logic.zr`     | 运算符：布尔字面量、`!` `\|\|` |
| `06_functions.zr` | 函数：定义、参数、返回值、嵌套调用 |
| `07_control_flow.zr` | 控制流：`if` / `else`、分支早返回 |
| `08_three_args.zr`| 函数：三参数调用（rdi/rsi/rdx） |
| `09_while.zr`      | 控制流：`while` 循环（依赖 φ 节点合并回边） |
| `10_if_expression.zr` | 控制流：`if` 作为表达式取值（依赖 φ 节点合流） |
| `11_if_else_chain.zr`| 控制流：`else if` 链作为表达式取值 |
| `12_comptime.zr`     | 编译期求值：`comptime` 表达式与块形式 |
| `13_casts.zr`        | 类型转换 `as`：截断、符号扩展、零扩展 |
| `14_spill.zr`        | 寄存器溢出：24 个同时存活的值（仅 x86-64） |
| `15_loops.zr`        | 循环控制流：`loop`/`break`/`continue`、循环头重定义计数器 |
| `16_comptime_fn.zr`  | 编译期求值：`comptime` 中调用函数与常量 |
| `17_loop_break.zr`   | 循环出口的 SSA 构造（`break` 的多种形状） |
| `18_spill_ops.zr`    | 溢出值参与运算：左右操作数、比较、移位、除法 |
| `19_float.zr`        | 浮点 `f64`：算术 / 比较 / NaN 语义 / 取负 / `as` 转换 |
| `20_f32.zr`          | 浮点 `f32`：算术 / 比较 / 取负 / `f32`↔`f64` / `as` |
| `21_literal_suffixes.zr` | 字面量类型后缀（`1u8` / `7i8` / `2.5f32` …） |
| `53_narrow_float.zr`     | 窄浮点 `f8` / `f16`：算术 / 比较 / 跨函数 / 整数互转 / 结构体与数组 / 舍入 / Inf |
| `54_struct_pattern.zr`   | 结构体模式：字段绑定 / 嵌套模式 / `..` 尾 / guard / `if let` |
| `55_tuple_pattern.zr`    | 元组：字面量 / 字段访问 / 元组模式 / `(_, _)` 兜底 / `if let` |
| `56_range_pattern.zr`    | 范围模式：闭区间 / 开区间 / char 范围 / `if let` |
| `57_enum.zr`             | 枚举：变体构造 / 判别式 / 变体模式 / 穷尽性 / 拷贝语义 |
| `22_call_preserve.zr` | 跨调用存活的值必须被保住（callee-save / 形参并行搬运） |
| `23_strings.zr`      | 字符串字面量落 `.rodata`、去重、按地址比较、`str` 作实参跨调用 |
| `24_pointers.zr`     | 指针解引用 / 索引、取址、指针算术与字符串按字节索引 |
| `25_match.zr`        | `match` 字面量 / 绑定 / 或模式 / guard 的运行期语义 |
| `26_comptime_struct.zr` | 编译期结构体字面量、字段读取与改写 |
| `27_inline_asm.zr`   | 内联汇编（x86-64 整数子集）输入/输出操作数与编码 |
| `28_arrays.zr`       | 定长数组：字面量、`[v; N]`、索引、数组参数、逐元素运算 |
| `29_async.zr`        | 受限 async/await：状态机、挂起/恢复、`async fn main` |
| `30_drop.zr`         | 用户 `Drop`：作用域/返回/循环析构、移动与字段递归析构 |
| `31_fn_ptr.zr`       | 一等函数指针：取址、参数/返回值/字段/数组、运行时间接调用（x86-64） |
| `32_fn_ptr_async.zr` | async 状态机中的函数指针与间接调用 |
| `33_methods.zr`      | 具名结构体 `impl` 方法、关联函数、`&self`/`&mut self`/`self` |
| `34_tuple_struct.zr` | 元组结构体：构造、`.0`/`.1`、嵌套、方法、Drop |
| `35_struct_update.zr` | 结构体更新 `..base`：部分覆盖、字段 base、连续更新 |
| `36_drop_array.zr` | Drop 数组：索引顺序、元素赋值、结构体数组、字段数组、按值传参 |
| `37_struct_array.zr` | 结构体数组：元素读写、按值传参/返回、副本值语义 |
| `38_drop_temp.zr` | 未绑定 Drop 临时量：表达式语句立即析构 |
| `39_partial_move.zr` | Drop 部分移动：字段、嵌套路径、实参、数组元素、重新赋值 |
| `40_drop_temp_nested.zr` | 嵌套 Drop 临时量：字段访问、二元表达式、if、return、尾表达式 |
| `41_return_drop_array.zr` | Drop 数组按值返回：return、尾表达式、参数转发、调用方析构 |
| `42_temp_partial_move.zr` | 临时结构体部分移动：普通/嵌套字段、整个 Drop 字段、剩余字段析构 |
| `43_return_drop_struct.zr` | 同步 Drop 结构体按值返回：return、尾表达式、参数转发 |
| `44_for_range.zr` | `for` 范围循环：排他/包含、终点单次求值、continue/break、嵌套与 u8 |
| `45_for_array.zr` | `for` 数组遍历：标量/结构体/Drop 数组、continue/break、嵌套与源表达式单次求值 |
| `46_dynamic_index_move.zr` | 动态下标 `arr[i]` 的 Drop 部分移动：按元素标记、循环内移动、标量数组读取 |
| `47_async_return_aggregate.zr` | 异步按值返回 Drop 结构体/Drop 数组，调用方作用域析构 |
| `48_async_nested_aggregate.zr` | 异步返回数组、结构体、结构体数组及元素读取 |
| `49_dynamic_future_source.zr` | `-> impl Future` 动态来源：不同分支返回不同异步 future，动态 poll/drop |
| `50_dynamic_future_aggregate.zr` | 动态 future 返回聚合值：数组结果跨 poll 搬运 |
| `51_spawn.zr` | `spawn` 任务队列：await 前轮询、退出前清空、Ready 后 drop 帧 |
| `52_if_let.zr` | `if let` 解构：绑定 / 嵌套 / else 分支 |
| `58_macros.zr` | `macro_rules!`：宏展开与调用 |
| `59_threads.zr` | 线程：`spawn`/`join`、共享数据 |
| `60_process.zr` | 进程：`std::process` 创建与等待（宿主端对照） |
| `61_labeled_loops.zr` | 带标签循环：`outer: loop { break outer; }` 与 Rust `'outer` 风格 |
| `62_while_let.zr` | `while let`：解构 + 条件循环 |
| `63_let_else.zr` | `let ... else { .. }`：绑定 + 发散 else |
| `64_fs_path.zr` | 文件系统与路径：`std::fs` / `std::path`（宿主端对照） |
| `65_slices.zr` | 切片：`[]const T`、数组→切片强转、切片参数 |
| `66_generics.zr` | 泛型：泛型函数 / impl、`T: Trait` 边界、单态化 |
| `67_unsigned_ops.zr` | 无符号整数：运算、环绕、比较、`as` |
| `68_float_widening.zr` | 浮点拓宽：`f32`→`f64`、实参位置隐式拓宽 |
| `69_question.zr` | `?` 后缀：`Result` / `Option` 传播、错误类型一致 |
| `70_type_alias.zr` | `type` 别名：使用点展开、别名套别名、前向引用 |
| `71_derive.zr` | `#[derive]`：`PartialEq`/`Eq`/`Hash`/`PartialOrd`/`Ord`/`Default`/`Debug`/`Clone`/`Copy` |
| `72_statics.zr` | `static` / `static mut`：标量初值、读/写/复合赋值/`&STATIC` |
| `73_closures.zr` | 闭包：无捕获立即调用、无捕获存进变量 |
| `74_generic_alias.zr` | `type X<T>` 泛型别名：保留参数、按实参替换 |
| `75_use_alias.zr` | `use x as y` 重命名：单段与多段路径 |
| `76_ssa_join_phi.zr` | SSA 合流 φ：Cooper–Harvey–Kennedy 支配边界 |
| `77_many_args.zr` | 多实参调用：SysV 栈上传参（第 7 个整数 / 第 9 个浮点） |
| `78_derive_methods.zr` | derive 方法体：`==`/`<`/`hash`/`default`/`clone` 真实生成 |
| `79_zero_const_high_reg.zr` | 零常量清 REX.R：r8–r15 不编码成 `xor rax, r8` |
| `80_match_value.zr` | `match` 返回值：穷尽 match 不被 `()` 覆盖 |
| `81_float_param_forward.zr` | 浮点形参转发：`SsaFunction::param_types` 播种 |
| `82_turbofish.zr` | turbofish `::<...>`：调用位置与类型路径 |
| `83_closure_fnptr.zr` | 无捕获闭包提升：存进变量 / 当实参 / 返回（`__closure_N`） |
| `84_closure_capture_value.zr` | 带捕获闭包：存进变量 / 嵌套 / 多次调用 |
| `85_unsafe.zr` | `unsafe` 块：可解析和通过类型检查 |
| `86_bounds_check.zr` | 常量越界：编译期下标越界报 `index out of bounds`；浮点 Display 回归 |
| `87_move_check.zr` | 最小移动检查：`let b = a` 后读 `a` 报 use-after-move |
| `88_dyn_keyword.zr` | `dyn Trait`：词法+解析+类型检查骨架 |
| `89_prelude_variant.zr` | prelude 变体：`Option`/`Result`/`Some`/`None`/`Ok`/`Err` 无需显式 `use` |
| `90_glob_import.zr` | `use foo::*` glob：`use core::*` / `use io::*` / 用户模块 |
| `91_multi_bound_generics.zr` | 多边界泛型 `T: Clone + Ord`：`P` 同时 impl 两个边界、两处都求解、形参按值移动 |
| `92_cfg_filter.zr` | `#[cfg(...)]` 过滤：互斥 host 键（linux/macos/windows）保证正好一个保留、恒假键被滤掉、运行期只调保留的那个 |

## 尚未覆盖（编译器还不支持）

Rust Book 的其余章节目前一个都测不了，因为对应特性尚未实现。列在这里
是为了让"测试通过"这个说法有明确边界，而不是暗示语言已经可用。

> 注：这张表在 2026-10-29 之前严重过期——它声称 `enum` / `match` / `泛型` /
> `闭包` / `智能指针` 都未实现，实际上 `57_enum.zr` / `25_match.zr` /
> `66_generics.zr` / `83_closure_fnptr.zr` / `75_use_alias.zr` /
> `90_glob_import.zr` 都在跑。以下按当前源码与用例重新核对。

| Rust Book 章节 | 现状 |
|----------------|------|
| 所有权 / 借用 / 引用 / 切片 | 🟡 部分：最小移动检查已实现（`87_move_check.zr`：`let b = a` 后读 `a` 报 use-after-move；字段级/按值传参移动暂不覆盖）；**借用检查仍是空壳**（`infer.rs::check_borrows` 只记录不检查）；切片已覆盖（`65_slices.zr`） |
| 枚举 | ✅ `57_enum.zr`：变体构造 / 判别式 / 变体模式 / 穷尽性 / 拷贝语义；`80_match_value.zr` 钉 match 返回值 |
| 浮点（仅 ARM32 / Thumb / RISC-V / wasm） | x86-64 与 aarch64 已支持；其余后端遇到浮点字面量会**明确报错**（不再静默发射 0） |
| 字符串字面量（仅 ARM32 / Thumb / RISC-V / wasm） | x86-64 与 aarch64 已落 `.rodata`（可取址、按地址比较），并可通过 `extern "C"` 调用运行时的 `zr_write` 输出；其余后端遇到字符串字面量会**明确报错**。尚无解引用 / 索引（需要 SSA 内存模型） |
| 模式匹配（`match` / `if let` / 解构） | ✅ `25_match.zr`（字面量/绑定/或/guard）、`52_if_let.zr`、`54_struct_pattern.zr`、`55_tuple_pattern.zr`、`56_range_pattern.zr`、`62_while_let.zr`；`match` 脱糖成 if/else 链。**缺臂报错已改进**（枚举列出缺失变体名） |
| 包管理与 Cargo | 🟡 模块系统已支持 `mod` / `use` / `use a::{b,c}` / `use x as y`（`75_use_alias.zr`）/ `use foo::*`（`90_glob_import.zr`）；但无 Cargo 等价物 |
| 集合（Vector / String / HashMap） | 🟡 `std/collections` 与 `std/string` 已加载，但 `String` 无 `BufRead::read_line` 形态的 API；无 `HashMap` |
| 错误处理（`panic!` / `Result` / `?`） | ✅ `69_question.zr`：`?` 对 `Result`/`Option` 生效；`Option`/`Result`（含变体）已在 prelude（`89_prelude_variant.zr`）。`panic!` 未实现 |
| 泛型 / Trait / 生命周期 | ✅ 泛型函数/impl 与 `T: Trait` 边界校验已实现（`66_generics.zr`、`74_generic_alias.zr`）；trait 方法/关联类型/`Self` 可用。**生命周期标注 `'a` 解析后丢弃**，无借用/存活检查 |
| 函数式（闭包 / 迭代器） | 🟡 **无捕获**闭包已可存进变量/当实参/返回（`83_closure_fnptr.zr`）；**带捕获**闭包已可存进变量/嵌套/多次调用（`84_closure_capture_value.zr`），但跨函数传 env 仍有限制 |
| 智能指针（`Box` / `Rc` / `RefCell`） | ❌ 未实现 |
| 并发（线程 / 通道 / `Mutex` / `Send`） | 🟡 线程已覆盖（`59_threads.zr`）；无通道 / `Mutex` / `Send` |
| 面向对象 / 状态模式 | ✅ `trait` 与 `enum` 都已实现；trait 对象 `dyn` 骨架已搭（`88_dyn_keyword.zr`），vtable 分派未完工 |
| `async` / 协程 | ✅ `29_async.zr` / `47..51` 覆盖 `.await` / future / `spawn`；`yield` 只是类型占位 |

| 32 位目标上的宽整数（`i64` / `u64` / `i128`） | 32 位寄存器放不下，多字长（寄存器对）尚未实现；**写出来会在类型检查阶段报错** |
| Thumb-2 的 spilling | 超过 8 个活跃寄存器会（正确地）报 `unsupported_pressure`；栈上传参（≥5 个）同理 |
| 高级特性（`unsafe` / 关联类型 / 宏） | 🟡 `unsafe` 已实现（`85_unsafe.zr`：`unsafe { ... }` 块可解析和通过类型检查）；关联类型 `type Output;` + `type Output = i32;` 已实现；宏走 `macro_rules!`（`58_macros.zr`）；proc-macro 未接线 |

## 循环控制流

`loop` + `break`、`continue`、`while true/false`、循环头重定义计数器
（`i = i + 1` 写在循环头里）、递归，现在在 O0/O1/O2/O3/Oz 下都正确
（`tests/cases/15_loops.zr`）。

这一块之前是"能编译但结果不对"，修掉的几个根因都值得记一笔：

* `break` / `continue` 在 MIR 降级里落进 `_ => Unit` 兜底分支，等价于空操作
  ——`loop { .. break; }` 直接**转不出来**（编译成功，运行挂死）。
* `continue` 在类型检查里被映射成了 `break`，跳出口而不是跳回边。
* 循环头缺 φ：`i = i + 1` 写在循环头里时，`i` 的定义点只有循环头自己 + 入口，
  只统计前驱会漏掉，于是计数器停在入口值。
* 内层循环的累加器 `s` 只在循环体里定义，但跨着整个内层循环活着——合流点
  需要 φ 的判据不能只看"前驱里有几处定义"，得看活跃性。
* φ 的 incoming 在寄存器分配里按**定义所在的前驱块末尾**延长，而不是 φ 自己
  的位置。回边上后者在定义之前，算出来的活跃区间会被 `iv.insert` 覆盖，循环
  体里刚算出的值立刻就"死"了、寄存器被下一条指令复用。

**仍然不对：嵌套循环在 O2 及以上仍然算错**（O0/O1 正确）。SSA 层面是对的，
差异只来自优化器把常量替换成 `copy` 之后那几条 `mov`，怀疑是 φ 并行搬运与
常量重物化（rematerialization）的交互。没定位清楚之前没有把它塞进用例里。

另外：`--dump-*` 在增量缓存命中时会跳过（直接复用产物），想看中间表示记得先
`zr clean`。

## Thumb-2 后端

`thumbv7em-none-eabi` 走 `src/codegen/encoder_thumb.rs`，与 `arm-none-eabi`
（A32）是两套独立后端。三元组首段以 `thumb` 开头即按 Thumb-2 处理。

指令编码**逐条对照 clang 的汇编器**确定，测试里写的是 clang 的实际输出，
不是照着手册猜的位模式（已经踩过两次：`.w` 变体的 hw2 高半字节、
`mls` 的 [7:4] 常量位）。仓库里没有 ARM 模拟器，所以验证手段是：

* `src/codegen/encoder_thumb.rs` 里的单测断言半字编码；
* `objdump -d --triple=thumbv7em-none-eabi` 反汇编，检查序言、调用、返回、
  分支重定位回填是否落在正确的半字上。

已经过反汇编确认的关键约定：

| 位置 | 约定 |
| --- | --- |
| 32 位指令顺序 | `w32` 先写 hw1（`0xF` 组），再写 hw2（`0xD` 组）；重定位回填必须同序 |
| `bl` / `b` (T1/T4) | `hw1 = 0xF000\|S\|imm10`，`hw2 = 0xD000\|J1\|J2\|imm11`；`bl` 的 hw2 bit12 = 1，`b` 为 0 |
| `cbnz` / `cbz` | 只支持 r0-r3；6 位有符号半字偏移在 [9:3]，Rn 在 [2:0] |
| `push` / `pop` | 寄存器掩码里 LR 和 PC 都落在 bit 8 |
| 返回路径 | `pop {r4-r7, pc}`，弹回 LR 会让函数从序言继续往下跑 |
| 符号 / 入口地址 | 函数符号值和 `e_entry` 要置 bit 0，标明 Thumb 状态 |
| `word(unit, w)` | 传整个 32 位指令字、由它拆半字：**高半字先写**（与 `w32` 的"先写 hw1"不同，VFP 全走它） |
| 32 位 `ldr`/`str` 的 Rt | 在 hw2 的 **[15:12]**（含 `sp` 相对），不是 [11:8] |
| `movw`/`movt` 绝对取址 | `movw rd,#0` + `movt rd,#0` 两条，各挂一条重定位；自链接映像里就地回填（ELF 47/48） |
| VFP | `vadd.f64 d0,d0,d1 = ee30 0b01`、`vldr d0,[sp,#8] = ed9d 0b02`、`vcvt.f64.s32 = eeb8 0bc0` |

后面补齐的功能（与 A32 同级）：寄存器溢出、指针/内存访问（`ldr`/`str` 各变体、
`s[i]`、指针算术）、字符串字面量经 `movw`/`movt` 取址落 `.rodata`、i64
（加减乘、比较、转换、2 的幂常量除余）、VFP 浮点。26 个语言用例在
`-O0`/`-O1`/`-O2`/`-O3`/`-Oz` 下全部编译通过。

本机没有 `qemu-arm`，所以 Thumb 产物只做编码与对象格式验证，不做运行期验证。

## AArch64 后端

`aarch64-unknown-linux-gnu` / `aarch64-apple-darwin` 等走
`src/codegen/encoder_aarch64.rs`。整数寄存器用线性扫描的活跃区间分配（与
x86-64 同源，不再是取模），池子含 caller-save 的 `x0..x17` 与 callee-save
的 `x19..x28`；跨调用存活的值会被分到 callee-save，序言保存、每个返回点
恢复。浮点值不占整数寄存器——每个浮点 SSA 值分配一个专属栈槽，运算时用
`d0`/`d1` 当暂存器读写内存，槽区在序言里 `sub sp, sp, #frame` 一次性预留。

浮点覆盖 `f64` 与 `f32`：算术（`fadd` / `fsub` / `fmul` / `fdiv`）、取负
（`fneg`）、比较（`fcmp` + `cset`）、`as` 转换（`scvtf` / `ucvtf` / `fcvtzs` /
`fcvtzu` / `fcvt`）。比较条件码按 IEEE754 取 `mi` / `ls` / `gt` / `ge` /
`eq` / `ne`，NaN 语义与 x86-64 一致。

验证分两层：

* 编码单测逐条对照 clang 的 `--target=aarch64-unknown-linux-gnu` 汇编输出；
* **运行时**用 Unicorn（`pip3 install unicorn`）加载 zr 生成的 aarch64 静态
  ELF 直接执行，`svc #0`（`write` / `exit`）由钩子处理。`tests/cases/*.zr`
  在 `-O0..-Oz` 全部通过（`14_spill` / `17_loop_break` 因未实现溢出而按设计
  报错）。

运行时验证落地时抓出了一批"能编译、反汇编看着也像、结果却是错的"编码错误，
都已修掉：

| 位置 | 错误 | 后果 |
| --- | --- | --- |
| `b.cond` 偏移量 | 写成 `imm19=1`（应为 `2`） | 条件跳转恒落到 `then`，`if` / `while` 全错 |
| 分支与 `!x` 的零判定 | `cmp x, x`（恒相等） | 条件为假也走 `then`；`!x` 恒为 `1` |
| `fneg` 基址 | `0x1e204000`（应为 `0x1e214000`） | `fneg d` 编成非 `fneg` 的指令 |
| 左移 / 右移基址 | `0x9ac22000` / `0x9ac22420` | 移位结果错 |
| `as` 转换 | 源寄存器放错字段、基址缺失 | SBFM/UBFM 编成非法指令 |
| 寄存器分配 | 取模而非线性扫描 | 编号同余的活跃值互相覆盖 |
| call-clobber | 只用 caller-save（`x0..x17`） | 跨调用存活的值被被调方覆盖 |
| 浮点常量 / int→float | 借 `x0` 当中转 | 踩掉同时活跃的整数值 |

## 字符串字面量与 `.rodata`

字符串字面量按内容去重后放进 `.rodata`（NUL 结尾），取字面量即取其地址，因此
`==` / `!=` 实际比较指针：同内容共享同一地址，不同内容地址不同。为此新增了
一条取址指令与两种重定位（见 `src/codegen/{encoder,encoder_aarch64}.rs`）：

| 目标 | 取址指令 | ELF | Mach-O | COFF |
| --- | --- | --- | --- | --- |
| x86-64 | `lea reg,[rip+disp32]` | `R_X86_64_PC32`(2) | `SIGNED`(1) | `REL32`(0x0004) |
| AArch64 | `adrp` + `add :lo12:` | `ADR_PREL_PG_HI21`(275) + `ADD_ABS_LO12_NC`(277) | `PAGE21`(3) + `PAGEOFFSET12`(4) | `PAGEBASE_REL21`(0x0004) + `PAGEOFFSET_12A`(0x0006) |

自构造可执行文件走 `apply_relocations` 就地回填；`lea` 的附加数是 `-4`（PC32
以字段末尾为基准，`S + A - P` 里的 `A = -4` 正好抵消）。符号归属 `.rodata`：
ELF 段索引 2、Mach-O `n_sect = 2`、COFF 节号 2。

运行时验证（`23_strings.zr`）又抓出两个"地址算错一半"的类型错误：

| 位置 | 错误 | 后果 |
| --- | --- | --- |
| x86-64 `lea` 的 REX 位 | 目的寄存器在 ModRM 的 `reg` 字段，却用了 REX.B（`0x01`）而非 REX.R（`0x04`） | `r12`/`r13` 的低 3 位被解码成 `rsp`/`rbp`，直接写坏栈指针与帧指针（`-O2`/`-Oz` 下必现段错误） |
| Mach-O 重定位类型 | 把取址当成分支，统一发 `BRANCH`(2)，且 `r_pcrel` 恒为 1 | x86-64 侥幸能跑但不合规；AArch64 的 `adrp`/`add` 会被当成 `BRANCH26`，且 `PAGEOFFSET12` 误标为 PC 相对 |

Mach-O 的验证手段：`-c --target <triple>` 出对象后 `otool -rv` 与 clang 的
对应输出逐字段比对（`type` 与 `pcrel` 两列）。

## `extern "C"` 与运行时

`extern "C" { fn ...; }` 里的函数声明会被并入函数表（`body` 为 `None`），因此
名称解析与类型检查把它当成普通函数；代码生成不给它产代码，只在调用点发一条指向
该符号的重定位（x86-64 `call` → `R_X86_64_PLT32`，AArch64 `bl` → `R_AARCH64_CALL26`）。
链接阶段 `add_extern_symbols` 把它登记成**未定义全局符号**，交给平台链接器绑定到
`libzrrt`（或系统 C 库）。

宿主上 `zr_write(fd, buf, count)`、`zr_read`、`zr_exit` 等由 `runtime/rt_*.s`
提供，签名按 C ABI（实参进 ABI 整数寄存器，`str` 就是一个指针字）。字符串字面量
本身是 `.rodata` 地址，把它的首地址与显式长度传给 `zr_write` 即可输出——不需要
胖指针或切片。

验证：
* 对象层：`-c` 出的目标文件里外部函数是 `*UND*` 全局符号，且带正确的调用重定位；
* 运行期：`tests/zr_language.rs::extern_c_write_prints_string_literal` 在
  `-O0..-Oz` 编译运行，断言标准输出是期望的字符串、退出码为 0。

`mod foo;` 文件模块里声明的外部函数会被并进全局函数表，按简单名字即可调用
（和标准库声明运行时函数的做法一致）。

自行构造的 ELF 路径（交叉、无平台链接器）不带运行时，外部符号会保持未解析，
因此上面这个用例只测宿主目标，不放进 `tests/cases/`。

## 指针、取址与字符串按字节索引

最小 SSA 内存模型，覆盖本语言目前能表达的指针操作：

* `&x` 取局部变量地址（`x` 的类型不能是浮点——见下）；
* `*p` 解引用读、`*p = v` 写；
* `p[i]` / `p[i] = v`：按**元素字节大小**缩放后等价于 `*(p + i*size)`；
* `s[i]`：字符串按字节索引，结果为 `u8`；
* 指针算术 `p + n` / `n + p` / `p - n`：同样按元素大小缩放，结果还是指针；
  `p - q`：结果是元素个数（`isize`，`(p - q) / size`），两个指针相加报错；
* **可变性**：`&x` / `*const T` / `&T` 是只读的——`*p = v` / `p[i] = v` 报编译
  错误（"`cannot assign through an immutable pointer`"），要写用 `&mut x` +
  `*mut T`。`*T` 单独写在类型里默认就是 `*const T`。`s[i] = v`（对字符串赋值）
  同样拒绝：字面量在 `.rodata` 只读段，不拦就是段错误；
* 索引 / 解引用在**指针**与**引用**上都成立，两者运行期都是同一个"地址"。

数据通路新增三条 IR 指令（MIR 与 SSA 同名）：

| 指令 | 含义 |
| --- | --- |
| `Load { result, ptr, ty }` | `result = *ptr`，`ty` 决定位宽与有无符号 |
| `Store { ptr, value, ty }` | `*ptr = value`，**有副作用**（DCE 不删、CSE 不并、LICM 不提） |
| `SlotAddr { result, local }` | `result = &slot{local}`：被取地址的局部变量的栈槽地址 |

**被取地址的局部变量改住栈槽**：只要一个局部量上出现过 `&x`，它就不能再是
寄存器里的 SSA 值（地址是它的身份）。降级时把它固定到一块栈槽，读写变成显式的
`Load`/`Store`，`&x` 得到槽的地址。槽区与浮点槽同一帧：x86-64 排在"整数溢出槽 +
浮点槽"之后，AArch64 排在浮点槽之后，寻址都还是 `[rsp/sp + 8*slot]`。

加载 / 存储按 `NumTy` 的位宽与符号选择指令：

| 位宽 | x86-64 读（无符号 / 有符号） | x86-64 写 | AArch64 读 | AArch64 写 |
| --- | --- | --- | --- | --- |
| 1 | `movzx` / `movsx` byte | `mov byte` | `ldrb w` / `ldrsb x` | `strb w` |
| 2 | `movzx` / `movsx` word | `mov word` | `ldrh w` / `ldrsh x` | `strh w` |
| 4 | `mov r32`（零扩展）/ `movsxd` | `mov dword` | `ldr w` / `ldrsw x` | `str w` |
| 8 | `mov r64` | `mov qword` | `ldr x` | `str x` |

x86-64 的写路径先保证地址在一个稳定寄存器里（溢出值或 `rax` 经 `rcx` 中转，
`rcx` 不参与分配），再把值的 64 位表示搬进 `rax`——小端下低字节就是要写的值，
不必纠缠 8 位寄存器命名，最后写 `rax` 的窄子寄存器即可。

**浮点通过指针访问一律报编译错误**（`&f` 与 `*pf` 都拒绝）：浮点值住专属栈槽、
只经 XMM/v 寄存器读写，而指针走 GPR 与整数 load/store，两者交叉只会安静地产生
错值。宁可编不过。

运行期验证由 `24_pointers.zr` 覆盖：字符串按字节索引、取址、解引用读写、指针
索引、地址被取的局部量与循环/φ 的交互、`i8` 的符号扩展。宿主与 AArch64
（Unicorn）在 `-O0..-Oz` 全部通过。

### 顺带修掉的解析 bug

写 `*p = v` 的用例时抓出一个解析错误：块状表达式会被当成二元运算的**左侧**。

```zr
if *p != 7 { return 1; }
*p = 42;              // 被解析成 `(if *p != 7 { return 1; }) * (p = 42)`
```

于是 `*p = 42` 那条赋值"消失"，类型检查报 `expression is not assignable`。修法是
在 Pratt 解析的主循环里，遇到块状表达式（`if`/`match`/`while`/`for`/`loop`/`{ }`）
就停止继续绑定运算符——Rust 解析器同样禁止块表达式出现在二元运算左侧。

## 运行期 match

`match` 早先只做类型检查：**运行期是空转**——`classify(1)` 会静默算错而不是
报错。现在 `match` 脱糖成 if/else 链，语义由 `25_match.zr` 锁住：

* 支持的模式：字面量（整数 / bool / char，**含负数** `-5`）、`_`、绑定臂
  `v`（一般与 guard 连用）、或模式 `3 | 4`、guard `3 if n > 2`、结构体模式
  `P { x: 1, y: v }`（含 `..` 尾与嵌套，`54_struct_pattern.zr`）、元组模式
  `(1, v)`（`55_tuple_pattern.zr`）、范围模式 `1..=10` / `1..10`（
  `56_range_pattern.zr`）、枚举变体模式 `Shape::Circle(r)`（`57_enum.zr`）。
* **穷尽性在编译期强制**：全覆盖的 match（枚举所有变体 / bool 双字面量）
  可省 `_`（与 Rust 一致）；否则需要一条不带 guard 的 `_` / 绑定臂兜底。
  缺臂报错现在区分场景：枚举列出**缺失变体名**（`match is not exhaustive:
  `Color` variant not covered: Blue`），整型/标量说需要尾 `_` 臂。不强制的话
  "没匹配上"的路径会落到 Unit 继续跑。
* **首匹配胜出**：脱糖是顺序 if/else 链，前面臂命中失败。
* 绑定臂的实现：guard / 臂体里对 `v` 的引用**内联成 scrutinee 表达式**
  （`substitute_local`），因为臂绑定没有独立存储。
* 限定（已知）：scrutinee 每臂重新求值——含副作用的 scrutinee 会重复执行；
  `comptime` 阶段的 `match` / 结构体字面量仍待做。

### 或模式曾是"只匹配第一个"

解析器早先把 `3 | 4` 的第二个分支**丢弃**（"目前或模式暂不展开为独立节点，
第一个分支作为主模式"）。于是：

```zr
match n {
    3 | 4 => 30,   // 只有 3 匹配；4 静默掉进别的臂或报尾 `_`
    _ => 40,
}
```

现在收集为 `parser::ast::Pattern::Or`（单分支不再包一层），类型检查侧
`push_or_alts` 把 `Or` 展开成几条独立的分支条件，共享同一 guard 与臂体。

### 类型检查穷尽性不是摆设

"尾 `_` 臂"这个检查本来就有，但它存在与否曾经无关紧要——因为运行期是空的。
两个改动发生在同一会话：先强制这个规则（枚举变体全覆盖 / bool 双字面量可
省 `_`，其余需尾 `_` 臂），再接通运行期。规则先行的原因：缺 else 的链会把
"没匹配上"变成"取到错误值跑下去"，比报错糟糕得多。缺臂报错在 2026-10-29
又改进过：枚举现在列出缺失的变体名（`17_match_missing_variant.zr`），不再
笼统说"补一个 `_` 臂"。

### 顺带修掉的常量编码 bug

接通 match 路径后暴露出：**bool / Unit 常量在 R8–R15 上编码错误**。
`emit_const` 把寄存器传给 `emit_const_int` 时用的是 3 位 ModRM 编码
（`Gpr::bits()`：`Rsi`→6、`Rdi`→7、`R8`→0），而 `emit_const_int` 里
`dst >= 8` 才加 REX.B 的判断把这套 3 位编码当成了寄存器**编号**——
两者口径不一致，`R8` 被编成 `Rax`。int 常量因为在 `emit_const` 里自带正确
的编码而没踩到。修统一走了 `Gpr`（rex_bit + bits），x86-64 `bool` 常量
落 R8+ 的程序从"布尔判断恒假"恢复为正确。

## ELF 输出上的两个坑

两个都属于"长度照样对、只有读出来才发现错"的类型，已在 `linker.rs` 的
单测里逐字段钉死：

* **`Elf32_Sym` 和 `Elf64_Sym` 字段顺序不一样**。32 位是
  `st_name, st_value, st_size, st_info, st_other, st_shndx`，64 位是
  `st_name, st_info, st_other, st_shndx, st_value, st_size`。按 64 位顺序
  写 32 位，条目长度一样是 16 字节，所有 32 位目标（arm / thumb /
  riscv32）的符号表会整体错位：名字被读成 `st_size`，`st_shndx` 变 0，
  于是符号全成 `*UND*`。
* **增量缓存不能记用户 `-o` 的路径当产物**。产物被删掉或覆盖之后，缓存
  仍会"命中"，把空文件拷回去并且报成功。现在缓存把产物拷进
  `target/zr-cache/` 自留一份，且命中前校验非空；空产物一律退回全量编译。

## 宽整数在 32 位目标上：为什么曾是报错而不是"能编过"

> **现状**：多字长运算已在 32 位原生目标（ARM32 / Thumb-2 / RISC-V32）落地
> （寄存器分配区分 32/64 位溢出单元、调用 ABI 把形参排成寄存器对），下面的
> 报错已不复存在；此节保留作为当初的设计取舍记录。

SSA 不带类型信息，后端分不清 `i64` 和 `i32`——它只看到 `SsaConst::Int(i128)`。
于是 32 位目标上宽整数会**静默截断**：

```zr
fn main() -> i64 {
    let a: i64 = 4294967296;   // 2^32
    let b: i64 = 1;
    a + b
}
```

`--target thumbv7em-none-eabi` 之前编出来是 `movw r4, #0`，高位直接丢掉，
`a + b` 得到 1 而不是 4294967297。截断能编过，但结果是错的——这比编不过
危险得多，所以在 `ast_to_hir_type` 这一层按目标指针宽度直接挡掉：

```
zr: 64 位整数在当前目标上尚未实现：该目标只有 32 位寄存器，编译器暂时还不能把
    宽整数拆成寄存器对。现在生成的是静默截断的代码，所以直接报错。
```

64 位目标不受影响。真正实现多字长运算需要先给 SSA 补类型标注，再改寄存器
分配（成对的活跃区间、成对的溢出/重载），最后改所有原生后端的调用 ABI。

## 多文件项目与增量编译

`mod foo;` 会去 `foo.zr`（或 `mod/foo.zr`）加载，并递归展开内联模块与
`use` 到的标准库模块。

增量编译**默认开启**（`--no-incremental` 关闭，`zr clean` 清空缓存）。
缓存放在 `target/zr-cache/`，键是源文件路径，值是：

- 所有参与编译的文件及其内容指纹；
- 上次的产物路径。

命中检查只**重新读取并哈希**这些文件，不解析任何源码，所以词法、语法、
类型检查、MIR、代码生成、链接全部跳过。指纹里混入了编译器版本、目标三元组、
优化等级、调试信息开关，所以换任何一个选项都会正确地失效。

实测（20 个模块、1325 行）：

| | 耗时 |
|---|---|
| 冷编译 | 29.3 ms |
| 热命中 | 8.5 ms（3.5×）|

### 为什么不是"按模块分别重新编译"

模块系统是**扁平命名空间**：模块条目被直接并进同一个 `Program`，共享一张
符号表。于是任何模块的改动都可能影响其它模块的名字解析，没有办法安全地
跳过它们的类型检查。实测各阶段耗时（`ZR_TIME=1`）：

```
lex     ≈ 0.4 ms
parse   ≈ 0.9 ms
typeck  ≈ 2.2 ms
mir     ≈ 2.3 ms
```

瓶颈在类型检查，所以缓存词法+语法最多只能省掉前端约 25%。要做到真正的
按模块增量，得先把模块系统改成两层符号表，那是另一个量级的改动。

## 工具链

除编译器本身外，`zr` 还带了四个子命令：

### `zr fmt`

见上文"编写用例的注意事项"之前的章节。走词法而非 AST，保留全部注释，幂等，
并校验格式化前后记号流不变。

### `zr pkg`（包管理器）

```
zr pkg init                 新建 zr.toml
zr pkg add <name> <path>    加路径依赖
zr pkg remove <name>
zr pkg list / tree
zr pkg build                按拓扑序构建全部依赖
zr pkg clean
```

清单是 TOML 的一个子集（`name` / `version` / `edition` / `[dependencies]`），
自己解析：不想为了一份清单引入依赖，也不想清单语法随 TOML 规范漂移。
**写错会报错**（未知段、未知键、值没加引号），不会静默生效。

嵌套依赖按各自清单所在目录解析（不是一律按最外层解析），循环依赖会明确报错。

### `zr lsp`（语言服务器）

stdio 上的 JSON-RPC 2.0，实现：

| 请求 | 说明 |
|------|------|
| `initialize` / `shutdown` / `exit` | 生命周期 |
| `textDocument/didOpen` / `didChange` / `didClose` | 诊断 |
| `textDocument/documentSymbol` | 文档符号（函数、类型、常量、形参） |
| `textDocument/definition` | 跳转到定义（含形参） |
| `textDocument/hover` | 悬停信息 |
| `textDocument/formatting` | 格式化（复用 `zr fmt`） |

诊断**只到语法级别**：类型检查需要完整模块环境，在单文件上跑会把 `mod`/`use`
引不到的符号全报一遍，满屏假错误反而没法用。语义诊断等模块系统到位后再接。

符号从**记号流**扫出（AST 目前不留标识符名字 span），顺带能容忍尚未支持的语法。

### `zr clean`

清空增量编译缓存（`target/zr-cache/`）。

## 属性与编译期求值

已经接上语义的部分：

- `comptime <表达式>` / `comptime { .. }`：编译期折成字面量，支持整数与
  浮点算术、位运算、移位、比较和逻辑运算。常量除以 0 在**编译期**报错。
- `#[inline]` / `#[inline(never)]`：`never` 的函数不会被自动内联（`-O3`）。
- `#[test]`：编译时合成一个跑所有测试函数的 `main`，返回失败的个数。
  测试函数不带参数、返回 `()` 算通过；返回 `i32` 时非 0 表示失败。
- `#[repr(C | packed | transparent | simd)]` / `#[repr(align = N)]`：
  写错会报错（`#[repr(bogus)]`、`#[repr(size = 4)]`），不再默默回退到默认布局。

仍然只是解析、没有任何语义的部分：`#[derive(..)]`、`#[cfg(..)]`、
`#[no_mangle]` 等。

## 优化等级

`cargo test --test zr_language` 会把每个用例在 `-O0 / -O1 / -O2 / -O3 / -Oz`
下各编译并运行一遍。全部通过才算绿：

| 等级 | 启用的 pass |
|------|-------------|
| `-O0` | 不优化 |
| `-O1` | 常量折叠、死代码消除 |
| `-O2` | 局部公共子表达式消除、全局值编号、窥孔优化 |
| `-O3` | 加上函数内联、循环不变量外提 |
| `-Oz` | `-O3` + 额外的死代码清理 |

宿主可执行文件走 x86-64 后端，其余目标只做编译（`-c`）检查，不运行。跨目标
的编译矩阵手动跑（集成测试里没做，因为会显著拉长测试时间）：

```sh
for t in arm-none-eabi thumbv7em-none-eabi \
         riscv32-unknown-none riscv64-unknown-none \
         x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
         x86_64-apple-darwin aarch64-apple-darwin \
         x86_64-pc-windows-msvc aarch64-pc-windows-msvc \
         wasm32-unknown-unknown wasm32-wasi \
         x86_64-unknown-freebsd aarch64-unknown-freebsd \
         x86_64-linux-android aarch64-linux-android \
         aarch64-apple-ios; do
  # 14_spill 只在 x86-64 上有意义：别的后端还没实现溢出，遇到高寄存器
  # 压力会（正确地）报错，所以不纳入矩阵。
  for f in $(ls tests/cases/*.zr | grep -v 14_spill); do
    ./target/debug/zr "$f" --target "$t" -O3 -c -o /tmp/t.o || echo "FAIL $t $f"
  done
done
```

## 编写用例的注意事项

寄存器分配器现在做了真正的活跃区间分析（`src/codegen/regalloc.rs`），
不再是"按 SSA 编号取模"，所以下面这条老限制已经解除：

> ~~当前寄存器分配器没有活跃性分析（`alloc_reg` 按 SSA 编号取模映射到固定
> 寄存器池）~~

仍然成立、需要注意的一点：

- **溢出（spilling）只在 x86-64 后端实现了**。可分配池是 10 个 GPR，其中
  跨调用存活的只能占 4 个 callee-save；活跃值超过这个数就会溢出到栈帧。
  AArch64 用线性扫描结果分配寄存器（不再取模），但同样没有溢出：活跃值超过
  18 个就**明确报错**。ARM32 / RISC-V / wasm 仍是取模分配，压力超限时也会
  报错，而不是安静地编出算错的程序。把它们也改成真溢出是剩下的一块工作。

> 早先的分配器是 `alloc_reg(v) = ALLOCATABLE[v.id() % 池大小]`，两个**同时
> 存活**的值只要编号同余就会抢同一个寄存器。它安静地算错过很多次，最严重的
> 一次是 `%5 = const 0` 放在 `r11`、后面 `%15 = const 7` 也落在 `r11`，
> `copy %5` 于是读到 7。这条限制已经解除。

辅助函数尽量保持无分支（`return a + b;` 这类），断言放在 `main`。分支会
拉长活跃区间，更早撞上池子大小。

另外，**用例里的断言必须和 Rust 的真实语义一致**。`05_logic.zr` 曾经写着
`if !(false || false) { return 2; }`——但 `!(false || false)` 是 `true`，
这条断言实际上是在断言"编译器有 bug"。当时编译器确实生成过错误代码，
恰好和这条错误断言对上了，所以用例一直是绿的；等代码生成修好、用例转红，
才暴露出是断言本身写错了。用 `rustc` 核对语义是确认此类问题的最快办法。

