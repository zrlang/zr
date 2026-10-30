//! zr 编译器主入口。
//!
//! 编译流水线：
//! ```text
//! 源码 (.zr)
//!   → lexer     词法分析
//!   → parser    语法分析（递归下降 + Pratt 表达式）
//!   → typeck    类型检查 + 类型推导（Hindley-Milner 子集）
//!   → mir       中级表示（带基本块与终结器）
//!   → ssa       静态单赋值形式（支配树 + φ 节点）
//!   → opt       优化（常量折叠 / DCE / CSE / GVN / 内联 / 循环）
//!   → codegen   x86-64 指令选择 + 寄存器分配（线性扫描）
//!   → ELF       Linux x86-64 可执行文件
//! ```

#![allow(clippy::module_name_repetitions)]
#![warn(unused_imports, unused_variables, dead_code)]

mod attributes;
mod codegen;
mod comptime;
mod error;
mod fmt;
mod json;
mod lexer;
mod lsp;
mod macros;
mod mir;
mod modules;
mod opt;
mod parser;
mod pkg;
mod ssa;
mod typeck;
mod zpm;

use std::env;
use std::path::Path;
use std::process::ExitCode;

use crate::codegen::target::{Arch, ObjectFormat, Os, Target};
use crate::opt::OptLevel;

