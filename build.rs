//! build.rs — 为各目标平台编译 zr 运行时（汇编）并导出其位置。
//!
//! 运行时提供基础 `zr_*` 符号（panic/write/read/exit/alloc/dealloc/
//! realloc）以及任务队列入口（spawn/run_tasks），各平台实现方式不同：
//!
//! | 目标 | 源文件 | 实现 |
//! |------|--------|------|
//! | Linux x86-64 | `rt_linux_x86_64.s` | 直接系统调用，不依赖 libc |
//! | Linux aarch64 | `rt_linux_aarch64.s` | 直接系统调用 |
//! | macOS x86-64 | `rt_macos_x86_64.s` | libc（libSystem）薄封装 |
//! | macOS aarch64 | `rt_macos_arm64.s` | libc 薄封装 |
//! | Windows x86-64 | `rt_windows_x86_64.s` | kernel32 薄封装 |
//! | Windows aarch64 | `rt_windows_arm64.s` | kernel32 薄封装 |
//! | 裸机 ARM | `rt_arm_baremetal.s` | semihosting I/O + 静态堆 |
//! | 裸机 RISC-V | `rt_riscv_baremetal.s` | semihosting I/O + 静态堆 |
//! | wasm32 | —— | 由 wasm 后端内联生成，无需外部文件 |
//!
//! 流程：
//! 1. 逐个目标尝试用可用的汇编器构建运行时（宿主必建，交叉目标尽力而为）；
//! 2. 打包为 `libzrrt-<平台>.a`；
//! 3. 通过 `cargo:rustc-env` 把结果告诉 zr 编译器：
//!    - `ZR_RUNTIME_LIB`：宿主运行时（保持 `ld -lzrrt` 的老路径可用）；
//!    - `ZR_RUNTIME_LIBS`：`规范三元组=路径` 的列表，编译用户程序时按
//!      目标三元组查找，从而支持交叉链接。
//!
//! 设计约束：**编译 zr 编译器本身不依赖运行时**，因此当某个目标缺少
//! 汇编器时，构建会安静地跳过它（不产生任何警告），只是为该目标编译
//! 的用户程序无法链接 `zr_*` 符号。
//!
//! 设计参考：Rust 的 `cc` crate、Zig 的 build runner。

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 运行时源文件：`(平台标签, 规范三元组, 文件名)`。
const RUNTIME_TARGETS: &[(&str, &str, &str)] = &[
    ("bare_arm", "arm-none-eabi", "rt_arm_baremetal.s"),
    (
        "bare_riscv32",
        "riscv32-unknown-none",
        "rt_riscv_baremetal.s",
    ),
    (
        "bare_riscv64",
        "riscv64-unknown-none",
        "rt_riscv_baremetal.s",
    ),
    (
        "linux_x86_64",
        "x86_64-unknown-linux-gnu",
        "rt_linux_x86_64.s",
    ),
    (
        "linux_aarch64",
        "aarch64-unknown-linux-gnu",
        "rt_linux_aarch64.s",
    ),
    ("macos_x86_64", "x86_64-apple-darwin", "rt_macos_x86_64.s"),
    ("macos_arm64", "aarch64-apple-darwin", "rt_macos_arm64.s"),
    (
        "windows_x86_64",
        "x86_64-pc-windows-msvc",
        "rt_windows_x86_64.s",
    ),
    (
        "windows_arm64",
        "aarch64-pc-windows-msvc",
        "rt_windows_arm64.s",
    ),
];

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let runtime_dir = manifest_dir.join("runtime");
    let host = env::var("HOST").unwrap_or_else(|_| "x86_64-unknown-linux-gnu".to_string());
    let host_triple = canonical_host_triple(&host);

    for (_, _, file) in RUNTIME_TARGETS {
        println!("cargo:rerun-if-changed=runtime/{file}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ZR_SKIP_RUNTIME");

    if env::var_os("ZR_SKIP_RUNTIME").is_some() {
        // 显式要求跳过（例如只发布编译器本身）
        return;
    }

    let mut built: BTreeMap<String, PathBuf> = BTreeMap::new();

    for (flavor, triple, file) in RUNTIME_TARGETS {
        let asm = runtime_dir.join(file);
        if !asm.exists() {
            continue;
        }
        let obj = out_dir.join(format!("{flavor}.o"));
        let archive = out_dir.join(format!("libzrrt-{flavor}.a"));

        let is_host = host_triple == *triple;
        let host_is_elf = host.contains("linux");
        let candidates = assembler_candidates(flavor, host_is_elf);
        let Some(status) = assemble(&candidates, &asm, &obj) else {
            // 没有任何可用的汇编器
            continue;
        };
        if !status {
            // 交叉目标汇编失败（工具链不支持该目标）时安静跳过；
            // 宿主目标失败说明运行时本身有问题，必须报错。
            if is_host {
                panic!("assembling host runtime '{}' failed", asm.display());
            }
            continue;
        }

        if !archive_with(&archive, &obj) {
            continue;
        }
        // 宿主运行时额外保留 libzrrt.a 这一传统名字。
        if is_host {
            let legacy = out_dir.join("libzrrt.a");
            let _ = std::fs::copy(&archive, &legacy);
        }
        built.insert((*triple).to_string(), archive);
    }

    if built.is_empty() {
        return;
    }

    // 宿主运行时：无条件链接进 zr 编译器（保持既有行为）。
    if let Some(path) = built.get(&host_triple) {
        println!("cargo:rustc-link-search={}", out_dir.display());
        println!("cargo:rustc-link-lib=static=zrrt");
        println!("cargo:rustc-env=ZR_RUNTIME_LIB={}", path.display());
    }

    // 全部运行时：供交叉编译时按目标三元组查找。
    let listing: Vec<String> = built
        .iter()
        .map(|(triple, path)| format!("{triple}={}", path.display()))
        .collect();
    println!("cargo:rustc-env=ZR_RUNTIME_LIBS={}", listing.join(";"));
}

