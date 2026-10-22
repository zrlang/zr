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
//! let_stmt       := 'let' 'mut'? pattern (':' type)? ('=' expr)? ';'
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
use crate::lexer::{Span, Token, TokenKind};

/// 解析器。
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// 正在解析 `if` / `while` 的条件：此时禁用结构体字面量。
    ///
    /// `if x { ... }` 里的 `{` 必须是条件体块，不能被当成结构体字面量
    /// 的起始（Rust 的 "no struct literal in condition position" 规则）。
    no_struct_literal: bool,
    no_block_expr: bool,
}

impl Parser {
    /// 创建解析器。
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            no_struct_literal: false,
            no_block_expr: false,
        }
    }

    /// 解析整个编译单元。
    pub fn parse(&mut self) -> Result<Program> {
        let mut program = Program {
            attrs: self.parse_inner_attrs()?,
            ..Program::default()
        };

        // crate 级别的 `#[cfg]`（`#![cfg(..)]`）：条件不成立时整个文件
        // 视为空。
        //
        // 逐文件判断（不是逐 crate）：模块文件里的 `#![cfg(..)]` 只影响
        // 那个文件本身。
        if program.attrs.iter().any(|a| {
            a.name == "cfg" && !crate::attributes::is_cfg_enabled(
                std::slice::from_ref(a),
                &crate::attributes::global_target(),
            )
        }) {
            return Ok(program);
        }

        while !self.is_at_end() {
            // 被 `#[cfg]` 过滤掉的条目直接丢弃：它对后面的名称解析而言
            // 根本不存在。
            let Some(item) = self.parse_module_item_opt()? else {
                continue;
            };
            match item {
                ModuleItem::Function(f) => program.functions.push(f),
                ModuleItem::TypeDef(t) => program.type_defs.push(t),
                ModuleItem::Trait(t) => program.traits.push(t),
                ModuleItem::Impl(i) => program.impls.push(i),
                ModuleItem::Const(c) => program.consts.push(c),
                ModuleItem::Static(s) => program.statics.push(s),
                ModuleItem::Use(u) => program.imports.push(u),
                ModuleItem::Module(m) => program.modules.push(m),
                ModuleItem::Extern(e) => {
                    // 外部函数并入函数表，让其参与类型检查 / 名称解析；
                    // 同时保留外部块（链接指令等将来从这里取）。
                    program.functions.extend(e.functions.iter().cloned());
                    program.externs.push(e);
                }
                ModuleItem::Macro(m) => program.macros.push(m),
            }
        }
        Ok(program)
    }

    /// 解析一个模块级条目。
    ///
    /// 被 `#[cfg]` 过滤掉的条目返回 `None`：它**不会**进入 AST，因此对
    /// 名称解析而言完全不存在（Rust 的语义）。
    fn parse_module_item_opt(&mut self) -> Result<Option<ModuleItem>> {
        let attrs = self.parse_outer_attrs()?;
        let vis = self.parse_visibility();

        // cfg 过滤：条件不成立的条目在进入 AST 之前就被剔除。
        //
        // 之所以在**解析阶段**做（而不是留给类型检查器）：`const` /
        // `static` / `enum` / `impl` / `type` 这些条目的属性并没有被 AST
        // 保留下来（只有 `fn` 与 `struct` 保留了），等到类型检查阶段已经
        // 无从知道它们带过什么属性了。
        if !crate::attributes::is_cfg_enabled(&attrs, &crate::attributes::global_target()) {
            // 条件不成立：条目照样解析（语法错误仍要报出来），但不进 AST。
            let _ = self.parse_module_item_body(vis, attrs)?;
            return Ok(None);
        }
        Ok(Some(self.parse_module_item_body(vis, attrs)?))
    }

    /// 解析一个模块级条目（`decl.rs` 里内联模块的调用入口）。
    ///
    /// 内联模块的条目列表由 `decl.rs` 收集，无法"少放一个"，所以被 cfg
    /// 掉的条目在这里以**空 extern 块**占位：它不含任何函数，对后续阶段
    /// 没有任何影响。
    fn parse_module_item(&mut self) -> Result<ModuleItem> {
        Ok(self.parse_module_item_opt()?.unwrap_or(ModuleItem::Extern(ExternBlock {
            functions: Vec::new(),
        })))
    }

    /// 模块级条目的本体（属性与可见性已经解析过了）。
    fn parse_module_item_body(
        &mut self,
        vis: Visibility,
        attrs: Vec<Attribute>,
    ) -> Result<ModuleItem> {
        // 分流
        // `macro_rules! name { ... }`
        if self.check(&TokenKind::Ident("macro_rules".into()))
            && self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::Bang)
                .unwrap_or(false)
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
        if self.check(&TokenKind::KwAsync)
            && self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::KwFn)
                .unwrap_or(false)
        {
            self.advance(); // async
            self.advance(); // fn
            return self
                .parse_function(vis, attrs, true, false)
                .map(ModuleItem::Function);
        }
        // `default fn`（特化中的默认方法）
        if self.check(&TokenKind::KwDefault)
            && self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::KwFn)
                .unwrap_or(false)
        {
            self.advance(); // default
            self.advance(); // fn
            return self
                .parse_function(vis, attrs, false, true)
                .map(ModuleItem::Function);
        }
        // `comptime fn`（编译期可调用）与 `comptime const`（编译期必为常量）。
        //
        // `parse_function` 内部本来就会吃掉 `comptime` 并设置 `is_comptime`，
        // 但条目分流在这里就得把它认出来——早先没有这一支，于是
        // `comptime fn` 直接撞上"期望 fn/let/const/…"的语法错误。
        if self.check(&TokenKind::KwComptime)
            && self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::KwFn)
                .unwrap_or(false)
        {
            self.advance(); // comptime
            self.advance(); // fn
            return self
                .parse_function_ext(vis, attrs, false, false, true)
                .map(ModuleItem::Function);
        } else if self.check(&TokenKind::KwComptime)
            && self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::KwConst)
                .unwrap_or(false)
        {
            self.advance(); // comptime
            return self.parse_const(vis, attrs).map(ModuleItem::Const);
        } else if self.match_kind(&TokenKind::KwFn) {
            return self
                .parse_function(vis, attrs, false, false)
                .map(ModuleItem::Function);
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
            "fn/let/const/struct/enum/union/type/trait/impl/mod/use/extern/static",
        ]))
    }

    // ===== 属性 =====

    /// 解析内部属性（出现在 `mod`/`fn` 之后的 `#![...]`）。
    fn parse_inner_attrs(&mut self) -> Result<Vec<Attribute>> {
        let mut attrs = Vec::new();
        while self.check(&TokenKind::Hash)
            && self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::Bang)
                .unwrap_or(false)
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
        // `cfg` 走**专用解析路径**：它的参数是 Rust 的 meta item（可以嵌套
        // 的谓词），不是普通表达式。走通用路径的话 `not(unix)` 会被当成
        // 函数调用解析，`target_os = "linux"` 里的 `=` 也会被当成赋值号。
        if name == "cfg" {
            let args = self.parse_cfg_attr_args()?;
            self.consume(&TokenKind::RBracket, "expected ']' to close attribute")?;
            let _ = inner;
            return Ok(Attribute { name, args });
        }
        // `#[deprecated = "..."]`：属性名后面直接跟 `=`（Rust 的写法）。
        //
        // 早先这里没有这种形态，`=` 被当成表达式起点，直接报
        // "unexpected expression start: Eq"。值按**属性名本身**作为键存进
        // 参数表（`deprecated_message` 认得它）。
        if self.check(&TokenKind::Eq) {
            self.advance(); // =
            let v = self.parse_expression()?;
            self.consume(&TokenKind::RBracket, "expected ']' to close attribute")?;
            let _ = inner;
            return Ok(Attribute {
                name: name.clone(),
                args: vec![AttributeArg::NameValue(name, v)],
            });
        }
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
            if self
                .peek_n(1)
                .map(|t| t.kind == TokenKind::Eq)
                .unwrap_or(false)
            {
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
            self.consume(
                &TokenKind::RParen,
                "expected ')' to close attribute arguments",
            )?;
        }
        self.consume(&TokenKind::RBracket, "expected ']' to close attribute")?;
        let _ = inner;
        Ok(Attribute { name, args })
    }

    // ===== cfg 谓词 =====

    /// 解析 `#[cfg(...)]` 的参数：Rust 的 meta item 语法。
    ///
    /// 谓词有三种形态：裸路径（`unix`）、键值对（`target_os = "linux"`）
    /// 与带参数的路径（`not(unix)` / `all(..)` / `any(..)`）。
    fn parse_cfg_attr_args(&mut self) -> Result<Vec<AttributeArg>> {
        self.consume(
            &TokenKind::LParen,
            "expected '(' after 'cfg' (Rust-style: `#[cfg(target_os = \"linux\")]`)",
        )?;
        let mut args = Vec::new();
        while !self.check(&TokenKind::RParen) {
            args.push(self.parse_cfg_pred()?.into_attr_arg());
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.consume(&TokenKind::RParen, "expected ')' to close cfg predicate")?;
        if args.is_empty() {
            return Err(self.expected("expected a cfg predicate", &TokenKind::Ident(String::new())));
        }
        Ok(args)
    }

    /// 解析一个 cfg 谓词。
    fn parse_cfg_pred(&mut self) -> Result<CfgPred> {
        let span = self.peek().span;
        let name = self.consume_ident("expected cfg predicate")?;
        // `key = "value"`
        if self.match_kind(&TokenKind::Eq) {
            let value = self.parse_cfg_value()?;
            return Ok(CfgPred::KeyValue(name, value));
        }
        // `op(..)` —— `all` / `any` / `not` 的嵌套谓词
        if self.check(&TokenKind::LParen) {
            self.advance();
            let mut preds = Vec::new();
            while !self.check(&TokenKind::RParen) {
                preds.push(self.parse_cfg_pred()?);
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
            }
            self.consume(&TokenKind::RParen, "expected ')' to close cfg predicate")?;
            return Ok(CfgPred::Nested(name, preds, span));
        }
        Ok(CfgPred::Flag(name, span))
    }

    /// cfg 键值对的值：必须是字符串字面量。
    fn parse_cfg_value(&mut self) -> Result<Expression> {
        let span = self.peek().span;
        if let TokenKind::StringLiteral(s) = self.peek().kind.clone() {
            self.advance();
            return Ok(Expression::Literal(Literal::String(s), span));
        }
        Err(self.expected(
            "expected a string literal in cfg predicate",
            &TokenKind::StringLiteral(String::new()),
        ))
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
            if self
                .peek_n(1)
                .map(|t| t.kind != TokenKind::Eq)
                .unwrap_or(true)
            {
                let name = self.consume_ident("expected attribute argument")?;
                return Ok(Expression::Variable(name, span));
            }
        } else if self.peek().kind.keyword_text().is_some()
            && self
                .peek_n(1)
                .map(|t| t.kind != TokenKind::Eq)
                .unwrap_or(true)
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
                    TokenKind::KwCrate => {
                        self.advance();
                        Visibility::Crate
                    }
                    TokenKind::KwSelf => {
                        self.advance();
                        Visibility::SelfOnly
                    }
                    TokenKind::KwSuper => {
                        self.advance();
                        Visibility::Super
                    }
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
        // `let` / `let mut`
        if self.match_kind(&TokenKind::KwLet) {
            return self.parse_let_stmt(false);
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

    fn parse_let_stmt(&mut self, mutable: bool) -> Result<Statement> {
        let span = self.peek().span;
        // `mut name` 语法糖
        let (mutability, pattern) = if !mutable && self.check(&TokenKind::KwMut) {
            self.advance();
            (
                crate::parser::ast::Mutability::Mutable,
                self.parse_pattern()?,
            )
        } else {
            (
                if mutable {
                    crate::parser::ast::Mutability::Mutable
                } else {
                    crate::parser::ast::Mutability::Immutable
                },
                self.parse_pattern()?,
            )
        };
        // `mut` 修饰的是绑定本身；把它写回顶层 Binding 模式，
        // 后续类型检查才能知道这个局部变量可被赋值。
        let pattern = match (mutability, pattern) {
            (
                crate::parser::ast::Mutability::Mutable,
                crate::parser::ast::Pattern::Binding(name, _, sub, span),
            ) => crate::parser::ast::Pattern::Binding(
                name,
                crate::parser::ast::Mutability::Mutable,
                sub,
                span,
            ),
            (_, pattern) => pattern,
        };
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
                crate::parser::ast::Pattern::Binding(
                    _,
                    crate::parser::ast::Mutability::Mutable,
                    _,
                    _
                )
            )
        {
            Ok(Statement::Var(pattern, ty, init, span))
        } else {
            Ok(Statement::Let(pattern, ty, init, span))
        }
    }

    /// 在一个**不会与块语法歧义**的上下文里解析表达式。
    ///
    /// `if cond { .. }` 的条件里，`{` 既可能是结构体字面量的开头，也可能是
    /// 块的开头——顶层必须禁用结构体字面量。但嵌套进去之后就没有这个歧义
    /// 了：`f(P { x: 1 })` 的实参、`arr[P { x: 1 }]` 的下标、字段值、
    /// 括号内的表达式里，`{` 只能是结构体字面量。早先这些位置也一并禁用，
    /// 于是 `if f(P { x: 1 }) { .. }` 直接报"expected ')'"。
    fn parse_nested_expr(&mut self) -> Result<crate::parser::ast::Expression> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = false;
        let result = self.parse_expression();
        self.no_struct_literal = saved;
        result
    }

    /// 解析一个块 `{ ... }`，不消费闭合大括号。
    ///
    /// 进入块内恢复结构体字面量：`no_struct_literal` 只用于消解
    /// `if cond { .. }` 顶层的歧义，块内部的 `let p = P { .. }` 不该被禁用。
    fn parse_block_contents(&mut self) -> Result<Block> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = false;
        let result = self.parse_block_contents_inner();
        self.no_struct_literal = saved;
        result
    }

    fn parse_block_contents_inner(&mut self) -> Result<Block> {
        let mut stmts = Vec::new();
        let mut tail: Option<Box<Expression>> = None;
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            // `let` / `const` 语句
            if matches!(
                self.peek().kind,
                TokenKind::KwLet | TokenKind::KwConst
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
        Ok(Block {
            stmts,
            tail,
            span: Span::unknown(),
        })
    }

    /// 解析完整块 `{ ... }`，消费闭合大括号。
    ///
    /// 进入块内要恢复结构体字面量：`no_struct_literal` 只用于消解
    /// `if cond { .. }` 这类顶层歧义，块内部（如 `if comptime { let p = P { .. }; }`）
    /// 再禁用会让合法的结构体字面量报"expected ';' after binding"。
    fn parse_block(&mut self) -> Result<Block> {
        self.consume(&TokenKind::LBrace, "expected '{' to start block")?;
        let saved = self.no_struct_literal;
        self.no_struct_literal = false;
        let blk = self.parse_block_contents();
        self.no_struct_literal = saved;
        let blk = blk?;
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
            | Expression::IfLet(..)
            | Expression::Match(..)
            | Expression::While(..)
            | Expression::For(..)
            | Expression::Loop(..)
            | Expression::Block(..)
    )
}

