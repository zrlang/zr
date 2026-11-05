# zr 与 Rust 的功能对照表

> 本表用于快速回答一个问题：**zr 现在能做什么？和 Rust 比差在哪？哪些是故意不一样的？**
>
> zr 是自己的语言设计（借鉴 Rust / Zig），**目标不是成为 Rust 的克隆**。
> 因此表里把状态分成三类：
>
> | 标记 | 含义 |
> |------|------|
> | ✅ | 已实现，语义与 Rust 一致（可放心按 Rust 的直觉写） |
> | 🟡 | 已实现，但**语义有意不同**或**只在受限场景下可用**，不能按 Rust 的直觉写 |
> | ❌ | 未实现：编译期明确报错，或功能缺失/被静默跳过 |
>
> **审计方式**：每一条都在 macOS / x86_64 上用 `cargo build` 出来的 `zr`
> 实际编译运行过（`zr --no-incremental <file>.zr -o <bin>`），表里的报错原文
> 均取自编译器实际输出。本表只覆盖语言与标准库，不含内联汇编、SIMD、
> 交叉编译与打包工具。

---

## 一、类型

### 1.1 标量类型

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `i8` / `i16` / `i32` / `i64` | 全部可用 | ✅ | 字面量后缀（`42u64`）、类型推导、`as` 截断转换都与 Rust 一致；实参位置允许隐式拓宽（`i32` → `i64`） |
| `u8` / `u16` / `u32` | 全部可用 | ✅ | 字面量后缀、推导、`as` 截断转换、比较 / 移位 / 除法均按无符号处理 |
| `u64` | 全部可用 | ✅ | 比较用 `setb`/`seta`、右移用逻辑右移、除法用 `div`（含 ≥ 2⁶³ 的值）；有符号仍走 `setl`/`sar`/`idiv` |
| `i128` / `u128` | 64 位目标可用 | 🟡 | 拆成两个连续 8 字节栈槽（lo / hi）：加减走 `adc`/`sbb`、移位走 `shld`/`shrd`、比较按高位优先定序，传参 / 返回用 `rdx:rax`；字面量 / 拷贝 / 转换可用；**乘 / 除 / 取模也已实现**（乘拆四段 64 位部分积，除 / 取模走 128 轮软件长除，有符号商向零截断、余数符号跟随被除数）；32 位目标仍禁用。回归 `tests/cases/111_i128.zr`、`tests/cases/113_i128_muldiv.zr` |
| `isize` / `usize` | 可用 | ✅ | 宽度等于目标指针宽度 |
| `f32` / `f64` | 可用 | ✅ | 算术、比较、字面量后缀（`3.14f32`）正常；实参位置允许 `f32` / 整数 → `f64` 的隐式拓宽，反向窄化需 `as` |
| `f8` / `f16` | 可用（zr 自定义格式） | ✅ | 窄浮点按 IEEE 754 binary16 / zr 自定义 8 位格式实现；传给 `f32` / `f64` 形参时会自动拓宽 |
| `bool` | 可用 | ✅ | 只有 `true` / `false`，没有 `bool` 的整数转换 |
| `char` | 可用 | ✅ | 4 字节 Unicode 标量，`'A'`、`'\n'` 等转义正常 |
| `str` | 可用 | 🟡 | zr 的 `str` 是 `{ptr: *const u8, len: usize}` 结构体，**不是 Rust 的胖指针**；`&str` 与 `str` 不是同一个类型 |
| `()` | 可用 | ✅ | `let u: () = ();` |
| `!`（never） | 可用 | 🟡 | 只能作返回类型标注；`fn f() -> !` 可用，但 `panic` 不在 prelude，需 `use core::panic;` |
| `void` | 可用（Zig 风格） | 🟡 | zr 特有类型，Rust 无对应物 |
| `anyerror` / `anytype` | 可用（Zig 风格） | 🟡 | zr 特有 |
| `T!E`（错误联合） | 类型能写，运算符不能 | 🟡 | `fn f() -> i32!i32` 能编译，但 `f()!` 解包运算符解析失败 |

### 1.2 复合类型

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `[T; N]` 定长数组 | 可用 | ✅ | 字面量、`[v; N]` 重复、索引读写、传参、逐元素算术、数组按值拷贝语义均正常 |
| `&[T]` / `&mut [T]` 切片 | `[]const T` / `[]mut T` | ✅ | 元素类型写在括号**后面**（zr 独有写法）。可变切片可写回原数组；任何数组都能隐式转成 `[]mut T`（借用检查只有最小切片，见 §2.4） |
| 数组 → 切片 强转 | 自动强转 | ✅ | `let s: []const i32 = arr;`、`f(arr)`（形参 `[]T`）都自动补 `(ptr, len)`；`arr[1..3]` / `arr[1..]` / `arr[..3]` / `arr[..]` / `arr[1..=3]` 均产生切片 |
| `(T, U)` 元组 | 可用 | 🟡 | 直接写元组类型可用；但**经类型别名后取 `.0` 失败**：``zr: no field `0` on type `type#75` `` |
| `struct` | 可用 | ✅ | 具名字段、元组结构体、嵌套字段、`impl` 方法、`impl Drop`、字段按值拷贝均正常 |
| `enum`（带载荷） | 可用 | ✅ | 判别式与载荷正常（含 `f64` 载荷、struct 载荷变体）；泛型枚举按实例单态化，`Option<T>` / `Result<T, E>` 的载荷也能正确读写 |
| `union` | 可用 | ✅ | `union U { i: i32, f: f32 }` 可构造（`U { i: 42 }`，恰好一个字段）、字段读/写、类型双关（写 f32 读 bytes）、按值传参 / 返回均正常 |
| `*const T` / `*mut T` | 可用 | ✅ | 裸指针可用；`*p = v` 走指针语义 |
| `&T` / `&mut T` | 可用 | 🟡 | 语义上 `&T` 是只读指针（``zr: cannot assign through an immutable pointer (use `&mut` and a `*mut` type) ``），`&mut T` 可写；但**`let` 注解里的 `&T` 被解析成 `*const T`**：`let r: &i32 = &x;` → ``zr: type mismatch: expected `*const i32`, found `&i32` `` |
| `Option<T>` / `Result<T, E>` | 可用 | ✅ | 在 `std::core` 里；`is_some()` / `is_none()` / `is_ok()` / `is_err()` / `unwrap()` / `unwrap_or()` / `map()`、模式匹配与后缀 `?` 均正常 |
| 泛型函数 | 可用 | ✅ | 按调用点单态化（含递归泛型）；类型实参从实参 / 期望返回类型推断。泛型约束（`T: Ord`、`where`）在**单态化时**校验（`check_trait_bounds`，含超类型展开）；实例化后再按具体类型检查方法调用 |
| 泛型类型 | 可用 | ✅ | 泛型结构体 / 枚举 + `impl<T> Type<T>` 的方法均可用（`Vec<T>`、`Option<T>` 等）；字段与载荷类型按类型实参替换 |
| `trait` | 可用 | ✅ | `trait` + `impl Trait for Type` 正常（`std::core` 的 `Eq`/`Ord`/`Copy`/`Clone`/`Display` 都在用）。**trait 默认方法已实现**（`124_trait_default_methods.zr`）：trait 里带方法体的方法（含 `default fn` 拼写）在 impl 未覆盖时按目标类型合成；没有默认体的方法缺失在 impl 块报 `not all trait items implemented`（负向 `37`）。局限：泛型 impl 的毯覆方法解析与"具体 impl 部分覆盖、其余继承毯覆默认体"的两级特化未做 |
| 关联类型 | 可用 | ✅ | `type Output;` + `type Output = i32;` 正常；`Self::Output`（impl 内）、`F::Output`（`F: Trait` 泛型参数）、`Type::Output`（具体类型）均可投影，含泛型 impl `impl<T> Tr for S<T> { type Item = T; }`；**限定写法 `<T as Trait>::Item` 已支持**（`119_qualified_assoc.zr`），相对 `F::Output` 多出的是 trait 校验（没实现就报 `the trait `X` is not implemented for `Y``，负向 `29`）。**同名关联类型消歧已支持**（`163_assoc_disambig.zr`）：`<Both as HasA>::Item` 与 `<Both as HasB>::Item` 按限定路径里的 trait 过滤 impl 表，不再取先收集者 |
| `impl` 块 | 可用 | ✅ | 关联函数、`self` / `&self` / `&mut self` 三种接收者、`Self` 类型均可 |
| `type` 别名 | 可用（含泛型） | ✅ | 在**使用点展开**：`let x: MyInt`、结构体字段、数组 / 指针、`Vec<MyInt>`、别名套别名、前向引用都正常；**泛型别名 `type V<T> = Vec<T>` 已支持**（`74_generic_alias.zr`，提交 `8d6c475`：保留参数并登记占位符，展平按实参替换） |
| 生命周期标注 `'a` | 解析后丢弃 | 🟡 | `&'a T`、`&'a mut T`、`&'static dyn Tomb`、`struct S<'a>`、`impl<'a> S<'a>`、`S<'a>` / `S<'_>` 都能解析（生命周期实参在 `parse_generic_args` 里跳过、不登记，见 `115_lifetime_args.zr`），类型检查阶段直接丢掉标注（`src/parser/ty.rs`），**没有生命周期 / 存活检查**（借用检查只是 §2.4 的最小切片）。结构体 `where` 子句与 `as *const _` 推断占位已补上。**仍不支持**：`'_` 作为**必须**的实参（会被跳过，`Foo<'_>` 等于 `Foo`） |
| `dyn Trait` / trait 对象 | 可用（x86-64 MVP） | ✅ | `Ty::Dyn` 是 16 字节值类型 `{data, vtable}`，`&dyn Trait` 是指向该记录的指针；`let d: dyn Tr = &x;` 由 `coerce_to_dyn` 触发（源类型须实现 `Tr`）。vtable 符号 `__vt_<Type>_<Trait>`，槽位 = trait 方法声明顺序，默认方法按类型合成进表；调用经 `ExprKind::DynCall` 降级为间接调用（槽宽与 MIR `VT_ENTRY_BYTES` 同步，见 AGENTS.md 红线 14）；聚合返回追加 sret 槽。MVP 边界：仅 x86-64（其它后端 `compile` 入口报错）、目标类型限非泛型具名类型/基元、vtable 无 drop/size/align 元数据。回归 `161_dyn_dispatch.zr` / `162_dyn_std_error.zr` / `169_box_dyn.zr`（`Box<dyn Wake>` 两种接收者、`Box::into_raw`/`from_raw`）；负向 `49` / `50` |
| auto trait | 不支持 | ❌ | 无 auto trait 机制、无 blanket impl 语法 |
| `PhantomData<T>` | 不存在 | ❌ | `std::core` 里没有 `PhantomData`，也没有等价物 |