/// 把宿主的 `HOST` 三元组规范化为运行时的命名方式。
fn canonical_host_triple(host: &str) -> String {
    let arch = host.split('-').next().unwrap_or("");
    let is_arm64 = matches!(arch, "aarch64" | "arm64");
    let hosted = host.contains("linux")
        || host.contains("darwin")
        || host.contains("apple")
        || host.contains("windows")
        || host.contains("mingw");

    if hosted {
        let suffix = if is_arm64 { "aarch64" } else { "x86_64" };
        if host.contains("linux") {
            return format!("{suffix}-unknown-linux-gnu");
        }
        if host.contains("windows") || host.contains("mingw") {
            return format!("{suffix}-pc-windows-msvc");
        }
        return format!("{suffix}-apple-darwin");
    }

    // 裸机宿主（arm-none-eabi / riscv32-unknown-none 等）
    if arch.starts_with("riscv32") {
        return "riscv32-unknown-none".to_string();
    }
    if arch.starts_with("riscv64") {
        return "riscv64-unknown-none".to_string();
    }
    if matches!(arch, "arm" | "armv7" | "armv7a" | "thumb" | "thumbv7em") {
        return "arm-none-eabi".to_string();
    }
    host.to_string()
}

/// 依次尝试候选汇编器。
///
/// `None` = 没有可用汇编器；`Some(ok)` = 已尝试，`ok` 表示是否成功。
fn assemble(candidates: &[Vec<String>], asm: &Path, obj: &Path) -> Option<bool> {
    for args in candidates {
        let Some((program, rest)) = args.split_first() else {
            continue;
        };
        let Ok(output) = Command::new(program)
            .args(rest)
            .arg("-o")
            .arg(obj)
            .arg(asm)
            .output()
        else {
            continue; // 汇编器不存在
        };
        return Some(output.status.success());
    }
    None
}

/// 用 `ar` 打包静态库。
fn archive_with(archive: &Path, obj: &Path) -> bool {
    let Ok(status) = Command::new("ar").arg("crs").arg(archive).arg(obj).status() else {
        return false;
    };
    status.success()
}

/// 汇编器候选（按优先级）。
fn assembler_candidates(flavor: &str, host_is_elf: bool) -> Vec<Vec<String>> {
    let clang = |target: &str| {
        vec![
            "clang".to_string(),
            "-target".to_string(),
            target.to_string(),
            "-c".to_string(),
        ]
    };
    match flavor {
        // Linux 目标必须产出 ELF 目标文件：clang 需要显式 -target，
        // GNU as 只能在 ELF 宿主上使用（Apple 的 as 产出 Mach-O）。
        "linux_x86_64" => {
            let mut list = vec![clang("x86_64-unknown-linux-gnu")];
            if host_is_elf {
                list.push(vec!["as".to_string(), "--64".to_string()]);
            }
            list
        }
        "linux_aarch64" => {
            let mut list = vec![
                vec!["aarch64-linux-gnu-as".to_string()],
                clang("aarch64-unknown-linux-gnu"),
            ];
            if host_is_elf {
                list.push(vec!["as".to_string()]);
            }
            list
        }
        "macos_x86_64" => vec![clang("x86_64-apple-macos11")],
        "macos_arm64" => vec![clang("arm64-apple-macos11")],
        "windows_x86_64" => vec![
            vec![
                "clang".to_string(),
                "--target=x86_64-pc-windows-msvc".to_string(),
                "-c".to_string(),
            ],
            vec!["x86_64-w64-mingw32-gcc".to_string(), "-c".to_string()],
        ],
        "windows_arm64" => vec![
            vec![
                "clang".to_string(),
                "--target=aarch64-pc-windows-msvc".to_string(),
                "-c".to_string(),
            ],
            vec!["aarch64-w64-mingw32-gcc".to_string(), "-c".to_string()],
        ],
        // 裸机 ARM：sdiv/udiv 需要 ARMv7-R/A，故显式指定 -march=armv7ve
        "bare_arm" => vec![
            vec![
                "arm-none-eabi-gcc".to_string(),
                "-march=armv7ve".to_string(),
                "-c".to_string(),
            ],
            vec![
                "clang".to_string(),
                "-target".to_string(),
                "armv7a-none-eabi".to_string(),
                "-march=armv7ve".to_string(),
                "-c".to_string(),
            ],
            clang("arm-none-eabi"),
        ],
        "bare_riscv32" => vec![
            vec![
                "riscv64-unknown-elf-gcc".to_string(),
                "-march=rv32imac".to_string(),
                "-mabi=ilp32".to_string(),
                "-c".to_string(),
            ],
            vec![
                "clang".to_string(),
                "-target".to_string(),
                "riscv32-unknown-elf".to_string(),
                "-march=rv32imac".to_string(),
                "-mabi=ilp32".to_string(),
                "-c".to_string(),
            ],
        ],
        "bare_riscv64" => vec![
            vec![
                "riscv64-unknown-elf-gcc".to_string(),
                "-march=rv64imac".to_string(),
                "-mabi=lp64".to_string(),
                "-c".to_string(),
            ],
            vec![
                "clang".to_string(),
                "-target".to_string(),
                "riscv64-unknown-elf".to_string(),
                "-march=rv64imac".to_string(),
                "-mabi=lp64".to_string(),
                "-c".to_string(),
            ],
        ],
        _ => Vec::new(),
    }
}
