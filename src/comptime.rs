//! 编译期求值（CTFE，compile-time function evaluation）。
//!
//! 早期版本的 comptime 只支持**表达式**：`comptime 40 + 2` 会在类型检查
//! 阶段折成字面量。`comptime fn` 虽然能被解析（`parse_function` 吃掉
//! `KwComptime`），但结果被丢进 `_is_const`，于是编译器根本没有调用
//! comptime 函数的能力——`comptime fn fact` 和 `fn fact` 完全一样，只能
//! 在运行期跑。
//!
//! 这里补上函数级的编译期求值：
//!
//! ```text
//! comptime fn fact(n: i32) -> i32 { if n <= 1 { 1 } else { n * fact(n-1) } }
//! const SIZE: usize = fact(5);      // 编译期求值为 120
//! ```
//!
//! 求值器在**类型检查之后**运行，直接解释 HIR：
//! - HIR 已经完成名称解析与类型检查，不需要求值器再做一遍；
//! - 求值成功的调用点被替换成 [`ExprKind::Comptime`]（字面量），之后
//!   MIR 降级会把它当成普通常量，运行期不再有任何代码。
//!
//! 终止性由两道闸保证：递归深度上限与求值步数上限。`comptime` 里写一个
//! 不收敛的循环会让编译器**报错退出**，而不是挂住——编译期求值一旦不
//! 终止，用户看到的是一个永远不返回的 `zr build`，没有任何线索。

use std::collections::HashMap;

use crate::typeck::hir::{
    BinaryOp, DefId, ExprKind, HirBlock, HirExpr, HirFunction, HirLiteral, HirStmt, LocalId, Ty,
    UnaryOp,
};

/// 递归调用深度上限。
///
/// `comptime fn` 允许递归（阶乘、斐波那契这类是主要用途），但必须有上界。
/// 65536 帧足够表达任何有意义的编译期计算，同时让深度失控在几秒内报错。
const MAX_DEPTH: usize = 4096;

/// 求值步数上限（单个调用点）。
///
/// 深度上限挡不住"深度有限但步数无限"的写法——尾递归形式的无限循环就是
/// 深度始终为 1 的死循环。步数预算与深度正交，两道闸一起才真正保证终止。
const MAX_STEPS: u64 = 4_000_000;

/// 求值失败。
#[derive(Debug, Clone)]
pub struct ComptimeError {
    pub message: String,
}

impl ComptimeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ComptimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

type Result<T> = std::result::Result<T, ComptimeError>;

/// 求值过程中的控制流信号。
pub enum Flow {
    /// 正常求出一个值。
    Value(HirLiteral),
    /// `return`：终止整个函数体。
    Return(HirLiteral),
    /// `break`：终止最内层循环。
    Break,
    /// `continue`：开始下一轮循环。
    Continue,
}

/// 求值环境：局部变量与全局常量的当前值。
#[derive(Default, Clone)]
pub struct Env {
    locals: HashMap<LocalId, HirLiteral>,
    /// 全局 `const` 的值（按需求值后缓存）。
    globals: HashMap<DefId, HirLiteral>,
}

/// comptime 求值器。
///
/// 借用类型检查器的函数表与常量表，因此求值**必须**发生在检查之后——
/// HIR 已经过名称解析与类型检查。
pub struct ComptimeEvaluator<'a> {
    functions: &'a HashMap<DefId, HirFunction>,
    depth: usize,
    steps: u64,
}

impl<'a> ComptimeEvaluator<'a> {
    /// 创建一个求值器。
    pub fn new(functions: &'a HashMap<DefId, HirFunction>) -> Self {
        Self {
            functions,
            depth: 0,
            steps: 0,
        }
    }

    /// 调用一个 comptime 函数并返回编译期结果。
    ///
    /// `args` 必须已经求值完毕（调用点保证：所有实参都是常量才会走到这里）。
    pub fn call(&mut self, callee: DefId, args: &[HirLiteral]) -> Result<HirLiteral> {
        let func = self
            .functions
            .get(&callee)
            .ok_or_else(|| ComptimeError::new("unknown function in compile-time call"))?;
        if !func.is_comptime {
            return Err(ComptimeError::new(format!(
                "`{}` is not a `comptime fn`; only comptime functions can be \
                 called from a compile-time context",
                func.name
            )));
        }
        let body = func.body.as_ref().ok_or_else(|| {
            ComptimeError::new(format!("`{}` has no body to evaluate", func.name))
        })?;
        if func.params.len() != args.len() {
            return Err(ComptimeError::new(format!(
                "`{}` expects {} argument(s), got {}",
                func.name,
                func.params.len(),
                args.len()
            )));
        }

        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(ComptimeError::new(format!(
                "compile-time evaluation exceeded the maximum recursion depth ({MAX_DEPTH}); \
                 `{}` is probably recursive without a base case",
                func.name
            )));
        }

        // 形参绑定：实参的值直接进环境，形参的 LocalId 作为键。
        let mut env = Env::default();
        for (param, value) in func.params.iter().zip(args.iter()) {
            env.locals.insert(param.local, value.clone());
        }

        let result = self.eval_block(body, &mut env);
        self.depth -= 1;

