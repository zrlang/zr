# AGENTS.md — zr 编译器贡献指南

zr 是一门系统编程语言（Rust/Zig 混合取向），编译器用 Rust 写、**零第三方依赖**。
本文件给在代码库里工作的 AI agent 看，是操作约定与"哪些文档可信"的清单。
项目入口与功能全景见根目录 `CLAUDE.md`。

> 最近更新：2026-10-29。工作树干净；`feat/lang-features` 上连续几个提交修的都是
> "编译通过、运行不崩、结果全错"的静默 bug，清单见 §5。

---

## 0. 先读这个：文档可信度分级

| 文档 | 可信度 | 说明 |
|---|---|---|
| **源码注释** | ✅ 最高 | 注释很密，且常记录"早先怎么错、为什么这么改"，是排查资料的第一手来源 |
| `AGENTS.md` / `CLAUDE.md` | ✅ 高 | 已逐条核对源码 |
| `README.md` 的**语法与用例** | ✅ 高 | 跟着用例就能写出能跑的程序 |
| `README.md` 的**安全模型**（"指针与 unsafe"） | ⚠️ 愿景 | 描述**不是**现状（借用检查/生命周期未实现）；2026-10-29 已加警示注明是设计目标 |
| `docs/rust-parity.md` | ✅ 高 | 2026-10-29 已修正（泛型别名改"已支持"、`dyn` 改"骨架可用"）；仍以源码为准 |
| `CONTRIBUTING.md` | ✅ 高 | 2026-10-29 已补测试约定与架构红线摘要 |
| `tests/README.md` | ✅ 高 | 2026-10-29 已重写"尚未覆盖"表并补 52–90 用例清单 |
| `future/README.md` | ✅ 高 | 2026-10-29 已按源码实际结论重排状态列 |
| `Cargo.toml` description | ✅ 高 | 2026-10-29 已改为 "ownership, zero-cost abstractions, and coroutines" |

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
  有时是环境问题（重跑 `cargo build`），**但别急着归因**——2026-10-29 在
  `60_process` / `64_fs_path` / `74_generic_alias` 上发现同样的报错其实是
  **真实 bug**：用户 `fn read` 与 prelude 拉进来的 `core` 模块 `pub fn read`
  同名，MIR 降级时都加 `z_` 前缀发射，目标文件里 `_z_read` 定义两次，macOS
  `ld` 直接断言崩溃（`atomShouldReplaceExisting`）。先 `nm` 看目标文件有没有
  重复符号，再下结论。已修复（提交 `1ef4a02`：MIR 按最终符号名去重，保留首个
  用户定义）。
- 测试：`tests/zr_language.rs` 把 92 个 `tests/cases/*.zr` 各在
  `-O0/-O1/-O2/-O3/-Oz` 跑一遍（460 次编译+运行）。退出码 0 = 过，非 0 = 失败（编号指向失败的断言）。
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
- 已接线的优化 pass（`src/opt/mod.rs:68-102`，按 `-O` 等级分层）：
  `fold_constants` `eliminate_dead_code` `eliminate_common_subexpr` `global_value_numbering`
  `eliminate_dead_blocks` `sparse_conditional_constant_propagation` `peephole`
  `inline_functions` `optimize_loops`。
  **`inline_functions` 只内联单块函数**（`#[inline(always)]` 的多块函数也跳过），
  重命名必须一次性映射全部操作数——见第 5 节第 1 条。

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
7. **占位降级（下游会拿到错的值）**：`src/ssa/mod.rs:1301` `Deref => SsaUnaryOp::Not`；
   `src/mir/builder.rs:1358` `AddrOf => MirUnaryOp::Deref`。
8. **`--emit-llvm` 输出的是手写伪 IR 文本**（整数统一 i64、指针 i8*），`clang` 消费不了。
9. **内联时的值重命名必须"同时替换"**，不能按值逐个 `replace_operand`：被内联体
   `%10 = %1 Add %9` 里先把 `%1` 换成新号 `%9`，再处理 `%9` 就会把两个操作数一起换掉
   （`spawn` 的帧初始化把偏移量当指针，`-O3` SIGSEGV）。用 `map_operands`。
