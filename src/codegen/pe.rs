//! PE/COFF 容器格式（Windows：x86-64 与 aarch64）。
//!
//! 本模块产出 64 位 COFF **目标文件**（`.obj`）：
//! ```text
//! IMAGE_FILE_HEADER              20B
//! Section Table                  每节 40B（.text / .rdata / .data）
//! 节数据 + 逐节的 IMAGE_RELOCATION 数组（10B / 项）
//! IMAGE_SYMBOLS（18B / 项）+ STRING_TABLE（4B 长度 + NUL 结尾）
//! ```
//!
//! 最终可执行文件由平台链接器（`lld-link` 或 MinGW `gcc`）在导入
//! `kernel32` 等系统库后生成——PE 的导入表必须由链接器填充，自行拼接
//! 的映像无法独立运行。
//!
//! 与 ELF 的关键差异：
//! - **没有 Program Header**：节通过 `VirtualAddress`（RVA）+ 镜像基址定位；
//! - **代码重定位存在所属节的数据之后**，由节头的
//!   `PointerToRelocations` / `NumberOfRelocations` 指向，不是独立的
//!   `.rela` 节；
//! - **字符串表在符号表之后**（与 ELF 的 `.strtab` 在前相反），
//!   且首 4 字节是总长度。
//!
//! Windows x64 / ARM64 遵循 Microsoft x64 ABI：前 4 个参数位于
//! `rcx/rdx/r8/r9`（ARM64 为 `x0..x3`），调用者负责预留 32 字节
//! shadow space，因此运行时封装中的每次调用前都要 `sub rsp, 0x20`。

use std::io::{self, Write};

use crate::codegen::objfile::{align_up, coff_reloc_type};
use crate::codegen::target::Target;
use crate::codegen::{CompilationUnit, SymbolKind, SymbolSection};

// ---- 文件头常量 ----
/// IMAGE_FILE_RELOCS_STRIPPED | IMAGE_FILE_LINE_NUMS_STRIPPED |
/// IMAGE_FILE_LOCAL_SYMS_STRIPPED | IMAGE_FILE_32BIT_MACHINE
const COFF_CHARS_OBJ: u16 = 0x0001 | 0x0004 | 0x0008 | 0x0100;

// ---- 可选头常量 ----
const IMAGE_SCN_CNT_CODE: u32 = 0x0000_0020;
const IMAGE_SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;
const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;

const SEC_TEXT: &str = ".text";
const SEC_RODATA: &str = ".rdata";
const SEC_DATA: &str = ".data";

/// COFF `IMAGE_RELOCATION` 大小（10 字节）。
const COFF_RELOC_SIZE: usize = 10;
/// COFF `IMAGE_SYMBOL` 大小（18 字节）。
const COFF_SYMBOL_SIZE: usize = 18;

/// 把编译产物组装为 COFF 目标文件（`.obj`）。
pub fn assemble_coff(target: &Target, unit: &CompilationUnit) -> io::Result<Vec<u8>> {
    let (symbols, strings) = build_coff_symbols(unit);
    // 符号表下标 → 符号名，用于填 IMAGE_RELOCATION::SymbolTableIndex。
    let symbol_index: Vec<(String, u32)> = unit
        .symbols
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.clone(), i as u32))
        .collect();

    // 所有代码重定位都作用于 .text（zr 后端只有 .text 有指令）。
    let relocs = build_coff_relocations(target, unit, &symbol_index);

    let mut sections: Vec<CoffSection> = vec![CoffSection::new(
        SEC_TEXT,
        unit.code.clone(),
        IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_EXECUTE | IMAGE_SCN_MEM_READ,
    )];
    if !unit.rodata.is_empty() {
        sections.push(CoffSection::new(
            SEC_RODATA,
            unit.rodata.clone(),
            IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ,
        ));
    }
    if !unit.data.is_empty() {
        sections.push(CoffSection::new(
            SEC_DATA,
            unit.data.clone(),
            IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE,
        ));
    }

    // ---- 布局：节数据后紧跟各节的重定位数组 ----
    let mut cursor = (20 + sections.len() * 40) as u32;
    for (i, section) in sections.iter_mut().enumerate() {
        section.raw_offset = cursor;
        section.raw_size = section.data.len() as u32;
        cursor = align_up(u64::from(cursor) + u64::from(section.raw_size), 4) as u32;
        if i == 0 {
            section.reloc_offset = cursor;
            section.relocs = relocs.clone();
            let bytes = (section.relocs.len() * COFF_RELOC_SIZE) as u64;
            cursor = align_up(u64::from(cursor) + bytes, 4) as u32;
        }
    }

    let symtab_offset = cursor;
    let has_symbols = !symbols.is_empty();
    let symtab_ptr = if has_symbols { symtab_offset } else { 0 };
    let nsyms = if has_symbols { (symbols.len() / COFF_SYMBOL_SIZE) as u32 } else { 0 };

    let mut buf: Vec<u8> = Vec::new();

    // ---- IMAGE_FILE_HEADER ----
    buf.write_all(&target.arch().pe_machine().to_le_bytes())?;
    buf.write_all(&(sections.len() as u16).to_le_bytes())?;
    buf.write_all(&0u32.to_le_bytes())?; // TimeDateStamp：保持可复现构建
    buf.write_all(&symtab_ptr.to_le_bytes())?;
    buf.write_all(&nsyms.to_le_bytes())?;
    buf.write_all(&0u16.to_le_bytes())?; // SizeOfOptionalHeader（目标文件为 0）
    buf.write_all(&COFF_CHARS_OBJ.to_le_bytes())?;

    // ---- Section Table ----
    let mut vaddr = 0u32;
    for section in &sections {
        write_coff_section_header(&mut buf, section, vaddr)?;
        vaddr = align_up(u64::from(vaddr) + u64::from(section.raw_size.max(1)), 16) as u32;
    }

    // ---- 节数据与逐节重定位 ----
    for section in &sections {
        pad_to(&mut buf, section.raw_offset as usize)?;
        buf.write_all(&section.data)?;
        if !section.relocs.is_empty() {
            pad_to(&mut buf, section.reloc_offset as usize)?;
            for reloc in &section.relocs {
                buf.write_all(&reloc.virtual_address.to_le_bytes())?;
                buf.write_all(&reloc.symbol_index.to_le_bytes())?;
                buf.write_all(&reloc.r#type.to_le_bytes())?;
            }
        }
    }

    // ---- 符号表与字符串表 ----
    if has_symbols {
        pad_to(&mut buf, symtab_offset as usize)?;
        buf.write_all(&symbols)?;
        buf.write_all(&strings)?;
    }
    Ok(buf)
}

