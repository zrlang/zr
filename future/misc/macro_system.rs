//! Macro system (declarative + procedural)
//!
//! Two macro flavors are planned:
//!
//! 1. **Declarative macros** (`macro_rules!`) — pattern-based, hygienic,
//!    similar to Rust's. These operate on token trees.
//!
//! 2. **Procedural macros** (`#[derive]`, `#[attribute]`, `function_like!`) —
//!    user-written compiler plugins that run at compile time and manipulate
//!    the AST. These require a stable plugin ABI and a sandbox.
//!
//! ## Status
//! ⬜ Not started. The parser does not yet expose token trees.

#![allow(dead_code)]

pub enum TokenTree {
    Token(crate::lexer::Token),
    Delimited(Delim, Vec<TokenTree>),
}

pub enum Delim { Paren, Brace, Bracket }

/// A `macro_rules!` matcher compiled down to a state machine.
pub struct Matcher {
    pub patterns: Vec<Arm>,
}

pub struct Arm {
    pub lhs: Pattern,
    pub rhs: Expansion,
}

pub enum Pattern {
    /// A literal token.
    Lit(crate::lexer::TokenKind),
    /// A metavariable like `$e:expr`.
    MetaVar(String, FragmentSpec),
    /// A repetition (`$()*`).
    Repeat(Box<Pattern>, Quantifier),
    Seq(Vec<Pattern>),
}

pub enum FragmentSpec {
    Item, Block, Stmt, Expr, Pat, Ty, Ident, Path,
    Meta, Tt, Vis, Lifetime, Literal,
}

pub enum Quantifier { ZeroOrMore, OneOrMore, ZeroOrOne }

/// Expansion side of a `macro_rules!` arm.
pub enum Expansion {
    Token(crate::lexer::TokenKind),
    MetaVar(String),
    Repeat(Box<Expansion>, Quantifier),
    Seq(Vec<Expansion>),
}

/// Hygiene: each identifier carries a `SyntaxContext` so that a macro-introduced
/// `x` does not collide with a user-introduced `x`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxContext(pub u32);

impl SyntaxContext {
    pub fn root() -> Self { SyntaxContext(0) }
}

/// Expand a macro invocation given its matcher and arguments.
pub fn expand(m: &Matcher, input: &[TokenTree]) -> Result<Vec<TokenTree>, String> {
    for arm in &m.patterns {
        if let Ok(bindings) = match_arm(&arm.lhs, input) {
            return Ok(apply_expansion(&arm.rhs, &bindings));
        }
    }
    Err("no rule matches input".into())
}

fn match_arm(_lhs: &Pattern, _input: &[TokenTree]) -> Result<Bindings, ()> {
    // TODO: implement the backtracking matcher.
    Err(())
}

fn apply_expansion(_rhs: &Expansion, _b: &Bindings) -> Vec<TokenTree> {
    // TODO: substitute metavariables and splice repetitions.
    Vec::new()
}

#[derive(Default)]
struct Bindings {
    vars: std::collections::HashMap<String, Vec<TokenTree>>,
}