### 1.3 泛型实参在类型注解里的写法

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `let x: Option<i32> = ...;` | 可用 | ✅ | 类型注解里的泛型实参保留，字段 / 载荷类型按实参替换 |
| `fn f(x: Option<i32>)` | 可用 | ✅ | 签名与函数体一致 |
| `Option::<i32>::None` | 可用 | ✅ | turbofish 后面继续吃路径段，类型实参挂在整条路径上：无载荷变体（`Option::<i32>::None`）、关联函数（`Box::<i32>::new(7)`）、元组结构体（`Wrap::<i32>(7)`）、结构体载荷变体（`Shape::<u8>::Circle { r: 7 }`）都可用。显式实参当期望类型喂给构造器；无上下文的 `Option::None` 仍需 `let` 注解 |
| `Vec<Vec<i32>>`（嵌套泛型，`>>` 收尾） | 可用 | ✅ | 词法器把 `>>` / `<<` 切成 `Shr` / `Shl`（为移位表达式），类型位置原先解析不了嵌套泛型。现在 `Parser::split_angle` 只在类型语境内把 `Shl`→`Lt Lt`、`Shr`→`Gt Gt`，`Vec<Vec<i32>>`、`Option<<T as Tr>::Item>` 都能解析（`119_qualified_assoc.zr`） |
| `struct Arr<const N: usize> { data: [i32; N] }` | 可用 | ✅ | `const` 参数进类型作用域（占位 `Ty::ConstParam`），数组长度记 `ArrayLen::Param(N)`，单态化时按名字换成具体值。声明 / 注解（`Arr<3>`）/ turbofish（`f::<4>`）/ 从实参数组长度反推都可用；`impl<const N: usize> Arr<N>` 的方法、`[0; N]` 重复、`N` 在值位置（`while i < N`）也可用（`122_const_generic.zr`，负向 `32`/`33`/`34`）。**长度表达式已实现**（`167_const_len_expr.zr`）：`[T; N + 1]` 返回类型、`[0; N + 1]` 重复、`[i32; 2 * N]` / `[i32; N - 1]` 加减乘、结构体字段类型里的长度表达式、数组 `.len` 发射编译期常量。**仍缺**：多参数长度表达式（`[T; N + M]`，长度表达式按编译期常量求值、只认 `ConstParamRef` 换值的单参数形式）与省略实参时的类型侧推断（结构体字面量只能从字段数组字面量长度反推） |

---

## 二、流程控制

### 2.1 条件与循环

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `if` / `else` 语句 | 可用 | ✅ | |
| `if` 表达式 | 可用 | ✅ | `let m = if a > b { a } else { b };` |
| `if let` | 可用 | ✅ | 本地枚举与导入枚举（`use core::Option;`）都能用 |
| `let ... else` | 可用 | ✅ | `let Step::Value(v) = s else { return 1; };`；else 必须发散 |
| `let` 解构 `let (a, b) = ...` | 可用 | ✅ | 元组 / 结构体 / 定长数组切片都能解构，子绑定逐个初始化；可反驳模式在 `let` 位置报 `refutable pattern in local binding; use \`let ... else\``。**函数形参上的解构模式仍只取一个 local** |
| `while let` | 可用 | ✅ | 脱糖成 `loop { let tmp = EXPR; if let PAT = tmp { .. } else { break; } }`，每轮只求值一次 |
| `loop` / `while` / `for` | 可用 | ✅ | `for i in 0..10`、`for i in 1..=10`、`for x in array` 均正常；**`for x in it`（Iterator 协议）已实现**（`165_iterator.zr`）：`it.next() -> *mut Item` 脱糖成 `loop { let p = it.next(); if p == 0 { break; } let x = *p; body }`，`Range`（含 `inclusive` / `range_step` 步长）与 `SliceIter`（数组值 / 数组引用）可用 |
| `break` 带值 | 可用 | ✅ | `let v = loop { break 42; };` |
| 带标签的 `break` / `continue` | 可用 | ✅ | 同时支持 Zig 风格 `outer: loop { break outer; }` 与 Rust 风格 `'outer: loop { break 'outer; }`；标签属于 `loop` / `while` / `for` |

### 2.2 模式匹配

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| 字面量臂 | 可用 | ✅ | 整数字面量、`bool`、`char` 可用；**浮点 / 字符串字面量模式报错**：`zr: match on a float / string pattern is not supported (only integer / char / bool literals)` |
| 绑定臂 | 可用 | ✅ | |
| 通配臂 `_` | 可用 | ✅ | |
| 或模式 `1 \| 2 \| 3` | 可用 | ✅ | |
| 范围模式 `1..=10` / `1..10` | 可用 | ✅ | |
| 结构体模式 `P { x: 1, y: v }` | 可用 | ✅ | 含 `..` 尾与嵌套 |
| 元组模式 `(1, v)` | 可用 | ✅ | |
| 枚举变体模式 `Shape::Circle(r)` | 可用 | ✅ | 判别式比较正确（`Shape::Empty` 能被正确排除） |
| 守卫 `n if n > 100` | 可用 | ✅ | |
| 穷尽性检查 | 可用 | ✅ | 枚举全覆盖时可以**不写 `_`**（与 Rust 一致）；缺臂时报错**列出缺失的变体名**（``match is not exhaustive: `Color` variant not covered: Blue``，`17_match_missing_variant.zr`）；整型 / 标量仍保留尾 `_` 臂措辞 |
| 切片模式 `[a, b, c]` | 定长可用 | 🟡 | 按**长度 + 逐下标**匹配：定长数组的长度是编译期常量（元素个数不符报 `slice pattern has N element(s), but array has M`），切片 / `str` 生成运行期 `.len == N` 条件；元素模式落在 `scrut[i]`。`let` 位置出现切片模式视为可反驳（`let [a, b] = xs else { .. };`）。**仍不支持 `..` 剩余模式**（`[first, .., last]` 的 `..` 在切片模式里不解析） |
| 引用模式 `&v` | 不支持 | ❌ | 解析期就失败：`zr: expected pattern, found Amp`（类型检查器里还有一条 `zr: match on reference patterns is not implemented yet` 但到不了） |

