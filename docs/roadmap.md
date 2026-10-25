# zr 完善功能与修复规划（v0.9.0 → v1.0.0）

> 更新时间：2026-10-25
>
> 设计主线：
> - **安全线**参考 Rust：编译期挡住的尽量挡，挡不住的用**显式关键字**声明"我接受后果"，**绝不静默降级**。
> - **易用线**参考 Zig：语法短、错误信息准、格式化不用宏、格式化器强制、编译器自带格式化器、`zr test` 一键跑、`comptime` 承担一部分样板。
>
> 两条线在 `unsafe` / `comptime` / `intrinsic!` 三个关键字处交汇——它们各自管一段，不能混。

---

## 0. 现状基线

- **语法面广、语义薄**：主流语法都解析过了，但安全模型（借用 / 移动 / 生命周期 / 边界 / `dyn` / `unsafe`）几乎全空；`trait` 求解的高级部分（关联类型 / 特化 / HRTB / 负边界）只有 AST 槽没语义；LTO / PGO / proc-macro / 自举全是文档空壳。
- **测试**：`tests/cases` 74 例 × 5 等级 = 370 项，当前 360 过 / 10 挂（`52_if_let` 全 5 等级 + `71_derive` 全 5 等级）。
- **没有负向测试机制**（见 §10）。
- **文档过期**：`docs/rust-parity.md` 部分过期、`future/README.md` 的 ✅ 表几乎全错、`tests/README.md` 严重过期、`Cargo.toml` description 夸大。

---

## 1. 设计原则（写代码前先立规矩）

1. **默认安全、显式逃逸**。凡是绕过检查的动作——裸指针写、跨生命周期借用、越界索引、类型双关——都必须有显式关键字入口。没有入口就**编译报错**，不许静默降级。
2. **一个语法不承诺两件事**。
   - `unsafe`：管"编译器不能证明的指针 / 类型 / 并发假设"。
   - `comptime`：管"编译期执行"。
   - `intrinsic!`：管"底层内置指令"。
   三段互不重叠。
3. **错误信息 > 通过编译**。宁可报错把用户吓一跳，也不要让错误悄悄进运行时。所以 `subtype` 的错误不许再被 `.ok()` 吞，`#[derive]` 不认识的 trait 名字必须报错（现在的行为是对的），越界检查开关默认开。
4. **格式化器是编译器的一部分**（学 Zig 的 `zig fmt`）：`zr fmt` 维持"记号流不变"的强约束；`zr test` / `zr run` / `zr build` 三件套齐了再谈生态。
5. **不要背不存在的债**。文档里写"已实现"就必须能被 `grep` 验证；`future/` 里的 ✅ 表在实现前先改成 🔧 或 📐。这条是**流程红线**，写进 `CONTRIBUTING.md`。

---

## 2. 安全线：借用 / 移动 / 生命周期的三层结构

Rust 的借用检查是**线性化 + 借用区间 + 生命周期子类型**三件事合起来，zr 目前三件事都没做，只能一次上。

### 2.1 借用区间图（阶段 A）

`src/typeck/infer.rs::check_borrows` 现在返回空 errors。改造方向：

- **借用分类**：
  - `&T` → 共享只读。
  - `&mut T` → 独占可写。
  - 裸指针 `*const T / *mut T` → 无检查，只能出现在 `unsafe` 块内。
- **借用区间**：每个借用从 `record_borrow` 起点到**最后一次使用**的结束。区间图用 MIR 的基本块 + 支配树算（`src/ssa/domtree.rs` 已有支配树）。
- **冲突判定**：
  - `&T` × `&T` → OK
  - `&mut T` × 任意 → 冲突，报 `error[E0499]: cannot borrow as mutable more than once`
  - `&T` × `&mut T` → 冲突，报 `error[E0502]: cannot borrow as immutable while mutable`

这条做完，`check_index_scalar` 里对 slice 的读可以自动升级为**可证明安全**（区间内不冲突）。

### 2.2 移动与借用（阶段 B）

现状：`let s2 = s1; return s.v;` 现在通过编译（无 use-after-move 检测）。

- **类型分类**：`Copy`（`i32 / u64 / bool / char` + `#[derive(Copy)]`）自动拷贝，其余为 move。
- **移动记账**：`let s2 = s1` 时把 `s1` 标记为 moved，之后读 `s1` 报 `error[E0382]: use of moved value`。
- **借用与移动交互**：
  - `&s1` 期间不许移动 `s1`。
  - `s1` 已经 moved 之后 `&s1` 报错。
