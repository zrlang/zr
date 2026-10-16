//! 源码位置。
//!
//! 用 `(start, end, line, col)` 四元组描述一段连续的源码区间，
//! 前端所有阶段都携带 span，最终能在错误信息里精确指出问题位置。

use std::fmt;

/// 一段连续的源码区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    /// 字节偏移起点（含）。
    pub start: usize,
    /// 字节偏移终点（不含）。
    pub end: usize,
    /// 起点所在行号（从 1 开始）。
    pub line: usize,
    /// 起点所在列号（从 1 开始，按字符计）。
    pub column: usize,
}

impl Span {
    /// 构造一个新的 span。
    pub const fn new(start: usize, end: usize, line: usize, column: usize) -> Self {
        Self {
            start,
            end,
            line,
            column,
        }
    }

    /// 构造一个「未知位置」的 span（用于合成的节点）。
    pub const fn unknown() -> Self {
        Self::new(0, 0, 0, 0)
    }

    /// 判断是否为未知位置的 span。
    #[cfg(test)]
    pub const fn is_unknown(&self) -> bool {
        self.start == 0 && self.end == 0 && self.line == 0 && self.column == 0
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}
