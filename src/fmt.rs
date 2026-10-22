//! `zr fmt` —— 源码格式化器。
//!
//! 走**词法**而不是 AST：格式化器必须在源码有语法错误时也能工作
//! （IDE 里按一下保存键就期待有输出），而且这样不需要完整的类型检查。
//!
//! 设计取向是"保守重排"：
//!
//! - **保留作者换行**，只规范缩进与记号之间的空格；
//! - 保留全部注释（词法器在 `keep_comments` 模式下产出注释 token）；
//! - 不重排语句、不合并/拆分表达式。
//!
//! 代价是不会把超长的表达式折行，好处是**不可能把能跑的代码改成不能跑**，
//! 而且格式化是幂等的：`fmt(fmt(x)) == fmt(x)`。
//!
//! 具体规则：
//!
//! | 规则 | 说明 |
//! |------|------|
//! | 缩进 | 4 空格一层，由 `{` `(` `[` 决定，`}` `)` `]` 回落 |
//! | 行尾空白 | 一律去掉 |
//! | 空行 | 连续空行压成 1 个；文件首尾不留空行；结尾恰好一个换行 |
//! | CRLF / Tab | 统一成 LF / 4 空格 |
//! | 二元运算符 | 两侧各一个空格 |
//! | `.` `::` | 紧贴，不留空格 |
//! | `,` `;` | 后面一个空格（行尾除外），前面不留 |
//! | 括号 | 内侧不留空格 |

use crate::lexer::{Lexer, Token, TokenKind};

/// 一级缩进。
const INDENT: &str = "    ";

/// 格式化一段源码。
pub fn format_source(source: &str) -> String {
    let tokens = Lexer::tokenize_keeping_comments(source);
    Formatter::new(&tokens).run()
}

/// 把 CRLF / 单独的 CR 统一成 LF。
pub fn normalize_line_endings(source: &str) -> String {
    source.replace("\r\n", "\n").replace('\r', "\n")
}

struct Formatter {
    tokens: Vec<Token>,
    idx: usize,
    /// 当前行的内容（不含缩进）。
    line: String,
    /// 当前行的缩进层级。
    indent: usize,
    out: String,
    /// 上一个已输出的记号（决定间距）。
    prev: Option<TokenKind>,
    /// 上一个记号是否是**前缀**用法（`-x` / `&y` / `*p`）。
    ///
    /// `*` 既可能是解引用（前缀）也可能是乘（二元），光看记号种类分不出来；
    /// 判据是它前面有没有操作数。
    prev_is_prefix: bool,
    /// 上一行是否为空行（压缩连续空行用）。
    prev_blank: bool,
    /// 是否已经输出过任何内容（抑制文件开头的空行）。
    started: bool,
    /// 上一个记号的源码行号。
    prev_end_line: usize,
    /// 当前处于函数参数列表 / struct 字段 / enum 变体列表的深度。
    /// 用于判断是否允许 trailing comma。
    in_param_list_depth: usize,
    /// 是否刚输出过左括号 `(`, `[`, `{`，用于判断是否为首个元素。
    just_after_open_delim: bool,
    /// 是否在 match arm 中（输出了 `=>` 后等待分支体）。
    in_match_arm: bool,
}

impl Formatter {
    fn new(tokens: &[Token]) -> Self {
        // 末尾的 Eof 由 run() 单独处理
        let mut toks: Vec<Token> = tokens.to_vec();
        toks.pop();
        Formatter {
            tokens: toks,
            idx: 0,
            line: String::new(),
            indent: 0,
            out: String::new(),
            prev: None,
            prev_is_prefix: false,
            prev_blank: true,
            started: false,
            prev_end_line: 0,
            in_param_list_depth: 0,
            just_after_open_delim: false,
            in_match_arm: false,
        }
    }

