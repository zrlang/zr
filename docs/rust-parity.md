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
| `i8` / `i16` / `i32` / `i64` | 全部可用 | ✅ | 字面量后缀（`42u64`）、类型推导、`as` 截断转换都与 Rust 一致 |
| `u8` / `u16` / `u32` | 可用 | 🟡 | 算术正确，但 `u64` 的高位问题见下行 |
| `u64` | 可用但高位出错 | 🟡 | **≥ 2⁶³ 的 `u64` 比较、`>>`、`/`、`%` 按有符号处理**，结果错误（详见「待办缺口」#1） |
| `i128` / `u128` | 编译期报错 | ❌ | `zr: 128 位整数在当前目标上尚未实现：该目标只有 64 位寄存器，编译器暂时还不能把宽整数拆成寄存器对。现在生成的是静默截断的代码，所以直接报错。`（64 位目标上恒定失败） |
| `isize` / `usize` | 可用 | ✅ | 宽度等于目标指针宽度 |
| `f32` / `f64` | 可用 | ✅ | 算术、比较、字面量后缀（`3.14f32`）正常 |
| `f8` / `f16` | 可用（zr 自定义格式） | 🟡 | 窄浮点按 IEEE 754 binary16 / zr 自定义 8 位格式实现；**隐式转 `f64` 的调用点转换是错的**（见 #2） |
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
| `&[T]` / `&mut [T]` 切片 | `[]const T` / `[]mut T` | ✅ | 元素类型写在括号**后面**（zr 独有写法）。可变切片可写回原数组；**没有借用检查**，任何数组都能隐式转成 `[]mut T` |
| 数组 → 切片 强转 | 自动强转 | ✅ | `let s: []const i32 = arr;`、`f(arr)`（形参 `[]T`）都自动补 `(ptr, len)`；`arr[1..3]` / `arr[1..]` / `arr[..3]` / `arr[..]` / `arr[1..=3]` 均产生切片 |
| `(T, U)` 元组 | 可用 | 🟡 | 直接写元组类型可用；但**经类型别名后取 `.0` 失败**：``zr: no field `0` on type `type#75` `` |
| `struct` | 可用 | ✅ | 具名字段、元组结构体、嵌套字段、`impl` 方法、`impl Drop`、字段按值拷贝均正常 |
| `enum`（带载荷） | 可用 | ✅ | 判别式与载荷正常（含 `f64` 载荷、struct 载荷变体）；泛型枚举按实例单态化，`Option<T>` / `Result<T, E>` 的载荷也能正确读写 |
| `union` | 只能声明 | ❌ | `union U { i: i32 }` 能声明，但构造报 ``zr: `U` is not a struct ``；字段读写也没有安全的入口 |
| `*const T` / `*mut T` | 可用 | ✅ | 裸指针可用；`*p = v` 走指针语义 |
| `&T` / `&mut T` | 可用 | 🟡 | 语义上 `&T` 是只读指针（``zr: cannot assign through an immutable pointer (use `&mut` and a `*mut` type) ``），`&mut T` 可写；但**`let` 注解里的 `&T` 被解析成 `*const T`**：`let r: &i32 = &x;` → ``zr: type mismatch: expected `*const i32`, found `&i32` `` |
| `Option<T>` / `Result<T, E>` | 可用 | ✅ | 在 `std::core` 里；`is_some()` / `is_none()` / `is_ok()` / `is_err()` / `unwrap()` / `unwrap_or()` / `map()` 与模式匹配均正常（`?` 仍是占位实现，见 P0 #3） |
| 泛型函数 | 可用 | ✅ | 按调用点单态化（含递归泛型）；类型实参从实参 / 期望返回类型推断。泛型约束（`T: Ord`、`where`）只解析不检查，实例化后按具体类型检查方法调用 |
| 泛型类型 | 可用 | ✅ | 泛型结构体 / 枚举 + `impl<T> Type<T>` 的方法均可用（`Vec<T>`、`Option<T>` 等）；字段与载荷类型按类型实参替换 |
| `trait` | 可用 | ✅ | `trait` + `impl Trait for Type` 正常（`std::core` 的 `Eq`/`Ord`/`Copy`/`Clone`/`Display` 都在用） |
| 关联类型 | 可用 | ✅ | `type Output;` + `type Output = i32;` 正常，`Self::Output` 能取到 |
| `impl` 块 | 可用 | ✅ | 关联函数、`self` / `&self` / `&mut self` 三种接收者、`Self` 类型均可 |
| `type` 别名 | 半可用 | 🟡 | **函数签名里可用**（`fn f(x: MyInt)`），**`let` 注解里不可用**：`let m: MyInt = 5;` → ``zr: type mismatch: expected `i32`, found `type#70` ``（别名被当成独立名义类型，不做展开） |
| 生命周期标注 `'a` | 解析后丢弃 | 🟡 | `&'a T`、`struct S<'a>` 能写，但类型检查阶段直接丢掉标注（`src/parser/ty.rs`），**没有任何借用/存活检查** |
| `dyn Trait` / trait 对象 | 不支持 | ❌ | 词法器里**没有 `dyn` 关键字**：`&dyn Speak` → `zr: expected ')': expected RParen, found Ident("Speak")` |
| auto trait | 不支持 | ❌ | 无 auto trait 机制、无 blanket impl 语法 |
| `PhantomData<T>` | 不存在 | ❌ | `std::core` 里没有 `PhantomData`，也没有等价物 |

