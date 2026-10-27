# CLAUDE.md — zr 编译器项目入口

zr 是一门系统编程语言，设计取向介于 Rust 与 Zig 之间：所有权 + 零成本抽象 + 协程，
但**没有宏卫生性问题**（`println` 是普通函数）、**没有 GC**、**没有运行时开销**。
编译器用 Rust 写，**零第三方依赖**（只连标准库）。

- 版本：`0.8.0`（`Cargo.toml`）
- 语言规范权威来源：**`src/` 源码及其注释** > `README.md` 的用例部分 > 其余文档
- 给 AI agent 的操作约定、架构红线、在途工作：见 `AGENTS.md`
- 与 Rust 的逐条对照：见 `docs/rust-parity.md`（**部分结论已被在途改动超越**）

> 最近更新：2026-10-25。工作树有**未提交的在途改动**，是当前工作的第一现场。

---

## 1. 快速开始

```sh
cargo build                                    # 编译编译器 + 运行时汇编
cargo test --test zr_language                  # 全量回归：74 用例 × 5 优化等级，约 3 分钟
target/debug/zr prog.zr -O1 -o /tmp/prog && /tmp/prog
```

当前基线：**360 / 370 项通过**。挂的 10 项 = `52_if_let` × 5（在途改动引入的回归，
见 `AGENTS.md` §5）+ `71_derive` × 5（`PartialEq`/`Eq`/`Hash` derive 未实现）。

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
| `if let pat = expr { .. } else { .. }` | ✅（**当前有回归，见 §6**） |
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
| 关联类型 `type Output;` + `type Output = i32;` | ✅（`Self::Output` 可取） |
| 函数指针 `fn(T) -> R`（取址/传参/存储） | ✅ **x86-64 专属**，其它后端明确报错 |
| `?` 对 `Result`/`Option` | ✅ |
| `#[derive(Debug, Clone, Copy)]` | 🟡 只登记 trait 标记，**不生成方法**（见 §5） |
| `#[derive(PartialEq, Eq, Hash, PartialOrd, Ord, Default)]` | ❌ 报 `cannot derive ...: unknown derive trait` |
| 生命周期标注 `'a` | 🟡 **解析后丢弃**（`src/parser/ty.rs`），无任何借用/存活检查 |
| `dyn Trait` / trait 对象 | ❌ **无 `dyn` 关键字** |
| auto trait / blanket impl / `PhantomData<T>` | ❌ |
| turbofish `size_of::<T>()` | ✅ **仅调用位置**；`Option::<i32>::None`（类型路径上）❌ |
| `impl Trait` 参数（APIT/RPIT） | ❌ 解析失败 |
| `where T: !Trait` 负边界、更高阶函数、特化、孤儿规则 | ❌ |

### 3.5 内存与所有权

| 特性 | 状态 |
|---|---|
| `let` 默认不可变，`let mut` 可变；给不可变绑定赋值报错 | ✅ |
| `*const T` / `*mut T` 裸指针，`&x` / `&mut x` | ✅（语法层面） |
| 裸指针解引用 / 写入 | ✅ |
| **移动语义（use-after-move 检查）** | ❌ 不检查 |
| **借用检查（`&mut` 独占、别名）** | ❌ `infer.rs::check_borrows` 是空壳，只记录不检查 |
| 切片越界检查 | ❌ `check_index_scalar` 不检查边界 |
| **`unsafe` 关键字** | ❌ 词法器无 `KwUnsafe` |
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
| `error` | 错误处理 | **导入不了**（依赖 `dyn`，而 zr 无 `dyn`） |
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
| `use x as y` 重命名 | ❌ **无效**（静默失败：报 `cannot find value`） |
| `use foo::*` glob 导入 | ❌ 明确报错 |
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

`tests/cases/` 共 74 个用例，全部通过时 `cargo test` 绿。以下**已实现但无用例**，
属于回归盲区，加功能时容易破坏而无人发现：

