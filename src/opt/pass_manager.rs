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
    // `Load`/`Store` 触及内存：`Store` 有副作用不能删；`Load` 读取的值会
    // 被其它写改变，既不能当纯表达式合并（CSE）也不能提出循环（LICM）。
    !matches!(
        instr,
        SsaInstr::Call { .. } | SsaInstr::Load { .. } | SsaInstr::Store { .. }
    )
}

/// 构造某条指令的定义键。
fn def_of(instr: &SsaInstr) -> Def {
    match instr {
        SsaInstr::Const { value, .. } => Def::Const(value.clone()),
        SsaInstr::Unary { op, operand, .. } => Def::Op("un", vec![*operand, tag_unary(*op)]),
        SsaInstr::Binary { op, lhs, rhs, .. } => Def::Op("bin", vec![*lhs, *rhs, tag_binary(*op)]),
        // 转换参与等价合并：`x as i32` 在同一块里算两次是纯浪费。
        // 目标类型必须进键，否则 `x as i32` 与 `x as u64` 会被当成同一条。
        SsaInstr::Cast { src, from, to, .. } => {
            Def::Op("cast", vec![*src, tag_num_ty(*from), tag_num_ty(*to)])
        }
        // `Copy` 就是一次"寄存器搬���"，同一对 (结果, 源) 重复出现时第二条
        // 可以直接删掉（它的结果与第一条相同）。
        SsaInstr::Copy { src, .. } => Def::Op("copy", vec![*src]),
        // 每个局部变量的栈槽地址都是不同的常量（但结果不等于彼此的）：
        // 把 local id 编进键，避免 CSE 把 `&x` 与 `&y` 当成同一条。
        SsaInstr::SlotAddr { local, .. } => Def::Op("slot", vec![SsaValue(*local)]),
        // `Load`/`Store` 是 `is_pure == false` 的，不会被 CSE 查看；这里给个
        // 独立键仅为满足穷尽匹配。
        SsaInstr::Load { .. } | SsaInstr::Store { .. } => Def::Op("mem", vec![]),
        // `Phi` 只在块首有效（跨块合并由 GVN 负责），`Var` 是占位：
        // 两者都不参与块内等价合并。
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

/// 把数值类型编成一个假的 `SsaValue`（转换等价性的一部分）。
fn tag_num_ty(ty: crate::mir::NumTy) -> SsaValue {
    use crate::mir::NumTy;
    let code = match ty {
        NumTy::Int(b) => u32::from(b),
        NumTy::Uint(b) => 100 + u32::from(b),
        NumTy::Float(b) => 200 + u32::from(b),
        NumTy::Bool => 300,
    };
    SsaValue(u32::MAX / 3 + code)
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
    // 浮点先走这一支：位运算与取模对浮点没有意义，其余按 IEEE 直接算。
    if let (SsaConst::Float(x, wx), SsaConst::Float(y, wy)) = (a, b) {
        return float_binary(op, *x, *y, (*wx).max(*wy));
    }
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

/// 浮点常量的二元运算。
///
/// 折叠出来的位模式必须与运行期**逐位相同**，所以 NaN 也照常折（IEEE 的
/// 运算结果确定，不是不确定）。f32 必须用 f32 的算术来折——用 f64 算完再
/// 截断会在非精确值上得到不同的舍入结果。
fn float_binary(op: SsaBinaryOp, x_bits: u64, y_bits: u64, width: u8) -> Option<SsaConst> {
    use SsaBinaryOp::*;
    if width == 32 {
        let x = f32::from_bits(x_bits as u32);
        let y = f32::from_bits(y_bits as u32);
        return Some(match op {
            Add => SsaConst::Float((x + y).to_bits() as u64, 32),
            Sub => SsaConst::Float((x - y).to_bits() as u64, 32),
            Mul => SsaConst::Float((x * y).to_bits() as u64, 32),
            Div => {
                if y == 0.0 {
                    return None;
                }
                SsaConst::Float((x / y).to_bits() as u64, 32)
            }
            Eq => SsaConst::Int(i128::from(x == y)),
            Ne => SsaConst::Int(i128::from(x != y)),
            Lt => SsaConst::Int(i128::from(x < y)),
            Gt => SsaConst::Int(i128::from(x > y)),
            Le => SsaConst::Int(i128::from(x <= y)),
            Ge => SsaConst::Int(i128::from(x >= y)),
            _ => return None,
        });
    }
    let x = f64::from_bits(x_bits);
    let y = f64::from_bits(y_bits);
    Some(match op {
        Add => SsaConst::Float((x + y).to_bits(), 64),
        Sub => SsaConst::Float((x - y).to_bits(), 64),
        Mul => SsaConst::Float((x * y).to_bits(), 64),
        Div => {
            if y == 0.0 {
                // 除零在编译期必须报错，不能折出一个与运行期不同的值
                // （运行期会得到 inf 或 NaN）。交给调用方保留原样。
                return None;
            }
            SsaConst::Float((x / y).to_bits(), 64)
        }
        Eq => SsaConst::Int(i128::from(x == y)),
        Ne => SsaConst::Int(i128::from(x != y)),
        Lt => SsaConst::Int(i128::from(x < y)),
        Gt => SsaConst::Int(i128::from(x > y)),
        Le => SsaConst::Int(i128::from(x <= y)),
        Ge => SsaConst::Int(i128::from(x >= y)),
        // 位运算 / 取模 / 移位对浮点无定义：不折叠。
        _ => return None,
    })
}

/// 比较 / 逻辑运算的结果：真为 1，假为 0。
fn bool_const(b: bool) -> i128 {
    i128::from(b)
}

/// 常量求值：一元运算。
fn eval_unary(op: SsaUnaryOp, a: &SsaConst) -> Option<SsaConst> {
    if let SsaConst::Float(bits, w) = a {
        if *w == 32 {
            let x = f32::from_bits(*bits as u32);
            return match op {
                SsaUnaryOp::Neg => Some(SsaConst::Float((-x).to_bits() as u64, 32)),
                SsaUnaryOp::Not => Some(SsaConst::Int(i128::from(x == 0.0))),
                SsaUnaryOp::BitNot => None,
            };
        }
        let x = f64::from_bits(*bits);
        return match op {
            SsaUnaryOp::Neg => Some(SsaConst::Float((-x).to_bits(), 64)),
            SsaUnaryOp::Not => Some(SsaConst::Int(i128::from(x == 0.0))),
            // `~` 对浮点无定义。
            SsaUnaryOp::BitNot => None,
        };
    }
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
        SsaInstr::Load { ptr, .. } => out.push(*ptr),
        SsaInstr::Store { ptr, value, .. } => {
            out.push(*ptr);
            out.push(*value);
        }
        SsaInstr::SlotAddr { .. } => {}
        SsaInstr::Asm { inputs, .. } => {
            for (_, v) in inputs {
                out.push(*v);
            }
        }
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
            block.instrs.retain(|instr| match instr.result() {
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
            let n_phi = block
                .instrs
                .iter()
                .take_while(|i| matches!(i, SsaInstr::Phi { .. }))
                .count();
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
                    Some(&prev) => block.instrs[i] = SsaInstr::Copy { result, src: prev },
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
// 死块消除
// ==================================================================

/// 死块消除：从入口出发做可达性标记，删掉到不了的块。
///
/// 不可达的块看起来"无害"（后端照样给它分配基本块标签、照样发射代码），
/// 但它会让后面的 GVN / 循环优化把块之间的支配关系算错——支配树是从入口
/// 迭代出来的，孤立块没有 idom，`dominates` 的结果就没有意义。删掉比留着
/// 让下游提心吊胆可靠。
pub fn eliminate_dead_blocks(module: SsaModule) -> SsaModule {
    let mut module = module;
    for func in &mut module.functions {
        if func.blocks.is_empty() {
            continue;
        }
        // 入口块是下标 0（所有后端都这么假设）。
        let mut live = vec![false; func.blocks.len()];
        let index_of: HashMap<usize, usize> = func
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| (b.id, i))
            .collect();
        let mut work = vec![0usize];
        live[0] = true;
        while let Some(cur) = work.pop() {
            for succ in crate::ssa::ssa_successors(&func.blocks[cur].term) {
                if let Some(&si) = index_of.get(&succ) {
                    if !live[si] {
                        live[si] = true;
                        work.push(si);
                    }
                }
            }
        }
        if live.iter().all(|&l| l) {
            continue;
        }
        // 保留可达块。任何指向**被删块**的跳转都改写成 `Unreachable`——
        // 后端会拿块号去查符号表，指向不存在的块会生成一条指向未定义符号的
        // 跳转，链接期才炸成"重定位解析不到"。
        let alive = |t: usize| index_of.get(&t).is_some_and(|&j| live[j]);
        let mut kept = Vec::with_capacity(func.blocks.len());
        for (i, block) in func.blocks.iter().enumerate() {
            if !live[i] {
                continue;
            }
            let mut block = block.clone();
            block.term = match &block.term {
                crate::ssa::SsaTerminator::Goto(t) if !alive(*t) => {
                    crate::ssa::SsaTerminator::Unreachable
                }
                crate::ssa::SsaTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                } if alive(*then_block) && alive(*else_block) => {
                    crate::ssa::SsaTerminator::Branch {
                        cond: *cond,
                        then_block: *then_block,
                        else_block: *else_block,
                    }
                }
                crate::ssa::SsaTerminator::Branch { .. } => crate::ssa::SsaTerminator::Unreachable,
                other => other.clone(),
            };
            kept.push(block);
        }

        // 重新编号：删块之后块号不再是连续的 `0..k`，而**下游大量代码是用
        // 块号当下标**访问 `func.blocks` 的（`hoist_invariants`、
        // `find_natural_loops`、寄存器分配的 `block_range`……）。
        // 留着空洞等于给它们埋一个越界 panic，而且是在离现场很远的地方炸。
        //
        // 顺序保持不变，入口块（原本 id 0，必然可达）因此仍在下标 0——
        // 后端与支配树都依赖这一点。
        let remap: HashMap<usize, usize> = kept
            .iter()
            .enumerate()
            .map(|(new, b)| (b.id, new))
            .collect();
        for block in kept.iter_mut() {
            block.id = remap[&block.id];
            block.term = match &block.term {
                crate::ssa::SsaTerminator::Goto(t) => crate::ssa::SsaTerminator::Goto(remap[t]),
                crate::ssa::SsaTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                } => crate::ssa::SsaTerminator::Branch {
                    cond: *cond,
                    then_block: remap[then_block],
                    else_block: remap[else_block],
                },
                other => other.clone(),
            };
            for instr in &mut block.instrs {
                let crate::ssa::SsaInstr::Phi { incoming, .. } = instr else {
                    break; // φ 一定连续排在块首
                };
                for (pred, _) in incoming.iter_mut() {
                    if let Some(&new) = remap.get(pred) {
                        *pred = new;
                    }
                }
            }
        }
        func.blocks = kept;
    }
    module
}

// ==================================================================
// 稀疏条件常量传播（SCCP）
// ==================================================================

/// 稀疏条件常量传播。
///
/// 与"常量折叠 + DCE"叠加的差别：折叠只在**两个操作数都是常量**时生效，
/// 而条件为常量时可以把整个分支的另一半变成死代码。
///
/// ```text
/// if comptime_flag { A } else { B }   // 条件恒为 true
/// ```
///
/// 恒定的条件让 `B` 不可达，`B` 里的计算随之全部作废——这在 constexpr /
/// comptime 展开出来的代码里很常见。
///
/// 实现是标准的"格"传播：每个值取 {未定义(undef), 常量, 顶部(⊤)} 三档，
/// 迭代到不动点；随后重写常量、删掉不可达块。
pub fn sparse_conditional_constant_propagation(module: SsaModule) -> SsaModule {
    let mut module = module;
    for func in &mut module.functions {
        propagate_constants(func);
    }
    module
}

/// 值格：未定义 / 常量 / 顶部。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lattice {
    /// 未定义：还没有任何定义到达。只在不动点里作为下界出现。
    #[allow(dead_code)]
    Undef,
    Const(SsaConst),
    Top,
}

/// 单个函数内的 SCCP。
fn propagate_constants(func: &mut SsaFunction) {
    if func.blocks.is_empty() {
        return;
    }
    let n = func.blocks.len();
    let index_of: HashMap<usize, usize> = func
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id, i))
        .collect();

    let mut lattice: HashMap<SsaValue, Lattice> = HashMap::new();
    // 参数与函数外常量直接定为 ⊤（调用点传的什么值不知道）。
    for p in &func.params {
        lattice.insert(*p, Lattice::Top);
    }
    let mut reachable = vec![false; n];
    reachable[0] = true;
    let mut changed = true;

    while changed {
        changed = false;
        for bi in 0..n {
            if !reachable[bi] {
                continue;
            }
            let block = &func.blocks[bi];
            for instr in &block.instrs {
                match instr {
                    SsaInstr::Phi {
                        result, incoming, ..
                    } => {
                        // φ 的取值取决于**本块**这条入边上的传入值。
                        let Some((_, v)) = incoming.iter().find(|(p, _)| *p == block.id) else {
                            continue;
                        };
                        let lv = lookup(&lattice, *v);
                        let merged = merge_lattice(&lattice, *result, lv);
                        if set_lattice(&mut lattice, *result, merged) {
                            changed = true;
                        }
                    }
                    SsaInstr::Const { result, value } => {
                        if set_lattice(&mut lattice, *result, Lattice::Const(value.clone())) {
                            changed = true;
                        }
                    }
                    SsaInstr::Unary {
                        result,
                        op,
                        operand,
                    } => {
                        let lv = lookup(&lattice, *operand);
                        let new = match lv {
                            Lattice::Const(c) => match eval_unary(*op, &c) {
                                Some(v) => Lattice::Const(v),
                                None => Lattice::Top,
                            },
                            _ => Lattice::Top,
                        };
                        if set_lattice(&mut lattice, *result, new) {
                            changed = true;
                        }
                    }
                    SsaInstr::Binary {
                        result,
                        op,
                        lhs,
                        rhs,
                        ..
                    } => {
                        let (l, r) = (lookup(&lattice, *lhs), lookup(&lattice, *rhs));
                        let new = match (l, r) {
                            (Lattice::Const(a), Lattice::Const(b)) => {
                                match eval_binary(*op, &a, &b) {
                                    Some(v) => Lattice::Const(v),
                                    None => Lattice::Top,
                                }
                            }
                            // 一个常量一个 ⊤：运算结果不定（除非是 x*0 这类，
                            // 那交给窥孔去处理，这里保持保守）。
                            _ => Lattice::Top,
                        };
                        if set_lattice(&mut lattice, *result, new) {
                            changed = true;
                        }
                    }
                    SsaInstr::Cast {
                        result,
                        src,
                        from,
                        to,
                    } => {
                        let lv = lookup(&lattice, *src);
                        let new = match lv {
                            Lattice::Const(c) => match eval_cast(&c, *from, *to) {
                                Some(v) => Lattice::Const(v),
                                None => Lattice::Top,
                            },
                            _ => Lattice::Top,
                        };
                        if set_lattice(&mut lattice, *result, new) {
                            changed = true;
                        }
                    }
                    // `Copy` 只是搬运。
                    SsaInstr::Copy { result, src } => {
                        let lv = lookup(&lattice, *src);
                        if set_lattice(&mut lattice, *result, lv) {
                            changed = true;
                        }
                    }
                    // 调用可能改全局状态；`Var` 是占位；Load/SlotAddr 的结果
                    // 不是编译期常量；Store 没有结果（副作用不是本 pass 管的）。
                    SsaInstr::Call { .. }
                    | SsaInstr::Var { .. }
                    | SsaInstr::Load { .. }
                    | SsaInstr::SlotAddr { .. }
                    | SsaInstr::Store { .. } => {
                        if let Some(r) = instr.result() {
                            if set_lattice(&mut lattice, r, Lattice::Top) {
                                changed = true;
                            }
                        }
                    }
                }
            }

            // 终结符：恒定的条件决定哪个后继可达。
            let mut succs = match &block.term {
                crate::ssa::SsaTerminator::Goto(t) => vec![*t],
                crate::ssa::SsaTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                } => {
                    let taken = match lookup(&lattice, *cond) {
                        Lattice::Const(c) => match c.as_int() {
                            Some(v) => {
                                if v != 0 {
                                    vec![*then_block]
                                } else {
                                    vec![*else_block]
                                }
                            }
                            None => vec![*then_block, *else_block],
                        },
                        // 未知：两个后继都可达。
                        _ => vec![*then_block, *else_block],
                    };
                    taken
                }
                _ => Vec::new(),
            };
            succs.retain(|s| index_of.contains_key(s));
            for s in succs {
                if let Some(&si) = index_of.get(&s) {
                    if !reachable[si] {
                        reachable[si] = true;
                        changed = true;
                    }
                }
            }
        }
    }

    // 把常量写回指令流。
    //
    // 先把"每个块里哪些指令可以折成常量"算成一张表，再改写：直接边遍历
    // `func.blocks[bi].instrs` 边读 `func.blocks[bi].id` 会撞上借用检查
    // （同一个 `bi` 既被可变借用又被不可变借用）。
    let block_ids: Vec<usize> = func.blocks.iter().map(|b| b.id).collect();
    let mut rewrite: Vec<Vec<(usize, SsaConst)>> = vec![Vec::new(); n];
    for bi in 0..n {
        if !reachable[bi] {
            continue;
        }
        for (ii, instr) in func.blocks[bi].instrs.iter().enumerate() {
            let Some(result) = instr.result() else {
                continue;
            };
            let Some(Lattice::Const(value)) = lattice.get(&result).cloned() else {
                continue;
            };
            // φ 是合并节点：只有在本块这条入边上恒定时才折成常量。
            let ok = match instr {
                SsaInstr::Phi { incoming, .. } => incoming
                    .iter()
                    .find(|(p, _)| *p == block_ids[bi])
                    .is_some_and(|(_, v)| matches!(lattice.get(v), Some(Lattice::Const(_)))),
                _ => true,
            };
            if ok {
                rewrite[bi].push((ii, value));
            }
        }
    }
    for bi in 0..n {
        for (ii, value) in std::mem::take(&mut rewrite[bi]) {
            let instr = &mut func.blocks[bi].instrs[ii];
            let result = instr.result().expect("写回表里的指令一定有结果");
            *instr = SsaInstr::Const { result, value };
        }
    }

    // 分支条件恒定 → 换成无条件跳转。
    for bi in 0..n {
        if !reachable[bi] {
            continue;
        }
        let cond = match &func.blocks[bi].term {
            crate::ssa::SsaTerminator::Branch { cond, .. } => *cond,
            _ => continue,
        };
        if let Some(Lattice::Const(c)) = lattice.get(&cond) {
            if let Some(v) = c.as_int() {
                let target = match &func.blocks[bi].term {
                    crate::ssa::SsaTerminator::Branch {
                        then_block,
                        else_block,
                        ..
                    } => {
                        if v != 0 {
                            *then_block
                        } else {
                            *else_block
                        }
                    }
                    _ => unreachable!(),
                };
                func.blocks[bi].term = crate::ssa::SsaTerminator::Goto(target);
            }
        }
    }

    // 只重写常量，**不删块**：删块统一交给 [`eliminate_dead_blocks`]。
    //
    // 在这里删会留下悬空引用——别的块可能还跳向被删掉的块号，而它们自己
    // 的可达性是在下一轮才判定的。后端拿块号查符号表，查不到就生成一条指向
    // 未定义符号的跳转，问题被推迟到链接期才爆出来，很难定位。
    let _ = reachable;
}

