//! 代码生成（Code Generation）。
//!
//! 设计参考：
//! - Rust：MIR → LLVM IR / Cranelift IR，再经后端产出机器码；
//!   `rustc_codegen_llvm` 与 `rustc_codegen_cranelift` 是可选后端。
//! - Zig：自带后端（Aro 汇编器 + 链接器），支持多架构交叉编译。
//! - Python：CPython 生成字节码（栈式虚拟机），JITs（PyPy）另做机器码生成。
//!
//! 流水线：
//! ```text
//! SSA → 指令选择 → 寄存器分配（线性扫描） → 栈帧布局 → 机器码 → 容器格式
//! ```
//!
//! 目标平台矩阵（详见 [`target`]）：
//! | 平台 | 架构 | 指令后端 | 容器格式 | 运行时 |
//! |------|------|----------|----------|--------|
//! | Linux | x86-64 / aarch64 | `encoder` | ELF | syscall |
//! | macOS | x86-64 / aarch64 | `encoder` | Mach-O | libc 薄封装 |
//! | Windows | x86-64 / aarch64 | `encoder` | PE/COFF | Win32 API 薄封装 |
//! | WASI | wasm32 | `wasm` | wasm | 模块内建 |
//!
//! 各平台的启动约定不同：Linux 自带 syscall 启动桩；macOS / Windows
//! 复用运行时的 `zr_exit` 退出进程（无需 libc 启动代码）。

pub mod encoder;
pub mod encoder_aarch64;
pub mod encoder_arm;
pub mod encoder_riscv;
pub mod linker;
pub mod macho;
pub mod objfile;
pub mod pe;
pub mod target;
pub mod wasm;

use std::path::Path;

use crate::ssa::SsaModule;
use target::{Arch, ObjectFormat, Os, Target};

/// 编译产物（可重定位目标文件 / 可执行文件）。
#[derive(Debug, Clone, Default)]
pub struct CompilationUnit {
    /// `.text` 段：机器码字节流。
    pub code: Vec<u8>,
    /// `.data` 段：已初始化数据。
    pub data: Vec<u8>,
    /// `.rodata` 段：只读数据（字符串字面量等）。
    pub rodata: Vec<u8>,
    /// 重定位条目。
    pub relocations: Vec<Relocation>,
    /// 符号表（名称 → 偏移）。
    pub symbols: Vec<Symbol>,
    /// 入口符号名（默认 `_start`，由链接器包装后跳转到 `main`）。
    pub entry: String,
    /// 文件类型。
    pub kind: OutputKind,
}

/// 输出文件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputKind {
    /// 可重定位目标文件（`.o`）。
    #[default]
    Relocatable,
    /// 可执行文件。
    Executable,
}

/// 重定位条目。
#[derive(Debug, Clone)]
pub struct Relocation {
    /// 立即数字段在段内的偏移（PC 相对重定位的计算基准）。
    pub offset: usize,
    /// 指令本身在 `code` 内的起始偏移。
    pub instr_start: usize,
    /// 指令长度（字节），用于计算 `S + A - P` 语义下的下一条指令地址。
    pub instr_len: usize,
    /// 附加到符号值的偏移量。
    pub addend: i64,
    /// 目标符号名。
    pub symbol: String,
    /// 重定位类型。
    pub kind: RelocationKind,
}

impl Relocation {
    /// 创建一条重定位。
    pub fn new(
        offset: usize,
        instr_start: usize,
        instr_len: usize,
        symbol: impl Into<String>,
        kind: RelocationKind,
    ) -> Self {
        Self {
            offset,
            instr_start,
            instr_len,
            addend: 0,
            symbol: symbol.into(),
            kind,
        }
    }
}

/// 重定位类型。
///
/// 每个平台只使用与自身指令编码对应的那一种；其余变体保留在同一枚举中，
/// 使同一份 [`CompilationUnit`] 可以直接交给不同的容器写入器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocationKind {
    /// x86-64：PC 相对 32 位（ELF `R_X86_64_PLT32` / COFF `REL32`）。
    PcRel32,
    /// AArch64：`b` 的 26 位字面量（ELF `R_AARCH64_JUMP26`）。
    Branch26,
    /// AArch64：`bl` 的 26 位字面量（ELF `R_AARCH64_CALL26`）。
    Call26,
    /// ARM：24 位字面量字对齐偏移（`b` / `bl`，ELF `R_ARM_JUMP24`）。
    ArmBranch24,
    /// RISC-V：21 位字对齐偏移（`jal`，ELF `R_RISCV_JAL`）。
    RiscVJal,
    /// RISC-V：13 位半字对齐偏移（条件分支，ELF `R_RISCV_BRANCH`）。
    RiscVBranch,
}

