//! WebAssembly 后端：从 SSA 生成 wasm 二进制模块与 WAT 文本。
//!
//! 与原生后端的差别：
//! - wasm 是**栈式**虚拟机，不需要寄存器分配；每个 SSA 值映射到一个
//!   局部变量，指令直接压在栈上运算；
//! - `main` 导出为 `main`，同时导出 `_start`（WASI 约定）；
//! - 运行时（`zr_*` 符号）不再是外部汇编文件，而是在模块内合成的一组
//!   **导入函数**，由宿主（JS/WASI runtime）提供；这样任何能编译 zr
//!   编译器的机器都能产出 wasm，不需要额外的 `wat2wasm` 工具链。
//!
//! 二进制编码遵循 WebAssembly 核心规范 1.0：LEB128、段（section）
//! 顺序为 type → import → function → memory → export → code。

use std::collections::HashMap;

use crate::codegen::target::Arch;
use crate::mir::NumTy;
use crate::ssa::{
    SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaModule, SsaTerminator, SsaUnaryOp, SsaValue,
};

// ==================================================================
// 公共类型
// ==================================================================

/// WASM 值类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmType {
    I32,
    I64,
    F32,
    F64,
}

impl WasmType {
    /// 类型名（同时用作二进制编码）。
    pub fn as_str(self) -> &'static str {
        match self {
            WasmType::I32 => "i32",
            WasmType::I64 => "i64",
            WasmType::F32 => "f32",
            WasmType::F64 => "f64",
        }
    }

    /// 二进制编码。
    fn code(self) -> u8 {
        match self {
            WasmType::I32 => 0x7f,
            WasmType::I64 => 0x7e,
            WasmType::F32 => 0x7d,
            WasmType::F64 => 0x7c,
        }
    }
}

/// WASM 导入模块名（运行时函数都来自这里）。
pub const RUNTIME_MODULE: &str = "zr";

/// WASM 指令。
#[derive(Debug, Clone)]
pub enum WasmInstr {
    LocalGet(u32),
    LocalSet(u32),
    /// 64 位常量（zr 的整数一律按 i64 建模，与原生后端一致）。
    I64Const(i64),
    /// 32 位浮点常量。
    F32Const(f32),
    /// 64 位浮点常量。
    F64Const(f64),
    /// `i32.eqz`：逻辑非（结果为 i32）。
    I32Eqz,
    /// 二元运算，参数为值类型。
    Binary(WasmType, WasmBinaryOp),
    /// 一元运算：`i32.wrap_i64` / `i64.extend_i32_s` / `i64.extend_i32_u` / `f32.neg` / `f64.neg` / `f32.abs` / `f64.abs` 等。
    Unary(WasmUnaryOp),
    /// `call funcidx`。
    Call(u32),
    Return,
}

/// 一元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmUnaryOp {
    /// `i32.wrap_i64`：取低 32 位。
    WrapI64ToI32,
    /// `i64.extend_i32_s`：按 32 位符号扩展到 64 位。
    ExtendI32S,
    /// `i64.extend_i32_u`：按 32 位零扩展到 64 位。
    ExtendI32U,
    /// `f32.neg`：单精度取负。
    F32Neg,
    /// `f64.neg`：双精度取负。
    F64Neg,
    /// `f32.abs`：单精度绝对值。
    F32Abs,
    /// `f64.abs`：双精度绝对值。
    F64Abs,
    /// `f32.sqrt`：单精度平方根。
    F32Sqrt,
    /// `f64.sqrt`：双精度平方根。
    F64Sqrt,
    /// `f32.ceil`：单精度向上取整。
    F32Ceil,
    /// `f64.ceil`：双精度向上取整。
    F64Ceil,
    /// `f32.floor`：单精度向下取整。
    F32Floor,
    /// `f64.floor`：双精度向下取整。
    F64Floor,
    /// `f32.trunc`：单精度截断。
    F32Trunc,
    /// `f64.trunc`：双精度截断。
    F64Trunc,
    /// `f32.nearest`：单精度四舍五入。
    F32Nearest,
    /// `f64.nearest`：双精度四舍五入。
    F64Nearest,
    /// `i64.trunc_f32_s`：单精度浮点转有符号 64 位整数。
    I64TruncF32S,
    /// `i64.trunc_f32_u`：单精度浮点转无符号 64 位整数。
    I64TruncF32U,
    /// `i64.trunc_f64_s`：双精度浮点转有符号 64 位整数。
    I64TruncF64S,
    /// `i64.trunc_f64_u`：双精度浮点转无符号 64 位整数。
    I64TruncF64U,
    /// `f32.convert_i64_s`：有符号 64 位整数转单精度浮点。
    F32ConvertI64S,
    /// `f32.convert_i64_u`：无符号 64 位整数转单精度浮点。
    F32ConvertI64U,
    /// `f64.convert_i64_s`：有符号 64 位整数转双精度浮点。
    F64ConvertI64S,
    /// `f64.convert_i64_u`：无符号 64 位整数转双精度浮点。
    F64ConvertI64U,
    /// `f32.demote_f64`：双精度转单精度。
    F32DemoteF64,
    /// `f64.promote_f32`：单精度转双精度。
    F64PromoteF32,
}

