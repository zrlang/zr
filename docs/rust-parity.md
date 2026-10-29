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
| `i128` / `u128` | 编译期报错 | ❌ | `zr: 128 位整数在当前目标上尚未实现：该目标只有 64 位寄存器，编译器暂时还不能把宽整数拆成寄存器对。现在生成的是静默截断的代码，所以直接报错。`（64 位目标上恒定失败） |
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
| `&[T]` / `&mut [T]` 切片 | `[]const T` / `[]mut T` | ✅ | 元素类型写在括号**后面**（zr 独有写法）。可变切片可写回原数组；**没有借用检查**，任何数组都能隐式转成 `[]mut T` |
| 数组 → 切片 强转 | 自动强转 | ✅ | `let s: []const i32 = arr;`、`f(arr)`（形参 `[]T`）都自动补 `(ptr, len)`；`arr[1..3]` / `arr[1..]` / `arr[..3]` / `arr[..]` / `arr[1..=3]` 均产生切片 |
| `(T, U)` 元组 | 可用 | 🟡 | 直接写元组类型可用；但**经类型别名后取 `.0` 失败**：``zr: no field `0` on type `type#75` `` |
| `struct` | 可用 | ✅ | 具名字段、元组结构体、嵌套字段、`impl` 方法、`impl Drop`、字段按值拷贝均正常 |
| `enum`（带载荷） | 可用 | ✅ | 判别式与载荷正常（含 `f64` 载荷、struct 载荷变体）；泛型枚举按实例单态化，`Option<T>` / `Result<T, E>` 的载荷也能正确读写 |
| `union` | 只能声明 | ❌ | `union U { i: i32 }` 能声明，但构造报 ``zr: `U` is not a struct ``；字段读写也没有安全的入口 |
| `*const T` / `*mut T` | 可用 | ✅ | 裸指针可用；`*p = v` 走指针语义 |
| `&T` / `&mut T` | 可用 | 🟡 | 语义上 `&T` 是只读指针（``zr: cannot assign through an immutable pointer (use `&mut` and a `*mut` type) ``），`&mut T` 可写；但**`let` 注解里的 `&T` 被解析成 `*const T`**：`let r: &i32 = &x;` → ``zr: type mismatch: expected `*const i32`, found `&i32` `` |
| `Option<T>` / `Result<T, E>` | 可用 | ✅ | 在 `std::core` 里；`is_some()` / `is_none()` / `is_ok()` / `is_err()` / `unwrap()` / `unwrap_or()` / `map()`、模式匹配与后缀 `?` 均正常 |
| 泛型函数 | 可用 | ✅ | 按调用点单态化（含递归泛型）；类型实参从实参 / 期望返回类型推断。泛型约束（`T: Ord`、`where`）在**单态化时**校验（`check_trait_bounds`，含超类型展开）；实例化后再按具体类型检查方法调用 |
| 泛型类型 | 可用 | ✅ | 泛型结构体 / 枚举 + `impl<T> Type<T>` 的方法均可用（`Vec<T>`、`Option<T>` 等）；字段与载荷类型按类型实参替换 |
| `trait` | 可用 | ✅ | `trait` + `impl Trait for Type` 正常（`std::core` 的 `Eq`/`Ord`/`Copy`/`Clone`/`Display` 都在用） |
| 关联类型 | 可用 | ✅ | `type Output;` + `type Output = i32;` 正常，`Self::Output` 能取到 |
| `impl` 块 | 可用 | ✅ | 关联函数、`self` / `&self` / `&mut self` 三种接收者、`Self` 类型均可 |
| `type` 别名 | 可用（含泛型） | ✅ | 在**使用点展开**：`let x: MyInt`、结构体字段、数组 / 指针、`Vec<MyInt>`、别名套别名、前向引用都正常；**泛型别名 `type V<T> = Vec<T>` 已支持**（`74_generic_alias.zr`，提交 `8d6c475`：保留参数并登记占位符，展平按实参替换） |
| 生命周期标注 `'a` | 解析后丢弃 | 🟡 | `&'a T`、`struct S<'a>` 能写，但类型检查阶段直接丢掉标注（`src/parser/ty.rs`），**没有任何借用/存活检查** |
| `dyn Trait` / trait 对象 | 骨架可用 | 🔧 | 词法+解析+类型检查三层已实现（`88_dyn_keyword.zr`：参数/局部注解里出现 `&dyn Error` 不再报错）；**完整 vtable 分派与 trait-object 转换仍在未实现状态**，调用 trait 方法不在此用例范围 |
| auto trait | 不支持 | ❌ | 无 auto trait 机制、无 blanket impl 语法 |
| `PhantomData<T>` | 不存在 | ❌ | `std::core` 里没有 `PhantomData`，也没有等价物 |