/// 符号。
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub section: SymbolSection,
    pub offset: usize,
    pub size: usize,
    pub global: bool,
    pub kind: SymbolKind,
}

/// 符号所在段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolSection {
    Text,
    Undefined,
}

/// 符号类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Section,
}

impl CompilationUnit {
    /// 创建新的编译产物。
    pub fn new() -> Self {
        Self {
            entry: "_start".to_string(),
            kind: OutputKind::Executable,
            ..Default::default()
        }
    }
}

/// 把一个 SSA 模块编译为机器码。
///
/// `target` 决定指令后端（x86-64 / aarch64）与入口桩的调用约定。
pub fn compile(target: &Target, module: &SsaModule) -> Result<CompilationUnit, String> {
    let mut unit = CompilationUnit::new();

    // 1. 指令选择 + 寄存器分配 + 代码发射（按目标架构分派后端）
    let arch = target.arch();
    for func in &module.functions {
        match arch {
            Arch::X86_64 => encoder::emit_function(target, &mut unit, func),
            Arch::AArch64 => encoder_aarch64::emit_function(target, &mut unit, func),
            Arch::Arm32 => encoder_arm::emit_function(target, &mut unit, func),
            Arch::RiscV32 | Arch::RiscV64 => encoder_riscv::emit_function(target, &mut unit, func),
            Arch::Wasm32 => {
                return Err("wasm32 is handled by `wasm::WasmModule`, not the native backend".into())
            }
        }
    }

    // 2. 若有 `main`，包装入口（调用 main 后退出进程）
    let has_main = module.functions.iter().any(|f| f.name == "main");
    if has_main {
        match arch {
            Arch::X86_64 => encoder::emit_start_wrapper(target, &mut unit),
            Arch::AArch64 => encoder_aarch64::emit_start_wrapper(target, &mut unit),
            Arch::Arm32 => encoder_arm::emit_start_wrapper(target, &mut unit),
            Arch::RiscV32 | Arch::RiscV64 => {
                encoder_riscv::emit_start_wrapper(target, &mut unit);
            }
            Arch::Wasm32 => {}
        }
    } else {
        // 没有 `main` 时入口落在代码段开头（后端仍会登记该符号）。
        let entry = target.entry_symbol().to_string();
        unit.symbols.push(Symbol {
            name: entry.clone(),
            section: SymbolSection::Text,
            offset: 0,
            size: unit.code.len(),
            global: true,
            kind: SymbolKind::Function,
        });
        unit.entry = entry;
    }

    Ok(unit)
}

/// 写出目标文件（`-c`：只编译不链接）。
pub fn emit_object(target: &Target, unit: &CompilationUnit, path: &Path) -> std::io::Result<()> {
    check_target(target)?;
    let mut unit = unit.clone();
    unit.kind = OutputKind::Relocatable;
    // 目标文件不解析重定位：外部符号必须显式声明，交由平台链接器绑定。
    objfile::add_extern_symbols(&mut unit);
    let bytes = match target.format() {
        ObjectFormat::Elf => linker::assemble_elf(target, &unit)?,
        ObjectFormat::MachO => macho::assemble_macho(target, &unit)?,
        ObjectFormat::PeCoff => pe::assemble_coff(target, &unit)?,
        ObjectFormat::Wasm => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "wasm32 has no relocatable object format; emit a module instead",
            ))
        }
    };
    std::fs::write(path, bytes)
}

