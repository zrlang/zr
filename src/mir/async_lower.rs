//! 受限异步降级，遵循 Rust 的 Future/poll 模型。
//!
//! `async fn f(args) -> T` 拆成：
//!
//! - `f`：只分配堆帧、保存实参、返回 future 帧指针，**不执行函数体**。
//! - `f__poll(frame) -> i64`：状态机。状态 0 从函数入口开始；子 future 返回
//!   Pending 时形成挂起点。返回 0 表示 Pending，1 表示 Ready，
//!   结果写在帧的 `RESULT_OFF`。
//!
//! `await f(args)` 在 MIR 阶段改写成 poll 循环：同步上下文忙轮询；异步上下文
//! 轮询一次，Pending 就把父状态机挂起，恢复后继续轮询同一个子 future。
//! 这与 Rust 的 `Future::poll` / `.await` 传播 Pending 的方式一致。
//!
//! 标量、数组槽以及被取地址的局部量都保存在堆帧中，因此可以跨挂起点存活；
//! 内联汇编也会正常参与状态机拆分。未被 await 且未转义的 future 在函数返回点
//! 释放；被 return、传参或写入内存的 future 视为所有权转义，由接收方释放。
//!
//! 挂起中的 future 被丢弃时，`f__drop` 会按帧状态递归释放仍然存活的子
//! future，再释放父帧。编译期还按活跃性在最后一次使用后提前释放局部
//! future（循环每轮都会回收）；函数返回点的清理作为兜底。尚未实现与 Rust
//! 完全一致的作用域结束 Drop 顺序；动态来源的普通函数 future（不同异步
//! 函数分支）仍需要函数指针/动态 poll 支持。

use std::collections::{HashMap, HashSet};

use super::{
    MirBasicBlock, MirBinaryOp, MirFunction, MirInstr, MirModule, MirParam, MirTerminator,
    MirValue, NumTy,
};

const STATE_OFF: i128 = 0;
const RESULT_OFF: i128 = 8;
/// 帧头里的动态 poll / drop 函数指针（由 future 构造器写入）。
const POLL_OFF: i128 = 16;
const DROP_OFF: i128 = 24;
/// 聚合返回值在帧里的存放位置（16 字节对齐）。标量仍使用 `RESULT_OFF`。
const RESULT_AGG_OFF: i128 = 32;
const NAMES_OFF: i128 = 32;

#[derive(Clone, Debug)]
enum FutureTarget {
    Static(String),
    Dynamic,
}

pub fn lower_module(module: &mut MirModule) {
    if !module.functions.iter().any(|f| f.is_async) {
        return;
    }
    let lowerable: HashSet<String> = module
        .functions
        .iter()
        .filter(|f| f.is_async && can_lower_async(f))
        .map(|f| f.name.clone())
        .collect();

    // 普通函数也可以返回 future（`-> impl Future`）。若它的所有返回路径
    // 都返回同一个异步函数的 future，就把该函数登记成 future 生产者，
    // 调用点即可像直接调用 async fn 一样 await/drop。
    let producers = compute_future_producers(&module.functions, &lowerable);
    let dynamic_futures: HashSet<String> = module
        .functions
        .iter()
        .filter(|f| !f.is_async && f.returns_future && !producers.contains_key(&f.name))
        .map(|f| f.name.clone())
        .collect();
    let uses_executor = module.functions.iter().any(|f| {
        f.blocks.iter().any(|b| {
            b.instrs
                .iter()
                .any(|i| matches!(i, MirInstr::Call(_, callee, _, _) if callee == "zr_spawn"))
        })
    });
    let needs_frame_metadata = !dynamic_futures.is_empty() || uses_executor;

    let mut out = Vec::with_capacity(module.functions.len() * 2);
    for mut func in module.functions.drain(..) {
        let is_lowered_async = lowerable.contains(&func.name);
        if func.is_async && !is_lowered_async {
            // 空块函数无法拆分状态机；它调用的可降级异步子函数仍按同步
            // busy-poll 改写。
            rewrite_awaits(
                &mut func,
                &producers,
                &dynamic_futures,
                false,
                uses_executor,
            );
            out.push(func);
            continue;
        }
        let is_async = func.is_async;
        let (future_callees, dynamic_callees) = rewrite_awaits(
            &mut func,
            &producers,
            &dynamic_futures,
            is_async,
            uses_executor,
        );
        if is_lowered_async {
            match lower_function(
                func.clone(),
                &future_callees,
                &dynamic_callees,
                needs_frame_metadata,
            ) {
                Some((mut wrapper, mut poll, drop)) => {
                    if wrapper.name == "main" {
                        // `async fn main`：构造函数改名，另生成同步入口来驱动
                        // future 直到 Ready，把结果作为进程退出码返回。
                        let future_name = "main__future".to_string();
                        rename_frame_metadata(&mut wrapper, "main", &future_name);
                        wrapper.name = future_name.clone();
                        poll.name = format!("{future_name}__poll");
                        let mut drop = drop;
                        drop.name = format!("{future_name}__drop");
                        let main = make_async_main(&future_name, &poll.name);
                        out.push(wrapper);
                        out.push(poll);
                        out.push(drop);
                        out.push(main);
                    } else {
                        out.push(wrapper);
                        out.push(poll);
                        out.push(drop);
                    }
                }
                None => out.push(func),
            }
        } else {
            out.push(func);
        }
    }
    if dynamic_futures
        .iter()
        .any(|name| out.iter().any(|f| f.name == *name))
    {
        out.push(dynamic_drop_helper());
    }
    module.functions = out;
}

/// 动态 future 的通用 drop 入口：空帧直接返回，否则取帧头 drop 指针。
fn dynamic_drop_helper() -> MirFunction {
    let frame = "arg_0".to_string();
    let cond = "__dyn_cond".to_string();
    let addr = "__dyn_addr".to_string();
    let fp = "__dyn_fp".to_string();
    let fcond = "__dyn_fcond".to_string();
    MirFunction {
        name: "__zr_dyn_future_drop".to_string(),
        params: vec![MirParam],
        param_types: vec![NumTy::Uint(64)],
        blocks: vec![
            MirBasicBlock {
                id: 0,
                instrs: vec![MirInstr::Binary(
                    cond.clone(),
                    MirValue::Var(frame.clone()),
                    MirBinaryOp::Ne,
                    MirValue::Int(0),
                    NumTy::Uint(64),
                )],
                term: Some(MirTerminator::Branch {
                    cond: MirValue::Var(cond),
                    then_block: 1,
                    else_block: 3,
                }),
            },
            MirBasicBlock {
                id: 1,
                instrs: vec![
                    MirInstr::Binary(
                        addr.clone(),
                        MirValue::Var(frame.clone()),
                        MirBinaryOp::Add,
                        MirValue::Int(DROP_OFF),
                        NumTy::Uint(64),
                    ),
                    MirInstr::Load(fp.clone(), MirValue::Var(addr), NumTy::Uint(64)),
                    MirInstr::Binary(
                        fcond.clone(),
                        MirValue::Var(fp.clone()),
                        MirBinaryOp::Ne,
                        MirValue::Int(0),
                        NumTy::Uint(64),
                    ),
                ],
                term: Some(MirTerminator::Branch {
                    cond: MirValue::Var(fcond),
                    then_block: 2,
                    else_block: 3,
                }),
            },
            MirBasicBlock {
                id: 2,
                instrs: vec![MirInstr::CallIndirect(
                    "__dyn_ret".to_string(),
                    MirValue::Var(fp),
                    vec![MirValue::Var(frame)],
                    NumTy::Uint(64),
                )],
                term: Some(MirTerminator::Goto(3)),
            },
            MirBasicBlock {
                id: 3,
                instrs: Vec::new(),
                term: Some(MirTerminator::Return(Some(MirValue::Int(0)))),
            },
        ],
        is_async: false,
        inline_never: true,
        inline_always: false,
        aggregate_ret: None,
        returns_future: false,
    }
}

