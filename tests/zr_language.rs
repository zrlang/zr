//==============================================================================
// tests/zr_language.rs — zr 语言测试用例的宿主侧运行器
//
// `tests/cases/*.zr` 里每个文件都是一个自检程序：全部断言通过时 `main`
// 返回 0，失败时返回一个非 0 的编号（指向失败的断言）。
//
// 本文件把这些用例接进 `cargo test`：逐个编译、逐个运行、检查退出码。
// 之所以用宿主运行器而不是语言内的测试框架，是因为 `#[test]`、断言宏和
// 闭包这些设施目前编译器还不支持。
//==============================================================================

use std::path::{Path, PathBuf};
use std::process::Command;

/// 用例目录。
fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases")
}

/// 需要逐个验证的优化等级。
///
/// 每个等级都要真跑一遍：优化 pass 只要有一条改坏了语义，用例就会在
/// 某个等级上红。只测默认等级会漏掉这类问题。
const OPT_LEVELS: [&str; 5] = ["-O0", "-O1", "-O2", "-O3", "-Oz"];

/// 编译并运行一个 `.zr` 用例，返回它的退出码。
///
/// 失败时返回 `Err`（带编译 / 运行的诊断文本），**不 panic**：早先这里一
/// 出错就 panic，整个 `#[test]` 当场中断，排在后面的用例连跑都跑不到——
/// 一次回归能把十几个用例一起藏起来。改成返回值让调用方收齐所有失败再
/// 汇总报告。`Err` 里的内容会在最终断言里原样打印出来。
fn run_case(zr: &str, source: &Path, out: &Path, opt: &str) -> Result<i32, String> {
    let compile = Command::new(zr)
        .arg(source)
        .arg(opt)
        .arg("-o")
        .arg(out)
        .output()
        .map_err(|e| format!("spawn 编译失败: {e}"))?;
    if !compile.status.success() {
        return Err(format!(
            "编译失败 ({opt}): {}\n{}",
            source.display(),
            String::from_utf8_lossy(&compile.stderr)
        ));
    }

    let run = Command::new(out)
        .output()
        .map_err(|e| format!("spawn 运行失败: {e}"))?;
    let code = run.status.code().unwrap_or(-1);
    if !run.status.success() {
        return Err(format!(
            "运行失败 ({opt}): {}\n退出码 {}\nstdout: {}\nstderr: {}",
            source.display(),
            code,
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        ));
    }
    Ok(code)
}

/// 每个 `.zr` 用例各跑一个测试，名字取自文件名。
#[test]
fn zr_language_cases() {
    let zr = env!("CARGO_BIN_EXE_zr");
    let dir = cases_dir();

    let mut cases: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("无法读取 {}: {e}", dir.display()))
        .map(|e| e.expect("目录项读取失败").path())
        .filter(|p| p.extension().map(|e| e == "zr").unwrap_or(false))
        .collect();
    cases.sort();

    assert!(!cases.is_empty(), "{} 下没有 .zr 用例", dir.display());

    // `failures` 收齐所有失败（编译失败、运行失败、断言非 0 三类），
    // 最后一次性报告。早先 `run_case` 直接 panic，只能看到第一个失败，
    // 其余用例被静默跳过。
    let mut failures: Vec<String> = Vec::new();
    for case in &cases {
        let name = case.file_stem().unwrap().to_string_lossy().to_string();
        for opt in OPT_LEVELS {
            let out = std::env::temp_dir().join(format!("zr_case_{name}"));
            let _ = std::fs::remove_file(&out);
            match run_case(zr, case, &out, opt) {
                Ok(code) if code == 0 => println!("ok   {name} {opt}"),
                Ok(code) => {
                    println!("FAIL {name} {opt} (断言 #{code})");
                    failures.push(format!("{name} {opt} 断言 #{code}"));
                }
                Err(msg) => {
                    println!("FAIL {name} {opt}\n{msg}");
                    failures.push(format!("{name} {opt}: {msg}"));
                }
            }
            let _ = std::fs::remove_file(&out);
        }
    }

    assert!(
        failures.is_empty(),
        "以下 {count} 个用例未通过：\n{list}",
        count = failures.len(),
        list = failures.join("\n")
    );
}

