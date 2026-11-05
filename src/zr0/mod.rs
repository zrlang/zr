//! zr0 缩减语法：zr 的最小化子集（~15 关键字）。
//!
//! zr0 是用于自举和教学的最小语法，只包含最核心的语言特性：
//! 函数、结构体、extern 声明、let、if/else、while、return、基本表达式。
//! 不包含泛型、trait、async、match、闭包、模块等高级特性。

use std::collections::HashSet;

/// zr0 允许的关键字集合。
pub const ZR0_KEYWORDS: &[&str] = &[
    "fn", "struct", "extern", "let", "mut", "if", "else", "while", "loop",
    "return", "break", "continue", "true", "false", "as",
];

/// zr0 允许的原始类型。
pub const ZR0_TYPES: &[&str] = &[
    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64",
    "f32", "f64", "bool", "char", "void", "str",
];

/// zr0 语法检查结果。
#[derive(Debug, Default)]
pub struct Zr0Report {
    /// 是否完全符合 zr0 语法。
    pub is_zr0: bool,
    /// 使用的 zr0 之外的特性。
    pub disallowed: Vec<String>,
}

/// 检查源码是否符合 zr0 缩减语法。
pub fn check_zr0(source: &str) -> Zr0Report {
    let mut report = Zr0Report::default();
    report.is_zr0 = true;

    let keywords: HashSet<&str> = ZR0_KEYWORDS.iter().copied().collect();
    let types: HashSet<&str> = ZR0_TYPES.iter().copied().collect();

    // 简单的词法分析：提取标识符
    let mut chars = source.chars().peekable();
    let mut current = String::new();
    let mut identifiers: Vec<String> = Vec::new();

    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '_' {
            current.push(c);
            chars.next();
        } else {
            if !current.is_empty() {
                identifiers.push(std::mem::take(&mut current));
            }
            // 跳过字符串
            if c == '"' {
                chars.next();
                while let Some(&sc) = chars.peek() {
                    chars.next();
                    if sc == '"' {
                        break;
                    }
                    if sc == '\\' {
                        chars.next();
                    }
                }
            } else if c == '/' {
                chars.next();
                if chars.peek() == Some(&'/') {
                    while let Some(&lc) = chars.peek() {
                        if lc == '\n' {
                            break;
                        }
                        chars.next();
                    }
                }
            } else {
                chars.next();
            }
        }
    }
    if !current.is_empty() {
        identifiers.push(current);
    }

    // 检查是否使用了 zr0 之外的关键字
    let forbidden_keywords = [
        "trait", "impl", "generic", "where", "async", "await", "match",
        "enum", "union", "pub", "use", "mod", "for", "in", "self",
        "Self", "dyn", "type", "trait", "unsafe", "const", "static",
        "defer", "try", "catch", "throw", "yield", "spawn",
    ];

    for ident in &identifiers {
        if forbidden_keywords.contains(&ident.as_str()) {
            if !keywords.contains(ident.as_str()) {
                report.disallowed.push(format!("keyword '{}' not in zr0", ident));
                report.is_zr0 = false;
            }
        }
    }

    // 检查是否使用了类型参数 `<...>`（泛型）
    if source.contains('<') && source.contains('>') {
        // 简单启发式：如果有 `<` 后紧跟标识符，可能是泛型
        let s = source;
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'<' {
                // 跳过注释和字符串
                if i > 0 && bytes[i - 1] == b'/' {
                    i += 1;
                    continue;
                }
                // 检查后面是否是标识符
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] as char).is_alphabetic() {
                    report.disallowed.push("generic type parameters not in zr0".to_string());
                    report.is_zr0 = false;
                    break;
                }
            }
            i += 1;
        }
    }

    let _ = types;
    report
}

/// zr0 语法树（简化版 AST）。
#[derive(Debug, Clone)]
pub enum Zr0Item {
    FnDecl(Zr0FnDecl),
    StructDecl(Zr0StructDecl),
    ExternDecl(Zr0ExternDecl),
}

