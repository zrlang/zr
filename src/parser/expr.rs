//! 表达式解析（Pratt 优先级升降法）。
//!
//! 优先级表（从低到高）：
//!
//! | 层级 | 运算符 | 结合性 |
//! |------|--------|--------|
//! | 0 | `=`, `+=`, ...（赋值） | 右结合 |
//! | 1 | `||` | 左结合 |
//! | 2 | `&&` | 左结合 |
//! | 3 | `|` | 左结合 |
//! | 4 | `^` | 左结合 |
//! | 5 | `&` | 左结合 |
//! | 6 | `==`, `!=` | 无 |
//! | 7 | `<`, `>`, `<=`, `>=`, `in`, `is` | 无 |
//! | 8 | `<<`, `>>` | 左结合 |
//! | 9 | `+`, `-`, `++`（字符串拼接） | 左结合 |
//! | 10 | `*`, `/`, `%` | 左结合 |
//! | 11 | `as`, `is` | 左结合 |
//! | 12 | 一元 `-`, `!`, `~`, `*`(解引用), `&`, `&mut` | 无 |
//! | 13 | 后缀：`()`（调用）, `[]`（索引）, `.`（字段）, `?` | 左结合 |

use crate::error::CompileError;
use crate::lexer::{Span, TokenKind};
use crate::parser::ast::*;
use crate::parser::Parser;

/// 运算符优先级（binding power）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Prec(u8);

impl Prec {
    const ASSIGN: Self = Self(1);
    const OR: Self = Self(2);
    const AND: Self = Self(3);
    const BIT_OR: Self = Self(4);
    const XOR: Self = Self(5);
    const BIT_AND: Self = Self(6);
    const EQUALITY: Self = Self(7);
    const COMPARE: Self = Self(8);
    const SHIFT: Self = Self(9);
    const TERM: Self = Self(10);
    const FACTOR: Self = Self(11);
    const CAST: Self = Self(12);
    const UNARY: Self = Self(13);
    const CALL: Self = Self(15);
}

impl Parser {
    /// 解析任意表达式（入口）。
    pub fn parse_expression(&mut self) -> Result<Expression, CompileError> {
        self.parse_expr_bp(Prec(0))
    }

    /// 解析表达式，直到遇到优先级 ≤ `min_prec` 的运算符为止。
    fn parse_expr_bp(&mut self, min_prec: Prec) -> Result<Expression, CompileError> {
        let mut left = self.parse_prefix()?;

        loop {
            let (op_kind, op_span) = {
                let tk = &self.peek().kind;
                match self.infix_binding_power(tk) {
                    Some((left_bp, _)) if left_bp >= min_prec => {}
                    _ => break,
                }
                let span = self.peek().span;
                (tk.clone(), span)
            };

            // 赋值运算符（右结合）
            if let Some(op) = self.assign_op(&op_kind) {
                self.advance();
                let right = self.parse_expr_bp(Prec::ASSIGN)?;
                left = Expression::Assign(Box::new(left), op, Box::new(right), op_span);
                continue;
            }

            // 后缀运算符：函数调用、字段访问、索引
            match op_kind {
                TokenKind::LParen => {
                    self.advance(); // (
                    let args = self.parse_call_args()?;
                    self.consume(&TokenKind::RParen, "expected ')'")?;
                    left = Expression::Call(Box::new(left), args, op_span);
                    continue;
                }
                TokenKind::Dot => {
                    self.advance(); // .
                    let field = self.consume_ident("expected field name")?;
                    left = Expression::Field(Box::new(left), field, op_span);
                    continue;
                }
                TokenKind::LBracket => {
                    self.advance(); // [
                    let index = self.parse_expression()?;
                    self.consume(&TokenKind::RBracket, "expected ']'")?;
                    left = Expression::Index(Box::new(left), Box::new(index), op_span);
                    continue;
                }
                TokenKind::KwAs => {
                    self.advance(); // as
                    let ty = self.parse_type()?;
                    left = Expression::Cast(Box::new(left), ty, op_span);
                    continue;
                }
                _ => {}
            }

            // 二元运算符
            if let Some((_, right_bp)) = self.infix_binding_power(&op_kind) {
                // 处理 `?`（后缀）作为单独的后缀运算符
                if op_kind == TokenKind::Question {
                    self.advance();
                    left = Expression::Unary(UnaryOp::Deref, Box::new(left), op_span); // placeholder
                    continue;
                }

                // 范围运算符 `..` / `..=`
                if matches!(op_kind, TokenKind::DotDot | TokenKind::DotDotEq) {
                    self.advance();
                    let op = if op_kind == TokenKind::DotDotEq {
                        BinaryOp::RangeInclusive
                    } else {
                        BinaryOp::Range
                    };
                    let right = self.parse_expr_bp(right_bp)?;
                    left = Expression::Binary(Box::new(left), op, Box::new(right), op_span);
                    continue;
                }

                self.advance();
                let op = self.binary_op(&op_kind).ok_or_else(|| {
                    CompileError::internal(format!("missing binary op handler for {:?}", op_kind))
                })?;
                let right = self.parse_expr_bp(right_bp)?;
                left = Expression::Binary(Box::new(left), op, Box::new(right), op_span);
                continue;
            }

            break;
        }

        Ok(left)
    }

