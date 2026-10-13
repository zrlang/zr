//! 编译器统一的错误类型。
//!
//! 设计要点：
//! - 所有阶段的错误最终收敛为 `CompileError`，便于在 `main` 统一报告。
//! - 携带源位置 `Span`，前端阶段应把 token / AST 上的 span 透传过来。
//! - 实现 `std::error::Error` + `Display`，可直接用于 `?` 传播。

use std::fmt;

use crate::lexer::Span;

/// 编译错误。
#[derive(Debug, Clone)]
pub struct CompileError {
    /// 人类可读的错误信息。
    pub message: String,
    /// 出错位置（前端阶段填充）。
    pub span: Option<Span>,
    /// 错误分类，便于 IDE / 工具做细粒度处理。
    pub kind: ErrorKind,
}

/// 错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 语法错误。
    Parse,
    /// 类型错误 / 类型推导失败。
    Type,
    /// 名称解析错误（未定义变量、重复定义、循环模块依赖）。
    Name,
    /// trait 一致性错误（孤儿规则、重叠 impl）。
    Coherence,
    /// 内部编译器错误（bug）。
    Internal,
}

/// `CompileError` 的 `Result` 别名。
pub type Result<T> = std::result::Result<T, CompileError>;

impl CompileError {
    /// 构造一条带分类的错误。
    pub fn new<S: Into<String>>(msg: S) -> Self {
        Self {
            message: msg.into(),
            span: None,
            kind: ErrorKind::Internal,
        }
    }

    /// 指定错误分类。
    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }

    /// 附加出错位置。
    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    /// 构造一条类型错误。
    pub fn type_error<S: Into<String>>(msg: S) -> Self {
        Self::new(msg).with_kind(ErrorKind::Type)
    }

    /// 构造一条 trait 一致性错误。
    pub fn coherence_error<S: Into<String>>(msg: S) -> Self {
        Self::new(msg).with_kind(ErrorKind::Coherence)
    }

    /// 构造一条名称解析错误。
    pub fn name_error<S: Into<String>>(msg: S) -> Self {
        Self::new(msg).with_kind(ErrorKind::Name)
    }

    /// 构造一条内部编译器错误。
    pub fn internal<S: Into<String>>(msg: S) -> Self {
        Self::new(msg).with_kind(ErrorKind::Internal)
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.span {
            Some(span) => write!(f, "{}:{}:{}: {}", span.line, span.column, span.start, self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CompileError {}

/// 在语法/类型检查阶段把错误信息与 span 关联起来。
#[macro_export]
macro_rules! span_err {
    ($span:expr, $($arg:tt)*) => {
        $crate::error::CompileError::new(format!($($arg)*)).with_span($span)
    };
}