        match result? {
            Flow::Value(v) | Flow::Return(v) => Ok(v),
            // 函数体以 `break` / `continue` 结束在语法上不可能（没有循环），
            // 真发生了说明 HIR 降级出了问题。
            Flow::Break | Flow::Continue => Err(ComptimeError::new(format!(
                "`{}` ended with a loop control transfer outside of a loop",
                func.name
            ))),
        }
    }

    /// 求值一条表达式。
    fn eval_expr(&mut self, expr: &HirExpr, env: &mut Env) -> Result<Flow> {
        self.step()?;
        match &expr.kind {
            ExprKind::Literal(lit) => Ok(Flow::Value(lit.clone())),

            // 已经是折好的常量（`comptime` 表达式）：直接透传。
            ExprKind::Comptime(lit) => Ok(Flow::Value((**lit).clone())),

            ExprKind::Local(id) => match env.locals.get(id) {
                Some(v) => Ok(Flow::Value(v.clone())),
                None => Err(ComptimeError::new(
                    "read of a local variable that has no value in compile-time context",
                )),
            },

            ExprKind::Binary(op, lhs, rhs) => {
                let l = self.eval_value(lhs, env)?;
                let r = self.eval_value(rhs, env)?;
                Ok(Flow::Value(binary(*op, &l, &r)?))
            }

            ExprKind::Unary(op, operand) => {
                let v = self.eval_value(operand, env)?;
                Ok(Flow::Value(unary(*op, &v)?))
            }

            ExprKind::Cast(inner) => {
                let v = self.eval_value(inner, env)?;
                Ok(Flow::Value(cast(&v, &expr.ty)?))
            }

            ExprKind::Assign(lhs, rhs) => {
                let value = self.eval_value(rhs, env)?;
                if let ExprKind::Local(id) = &lhs.kind {
                    env.locals.insert(*id, value);
                }
                // 赋值本身在 zr 里返回 unit，这里用 unit 字面量表示。
                Ok(Flow::Value(HirLiteral::Int(0)))
            }

            ExprKind::Def(id) => match env.globals.get(id) {
                Some(v) => Ok(Flow::Value(v.clone())),
                None => Err(ComptimeError::new(
                    "reference to a global in compile-time context; \
                     only `const` items with constant initializers are available",
                )),
            },

            ExprKind::Block(block) => self.eval_block(block, env),

            ExprKind::If(cond, then_block, else_block) => {
                let c = self.eval_value(cond, env)?;
                let taken = match &c {
                    HirLiteral::Bool(b) => *b,
                    HirLiteral::Int(i) => *i != 0,
                    _ => {
                        return Err(ComptimeError::new(
                            "`if` condition in compile-time context must be a boolean",
                        ))
                    }
                };
                let branch = if taken {
                    Some(then_block)
                } else {
                    else_block.as_ref()
                };
                match branch {
                    Some(b) => self.eval_block(b, env),
                    // 没有 else 的假分支：unit。
                    None => Ok(Flow::Value(HirLiteral::Int(0))),
                }
            }

            ExprKind::While(cond, body) => loop {
                self.step()?;
                let c = self.eval_value(cond, env)?;
                if !truthy(&c) {
                    return Ok(Flow::Value(HirLiteral::Int(0)));
                }
                match self.eval_block(body, env)? {
                    Flow::Break => return Ok(Flow::Value(HirLiteral::Int(0))),
                    Flow::Continue | Flow::Value(_) => {}
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                }
            },

            ExprKind::Loop(body) => loop {
                self.step()?;
                match self.eval_block(body, env)? {
                    Flow::Break => return Ok(Flow::Value(HirLiteral::Int(0))),
                    Flow::Continue | Flow::Value(_) => {}
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                }
            },

            ExprKind::Break => Ok(Flow::Break),
            ExprKind::Continue => Ok(Flow::Continue),

            ExprKind::Return(value) => {
                let v = match value {
                    Some(e) => self.eval_value(e, env)?,
                    None => HirLiteral::Int(0),
                };
                Ok(Flow::Return(v))
            }

            ExprKind::Call {
                callee,
                args,
                result,
            } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval_value(a, env)?);
                }
                let v = self.call(*callee, &vals)?;
                // 调用结果绑到 `result` 这个临时局部，后续引用能读到。
                env.locals.insert(*result, v.clone());
                Ok(Flow::Value(v))
            }

            other => Err(ComptimeError::new(format!(
                "`{}` is not supported in compile-time evaluation",
                describe(other)
            ))),
        }
    }

    /// 求值一条必须产出值的表达式（语句位置 / 操作数位置）。
    fn eval_value(&mut self, expr: &HirExpr, env: &mut Env) -> Result<HirLiteral> {
        match self.eval_expr(expr, env)? {
            Flow::Value(v) | Flow::Return(v) => Ok(v),
            Flow::Break => Err(ComptimeError::new(
                "`break` in compile-time context must be inside a loop",
            )),
            Flow::Continue => Err(ComptimeError::new(
                "`continue` in compile-time context must be inside a loop",
            )),
        }
    }

    /// 求值一个块：执行全部语句，再取尾表达式。
    fn eval_block(&mut self, block: &HirBlock, env: &mut Env) -> Result<Flow> {
        for stmt in &block.stmts {
            match stmt {
                HirStmt::Let { local, init } | HirStmt::Var { local, init } => {
                    let value = match init {
                        // 没有初始化式：局部变量先是"无值"，后面必然有
                        // 一次赋值（`let x; x = 3;`）。
                        None => continue,
                        Some(e) => self.eval_value(e, env)?,
                    };
                    env.locals.insert(*local, value);
                }
                HirStmt::Expr(e) => match self.eval_expr(e, env)? {
                    // `return` 直接终止函数体。
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                    // 语句位置的 break / continue 属于调用方的循环，
                    // 这里当作"这一轮结束"处理。
                    Flow::Break | Flow::Continue => return Ok(Flow::Value(HirLiteral::Int(0))),
                    Flow::Value(_) => {}
                },
                HirStmt::Item => {}
            }
        }
        match &block.tail {
            Some(tail) => self.eval_expr(tail, env),
            None => Ok(Flow::Value(HirLiteral::Int(0))),
        }
    }

    /// 消耗一步预算。
    fn step(&mut self) -> Result<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(ComptimeError::new(format!(
                "compile-time evaluation exceeded {MAX_STEPS} steps; \
                 the expression does not terminate"
            )));
        }
        Ok(())
    }
}

/// 字面量真值判定。
fn truthy(lit: &HirLiteral) -> bool {
    match lit {
        HirLiteral::Bool(b) => *b,
        HirLiteral::Int(i) => *i != 0,
        _ => false,
    }
}

/// 二元常量运算。
fn binary(op: BinaryOp, l: &HirLiteral, r: &HirLiteral) -> Result<HirLiteral> {
    use BinaryOp::*;
    // 算术 / 位运算：两侧必须是同一类数值。
    let arith = matches!(
        op,
        Add | Sub | Mul | Div | Rem | BitAnd | BitOr | BitXor | Shl | Shr
    );
    if arith {
        return match (l, r) {
            (HirLiteral::Int(a), HirLiteral::Int(b)) => int_binary(op, *a, *b).map(HirLiteral::Int),
            (HirLiteral::Float(a), HirLiteral::Float(b)) => {
                float_binary(op, *a, *b).map(HirLiteral::Float)
            }
            // char 与整数同构
            (HirLiteral::Char(a), HirLiteral::Int(b)) => {
                int_binary(op, *a as i128, *b).map(HirLiteral::Int)
            }
            (HirLiteral::Int(a), HirLiteral::Char(b)) => {
                int_binary(op, *a, *b as i128).map(HirLiteral::Int)
            }
            _ => Err(ComptimeError::new(
                "mismatched operand types in compile-time expression",
            )),
        };
    }
    match op {
        Eq => Ok(HirLiteral::Bool(lit_eq(l, r))),
        Ne => Ok(HirLiteral::Bool(!lit_eq(l, r))),
        Lt | Le | Gt | Ge => Ok(HirLiteral::Bool(ord(op, l, r)?)),
        // 短路：左边定了就不再看右边（但右边仍须是常量，否则报错）。
        And => match truthy(l) {
            false => Ok(HirLiteral::Bool(false)),
            true => Ok(HirLiteral::Bool(truthy(r))),
        },
        Or => match truthy(l) {
            true => Ok(HirLiteral::Bool(true)),
            false => Ok(HirLiteral::Bool(truthy(r))),
        },
        _ => Err(ComptimeError::new(format!(
            "operator `{}` is not supported in compile-time evaluation",
            binary_op_name(op)
        ))),
    }
}

