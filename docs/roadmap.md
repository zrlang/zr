# zr 完善功能与修复规划（v0.9.0 → v1.0.0）

> 更新时间：2026-10-27
>
> 设计主线：
> - **安全线**参考 Rust：编译期挡住的尽量挡，挡不住的用**显式关键字**声明"我接受后果"，**绝不静默降级**。
> - **易用线**参考 Zig：语法短、错误信息准、格式化不用宏、格式化器强制、编译器自带格式化器、`zr test` 一键跑、`comptime` 承担一部分样板。
>
> 两条线在 `unsafe` / `comptime` / `intrinsic!` 三个关键字处交汇——它们各自管一段，不能混。

---

## 0. 现状基线

> 基线数据核对于 2026-10-27（`feat/lang-features`）。

- **语法面广、语义薄**：主流语法都解析过了，但安全模型（借用 / 移动 / 生命周期 / 边界 / `dyn` / `unsafe`）几乎全空；`trait` 求解的高级部分（关联类型 / 特化 / HRTB / 负边界）只有 AST 槽没语义；LTO / PGO / proc-macro / 自举全是文档空壳。
- **测试**：`tests/cases` 79 例 × 5 等级 = 395 项，**全绿**；`tests/negative` 15 例（该报错的必须继续报错）；`cargo test --bin zr` 265 项。`52_if_let` 与 `71_derive` 早已修好。
- **负向测试机制已有**（`tests/zr_negative.rs`，`// expect:` 子串约定），见 §10。
- **`#[derive]` 生成真实方法体**（`src/typeck/derive.rs`）：结构体 `Clone/Copy/PartialEq/Eq/PartialOrd/Ord/Hash/Default` 全支持；枚举支持 `Clone/PartialEq/Eq/Hash`（`Default`/`Ord` 显式报错）。`==`/`<` 等聚合比较改写成方法调用，**没有 derive 就报错**——不再静默恒假。
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

现状：`println!` 是函数 + `println_fmt` 走格式串。规格已经与 Rust 逐字符对齐
（`{}` `{:?}` `{:x}` `{:X}` `{:b}` `{:o}` `{:e}` `{:E}`、位置参数、填充/对齐/`+`/`#`/`0`/
宽度/精度、动态宽度 `{:1$}`、动态精度 `{:.*}` / `{:.1$}`），但**仍是运行期解析**：
占位符个数/类型不匹配不报错，命名参数 `{name}` 与 `{:#?}` 未实现。

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

### 4.6 交互式 shell（`zr shell`，阶段 L）**最后做**

> 优先级明确排在**所有其他条目之后**（自举、跨平台、benchmark 都完成后再动）。

目标：像 IPython 一样"边写边跑"——输入一段代码立刻看到结果，状态跨行保留。

```
$ zr shell
zr 0.8.0 shell — :help 查看元命令，Ctrl-D 退出
>>> let xs = [1, 2, 3];
>>> fn sum(a: []const i32) -> i32 { let mut s = 0; ... }
>>> sum(xs)
6
```

**第一版（可用为限，不做增量编译）**

- **入口**：`zr shell`。与 `zr run` / `zr build` / `zr fmt` 并列，不改动现有参数的语义；
  `--` 之后的参数原样传给被求值的程序（`args()` 用得上）。
- **累积式求值**：会话维护一份**内存里的源码缓冲**。每次提交（一个完整语句 / 条目）
  先追加到缓冲，然后
  1. 缓冲里已有的条目（`fn` / `struct` / `impl` / `use` / 常量）+ 新条目一起走完整流水线；
  2. 若最后一条是**表达式**，把它包进一个壳函数 `fn __shell_eval_N() -> i32`（或返回该
     表达式的类型），调用它并用 `println_fmt` 打印结果；若是 `let` / 条目，则只编译不调用。
- **为什么先不做增量**：编译器是"整份源码 → 全部阶段"，没有任何可复用的中间产物落盘。
  第一版按"每次重编译整个会话"实现，代码量最小、语义最容易解释正确。会话规模是几十行，
  `zr` 自编译都在秒级，交互体验可以接受。
- **已知限定（必须写进 `:help`）**：重编译会把会话**从头执行一遍**，前面积累的
  `println` / 写文件等副作用会**重复发生**。所以第一版把"有副作用的语句"也当普通语句重放，
  并在 `:help` 里明说"看到重复输出不是 bug"。真正的修法见第二版。
- **多行输入**：括号 / 大括号 / 方括号未闭合时提示符变成 `...` 继续读；空行（或 `;;`）
  结束一次提交。词法层面用现成的 `lexer` 数括号深度即可，不要自己再写一遍括号匹配。
- **错误恢复**：编译失败时把错误打印出来、**把这次输入从缓冲里撤掉**（保持会话可继续），
  并把光标停在新提示符。绝不能因为一次语法错误让整个 shell 退出。