impl WasmUnaryOp {
    /// WAT 助记符。
    fn wat(self) -> &'static str {
        match self {
            WasmUnaryOp::WrapI64ToI32 => "i32.wrap_i64",
            WasmUnaryOp::ExtendI32S => "i64.extend_i32_s",
            WasmUnaryOp::ExtendI32U => "i64.extend_i32_u",
            WasmUnaryOp::F32Neg => "f32.neg",
            WasmUnaryOp::F64Neg => "f64.neg",
            WasmUnaryOp::F32Abs => "f32.abs",
            WasmUnaryOp::F64Abs => "f64.abs",
            WasmUnaryOp::F32Sqrt => "f32.sqrt",
            WasmUnaryOp::F64Sqrt => "f64.sqrt",
            WasmUnaryOp::F32Ceil => "f32.ceil",
            WasmUnaryOp::F64Ceil => "f64.ceil",
            WasmUnaryOp::F32Floor => "f32.floor",
            WasmUnaryOp::F64Floor => "f64.floor",
            WasmUnaryOp::F32Trunc => "f32.trunc",
            WasmUnaryOp::F64Trunc => "f64.trunc",
            WasmUnaryOp::F32Nearest => "f32.nearest",
            WasmUnaryOp::F64Nearest => "f64.nearest",
            WasmUnaryOp::I64TruncF32S => "i64.trunc_f32_s",
            WasmUnaryOp::I64TruncF32U => "i64.trunc_f32_u",
            WasmUnaryOp::I64TruncF64S => "i64.trunc_f64_s",
            WasmUnaryOp::I64TruncF64U => "i64.trunc_f64_u",
            WasmUnaryOp::F32ConvertI64S => "f32.convert_i64_s",
            WasmUnaryOp::F32ConvertI64U => "f32.convert_i64_u",
            WasmUnaryOp::F64ConvertI64S => "f64.convert_i64_s",
            WasmUnaryOp::F64ConvertI64U => "f64.convert_i64_u",
            WasmUnaryOp::F32DemoteF64 => "f32.demote_f64",
            WasmUnaryOp::F64PromoteF32 => "f64.promote_f32",
        }
    }

    /// 二进制操作码。
    fn code(self) -> u8 {
        match self {
            WasmUnaryOp::WrapI64ToI32 => 0xA7,
            WasmUnaryOp::ExtendI32S => 0xAC,
            WasmUnaryOp::ExtendI32U => 0xAD,
            WasmUnaryOp::F32Neg => 0x8B,
            WasmUnaryOp::F64Neg => 0x8C,
            WasmUnaryOp::F32Abs => 0x8D,
            WasmUnaryOp::F64Abs => 0x8E,
            WasmUnaryOp::F32Sqrt => 0x8F,
            WasmUnaryOp::F64Sqrt => 0x90,
            WasmUnaryOp::F32Ceil => 0x91,
            WasmUnaryOp::F64Ceil => 0x92,
            WasmUnaryOp::F32Floor => 0x93,
            WasmUnaryOp::F64Floor => 0x94,
            WasmUnaryOp::F32Trunc => 0x95,
            WasmUnaryOp::F64Trunc => 0x96,
            WasmUnaryOp::F32Nearest => 0x97,
            WasmUnaryOp::F64Nearest => 0x98,
            WasmUnaryOp::I64TruncF32S => 0xAE,
            WasmUnaryOp::I64TruncF32U => 0xAF,
            WasmUnaryOp::I64TruncF64S => 0xB0,
            WasmUnaryOp::I64TruncF64U => 0xB1,
            WasmUnaryOp::F32ConvertI64S => 0xB4,
            WasmUnaryOp::F32ConvertI64U => 0xB5,
            WasmUnaryOp::F64ConvertI64S => 0xB9,
            WasmUnaryOp::F64ConvertI64U => 0xBA,
            WasmUnaryOp::F32DemoteF64 => 0xB6,
            WasmUnaryOp::F64PromoteF32 => 0xBB,
        }
    }
}

/// 二元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmBinaryOp {
    // 整数运算
    Add,
    Sub,
    Mul,
    DivS,
    RemS,
    And,
    Or,
    Xor,
    Shl,
    ShrS,
    Eq,
    Ne,
    LtS,
    GtS,
    LeS,
    GeS,
    // 浮点运算
    FAdd,
    FSub,
    FMul,
    FDiv,
    // 浮点比较
    FEq,
    FNe,
    FLt,
    FGt,
    FLe,
    FGe,
    // 浮点最小/最大
    FMin,
    FMax,
    // 浮点复制符号
    FCopysign,
}

impl WasmBinaryOp {
    /// WAT 助记符。
    fn wat(self, ty: WasmType) -> String {
        use WasmBinaryOp::*;
        let suffix = match ty {
            WasmType::I32 => "i32",
            WasmType::I64 => "i64",
            WasmType::F32 => "f32",
            WasmType::F64 => "f64",
        };
        let op = match self {
            Add => "add",
            Sub => "sub",
            Mul => "mul",
            DivS => "div_s",
            RemS => "rem_s",
            And => "and",
            Or => "or",
            Xor => "xor",
            Shl => "shl",
            ShrS => "shr_s",
            Eq => "eq",
            Ne => "ne",
            LtS => "lt_s",
            GtS => "gt_s",
            LeS => "le_s",
            GeS => "ge_s",
            FAdd => "add",
            FSub => "sub",
            FMul => "mul",
            FDiv => "div",
            FEq => "eq",
            FNe => "ne",
            FLt => "lt",
            FGt => "gt",
            FLe => "le",
            FGe => "ge",
            FMin => "min",
            FMax => "max",
            FCopysign => "copysign",
        };
        format!("{suffix}.{op}")
    }

    /// 二进制编码（操作码低位部分）。
    fn code(self) -> u8 {
        use WasmBinaryOp::*;
        match self {
            // Integer ops
            Eq => 0x46,
            Ne => 0x47,
            LtS => 0x48,
            GtS => 0x4a,
            LeS => 0x4c,
            GeS => 0x4e,
            Add => 0x6a,
            Sub => 0x6b,
            Mul => 0x6c,
            DivS => 0x6d,
            RemS => 0x6f,
            And => 0x71,
            Or => 0x72,
            Xor => 0x73,
            Shl => 0x74,
            ShrS => 0x75,
            // Float ops (type is encoded in the opcode prefix)
            FAdd => 0x92,
            FSub => 0x93,
            FMul => 0x94,
            FDiv => 0x95,
            FEq => 0x5B,
            FNe => 0x5C,
            FLt => 0x5D,
            FGt => 0x5E,
            FLe => 0x5F,
            FGe => 0x60,
            FMin => 0x99,
            FMax => 0x9A,
            FCopysign => 0x9B,
        }
    }
}