/// 整数常量运算。
fn int_binary(op: BinaryOp, a: i128, b: i128) -> Result<i128> {
    use BinaryOp::*;
    Ok(match op {
        Add => a.wrapping_add(b),
        Sub => a.wrapping_sub(b),
        Mul => a.wrapping_mul(b),
        Div => {
            if b == 0 {
                return Err(ComptimeError::new(
                    "division by zero in compile-time expression",
                ));
            }
            a.wrapping_div(b)
        }
        Rem => {
            if b == 0 {
                return Err(ComptimeError::new(
                    "remainder by zero in compile-time expression",
                ));
            }
            a.wrapping_rem(b)
        }
        BitAnd => a & b,
        BitOr => a | b,
        BitXor => a ^ b,
        // 移位按**64 位**语义折叠，而不是 IR 的 i128 语义。
        //
        // 值在后端是躺在 64 位寄存器里的：x86 的 `shl` 只看 CL 的低 6 位，
        // AArch64 / RISC-V 也是 64 位运算。用 i128 折叠会得到运行期永远
        // 得不到的结果——`comptime 1 << 63` 折出 +2^63，运行期算出来却是
        // i64::MIN。编译期折出的值必须与运行期**逐位相同**，否则同一个常量在
        // 两种路径下行为不一致，比不折叠糟糕得多。
        //
        // 移位量 >= 64 同样没有良定义结果（移 64 位与移 0 位同义），报错。
        Shl | Shr => {
            if !(0..64).contains(&b) {
                return Err(ComptimeError::new(format!(
                    "shift count {b} is out of range for a 64-bit value"
                )));
            }
            let n = b as u32;
            let word = a as i64;
            let shifted = if op == Shl { word << n } else { word >> n };
            shifted as i128
        }
        _ => {
            return Err(ComptimeError::new(format!(
                "operator `{}` is not an integer operation",
                binary_op_name(op)
            )))
        }
    })
}

/// 浮点常量运算。
fn float_binary(op: BinaryOp, a: f64, b: f64) -> Result<f64> {
    use BinaryOp::*;
    Ok(match op {
        Add => a + b,
        Sub => a - b,
        Mul => a * b,
        Div => {
            if b == 0.0 {
                return Err(ComptimeError::new(
                    "division by zero in compile-time expression",
                ));
            }
            a / b
        }
        // `%` / 位运算对浮点没有意义。
        _ => {
            return Err(ComptimeError::new(format!(
                "operator `{}` is not defined for floating-point values",
                binary_op_name(op)
            )))
        }
    })
}

/// 字面量相等（跨类型一律判不等，与运行期一致）。
fn lit_eq(l: &HirLiteral, r: &HirLiteral) -> bool {
    match (l, r) {
        (HirLiteral::Int(a), HirLiteral::Int(b)) => a == b,
        (HirLiteral::Float(a), HirLiteral::Float(b)) => a == b,
        (HirLiteral::Bool(a), HirLiteral::Bool(b)) => a == b,
        (HirLiteral::String(a), HirLiteral::String(b)) => a == b,
        (HirLiteral::Char(a), HirLiteral::Char(b)) => a == b,
        (HirLiteral::Char(a), HirLiteral::Int(b)) | (HirLiteral::Int(b), HirLiteral::Char(a)) => {
            *a as i128 == *b
        }
        _ => false,
    }
}

/// 有序比较（`<` / `<=` / `>` / `>=`）。
fn ord(op: BinaryOp, l: &HirLiteral, r: &HirLiteral) -> Result<bool> {
    use BinaryOp::*;
    let ord = |o: std::cmp::Ordering| match op {
        Lt => o.is_lt(),
        Le => o.is_le(),
        Gt => o.is_gt(),
        Ge => o.is_ge(),
        _ => unreachable!("只对有序比较调用"),
    };
    match (l, r) {
        (HirLiteral::Int(a), HirLiteral::Int(b)) => Ok(ord(a.cmp(b))),
        (HirLiteral::Float(a), HirLiteral::Float(b)) => a
            .partial_cmp(b)
            .map(ord)
            .ok_or_else(|| ComptimeError::new("cannot order NaN in compile-time expression")),
        (HirLiteral::Char(a), HirLiteral::Char(b)) => Ok(ord(a.cmp(b))),
        (HirLiteral::Char(a), HirLiteral::Int(b)) => Ok(ord((*a as i128).cmp(b))),
        (HirLiteral::Int(a), HirLiteral::Char(b)) => Ok(ord(a.cmp(&(*b as i128)))),
        (HirLiteral::String(a), HirLiteral::String(b)) => Ok(ord(a.cmp(b))),
        (HirLiteral::Bool(a), HirLiteral::Bool(b)) => Ok(ord(a.cmp(b))),
        _ => Err(ComptimeError::new(
            "mismatched operand types in compile-time comparison",
        )),
    }
}

/// 一元常量运算。
fn unary(op: UnaryOp, v: &HirLiteral) -> Result<HirLiteral> {
    use UnaryOp::*;
    match (op, v) {
        (Not, HirLiteral::Bool(b)) => Ok(HirLiteral::Bool(!b)),
        (Not, HirLiteral::Int(i)) => Ok(HirLiteral::Bool(*i == 0)),
        (Not, other) => Err(ComptimeError::new(format!(
            "`!` is not defined for {}",
            literal_kind(other)
        ))),
        (BitNot, HirLiteral::Int(i)) => Ok(HirLiteral::Int(!*i)),
        (BitNot, HirLiteral::Char(c)) => Ok(HirLiteral::Int(!(*c as i128))),
        (Neg, HirLiteral::Int(i)) => Ok(HirLiteral::Int(i.wrapping_neg())),
        (Neg, HirLiteral::Float(f)) => Ok(HirLiteral::Float(-f)),
        (Neg, other) => Err(ComptimeError::new(format!(
            "unary `-` is not defined for {}",
            literal_kind(other)
        ))),
        // 解引用 / 取地址：编译期没有内存可解。
        (Deref | AddrOf, _) => Err(ComptimeError::new(
            "pointer operations are not supported in compile-time evaluation",
        )),
        // 其余组合（`~f64` / `~true` / `~str`）无意义。
        (_, other) => Err(ComptimeError::new(format!(
            "this unary operator is not defined for {}",
            literal_kind(other)
        ))),
    }
}

