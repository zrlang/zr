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
pub mod encoder_thumb;
pub mod linker;
pub mod macho;
pub mod objfile;
pub mod pe;
pub mod regalloc;
pub mod target;
pub mod wasm;

use std::path::Path;

use crate::ssa::{SsaConst, SsaFunction, SsaInstr, SsaModule};
use target::{Arch, ObjectFormat, Os, Target};

/// 编译产物（可重定位目标文件 / 可执行文件）。
#[derive(Debug, Clone, Default)]
pub struct CompilationUnit {
    /// 目标是不是 Thumb-2 编码的 ARM 32 位。
    ///
    /// 链接写符号表 / 入口地址时要置 bit 0——那是"该地址是 Thumb 状态"
    /// 的标记，缺了加载器会按 A32 取指。
    pub is_thumb: bool,
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
    /// 链接指令（`#[link(name = "c", kind = "static")]`）。
    pub link_directives: Vec<crate::attributes::LinkDirective>,
    /// 结构体 repr 布局信息（`#[repr(C)]` / `#[repr(packed)]` 等）。
    pub struct_reprs: Vec<(String, crate::typeck::hir::Repr)>,
    /// 字符串字面量去重表：文本 → `.rodata` 中的局部符号名。
    ///
    /// 相同文本只存一份字节，符号名形如 `.Lstr0`。用 `CompilationUnit`
    /// 上的字段（而不是全局计数）保证同一份产物里的编号确定，且跨编译
    /// 互不影响。
    pub string_pool: std::collections::HashMap<String, String>,
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
    ///
    /// 用于 `call`/`jmp`/`jcc`：位移基准是**下一条指令**，因此自构造
    /// 可执行文件时按 `instr_start + instr_len` 回填，目标文件则由链接器
    /// 依 `S + A - P` 处理。
    PcRel32,
    /// x86-64：`lea reg, [rip+disp32]` 的 32 位位移（ELF `R_X86_64_PC32`）。
    ///
    /// 与 `PcRel32` 分开：它加载的是**符号地址**（字符串字面量、将来的
    /// 静态数据），链接器要的是 `PC32` 而不是 `PLT32`，附加数是 `-4`。
    RipRel32,
    /// AArch64：`adrp` 的 21 位页差（ELF `R_AARCH64_ADR_PREL_PG_HI21`）。
    AdrPrelPage21,
    /// AArch64：`add Xd, Xn, #imm12` 的 12 位页内偏移
    /// （ELF `R_AARCH64_ADD_ABS_LO12_NC`）。
    AddAbsLo12,
    /// AArch64：`b` 的 26 位字面量（ELF `R_AARCH64_JUMP26`）。
    Branch26,
    /// AArch64：`bl` 的 26 位字面量（ELF `R_AARCH64_CALL26`）。
    Call26,
    /// ARM：24 位字面量字对齐偏移（`b` / `bl`，ELF `R_ARM_JUMP24`）。
    ArmBranch24,
    /// Thumb-2 `bl`（T1）：PC 相对半字偏移 + S 位。
    ///
    /// 与 A32 的 `ArmBranch24` 分开：Thumb 的偏移单位是**半字**且带一个
    /// 额外的高位符号位，用错重定位类型会把调用跳到垃圾地址。
    ArmCallThumb,
    /// Thumb-2 `b`（T4）/ `b<cond>`（T1）：PC 相对半字偏移。
    ArmBranchThumb,
    /// Thumb `cbz` / `cbnz`：8 位半字偏移，落在指令的 [9:2]，且只支持 r0-r3。
    ArmCbzThumb,
    /// ARM32（A32）`movw rd, #imm16`：绝对取址对的下半（ELF `R_ARM_MOVW_ABS_NC`）。
    ///
    /// 立即数取 `(S + A) & 0xffff`，拆成 imm4[19:16] 与 imm12[11:0]。
    ArmMovwAbs,
    /// ARM32（A32）`movt rd, #imm16`：绝对取址对的上半（ELF `R_ARM_MOVT_ABS`）。
    ///
    /// 立即数取 `(S + A) >> 16`，与 [`RelocationKind::ArmMovwAbs`] 成对。
    ArmMovtAbs,
    /// Thumb `movw rd, #imm16`：绝对取址对的下半（ELF `R_ARM_THM_MOVW_ABS_NC`）。
    ///
    /// 立即数取 `(S + A) & 0xffff`，散布在 `i`/`imm4`/`imm3`/`imm8` 四处。
    ThumbMovwAbs,
    /// Thumb `movt rd, #imm16`：绝对取址对的上半（ELF `R_ARM_THM_MOVT_ABS`）。
    ///
    /// 立即数取 `(S + A) >> 16`，与 [`RelocationKind::ThumbMovwAbs`] 成对。
    ThumbMovtAbs,
    /// RISC-V：21 位字对齐偏移（`jal`，ELF `R_RISCV_JAL`）。
    RiscVJal,
    /// RISC-V：13 位半字对齐偏移（条件分支，ELF `R_RISCV_BRANCH`）。
    RiscVBranch,
    /// RISC-V：`lui rd, %hi(sym)` 的高 20 位（ELF `R_RISCV_HI20`）。
    ///
    /// `lui + addi %lo` 的绝对取址对——符号直引（REL 型），自链接与
    /// GNU ld 都按 `S + A` 处理；`hi` 取 `(S + 0x800) >> 12` 的标准
    /// 进位舍入，与负的 `lo12` 相互抵消。
    RiscVHi20,
    /// RISC-V：`addi rd, rd, %lo(sym)` 的符号扩展 imm12（ELF
    /// `R_RISCV_LO12_I`），与 [`RelocationKind::RiscVHi20`] 成对。
    RiscVLo12I,
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
    /// `.text`：代码。
    Text,
    /// `.rodata`：只读数据（字符串字面量等）。
    Rodata,
    /// 未定义：外部符号，交由平台链接器解析。
    Undefined,
}

