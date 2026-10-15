//! 容器无关的目标文件工具：符号解析、重定位计算与名称修饰。
//!
//! ELF / Mach-O / PE-COFF 三种容器在"符号 → 地址"与"重定位 → 立即数"
//! 两件事上的语义是一致的（都遵循 `S + A - P`），差别只在字段编码方式
//! 与宽度。本模块把这部分抽出来共享，各容器写入器只负责排版头部。

use std::collections::HashMap;

use crate::codegen::{CompilationUnit, Relocation, RelocationKind, SymbolKind, SymbolSection, Symbol};

/// 按对齐要求向上取整（`align` 必须是 2 的幂）。
pub fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    (value + align - 1) & !(align - 1)
}

/// 重定位立即数字段的宽度（字节）。
///
/// x86-64 的 `call/jmp/jcc` 与 AArch64 的 `b/bl/b.cond` 都使用 4 字节
/// 字段（分别是 32 位位移与 26 位字面量）。
pub fn reloc_field_size(kind: RelocationKind) -> usize {
    match kind {
        // 除 PcRel32 外，其余变体的立即数都嵌在一条 4 字节指令内部
        // （ARM 的 [23:0]、AArch64 的 [25:0]、RISC-V J/B 型的分散字段），
        // 因此回填时都以整条指令为单位做"读-改-写"。
        RelocationKind::PcRel32
        | RelocationKind::Branch26
        | RelocationKind::Call26
        | RelocationKind::ArmBranch24
        | RelocationKind::RiscVJal
        | RelocationKind::RiscVBranch => 4,
    }
}

/// ELF 重定位类型号（按架构）。
pub fn elf_reloc_type(arch: crate::codegen::target::Arch, kind: RelocationKind) -> u32 {
    use crate::codegen::target::Arch;
    match (arch, kind) {
        (Arch::X86_64, RelocationKind::PcRel32) => 4, // R_X86_64_PLT32
        (Arch::AArch64, RelocationKind::Branch26) => 282, // R_AARCH64_JUMP26
        (Arch::AArch64, RelocationKind::Call26) => 283,   // R_AARCH64_CALL26
        (Arch::Arm32, RelocationKind::ArmBranch24) => 29, // R_ARM_JUMP24
        (Arch::RiscV32 | Arch::RiscV64, RelocationKind::RiscVJal) => 17, // R_RISCV_JAL
        (Arch::RiscV32 | Arch::RiscV64, RelocationKind::RiscVBranch) => 16, // R_RISCV_BRANCH
        _ => 0,
    }
}

/// COFF 重定位类型号（按架构）。
pub fn coff_reloc_type(arch: crate::codegen::target::Arch, kind: RelocationKind) -> u16 {
    use crate::codegen::target::Arch;
    match (arch, kind) {
        (Arch::X86_64, RelocationKind::PcRel32) => 0x0004, // IMAGE_REL_AMD64_REL32
        (Arch::AArch64, RelocationKind::Branch26) | (Arch::AArch64, RelocationKind::Call26) => {
            0x0003 // IMAGE_REL_ARM64_BRANCH26
        }
        _ => 0,
    }
}

/// Mach-O 重定位类型号（按架构）。
pub fn macho_reloc_type(arch: crate::codegen::target::Arch, kind: RelocationKind) -> u8 {
    use crate::codegen::target::Arch;
    match (arch, kind) {
        // X86_64_RELOC_BRANCH 与 ARM64_RELOC_BRANCH26 都是 2（`bl` 与 `b` 同理）。
        (Arch::X86_64, RelocationKind::PcRel32)
        | (Arch::AArch64, RelocationKind::Branch26)
        | (Arch::AArch64, RelocationKind::Call26)
        | (Arch::Arm32, RelocationKind::ArmBranch24)
        | (Arch::RiscV32, RelocationKind::RiscVJal)
        | (Arch::RiscV64, RelocationKind::RiscVJal)
        | (Arch::RiscV32, RelocationKind::RiscVBranch)
        | (Arch::RiscV64, RelocationKind::RiscVBranch) => 2,
        _ => 0,
    }
}

/// Mach-O 重定位项长度编码（2 = 4 字节）。
pub const MACH_O_R_LEN_4: u8 = 2;

/// 把符号名按目标平台约定修饰。
///
/// Mach-O 在汇编层对 C 符号统一加一个下划线（`main` → `_main`）；
/// ELF 与 COFF(x64/arm64) 直接使用原名。
pub fn mangle_symbol(is_macho: bool, name: &str) -> String {
    if is_macho && !name.starts_with('_') {
        format!("_{name}")
    } else {
        name.to_string()
    }
}