#[derive(Debug, Clone)]
pub struct Zr0FnDecl {
    pub name: String,
    pub params: Vec<Zr0Param>,
    pub return_type: Zr0Type,
    pub body: Vec<Zr0Stmt>,
}

#[derive(Debug, Clone)]
pub struct Zr0Param {
    pub name: String,
    pub ty: Zr0Type,
}

#[derive(Debug, Clone)]
pub struct Zr0StructDecl {
    pub name: String,
    pub fields: Vec<Zr0Field>,
}

#[derive(Debug, Clone)]
pub struct Zr0Field {
    pub name: String,
    pub ty: Zr0Type,
}

#[derive(Debug, Clone)]
pub struct Zr0ExternDecl {
    pub name: String,
    pub params: Vec<Zr0Type>,
    pub return_type: Zr0Type,
}

#[derive(Debug, Clone)]
pub enum Zr0Stmt {
    Let { name: String, ty: Option<Zr0Type>, init: Option<Zr0Expr> },
    If { cond: Zr0Expr, then_branch: Vec<Zr0Stmt>, else_branch: Option<Vec<Zr0Stmt>> },
    While { cond: Zr0Expr, body: Vec<Zr0Stmt> },
    Return(Option<Zr0Expr>),
    Expr(Zr0Expr),
    Break,
    Continue,
}
#[derive(Debug, Clone)]
pub enum Zr0Expr {
    IntLit(i128),
    BoolLit(bool),
    Var(String),
    Unary { op: String, operand: Box<Zr0Expr> },
    Binary { op: String, lhs: Box<Zr0Expr>, rhs: Box<Zr0Expr> },
    Call { callee: Box<Zr0Expr>, args: Vec<Zr0Expr> },
    Field { object: Box<Zr0Expr>, field: String },
    Index { array: Box<Zr0Expr>, index: Box<Zr0Expr> },
    Assign { target: Box<Zr0Expr>, value: Box<Zr0Expr> },
    /// 结构体字面量：`Point { x: 3, y: 4 }`。
    StructLit { name: String, fields: Vec<(String, Zr0Expr)> },
}

#[derive(Debug, Clone)]
pub enum Zr0Type {
    Named(String),
    Pointer(Box<Zr0Type>),
    Array(Box<Zr0Type>, u64),
}