- 多边界泛型 `T: Clone + Ord`（在途改动新支持，仅解析，无校验用例）
- 关联类型在泛型枚举上的投影
- `f8` / `f16` 窄浮点（`53_narrow_float.zr` 覆盖基础算术，但溢出/NaN/舍入未测）
- `static mut` 的复合赋值与 `&mut STATIC`
- `#[cfg(...)]` 的过滤路径（无"被过滤项引用应报错"的用例）
- 所有非 x86-64 后端（测试只跑宿主架构）
- `--emit-llvm` / `--wat` 输出
- 全部优化 pass 各自的隔离行为
- `zr fmt --check`、`zr pkg`、`zr lsp` 三个子命令

---

## 5. `#[derive]` 现状详解（高频踩坑区）

**能用的**：`Debug`、`Clone`、`Copy`。它们只**登记 trait 标记**：
- 生成的 `HirImpl` 块 `trait_id` 为 `None`（属性 args 里只有字符串）
- 求解靠 `trait_defs_by_name` 按名字反查
- 所以 `fn needs_clone<T: Clone>(x: T)` 配 `#[derive(Clone)]` 的类型**能过**
- 但**不生成 `.clone()` 方法**，`#[derive(Clone)]` 的结构体**不能** `.clone()`

**不能用但文档声称可用的**（`docs/rust-parity.md` 的错）：
`PartialEq`、`Eq`、`Hash`、`PartialOrd`、`Ord`、`Default` ——
全部报 `cannot derive <X>: unknown derive trait`，`tests/cases/71_derive.zr` 因此挂 5 个等级。

**修复位置**：`src/typeck/mod.rs::generate_derive_impls`（约第 660 行），
`match trait_name.as_str()` 里只有三个分支。

**在途已修的一个相关 bug**：早先枚举的 `attrs` 在 `collect_declarations` 阶段
落到 `_ => &[]`，`#[derive]` 在 `enum` 上被**静默丢弃**（结构体正常）。现在
`ast::TypeDef::Enum(e) => &e.attrs` 已修好；联合体仍走 `&[]`（`ast::UnionDef`
根本没有 `attrs` 字段）。

---

## 6. 当前未完成 / 有回归的工作

> 细节与最小复现见 `AGENTS.md` §5。

1. **⚠️ `52_if_let.zr` 回归（P0，在途改动引入）**
   `expected i32, found ()`，全 5 个优化等级失败。
   根因：新增的 `Expression::If` 分支类型计算对 `then_b` 套了 `block_diverges`，
   但 `If.then` 在 AST 里是 **`Block` 不是 `Expression`**；且 `IfLet` 有绑定时
   `then_hir` 经 wrapper 替换后 tail 丢失。只有**嵌套** `if let` 触发。

2. **`71_derive.zr` 失败**：`PartialEq` / `Eq` / `Hash` derive 未实现。

3. **`io_print_matches_rust` 失败**：宿主端 Rust 对照用例，需单独查。

---

## 7. 待办缺口（按优先级）

> 排序依据：**影响面 × 静默出错的可能性**。P0 = 会产生错误结果但不报错。

### P0 — 静默出错，必须先修

| 项 | 现状 |
|---|---|
| 无借用检查 / 移动检查 | README「所有权系统」「借用检查」两节与实际不符 |
| 切片/数组索引不检查越界 | `check_index_scalar` 不检查边界 |
| 无 `unsafe` 关键字 | README「指针与 unsafe」一节是愿景 |
| 无 `dyn` 关键字 | `std/error` 因此导入不了 |
| `use x as y` 静默无效 | 报 `cannot find value`，不是"重命名失败" |
| `f8` / `f16` 溢出行为 | 未验证（`f8` 最大约 240，无 IEEE 定义） |
| 浮点 `Display` 在 \|v\| ≥ 2⁶⁴ 时输出 `.0` | `std/io.zr::write_float` 用 `val as u64` 取整数部分 |

### P1 — 明确报错或明显缺失

