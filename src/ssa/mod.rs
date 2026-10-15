//! 静态单赋值形式（SSA）中间表示。
//!
//! 设计参考：
//! - Rust：MIR → LLVM IR 前的 SSA 化，用 `mir::VarDebugInfo` 关联原始变量。
//! - Zig：Air（Analysis IR）介于 AST 与机器码之间，保留调试与优化信息。
//! - Python：无 SSA 层（CPython 用栈式字节码）。
//!
//! 核心概念：
//! - 每个变量只被赋值一次（Single Static Assignment）。
//! - 在控制流汇合点插入 φ（phi）函数，把来自不同前驱的值合并。
//! - 支配树（dominator tree）是构造 φ 函数的理论基础。

pub mod print;

pub use print::print_module;

use std::collections::HashMap;
use crate::mir::{self, MirInstr, MirModule, MirTerminator, MirValue};
use std::cell::Cell;

/// 一个 SSA 模块，包含若干函数。
#[derive(Debug, Clone, Default)]
pub struct SsaModule {
    pub functions: Vec<SsaFunction>,
}

/// 一个 SSA 函数。
#[derive(Debug, Clone)]
pub struct SsaFunction {
    pub name: String,
    pub params: Vec<SsaValue>,
    pub blocks: Vec<SsaBasicBlock>,
}

/// 一个 SSA 基本块。
#[derive(Debug, Clone)]
pub struct SsaBasicBlock {
    pub id: usize,
    pub instrs: Vec<SsaInstr>,
    pub term: SsaTerminator,
}

/// SSA 指令。
#[derive(Debug, Clone)]
pub enum SsaInstr {
    /// 常量赋值 `v = c`。
    Const {
        result: SsaValue,
        value: SsaConst,
    },
    /// 一元运算 `v = op x`。
    Unary {
        result: SsaValue,
        op: SsaUnaryOp,
        operand: SsaValue,
    },
    /// 二元运算 `v = a op b`。
    Binary {
        result: SsaValue,
        op: SsaBinaryOp,
        lhs: SsaValue,
        rhs: SsaValue,
    },
    /// 函数调用：`result = callee(args...)`。
    ///
    /// 各后端按目标 ABI 负责把实参搬进调用约定寄存器。
    Call {
        result: SsaValue,
        callee: String,
        args: Vec<SsaValue>,
    },
    /// 变量引用（用于 φ 函数合并前的原始变量）。
    Var {
        result: SsaValue,
        name: String,
    },
}

/// SSA 终结器（基本块末尾的控制流指令）。
#[derive(Debug, Clone)]
pub enum SsaTerminator {
    /// 无条件跳转。
    Goto(usize),
    /// 条件分支。
    Branch {
        cond: SsaValue,
        then_block: usize,
        else_block: usize,
    },
    /// 函数返回。
    Return(Option<SsaValue>),
    /// 不可达（发散调用）。
    Unreachable,
}

/// SSA 值（一个版本化的变量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SsaValue(pub u32);

impl SsaValue {
    /// 返回底层 ID。
    pub fn id(self) -> u32 {
        self.0
    }
}

/// SSA 常量。
#[derive(Debug, Clone, PartialEq)]
pub enum SsaConst {
    Int(i128),
    /// 布尔常量。
    Bool(bool),
    /// 浮点常量（按位存储）。
    Float(u64),
    /// 单元值。
    Unit,
    /// 字符串字面量。
    Str(String),
}

/// 一元运算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsaUnaryOp {
    /// 逻辑非。
    Not,
    /// 位取反。
    BitNot,
    /// 取负。
    Neg,
}

/// 二元运算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsaBinaryOp {
    Add, Sub, Mul, Div, Rem,
    BitAnd, BitOr, BitXor, Shl, Shr,
    Eq, Ne, Lt, Gt, Le, Ge,
    And, Or,
}

impl SsaInstr {
    /// 返回此指令定义的值（若有）。
    pub fn result(&self) -> Option<SsaValue> {
        match self {
            SsaInstr::Const { result, .. } => Some(*result),
            SsaInstr::Unary { result, .. } => Some(*result),
            SsaInstr::Binary { result, .. } => Some(*result),
            SsaInstr::Call { result, .. } => Some(*result),
            SsaInstr::Var { result, .. } => Some(*result),
        }
    }
}

/// 从 MIR 模块构造 SSA 形式。
///
/// 当前为「朴素 SSA」降级：把 MIR 基本块与指令线性映射到 SSA，
/// 每个赋值产生一个新版本的值。φ 函数的插入需要支配树分析，
/// 见 `domtree` 模块的 `build_dominator_tree`。
pub fn from_mir(mir: &MirModule) -> SsaModule {
    let mut functions = Vec::with_capacity(mir.functions.len());
    for func in &mir.functions {
        functions.push(convert_function(func));
    }
    SsaModule { functions }
}

