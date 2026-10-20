//! 寄存器分配：活跃区间 + 线性扫描。
//!
//! 之前 `alloc_reg` 直接用 `id % 池大小` 取模——两个**同时存活**的值
//! 只要编号同余就会抢同一个寄存器，后定义的把先定义的值冲掉。优化 pass
//! 增大了编号，隐藏了几百行的这种冲突，于是 `-O2` 生成的代码是错的。
//!
//! 这里按真正��活跃区间来分配：
//!
//! 1. 把函数的块按线性顺序排开，指令按序编号，得到每个值的
//!    `[起点, 终点]` 活跃区间（φ 的每个传入值单独算一条）；
//! 2. 按起点排序做线性扫描：与当前区间**相交**的旧区间占着寄存器，
//!    只回收已经结束的；
//! 3. 池子满了就把结束最早的区间挤出（溢出）。溢出值没有栈槽，
//!    只能退化成与被挤出的值共用寄存器——这仍然比取模安全得多，
//!    因为交叠只发生在真正的溢出点。

use std::collections::{HashMap, HashSet};

use crate::ssa::{SsaBinaryOp, SsaFunction, SsaInstr, SsaTerminator, SsaValue};
use Location as L;

/// 一个值的存放位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// 寄存器池下标。
    Reg(usize),
    /// 栈槽（每个槽 8 字节，相对 `rsp`）。
    Stack(usize),
}

/// 线性扫描结果：`值 id -> 位置`。
pub type RegMap = HashMap<u32, Location>;

/// 分配结果：值位置 + 需要预留的栈槽数。
#[derive(Debug, Default)]
pub struct Allocation {
    pub map: RegMap,
    /// 溢出到内存的值个数（也就是栈槽数）。
    pub spilled: usize,
}

/// x86-64 SysV 里由**被调用方**保存、跨调用仍然有效的寄存器下标
/// （对应 `ALLOCATABLE` 的 R12–R15）。
///
/// 这是**目标相关**的：别的 ABI 的 callee-save 集合不一样，寄存器数量也
/// 可能比这少。调用方必须按目标传进来，不能在这里写死。
pub const X86_64_CALLEE_SAVE: [usize; 4] = [6, 7, 8, 9];

