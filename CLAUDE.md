# CLAUDE.md — zr 编译器项目入口

zr 是一门系统编程语言，设计取向介于 Rust 与 Zig 之间：所有权 + 零成本抽象 + 协程，
但**没有宏卫生性问题**（`println` 是普通函数）、**没有 GC**、**没有运行时开销**。
编译器用 Rust 写，**零第三方依赖**（只连标准库）。

- 版本：`0.8.0`（`Cargo.toml`）
- 语言规范权威来源：**`src/` 源码及其注释** > `README.md` 的用例部分 > 其余文档
- 给 AI agent 的操作约定、架构红线、在途工作：见 `AGENTS.md`
- 与 Rust 的逐条对照：见 `docs/rust-parity.md`（**部分结论已被在途改动超越**）

> 最近更新：2026-10-29。`feat/lang-features` 上已完成：`unsafe`/`dyn`/最小移动检查/常量越界检查；`52_if_let` 回归与 `71_derive` 失败均已修复；**又修掉了 `55_tuple_pattern` / `74_generic_alias` / `60_process` / `64_fs_path` / `io_print_matches_rust`（用户定义遮蔽标准库同名函数，见提交 `1ef4a02`）**；**支持 `use foo::*` glob 导入（见提交 `40b5b45`）**；全绿（93 用例 × 5 等级 + 宿主端对照用例）。

---

## 1. 快速开始

```sh
cargo build                                    # 编译编译器 + 运行时汇编
cargo test --test zr_language                  # 全量回归：93 用例 × 5 优化等级，约 3 分钟
target/debug/zr prog.zr -O1 -o /tmp/prog && /tmp/prog
```

当前基线：**93 用例 × 5 等级 + 宿主端对照用例 全绿**。

诊断：`--dump-tokens|ast|hir|mir|ssa|ssa-after|asm`、`--wat`、`--emit-llvm`、
`--no-incremental`、`--target <triple>`、`-g`；`ZR_TIME=1` 看各阶段耗时。
子命令：`zr fmt [--check]`、`zr run`、`zr build`、`zr pkg`、`zr lsp`。

---

## 2. 编译流水线

```
源码 .zr
  → lexer      (src/lexer/       token + Span)
  → parser     (src/parser/      递归下降 + Pratt；ast.rs / decl.rs / expr.rs / stmt.rs / ty.rs)
  → typeck     (src/typeck/      mod.rs ~8100 行是主战场；hir.rs 定义 HIR；infer.rs 只做推断)
  → mir        (src/mir/         builder.rs / async_lower.rs / narrow.rs / test_harness.rs)
  → ssa        (src/ssa/         支配树 + φ 节点；domtree.rs / print.rs)
  → opt        (src/opt/         pass_manager.rs)
  → codegen    (src/codegen/     寄存器分配 + 指令选择 + 目标格式)
  → ELF / Mach-O / PE / wasm
```

### 已接线的优化 pass（`src/opt/mod.rs:74-99`）

| Pass | 生效等级 |
|---|---|
| `fold_constants` | `-O1` 起 |
| `eliminate_dead_code` | `-O1` 起 |
| `eliminate_common_subexpr` | `-O2` 起 |
| `global_value_numbering` | `-O2` 起 |
| `peephole` | `-O2` 起 |
| `sparse_conditional_constant_propagation` | `-O2` 起 |
| `eliminate_dead_blocks` | `-O2` 起 |
| `inline_functions` | `-O3` / `-Oz` |
| `optimize_loops` | `-O3` / `-Oz` |

### 后端与目标格式

- **架构**：x86-64、aarch64、ARM A32、Thumb-2、RISC-V RV32IM/RV64IM、wasm32、wasm64
- **目标格式**：ELF（`objfile.rs`）、Mach-O（`macho.rs`）、PE（`pe.rs`）、wasm（`wasm.rs`）
- **寄存器分配**：线性扫描（`regalloc.rs`）；调试：`ZR_DEBUG_ALLOC`
- **调试信息**：DWARFv5（`dwarf.rs`，`-g`）
- 运行时：`runtime/*.s`，每平台一份汇编（Linux / macOS / Windows 的 x86-64 + aarch64，裸机 ARM / RISC-V）

