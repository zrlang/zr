//! build.rs — 编译 zr 运行时（汇编）并与编译器主程序链接。
//!
//! 运行时提供 7 个 `zr_*` 符号（panic/write/read/exit/alloc/dealloc/realloc），
//! 用 Linux x86-64 系统调用直接实现，不依赖 libc。
//!
//! 流程：
//! 1. 调用 `cc` crate 汇编 `runtime/rt_linux_x86_64.s` → `rt_linux_x86_64.o`
//! 2. 通过 `cargo:rustc-link-arg` 和 `cargo:rustc-link-search` 把目标文件加入链接。

use std::env;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let src_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let runtime_dir = src_dir.join("runtime");

    // 目标三元组（用于判断目标平台）
    let target = env::var("TARGET").unwrap_or_else(|_| "x86_64-unknown-linux-gnu".to_string());
    let is_linux_x86_64 = target.contains("linux") && target.contains("x86_64");

    if is_linux_x86_64 {
        let asm = runtime_dir.join("rt_linux_x86_64.s");
        let obj = out_dir.join("rt_linux_x86_64.o");

        // 汇编
        let status = std::process::Command::new("as")
            .arg("--64")
            .arg("-o")
            .arg(&obj)
            .arg(&asm)
            .status()
            .expect("failed to invoke `as` (GNU assembler not found)");
        if !status.success() {
            panic!("assembly of runtime failed (is `as` installed?)");
        }

        // 打包成静态库（命名为 libzrrt.a），用库名链接
        let archive = out_dir.join("libzrrt.a");
        let ar_status = std::process::Command::new("ar")
            .arg("crs")
            .arg(&archive)
            .arg(&obj)
            .status();
        match ar_status {
            Ok(s) if s.success() => {}
            _ => {
                // 若 `ar` 不可用，退回到直接传目标文件
                println!("cargo:rustc-link-search={}", out_dir.display());
                println!("cargo:rustc-link-arg={}", obj.display());
                println!("cargo:warning=`ar` not available; linking runtime object directly");
                return;
            }
        }

        println!("cargo:rustc-link-search={}", out_dir.display());
        println!("cargo:rustc-link-lib=static=zrrt");

        // 导出运行时库路径，供 zr 编译器在链接用户程序时使用。
        // 编译器读取 `ZR_RUNTIME_LIB` 环境变量定位 libzrrt.a。
        println!("cargo:rustc-env=ZR_RUNTIME_LIB={}", archive.display());
    } else {
        println!("cargo:warning=zr runtime only supports linux-x86_64; target `{}` will link without runtime symbols", target);
    }

    // 重新构建时检查汇编文件
    println!("cargo:rerun-if-changed=runtime/rt_linux_x86_64.s");
    println!("cargo:rerun-if-changed=build.rs");
}
