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
#![cfg_attr(test, allow(dead_code))]

mod codegen;
mod error;
mod lexer;
mod mir;
mod opt;
mod parser;
mod ssa;
mod typeck;

use std::env;
use std::path::Path;
use std::process::ExitCode;

use crate::codegen::CompilationUnit;
use crate::opt::OptLevel;

fn main() -> ExitCode {
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
        println!("zr {}", env!("CARGO_PKG_VERSION"));
        println!("host: x86_64-unknown-linux-gnu");
        return ExitCode::SUCCESS;
    }

    if cmd.help {
        print_usage(&args[0]);
        return ExitCode::SUCCESS;
    }

    let input = match cmd.input.as_deref() {
        Some(p) => p,
        None => {
            print_usage(&args[0]);
            return ExitCode::SUCCESS;
        }
    };

    match compile(input, &cmd) {
        Ok(()) => {
            eprintln!(
                "zr: compiled {} -> {} ({})",
                input,
                cmd.output.display(),
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
    #[allow(dead_code)]
    dump_asm: bool,
    #[allow(dead_code)]
    emit_llvm: bool,
    #[allow(dead_code)]
    no_link: bool,
    #[allow(dead_code)]
    target: String,
    #[allow(dead_code)]
    sysroot: Option<String>,
    help: bool,
    version: bool,
}

impl Cli {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut cli = Cli::default();
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
                "--dump-asm" => cli.dump_asm = true,
                "--emit-llvm" => cli.emit_llvm = true,
                "-c" => cli.no_link = true,
                "--target" => {
                    i += 1;
                    cli.target = args
                        .get(i)
                        .ok_or_else(|| "missing argument to '--target'".to_string())?
                        .clone();
                }
                "--sysroot" => {
                    i += 1;
                    cli.sysroot = Some(
                        args.get(i)
                            .ok_or_else(|| "missing argument to '--sysroot'".to_string())?
                            .clone(),
                    );
                }
                "-O0" => cli.opt_level = OptLevel::O0,
                "-O1" => cli.opt_level = OptLevel::O1,
                "-O2" => cli.opt_level = OptLevel::O2,
                "-O3" => cli.opt_level = OptLevel::O3,
                "-Oz" => cli.opt_level = OptLevel::Oz,
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
        Ok(cli)
    }
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
    println!("    --sysroot <dir>     Sysroot directory");
    println!("    --dump-tokens      Dump lexer output");
    println!("    --dump-ast          Dump AST");
    println!("    --dump-hir          Dump HIR");
    println!("    --dump-mir          Dump MIR");
    println!("    --dump-ssa          Dump SSA");
    println!("    --dump-asm          Dump assembly");
    println!("    --emit-llvm         Emit LLVM IR (future)");
    println!("    -h, --help          Show this help");
    println!("    -V, --version       Print version");
}

/// 编译单个文件。
fn compile(input: &str, cmd: &Cli) -> Result<(), String> {
    // 1. 读取源文件
    let source = std::fs::read_to_string(input)
        .map_err(|e| format!("cannot read '{input}': {e}"))?;

    // 2. 词法分析
    let mut lexer = lexer::Lexer::new(&source);
    let tokens = lexer.tokenize();
    if cmd.dump_tokens {
        println!("=== Tokens ({}) ===", tokens.len());
        for t in &tokens {
            println!("  {:?}", t);
        }
    }

    // 3. 语法分析
    let mut parser = parser::Parser::new(tokens);
    let ast = parser.parse().map_err(|e| e.to_string())?;
    if cmd.dump_ast {
        println!("=== AST ===");
        println!("{:#?}", ast);
    }

    // 4. 类型检查 + HIR
    let mut checker = typeck::TypeChecker::new();
    let hir = checker.check_program(&ast).map_err(|e| e.to_string())?;
    if cmd.dump_hir {
        println!("=== HIR ===");
        println!("{:#?}", hir);
    }

    // 5. MIR 降级
    let mir_module = mir::lower(&hir);
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

    // 8. 代码生成
    let unit: CompilationUnit = codegen::compile(&ssa_module);

    // 9. 输出目标文件 / 可执行文件
    if cmd.no_link {
        write_obj(&unit, &cmd.output)?;
    } else {
        codegen::emit_elf(&unit, Path::new(&cmd.output))
            .map_err(|e| format!("linker: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cmd.output, std::fs::Permissions::from_mode(0o755))
                .ok();
        }
    }

    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn write_obj(_unit: &CompilationUnit, output: &Path) -> Result<(), String> {
    // 目标文件格式（ELF relocatable）尚未实现，暂回退为写入裸机器码。
    Err(format!(
        "incremental compilation not implemented yet (-c to {}); \
         falling back to full link",
        output.display()
    ))
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
        assert_eq!(Cli::parse(&["zr".into(), "-O2", "a.zr".into()]).unwrap().opt_level, OptLevel::O2);
        assert_eq!(Cli::parse(&["zr".into(), "-Oz", "a.zr".into()]).unwrap().opt_level, OptLevel::Oz);
    }
}
