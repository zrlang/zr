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

## 代码风格

- 遵循 `rustfmt` 默认配置
- 公开 API 必须有文档注释
- 提交信息遵循 Conventional Commits

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

## 报告问题

请包含：
- 使用的 zr 版本
- 最小复现示例
- 期望行为与实际行为
- 如有，附上编译器的 dump 输出（`--dump-ssa` 等）
