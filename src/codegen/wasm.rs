//! WebAssembly 后端：从 SSA 生成 WAT（WebAssembly Text Format）。
//!
//! 目前生成最小化 WAT，支持 i32/i64/f32/f64 基本运算和导入/导出。

use crate::ssa::{SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaModule, SsaUnaryOp, SsaValue};
use std::collections::HashMap;

/// WASM 值类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmType {
    I32,
    I64,
    F32,
    F64,
}

impl WasmType {
    pub fn as_str(self) -> &'static str {
        match self {
            WasmType::I32 => "i32",
            WasmType::I64 => "i64",
            WasmType::F32 => "f32",
            WasmType::F64 => "f64",
        }
    }
}

/// WASM 指令。
#[derive(Debug, Clone)]
pub enum WasmInstr {
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    I32Const(i32),
    I64Const(i64),
    F32Const(f32),
    F64Const(f64),
    Add(WasmType),
    Sub(WasmType),
    Mul(WasmType),
    Div(WasmType),
    Rem(WasmType),
    Not,
    Return,
    Drop,
    /// 直接注入的 WAT 文本。
    Raw(String),
}

/// WASM 函数。
#[derive(Debug, Clone)]
pub struct WasmFunction {
    pub name: String,
    pub params: Vec<WasmType>,
    pub results: Vec<WasmType>,
    pub locals: Vec<WasmType>,
    pub body: Vec<WasmInstr>,
    pub exported: bool,
}

/// WASM 导入。
#[derive(Debug, Clone)]
pub struct WasmImport {
    pub module: String,
    pub name: String,
    pub params: Vec<WasmType>,
    pub results: Vec<WasmType>,
}

/// WASM 模块。
#[derive(Debug, Default)]
pub struct WasmModule {
    pub imports: Vec<WasmImport>,
    pub functions: Vec<WasmFunction>,
}

impl WasmModule {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 SSA 模块构建 WASM 模块。
    pub fn from_ssa(module: &SsaModule) -> Self {
        let mut wasm = WasmModule::new();
        for func in &module.functions {
            wasm.functions.push(WasmFunction::from_ssa(func));
        }
        wasm
    }

    /// 编码为 WAT 文本。
    pub fn to_wat(&self) -> String {
        let mut out = String::from("(module\n");

        // 导入
        for imp in &self.imports {
            out.push_str("  (import \"");
            out.push_str(&imp.module);
            out.push_str("\" \"");
            out.push_str(&imp.name);
            out.push_str("\" (func $");
            out.push_str(&imp.name);
            out.push_str(" (param");
            for p in &imp.params {
                out.push(' ');
                out.push_str(p.as_str());
            }
            out.push(')');
            if !imp.results.is_empty() {
                out.push_str(" (result");
                for r in &imp.results {
                    out.push(' ');
                    out.push_str(r.as_str());
                }
                out.push(')');
            }
            out.push_str("))\n");
        }

        // 函数
        for func in &self.functions {
            out.push_str("  (func $");
            out.push_str(&func.name);
            if !func.params.is_empty() {
                out.push_str(" (param");
                for p in &func.params {
                    out.push(' ');
                    out.push_str(p.as_str());
                }
                out.push(')');
            }
            if !func.results.is_empty() {
                out.push_str(" (result");
                for r in &func.results {
                    out.push(' ');
                    out.push_str(r.as_str());
                }
                out.push(')');
            }
            out.push('\n');
            for local in &func.locals {
                out.push_str("    (local ");
                out.push_str(local.as_str());
                out.push_str(")\n");
            }
            for instr in &func.body {
                out.push_str("    ");
                out.push_str(&instr.to_wat());
                out.push('\n');
            }
            out.push_str("  )\n");
            if func.exported {
                out.push_str(&format!("  (export \"{}\" (func ${}))\n", func.name, func.name));
            }
        }

        out.push_str(")\n");
        out
    }
}

impl WasmInstr {
    pub fn to_wat(&self) -> String {
        match self {
            WasmInstr::LocalGet(i) => format!("local.get {}", i),
            WasmInstr::LocalSet(i) => format!("local.set {}", i),
            WasmInstr::LocalTee(i) => format!("local.tee {}", i),
            WasmInstr::I32Const(v) => format!("i32.const {}", v),
            WasmInstr::I64Const(v) => format!("i64.const {}", v),
            WasmInstr::F32Const(v) => format!("f32.const {}", v),
            WasmInstr::F64Const(v) => format!("f64.const {}", v),
            WasmInstr::Add(t) => format!("{}.add", t.as_str()),
            WasmInstr::Sub(t) => format!("{}.sub", t.as_str()),
            WasmInstr::Mul(t) => format!("{}.mul", t.as_str()),
            WasmInstr::Div(t) => format!("{}.div", t.as_str()),
            WasmInstr::Rem(t) => format!("{}.rem", t.as_str()),
            WasmInstr::Not => "i32.eqz".to_string(),
            WasmInstr::Return => "return".to_string(),
            WasmInstr::Drop => "drop".to_string(),
            WasmInstr::Raw(s) => s.clone(),
        }
    }
}