    /// 解析前缀表达式（字面量、标识符、一元运算符、控制流等）。
    fn parse_prefix(&mut self) -> Result<Expression, CompileError> {
        let span = self.peek().span;
        // 字节字面量：`b'x'` / `b"str"`。词法器不认识这个前缀，所以在
        // 看到标识符 `b` 且下一个 token 是引号时手工拼一个。
        if let TokenKind::Ident(name) = &self.peek().kind {
            if name == "b" {
                let next = self.peek_n(1).map(|t| t.kind.clone());
                match next {
                    Some(TokenKind::CharLiteral(_)) => {
                        self.advance();
                        if let TokenKind::CharLiteral(c) = self.advance().kind {
                            return Ok(Expression::Literal(Literal::Byte(c as u32 as u8), span));
                        }
                    }
                    Some(TokenKind::StringLiteral(_)) => {
                        self.advance();
                        if let TokenKind::StringLiteral(s) = self.advance().kind {
                            return Ok(Expression::Literal(
                                Literal::ByteString(s.chars().map(|c| c as u32 as u8).collect()),
                                span,
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }

        let kind = self.peek().kind.clone();

        // ===== 字面量 =====
        if let Some(lit) = self.parse_literal()? {
            return Ok(Expression::Literal(lit, span));
        }

        // ===== 块 =====
        if self.match_kind(&TokenKind::LBrace) {
            let blk = self.parse_block_contents()?;
            self.consume(&TokenKind::RBrace, "expected '}' to close block")?;
            return Ok(Expression::Block(blk, span));
        }

        // ===== if 表达式 =====
        if self.match_kind(&TokenKind::KwIf) {
            return self
                .parse_if()
                .map(|(c, t, e, s)| Expression::If(c, t, e, s));
        }

        // ===== match 表达式 =====
        if self.match_kind(&TokenKind::KwMatch) {
            return self
                .parse_match()
                .map(|(e, arms, s)| Expression::Match(e, arms, s));
        }

        // ===== while 表达式 =====
        if self.match_kind(&TokenKind::KwWhile) {
            return self
                .parse_while()
                .map(|(c, b, e, s)| Expression::While(c, b, e, s));
        }

        // ===== for 表达式 =====
        if self.match_kind(&TokenKind::KwFor) {
            return self.parse_for();
        }

        // ===== loop 表达式 =====
        if self.match_kind(&TokenKind::KwLoop) {
            let blk = self.parse_block()?;
            return Ok(Expression::Loop(blk, span));
        }

        // ===== break / continue / return =====
        if self.match_kind(&TokenKind::KwBreak) {
            let label = if self.peek().kind == TokenKind::Ident(String::new()) {
                None
            } else {
                None
            };
            let value = if self.check_expr_start() {
                Some(Box::new(self.parse_expression()?))
            } else {
                None
            };
            return Ok(Expression::Break(label, value, span));
        }
        if self.match_kind(&TokenKind::KwContinue) {
            return Ok(Expression::Continue(None, span));
        }
        if self.match_kind(&TokenKind::KwReturn) {
            let value = if !self.check(&TokenKind::Semicolon)
                && !self.check(&TokenKind::RBrace)
                && !self.check(&TokenKind::Comma)
            {
                Some(Box::new(self.parse_expression()?))
            } else {
                None
            };
            return Ok(Expression::Return(value, span));
        }

        // ===== defer =====
        if self.match_kind(&TokenKind::KwDefer) {
            let expr = self.parse_expression()?;
            return Ok(Expression::Defer(Box::new(expr), span));
        }

        // ===== asm! 内联汇编 =====
        if self.check(&TokenKind::Ident("asm".into()))
            && self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::Bang)
                .unwrap_or(false)
        {
            self.advance(); // asm
            self.advance(); // !
            return self.parse_asm(span);
        }

        // ===== try / throw =====
        if self.match_kind(&TokenKind::KwTry) {
            let expr = self.parse_expression()?;
            let catch = if self.match_kind(&TokenKind::KwCatch) {
                let pat = self.parse_pattern()?;
                let body = self.parse_block()?;
                Some((pat, body))
            } else {
                None
            };
            let (pat, body) =
                catch.unwrap_or_else(|| (Pattern::Wildcard(Span::unknown()), Block::default()));
            return Ok(Expression::Try(Box::new(expr), Some(pat), body, span));
        }
        if self.match_kind(&TokenKind::KwThrow) {
            let expr = self.parse_expression()?;
            return Ok(Expression::Throw(Box::new(expr), span));
        }

        // ===== async / await / yield / spawn =====
        if self.match_kind(&TokenKind::KwAsync) {
            let expr = self.parse_expression()?;
            return Ok(Expression::Await(Box::new(expr), span));
        }
        if self.match_kind(&TokenKind::KwAwait) {
            let expr = self.parse_expression()?;
            return Ok(Expression::Await(Box::new(expr), span));
        }
        if self.match_kind(&TokenKind::KwYield) {
            let expr = self.parse_expression()?;
            return Ok(Expression::Yield(Box::new(expr), span));
        }
        if self.match_kind(&TokenKind::KwSpawn) {
            let expr = self.parse_expression()?;
            return Ok(Expression::Spawn(Box::new(expr), span));
        }

        // ===== comptime =====
        if self.match_kind(&TokenKind::KwComptime) {
            let expr = self.parse_expression()?;
            return Ok(Expression::Comptime(Box::new(expr), span));
        }

        // ===== goto =====
        if self.match_kind(&TokenKind::KwGoto) {
            let label = self.consume_ident("expected label after 'goto'")?;
            return Ok(Expression::Goto(label, span));
        }

        // ===== 一元运算符 =====
        if kind == TokenKind::Amp {
            // `&x` / `&mut x` / `&const x`
            self.advance();
            let mutability = if self.match_kind(&TokenKind::KwMut) {
                Mutability::Mutable
            } else if self.match_kind(&TokenKind::KwConst) {
                Mutability::Immutable
            } else {
                Mutability::Immutable
            };
            let operand = self.parse_expr_bp(Prec::UNARY)?;
            return Ok(Expression::AddrOf(Box::new(operand), mutability, span));
        }
        if let Some(op) = self.unary_op(&kind) {
            self.advance();
            let operand = self.parse_expr_bp(Prec::UNARY)?;
            return Ok(Expression::Unary(op, Box::new(operand), span));
        }

        // ===== Lambda：`|a, b| body` =====
        if self.match_kind(&TokenKind::Pipe) {
            let mut params = Vec::new();
            if !self.check(&TokenKind::Pipe) {
                params.push(self.parse_pattern_single()?);
                while self.match_kind(&TokenKind::Comma) {
                    params.push(self.parse_pattern_single()?);
                }
            }
            self.consume(&TokenKind::Pipe, "expected '|' after lambda parameters")?;
            let ret = if self.match_kind(&TokenKind::Arrow) {
                Some(self.parse_type()?)
            } else {
                None
            };
            let body = if self.check(&TokenKind::LBrace) {
                self.parse_block()?
            } else {
                let expr = self.parse_expression()?;
                Block {
                    stmts: Vec::new(),
                    tail: Some(Box::new(expr)),
                    span,
                }
            };
            return Ok(Expression::Lambda(params, ret, body, span));
        }

        // ===== 有标签的块 =====
        if let TokenKind::Ident(name) = &kind {
            if self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::Colon)
                .unwrap_or(false)
            {
                let label = name.clone();
                self.advance(); // ident
                self.advance(); // :
                let expr = self.parse_expression()?;
                return Ok(Expression::Labeled(label, Box::new(expr), span));
            }
        }

        // ===== 数组 / 元组 / 结构体字面量（以 `(` / `[` 开头）=====
        if self.match_kind(&TokenKind::LParen) {
            return self.parse_paren_or_tuple(span);
        }
        if self.match_kind(&TokenKind::LBracket) {
            return self.parse_array_literal(span);
        }

        // ===== 变量 / 路径 =====
        if let TokenKind::Ident(name) = &kind {
            let n = name.clone();
            self.advance();
            // ===== `intrinsic!` 宏调用 =====
            // 形如：`intrinsic!(size_of T)`、`intrinsic!(yield)`、
            //       `intrinsic!(drop value)`、`intrinsic!(transmute src -> Dst)`
            if n == "intrinsic" && self.match_kind(&TokenKind::Bang) {
                self.consume(&TokenKind::LParen, "expected '(' after `intrinsic!`")?;
                let kind = self.parse_intrinsic_kind()?;
                let mut types = Vec::new();
                let mut operands = Vec::new();

                // 有的内建第一个实参是**表达式**（`transmute src -> Dst`、
                // `drop x`），有的是**类型**（`size_of T`）。先把实参统一
                // 解析出来：能当类型就当类型，否则按表达式处理。
                while !self.check(&TokenKind::RParen) && !self.check(&TokenKind::Eof) {
                    if let Some(ty) = self.try_parse_type() {
                        types.push(ty);
                    } else {
                        let e = self.parse_expression()?;
                        operands.push(e);
                        // `transmute src -> Dst` 的目标类型
                        if self.match_kind(&TokenKind::Arrow) {
                            types.push(self.parse_type()?);
                            break;
                        }
                    }
                    if !self.match_kind(&TokenKind::Comma) {
                        break;
                    }
                }
                self.consume(&TokenKind::RParen, "expected ')' to close `intrinsic!`")?;
                return Ok(Expression::Intrinsic(kind, types, operands, span));
            }
            if self.match_kind(&TokenKind::ColonColon) {
                // 泛型实参（turbofish）：`size_of::<T>()` 的 `::<T>`
                if self.check(&TokenKind::Lt) {
                    self.advance();
                    let _args = self.parse_generic_args()?;
                    // turbofish 后面必定跟着调用
                    if self.match_kind(&TokenKind::LParen) {
                        let args = self.parse_call_args()?;
                        self.consume(&TokenKind::RParen, "expected ')'")?;
                        return Ok(Expression::Call(
                            Box::new(Expression::Variable(n, span)),
                            args,
                            span,
                        ));
                    }
                    return Ok(Expression::Variable(n, span));
                }
                // 路径：`a::b::c`
                let mut path = vec![n];
                path.push(self.consume_path_segment("expected path segment")?);
                while self.match_kind(&TokenKind::ColonColon) {
                    if self.check(&TokenKind::Lt) {
                        break;
                    }
                    path.push(self.consume_path_segment("expected path segment")?);
                }
                return Ok(Expression::Path(path, span));
            }
            // 结构体字面量：`Point { x: 1, y: 2 }` 或简写 `Point { x, y }`。
            // `{` 后紧跟「标识符」且其后是 `:` 或 `,`/`}` 时才认定，
            // 避免和块语法冲突；`if` / `while` 条件位置由
            // `no_struct_literal` 额外禁用。
            let looks_like_struct = !self.no_struct_literal
                && self.check(&TokenKind::LBrace)
                && self
                    .peek_n(1)
                    .map(|t| matches!(t.kind, TokenKind::Ident(_)))
                    .unwrap_or(false)
                && self
                    .peek_n(2)
                    .map(|t| {
                        matches!(
                            t.kind,
                            TokenKind::Colon | TokenKind::Comma | TokenKind::RBrace
                        )
                    })
                    .unwrap_or(false);
            if looks_like_struct {
                self.advance(); // {
                return self.parse_struct_literal(vec![n], span);
            }
            return Ok(Expression::Variable(n, span));
        }

        // ===== 插值字符串 =====
        if self.match_kind(&TokenKind::KwSelf) {
            return Ok(Expression::Variable("self".into(), span));
        }

        // 走到这里说明这个关键字不参与表达式语法，可以当普通标识符用
        // （例如标准库里的 `return default`）。
        if let Some(kw) = kind.keyword_text() {
            self.advance();
            return Ok(Expression::Variable(kw.to_string(), span));
        }

        Err(CompileError::internal(format!(
            "unexpected expression start: {:?} at line {}",
            kind,
            self.peek().span.line
        )))
    }

    // ===== 字面量 =====

    pub(crate) fn parse_literal(&mut self) -> Result<Option<Literal>, CompileError> {
        let tk = self.peek().kind.clone();
        Ok(Some(match tk {
            TokenKind::IntLiteral(v) => {
                self.advance();
                Literal::Int(v, None)
            }
            TokenKind::FloatLiteral(v) => {
                self.advance();
                Literal::Float(v, None)
            }
            TokenKind::StringLiteral(s) => {
                self.advance();
                Literal::String(s)
            }
            TokenKind::CharLiteral(c) => {
                self.advance();
                Literal::Char(c)
            }
            TokenKind::KwTrue => {
                self.advance();
                Literal::Bool(true)
            }
            TokenKind::KwFalse => {
                self.advance();
                Literal::Bool(false)
            }
            _ => return Ok(None),
        }))
    }

    // ===== 控制流 =====

    fn parse_if(
        &mut self,
    ) -> Result<(Box<Expression>, Block, Option<Box<Expression>>, Span), CompileError> {
        let span = self.previous().span;
        let cond = Box::new(self.parse_condition()?);
        let then = self.parse_block()?;
        let else_branch = if self.match_kind(&TokenKind::KwElse) {
            if self.check(&TokenKind::KwIf) {
                // `else if` 作为嵌套 if 表达式。`parse_if` 假定 `if` 已经被
                // 匹配掉了，所以这里必须先把 `if` 吃掉，否则递归进去时
                // 当前 token 还是 `if`，条件会被解析成一个嵌套 if。
                self.advance();
                let (c2, t2, e2, _) = self.parse_if()?;
                Some(Box::new(Expression::If(c2, t2, e2, Span::unknown())))
            } else {
                let b = self.parse_block()?;
                Some(Box::new(Expression::Block(b, Span::unknown())))
            }
        } else {
            None
        };
        Ok((cond, then, else_branch, span))
    }

    fn parse_match(&mut self) -> Result<(Box<Expression>, Vec<Arm>, Span), CompileError> {
        let span = self.previous().span;
        let expr = Box::new(self.parse_expression()?);
        self.consume(&TokenKind::LBrace, "expected '{' after 'match'")?;
        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let pat = self.parse_pattern()?;
            let guard = if self.match_kind(&TokenKind::KwIf) {
                Some(Box::new(self.parse_expression()?))
            } else {
                None
            };
            self.consume(&TokenKind::Rocket, "expected '=>' after match pattern")?;
            let body = if self.match_kind(&TokenKind::LBrace) {
                let blk = self.parse_block_contents()?;
                self.consume(&TokenKind::RBrace, "expected '}'")?;
                Box::new(Expression::Block(blk, Span::unknown()))
            } else {
                let e = self.parse_expression()?;
                if self.match_kind(&TokenKind::Comma) {
                    // 多语句 arm（实验性）
                }
                Box::new(e)
            };
            arms.push(Arm {
                pattern: pat,
                guard,
                body,
            });
            // 允许省略最后一个逗号
            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
            } else if !self.check(&TokenKind::RBrace) {
                // 允许换行分隔
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}' to close match")?;
        Ok((expr, arms, span))
    }

    fn parse_while(
        &mut self,
    ) -> Result<(Box<Expression>, Block, Option<Block>, Span), CompileError> {
        let span = self.previous().span;
        let cond = Box::new(self.parse_condition()?);
        let body = self.parse_block()?;
        let else_body = if self.match_kind(&TokenKind::KwElse) {
            Some(self.parse_block()?)
        } else {
            None
        };
        Ok((cond, body, else_body, span))
    }

    fn parse_for(&mut self) -> Result<Expression, CompileError> {
        let span = self.previous().span;
        let pat = self.parse_pattern()?;
        self.consume(&TokenKind::KwIn, "expected 'in' after for pattern")?;
        let iter = Box::new(self.parse_expression()?);
        let body = self.parse_block()?;
        Ok(Expression::For(pat, iter, body, span))
    }

    // ===== 复合字面量 =====

    fn parse_paren_or_tuple(&mut self, span: Span) -> Result<Expression, CompileError> {
        let mut exprs = Vec::new();
        if !self.check(&TokenKind::RParen) {
            exprs.push(self.parse_expression()?);
            while self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                exprs.push(self.parse_expression()?);
            }
        }
        self.consume(&TokenKind::RParen, "expected ')'")?;

        // 单一表达式 + 无逗号 → 括号分组
        if exprs.len() == 1 && !self.previous_was_comma() {
            return Ok(exprs.into_iter().next().unwrap());
        }
        Ok(Expression::Tuple(exprs, span))
    }

    fn parse_array_literal(&mut self, span: Span) -> Result<Expression, CompileError> {
        let mut elems = Vec::new();
        let mut repeat = None;
        if !self.check(&TokenKind::RBracket) {
            let first = self.parse_expression()?;
            if self.match_kind(&TokenKind::Semicolon) {
                // `[init; len]` 重复初始化
                repeat = Some(Box::new(self.parse_expression()?));
                self.consume(&TokenKind::RBracket, "expected ']'")?;
                return Ok(Expression::Array(vec![first], repeat, span));
            }
            elems.push(first);
            while self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBracket) {
                    break;
                }
                elems.push(self.parse_expression()?);
            }
        }
        self.consume(&TokenKind::RBracket, "expected ']'")?;
        Ok(Expression::Array(elems, repeat, span))
    }