/// 数值转换 `v as T`（目标类型取自表达式节点的 `ty`）。
fn cast(v: &HirLiteral, target: &Ty) -> Result<HirLiteral> {
    match target {
        Ty::Int(bits) => {
            let raw = to_i128(v)?;
            Ok(HirLiteral::Int(truncate_int(raw, *bits, true)))
        }
        Ty::Uint(bits) => {
            let raw = to_i128(v)?;
            Ok(HirLiteral::Int(truncate_int(raw, *bits, false)))
        }
        Ty::Float(bits) => {
            let f = match v {
                HirLiteral::Float(f) => *f,
                HirLiteral::Int(i) => *i as f64,
                HirLiteral::Char(c) => *c as u32 as f64,
                other => {
                    return Err(ComptimeError::new(format!(
                        "cannot convert {} to a floating-point type in compile-time context",
                        literal_kind(other)
                    )))
                }
            };
            // f32 要先过一次 f32 舍入，否则 `0.1f32` 折出来的位模式是
            // f64 的 0.1，赋给 f32 时还要再舍一次，可能差一个 ulp。
            Ok(HirLiteral::Float(if *bits <= 32 {
                f as f32 as f64
            } else {
                f
            }))
        }
        Ty::Bool => Ok(HirLiteral::Bool(truthy(v))),
        Ty::Char => {
            let raw = to_i128(v)?;
            match u32::try_from(raw).ok().and_then(char::from_u32) {
                Some(c) => Ok(HirLiteral::Char(c)),
                None => Err(ComptimeError::new(format!(
                    "{raw} is not a valid Unicode scalar value"
                ))),
            }
        }
        other => Err(ComptimeError::new(format!(
            "cast to `{other:?}` is not supported in compile-time evaluation"
        ))),
    }
}

/// 字面量 → 整数（bool 按 0/1）。
fn to_i128(v: &HirLiteral) -> Result<i128> {
    match v {
        HirLiteral::Int(i) => Ok(*i),
        HirLiteral::Char(c) => Ok(*c as i128),
        HirLiteral::Bool(b) => Ok(i128::from(*b)),
        HirLiteral::Float(f) => Ok(*f as i128),
        other => Err(ComptimeError::new(format!(
            "cannot convert {} to an integer in compile-time context",
            literal_kind(other)
        ))),
    }
}

/// 按目标位宽截断并做符号扩展 / 零扩展。
fn truncate_int(raw: i128, bits: u16, signed: bool) -> i128 {
    if bits == 0 || bits >= 128 {
        return raw;
    }
    let mask = (1i128 << bits) - 1;
    let low = raw & mask;
    if signed && bits < 128 && (low >> (bits - 1)) & 1 == 1 {
        // 符号位为 1：补上前导 1。
        low - (1i128 << bits)
    } else {
        low
    }
}

/// 运算符的可读名字（用于报错）。
fn binary_op_name(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Rem => "%",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Gt => ">",
        Le => "<=",
        Ge => ">=",
        And => "&&",
        Or => "||",
        Range => "..",
        RangeInclusive => "..=",
    }
}

/// 字面量的类别名（用于报错）。
fn literal_kind(v: &HirLiteral) -> &'static str {
    match v {
        HirLiteral::Int(_) => "an integer",
        HirLiteral::Float(_) => "a floating-point value",
        HirLiteral::Bool(_) => "a boolean",
        HirLiteral::String(_) => "a string",
        HirLiteral::Char(_) => "a char",
    }
}

/// 不受支持的表达式种类的可读名字。
fn describe(kind: &ExprKind) -> &'static str {
    match kind {
        ExprKind::Match => "`match`",
        ExprKind::For => "`for`",
        ExprKind::Field(_) => "field access",
        ExprKind::Deref(_) => "dereference",
        ExprKind::AddrOf => "address-of",
        ExprKind::Index(_) => "indexing",
        ExprKind::Break => "`break`",
        ExprKind::Continue => "`continue`",
        ExprKind::Defer => "`defer`",
        ExprKind::Try => "`try`",
        ExprKind::Throw => "`throw`",
        ExprKind::Array => "an array literal",
        ExprKind::Tuple => "a tuple literal",
        ExprKind::Struct => "a struct literal",
        ExprKind::Closure => "a closure",
        ExprKind::Await(_) => "`await`",
        ExprKind::Asm => "inline assembly",
        ExprKind::Yield => "`yield`",
        ExprKind::Spawn => "`spawn`",
        ExprKind::Intrinsic => "a compile-time intrinsic",
        _ => "this expression",
    }
}

/// 递归重写 HIR：把每个「comptime fn + 全常量实参」的调用点折成字面量。
///
/// 返回替换掉的调用点个数（用于诊断信息）。
pub fn fold_comptime_calls(
    expr: &mut HirExpr,
    functions: &HashMap<DefId, HirFunction>,
    depth: usize,
    steps: &mut u64,
) -> usize {
    // 先处理子表达式，再决定自己是否可折叠（自底向上）。
    let mut folded = 0;
    for child in children_mut(expr) {
        folded += fold_comptime_calls(child, functions, depth, steps);
    }

    // 先取出调用点信息，再改写 `expr.kind`——否则 `&expr.kind` 的借用还活着
    // 的时候赋值会撞上 E0506。
    let call = match &expr.kind {
        ExprKind::Call { callee, args, .. } => Some((*callee, args.clone())),
        _ => None,
    };
    let Some((callee, args)) = call else {
        return folded;
    };

    let Some(func) = functions.get(&callee) else {
        return folded;
    };
    if !func.is_comptime || func.body.is_none() {
        return folded;
    }
    *steps += 1;
    if *steps > MAX_STEPS || depth > MAX_DEPTH {
        return folded;
    }

    // 实参必须全是常量；有一个不是就保留调用（留给运行期）。
    let mut vals = Vec::with_capacity(args.len());
    for a in args.iter() {
        match &a.kind {
            ExprKind::Literal(lit) => vals.push(lit.clone()),
            ExprKind::Comptime(lit) => vals.push((**lit).clone()),
            _ => return folded,
        }
    }

    let mut ev = ComptimeEvaluator::new(functions);
    if let Ok(value) = ev.call(callee, &vals) {
        expr.kind = ExprKind::Comptime(Box::new(value));
        folded += 1;
    }
    folded
}

