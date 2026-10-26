//==============================================================================
// tests/zr_negative.rs — "该报错的还在报错"回归
//
// `tests/cases/` 下每个 `.zr` 都必须编译通过并返回 0；那里放不下**应该失败**
// 的用例。本文件补上另一半：`tests/negative/` 下每个 `.zr` 必须**编译失败**，
// 且报错文本必须包含文件里 `// expect:` 标注的期望子串。
//
// 为什么要单独一个 `#[test]`：`zr_language_cases` 用 `read_dir` 扫描
// `tests/cases`，两个目录分开就不会互相污染（放同一个目录，负向用例会被
// 当成编译失败、正向用例的返回码约定也和这里相反）。
//
// 为什么用子串而不是精确匹配：报错文案里的行号/列号/字节偏移会随无关改动
// 漂移，精确匹配会让整个套件变成"改一个字都红"。子串只钉住**语义**——
// 报的是哪个错、关键词是什么——而把格式细节留给编译器自由演进。
//
// 约定：每个负向用例是一行注释 `// expect: <期望子串>`，可以是任意一行
// （不要求是文件最后一行），也可以完全没有——没有期望时只要求"确实报错"。
//==============================================================================

use std::path::{Path, PathBuf};
use std::process::Command;

/// 负向用例目录。
fn negative_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/negative")
}

/// 提取用例里的 `// expect:` 期望子串（按行序去重保序）。
///
/// `// expect:` 前面可以有任意空白（缩进、制表符），但不允许前面有非空白
/// 的注释内容——那样写出来的是说明而不是断言，会被静默忽略，很难发现。
fn expectations(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let Some(t) = line.trim().strip_prefix("// expect:") else {
            continue;
        };
        let expect = t.trim();
        if expect.is_empty() {
            continue;
        }
        if !out.iter().any(|e| e == expect) {
            out.push(expect.to_string());
        }
    }
    out
}

/// 编译一个负向用例，返回 (退出码, stderr)。不 panic——把所有失败收齐再报。
fn compile_failure(zr: &str, source: &Path, out: &Path) -> Result<(i32, String), String> {
    let res = Command::new(zr)
        .arg(source)
        .arg("-O0")
        .arg("-o")
        .arg(out)
        .output()
        .map_err(|e| format!("spawn 编译失败: {e}"))?;
    Ok((res.status.code().unwrap_or(-1), String::from_utf8_lossy(&res.stderr).to_string()))
}

#[test]
fn negative_cases_reject_as_expected() {
    let zr = env!("CARGO_BIN_EXE_zr");
    let dir = negative_dir();

    let mut cases: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("无法读取 {}: {e}", dir.display()))
        .map(|e| e.expect("目录项读取失败").path())
        .filter(|p| p.extension().map(|e| e == "zr").unwrap_or(false))
        .collect();
    cases.sort();

    assert!(!cases.is_empty(), "{} 下没有 .zr 用例", dir.display());

    let mut failures: Vec<String> = Vec::new();
    for case in &cases {
        let name = case.file_stem().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(case)
            .unwrap_or_else(|e| panic!("无法读取 {}: {e}", case.display()));
        let expects = expectations(&src);
        let out = std::env::temp_dir().join(format!("zr_neg_{name}"));
        let _ = std::fs::remove_file(&out);

        let (code, stderr) = match compile_failure(zr, case, &out) {
            Ok(r) => r,
            Err(msg) => {
                failures.push(format!("{name}: {msg}"));
                continue;
            }
        };

        // 负向用例的第一条断言：它必须真的编不过。这里红了说明编译器
        // 把一个本该拒绝的程序放行了——是静默错误，比报错本身严重。
        if code == 0 {
            failures.push(format!(
                "{name}: 编译**通过**了，但这个用例本该报错\n期望包含: {expects:?}"
            ));
        } else {
            // 逐条核对期望子串（大小写不敏感，避免中英文标点抖动）
            let lower: Vec<String> = expects.iter().map(|e| e.to_lowercase()).collect();
            let have = stderr.to_lowercase();
            for (i, want) in lower.iter().enumerate() {
                if !have.contains(want.as_str()) {
                    failures.push(format!(
                        "{name}: 期望报错包含「{}」(第 {i} 条)，实际:\n{stderr}",
                        expects[i]
                    ));
                }
            }
        }
        let _ = std::fs::remove_file(&out);
    }

    if !failures.is_empty() {
        println!("负向用例失败：\n{}", failures.join("\n\n"));
        panic!(
            "以下 {} 个负向用例未通过：\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
