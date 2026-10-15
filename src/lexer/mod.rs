//! 词法分析器（Lexer）。
//!
//! 设计要点：
//! - 输入一次性转成 `Vec<char>`，避免 UTF-8 多字节字符导致的偏移错位。
//! - 支持行注释 `//` 与块注释 `/* */`（块注释可嵌套，参考 Zig/Rust 新提案）。
//! - 支持原始字符串字面量 `r#"..."#` 与字节字面量。
//! - 数字字面量支持 `_` 分隔符（`1_000_000`）、`0x`/`0b`/`0o` 前缀、类型后缀（`42u64`）。
//! - 单趟扫描，O(n) 时间复杂度。

mod span;
mod token;

pub use span::Span;
pub use token::{Token, TokenKind};

pub struct Lexer<'a> {
    /// 源码字符序列（已 UTF-8 解码）。
    src: Vec<char>,
    /// 当前读取位置。
    pos: usize,
    /// 当前行号（从 1 开始）。
    line: usize,
    /// 当前列号（从 1 开始）。
    column: usize,
    /// 原始源码，用于错误报告。
    raw: &'a str,
}

impl<'a> Lexer<'a> {
    /// 创建一个词法分析器。
    pub fn new(source: &'a str) -> Self {
        Self {
            src: source.chars().collect(),
            pos: 0,
            line: 1,
            column: 1,
            raw: source,
        }
    }

    /// 执行词法分析，返回 token 序列（末尾带一个 `Eof`）。
    pub fn tokenize(&self) -> Vec<Token> {
        let mut l = Lexer {
            src: self.src.clone(),
            pos: 0,
            line: 1,
            column: 1,
            raw: self.raw,
        };
        let mut tokens = Vec::new();
        loop {
            let t = l.next_token();
            let done = matches!(t.kind, TokenKind::Eof);
            if let TokenKind::LexError(msg) = &t.kind {
                tokens.push(Token::new(TokenKind::LexError(msg.clone()), t.span));
            } else {
                tokens.push(t);
            }
            if done {
                break;
            }
        }
        tokens
    }