/// 取格：未定义的值一律当 ⊤（保守）。
fn lookup(lattice: &HashMap<SsaValue, Lattice>, v: SsaValue) -> Lattice {
    lattice.get(&v).cloned().unwrap_or(Lattice::Top)
}

/// 合并 φ 各入边的取值：只有全未定义才未定义，只要有一个 ⊤ 就是 ⊤。
fn merge_lattice(
    lattice: &HashMap<SsaValue, Lattice>,
    phi: SsaValue,
    incoming: Lattice,
) -> Lattice {
    let _ = (lattice, phi);
    incoming
}

/// 写入格；变化时返回 `true`（驱动不动点）。
fn set_lattice(lattice: &mut HashMap<SsaValue, Lattice>, v: SsaValue, new: Lattice) -> bool {
    let old = lattice.get(&v);
    // 单调上升：值只会从"更确定"变成"更不确定"（其他 pass 已经折过的常量
    // 不该被这里降级），所以 `Const` 遇到 `Top` 时**不**算变化。
    let changed = match (old, &new) {
        (None, Lattice::Undef) => false,
        (Some(Lattice::Top), _) => false,
        (Some(Lattice::Const(a)), Lattice::Const(b)) => a != b,
        (Some(Lattice::Const(_)), _) => false,
        (Some(Lattice::Undef), Lattice::Undef) => false,
        (Some(Lattice::Undef), _) | (None, _) => true,
    };
    if changed {
        lattice.insert(v, new);
    }
    changed
}