### 确认是死代码 / 未接线的模块

| 路径 | 状态 |
|---|---|
| `src/zr0/` | `main.rs` 未 `mod` 声明（自举语法子集，仅设计文档） |
| `src/macro_system/` | `main.rs` 未 `mod` 声明（`macro_rules!` 走 `src/macros.rs`） |
| `src/opt/lto.rs` | `src/opt/mod.rs` 未声明 |
| `src/opt/pgo.rs` | `src/opt/mod.rs` 未声明 |
| `src/codegen/c_backend.rs` | CLI 无 `--emit-c` 入口 |

---

## 3. 已实现的功能特性（逐条已核对源码）

### 3.1 标量与字面量

| 特性 | 状态 |
|---|---|
| `i8 i16 i32 i64`、`u8 u16 u32 u64`、`isize` / `usize`（宽度 = 指针宽度） | ✅ |
| `f32` / `f64`，字面量后缀 `3.14f32` | ✅ |
| `f8` / `f16` 窄浮点 | ✅（zr 自创；`f16` 兼容 IEEE binary16，`f8` 是自定义 1/4/3 格式） |
| `bool`、`char` | ✅ |
| 整数字面量后缀 `42u64`、窄浮点后缀 | ✅ |
| `as` 截断/拓宽转换；实参位置**允许隐式拓宽**（`i32` → `i64`） | ✅ |
| 浮点 → 整数的 `as` 是 shortest round-trip | ✅ |
| `i128` / `u128` | ❌ **编译期报错**（64 位目标无寄存器对拆分；曾静默截断，现改为报错） |

### 3.2 控制流

| 特性 | 状态 |
|---|---|
| `if` / `else` 语句、`if` 表达式 | ✅ |
| `if let pat = expr { .. } else { .. }` | ✅ |
| `let ... else { .. }` | ✅（else 必须发散） |
| `while` / `while let` / `for` / `loop` | ✅ |
| `for i in 0..10`、`0..=10`、`for x in array` | ✅ |
| `break` 带值、`continue` | ✅ |
| 带标签循环 | ✅ 双风格：Zig `outer: loop { break outer; }` 与 Rust `'outer: loop { break 'outer; }` |
| `return` / `throw` | `return` ✅；`throw` ❌ 明确报错（"use `return Err(..)`"） |
| `?` 后缀运算符 | ✅ 对 `Result<T,E>` / `Option<T>` 生效；错误类型必须一致（无 `From` 转换） |
| `try` / `catch` | ❌ 语法可解析，类型检查明确报错 |
| `defer` | ❌ 类型检查明确报错（"the block would never run"） |
| `goto` | ❌ 类型检查明确报错（需基本块构造） |

### 3.3 模式匹配

| 特性 | 状态 |
|---|---|
| 整数字面量、`bool`、`char` 模式 | ✅ |
| 绑定臂、通配 `_` | ✅ |
| 或模式 `1 \| 2 \| 3` | ✅ |
| 范围模式 `1..=10` / `1..10` | ✅ |
| 结构体模式 `P { x: 1, y: v }`，含 `..` 尾与嵌套 | ✅ |
| 元组模式 `(1, v)` | ✅ |
| 枚举变体模式 `Shape::Circle(r)` | ✅ |
| 守卫 `n if n > 100` | ✅ |
| 穷尽性检查 | 🟡 全覆盖可省 `_`（与 Rust 一致），但缺臂报错信息误导：`match must have a trailing _ arm` |
| 浮点 / 字符串字面量模式 | ❌ 明确报错 |
| 切片模式 `[a, b, c]` | ❌ 明确报错 |
| 引用模式 `&v` | ❌ 解析期失败 |