/// 符号类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// 函数（ELF `STT_FUNC`）。
    Function,
    /// 数据对象（ELF `STT_OBJECT`），例如 `.rodata` 里的字符串字面量。
    Object,
    /// 段符号（ELF `STT_SECTION`），用于基本块标签等。
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

    /// 把字符串字面量的字节放进 `.rodata`，返回其符号名。
    ///
    /// 字节后跟一个 NUL 终止符（C 字符串约定）；相同文本只存一份，因此
    /// 重复字面量不会把 `.rodata` 撑大。
    pub fn intern_string(&mut self, text: &str) -> String {
        if let Some(name) = self.string_pool.get(text) {
            return name.clone();
        }
        let name = format!(".Lstr{}", self.string_pool.len());
        let offset = self.rodata.len();
        self.rodata.extend_from_slice(text.as_bytes());
        self.rodata.push(0);
        self.symbols.push(Symbol {
            name: name.clone(),
            section: SymbolSection::Rodata,
            offset,
            size: text.len() + 1,
            global: false,
            kind: SymbolKind::Object,
        });
        self.string_pool.insert(text.to_string(), name.clone());
        name
    }
}

/// 把一个 SSA 模块编译为机器码。
///
/// `target` 决定指令后端（x86-64 / aarch64 / …）与入口桩的调用约定。
///
/// **多线程编译**：指令选择 / 寄存器分配 / 代码发射是**逐函数独立**的
/// （活跃区间、栈帧布局都局限在函数内），因此按函数并行发射。为了拿到
/// 逐字节可复现的产物，字符串字面量在并行之前按确定顺序统一登记进
/// `.rodata`，各线程只做查表；片段按函数顺序（连续分块）合并，符号 /
/// 重定位顺序与单线程一致。
pub fn compile(target: &Target, module: &SsaModule) -> Result<CompilationUnit, String> {
    let mut unit = CompilationUnit::new();
    unit.is_thumb = target.is_arm32_thumb();

    // 0. 先把所有字符串字面量按确定顺序登记进 `.rodata`。并行发射时每个
    //    函数只查表（`intern_string` 命中已有条目），不再改共享的字符串池，
    //    编号与单线程顺序完全一致。
    intern_module_strings(&mut unit, module);

    // 1. 指令选择 + 寄存器分配 + 代码发射（按目标架构分派后端）
    let arch = target.arch();
    let workers = codegen_workers(module.functions.len());
    if workers > 1 {
        emit_functions_parallel(target, arch, module, &mut unit, workers);
    } else {
        for func in &module.functions {
            emit_one(target, arch, &mut unit, func);
        }
    }

    // 2. 若有 `main`，包装入口（调用 main 后退出进程）
    let has_main = module.functions.iter().any(|f| f.name == "main");
    let has_spawn = unit.relocations.iter().any(|r| r.symbol == "zr_spawn");
    if has_spawn && arch != Arch::X86_64 {
        return Err(format!(
            "`spawn` requires x86-64 function pointers; target {} is not supported yet",
            target.describe()
        ));
    }
    if has_main {
        match arch {
            Arch::X86_64 => encoder::emit_start_wrapper(target, &mut unit, has_spawn),
            Arch::AArch64 => encoder_aarch64::emit_start_wrapper(target, &mut unit),
            Arch::Arm32 | Arch::Arm32Thumb => {
                if target.is_arm32_thumb() {
                    encoder_thumb::emit_start_wrapper(target, &mut unit)
                } else {
                    encoder_arm::emit_start_wrapper(target, &mut unit)
                }
            }
            Arch::RiscV32 | Arch::RiscV64 => {
                encoder_riscv::emit_start_wrapper(target, &mut unit);
            }
            Arch::Wasm32 | Arch::Wasm64 => {}
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

/// 一个函数分派给对应架构的后端发射。
fn emit_one(target: &Target, arch: Arch, unit: &mut CompilationUnit, func: &SsaFunction) {
    match arch {
        Arch::X86_64 => encoder::emit_function(target, unit, func),
        Arch::AArch64 => encoder_aarch64::emit_function(target, unit, func),
        Arch::Arm32 | Arch::Arm32Thumb => {
            if target.is_arm32_thumb() {
                encoder_thumb::emit_function(target, unit, func)
            } else {
                encoder_arm::emit_function(target, unit, func)
            }
        }
        Arch::RiscV32 | Arch::RiscV64 => encoder_riscv::emit_function(target, unit, func),
        Arch::Wasm32 | Arch::Wasm64 => {
            unreachable!("wasm 由 `wasm::WasmModule` 处理，不走原生后端")
        }
    }
}

/// 并行代码生成的线程数。`ZR_THREADS` 可覆盖（`1` 关闭并行）。
///
/// 函数太少时并行得不偿失（线程启动开销大于收益），低于两函数一律串行。
fn codegen_workers(func_count: usize) -> usize {
    if func_count < 2 {
        return 1;
    }
    if let Some(override_n) = std::env::var_os("ZR_THREADS") {
        if let Ok(k) = override_n.to_string_lossy().parse::<usize>() {
            return k.clamp(1, func_count);
        }
    }
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    hw.min(MAX_CODEGEN_WORKERS).clamp(1, func_count)
}

/// 并行线程上限（避免在核数很多的机器上为一个模块开过多线程）。
const MAX_CODEGEN_WORKERS: usize = 8;

/// 按确定顺序把所有 `SsaConst::Str` 登记进 `.rodata`。
fn intern_module_strings(unit: &mut CompilationUnit, module: &SsaModule) {
    for func in &module.functions {
        for block in &func.blocks {
            for instr in &block.instrs {
                if let SsaInstr::Const {
                    value: SsaConst::Str(text),
                    ..
                } = instr
                {
                    unit.intern_string(text);
                }
            }
        }
    }
}

/// 按函数连续分块、并行发射，再按块顺序合并——符号 / 重定位顺序与串行一致。
fn emit_functions_parallel(
    target: &Target,
    arch: Arch,
    module: &SsaModule,
    unit: &mut CompilationUnit,
    workers: usize,
) {
    let funcs: &[SsaFunction] = &module.functions;
    let chunk = funcs.len().div_ceil(workers);
    let is_thumb = target.is_arm32_thumb();
    let pool = unit.string_pool.clone();
    if std::env::var_os("ZR_CODEGEN_DEBUG").is_some() {
        eprintln!(
            "[codegen] {} funcs, {} workers, chunk {}",
            funcs.len(),
            workers,
            chunk
        );
        for (i, f) in funcs.iter().enumerate() {
            eprintln!("  func[{i}] = {}", f.name);
        }
    }

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for t in 0..workers {
            let lo = t * chunk;
            let hi = ((t + 1) * chunk).min(funcs.len());
            if lo >= hi {
                break;
            }
            let pool = pool.clone();
            handles.push(scope.spawn(move || {
                // 片段单元：只带字符串池（已全部预登记，`intern_string` 命中
                // 已有的编号），代码 / 符号 / 重定位都从空开始。
                let mut part = CompilationUnit::new();
                part.is_thumb = is_thumb;
                part.string_pool = pool;
                for func in &funcs[lo..hi] {
                    emit_one(target, arch, &mut part, func);
                }
                part
            }));
        }
        for h in handles {
            let mut part = h.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
            if std::env::var_os("ZR_CODEGEN_DEBUG").is_some() {
                eprintln!(
                    "[codegen] merge part: {} bytes, first sym {:?}",
                    part.code.len(),
                    part.symbols.first().map(|s| &s.name)
                );
            }
            merge_fragment(unit, &mut part);
        }
    });
}

/// 把一个函数的发射片段并入主单元（重定位与 `.text` 符号整体平移）。
fn merge_fragment(unit: &mut CompilationUnit, part: &mut CompilationUnit) {
    // 片段里不该再出现字符串符号 / 字节——预登记后 `intern_string` 只查表。
    debug_assert!(
        part.rodata.is_empty(),
        "并行代码生成片段里又登记了字符串，预登记漏了字面量"
    );
    let base = unit.code.len() as i64;
    unit.code.append(&mut part.code);
    for r in &mut part.relocations {
        r.offset = (r.offset as i64 + base) as usize;
        r.instr_start = (r.instr_start as i64 + base) as usize;
    }
    unit.relocations.append(&mut part.relocations);
    for s in &mut part.symbols {
        if s.section == SymbolSection::Text {
            s.offset = (s.offset as i64 + base) as usize;
        }
    }
    unit.symbols.append(&mut part.symbols);
}

/// 从 HIR 模块提取链接指令和 repr 布局信息。
///
/// 在代码生成前调用，把 `#[link(...)]` 和 `#[repr(...)]` 属性
/// 从 AST/HIR 提取到 `CompilationUnit` 中，供链接器使用。
pub fn extract_attributes(
    unit: &mut CompilationUnit,
    hir: &crate::typeck::hir::HirModule,
    prog: &crate::parser::ast::Program,
) {
    // 提取链接指令。
    //
    // `extern "C"` 块里的函数声明已被并进 `prog.functions`（`body` 为 `None`），
    // 所以这里一并覆盖，无需单独遍历 `prog.externs`。
    for item in &prog.functions {
        if let Some(directive) = crate::attributes::link_directive(&item.attrs) {
            unit.link_directives.push(directive);
        }
    }

    // 提取 repr 布局
    for (def_id, name) in &hir.def_names {
        if let Some(crate::typeck::hir::HirTypeDef::Struct(s)) = hir.type_defs.get(def_id) {
            unit.struct_reprs.push((name.clone(), s.repr));
        }
    }
}

/// 写出目标文件（`-c`：只编译不链接）。
pub fn emit_object(target: &Target, unit: &CompilationUnit, path: &Path) -> std::io::Result<()> {
    check_target(target)?;
    let mut unit = unit.clone();
    unit.kind = OutputKind::Relocatable;
    // 目标文件不解析重定位：外部符号必须显式声明，交由平台链接器绑定。
    objfile::add_extern_symbols(&mut unit, target.os());
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
pub fn emit_executable(
    target: &Target,
    unit: &CompilationUnit,
    path: &Path,
) -> std::io::Result<()> {
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
            format!(
                "big-endian targets are not supported yet: {}",
                target.triple()
            ),
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
            c.arg("-o")
                .arg(output.as_os_str())
                .arg("-e")
                .arg(target.entry_symbol())
                .arg("-static")
                .arg(obj.as_os_str());
            if let Some(lib) = runtime_lib {
                c.arg(lib);
            }
            commands.push(c);

            // gcc 驱动（自带正确的 CRT 与入口约定）
            let mut c = std::process::Command::new("gcc");
            c.arg("-nostdlib")
                .arg("-no-pie")
                .arg("-o")
                .arg(output.as_os_str())
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
                c.arg("-o")
                    .arg(output.as_os_str())
                    .arg(obj.as_os_str())
                    .arg(format!("-Wl,-e,{}", target.entry_symbol()));
                if let Some(lib) = runtime_lib {
                    c.arg(lib);
                    // 也直接链接运行时 .o 文件，确保 Mach-O 符号存根正确生成
                    // 静态库名格式: libzrrt-<flavor>.a, 对应 .o 文件: <flavor>.o
                    let lib_path = std::path::Path::new(lib);
                    if let Some(stem) = lib_path.file_stem().and_then(|s| s.to_str()) {
                        if let Some(flavor) = stem.strip_prefix("libzrrt-") {
                            let obj_name = format!("{}.o", flavor);
                            let obj_path = lib_path.with_file_name(obj_name);
                            if obj_path.exists() {
                                c.arg(&obj_path);
                            }
                        }
                    }
                }
                commands.push(c);
            }
        }
        ObjectFormat::PeCoff => {
            // clang 驱动：自动附加 kernel32.lib 等系统导入库
            let mut c = std::process::Command::new("clang");
            c.arg(format!("--target={}", target.triple()))
                .arg("-o")
                .arg(output.as_os_str())
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
            c.arg("-nostdlib")
                .arg("-o")
                .arg(output.as_os_str())
                .arg(obj.as_os_str());
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
        Arch::Wasm64 => "wasm64-unknown-unknown",
        // 裸机目标不经过平台链接器，这里只是让穷尽性成立。
        Arch::Arm32 | Arch::Arm32Thumb | Arch::RiscV32 | Arch::RiscV64 => "arm-none-eabi",
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

/// 寄存器压力超出后端能力时报错并退出。
///
/// 已经实现溢出（spilling）的后端不会走到这里。
#[allow(dead_code)]
pub fn unsupported_pressure(func: &str, pool: usize) -> ! {
    eprintln!(
        "zr: error: function `{func}` needs more than {pool} registers at once, \
         and register spilling is not implemented for this target"
    );
    std::process::exit(1);
}

/// 遇到后端还没实现的字面量时报错并退出。
///
/// 早先五个后端在遇到浮点或字符串字面量时都安静地发射 `0`：程序能编出来、
/// 能跑，只是结果是错的。那种 bug 比编译失败难查得多——运行期看到的
/// 只是一个诡异的数字，没有任何线索指向代码生成。所以这里宁可编译失败。
pub fn unsupported_const(kind: &str) -> ! {
    eprintln!("zr: error: `{kind}` literals are not implemented for this target");
    std::process::exit(1);
}

/// 这个目标后端没有实现该运算码。报错并终止。
pub fn unsupported_binary_op(op: crate::ssa::SsaBinaryOp) -> ! {
    panic!("zr: 该目标后端尚未实现二元运算 `{op:?}`（浮点/位运算等）")
}

/// 目标后端没有内联汇编支持时，明确报错并终止。
pub fn unsupported_inline_asm(target: &str) -> ! {
    eprintln!("zr: error: inline assembly (`asm!`) is not implemented for the {target} target");
    std::process::exit(1);
}

/// 目标后端没有数组 SIMD 指令选择支持时，明确报错并终止。
pub fn unsupported_simd(target: &str) -> ! {
    eprintln!("zr: error: SIMD array operations are not implemented for the {target} target");
    std::process::exit(1);
}

/// 目标后端还没有函数指针/间接调用支持。
pub fn unsupported_function_pointer(target: &str) -> ! {
    eprintln!("zr: error: function pointers and indirect calls are not implemented for the {target} target");
    std::process::exit(1);
}

pub fn unsupported_cast(from: crate::mir::NumTy, to: crate::mir::NumTy) -> ! {
    eprintln!(
        "zr: error: conversion {} -> {} is not implemented for this target",
        num_ty_name(from),
        num_ty_name(to)
    );
    std::process::exit(1);
}

/// 数值类型的可读名字。
pub fn num_ty_name(t: crate::mir::NumTy) -> &'static str {
    use crate::mir::NumTy::*;
    match t {
        Int(8) => "i8",
        Int(16) => "i16",
        Int(32) => "i32",
        Int(_) => "i64",
        Uint(8) => "u8",
        Uint(16) => "u16",
        Uint(32) => "u32",
        Uint(_) => "u64",
        Float(8) => "f8",
        Float(16) => "f16",
        Float(32) => "f32",
        Float(_) => "f64",
        Bool => "bool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_string_deduplicates_and_nul_terminates() {
        let mut u = CompilationUnit::new();
        let a = u.intern_string("hello");
        let b = u.intern_string("world");
        let c = u.intern_string("hello");
        assert_eq!(a, ".Lstr0");
        assert_eq!(b, ".Lstr1");
        assert_eq!(c, a, "相同文本应共享同一个符号");
        assert_eq!(u.rodata, b"hello\0world\0");

        let s0 = u.symbols.iter().find(|s| s.name == ".Lstr0").unwrap();
        assert_eq!(s0.section, SymbolSection::Rodata);
        assert_eq!((s0.offset, s0.size), (0, 6));
        let s1 = u.symbols.iter().find(|s| s.name == ".Lstr1").unwrap();
        assert_eq!((s1.offset, s1.size), (6, 6));
    }
}