/// 改写帧头里记录的函数指针符号名（主要用于 `main` future 改名）。
fn rename_frame_metadata(func: &mut MirFunction, old: &str, new: &str) {
    for block in &mut func.blocks {
        for instr in &mut block.instrs {
            if let MirInstr::FuncAddr(_, sym) = instr {
                if sym == &format!("{old}__poll") {
                    *sym = format!("{new}__poll");
                } else if sym == &format!("{old}__drop") {
                    *sym = format!("{new}__drop");
                }
            }
        }
    }
}

/// 空函数没有可拆分的块；其余异步函数都可以降级。
fn can_lower_async(func: &MirFunction) -> bool {
    !func.blocks.is_empty()
}

/// 把 `await f(args)` 改写成 Rust 风格的 poll 循环：
///
/// - 调用 `f` 只得到堆上的 future（帧指针），不执行函数体；
/// - 在同步函数里原地忙轮询 `f__poll` 直到 Ready；
/// - 在异步函数里轮询一次，Pending 就 `Suspend`（保存到父状态机），
///   恢复后从同一处再次轮询子 future。
///
/// `let fut = f(); await fut;` 通过别名表也能识别。
fn rewrite_awaits(
    func: &mut MirFunction,
    producers: &HashMap<String, String>,
    dynamic_futures: &HashSet<String>,
    is_async: bool,
    executor_tick: bool,
) -> (HashMap<String, String>, HashSet<String>) {
    if func.blocks.is_empty() {
        return (HashMap::new(), HashSet::new());
    }
    let old_blocks = std::mem::take(&mut func.blocks);
    let mut next_local = old_blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| match i {
            MirInstr::SlotAddr(_, local, _) => Some(*local),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    // `await` 记录 future 的输出类型。调用点必须用输出类型而不是默认的
    // i64，否则浮点返回会走错寄存器。
    let mut await_types: HashMap<String, NumTy> = HashMap::new();
    for old in &old_blocks {
        for instr in &old.instrs {
            if let MirInstr::Await(_, MirValue::Var(v), ty, _) = instr {
                await_types.insert(v.clone(), *ty);
            }
        }
    }
    for _ in 0..old_blocks.len() + 1 {
        let mut changed = false;
        for old in &old_blocks {
            for instr in &old.instrs {
                if let MirInstr::Assign(dst, MirValue::Var(src)) = instr {
                    if let Some(ty) = await_types.get(src).copied() {
                        if await_types.insert(dst.clone(), ty).is_none() {
                            changed = true;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    // future 所有权：async 调用结果以及被赋值到的局部量。用于在返回时
    // 释放未 await 的 future（Rust 的 Drop 语义），以及移动后清空源名字。
    let mut owned: Vec<String> = Vec::new();
    for old in &old_blocks {
        for instr in &old.instrs {
            if let MirInstr::Call(dst, callee, _, _) = instr {
                if (producers.contains_key(callee) || dynamic_futures.contains(callee))
                    && !owned.contains(dst)
                {
                    owned.push(dst.clone());
                }
            }
        }
    }
    for _ in 0..old_blocks.len() + 1 {
        let mut changed = false;
        for old in &old_blocks {
            for instr in &old.instrs {
                if let MirInstr::Assign(dst, MirValue::Var(src)) = instr {
                    if owned.contains(src) && !owned.contains(dst) {
                        owned.push(dst.clone());
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    // 转义（return / 作为实参 / 写进内存 / asm）的 future 由接收方负责，
    // 不在本函数出口释放。
    let mut escaped: HashSet<String> = HashSet::new();
    for old in &old_blocks {
        for instr in &old.instrs {
            match instr {
                MirInstr::Call(_, _, args, _) => {
                    for arg in args {
                        if let MirValue::Var(v) = arg {
                            if owned.contains(v) {
                                escaped.insert(v.clone());
                            }
                        }
                    }
                }
                MirInstr::Store(_, MirValue::Var(v), _) => {
                    if owned.contains(v) {
                        escaped.insert(v.clone());
                    }
                }
                MirInstr::Asm { inputs, .. } => {
                    for (_, value, _) in inputs {
                        if let MirValue::Var(v) = value {
                            if owned.contains(v) {
                                escaped.insert(v.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(MirTerminator::Return(Some(MirValue::Var(v)))) = &old.term {
            if owned.contains(v) {
                escaped.insert(v.clone());
            }
        }
    }

    let mut blocks: Vec<MirBasicBlock> = Vec::new();
    let mut old_first: HashMap<usize, usize> = HashMap::new();
    let mut pending_terms: Vec<(usize, Option<MirTerminator>)> = Vec::new();
    let mut aliases: HashMap<String, FutureTarget> = HashMap::new();
    let mut temp = 0usize;

    for old in &old_blocks {
        let first = blocks.len();
        old_first.insert(old.id, first);
        let mut cur_id = first;
        let mut cur_instrs: Vec<MirInstr> = Vec::new();

        for instr in &old.instrs {
            match instr {
                MirInstr::Call(dst, callee, args, _)
                    if producers.contains_key(callee) || dynamic_futures.contains(callee) =>
                {
                    let target = match producers.get(callee) {
                        Some(target) => FutureTarget::Static(target.clone()),
                        None => FutureTarget::Dynamic,
                    };
                    aliases.insert(dst.clone(), target);
                    cur_instrs.push(MirInstr::Call(
                        dst.clone(),
                        callee.clone(),
                        args.clone(),
                        NumTy::Uint(64),
                    ));
                }
                MirInstr::Call(dst, callee, args, ty) => {
                    let ty = await_types.get(dst).copied().unwrap_or(*ty);
                    cur_instrs.push(MirInstr::Call(
                        dst.clone(),
                        callee.clone(),
                        args.clone(),
                        ty,
                    ));
                }
                MirInstr::Assign(dst, MirValue::Var(src)) => {
                    if let Some(target) = aliases.get(src).cloned() {
                        aliases.insert(dst.clone(), target);
                    }
                    cur_instrs.push(instr.clone());
                    // future 是移动语义：赋值后源名字失效，清 0 防止重复释放。
                    if owned.contains(src) && src != dst {
                        cur_instrs.push(MirInstr::Assign(src.clone(), MirValue::Int(0)));
                    }
                }
                MirInstr::Await(dst, MirValue::Var(fut), ty, agg) if aliases.contains_key(fut) => {
                    let target = aliases[fut].clone();
                    let poll_id = blocks.len() + 1;
                    let pending_id = poll_id + 1;
                    let ready_id = if is_async {
                        pending_id + 1
                    } else {
                        poll_id + 1
                    };

                    blocks.push(MirBasicBlock {
                        id: cur_id,
                        instrs: std::mem::take(&mut cur_instrs),
                        term: Some(MirTerminator::Goto(poll_id)),
                    });
                    if executor_tick {
                        if let Some(block) = blocks.last_mut() {
                            block.instrs.push(MirInstr::Call(
                                fresh(&mut temp, "__await_t"),
                                "zr_run_tasks".to_string(),
                                Vec::new(),
                                NumTy::Uint(64),
                            ));
                        }
                    }

                    let status = fresh(&mut temp, "__await_t");
                    let cond = fresh(&mut temp, "__await_t");
                    let poll = match &target {
                        FutureTarget::Static(callee) => MirInstr::Call(
                            status.clone(),
                            format!("{callee}__poll"),
                            vec![MirValue::Var(fut.clone())],
                            NumTy::Int(64),
                        ),
                        FutureTarget::Dynamic => {
                            let addr = fresh(&mut temp, "__await_t");
                            let fp = fresh(&mut temp, "__await_t");
                            if let Some(block) = blocks.last_mut() {
                                block.instrs.push(MirInstr::Binary(
                                    addr.clone(),
                                    MirValue::Var(fut.clone()),
                                    MirBinaryOp::Add,
                                    MirValue::Int(POLL_OFF),
                                    NumTy::Uint(64),
                                ));
                                block.instrs.push(MirInstr::Load(
                                    fp.clone(),
                                    MirValue::Var(addr),
                                    NumTy::Uint(64),
                                ));
                            }
                            MirInstr::CallIndirect(
                                status.clone(),
                                MirValue::Var(fp),
                                vec![MirValue::Var(fut.clone())],
                                NumTy::Int(64),
                            )
                        }
                    };
                    blocks.push(MirBasicBlock {
                        id: poll_id,
                        instrs: vec![
                            poll,
                            MirInstr::Binary(
                                cond.clone(),
                                MirValue::Var(status),
                                MirBinaryOp::Ne,
                                MirValue::Int(0),
                                NumTy::Uint(64),
                            ),
                        ],
                        term: Some(MirTerminator::Branch {
                            cond: MirValue::Var(cond),
                            then_block: ready_id,
                            else_block: if is_async { pending_id } else { poll_id },
                        }),
                    });

                    if is_async {
                        blocks.push(MirBasicBlock {
                            id: pending_id,
                            instrs: vec![MirInstr::Suspend],
                            term: Some(MirTerminator::Goto(poll_id)),
                        });
                    }

                    cur_id = ready_id;
                    if let Some(size) = agg {
                        // 聚合返回值：输出字节在子帧的 RESULT_AGG_OFF。先搬进
                        // 调用方自己的存储，再释放子帧；若结果未被消费，builder
                        // 注册的临时量会在语句结束时对这块存储执行 Drop。
                        let storage = fresh(&mut temp, "__await_agg");
                        cur_instrs.push(MirInstr::SlotAddr(
                            storage.clone(),
                            next_local,
                            (*size).max(1) as u32,
                        ));
                        next_local += 1;
                        let addr = fresh(&mut temp, "__await_t");
                        cur_instrs.push(MirInstr::Binary(
                            addr.clone(),
                            MirValue::Var(fut.clone()),
                            MirBinaryOp::Add,
                            MirValue::Int(RESULT_AGG_OFF),
                            NumTy::Uint(64),
                        ));
                        copy_bytes(
                            &mut cur_instrs,
                            MirValue::Var(storage.clone()),
                            0,
                            MirValue::Var(addr),
                            *size,
                            &mut temp,
                        );
                        cur_instrs.push(MirInstr::Call(
                            fresh(&mut temp, "__await_t"),
                            "zr_dealloc".to_string(),
                            vec![MirValue::Var(fut.clone())],
                            NumTy::Uint(64),
                        ));
                        // 立即清空 future 名字：后续的聚合结果搬运/赋值不能
                        // 让“最后一次使用”分析把已释放的帧再 drop 一次。
                        if owned.contains(fut) {
                            cur_instrs.push(MirInstr::Assign(fut.clone(), MirValue::Int(0)));
                        }
                        cur_instrs.push(MirInstr::Assign(dst.clone(), MirValue::Var(storage)));
                    } else {
                        let addr = fresh(&mut temp, "__await_t");
                        cur_instrs.push(MirInstr::Binary(
                            addr.clone(),
                            MirValue::Var(fut.clone()),
                            MirBinaryOp::Add,
                            MirValue::Int(RESULT_OFF),
                            NumTy::Uint(64),
                        ));
                        cur_instrs.push(MirInstr::Load(dst.clone(), MirValue::Var(addr), *ty));
                        cur_instrs.push(MirInstr::Call(
                            fresh(&mut temp, "__await_t"),
                            "zr_dealloc".to_string(),
                            vec![MirValue::Var(fut.clone())],
                            NumTy::Uint(64),
                        ));
                        // await 消费 future；出口 drop 不应再次释放。
                        if owned.contains(fut) {
                            cur_instrs.push(MirInstr::Assign(fut.clone(), MirValue::Int(0)));
                        }
                    }
                }
                _ => cur_instrs.push(instr.clone()),
            }
        }

        blocks.push(MirBasicBlock {
            id: cur_id,
            instrs: cur_instrs,
            term: None,
        });
        pending_terms.push((cur_id, old.term.clone()));
    }

    for (sid, old_term) in pending_terms {
        let block = blocks.iter_mut().find(|b| b.id == sid).unwrap();
        block.term = Some(match old_term.unwrap_or(MirTerminator::Return(None)) {
            MirTerminator::Goto(t) => MirTerminator::Goto(old_first[&t]),
            MirTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => MirTerminator::Branch {
                cond,
                then_block: old_first[&then_block],
                else_block: old_first[&else_block],
            },
            other => other,
        });
    }

    // 活性分析：future 在最后一次使用后立即释放，覆盖循环/局部作用域，
    // 而不是只在函数返回时统一释放。await 已经释放的 future 会被跳过。
    let insertions = early_drop_points(&blocks, &owned, &escaped, &aliases);
    let mut early_tmp = 0usize;
    for (bi, ii, names) in insertions.into_iter().rev() {
        let mut seq = Vec::new();
        for name in names {
            let target = aliases[&name].clone();
            emit_future_drop(&mut seq, &name, &target, &mut early_tmp);
            seq.push(MirInstr::Assign(name, MirValue::Int(0)));
        }
        blocks[bi].instrs.splice(ii + 1..ii + 1, seq);
    }

    // 所有可能持有 future 的名字入口置 0；这样任意返回路径上的 drop 都
    // 只释放真正初始化过的帧（drop 辅助函数会跳过空指针）。
    if !owned.is_empty() {
        let mut inits: Vec<MirInstr> = owned
            .iter()
            .map(|name| MirInstr::Assign(name.clone(), MirValue::Int(0)))
            .collect();
        inits.extend(std::mem::take(&mut blocks[0].instrs));
        blocks[0].instrs = inits;
    }

    // 在每个返回点释放仍未消费、未转义的 future。
    for block in &mut blocks {
        if matches!(block.term, Some(MirTerminator::Return(_))) {
            for name in owned.iter().rev() {
                if escaped.contains(name) {
                    continue;
                }
                if let Some(target) = aliases.get(name).cloned() {
                    emit_future_drop(&mut block.instrs, name, &target, &mut temp);
                }
            }
        }
    }

    func.blocks = blocks;
    let live: Vec<String> = owned
        .into_iter()
        .filter(|name| !escaped.contains(name))
        .collect();
    let static_callees = live
        .iter()
        .filter_map(|name| match aliases.get(name) {
            Some(FutureTarget::Static(callee)) => Some((name.clone(), callee.clone())),
            _ => None,
        })
        .collect();
    let dynamic = live
        .into_iter()
        .filter(|name| matches!(aliases.get(name), Some(FutureTarget::Dynamic)))
        .collect();
    (static_callees, dynamic)
}

/// 计算“普通函数返回某个异步函数 future”的映射（fixpoint，支持包装器
/// 多层转返回）。所有返回路径必须指向同一个异步函数，否则视为动态 future，
/// 不参与 await/drop 重写。
fn compute_future_producers(
    functions: &[MirFunction],
    lowerable: &HashSet<String>,
) -> HashMap<String, String> {
    let mut producers: HashMap<String, String> = lowerable
        .iter()
        .map(|name| (name.clone(), name.clone()))
        .collect();
    loop {
        let mut changed = false;
        for func in functions {
            if func.is_async || producers.contains_key(&func.name) {
                continue;
            }
            let mut aliases: HashMap<String, String> = HashMap::new();
            for block in &func.blocks {
                for instr in &block.instrs {
                    match instr {
                        MirInstr::Call(dst, callee, _, _) => {
                            if let Some(target) = producers.get(callee) {
                                aliases.insert(dst.clone(), target.clone());
                            }
                        }
                        MirInstr::Assign(dst, MirValue::Var(src)) => {
                            if let Some(target) = aliases.get(src) {
                                aliases.insert(dst.clone(), target.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
            for _ in 0..func.blocks.len() + 1 {
                let mut again = false;
                for block in &func.blocks {
                    for instr in &block.instrs {
                        if let MirInstr::Assign(dst, MirValue::Var(src)) = instr {
                            if let Some(target) = aliases.get(src).cloned() {
                                if aliases.insert(dst.clone(), target).is_none() {
                                    again = true;
                                }
                            }
                        }
                    }
                }
                if !again {
                    break;
                }
            }
            let mut rets: HashSet<String> = HashSet::new();
            for block in &func.blocks {
                if let Some(MirTerminator::Return(Some(MirValue::Var(v)))) = &block.term {
                    if let Some(target) = aliases.get(v) {
                        rets.insert(target.clone());
                    }
                }
            }
            if rets.len() == 1 {
                let target = rets.into_iter().next().unwrap();
                producers.insert(func.name.clone(), target);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    producers
}

/// 找出 future 的最后一次使用/死定义，返回 (块下标, 指令下标, 名字) 插入点。
/// 跳过被 await 消费（下一条指令把名字清 0）和所有权转义的 future。
fn early_drop_points(
    blocks: &[MirBasicBlock],
    owned: &[String],
    escaped: &HashSet<String>,
    aliases: &HashMap<String, FutureTarget>,
) -> Vec<(usize, usize, Vec<String>)> {
    let mut live_in: HashMap<usize, HashSet<String>> =
        blocks.iter().map(|b| (b.id, HashSet::new())).collect();
    loop {
        let mut changed = false;
        for block in blocks {
            let mut live = HashSet::new();
            for succ in term_successors(block.term.as_ref()) {
                if let Some(s) = live_in.get(&succ) {
                    live.extend(s.iter().cloned());
                }
            }
            if let Some(term) = &block.term {
                live.extend(term_uses(term));
            }
            for instr in block.instrs.iter().rev() {
                if let Some(name) = instr_def(instr) {
                    live.remove(&name);
                }
                live.extend(instr_uses(instr));
            }
            if live_in.get(&block.id) != Some(&live) {
                live_in.insert(block.id, live);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut insertions = Vec::new();
    for (bi, block) in blocks.iter().enumerate() {
        let mut live: HashSet<String> = HashSet::new();
        for succ in term_successors(block.term.as_ref()) {
            if let Some(s) = live_in.get(&succ) {
                live.extend(s.iter().cloned());
            }
        }
        if let Some(term) = &block.term {
            live.extend(term_uses(term));
        }
        for ii in (0..block.instrs.len()).rev() {
            let instr = &block.instrs[ii];
            let def = instr_def(instr);
            let uses = instr_uses(instr);
            // `dst = src` 是 future 移动：源名字随后会被清 0，不能在这里
            // 提前 drop 源，否则和目标的 drop 重复释放同一帧。
            let moved_src = match instr {
                MirInstr::Assign(dst, MirValue::Var(src)) if src != dst && owned.contains(src) => {
                    Some(src.as_str())
                }
                _ => None,
            };
            let mut names = Vec::new();
            for name in owned.iter().rev() {
                if escaped.contains(name) || !aliases.contains_key(name) {
                    continue;
                }
                if moved_src == Some(name.as_str()) {
                    continue;
                }
                // 移动/await 后的清 0 不是新 future，别在空指针上再 drop。
                if matches!(instr, MirInstr::Assign(n, MirValue::Int(0)) if n == name) {
                    continue;
                }
                if def.as_deref() != Some(name.as_str()) && !uses.contains(name) {
                    continue;
                }
                if live.contains(name) {
                    continue;
                }
                if ii + 1 < block.instrs.len()
                    && matches!(&block.instrs[ii + 1], MirInstr::Assign(n, MirValue::Int(0)) if n == name)
                {
                    continue;
                }
                names.push(name.clone());
            }
            if !names.is_empty() {
                insertions.push((bi, ii, names));
            }
            if let Some(name) = def {
                live.remove(&name);
            }
            for name in uses {
                live.insert(name);
            }
        }
    }
    insertions
}

fn lower_function(
    mut func: MirFunction,
    future_callees: &HashMap<String, String>,
    dynamic_callees: &HashSet<String>,
    frame_metadata: bool,
) -> Option<(MirFunction, MirFunction, MirFunction)> {
    if func.blocks.is_empty() {
        return None;
    }

    let (suspend_live, suspend_count) = liveness(&func);

    let mut stored: HashSet<String> = HashSet::new();
    for live in suspend_live.values() {
        stored.extend(live.iter().cloned());
    }
    let arg_count = func.param_types.len();
    for i in 0..arg_count {
        stored.insert(format!("arg_{i}"));
    }

    let types = infer_types(&func);

    // 被取地址的局部量/数组槽也搬进堆帧（Rust 协程把所有跨挂起点存活的
    // 局部量保存在状态里）。槽按 16 字节对齐，兼容 SSE 数组访问。
    let mut local_offsets: HashMap<u32, i128> = HashMap::new();
    let result_bytes = func.aggregate_ret.unwrap_or(0) as i128;
    let mut cursor = if result_bytes > 0 {
        (RESULT_AGG_OFF + result_bytes + 15) & !15
    } else if frame_metadata {
        NAMES_OFF
    } else {
        POLL_OFF
    };
    for block in &func.blocks {
        for instr in &block.instrs {
            if let MirInstr::SlotAddr(_, local, size) = instr {
                local_offsets.entry(*local).or_insert_with(|| {
                    cursor = (cursor + 15) & !15;
                    let off = cursor;
                    cursor += ((*size as i128 + 15) & !15).max(16);
                    off
                });
            }
        }
    }
    for block in &mut func.blocks {
        for instr in &mut block.instrs {
            if let MirInstr::SlotAddr(name, local, _) = instr {
                let off = local_offsets[local];
                *instr = MirInstr::Binary(
                    name.clone(),
                    MirValue::Var("__frame".to_string()),
                    MirBinaryOp::Add,
                    MirValue::Int(off),
                    NumTy::Uint(64),
                );
            }
        }
    }

    let mut names: Vec<String> = stored.into_iter().collect();
    names.sort();
    let mut offsets: HashMap<String, i128> = HashMap::new();
    for (i, name) in names.iter().enumerate() {
        offsets.insert(name.clone(), cursor + (i as i128) * 8);
    }
    let frame_size = cursor + (names.len() as i128) * 8;

    let mut temp_id = 0usize;
    let ty_of = |name: &str| -> NumTy { types.get(name).copied().unwrap_or(NumTy::Uint(64)) };

    // 状态编号：0 为入口，1..=suspend_count 为各挂起点的续体。
    let mut states: HashMap<(usize, usize), i64> = HashMap::new();
    let mut state_id = 1i64;
    for block in &func.blocks {
        for (idx, instr) in block.instrs.iter().enumerate() {
            if matches!(instr, MirInstr::Suspend) {
                states.insert((block.id, idx), state_id);
                state_id += 1;
            }
        }
    }

    // 预先分配块号：dispatch、每个状态的检查块、无效块，然后才是状态机段。
    let nstates = suspend_count + 1;
    let invalid_id = 1 + nstates;
    let first_segment_id = invalid_id + 1;

    let mut old_first: HashMap<usize, usize> = HashMap::new();
    let mut resume_target: HashMap<i64, usize> = HashMap::new();
    let mut segments: Vec<MirBasicBlock> = Vec::new();
    let mut pending_terms: Vec<(usize, usize)> = Vec::new();
    let mut seg_id = first_segment_id;

    for (block_pos, block) in func.blocks.iter().enumerate() {
        let split_points: Vec<usize> = block
            .instrs
            .iter()
            .enumerate()
            .filter_map(|(i, ins)| matches!(ins, MirInstr::Suspend).then_some(i))
            .collect();

        let mut start = 0usize;
        let mut segment_index = 0usize;
        loop {
            let end = split_points
                .get(segment_index)
                .copied()
                .unwrap_or(block.instrs.len());
            let mut instrs: Vec<MirInstr> = Vec::new();

            if start == 0 && block_pos == 0 {
                for i in 0..arg_count {
                    let name = format!("arg_{i}");
                    let off = offsets[&name];
                    let a = seg_addr(&mut instrs, "__frame", off, &mut temp_id);
                    instrs.push(MirInstr::Load(name, MirValue::Var(a), func.param_types[i]));
                }
            } else if start > 0 {
                let live = suspend_live
                    .get(&(block.id, start - 1))
                    .cloned()
                    .unwrap_or_default();
                for name in live {
                    let off = offsets[&name];
                    let a = seg_addr(&mut instrs, "__frame", off, &mut temp_id);
                    instrs.push(MirInstr::Load(name.clone(), MirValue::Var(a), ty_of(&name)));
                }
                if let Some(state) = states.get(&(block.id, start - 1)) {
                    resume_target.insert(*state, seg_id);
                }
            }

            instrs.extend(block.instrs[start..end].iter().cloned());
            let ended_at_suspend = end < block.instrs.len();
            let term = if ended_at_suspend {
                let state = states[&(block.id, end)];
                let live = suspend_live
                    .get(&(block.id, end))
                    .cloned()
                    .unwrap_or_default();
                let mut live: Vec<String> = live.into_iter().collect();
                live.sort();
                for name in live {
                    let off = offsets[&name];
                    let a = seg_addr(&mut instrs, "__frame", off, &mut temp_id);
                    instrs.push(MirInstr::Store(
                        MirValue::Var(a),
                        MirValue::Var(name.clone()),
                        ty_of(&name),
                    ));
                }
                let a = seg_addr(&mut instrs, "__frame", STATE_OFF, &mut temp_id);
                instrs.push(MirInstr::Store(
                    MirValue::Var(a),
                    MirValue::Int(state as i128),
                    NumTy::Uint(64),
                ));
                Some(MirTerminator::Return(Some(MirValue::Int(0))))
            } else {
                None
            };

            if segment_index == 0 {
                old_first.insert(block.id, seg_id);
            }
            segments.push(MirBasicBlock {
                id: seg_id,
                instrs,
                term: term.clone(),
            });
            if term.is_none() {
                pending_terms.push((seg_id, block.id));
            }
            seg_id += 1;
            if !ended_at_suspend {
                break;
            }
            start = end + 1;
            segment_index += 1;
        }
    }

    // 重写原始终结器（把旧块号映射到各自第一段）。
    for (sid, old_id) in pending_terms {
        let old_block = func.blocks.iter().find(|b| b.id == old_id).unwrap();
        let seg = segments.iter_mut().find(|s| s.id == sid).unwrap();
        seg.term = Some(
            match old_block
                .term
                .clone()
                .unwrap_or(MirTerminator::Return(None))
            {
                MirTerminator::Goto(t) => MirTerminator::Goto(old_first[&t]),
                MirTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                } => MirTerminator::Branch {
                    cond,
                    then_block: old_first[&then_block],
                    else_block: old_first[&else_block],
                },
                MirTerminator::Return(value) => {
                    if let Some(value) = value {
                        if !matches!(value, MirValue::Unit) {
                            if let Some(size) = func.aggregate_ret {
                                copy_bytes(
                                    &mut seg.instrs,
                                    MirValue::Var("__frame".to_string()),
                                    RESULT_AGG_OFF,
                                    value,
                                    size,
                                    &mut temp_id,
                                );
                            } else {
                                let (_, vty) = value_type(&value, &types);
                                let (value, store_ty) =
                                    widen_int_result(&mut seg.instrs, value, vty, &mut temp_id);
                                let a =
                                    seg_addr(&mut seg.instrs, "__frame", RESULT_OFF, &mut temp_id);
                                seg.instrs
                                    .push(MirInstr::Store(MirValue::Var(a), value, store_ty));
                            }
                        }
                    }
                    MirTerminator::Return(Some(MirValue::Int(1)))
                }
                MirTerminator::Unreachable => MirTerminator::Unreachable,
            },
        );
    }

    // 入口段是旧入口块的第一段；没有挂起时也必须有。
    let entry_id = old_first[&func.blocks[0].id];

    let mut blocks: Vec<MirBasicBlock> = Vec::new();
    let state_name = fresh(&mut temp_id, "__async_t");
    blocks.push(MirBasicBlock {
        id: 0,
        instrs: vec![
            MirInstr::Assign("__frame".to_string(), MirValue::Var("arg_0".to_string())),
            MirInstr::Load(
                state_name.clone(),
                MirValue::Var("__frame".to_string()),
                NumTy::Uint(64),
            ),
        ],
        term: Some(MirTerminator::Goto(1)),
    });

    for k in 0..nstates {
        let target = if k == 0 {
            entry_id
        } else {
            resume_target[&(k as i64)]
        };
        let cond = fresh(&mut temp_id, "__async_t");
        let next = if k + 1 < nstates { 2 + k } else { invalid_id };
        blocks.push(MirBasicBlock {
            id: 1 + k,
            instrs: vec![MirInstr::Binary(
                cond.clone(),
                MirValue::Var(state_name.clone()),
                MirBinaryOp::Eq,
                MirValue::Int(k as i128),
                NumTy::Uint(64),
            )],
            term: Some(MirTerminator::Branch {
                cond: MirValue::Var(cond),
                then_block: target,
                else_block: next,
            }),
        });
    }
    blocks.push(MirBasicBlock {
        id: invalid_id,
        instrs: Vec::new(),
        term: Some(MirTerminator::Return(Some(MirValue::Int(-1)))),
    });

    let offset = first_segment_id;
    for (i, mut seg) in segments.drain(..).enumerate() {
        seg.id = offset + i;
        blocks.push(seg);
    }

    let poll = MirFunction {
        name: format!("{}__poll", func.name),
        params: vec![MirParam],
        param_types: vec![NumTy::Uint(64)],
        blocks,
        is_async: false,
        inline_never: true,
        inline_always: false,
        aggregate_ret: None,
        returns_future: false,
    };

    // Rust 风格：`f(args)` 只创建 future（堆帧），不执行函数体；
    // 由 `await` 负责轮询 `f__poll`，Ready 后再读结果并释放帧。
    let mut wtemp = 0usize;
    let frame = "__wrap_frame".to_string();
    let mut entry_instrs = vec![MirInstr::Call(
        frame.clone(),
        "zr_alloc".to_string(),
        vec![MirValue::Int(frame_size)],
        NumTy::Uint(64),
    )];
    let a = bin_addr(&mut entry_instrs, &frame, STATE_OFF, &mut wtemp);
    entry_instrs.push(MirInstr::Store(
        MirValue::Var(a),
        MirValue::Int(0),
        NumTy::Uint(64),
    ));
    let a = bin_addr(&mut entry_instrs, &frame, RESULT_OFF, &mut wtemp);
    entry_instrs.push(MirInstr::Store(
        MirValue::Var(a),
        MirValue::Int(0),
        NumTy::Uint(64),
    ));
    let future_name = func.name.clone();
    if frame_metadata {
        for (off, suffix) in [(POLL_OFF, "poll"), (DROP_OFF, "drop")] {
            let fp = format!("__wrap_fp{wtemp}");
            wtemp += 1;
            entry_instrs.push(MirInstr::FuncAddr(
                fp.clone(),
                format!("{future_name}__{suffix}"),
            ));
            let a = bin_addr(&mut entry_instrs, &frame, off, &mut wtemp);
            entry_instrs.push(MirInstr::Store(
                MirValue::Var(a),
                MirValue::Var(fp),
                NumTy::Uint(64),
            ));
        }
    }
    for i in 0..arg_count {
        let name = format!("arg_{i}");
        let a = bin_addr(&mut entry_instrs, &frame, offsets[&name], &mut wtemp);
        entry_instrs.push(MirInstr::Store(
            MirValue::Var(a),
            MirValue::Var(name),
            func.param_types[i],
        ));
    }

    let wrapper = MirFunction {
        name: func.name,
        params: vec![MirParam; arg_count],
        param_types: func.param_types,
        blocks: vec![MirBasicBlock {
            id: 0,
            instrs: entry_instrs,
            term: Some(MirTerminator::Return(Some(MirValue::Var(frame)))),
        }],
        is_async: false,
        inline_never: func.inline_never,
        inline_always: false,
        aggregate_ret: None,
        returns_future: false,
    };

    let drop = make_async_drop(
        &wrapper.name,
        future_callees,
        dynamic_callees,
        &offsets,
        &states,
        &suspend_live,
    );
    Some((wrapper, poll, drop))
}

/// 生成 `f__drop(frame)`：按帧里的状态释放仍然存活的子 future，再释放父帧。
/// 这样未 await 的挂起 future 不会泄漏整棵子 future 树。
fn make_async_drop(
    name: &str,
    future_callees: &HashMap<String, String>,
    dynamic_callees: &HashSet<String>,
    offsets: &HashMap<String, i128>,
    states: &HashMap<(usize, usize), i64>,
    suspend_live: &HashMap<(usize, usize), HashSet<String>>,
) -> MirFunction {
    let mut temp = 0usize;
    let nstates = states.values().copied().max().unwrap_or(0) as usize + 1;
    let ret_id = 2 * nstates;
    let state0_id = nstates;
    let state_id = |k: i64| nstates + k as usize;

    let mut state_children: HashMap<i64, Vec<String>> = HashMap::new();
    for (key, state) in states {
        let mut names: Vec<String> = suspend_live
            .get(key)
            .into_iter()
            .flatten()
            .filter(|n| {
                (future_callees.contains_key(*n) || dynamic_callees.contains(*n))
                    && offsets.contains_key(*n)
            })
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        if !names.is_empty() {
            state_children.insert(*state, names);
        }
    }

    let cond = "__drop_cond".to_string();
    let state = "__drop_state".to_string();
    let mut blocks: Vec<MirBasicBlock> = Vec::new();
    blocks.push(MirBasicBlock {
        id: 0,
        instrs: vec![MirInstr::Binary(
            cond.clone(),
            MirValue::Var("arg_0".to_string()),
            MirBinaryOp::Ne,
            MirValue::Int(0),
            NumTy::Uint(64),
        )],
        term: Some(MirTerminator::Branch {
            cond: MirValue::Var(cond),
            then_block: if nstates > 1 { 1 } else { state0_id },
            else_block: ret_id,
        }),
    });

    for k in 1..nstates {
        let eq = fresh(&mut temp, "__drop_t");
        let next = if k + 1 < nstates { k + 1 } else { state0_id };
        blocks.push(MirBasicBlock {
            id: k,
            instrs: vec![
                MirInstr::Load(
                    state.clone(),
                    MirValue::Var("arg_0".to_string()),
                    NumTy::Uint(64),
                ),
                MirInstr::Binary(
                    eq.clone(),
                    MirValue::Var(state.clone()),
                    MirBinaryOp::Eq,
                    MirValue::Int(k as i128),
                    NumTy::Uint(64),
                ),
            ],
            term: Some(MirTerminator::Branch {
                cond: MirValue::Var(eq),
                then_block: state_id(k as i64),
                else_block: next,
            }),
        });
    }

    // 块在 vec 中的顺序必须与 id 一致：state0 在状态块之前。
    let state0_tmp = fresh(&mut temp, "__drop_t");
    blocks.push(MirBasicBlock {
        id: state0_id,
        instrs: vec![MirInstr::Call(
            state0_tmp,
            "zr_dealloc".to_string(),
            vec![MirValue::Var("arg_0".to_string())],
            NumTy::Uint(64),
        )],
        term: Some(MirTerminator::Return(None)),
    });

    let mut drop_state = |k: i64| -> MirBasicBlock {
        let mut instrs = Vec::new();
        if let Some(children) = state_children.get(&k) {
            for child in children {
                let off = offsets[child];
                let addr = seg_addr(&mut instrs, "arg_0", off, &mut temp);
                let value = fresh(&mut temp, "__drop_t");
                instrs.push(MirInstr::Load(
                    value.clone(),
                    MirValue::Var(addr),
                    NumTy::Uint(64),
                ));
                if dynamic_callees.contains(child) {
                    instrs.push(MirInstr::Call(
                        fresh(&mut temp, "__drop_t"),
                        "__zr_dyn_future_drop".to_string(),
                        vec![MirValue::Var(value)],
                        NumTy::Uint(64),
                    ));
                } else {
                    let callee = &future_callees[child];
                    instrs.push(MirInstr::Call(
                        fresh(&mut temp, "__drop_t"),
                        format!("{callee}__drop"),
                        vec![MirValue::Var(value)],
                        NumTy::Uint(64),
                    ));
                }
            }
        }
        instrs.push(MirInstr::Call(
            fresh(&mut temp, "__drop_t"),
            "zr_dealloc".to_string(),
            vec![MirValue::Var("arg_0".to_string())],
            NumTy::Uint(64),
        ));
        MirBasicBlock {
            id: state_id(k),
            instrs,
            term: Some(MirTerminator::Return(None)),
        }
    };

    for k in 1..nstates {
        blocks.push(drop_state(k as i64));
    }
    blocks.push(MirBasicBlock {
        id: ret_id,
        instrs: Vec::new(),
        term: Some(MirTerminator::Return(None)),
    });

    MirFunction {
        name: format!("{name}__drop"),
        params: vec![MirParam],
        param_types: vec![NumTy::Uint(64)],
        blocks,
        is_async: false,
        inline_never: true,
        inline_always: false,
        aggregate_ret: None,
        returns_future: false,
    }
}

fn make_async_main(future: &str, poll: &str) -> MirFunction {
    let mut temp = 0usize;
    let frame = "__main_future".to_string();
    let status = fresh(&mut temp, "__main_t");
    let cond = fresh(&mut temp, "__main_t");
    let result = fresh(&mut temp, "__main_t");
    let mut done = Vec::new();
    let a = bin_addr(&mut done, &frame, RESULT_OFF, &mut temp);
    done.push(MirInstr::Load(
        result.clone(),
        MirValue::Var(a),
        NumTy::Int(64),
    ));
    done.push(MirInstr::Call(
        fresh(&mut temp, "__main_t"),
        "zr_dealloc".to_string(),
        vec![MirValue::Var(frame.clone())],
        NumTy::Uint(64),
    ));
    MirFunction {
        name: "main".to_string(),
        params: Vec::new(),
        param_types: Vec::new(),
        blocks: vec![
            MirBasicBlock {
                id: 0,
                instrs: vec![MirInstr::Call(
                    frame.clone(),
                    future.to_string(),
                    Vec::new(),
                    NumTy::Uint(64),
                )],
                term: Some(MirTerminator::Goto(1)),
            },
            MirBasicBlock {
                id: 1,
                instrs: vec![
                    MirInstr::Call(
                        status.clone(),
                        poll.to_string(),
                        vec![MirValue::Var(frame.clone())],
                        NumTy::Int(64),
                    ),
                    MirInstr::Binary(
                        cond.clone(),
                        MirValue::Var(status),
                        MirBinaryOp::Ne,
                        MirValue::Int(0),
                        NumTy::Uint(64),
                    ),
                ],
                term: Some(MirTerminator::Branch {
                    cond: MirValue::Var(cond),
                    then_block: 2,
                    else_block: 1,
                }),
            },
            MirBasicBlock {
                id: 2,
                instrs: done,
                term: Some(MirTerminator::Return(Some(MirValue::Var(result)))),
            },
        ],
        is_async: false,
        inline_never: false,
        inline_always: false,
        aggregate_ret: None,
        returns_future: false,
    }
}

/// 把窄整数结果扩展成 64 位再写入结果槽，避免包装器按 64 位读取时
/// 高半部分是无初始化的堆内存（负数会变成很大的正数）。
fn widen_int_result(
    instrs: &mut Vec<MirInstr>,
    value: MirValue,
    ty: NumTy,
    temp: &mut usize,
) -> (MirValue, NumTy) {
    let to = match ty {
        NumTy::Int(w) if w < 64 => NumTy::Int(64),
        NumTy::Uint(w) if w < 64 => NumTy::Uint(64),
        NumTy::Bool => NumTy::Int(64),
        _ => return (value, ty),
    };
    let name = fresh(temp, "__async_t");
    instrs.push(MirInstr::Cast(name.clone(), value, ty, to));
    (MirValue::Var(name), to)
}

fn value_type(value: &MirValue, types: &HashMap<String, NumTy>) -> (bool, NumTy) {
    match value {
        MirValue::Var(name) => {
            let ty = types.get(name).copied().unwrap_or(NumTy::Uint(64));
            (ty.is_float(), ty)
        }
        MirValue::Float(_, w) => (true, NumTy::Float(*w)),
        MirValue::Int(_) => (false, NumTy::Int(64)),
        MirValue::Bool(_) => (false, NumTy::Bool),
        MirValue::Unit | MirValue::Str { .. } => (false, NumTy::Uint(64)),
    }
}

fn infer_types(func: &MirFunction) -> HashMap<String, NumTy> {
    let mut types: HashMap<String, NumTy> = HashMap::new();
    for (i, ty) in func.param_types.iter().enumerate() {
        types.insert(format!("arg_{i}"), *ty);
    }
    // 类型推导只允许"变窄/变具体"：浮点覆盖整数，具体宽度覆盖默认的 64 位。
    // 同一个 MIR 名字可能有多个不同宽度的定义（例如先 `= 0` 再 `= i32+1`），
    // 如果允许来回覆盖就会在不动点迭代里振荡。
    for _ in 0..64 {
        let mut changed = false;
        for block in &func.blocks {
            for instr in &block.instrs {
                let (name, ty) = match instr {
                    MirInstr::Assign(n, v) => (n, value_type(v, &types).1),
                    MirInstr::Binary(n, _, _, _, t) => (n, *t),
                    MirInstr::Unary(n, _, v) => (n, value_type(v, &types).1),
                    MirInstr::Await(n, _, t, _) => (n, *t),
                    MirInstr::Call(n, _, _, t) => (n, *t),
                    MirInstr::Cast(n, _, _, to) => (n, *to),
                    MirInstr::Load(n, _, t) => (n, *t),
                    MirInstr::SlotAddr(n, ..) => (n, NumTy::Uint(64)),
                    MirInstr::Asm { name, .. } => (name, NumTy::Uint(64)),
                    _ => continue,
                };
                let replace = match types.get(name) {
                    None => true,
                    Some(old) => !old.is_float() && ty.is_float(),
                };
                if replace && types.get(name) != Some(&ty) {
                    types.insert(name.clone(), ty);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    types
}

fn liveness(func: &MirFunction) -> (HashMap<(usize, usize), HashSet<String>>, usize) {
    let mut live_in: HashMap<usize, HashSet<String>> =
        func.blocks.iter().map(|b| (b.id, HashSet::new())).collect();
    let mut suspend_live: HashMap<(usize, usize), HashSet<String>> = HashMap::new();

    loop {
        let mut changed = false;
        for block in &func.blocks {
            let mut live: HashSet<String> = HashSet::new();
            for succ in term_successors(block.term.as_ref()) {
                if let Some(s) = live_in.get(&succ) {
                    live.extend(s.iter().cloned());
                }
            }
            if let Some(term) = &block.term {
                for use_name in term_uses(term) {
                    live.insert(use_name);
                }
            }
            for (idx, instr) in block.instrs.iter().enumerate().rev() {
                if matches!(instr, MirInstr::Suspend) {
                    let entry = suspend_live.entry((block.id, idx)).or_default();
                    if *entry != live {
                        *entry = live.clone();
                    }
                }
                if let Some(name) = instr_def(instr) {
                    live.remove(&name);
                }
                for use_name in instr_uses(instr) {
                    live.insert(use_name);
                }
            }
            if live_in.get(&block.id) != Some(&live) {
                live_in.insert(block.id, live);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let count = func
        .blocks
        .iter()
        .map(|b| {
            b.instrs
                .iter()
                .filter(|i| matches!(i, MirInstr::Suspend))
                .count()
        })
        .sum();
    (suspend_live, count)
}

fn instr_def(instr: &MirInstr) -> Option<String> {
    match instr {
        MirInstr::Assign(n, _)
        | MirInstr::Binary(n, ..)
        | MirInstr::Unary(n, ..)
        | MirInstr::Await(n, ..)
        | MirInstr::Call(n, ..)
        | MirInstr::FuncAddr(n, ..)
        | MirInstr::CallIndirect(n, ..)
        | MirInstr::Cast(n, ..)
        | MirInstr::Load(n, ..)
        | MirInstr::SlotAddr(n, ..) => Some(n.clone()),
        MirInstr::Asm { name, .. } => Some(name.clone()),
        _ => None,
    }
}

fn instr_uses(instr: &MirInstr) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |v: &MirValue| {
        if let MirValue::Var(name) = v {
            out.push(name.clone());
        }
    };
    match instr {
        MirInstr::Assign(_, v) => push(v),
        MirInstr::Binary(_, a, _, b, _) => {
            push(a);
            push(b);
        }
        MirInstr::Unary(_, _, v) | MirInstr::Await(_, v, ..) | MirInstr::Cast(_, v, ..) => push(v),
        MirInstr::Call(_, _, args, _) => {
            for a in args {
                push(a);
            }
        }
        MirInstr::CallIndirect(_, callee, args, _) => {
            push(callee);
            for a in args {
                push(a);
            }
        }
        MirInstr::FuncAddr(..) => {}
        MirInstr::Load(_, a, _) => push(a),
        MirInstr::Store(a, v, _) => {
            push(a);
            push(v);
        }
        MirInstr::SimdBinary { lhs, rhs, out, .. } => {
            push(lhs);
            push(rhs);
            push(out);
        }
        MirInstr::Asm {
            outputs, inputs, ..
        } => {
            for (_, v, _) in outputs {
                push(v);
            }
            for (_, v, _) in inputs {
                push(v);
            }
        }
        MirInstr::Suspend | MirInstr::SlotAddr(..) => {}
    }
    out
}

fn term_uses(term: &MirTerminator) -> Vec<String> {
    match term {
        MirTerminator::Branch {
            cond: MirValue::Var(name),
            ..
        } => vec![name.clone()],
        MirTerminator::Return(Some(MirValue::Var(name))) => vec![name.clone()],
        _ => Vec::new(),
    }
}

fn term_successors(term: Option<&MirTerminator>) -> Vec<usize> {
    match term {
        Some(MirTerminator::Goto(t)) => vec![*t],
        Some(MirTerminator::Branch {
            then_block,
            else_block,
            ..
        }) => vec![*then_block, *else_block],
        _ => Vec::new(),
    }
}

/// 释放一个 future：静态来源直接调 `f__drop`；动态来源从帧头取 drop
/// 函数指针做间接调用。
fn emit_future_drop(
    instrs: &mut Vec<MirInstr>,
    name: &str,
    target: &FutureTarget,
    temp: &mut usize,
) {
    match target {
        FutureTarget::Static(callee) => instrs.push(MirInstr::Call(
            fresh(temp, "__drop_t"),
            format!("{callee}__drop"),
            vec![MirValue::Var(name.to_string())],
            NumTy::Uint(64),
        )),
        FutureTarget::Dynamic => instrs.push(MirInstr::Call(
            fresh(temp, "__drop_t"),
            "__zr_dyn_future_drop".to_string(),
            vec![MirValue::Var(name.to_string())],
            NumTy::Uint(64),
        )),
    }
}

/// 逐字节/逐 8 字节复制一块内存。用于异步聚合返回值跨帧搬运；
/// 不调用外部 memcpy，保持后端无 libc 依赖。
fn copy_bytes(
    instrs: &mut Vec<MirInstr>,
    dst: MirValue,
    dst_off: i128,
    src: MirValue,
    size: usize,
    counter: &mut usize,
) {
    let addr = |base: &MirValue, off: i128, instrs: &mut Vec<MirInstr>, counter: &mut usize| {
        if off == 0 {
            base.clone()
        } else {
            let name = fresh(counter, "__copy_t");
            instrs.push(MirInstr::Binary(
                name.clone(),
                base.clone(),
                MirBinaryOp::Add,
                MirValue::Int(off),
                NumTy::Uint(64),
            ));
            MirValue::Var(name)
        }
    };
    let mut off = 0usize;
    while off + 8 <= size {
        let tmp = fresh(counter, "__copy_t");
        let s = addr(&src, off as i128, instrs, counter);
        instrs.push(MirInstr::Load(tmp.clone(), s, NumTy::Uint(64)));
        let d = addr(&dst, dst_off + off as i128, instrs, counter);
        instrs.push(MirInstr::Store(d, MirValue::Var(tmp), NumTy::Uint(64)));
        off += 8;
    }
    while off < size {
        let tmp = fresh(counter, "__copy_t");
        let s = addr(&src, off as i128, instrs, counter);
        instrs.push(MirInstr::Load(tmp.clone(), s, NumTy::Uint(8)));
        let d = addr(&dst, dst_off + off as i128, instrs, counter);
        instrs.push(MirInstr::Store(d, MirValue::Var(tmp), NumTy::Uint(8)));
        off += 1;
    }
}

fn fresh(counter: &mut usize, prefix: &str) -> String {
    let name = format!("{prefix}{counter}");
    *counter += 1;
    name
}

fn seg_addr(instrs: &mut Vec<MirInstr>, base: &str, off: i128, counter: &mut usize) -> String {
    let name = fresh(counter, "__async_t");
    instrs.push(MirInstr::Binary(
        name.clone(),
        MirValue::Var(base.to_string()),
        MirBinaryOp::Add,
        MirValue::Int(off),
        NumTy::Uint(64),
    ));
    name
}

fn bin_addr(instrs: &mut Vec<MirInstr>, base: &str, off: i128, counter: &mut usize) -> String {
    let name = format!("__wrap_t{counter}");
    *counter += 1;
    instrs.push(MirInstr::Binary(
        name.clone(),
        MirValue::Var(base.to_string()),
        MirBinaryOp::Add,
        MirValue::Int(off),
        NumTy::Uint(64),
    ));
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_drop_blocks_are_in_id_order() {
        let mut futures = HashMap::new();
        futures.insert("child".to_string(), "leaf".to_string());
        let mut offsets = HashMap::new();
        offsets.insert("child".to_string(), 16);
        let mut states = HashMap::new();
        states.insert((0, 0), 1);
        states.insert((0, 2), 2);
        let mut live = HashMap::new();
        live.insert((0, 0), HashSet::from(["child".to_string()]));
        live.insert((0, 2), HashSet::from(["child".to_string()]));

        let f = make_async_drop("f", &futures, &HashSet::new(), &offsets, &states, &live);
        assert_eq!(f.name, "f__drop");
        for (i, block) in f.blocks.iter().enumerate() {
            assert_eq!(block.id, i, "drop blocks must be indexed by id");
        }
        let calls = f
            .blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .filter(|i| matches!(i, MirInstr::Call(_, c, _, _) if c == "leaf__drop"))
            .count();
        assert_eq!(calls, 2);
    }

    #[test]
    fn early_drop_skips_moved_source_and_null_assignments() {
        let blocks = vec![MirBasicBlock {
            id: 0,
            instrs: vec![
                MirInstr::Call(
                    "t".to_string(),
                    "f".to_string(),
                    Vec::new(),
                    NumTy::Uint(64),
                ),
                MirInstr::Assign("v".to_string(), MirValue::Var("t".to_string())),
                MirInstr::Assign("t".to_string(), MirValue::Int(0)),
            ],
            term: Some(MirTerminator::Return(None)),
        }];
        let mut futures = HashMap::new();
        futures.insert("t".to_string(), FutureTarget::Static("f".to_string()));
        futures.insert("v".to_string(), FutureTarget::Static("f".to_string()));
        let owned = vec!["t".to_string(), "v".to_string()];
        let points = early_drop_points(&blocks, &owned, &HashSet::new(), &futures);
        let names: Vec<&str> = points
            .iter()
            .flat_map(|(_, _, names)| names.iter().map(String::as_str))
            .collect();
        assert_eq!(names, vec!["v"]);
    }

    #[test]
    fn futures_dying_at_same_point_drop_in_reverse_order() {
        let blocks = vec![MirBasicBlock {
            id: 0,
            instrs: vec![
                MirInstr::Call(
                    "t0".to_string(),
                    "f".to_string(),
                    Vec::new(),
                    NumTy::Uint(64),
                ),
                MirInstr::Call(
                    "t1".to_string(),
                    "f".to_string(),
                    Vec::new(),
                    NumTy::Uint(64),
                ),
                MirInstr::Call(
                    "r".to_string(),
                    "g".to_string(),
                    vec![
                        MirValue::Var("t0".to_string()),
                        MirValue::Var("t1".to_string()),
                    ],
                    NumTy::Uint(64),
                ),
            ],
            term: Some(MirTerminator::Return(None)),
        }];
        let futures = HashMap::from([
            ("t0".to_string(), FutureTarget::Static("f".to_string())),
            ("t1".to_string(), FutureTarget::Static("f".to_string())),
        ]);
        let owned = vec!["t0".to_string(), "t1".to_string()];
        let points = early_drop_points(&blocks, &owned, &HashSet::new(), &futures);
        let names: Vec<&str> = points
            .iter()
            .flat_map(|(_, _, names)| names.iter().map(String::as_str))
            .collect();
        assert_eq!(names, vec!["t1", "t0"]);
    }

    #[test]
    fn normal_wrappers_propagate_future_callee() {
        let ret = |_name: &str, value: &str| MirBasicBlock {
            id: 0,
            instrs: Vec::new(),
            term: Some(MirTerminator::Return(Some(MirValue::Var(
                value.to_string(),
            )))),
        };
        let make = MirFunction {
            name: "make".to_string(),
            params: Vec::new(),
            param_types: Vec::new(),
            blocks: vec![MirBasicBlock {
                id: 0,
                instrs: vec![
                    MirInstr::Call(
                        "t".to_string(),
                        "f".to_string(),
                        Vec::new(),
                        NumTy::Uint(64),
                    ),
                    MirInstr::Assign("v".to_string(), MirValue::Var("t".to_string())),
                ],
                term: Some(MirTerminator::Return(Some(MirValue::Var("v".to_string())))),
            }],
            is_async: false,
            inline_never: false,
            inline_always: false,
            aggregate_ret: None,
            returns_future: false,
        };
        let wrap = MirFunction {
            name: "wrap".to_string(),
            params: Vec::new(),
            param_types: Vec::new(),
            blocks: vec![MirBasicBlock {
                id: 0,
                instrs: vec![MirInstr::Call(
                    "t".to_string(),
                    "make".to_string(),
                    Vec::new(),
                    NumTy::Uint(64),
                )],
                term: Some(MirTerminator::Return(Some(MirValue::Var("t".to_string())))),
            }],
            is_async: false,
            inline_never: false,
            inline_always: false,
            aggregate_ret: None,
            returns_future: false,
        };
        let f = MirFunction {
            name: "f".to_string(),
            params: Vec::new(),
            param_types: Vec::new(),
            blocks: vec![ret("f", "arg_0")],
            is_async: true,
            inline_never: false,
            inline_always: false,
            aggregate_ret: None,
            returns_future: false,
        };
        let lowerable = HashSet::from(["f".to_string()]);
        let producers = compute_future_producers(&[f, make, wrap], &lowerable);
        assert_eq!(producers.get("make").map(String::as_str), Some("f"));
        assert_eq!(producers.get("wrap").map(String::as_str), Some("f"));
    }
}
