//! 属性系统：统一查询接口与语义处理。
//!
//! 属性以 `#[name(args)]` 形式出现在条目上，编译器各阶段通过本模块
//! 提供的查询函数读取属性，避免直接遍历属性列表。
//!
//! 支持的属性：
//! - `#[inline]` / `#[inline(always)]` / `#[inline(never)]` — 内联提示
//! - `#[repr(C)]` / `#[repr(u8)]` / `#[repr(C, packed)]` — 内存布局
//! - `#[link(name = "...", kind = "...")]` — 链接指令
//! - `#[derive(Debug, Clone, Copy)]` — 自动 trait 派生
//! - `#[deprecated]` — 废弃警告
//! - `#[allow(...)]` / `#[deny(...)]` — lint 级别控制
//! - `#[cfg(...)]` — 条件编译

#![allow(dead_code)]

use crate::parser::ast::{self, Attribute, AttributeArg};

// ======================================================================
// 基本查询
// ======================================================================

/// 检查属性列表中是否存在指定属性。
pub fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.name == name)
}

/// 获取指定属性的第一个匹配项。
pub fn get_attr<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a Attribute> {
    attrs.iter().find(|a| a.name == name)
}

/// 检查属性是否带某个位置参数（`#[inline(always)]`）。
pub fn has_arg(attrs: &[Attribute], name: &str, arg: &str) -> bool {
    get_attr(attrs, name).is_some_and(|a| {
        a.args.iter().any(|arg_expr| match arg_expr {
            AttributeArg::Pos(ast::Expression::Variable(n, _)) => n == arg,
            _ => false,
        })
    })
}

/// 获取属性的具名参数值（`#[link(name = "c")]`）。
pub fn get_named_arg(attrs: &[Attribute], name: &str, key: &str) -> Option<String> {
    get_attr(attrs, name).and_then(|a| {
        a.args.iter().find_map(|arg| match arg {
            AttributeArg::NameValue(k, v) if k == key => Some(expr_to_string(v)),
            _ => None,
        })
    })
}

/// 把属性参数表达式渲染成字符串。
fn expr_to_string(e: &ast::Expression) -> String {
    match e {
        ast::Expression::Literal(ast::Literal::Int(v, _), _) => v.to_string(),
        ast::Expression::Literal(ast::Literal::Float(v, _), _) => v.to_string(),
        ast::Expression::Literal(ast::Literal::String(s), _) => s.clone(),
        ast::Expression::Literal(ast::Literal::Char(c), _) => c.to_string(),
        ast::Expression::Literal(ast::Literal::Bool(b), _) => b.to_string(),
        ast::Expression::Variable(name, _) => name.clone(),
        ast::Expression::Path(segments, _) => segments.join("::"),
        _ => String::new(),
    }
}

// ======================================================================
// 内联策略
// ======================================================================

/// 内联提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InlineHint {
    /// 无提示（默认）。
    #[default]
    None,
    /// `#[inline]` — 建议内联。
    Inline,
    /// `#[inline(always)]` — 强制内联（即使多块函数）。
    Always,
    /// `#[inline(never)]` — 禁止内联。
    Never,
}

/// 从属性列表提取内联策略。
pub fn inline_hint(attrs: &[Attribute]) -> InlineHint {
    if has_arg(attrs, "inline", "always") {
        InlineHint::Always
    } else if has_arg(attrs, "inline", "never") {
        InlineHint::Never
    } else if has_attr(attrs, "inline") {
        InlineHint::Inline
    } else {
        InlineHint::None
    }
}

// ======================================================================
// 链接指令
// ======================================================================

/// 链接指令（`#[link(name = "c", kind = "static")]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDirective {
    /// 库名。
    pub name: String,
    /// 链接类型（`"static"` / `"dynamic"`）。
    pub kind: String,
}

/// 从属性列表提取链接指令。
pub fn link_directive(attrs: &[Attribute]) -> Option<LinkDirective> {
    get_attr(attrs, "link")?;
    let name = get_named_arg(attrs, "link", "name").unwrap_or_default();
    let kind = get_named_arg(attrs, "link", "kind").unwrap_or_else(|| "dynamic".to_string());
    Some(LinkDirective { name, kind })
}

// ======================================================================
// 条件编译
// ======================================================================

