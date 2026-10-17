//! Mach-O 容器格式（macOS：x86-64 与 aarch64）。
//!
//! 产出 64 位 Mach-O 可执行文件 / 目标文件：
//! ```text
//! mach_header_64                     32B
//! LC_SEGMENT_64 __TEXT               内联 __text / __rodata / __data 三个 section
//! LC_MAIN                            入口（文件偏移）—— 仅可执行文件
//! LC_BUILD_VERSION                   平台版本
//! LC_SYMTAB + LC_DYSYMTAB           nlist_64 + 字符串表
//! relocation_info[]                  逐 section 的 reloff/nreloc 指向它
//! ```
//!
//! 与 ELF 的关键差异：
//! - **符号带下划线**：汇编层的 C 符号写作 `_main`、`_write`；
//! - **入口是 LC_MAIN 的文件偏移**，而不是虚拟地址；
//! - **重定位放在 `relocation_info` 数组**中，由所属 section 的
//!   `reloff/nreloc` 指明（不是独立的 `.rela` 段）；
//! - **虚拟地址从 `0x100000000` 起**，且 `__TEXT` 段按 16 KiB 页对齐
//!   （Apple Silicon 的最小页大小），x86-64 侧同样满足 ld64 的对齐要求。
//!
//! 排版与 `clang -c` 在本机生成的真实对象文件逐字段对齐（可用
//! `otool -l` 对照）。

use std::io::{self, Write};

use crate::codegen::objfile::{
    align_up, fixed_name, macho_reloc_is_pcrel, macho_reloc_type, mangle_symbol, MACH_O_R_LEN_4,
};
use crate::codegen::target::{Os, Target};
use crate::codegen::{CompilationUnit, OutputKind, SymbolSection};

// ---- 头常量 ----
const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_OBJECT: u32 = 0x1;
const MH_EXECUTE: u32 = 0x2;
/// MH_NOUNDEFS | MH_DYLDLINK | MH_TWOLEVEL
const MH_FLAGS_EXEC: u32 = 0x0020_0000 | 0x0040_0000 | 0x0080_0000;
/// MH_NOUNDEFS | MH_DYLDLINK（目标文件）
const MH_FLAGS_OBJ: u32 = 0x0000_0001 | 0x0040_0000;

// ---- Load command ----
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_MAIN: u32 = 0x8000_0028;
const LC_BUILD_VERSION: u32 = 0x32;

/// LC_BUILD_VERSION 的 `platform` 字段。
///
/// 写错的后果很隐蔽：平台号决定 dyld 能不能加载、用哪套可用性检查，
/// macOS 映像标成 iOS 在 macOS 上通常仍能加载，但在 iOS 设备上会被拒绝。
const PLATFORM_MACOS: u32 = 1;
const PLATFORM_IOS: u32 = 2;

/// `x.y` 形式的版本号（高 16 位为主版本）。
const VERSION_11_0: u32 = 11 << 16;
const VERSION_14_0: u32 = 14 << 16;

/// section 类型（`s_flags`）。
const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

// ---- 内存保护位 ----
const VM_PROT_READ: i32 = 0x1;
const VM_PROT_EXECUTE: i32 = 0x4;

const SEG_TEXT: &str = "__TEXT";
const SEG_DATA: &str = "__DATA";

/// `nlist_64` 的符号类型位。
const N_EXT: u8 = 0x01;
const N_SECT: u8 = 0x0e;

/// `relocation_info` 条目大小（8 字节）。
const RELOC_SIZE: usize = 8;

/// `mach_header_64` 大小（32 字节）。
const MACH_HEADER_SIZE: u64 = 32;