### 2.3 函数与表达式

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| 块表达式 | 可用 | ✅ | `let v = { let a = 1; a + 1 };` |
| 函数指针 `fn(T) -> R` | 可用 | ✅ | 取址、传参、返回值、存进结构体字段都行（x86-64 专属，其它后端明确报错） |
| 闭包 `\|x\| x * factor` | 基本可用 | 🟡 | 无捕获闭包可存变量 / 当实参 / 返回；带捕获闭包也可存变量 / 嵌套 / 多次调用、按值传给**泛型**形参 / 从泛型返回值透传（`84_closure_capture_value.zr`、`109_closure_env_cross_fn.zr`、`110_closure_env_generic_passthrough.zr`）。**带捕获闭包也能转成裸 `fn(..)` 值**（传给 `fn(..)` 形参 / 作 `fn(..)` 返回值 / `let f: fn(..) = |..|`）：函数值 = 16 字节记录 `{code, env}`，调用统一多传一个 env（`125_closure_to_fn.zr`）；泛型 / 异步函数当值明确报错（负向 `38` / `39`） |
| `impl Trait` 参数位置（APIT） | 可用 | ✅ | `fn f(x: impl Trait)` 在声明收集前脱糖成匿名泛型参数（`fn f<__apit_1: Trait>(x: __apit_1)`），按实参单态化、边界真的被校验（`120_apit.zr`，负向 `30_apit_bound.zr`）。`&impl Trait` 绑**内层**类型（`&T`），`Vec<impl Trait>` 是 `Vec<T>`，支持 `impl A + B` 多边界、与显式泛型参数共存。**trait 声明方法签名里的 APIT 也已脱糖**（`170_trait_apit.zr`：带 `&self` 接收者 / 默认方法 / 关联函数三种形态，参数与 impl 方法一致） |
| `impl Trait` 返回值（RPIT） | 可用 | ✅ | `-> impl Trait` 是真存在类型 `opaque(impl_trait_N)`：体内先以 `Infer` 检查、返回后把隐藏类型记进 `opaque_hidden`，**字段访问（`r.x`）与方法访问（`r.area()`）都按隐藏类型解析**、MIR 布局按隐藏类型传/返回（`167_rpit_field.zr`；负向 `47` 未实现边界 / `48` 无返回值）。**仍不支持**：`impl Fn(T) -> T`（带括号的 trait 路径，无 `Fn` trait 语法）；trait 方法签名里的 RPIT |
| `?` 运算符 | 可用 | ✅ | 对 `Result<T, E>` / `Option<T>` 生效：成功取载荷，失败从当前函数 `return Err(e)` / `return None`。返回类型必须是同一种枚举，错误类型必须一致（没有 `From` 转换） |
| `try` / `catch` | 明确报错 | ❌ | 语法能解析，类型检查报 `` `try` / `catch` is not implemented yet; use the postfix `?` operator ... `` |
| `throw` | 明确报错 | ❌ | 类型检查报 `` `throw` is not implemented yet; return `Err(...)` instead `` |
| `defer` | 明确报错 | ❌ | 类型检查报 `` `defer` is not implemented yet (the block would never run) ... `` |
| `async fn` / `.await` | 可用 | ✅ | Rust 风格受限降级：future 帧在堆上，`.await` 轮询；`async fn main` 入口执行器可用（实测 `count(5).await` → 10，`double(10).await` → 20） |
| `comptime` | 可用 | ✅ | `let v = comptime 1 + 2;` → 3 |
| `unsafe { }` | 骨架可用 | 🟡 | `unsafe { ... }` 块可解析、可通过类型检查（`85_unsafe.zr`）；但**当前不改变语义**（按块表达式透传），并非 Rust 的 unsafe 保障 |

### 2.4 所有权 / 借用

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| 移动语义（use after move 报错） | 最小版已实现 | 🟡 | `let b = a` 对含 drop glue 的类型是移动，之后读 `a` 报 use-after-move（`87_move_check.zr`、负向 `09_use_after_move.zr`）；**字段级 / 按值传参的移动不覆盖** |
| 借用检查（`&mut` 独占） | 最小切片已实现 | 🟡 | 同位置借用重叠（`&mut` 与任意、`&` 与 `&mut`）报 `cannot borrow ... while already borrowed`；写被借用位置报 `cannot assign ... while it is borrowed`；读被可变借用位置报 `cannot read ... while it is mutably borrowed`；NLL 按 last-use 近似（`105_borrow_nll.zr` + 负向 20/21/22）。**多绑定 / 未关联借用、跨函数流动不检查** |
| `let` 默认不可变 | 一致 | ✅ | `let x = 1; x = 2;` → ``zr: cannot assign to immutable binding (declare it with `let mut`) `` |
| `var` | 已移除 | ✅ | `var x = 1;` → 解析错误（zr 用 `let` / `let mut`） |

### 2.5 模块与属性

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `mod` / `use` / `use ... as ...` | 可用 | ✅ | `mod`、`use`、`use a::{b, c}` 分组导入、`use x as y` 重命名（`75_use_alias.zr`）均可 |
| `use foo::*` | 支持 | ✅ | glob 导入按模块的导出名登记别名（`40b5b45`，`90_glob_import.zr`） |
| `use std::core::Option` | 路径不带 `std::` | 🟡 | zr 的标准库模块直接叫 `core` / `io` / `string` / `collections`：`use core::Option;`。写 `std::core::Option` → ``zr: unknown module `std` `` |
| `pub` / `pub(crate)` / `pub(super)` | 可用 | ✅ | 解析阶段保留可见性，类型检查按「条目可见性 × 引用方模块」判定（`e0c5541`）：`pub` / `pub(crate)` 单 crate 内处处可见，`pub(super)` 对父模块及后代可见，私有 / `pub(self)` 对声明模块及后代可见（负向 `23_visibility_private.zr` / `24_visibility_super.zr`）。局限：`use` 点导入私有条目未检查 |
| `#[derive(Clone, PartialEq, Eq, Hash, Default, Debug, ...)]` | 生成真实方法体 | 🟡 | `src/typeck/derive.rs` 在 AST 层生成 `Type__clone` / `__eq` / `__lt` / `__hash` / `__default` / `__debug`，与手写 `impl` 方法同形。结构体支持 `Clone/Copy/PartialEq/Eq/PartialOrd/Ord/Hash/Default/Debug`（含元组结构体、嵌套结构体、**`str` 字段后的嵌套结构体字段**）；枚举支持 `Clone/PartialEq/Eq/Hash/Debug/PartialOrd/Ord/Default`（`Default` 需 `#[default]` 标出恰好一个变体，`Ord` 对无显式判别式枚举按声明顺序）。`==`/`<` 等聚合比较改写成方法调用，**没 derive 就报错**。`Debug` 生成 `.debug() -> String`（Rust `{:?}` 风格）与多行 `.debug_pretty()`（Rust `{:#?}` 风格，`13acff2`；浮点/指针/数组/元组/`String` 字段显式报错）。仍缺：泛型类型 derive 报错、带显式判别式的枚举 `Ord` 报错；**格式引擎 `{:#?}` 尚未接自定义类型**。 |
| `#[repr(C)]` / `#[repr(u8)]` | 可用 | ✅ | |
| `#[inline(...)]` / `#[link(...)]` / `#[test]` | 可用 | ✅ | `#[test]` 的函数会被编进产物并在 `main` 前调用 |
| `static` / `static mut` | 标量可用 | ✅ | 初值能折成字面量的**标量**静态量会写进 `.data`：读、写、复合赋值、`&STATIC` / `&mut STATIC` 都正常。聚合静态量（结构体 / 数组 / 枚举）暂不生成数据，读取时明确报错；`static mut` 赋值不需要 `let mut`，非 `mut` 静态量赋值报错 |
| `extern "C" { ... }` 块 | 可用 | ✅ | `extern "C" fn ...`（Rust 风格的带体定义）不支持：`zr: expected '{' after extern: expected LBrace, found KwFn` |

---

## 三、文件与 IO

