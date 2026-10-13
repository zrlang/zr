//! 宏系统：声明式宏 (`macro_rules!`)。
//!
//! 基于 token tree 的模式匹配与展开。

use crate::lexer::{Token, TokenKind};
use std::collections::HashMap;

/// token 树：单个 token 或带分隔符的 token 序列。
#[derive(Debug, Clone)]
pub enum TokenTree {
    Token(Token),
    Delimited(Delim, Vec<TokenTree>),
}

/// 分隔符类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delim {
    Paren,
    Brace,
    Bracket,
}

/// `macro_rules!` 的匹配器。
#[derive(Debug, Clone)]
pub struct Matcher {
    pub arms: Vec<Arm>,
}

/// 单个匹配分支：模式 → 展开。
#[derive(Debug, Clone)]
pub struct Arm {
    pub lhs: Pattern,
    pub rhs: Expansion,
}

/// 匹配模式。
#[derive(Debug, Clone)]
pub enum Pattern {
    /// 字面量 token。
    Lit(TokenKind),
    /// 元变量 `$name:frag`。
    MetaVar(String, FragmentSpec),
    /// 重复 `$(pat)*`。
    Repeat(Box<Pattern>, Quantifier),
    /// 序列。
    Seq(Vec<Pattern>),
}

/// 片段说明符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentSpec {
    Item,
    Block,
    Stmt,
    Expr,
    Pat,
    Ty,
    Ident,
    Path,
    Meta,
    Tt,
    Vis,
    Lifetime,
    Literal,
}

/// 量词。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantifier {
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

/// 展开侧。
#[derive(Debug, Clone)]
pub enum Expansion {
    Token(TokenKind),
    MetaVar(String),
    Repeat(Box<Expansion>, Quantifier),
    Seq(Vec<Expansion>),
}

/// 元变量绑定：名称 → 捕获的 token 树列表。
#[derive(Debug, Default, Clone)]
pub struct Bindings {
    pub vars: HashMap<String, Vec<TokenTree>>,
}

impl Bindings {
    pub fn new() -> Self {
        Self { vars: HashMap::new() }
    }

    pub fn insert(&mut self, name: String, trees: Vec<TokenTree>) {
        self.vars.insert(name, trees);
    }

    pub fn get(&self, name: &str) -> Option<&Vec<TokenTree>> {
        self.vars.get(name)
    }
}

/// 展开宏调用：尝试依次匹配每个分支。
pub fn expand(m: &Matcher, input: &[TokenTree]) -> Result<Vec<TokenTree>, String> {
    for arm in &m.arms {
        if let Ok(bindings) = match_arm(&arm.lhs, input) {
            return Ok(apply_expansion(&arm.rhs, &bindings));
        }
    }
    Err("no rule matches input".into())
}

/// 匹配一个分支模式。
pub fn match_arm(lhs: &Pattern, input: &[TokenTree]) -> Result<Bindings, ()> {
    let mut bindings = Bindings::new();
    let consumed = match_pattern(lhs, input, &mut bindings)?;
    if consumed != input.len() {
        return Err(());
    }
    Ok(bindings)
}

/// 匹配单个模式，返回消耗的 token 树数量。
fn match_pattern(
    pat: &Pattern,
    input: &[TokenTree],
    bindings: &mut Bindings,
) -> Result<usize, ()> {
    match pat {
        Pattern::Lit(kind) => {
            if let Some(TokenTree::Token(t)) = input.first() {
                if t.kind == *kind {
                    return Ok(1);
                }
            }
            Err(())
        }
        Pattern::MetaVar(name, spec) => {
            // 元变量匹配单个 token 树（简化实现）
            if let Some(tree) = input.first() {
                if matches_fragment(tree, *spec) {
                    bindings.insert(name.clone(), vec![tree.clone()]);
                    return Ok(1);
                }
            }
            Err(())
        }
        Pattern::Repeat(inner, q) => {
            let mut count = 0;
            let mut pos = 0;
            loop {
                match match_pattern(inner, &input[pos..], bindings) {
                    Ok(n) => {
                        pos += n;
                        count += 1;
                        if matches!(q, Quantifier::ZeroOrOne) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            match q {
                Quantifier::ZeroOrMore => Ok(pos),
                Quantifier::OneOrMore => {
                    if count > 0 { Ok(pos) } else { Err(()) }
                }
                Quantifier::ZeroOrOne => Ok(pos),
            }
        }
        Pattern::Seq(pats) => {
            let mut pos = 0;
            for p in pats {
                let n = match_pattern(p, &input[pos..], bindings)?;
                pos += n;
            }
            Ok(pos)
        }
    }
}

/// 判断 token 树是否匹配片段说明符。
fn matches_fragment(tree: &TokenTree, spec: FragmentSpec) -> bool {
    match spec {
        FragmentSpec::Tt => true,
        FragmentSpec::Ident => {
            matches!(tree, TokenTree::Token(t) if matches!(t.kind, TokenKind::Ident(_)))
        }
        FragmentSpec::Literal => {
            matches!(tree, TokenTree::Token(t) if t.is_literal())
        }
        FragmentSpec::Expr | FragmentSpec::Stmt | FragmentSpec::Item | FragmentSpec::Block
        | FragmentSpec::Pat | FragmentSpec::Ty | FragmentSpec::Path | FragmentSpec::Meta
        | FragmentSpec::Vis | FragmentSpec::Lifetime => {
            // 简化：接受单个 token 或带分隔符的组
            true
        }
    }
}

/// 应用展开，替换元变量。
pub fn apply_expansion(rhs: &Expansion, bindings: &Bindings) -> Vec<TokenTree> {
    match rhs {
        Expansion::Token(kind) => {
            vec![TokenTree::Token(Token::new(kind.clone(), crate::lexer::Span::unknown()))]
        }
        Expansion::MetaVar(name) => {
            bindings.get(name).cloned().unwrap_or_default()
        }
        Expansion::Repeat(_inner, _q) => {
            // 简化：重复展开需要额外的绑定状态，此处返回空
            Vec::new()
        }
        Expansion::Seq(parts) => {
            let mut out = Vec::new();
            for p in parts {
                out.extend(apply_expansion(p, bindings));
            }
            out
        }
    }
}
