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
//! - `#[cfg(...)]` — 条件编译（Rust 风格谓词）

#![allow(dead_code)]

use std::sync::{OnceLock, RwLock};

use crate::codegen::target::{Arch, Os, Target};
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

/// 目标平台信息：cfg 谓词求值所需的全部信息。
///
/// 字段与 Rust 的 cfg 键一一对应：`target_os` / `target_arch` /
/// `target_family` / `target_env` / `target_vendor` /
/// `target_pointer_width` / `target_endian`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfgTarget {
    /// 操作系统（`target_os`）：`linux` / `macos` / `windows` / `wasi` / …
    pub os: String,
    /// 架构（`target_arch`）：`x86_64` / `aarch64` / `wasm32` / …
    pub arch: String,
    /// 目标族（`target_family`）：`unix` / `windows` / `wasm`。
    pub family: String,
    /// 运行环境（`target_env`）：`gnu` / `musl` / `msvc` / 空串。
    pub env: String,
    /// 厂商（`target_vendor`）：`unknown` / `apple` / `pc`。
    pub vendor: String,
    /// 指针位宽（`target_pointer_width`）：32 或 64。
    pub pointer_width: u32,
    /// 字节序（`target_endian`）：`little` / `big`。
    pub endian: String,
    /// 启用的 feature（`feature = "..."` 与 `target_feature = "..."`）。
    pub features: Vec<String>,
}

impl CfgTarget {
    /// 宿主平台的 cfg 上下文（编译目标未指定时的缺省值）。
    pub fn host() -> Self {
        Self::from_target(&Target::host())
    }

    /// 由编译目标构造 cfg 上下文。
    ///
    /// 注意 `Os::Darwin` 在三元组里写作 `darwin`，而 Rust 的 `target_os`
    /// 是 `macos`——两者必须在这里对齐，否则 `#[cfg(target_os = "macos")]`
    /// 在 macOS 上会失效。
    pub fn from_target(target: &Target) -> Self {
        let os = target.os();
        Self {
            os: os_name(os).to_string(),
            arch: arch_name(target.arch()).to_string(),
            family: os_family(os).to_string(),
            env: target.env().to_string(),
            vendor: target.vendor().to_string(),
            pointer_width: u32::from(target.pointer_size()) * 8,
            endian: if target.is_little_endian() {
                "little"
            } else {
                "big"
            }
            .to_string(),
            features: arch_features(target.arch()).iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 裸标志是否成立：`unix` / `windows` / `wasm`。
    ///
    /// 裸标志与对应的键值对等价：`cfg(unix)` 就是 `cfg(target_family =
    /// "unix")`。
    pub fn has_flag(&self, name: &str) -> bool {
        match name {
            "unix" => self.family == "unix",
            "windows" => self.family == "windows",
            "wasm" => self.family == "wasm",
            _ => false,
        }
    }

    /// 键值对是否成立。
    ///
    /// 未知的键一律不成立（与 Rust 一致：未知键只会触发 `unexpected_cfgs`
    /// 警告，条件本身为假）。
    pub fn matches(&self, key: &str, value: &str) -> bool {
        match key {
            "target_os" => self.os == value,
            "target_arch" => self.arch == value,
            "target_family" => self.family == value,
            "target_env" => self.env == value,
            "target_vendor" => self.vendor == value,
            "target_pointer_width" => self.pointer_width.to_string() == value,
            "target_endian" => self.endian == value,
            "target_feature" | "feature" => self.features.iter().any(|f| f == value),
            _ => false,
        }
    }
}

/// 操作系统的 Rust 风格名字（`target_os`）。
fn os_name(os: Os) -> &'static str {
    match os {
        Os::Bare => "none",
        Os::Linux => "linux",
        // 三元组里叫 darwin，Rust 的 target_os 叫 macos。
        Os::Darwin => "macos",
        Os::Win => "windows",
        Os::Wasi => "wasi",
        Os::FreeBsd => "freebsd",
        Os::Android => "android",
        Os::Ios => "ios",
    }
}

/// 架构的 Rust 风格名字（`target_arch`）。
fn arch_name(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "x86_64",
        Arch::AArch64 => "aarch64",
        Arch::Wasm32 => "wasm32",
        Arch::Wasm64 => "wasm64",
        Arch::Arm32 => "arm",
        // Rust 的 target_arch 对 thumb 目标同样是 "arm"（thumb 是
        // target_feature）。
        Arch::Arm32Thumb => "arm",
        Arch::RiscV32 => "riscv32",
        Arch::RiscV64 => "riscv64",
    }
}