/// COFF 节描述。
struct CoffSection {
    name: &'static str,
    data: Vec<u8>,
    characteristics: u32,
    raw_offset: u32,
    raw_size: u32,
    reloc_offset: u32,
    relocs: Vec<CoffRelocation>,
}

impl CoffSection {
    fn new(name: &'static str, data: Vec<u8>, characteristics: u32) -> Self {
        Self {
            name,
            data,
            characteristics,
            raw_offset: 0,
            raw_size: 0,
            reloc_offset: 0,
            relocs: Vec::new(),
        }
    }
}

/// COFF `IMAGE_RELOCATION`。
#[derive(Clone)]
struct CoffRelocation {
    /// 被修正字段在节内的偏移。
    virtual_address: u32,
    /// 目标符号在符号表中的下标。
    symbol_index: u32,
    /// 重定位类型（`IMAGE_REL_AMD64_REL32` / `IMAGE_REL_ARM64_BRANCH26`）。
    r#type: u16,
}

/// 构建 .text 的 COFF 重定位数组。
fn build_coff_relocations(
    target: &Target,
    unit: &CompilationUnit,
    symbol_index: &[(String, u32)],
) -> Vec<CoffRelocation> {
    unit.relocations
        .iter()
        .map(|reloc| CoffRelocation {
            virtual_address: reloc.offset as u32,
            symbol_index: symbol_index
                .iter()
                .find(|(name, _)| *name == reloc.symbol)
                .map(|(_, i)| *i)
                .unwrap_or(0),
            r#type: coff_reloc_type(target.arch(), reloc.kind),
        })
        .collect()
}

