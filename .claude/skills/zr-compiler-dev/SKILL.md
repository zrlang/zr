---
name: zr-compiler-dev
description: 在 zr 编译器代码库里工作的操作手册——构建、测试、定位 bug、新增语言特性、避开已知陷阱。改动 zr 源码、排查编译失败、加语言特性前必读。
---

# zr 编译器开发

zr 是一门系统编程语言（Rust/Zig 混合取向），编译器用 Rust 写、零第三方依赖。
根目录 `CLAUDE.md` 和 `AGENTS.md` 有完整项目背景，本 skill 是可执行的操作流程。

## 1. 先建基线

任何改动之前，先确认树是干净的、能编译：

```sh
cd /Users/navysummer/Desktop/other/zr
git status --short
cargo build -q 2>&1 | tail -20
```

`build.rs` 会同时汇编 `runtime/*.s` 为 `libzrrt-<平台>.a`。若报
`linking ... requires the platform linker` + `runtime source ... was not built`，
是环境问题，重跑 `cargo build`，**不要**去改编译器。

## 2. 诊断一个编译失败

```sh
target/debug/zr prog.zr -o /tmp/out 2>&1 | head -10      # 真实报错
target/debug/zr prog.zr -o /tmp/out                      # EXIT=$?
```

报错格式是 `zr: <line>:<col>:<byte-offset>: <message>`。

**按流水线阶段 dump，逐层定位：**

```sh
--dump-tokens     # lexer 出问题（token 不对）
--dump-ast        # 解析出问题（形状不对——最常用）
--dump-hir        # 类型检查产出
--dump-mir        # MIR 降级
--dump-ssa        # SSA（--dump-ssa-after 是优化后）
--dump-asm        # 最终汇编
```

典型用法：类型报错说不通时 `--dump-ast`，看解析器把语句建成了什么形状——
**大多数"神秘类型错"都是解析形状问题，不是类型规则问题。**

环境开关：`ZR_TIME=1`（各阶段耗时）、`ZR_VERBOSE=1`（comptime 折叠）、
`ZR_DEBUG_ALLOC` / `ZR_DEBUG_BACKEDGE`（寄存器分配 / 回边）。

## 3. 测试

```sh
cargo test --test zr_language                    # 全量，约 3 分钟
cargo test --test zr_language zr_language_cases  # 只跑语言用例回归
```

输出很长，重定向再筛：

```sh
cargo test --test zr_language > /tmp/t.log 2>&1; grep -c "^ok" /tmp/t.log; tail -30 /tmp/t.log
```

`tests/zr_language.rs` 有 3 个 test：`zr_language_cases`（74 个 `cases/*.zr`
× 5 个优化等级）、`io_print_matches_rust`、`extern_c_write_prints_string_literal`。

退出码 0 = 通过，非 0 = 失败（编号指向失败的断言）。

**关键限制：没有负向测试。** `tests/cases/` 下每个 `.zr` 都必须编译通过并返回 0。
想验证"某错误真被拦住了"，单独写 `/tmp/x.zr` 手工编译确认报错。
改类型检查后务必回头确认：该报错的还在报错。

## 4. 加语言特性的顺序

不能跳步——跳了下游会拿到占位数据：

```
lexer(token) → parser → typeck → mir → ssa → codegen → std/ → tests/cases/
```

各步要改的地方：

1. **`src/lexer/token.rs`** — 新关键字/运算符 token。先确认没有同名 token。
2. **`src/parser/`** — 语法。注意 `parse_block_contents` 的分号规则：
   block-like 表达式（`if`/`match`/`while`/`loop`/`for`/块）后允许省分号，
   由 `is_block_like` 判定，会建成语句而非尾表达式。
3. **`src/typeck/`** — 类型规则。**主战场是 `mod.rs`（~8000 行），不是 `infer.rs`。**
   impl 匹配的真实实现在 `mod.rs::impl_target_matches`；`infer.rs::impl_matches_type`
   是恒 `true` 的死桩。
4. **`src/mir/`、`src/ssa/`** — 降级与 SSA 构造。
5. **`src/codegen/`** — 6 个后端都要动（x86-64/aarch64/arm/thumb/riscv/wasm），
   或用 `mod.rs` 里的 `unsupported_*` 辅助函数让其余后端明确报错。
6. **`std/`** — 标准库 API（14 个模块，`.zr` 源文件，编译器自编译）。
7. **`tests/cases/`** — 新增自检用例，编号顺延。