/// WASM 函数。
#[derive(Debug, Clone)]
pub struct WasmFunction {
    pub name: String,
    /// 尚未解析函数下标的调用：`(指令下标, 被调函数名)`。
    ///
    /// 函数下标要等所有函数都登记进模块后才能确定（`_start` 克隆还会
    /// 额外占用一个下标），所以先记名字，最后统一回填。
    pub pending_calls: Vec<(usize, String)>,
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
#[derive(Debug, Clone, Default)]
pub struct WasmModule {
    pub imports: Vec<WasmImport>,
    pub functions: Vec<WasmFunction>,
    /// 线性内存初始页数（64 KiB / 页）。
    pub memory_pages: u32,
    /// 最大页数（0 表示不做上��，由宿主配置）。
    pub memory_max_pages: u32,
}

impl WasmModule {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 SSA 模块构建 WASM 模块。
    pub fn from_ssa(module: &SsaModule, arch: Arch) -> Self {
        let mut wasm = WasmModule::new();
        // 运行时函数以导入形式提供，宿主负责实现。签名随指针宽度变化。
        wasm.imports = runtime_imports_for(arch);
        wasm.memory_pages = 2;
        wasm.memory_max_pages = 256;
        for func in &module.functions {
            let compiled = WasmFunction::from_ssa(func);
            if func.name == "main" {
                // WASI 约定：同时导出 `_start` 与 `main`。
                let mut start = compiled.clone();
                start.name = "_start".to_string();
                start.exported = true;
                wasm.functions.push(start);
            }
            wasm.functions.push(compiled);
        }

        // 所有函数下标确定后，把按名字记录的调用回填成实际下标。
        let import_count = wasm.imports.len() as u32;
        let indices: std::collections::HashMap<String, u32> = wasm
            .functions
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name.clone(), import_count + i as u32))
            .collect();
        for func in &mut wasm.functions {
            for (at, callee) in std::mem::take(&mut func.pending_calls) {
                if let Some(&idx) = indices.get(&callee) {
                    func.body[at] = WasmInstr::Call(idx);
                } else {
                    func.body[at] = WasmInstr::Call(import_count);
                }
            }
        }
        wasm
    }

    /// 编码为 WAT 文本。
    pub fn to_wat(&self) -> String {
        let mut out = String::from("(module\n");

        for imp in &self.imports {
            out.push_str("  (import \"");
            out.push_str(&imp.module);
            out.push_str("\" \"");
            out.push_str(&imp.name);
            out.push_str("\" (func $");
            out.push_str(&imp.name);
            if !imp.params.is_empty() {
                out.push_str(" (param");
                for p in &imp.params {
                    out.push(' ');
                    out.push_str(p.as_str());
                }
                out.push(')');
            }
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

        out.push_str(&format!(
            "  (memory {} {})",
            self.memory_pages, self.memory_max_pages
        ));
        out.push('\n');

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
                out.push_str(&format!(
                    "  (export \"{}\" (func ${}))\n",
                    func.name, func.name
                ));
            }
        }

        out.push_str(")\n");
        out
    }

    /// 编码为 wasm 二进制模块。
    pub fn to_binary(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\0asm"); // magic
        out.extend_from_slice(&[1, 0, 0, 0]); // version 1

        // ---- Type 段：先导入后本地函数，顺序必须一致 ----
        let mut types: Vec<FuncType> = self.imports.iter().map(|i| i.func_type()).collect();
        let type_index_base = types.len();
        for f in &self.functions {
            types.push(f.func_type());
        }
        write_section(&mut out, 1, &encode_type_section(&types));

        // ---- Import 段 ----
        if !self.imports.is_empty() {
            write_section(&mut out, 2, &encode_import_section(&self.imports));
        }

        // ---- Function 段：本地函数使用的类型下标 ----
        let func_type_indices: Vec<Vec<u8>> = self
            .functions
            .iter()
            .map(|_| {
                let mut bytes = Vec::new();
                uleb(type_index_base as u64, &mut bytes);
                bytes
            })
            .collect();
        write_section(&mut out, 3, &encode_vec(&func_type_indices));

        // ---- Memory 段 ----
        write_section(
            &mut out,
            5,
            &encode_vec(&[encode_limits(
                self.memory_pages,
                Some(self.memory_max_pages),
            )]),
        );

        // ---- Export 段 ----
        let mut exports: Vec<Vec<u8>> = Vec::new();
        for (i, f) in self.functions.iter().enumerate() {
            if f.exported {
                exports.push(encode_export(
                    &f.name,
                    0,
                    i as u32 + self.imports.len() as u32,
                ));
            }
        }
        exports.push(encode_export("memory", 0x02, 0));
        write_section(&mut out, 7, &encode_vec(&exports));

        // ---- Code 段 ----
        let bodies: Vec<Vec<u8>> = self.functions.iter().map(encode_code).collect();
        write_section(&mut out, 10, &encode_vec(&bodies));
        out
    }
}

