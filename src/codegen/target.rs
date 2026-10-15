//! 目标平台描述：三元组解析与平台属性查询。
//!
//! 编译器需要知道"为谁生成代码"，涉及三件事：
//! 1. **架构**（x86-64 / aarch64 / wasm32 / arm / riscv32 / riscv64）
//!    ——决定指令编码与指针宽度；
//! 2. **操作系统**（Linux / macOS / Windows / WASI / 裸机）——决定系统
//!    调用与运行时（`zr_*` 符号）的实现方式；
//! 3. **目标文件格式**（ELF32/ELF64 / Mach-O / PE-COFF / wasm）。
//!
//! 三元组形式为 `arch-vendor-os-env`（如 `aarch64-apple-darwin`），
//! 与 Rust/Cargo 的约定一致，因此 `cargo build --target=...` 的结果
//! 可以直接作为 `--target` 参数传给 zr 编译器。

use std::fmt;

/// CPU 架构。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    /// AMD64（x86-64）。
    X86_64,
    /// ARM 64 位（aarch64 / arm64）。
    AArch64,
    /// WebAssembly 32 位。
    Wasm32,
    /// ARM 32 位（嵌入式，A32 编码）。
    Arm32,
    /// RISC-V 32 位（RV32I + M 扩展）。
    RiscV32,
    /// RISC-V 64 位（RV64I + M 扩展）。
    RiscV64,
}

impl Arch {
    /// 从三元组首段解析架构，兼容常见别名。
    ///
    /// 嵌入式常见的写法都在此归一：`armv7-none-eabi` / `arm-none-eabi`
    /// 都映射到 [`Arch::Arm32`]，`riscv64gc-unknown-elf` 映射到
    /// [`Arch::RiscV64`]。
    pub fn parse(s: &str) -> Option<Arch> {
        match s {
            "x86_64" | "amd64" | "x86-64" => Some(Arch::X86_64),
            "aarch64" | "arm64" => Some(Arch::AArch64),
            "wasm32" | "wasm" => Some(Arch::Wasm32),
            // ARM 32 位：Thumb（thumb / thumbv7em …）目前没有后端，
            // 显式不识别，让上层给出"暂不支持"的提示而不是默默按 A32 生成。
            "arm" | "armv4" | "armv5" | "armv6" | "armv7" | "armv7a" | "armv7-a" | "armeb" => {
                Some(Arch::Arm32)
            }
            s if s.starts_with("riscv32") => Some(Arch::RiscV32),
            s if s.starts_with("riscv64") => Some(Arch::RiscV64),
            _ => None,
        }
    }

    /// 规范架构名（用于生成三元组）。
    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::AArch64 => "aarch64",
            Arch::Wasm32 => "wasm32",
            Arch::Arm32 => "arm",
            Arch::RiscV32 => "riscv32",
            Arch::RiscV64 => "riscv64",
        }
    }

    /// 指针宽度（字节）。
    pub fn pointer_size(self) -> u8 {
        match self {
            Arch::Wasm32 | Arch::Arm32 | Arch::RiscV32 => 4,
            Arch::X86_64 | Arch::AArch64 | Arch::RiscV64 => 8,
        }
    }

    /// 是否为小端。
    ///
    /// 当前支持的三种架构（x86-64 / aarch64 / wasm32）都是小端；
    /// 接入大端架构时需要在各容器写入器中补上字节序分支。
    pub fn is_little_endian(self) -> bool {
        true
    }

    /// ELF `e_machine` 取值。
    pub fn elf_machine(self) -> u16 {
        match self {
            Arch::X86_64 => 62,  // EM_X86_64
            Arch::AArch64 => 183, // EM_AARCH64
            Arch::Wasm32 => 243,  // EM_WASM
            Arch::Arm32 => 40,   // EM_ARM
            Arch::RiscV32 | Arch::RiscV64 => 243, // EM_RISCV
        }
    }

    /// ELF `e_flags` 取值。
    ///
    /// ARM 用它记录 EABI 版本（其余架构约定为 0）。
    pub fn elf_flags(self) -> u32 {
        match self {
            // EABI5
            Arch::Arm32 => 0x0500_0000,
            _ => 0,
        }
    }

    /// Mach-O `cputype` / `cpusubtype`。
    pub fn macho_cpu(self) -> (u32, u32) {
        match self {
            // CPU_TYPE_X86_64 / CPU_SUBTYPE_X86_64_ALL
            Arch::X86_64 => (0x0100_0007, 0x0000_0003),
            // CPU_TYPE_ARM64 / CPU_SUBTYPE_ARM64_ALL
            Arch::AArch64 => (0x0100_000c, 0x0000_0000),
            Arch::Wasm32 => (0x0100_0007, 0x0000_0003),
            Arch::Arm32 | Arch::RiscV32 | Arch::RiscV64 => (0x0100_0007, 0x0000_0003),
        }
    }

    /// COFF `Machine` 取值。
    pub fn pe_machine(self) -> u16 {
        match self {
            Arch::X86_64 => 0x8664,   // IMAGE_FILE_MACHINE_AMD64
            Arch::AArch64 => 0xaa64,  // IMAGE_FILE_MACHINE_ARM64
            Arch::Wasm32 => 0x8664,
            Arch::Arm32 => 0x01c0,    // IMAGE_FILE_MACHINE_ARM
            Arch::RiscV32 | Arch::RiscV64 => 0x8664,
        }
    }
}

