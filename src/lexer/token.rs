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

    // ===== 注释（仅 `zr fmt` 使用） =====
    /// 行注释 `// ...`，内容不含 `//` 与结尾换行。
    ///
    /// 只有词法器开了 `keep_comments` 才会产出这些 token；解析器用的
    /// 常规模式仍然跳过注释，所以不影响编译路径。
    LineComment(String),
    /// 块注释 `/* ... */`，内容已解掉一层外层定界符。
    BlockComment(String),

    // ===== 字面量 =====
    /// 整数字面量，含 `0x`/`0b`/`0o` 前缀，值已归一化为 i128；第二项是
    /// 可选的类型后缀（`i8`/`u64`/…）。
    IntLiteral(i128, Option<String>),
    /// 浮点字面量（f32/f64）；第二项是可选的类型后缀。
    FloatLiteral(f64, Option<String>),
    /// 字符串字面量（UTF-8，转义已处理）。
    StringLiteral(String),
    /// 字符字面量。
    CharLiteral(char),

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
    /// 生命周期：`'static`、`'a`。与字符字面量靠词法形态区分。
    Lifetime(String),
    /// `spawn`
    KwSpawn,
    /// `comptime`
    KwComptime,
    /// `default`（特化中的默认方法）
    KwDefault,
    /// `goto`（受限跳转，仅函数内）
    KwGoto,
    /// `unsafe`（原始指针解引用 / 外部函数调用等需开发者自行保证安全的操作）
    KwUnsafe,
    /// `dyn`（trait 对象：`dyn Error` / `&dyn Error`）
    KwDyn,

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
    /// `extern`
    KwExtern,
    /// `crate`
    KwCrate,

    // ===== 类型关键字 =====
    /// `i8`..`i128`
    TyI(usize),
    /// `u8`..`u128`
    TyU(usize),
    /// `f32` / `f64`
    TyF(usize),
    /// `usize`
    TyUsize,
    /// `isize`
    TyIsize,
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
    /// `$`（`macro_rules!` 的元变量前缀）
    Dollar,
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
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format!("{:?}", self.kind))
    }
}

impl TokenKind {
    /// 关键字变体对应的源码文本。
    ///
    /// 词法器把关键字收成独立变体后，源码原文就丢了；但 `use async::{…}`
    /// 这类路径段需要把关键字还原成标识符拼进模块名，所以这里按
    /// 词法器的关键字表逐条列出来。
    pub fn keyword_text(&self) -> Option<&'static str> {
        match self {
            TokenKind::KwFn => Some("fn"),
            TokenKind::KwLet => Some("let"),
            TokenKind::KwConst => Some("const"),
            TokenKind::KwMut => Some("mut"),
            TokenKind::KwIf => Some("if"),
            TokenKind::KwElse => Some("else"),
            TokenKind::KwMatch => Some("match"),
            TokenKind::KwWhile => Some("while"),
            TokenKind::KwFor => Some("for"),
            TokenKind::KwLoop => Some("loop"),
            TokenKind::KwBreak => Some("break"),
            TokenKind::KwContinue => Some("continue"),
            TokenKind::KwReturn => Some("return"),
            TokenKind::KwDefer => Some("defer"),
            TokenKind::KwTry => Some("try"),
            TokenKind::KwCatch => Some("catch"),
            TokenKind::KwThrow => Some("throw"),
            TokenKind::KwAsync => Some("async"),
            TokenKind::KwAwait => Some("await"),
            TokenKind::KwYield => Some("yield"),
            TokenKind::KwSpawn => Some("spawn"),
            TokenKind::KwComptime => Some("comptime"),
            TokenKind::KwDefault => Some("default"),
            TokenKind::KwGoto => Some("goto"),
            TokenKind::KwUnsafe => Some("unsafe"),
            TokenKind::KwDyn => Some("dyn"),
            TokenKind::KwStruct => Some("struct"),
            TokenKind::KwEnum => Some("enum"),
            TokenKind::KwUnion => Some("union"),
            TokenKind::KwTrait => Some("trait"),
            TokenKind::KwImpl => Some("impl"),
            TokenKind::KwType => Some("type"),
            TokenKind::KwWhere => Some("where"),
            TokenKind::KwPub => Some("pub"),
            TokenKind::KwMod => Some("mod"),
            TokenKind::KwUse => Some("use"),
            TokenKind::KwAs => Some("as"),
            TokenKind::KwSelf => Some("self"),
            TokenKind::KwUpperSelf => Some("Self"),
            TokenKind::KwSuper => Some("super"),
            TokenKind::KwTrue => Some("true"),
            TokenKind::KwFalse => Some("false"),
            TokenKind::KwVoid => Some("void"),
            TokenKind::KwAnytype => Some("anytype"),
            TokenKind::KwAuto => Some("auto"),
            TokenKind::KwIn => Some("in"),
            TokenKind::KwIs => Some("is"),
            TokenKind::KwRef => Some("ref"),
            TokenKind::KwStatic => Some("static"),
            TokenKind::KwExtern => Some("extern"),
            TokenKind::KwCrate => Some("crate"),
            TokenKind::KwAnyerror => Some("anyerror"),
            // 类型关键字也要能在"名字位置"退化成标识符：`#[inline(never)]`
            // 里的 `never` 就是这种用法。缺了这条，`consume_ident` 会直接
            // 报错，`inline(never)` 根本解析不出来。
            TokenKind::TyNever => Some("never"),
            TokenKind::TyUsize => Some("usize"),
            TokenKind::TyIsize => Some("isize"),
            TokenKind::TyBool => Some("bool"),
            TokenKind::TyStr => Some("str"),
            TokenKind::TyChar => Some("char"),
            _ => None,
        }
    }
}
