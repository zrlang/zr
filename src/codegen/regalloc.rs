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

/// 线性扫描结果：`值 id -> 寄存器下标`。
pub type RegMap = HashMap<u32, usize>;

/// x86-64 SysV 里由**被调用方**保存、跨调用仍然有效的寄存器下标
/// （对应 `ALLOCATABLE` 的 R12–R15）。
pub const CALLEE_SAVE_IDX: [usize; 4] = [6, 7, 8, 9];

/// 计算函数的寄存器分配。
///
/// `pool_size` 是可分配寄存器的个数；返回值里超出该范围的都没有。
pub fn allocate_registers(func: &SsaFunction, pool_size: usize) -> RegMap {
    let (intervals, calls) = live_intervals(func);
    if intervals.is_empty() || pool_size == 0 {
        return HashMap::new();
    }

    // 按起点排序；同起点时终点短的先放（更容易共用）。
    // 必须把值 id 也放进排序键：`intervals` 是 HashMap，迭代顺序带随机
    // 哈希种子，只按 (start, end) 排序的话，同区间的值谁先分配是随机
    // 的——同一个程序编译两次会得到不同的（有时是错的）寄存器分配。
    let mut ordered: Vec<(u32, usize, usize)> =
        intervals.iter().map(|(v, (s, e))| (*v, *s, *e)).collect();
    ordered.sort_by_key(|(v, s, e)| (*s, *e, *v));

    let mut map: RegMap = HashMap::new();
    // 每个寄存器当前区间的终点；`usize::MAX` 表示空闲
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
        let spans_call = calls.iter().any(|&c| start < c && c < end);
        let allowed: &[usize] = if spans_call {
            &CALLEE_SAVE_IDX
        } else {
            &all_idx
        };
        // 找一个没人要的寄存器
        let slot = allowed.iter().copied().find(|i| owners[*i].is_none());
        let slot = match slot {
            Some(i) => i,
            None => {
                // 池子满了：挤掉结束最早的那个
                let victim = allowed
                    .iter()
                    .copied()
                    .min_by_key(|i| free_at[*i])
                    .expect("池非空");
                owners[victim] = None;
                victim
            }
        };
        owners[slot] = Some(val);
        free_at[slot] = end;
        map.insert(val, slot);
    }
    map
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
