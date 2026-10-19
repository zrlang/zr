# zr — A Systems Programming Language

> 融合 Rust 的内存安全、Zig 的显式控制与 Python 的表达力，
> 一门为系统编程而生的现代编程语言。

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Version](https://img.shields.io/badge/version-0.7.0-blue.svg)](./Cargo.toml)
[![Build](https://img.shields.io/badge/build-passing-brightgreen.svg)]()

---

## 目录

- [语言概览](#语言概览)
- [核心特性](#核心特性)
- [快速开始](#快速开始)
- [语法速览](#语法速览)
- [语言特性详解](#语言特性详解)
- [模块与标准库](#模块与标准库)
- [编译与工具链](#编译与工具链)
- [编译器架构](#编译器架构)
- [设计哲学](#设计哲学)
- [路线图](#路线图)
- [贡献](#贡献)
- [许可证](#许可证)

---

## 语言概览

zr 是一门**静态类型、编译型、系统级**编程语言，设计目标：

| 维度 | zr 的设计 |
|------|-----------|
| 内存安全 | 所有权 + 借用 + 生命周期（类似 Rust） |
| 控制力 | 显式内存布局、`unsafe` 边界、手动内存操作（类似 Zig） |
| 表达力 | 类型推导、模式匹配、代数数据类型、闭包（类似 Rust/OCaml） |
| 可读性 | 缩进无关、统一语法、最小标点（类似 Python） |
| 并发 | 一等公民协程 + async/await + 通道（无数据竞争） |
| 元编程 | `comptime` 编译期求值、属性、泛型特化 |
| 运行时 | 可选最小运行时，零成本抽象 |
| 互操作 | C ABI 兼容，可直接链接 C 库 |

```rust
// hello.zr —— 你的第一个 zr 程序
use io::println;

fn main() -> i32 {
    println("Hello, zr!");
    return 0;
}
```

```bash
$ zr build hello.zr -o hello
$ ./hello
Hello, zr!
```

---

## 核心特性

### 1. 内存安全，无需垃圾回收

zr 采用**所有权（Ownership）+ 借用（Borrowing）+ 生命周期（Lifetimes）**三重机制，
在编译期保证内存安全，无运行时开销：

```rust
// 所有权转移（move）
fn take(s: String) { /* s 在这里被消费 */ }
let s = String::from_bytes("zr");
take(s);
// s 在此之后不再可用 —— 编译期禁止

// 借用：同一时刻只能有一个可变借用，或多个不可变借用
fn borrow(s: &String) -> usize { return s.len(); }
let len = borrow(&s);        // 不可变借用

// 可变借用
fn append(s: &mut String, c: u8) { s.push_byte(c); }
append(&mut s, b'!');
```

### 2. 代数数据类型（ADT）+ 模式匹配

```rust
pub enum Result<T, E> {
    Ok(T),
    Err(E),
}

pub enum Expr {
    Num(f64),
    Add(Box<Expr>, Box<Expr>),
    Var(String),
}

fn eval(e: &Expr) -> f64 {
    match e {
        Expr::Num(n)               => *n,
        Expr::Add(left, right)     => eval(left) + eval(right),
        Expr::Var(name)            => panic("unbound variable"),
    }
}

// 模式匹配支持：字面量、范围、或模式、解构、通配符
match status {
    200          => "OK",
    400..=499    => "Client Error",
    500..=599    => "Server Error",
    _            => "Unknown",
}
```

### 3. 泛型 + Trait + 关联类型

```rust
/// 一个可对容器元素应用映射的 trait
pub trait Mappable {
    type Item;
    type Output;

    fn map<F>(self, f: F) -> Self::Output
    where F: Fn(Self::Item) -> Self::Output;
}

impl<T, U> Mappable for Vec<T> {
    type Item = T;
    type Output = Vec<U>;

    fn map<F>(self, f: F) -> Vec<U>
    where F: Fn(T) -> U
    {
        let mut out = Vec::with_capacity(self.len());
        let mut i = 0;
        while i < self.len() {
            out.push(f(self.get(i)));
            i += 1;
        }
        out
    }
}
```

### 4. 一等公民协程与 async/await

```rust
/// 一个异步的 TCP 回声处理器
async fn echo(conn: &mut Connection) -> Result<(), IoError> {
    let mut buf = [0u8; 1024];
    loop {
        let n = await conn.read(&mut buf);
        if n == 0 { break; }
        await conn.write(&buf[0..n]);
    }
    Result::Ok(())
}

/// 并发地等待多个 Future
async fn fastest(a: Future<String>, b: Future<String>) -> String {
    await race(a, b)   // 谁先就绪就用谁
}
```

### 5. 编译期求值（comptime）

```rust
/// 编译期计算阶乘（递归、编译期循环、break 都可以用）
comptime fn fact(n: i32) -> i32 {
    if n <= 1 { 1 } else { n * fact(n - 1) }
}

const SIZE: i32 = fact(5);      // 在编译期求值为 120

/// 编译期求值的终止性由递归深度与步数预算保证；
/// 写一个不收敛的循环会让编译**报错退出**，而不是挂住。
///
/// 当前支持的构造：字面量、算术/位运算、比较、逻辑、`let` / `var` / `const`、
/// 赋值与复合赋值、`if`、`while`、`loop`、`break`、`continue`、`return`、
/// 数值转换，以及调用其它 `comptime fn`。
/// 不支持：`match`、结构体 / 数组 / 元组字面量、闭包、指针运算
/// （这些节点在 HIR 阶段尚未保留数据）。
```

### 6. C ABI 兼容

```rust
// 直接声明并调用 C 函数
extern "C" {
    fn printf(fmt: *const u8, ...) -> i32;
    fn strlen(s: *const u8) -> usize;
}

fn main() -> i32 {
    printf("hello from C!\n" as *const u8);
    return 0;
}
```

### 7. 显式错误处理（`?` 运算符）

```rust
fn read_config(path: &str) -> Result<Config, IoError> {
    let file = File::open(path)?;     // 出错时自动返回 Err
    let text = file.read_to_string()?;
    parse_config(text)
}
```

---

## 快速开始

### 环境要求

- Rust 工具链 ≥ 1.70（用于构建 zr 编译器本身）
- 宿主平台：Linux / macOS / Windows（x86-64 与 aarch64）
- 交叉编译时需要对应目标的 C 工具链（clang / as / lld-link / mingw）
- 编译嵌入式目标时需要对应的裸机工具链（`arm-none-eabi-gcc`、
  `riscv64-unknown-elf-gcc`，或支持这些目标的 clang）

### 构建编译器

```bash
git clone https://example.org/zr/zr.git
cd zr
cargo build --release
# 编译器位于 ./target/release/zr
```

### 编译一个 zr 程序

```bash
# 编译并链接为可执行文件
zr build hello.zr -o hello

# 指定优化级别
zr build app.zr -O2 -o app

# 只编译不链接（生成目标文件）
zr build lib.zr -c -o lib.o

# 交叉编译：目标三元组与 cargo --target 同名
zr app.zr --target aarch64-unknown-linux-gnu -o app-arm64
zr app.zr --target aarch64-apple-darwin -o app-macos-arm64
zr app.zr --target x86_64-pc-windows-msvc -o app.exe
zr app.zr --target wasm32-unknown-unknown -o app.wasm
zr app.zr --target wasm32-unknown-unknown --wat -o app.wat

# 嵌入式（裸机）交叉编译：直接产出可烧录的 ELF 映像
zr app.zr --target arm-none-eabi -o app.elf
zr app.zr --target riscv64-unknown-none -o app.elf

# 查看中间表示（调试用）
zr build program.zr --dump-ssa
```

### 运行测试

```bash
cargo test                 # 全部：单元测试 + tests/cases 下的语言用例
```

语言用例（`tests/cases/*.zr`）是**自检程序**：全部断言通过时 `main` 返回 0，
失败时返回失败断言的编号。宿主测试驱动会把每个用例在 `-O0` / `-O1` / `-O2` /
`-O3` / `-Oz` 下各编译并运行一遍——优化 pass 只要有一条改坏了语义，用例就会
在某个等级上红。

| 用例 | 覆盖 |
|------|------|
| `01_variables` … `11_if_else_chain` | 变量、标量、优先级、控制流、函数 |
| `12_comptime` | `comptime` 表达式折叠 |
| `13_casts` | 整数 / 浮点类型转换 |
| `14_spill` | 寄存器溢出 |
| `15_loops` | 循环 |
| `16_comptime_fn` | `comptime fn`、递归、编译期循环、`const` 右值内联 |
| `17_loop_break` | 循环出口的 φ（`break` / `continue` / 嵌套循环） |
| `18_spill_ops` | 溢出值参与每一种运算（含 `idiv` 内存操作数） |
| `19_float` | f64 算术、比较、取负、int<->f64 转换、浮点函数调用、循环累加、NaN 语义 |
| `20_f32` | f32 算术、比较、取负、int<->f32、f32<->f64、无注解 `let`、字面量位宽推断、NaN 语义 |
| `21_literal_suffixes` | 字面量类型后缀（`42u64` / `3.14f32` / `usize` …）影响推断，f32 舍入 |
| `22_call_preserve` | 跨调用存活的值必须被保住（callee-save / 形参并行搬运） |
| `23_strings` | 字符串字面量落 `.rodata`、去重、按地址比较、`str` 作实参跨调用 |
| `24_pointers` | 字符串按字节索引 `s[i]`、取址 `&x`/`&mut x`、解引用读写 `*p`/`*p = v`（可变性校验）、指针索引 `p[i]`、指针算术 `p + n`/`n + p`/`p - q`、地址被取的局部量与循环/φ 交互、i8 符号扩展 |
| `25_match` | 运行期 `match`：字面量臂（含负数 `-5`、bool 双字面量穷尽、char）、或模式 `1 \| 2`、guard `if`、绑定臂 `v` |
| `26_comptime_struct` | 编译期结构体字面量、字段读写（`p.x = v`）、`comptime Point { .. }.x`、嵌套与 AST/HIR 两条求值路径 |

除此之外，`tests/zr_language.rs` 里还有一个**只在宿主目标**跑的用例
`extern_c_write_prints_string_literal`：声明 `extern "C" { fn zr_write(...); }`
并通过运行时把字符串字面量写到标准输出。它不进 `tests/cases/`，因为自行构造的
ELF（交叉、无平台链接器）不带运行时，`zr_write` 会是未定义符号。

---

## 语法速览

### 变量与类型

```rust
// 不可变绑定（默认）
let x: i32 = 42;
let y = 3.14;              // 类型推导为 f64

// 可变绑定
var counter: i32 = 0;
counter += 1;

// 常量（编译期）
const PI: f64 = 3.141592653589793;

// 静态变量
static mut GLOBAL: i32 = 0;
```

### 基础类型

| 类别 | 类型 | 大小 |
|------|------|------|
| 有符号整数 | `i8`, `i16`, `i32`, `i64`, `i128`, `isize` | 1/2/4/8/16/ptr 字节 |
| 无符号整数 | `u8`, `u16`, `u32`, `u64`, `u128`, `usize` | 同上 |
| 浮点 | `f16`, `f32`, `f64` | 2/4/8 字节 |
| 布尔 | `bool` | 1 字节 |
| 字符 | `char` | 4 字节（Unicode 标量） |
| 指针 | `*const T`, `*mut T` | ptr 字节 |
| 引用 | `&T`, `&mut T` | ptr 字节 |

数字字面量可带类型后缀直接确定类型：`42u64`、`255u8`、`7usize`、`3.14f32`、
`2.0f64`。后缀须与字面量相符（整数后缀不能用在浮点字面量上，反之亦然）。
无后缀时按上下文推断，默认 `i32` / `f64`。

### 控制流

```rust
// if 是表达式
let max = if a > b { a } else { b };

// while
while !done {
    step();
}

// for-in（通过迭代器）
for i in 0..10 {
    println_int(i);
}

// loop + break/continue
loop {
    let n = next();
    if n == 0 { break; }
    if n < 0 { continue; }
}

// 带值的 break
let result = loop {
    if ready() { break compute(); }
    yield_now();
};
```

### 函数

```rust
/// 计算两点之间的距离
pub fn distance(p1: Point, p2: Point) -> f64 {
    let dx = p1.x - p2.x;
    let dy = p1.y - p2.y;
    return (dx * dx + dy * dy).sqrt();
}

// 多返回值（通过元组）
fn divmod(a: i32, b: i32) -> (i32, i32) {
    return (a / b, a % b);
}

// 命名参数（规划中）
fn create_window(width: u32, height: u32, title: String) -> Window {
    ...
}

// 可变参数（通过切片）
fn sum(values: []const i32) -> i32 { ... }
```

### 结构体

```rust
// 元组结构体
pub struct Point(f64, f64);

// 具名字段结构体
pub struct Rectangle {
    pub width:  f64,
    pub height: f64,
}

// 方法
impl Rectangle {
    pub fn area(self) -> f64 {
        self.width * self.height
    }

    pub fn scale(self: &mut Rectangle, factor: f64) {
        self.width  *= factor;
        self.height *= factor;
    }
}

// 结构体更新语法
let bigger = Rectangle { width: 100, ..smaller };
```

### 枚举

```rust
// 简单枚举
pub enum Color { Red, Green, Blue }

// 带载荷的枚举（和 Rust 一样是 tagged union）
pub enum Value {
    Int(i64),
    Float(f64),
    Text(String),
    List(Vec<Value>),
}

// 内存布局控制
#[repr(u8)]
pub enum Status {
    Pending = 0,
    Running = 1,
    Done    = 2,
}

#[repr(C)]
pub struct Packet { ... }   // C 兼容布局
```

### 模式匹配

```rust
match value {
    // 字面量
    0            => "zero",
    // 范围
    1..=10       => "small",
    // 或模式
    12 | 24 | 36 => "dozen",
    // 解构
    Point(x, y)  => "at ({x}, {y})",
    // 守卫
    n if n > 100 => "big",
    // 通配符
    _            => "other",
}
```

### 错误处理

```rust
// Result 类型
pub fn parse(s: &str) -> Result<i32, ParseError> { ... }

// ? 运算符：自动传播错误
fn process(path: &str) -> Result<Data, Error> {
    let file = File::open(path)?;       // 若 Err，直接 return
    let text = file.read_to_string()?;
    let data = parse(&text)?;
    Result::Ok(data)
}

// 提供上下文
let config = read_config("app.conf").context("failed to read config")?;

// 组合子
let value = parse(input)
    .map(|v| v * 2)
    .unwrap_or(0);
```

### 泛型

```rust
// 泛型函数
fn swap<T>(a: &mut T, b: &mut T) {
    let tmp = *a; *a = *b; *b = tmp;
}

// 泛型约束（trait bound）
fn largest<T: Ord + Copy>(slice: []const T) -> T {
    let mut m = slice[0];
    for item in slice[1..] {
        if item > m { m = item; }
    }
    m
}

// where 子句
fn process<T>(value: T) -> T
where T: Clone + Default
{
    ...
}

// 关联类型
pub trait Iterator {
    type Item;
    fn next(self: &mut Self) -> Option<Self::Item>;
}
```

### 闭包

```rust
// 闭包捕获
let factor = 3;
let multiply = |x: i32| x * factor;

// 高阶函数
fn apply_twice<T>(x: T, f: fn(T) -> T) -> T {
    f(f(x))
}

// 闭包作为参数（通过 trait）
fn call_with<T>(value: T, op: impl Fn(T) -> T) -> T {
    op(value)
}
```

### 模块与可见性

```rust
// 导入
use std::io::{println, print_fmt};
use std::collections::HashMap;

// 重命名
use std::string::String as StringBuilder;

// 可见性
pub struct PublicStruct {        // 公开
    pub field: i32,             // 公开字段
    internal: i32,              // 私有（默认）
}

pub(crate) fn crate_local() {}  // 仅当前 crate 可见
pub(super)  fn parent_only() {} // 仅父模块可见
```

### 属性

```rust
#[inline(always)]               // 强制内联（即使函数体不止一个块）
fn hot_path() { ... }

#[inline(never)]                // 禁止自动内联
fn cold_path() { ... }

#[link(name = "c", kind = "static")]  // 链接指令
extern "C" { ... }

#[test]                         // 标记为测试
fn test_basic() { ... }

#[repr(C, packed)]              // 布局控制
struct Header { ... }

#[derive(Debug, Clone, Copy)]   // 自动派生
struct Point { x: f64, y: f64 }
```

### 异步

```rust
// async 函数返回 Future
async fn fetch(url: &str) -> Result<Response, Error> {
    let conn = await connect(url)?;
    let req  = await conn.send_request()?;
    await req.read_response()
}

// 并发组合子
let (a, b) = await join(task1, task2);          // 等待全部
let first  = await race(task1, task2);          // 等待最快
let all    = await try_join_all(tasks);         // 任一失败即失败

// 通道
let (tx, rx) = channel<i32>(16);
spawn(producer(tx));
let value = await rx.recv();
```

---

## 语言特性详解

### 所有权系统

zr 的所有权规则与 Rust 一致，但诊断信息更友好：

1. **每个值有且仅有一个所有者**
2. **值离开作用域时自动释放**（调用析构器）
3. **赋值或传参会转移所有权**（除非类型实现了 `Copy`）
4. **可以通过借用临时共享访问**

```rust
let s1 = String::from_bytes("hello");
let s2 = s1;              // 所有权转移到 s2
// println(s1);           // ❌ 编译错误：s1 已失效

let s3 = s2.clone();      // 显式克隆，深拷贝
```

### 借用检查

```rust
let mut data = vec![1, 2, 3];

let r1 = &data;           // 多个不可变借用 ✅
let r2 = &data;           // ✅

let r3 = &mut data;       // ❌ 编译错误：已有不可变借用
```

借用规则：
- 任意时刻可以有**一个或多个不可变借用**，**或**一个可变借用
- 借用必须**总是有效**（生命周期不能悬垂）

### 生命周期

```rust
// 函数签名中的生命周期标注
fn longest<'a>(x: &'a str, y: &'a str) -> &'a str {
    if x.len() > y.len() { x } else { y }
}

// 结构体中的生命周期
struct Ref<'a, T> {
    value: &'a T,
}
```

### trait 与动态分发

```rust
// 静态分发（单态化，零开销）
fn print_all<T: Display>(items: &[T]) {
    for item in items { item.display(STDOUT_FD); }
}

// 动态分发（通过 trait 对象，小开销）
fn log_any(writer: &dyn Write, msg: &str) {
    writer.write(msg);
}
```

### 切片（Slice）

```rust
// 切片是数组的视图，避免拷贝
fn first_word(s: &str) -> &str {
    let bytes = s.as_bytes();
    match bytes.find_byte(b' ') {
        Option::Some(pos) => &s[0..pos],
        Option::None       => s,
    }
}
```

### 指针与 unsafe

```rust
// 原始指针：不受借用规则约束，需 unsafe 块
unsafe {
    let ptr = 0x1000 as *mut u8;
    *ptr = 42;             // 危险操作，开发者自行保证安全
}

// 与 C 互操作
extern "C" fn callback(data: *mut void) -> i32 {
    unsafe { *(data as *mut i32) = 1; }
    0
}
```

---

## 模块与标准库

zr 的标准库完全用 zr 自身编写（自举），通过 `extern "C"` 链接系统调用。

### 模块组织

```
std/
├── core.zr         // 基础类型：Option/Result/trait/panic/数值工具
├── mem.zr          // 内存管理：alloc/dealloc/memcpy/memmove/Layout
├── io.zr           // 格式化输出与基础 IO
├── string.zr       // 可增长 UTF-8 字符串
├── slice.zr        // 切片工具、迭代器、高阶函数
├── collections.zr  // Vec<T> 动态数组
├── async.zr        // 协程、Future、Executor、通道
├── net.zr          // TCP / UDP 套接字
├── simd.zr         // 向量类型与内建函数
└── error.zr        // 错误处理、回溯、断言
```

### 使用标准库

```rust
// 引入整个模块
use std::io;
io::println("hello");

// 选择性导入
use std::collections::Vec;
use std::string::String;

// 通配符导入
use std::math::*;
```

### `std::core` — 基础类型

```rust
use std::core::{
    Option, Result,        // 可选值与结果
    Eq, Ord, Copy, Clone, // 核心 trait
    Display, Debug,       // 格式化 trait
    panic, assert,        // 诊断工具
    min, max, clamp,      // 数值工具
};
```

### `std::io` — 输入输出

```rust
use std::io::{
    println, print, eprintln,  // 打印
    print_int, print_uint,     // 数值
    print_bool, print_char,
    BufWriter,                 // 缓冲写入
    write_bytes, write_byte,
};
```

### `std::string` — 字符串

```rust
use std::string::String;

let mut s = String::new();
s.push_byte(b'h');
s.push_str(&other);
s.push_int(42);

let len = s.len();
let sub = s.substr(0, 5);
let found = s.find_byte(b'x');
```

### `std::collections` — 容器

```rust
use std::collections::Vec;

let mut v = Vec::new();
v.push(1);
v.push(2);
let first = v.get(0);
v.insert(1, 99);
v.remove(0);
v.sort();
```

### `std::async` — 异步运行时

```rust
use std::async::{
    Executor, Task,
    spawn, yield_now, sleep,
    channel, Sender, Receiver,
    Join2, Race,
};

let mut ex = Executor::new();
ex.spawn(worker_task());
ex.block_on(main_future());
ex.run();
```

### `std::error` — 错误处理

```rust
use std::error::{
    Error, Backtrace,
    report_fatal, report,
    with_context,
};
```

---

## 编译与工具链

### 命令行接口

```
zr [OPTIONS] <INPUT.zr>

SUBCOMMANDS:
  lsp                    启动语言服务器（stdio）
  pkg <SUBCOMMAND> ...   包管理器（转交给 zr pkg）
  clean                  清空增量编译缓存
  fmt [--check] <FILE>   格式化源码

OPTIONS:
  -o <FILE>              输出文件名
  -O0..-O3, -Oz          优化级别
  -c                     只编译不链接（产出目标文件）
  --target <TRIPLE>      目标三元组（默认：宿主平台）
  --wat                  输出 WebAssembly 文本（wasm32 专用）
  --no-incremental       关闭增量编译
  -g, --debug            生成 DWARF 调试信息
  -v, --verbose          打印缓存命中等诊断信息
  --dump-tokens          dump 词法分析结果
  --dump-ast             dump AST
  --dump-hir             dump HIR
  --dump-mir             dump MIR
  --dump-ssa             dump SSA
  --dump-ssa-after       dump 优化后的 SSA
  --dump-asm             dump 机器码
  --emit-llvm            输出 LLVM IR 文本
  -h, --help             显示帮助
  -V, --version          显示版本
```

### 编译模型

zr 采用单态化泛型（monomorphization），与 Rust/C++ 类似：

- 泛型函数在编译期为每个具体类型生成一份代码
- 零成本抽象：trait 约束在编译期解析，无运行时开销
- 增量编译：默认开启，源文件与所有模块的内容指纹都未变时复用上次产物

### 链接

```bash
# 静态链接（默认）
zr build app.zr -o app

# 链接 C 库
zr build app.zr -lc -lm -o app

# 链接其他 zr 编译的目标文件
zr build main.zr -o main.o -c
zr build lib.zr -o lib.o -c
zr link main.o lib.o -o app
```

---

## 编译器架构

zr 编译器采用经典的多阶段架构：

```
源码 (.zr)
   │
   ▼ ① Lexer ──────── 词法分析（基于生成器的有限状态机）
   │
   ▼ ② Parser ─────── 语法分析（递归下降 + Pratt 表达式）
   │                   生成 AST
   ▼ ③ Type Checker ── 类型检查 + 类型推导（Hindley-Milner 子集）
   │   ├─ Name Resolution     名称解析
   │   ├─ HIR Construction    HIR 构建
   │   ├─ Type Inference      类型推断/统一
   │   ├─ Trait Solving       trait 约束求解
   │   ├─ Borrow Checking     借用检查（NLL）
   │   ├─ Exhaustiveness      模式穷尽性检查
   │   └─ Closure Analysis    闭包捕获分析
   ▼ ④ MIR ────────── 中级表示（带基本块与终结器）
   │                   控制流脱糖、λ提升
   ▼ ⑤ SSA ────────── 静态单赋值形式
   │   ├─ 支配树 + 支配边界
   │   ├─ Cytron φ 节点插入
   │   └─ 寄存器重命名
   ▼ ⑥ Optimizer ──── 优化（可配置）
   │   ├─ 常量折叠
   │   ├─ 死代码消除（DCE）
   │   ├─ 公共子表达式消除（CSE）
   │   ├─ 全局值编号（GVN）
   │   ├─ 内联
   │   ├─ 循环优化
   │   └─ 窥孔优化
   ▼ ⑦ Code Generator ─ 指令选择 + 寄存器分配
   │   ├─ 指令选择（树覆盖）
   │   ├─ 线性扫描寄存器分配
   │   ├─ 栈帧布局
   │   └─ 汇编/机器码发射
   ▼ ⑧ Container Writer ─ ELF / Mach-O / PE-COFF / wasm
       └─ 平台链接器（ld · cc · lld-link）完成最终链接
```

### 目标平台

| 目标三元组 | 指令后端 | 容器格式 | 运行时 | 链接方式 |
|------------|----------|----------|--------|----------|
| `arm-none-eabi` | ARM32（A32） | ELF32 裸机映像 | `runtime/rt_arm_baremetal.s`（semihosting） | 无需链接器 |
| `thumbv7em-none-eabi` | ARM32（Thumb-2） | ELF32 裸机映像 | `runtime/rt_arm_baremetal.s`（semihosting） | 无需链接器 |
| `riscv32-unknown-none` | RV32IMAC | ELF32 裸机映像 | `runtime/rt_riscv_baremetal.s`（semihosting） | 无需链接器 |
| `riscv64-unknown-none` | RV64IMAC | ELF64 裸机映像 | `runtime/rt_riscv_baremetal.s`（semihosting） | 无需链接器 |
| `x86_64-unknown-linux-gnu` | x86-64 | ELF | `runtime/rt_linux_x86_64.s`（裸系统调用） | `ld -static` 或 gcc |
| `aarch64-unknown-linux-gnu` | AArch64 | ELF | `runtime/rt_linux_aarch64.s`（裸系统调用） | `ld -static` 或 gcc |
| `x86_64-apple-darwin` | x86-64 | Mach-O | `runtime/rt_macos_x86_64.s`（libSystem） | `cc` / `clang` |
| `aarch64-apple-darwin` | AArch64 | Mach-O | `runtime/rt_macos_arm64.s`（libSystem） | `clang -target` |
| `x86_64-pc-windows-msvc` | x86-64 | PE/COFF | `runtime/rt_windows_x86_64.s`（kernel32） | `lld-link` / mingw |
| `aarch64-pc-windows-msvc` | AArch64 | PE/COFF | `runtime/rt_windows_arm64.s`（kernel32） | `lld-link` / mingw |
| `wasm32-unknown-unknown` | 栈式字节码 | wasm | 模块导入（`zr.*`） | 无需链接器 |
| `wasm32-wasi` | 栈式字节码 | wasm | 模块导入（`zr.*`） | 无需链接器 |

要点：

- **启动约定按平台不同**：Linux 的 `_start` 直接用系统调用退出进程
  （不依赖运行时）；macOS / Windows 的 `_start` 调用运行时的 `zr_exit`
  （分别转发到 libc 与 `ExitProcess`）；**裸机**的 `_start` 调用 `main`
  后停在自循环（没有"退出进程"，靠调试器或复位接管）。
- **裸机镜像没有 Program Header**：`e_entry` 直接指向 `_start`，节地址
  从 `0x08000000`（Cortex-M 向量表）起算，烧录工具 / 仿真器按节加载即可。
  ARM 有 A32（`arm-none-eabi`）与 Thumb-2（`thumbv7em-none-eabi`）两套独立
  后端，除法都用 `sdiv`/`mls`，因此需要 ARMv7-R/A（M 核没有硬件除法）。
- **RISC-V 没有条件执行**：比较结果用 `seqz`/`slt` 显式物化；
  除法余数固定写回 `a0`，需要一次搬运；常量用 `lui`+`addi` 拼装。
- **运行时按目标分别构建**：`build.rs` 为每个可用目标产出
  `libzrrt-<平台>.a`，编译器链接用户程序时按目标三元组自动选取；
  缺少交叉工具链时该目标会被安静跳过（编译器本身不依赖运行时）。
- **`-c` 产出目标文件**：三种原生容器都可直接交给平台链接器；
  wasm 没有可重定位对象，`-c` 与链接等价。

### 设计参考

| 组件 | 参考来源 |
|------|----------|
| MIR → LLVM/Cranelift | Rust (`rustc_codegen_llvm`) |
| 自带后端与交叉编译 | Zig |
| 字节码虚拟机 | Python (CPython) |
| Hindley-Milner 类型推导 | ML / Haskell / Rust |
| 借用检查器（NLL） | Rust |
| 线性扫描寄存器分配 | Java HotSpot / LLVM |
| C 兼容 ABI | Zig / C2 |

---

## 设计哲学

### 1. 显式优于隐式
每个内存操作、所有权转移、错误处理都清晰可见。

### 2. 零成本抽象
高层语法糖在编译期完全消除，运行时表现等同于手写 C。

### 3. 内存安全是默认
不牺牲安全性换取性能——所有权系统在编译期保证安全。

### 4. 渐进式复杂度
从 `let x = 1` 到协程、泛型、编译期元编程，按需深入。

### 5. 工具链即语言的一部分
编译器、格式化器、文档生成器、包管理器是同一整体。

### 6. 与 C 共存
不是"取代 C"，而是"更好地使用 C"——无缝链接任何 C 库。

---

## 路线图

### v0.5.0（当前） ✅
- [x] 词法分析器
- [x] 递归下降 + Pratt 解析器
- [x] HIR + Hindley-Milner 类型推导
- [x] trait 系统 + 约束求解
- [x] 借用检查器框架
- [x] MIR / SSA 降级
- [x] 优化 pass 框架
- [x] x86-64、AArch64、ARM32、RISC-V 32/64 代码生成
- [x] ELF32 / ELF64 / Mach-O / PE-COFF / wasm 输出
- [x] Linux / macOS / Windows × x86-64 / aarch64 + 裸机 ARM / RISC-V + wasm32
- [x] 标准库（core / io / string / collections / async / error）

### v0.6.0 ✅

- [x] `comptime` 编译期求值
  - [x] `comptime fn`：递归、编译期循环、`break`、复合赋值
  - [x] `const` 右值内联（`const SIZE: i32 = fact(5)`）
  - [x] 结构体字面量与字段访问（构造、读字段、编译期改写 `p.x = v`）
  - [x] 终止性双闸：递归深度上限 + 求值步数预算
- [x] 属性系统
  - [x] `#[inline(always)]` / `#[inline(never)]` 驱动内联决策
  - [x] `#[repr(C)]` / `#[repr(uN)]` / `#[repr(packed)]` 布局
  - [x] `#[link(name, kind)]` 传递给链接器
  - [x] `#[cfg(...)]` 条件编译、`#[allow]` / `#[deny]` lint 级别
  - [x] `#[derive(...)]` 生成默认 impl、`#[deprecated]` 使用警告
  - [x] `#[test]` 测试桩
- [x] SSA 构造：迭代支配边界插 φ（Braun et al.）+ DFS 重命名
  - [x] 循环头的隐式合流、循环出口的合流（`break` 路径）
  - [x] 关键边拆分，使 φ 的并行拷贝按边发射
- [x] 寄存器分配：CFG 活跃性分析 + 线性扫描 + 栈溢出
  - [x] 回边活跃区间延长；跨调用存活的值限定 callee-save
  - [x] 溢出路径覆盖除法 / 取模 / 移位 / 比较的每一个操作数位置
- [x] x86-64 f64 指令选择（SSE2）
  - [x] 浮点常量、算术（`addsd`/`subsd`/`mulsd`/`divsd`）、取负（`xorpd` 翻符号位）
  - [x] 浮点比较（`ucomisd` + 交换操作数 + `setnp`，NaN 语义符合 IEEE754）
  - [x] `cvtsi2sd` / `cvttsd2si` 的 int<->f64 转换
  - [x] 浮点走专用栈槽 + XMM 暂存，不占用整数寄存器分配器
  - [x] 浮点实参/返回值按 SysV ABI 走 `xmm0`-`xmm7`（与整数实参分别计数）
- [x] x86-64 f32 指令选择（SSE2）
  - [x] 浮点常量携带位宽（`Float(bits, width)`），按 32/64 位分别编码
  - [x] `movss`/`addss`/`subss`/`mulss`/`divss`、`xorps` 取负、`ucomiss` 比较（NaN 语义同 f64）
  - [x] `cvtsi2ss` / `cvttss2si` / `cvtss2sd` / `cvtsd2ss` 转换
  - [x] 类型推断修正：函数名类型、无注解 `let` 的推断变量解析、一元取负与二元字面量的位宽跟随
- [x] 字面量类型后缀贯通（`42u64` / `3.14f32` / `7usize` …）
  - [x] 词法器把后缀随 `IntLiteral` / `FloatLiteral` token 传递，并在词法层拒绝后缀与字面量不符的组合
  - [x] 类型推断据后缀定型；无后缀时沿用上下文推断
- [x] 优化 pass 完整化
  - [x] 常量折叠（整数 + 浮点 + 类型转换）
  - [x] SCCP 稀疏条件常量传播（含恒定分支折叠）
  - [x] 死代码消除、死块消除（删块后重编号）
  - [x] 块内 CSE 与全局值编号（覆盖 `Cast` / `Copy`）
  - [x] 窥孔优化、循环不变量外提、单块函数内联
- [x] 编译确定性：块号与 φ 编号不再依赖 `HashMap` 遍历顺序
- [x] 浮点类型贯通：`Binary` / `Call` 携带结果类型，后端据此选 GPR 还是 XMM 指令

### v0.7.0（进行中） 🚧
- [x] x86-64 f32 支持（常量携带位宽，走 `ss` 指令；见 v0.6.0）
- [x] AArch64 f32/f64 指令选择
  - [x] 浮点常量、算术（`fadd`/`fsub`/`fmul`/`fdiv`）、取负（`fneg`）
  - [x] 浮点比较（`fcmp` + `cset`，条件码 `mi`/`ls`/`gt`/`ge`/`eq`/`ne`，NaN 语义同 x86-64）
  - [x] `scvtf`/`ucvtf`/`fcvtzs`/`fcvtzu`/`fcvt` 的 int<->f32/f64、f32<->f64 转换
  - [x] 浮点走专用栈槽 + `d0`/`d1` 暂存，不占用整数寄存器分配器
  - [x] 浮点实参/返回值按 AAPCS64 走 `v0`-`v7`（与整数实参分别计数）
  - [x] 顺带修掉若干 AArch64 编码/分配 bug：`b.cond` 偏移量、零判定（`cmp x,x`）、
        `fneg` 基址、移位基址、`as` 转换（SBFM/UBFM）、取模改线性扫描分配
  - [x] AArch64 callee-save 寄存器（`x19`-`x28`）：跨调用存活的值不再被被调方覆盖
  - [x] 修复 x86-64 `emit_param_moves`：形参搬运改回并行拷贝（顺序搬运会踩掉尚未读取的源）
- [x] 修复 ARM32 一批"能编过但算错"：旋转立即数方向反了（`mov r,#300` 之类
      静默编错）、`emit_extend` 的 sxtb/uxth 基址、形参未钉在实参寄存器
      （分配顺序让短命值抢走 r0-r3，调用两侧来回搬错值）、第 5+ 个整数
      实参接通栈上传参区、大槽区 `sub sp, #1000+` 经 r12 兜底
- [x] 修复 ARM32 另一批"能编过但算错"：VFP 编码（`vldr`/`vstr` 的双精度
      槽偏移、数据处理的 `opc1`/减法位、`vneg`/`vcmp`/`vcvt` 的寄存器半位、
      `vmov` 核心↔VFP）、立即数移位（`asr rd,rm,#31` 的 imm5 在 [11:7]、类型在
      [6:5]）、int↔float 转换的取数路径（整数先 `vmov` 进 `s` 寄存器再 `vcvt`，
      结果再 `vmov` 回核心寄存器）；编码逐条对照 clang 钉进单测
- [x] ARM32 字符串字面量取址（`movw`/`movt` + `R_ARM_MOVW_ABS_NC`/`MOVT_ABS`，
      落 `.rodata`）、i64 除以 2 的幂常量的强度削减——`23_strings`/`24_pointers`
      在 `arm-none-eabi` 打通
- [x] RISC-V 后端的浮点指令选择（F/D 扩展，复用 AArch64 的"浮点槽"数据流
      模型）：算术 / 比较（IEEE754 无序取假）/ 取负、int↔f32↔f64 转换
      （`as` 按向零截断）、跨函数浮点实参（整数/浮点按 psABI 分别计数）
      经 fa0-fa7 与返回值 fa0、循环 φ 电位累加；19/20/21 全绿
- [x] ARM32 后端的浮点指令选择（VFP：`vldr`/`vstr`/`vadd`/`vsub`/`vmul`/`vdiv`/
      `vneg`/`vcmp`/`vcvt`，浮点槽不占整数寄存器分配器，实参/返回值按 AAPCS32
      走 `s0`-`s15`/`d0`-`d7` 与 `s0`/`d0`；浮点标记按常量、二元/一元、转换、
      调用返回值、`Copy`/`Phi` 双向传播迭代到不动点，`19_float`/`20_f32`
      均可交叉编译到 `arm-none-eabi`）
- [x] wasm 后端的浮点指令选择（`f32`/`f64` 常量与算术/比较/取负、转换用
      `f32.convert_i64_s`/`i64.trunc_f64_s`/`f32.demote_f64`/`f64.promote_f32`
      等，不再用取负占位；函数形参/返回值与局部变量按推断出的 f32/f64 位宽
      声明，`addf` 之类跨函数浮点调用发出 `f32.add` 而非 `i64.add`）
- [x] 修增量编译缓存键：`--wat` 与二进制产物此前共用同一指纹，编译过
      `--wat` 后再编同名输出会命中缓存拿到 WAT 文本；指纹现已纳入该开关
- [x] 字符串字面量落到 `.rodata`（去重、NUL 结尾，取址即取指针）
  - [x] 取址：x86-64 `lea reg,[rip+disp]`（`R_X86_64_PC32` / `X86_64_RELOC_SIGNED` /
        `IMAGE_REL_AMD64_REL32`）、AArch64 `adrp`+`add :lo12:`
        （`R_AARCH64_ADR_PREL_PG_HI21`+`ADD_ABS_LO12_NC` / `ARM64_RELOC_PAGE21`+`PAGEOFFSET12`）、
        RISC-V `lui`+`addi`（`R_RISCV_HI20`+`LO12_I`，REL 符号直引）
  - [x] `.rodata` 段归属：ELF 段索引 2、Mach-O `n_sect=2`、COFF 节号 2
  - [x] 字面量按内容去重，`==`/`!=` 比较指针（同内容共享同一地址），`str` 可作实参跨调用
  - [x] 裸机自构造 ELF 的 `.rodata` 布局：vaddr 与文件偏移同对齐（SECT_ALIGN），
        保证整文件平移可加载（`vaddr - off` 恒定）——页对齐会破坏平移不变性
- [x] `extern "C"` 声明接通运行时（`zr_write` / `zr_read` / `zr_exit` …）
  - [x] 外部函数声明并入函数表，参与类型检查与名称解析，调用点发未定义符号给平台链接器
  - [x] 可把字符串字面量交给 `zr_write` 输出（宿主目标经 `clang`+`libzrrt.a`）
  - [x] std 里的 `extern`（`std/core.zr`）随标准库编译接通
- [x] 指针 / 字符串的取址、解引用与索引（最小 SSA 内存模型）
  - [x] 新指令 `Load` / `Store` / `SlotAddr`（MIR + SSA），按位宽区分有无符号
        （x86-64 `movzx`/`movsx`/`movsxd`，AArch64 `ldrb`/`ldrsb`/`ldr`…）
  - [x] `&x` 取局部变量地址：被取地址的量固定到栈槽（x86-64 / AArch64 各一套
        槽位，与浮点槽同一帧），读写变成显式的 `Load`/`Store`
  - [x] `*p` / `*p = v` / `p[i]` / `p[i] = v`，索引按元素字节大小缩放
  - [x] `s[i]` 字符串按字节索引，结果为 `u8`
  - [x] 指针算术：`p + n` / `n + p` / `p - n` 按 pointee 大小缩放，`p - q` 得到
        元素个数（isize）；两个指针相加显式报错
  - [x] 可变性：`&x`（`*const T`）只读，写入（`*p = v` / `p[i] = v`）必须
        `&mut x` + `*mut T`；对字符串赋值（`.rodata` 只读段）同样在类型检查拒绝
  - [x] 浮点值的取址 / 解引用显式报错（后端不支持，避免"能编过但算错"）
  - [x] 解析修复：块状表达式（`if`/`while`/`for`/`loop`/`match`/`{ }`）不再作为
        二元运算的左侧——否则 `if c { }` 后跟 `*p = v` 会被误解析为乘法
  - [x] `str` 的真正切片表示（ptr+len）：字符串字面量携带长度，MIR/SSA 传递 ptr+len
- [x] `match` 的运行期语义（脱糖成 if/else 链）
  - [x] 之前 `match` 只过类型检查、运行期静默算错；现在 `classify(1)` 这类
        用例真的能对上分支
  - [x] 模式支持：字面量（整数 / bool / char，含负数 `-5`）、`_`、绑定臂
        `v`（多带 guard，guard / 臂体里的 v 内联为 scrutinee）、或模式
        `3 | 4`、guard `if`
  - [x] 穷尽性：要求尾 `_` 臂；bool 的两条字面量臂（true/false）视作穷尽，
        可无尾 `_`——否则"没匹配上"会取到 Unit 继续跑
  - [x] 语义限定（已知）：首匹配胜出；scrutinee 每臂重新求值（含副作用时
        会重复执行）；枚举 payload / tuple / 结构体 / 范围模式报"未支持"
  - [x] 或模式解析修复：早先解析器丢弃 `3 | 4` 的第二个分支（4 静默落进
        `_`，是"能编过但算错"）；现在收集为 `Pattern::Or` 展开为多条分支
  - [x] `comptime` 的 `match` 支持（脱糖为 if/else 后在编译期求值）
- [x] ARM32 / Thumb-2 / RISC-V 指针 / 内存访问（`Load`/`Store`/`SlotAddr`、
      指针算术、`s[i]`）、字符串字面量取址——`23_strings`/`24_pointers` 在
      `arm-none-eabi` / `thumbv7em-none-eabi` / `riscv32` 全部通过
      （A32 用 `movw`/`movt` + `R_ARM_MOVW_ABS_NC`/`MOVT_ABS` 落在 `.rodata`）
- [x] 寄存器溢出四大整数后端齐了：x86-64 / AArch64 / ARM32 /
      RISC-V64 都用线性扫描把装不下的值溢出到栈槽（AArch64 用 x16/x17 暂存、
      x18 兼 φ 中转；ARM32/Thumb-2 用 r9/r10/r12；溢出槽开在槽区下方）
- [x] Thumb-2（`thumbv7em-none-eabi`）后端补齐到与 A32 同级
  - [x] 寄存器溢出（`push {r4-r7,lr}` 帧 + 溢出/宽值/浮点/取址槽），
        形参按 AAPCS 钉进 r0-r3（多字长形参占连续两寄存器并落栈槽）
  - [x] 指针 / 内存访问：`ldr`/`ldrb`/`ldrsb`/`ldrh`/`ldrsh` 与
        `str`/`strb`/`strh`（`.w` 变体，Rt 在 [15:12]）、`s[i]`、指针算术
  - [x] 字符串字面量经 `movw`+`movt` 绝对取址对落到 `.rodata`（新重定位
        `R_ARM_THM_MOVW_ABS_NC`/`R_ARM_THM_MOVT_ABS`，自构造映像的就地回填
        与目标文件两条路径都覆盖）
  - [x] i64 多字长：加/减（`adds`/`adcs`/`subs`/`sbcs`）、乘（`umull` + 交叉项）、
        比较（高低字两级比较）、32↔64 转换（`asr #31` 符号扩展）、
        变量移位（`lsl`/`lsr`/`asr` 按 `cnt<32` 分两段，用函数内局部标签 +
        T1 条件分支拼接跨字进位/借位）、
        2 的幂常量除/取余（移位 / 掩码强度削减）
  - [x] 浮点 VFP：算术 / 取负 / 比较（`vcmp`+`vmrs`）/ 整数↔浮点↔单双互转 /
        常量装载；`w32` 与 `word` 两种 32 位写法分清（Thumb-2 高半字在前）
  - [x] 顺带修正 Thumb 一处静默算错：`ldr`/`str`（含 `sp` 相对）的
        Rt 字段是 [15:12] 而非 [11:8]；`sub.w sp` 大立即数经 r12 兜底；
        T1 条件分支（`ArmBranchThumb`，`instr_len == 2`）的 8 位半字偏移
        应落在 [7:0] 而非 [9:2]（原回填掩码会改写条件码）
  - [x] 26 个语言用例在 `-O0..-Oz` 全部交叉编译通过（本机无 ARM 模拟器，
        以编码/对象格式验证为准）
- [x] 更多架构与平台：wasm64、FreeBSD、Android 交叉编译打通；iOS 仅缺
      本机 SDK / 链接器（环境限制，非编译器缺口）
- [x] 32 位目标的多字长（i64）算术（ARM32 / Thumb-2 / RISC-V32 用双字 32 位
      指令对 + 栈槽实现 `add`/`sub`/`mul`/比较/转换，寄存器分配器区分
      32/64 位值的溢出单元；RISC-V32 另有通用 `divmod`/`shl`/`shr`，
      ARM32 与 Thumb-2 的 64 位变量移位也已补齐（`cmp n,#32` 分两段；
      ARM32 用 A32 条件执行后缀拼接，Thumb-2 用新增的函数内局部标签 +
      T1 条件分支绕开——编码逐条对照 clang 钉进单测）；
      ARM32 / Thumb-2 的除法支持 2 的幂常量（移位/掩码强度削减））
- [x] `comptime` 的结构体字面量：`HirLiteral::Composite` 保留字段名/值，
      `comptime fn` 内可构造结构体、读字段、编译期改写字段（`p.x = v`），
      也支持 `comptime Point { .. }.x` 直接取字段；AST 与 HIR 两条求值路径
      均覆盖（`ExprKind::Struct`/`Field` 现携带字段数据，不再被丢弃）
- [x] wasm 后端补齐结构化控制流与线性内存：`wasm32-wasi` 下 26 个语言用例
      经 Node 实际运行全部退 0（此前只能"编过"，运行时语义不完整）
  - [x] 控制流：新增 `block`/`loop`/`if`/`else`/`br`/`br_table`/`unreachable`；
        每函数在形参后预留一个 i32 `pc` 局部，用 `loop` + 嵌套 `block` +
        `br_table` 做 CFG 栈调度，回边 φ 复制落在各自前驱边上；函数尾补
        `unreachable` 让栈多态通过校验（此前 `Goto`/`Branch` 被静默忽略，
        函数只会顺着第一个块线性跑）
  - [x] 线性内存与数据段：`Load`/`Store`/`SlotAddr` 落成
        `i32/i64.loadN`/`storeN`（wasm32 地址先 `i32.wrap_i64`，小位宽再
        `extend_i32_s/u`）；字符串字面量与"被取地址的局部量"槽组成 Data
        段（起始 1024，字符串去重、NUL 结尾）——`23_strings`/`24_pointers`
        在 `wasm32-wasi` 真正跑通
  - [x] 整数比较结果（i32）统一 `i64.extend_i32_u` 扩展回 i64，修掉
        "比较结果写进 i64 局部"导致的 `WebAssembly.validate` 失败
  - [x] 一元浮点指令表整体错位修复（`f32.neg` = 0x8C、`f32.abs` = 0x8B …
        `f64.neg` = 0x9A、`f64.sqrt` = 0x9F；此前把 `f64.neg` 编成
        `f32.neg`，模块直接校验失败）；整数取负改用 `× -1`（原先把
        `x - 0` 当成 `0 - x`，负数静默变正）
  - [x] 变窄转换按目标位宽截断再扩展（`(x << (64-B)) >> (64-B)`，符号
        用 `shr_s`、零扩展用新增的 `i64.shr_u`），修掉 `321 as i8` 未取低
        8 位（此前只按 32 位 wrap+extend）
  - [x] Function 段每个本地函数用各自的类型下标（此前一律写成第一个本地
        函数的签名，导致校验按错误形参类型解读局部变量）
  - [x] wasm 单测补充：一元/二元操作码、Data 段位置与段序
- [x] 增量编译、多线程编译
  - [x] 按函数并行代码生成（指令选择 + 寄存器分配 + 栈帧布局），`ZR_THREADS` 环境变量控制线程数（默认 = 逻辑核数，上限 8）。
  - [x] 字符串字面量预先登记进 `.rodata`（确定性编号），各线程仅查表；片段按函数顺序合并，产物**逐字节**与串行一致。
  - [x] 函数顺序确定化：类型检查/ HIR 生成按 DefId 排序，修掉 HashMap 迭代导致的非确定性（连带修复 `comptime` 折叠需先看到函数定义的问题）。

### v0.8.0（进行中） 📋
- [x] `zr build` / `zr run` 子命令（类似 `go build` / `go run`）
- [x] `zr pkg` 注册中心协议与联网能力（搜索、发布、下载、添加 registry 依赖）
- [ ] 格式化器 `zr fmt` 风格定稿
- [ ] 语言服务器（LSP）补全与跳转
- [ ] 内联汇编
- [ ] SIMD 指令选择

### v1.0.0（目标） 🎯
- [ ] API 稳定
- [ ] 完整的异步运行时
- [ ] 自举（编译器用 zr 重写）
- [ ] 性能基准达标（接近 Rust）

---

## 与同类语言的对比

| 特性 | zr | Rust | Zig | Go | Python |
|------|----|------|-----|----|----|
| 内存安全 | ✅ 所有权 | ✅ 所有权 | ⚠️ 手动 | ✅ GC | ✅ GC |
| 无 GC | ✅ | ✅ | ✅ | ❌ | ❌ |
| 编译速度 | 🚧 | 慢 | 快 | 快 | — |
| C 互操作 | ✅ | ✅ | ✅ | ⚠️ cgo | ⚠️ ctypes |
| 协程 | ✅ 一等公民 | ⚠️ 宏/async | ❌ | ✅ | ⚠️ |
| 泛型 | ✅ trait | ✅ trait | ⚠️ comptime | ❌ | 鸭子类型 |
| 编译期求值 | ✅ comptime | ⚠️ const fn | ✅ | ❌ | ❌ |
| 错误模型 | ✅ Result | ✅ Result | ✅ ErrorUnion | ❌ panic | ❌ 异常 |
| 学习曲线 | 中 | 陡 | 中 | 平缓 | 平缓 |

---

## 示例项目

### FizzBuzz

```rust
use std::io::{println, print_int};

fn main() -> i32 {
    let mut i = 1;
    while i <= 100 {
        if i % 15 == 0 { println("FizzBuzz"); }
        else if i % 3 == 0 { println("Fizz"); }
        else if i % 5 == 0 { println("Buzz"); }
        else { print_int(i); println(""); }
        i += 1;
    }
    return 0;
}
```

### 快速排序

```rust
fn quicksort(slice: []mut i32) {
    if slice.len <= 1 { return; }
    let pivot = slice[slice.len - 1];
    let mut i = 0;
    let mut j = 0;
    while j < slice.len - 1 {
        if slice[j] <= pivot {
            swap(&slice[i], &slice[j]);
            i += 1;
        }
        j += 1;
    }
    swap(&slice[i], &slice[slice.len - 1]);
    quicksort(slice[0..i]);
    quicksort(slice[i + 1..slice.len]);
}
```

### 链表

```rust
pub enum List<T> {
    Nil,
    Cons(T, Box<List<T>>),
}

impl<T> List<T> {
    pub fn push(self, value: T) -> List<T> {
        List::Cons(value, Box::new(self))
    }

    pub fn length(self) -> usize {
        match self {
            List::Nil => 0,
            List::Cons(_, tail) => 1 + tail.length(),
        }
    }
}
```

---

## 贡献

欢迎贡献！请阅读 [CONTRIBUTING.md](./CONTRIBUTING.md) 了解流程。

### 开发工作流

```bash
# 克隆仓库
git clone https://example.org/zr/zr.git
cd zr

# 构建调试版本
cargo build

# 运行测试
cargo test

# 运行 zr 自带的测试套件
./target/debug/zr test tests/language_basics.zr

# 格式化
cargo fmt

# 静态检查
cargo clippy
```

### 项目结构

```
zr/
├── Cargo.toml              # Rust 项目配置
├── build.rs                # 为每个目标构建 libzrrt-<平台>.a
├── README.md               # 本文件
├── LICENSE                 # MIT 许可证
├── src/                    # 编译器源码（Rust）
│   ├── main.rs             # 入口与 CLI
│   ├── error.rs            # 错误类型
│   ├── attributes.rs       # 属性解析与语义
│   ├── comptime.rs         # 编译期求值（comptime）
│   ├── lexer/              # 词法分析
│   ├── parser/             # 语法分析
│   ├── typeck/             # 类型检查 + HIR
│   ├── mir/                # 中级表示
│   ├── ssa/                # SSA 构造（IDF φ 插入 + 重命名）
│   ├── opt/                # 优化 pass
│   ├── codegen/            # 指令选择、寄存器分配、容器格式
│   ├── modules.rs          # 模块加载与增量编译缓存
│   ├── fmt.rs              # 格式化器
│   ├── lsp.rs              # 语言服务器
│   ├── pkg.rs              # 包管理器
│   ├── json.rs             # JSON 解析（供 LSP / pkg 使用）
│   ├── zr0/                # 内建（bootstrap）函数
│   └── macro_system/       # 宏
├── runtime/                # 各平台启动代码与系统调用桩（汇编）
├── std/                    # 标准库（zr）
├── tests/                  # 语言测试套件（tests/cases/*.zr）
├── future/                 # 尚未实现特性的设计文档
└── examples/               # 示例程序
```

---

## 许可证

zr 以 MIT 许可证开源。详见 [LICENSE](./LICENSE)。

---

## 致谢

zr 的设计深受以下项目启发：

- **Rust** —— 所有权、借用、trait、HIR/MIR 设计
- **Zig** —— 显式控制、C 互操作、comptime
- **Python** —— 可读性、表达力
- **OCaml / Haskell** —— Hindley-Milner 类型系统
- **Go** —— 协程设计
- **C** —— 贴近硬件的哲学

---

<div align="center">
  <strong>zr — 让系统编程重新变得愉快</strong>
</div>