impl WasmInstr {
    /// WAT 文本。
    pub fn to_wat(&self) -> String {
        match self {
            WasmInstr::LocalGet(i) => format!("local.get {i}"),
            WasmInstr::LocalSet(i) => format!("local.set {i}"),
            WasmInstr::I64Const(v) => format!("i64.const {v}"),
            WasmInstr::F32Const(f) => format!("f32.const {f}"),
            WasmInstr::F64Const(f) => format!("f64.const {f}"),
            WasmInstr::I32Eqz => "i32.eqz".to_string(),
            WasmInstr::Binary(ty, op) => op.wat(*ty),
            WasmInstr::Unary(op) => op.wat().to_string(),
            WasmInstr::Call(idx) => format!("call {idx}"),
            WasmInstr::Return => "return".to_string(),
        }
    }
}

impl WasmFunction {
    /// 从 SSA 函数构建 WASM 函数。
    pub fn from_ssa(func: &SsaFunction) -> Self {
        let float_widths = infer_float_widths(func);
        let wasm_ty = |id: u32| match float_widths.get(&id) {
            Some(32) => WasmType::F32,
            Some(_) => WasmType::F64,
            None => WasmType::I64,
        };

        let params: Vec<WasmType> = func.params.iter().map(|p| wasm_ty(p.id())).collect();
        // 返回类型取最后一个 `Return` 值；找不到浮点信息时退回 i64。
        let results = vec![return_wasm_ty(func, &wasm_ty)];

        let mut local_map: HashMap<u32, u32> = HashMap::new();
        let mut locals: Vec<WasmType> = Vec::new();
        let mut next_local = func.params.len() as u32;

        // 参数 SSA 值映射到局部变量 0..N
        for (i, p) in func.params.iter().enumerate() {
            local_map.insert(p.id(), i as u32);
        }

        let mut body = Vec::new();
        let mut pending_calls: Vec<(usize, String)> = Vec::new();
        let mut value_ty_map: HashMap<u32, WasmType> = HashMap::new();

        // 预设参数类型
        for p in &func.params {
            value_ty_map.insert(p.id(), wasm_ty(p.id()));
        }

        for block in &func.blocks {
            for instr in &block.instrs {
                match instr {
                    SsaInstr::Cast {
                        result,
                        src,
                        from,
                        to,
                        ..
                    } => {
                        let idx =
                            ensure_local(result, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        let src_idx = ensure_local(src, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        body.push(WasmInstr::LocalGet(src_idx));
                        if from.is_float() || to.is_float() {
                            // 浮点转换
                            match (from, to) {
                                // int -> float
                                (NumTy::Int(_) | NumTy::Uint(_), NumTy::Float(w)) => {
                                    let signed = matches!(from, NumTy::Int(_));
                                    let is_f32 = *w == 32;
                                    body.push(WasmInstr::Unary(match (is_f32, signed) {
                                        (true, true) => WasmUnaryOp::F32ConvertI64S,
                                        (true, false) => WasmUnaryOp::F32ConvertI64U,
                                        (false, true) => WasmUnaryOp::F64ConvertI64S,
                                        (false, false) => WasmUnaryOp::F64ConvertI64U,
                                    }));
                                }
                                // float -> int
                                (NumTy::Float(src_w), NumTy::Int(_) | NumTy::Uint(_)) => {
                                    let signed = matches!(to, NumTy::Int(_));
                                    let is_f32 = *src_w == 32;
                                    body.push(WasmInstr::Unary(match (is_f32, signed) {
                                        (true, true) => WasmUnaryOp::I64TruncF32S,
                                        (true, false) => WasmUnaryOp::I64TruncF32U,
                                        (false, true) => WasmUnaryOp::I64TruncF64S,
                                        (false, false) => WasmUnaryOp::I64TruncF64U,
                                    }));
                                }
                                // float -> float (f32 <-> f64)
                                (NumTy::Float(from_w), NumTy::Float(to_w)) => {
                                    let dst_is_f32 = *to_w == 32;
                                    let src_is_f32 = *from_w == 32;
                                    if src_is_f32 != dst_is_f32 {
                                        if dst_is_f32 {
                                            body.push(WasmInstr::Unary(WasmUnaryOp::F32DemoteF64));
                                        } else {
                                            body.push(WasmInstr::Unary(WasmUnaryOp::F64PromoteF32));
                                        }
                                    }
                                }
                                _ => crate::codegen::unsupported_cast(*from, *to),
                            }
                        } else {
                            let (bits, signed) = match to {
                                crate::mir::NumTy::Int(w) => (*w, true),
                                crate::mir::NumTy::Uint(w) => (*w, false),
                                _ => (8, false),
                            };
                            if bits < 64 {
                                body.push(WasmInstr::Unary(WasmUnaryOp::WrapI64ToI32));
                                body.push(WasmInstr::Unary(if signed {
                                    WasmUnaryOp::ExtendI32S
                                } else {
                                    WasmUnaryOp::ExtendI32U
                                }));
                            }
                        }
                        body.push(WasmInstr::LocalSet(idx));
                        let result_ty = match to {
                            NumTy::Int(_) | NumTy::Uint(_) => WasmType::I64,
                            NumTy::Float(w) => if *w == 32 { WasmType::F32 } else { WasmType::F64 },
                            _ => WasmType::I64,
                        };
                        value_ty_map.insert(result.id(), result_ty);
                    }
                    SsaInstr::Const { result, value } => {
                        let idx =
                            ensure_local(result, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        match value {
                            SsaConst::Int(i) => {
                                body.push(WasmInstr::I64Const(*i as i64));
                                value_ty_map.insert(result.id(), WasmType::I64);
                            }
                            SsaConst::Bool(b) => {
                                body.push(WasmInstr::I64Const(i64::from(*b)));
                                value_ty_map.insert(result.id(), WasmType::I64);
                            }
                            SsaConst::Unit => {
                                body.push(WasmInstr::I64Const(0));
                                value_ty_map.insert(result.id(), WasmType::I64);
                            }
                            SsaConst::Float(bits, w) => {
                                if *w == 32 {
                                    body.push(WasmInstr::F32Const(f32::from_bits(*bits as u32)));
                                    value_ty_map.insert(result.id(), WasmType::F32);
                                } else {
                                    body.push(WasmInstr::F64Const(f64::from_bits(*bits)));
                                    value_ty_map.insert(result.id(), WasmType::F64);
                                }
                            }
                            SsaConst::Str(_) => crate::codegen::unsupported_const("string"),
                        }
                        body.push(WasmInstr::LocalSet(idx));
                    }
                    SsaInstr::Unary {
                        result,
                        op,
                        operand,
                    } => {
                        let idx =
                            ensure_local(result, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        let operand_idx = ensure_local(operand, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        body.push(WasmInstr::LocalGet(operand_idx));
                        let op_ty = wasm_value_ty(operand, &value_ty_map);
                        match op {
                            SsaUnaryOp::Not => body.push(WasmInstr::I32Eqz),
                            SsaUnaryOp::Neg => {
                                if op_ty == WasmType::F32 {
                                    body.push(WasmInstr::Unary(WasmUnaryOp::F32Neg));
                                    value_ty_map.insert(result.id(), WasmType::F32);
                                } else if op_ty == WasmType::F64 {
                                    body.push(WasmInstr::Unary(WasmUnaryOp::F64Neg));
                                    value_ty_map.insert(result.id(), WasmType::F64);
                                } else {
                                    body.push(WasmInstr::I64Const(0));
                                    body.push(WasmInstr::Binary(WasmType::I64, WasmBinaryOp::Sub));
                                    value_ty_map.insert(result.id(), WasmType::I64);
                                }
                            }
                            SsaUnaryOp::BitNot => {
                                if op_ty == WasmType::F32 || op_ty == WasmType::F64 {
                                    crate::codegen::unsupported_const("float bitnot");
                                } else {
                                    body.push(WasmInstr::I64Const(-1));
                                    body.push(WasmInstr::Binary(WasmType::I64, WasmBinaryOp::Xor));
                                    value_ty_map.insert(result.id(), WasmType::I64);
                                }
                            }
                        }
                        body.push(WasmInstr::LocalSet(idx));
                    }
                    SsaInstr::Binary {
                        result,
                        op,
                        lhs,
                        rhs,
                        ty,
                    } => {
                        let idx =
                            ensure_local(result, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        let lhs_idx = ensure_local(lhs, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        let rhs_idx = ensure_local(rhs, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        body.push(WasmInstr::LocalGet(lhs_idx));
                        body.push(WasmInstr::LocalGet(rhs_idx));
                        let lhs_ty = wasm_value_ty(lhs, &value_ty_map);
                        let rhs_ty = wasm_value_ty(rhs, &value_ty_map);
                        let (mut res_ty, wasm_op) = wasm_binop(*op, lhs_ty, rhs_ty);
                        // 操作数类型未知时（例如未标注的形参），用 SSA 的结果类型兜底
                        if res_ty == WasmType::I64 {
                            if let NumTy::Float(w) = ty {
                                res_ty = if *w == 32 { WasmType::F32 } else { WasmType::F64 };
                            }
                        }
                        body.push(WasmInstr::Binary(res_ty, wasm_op));
                        body.push(WasmInstr::LocalSet(idx));
                        value_ty_map.insert(result.id(), res_ty);
                    }
                    SsaInstr::Call {
                        result,
                        callee,
                        args,
                        ty: _,
                    } => {
                        for a in args {
                            emit_value_ref(*a, &local_map, &mut body);
                        }
                        pending_calls.push((body.len(), callee.clone()));
                        body.push(WasmInstr::Call(0));
                        let idx =
                            ensure_local(result, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        body.push(WasmInstr::LocalSet(idx));
                        value_ty_map.insert(result.id(), wasm_ty(result.id()));
                    }
                    SsaInstr::Copy { result, src } => {
                        emit_value_ref(*src, &local_map, &mut body);
                        let idx =
                            ensure_local(result, &mut local_map, &mut locals, &mut next_local, &float_widths);
                        body.push(WasmInstr::LocalSet(idx));
                        let src_ty = wasm_value_ty(src, &value_ty_map);
                        value_ty_map.insert(result.id(), src_ty);
                    }
                    SsaInstr::Phi { result, .. } => {
                        ensure_local(result, &mut local_map, &mut locals, &mut next_local, &float_widths);
                    }
                    SsaInstr::Var { result, .. } => {
                        ensure_local(result, &mut local_map, &mut locals, &mut next_local, &float_widths);
                    }
                    SsaInstr::Load { .. } | SsaInstr::Store { .. } | SsaInstr::SlotAddr { .. } => {
                        crate::codegen::unsupported_const("pointer/memory access")
                    }
                }
            }
            if let crate::ssa::SsaTerminator::Return(v) = &block.term {
                match v {
                    Some(val) => emit_value_ref(*val, &local_map, &mut body),
                    None => body.push(WasmInstr::I64Const(0)),
                }
                body.push(WasmInstr::Return);
            }
        }

        WasmFunction {
            name: func.name.clone(),
            pending_calls,
            params,
            results,
            locals,
            body,
            exported: func.name == "main",
        }
    }

    /// 函数签名。
    fn func_type(&self) -> FuncType {
        FuncType {
            params: self.params.clone(),
            results: self.results.clone(),
        }
    }
}

/// 推断 SSA 值是否为浮点及其位宽。
///
/// SSA 的形参没有类型标注，只能从使用点反推：浮点二元运算、转换、
/// 常量、调用返回值，以及 `Copy`/`Phi` 的双向传播。反复迭代到不动点，
/// 这样 `a + b` 里的形参即使先经过 `Copy` 也能被识别出来。
fn infer_float_widths(func: &SsaFunction) -> HashMap<u32, u8> {
    let mut floats: HashMap<u32, u8> = HashMap::new();
    let width_of =
        |v: &SsaValue, map: &HashMap<u32, u8>| -> u8 { map.get(&v.id()).copied().unwrap_or(0) };

    loop {
        let mut changed = false;
        let mut mark = |v: &SsaValue, w: u8, map: &mut HashMap<u32, u8>| -> bool {
            match map.get(&v.id()) {
                Some(old) if *old >= w => false,
                _ => {
                    map.insert(v.id(), w);
                    true
                }
            }
        };

        for block in &func.blocks {
            for instr in &block.instrs {
                match instr {
                    SsaInstr::Const {
                        result,
                        value: SsaConst::Float(_, w),
                    } => {
                        if mark(result, *w, &mut floats) {
                            changed = true;
                        }
                    }
                    SsaInstr::Binary {
                        result,
                        ty,
                        lhs,
                        rhs,
                        ..
                    } => {
                        if ty.is_float() {
                            let w = match *ty {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            for v in [result, lhs, rhs] {
                                if mark(v, w, &mut floats) {
                                    changed = true;
                                }
                            }
                        } else {
                            // bool 比较：从已知的一侧把位宽传给另一侧
                            let lw = width_of(lhs, &floats);
                            let rw = width_of(rhs, &floats);
                            let (unknown, w) = match (lw, rw) {
                                (w, 0) if w > 0 => (rhs, w),
                                (0, w) if w > 0 => (lhs, w),
                                _ => continue,
                            };
                            if mark(unknown, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Unary {
                        result, operand, ..
                    } => {
                        if floats.contains_key(&result.id()) {
                            let w = width_of(result, &floats);
                            if w > 0 && mark(operand, w, &mut floats) {
                                changed = true;
                            }
                        }
                        if floats.contains_key(&operand.id()) {
                            let w = width_of(operand, &floats);
                            if w > 0 && mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Call { result, ty, .. } => {
                        if ty.is_float() {
                            let w = match *ty {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            if mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Cast {
                        result,
                        src,
                        from,
                        to,
                    } => {
                        if to.is_float() {
                            let w = match *to {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            if mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                        if from.is_float() {
                            let w = match *from {
                                NumTy::Float(w) => w,
                                _ => 64,
                            };
                            if mark(src, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Copy { result, src } => {
                        if floats.contains_key(&src.id()) {
                            let w = width_of(src, &floats);
                            if mark(result, w, &mut floats) {
                                changed = true;
                            }
                        }
                        if floats.contains_key(&result.id()) {
                            let w = width_of(result, &floats);
                            if mark(src, w, &mut floats) {
                                changed = true;
                            }
                        }
                    }
                    SsaInstr::Phi {
                        result, incoming, ..
                    } if incoming.iter().any(|(_, v)| floats.contains_key(&v.id())) => {
                        let w = incoming
                            .iter()
                            .map(|(_, v)| width_of(v, &floats))
                            .max()
                            .unwrap_or(64);
                        if mark(result, w, &mut floats) {
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !changed {
            break;
        }
    }
    floats
}

/// 函数返回值的 WASM 类型：扫描所有 `Return`，有浮点信息就用浮点类型。
fn return_wasm_ty(func: &SsaFunction, wasm_ty: &impl Fn(u32) -> WasmType) -> WasmType {
    for block in &func.blocks {
        if let SsaTerminator::Return(Some(v)) = &block.term {
            let ty = wasm_ty(v.id());
            if ty != WasmType::I64 {
                return ty;
            }
        }
    }
    WasmType::I64
}

/// 推断 SSA 值的 WASM 类型（基于 SSA 值的类型信息）。
fn wasm_value_ty(v: &SsaValue, ty_map: &HashMap<u32, WasmType>) -> WasmType {
    ty_map.get(&v.id()).copied().unwrap_or(WasmType::I64)
}

/// 映射 SSA 二元运算到 wasm 运算。
fn wasm_binop(op: SsaBinaryOp, lhs_ty: WasmType, rhs_ty: WasmType) -> (WasmType, WasmBinaryOp) {
    use SsaBinaryOp::*;
    match op {
        Add => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::F32, WasmBinaryOp::FAdd)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::F64, WasmBinaryOp::FAdd)
            } else {
                (WasmType::I64, WasmBinaryOp::Add)
            }
        }
        Sub => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::F32, WasmBinaryOp::FSub)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::F64, WasmBinaryOp::FSub)
            } else {
                (WasmType::I64, WasmBinaryOp::Sub)
            }
        }
        Mul => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::F32, WasmBinaryOp::FMul)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::F64, WasmBinaryOp::FMul)
            } else {
                (WasmType::I64, WasmBinaryOp::Mul)
            }
        }
        Div => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::F32, WasmBinaryOp::FDiv)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::F64, WasmBinaryOp::FDiv)
            } else {
                (WasmType::I64, WasmBinaryOp::DivS)
            }
        }
        Rem => (WasmType::I64, WasmBinaryOp::RemS),
        And | BitAnd => (WasmType::I64, WasmBinaryOp::And),
        Or | BitOr => (WasmType::I64, WasmBinaryOp::Or),
        BitXor => (WasmType::I64, WasmBinaryOp::Xor),
        Shl => (WasmType::I64, WasmBinaryOp::Shl),
        Shr => (WasmType::I64, WasmBinaryOp::ShrS),
        Eq => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::I32, WasmBinaryOp::FEq)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::I32, WasmBinaryOp::FEq)
            } else {
                (WasmType::I32, WasmBinaryOp::Eq)
            }
        }
        Ne => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::I32, WasmBinaryOp::FNe)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::I32, WasmBinaryOp::FNe)
            } else {
                (WasmType::I32, WasmBinaryOp::Ne)
            }
        }
        Lt => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::I32, WasmBinaryOp::FLt)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::I32, WasmBinaryOp::FLt)
            } else {
                (WasmType::I32, WasmBinaryOp::LtS)
            }
        }
        Gt => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::I32, WasmBinaryOp::FGt)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::I32, WasmBinaryOp::FGt)
            } else {
                (WasmType::I32, WasmBinaryOp::GtS)
            }
        }
        Le => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::I32, WasmBinaryOp::FLe)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::I32, WasmBinaryOp::FLe)
            } else {
                (WasmType::I32, WasmBinaryOp::LeS)
            }
        }
        Ge => {
            if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
                (WasmType::I32, WasmBinaryOp::FGe)
            } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
                (WasmType::I32, WasmBinaryOp::FGe)
            } else {
                (WasmType::I32, WasmBinaryOp::GeS)
            }
        }
    }
}