/// 将 zr0 语法树翻译为 C 代码。
pub fn zr0_to_c(items: &[Zr0Item]) -> String {
    let mut out = String::new();
    out.push_str("// Generated by zr0 -> C translator\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include <stdbool.h>\n\n");

    // 前向声明
    for item in items {
        match item {
            Zr0Item::StructDecl(s) => {
                out.push_str(&format!("struct {};\n", s.name));
            }
            Zr0Item::FnDecl(f) => {
                let params = if f.params.is_empty() {
                    "void".to_string()
                } else {
                    f.params
                        .iter()
                        .map(|p| format!("{} {}", zr0_type_to_c(&p.ty), p.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                out.push_str(&format!("{} {}({});\n", zr0_type_to_c(&f.return_type), f.name, params));
            }
            Zr0Item::ExternDecl(e) => {
                let params = e.params.iter().map(|t| zr0_type_to_c(t)).collect::<Vec<_>>().join(", ");
                out.push_str(&format!(
                    "{} {}({});\n",
                    zr0_type_to_c(&e.return_type),
                    e.name,
                    if params.is_empty() { "void".to_string() } else { params }
                ));
            }
        }
    }
    out.push('\n');

    // 结构体定义
    for item in items {
        if let Zr0Item::StructDecl(s) = item {
            out.push_str(&format!("struct {} {{\n", s.name));
            for field in &s.fields {
                out.push_str(&format!("    {} {};\n", zr0_type_to_c(&field.ty), field.name));
            }
            out.push_str("};\n\n");
        }
    }

    // 函数定义
    for item in items {
        if let Zr0Item::FnDecl(f) = item {
            let params = if f.params.is_empty() {
                "void".to_string()
            } else {
                f.params
                    .iter()
                    .map(|p| format!("{} {}", zr0_type_to_c(&p.ty), p.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push_str(&format!("{} {}({}) {{\n", zr0_type_to_c(&f.return_type), f.name, params));
            for stmt in &f.body {
                zr0_stmt_to_c(stmt, &mut out, 1);
            }
            out.push_str("}\n\n");
        }
    }

    out
}

fn zr0_type_to_c(ty: &Zr0Type) -> String {
    match ty {
        Zr0Type::Named(name) => match name.as_str() {
            "i8" => "int8_t".to_string(),
            "i16" => "int16_t".to_string(),
            "i32" => "int32_t".to_string(),
            "i64" => "int64_t".to_string(),
            "u8" => "uint8_t".to_string(),
            "u16" => "uint16_t".to_string(),
            "u32" => "uint32_t".to_string(),
            "u64" => "uint64_t".to_string(),
            "f32" => "float".to_string(),
            "f64" => "double".to_string(),
            "bool" => "bool".to_string(),
            "void" => "void".to_string(),
            "char" => "char".to_string(),
            "str" => "const char*".to_string(),
            other => format!("struct {}", other),
        },
        Zr0Type::Pointer(inner) => format!("{}*", zr0_type_to_c(inner)),
        Zr0Type::Array(inner, len) => format!("{}[{}]", zr0_type_to_c(inner), len),
    }
}

fn zr0_stmt_to_c(stmt: &Zr0Stmt, out: &mut String, indent: usize) {
    let pad = "    ".repeat(indent);
    match stmt {
        Zr0Stmt::Let { name, ty, init } => {
            let ty_str = ty.as_ref().map(zr0_type_to_c).unwrap_or_else(|| "int32_t".to_string());
            if let Some(init) = init {
                out.push_str(&format!("{}{} {} = {};\n", pad, ty_str, name, zr0_expr_to_c(init)));
            } else {
                out.push_str(&format!("{}{} {};\n", pad, ty_str, name));
            }
        }
        Zr0Stmt::If { cond, then_branch, else_branch } => {
            out.push_str(&format!("{}if ({}) {{\n", pad, zr0_expr_to_c(cond)));
            for s in then_branch {
                zr0_stmt_to_c(s, out, indent + 1);
            }
            out.push_str(&format!("{}}}", pad));
            if let Some(el) = else_branch {
                out.push_str(" else {\n");
                for s in el {
                    zr0_stmt_to_c(s, out, indent + 1);
                }
                out.push_str(&format!("{}}}", pad));
            }
            out.push('\n');
        }
        Zr0Stmt::While { cond, body } => {
            out.push_str(&format!("{}while ({}) {{\n", pad, zr0_expr_to_c(cond)));
            for s in body {
                zr0_stmt_to_c(s, out, indent + 1);
            }
            out.push_str(&format!("{}}}\n", pad));
        }
        Zr0Stmt::Return(expr) => {
            if let Some(e) = expr {
                out.push_str(&format!("{}return {};\n", pad, zr0_expr_to_c(e)));
            } else {
                out.push_str(&format!("{}return;\n", pad));
            }
        }
        Zr0Stmt::Expr(e) => {
            out.push_str(&format!("{}{};\n", pad, zr0_expr_to_c(e)));
        }
        Zr0Stmt::Break => {
            out.push_str(&format!("{}break;\n", pad));
        }
        Zr0Stmt::Continue => {
            out.push_str(&format!("{}continue;\n", pad));
        }
    }
}

fn zr0_expr_to_c(expr: &Zr0Expr) -> String {
    match expr {
        Zr0Expr::IntLit(i) => i.to_string(),
        Zr0Expr::BoolLit(b) => b.to_string(),
        Zr0Expr::Var(name) => name.clone(),
        Zr0Expr::Unary { op, operand } => {
            format!("{}{}", op, zr0_expr_to_c(operand))
        }
        Zr0Expr::Binary { op, lhs, rhs } => {
            format!("({} {} {})", zr0_expr_to_c(lhs), op, zr0_expr_to_c(rhs))
        }
        Zr0Expr::Call { callee, args } => {
            let args_str = args.iter().map(zr0_expr_to_c).collect::<Vec<_>>().join(", ");
            format!("{}({})", zr0_expr_to_c(callee), args_str)
        }
        Zr0Expr::Field { object, field } => {
            format!("{}.{}", zr0_expr_to_c(object), field)
        }
        Zr0Expr::Index { array, index } => {
            format!("{}[{}]", zr0_expr_to_c(array), zr0_expr_to_c(index))
        }
        Zr0Expr::Assign { target, value } => {
            format!("{} = {}", zr0_expr_to_c(target), zr0_expr_to_c(value))
        }
        Zr0Expr::StructLit { name, fields } => {
            let fields_str = fields
                .iter()
                .map(|(fname, val)| format!(".{} = {}", fname, zr0_expr_to_c(val)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({}) {{ {} }}", name, fields_str)
        }
    }
}

// =============================================================================
// zr0 解析器：把源码解析成 zr0 语法树（供 `zr0-c` 翻译成 C）。
//
// 用主 lexer 的 token 流做递归下降。zr0 子集很小，解析器可以直接复用
// `crate::lexer`，不依赖主 parser 的 AST（那是全语言，zr0 只要最小子集）。
// =============================================================================

use crate::lexer::{Lexer, Token, TokenKind};

/// 把源码解析成 zr0 语法树。语法错误返回 `Err(消息)`。
pub fn parse_zr0(source: &str) -> Vec<Zr0Item> {
    let tokens = Lexer::new(source).tokenize();
    let mut p = Zr0Parser {
        tokens,
        pos: 0,
    };
    p.parse_items()
}

/// 递归下降解析器（zr0 子集）。
struct Zr0Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Zr0Parser {
    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos.min(self.tokens.len() - 1)].kind
    }

    fn next(&mut self) -> TokenKind {
        let k = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        k
    }

    fn expect(&mut self, kind: TokenKind) -> bool {
        if self.peek() == &kind {
            self.next();
            true
        } else {
            false
        }
    }

    fn parse_items(&mut self) -> Vec<Zr0Item> {
        let mut items = Vec::new();
        while self.peek() != &TokenKind::Eof {
            match self.peek() {
                TokenKind::KwFn => {
                    if let Some(f) = self.parse_fn() {
                        items.push(Zr0Item::FnDecl(f));
                    }
                }
                TokenKind::KwStruct => {
                    if let Some(s) = self.parse_struct() {
                        items.push(Zr0Item::StructDecl(s));
                    }
                }
                TokenKind::KwExtern => {
                    if let Some(e) = self.parse_extern() {
                        items.push(Zr0Item::ExternDecl(e));
                    }
                }
                TokenKind::LexError(msg) => {
                    let msg = msg.clone();
                    self.next();
                    let _ = msg;
                }
                _ => {
                    self.next();
                }
            }
        }
        items
    }

    fn parse_fn(&mut self) -> Option<Zr0FnDecl> {
        self.next(); // fn
        let name = match self.next() {
            TokenKind::Ident(n) => n,
            _ => return None,
        };
        if !self.expect(TokenKind::LParen) {
            return None;
        }
        let mut params = Vec::new();
        while self.peek() != &TokenKind::RParen && self.peek() != &TokenKind::Eof {
            if let TokenKind::Ident(pname) = self.next() {
                let ty = self.parse_type();
                if let Some(ty) = ty {
                    params.push(Zr0Param { name: pname, ty });
                }
            } else {
                self.next();
            }
            if !self.expect(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen);
        let return_type = if self.expect(TokenKind::Arrow) {
            self.parse_type().unwrap_or(Zr0Type::Named("void".to_string()))
        } else {
            Zr0Type::Named("void".to_string())
        };
        if !self.expect(TokenKind::LBrace) {
            return None;
        }
        let body = self.parse_block();
        Some(Zr0FnDecl {
            name,
            params,
            return_type,
            body,
        })
    }

    fn parse_struct(&mut self) -> Option<Zr0StructDecl> {
        self.next(); // struct
        let name = match self.next() {
            TokenKind::Ident(n) => n,
            _ => return None,
        };
        if !self.expect(TokenKind::LBrace) {
            return None;
        }
        let mut fields = Vec::new();
        while self.peek() != &TokenKind::RBrace && self.peek() != &TokenKind::Eof {
            if let TokenKind::Ident(fname) = self.next() {
                self.expect(TokenKind::Colon);
                let ty = self.parse_type();
                if let Some(ty) = ty {
                    fields.push(Zr0Field { name: fname, ty });
                }
            } else {
                self.next();
            }
            self.expect(TokenKind::Comma);
            self.expect(TokenKind::Semicolon);
        }
        self.expect(TokenKind::RBrace);
        Some(Zr0StructDecl { name, fields })
    }

    fn parse_extern(&mut self) -> Option<Zr0ExternDecl> {
        self.next(); // extern
        // 可选 "C" 字符串
        if let TokenKind::StringLiteral(_) = self.peek() {
            self.next();
        }
        if !self.expect(TokenKind::LBrace) {
            return None;
        }
        let mut decls = Vec::new();
        while self.peek() != &TokenKind::RBrace && self.peek() != &TokenKind::Eof {
            if let TokenKind::KwFn = self.peek() {
                self.next();
                if let TokenKind::Ident(name) = self.next() {
                    self.expect(TokenKind::LParen);
                    let mut params = Vec::new();
                    while self.peek() != &TokenKind::RParen && self.peek() != &TokenKind::Eof {
                        let ty = self.parse_type();
                        if let Some(ty) = ty {
                            params.push(ty);
                        } else {
                            self.next();
                        }
                        if !self.expect(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen);
                    let ret = if self.expect(TokenKind::Arrow) {
                        self.parse_type().unwrap_or(Zr0Type::Named("void".to_string()))
                    } else {
                        Zr0Type::Named("void".to_string())
                    };
                    decls.push(Zr0ExternDecl { name, params, return_type: ret });
                }
            } else {
                self.next();
            }
            self.expect(TokenKind::Semicolon);
        }
        self.expect(TokenKind::RBrace);
        decls.into_iter().next()
    }

    fn parse_type(&mut self) -> Option<Zr0Type> {
        match self.peek().clone() {
            TokenKind::Ident(n) => {
                self.next();
                // 指针 / 数组后缀
                let mut ty = Zr0Type::Named(n);
                while let TokenKind::Star = self.peek() {
                    self.next();
                    ty = Zr0Type::Pointer(Box::new(ty));
                }
                Some(ty)
            }
            TokenKind::TyI(w) => {
                self.next();
                Some(Zr0Type::Named(format!("i{w}")))
            }
            TokenKind::TyU(w) => {
                self.next();
                Some(Zr0Type::Named(format!("u{w}")))
            }
            TokenKind::TyF(w) => {
                self.next();
                Some(Zr0Type::Named(format!("f{w}")))
            }
            TokenKind::TyBool => {
                self.next();
                Some(Zr0Type::Named("bool".to_string()))
            }
            TokenKind::TyStr => {
                self.next();
                Some(Zr0Type::Named("str".to_string()))
            }
            TokenKind::TyChar => {
                self.next();
                Some(Zr0Type::Named("char".to_string()))
            }
            TokenKind::KwVoid => {
                self.next();
                Some(Zr0Type::Named("void".to_string()))
            }
            TokenKind::LBracket => {
                // 数组类型：`[T; N]`
                self.next();
                let inner = self.parse_type()?;
                let len = if self.expect(TokenKind::Semicolon) {
                    match self.next() {
                        TokenKind::IntLiteral(v, _) => v as u64,
                        _ => 0,
                    }
                } else {
                    0
                };
                self.expect(TokenKind::RBracket);
                Some(Zr0Type::Array(Box::new(inner), len))
            }
            _ => None,
        }
    }

    fn parse_block(&mut self) -> Vec<Zr0Stmt> {
        let mut stmts = Vec::new();
        while self.peek() != &TokenKind::RBrace && self.peek() != &TokenKind::Eof {
            if let Some(s) = self.parse_stmt() {
                stmts.push(s);
            } else {
                self.next();
            }
        }
        self.expect(TokenKind::RBrace);
        stmts
    }

    fn parse_stmt(&mut self) -> Option<Zr0Stmt> {
        match self.peek() {
            TokenKind::KwLet => {
                self.next();
                // 可选 `mut`
                if self.peek() == &TokenKind::KwMut {
                    self.next();
                }
                let name = match self.next() {
                    TokenKind::Ident(n) => n,
                    _ => return None,
                };
                let ty = if self.expect(TokenKind::Colon) {
                    self.parse_type()
                } else {
                    None
                };
                let init = if self.expect(TokenKind::Eq) {
                    self.parse_expr()
                } else {
                    None
                };
                self.expect(TokenKind::Semicolon);
                Some(Zr0Stmt::Let { name, ty, init })
            }
            TokenKind::KwIf => {
                self.next();
                let cond = self.parse_expr().unwrap_or(Zr0Expr::BoolLit(false));
                self.expect(TokenKind::LBrace);
                let then_branch = self.parse_block();
                let else_branch = if self.expect(TokenKind::KwElse) {
                    if self.expect(TokenKind::KwIf) {
                        // else if 链：合成嵌套 if
                        let cond2 = self.parse_expr().unwrap_or(Zr0Expr::BoolLit(false));
                        self.expect(TokenKind::LBrace);
                        let b2 = self.parse_block();
                        let tail = if self.expect(TokenKind::KwElse) {
                            self.expect(TokenKind::LBrace);
                            Some(self.parse_block())
                        } else {
                            None
                        };
                        Some(vec![Zr0Stmt::If { cond: cond2, then_branch: b2, else_branch: tail }])
                    } else {
                        self.expect(TokenKind::LBrace);
                        Some(self.parse_block())
                    }
                } else {
                    None
                };
                Some(Zr0Stmt::If { cond, then_branch, else_branch })
            }
            TokenKind::KwWhile => {
                self.next();
                let cond = self.parse_expr().unwrap_or(Zr0Expr::BoolLit(false));
                self.expect(TokenKind::LBrace);
                let body = self.parse_block();
                Some(Zr0Stmt::While { cond, body })
            }
            TokenKind::KwReturn => {
                self.next();
                let expr = if self.peek() != &TokenKind::Semicolon {
                    self.parse_expr()
                } else {
                    None
                };
                self.expect(TokenKind::Semicolon);
                Some(Zr0Stmt::Return(expr))
            }
            TokenKind::KwBreak => {
                self.next();
                self.expect(TokenKind::Semicolon);
                Some(Zr0Stmt::Break)
            }
            TokenKind::KwContinue => {
                self.next();
                self.expect(TokenKind::Semicolon);
                Some(Zr0Stmt::Continue)
            }
            _ => {
                let e = self.parse_expr();
                self.expect(TokenKind::Semicolon);
                e.map(Zr0Stmt::Expr)
            }
        }
    }

    fn parse_expr(&mut self) -> Option<Zr0Expr> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Option<Zr0Expr> {
        let lhs = self.parse_binary(0)?;
        if self.expect(TokenKind::Eq) {
            let rhs = self.parse_assign()?;
            return Some(Zr0Expr::Assign { target: Box::new(lhs), value: Box::new(rhs) });
        }
        Some(lhs)
    }

    fn parse_binary(&mut self, min_prec: u8) -> Option<Zr0Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let prec = match self.peek() {
                TokenKind::Plus | TokenKind::Minus => 1,
                TokenKind::Star | TokenKind::Slash | TokenKind::Percent => 2,
                TokenKind::EqEq | TokenKind::BangEq | TokenKind::Lt | TokenKind::Gt
                | TokenKind::Le | TokenKind::Ge => 3,
                TokenKind::AmpAmp => 4,
                TokenKind::PipePipe => 5,
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            let op = match self.next() {
                TokenKind::Plus => "+",
                TokenKind::Minus => "-",
                TokenKind::Star => "*",
                TokenKind::Slash => "/",
                TokenKind::Percent => "%",
                TokenKind::EqEq => "==",
                TokenKind::BangEq => "!=",
                TokenKind::Lt => "<",
                TokenKind::Gt => ">",
                TokenKind::Le => "<=",
                TokenKind::Ge => ">=",
                TokenKind::AmpAmp => "&&",
                TokenKind::PipePipe => "||",
                _ => break,
            }.to_string();
            let rhs = self.parse_binary(prec + 1)?;
            lhs = Zr0Expr::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Some(lhs)
    }

    fn parse_unary(&mut self) -> Option<Zr0Expr> {
        match self.peek() {
            TokenKind::Minus => {
                self.next();
                let operand = self.parse_unary()?;
                Some(Zr0Expr::Unary { op: "-".to_string(), operand: Box::new(operand) })
            }
            TokenKind::Bang => {
                self.next();
                let operand = self.parse_unary()?;
                Some(Zr0Expr::Unary { op: "!".to_string(), operand: Box::new(operand) })
            }
            TokenKind::Star => {
                self.next();
                let operand = self.parse_unary()?;
                Some(Zr0Expr::Unary { op: "*".to_string(), operand: Box::new(operand) })
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Option<Zr0Expr> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek() {
                TokenKind::Dot => {
                    self.next();
                    if let TokenKind::Ident(field) = self.next() {
                        e = Zr0Expr::Field { object: Box::new(e), field };
                    } else {
                        break;
                    }
                }
                TokenKind::LBracket => {
                    self.next();
                    let idx = self.parse_expr()?;
                    self.expect(TokenKind::RBracket);
                    e = Zr0Expr::Index { array: Box::new(e), index: Box::new(idx) };
                }
                TokenKind::LParen => {
                    self.next();
                    let mut args = Vec::new();
                    while self.peek() != &TokenKind::RParen && self.peek() != &TokenKind::Eof {
                        if let Some(a) = self.parse_expr() {
                            args.push(a);
                        } else {
                            self.next();
                        }
                        if !self.expect(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen);
                    e = Zr0Expr::Call { callee: Box::new(e), args };
                }
                _ => break,
            }
        }
        Some(e)
    }

    fn parse_primary(&mut self) -> Option<Zr0Expr> {
        match self.peek().clone() {
            TokenKind::IntLiteral(v, _) => {
                self.next();
                Some(Zr0Expr::IntLit(v))
            }
            TokenKind::KwTrue => {
                self.next();
                Some(Zr0Expr::BoolLit(true))
            }
            TokenKind::KwFalse => {
                self.next();
                Some(Zr0Expr::BoolLit(false))
            }
            TokenKind::Ident(name) => {
                self.next();
                // 结构体字面量：`Point { x: 3, y: 4 }`
                if self.peek() == &TokenKind::LBrace {
                    self.next();
                    let mut fields = Vec::new();
                    while self.peek() != &TokenKind::RBrace && self.peek() != &TokenKind::Eof {
                        if let TokenKind::Ident(fname) = self.next() {
                            self.expect(TokenKind::Colon);
                            if let Some(val) = self.parse_expr() {
                                fields.push((fname, val));
                            }
                        } else {
                            self.next();
                        }
                        self.expect(TokenKind::Comma);
                    }
                    self.expect(TokenKind::RBrace);
                    return Some(Zr0Expr::StructLit { name, fields });
                }
                Some(Zr0Expr::Var(name))
            }
            TokenKind::LParen => {
                self.next();
                let e = self.parse_expr();
                self.expect(TokenKind::RParen);
                e
            }
            _ => None,
        }
    }
}