/// `extern "C"` 声明在运行期接通到运行时：调用 `zr_write` 把字符串字面量
/// 写到标准输出，并检查返回值。
///
/// 只测宿主目标——自行构造的 ELF（交叉编译、无平台链接器时走的那条路）不带
/// 运行时，`zr_write` 会是未定义符号。所以这个用例不放进 `tests/cases/`：
/// 那里每个用例还会在 aarch64 模拟器下再跑一遍。
/// `io` 模块的输出要和 Rust 一致：整数的十进制、浮点的"最短往返十进制"
/// （2.675 打成 "2.675" 而不是 "2.6749999999999998"）、Debug 的引号与
/// ".0"、十六进制规格、格式串里的转义序列。
#[test]
fn io_print_matches_rust() {
    let zr = env!("CARGO_BIN_EXE_zr");
    let source = r#"use io::{println_fmt, fmt_int, fmt_float, fmt_str, fmt_bool};

fn main() -> i32 {
    let a1 = [fmt_int(42), fmt_float(2.675), fmt_str("hi"), fmt_bool(true)];
    println_fmt("int={} float={} str={} bool={}", &a1[0] as *const FormatArg, 4);
    let a2 = [fmt_float(1.0), fmt_int(255), fmt_int(255)];
    println_fmt("debug={:?} hex={:x} hexu={:X} esc=a\tb\\c", &a2[0] as *const FormatArg, 3);
    return 0;
}
"#;
    let src = std::env::temp_dir().join("zr_io_print.zr");
    std::fs::write(&src, source).unwrap();

    for opt in OPT_LEVELS {
        let out = std::env::temp_dir().join("zr_io_print");
        let compile = Command::new(zr)
            .arg(&src)
            .arg(opt)
            .arg("-o")
            .arg(&out)
            .output()
            .expect("failed to spawn the zr compiler");
        assert!(
            compile.status.success(),
            "编译失败 ({opt}):\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
        let run = Command::new(&out).output().expect("failed to run program");
        assert_eq!(
            run.status.code(),
            Some(0),
            "退出码异常 ({opt}); stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(
            run.stdout,
            b"int=42 float=2.675 str=hi bool=true\ndebug=1.0 hex=ff hexu=FF esc=a\tb\\c\n",
            "标准输出不符 ({opt})"
        );
        let _ = std::fs::remove_file(&out);
    }
    let _ = std::fs::remove_file(&src);
}

#[test]
fn extern_c_write_prints_string_literal() {
    let zr = env!("CARGO_BIN_EXE_zr");
    // 运行时里的 `zr_write(fd, buf, count)` 收的是**瘦指针** `*const u8`。
    // zr 的 `str` 是胖指针（数据指针 + 长度，占两个寄存器），直接当实参
    // 传过去寄存器会对不上——所以这里按真实签名声明，再取 `s.ptr`。
    let source = r#"extern "C" {
    fn zr_write(fd: i32, buf: *const u8, count: i64) -> i64;
}

fn main() -> i32 {
    let s = "hello, extern!\n";
    let n = zr_write(1, s.ptr, 15);
    if n != 15 { return 1; }
    return 0;
}
"#;
    let src = std::env::temp_dir().join("zr_extern_write.zr");
    std::fs::write(&src, source).unwrap();

    for opt in OPT_LEVELS {
        let out = std::env::temp_dir().join("zr_extern_write");
        let compile = Command::new(zr)
            .arg(&src)
            .arg(opt)
            .arg("-o")
            .arg(&out)
            .output()
            .expect("failed to spawn the zr compiler");
        assert!(
            compile.status.success(),
            "编译失败 ({opt}):\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
        let run = Command::new(&out).output().expect("failed to run program");
        assert_eq!(
            run.status.code(),
            Some(0),
            "退出码异常 ({opt}); stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(run.stdout, b"hello, extern!\n", "标准输出不符 ({opt})");
        let _ = std::fs::remove_file(&out);
    }
    let _ = std::fs::remove_file(&src);
}