/// 目标族（`target_family`）。
fn os_family(os: Os) -> &'static str {
    match os {
        Os::Linux | Os::Darwin | Os::FreeBsd | Os::Android | Os::Ios => "unix",
        Os::Win => "windows",
        Os::Wasi => "wasm",
        // 裸机没有目标族。
        Os::Bare => "",
    }
}

/// 架构默认开启的 CPU 特性（`target_feature`）。
fn arch_features(arch: Arch) -> &'static [&'static str] {
    match arch {
        // x86_64 的基线指令集包含 SSE2，aarch64 的基线包含 NEON。
        Arch::X86_64 => &["sse2"],
        Arch::AArch64 => &["neon"],
        _ => &[],
    }
}

/// 进程级的当前编译目标。
///
/// cfg 谓词在**解析阶段**就要求值（被 cfg 掉的条目在进入 AST 之前就被剔除），
/// 而解析器在主文件与模块文件两处被创建——`modules.rs` 不方便逐个传参，
/// 所以目标信息放在这里，由 `main.rs` 在解析之前设置一次。
static GLOBAL_TARGET: OnceLock<RwLock<CfgTarget>> = OnceLock::new();

/// 全局目标的存放位置（首次访问时以宿主平台初始化）。
fn global_target_slot() -> &'static RwLock<CfgTarget> {
    GLOBAL_TARGET.get_or_init(|| RwLock::new(CfgTarget::from_target(&Target::host())))
}

/// 设置进程级的编译目标（必须在解析源码之前调用）。
pub fn set_global_target(target: CfgTarget) {
    match global_target_slot().write() {
        Ok(mut slot) => *slot = target,
        // 锁中毒（其它线程 panic）时退回宿主平台：cfg 求值失败不该让
        // 整个编译器挂掉。
        Err(_) => {}
    }
}

/// 当前的编译目标（缺省为宿主平台）。
pub fn global_target() -> CfgTarget {
    match global_target_slot().read() {
        Ok(slot) => slot.clone(),
        Err(_) => CfgTarget::from_target(&Target::host()),
    }
}

/// `#[cfg(...)]` 条件：Rust 风格的谓词树。
///
/// 支持的形态：
/// - `cfg(unix)` —— 裸标志；
/// - `cfg(target_os = "linux")` —— 键值对；
/// - `cfg(not(..))` / `cfg(all(..))` / `cfg(any(..))` —— 嵌套谓词。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgCondition {
    /// `cfg(all(..))` —— 全部成立。
    All(Vec<CfgCondition>),
    /// `cfg(any(..))` —— 任一成立。
    Any(Vec<CfgCondition>),
    /// `cfg(not(..))` —— 取反。
    Not(Box<CfgCondition>),
    /// `cfg(unix)` —— 裸标志。
    Flag(String),
    /// `cfg(target_os = "linux")` —— 键值对。
    KeyValue(String, String),
}

/// 解析 `#[cfg(...)]` 条件。
///
/// 属性参数里的谓词树由解析器编码成表达式（见 `parser::mod`），这里再把它
/// 还原成结构化的条件。多个谓词并列（`cfg(a, b)`）等价于 `cfg(all(a, b))`。
pub fn parse_cfg(attrs: &[Attribute]) -> Option<CfgCondition> {
    let attr = get_attr(attrs, "cfg")?;
    let conds: Vec<CfgCondition> = attr.args.iter().filter_map(attr_arg_to_cond).collect();
    match conds.len() {
        0 => None,
        1 => conds.into_iter().next(),
        _ => Some(CfgCondition::All(conds)),
    }
}