### 3.4 类型系统

| 特性 | 状态 |
|---|---|
| 结构体（含 `#[repr(C)]` / `#[repr(C, packed)]` / `#[repr(u8)]`） | ✅ |
| 元组结构体、结构体更新语法 `P { x: 1, ..base }` | ✅ |
| 枚举（判别式 + 载荷区，布局与 Rust 一致；`Auto` 判别式挑最小整数） | ✅ |
| `type` 别名（非泛型） | ✅ **使用点展开**：注解、字段、数组/指针、`Vec<MyInt>`、别名套别名、前向引用 |
| `type X<T> = ...` **泛型别名** | ✅ **在途已修**（HEAD 提交 `8d6c475`）：保留参数并登记占位符，展平按实参替换 |
| 泛型函数 `fn f<T>`、泛型 `impl<T>` | ✅ |
| 泛型实参在类型注解里 `let x: Option<i32>` | ✅ |
| 单态化（`mono_cache` 去重，512 实例上限） | ✅ |
| `T: Trait` / `where` 边界**校验** | ✅ **在途已修**：单态化时经 `check_trait_bounds` 验证；早先解析完就丢弃 |
| 超类型继承方向（`trait Ord: Eq` 蕴含 `T: Equ`） | ✅ **在途已修**：`implements_trait` 把每个直接实现的 trait 往上展开 |
| trait 方法：关联函数、`self` / `&self` / `&mut self` | ✅ |
| `Self` 类型 | ✅ |
| 关联类型 `type Output;` + `type Output = i32;` | 🟡 **声明可用**，但 `Self::Output` / `P::Output` **投影不可用**（方法签名/体里写 `-> Self::Output` 报 `cannot find value`，`assoc_types` 只过 HIR、无投影解析） |
| 函数指针 `fn(T) -> R`（取址/传参/存储） | ✅ **x86-64 专属**，其它后端明确报错 |
| `?` 对 `Result`/`Option` | ✅ |
| `#[derive(Debug, Clone, Copy)]` | 🟡 `Clone` / `Copy` 是标记 trait；`Debug` 仍未生成方法（见 §5） |
| `#[derive(PartialEq, Eq, Hash, PartialOrd, Ord, Default)]` | ✅ 生成真实方法体（`src/typeck/derive.rs`）；泛型类型 / 枚举 `Default` / 枚举 `Ord` 显式报错 |
| 生命周期标注 `'a` | 🟡 **解析后丢弃**（`src/parser/ty.rs`），无任何借用/存活检查 |
| `dyn Trait` / trait 对象 | ✅ 词法+解析+类型检查骨架已实现（vtable 分派仍在未实现状态） |
| auto trait / blanket impl / `PhantomData<T>` | ❌ |
| turbofish `::<...>` | ✅ 调用位置（`size_of::<T>()`）与类型路径（`Option::<i32>::None`、`Box::<i32>::new(7)`、`Shape::<u8>::Circle { r: 7 }`）都可用 |
| `impl Trait` 参数（APIT/RPIT） | ❌ 解析失败 |
| `where T: !Trait` 负边界、更高阶函数、特化、孤儿规则 | ❌ |

### 3.5 内存与所有权