    fn run(mut self) -> String {
        while self.idx < self.tokens.len() {
            let kind = self.tokens[self.idx].kind.clone();
            self.idx += 1;
            match kind {
                TokenKind::LineComment(text) => self.push_line_comment(&text),
                TokenKind::BlockComment(text) => self.push_block_comment(&text),
                TokenKind::Eof => break,
                k => self.push_code(k),
            }
        }
        self.flush_line();
        // 结尾恰好一个换行
        while self.out.ends_with('\n') {
            self.out.pop();
        }
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        self.out
    }

    /// 收尾当前行。
    fn flush_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        for _ in 0..self.indent {
            self.out.push_str(INDENT);
        }
        self.out.push_str(&self.line);
        self.out.push('\n');
        self.line.clear();
        self.prev = None;
    }

    /// 空一行（连续的空行会被压掉）。
    fn blank_line(&mut self) {
        // 已经在文件开头，或上一行本身就是空行，就不再加
        if self.out.is_empty() || self.out.ends_with("\n\n") {
            return;
        }
        self.out.push('\n');
    }

    /// 行注释：前面有代码就作为尾随注释同行，否则独占一行并跟随缩进。
    fn push_line_comment(&mut self, text: &str) {
        let text = text.trim_end().to_string();
        if !self.line.is_empty() {
            self.line.push(' ');
            self.line.push_str(&text);
            self.flush_line();
        } else {
            // 缩进直接写进 out。绕道 `self.line` 会多出一个只含缩进的
            // 空行（文件开头 indent=0 时就是纯空行）。
            self.push_indent();
            self.out.push_str(&text);
            self.out.push('\n');
        }
        // 行注释一定结束当前行，后面不需要间距信息
        self.prev = None;
        self.prev_blank = false;
        self.started = true;
    }

    /// 把当前缩进写到 `out`。
    fn push_indent(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str(INDENT);
        }
    }

    /// 块注释：能放行内就放行内，跨行则按原样缩进输出。
    fn push_block_comment(&mut self, text: &str) {
        if text.contains('\n') {
            // 跨行块注释：按原样保留内部换行，只重排首尾
            if !self.line.is_empty() {
                self.line.push(' ');
            }
            let mut it = text.lines();
            if let Some(first) = it.next() {
                self.line.push_str(first);
            }
            self.flush_line();
            for l in it {
                self.push_indent();
                self.out.push_str(l.trim_end());
                self.out.push('\n');
            }
        } else {
            if !self.line.is_empty() {
                self.line.push(' ');
            }
            self.line.push_str(text.trim_end());
        }
        self.prev = Some(TokenKind::BlockComment(String::new()));
        self.prev_blank = false;
        self.started = true;
    }

    /// 普通代码记号。
    fn push_code(&mut self, kind: TokenKind) {
        // 还原源码里的换行。空行判定不能挂在 `!self.line.is_empty()` 上：
        // `{` 之后行已经是空的，那里的空行就永远检测不到。
        if self.idx >= 2 {
            let prev_line = self.tokens[self.idx - 2].span.line;
            let cur_line = self.tokens[self.idx - 1].span.line;
            if cur_line > prev_line {
                // 文件开头之前不算空行
                let had_blank = cur_line > prev_line + 1 && self.started;
                self.flush_line();
                if had_blank {
                    self.blank_line();
                }
                self.prev_end_line = cur_line;
            }
        }
        // 闭括号要先退一层缩进再落到行上；顺序反了就会多缩进一级。
        if matches!(kind, TokenKind::RBrace) {
            self.indent = self.indent.saturating_sub(1);
        }

        // 跟踪参数列表深度：进入函数参数/struct字段/enum变体列表
        if matches!(
            kind,
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace
        ) {
            // 检查是否是函数参数列表（前面是 fn/struct/enum 或在 where 之后）
            let is_param_list = matches!(
                self.prev,
                Some(TokenKind::KwFn)
                    | Some(TokenKind::KwStruct)
                    | Some(TokenKind::KwEnum)
                    | Some(TokenKind::KwWhere)
                    | Some(TokenKind::Arrow)
                    | Some(TokenKind::KwType)
                    | Some(TokenKind::KwImpl)
            );
            if is_param_list {
                self.in_param_list_depth += 1;
            }
            self.just_after_open_delim = true;
        }

        // 处理逗号：在参数列表中，逗号后加空格；如果是 trailing comma（后面紧跟右括号），保留
        if matches!(kind, TokenKind::Comma) {
            if self.in_param_list_depth > 0 {
                self.line.push_str(", ");
            } else {
                self.line.push(',');
            }
            // match arm 中的逗号：断行
            if self.in_match_arm {
                self.flush_line();
                self.in_match_arm = false;
            }
            self.prev = Some(kind);
            self.prev_is_prefix = false;
            self.just_after_open_delim = false;
            return;
        }

        // 处理右括号：可能结束参数列表（不含 RBrace，已单独处理）
        let closing_param_list =
            self.in_param_list_depth > 0 && matches!(kind, TokenKind::RParen | TokenKind::RBracket);

        let sep = self.separator_before(&kind);
        if !self.line.is_empty() && sep {
            self.line.push(' ');
        }
        self.line.push_str(&render(&kind));
        self.started = true;

        // 前缀用法要靠"前面有没有操作数"判定，所以先记下来给下一个记号用
        self.prev_is_prefix =
            is_prefix_op(&kind) && !self.prev.as_ref().map(ends_expression).unwrap_or(true);

        // 闭括号要先刷新当前行，然后再退一层缩进；顺序反了会导致最后一行缩进不对。
        if matches!(kind, TokenKind::RBrace) {
            self.flush_line();
            self.indent = self.indent.saturating_sub(1);
        } else {
            match kind {
                // 行尾记号：立刻断行；`{` 之后进入一层缩进
                TokenKind::Semicolon => self.flush_line(),
                TokenKind::LBrace => {
                    self.flush_line();
                    self.indent += 1;
                }
                TokenKind::RBrace => {} // 已在上面处理
                TokenKind::Rocket => {
                    // 进入 match arm：标记在 match arm 中，不在此处断行
                    // 模式和 `=>` 保持在同一行，分支体跟在后面
                    self.in_match_arm = true;
                }
                _ => {}
            }

            // match arm 结束：逗号后断行
            if self.in_match_arm && matches!(kind, TokenKind::Comma) {
                self.flush_line();
                self.in_match_arm = false;
            }
        } // 闭合 else 块

        if closing_param_list && self.in_param_list_depth > 0 {
            self.in_param_list_depth -= 1;
        }
        self.just_after_open_delim = false;
        self.prev = Some(kind);
    }

    /// 记号之前是否需要空格。
    fn separator_before(&self, kind: &TokenKind) -> bool {
        let Some(prev) = &self.prev else {
            return false;
        };
        // 贴左：右括号、标点、路径分隔
        if matches!(
            kind,
            TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::Comma
                | TokenKind::Semicolon
                | TokenKind::Colon
                | TokenKind::Dot
                | TokenKind::ColonColon
                | TokenKind::Question
                | TokenKind::DotDot
                | TokenKind::DotDotEq
        ) {
            return false;
        }
        // 贴左：左括号之后、点/路径之后、属性之后
        if matches!(
            prev,
            TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::Dot
                | TokenKind::ColonColon
                | TokenKind::At
                | TokenKind::Hash
        ) {
            return false;
        }
        // `=>` 在 match arm 中：前后各一个空格
        if matches!(kind, TokenKind::Rocket) {
            return true;
        }
        // `=>` 之后不需要空格（表达式紧跟） - 改为需要空格
        // if matches!(prev, TokenKind::Rocket) {
        //     return false;
        // }
        // 前缀运算符后面直接跟操作数
        if self.prev_is_prefix {
            return false;
        }
        // 调用与分组：操作数 / 右括号 / 控制关键字后的 `(` 贴左
        if matches!(kind, TokenKind::LParen)
            && (ends_expression(prev)
                || matches!(
                    prev,
                    TokenKind::KwFn
                        | TokenKind::KwIf
                        | TokenKind::KwWhile
                        | TokenKind::KwFor
                        | TokenKind::KwMatch
                        | TokenKind::KwLoop
                        | TokenKind::KwReturn
                        | TokenKind::KwUse
                        | TokenKind::KwStruct
                        | TokenKind::KwEnum
                ))
        {
            return false;
        }
        true
    }
}

