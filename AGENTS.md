# AGENTS.md — zr 编译器贡献指南

zr 是一门系统编程语言（Rust/Zig 混合取向），编译器用 Rust 写、**零第三方依赖**。
本文件给在代码库里工作的 AI agent 看，是操作约定与"哪些文档可信"的清单。
项目入口与功能全景见根目录 `CLAUDE.md`。

> 最近更新：2026-10-25。工作树上有**未提交的在途改动**（见下文「在途工作」），
> 那是继续实现的第一现场。

---

## 0. 先读这个：文档可信度分级

| 文档 | 可信度 | 说明 |
|---|---|---|
| **源码注释** | ✅ 最高 | 注释很密，且常记录"早先怎么错、为什么这么改"，是排查资料的第一手来源 |
| `AGENTS.md` / `CLAUDE.md` | ✅ 高 | 已逐条核对源码 |
| `README.md` 的**语法与用例** | ✅ 高 | 跟着用例就能写出能跑的程序 |
| `README.md` 的**安全模型**（"指针与 unsafe"） | ⚠️ 愿景 | 描述的**不是**现状，见「红线」 |
| `docs/rust-parity.md` | ⚠️ 部分过期 | 大方向对，但个别结论已被在途改动超越（例：称泛型别名不支持、`#[derive]` 结构体报错——都已修好） |
| `CONTRIBUTING.md` | ⚠️ 部分过期 | 步骤对，没提负向测试缺失和架构红线 |
| `tests/README.md` | ❌ 严重过期 | "尚未覆盖"表说 `enum`/`match`/`泛型`/`智能指针` 未实现，实际 `57_enum.zr`/`66_generics.zr` 都在跑 |
| `future/README.md` | ❌ **不可信** | 状态列几乎全标 ✅ Done，源码里全是空壳或没接线。见下 |
| `Cargo.toml` description | ❌ 夸大 | 称 "coroutines, CTFE, and a full async runtime"，实际 `yield` 是类型占位、async runtime 仅部分平台 |

**结论：以源码为准。任何"已实现"的声明都要 `grep` 验证。**

`future/README.md` 标 ✅ 但实际是空壳或没接线的：`lto`（`src/opt/lto.rs`，`opt/mod.rs` 只声明 `pass_manager`）、
`pgo`（`src/opt/pgo.rs`，同上）、`macro_system`（`src/macro_system/`，`main.rs` 未 `mod` 声明）、
`const_generics` / `specialization` / `negative_bounds` / `higher_ranked` / `impl_trait` /
`coherence_rules` / `assoc_type_projection`（trait 求解代码里查无实现）。

---

## 1. 构建 / 测试 / 诊断

```sh
cargo build                            # 也构建 runtime/*.s → libzrrt-<平台>.a
cargo build -q 2>&1 | tail -20         # 只看错误
cargo test --test zr_language          # 全量回归，约 2.5–3 分钟
target/debug/zr prog.zr -O1 -o /tmp/prog   # 编译单个程序
```

- **运行时链接失败**（`requires the platform linker` + `runtime source ... was not built`）
  **是环境问题**，重跑 `cargo build`。别把它当语言 bug 去改编译器。
- 测试：`tests/zr_language.rs` 把 74 个 `tests/cases/*.zr` 各在
  `-O0/-O1/-O2/-O3/-Oz` 跑一遍（370 次编译+运行）。退出码 0 = 过，非 0 = 失败（编号指向失败的断言）。
  宿主端另有 `io_print_matches_rust` 等 Rust 对照用例。
- **没有负向测试机制。** `tests/cases/` 下每个 `.zr` 都必须编译通过并返回 0。
  想验证"某错误真被拦住了"，单独写 `/tmp/x.zr` 手工编译确认。
- 最小复现优先放 `/tmp/`，**别塞进 `tests/cases/`**（塞进去就会挂测试）。

诊断开关：`--dump-tokens|ast|hir|mir|ssa|ssa-after|asm`、`--wat`、`--emit-llvm`；
环境变量 `ZR_TIME=1`（各阶段耗时）、`ZR_VERBOSE=1`、`ZR_DEBUG_ALLOC`、`ZR_DEBUG_BACKEDGE`。
`cargo test` 输出很长，重定向到文件再 grep。