/// 操作系统。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// 裸机（无操作系统）：嵌入式目标，由 semihosting / 静态堆提供运行时。
    Bare,
    /// Linux（含 musl / android 等 GNU 风格 ABI）。
    Linux,
    /// macOS（Darwin）。三元组里写作 `darwin`，故名 `Darwin`。
    Darwin,
    /// Windows（MSVC 与 MinGW 共用同一 ABI 描述）。
    Win,
    /// WASI（wasm32 沙箱环境，通过导入函数访问宿主）。
    Wasi,
}

impl Os {
    /// 从三元组中解析操作系统。
    pub fn parse(s: &str) -> Option<Os> {
        match s {
            "none" | "elf" | "eabi" | "eabihf" | "none-eabi" => Some(Os::Bare),
            "linux" | "android" | "ohos" => Some(Os::Linux),
            "darwin" | "macos" | "ios" | "tvos" | "watchos" => Some(Os::Darwin),
            "windows" | "win32" | "mingw" => Some(Os::Win),
            "wasi" => Some(Os::Wasi),
            _ => None,
        }
    }

    /// 规范名称（macOS 在三元组里写作 `darwin`）。
    pub fn name(self) -> &'static str {
        match self {
            Os::Bare => "none",
            Os::Linux => "linux",
            Os::Darwin => "darwin",
            Os::Win => "windows",
            Os::Wasi => "wasi",
        }
    }

    /// 人类可读的平台名。
    pub fn display_name(self) -> &'static str {
        match self {
            Os::Bare => "bare metal",
            Os::Linux => "Linux",
            Os::Darwin => "macOS",
            Os::Win => "Windows",
            Os::Wasi => "WASI",
        }
    }

    /// 该平台对应的运行时源码文件名（`runtime/` 下）。
    ///
    /// wasm32-wasi 与 wasm32-unknown-unknown 的运行时都由后端内联生成，
    /// 不需要外部汇编文件，返回 `None`。
    pub fn runtime_source(self, arch: Arch) -> Option<&'static str> {
        match (self, arch) {
            (Os::Bare, Arch::Arm32) => Some("rt_arm_baremetal.s"),
            (Os::Bare, Arch::RiscV32) | (Os::Bare, Arch::RiscV64) => Some("rt_riscv_baremetal.s"),
            (Os::Linux, Arch::X86_64) => Some("rt_linux_x86_64.s"),
            (Os::Linux, Arch::AArch64) => Some("rt_linux_aarch64.s"),
            (Os::Darwin, Arch::X86_64) => Some("rt_macos_x86_64.s"),
            (Os::Darwin, Arch::AArch64) => Some("rt_macos_arm64.s"),
            (Os::Win, Arch::X86_64) => Some("rt_windows_x86_64.s"),
            (Os::Win, Arch::AArch64) => Some("rt_windows_arm64.s"),
            _ => None,
        }
    }

    /// 该平台是否使用系统调用而非宿主 API 实现运行时。
    pub fn uses_syscalls(self) -> bool {
        matches!(self, Os::Linux)
    }

    /// 运行时的链接方式（用于诊断信息）。
    pub fn runtime_link_hint(self) -> &'static str {
        match self {
            Os::Bare => "bare metal: semihosting I/O + static heap",
            Os::Linux => "freestanding: zr runtime talks to the kernel directly",
            Os::Darwin => "libc: zr runtime wraps libSystem",
            Os::Win => "kernel32: zr runtime wraps Win32 API",
            Os::Wasi => "imports: zr runtime is imported from the host",
        }
    }
}