/// 该记号出现在"操作数位置"（前面没有操作数），于是 `*`/`-`/`&` 是前缀。
///
/// 光看记号种类不够：`2 * 3` 里的 `*` 是二元、`2 * p` 里的也是，
/// 但 `-x` 的 `-` 是前缀。判据是前一个记号**能不能结束一个表达式**。
fn ends_expression(prev: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        prev,
        Ident(_)
            | IntLiteral(..)
            | FloatLiteral(..)
            | StringLiteral(_)
            | CharLiteral(_)
            | Lifetime(_)
            | KwTrue
            | KwFalse
            | KwSelf
            | KwUpperSelf
            | RParen
            | RBracket
            | RBrace
    )
}

/// 前缀运算符：后面直接跟操作数。
fn is_prefix_op(k: &TokenKind) -> bool {
    matches!(
        k,
        TokenKind::Minus | TokenKind::Bang | TokenKind::Amp | TokenKind::Tilde | TokenKind::Star
    )
}

/// 记号的规范写法。
fn render(kind: &TokenKind) -> String {
    use TokenKind::*;
    match kind {
        Ident(s) => s.clone(),
        IntLiteral(v, suffix) => format!("{v}{}", suffix.as_deref().unwrap_or("")),
        // `{}` 会把 100.0 打成 "100"——浮点字面量丢了小数点就变成整数字面量
        // 了，类型也跟着变。Debug 格式（`{:?}`）给的是最短的往返表示。
        FloatLiteral(v, suffix) => {
            let text = format!("{v:?}");
            // 词法只在见过小数点之后才认指数（`1e-5` 会被读成整数 1 加后缀
            // `e-5`），所以指数形式要补上小数点。
            let text = match text.find(['e', 'E']) {
                Some(pos) if !text[..pos].contains('.') => {
                    format!("{}.0{}", &text[..pos], &text[pos..])
                }
                _ if !text.contains('.') => format!("{text}.0"),
                _ => text,
            };
            format!("{text}{}", suffix.as_deref().unwrap_or(""))
        }
        StringLiteral(s) => format!("{s:?}"),
        CharLiteral(c) => format!("{c:?}"),
        LineComment(s) | BlockComment(s) => s.clone(),
        // 类型记号带位宽负载，`i32` / `f64` 不能靠判别式查表
        TyI(w) => format!("i{w}"),
        TyU(w) => format!("u{w}"),
        TyF(w) => format!("f{w}"),
        Lifetime(s) => format!("'{s}"),
        LexError(m) => format!("<lex error: {m}>"),
        // 关键字与符号用固定表；没有的退回调试名，避免静默输出空串
        other => SYMBOLS
            .iter()
            .find(|(k, _)| std::mem::discriminant(k) == std::mem::discriminant(other))
            .map(|(_, s)| (*s).to_string())
            .unwrap_or_else(|| format!("{other:?}")),
    }
}