- **部分移动**：`let (a, _) = pair;` 允许字段级移动，`pair.a` 之后不可用但 `pair.b` 仍可用。`tests/cases/42_temp_partial_move.zr` 已经有雏形，扩到结构体字段。

### 2.3 生命周期标注（阶段 C）

现状：`'a` 在 `src/parser/ty.rs` 里被解析后**丢弃**，不产生任何检查。

- **默认省略**：绝大多数函数体里的局部借用不需要显式写，`&T` 的 lifetime 自动绑定到参数。
- **显式场景**：只有三种情况强制写——
  - 跨函数的返回借用。
  - 结构体字段里的借用。
  - `static` 借引用。
- **子类型判定**：`'long: 'short`（长寿命能赋给短寿命）。
- **静态常量**：`'static` 单独一类。
- **红线**：生命周期只做**局部检查**，不做全局流分析——zr 没有多 crate、没有 `PhantomData`，全局 lifetime 分析收益太低。

---

## 3. 逃逸机制：三个入口

### 3.1 `unsafe`（阶段 D）

现状：`src/lexer/token.rs` 里**没有** `KwUnsafe`，`unsafe { … }` 直接解析报错。

- **词法**：`KwUnsafe` 加进 `token.rs`。
- **语法**：
  - `unsafe { ... }` 块。
  - `unsafe fn f(...)`（整个函数）。
  - `extern "C" fn f(...)`（C ABI 声明，配合 `std::mem::transmute` 之类）。
- **语义**：`unsafe` 块内允许——裸指针读写、跨类型双关、`intrinsic!(transmute)`、跨 crate 的私有字段访问。
- **诊断**：每次使用**必须**给出具体理由的注释 `// unsafe: <reason>`，`zr check` 时如果 `unsafe` 块内没有 `// unsafe:` 注释，warn 级别提示。
- **边界**：`unsafe` **不能**关借用检查、不能关移动检查、不能关类型检查。这三条是 Rust 的红线，zr 也要守住。

### 3.2 `dyn Trait`（阶段 E）

现状：词法表里没 `KwDyn`；`fn f(x: dyn A)` 报 `expected ')'`。

- **词法**：`KwDyn` 加进 `token.rs`。
- **语法**：`dyn Trait` / `&dyn Trait` / `&mut dyn Trait` / `Box<dyn Trait>`（`Box` 是 `std::mem::Box<T>`，独占堆对象）。
- **运行时表示**：`(data_ptr: *mut T, vtable: *const Vtable)`，vtable 是编译器生成的方法表 `{ method1, method2, drop, size, align, name }`。
- **代码生成**：
  - `x.method()` 变成 `(**x.vtable).method(x.data)`。
  - 虚调用的寄存器分配要单独处理（不能用调用方约定的固定寄存器）。
  - `Drop` 通过 vtable 调用，`Box<dyn Trait>` 析构时走 `vtable.drop`。
- **诊断**：没有 `dyn` 但试图做动态分派时报错，指向正确的语法：`error[E0716]: dynamic dispatch requires `dyn Trait`; use `fn f(x: dyn Trait)` or wrap in `Box<dyn Trait>``.
- **前置依赖**：这个做完 `std/error` 才能导入（现在因为 `dyn` 不存在导入不了）。

### 3.3 `comptime`（阶段 F）

现状：`src/comptime.rs` 有求值器，能跑 `comptime 40 + 2` 和 `comptime fn fact`，能力中等。

- **`comptime` 关键字的作用范围**：
  - `comptime fn`（编译期函数）。
  - `comptime` 表达式（编译期表达式）。
  - `comptime { ... }` 块。
- **限制清单**：`comptime fn` 内不允许——
  - `return`（要用 `break`）。
  - `yield`、`break/continue` 带值。
  - `loop` 无显式退出（编译期死循环要能在类型检查阶段发现）。
  - 外部 I/O、动态内存分配。
- **求值器补齐**：目前支持标量 / 数组 / 结构体 / 枚举 / 字符串字面量；补上——
  - 元组。
  - 位运算。
  - `if/else` 全分支。
  - `while` 循环（带步数上限）。
  - 递归（带深度上限）。
- **递归上限**：目前递归深度和步数上限都写了常量；改成 `comptime fn` 上可以标 `#[limit(depth: 100, steps: 10_000)]`。
- **诊断**：`comptime` 里的错误信息必须打印 `at <span>`，让用户知道是编译期还是运行期。

