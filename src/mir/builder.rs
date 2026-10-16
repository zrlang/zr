//! HIR → MIR 降级器。
//!
//! 降级策略：
//! - 每个 HIR 块对应一个 MIR 基本块；控制结构拆分成多个基本块。
//! - 表达式降级为线性指令序列，结果存入命名临时量。
//! - `if`/`match`/`while`/`for` 脱糖为 `Branch`/`Switch` 终结器。
//! - `break`/`continue` 转为对循环出口/头部块的 `Goto`。

use crate::mir::{
    NumTy,
    MirBasicBlock, MirBinaryOp, MirFunction, MirInstr, MirParam,
    MirTerminator, MirUnaryOp, MirValue,
};
use crate::typeck::hir::{self, HirBlock, HirExpr, HirStmt, HirFunction};
use std::collections::HashMap;

/// 降级单个函数。
pub fn lower_function(
    func: &HirFunction,
    body: &HirBlock,
    def_names: &std::collections::HashMap<hir::DefId, String>,
) -> MirFunction {
    let mut b = Builder::new(func.name.clone(), def_names.clone());

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
        b.current_block_mut().instrs.push(MirInstr::Assign(
            format!("v{}", p.local.0),
            MirValue::Var(format!("arg_{i}")),
        ));
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
        inline_never: func
            .attr("inline")
            .is_some_and(|a| a.has_arg("never")),
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
            let name = format!("v{}", local.0);
            if let Some(init) = init {
                let v = lower_expr(b, init);
                b.current_block_mut()
                    .instrs
                    .push(MirInstr::Assign(name, v));
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
        Literal(lit) => mir_literal(lit),
        Local(id) => MirValue::Var(format!("v{}", id.0)),
        Def(_id) => MirValue::Unit, // 函数/常量引用降级为占位
        Binary(op, lhs, rhs) => {
            let l = lower_expr(b, lhs);
            let r = lower_expr(b, rhs);
            let name = format!("t{}", b.temp_id());
            b.current_block_mut().instrs.push(MirInstr::Binary(
                name.clone(),
                l,
                convert_binary(*op),
                r,
            ));
            MirValue::Var(name)
        }
        Unary(op, operand) => {
            let o = lower_expr(b, operand);
            let name = format!("t{}", b.temp_id());
            b.current_block_mut().instrs.push(MirInstr::Unary(
                name.clone(),
                convert_unary(*op),
                o,
            ));
            MirValue::Var(name)
        }
        Assign(lhs, rhs) => {
            // 骨架：仅处理局部变量赋值
            let target = match &lhs.kind {
                Local(id) => format!("v{}", id.0),
                _ => format!("t{}", b.temp_id()),
            };
            let v = lower_expr(b, rhs);
            b.current_block_mut()
                .instrs
                .push(MirInstr::Assign(target.clone(), v));
            MirValue::Var(target)
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

            b.current_block_mut().term = Some(MirTerminator::Goto(header));
            b.current = header;
            let c = lower_expr(b, cond);
            b.current_block_mut().term = Some(MirTerminator::Branch {
                cond: c,
                then_block: body_bb,
                else_block: exit,
            });

            b.current = body_bb;
            let _ = lower_block(b, body);
            b.terminate_if_open(MirTerminator::Goto(header));

            b.current = exit;
            MirValue::Unit
        }
        Loop(body) => {
            let head = b.new_block();
            let exit = b.new_block();
            b.current_block_mut().term = Some(MirTerminator::Goto(head));
            b.current = head;
            let _ = lower_block(b, body);
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
            b.current_block_mut().instrs.push(MirInstr::Await(name.clone(), f));
            MirValue::Var(name)
        }
        Call { callee, args, result } => {
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
            b.current_block_mut()
                .instrs
                .push(MirInstr::Call(name.clone(), fn_name, mir_args));
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
            mir_literal(folded)
        }
        _ => {
            // 其他表达式（Match/For/Struct 等）骨架阶段降级为占位
            MirValue::Unit
        }
    }
}

/// 把 HIR 字面量转为 MIR 值。
fn mir_literal(lit: &hir::HirLiteral) -> MirValue {
    use hir::HirLiteral::*;
    match lit {
        Int(i) => MirValue::Int(*i),
        Float(f) => MirValue::Float(f.to_bits()),
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
}

impl Builder {
    fn new(name: String, functions: HashMap<hir::DefId, String>) -> Self {
        let _ = name;
        Self {
            blocks: Vec::new(),
            current: 0,
            param_names: Vec::new(),
            next_temp: 0,
            functions,
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
