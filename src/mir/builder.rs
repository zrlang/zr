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
use crate::typeck::hir::{self, HirBlock, HirConst, HirExpr, HirFunction, HirStmt};
use std::collections::{HashMap, HashSet};

/// 降级单个函数。
pub fn lower_function(
    func: &HirFunction,
    body: &HirBlock,
    def_names: &std::collections::HashMap<hir::DefId, String>,
    consts: &std::collections::HashMap<hir::DefId, HirConst>,
) -> MirFunction {
    let mut b = Builder::new(func.name.clone(), def_names.clone(), const_values(consts));
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

    // 分配入口块
    let entry = b.new_block();
    b.current = entry;

    for (i, p) in func.params.iter().enumerate() {
        let ty = num_ty_of(&p.ty);
        b.def_local(p.local.0, MirValue::Var(format!("arg_{i}")), ty);
    }

    // 降级函数体
    let tail = lower_block(&mut b, body);

    // 终结入口块：尾表达式的值即函数返回值。
    // 已经有终结符（函数体里写了显式的 `return`）就不要覆盖。
    if b.current_block_mut().term.is_none() {
        b.current_block_mut().term = Some(MirTerminator::Return(tail));
    }

    // 把收集到的块按分配顺序落盘
    MirFunction {
        name: func.name.clone(),
        inline_never: func.attr("inline").is_some_and(|a| a.has_arg("never")),
        inline_always: func.attr("inline").is_some_and(|a| a.has_arg("always")),
        params: b.param_names.iter().map(|_| MirParam).collect(),
        blocks: b.blocks,
    }
}

/// 降级一个块，返回其尾表达式产生的值（若有）。
fn lower_block(b: &mut Builder, block: &HirBlock) -> Option<MirValue> {
    let mut tail = None;
    for stmt in &block.stmts {
        lower_stmt(b, stmt);
    }
    if let Some(expr) = &block.tail {
        tail = Some(lower_expr(b, expr));
    }
    tail
}

