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
/// ".0"、十六进制规格、指数形式（`{:e}` / `{:E}`）、动态宽度/精度
/// （`{:1$}` / `{:.*}`）、格式串里的转义序列。
#[test]
fn io_print_matches_rust() {
    let zr = env!("CARGO_BIN_EXE_zr");
    let source = r#"use io::{println_fmt, println_fmt_named, fmt_int, fmt_float, fmt_str, fmt_bool};

fn main() -> i32 {
    let a1 = [fmt_int(42), fmt_float(2.675), fmt_str("hi"), fmt_bool(true)];
    println_fmt("int={} float={} str={} bool={}", &a1[0] as *const FormatArg, 4);
    let a2 = [fmt_float(1.0), fmt_int(255), fmt_int(255)];
    println_fmt("debug={:?} hex={:x} hexu={:X} esc=a\tb\\c", &a2[0] as *const FormatArg, 3);
    let a3 = [fmt_float(1234.5), fmt_float(1000.0), fmt_float(1234.5)];
    println_fmt("exp={:e} {:e} {:E}", &a3[0] as *const FormatArg, 3);
    let a4 = [fmt_float(1234.5), fmt_float(0.0), fmt_float(999.0)];
    println_fmt("expp={:.2e} {:.2e} {:.2e}", &a4[0] as *const FormatArg, 3);
    let a5 = [fmt_int(1234), fmt_int(-1000), fmt_int(1299)];
    println_fmt("iexp={:e} {:e} {:.2e}", &a5[0] as *const FormatArg, 3);
    let a6 = [fmt_int(5), fmt_int(10)];
    println_fmt("dyn=[{:1$}]", &a6[0] as *const FormatArg, 2);
    let a7 = [fmt_int(5), fmt_int(10)];
    println_fmt("dync=[{:*^1$}]", &a7[0] as *const FormatArg, 2);
    let a8 = [fmt_int(2), fmt_float(1.23456)];
    println_fmt("dynp=[{:.*}]", &a8[0] as *const FormatArg, 2);
    let a9 = [fmt_float(1.23456), fmt_int(3)];
    println_fmt("dynp2=[{:.1$}]", &a9[0] as *const FormatArg, 2);
    let a10 = [fmt_int(2), fmt_int(1234)];
    println_fmt("idynp=[{:.*}]", &a10[0] as *const FormatArg, 2);
    let a11 = [fmt_float(3.14159)];
    println_fmt("w=[{:08.3}]", &a11[0] as *const FormatArg, 1);
    let a12 = [fmt_float(2.675), fmt_str("hello")];
    println_fmt("noprec=[{:.}][{:.}]", &a12[0] as *const FormatArg, 2);
    // 大浮点 Display：|v| ≥ 2⁶⁴ 时 Display 不能输出 `.0`（Rust 的 `{}` 对
    // 整数值不带小数点）。早先用 `val as u64` 取整数部分，1e30 这种值会
    // 截断/溢出，现在走位模式 + 最短往返。注意 Debug 的 `{:?}` 对 |v| ≥
    // 1e16 用科学计数法（Rust 行为），zr 的 `write_float_debug` 目前仍只
    // 输出位置计数法——那是另一处差异，不在本回归钉住的范围内。
    let a13 = [fmt_float(1.0e30), fmt_float(1.2345e30), fmt_float(-1.5e30)];
    println_fmt("big={} {} {}", &a13[0] as *const FormatArg, 3);
    // 命名参数 `{name}`：`{x}` 按名字查 `NamedArg` 表。名字没匹配按字面
    // 输出；重复用同一个名字；规格（`:>5`、`:x`）与位置参数一致。
    let na = [NamedArg { name: "x", value: fmt_int(7) }, NamedArg { name: "y", value: fmt_int(9) }];
    println_fmt_named("named {x}+{y}={x}", &na[0] as *const NamedArg, 2);
    println_fmt_named("nmiss={unknown} {x}", &na[0] as *const NamedArg, 2);
    println_fmt_named("spec=[{x:>5}] 0x{x:x}", &na[0] as *const NamedArg, 2);
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
            b"int=42 float=2.675 str=hi bool=true\ndebug=1.0 hex=ff hexu=FF esc=a\tb\\c\nexp=1.2345e3 1e3 1.2345E3\nexpp=1.23e3 0.00e0 9.99e2\niexp=1.234e3 -1e3 1.30e3\ndyn=[         5]\ndync=[****5*****]\ndynp=[1.23]\ndynp2=[1.235]\nidynp=[1234]\nw=[0003.142]\nnoprec=[2.675][hello]\nbig=1000000000000000000000000000000 1234500000000000000000000000000 -1500000000000000000000000000000\nnamed 7+9=7\nnmiss={unknown} 7\nspec=[    7] 0x7\n",
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

/// 工具链子命令 / 发射开关（§4 回归盲区）。
///
/// 逐项验证：
/// - `zr fmt` 能重排源码、`zr fmt --check` 能报告"需要格式化 / 已格式化"
///   （`--check` 对未格式化返回非 0，对已格式化返回 0——与 `git diff --exit-code`
///   的惯例一致）；
/// - `--emit-llvm` 真能写出 LLVM IR 文本（虽然目前是伪 IR，整数统一 i64）；
/// - `--wat`（wasm32）能写出 WAT 文本。
#[test]
fn toolchain_subcommands_emit() {
    let zr = env!("CARGO_BIN_EXE_zr");
    let dir = std::env::temp_dir();

    // --- zr fmt / zr fmt --check ---
    let unformatted = dir.join("zr_fmt_unformatted.zr");
    std::fs::write(&unformatted, "fn main()->i32{return 0;}\n").unwrap();
    // 未格式化时 --check 报"需要格式化"并返回非 0。
    let check = Command::new(zr)
        .arg("fmt")
        .arg("--check")
        .arg(&unformatted)
        .output()
        .expect("failed to spawn zr fmt --check");
    assert!(
        !check.status.success(),
        "未格式化源码 `zr fmt --check` 应当返回非 0"
    );
    let msg = String::from_utf8_lossy(&check.stdout);
    assert!(
        msg.contains("needs formatting"),
        "`zr fmt --check` 应当报 `needs formatting`，得到: {msg}"
    );
    // 实际格式化。
    let fmt = Command::new(zr)
        .arg("fmt")
        .arg(&unformatted)
        .output()
        .expect("failed to spawn zr fmt");
    assert!(
        fmt.status.success(),
        "`zr fmt` 失败: {}",
        String::from_utf8_lossy(&fmt.stderr)
    );
    let formatted = std::fs::read_to_string(&unformatted).unwrap();
    assert!(
        formatted.contains("fn main() -> i32 {"),
        "`zr fmt` 应把 `fn main()->i32{{` 重排成 `fn main() -> i32 {{`，得到:\n{formatted}"
    );
    // 已格式化后 --check 返回 0。
    let check2 = Command::new(zr)
        .arg("fmt")
        .arg("--check")
        .arg(&unformatted)
        .output()
        .expect("failed to spawn zr fmt --check");
    assert!(
        check2.status.success(),
        "已格式化源码 `zr fmt --check` 应当返回 0"
    );
    let _ = std::fs::remove_file(&unformatted);

    // --- --emit-llvm 写出 IR 文本 ---
    let src = dir.join("zr_emit_llvm.zr");
    std::fs::write(&src, "fn main() -> i32 {\n    let a = 1 + 2;\n    return a;\n}\n").unwrap();
    let ir_out = dir.join("zr_emit_llvm.ll");
    let emit = Command::new(zr)
        .arg(&src)
        .arg("--emit-llvm")
        .arg("-O0")
        .arg("-o")
        .arg(&ir_out)
        .output()
        .expect("failed to spawn zr --emit-llvm");
    assert!(
        emit.status.success(),
        "`--emit-llvm` 失败: {}",
        String::from_utf8_lossy(&emit.stderr)
    );
    let ir = std::fs::read_to_string(&ir_out).unwrap();
    assert!(
        ir.contains("define i64 @main()"),
        "LLVM IR 应当含 `define i64 @main()`，得到:\n{ir}"
    );
    let _ = std::fs::remove_file(&ir_out);

    // --- --wat（wasm32）写出 WAT 文本 ---
    let wat_out = dir.join("zr_emit.wat");
    let wat = Command::new(zr)
        .arg(&src)
        .arg("--wat")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("-O0")
        .arg("-o")
        .arg(&wat_out)
        .output()
        .expect("failed to spawn zr --wat");
    assert!(
        wat.status.success(),
        "`--wat` 失败: {}",
        String::from_utf8_lossy(&wat.stderr)
    );
    let wat_txt = std::fs::read_to_string(&wat_out).unwrap();
    assert!(
        wat_txt.trim_start().starts_with("(module"),
        "WAT 应当以 `(module` 开头，得到:\n{}",
        &wat_txt[..wat_txt.len().min(120)]
    );
    let _ = std::fs::remove_file(&wat_out);
    let _ = std::fs::remove_file(&src);

    // --- zr pkg init：在空目录里创建 zr.toml ---
    let pkg_dir = dir.join("zr_pkg_init_dir");
    let _ = std::fs::remove_dir_all(&pkg_dir);
    std::fs::create_dir_all(&pkg_dir).unwrap();
    let pkg = Command::new(zr)
        .arg("pkg")
        .arg("init")
        .current_dir(&pkg_dir)
        .output()
        .expect("failed to spawn zr pkg init");
    assert!(
        pkg.status.success(),
        "`zr pkg init` 失败: {}",
        String::from_utf8_lossy(&pkg.stderr)
    );
    assert!(
        pkg_dir.join("zr.toml").exists(),
        "`zr pkg init` 应创建 zr.toml"
    );
    let _ = std::fs::remove_dir_all(&pkg_dir);

    // --- zr lsp：stdio 服务在 stdin 关闭时打印 banner 并干净退出 ---
    let lsp = Command::new(zr)
        .arg("lsp")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn zr lsp");
    assert!(
        lsp.status.success(),
        "`zr lsp` 在 stdin 关闭时应干净退出"
    );
    let banner = String::from_utf8_lossy(&lsp.stderr);
    assert!(
        banner.contains("listening on stdio"),
        "`zr lsp` 应打印 `listening on stdio` banner，得到: {banner}"
    );
}

/// `#[derive(..)]` 的诊断要指到真实源码位置，而不是 `0:0:0`。
///
/// 早先 derive 在 AST 层生成 `Type__eq` / `__hash` / `__default` 等方法时全部
/// 用 `Span::unknown()`（`derive.rs` 的 `SPAN`），某字段类型没实现所需 trait、
/// 或 derive 明确拒绝的用例（泛型 / 枚举 `Default` / 枚举 `Ord`）报错位置都落
/// 在 `0:0:0`——用户只知道"有个类型缺 trait"，不知道**哪个字段**。
/// 现在 `StructDef` / `EnumDef` / `Field` 都带真实 span，生成的字段访问与
/// 拒绝诊断用字段 / 类型定义的 span。这里钉住三类：比较缺 trait、枚举
/// `Default` 拒绝、字段无默认值——报错位置都不是 `0:0:0`。
#[test]
fn derive_field_error_has_span() {
    let zr = env!("CARGO_BIN_EXE_zr");
    let dir = std::env::temp_dir();

    // 用例：源码 + 期望的报错子串。
    let cases: Vec<(&str, &str)> = vec![
        // 结构体字段缺 PartialEq。
        (
            "struct Inner { v: i32 }\n\
             #[derive(PartialEq)]\n\
             struct Outer { a: i32, b: Inner }\n\
             fn main() -> i32 { return 0; }\n",
            "cannot compare values of type `Inner`",
        ),
        // 枚举不能 derive Default。
        (
            "#[derive(Default)]\n\
             enum E { A, B(i32) }\n\
             fn main() -> i32 { return 0; }\n",
            "cannot derive `Default` for enum `E`",
        ),
        // 泛型类型上的 derive 明确拒绝。
        (
            "#[derive(Clone)]\n\
             struct G<T> { v: T }\n\
             fn main() -> i32 { return 0; }\n",
            "`#[derive(..)]` on generic type `G`",
        ),
        // 某字段类型没 derive Default（递归找 `S__default` 失败）。
        (
            "struct S { x: f64 }\n\
             #[derive(Default)]\n\
             struct T { a: i32, b: S }\n\
             fn main() -> i32 { return 0; }\n",
            "cannot find value `S__default`",
        ),
    ];

    for (i, (src_text, expect)) in cases.iter().enumerate() {
        let src = dir.join(format!("zr_derive_field_span_{i}.zr"));
        let out = dir.join(format!("zr_derive_field_span_{i}.bin"));
        std::fs::write(&src, src_text).expect("写临时源码失败");
        let compile = Command::new(zr)
            .arg(&src)
            .arg("-O0")
            .arg("-o")
            .arg(&out)
            .output()
            .expect("spawn 编译失败");
        assert!(!compile.status.success(), "用例 {i} 应编译失败");
        let stderr = String::from_utf8_lossy(&compile.stderr);
        assert!(
            stderr.contains(expect),
            "用例 {i} 应报 `{expect}`，得到: {stderr}"
        );
        // 报错位置 `zr: <line>:<col>:<off>:` 的三个数字不能全为 0。
        assert!(
            !stderr.starts_with("zr: 0:0:0:"),
            "用例 {i} 应指向真实源码位置，而不是 0:0:0，得到: {stderr}"
        );
    }
}