/// `zr fmt [--check] <file.zr>`
///
/// `--check` 不写回文件：不符合规范格式就返回退出码 1，方便接进 CI。
fn run_fmt(cmd: &Cli, path: &str) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zr: cannot read {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let formatted = fmt::format_source(&source);
    let already = formatted == fmt::normalize_line_endings(&source);

    // 安全检查：格式化**只允许**改空白与间距，不允许改记号序列。
    //
    // 这里刻意**不**拿解析器当守门人：格式化器必须能处理编译器还不认识的
    // 语法（IDE 里随手按一下保存键就有这种文件），否则 `zr fmt` 直接罢工。
    // "记号流不变"比"能解析"更本质，也更贴合格式化的定义。
    let before = code_tokens(&source);
    let after = code_tokens(&formatted);
    if before != after {
        eprintln!("zr: internal error: formatting changed the token stream");
        eprintln!("zr: {} tokens -> {} tokens", before.len(), after.len());
        for (i, (a, b)) in before.iter().zip(after.iter()).enumerate() {
            if a != b {
                eprintln!("zr: first difference at #{i}: {a:?} vs {b:?}");
                break;
            }
        }
        return ExitCode::from(2);
    }

    if cmd.fmt_check {
        if already {
            println!("{path}: already formatted");
            return ExitCode::SUCCESS;
        }
        println!("{path}: needs formatting");
        return ExitCode::from(1);
    }

    if already {
        println!("{path}: already formatted");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = std::fs::write(path, &formatted) {
        eprintln!("zr: cannot write {path}: {e}");
        return ExitCode::from(2);
    }
    println!("{path}: formatted");
    ExitCode::SUCCESS
}

/// 记号序列（去掉注释与 EOF），用于验证格式化没有改变语义单元。
fn code_tokens(src: &str) -> Vec<lexer::TokenKind> {
    lexer::Lexer::new(src)
        .tokenize()
        .into_iter()
        .map(|t| t.kind)
        .filter(|k| {
            !matches!(
                k,
                lexer::TokenKind::Eof
                    | lexer::TokenKind::LineComment(_)
                    | lexer::TokenKind::BlockComment(_)
            )
        })
        .collect()
}

const SERVER_LSP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `zr build <file.zr>`：编译生成可执行文件（类似 `go build`）。
fn run_build(cmd: &Cli, input: &str) -> ExitCode {
    // build 与普通编译流程一致，只是输出信息不同
    match compile(input, cmd) {
        Ok(out_path) => {
            eprintln!(
                "zr: built {} -> {} ({})",
                input,
                out_path.display(),
                cmd.opt_level.name()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zr: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `zr run <file.zr> [-- <args...>]`：编译并运行（类似 `go run`）。
fn run_run(cmd: &Cli, input: &str) -> ExitCode {
    match compile(input, cmd) {
        Ok(out_path) => {
            // 运行编译产物
            let mut command = std::process::Command::new(&out_path);
            command.args(&cmd.run_args);
            match command.status() {
                Ok(status) => {
                    if status.success() {
                        ExitCode::SUCCESS
                    } else {
                        eprintln!("zr: process exited with {}", status);
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("zr: failed to run {}: {}", out_path.display(), e);
                    ExitCode::FAILURE
                }
            }
        }
        Err(e) => {
            eprintln!("zr: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    // 语法树递归下降、类型检查、MIR 降级都是深递归。默认 8 MiB 线程栈
    // 在几百行的一个函数上就会溢出（实测约 300 个局部变量就 abort），
    // 而溢出是 abort 而不是可恢复的错误——用户只会看到一句
    // "has overflowed its stack"。这里把整个编译搬到 256 MiB 的栈上跑。
    let worker = std::thread::Builder::new()
        .name("zr-compile".into())
        .stack_size(256 * 1024 * 1024)
        .spawn(real_main);
    match worker {
        Ok(h) => match h.join() {
            Ok(code) => code,
            Err(_) => {
                eprintln!("zr: internal error: compiler thread panicked");
                ExitCode::from(2)
            }
        },
        Err(e) => {
            eprintln!("zr: cannot spawn compiler thread: {e}");
            ExitCode::from(2)
        }
    }
}

fn real_main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let cmd = match Cli::parse(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("zr: {msg}");
            eprintln!();
            print_usage(&args[0]);
            return ExitCode::from(2);
        }
    };

    if cmd.version {
        let host = Target::host();
        println!("zr {}", env!("CARGO_PKG_VERSION"));
        println!("host: {} ({})", host.triple(), host.format().name());
        println!("targets: {}", Target::supported_triples().join(", "));
        return ExitCode::SUCCESS;
    }

    if cmd.help {
        print_usage(&args[0]);
        return ExitCode::SUCCESS;
    }

    // `zr lsp`：语言服务器，跑 stdio 循环直到 `exit`。
    if cmd.lsp {
        eprintln!("zr-lsp {} listening on stdio", SERVER_LSP_VERSION);
        lsp::Server::new().run();
        return ExitCode::SUCCESS;
    }

    // `zr pkg <子命令>`：不需要输入文件。
    if let Some(sub_args) = &cmd.pkg {
        return match pkg::run(sub_args) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => {
                print_pkg_usage();
                ExitCode::from(2)
            }
            Err(e) => {
                eprintln!("zr: {e}");
                ExitCode::from(2)
            }
        };
    }

    // `zr clean`：清空增量缓存（不需要输入文件）。
    if cmd.clean_cache {
        return match modules::clean() {
            Ok(n) => {
                println!("removed {n} cache entr{}", if n == 1 { "y" } else { "ies" });
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("zr: {e}");
                ExitCode::from(2)
            }
        };
    }

    // `zr build`：编译生成可执行文件（类似 `go build`）。
    if cmd.build {
        let input = match cmd.input.as_deref() {
            Some(p) => p,
            None => {
                eprintln!("zr: build requires an input file");
                return ExitCode::from(2);
            }
        };
        return run_build(&cmd, input);
    }

    // `zr run`：编译并运行（类似 `go run`）。
    if cmd.run {
        let input = match cmd.input.as_deref() {
            Some(p) => p,
            None => {
                eprintln!("zr: run requires an input file");
                return ExitCode::from(2);
            }
        };
        return run_run(&cmd, input);
    }

    let input = match cmd.input.as_deref() {
        Some(p) => p,
        None => {
            print_usage(&args[0]);
            return ExitCode::SUCCESS;
        }
    };

    // `zr fmt` 走独立路径：不编译、不做类型检查。
    if cmd.fmt {
        return run_fmt(&cmd, input);
    }

    match compile(input, &cmd) {
        Ok(out_path) => {
            // 编译成功后才落缓存：失败的编译不能留下"下次直接复用"的记录
            if let Some((fp, artifact, files)) = CACHE_FOR_STORE.with(|c| c.borrow_mut().take()) {
                // 缓存键必须和查询时用的**同一个路径**（源文件），早先这里
                // 误用了输出路径，于是存和取落在两个不同的键上，永远命中不了。
                let root = std::fs::canonicalize(input)
                    .unwrap_or_else(|_| std::path::PathBuf::from(input));
                let mut cache = modules::Cache::open(&root);
                if let Err(e) = cache.store(fp, &artifact, &files) {
                    if cmd.verbose {
                        eprintln!("zr: {e}");
                    }
                }
            }
            eprintln!(
                "zr: compiled {} -> {} ({})",
                input,
                out_path.display(),
                cmd.opt_level.name()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zr: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 命令行参数。
#[derive(Debug, Default)]
struct Cli {
    input: Option<String>,
    output: std::path::PathBuf,
    opt_level: OptLevel,
    dump_tokens: bool,
    dump_ast: bool,
    dump_hir: bool,
    dump_mir: bool,
    dump_ssa: bool,
    dump_ssa_after: bool,
    dump_asm: bool,
    emit_llvm: bool,
    no_link: bool,
    target: String,
    /// 输出 WebAssembly 文本（WAT）而不是二进制模块。
    wat: bool,
    /// 生成 DWARF 调试信息。
    debug_info: bool,
    help: bool,
    version: bool,
    /// 增量编译（默认开启；`--no-incremental` 关闭）。
    incremental: bool,
    /// `-v`：打印缓存命中这类诊断信息。
    verbose: bool,
    /// 清空增量缓存。
    clean_cache: bool,
    /// `zr lsp`：启动语言服务器。
    lsp: bool,
    /// `zr pkg <子命令> <参数...>`（原样转交给 pkg 模块）。
    pkg: Option<Vec<String>>,
    /// `zr fmt`：格式化源码。
    fmt: bool,
    /// `zr fmt --check`：只检查是否为规范格式，不写回文件。
    fmt_check: bool,
    /// `zr build`：编译但不运行（类似 `go build`）。
    build: bool,
    /// `zr run`：编译并运行（类似 `go run`）。
    run: bool,
    /// `zr run` 传递给程序的参数（`--` 之后的参数）。
    run_args: Vec<String>,
}

impl Cli {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut cli = Cli {
            // 增量编译默认开启（缓存只在"什么都没改"时命中，绝不会给出
            // 过时结果，所以默认开着是安全的）
            incremental: true,
            ..Cli::default()
        };
        let mut i = 1;
        while i < args.len() {
            let a = &args[i];
            match a.as_str() {
                "-h" | "--help" => cli.help = true,
                "-V" | "--version" => cli.version = true,
                "-o" => {
                    i += 1;
                    cli.output = args
                        .get(i)
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| "missing argument to '-o'".to_string())?;
                }
                "--dump-tokens" => cli.dump_tokens = true,
                "--dump-ast" => cli.dump_ast = true,
                "--dump-hir" => cli.dump_hir = true,
                "--dump-mir" => cli.dump_mir = true,
                "--dump-ssa" => cli.dump_ssa = true,
                "--dump-ssa-after" => cli.dump_ssa_after = true,
                "--dump-asm" => cli.dump_asm = true,
                "--emit-llvm" => cli.emit_llvm = true,
                "--wat" => cli.wat = true,
                "--no-incremental" => cli.incremental = false,
                "-v" | "--verbose" => cli.verbose = true,
                "clean" => cli.clean_cache = true,
                // `pkg` 之后的一切都原样转交给包管理器，不���再当成输入文件
                // ——早先把 `pkg init` 的两个词都当成位置参数，于是直接
                // 报 "multiple input files not supported yet"。
                "lsp" | "lsp-server" => {
                    cli.pkg = None;
                    cli.lsp = true;
                    return Ok(cli);
                }
                "pkg" => {
                    cli.pkg = Some(args[i + 1..].to_vec());
                    return Ok(cli);
                }
                "build" => cli.build = true,
                "run" => cli.run = true,
                "fmt" | "format" => cli.fmt = true,
                "--check" => cli.fmt_check = true,
                "-c" => cli.no_link = true,
                "--target" => {
                    i += 1;
                    cli.target = args
                        .get(i)
                        .ok_or_else(|| "missing argument to '--target'".to_string())?
                        .clone();
                }
                "-O0" => cli.opt_level = OptLevel::O0,
                "-O1" => cli.opt_level = OptLevel::O1,
                "-O2" => cli.opt_level = OptLevel::O2,
                "-O3" => cli.opt_level = OptLevel::O3,
                "-Oz" => cli.opt_level = OptLevel::Oz,
                "-g" | "--debug" => cli.debug_info = true,
                "--" => {
                    // `--` 后的所有参数传给程序运行
                    cli.run_args = args[i + 1..].to_vec();
                    break;
                }
                s if s.starts_with('-') => return Err(format!("unknown argument '{s}'")),
                _ => {
                    if cli.input.is_some() {
                        return Err("multiple input files not supported yet".to_string());
                    }
                    cli.input = Some(a.clone());
                }
            }
            i += 1;
        }
        // 无输入文件时默认显示帮助——但 `clean` / `fmt --check` / `build` / `run` 这类
        // 子命令本来就不需要输入文件，不能被这个默认值吃掉。
        if cli.input.is_none()
            && !cli.clean_cache
            && !(cli.fmt && cli.fmt_check)
            && !cli.build
            && !cli.run
        {
            cli.help = true;
        }
        // 没给 `-o` 就用输入文件名当输出名。少了这一步输出路径是空串，
        // 平台链接器会收到一个空的 `-o` 参数而失败。
        if cli.output.as_os_str().is_empty() {
            if let Some(input) = &cli.input {
                cli.output = std::path::PathBuf::from(input);
            }
        }
        Ok(cli)
    }
}

fn print_pkg_usage() {
    println!("zr pkg <subcommand>");
    println!();
    println!("    init                    Create a zr.toml in the current directory");
    println!("    add <name> <path>       Add a path dependency");
    println!("    add <name>@<version>    Add a dependency from registry");
    println!("    remove <name>           Remove a dependency");
    println!("    list                    List dependencies");
    println!("    tree                    Print the dependency graph");
    println!("    build                   Build all dependencies in order");
    println!("    clean                   Remove build artifacts");
    println!("    search <query> [limit]  Search packages in registry");
    println!("    publish <name> <version> Publish package to registry");
    println!("    fetch <name> <version>  Fetch package from registry");
}

fn print_usage(prog: &str) {
    println!("zr {} — zr language compiler", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("    {prog} [OPTIONS] <INPUT.zr>");
    println!("    {prog} build [OPTIONS] <INPUT.zr>");
    println!("    {prog} run [OPTIONS] <INPUT.zr> [-- <ARGS...>]");
    println!();
    println!("COMMANDS:");
    println!("    build           Compile to executable (like go build)");
    println!("    run             Compile and run (like go run)");
    println!("    fmt             Format source code");
    println!("    pkg             Package manager");
    println!("    lsp             Start language server");
    println!("    clean           Clean incremental cache");
    println!();
    println!("OPTIONS:");
    println!("    -o <file>           Output file");
    println!("    -O0..-O3, -Oz       Optimization level");
    println!("    -c                  Compile only, do not link");
    println!("    --target <triple>   Target triple (default: host)");
    println!("    --wat               Emit WebAssembly text (wasm32 only)");
    println!("    --dump-tokens      Dump lexer output");
    println!("    --dump-ast          Dump AST");
    println!("    --dump-hir          Dump HIR");
    println!("    --dump-mir          Dump MIR");
    println!("    --dump-ssa          Dump SSA");
    println!("    --dump-ssa-after    Dump SSA after optimization");
    println!("    --dump-asm          Dump assembly");
    println!("    --emit-llvm         Emit LLVM IR (future)");
    println!("    -h, --help          Show this help");
    println!("    -V, --version       Print version");
}

/// 编译单个文件。
/// 各编译阶段的耗时（`ZR_TIME=1` 时打印到 stderr）。
///
/// 增量编译要优化的是"大头"。没有这份数据就只能凭感觉猜，而实测下来
/// 这个编译器的瓶颈并不在词法/语法上。
fn time_phase(t0: std::time::Instant, name: &str) {
    if std::env::var_os("ZR_TIME").is_some() {
        eprintln!("[time] {name:>12}: {:?}", t0.elapsed());
    }
}

type StoreCache = (u64, std::path::PathBuf, Vec<(std::path::PathBuf, u64)>);

// 待写入的缓存条目：`compile()` 成功后才真正落盘。
thread_local! {
    static CACHE_FOR_STORE: std::cell::RefCell<Option<StoreCache>> =
        const { std::cell::RefCell::new(None) };
}

/// 指纹盐：编译器版本 + 目标 + 优化等级 + 调试信息 + 是否只编译不链接
/// + 源文件路径。
///
/// 任何一个变了都必须重编——否则 `--target` 换一下就会拿到上一个目标的产物，
/// 那比不缓存糟糕得多。
fn build_salt(cmd: &Cli, target: &Target, input: &str) -> String {
    format!(
        "v{}|{}|{:?}|{}|{}|{}|{}|{}",
        env!("CARGO_PKG_VERSION"),
        target.triple(),
        cmd.opt_level,
        cmd.debug_info,
        if cmd.no_link { "c" } else { "link" },
        if cmd.wat { "wat" } else { "bin" },
        input,
        compiler_identity()
    )
}

/// 编译器自身的身份：可执行文件的大小与修改时间。
///
/// 盐里只带包版本号是不够的：从 git 检出重新构建编译器后包版本没变，
/// 增量缓存的键也就没变——上个编译器编出来的产物会被当成"没变"直接复用，
/// 用户拿到的是旧二进制。把可执行文件的大小与修改时间混进盐里，编译器
/// 一重编，缓存自然失效。
fn compiler_identity() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok())
        .map(|m| {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{}:{}", m.len(), mtime)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn compile(input: &str, cmd: &Cli) -> Result<std::path::PathBuf, String> {
    let t0 = std::time::Instant::now();
    // 0. 解析目标三元组（缺省为宿主平台）
    let target = resolve_target(&cmd.target)?;
    if !target.is_supported() {
        return Err(format!(
            "unsupported target '{}'; supported targets are: {}",
            cmd.target,
            Target::supported_triples().join(", ")
        ));
    }

    // 0.5 cfg 上下文：按**编译目标**求值 `#[cfg(..)]` 条件。
    //
    // 必须在解析之前设好：被 cfg 掉的条目在解析阶段就被剔除（它们对名称
    // 解析而言根本不存在），类型检查阶段再兜一遍。
    let cfg_target = attributes::CfgTarget::from_target(&target);
    attributes::set_global_target(cfg_target.clone());

    // 1. 读取源文件
    let source =
        std::fs::read_to_string(input).map_err(|e| format!("cannot read '{input}': {e}"))?;

    // 2. 词法分析
    let lexer = lexer::Lexer::new(&source);
    let tokens = lexer.tokenize();
    time_phase(t0, "lex");
    if cmd.dump_tokens {
        println!("=== Tokens ({}) ===", tokens.len());
        for t in &tokens {
            println!("  {:?}", t);
        }
    }

    // 3. 语法分析
    let mut parser = parser::Parser::new(tokens);
    let mut ast = parser.parse().map_err(|e| e.to_string())?;
    time_phase(t0, "parse");
    if cmd.dump_ast {
        println!("=== AST ===");
        println!("{:#?}", ast);
    }

    // 3.5 注入 prelude：`Option` / `Result`（连同它们的枚举变体 `Some` /
    // `None` / `Ok` / `Err`）是每个 zr 程序最常用的类型，要求每个程序都手写
    // `use core::{Option, Result, Some, None, Ok, Err}` 是不必要的样板。这里
    // 在解析后、类型检查前把这些隐式 use 插到用户导入之前——用户再写同名
    // use 也不会冲突（类型检查器会去重）。
    //
    // 必须构造正确的 `UseTree::Group([Name("core"), Name(...)])` 结构
    // ——早先写成 `Name("Option")` 会让模块加载器误把 `Option` 当成模块名，
    // 报 "unknown module `Option`"。
    //
    // 变体用 `Group([Name("core"), Group([Name("Option"), Name("Some")])])`
    // 嵌套导入（与 `parse_use_tree` 的段追加顺序一致——每往右推一层就包一个
    // 新 Group，旧树是 Group 的第一个元素）；枚举变体在 zr 里是模块内符号，
    // 不会与用户本地的同名绑定冲突（同名时用户的绑定遮蔽 prelude）。
    // 把一条 `use` 路径注入 root 程序（带一个"把新段追加到现有树"的顺序）。
    //
    // `use core::Option` → `Group([Name("core"), Name("Option")])`
    // `use core::Option::Some` → `Group([Name("core"), Group([Name("Option"), Name("Some")])])`
    let inject_import = |ast: &mut parser::ast::Program, path: &str| {
        let segments: Vec<&str> = path.split("::").collect();
        if segments.is_empty() {
            return;
        }
        let mut tree = parser::ast::UseTree::Name(segments[segments.len() - 1].to_string());
        for &seg in segments.iter().rev().skip(1) {
            tree = parser::ast::UseTree::Group(vec![
                parser::ast::UseTree::Name(seg.to_string()),
                tree,
            ]);
        }
        ast.imports.insert(
            0,
            parser::ast::UseDecl {
                tree,
                span: parser::ast::Span::unknown(),
            },
        );
    };
    const PRELUDE_IMPORTS: &[&str] = &[
        "core::Option",
        "core::Result",
        "core::Option::Some",
        "core::Option::None",
        "core::Result::Ok",
        "core::Result::Err",
        // `#[derive(Debug)]` 生成的 `Type__debug(self) -> String` 直接用
        // `String` 构造/拼接，用户不必在用到 derive 的模块里手写
        // `use string::String`。`std/string.zr` 只依赖 `core`/`mem`
        // （早先那条 `use io::..` 是死导入，会把 `io.zr` 拖进每个程序），
        // 所以无条件注入的代价可忽略。
        "string::String",
    ];
    for path in PRELUDE_IMPORTS {
        inject_import(&mut ast, path);
    }

    // 3.4 增量编译命中检查。
    //
    // 必须在**加载模块之前**：命中时只按缓存里记的清单重新哈希文件，
    // 不解析任何一个源码，于是词法/语法/类型检查/MIR/代码生成/链接
    // 全都跳过——这才是增量编译能省下的东西。放在后面只能省下 SSA
    // 之后的几段，收益小得多。
    let root_abs = std::fs::canonicalize(input).unwrap_or_else(|_| std::path::PathBuf::from(input));
    if cmd.incremental {
        let cache = modules::Cache::open(&root_abs);
        if cache.try_reuse(&build_salt(cmd, &target, input)) {
            let out = output_with_extension(Path::new(&cmd.output), &target);
            if cache.restore(&out).is_ok() {
                time_phase(t0, "incremental-hit");
                if cmd.verbose {
                    eprintln!("zr: up to date, reused cached artifact");
                }
                return Ok(out);
            }
        }
    }

    // 3.5 加载模块：`mod foo;` 指向的文件 + `use` 到的标准库模块，
    //     一并并进 `ast`，同时记录读过的文件指纹供增量编译用。
    let root_dir = std::path::Path::new(input)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let loaded = modules::load(&mut ast, &root_dir);
    if let Some(e) = loaded.error {
        return Err(e);
    }

    // 3.6 增量编译：主文件 + 所有模块文件的内容指纹都没变，就复用上次产物。
    //
    // 指纹里混入了目标三元组、优化等级、调试信息开关与编译器版本——
    // 任何一个变了都必须重编，否则 `--target` 换一下就会拿到上一个目标的
    // 产物，那比不缓存糟糕得多。
    let salt = build_salt(cmd, &target, input);
    let mut all_files: Vec<(std::path::PathBuf, u64)> = loaded.report.files.clone();
    if let Ok(bytes) = std::fs::read(input) {
        all_files.push((root_abs.clone(), modules::fingerprint(&bytes)));
    }
    let fingerprint = modules::build_fingerprint(&all_files, &salt);
    if cmd.incremental {
        CACHE_FOR_STORE.with(|c| {
            *c.borrow_mut() = Some((
                fingerprint,
                output_with_extension(Path::new(&cmd.output), &target),
                all_files,
            ))
        });
    }

    // 4. 类型检查 + HIR
    //
    // 宏展开在类型检查之前：宏调用点被替换成真正的表达式，类型检查器
    // 就看不到 `MacroCall` 了。展开必须在**模块加载之后**做——宏定义
    // 可能来自标准库模块。
    let ast = macros::expand(&ast).map_err(|e| e.to_string())?;

    let mut checker = typeck::TypeChecker::new();
    checker.set_known_modules(loaded.modules.iter().cloned().collect());
    // 内联模块（`mod af { .. }`）的条目扁平并入顶层，但 `af::INET` 这样的
    // 限定路径仍要解析：`resolve_path` 据此把叶子段拿到顶层符号表。
    checker.set_flattened_modules(loaded.flattened.iter().cloned().collect());
    // 每个模块导出的条目名：`use core::*` 要按它导入全部顶层名字。
    checker.set_module_exports(loaded.module_exports);
    // 32 位目标支持多字长（i64）运算：值走 8 字节栈槽，用两条 32 位指令
    // 计算。未实现的运算（如 i64 乘法/除法）在后端报错，不会静默截断。
    checker.set_max_int_bits(64);
    checker.set_pointer_bits(target.pointer_size() as u16 * 8);
    // cfg 条件按编译目标求值（交叉编译到 Linux 时 `target_os = "linux"` 成立）。
    checker.set_cfg_target(cfg_target);
    let hir = checker.check_program(&ast).map_err(|e| e.to_string())?;
    time_phase(t0, "typeck");
    if cmd.dump_hir {
        println!("=== HIR ===");
        println!("{:#?}", hir);
    }

    // 4.5 提取属性（链接指令、repr 布局）到编译单元
    let mut unit = codegen::CompilationUnit::new();
    codegen::extract_attributes(&mut unit, &hir, &ast);

    

    // 5. MIR 降级
    let mir_module = mir::lower(&hir);
    time_phase(t0, "mir");
    if cmd.dump_mir {
        println!("=== MIR ===");
        println!("{:#?}", mir_module);
    }

    // 6. SSA 转换
    let mut ssa_module = ssa::from_mir(&mir_module);
    if cmd.dump_ssa {
        println!("=== SSA ===");
        ssa::print_module(&ssa_module);
    }

    // 7. 优化
    ssa_module = opt::run(ssa_module, cmd.opt_level);
    if cmd.dump_ssa_after {
        println!("=== SSA (after {} optimization) ===", cmd.opt_level.name());
        ssa::print_module(&ssa_module);
    }

    // 7.5 发射 LLVM IR（若指定 --emit-llvm）
    if cmd.emit_llvm {
        let ir = emit_llvm_ir(&target, &ssa_module);
        let out = Path::new(&cmd.output).to_path_buf();
        std::fs::write(&out, ir)
            .map_err(|e| format!("cannot write LLVM IR to '{}': {e}", out.display()))?;
        return Ok(out);
    }

    // 7.6 目标选择：wasm 走栈式字节码后端
    if target.format() == ObjectFormat::Wasm {
        let wasm = codegen::wasm::WasmModule::from_ssa(&ssa_module, target.arch());
        let bytes: Vec<u8> = if cmd.wat {
            wasm.to_wat().into_bytes()
        } else {
            wasm.to_binary()
        };
        let out = output_with_extension(Path::new(&cmd.output), &target);
        std::fs::write(&out, bytes)
            .map_err(|e| format!("cannot write wasm to '{}': {e}", out.display()))?;
        return Ok(out);
    }

    // 8. 代码生成（按目标架构选择指令后端）
    let unit = codegen::compile(&target, &ssa_module, &mir_module.statics)?;

    // 8.5 机器码转储（--dump-asm）
    if cmd.dump_asm {
        println!("=== Machine code ({}) ===", target.describe());
        for (i, chunk) in unit.code.chunks(16).enumerate() {
            let hex: String = chunk
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("  {:04x}:  {}", i * 16, hex);
        }
    }

    // 9. 输出目标文件 / 可执行文件
    if cmd.no_link {
        let obj = object_output_path(Path::new(&cmd.output), &target);
        codegen::emit_object(&target, &unit, &obj).map_err(|e| format!("object writer: {e}"))?;
        Ok(obj)
    } else {
        let exe = output_with_extension(Path::new(&cmd.output), &target);
        codegen::emit_executable(&target, &unit, &exe).map_err(|e| {
            format!(
                "linker: {e}\nnote: {}",
                codegen::runtime_diagnostic(&target)
            )
        })?;
        make_executable(&exe, &target)?;
        Ok(exe)
    }
}

/// root 程序（含 `mod foo;` 加载进来的模块与内联模块）里是否出现
/// 解析 `--target`（空串表示宿主平台）。
fn resolve_target(triple: &str) -> Result<Target, String> {
    if triple.is_empty() {
        Ok(Target::host())
    } else {
        Target::parse(triple)
    }
}

/// 推导产物文件名。
///
/// 规则：
/// * `foo.zr` → `foo`（去掉源码后缀）再补平台扩展名。**必须**去掉，
///   否则默认输出会把源文件本身覆盖掉；
/// * `foo`    → `foo` + 平台扩展名（Windows 的 `.exe` / wasm 的 `.wasm`）；
/// * `foo.bin` → 原样保留（用户显式指定了产物名）。
fn output_with_extension(path: &Path, target: &Target) -> std::path::PathBuf {
    let platform_ext = target.exe_extension();
    match path.extension().and_then(|e| e.to_str()) {
        // 源文件名：去掉 `.zr` 再补平台后缀
        Some("zr") => {
            let base = path.with_extension("");
            if platform_ext.is_empty() {
                base
            } else {
                let mut name = base.as_os_str().to_os_string();
                name.push(platform_ext);
                std::path::PathBuf::from(name)
            }
        }
        Some(_) => path.to_path_buf(),
        None => {
            if platform_ext.is_empty() {
                path.to_path_buf()
            } else {
                let mut name = path.as_os_str().to_os_string();
                name.push(platform_ext);
                std::path::PathBuf::from(name)
            }
        }
    }
}

/// `-c` 的目标文件名：没有扩展名时补 `.o`。
fn object_output_path(path: &Path, target: &Target) -> std::path::PathBuf {
    let _ = target;
    match path.extension() {
        Some(_) => path.to_path_buf(),
        None => {
            let mut name = path.as_os_str().to_os_string();
            name.push(".o");
            std::path::PathBuf::from(name)
        }
    }
}

/// 标记产物可执行。
///
/// POSIX 需要 `+x` 位；Windows 的 PE 依赖文件扩展名，不需要权限位。
/// 两套写法按平台条件编译，保证每个平台只引用自己存在的 API。
fn make_executable(path: &Path, target: &Target) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if target.os() != Os::Win && path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("cannot mark '{}' executable: {e}", path.display()))?;
        }
    }
    #[cfg(not(unix))]
    {
        // 非 Unix 宿主：Windows 产物无需权限位，wasm 模块由宿主加载。
        let _ = (path, target);
    }
    Ok(())
}

/// 将 SSA 模块以 LLVM IR 文本形式输出（简化版）。
///
/// 该发射器把 SSA 指令映射为接近 LLVM IR 的文本表示，便于调试与
/// 对接外部 LLVM 工具链。类型信息做了保守降级（整数统一为 i64，
/// 浮点为 double，指针为 i8*）。
fn emit_llvm_ir(target: &Target, module: &ssa::SsaModule) -> String {
    use ssa::{SsaBinaryOp, SsaInstr, SsaTerminator, SsaUnaryOp};

    let ptr = target.pointer_size();
    let datalayout = format!("e-m:e-i{ptr}:{ptr}-f80:128-n8:16:32:64-S128");
    let mut out = String::new();
    out.push_str("; Module generated by zr compiler (LLVM IR emission)\n");
    out.push_str(&format!("target datalayout = \"{datalayout}\"\n"));
    out.push_str(&format!("target triple = \"{}\"\n\n", target.llvm_triple()));

    for func in &module.functions {
        let ret_ty = if target.arch() == Arch::Wasm32 {
            "i32"
        } else {
            "i64"
        };
        let params: Vec<String> = func
            .params
            .iter()
            .map(|p| format!("i64 %{}", p.id()))
            .collect();
        out.push_str(&format!(
            "define {} @{}({}) {{\n",
            ret_ty,
            func.name,
            params.join(", ")
        ));

        for block in &func.blocks {
            out.push_str(&format!("bb{}:\n", block.id));
            for instr in &block.instrs {
                match instr {
                    SsaInstr::Cast {
                        result,
                        src,
                        from,
                        to,
                    } => {
                        out.push_str(&format!(
                            "%{} = cast %{} : {:?} -> {:?}\n",
                            result.id(),
                            src.id(),
                            from,
                            to
                        ));
                    }
                    SsaInstr::Const { result, value } => {
                        let v = match value {
                            ssa::SsaConst::Int(i) => i.to_string(),
                            ssa::SsaConst::Bool(b) => (i64::from(*b)).to_string(),
                            ssa::SsaConst::Unit => "0".to_string(),
                            ssa::SsaConst::Float(bits, 32) => {
                                format!("0x{:08x}", *bits as u32)
                            }
                            ssa::SsaConst::Float(bits, _) => format!("0x{bits:016x}"),
                            ssa::SsaConst::Str(text) => {
                                format!("c\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
                            }
                        };
                        out.push_str(&format!("  %{} = add i64 0, {}\n", result.id(), v));
                    }
                    SsaInstr::Unary {
                        result,
                        op,
                        operand,
                    } => match op {
                        SsaUnaryOp::Neg => {
                            out.push_str(&format!(
                                "  %{} = sub i64 0, %{}\n",
                                result.id(),
                                operand.id()
                            ));
                        }
                        SsaUnaryOp::BitNot => {
                            out.push_str(&format!(
                                "  %{} = xor i64 %{}, -1\n",
                                result.id(),
                                operand.id()
                            ));
                        }
                        SsaUnaryOp::Not => {
                            out.push_str(&format!(
                                "  %{} = icmp eq i64 %{}, 0\n",
                                result.id(),
                                operand.id()
                            ));
                        }
                    },
                    SsaInstr::Binary {
                        result,
                        op,
                        lhs,
                        rhs,
                        ..
                    } => {
                        let op_str = match op {
                            SsaBinaryOp::Add => "add",
                            SsaBinaryOp::Sub => "sub",
                            SsaBinaryOp::Mul => "mul",
                            SsaBinaryOp::Div => "sdiv",
                            SsaBinaryOp::Rem => "srem",
                            SsaBinaryOp::BitAnd => "and",
                            SsaBinaryOp::BitOr => "or",
                            SsaBinaryOp::BitXor => "xor",
                            SsaBinaryOp::Shl => "shl",
                            SsaBinaryOp::Shr => "ashr",
                            SsaBinaryOp::Eq => "icmp eq",
                            SsaBinaryOp::Ne => "icmp ne",
                            SsaBinaryOp::Lt => "icmp slt",
                            SsaBinaryOp::Gt => "icmp sgt",
                            SsaBinaryOp::Le => "icmp sle",
                            SsaBinaryOp::Ge => "icmp sge",
                            SsaBinaryOp::And => "and",
                            SsaBinaryOp::Or => "or",
                        };
                        // 比较指令返回 i1，其余返回 i64；这里统一降级为 i64 文本。
                        out.push_str(&format!(
                            "  %{} = {} i64 %{}, %{}\n",
                            result.id(),
                            op_str,
                            lhs.id(),
                            rhs.id()
                        ));
                    }
                    ssa::SsaInstr::Call {
                        result,
                        callee,
                        args,
                        ..
                    } => {
                        let a: Vec<String> = args.iter().map(|v| format!("%{}", v.id())).collect();
                        out.push_str(&format!(
                            "  %{} = call {} {}({})\n",
                            result.id(),
                            callee,
                            "i64",
                            a.join(", ")
                        ));
                    }
                    ssa::SsaInstr::Phi {
                        result,
                        name,
                        incoming,
                    } => {
                        let ins: Vec<String> = incoming
                            .iter()
                            .map(|(p, v)| format!("[bb{p}: %{}]", v.id()))
                            .collect();
                        out.push_str(&format!(
                            "  %{} = phi i64 {} {}\n",
                            result.id(),
                            name,
                            ins.join(", ")
                        ));
                    }
                    SsaInstr::Copy { result, src } => {
                        out.push_str(&format!(
                            "  %{} = copy %{} ; {}\n",
                            result.id(),
                            src.id(),
                            src.id()
                        ));
                    }
                    ssa::SsaInstr::Load { result, ptr, ty } => {
                        out.push_str(&format!(
                            "  %{} = load {:?}, ptr %{}\n",
                            result.id(),
                            ty,
                            ptr.id()
                        ));
                    }
                    ssa::SsaInstr::Store { ptr, value, ty } => {
                        out.push_str(&format!(
                            "  store {:?} %{}, ptr %{}\n",
                            ty,
                            value.id(),
                            ptr.id()
                        ));
                    }
                    ssa::SsaInstr::SlotAddr { result, local, .. } => {
                        out.push_str(&format!("  %{} = alloca_slot {}\n", result.id(), local));
                    }
                    SsaInstr::Var { result, name } => {
                        out.push_str(&format!("  ; var {} -> %{}\n", name, result.id()));
                    }
                    SsaInstr::SimdBinary {
                        lhs,
                        op,
                        rhs,
                        out: out_addr,
                        ty,
                        lanes,
                    } => {
                        out.push_str(&format!(
                            "  simd {:?} {:?} x{} %{} %{} -> %{}\n",
                            ty,
                            op,
                            lanes,
                            lhs.id(),
                            rhs.id(),
                            out_addr.id()
                        ));
                    }
                    SsaInstr::FuncAddr { result, target } => {
                        out.push_str(&format!("  %{} = &@{}\n", result.id(), target));
                    }
                    SsaInstr::CallIndirect {
                        result,
                        callee,
                        args,
                        ..
                    } => {
                        let a: Vec<String> = args.iter().map(|v| format!("%{}", v.id())).collect();
                        out.push_str(&format!(
                            "  %{} = call_indirect %{}({})\n",
                            result.id(),
                            callee.id(),
                            a.join(", ")
                        ));
                    }
                    SsaInstr::Asm {
                        template,
                        outputs,
                        inputs,
                    } => {
                        let fmt_ops = |ops: &[(String, ssa::SsaValue, crate::mir::NumTy)]| {
                            ops.iter()
                                .map(|(c, v, t)| format!("{c}:{t:?} %{}", v.id()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        out.push_str(&format!(
                            "  ; asm!({template:?}) out[{}] in[{}]\n",
                            fmt_ops(outputs),
                            fmt_ops(inputs)
                        ));
                    }
                }
            }

            match &block.term {
                SsaTerminator::Goto(target) => {
                    out.push_str(&format!("  br label %bb{}\n", target));
                }
                SsaTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                } => {
                    out.push_str(&format!(
                        "  br i1 %{}, label %bb{}, label %bb{}\n",
                        cond.id(),
                        then_block,
                        else_block
                    ));
                }
                SsaTerminator::Return(Some(v)) => {
                    out.push_str(&format!("  ret i64 %{}\n", v.id()));
                }
                SsaTerminator::Return(None) => {
                    out.push_str("  ret void\n");
                }
                SsaTerminator::Unreachable => {
                    out.push_str("  unreachable\n");
                }
            }
        }

        out.push_str("}\n\n");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_no_args_shows_help() {
        let res = Cli::parse(&["zr".into()]);
        assert!(res.is_ok());
        assert!(res.unwrap().help);
    }

    #[test]
    fn cli_opt_levels() {
        assert_eq!(
            Cli::parse(&["zr".into(), "-O2".to_string(), "a.zr".into()])
                .unwrap()
                .opt_level,
            OptLevel::O2
        );
        assert_eq!(
            Cli::parse(&["zr".into(), "-Oz".to_string(), "a.zr".into()])
                .unwrap()
                .opt_level,
            OptLevel::Oz
        );
    }
}