### 1.3 泛型实参在类型注解里的写法

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `let x: Option<i32> = ...;` | 可用 | ✅ | 类型注解里的泛型实参保留，字段 / 载荷类型按实参替换 |
| `fn f(x: Option<i32>)` | 可用 | ✅ | 签名与函数体一致 |
| `Option::<i32>::None` | 不支持 | ❌ | turbofish 只在**调用位置**生效（`size_of::<T>()`、`Foo::<i32>::new()`）；类型路径上的 `::<>` 解析失败。无上下文的 `Option::None` 需要 `let` 注解 |

---

## 二、流程控制

### 2.1 条件与循环

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `if` / `else` 语句 | 可用 | ✅ | |
| `if` 表达式 | 可用 | ✅ | `let m = if a > b { a } else { b };` |
| `if let` | 可用 | ✅ | 本地枚举与导入枚举（`use core::Option;`）都能用 |
| `let ... else` | 可用 | ✅ | `let Step::Value(v) = s else { return 1; };`；else 必须发散 |
| `while let` | 可用 | ✅ | 脱糖成 `loop { let tmp = EXPR; if let PAT = tmp { .. } else { break; } }`，每轮只求值一次 |
| `loop` / `while` / `for` | 可用 | ✅ | `for i in 0..10`、`for i in 1..=10`、`for x in array` 均正常 |
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
| 穷尽性检查 | 可用 | 🟡 | 枚举全覆盖时**可以**不写 `_`（与 Rust 一致）；但缺臂时报错信息有误导性：``zr: match must have a trailing `_` arm `` |
| 切片模式 `[a, b, c]` | 不支持 | ❌ | `zr: match on slice patterns is not implemented yet` |
| 引用模式 `&v` | 不支持 | ❌ | 解析期就失败：`zr: expected pattern, found Amp`（类型检查器里还有一条 `zr: match on reference patterns is not implemented yet` 但到不了） |