---

## 2. 流水线与模块地图

```
lexer → parser(ast) → typeck(hir, infer) → mir → ssa → opt → codegen → ELF/Mach-O/PE/wasm
```

`src/main.rs:19-35` 的 `mod` 声明就是**已接线清单**。

- **类型检查主战场是 `src/typeck/mod.rs`（约 8100 行），不是 `infer.rs`。**
  impl 匹配的真实实现在 `mod.rs::impl_target_matches`；`infer.rs` 里的
  `impl_matches_type` 是恒返回 `true` 的死桩，别去那里找逻辑。
- 后端 6 个：`encoder.rs`(x86-64)、`encoder_aarch64.rs`、`encoder_arm.rs`(A32)、
  `encoder_thumb.rs`(Thumb-2)、`encoder_riscv.rs`(RV32/64IM)、`wasm.rs`(wasm32/64)。
  目标格式：`objfile.rs`(ELF)、`macho.rs`、`pe.rs`、`wasm.rs`。另有 `c_backend.rs`（**未接 CLI**）。
- 标准库 `std/`：14 个 `.zr` 源文件（编译器自编译）——
  `async` `collections` `core` `error` `fs` `io` `mem` `net` `path` `process` `simd` `slice` `string` `thread`。
  **注意模块名不带 `std::` 前缀**：`use core::Option;`，不是 `use std::core::Option;`。
- 运行时 `runtime/*.s`：每平台一份汇编（Linux/macOS/Windows 的 x86-64+aarch64，裸机 ARM/RISC-V）。
  提供 `zr_write` / `zr_alloc` 等符号，与编译器分离编译。
- 已接线的优化 pass（`src/opt/mod.rs:74-99`，按 `-O` 等级分层）：
  `fold_constants` `eliminate_dead_code` `eliminate_common_subexpr` `global_value_numbering`
  `eliminate_dead_blocks` `sparse_conditional_constant_propagation` `peephole`
  `inline_functions` `optimize_loops`。

### 确认是死代码 / 未接线的

| 路径 | 状态 |
|---|---|
| `src/zr0/` | `main.rs` 未 `mod` 声明 |
| `src/macro_system/` | `main.rs` 未 `mod` 声明（`macro_rules!` 走的是 `src/macros.rs`） |
| `src/opt/lto.rs` | `opt/mod.rs` 未声明 |
| `src/opt/pgo.rs` | `opt/mod.rs` 未声明 |
| `src/codegen/c_backend.rs` | CLI 无 `--emit-c` 入口 |

**不要在这几个目录里"修 bug"——先确认它们该不该接线。**

---

## 3. 架构红线（务必先读）

1. **编译跑在 256 MiB 栈的线程上**（`main.rs:170-173`）。栈溢出 = **abort**，
   不可恢复、无诊断输出。写递归解析/检查代码必须能证明终止，别靠"应该不会爆"。
2. **`Ty::Infer(InferId(0))` 不是哨兵，是活的推断变量。** 大文件里会被绑定掉。
   拿它当"未知类型"的占位返回值会让 `subtype` resolve 出旧值，报出完全无中生有的错
   （曾导致 `std/io.zr` 报 `expected (), found u8` 且报错位置与真实问题无关）。
   **不一致时应返回 `Unit` 或显式报错，绝不复用已有 `InferId`。**
3. **`tail.is_none()` ≠ 发散。** 两种情况 tail 都是 `None`：块以 `return;` 结尾（真发散），
   或块最后一句带了分号（`f(x);` 后直接 `}`，不产生值但**不**发散，类型是 `()`）。
   混同会让 `if` 分支被误判成 `Never`。发散要按 AST 形状判（`block_diverges` / `expr_diverges`）。
4. **`type_mismatch(expected, found)` 的参数顺序在 `unify` 和 `subtype` 里相反。**
   `unify(a, b)`：`a` 期望、`b` 实际。`subtype(sub, sup)`：`sub` 实际、`sup` 期望。
   写反了报错文案整段倒过来（`expected u8, found ()` 其实是 `expected (), found u8`）。
