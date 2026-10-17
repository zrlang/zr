//! 模式解析。
//!
//! 模式用于 `let` / `match` / `for` / 函数参数解构。支持：
//! - 字面量模式
//! - 通配符 `_`
//! - 绑定 `x` / `ref mut x`
//! - 元组 / 结构体 / 枚举变体 / 切片 / 范围
//! - 守卫 `p if cond`
//!
//! 语法：
//! ```ebnf
//! pattern      := 'ref' mut? pattern
//!               | pattern '|' pattern          # 或模式（备选）
//!               | '~'? pattern                 # 否定（预留）
//!               | literal
//!               | '_'
//!               | ident (':' type)?
//!               | tuple_pattern
//!               | struct_pattern
//!               | variant_pattern
//!               | slice_pattern
//!               | range_pattern
//! ```

use crate::error::CompileError;
use crate::lexer::{Span, TokenKind};
use crate::parser::ast::*;
use crate::parser::Parser;

impl Parser {
    /// 解析一个模式（入口，允许或模式）。
    pub fn parse_pattern(&mut self) -> Result<Pattern, CompileError> {
        self.parse_pattern_or()
    }

    /// 解析或模式 `p1 | p2 | ...`。
    fn parse_pattern_or(&mut self) -> Result<Pattern, CompileError> {
        let pat = self.parse_pattern_single()?;
        // 目前或模式暂不展开为独立节点，第一个分支作为主模式
        while self.check(&TokenKind::Pipe) && self.next_can_start_pattern() {
            self.advance(); // |
            self.parse_pattern_single()?;
        }
        Ok(pat)
    }

    /// 判断下一个 token 是否可以作为模式的起始。
    fn next_can_start_pattern(&self) -> bool {
        self.peek_n(1)
            .map(|t| {
                matches!(
                    t.kind,
                    TokenKind::Ident(_)
                        | TokenKind::Underscore
                        | TokenKind::IntLiteral(..)
                        | TokenKind::FloatLiteral(..)
                        | TokenKind::CharLiteral(_)
                        | TokenKind::StringLiteral(_)
                        | TokenKind::KwTrue
                        | TokenKind::KwFalse
                        | TokenKind::KwRef
                        | TokenKind::KwMut
                        | TokenKind::Tilde
                        | TokenKind::Minus
                        | TokenKind::LParen
                        | TokenKind::LBracket
                )
            })
            .unwrap_or(false)
    }