    /// 解析 `if` / `while` 的条件表达式：禁用结构体字面量。
    fn parse_condition(&mut self) -> Result<Expression, CompileError> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = true;
        let cond = self.parse_expression();
        self.no_struct_literal = saved;
        cond
    }
    fn parse_struct_literal(
        &mut self,
        path: Vec<String>,
        span: Span,
    ) -> Result<Expression, CompileError> {
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let name = self.consume_ident("expected field name")?;
            // 字段简写：`Point { x, y }` 等价于 `Point { x: x, y: y }`
            let value = if self.match_kind(&TokenKind::Colon) {
                self.parse_expression()?
            } else {
                Expression::Variable(name.clone(), span)
            };
            fields.push((name, value));
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(Expression::StructInit(path, fields, span))
    }

    /// 解析函数调用参数列表（不含括号）。
    fn parse_call_args(&mut self) -> Result<Vec<Argument>, CompileError> {
        let mut args = Vec::new();
        if self.check(&TokenKind::RParen) {
            return Ok(args);
        }
        loop {
            // 命名参数：`name: value`
            if let TokenKind::Ident(name) = &self.peek().kind {
                if self
                    .peek_n(1)
                    .map(|t| t.kind == TokenKind::Colon)
                    .unwrap_or(false)
                {
                    let name = name.clone();
                    self.advance(); // ident
                    self.advance(); // :
                    let value = self.parse_expression()?;
                    args.push(Argument::Named(name, value));
                } else {
                    let value = self.parse_expression()?;
                    args.push(Argument::Pos(value));
                }
            } else {
                let value = self.parse_expression()?;
                args.push(Argument::Pos(value));
            }
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
            // 允许省略最后一个实参后的逗号：`f(a, b,)`
            if self.check(&TokenKind::RParen) {
                break;
            }
        }
        Ok(args)
    }

    // ===== 运算符优先级表 =====

    fn infix_binding_power(&self, kind: &TokenKind) -> Option<(Prec, Prec)> {
        Some(match kind {
            TokenKind::Eq
            | TokenKind::PlusEq
            | TokenKind::MinusEq
            | TokenKind::StarEq
            | TokenKind::SlashEq
            | TokenKind::PercentEq
            | TokenKind::AmpEq
            | TokenKind::PipeEq
            | TokenKind::CaretEq
            | TokenKind::ShlEq
            | TokenKind::ShrEq => (Prec::ASSIGN, Prec(0)),

            TokenKind::PipePipe => (Prec::OR, Prec(Prec::OR.0 + 1)),
            TokenKind::AmpAmp => (Prec::AND, Prec(Prec::AND.0 + 1)),

            // 范围运算符 `..` / `..=`（优先级低于逻辑或）
            TokenKind::DotDot | TokenKind::DotDotEq => (Prec::OR, Prec(Prec::OR.0 + 1)),

            TokenKind::Pipe => (Prec::BIT_OR, Prec(Prec::BIT_OR.0 + 1)),
            TokenKind::Caret => (Prec::XOR, Prec(Prec::XOR.0 + 1)),
            TokenKind::Amp => (Prec::BIT_AND, Prec(Prec::BIT_AND.0 + 1)),

            TokenKind::EqEq | TokenKind::BangEq => (Prec::EQUALITY, Prec::EQUALITY),
            TokenKind::Lt | TokenKind::Gt | TokenKind::Le | TokenKind::Ge => {
                (Prec::COMPARE, Prec::COMPARE)
            }

            TokenKind::KwIn | TokenKind::KwIs => (Prec::COMPARE, Prec::COMPARE),

            TokenKind::Shl | TokenKind::Shr => (Prec::SHIFT, Prec(Prec::SHIFT.0 + 1)),

            TokenKind::Plus | TokenKind::Minus | TokenKind::PlusPlus => {
                (Prec::TERM, Prec(Prec::TERM.0 + 1))
            }

            TokenKind::Star | TokenKind::Slash | TokenKind::Percent => {
                (Prec::FACTOR, Prec(Prec::FACTOR.0 + 1))
            }

            TokenKind::KwAs => (Prec::CAST, Prec::UNARY),

            TokenKind::Dot | TokenKind::LParen | TokenKind::LBracket | TokenKind::Question => {
                (Prec::CALL, Prec::CALL)
            }

            _ => return None,
        })
    }

    fn binary_op(&self, kind: &TokenKind) -> Option<BinaryOp> {
        Some(match kind {
            TokenKind::Plus | TokenKind::PlusPlus => BinaryOp::Add,
            TokenKind::Minus => BinaryOp::Sub,
            TokenKind::Star => BinaryOp::Mul,
            TokenKind::Slash => BinaryOp::Div,
            TokenKind::Percent => BinaryOp::Rem,
            TokenKind::Amp => BinaryOp::BitAnd,
            TokenKind::Pipe => BinaryOp::BitOr,
            TokenKind::Caret => BinaryOp::BitXor,
            TokenKind::AmpAmp => BinaryOp::And,
            TokenKind::PipePipe => BinaryOp::Or,
            TokenKind::Shl => BinaryOp::Shl,
            TokenKind::Shr => BinaryOp::Shr,
            TokenKind::EqEq => BinaryOp::Eq,
            TokenKind::BangEq => BinaryOp::Ne,
            TokenKind::Lt => BinaryOp::Lt,
            TokenKind::Gt => BinaryOp::Gt,
            TokenKind::Le => BinaryOp::Le,
            TokenKind::Ge => BinaryOp::Ge,
            TokenKind::KwIn => BinaryOp::Range,
            _ => return None,
        })
    }

    fn assign_op(&self, kind: &TokenKind) -> Option<AssignOp> {
        Some(match kind {
            TokenKind::Eq => AssignOp::Assign,
            TokenKind::PlusEq => AssignOp::AddAssign,
            TokenKind::MinusEq => AssignOp::SubAssign,
            TokenKind::StarEq => AssignOp::MulAssign,
            TokenKind::SlashEq => AssignOp::DivAssign,
            TokenKind::PercentEq => AssignOp::RemAssign,
            TokenKind::AmpEq => AssignOp::AndAssign,
            TokenKind::PipeEq => AssignOp::OrAssign,
            TokenKind::CaretEq => AssignOp::XorAssign,
            TokenKind::ShlEq => AssignOp::ShlAssign,
            TokenKind::ShrEq => AssignOp::ShrAssign,
            _ => return None,
        })
    }

    fn unary_op(&self, kind: &TokenKind) -> Option<UnaryOp> {
        Some(match kind {
            TokenKind::Minus => UnaryOp::Neg,
            TokenKind::Bang => UnaryOp::Not,
            TokenKind::Tilde => UnaryOp::BitNot,
            TokenKind::Star => UnaryOp::Deref,
            TokenKind::Amp => UnaryOp::AddrOf,
            _ => return None,
        })
    }

    fn check_expr_start(&self) -> bool {
        matches!(
            self.peek().kind,
            TokenKind::IntLiteral(_)
                | TokenKind::FloatLiteral(_)
                | TokenKind::StringLiteral(_)
                | TokenKind::CharLiteral(_)
                | TokenKind::KwTrue
                | TokenKind::KwFalse
                | TokenKind::KwIf
                | TokenKind::KwMatch
                | TokenKind::KwWhile
                | TokenKind::KwFor
                | TokenKind::KwLoop
                | TokenKind::KwReturn
                | TokenKind::KwBreak
                | TokenKind::KwContinue
                | TokenKind::KwTry
                | TokenKind::KwThrow
                | TokenKind::KwAsync
                | TokenKind::KwAwait
                | TokenKind::KwYield
                | TokenKind::KwSpawn
                | TokenKind::KwComptime
                | TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::LBrace
                | TokenKind::Minus
                | TokenKind::Bang
                | TokenKind::Tilde
                | TokenKind::Star
                | TokenKind::Amp
                | TokenKind::Pipe
                | TokenKind::KwLet
                | TokenKind::KwVar
                | TokenKind::KwConst
                | TokenKind::Ident(_)
        )
    }

    fn previous_was_comma(&self) -> bool {
        // 简化：通过查看前一个 token 判断
        if self.pos < 2 {
            return false;
        }
        self.tokens[self.pos - 2].kind == TokenKind::Comma
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn expr(src: &str) -> Expression {
        let tokens = Lexer::new(src).tokenize();
        let mut p = Parser::new(tokens);
        p.parse_expression().expect("should parse")
    }

    #[test]
    fn precedence_add_mul() {
        // 1 + 2 * 3 → Add(1, Mul(2, 3))
        let e = expr("1 + 2 * 3");
        match e {
            Expression::Binary(l, BinaryOp::Add, r, _) => {
                assert!(matches!(*l, Expression::Literal(Literal::Int(1, None), _)));
                assert!(matches!(*r, Expression::Binary(_, BinaryOp::Mul, _, _)));
            }
            _ => panic!("wrong shape: {:?}", e),
        }
    }

    #[test]
    fn precedence_unary() {
        let e = expr("-a + b");
        assert!(matches!(e, Expression::Binary(_, BinaryOp::Add, _, _)));
    }

    #[test]
    fn parentheses_override() {
        let e = expr("(1 + 2) * 3");
        match e {
            Expression::Binary(l, BinaryOp::Mul, _, _) => {
                assert!(matches!(*l, Expression::Binary(_, BinaryOp::Add, _, _)));
            }
            _ => panic!("wrong shape: {:?}", e),
        }
    }

    #[test]
    fn assignment_right_assoc() {
        let e = expr("a = b = 1");
        match e {
            Expression::Assign(_, AssignOp::Assign, rhs, _) => {
                assert!(matches!(*rhs, Expression::Assign(_, _, _, _)));
            }
            _ => panic!("wrong shape: {:?}", e),
        }
    }

    #[test]
    fn comparison_chain_no_assoc() {
        let e = expr("a < b && b < c");
        assert!(matches!(e, Expression::Binary(_, BinaryOp::And, _, _)));
    }

    #[test]
    fn field_access_chaining() {
        let e = expr("a.b.c");
        assert!(matches!(e, Expression::Field(_, _, _)));
    }

    #[test]
    fn call_expression() {
        let e = expr("f(a, b, 3)");
        assert!(matches!(e, Expression::Call(_, _, _)));
    }

    #[test]
    fn index_expression() {
        let e = expr("arr[i + 1]");
        assert!(matches!(e, Expression::Index(_, _, _)));
    }

    #[test]
    fn cast_expression() {
        let e = expr("x as i64");
        assert!(matches!(e, Expression::Cast(_, Type::I(64), _)));
    }

    #[test]
    fn lambda_expression() {
        let e = expr("|a, b| a + b");
        assert!(matches!(e, Expression::Lambda(_, _, _, _)));
    }

    #[test]
    fn array_and_tuple() {
        expr("[1, 2, 3]");
        expr("[0; 10]");
        expr("(1, 2, 3)");
        expr("(1,)");
    }

    #[test]
    fn if_expression() {
        let e = expr("if a { 1 } else { 2 }");
        assert!(matches!(e, Expression::If(_, _, _, _)));
    }

    #[test]
    fn match_expression() {
        let e = expr("match x { 1 => 2, _ => 0 }");
        assert!(matches!(e, Expression::Match(_, _, _)));
    }
}