// ==================================================================
// AST 层的编译期求值
// ==================================================================

/// AST 层的 comptime 求值上下文。
///
/// 存在的理由是**求值时机**：`comptime fact(4)` 出现在某个函数体里，而类型
/// 检查器是**顺序**检查函数体的——用到 `fact` 的那个函数完全可能排在
/// `fact` 自己前面。等到 HIR 版的 [`fold_comptime_calls`] 能跑的时候，
/// 这个 `comptime` 表达式的类型早就定下来了。
///
/// 所以即时求值走 AST：此时 `fact` 的语法树已经完整（声明阶段收完了），
/// 直接解释语法树即可。
pub struct AstEvaluator<'a> {
    functions: &'a HashMap<String, crate::parser::ast::Function>,
    depth: usize,
    steps: u64,
}

/// AST 层的求值环境（按名字绑定，允许遮蔽）。
type AstEnv = Vec<(String, HirLiteral)>;

impl<'a> AstEvaluator<'a> {
    /// 创建求值器。
    pub fn new(functions: &'a HashMap<String, crate::parser::ast::Function>) -> Self {
        Self {
            functions,
            depth: 0,
            steps: 0,
        }
    }

    /// 调用一个 `comptime fn`（按名字）。
    pub fn call(&mut self, name: &str, args: &[HirLiteral]) -> Result<HirLiteral> {
        use crate::parser::ast;
        let func = self
            .functions
            .get(name)
            .ok_or_else(|| ComptimeError::new(format!("unknown function `{name}`")))?;
        if !func.is_comptime {
            return Err(ComptimeError::new(format!(
                "`{name}` is not a `comptime fn`; only comptime functions can be \
                 be called from a compile-time context"
            )));
        }
        let body = func
            .body
            .as_ref()
            .ok_or_else(|| ComptimeError::new(format!("`{name}` has no body to evaluate")))?;
        if func.params.len() != args.len() {
            return Err(ComptimeError::new(format!(
                "`{name}` expects {} argument(s), got {}",
                func.params.len(),
                args.len()
            )));
        }

        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(ComptimeError::new(format!(
                "compile-time evaluation of `{name}` exceeded the maximum recursion \
                 depth ({MAX_DEPTH}); is it recursive without a base case?"
            )));
        }

        // 形参按**模式**的名字绑定：只支持标识符模式。解构形参在编译期求值
        // 里没有对应的值可解，给出明确报错而不是静默忽略。
        let mut env: AstEnv = Vec::with_capacity(func.params.len());
        for (param, value) in func.params.iter().zip(args.iter()) {
            match &param.pattern {
                ast::Pattern::Binding(name, _, None, _) => env.push((name.clone(), value.clone())),
                other => {
                    self.depth -= 1;
                    return Err(ComptimeError::new(format!(
                        "`{name}` has a destructuring parameter ({}), which is not \
                         supported in compile-time evaluation",
                        pattern_kind(other)
                    )));
                }
            }
        }