### 2.3 函数与表达式

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| 块表达式 | 可用 | ✅ | `let v = { let a = 1; a + 1 };` |
| 函数指针 `fn(T) -> R` | 可用 | ✅ | 取址、传参、返回值、存进结构体字段都行（x86-64 专属，其它后端明确报错） |
| 闭包 `\|x\| x * factor` | 不支持 | ❌ | `zr: closures are not implemented yet (captures are not lowered)`（README 已记录） |
| `impl Trait` 参数 | 不支持 | ❌ | `fn f(op: impl Fn(T) -> T)` → `zr: expected ')': expected LParen`；`impl Fn(T) -> T` 这种 trait 路径参数写不了 |
| `?` 运算符 | 语法保留，语义是占位 | ❌ | **后缀 `?` 在解析期被替换成解引用**（`src/parser/expr.rs`：`left = Expression::Unary(UnaryOp::Deref, ...); // placeholder`）。后果：不传播错误（`Err` 分支不会被选中）、成功时返回**未初始化内存**。`let v = inner(x)?;` 得到垃圾值 |
| `try` / `catch` | 不支持 | ❌ | `catch` 必须带模式，带了又解析不过：`zr: expected pattern, found LBrace` / `zr: expected field name: expected Ident(""), found IntLiteral(2, None)` |
| `throw` | 解析但不执行 | ❌ | `let v = throw 5;` 能编译，`v` 恒为 0，异常不传播也不中止 |
| `defer` | 解析但不执行 | ❌ | `defer { ... }` 能编译，但块**从不执行** |
| `async fn` / `.await` | 可用 | ✅ | Rust 风格受限降级：future 帧在堆上，`.await` 轮询；`async fn main` 入口执行器可用（实测 `count(5).await` → 10，`double(10).await` → 20） |
| `comptime` | 可用 | ✅ | `let v = comptime 1 + 2;` → 3 |
| `unsafe { }` | 不支持 | ❌ | **词法器里没有 `unsafe` 关键字**；README「指针与 unsafe」一节写的写法目前无法编译（`zr: expected ';' after binding: expected Semicolon, found LBrace`） |

### 2.4 所有权 / 借用

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| 移动语义（use after move 报错） | 不检查 | ❌ | `let s2 = s1;` 之后再用 `s1` 不报错（README「所有权系统」一节与实际不符） |
| 借用检查（`&mut` 独占） | 不检查 | ❌ | 同一变量可以同时取两个 `&mut`，不报 E0499 类错误 |
| `let` 默认不可变 | 一致 | ✅ | `let x = 1; x = 2;` → ``zr: cannot assign to immutable binding (declare it with `let mut`) `` |
| `var` | 已移除 | ✅ | `var x = 1;` → 解析错误（zr 用 `let` / `let mut`） |

### 2.5 模块与属性

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `mod` / `use` / `use ... as ...` | 部分可用 | 🟡 | `mod`、`use`、`use a::{b, c}` 分组导入可用；**`use x as y` 重命名无效**：``zr: cannot find value `hf` in this scope `` |
| `use foo::*` | 不支持 | ❌ | ``zr: glob imports (`use foo::*`) require module system support (not yet implemented) `` |
| `use std::core::Option` | 路径不带 `std::` | 🟡 | zr 的标准库模块直接叫 `core` / `io` / `string` / `collections`：`use core::Option;`。写 `std::core::Option` → ``zr: unknown module `std` `` |
| `pub` / `pub(crate)` / `pub(super)` | 仅 `pub` | 🟡 | 没有 `pub(crate)` / `pub(super)` 的可见性检查 |
| `#[derive(Debug, Clone, Copy)]` | 完全不可用 | ❌ | **结构体上直接报错**：``zr: undefined name `P` ``（derive 在阶段 0 生成 impl，此时类型名还没登记）；**枚举上被静默忽略**（`src/typeck/mod.rs` 对非 struct 的 `attrs` 硬编码成 `&[]`），`e.clone()` → ``zr: no field `clone` on type `type#70` `` |
| `#[repr(C)]` / `#[repr(u8)]` | 可用 | ✅ | |
| `#[inline(...)]` / `#[link(...)]` / `#[test]` | 可用 | ✅ | `#[test]` 的函数会被编进产物并在 `main` 前调用 |
| `static mut X: i32 = 0;` | 不可用 | ❌ | 静态量**没有生成数据符号**：`nm` 显示 `U _COUNTER`（未定义）→ 链接失败；赋值也报 `zr: expression is not assignable (expected variable, field, deref, or index)` |
| `extern "C" { ... }` 块 | 可用 | ✅ | `extern "C" fn ...`（Rust 风格的带体定义）不支持：`zr: expected '{' after extern: expected LBrace, found KwFn` |

---

## 三、文件与 IO

