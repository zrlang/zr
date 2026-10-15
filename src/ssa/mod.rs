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
use crate::mir::{self, MirModule, MirTerminator, MirValue};
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
    /// φ 函数：`result = φ(p0: v0, p1: v1, ...)`。
    ///
    /// 放在有多个前驱的合流块开头，把各前驱末尾的值合并成一个新版本。
    /// 没有 φ 就没有真正的 SSA：循环回边和 `if` 合流处会一直读到旧的
    /// 定义（循环不终止、`if` 表达式取错值）。
    Phi {
        result: SsaValue,
        /// 被合流的变量名（调试用）。
        name: String,
        /// `(前驱块号, 该前驱传入的值)`。
        incoming: Vec<(usize, SsaValue)>,
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
            SsaInstr::Phi { result, .. } => Some(*result),
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

    let params: Vec<SsaValue> = (0..func.params.len()).map(|_| alloc()).collect();

    // ---- φ 插入计划（依赖 CFG，先于生成指令）----
    let preds = predecessors(&func.blocks);
    let plan = phi_plan(&func.blocks, &preds);
    let rpo = reverse_postorder(&func.blocks);

    // MIR 用字符串给变量/临时值命名。`cur` 是"当前可见的定义"，
    // 按 RPO 前进时单调更新；φ 结果在进入合流块时就写进去，
    // 这样块内的引用会指向 φ 而不是某一条路径上的旧定义。
    let mut cur: HashMap<String, SsaValue> = HashMap::new();
    for (i, p) in params.iter().enumerate() {
        cur.insert(format!("arg_{i}"), *p);
    }

    // 预先给计划里的每个 φ 分配结果值。必须早于遍历完成：处理 bb0 时
    // 就要能把"bb1 的 φ 在 bb0 这条路径上的 incoming"记下来，而那时还没
    // 走到 bb1、φ 还不存在。
    let mut phi_results: HashMap<usize, Vec<(String, SsaValue)>> = HashMap::new();
    for (b, names) in &plan {
        let mut made = Vec::new();
        for name in names.clone() {
            made.push((name, alloc()));
        }
        phi_results.insert(*b, made);
    }
    let mut out_blocks: Vec<SsaBasicBlock> = Vec::with_capacity(func.blocks.len());
    // `(合流块, φ 结果) -> [(前驱, 传入值)]`
    let mut phi_incoming: HashMap<(usize, SsaValue), Vec<(usize, SsaValue)>> = HashMap::new();
    // 先占位，保证 block.id 能对上
    for b in &func.blocks {
        out_blocks.push(SsaBasicBlock { id: b.id, instrs: Vec::new(), term: SsaTerminator::Unreachable });
    }

    for &bid in &rpo {
        let block = &func.blocks[bid];
        let mut instrs: Vec<SsaInstr> = Vec::new();

        // 进入块：把该块的 φ 结果绑定为当前值，让块内引用指向它们。
        // φ 指令本身**等遍历全部结束再插到块首**——回边（循环体的前驱）
        // 在 RPO 里排在本块之后，那时它的 incoming 还拿不到。
        if let Some(made) = phi_results.get(&bid).cloned() {
            for (name, v) in made {
                cur.insert(name, v);
            }
        }

        for instr in &block.instrs {
            match instr {
                mir::MirInstr::Assign(name, val) => {
                    let v = materialize(val, &mut instrs, &mut cur, &next_value);
                    cur.insert(name.clone(), v);
                }
                mir::MirInstr::Binary(name, a, op, b) => {
                    let l = materialize(a, &mut instrs, &mut cur, &next_value);
                    let r = materialize(b, &mut instrs, &mut cur, &next_value);
                    let v = alloc();
                    instrs.push(SsaInstr::Binary {
                        result: v,
                        op: convert_binary_op(*op),
                        lhs: l,
                        rhs: r,
                    });
                    cur.insert(name.clone(), v);
                }
                mir::MirInstr::Unary(name, op, x) => {
                    let o = materialize(x, &mut instrs, &mut cur, &next_value);
                    let v = alloc();
                    instrs.push(SsaInstr::Unary {
                        result: v,
                        op: convert_unary_op(*op),
                        operand: o,
                    });
                    cur.insert(name.clone(), v);
                }
                mir::MirInstr::Call(name, callee, args) => {
                    let mut ssa_args = Vec::with_capacity(args.len());
                    for a in args {
                        ssa_args.push(materialize(a, &mut instrs, &mut cur, &next_value));
                    }
                    let v = alloc();
                    instrs.push(SsaInstr::Call {
                        result: v,
                        callee: callee.clone(),
                        args: ssa_args,
                    });
                    cur.insert(name.clone(), v);
                }
                mir::MirInstr::Await(name, future) => {
                    let v = materialize(future, &mut instrs, &mut cur, &next_value);
                    cur.insert(name.clone(), v);
                }
            }
        }

        let term = match block.term.as_ref() {
            None => SsaTerminator::Unreachable,
            Some(mir::MirTerminator::Return(Some(val))) => {
                SsaTerminator::Return(Some(materialize(val, &mut instrs, &mut cur, &next_value)))
            }
            Some(mir::MirTerminator::Branch { cond, then_block, else_block }) => {
                let c = materialize(cond, &mut instrs, &mut cur, &next_value);
                SsaTerminator::Branch {
                    cond: c,
                    then_block: *then_block,
                    else_block: *else_block,
                }
            }
            Some(other) => convert_terminator(other, &next_value),
        };

        out_blocks[bid].instrs = instrs;
        out_blocks[bid].term = term;

        // 本块处理完，`cur` 就是"沿本条路径离开该块时"的定义环境。
        // 把它作为后继各个 φ 的 incoming 记下来——必须在遍历过程中记，
        // 事后再回填会丢掉分支之间的差别。
        for succ in successors_of_term(&block.term) {
            let Some(made) = phi_results.get(&succ).cloned() else {
                continue;
            };
            for (name, v) in made {
                let incoming = cur.get(&name).copied().unwrap_or(v);
                phi_incoming.entry((succ, v)).or_default().push((bid, incoming));
            }
        }
    }

    // 把 φ 插到各块开头
    for b in 0..out_blocks.len() {
        let Some(made) = phi_results.get(&b) else {
            continue;
        };
        let mut head = Vec::with_capacity(made.len());
        for (name, v) in made {
            let incoming = phi_incoming.get(&(b, *v)).cloned().unwrap_or_default();
            head.push(SsaInstr::Phi {
                result: *v,
                name: name.clone(),
                incoming,
            });
        }
        head.append(&mut out_blocks[b].instrs);
        out_blocks[b].instrs = head;
    }

    SsaFunction {
        name: func.name.clone(),
        params,
        blocks: out_blocks,
    }
}



