//! HIR → MIR 降级器。
//!
//! 降级策略：
//! - 每个 HIR 块对应一个 MIR 基本块；控制结构拆分成多个基本块。
//! - 表达式降级为线性指令序列，结果存入命名临时量。
//! - `if`/`match`/`while`/`for` 脱糖为 `Branch`/`Switch` 终结器。
//! - `break`/`continue` 转为对循环出口/头部块的 `Goto`。

use crate::mir::narrow;
use crate::typeck::hir::DefId;
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
    generic_params: &HashMap<hir::DefId, Vec<hir::Ty>>,
    statics: &HashMap<hir::DefId, (String, hir::Ty)>,
) -> MirFunction {
    let mut b = Builder::new(
        func.name.clone(),
        def_names.clone(),
        const_values(consts),
        type_defs.clone(),
        drop_impls.clone(),
        fn_params.clone(),
        generic_params.clone(),
        statics.clone(),
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
        if let Some(layout) = aggregate_layout(&b, actual_ret) {
            let hidden = format!("arg_{}", func.params.len());
            b.param_names.push(hidden.clone());
            b.ret_struct = Some((actual_ret.clone(), layout.size));
            b.hidden_ret = Some(hidden);
        } else if let hir::Ty::Named(id, args) = actual_ret {
            // 枚举没有 `StructLayout`（变体载荷各异），但同样需要一个
            // 调用方提供的返回槽：返回指向自己栈帧的指针会立刻失效。
            if let Some(el) = enum_layout(&b, *id, args) {
                let hidden = format!("arg_{}", func.params.len());
                b.param_names.push(hidden.clone());
                b.ret_bytes = Some((el.size, el.align));
                b.hidden_ret = Some(hidden);
            }
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
    b.drop_scopes.push(DropScope::default());
    let dropping_self = func.name.ends_with("__drop") && func.params.len() == 1;
    for (i, p) in func.params.iter().enumerate() {
        b.def_local(p.local.0, MirValue::Var(format!("arg_{i}")), &p.ty);
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
    if let (Some((size, align)), Some(dst)) = (b.ret_bytes, b.hidden_ret.clone()) {
        if let Some(value) = tail.take() {
            b.consume_temp(&value);
            copy_bytes_into(&mut b, value, MirValue::Var(dst.clone()), size, align);
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
            let mut tys: Vec<NumTy> = func.params.iter().map(|p| reg_ty_of(&p.ty)).collect();
            if b.hidden_ret.is_some() {
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

/// 降级一个"延迟到块尾执行"的 `defer` 块。
///
/// 与 `lower_block` 的唯一差别是**不**先压一个作用域：`defer` 作用域
/// 必须与它所属的块完全重合（析构点由调用方按逆序统一发出），所以这里
/// 只负责把语句降级出来。若块已经用 `return` / `break` 终结，就不再补
/// 跳转，避免覆盖掉已有的终结符。
fn lower_defer_block(b: &mut Builder, block: &HirBlock) {
    b.drop_scopes.push(DropScope::default());
    for stmt in &block.stmts {
        lower_stmt(b, stmt);
    }
    if let Some(expr) = &block.tail {
        let _ = lower_expr(b, expr);
    }
    let scope = b.drop_scopes.pop().unwrap();
    if b.current_block_mut().term.is_none() {
        b.emit_drop_scope(scope);
    }
    // 否则 defer 块内又 `return` / `break` 了：该层与外层作用域的析构都已由
    // 终结符处理过（`Return` 走 `emit_all_drops`，`Break`/`Continue` 走
    // `emit_drops_from`），这里不能再压回栈上——压回去会让外层块 pop 到**这**
    // 个空层而不是它自己的那一层，真正带着 Drop 局部量的层就永远留在栈上。
}

/// `emit_scope_drops == false` 用于函数根块：返回值可能来自局部量，必须先
/// 复制到返回槽/标记移动，再统一析构，不能提前把尾表达式的源值析构掉。
fn lower_block_opts(b: &mut Builder, block: &HirBlock, emit_scope_drops: bool) -> Option<MirValue> {
    b.drop_scopes.push(DropScope::default());
    let mut tail = None;
    // `defer` 语句由 `lower_stmt` 直接排进当前作用域的 `deferred` 队列，
    // 这里不再单独收集（见下方退出路径的注释）。
    for stmt in &block.stmts {
        lower_stmt(b, stmt);
    }
    if let Some(expr) = &block.tail {
        tail = Some(lower_expr(b, expr));
    }
    // 正常退出路径：先跑 defer（声明顺序逆序），再析构本作用域。
    // 作用域**总是弹出**——`emit_drop_scope` 内部会因终结符已存在而早退。
    // 这里不能把作用域压回栈上：压回去会让外层块的 `pop` 取到这一空层而不是
    // 它自己的那一层，真正带着 Drop 局部量的层就永远留在栈上、漏析构。
    // defer 由 `lower_stmt` 直接排进 `scope.deferred`，这里取空即得；不要另设
    // 局部收集器——那会在下面被 `mem::take` 整层覆盖掉，等于白收集。
    if emit_scope_drops {
        if let Some(mut scope) = b.drop_scopes.pop() {
            let deferred = std::mem::take(&mut scope.deferred);
            for d in deferred.into_iter().rev() {
                lower_defer_block(b, &d);
            }
            b.emit_drop_scope(scope);
        }
    }
    tail
}

/// 降级一条语句。
fn lower_stmt(b: &mut Builder, stmt: &HirStmt) {
    let temp_mark = b.temp_drops.len();
    // `defer` 不立刻执行：排进当前作用域，块尾逆序取出。
    if let HirStmt::Expr(e) = stmt {
        if let hir::ExprKind::Defer(d) = &e.kind {
            if let Some(scope) = b.drop_scopes.last_mut() {
                scope.deferred.push(d.clone());
            }
            return;
        }
    }
    match stmt {
        HirStmt::Let { local, init } | HirStmt::Var { local, init } => {
            if let Some(init) = init {
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
                b.def_local(local.0, v, &init.ty);
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
        Literal(lit) => match lit {
            // 字符串字面量在运行期是 (ptr, len) 一对：.rodata 里的地址加上
            // 字节数。别的字面量都是单值，直接降成常量即可。
            hir::HirLiteral::String(text) => {
                let addr = b.synthetic_storage(16);
                let ptr_at = offset_addr(b, MirValue::Var(addr.clone()), 0);
                b.current_block_mut().instrs.push(MirInstr::Store(
                    ptr_at,
                    MirValue::Str {
                        content: text.clone(),
                        len: text.len(),
                    },
                    NumTy::Uint(64),
                ));
                let len_at = offset_addr(b, MirValue::Var(addr.clone()), 8);
                b.current_block_mut().instrs.push(MirInstr::Store(
                    len_at,
                    MirValue::Int(text.len() as i128),
                    NumTy::Uint(64),
                ));
                b.register_temp(MirValue::Var(addr.clone()), &expr.ty);
                MirValue::Var(addr)
            }
            _ => mir_literal(lit, &expr.ty),
        },
        Local(id) => b.use_local(id.0, &expr.ty),
        // 全局 `const` 引用：直接把常量的值内联进来。
        //
        // 早先这里一律降级成 `MirValue::Unit`（"函数/常量引用降级为占位"），
        // 于是 `const LIMIT: i32 = 40; if LIMIT != 40 { .. }` 编出来是拿
        // unit 去比——恒不相等。`const` 是编译期已知的值，内联它是唯一正确
        // 的处理：它没有存储，也就没有"读一个全局"这种运行时操作。
        Def(id) => match b.const_values.get(id) {
            Some(value) => value.clone(),
            None if b.statics.contains_key(id) => {
                let (symbol, ty) = b.statics.get(id).cloned().unwrap();
                let addr = b.temp();
                b.current_block_mut()
                    .instrs
                    .push(MirInstr::FuncAddr(addr.clone(), symbol));
                if is_scalar_ty(&ty) {
                    let out = b.temp();
                    let num = num_ty_of(&ty);
                    b.current_block_mut().instrs.push(MirInstr::Load(
                        out.clone(),
                        MirValue::Var(addr),
                        num,
                    ));
                    b.register_temp(MirValue::Var(out.clone()), &expr.ty);
                    MirValue::Var(out)
                } else {
                    // 聚合值在运行期就是地址。
                    b.register_temp(MirValue::Var(addr.clone()), &expr.ty);
                    MirValue::Var(addr)
                }
            }
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
            // `str` 相等比较：两侧的值都是"胖指针"（数据指针 + 长度），
            // 存在各自的槽里。直接比较**槽地址**比的是两个局部变量的
            // 住址，不是字符串内容——`"a" == "a"` 恒为 false。
            //
            // zr 的 `str` 相等按**数据指针**比较：字符串字面量在 `.rodata`
            // 里去重存放，内容相同的字面量共享同一个槽，因此按地址比较
            // 与按内容比较对字面量而言结果一致，而后者对运行时拼出来的
            // 字符串也正确。
            if matches!(op, hir::BinaryOp::Eq | hir::BinaryOp::Ne) {
                if matches!(lhs.ty, hir::Ty::Str) && matches!(rhs.ty, hir::Ty::Str) {
                    let l = lower_expr(b, lhs);
                    let r = lower_expr(b, rhs);
                    let lptr = load_str_ptr(b, l);
                    let rptr = load_str_ptr(b, r);
                    let name = format!("t{}", b.temp_id());
                    b.current_block_mut().instrs.push(MirInstr::Binary(
                        name.clone(),
                        lptr,
                        convert_binary(*op),
                        rptr,
                        NumTy::Uint(64),
                    ));
                    return MirValue::Var(name);
                }
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
            let result = MirValue::Var(name);
            // 窄浮点的算术结果要舍入回窄格式——f16 的和不一定还是 f16，
            // Rust 的 f16 算术也是这么做的（算完再舍入）。
            if is_float_arith(*op) {
                if let Some(bits) = narrow_bits_of(&expr.ty) {
                    return round_to_narrow(b, result, bits);
                }
            }
            result
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
                    b.def_local(id.0, v, &lhs.ty);
                    b.use_local(id.0, &lhs.ty)
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
                // `static mut X = v`：写到数据符号的地址。
                Def(id) if b.statics.contains_key(id) => {
                    let (symbol, _) = b.statics.get(id).cloned().unwrap();
                    let addr = b.temp();
                    b.current_block_mut()
                        .instrs
                        .push(MirInstr::FuncAddr(addr.clone(), symbol));
                    store_value(b, MirValue::Var(addr), v, &lhs.ty);
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
        Tuple(elems) => {
            // 元组值在运行期是"指向元素存储的地址"，与结构体同一套布局。
            let Some(layout) = struct_layout(b, &expr.ty) else {
                return MirValue::Unit;
            };
            let addr = b.synthetic_storage(layout.size);
            for (idx, e) in elems.iter().enumerate() {
                let Some((off, elem_ty)) = layout.offset(&idx.to_string()) else {
                    continue;
                };
                let v = lower_expr(b, e);
                let at = offset_addr(b, MirValue::Var(addr.clone()), off);
                store_value(b, at, v, elem_ty);
            }
            b.register_temp(MirValue::Var(addr.clone()), &expr.ty);
            MirValue::Var(addr)
        }
        hir::ExprKind::EnumVariant {
            def_id,
            index,
            args,
        } => {
            // 枚举值在运行期是"指向一块存储的地址"：判别式 + 载荷区。
            let targs: Vec<hir::Ty> = match &expr.ty {
                hir::Ty::Named(id, a) if id == def_id => a.clone(),
                _ => Vec::new(),
            };
            let Some(layout) = enum_layout(b, *def_id, &targs) else {
                return MirValue::Unit;
            };
            let addr = b.synthetic_storage(layout.size);
            // 判别式放在偏移 0。判别式的值是变体自己的判别式（显式写的
            // 那个，或上一个 + 1），不是变体的序号。
            let discriminant = variant_discriminant(b, *def_id, *index);
            let tag_at = offset_addr(b, MirValue::Var(addr.clone()), 0);
            b.current_block_mut().instrs.push(MirInstr::Store(
                tag_at,
                MirValue::Int(discriminant),
                reg_ty_of(&layout.tag_ty),
            ));
            // 载荷按变体的字段偏移写。
            let field_tys = variant_field_tys_of(b, *def_id, *index, &targs);
            let offsets =
                variant_field_offsets(b, *def_id, *index, &targs).unwrap_or_default();
            for (i, arg) in args.iter().enumerate() {
                let Some(off) = offsets.get(i).copied() else {
                    continue;
                };
                let Some(field_ty) = field_tys.get(i) else {
                    continue;
                };
                let v = lower_expr(b, arg);
                let at = offset_addr(b, MirValue::Var(addr.clone()), layout.payload_off + off);
                store_value(b, at, v, field_ty);
            }
            b.register_temp(MirValue::Var(addr.clone()), &expr.ty);
            MirValue::Var(addr)
        }
        hir::ExprKind::EnumTag(value) => {
            // 判别式在偏移 0。
            let hir::Ty::Named(def_id, targs) = &value.ty else {
                return MirValue::Unit;
            };
            let Some(layout) = enum_layout(b, *def_id, targs) else {
                return MirValue::Unit;
            };
            let base = lower_expr(b, value);
            let at = offset_addr(b, base, 0);
            let name = b.temp();
            b.current_block_mut().instrs.push(MirInstr::Load(
                name.clone(),
                at,
                reg_ty_of(&layout.tag_ty),
            ));
            MirValue::Var(name)
        }
        hir::ExprKind::EnumField {
            value,
            variant,
            index,
        } => {
            // 载荷字段的偏移随变体而变，所以要按变体算。
            let hir::Ty::Named(def_id, targs) = &value.ty else {
                return MirValue::Unit;
            };
            let Some(layout) = enum_layout(b, *def_id, targs) else {
                return MirValue::Unit;
            };
            let Some(offsets) = variant_field_offsets(b, *def_id, *variant, targs) else {
                return MirValue::Unit;
            };
            let Some(off) = offsets.get(*index).copied() else {
                return MirValue::Unit;
            };
            let base = lower_expr(b, value);
            let at = offset_addr(b, base, layout.payload_off + off);
            let field_tys = variant_field_tys_of(b, *def_id, *variant, targs);
            let Some(field_ty) = field_tys.get(*index).cloned() else {
                return MirValue::Unit;
            };
            let name = b.temp();
            b.current_block_mut().instrs.push(MirInstr::Load(
                name.clone(),
                at,
                reg_ty_of(&field_ty),
            ));
            MirValue::Var(name)
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
            // 聚合类型（结构体 / 元组 / 枚举）的字段值就是它自己的地址，
            // 不再 Load 首字段。
            if is_aggregate(b, field_ty) {
                at
            } else {
                match narrow_bits_of(field_ty) {
                    Some(bits) => load_narrow(b, at, bits),
                    None => {
                        let name = format!("t{}", b.temp_id());
                        b.current_block_mut().instrs.push(MirInstr::Load(
                            name.clone(),
                            at,
                            reg_ty_of(field_ty),
                        ));
                        MirValue::Var(name)
                    }
                }
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
        While(label, cond, body) => {
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
                label: label.clone(),
            });
            let _ = lower_block(b, body);
            b.loops.pop();
            b.terminate_if_open(MirTerminator::Goto(header));

            b.current = exit;
            MirValue::Unit
        }
        Loop(label, body) => {
            let head = b.new_block();
            let exit = b.new_block();
            b.current_block_mut().term = Some(MirTerminator::Goto(head));
            b.current = head;
            b.loops.push(LoopCtx {
                header: head,
                exit,
                drop_depth: b.drop_scopes.len(),
                label: label.clone(),
            });
            let _ = lower_block(b, body);
            b.loops.pop();
            // 只在块还"开着"时补回边。循环体末尾若无条件写了 `break` /
            // `return`，块已经有终结符了；这里再无条件覆盖会把 `break`
            // 的出口跳转冲掉，整个循环退化成死循环。
            b.terminate_if_open(MirTerminator::Goto(head));
            b.current = exit;
            MirValue::Unit
        }
        Return(v) => {
            let temp_mark = b.temp_drops.len();
            // `defer` 必须先于返回值求值执行：从最内层作用域向外，各自按声明
            // 顺序逆序。作用域本身**不能**弹出——弹掉连这层登记的 Drop 局部
            // 量一起丢，返回路径就漏析构了（后面 `emit_all_drops` 只遍历栈上
            // 还留着的层）。这里只取空排队项，层留在栈上。
            b.flush_deferred();
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
            if let (Some((size, align)), Some(dst)) = (b.ret_bytes, b.hidden_ret.clone()) {
                if let Some(value) = rv.take() {
                    b.consume_temp(&value);
                    copy_bytes_into(b, value, MirValue::Var(dst.clone()), size, align);
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
            } else if let Some(layout) = aggregate_layout(b, &expr.ty) {
                let dst = b.synthetic_storage(layout.size);
                mir_args.push(MirValue::Var(dst.clone()));
                dest = Some(MirValue::Var(dst));
            } else if let hir::Ty::Named(id, args) = &expr.ty {
                if let Some(el) = enum_layout(b, *id, args) {
                    let dst = b.synthetic_storage(el.size);
                    mir_args.push(MirValue::Var(dst.clone()));
                    dest = Some(MirValue::Var(dst));
                }
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
            } else if let Some(layout) = aggregate_layout(b, &expr.ty) {
                let dst = b.synthetic_storage(layout.size);
                mir_args.push(MirValue::Var(dst.clone()));
                dest = Some(MirValue::Var(dst));
            } else if let hir::Ty::Named(id, args) = &expr.ty {
                if let Some(el) = enum_layout(b, *id, args) {
                    let dst = b.synthetic_storage(el.size);
                    mir_args.push(MirValue::Var(dst.clone()));
                    dest = Some(MirValue::Var(dst));
                }
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
            let v = lower_expr(b, inner);
            let src_narrow = narrow_bits_of(&inner.ty);
            let dst_narrow = narrow_bits_of(&expr.ty);
            match (src_narrow, dst_narrow) {
                // 窄 → 窄且宽度相同：值已经是精确的 f32 形态，不用动。
                (Some(a), Some(d)) if a == d => v,
                // → 窄：先转成 f32，再舍入到窄格式。
                (_, Some(bits)) => {
                    let f32_val = match &inner.ty {
                        // f32 形态的值（f32、f64 以外）已经就位。
                        hir::Ty::Float(32) => v,
                        // 窄浮点在寄存器里也是 f32 形态。
                        _ if src_narrow.is_some() => v,
                        // f64 或整数：先转成 f32。
                        _ => {
                            let from = num_ty_of(&inner.ty);
                            let name = b.temp();
                            b.current_block_mut().instrs.push(MirInstr::Cast(
                                name.clone(),
                                v,
                                from,
                                NumTy::Float(32),
                            ));
                            MirValue::Var(name)
                        }
                    };
                    round_to_narrow(b, f32_val, bits)
                }
                // 窄 → 宽 / 宽 → 宽 / 整数之间的转换。
                _ => {
                    let from = reg_ty_of(&inner.ty);
                    let to = reg_ty_of(&expr.ty);
                    let name = format!("t{}", b.temp_id());
                    b.current_block_mut()
                        .instrs
                        .push(MirInstr::Cast(name.clone(), v, from, to));
                    MirValue::Var(name)
                }
            }
        }
        Comptime(folded) => {
            // 编译期求值的结果直接变成字面量常量：内层表达式在 HIR 阶段就
            // 被折叠掉了，这里没有原始表达式可降级，`comptime 40 + 2` 到
            // 这一步已经是一个 `Int(42)`。
            mir_literal(folded, &expr.ty)
        }
        Break(label) => {
            // `break` 终结当前块并跳到对应循环的出口。带标签时从内向外
            // 找第一个匹配标签的循环，否则取最内层。用 `terminate_if_open`
            // 是为了不覆盖已经终结的块（比如 `if c { break; }` 里 if 自己刚
            // 写完跳转）。
            if let Some(ctx) = find_loop(b, label.as_deref()).cloned() {
                b.flush_deferred();
                b.emit_drops_from(ctx.drop_depth);
                b.terminate_if_open(MirTerminator::Goto(ctx.exit));
            }
            MirValue::Unit
        }
        Continue(label) => {
            // `continue` 跳回循环头（回边），而不是像 `break` 那样跳出口。
            if let Some(ctx) = find_loop(b, label.as_deref()).cloned() {
                b.emit_drops_from(ctx.drop_depth);
                b.terminate_if_open(MirTerminator::Goto(ctx.header));
            }
            MirValue::Unit
        }
        // `*p`：从指针指向的地址加载。结构体值本身就以地址表示，
        // 解引用只需把地址原样传下去。
        Deref(inner) => {
            let addr = lower_expr(b, inner);
            if is_aggregate(b, &expr.ty) {
                return addr;
            }
            if let Some(bits) = narrow_bits_of(&expr.ty) {
                return load_narrow(b, addr, bits);
            }
            let name = format!("t{}", b.temp_id());
            let ty = reg_ty_of(&expr.ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Load(name.clone(), addr, ty));
            MirValue::Var(name)
        }
        // `p[i]` / `s[i]`：算出 `base + i*size` 再从该地址加载。
        // 结构体/数组元素本身以地址表示，直接把这个地址当值。
        Index(recv, idx) => {
            let addr = lower_index_addr(b, recv, idx, &expr.ty);
            if is_aggregate(b, &expr.ty) || array_info(b, &expr.ty).is_some() {
                return addr;
            }
            if let Some(bits) = narrow_bits_of(&expr.ty) {
                return load_narrow(b, addr, bits);
            }
            let name = format!("t{}", b.temp_id());
            let ty = reg_ty_of(&expr.ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Load(name.clone(), addr, ty));
            MirValue::Var(name)
        }
        // `[T; N]` 隐式转 `[]T`：数组本身就是元素存储地址，补上长度即可。
        ArrayToSlice(inner) => {
            let arr = lower_expr(b, inner);
            let n = match &inner.ty {
                hir::Ty::Array(_, len) => hir::array_len_value(len).unwrap_or(0),
                _ => 0,
            };
            let addr = b.synthetic_storage(16);
            let len_at = offset_addr(b, MirValue::Var(addr.clone()), 8);
            b.current_block_mut().instrs.push(MirInstr::Store(
                MirValue::Var(addr.clone()),
                arr,
                NumTy::Uint(64),
            ));
            b.current_block_mut()
                .instrs
                .push(MirInstr::Store(len_at, MirValue::Int(n), NumTy::Uint(64)));
            b.register_temp(MirValue::Var(addr.clone()), &expr.ty);
            MirValue::Var(addr)
        }
        // `s[a..b]` / `s[..b]` / `s[a..]` / `s[..]`：构造 `(ptr, len)`。
        IndexRange(recv, start, end, inclusive) => lower_index_range(
            b,
            recv,
            start.as_deref(),
            end.as_deref(),
            *inclusive,
            &expr.ty,
        ),
        // `&place`：取位置地址。结构体/数组值本身已经是指向存储的地址，
        // 再对“保存该地址的槽”取地址会多解一层。
        AddrOf(place) => {
            if is_aggregate(b, &place.ty) || array_info(b, &place.ty).is_some() {
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
                // `&STATIC`：数据符号本身的地址。
                Def(id) if b.statics.contains_key(id) => {
                    let (symbol, _) = b.statics.get(id).cloned().unwrap();
                    let name = b.temp();
                    b.current_block_mut()
                        .instrs
                        .push(MirInstr::FuncAddr(name.clone(), symbol));
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
        // `defer` 不立刻执行：排进当前作用域，块尾（或 `return` / `break`）
        // 逆序取出。块的最后一条语句在 HIR 里是 `tail` 而不是 `stmts`，所以
        // 尾表达式位置的 `defer` 只会走到这里——若没有这个分支，它会被兜底
        // 分支当成 unit 静默丢掉，`defer` 编译通过却从不执行。
        Defer(d) => {
            if let Some(scope) = b.drop_scopes.last_mut() {
                scope.deferred.push(d.clone());
            }
            MirValue::Unit
        }
        Yield => {
            b.current_block_mut().instrs.push(MirInstr::Suspend);
            MirValue::Unit
        }
        Spawn(inner) => {
            let frame = lower_expr(b, inner);
            b.consume_temp(&frame);
            let name = format!("t{}", b.temp_id());
            b.current_block_mut().instrs.push(MirInstr::Call(
                name,
                "zr_spawn".to_string(),
                vec![frame],
                NumTy::Uint(64),
            ));
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
            // 窄浮点：先舍入到窄格式，再还原成 f32 的位模式——寄存器里
            // 窄浮点也是 f32 形态。
            hir::Ty::Float(8) | hir::Ty::Float(16) => {
                let bits = if matches!(ty, hir::Ty::Float(8)) { 8 } else { 16 };
                let narrow = narrow::f32_to_narrow_bits_ref((*f as f32).to_bits(), bits);
                let back = narrow::narrow_bits_to_f32_ref(narrow, bits);
                MirValue::Float(back as u64, 32)
            }
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

/// 分配一个合成栈槽的 id（全局唯一）。
pub(super) fn alloc_synthetic_slot() -> u32 {
    NEXT_SYNTHETIC.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 合成栈槽的全局编号。
///
/// 栈槽 id 会跟着函数体一起被内联进调用方，所以编号必须**全局唯一**——
/// 早先按函数各自从 0 开始发号，内联后调用方与被内联函数的 id 会撞车，
/// 两段代码指向同一块栈槽，窄浮点的位模式转换就此被踩坏。
static NEXT_SYNTHETIC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x4000_0000);

/// MIR 构建器：管理基本块分配与临时量编号。
/// 一个词法作用域：需要 Drop 的局部量 + 排队等待执行的 `defer`。
#[derive(Clone, Default)]
struct DropScope {
    drops: Vec<DropLocal>,
    /// 按声明顺序排队的 `defer` 块；块尾（或 `return`）时逆序取出执行。
    deferred: Vec<hir::HirBlock>,
}

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

pub(super) struct Builder {
    blocks: Vec<MirBasicBlock>,
    current: usize,
    param_names: Vec<String>,
    next_temp: u32,
    /// DefId → 函数名（由降级入口注入）。
    functions: HashMap<hir::DefId, String>,
    /// DefId → (数据符号名, 类型)：全局 `static` 变量。
    statics: HashMap<hir::DefId, (String, hir::Ty)>,
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
    /// 泛型类型定义的占位符参数（布局 / 字段偏移按实参替换）。
    generic_params: HashMap<hir::DefId, Vec<hir::Ty>>,
    /// 词法作用域栈，每层记录该作用域内需要 Drop 的局部量与 `defer`。
    drop_scopes: Vec<DropScope>,
    /// 合成局部 id 的起点（数组字面量等没有源码局部变量的存储）。
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
    /// 按字节整块返回的聚合（枚举）：(大小, 对齐)。
    ret_bytes: Option<(i128, i128)>,
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
#[derive(Clone)]
struct LoopCtx {
    /// `continue` 的落点（循环头）。
    header: usize,
    /// `break` 的落点（循环出口）。
    exit: usize,
    /// 进入循环体前的作用域深度；break/continue 只清理更内层的作用域。
    drop_depth: usize,
    /// 循环标签（`outer: loop`）。无标签的 `break` / `continue` 取最内层。
    label: Option<String>,
}

/// 按标签找循环上下文：`Some(label)` 从内向外找名字匹配的，
/// `None` 取最内层。
fn find_loop<'a>(b: &'a Builder, label: Option<&str>) -> Option<&'a LoopCtx> {
    match label {
        None => b.loops.last(),
        Some(name) => b
            .loops
            .iter()
            .rev()
            .find(|ctx| ctx.label.as_deref() == Some(name)),
    }
}

impl Builder {
    fn new(
        name: String,
        functions: HashMap<hir::DefId, String>,
        const_values: HashMap<hir::DefId, MirValue>,
        types: HashMap<hir::DefId, hir::HirTypeDef>,
        drops: HashMap<hir::DefId, String>,
        fn_params: HashMap<String, Vec<hir::Ty>>,
        generic_params: HashMap<hir::DefId, Vec<hir::Ty>>,
        statics: HashMap<hir::DefId, (String, hir::Ty)>,
    ) -> Self {
        let _ = name;
        Self {
            blocks: Vec::new(),
            current: 0,
            param_names: Vec::new(),
            next_temp: 0,
            loops: Vec::new(),
            functions,
            statics,
            fn_params,
            const_values,
            types,
            drops,
            generic_params,
            drop_scopes: Vec::new(),
            addr_taken: HashSet::new(),
            ret_array: None,
            hidden_ret: None,
            ret_struct: None,
            ret_bytes: None,
            temp_drops: Vec::new(),
            async_ret_bytes: None,
        }
    }

    /// 新的临时量名。
    pub(super) fn temp(&mut self) -> String {
        format!("t{}", self.temp_id())
    }

    /// 定义一个局部变量：被取地址的写进栈槽，否则是普通寄存器名。
    fn def_local(&mut self, id: u32, value: MirValue, ty: &hir::Ty) {
        if self.addr_taken.contains(&id) {
            let a = self.temp();
            self.current_block_mut()
                .instrs
                .push(MirInstr::SlotAddr(a.clone(), id, 8));
            // 窄浮点在内存里按窄位宽存，寄存器里仍是 f32。
            match narrow_bits_of(ty) {
                Some(bits) => {
                    let at = MirValue::Var(a);
                    store_narrow(self, at, value, bits);
                }
                None => {
                    self.current_block_mut()
                        .instrs
                        .push(MirInstr::Store(MirValue::Var(a), value, reg_ty_of(ty)));
                }
            }
        } else {
            self.current_block_mut()
                .instrs
                .push(MirInstr::Assign(format!("v{id}"), value));
        }
    }

    /// 读一个局部变量的值：被取地址的从栈槽加载，否则直接用寄存器名。
    fn use_local(&mut self, id: u32, ty: &hir::Ty) -> MirValue {
        if self.addr_taken.contains(&id) {
            let a = self.temp();
            self.current_block_mut()
                .instrs
                .push(MirInstr::SlotAddr(a.clone(), id, 8));
            let at = MirValue::Var(a);
            match narrow_bits_of(ty) {
                Some(bits) => return load_narrow(self, at, bits),
                None => {}
            }
            let t = self.temp();
            self.current_block_mut().instrs.push(MirInstr::Load(
                t.clone(),
                at,
                reg_ty_of(ty),
            ));
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

    pub(super) fn current_block_mut(&mut self) -> &mut MirBasicBlock {
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
            scope.drops.push(DropLocal {
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
                        .flat_map(|s| s.drops.iter().rev())
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
            for entry in scope.drops.iter_mut().rev() {
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
            .flat_map(|s| s.drops.iter().rev())
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
            for entry in scope.drops.iter_mut().rev() {
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
            for entry in scope.drops.iter_mut().rev() {
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
            for (ei, entry) in scope.drops.iter().enumerate().rev() {
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
                self.drop_scopes[si].drops[ei].moved = false;
                self.drop_scopes[si].drops[ei].moved_paths.clear();
            }
            None => self.track_drop(LocalId(local), ty),
        }
    }

    /// 执行栈上各作用域排队的 `defer`：**从最内层作用域向外**，每层内部再按
    /// 声明顺序逆序。
    ///
    /// 只取空 `deferred` 队列，**不**弹出作用域——各层登记的 Drop 局部量还得
    /// 留着，交给 `emit_drops_from` / `emit_all_drops` 统一析构。
    ///
    /// 方向要和 `emit_drops_from` 一致（它也是 `(depth..len).rev()` 从内向外）：
    /// `defer` 里的析构可能依赖内层块的变量，反过来跑就会读到已析构的值。
    fn flush_deferred(&mut self) {
        if self.current_block_mut().term.is_some() {
            return;
        }
        let pending: Vec<hir::HirBlock> = self.drop_scopes
            .iter_mut()
            .rev()
            .flat_map(|s| std::mem::take(&mut s.deferred).into_iter().rev())
            .collect();
        for d in pending {
            lower_defer_block(self, &d);
        }
    }

    fn emit_drop_scope(&mut self, scope: DropScope) {
        if std::env::var_os("ZR_DEBUG_DROP").is_some() {
            eprintln!(
                "emit scope len={} term={}",
                scope.drops.len(),
                self.current_block_mut().term.is_some()
            );
        }
        if self.current_block_mut().term.is_some() {
            return;
        }
        let mut drops = scope.drops;
        while let Some(entry) = drops.pop() {
            self.emit_drop_entry(&entry);
        }
    }

    fn emit_drops_from(&mut self, depth: usize) {
        if self.current_block_mut().term.is_some() {
            return;
        }
        for idx in (depth..self.drop_scopes.len()).rev() {
            let scope = self.drop_scopes[idx].drops.clone();
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
        let value = self.use_local(entry.local.0, &entry.ty);
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
            .flat_map(|s| s.drops.iter().rev())
            .find(|e| e.local == local)
            .map(|e| (e.moved, e.moved_paths.clone(), e.flags.clone()))
            .unwrap_or((false, Vec::new(), None));
        if paths.0 {
            return;
        }
        if std::env::var_os("ZR_DEBUG_DROP").is_some() {
            eprintln!("emit local={} paths={:?}", local.0, paths.1);
        }
        let value = self.use_local(local.0, ty);
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

    pub(super) fn synthetic_storage(&mut self, bytes: i128) -> String {
        let id = alloc_synthetic_slot();
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
    if matches!(op, Lt | Gt | Le | Ge) {
        // 比较结果的寄存器类型仍是 bool，但把**无符号操作数**的类型带下去，
        // 后端据此选 `setb`/`seta`（而不是 `setl`/`setg`）。
        let l = reg_ty_of(lhs_ty);
        if matches!(l, NumTy::Uint(_)) {
            return l;
        }
        let r = reg_ty_of(rhs_ty);
        if matches!(r, NumTy::Uint(_)) {
            return r;
        }
        return NumTy::Bool;
    }
    if matches!(op, Eq | Ne | And | Or) {
        return NumTy::Bool;
    }
    let t = reg_ty_of(expr_ty);
    if is_resolved(&t) {
        return t;
    }
    let l = reg_ty_of(lhs_ty);
    if is_resolved(&l) {
        return l;
    }
    let r = reg_ty_of(rhs_ty);
    if is_resolved(&r) {
        return r;
    }
    t
}

/// 是不是会产生浮点结果的算术运算（比较与逻辑运算不算）。
fn is_float_arith(op: hir::BinaryOp) -> bool {
    use hir::BinaryOp::*;
    matches!(op, Add | Sub | Mul | Div | Rem)
}

/// 标量（有固定寄存器宽度），与聚合值（运行期是地址）相对。
fn is_scalar_ty(ty: &hir::Ty) -> bool {
    matches!(
        ty,
        hir::Ty::Int(_)
            | hir::Ty::Uint(_)
            | hir::Ty::Float(_)
            | hir::Ty::Bool
            | hir::Ty::Char
            | hir::Ty::Pointer(..)
            | hir::Ty::Reference(..)
            | hir::Ty::Function { .. }
    )
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

/// 窄浮点（`f8` / `f16`）在**寄存器里以 f32 形态存放**：算术与比较都按
/// f32 做，只有落内存时才按窄位宽打包。所以寄存器层面的类型一律映射成
/// `Float(32)`——后端只认 32 / 64 两种浮点宽度。
fn reg_ty_of(ty: &hir::Ty) -> NumTy {
    match ty {
        hir::Ty::Float(8) | hir::Ty::Float(16) => NumTy::Float(32),
        _ => num_ty_of(ty),
    }
}

/// 窄浮点的位宽（8 / 16）；不是窄浮点时返回 `None`。
fn narrow_bits_of(ty: &hir::Ty) -> Option<u8> {
    match ty {
        hir::Ty::Float(8) => Some(8),
        hir::Ty::Float(16) => Some(16),
        _ => None,
    }
}

/// 把窄浮点的值写进内存：先转成窄位模式，再按窄位宽存。
fn store_narrow(b: &mut Builder, at: MirValue, value: MirValue, bits: u8) {
    narrow::mark_used(bits);
    let packed = call_narrow(
        b,
        &narrow::f32_to_narrow_name(bits),
        vec![value],
        narrow::bits_ty(bits),
    );
    b.current_block_mut().instrs.push(MirInstr::Store(
        at,
        packed,
        if bits == 8 {
            NumTy::Uint(8)
        } else {
            NumTy::Uint(16)
        },
    ));
}

/// 从内存读窄浮点：读出窄位模式，再还原成 f32。
fn load_narrow(b: &mut Builder, at: MirValue, bits: u8) -> MirValue {
    narrow::mark_used(bits);
    let raw = b.temp();
    b.current_block_mut().instrs.push(MirInstr::Load(
        raw.clone(),
        at,
        if bits == 8 {
            NumTy::Uint(8)
        } else {
            NumTy::Uint(16)
        },
    ));
    call_narrow(
        b,
        &narrow::narrow_to_f32_name(bits),
        vec![MirValue::Var(raw)],
        NumTy::Float(32),
    )
}

/// 把一个 f32 的值舍入到窄浮点精度（结果仍以 f32 形态存放）。
fn round_to_narrow(b: &mut Builder, value: MirValue, bits: u8) -> MirValue {
    narrow::mark_used(bits);
    let packed = call_narrow(
        b,
        &narrow::f32_to_narrow_name(bits),
        vec![value],
        narrow::bits_ty(bits),
    );
    call_narrow(
        b,
        &narrow::narrow_to_f32_name(bits),
        vec![packed],
        NumTy::Float(32),
    )
}

/// 调用一个窄浮点转换辅助函数，返回它的结果。
///
/// 转换本身有上百条指令，编在调用点里会把栈帧撑爆（32 位目标尤其吃不消），
/// 所以统一编成独立的辅助函数，调用点只留一条 `call`。
fn call_narrow(
    b: &mut Builder,
    callee: &str,
    args: Vec<MirValue>,
    result: NumTy,
) -> MirValue {
    let name = b.temp();
    b.current_block_mut().instrs.push(MirInstr::Call(
        name.clone(),
        callee.to_string(),
        args,
        result,
    ));
    MirValue::Var(name)
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
        // 切片 / 字符串是 (ptr, len) 一对——16 字节。
        Ty::Str | Ty::Slice(..) => 16,
        Ty::Pointer(..) | Ty::Reference(..) | Ty::Function { .. } => 8,
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

/// 用类型定义的泛型实参替换类型里的占位符。
fn subst_generic(
    b: &Builder,
    def_id: DefId,
    args: &[hir::Ty],
    ty: &hir::Ty,
) -> hir::Ty {
    let Some(params) = b.generic_params.get(&def_id) else {
        return ty.clone();
    };
    if params.len() != args.len() {
        return ty.clone();
    }
    let map: HashMap<hir::InferId, hir::Ty> = params
        .iter()
        .zip(args)
        .filter_map(|(p, a)| match p {
            hir::Ty::Infer(id) => Some((id.clone(), a.clone())),
            _ => None,
        })
        .collect();
    subst_infer_ty(ty, &map)
}

/// 递归替换 `Ty::Infer` 占位符。
fn subst_infer_ty(ty: &hir::Ty, map: &HashMap<hir::InferId, hir::Ty>) -> hir::Ty {
    use hir::Ty;
    match ty {
        Ty::Infer(id) => map.get(id).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Named(id, args) => {
            Ty::Named(*id, args.iter().map(|t| subst_infer_ty(t, map)).collect())
        }
        Ty::Pointer(t, m) => Ty::Pointer(Box::new(subst_infer_ty(t, map)), *m),
        Ty::Reference(t, m) => Ty::Reference(Box::new(subst_infer_ty(t, map)), *m),
        Ty::Array(t, len) => Ty::Array(Box::new(subst_infer_ty(t, map)), len.clone()),
        Ty::Slice(t, m) => Ty::Slice(Box::new(subst_infer_ty(t, map)), *m),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| subst_infer_ty(t, map)).collect()),
        Ty::Function {
            params,
            ret,
            is_async,
            abi,
        } => Ty::Function {
            params: params.iter().map(|t| subst_infer_ty(t, map)).collect(),
            ret: Box::new(subst_infer_ty(ret, map)),
            is_async: *is_async,
            abi: abi.clone(),
        },
        Ty::Optional(t) => Ty::Optional(Box::new(subst_infer_ty(t, map))),
        Ty::ErrorUnion(t, e) => Ty::ErrorUnion(
            Box::new(subst_infer_ty(t, map)),
            Box::new(subst_infer_ty(e, map)),
        ),
        Ty::Pin(t) => Ty::Pin(Box::new(subst_infer_ty(t, map))),
        _ => ty.clone(),
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
        Ty::Tuple(elems) => {
            // 与结构体同布局：逐字段对齐，末尾补齐到最大对齐。
            let mut off = 0i128;
            let mut align = 1i128;
            for elem in elems {
                let (sz, al) = type_size_align(b, elem, depth + 1)?;
                off = align_up(off, al);
                off += sz;
                align = align.max(al);
            }
            (align_up(off, align), align)
        }
        Ty::Named(id, args) => {
            // 枚举：判别式 + 载荷区。
            if let Some(el) = enum_layout(b, *id, args) {
                return Some((el.size, el.align));
            }
            let hs = match b.types.get(id)? {
                hir::HirTypeDef::Struct(hs) => hs,
                _ => return None,
            };
            let mut off = 0i128;
            let mut align = 1i128;
            for f in &hs.fields {
                let fty = subst_generic(b, *id, args, &f.ty);
                let (sz, al) = type_size_align(b, &fty, depth + 1)?;
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
    // 元组与结构体同一套布局。
    if let hir::Ty::Tuple(elems) = ty {
        return tuple_layout(b, elems);
    }
    // 切片 / 字符串在运行期是 (ptr, len) 一对。
    if matches!(ty, hir::Ty::Str | hir::Ty::Slice(..)) {
        let mut names = HashMap::new();
        names.insert("ptr".to_string(), 0usize);
        names.insert("len".to_string(), 1usize);
        return Some(StructLayout {
            size: 16,
            fields: vec![
                (0, hir::Ty::Pointer(Box::new(hir::Ty::Uint(8)), hir::Mutability::Immutable)),
                (8, hir::Ty::Uint(64)),
            ],
            names,
        });
    }
    let (id, args) = match ty {
        hir::Ty::Named(id, args) => (*id, args.clone()),
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
        let fty = subst_generic(b, id, &args, &f.ty);
        let (sz, al) = type_size_align(b, &fty, 0)?;
        off = align_up(off, al);
        fields.push((off, fty));
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

/// **值聚合**的布局：结构体 / 元组 / 字符串 / 切片。
///
/// 与 [`struct_layout`] 的区别：**指向结构体的指针不是值聚合**。
/// `*mut T` 是一个 8 字节的瘦指针，按值传递、返回都不需要调用方提供
/// 返回槽。`struct_layout` 会把 `*mut T` 递归成 `T` 的布局（那是字段
/// 访问 `p.field` 需要的），所以凡是"这个类型要按聚合方式传/返回"的地方
/// 都必须用本函数，否则 `*mut T` 会被错当成 `T` 整块拷贝。
fn aggregate_layout(b: &Builder, ty: &hir::Ty) -> Option<StructLayout> {
    match ty {
        hir::Ty::Pointer(..) | hir::Ty::Reference(..) | hir::Ty::Function { .. } => None,
        _ => struct_layout(b, ty),
    }
}

/// 元组的布局：字段按序排、各自对齐，末尾补齐到最大对齐。
///
/// 字段名用下标的十进制写法（"0" / "1" …），与 `t.0` 的访问方式一致。
fn tuple_layout(b: &Builder, elems: &[hir::Ty]) -> Option<StructLayout> {
    let mut fields = Vec::with_capacity(elems.len());
    let mut names = HashMap::new();
    let mut off = 0i128;
    let mut align = 1i128;
    for (idx, elem) in elems.iter().enumerate() {
        let (sz, al) = type_size_align(b, elem, 0)?;
        off = align_up(off, al);
        fields.push((off, elem.clone()));
        names.insert(idx.to_string(), idx);
        off += sz;
        align = align.max(al);
    }
    Some(StructLayout {
        size: align_up(off, align),
        fields,
        names,
    })
}

/// 枚举值的布局：判别式 + 载荷区。
///
/// 枚举值在运行期是"指向一块存储的地址"，这块存储的形状是
///
/// ```text
/// [判别式][载荷区]
/// ```
///
/// 判别式放在偏移 0，载荷区紧跟其后（按载荷区的对齐补齐）。载荷区的大小
/// 取各变体载荷的最大值——不同变体的载荷类型不同，但值的形状必须一样，
/// 这样枚举值才能按值拷贝。
struct EnumLayout {
    /// 判别式的类型。
    tag_ty: hir::Ty,
    /// 载荷区的起始偏移。
    payload_off: i128,
    /// 枚举值的总大小。
    size: i128,
    /// 枚举值的对齐。
    align: i128,
}

/// 枚举的判别式类型。
///
/// `Auto` 按变体个数挑最小的够用的整数（和 Rust 一样）：≤256 个变体用
/// `i8`，≤65536 个用 `i16`，再不行用 `i32`。
fn enum_tag_ty(b: &Builder, def_id: DefId) -> Option<hir::Ty> {
    let hir::HirTypeDef::Enum(he) = b.types.get(&def_id)? else {
        return None;
    };
    Some(match he.repr {
        hir::IntegerRepr::I8 => hir::Ty::Int(8),
        hir::IntegerRepr::I16 => hir::Ty::Int(16),
        hir::IntegerRepr::I32 => hir::Ty::Int(32),
        hir::IntegerRepr::I64 | hir::IntegerRepr::I128 => hir::Ty::Int(64),
        hir::IntegerRepr::Auto => match he.variants.len() {
            0..=256 => hir::Ty::Int(8),
            257..=65536 => hir::Ty::Int(16),
            _ => hir::Ty::Int(32),
        },
    })
}

/// 变体载荷的字段类型（按声明顺序）。
fn variant_field_tys(v: &hir::HirVariant) -> Vec<hir::Ty> {
    match &v.payload {
        hir::HirVariantPayload::Unit => Vec::new(),
        hir::HirVariantPayload::Tuple(ts) => ts.clone(),
        hir::HirVariantPayload::Struct(fields) => fields.iter().map(|f| f.ty.clone()).collect(),
    }
}

/// 枚举的布局。
fn enum_layout(b: &Builder, def_id: DefId, args: &[hir::Ty]) -> Option<EnumLayout> {
    let hir::HirTypeDef::Enum(he) = b.types.get(&def_id)? else {
        return None;
    };
    let tag_ty = enum_tag_ty(b, def_id)?;
    let (tag_size, tag_align) = type_size_align(b, &tag_ty, 0)?;
    // 载荷区：各变体载荷里最大的那个。
    let mut payload_size = 0i128;
    let mut payload_align = 1i128;
    for v in &he.variants {
        let tys: Vec<hir::Ty> = variant_field_tys(v)
            .iter()
            .map(|t| subst_generic(b, def_id, args, t))
            .collect();
        let layout = tuple_layout(b, &tys)?;
        payload_size = payload_size.max(layout.size);
        for t in &tys {
            let (_, al) = type_size_align(b, t, 0)?;
            payload_align = payload_align.max(al);
        }
    }
    let payload_off = align_up(tag_size, payload_align);
    let size = payload_off + payload_size;
    let align = tag_align.max(payload_align);
    Some(EnumLayout {
        tag_ty,
        payload_off,
        size: align_up(size, align),
        align,
    })
}

/// 类型是不是"聚合"（运行期用地址表示）。
///
/// 结构体、元组、枚举的值在运行期都是"指向存储的地址"，取它们的字段
/// 时不该再 Load 一次。
fn is_aggregate(b: &Builder, ty: &hir::Ty) -> bool {
    if struct_layout(b, ty).is_some() {
        return true;
    }
    match ty {
        hir::Ty::Named(def_id, args) => enum_layout(b, *def_id, args).is_some(),
        _ => false,
    }
}

/// 某个变体的判别式值。
fn variant_discriminant(b: &Builder, def_id: DefId, variant: usize) -> i128 {
    let Some(hir::HirTypeDef::Enum(he)) = b.types.get(&def_id) else {
        return variant as i128;
    };
    he.variants
        .get(variant)
        .map(|v| v.discriminant)
        .unwrap_or(variant as i128)
}

/// 某个变体的载荷字段类型（按声明顺序）。
fn variant_field_tys_of(
    b: &Builder,
    def_id: DefId,
    variant: usize,
    args: &[hir::Ty],
) -> Vec<hir::Ty> {
    let Some(hir::HirTypeDef::Enum(he)) = b.types.get(&def_id) else {
        return Vec::new();
    };
    let Some(v) = he.variants.get(variant) else {
        return Vec::new();
    };
    variant_field_tys(v)
        .iter()
        .map(|t| subst_generic(b, def_id, args, t))
        .collect()
}

/// 某个变体的载荷字段偏移（相对载荷区起点）。
fn variant_field_offsets(
    b: &Builder,
    def_id: DefId,
    variant: usize,
    args: &[hir::Ty],
) -> Option<Vec<i128>> {
    let hir::HirTypeDef::Enum(he) = b.types.get(&def_id)? else {
        return None;
    };
    let v = he.variants.get(variant)?;
    let tys: Vec<hir::Ty> = variant_field_tys(v)
        .iter()
        .map(|t| subst_generic(b, def_id, args, t))
        .collect();
    let layout = tuple_layout(b, &tys)?;
    Some(layout.fields.iter().map(|(off, _)| *off).collect())
}

/// 把 `size` 个字节从 `src` 复制到 `dst`。
///
/// 尽量按 8 字节（一次 64 位读写）复制，剩下不足 8 字节的尾巴再按
/// 4 / 2 / 1 字节补齐。逐字节复制会生成几十条指令——枚举的整块拷贝
/// 尤其致命，32 位目标的栈帧会被撑爆。
fn copy_bytes_into(b: &mut Builder, src: MirValue, dst: MirValue, size: i128, align: i128) {
    // 一次最多搬 `align` 字节：超过对齐的访问在 ARM / RISC-V 上会触发
    // 对齐异常（x86 上只是慢）。
    let max_chunk = align.clamp(1, 8);
    let mut off = 0i128;
    while off < size {
        let rest = size - off;
        let (chunk, ty) = if rest >= 8 && max_chunk >= 8 {
            (8i128, NumTy::Uint(64))
        } else if rest >= 4 && max_chunk >= 4 {
            (4i128, NumTy::Uint(32))
        } else if rest >= 2 && max_chunk >= 2 {
            (2i128, NumTy::Uint(16))
        } else {
            (1i128, NumTy::Uint(8))
        };
        let src_at = offset_addr(b, src.clone(), off);
        let dst_at = offset_addr(b, dst.clone(), off);
        let tmp = b.temp();
        b.current_block_mut()
            .instrs
            .push(MirInstr::Load(tmp.clone(), src_at, ty));
        b.current_block_mut()
            .instrs
            .push(MirInstr::Store(dst_at, MirValue::Var(tmp), ty));
        off += chunk;
    }
}

/// 把 `src` 处的值复制到 `dst` 处（按类型决定怎么复制）。
///
/// 聚合类型（结构体 / 元组 / 枚举）整块复制，其余按标量处理。
fn copy_value_into(b: &mut Builder, src: MirValue, dst: MirValue, ty: &hir::Ty) {
    if let Some(layout) = struct_layout(b, ty) {
        copy_struct_into(b, src, &layout, dst);
        return;
    }
    if let Some((elem, n, _)) = array_info(b, ty) {
        copy_array_into(b, src, &elem, n, dst);
        return;
    }
    if let hir::Ty::Named(def_id, args) = ty {
        if let Some(el) = enum_layout(b, *def_id, args) {
            copy_bytes_into(b, src, dst, el.size, el.align);
            return;
        }
    }
    let v = match narrow_bits_of(ty) {
        Some(bits) => load_narrow(b, src, bits),
        None => {
            let tmp = b.temp();
            b.current_block_mut().instrs.push(MirInstr::Load(
                tmp.clone(),
                src,
                reg_ty_of(ty),
            ));
            MirValue::Var(tmp)
        }
    };
    match narrow_bits_of(ty) {
        Some(bits) => store_narrow(b, dst, v, bits),
        None => {
            b.current_block_mut()
                .instrs
                .push(MirInstr::Store(dst, v, reg_ty_of(ty)));
        }
    }
}

/// 把值写进一个字段/元素位置，结构体和数组递归复制。
fn store_value(b: &mut Builder, at: MirValue, value: MirValue, ty: &hir::Ty) {
    b.consume_temp(&value);
    if let Some(layout) = struct_layout(b, ty) {
        copy_struct_into(b, value, &layout, at);
        return;
    }
    // 枚举：整块按字节复制。
    if let hir::Ty::Named(def_id, args) = ty {
        if let Some(el) = enum_layout(b, *def_id, args) {
            copy_bytes_into(b, value, at, el.size, el.align);
            return;
        }
    }
    if let Some((elem, n, _)) = array_info(b, ty) {
        copy_array_into(b, value, &elem, n, at);
        return;
    }
    match narrow_bits_of(ty) {
        Some(bits) => store_narrow(b, at, value, bits),
        None => {
            let ty = reg_ty_of(ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Store(at, value, ty));
        }
    }
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
        } else if is_aggregate(b, field_ty) {
            // 聚合字段（结构体 / 元组 / 枚举）：整块复制。
            copy_value_into(b, s, d, field_ty);
        } else {
            let v = match narrow_bits_of(field_ty) {
                Some(bits) => load_narrow(b, s, bits),
                None => {
                    let tmp = format!("t{}", b.temp_id());
                    b.current_block_mut().instrs.push(MirInstr::Load(
                        tmp.clone(),
                        s,
                        reg_ty_of(field_ty),
                    ));
                    MirValue::Var(tmp)
                }
            };
            match narrow_bits_of(field_ty) {
                Some(bits) => store_narrow(b, d, v, bits),
                None => {
                    b.current_block_mut().instrs.push(MirInstr::Store(
                        d,
                        v,
                        reg_ty_of(field_ty),
                    ));
                }
            }
        }
    }
}

/// 结构体按值拷贝：返回一块新存储的地址。
fn copy_struct_value(b: &mut Builder, src: MirValue, ty: &hir::Ty) -> MirValue {
    // 枚举：整块按字节复制（不同变体的载荷类型不同，逐字段复制做不到）。
    if let hir::Ty::Named(def_id, args) = ty {
        if let Some(el) = enum_layout(b, *def_id, args) {
            b.consume_temp(&src);
            let dst = b.synthetic_storage(el.size);
            copy_bytes_into(b, src, MirValue::Var(dst.clone()), el.size, el.align);
            return MirValue::Var(dst);
        }
    }
    let Some(layout) = aggregate_layout(b, ty) else {
        return src;
    };
    b.consume_temp(&src);
    let dst = b.synthetic_storage(layout.size);
    copy_struct_into(b, src, &layout, MirValue::Var(dst.clone()));
    MirValue::Var(dst)
}

/// 从一个 `str` 值的存储地址里取出它的数据指针（胖指针的第一个字段）。
///
/// `str` 的运行时表示是 `{ ptr: *const u8, len: usize }`，存放在一块
/// 16 字节的存储里。比较两个 `str` 时要比较的是数据指针，不是这块
/// 存储的地址。
fn load_str_ptr(b: &mut Builder, addr: MirValue) -> MirValue {
    let name = format!("t{}", b.temp_id());
    b.current_block_mut()
        .instrs
        .push(MirInstr::Load(name.clone(), addr, NumTy::Uint(64)));
    MirValue::Var(name)
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
            let v = match narrow_bits_of(elem_ty) {
                Some(bits) => load_narrow(b, src_at, bits),
                None => {
                    let elem = b.temp();
                    b.current_block_mut().instrs.push(MirInstr::Load(
                        elem.clone(),
                        src_at,
                        reg_ty_of(elem_ty),
                    ));
                    MirValue::Var(elem)
                }
            };
            match narrow_bits_of(elem_ty) {
                Some(bits) => store_narrow(b, dst_at, v, bits),
                None => {
                    b.current_block_mut().instrs.push(MirInstr::Store(
                        dst_at,
                        v,
                        reg_ty_of(elem_ty),
                    ));
                }
            }
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
    let mut base = lower_expr(b, recv);
    // `str` 的运行时表示是 `{ ptr: *const u8, len: usize }`，存在一块
    // 16 字节的存储里；`str` 的值就是**这块存储的地址**。
    //
    // 按字节索引要的是数据指针，不是这块存储的地址——直接拿存储地址
    // 加偏移，读到的是胖指针自己的字节（指针的低位），不是字符。
    if matches!(recv.ty, hir::Ty::Str | hir::Ty::Slice(..)) {
        base = load_str_ptr(b, base);
    }
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

/// 降级切片表达式 `s[a..b]`。
///
/// 结果是一块 16 字节的合成存储 `{ ptr, len }`。三种接收者：
///   - 定长数组：值本身就是元素存储的地址，长度是编译期常量；
///   - `str` / 切片：值是指向 `{ptr, len}` 存储的地址，先把它读出来；
///   - 裸指针 / 引用：只有基址，没有长度，因此 `[a..]` 无法确定终点。
fn lower_index_range(
    b: &mut Builder,
    recv: &HirExpr,
    start: Option<&HirExpr>,
    end: Option<&HirExpr>,
    inclusive: bool,
    result_ty: &hir::Ty,
) -> MirValue {
    let elem_ty = match result_ty {
        hir::Ty::Slice(inner, _) => (**inner).clone(),
        hir::Ty::Str => hir::Ty::Uint(8),
        _ => return MirValue::Unit,
    };
    let elem_size = type_size(b, &elem_ty).max(1);

    let recv_val = lower_expr(b, recv);
    let (base_ptr, base_len) = match &recv.ty {
        hir::Ty::Array(_, len) => {
            let n = hir::array_len_value(len).unwrap_or(0);
            (recv_val, Some(MirValue::Int(n)))
        }
        hir::Ty::Str | hir::Ty::Slice(..) => {
            let ptr = load_str_ptr(b, recv_val.clone());
            let len_at = offset_addr(b, recv_val, 8);
            let len = load_str_ptr(b, len_at);
            (ptr, Some(len))
        }
        hir::Ty::Pointer(..) | hir::Ty::Reference(..) => (recv_val, None),
        _ => return MirValue::Unit,
    };

    let start_v = match start {
        Some(e) => lower_expr(b, e),
        None => MirValue::Int(0),
    };
    let end_v = match end {
        Some(e) => lower_expr(b, e),
        None => match base_len {
            Some(l) => l,
            None => return MirValue::Unit,
        },
    };

    let mut len = binary_into(b, end_v, MirBinaryOp::Sub, start_v.clone());
    if inclusive {
        len = binary_into(b, len, MirBinaryOp::Add, MirValue::Int(1));
    }
    let byte_off = binary_into(b, start_v, MirBinaryOp::Mul, MirValue::Int(elem_size));
    let ptr = binary_into(b, base_ptr, MirBinaryOp::Add, byte_off);

    let addr = b.synthetic_storage(16);
    let len_at = offset_addr(b, MirValue::Var(addr.clone()), 8);
    b.current_block_mut().instrs.push(MirInstr::Store(
        MirValue::Var(addr.clone()),
        ptr,
        NumTy::Uint(64),
    ));
    b.current_block_mut()
        .instrs
        .push(MirInstr::Store(len_at, len, NumTy::Uint(64)));
    b.register_temp(MirValue::Var(addr.clone()), result_ty);
    MirValue::Var(addr)
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
        While(_, c, b) => {
            collect_addr_taken_expr(c, out);
            collect_addr_taken(b, out);
        }
        Loop(_, b) => collect_addr_taken(b, out),
        // `defer` 块只是延后执行，里面的 `&x` 仍然会让 `x` 变成栈槽变量，
        // 所以必须照常递归进去。漏掉这个分支时，defer 里取址的局部量会被当成
        // 普通值处理，结构体字面量拿到的就是值而不是地址。
        Defer(b) => collect_addr_taken(b, out),
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
        Return(Some(e)) | Spawn(e) => collect_addr_taken_expr(e, out),
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
