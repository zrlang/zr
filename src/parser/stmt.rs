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
use crate::lexer::TokenKind;
use crate::parser::ast::*;
use crate::parser::Parser;

impl Parser {
    /// 解析一个模式（入口，允许或模式）。
    pub fn parse_pattern(&mut self) -> Result<Pattern, CompileError> {
        self.parse_pattern_or()
    }

    /// 解析或模式 `p1 | p2 | ...`。
    fn parse_pattern_or(&mut self) -> Result<Pattern, CompileError> {
        let mut pat = self.parse_pattern_single()?;
        // 目前或模式暂不展开为独立节点，第一个分支作为主模式
        while self.match_kind(&TokenKind::Pipe) {
            self.parse_pattern_single()?;
        }
        Ok(pat)
    }

    /// 解析单个模式。
    fn parse_pattern_single(&mut self) -> Result<Pattern, CompileError> {
        let span = self.peek().span;

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

        // 字面量模式
        if let TokenKind::IntLiteral(v) = &self.peek().kind {
            let v = *v;
            self.advance();
            return Ok(Pattern::Literal(Literal::Int(v, None)));
        }
        if let TokenKind::StringLiteral(s) = &self.peek().kind {
            let s = s.clone();
            self.advance();
            return Ok(Pattern::Literal(Literal::String(s)));
        }
        if let TokenKind::CharLiteral(c) = &self.peek().kind {
            let c = *c;
            self.advance();
            return Ok(Pattern::Literal(Literal::Char(c)));
        }
        if matches!(self.peek().kind, TokenKind::KwTrue | TokenKind::KwFalse) {
            let b = self.match_kind(&TokenKind::KwTrue);
            return Ok(Pattern::Literal(Literal::Bool(b)));
        }

        // 范围模式 `1..=5`（仅作为独立模式，不出现在其他模式内）
        if self.peek().kind.is_lit_or_minus() {
            let start = self.parse_expression()?;
            if self.match_kind(&TokenKind::DotDot) || self.match_kind(&TokenKind::DotDotEq) {
                let inclusive = self.previous().kind == TokenKind::DotDotEq;
                let end = self.parse_expression()?;
                return Ok(Pattern::Range(Box::new(start), Box::new(end), inclusive, span));
            }
            // 不是范围模式，回退——但此时已消费了字面量，构造一个 Literal 模式
            return if let Expression::Literal(lit, _) = start {
                Ok(Pattern::Literal(lit))
            } else {
                Err(CompileError::internal("expected pattern"))
            };
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

        // 绑定：`name` / `name @ subpat` / `mut name`
        if let TokenKind::Ident(name) = &self.peek().kind {
            let ident = name.clone();
            self.advance();

            // 枚举变体 / 结构体模式：`Name { ... }` 或 `Name(p1, p2)`
            if self.match_kind(&TokenKind::LBrace) {
                return self.parse_struct_pattern(vec![ident], span);
            }
            if self.match_kind(&TokenKind::LParen) {
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
                return Ok(Pattern::Variant {
                    path: vec![ident],
                    args,
                    span,
                });
            }

            // `name @ pattern`（绑定 + 子模式）
            if self.match_kind(&TokenKind::At) {
                let sub = self.parse_pattern_single()?;
                return Ok(Pattern::Binding(ident, crate::parser::ast::Mutability::Immutable, Some(Box::new(sub)), span));
            }

            // 普通绑定，可选类型注解 `: T`
            let ty = if self.match_kind(&TokenKind::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            let mutability = crate::parser::ast::Mutability::Immutable;
            // 类型注解绑定：`mut name: T`
            if ident == "mut" {
                let real = self.consume_ident("expected name after 'mut'")?;
                let ty = if self.match_kind(&TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                return Ok(Pattern::Binding(real, crate::parser::ast::Mutability::Mutable, None, span));
            }
            let _ = ty;
            return Ok(Pattern::Binding(ident, mutability, None, span));
        }

        Err(CompileError::new(format!(
            "expected pattern, found {:?}",
            self.peek().kind
        )))
    }

    /// 解析结构体模式 `Name { f1, f2: pat, .. }`。
    fn parse_struct_pattern(&mut self, path: Vec<String>, span: Span) -> Result<Pattern, CompileError> {
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
                    Pattern::Binding(name.clone(), crate::parser::ast::Mutability::Immutable, None, Span::unknown())
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
                        Pattern::Binding(n.clone(), crate::parser::ast::Mutability::Immutable, None, Span::unknown())
                    };
                    fields.push(FieldPat { name: n, pattern: p });
                }
                if self.match_kind(&TokenKind::DotDot) {
                    rest = true;
                }
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(Pattern::Struct { path, fields, rest, span })
    }
}

// 为 TokenKind 补充的便捷方法
impl TokenKind {
    fn is_lit_or_minus(&self) -> bool {
        matches!(
            self,
            TokenKind::IntLiteral(_)
                | TokenKind::Minus
                | TokenKind::FloatLiteral(_)
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
        assert!(matches!(
            pat("Point { x, y: 0 }"),
            Pattern::Struct { .. }
        ));
    }

    #[test]
    fn variant_pattern() {
        assert!(matches!(
            pat("Some(value)"),
            Pattern::Variant { .. }
        ));
    }

    #[test]
    fn range_pattern() {
        assert!(matches!(pat("1..=5"), Pattern::Range(_, _, true, _)));
    }

    #[test]
    fn literal_pattern() {
        assert!(matches!(pat("42"), Pattern::Literal(Literal::Int(42, _))));
        assert!(matches!(pat("\"hello\""), Pattern::Literal(Literal::String(_))));
    }

    #[test]
    fn ref_pattern() {
        assert!(matches!(pat("ref mut x"), Pattern::Ref(_, crate::parser::ast::Mutability::Mutable, _)));
    }

    #[test]
    fn at_pattern() {
        assert!(matches!(pat("x @ 1..=5"), Pattern::Binding(_, _, Some(_), _)));
    }

    #[test]
    fn mut_binding() {
        assert!(matches!(pat("mut count"), Pattern::Binding(_, crate::parser::ast::Mutability::Mutable, _, _)));
    }
}