### 1.3 泛型实参在类型注解里的写法

| Rust 的写法 | zr 的现状 | 状态 | 说明 |
|------|------|------|------|
| `let x: Option<i32> = ...;` | 可用 | ✅ | 类型注解里的泛型实参保留，字段 / 载荷类型按实参替换 |
| `fn f(x: Option<i32>)` | 可用 | ✅ | 签名与函数体一致 |
| `Option::<i32>::None` | 可用 | ✅ | turbofish 后面继续吃路径段，类型实参挂在整条路径上：无载荷变体（`Option::<i32>::None`）、关联函数（`Box::<i32>::new(7)`）、元组结构体（`Wrap::<i32>(7)`）、结构体载荷变体（`Shape::<u8>::Circle { r: 7 }`）都可用。显式实参当期望类型喂给构造器；无上下文的 `Option::None` 仍需 `let` 注解 |

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
| `?` 运算符 | 可用 | ✅ | 对 `Result<T, E>` / `Option<T>` 生效：成功取载荷，失败从当前函数 `return Err(e)` / `return None`。返回类型必须是同一种枚举，错误类型必须一致（没有 `From` 转换） |
| `try` / `catch` | 明确报错 | ❌ | 语法能解析，类型检查报 `` `try` / `catch` is not implemented yet; use the postfix `?` operator ... `` |
| `throw` | 明确报错 | ❌ | 类型检查报 `` `throw` is not implemented yet; return `Err(...)` instead `` |
| `defer` | 明确报错 | ❌ | 类型检查报 `` `defer` is not implemented yet (the block would never run) ... `` |
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
| `#[derive(Clone, PartialEq, Eq, Hash, Default, ...)]` | 生成真实方法体 | 🟡 | `src/typeck/derive.rs` 在 AST 层生成 `Type__clone` / `__eq` / `__lt` / `__hash` / `__default`，与手写 `impl` 方法同形。结构体支持 `Clone/Copy/PartialEq/Eq/PartialOrd/Ord/Hash/Default`（含元组结构体、嵌套结构体）；枚举支持 `Clone/PartialEq/Eq/Hash`。`==`/`<` 等聚合比较改写成方法调用，**没 derive 就报错**。仍缺：`Debug` 只是标记（`{:?}` 不走它）、泛型类型 derive 报错、枚举 `Default`/`Ord` 报错。 |
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
| `print!` / `println!` 格式化 `{}` | `println_fmt(fmt, &[FormatArg], n)` | 🟡 | 支持 `{}`、`{:?}`、`{:x}`、`{:X}`、`{:b}`、`{:o}`、`{:e}`、`{:E}`、位置参数 `{0}`、`{{`/`}}` 转义，以及填充 / 对齐（`<` `>` `^`）、`+`、`#`、`0`、宽度、精度（`{:08.2}`、`{:*^10.3}`）、动态宽度 `{:1$}`、动态精度 `{:.*}` / `{:.1$}`；与 Rust 逐字符对比一致。**没有**命名参数 `{name}`，`{:#?}` 也只会输出一行（`#` 对 `{:?}` 被忽略） |
| `Display` 浮点格式 | 最短可回读十进制 | ✅ | `1.0`→`1`、`3.5`→`3.5`、`1.0/3.0`→`0.3333333333333333`、`0.1+0.2`→`0.30000000000000004`、`-0.0`→`-0`、`1e30`→`1000000000000000000000000000000`，与 Rust 一致（早先 ≥2⁶⁴ 会打成 `.0`，已修） |
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
   Rust 风格的 `?` 只对 `Result` / `Option` 生效，不作用于 `T!E`。

