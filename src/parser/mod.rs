//! 递归下降 + Pratt 表达式解析器。
//!
//! 语法概览：
//!
//! ```ebnf
//! program        := module_item*
//! module_item    := use_decl | mod_decl | extern_block | fn_decl
//!                 | type_def | const_decl | static_decl
//!                 | trait_def | impl_block | attr
//! statement      := let_stmt | var_stmt | const_stmt | expr_stmt
//! let_stmt       := 'let' pattern (':' type)? ('=' expr)? ';'
//! var_stmt       := 'var' pattern (':' type)? ('=' expr)? ';'
//! expr_stmt      := expr ';'
//! expression     := assignment
//! assignment     := logical_or (assign_op expression)?
//! logical_or     := logical_and ('||' logical_and)*
//! logical_and    := bitwise_or ('&&' bitwise_or)*
//! bitwise_or     := bitwise_xor ('|' bitwise_xor)*
//! bitwise_xor    := bitwise_and ('^' bitwise_and)*
//! bitwise_and    := equality ('&' equality)*
//! equality       := comparison (('==' | '!=') comparison)*
//! comparison     := shift (('<'|'>'|'<='|'>=') shift)*
//! shift          := additive (('<<'|'>>') additive)*
//! additive       := multiplicative (('+'|'-') multiplicative)*
//! multiplicative := unary (('*'|'/'|'%') unary)*
//! unary          := ('-'|'!'|'~'|'*'|'&') unary | cast
//! cast           := postfix ('as' type)?
//! postfix        := primary (call|index|field|method)*
//! primary        := literal | ident | '(' expr ')' | block
//!                 | if_expr | match_expr | while_expr | for_expr
//!                 | loop_expr | lambda | array | tuple | struct_init
//! ```

pub mod ast;
mod decl;
mod expr;
mod stmt;
mod ty;

pub use ast::*;

use crate::error::{CompileError, ErrorKind, Result};
use crate::lexer::{Token, TokenKind, Span};

/// 解析器。
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// 正在解析 `if` / `while` 的条件：此时禁用结构体字面量。
    ///
    /// `if x { ... }` 里的 `{` 必须是条件体块，不能被当成结构体字面量
    /// 的起始（Rust 的 "no struct literal in condition position" 规则）。
    no_struct_literal: bool,
}

