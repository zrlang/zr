//! 代码生成（Code Generation）。
//!
//! 设计参考：
//! - Rust：MIR → LLVM IR / Cranelift IR，再经后端产出机器码；
//!   `rustc_codegen_llvm` 与 `rustc_codegen_cranelift` 是可选后端。
//! - Zig：自带后端（Aro 汇编器 + 链接器），支持多架构交叉编译。
//! - Python：CPython 生成字节码（栈式虚拟机），JITs（PyPy）另做机器码生成。
//!
//! 当前后端为 Linux x86-64 ELF。流水线：
//! ```text
//! SSA → 指令选择 → 寄存器分配（线性扫描） → 栈帧布局 → 汇编/机器码 → ELF
//! ```

pub mod encoder;
pub mod regalloc;
pub mod frame;
pub mod reloc;
pub mod linker;

use crate::ssa::SsaModule;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// 可重定位目标文件（`.o`）。
    Relocatable,
    /// 可执行文件。
    Executable,
    /// 汇编文本（`.s`）。
    Assembly,
}

/// 重定位条目。
#[derive(Debug, Clone)]
pub struct Relocation {
    /// 在 `code`/`data` 中的偏移。
    pub offset: usize,
    /// 目标符号名。
    pub symbol: String,
    /// 重定位类型。
    pub kind: RelocationKind,
    /// 加数（用于 PC 相对寻址的偏移修正）。
    pub addend: i64,
}

/// 重定位类型（x86-64 ELF 定义子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocationKind {
    /// 绝对 64 位。
    Abs64,
    /// PC 相对 32 位。
    PcRel32,
    /// GOT 条目 64 位。
    Got64,
    /// PLT 条目 32 位。
    Plt32,
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
    Data,
    Rodata,
    Undefined,
}

/// 符号类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Object,
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
/// 当前为骨架实现：产出合法的 ELF 外壳，入口点调用 `exit(0)`。
/// 真实实现需依次完成指令选择、寄存器分配、栈帧布局。
pub fn compile(module: &SsaModule) -> CompilationUnit {
    let mut unit = CompilationUnit::new();

    // 1. 指令选择 + 寄存器分配 + 代码发射
    for func in &module.functions {
        encoder::emit_function(&mut unit, func);
    }

    // 2. 若有 `main`，包装入口 `_start` 调用 `main` 后退出
    if module.functions.iter().any(|f| f.name == "main") {
        encoder::emit_start_wrapper(&mut unit);
    }

    // 3. 填充符号表
    unit.symbols.push(Symbol {
        name: unit.entry.clone(),
        section: SymbolSection::Text,
        offset: 0,
        size: unit.code.len(),
        global: true,
        kind: SymbolKind::Function,
    });

    unit
}

/// 把编译产物写为 ELF 可执行文件。
///
/// 流程：
/// 1. 先把 zr 编译器的输出写成一个**可重定位目标文件**（`.o`），
///    含 `_start`、`main` 等符号及 `call main` 的重定位。
/// 2. 调用系统链接器 `ld`（或 `cc`）把该目标文件与 zr 运行时
///    （`libzrrt.a`）链接为最终可执行文件，由链接器处理所有重定位。
///
/// 若系统链接器不可用（例如在缺少 `ld`/`cc` 的环境），则退回到
/// 直接写出由 `linker` 模块构造的 ELF（此时程序仍可运行，只要不调用
/// 标准库运行时函数）。
pub fn emit_elf(unit: &CompilationUnit, path: &std::path::Path) -> std::io::Result<()> {
    // 优先使用外部链接器（能正确解析 `zr_*` 运行时符号）
    if let Ok(()) = link_with_system_linker(unit, path) {
        return Ok(());
    }

    // 降级：自己构造 ELF（适用于没有 `ld`/`cc` 的构建环境）
    linker::emit_elf(unit, path)
}

/// 通过系统链接器把用户程序与运行时链接成可执行文件。
fn link_with_system_linker(unit: &CompilationUnit, output: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;

    // 先把编译产物写成临时目标文件
    let tmp_obj = output.with_extension("zr.o");
    {
        let mut f = std::fs::File::create(&tmp_obj)?;
        // 可重定位对象文件：复用 assemble_elf 但设置 kind=Relocatable
        let mut reloc = unit.clone();
        reloc.kind = crate::codegen::OutputKind::Relocatable;
        let bytes = linker::assemble_elf(&reloc)?;
        f.write_all(&bytes)?;
    }

    // 运行时静态库路径（由 build.rs 通过环境变量传入）
    let runtime_lib = option_env!("ZR_RUNTIME_LIB").unwrap_or("");
    let runtime_arg = if !runtime_lib.is_empty() {
        // ZR_RUNTIME_LIB 指向 libzrrt.a，链接时用 -lzrrt + 搜索路径
        let lib_path = std::path::Path::new(runtime_lib);
        if let Some(dir) = lib_path.parent() {
            let mut arg = String::from("-L");
            arg.push_str(dir.to_str().unwrap_or(""));
            Some((arg, String::from("-lzrrt")))
        } else {
            None
        }
    } else {
        None
    };

    // 构造链接器命令行
    // ld -o output -e _start -static user.o -L<dir> -lzrrt
    let mut cmd = std::process::Command::new("ld");
    cmd.arg("-o").arg(output.as_os_str())
        .arg("-e").arg("_start")
        .arg("-static")
        .arg(tmp_obj.as_os_str());
    if let Some((lpath, lname)) = &runtime_arg {
        cmd.arg(lpath).arg(lname);
    }

    let result = cmd.output()?;
    // 清理临时文件
    let _ = std::fs::remove_file(&tmp_obj);

    if !result.status.success() {
        let msg = String::from_utf8_lossy(&result.stderr);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("linker failed: {}", msg.trim()),
        ));
    }
    Ok(())
}

/// 把编译产物写为可重定位目标文件。
pub fn emit_object(unit: &CompilationUnit, path: &std::path::Path) -> std::io::Result<()> {
    use std::fs::File;
    use std::io::Write;
    let mut unit = unit.clone();
    unit.kind = OutputKind::Relocatable;
    let elf = linker::assemble_elf(&unit)?;
    let mut f = File::create(path)?;
    f.write_all(&elf)?;
    Ok(())
}

/// 把编译产物写为汇编文本。
pub fn emit_asm(unit: &CompilationUnit, path: &std::path::Path) -> std::io::Result<()> {
    use std::fs::File;
    use std::io::Write;
    let asm = encoder::disassemble(unit);
    let mut f = File::create(path)?;
    f.write_all(asm.as_bytes())?;
    Ok(())
}