/// 写入 COFF `IMAGE_SECTION_HEADER`（40 字节）。
fn write_coff_section_header<W: Write>(w: &mut W, s: &CoffSection, vaddr: u32) -> io::Result<()> {
    w.write_all(&fixed_section_name(s.name))?;
    w.write_all(&0u32.to_le_bytes())?; // VirtualSize（COFF 对象文件不使用）
    w.write_all(&vaddr.to_le_bytes())?;
    w.write_all(&s.raw_size.to_le_bytes())?;
    w.write_all(&s.raw_offset.to_le_bytes())?;
    w.write_all(&(if s.relocs.is_empty() { 0 } else { s.reloc_offset }).to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // PointerToLinenumbers
    w.write_all(&(s.relocs.len() as u16).to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?; // NumberOfLinenumbers
    w.write_all(&s.characteristics.to_le_bytes())?;
    Ok(())
}

/// 8 字节节名。
fn fixed_section_name(name: &str) -> [u8; 8] {
    let mut buf = [0u8; 8];
    let bytes = name.as_bytes();
    let len = bytes.len().min(8);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

/// 构建 COFF 符号表与字符串表。
///
/// 字符串表以 4 字节总长度开头；符号名超过 8 字节时改存
/// `偏移 | 0x20000000` 的间接引用。
fn build_coff_symbols(unit: &CompilationUnit) -> (Vec<u8>, Vec<u8>) {
    let mut symbols: Vec<u8> = Vec::new();
    let mut strings: Vec<u8> = Vec::new();

    for sym in &unit.symbols {
        let mut name_field = [0u8; 8];
        if sym.name.len() <= 8 && sym.name.is_ascii() {
            name_field[..sym.name.len()].copy_from_slice(sym.name.as_bytes());
        } else {
            strings.extend_from_slice(sym.name.as_bytes());
            strings.push(0);
            // 偏移相对于字符串表首字节（含 4 字节长度字段）。
            let encoded = (strings.len() as u32) | 0x2000_0000;
            name_field.copy_from_slice(&encoded.to_le_bytes());
        }

        symbols.extend_from_slice(&name_field);
        symbols.extend_from_slice(&(sym.offset as u32).to_le_bytes());
        let section_number: i16 = if sym.section == SymbolSection::Undefined { 0 } else { 1 };
        symbols.extend_from_slice(&section_number.to_le_bytes());
        let symbol_type: u16 = match sym.kind {
            SymbolKind::Function => 0x20,
            SymbolKind::Section => 0,
        };
        symbols.extend_from_slice(&symbol_type.to_le_bytes());
        symbols.push(if sym.global { 3 } else { 0 }); // StorageClass = EXTERNAL / STATIC
        symbols.push(0); // NumberOfAuxSymbols
    }

    let mut strtab: Vec<u8> = Vec::with_capacity(strings.len() + 4);
    strtab.extend_from_slice(&((strings.len() + 4) as u32).to_le_bytes());
    strtab.extend_from_slice(&strings);
    (symbols, strtab)
}

/// 把缓冲区填充到指定长度（用零填充）。
fn pad_to(buf: &mut Vec<u8>, target: usize) -> io::Result<()> {
    if buf.len() < target {
        buf.resize(target, 0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::target::{Arch, Os};
    use crate::codegen::{OutputKind, Relocation, RelocationKind, Symbol};

    fn target(arch: Arch) -> Target {
        Target::new(arch, Os::Win)
    }

    fn sample_unit(kind: OutputKind) -> CompilationUnit {
        let mut unit = CompilationUnit::new();
        unit.kind = kind;
        unit.code = vec![0x48, 0x89, 0xc1, 0xc3]; // mov rcx, rax; ret
        unit.entry = "_start".to_string();
        unit.symbols.push(Symbol {
            name: "main".to_string(),
            section: SymbolSection::Text,
            offset: 0,
            size: 4,
            global: true,
            kind: SymbolKind::Function,
        });
        unit
    }

    #[test]
    fn coff_header_uses_target_machine() {
        for (arch, expected) in [(Arch::X86_64, 0x8664u16), (Arch::AArch64, 0xaa64u16)] {
            let unit = sample_unit(OutputKind::Relocatable);
            let bytes = assemble_coff(&target(arch), &unit).unwrap();
            assert_eq!(u16::from_le_bytes(bytes[0..2].try_into().unwrap()), expected);
            assert_eq!(u16::from_le_bytes(bytes[2..4].try_into().unwrap()), 1, ".text");
        }
    }

    #[test]
    fn object_declares_text_section() {
        let unit = sample_unit(OutputKind::Relocatable);
        let bytes = assemble_coff(&target(Arch::X86_64), &unit).unwrap();
        assert_eq!(&bytes[20..28], b".text\0\0\0");
    }

    #[test]
    fn external_references_get_relocations_and_symbols() {
        let mut unit = sample_unit(OutputKind::Relocatable);
        unit.relocations.push(Relocation::new(1, 0, 5, "zr_exit", RelocationKind::PcRel32));
        crate::codegen::objfile::add_extern_symbols(&mut unit);
        let bytes = assemble_coff(&target(Arch::X86_64), &unit).unwrap();

        // 节头的 NumberOfRelocations 应为 1
        let nrelocs = u16::from_le_bytes(bytes[20 + 32..20 + 34].try_into().unwrap());
        assert_eq!(nrelocs, 1);
        // IMAGE_REL_AMD64_REL32 = 4
        let reloc_type = coff_reloc_type(Arch::X86_64, RelocationKind::PcRel32);
        assert_eq!(reloc_type, 4);
        // 重定位数组紧跟 .text 数据，长度为 10 字节的整数倍
        let reloc_ptr = u32::from_le_bytes(bytes[20 + 24..20 + 28].try_into().unwrap());
        assert_ne!(reloc_ptr, 0);
        let reloc_type = u16::from_le_bytes(bytes[reloc_ptr as usize + 8..reloc_ptr as usize + 10].try_into().unwrap());
        assert_eq!(reloc_type, 4, "IMAGE_REL_AMD64_REL32");
    }

    #[test]
    fn arm64_uses_branch26_relocation() {
        assert_eq!(
            coff_reloc_type(Arch::AArch64, RelocationKind::Branch26),
            0x0003,
            "IMAGE_REL_ARM64_BRANCH26"
        );
    }

}
