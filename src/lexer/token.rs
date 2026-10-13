//! Token 与 TokenKind 定义。
//!
//! 设计参考：
//! - Rust：原始字面量 `r#"..."#`、字节字面量、原始标识符。
//! - Zig：label（`blk:`）、`anytype`、容器字段语法。
//! - Python：缩进敏感的 token 在 zr 中**不采用**——zr 使用大括号。

use std::fmt;

use super::Span;

/// Token 种类。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // ===== 文件结束 =====
    /// 文件结束。
    Eof,

    // ===== 字面量 =====
    /// 整数字面量，含 `0x`/`0b`/`0o` 前缀，值已归一化为 i128。
    IntLiteral(i128),
    /// 浮点字面量（f32/f64）。
    FloatLiteral(f64),
    /// 字符串字面量（UTF-8，转义已处理）。
    StringLiteral(String),
    /// 字符字面量。
    CharLiteral(char),
    /// 布尔字面量。
    BoolLiteral(bool),

    // ===== 标识符与关键字 =====
    /// 标识符 / 关键字。
    Ident(String),

    // ===== 关键字（表达式与语句） =====
    /// `fn`
    KwFn,
    /// `let`
    KwLet,
    /// `const`
    KwConst,
    /// `var`（可变绑定，区别于 `let`）
    KwVar,
    /// `mut`
    KwMut,
    /// `if`
    KwIf,
    /// `else`
    KwElse,
    /// `match`
    KwMatch,
    /// `while`
    KwWhile,
    /// `for`
    KwFor,
    /// `loop`
    KwLoop,
    /// `break`
    KwBreak,
    /// `continue`
    KwContinue,
    /// `return`
    KwReturn,
    /// `defer`
    KwDefer,
    /// `try`
    KwTry,
    /// `catch`
    KwCatch,
    /// `throw`
    KwThrow,
    /// `async`
    KwAsync,
    /// `await`
    KwAwait,
    /// `yield`
    KwYield,
    /// `spawn`
    KwSpawn,
    /// `comptime`
    KwComptime,
    /// `goto`（受限跳转，仅函数内）
    KwGoto,

    // ===== 关键字（类型系统） =====
    /// `struct`
    KwStruct,
    /// `enum`
    KwEnum,
    /// `union`
    KwUnion,
    /// `trait`
    KwTrait,
    /// `impl`
    KwImpl,
    /// `type`
    KwType,
    /// `where`
    KwWhere,
    /// `pub`
    KwPub,
    /// `mod`
    KwMod,
    /// `use`
    KwUse,
    /// `as`
    KwAs,
    /// `self`
    KwSelf,
    /// `Self`（大写）
    KwUpperSelf,
    /// `super`
    KwSuper,
    /// `true`
    KwTrue,
    /// `false`
    KwFalse,
    /// `void` / `unit`
    KwVoid,
    /// `anytype`（输入类型占位，类似 Zig）
    KwAnytype,
    /// `auto`（自动推导类型）
    KwAuto,
    /// `in`（for 循环迭代）
    KwIn,
    /// `is`（类型测试 / 向下转型）
    KwIs,
    /// `ref`
    KwRef,
    /// `static`
    KwStatic,

    // ===== 类型关键字 =====
    /// `i8`..`i128`
    TyI(usize),
    /// `u8`..`u128`
    TyU(usize),
    /// `f32` / `f64`
    TyF(usize),
    /// `bool`
    TyBool,
    /// `str`（UTF-8 字符串切片 `[]const u8`）
    TyStr,
    /// `char`
    TyChar,
    /// `never`（`!`）
    TyNever,
    /// `anyerror`
    KwAnyerror,

    // ===== 标点符号 =====
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `=`
    Eq,
    /// `==`
    EqEq,
    /// `!=`
    BangEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `!`
    Bang,
    /// `&`
    Amp,
    /// `|`
    Pipe,
    /// `^`
    Caret,
    /// `~`（按位非，区别于 `!` 的逻辑非）
    Tilde,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `&&`
    AmpAmp,
    /// `||`
    PipePipe,
    /// `..`（区间）
    DotDot,
    /// `..=`（闭区间）
    DotDotEq,
    /// `=>`
    Rocket,
    /// `->`
    Arrow,
    /// `::`
    ColonColon,
    /// `:=`（walrus / 声明式赋值）
    ColonEq,
    /// `+=`
    PlusEq,
    /// `-=`
    MinusEq,
    /// `*=`
    StarEq,
    /// `/=`
    SlashEq,
    /// `%=`
    PercentEq,
    /// `&=`
    AmpEq,
    /// `|=`
    PipeEq,
    /// `^=`
    CaretEq,
    /// `<<=`
    ShlEq,
    /// `>>=`
    ShrEq,
    /// `++`（字符串拼接 / 切片连接）
    PlusPlus,
    /// `##`（编译期字符串拼接）
    HashHash,

    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `:`
    Colon,
    /// `_`（通配符模式 / 忽略绑定）
    Underscore,
    /// `.`
    Dot,
    /// `?`（可选类型 / try 简写）
    Question,
    /// `@`（属性 / 内建函数前缀）
    At,
    /// `#`（编译指示）
    Hash,

    // ===== 错误恢复用 =====
    /// 词法错误占位（含错误信息，解析阶段报告）。
    LexError(String),
}

/// 一个 token：种类 + 位置。
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// 是否为字面量 token。
    pub fn is_literal(&self) -> bool {
        matches!(
            self.kind,
            TokenKind::IntLiteral(_)
                | TokenKind::FloatLiteral(_)
                | TokenKind::StringLiteral(_)
                | TokenKind::CharLiteral(_)
                | TokenKind::BoolLiteral(_)
        )
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format!("{:?}", self.kind))
    }
}