/// 目标文件（可执行文件）容器格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
    /// Linux ELF。
    Elf,
    /// macOS Mach-O。
    MachO,
    /// Windows PE/COFF。
    PeCoff,
    /// WebAssembly 二进制模块。
    Wasm,
}

impl ObjectFormat {
    /// 格式名（用于诊断信息）。
    pub fn name(self) -> &'static str {
        match self {
            ObjectFormat::Elf => "ELF",
            ObjectFormat::MachO => "Mach-O",
            ObjectFormat::PeCoff => "PE/COFF",
            ObjectFormat::Wasm => "wasm",
        }
    }
}

/// 编译目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    arch: Arch,
    os: Os,
    vendor: String,
    env: String,
}

impl Target {
    /// 由已知架构/操作系统构造（vendor/env 留空）。
    pub fn new(arch: Arch, os: Os) -> Self {
        Self {
            arch,
            os,
            vendor: default_vendor(os).to_string(),
            env: default_env(os).to_string(),
        }
    }

    /// 解析目标三元组。
    ///
    /// 接受标准三元组（`x86_64-unknown-linux-gnu`）与常见简写
    /// （`x86_64-linux`、`wasm32`、`aarch64-apple-darwin`）。架构别名
    /// `amd64`/`arm64` 会被规范化。
    pub fn parse(triple: &str) -> Result<Self, String> {
        let triple = triple.trim();
        if triple.is_empty() {
            return Err("empty target triple".to_string());
        }

        let parts: Vec<&str> = triple.split('-').collect();
        let arch = Arch::parse(parts[0])
            .ok_or_else(|| format!("unsupported architecture '{}' in target '{}'", parts[0], triple))?;

        // 操作系统可能出现在第 2 段（如 `-windows-`、`-apple-darwin` 中的
        // 最后一段）或第 3 段；逐段匹配，找不到时按"裸架构"处理。
        let mut os: Option<Os> = None;
        let mut os_index = 0usize;
        for (i, part) in parts.iter().enumerate().skip(1) {
            // 版本后缀（darwin23.0、androideabi 等）不参与匹配。
            let base = part.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
            if let Some(found) = Os::parse(base) {
                os = Some(found);
                os_index = i;
                break;
            }
        }

        // wasm32 没有操作系统：`none` 只是 LLVM 三元组里的占位，
        // 其运行时由宿主导入实现（WASI 语义），而不是 semihosting 裸机。
        let os = match os {
            Some(Os::Bare) if arch == Arch::Wasm32 => Some(Os::Wasi),
            other => other,
        };

        let (os, vendor, env) = match os {
            Some(os) => {
                let vendor = parts
                    .get(1)
                    .copied()
                    .filter(|v| is_vendor_token(os, v))
                    .unwrap_or(default_vendor(os));
                let env = parts
                    .get(os_index + 1)
                    .copied()
                    .filter(|e| is_env_token(e))
                    .unwrap_or_else(|| default_env(os));
                (os, vendor, env)
            }
            None => {
                // 裸架构简写：wasm32 落到 unknown-unknown（导入式运行时），
                // 其他架构缺少操作系统信息，直接报错。
                if arch == Arch::Wasm32 {
                    (Os::Wasi, default_vendor(Os::Wasi), "")
                } else {
                    return Err(format!(
                        "cannot infer operating system from target '{}'; \
                         use a full triple such as 'aarch64-unknown-linux-gnu'",
                        triple
                    ));
                }
            }
        };

        Ok(Target {
            arch,
            os,
            vendor: vendor.to_string(),
            env: env.to_string(),
        })
    }