/// cfg 谓词的解析结果。
///
/// Rust 的 meta item 有三种形态：裸路径（`unix`）、键值对
/// （`target_os = "linux"`）与带参数的路径（`not(unix)`）。解析时先收成
/// 这个中间形态，再按位置编码成属性参数（顶层）或嵌套实参（`all(..)` 里）。
enum CfgPred {
    /// 裸路径：`unix`。
    Flag(String, Span),
    /// 键值对：`target_os = "linux"`。
    KeyValue(String, Expression),
    /// 带参数的路径：`not(unix)` / `all(..)` / `any(..)`。
    Nested(String, Vec<CfgPred>, Span),
}

impl CfgPred {
    /// 编码成属性参数（`#[cfg(..)]` 的顶层）。
    fn into_attr_arg(self) -> AttributeArg {
        match self {
            CfgPred::Flag(name, span) => AttributeArg::Pos(Expression::Variable(name, span)),
            CfgPred::KeyValue(key, value) => AttributeArg::NameValue(key, value),
            CfgPred::Nested(op, preds, span) => {
                let args = preds.into_iter().map(CfgPred::into_arg).collect();
                AttributeArg::Pos(Expression::Call(
                    Box::new(Expression::Variable(op, span)),
                    args,
                    span,
                ))
            }
        }
    }