## 5. 避坑清单（都真实触发过）

**`Ty::Infer(InferId(0))` 不是哨兵，是活的推断变量。**
大文件里它会被绑定掉，拿它当"未知类型"返回值会让 `subtype` resolve 出旧值，
报出完全无中生有的错。曾导致 `std/io.zr` 报 `expected (), found u8`，
且报错位置与真实问题无关。类型不一致时返回 `Unit` 或显式报错，
**绝不复用已有 InferId。**

**`tail.is_none()` ≠ 发散。**
块以 `return;` 结尾是真发散；块最后一句带分号（`f(x);` 后直接 `}`）
不产生值但**不**发散（类型是 `()`）。按 AST 形状判（`block_diverges` / `expr_diverges`）。

**`type_mismatch(expected, found)` 参数顺序在 `unify` 与 `subtype` 里相反。**
`unify(a,b)` 里 a=期望；`subtype(sub,sup)` 里 sub=实际。写反了报错文案整段倒置，
排查时极具误导性。

**`subtype` 的错误被 `.ok()` 静默吞掉。**
`check_block` 里吞掉后只累积到 `self.errors`，一个标准库文件能攒出 70+ 个
`Int(32)`/`Uint(64)` 宽度不匹配。**编译通过 ≠ 无类型不一致。**

**编译跑在 256 MiB 栈的线程上**（`main.rs:170-173`）。栈溢出是 abort，不可恢复、
无诊断输出。写递归解析/检查代码必须能证明终止。

## 6. 文档可信度（重要）

| 文档 | 可信度 |
|---|---|
| 源码注释 | ✅ 最高——常记录"早先怎么错、为什么这么改" |
| `CLAUDE.md` / `AGENTS.md` | ✅ 已核对源码 |
| `README.md` 语法与用例 | ✅ 高 |
| `README.md` 安全模型（"指针与 unsafe"节） | ⚠️ 愿景，非现状 |
| `CONTRIBUTING.md` | ⚠️ 步骤对，但漏了负向测试缺失和架构红线 |
| `docs/rust-parity.md` | ⚠️ 逐条核对，个别结论已过期 |
| `tests/README.md` "尚未覆盖"表 | ❌ 严重过期（说 enum/match/泛型未实现，实际都有用例且通过） |
| `future/README.md` 状态列 | ❌ 不可信，几乎全标 ✅ Done |
| `Cargo.toml` description | ❌ 夸大（称 full async runtime，实际 `yield` 是占位、async 仅 x86-64） |

**凡"已实现"的声明都要 grep 验证。**

**确认是死代码的模块**（文件在但无人引用，别在里面"修 bug"）：
`src/zr0/`、`src/macro_system/`（未在 `main.rs` 声明）、
`src/opt/lto.rs`、`src/opt/pgo.rs`（`opt/mod.rs` 只声明 `pass_manager`）、
`src/codegen/c_backend.rs`（CLI 无 `--emit-c`）。

## 7. 占位降级陷阱（下游拿到错值）

改相关区域前先确认这些不是你的依赖：

- `src/ssa/mod.rs:1267` — `Deref => SsaUnaryOp::Not`
- `src/mir/builder.rs:1358` — `AddrOf => MirUnaryOp::Deref`
- `src/codegen/c_backend.rs` — 未知表达式一律降级成 `"0"`
- `src/codegen/objfile.rs` — 有零长度条件分支占位留在产物里
- `src/typeck/mod.rs` — 有 `if false &&` 的死条件（enum 聚合载荷检查）
- `--emit-llvm` 是手写伪 IR 文本（整数统一 i64、指针 i8\*），clang 消费不了

## 8. 收尾

```sh
cargo build -q 2>&1 | tail -20          # 必须干净通过
cargo test --test zr_language > /tmp/t.log 2>&1; tail -30 /tmp/t.log
grep -rn "DBG\|eprintln!\|TODO\|todo!\|TEMP" src/   # 确认没留调试痕迹
git status --short
```

提交信息用中文 + Conventional Commits（`feat:` / `fix:`）。
注释用中文，密度对齐现有代码。最小复现放 `/tmp/`，不要塞进 `tests/cases/`。

**`cargo build` 通过 ≠ 改对了。** 语言改动至少跑一遍全量回归；
类型检查改动要额外确认该报错的还在报错。
