//! 优化 pass 的具体实现。
//!
//! 每个 pass 都是一个纯函数 `SsaModule -> SsaModule`，按
//! [`crate::opt::OptLevel`] 分级调用。实现基于 SSA 的值编号：
//! 先把每个 `SsaValue` 映射到一个「定义」（常量或指令），再在需要时
//! 按定义做等价判定。
//!
//! 约定：只有**纯**指令可以被删除或替换。`Call`（可能改全局状态、
//! 调运行时）和 `Asm`（内联汇编）一律视为有副作用，不会被 DCE 移除。

use std::collections::{HashMap, HashSet};

use crate::ssa::{
    SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaModule, SsaTerminator, SsaUnaryOp, SsaValue,
};

// ==================================================================
// 值编号
// ==================================================================

/// 一个 SSA 值的定义：要么是常量，要么是产生它的指令。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Def {
    Const(SsaConst),
    /// `(种类, 操作数)` —— 种类用于区分不同指令。
    Op(&'static str, Vec<SsaValue>),
}

impl Def {
    /// 常量值（若该定义是常量）。
    fn as_const(&self) -> Option<&SsaConst> {
        match self {
            Def::Const(c) => Some(c),
            Def::Op(..) => None,
        }
    }
}

/// 指令是否无副作用（可被删除 / 替换）。
fn is_pure(instr: &SsaInstr) -> bool {
    // `Call` 会调用任意函数（运行时 syscall、改全局状态），不能删。
    !matches!(instr, SsaInstr::Call { .. })
}

/// 构造某条指令的定义键。
fn def_of(instr: &SsaInstr) -> Def {
    match instr {
        SsaInstr::Const { value, .. } => Def::Const(value.clone()),
        SsaInstr::Unary { op, operand, .. } => Def::Op("un", vec![*operand, tag_unary(*op)]),
        SsaInstr::Binary { op, lhs, rhs, .. } => {
            Def::Op("bin", vec![*lhs, *rhs, tag_binary(*op)])
        }
        // `Phi` 只在块首有效，`Var` 是占位：都不参与等价合并。
        _ => Def::Op("other", vec![]),
    }
}

/// 把一元的操作码编成一个假的 `SsaValue`，让它能参与 `Def` 比较。
fn tag_unary(op: SsaUnaryOp) -> SsaValue {
    SsaValue(u32::MAX - (op as u32))
}

/// 把二元的操作码编成一个假的 `SsaValue`。
fn tag_binary(op: SsaBinaryOp) -> SsaValue {
    SsaValue(u32::MAX / 2 - (op as u32))
}

/// 函数内所有值的定义表。
fn build_defs(func: &crate::ssa::SsaFunction) -> HashMap<SsaValue, Def> {
    let mut defs = HashMap::new();
    for block in &func.blocks {
        for instr in &block.instrs {
            if let Some(r) = instr.result() {
                defs.insert(r, def_of(instr));
            }
        }
    }
    defs
}

/// 函数内某个值的常量（沿定义链回溯）。
fn const_of(defs: &HashMap<SsaValue, Def>, v: SsaValue) -> Option<SsaConst> {
    defs.get(&v).and_then(|d| d.as_const()).cloned()
}

// ==================================================================
// 常量折叠
// ==================================================================