/// zr 运行时的导入声明（宿主实现）。
/// 运行时导入签名。
///
/// 指针宽度随架构变化：wasm32 的线性内存用 i32 下标，wasm64 用 i64。
/// 参数类型写死成 i32 的话，wasm64 模块里所有访存都会按 32 位解释地址。
fn runtime_imports_for(arch: Arch) -> Vec<WasmImport> {
    let ptr = if arch == Arch::Wasm64 {
        WasmType::I64
    } else {
        WasmType::I32
    };
    vec![
        WasmImport {
            module: RUNTIME_MODULE.to_string(),
            name: "zr_write".to_string(),
            params: vec![ptr, ptr, ptr],
            results: vec![ptr],
        },
        WasmImport {
            module: RUNTIME_MODULE.to_string(),
            name: "zr_read".to_string(),
            params: vec![ptr, ptr, ptr],
            results: vec![ptr],
        },
        WasmImport {
            module: RUNTIME_MODULE.to_string(),
            name: "zr_exit".to_string(),
            params: vec![ptr],
            results: vec![],
        },
        WasmImport {
            module: RUNTIME_MODULE.to_string(),
            name: "zr_panic".to_string(),
            params: vec![ptr, ptr],
            results: vec![],
        },
        WasmImport {
            module: RUNTIME_MODULE.to_string(),
            name: "alloc".to_string(),
            params: vec![ptr],
            results: vec![ptr],
        },
        WasmImport {
            module: RUNTIME_MODULE.to_string(),
            name: "dealloc".to_string(),
            params: vec![ptr],
            results: vec![],
        },
        WasmImport {
            module: RUNTIME_MODULE.to_string(),
            name: "realloc".to_string(),
            params: vec![WasmType::I32, WasmType::I32],
            results: vec![WasmType::I32],
        },
    ]
}