### 3.1 标准输出 / 错误输出

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `println!("...")` 宏 | `println(s: str)` 普通函数 | 🟡 | zr 的 `println` 是**函数不是宏**：`println!("{}", 1)` 解析失败（`zr: expected '}' to close block: expected RBrace, found Bang`）；且**参数类型不检查**：`println(42)` 能编译，运行时段错误（exit 139） |
| `print!` / `println!` 格式化 `{}` | `println_fmt(fmt, &[FormatArg], n)` | 🟡 | 支持 `{}`、`{:?}`、`{:x}`、`{:X}` 与 `{{` / `}}` 转义；**不支持宽度 / 精度 / 对齐 / 填充**：`"{:>8}"`、`"{:04}"`、`"{:.2}"` 会被**原样打印**（不报错） |
| `Display` 浮点格式 | 大数出错 | ❌ | `1.0e300` 打印成 `.0`（`std/io.zr` 的 `write_float` 用 `val as u64` 取整数部分，溢出后全丢）。`1.0`→`1`、`3.5`→`3.5`、`1.0/3.0`→`0.3333333333333333`、`0.1+0.2`→`0.30000000000000004`、`-0.0`→`-0` 与 Rust 一致 |
| `eprintln!` / `eprint!` | `eprintln(s)` / `eprint(s)` | ✅ | 写到 STDERR |
| `println!("{:?}", x)` 结构体调试 | 需手写 `fmt_*` | 🟡 | 只有 `fmt_int` / `fmt_uint` / `fmt_float` / `fmt_bool` / `fmt_str` / `fmt_char` / `fmt_hex` 几种预制参数，没有自动 `Debug` |

### 3.2 标准输入

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `BufRead::read_line(&mut String)` | `stdin_read_line(buf, cap) -> usize` | 🟡 | 读的是**裸字节**（不含 `\n`），返回读到的字节数；没有 `String` 版本的行读取 |
| `Read::read(&mut [u8])` | `read_bytes(buf, cap) -> usize` | 🟡 | 返回字节数，EOF 返回 0 |

### 3.3 文件

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `File::open` / `OpenOptions` | `File::open(path, mode)` | 🟡 | mode 是整数常量（`FILE_READ` / `FILE_WRITE` / `FILE_APPEND`）；`is_ok()` 判断成败（没有 `Result`） |
| `read` / `read_to_end` | `read(buf, cap)` / `file_read_all` | 🟡 | 返回字节数；`read_to_string` 没有 |
| `write` / `write_all` | `write(bytes)` / `write_all(bytes)` | ✅ | |
| `seek` | `seek(offset, whence)` | 🟡 | `SEEK_SET` / `SEEK_CUR` / `SEEK_END`，返回新位置 |
| `close` | `close() -> bool` | 🟡 | **显式关闭**，`File` 不实现 `Drop`，不会自动关闭 |
| `metadata().len()` | `size() -> i64` | 🟡 | 只有大小，**没有权限 / 时间戳 / inode 等元数据** |
| `is_terminal` | `is_terminal()` | ✅ | |
| 目录操作（`create_dir` / `read_dir` / `remove_dir`） | `fs::create_dir` / `read_dir_count` + `read_dir_name` / `remove_dir` | ✅ | 目录遍历跳过 `.` 与 `..`；`read_dir_name` 写进调用方缓冲区 |
| 路径操作（`Path` / `join` / `exists`） | `path::join` / `normalize` / `basename` / `extname` / `substr` / `fs::exists` | 🟡 | 没有 `Path` 类型，直接在 `str` 上算；`substr` / `basename` / `extname` 直接返回 `str` 切片 |
| `Instant` / `SystemTime` / `sleep` | `zr_time_ns` / `zr_time_unix_ns` / `zr_sleep_ms`（`thread::sleep_ms`） | 🟡 | 只有纳秒整数，没有类型化的时间差 / 格式化 |
| 临时文件（`tempfile`） | 无 | ❌ | 同上 |

---

## 四、与 Rust 语义有意不同的地方

下面这些**不是 bug，是 zr 的设计选择**，写 zr 代码时不能照搬 Rust 的直觉。

1. **整数溢出回绕，不 panic。**
   `let x: i32 = 2147483647; let y = x + 1;` → `y == 2147483648`（Rust 会在 debug 下 panic）。
   zr 没有 debug/release 之分，也没有溢出检查开关，整体行为对齐 Zig / C。