| 特性 | 状态 |
|---|---|
| `let` 默认不可变，`let mut` 可变；给不可变绑定赋值报错 | ✅ |
| `*const T` / `*mut T` 裸指针，`&x` / `&mut x` | ✅（语法层面） |
| 裸指针解引用 / 写入 | ✅ |
| **移动语义（use-after-move 检查）** | ✅ 最小版已实现（`let b = a` 后读 `a` 报 use-after-move；字段级/按值传参移动暂不覆盖） |
| **借用检查（`&mut` 独占、别名）** | ❌ `infer.rs::check_borrows` 仍是空壳，只记录不检查 |
| 切片越界检查 | 🟡 编译期常量下标越界已报 `index out of bounds`（变量下标暂不检查） |
| **`unsafe` 关键字** | ✅ 已实现（`unsafe { ... }` 块可解析和通过类型检查；当前按块表达式透传） |
| `Drop` trait 与析构 | ✅（`drop_impls`；数组/嵌套临时值析构有序） |
| 部分移动（partial move） | ✅ |
| static / static mut（**标量**） | ✅ 初值可折成字面量 → `.data`；读/写/复合赋值/`&STATIC` 正常。`static mut` 不需 `let mut` |
| 聚合 static（结构体/数组/枚举） | ❌ 不生成数据，读取时明确报错 |

### 3.6 协程与 async

| 特性 | 状态 |
|---|---|
| `async fn` / `.await` | ✅ 受限降级：future 帧在**堆上**，`.await` 轮询；`async fn main` 入口执行器可用 |
| `Pin<P>` / `!Unpin` | 🟡 有类型（`Ty::Pin`），语义完整度未验证 |
| `spawn` / 多任务 | ✅（`std/async.zr`、测试 `51_spawn.zr`） |
| 裸协程 `yield` | ❌ **类型占位，不是可执行的协程**（`Cargo.toml` 描述夸大） |

### 3.7 编译期求值（comptime）

| 特性 | 状态 |
|---|---|
| `comptime` 块 / 表达式（`src/comptime.rs`） | ✅ |
| `comptime fn` | ✅ |
| 宏展开 `macro_rules!`（`src/macros.rs`） | ✅ |
| const 泛型参数 `<const N: usize>` | ❌ 解析接受但检查时报错 |
| 常量折叠 pass | ✅ |

### 3.8 标准库（`std/`，14 个 `.zr` 文件，**模块名不带 `std::` 前缀**）

| 模块 | 内容 | 备注 |
|---|---|---|
| `core` | `Option` / `Result` / `Eq` / `Ord` / `Copy` / `Clone` / `Display` | `Option` 方法齐全（`is_some`/`unwrap`/`map`）；**无 `PhantomData`** |
| `io` | `println(s)` / `eprintln(s)` / `println_fmt(fmt, &[FormatArg], n)` / `read_bytes` / `stdin_read_line` / `File` | 格式串支持 `{}` `{:?}` `{:x}` `{:X}` `{:b}` `{:o}` `{:e}` `{:E}`、填充/对齐/`+`/`#`/`0`/宽度/精度、动态宽度 `{:1$}`、动态精度 `{:.*}`；缺命名参数 `{name}` 与 `{:#?}` |
| `string` | `String` | 无 `BufRead::read_line` 形态的 API |
| `collections` | 容器 | |
| `async` | 运行时 | 仅部分平台完整 |
| `error` | 错误处理 | **导入不了**（依赖 `dyn` vtable 分派，骨架已搭但未完工） |
| `fs` / `path` | 文件/目录/路径 | ✅；无权限、时间戳、`tempfile` API |
| `mem` | 内存 | |
| `net` | 网络 | |
| `process` | 进程 | |
| `simd` | 向量类型 | 目标相关 |
| `slice` | 切片辅助 | |
| `thread` | 线程 | |

**重要语义差异**：
- `str` 是 `{ptr, len}` **结构体**，切片写成 `[]const T`（元素在后），不是 Rust 的 `&str` / `&[T]`
- 数组 → 切片是隐式强转
- `println` 是**函数不是宏**：`println!("{}", 1)` 解析失败，要写
  `println_fmt("{} {}", &[fmt_int(a), fmt_int(b)], 2)`

### 3.9 属性与元数据