        let result = self.eval_block(body, &mut env);
        self.depth -= 1;
        match result? {
            AstFlow::Value(v) | AstFlow::Break(v) | AstFlow::Return(v) => {
                Ok(v.unwrap_or(HirLiteral::Int(0)))
            }
            // 函数体以 `continue` 结束在语法上不可能（没有循环），
            // 真发生了说明 AST 降级出了问题。
            AstFlow::Continue => Err(ComptimeError::new(format!(
                "`{name}` ended with a loop control transfer outside of a loop"
            ))),
        }
    }

    /// 求值一个 AST 块，返回控制流信号。
    fn eval_block(
        &mut self,
        block: &crate::parser::ast::Block,
        env: &mut AstEnv,
    ) -> Result<AstFlow> {
        use crate::parser::ast;
        let base = env.len();
        for stmt in &block.stmts {
            match stmt {
                ast::Statement::Let(pat, _, init, _) | ast::Statement::Var(pat, _, init, _) => {
                    let Some(init) = init else { continue };
                    let value = self.value_of(init, env)?;
                    match pat {
                        ast::Pattern::Binding(name, _, None, _) => env.push((name.clone(), value)),
                        // `let (a, b) = ...` 在编译期没有可解的值
                        other => {
                            return Err(ComptimeError::new(format!(
                                "destructuring binding ({}) is not supported in \
                                 compile-time evaluation",
                                pattern_kind(other)
                            )))
                        }
                    }
                }
                ast::Statement::Expr(e, _) => {
                    // 语句位置：求值即可，`return` / `break` 会作为信号透出。
                    match self.eval_expr(e, env)? {
                        AstFlow::Value(_) | AstFlow::Continue => {}
                        other => {
                            // break / continue / return 终止当前块，原样上传。
                            env.truncate(base);
                            return Ok(other);
                        }
                    }
                }
                // `const` 语句：编译期求值，等价于一次常量绑定。
                ast::Statement::Const(name, _, init, _) => {
                    let value = self.value_of(init, env)?;
                    env.push((name.clone(), value));
                }
            }
        }
        let flow = match &block.tail {
            Some(tail) => self.eval_expr(tail, env)?,
            None => AstFlow::Value(None),
        };
        // 离开作用域：弹出本块引入的绑定，避免同名变量泄漏到外层。
        env.truncate(base);
        Ok(flow)
    }

    /// 求值一条必须产出值的 AST 表达式（操作数 / `let` 初值位置）。
    fn value_of(
        &mut self,
        expr: &crate::parser::ast::Expression,
        env: &mut AstEnv,
    ) -> Result<HirLiteral> {
        match self.eval_expr(expr, env)? {
            AstFlow::Value(v) => Ok(v.unwrap_or(HirLiteral::Int(0))),
            // `break v` / `return v` 的值就是 `v`，在值位置与普通求值同义。
            AstFlow::Break(v) | AstFlow::Return(v) => Ok(v.unwrap_or(HirLiteral::Int(0))),
            AstFlow::Continue => Err(ComptimeError::new(
                "`continue` in compile-time context must be inside a loop",
            )),
        }
    }

    /// 求值一条 AST 表达式。
    fn eval_expr(
        &mut self,
        expr: &crate::parser::ast::Expression,
        env: &mut AstEnv,
    ) -> Result<AstFlow> {
        use crate::parser::ast;
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(ComptimeError::new(format!(
                "compile-time evaluation exceeded {MAX_STEPS} steps; \
                 the expression does not terminate"
            )));
        }

        // 值位置的包装层：绝大多数表达式种类直接产出一个值。
        macro_rules! val {
            ($e:expr) => {
                AstFlow::Value(Some($e))
            };
        }

        Ok(match expr {
            ast::Expression::Literal(lit, _) => val!(literal_of(lit)?),

            ast::Expression::Variable(name, _) => match lookup(env, name) {
                Some(v) => val!(v),
                None => {
                    return Err(ComptimeError::new(format!(
                        "`{name}` cannot be used in a compile-time context"
                    )))
                }
            },

            ast::Expression::Binary(lhs, op, rhs, _) => {
                let l = self.value_of(lhs, env)?;
                let r = self.value_of(rhs, env)?;
                val!(binary(hir_binary_op(*op), &l, &r)?)
            }

            ast::Expression::Unary(op, operand, _) => {
                let v = self.value_of(operand, env)?;
                val!(unary(hir_unary_op(*op), &v)?)
            }

            ast::Expression::Comptime(inner, _) => AstFlow::Value(Some(self.value_of(inner, env)?)),

            // 赋值：`x = e` 与 `x op= e`。
            //
            // 复合赋值（`x += e`）在语法树里是独立的 `AssignOp`，这里折成
            // "读 x -> 运算 -> 写回 x"。不处理的话 `while` 里最常见的
            // `total += i` 会直接报"不支持"，累加类的 comptime 函数全都用不了。
            ast::Expression::Assign(lhs, op, rhs, _) => {
                let value = self.value_of(rhs, env)?;
                let combined = match op {
                    ast::AssignOp::Assign => value,
                    ast::AssignOp::AddAssign
                    | ast::AssignOp::SubAssign
                    | ast::AssignOp::MulAssign
                    | ast::AssignOp::DivAssign
                    | ast::AssignOp::RemAssign
                    | ast::AssignOp::AndAssign
                    | ast::AssignOp::OrAssign
                    | ast::AssignOp::XorAssign
                    | ast::AssignOp::ShlAssign
                    | ast::AssignOp::ShrAssign => {
                        let current = match callee_name(lhs) {
                            Some(name) => lookup(env, &name).ok_or_else(|| {
                                ComptimeError::new(format!(
                                    "`{name}` is read before it is bound in a \
                                     compile-time context"
                                ))
                            })?,
                            None => {
                                return Err(ComptimeError::new(
                                    "compound assignment only supports simple variables \
                                     in compile-time evaluation",
                                ))
                            }
                        };
                        let hir_op = assign_op_to_binary(*op);
                        binary(hir_op, &current, &value)?
                    }
                };
                let name = callee_name(lhs).ok_or_else(|| {
                    ComptimeError::new(
                        "assignment target must be a simple variable in compile-time \
                         evaluation",
                    )
                })?;
                bind(env, &name, combined.clone());
                val!(combined)
            }

            ast::Expression::Block(b, _) => self.eval_block(b, env)?,

            // `if`：条件必须是编译期常量，只求值被选中的那一支。
            //
            // 这正是 comptime 的用处：另一支里的代码即使引用了运行时才有的
            // 变量也不会被碰到（`comptime { if N > 0 { size(N) } else { 1 } }`）。
            ast::Expression::If(cond, then_block, else_branch, _) => {
                let c = self.value_of(cond, env)?;
                if truthy(&c) {
                    self.eval_block(then_block, env)?
                } else {
                    // `else` 后面既可能是块（`else { ... }`）也可能是裸表达式
                    // （`else expr`），两种形态都要接住。
                    match else_branch {
                        Some(e) => AstFlow::Value(Some(self.value_of(e, env)?)),
                        None => AstFlow::Value(None),
                    }
                }
            }

            // `while`：编译期循环，带步数预算兜底。
            ast::Expression::While(cond, body, _, _) => loop {
                self.bump()?;
                if !truthy(&self.value_of(cond, env)?) {
                    break AstFlow::Value(None);
                }
                match self.eval_block(body, env)? {
                    AstFlow::Break(v) => break AstFlow::Value(v),
                    AstFlow::Return(v) => break AstFlow::Return(v),
                    AstFlow::Continue | AstFlow::Value(_) => {}
                }
            },

            ast::Expression::Loop(body, _) => loop {
                self.bump()?;
                match self.eval_block(body, env)? {
                    AstFlow::Break(v) => break AstFlow::Value(v),
                    AstFlow::Return(v) => break AstFlow::Return(v),
                    AstFlow::Continue | AstFlow::Value(_) => {}
                }
            },

            ast::Expression::Break(_, value, _) => match value {
                Some(e) => AstFlow::Break(Some(self.value_of(e, env)?)),
                None => AstFlow::Break(None),
            },

            ast::Expression::Continue(..) => AstFlow::Continue,

            ast::Expression::Return(value, _) => AstFlow::Return(Some(match value {
                Some(e) => self.value_of(e, env)?,
                None => HirLiteral::Int(0),
            })),

            // 函数调用：被调者必须是 `comptime fn` 且实参都是常量。
            ast::Expression::Call(callee, args, _) => {
                let name = callee_name(callee).ok_or_else(|| {
                    ComptimeError::new(
                        "only direct calls to `comptime fn` are supported \
                         in compile-time contexts",
                    )
                })?;
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    match a {
                        ast::Argument::Pos(e) => vals.push(self.value_of(e, env)?),
                        ast::Argument::Named(_, _) => {
                            return Err(ComptimeError::new(
                                "named arguments are not supported in compile-time evaluation",
                            ))
                        }
                    }
                }
                val!(self.call(&name, &vals)?)
            }

            other => {
                return Err(ComptimeError::new(format!(
                    "{} is not supported in compile-time evaluation",
                    describe_ast(other)
                )))
            }
        })
    }

    /// 消耗一步循环迭代预算。
    ///
    /// 与表达式步数预算是**同一个**计数器：两者都是"求值做了多少工作"的
    /// 度量，分开记的话一个内层死循环能把循环预算耗光而步数预算还满着。
    fn bump(&mut self) -> Result<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(ComptimeError::new(format!(
                "compile-time loop exceeded {MAX_STEPS} iterations; \
                 the loop does not terminate for these arguments"
            )));
        }
        Ok(())
    }
}

/// AST 层的控制流信号。
///
/// `Value(None)` 表示"正常求值、结果为 unit"（循环体还要继续转下一轮）；
/// `Value(Some(v))` 是 `return` / `break value` 的值，会终止外层。
enum AstFlow {
    Value(Option<HirLiteral>),
    Break(Option<HirLiteral>),
    Continue,
    /// `return v`：终止整个函数体。
    ///
    /// 必须与 `Value` 分开。两者都携带"值"，但语义完全不同：`Value` 是普通
    /// 求值结果（语句位置会被丢弃），`Return` 要一路传到函数边界。早先只
    /// 有 `Value`，于是 `comptime fn three() -> i32 { return 3; }` 里的
    /// `return 3` 被当成一条普通语句求值后**丢弃**，函数返回 unit（0）——
    /// 编译成功，值却是错的。
    Return(Option<HirLiteral>),
}