    /// 解析单个模式。
    /// 解析变体模式的 `(p1, p2, ...)` 实参列表（不含左括号）。
    fn parse_pattern_call_args(&mut self) -> Result<Vec<Pattern>, CompileError> {
        let mut args = Vec::new();
        if !self.check(&TokenKind::RParen) {
            args.push(self.parse_pattern()?);
            while self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                args.push(self.parse_pattern()?);
            }
        }
        self.consume(&TokenKind::RParen, "expected ')'")?;
        Ok(args)
    }

    pub(crate) fn parse_pattern_single(&mut self) -> Result<Pattern, CompileError> {
        let span = self.peek().span;

        // `mut name`（可变绑定）
        if self.match_kind(&TokenKind::KwMut) {
            let name = self.consume_ident("expected name after 'mut'")?;
            return Ok(Pattern::Binding(
                name,
                crate::parser::ast::Mutability::Mutable,
                None,
                span,
            ));
        }

        // `ref mut? p`
        if self.match_kind(&TokenKind::KwRef) {
            let mutability = if self.match_kind(&TokenKind::KwMut) {
                crate::parser::ast::Mutability::Mutable
            } else {
                crate::parser::ast::Mutability::Immutable
            };
            let inner = self.parse_pattern_single()?;
            return Ok(Pattern::Ref(Box::new(inner), mutability, span));
        }

        // 否定模式 `~p`（预留）
        if self.match_kind(&TokenKind::Tilde) {
            let inner = self.parse_pattern_single()?;
            return Ok(inner);
        }

        // 范围模式 `1..=5` / 字面量模式（必须在字面量单独处理之前，
        // 否则 `1` 会被当作普通字面量而无法识别范围）
        if self.peek().kind.is_lit_or_minus() {
            // 只解析字面量前缀（不消费 `..` / `..=`，因为它们现在是表达式二元运算符）
            let start = self.parse_pattern_literal_expr()?;
            if self.match_kind(&TokenKind::DotDot) || self.match_kind(&TokenKind::DotDotEq) {
                let inclusive = self.previous().kind == TokenKind::DotDotEq;
                let end = self.parse_pattern_literal_expr()?;
                return Ok(Pattern::Range(
                    Box::new(start),
                    Box::new(end),
                    inclusive,
                    span,
                ));
            }
            // 不是范围模式，回退为字面量模式
            return if let Expression::Literal(lit, _) = start {
                Ok(Pattern::Literal(lit))
            } else {
                Err(CompileError::internal("expected pattern"))
            };
        }

        // 字符串字面量模式（不在 is_lit_or_minus 中）
        if let TokenKind::StringLiteral(s) = &self.peek().kind {
            let s = s.clone();
            self.advance();
            return Ok(Pattern::Literal(Literal::String(s)));
        }

        // 元组模式 `(p1, p2)`
        if self.match_kind(&TokenKind::LParen) {
            let mut pats = Vec::new();
            if !self.check(&TokenKind::RParen) {
                pats.push(self.parse_pattern()?);
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    pats.push(self.parse_pattern()?);
                }
            }
            self.consume(&TokenKind::RParen, "expected ')' to close tuple pattern")?;
            return Ok(Pattern::Tuple(pats, span));
        }

        // 切片模式 `[p1, .., p2]`
        if self.match_kind(&TokenKind::LBracket) {
            let mut pats = Vec::new();
            while !self.check(&TokenKind::RBracket) && !self.is_at_end() {
                pats.push(self.parse_pattern()?);
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
            }
            self.consume(&TokenKind::RBracket, "expected ']' to close slice pattern")?;
            return Ok(Pattern::Slice(pats, span));
        }

        // `_` 通配符 —— 用一个永远不会匹配的占位检查，
        // 由 TokenKind 扩展支持 `Underscore`
        if matches!(self.peek().kind, TokenKind::Underscore) {
            self.advance();
            return Ok(Pattern::Wildcard(span));
        }

        // 关键字当绑定名：`fn unwrap_or(self, default: T)` 里的 `default`。
        // 先把它还原成文本再走下面和普通标识符一样的路径。
        if let Some(kw) = self.peek().kind.keyword_text() {
            if !matches!(self.peek().kind, TokenKind::KwTrue | TokenKind::KwFalse) {
                let ident = kw.to_string();
                self.advance();
                return Ok(Pattern::Binding(
                    ident,
                    crate::parser::ast::Mutability::Immutable,
                    None,
                    span,
                ));
            }
        }

        // 绑定：`name` / `name @ subpat` / `mut name`
        if let TokenKind::Ident(name) = &self.peek().kind {
            let ident = name.clone();
            self.advance();

            // 限定路径模式：`Enum::Variant(p)` / `module::Type { .. }`
            if self.check(&TokenKind::ColonColon) {
                let mut path = vec![ident.clone()];
                while self.match_kind(&TokenKind::ColonColon) {
                    path.push(self.consume_path_segment("expected path segment in pattern")?);
                }
                if self.match_kind(&TokenKind::LBrace) {
                    return self.parse_struct_pattern(path, span);
                }
                if self.match_kind(&TokenKind::LParen) {
                    let args = self.parse_pattern_call_args()?;
                    return Ok(Pattern::Variant { path, args, span });
                }
                // unit 变体：`Color::Red => ...` —— 退化成最后一段的绑定名
                let last = path.last().cloned().unwrap_or(ident);
                return Ok(Pattern::Binding(
                    last,
                    crate::parser::ast::Mutability::Immutable,
                    None,
                    span,
                ));
            }

            // 枚举变体 / 结构体模式：`Name { ... }` 或 `Name(p1, p2)`
            if self.match_kind(&TokenKind::LBrace) {
                return self.parse_struct_pattern(vec![ident], span);
            }
            if self.match_kind(&TokenKind::LParen) {
                let args = self.parse_pattern_call_args()?;
                return Ok(Pattern::Variant {
                    path: vec![ident],
                    args,
                    span,
                });
            }

            // `name @ pattern`（绑定 + 子模式）
            if self.match_kind(&TokenKind::At) {
                let sub = self.parse_pattern_single()?;
                return Ok(Pattern::Binding(
                    ident,
                    crate::parser::ast::Mutability::Immutable,
                    Some(Box::new(sub)),
                    span,
                ));
            }

            // 类型注解 `: T` 由调用方（parse_param / parse_let_stmt）处理，
            // 这里不消费 `:`，避免与参数/绑定的类型注解冲突。

            // `mut name`（可变绑定）
            if ident == "mut" {
                let real = self.consume_ident("expected name after 'mut'")?;
                return Ok(Pattern::Binding(
                    real,
                    crate::parser::ast::Mutability::Mutable,
                    None,
                    span,
                ));
            }
            let mutability = crate::parser::ast::Mutability::Immutable;
            return Ok(Pattern::Binding(ident, mutability, None, span));
        }

        Err(CompileError::new(format!(
            "expected pattern, found {:?}",
            self.peek().kind
        )))
    }

    /// 解析结构体模式 `Name { f1, f2: pat, .. }`。
    fn parse_struct_pattern(
        &mut self,
        path: Vec<String>,
        span: Span,
    ) -> Result<Pattern, CompileError> {
        let mut fields = Vec::new();
        let mut rest = false;
        if !self.check(&TokenKind::RBrace) {
            if self.match_kind(&TokenKind::DotDot) {
                rest = true;
            } else {
                let name = self.consume_ident("expected field name")?;
                let pat = if self.match_kind(&TokenKind::Colon) {
                    self.parse_pattern()?
                } else {
                    Pattern::Binding(
                        name.clone(),
                        crate::parser::ast::Mutability::Immutable,
                        None,
                        Span::unknown(),
                    )
                };
                fields.push(FieldPat { name, pattern: pat });
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RBrace) || self.check(&TokenKind::DotDot) {
                        break;
                    }
                    let n = self.consume_ident("expected field name")?;
                    let p = if self.match_kind(&TokenKind::Colon) {
                        self.parse_pattern()?
                    } else {
                        Pattern::Binding(
                            n.clone(),
                            crate::parser::ast::Mutability::Immutable,
                            None,
                            Span::unknown(),
                        )
                    };
                    fields.push(FieldPat {
                        name: n,
                        pattern: p,
                    });
                }
                if self.match_kind(&TokenKind::DotDot) {
                    rest = true;
                }
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(Pattern::Struct {
            path,
            fields,
            rest,
            span,
        })
    }

    /// 解析模式中的字面量表达式（仅前缀，不消费 `..` / `..=` 等二元运算符）。
    fn parse_pattern_literal_expr(&mut self) -> Result<Expression, CompileError> {
        let span = self.peek().span;
        let neg = self.match_kind(&TokenKind::Minus);
        let lit = self
            .parse_literal()?
            .ok_or_else(|| CompileError::internal("expected literal in pattern range"))?;
        let expr = Expression::Literal(lit, span);
        Ok(if neg {
            Expression::Unary(UnaryOp::Neg, Box::new(expr), span)
        } else {
            expr
        })
    }
}

