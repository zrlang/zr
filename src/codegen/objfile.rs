//! 容器无关的目标文件工具：符号解析、重定位计算与名称修饰。
//!
//! ELF / Mach-O / PE-COFF 三种容器在"符号 → 地址"与"重定位 → 立即数"
//! 两件事上的语义是一致的（都遵循 `S + A - P`），差别只在字段编码方式
//! 与宽度。本模块把这部分抽出来共享，各容器写入器只负责排版头部。

use std::collections::HashMap;

use crate::codegen::{
    CompilationUnit, Relocation, RelocationKind, Symbol, SymbolKind, SymbolSection,
};

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
        | RelocationKind::RipRel32
        | RelocationKind::AdrPrelPage21
        | RelocationKind::AddAbsLo12
        | RelocationKind::Branch26
        | RelocationKind::Call26
        | RelocationKind::ArmBranch24
        | RelocationKind::ArmCallThumb
        | RelocationKind::ArmBranchThumb
        | RelocationKind::ArmCbzThumb
        | RelocationKind::ArmMovwAbs
        | RelocationKind::ArmMovtAbs
        | RelocationKind::ThumbMovwAbs
        | RelocationKind::ThumbMovtAbs
        | RelocationKind::RiscVJal
        | RelocationKind::RiscVBranch
        | RelocationKind::RiscVHi20
        | RelocationKind::RiscVLo12I => 4,
    }
}

/// ELF 重定位类型号（按架构）。
pub fn elf_reloc_type(arch: crate::codegen::target::Arch, kind: RelocationKind) -> u32 {
    use crate::codegen::target::Arch;
    match (arch, kind) {
        (Arch::X86_64, RelocationKind::PcRel32) => 4, // R_X86_64_PLT32
        (Arch::X86_64, RelocationKind::RipRel32) => 2, // R_X86_64_PC32
        (Arch::AArch64, RelocationKind::Branch26) => 282, // R_AARCH64_JUMP26
        (Arch::AArch64, RelocationKind::Call26) => 283, // R_AARCH64_CALL26
        (Arch::AArch64, RelocationKind::AdrPrelPage21) => 275, // R_AARCH64_ADR_PREL_PG_HI21
        (Arch::AArch64, RelocationKind::AddAbsLo12) => 277, // R_AARCH64_ADD_ABS_LO12_NC
        (Arch::Arm32, RelocationKind::ArmBranch24) => 29, // R_ARM_JUMP24
        (Arch::Arm32, RelocationKind::ArmCallThumb) => 28, // R_ARM_THM_CALL
        (Arch::Arm32, RelocationKind::ArmBranchThumb) => 30, // R_ARM_THM_JUMP24
        (Arch::Arm32, RelocationKind::ArmCbzThumb) => 31, // R_ARM_THM_JUMP8
        (Arch::Arm32, RelocationKind::ArmMovwAbs) => 43, // R_ARM_MOVW_ABS_NC
        (Arch::Arm32, RelocationKind::ArmMovtAbs) => 44, // R_ARM_MOVT_ABS
        (Arch::Arm32Thumb, RelocationKind::ThumbMovwAbs) => 47, // R_ARM_THM_MOVW_ABS_NC
        (Arch::Arm32Thumb, RelocationKind::ThumbMovtAbs) => 48, // R_ARM_THM_MOVT_ABS
        (Arch::RiscV32 | Arch::RiscV64, RelocationKind::RiscVJal) => 17, // R_RISCV_JAL
        (Arch::RiscV32 | Arch::RiscV64, RelocationKind::RiscVBranch) => 16, // R_RISCV_BRANCH
        (Arch::RiscV32 | Arch::RiscV64, RelocationKind::RiscVHi20) => 26, // R_RISCV_HI20
        (Arch::RiscV32 | Arch::RiscV64, RelocationKind::RiscVLo12I) => 27, // R_RISCV_LO12_I
        _ => 0,
    }
}