### 3.1 标准输出 / 错误输出

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `println!("...")` 宏 | `println(s: str)` 普通函数 | 🟡 | zr 的 `println` 是**函数不是宏**：`println!("{}", 1)` 解析失败（`zr: expected '}' to close block: expected RBrace, found Bang`）。参数类型现在会检查：`println(42)` 报 `type mismatch: expected `str`, found `i32`` |
| `print!` / `println!` 格式化 `{}` | `println_fmt(fmt, &[FormatArg], n)` | 🟡 | 支持 `{}`、`{:?}`、`{:x}`、`{:X}`、`{:b}`、`{:o}`、`{:e}`、`{:E}`、位置参数 `{0}`、`{{`/`}}` 转义，以及填充 / 对齐（`<` `>` `^`）、`+`、`#`、`0`、宽度、精度（`{:08.2}`、`{:*^10.3}`）、动态宽度 `{:1$}`、动态精度 `{:.*}` / `{:.1$}`；与 Rust 逐字符对比一致。命名参数走 `println_fmt_named(fmt, &[NamedArg], n)`；自定义类型的 `{:?}` / `{:#?}` 走 `io::fmt_debug(&v as *const void, Type__debug_fmt)`（`FormatArg` 的 `kind = 7` 类型化值通道）。**标量**的 `{:#?}` 仍只输出一行（`#` 对标量被忽略） |
| `Display` 浮点格式 | 最短可回读十进制 | ✅ | `1.0`→`1`、`3.5`→`3.5`、`1.0/3.0`→`0.3333333333333333`、`0.1+0.2`→`0.30000000000000004`、`-0.0`→`-0`、`1e30`→`1000000000000000000000000000000`，与 Rust 一致（早先 ≥2⁶⁴ 会打成 `.0`，已修） |
| `eprintln!` / `eprint!` | `eprintln(s)` / `eprint(s)` | ✅ | 写到 STDERR |
| `println!("{:?}", x)` 结构体调试 | `fmt_debug(&v as *const void, Type__debug_fmt)` | 🟡 | `#[derive(Debug)]` 生成 `.debug()` / `.debug_pretty()` 与格式引擎入口 `Type__debug_fmt`；`println_fmt("… {:?} …", &[fmt_debug(..)], n)` 单行、`{:#?}` 多行，与 Rust 逐字符一致。**未覆盖**：泛型类型上的 derive、`{:#?}` 对浮点/指针/数组/元组字段（derive 侧显式报错） |

### 3.2 标准输入

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `BufRead::read_line(&mut String)` | `stdin_read_line(buf, cap) -> usize` | 🟡 | 读的是**裸字节**（不含 `\n`），返回读到的字节数；没有 `String` 版本的行读取 |
| `Read::read(&mut [u8])` | `stdin_read_bytes(buf, cap) -> usize` | 🟡 | 返回字节数，EOF 返回 0。名字带 `stdin_` 前缀是为了不和 `fs::read_bytes(path)` 在扁平符号表里撞名 |

### 3.3 文件

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `File::open` / `OpenOptions` | `File::open(path, mode)` | 🟡 | mode 是整数常量（`FILE_READ` / `FILE_WRITE` / `FILE_APPEND`）；`is_ok()` 判断成败（没有 `Result`） |
| `read` / `read_to_end` / `read_to_string` | `read(buf, cap)` / `file_read_all` / `File::read_to_string()` / `fs::read_to_string(path)` / `fs::read_bytes(path)` | ✅ | `File::read_to_string` 用 4 KB 栈缓冲循环读到 EOF 拼成 `String`；路径版返回 `Option`，文件打不开给 `None` |
| `write` / `write_all` | `write(bytes)` / `write_all(bytes)` | ✅ | |
| `seek` | `seek(offset, whence)` | 🟡 | `SEEK_SET` / `SEEK_CUR` / `SEEK_END`，返回新位置 |
| `close` | `close() -> bool` | 🟡 | **显式关闭**，`File` 不实现 `Drop`，不会自动关闭 |
| `metadata().len()` | `fs::file_size` / `file_mode` / `file_mtime` / `file_ctime` | 🟡 | 路径版元数据已补（大小 / mode / mtime / ctime）；**没有 `File` 上的 `metadata()`，也没有 inode** |
| `is_terminal` | `is_terminal()` | ✅ | |
| 目录操作（`create_dir` / `create_dir_all` / `read_dir` / `remove_dir`） | `fs::create_dir` / `create_dir_all` / `read_dir`（`Vec<String>`）/ `read_dir_count` + `read_dir_name` / `remove_dir` | ✅ | `create_dir_all` 逐段 `mkdir`、已存在视为成功；目录遍历跳过 `.` 与 `..`；`read_dir_name` 写进调用方缓冲区。回归 `tests/cases/151_std_fs_time.zr` |
| 一次性读写（`fs::read_to_string` / `fs::write` / `fs::copy` / `fs::append`） | `fs::read_to_string` / `read_bytes` / `write_string` / `append_string` / `copy_file` | ✅ | 文件名 `copy_file` 而不叫 `copy`，避免和 `slice::copy` 撞名 |
| 路径操作（`Path` / `join` / `exists`） | `path::join` / `normalize` / `basename` / `extname` / `substr` / `fs::exists` | 🟡 | 没有 `Path` 类型，直接在 `str` 上算；`substr` / `basename` / `extname` 直接返回 `str` 切片 |
| `Instant` / `SystemTime` / `Duration` / `sleep` | `time::Instant`（`now` / `elapsed` / `as_nanos`）、`time::Duration`（`from_secs` / `from_millis` / `from_micros` / `from_nanos` / `as_*` / `subsec_nanos` / `is_zero`）、`time::sleep(Duration)`；底层 `zr_time_ns` / `zr_time_unix_ns` / `zr_sleep_ms` | ✅ | 墙钟仍是整数纳秒（`unix_ns` / `unix_secs` / `now_ms`），`DateTime` 负责日历换算与 `format_iso` / `parse_iso`；`sleep` 精度毫秒。回归 `tests/cases/151_std_fs_time.zr` |
| 临时文件（`tempfile`） | `fs::temp_file()` / `temp_file_raw()` | 🟡 | `/tmp/zr_tmp_<纳秒>`；没有 `NamedTempFile` / 自动删除（`File` 不实现 `Drop`） |

### 3.4 进程参数 / 环境变量 / 配置格式

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `std::env::args()` | `argparse::arg_count()` / `argparse::arg(i)` / `argparse::arg_opt(i)` / `argparse::args()` / `argparse::get(i, default)` / `argparse::program_name()`；按名字读 `argparse::value(name)` / `value_or(name, default)` / `has(name)` / `flag(name)` / `positionals()` | ✅ | 入口桩在 `main` 前捕获 `argc/argv`（Linux 读 `[rsp]`、macOS 由 dyld 传入、Windows 用 `GetCommandLineA`），存进运行时 `zr_args_*`；`arg` 返回 `String`（复制），`arg_opt` 越界给 `None`，`arg` / `get` 越界分别给空串 / 默认值。**没有迭代器**，`args()` 一次性收集成 `Vec<String>`。名字版支持 `--name=value`、`--name value`、`-n value` 与裸 flag，另有 `value_in` / `value_or_in` / `has_in` / `flag_in` / `positionals_in` 可注入参数表便于单测。回归 `tests/cases/126_cli_args.zr`、`tests/cases/131_argparse_named.zr` |
| `std::env::var` / `set_var` / `remove_var` | `env::var(key)` / `env::var_or(key, default)` / `env::set_var` / `env::remove_var` / `env::has_var` | ✅ | `var` 返回 `Option<String>`；写操作进入 `std/std/env.zr` 的进程内覆盖表（初值来自入口 `envp` 的 `zr_env_count` / `zr_env_entry`），**不写回父进程 / 不改子进程继承块**；`remove_var` 用墓碑实现，后写优先（从后往前扫）。非法键（空串 / 含 `=`）的写操作返回假。回归 `tests/cases/127_env_vars.zr` |
| `serde_json` / `std::json`（Rust 无内置） | `json::json_parse` / `json::json_stringify` / `json::json_stringify_pretty` | ✅ | 手写递归下降：`null` / 布尔 / 整数与浮点 / 字符串（含 `\uXXXX` 与代理对）/ 数组 / 对象；值树是递归枚举 `Json`，访问器 `len` / `at` / `get` / `as_*` / `is_null`；生成分紧凑与缩进两种。错误带字节位置。回归 `tests/cases/128_json.zr` |
| XML（Rust 无内置） | `xml::xml_parse` / `xml::xml_stringify` / `xml::xml_stringify_pretty` | ✅ | 支持 XML 声明、注释、DOCTYPE、CDATA、自闭合标签、属性、五种实体与数字实体；`XmlNode` 带 `name` / `attrs` / `children` / `text`，访问器 `attr` / `child` / `child_named` / `child_count`；生成时转义（pretty 只序列化节点、不带声明）。错误带字节位置。回归 `tests/cases/129_xml.zr` |
| TOML（Rust 无内置，生态用 `toml` crate） | `toml::toml_parse` / `toml::toml_stringify` | ✅ | 支持注释、`[table]`、`[table.sub]`、`[[array-of-tables]]`、点分键、基本 / 字面字符串、整数（含 `0x` / `0o` / `0b` 与 `_` 分隔）/ 浮点 / 布尔、数组（含尾逗号）、内联表；`Toml` 递归值树与 `get` / `get_path` / `at` / `len` / `as_*` 访问器。**已知限制**：不解析日期时间、浮点最多 6 位小数、重复键后写覆盖；未闭合数组 / 缺值返回带位置的 `Err`。回归 `tests/cases/130_toml.zr` |