/// 从被调表达式里取出函数名（只认变量 / 路径，不认 `obj.method(...)`）。
fn callee_name(e: &crate::parser::ast::Expression) -> Option<String> {
    use crate::parser::ast::Expression;
    match e {
        Expression::Variable(name, _) => Some(name.clone()),
        Expression::Field(inner, ..) => callee_name(inner),
        _ => None,
    }
}

/// AST 二元运算符 → HIR 二元运算符。
///
/// AST 与 HIR 的运算符集合一一对应（`..` / `..=` 只是保留给 HIR，AST 侧
/// 用不到），所以这是一次纯映射：收敛到一处之后，两个求值器共用同一套折叠。
fn hir_binary_op(op: crate::parser::ast::BinaryOp) -> BinaryOp {
    use crate::parser::ast::BinaryOp as A;
    match op {
        A::Add => BinaryOp::Add,
        A::Sub => BinaryOp::Sub,
        A::Mul => BinaryOp::Mul,
        A::Div => BinaryOp::Div,
        A::Rem => BinaryOp::Rem,
        A::BitAnd => BinaryOp::BitAnd,
        A::BitOr => BinaryOp::BitOr,
        A::BitXor => BinaryOp::BitXor,
        A::Shl => BinaryOp::Shl,
        A::Shr => BinaryOp::Shr,
        A::Eq => BinaryOp::Eq,
        A::Ne => BinaryOp::Ne,
        A::Lt => BinaryOp::Lt,
        A::Gt => BinaryOp::Gt,
        A::Le => BinaryOp::Le,
        A::Ge => BinaryOp::Ge,
        A::And => BinaryOp::And,
        A::Or => BinaryOp::Or,
        // 范围运算符不是真运算（只出现在模式里），映射过去只为穷尽性。
        A::Range => BinaryOp::Range,
        A::RangeInclusive => BinaryOp::RangeInclusive,
    }
}

/// AST 一元运算符 → HIR 一元运算符。
fn hir_unary_op(op: crate::parser::ast::UnaryOp) -> UnaryOp {
    use crate::parser::ast::UnaryOp as A;
    match op {
        A::Neg => UnaryOp::Neg,
        A::Not => UnaryOp::Not,
        A::BitNot => UnaryOp::BitNot,
        A::Deref => UnaryOp::Deref,
        A::AddrOf => UnaryOp::AddrOf,
    }
}

/// 复合赋值运算符 -> 对应的二元运算符。
fn assign_op_to_binary(op: crate::parser::ast::AssignOp) -> BinaryOp {
    use crate::parser::ast::AssignOp as A;
    match op {
        A::Assign => BinaryOp::Add, // 不会走到这里
        A::AddAssign => BinaryOp::Add,
        A::SubAssign => BinaryOp::Sub,
        A::MulAssign => BinaryOp::Mul,
        A::DivAssign => BinaryOp::Div,
        A::RemAssign => BinaryOp::Rem,
        A::AndAssign => BinaryOp::BitAnd,
        A::OrAssign => BinaryOp::BitOr,
        A::XorAssign => BinaryOp::BitXor,
        A::ShlAssign => BinaryOp::Shl,
        A::ShrAssign => BinaryOp::Shr,
    }
}

/// 绑定一个名字。
///
/// `let` 引入新绑定（压栈），而**赋值必须就地改写**已有绑定。早先赋值也走
/// `push`，于是 `while i < n { total += i }` 每轮的更新都被 `eval_block`
/// 收尾时的 `env.truncate(base)` 回滚——循环变量永远停在初值，编译期求值
/// 直接跑到步数上限才报错。
fn bind(env: &mut AstEnv, name: &str, value: HirLiteral) {
    if let Some(slot) = env.iter_mut().rev().find(|(n, _)| n == name) {
        slot.1 = value;
    } else {
        env.push((name.to_string(), value));
    }
}

/// 环境里查找变量（从内层往外）。
fn lookup(env: &AstEnv, name: &str) -> Option<HirLiteral> {
    env.iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.clone())
}

/// AST 字面量 → HIR 字面量。
fn literal_of(lit: &crate::parser::ast::Literal) -> Result<HirLiteral> {
    use crate::parser::ast::Literal;
    Ok(match lit {
        Literal::Int(v, _) => HirLiteral::Int(*v),
        Literal::Float(v, _) => HirLiteral::Float(*v),
        Literal::Bool(v) => HirLiteral::Bool(*v),
        Literal::Char(c) => HirLiteral::Char(*c),
        Literal::String(s) => HirLiteral::String(s.clone()),
        Literal::Byte(b) => HirLiteral::Int(*b as i128),
        Literal::ByteString(b) => HirLiteral::String(String::from_utf8_lossy(b).into_owned()),
    })
}

/// 模式的可读名字（用于报错）。
fn pattern_kind(p: &crate::parser::ast::Pattern) -> &'static str {
    use crate::parser::ast::Pattern;
    match p {
        Pattern::Binding(..) => "binding",
        Pattern::Wildcard(_) => "wildcard",
        Pattern::Tuple(..) => "tuple",
        Pattern::Struct { .. } => "struct",
        Pattern::Variant { .. } => "enum variant",
        Pattern::Slice(..) => "slice",
        Pattern::Ref(..) => "reference",
        Pattern::Literal(..) => "literal",
        Pattern::Range(..) => "range",
    }
}

/// 不受支持的 AST 表达式的可读名字。
fn describe_ast(e: &crate::parser::ast::Expression) -> &'static str {
    use crate::parser::ast::Expression;
    match e {
        Expression::Match(..) => "`match`",
        Expression::For(..) => "`for`",
        Expression::StructInit(..) => "a struct literal",
        Expression::Array(..) => "an array literal",
        Expression::Tuple(..) => "a tuple literal",
        Expression::Lambda(..) => "a closure",
        Expression::Await(..) => "`await`",
        Expression::Asm(..) => "inline assembly",
        Expression::Yield(..) => "`yield`",
        Expression::Spawn(..) => "`spawn`",
        Expression::Intrinsic(..) => "a compile-time intrinsic",
        Expression::Cast(..) => "a cast",
        Expression::Field(..) => "field access",
        Expression::Unary(crate::parser::ast::UnaryOp::Deref, ..) => "dereference",
        Expression::AddrOf(..) => "address-of",
        Expression::Index(..) => "indexing",
        _ => "this expression",
    }
}