impl Parser {
    /// 创建解析器。
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            no_struct_literal: false,
        }
    }

    /// 解析整个编译单元。
    pub fn parse(&mut self) -> Result<Program> {
        let mut program = Program::default();
        program.attrs = self.parse_inner_attrs()?;

        while !self.is_at_end() {
            let item = self.parse_module_item()?;
            match item {
                ModuleItem::Function(f) => program.functions.push(f),
                ModuleItem::TypeDef(t) => program.type_defs.push(t),
                ModuleItem::Trait(t) => program.traits.push(t),
                ModuleItem::Impl(i) => program.impls.push(i),
                ModuleItem::Const(c) => program.consts.push(c),
                ModuleItem::Static(s) => program.statics.push(s),
                ModuleItem::Use(u) => program.imports.push(u),
                ModuleItem::Module(m) => program.modules.push(m),
                ModuleItem::Extern(e) => program.externs.push(e),
                ModuleItem::Macro(m) => program.macros.push(m),
            }
        }
        Ok(program)
    }

    /// 解析一个模块级条目。
    fn parse_module_item(&mut self) -> Result<ModuleItem> {
        let attrs = self.parse_outer_attrs()?;
        let vis = self.parse_visibility();

        // 分流
        // `macro_rules! name { ... }`
        if self.check(&TokenKind::Ident("macro_rules".into()))
            && self.peek_n(1).map(|t| t.kind == TokenKind::Bang).unwrap_or(false)
        {
            self.advance(); // macro_rules
            self.advance(); // !
            return self.parse_macro_rules(attrs).map(ModuleItem::Macro);
        }
        if self.match_kind(&TokenKind::KwUse) {
            return Ok(ModuleItem::Use(self.parse_use(attrs)?));
        }
        if self.match_kind(&TokenKind::KwMod) {
            return self.parse_mod(vis, attrs).map(ModuleItem::Module);
        }
        if self.match_kind(&TokenKind::KwExtern) {
            return self.parse_extern(attrs).map(ModuleItem::Extern);
        }
        if self.check(&TokenKind::KwAsync) && self.peek_n(1).map(|t| t.kind == TokenKind::KwFn).unwrap_or(false) {
            self.advance(); // async
            self.advance(); // fn
            return self.parse_function(vis, attrs, true, false).map(ModuleItem::Function);
        }
        // `default fn`（特化中的默认方法）
        if self.check(&TokenKind::KwDefault) && self.peek_n(1).map(|t| t.kind == TokenKind::KwFn).unwrap_or(false) {
            self.advance(); // default
            self.advance(); // fn
            return self.parse_function(vis, attrs, false, true).map(ModuleItem::Function);
        }
        if self.match_kind(&TokenKind::KwFn) {
            return self.parse_function(vis, attrs, false, false).map(ModuleItem::Function);
        }
        if self.match_kind(&TokenKind::KwConst) {
            return self.parse_const(vis, attrs).map(ModuleItem::Const);
        }
        if self.match_kind(&TokenKind::KwStatic) {
            return self.parse_static(vis, attrs).map(ModuleItem::Static);
        }
        if self.match_kind(&TokenKind::KwStruct) {
            return self.parse_struct_def(vis, attrs).map(ModuleItem::TypeDef);
        }
        if self.match_kind(&TokenKind::KwEnum) {
            return self.parse_enum_def(vis, attrs).map(ModuleItem::TypeDef);
        }
        if self.match_kind(&TokenKind::KwUnion) {
            return self.parse_union_def(vis, attrs).map(ModuleItem::TypeDef);
        }
        if self.match_kind(&TokenKind::KwType) {
            return self.parse_type_alias(vis, attrs).map(ModuleItem::TypeDef);
        }
        if self.match_kind(&TokenKind::KwTrait) {
            return self.parse_trait_def(vis, attrs).map(ModuleItem::Trait);
        }
        if self.match_kind(&TokenKind::KwImpl) {
            return self.parse_impl_block(attrs).map(ModuleItem::Impl);
        }

        Err(self.unexpected(&[
            "fn/let/const/var/struct/enum/union/type/trait/impl/mod/use/extern/static",
        ]))
    }

    // ===== 属性 =====

    /// 解析内部属性（出现在 `mod`/`fn` 之后的 `#![...]`）。
    fn parse_inner_attrs(&mut self) -> Result<Vec<Attribute>> {
        let mut attrs = Vec::new();
        while self.check(&TokenKind::Hash)
            && self.peek_n(1).map(|t| t.kind == TokenKind::Bang).unwrap_or(false)
        {
            self.advance(); // #
            self.advance(); // !
            attrs.push(self.parse_attribute_body(true)?);
        }
        Ok(attrs)
    }

    /// 解析外部属性（出现在条目之前的 `#[...]`）。
    fn parse_outer_attrs(&mut self) -> Result<Vec<Attribute>> {
        let mut attrs = Vec::new();
        while self.match_kind(&TokenKind::Hash) {
            attrs.push(self.parse_attribute_body(false)?);
        }
        Ok(attrs)
    }

    fn parse_attribute_body(&mut self, inner: bool) -> Result<Attribute> {
        self.consume(&TokenKind::LBracket, "expected '[' after '#'")?;
        let name = self.consume_ident("expected attribute name")?;
        let mut args = Vec::new();
        // 参数写在圆括号里：`#[repr(C)]`、`#[inline(always)]`、
        // `#[repr(align = 8)]`。早先这里没有消费 `(`，整个 `(...)` 被
        // 当成一个括号表达式解析，于是 `#[repr(size = 4)]` 里的 `=`
        // 变成了一个**赋值表达式**，具名参数根本没被识别出来。
        let paren = self.match_kind(&TokenKind::LParen);
        let closer = if paren {
            TokenKind::RParen
        } else {
            TokenKind::RBracket
        };
        while !self.check(&closer) {
            if self.peek_n(1).map(|t| t.kind == TokenKind::Eq).unwrap_or(false) {
                // `name = value`
                let n = self.consume_ident("expected argument name")?;
                self.advance(); // =
                let v = self.parse_expression()?;
                args.push(AttributeArg::NameValue(n, v));
            } else {
                args.push(AttributeArg::Pos(self.parse_attribute_arg()?));
            }
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        if paren {
            self.consume(&TokenKind::RParen, "expected ')' to close attribute arguments")?;
        }
        self.consume(&TokenKind::RBracket, "expected ']' to close attribute")?;
        let _ = inner;
        Ok(Attribute { name, args })
    }

    /// 属性的一个位置参数。
    ///
    /// 走 `parse_expression` 会让 `#[inline(never)]` 挂掉：`never` 是
    /// `TyNever` 关键字，`parse_expression` 拿它当表达式起点直接报错。
    /// 属性参数里的裸名字（`never` / `always` / `C`）都是标识符位置，
    /// 关键字在这里要退化成标识符，和 `consume_ident` 的处理一致。
    fn parse_attribute_arg(&mut self) -> Result<Expression> {
        let span = self.peek().span;
        if let TokenKind::Ident(_) = self.peek().kind {
            if self.peek_n(1).map(|t| t.kind != TokenKind::Eq).unwrap_or(true) {
                let name = self.consume_ident("expected attribute argument")?;
                return Ok(Expression::Variable(name, span));
            }
        } else if self.peek().kind.keyword_text().is_some()
            && self.peek_n(1).map(|t| t.kind != TokenKind::Eq).unwrap_or(true)
        {
            let name = self.consume_ident("expected attribute argument")?;
            return Ok(Expression::Variable(name, span));
        }
        self.parse_expression()
    }

    // ===== 可见性 =====

    fn parse_visibility(&mut self) -> Visibility {
        if self.match_kind(&TokenKind::KwPub) {
            if self.match_kind(&TokenKind::LParen) {
                let vis = match self.peek().kind {
                    TokenKind::KwCrate => { self.advance(); Visibility::Crate }
                    TokenKind::KwSelf => { self.advance(); Visibility::SelfOnly }
                    TokenKind::KwSuper => { self.advance(); Visibility::Super }
                    _ => Visibility::Crate,
                };
                self.consume(&TokenKind::RParen, "expected ')'").ok();
                vis
            } else {
                Visibility::Public
            }
        } else {
            Visibility::Private
        }
    }

    // ===== 语句 =====

    /// 解析一条语句。
    fn parse_statement(&mut self) -> Result<Statement> {
        // `let` / `var`
        if self.match_kind(&TokenKind::KwLet) {
            return self.parse_let_stmt(false);
        }
        if self.match_kind(&TokenKind::KwVar) {
            return self.parse_let_stmt(true);
        }
        // `const` 在语句位置 = 局部常量（编译期求值）
        if self.match_kind(&TokenKind::KwConst) {
            let name = self.consume_ident("expected constant name")?;
            self.consume(&TokenKind::Colon, "expected ':' after constant name")?;
            let ty = self.parse_type()?;
            self.consume(&TokenKind::Eq, "expected '=' in constant declaration")?;
            let value = self.parse_expression()?;
            self.consume(&TokenKind::Semicolon, "expected ';' after constant")?;
            let span = Span::unknown();
            return Ok(Statement::Const(name, ty, value, span));
        }
        // 表达式语句
        let expr = self.parse_expression()?;
        if self.match_kind(&TokenKind::Semicolon) {
            Ok(Statement::Expr(expr, Span::unknown()))
        } else {
            // 表达式后没有分号 → 块尾表达式，交给调用者处理
            Ok(Statement::Expr(expr, Span::unknown()))
        }
    }

    fn parse_let_stmt(
        &mut self,
        mutable: bool,
    ) -> Result<Statement> {
        let span = self.peek().span;
        // `mut name` 语法糖
        let (mutability, pattern) = if !mutable && self.check(&TokenKind::KwMut) {
            self.advance();
            (crate::parser::ast::Mutability::Mutable, self.parse_pattern()?)
        } else {
            (if mutable {
                crate::parser::ast::Mutability::Mutable
            } else {
                crate::parser::ast::Mutability::Immutable
            }, self.parse_pattern()?)
        };
        let _ = mutability;
        let ty = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let init = if self.match_kind(&TokenKind::Eq) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        self.consume(&TokenKind::Semicolon, "expected ';' after binding")?;
        // 根据可变性返回 Let 或 Var
        if mutable
            || matches!(
                pattern,
                crate::parser::ast::Pattern::Binding(_, crate::parser::ast::Mutability::Mutable, _, _)
            )
        {
            Ok(Statement::Var(pattern, ty, init, span))
        } else {
            Ok(Statement::Let(pattern, ty, init, span))
        }
    }

    /// 解析一个块 `{ ... }`，不消费闭合大括号。
    fn parse_block_contents(&mut self) -> Result<Block> {
        let mut stmts = Vec::new();
        let mut tail: Option<Box<Expression>> = None;
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            // `let` / `var` / `const` 语句
            if matches!(
                self.peek().kind,
                TokenKind::KwLet | TokenKind::KwVar | TokenKind::KwConst
            ) {
                stmts.push(self.parse_statement()?);
                continue;
            }
            // 表达式语句 / 块尾表达式
            let expr = self.parse_expression()?;
            if self.match_kind(&TokenKind::Semicolon) {
                stmts.push(Statement::Expr(expr, Span::unknown()));
            } else if self.check(&TokenKind::RBrace) {
                tail = Some(Box::new(expr));
                break;
            } else if is_block_like(&expr) {
                // `if` / `match` / `while` / `for` / `loop` / `{ ... }` 这些
                // 自带块的表达式在语句位置不需要分号（和 Rust 一致）。
                // 必须继续解析本块的后续语句，否则 `if c { .. } 下一句;`
                // 会把后面的语句当成"块外"的垃圾，报 "expected '}'"。
                stmts.push(Statement::Expr(expr, Span::unknown()));
            } else {
                // 裸表达式当作尾表达式
                tail = Some(Box::new(expr));
                break;
            }
        }
        Ok(Block { stmts, tail, span: Span::unknown() })
    }

    /// 解析完整块 `{ ... }`，消费闭合大括号。
    fn parse_block(&mut self) -> Result<Block> {
        self.consume(&TokenKind::LBrace, "expected '{' to start block")?;
        let blk = self.parse_block_contents()?;
        self.consume(&TokenKind::RBrace, "expected '}' to close block")?;
        Ok(blk)
    }

    // ===== Token 流操作 =====

    /// 消费当前 token 并返回它。
    fn advance(&mut self) -> Token {
        if !self.is_at_end() {
            self.pos += 1;
        }
        self.previous()
    }

    /// 上一个被消费的 token。
    fn previous(&self) -> Token {
        self.tokens[self.pos - 1].clone()
    }

    /// 查看当前 token（不消费）。
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    /// 查看第 n 个之后的 token。
    fn peek_n(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.pos + n)
    }

    /// 判断当前 token 是否为给定 token。
    ///
    /// 这里必须做**完整相等比较**，不能只比枚举判别值：`Ident`、`IntLiteral`
    /// 这类变体带 payload，只比 discriminant 的话
    /// `check(&Ident("asm"))` 会匹配上**任意**标识符——曾经就让
    /// `intrinsic!(...)` 被误当成 `asm!(...)` 送进内联汇编解析器。
    fn check(&self, kind: &TokenKind) -> bool {
        if self.is_at_end() {
            return false;
        }
        &self.peek().kind == kind
    }

    /// 若匹配则消费，返回 true。
    fn match_kind(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// 必须匹配给定种类，否则报错。
    fn consume(&mut self, kind: &TokenKind, msg: &str) -> Result<Token> {
        if self.check(kind) {
            Ok(self.advance())
        } else {
            Err(self.expected(msg, kind))
        }
    }

    /// 消费一个标识符。
    fn consume_ident(&mut self, msg: &str) -> Result<String> {
        if let TokenKind::Ident(name) = &self.peek().kind {
            let n = name.clone();
            self.advance();
            return Ok(n);
        }
        // 关键字在"名字位置"退化成标识符：标准库里就有
        // `trait Default { fn default() -> Self; }` 这种写法，
        // 直接拒绝会让整个模块编译不过。
        if let Some(kw) = self.peek().kind.keyword_text() {
            let n = kw.to_string();
            self.advance();
            return Ok(n);
        }
        Err(self.expected(msg, &TokenKind::Ident(String::new())))
    }

    /// 路径段：标识符，**或者**被当作标识符的关键字。
    ///
    /// 模块名可以撞上关键字（标准库里的 `async` 就是一例），所以
    /// `use async::{spawn}` 必须能解析——关键字在路径位置退化成标识符。
    fn consume_path_segment(&mut self, msg: &str) -> Result<String> {
        if let TokenKind::Ident(name) = &self.peek().kind {
            let n = name.clone();
            self.advance();
            return Ok(n);
        }
        if let Some(kw) = self.peek().kind.keyword_text() {
            self.advance();
            return Ok(kw.to_string());
        }
        Err(self.expected(msg, &TokenKind::Ident(String::new())))
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.tokens.len() || self.peek().kind == TokenKind::Eof
    }

    /// 构造「期望 X，但得到 Y」的错误。
    fn expected(&self, msg: &str, expected: &TokenKind) -> CompileError {
        let span = self.peek().span;
        let found = format!("{:?}", self.peek().kind);
        CompileError::new(format!("{msg}: expected {expected:?}, found {found}"))
            .with_kind(ErrorKind::Parse)
            .with_span(span)
    }

    /// 构造「意外 token」的错误。
    fn unexpected(&self, expected: &[&str]) -> CompileError {
        let span = self.peek().span;
        let found = format!("{:?}", self.peek().kind);
        CompileError::new(format!(
            "unexpected token {found}, expected one of: {}",
            expected.join(", ")
        ))
        .with_kind(ErrorKind::Parse)
        .with_span(span)
    }
}