/// 把单个 MIR 函数转为 SSA。
fn convert_function(func: &mir::MirFunction) -> SsaFunction {
    let next_value: Cell<u32> = Cell::new(0);
    let alloc = || {
        let v = SsaValue(next_value.get());
        next_value.set(next_value.get() + 1);
        v
    };

    let params: Vec<SsaValue> = func
        .params
        .iter()
        .map(|_| alloc())
        .collect();

    let mut blocks = Vec::with_capacity(func.blocks.len());
    // MIR 用字符串给变量/临时值命名，这里维护"名字 → SSA 值"的环境。
    // 之前每处引用都现�� `alloc()` 一个新值、并且把赋值的目标值直接丢掉，
    // 导致 `let x = 42; x + 1` 里的 x 是个没有定义指令的野值。
    let mut env: HashMap<String, SsaValue> = HashMap::new();
    for (i, p) in params.iter().enumerate() {
        env.insert(format!("arg_{i}"), *p);
    }

    for block in &func.blocks {
        let mut instrs = Vec::new();
        for instr in &block.instrs {
            match instr {
                MirInstr::Assign(name, val) => {
                    let v = materialize(val, &mut instrs, &mut env, &next_value);
                    env.insert(name.clone(), v);
                }
                MirInstr::Binary(name, a, op, b) => {
                    let l = materialize(a, &mut instrs, &mut env, &next_value);
                    let r = materialize(b, &mut instrs, &mut env, &next_value);
                    let v = alloc();
                    instrs.push(SsaInstr::Binary {
                        result: v,
                        op: convert_binary_op(*op),
                        lhs: l,
                        rhs: r,
                    });
                    env.insert(name.clone(), v);
                }
                MirInstr::Unary(name, op, x) => {
                    let o = materialize(x, &mut instrs, &mut env, &next_value);
                    let v = alloc();
                    instrs.push(SsaInstr::Unary {
                        result: v,
                        op: convert_unary_op(*op),
                        operand: o,
                    });
                    env.insert(name.clone(), v);
                }
                MirInstr::Await(name, future) => {
                    let v = materialize(future, &mut instrs, &mut env, &next_value);
                    env.insert(name.clone(), v);
                }
                MirInstr::Call(name, callee, args) => {
                    let mut ssa_args = Vec::with_capacity(args.len());
                    for a in args {
                        ssa_args.push(materialize(a, &mut instrs, &mut env, &next_value));
                    }
                    let v = alloc();
                    instrs.push(SsaInstr::Call {
                        result: v,
                        callee: callee.clone(),
                        args: ssa_args,
                    });
                    env.insert(name.clone(), v);
                }
            }
        }

        // 返回值可能是一条尚未物化的常量：先补 Const 指令，再终结。
        let term = match block.term.as_ref() {
            // 未终结的块：MIR 降级阶段保证函数体收尾时补上终结符，
            // 这里按不可达处理，避免生成悬空的基本块。
            None => SsaTerminator::Unreachable,
            Some(mir::MirTerminator::Return(Some(val))) => {
                SsaTerminator::Return(Some(materialize(
                    val,
                    &mut instrs,
                    &mut env,
                    &next_value,
                )))
            }
            // 条件分支：条件值要从环境里解析出来（不能像以前那样硬编码
            // 成 SsaValue(0)，否则 `if` 判断的永远是第一个值）。
            Some(mir::MirTerminator::Branch { cond, then_block, else_block }) => {
                let c = materialize(cond, &mut instrs, &mut env, &next_value);
                SsaTerminator::Branch { cond: c, then_block: *then_block, else_block: *else_block }
            }
            Some(other) => convert_terminator(other, &next_value),
        };
        blocks.push(SsaBasicBlock {
            id: block.id,
            instrs,
            term,
        });
    }

    SsaFunction {
        name: func.name.clone(),
        params,
        blocks,
    }
}

/// 把 MIR 二元运算转为 SSA 二元运算。
fn convert_binary_op(op: mir::MirBinaryOp) -> SsaBinaryOp {
    use mir::MirBinaryOp::*;
    match op {
        Add => SsaBinaryOp::Add,
        Sub => SsaBinaryOp::Sub,
        Mul => SsaBinaryOp::Mul,
        Div => SsaBinaryOp::Div,
        Rem => SsaBinaryOp::Rem,
        BitAnd => SsaBinaryOp::BitAnd,
        BitOr => SsaBinaryOp::BitOr,
        BitXor => SsaBinaryOp::BitXor,
        Shl => SsaBinaryOp::Shl,
        Shr => SsaBinaryOp::Shr,
        Eq => SsaBinaryOp::Eq,
        Ne => SsaBinaryOp::Ne,
        Lt => SsaBinaryOp::Lt,
        Gt => SsaBinaryOp::Gt,
        Le => SsaBinaryOp::Le,
        Ge => SsaBinaryOp::Ge,
        And => SsaBinaryOp::And,
        Or => SsaBinaryOp::Or,
    }
}