/// 属性参数 → cfg 条件。
fn attr_arg_to_cond(arg: &AttributeArg) -> Option<CfgCondition> {
    match arg {
        AttributeArg::Pos(expr) => expr_to_cond(expr),
        AttributeArg::NameValue(key, value) => {
            Some(CfgCondition::KeyValue(key.clone(), literal_str(value)?.to_string()))
        }
    }
}

/// 嵌套实参 → cfg 条件。
///
/// 嵌套谓词里的 `key = "value"` 复用 `Argument::Named` 承载（键 + 值），
/// 位置参数则是一个子谓词。
fn arg_to_cond(arg: &ast::Argument) -> Option<CfgCondition> {
    match arg {
        ast::Argument::Pos(expr) => expr_to_cond(expr),
        ast::Argument::Named(key, value) => {
            Some(CfgCondition::KeyValue(key.clone(), literal_str(value)?.to_string()))
        }
    }
}

/// 表达式 → cfg 条件。
fn expr_to_cond(expr: &ast::Expression) -> Option<CfgCondition> {
    match expr {
        // 裸标志：`unix`
        ast::Expression::Variable(name, _) => Some(CfgCondition::Flag(name.clone())),
        // 嵌套谓词：`not(..)` / `all(..)` / `any(..)`
        ast::Expression::Call(callee, args, _) => {
            let ast::Expression::Variable(op, _) = callee.as_ref() else {
                return None;
            };
            let conds: Vec<CfgCondition> = args.iter().filter_map(arg_to_cond).collect();
            match op.as_str() {
                "all" => Some(CfgCondition::All(conds)),
                "any" => Some(CfgCondition::Any(conds)),
                // `not` 只接受一个谓词。
                "not" => conds
                    .into_iter()
                    .next()
                    .map(|c| CfgCondition::Not(Box::new(c))),
                _ => None,
            }
        }
        _ => None,
    }
}

/// 取字符串字面量的内容。
fn literal_str(e: &ast::Expression) -> Option<&str> {
    match e {
        ast::Expression::Literal(ast::Literal::String(s), _) => Some(s),
        _ => None,
    }
}

/// 评估 cfg 条件。
pub fn eval_cfg(cond: &CfgCondition, target: &CfgTarget) -> bool {
    match cond {
        CfgCondition::All(conds) => conds.iter().all(|c| eval_cfg(c, target)),
        CfgCondition::Any(conds) => conds.iter().any(|c| eval_cfg(c, target)),
        CfgCondition::Not(inner) => !eval_cfg(inner, target),
        CfgCondition::Flag(flag) => target.has_flag(flag),
        CfgCondition::KeyValue(key, value) => target.matches(key, value),
    }
}

/// 检查条目是否应该被 cfg 过滤掉。
///
/// 没有 `#[cfg]` 属性的条目一律保留；条件不成立的条目应当**完全不存在**
/// （Rust 的语义）：它不会进入符号表，引用它会报 "cannot find value"。
pub fn is_cfg_enabled(attrs: &[Attribute], target: &CfgTarget) -> bool {
    match parse_cfg(attrs) {
        Some(cond) => eval_cfg(&cond, target),
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
///
/// 两种写法都认：`#[deprecated = "..."]`（Rust 的写法，值按属性名本身作
/// 为键存放）与 `#[deprecated(message = "...")]`。
pub fn deprecated_message(attrs: &[Attribute]) -> Option<String> {
    get_named_arg(attrs, "deprecated", "message")
        .or_else(|| get_named_arg(attrs, "deprecated", "deprecated"))
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