---

## 4. 易用性线：Zig 风格的"少即是多"

### 4.1 错误处理（阶段 G）

现状：zr 有 Zig 的 `T!E` 类型和 Rust 的 `Result<T, E>` 两套，且 `f()!` 解包运算符不工作。

- **保留两套，明确分工**：
  - `T!E`：编译期确定错误集合（Zig 风格），`!` 后缀解包，配合 `try` 关键字。
  - `Result<T, E>`：跨模块 / 跨 crate 返回，`?` 后缀解包，配合 `From` 转换。
- **`?` 与 `!` 的语义统一**：都从当前函数 `return`，都是把"错误分支"提前返回。
- **`catch` 表达式**：`x.catch(0)` 相当于 `x.unwrap_or(0)`，比 Zig 的 `try` 更省字。
- **诊断**：`T!E` 的 `E` 必须编译期可枚举，运行时动态错误要用 `Error` trait + `dyn Error`。

### 4.2 格式化输出（阶段 H）

现状：`println!` 是函数 + `println_fmt` 走格式串，规格只支持 `{}` `{:?}` `{:x}` `{:X}`，宽度 / 精度 / 对齐 / 填充**原样打印**（不报错）。

- **升级为宏风格但不引入卫生性问题**：`println!` 通过 `macro_rules!` 展开成 `println_fmt` 调用，格式串经过编译期扫描确定参数个数和类型，编译期就能发现占位符不匹配。
- **规格扩展**：`{:width}` / `{:precision}` / `{:#}`（紧凑） / `{:>width}` / `{:>width$}`。借用 Rust 完整规格，但**编译期报错**不认识的规格，绝不原样打印。
- **`format!`**：返回 `str` 值而不是直接写 stdout，`println!("{}", format!("x={}", x))`。
- **`write!`**：往任意 `io::Write` 写。
- **诊断**：格式串里出现 `{}` 但没参数、参数多于占位符、类型不支持该规格（如浮点不支持 `{:x}`）——**编译期报错**。

### 4.3 标准库补齐（阶段 I）

按优先级：

1. **`HashMap<K, V>` / `HashSet<T>`**：现在 `std/collections.zr` 只有 `Vec` / `Array`，没有哈希表——这是标准库不完整的第一块短板。
2. **`Iterator` combinator**：`filter_map / zip / chain / rev / enumerate / partition / collect`。`std/core.zr:629` 现在只有 `next`。
3. **`From / Into` blanket impl**：现在没有，`Result<T, E>` 的 `?` 只能做同类型错误，不能自动转换。
4. **`Arc<T>` / `Mutex<T>` / `OnceCell<T>` / `Lazy<T>`**：并发原语从 `thread` 提到标准库顶层。
5. **文件元数据 / `tempfile`**：`fs::metadata` 返回 `{ len, mode, mtime, ctime, inode }`；`tempfile::temp_dir()` / `temp_dir_file()`。
6. **类型化时间**：`Duration` / `Instant` / `SystemTime`，`zr_time_ns` 藏起来。
7. **`panic!` / `assert!` / `assert_eq!`**：三个宏，配合 `#[test]` 桩（`src/mir/test_harness.rs` 已有）。
8. **`f16` / `f8` 的 `Display` 修复**：|v| ≥ 2⁶⁴ 时输出 `.0` 的 bug（`std/io.zr` 的 `write_float`）。
9. **`Iterator` 对 slice 的 `iter / iter_mut` 实现**：现在 slice 没有迭代器。
10. **`Option / Result` 的方法**：`map_or / map_err / and_then / or_else / filter / zip`。

### 4.4 宏系统（阶段 J）

现状：`src/macros.rs` 已经跑通了 `macro_rules!`。

- **保留 `macro_rules!`**，这是主要路径。
- **暂不做 proc-macro**：`future/misc/macro_system.rs` 里的 proc-macro 提议先撤掉——zr 自举都没完成，proc-macro 生态不现实。
- **`#[derive]` 补齐**：加 `PartialEq / Eq / Hash / PartialOrd / Ord / Default` 六个名字，每个都生成**方法体**而不是只登记标记（现在是 `trait_id: None` 空壳）。
- **诊断**：derive 不认识的 trait 名字继续报错（现在是对的）。

### 4.5 工具链（阶段 K）

现状：`zr fmt` 有、`zr --lsp` 有（`src/lsp.rs`）、`zpm` 有骨架、`#[test]` 桩有。