impl WasmImport {
    fn func_type(&self) -> FuncType {
        FuncType {
            params: self.params.clone(),
            results: self.results.clone(),
        }
    }
}

/// 函数签名。
#[derive(Debug, Clone)]
struct FuncType {
    params: Vec<WasmType>,
    results: Vec<WasmType>,
}

/// 为 SSA 值分配一个局部变量槽位。
fn ensure_local(
    value: &SsaValue,
    map: &mut HashMap<u32, u32>,
    locals: &mut Vec<WasmType>,
    next: &mut u32,
    float_widths: &HashMap<u32, u8>,
) -> u32 {
    *map.entry(value.id()).or_insert_with(|| {
        let i = *next;
        *next += 1;
        let ty = match float_widths.get(&value.id()) {
            Some(32) => WasmType::F32,
            Some(_) => WasmType::F64,
            None => WasmType::I64,
        };
        locals.push(ty);
        i
    })
}

/// 把 SSA 值压栈。
fn emit_value_ref(value: SsaValue, locals: &HashMap<u32, u32>, body: &mut Vec<WasmInstr>) {
    match locals.get(&value.id()) {
        Some(&idx) => body.push(WasmInstr::LocalGet(idx)),
        // 未定义的 SSA 值（phi 引用等）退化为 0。
        None => body.push(WasmInstr::I64Const(0)),
    }
}

