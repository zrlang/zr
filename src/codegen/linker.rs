//! ELF 链接与文件格式构造。
//!
//! 覆盖 64 位（Linux x86-64 / aarch64、RISC-V 64）与 32 位
//! （Linux x86、i386、ARM、Cortex-M、RISC-V 32）两类目标：
//! ```text
//! ELF Header        (64B / 52B)
//! Program Header    (56B / 32B × n)  [仅宿主平台的可执行文件]
//! .shstrtab
//! .text             [R-X]
//! .rodata / .data
//! .symtab / .strtab
//! .rela.text        [仅对象文件]
//! Section Headers   (64B / 40B × m)
//! ```
//!
//! **裸机目标**（`arm-none-eabi`、`riscv32/64-unknown-none`）没有操作系统
//! 加载器，因此不写 Program Header：`e_entry` 直接指向 `_start`，节地址
//! 从 `image_base`（默认 0x08000000）开始，烧录工具 / 仿真器按节加载即可。
//!
//! 设计参考：
//! - Rust：`rustc_codegen_llvm` 通过 LLVM 的 MC 层产出目标文件。
//! - Zig：自带 ELF 链接器，支持增量链接与打包。
//! - Python：`pyinstaller` 打包为可执行文件（思路类比）。
//!
//! 关键不变量：
//! - `sh_addr`（以及 Program Header 的 `p_vaddr`/`p_paddr`）必须是**加载后
//!   的虚拟地址**，而 `sh_offset` 是**文件内偏移**，两者绝不能混用。
//! - 自构造可执行文件时重定位必须先回填；写成目标文件时则要输出
//!   `.rela.text`，把修正工作交给平台链接器。

use std::io::{self, Write};

use crate::codegen::objfile::{align_up, elf_reloc_type};
use crate::codegen::target::{Os, Target};
use crate::codegen::{CompilationUnit, OutputKind, Symbol, SymbolKind, SymbolSection};

/// 页对齐（PT_LOAD 段必须按页对齐）。
const PAGE_ALIGN: u64 = 0x1000;

/// 段对齐（段内数据按 16 字节对齐）。
const SECT_ALIGN: u64 = 16;

/// ELF32 / ELF64 的结构体尺寸。
struct ElfClass {
    ehsize: u64,
    phentsize: u64,
    phnum: u16,
    shentsize: u64,
    /// Sym64/Sym32 条目大小（`.symtab` 的 sh_entsize）。
    sym_entsize: u64,
    /// Rela64/Rela32 条目大小（`.rela.text` 的 sh_entsize）。
    rela_entsize: u64,
}

const ELF64: ElfClass = ElfClass {
    ehsize: 64,
    phentsize: 56,
    phnum: 3,
    shentsize: 64,
    sym_entsize: 24,
    rela_entsize: 24,
};
const ELF32: ElfClass = ElfClass {
    ehsize: 52,
    phentsize: 32,
    phnum: 3,
    shentsize: 40,
    sym_entsize: 16,
    rela_entsize: 12,
};