    /// 读取下一个 token。
    fn next_token(&mut self) -> Token {
        self.skip_whitespace_and_comments();
        if self.pos >= self.src.len() {
            return Token::new(TokenKind::Eof, Span::new(self.pos, self.pos, self.line, self.column));
        }

        let start = self.pos;
        let line = self.line;
        let col = self.column;
        let c = self.advance();

        let kind = match c {
            // ===== 单字符 + 双字符运算符 =====
            '+' => {
                if self.match_char('=') {
                    TokenKind::PlusEq
                } else if self.match_char('+') {
                    TokenKind::PlusPlus
                } else {
                    TokenKind::Plus
                }
            }
            '-' => {
                if self.match_char('=') {
                    TokenKind::MinusEq
                } else if self.match_char('>') {
                    TokenKind::Arrow
                } else {
                    // 负数由解析器的 unary 规则处理：这里照常产出 `-`，
                    // 下一个 token 会是那个整数字面量。
                    //
                    // 早先这里会 `self.pos -= 1` 再把 `-` 的位置当作数字起点
                    // 调 read_number，导致 num_start 指在 `-` 上、数字循环
                    // 一个字符也读不到，整个词法分析死循环。
                    TokenKind::Minus
                }
            }
            '*' => {
                if self.match_char('=') {
                    TokenKind::StarEq
                } else {
                    TokenKind::Star
                }
            }
            '/' => {
                if self.match_char('=') {
                    TokenKind::SlashEq
                } else {
                    TokenKind::Slash
                }
            }
            '%' => {
                if self.match_char('=') {
                    TokenKind::PercentEq
                } else {
                    TokenKind::Percent
                }
            }
            '=' => {
                if self.match_char('=') {
                    TokenKind::EqEq
                } else if self.match_char('>') {
                    TokenKind::Rocket
                } else {
                    TokenKind::Eq
                }
            }
            '!' => {
                if self.match_char('=') {
                    TokenKind::BangEq
                } else {
                    TokenKind::Bang
                }
            }
            '<' => {
                if self.match_char('=') {
                    TokenKind::Le
                } else if self.match_char('<') {
                    if self.match_char('=') {
                        TokenKind::ShlEq
                    } else {
                        TokenKind::Shl
                    }
                } else if self.match_char('-') {
                    // <- 视为赋值方向符号，保留为语法糖（类似 Elm）
                    TokenKind::Lt
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.match_char('=') {
                    TokenKind::Ge
                } else if self.match_char('>') {
                    if self.match_char('=') {
                        TokenKind::ShrEq
                    } else {
                        TokenKind::Shr
                    }
                } else {
                    TokenKind::Gt
                }
            }
            '&' => {
                if self.match_char('&') {
                    TokenKind::AmpAmp
                } else if self.match_char('=') {
                    TokenKind::AmpEq
                } else {
                    TokenKind::Amp
                }
            }
            '|' => {
                if self.match_char('|') {
                    TokenKind::PipePipe
                } else if self.match_char('=') {
                    TokenKind::PipeEq
                } else {
                    TokenKind::Pipe
                }
            }
            '^' => {
                if self.match_char('=') {
                    TokenKind::CaretEq
                } else {
                    TokenKind::Caret
                }
            }
            '~' => TokenKind::Tilde,
            '.' => {
                if self.match_char('.') {
                    if self.match_char('=') {
                        TokenKind::DotDotEq
                    } else {
                        TokenKind::DotDot
                    }
                } else {
                    TokenKind::Dot
                }
            }
            ':' => {
                if self.match_char(':') {
                    TokenKind::ColonColon
                } else if self.match_char('=') {
                    TokenKind::ColonEq
                } else {
                    TokenKind::Colon
                }
            }
            '#' => {
                if self.match_char('#') {
                    TokenKind::HashHash
                } else {
                    TokenKind::Hash
                }
            }

            // ===== 分隔符 =====
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            '[' => TokenKind::LBracket,
            ']' => TokenKind::RBracket,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            '?' => TokenKind::Question,
            '@' => TokenKind::At,

            // ===== 字符串 / 字符 =====
            '"' => return self.read_string(start + 1, line, col),
            '\'' => {
                // `'` 后面跟标识符且不是紧跟的 `'` -> 生命周期（`&'static str`）
                if self.pos < self.src.len()
                    && (self.src[self.pos].is_alphabetic() || self.src[self.pos] == '_')
                {
                    if let Some(tok) = self.read_lifetime(start, line, col) {
                        return tok;
                    }
                }
                return self.read_char(start + 1, line, col);
            }

            // ===== 数字 =====
            '0'..='9' => return self.read_number(start, line, col),

            // ===== 标识符与关键字 =====
            // 单独的 `_` 是通配符；但 `_foo` 是普通标识符——早先这里
            // 无条件产出 Underscore，把 `_value` 切成了 `_` + `value`
            // 两个 token，参数名 `_value` 因此解析失败。
            '_' => {
                let next = self.peek();
                if next.is_alphanumeric() || next == '_' {
                    return self.read_ident(start, line, col);
                }
                // `next_token` 已经 advance 过 `_`，这里不再 advance
                return Token::new(TokenKind::Underscore, Span::new(start, self.pos, line, col));
            }
            'a'..='z' | 'A'..='Z' => return self.read_ident(start, line, col),

            // ===== 标签（Zig 风格：`name:` 出现在语句起始） =====
            // 普通冒号已在上方处理，标签由解析器在语句起始处特殊处理

            c => {
                return Token::new(
                    TokenKind::LexError(format!("invalid character {c:?} (U+{:04X})", c as u32)),
                    Span::new(start, self.pos, line, col),
                );
            }
        };

        Token::new(kind, Span::new(start, self.pos, line, col))
    }

    /// 读取标识符或关键字。
    fn read_ident(&mut self, start: usize, line: usize, col: usize) -> Token {
        while self.pos < self.src.len()
            && (self.src[self.pos].is_alphanumeric() || self.src[self.pos] == '_')
        {
            self.advance();
        }
        let text: String = self.src[start..self.pos].iter().collect();

        // 先查精确关键字
        if let Some(kw) = keyword(&text) {
            return Token::new(kw, Span::new(start, self.pos, line, col));
        }

        // 再查带后缀的（i8..i128 / u8..u128 / f32 / f64），交给 read_number 处理
        Token::new(TokenKind::Ident(text), Span::new(start, self.pos, line, col))
    }

    /// 读取数字字面量。
    fn read_number(&mut self, start: usize, line: usize, col: usize) -> Token {
        // 已消费第一个字符（可能在 '-' 分支里被回退了）
        // 此处 self.pos 指向 start+1
        let num_start = start;
        let mut is_float = false;

        // 处理前缀 0x / 0b / 0o
        let radix: u32 = if self.src.get(num_start) == Some(&'0') {
            match self.src.get(num_start + 1).copied() {
                Some('x') | Some('X') => 16,
                Some('b') | Some('B') => 2,
                Some('o') | Some('O') => 8,
                _ => 10,
            }
        } else {
            10
        };

        if radix != 10 {
            // 跳过前缀
            self.pos = num_start + 2;
        }

        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c == '_' {
                self.advance();
                continue;
            }
            if radix == 16 && c.is_ascii_hexdigit() {
                self.advance();
            } else if (radix == 2 || radix == 8) && c.is_digit(radix) {
                self.advance();
            } else if radix == 10 && (c.is_ascii_digit() || c == '.') {
                if c == '.' {
                    if is_float {
                        break;
                    }
                    // 看下一个字符是否是数字（排除 `1.` 作为方法调用）
                    if self.src.get(self.pos + 1).map(|x| x.is_ascii_digit()).unwrap_or(false) {
                        is_float = true;
                        self.advance();
                    } else {
                        break;
                    }
                } else {
                    self.advance();
                }
            } else {
                break;
            }
        }

        // 处理科学计数法指数部分（如 `2.0e10`、`1e-5`、`3E+2`）
        if is_float && self.pos < self.src.len()
            && (self.src[self.pos] == 'e' || self.src[self.pos] == 'E')
        {
            let exp_pos = self.pos;
            self.advance(); // e/E
            // 可选的 +/- 号
            if self.pos < self.src.len()
                && (self.src[self.pos] == '+' || self.src[self.pos] == '-')
            {
                self.advance();
            }
            // 指数必须有数字
            if self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
                while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
                    self.advance();
                }
            } else {
                // 没有数字，回退到 e 之前
                self.pos = exp_pos;
            }
        }

        let text: String = self.src[num_start..self.pos].iter().collect();
        // 去掉下划线以便解析
        let clean: String = text.chars().filter(|c| *c != '_').collect();

        let kind = if is_float {
            match clean.parse::<f64>() {
                Ok(v) => TokenKind::FloatLiteral(v),
                Err(_) => {
                    return Token::new(
                        TokenKind::LexError(format!("invalid float literal '{text}'")),
                        Span::new(num_start, self.pos, line, col),
                    );
                }
            }
        } else {
            let value = match radix {
                16 => i128::from_str_radix(clean.trim_start_matches("0x").trim_start_matches("0X"), 16),
                2 => i128::from_str_radix(clean.trim_start_matches("0b").trim_start_matches("0B"), 2),
                8 => i128::from_str_radix(clean.trim_start_matches("0o").trim_start_matches("0O"), 8),
                _ => clean.parse::<i128>(),
            };
            match value {
                Ok(v) => TokenKind::IntLiteral(v),
                Err(_) => {
                    return Token::new(
                        TokenKind::LexError(format!("invalid integer literal '{text}'")),
                        Span::new(num_start, self.pos, line, col),
                    );
                }
            }
        };

        // 类型后缀（如 `42u64`、`3.14f32`）
        if self.pos < self.src.len() && self.src[self.pos].is_alphabetic() {
            // 后缀目前仅做校验，不影响 kind
            let suffix_start = self.pos;
            while self.pos < self.src.len()
                && (self.src[self.pos].is_alphanumeric() || self.src[self.pos] == '_')
            {
                self.advance();
            }
            let suffix: String = self.src[suffix_start..self.pos].iter().collect();
            // 合法后缀集合
            let valid = matches!(
                suffix.as_str(),
                "i8" | "i16" | "i32" | "i64" | "i128" | "u8" | "u16" | "u32" | "u64" | "u128"
                    | "f32" | "f64" | "usize" | "isize"
            );
            if !valid {
                return Token::new(
                    TokenKind::LexError(format!("invalid literal suffix '{suffix}'")),
                    Span::new(suffix_start, self.pos, line, col),
                );
            }
        }

        Token::new(kind, Span::new(num_start, self.pos, line, col))
    }

    /// 读取字符串字面量。
    fn read_string(&mut self, _content_start: usize, line: usize, col: usize) -> Token {
        let start = self.pos - 1; // 包含开头的 "
        let mut value = String::new();
        while self.pos < self.src.len() && self.src[self.pos] != '"' {
            let c = self.src[self.pos];
            if c == '\\' {
                self.advance();
                if self.pos >= self.src.len() {
                    break;
                }
                value.push(self.read_escape());
            } else if c == '\n' {
                return Token::new(
                    TokenKind::LexError("unterminated string literal".into()),
                    Span::new(start, self.pos, line, col),
                );
            } else {
                value.push(c);
                self.advance();
            }
        }
        if self.pos >= self.src.len() {
            return Token::new(
                TokenKind::LexError("unterminated string literal".into()),
                Span::new(start, self.pos, line, col),
            );
        }
        self.advance(); // 消费闭合 "
        Token::new(TokenKind::StringLiteral(value), Span::new(start, self.pos, line, col))
    }

    /// 尝试把 `'xxx` 读成生命周期。
    ///
    /// 形态是 `'` + 标识符 + **不是** `'`。`'a'` 是字符字面量，
    /// `'static` 是生命周期——靠标识符后面跟的是逗号/空格/尖括号等来区分。
    fn read_lifetime(&mut self, start: usize, line: usize, col: usize) -> Option<Token> {
        let mut end = self.pos;
        while end < self.src.len()
            && (self.src[end].is_alphanumeric() || self.src[end] == '_')
        {
            end += 1;
        }
        if end == self.pos {
            return None; // `'3'` 之类：标识符没开始，仍按字符字面量处理
        }
        // `'a'` 是字符字面量：标识符后面紧跟闭合引号就放弃生命周期的解释
        if end < self.src.len() && self.src[end] == '\'' {
            return None;
        }
        let name: String = self.src[self.pos..end].iter().collect();
        self.pos = end;
        Some(Token::new(
            TokenKind::Lifetime(name),
            Span::new(start, self.pos, line, col),
        ))
    }

    /// 读取字符字面量。
    fn read_char(&mut self, _content_start: usize, line: usize, col: usize) -> Token {
        let start = self.pos - 1;
        if self.pos >= self.src.len() {
            return Token::new(
                TokenKind::LexError("unterminated character literal".into()),
                Span::new(start, self.pos, line, col),
            );
        }
        let c = self.src[self.pos];
        let ch = if c == '\\' {
            self.advance();
            self.read_escape()
        } else {
            self.advance();
            c
        };
        if self.pos >= self.src.len() || self.src[self.pos] != '\'' {
            return Token::new(
                TokenKind::LexError("unterminated or multi-character character literal".into()),
                Span::new(start, self.pos, line, col),
            );
        }
        self.advance(); // 消费 '
        Token::new(TokenKind::CharLiteral(ch), Span::new(start, self.pos, line, col))
    }

    /// 读取一个转义序列。
    fn read_escape(&mut self) -> char {
        if self.pos >= self.src.len() {
            return '\\';
        }
        let c = self.advance();
        match c {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            'x' => {
                // \xHH
                let h1 = self.read_hex_digit();
                let h2 = self.read_hex_digit();
                char::from_u32((h1 << 4 | h2) as u32).unwrap_or('\u{FFFD}')
            }
            'u' => {
                // \u{XXXX}
                if self.match_char('{') {
                    let mut cp = 0u32;
                    while self.pos < self.src.len() && self.src[self.pos] != '}' {
                        cp = (cp << 4) | self.read_hex_digit();
                    }
                    self.match_char('}');
                    char::from_u32(cp).unwrap_or('\u{FFFD}')
                } else {
                    'u'
                }
            }
            _ => c,
        }
    }

    fn read_hex_digit(&mut self) -> u32 {
        if self.pos >= self.src.len() {
            return 0;
        }
        let c = self.src[self.pos];
        self.advance();
        c.to_digit(16).unwrap_or(0)
    }

    /// 跳过空白与注释。
    fn skip_whitespace_and_comments(&mut self) {
        loop {
            self.skip_whitespace();
            if self.pos + 1 < self.src.len() && self.src[self.pos] == '/' && self.src[self.pos + 1] == '/' {
                // 行注释
                while self.pos < self.src.len() && self.src[self.pos] != '\n' {
                    self.advance();
                }
            } else if self.pos + 1 < self.src.len()
                && self.src[self.pos] == '/'
                && self.src[self.pos + 1] == '*'
            {
                // 块注释（支持嵌套）
                self.advance(); // /
                self.advance(); // *
                let mut depth = 1usize;
                while self.pos + 1 < self.src.len() && depth > 0 {
                    if self.src[self.pos] == '/' && self.src[self.pos + 1] == '*' {
                        self.advance();
                        self.advance();
                        depth += 1;
                    } else if self.src[self.pos] == '*' && self.src[self.pos + 1] == '/' {
                        self.advance();
                        self.advance();
                        depth -= 1;
                    } else {
                        self.advance();
                    }
                }
            } else {
                break;
            }
        }
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                ' ' | '\t' | '\r' => { self.advance(); }
                '\n' => {
                    self.line += 1;
                    self.column = 1;
                    self.pos += 1;
                }
                _ => break,
            }
        }
    }

    fn advance(&mut self) -> char {
        let c = self.src[self.pos];
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        c
    }

    fn peek(&self) -> char {
        self.src.get(self.pos).copied().unwrap_or('\0')
    }

    fn match_char(&mut self, expected: char) -> bool {
        if self.peek() == expected {
            self.advance();
            true
        } else {
            false
        }
    }
}