### 3.5 字符串格式化 / 平台 / 时间 / 编码 / 网络 / 科学计算

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `format!` / `Iterator::join` | `io::format(fmt, args, nargs) -> String` / `string::join_parts(parts, count, sep) -> String` | ✅ | `format` 把格式串与 `io::FormatArg` 拼成**新 `String`**（不写 stdout）；支持 `{}` `{n}` `{:?}` `{:#?}` `{:x}` `{:X}` `{:b}` `{:o}`、`{{` / `}}`。`join` 用分隔符连接若干 `str`。回归 `tests/cases/132_format.zr` |
| `std::env` 的 `pid` / `current_dir` / `temp_dir` / `home_dir` | `os::pid` / `os::cwd_len(buf, cap)` / `os::temp_dir` / `os::home_dir` / `os::os_name` / `os::pointer_bits` / `os::exit` | ✅ | `temp_dir` 返回 `String`，`home_dir` 返回 `Option<String>`，`os_name` 在运行期探测 `macos` / `linux` / `windows`。回归 `tests/cases/138_os_posix.zr` |
| `libc`（`isatty` / `getpid` / `kill` / `getcwd`、`SIG*`、`*_FILENO`、`wait` 状态解码） | `posix::isatty` / `getpid` / `kill` / `getcwd`、`SIG*` / `*_FILENO` 常量、`wifexited` / `wexitstatus` / `wifsignaled` / `wtermsig` | ✅ | 薄封装运行时 `zr_isatty` / `zr_process_*` 等符号，不依赖 libc 头文件。回归 `tests/cases/138_os_posix.zr` |
| `std::time::Instant` / `SystemTime` / `Duration` | `time::monotonic_ns` / `unix_ns` / `unix_secs` / `now_ms` / `sleep_ms`；`DateTime` + `civil_from_unix` / `unix_from_civil` / `is_leap` / `days_in_month` / `format_iso` / `parse_iso` | ✅ | 时间是整数纳秒 / 秒；`DateTime` 是 `{year, month, day, hour, minute, second, weekday, nanos}` 值类型，支持负秒与闰年。回归 `tests/cases/136_time.zr` |
| `chrono-tz`（Rust 生态） | `tz::zone_exists` / `utc_offset_seconds(name, unix_secs)` / `to_local` / `from_local` | ✅ | 内置 UTC、Asia/Shanghai、Asia/Tokyo、Asia/Kolkata、Europe/London、Europe/Berlin、America/New_York、America/Los_Angeles、Australia/Sydney，按真实 EU/US/AU DST 规则计算；未知时区按 UTC。回归 `tests/cases/137_tz.zr` |
| `md5` crate / `sha2` crate | `md5::md5_hex` / `md5_hex_str`；`sha256::sha256_hex` / `sha256_hex_str` | ✅ | 纯 zr 实现（RFC 1321 / FIPS 180-4），与 Python `hashlib` 0–130 字节逐值对照一致。回归 `tests/cases/134_md5.zr`、`tests/cases/135_sha256.zr` |
| `base64` crate | `base64::base64_encode` / `base64_decode`、`base64url_encode` / `base64url_decode` | ✅ | 标准与 URL-safe，带 `=` padding；严格解码（非法字符 / 长度 / 缺 padding 返回 `None`）。回归 `tests/cases/133_base64.zr` |
| `reqwest` / `http` crate | `http::url_parse` / `http_build_request` / `http_parse_response` / `http_get` / `http_post` / `http_request(method, url, body, headers, follow)`；`Url` / `HttpHeader` / `HttpResponse` / `HttpError` | 🟡 | 明文 HTTP，点分 IPv4，不支持 HTTP/2 与代理；自动跟随 301/302/303/307/308（303 强制 GET、307/308 保留方法与 body、相对 Location 拼回原 host）；`https://` 可解析但本模块返回 `TlsNotSupported`（TLS 见 `std/std/tls.zr`）。`http_get` 复用 `net::SockAddrStorage` 的 `zr_*` 套接字封装（旧版 net HTTP 客户端已删除，避免扁平符号表重名）。响应解析支持状态行、全部头、`Content-Length`、`chunked` 与无长度。回归 `tests/cases/140_http.zr` |
| `rustls` / `openssl` crate | `tls::ClientConfig` / `tls::ClientConnection`（`handshake` / `write` / `read` / `close`）、`crypto::aes128_*` / `crypto::Sha256State` / `crypto::crypto_hmac_sha256` | 🟡 | **纯 zr** TLS 1.2 客户端（零外部依赖），rustls 式分层；PSK 完整握手（0x00B2）+ AES-128-CTR/HMAC-SHA256 加密 record 层 + PRF 密钥导出；非 PSK 套件走 ClientHello/ServerHello 骨架；RSA/ECDH 证书验证留 TODO（无 u128/BigInt）。底层密码学：`crypto.zr` 纯 zr 实现 AES-128-CTR + SHA-256 流式 + HMAC-SHA256 + PRNG |
| `pdbtbx` / `bio` crate | `pdb::pdb_parse` / `Pdb::atom_count` / `atom` / `centroid` | ✅ | 解析 PDB 固定列宽的 `ATOM` / `HETATM`（含 `HEADER` / `TITLE`），坏行跳过。回归 `tests/cases/141_pdb.zr` |
| `ndarray` / `tch` / `candle` | `tensor::Tensor`（`zeros` / `ones` / `full` / `scalar` / `identity` / `from_slice` / `from_vec` / `from_matrix`；`get_flat` / `set_flat` / `get2` / `set2` / `get3` / `set3`；`add` / `sub` / `mul` / `div` / `neg` / `add_scalar` / `mul_scalar`；`relu` / `leaky_relu`；`matmul` / `transpose` / `reshape`；`sum` / `mean` / `max` / `min` / `argmax`；`square` / `mse` / `same_shape` / `close_to` / `copy` / `fill` / `destroy`；广播 `broadcast` / `broadcast_add` / `broadcast_mul`；`conv2d`；autograd `track_inplace` / `backward` / `zero_grad` / `sum_tensor`；`gpu_available` / `gpu_to` 桩） | 🟡 | `f64`、行主序、最多 4 维，值语义（无 `impl Drop`，手动 `destroy`）；广播按 NumPy 规则；`conv2d` 是直接卷积（无 im2col）且无反向；autograd 是极简反向模式（`add` / `mul` / `matmul` / `relu` / `sum_tensor`，依赖前向对象的生命周期）；GPU 只有占位接口（`gpu_available()` 恒 false、`gpu_to()` panic）；`matmul` 仅 2-D。回归 `tests/cases/142_tensor.zr`、`tests/cases/143_tensor_ai.zr`（两层 MLP 前向做 XOR）、`tests/cases/146_tensor_ext.zr` |
| 便携 SIMD（`std::simd`） | `simd::f32x4` / `i32x4` / `f64x2` / `i8x8` / `i64x2` / `f32x8` / `f64x4`：`from_array` / `to_array` / `splat` / `add` / `sub` / `mul` / `div`（浮点）/ `neg` / `min` / `max` / `abs`（浮点）/ `sum` / `dot`；`best_width()` | ✅ | 逐 lane 展开（128 位整型数组的 `+`/`-`/`&` 由后端直接出 SSE2）；`i8x8` 的 `sum` / `dot` 用 `i32` 累加。`best_width()` 按指针宽度给"推荐字节数"（64 位=32、32 位=16），不做 CPUID。回归 `tests/cases/139_simd.zr`、`tests/cases/144_simd_ext.zr` |

