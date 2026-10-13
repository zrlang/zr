//! 重定位（Relocation）处理。
//!
//! 在链接/代码生成阶段，把符号引用转换为可重定位的占位，
//! 最终由链接器在知道所有符号地址后回填。

use crate::codegen::{CompilationUnit, Relocation, RelocationKind, Symbol};

/// 应用所有重定位：把符号地址填入对应的代码/数据偏移。
///
/// 对可重定位目标文件，仅记录重定位条目，不回填；
/// 对可执行文件，由链接器解析所有符号后回填绝对地址或相对偏移。
pub fn apply(unit: &mut CompilationUnit) {
    // 建立符号名 → 地址的映射
    let sym_map: std::collections::HashMap<&str, usize> = unit
        .symbols
        .iter()
        .filter(|s| s.section != crate::codegen::SymbolSection::Undefined)
        .map(|s| (s.name.as_str(), s.offset))
        .collect();

    for reloc in &unit.relocations {
        let target = match sym_map.get(reloc.symbol.as_str()) {
            Some(&addr) => addr,
            None => {
                // 外部符号（如 libc）：保留重定位条目，由链接器处理
                continue;
            }
        };
        let value = (target as i64 + reloc.addend) as u64;
        match reloc.kind {
            RelocationKind::Abs64 => {
                let bytes = value.to_le_bytes();
                unit.code[reloc.offset..reloc.offset + 8].copy_from_slice(&bytes);
            }
            RelocationKind::PcRel32 => {
                // PC 相对：目标地址 - (指令地址 + 指令长度)
                let instr_addr = reloc.offset as i64 - 1; // call 的 imm32 位于 offset+1..+5
                let delta = (target as i64) - (instr_addr + 5);
                let imm = (delta as i32).to_le_bytes();
                unit.code[reloc.offset..reloc.offset + 4].copy_from_slice(&imm);
            }
            RelocationKind::Got64 | RelocationKind::Plt32 => {
                // 骨架：GOT/PLT 需由链接器分配条目
            }
        }
    }
}

/// 在符号表中查找符号地址。
pub fn resolve_symbol<'a>(unit: &'a CompilationUnit, name: &str) -> Option<&'a Symbol> {
    unit.symbols.iter().find(|s| s.name == name)
}