/// 可变的子表达式列表。
fn children_mut(expr: &mut HirExpr) -> Vec<&mut HirExpr> {
    match &mut expr.kind {
        ExprKind::Binary(_, a, b) | ExprKind::Assign(a, b) => vec![a, b],
        ExprKind::Unary(_, a) | ExprKind::Field(a) | ExprKind::Deref(a) | ExprKind::Index(a) => {
            vec![a]
        }
        ExprKind::Cast(a) | ExprKind::Await(a) => vec![a],
        ExprKind::If(c, then_block, else_block) => {
            let mut out = vec![c.as_mut()];
            out.extend(block_children_mut(then_block));
            if let Some(b) = else_block {
                out.extend(block_children_mut(b));
            }
            out
        }
        ExprKind::Block(b) => block_children_mut(b),
        ExprKind::While(c, b) => {
            let mut out = vec![c.as_mut()];
            out.extend(block_children_mut(b));
            out
        }
        ExprKind::Loop(b) => block_children_mut(b),
        ExprKind::Return(Some(e)) => vec![e.as_mut()],
        ExprKind::Call { args, .. } => args.iter_mut().collect(),
        _ => Vec::new(),
    }
}

/// 块内全部可变子表达式（语句里的表达式 + 尾表达式）。
fn block_children_mut(block: &mut HirBlock) -> Vec<&mut HirExpr> {
    let mut out = Vec::new();
    for stmt in &mut block.stmts {
        match stmt {
            HirStmt::Let { init: Some(e), .. } | HirStmt::Var { init: Some(e), .. } => {
                out.push(e.as_mut())
            }
            HirStmt::Expr(e) => out.push(e.as_mut()),
            HirStmt::Let { init: None, .. } | HirStmt::Var { init: None, .. } | HirStmt::Item => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        out.push(tail.as_mut());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_arithmetic_wraps_like_the_backend() {
        assert_eq!(int_binary(BinaryOp::Add, 2, 3).unwrap(), 5);
        assert_eq!(int_binary(BinaryOp::Mul, 6, 7).unwrap(), 42);
        assert_eq!(int_binary(BinaryOp::Div, 100, 3).unwrap(), 33);
        assert_eq!(int_binary(BinaryOp::Rem, 100, 3).unwrap(), 1);
        assert_eq!(int_binary(BinaryOp::Shl, 1, 5).unwrap(), 32);
        assert_eq!(int_binary(BinaryOp::BitOr, 16, 32).unwrap(), 48);
    }

    #[test]
    fn division_by_zero_is_an_error_not_a_panic() {
        assert!(int_binary(BinaryOp::Div, 1, 0).is_err());
        assert!(int_binary(BinaryOp::Rem, 1, 0).is_err());
        assert!(float_binary(BinaryOp::Div, 1.0, 0.0).is_err());
    }

    #[test]
    fn out_of_range_shifts_are_rejected_not_silently_wrapped() {
        // 后端按 64 位移位（x86 的 CL 只看低 6 位），所以移位量 >= 64 没有
        // 良定义的结果。必须报错，折成任何一个数都会与运行期不一致。
        assert!(int_binary(BinaryOp::Shl, 1, 200).is_err());
        assert!(int_binary(BinaryOp::Shr, -1, 64).is_err());
        // 64 位语义：1 << 63 = i64::MIN（符号位被顶出来），与后端一致。
        assert_eq!(int_binary(BinaryOp::Shl, 1, 63).unwrap(), i64::MIN as i128);
        assert_eq!(int_binary(BinaryOp::Shr, -1, 1).unwrap(), -1);
        assert_eq!(int_binary(BinaryOp::Shl, 1, 5).unwrap(), 32);
    }

    #[test]
    fn truncate_int_sign_extends() {
        // 0xFF 作为 i8 应为 -1，作为 u8 应为 255。
        assert_eq!(truncate_int(0xFF, 8, true), -1);
        assert_eq!(truncate_int(0xFF, 8, false), 255);
        assert_eq!(truncate_int(0x1FF, 8, true), -1);
        assert_eq!(truncate_int(5, 32, true), 5);
    }

    #[test]
    fn ordering_rejects_nan_instead_of_panicking() {
        let nan = HirLiteral::Float(f64::NAN);
        assert!(ord(BinaryOp::Lt, &nan, &HirLiteral::Float(1.0)).is_err());
    }

    #[test]
    fn comparisons_and_logic_fold() {
        assert!(matches!(
            binary(BinaryOp::Eq, &HirLiteral::Int(3), &HirLiteral::Int(3)).unwrap(),
            HirLiteral::Bool(true)
        ));
        assert!(matches!(
            binary(BinaryOp::Lt, &HirLiteral::Int(3), &HirLiteral::Int(4)).unwrap(),
            HirLiteral::Bool(true)
        ));
        assert!(matches!(
            binary(
                BinaryOp::And,
                &HirLiteral::Bool(false),
                &HirLiteral::Bool(true)
            )
            .unwrap(),
            HirLiteral::Bool(false)
        ));
        assert!(matches!(
            binary(
                BinaryOp::Or,
                &HirLiteral::Bool(true),
                &HirLiteral::Bool(false)
            )
            .unwrap(),
            HirLiteral::Bool(true)
        ));
    }

    #[test]
    fn unary_ops_fold() {
        assert!(matches!(
            unary(UnaryOp::Not, &HirLiteral::Int(0)).unwrap(),
            HirLiteral::Bool(true)
        ));
        assert_eq!(
            unary(UnaryOp::BitNot, &HirLiteral::Int(0)).unwrap(),
            HirLiteral::Int(-1)
        );
        assert_eq!(
            unary(UnaryOp::Neg, &HirLiteral::Int(42)).unwrap(),
            HirLiteral::Int(-42)
        );
    }

    #[test]
    fn float_arithmetic_folds() {
        let r = float_binary(BinaryOp::Mul, 1.5, 2.0).unwrap();
        assert_eq!(r, 3.0);
        // 位运算对浮点无意义，必须报错而不是静默取整。
        assert!(float_binary(BinaryOp::BitAnd, 1.5, 2.0).is_err());
    }

    #[test]
    fn cast_to_f32_rounds_once() {
        // f32 的 0.1 不能是 f64 的 0.1，否则赋给 f32 时会差一个 ulp。
        let got = cast(&HirLiteral::Float(0.1), &Ty::Float(32)).unwrap();
        match got {
            HirLiteral::Float(f) => assert_eq!(f, 0.1f32 as f64),
            other => panic!("expected float, got {other:?}"),
        }
    }
}