- **元命令**（以 `:` 开头，不进入源码缓冲）：
  - `:help` / `:quit` / `:clear`（清空会话）
  - `:type <expr>` —— 只做类型检查并打印推断出的类型（走 `--dump-hir` 的同一条路径）
  - `:ast` / `:ssa` —— 打印最近一次提交的 AST / SSA（复用 `--dump-ast` / `--dump-ssa`）
  - `:opt -O0..-O3|Oz` —— 切换后续提交的优化等级
  - `:time` —— 打开/关闭"本次编译耗时"（复用 `ZR_TIME=1` 的输出）
- **零第三方依赖**：不能用 `rustyline` / `reedline`。第一版用 stdin 行缓冲 + 自己维护
  内存历史（上下键先不做）；后续若要做方向键，`termios` 的 `tcsetattr` 直接用 `libc`
  声明，不需要新依赖（`std` 侧已经有 `extern "C"` 的用法）。

**第二版（真增量，第一版跑顺之后）**

- **绑定变成会话槽位**：`let x = ...` 不再塞进一次性函数，而是提升为模块级
  `static mut __shell_x: T`，后续语句直接读写这个槽位。这样"重编译"只影响新提交，
  旧语句**不再重放**，副作用重复的问题消失；代价是 `let` 的作用域从块级变成会话级
  （与 IPython 的全局变量一致，属于用户预期内）。
- **函数级增量**：已提交的条目编译结果（目标文件 / 机器码）缓存起来，新提交只编译新增部分，
  再链接到一起。这一层需要先有 `-C incremental`（现在只有开关、无缓存实现，见 §5.1）。
- **不落盘的执行**：把生成的机器码 `mmap(MAP_ANON|PROT_EXEC)` 后直接调用，省掉
  "写临时文件 → exec" 的一次往返。**不能**用 LLVM/JIT——零依赖红线不允许。
- **补全 / 高亮**：Tab 补全关键字、标准库函数名、会话里已定义的名字（符号表现成）；
  语法高亮在行编辑层做，只处理关键字 / 字符串 / 数字三类即可。

**验收标准（第一版）**

- `zr shell` 里 `let` + 表达式 + 多行 `fn` 定义 + 编译错误恢复 四条路径都要有回归测试；
  测试用管道喂 stdin（`echo '1+1\n:quit' | zr shell`），断言 stdout，不依赖 TTY。
- 会话里 `use io::println;` 之后 `println("hi")` 能生效，说明会话源码与标准库的导入解析
  走的是同一条路径，没有第二套符号表。


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

现状（2026-10-27）：`src/typeck/derive.rs` 在 **AST 层** 生成真正的
`ast::Function`（`Type__clone` / `__eq` / `__lt` / `__le` / `__gt` / `__ge` /
`__hash` / `__default`），并登记到 `impl_targets`。生成物与手写 `impl` 的方法
同形，签名检查 / 函数体检查 / MIR 降级 / 代码生成完全无感知——不需要第二套
流水线。

- **已支持**
  - 结构体：`Clone / Copy / PartialEq / Eq / PartialOrd / Ord / Hash / Default`。
    命名字段与元组结构体都覆盖（元组结构体的字段名在 AST 里是空串，生成时补成
    `"0"` / `"1"`），嵌套结构体递归调用字段自己的方法。
  - 枚举：`Clone / PartialEq / Eq / Hash`。按变体嵌套 `match`，结构体载荷用
    `前缀_字段名` 绑定（`E::S { x, y }`）。
- **聚合比较改写**：`check_binary` 把 `Ty::Named` 上的 `==` / `!=` / `<` / `<=` /
  `>` / `>=` 改写成对应方法调用（`!=` 包一层 `Not`）。**没 derive 就报错**
  ``cannot compare values of type `S`: it does not derive or implement
  `PartialEq` ``。早先直接落到 MIR 的标量比较上——聚合值在运行期是地址，
  内容相同的两个值比较的是两个不同地址，`==` 恒假、`!=` 恒真，编译期一声不响。
- **显式拒绝**（不再"只登记标记"糊过去）
  - 泛型类型上的 derive（方法体里字段的具体类型要等单态化）；
  - 枚举的 `Default`（zr 没有 `#[default]` 变体属性）；
  - 枚举的 `PartialOrd` / `Ord`（要按判别式排序，判别式可以是显式且非单调的）。
  三种都报错，负向用例见 `tests/negative/13`–`15`。

**剩下的**（P2，优先级低于安全线）：

- **`Debug`**：仍是标记 trait，`{:?}` 不经过它。要等 `std/fmt.zr` 里的
  `Formatter` 与字符串拼接齐备，否则生成的 `__debug` 无法拼出格式串。
- **字段级诊断**：某字段类型本身没实现所需 trait 时，现在报的是整个类型
  `__eq` 的类型检查失败；应指向**那个字段**并说清缺什么。
