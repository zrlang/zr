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
属性、断言内建、闭包这些设施编译器目前还不支持（见下方"尚未覆盖"）。

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

## 尚未覆盖（编译器还不支持）

Rust Book 的其余章节目前一个都测不了，因为对应特性尚未实现。列在这里
是为了让"测试通过"这个说法有明确边界，而不是暗示语言已经可用。

| Rust Book 章节 | 阻塞原因 |
|----------------|----------|
| 控制流：`loop` + `break` | `loop` 的回边降级还不完整，`break` 跳不出去 |
| 控制流：嵌套循环 | 外层循环体被当成直线代码，内层变量没有独立的 φ |
| 控制流：`for` | `for x in a..b` 的 `..` 表达式仍解析成标识符 |
| 函数：递归 | 缺少栈帧：调用者不保存被调用者用到的 callee-save 寄存器 |
| 所有权 / 借用 / 引用 / 切片 | 借用检查与引用语义未实现 |
| 结构体与枚举 | `struct` 能声明，但字段读取返回错值；`enum` 未实现 |
| 浮点 | 五个后端都没有浮点支持，遇到浮点字面量会**明确报错**（不再静默发射 0） |
| 字符串字面量 | 同上，遇到字符串字面量会明确报错 |
| 模式匹配（`match` / `if let` / 解构） | `match` 未降低到 MIR |
| 包管理与 Cargo | 标准库按需加载刚落地，模块系统还不完整 |
| 集合（Vector / String / HashMap） | `std` 里对应的模块仍编译不过 |
| 错误处理（`panic!` / `Result` / `?`） | 断言内建与 `Result` 未实现 |
| 泛型 / Trait / 生命周期 | 类型检查器没把类型参数绑定成类型 |
| 函数式（闭包 / 迭代器） | 闭包未实现 |
| 智能指针（`Box` / `Rc` / `RefCell`） | 未实现 |
| 并发（线程 / 通道 / `Mutex` / `Send`） | 未实现 |
| 面向对象 / 状态模式 | 依赖 `trait` 与 `enum` |
| 高级特性（`unsafe` / 关联类型 / 宏） | 仅 `asm!` / `intrinsic!` 能解析，代码生成未实现 |

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
for t in arm-none-eabi riscv32-unknown-none riscv64-unknown-none \
         x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
         x86_64-apple-darwin aarch64-apple-darwin \
         x86_64-pc-windows-msvc aarch64-pc-windows-msvc \
         wasm32-unknown-unknown wasm32-wasi; do
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
  其余四个后端（AArch64 / ARM32 / RISC-V / wasm）还按编号取模分配寄存器，
  一旦寄存器压力超限就会**明确报错**，而不是安静地编出算错的程序。
  把它们也改成真溢出是剩下的一块工作。

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

