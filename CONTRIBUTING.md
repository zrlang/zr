# Contributing to zr

谢谢你对 zr 感兴趣！以下是参与贡献的指南。

## 开发环境

- Rust 工具链 ≥ 1.70
- Linux x86-64（主要开发平台）
- `cargo`、`cargo-fmt`、`cargo-clippy`

## 工作流

1. Fork 仓库
2. 创建特性分支：`git checkout -b feat/my-feature`
3. 编码并确保通过测试：`cargo test && cargo clippy`
4. 提交：`git commit -m "feat: add something"`
5. 推送并发起 PR

> 提交信息用**中文**、Conventional Commits，风格是**详细段落**：先讲"早先
> 怎么错"，再讲"为什么这么改"，最后 `Co-Authored-By: Claude Code
> <noreply@anthropic.com>`（项目内惯例，见 `CLAUDE.md` §9）。

## 测试约定

- `tests/cases/*.zr`：每个都**必须编译通过并返回 0**（宿主运行器 `tests/zr_language.rs`
  在每个 `-O0/-O1/-O2/-O3/-Oz` 下各跑一遍，共 90 用例 × 5 = 450 项）。
- **没有负向测试机制**：想验证"某错误真被拦住了"，单独写 `/tmp/x.zr` 手工编译
  确认，**别塞进 `tests/cases/`**（塞进去就会挂测试）。
- 最小复现优先放 `/tmp/`。

## 架构红线（务必先读，详见 `AGENTS.md` §3）

- 编译跑在 256 MiB 栈的线程上（`main.rs:170-173`）；栈溢出 = abort，无诊断。
- `Ty::Infer(InferId(0))` 不是哨兵，是活的推断变量——别复用。
- `tail.is_none()` ≠ 发散；`type_mismatch` 参数顺序在 `unify` / `subtype` 里相反。
- `subtype` 的错误被 `.ok()` 静默吞掉；编译通过 ≠ 没有类型不一致。
- `#[derive]` 生成的 impl 块 `trait_id` 为 `None`，求解靠 `trait_defs_by_name` 反查。
- 内联重命名必须**一次性**映射全部操作数（`map_operands`），不能按值逐个替换。
- 后端的"值是不是浮点"是反推的，浮点形参要 `SsaFunction::param_types` 显式播种。
- **不要在死代码目录里"修 bug"**：`src/zr0/`、`src/macro_system/`、
  `src/opt/lto.rs`、`src/opt/pgo.rs`、`src/codegen/c_backend.rs`——先确认该不该接线。

## 添加新语言特性

1. 在 `src/lexer/` 添加 token（如需要）
2. 在 `src/parser/` 添加语法
3. 在 `src/typeck/` 添加类型规则
4. 在 `src/mir/` 添加降级规则
5. 在 `src/ssa/` 添加 SSA 构造
6. 在 `src/codegen/` 添加代码生成
7. 在标准库 `std/` 添加相关 API
8. 在 `tests/` 添加测试
9. 更新 `README.md`

> **跳步会让下游拿到占位数据。** 按 `lexer → parser → typeck → mir → ssa →
> codegen → std/ → tests/cases/` 顺序，不跳步。

## 报告问题

请包含：
- 使用的 zr 版本
- 最小复现示例
- 期望行为与实际行为
- 如有，附上编译器的 dump 输出（`--dump-ssa` 等）