| 属性 | 状态 |
|---|---|
| `#[inline(always)]` / `#[inline(never)]` | ✅ |
| `#[link(name = "c", kind = "static")]` | ✅ |
| `#[test]` | ✅ 函数编进产物，`main` 前调用 |
| `#[repr(C)]` / `#[repr(C, packed)]` / `#[repr(u8)]` | ✅ |
| `#[derive(Debug, Clone, Copy)]` | 🟡 见 §5 |
| `#[derive(其它名字)]` | ❌ 报 `cannot derive ...` |
| `#[cfg(...)]` | ✅ 被过滤的项**不存在**（引用会报 undefined） |
| `#[marker]` trait | ✅（`is_marker`，可多 impl，只要不重叠） |
| `#[deprecated]` / lint 级别 | 🟡 有 `attributes.rs` 扫描，语义完整度未验证 |
| proc-macro | ❌（`src/macro_system/` 未接线） |

### 3.10 模块与外部接口

| 特性 | 状态 |
|---|---|
| `mod`、`use`、`use a::{b, c}` 分组导入 | ✅ |
| `use x as y` 重命名 | ✅（`75_use_alias.zr`；含多段路径 `use core::Option as Opt`） |
| `use foo::*` glob 导入 | ✅（`40b5b45`，`90_glob_import.zr`） |
| `pub` / `pub(crate)` / `pub(super)` | 🟡 仅 `pub`，无可见性检查 |
| `extern "C" { ... }` 声明块 | ✅ |
| `extern "C" fn ...`（带体定义） | ❌ 明确报错 |
| `C` ABI 兼容 | ✅ |
| 内联汇编 `asm!` | ✅（另有 SIMD） |
| 交叉编译 | ✅ `--target <triple>`，目标名与 `cargo --target` 同名；支持裸机 ELF 映像 |

### 3.11 工具链（`zr` 子命令）

| 子命令 | 状态 |
|---|---|
| 默认编译 / `zr build` / `zr run` | ✅ |
| `zr fmt [--check]`（`src/fmt.rs`） | ✅ |
| `zr pkg`（`src/pkg.rs`） | ✅ |
| `zr lsp`（`src/lsp.rs`） | ✅ |
| `zpm` 包管理（`src/zpm/`：registry / build_scripts） | 🟡 有代码，设计文档在 `future/zpm/` |
| 增量编译 | ✅ 默认开启，`--no-incremental` 关闭 |
| 测试框架 `#[test]` 内嵌 | ✅ |

---

## 4. 已实现但未测试覆盖

`tests/cases/` 共 93 个用例，全部通过时 `cargo test` 绿。以下**已实现但无用例**，
属于回归盲区，加功能时容易破坏而无人发现：

- 多边界泛型 `T: Clone + Ord`（✅ 已补 `91_multi_bound_generics.zr`：`P` 同时 impl 两个边界，`pick<P>` 两处边界都求解，形参按值移动两次）
- 关联类型投影（`Self::Output` / `P::Output`）——❌ 实测**不可用**（方法签名/体里报 `cannot find value`；`assoc_types` 只过 HIR、无投影解析，见 §3.4 修正）
- `f8` / `f16` 窄浮点——✅ 已覆盖（`53_narrow_float.zr`：基础算术、舍入（就近偶数，f8 尾数 3 位）、溢出成 Inf（第 127 行 `big * 100.0` → Inf）、`Inf - Inf` → NaN（第 130 行）、下溢成 ±0（第 134 行））
- `static mut` 的复合赋值与 `&mut STATIC`——✅ 已覆盖（`72_statics.zr`：`COUNTER += 2` 且断言值、`&mut COUNTER` 解引用写且断言值）
- `#[cfg(...)]` 的过滤路径（✅ 已补 `92_cfg_filter.zr`：互斥 host 键保证正好一个保留 + 恒假键被滤掉 + 引用滤掉的项报 undefined）
- 所有非 x86-64 后端（测试只跑宿主架构）
- `--emit-llvm` / `--wat` 输出——✅ 已补宿主端测试（`tests/zr_language.rs::toolchain_subcommands_emit`：`--emit-llvm` 写出含 `define i64 @main()` 的 IR 文本、`--wat`（wasm32）写出以 `(module` 开头的 WAT 文本）
- 全部优化 pass 各自的隔离行为
- `zr fmt --check`、`zr pkg`、`zr lsp` 三个子命令——✅ 已补宿主端测试（`toolchain_subcommands_emit`：`zr fmt` 重排源码、`zr fmt --check` 对未格式化返回非 0 且报 `needs formatting`、对已格式化返回 0；`zr pkg init` 创建 `zr.toml`；`zr lsp` 在 stdin 关闭时打印 `listening on stdio` 并干净退出）