- **`zr check`**：走完整编译直到 MIR，不生成机器码。配合 `-C debug-assertions=yes` 打开边界 / 溢出检查。
- **`zr run`**：编译 + 直接执行，替代现在 `zr prog.zr -o /tmp/prog && /tmp/prog`。
- **`zr build`**：多文件项目，读 `zr.toml`，支持 `build.zr` 脚本。
- **LSP 补齐**：`references / rename / signatureHelp / inlayHints / hover 显示类型详情 / codeAction`（快速修复：`unwrap()` → `unwrap_or(0)`）。
- **`zr doc`**：类似 `rustdoc`，从 `///` 注释生成 HTML 文档。这条放到最后。

---

## 5. 编译器基础设施的清理

### 5.1 死代码清理

- **保留并接线**：
  - `src/codegen/c_backend.rs`（`--emit-c`）。
  - `src/opt/lto.rs`（`-C lto`）。
  - `src/opt/pgo.rs`（`-C profile-generate / profile-use`）。
- **保留但不接线**：
  - `src/zr0/`（自举子集）。
  - `src/macro_system/`（等 proc-macro 决定后再评估）。
- **删除或归档**：把上面这些空壳移到 `future/scaffold/` 里，明确标记"未接入编译流水线"。这一步**先做**，因为它影响开发者判断"该在哪里改 bug"。

### 5.2 优化 pass 补齐

现状已接线的 pass（`src/opt/mod.rs::run`）：`fold_constants`、`eliminate_dead_code`、`eliminate_common_subexpr`、`global_value_numbering`、`peephole`、`sparse_conditional_constant_propagation`、`eliminate_dead_blocks`、`inline_functions`、`optimize_loops`。

按对性能的实际影响排序：

1. **循环不变量外提**（LICM）：现在没有；`for i in 0..n { a[i] = x + c; }` 里的 `x + c` 每轮都算。
2. **循环展开**（Ungrouping）：小常量循环（≤ 8 次迭代）直接展开。
3. **逃逸分析**（Escape Analysis）：函数内分配的结构体如果能证明不逃逸，落栈上。
4. **部分求值**（Partial Evaluation）：`comptime` 已经在类型检查阶段跑，把结果回灌到 MIR 层做常量传播。
5. **函数去虚化**（Devirtualization）：`dyn Trait` 只有一个实现类时，直接调静态方法。
6. **SCEV / super-scalar 归纳变量**：`for i in 0..n` 里的 `i` 识别为可归纳，向量化到 SIMD。
7. **内联启发式升级**：现在 `inline_functions` 是无脑内联；加 size cost + hotness 启发式，冷路径不内联。

### 5.3 后端

现状：6 个后端已接线（`encoder.rs / encoder_aarch64.rs / encoder_arm.rs / encoder_thumb.rs / encoder_riscv.rs / wasm.rs`）；4 种对象格式（ELF / Mach-O / PE / wasm）。

- **x86_64**：补 SSE2 / AVX2 指令选择（`SimdBinary` 现在报 `unsupported_simd`）；补寄存器分配的 spill reload 优化。
- **aarch64**：补 SVE（`std/simd.zr` 已经预留了接口）。
- **RISC-V**：补 RV64GC 的 C 扩展（压缩指令）、M 扩展的除法优化。
- **WASM**：补 wasm64 提案的稳定支持（现在指针宽度靠 `addr64` 判断）；补 simd128 提案。
- **DWARF 补齐**：`.zdebug_info` / `.zdebug_line` / `.zdebug_str` / `.zdebug_abbrev` / `.zdebug_ranges` 全上；`-g` 打开。现在 `src/codegen/dwarf.rs` 只覆盖字符串表和最小 DIE。
- **`--emit-llvm`**：**删掉**或**真做**。现在的伪 IR 文本 `clang` 消费不了，留着会误导用户。真要 emit IR 就 emit 真实的 LLVM IR（但那是大工程，可以放到 v1.0 之后）。

### 5.4 目标格式

- ELF / Mach-O / PE 三件套已支持，`wasm` 也支持。
- 补 **PE on macOS**（罕见但 x86_64 Windows 通过 wine 跑需要）。
- 补 **裸机固件格式**（`.bin` / `.hex` / `.elf` with entry point）。

---

## 6. `#[derive]` 补齐（独立小项目）

现状：`generate_derive_impls` 只接受 `Debug / Clone / Copy`，其余名字直接报 `cannot derive ...`。**只登记 trait 标记（`trait_id: None`），不生成方法体**——`#[derive(Clone)]` 之后 `p.clone()` 报 `no field 'clone' on type`。

