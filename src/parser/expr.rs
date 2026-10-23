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

/// `parse_if` 的结果：条件为真的块、`else` 分支（可能嵌套 `else if`）与源码位置。
type IfParseResult = (Box<Expression>, Block, Option<Box<Expression>>, Span);

impl Parser {
    /// 解析任意表达式（入口）。
    pub fn parse_expression(&mut self) -> Result<Expression, CompileError> {
        self.parse_expr_bp(Prec(0))
    }

    /// 解析方括号里的内容：`i`、`a..b`、`a..=b`、`a..`、`..b`、`..`。
    ///
    /// 开区间用一个空元组 `()` 当"缺失边界"的哨兵（类型检查阶段转为
    /// `IndexRange` 的 `None`）。解析起点时用略高于 `..` 的绑定力，避免
    /// `..` 被当成普通的二元范围运算吃掉右侧直到 `]`。
    fn parse_index_operand(&mut self) -> Result<Expression, CompileError> {
        let span = self.peek().span;
        let missing = || Expression::Tuple(Vec::new(), span);
        let starts_open = self.check(&TokenKind::DotDot) || self.check(&TokenKind::DotDotEq);
        if starts_open {
            let inclusive = self.match_kind(&TokenKind::DotDotEq);
            if !inclusive {
                self.advance(); // `..`
            }
            let end = if self.check(&TokenKind::RBracket) {
                missing()
            } else {
                self.parse_expr_bp(Prec(Prec::OR.0 + 1))?
            };
            let op = if inclusive {
                BinaryOp::RangeInclusive
            } else {
                BinaryOp::Range
            };
            return Ok(Expression::Binary(
                Box::new(missing()),
                op,
                Box::new(end),
                span,
            ));
        }

        let start = self.parse_expr_bp(Prec(Prec::OR.0 + 1))?;
        let has_range = self.check(&TokenKind::DotDot) || self.check(&TokenKind::DotDotEq);
        if !has_range {
            return Ok(start);
        }
        let inclusive = self.match_kind(&TokenKind::DotDotEq);
        if !inclusive {
            self.advance(); // `..`
        }
        let end = if self.check(&TokenKind::RBracket) {
            missing()
        } else {
            self.parse_expr_bp(Prec(Prec::OR.0 + 1))?
        };
        let op = if inclusive {
            BinaryOp::RangeInclusive
        } else {
            BinaryOp::Range
        };
        Ok(Expression::Binary(
            Box::new(start),
            op,
            Box::new(end),
            span,
        ))
    }

    /// 解析 `break` / `continue` 后面可能跟的循环标签。
    ///
    /// - `'outer`（Lifetime token）无条件当标签；
    /// - 裸标识符只有在与外层已声明的标签同名时才算标签，
    ///   否则留给"带值的 break"（`loop { break 42; }`）。
    fn parse_loop_label(&mut self) -> Option<String> {
        if let TokenKind::Lifetime(name) = &self.peek().kind {
            let name = name.clone();
            self.advance();
            return Some(name);
        }
        if let TokenKind::Ident(name) = &self.peek().kind {
            if self.labels.iter().rev().any(|l| l == name) {
                let name = name.clone();
                self.advance();
                return Some(name);
            }
        }
        None
    }

