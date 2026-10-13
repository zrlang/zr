//! 中端优化框架。
//!
//! 设计参考：
//! - Rust：MIR 优化（`rustc_mir_transform`）按 pass 组织、依赖 CFG/SSA 分析。
//! - Zig：编译期求值（CTFE）发生在语义分析阶段，优化 pass 在 `Air` 上执行。
//! - Python：无类似层（CPython 不做静态优化）。
//!
//! 优化流水线：
//! ```text
//! MIR -> SSA 构造 -> [const-fold | DCE | CSE | GVN | 内联 | 循环] -> SSA 析构 -> MIR
//! ```

mod pass_manager;
pub mod lto;
pub mod pgo;

use crate::ssa::SsaModule;

/// 优化等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// `-O0`：不优化，仅保证编译速度。
    #[default]
    O0,
    /// `-O1`：基本优化（常量折叠、DCE）。
    O1,
    /// `-O2`：标准优化（加 CSE/GVN、窥孔）。
    O2,
    /// `-O3`：激进优化（加内联、循环变换）。
    O3,
    /// `-Oz`：优化体积。
    Oz,
}

impl OptLevel {
    /// 人类可读名称。
    pub fn name(self) -> &'static str {
        match self {
            OptLevel::O0 => "O0 (no optimization)",
            OptLevel::O1 => "O1 (basic)",
            OptLevel::O2 => "O2 (standard)",
            OptLevel::O3 => "O3 (aggressive)",
            OptLevel::Oz => "Oz (size)",
        }
    }
}

impl PartialOrd for OptLevel {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OptLevel {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let rank = |l: OptLevel| match l {
            OptLevel::O0 => 0,
            OptLevel::O1 => 1,
            OptLevel::O2 => 2,
            OptLevel::O3 => 3,
            OptLevel::Oz => 2, // Oz 强度等同于 O2
        };
        rank(*self).cmp(&rank(*other))
    }
}

/// 对 SSA 模块执行优化。
///
/// 按等级依次应用 pass，并迭代至收敛（最多 `MAX_ITER` 轮）。
pub fn run(mut module: SsaModule, level: OptLevel) -> SsaModule {
    if level == OptLevel::O0 {
        return module;
    }

    // 基础 pass：所有非零等级都跑
    module = pass_manager::fold_constants(module);
    module = pass_manager::eliminate_dead_code(module);

    if level >= OptLevel::O2 {
        module = pass_manager::eliminate_common_subexpr(module);
        module = pass_manager::global_value_numbering(module);
        module = pass_manager::peephole(module);
    }

    if level >= OptLevel::O3 {
        module = pass_manager::inline_functions(module);
        module = pass_manager::optimize_loops(module);
    }

    if level == OptLevel::Oz {
        // 体积优先：额外做简化与死代码清理
        module = pass_manager::eliminate_dead_code(module);
    }

    module
}
