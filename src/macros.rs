//==============================================================================
// macros.rs — `macro_rules!` 声明式宏的展开
//
// 流程：
//
// 1. 解析阶段把 `name!(...)` 记成 `Expression::MacroCall`，参数是**记号序列**；
//    `macro_rules!` 的定义也只保存记号序列（模式 + 替换体）。
// 2. 整份程序解析完之后，本模块遍历 AST，把每个 `MacroCall` 换成展开结果：
//    - 按顺序试每一条臂，用臂的模式匹配实参的记号序列；
//    - 匹配上就按模式里的元变量（`$x:expr` / `$x:ident` / `$x:tt` …）
//      把替换体里的 `$x` 换成对应的记号；
//    - 替换后的记号重新词法分析 + 语法分析，得到真正的表达式。
//
// 为什么"先收集、后展开"而不是"边解析边展开"：
// 宏可以引用后面才定义的宏（Rust 也允许），先展开的话拿不到定义。
// 代价是展开发生在 AST 构造之后，宏里不能再写"半个语句"这种
// 需要跨语句边界的片段——这与 Rust 的限制一致。
//==============================================================================

use crate::error::CompileError;
use crate::lexer::{Lexer, TokenKind};
use crate::parser::ast::{Expression, Program, Span};
use crate::parser::Parser;

/// 元变量的片段类别。
///
/// 类别决定捕获的记号序列要**怎么校验**：
/// - `expr`：整段必须是一个合法表达式；
/// - `ident`：必须是一个标识符；
/// - `ty`：必须是一个合法类型；
/// - `tt`：单个记号（不做额外校验，最宽松）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FragKind {
    Expr,
    Ident,
    Ty,
    Tt,
}

impl FragKind {
    fn from_name(name: &str) -> Option<FragKind> {
        match name {
            "expr" => Some(FragKind::Expr),
            "ident" => Some(FragKind::Ident),
            "ty" => Some(FragKind::Ty),
            "tt" => Some(FragKind::Tt),
            _ => None,
        }
    }
}

/// 一条臂：模式 + 替换体。
#[derive(Debug, Clone)]
struct Arm {
    pattern: Vec<TokenKind>,
    body: Vec<TokenKind>,
}

/// 一次匹配的结果：元变量名 -> 捕获的记号序列。
type Bindings = Vec<(String, Vec<TokenKind>)>;

/// 展开整份程序里的所有宏调用。返回展开后的程序。
pub fn expand(prog: &Program) -> Result<Program, CompileError> {
    let mut prog = prog.clone();
    // 宏定义按名字索引；同名定义后写的覆盖先写的（与 Rust 一致）。
    let mut defs: Vec<(String, Vec<Arm>)> = Vec::new();
    for m in &prog.macros {
        let arms = m
            .arms
            .iter()
            .map(|a| Arm {
                pattern: a.pattern.clone(),
                body: a.body.clone(),
            })
            .collect();
        // 名字重复时后者覆盖前者：与 Rust 的 `macro_rules!` 一致。
        defs.retain(|(n, _)| n != &m.name);
        defs.push((m.name.clone(), arms));
    }
    // 宏定义的行号：报错时告诉用户"这个宏定义在这里"，便于定位。
    let def_lines: Vec<(String, usize)> = prog
        .macros
        .iter()
        .map(|m| (m.name.clone(), m.span.line))
        .collect();

    if defs.is_empty() {
        return Ok(prog);
    }

    // 展开体里可能还有宏调用（宏套宏），所以要反复扫到没有新调用为止。
    // 每一轮至少展开一个调用点，否则说明有环，直接报错收场。
    let mut rounds = 0usize;
    loop {
        let mut changed = false;
        let mut defs_ref = &mut defs;
        expand_in_program(&mut prog, &mut defs_ref, &def_lines, &mut changed)?;
        if !changed {
            break;
        }
        rounds += 1;
        if rounds > 64 {
            return Err(CompileError::new(
                "macro expansion did not terminate (recursive macro?)".to_string(),
            ));
        }
    }
    Ok(prog)
}