10. **`Option` / `Result` 的方法与模式匹配都可用。**
    `is_some()` / `is_none()` / `is_ok()` / `is_err()` / `unwrap()` /
    `unwrap_or()` / `map()`、模式匹配与后缀 `?` 均返回正确结果。

---

## 五、按优先级排序的待办缺口

> 排序依据：**影响面 × 静默出错的可能性**。P0 = 会产生错误结果但不报错，
> 任何一条都能让「看起来能跑的程序」给出错误答案。

### P0 — 静默出错，必须先修

（当前没有已知的 P0 问题。）

### P1 — 明确报错或明显缺失的功能（能用不了，但至少看得见）

1. **`#[derive(...)]` 的剩余缺口**（方法体本身已能生成，见 §2.5 那一行）：
   `Debug` 仍只是标记 trait（`{:?}` 不经过它）；泛型类型上的 derive 明确报错；
   枚举的 `Default` / `PartialOrd` / `Ord` 明确报错；字段类型缺少所需 trait 时
   报的是整个类型的方法检查失败，尚未精确指向字段。可接受的 derive 名：
   `Debug` `Clone` `Copy` `PartialEq` `Eq` `PartialOrd` `Ord` `Hash` `Default`；
   其它名字报 `cannot derive ...`。
2. **`unsafe` 只是骨架**（`unsafe { ... }` 块可解析和通过类型检查，`85_unsafe.zr`；
   当前不改变语义，按块表达式透传）；**`dyn` 只是骨架**（词法+解析+类型检查
   已实现，`88_dyn_keyword.zr`；vtable 分派未完工，trait-object 转换受限，
   `std::error` 因此仍难导入）。
3. **没有借用检查 / 移动检查**（README「所有权系统」「借用检查」两节与实际不符）。
4. **没有元数据 / 临时文件 API**：目录、路径、时间已可用（`std/fs.zr`、
    `std/path.zr`），但权限、时间戳、`tempfile` 仍缺。
5. **格式规格只剩两处没做**：命名参数 `{name}` 与多行 Debug `{:#?}`
   （`#` 对 `{:?}` 目前被忽略，输出的是一行）。其余（`{}` `{:?}` `{:x}`
   `{:X}` `{:b}` `{:o}` `{:e}` `{:E}`、位置参数、填充 / 对齐 / `+` / `#` /
   `0` / 宽度 / 精度、动态宽度 `{:1$}`、动态精度 `{:.*}` / `{:.1$}`）
   已与 Rust 逐字符对齐。
6. **闭包只支持"定义处立即调用"**：`(|x| x + 1)(5)` 会在调用点内联展开，
   捕获变量复用外层 local。把闭包存进变量 / 当实参 / 返回会报
   `closures can only be invoked immediately where they are defined`。
7. **`i128` / `u128` 在 64 位目标上不可用**（需要寄存器对拆分）。
8. **泛型推断的边界**：类型参数只从实参和期望返回类型推断，不看作法体
   （`let v = Vec::new(); v.push(1)` 推不出 `T`，需要 `let v: Vec<i32>` 注解）。
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
- 审计期间 `std/io.zr` 正被并行修改（新增了 `zr_seek` / `zr_fstat_size` /
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
   没实现，`std/io.zr` 里 7 形参的 `emit_arg` 宽度恒为 0，所有填充 / 对齐 / 零填充
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