/// 计算函数的寄存器分配。
///
/// 池子用完时**不再抢占**别人的寄存器，而是把当前值溢出到栈槽。早先的
/// 做法是"挤掉结束最早的占用者"——可那个值往往**还活着**，寄存器被复用
/// 之后它的内容就没了，生成的程序会安静地算错。这里宁可溢出，也不产出
/// 错代码：调用方负责在溢出发生时报错（见 `emit_function`）。
///
/// 扫出函数里所有多字长（i64）值的 id 集合。
///
/// zr 的 SSA 把整数统一建模为 i64，但 32 位目标上一次只能运算 32 位——
/// 宽度不够的值必须走"双 32 位"路径（栈槽 + 两条指令）。识别哪些值
/// 真的是 64 位：常量超 i32 范围的、64 位类型的运算/转换结果、以及经
/// `Copy`/`Phi` 传播过去的。
///
/// 传播到不动点：`Unary`/`Copy` 的宽度要等对端确定，`Phi` 取各 incoming
/// 的最大宽度。
pub fn assign_int_widths(func: &SsaFunction) -> HashSet<u32> {
    let mut wide: HashSet<u32> = HashSet::new();
    let is_wide_ty = |ty: &crate::mir::NumTy| -> bool {
        matches!(ty, crate::mir::NumTy::Int(64) | crate::mir::NumTy::Uint(64))
    };
    loop {
        let mut changed = false;
        let mark = |v: &SsaValue, set: &mut HashSet<u32>| -> bool { set.insert(v.id()) };
        for block in &func.blocks {
            for instr in &block.instrs {
                match instr {
                    SsaInstr::Const {
                        result,
                        value: crate::ssa::SsaConst::Int(i),
                    } => {
                        // 常量超 i32 范围 → i64
                        if (*i < i32::MIN as i128 || *i > i32::MAX as i128)
                            && mark(result, &mut wide)
                        {
                            changed = true;
                        }
                    }
                    SsaInstr::Binary {
                        result,
                        ty,
                        lhs,
                        rhs,
                        op,
                    } => {
                        if is_wide_ty(ty) {
                            for v in [result, lhs, rhs] {
                                if mark(v, &mut wide) {
                                    changed = true;
                                }
                            }
                        }
                        // 比较运算（Eq/Ne/Lt/Gt/Le/Ge）结果是 Bool，但若任一操作数是
                        // i64，另一侧也必须是 i64——比较要求两侧位宽一致。
                        if matches!(
                            op,
                            SsaBinaryOp::Eq
                                | SsaBinaryOp::Ne
                                | SsaBinaryOp::Lt
                                | SsaBinaryOp::Gt
                                | SsaBinaryOp::Le
                                | SsaBinaryOp::Ge
                        ) {
                            let lhs_wide = wide.contains(&lhs.id());
                            let rhs_wide = wide.contains(&rhs.id());
                            if lhs_wide && mark(rhs, &mut wide) {
                                changed = true;
                            }
                            if rhs_wide && mark(lhs, &mut wide) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Unary {
                        result, operand, ..
                    } => {
                        // 一元运算：result 和 operand 同宽
                        if wide.contains(&result.id()) && mark(operand, &mut wide) {
                            changed = true;
                        }
                        if wide.contains(&operand.id()) && mark(result, &mut wide) {
                            changed = true;
                        }
                    }
                    SsaInstr::Call { result, ty, .. }
                    | SsaInstr::CallIndirect { result, ty, .. } => {
                        if is_wide_ty(ty) && mark(result, &mut wide) {
                            changed = true;
                        }
                    }
                    SsaInstr::Cast {
                        result,
                        src,
                        from,
                        to,
                    } => {
                        if is_wide_ty(from) && mark(src, &mut wide) {
                            changed = true;
                        }
                        if is_wide_ty(to) && mark(result, &mut wide) {
                            changed = true;
                        }
                    }
                    SsaInstr::Copy { result, src } => {
                        if wide.contains(&result.id()) && mark(src, &mut wide) {
                            changed = true;
                        }
                        if wide.contains(&src.id()) && mark(result, &mut wide) {
                            changed = true;
                        }
                    }
                    SsaInstr::Phi {
                        result, incoming, ..
                    } => {
                        if incoming.iter().any(|(_, v)| wide.contains(&v.id()))
                            && mark(result, &mut wide)
                        {
                            changed = true;
                        }
                    }
                    SsaInstr::Load { result, ty, .. } => {
                        if is_wide_ty(ty) && mark(result, &mut wide) {
                            changed = true;
                        }
                    }
                    SsaInstr::Store { value, ty, .. }
                        if is_wide_ty(ty) && mark(value, &mut wide) =>
                    {
                        changed = true;
                    }
                    _ => {}
                }
            }
        }
        if !changed {
            break;
        }
    }
    wide
}

/// `pin_params` 是要钉进池前 `pin_params` 个下标的形参个数（= 实参寄存器
/// 个数，且要求目标后端的池下标与实参寄存器一一对应——x86-64 的池以
/// rsi/rdi 开头、顺序与实参寄存器不一致，传 0 走老路径；thumb 同理）。
///
/// `wide` 是多字长（i64）值的 id 集合（32 位目标）：这些值不占寄存器池，
/// 直接走 8 字节栈槽，后端用两条 32 位指令实现 i64 运算。64 位目标传
/// 空集（i64 就是机器字长）。
pub fn allocate_registers(
    func: &SsaFunction,
    pool_size: usize,
    callee_save: &[usize],
    pin_params: usize,
    wide: &HashSet<u32>,
) -> Allocation {
    let (intervals, calls) = live_intervals(func);
    let mut out = Allocation::default();
    if intervals.is_empty() || pool_size == 0 {
        return out;
    }

    // —— 形参钉子 ——
    //
    // ABI 保证第 i 个整型实参已经在实参寄存器 i 里。把形参钉在池下标
    // `0..pin_params` 上（池下标与实参寄存器一一对应），caller 与 callee
    // 的形参搬运就都不需要任何 mov——此前分配按 `(start,end,val)` 排序，
    // 短命值（如 `add(x,y)` 调用点临时造的中间值）会先抢走池首的实参寄存器，
    // 形参被挤到别的寄存器，产生一堆来回搬的 mov。
    //
    // 例外：实地越过某次 `call` 的形参**不能**钉住——实参寄存器全是
    // caller-save，`call` 的被调用方会覆盖它们。这样的形参让常规分配
    // 走 callee-save / 溢出。多字长（i64）形参同样不钉：它占两个实参
    // 寄存器、以 8 字节栈槽保存，由后端在序言里搬进槽。
    let mut pinned: HashSet<u32> = HashSet::new();
    let mut gi = 0usize;
    for &p in &func.params {
        if gi >= pin_params || gi >= pool_size {
            break;
        }
        let pv = p.id();
        if wide.contains(&pv) {
            gi += 2;
            continue;
        }
        let Some(&(start, end)) = intervals.get(&pv) else {
            gi += 1;
            continue; // 没用到的形参没有活跃区间，钉不钉都行
        };
        let spans_call = !callee_save.is_empty() && calls.iter().any(|&c| start < c && c < end);
        if spans_call {
            gi += 1;
            continue;
        }
        out.map.insert(pv, L::Reg(gi));
        pinned.insert(pv);
        gi += 1;
    }

    // 按起点排序；同起点时终点短的先放（更容易共用）。
    // 必须把值 id 也放进排序键：`intervals` 是 HashMap，迭代顺序带随机
    // 哈希种子，只按 (start, end) 排序的话，同区间的值谁先分配是随机
    // 的——同一个程序编译两次会得到不同的（有时是错的）寄存器分配。
    let mut ordered: Vec<(u32, usize, usize)> =
        intervals.iter().map(|(v, (s, e))| (*v, *s, *e)).collect();
    ordered.sort_by_key(|(v, s, e)| (*s, *e, *v));

    // 每个寄存器当前区间的终点
    let mut free_at: Vec<usize> = vec![0; pool_size];
    let mut owners: Vec<Option<u32>> = vec![None; pool_size];
    let all_idx: Vec<usize> = (0..pool_size).collect();

    // 被钉住的形参先占住实参寄存器：常规分配看到的是"还活着的占用者"，
    // 不会把别的值塞进来；形参自己被 `pinned` 跳过。
    for (&v, loc) in &out.map {
        let L::Reg(i) = loc else { continue };
        if let Some(&(_, e)) = intervals.get(&v) {
            owners[*i] = Some(v);
            free_at[*i] = e;
        }
    }

    for (val, start, end) in &ordered {
        let (val, start, end) = (*val, *start, *end);
        if pinned.contains(&val) {
            continue;
        }
        // 多字长（i64）值：不占寄存器池，直接走 8 字节栈槽。
        // 后端 emit 时从栈槽装载低/高两个 32 位半做运算。
        if wide.contains(&val) {
            out.map.insert(val, Location::Stack(out.spilled));
            out.spilled += 1;
            continue;
        }
        // 回收所有已经结束的区间
        for i in 0..pool_size {
            if owners[i].is_some() && free_at[i] < start {
                owners[i] = None;
            }
        }
        // 跨调用存活的值只能放 callee-save 寄存器：其余（GPR、rax、rdx…）
        // 都是 caller-save，`call` 返回时会被被调用方覆盖掉。
        //
        // 判据是"调用之后还有没有用"（`start < c < end`）。把区间右端
        // 正好等于调用位置的值也算进来是错的：那只是 `call` 的实参，
        // `call` 自己会读它，返回后不需要它 —— 而这样会把大量普通值挤进
        // 只有 4 个的 callee-save 集合，逼出本来不会发生的冲突。
        let spans_call = !callee_save.is_empty() && calls.iter().any(|&c| start < c && c < end);
        let allowed: &[usize] = if spans_call { callee_save } else { &all_idx };
        match allowed.iter().copied().find(|i| owners[*i].is_none()) {
            Some(i) => {
                owners[i] = Some(val);
                free_at[i] = end;
                out.map.insert(val, Location::Reg(i));
            }
            None => {
                // 池子满了：溢出到栈槽。**不能**去抢一个还活着的值的寄存器。
                out.map.insert(val, Location::Stack(out.spilled));
                out.spilled += 1;
            }
        }
    }
    if std::env::var("ZR_DEBUG_ALLOC").is_ok() {
        eprintln!(
            "== allocation for {} (pool {pool_size}, spilled {}) ==",
            func.name, out.spilled
        );
        for (v, s, e) in &ordered {
            let loc = match out.map.get(v) {
                Some(L::Reg(i)) => format!("r{i}"),
                Some(L::Stack(slot)) => format!("stack[{slot}]"),
                None => "??".to_string(),
            };
            eprintln!("  v{v} = ({s},{e}) -> {loc}");
        }
    }
    out
}

/// 每个值的活跃区间 `[首定义, 末使用]`（线性位置）。
///
/// 这一版走**真正的 CFG 活跃性分析**，而不是"沿块下标顺序扫一遍"。差别不是
/// 风格问题，是正确性问题：
///
/// 块下标顺序不满足"前驱排在后继之前"。`while c { ... break; }` 展开出来是
///
/// ```text
/// bb1: 头（含 φ）      ← v0 = 累加器在这里定义
/// bb2: 循环体
/// bb3: 出口检查        ← v0 在这里被读
/// bb4: break 的落点    ← 块号 4 > 3，排在读之后
/// ```
///
/// `break` 从 bb2 跳到 bb4，bb4 又跳回 bb3 读 v0。于是一条真实存在的路径是
/// bb2 → bb4 → bb3：v0 必须从 bb2 一直活到 bb3 的那次读。扫块下标时 bb4 排在
/// 那次读**之后**，v0 的区间就在进入 bb4 之前就结束了，中途的寄存器被复用，
/// 读到的不是累加器的值。
///
/// 支配树那套"回边延长"救不了这种形状：bb4 → bb3 是**回边**（DFS 里 bb3 先被
/// 访问），但它不是循环的回边——`dominates(idom, bb3, bb4)` 成立，于是延长逻辑
/// 把它当成普通边处理，而普通边不需要延长（读在前、写在后，顺序恰好自洽）。
///
/// 正确做法是标准的反向活跃性不动点：
///
/// ```text
/// live_out[B] = ⋃_{S∈succ(B)} (live_in[S] − φ_results(S)) ∪ {φ 在 B 上的传入值}
/// live_in[B]  = use[B] ∪ (live_out[B] − def[B])
/// ```
///
/// 之后把区间按"块内有活跃"来撑开：值只要在某个块的入口活跃，就必须覆盖整个块。
fn sorted(s: &HashSet<u32>) -> Vec<u32> {
    let mut v: Vec<u32> = s.iter().copied().collect();
    v.sort_unstable();
    v
}

fn live_intervals(func: &SsaFunction) -> (HashMap<u32, (usize, usize)>, Vec<usize>) {
    let n = func.blocks.len();
    if n == 0 {
        return (HashMap::new(), Vec::new());
    }

    // ---- 线性位置：块下标 -> [起, 终) ----
    //
    // 位置只是**活跃区间端点的度量**，正确性完全由下面的 CFG 活跃性保证；
    // 块之间的先后顺序不影响结果。
    let mut block_range: Vec<(usize, usize)> = Vec::with_capacity(n);
    let mut pos = 0usize;
    for block in &func.blocks {
        let len = block.instrs.len() + 1; // +1 给终结符
        block_range.push((pos, pos + len));
        pos += len;
    }
    // 块号 -> 下标（MIR 降级保证块号连续，但别依赖这一点）。
    let index_of: HashMap<usize, usize> = func
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id, i))
        .collect();

    // ---- 前驱表 ----
    let succs: Vec<Vec<usize>> = func
        .blocks
        .iter()
        .map(|b| crate::ssa::successors_of_ssa_term(&b.term))
        .collect();
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ss) in succs.iter().enumerate() {
        for &s in ss {
            if let Some(&si) = index_of.get(&s) {
                preds[si].push(i);
            }
        }
    }

    // ---- 每块的 use / def ----
    //
    // φ 的传入值算**本块**的 use（它们在本块末尾被读），φ 的结果算**本块**的
    // def（它在本块开头生效）。
    let mut uses: Vec<HashSet<u32>> = vec![HashSet::new(); n];
    let mut defs: Vec<HashSet<u32>> = vec![HashSet::new(); n];
    //
    // `use[B]` 的定义是"**在本块内**先读后写"的值，必须**顺序**扫描才能算对。
    // 早先这里把所有操作数一股脑塞进 use，于是 `const v5 = 10; v6 = v1 < v5`
    // 里的 v5 也成了 use——它明明在本块内已经定义过。后果不是多占一点寄存器：
    // v5 ∈ use[B] ⇒ v5 ∈ live_in[B] ⇒ 一路经 `live_out[前驱]` 传回入口块，
    // 整条链上的值都被撑成"全程活跃"，活跃区间完全失去意义（实测每个中间值
    // 的区间都一直伸到函数末尾）。
    for (bi, block) in func.blocks.iter().enumerate() {
        let mut defined: HashSet<u32> = HashSet::new();
        let take_use = |v: SsaValue, defined: &HashSet<u32>, uses: &mut HashSet<u32>| {
            if !defined.contains(&v.id()) {
                uses.insert(v.id());
            }
        };
        for instr in &block.instrs {
            if matches!(instr, SsaInstr::Phi { .. }) {
                // φ 的**传入值**不是本块的 use——它们在各前驱块的末尾被读，
                // 由下面的 `phi_inputs` 单独记账。混进 `uses` 会让
                // `live_out[前驱]` 通过 `live_in[本块]` 把这些值一路传到入口块。
                if let Some(r) = instr.result() {
                    defined.insert(r.id());
                    defs[bi].insert(r.id());
                }
                continue;
            }
            let mut us = Vec::new();
            crate::ssa::instr_uses(instr, &mut us);
            for u in us {
                take_use(u, &defined, &mut uses[bi]);
            }
            if let Some(r) = instr.result() {
                defined.insert(r.id());
                defs[bi].insert(r.id());
            }
        }
        let mut tus = Vec::new();
        term_uses(&block.term, &mut tus);
        for v in tus {
            take_use(v, &defined, &mut uses[bi]);
        }
    }
    // 函数参数在入口定义。
    for p in &func.params {
        defs[0].insert(p.id());
    }

    // ---- 反向不动点 ----
    let mut live_in: Vec<HashSet<u32>> = vec![HashSet::new(); n];
    let mut live_out: Vec<HashSet<u32>> = vec![HashSet::new(); n];

    // 每个块作为前驱时，需要在末尾保持活跃的值：后继块 φ 的传入值。
    // （等价于把 φ 节点"下沉"到前驱末尾，活跃性才看得见。）
    let mut phi_inputs: Vec<HashSet<u32>> = vec![HashSet::new(); n];
    for bi in 0..n {
        for &s in &succs[bi] {
            let Some(&si) = index_of.get(&s) else {
                continue;
            };
            for instr in &func.blocks[si].instrs {
                let SsaInstr::Phi { incoming, .. } = instr else {
                    break; // φ 一定连续排在块首
                };
                for (p, v) in incoming {
                    // 只收"本块确实是这条前驱边"的传入值。incoming 记的是块号，
                    // 所以按块号比较而不是下标。
                    if func.blocks[bi].id == *p {
                        phi_inputs[bi].insert(v.id());
                    }
                }
            }
        }
    }

    loop {
        let mut changed = false;
        for bi in (0..n).rev() {
            // live_out[B] = ⋃_S (live_in[S] − φ结果(S)) ∪ φ在B上的传入值
            let mut out: HashSet<u32> = phi_inputs[bi].clone();
            for &s in &succs[bi] {
                let Some(&si) = index_of.get(&s) else {
                    continue;
                };
                for &v in &live_in[si] {
                    // 后继块 φ 的结果由 φ 自己算，不算"B 出口时必须活着"。
                    let defined_by_phi = func.blocks[si]
                        .instrs
                        .iter()
                        .take_while(|i| matches!(i, SsaInstr::Phi { .. }))
                        .any(|i| i.result().is_some_and(|r| r.id() == v));
                    if !defined_by_phi {
                        out.insert(v);
                    }
                }
            }
            // live_in[B] = use[B] ∪ (live_out[B] − def[B])
            let mut inn: HashSet<u32> = uses[bi].clone();
            for &v in &out {
                if !defs[bi].contains(&v) {
                    inn.insert(v);
                }
            }
            if out != live_out[bi] || inn != live_in[bi] {
                live_out[bi] = out;
                live_in[bi] = inn;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // ---- 汇总成区间 ----
    //
    // 端点取三处的最大值 / 最小值：
    // 1. 定义点（函数参数算 0）；
    // 2. 直接使用点；
    // 3. 活跃性分析给出的"必须撑满"的块。
    //
    // 第 3 项是关键：一个值只要在块 B 的入口活跃，就必须在整个 B 期间保持
    // 有效——包括 B 末尾那些 φ 拷贝。break 路径正是靠这一项被撑开的。
    let mut iv: HashMap<u32, (usize, usize)> = HashMap::new();
    let bump_end = |iv: &mut HashMap<u32, (usize, usize)>, v: u32, end: usize| {
        let e = iv.entry(v).or_insert((end, end));
        if e.1 < end {
            e.1 = end;
        }
        if e.0 > end {
            e.0 = end;
        }
    };

    for p in &func.params {
        iv.insert(p.id(), (0, 0));
    }

    let mut pos = 0usize;
    let mut calls: Vec<usize> = Vec::new();
    for (bi, block) in func.blocks.iter().enumerate() {
        let n_phi = block
            .instrs
            .iter()
            .take_while(|i| matches!(i, SsaInstr::Phi { .. }))
            .count();
        // φ 的结果在本块开头定义；传入值不在这里记账（见 `phi_inputs`）。
        for instr in block.instrs.iter().take(n_phi) {
            if let Some(r) = instr.result() {
                iv.insert(r.id(), (pos, pos));
            }
            pos += 1;
        }
        for instr in block.instrs.iter().skip(n_phi) {
            if matches!(instr, SsaInstr::Call { .. } | SsaInstr::CallIndirect { .. }) {
                calls.push(pos);
            }
            let mut us = Vec::new();
            crate::ssa::instr_uses(instr, &mut us);
            for u in us {
                bump_end(&mut iv, u.id(), pos);
            }
            if let Some(r) = instr.result() {
                iv.insert(r.id(), (pos, pos));
            }
            pos += 1;
        }
        // 终结符的使用
        let mut tus = Vec::new();
        term_uses(&block.term, &mut tus);
        for v in tus {
            bump_end(&mut iv, v.id(), pos);
        }
        pos += 1;

        // 活跃性撑开：入口活跃 → 覆盖整块；出口活跃 → 至少到块末。
        let (start, end) = block_range[bi];
        for &v in &live_in[bi] {
            if let Some(e) = iv.get_mut(&v) {
                if e.0 > start {
                    e.0 = start;
                }
                if e.1 < end {
                    e.1 = end;
                }
            } else {
                iv.insert(v, (start, end));
            }
        }
        for &v in &live_out[bi] {
            if let Some(e) = iv.get_mut(&v) {
                if e.1 < end {
                    e.1 = end;
                }
            } else {
                iv.insert(v, (end, end));
            }
        }
        // φ 传入值必须活到本块末尾（末尾要做并行拷贝）。
        for &v in &phi_inputs[bi] {
            bump_end(&mut iv, v, end);
        }
    }

    if std::env::var("ZR_DEBUG_BACKEDGE").is_ok() {
        eprintln!("== live intervals for {} ==", func.name);
        for bi in 0..n {
            let mut li: Vec<_> = live_in[bi].iter().copied().collect();
            let mut lo: Vec<_> = live_out[bi].iter().copied().collect();
            let mut pd: Vec<_> = phi_inputs[bi].iter().copied().collect();
            li.sort_unstable();
            lo.sort_unstable();
            pd.sort_unstable();
            eprintln!(
                "  bb{} {:?} use={:?} def={:?} in={:?} out={:?} phi_in={:?}",
                func.blocks[bi].id,
                block_range[bi],
                sorted(&uses[bi]),
                sorted(&defs[bi]),
                li,
                lo,
                pd
            );
        }
        let mut items: Vec<(u32, (usize, usize))> = iv.iter().map(|(k, v)| (*k, *v)).collect();
        items.sort();
        for (k, v) in items {
            eprintln!("  v{k} = {:?}", v);
        }
    }
    (iv, calls)
}

/// 终结符引用的值。
fn term_uses(term: &SsaTerminator, out: &mut Vec<SsaValue>) {
    match term {
        SsaTerminator::Return(Some(v)) => out.push(*v),
        SsaTerminator::Branch { cond, .. } => out.push(*cond),
        _ => {}
    }
}