/// 常量折叠：把两个操作数都是编译期常量的运算替换成结果常量。
///
/// 反复迭代到不动点：折叠出一条新常量后，它可能让后续指令也变得可折叠
/// （例如 `(2 * 3) + 4`）。
pub fn fold_constants(module: SsaModule) -> SsaModule {
    let mut module = module;
    for func in &mut module.functions {
        loop {
            let defs = build_defs(func);
            let mut changed = false;
            for block in &mut func.blocks {
                for instr in &mut block.instrs {
                    let folded = match instr {
                        SsaInstr::Binary { op, lhs, rhs, .. } => {
                            match (const_of(&defs, *lhs), const_of(&defs, *rhs)) {
                                (Some(a), Some(b)) => eval_binary(*op, &a, &b),
                                _ => None,
                            }
                        }
                        SsaInstr::Unary { op, operand, .. } => match const_of(&defs, *operand) {
                            Some(a) => eval_unary(*op, &a),
                            None => None,
                        },
                        _ => None,
                    };
                    if let Some(value) = folded {
                        if let Some(result) = instr.result() {
                            *instr = SsaInstr::Const { result, value };
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }
    module
}

/// 常量求值：二元运算。
fn eval_binary(op: SsaBinaryOp, a: &SsaConst, b: &SsaConst) -> Option<SsaConst> {
    use SsaBinaryOp::*;
    // 只有整型常量参与求值；浮点 / 字符串等留到后端。
    let (x, y) = (a.as_int()?, b.as_int()?);
    let r = match op {
        Add => x.checked_add(y)?,
        Sub => x.checked_sub(y)?,
        Mul => x.checked_mul(y)?,
        Div => {
            if y == 0 {
                return None; // 除零交给运行时，不折叠
            }
            x.checked_div(y)?
        }
        Rem => {
            if y == 0 {
                return None;
            }
            x.checked_rem(y)?
        }
        BitAnd => x & y,
        BitOr => x | y,
        BitXor => x ^ y,
        Shl => {
            if !(0..64).contains(&y) {
                return None;
            }
            x.checked_shl(y as u32)?
        }
        Shr => {
            if !(0..64).contains(&y) {
                return None;
            }
            x.checked_shr(y as u32)?
        }
        // 比较运算恒为 0/1
        Eq => bool_const(x == y),
        Ne => bool_const(x != y),
        Lt => bool_const(x < y),
        Gt => bool_const(x > y),
        Le => bool_const(x <= y),
        Ge => bool_const(x >= y),
        And => bool_const(x != 0 && y != 0),
        Or => bool_const(x != 0 || y != 0),
    };
    Some(SsaConst::Int(r))
}

/// 比较 / 逻辑运算的结果：真为 1，假为 0。
fn bool_const(b: bool) -> i128 {
    i128::from(b)
}

/// 常量求值：一元运算。
fn eval_unary(op: SsaUnaryOp, a: &SsaConst) -> Option<SsaConst> {
    let x = a.as_int()?;
    let r = match op {
        SsaUnaryOp::Not => bool_const(x == 0),
        SsaUnaryOp::BitNot => !x,
        SsaUnaryOp::Neg => x.checked_neg()?,
    };
    Some(SsaConst::Int(r))
}

// ==================================================================
// 死代码消除
// ==================================================================

/// 指令引用的 SSA 值。
fn uses(instr: &SsaInstr, out: &mut Vec<SsaValue>) {
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
    }
}

/// 终结符引用的 SSA 值。
fn term_uses(term: &SsaTerminator, out: &mut Vec<SsaValue>) {
    match term {
        SsaTerminator::Return(Some(v)) => out.push(*v),
        SsaTerminator::Branch { cond, .. } => out.push(*cond),
        _ => {}
    }
}

/// 死代码消除（DCE）。
///
/// 从终结符与有副作用的指令出发做反向标记，只有被标记的指令才保留。
/// `Call` / `Asm` 视为根（副作用），不会被删。
pub fn eliminate_dead_code(module: SsaModule) -> SsaModule {
    let mut module = module;
    for func in &mut module.functions {
        // 每个块里的值 → (块下标, 指令下标)
        let mut def_site: HashMap<SsaValue, (usize, usize)> = HashMap::new();
        for (bi, block) in func.blocks.iter().enumerate() {
            for (ii, instr) in block.instrs.iter().enumerate() {
                if let Some(r) = instr.result() {
                    def_site.insert(r, (bi, ii));
                }
            }
        }

        let mut live: HashSet<SsaValue> = HashSet::new();
        let mut work: Vec<SsaValue> = Vec::new();

        // 根：终结符引用的值 + 有副作用的指令引用的值
        for block in &func.blocks {
            let mut us = Vec::new();
            term_uses(&block.term, &mut us);
            for instr in &block.instrs {
                if !is_pure(instr) {
                    uses(instr, &mut us);
                }
            }
            for v in us {
                if live.insert(v) {
                    work.push(v);
                }
            }
        }

        // 传播：一个值存活 -> 产生它的指令的操作数也存活
        while let Some(v) = work.pop() {
            let Some(&(bi, ii)) = def_site.get(&v) else {
                continue;
            };
            let mut us = Vec::new();
            uses(&func.blocks[bi].instrs[ii], &mut us);
            for u in us {
                if live.insert(u) {
                    work.push(u);
                }
            }
        }

        // 删掉既不存活、又没有副作用的指令
        for block in &mut func.blocks {
            block
                .instrs
                .retain(|instr| match instr.result() {
                    Some(r) => live.contains(&r) || !is_pure(instr),
                    None => true,
                });
        }
    }
    module
}

// ==================================================================
// 局部公共子表达式消除
// ==================================================================

/// 局部 CSE：在每个基本块内，把重复的纯运算替换成第一次的结果。
///
/// 只在块内做（局部值编号），因此不需要支配关系，跨块的重复交给 GVN。
pub fn eliminate_common_subexpr(module: SsaModule) -> SsaModule {
    let mut module = module;
    for func in &mut module.functions {
        for block in &mut func.blocks {
            // 块首的 phi 不能被替换：它是块与前驱的接口
            let n_phi = block.instrs.iter().take_while(|i| matches!(i, SsaInstr::Phi { .. })).count();
            let mut seen: HashMap<Def, SsaValue> = HashMap::new();
            for i in n_phi..block.instrs.len() {
                let Some(result) = block.instrs[i].result() else {
                    continue;
                };
                if !is_pure(&block.instrs[i]) {
                    continue;
                }
                let d = def_of(&block.instrs[i]);
                if matches!(d, Def::Op("other", _)) {
                    continue;
                }
                match seen.get(&d) {
                    // 注意：操作数相同时定义才等价，所以直接替换成旧值
                    Some(&prev) => {
                        block.instrs[i] = SsaInstr::Copy { result, src: prev }
                    }
                    None => {
                        seen.insert(d, result);
                    }
                }
            }
        }
    }
    module
}

// ==================================================================
// 全局值编号
// ==================================================================

/// 全局值编号（GVN）：跨基本块合并等价的纯运算。
///
/// 正确性依赖**真实支配关系**：只有当候选定义支配当前指令时才复用。
/// 这里用 [`crate::ssa::dominator_tree`] 求 idom，再判断支配集合——
/// 早先版本用"RPO 里处理得早"当支配的近似，会把兄弟分支上的值当成
/// 支配值，替换后生成错代码（`if` 的两个分支互为兄弟，必然中招）。
pub fn global_value_numbering(module: SsaModule) -> SsaModule {
    let mut module = module;
    for func in &mut module.functions {
        let idom = crate::ssa::dominator_tree(&func.blocks);
        let phi_values: HashSet<SsaValue> = func
            .blocks
            .iter()
            .flat_map(|b| b.instrs.iter())
            .filter_map(|i| match i {
                SsaInstr::Phi { result, .. } => Some(*result),
                _ => None,
            })
            .collect();

        let mut def_block: HashMap<SsaValue, usize> = HashMap::new();
        for b in &func.blocks {
            for instr in &b.instrs {
                if let Some(r) = instr.result() {
                    def_block.insert(r, b.id);
                }
            }
        }

        let mut seen: HashMap<Def, (SsaValue, usize)> = HashMap::new();
        let blocks = std::mem::take(&mut func.blocks);
        let mut rebuilt = Vec::with_capacity(blocks.len());
        for b in blocks {
            let mut instrs = b.instrs;
            let n_phi = instrs
                .iter()
                .take_while(|i| matches!(i, SsaInstr::Phi { .. }))
                .count();
            for i in n_phi..instrs.len() {
                let Some(result) = instrs[i].result() else {
                    continue;
                };
                let d = def_of(&instrs[i]);
                let candidate = if is_pure(&instrs[i]) && !matches!(d, Def::Op("other", _)) {
                    seen.get(&d).copied()
                } else {
                    None
                };
                match candidate {
                    Some((prev, prev_block)) if crate::ssa::dominates(&idom, prev_block, b.id) => {
                        let mut us = Vec::new();
                        uses(&instrs[i], &mut us);
                        let operands_ok = us
                            .iter()
                            .all(|u| !phi_values.contains(u) && def_block.get(u) != Some(&b.id));
                        if operands_ok {
                            instrs[i] = SsaInstr::Copy { result, src: prev };
                        }
                    }
                    _ => {
                        if is_pure(&instrs[i]) && !matches!(d, Def::Op("other", _)) {
                            seen.entry(d).or_insert((result, b.id));
                        }
                    }
                }
            }
            rebuilt.push(crate::ssa::SsaBasicBlock {
                id: b.id,
                instrs,
                term: b.term,
            });
        }
        func.blocks = rebuilt;
    }
    module
}


// ==================================================================
// 窥孔优化
// ==================================================================

/// 窥孔优化：块内的局部模式替换。
///
/// 目前覆盖：
/// * `x + 0` / `x - 0` / `x | 0` / `x ^ 0` → `x`
/// * `x * 1` → `x`，`x / 1` → `x`
/// * `x & -1` → `x`
/// * `x - x` → `0`，`x ^ x` → `0`，`x | x` → `x`
pub fn peephole(module: SsaModule) -> SsaModule {
    let mut module = module;
    for func in &mut module.functions {
        let defs = build_defs(func);
        for block in &mut func.blocks {
            for instr in &mut block.instrs {
                let Some(result) = instr.result() else {
                    continue;
                };
                let SsaInstr::Binary {
                    result: _,
                    op,
                    lhs,
                    rhs,
                } = instr
                else {
                    continue;
                };
                let (op, lhs, rhs) = (*op, *lhs, *rhs);
                let b = const_of(&defs, rhs);
                let repl: Option<Def> = match op {
                    // x + 0 / x - 0 / x | 0 / x ^ 0 -> x
                    SsaBinaryOp::Add | SsaBinaryOp::Sub | SsaBinaryOp::BitOr
                    | SsaBinaryOp::BitXor
                        if b.as_ref().and_then(SsaConst::as_int) == Some(0) =>
                    {
                        Some(Def::Op("copy", vec![lhs]))
                    }
                    // x * 1 / x / 1 -> x
                    SsaBinaryOp::Mul | SsaBinaryOp::Div
                        if b.as_ref().and_then(SsaConst::as_int) == Some(1) =>
                    {
                        Some(Def::Op("copy", vec![lhs]))
                    }
                    // x & -1 -> x
                    SsaBinaryOp::BitAnd
                        if b.as_ref().and_then(SsaConst::as_int) == Some(-1) =>
                    {
                        Some(Def::Op("copy", vec![lhs]))
                    }
                    // x - x / x ^ x -> 0
                    SsaBinaryOp::Sub | SsaBinaryOp::BitXor if lhs == rhs => {
                        Some(Def::Const(SsaConst::Int(0)))
                    }
                    // x | x -> x
                    SsaBinaryOp::BitOr if lhs == rhs => Some(Def::Op("copy", vec![lhs])),
                    _ => None,
                };
                match repl {
                    Some(Def::Const(c)) => *instr = SsaInstr::Const { result, value: c },
                    // `copy` 用 Var 表达：后端把它发射成一条寄存器间 move
                    Some(Def::Op(_, refs)) if refs.len() == 1 => {
                        *instr = SsaInstr::Copy { result, src: refs[0] };
                    }
                    _ => {}
                }
            }
        }
    }
    module
}

// ==================================================================
// 函数内联
// ==================================================================

/// 函数内联。
///
/// 只处理**只有一个基本块、且不递归**的被调函数——这是最容易保证正确的
/// 子集。调用点按实参替换形参。
pub fn inline_functions(module: SsaModule) -> SsaModule {
    let mut module = module;

    // 可内联的单块函数：名字 -> (形参, 函数体, 返回值)
    let mut candidates: HashMap<String, (Vec<SsaValue>, Vec<SsaInstr>, SsaValue)> =
        HashMap::new();
    for func in &module.functions {
        if func.blocks.len() != 1 {
            continue;
        }
        // `#[inline(never)]` 的函数不作为候选——属性语义在这里生效
        if func.inline_never {
            continue;
        }
        let block = &func.blocks[0];
        let SsaTerminator::Return(Some(ret)) = block.term else {
            continue;
        };
        candidates.insert(
            func.name.clone(),
            (func.params.clone(), block.instrs.clone(), ret),
        );
    }
    if candidates.is_empty() {
        return module;
    }

    for func in &mut module.functions {
        // 内联进来的指令要用**本函数从未用过**的值号。SsaValue 的 id 是
        // 每个函数独立编号的，所以从本函数现有最大值之后开始发号；
        // 早先从 `call` 结果 id + 1 起发，会和本函数已有的值撞号。
        let mut next = max_value_id(func) as i64 + 1;

        for block in &mut func.blocks {
            // 收集替换，再统一写回：直接边遍历边 splice 会造成两次可变借用
            let mut splices: Vec<(usize, Vec<SsaInstr>)> = Vec::new();
            for i in 0..block.instrs.len() {
                let SsaInstr::Call {
                    result,
                    callee,
                    args,
                } = &block.instrs[i]
                else {
                    continue;
                };
                // 不内联自己（递归）
                if *callee == func.name {
                    continue;
                }
                let Some((params, body, ret)) = candidates.get(callee) else {
                    continue;
                };
                if params.len() != args.len() {
                    continue;
                }
                // 形参直接换成调用点的实参，不给它们另发号
                let subst: HashMap<SsaValue, SsaValue> =
                    params.iter().copied().zip(args.iter().copied()).collect();
                let mut renamed: HashMap<SsaValue, SsaValue> = HashMap::new();
                let mut emit: Vec<SsaInstr> = Vec::with_capacity(body.len());
                for b in body {
                    let mut new_instr = b.clone();
                    let mut us = Vec::new();
                    uses(b, &mut us);
                    for u in us {
                        let target = if let Some(a) = subst.get(&u) {
                            *a
                        } else {
                            renamed.get(&u).copied().unwrap_or(u)
                        };
                        replace_operand(&mut new_instr, u, target);
                    }
                    if let Some(r) = new_instr.result() {
                        let mapped = SsaValue(next as u32);
                        next += 1;
                        renamed.insert(r, mapped);
                        new_instr = set_result(new_instr, mapped);
                    }
                    emit.push(new_instr);
                }
                // 返回值 = 被调函数返回值重命名后的值（返回形参时用实参）
                let ret_src = subst
                    .get(ret)
                    .copied()
                    .or_else(|| renamed.get(ret).copied())
                    .unwrap_or(*ret);

                let mut new_block = emit;
                new_block.push(SsaInstr::Copy {
                    result: *result,
                    src: ret_src,
                });
                splices.push((i, new_block));
            }
            // 倒序应用，避免下标失效
            for (at, repl) in splices.into_iter().rev() {
                block.instrs.splice(at..at + 1, repl);
            }
        }
    }
    module
}

/// 函数里出现过的最大 `SsaValue` id（没有则 0）。
fn max_value_id(func: &SsaFunction) -> u32 {
    let mut max = 0u32;
    let mut bump = |v: SsaValue| {
        if v.id() > max {
            max = v.id();
        }
    };
    for p in &func.params {
        bump(*p);
    }
    for block in &func.blocks {
        for instr in &block.instrs {
            if let Some(r) = instr.result() {
                bump(r);
            }
            let mut us = Vec::new();
            uses(instr, &mut us);
            for u in us {
                bump(u);
            }
        }
        match &block.term {
            SsaTerminator::Return(Some(v)) => bump(*v),
            SsaTerminator::Branch { cond, .. } => bump(*cond),
            _ => {}
        }
    }
    max
}

/// 把指令里所有对 `from` 的引用换成 `to`。
fn replace_operand(instr: &mut SsaInstr, from: SsaValue, to: SsaValue) {
    match instr {
        SsaInstr::Unary { operand, .. } => {
            if *operand == from {
                *operand = to;
            }
        }
        SsaInstr::Binary { lhs, rhs, .. } => {
            if *lhs == from {
                *lhs = to;
            }
            if *rhs == from {
                *rhs = to;
            }
        }
        SsaInstr::Phi { incoming, .. } => {
            for (_, v) in incoming.iter_mut() {
                if *v == from {
                    *v = to;
                }
            }
        }
        SsaInstr::Call { args, .. } => {
            for a in args.iter_mut() {
                if *a == from {
                    *a = to;
                }
            }
        }
        SsaInstr::Copy { src, .. } => {
            if *src == from {
                *src = to;
            }
        }
        _ => {}
    }
}

/// 返回一条 `result` 被改成 `new` 的同型指令。
fn set_result(instr: SsaInstr, new: SsaValue) -> SsaInstr {
    match instr {
        SsaInstr::Const { value, .. } => SsaInstr::Const { result: new, value },
        SsaInstr::Unary { op, operand, .. } => SsaInstr::Unary {
            result: new,
            op,
            operand,
        },
        SsaInstr::Binary { op, lhs, rhs, .. } => SsaInstr::Binary {
            result: new,
            op,
            lhs,
            rhs,
        },
        SsaInstr::Phi { name, incoming, .. } => SsaInstr::Phi {
            result: new,
            name,
            incoming,
        },
        SsaInstr::Var { name, .. } => SsaInstr::Var { result: new, name },
        SsaInstr::Copy { src, .. } => SsaInstr::Copy { result: new, src },
        SsaInstr::Cast { src, from, to, .. } => SsaInstr::Cast {
            result: new,
            src,
            from,
            to,
        },
        // `Call` 必须一起改，否则内联时调用点保留了被调函数的旧结果号，
        // 后面追加的 `copy` 却指向新发号的临时量，直接读到未定义的值。
        SsaInstr::Call {
            callee, args, ..
        } => SsaInstr::Call {
            result: new,
            callee,
            args,
        },
    }
}

// ==================================================================
// 循环优化
// ==================================================================

/// 循环优化：识别自然循环，把**循环不变量**提到循环头之前。
///
/// 只处理一条指令级的不变量：表达式里引用的值都定义在循环**外面**，
/// 且该指令自身无副作用。这类指令在循环里重复执行但结果不变。
pub fn optimize_loops(module: SsaModule) -> SsaModule {
    let mut module = module;
    for func in &mut module.functions {
        let loops = find_natural_loops(&func.blocks);
        for (header, members) in loops {
            hoist_invariants(func, header, &members);
        }
    }
    module
}

/// 用「回边」求自然循环：块 `b` 若有一条回边 `latch -> header`，
/// 则所有能到达 `latch` 的块（回边上的除外）都属于该循环。
fn find_natural_loops(blocks: &[crate::ssa::SsaBasicBlock]) -> Vec<(usize, Vec<usize>)> {
    let mut out = Vec::new();
    let pairs: Vec<(usize, &crate::ssa::SsaTerminator)> =
        blocks.iter().map(|b| (b.id, &b.term)).collect();
    let preds = crate::ssa::predecessors_of(&pairs);
    for b in blocks {
        let SsaTerminator::Goto(target) = b.term else {
            continue;
        };
        // 回边：target 支配 b。这里用「块号不大于当前块」近似判定——
        // RPO 编号下，回边总是指向更早的块。
        if target > b.id {
            continue;
        }
        // 反向可达：从 latch 往上走
        let mut members = vec![b.id, target];
        let mut stack = vec![b.id];
        while let Some(cur) = stack.pop() {
            if let Some(ps) = preds.get(&cur) {
                for &p in ps {
                    if !members.contains(&p) {
                        members.push(p);
                        stack.push(p);
                    }
                }
            }
        }
        out.push((target, members));
    }
    out
}

/// 把循环不变量提到循环头之前。
fn hoist_invariants(func: &mut crate::ssa::SsaFunction, header: usize, members: &[usize]) {
    let defs = build_defs(func);
    // 循环内定义的值
    let mut inside = HashSet::new();
    for &b in members {
        for instr in &func.blocks[b].instrs {
            if let Some(r) = instr.result() {
                inside.insert(r);
            }
        }
    }

    let header_pos = func.blocks[header]
        .instrs
        .iter()
        .take_while(|i| matches!(i, SsaInstr::Phi { .. }))
        .count();
    let mut hoisted: Vec<SsaInstr> = Vec::new();
    let mut i = header_pos;
    while i < func.blocks[header].instrs.len() {
        let inv = {
            let instr = &func.blocks[header].instrs[i];
            if !is_pure(instr) || !inside.contains(&instr.result().unwrap_or(SsaValue(u32::MAX))) {
                false
            } else {
                let mut us = Vec::new();
                uses(instr, &mut us);
                // 操作数全部来自循环外 -> 不变量
                us.iter().all(|u| !inside.contains(u) || *u == instr.result().unwrap())
            }
        };
        if inv {
            hoisted.push(func.blocks[header].instrs[i].clone());
            func.blocks[header].instrs.remove(i);
        } else {
            i += 1;
        }
    }
    if hoisted.is_empty() {
        return;
    }
    // 插到循环头的 phi 之后、其余指令之前
    let at = func.blocks[header]
        .instrs
        .iter()
        .take_while(|i| matches!(i, SsaInstr::Phi { .. }))
        .count();
    for (k, instr) in hoisted.into_iter().enumerate() {
        func.blocks[header].instrs.insert(at + k, instr);
    }
    let _ = defs;
}