// ==================================================================
// φ 插入与重命名
// ==================================================================

/// 从 MIR 的终结符推出每个块的前驱集合。
/// 终结符指向的后继块。
fn successors_of_term(term: &Option<mir::MirTerminator>) -> Vec<usize> {
    match term {
        Some(mir::MirTerminator::Goto(t)) => vec![*t],
        Some(mir::MirTerminator::Branch {
            then_block,
            else_block,
            ..
        }) => vec![*then_block, *else_block],
        _ => Vec::new(),
    }
}

fn predecessors(blocks: &[mir::MirBasicBlock]) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); blocks.len()];
    for b in blocks {
        for s in successors_of_term(&b.term) {
            if s < preds.len() {
                preds[s].push(b.id);
            }
        }
    }
    preds
}

/// 从入口做 DFS，得到逆后序（RPO）——SSA 重命名必须按 RPO 走，
/// 否则回边会先于它所更新的定义被处理。
fn reverse_postorder(blocks: &[mir::MirBasicBlock]) -> Vec<usize> {
    let n = blocks.len();
    let mut visited = vec![false; n];
    let mut post = Vec::with_capacity(n);
    // 迭代式 DFS，避免深图爆栈
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    if n == 0 {
        return post;
    }
    visited[0] = true;
    while let Some((node, child_idx)) = stack.pop() {
        let succs: Vec<usize> = match &blocks[node].term {
            Some(mir::MirTerminator::Goto(t)) => vec![*t],
            Some(mir::MirTerminator::Branch {
                then_block,
                else_block,
                ..
            }) => vec![*then_block, *else_block],
            _ => Vec::new(),
        };
        if child_idx < succs.len() {
            stack.push((node, child_idx + 1));
            let nxt = succs[child_idx];
            if nxt < n && !visited[nxt] {
                visited[nxt] = true;
                stack.push((nxt, 0));
            }
        } else {
            post.push(node);
        }
    }
    post.reverse();
    post
}