/// 把 MIR 一元运算转为 SSA 一元运算。
fn convert_unary_op(op: mir::MirUnaryOp) -> SsaUnaryOp {
    use mir::MirUnaryOp::*;
    match op {
        Neg => SsaUnaryOp::Neg,
        Not => SsaUnaryOp::Not,
        BitNot => SsaUnaryOp::BitNot,
        Deref => SsaUnaryOp::Not, // 指针解引用在骨架阶段占位
    }
}

/// 把 MIR 值转为 SSA 值（含常量物化）。
fn value_to_ssa(val: &MirValue, next: &Cell<u32>) -> SsaValue {
    let alloc = || {
        let v = SsaValue(next.get());
        next.set(next.get() + 1);
        v
    };
    match val {
        MirValue::Var(name) => {
            // 变量名 -> 简单哈希为 u32，真实实现需维护版本映射表
            let h = hash_name(name);
            SsaValue(h)
        }
        MirValue::Int(i) => {
            let v = alloc();
            let _ = i;
            v
        }
        MirValue::Bool(b) => {
            let v = alloc();
            let _ = b;
            v
        }
        MirValue::Unit => alloc(),
        MirValue::Float(_) | MirValue::Str(_) => alloc(),
    }
}

/// 把 MIR 终结器转为 SSA 终结器。
fn convert_terminator(term: &MirTerminator, next_value: &Cell<u32>) -> SsaTerminator {
    let alloc = || {
        let v = SsaValue(next_value.get());
        next_value.set(next_value.get() + 1);
        v
    };
    let _ = alloc;
    match term {
        MirTerminator::Goto(target) => SsaTerminator::Goto(*target),
        MirTerminator::Branch { cond, then_block, else_block } => SsaTerminator::Branch {
            cond: SsaValue(match cond {
                MirValue::Int(i) => *i as u32,
                _ => 0,
            }),
            then_block: *then_block,
            else_block: *else_block,
        },
        // 整数字面量由调用方补一条 Const 指令后直接引用；
        // 其余值按变量名映射到稳定的 SSA 值。
        MirTerminator::Return(value) => SsaTerminator::Return(
            value
                .as_ref()
                .filter(|v| !matches!(v, mir::MirValue::Unit))
                .map(|v| value_to_ssa(v, next_value)),
        ),
        MirTerminator::Unreachable => SsaTerminator::Unreachable,
    }
}

/// 对变量名做确定性哈希，把字符串映射到 u32。
fn hash_name(name: &str) -> u32 {
    let mut h: u32 = 0x9e3779b9;
    for b in name.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u32);
    }
    // 保留高 8 位为 0，避免与字面量分配的值碰撞
    h & 0x00ff_ffff
}

/// 把一个 MIR 操作数物化成 SSA 值，必要时发射一条定义指令。
///
/// 变量名先查环境；查不到说明它来自块外（形参或前驱块），此时保留一条
/// `Var` 指令占位，由 SSA 重命名阶段补上 φ 节点。
fn materialize(
    val: &MirValue,
    instrs: &mut Vec<SsaInstr>,
    env: &mut HashMap<String, SsaValue>,
    next: &Cell<u32>,
) -> SsaValue {
    let v = SsaValue(next.get());
    next.set(next.get() + 1);
    match val {
        MirValue::Var(name) => {
            if let Some(&existing) = env.get(name) {
                return existing;
            }
            instrs.push(SsaInstr::Var { result: v, name: name.clone() });
            env.insert(name.clone(), v);
        }
        MirValue::Int(i) => instrs.push(SsaInstr::Const {
            result: v,
            value: SsaConst::Int(*i),
        }),
        MirValue::Bool(b) => instrs.push(SsaInstr::Const {
            result: v,
            value: SsaConst::Bool(*b),
        }),
        MirValue::Float(bits) => instrs.push(SsaInstr::Const {
            result: v,
            value: SsaConst::Float(*bits),
        }),
        MirValue::Str(s) => instrs.push(SsaInstr::Const {
            result: v,
            value: SsaConst::Str(s.clone()),
        }),
        MirValue::Unit => instrs.push(SsaInstr::Const {
            result: v,
            value: SsaConst::Unit,
        }),
    }
    v
}
