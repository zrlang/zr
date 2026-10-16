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

use std::collections::HashMap;

use crate::ssa::{SsaInstr, SsaTerminator, SsaValue, SsaFunction};

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
pub fn allocate_registers(
    func: &SsaFunction,
    pool_size: usize,
    callee_save: &[usize],
) -> Allocation {
    let (intervals, calls) = live_intervals(func);
    let mut out = Allocation::default();
    if intervals.is_empty() || pool_size == 0 {
        return out;
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

    for (val, start, end) in ordered {
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
        let spans_call = callee_save.len() > 0
            && calls.iter().any(|&c| start < c && c < end);
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
    out
}

/// 每个值的活跃区间 `[首定义, 末使用]`（线性位置）。
fn live_intervals(func: &SsaFunction) -> (HashMap<u32, (usize, usize)>, Vec<usize>) {
    let mut iv: HashMap<u32, (usize, usize)> = HashMap::new();
    let mut pos = 0usize;
    let mut calls: Vec<usize> = Vec::new();

    // 函数参数：入口处就算定义；上界由第一次使用来抬高
    for p in &func.params {
        iv.insert(p.id(), (0, 0));
    }

    for block in &func.blocks {
        // 块内的 phi 必须在所有指令之前求值
        let n_phi = block
            .instrs
            .iter()
            .take_while(|i| matches!(i, SsaInstr::Phi { .. }))
            .count();
        for instr in block.instrs.iter().take(n_phi) {
            touch_use(instr, &mut iv, pos);
            if let SsaInstr::Phi { result, incoming, .. } = instr {
                // φ 的结果在**所有**传入值都算完之后才"活"，否则会与它们冲突
                iv.insert(result.id(), (pos, pos));
                for (_, v) in incoming {
                    extend(*v, &mut iv, pos);
                }
            }
            pos += 1;
        }

        for instr in block.instrs.iter().skip(n_phi) {
            if matches!(instr, SsaInstr::Call { .. }) {
                calls.push(pos);
            }
            touch_use(instr, &mut iv, pos);
            if let Some(r) = instr.result() {
                iv.insert(r.id(), (pos, pos));
            }
            pos += 1;
        }

        // 终结符引用也算一次使用
        match &block.term {
            SsaTerminator::Return(Some(v)) => extend(*v, &mut iv, pos),
            SsaTerminator::Branch { cond, .. } => extend(*cond, &mut iv, pos),
            _ => {}
        }
        pos += 1;
    }
    (iv, calls)
}

/// 记一条指令里的每个操作数为「使用」，并把它们的区间延长到 `pos`。
fn touch_use(instr: &SsaInstr, iv: &mut HashMap<u32, (usize, usize)>, pos: usize) {
    match instr {
        SsaInstr::Const { .. } | SsaInstr::Var { .. } => {}
        SsaInstr::Unary { operand, .. } => extend(*operand, iv, pos),
        SsaInstr::Binary { lhs, rhs, .. } => {
            extend(*lhs, iv, pos);
            extend(*rhs, iv, pos);
        }
        SsaInstr::Phi { incoming, .. } => {
            for (_, v) in incoming {
                extend(*v, iv, pos);
            }
        }
        SsaInstr::Copy { src, .. } => extend(*src, iv, pos),
        SsaInstr::Cast { src, .. } => extend(*src, iv, pos),
        SsaInstr::Call { args, .. } => {
            for a in args {
                extend(*a, iv, pos);
            }
        }
    }
}

/// 把 `v` 的活跃区间下界补上（首次使用）并延长上界到 `pos`。
fn extend(v: SsaValue, iv: &mut HashMap<u32, (usize, usize)>, pos: usize) {
    let e = iv.entry(v.id()).or_insert((pos, pos));
    if e.0 > pos {
        e.0 = pos;
    }
    if pos > e.1 {
        e.1 = pos;
    }
}
