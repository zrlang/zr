//! SSA 模块的可读文本表示（调试用）。

use super::SsaModule;

/// 打印整个 SSA 模块。
pub fn print_module(module: &SsaModule) {
    for func in &module.functions {
        eprintln!("fn {}(", func.name);
        eprintln!(") {{");
        for block in &func.blocks {
            eprintln!("  bb{}:", block.id);
            for instr in &block.instrs {
                eprintln!("    {:?}", instr);
            }
        }
    }
}