10. **REX 的位是 W=0x08 / R=0x04 / X=0x02 / B=0x01，`0x09` 不是"R|B"而是"W|B"。**
   `xor reg, reg`（`31 /r`）的 dst 同时占 reg 与 r/m 两个字段，r8–r15 要置
   `REX.W|R|B = 0x4D`；写成 `0x48 | 0x09` 会丢 REX.R，编码成 `xor rax, r8`
   ——零常量就变成"用 rax 异或"。新增 REX 计算时先写出位定义再拼。
11. **后端的"这个值是不是浮点"是**反推**出来的**（`assign_float_slots` 扫
   `SsaInstr::Binary { ty: Float }` 等指令），形参本身**没有类型标注**。所以
   形参必须靠 `SsaFunction::param_types`（从 MIR 抄来）**显式播种**：只被转手的
   浮点形参（`fn outer(f: f64) { inner(f) }`）反推不出来，会被当成整数走 rdi，
   而调用方按 SysV 放在 xmm0 —— 位模式当指针用直接 SIGSEGV，或者后面所有整数
   形参整体错位（读到的全是别人的值）。新增后端要照抄这一步。

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
  支持 `{}` `{:?}` `{:x}` `{:X}` `{:b}` `{:o}` `{:e}` `{:E}`、位置参数、填充/对齐/`+`/`#`/`0`/宽度/精度、
  动态宽度 `{:1$}`、动态精度 `{:.*}` / `{:.1$}`（与 Rust 逐字符对比一致）；
  **没有**命名参数 `{name}`，`{:#?}` 也只输出一行（`#` 对 `{:?}` 被忽略）。
- **索引不做越界检查**，但对**标量**下标会在类型检查阶段报错
  （``cannot index a value of type `u8` ``）；指针下标写成 `p[i]`，不要写 `(*p)[i]`。

---

## 5. 最近修掉的静默 bug 与当前基线

2026-10-27 的 `feat/lang-features` 上连续几个提交修的都是"编译通过、运行不崩、
结果全错"的静默 bug；2026-10-29 又修掉了"用户定义被标准库同名函数遮蔽"的两个
关联 bug（提交 `1ef4a02`，详见 §1 的链接失败说明）：

