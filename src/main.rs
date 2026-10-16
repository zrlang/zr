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
mod mir;
mod modules;
mod opt;
mod parser;
mod pkg;
mod ssa;
mod typeck;

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
        // 无输入文件时默认显示帮助——但 `clean` / `fmt --check` 这类
        // 子命令本来就不需要输入文件，不能被这个默认值吃掉。
        if cli.input.is_none() && !cli.clean_cache && !(cli.fmt && cli.fmt_check) {
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
    println!("    remove <name>           Remove a dependency");
    println!("    list                    List dependencies");
    println!("    tree                    Print the dependency graph");
    println!("    build                   Build all dependencies in order");
    println!("    clean                   Remove build artifacts");
}

fn print_usage(prog: &str) {
    println!("zr {} — zr language compiler", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("    {prog} [OPTIONS] <INPUT.zr>");
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

// 待写入的缓存条目：`compile()` 成功后才真正落盘。
thread_local! {
    static CACHE_FOR_STORE: std::cell::RefCell<Option<(u64, std::path::PathBuf, Vec<(std::path::PathBuf, u64)>)>> =
        const { std::cell::RefCell::new(None) };
}

/// 指纹盐：编译器版本 + 目标 + 优化等级 + 调试信息 + 是否只编译不链接
/// + 源文件路径。
///
/// 任何一个变了都必须重编——否则 `--target` 换一下就会拿到上一个目标的产物，
/// 那比不缓存糟糕得多。
fn build_salt(cmd: &Cli, target: &Target, input: &str) -> String {
    format!(
        "v{}|{}|{:?}|{}|{}|{}",
        env!("CARGO_PKG_VERSION"),
        target.triple(),
        cmd.opt_level,
        cmd.debug_info,
        if cmd.no_link { "c" } else { "link" },
        input
    )
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
    let mut checker = typeck::TypeChecker::new();
    checker.set_known_modules(loaded.modules.iter().cloned().collect());
    // 32 位目标暂时没有多字长整数支持：显式写 i64 会在类型检查阶段报错，
    // 而不是默默编出错的结果。
    checker.set_max_int_bits(u16::from(target.arch().pointer_size()) * 8);
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
    let unit = codegen::compile(&target, &ssa_module)?;

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
                            ssa::SsaConst::Float(bits) => format!("0x{bits:016x}"),
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
                    SsaInstr::Var { result, name } => {
                        out.push_str(&format!("  ; var {} -> %{}\n", name, result.id()));
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
