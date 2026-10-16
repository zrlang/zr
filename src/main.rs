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

mod stdlib;
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

use crate::codegen::target::{Arch, ObjectFormat, Os, Target};
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

    let input = match cmd.input.as_deref() {
        Some(p) => p,
        None => {
            print_usage(&args[0]);
            return ExitCode::SUCCESS;
        }
    };

    match compile(input, &cmd) {
        Ok(out_path) => {
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
                "--dump-ssa-after" => cli.dump_ssa_after = true,
                "--dump-asm" => cli.dump_asm = true,
                "--emit-llvm" => cli.emit_llvm = true,
                "--wat" => cli.wat = true,
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
        // 无输入文件时默认显示帮助
        if cli.input.is_none() {
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
fn compile(input: &str, cmd: &Cli) -> Result<std::path::PathBuf, String> {
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
    let source = std::fs::read_to_string(input)
        .map_err(|e| format!("cannot read '{input}': {e}"))?;

    // 2. 词法分析
    let lexer = lexer::Lexer::new(&source);
    let tokens = lexer.tokenize();
    if cmd.dump_tokens {
        println!("=== Tokens ({}) ===", tokens.len());
        for t in &tokens {
            println!("  {:?}", t);
        }
    }

    // 3. 语法分析
    let mut parser = parser::Parser::new(tokens);
    let mut ast = parser.parse().map_err(|e| e.to_string())?;
    if cmd.dump_ast {
        println!("=== AST ===");
        println!("{:#?}", ast);
    }

    // 3.5 加载标准库：把源文件 `use` 到的模块（及其传递依赖）并进来。
    let loaded = stdlib::load(&mut ast)?;

    // 4. 类型检查 + HIR
    let mut checker = typeck::TypeChecker::new();
    checker.set_known_modules(loaded.modules);
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

    // 7.6 目标选择：wasm32 走栈式字节码后端
    if target.format() == ObjectFormat::Wasm {
        let wasm = codegen::wasm::WasmModule::from_ssa(&ssa_module);
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
        codegen::emit_object(&target, &unit, &obj)
            .map_err(|e| format!("object writer: {e}"))?;
        Ok(obj)
    } else {
        let exe = output_with_extension(Path::new(&cmd.output), &target);
        codegen::emit_executable(&target, &unit, &exe).map_err(|e| {
            format!("linker: {e}\nnote: {}", codegen::runtime_diagnostic(&target))
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
        let ret_ty = if target.arch() == Arch::Wasm32 { "i32" } else { "i64" };
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
                    SsaInstr::Cast { result, src, from, to } => {
                        out.push_str(&format!("%{} = cast %{} : {:?} -> {:?}\n", result.id(), src.id(), from, to));
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
                    SsaInstr::Unary { result, op, operand } => {
                        match op {
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
                        }
                    }
                    SsaInstr::Binary { result, op, lhs, rhs } => {
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
                    ssa::SsaInstr::Call { result, callee, args } => {
                        let a: Vec<String> =
                            args.iter().map(|v| format!("%{}", v.id())).collect();
                        out.push_str(&format!(
                            "  %{} = call {} {}({})\n",
                            result.id(),
                            callee,
                            "i64",
                            a.join(", ")
                        ));
                    }
                    ssa::SsaInstr::Phi { result, name, incoming } => {
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
                        out.push_str(&format!("  %{} = copy %{} ; {}\n", result.id(), src.id(), src.id()));
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
                SsaTerminator::Branch { cond, then_block, else_block } => {
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
        assert_eq!(Cli::parse(&["zr".into(), "-O2".to_string(), "a.zr".into()]).unwrap().opt_level, OptLevel::O2);
        assert_eq!(Cli::parse(&["zr".into(), "-Oz".to_string(), "a.zr".into()]).unwrap().opt_level, OptLevel::Oz);
    }
}
