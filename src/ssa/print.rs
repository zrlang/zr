//! SSA 模块的可读文本表示（调试用）。

use super::{SsaBinaryOp, SsaModule, SsaUnaryOp};

/// 打印整个 SSA 模块。
pub fn print_module(module: &SsaModule) {
    for func in &module.functions {
        println!("fn {}(", func.name);
        println!(") {{");
        for block in &func.blocks {
            println!("  bb{}:", block.id);
            for instr in &block.instrs {
                print!("    ");
                match instr {
                    super::SsaInstr::Const { result, value } => {
                        println!("%{} = const {:?}", result.id(), value)
                    }
                    super::SsaInstr::Unary { result, op, operand } => {
                        println!("%{} = {:?} %{}", result.id(), op, operand.id())
                    }
                    super::SsaInstr::Binary { result, op, lhs, rhs } => {
                        println!(
                            "%{} = %{} {:?} %{}",
                            result.id(),
                            lhs.id(),
                            op,
                            rhs.id()
                        )
                    }
                    super::SsaInstr::Cast { result, operand, to } => {
                        println!("%{} = cast<{:?}>(%{})", result.id(), to, operand.id())
                    }
                    super::SsaInstr::Var { result, name } => {
                        println!("%{} = var {}", result.id(), name)
                    }
                }
            }
            print!("    ");
            match &block.term {
                super::SsaTerminator::Goto(t) => println!("goto bb{}", t),
                super::SsaTerminator::Branch { cond, then_block, else_block } => {
                    println!("if %{} then bb{} else bb{}", cond.id(), then_block, else_block)
                }
                super::SsaTerminator::Return(v) => match v {
                    Some(v) => println!("return %{}", v.id()),
                    None => println!("return"),
                },
                super::SsaTerminator::Unreachable => println!("unreachable"),
            }
        }
        println!("}}");
    }
}