/// 把程序里所有 `MacroCall` 展开掉。
fn expand_in_program(
    prog: &mut Program,
    defs: &mut Vec<(String, Vec<Arm>)>,
    def_lines: &[(String, usize)],
    changed: &mut bool,
) -> Result<(), CompileError> {
    for f in prog.functions.iter_mut() {
        if let Some(body) = f.body.as_mut() {
            expand_in_block(body, defs, def_lines, changed)?;
        }
    }
    for i in prog.impls.iter_mut() {
        expand_in_impl(i, defs, def_lines, changed)?;
    }
    Ok(())
}

fn expand_in_impl(
    imp: &mut crate::parser::ast::ImplBlock,
    defs: &mut Vec<(String, Vec<Arm>)>,
    def_lines: &[(String, usize)],
    changed: &mut bool,
) -> Result<(), CompileError> {
    for m in imp.methods.iter_mut() {
        if let Some(body) = m.body.as_mut() {
            expand_in_block(body, defs, def_lines, changed)?;
        }
    }
    Ok(())
}

fn expand_in_block(
    block: &mut crate::parser::ast::Block,
    defs: &mut Vec<(String, Vec<Arm>)>,
    def_lines: &[(String, usize)],
    changed: &mut bool,
) -> Result<(), CompileError> {
    for s in block.stmts.iter_mut() {
        expand_in_stmt(s, defs, def_lines, changed)?;
    }
    if let Some(tail) = block.tail.as_mut() {
        expand_in_expr(tail, defs, def_lines, changed)?;
    }
    Ok(())
}

