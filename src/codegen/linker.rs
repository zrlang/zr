//! ELF 链接与文件格式构造。
//!
//! 产出 Linux x86-64 ELF64 可执行文件。ELF 结构：
//! ```text
//! ELF Header (64B)
//! Program Header (56B × n)   [仅可执行文件]
//! .shstrtab
//! .text                     [R-X, 虚拟基址 0x400000]
//! .rodata
//! .data                     [RW-]
//! Section Headers (64B × m)
//! ```
//!
//! 设计参考：
//! - Rust：`rustc_codegen_llvm` 通过 LLVM 的 MC 层产出目标文件。
//! - Zig：自带 ELF 链接器，支持增量链接与打包。
//! - Python：`pyinstaller` 打包为可执行文件（思路类比）。
//!
//! 关键不变量：
//! - `sh_addr`（以及 Program Header 的 `p_vaddr`/`p_paddr`）必须是**加载后的虚拟地址**，
//!   而 `sh_offset` 是**文件内偏移**。两者绝不能混用。
//! - 所有重定位（`call` 的 PC 相对偏移、绝对符号引用）必须在写出文件前应用，
//!   否则生成的可执行文件在运行时会调用到地址 0。

use std::io::{self, Write};

use crate::codegen::{CompilationUnit, Symbol, SymbolKind, SymbolSection};

/// 进程虚拟基址。Linux x86-64 传统约定为 0x400000。
const VIRT_BASE: u64 = 0x400000;

/// 页对齐（PT_LOAD 段必须按页对齐）。
const PAGE_ALIGN: u64 = 0x1000;

/// 段对齐（段内数据按 16 字节对齐）。
const SECT_ALIGN: u64 = 16;

/// 把编译产物组装为 ELF 字节流。
pub fn assemble_elf(unit: &CompilationUnit) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();

    let text = &unit.code;
    let data = &unit.data;
    let rodata = &unit.rodata;

    // 构建符号表与字符串表（必须在计算偏移前完成）。
    let (strtab, symtab) = build_symbol_tables(unit);

    // 可执行文件用虚拟地址布局；可重定位目标文件没有加载地址概念。
    let is_exec = unit.kind == crate::codegen::OutputKind::Executable;

    // ---------- 文件偏移布局 ----------
    let e_phoff: u64 = 64; // ELF header 紧接 Program Header
    let ph_size: u64 = if is_exec { 56 * 3 } else { 0 }; // text / rodata / data 三个 PT_LOAD

    // 各段文件偏移
    let shstrtab_off = align_up(64 + ph_size, SECT_ALIGN);
    let text_off = align_up(shstrtab_off + shstrtab_len(), SECT_ALIGN);
    let rodata_off = align_up(text_off + text.len() as u64, SECT_ALIGN);
    let data_off = align_up(rodata_off + rodata.len() as u64, SECT_ALIGN);
    let symtab_off = align_up(data_off + data.len() as u64, SECT_ALIGN);
    let strtab_off = align_up(symtab_off + symtab.len() as u64, SECT_ALIGN);
    let sh_off = align_up(strtab_off + strtab.len() as u64, SECT_ALIGN);

    // ---------- 虚拟地址布局 ----------
    // 三个 PT_LOAD 段共享同一个虚拟基址起点（按页对齐）。
    let text_vaddr = align_up(VIRT_BASE, PAGE_ALIGN);
    let rodata_vaddr = align_up(text_vaddr + text.len() as u64, PAGE_ALIGN);
    let data_vaddr = align_up(rodata_vaddr + rodata.len() as u64, PAGE_ALIGN);

    // === ELF Header ===
    buf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]); // ident
    buf.extend_from_slice(&[0; 8]); // padding
    let e_type: u16 = if is_exec { 2 } else { 1 };
    buf.extend_from_slice(&e_type.to_le_bytes());
    buf.extend_from_slice(&[0x3e, 0]); // e_machine: EM_X86_64
    buf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    let e_entry: u64 = if is_exec {
        text_vaddr + find_entry_offset(unit) as u64
    } else {
        0
    };
    buf.extend_from_slice(&e_entry.to_le_bytes()); // e_entry
    buf.extend_from_slice(&e_phoff.to_le_bytes()); // e_phoff
    buf.extend_from_slice(&sh_off.to_le_bytes()); // e_shoff
    buf.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    buf.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
    buf.extend_from_slice(&56u16.to_le_bytes()); // e_phentsize
    let phnum: u16 = if is_exec { 3 } else { 0 };
    buf.extend_from_slice(&phnum.to_le_bytes()); // e_phnum
    buf.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
    buf.extend_from_slice(&(SECTION_COUNT as u16).to_le_bytes()); // e_shnum
    buf.extend_from_slice(&(SHN_SHSTRTAB as u16).to_le_bytes()); // e_shstrndx

    // === Program Headers（仅可执行文件）===
    if is_exec {
        // PT_LOAD: .text (R-X, PF_R | PF_X = 0x5)
        write_program_header(
            &mut buf, 1, 0x5, text_off, text_vaddr, text_vaddr,
            text.len() as u64, text.len() as u64, PAGE_ALIGN,
        )?;
        // PT_LOAD: .rodata (R--, PF_R = 0x4)
        write_program_header(
            &mut buf, 1, 0x4, rodata_off, rodata_vaddr, rodata_vaddr,
            rodata.len() as u64, rodata.len() as u64, PAGE_ALIGN,
        )?;
        // PT_LOAD: .data (RW-, PF_R | PF_W = 0x6)
        write_program_header(
            &mut buf, 1, 0x6, data_off, data_vaddr, data_vaddr,
            data.len() as u64, data.len() as u64, PAGE_ALIGN,
        )?;
    }

    // === Section Headers ===
    // idx 0: NULL
    write_section_header(&mut buf, 0, 0, 0, 0, 0, 0, 0, 0, 0)?;
    // idx 1: .text
    write_section_header(
        &mut buf, SHN_TEXT, 1, 0x6, text_vaddr, text_off,
        text.len() as u64, 0, 0, SECT_ALIGN,
    )?;
    // idx 2: .rodata
    write_section_header(
        &mut buf, SHN_RODATA, 1, 0x2, rodata_vaddr, rodata_off,
        rodata.len() as u64, 0, 0, SECT_ALIGN,
    )?;
    // idx 3: .data
    write_section_header(
        &mut buf, SHN_DATA, 1, 0x3, data_vaddr, data_off,
        data.len() as u64, 0, 0, SECT_ALIGN,
    )?;
    // idx 4: .symtab
    write_section_header(
        &mut buf, SHN_SYMTAB, 2, 0, 0, symtab_off,
        symtab.len() as u64, SHN_STRTAB, 1, 8,
    )?;
    // idx 5: .strtab
    write_section_header(
        &mut buf, SHN_STRTAB, 3, 0, 0, strtab_off,
        strtab.len() as u64, 0, 0, 1,
    )?;
    // idx 6: .shstrtab
    write_section_header(
        &mut buf, SHN_SHSTRTAB, 3, 0, 0, shstrtab_off,
        shstrtab_len(), 0, 0, 1,
    )?;

    // === 段数据 ===
    pad_to(&mut buf, shstrtab_off as usize)?;
    buf.extend_from_slice(&shstrtab());
    pad_to(&mut buf, text_off as usize)?;
    buf.extend_from_slice(text);
    pad_to(&mut buf, rodata_off as usize)?;
    buf.extend_from_slice(rodata);
    pad_to(&mut buf, data_off as usize)?;
    buf.extend_from_slice(data);
    pad_to(&mut buf, symtab_off as usize)?;
    buf.extend_from_slice(&symtab);
    pad_to(&mut buf, strtab_off as usize)?;
    buf.extend_from_slice(&strtab);

    Ok(buf)
}

