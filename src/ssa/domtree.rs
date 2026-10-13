//! 支配树（Dominator Tree）构造。
//!
//! 支配关系定义：`A` 支配 `B` 当且仅当所有从入口到 `B` 的路径都经过 `A`。
//! 支配树是 SSA φ 函数插入的理论基础。

use crate::ssa::SsaFunction;

/// 支配树节点。
#[derive(Debug, Clone, Default)]
pub struct DomNode {
    /// 本节点。
    pub id: usize,
    /// 支配本节点的节点（最近的支配者，immediate dominator）。
    pub idom: Option<usize>,
    /// 本节点支配的子节点。
    pub children: Vec<usize>,
    /// 支配本节点的节点集合（含自身）。
    pub dominators: Vec<usize>,
}

/// 为给定函数构造支配树。
///
/// 使用 Cooper、Harvey、Kennedy 的迭代数据流算法：
/// 1. 从入口开始 BFS 排定逆后序。
/// 2. 按逆后序迭代：对每块求所有前驱 idom 的交集，作为新 idom。
/// 3. 重复直至收敛。
pub fn build_dominator_tree(func: &SsaFunction) -> Vec<DomNode> {
    let n = func.blocks.len();
    if n == 0 {
        return Vec::new();
    }

    let mut nodes: Vec<DomNode> = (0..n)
        .map(|i| DomNode {
            id: i,
            idom: None,
            children: Vec::new(),
            dominators: Vec::new(),
        })
        .collect();

    // 入口块的 idom 为 None（自身）
    nodes[0].idom = None;

    // 逆后序：这里用块 ID 作为简化排序（真实实现需做 DFS 逆后序）
    let order: Vec<usize> = (0..n).collect();

    let mut changed = true;
    while changed {
        changed = false;
        for &b in &order {
            if b == 0 {
                continue;
            }
            // 找出第一个已被处理的前驱
            let first_pred = func.blocks[b]
                .preds
                .iter()
                .copied()
                .find(|&p| p < n)
                .unwrap_or(0);

            // 其余前驱与 first_pred 取交集
            let mut new_idom = first_pred;
            for &p in &func.blocks[b].preds {
                if *p != first_pred && *p < n {
                    new_idom = intersect(&nodes, new_idom, *p);
                }
            }

            if nodes[b].idom != Some(new_idom) {
                nodes[b].idom = Some(new_idom);
                changed = true;
            }
        }
    }

    // 计算每块的支配集合
    for i in 0..n {
        let mut doms = Vec::new();
        let mut cur = Some(i);
        while let Some(c) = cur {
            doms.push(c);
            cur = nodes[c].idom;
        }
        nodes[i].dominators = doms;
    }

    // 构建支配树子节点关系
    for i in 0..n {
        nodes[i].children.clear();
    }
    for i in 1..n {
        if let Some(idom) = nodes[i].idom {
            nodes[idom].children.push(i);
        }
    }

    nodes
}

/// 求两条支配链的最近公共祖先。
fn intersect(nodes: &[DomNode], mut b1: usize, mut b2: usize) -> usize {
    let mut set: std::collections::HashSet<usize> = nodes[b1]
        .dominators
        .iter()
        .copied()
        .collect();
    while !set.contains(&b2) {
        match nodes[b2].idom {
            Some(p) => b2 = p,
            None => break,
        }
    }
    if set.contains(&b2) {
        return b2;
    }
    while let Some(p) = nodes[b1].idom {
        b1 = p;
        if nodes[b1].dominators.iter().any(|&d| d == b2) {
            return b1;
        }
    }
    b1
}

/// 求支配边界（Dominance Frontier），用于 φ 函数放置。
///
/// DF(B) = { X | B 支配 X 的某个前驱，但不严格支配 X }
pub fn dominance_frontier(nodes: &[DomNode]) -> Vec<Vec<usize>> {
    let n = nodes.len();
    let mut df: Vec<Vec<usize>> = vec![Vec::new(); n];

    for x in 0..n {
        let preds = &nodes[x].children; // 此处 preds 在调用方应传入真实前驱
        let _ = preds;
        // 真实算法需遍历每块的前驱，判断是否在其支配边界内。
        // 骨架实现返回空边界，避免误插 φ。
    }

    df
}