/// 把编译产物组装为 Mach-O 字节流。
pub fn assemble_macho(target: &Target, unit: &CompilationUnit) -> io::Result<Vec<u8>> {
    let is_exec = unit.kind == OutputKind::Executable;
    let (cputype, cpusubtype) = target.arch().macho_cpu();

    let text = &unit.code;
    let rodata = &unit.rodata;
    let data = &unit.data;
    let nsects: u32 = 3;

    // ---------- 虚拟地址布局（仅可执行文件有意义）----------
    let page = target.page_size();
    let text_vmaddr = align_up(target.image_base(), page);
    let rodata_vmaddr = align_up(text_vmaddr + text.len() as u64, page);
    let data_vmaddr = align_up(rodata_vmaddr + rodata.len() as u64, page);
    let vm_size = align_up(data_vmaddr + data.len() as u64, page) - text_vmaddr;
    let value_base = if is_exec { text_vmaddr } else { 0 };

    // 符号表必须先构建：重定位要引用它在 nlist 中的下标。
    let symtab = Symtab::build(unit, value_base);

    // 目标文件才需要把重定位交给链接器；可执行文件的重定位已就地应用。
    let relo_count = if is_exec {
        0usize
    } else {
        unit.relocations.len()
    };

    // ---------- Load command 组合 ----------
    let sizeofcmds: u32 = 72 + nsects * 80 // LC_SEGMENT_64
        + 24 // LC_SYMTAB
        + 80 // LC_DYSYMTAB
        + 24 // LC_BUILD_VERSION
        + if is_exec { 24 } else { 0 }; // LC_MAIN
    let ncmds: u32 = if is_exec { 5 } else { 4 };

    // ---------- 文件偏移布局 ----------
    // 命令表紧跟在 32 字节的 mach_header_64 之后。
    let text_off = align_up(MACH_HEADER_SIZE + u64::from(sizeofcmds), 16);
    let rodata_off = align_up(text_off + text.len() as u64, 16);
    let data_off = align_up(rodata_off + rodata.len() as u64, 16);
    let symoff = align_up(data_off + data.len() as u64, 8);
    let stroff = symoff + symtab.entries.len() as u64;
    let reloff = align_up(stroff + symtab.strings.len() as u64, 8);
    let total_size = reloff + (relo_count * RELOC_SIZE) as u64;
    let payload_size = data_off + data.len() as u64 - text_off;

    // 目标文件中 section 的 addr 约定与 clang 一致（相对段首，从 0 开始）。
    let (text_addr, rodata_addr, data_addr) = if is_exec {
        (text_vmaddr, rodata_vmaddr, data_vmaddr)
    } else {
        (0, 0, 0)
    };

    let mut buf: Vec<u8> = Vec::with_capacity(total_size as usize);

    // === mach_header_64 ===
    buf.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
    buf.extend_from_slice(&cputype.to_le_bytes());
    buf.extend_from_slice(&cpusubtype.to_le_bytes());
    buf.extend_from_slice(&(if is_exec { MH_EXECUTE } else { MH_OBJECT }).to_le_bytes());
    buf.extend_from_slice(&ncmds.to_le_bytes());
    buf.extend_from_slice(&sizeofcmds.to_le_bytes());
    buf.extend_from_slice(&(if is_exec { MH_FLAGS_EXEC } else { MH_FLAGS_OBJ }).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // reserved

    // === LC_SEGMENT_64 ===
    // 目标文件的段没有加载地址（clang 同样写 0），可执行文件才需要。
    let (seg_vmaddr, seg_vmsize) = if is_exec {
        (text_vmaddr, vm_size)
    } else {
        (0, payload_size)
    };
    write_segment_header(
        &mut buf,
        SEG_TEXT,
        seg_vmaddr,
        seg_vmsize,
        text_off,
        payload_size,
        nsects,
    )?;
    let text_flags = S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS;
    write_section(
        &mut buf,
        "__text",
        SEG_TEXT,
        text_addr,
        text_off,
        text.len() as u64,
        reloff as u32,
        relo_count as u32,
        text_flags,
    )?;
    write_section(
        &mut buf,
        "__rodata",
        SEG_TEXT,
        rodata_addr,
        rodata_off,
        rodata.len() as u64,
        0,
        0,
        0,
    )?;
    write_section(
        &mut buf,
        "__data",
        SEG_DATA,
        data_addr,
        data_off,
        data.len() as u64,
        0,
        0,
        0,
    )?;

    if is_exec {
        // === LC_MAIN：入口点的文件偏移 ===
        buf.extend_from_slice(&LC_MAIN.to_le_bytes());
        buf.extend_from_slice(&24u32.to_le_bytes());
        buf.extend_from_slice(&(text_off + find_entry_offset(unit) as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // stacksize
    }

    // === LC_BUILD_VERSION ===
    buf.extend_from_slice(&LC_BUILD_VERSION.to_le_bytes());
    buf.extend_from_slice(&24u32.to_le_bytes());
    // iOS 与 macOS 共用 Darwin ABI，但平台号必须如实写。
    let platform = if target.os() == Os::Ios {
        PLATFORM_IOS
    } else {
        PLATFORM_MACOS
    };
    buf.extend_from_slice(&platform.to_le_bytes());
    buf.extend_from_slice(&VERSION_11_0.to_le_bytes()); // minos
    buf.extend_from_slice(&VERSION_14_0.to_le_bytes()); // sdk
    buf.extend_from_slice(&0u32.to_le_bytes()); // ntools

    // === LC_SYMTAB ===
    let nsyms = (symtab.entries.len() / 16) as u32;
    buf.extend_from_slice(&LC_SYMTAB.to_le_bytes());
    buf.extend_from_slice(&24u32.to_le_bytes());
    buf.extend_from_slice(&(symoff as u32).to_le_bytes());
    buf.extend_from_slice(&nsyms.to_le_bytes());
    buf.extend_from_slice(&(stroff as u32).to_le_bytes());
    buf.extend_from_slice(&(symtab.strings.len() as u32).to_le_bytes());

    // === LC_DYSYMTAB ===
    // 目标文件的重定位由各 section 的 reloff/nreloc 描述，
    // 全局 extrel 表必须留空，否则与 section 重定位区重叠。
    let (ilocalsym, nlocalsym, iextdefsym, nextdefsym, iundefsym, nundefsym) = symtab.ranges;
    let (extreloff, nextrel) = if is_exec {
        (reloff as u32, relo_count as u32)
    } else {
        (0, 0)
    };
    write_dysymtab(
        &mut buf, ilocalsym, nlocalsym, iextdefsym, nextdefsym, iundefsym, nundefsym, extreloff,
        nextrel,
    )?;

    // === 段数据 ===
    pad_to(&mut buf, text_off as usize)?;
    buf.extend_from_slice(text);
    pad_to(&mut buf, rodata_off as usize)?;
    buf.extend_from_slice(rodata);
    pad_to(&mut buf, data_off as usize)?;
    buf.extend_from_slice(data);
    pad_to(&mut buf, symoff as usize)?;
    buf.extend_from_slice(&symtab.entries);
    buf.extend_from_slice(&symtab.strings);
    pad_to(&mut buf, reloff as usize)?;
    if relo_count > 0 {
        write_relocations(target, &mut buf, unit, &symtab.order)?;
    }

    if buf.len() as u64 != total_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "internal Mach-O layout error: wrote {} bytes, expected {total_size}",
                buf.len()
            ),
        ));
    }
    Ok(buf)
}