**这是 P0，因为 `71_derive.zr` 全 5 等级挂着。**

- **`generate_derive_impls`** 从只登记 `HirImpl { trait_id: None }` 升级为生成**完整方法体**。
- 每个 derive 展开成什么样的方法：
  - `Clone`：结构体逐字段 `clone`，枚举按变体分派。
  - `Copy`：marker trait，方法体空。
  - `PartialEq`：结构体逐字段 `==`，枚举先比判别式再比载荷。
  - `Eq`：marker trait。
  - `PartialOrd`：同 `PartialEq` 但返回 `Ordering`。
  - `Ord`：同 `PartialEq` 但返回 `Ordering` 且不许 `Equal`。
  - `Hash`：调用 `std::hash::Hasher::write_*`，字段顺序敏感。
  - `Default`：每个字段调 `Default::default()`，枚举必须有一个 `#[default]` 标记的变体。
  - `Debug`：生成 `Debug::fmt` 返回格式化的字符串表示。
- **诊断**：字段类型不满足对应 trait 时报错，指向具体字段。

---

## 7. 阶段路线图

### v0.9.0 — 修复 + 文档对齐（1 周）

- 修 `52_if_let.zr` 回归：`expr_diverges` 把 `If.then` 当 `Expression` 处理，改成按 `Block` 处理。
- 修 `subtype().ok()` 静默吞错：改成收集 + 报错。
- `generate_derive_impls` 支持 `PartialEq / Eq`（先这两个，`71_derive` 通过）。
- 修 `--emit-llvm` 输出（要么删掉，要么真做）。
- `docs/rust-parity.md` 更新到与源码一致；`future/README.md` 的 ✅ 表全改成 🔧 / 📐；`tests/README.md` 里"尚未覆盖"的 enum / match / 泛型 / 智能指针条目删掉。
- `CONTRIBUTING.md` 加"文档红线"一节。
- **补 `tests/cases/negative/` + `tests/zr_language_negative.rs` 测试框架**（见 §10）。

### v0.10.0 — 安全基础（3 周）

- `unsafe` 关键字 + 作用域语义（§3.1）。
- 借用区间图 + 冲突检测（§2.1）。
- 移动记账 + use-after-move 检测（§2.2）。
- 切片 / 数组边界检查默认开启（`-C debug-assertions=yes`）。
- `#[derive]` 支持 `PartialEq / Eq / Hash / Default`。
- `HashMap` / `HashSet` 进 `std/collections.zr`。
- **目标**：`tests/cases` 全绿，新加 `tests/cases/unsafe_*` / `borrow_*` / `move_*` 三个系列。

### v0.11.0 — 类型系统（3 周）

- 生命周期 `'a` 保留 + 子类型判定（§2.3）。
- `dyn Trait` + vtable（§3.2）。
- 关联类型解析 + 求解（`HirAssocType` 从空结构体升级）。
- `impl Trait` 在参数位置（APIT）真作为泛型参数处理。
- `for<'a>` 高阶边界保留 + 求解。
- **目标**：`std/error` 能导入；`Option::<i32>::None` 能解析。

### v0.12.0 — 易用性（2 周）

- `?` / `!` / `catch` 三种错误解包语义统一（§4.1）。
- `println!` 升级为 `macro_rules!` + 编译期规格检查（§4.2）。
- `Iterator` combinator 全上（§4.3 第 2 项）。
- `Arc / Mutex / OnceCell / Lazy` 进标准库（§4.3 第 4 项）。
- LSP 补 `references / rename / signatureHelp / inlayHints`。

### v0.13.0 — 编译器性能（2 周）

- 循环不变量外提 + 循环展开 + 逃逸分析（§5.2 第 1-3 项）。
- 内联启发式升级（size cost + hotness）。
- DWARF 补齐 `.zdebug_line` / `.zdebug_info`（`-g` 完整可用）。
- LTO（fat）+ PGO（插桩 + 消费）接线到 CLI。

### v1.0.0 — 稳定（1 月）

- **API 稳定承诺**：语言语法冻结，只加不减（除了明确标记 `#[deprecated]`）。
- **自举**：`zr0` 语法子集 + `c_backend` + `-C target=c-c`，`zr` 自己用 `zr0` 重写。
- **跨平台**：Linux x86_64 / aarch64 / macos x86_64 / arm64 / windows x86_64 全上 CI。
- **基准**：`benchmarks/` 里跑 rustc 同源代码对比，性能差距报告写进 `docs/benchmarks.md`。