// 为 TokenKind 补充的便捷方法
impl TokenKind {
    fn is_lit_or_minus(&self) -> bool {
        matches!(
            self,
            TokenKind::IntLiteral(..)
                | TokenKind::Minus
                | TokenKind::FloatLiteral(..)
                | TokenKind::CharLiteral(_)
                | TokenKind::KwTrue
                | TokenKind::KwFalse
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn pat(src: &str) -> Pattern {
        let tokens = Lexer::new(src).tokenize();
        let mut p = Parser::new(tokens);
        p.parse_pattern().expect("should parse")
    }

    #[test]
    fn wildcard() {
        assert!(matches!(pat("_"), Pattern::Wildcard(_)));
    }

    #[test]
    fn binding() {
        assert!(matches!(pat("x"), Pattern::Binding(_, _, _, _)));
    }

    #[test]
    fn tuple_pattern() {
        assert!(matches!(pat("(a, b, c)"), Pattern::Tuple(_, _)));
    }

    #[test]
    fn struct_pattern() {
        assert!(matches!(pat("Point { x, y: 0 }"), Pattern::Struct { .. }));
    }

    #[test]
    fn variant_pattern() {
        assert!(matches!(pat("Some(value)"), Pattern::Variant { .. }));
    }

    #[test]
    fn range_pattern() {
        assert!(matches!(pat("1..=5"), Pattern::Range(_, _, true, _)));
    }

    #[test]
    fn literal_pattern() {
        assert!(matches!(pat("42"), Pattern::Literal(Literal::Int(42, _))));
        assert!(matches!(
            pat("\"hello\""),
            Pattern::Literal(Literal::String(_))
        ));
    }

    #[test]
    fn ref_pattern() {
        assert!(matches!(
            pat("ref mut x"),
            Pattern::Ref(_, crate::parser::ast::Mutability::Mutable, _)
        ));
    }

    #[test]
    fn at_pattern() {
        assert!(matches!(
            pat("x @ 1..=5"),
            Pattern::Binding(_, _, Some(_), _)
        ));
    }

    #[test]
    fn mut_binding() {
        assert!(matches!(
            pat("mut count"),
            Pattern::Binding(_, crate::parser::ast::Mutability::Mutable, _, _)
        ));
    }
}