/// 把编译产物组装为 ELF 字节流。
pub fn assemble_elf(target: &Target, unit: &CompilationUnit) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();

    let is_64 = target.pointer_size() == 8;
    let elf = if is_64 { &ELF64 } else { &ELF32 };

    let text = &unit.code;
    let data = &unit.data;
    let rodata = &unit.rodata;

    // 构建节名表、符号表与字符串表（必须在计算偏移前完成）。
    let shstrtab = Shstrtab::build();
    let (strtab, symtab, local_syms) = build_symbol_tables(unit, is_64);

    // 可执行文件用虚拟地址布局；可重定位目标文件没有加载地址概念。
    let is_exec = unit.kind == OutputKind::Executable;
    // 裸机镜像没有加载器，不需要 Program Header。
    let has_segments = is_exec && target.os() != Os::Bare;

    // 目标文件需要 `.rela.text` 交给链接器应用重定位。
    let rela = if is_exec {
        Vec::new()
    } else {
        build_rela(target, unit, is_64)
    };

    // ---------- 文件偏移布局 ----------
    // 顺序：ELF 头 → Program Header → Section Header 表 → 各节数据。
    // Section Header 表必须整块落在 .shstrtab 之前，否则会与节数据重叠。
    let e_phoff: u64 = elf.ehsize; // ELF header 紧接 Program Header
    let ph_size: u64 = if has_segments {
        elf.phentsize * elf.phnum as u64
    } else {
        0
    };
    let sh_off = align_up(e_phoff + ph_size, SECT_ALIGN);

    // 各段文件偏移
    let shstrtab_off = align_up(sh_off + (SECTION_COUNT as u64 * elf.shentsize), SECT_ALIGN);
    let text_off = align_up(shstrtab_off + shstrtab.len(), SECT_ALIGN);
    let rodata_off = align_up(text_off + text.len() as u64, SECT_ALIGN);
    let data_off = align_up(rodata_off + rodata.len() as u64, SECT_ALIGN);
    let rela_off = align_up(data_off + data.len() as u64, SECT_ALIGN);
    let symtab_off = align_up(rela_off + rela.len() as u64, SECT_ALIGN);
    let strtab_off = align_up(symtab_off + symtab.len() as u64, SECT_ALIGN);

    // ---------- 虚拟地址布局 ----------
    // 三个 PT_LOAD 段共享同一个虚拟基址起点（按页对齐）。
    let text_vaddr = align_up(target.image_base(), PAGE_ALIGN);
    let rodata_vaddr = align_up(text_vaddr + text.len() as u64, PAGE_ALIGN);
    let data_vaddr = align_up(rodata_vaddr + rodata.len() as u64, PAGE_ALIGN);

    // === ELF Header ===
    // ident: magic + 32/64 位标记 + 小端 + 版本
    buf.extend_from_slice(&[0x7f, b'E', b'L', b'F', if is_64 { 2 } else { 1 }, 1, 1, 0]);
    buf.extend_from_slice(&[0; 8]); // padding
    let e_type: u16 = if is_exec { 2 } else { 1 };
    buf.extend_from_slice(&e_type.to_le_bytes());
    buf.extend_from_slice(&target.arch().elf_machine().to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    let e_entry: u64 = if is_exec {
        // 同样：Thumb 入口要置 bit 0 表明指令状态
        let bit = if unit.is_thumb { 1 } else { 0 };
        (text_vaddr + find_entry_offset(unit) as u64) | bit
    } else {
        0
    };
    let e_flags = target.arch().elf_flags();
    // e_entry / e_phoff / e_shoff / e_flags：64 位用 8 字节，32 位用 4 字节
    if is_64 {
        buf.extend_from_slice(&e_entry.to_le_bytes());
        buf.extend_from_slice(&e_phoff.to_le_bytes());
        buf.extend_from_slice(&sh_off.to_le_bytes());
        buf.extend_from_slice(&e_flags.to_le_bytes());
    } else {
        buf.extend_from_slice(&(e_entry as u32).to_le_bytes());
        buf.extend_from_slice(&(e_phoff as u32).to_le_bytes());
        buf.extend_from_slice(&(sh_off as u32).to_le_bytes());
        buf.extend_from_slice(&e_flags.to_le_bytes());
    }
    // 两种 ELF 类的这 6 个字段都是 2 字节
    buf.extend_from_slice(&(elf.ehsize as u16).to_le_bytes()); // e_ehsize
    buf.extend_from_slice(&(elf.phentsize as u16).to_le_bytes()); // e_phentsize
    buf.extend_from_slice(&phnum_of(elf, has_segments).to_le_bytes()); // e_phnum
    buf.extend_from_slice(&(elf.shentsize as u16).to_le_bytes()); // e_shentsize
    buf.extend_from_slice(&(SECTION_COUNT as u16).to_le_bytes()); // e_shnum
    buf.extend_from_slice(&(SHN_SHSTRTAB as u16).to_le_bytes()); // e_shstrndx

    // === Program Headers（仅宿主平台的可执行文件）===
    if has_segments {
        // PT_LOAD: .text (R-X, PF_R | PF_X = 0x5)
        write_program_header(
            &mut buf,
            is_64,
            1,
            0x5,
            text_off,
            text_vaddr,
            text_vaddr,
            text.len() as u64,
            text.len() as u64,
            PAGE_ALIGN,
        )?;
        // PT_LOAD: .rodata (R--, PF_R = 0x4)
        write_program_header(
            &mut buf,
            is_64,
            1,
            0x4,
            rodata_off,
            rodata_vaddr,
            rodata_vaddr,
            rodata.len() as u64,
            rodata.len() as u64,
            PAGE_ALIGN,
        )?;
        // PT_LOAD: .data (RW-, PF_R | PF_W = 0x6)
        write_program_header(
            &mut buf,
            is_64,
            1,
            0x6,
            data_off,
            data_vaddr,
            data_vaddr,
            data.len() as u64,
            data.len() as u64,
            PAGE_ALIGN,
        )?;
    }

    // === Section Headers ===
    // 表整体落在 sh_off 处（必须先补齐到 16 字节边界）。
    pad_to(&mut buf, sh_off as usize)?;
    // idx 0: NULL
    write_section_header(&mut buf, is_64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)?;
    let n_text = shstrtab.offset_of(".text");
    let n_rodata = shstrtab.offset_of(".rodata");
    let n_data = shstrtab.offset_of(".data");
    let n_rela = shstrtab.offset_of(".rela.text");
    let n_symtab = shstrtab.offset_of(".symtab");
    let n_strtab = shstrtab.offset_of(".strtab");
    let n_shstrtab = shstrtab.offset_of(".shstrtab");

    // idx 1: .text
    write_section_header(
        &mut buf,
        is_64,
        n_text,
        1,
        0x6,
        text_vaddr,
        text_off,
        text.len() as u64,
        0,
        0,
        SECT_ALIGN,
        0,
    )?;
    // idx 2: .rodata
    write_section_header(
        &mut buf,
        is_64,
        n_rodata,
        1,
        0x2,
        rodata_vaddr,
        rodata_off,
        rodata.len() as u64,
        0,
        0,
        SECT_ALIGN,
        0,
    )?;
    // idx 3: .data
    write_section_header(
        &mut buf,
        is_64,
        n_data,
        1,
        0x3,
        data_vaddr,
        data_off,
        data.len() as u64,
        0,
        0,
        SECT_ALIGN,
        0,
    )?;
    // idx 4: .rela.text
    // sh_link 必须是 `.symtab` 的**节表索引**（5），不是 `SHN_SYMTAB` 类型常量
    // （2）——写错会让重定位的符号号指向 `.rodata`。
    write_section_header(
        &mut buf,
        is_64,
        n_rela,
        SHT_RELA,
        0,
        0,
        rela_off,
        rela.len() as u64,
        SHN_SYMTAB,
        SHN_TEXT,
        SECT_ALIGN,
        elf.rela_entsize,
    )?;
    // idx 5: .symtab
    // sh_link 指向 `.strtab`（6），sh_info 指向第一个全局符号（下标 local_syms+1）。
    write_section_header(
        &mut buf,
        is_64,
        n_symtab,
        SHT_SYMTAB,
        0,
        0,
        symtab_off,
        symtab.len() as u64,
        SHN_STRTAB,
        local_syms as u32 + 1,
        8,
        elf.sym_entsize,
    )?;
    // idx 6: .strtab
    write_section_header(
        &mut buf,
        is_64,
        n_strtab,
        SHT_STRTAB,
        0,
        0,
        strtab_off,
        strtab.len() as u64,
        0,
        0,
        1,
        0,
    )?;
    // idx 7: .shstrtab
    write_section_header(
        &mut buf,
        is_64,
        n_shstrtab,
        SHT_STRTAB,
        0,
        0,
        shstrtab_off,
        shstrtab.len(),
        0,
        0,
        1,
        0,
    )?;

    // === 段数据 ===
    pad_to(&mut buf, shstrtab_off as usize)?;
    buf.extend_from_slice(&shstrtab.bytes);
    pad_to(&mut buf, text_off as usize)?;
    buf.extend_from_slice(text);
    pad_to(&mut buf, rodata_off as usize)?;
    buf.extend_from_slice(rodata);
    pad_to(&mut buf, data_off as usize)?;
    buf.extend_from_slice(data);
    pad_to(&mut buf, rela_off as usize)?;
    buf.extend_from_slice(&rela);
    pad_to(&mut buf, symtab_off as usize)?;
    buf.extend_from_slice(&symtab);
    pad_to(&mut buf, strtab_off as usize)?;
    buf.extend_from_slice(&strtab);

    Ok(buf)
}