fn expand_in_stmt(
    stmt: &mut crate::parser::ast::Statement,
    defs: &mut Vec<(String, Vec<Arm>)>,
    def_lines: &[(String, usize)],
    changed: &mut bool,
) -> Result<(), CompileError> {
    use crate::parser::ast::Statement;
    match stmt {
        Statement::Expr(e, _) => expand_in_expr(e, defs, def_lines, changed),
        Statement::Let(_, _, v, _) | Statement::Var(_, _, v, _) => {
            if let Some(v) = v.as_mut() {
                expand_in_expr(v, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Statement::Const(_, _, v, _) => expand_in_expr(v, defs, def_lines, changed),
        Statement::LetElse(_, _, v, block, _) => {
            expand_in_expr(v, defs, def_lines, changed)?;
            for stmt in &mut block.stmts {
                expand_in_stmt(stmt, defs, def_lines, changed)?;
            }
            if let Some(tail) = block.tail.as_mut() {
                expand_in_expr(tail, defs, def_lines, changed)?;
            }
            Ok(())
        }
    }
}

fn expand_in_expr(
    expr: &mut Expression,
    defs: &mut Vec<(String, Vec<Arm>)>,
    def_lines: &[(String, usize)],
    changed: &mut bool,
) -> Result<(), CompileError> {
    use crate::parser::ast::Expression;

    // 宏调用：先展开自己，再展开展开结果内部可能有的宏调用。
    if let Expression::MacroCall { name, args, span } = expr {
        let name = name.clone();
        let args = args.clone();
        let span = *span;
        let expanded = expand_call(&name, &args, defs, def_lines, span)?;
        *expr = expanded;
        *changed = true;
        // 展开结果里可能还有宏调用，继续往下走。
        return expand_in_expr(expr, defs, def_lines, changed);
    }

    match expr {
        Expression::MacroCall { .. } => unreachable!(),
        Expression::Literal(..) | Expression::Variable(..) | Expression::Path(..) => Ok(()),
        Expression::Binary(a, _, b, _) => {
            expand_in_expr(a, defs, def_lines, changed)?;
            expand_in_expr(b, defs, def_lines, changed)
        }
        Expression::Unary(_, a, _) => expand_in_expr(a, defs, def_lines, changed),
        Expression::Assign(a, _, b, _) => {
            expand_in_expr(a, defs, def_lines, changed)?;
            expand_in_expr(b, defs, def_lines, changed)
        }
        Expression::Call(callee, args, _) => {
            expand_in_expr(callee, defs, def_lines, changed)?;
            for a in args.iter_mut() {
                match a {
                    crate::parser::ast::Argument::Pos(e)
                    | crate::parser::ast::Argument::Named(_, e) => {
                        expand_in_expr(e, defs, def_lines, changed)?;
                    }
                }
            }
            Ok(())
        }
        Expression::Index(a, b, _) => {
            expand_in_expr(a, defs, def_lines, changed)?;
            expand_in_expr(b, defs, def_lines, changed)
        }
        Expression::Field(a, _, _) => expand_in_expr(a, defs, def_lines, changed),
        Expression::AddrOf(a, _, _) => expand_in_expr(a, defs, def_lines, changed),
        Expression::Cast(a, _, _) => expand_in_expr(a, defs, def_lines, changed),
        Expression::Block(b, _) => expand_in_block(b, defs, def_lines, changed),
        Expression::If(c, t, e, _) => {
            expand_in_expr(c, defs, def_lines, changed)?;
            expand_in_block(t, defs, def_lines, changed)?;
            if let Some(e) = e.as_mut() {
                expand_in_expr(e, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::IfLet(_, s, t, e, _) => {
            expand_in_expr(s, defs, def_lines, changed)?;
            expand_in_block(t, defs, def_lines, changed)?;
            if let Some(e) = e.as_mut() {
                expand_in_expr(e, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::Match(scrut, arms, _) => {
            expand_in_expr(scrut, defs, def_lines, changed)?;
            for a in arms.iter_mut() {
                if let Some(g) = a.guard.as_mut() {
                    expand_in_expr(g, defs, def_lines, changed)?;
                }
                expand_in_expr(&mut a.body, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::While(c, b, e, _) => {
            expand_in_expr(c, defs, def_lines, changed)?;
            expand_in_block(b, defs, def_lines, changed)?;
            if let Some(e) = e.as_mut() {
                expand_in_block(e, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::For(_, it, b, _) => {
            expand_in_expr(it, defs, def_lines, changed)?;
            expand_in_block(b, defs, def_lines, changed)
        }
        Expression::Loop(b, _) => expand_in_block(b, defs, def_lines, changed),
        Expression::Break(_, v, _) => {
            if let Some(v) = v.as_mut() {
                expand_in_expr(v, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::Continue(..) => Ok(()),
        Expression::Return(v, _) => {
            if let Some(v) = v.as_mut() {
                expand_in_expr(v, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::Defer(e, _) => expand_in_expr(e, defs, def_lines, changed),
        Expression::Try(e, _, b, _) => {
            expand_in_expr(e, defs, def_lines, changed)?;
            expand_in_block(b, defs, def_lines, changed)
        }
        Expression::Throw(e, _) => expand_in_expr(e, defs, def_lines, changed),
        Expression::Array(items, count, _) => {
            for i in items.iter_mut() {
                expand_in_expr(i, defs, def_lines, changed)?;
            }
            if let Some(c) = count.as_mut() {
                expand_in_expr(c, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::Tuple(items, _) => {
            for i in items.iter_mut() {
                expand_in_expr(i, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::StructInit(_, fields, base, _) => {
            for (_, v) in fields.iter_mut() {
                expand_in_expr(v, defs, def_lines, changed)?;
            }
            if let Some(b) = base.as_mut() {
                expand_in_expr(b, defs, def_lines, changed)?;
            }
            Ok(())
        }
        Expression::Lambda(_, _, b, _) => expand_in_block(b, defs, def_lines, changed),
        Expression::Comptime(e, _) => expand_in_expr(e, defs, def_lines, changed),
        Expression::Await(e, _) => expand_in_expr(e, defs, def_lines, changed),
        Expression::Asm(_, _) => Ok(()),
        Expression::Yield(e, _) => expand_in_expr(e, defs, def_lines, changed),
        Expression::Spawn(e, _) => expand_in_expr(e, defs, def_lines, changed),
        Expression::Labeled(_, e, _) => expand_in_expr(e, defs, def_lines, changed),
        Expression::Goto(_, _) => Ok(()),
        Expression::Intrinsic(_, _, ops, _) => {
            for o in ops.iter_mut() {
                expand_in_expr(o, defs, def_lines, changed)?;
            }
            Ok(())
        }
    }
}

/// 展开一次宏调用：匹配臂 -> 替换元变量 -> 重新解析。
fn expand_call(
    name: &str,
    args: &[TokenKind],
    defs: &[(String, Vec<Arm>)],
    def_lines: &[(String, usize)],
    span: Span,
) -> Result<Expression, CompileError> {
    let Some((_, arms)) = defs.iter().find(|(n, _)| n == name) else {
        return Err(CompileError::new(format!(
            "cannot find macro `{name}!` in this scope"
        )));
    };
    let def_line = def_lines
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, l)| *l)
        .unwrap_or(0);

    let mut last_err: Option<String> = None;
    for arm in arms {
        match match_arm(arm, args) {
            Ok(bindings) => {
                let tokens = substitute(&arm.body, &bindings);
                return parse_expansion(&tokens, span);
            }
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(CompileError::new(format!(
            "no rule matches macro invocation `{name}!`: {e} (macro defined at line {})",
            def_line
        ))),
        None => Err(CompileError::new(format!(
            "no rule matches macro invocation `{name}!` (macro defined at line {def_line})"
        ))),
    }
}

/// 模式里是否有元变量（`$name:kind`）。
fn pattern_has_meta(pat: &[TokenKind]) -> bool {
    pat.windows(2).any(|w| w[0] == TokenKind::Dollar)
}

/// 用一条臂去匹配实参。成功返回绑定表。
fn match_arm(arm: &Arm, args: &[TokenKind]) -> Result<Bindings, String> {
    // 模式最外层通常是一对括号：`($x:expr)`。剥掉它再逐记号比对。
    let pat: &[TokenKind] = match arm.pattern.as_slice() {
        [TokenKind::LParen, rest @ ..] => match rest.split_last() {
            Some((TokenKind::RParen, head)) => head,
            _ => rest,
        },
        _ => &arm.pattern,
    };

    // 模式里没有元变量：要求实参记号数与模式一致。
    if !pattern_has_meta(pat) {
        if pat.len() != args.len() {
            return Err("arity mismatch".to_string());
        }
        for (p, a) in pat.iter().zip(args.iter()) {
            if p != a {
                return Err("token mismatch".to_string());
            }
        }
        return Ok(Vec::new());
    }

    // 有元变量：按模式里的 `$name:kind` 逐个切分实参。
    //
    // Rust 的片段规则：
    // - 元变量后面**跟着分隔符**（模式里的 `,`）时，它贪心地吃到实参里
    //   下一个同层 `,` 之前（不含分隔符）；
    // - 元变量后面**没有分隔符**（模式的最后一个片段）时，它吃掉实参的
    //   剩余部分。
    // 这样 `($a:expr, $b:expr)` 才能把 `f(1, 2)` 切成 `1` 和 `2`——
    // 早先按"单记号"匹配，`$b` 拿到的是空片段。
    let mut bindings: Bindings = Vec::new();
    let mut arg_i = 0usize;
    let mut pat_i = 0usize;
    while pat_i < pat.len() {
        // 遇到 `$` 就是元变量：形如 `$` `name` `:` `kind`。
        if pat[pat_i] == TokenKind::Dollar {
            let Some(TokenKind::Ident(name)) = pat.get(pat_i + 1) else {
                return Err("expected metavariable name after '$'".to_string());
            };
            let kind_name = match pat.get(pat_i + 3) {
                Some(TokenKind::Ident(k)) => k.clone(),
                _ => return Err(format!("expected fragment kind after `${name}:`")),
            };
            let kind = FragKind::from_name(&kind_name)
                .ok_or_else(|| format!("unknown fragment kind `{kind_name}`"))?;

            // 元变量后面是不是跟着分隔符？
            let followed_by_sep = pat.get(pat_i + 4) == Some(&TokenKind::Comma);

            let start = arg_i;
            if followed_by_sep {
                // 贪心吃到实参里下一个同层 `,` 之前。
                let mut end = args.len();
                let mut depth = 0i32;
                for (k, t) in args.iter().enumerate().skip(arg_i) {
                    match t {
                        TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
                        TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => depth -= 1,
                        TokenKind::Comma if depth == 0 => {
                            end = k;
                            break;
                        }
                        _ => {}
                    }
                }
                if end <= arg_i {
                    return Err(format!("missing argument for `${name}`"));
                }
                arg_i = end;
            } else {
                // 模式的最后一个片段：吃掉实参剩余部分。
                if arg_i >= args.len() {
                    return Err(format!("missing argument for `${name}`"));
                }
                arg_i = args.len();
            }
            let captured: Vec<TokenKind> = args[start..arg_i].to_vec();
            // 校验片段类别。
            validate_fragment(kind, &captured, name)?;
            bindings.push((name.clone(), captured));
            pat_i += 4;
            // 跳过分隔符（模式里的 `,` 要在实参里也出现一个）。
            if pat.get(pat_i) == Some(&TokenKind::Comma) {
                if args.get(arg_i) != Some(&TokenKind::Comma) {
                    return Err("expected ',' in macro arguments".to_string());
                }
                arg_i += 1;
                pat_i += 1;
            }
            continue;
        }
        // 普通记号：必须与实参当前位置一致。
        let Some(a) = args.get(arg_i) else {
            return Err("not enough arguments".to_string());
        };
        if pat[pat_i] != *a {
            return Err("token mismatch".to_string());
        }
        arg_i += 1;
        pat_i += 1;
    }
    // 实参不能有剩余（除非模式以 `,` 结尾，允许尾随逗号）。
    let trailing_comma = matches!(pat.last(), Some(TokenKind::Comma));
    if !trailing_comma && arg_i < args.len() {
        return Err("too many arguments".to_string());
    }
    Ok(bindings)
}

/// 校验捕获到的记号是否符合片段类别。
fn validate_fragment(kind: FragKind, toks: &[TokenKind], name: &str) -> Result<(), String> {
    match kind {
        FragKind::Tt => Ok(()),
        FragKind::Ident => match toks {
            [TokenKind::Ident(_)] => Ok(()),
            _ => Err(format!("`${name}` must be an identifier")),
        },
        FragKind::Expr => {
            if toks.is_empty() {
                return Err(format!("`${name}` must be an expression"));
            }
            // 用"能不能解析成一个表达式"来校验。
            parse_expansion(toks, Span::default())
                .map(|_| ())
                .map_err(|_| format!("`${name}` must be an expression"))
        }
        FragKind::Ty => {
            if toks.is_empty() {
                return Err(format!("`${name}` must be a type"));
            }
            parse_type_only(toks)
                .map(|_| ())
                .map_err(|_| format!("`${name}` must be a type"))
        }
    }
}

/// 校验一段记号能否解析成类型。
fn parse_type_only(toks: &[TokenKind]) -> Result<(), CompileError> {
    // 借用解析器把这段记号当成一个类型解析。
    let mut parser = parser_for_tokens(toks);
    parser.try_parse_type().map(|_| ()).ok_or_else(|| {
        CompileError::new("expected a type".to_string())
    })
}


/// 按绑定表把替换体里的 `$name` 换成捕获到的记号。
///
/// 未被绑定的 `$name` 原样保留（可能是字面美元号，罕见）。
fn substitute(body: &[TokenKind], bindings: &Bindings) -> Vec<TokenKind> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0usize;
    while i < body.len() {
        if body[i] == TokenKind::Dollar {
            if let Some(TokenKind::Ident(name)) = body.get(i + 1) {
                if let Some((_, toks)) = bindings.iter().find(|(n, _)| n == name) {
                    out.extend(toks.iter().cloned());
                    i += 2;
                    continue;
                }
            }
        }
        out.push(body[i].clone());
        i += 1;
    }
    out
}

/// 把记号序列重新词法分析 + 语法分析，取出唯一的表达式。
fn parse_expansion(toks: &[TokenKind], span: Span) -> Result<Expression, CompileError> {
    if toks.is_empty() {
        return Err(CompileError::new(
            "macro expansion produced no expression".to_string(),
        ));
    }
    // 记号 -> 源码文本：借用词法器的 `TokenKind` 显示实现不行（标识符
    // 的文本在 kind 里，但拼回源码需要知道具体写法）。这里改用
    // "把 kind 反向渲染成 token 文本"的方式。
    let src = render_tokens(toks);
    let tokens = Lexer::new(&src).tokenize();
    let mut parser = Parser::new(tokens);
    let expr = parser.parse_expression().map_err(|e| {
        CompileError::new(format!(
            "error expanding macro at line {}: {e}",
            span.line
        ))
    })?;
    Ok(expr)
}

/// 建一个只喂了这些记号的解析器（用于类型片段校验）。
fn parser_for_tokens(toks: &[TokenKind]) -> Parser {
    let src = render_tokens(toks);
    let tokens = Lexer::new(&src).tokenize();
    Parser::new(tokens)
}

/// 把记号序列渲染回源码文本。
///
/// 记号本身不携带原始拼写（`TokenKind::Ident(String)` 带文本，但
/// `TokenKind::Plus` 之类只有类别），这里按类别生成规范写法，再由
/// 词法器读回去。生成的文本对**所有**记号类别都必须是合法输入。
fn render_tokens(toks: &[TokenKind]) -> String {
    let mut out = String::new();
    for t in toks {
        out.push_str(&render_token(t));
        out.push(' ');
    }
    out
}

/// 单个记号的规范文本。
fn render_token(t: &TokenKind) -> String {
    match t {
        TokenKind::Eof => String::new(),
        TokenKind::LineComment(s) | TokenKind::BlockComment(s) => {
            let _ = s;
            String::new()
        }
        TokenKind::IntLiteral(v, suffix) => match suffix {
            Some(s) => format!("{v}{s}"),
            None => format!("{v}"),
        },
        TokenKind::FloatLiteral(v, suffix) => match suffix {
            Some(s) => format!("{v}{s}"),
            None => format!("{v}"),
        },
        TokenKind::StringLiteral(s) => {
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        TokenKind::CharLiteral(c) => format!("'{c}'"),
        TokenKind::Ident(s) => s.clone(),
        TokenKind::Underscore => "_".to_string(),
        TokenKind::Dollar => "$".to_string(),
        TokenKind::Lifetime(_) => "'a".to_string(),
        other => keyword_or_symbol(other).to_string(),
    }
}
/// 记号类别 -> 规范拼写。
fn keyword_or_symbol(t: &TokenKind) -> &'static str {
    match t {
        TokenKind::KwFn => "fn",
        TokenKind::KwLet => "let",
        TokenKind::KwConst => "const",
        TokenKind::KwMut => "mut",
        TokenKind::KwIf => "if",
        TokenKind::KwElse => "else",
        TokenKind::KwMatch => "match",
        TokenKind::KwWhile => "while",
        TokenKind::KwFor => "for",
        TokenKind::KwLoop => "loop",
        TokenKind::KwBreak => "break",
        TokenKind::KwContinue => "continue",
        TokenKind::KwReturn => "return",
        TokenKind::KwDefer => "defer",
        TokenKind::KwTry => "try",
        TokenKind::KwCatch => "catch",
        TokenKind::KwThrow => "throw",
        TokenKind::KwAsync => "async",
        TokenKind::KwAwait => "await",
        TokenKind::KwYield => "yield",
        TokenKind::Lifetime(_) => "'a",
        TokenKind::KwSpawn => "spawn",
        TokenKind::KwComptime => "comptime",
        TokenKind::KwDefault => "default",
        TokenKind::KwGoto => "goto",
        TokenKind::KwStruct => "struct",
        TokenKind::KwEnum => "enum",
        TokenKind::KwUnion => "union",
        TokenKind::KwTrait => "trait",
        TokenKind::KwImpl => "impl",
        TokenKind::KwType => "type",
        TokenKind::KwWhere => "where",
        TokenKind::KwPub => "pub",
        TokenKind::KwMod => "mod",
        TokenKind::KwUse => "use",
        TokenKind::KwAs => "as",
        TokenKind::KwSelf => "self",
        TokenKind::KwUpperSelf => "Self",
        TokenKind::KwSuper => "super",
        TokenKind::KwTrue => "true",
        TokenKind::KwFalse => "false",
        TokenKind::KwVoid => "void",
        TokenKind::KwAnytype => "anytype",
        TokenKind::KwAuto => "auto",
        TokenKind::KwIn => "in",
        TokenKind::KwIs => "is",
        TokenKind::KwRef => "ref",
        TokenKind::KwStatic => "static",
        TokenKind::KwExtern => "extern",
        TokenKind::KwCrate => "crate",
        TokenKind::TyI(_) => "i32",
        TokenKind::TyU(_) => "u32",
        TokenKind::TyF(_) => "f32",
        TokenKind::TyUsize => "usize",
        TokenKind::TyIsize => "isize",
        TokenKind::TyBool => "bool",
        TokenKind::TyStr => "str",
        TokenKind::TyChar => "char",
        TokenKind::TyNever => "!",
        TokenKind::KwAnyerror => "anyerror",
        TokenKind::Plus => "+",
        TokenKind::Minus => "-",
        TokenKind::Star => "*",
        TokenKind::Slash => "/",
        TokenKind::Percent => "%",
        TokenKind::Eq => "=",
        TokenKind::EqEq => "==",
        TokenKind::BangEq => "!=",
        TokenKind::Lt => "<",
        TokenKind::Gt => ">",
        TokenKind::Le => "<=",
        TokenKind::Ge => ">=",
        TokenKind::Bang => "!",
        TokenKind::Amp => "&",
        TokenKind::Pipe => "|",
        TokenKind::Caret => "^",
        TokenKind::Tilde => "~",
        TokenKind::Shl => "<<",
        TokenKind::Shr => ">>",
        TokenKind::AmpAmp => "&&",
        TokenKind::PipePipe => "||",
        TokenKind::DotDot => "..",
        TokenKind::DotDotEq => "..=",
        TokenKind::Rocket => "=>",
        TokenKind::Arrow => "->",
        TokenKind::ColonColon => "::",
        TokenKind::ColonEq => ":=",
        TokenKind::PlusEq => "+=",
        TokenKind::MinusEq => "-=",
        TokenKind::StarEq => "*=",
        TokenKind::SlashEq => "/=",
        TokenKind::PercentEq => "%=",
        TokenKind::AmpEq => "&=",
        TokenKind::PipeEq => "|=",
        TokenKind::CaretEq => "^=",
        TokenKind::ShlEq => "<<=",
        TokenKind::ShrEq => ">>=",
        TokenKind::PlusPlus => "++",
        TokenKind::HashHash => "##",
        TokenKind::LParen => "(",
        TokenKind::RParen => ")",
        TokenKind::LBrace => "{",
        TokenKind::RBrace => "}",
        TokenKind::LBracket => "[",
        TokenKind::RBracket => "]",
        TokenKind::Comma => ",",
        TokenKind::Semicolon => ";",
        TokenKind::Dollar => "$",
        TokenKind::Colon => ":",
        TokenKind::Underscore => "_",
        TokenKind::Dot => ".",
        TokenKind::Question => "?",
        TokenKind::At => "@",
        TokenKind::Hash => "#",
        TokenKind::LexError(_) => "",
        TokenKind::Eof
        | TokenKind::LineComment(_)
        | TokenKind::BlockComment(_)
        | TokenKind::IntLiteral(..)
        | TokenKind::FloatLiteral(..)
        | TokenKind::StringLiteral(_)
        | TokenKind::CharLiteral(_)
        | TokenKind::Ident(_) => "",
    }
}