/// 应用重定位后组装并写出 ELF。
///
/// 这是对外入口：先回填所有重定位（此时知道各符号的虚拟地址），
/// 再写出文件。调用方负责打开文件。
pub fn emit_elf(unit: &CompilationUnit, path: &std::path::Path) -> io::Result<()> {
    // 先做一个可变的拷贝来应用重定位。重定位作用于 code 段，
    // 且必须基于虚拟地址计算 PC 相对偏移。
    let mut unit = unit.clone();
    apply_relocations(&mut unit);
    let bytes = assemble_elf(&unit)?;
    std::fs::write(path, &bytes)
}

/// 对可重定位单元应用重定位（虚拟地址语义）。
fn apply_relocations(unit: &mut CompilationUnit) {
    use crate::codegen::RelocationKind;

    // 符号名 → 虚拟地址（函数入口用 text_vaddr + offset；外部符号保留）。
    let text_vaddr = if unit.kind == crate::codegen::OutputKind::Executable {
        align_up(VIRT_BASE, PAGE_ALIGN)
    } else {
        0
    };

    let sym_map: std::collections::HashMap<&str, u64> = unit
        .symbols
        .iter()
        .filter(|s| s.section != crate::codegen::SymbolSection::Undefined)
        .map(|s| (s.name.as_str(), text_vaddr + s.offset as u64))
        .collect();

    for reloc in &unit.relocations {
        let Some(&target_va) = sym_map.get(reloc.symbol.as_str()) else {
            // 外部符号：保留重定位条目交给链接器处理。
            continue;
        };
        match reloc.kind {
            RelocationKind::PcRel32 => {
                // PC 相对：target - (instr_addr + instr_len)。
                // reloc.offset 指向立即数字段的第一个字节，call 的 instr_addr = offset - 1。
                let instr_addr = reloc.offset as i64 - 1;
                let delta = (target_va as i64) - (instr_addr + 5);
                let imm = (delta as i32).to_le_bytes();
                unit.code[reloc.offset..reloc.offset + 4].copy_from_slice(&imm);
            }
        }
    }
}