/// 应用重定位后组装并写出 ELF 可执行文件。
///
/// 这是对外入口：先回填所有重定位（此时知道各符号的虚拟地址），
/// 再写出文件。
pub fn emit_elf(target: &Target, unit: &CompilationUnit, path: &std::path::Path) -> io::Result<()> {
    // 重定位作用于 code 段，且必须基于虚拟地址计算 PC 相对偏移。
    let mut unit = unit.clone();
    let text_base = align_up(target.image_base(), PAGE_ALIGN);
    // `.rodata` 紧跟在 `.text` 之后，按页对齐——与 `assemble_elf` 的布局一致。
    let rodata_base = align_up(text_base + unit.code.len() as u64, PAGE_ALIGN);
    crate::codegen::objfile::apply_relocations(&mut unit, text_base, rodata_base);
    let bytes = assemble_elf(target, &unit)?;
    std::fs::write(path, &bytes)
}

/// Program Header 的个数（无加载器的裸机镜像为 0）。
fn phnum_of(elf: &ElfClass, has_segments: bool) -> u16 {
    if has_segments {
        elf.phnum
    } else {
        0
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

/// 写入一个 Program Header（ELF64: 56 字节 / ELF32: 32 字节）。
#[allow(clippy::too_many_arguments)]
fn write_program_header<W: Write>(
    w: &mut W,
    is_64: bool,
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
    if is_64 {
        w.write_all(&p_offset.to_le_bytes())?;
        w.write_all(&p_vaddr.to_le_bytes())?;
        w.write_all(&p_paddr.to_le_bytes())?;
        w.write_all(&p_filesz.to_le_bytes())?;
        w.write_all(&p_memsz.to_le_bytes())?;
        w.write_all(&p_align.to_le_bytes())?;
    } else {
        // ELF32 的地址与大小都是 4 字节，p_flags 也是 4 字节
        w.write_all(&(p_offset as u32).to_le_bytes())?;
        w.write_all(&(p_vaddr as u32).to_le_bytes())?;
        w.write_all(&(p_paddr as u32).to_le_bytes())?;
        w.write_all(&(p_filesz as u32).to_le_bytes())?;
        w.write_all(&(p_memsz as u32).to_le_bytes())?;
        w.write_all(&(p_align as u32).to_le_bytes())?;
    }
    Ok(())
}

/// 写入一个 Section Header（ELF64: 64 字节 / ELF32: 40 字节）。
#[allow(clippy::too_many_arguments)]
fn write_section_header<W: Write>(
    w: &mut W,
    is_64: bool,
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
) -> io::Result<()> {
    w.write_all(&sh_name.to_le_bytes())?;
    w.write_all(&sh_type.to_le_bytes())?;
    if is_64 {
        w.write_all(&sh_flags.to_le_bytes())?;
        w.write_all(&sh_addr.to_le_bytes())?; // 虚拟地址，非文件偏移
        w.write_all(&sh_offset.to_le_bytes())?; // 文件偏移
        w.write_all(&sh_size.to_le_bytes())?;
        w.write_all(&sh_link.to_le_bytes())?;
        w.write_all(&sh_info.to_le_bytes())?;
        w.write_all(&sh_addralign.to_le_bytes())?;
        w.write_all(&sh_entsize.to_le_bytes())?;
    } else {
        w.write_all(&(sh_flags as u32).to_le_bytes())?;
        w.write_all(&(sh_addr as u32).to_le_bytes())?;
        w.write_all(&(sh_offset as u32).to_le_bytes())?;
        w.write_all(&(sh_size as u32).to_le_bytes())?;
        w.write_all(&sh_link.to_le_bytes())?;
        w.write_all(&sh_info.to_le_bytes())?;
        w.write_all(&(sh_addralign as u32).to_le_bytes())?;
        w.write_all(&(sh_entsize as u32).to_le_bytes())?;
    }
    Ok(())
}

/// 节表条目数（含索引 0 的 NULL 节）。
const SECTION_COUNT: usize = 8;

/// 段类型：SHT_RELA / SHT_SYMTAB / SHT_STRTAB。
const SHT_RELA: u32 = 4;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
/// 节头 `sh_link` / `sh_info` 使用的节索引（0 为 NULL 节）。
/// 这些是**节表索引**，和上面的 `SHT_*` 段类型常量完全是两回事。
const SHN_TEXT: u32 = 1;
const SHN_SYMTAB: u32 = 5;
const SHN_STRTAB: u32 = 6;
const SHN_SHSTRTAB: u32 = 7;

/// 节名（节表按此顺序编号，索引 0 保留给 SHT_NULL）。
const SECTION_NAMES: [&str; SECTION_COUNT - 1] = [
    ".text",
    ".rodata",
    ".data",
    ".rela.text",
    ".symtab",
    ".strtab",
    ".shstrtab",
];

/// `.shstrtab` 段：保存节名，并记录每个名字在段内的偏移
/// （节头的 `sh_name` 就是这个偏移，不能用节索引代替）。
struct Shstrtab {
    bytes: Vec<u8>,
}

impl Shstrtab {
    /// 构建节名表。
    fn build() -> Self {
        let mut bytes = vec![0]; // 首项为 NUL
        for name in SECTION_NAMES {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
        }
        Shstrtab { bytes }
    }

    /// 节名在 `.shstrtab` 中的偏移（名字不存在时返回 0）。
    fn offset_of(&self, name: &str) -> u32 {
        let needle: Vec<u8> = name.bytes().chain(std::iter::once(0)).collect();
        self.bytes
            .windows(needle.len())
            .position(|w| w == needle.as_slice())
            .map(|p| p as u32)
            .unwrap_or(0)
    }

    /// `.shstrtab` 段长度。
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// 把缓冲区填充到指定长度（用零填充）。
fn pad_to(w: &mut Vec<u8>, target: usize) -> io::Result<()> {
    if w.len() < target {
        w.resize(target, 0);
    }
    Ok(())
}

/// 把符号转为 ELF 符号表条目（供 `.symtab` 使用）。
/// `name_offset` 是符号名在 `.strtab` 中的偏移。
fn symbol_to_elf(sym: &Symbol, name_offset: u32, is_64: bool, thumb: bool) -> Vec<u8> {
    let mut b = Vec::new();
    // st_info: bind(高4) + type(低4)
    let bind = if sym.global { 1u8 } else { 0u8 }; // STB_GLOBAL or STB_LOCAL
    let ty = match sym.kind {
        SymbolKind::Function => 2u8, // STT_FUNC
        SymbolKind::Object => 1u8,   // STT_OBJECT
        SymbolKind::Section => 3u8,  // STT_SECTION
    };
    let info = (bind << 4) | ty;
    // st_other
    let other = 0u8;
    // st_shndx
    // SHN_TEXT = 1（节表索引 0 保留给 SHT_NULL）。
    let shndx = match sym.section {
        SymbolSection::Text => 1u16,   // 节表索引 1 = .text
        SymbolSection::Rodata => 2u16, // 节表索引 2 = .rodata
        SymbolSection::Undefined => 0,
    };
    // st_value。Thumb 的函数符号要带 bit 0：那是"这个地址是 Thumb 状态"
    // 的标记，缺了它加载器/链接器会当 ARM 状态去取指，直接跑飞。
    let value = match (thumb, sym.kind) {
        (true, SymbolKind::Function) => sym.offset | 1,
        _ => sym.offset,
    };

    // ELF32 和 ELF64 的 `ElfNN_Sym` 字段顺序**不一样**，且都是 16 字节（32 位）/
    // 24 字节（64 位）——写错顺序长度照样对，只有读出来才发现是乱的：
    //
    //   Elf32_Sym: st_name, st_value, st_size, st_info, st_other, st_shndx
    //   Elf64_Sym: st_name, st_info, st_other, st_shndx, st_value, st_size
    //
    // 之前两种都按 64 位顺序写，于是所有 32 位目标（arm / thumb / riscv32）
    // 的符号表全是错位的：名字串到了 st_size，st_shndx 读成 0（*UND*）。
    b.extend_from_slice(&name_offset.to_le_bytes()); // st_name
    if is_64 {
        b.push(info);
        b.push(other);
        b.extend_from_slice(&shndx.to_le_bytes());
        b.extend_from_slice(&(value as u64).to_le_bytes());
        b.extend_from_slice(&(sym.size as u64).to_le_bytes());
    } else {
        b.extend_from_slice(&(value as u32).to_le_bytes());
        b.extend_from_slice(&(sym.size as u32).to_le_bytes());
        b.push(info);
        b.push(other);
        b.extend_from_slice(&shndx.to_le_bytes());
    }
    b
}

/// ELF 规定 `.symtab` 里的局部符号必须全部排在全局符号之前，且
/// `sh_info` 指向第一个全局符号。两处都得用同一个顺序，否则重定位算出来的
/// 符号号会跟符号表对不上。
fn symbols_in_elf_order(unit: &CompilationUnit) -> Vec<&Symbol> {
    let (locals, globals): (Vec<_>, Vec<_>) = unit.symbols.iter().partition(|s| !s.global);
    locals.into_iter().chain(globals).collect()
}

/// 构建 `.strtab`（符号名字符串表）和 `.symtab`（符号表）数据。
///
/// 返回 (strtab_bytes, symtab_bytes, 局部符号数)。
fn build_symbol_tables(unit: &CompilationUnit, is_64: bool) -> (Vec<u8>, Vec<u8>, usize) {
    let mut strtab: Vec<u8> = vec![0]; // 首项为 NUL
    let mut symtab: Vec<u8> = Vec::new();

    // ELF 要求 symtab[0] 是一个全零的未定义符号。
    symtab.extend_from_slice(if is_64 { &[0u8; 24] } else { &[0u8; 16] });

    let ordered = symbols_in_elf_order(unit);
    let local_count = ordered.iter().filter(|s| !s.global).count();
    for sym in ordered {
        // 将符号名追加到 strtab
        let name_offset = strtab.len() as u32;
        strtab.extend_from_slice(sym.name.as_bytes());
        strtab.push(0);
        // 生成符号表条目
        symtab.extend_from_slice(&symbol_to_elf(sym, name_offset, is_64, unit.is_thumb));
    }

    (strtab, symtab, local_count)
}

/// 构建 `.rela.text`（重定位表，仅目标文件需要）。
///
/// 每项 24 字节：`r_offset`（字段在 `.text` 中的偏移）、`r_info`
/// （符号表索引 << 32 | 类型）、`r_addend`。
fn build_rela(target: &Target, unit: &CompilationUnit, is_64: bool) -> Vec<u8> {
    // 符号表索引 0 保留（未定义），因此第 i 个符号对应索引 i + 1。
    let ordered = symbols_in_elf_order(unit);
    let symbol_index = |name: &str| -> u64 {
        ordered
            .iter()
            .position(|s| s.name == name)
            .map(|i| i as u64 + 1)
            .unwrap_or(0)
    };

    let entry = if is_64 { 24 } else { 12 };
    let mut out = Vec::with_capacity(unit.relocations.len() * entry);
    for reloc in &unit.relocations {
        let sym = symbol_index(&reloc.symbol) as u32;
        let rtype = elf_reloc_type(target.arch(), reloc.kind);
        if is_64 {
            out.extend_from_slice(&(reloc.offset as u64).to_le_bytes());
            out.extend_from_slice(&((u64::from(sym) << 32) | u64::from(rtype)).to_le_bytes());
            out.extend_from_slice(&reloc.addend.to_le_bytes());
        } else {
            // ELF32 的 r_info 是 32 位：符号号占高 24 位，类型占低 8 位
            out.extend_from_slice(&(reloc.offset as u32).to_le_bytes());
            out.extend_from_slice(&((sym << 8) | rtype).to_le_bytes());
            out.extend_from_slice(&(reloc.addend as i32).to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::target::{Arch, Os};

    fn target() -> Target {
        Target::new(Arch::X86_64, Os::Linux)
    }

    fn sample_unit(kind: OutputKind) -> CompilationUnit {
        let mut unit = CompilationUnit::new();
        unit.kind = kind;
        unit.code = vec![0xe8, 0, 0, 0, 0]; // call rel32
        unit.symbols.push(Symbol {
            name: "main".to_string(),
            section: SymbolSection::Text,
            offset: 0,
            size: 5,
            global: true,
            kind: SymbolKind::Function,
        });
        unit
    }

    #[test]
    fn rodata_symbol_points_at_rodata_section() {
        let sym = Symbol {
            name: ".Lstr0".to_string(),
            section: SymbolSection::Rodata,
            offset: 0,
            size: 6,
            global: false,
            kind: SymbolKind::Object,
        };
        let b = symbol_to_elf(&sym, 0, true, false);
        // Elf64_Sym 布局：st_name[0..4] st_info[4] st_other[5] st_shndx[6..8] …
        assert_eq!(b[4], 1, "st_info 的类型应为 STT_OBJECT");
        assert_eq!(
            u16::from_le_bytes(b[6..8].try_into().unwrap()),
            2,
            "st_shndx = .rodata"
        );
    }

    #[test]
    fn executable_has_elf_magic_and_entry() {
        let unit = sample_unit(OutputKind::Executable);
        let bytes = assemble_elf(&target(), &unit).unwrap();
        assert_eq!(&bytes[..4], b"\x7fELF");
        assert_eq!(bytes[4], 2, "EI_CLASS = 64 位");
        assert_eq!(bytes[5], 1, "EI_DATA = 小端");
        assert_eq!(
            u16::from_le_bytes(bytes[16..18].try_into().unwrap()),
            2,
            "ET_EXEC"
        );
        assert_eq!(
            u16::from_le_bytes(bytes[18..20].try_into().unwrap()),
            62,
            "EM_X86_64"
        );
        // e_entry 落在 .text 的虚拟地址范围内
        let entry = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        assert_eq!(entry, 0x400000);
        assert_eq!(
            u16::from_le_bytes(bytes[56..58].try_into().unwrap()),
            3,
            "e_phnum（text/rodata/data）"
        );
    }

    #[test]
    fn machine_field_matches_architecture() {
        for (arch, expected) in [(Arch::X86_64, 62u16), (Arch::AArch64, 183u16)] {
            let t = Target::new(arch, Os::Linux);
            let bytes = assemble_elf(&t, &sample_unit(OutputKind::Executable)).unwrap();
            let machine = u16::from_le_bytes(bytes[18..20].try_into().unwrap());
            assert_eq!(machine, expected, "{arch:?}");
        }
    }

    #[test]
    fn relocatable_object_emits_rela_entries() {
        let mut unit = sample_unit(OutputKind::Relocatable);
        unit.relocations.push(crate::codegen::Relocation::new(
            1,
            0,
            5,
            "main",
            crate::codegen::RelocationKind::PcRel32,
        ));
        let bytes = assemble_elf(&target(), &unit).unwrap();
        assert_eq!(
            u16::from_le_bytes(bytes[16..18].try_into().unwrap()),
            1,
            "ET_REL"
        );
        assert_eq!(
            u16::from_le_bytes(bytes[56..58].try_into().unwrap()),
            0,
            "e_phnum"
        );
        assert_eq!(
            u16::from_le_bytes(bytes[60..62].try_into().unwrap()),
            8,
            "e_shnum"
        );

        // .rela.text 数据里应能找到一条指向 main 的重定位
        let rela = build_rela(&target(), &unit, true);
        assert_eq!(rela.len(), 24);
        assert_eq!(
            u64::from_le_bytes(rela[0..8].try_into().unwrap()),
            1,
            "字段偏移"
        );
        let info = u64::from_le_bytes(rela[8..16].try_into().unwrap());
        assert_eq!(info & 0xffff_ffff, 4, "R_X86_64_PLT32");
        assert_eq!(info >> 32, 1, "symtab[1] = main");
    }

    #[test]
    fn section_header_table_does_not_overlap_section_data() {
        let unit = sample_unit(OutputKind::Executable);
        let bytes = assemble_elf(&target(), &unit).unwrap();
        let e_shoff = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
        let e_shnum = u16::from_le_bytes(bytes[60..62].try_into().unwrap()) as usize;
        let e_shentsize = u16::from_le_bytes(bytes[58..60].try_into().unwrap()) as usize;

        // 节头表必须完整落在文件内
        assert!(e_shoff + e_shnum * e_shentsize <= bytes.len());

        // 每个节的 [sh_offset, sh_offset + sh_size) 都必须落在文件内，
        // 且不与节头表重叠。
        let text_off = e_shoff + e_shentsize; // idx 1 = .text
        let (sh_offset, sh_size) = (
            u64::from_le_bytes(bytes[text_off + 24..text_off + 32].try_into().unwrap()) as usize,
            u64::from_le_bytes(bytes[text_off + 32..text_off + 40].try_into().unwrap()) as usize,
        );
        assert!(sh_offset + sh_size <= bytes.len(), ".text 超出文件末尾");
        assert!(
            sh_offset >= e_shoff + e_shnum * e_shentsize,
            ".text 与节头表重叠"
        );

        // sh_name 必须指向 .shstrtab 中的真实名字（不能是节索引）
        let shstrndx = u16::from_le_bytes(bytes[62..64].try_into().unwrap()) as usize;
        let strtab_header = e_shoff + shstrndx * e_shentsize;
        let strtab_off = u64::from_le_bytes(
            bytes[strtab_header + 24..strtab_header + 32]
                .try_into()
                .unwrap(),
        ) as usize;
        let name_off =
            u32::from_le_bytes(bytes[text_off..text_off + 4].try_into().unwrap()) as usize;
        let name_end = bytes[strtab_off + name_off..]
            .iter()
            .position(|b| *b == 0)
            .expect("节名应以 NUL 结尾");
        let name =
            std::str::from_utf8(&bytes[strtab_off + name_off..strtab_off + name_off + name_end])
                .unwrap();
        assert_eq!(name, ".text");
    }

    #[test]
    fn emitted_executable_applies_internal_relocations() {
        let dir = std::env::temp_dir().join("zr-elf-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.out");
        let mut unit = sample_unit(OutputKind::Executable);
        unit.symbols.push(Symbol {
            name: "_start".to_string(),
            section: SymbolSection::Text,
            offset: 0,
            size: 5,
            global: true,
            kind: SymbolKind::Function,
        });
        unit.entry = "_start".to_string();
        unit.relocations.push(crate::codegen::Relocation::new(
            1,
            0,
            5,
            "main",
            crate::codegen::RelocationKind::PcRel32,
        ));
        emit_elf(&target(), &unit, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"\x7fELF");
        let _ = std::fs::remove_file(&path);
    }

    /// ELF32 与 ELF64 的 `ElfNN_Sym` 字段顺序不同，而且两种长度都"看着对"
    /// （16 / 24 字节），写错了 objdump 也照样能读，只是内容全错位。
    /// 这里按字节逐字段断言，把顺序钉死。
    #[test]
    fn symbol_layout_is_correct_for_both_elf_classes() {
        let sym = Symbol {
            name: "f".to_string(),
            section: SymbolSection::Text,
            offset: 0x40,
            size: 0x12,
            global: true,
            kind: SymbolKind::Function,
        };

        // ELF64: st_name, st_info, st_other, st_shndx, st_value(8), st_size(8)
        let e = symbol_to_elf(&sym, 7, true, false);
        assert_eq!(e.len(), 24);
        assert_eq!(&e[0..4], &7u32.to_le_bytes(), "st_name");
        assert_eq!(e[4], 0x12, "st_info = GLOBAL|FUNC");
        assert_eq!(e[5], 0, "st_other");
        assert_eq!(&e[6..8], &1u16.to_le_bytes(), "st_shndx = .text");
        assert_eq!(&e[8..16], &0x40u64.to_le_bytes(), "st_value");
        assert_eq!(&e[16..24], &0x12u64.to_le_bytes(), "st_size");

        // ELF32: st_name, st_value(4), st_size(4), st_info, st_other, st_shndx
        let e = symbol_to_elf(&sym, 7, false, false);
        assert_eq!(e.len(), 16);
        assert_eq!(&e[0..4], &7u32.to_le_bytes(), "st_name");
        assert_eq!(&e[4..8], &0x40u32.to_le_bytes(), "st_value");
        assert_eq!(&e[8..12], &0x12u32.to_le_bytes(), "st_size");
        assert_eq!(e[12], 0x12, "st_info = GLOBAL|FUNC");
        assert_eq!(e[13], 0, "st_other");
        assert_eq!(&e[14..16], &1u16.to_le_bytes(), "st_shndx = .text");
    }

    /// Thumb 函数符号的值要带 bit 0（指令状态标记），否则链接器/加载器
    /// 会按 A32 去取指。A32 目标不受影响。
    #[test]
    fn thumb_function_symbols_carry_the_thumb_bit() {
        let sym = Symbol {
            name: "f".to_string(),
            section: SymbolSection::Text,
            offset: 0x40,
            size: 0x12,
            global: true,
            kind: SymbolKind::Function,
        };
        // ELF32 下 st_value 在 [4..8]
        let e = symbol_to_elf(&sym, 1, false, true);
        assert_eq!(&e[4..8], &0x41u32.to_le_bytes(), "0x40 | 1");
        // 非 Thumb 目标不加位
        let e = symbol_to_elf(&sym, 1, false, false);
        assert_eq!(&e[4..8], &0x40u32.to_le_bytes());
    }

    /// 局部符号必须排在全局符号之前，且 `sh_info` 指向第一个全局符号；
    /// 不满足时重定位算出来的符号号会跟符号表错位。
    #[test]
    fn locals_precede_globals_and_sh_info_points_at_the_first_global() {
        let mut unit = CompilationUnit::new();
        for (name, global) in [("g0", true), ("l0", false), ("g1", true), ("l1", false)] {
            unit.symbols.push(Symbol {
                name: name.to_string(),
                section: SymbolSection::Text,
                offset: 0,
                size: 0,
                global,
                kind: SymbolKind::Section,
            });
        }
        let ordered = symbols_in_elf_order(&unit);
        let names: Vec<&str> = ordered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["l0", "l1", "g0", "g1"]);

        let (_, _, local_count) = build_symbol_tables(&unit, true);
        assert_eq!(local_count, 2, "sh_info = local_count + 1 = 3");
    }
}