- **`Hash` 的分布**：`str` / 切片 / 数组 / 元组 / 指针字段目前按 `0` 计入，
  只保证"相等 ⇒ 同哈希"。等这些类型有按内容比较的语义后再替换成真实哈希。

**回归**：`tests/cases/78_derive_methods.zr`（全 5 档）+ 负向 13/14/15。

---

## 7. 阶段路线图

### v0.9.0 — 修复 + 文档对齐（**已完成**）

- ✅ `52_if_let` / `71_derive` 回归修好：`if let` 的分支发散按 `Block` 判。
- ✅ `subtype().ok()` 静默吞错：`check_index_scalar` 等关键路径不再吞（`(*p)[i]` 现在编译报错）。
- ✅ `#[derive]` 生成真实方法体（§6），`71_derive` / `78_derive_methods` 全档通过。
- ✅ 负向测试框架 `tests/negative/` + `tests/zr_negative.rs`（`// expect:` 子串约定，见 §10）。
- ✅ turbofish 三种落点补齐（无载荷变体 / 关联函数 / 结构体字面量），见 `tests/cases/82_turbofish.zr`。
- ✅ `docs/rust-parity.md` / `AGENTS.md` / `tests/README.md` 与源码对齐。
- ⏳ `--emit-llvm` 仍是手写伪 IR（决定见 §8 第 5 条：删，但尚未动手）。
- ⏳ `CONTRIBUTING.md` 的"文档红线"一节待补。

### v0.10.0 — 安全基础（3 周）

- `unsafe` 关键字 + 作用域语义（§3.1）。
- 借用区间图 + 冲突检测（§2.1）。
- 移动记账 + use-after-move 检测（§2.2）。
- 切片 / 数组边界检查默认开启（`-C debug-assertions=yes`）。
- ✅ `#[derive]` 支持 `PartialEq / Eq / Hash / Default`（v0.9.0 已提前完成，见 §6）。
- `HashMap` / `HashSet` 进 `std/collections.zr`。
- **目标**：`tests/cases` 全绿，新加 `tests/cases/unsafe_*` / `borrow_*` / `move_*` 三个系列。

### v0.11.0 — 类型系统（3 周）

- 生命周期 `'a` 保留 + 子类型判定（§2.3）。
- `dyn Trait` + vtable（§3.2）。
- 关联类型解析 + 求解（`HirAssocType` 从空结构体升级）。
- `impl Trait` 在参数位置（APIT）真作为泛型参数处理。
- `for<'a>` 高阶边界保留 + 求解。
- **目标**：`std/error` 能导入。

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

### v1.1.0 — 交互式 shell（最后）

- `zr shell` 第一版：累积式求值 + 多行输入 + 错误恢复 + `:help/:type/:ast/:ssa/:opt/:time`（§4.6）。
- 第二版（增量编译 + `mmap` 执行 + Tab 补全）要等 `-C incremental` 先落地。

**排期原则**：这一条**故意放在最后**。它依赖的东西比看起来多——`zr run` 的完整流水线、
可复用的局部编译、稳定的符号解析。前面任何一项没做完，shell 都只能做成"每次重编译整个
会话"的玩具；而那正是第一版要交付的东西，所以先让第一版跑起来，再逐项替换。

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

## 10. 负向测试（已落地，继续按此约定加）

**先修测试，再改语言。**

`tests/cases/` 下每个 `.zr` 都必须编译通过并返回 0，放不下"应该失败"的用例。
负向用例在 `tests/negative/`，由 `tests/zr_negative.rs` 跑：每个文件必须**编译
失败**，且 stderr 里必须包含文件内 `// expect: <子串>` 标注的文本（可以标多行，
没有标注时只要求"确实报错"）。

约定：

- 用**子串**而不是精确匹配。报错文案里的行号 / 列号 / 字节偏移会随无关改动漂移，
  精确匹配会让整个套件变成"改一个字都红"；子串只钉语义。
- 一个用例只钉**一个**错误，文件名 `<编号>_<特性>.zr`，头注释写清"早先怎么错、
  现在为什么必须报"。
- 新增检查（借用 / 移动 / 生命周期 / `unsafe` / `dyn`）时，**先**加这个检查该拒绝
  的负向用例，再改代码。没有负向用例的检查等于没有检查。

```sh
cargo test --test zr_negative     # 目前 15 个用例
```

---

## 11. 一句话总结

**v0.9 修文档 + 修回归 + 补负向测试，v0.10 建安全底线（`unsafe` + 借用 + 移动 + 边界），v0.11 建类型系统（生命周期 + `dyn` + 关联类型 + HRTB），v0.12 建易用性（错误处理 + 格式化 + 标准库），v0.13 建性能（LTO / PGO / 循环优化 / DWARF），v1.0 冻结 API 并自举。** 每一步都要先有负向测试，再改代码；文档里的 ✅ 表跟着源码走，不许先标后补。
