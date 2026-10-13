//! HIR → MIR 降级器。
//!
//! 降级策略：
//! - 每个 HIR 块对应一个 MIR 基本块；控制结构拆分成多个基本块。
//! - 表达式降级为线性指令序列，结果存入命名临时量。
//! - `if`/`match`/`while`/`for` 脱糖为 `Branch`/`Switch` 终结器。
//! - `break`/`continue` 转为对循环出口/头部块的 `Goto`。

use crate::mir::{
    MirBasicBlock, MirBinaryOp, MirFunction, MirInstr, MirParam,
    MirTerminator, MirUnaryOp, MirValue,
};
use crate::typeck::hir::{self, HirBlock, HirExpr, HirStmt, HirFunction};

/// 降级单个函数。
pub fn lower_function(func: &HirFunction, body: &HirBlock) -> MirFunction {
    let mut b = Builder::new(func.name.clone());

    // 参数
    for _p in &func.params {
        let name = format!("arg_{}", b.param_names.len());
        b.param_names.push(name);
    }

    // 分配入口块
    let entry = b.new_block();
    b.current = entry;

    // 降级函数体
    let tail = lower_block(&mut b, body);

    // 终结入口块：若有尾表达式则 return，否则 fallthrough
    let _ = tail;
    b.current_block_mut().term = MirTerminator::Return;

    // 把收集到的块按分配顺序落盘
    MirFunction {
        name: func.name.clone(),
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
        HirStmt::Let { local, init, .. } | HirStmt::Var { local, init, .. } => {
            let name = format!("v{}", local.0);
            if let Some(init) = init {
                let v = lower_expr(b, init);
                b.current_block_mut()
                    .instrs
                    .push(MirInstr::Assign(name, v));
            }
        }
        HirStmt::Const { .. } => {
            // 顶层常量在 `lower` 中收集，函数内 `const` 降级为 let
        }
        HirStmt::Expr(expr, _) => {
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
            let _ = c;
            let then_bb = b.new_block();
            let else_bb = b.new_block();
            let join = b.new_block();

            b.current_block_mut().term = MirTerminator::Branch {
                then_block: then_bb,
                else_block: else_bb,
            };

            // then
            b.current = then_bb;
            let _then_val = lower_block(b, then_block);
            b.current_block_mut().term = MirTerminator::Goto(join);

            // else
            b.current = else_bb;
            if let Some(eb) = else_block {
                let _ = lower_block(b, eb);
            }
            b.current_block_mut().term = MirTerminator::Goto(join);

            b.current = join;
            MirValue::Unit
        }
        While(cond, body, _) => {
            let header = b.new_block();
            let body_bb = b.new_block();
            let exit = b.new_block();

            b.current_block_mut().term = MirTerminator::Goto(header);
            b.current = header;
            let c = lower_expr(b, cond);
            let _ = c;
            b.current_block_mut().term = MirTerminator::Branch {
                then_block: body_bb,
                else_block: exit,
            };

            b.current = body_bb;
            let _ = lower_block(b, body);
            b.current_block_mut().term = MirTerminator::Goto(header);

            b.current = exit;
            MirValue::Unit
        }
        Loop(body) => {
            let head = b.new_block();
            let exit = b.new_block();
            b.current_block_mut().term = MirTerminator::Goto(head);
            b.current = head;
            let _ = lower_block(b, body);
            b.current_block_mut().term = MirTerminator::Goto(head);
            b.current = exit;
            MirValue::Unit
        }
        Return(v) => {
            let rv = v.as_ref().map(|e| lower_expr(b, e));
            let _ = rv;
            b.current_block_mut().term = MirTerminator::Return;
            MirValue::Unit
        }
        Await(future) => {
            // await 表达式：先计算 future，然后生成 await 点
            let f = lower_expr(b, future);
            let name = format!("t{}", b.temp_id());
            b.current_block_mut().instrs.push(MirInstr::Await(name.clone(), f));
            MirValue::Var(name)
        }
        _ => {
            // 其他表达式（Call/Match/For/Struct 等）骨架阶段降级为占位
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
        _ => MirValue::Unit,
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
        Range | RangeInclusive | Concat => MirBinaryOp::Add,
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
}

impl Builder {
    fn new(name: String) -> Self {
        let _ = name;
        Self {
            blocks: Vec::new(),
            current: 0,
            param_names: Vec::new(),
            next_temp: 0,
        }
    }

    fn new_block(&mut self) -> usize {
        let id = self.blocks.len();
        self.blocks.push(MirBasicBlock {
            id,
            instrs: Vec::new(),
            term: MirTerminator::Unreachable,
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