/// `#[cfg(...)]` 条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgCondition {
    /// `cfg(feature = "foo")`。
    Feature(String),
    /// `cfg(target_os = "linux")`。
    TargetOs(String),
    /// `cfg(target_arch = "x86_64")`。
    TargetArch(String),
    /// `cfg(unix)` / `cfg(windows)` — 布尔标志。
    Flag(String),
    /// `cfg(not(...))`。
    Not(Box<CfgCondition>),
    /// `cfg(all(...))`。
    All(Vec<CfgCondition>),
    /// `cfg(any(...))`。
    Any(Vec<CfgCondition>),
}

/// 解析 `#[cfg(...)]` 条件。
pub fn parse_cfg(attrs: &[Attribute]) -> Option<CfgCondition> {
    let attr = get_attr(attrs, "cfg")?;
    let arg = attr.args.first()?;
    match arg {
        AttributeArg::Pos(ast::Expression::Variable(name, _)) => {
            Some(CfgCondition::Flag(name.clone()))
        }
        AttributeArg::NameValue(key, val) => {
            let v = expr_to_string(val);
            match key.as_str() {
                "feature" => Some(CfgCondition::Feature(v)),
                "target_os" => Some(CfgCondition::TargetOs(v)),
                "target_arch" => Some(CfgCondition::TargetArch(v)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// 评估 cfg 条件。
pub fn eval_cfg(
    cond: &CfgCondition,
    target_os: &str,
    target_arch: &str,
    features: &[String],
) -> bool {
    match cond {
        CfgCondition::Feature(f) => features.iter().any(|x| x == f),
        CfgCondition::TargetOs(os) => os == target_os,
        CfgCondition::TargetArch(arch) => arch == target_arch,
        CfgCondition::Flag(flag) => match flag.as_str() {
            "unix" => matches!(target_os, "linux" | "macos" | "freebsd"),
            "windows" => target_os == "windows",
            _ => false,
        },
        CfgCondition::Not(inner) => !eval_cfg(inner, target_os, target_arch, features),
        CfgCondition::All(conds) => conds
            .iter()
            .all(|c| eval_cfg(c, target_os, target_arch, features)),
        CfgCondition::Any(conds) => conds
            .iter()
            .any(|c| eval_cfg(c, target_os, target_arch, features)),
    }
}

/// 检查条目是否应该被 cfg 过滤掉。
pub fn is_cfg_enabled(
    attrs: &[Attribute],
    target_os: &str,
    target_arch: &str,
    features: &[String],
) -> bool {
    match parse_cfg(attrs) {
        Some(cond) => eval_cfg(&cond, target_os, target_arch, features),
        None => true,
    }
}

// ======================================================================
// Lint 级别
// ======================================================================

/// lint 级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LintLevel {
    /// `#[allow(...)]` — 允许（不警告）。
    Allow,
    /// 默认级别。
    Warn,
    /// `#[deny(...)]` — 拒绝（报错）。
    Deny,
}

/// 从属性列表提取 lint 级别。
pub fn lint_level(attrs: &[Attribute], lint_name: &str) -> LintLevel {
    for attr in attrs {
        match attr.name.as_str() {
            "allow" if attr.args.iter().any(|a| attr_matches_lint(a, lint_name)) => {
                return LintLevel::Allow;
            }
            "deny" if attr.args.iter().any(|a| attr_matches_lint(a, lint_name)) => {
                return LintLevel::Deny;
            }
            _ => {}
        }
    }
    LintLevel::Warn
}

fn attr_matches_lint(arg: &AttributeArg, lint_name: &str) -> bool {
    match arg {
        AttributeArg::Pos(ast::Expression::Variable(n, _)) => n == lint_name,
        _ => false,
    }
}

// ======================================================================
// 废弃
// ======================================================================

/// 检查属性是否标记为 deprecated。
pub fn is_deprecated(attrs: &[Attribute]) -> bool {
    has_attr(attrs, "deprecated")
}

/// 获取 deprecated 消息。
pub fn deprecated_message(attrs: &[Attribute]) -> Option<String> {
    get_named_arg(attrs, "deprecated", "message")
}

// ======================================================================
// 派生
// ======================================================================

/// 从 derive 属性提取 trait 名列表。
pub fn derive_traits(attrs: &[Attribute]) -> Vec<String> {
    get_attr(attrs, "derive")
        .map(|a| {
            a.args
                .iter()
                .filter_map(|arg| match arg {
                    AttributeArg::Pos(ast::Expression::Variable(n, _)) => Some(n.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}