/// 符号表 → 地址表（`text_base + offset`）。
///
/// 外部（未定义）符号不在表内，交由平台链接器解析。
pub fn symbol_addresses(unit: &CompilationUnit, text_base: u64) -> HashMap<String, u64> {
    unit.symbols
        .iter()
        .filter(|s| s.section != SymbolSection::Undefined)
        .map(|s| (s.name.clone(), text_base + s.offset as u64))
        .collect()
}

/// 就地应用重定位（用于自行构造可执行文件的场景）。
///
/// 重定位语义为 `S + A - P`：
/// - `PcRel32`：立即数字段整体就是位移，x86-64 的 `call/jmp/jcc` 都如此；
/// - `Branch26`：AArch64 的 26 位字面量嵌在指令自身的 [25:0] 位里，
///   因此必须"读-改-写"，保留高 6 位操作码。
pub fn apply_relocations(unit: &mut CompilationUnit, text_base: u64) {
    let addrs = symbol_addresses(unit, text_base);

    for reloc in &unit.relocations {
        let Some(&target) = addrs.get(reloc.symbol.as_str()) else {
            // 外部符号（如运行时的 zr_* 函数）没有本地地址，保持
            // 立即数为 0，由平台链接器填充。
            continue;
        };

        let instr_va = text_base + reloc.instr_start as u64;
        let target = target as i64 + reloc.addend;

        match reloc.kind {
            RelocationKind::PcRel32 => {
                let next_ip = instr_va + reloc.instr_len as u64;
                let delta = target - next_ip as i64;
                let width = reloc_field_size(reloc.kind);
                let end = reloc.offset + width;
                if end <= unit.code.len() {
                    unit.code[reloc.offset..end].copy_from_slice(&(delta as i32).to_le_bytes());
                }
            }
            RelocationKind::ArmBranch24 => {
                // ARM：A32 执行分支时读到的 PC 是"当前指令地址 + 8"
                // （流水线预取），因此偏移基准是 instr_va + 8。
                // imm24 以 4 字节为单位编码在指令的 [23:0]。
                let delta = target - instr_va as i64 - 8;
                let imm = ((delta >> 2) as u32) & 0x00ff_ffff;
                let end = reloc.offset + reloc_field_size(reloc.kind);
                if end > unit.code.len() {
                    continue;
                }
                let word = u32::from_le_bytes(unit.code[reloc.offset..end].try_into().unwrap());
                let patched = (word & 0xff00_0000) | imm;
                unit.code[reloc.offset..end].copy_from_slice(&patched.to_le_bytes());
            }
            RelocationKind::RiscVJal => {
                // J 型（jal）：21 位半字偏移被拆成 4 段散落在指令里
                //   imm[20]   → [31]      imm[10:1] → [30:21]
                //   imm[11]   → [20]      imm[19:12]→ [19:12]
                let delta = target - instr_va as i64;
                let imm = (delta >> 1) as u32 & 0x1f_ffff;
                patch_word(&mut unit.code, reloc, |word| {
                    (word & 0x0007_ffff)                 // 保留 [18:0]（rd + opcode）
                        | (imm & 0x000f_f000)             // imm[19:12] 原位
                        | ((imm & 0x0000_0800) << 9)      // imm[11]    → [20]
                        | ((imm & 0x0000_07fe) << 20)     // imm[10:1]  → [30:21]
                        | ((imm & 0x0010_0000) << 11)     // imm[20]    → [31]
                });
            }
            RelocationKind::RiscVBranch => {
                // B 型（beq 等）：13 位半字偏移散落在指令里
                //   imm[12]  → [31]   imm[10:5] → [30:25]
                //   imm[4:1] → [11:8] imm[11]   → [7]
                let delta = target - instr_va as i64;
                let imm = (delta >> 1) as u32 & 0x1fff;
                patch_word(&mut unit.code, reloc, |word| {
                    (word & 0x0000_707f)                 // 保留 funct3 + opcode
                        | ((imm & 0x0000_1000) >> 4)     // imm[11]   → [7]
                        | ((imm & 0x0000_000f) << 7)     // imm[4:1]  → [11:8]
                        | ((imm & 0x0000_0fe0) << 20)    // imm[10:5] → [30:25]
                        | ((imm & 0x0000_1000) << 19)    // imm[12]   → [31]
                });
            }
            RelocationKind::Branch26 | RelocationKind::Call26 => {
                let delta = target - instr_va as i64;
                let imm = ((delta >> 2) as u32) & 0x03ff_ffff;
                let end = reloc.offset + reloc_field_size(reloc.kind);
                if end > unit.code.len() {
                    continue;
                }
                let word = u32::from_le_bytes(
                    unit.code[reloc.offset..end]
                        .try_into()
                        .expect("branch26 字段宽度为 4 字节"),
                );
                let patched = (word & 0xfc00_0000) | imm;
                unit.code[reloc.offset..end].copy_from_slice(&patched.to_le_bytes());
            }
        }
    }
}