### 3.6 三层标准库（`core ⊂ alloc ⊂ std`）

目录与 Rust 同构：`std/core/*.zr`（零依赖）、`std/alloc/*.zr`（只依赖 core，有堆）、
`std/std/*.zr`（依赖 OS）；门面 `std/std.zr` 用 `use x::*` 把三层传递性再导出，
日常业务只 `use std::*`。依赖方向由 `Loader.layers` 强制（core 不能 import alloc/std，
alloc 不能 import std）。

| Rust 的模块 | zr 对应 | 状态 | 说明 |
|------|------|------|------|
| `core::cmp` | `cmp::Ordering`（`Less` / `Equal` / `Greater` + `is_*` / `reverse`）、`cmp::cmp` / `cmp_min` / `cmp_max` | ✅ | `mem.zr` 里的原子内存序枚举已改名 `AtomicOrdering`，避免与 `Ordering` 在扁平符号表里冲突 |
| `core::char` | `char::is_ascii_digit` / `is_ascii_alphabetic` / `is_ascii_alphanumeric` / `is_ascii_whitespace` / `is_ascii_uppercase` / `is_ascii_lowercase` / `is_ascii_graphic` / `to_ascii_lowercase` / `to_ascii_uppercase` / `ascii_to_digit` / `ascii_from_digit` | ✅ | zr 不能给内建类型写固有 `impl`（orphan rule），所以全部是**自由函数** |
| `core::num` | `num::abs_i64` / `abs_i32` / `sign_*` / `pow_*` / `saturating_*` / `checked_*` / `wrapping_*` / `count_ones_u64` / `leading_zeros_u64` / `trailing_zeros_u64` / `gcd_u64` / `lcm_u64` / `clamp_i64` / `i64_min()` / `i64_max()` | ✅ | 同样用带类型后缀的自由函数；`i64_min/max` 是**函数不是常量**（跨模块引用的大整数字面量 `const` 不会折叠，会留下未定义符号） |
| `core::option` / `core::result` | `Option` 的 `expect` / `unwrap_or_else` / `map_or` / `ok_or` / `filter` / `or`；`Result` 的 `unwrap_or` / `unwrap_or_else` / `ok` / `err` / `map_err` | ✅ | 回归 `tests/cases/149_std_core_ext.zr` |
| `alloc::boxed::Box` | `Box<T>`：`new` / `get` / `get_mut` / `as_ptr` / `into_raw` / `replace` / `free`；`box_new` / `box_from_raw` | ✅ | 值语义，**没有 `Drop`**，要显式 `free`。`Box<dyn Trait>` 的 vtable 分派可用（`169_box_dyn.zr`：`Box<Self>` 与 `&Self` 两种接收者、`*mut T as *mut dyn Tr` 指针 fatten、`into_raw` / `from_raw` 重建）；`Box<T>` 自身方法表不被无条件 deref 屏蔽（`lookup_method` 先查 `Box` 再 deref 到 `T`） |
| `alloc::rc::Rc` / `cell::RefCell` | `Rc<T>`：`new` / `clone` / `get` / `strong_count` / `free`；`rc_new`；`RefCell<T>`：`new` / `borrow_mut` / `replace`；`refcell_new` | 🟡 | `Rc` 引用计数共享所有权（堆上 `{strong, value}`，栈上只留指针），`free` 递减计数、归零释放；`RefCell` 内部可变性（`borrow_mut` 给裸指针）。无自动析构，用完必须显式 `free`（`167_rc_refcell.zr`）。无借用运行期检查（`borrow_mut` 不 panic） |
| `sync::Mutex` / `marker::Send` | `Mutex<T>`：`new` / `lock` / `unlock` / `free`；`Send`（marker trait） | 🟡 | 两线程各 1000 次持锁递增共享计数器精确等于 2000（`168_mutex.zr`）；`Send` 是空 marker trait（无编译期自动推导）。无通道 |
| `alloc::vec::Vec` | 在原有 API 上补 `first` / `last` / `truncate` / `retain` / `reverse` / `swap` / `extend_from_slice` / `dedup` | ✅ | 回归 `tests/cases/150_std_alloc_ext.zr` |
| `alloc::string::String` | 补 `trim` / `trim_start` / `trim_end` / `to_lowercase` / `to_uppercase` / `find_str` / `rfind_byte` / `split` / `split_whitespace` / `replace` / `parse_i64` / `parse_i32`，并实现 `Eq` / `Ord` / `Hash` / `Clone` / `Default` | ✅ | `Eq` 手写字节比较（不能在 trait impl 里调同名方法，会递归成栈溢出）；`Hash` 用 FNV-1a |
| `alloc::collections::VecDeque` | `VecDeque<T>`：`new` / `with_capacity` / `push_back` / `push_front` / `pop_front` / `pop_back` / `front` / `back` / `get` / `set` / `clear` / `destroy` / `into_vec` | ✅ | 环形缓冲，字段叫 `size` / `capac` 以避开同名方法 |
| `alloc::collections::HashMap` | `HashMap<K, V>`（`K: Eq + Hash`）：`new` / `with_capacity` / `insert` / `insert_get` / `get` / `get_ref` / `contains_key` / `remove` / `clear` / `destroy` | ✅ | 开放寻址线性探测 + 装载因子 1/2 扩容 rehash；`remove` 用 O(n) 重建（直接标空会截断探测链）。String 键与 i64 键都已回归 |
| `std::io::stdout/stderr/stdin` | `stdout()` / `stderr()` / `stdin()` 返回 `File`；`File::read_line` / `File::read_to_string` / `File::write_string` | ✅ | 与路径版 `fs` 函数同层，注意两个 `read_bytes` 已拆分（见 §3.2） |

**本轮顺带修的三处编译器缺陷**（否则泛型容器的分配尺寸对不上真实步长）：

1. `size_of` / `align_of` 对**泛型具名类型**没有按实参实例化——`size_of::<Entry<K, V>>()`
   把每个形参字段都按 1 字节算（`Entry<String, i32>` 得 1，真实 32）。修法：
   `InferCtxt` 增加 `type_params`，`size_of` / `align_of` 先用 `subst_ty` 把字段里的
   `Ty::Infer` / `ConstParam` 换成实参（`src/typeck/infer.rs`）。
2. `size_of` 的结构体 / 元组 / 枚举尺寸**没有按对齐补齐**（`{String, i32, bool}` 得 29，
   MIR 的 `struct_layout` 步长 32）。按 `size_of` 分配的桶数组会越界写堆。修法：
   与 `src/mir/builder.rs` 的 `type_size_align` / `enum_layout` 同一口径，末尾
   `align_up`；定长数组的 `size_of` 也改用 `hir::array_len_value`（原来一律按 1 个元素算）。
   同时把 `char` 的尺寸 / 对齐改成 1 字节，和 MIR 一致。
3. 排掉一个符号遮蔽：`std/std/io.zr` 里的私有助手 `align_of(c: u8) -> i32` 与内建
   `align_of::<T>()` 撞名，`use std::*` 后用户程序里的 `align_of::<i64>()` 会绑到它
   （返回对齐枚举常量 2）。已改名 `io_align_of`。

---

## 四、与 Rust 语义有意不同的地方

下面这些**不是 bug，是 zr 的设计选择**，写 zr 代码时不能照搬 Rust 的直觉。

1. **整数溢出回绕，不 panic。**
   `let x: i32 = 2147483647; let y = x + 1;` → `y == 2147483648`（Rust 会在 debug 下 panic）。
   zr 没有 debug/release 之分，也没有溢出检查开关，整体行为对齐 Zig / C。

2. **`let` 默认不可变，可变要写 `let mut`；没有 `var`。**
   `let x = 1; x = 2;` 编译期报 ``cannot assign to immutable binding (declare it with `let mut`)``。
   这与 Rust 一致，但 Rust 的 `let mut` 之外还有 `mut` 引用绑定等更细的规则，zr 只有「绑定可不可变」这一层。

3. **`&T` 是只读指针，`&mut T` 是可写指针；借用检查只有最小切片。**
   zr 把引用降级成裸指针语义：`&T` 上赋值会被拦（`cannot assign through an immutable pointer`），
   借用规则也做了最小检查（同位置重叠借用、写 / 读被借用位置，NLL 按 last-use
   近似，见 §2.4），移动也只检查整值。但它**远不是完整的借用检查器**——多绑定 /
   未关联借用、跨函数流动都不检查；`let r: &i32 = &x;` 还会把注解解析成
   `*const i32`。