---

## 5. `#[derive]` 现状详解（高频踩坑区）

**能用的**：`Clone`、`Copy`、`Debug`。它们只**登记 trait 标记**：
- 生成的 `HirImpl` 块 `trait_id` 为 `None`（属性 args 里只有字符串）
- 求解靠 `trait_defs_by_name` 按名字反查
- 所以 `fn needs_clone<T: Clone>(x: T)` 配 `#[derive(Clone)]` 的类型**能过**
- **不生成方法体**，`#[derive(Clone)]` 的结构体**不能** `.clone()`；
  `#[derive(Debug)]` 同理（不生成 `fmt` 方法）

**已实现（生成真实方法体）**：`PartialEq`、`Eq`、`Hash`、`PartialOrd`、`Ord`、`Default`。
- 实现在 `src/typeck/derive.rs`，在 AST 层生成 `Type__eq` / `Type__lt` / `Type__hash` / `Type__default` / `Type__clone` 方法体
- 泛型类型 / 枚举 `Default` / 枚举 `Ord` 显式报错
- `Hash` 对 `str` / 切片 / 数组按 `0` 计入（缺按内容 hash 的语义）；**指针 / 引用已改为按地址计入**（`b5f5a56`）；**元组已改为按内容计**（`38f1ddd`，`29fc950` 修好混合宽度元组字段访问后）
- 字段级诊断已修到**真实源码位置**：`StructDef` / `EnumDef` / `Field` 带 span，结构体字段精准到列、枚举结构体载荷也精准到字段、derive 拒绝诊断与默认值字段诊断都用类型/字段的位置（`bcf29f6` / `9238875` / `60174ca`）

**在途修过的一个相关 bug**：早先枚举的 `attrs` 在 `collect_declarations` 阶段
落到 `_ => &[]`，`#[derive]` 在 `enum` 上被**静默丢弃**（结构体正常）。现在
`ast::TypeDef::Enum(e) => &e.attrs` 已修好；联合体仍走 `&[]`（`ast::UnionDef`
根本没有 `attrs` 字段）。

**已知局限**：
- `Debug` 仍未生成方法体（需要运行期格式化设施，见 `docs/rust-parity.md`）
- 泛型类型上的 derive 不生成方法体（字段类型要靠单态化才知道）

---

## 6. 当前状态

> 所有已知 P0 回归（`52_if_let`、`71_derive`、`io_print_matches_rust`）已修复。
> 详见下方 §7。

---

## 7. 待办缺口（按优先级）

> 排序依据：**影响面 × 静默出错的可能性**。P0 = 会产生错误结果但不报错。

### P0 — 静默出错，必须先修