/// 查找入口符号在代码段中的偏移（`main` / `_main` 均可）。
fn find_entry_offset(unit: &CompilationUnit) -> usize {
    let entry = mangle_symbol(true, &unit.entry);
    unit.symbols
        .iter()
        .find(|s| s.name == unit.entry || s.name == entry)
        .map(|s| s.offset)
        .unwrap_or(0)
}

/// 写入 `segment_command_64`（72 字节 + 内联 section 表）。
fn write_segment_header<W: Write>(
    w: &mut W,
    segname: &str,
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    nsects: u32,
) -> io::Result<()> {
    let maxprot = (VM_PROT_READ | VM_PROT_EXECUTE) as u32;
    w.write_all(&LC_SEGMENT_64.to_le_bytes())?;
    w.write_all(&(72 + nsects * 80).to_le_bytes())?;
    w.write_all(&fixed_name(segname))?;
    w.write_all(&vmaddr.to_le_bytes())?;
    w.write_all(&vmsize.to_le_bytes())?;
    w.write_all(&fileoff.to_le_bytes())?;
    w.write_all(&filesize.to_le_bytes())?;
    w.write_all(&maxprot.to_le_bytes())?;
    w.write_all(&maxprot.to_le_bytes())?;
    w.write_all(&nsects.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // flags
    Ok(())
}

/// 写入 `section_64`（80 字节）。
#[allow(clippy::too_many_arguments)]
fn write_section<W: Write>(
    w: &mut W,
    sectname: &str,
    segname: &str,
    addr: u64,
    offset: u64,
    size: u64,
    reloff: u32,
    nreloc: u32,
    flags: u32,
) -> io::Result<()> {
    w.write_all(&fixed_name(sectname))?;
    w.write_all(&fixed_name(segname))?;
    w.write_all(&addr.to_le_bytes())?;
    w.write_all(&size.to_le_bytes())?;
    w.write_all(&(offset as u32).to_le_bytes())?;
    w.write_all(&4u32.to_le_bytes())?; // align = 2^4 = 16 字节
    w.write_all(&reloff.to_le_bytes())?;
    w.write_all(&nreloc.to_le_bytes())?;
    w.write_all(&flags.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // reserved1
    w.write_all(&0u32.to_le_bytes())?; // reserved2
    w.write_all(&0u32.to_le_bytes())?; // reserved3
    Ok(())
}

/// 写入 `dysymtab_command`（80 字节）。
#[allow(clippy::too_many_arguments)]
fn write_dysymtab<W: Write>(
    w: &mut W,
    ilocalsym: u32,
    nlocalsym: u32,
    iextdefsym: u32,
    nextdefsym: u32,
    iundefsym: u32,
    nundefsym: u32,
    extreloff: u32,
    nextrel: u32,
) -> io::Result<()> {
    w.write_all(&LC_DYSYMTAB.to_le_bytes())?;
    w.write_all(&80u32.to_le_bytes())?;
    w.write_all(&ilocalsym.to_le_bytes())?;
    w.write_all(&nlocalsym.to_le_bytes())?;
    w.write_all(&iextdefsym.to_le_bytes())?;
    w.write_all(&nextdefsym.to_le_bytes())?;
    w.write_all(&iundefsym.to_le_bytes())?;
    w.write_all(&nundefsym.to_le_bytes())?;
    for zero in [0u32, 0, 0, 0, 0, 0, 0, 0] {
        w.write_all(&zero.to_le_bytes())?;
    }
    w.write_all(&extreloff.to_le_bytes())?;
    w.write_all(&nextrel.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // locreloff
    w.write_all(&0u32.to_le_bytes())?; // nlocrel
    Ok(())
}

/// Mach-O 符号表（nlist_64 数组 + 字符串表）。
struct Symtab {
    /// 已排序并序列化的 nlist_64 条目（每条 16 字节）。
    entries: Vec<u8>,
    /// 字符串表（首字节为 NUL）。
    strings: Vec<u8>,
    /// 与 `entries` 同序的符号，重定位按此下标引用。
    order: Vec<String>,
    /// (ilocalsym, nlocalsym, iextdefsym, nextdefsym, iundefsym, nundefsym)
    ranges: (u32, u32, u32, u32, u32, u32),
}

impl Symtab {
    /// 按"局部 → 外部已定义 → 外部未定义"排序构建符号表。
    ///
    /// `value_base` 是符号值的基址：可执行文件为 `.text` 的虚拟地址，
    /// 目标文件为 0（`n_value` 为段内偏移）。
    fn build(unit: &CompilationUnit, value_base: u64) -> Symtab {
        let mut strings: Vec<u8> = vec![0];
        // 三个桶与 nlist 的最终顺序一一对应，重定位下标也据此计算。
        let mut buckets: [Vec<u8>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut names: [Vec<String>; 3] = [Vec::new(), Vec::new(), Vec::new()];

        for sym in &unit.symbols {
            let name = mangle_symbol(true, &sym.name);
            let strx = strings.len() as u32;
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);

            let mut entry = Vec::with_capacity(16);
            entry.extend_from_slice(&strx.to_le_bytes());
            match sym.section {
                SymbolSection::Undefined => {
                    entry.push(N_EXT); // N_UNDF | N_EXT
                    entry.push(0); // n_sect
                    entry.extend_from_slice(&0u16.to_le_bytes()); // n_desc
                    entry.extend_from_slice(&0u64.to_le_bytes()); // n_value
                    push_bucket(&mut buckets, &mut names, 2, &entry, name);
                }
                SymbolSection::Text | SymbolSection::Rodata => {
                    entry.push(if sym.global { N_SECT | N_EXT } else { N_SECT });
                    // n_sect：1 = __text，2 = __rodata（与 section 表顺序一致）。
                    entry.push(match sym.section {
                        SymbolSection::Rodata => 2,
                        _ => 1,
                    });
                    entry.extend_from_slice(&0u16.to_le_bytes());
                    entry.extend_from_slice(&(value_base + sym.offset as u64).to_le_bytes());
                    // bucket 0 = 局部符号，bucket 1 = 外部已定义符号
                    let bucket = usize::from(sym.global);
                    push_bucket(&mut buckets, &mut names, bucket, &entry, name);
                }
            }
        }

        let counts: Vec<u32> = buckets.iter().map(|b| (b.len() / 16) as u32).collect();
        let [nlocals, nextdef, nundef] = [counts[0], counts[1], counts[2]];

        let mut entries = buckets[0].clone();
        entries.extend_from_slice(&buckets[1]);
        entries.extend_from_slice(&buckets[2]);

        let mut order = names[0].clone();
        order.extend(names[1].iter().cloned());
        order.extend(names[2].iter().cloned());

        Symtab {
            entries,
            strings,
            order,
            ranges: (0, nlocals, nlocals, nextdef, nlocals + nextdef, nundef),
        }
    }
}

/// 把一个 nlist 条目压入对应的桶。
fn push_bucket(
    buckets: &mut [Vec<u8>; 3],
    names: &mut [Vec<String>; 3],
    index: usize,
    entry: &[u8],
    name: String,
) {
    buckets[index].extend_from_slice(entry);
    names[index].push(name);
}

/// 把缓冲区填充到指定长度（用零填充）。
fn pad_to(buf: &mut Vec<u8>, target: usize) -> io::Result<()> {
    if buf.len() < target {
        buf.resize(target, 0);
    }
    Ok(())
}

/// 写入 `relocation_info` 数组（每项 8 字节）。
///
/// 条目布局（见 `<mach-o/reloc.h>`）：
/// ```text
/// u32 r_address
/// u32 r_symbolnum:24 | r_pcrel:1 | r_length:2 | r_extern:1 | r_type:4
/// ```
/// `r_length` 使用编码值（2 = 4 字节）。
fn write_relocations<W: Write>(
    target: &Target,
    w: &mut W,
    unit: &CompilationUnit,
    order: &[String],
) -> io::Result<()> {
    for reloc in &unit.relocations {
        let symbol = mangle_symbol(true, &reloc.symbol);
        let index = order.iter().position(|s| *s == symbol).unwrap_or(0) as u32;
        let packed = (index & 0x00ff_ffff)
            | (u32::from(macho_reloc_is_pcrel(reloc.kind)) << 24) // r_pcrel
            | ((MACH_O_R_LEN_4 as u32) << 25) // r_length
            | (1 << 27) // r_extern
            | ((macho_reloc_type(target.arch(), reloc.kind) as u32) << 28); // r_type

        w.write_all(&(reloc.offset as u32).to_le_bytes())?; // r_address（段内偏移）
        w.write_all(&packed.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::target::{Arch, Os};
    use crate::codegen::{Relocation, RelocationKind, Symbol, SymbolKind};

    fn target(arch: Arch) -> Target {
        Target::new(arch, Os::Darwin)
    }

    fn sample_unit(kind: OutputKind) -> CompilationUnit {
        let mut unit = CompilationUnit::new();
        unit.kind = kind;
        unit.code = vec![0x20, 0x00, 0x80, 0xd2]; // mov x0, #0
        unit.entry = "_main".to_string();
        unit.symbols.push(Symbol {
            name: "main".to_string(),
            section: SymbolSection::Text,
            offset: 0,
            size: 4,
            global: true,
            kind: SymbolKind::Function,
        });
        unit.symbols.push(Symbol {
            name: "bb0".to_string(),
            section: SymbolSection::Text,
            offset: 0,
            size: 4,
            global: false,
            kind: SymbolKind::Section,
        });
        unit
    }

    #[test]
    fn header_reports_macho64_and_arm64() {
        let unit = sample_unit(OutputKind::Executable);
        let bytes = assemble_macho(&target(Arch::AArch64), &unit).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            0xfeed_facf
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            0x0100_000c
        );
        assert_eq!(
            u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            MH_EXECUTE
        );
    }

    #[test]
    fn x86_64_target_uses_x86_cpu_type() {
        let unit = sample_unit(OutputKind::Executable);
        let bytes = assemble_macho(&target(Arch::X86_64), &unit).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            0x0100_0007
        );
    }

    #[test]
    fn command_sizes_are_self_consistent() {
        let unit = sample_unit(OutputKind::Executable);
        let bytes = assemble_macho(&target(Arch::AArch64), &unit).unwrap();
        let sizeofcmds = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
        assert_eq!(sizeofcmds, 72 + 3 * 80 + 24 + 80 + 24 + 24);
        let ncmds = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        assert_eq!(ncmds, 5);
    }

    #[test]
    fn object_declares_sections_and_undefined_symbols() {
        let mut unit = sample_unit(OutputKind::Relocatable);
        unit.relocations.push(Relocation::new(
            0,
            0,
            4,
            "zr_exit",
            RelocationKind::Branch26,
        ));
        crate::codegen::objfile::add_extern_symbols(&mut unit);
        let bytes = assemble_macho(&target(Arch::AArch64), &unit).unwrap();
        let ncmds = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        assert_eq!(ncmds, 4, "segment + build_version + symtab + dysymtab");
        assert!(
            bytes.windows(7).any(|w| w == b"_zr_exi"),
            "应出现未定义符号名"
        );
    }

    fn index_of(symtab: &Symtab, name: &str) -> u32 {
        symtab.order.iter().position(|s| s == name).unwrap() as u32
    }

    #[test]
    fn relocation_symbol_index_matches_serialised_symtab() {
        // 回归测试：符号表按"局部 → 外部已定义 → 外部未定义"排序，
        // 重定位里的 r_symbolnum 必须指向排序后的 nlist 下标，
        // 否则链接器会把 `call main` 解析成别的符号。
        let mut unit = sample_unit(OutputKind::Relocatable);
        unit.relocations
            .push(Relocation::new(0, 0, 4, "main", RelocationKind::Branch26));
        crate::codegen::objfile::add_extern_symbols(&mut unit);

        let symtab = Symtab::build(&unit, 0);
        let mut out = Vec::new();
        write_relocations(&target(Arch::AArch64), &mut out, &unit, &symtab.order).unwrap();
        let packed = u32::from_le_bytes(out[4..8].try_into().unwrap());
        assert_eq!(packed & 0x00ff_ffff, index_of(&symtab, "_main"));

        // nlist 顺序：局部符号在前，外部已定义其次，未定义最后
        assert_eq!(symtab.order[0], "_bb0");
        assert_eq!(symtab.order[1], "_main");
        assert_eq!(symtab.ranges.0, 0);
        assert_eq!(symtab.ranges.1, 1, "1 个局部符号");
    }

    #[test]
    fn relocation_entries_use_branch26_type() {
        let mut unit = sample_unit(OutputKind::Relocatable);
        unit.relocations
            .push(Relocation::new(0, 0, 4, "foo", RelocationKind::Branch26));
        crate::codegen::objfile::add_extern_symbols(&mut unit);
        let symtab = Symtab::build(&unit, 0);
        let mut out = Vec::new();
        write_relocations(&target(Arch::AArch64), &mut out, &unit, &symtab.order).unwrap();
        assert_eq!(out.len(), RELOC_SIZE);
        assert_eq!(
            u32::from_le_bytes(out[0..4].try_into().unwrap()),
            0,
            "r_address"
        );
        let packed = u32::from_le_bytes(out[4..8].try_into().unwrap());
        assert_eq!(
            packed & 0x00ff_ffff,
            index_of(&symtab, "_foo"),
            "r_symbolnum"
        );
        assert_eq!((packed >> 24) & 1, 1, "r_pcrel");
        assert_eq!((packed >> 25) & 3, 2, "r_length = 4 字节");
        assert_eq!((packed >> 27) & 1, 1, "r_extern");
        assert_eq!((packed >> 28) & 0xf, 2, "ARM64_RELOC_BRANCH26");
    }

    #[test]
    fn string_relocations_use_signed_page21_pageoff12() {
        use crate::codegen::objfile::{macho_reloc_is_pcrel, macho_reloc_type};
        // clang: `lea` → SIGNED，`adrp` → PAGE21，`add :lo12:` → PAGEOFFSET12。
        assert_eq!(
            macho_reloc_type(Arch::X86_64, RelocationKind::RipRel32),
            1,
            "X86_64_RELOC_SIGNED"
        );
        assert_eq!(
            macho_reloc_type(Arch::AArch64, RelocationKind::AdrPrelPage21),
            3,
            "ARM64_RELOC_PAGE21"
        );
        assert_eq!(
            macho_reloc_type(Arch::AArch64, RelocationKind::AddAbsLo12),
            4,
            "ARM64_RELOC_PAGEOFFSET12"
        );
        assert!(macho_reloc_is_pcrel(RelocationKind::RipRel32));
        assert!(macho_reloc_is_pcrel(RelocationKind::AdrPrelPage21));
        assert!(
            !macho_reloc_is_pcrel(RelocationKind::AddAbsLo12),
            "页内偏移不是 PC 相对"
        );
    }

    #[test]
    fn string_address_relocation_entries_carry_types_and_pcrel() {
        let mut unit = sample_unit(OutputKind::Relocatable);
        unit.symbols.push(Symbol {
            name: ".Lstr0".to_string(),
            section: SymbolSection::Rodata,
            offset: 0,
            size: 6,
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
        let symtab = Symtab::build(&unit, 0);
        let mut out = Vec::new();
        write_relocations(&target(Arch::AArch64), &mut out, &unit, &symtab.order).unwrap();

        let adrp = u32::from_le_bytes(out[4..8].try_into().unwrap());
        assert_eq!((adrp >> 28) & 0xf, 3, "PAGE21");
        assert_eq!((adrp >> 24) & 1, 1, "r_pcrel");
        let add = u32::from_le_bytes(out[12..16].try_into().unwrap());
        assert_eq!((add >> 28) & 0xf, 4, "PAGEOFFSET12");
        assert_eq!((add >> 24) & 1, 0, "lo12 非 r_pcrel");
    }
}