2. **`let` 默认不可变，可变要写 `let mut`；没有 `var`。**
   `let x = 1; x = 2;` 编译期报 ``cannot assign to immutable binding (declare it with `let mut`)``。
   这与 Rust 一致，但 Rust 的 `let mut` 之外还有 `mut` 引用绑定等更细的规则，zr 只有「绑定可不可变」这一层。

3. **`&T` 是只读指针，`&mut T` 是可写指针，但没有借用检查。**
   zr 把引用降级成裸指针语义：`&T` 上赋值会被拦（`cannot assign through an immutable pointer`），
   但**借用规则本身不检查**——多个 `&mut`、移动后继续使用都不会报错。
   也就是说：Rust 的借用检查器在 zr 里还没有，类型系统只挡得住「给不可变引用写值」这一种。

4. **`str` 是 `{ptr, len}` 结构体，切片是 `[]const T`。**
   Rust 的 `&str` 是胖指针、`&[T]` 是 `&` + 长度；zr 把 `str` 做成了有两个字段的结构体，
   切片类型写成 `[]const T`（元素在后），而不是 Rust 的 `&[T]`。
   `[]mut T` 可用；数组 → 切片是隐式强转（Rust 里 `&arr` 也要显式配合）。
   切片没有越界检查，也没有借用检查。

5. **`print` / `println` 是函数，不是宏；格式化另走 `println_fmt`。**
   Rust 的 `println!("{} {}", a, b)` 在 zr 里要写成
   `println_fmt("{} {}", &[fmt_int(a), fmt_int(b)], 2)`。
   好处是 zr 没有宏卫生性问题；代价是写起来啰嗦，且格式规格只支持四种。

6. **标准库模块不带 `std::` 前缀，`Option` / `Result` 不在 prelude。**
   Rust 写 `std::option::Option`（或直接 `Option`）；zr 要写 `use core::Option;`，
   模块名是 `core` / `io` / `string` / `collections` / `error` / `mem` / `slice` / `net` / `async`。

7. **枚举的内存布局与 Rust 一致。**
   zr 的枚举是 `[判别式][载荷区]`，`Auto` 判别式按变体个数挑最小够用的整数，
   变体模式先比判别式再看载荷——这部分和 Rust 一样（泛型枚举按实例替换载荷类型）。

8. **窄浮点 `f8` / `f16` 是 zr 自创的。**
   `f16` 兼容 IEEE 754 binary16；`f8` 是 zr 自定义（1 符号 + 4 指数 + 3 尾数，偏置 7），
   范围很小（最大约 240，最小规格化数 2⁻⁶），Rust 没有对应类型。

9. **错误处理保留了 Zig 风格的 `T!E` 类型，但没保留运算符。**
   `fn f() -> i32!i32` 能写，`f()!` 的解包运算符解析不了；
   Rust 风格的 `?` 又只是占位实现（见下）。

10. **`Option` / `Result` 的方法与模式匹配都可用。**
    `is_some()` / `is_none()` / `is_ok()` / `is_err()` / `unwrap()` /
    `unwrap_or()` / `map()` 均返回正确结果；唯一例外是 `?` 运算符仍是占位实现
    （见 P0 #3）。

---

## 五、按优先级排序的待办缺口

> 排序依据：**影响面 × 静默出错的可能性**。P0 = 会产生错误结果但不报错，
> 任何一条都能让「看起来能跑的程序」给出错误答案。

### P0 — 静默出错，必须先修

1. **无符号 64 位高位运算错误。**
   `u64` 的 `>` `<` `>=` `<=` 用的是有符号条件码（`setl` / `setg`），
   `>>` 用的是算术右移（`sar`），`/` `%` 用的是有符号 `idiv`
   （`src/codegen/encoder.rs` 的 `condition_code()` 与移位/除法发射处）。
   于是 **≥ 2⁶³ 的 `u64` 比较、`>>`、`/`、`%` 全部给出错误结果**。
   实测（`-O0` 与 `-O2` 都错）：
   ```zr
   let a: u64 = 9223372036854775808;
   let b: u64 = 1;
   a > b            // 实测 false，应为 true
   a >> 1           // 实测 ≠ 4611686018427387904
   a / 2            // 实测 ≠ 4611686018427387904
   ```
   连带影响：`std::io::write_uint` 的 `while n > 0` 也用这个比较，
   所以 **`println_uint` 打印任何 ≥ 2⁶³ 的 `u64` 会输出空行**。
   影响面：所有无符号大数运算、哈希、时间戳、文件偏移。