| 项 | 现状 |
|---|---|
| 借用检查 | ❌ `infer.rs::check_borrows` 仍是空壳（只记录不检查） |
| 无 `unsafe` 关键字 | ✅ 已实现（`unsafe { ... }` 块可解析和通过类型检查） |
| 无 `dyn` 关键字 | ✅ 解析+类型检查骨架已实现（vtable 分派仍在未实现状态） |
| 切片/数组索引不检查越界 | ✅ 编译期常量下标越界已报 `index out of bounds`（变量下标暂不检查） |
| 移动语义不检查 | ✅ 最小移动检查已实现（`let b = a` 后读 `a` 报 use-after-move；字段级/按值传参移动暂不覆盖） |
| `use x as y` 静默无效 | ✅ 已修复（`75_use_alias.zr`；含多段路径 `use core::Option as Opt`） |
| `f8` / `f16` 溢出行为 | ✅ 已验证：`f8` 最大约 240，`65504.0 * 2.0` 在 f16 正确输出 Inf |
| 浮点 `Display` 在 \|v\| ≥ 2⁶⁴ 时输出 `.0` | ✅ 已钉住回归（`tests/cases/86_bounds_check.zr` / `tests/zr_language.rs` `io_print_matches_rust` 已扩展） |
| 混合宽度元组字段访问读垃圾 | ✅ 已修复（`29fc950`）：元组字面量元素按期望元素类型检查，`(i32, i64)` 的 `t.1` 不再读到"值\|高位垃圾"；新增 `tests/cases/93_mixed_width_tuple.zr` |
### P1 — 明确报错或明显缺失

1. **`#[derive]` 剩余缺口**：方法体已经生成（`src/typeck/derive.rs`），
   `Debug` 仍是标记 trait（不生成方法）；泛型类型 / 枚举 `Default` / 枚举 `Ord`
   显式报错（位置已修到类型定义，`60174ca`）；字段级诊断已修到真实源码位置
   （`bcf29f6` / `9238875`）；`Hash` 对 `str` / 切片 / 数组按 `0` 计入
   （指针/引用按地址计 `b5f5a56`，元组按内容计 `38f1ddd`）
2. **闭包**：**无捕获**闭包已可存进变量 / 当实参 / 返回（提升成 `__closure_N`
   顶层函数指针，`tests/cases/83_closure_fnptr.zr`）；`(|x| x + 1)(5)` 仍是
   定义处立即调用（单态化内联）。**带捕获**闭包仍只支持立即调用——存进变量 /
   当实参 / 返回报 `closures can only be invoked immediately where they are defined`
3. **`i128` / `u128` 在 64 位目标上不可用**（需寄存器对拆分）
4. **泛型推断只从实参和期望返回类型推断**，不看用法体
   （`let v = Vec::new(); v.push(1)` 推不出 `T`，需 `let v: Vec<i32>`）
5. **穷尽性报错信息误导**——✅ 已改进（`a526953`：枚举缺臂列出缺失变体名，`17_match_missing_variant.zr`；整型/标量仍保留尾 `_` 臂措辞）
6. **格式串剩两处**：命名参数 `{name}` 与聚合类型的多行 Debug `{:#?}`
   （标量/字符串的 `{:#?}` 与 `{:?}` 输出一致，符合 Rust；`#` 对 `{:?}`
   只对聚合有影响，而聚合 Debug 还未实现）；其余规格已与 Rust 逐字符对齐
7. **`Option` / `Result`（含变体 `Some` / `None` / `Ok` / `Err`）已在 prelude**——✅ 已实现（`4ed15d7`），无需显式 `use core::Option;`
8. **无元数据 / 临时文件 API**：权限、时间戳、`tempfile` 缺失

### P2 — 工具链与生态

1. **LTO 未接线**（`src/opt/lto.rs`，`opt/mod.rs` 未声明）
2. **PGO 未接线**（`src/opt/pgo.rs`）
3. **C 后端未接 CLI**（`src/codegen/c_backend.rs`，无 `--emit-c`）
4. **`--emit-llvm` 是伪 IR**：手写文本，整数统一 i64、指针 i8\*，`clang` 消费不了
5. **自举（bootstrap）未开始**：`src/zr0/` 未接线，`future/bootstrap/` 只有设计文档
6. **宏系统未接线**：`src/macro_system/` 未 `mod` 声明；`proc-macro` 不存在
7. **可见性检查缺失**：只有 `pub`，无 `pub(crate)` / `pub(super)`
8. **glob 导入 `use foo::*` 不支持** —— ✅ 已实现（`40b5b45`，`90_glob_import.zr`）
9. **协程 `yield` 只是类型占位**，不是可执行协程
10. **`const` 泛型参数解析接受但检查报错**
11. **trait 求解的高级特性全缺**：关联类型投影、更高阶函数（`for<'a>`）、特化、
    孤儿规则、负边界、`impl Trait` 参数