// ==================================================================
// `intrinsic!` 宏解析辅助
// ==================================================================

impl Parser {
    /// 解析 `intrinsic!` 宏的第一个"操作"标识符，映射到 IntrinsicKind。
    ///
    /// 支持的写法（括号前的标识符）：
    /// - `size_of` → `IntrinsicKind::SizeOf`
    /// - `align_of` → `IntrinsicKind::AlignOf`
    /// - `type_of` → `IntrinsicKind::TypeOf`
    /// - `offset_of` → `IntrinsicKind::OffsetOf`
    /// - `bitcast` / `transmute` → `IntrinsicKind::Transmute`
    /// - `intcast` / `int_cast` → `IntrinsicKind::IntCast`
    /// - `drop` → `IntrinsicKind::Drop`
    /// - `forget` → `IntrinsicKind::Forget`
    /// - `zeroed` → `IntrinsicKind::Zeroed`
    /// - `yield` → `IntrinsicKind::Yield`
    /// - `now_ms` → `IntrinsicKind::NowMs`
    /// - `capture_backtrace` → `IntrinsicKind::CaptureBacktrace`
    /// - `sum` → `IntrinsicKind::Sum`
    fn parse_intrinsic_kind(&mut self) -> Result<IntrinsicKind, crate::error::CompileError> {
        let name = self.consume_ident("expected intrinsic operation after `intrinsic!(`")?;
        Ok(match name.as_str() {
            "size_of" | "sizeof" => IntrinsicKind::SizeOf,
            "align_of" | "alignof" => IntrinsicKind::AlignOf,
            "type_of" | "typeof" => IntrinsicKind::TypeOf,
            "offset_of" | "offsetof" => IntrinsicKind::OffsetOf,
            "bitcast" | "transmute" => IntrinsicKind::Transmute,
            "intcast" | "int_cast" => IntrinsicKind::IntCast,
            "drop" => IntrinsicKind::Drop,
            "forget" => IntrinsicKind::Forget,
            "zeroed" => IntrinsicKind::Zeroed,
            "yield" => IntrinsicKind::Yield,
            "now_ms" | "nowms" => IntrinsicKind::NowMs,
            "capture_backtrace" | "capture" => IntrinsicKind::CaptureBacktrace,
            "sum" => IntrinsicKind::Sum,
            other => {
                return Err(crate::error::CompileError::type_error(format!(
                    "unknown intrinsic operation: `{}`",
                    other
                )));
            }
        })
    }