/// 某块内被赋值过的变量名（`Assign` / `Binary` / `Unary` / `Call` 的目标）。
fn defined_names(block: &mir::MirBasicBlock) -> Vec<String> {
    let mut out = Vec::new();
    for i in &block.instrs {
        let name = match i {
            mir::MirInstr::Assign(n, _) => Some(n),
            mir::MirInstr::Binary(n, ..) => Some(n),
            mir::MirInstr::Unary(n, ..) => Some(n),
            mir::MirInstr::Call(n, ..) => Some(n),
            mir::MirInstr::Await(n, _) => Some(n),
        };
        if let Some(n) = name {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
    }
    out
}

/// 决定每个合流块上要为哪些变量插 φ。
///
/// 判定规则：块 `B` 有 ≥2 个前驱，且某个变量名在 B 的**两个不同前驱**里
/// 都被定义过。循环回边（header 的前驱是入口和循环体）正好命中这条。
fn phi_plan(blocks: &[mir::MirBasicBlock], preds: &[Vec<usize>]) -> HashMap<usize, Vec<String>> {
    let mut plan: HashMap<usize, Vec<String>> = HashMap::new();
    for (b, ps) in preds.iter().enumerate() {
        if ps.len() < 2 {
            continue;
        }
        // 统计每个名字出现在几个不同前驱里
        let mut hits: HashMap<String, usize> = HashMap::new();
        for &p in ps {
            for name in defined_names(&blocks[p]) {
                *hits.entry(name).or_insert(0) += 1;
            }
        }
        let mut names: Vec<String> = hits
            .into_iter()
            .filter(|(_, c)| *c >= 2)
            .map(|(n, _)| n)
            .collect();
        names.sort();
        if !names.is_empty() {
            plan.insert(b, names);
        }
    }
    plan
}

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
fn convert_unary_op(op: mir::MirUnaryOp) -> SsaUnaryOp {
    use mir::MirUnaryOp::*;
    match op {
        Neg => SsaUnaryOp::Neg,
        Not => SsaUnaryOp::Not,
        BitNot => SsaUnaryOp::BitNot,
        Deref => SsaUnaryOp::Not, // 指针解引用在骨架阶段占位
    }
}
fn convert_terminator(term: &MirTerminator, _next_value: &Cell<u32>) -> SsaTerminator {
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
        // `Return` / `Branch` 在 convert_function 里已经带着环境解析过了，
        // 走到这里只可能是没有携带值的不可达或 unit 返回。
        MirTerminator::Return(_) => SsaTerminator::Return(None),
        MirTerminator::Unreachable => SsaTerminator::Unreachable,
    }
}

/// 把一个 MIR 操作数物化成 SSA 值，必要时发射一条定义指令。
///
/// 变量名先查当前作用域；查不到说明它来自块外（形参或前驱块），此时保留
/// 一条 `Var` 指令占位，由 φ 插入/重命名补上真正的定义。
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