| 文件 | 在做什么 |
|---|---|
| `src/typeck/mod.rs` | `alloc_def_top`（同名保留用户）+ `function_def_ids`（阶段 6/7 逐条取本函数 DefId）：用户 `fn swap` 不再被 prelude 拉进来的 `core` 的 `swap<T>` 覆盖，元组返回类型不再被静默改成 `()` |
| `src/parser/decl.rs`、`src/modules.rs`、`src/typeck/mod.rs` | `use foo::*` glob 导入（提交 `40b5b45`）：解析器保留 `*` 前的路径段（早先 `use core::*` 把 `core` 丢掉）；模块加载器记录每个模块的导出名（`module_exports`），类型检查的 Glob 分支把它们全部登记成别名 |
| `src/mir/mod.rs` | 按最终符号名去重（保留首个用户定义）：用户 `fn read` 与 `core` 的 `pub fn read` 同名同体不再都发射成 `z_read`，`_z_read` 不再定义两次，macOS `ld` 不再断言崩溃 |
| `src/ssa/mod.rs` | 支配边界从"直接前驱版"改成 Cooper–Harvey–Kennedy：漏 φ 会让合流块读到未初始化的栈槽 |
| `src/opt/pass_manager.rs` | 内联重命名改成"先建完整重命名表、再逐操作数一次性映射"（见下） |
| `src/codegen/encoder.rs` | SysV 栈上传参（第 7 个整数 / 第 9 个浮点实参）+ `call` 的 16 字节对齐；零常量的 `xor reg, reg` 补回 REX.R |
| `src/typeck/mod.rs` | `check_index_scalar` 不再吞 unify 错误；`check_match` 给穷尽 match 补发散 else；聚合类型上的 `==`/`<` 改写成 derive 方法调用 |
| `src/typeck/mod.rs` | 枚举缺臂报错列出**缺失变体名**（提交 `a526953`）：早先统一说 `match must have a trailing `_` arm`，误导人以为补个 `_` 就行；现在 `match is not exhaustive: `Color` variant not covered: Blue`，整型/标量仍保留尾 `_` 臂措辞 |
| `src/typeck/derive.rs` | 新增：在 AST 层生成 `Type__clone` / `__eq` / `__lt` / `__hash` / `__default` 方法体 |
| `src/typeck/hir.rs`、`src/mir/builder.rs` | 新增 `ExprKind::Unreachable` → `MirTerminator::Unreachable`（上面的发散 else） |
| `src/typeck/infer.rs` | 诊断名字表：`unify`/`subtype` 的错误不再把具名类型打成 `type#N` |
| `std/io.zr` | `(*p)[i]` → `p[i]`（两处）；`{:#X}` 的前缀改回小写 `0x`（与 Rust 一致） |
| `src/ssa/mod.rs`、`src/codegen/encoder*.rs` | `SsaFunction` 新增 `param_types`（从 MIR 抄），两个后端拿它给浮点形参**播种**：只被转手的 `f64` 形参早先被判成整数，走 rdi 而调用方放 xmm0 |
| `std/io.zr` | 格式串补 `{:e}` / `{:E}`（浮点与整数都走十进制指数形式，整数按有效数字四舍五入）、动态宽度 `{:1$}`、动态精度 `{:.*}` / `{:.1$}`；顺带修 `{:.}`（点后面没数字）不再被当成精度 0 |
| `tests/cases/76..79`、`81`、`tests/negative/12..15` | 分别钉住上面每一处 |

### 三个"改对了才算改对"的点（代码里都有长注释）

1. **内联重命名必须同时替换**：被内联函数体 `%10 = %1 Add %9`，如果按值逐个
   `replace_operand(instr, from, to)`——先把 `%1` 换成调用点的新号 `%9`，指令变成
   `%10 = %9 Add %9`，接着处理 `%9` 就把两个操作数一起换掉了。症状：`spawn` /
   future 的帧初始化把偏移量当指针，往 `0x20` 写，`-O3` 直接 SIGSEGV（`-O0/-O2`
   不内联所以看不出来）。现在用 `map_operands` 一次性映射。
2. **φ 位置要按迭代替换支配边界求**：`DF(x) = { y | x 支配 y 的某个前驱，x 不严格支配 y }`。
   只算 `x ∈ preds(y)` 会漏掉"if 里套循环"的汇合块，走 else 路径时读到上一轮的垃圾。
3. **栈上传参的偏移要跟着 `rsp` 走**：`Src::Mem(slot)` 是 `[rsp + 8*slot]` 的原始帧偏移，
   压入实参后必须整体加 `released/8`；被调方读 `[rbp + 16 + 8k]`（`rbp` 才是稳定基准）。

### `cargo test` 当前基线（2026-10-29，全绿）

- `cargo test --bin zr`：265 项。
- `cargo test --test zr_language`：92 个用例 × 5 个优化等级 = 460 项，另有 3 个宿主端
  对照用例（`io_print_matches_rust`——逐字符对比 Rust 的格式化输出，含 `{:e}` / `{:E}`
  与大浮点 Display（`|v| ≥ 2⁶⁴` 不带 `.0`）的回归；`extern_c_write_prints_string_literal`；
  `toolchain_subcommands_emit`——`zr fmt` / `zr fmt --check` / `zr pkg init` / `zr lsp`
  与 `--emit-llvm` / `--wat` 的发射验证）。
- `cargo test --test zr_negative`：17 个负向用例（含新加的越界、use-after-move 与枚举缺臂 `17_match_missing_variant.zr`）。