    /// 当前宿主平台。
    pub fn host() -> Self {
        let arch = if cfg!(target_arch = "x86_64") {
            Arch::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Arch::AArch64
        } else if cfg!(target_arch = "wasm32") {
            Arch::Wasm32
        } else {
            Arch::X86_64
        };

        let os = if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::Darwin
        } else if cfg!(target_os = "windows") {
            Os::Win
        } else if cfg!(target_os = "wasi") {
            Os::Wasi
        } else {
            Os::Linux
        };

        Self::new(arch, os)
    }

    /// 架构。
    pub fn arch(&self) -> Arch {
        self.arch
    }

    /// 操作系统。
    pub fn os(&self) -> Os {
        self.os
    }

    /// vendor 段。
    pub fn vendor(&self) -> &str {
        &self.vendor
    }

    /// env 段（gnu / musl / msvc / …）。
    pub fn env(&self) -> &str {
        &self.env
    }

    /// 容器格式（由 OS 决定）。
    pub fn format(&self) -> ObjectFormat {
        match self.os {
            // 裸机目标只产出 ELF 映像（32 位架构用 ELF32）
            Os::Bare | Os::Linux => ObjectFormat::Elf,
            Os::Darwin => ObjectFormat::MachO,
            Os::Win => ObjectFormat::PeCoff,
            Os::Wasi => ObjectFormat::Wasm,
        }
    }

    /// 是否为小端。
    pub fn is_little_endian(&self) -> bool {
        self.arch.is_little_endian()
    }

    /// 指针宽度（字节）。
    pub fn pointer_size(&self) -> u8 {
        self.arch.pointer_size()
    }

    /// 规范三元组字符串。
    pub fn triple(&self) -> String {
        let mut s = format!("{}-{}-{}", self.arch.name(), self.vendor(), self.os.name());
        if !self.env().is_empty() {
            s.push('-');
            s.push_str(self.env());
        }
        s
    }

    /// 规范三元组（vendor / env 取默认值）。
    ///
    /// 这是运行时的查找键：`build.rs` 按同一套名字登记 `libzrrt-*.a`，
    /// 因此 `--target aarch64-apple-darwin23.0` 与 `--target
    /// aarch64-apple-darwin` 能命中同一个运行时。
    pub fn canonical_triple(&self) -> String {
        let arch = self.arch.name();
        match self.os {
            // 裸机目标的运行时按架构命名
            Os::Bare => match self.arch {
                Arch::Arm32 => "arm-none-eabi".to_string(),
                Arch::RiscV32 => "riscv32-unknown-none".to_string(),
                Arch::RiscV64 => "riscv64-unknown-none".to_string(),
                _ => format!("{arch}-unknown-none"),
            },
            Os::Linux => format!("{arch}-unknown-linux-gnu"),
            Os::Darwin => format!("{arch}-apple-darwin"),
            Os::Win => format!("{arch}-pc-windows-msvc"),
            Os::Wasi => "wasm32-unknown-unknown".to_string(),
        }
    }

    /// LLVM IR `target triple` 字符串（wasm 使用 LLVM 约定的
    /// `wasm32-unknown-unknown` 写法）。
    pub fn llvm_triple(&self) -> String {
        match self.os {
            Os::Wasi => "wasm32-unknown-unknown".to_string(),
            Os::Darwin => format!("{}-{}-darwin", self.arch.name(), self.vendor),
            _ => self.triple(),
        }
    }

    /// 可执行文件的扩展名（Windows 为 `.exe`，wasm 为 `.wasm`）。
    pub fn exe_extension(&self) -> &'static str {
        match self.os {
            Os::Bare => ".elf",
            Os::Win => ".exe",
            Os::Wasi => ".wasm",
            _ => "",
        }
    }

    /// 目标文件扩展名（Windows 为 `.obj`）。
    pub fn object_extension(&self) -> &'static str {
        match self.os {
            Os::Bare => ".elf",
            Os::Win => ".obj",
            Os::Wasi => ".wasm",
            _ => ".o",
        }
    }

    /// 默认加载基址。
    pub fn image_base(&self) -> u64 {
        match self.os {
            // 裸机镜像默认从 flash 起始（0x08000000，对应 Cortex-M 向量表），
            // 可被链接脚本 / 烧录工具覆盖。
            Os::Bare => 0x0800_0000,
            Os::Linux => 0x0040_0000,
            Os::Darwin => 0x1_0000_0000,
            Os::Win => 0x0040_0000,
            Os::Wasi => 0,
        }
    }

    /// 页大小。
    pub fn page_size(&self) -> u64 {
        match self.os {
            Os::Bare => 0x1000,
            // Apple Silicon 的最小页为 16 KiB；macOS 的 __TEXT 段要求按
            // 页（16 KiB）对齐，这里使用 16 KiB 以覆盖两种架构。
            Os::Darwin => 0x4000,
            _ => 0x1000,
        }
    }

    /// 入口符号名。
    ///
    /// 各平台统一为 `_start`：
    /// - Linux：`_start`（ELF 入口，自备启动桩）
    /// - macOS：`_start`（Mach-O 符号名本身已带下划线，ld64 用
    ///   `-e _start` 指定；注意不要与 libc 的 `_main` 混淆）
    /// - Windows：`_start`（PE 头记录 RVA，符号名仅供调试）
    /// - WASI：`_start`（导出给宿主）
    pub fn entry_symbol(&self) -> &'static str {
        "_start"
    }

    /// 该目标是否被后端完整支持。
    pub fn is_supported(&self) -> bool {
        matches!(
            (self.os, self.arch),
            (Os::Bare, Arch::Arm32)
                | (Os::Bare, Arch::RiscV32)
                | (Os::Bare, Arch::RiscV64)
                | (Os::Linux, Arch::X86_64)
                | (Os::Linux, Arch::AArch64)
                | (Os::Darwin, Arch::X86_64)
                | (Os::Darwin, Arch::AArch64)
                | (Os::Win, Arch::X86_64)
                | (Os::Win, Arch::AArch64)
                | (Os::Wasi, Arch::Wasm32)
        )
    }

    /// 受支持目标的完整列表（供 `--help` 与测试使用）。
    pub fn supported_triples() -> &'static [&'static str] {
        &[
            "arm-none-eabi",
            "riscv32-unknown-none",
            "riscv64-unknown-none",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "wasm32-unknown-unknown",
            "wasm32-wasi",
        ]
    }

    /// 面向用户的目标描述，例如 `aarch64-apple-darwin (Mach-O)`。
    pub fn describe(&self) -> String {
        format!("{} ({}, {})", self.triple(), self.format().name(), self.os.display_name())
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.triple())
    }
}