5. **`subtype` 的错误被 `.ok()` 静默吞掉**（`check_block` 里），一个标准库文件里能攒出
   70 多个 `Int(32)`/`Uint(64)` 宽度不匹配。**编译能通过不代表没有类型不一致。**
6. **`#[derive]` 生成的 impl 块 `trait_id` 是 `None`**（属性 args 只有字符串）。
   求解必须靠 `trait_defs_by_name` 反查名字；只认**真正写在 `derive` 属性里**的名字，
   不能一见 `derive` 属性就算满足——那样 derive 能满足任何 trait。
7. **占位降级（下游会拿到错的值）**：`src/ssa/mod.rs:1267` `Deref => SsaUnaryOp::Not`；
   `src/mir/builder.rs:1358` `AddrOf => MirUnaryOp::Deref`。
8. **`--emit-llvm` 输出的是手写伪 IR 文本**（整数统一 i64、指针 i8*），`clang` 消费不了。

---

## 4. 安全模型的现实

README 里"指针与 unsafe"一节目前是**愿景**：

- **没有 `unsafe` 关键字。** `src/lexer/` 搜不到 `KwUnsafe`，`unsafe { }` 无法解析。
- **没有 `dyn` 关键字**，trait 对象不可用，因此 `std/error` 导入不了。
- **切片/数组索引不做越界检查**：`check_index_scalar` 不检查边界。
- **借用检查是空壳**：`infer.rs::check_borrows` 只做记录不做检查。
- **移动语义不检查**：`let s2 = s1;` 之后再用 `s1` 不报错。
- **`str` 是 `{ptr, len}` 结构体**，切片写成 `[]const T`（元素在后），不是 Rust 的 `&[T]`。
- **`println` 是函数不是宏**：`println(s: str)`；格式化走 `println_fmt(fmt, &[FormatArg], n)`，
  只支持 `{}` `{:?}` `{:x}` `{:X}`，宽度/精度/对齐/填充被**原样打印**（不报错）。

---

## 5. 在途工作（继续实现的起点）

`git status --short` 有 6 个已修改未提交文件，都在 `feat/lang-features` 分支：

| 文件 | 在做什么 |
|---|---|
| `src/parser/ast.rs` | `GenericParam::Type(String)` → `Type(String, Vec<Type>)`：保留 `<T: Ord>` 的 trait 边界 |
| `src/parser/decl.rs` | 解析 `T: Clone + Ord` 多边界时收集而非丢弃 |
| `src/typeck/hir.rs` | `HirTrait` 新增 `super_traits: Vec<DefId>` |
| `src/typeck/infer.rs` | 修 `subtype` 的 `type_mismatch` 参数顺序（报错文案整段倒置的 bug） |
| `src/typeck/mod.rs` | 见下 |
| `tests/cases/66_generics.zr` | 新增超类型方向测试（`Rankable: Equ`） |

`src/typeck/mod.rs` 在途改动（**已全部编译通过**）：

- 新增 `trait_defs_by_name`、`generic_bounds` 两张表；`generic_bounds` 的边界顺序
  必须与 `generic_names` **严格对齐**（都只收类型参数，不能给 lifetime/const 留占位空 vec，
  否则 `fn f<'a, T: Ord>` 里 `T` 会索引到空 vec，边界静默丢失）。
- 新增 `expand_trait` / `implements_trait` / `direct_impls` / `impl_target_matches` /
  `resolve_ty` / `check_trait_bounds`；在 `monomorphize` 里调用 `check_trait_bounds`
  ——**单态化是验证 trait 边界的唯一时刻**。
- **超类型方向**：`implements_trait` 必须"每个直接实现的 trait 各自往上展开，再判目标在不在里面"。
  反过来（展开目标 trait）会同时**漏掉继承关系**并**削弱边界**（只实现 `Equ` 的类型
  满足 `T: Rankable`），两个方向都错，削弱边界是类型安全漏洞。
- `unify_or` 修好：`Never` 是底类型（`unify_or(a, Never) == a`），不一致时返回 `Unit`
  而非 `Ty::Infer(InferId(0))`。
