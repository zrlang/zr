# zr — A Systems Programming Language

> 融合 Rust 的内存安全、Zig 的显式控制与 Python 的表达力，
> 一门为系统编程而生的现代编程语言。

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Version](https://img.shields.io/badge/version-0.5.0-blue.svg)](./Cargo.toml)
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
/// 编译期计算阶乘
comptime fn fact(n: i32) -> i32 {
    if n <= 1 { 1 } else { n * fact(n - 1) }
}

const SIZE: usize = fact(5);   // 在编译期求值为 120

/// 编译期反射：根据类型信息生成代码
fn make_default<T>() -> T {
    comptime match @typeInfo(T) {
        .Struct  => T::default(),
        .Int     => 0 as T,
        .Pointer => null,
        else     => unreachable(),
    }
}
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
- Linux x86-64（当前支持的目标平台）

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

# 交叉编译（规划中）
zr build app.zr --target aarch64-linux-gnu -o app-arm64

# 查看中间表示（调试用）
zr build program.zr --dump-ssa
```

### 运行测试

```bash
zr test                    # 运行 tests/ 目录下所有测试
zr test language_basics     # 运行指定测试
```

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
#[inline(always)]               // 强制内联
fn hot_path() { ... }

#[link(name = "c", kind = "static")]  // 链接指令
extern "C" { ... }

#[test]                         // 标记为测试
fn test_basic() { ... }

#[repr(C, packed)]              // 布局控制
struct Header { ... }

#[derive(Debug, Clone, Copy)]   // 自动派生（规划中）
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
├── string.zr        // 可增长 UTF-8 字符串
├── slice.zr        // 切片工具、迭代器、高阶函数
├── collections.zr  // Vec<T> 动态数组
├── async.zr        // 协程、Future、Executor、通道
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
zr <COMMAND> [OPTIONS] <INPUT>

COMMANDS:
  build <FILE>     编译并链接为可执行文件（默认）
  check <FILE>     仅类型检查，不生成代码
  run <FILE>       编译并立即运行
  test [NAME]      运行测试
  fmt [FILE]       格式化代码（规划中）
  doc [FILE]       生成文档（规划中）

OPTIONS:
  -o, --output <FILE>     输出文件名
  -O0..-O3, -Oz           优化级别
  -c                      只编译不链接
  --target <TRIPLE>       目标三元组
  --sysroot <DIR>         sysroot 目录
  --dump-tokens           dump 词法分析结果
  --dump-ast              dump AST
  --dump-hir              dump HIR
  --dump-mir              dump MIR
  --dump-ssa              dump SSA
  --dump-asm              dump 汇编
  --emit-llvm             输出 LLVM IR（规划中）
  -h, --help              显示帮助
  -V, --version           显示版本
```

### 编译模型

zr 采用单态化泛型（monomorphization），与 Rust/C++ 类似：

- 泛型函数在编译期为每个具体类型生成一份代码
- 零成本抽象：trait 约束在编译期解析，无运行时开销
- 增量编译（规划中）：仅重新编译变更的模块

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
   ▼ ⑧ ELF Linker ── 生成 Linux x86-64 可执行文件
```

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
- [x] x86-64 代码生成
- [x] ELF 输出
- [x] 标准库（core / io / string / collections / async / error）

### v0.6.0（计划中） 🚧
- [ ] 完整的真实指令选择（覆盖所有 IR 操作码）
- [ ] 真正的线性扫描寄存器分配实现
- [ ] 所有优化 pass 的完整实现
- [ ] `comptime` 编译期求值
- [ ] 属性系统完善

### v0.7.0（计划中） 📋
- [ ] 增量编译
- [ ] 交叉编译（AArch64 / RISC-V / WASM）
- [ ] 包管理器 `zr pkg`
- [ ] 格式化器 `zr fmt`
- [ ] 语言服务器（LSP）

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
├── README.md               # 本文件
├── LICENSE                 # MIT 许可证
├── src/                    # 编译器源码（Rust）
│   ├── main.rs             # 入口与 CLI
│   ├── error.rs            # 错误类型
│   ├── lexer/              # 词法分析
│   ├── parser/             # 语法分析
│   ├── typeck/             # 类型检查
│   ├── mir/                # 中级表示
│   ├── ssa/                # SSA 构造
│   ├── opt/                # 优化 pass
│   └── codegen/            # 代码生成
├── std/                    # 标准库（zr）
├── tests/                  # 语言测试套件
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