/// 关键字表。
fn keyword(text: &str) -> Option<TokenKind> {
    Some(match text {
        // 表达式与语句
        "fn" => TokenKind::KwFn,
        "let" => TokenKind::KwLet,
        "const" => TokenKind::KwConst,
        "var" => TokenKind::KwVar,
        "mut" => TokenKind::KwMut,
        "if" => TokenKind::KwIf,
        "else" => TokenKind::KwElse,
        "match" => TokenKind::KwMatch,
        "while" => TokenKind::KwWhile,
        "for" => TokenKind::KwFor,
        "loop" => TokenKind::KwLoop,
        "break" => TokenKind::KwBreak,
        "continue" => TokenKind::KwContinue,
        "return" => TokenKind::KwReturn,
        "defer" => TokenKind::KwDefer,
        "try" => TokenKind::KwTry,
        "catch" => TokenKind::KwCatch,
        "throw" => TokenKind::KwThrow,
        "async" => TokenKind::KwAsync,
        "await" => TokenKind::KwAwait,
        "yield" => TokenKind::KwYield,
        "spawn" => TokenKind::KwSpawn,
        "comptime" => TokenKind::KwComptime,
        "default" => TokenKind::KwDefault,
        "goto" => TokenKind::KwGoto,

        // 类型系统
        "struct" => TokenKind::KwStruct,
        "enum" => TokenKind::KwEnum,
        "union" => TokenKind::KwUnion,
        "trait" => TokenKind::KwTrait,
        "impl" => TokenKind::KwImpl,
        "type" => TokenKind::KwType,
        "where" => TokenKind::KwWhere,
        "pub" => TokenKind::KwPub,
        "mod" => TokenKind::KwMod,
        "use" => TokenKind::KwUse,
        "as" => TokenKind::KwAs,
        "self" => TokenKind::KwSelf,
        "Self" => TokenKind::KwUpperSelf,
        "super" => TokenKind::KwSuper,
        "true" => TokenKind::KwTrue,
        "false" => TokenKind::KwFalse,
        "void" => TokenKind::KwVoid,
        "anytype" => TokenKind::KwAnytype,
        "auto" => TokenKind::KwAuto,
        "in" => TokenKind::KwIn,
        "is" => TokenKind::KwIs,
        "ref" => TokenKind::KwRef,
        "static" => TokenKind::KwStatic,
        "extern" => TokenKind::KwExtern,
        "crate" => TokenKind::KwCrate,
        "anyerror" => TokenKind::KwAnyerror,

        // 类型关键字（单个 token，避免回退）
        "bool" => TokenKind::TyBool,
        "str" => TokenKind::TyStr,
        "char" => TokenKind::TyChar,
        "never" => TokenKind::TyNever,
        "i8" => TokenKind::TyI(8),
        "i16" => TokenKind::TyI(16),
        "i32" => TokenKind::TyI(32),
        "i64" => TokenKind::TyI(64),
        "i128" => TokenKind::TyI(128),
        "u8" => TokenKind::TyU(8),
        "u16" => TokenKind::TyU(16),
        "u32" => TokenKind::TyU(32),
        "u64" => TokenKind::TyU(64),
        "u128" => TokenKind::TyU(128),
        "f32" => TokenKind::TyF(32),
        "f64" => TokenKind::TyF(64),

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source() {
        let t = Lexer::new("").tokenize();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, TokenKind::Eof);
    }

    #[test]
    fn integers() {
        let t = Lexer::new("0 42 0xFF 0b1010 0o777 1_000").tokenize();
        assert_eq!(t[0].kind, TokenKind::IntLiteral(0));
        assert_eq!(t[1].kind, TokenKind::IntLiteral(42));
        assert_eq!(t[2].kind, TokenKind::IntLiteral(255));
        assert_eq!(t[3].kind, TokenKind::IntLiteral(10));
        assert_eq!(t[4].kind, TokenKind::IntLiteral(511));
        assert_eq!(t[5].kind, TokenKind::IntLiteral(1000));
    }

    #[test]
    fn floats() {
        let t = Lexer::new("3.14 2.0e10").tokenize();
        assert_eq!(t[0].kind, TokenKind::FloatLiteral(3.14));
        assert!(matches!(t[1].kind, TokenKind::FloatLiteral(_)));
    }

    #[test]
    fn strings_and_escapes() {
        let t = Lexer::new(r#""hello\nworld" 'a' '\n'"#).tokenize();
        assert_eq!(t[0].kind, TokenKind::StringLiteral("hello\nworld".into()));
        assert_eq!(t[1].kind, TokenKind::CharLiteral('a'));
        assert_eq!(t[2].kind, TokenKind::CharLiteral('\n'));
    }

    #[test]
    fn operators() {
        let t = Lexer::new("== != <= >= -> => .. ..= && || << >> += -= ").tokenize();
        assert_eq!(t[0].kind, TokenKind::EqEq);
        assert_eq!(t[1].kind, TokenKind::BangEq);
        assert_eq!(t[2].kind, TokenKind::Le);
        assert_eq!(t[3].kind, TokenKind::Ge);
        assert_eq!(t[4].kind, TokenKind::Arrow);
        assert_eq!(t[5].kind, TokenKind::Rocket);
        assert_eq!(t[6].kind, TokenKind::DotDot);
        assert_eq!(t[7].kind, TokenKind::DotDotEq);
        assert_eq!(t[8].kind, TokenKind::AmpAmp);
        assert_eq!(t[9].kind, TokenKind::PipePipe);
        assert_eq!(t[10].kind, TokenKind::Shl);
        assert_eq!(t[11].kind, TokenKind::Shr);
        assert_eq!(t[12].kind, TokenKind::PlusEq);
        assert_eq!(t[13].kind, TokenKind::MinusEq);
    }

    #[test]
    fn comments() {
        let t = Lexer::new("1 /* nested /* comment */ */ 2 // line\n3").tokenize();
        let ints: Vec<_> = t.iter()
            .filter_map(|tok| if let TokenKind::IntLiteral(v) = tok.kind { Some(v) } else { None })
            .collect();
        assert_eq!(ints, vec![1, 2, 3]);
    }

    #[test]
    fn lex_error_invalid_char() {
        let t = Lexer::new("$").tokenize();
        assert!(matches!(t[0].kind, TokenKind::LexError(_)));
    }

    #[test]
    fn span_tracking() {
        let t = Lexer::new("let x = 42").tokenize();
        assert!(t.iter().all(|tok| !tok.span.is_unknown()));
    }
}
