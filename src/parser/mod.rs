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
}

impl Parser {
    /// 创建解析器。
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
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
        if !self.check(&TokenKind::RBracket) {
            if self.peek_n(1).map(|t| t.kind == TokenKind::Eq).unwrap_or(false) {
                self.advance(); // name
                self.advance(); // =
                args.push(AttributeArg::Pos(self.parse_expression()?));
            } else {
                args.push(AttributeArg::Pos(self.parse_expression()?));
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RBracket) {
                        break;
                    }
                    if self.peek_n(1).map(|t| t.kind == TokenKind::Eq).unwrap_or(false) {
                        let _n = self.consume_ident("expected argument name")?;
                        self.advance(); // =
                        let _v = self.parse_expression()?;
                    } else {
                        args.push(AttributeArg::Pos(self.parse_expression()?));
                    }
                }
            }
        }
        self.consume(&TokenKind::RBracket, "expected ']' to close attribute")?;
        let _ = inner;
        Ok(Attribute { name, args })
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

    /// 判断当前 token 是否为给定种类。
    fn check(&self, kind: &TokenKind) -> bool {
        if self.is_at_end() {
            return false;
        }
        std::mem::discriminant(&self.peek().kind) == std::mem::discriminant(kind)
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
            Ok(n)
        } else {
            Err(self.expected(msg, &TokenKind::Ident(String::new())))
        }
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
        assert_eq!(p.functions[0].generics.len(), 2);
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
        assert_eq!(p.functions[0].visibility, Visibility::Public);
        assert_eq!(p.functions[1].visibility, Visibility::Crate);
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