### 最近修掉的静默 bug（都在 `tests/` 里留了钉子）

- **穷尽 `match` 的返回值被 `Unit` 覆盖**：靠变体/bool 全覆盖证明穷尽的 match 没有
  `_` 臂，脱糖出的 if/else 链最后一条分支"有值"，于是整块被当成 `()`，返回值被
  丢掉。现在 `check_match` 在这种形状下补一个发散 else（`hir::ExprKind::Unreachable`
  → `MirTerminator::Unreachable`）。
- **零常量没有清零自己的寄存器**：`emit_const_int(_, 0)` 走 `xor reg, reg` 捷径，
  REX 只置了 0x09（W|B，W 本就在 0x48 里），REX.R 丢了 —— r8–r15 被编码成
  `xor rax, r8`，零常量变成"用 rax 异或"。症状是 `match` 里返回常量 `false`
  的路径给出 32513 之类的垃圾。钉子：`tests/cases/79_zero_const_high_reg.zr`。

### 在途工作（`feat/lang-features` 上）

以下项已在本分支上完成或部分完成，更新如下：

- **`#[derive]`**：方法体已生成（`PartialEq`/`Eq`/`Hash`/`PartialOrd`/`Ord`/`Default`），
  **剩**：`Debug` 仍是标记 trait（不生成方法体）；泛型类型 / 枚举 `Default` / 枚举 `Ord`
  显式报错；字段级诊断不精确。
- **`unsafe` 关键字**：✅ 已实现（`unsafe { ... }` 块可解析和通过类型检查；当前不改变语义，按块表达式透传）。
- **`dyn` 关键字**：✅ 词法+解析+类型检查骨架已实现；vtable 分派仍在未实现状态。
- **借用检查**：❌ `infer.rs::check_borrows` 仍是空壳（只记录不检查）。
- **移动检查**：✅ 最小版已实现（`let b = a` 对含 drop glue 的类型是移动，读被移动的源报错）。
- **元数据**（权限、时间戳）与 `tempfile` 缺失。
- **格式串**：命名参数 `{name}` 与多行 Debug `{:#?}` 没做；`#` 对 `{:?}` 被忽略，只输出一行。
- **闭包**：**无捕获**闭包已可存进变量/当实参/返回（提升成 `__closure_N` 顶层函数指针）；**带捕获**闭包已可存进变量/嵌套/多次调用（`tests/cases/84_closure_capture_value.zr`），但跨函数传 env 仍有限制。
- **`i128`/`u128`** 在 64 位目标上不可用（需寄存器对拆分）。
- **穷尽性报错**：✅ 已改进（`a526953`：枚举缺臂列出缺失变体名，`17_match_missing_variant.zr`；整型/标量仍保留尾 `_` 臂措辞）。
- **文档债**：✅ 已清（`fb4dfb2` 修 `tests/README.md` / `future/README.md`；`1d38ca3` 修 README 安全模型节 / `rust-parity.md` / `CONTRIBUTING.md` / `Cargo.toml` description）。

## 6. 工作约定

- **提交信息用中文**，Conventional Commits（`feat:` / `fix:`）。
  本项目提交信息风格是**详细中文段落**：先讲"早先怎么错"，再讲"为什么这么改"，
  最后 `Co-Authored-By: Claude Code <noreply@anthropic.com>`。照这个写。
- 注释用中文，密度与现有代码一致（本项目注释很密，改代码时顺手补上下文是对的）。
- 加语言特性按顺序：`lexer`（token）→ `parser` → `typeck` → `mir` → `ssa` →
  `codegen` → `std/` → `tests/cases/`。**跳步会让下游拿到占位数据。**
- 改完 `cargo build` 通过 **≠** 改对了。语言改动至少 `cargo test --test zr_language` 一遍；
  类型检查改动要额外确认负向用例（该报错的还是报错，用 `/tmp/x.zr` 手工验）。