    /// 尝试解析一个类型。若当前 token 不是有效的类型开头，则返回 None 且不消耗 token。
    /// 与 `parse_type` 的区别：失败时不报错、不前进。
    fn try_parse_type(&mut self) -> Option<Type> {
        // 通过 peek 判断是否是类型开头：标识符、基础类型关键字、'*'、'&'、'('、'[' 等。
        use crate::lexer::TokenKind::*;
        match self.peek().kind {
            Ident(_) | TyI(_) | TyU(_) | TyF(_) | TyBool | TyStr | TyChar | TyNever | KwVoid
            | KwAnytype | KwUpperSelf | KwFn | Star | Amp | LParen | LBracket => {
                self.parse_type().ok()
            }
            _ => None,
        }
    }

    /// 解析 `asm!(...)` 内联汇编。
    fn parse_asm(&mut self, span: Span) -> Result<Expression, CompileError> {
        self.consume(&TokenKind::LParen, "expected '(' after asm!")?;

        // 第一个参数：模板字符串
        let _template = match self.peek().kind.clone() {
            TokenKind::StringLiteral(s) => {
                self.advance();
                s
            }
            _ => return Err(self.unexpected(&["string literal"])),
        };

        let mut outputs = Vec::new();
        let mut inputs = Vec::new();
        let mut _options = Vec::new();

        // 解析操作数和选项
        while self.match_kind(&TokenKind::Comma) {
            if self.check(&TokenKind::RBrace) || self.check(&TokenKind::RParen) {
                break;
            }
            // 检查是否是 options(...)
            if self.check(&TokenKind::Ident("options".into())) {
                self.advance();
                self.consume(&TokenKind::LParen, "expected '(' after options")?;
                while !self.check(&TokenKind::RParen) && !self.is_at_end() {
                    if let TokenKind::Ident(opt) = self.peek().kind.clone() {
                        _options.push(opt);
                        self.advance();
                    } else {
                        self.advance();
                    }
                    self.match_kind(&TokenKind::Comma);
                }
                self.consume(&TokenKind::RParen, "expected ')' to close options")?;
                continue;
            }
            // 解析操作数：in/out(reg) expr
            let is_out = self.check(&TokenKind::Ident("out".into()));
            let is_in = self.check(&TokenKind::Ident("in".into()));
            if !is_out && !is_in {
                // 跳过未知参数
                self.advance();
                continue;
            }
            self.advance(); // in/out
            self.consume(&TokenKind::LParen, "expected '(' after in/out")?;
            let _constraint = match self.peek().kind.clone() {
                TokenKind::Ident(c) => {
                    self.advance();
                    c
                }
                TokenKind::StringLiteral(s) => {
                    self.advance();
                    s
                }
                _ => return Err(self.unexpected(&["register constraint"])),
            };
            self.consume(&TokenKind::RParen, "expected ')' after constraint")?;
            let expr = self.parse_expression()?;
            let operand = crate::parser::ast::AsmOperand { expr };
            if is_out {
                outputs.push(operand);
            } else {
                inputs.push(operand);
            }
        }

        self.consume(&TokenKind::RParen, "expected ')' to close asm!")?;

        Ok(Expression::Asm(
            crate::parser::ast::AsmExpr { outputs, inputs },
            span,
        ))
    }
}