/// 降级一条语句。
fn lower_stmt(b: &mut Builder, stmt: &HirStmt) {
    match stmt {
        HirStmt::Let { local, init } | HirStmt::Var { local, init } => {
            if let Some(init) = init {
                let ty = num_ty_of(&init.ty);
                let v = lower_expr(b, init);
                b.def_local(local.0, v, ty);
            }
        }
        HirStmt::Expr(expr) => {
            let _ = lower_expr(b, expr);
        }
        HirStmt::Item => {}
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
            // 非 `const` 的全局（函数 / static）：仍无值可内联。
            None => MirValue::Unit,
        },
        Binary(op, lhs, rhs) => {
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
                // `p[i] = v`：算出 `p + i*size` 再存。
                Index(recv, idx) => {
                    let addr = lower_index_addr(b, recv, idx, &lhs.ty);
                    let ty = num_ty_of(&lhs.ty);
                    b.current_block_mut()
                        .instrs
                        .push(MirInstr::Store(addr, v, ty));
                    MirValue::Unit
                }
                _ => {
                    let target = format!("t{}", b.temp_id());
                    b.current_block_mut()
                        .instrs
                        .push(MirInstr::Assign(target.clone(), v));
                    MirValue::Var(target)
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
            b.loops.push(LoopCtx { header, exit });
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
            b.loops.push(LoopCtx { header: head, exit });
            let _ = lower_block(b, body);
            b.loops.pop();
            b.current_block_mut().term = Some(MirTerminator::Goto(head));
            b.current = exit;
            MirValue::Unit
        }
        Return(v) => {
            let rv = v.as_ref().map(|e| lower_expr(b, e));
            b.current_block_mut().term = Some(MirTerminator::Return(rv.clone()));
            rv.unwrap_or(MirValue::Unit)
        }
        Await(future) => {
            // await 表达式：先计算 future，然后生成 await 点
            let f = lower_expr(b, future);
            let name = format!("t{}", b.temp_id());
            b.current_block_mut()
                .instrs
                .push(MirInstr::Await(name.clone(), f));
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
            let mut mir_args = Vec::with_capacity(args.len());
            for a in args {
                mir_args.push(lower_expr(b, a));
            }
            // 调用表达式的类型就是被调函数的返回类型
            let ty = num_ty_of(&expr.ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Call(name.clone(), fn_name, mir_args, ty));
            MirValue::Var(name)
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
            if let Some(ctx) = b.loops.last() {
                b.terminate_if_open(MirTerminator::Goto(ctx.exit));
            }
            MirValue::Unit
        }
        Continue => {
            // `continue` 跳回循环头（回边），而不是像 `break` 那样跳出口。
            if let Some(ctx) = b.loops.last() {
                b.terminate_if_open(MirTerminator::Goto(ctx.header));
            }
            MirValue::Unit
        }
        // `*p`：从指针指向的地址加载。
        Deref(inner) => {
            let addr = lower_expr(b, inner);
            let name = format!("t{}", b.temp_id());
            let ty = num_ty_of(&expr.ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Load(name.clone(), addr, ty));
            MirValue::Var(name)
        }
        // `p[i]` / `s[i]`：算出 `base + i*size` 再从该地址加载。
        Index(recv, idx) => {
            let addr = lower_index_addr(b, recv, idx, &expr.ty);
            let name = format!("t{}", b.temp_id());
            let ty = num_ty_of(&expr.ty);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Load(name.clone(), addr, ty));
            MirValue::Var(name)
        }
        // `&place`：取位置地址。
        AddrOf(place) => match &place.kind {
            // `&local`：局部变量的栈槽地址。
            Local(id) => {
                let name = format!("t{}", b.temp_id());
                b.current_block_mut()
                    .instrs
                    .push(MirInstr::SlotAddr(name.clone(), id.0));
                MirValue::Var(name)
            }
            // `&*p` 就是 `p`。
            Deref(inner) => lower_expr(b, inner),
            // `&p[i]` 就是 `p + i*size`。
            Index(recv, idx) => lower_index_addr(b, recv, idx, &place.ty),
            _ => MirValue::Unit,
        },
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
        String(s) => MirValue::Str(s.clone()),
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
struct Builder {
    blocks: Vec<MirBasicBlock>,
    current: usize,
    param_names: Vec<String>,
    next_temp: u32,
    /// DefId → 函数名（由降级入口注入）。
    functions: HashMap<hir::DefId, String>,
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
    /// 被取过地址（`&x`）的局部变量 id。
    ///
    /// 这些变量不能再当作寄存器里的 SSA 值——取地址意味着它们的身份是
    /// 一块内存。降级时把它们固定到栈槽，读写都变成显式的 Load/Store。
    addr_taken: HashSet<u32>,
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
}

impl Builder {
    fn new(
        name: String,
        functions: HashMap<hir::DefId, String>,
        const_values: HashMap<hir::DefId, MirValue>,
    ) -> Self {
        let _ = name;
        Self {
            blocks: Vec::new(),
            current: 0,
            param_names: Vec::new(),
            next_temp: 0,
            loops: Vec::new(),
            functions,
            const_values,
            addr_taken: HashSet::new(),
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
                .push(MirInstr::SlotAddr(a.clone(), id));
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
                .push(MirInstr::SlotAddr(a.clone(), id));
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
        Ty::Pointer(..) | Ty::Reference(..) | Ty::Str | Ty::Slice(..) => 8,
        _ => 1,
    }
}

/// 降级索引地址：`base + index * size(base 的元素类型)`。
///
/// `elem_ty` 是**元素**类型（不是 receiver 类型），用来定缩放系数：
/// `s[i]`（str）缩放 1，`p[i]`（`*i32`）缩放 4。
fn lower_index_addr(b: &mut Builder, recv: &HirExpr, idx: &HirExpr, elem_ty: &hir::Ty) -> MirValue {
    let base = lower_expr(b, recv);
    let index = lower_expr(b, idx);
    let size = element_size(elem_ty);
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
        Unary(_, a) | Field(a) | Deref(a) | Cast(a) | Await(a) => {
            collect_addr_taken_expr(a, out);
        }
        Call { args, .. } => {
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
        Return(Some(e)) => collect_addr_taken_expr(e, out),
        _ => {}
    }
}