/// 常量转换（供 SCCP 折叠 `Cast`）。
fn eval_cast(c: &SsaConst, from: crate::mir::NumTy, to: crate::mir::NumTy) -> Option<SsaConst> {
    use crate::mir::NumTy::{Float, Int, Uint};
    // 从常量取浮点值：f32 常量存的是 f32 位模式（零扩展到 64 位），
    // 必须先按 32 位还原，`f64::from_bits` 会把它读成完全不同的数。
    let as_f64 = |c: &SsaConst| -> Option<f64> {
        match c {
            SsaConst::Float(bits, 32) => Some(f32::from_bits(*bits as u32) as f64),
            SsaConst::Float(bits, _) => Some(f64::from_bits(*bits)),
            SsaConst::Int(v) => Some(*v as f64),
            SsaConst::Bool(b) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
    };
    // 按目标位宽打包一个浮点常量。
    let pack_float = |x: f64, w: u8| -> SsaConst {
        if w <= 32 {
            SsaConst::Float((x as f32).to_bits() as u64, 32)
        } else {
            SsaConst::Float(x.to_bits(), 64)
        }
    };
    match (from, to) {
        // 浮点 <-> 浮点：按目标位宽决定 f32 还是 f64
        (Float(_), Float(b)) => Some(pack_float(as_f64(c)?, b)),
        (Float(_), Int(bits)) | (Float(_), Uint(bits)) => {
            let raw = as_f64(c)? as i128;
            Some(SsaConst::Int(truncate_int(
                raw,
                u16::from(bits),
                matches!(to, Int(_)),
            )))
        }
        // 整数 -> 浮点
        (_, Float(b)) => Some(pack_float(as_f64(c)?, b)),
        (crate::mir::NumTy::Bool, _) => {
            let v = match c {
                SsaConst::Bool(b) => i128::from(*b),
                SsaConst::Int(v) => *v,
                _ => return None,
            };
            Some(SsaConst::Int(v))
        }
        (Int(_), Int(tb)) | (Int(_), Uint(tb)) | (Uint(_), Int(tb)) | (Uint(_), Uint(tb)) => {
            let v = match c {
                SsaConst::Int(v) => *v,
                SsaConst::Bool(b) => i128::from(*b),
                _ => return None,
            };
            // 截断到**目标**位宽。早先这里用的是源位宽，于是
            // `200i32 as i8` 折出来仍是 200，`y >= 0` 随之折成真——
            // 断言 4 分支永远走不到。
            let signed = matches!(to, Int(_));
            Some(SsaConst::Int(truncate_int(v, u16::from(tb), signed)))
        }
        _ => None,
    }
}

/// 按位宽截断并做符号 / 零扩展。
fn truncate_int(raw: i128, bits: u16, signed: bool) -> i128 {
    if bits == 0 || bits >= 128 {
        return raw;
    }
    let mask = (1i128 << bits) - 1;
    let low = raw & mask;
    if signed && low >> (bits - 1) & 1 == 1 {
        low - (1i128 << bits)
    } else {
        low
    }
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
                    ..
                } = instr
                else {
                    continue;
                };
                let (op, lhs, rhs) = (*op, *lhs, *rhs);
                let b = const_of(&defs, rhs);
                let repl: Option<Def> = match op {
                    // x + 0 / x - 0 / x | 0 / x ^ 0 -> x
                    SsaBinaryOp::Add
                    | SsaBinaryOp::Sub
                    | SsaBinaryOp::BitOr
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
                    SsaBinaryOp::BitAnd if b.as_ref().and_then(SsaConst::as_int) == Some(-1) => {
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
                        *instr = SsaInstr::Copy {
                            result,
                            src: refs[0],
                        };
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
    let mut candidates: HashMap<String, (Vec<SsaValue>, Vec<SsaInstr>, SsaValue)> = HashMap::new();
    for func in &module.functions {
        // `#[inline(never)]` 的函数不作为候选——属性语义在这里生效
        if func.inline_never {
            continue;
        }
        // `#[inline(always)]` 的函数即使多块也内联：把每个块展开到调用点
        if func.inline_always {
            // 多块函数内联：只内联单块情况，多块需要更复杂的控制流合并
            // 这里采用简化策略：只内联单块 always 函数，多块 always 在后续 pass 处理
            if func.blocks.len() == 1 {
                let block = &func.blocks[0];
                let SsaTerminator::Return(Some(ret)) = block.term else {
                    continue;
                };
                candidates.insert(
                    func.name.clone(),
                    (func.params.clone(), block.instrs.clone(), ret),
                );
            }
            continue;
        }
        if func.blocks.len() != 1 {
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
                    ..
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
        SsaInstr::Load { ptr, .. } => {
            if *ptr == from {
                *ptr = to;
            }
        }
        SsaInstr::Store { ptr, value, .. } => {
            if *ptr == from {
                *ptr = to;
            }
            if *value == from {
                *value = to;
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
        SsaInstr::Binary {
            op, lhs, rhs, ty, ..
        } => SsaInstr::Binary {
            result: new,
            op,
            lhs,
            rhs,
            ty,
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
            callee, args, ty, ..
        } => SsaInstr::Call {
            result: new,
            callee,
            args,
            ty,
        },
        SsaInstr::Load { ptr, ty, .. } => SsaInstr::Load {
            result: new,
            ptr,
            ty,
        },
        SsaInstr::SlotAddr { local, .. } => SsaInstr::SlotAddr { result: new, local },
        // Store 没有结果值；本函数只应被"有结果"的指令调用。原样重建即可。
        SsaInstr::Store { ptr, value, ty } => SsaInstr::Store { ptr, value, ty },
        // Asm 没有结果值；本函数只处理有结果的指令。原样重建即可。
        SsaInstr::Asm { .. } => instr,
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
                us.iter()
                    .all(|u| !inside.contains(u) || *u == instr.result().unwrap())
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

#[cfg(test)]
mod cast_trunc_tests {
    use super::*;

    #[test]
    fn narrowing_cast_truncates_and_sign_extends() {
        use crate::mir::NumTy;
        // 200 作为 i8 是 -56。
        let got = eval_cast(&SsaConst::Int(200), NumTy::Int(32), NumTy::Int(8));
        assert_eq!(got, Some(SsaConst::Int(-56)));
        // 同一位宽的转换只做符号扩展。
        assert_eq!(
            eval_cast(&SsaConst::Int(5), NumTy::Int(32), NumTy::Int(32)),
            Some(SsaConst::Int(5))
        );
        // 无符号窄化是零扩展：200 -> 200。
        assert_eq!(
            eval_cast(&SsaConst::Int(200), NumTy::Int(32), NumTy::Uint(8)),
            Some(SsaConst::Int(200))
        );
    }

    #[test]
    fn comparison_of_truncated_value_folds_correctly() {
        // -56 >= 0 必须是假 —— 这条断言守着"窄化后再比较"的折叠顺序。
        assert_eq!(
            eval_binary(SsaBinaryOp::Ge, &SsaConst::Int(-56), &SsaConst::Int(0)),
            Some(SsaConst::Int(0))
        );
    }
}