4. **`str` 是 `{ptr, len}` 结构体，切片是 `[]const T`。**
   Rust 的 `&str` 是胖指针、`&[T]` 是 `&` + 长度；zr 把 `str` 做成了有两个字段的结构体，
   切片类型写成 `[]const T`（元素在后），而不是 Rust 的 `&[T]`。
   `[]mut T` 可用；数组 → 切片是隐式强转（Rust 里 `&arr` 也要显式配合）。
   切片没有越界检查（借用检查见 §2.4 的最小切片）。

5. **`print` / `println` 是函数，不是宏；格式化另走 `println_fmt`。**
   Rust 的 `println!("{} {}", a, b)` 在 zr 里要写成
   `println_fmt("{} {}", &[fmt_int(a), fmt_int(b)], 2)`。
   好处是 zr 没有宏卫生性问题；代价是写起来啰嗦，且格式规格只支持四种。

6. **标准库模块不带 `std::` 前缀，`Option` / `Result` 不在 prelude。**
   Rust 写 `std::option::Option`（或直接 `Option`）；zr 要写 `use core::Option;`，
   模块名是 `argparse` / `async` / `base64` / `collections` / `core` / `env` /
   `error` / `fs` / `http` / `io` / `json` / `md5` / `mem` / `net` / `os` / `path` /
   `pdb` / `posix` / `process` / `sha256` / `simd` / `slice` / `string` / `tensor` /
   `thread` / `time` / `tls` / `toml` / `tz` / `xml`（`cli` 已重命名为 `argparse`；
   `tls`、`crypto` 为纯 ZR 实现、零外部依赖，其余随 `use` 按需加载）。

7. **枚举的内存布局与 Rust 一致。**
   zr 的枚举是 `[判别式][载荷区]`，`Auto` 判别式按变体个数挑最小够用的整数，
   变体模式先比判别式再看载荷——这部分和 Rust 一样（泛型枚举按实例替换载荷类型）。

8. **窄浮点 `f8` / `f16` 是 zr 自创的。**
   `f16` 兼容 IEEE 754 binary16；`f8` 是 zr 自定义（1 符号 + 4 指数 + 3 尾数，偏置 7），
   范围很小（最大约 240，最小规格化数 2⁻⁶），Rust 没有对应类型。

9. **错误处理保留了 Zig 风格的 `T!E` 类型，但没保留运算符。**
   `fn f() -> i32!i32` 能写，`f()!` 的解包运算符解析不了；
   Rust 风格的 `?` 只对 `Result` / `Option` 生效，不作用于 `T!E`。

10. **`Option` / `Result` 的方法与模式匹配都可用。**
    `is_some()` / `is_none()` / `is_ok()` / `is_err()` / `unwrap()` /
    `unwrap_or()` / `map()`、模式匹配与后缀 `?` 均返回正确结果。

11. **`&&` / `||` 不短路（两侧都会求值）。**
    `if false && boom()` 会执行 `boom()`（实测 `tests/cases/05_logic.zr` 只验证了
    纯函数布尔代数，没暴露求值顺序）。因此**不能**用 `&&` 做"空指针/越界保护"：
    `map.data[i].used && map.data[i].key.eq(k)` 在空槽上仍会调 `eq`，读到未初始化
    的 `ptr` 直接段错误。标准库的写法是先用 `if used { .. }` 把求值守住
    （见 `std/alloc/hashmap.zr` 的 `probe`）。这条是编译器控制的语义缺口，
    在补齐短路求值之前，写 zr 代码请把它当 C 的位运算 `&` / `|` 对待。

12. **`Vec<T>` 经引用参数下标取值会取到错误地址。**
    `fn f(v: &Vec<i32>) { v[i] }` 里 `v[i]` 返回的是垃圾值（同一份数据用
    `v.get(i)` / `v.as_slice()[i]` / `v.get_ref(i)` 都正常；裸指针 `self.data[i]`
    也不受影响，所以 `hashmap.zr` 没踩到）。`&mut Vec<T>` 同样中招。
    `std/std/http2.zr` 的 `HpackDecoder` 因此一律用 `as_slice()[i]` 访问动态表。
    这是 SILENT 级别的缺陷（不报错、值错），在修掉之前，**结构体方法里拿
    `&Vec` / `&mut Vec` 形参时不要写 `v[i]`**。

13. **超大栈数组字面量会让编译退化到分钟级。**
    `let mut buf: [u8; 65536] = [0; 65536];` 实测 5 分钟仍未完成编译（同文件
    删掉这一行后 6.2s 编完）。`std/std/http2.zr` 的读帧路径改成
    `Vec::with_capacity(len)` 逐字节铺零 + `payload.data` 原地读入。
    栈上大数组在 zr 里请一律换成堆缓冲。

---

## 五、按优先级排序的待办缺口

> 排序依据：**影响面 × 静默出错的可能性**。P0 = 会产生错误结果但不报错，
> 任何一条都能让「看起来能跑的程序」给出错误答案。

### P0 — 静默出错，必须先修

- **`&Vec<T>` / `&mut Vec<T>` 形参上的 `v[i]` 返回垃圾值**（上文 §四.12）。
  不报错、不越界检查，只是读到错误地址；没踩到是因为标准库恰好都用
  `as_slice()` / 裸指针访问。修法是让引用参数的索引走与 `as_slice()` 相同的
  地址计算。

### P1 — 明确报错或明显缺失的功能（能用不了，但至少看得见）

1. **`#[derive(...)]` 的剩余缺口**（方法体本身已能生成，见 §2.5 那一行）：
   `Debug` 已生成 `.debug()` 方法体（`Type__debug(self) -> String`，Rust `{:?}`
   风格；浮点/指针/数组/元组/`String` 字段显式报错；**结构体变体载荷里的
   `str` 字段与 `str` 后嵌套字段两处组合错经 `EnumField` 聚合特判后也已对齐
   Rust**）；泛型类型上的 derive 明确报错；**枚举的 `Default` 已支持**
   （用 `#[default]` 标出恰好一个变体，`116_enum_default.zr`；没有或多个
   `#[default]` 都报错，负向 `14_enum_default.zr`）；**枚举的 `PartialOrd` /
   `Ord` 已支持**
   （无显式判别式时按声明顺序比变体、同一变体内再比载荷字典序，
   `114_enum_ord_derive.zr`；带显式判别式的枚举仍报错，负向
   `26_enum_ord_discriminant.zr`）；字段类型缺少所需 trait 时
   报的是整个类型的方法检查失败，尚未精确指向字段。可接受的 derive 名：
   `Debug` `Clone` `Copy` `PartialEq` `Eq` `PartialOrd` `Ord` `Hash` `Default`；
   其它名字报 `cannot derive ...`。
2. **`unsafe` 只是骨架**（`unsafe { ... }` 块可解析和通过类型检查，`85_unsafe.zr`；
   当前不改变语义，按块表达式透传）；**`dyn` 只是骨架**（词法+解析+类型检查
   已实现，`88_dyn_keyword.zr`；vtable 分派未完工，trait-object 转换受限，
   `std::error` 因此仍难导入）。
3. **借用检查 / 移动检查已是最小切片**（不是没有）。同位置借用重叠（`&mut` 与
   任意、`&` 与 `&mut`）报 `cannot borrow ... while already borrowed`；写被借用
   位置报 `cannot assign to ... while it is borrowed`；读被可变借用位置报
   `cannot read ... while it is mutably borrowed`（NLL 按 last-use 近似，
   `105_borrow_nll.zr` + 负向 20/21/22）；`let b = a` 对含 drop glue 的类型是
   移动、读被移动的源报 use-after-move（`87_move_check.zr`）。**已知局限**：
   多绑定 / 未关联借用仍按作用域结束；移动只覆盖整值，字段级 / 按值传参不覆盖。
4. **格式引擎已接自定义类型的 `{:?}` / `{:#?}`**：命名参数 `{name}` 走
   `println_fmt_named`；`#[derive(Debug)]` 生成 `.debug()` / `.debug_pretty()`
   及格式引擎入口 `Type__debug_fmt(*const void, bool, *mut u8, usize) -> usize`，
   调用方经 `io::fmt_debug(&v as *const void, Type__debug_fmt)` 把值交给
   `println_fmt` 的格式串（`FormatArg` 的 `kind = 7` 类型化值通道，`#` 在运行期
   决定单行 / 多行）。其余（`{}` `{:?}` `{:x}` `{:X}` `{:b}` `{:o}` `{:e}`
   `{:E}`、位置参数、填充 / 对齐 / `+` / `#` / `0` / 宽度 / 精度、动态宽度
   `{:1$}`、动态精度 `{:.*}` / `{:.1$}`）已与 Rust 逐字符对齐。**未覆盖**：
   泛型类型上的 derive；`{:#?}` 对浮点 / 指针 / 数组 / 元组字段（derive 侧显式报错）；
   标量的 `{:#?}` 仍只输出一行。