---

## 8. 关键决策记录

先决策再动手，避免来回改。

1. **`unsafe` 要不要允许关借用检查？** 不允许。借用检查是 zr 的核心承诺，开了 `unsafe` 就回到 Zig，反而失去 Rust 差异化。真要关就另设 `#[unsafe_noborrow]` 属性，显式到属性级别。
2. **生命周期要不要做全局流分析？** 不做。zr 没有多 crate，`PhantomData` 也没有，局部检查足够。
3. **`comptime` 里的递归要不要允许？** 允许，但要显式声明上限；默认上限递归深度 100、步数 10⁴。
4. **`dyn Trait` 的 vtable 要不要含 `size / align`？** 要——`Box<dyn Trait>` 需要知道实际大小才能分配。
5. **`--emit-llvm` 是删还是做？** 删。手写伪 IR 文本 `clang` 消费不了，留着只会误导。真要 emit IR 就 emit 真实的 LLVM IR（放到 v1.0 之后）。
6. **`c_backend` 是真做还是删？** 真做。它配合 `zr0` 是自举的关键路径，`future/bootstrap/c_backend.md` 的设计文档已经存在。
7. **proc-macro 做不做？** 不做。等自举完成、`zr.toml` 稳定之后再评估。`future/misc/macro_system.rs` 里 proc-macro 的段落先撤。
8. **`f16` / `f8` 保留还是删？** 保留。这是 zr 相对 Rust 的差异化点，但要把 `Display` 的 bug 修掉，并明确标注"仅用于嵌入式 / 量化场景"。

---

## 9. 文档同步清单

每次落地上述任一改动，都要同步更新下面这些文件：

- `README.md`：语法速览 / 语言特性详解 / 路线图。
- `docs/rust-parity.md`：状态表（✅ / 🟡 / ❌ 三态）。
- `future/README.md`：**实现前不许标 ✅**，只有代码合并且测试通过才能改。
- `tests/README.md`：覆盖清单。
- `CONTRIBUTING.md`：新增流程红线。
- `AGENTS.md`：架构红线（如果改动触碰了 §3 的红线）。
- `Cargo.toml` description：不能夸大，只能描述已实现的能力。

---

## 10. 最后一条建议

**先修测试，再改语言。**

现在 `tests/cases` 只有正例，没有负例。想验证"某个错误真被挡了"，只能手工写 `/tmp/x.zr` 编译确认（`AGENTS.md` 里也承认了）。建议 v0.9.0 阶段就先补一个 `tests/cases/negative/` 目录 + `tests/zr_language_negative.rs` 测试框架，专门跑"必须编译失败"的用例。

这是后面所有安全改动（借用检查、移动检查、生命周期、`unsafe`、`dyn`）能不能被验证的**基础设施**——没有它，后面每加一条检查都得靠人工编译确认，回归很快就会失控。

**测试框架草图**：

```rust
// tests/zr_language_negative.rs
// 每个 tests/cases/negative/*.zr 都必须编译失败，且错误信息包含期望的关键字。
// 文件名约定：<编号>_<特性>.zr；期望错误关键词放在同目录的 <编号>_<特性>.err 文件里。

#[test]
fn negative_cases() {
    for path in fs::read_dir("tests/cases/negative").unwrap() {
        let zr_path = path.unwrap().path();
        if zr_path.extension().unwrap() != "zr" { continue; }
        let err_path = {
            let mut p = zr_path.clone();
            p.set_extension("err");
            p
        };
        let expected = fs::read_to_string(err_path).unwrap();
        let output = Command::new(./target/debug/zr)
            .arg(&zr_path)
            .arg("-o").arg("/tmp/_never_written")
            .output().unwrap();
        assert!(!output.status.success(), "{} should fail", zr_path.display());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&expected), "missing error text in {}", zr_path.display());
    }
}
```

---

## 11. 一句话总结

**v0.9 修文档 + 修回归 + 补负向测试，v0.10 建安全底线（`unsafe` + 借用 + 移动 + 边界），v0.11 建类型系统（生命周期 + `dyn` + 关联类型 + HRTB），v0.12 建易用性（错误处理 + 格式化 + 标准库），v0.13 建性能（LTO / PGO / 循环优化 / DWARF），v1.0 冻结 API 并自举。** 每一步都要先有负向测试，再改代码；文档里的 ✅ 表跟着源码走，不许先标后补。