/// 为重定位引用到的外部符号补充"未定义"条目。
///
/// 目标文件必须显式声明被引用的外部符号（`zr_exit`、`main` 等），
/// 否则平台链接器无法把重定位绑定到对应实现。ELF / Mach-O / COFF
/// 三种容器都用这一表示，只是外部符号的编码方式不同。
pub fn add_extern_symbols(unit: &mut CompilationUnit) {
    let mut missing: Vec<String> = unit
        .relocations
        .iter()
        .filter(|reloc| !unit.symbols.iter().any(|s| s.name == reloc.symbol))
        .map(|reloc| reloc.symbol.clone())
        .collect();
    missing.dedup();

    for name in missing {
        unit.symbols.push(Symbol {
            name,
            section: SymbolSection::Undefined,
            offset: 0,
            size: 0,
            global: true,
            kind: SymbolKind::Function,
        });
    }
}

/// 读-改-写一个 4 字节指令（用于立即数字段被拆散的架构）。
fn patch_word(code: &mut [u8], reloc: &Relocation, f: impl Fn(u32) -> u32) {
    let end = reloc.offset + reloc_field_size(reloc.kind);
    if end > code.len() {
        return;
    }
    let word = u32::from_le_bytes(code[reloc.offset..end].try_into().expect("字段宽度 4 字节"));
    code[reloc.offset..end].copy_from_slice(&f(word).to_le_bytes());
}