/// 查找入口符号在代码段中的偏移。
fn find_entry_offset(unit: &CompilationUnit) -> usize {
    unit.symbols
        .iter()
        .find(|s| s.name == unit.entry)
        .map(|s| s.offset)
        .unwrap_or(0)
}

/// 写入一个 Program Header（56 字节）。
fn write_program_header<W: Write>(
    w: &mut W,
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
) -> io::Result<()> {
    w.write_all(&p_type.to_le_bytes())?;
    w.write_all(&p_flags.to_le_bytes())?;
    w.write_all(&p_offset.to_le_bytes())?;
    w.write_all(&p_vaddr.to_le_bytes())?;
    w.write_all(&p_paddr.to_le_bytes())?;
    w.write_all(&p_filesz.to_le_bytes())?;
    w.write_all(&p_memsz.to_le_bytes())?;
    w.write_all(&p_align.to_le_bytes())?;
    Ok(())
}

/// 写入一个 Section Header（64 字节）。
fn write_section_header<W: Write>(
    w: &mut W,
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
) -> io::Result<()> {
    w.write_all(&sh_name.to_le_bytes())?;
    w.write_all(&sh_type.to_le_bytes())?;
    w.write_all(&sh_flags.to_le_bytes())?;
    w.write_all(&sh_addr.to_le_bytes())?;   // 虚拟地址，非文件偏移
    w.write_all(&sh_offset.to_le_bytes())?; // 文件偏移
    w.write_all(&sh_size.to_le_bytes())?;
    w.write_all(&sh_link.to_le_bytes())?;
    w.write_all(&sh_info.to_le_bytes())?;
    w.write_all(&sh_addralign.to_le_bytes())?;
    w.write_all(&0u64.to_le_bytes())?; // sh_entsize
    Ok(())
}

/// Section 索引常量。
const SHN_TEXT: u32 = 1;
const SHN_RODATA: u32 = 2;
const SHN_DATA: u32 = 3;
const SHN_SYMTAB: u32 = 4;
const SHN_STRTAB: u32 = 5;
const SHN_SHSTRTAB: u32 = 6;
const SECTION_COUNT: usize = 7;

/// `.shstrtab` 段内容（各段名）。
fn shstrtab() -> Vec<u8> {
    let mut b = vec![0]; // 首项为 NUL
    for name in &[".text", ".rodata", ".data", ".symtab", ".strtab", ".shstrtab"] {
        b.extend_from_slice(name.as_bytes());
        b.push(0);
    }
    b
}

/// `.shstrtab` 段长度。
fn shstrtab_len() -> u64 {
    shstrtab().len() as u64
}

/// 把缓冲区填充到指定长度（用零填充）。
fn pad_to(w: &mut Vec<u8>, target: usize) -> io::Result<()> {
    if w.len() < target {
        w.resize(target, 0);
    }
    Ok(())
}

/// 按对齐要求向上取整（要求 `align` 为 2 的幂，否则行为未定义）。
fn align_up(addr: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two(), "align must be a power of two");
    (addr + align - 1) & !(align - 1)
}

/// 把符号转为 ELF 符号表条目（供 `.symtab` 使用）。
/// `name_offset` 是符号名在 `.strtab` 中的偏移。
fn symbol_to_elf(sym: &Symbol, name_offset: u32) -> Vec<u8> {
    let mut b = Vec::new();
    // st_name
    b.extend_from_slice(&name_offset.to_le_bytes());
    // st_info: bind(高4) + type(低4)
    let bind = if sym.global { 1u8 } else { 0u8 }; // STB_GLOBAL or STB_LOCAL
    let ty = match sym.kind {
        SymbolKind::Function => 2u8, // STT_FUNC
        SymbolKind::Section => 3u8,  // STT_SECTION
    };
    b.push((bind << 4) | ty);
    // st_other
    b.push(0);
    // st_shndx
    let shndx = match sym.section {
        SymbolSection::Text => SHN_TEXT as u16,
        SymbolSection::Undefined => 0,
    };
    b.extend_from_slice(&shndx.to_le_bytes());
    // st_value
    b.extend_from_slice(&(sym.offset as u64).to_le_bytes());
    // st_size
    b.extend_from_slice(&(sym.size as u64).to_le_bytes());
    b
}

/// 构建 `.strtab`（符号名字符串表）和 `.symtab`（符号表）数据。
///
/// 返回 (strtab_bytes, symtab_bytes)。
fn build_symbol_tables(unit: &CompilationUnit) -> (Vec<u8>, Vec<u8>) {
    let mut strtab: Vec<u8> = vec![0]; // 首项为 NUL
    let mut symtab: Vec<u8> = Vec::new();

    // ELF 要求 symtab[0] 是一个全零的未定义符号。
    symtab.extend_from_slice(&[0u8; 24]);

    for sym in &unit.symbols {
        // 将符号名追加到 strtab
        let name_offset = strtab.len() as u32;
        strtab.extend_from_slice(sym.name.as_bytes());
        strtab.push(0);
        // 生成符号表条目
        symtab.extend_from_slice(&symbol_to_elf(sym, name_offset));
    }

    (strtab, symtab)
}