impl WasmFunction {
    /// 从 SSA 函数构建 WASM 函数。
    pub fn from_ssa(func: &SsaFunction) -> Self {
        let params = vec![WasmType::I32; func.params.len()];
        let results = vec![WasmType::I32];

        let mut local_map: HashMap<u32, u32> = HashMap::new();
        let mut locals: Vec<WasmType> = Vec::new();
        let mut next_local = func.params.len() as u32;

        // 参数 SSA 值映射到局部变量 0..N
        for (i, p) in func.params.iter().enumerate() {
            local_map.insert(p.id(), i as u32);
        }

        let mut body = Vec::new();

        for block in &func.blocks {
            for instr in &block.instrs {
                match instr {
                    SsaInstr::Const { result, value } => {
                        let idx = ensure_local(result, &mut local_map, &mut locals, &mut next_local);
                        match value {
                            SsaConst::Int(i) => body.push(WasmInstr::I32Const(*i as i32)),
                            SsaConst::Bool(b) => body.push(WasmInstr::I32Const(if *b { 1 } else { 0 })),
                            SsaConst::Float(f) => {
                                let bits = *f;
                                let val = f64::from_bits(bits);
                                body.push(WasmInstr::F64Const(val));
                            }
                            SsaConst::Unit => body.push(WasmInstr::I32Const(0)),
                        }
                        body.push(WasmInstr::LocalSet(idx));
                    }
                    SsaInstr::Unary { result, op, operand } => {
                        let idx = ensure_local(result, &mut local_map, &mut locals, &mut next_local);
                        emit_value_ref(*operand, &local_map, &mut body);
                        match op {
                            SsaUnaryOp::Not => body.push(WasmInstr::Not),
                            SsaUnaryOp::Neg => {
                                body.push(WasmInstr::I32Const(0));
                                body.push(WasmInstr::Sub(WasmType::I32));
                            }
                            SsaUnaryOp::BitNot => {
                                body.push(WasmInstr::I32Const(-1));
                                body.push(WasmInstr::Raw("i32.xor".to_string()));
                            }
                        }
                        body.push(WasmInstr::LocalSet(idx));
                    }
                    SsaInstr::Binary { result, op, lhs, rhs } => {
                        let idx = ensure_local(result, &mut local_map, &mut locals, &mut next_local);
                        emit_value_ref(*lhs, &local_map, &mut body);
                        emit_value_ref(*rhs, &local_map, &mut body);
                        match op {
                            SsaBinaryOp::Add => body.push(WasmInstr::Add(WasmType::I32)),
                            SsaBinaryOp::Sub => body.push(WasmInstr::Sub(WasmType::I32)),
                            SsaBinaryOp::Mul => body.push(WasmInstr::Mul(WasmType::I32)),
                            SsaBinaryOp::Div => body.push(WasmInstr::Div(WasmType::I32)),
                            SsaBinaryOp::Rem => body.push(WasmInstr::Rem(WasmType::I32)),
                            _ => body.push(WasmInstr::Add(WasmType::I32)),
                        }
                        body.push(WasmInstr::LocalSet(idx));
                    }
                    SsaInstr::Var { result, name: _ } => {
                        let _ = ensure_local(result, &mut local_map, &mut locals, &mut next_local);
                    }
                    SsaInstr::Cast { result, operand, to: _ } => {
                        let idx = ensure_local(result, &mut local_map, &mut locals, &mut next_local);
                        emit_value_ref(*operand, &local_map, &mut body);
                        body.push(WasmInstr::LocalSet(idx));
                    }
                }
            }
            // 终结器
            match &block.term {
                crate::ssa::SsaTerminator::Return(v) => {
                    if let Some(val) = v {
                        emit_value_ref(*val, &local_map, &mut body);
                    }
                    body.push(WasmInstr::Return);
                }
                _ => {}
            }
        }

        WasmFunction {
            name: func.name.clone(),
            params,
            results,
            locals,
            body,
            exported: func.name == "main",
        }
    }
}

fn ensure_local(
    value: &SsaValue,
    map: &mut HashMap<u32, u32>,
    locals: &mut Vec<WasmType>,
    next: &mut u32,
) -> u32 {
    *map.entry(value.id()).or_insert_with(|| {
        let i = *next;
        *next += 1;
        locals.push(WasmType::I32);
        i
    })
}

fn emit_value_ref(value: SsaValue, locals: &HashMap<u32, u32>, body: &mut Vec<WasmInstr>) {
    if let Some(&idx) = locals.get(&value.id()) {
        body.push(WasmInstr::LocalGet(idx));
    } else {
        body.push(WasmInstr::I32Const(0));
    }
}
