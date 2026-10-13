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
    }
}