    /// 解析表达式，直到遇到优先级 ≤ `min_prec` 的运算符为止。
    fn parse_expr_bp(&mut self, min_prec: Prec) -> Result<Expression, CompileError> {
        let mut left = self.parse_prefix()?;

        loop {
            // 块状表达式（`if` / `match` / `while` / `for` / `loop` / `{ }`）
            // 在语句位置不再参与后续运算符绑定。
            //
            // 否则 `if c { .. }` 后面紧跟一条以 `*` 开头的语句（例如
            // `*p = v`）会被当成二元乘法：`(if c { .. }) * (p = v)`，于是
            // 赋值语句"消失"、类型检查报"expression is not assignable"。
            // Rust 解析器同样禁止块表达式出现在二元运算的左侧。
            if super::is_block_like(&left) {
                break;
            }

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
                                    // Rust 风格后缀 `.await`，优先级与字段访问一致。
                    if self.match_kind(&TokenKind::KwAwait) {
                        left = Expression::Await(Box::new(left), op_span);
                        continue;
                    }
                    // 元组字段 `.0` / `.1`：词法上是点 + 整数字面量。
                    let field = match &self.peek().kind {
                        TokenKind::IntLiteral(n, _) => {
                            let field = n.to_string();
                            self.advance();
                            field
                        }
                        _ => self.consume_ident("expected field name")?,
                    };
                    left = Expression::Field(Box::new(left), field, op_span);
                    continue;
                }
                TokenKind::LBracket => {
                    self.advance(); // [
                    let index = self.parse_index_operand()?;
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
        if !self.no_block_expr && self.match_kind(&TokenKind::LBrace) {
            let blk = self.parse_block_contents()?;
            self.consume(&TokenKind::RBrace, "expected '}' to close block")?;
            return Ok(Expression::Block(blk, span));
        }

        // ===== if 表达式（包括 if let） =====
        if self.match_kind(&TokenKind::KwIf) {
            if self.match_kind(&TokenKind::KwLet) {
                // if let pattern = expr { ... } else { ... }
                let span = self.previous().span;
                let pat = self.parse_pattern()?;
                self.consume(&TokenKind::Eq, "expected '=' after pattern in if let")?;
                let saved_block = self.no_block_expr;
                let saved_struct = self.no_struct_literal;
                self.no_block_expr = true;
                self.no_struct_literal = true;
                let expr = self.parse_expression()?;
                self.no_block_expr = saved_block;
                self.no_struct_literal = saved_struct;
                let then_b = self.parse_block()?;
                let else_b = if self.match_kind(&TokenKind::KwElse) {
                    if self.check(&TokenKind::KwIf) {
                        self.advance();
                        let (c2, t2, e2, _) = self.parse_if()?;
                        Some(Box::new(Expression::If(c2, t2, e2, Span::unknown())))
                    } else if self.check(&TokenKind::LBrace) {
                        let b = self.parse_block()?;
                        Some(Box::new(Expression::Block(b, Span::unknown())))
                    } else {
                        // else 后面是表达式
                        let e = self.parse_expression()?;
                        Some(Box::new(e))
                    }
                } else {
                    None
                };
                return Ok(Expression::IfLet(pat, Box::new(expr), then_b, else_b, span));
            } else {
                return self
                    .parse_if()
                    .map(|(c, t, e, s)| Expression::If(c, t, e, s));
            }
        }

        // ===== match 表达式 =====
        if self.match_kind(&TokenKind::KwMatch) {
            return self
                .parse_match()
                .map(|(e, arms, s)| Expression::Match(e, arms, s));
        }

        // ===== while 表达式 =====
        if self.match_kind(&TokenKind::KwWhile) {
            return self.parse_while();
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
            let label = self.parse_loop_label();
            let value = if label.is_none() && self.check_expr_start() {
                Some(Box::new(self.parse_expression()?))
            } else {
                None
            };
            return Ok(Expression::Break(label, value, span));
        }
        if self.match_kind(&TokenKind::KwContinue) {
            let label = self.parse_loop_label();
            return Ok(Expression::Continue(label, span));
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
        // `await` 只允许后缀写法（`future.await`），与 Rust 一致。
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
            let saved_block = self.no_block_expr;
            self.no_block_expr = false;
            let expr = self.parse_expression()?;
            self.no_block_expr = saved_block;
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
            } else {
                self.match_kind(&TokenKind::KwConst);
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
                params.push(self.parse_lambda_param()?);
                while self.match_kind(&TokenKind::Comma) {
                    params.push(self.parse_lambda_param()?);
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

        // ===== 有标签的块 / 循环 =====
        //
        // 标签语法同时接受 Zig 风格的 `outer: loop { .. }` 和 Rust 风格的
        // `'outer: loop { .. }`。`break` / `continue` 里的标签在解析期记在
        // `self.labels` 里，用来和"带值的 break"区分。
        if let TokenKind::Lifetime(label) = &kind {
            let label = label.clone();
            self.advance(); // 'label
            self.consume(&TokenKind::Colon, "expected ':' after label")?;
            self.labels.push(label.clone());
            let expr = self.parse_expression()?;
            self.labels.pop();
            return Ok(Expression::Labeled(label, Box::new(expr), span));
        }
        if let TokenKind::Ident(name) = &kind {
            if self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::Colon)
                .unwrap_or(false)
            {
                let label = name.clone();
                self.advance(); // ident
                self.advance(); // :
                self.labels.push(label.clone());
                let expr = self.parse_expression()?;
                self.labels.pop();
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
            // ===== `macro_rules!` 宏调用 =====
            // 形如：`double!(21)`、`max!(a, b)`。
            //
            // 参数按**记号序列**原样收集：宏的匹配模式是记号级的
            // （`($x:expr)`），要等整份程序解析完、宏定义都齐了之后
            // 才能匹配并展开，所以这里不能直接解析成表达式。
            if self.check(&TokenKind::Bang) {
                self.advance(); // !
                // 允许 `name! { ... }` 与 `name! [ ... ]` 两种定界符。
                let close = match self.peek().kind {
                    TokenKind::LParen => TokenKind::RParen,
                    TokenKind::LBrace => TokenKind::RBrace,
                    TokenKind::LBracket => TokenKind::RBracket,
                    _ => {
                        return Err(self.expected(
                            "expected '(', '{' or '[' after macro name",
                            &self.peek().kind,
                        ))
                    }
                };
                self.advance(); // 开定界符
                let mut args: Vec<TokenKind> = Vec::new();
                let mut depth: i32 = 0;
                loop {
                    if self.is_at_end() {
                        return Err(self.expected("unterminated macro call", &close));
                    }
                    if self.check(&close) && depth == 0 {
                        self.advance(); // 闭定界符
                        break;
                    }
                    let k = self.advance().kind;
                    match &k {
                        TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
                        TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => depth -= 1,
                        _ => {}
                    }
                    args.push(k);
                }
                return Ok(Expression::MacroCall {
                    name: n,
                    args,
                    span,
                });
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
                // 带结构体载荷的枚举变体：`Shape::Circle { r: 3 }`。
                // 路径后面跟着 `{` 时按结构体字面量解析（判别条件与单段
                // 路径的 `Point { .. }` 相同）。
                if !self.no_struct_literal && self.check(&TokenKind::LBrace) && self.looks_like_struct_literal() {
                    self.advance(); // {
                    return self.parse_struct_literal(path, span);
                }
                return Ok(Expression::Path(path, span));
            }
            // 结构体字面量：`Point { x: 1, y: 2 }` 或简写 `Point { x, y }`。
            // `{` 后紧跟「标识符」且其后是 `:` 或 `,`/`}` 时才认定，
            // 避免和块语法冲突；`if` / `while` 条件位置由
            // `no_struct_literal` 额外禁用。
            let looks_like_struct =
                !self.no_struct_literal && self.check(&TokenKind::LBrace) && self.looks_like_struct_literal();
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
            TokenKind::IntLiteral(v, suffix) => {
                self.advance();
                Literal::Int(v, suffix)
            }
            TokenKind::FloatLiteral(v, suffix) => {
                self.advance();
                Literal::Float(v, suffix)
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

    fn parse_if(&mut self) -> Result<IfParseResult, CompileError> {
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

    fn parse_while(&mut self) -> Result<Expression, CompileError> {
        let span = self.previous().span;

        // `while let PAT = EXPR { body }`：脱糖成
        // `loop { if let PAT = EXPR { body } else { break; } }`。
        // 这样每轮都重新求值 EXPR，`continue` 也自然回到循环头。
        if self.match_kind(&TokenKind::KwLet) {
            let pat = self.parse_pattern()?;
            self.consume(&TokenKind::Eq, "expected '=' after pattern in while let")?;
            let saved_block = self.no_block_expr;
            let saved_struct = self.no_struct_literal;
            self.no_block_expr = true;
            self.no_struct_literal = true;
            let scrutinee = self.parse_expression()?;
            self.no_block_expr = saved_block;
            self.no_struct_literal = saved_struct;
            let body = self.parse_block()?;
            // 先把被匹配值绑到一个临时量：每轮只求值一次，且让
            // 枚举/结构体的聚合返回值先落到局部量里（直接在
            // `EnumTag(<调用>)` 里用会在 MIR 侧降成 Unit）。
            let scrut_name = format!("__while_let_{}", span.start);
            let bind_scrut = Statement::Let(
                Pattern::Binding(
                    scrut_name.clone(),
                    crate::parser::ast::Mutability::Immutable,
                    None,
                    span,
                ),
                None,
                Some(scrutinee),
                span,
            );
            let break_expr = Expression::Break(None, None, span);
            let else_block = Block {
                stmts: vec![Statement::Expr(break_expr, span)],
                tail: None,
                span,
            };
            let scrut_ref = Expression::Variable(scrut_name, span);
            let iflet = Expression::IfLet(
                pat,
                Box::new(scrut_ref),
                body,
                Some(Box::new(Expression::Block(else_block, span))),
                span,
            );
            let loop_block = Block {
                stmts: vec![bind_scrut, Statement::Expr(iflet, span)],
                tail: None,
                span,
            };
            // 脱糖完成，`else` 分支由 `break` 提供。
            return Ok(Expression::Loop(loop_block, span));
        }

        let cond = Box::new(self.parse_condition()?);
        let body = self.parse_block()?;
        let else_body = if self.match_kind(&TokenKind::KwElse) {
            Some(self.parse_block()?)
        } else {
            None
        };
        Ok(Expression::While(cond, body, else_body, span))
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
                elems.push(self.parse_nested_expr()?);
            }
        }
        self.consume(&TokenKind::RBracket, "expected ']'")?;
        Ok(Expression::Array(elems, repeat, span))
    }

    /// 当前的 `{` 是不是结构体字面量的开头。
    ///
    /// `{` 后紧跟「标识符」且其后是 `:` 或 `,` / `}` 时才认定，避免和块
    /// 语法冲突；`Type { ..base }` 没有字段名，`{` 后面直接是 `..`。
    fn looks_like_struct_literal(&self) -> bool {
        self.peek_n(1).is_some_and(|t| {
            if matches!(t.kind, TokenKind::DotDot) {
                return true;
            }
            matches!(t.kind, TokenKind::Ident(_))
                && self
                    .peek_n(2)
                    .map(|t| {
                        matches!(
                            t.kind,
                            TokenKind::Colon | TokenKind::Comma | TokenKind::RBrace
                        )
                    })
                    .unwrap_or(false)
        })
    }

    /// 解析闭包的一个参数：模式 + 可选的类型标注（`|x: i32|`）。
    fn parse_lambda_param(&mut self) -> Result<crate::parser::ast::LambdaParam, CompileError> {
        let pattern = self.parse_pattern_single()?;
        let ty = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(crate::parser::ast::LambdaParam { pattern, ty })
    }

    /// 解析 `if` / `while` 的条件表达式：禁用结构体字面量。
    fn parse_condition(&mut self) -> Result<Expression, CompileError> {
        let saved_struct = self.no_struct_literal;
        let saved_block = self.no_block_expr;
        self.no_struct_literal = true;
        self.no_block_expr = true;
        let cond = self.parse_expression();
        self.no_struct_literal = saved_struct;
        self.no_block_expr = saved_block;
        cond
    }
    fn parse_struct_literal(
        &mut self,
        path: Vec<String>,
        span: Span,
    ) -> Result<Expression, CompileError> {
        let mut fields = Vec::new();
        let mut base = None;
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            if self.match_kind(&TokenKind::DotDot) {
                base = Some(Box::new(self.parse_expression()?));
                let _ = self.match_kind(&TokenKind::Comma);
                break;
            }
            let name = self.consume_ident("expected field name")?;
            // 字段简写：`Point { x, y }` 等价于 `Point { x: x, y: y }`
            let value = if self.match_kind(&TokenKind::Colon) {
                self.parse_nested_expr()?
            } else {
                Expression::Variable(name.clone(), span)
            };
            fields.push((name, value));
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(Expression::StructInit(path, fields, base, span))
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
                    let value = self.parse_nested_expr()?;
                    args.push(Argument::Named(name, value));
                } else {
                    let value = self.parse_nested_expr()?;
                    args.push(Argument::Pos(value));
                }
            } else {
                let value = self.parse_nested_expr()?;
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
            TokenKind::IntLiteral(..)
                | TokenKind::FloatLiteral(..)
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
    pub fn try_parse_type(&mut self) -> Option<Type> {
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
        let template = match self.peek().kind.clone() {
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
            let is_in = self.check(&TokenKind::KwIn) || self.check(&TokenKind::Ident("in".into()));
            if !is_out && !is_in {
                // 跳过未知参数
                self.advance();
                continue;
            }
            self.advance(); // in/out
            self.consume(&TokenKind::LParen, "expected '(' after in/out")?;
            let constraint = match self.peek().kind.clone() {
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
            let operand = crate::parser::ast::AsmOperand { constraint, expr };
            if is_out {
                outputs.push(operand);
            } else {
                inputs.push(operand);
            }
        }

        self.consume(&TokenKind::RParen, "expected ')' to close asm!")?;

        Ok(Expression::Asm(
            crate::parser::ast::AsmExpr {
                template,
                outputs,
                inputs,
            },
            span,
        ))
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
