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

use crate::mir::{self, MirModule, MirTerminator, MirValue};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

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
    /// `#[inline(never)]`：内联 pass 必须跳过它。
    pub inline_never: bool,
    /// `#[inline(always)]`：内联 pass 必须内联它（即使多块）。
    pub inline_always: bool,
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
    Const { result: SsaValue, value: SsaConst },
    /// 一元运算 `v = op x`。
    Unary {
        result: SsaValue,
        op: SsaUnaryOp,
        operand: SsaValue,
    },
    /// 二元运算 `v = a op b`。
    ///
    /// `ty` 是运算的结果类型，决定后端选哪条指令：`add` vs `addsd`、
    /// `idiv` vs `divsd`、整数比较的 `setcc` vs 浮点比较的 `ucomisd`。
    /// 没有它就无法区分整数与浮点运算——浮点会按位模式当整数算，
    /// 而且照样"能跑"，只是结果是错的。
    Binary {
        result: SsaValue,
        op: SsaBinaryOp,
        lhs: SsaValue,
        rhs: SsaValue,
        ty: mir::NumTy,
    },
    /// 函数调用：`result = callee(args...)`。
    ///
    /// 各后端按目标 ABI 负责把实参搬进调用约定寄存器。`ty` 是返回值类型：
    /// 后端要靠它区分 `rax`（整数）与 `xmm0`（浮点），以及给返回值分配
    /// 浮点栈槽。IR 的浮点常量不带位宽，所以这个类型没法事后反推。
    Call {
        result: SsaValue,
        callee: String,
        args: Vec<SsaValue>,
        ty: mir::NumTy,
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
    /// 寄存器间复制：`result = src`。
    ///
    /// 优化 pass 用它表达 `x + 0 -> x`、`CSE/GVN 复用已有值` 这类
    /// 替换。**不能**用 `Var` 代替：`Var` 只是名字占位，后端不发射任何
    /// 指令，目标寄存器会保持未定义。
    Copy { result: SsaValue, src: SsaValue },
    /// 类型转换 `result = cast src`。
    Cast {
        result: SsaValue,
        src: SsaValue,
        from: mir::NumTy,
        to: mir::NumTy,
    },
    /// 从地址加载：`result = *ptr`。`ty` 决定读取位宽与有无符号。
    Load {
        result: SsaValue,
        ptr: SsaValue,
        ty: mir::NumTy,
    },
    /// 存储：`*ptr = value`。**没有结果值，但有副作用**：不能被 DCE 消除，
    /// 也不能与后续的 Load 交换顺序。
    Store {
        ptr: SsaValue,
        value: SsaValue,
        ty: mir::NumTy,
    },
    /// 取局部变量栈槽地址：`result = &slot{local}`。
    SlotAddr { result: SsaValue, local: u32 },
    /// 内联汇编：`asm!("template", in(reg) x, out(reg) y)`。
    ///
    /// 这是一个语句（无结果值），但有副作用，不可被消除或重排。
    Asm {
        /// 模板字符串，如 `"add {0}, {1}"`。
        template: String,
        /// 输出操作数：`(约束, 目标地址, 值类型)`。
        outputs: Vec<(String, SsaValue, mir::NumTy)>,
        /// 输入操作数：`(约束, 值, 值类型)`。
        inputs: Vec<(String, SsaValue, mir::NumTy)>,
    },
    /// 变量引用（用于 φ 函数合并前的原始变量）。
    Var { result: SsaValue, name: String },
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SsaConst {
    Int(i128),
    /// 布尔常量。
    Bool(bool),
    /// 浮点常量（按位存储 + 位宽）。
    ///
    /// 位宽（32 / 64）随值携带，后端据此选 `ss` 还是 `sd` 指令；常量自己没有
    /// 类型标注，丢了位宽就只有按 f64 编码这一种可能——f32 会读到错的位。
    Float(u64, u8),
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
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

impl SsaInstr {
    /// 返回此指令定义的值（若有）。
    pub fn result(&self) -> Option<SsaValue> {
        match self {
            SsaInstr::Const { result, .. } => Some(*result),
            SsaInstr::Unary { result, .. } => Some(*result),
            SsaInstr::Binary { result, .. } => Some(*result),
            SsaInstr::Phi { result, .. } => Some(*result),
            SsaInstr::Copy { result, .. } => Some(*result),
            SsaInstr::Cast { result, .. } => Some(*result),
            SsaInstr::Call { result, .. } => Some(*result),
            SsaInstr::Load { result, .. } => Some(*result),
            SsaInstr::SlotAddr { result, .. } => Some(*result),
            SsaInstr::Var { result, .. } => Some(*result),
            // 存储没有结果值（但仍有副作用）。
            SsaInstr::Store { .. } => None,
            SsaInstr::Asm { .. } => None,
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
        // 关键边拆分在 φ 插入之后做：只有这时才知道哪些块带 φ。
        functions.push(split_critical_edges(&convert_function(func)));
    }
    SsaModule { functions }
}

/// 把单个 MIR 函数转为 SSA。
///
/// φ 的插入用**迭代支配边界**（Braun et al., "Simple and Efficient
/// Construction of Static Single Assignment Form"），而不是"块里定义过就插"
/// 的启发式。
///
/// 启发式漏掉的形状非常常见：`while` + `break`。
///
/// ```text
/// bb0: k=0 ─► bb1: 头 ─► bb2: 体 (k+=1) ─┬─► bb4 ─┐
///             ▲                          │       ├─► bb3: 出口
///             └──────── bb6 ◄── bb5 ◄────┘       │
/// ```
///
/// 出口 bb3 有**两个**前驱：循环头 bb1（条件为假）与 break 落点 bb4。而循环体
/// 刚算出的新值只经 bb4 到达 bb3——循环头的 φ 只在回边 bb6 上被更新，break
/// 路径绕过它，于是 break 之后读到的是**上一轮**的值：
///
/// ```text
/// var mut k = 0;
/// while k < 10 { k += 1; if k == 5 { break; } }
/// // k == 4，不是 5
/// ```
///
/// 支配边界能抓到它：把「循环内有定义」的变量，其**循环头**也算进 defsite，
/// 于是 `DF` 同时命中 bb1（回边那条）与 bb3（break 那条）；取迭代闭包就把出口
/// 的 φ 补上了。缺了这一步，`break` 之后的每一次读取都差一轮。
fn convert_function(func: &mir::MirFunction) -> SsaFunction {
    let next_value: Cell<u32> = Cell::new(0);
    let alloc = || {
        let v = SsaValue(next_value.get());
        next_value.set(next_value.get() + 1);
        v
    };

    let params: Vec<SsaValue> = (0..func.params.len()).map(|_| alloc()).collect();
    let n = func.blocks.len();
    if n == 0 {
        return SsaFunction {
            name: func.name.clone(),
            params,
            blocks: Vec::new(),
            inline_never: func.inline_never,
            inline_always: func.inline_always,
        };
    }

    // ---- CFG ----
    let succs: Vec<Vec<usize>> = func
        .blocks
        .iter()
        .map(|b| successors_of_block(&b.term))
        .collect();
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ss) in succs.iter().enumerate() {
        for &s in ss {
            if s < n {
                preds[s].push(i);
            }
        }
    }

    // ---- 每个变量的定义块 ----
    let mut defsites: HashMap<String, HashSet<usize>> = HashMap::new();
    for (bi, block) in func.blocks.iter().enumerate() {
        for name in defined_names(block) {
            defsites.entry(name).or_default().insert(bi);
        }
    }
    for i in 0..params.len() {
        defsites.entry(format!("arg_{i}")).or_default().insert(0);
    }

    // ---- 支配树与支配边界 ----
    let idom = mir_dominator_tree(n, &preds, &succs);
    // DF(x) = { y | y 有 >=2 个前驱，且 x ∈ preds(y) }
    let mut df: HashMap<usize, Vec<usize>> = HashMap::new();
    for y in 0..n {
        if preds[y].len() < 2 {
            continue;
        }
        for &x in &preds[y] {
            df.entry(x).or_default().push(y);
        }
    }

    // ---- 循环体成员表 ----
    //
    // 每条回边（latch -> header 且 header 支配 latch）确定一个自然循环。
    let mut loops: Vec<(usize, HashSet<usize>)> = Vec::new();
    for latch in 0..n {
        for &h in &succs[latch] {
            if h >= n || !mir_dominates(&idom, h, latch) {
                continue;
            }
            loops.push((h, natural_loop(&preds, h, latch)));
        }
    }
    // 变量在循环内有定义 => 循环头也进 defsite。回边让循环头成为合流点；
    // 没有这一步 DF 命中不了循环头（回边的源块不是头的**直接**前驱，中间隔着
    // latch 块），循环头的 φ 就会整个漏掉。
    for sites in defsites.values_mut() {
        for (header, members) in &loops {
            if sites.iter().any(|b| members.contains(b)) {
                sites.insert(*header);
            }
        }
    }

    // ---- 活跃性：只给"用得到的变量"插 φ ----
    let live_in = mir_liveness(&func.blocks, &succs);

    // ---- φ 位置：IDF(defsites) ∩ live_in ----
    //
    // `defsites` 是 HashMap，必须按**变量名排序**后遍历：遍历顺序决定了
    // `phi_names` 里各块 φ 的插入顺序，进而决定 φ 结果值号的分配顺序。
    // 顺序不定 = 同一个程序每次编译出来的值编号不同，机器码也不同——
    // 同一个缺陷有时复现有时不���现（实测同一份源码在 -O0 挂、-O2 过）。
    let mut defsite_names: Vec<&String> = defsites.keys().collect();
    defsite_names.sort();
    let mut phi_names: HashMap<usize, Vec<String>> = HashMap::new();
    for name in defsite_names {
        let sites = &defsites[name];
        let mut work: Vec<usize> = sites.iter().copied().collect();
        let mut seen: HashSet<usize> = HashSet::new();
        let mut placed: Vec<usize> = Vec::new();
        while let Some(x) = work.pop() {
            let Some(frontier) = df.get(&x) else { continue };
            for &y in frontier {
                if !seen.insert(y) {
                    continue;
                }
                // y 必须是这个变量活跃的地方，否则插出来的 φ 没人读。
                if live_in[y].contains(name) {
                    placed.push(y);
                }
                work.push(y);
            }
        }
        if placed.is_empty() {
            continue;
        }
        placed.sort_unstable();
        for y in placed {
            phi_names.entry(y).or_default().push(name.clone());
        }
    }
    for names in phi_names.values_mut() {
        names.sort();
        names.dedup();
    }

    // ---- 预先给每个 φ 分配结果值 ----
    //
    // 必须早于遍历完成：处理 bb0 时就要能把"bb1 的 φ 在 bb0 这条路径上的
    // incoming"记下来，而那时还没走到 bb1、φ 还不存在。
    //
    // 按**块号升序**分配：块顺序决定值号顺序，随意遍历会让同一份源码每次
    // 编出不同的机器码（见上面 `defsites` 那段注释）。
    let mut phi_blocks: Vec<usize> = phi_names.keys().copied().collect();
    phi_blocks.sort_unstable();
    let mut phi_results: HashMap<usize, Vec<(String, SsaValue)>> = HashMap::new();
    for b in phi_blocks {
        let names = &phi_names[&b];
        let made: Vec<(String, SsaValue)> = names.iter().map(|n| (n.clone(), alloc())).collect();
        phi_results.insert(b, made);
    }

    // ---- 重命名 ----
    //
    // `stacks` 是"名字 -> 该名字所有版本的栈"。遇到定义 push，离开块时 pop。
    // 进入块时先压入该块的 φ 结果，块内的引用才会指向 φ 而不是某条路径上的
    // 旧定义。
    let mut stacks: HashMap<String, Vec<SsaValue>> = HashMap::new();
    for (i, p) in params.iter().enumerate() {
        stacks.insert(format!("arg_{i}"), vec![*p]);
    }

    // 遍历必须是**深度优先**的，不能用逆后序。
    //
    // 重命名靠"名字 -> 版本栈"：进入一个块时压入它的 φ 与定义，离开时弹回。
    // 这样某个块被处理时，它**支配者**建立的绑定仍在栈上——合流块读到的正是
    // 支配块的值。用逆后序就全乱了：RPO 里 bb3 排在 bb1 之后，可 bb1 早就
    // 处理完并弹了栈，bb3 再读 `v1` 就查不到，只能生成一条 `var v1` 占位。
    // 后端对 `Var` 不发射任何指令，目标寄存器保持未定义——循环读到的于是
    // 上一轮的残留值（`while` + `break` 会算错，嵌套循环直接不收敛）。
    let mut out_blocks: Vec<SsaBasicBlock> = (0..n)
        .map(|id| SsaBasicBlock {
            id,
            instrs: Vec::new(),
            term: SsaTerminator::Unreachable,
        })
        .collect();
    let mut phi_incoming: HashMap<(usize, SsaValue), Vec<(usize, SsaValue)>> = HashMap::new();

    enum Step {
        Enter(usize),
        Exit(Vec<String>),
    }
    let mut stack: Vec<Step> = vec![Step::Enter(0)];
    let mut visited: HashSet<usize> = HashSet::new();

    while let Some(step) = stack.pop() {
        let bid = match step {
            Step::Exit(names) => {
                // 离开块：弹回本块压入的绑定。
                for name in names.into_iter().rev() {
                    if let Some(st) = stacks.get_mut(&name) {
                        st.pop();
                    }
                }
                continue;
            }
            Step::Enter(b) => b,
        };
        if bid >= n || !visited.insert(bid) {
            continue;
        }

        // 进入块：压入该块的 φ 结果，块内的引用才会指向 φ 而不是某条路径上的
        // 旧定义。
        let mut pushed: Vec<String> = Vec::new();
        if let Some(made) = phi_results.get(&bid) {
            for (name, v) in made {
                stacks.entry(name.clone()).or_default().push(*v);
                pushed.push(name.clone());
            }
        }

        let mut instrs: Vec<SsaInstr> = Vec::new();
        let block = &func.blocks[bid];
        for instr in &block.instrs {
            // 每条指令算出一个新版本，并压进它所定义的名字的栈。
            match instr {
                mir::MirInstr::Assign(name, val) => {
                    let v = materialize(val, &mut instrs, &mut stacks, &next_value);
                    // `x = e` 用一条显式 `Copy` 表达：后端据此发射一条
                    // 寄存器间搬运，而不是让 x 与 e 共用同一个值号（那样优化
                    // pass 改写 x 的定义时会顺带改掉 e）。
                    let r = alloc();
                    instrs.push(SsaInstr::Copy { result: r, src: v });
                    stacks.entry(name.clone()).or_default().push(r);
                    pushed.push(name.clone());
                }
                mir::MirInstr::Cast(name, val, from, to) => {
                    let s = materialize(val, &mut instrs, &mut stacks, &next_value);
                    let r = alloc();
                    instrs.push(SsaInstr::Cast {
                        result: r,
                        src: s,
                        from: *from,
                        to: *to,
                    });
                    stacks.entry(name.clone()).or_default().push(r);
                    pushed.push(name.clone());
                }
                mir::MirInstr::Binary(name, a, op, b, ty) => {
                    let l = materialize(a, &mut instrs, &mut stacks, &next_value);
                    let r = materialize(b, &mut instrs, &mut stacks, &next_value);
                    let res = alloc();
                    instrs.push(SsaInstr::Binary {
                        result: res,
                        op: convert_binary_op(*op),
                        lhs: l,
                        rhs: r,
                        ty: *ty,
                    });
                    stacks.entry(name.clone()).or_default().push(res);
                    pushed.push(name.clone());
                }
                mir::MirInstr::Unary(name, op, x) => {
                    let o = materialize(x, &mut instrs, &mut stacks, &next_value);
                    let res = alloc();
                    instrs.push(SsaInstr::Unary {
                        result: res,
                        op: convert_unary_op(*op),
                        operand: o,
                    });
                    stacks.entry(name.clone()).or_default().push(res);
                    pushed.push(name.clone());
                }
                mir::MirInstr::Call(name, callee, args, ty) => {
                    let mut ssa_args = Vec::with_capacity(args.len());
                    for a in args {
                        ssa_args.push(materialize(a, &mut instrs, &mut stacks, &next_value));
                    }
                    let res = alloc();
                    instrs.push(SsaInstr::Call {
                        result: res,
                        callee: callee.clone(),
                        args: ssa_args,
                        ty: *ty,
                    });
                    stacks.entry(name.clone()).or_default().push(res);
                    pushed.push(name.clone());
                }
                mir::MirInstr::Await(name, future) => {
                    // `await` 对后端是透明的（协程由运行时调度），所以不产生
                    // 新值：future 本身就是结果。
                    let res = materialize(future, &mut instrs, &mut stacks, &next_value);
                    stacks.entry(name.clone()).or_default().push(res);
                    pushed.push(name.clone());
                }
                mir::MirInstr::Load(name, addr, ty) => {
                    let p = materialize(addr, &mut instrs, &mut stacks, &next_value);
                    let res = alloc();
                    instrs.push(SsaInstr::Load {
                        result: res,
                        ptr: p,
                        ty: *ty,
                    });
                    stacks.entry(name.clone()).or_default().push(res);
                    pushed.push(name.clone());
                }
                mir::MirInstr::SlotAddr(name, local) => {
                    let res = alloc();
                    instrs.push(SsaInstr::SlotAddr {
                        result: res,
                        local: *local,
                    });
                    stacks.entry(name.clone()).or_default().push(res);
                    pushed.push(name.clone());
                }
                mir::MirInstr::Store(addr, value, ty) => {
                    let p = materialize(addr, &mut instrs, &mut stacks, &next_value);
                    let v = materialize(value, &mut instrs, &mut stacks, &next_value);
                    instrs.push(SsaInstr::Store {
                        ptr: p,
                        value: v,
                        ty: *ty,
                    });
                    // Store 不定义任何变量，也不压栈。
                }
            mir::MirInstr::Asm {
                name: _,
                template,
                outputs,
                inputs,
            } => {
                let ssa_outputs = outputs
                    .iter()
                    .map(|(c, v, ty)| {
                        let v = materialize(v, &mut instrs, &mut stacks, &next_value);
                        (c.clone(), v, *ty)
                    })
                    .collect();
                let ssa_inputs = inputs
                    .iter()
                    .map(|(c, v, ty)| {
                        let v = materialize(v, &mut instrs, &mut stacks, &next_value);
                        (c.clone(), v, *ty)
                    })
                    .collect();
                instrs.push(SsaInstr::Asm {
                    template: template.clone(),
                    outputs: ssa_outputs,
                    inputs: ssa_inputs,
                });
                // 汇编语句本身没有值；`name` 只供 SSA 定义点记录使用。
            }
            }
        }

        out_blocks[bid].term = match block.term.as_ref() {
            None => SsaTerminator::Unreachable,
            Some(mir::MirTerminator::Return(Some(val))) => SsaTerminator::Return(Some(
                materialize(val, &mut instrs, &mut stacks, &next_value),
            )),
            Some(mir::MirTerminator::Branch {
                cond,
                then_block,
                else_block,
            }) => SsaTerminator::Branch {
                cond: materialize(cond, &mut instrs, &mut stacks, &next_value),
                then_block: *then_block,
                else_block: *else_block,
            },
            Some(other) => convert_terminator(other, &next_value),
        };
        out_blocks[bid].instrs = instrs;

        // 本块处理完，栈顶就是"沿本条路径离开该块时"的值——记成后继各个 φ
        // 的 incoming。
        //
        // 必须在递归进后继**之前**记：回边上后继往往已经访问过（φ 早就建好），
        // 但它仍然需要本块提供的 incoming。事后回填则会丢掉分支之间的差别。
        for &succ in &succs[bid] {
            let Some(made) = phi_results.get(&succ) else {
                continue;
            };
            for (name, v) in made {
                let incoming = stacks
                    .get(name)
                    .and_then(|st| st.last().copied())
                    .unwrap_or(*v);
                phi_incoming
                    .entry((succ, *v))
                    .or_default()
                    .push((bid, incoming));
            }
        }

        // LIFO：先压 Exit，再压后继的 Enter，弹出时就是"深度优先地走完
        // 后继、最后才离开本块"。
        stack.push(Step::Exit(pushed));
        for &succ in succs[bid].iter().rev() {
            if succ < n && !visited.contains(&succ) {
                stack.push(Step::Enter(succ));
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
        inline_never: func.inline_never,
        inline_always: func.inline_always,
    }
}

/// 拆分关键边：让每条需要 φ 拷贝的边都拥有自己的块。
///
/// 后端把 φ 的并行拷贝发射在**前驱块的末尾**——也就是"跳出该块之前"。
/// 这对单后继的块没问题，但对有多个后继的块就不成立：拷贝会在**每条出边上
/// 都执行**，包括那些并不经过带 φ 的后继的那条。
///
/// ```text
/// bb1: 循环头 ─┬─(真)─► bb2: 循环体 ──► ... ──► bb1
///              └─(假)─► bb3: 出口（含 φ）
/// ```
///
/// bb1 的末尾要为 bb3 做拷贝，于是条件为真的那条边上也会执行它；拷贝写的是
/// bb3 的 φ 目标寄存器，与 bb2 还要读的循环变量寄存器重叠——循环体读到的
/// 是被覆盖过的值。
///
/// 修法是标准的**关键边拆分**：在 P→B 这条边上插一个中转块 T（只含
/// `Goto(B)`）。P 于是有若干个单前驱单后继的中转块作为后继，每个中转块在
/// 自己末尾恰好做一次属于那条边的 φ 拷贝。所有后端共用这一处修复，不必
/// 各自实现"按边发射拷贝"的发射逻辑。
pub fn split_critical_edges(func: &SsaFunction) -> SsaFunction {
    let n = func.blocks.len();
    if n == 0 {
        return func.clone();
    }
    // 每个块的前驱（按块号）。
    let mut pred_count: HashMap<usize, usize> = HashMap::new();
    for b in &func.blocks {
        for s in successors_of_ssa_term(&b.term) {
            *pred_count.entry(s).or_insert(0) += 1;
        }
    }

    // 哪些块以 φ 开头（拷贝的目标）。
    let has_phi: HashMap<usize, bool> = func
        .blocks
        .iter()
        .map(|b| {
            (
                b.id,
                b.instrs.iter().any(|i| matches!(i, SsaInstr::Phi { .. })),
            )
        })
        .collect();

    let mut blocks: Vec<SsaBasicBlock> = func.blocks.clone();
    let mut next_id = blocks.iter().map(|b| b.id).max().unwrap_or(0) + 1;
    // 旧块号 -> 该边对应的新中转块号
    let mut trampoline: HashMap<(usize, usize), usize> = HashMap::new();

    for i in 0..n {
        let edges: Vec<usize> = match &blocks[i].term {
            SsaTerminator::Goto(t) => vec![*t],
            SsaTerminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                vec![*then_block, *else_block]
            }
            _ => Vec::new(),
        };
        for succ in edges {
            let needs_split = pred_count.get(&succ).copied().unwrap_or(0) > 1
                && has_phi.get(&succ).copied().unwrap_or(false)
                && succ != blocks[i].id;
            if !needs_split {
                continue;
            }
            let id = next_id;
            next_id += 1;
            trampoline.insert((blocks[i].id, succ), id);
            blocks.push(SsaBasicBlock {
                id,
                instrs: Vec::new(),
                term: SsaTerminator::Goto(succ),
            });
        }
    }

    // 重写终结符：指向中转块。
    for b in blocks.iter_mut() {
        let from = b.id;
        let redirect =
            |target: usize| -> usize { trampoline.get(&(from, target)).copied().unwrap_or(target) };
        b.term = match &b.term {
            SsaTerminator::Goto(t) => SsaTerminator::Goto(redirect(*t)),
            SsaTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => SsaTerminator::Branch {
                cond: *cond,
                then_block: redirect(*then_block),
                else_block: redirect(*else_block),
            },
            other => other.clone(),
        };
    }

    // 重写 φ 的 incoming：前驱 P 改写成中转块 T。
    for b in blocks.iter_mut() {
        for instr in &mut b.instrs {
            let SsaInstr::Phi { incoming, .. } = instr else {
                break; // φ 一定连续排在块首
            };
            for (pred, _) in incoming.iter_mut() {
                if let Some(&t) = trampoline.get(&(*pred, b.id)) {
                    *pred = t;
                }
            }
        }
    }

    SsaFunction {
        name: func.name.clone(),
        params: func.params.clone(),
        blocks,
        inline_never: func.inline_never,
        inline_always: func.inline_always,
    }
}

/// MIR 层的支配树（Cooper-Harvey-Kennedy 迭代算法）。
fn mir_dominator_tree(
    n: usize,
    preds: &[Vec<usize>],
    succs: &[Vec<usize>],
) -> HashMap<usize, usize> {
    let mut idom: HashMap<usize, usize> = HashMap::new();
    if n == 0 {
        return idom;
    }
    let order = mir_rpo(n, succs);
    let rank: HashMap<usize, usize> = order.iter().enumerate().map(|(r, &b)| (b, r)).collect();
    idom.insert(0, 0);

    let mut changed = true;
    while changed {
        changed = false;
        for &b in &order {
            if b == 0 {
                continue;
            }
            let mut new_idom: Option<usize> = None;
            for &p in &preds[b] {
                if !idom.contains_key(&p) {
                    continue;
                }
                new_idom = Some(match new_idom {
                    None => p,
                    Some(cur) => mir_intersect(&idom, &rank, p, cur),
                });
            }
            if let Some(ni) = new_idom {
                if idom.get(&b) != Some(&ni) {
                    idom.insert(b, ni);
                    changed = true;
                }
            }
        }
    }
    idom
}

/// `a` 是否支配 `b`。
fn mir_dominates(idom: &HashMap<usize, usize>, a: usize, b: usize) -> bool {
    let mut cur = b;
    for _ in 0..=idom.len() {
        if cur == a {
            return true;
        }
        match idom.get(&cur) {
            Some(&p) if p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

/// 支配树上的最近公共祖先。
fn mir_intersect(
    idom: &HashMap<usize, usize>,
    rank: &HashMap<usize, usize>,
    mut a: usize,
    mut b: usize,
) -> usize {
    let ra = |x: usize| *rank.get(&x).unwrap_or(&0);
    while a != b {
        if ra(a) <= ra(b) {
            match idom.get(&b) {
                Some(&p) if p != b => b = p,
                _ => return a,
            }
        } else {
            match idom.get(&a) {
                Some(&p) if p != a => a = p,
                _ => return b,
            }
        }
    }
    a
}

/// 逆后序（块号序列）。
fn mir_rpo(n: usize, succs: &[Vec<usize>]) -> Vec<usize> {
    let mut visited = vec![false; n];
    let mut post = Vec::with_capacity(n);
    if n == 0 {
        return post;
    }
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    visited[0] = true;
    while let Some((node, ci)) = stack.pop() {
        let ss = &succs[node];
        if ci < ss.len() {
            stack.push((node, ci + 1));
            let next = ss[ci];
            if next < n && !visited[next] {
                visited[next] = true;
                stack.push((next, 0));
            }
        } else {
            post.push(node);
        }
    }
    post.reverse();
    post
}

/// 回边 `latch -> header` 确定的那个自然循环的成员（含 header 与 latch）。
fn natural_loop(preds: &[Vec<usize>], header: usize, latch: usize) -> HashSet<usize> {
    let mut members: HashSet<usize> = HashSet::new();
    members.insert(header);
    if latch == header {
        return members;
    }
    members.insert(latch);
    let mut stack = vec![latch];
    while let Some(cur) = stack.pop() {
        for &p in &preds[cur] {
            if members.insert(p) {
                stack.push(p);
            }
        }
    }
    members
}

/// MIR 层的活跃性分析：每块**入口**处活跃的变量名。
fn mir_liveness(blocks: &[mir::MirBasicBlock], succs: &[Vec<usize>]) -> Vec<HashSet<String>> {
    let n = blocks.len();
    let uses: Vec<HashSet<String>> = blocks
        .iter()
        .map(|b| used_before_def(b).into_iter().collect())
        .collect();
    let defs: Vec<HashSet<String>> = blocks.iter().map(defined_name_set).collect();

    let mut live_in: Vec<HashSet<String>> = vec![HashSet::new(); n];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            let mut out: HashSet<String> = HashSet::new();
            for &s in &succs[b] {
                for name in &live_in[s] {
                    out.insert(name.clone());
                }
            }
            let mut next: HashSet<String> = uses[b].clone();
            for name in &out {
                if !defs[b].contains(name) {
                    next.insert(name.clone());
                }
            }
            if next != live_in[b] {
                live_in[b] = next;
                changed = true;
            }
        }
    }
    live_in
}

/// 块内被赋过值的变量名（集合形式）。
fn defined_name_set(block: &mir::MirBasicBlock) -> HashSet<String> {
    block
        .instrs
        .iter()
        .filter_map(|i| match i {
            mir::MirInstr::Assign(n, _)
            | mir::MirInstr::Binary(n, ..)
            | mir::MirInstr::Unary(n, ..)
            | mir::MirInstr::Call(n, ..)
            | mir::MirInstr::Await(n, _)
            | mir::MirInstr::Cast(n, ..)
            | mir::MirInstr::Load(n, ..)
            | mir::MirInstr::SlotAddr(n, _)
            | mir::MirInstr::Asm { name: n, .. } => Some(n),
            // Store 不定义名字。
            mir::MirInstr::Store(..) => None,
        })
        .cloned()
        .collect()
}

// ==================================================================
// φ 插入与重命名
// ==================================================================

/// 从 MIR 的终结符推出每个块的前驱集合。
/// 终结符指向的后继块。
/// SSA 终结符的后继块。
pub fn successors_of_ssa_term(term: &SsaTerminator) -> Vec<usize> {
    match term {
        SsaTerminator::Goto(t) => vec![*t],
        SsaTerminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![*then_block, *else_block],
        SsaTerminator::Return(_) | SsaTerminator::Unreachable => Vec::new(),
    }
}

pub fn successors_of_block(term: &Option<mir::MirTerminator>) -> Vec<usize> {
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

/// 某块内被赋值过的变量名（`Assign` / `Binary` / `Unary` / `Call` 的目标）。
/// 收集块内"先读后写"（含只读不写）的变量名。
///
/// 只收**读到时还没在本块被定义过**的名字。块内新定义的名字不算 use：
/// 它们由 `defs[b]` 负责，如果混进来，活跃性传播会把后继块自己产生的临时量
/// 一路反向带成"活跃"，进而给合流点插出无意义的 φ。
fn used_before_def(block: &mir::MirBasicBlock) -> Vec<String> {
    let mut defined: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();

    let note_read = |names: Vec<String>, defined: &Vec<String>, out: &mut Vec<String>| {
        for r in names {
            if !defined.contains(&r) && !out.contains(&r) {
                out.push(r);
            }
        }
    };

    for i in &block.instrs {
        // 先记本条指令读到的变量（此时它们还没被本条指令写）
        let reads: Vec<String> = match i {
            mir::MirInstr::Assign(_, v) => mir_value_names(v),
            mir::MirInstr::Binary(_, a, _, b, _) => {
                let mut v = mir_value_names(a);
                v.extend(mir_value_names(b));
                v
            }
            mir::MirInstr::Unary(_, _, a) => mir_value_names(a),
            mir::MirInstr::Call(_, _, args, _) => args.iter().flat_map(mir_value_names).collect(),
            mir::MirInstr::Await(_, v) => mir_value_names(v),
            mir::MirInstr::Cast(_, v, ..) => mir_value_names(v),
            mir::MirInstr::Load(_, addr, _) => mir_value_names(addr),
            mir::MirInstr::SlotAddr(..) => Vec::new(),
            mir::MirInstr::Store(addr, value, _) => {
                let mut v = mir_value_names(addr);
                v.extend(mir_value_names(value));
                v
            }
            mir::MirInstr::Asm { inputs, .. } => inputs
                .iter()
                .flat_map(|(_, v, _)| mir_value_names(v))
                .collect(),
        };
        note_read(reads, &defined, &mut out);

        // 再记本条指令的定义
        let name = match i {
            mir::MirInstr::Assign(n, _) => Some(n),
            mir::MirInstr::Binary(n, ..) => Some(n),
            mir::MirInstr::Unary(n, ..) => Some(n),
            mir::MirInstr::Call(n, ..) => Some(n),
            mir::MirInstr::Await(n, _) => Some(n),
            mir::MirInstr::Cast(n, ..) => Some(n),
            mir::MirInstr::Load(n, ..) => Some(n),
            mir::MirInstr::SlotAddr(n, _) => Some(n),
            mir::MirInstr::Asm { name, .. } => Some(name),
            // Store 无定义。
            mir::MirInstr::Store(..) => None,
        };
        if let Some(n) = name {
            if !defined.contains(n) {
                defined.push(n.clone());
            }
        }
    }

    // 终结符里的操作数同样是"读"
    match block.term.as_ref() {
        Some(mir::MirTerminator::Return(Some(v))) => {
            let r = mir_value_names(v);
            note_read(r, &defined, &mut out);
        }
        Some(mir::MirTerminator::Branch { cond, .. }) => {
            let r = mir_value_names(cond);
            note_read(r, &defined, &mut out);
        }
        _ => {}
    }
    out
}

/// 取 MIR 值里出现的所有变量名（常量没有名字）。
fn mir_value_names(v: &mir::MirValue) -> Vec<String> {
    match v {
        mir::MirValue::Var(n) => vec![n.clone()],
        _ => Vec::new(),
    }
}

fn defined_names(block: &mir::MirBasicBlock) -> Vec<String> {
    let mut out = Vec::new();
    for i in &block.instrs {
        let name = match i {
            mir::MirInstr::Assign(n, _) => Some(n),
            mir::MirInstr::Binary(n, ..) => Some(n),
            mir::MirInstr::Unary(n, ..) => Some(n),
            mir::MirInstr::Call(n, ..) => Some(n),
            mir::MirInstr::Await(n, _) => Some(n),
            mir::MirInstr::Cast(n, ..) => Some(n),
            mir::MirInstr::Load(n, ..) => Some(n),
            mir::MirInstr::SlotAddr(n, _) => Some(n),
            mir::MirInstr::Asm { .. } => None,
            mir::MirInstr::Store(..) => None,
        };
        if let Some(n) = name {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
    }
    out
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
        MirTerminator::Branch {
            cond,
            then_block,
            else_block,
        } => SsaTerminator::Branch {
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
    stacks: &mut HashMap<String, Vec<SsaValue>>,
    next: &Cell<u32>,
) -> SsaValue {
    match val {
        // 变量：取栈顶，即该名字当前可见的版本。
        MirValue::Var(name) => {
            if let Some(top) = stacks.get(name).and_then(|st| st.last().copied()) {
                return top;
            }
            // 不在任何栈上：形参或来自块外的引用。保留一条 `Var` 占位，
            // 由 φ 插入 / 重命名补上真正的定义。
            let v = SsaValue(next.get());
            next.set(next.get() + 1);
            instrs.push(SsaInstr::Var {
                result: v,
                name: name.clone(),
            });
            stacks.entry(name.clone()).or_default().push(v);
            v
        }
        // 字面量：发一个新值并发射 Const。
        _ => {
            let v = SsaValue(next.get());
            next.set(next.get() + 1);
            let value = match val {
                MirValue::Int(i) => SsaConst::Int(*i),
                MirValue::Bool(b) => SsaConst::Bool(*b),
                MirValue::Float(bits, w) => SsaConst::Float(*bits, *w),
                MirValue::Str { content: text, .. } => SsaConst::Str(text.clone()),
                MirValue::Unit => SsaConst::Unit,
                MirValue::Var(_) => unreachable!("上面已经处理了变量"),
            };
            instrs.push(SsaInstr::Const { result: v, value });
            v
        }
    }
}

impl SsaConst {
    /// 整数值（其他变体返回 `None`）。
    pub fn as_int(&self) -> Option<i128> {
        match self {
            SsaConst::Int(v) => Some(*v),
            _ => None,
        }
    }
}

/// 从 `(块号, SSA 终结符)` 列表求每个块的前驱。
pub fn predecessors_of(blocks: &[(usize, &SsaTerminator)]) -> HashMap<usize, Vec<usize>> {
    let mut map: HashMap<usize, Vec<usize>> = HashMap::new();
    for (id, term) in blocks {
        for s in ssa_successors(term) {
            map.entry(s).or_default().push(*id);
        }
    }
    map
}

/// SSA 终结符的后继块。
pub fn ssa_successors(term: &SsaTerminator) -> Vec<usize> {
    match term {
        SsaTerminator::Goto(t) => vec![*t],
        SsaTerminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![*then_block, *else_block],
        _ => Vec::new(),
    }
}

/// 支配树：每个块的直接支配者（idom）。
///
/// Cooper–Harvey–Kennedy 迭代算法：先算逆后序，再按逆后序反复用
/// 「所有前驱的 idom 的最近公共祖先」更新自己，直到不动点。
///
/// GVN 这类需要判断"某定义是否支配某使用"的变换**必须**依赖真实支配
/// 关系——按块号或 RPO 出现顺序近似会把兄弟分支上的值误当成支配值，
/// 替换后就是错代码。
pub fn dominator_tree(blocks: &[SsaBasicBlock]) -> HashMap<usize, usize> {
    let n = blocks.len();
    let mut idom: HashMap<usize, usize> = HashMap::new();
    if n == 0 {
        return idom;
    }
    let order = ssa_rpo_indices(blocks); // 块**下标**的 RPO
    let id: Vec<usize> = blocks.iter().map(|b| b.id).collect();
    let pos: HashMap<usize, usize> = id.iter().enumerate().map(|(i, b)| (*b, i)).collect();
    // 块号 -> RPO 序号（越大越"深"）
    let rank: HashMap<usize, usize> = order.iter().enumerate().map(|(r, &i)| (id[i], r)).collect();

    let preds = predecessors_of(&blocks.iter().map(|b| (b.id, &b.term)).collect::<Vec<_>>());
    let entry = blocks[0].id;
    idom.insert(entry, entry);

    let mut changed = true;
    while changed {
        changed = false;
        for &i in &order {
            if i == 0 {
                continue;
            }
            let b = id[i];
            let Some(ps) = preds.get(&b) else { continue };
            let mut new_idom: Option<usize> = None;
            for &p in ps {
                if !idom.contains_key(&p) {
                    continue; // 前驱尚未定 idom
                }
                new_idom = Some(match new_idom {
                    None => p,
                    Some(cur) => intersect(&idom, &rank, p, cur),
                });
            }
            if let Some(ni) = new_idom {
                if idom.get(&b) != Some(&ni) {
                    idom.insert(b, ni);
                    changed = true;
                }
            }
        }
    }
    let _ = pos;
    idom
}

/// RPO 下的块下标序列（不是块号）。
fn ssa_rpo_indices(blocks: &[SsaBasicBlock]) -> Vec<usize> {
    let n = blocks.len();
    let mut visited = vec![false; n];
    let mut post = Vec::with_capacity(n);
    if n == 0 {
        return post;
    }
    let idx: HashMap<usize, usize> = blocks.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    visited[0] = true;
    while let Some((node, ci)) = stack.pop() {
        let succs = ssa_successors(&blocks[node].term);
        if ci < succs.len() {
            stack.push((node, ci + 1));
            if let Some(&ni) = idx.get(&succs[ci]) {
                if !visited[ni] {
                    visited[ni] = true;
                    stack.push((ni, 0));
                }
            }
        } else {
            post.push(node);
        }
    }
    post.reverse();
    post
}

/// 支配树上的最近公共祖先：先把 RPO 更深的那个往上提，齐平后一起上溯。
fn intersect(
    idom: &HashMap<usize, usize>,
    rank: &HashMap<usize, usize>,
    mut a: usize,
    mut b: usize,
) -> usize {
    const UNREACHED: usize = usize::MAX;
    let ra = |x: usize| *rank.get(&x).unwrap_or(&0);
    while a != b {
        if ra(a) <= ra(b) {
            match idom.get(&b) {
                Some(&p) if p != b => b = p,
                _ => return a,
            }
        } else {
            match idom.get(&a) {
                Some(&p) if p != a => a = p,
                _ => return b,
            }
        }
    }
    let _ = UNREACHED;
    a
}

/// `a` 是否支配 `b`（含 `a == b`）。
pub fn dominates(idom: &HashMap<usize, usize>, a: usize, b: usize) -> bool {
    let mut cur = b;
    // 支配树深度有限，用步数上限避免 idom 成环时死循环
    for _ in 0..=idom.len() {
        if cur == a {
            return true;
        }
        match idom.get(&cur) {
            Some(&p) if p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

/// 指令引用的 SSA 值（公开给寄存器分配等模块用）。
pub fn instr_uses(instr: &SsaInstr, out: &mut Vec<SsaValue>) {
    match instr {
        SsaInstr::Const { .. } | SsaInstr::Var { .. } => {}
        SsaInstr::Copy { src, .. } => out.push(*src),
        SsaInstr::Cast { src, .. } => out.push(*src),
        SsaInstr::Unary { operand, .. } => out.push(*operand),
        SsaInstr::Binary { lhs, rhs, .. } => {
            out.push(*lhs);
            out.push(*rhs);
        }
        SsaInstr::Phi { incoming, .. } => {
            for (_, v) in incoming {
                out.push(*v);
            }
        }
        SsaInstr::Call { args, .. } => out.extend(args.iter().copied()),
        SsaInstr::Load { ptr, .. } => out.push(*ptr),
        // 存储读地址与值；地址是 uses，存储的值也是 uses。
        SsaInstr::Store { ptr, value, .. } => {
            out.push(*ptr);
            out.push(*value);
        }
        SsaInstr::SlotAddr { .. } => {}
        SsaInstr::Asm {
            outputs, inputs, ..
        } => {
            // 输出是"目标地址"，同样要在汇编指令处保持存活。
            for (_, addr, _) in outputs {
                out.push(*addr);
            }
            for (_, value, _) in inputs {
                out.push(*value);
            }
        }
    }
}