/// 记号 -> 规范文本。带 payload 的记号不在表里（由 `render` 单独处理）。
const SYMBOLS: &[(TokenKind, &str)] = &[
    (TokenKind::KwFn, "fn"),
    (TokenKind::KwLet, "let"),
    (TokenKind::KwConst, "const"),
    (TokenKind::KwMut, "mut"),
    (TokenKind::KwIf, "if"),
    (TokenKind::KwElse, "else"),
    (TokenKind::KwMatch, "match"),
    (TokenKind::KwWhile, "while"),
    (TokenKind::KwFor, "for"),
    (TokenKind::KwLoop, "loop"),
    (TokenKind::KwBreak, "break"),
    (TokenKind::KwContinue, "continue"),
    (TokenKind::KwReturn, "return"),
    (TokenKind::KwDefer, "defer"),
    (TokenKind::KwTry, "try"),
    (TokenKind::KwCatch, "catch"),
    (TokenKind::KwThrow, "throw"),
    (TokenKind::KwAsync, "async"),
    (TokenKind::KwAwait, "await"),
    (TokenKind::KwYield, "yield"),
    (TokenKind::KwSpawn, "spawn"),
    (TokenKind::KwComptime, "comptime"),
    (TokenKind::KwDefault, "default"),
    (TokenKind::KwGoto, "goto"),
    (TokenKind::KwStruct, "struct"),
    (TokenKind::KwEnum, "enum"),
    (TokenKind::KwUnion, "union"),
    (TokenKind::KwTrait, "trait"),
    (TokenKind::KwImpl, "impl"),
    (TokenKind::KwType, "type"),
    (TokenKind::KwWhere, "where"),
    (TokenKind::KwPub, "pub"),
    (TokenKind::KwMod, "mod"),
    (TokenKind::KwUse, "use"),
    (TokenKind::KwAs, "as"),
    (TokenKind::KwSelf, "self"),
    (TokenKind::KwUpperSelf, "Self"),
    (TokenKind::KwSuper, "super"),
    (TokenKind::KwTrue, "true"),
    (TokenKind::KwFalse, "false"),
    (TokenKind::KwVoid, "void"),
    (TokenKind::KwAnytype, "anytype"),
    (TokenKind::KwAuto, "auto"),
    (TokenKind::KwIn, "in"),
    (TokenKind::KwIs, "is"),
    (TokenKind::KwRef, "ref"),
    (TokenKind::KwStatic, "static"),
    (TokenKind::KwExtern, "extern"),
    (TokenKind::KwCrate, "crate"),
    (TokenKind::TyUsize, "usize"),
    (TokenKind::TyIsize, "isize"),
    (TokenKind::TyBool, "bool"),
    (TokenKind::TyStr, "str"),
    (TokenKind::TyChar, "char"),
    (TokenKind::TyNever, "never"),
    (TokenKind::KwAnyerror, "anyerror"),
    (TokenKind::Plus, "+"),
    (TokenKind::Minus, "-"),
    (TokenKind::Star, "*"),
    (TokenKind::Slash, "/"),
    (TokenKind::Percent, "%"),
    (TokenKind::Eq, "="),
    (TokenKind::EqEq, "=="),
    (TokenKind::BangEq, "!="),
    (TokenKind::Lt, "<"),
    (TokenKind::Gt, ">"),
    (TokenKind::Le, "<="),
    (TokenKind::Ge, ">="),
    (TokenKind::Bang, "!"),
    (TokenKind::Amp, "&"),
    (TokenKind::Pipe, "|"),
    (TokenKind::Caret, "^"),
    (TokenKind::Tilde, "~"),
    (TokenKind::Shl, "<<"),
    (TokenKind::Shr, ">>"),
    (TokenKind::AmpAmp, "&&"),
    (TokenKind::PipePipe, "||"),
    (TokenKind::DotDot, ".."),
    (TokenKind::DotDotEq, "..="),
    (TokenKind::Rocket, "=>"),
    (TokenKind::Arrow, "->"),
    (TokenKind::ColonColon, "::"),
    (TokenKind::ColonEq, ":="),
    (TokenKind::PlusEq, "+="),
    (TokenKind::MinusEq, "-="),
    (TokenKind::StarEq, "*="),
    (TokenKind::SlashEq, "/="),
    (TokenKind::PercentEq, "%="),
    (TokenKind::AmpEq, "&="),
    (TokenKind::PipeEq, "|="),
    (TokenKind::CaretEq, "^="),
    (TokenKind::ShlEq, "<<="),
    (TokenKind::ShrEq, ">>="),
    (TokenKind::PlusPlus, "++"),
    (TokenKind::HashHash, "##"),
    (TokenKind::LParen, "("),
    (TokenKind::RParen, ")"),
    (TokenKind::LBrace, "{"),
    (TokenKind::RBrace, "}"),
    (TokenKind::LBracket, "["),
    (TokenKind::RBracket, "]"),
    (TokenKind::Comma, ","),
    (TokenKind::Dot, "."),
    (TokenKind::Colon, ":"),
    (TokenKind::Semicolon, ";"),
    (TokenKind::Hash, "#"),
    (TokenKind::Underscore, "_"),
    (TokenKind::Question, "?"),
    (TokenKind::At, "@"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(s: &str) -> String {
        format_source(s)
    }

    #[test]
    fn normalizes_indentation() {
        let got = fmt("fn main() -> i32 {\nreturn 0;\n}");
        assert_eq!(got, "fn main() -> i32 {\n    return 0;\n}\n");
    }

    #[test]
    fn is_idempotent() {
        let src = "fn  f( a:i32 )->i32{\nlet x=a+1;\nreturn x;\n}\n";
        let once = fmt(src);
        assert_eq!(fmt(&once), once, "格式化必须幂等");
    }

    #[test]
    fn preserves_comments() {
        let src = "// 头部注释\nfn main() {\n    // 行内注释\n    return 0; // 尾随\n}\n";
        let got = fmt(src);
        assert!(got.contains("// 头部注释"), "丢失了头部注释: {got}");
        assert!(got.contains("// 行内注释"), "丢失了块内注释: {got}");
        assert!(got.contains("// 尾随"), "丢失了尾随注释: {got}");
    }

    #[test]
    fn collapses_blank_lines() {
        let got = fmt("fn main() {\n\n\n\n    return 0;\n}\n");
        assert_eq!(got, "fn main() {\n\n    return 0;\n}\n");
    }

    #[test]
    fn normalizes_crlf_and_tabs() {
        let got = fmt("fn main() {\r\n\treturn 0;\r\n}\r\n");
        assert!(!got.contains('\r'), "CRLF 未被归一: {got:?}");
        assert!(!got.contains('\t'), "Tab 未被替换: {got:?}");
    }

    #[test]
    fn operator_spacing() {
        let got = fmt("fn main() -> i32 {\n    return 1+2*3;\n}\n");
        assert!(got.contains("1 + 2 * 3"), "运算符间距不对: {got}");
    }

    #[test]
    fn strips_trailing_whitespace() {
        let got = fmt("fn main() {   \n    return 0;  \n}\n");
        for l in got.lines() {
            assert_eq!(l, l.trim_end(), "存在行尾空白: {l:?}");
        }
    }

    #[test]
    fn ends_with_single_newline() {
        let got = fmt("fn main() {\n    return 0;\n}\n\n\n");
        assert!(got.ends_with("}\n"));
        assert!(!got.ends_with("\n\n"));
    }

    #[test]
    fn formats_match_arms() {
        let got = fmt("fn main()->i32{match 1{1=>2,2=>3,_=>0}}");
        assert!(got.contains("1 => 2"), "match arm spacing: {got}");
        assert!(got.contains("2 => 3"), "match arm spacing: {got}");
        assert!(got.contains("_ => 0"), "match arm spacing: {got}");
    }

    #[test]
    fn formats_usize_isize_keywords() {
        let got = fmt("fn f(x: usize, y: isize) -> usize { return x + y as usize; }");
        assert!(got.contains("usize"), "usize missing: {got}");
        assert!(got.contains("isize"), "isize missing: {got}");
    }

    #[test]
    fn trailing_comma_in_fn_params() {
        let got = fmt("fn foo(a:i32,b:i32,)->i32{return a+b;}");
        assert!(
            got.contains("a: i32, b: i32,"),
            "trailing comma in params: {got}"
        );
    }
}
