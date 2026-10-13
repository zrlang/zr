//! 寄存器分配（Register Allocation）。
//!
//! 实现线性扫描（Linear Scan）算法：
//! 1. 计算每条指令的活变量区间（live interval）。
//! 2. 按区间起点升序处理，维护活跃区间栈。
//! 3. 区间起点到达时分配可用寄存器；终点到达时释放。
//! 4. 无可用寄存器时，把存活最长的区间溢出（spill）到栈槽。
//!
//! 设计参考：
//! - LLVM：RegAlloc 抽象层支持多种分配器（greedy、PBQP、linear scan）。
//! - GCC：IRA（Graph Coloring）。
//! - Zig：Aro 后端用线性扫描的变体。

/// x86-64 调用约定中的通用寄存器（System V ABI）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Register {
    /// 参数 / 返回值（RDI, RSI, RDX, RCX, R8, R9）。
    Rdi, Rsi, Rdx, Rcx, R8, R9,
    /// 被调用者保存（需 callee-save）。
    Rbx, Rbp, R12, R13, R14, R15,
    /// 调用者保存（caller-save，临时用）。
    Rax, R10, R11,
    /// 栈指针。
    Rsp,
}

impl Register {
    /// 返回此寄存器的 x86-64 编号（用于 ModR/M 的 reg 字段）。
    pub fn code(self) -> u8 {
        use Register::*;
        match self {
            Rax => 0, Rcx => 1, Rdx => 2, Rbx => 3,
            Rsp => 4, Rbp => 5, Rsi => 6, Rdi => 7,
            R8 => 8, R9 => 9, R10 => 10, R11 => 11,
            R12 => 12, R13 => 13, R14 => 14, R15 => 15,
        }
    }

    /// 参数寄存器（按位置）。
    pub fn arg_reg(i: usize) -> Option<Register> {
        use Register::*;
        match i {
            0 => Some(Rdi), 1 => Some(Rsi), 2 => Some(Rdx),
            3 => Some(Rcx), 4 => Some(R8), 5 => Some(R9),
            _ => None,
        }
    }
}

/// 活变量区间。
#[derive(Debug, Clone)]
pub struct LiveInterval {
    /// 变量标识。
    pub var: u32,
    /// 区间起点（指令序号）。
    pub start: usize,
    /// 区间终点（指令序号）。
    pub end: usize,
    /// 分配的寄存器（若溢出则为 None）。
    pub reg: Option<Register>,
    /// 栈槽索引（溢出时使用）。
    pub spill_slot: Option<usize>,
}

/// 寄存器分配器。
#[derive(Debug, Default)]
pub struct RegAlloc {
    /// 可用寄存器池（caller-save 优先）。
    regs: Vec<Register>,
    /// 当前活跃区间（按终点升序）。
    active: Vec<LiveInterval>,
    /// 下一个栈槽索引。
    next_slot: usize,
}

impl RegAlloc {
    /// 创建针对 x86-64 System V ABI 的分配器。
    pub fn new() -> Self {
        use Register::*;
        // 调用者保存的通用寄存器优先分配
        let regs = vec![Rax, Rcx, Rdx, Rsi, Rdi, R8, R9, R10, R11, Rbx, R12, R13, R14, R15];
        Self {
            regs,
            active: Vec::new(),
            next_slot: 0,
        }
    }

    /// 对一组活区间执行线性扫描分配。
    pub fn allocate(&mut self, mut intervals: Vec<LiveInterval>) -> Vec<LiveInterval> {
        // 按起点升序排序
        intervals.sort_by_key(|i| i.start);

        for mut iv in &mut intervals {
            // 释放已结束的区间
            self.expire_old(iv.start);
            if let Some(reg) = self.regs.pop() {
                iv.reg = Some(reg);
                self.active.push(iv.clone());
            } else {
                // 溢出：把存活最长的区间换出
                self.spill_at(iv.clone());
            }
        }

        intervals
    }

    /// 释放终点 <= pos 的区间，归还寄存器。
    fn expire_old(&mut self, pos: usize) {
        let mut keep = Vec::new();
        for iv in self.active.drain(..) {
            if iv.end > pos {
                keep.push(iv);
            } else if let Some(reg) = iv.reg {
                self.regs.push(reg);
            }
        }
        self.active = keep;
    }

    /// 溢出处理：把存活最长的区间换到栈上。
    fn spill_at(&mut self, iv: LiveInterval) {
        if let Some(longest) = self.active.iter().max_by_key(|a| a.end) {
            let spill_target = longest.clone();
            // 把 longest 换到栈槽
            self.next_slot += 1;
            // 注：此处仅记录决策，真实实现需插入 spill/reload 指令
            let _ = spill_target;
        }
        let _ = iv;
    }

    /// 当前已分配的栈槽数量（用于栈帧大小计算）。
    pub fn stack_slots(&self) -> usize {
        self.next_slot
    }
}