    /// 编码成嵌套实参（`all(..)` / `any(..)` / `not(..)` 里面）。
    ///
    /// 键值对在这里复用 `Argument::Named` 承载：它的"名字 + 值"结构正好
    /// 对得上 `target_os = "linux"`。
    fn into_arg(self) -> Argument {
        match self {
            CfgPred::Flag(name, span) => Argument::Pos(Expression::Variable(name, span)),
            CfgPred::KeyValue(key, value) => Argument::Named(key, value),
            CfgPred::Nested(op, preds, span) => {
                let args = preds.into_iter().map(CfgPred::into_arg).collect();
                Argument::Pos(Expression::Call(
                    Box::new(Expression::Variable(op, span)),
                    args,
                    span,
                ))
            }
        }
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
    fn let_mut_marks_the_binding_as_mutable() {
        let p = parse_ok("fn main() { let mut x: i32 = 1; x = 2; }");
        let body = p.functions[0].body.as_ref().expect("body");
        match &body.stmts[0] {
            Statement::Var(
                Pattern::Binding(_, Mutability::Mutable, _, _),
                _,
                _,
                _,
            ) => {}
            other => panic!("expected a mutable let binding, got {other:?}"),
        }
    }

    #[test]
    fn parse_if_let_with_else_block_debug() {
        let src = "fn main() { let y = if let z = x { z } else { 0 }; }";
        let tokens = crate::lexer::Lexer::new(src).tokenize();
        println!("Tokens:");
        for (i, t) in tokens.iter().enumerate() {
            println!("  {}: {:?}", i, t);
        }
        let mut p = crate::parser::Parser::new(tokens);
        match p.parse() {
            Ok(prog) => println!("Parsed successfully"),
            Err(e) => {
                println!("Parse error: {}", e);
                // Try to see current parser state
            }
        }
    }

    #[test]
    fn parse_if_let_with_else_block() {
        let tokens = crate::lexer::Lexer::new("fn main() { let y = if let z = x { z } else { 0 }; }").tokenize();
        let mut p = crate::parser::Parser::new(tokens);
        let prog = p.parse().expect("should parse successfully");
        assert_eq!(prog.functions.len(), 1);
        let body = prog.functions[0].body.as_ref().expect("body");
        assert_eq!(body.stmts.len(), 1);
    }

    #[test]
    fn var_is_no_longer_a_binding_keyword() {
        let tokens = Lexer::new("fn main() { var x: i32 = 1; }").tokenize();
        let err = Parser::new(tokens).parse().expect_err("`var` 不应再被接受");
        assert!(!err.message.is_empty());
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
        // 外部函数声明必须并入函数表（否则调用点会报"未定义名称"）。
        assert_eq!(p.externs[0].functions.len(), 1);
        assert_eq!(p.functions.len(), 1);
        assert_eq!(p.functions[0].name, "printf");
        assert!(p.functions[0].body.is_none(), "外部函数没有函数体");
        assert_eq!(p.functions[0].return_type, Type::I(32));
    }

    #[test]
    fn block_expr_does_not_absorb_following_star_stmt() {
        // 回归：`if c { }` 后跟一条以 `*` 开头的语句，曾被 Pratt 循环贪婪地
        // 解析成 `(if c { }) * (p = v)`，于是赋值语句消失。块状表达式不能当
        // 二元运算的左侧，这里应得到两条独立的语句。
        let p = parse_ok("fn main() { if c { d; } *p = 42; }");
        let body = p.functions[0].body.as_ref().expect("函数有体");
        assert_eq!(body.stmts.len(), 2, "块后应另起一条语句");
        assert!(
            matches!(&body.stmts[1], Statement::Expr(Expression::Assign(..), _)),
            "第二条语句应是赋值 `*p = 42`，实际是 {:?}",
            body.stmts[1]
        );
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
    fn parse_impl_methods() {
        let p = parse_ok("impl Drop for Foo { fn drop(&mut self) { } }");
        assert_eq!(p.impls.len(), 1);
        assert_eq!(p.impls[0].methods.len(), 1);
        assert_eq!(p.impls[0].methods[0].name, "drop");
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