/// 写出可执行文件 / wasm 模块（对应目标平台的容器格式）。
///
/// 流程：
/// 1. 先把编译产物写成**可重定位目标文件**，含入口、`main` 等符号及
///    `call main` 的重定位；
/// 2. 调用平台链接器（Linux `ld`、macOS/Windows `clang`、Windows
///    `lld-link`）把目标文件与 zr 运行时（`libzrrt.a`）链接为最终产物。
///
/// 若平台链接器不可用（例如缺少交叉工具链），则退回到自行构造容器
/// （只要程序不依赖运行时的 `zr_*` 符号，产物依然可运行）。
pub fn emit_executable(target: &Target, unit: &CompilationUnit, path: &Path) -> std::io::Result<()> {
    check_target(target)?;
    if target.os() == Os::Bare {
        // 裸机映像没有操作系统加载器：既不需要程序头，也不需要链接器，
        // 直接把 `.text` 写成 ELF 可执行映像交给烧录工具 / 模拟器。
        return linker::emit_elf(target, unit, path);
    }
    match target.format() {
        ObjectFormat::Elf => {
            if link_with_system_linker(target, unit, path).is_ok() {
                return Ok(());
            }
            linker::emit_elf(target, unit, path)
        }
        ObjectFormat::MachO | ObjectFormat::PeCoff => {
            if link_with_system_linker(target, unit, path).is_ok() {
                return Ok(());
            }
            // 自行构造的 Mach-O 缺少 dyld 所需的载入信息、PE 缺少导入表，
            // 两者都无法独立运行——与其产出坏文件，不如明确报错。
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "linking {} requires the platform linker (clang/cc on macOS, \
                     lld-link or mingw-gcc on Windows); \
                     use `-c` to emit a relocatable {} object instead",
                    target.describe(),
                    target.format().name(),
                ),
            ))
        }
        ObjectFormat::Wasm => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "wasm32 modules are emitted by `wasm::WasmModule`, not by the native path",
        )),
    }
}

/// 校验目标是否被后端支持（字节序 / 架构）。
fn check_target(target: &Target) -> std::io::Result<()> {
    if !target.is_little_endian() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("big-endian targets are not supported yet: {}", target.triple()),
        ));
    }
    if !target.is_supported() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("unsupported target: {}", target.describe()),
        ));
    }
    Ok(())
}

/// 通过平台链接器把用户程序与运行时链接成可执行文件。
fn link_with_system_linker(
    target: &Target,
    unit: &CompilationUnit,
    output: &Path,
) -> std::io::Result<()> {
    let tmp_obj = output.with_extension(format!("zr{}", target.object_extension()));
    emit_object(target, unit, &tmp_obj)?;

    let result = run_platform_linker(target, &tmp_obj, output);
    let _ = std::fs::remove_file(&tmp_obj);
    result
}

/// 平台链接器命令行。返回 `Ok(())` 表示链接成功。
///
/// 每个平台可能有多种驱动（例如 Windows 既能用 clang 驱动，也能用
/// lld-link），这里按顺序尝试，第一个可用且成功的即被采用。
fn run_platform_linker(target: &Target, obj: &Path, output: &Path) -> std::io::Result<()> {
    // 运行时静态库路径（由 build.rs 通过 `ZR_RUNTIME_LIB` 注入）。
    let runtime_lib = runtime_library(target);
    let mut commands = linker_commands(target, obj, output, runtime_lib.as_deref());

    let mut last_error = String::new();
    for mut cmd in commands.drain(..) {
        let label = format!("`{}`", cmd.get_program().to_string_lossy());
        match cmd.output() {
            Ok(result) if result.status.success() => return Ok(()),
            Ok(result) => {
                let msg = String::from_utf8_lossy(&result.stderr);
                last_error = format!("{label}: {}", msg.trim());
            }
            Err(e) => last_error = format!("{label}: {e}"),
        }
    }

    Err(std::io::Error::other(format!(
        "no working linker driver for {}\nlast error: {last_error}",
        target.describe()
    )))
}