/// 生成固定宽度、定长分隔的字符串表（如 Mach-O 的 16 字节段名）。
pub fn fixed_name(name: &str) -> [u8; 16] {
    let mut buf = [0u8; 16];
    let bytes = name.as_bytes();
    let len = bytes.len().min(15);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::{Relocation, Symbol, SymbolKind};

    fn unit_with_one_call() -> (CompilationUnit, Vec<Relocation>) {
        // call rel32 占 5 字节，立即数位于偏移 1。
        let mut unit = CompilationUnit::new();
        unit.code = vec![0xe8, 0, 0, 0, 0];
        unit.symbols.push(Symbol {
            name: "main".to_string(),
            section: SymbolSection::Text,
            offset: 0,
            size: 4,
            global: true,
            kind: SymbolKind::Function,
        });
        let relocs = vec![Relocation::new(
            1,
            0,
            5,
            "main",
            RelocationKind::PcRel32,
        )];
        (unit, relocs)
    }

    #[test]
    fn pcrel32_uses_next_instruction_address() {
        let (mut unit, relocs) = unit_with_one_call();
        unit.relocations = relocs;
        apply_relocations(&mut unit, 0x400000);
        // main 在 0x400000，call 在 0x400000，下一条指令 0x400005，
        // 因此位移应为 -5。
        assert_eq!(i32::from_le_bytes(unit.code[1..5].try_into().unwrap()), -5);
    }

    #[test]
    fn branch26_counts_four_byte_units() {
        let mut unit = CompilationUnit::new();
        // 前 8 字节是 main 本身，偏移 8 处是待修正的 bl
        unit.code = vec![0x20, 0x00, 0x80, 0xd2, 0x20, 0x00, 0x80, 0xd2, 0x00, 0x00, 0x00, 0x94];
        unit.symbols.push(Symbol {
            name: "main".to_string(),
            section: SymbolSection::Text,
            offset: 0, // main 位于偏移 0，bl 位于偏移 8
            size: 8,
            global: true,
            kind: SymbolKind::Function,
        });
        unit.relocations.push(Relocation::new(
            8,
            8,
            4,
            "main",
            RelocationKind::Branch26,
        ));
        apply_relocations(&mut unit, 0x1000);
        let word = u32::from_le_bytes(unit.code[8..12].try_into().unwrap());
        assert_eq!(word >> 26, 0b100101, "bl 操作码应保留");
        // 向后跳 8 字节 = -2 条指令，按 26 位补码编码。
        assert_eq!(word & 0x03ff_ffff, 0x03ff_fffe, "imm26 = -2");
        assert_eq!(unit.code[..8], [0x20, 0x00, 0x80, 0xd2, 0x20, 0x00, 0x80, 0xd2], "main 未被改动");
    }

    #[test]
    fn external_symbols_are_left_to_the_linker() {
        let (mut unit, relocs) = unit_with_one_call();
        unit.relocations = vec![Relocation::new(1, 0, 5, "zr_exit", RelocationKind::PcRel32)];
        let _ = relocs;
        apply_relocations(&mut unit, 0x400000);
        assert_eq!(unit.code[1..5], [0, 0, 0, 0]);
    }

    #[test]
    fn mangling_follows_platform_convention() {
        assert_eq!(mangle_symbol(true, "main"), "_main");
        assert_eq!(mangle_symbol(true, "_start"), "_start");
        assert_eq!(mangle_symbol(false, "main"), "main");
    }

    #[test]
    fn field_sizes_and_types_are_distinct_per_arch() {
        assert_eq!(reloc_field_size(RelocationKind::PcRel32), 4);
        assert_eq!(reloc_field_size(RelocationKind::Branch26), 4);
        assert_eq!(
            coff_reloc_type(crate::codegen::target::Arch::AArch64, RelocationKind::Branch26),
            0x0003
        );
        assert_eq!(elf_reloc_type(crate::codegen::target::Arch::AArch64, RelocationKind::Call26), 283);
    }

    #[test]
    fn riscv_jal_relocation_scatters_immediate_correctly() {
        // jal ra, <向前 12 字节>：J 型立即数 -6（半字）要拆成 4 段
        let mut unit = CompilationUnit::new();
        let mut code = vec![0u8; 12];
        code.extend_from_slice(&0x0000_00efu32.to_le_bytes()); // jal ra, imm=0
        unit.code = code;
        unit.symbols.push(Symbol {
            name: "main".to_string(),
            section: SymbolSection::Text,
            offset: 0,
            size: 0,
            global: true,
            kind: SymbolKind::Function,
        });
        unit.relocations.push(Relocation::new(12, 12, 4, "main", RelocationKind::RiscVJal));
        apply_relocations(&mut unit, 0x1000);
        let word = u32::from_le_bytes(unit.code[12..16].try_into().unwrap());
        // imm = -6（0x1FFFFA）→ imm[20]=1, imm[10:1]=0x3FD, imm[11]=1,
        // imm[19:12]=0xFF，目的寄存器 ra=1
        assert_eq!(word, 0xffbf_f0ef);
        assert_eq!(word & 0xfff, 0x0ef, "rd 与 opcode 必须保留");
    }

    #[test]
    fn riscv_branch_relocation_scatters_immediate_correctly() {
        // beq t0, zero, <向后 8 字节>：B 型立即数 4（半字）
        let mut unit = CompilationUnit::new();
        let mut code = vec![0x00u8; 8];
        code.extend_from_slice(&0x0000_8063u32.to_le_bytes()); // beq t0, zero, 0（占位）
        unit.code = code;
        unit.symbols.push(Symbol {
            name: "bb1".to_string(),
            section: SymbolSection::Text,
            offset: 16,
            size: 0,
            global: false,
            kind: SymbolKind::Function,
        });
        unit.relocations.push(Relocation::new(8, 8, 4, "bb1", RelocationKind::RiscVBranch));
        apply_relocations(&mut unit, 0x2000);
        let word = u32::from_le_bytes(unit.code[8..12].try_into().unwrap());
        // imm = 4 个半字 → imm[4:1] = 2 → 指令的 [11:8] = 2
        assert_eq!((word >> 8) & 0xf, 2);
        // funct3 = 001（bne 用了 001；此处用 beq 的 000）与 opcode 0x63 必须保留
        assert_eq!(word & 0x707f, 0x0063, "funct3 与 opcode 必须保留");
    }

    #[test]
    fn extern_symbols_are_added_once() {
        let mut unit = CompilationUnit::new();
        unit.relocations.push(Relocation::new(0, 0, 4, "zr_exit", RelocationKind::PcRel32));
        unit.relocations.push(Relocation::new(4, 4, 4, "zr_exit", RelocationKind::PcRel32));
        add_extern_symbols(&mut unit);
        add_extern_symbols(&mut unit);
        let externs: Vec<_> = unit
            .symbols
            .iter()
            .filter(|s| s.section == SymbolSection::Undefined)
            .collect();
        assert_eq!(externs.len(), 1);
        assert_eq!(externs[0].name, "zr_exit");
        assert!(externs[0].global);
    }
}