/// OS 的默认 vendor 段。
fn default_vendor(os: Os) -> &'static str {
    match os {
        Os::Bare => "unknown",
        Os::Linux => "unknown",
        Os::Darwin => "apple",
        Os::Win => "pc",
        Os::Wasi => "unknown",
    }
}

/// OS 的默认 env 段。
fn default_env(os: Os) -> &'static str {
    match os {
        Os::Bare => "",
        Os::Linux => "gnu",
        Os::Darwin => "",
        Os::Win => "msvc",
        Os::Wasi => "",
    }
}

/// 判断三元组段是否为合法的 env 段（gnu/musl/msvc 等，不含版本号）。
fn is_env_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !s.starts_with('0')
}

/// 是否为该 OS 的默认 vendor 段。
fn is_vendor_token(os: Os, s: &str) -> bool {
    s == default_vendor(os)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_triples() {
        let cases = [
            ("x86_64-unknown-linux-gnu", Arch::X86_64, Os::Linux),
            ("aarch64-unknown-linux-musl", Arch::AArch64, Os::Linux),
            ("x86_64-apple-darwin", Arch::X86_64, Os::Darwin),
            ("aarch64-apple-darwin23.0", Arch::AArch64, Os::Darwin),
            ("x86_64-pc-windows-msvc", Arch::X86_64, Os::Win),
            ("aarch64-pc-windows-gnu", Arch::AArch64, Os::Win),
            ("wasm32-unknown-unknown", Arch::Wasm32, Os::Wasi),
            ("wasm32-wasi", Arch::Wasm32, Os::Wasi),
        ];
        for (triple, arch, os) in cases {
            let t = Target::parse(triple).expect(triple);
            assert_eq!(t.arch(), arch, "{triple}");
            assert_eq!(t.os(), os, "{triple}");
        }
    }

    #[test]
    fn wasm32_none_triple_maps_to_wasi() {
        // LLVM 约定：wasm32-unknown-none 的 "none" 表示"无操作系统"，
        // 运行时由宿主导入（WASI 语义），而不是 semihosting 裸机。
        let t = Target::parse("wasm32-unknown-none").unwrap();
        assert_eq!(t.arch(), Arch::Wasm32);
        assert_eq!(t.os(), Os::Wasi);
        assert!(t.is_supported());

        // 裸机三��组仍然走 semihosting
        for triple in ["arm-none-eabi", "riscv64-unknown-none"] {
            let t = Target::parse(triple).unwrap();
            assert_eq!(t.os(), Os::Bare, "{triple}");
        }
    }

    #[test]
    fn parse_aliases_and_short_forms() {
        let t = Target::parse("amd64-linux").unwrap();
        assert_eq!(t.arch(), Arch::X86_64);
        assert_eq!(t.os(), Os::Linux);

        let t = Target::parse("arm64-apple-darwin").unwrap();
        assert_eq!(t.arch(), Arch::AArch64);
        assert_eq!(t.os(), Os::Darwin);

        let t = Target::parse("wasm32").unwrap();
        assert_eq!(t.os(), Os::Wasi);
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(Target::parse("").is_err());
        assert!(Target::parse("sparc64-unknown-linux-gnu").is_err());
        assert!(Target::parse("x86_64").is_err());
    }

    #[test]
    fn formats_and_extensions() {
        assert_eq!(Target::parse("x86_64-unknown-linux-gnu").unwrap().format(), ObjectFormat::Elf);
        assert_eq!(Target::parse("aarch64-apple-darwin").unwrap().format(), ObjectFormat::MachO);
        assert_eq!(Target::parse("x86_64-pc-windows-msvc").unwrap().format(), ObjectFormat::PeCoff);
        assert_eq!(Target::parse("wasm32-unknown-unknown").unwrap().format(), ObjectFormat::Wasm);

        assert_eq!(Target::parse("x86_64-pc-windows-msvc").unwrap().exe_extension(), ".exe");
        assert_eq!(Target::parse("wasm32-wasi").unwrap().exe_extension(), ".wasm");
        assert_eq!(Target::parse("x86_64-unknown-linux-gnu").unwrap().exe_extension(), "");
    }

    #[test]
    fn every_listed_triple_is_supported() {
        for triple in Target::supported_triples() {
            let t = Target::parse(triple).unwrap();
            assert!(t.is_supported(), "{triple}");
        }
    }

    #[test]
    fn canonical_triple_normalizes_versions_and_aliases() {
        let cases = [
            ("aarch64-apple-darwin23.0", "aarch64-apple-darwin"),
            ("amd64-linux", "x86_64-unknown-linux-gnu"),
            ("aarch64-linux-musl", "aarch64-unknown-linux-gnu"),
            ("x86_64-pc-windows-gnu", "x86_64-pc-windows-msvc"),
            ("wasm32-wasi", "wasm32-unknown-unknown"),
        ];
        for (input, expected) in cases {
            assert_eq!(Target::parse(input).unwrap().canonical_triple(), expected, "{input}");
        }
    }

    #[test]
    fn host_is_supported() {
        assert!(Target::host().is_supported());
    }

    #[test]
    fn machine_codes_match_arch() {
        assert_eq!(Arch::X86_64.elf_machine(), 62);
        assert_eq!(Arch::AArch64.elf_machine(), 183);
        assert_eq!(Arch::X86_64.pe_machine(), 0x8664);
        assert_eq!(Arch::AArch64.pe_machine(), 0xaa64);
    }
}