/// COFF 重定位类型号（按架构）。
pub fn coff_reloc_type(arch: crate::codegen::target::Arch, kind: RelocationKind) -> u16 {
    use crate::codegen::target::Arch;
    match (arch, kind) {
        (Arch::X86_64, RelocationKind::PcRel32) => 0x0004, // IMAGE_REL_AMD64_REL32
        (Arch::X86_64, RelocationKind::RipRel32) => 0x0004, // IMAGE_REL_AMD64_REL32
        (Arch::AArch64, RelocationKind::Branch26) | (Arch::AArch64, RelocationKind::Call26) => {
            0x0003 // IMAGE_REL_ARM64_BRANCH26
        }
        (Arch::AArch64, RelocationKind::AdrPrelPage21) => 0x0004, // IMAGE_REL_ARM64_PAGEBASE_REL21
        (Arch::AArch64, RelocationKind::AddAbsLo12) => 0x0006,    // IMAGE_REL_ARM64_PAGEOFFSET_12A
        _ => 0,
    }
}

/// Mach-O 重定位类型号（按架构）。
pub fn macho_reloc_type(arch: crate::codegen::target::Arch, kind: RelocationKind) -> u8 {
    use crate::codegen::target::Arch;
    match (arch, kind) {
        // X86_64_RELOC_BRANCH 与 ARM64_RELOC_BRANCH26 都是 2（`bl` 与 `b` 同理）。
        (Arch::X86_64, RelocationKind::PcRel32) => 2, // X86_64_RELOC_BRANCH
        // 取符号地址的 `lea reg, [rip+disp]` 用 SIGNED（1），不能复用 BRANCH。
        (Arch::X86_64, RelocationKind::RipRel32) => 1, // X86_64_RELOC_SIGNED
        (Arch::AArch64, RelocationKind::Branch26) | (Arch::AArch64, RelocationKind::Call26) => 2, // ARM64_RELOC_BRANCH26
        (Arch::AArch64, RelocationKind::AdrPrelPage21) => 3, // ARM64_RELOC_PAGE21
        (Arch::AArch64, RelocationKind::AddAbsLo12) => 4,    // ARM64_RELOC_PAGEOFFSET12
        (Arch::Arm32, RelocationKind::ArmBranch24)
        | (Arch::RiscV32, RelocationKind::RiscVJal)
        | (Arch::RiscV64, RelocationKind::RiscVJal)
        | (Arch::RiscV32, RelocationKind::RiscVBranch)
        | (Arch::RiscV64, RelocationKind::RiscVBranch) => 2,
        _ => 0,
    }
}