1. **`#[derive]` 不生成方法**（`PartialEq`/`Eq`/`Hash`/`Ord`/`Default` 直接报错）
2. **无负向测试机制** —— `tests/cases/` 全部必须编译通过，"该报错的还在报错"没有自动化保障
3. **闭包未实现**：`closures are not implemented yet (captures are not lowered)`
   （注：`73_closures.zr` 通过，说明支持的是受限子集，README 已记录）
4. **`i128` / `u128` 在 64 位目标上不可用**（需寄存器对拆分）
5. **泛型推断只从实参和期望返回类型推断**，不看用法体
   （`let v = Vec::new(); v.push(1)` 推不出 `T`，需 `let v: Vec<i32>`）
6. **类型路径上的 turbofish 不支持**（`Option::<i32>::None`）
7. **穷尽性报错信息误导**：`match must have a trailing _ arm`
8. **格式规格只支持 4 种**，宽度/精度/对齐/填充被原样打印（不报错）
9. **`Option` / `Result` 不在 prelude**，需 `use core::Option;`
10. **无元数据 / 临时文件 API**：权限、时间戳、`tempfile` 缺失

### P2 — 工具链与生态

1. **LTO 未接线**（`src/opt/lto.rs`，`opt/mod.rs` 未声明）
2. **PGO 未接线**（`src/opt/pgo.rs`）
3. **C 后端未接 CLI**（`src/codegen/c_backend.rs`，无 `--emit-c`）
4. **`--emit-llvm` 是伪 IR**：手写文本，整数统一 i64、指针 i8\*，`clang` 消费不了
5. **自举（bootstrap）未开始**：`src/zr0/` 未接线，`future/bootstrap/` 只有设计文档
6. **宏系统未接线**：`src/macro_system/` 未 `mod` 声明；`proc-macro` 不存在
7. **可见性检查缺失**：只有 `pub`，无 `pub(crate)` / `pub(super)`
8. **glob 导入 `use foo::*` 不支持**
9. **协程 `yield` 只是类型占位**，不是可执行协程
10. **`const` 泛型参数解析接受但检查报错**
11. **trait 求解的高级特性全缺**：关联类型投影、更高阶函数（`for<'a>`）、特化、
    孤儿规则、负边界、`impl Trait` 参数

### P3 — 文档债

| 文档 | 问题 |
|---|---|
| `tests/README.md` | "尚未覆盖"表严重过期（`enum`/`match`/`泛型`/`智能指针` 说未实现，实际都在跑） |
| `future/README.md` | 状态列几乎全标 ✅ Done，与源码完全不符 |
| `docs/rust-parity.md` | 称泛型别名不支持、`#[derive]` 在结构体上报错——都已修好 |
| `README.md` 安全模型节 | 描述的是愿景（`unsafe`、借用检查、越界检查） |
| `Cargo.toml` description | "coroutines, CTFE, and a full async runtime" 夸大 |
| `CONTRIBUTING.md` | 未提负向测试缺失与架构红线 |

---

## 8. 继续实现时的顺序建议

按"依赖最少 + 影响最大"排序，每条都是可独立完成的工作单元：

1. **修 §6.1 的 `if let` 回归** —— 阻塞整个测试套件，且是类型安全相关
2. **给 `generate_derive_impls` 加 `PartialEq` / `Eq` 分支并真生成方法** —— 解锁 `71_derive`，
   是"derive 只登记不生成"这条 P1 缺口的样板实现
3. **建负向测试机制** —— 新目录 `tests/negative/*.zr` + 期望错误串文件，
   让"该报错的还在报错"有自动化保障。当前所有类型安全修复都无回归保障
4. **实现闭包捕获降级**（`captures are not lowered`）
5. **`use x as y` 重命名修复** —— 现在静默失败，是 P0 级的误导
6. **LTO 接线**（`src/opt/lto.rs` → `src/opt/mod.rs` → CLI 开关）
7. **`use foo::*` glob 导入**
8. **越界检查开关**（`ZR_BOUNDS_CHECK=1` 之类的编译期开关，先给切片）
9. **更新 `tests/README.md` 与 `future/README.md`** —— 两份文档严重失真，
   每次加功能都会让下一个 agent 走弯路

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