- 新增 `expr_diverges`，`Expression::If` 分支改用 `block_diverges` / `expr_diverges` 判发散。
- 枚举的 `attrs` 现在会取到（早先落到 `_ => &[]`，`#[derive]` 在 `enum` 上被静默丢弃）。

### ⚠️ 已知遗留回归：`52_if_let.zr` 全 5 个优化等级失败

```
zr: 30:8:770: type mismatch: expected `i32`, found `()`
```

**最小复现**（`/tmp/r2.zr`，已验证）：

```zr
fn nested(x: i32) -> i32 {
    if let y = x {
        if let z = y {
            return z + 10;
        } else {
            return 1;
        }
    } else {
        return 2;
    }
}
fn main() -> i32 { return nested(5); }
```

**根因**：新增的 `expr_diverges` 假设 `Expression::If` 的 `then_b` 是表达式，套了
`block_diverges(then_b)`。但 `src/parser/ast.rs` 里 `If` 的签名是
`If(Box<Expression>, Block, Option<Box<Expression>>, Span)` —— **then 分支是
`Block`，不是 `Expression`**。所以 `then_diverges` 恒为 `false`，`t` 取到
`then_hir.tail.unwrap_or(Ty::Unit)`。而 `IfLet` 里 `then_hir` 在"有绑定需要替换"
时被包进 `substitute_local` 的 wrapper 再解包，导致 tail 丢失 → `t == Unit`
→ 报 `expected i32, found ()`。单层的 `if let ... { return 1; }` 不受影响
（`/tmp/r1.zr` 通过），只有**嵌套** `if let` 触发。

**修法方向**（两者任选，推荐 1）：
1. 把 `Expression::If` 分支的类型推断改回按 `Block` 处理（`then_hir.tail` 已有正确类型，
   发散判断直接用 `block_diverges(then_b)` 作用于 AST 的 `Block`，别经 wrapper 绕行）；
2. 或在 `Expression::IfLet` 的分支类型计算处加一条：`then_b` 是 `Block` 且 `block_diverges`
   为真时取 `Never`。

注意 `If` 与 `IfLet` 的 AST 形状**不同**：`If.else` 是 `Option<Box<Expression>>`，
`If.then` 是 `Block`；`IfLet.then` 是 `Block`，`IfLet.else` 也是 `Option<Box<Expression>>`。
`expr_diverges` 里已经正确区分了这两点（`If` 用 `block_diverges`，`IfLet` 的 else 用递归），
问题只在上面那段新增的分支类型计算代码。

### `cargo test` 当前基线（2026-10-25）

- 74 个用例 × 5 等级 = 370 项，**360 过 / 10 挂**。
- 挂的 10 项 = `52_if_let` × 5 等级（上面的回归）+ `71_derive` × 5 等级。
- `71_derive` 挂的原因：测试文件写了 `#[derive(Debug, PartialEq, Eq, Hash)]`，
  而 `generate_derive_impls` 只接受 `Debug` / `Clone` / `Copy`，其余名字直接报
  `cannot derive ...: unknown derive trait`。**要支持 `PartialEq` / `Eq` / `Hash` /
  `PartialOrd` / `Ord` / `Default`，得在 `generate_derive_impls` 里加分支**
  （目前这些名字在 `docs/rust-parity.md` 里被列为"可接受的 derive 名"，与代码不符）。
- 宿主端 `io_print_matches_rust` 也在失败列表里，需单独查。

---

## 6. 工作约定

- **提交信息用中文**，Conventional Commits（`feat:` / `fix:`）。
  本项目提交信息风格是**详细中文段落**：先讲"早先怎么错"，再讲"为什么这么改"，
  最后 `Co-Authored-By: Claude Code <noreply@anthropic.com>`。照这个写。
- 注释用中文，密度与现有代码一致（本项目注释很密，改代码时顺手补上下文是对的）。
- 加语言特性按顺序：`lexer`（token）→ `parser` → `typeck` → `mir` → `ssa` →
  `codegen` → `std/` → `tests/cases/`。**跳步会让下游拿到占位数据。**
- 改完 `cargo build` 通过 **≠** 改对了。语言改动至少 `cargo test --test zr_language` 一遍；
  类型检查改动要额外确认负向用例（该报错的还是报错，用 `/tmp/x.zr` 手工验）。