2. **隐式窄浮点 → `f64` 的实参转换错误。**
   把 `f32` / `f16` / `f8` 直接传给 `f64` 形参时，位模式被当成 `f64` 解释，
   结果是 `0.0`（显式写 `as f64` 则正确）。
   ```zr
   fn takes_f64(x: f64) -> f64 { return x; }
   let a: f32 = 3.5;
   takes_f64(a)     // 实测 0.0，应为 3.5
   a as f64         // 实测 3.5，正确
   ```
   影响面：所有窄浮点跨精度的调用。

3. **`?` 运算符是占位实现。**
   后缀 `?` 在解析期被替换成解引用（`src/parser/expr.rs`）。
   既不传播错误也不取到值：`Err` 分支不会被选中，成功路径返回未初始化内存。
   影响面：所有 `?` 错误传播；`Result` 本身（`match` / 方法）可用。

### P1 — 明确报错或明显缺失的功能（能用不了，但至少看得见）

4. **`#[derive(...)]` 完全不可用**：结构体上报 `undefined name`，枚举上被静默忽略。
5. **静态变量 `static mut` 没有生成数据符号** → 链接失败。
6. **类型别名在 `let` 注解里不展开**：`type MyInt = i32; let m: MyInt = 5;`
   报 ``type mismatch: expected `i32`, found `type#2` ``（函数签名里的别名可用）。
7. **没有 `unsafe` 关键字**（README「指针与 unsafe」一节已过期）、
   **没有 `dyn` 关键字**（trait 对象不可用，`std::error` 因此导入不了）。
8. **没有借用检查 / 移动检查**（README「所有权系统」「借用检查」两节与实际不符）。
9. **`try` / `catch` / `throw` / `defer` 解析但不执行**：`defer` 的块从不运行，
    `throw` 既不抛也不中止 —— 建议先改成编译期明确报错。
10. **没有元数据 / 临时文件 API**：目录、路径、时间已可用（`std/fs.zr`、
    `std/path.zr`），但权限、时间戳、`tempfile` 仍缺。
11. **浮点 `Display` 在 |v| ≥ 2⁶⁴ 时输出 `.0`**（`std/io.zr` 的 `write_float`）。
12. **格式规格只支持 `{}` `{:?}` `{:x}` `{:X}`**，宽度 / 精度 / 对齐 / 填充会被原样打印。
13. **`println` 不检查参数类型**：`println(42)` 编译通过、运行段错误。
14. **闭包未实现**（README 已记录）：`closures are not implemented yet`。
15. **`i128` / `u128` 在 64 位目标上不可用**（需要寄存器对拆分）。
16. **泛型推断与约束的边界**：类型参数只从实参和期望返回类型推断，不看作法体
    （`let v = Vec::new(); v.push(1)` 推不出 `T`，需要 `let v: Vec<i32>` 注解）；
    类型路径上的 turbofish（`Option::<i32>::None`）不支持；`T: Trait` / `where`
    约束只解析不检查，实例化后按具体类型报"方法/字段缺失"。

---

## 附录：本次审计的验证环境

- 主机：macOS（x86_64-apple-darwin, Mach-O）
- 编译器：`cargo build` 产出的 `./target/debug/zr`（v0.8.0）
- 编译命令统一为：`./target/debug/zr --no-incremental <case>.zr -o <bin>`
  （必须带 `--no-incremental`，否则增量缓存会给出旧产物）
- 判定「已实现」的标准：程序**编译通过且运行结果与 Rust 一致**；
  判定「未实现」的标准：编译期明确报错，或运行结果与 Rust 不一致
  （后者一律在正文里标为 ❌ 并注明实测输出）
- 审计期间 `std/io.zr` 正被并行修改（新增了 `zr_seek` / `zr_fstat_size` /
  `zr_isatty` 运行时导出），第三章的文件能力反映的是**当时的在途状态**；
  其余章节不依赖该文件的改动。