5. **闭包：`fn(..)` 形参 / 返回值两条路径都已支持**（P1-2c，2026-11-02）。无捕获闭包
   可存变量 / 当实参 / 返回；带捕获闭包也可存变量 / 嵌套 / 多次调用，并能按值传给
   **泛型**形参、从泛型返回值透传（`109_closure_env_cross_fn.zr`、
   `110_closure_env_generic_passthrough.zr`）；**还能转成裸 `fn(..)` 值**——函数值
   统一是 16 字节记录 `{code, env}`，`code` 指向末尾多收一个 `env` 的 shim（普通
   函数是转发 shim、带捕获闭包是特化 thunk，环境在降级时复制到堆），间接调用统一按
   `(实参..., env, [隐藏返回槽])` 传参；`extern "C"` 边界退化回裸代码地址
   （`125_closure_to_fn.zr`，负向 `38_generic_fn_as_value.zr` / `39_async_fn_as_value.zr`）。
   **仍不支持**：带捕获闭包经 `extern "C"` 交给 C 侧回调（env 无处安放）；泛型 /
   异步函数当值（明确报错）。
6. **`i128` / `u128` 只在 64 位目标上可用**（拆成两个连续 8 字节栈槽，加减
   走 `adc`/`sbb`、移位走 `shld`/`shrd`、比较按高位优先定序、传参 / 返回用
   `rdx:rax`；源码见 `src/codegen/encoder.rs`）。**乘 / 除 / 取模已实现**
   （乘拆四段 64 位部分积、除 / 取模走 128 轮软件长除；宽整数实参要占两个
   实参寄存器，宽度靠 `regalloc::CalleeParams` 查被调方声明——早先只按返回值
   标宽，`div128(-100, 7)` 在 -O2/-Oz 直接 SIGILL）；**32 位目标仍禁用**
   （类型检查阶段报错）。回归 `tests/cases/111_i128.zr`、
   `tests/cases/113_i128_muldiv.zr`。
7. **标准库新增进程参数 / 环境变量 / 配置格式 / 平台与编码工具**：`std/std/argparse.zr`
   （`arg_count` / `arg` / `args` / 按名读取 / 子命令 / `print_help`）、`std/std/env.zr`
   （`var` / `set_var` / `remove_var`）、`std/alloc/json.zr`、`std/alloc/xml.zr`、`std/alloc/toml.zr`、
   `std/std/os.zr`、`std/std/posix.zr`、`std/std/time.zr`、`std/std/tz.zr`、`std/alloc/md5.zr`、
   `std/alloc/sha256.zr`、`std/alloc/base64.zr`、`std/std/http.zr`、`std/std/http2.zr`、`std/alloc/pdb.zr`、`std/alloc/tensor.zr`、
   `std/std/tls.zr`、`std/core/simd.zr`（七种 lane 宽度）。JSON/XML/TOML
   的公开入口带 `json_` / `xml_` / `toml_` 前缀以避开扁平符号表
   （`std/std/net.zr` 的旧版 HTTP 客户端已移除，`http` 复用 `net::SockAddrStorage`）；解析错误都带字节位置。
   限制：环境变量写入只影响本进程的覆盖表；TOML 不支持日期时间与 `[[array.of.tables]]`；
   HTTP 仅明文 + 点分 IPv4；HTTP/2 走明文 h2c（prior knowledge，无 TLS/ALPN），
   实现了 HPACK 静态表 + 动态表 + Huffman 解码、HEADERS/CONTINUATION 拆帧、
   DATA 帧与流控（每帧回 WINDOW_UPDATE，请求体上限 65535），不支持服务端推送与
   PRIORITY 帧；`Tensor` 的 `conv2d` 无反向、autograd 是极简反向模式、
   GPU 仅占位；`tls` 是纯 ZR TLS 1.2 客户端（PSK 握手中实现、AES-128-CTR+HMAC-SHA256
   加密 record 层），零外部依赖，但无 RSA/ECDH 证书验证（受限于无 u128/BigInt）。

8. **泛型推断的边界**：类型参数从实参、期望返回类型**和用法体**推断
   （`try_instantiate_call` 推不出时留推断变量，`let v = Vec::new(); v.push(1)`
   能靠 `push(1)` 定出 `T = i32`，`98_generic_infer_usage.zr`；若整个实例化
   体检查完仍停在未解析的推断变量才报 `cannot infer type parameter`，
   负向 `18_generic_infer_unused.zr`）。
   `T: Trait` / `where` 约束现在会在**单态化时**校验（`check_trait_bounds`，
   含超类型的双向展开）。

---

## 附录：本次审计的验证环境

- 主机：macOS（x86_64-apple-darwin, Mach-O）
- 编译器：`cargo build` 产出的 `./target/debug/zr`（v0.8.0）
- 编译命令统一为：`./target/debug/zr --no-incremental <case>.zr -o <bin>`
  （必须带 `--no-incremental`，否则增量缓存会给出旧产物）
- 判定「已实现」的标准：程序**编译通过且运行结果与 Rust 一致**；
  判定「未实现」的标准：编译期明确报错，或运行结果与 Rust 不一致
  （后者一律在正文里标为 ❌ 并注明实测输出）
- 审计期间 `std/std/io.zr` 正被并行修改（新增了 `zr_seek` / `zr_fstat_size` /
  `zr_isatty` 运行时导出），第三章的文件能力反映的是**当时的在途状态**；
  其余章节不依赖该文件的改动。

### 已修复的静默 bug（回归时值得重跑的形状）

这些曾经"编译通过、运行不崩、结果全错"，都已修好并有测试钉住
（`tests/cases/76_ssa_join_phi.zr`、`77_many_args.zr`、
`tests/negative/12_index_scalar.zr`）：

1. **合流块漏 φ**（`src/ssa/mod.rs`）：支配边界只算直接前驱版，`if c { while .. { x = .. } }`
   之后再读 `x`，走 else 那条路径时读到未初始化的栈槽（实测是一段栈地址）。
   现用 Cooper–Harvey–Kennedy 沿支配树求迭代替换边界。
2. **内联时逐个"按值替换"操作数**（`src/opt/pass_manager.rs`）：`%10 = %1 Add %9`
   里先把 `%1` 换成新号 `%9`，再把 `%9` 换掉，两个操作数一起变成新号——
   `spawn` 的帧初始化把偏移量当地址，往 `0x20` 写，`-O3` 直接 SIGSEGV。
   现在先建完整重命名表，再逐操作数一次性映射。
3. **第 7 个及之后的实参被静默丢弃**（`src/codegen/encoder.rs`）：SysV 的栈上传参
   没实现，`std/std/io.zr` 里 7 形参的 `emit_arg` 宽度恒为 0，所有填充 / 对齐 / 零填充
   规格失效。现在按 ABI 压栈、被调方从 `[rbp+16+8k]` 读，并补 16 字节对齐。
4. **指针指向数组时元素类型取错**（`src/typeck/mod.rs`）：`p: *mut [u8; 16]` 的
   `p[k]` 曾被当成 `[u8; 16]`，按 16 字节步长缩放。
5. **对标量下标被静默接受**：`(*p)[i]`（`p: *const u8`）曾照编不误、运行时
   SIGSEGV；现在报 ``cannot index a value of type `u8` ``。指针下标要写 `p[i]`。
6. **浮点形参的"浮点性"靠用法反推**（`src/codegen/encoder*.rs` 的
   `assign_float_slots`）：只被转手的浮点形参（`fn outer(f: f64) { inner(f) }`）
   从不参与浮点运算，于是被判成整数——调用方按 SysV 放进 xmm0，被调方从 rdi 读。
   `fmt_float(f)` 的实参是 f64 形参时 `-O0/-O1/-O2` 直接 SIGSEGV（位模式当指针），
   混排整数形参时则是"编译通过、运行不崩、结果全错"。现在 `SsaFunction::param_types`
   从 MIR 抄过来，两个后端都拿它给形参显式播种。