/// Mach-O 重定位是否置 `r_pcrel` 位。
///
/// 只有 AArch64 的 `add :lo12:`（页内偏移）不是 PC 相对；其余（分支、
/// `adrp` 页号、x86-64 的 `call`/`lea`）都置位。
pub fn macho_reloc_is_pcrel(kind: RelocationKind) -> bool {
    !matches!(kind, RelocationKind::AddAbsLo12)
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
pub fn symbol_addresses(
    unit: &CompilationUnit,
    text_base: u64,
    rodata_base: u64,
) -> HashMap<String, u64> {
    unit.symbols
        .iter()
        .filter(|s| s.section != SymbolSection::Undefined)
        .map(|s| {
            // 符号的偏移是**段内**偏移，因此要按所在段选基址：代码段用
            // `text_base`，只读数据段用 `rodata_base`。
            let base = match s.section {
                SymbolSection::Rodata => rodata_base,
                _ => text_base,
            };
            (s.name.clone(), base + s.offset as u64)
        })
        .collect()
}

/// 就地应用重定位（用于自行构造可执行文件的场景）。
///
/// 重定位语义为 `S + A - P`：
/// - `PcRel32`：立即数字段整体就是位移，x86-64 的 `call/jmp/jcc` 都如此；
/// - `Branch26`：AArch64 的 26 位字面量嵌在指令自身的 [25:0] 位里，
///   因此必须"读-改-写"，保留高 6 位操作码。
pub fn apply_relocations(unit: &mut CompilationUnit, text_base: u64, rodata_base: u64) {
    let addrs = symbol_addresses(unit, text_base, rodata_base);

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
            RelocationKind::RipRel32 => {
                // `lea reg, [rip+disp32]`：P 是 4 字节位移字段本身的地址，
                // 目标值 = S + A - P（加载符号地址时 A = -4）。x86 的 RIP
                // 相对位移基准是字段末尾，两者正好抵消成 "S - 下一条指令"。
                let p = text_base + reloc.offset as u64;
                let delta = target - p as i64;
                if reloc.offset + 4 <= unit.code.len() {
                    unit.code[reloc.offset..reloc.offset + 4]
                        .copy_from_slice(&(delta as i32).to_le_bytes());
                }
            }
            RelocationKind::AdrPrelPage21 => {
                // `adrp Xd, sym`：imm21 = (page(S+A) - page(P)) >> 12，P 是
                // 指令地址（不是字段地址）。immlo 在 [30:29]、immhi 在 [23:5]。
                let p = text_base + reloc.instr_start as u64;
                let delta = (target - (p & !0xfff) as i64) >> 12;
                let imm = delta as u32;
                patch_word(&mut unit.code, reloc, |word| {
                    (word & 0x9f00_001f) | ((imm & 0x3) << 29) | (((imm >> 2) & 0x7ffff) << 5)
                });
            }
            RelocationKind::AddAbsLo12 => {
                // `add Xd, Xn, #imm12`：imm12 = (S + A) & 0xfff，落在 [21:10]。
                let imm12 = (target as u32) & 0xfff;
                patch_word(&mut unit.code, reloc, |word| {
                    (word & 0xffc0_03ff) | (imm12 << 10)
                });
            }
            RelocationKind::ArmCbzThumb => {
                // `cbz` / `cbnz`：6 位**有符号**半字偏移，落在 [9:3]，
                // Rn 在 [2:0]（16 位形式只支持 r0-r3）。
                let pc = instr_va as i64 + 4;
                let delta = target - pc;
                let off = (delta >> 1) & 0x3f;
                if reloc.offset + 2 <= unit.code.len() {
                    let hw =
                        u16::from_le_bytes([unit.code[reloc.offset], unit.code[reloc.offset + 1]]);
                    let patched = (hw & !0x03f8) | ((off as u16 & 0x3f) << 3);
                    unit.code[reloc.offset..reloc.offset + 2]
                        .copy_from_slice(&patched.to_le_bytes());
                }
            }
            RelocationKind::ArmCallThumb => {
                // `bl` (T1)。imm32 = SignExtend(S:I1:I2:imm10:imm11:'0')，
                // J1/J2 是 I1/I2 取反；hw1 是 0xF 组，hw2 是 0xD 组，
                // **hw1 必须在低地址**（`w32` 就是这个顺序）。
                // clang: `bl g1`(delta=8) = f000 f804
                let pc = instr_va as i64 + 4;
                let delta = target - pc;
                let off = delta >> 1;
                let s = (off < 0) as u16;
                let i1 = ((off >> 23) & 1) as u16;
                let i2 = ((off >> 22) & 1) as u16;
                let imm10 = ((off >> 12) & 0x3ff) as u16;
                let imm11 = ((off >> 1) & 0x7ff) as u16;
                let j1 = i1 ^ 1;
                let j2 = i2 ^ 1;
                let hw1 = 0xf000 | (s << 10) | imm10;
                let hw2 = 0xd000 | (j1 << 13) | 0x1000 | (j2 << 11) | imm11;
                if reloc.offset + 4 <= unit.code.len() {
                    unit.code[reloc.offset..reloc.offset + 2].copy_from_slice(&hw1.to_le_bytes());
                    unit.code[reloc.offset + 2..reloc.offset + 4]
                        .copy_from_slice(&hw2.to_le_bytes());
                }
            }
            RelocationKind::ArmBranchThumb => {
                if reloc.instr_len == 2 {
                    // T1 条件分支：8 位半字偏移在 [7:0]（`1101 cond imm8`）
                    let pc = instr_va as i64 + 4;
                    let delta = target - pc;
                    let off = (delta >> 1) & 0xff;
                    if reloc.offset + 2 <= unit.code.len() {
                        let hw = u16::from_le_bytes([
                            unit.code[reloc.offset],
                            unit.code[reloc.offset + 1],
                        ]);
                        let patched = (hw & !0x00ff) | (off as u16 & 0xff);
                        unit.code[reloc.offset..reloc.offset + 2]
                            .copy_from_slice(&patched.to_le_bytes());
                    }
                } else {
                    // `b` (T4)。与 BL 同形，只是 hw2 的 bit12 为 0。
                    // clang: `b.w g3`(delta=4) = f000 b802
                    let pc = instr_va as i64 + 4;
                    let delta = target - pc;
                    let off = delta >> 1;
                    let s = (off < 0) as u16;
                    let i1 = ((off >> 23) & 1) as u16;
                    let i2 = ((off >> 22) & 1) as u16;
                    let imm10 = ((off >> 12) & 0x3ff) as u16;
                    let imm11 = ((off >> 1) & 0x7ff) as u16;
                    let j1 = i1 ^ 1;
                    let j2 = i2 ^ 1;
                    let hw1 = 0xf000 | (s << 10) | imm10;
                    let hw2 = 0xd000 | (j1 << 13) | (j2 << 11) | imm11;
                    if reloc.offset + 4 <= unit.code.len() {
                        unit.code[reloc.offset..reloc.offset + 2]
                            .copy_from_slice(&hw1.to_le_bytes());
                        unit.code[reloc.offset + 2..reloc.offset + 4]
                            .copy_from_slice(&hw2.to_le_bytes());
                    }
                }
            }
            RelocationKind::ArmMovwAbs | RelocationKind::ArmMovtAbs => {
                // A32 `movw`/`movt` 绝对取址对：`movw` 装 (S+A) 低 16 位、
                // `movt` 装高 16 位。imm16 拆成 imm4[19:16] 与 imm12[11:0]。
                let full = target as u32;
                let imm16 = match reloc.kind {
                    RelocationKind::ArmMovwAbs => full & 0xffff,
                    _ => full >> 16,
                };
                let imm4 = ((imm16 >> 12) & 0xf) << 16;
                let imm12 = imm16 & 0xfff;
                patch_word(&mut unit.code, reloc, |word| {
                    const MASK: u32 = !((0xf << 16) | 0xfff);
                    (word & MASK) | imm4 | imm12
                });
            }
            RelocationKind::ThumbMovwAbs | RelocationKind::ThumbMovtAbs => {
                // `movw`/`movt` 绝对取址对：`movw` 装 (S+A) 低 16 位、
                // `movt` 装高 16 位。imm16 的位散布在四个字段：
                //   i    = imm16[11]   → [10]      imm4 = imm16[15:12] → [3:0]
                //   imm3 = imm16[10:8] → [30:28]   imm8 = imm16[7:0]   → [23:16]
                let full = target as u32;
                let imm16 = match reloc.kind {
                    RelocationKind::ThumbMovwAbs => full & 0xffff,
                    _ => full >> 16,
                };
                let i = ((imm16 >> 11) & 1) << 10;
                let imm4 = (imm16 >> 12) & 0xf;
                let imm3 = ((imm16 >> 8) & 7) << 28;
                let imm8 = (imm16 & 0xff) << 16;
                patch_word(&mut unit.code, reloc, |word| {
                    const MASK: u32 = !((1 << 10) | 0xf | (7 << 28) | (0xff << 16));
                    (word & MASK) | i | imm4 | imm3 | imm8
                });
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
                // J 型（jal）：立即数是**字节**差（目标 − 指令地址），
                // imm[0] 因指令半字对齐恒为 0、不入字段，拆位按规范：
                //   imm[20]   → [31]      imm[10:1] → [30:21]
                //   imm[11]   → [20]      imm[19:12]→ [19:12]
                let delta = target - instr_va as i64;
                let imm = delta as u32 & 0x1f_ffff;
                patch_word(&mut unit.code, reloc, |word| {
                    (word & 0x0000_0fff)                 // 保留 [11:0]（rd + opcode）
                        | (imm & 0x000f_f000)             // imm[19:12] 原位
                        | ((imm & 0x0000_0800) << 9)      // imm[11]    → [20]
                        | ((imm & 0x0000_07fe) << 20)     // imm[10:1]  → [30:21]
                        | ((imm & 0x0010_0000) << 11) // imm[20]    → [31]
                });
            }
            RelocationKind::RiscVBranch => {
                // B 型（beq 等）：立即数同样以字节为单位（imm[0] = 0 缺省），
                // 字段拆位按规范：
                //   imm[12]  → [31]   imm[10:5] → [30:25]
                //   imm[4:1] → [11:8] imm[11]   → [7]
                let delta = target - instr_va as i64;
                let imm = delta as u32 & 0x1fff;
                patch_word(&mut unit.code, reloc, |word| {
                    (word & 0x01ff_f07f)                 // 保留 rs1/rs2/funct3/opcode
                        | ((imm & 0x0000_1000) << 19)    // imm[12]   → [31]
                        | ((imm & 0x0000_07e0) << 20)    // imm[10:5] → [30:25]
                        | ((imm & 0x0000_001e) << 7)     // imm[4:1]  → [11:8]
                        | ((imm & 0x0000_0800) >> 4) // imm[11]   → [7]
                });
            }
            RelocationKind::RiscVHi20 => {
                // `lui rd, %hi(S+A)`：imm20 = (S + 0x800) >> 12（进位舍入，
                // 和 addi %lo 的符号扩展互相抵消），落在 [31:12]。
                let hi = ((target + 0x800) >> 12) as u32 & 0xf_ffff;
                patch_word(&mut unit.code, reloc, |word| {
                    (word & 0x0000_0fff) | (hi << 12)
                });
            }
            RelocationKind::RiscVLo12I => {
                // `addi rd, rd, %lo(S+A)`：imm12 = (S + A) & 0xfff，按
                // 符号扩展进 [31:20]——负半程由 `hi20` 的进位舍入抵消。
                let lo = target as u32 & 0xfff;
                patch_word(&mut unit.code, reloc, |word| {
                    (word & 0x000f_ffff) | (lo << 20)
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
        let relocs = vec![Relocation::new(1, 0, 5, "main", RelocationKind::PcRel32)];
        (unit, relocs)
    }

    #[test]
    fn pcrel32_uses_next_instruction_address() {
        let (mut unit, relocs) = unit_with_one_call();
        unit.relocations = relocs;
        apply_relocations(&mut unit, 0x400000, 0);
        // main 在 0x400000，call 在 0x400000，下一条指令 0x400005，
        // 因此位移应为 -5。
        assert_eq!(i32::from_le_bytes(unit.code[1..5].try_into().unwrap()), -5);
    }

    #[test]
    fn branch26_counts_four_byte_units() {
        let mut unit = CompilationUnit::new();
        // 前 8 字节是 main 本身，偏移 8 处是待修正的 bl
        unit.code = vec![
            0x20, 0x00, 0x80, 0xd2, 0x20, 0x00, 0x80, 0xd2, 0x00, 0x00, 0x00, 0x94,
        ];
        unit.symbols.push(Symbol {
            name: "main".to_string(),
            section: SymbolSection::Text,
            offset: 0, // main 位于偏移 0，bl 位于偏移 8
            size: 8,
            global: true,
            kind: SymbolKind::Function,
        });
        unit.relocations
            .push(Relocation::new(8, 8, 4, "main", RelocationKind::Branch26));
        apply_relocations(&mut unit, 0x1000, 0);
        let word = u32::from_le_bytes(unit.code[8..12].try_into().unwrap());
        assert_eq!(word >> 26, 0b100101, "bl 操作码应保留");
        // 向后跳 8 字节 = -2 条指令，按 26 位补码编码。
        assert_eq!(word & 0x03ff_ffff, 0x03ff_fffe, "imm26 = -2");
        assert_eq!(
            unit.code[..8],
            [0x20, 0x00, 0x80, 0xd2, 0x20, 0x00, 0x80, 0xd2],
            "main 未被改动"
        );
    }

    #[test]
    fn external_symbols_are_left_to_the_linker() {
        let (mut unit, relocs) = unit_with_one_call();
        unit.relocations = vec![Relocation::new(1, 0, 5, "zr_exit", RelocationKind::PcRel32)];
        let _ = relocs;
        apply_relocations(&mut unit, 0x400000, 0);
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
            coff_reloc_type(
                crate::codegen::target::Arch::AArch64,
                RelocationKind::Branch26
            ),
            0x0003
        );
        assert_eq!(
            elf_reloc_type(
                crate::codegen::target::Arch::AArch64,
                RelocationKind::Call26
            ),
            283
        );
    }

    #[test]
    fn riscv_jal_relocation_scatters_immediate_correctly() {
        // jal ra, <向后 12 字节>：字节差 -12 拆到 J 型 4 段
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
        unit.relocations
            .push(Relocation::new(12, 12, 4, "main", RelocationKind::RiscVJal));
        apply_relocations(&mut unit, 0x1000, 0);
        let word = u32::from_le_bytes(unit.code[12..16].try_into().unwrap());
        // 字节差 -12（imm = 0x1FFFF4）→ imm[20]=1（→[31]）、imm[10:1]=0x3FA
        // （→[30:21]）、imm[11]=1（→[20]）、imm[19:12]=0xFF（原位），ra=1
        assert_eq!(word, 0xff5f_f0ef);
        assert_eq!(word & 0xfff, 0x0ef, "rd 与 opcode 必须保留");
    }

    #[test]
    fn riscv_branch_relocation_scatters_immediate_correctly() {
        // beq t0, zero, <向前 8 字节>：字节差 +8（bb1 在 +16，指令在 +8）
        let mut unit = CompilationUnit::new();
        let mut code = vec![0x00u8; 8];
        code.extend_from_slice(&0x0002_8063u32.to_le_bytes()); // beq t0, zero, 0（占位）
        unit.code = code;
        unit.symbols.push(Symbol {
            name: "bb1".to_string(),
            section: SymbolSection::Text,
            offset: 16,
            size: 0,
            global: false,
            kind: SymbolKind::Function,
        });
        unit.relocations
            .push(Relocation::new(8, 8, 4, "bb1", RelocationKind::RiscVBranch));
        apply_relocations(&mut unit, 0x2000, 0);
        let word = u32::from_le_bytes(unit.code[8..12].try_into().unwrap());
        // 字节差 +8：imm[4:1] = 4 → [11:8] = 4，其余段为 0
        assert_eq!(word, 0x0002_8463);
        assert_eq!(word & 0x7000, 0, "funct3 必须保留");
        assert_eq!((word >> 15) & 0x1f, 5, "rs1 必须保留");
        assert_eq!((word >> 20) & 0x1f, 0, "rs2 必须保留");
    }

    #[test]
    fn rip_relative_lea_resolves_rodata_symbol() {
        // `lea rax, [rip+disp32]` 位于 .text+0，符号在 .rodata+0。
        let mut unit = CompilationUnit::new();
        unit.code = vec![0x48, 0x8d, 0x05, 0, 0, 0, 0];
        unit.symbols.push(Symbol {
            name: ".Lstr0".to_string(),
            section: SymbolSection::Rodata,
            offset: 0,
            size: 3,
            global: false,
            kind: SymbolKind::Object,
        });
        let mut reloc = Relocation::new(3, 0, 7, ".Lstr0", RelocationKind::RipRel32);
        reloc.addend = -4;
        unit.relocations.push(reloc);
        apply_relocations(&mut unit, 0x40_0000, 0x41_0000);
        let disp = i32::from_le_bytes(unit.code[3..7].try_into().unwrap());
        // 下一条指令地址 + disp 必须等于 .rodata 的地址。
        assert_eq!(0x40_0000 + 7 + disp as i64, 0x41_0000);
    }

    #[test]
    fn aarch64_page_relocations_resolve_rodata() {
        let mut unit = CompilationUnit::new();
        unit.code = vec![0u8; 8];
        unit.code[0..4].copy_from_slice(&0x9000_0000u32.to_le_bytes()); // adrp x0, #0
        unit.code[4..8].copy_from_slice(&0x9100_0000u32.to_le_bytes()); // add x0, x0, #0
        unit.symbols.push(Symbol {
            name: ".Lstr0".to_string(),
            section: SymbolSection::Rodata,
            offset: 0x0d,
            size: 3,
            global: false,
            kind: SymbolKind::Object,
        });
        unit.relocations.push(Relocation::new(
            0,
            0,
            4,
            ".Lstr0",
            RelocationKind::AdrPrelPage21,
        ));
        unit.relocations.push(Relocation::new(
            4,
            4,
            4,
            ".Lstr0",
            RelocationKind::AddAbsLo12,
        ));
        apply_relocations(&mut unit, 0x40_0000, 0x40_1000);
        let adrp = u32::from_le_bytes(unit.code[0..4].try_into().unwrap());
        let add = u32::from_le_bytes(unit.code[4..8].try_into().unwrap());
        // 目标页 0x401000 − 指令页 0x400000 = 1 页。
        let imm = ((adrp >> 5) & 0x7ffff) << 2 | ((adrp >> 29) & 0x3);
        assert_eq!(imm, 1, "adrp 页差应为 1");
        assert_eq!(add & 0xffc0_03ff, 0x9100_0000, "保留 opcode/Rn/Rd");
        assert_eq!((add >> 10) & 0xfff, 0x0d, "lo12 = 0x0d");
    }

    #[test]
    fn arm_movw_movt_absolute_pair_resolves_rodata() {
        // `movw r9,#0` / `movt r9,#0` 位于 .text+0/+4，字符串在 .rodata+0x1a6。
        let mut unit = CompilationUnit::new();
        unit.code = Vec::new();
        unit.code.extend_from_slice(&0xe300_9000u32.to_le_bytes()); // movw r9, #0
        unit.code.extend_from_slice(&0xe340_9000u32.to_le_bytes()); // movt r9, #0
        unit.symbols.push(Symbol {
            name: ".Lstr0".to_string(),
            section: SymbolSection::Rodata,
            offset: 0x1a6,
            size: 6,
            global: false,
            kind: SymbolKind::Object,
        });
        unit.relocations.push(Relocation::new(
            0,
            0,
            4,
            ".Lstr0",
            RelocationKind::ArmMovwAbs,
        ));
        unit.relocations.push(Relocation::new(
            4,
            4,
            4,
            ".Lstr0",
            RelocationKind::ArmMovtAbs,
        ));
        apply_relocations(&mut unit, 0x0800_0000, 0x0800_0000);
        let movw = u32::from_le_bytes(unit.code[0..4].try_into().unwrap());
        let movt = u32::from_le_bytes(unit.code[4..8].try_into().unwrap());
        // 目标 = 0x08000000 + 0x1a6 = 0x080001a6：movw 装低 16 位 0x01a6，
        // movt 装高 16 位 0x0800（imm4[19:16] | imm12[11:0]）。
        assert_eq!(movw, 0xe300_91a6, "movw 应装 (S+A) 低 16 位");
        assert_eq!(movt, 0xe340_9800, "movt 应装 (S+A) 高 16 位");
    }

    #[test]
    fn extern_symbols_are_added_once() {
        let mut unit = CompilationUnit::new();
        unit.relocations
            .push(Relocation::new(0, 0, 4, "zr_exit", RelocationKind::PcRel32));
        unit.relocations
            .push(Relocation::new(4, 4, 4, "zr_exit", RelocationKind::PcRel32));
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