/// 为目标平台列出候选的链接命令。
fn linker_commands(
    target: &Target,
    obj: &Path,
    output: &Path,
    runtime_lib: Option<&str>,
) -> Vec<std::process::Command> {
    let mut commands = Vec::new();

    match target.format() {
        ObjectFormat::Elf => {
            // ld -o out -e _start -static user.o libzrrt-*.a
            let mut c = std::process::Command::new("ld");
            c.arg("-o").arg(output.as_os_str())
                .arg("-e").arg(target.entry_symbol())
                .arg("-static")
                .arg(obj.as_os_str());
            if let Some(lib) = runtime_lib {
                c.arg(lib);
            }
            commands.push(c);

            // gcc 驱动（自带正确的 CRT 与入口约定）
            let mut c = std::process::Command::new("gcc");
            c.arg("-nostdlib").arg("-no-pie")
                .arg("-o").arg(output.as_os_str())
                .arg(obj.as_os_str())
                .arg(format!("-Wl,-e,{}", target.entry_symbol()));
            if let Some(lib) = runtime_lib {
                c.arg(lib);
            }
            commands.push(c);
        }
        ObjectFormat::MachO => {
            // clang / cc 驱动：自动带 libSystem，并用 -e 指定入口符号。
            // 交叉编译（如 Intel → Apple Silicon）时需要显式 -target。
            let host = Target::host();
            let needs_target = host.arch() != target.arch();
            for driver in ["cc", "clang"] {
                let mut c = std::process::Command::new(driver);
                if needs_target && driver == "clang" {
                    c.arg("-target").arg(macho_driver_triple(target));
                }
                c.arg("-o").arg(output.as_os_str())
                    .arg(obj.as_os_str())
                    .arg(format!("-Wl,-e,{}", target.entry_symbol()));
                if let Some(lib) = runtime_lib {
                    c.arg(lib);
                }
                commands.push(c);
            }
        }
        ObjectFormat::PeCoff => {
            // clang 驱动：自动附加 kernel32.lib 等系统导入库
            let mut c = std::process::Command::new("clang");
            c.arg(format!("--target={}", target.triple()))
                .arg("-o").arg(output.as_os_str())
                .arg(obj.as_os_str());
            if let Some(lib) = runtime_lib {
                c.arg(lib);
            }
            commands.push(c);

            // MinGW 驱动
            let driver = match target.arch() {
                Arch::AArch64 => "aarch64-w64-mingw32-gcc",
                _ => "x86_64-w64-mingw32-gcc",
            };
            let mut c = std::process::Command::new(driver);
            c.arg("-nostdlib").arg("-o").arg(output.as_os_str()).arg(obj.as_os_str());
            if let Some(lib) = runtime_lib {
                c.arg(lib);
            }
            commands.push(c);

            // lld-link：显式指定入口与系统库
            let mut c = std::process::Command::new("lld-link");
            c.arg(format!("/out:{}", output.display()))
                .arg(format!("/entry:{}", target.entry_symbol()))
                .arg("/subsystem:console")
                .arg("/nodefaultlib")
                .arg(obj.as_os_str())
                .arg("kernel32.lib");
            if let Some(lib) = runtime_lib {
                c.arg(lib);
            }
            commands.push(c);
        }
        ObjectFormat::Wasm => {}
    }
    commands
}

/// clang 驱动使用的 Mach-O 目标三元组。
fn macho_driver_triple(target: &Target) -> &'static str {
    match target.arch() {
        Arch::X86_64 => "x86_64-apple-macos11",
        Arch::AArch64 => "arm64-apple-macos11",
        Arch::Wasm32 => "wasm32-unknown-unknown",
        // 裸机目标不经过平台链接器，这里只是让穷尽性成立。
        Arch::Arm32 | Arch::RiscV32 | Arch::RiscV64 => "arm-none-eabi",
    }
}

/// zr 运行时的静态库路径（编译 zr 编译器时由 `build.rs` 注入）。
///
/// 只有当目标与宿主一致时该库才可用——交叉编译需要目标平台自己的
/// `libzrrt.a`（否则架构不匹配的 `.o` 会被链接器拒绝）。
fn runtime_library(target: &Target) -> Option<String> {
    // 优先按目标三元组查找（`build.rs` 为每个可用目标都建了运行时）。
    let key = target.canonical_triple();
    if let Some(list) = option_env!("ZR_RUNTIME_LIBS") {
        for entry in list.split(';').filter(|e| !e.is_empty()) {
            if let Some((triple, path)) = entry.split_once('=') {
                if triple == key {
                    return Some(path.to_string());
                }
            }
        }
    }
    // 回退：宿主运行时（`ZR_RUNTIME_LIB`）。
    let path = option_env!("ZR_RUNTIME_LIB")?;
    if path.is_empty() {
        return None;
    }
    let host = Target::host();
    if host.arch() == target.arch() && host.os() == target.os() {
        Some(path.to_string())
    } else {
        None
    }
}

/// 说明当前目标缺少运行时时会缺少什么（用于诊断信息）。
pub fn runtime_diagnostic(target: &Target) -> String {
    match target.os().runtime_source(target.arch()) {
        Some(file) => format!(
            "runtime source `runtime/{file}` ({}) was not built; \
             run `cargo build` on a {}-{} host to produce libzrrt.a",
            target.os().runtime_link_hint(),
            target.arch().name(),
            target.os().name(),
        ),
        None => format!("target {} needs no external runtime", target.describe()),
    }
}