### P3 — 文档债

| 文档 | 问题 |
|---|---|
| `tests/README.md` | ✅ 已修正（`fb4dfb2`）："尚未覆盖"表重写为按源码核对的两栏，并补 52–90 用例清单 |
| `future/README.md` | ✅ 已修正（`fb4dfb2`）：状态列按源码实际结论重排（真正实现的留 ✅，骨架 🔧，空壳 ⬜） |
| `docs/rust-parity.md` | ✅ 已修正（`1d38ca3`）：泛型别名改"已支持"、`dyn` 改"骨架可用" |
| `README.md` 安全模型节 | ✅ 已修正（`1d38ca3`）：加警示说明是愿景，不是现状 |
| `Cargo.toml` description | ✅ 已修正（`1d38ca3`）：去掉夸大的 "CTFE, and a full async runtime" |
| `CONTRIBUTING.md` | ✅ 已修正（`1d38ca3`）：补测试约定与架构红线摘要 |

---

## 8. 继续实现时的顺序建议

按"依赖最少 + 影响最大"排序，每条都是可独立完成的工作单元：

1. **`use x as y` 重命名修复** —— ✅ 已完成（`75_use_alias.zr`，含多段路径 `use core::Option as Opt`）
2. **格式串 `{name}` 命名参数与 `{:#?}` 多行 Debug** —— P1 缺口，影响可读性
3. **LTO 接线**（`src/opt/lto.rs` → `src/opt/mod.rs` → CLI 开关）—— P2
4. **`use foo::*` glob 导入** —— ✅ 已完成（`40b5b45`，`90_glob_import.zr`）
5. **穷尽性报错信息修措辞** —— ✅ 已完成（`a526953`：枚举缺臂列出缺失变体名，`17_match_missing_variant.zr`）
6. **`Option` / `Result` 加进 prelude** —— ✅ 已完成（`4ed15d7`，含变体 `Some`/`None`/`Ok`/`Err`）
7. **`i128` / `u128` 64 位目标** —— P1，需寄存器对拆分
8. **更新 `tests/README.md` 与 `future/README.md`** —— ✅ 已完成（`fb4dfb2`；另 `1d38ca3` 修正 README / rust-parity / CONTRIBUTING / Cargo.toml）
9. **`Debug` derive 方法体** —— P1，需要运行期格式化设施（目前只有标量+结构体/元组/枚举的组合）

---

## 9. 约定

- **提交信息用中文**，Conventional Commits（`feat:` / `fix:`）。风格是**详细段落**：
  先讲"早先怎么错"，再讲"为什么这么改"，最后 `Co-Authored-By: Claude Code <noreply@anthropic.com>`
- 注释用中文，密度与现有代码一致（本项目注释很密，改代码时顺手补上下文是对的）
- 加语言特性顺序：`lexer` → `parser` → `typeck` → `mir` → `ssa` → `codegen`
  → `std/` → `tests/cases/`。**跳步会让下游拿到占位数据。**
- 最小复现放 `/tmp/`，别塞进 `tests/cases/`（那里全部必须编译通过并返回 0）
- `cargo build` 通过 ≠ 改对了。语言改动至少跑 `cargo test --test zr_language`；
  类型检查改动要额外用 `/tmp/x.zr` 手工确认负向用例
- **不要在死代码目录里"修 bug"**（`src/zr0/`、`src/macro_system/`、`src/opt/lto.rs`、
  `src/opt/pgo.rs`、`src/codegen/c_backend.rs`）—— 先确认它们该不该接线
