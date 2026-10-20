//! HIR → MIR 降级器。
//!
//! 降级策略：
//! - 每个 HIR 块对应一个 MIR 基本块；控制结构拆分成多个基本块。
//! - 表达式降级为线性指令序列，结果存入命名临时量。
//! - `if`/`match`/`while`/`for` 脱糖为 `Branch`/`Switch` 终结器。
//! - `break`/`continue` 转为对循环出口/头部块的 `Goto`。

use crate::mir::{
    MirBasicBlock, MirBinaryOp, MirFunction, MirInstr, MirParam, MirTerminator, MirUnaryOp,
    MirValue, NumTy,
};
use crate::typeck::hir::{self, HirBlock, HirConst, HirExpr, HirFunction, HirStmt, LocalId};
use std::collections::{HashMap, HashSet};

/// 降级单个函数。
pub fn lower_function(
    func: &HirFunction,
    body: &HirBlock,
    def_names: &std::collections::HashMap<hir::DefId, String>,
    consts: &std::collections::HashMap<hir::DefId, HirConst>,
    type_defs: &HashMap<hir::DefId, hir::HirTypeDef>,
    drop_impls: &HashMap<hir::DefId, String>,
    fn_params: &HashMap<String, Vec<hir::Ty>>,
) -> MirFunction {
    let mut b = Builder::new(
        func.name.clone(),
        def_names.clone(),
        const_values(consts),
        type_defs.clone(),
        drop_impls.clone(),
        fn_params.clone(),
    );
    // 先扫出被取地址的局部变量：它们不能住在寄存器里，必须固定到栈槽。
    if let Some(body) = &func.body {
        collect_addr_taken(body, &mut b.addr_taken);
    }

    // 参数：先登记参数名，再分配入口块后把 `arg_N` 写进参数对应的局部
    // 变量。类型检查阶段已经把参数绑定成了局部变量，函数体里引用的是
    // 局部名，所以这里必须补一条显式赋值，否则参数永远读不到值。
    for i in 0..func.params.len() {
        b.param_names.push(format!("arg_{i}"));
    }

    // 同步函数按值返回数组/结构体：追加一个隐藏参数指向调用方的返回槽，
    // 避免把指针返回到已经销毁的被调函数栈帧。异步函数对外返回 Future，
    // 真实输出类型在 `async_output` 里。
    let actual_ret = if func.is_async {
        func.async_output.as_ref().unwrap_or(&func.return_type)
    } else {
        &func.return_type
    };
    if !func.is_async {
        if let hir::Ty::Array(elem, len) = actual_ret {
            if let Some(n) = hir::array_len_value(len) {
                let hidden = format!("arg_{}", func.params.len());
                b.param_names.push(hidden.clone());
                b.ret_array = Some(((**elem).clone(), n));
                b.hidden_ret = Some(hidden);
            }
        }
        if let Some(layout) = struct_layout(&b, actual_ret) {
            let hidden = format!("arg_{}", func.params.len());
            b.param_names.push(hidden.clone());
            b.ret_struct = Some((actual_ret.clone(), layout.size));
            b.hidden_ret = Some(hidden);
        }
    } else if matches!(actual_ret, hir::Ty::Array(..) | hir::Ty::Named(..)) {
        // 异步函数没有调用方返回槽：结果先写进堆帧的输出区，await 再把
        // 字节复制到调用方存储。因此这里只需要知道输出大小。
        if let Some((size, _)) = type_size_align(&b, actual_ret, 0) {
            b.async_ret_bytes = Some(size.max(0) as usize);
        }
    }

    // 分配入口块
    let entry = b.new_block();
    b.current = entry;

    // 函数级作用域：参数在函数结束时按逆序 Drop。Drop 实现自己的
    // `self` 参数不能再次 Drop，否则会无限递归。
    b.drop_scopes.push(Vec::new());
    let dropping_self = func.name.ends_with("__drop") && func.params.len() == 1;
    for (i, p) in func.params.iter().enumerate() {
        let ty = num_ty_of(&p.ty);
        b.def_local(p.local.0, MirValue::Var(format!("arg_{i}")), ty);
        if !(dropping_self && i == 0) {
            b.track_drop(p.local, &p.ty);
        }
    }

    // 降级函数体：根块暂不析构，先把尾表达式移进返回槽。
    let mut tail = lower_block_opts(&mut b, body, false);
    if let Some(expr) = &body.tail {
        if b.is_drop_ty(&expr.ty) {
            if let hir::ExprKind::Local(id) = expr.kind {
                b.mark_moved(id);
            } else {
                b.mark_place_moved(expr);
            }
        }
    }
    if b.async_ret_bytes.is_some() {
        if let Some(value) = &tail {
            b.consume_temp(value);
        }
    }
    if let (Some((elem, n)), Some(dst)) = (b.ret_array.clone(), b.hidden_ret.clone()) {
        if let Some(value) = tail.take() {
            b.consume_temp(&value);
            copy_array_into(&mut b, value, &elem, n, MirValue::Var(dst.clone()));
            tail = Some(MirValue::Var(dst));
        }
    }
    if let (Some((ty, _)), Some(dst)) = (b.ret_struct.clone(), b.hidden_ret.clone()) {
        if let Some(value) = tail.take() {
            b.consume_temp(&value);
            if let Some(layout) = struct_layout(&b, &ty) {
                copy_struct_into(&mut b, value, &layout, MirValue::Var(dst.clone()));
            }
            tail = Some(MirValue::Var(dst));
        }
    }

    // 终结入口块：尾表达式的值即函数返回值。先把仍存活的局部量
    // 逆序析构；显式 `return` 已经清理过的情况 term 已存在，不再重复。
    if b.current_block_mut().term.is_none() {
        b.emit_temp_drops_from(0);
        b.emit_all_drops();
        b.current_block_mut().term = Some(MirTerminator::Return(tail));
    }

    // 把收集到的块按分配顺序落盘
    MirFunction {
        name: func.name.clone(),
        is_async: func.is_async,
        param_types: {
            let mut tys: Vec<NumTy> = func.params.iter().map(|p| num_ty_of(&p.ty)).collect();
            if b.ret_array.is_some() || b.ret_struct.is_some() {
                tys.push(NumTy::Uint(64));
            }
            tys
        },
        inline_never: func.attr("inline").is_some_and(|a| a.has_arg("never")),
        inline_always: func.attr("inline").is_some_and(|a| a.has_arg("always")),
        aggregate_ret: b.async_ret_bytes,
        returns_future: !func.is_async && matches!(func.return_type, hir::Ty::Opaque(_)),
        params: b.param_names.iter().map(|_| MirParam).collect(),
        blocks: b.blocks,
    }
}

/// 降级一个块，返回其尾表达式产生的值（若有）。
fn lower_block(b: &mut Builder, block: &HirBlock) -> Option<MirValue> {
    lower_block_opts(b, block, true)
}

/// `emit_scope_drops == false` 用于函数根块：返回值可能来自局部量，必须先
/// 复制到返回槽/标记移动，再统一析构，不能提前把尾表达式的源值析构掉。
fn lower_block_opts(b: &mut Builder, block: &HirBlock, emit_scope_drops: bool) -> Option<MirValue> {
    b.drop_scopes.push(Vec::new());
    let mut tail = None;
    for stmt in &block.stmts {
        lower_stmt(b, stmt);
    }
    if let Some(expr) = &block.tail {
        tail = Some(lower_expr(b, expr));
    }
    if emit_scope_drops {
        if let Some(scope) = b.drop_scopes.pop() {
            b.emit_drop_scope(scope);
        }
    }
    tail
}