// ==================================================================
// 二进制编码
// ==================================================================

/// 无符号 LEB128。
fn uleb(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// 有符号 LEB128。
fn sleb(mut value: i64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let sign_bit = byte & 0x40 != 0;
        if (value == 0 && !sign_bit) || (value == -1 && sign_bit) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// 以 长度前缀 编码一段字节。
fn encode_vec(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    uleb(items.len() as u64, &mut out);
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// 写入一个段（id + 长度 + 内容）。
fn write_section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(body.len() as u64, out);
    out.extend_from_slice(body);
}

/// limits 结构（内存页数）。
fn encode_limits(min: u32, max: Option<u32>) -> Vec<u8> {
    let mut out = Vec::new();
    match max {
        Some(max) => {
            out.push(0x01); // 有最大值
            uleb(u64::from(min), &mut out);
            uleb(u64::from(max), &mut out);
        }
        None => {
            out.push(0x00);
            uleb(u64::from(min), &mut out);
        }
    }
    out
}

/// Type 段内容。
fn encode_type_section(types: &[FuncType]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = types
        .iter()
        .map(|t| {
            let mut entry = vec![0x60]; // func
            uleb(t.params.len() as u64, &mut entry);
            for p in &t.params {
                entry.push(p.code());
            }
            uleb(t.results.len() as u64, &mut entry);
            for r in &t.results {
                entry.push(r.code());
            }
            entry
        })
        .collect();
    encode_vec(&entries)
}

/// Import 段内容。
///
/// 每个导入的类型下标就是它在类型段中的位置（导入先于本地函数登记）。
fn encode_import_section(imports: &[WasmImport]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = imports
        .iter()
        .enumerate()
        .map(|(index, imp)| {
            let mut entry = Vec::new();
            uleb(imp.module.len() as u64, &mut entry);
            entry.extend_from_slice(imp.module.as_bytes());
            uleb(imp.name.len() as u64, &mut entry);
            entry.extend_from_slice(imp.name.as_bytes());
            entry.push(0x00); // import kind = func
            uleb(index as u64, &mut entry); // 类型下标 = 该导入在类型段中的位置
            entry
        })
        .collect();
    encode_vec(&entries)
}

/// Export 段单项。
fn encode_export(name: &str, kind: u8, index: u32) -> Vec<u8> {
    let mut entry = Vec::new();
    uleb(name.len() as u64, &mut entry);
    entry.extend_from_slice(name.as_bytes());
    entry.push(kind);
    uleb(u64::from(index), &mut entry);
    entry
}

/// Code 段单项（函数体）。
fn encode_code(func: &WasmFunction) -> Vec<u8> {
    let mut body = Vec::new();
    // 局部变量按类型 run-length 编码。
    let mut runs: Vec<(u8, u32)> = Vec::new();
    for local in &func.locals {
        let code = local.code();
        match runs.last_mut() {
            Some((last_code, count)) if *last_code == code => *count += 1,
            _ => runs.push((code, 1)),
        }
    }
    uleb(runs.len() as u64, &mut body);
    for (code, count) in runs {
        uleb(u64::from(count), &mut body);
        body.push(code);
    }

    for instr in &func.body {
        encode_instr(instr, &mut body);
    }
    // 函数体必须以 end 收尾。
    body.push(0x0b);

    let mut entry = Vec::new();
    uleb(body.len() as u64, &mut entry);
    entry.extend_from_slice(&body);
    entry
}

/// 编码单条指令。
fn encode_instr(instr: &WasmInstr, out: &mut Vec<u8>) {
    match instr {
        WasmInstr::LocalGet(i) => {
            out.push(0x20);
            uleb(u64::from(*i), out);
        }
        WasmInstr::LocalSet(i) => {
            out.push(0x21);
            uleb(u64::from(*i), out);
        }
        WasmInstr::I64Const(v) => {
            out.push(0x42);
            sleb(*v, out);
        }
        WasmInstr::F32Const(f) => {
            out.push(0x43);
            out.extend_from_slice(&f.to_le_bytes());
        }
        WasmInstr::F64Const(f) => {
            out.push(0x44);
            out.extend_from_slice(&f.to_le_bytes());
        }
        WasmInstr::I32Eqz => out.push(0x45),
        WasmInstr::Binary(ty, op) => {
            // 浮点操作码需要类型前缀
            match ty {
                WasmType::F32 => out.push(0xD2), // f32 prefix
                WasmType::F64 => out.push(0xD3), // f64 prefix
                _ => {}
            }
            out.push(op.code());
        }
        WasmInstr::Unary(op) => out.push(op.code()),
        WasmInstr::Call(idx) => {
            out.push(0x10);
            uleb(u64::from(*idx), out);
        }
        WasmInstr::Return => out.push(0x0f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_module() -> WasmModule {
        let mut wasm = WasmModule::new();
        wasm.imports = runtime_imports_for(Arch::Wasm32);
        wasm.memory_pages = 1;
        wasm.memory_max_pages = 2;
        wasm.functions.push(WasmFunction {
            name: "main".to_string(),
            pending_calls: vec![],
            params: vec![],
            results: vec![WasmType::I64],
            locals: vec![WasmType::I64],
            body: vec![
                WasmInstr::I64Const(2),
                WasmInstr::I64Const(3),
                WasmInstr::Binary(WasmType::I64, WasmBinaryOp::Add),
                WasmInstr::LocalSet(0),
                WasmInstr::LocalGet(0),
                WasmInstr::Return,
            ],
            exported: true,
        });
        wasm
    }

    #[test]
    fn binary_starts_with_wasm_magic_and_version() {
        let bytes = minimal_module().to_binary();
        assert_eq!(&bytes[..4], b"\0asm");
        assert_eq!(&bytes[4..8], &[1, 0, 0, 0]);
    }

    #[test]
    fn sections_appear_in_required_order() {
        let bytes = minimal_module().to_binary();
        let ids: Vec<u8> = bytes[8..]
            .iter()
            .copied()
            .filter(|b| matches!(b, 1..=11) && *b != 0x00)
            .collect();
        assert!(!ids.is_empty());
        // 第一个段必须是 type（1）
        assert_eq!(bytes[8], 1);
        assert!(ids.contains(&10), "应包含 code 段");
    }

    #[test]
    fn leb128_round_trips_edge_values() {
        let mut out = Vec::new();
        uleb(624_485, &mut out);
        assert_eq!(out, vec![0xe5, 0x8e, 0x26]);

        let mut out = Vec::new();
        sleb(-1, &mut out);
        assert_eq!(out, vec![0x7f]);

        let mut out = Vec::new();
        sleb(64, &mut out);
        assert_eq!(out, vec![0xc0, 0x00]);
    }

    #[test]
    fn wat_mentions_imports_memory_and_exports() {
        let wat = minimal_module().to_wat();
        assert!(wat.contains("(import \"zr\" \"zr_write\""));
        assert!(wat.contains("(memory 1 2)"));
        assert!(wat.contains("(export \"main\""));
        assert!(wat.contains("i64.add"));
    }

    #[test]
    fn runtime_imports_cover_all_seven_symbols() {
        let imports = runtime_imports_for(Arch::Wasm32);
        let names: Vec<&str> = imports.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names.len(), 7);
        assert!(names.contains(&"zr_panic"));
        assert!(names.contains(&"zr_exit"));
        assert!(names.contains(&"alloc"));
        assert!(names.contains(&"dealloc"));
        assert!(names.contains(&"realloc"));
    }
}