/// 判断表达式是否"自带块"——这类表达式在语句位置可以省略分号。
fn is_block_like(expr: &Expression) -> bool {
    matches!(
        expr,
        Expression::If(..)
            | Expression::Match(..)
            | Expression::While(..)
            | Expression::For(..)
            | Expression::Loop(..)
            | Expression::Block(..)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn parse_ok(src: &str) -> Program {
        let tokens = Lexer::new(src).tokenize();
        let mut p = Parser::new(tokens);
        p.parse().expect("should parse successfully")
    }

    #[test]
    fn parse_empty() {
        let p = parse_ok("");
        assert!(p.functions.is_empty());
    }

    #[test]
    fn parse_function_signature() {
        let p = parse_ok("fn add(a: i32, b: i32) -> i32 { return a + b; }");
        assert_eq!(p.functions.len(), 1);
        assert_eq!(p.functions[0].name, "add");
        assert_eq!(p.functions[0].params.len(), 2);
        assert_eq!(p.functions[0].return_type, Type::I(32));
    }

    #[test]
    fn parse_async_function() {
        let p = parse_ok("async fn fetch(url: str) -> str { return url; }");
        assert!(p.functions[0].is_async);
    }

    #[test]
    fn parse_generic_function() {
        let p = parse_ok("fn map<T, const N: usize>(arr: [T; N]) -> T { }");
        assert_eq!(p.functions.len(), 1);
        assert_eq!(p.functions[0].name, "map");
    }

    #[test]
    fn parse_struct() {
        let p = parse_ok("struct Point { x: f64, y: f64, }");
        assert_eq!(p.type_defs.len(), 1);
    }

    #[test]
    fn parse_enum_with_payload() {
        let p = parse_ok("enum Result<T, E> { Ok(T), Err(E), }");
        assert_eq!(p.type_defs.len(), 1);
    }

    #[test]
    fn parse_use() {
        let p = parse_ok("use std::io::{print, println};");
        assert_eq!(p.imports.len(), 1);
    }

    #[test]
    fn parse_extern_block() {
        let p = parse_ok(r#"extern "C" { fn printf(fmt: *const u8, ...) -> i32; }"#);
        assert_eq!(p.externs.len(), 1);
    }

    #[test]
    fn parse_visibility() {
        let p = parse_ok("pub fn public() {} pub(crate) fn crate_local() {}");
        assert_eq!(p.functions.len(), 2);
        assert_eq!(p.functions[0].name, "public");
        assert_eq!(p.functions[1].name, "crate_local");
    }

    #[test]
    fn parse_impl_block() {
        let p = parse_ok("impl<T> Vec<T> { fn push(&mut self, v: T) {} }");
        assert_eq!(p.impls.len(), 1);
    }

    #[test]
    fn parse_trait_def() {
        let p = parse_ok("trait Iterator { type Item; fn next(&mut self) -> Option<Self::Item>; }");
        assert_eq!(p.traits.len(), 1);
    }

    #[test]
    fn parse_const_and_static() {
        let p = parse_ok("const MAX: i32 = 100; static mut COUNTER: i64 = 0;");
        assert_eq!(p.consts.len(), 1);
        assert_eq!(p.statics.len(), 1);
    }

    #[test]
    fn parse_attribute() {
        let p = parse_ok("#[inline(always)] #[link(name = \"c\")] fn f() {}");
        assert_eq!(p.functions.len(), 1);
    }

    #[test]
    fn parse_expressions() {
        parse_ok("fn main() { let x = 1 + 2 * 3; let b = (1 < 2) && (3 >= 4); }");
        parse_ok("fn main() { let s = \"hello\"; let c = '\\n'; let f = 3.14; }");
        parse_ok("fn main() { let a = [1, 2, 3]; let t = (1, 2.0, \"x\"); }");
        parse_ok("fn main() { let r = &mut x; let v = *p; }");
    }

    #[test]
    fn parse_control_flow() {
        parse_ok("fn main() { if a { b } else { c } }");
        parse_ok("fn main() { match x { 1 => 2, 2..=5 => 3, _ => 0 } }");
        parse_ok("fn main() { while cond { break; } }");
        parse_ok("fn main() { for i in 0..10 { continue; } }");
        parse_ok("fn main() { loop { return; } }");
    }
}