/// 降级一条语句。
fn lower_stmt(b: &mut Builder, stmt: &HirStmt) {
    let temp_mark = b.temp_drops.len();
    match stmt {
        HirStmt::Let { local, init } | HirStmt::Var { local, init } => {
            if let Some(init) = init {
                let ty = num_ty_of(&init.ty);
                // `let b = a` 对 Drop 类型是移动，不是复制：标记源已移动，
                // 目标直接接管同一份存储。
                let moved = if let hir::ExprKind::Local(src) = init.kind {
                    if b.is_drop_ty(&init.ty) {
                        b.mark_moved(src);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !moved && b.is_drop_ty(&init.ty) {
                    b.mark_place_moved(init);
                }
                let v = lower_expr(b, init);
                let v = if moved {
                    v
                } else {
                    let v = copy_array_value(b, v, &init.ty);
                    copy_struct_value(b, v, &init.ty)
                };
                b.def_local(local.0, v, ty);
                b.track_drop(*local, &init.ty);
            }
        }
        HirStmt::Expr(expr) => {
            let _ = lower_expr(b, expr);
        }
        HirStmt::Item => {}
    }
    b.emit_temp_drops_from(temp_mark);
}

/// 把一个位置表达式拆成“根局部量 + 字段/常量下标路径”。
/// 动态下标无法静态跟踪，返回空路径让调用方保守地把整个局部量当作已移动。
fn place_move_path(b: &Builder, expr: &HirExpr) -> Option<(LocalId, Vec<usize>)> {
    match &expr.kind {
        hir::ExprKind::Local(id) => Some((*id, Vec::new())),
        hir::ExprKind::Field(base, name) => {
            let (local, mut path) = place_move_path(b, base)?;
            let layout = struct_layout(b, &base.ty)?;
            let idx = *layout.names.get(name)?;
            path.push(idx);
            Some((local, path))
        }
        hir::ExprKind::Index(base, idx) => {
            let (local, mut path) = place_move_path(b, base)?;
            if let hir::ExprKind::Literal(hir::HirLiteral::Int(i)) = idx.kind {
                if i >= 0 {
                    path.push(i as usize);
                    return Some((local, path));
                }
            }
            Some((local, Vec::new()))
        }
        _ => None,
    }
}

/// 降级一个表达式，返回其产生的值。
fn lower_expr(b: &mut Builder, expr: &HirExpr) -> MirValue {
    use hir::ExprKind::*;
    match &expr.kind {
        Literal(lit) => mir_literal(lit, &expr.ty),
        Local(id) => {
            let ty = num_ty_of(&expr.ty);
            b.use_local(id.0, ty)
        }
        // 全局 `const` 引用：直接把常量的值内联进来。
        //
        // 早先这里一律降级成 `MirValue::Unit`（"函数/常量引用降级为占位"），
        // 于是 `const LIMIT: i32 = 40; if LIMIT != 40 { .. }` 编出来是拿
        // unit 去比——恒不相等。`const` 是编译期已知的值，内联它是唯一正确
        // 的处理：它没有存储，也就没有"读一个全局"这种运行时操作。
        Def(id) => match b.const_values.get(id) {
            Some(value) => value.clone(),
            None => {
                // 函数名作为一等值：取符号地址，供函数指针调用/传参。
                if let Some(fn_name) = b.functions.get(id).cloned() {
                    let name = format!("t{}", b.temp_id());
                    b.current_block_mut()
                        .instrs
                        .push(MirInstr::FuncAddr(name.clone(), fn_name));
                    MirValue::Var(name)
                } else {
                    // 其它非 `const` 全局（static）目前没有运行时存储。
                    MirValue::Unit
                }
            }
        },
        Binary(op, lhs, rhs) => {
            // 指针算术单独处理：需要按 pointee 大小缩放偏移、以及把指针差
            // 除回元素个数，不能走下面的普通整数二元运算。
            if let Some(v) = lower_array_binary(b, *op, lhs, rhs, &expr.ty) {
                return v;
            }
            if let Some(v) = lower_pointer_arith(b, *op, lhs, rhs) {
                return v;
            }
            // 结果类型：优先用表达式自身的类型，推断没解出来时退回看操作数。
            let ty = binary_result_ty(*op, &expr.ty, &lhs.ty, &rhs.ty);
            let l = lower_expr(b, lhs);
            let r = lower_expr(b, rhs);
            let name = format!("t{}", b.temp_id());
            b.current_block_mut().instrs.push(MirInstr::Binary(
                name.clone(),
                l,
                convert_binary(*op),
                r,
                ty,
            ));
            MirValue::Var(name)
        }
        Unary(op, operand) => {
            let o = lower_expr(b, operand);
            let name = format!("t{}", b.temp_id());
            b.current_block_mut()
                .instrs
                .push(MirInstr::Unary(name.clone(), convert_unary(*op), o));
            MirValue::Var(name)
        }
        Assign(lhs, rhs) => {
            let v = lower_expr(b, rhs);
            match &lhs.kind {
                // 局部变量赋值。被取地址的局部写进它的栈槽，否则还是寄存器名。
                Local(id) => {
                    let ty = num_ty_of(&lhs.ty);
                    // Drop 类型赋值：先移动/复制右值，再析构旧值。
                    let moved = if let hir::ExprKind::Local(src) = rhs.kind {
                        if b.is_drop_ty(&lhs.ty) {
                            b.mark_moved(src);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !moved && b.is_drop_ty(&lhs.ty) {
                        b.mark_place_moved(rhs);
                    }
                    let v = if moved {
                        v
                    } else {
                        let v = copy_array_value(b, v, &rhs.ty);
                        copy_struct_value(b, v, &rhs.ty)
                    };
                    if b.is_drop_ty(&lhs.ty) {
                        b.replace_drop_local(id.0, &lhs.ty);
                    }
                    b.def_local(id.0, v, ty);
                    b.use_local(id.0, ty)
                }
                // `*p = v`：把值存到指针指向的地址。
                Deref(inner) => {
                    let addr = lower_expr(b, inner);
                    let ty = num_ty_of(&lhs.ty);
                    b.current_block_mut()
                        .instrs
                        .push(MirInstr::Store(addr, v, ty));
                    MirValue::Unit
                }
                // `p[i] = v`：算出 `p + i*size` 再存；结构体元素按值递归复制，
                // 含 Drop 的元素先析构旧值。
                Index(recv, idx) => {
                    let addr = lower_index_addr(b, recv, idx, &lhs.ty);
                    if b.is_drop_ty(&lhs.ty) {
                        if let hir::ExprKind::Local(src) = rhs.kind {
                            b.mark_moved(src);
                        } else {
                            b.mark_place_moved(rhs);
                        }
                        let overwriting_moved = place_move_path(b, lhs)
                            .map(|(l, p)| b.take_moved_path(l, &p))
                            .unwrap_or(false);
                        if !overwriting_moved {
                            b.emit_drop_at(addr.clone(), &lhs.ty, 0);
                        }
                    }
                    store_value(b, addr, v, &lhs.ty);
                    MirValue::Unit
                }
                // `s.field = v`：结构体值以“指向存储的地址”表示。
                Field(recv, name) => {
                    let base = lower_expr(b, recv);
                    if let Some(layout) = struct_layout(b, &recv.ty) {
                        if let Some((off, field_ty)) = layout.offset(name) {
                            let at = offset_addr(b, base, off);
                            if b.is_drop_ty(field_ty) {
                                if let hir::ExprKind::Local(src) = rhs.kind {
                                    b.mark_moved(src);
                                } else {
                                    b.mark_place_moved(rhs);
                                }
                                let overwriting_moved = place_move_path(b, lhs)
                                    .map(|(l, p)| b.take_moved_path(l, &p))
                                    .unwrap_or(false);
                                if !overwriting_moved {
                                    b.emit_drop_at(at.clone(), field_ty, 0);
                                }
                            }
                            store_value(b, at, v, field_ty);
                        }
                    }
                    MirValue::Unit
                }
                _ => MirValue::Unit,
            }
        }
        Array(elems) => {
            // 数组值在运行期就是"指向元素存储的地址"。为字面量分配一块
            // 合成栈槽，逐元素 Store，再把地址当作值返回（与切片/指针同一模型）。
            let elem_ty = match &expr.ty {
                hir::Ty::Array(inner, _) => (**inner).clone(),
                _ => return MirValue::Unit,
            };
            let size = type_size(b, &elem_ty);
            let bytes = (elems.len() as i128) * size;
            let addr = b.synthetic_storage(bytes);
            for (i, e) in elems.iter().enumerate() {
                let v = lower_expr(b, e);
                let off = (i as i128) * size;
                let at = if off == 0 {
                    MirValue::Var(addr.clone())
                } else {
                    binary_into(
                        b,
                        MirValue::Var(addr.clone()),
                        MirBinaryOp::Add,
                        MirValue::Int(off),
                    )
                };
                store_value(b, at, v, &elem_ty);
            }
            b.register_temp(MirValue::Var(addr.clone()), &expr.ty);
            MirValue::Var(addr)
        }
        Struct(fields) => {
            // 结构体值在运行期是“指向字段存储的地址”。每个字段按布局
            // 写入；嵌套结构体/数组递归复制，保证值语义。
            let Some(layout) = struct_layout(b, &expr.ty) else {
                return MirValue::Unit;
            };
            let addr = b.synthetic_storage(layout.size);
            for (name, e) in fields {
                let Some((off, field_ty)) = layout.offset(name) else {
                    continue;
                };
                let v = lower_expr(b, e);
                let at = offset_addr(b, MirValue::Var(addr.clone()), off);
                store_value(b, at, v, field_ty);
            }
            b.register_temp(MirValue::Var(addr.clone()), &expr.ty);
            MirValue::Var(addr)
        }
        Field(recv, name) => {
            let base = lower_expr(b, recv);
            let Some(layout) = struct_layout(b, &recv.ty) else {
                return MirValue::Unit;
            };
            let Some((off, field_ty)) = layout.offset(name) else {
                return MirValue::Unit;
            };
            let at = offset_addr(b, base.clone(), off);
            // 从临时结构体移出 Drop 字段：标记临时量的对应路径，并把字段
            // 地址登记成新的临时量，由消费它的位置（绑定/实参/语句结尾）
            // 负责析构。这样临时量的其余字段仍会正常析构。
            if b.is_drop_ty(field_ty) {
                if let Some(layout) = struct_layout(b, &recv.ty) {
                    if let Some(idx) = layout.names.get(name).copied() {
                        if b.mark_temp_moved_path(&base, idx) {
                            b.register_temp(at.clone(), field_ty);
                        }
                    }
                }
            }
            // 结构体字段的值就是它自己的地址，不再 Load 首字段。
            if struct_layout(b, field_ty).is_some() {
                at
            } else {
                let name = format!("t{}", b.temp_id());
                b.current_block_mut().instrs.push(MirInstr::Load(
                    name.clone(),
                    at,
                    num_ty_of(field_ty),
                ));
                MirValue::Var(name)
            }
        }
        Block(block) => lower_block(b, block).unwrap_or(MirValue::Unit),
        If(cond, then_block, else_block) => {
            let c = lower_expr(b, cond);
            let then_bb = b.new_block();
            let else_bb = b.new_block();
            let join = b.new_block();

            b.current_block_mut().term = Some(MirTerminator::Branch {
                cond: c,
                then_block: then_bb,
                else_block: else_bb,
            });

            // `if` 作为**表达式**时的值合流：两个分支各自把结果写进同一个
            // 临时名，join 块读这个临时名。两个前驱都定义同名变量，
            // SSA 阶段的 φ 插入正好会在这里合流。
            let temp = format!("t{}", b.temp_id());

            // 分支体里可能已经用 `return` 终结了块，这时不能再补指令/跳转
            //（否则会把 return 覆盖掉，分支体里的返回永远到不了）。
            b.current = then_bb;
            let then_val = lower_block(b, then_block);
            b.assign_if_open(&temp, then_val.unwrap_or(MirValue::Unit));
            b.terminate_if_open(MirTerminator::Goto(join));

            // else
            b.current = else_bb;
            let has_else = else_block.is_some();
            if let Some(eb) = else_block {
                let else_val = lower_block(b, eb).unwrap_or(MirValue::Unit);
                b.assign_if_open(&temp, else_val);
            }
            b.terminate_if_open(MirTerminator::Goto(join));

            b.current = join;
            // 没有 else 分支的 `if` 当语句用，值恒为空：不造临时名，
            // 省掉一条永远没人读的定义（以及它占用的寄存器）。
            if has_else {
                b.register_temp(MirValue::Var(temp.clone()), &expr.ty);
                MirValue::Var(temp)
            } else {
                MirValue::Unit
            }
        }
        While(cond, body) => {
            let header = b.new_block();
            let body_bb = b.new_block();
            let exit = b.new_block();

            // `while true` / `while false` 的条件是常量，直接展开，不生成条件
            // 跳转。否则那个常量会被物化进一个寄存器，而循环体里的临时值会
            // 把它占掉——回头再测条件时测到的是临时值，`while true { .. break; }`
            // 会提前一轮退出。
            let const_cond = match &cond.kind {
                hir::ExprKind::Literal(hir::HirLiteral::Bool(v)) => Some(*v),
                _ => None,
            };

            match const_cond {
                Some(true) => {
                    b.current_block_mut().term = Some(MirTerminator::Goto(header));
                    b.current = header;
                    b.current_block_mut().term = Some(MirTerminator::Goto(body_bb));
                }
                Some(false) => {
                    // 条件恒假：整个循环跳过
                    b.current_block_mut().term = Some(MirTerminator::Goto(exit));
                    b.current = exit;
                    return MirValue::Unit;
                }
                None => {
                    b.current_block_mut().term = Some(MirTerminator::Goto(header));
                    b.current = header;
                    let c = lower_expr(b, cond);
                    b.current_block_mut().term = Some(MirTerminator::Branch {
                        cond: c,
                        then_block: body_bb,
                        else_block: exit,
                    });
                }
            }

            b.current = body_bb;
            b.loops.push(LoopCtx {
                header,
                exit,
                drop_depth: b.drop_scopes.len(),
            });
            let _ = lower_block(b, body);
            b.loops.pop();
            b.terminate_if_open(MirTerminator::Goto(header));

            b.current = exit;
            MirValue::Unit
        }
        Loop(body) => {
            let head = b.new_block();
            let exit = b.new_block();
            b.current_block_mut().term = Some(MirTerminator::Goto(head));
            b.current = head;
            b.loops.push(LoopCtx {
                header: head,
                exit,
                drop_depth: b.drop_scopes.len(),
            });
            let _ = lower_block(b, body);
            b.loops.pop();
            b.current_block_mut().term = Some(MirTerminator::Goto(head));
            b.current = exit;
            MirValue::Unit
        }
        Return(v) => {
            let temp_mark = b.temp_drops.len();
            if let Some(e) = v {
                if b.is_drop_ty(&e.ty) {
                    if let hir::ExprKind::Local(id) = e.kind {
                        b.mark_moved(id);
                    } else {
                        b.mark_place_moved(e);
                    }
                }
            }
            let mut rv = v.as_ref().map(|e| lower_expr(b, e));
            if let (Some((elem, n)), Some(dst)) = (b.ret_array.clone(), b.hidden_ret.clone()) {
                if let Some(value) = rv.take() {
                    b.consume_temp(&value);
                    copy_array_into(b, value, &elem, n, MirValue::Var(dst.clone()));
                    rv = Some(MirValue::Var(dst));
                }
            }
            if let (Some((ty, _)), Some(dst)) = (b.ret_struct.clone(), b.hidden_ret.clone()) {
                if let Some(value) = rv.take() {
                    b.consume_temp(&value);
                    if let Some(layout) = struct_layout(b, &ty) {
                        copy_struct_into(b, value, &layout, MirValue::Var(dst.clone()));
                    }
                    rv = Some(MirValue::Var(dst));
                }
            }
            if b.async_ret_bytes.is_some() {
                if let Some(value) = &rv {
                    b.consume_temp(value);
                }
            }
            b.emit_temp_drops_from(temp_mark);
            b.emit_all_drops();
            b.current_block_mut().term = Some(MirTerminator::Return(rv.clone()));
            rv.unwrap_or(MirValue::Unit)
        }
        Await(future) => {
            // await 表达式：先计算 future，然后生成 await 点
            let f = lower_expr(b, future);
            let name = format!("t{}", b.temp_id());
            let agg = match &expr.ty {
                hir::Ty::Array(..) | hir::Ty::Named(..) => {
                    type_size_align(b, &expr.ty, 0).map(|(size, _)| size.max(0) as usize)
                }
                _ => None,
            };
            b.current_block_mut().instrs.push(MirInstr::Await(
                name.clone(),
                f,
                num_ty_of(&expr.ty),
                agg,
            ));
            MirValue::Var(name)
        }
        Call {
            callee,
            args,
            result,
        } => {
            let name = format!("v{}", result.0);
            let fn_name = b
                .functions
                .get(callee)
                .cloned()
                .unwrap_or_else(|| "<unknown>".to_string());
            let mut mir_args = Vec::with_capacity(args.len() + 1);
            for (i, a) in args.iter().enumerate() {
                // `&self` / `&T` 形参只借用存储：不能复制结构体，更不能把
                // Drop 类型的实参标记成已移动。结构体值本身已经是地址，
                // 直接把它交出去即可。
                let by_ref = b
                    .fn_params
                    .get(&fn_name)
                    .and_then(|ps| ps.get(i))
                    .is_some_and(|t| matches!(t, hir::Ty::Reference(..) | hir::Ty::Pointer(..)));
                let moved = !by_ref
                    && if let hir::ExprKind::Local(src) = a.kind {
                        if b.is_drop_ty(&a.ty) {
                            b.mark_moved(src);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                if !by_ref && !moved && b.is_drop_ty(&a.ty) {
                    b.mark_place_moved(a);
                }
                let v = lower_expr(b, a);
                let v = if moved || by_ref {
                    v
                } else {
                    let v = copy_array_value(b, v, &a.ty);
                    copy_struct_value(b, v, &a.ty)
                };
                mir_args.push(v);
            }
            // 数组/结构体返回值由调用方提供存储：把返回槽地址作为隐藏实参传入。
            let mut dest = None;
            if let Some((_elem, n, size)) = array_info(b, &expr.ty) {
                let dst = b.synthetic_storage(n * size);
                mir_args.push(MirValue::Var(dst.clone()));
                dest = Some(MirValue::Var(dst));
            } else if let Some(layout) = struct_layout(b, &expr.ty) {
                let dst = b.synthetic_storage(layout.size);
                mir_args.push(MirValue::Var(dst.clone()));
                dest = Some(MirValue::Var(dst));
            }
            // 调用表达式的类型就是被调函数的返回类型
            let ty = num_ty_of(&expr.ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Call(name.clone(), fn_name, mir_args, ty));
            let value = dest.unwrap_or(MirValue::Var(name));
            b.register_temp(value.clone(), &expr.ty);
            value
        }
        CallIndirect {
            callee,
            args,
            result,
        } => {
            let callee = lower_expr(b, callee);
            let name = format!("v{}", result.0);
            let mut mir_args = Vec::with_capacity(args.len() + 1);
            for a in args {
                let moved = if let hir::ExprKind::Local(src) = a.kind {
                    if b.is_drop_ty(&a.ty) {
                        b.mark_moved(src);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !moved && b.is_drop_ty(&a.ty) {
                    b.mark_place_moved(a);
                }
                let v = lower_expr(b, a);
                let v = if moved {
                    v
                } else {
                    let v = copy_array_value(b, v, &a.ty);
                    copy_struct_value(b, v, &a.ty)
                };
                mir_args.push(v);
            }
            let mut dest = None;
            if let Some((_elem, n, size)) = array_info(b, &expr.ty) {
                let dst = b.synthetic_storage(n * size);
                mir_args.push(MirValue::Var(dst.clone()));
                dest = Some(MirValue::Var(dst));
            } else if let Some(layout) = struct_layout(b, &expr.ty) {
                let dst = b.synthetic_storage(layout.size);
                mir_args.push(MirValue::Var(dst.clone()));
                dest = Some(MirValue::Var(dst));
            }
            let ty = num_ty_of(&expr.ty);
            b.current_block_mut().instrs.push(MirInstr::CallIndirect(
                name.clone(),
                callee,
                mir_args,
                ty,
            ));
            let value = dest.unwrap_or(MirValue::Var(name));
            b.register_temp(value.clone(), &expr.ty);
            value
        }
        Cast(inner) => {
            // 源类型取自内层表达式，目标类型是节点的 ty。
            let from = num_ty_of(&inner.ty);
            let to = num_ty_of(&expr.ty);
            let v = lower_expr(b, inner);
            let name = format!("t{}", b.temp_id());
            b.current_block_mut()
                .instrs
                .push(MirInstr::Cast(name.clone(), v, from, to));
            MirValue::Var(name)
        }
        Comptime(folded) => {
            // 编译期求值的结果直接变成字面量常量：内层表达式在 HIR 阶段就
            // 被折叠掉了，这里没有原始表达式可降级，`comptime 40 + 2` 到
            // 这一步已经是一个 `Int(42)`。
            mir_literal(folded, &expr.ty)
        }
        Break => {
            // `break` 终结当前块并跳到最内层循环的出口。用 `terminate_if_open`
            // 是为了不覆盖已经终结的块（比如 `if c { break; }` 里 if 自己刚
            // 写完跳转）。
            if let Some(ctx) = b.loops.last().copied() {
                b.emit_drops_from(ctx.drop_depth);
                b.terminate_if_open(MirTerminator::Goto(ctx.exit));
            }
            MirValue::Unit
        }
        Continue => {
            // `continue` 跳回循环头（回边），而不是像 `break` 那样跳出口。
            if let Some(ctx) = b.loops.last().copied() {
                b.emit_drops_from(ctx.drop_depth);
                b.terminate_if_open(MirTerminator::Goto(ctx.header));
            }
            MirValue::Unit
        }
        // `*p`：从指针指向的地址加载。结构体值本身就以地址表示，
        // 解引用只需把地址原样传下去。
        Deref(inner) => {
            let addr = lower_expr(b, inner);
            if struct_layout(b, &expr.ty).is_some() {
                return addr;
            }
            let name = format!("t{}", b.temp_id());
            let ty = num_ty_of(&expr.ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Load(name.clone(), addr, ty));
            MirValue::Var(name)
        }
        // `p[i]` / `s[i]`：算出 `base + i*size` 再从该地址加载。
        // 结构体/数组元素本身以地址表示，直接把这个地址当值。
        Index(recv, idx) => {
            let addr = lower_index_addr(b, recv, idx, &expr.ty);
            if struct_layout(b, &expr.ty).is_some() || array_info(b, &expr.ty).is_some() {
                return addr;
            }
            let name = format!("t{}", b.temp_id());
            let ty = num_ty_of(&expr.ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Load(name.clone(), addr, ty));
            MirValue::Var(name)
        }
        // `&place`：取位置地址。结构体/数组值本身已经是指向存储的地址，
        // 再对“保存该地址的槽”取地址会多解一层。
        AddrOf(place) => {
            if struct_layout(b, &place.ty).is_some() || array_info(b, &place.ty).is_some() {
                return lower_expr(b, place);
            }
            match &place.kind {
                // `&local`：局部变量的栈槽地址。
                Local(id) => {
                    let name = format!("t{}", b.temp_id());
                    b.current_block_mut()
                        .instrs
                        .push(MirInstr::SlotAddr(name.clone(), id.0, 8));
                    MirValue::Var(name)
                }
                // `&*p` 就是 `p`。
                Deref(inner) => lower_expr(b, inner),
                // `&p[i]` 就是 `p + i*size`。
                Index(recv, idx) => lower_index_addr(b, recv, idx, &place.ty),
                // `&s.field`：字段地址。
                Field(recv, name) => {
                    let base = lower_expr(b, recv);
                    let off =
                        struct_layout(b, &recv.ty).and_then(|l| l.offset(name).map(|(off, _)| off));
                    match off {
                        Some(off) => offset_addr(b, base, off),
                        None => MirValue::Unit,
                    }
                }
                _ => MirValue::Unit,
            }
        }
        Yield => {
            b.current_block_mut().instrs.push(MirInstr::Suspend);
            MirValue::Unit
        }
        hir::ExprKind::Asm {
            template,
            outputs,
            inputs,
        } => {
            // 输出操作数是写入位置：局部变量走栈槽地址，`*p` 直接取指针值。
            let mut mir_outputs = Vec::with_capacity(outputs.len());
            for (constraint, out) in outputs {
                let addr = match &out.kind {
                    Local(id) => {
                        let slot = b.temp();
                        b.current_block_mut().instrs.push(MirInstr::SlotAddr(
                            slot.clone(),
                            id.0,
                            8,
                        ));
                        MirValue::Var(slot)
                    }
                    Deref(ptr) => lower_expr(b, ptr),
                    _ => MirValue::Unit,
                };
                mir_outputs.push((constraint.clone(), addr, num_ty_of(&out.ty)));
            }
            let mut mir_inputs = Vec::with_capacity(inputs.len());
            for (constraint, inp) in inputs {
                let ty = num_ty_of(&inp.ty);
                mir_inputs.push((constraint.clone(), lower_expr(b, inp), ty));
            }
            let name = format!("asm{}", b.temp_id());
            b.current_block_mut().instrs.push(MirInstr::Asm {
                name,
                template: template.clone(),
                outputs: mir_outputs,
                inputs: mir_inputs,
            });
            MirValue::Unit
        }
        _ => {
            // 其他表达式（Match/For/Struct 等）骨架阶段降级为占位
            MirValue::Unit
        }
    }
}

/// 把 HIR 字面量转为 MIR 值。
///
/// `ty` 是字面量所在表达式的类型：浮点字面量要由此确定位宽（f32 / f64），
/// `HirLiteral::Float` 自己只存了一个 `f64`。
fn mir_literal(lit: &hir::HirLiteral, ty: &hir::Ty) -> MirValue {
    use hir::HirLiteral::*;
    match lit {
        Int(i) => MirValue::Int(*i),
        Float(f) => match ty {
            hir::Ty::Float(32) => MirValue::Float((*f as f32).to_bits() as u64, 32),
            _ => MirValue::Float(f.to_bits(), 64),
        },
        Bool(b) => MirValue::Bool(*b),
        Char(c) => MirValue::Int(*c as i128),
        String(s) => MirValue::Str {
            content: s.clone(),
            len: s.len(),
        },
        // 编译期结构体字面量应已被折叠成标量；逃到这里说明没被折掉。
        Composite(_) => MirValue::Unit,
    }
}

/// 转换二元运算。
fn convert_binary(op: hir::BinaryOp) -> MirBinaryOp {
    use hir::BinaryOp::*;
    match op {
        Add => MirBinaryOp::Add,
        Sub => MirBinaryOp::Sub,
        Mul => MirBinaryOp::Mul,
        Div => MirBinaryOp::Div,
        Rem => MirBinaryOp::Rem,
        BitAnd => MirBinaryOp::BitAnd,
        BitOr => MirBinaryOp::BitOr,
        BitXor => MirBinaryOp::BitXor,
        Shl => MirBinaryOp::Shl,
        Shr => MirBinaryOp::Shr,
        Eq => MirBinaryOp::Eq,
        Ne => MirBinaryOp::Ne,
        Lt => MirBinaryOp::Lt,
        Gt => MirBinaryOp::Gt,
        Le => MirBinaryOp::Le,
        Ge => MirBinaryOp::Ge,
        And => MirBinaryOp::And,
        Or => MirBinaryOp::Or,
        Range | RangeInclusive => MirBinaryOp::Add,
    }
}

/// 转换一元运算。
fn convert_unary(op: hir::UnaryOp) -> MirUnaryOp {
    use hir::UnaryOp::*;
    match op {
        Neg => MirUnaryOp::Neg,
        Not => MirUnaryOp::Not,
        BitNot => MirUnaryOp::BitNot,
        Deref => MirUnaryOp::Deref,
        AddrOf => MirUnaryOp::Deref, // 取地址骨架阶段占位
    }
}

/// MIR 构建器：管理基本块分配与临时量编号。
/// 一个等待作用域结束析构的局部量。
#[derive(Clone)]
struct DropLocal {
    local: LocalId,
    ty: hir::Ty,
    moved: bool,
    /// 已从该局部量中移出的字段/数组元素路径。路径段是字段声明下标或
    /// 数组下标；析构时跳过这些子树，其余字段仍正常析构。
    moved_paths: Vec<Vec<usize>>,
    /// 动态数组下标的逐元素“已移动”标志。数组每个顶层元素一字节；
    /// `arr[i]` 移动时写入 1，数组析构时跳过对应元素。没有动态下标时
    /// 全为 0，不改变原有析构路径。
    flags: Option<DropFlags>,
}

#[derive(Clone)]
struct DropFlags {
    addr: String,
    len: i128,
}

/// 一个尚未绑定、等待语句结束析构的右值临时量。
#[derive(Clone)]
struct TempDrop {
    value: MirValue,
    ty: hir::Ty,
    moved: bool,
    moved_paths: Vec<Vec<usize>>,
}

struct Builder {
    blocks: Vec<MirBasicBlock>,
    current: usize,
    param_names: Vec<String>,
    next_temp: u32,
    /// DefId → 函数名（由降级入口注入）。
    functions: HashMap<hir::DefId, String>,
    /// 函数名 → 形参类型。实参按引用传递时要直接传地址，不能复制/移动。
    fn_params: HashMap<String, Vec<hir::Ty>>,
    /// 正在展开的循环（由内到外）：`(回边目标块, 出口块)`。
    ///
    /// `break` / `continue` 本身是表达式，拿不到"我在哪个循环里"，只能靠
    /// 降级时维护的这份栈。之前它们落进 `_ => Unit` 的兜底分支，等价于
    /// 什么都没做——`loop { ... break; }` 于是**编译通过但永远转不出来**。
    loops: Vec<LoopCtx>,
    /// DefId → 常量的值（MIR 值形式）。
    ///
    /// 引用 `const` 时把值直接内联进指令流：常量在编译期就定下来了，
    /// 运行时既没有存储也没有读取动作。
    const_values: HashMap<hir::DefId, MirValue>,
    /// 类型定义表：结构体布局与字段偏移需要它。
    types: HashMap<hir::DefId, hir::HirTypeDef>,
    /// 类型 DefId → 用户 Drop 析构函数名。
    drops: HashMap<hir::DefId, String>,
    /// 词法作用域栈，每层记录该作用域内需要 Drop 的局部量。
    drop_scopes: Vec<Vec<DropLocal>>,
    /// 合成局部 id 的起点（数组字面量等没有源码局部变量的存储）。
    next_synthetic: u32,
    /// 被取过地址（`&x`）的局部变量 id。
    ///
    /// 这些变量不能再当作寄存器里的 SSA 值——取地址意味着它们的身份是
    /// 一块内存。降级时把它们固定到栈槽，读写都变成显式的 Load/Store。
    addr_taken: HashSet<u32>,
    /// 同步函数返回数组时，调用方传入的隐藏返回槽类型 `(元素, 长度)`。
    ret_array: Option<(hir::Ty, i128)>,
    /// 隐藏返回槽对应的 MIR 参数名。
    hidden_ret: Option<String>,
    /// 同步函数按值返回结构体时的 `(类型, 字节大小)`。
    ret_struct: Option<(hir::Ty, i128)>,
    /// 当前语句创建的 Drop 临时量；语句结束时逆序析构未移出的项。
    temp_drops: Vec<TempDrop>,
    /// 异步函数按值返回聚合类型时的输出字节数。
    async_ret_bytes: Option<usize>,
}

/// 把常量表折成 `DefId -> MirValue`。
///
/// 只收**字面量**初值：`const X: T = f(2)` 之类的复合初值在 HIR 里不是字面量，
/// 引用它的地方降级成 unit（与"无法内联"同义），而不是猜一个值。
fn const_values(consts: &HashMap<hir::DefId, HirConst>) -> HashMap<hir::DefId, MirValue> {
    let mut out = HashMap::with_capacity(consts.len());
    for (id, c) in consts {
        if let hir::ExprKind::Literal(lit) = &c.value.kind {
            out.insert(*id, mir_literal(lit, &c.value.ty));
        } else if let hir::ExprKind::Comptime(folded) = &c.value.kind {
            // comptime 求值出来的常量：同样是一条字面量。
            out.insert(*id, mir_literal(folded, &c.value.ty));
        }
    }
    out
}

/// 一个 `break` / `continue` 各自的目标块。
#[derive(Clone, Copy)]
struct LoopCtx {
    /// `continue` 的落点（循环头）。
    header: usize,
    /// `break` 的落点（循环出口）。
    exit: usize,
    /// 进入循环体前的作用域深度；break/continue 只清理更内层的作用域。
    drop_depth: usize,
}

impl Builder {
    fn new(
        name: String,
        functions: HashMap<hir::DefId, String>,
        const_values: HashMap<hir::DefId, MirValue>,
        types: HashMap<hir::DefId, hir::HirTypeDef>,
        drops: HashMap<hir::DefId, String>,
        fn_params: HashMap<String, Vec<hir::Ty>>,
    ) -> Self {
        let _ = name;
        Self {
            blocks: Vec::new(),
            current: 0,
            param_names: Vec::new(),
            next_temp: 0,
            loops: Vec::new(),
            functions,
            fn_params,
            const_values,
            types,
            drops,
            drop_scopes: Vec::new(),
            next_synthetic: 0,
            addr_taken: HashSet::new(),
            ret_array: None,
            hidden_ret: None,
            ret_struct: None,
            temp_drops: Vec::new(),
            async_ret_bytes: None,
        }
    }

    /// 新的临时量名。
    fn temp(&mut self) -> String {
        format!("t{}", self.temp_id())
    }

    /// 定义一个局部变量：被取地址的写进栈槽，否则是普通寄存器名。
    fn def_local(&mut self, id: u32, value: MirValue, ty: NumTy) {
        if self.addr_taken.contains(&id) {
            let a = self.temp();
            self.current_block_mut()
                .instrs
                .push(MirInstr::SlotAddr(a.clone(), id, 8));
            self.current_block_mut()
                .instrs
                .push(MirInstr::Store(MirValue::Var(a), value, ty));
        } else {
            self.current_block_mut()
                .instrs
                .push(MirInstr::Assign(format!("v{id}"), value));
        }
    }

    /// 读一个局部变量的值：被取地址的从栈槽加载，否则直接用寄存器名。
    fn use_local(&mut self, id: u32, ty: NumTy) -> MirValue {
        if self.addr_taken.contains(&id) {
            let a = self.temp();
            self.current_block_mut()
                .instrs
                .push(MirInstr::SlotAddr(a.clone(), id, 8));
            let t = self.temp();
            self.current_block_mut()
                .instrs
                .push(MirInstr::Load(t.clone(), MirValue::Var(a), ty));
            MirValue::Var(t)
        } else {
            MirValue::Var(format!("v{id}"))
        }
    }

    /// 只在当前块还没有终结符时补一个。
    fn terminate_if_open(&mut self, term: MirTerminator) {
        if self.current_block_mut().term.is_none() {
            self.current_block_mut().term = Some(term);
        }
    }

    /// 块还没终结时才追加赋值——块已经 `return` 的话不能再插指令。
    fn assign_if_open(&mut self, name: &str, val: MirValue) {
        self.consume_temp(&val);
        if self.current_block_mut().term.is_none() {
            self.current_block_mut()
                .instrs
                .push(MirInstr::Assign(name.to_string(), val));
        }
    }

    fn new_block(&mut self) -> usize {
        let id = self.blocks.len();
        self.blocks.push(MirBasicBlock {
            id,
            instrs: Vec::new(),
            // 新建的块还没终结；函数体收尾时会补默认 Return。
            term: None,
        });
        id
    }

    fn current_block_mut(&mut self) -> &mut MirBasicBlock {
        &mut self.blocks[self.current]
    }

    fn temp_id(&mut self) -> u32 {
        let t = self.next_temp;
        self.next_temp += 1;
        t
    }

    /// 为数组字面量分配一块合成栈槽，返回其地址临时名。
    ///
    /// `bytes` 决定后端起栈帧时要预留多大空间；不给每个数组元素单独发
    /// local id，避免把连续元素拆散。
    fn is_drop_ty(&self, ty: &hir::Ty) -> bool {
        self.has_drop_glue(ty, 0)
    }

    fn has_drop_glue(&self, ty: &hir::Ty, depth: usize) -> bool {
        if depth > 16 {
            return false;
        }
        match ty {
            hir::Ty::Array(elem, _) => self.has_drop_glue(elem, depth + 1),
            hir::Ty::Named(id, _) => {
                if self.drops.contains_key(id) {
                    return true;
                }
                let Some(hir::HirTypeDef::Struct(hs)) = self.types.get(id) else {
                    return false;
                };
                hs.fields
                    .iter()
                    .any(|f| self.has_drop_glue(&f.ty, depth + 1))
            }
            _ => false,
        }
    }

    fn track_drop(&mut self, local: LocalId, ty: &hir::Ty) {
        if std::env::var_os("ZR_DEBUG_DROP").is_some() {
            eprintln!(
                "track_drop local={} ty={:?} yes={}",
                local.0,
                ty,
                self.is_drop_ty(ty)
            );
        }
        if !self.is_drop_ty(ty) {
            return;
        }
        let flags = if let hir::Ty::Array(_, len) = ty {
            hir::array_len_value(len).filter(|n| *n > 0).map(|n| {
                let addr = self.synthetic_storage(n);
                for i in 0..n {
                    let at = offset_addr(self, MirValue::Var(addr.clone()), i);
                    self.current_block_mut().instrs.push(MirInstr::Store(
                        at,
                        MirValue::Int(0),
                        NumTy::Uint(8),
                    ));
                }
                DropFlags { addr, len: n }
            })
        } else {
            None
        };
        if let Some(scope) = self.drop_scopes.last_mut() {
            scope.push(DropLocal {
                local,
                ty: ty.clone(),
                moved: false,
                moved_paths: Vec::new(),
                flags,
            });
        }
    }

    /// 标记“整个局部量”已移动。
    fn mark_place_moved(&mut self, expr: &HirExpr) {
        // 动态下标移动：`arr[i]`（i 为局部量/字面量）。为每个元素维护
        // 一字节标志，析构时逐个检查，因此移动一个元素不会连累兄弟元素。
        if let hir::ExprKind::Index(base, idx) = &expr.kind {
            if let hir::ExprKind::Local(local) = &base.kind {
                if matches!(
                    idx.kind,
                    hir::ExprKind::Local(_) | hir::ExprKind::Literal(_)
                ) {
                    let flags = self
                        .drop_scopes
                        .iter()
                        .rev()
                        .flat_map(|s| s.iter().rev())
                        .find(|e| e.local == *local)
                        .and_then(|e| e.flags.clone());
                    if let Some(flags) = flags {
                        let index = lower_expr(self, idx);
                        let at =
                            binary_into(self, MirValue::Var(flags.addr), MirBinaryOp::Add, index);
                        self.current_block_mut().instrs.push(MirInstr::Store(
                            at,
                            MirValue::Int(1),
                            NumTy::Uint(8),
                        ));
                        return;
                    }
                }
            }
        }
        let Some((local, path)) = place_move_path(self, expr) else {
            if std::env::var_os("ZR_DEBUG_DROP").is_some() {
                eprintln!("partial-move: no path");
            }
            return;
        };
        if std::env::var_os("ZR_DEBUG_DROP").is_some() {
            eprintln!("partial-move local={} path={:?}", local.0, path);
        }
        if path.is_empty() {
            self.mark_moved(local);
            return;
        }
        // 直接实现 Drop 的类型不允许部分移动（否则用户 drop 会看到已移出的
        // 字段）。这里保守地把整个局部量视为已移动，避免重复析构。
        if self.local_has_direct_drop(local) {
            self.mark_moved(local);
            return;
        }
        for scope in self.drop_scopes.iter_mut().rev() {
            for entry in scope.iter_mut().rev() {
                if entry.local == local {
                    if !entry.moved_paths.contains(&path) {
                        entry.moved_paths.push(path.clone());
                    }
                    if std::env::var_os("ZR_DEBUG_DROP").is_some() {
                        eprintln!(
                            "partial stored local={} now={:?}",
                            local.0, entry.moved_paths
                        );
                    }
                    return;
                }
            }
        }
    }

    fn local_has_direct_drop(&self, local: LocalId) -> bool {
        self.drop_scopes
            .iter()
            .rev()
            .flat_map(|s| s.iter().rev())
            .find(|e| e.local == local)
            .is_some_and(|e| match &e.ty {
                hir::Ty::Named(id, _) => self.drops.contains_key(id),
                _ => false,
            })
    }

    /// 取出并清除一个已移动路径；返回 true 表示该字段/元素没有现存值，
    /// 覆盖赋值时不需要（也不能）先析构。
    fn take_moved_path(&mut self, local: LocalId, path: &[usize]) -> bool {
        for scope in self.drop_scopes.iter_mut().rev() {
            for entry in scope.iter_mut().rev() {
                if entry.local == local {
                    if let Some(pos) = entry.moved_paths.iter().position(|p| p == path) {
                        entry.moved_paths.remove(pos);
                        return true;
                    }
                    return false;
                }
            }
        }
        false
    }

    fn mark_moved(&mut self, local: LocalId) {
        for scope in self.drop_scopes.iter_mut().rev() {
            for entry in scope.iter_mut().rev() {
                if entry.local == local {
                    entry.moved = true;
                    entry.moved_paths.clear();
                    return;
                }
            }
        }
    }

    /// 赋值覆盖一个 Drop 局部量：析构旧值，并把该局部量重新标记为存活。
    fn replace_drop_local(&mut self, local: u32, ty: &hir::Ty) {
        let mut found: Option<(usize, usize, bool, hir::Ty)> = None;
        for (si, scope) in self.drop_scopes.iter().enumerate().rev() {
            for (ei, entry) in scope.iter().enumerate().rev() {
                if entry.local.0 == local {
                    found = Some((si, ei, entry.moved, entry.ty.clone()));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        match found {
            Some((si, ei, moved, ty)) => {
                if !moved {
                    self.emit_drop(LocalId(local), &ty);
                }
                self.drop_scopes[si][ei].moved = false;
                self.drop_scopes[si][ei].moved_paths.clear();
            }
            None => self.track_drop(LocalId(local), ty),
        }
    }

    fn emit_drop_scope(&mut self, mut scope: Vec<DropLocal>) {
        if std::env::var_os("ZR_DEBUG_DROP").is_some() {
            eprintln!(
                "emit scope len={} term={}",
                scope.len(),
                self.current_block_mut().term.is_some()
            );
        }
        if self.current_block_mut().term.is_some() {
            return;
        }
        while let Some(entry) = scope.pop() {
            self.emit_drop_entry(&entry);
        }
    }

    fn emit_drops_from(&mut self, depth: usize) {
        if self.current_block_mut().term.is_some() {
            return;
        }
        for idx in (depth..self.drop_scopes.len()).rev() {
            let scope = self.drop_scopes[idx].clone();
            for entry in scope.iter().rev() {
                self.emit_drop_entry(entry);
            }
        }
    }

    fn emit_all_drops(&mut self) {
        self.emit_drops_from(0);
    }

    fn register_temp(&mut self, value: MirValue, ty: &hir::Ty) {
        if self.is_drop_ty(ty) {
            if std::env::var_os("ZR_DEBUG_DROP").is_some() {
                eprintln!("temp register {:?} {:?}", value, ty);
            }
            self.temp_drops.push(TempDrop {
                value,
                ty: ty.clone(),
                moved: false,
                moved_paths: Vec::new(),
            });
        }
    }

    /// 从临时结构体中移出一个字段：记录路径，返回是否找到了对应的临时量。
    fn mark_temp_moved_path(&mut self, value: &MirValue, idx: usize) -> bool {
        let MirValue::Var(name) = value else {
            return false;
        };
        for temp in self.temp_drops.iter_mut().rev() {
            if let MirValue::Var(n) = &temp.value {
                if n == name {
                    if !temp.moved_paths.contains(&vec![idx]) {
                        temp.moved_paths.push(vec![idx]);
                    }
                    return true;
                }
            }
        }
        false
    }

    /// 右值临时量被复制进绑定/字段/实参后，原临时量不再负责析构。
    fn consume_temp(&mut self, value: &MirValue) {
        let MirValue::Var(name) = value else {
            return;
        };
        for temp in self.temp_drops.iter_mut().rev() {
            if let MirValue::Var(n) = &temp.value {
                if n == name {
                    temp.moved = true;
                    return;
                }
            }
        }
    }

    /// 语句结束时逆序析构本语句遗留的临时量。
    fn emit_temp_drops_from(&mut self, mark: usize) {
        if mark >= self.temp_drops.len() {
            return;
        }
        let temps = self.temp_drops.split_off(mark);
        if std::env::var_os("ZR_DEBUG_DROP").is_some() {
            eprintln!("temp cleanup mark={} temps={}", mark, temps.len());
        }
        if self.current_block_mut().term.is_some() {
            return;
        }
        for temp in temps.iter().rev() {
            if !temp.moved {
                self.emit_drop_at_moved(
                    temp.value.clone(),
                    &temp.ty,
                    0,
                    &[],
                    &temp.moved_paths,
                    None,
                );
            }
        }
    }

    fn emit_drop_entry(&mut self, entry: &DropLocal) {
        if entry.moved {
            return;
        }
        let value = self.use_local(entry.local.0, num_ty_of(&entry.ty));
        self.emit_drop_at_moved(
            value,
            &entry.ty,
            0,
            &[],
            &entry.moved_paths,
            entry.flags.as_ref(),
        );
    }

    fn emit_drop(&mut self, local: LocalId, ty: &hir::Ty) {
        let paths = self
            .drop_scopes
            .iter()
            .rev()
            .flat_map(|s| s.iter().rev())
            .find(|e| e.local == local)
            .map(|e| (e.moved, e.moved_paths.clone(), e.flags.clone()))
            .unwrap_or((false, Vec::new(), None));
        if paths.0 {
            return;
        }
        if std::env::var_os("ZR_DEBUG_DROP").is_some() {
            eprintln!("emit local={} paths={:?}", local.0, paths.1);
        }
        let value = self.use_local(local.0, num_ty_of(ty));
        self.emit_drop_at_moved(value, ty, 0, &[], &paths.1, paths.2.as_ref());
    }

    /// 对一块存储执行析构：先调用用户 `Drop::drop`，再递归析构字段。
    /// 没有直接 impl 但包含 Drop 字段的结构体也会自动得到字段析构。
    fn emit_drop_at(&mut self, addr: MirValue, ty: &hir::Ty, depth: usize) {
        self.emit_drop_at_moved(addr, ty, depth, &[], &[], None);
    }

    fn emit_drop_at_moved(
        &mut self,
        addr: MirValue,
        ty: &hir::Ty,
        depth: usize,
        path: &[usize],
        moved_paths: &[Vec<usize>],
        flags: Option<&DropFlags>,
    ) {
        if depth > 16 {
            return;
        }
        // 数组按索引顺序逐元素析构（与 Rust 一致；结构体字段仍按声明顺序）。
        if let hir::Ty::Array(elem, len) = ty {
            let Some(n) = hir::array_len_value(len) else {
                return;
            };
            let size = type_size(self, elem);
            let guarded = flags.filter(|f| depth == 0 && f.len == n);
            for i in 0..n {
                let mut child = path.to_vec();
                child.push(i as usize);
                if moved_paths.contains(&child) {
                    continue;
                }
                let at = if i == 0 {
                    addr.clone()
                } else {
                    offset_addr(self, addr.clone(), i * size)
                };
                if let Some(flags) = guarded {
                    if self.current_block_mut().term.is_none() {
                        let flag_at = offset_addr(self, MirValue::Var(flags.addr.clone()), i);
                        let flag = self.temp();
                        self.current_block_mut().instrs.push(MirInstr::Load(
                            flag.clone(),
                            flag_at,
                            NumTy::Uint(8),
                        ));
                        let alive = self.temp();
                        self.current_block_mut().instrs.push(MirInstr::Binary(
                            alive.clone(),
                            MirValue::Var(flag),
                            MirBinaryOp::Eq,
                            MirValue::Int(0),
                            NumTy::Uint(8),
                        ));
                        let drop_bb = self.new_block();
                        let next_bb = self.new_block();
                        self.current_block_mut().term = Some(MirTerminator::Branch {
                            cond: MirValue::Var(alive),
                            then_block: drop_bb,
                            else_block: next_bb,
                        });
                        self.current = drop_bb;
                        self.emit_drop_at_moved(at, elem, depth + 1, &child, moved_paths, None);
                        self.terminate_if_open(MirTerminator::Goto(next_bb));
                        self.current = next_bb;
                        continue;
                    }
                }
                self.emit_drop_at_moved(at, elem, depth + 1, &child, moved_paths, None);
            }
            return;
        }
        let hir::Ty::Named(id, _) = ty else {
            return;
        };
        if let Some(callee) = self.drops.get(id).cloned() {
            let dst = self.temp();
            self.current_block_mut().instrs.push(MirInstr::Call(
                dst,
                callee,
                vec![addr.clone()],
                NumTy::Uint(64),
            ));
        }
        let Some(layout) = struct_layout(self, ty) else {
            return;
        };
        for (idx, (off, field_ty)) in layout.fields.iter().enumerate() {
            let mut child = path.to_vec();
            child.push(idx);
            if moved_paths.contains(&child) {
                continue;
            }
            if self.has_drop_glue(field_ty, depth + 1) {
                let at = offset_addr(self, addr.clone(), *off);
                self.emit_drop_at_moved(at, field_ty, depth + 1, &child, moved_paths, None);
            }
        }
    }

    fn synthetic_storage(&mut self, bytes: i128) -> String {
        let id = 0x4000_0000 + self.next_synthetic;
        self.next_synthetic += 1;
        let addr = self.temp();
        self.current_block_mut().instrs.push(MirInstr::SlotAddr(
            addr.clone(),
            id,
            bytes.max(8) as u32,
        ));
        addr
    }
}

/// HIR 类型 -> MIR 数值类型描述。
///
/// 转换指令要靠它决定符号扩展还是零扩展，拿不到宽度就只能退化成
/// "什么都不做"——那正是 `x as i32` 变成 0 的原因。
/// 这个 [`NumTy`] 是不是"真的"推断出来了（而不是 [`num_ty_of`] 的兜底值）。
///
/// [`num_ty_of`] 对不认识的类型一律回退成 `Int(64)`，所以没法直接拿它判断
/// "有没有类型"。这里单独判一次。
fn is_resolved(ty: &NumTy) -> bool {
    !matches!(ty, NumTy::Int(64) | NumTy::Uint(64))
}

/// 二元运算的结果类型。
///
/// 类型检查器偶尔会把 `f64 * f64` 留成一个没解出来的推断变量（尤其是运算
/// 结果直接喂给比较、没有 `let` 标注的时候）。这时 `num_ty_of` 兜底成
/// `Int(64)`，后端就会按整数指令编码浮点运算——位模式直接相加，"能跑"，
/// 结果是错的。所以表达式类型不可信时，退回看操作数的类型。
///
/// **比较与逻辑运算单独处理**：它们的结果恒为 `Bool`，不能退回看操作数——
/// `f < g` 的操作数是浮点，但结果是布尔。误当成浮点会让后端把布尔值塞进
/// 浮点栈槽，`Not` 也会被编成翻符号位。
fn binary_result_ty(
    op: hir::BinaryOp,
    expr_ty: &hir::Ty,
    lhs_ty: &hir::Ty,
    rhs_ty: &hir::Ty,
) -> NumTy {
    use hir::BinaryOp::*;
    if matches!(op, Eq | Ne | Lt | Gt | Le | Ge | And | Or) {
        return NumTy::Bool;
    }
    let t = num_ty_of(expr_ty);
    if is_resolved(&t) {
        return t;
    }
    let l = num_ty_of(lhs_ty);
    if is_resolved(&l) {
        return l;
    }
    let r = num_ty_of(rhs_ty);
    if is_resolved(&r) {
        return r;
    }
    t
}

fn num_ty_of(ty: &hir::Ty) -> NumTy {
    use hir::Ty;
    match ty {
        Ty::Int(w) => NumTy::Int(*w as u8),
        Ty::Uint(w) => NumTy::Uint(*w as u8),
        Ty::Float(w) => NumTy::Float(*w as u8),
        Ty::Bool => NumTy::Bool,
        _ => NumTy::Int(64),
    }
}

/// 元素类型的字节大小（用于索引缩放 `p + i*size`）。
///
/// 只处理本后端支持的标量；未知类型按 1 字节算（字符串字节正是这种情况）。
fn element_size(ty: &hir::Ty) -> i128 {
    use hir::Ty;
    match ty {
        Ty::Int(w) | Ty::Uint(w) => (*w as i128) / 8,
        Ty::Float(w) => (*w as i128) / 8,
        Ty::Bool | Ty::Char => 1,
        Ty::Pointer(..) | Ty::Reference(..) | Ty::Str | Ty::Slice(..) | Ty::Function { .. } => 8,
        _ => 1,
    }
}

/// 结构体布局：字段偏移、尺寸与对齐。
struct StructLayout {
    /// (字节偏移, 字段类型)，按声明顺序。
    fields: Vec<(i128, hir::Ty)>,
    /// 字段名 → 声明下标（元组字段写成 `"0"`、`"1"`）。
    names: HashMap<String, usize>,
    size: i128,
}

impl StructLayout {
    fn offset(&self, name: &str) -> Option<(i128, &hir::Ty)> {
        let idx = self
            .names
            .get(name)
            .copied()
            .or_else(|| name.parse::<usize>().ok())?;
        self.fields.get(idx).map(|(off, ty)| (*off, ty))
    }
}

fn align_up(n: i128, align: i128) -> i128 {
    if align <= 1 {
        n
    } else {
        (n + align - 1) / align * align
    }
}

fn type_size_align(b: &Builder, ty: &hir::Ty, depth: usize) -> Option<(i128, i128)> {
    use hir::Ty;
    if depth > 32 {
        return None;
    }
    Some(match ty {
        Ty::Int(w) | Ty::Uint(w) | Ty::Float(w) => {
            let bytes = (*w as i128) / 8;
            (bytes, bytes)
        }
        Ty::Bool | Ty::Char => (1, 1),
        Ty::Unit | Ty::Never => (0, 1),
        Ty::Pointer(..) | Ty::Reference(..) | Ty::Str | Ty::Slice(..) | Ty::Function { .. } => {
            (8, 8)
        }
        Ty::Array(elem, len) => {
            let n = hir::array_len_value(len)?;
            let (sz, al) = type_size_align(b, elem, depth + 1)?;
            (sz * n, al)
        }
        Ty::Named(id, _) => {
            let hs = match b.types.get(id)? {
                hir::HirTypeDef::Struct(hs) => hs,
                _ => return None,
            };
            let mut off = 0i128;
            let mut align = 1i128;
            for f in &hs.fields {
                let (sz, al) = type_size_align(b, &f.ty, depth + 1)?;
                off = align_up(off, al);
                off += sz;
                align = align.max(al);
            }
            (align_up(off, align), align)
        }
        _ => return None,
    })
}

fn struct_layout(b: &Builder, ty: &hir::Ty) -> Option<StructLayout> {
    let id = match ty {
        hir::Ty::Named(id, _) => *id,
        hir::Ty::Reference(inner, _) | hir::Ty::Pointer(inner, _) => {
            return struct_layout(b, inner);
        }
        _ => return None,
    };
    let hs = match b.types.get(&id)? {
        hir::HirTypeDef::Struct(hs) => hs,
        _ => return None,
    };
    let mut fields = Vec::with_capacity(hs.fields.len());
    let mut names = HashMap::new();
    let mut off = 0i128;
    let mut align = 1i128;
    for (idx, f) in hs.fields.iter().enumerate() {
        let (sz, al) = type_size_align(b, &f.ty, 0)?;
        off = align_up(off, al);
        fields.push((off, f.ty.clone()));
        names.insert(f.name.clone(), idx);
        off += sz;
        align = align.max(al);
    }
    Some(StructLayout {
        size: align_up(off, align).max(0),
        fields,
        names,
    })
}

/// 把值写进一个字段/元素位置，结构体和数组递归复制。
fn store_value(b: &mut Builder, at: MirValue, value: MirValue, ty: &hir::Ty) {
    b.consume_temp(&value);
    if let Some(layout) = struct_layout(b, ty) {
        copy_struct_into(b, value, &layout, at);
        return;
    }
    if let Some((elem, n, _)) = array_info(b, ty) {
        copy_array_into(b, value, &elem, n, at);
        return;
    }
    let ty = num_ty_of(ty);
    b.current_block_mut()
        .instrs
        .push(MirInstr::Store(at, value, ty));
}

/// 把结构体源地址的内容复制到目标地址。
fn copy_struct_into(b: &mut Builder, src: MirValue, layout: &StructLayout, dst: MirValue) {
    for (off, field_ty) in &layout.fields {
        let s = offset_addr(b, src.clone(), *off);
        let d = offset_addr(b, dst.clone(), *off);
        if let Some(nested) = struct_layout(b, field_ty) {
            copy_struct_into(b, s, &nested, d);
        } else if let Some((elem, n, _)) = array_info(b, field_ty) {
            copy_array_into(b, s, &elem, n, d);
        } else {
            let tmp = format!("t{}", b.temp_id());
            b.current_block_mut()
                .instrs
                .push(MirInstr::Load(tmp.clone(), s, num_ty_of(field_ty)));
            b.current_block_mut().instrs.push(MirInstr::Store(
                d,
                MirValue::Var(tmp),
                num_ty_of(field_ty),
            ));
        }
    }
}

/// 结构体按值拷贝：返回一块新存储的地址。
fn copy_struct_value(b: &mut Builder, src: MirValue, ty: &hir::Ty) -> MirValue {
    let Some(layout) = struct_layout(b, ty) else {
        return src;
    };
    b.consume_temp(&src);
    let dst = b.synthetic_storage(layout.size);
    copy_struct_into(b, src, &layout, MirValue::Var(dst.clone()));
    MirValue::Var(dst)
}

/// 地址 + 字节偏移；偏移为 0 时直接复用原地址。
fn offset_addr(b: &mut Builder, base: MirValue, off: i128) -> MirValue {
    if off == 0 {
        base
    } else {
        binary_into(b, base, MirBinaryOp::Add, MirValue::Int(off))
    }
}

/// 数组按值拷贝：把源数组的每个元素读到新栈槽，返回新地址。
///
/// zr 的数组运行时表示是指向元素存储的地址；如果直接移动这个地址，
/// `let b = a`、数组实参和赋值都会变成别名。Rust 的 `[T; N]` 是值语义，
/// 这里用定长展开复制来保持“修改副本不影响原数组”。
fn copy_array_value(b: &mut Builder, src: MirValue, ty: &hir::Ty) -> MirValue {
    let Some((elem, n, size)) = array_info(b, ty) else {
        return src;
    };
    b.consume_temp(&src);
    if n <= 0 {
        return src;
    }
    let dst = b.synthetic_storage(n * size);
    copy_array_into(b, src, &elem, n, MirValue::Var(dst.clone()));
    MirValue::Var(dst)
}

/// 把源数组逐元素复制到指定返回槽/存储地址。
fn copy_array_into(b: &mut Builder, src: MirValue, elem_ty: &hir::Ty, len: i128, dst: MirValue) {
    if len <= 0 {
        return;
    }
    let size = type_size(b, elem_ty);
    for i in 0..len {
        let off = i * size;
        let src_at = if off == 0 {
            src.clone()
        } else {
            binary_into(b, src.clone(), MirBinaryOp::Add, MirValue::Int(off))
        };
        let dst_at = if off == 0 {
            dst.clone()
        } else {
            binary_into(b, dst.clone(), MirBinaryOp::Add, MirValue::Int(off))
        };
        if let Some(layout) = struct_layout(b, elem_ty) {
            copy_struct_into(b, src_at, &layout, dst_at);
        } else if let Some((inner, inner_len, _)) = array_info(b, elem_ty) {
            copy_array_into(b, src_at, &inner, inner_len, dst_at);
        } else {
            let elem = b.temp();
            b.current_block_mut().instrs.push(MirInstr::Load(
                elem.clone(),
                src_at,
                num_ty_of(elem_ty),
            ));
            b.current_block_mut().instrs.push(MirInstr::Store(
                dst_at,
                MirValue::Var(elem),
                num_ty_of(elem_ty),
            ));
        }
    }
}

/// 任意类型在栈上的字节大小；结构体/嵌套数组按真实布局计算。
fn type_size(b: &Builder, ty: &hir::Ty) -> i128 {
    type_size_align(b, ty, 0)
        .map(|(size, _)| size)
        .unwrap_or_else(|| element_size(ty))
}

/// 数组类型 -> (元素类型, 长度, 元素字节数)；非定长数组返回 None。
fn array_info(b: &Builder, ty: &hir::Ty) -> Option<(hir::Ty, i128, i128)> {
    let hir::Ty::Array(elem, len) = ty else {
        return None;
    };
    let n = hir::array_len_value(len)?;
    Some(((**elem).clone(), n, type_size(b, elem)))
}

/// 降级索引地址：`base + index * size(base 的元素类型)`。
///
/// `elem_ty` 是**元素**类型（不是 receiver 类型），用来定缩放系数：
/// `s[i]`（str）缩放 1，`p[i]`（`*i32`）缩放 4。
fn lower_index_addr(b: &mut Builder, recv: &HirExpr, idx: &HirExpr, elem_ty: &hir::Ty) -> MirValue {
    let base = lower_expr(b, recv);
    let index = lower_expr(b, idx);
    let size = type_size(b, elem_ty);
    let scaled = if size == 1 {
        index
    } else {
        let name = b.temp();
        b.current_block_mut().instrs.push(MirInstr::Binary(
            name.clone(),
            index,
            MirBinaryOp::Mul,
            MirValue::Int(size),
            NumTy::Int(64),
        ));
        MirValue::Var(name)
    };
    let name = b.temp();
    b.current_block_mut().instrs.push(MirInstr::Binary(
        name.clone(),
        base,
        MirBinaryOp::Add,
        scaled,
        NumTy::Int(64),
    ));
    MirValue::Var(name)
}

/// 类型是不是指针 / 引用（运行期都是同一个"地址"）。
fn is_pointer_ty(ty: &hir::Ty) -> bool {
    matches!(ty, hir::Ty::Pointer(..) | hir::Ty::Reference(..))
}

/// 指针指向元素的字节大小，用于 `p + n` 的偏移缩放。
fn pointee_element_size(ty: &hir::Ty) -> i128 {
    match ty {
        hir::Ty::Pointer(inner, _) | hir::Ty::Reference(inner, _) => element_size(inner),
        _ => 1,
    }
}

/// 把某个 `MirValue` 与常量一起发一条 `MirInstr::Binary`，返回临时名。
/// 数组逐元素二元运算：`[a, b] + [c, d] == [a+c, b+d]`。
///
/// 128 位且元素类型/运算受支持时发 `SimdBinary`（x86-64 走 SSE2）；
/// 其余整数数组按元素展开成 Load/Binary/Store。数组的 MIR 值是指向元素
/// 存储的地址，所以这里操作的是地址而不是逐个 array 值。
fn lower_array_binary(
    b: &mut Builder,
    op: hir::BinaryOp,
    lhs: &HirExpr,
    rhs: &HirExpr,
    ty: &hir::Ty,
) -> Option<MirValue> {
    let arr_ty = match ty {
        hir::Ty::Array(..) => ty,
        _ => match &lhs.ty {
            hir::Ty::Array(..) => &lhs.ty,
            _ => return None,
        },
    };
    let (elem, len) = match arr_ty {
        hir::Ty::Array(elem, len) => (&**elem, hir::array_len_value(len)?),
        _ => return None,
    };
    if len <= 0 || len > 4096 {
        return None;
    }
    let size = element_size(elem);
    let bytes = len * size;
    let mir_op = convert_binary(op);

    if bytes == 16 && simd_supported(elem, mir_op) {
        let l = lower_expr(b, lhs);
        let r = lower_expr(b, rhs);
        let out = b.synthetic_storage(bytes);
        b.current_block_mut().instrs.push(MirInstr::SimdBinary {
            lhs: l,
            op: mir_op,
            rhs: r,
            out: MirValue::Var(out.clone()),
            ty: num_ty_of(elem),
            lanes: len as u32,
        });
        return Some(MirValue::Var(out));
    }

    let scalar = matches!(
        mir_op,
        MirBinaryOp::Add
            | MirBinaryOp::Sub
            | MirBinaryOp::Mul
            | MirBinaryOp::Div
            | MirBinaryOp::Rem
            | MirBinaryOp::BitAnd
            | MirBinaryOp::BitOr
            | MirBinaryOp::BitXor
            | MirBinaryOp::Shl
            | MirBinaryOp::Shr
    );
    if !matches!(elem, hir::Ty::Int(_) | hir::Ty::Uint(_)) || !scalar {
        return None;
    }

    let l = lower_expr(b, lhs);
    let r = lower_expr(b, rhs);
    let out = b.synthetic_storage(bytes);
    let elem_ty = num_ty_of(elem);
    for i in 0..len {
        let off = i * size;
        let la = addr_offset(b, &l, off);
        let ra = addr_offset(b, &r, off);
        let oa = addr_offset(b, &MirValue::Var(out.clone()), off);
        let lv = b.temp();
        b.current_block_mut()
            .instrs
            .push(MirInstr::Load(lv.clone(), la, elem_ty));
        let rv = b.temp();
        b.current_block_mut()
            .instrs
            .push(MirInstr::Load(rv.clone(), ra, elem_ty));
        let res = b.temp();
        b.current_block_mut().instrs.push(MirInstr::Binary(
            res.clone(),
            MirValue::Var(lv),
            mir_op,
            MirValue::Var(rv),
            elem_ty,
        ));
        b.current_block_mut()
            .instrs
            .push(MirInstr::Store(oa, MirValue::Var(res), elem_ty));
    }
    Some(MirValue::Var(out))
}

/// 该元素类型 + 运算是否有 packed SIMD 指令可用（当前对应 128 位向量）。
fn simd_supported(elem: &hir::Ty, op: MirBinaryOp) -> bool {
    use hir::Ty;
    let int_ops = matches!(
        op,
        MirBinaryOp::Add
            | MirBinaryOp::Sub
            | MirBinaryOp::BitAnd
            | MirBinaryOp::BitOr
            | MirBinaryOp::BitXor
    );
    let float_ops = matches!(
        op,
        MirBinaryOp::Add | MirBinaryOp::Sub | MirBinaryOp::Mul | MirBinaryOp::Div
    );
    match elem {
        Ty::Float(32) | Ty::Float(64) => float_ops,
        Ty::Int(32) | Ty::Uint(32) | Ty::Int(64) | Ty::Uint(64) => int_ops,
        _ => false,
    }
}

/// `base + off`（off 为 0 时直接复用 base，避免多余临时值）。
fn addr_offset(b: &mut Builder, base: &MirValue, off: i128) -> MirValue {
    if off == 0 {
        return base.clone();
    }
    binary_into(b, base.clone(), MirBinaryOp::Add, MirValue::Int(off))
}

fn binary_into(b: &mut Builder, l: MirValue, op: MirBinaryOp, r: MirValue) -> MirValue {
    let name = b.temp();
    b.current_block_mut()
        .instrs
        .push(MirInstr::Binary(name.clone(), l, op, r, NumTy::Int(64)));
    MirValue::Var(name)
}

/// 降级指针算术；不是指针算术时返回 `None`。
///
/// - `p + n` / `n + p` / `p - n`：`p ± n*size`，结果是指针；
/// - `p - q`：`(p - q) / size`，结果是元素个数（`isize`，MIR 里按 i64）。
///
/// 类型检查已保证这两种指针操作数的组合是合法的（不会两个指针相加）。
fn lower_pointer_arith(
    b: &mut Builder,
    op: hir::BinaryOp,
    lhs: &HirExpr,
    rhs: &HirExpr,
) -> Option<MirValue> {
    use hir::BinaryOp::*;
    let l_ptr = is_pointer_ty(&lhs.ty);
    let r_ptr = is_pointer_ty(&rhs.ty);
    match (op, l_ptr, r_ptr) {
        (Add, true, false) | (Sub, true, false) => {
            let base = lower_expr(b, lhs);
            let index = lower_expr(b, rhs);
            let scaled = scale_index(b, index, pointee_element_size(&lhs.ty));
            let bin = if op == Add {
                MirBinaryOp::Add
            } else {
                MirBinaryOp::Sub
            };
            Some(binary_into(b, base, bin, scaled))
        }
        (Add, false, true) => {
            let base = lower_expr(b, rhs);
            let index = lower_expr(b, lhs);
            let scaled = scale_index(b, index, pointee_element_size(&rhs.ty));
            Some(binary_into(b, base, MirBinaryOp::Add, scaled))
        }
        (Sub, true, true) => {
            let a = lower_expr(b, lhs);
            let c = lower_expr(b, rhs);
            let diff = binary_into(b, a, MirBinaryOp::Sub, c);
            let size = pointee_element_size(&lhs.ty);
            if size == 1 {
                Some(diff)
            } else {
                Some(binary_into(b, diff, MirBinaryOp::Div, MirValue::Int(size)))
            }
        }
        _ => None,
    }
}

/// `index * size`，`size == 1` 时直接返回原值（省一条指令）。
fn scale_index(b: &mut Builder, index: MirValue, size: i128) -> MirValue {
    if size == 1 {
        index
    } else {
        binary_into(b, index, MirBinaryOp::Mul, MirValue::Int(size))
    }
}

/// 扫出函数体里所有被取地址的局部变量 id（`&x`）。
///
/// 只认 `&local` 这种直接形式。取地址是"该变量必须住内存"的信号，
/// 后续降级据此决定走栈槽而不是寄存器。
fn collect_addr_taken(block: &HirBlock, out: &mut HashSet<u32>) {
    for stmt in &block.stmts {
        match stmt {
            HirStmt::Let { init, .. } | HirStmt::Var { init, .. } => {
                if let Some(e) = init {
                    collect_addr_taken_expr(e, out);
                }
            }
            HirStmt::Expr(e) => collect_addr_taken_expr(e, out),
            HirStmt::Item => {}
        }
    }
    if let Some(t) = &block.tail {
        collect_addr_taken_expr(t, out);
    }
}

fn collect_addr_taken_expr(e: &HirExpr, out: &mut HashSet<u32>) {
    use hir::ExprKind::*;
    match &e.kind {
        AddrOf(place) => {
            if let Local(id) = &place.kind {
                out.insert(id.0);
            }
            collect_addr_taken_expr(place, out);
        }
        Binary(_, a, b) | Assign(a, b) | Index(a, b) => {
            collect_addr_taken_expr(a, out);
            collect_addr_taken_expr(b, out);
        }
        Unary(_, a) | Field(a, _) | Deref(a) | Cast(a) | Await(a) => {
            collect_addr_taken_expr(a, out);
        }
        Call { args, .. } => {
            for a in args {
                collect_addr_taken_expr(a, out);
            }
        }
        CallIndirect { callee, args, .. } => {
            collect_addr_taken_expr(callee, out);
            for a in args {
                collect_addr_taken_expr(a, out);
            }
        }
        Block(b) => collect_addr_taken(b, out),
        If(c, t, e2) => {
            collect_addr_taken_expr(c, out);
            collect_addr_taken(t, out);
            if let Some(e2) = e2 {
                collect_addr_taken(e2, out);
            }
        }
        While(c, b) => {
            collect_addr_taken_expr(c, out);
            collect_addr_taken(b, out);
        }
        Loop(b) => collect_addr_taken(b, out),
        Array(elems) => {
            for e in elems {
                collect_addr_taken_expr(e, out);
            }
        }
        Struct(fields) => {
            for (_, e) in fields {
                collect_addr_taken_expr(e, out);
            }
        }
        Return(Some(e)) => collect_addr_taken_expr(e, out),
        Asm {
            outputs, inputs, ..
        } => {
            for (_, e) in outputs {
                if let Local(id) = &e.kind {
                    out.insert(id.0);
                }
                collect_addr_taken_expr(e, out);
            }
            for (_, e) in inputs {
                collect_addr_taken_expr(e, out);
            }
        }
        _ => {}
    }
}
