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
    /// 32 位常量（栈调度用的程序计数器等）。
    I32Const(i32),
    /// 32 位浮点常量。
    F32Const(f32),
    /// 64 位浮点常量。
    F64Const(f64),
    /// `i64.eqz`：逻辑非（操作数是 i64，结果为 i32）。
    I64Eqz,
    /// 二元运算。类型参数是**操作数**类型（决定选哪条指令），不是结果类型：
    /// 整数比较的操作数是 i64，但结果是 i32。
    Binary(WasmType, WasmBinaryOp),
    /// 一元运算：`i32.wrap_i64` / `i64.extend_i32_s` / `i64.extend_i32_u` / `f32.neg` / `f64.neg` / `f32.abs` / `f64.abs` 等。
    Unary(WasmUnaryOp),
    /// `call funcidx`。
    Call(u32),
    Return,
    /// `unreachable`。
    Unreachable,
    /// `block`（空结果类型）。
    Block,
    /// `loop`（空结果类型）。
    Loop,
    /// `if`（空结果类型）。
    If,
    /// `else`。
    Else,
    /// `end`：关闭最近的 `block`/`loop`/`if`。
    End,
    /// `br depth`：跳转到处于相对深度 `depth` 的块标签。
    Br(u32),
    /// `br_table`：按栈顶索引跳转（`targets` 为各索引对应的深度，`default` 为越界回退）。
    BrTable(Vec<u32>, u32),
    /// 从线性内存加载：`(addr) -> value`。
    Load { op: MemOp, addr64: bool },
    /// 存入线性内存：`(addr, value) -> ()`。
    Store { op: MemOp, addr64: bool },
}

/// 一次线性内存访问的形态：字节宽度、整数窄访问是否符号扩展、是否浮点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemOp {
    /// 访问宽度（字节数）：1 / 2 / 4 / 8。
    pub width: u8,
    /// 窄整数加载是否符号扩展（存储时会忽略）。
    pub signed: bool,
    /// 是否浮点访问（f32 / f64）。
    pub float: bool,
}

/// 一元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
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
            // f32 一组：abs 0x8B … sqrt 0x91；f64 一组：abs 0x99 … sqrt 0x9F。
            WasmUnaryOp::F32Abs => 0x8B,
            WasmUnaryOp::F32Neg => 0x8C,
            WasmUnaryOp::F32Ceil => 0x8D,
            WasmUnaryOp::F32Floor => 0x8E,
            WasmUnaryOp::F32Trunc => 0x8F,
            WasmUnaryOp::F32Nearest => 0x90,
            WasmUnaryOp::F32Sqrt => 0x91,
            WasmUnaryOp::F64Abs => 0x99,
            WasmUnaryOp::F64Neg => 0x9A,
            WasmUnaryOp::F64Ceil => 0x9B,
            WasmUnaryOp::F64Floor => 0x9C,
            WasmUnaryOp::F64Trunc => 0x9D,
            WasmUnaryOp::F64Nearest => 0x9E,
            WasmUnaryOp::F64Sqrt => 0x9F,
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
#[allow(dead_code)]
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
    ShrU,
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
            ShrU => "shr_u",
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

    /// 二进制操作码。`ty` 是**操作数**类型：整数运算有 i32/i64 两套操作码，
    /// 浮点运算的 f32/f64 也各有独立操作码（不是前缀）。
    fn code(self, ty: WasmType) -> u8 {
        use WasmBinaryOp::*;
        if ty == WasmType::F32 || ty == WasmType::F64 {
            let f64 = ty == WasmType::F64;
            return match self {
                FAdd => if f64 { 0xA0 } else { 0x92 },
                FSub => if f64 { 0xA1 } else { 0x93 },
                FMul => if f64 { 0xA2 } else { 0x94 },
                FDiv => if f64 { 0xA3 } else { 0x95 },
                FMin => if f64 { 0xA4 } else { 0x96 },
                FMax => if f64 { 0xA5 } else { 0x97 },
                FCopysign => if f64 { 0xA6 } else { 0x98 },
                FEq => if f64 { 0x61 } else { 0x5B },
                FNe => if f64 { 0x62 } else { 0x5C },
                FLt => if f64 { 0x63 } else { 0x5D },
                FGt => if f64 { 0x64 } else { 0x5E },
                FLe => if f64 { 0x65 } else { 0x5F },
                FGe => if f64 { 0x66 } else { 0x60 },
                _ => unreachable!("整数运算不作用于浮点类型"),
            };
        }
        let i64 = ty == WasmType::I64;
        match self {
            Eq => if i64 { 0x51 } else { 0x46 },
            Ne => if i64 { 0x52 } else { 0x47 },
            LtS => if i64 { 0x53 } else { 0x48 },
            GtS => if i64 { 0x55 } else { 0x4a },
            LeS => if i64 { 0x57 } else { 0x4c },
            GeS => if i64 { 0x59 } else { 0x4e },
            Add => if i64 { 0x7c } else { 0x6a },
            Sub => if i64 { 0x7d } else { 0x6b },
            Mul => if i64 { 0x7e } else { 0x6c },
            DivS => if i64 { 0x7f } else { 0x6d },
            RemS => if i64 { 0x81 } else { 0x6f },
            And => if i64 { 0x83 } else { 0x71 },
            Or => if i64 { 0x84 } else { 0x72 },
            Xor => if i64 { 0x85 } else { 0x73 },
            Shl => if i64 { 0x86 } else { 0x74 },
            ShrS => if i64 { 0x87 } else { 0x75 },
            ShrU => if i64 { 0x88 } else { 0x76 },
            _ => unreachable!("浮点运算需要浮点类型"),
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
    /// 线性内存初值（从地址 0 起）：字符串字面量的 `.rodata` 与被取地址
    /// 局部变量的栈槽都放在这里。
    pub data: Vec<u8>,
    /// 数据段使用 i64 偏移（wasm64）。
    pub data_addr64: bool,
}

/// 字符串字面量与被取地址局部变量在数据区里的起始地址。
///
/// 首 1 KiB 留空，避免合法的字符串/槽地址正好等于 0（空指针）。
const DATA_START: usize = 1024;

impl WasmModule {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 SSA 模块构建 WASM 模块。
    pub fn from_ssa(module: &SsaModule, arch: Arch) -> Self {
        let mut wasm = WasmModule::new();
        let addr64 = arch == Arch::Wasm64;
        // 运行时函数以导入形式提供，宿主负责实现。签名随指针宽度变化。
        wasm.imports = runtime_imports_for(arch);
        wasm.data_addr64 = addr64;

        // ---- 规划数据区：字符串字面量（去重）在前，取地址槽在后 ----
        let mut data = vec![0u8; DATA_START];
        let mut strings: HashMap<String, i64> = HashMap::new();
        let mut slot_maps: Vec<HashMap<u32, i64>> = Vec::with_capacity(module.functions.len());
        for func in &module.functions {
            // 字符串字面量按首次出现顺序分配，保证产物确定。
            for block in &func.blocks {
                for instr in &block.instrs {
                    if let SsaInstr::Const {
                        value: SsaConst::Str(text),
                        ..
                    } = instr
                    {
                        if !strings.contains_key(text) {
                            let addr = data.len() as i64;
                            data.extend_from_slice(text.as_bytes());
                            data.push(0);
                            strings.insert(text.clone(), addr);
                        }
                    }
                }
            }
            // 本函数里每个被取地址的局部变量各留 8 字节槽（零初始化）。
            let mut slots: HashMap<u32, i64> = HashMap::new();
            for block in &func.blocks {
                for instr in &block.instrs {
                    if let SsaInstr::SlotAddr { local, .. } = instr {
                        slots.entry(*local).or_insert_with(|| {
                            let addr = data.len() as i64;
                            data.extend_from_slice(&[0u8; 8]);
                            addr
                        });
                    }
                }
            }
            slot_maps.push(slots);
        }

        // 内存页数至少要覆盖数据区。
        let pages = (data.len().div_ceil(65536)).max(2) as u32;
        wasm.memory_pages = pages;
        wasm.memory_max_pages = pages.max(256);
        wasm.data = data;

        for (i, func) in module.functions.iter().enumerate() {
            let compiled = WasmFunction::from_ssa(func, addr64, &strings, &slot_maps[i]);
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

        // memory64 的 WAT 写法是 `(memory i64 1 2)`。
        let addr_ty = if self.data_addr64 { "i64 " } else { "" };
        out.push_str(&format!(
            "  (memory {}{} {})",
            addr_ty, self.memory_pages, self.memory_max_pages
        ));
        out.push('\n');

        if !self.data.is_empty() {
            let ty = if self.data_addr64 { "i64" } else { "i32" };
            out.push_str(&format!("  (data ({ty}.const 0) \""));
            out.push_str(&wat_escape(&self.data));
            out.push_str("\")\n");
        }

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
        //
        // 每个本地函数的类型按顺序追加到类型段尾部，所以它在类型段里的下标
        // 是 `type_index_base + i`。全部写成 `type_index_base` 会把所有函数
        // 都错标成第一个本地函数的签名。
        let func_type_indices: Vec<Vec<u8>> = (0..self.functions.len())
            .map(|i| {
                let mut bytes = Vec::new();
                uleb((type_index_base + i) as u64, &mut bytes);
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
                self.data_addr64,
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

        // ---- Data 段（活跃数据段，偏移 0）----
        if !self.data.is_empty() {
            write_section(&mut out, 11, &encode_data_section(&self.data, self.data_addr64));
        }
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
            WasmInstr::I32Const(v) => format!("i32.const {v}"),
            WasmInstr::F32Const(f) => format!("f32.const {f}"),
            WasmInstr::F64Const(f) => format!("f64.const {f}"),
            WasmInstr::I64Eqz => "i64.eqz".to_string(),
            WasmInstr::Binary(ty, op) => op.wat(*ty),
            WasmInstr::Unary(op) => op.wat().to_string(),
            WasmInstr::Call(idx) => format!("call {idx}"),
            WasmInstr::Return => "return".to_string(),
            WasmInstr::Unreachable => "unreachable".to_string(),
            WasmInstr::Block => "block".to_string(),
            WasmInstr::Loop => "loop".to_string(),
            WasmInstr::If => "if".to_string(),
            WasmInstr::Else => "else".to_string(),
            WasmInstr::End => "end".to_string(),
            WasmInstr::Br(depth) => format!("br {depth}"),
            WasmInstr::BrTable(targets, default) => {
                let list: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
                format!("br_table {} {}", list.join(" "), default)
            }
            WasmInstr::Load { op, addr64 } => mem_wat(true, *op, *addr64),
            WasmInstr::Store { op, addr64 } => mem_wat(false, *op, *addr64),
        }
    }
}

impl WasmFunction {
    /// 从 SSA 函数构建 WASM 函数。
    ///
    /// `addr64` 决定线性内存地址宽度（wasm64 用 i64，wasm32 用 i32）；
    /// `strings` 是字符串字面量 → 线性内存地址；`slots` 是本函数里被取地址
    /// 的局部变量编号 → 线性内存槽地址。
    ///
    /// 控制流用**栈调度**（stackification）实现：所有基本块体放进一个
    /// `loop`，块首用 `br_table` 按程序计数器分派；每个块的终结符把下一条
    /// 目标写回程序计数器后 `br` 回循环头。这样任意 CFG（含循环、回边）都
    /// 能映射到 wasm 的结构化控制流。
    pub fn from_ssa(
        func: &SsaFunction,
        addr64: bool,
        strings: &HashMap<String, i64>,
        slots: &HashMap<u32, i64>,
    ) -> Self {
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
        // 保留一个 i32 局部变量作为栈调度的程序计数器。
        let pc_idx = next_local;
        locals.push(WasmType::I32);
        next_local += 1;

        let mut body = Vec::new();
        let mut pending_calls: Vec<(usize, String)> = Vec::new();

        // 收集每条边上要执行的 φ 复制：`(目标块, 前驱块) -> [(φ 结果, 传入值)]`。
        let mut edge_phis: HashMap<(usize, usize), Vec<(SsaValue, SsaValue)>> = HashMap::new();
        for (bi, block) in func.blocks.iter().enumerate() {
            for instr in &block.instrs {
                if let SsaInstr::Phi {
                    result, incoming, ..
                } = instr
                {
                    for (pred, val) in incoming {
                        edge_phis
                            .entry((bi, *pred))
                            .or_default()
                            .push((*result, *val));
                    }
                }
            }
        }

        let n = func.blocks.len();

        // ---- 调度头：loop { block b0 { ... { block b_{n-1} { br_table } } } ----
        body.push(WasmInstr::Loop);
        for _ in 0..n {
            body.push(WasmInstr::Block);
        }
        body.push(WasmInstr::LocalGet(pc_idx));
        let targets: Vec<u32> = (0..n as u32).collect();
        body.push(WasmInstr::BrTable(targets, 0));

        for (bi, block) in func.blocks.iter().enumerate() {
            // 关闭这个块。`br_table` 分派到某块时会落到它 `end` 之后，
            // 也就是下面紧跟的块体。
            body.push(WasmInstr::End);
            for instr in &block.instrs {
                emit_ssa_instr(
                    instr,
                    addr64,
                    strings,
                    slots,
                    &mut local_map,
                    &mut locals,
                    &mut next_local,
                    &float_widths,
                    &mut body,
                    &mut pending_calls,
                );
            }

            // 循环体的 `br` 目标深度：块体 i 之上还压着 b_{i+1}..b_{n-1}。
            let loop_depth = (n - 1 - bi) as u32;
            match &block.term {
                SsaTerminator::Goto(target) => {
                    emit_edge_phis(
                        *target,
                        bi,
                        &edge_phis,
                        &mut local_map,
                        &mut locals,
                        &mut next_local,
                        &float_widths,
                        &mut body,
                    );
                    body.push(WasmInstr::I32Const(*target as i32));
                    body.push(WasmInstr::LocalSet(pc_idx));
                    body.push(WasmInstr::Br(loop_depth));
                }
                SsaTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                } => {
                    emit_value_ref(*cond, &local_map, &mut body);
                    body.push(WasmInstr::Unary(WasmUnaryOp::WrapI64ToI32));
                    body.push(WasmInstr::If);
                    emit_edge_phis(
                        *then_block,
                        bi,
                        &edge_phis,
                        &mut local_map,
                        &mut locals,
                        &mut next_local,
                        &float_widths,
                        &mut body,
                    );
                    body.push(WasmInstr::I32Const(*then_block as i32));
                    body.push(WasmInstr::LocalSet(pc_idx));
                    body.push(WasmInstr::Else);
                    emit_edge_phis(
                        *else_block,
                        bi,
                        &edge_phis,
                        &mut local_map,
                        &mut locals,
                        &mut next_local,
                        &float_widths,
                        &mut body,
                    );
                    body.push(WasmInstr::I32Const(*else_block as i32));
                    body.push(WasmInstr::LocalSet(pc_idx));
                    body.push(WasmInstr::End);
                    body.push(WasmInstr::Br(loop_depth));
                }
                SsaTerminator::Return(v) => {
                    match v {
                        Some(val) => emit_value_ref(*val, &local_map, &mut body),
                        None => body.push(WasmInstr::I64Const(0)),
                    }
                    body.push(WasmInstr::Return);
                }
                SsaTerminator::Unreachable => body.push(WasmInstr::Unreachable),
            }
        }

        // 关闭调度循环。循环只会通过 `return` 退出，所以后面的函数尾是
        // 不可达代码——放一条 `unreachable` 让栈多态，函数才通过校验。
        body.push(WasmInstr::End);
        body.push(WasmInstr::Unreachable);

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
        let mark = |v: &SsaValue, w: u8, map: &mut HashMap<u32, u8>| -> bool {
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
                    // 浮点元素的加载结果与浮点存储的值也要标成浮点，
                    // 否则它们会被当成 i64，操作码选错。
                    SsaInstr::Load { result, ty, .. } if ty.is_float() => {
                        let w = match *ty {
                            NumTy::Float(w) => w,
                            _ => 64,
                        };
                        if mark(result, w, &mut floats) {
                            changed = true;
                        }
                    }
                    SsaInstr::Store { value, ty, .. } if ty.is_float() => {
                        let w = match *ty {
                            NumTy::Float(w) => w,
                            _ => 64,
                        };
                        if mark(value, w, &mut floats) {
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
                    SsaInstr::Asm { .. } => {
                        // 内联汇编：无浮点结果，不需要标记浮点宽度
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

/// 二元运算是否产生布尔（i32）结果。
fn binop_is_compare(op: SsaBinaryOp) -> bool {
    use SsaBinaryOp::*;
    matches!(op, Eq | Ne | Lt | Gt | Le | Ge)
}

/// 映射 SSA 二元运算到 wasm 运算，返回**操作数**类型与操作。
///
/// 比较运算的操作数是 i64 或浮点，结果却是 i32；调用方负责把 i32 结果
/// 扩展回 i64（见 `emit_ssa_instr` / `compute_value_types`）。
fn wasm_binop(op: SsaBinaryOp, lhs_ty: WasmType, rhs_ty: WasmType) -> (WasmType, WasmBinaryOp) {
    use SsaBinaryOp::*;
    let float_ty = if lhs_ty == WasmType::F32 || rhs_ty == WasmType::F32 {
        Some(WasmType::F32)
    } else if lhs_ty == WasmType::F64 || rhs_ty == WasmType::F64 {
        Some(WasmType::F64)
    } else {
        None
    };
    let ft = float_ty.unwrap_or(WasmType::I64);
    let is_f = float_ty.is_some();
    match op {
        Add => (ft, if is_f { WasmBinaryOp::FAdd } else { WasmBinaryOp::Add }),
        Sub => (ft, if is_f { WasmBinaryOp::FSub } else { WasmBinaryOp::Sub }),
        Mul => (ft, if is_f { WasmBinaryOp::FMul } else { WasmBinaryOp::Mul }),
        Div => (ft, if is_f { WasmBinaryOp::FDiv } else { WasmBinaryOp::DivS }),
        Rem => (WasmType::I64, WasmBinaryOp::RemS),
        And | BitAnd => (WasmType::I64, WasmBinaryOp::And),
        Or | BitOr => (WasmType::I64, WasmBinaryOp::Or),
        BitXor => (WasmType::I64, WasmBinaryOp::Xor),
        Shl => (WasmType::I64, WasmBinaryOp::Shl),
        Shr => (WasmType::I64, WasmBinaryOp::ShrS),
        Eq => (ft, if is_f { WasmBinaryOp::FEq } else { WasmBinaryOp::Eq }),
        Ne => (ft, if is_f { WasmBinaryOp::FNe } else { WasmBinaryOp::Ne }),
        Lt => (ft, if is_f { WasmBinaryOp::FLt } else { WasmBinaryOp::LtS }),
        Gt => (ft, if is_f { WasmBinaryOp::FGt } else { WasmBinaryOp::GtS }),
        Le => (ft, if is_f { WasmBinaryOp::FLe } else { WasmBinaryOp::LeS }),
        Ge => (ft, if is_f { WasmBinaryOp::FGe } else { WasmBinaryOp::GeS }),
    }
}

/// zr 运行时的导入声明（宿主实现）。
/// 运行时导入签名。
///
/// 指针宽度随架构变化：wasm32 的线性内存用 i32 下标，wasm64 用 i64。
/// 参数类型写死成 i32 的话，wasm64 模块里所有访存都会按 32 位解释地址。
pub fn runtime_imports_for(arch: Arch) -> Vec<WasmImport> {
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

/// 线性内存访问形态。
fn mem_op_for(ty: NumTy) -> MemOp {
    match ty {
        NumTy::Int(w) | NumTy::Uint(w) => MemOp {
            width: (w / 8).max(1),
            signed: matches!(ty, NumTy::Int(_)),
            float: false,
        },
        NumTy::Float(w) => MemOp {
            width: (w / 8).max(1),
            signed: false,
            float: true,
        },
        NumTy::Bool => MemOp {
            width: 1,
            signed: false,
            float: false,
        },
    }
}

/// 发射一个基本块体内的非终结指令。
#[allow(clippy::too_many_arguments)]
fn emit_ssa_instr(
    instr: &SsaInstr,
    addr64: bool,
    strings: &HashMap<String, i64>,
    slots: &HashMap<u32, i64>,
    local_map: &mut HashMap<u32, u32>,
    locals: &mut Vec<WasmType>,
    next_local: &mut u32,
    float_widths: &HashMap<u32, u8>,
    body: &mut Vec<WasmInstr>,
    pending_calls: &mut Vec<(usize, String)>,
) {
    // 值的 wasm 类型以 `float_widths` 为唯一依据：局部变量槽位类型也是
    // 由它决定的，两边必须一致，否则会出现"指令按 f32、槽位是 f64"的校验错。
    let wasm_ty = |id: u32| match float_widths.get(&id) {
        Some(32) => WasmType::F32,
        Some(_) => WasmType::F64,
        None => WasmType::I64,
    };
    match instr {
        SsaInstr::Cast {
            result,
            src,
            from,
            to,
        } => {
            let idx = ensure_local(result, local_map, locals, next_local, float_widths);
            let src_idx = ensure_local(src, local_map, locals, next_local, float_widths);
            body.push(WasmInstr::LocalGet(src_idx));
            if from.is_float() || to.is_float() {
                match (from, to) {
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
                // 整数 → 整数：值按 i64 建模，只有目标位宽 B<64 时才需要
                // 截断并重新扩展。`(x << (64-B)) >> (64-B)` 对符号/零扩展
                // 都适用（B=32 时等价于 wrap+extend，B<32 时正确截到低 B 位）。
                let (bits, signed) = match to {
                    NumTy::Int(w) => (*w, true),
                    NumTy::Uint(w) => (*w, false),
                    _ => (8, false),
                };
                if bits < 64 {
                    let shamt = i64::from(64 - bits);
                    body.push(WasmInstr::I64Const(shamt));
                    body.push(WasmInstr::Binary(WasmType::I64, WasmBinaryOp::Shl));
                    body.push(WasmInstr::I64Const(shamt));
                    body.push(WasmInstr::Binary(
                        WasmType::I64,
                        if signed {
                            WasmBinaryOp::ShrS
                        } else {
                            WasmBinaryOp::ShrU
                        },
                    ));
                }
            }
            body.push(WasmInstr::LocalSet(idx));
        }
        SsaInstr::Const { result, value } => {
            let idx = ensure_local(result, local_map, locals, next_local, float_widths);
            match value {
                SsaConst::Int(i) => body.push(WasmInstr::I64Const(*i as i64)),
                SsaConst::Bool(b) => body.push(WasmInstr::I64Const(i64::from(*b))),
                SsaConst::Unit => body.push(WasmInstr::I64Const(0)),
                SsaConst::Float(bits, w) => {
                    if *w == 32 {
                        body.push(WasmInstr::F32Const(f32::from_bits(*bits as u32)));
                    } else {
                        body.push(WasmInstr::F64Const(f64::from_bits(*bits)));
                    }
                }
                // 字符串字面量就是它在 `.rodata`（线性内存）里的地址。
                SsaConst::Str(text) => {
                    let addr = strings.get(text).copied().unwrap_or(0);
                    body.push(WasmInstr::I64Const(addr));
                }
            }
            body.push(WasmInstr::LocalSet(idx));
        }
        SsaInstr::Unary {
            result,
            op,
            operand,
        } => {
            let idx = ensure_local(result, local_map, locals, next_local, float_widths);
            let operand_idx = ensure_local(operand, local_map, locals, next_local, float_widths);
            body.push(WasmInstr::LocalGet(operand_idx));
            let op_ty = wasm_ty(operand.id());
            match op {
                SsaUnaryOp::Not => {
                    // i64.eqz 出 i32，扩展回 i64 以保持"整数即 i64"的不变式。
                    body.push(WasmInstr::I64Eqz);
                    body.push(WasmInstr::Unary(WasmUnaryOp::ExtendI32U));
                }
                SsaUnaryOp::Neg => {
                    if op_ty == WasmType::F32 {
                        body.push(WasmInstr::Unary(WasmUnaryOp::F32Neg));
                    } else if op_ty == WasmType::F64 {
                        body.push(WasmInstr::Unary(WasmUnaryOp::F64Neg));
                    } else {
                        // 栈上已是操作数，乘以 -1 即可取负（`0 - x` 需要
                        // 先压 0，而操作数已经压好了）。
                        body.push(WasmInstr::I64Const(-1));
                        body.push(WasmInstr::Binary(WasmType::I64, WasmBinaryOp::Mul));
                    }
                }
                SsaUnaryOp::BitNot => {
                    if op_ty == WasmType::F32 || op_ty == WasmType::F64 {
                        crate::codegen::unsupported_const("float bitnot");
                    } else {
                        body.push(WasmInstr::I64Const(-1));
                        body.push(WasmInstr::Binary(WasmType::I64, WasmBinaryOp::Xor));
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
            let idx = ensure_local(result, local_map, locals, next_local, float_widths);
            let lhs_idx = ensure_local(lhs, local_map, locals, next_local, float_widths);
            let rhs_idx = ensure_local(rhs, local_map, locals, next_local, float_widths);
            body.push(WasmInstr::LocalGet(lhs_idx));
            body.push(WasmInstr::LocalGet(rhs_idx));
            let lhs_ty = wasm_ty(lhs.id());
            let rhs_ty = wasm_ty(rhs.id());
            let (mut op_ty, wasm_op) = wasm_binop(*op, lhs_ty, rhs_ty);
            // 操作数类型未知时（例如未标注的形参），用 SSA 的结果类型兜底
            if op_ty == WasmType::I64 {
                if let NumTy::Float(w) = ty {
                    op_ty = if *w == 32 { WasmType::F32 } else { WasmType::F64 };
                }
            }
            body.push(WasmInstr::Binary(op_ty, wasm_op));
            if binop_is_compare(*op) {
                // 比较结果（i32）统一扩展到 i64，以维持"整数即 i64"。
                body.push(WasmInstr::Unary(WasmUnaryOp::ExtendI32U));
            }
            body.push(WasmInstr::LocalSet(idx));
        }
        SsaInstr::Call {
            result,
            callee,
            args,
            ty: _,
        } => {
            for a in args {
                emit_value_ref(*a, local_map, body);
            }
            pending_calls.push((body.len(), callee.clone()));
            body.push(WasmInstr::Call(0));
            let idx = ensure_local(result, local_map, locals, next_local, float_widths);
            body.push(WasmInstr::LocalSet(idx));
        }
        SsaInstr::Copy { result, src } => {
            emit_value_ref(*src, local_map, body);
            let idx = ensure_local(result, local_map, locals, next_local, float_widths);
            body.push(WasmInstr::LocalSet(idx));
        }
        SsaInstr::Phi { result, .. } | SsaInstr::Var { result, .. } => {
            // φ 的实际复制在**前驱边**上执行（见 `emit_edge_phis`）；
            // `Var` 只是名字占位。这里只需保证槽位存在。
            ensure_local(result, local_map, locals, next_local, float_widths);
        }
        SsaInstr::SlotAddr { result, local } => {
            let addr = slots.get(local).copied().unwrap_or(0);
            body.push(WasmInstr::I64Const(addr));
            let idx = ensure_local(result, local_map, locals, next_local, float_widths);
            body.push(WasmInstr::LocalSet(idx));
        }
        SsaInstr::Load { result, ptr, ty } => {
            let idx = ensure_local(result, local_map, locals, next_local, float_widths);
            let ptr_idx = ensure_local(ptr, local_map, locals, next_local, float_widths);
            let op = mem_op_for(*ty);
            body.push(WasmInstr::LocalGet(ptr_idx));
            push_addr(addr64, body);
            body.push(WasmInstr::Load { op, addr64 });
            if !op.float && !addr64 {
                // wasm32 的窄/全宽整数加载出 i32，扩展回 i64。
                // width==8 时用的是 i64.load，已是 i64，无需扩展。
                if op.width < 8 {
                    body.push(WasmInstr::Unary(if op.signed {
                        WasmUnaryOp::ExtendI32S
                    } else {
                        WasmUnaryOp::ExtendI32U
                    }));
                }
            }
            body.push(WasmInstr::LocalSet(idx));
        }
        SsaInstr::Store { ptr, value, ty } => {
            let op = mem_op_for(*ty);
            let ptr_idx = ensure_local(ptr, local_map, locals, next_local, float_widths);
            let val_idx = ensure_local(value, local_map, locals, next_local, float_widths);
            body.push(WasmInstr::LocalGet(ptr_idx));
            push_addr(addr64, body);
            body.push(WasmInstr::LocalGet(val_idx));
            if !op.float && !addr64 && op.width < 8 {
                // wasm32 的窄整数存储取 i32 值。
                body.push(WasmInstr::Unary(WasmUnaryOp::WrapI64ToI32));
            }
            body.push(WasmInstr::Store { op, addr64 });
        }
        SsaInstr::Asm { .. } => {
            crate::codegen::unsupported_inline_asm("wasm32");
        }
    }
}

/// 把栈上的 i64 地址转成目标线性内存地址宽度（wasm32 截到 i32）。
fn push_addr(addr64: bool, body: &mut Vec<WasmInstr>) {
    if !addr64 {
        body.push(WasmInstr::Unary(WasmUnaryOp::WrapI64ToI32));
    }
}

/// 在前驱边上执行目标块的 φ 复制。
#[allow(clippy::too_many_arguments)]
fn emit_edge_phis(
    target: usize,
    pred: usize,
    edge_phis: &HashMap<(usize, usize), Vec<(SsaValue, SsaValue)>>,
    local_map: &mut HashMap<u32, u32>,
    locals: &mut Vec<WasmType>,
    next_local: &mut u32,
    float_widths: &HashMap<u32, u8>,
    body: &mut Vec<WasmInstr>,
) {
    if let Some(list) = edge_phis.get(&(target, pred)) {
        for (phi, val) in list {
            emit_value_ref(*val, local_map, body);
            let idx = ensure_local(phi, local_map, locals, next_local, float_widths);
            body.push(WasmInstr::LocalSet(idx));
        }
    }
}

/// 线性内存 load/store 的 WAT 助记符。
fn mem_wat(is_load: bool, op: MemOp, addr64: bool) -> String {
    if op.float {
        let name = if op.width == 4 { "f32" } else { "f64" };
        let action = if is_load { "load" } else { "store" };
        return format!("{name}.{action}");
    }
    if addr64 {
        let name = match (is_load, op.width, op.signed) {
            (true, 1, true) => "i64.load8_s",
            (true, 1, false) => "i64.load8_u",
            (true, 2, true) => "i64.load16_s",
            (true, 2, false) => "i64.load16_u",
            (true, 4, true) => "i64.load32_s",
            (true, 4, false) => "i64.load32_u",
            (true, _, _) => "i64.load",
            (false, 1, _) => "i64.store8",
            (false, 2, _) => "i64.store16",
            (false, 4, _) => "i64.store32",
            (false, _, _) => "i64.store",
        };
        name.to_string()
    } else {
        let name = match (is_load, op.width, op.signed) {
            (true, 1, true) => "i32.load8_s",
            (true, 1, false) => "i32.load8_u",
            (true, 2, true) => "i32.load16_s",
            (true, 2, false) => "i32.load16_u",
            (true, _, _) => "i32.load",
            (false, 1, _) => "i32.store8",
            (false, 2, _) => "i32.store16",
            (false, _, _) => "i32.store",
        };
        name.to_string()
    }
}

/// 线性内存 load/store 的主操作码与自然对齐（log2 字节数）。
fn mem_opcode(is_load: bool, op: MemOp, addr64: bool) -> (u8, u32) {
    let align = match op.width {
        1 => 0,
        2 => 1,
        4 => 2,
        _ => 3,
    };
    let code = if op.float {
        match (is_load, op.width) {
            (true, 4) => 0x2a,
            (true, _) => 0x2b,
            (false, 4) => 0x38,
            (false, _) => 0x39,
        }
    } else if addr64 {
        match (is_load, op.width, op.signed) {
            (true, 1, true) => 0x30,
            (true, 1, false) => 0x31,
            (true, 2, true) => 0x32,
            (true, 2, false) => 0x33,
            (true, 4, true) => 0x34,
            (true, 4, false) => 0x35,
            (true, _, _) => 0x29,
            (false, 1, _) => 0x3c,
            (false, 2, _) => 0x3d,
            (false, 4, _) => 0x3e,
            (false, _, _) => 0x37,
        }
    } else {
        match (is_load, op.width, op.signed) {
            (true, 1, true) => 0x2c,
            (true, 1, false) => 0x2d,
            (true, 2, true) => 0x2e,
            (true, 2, false) => 0x2f,
            (true, 4, _) => 0x28,
            (true, _, _) => 0x29,
            (false, 1, _) => 0x3a,
            (false, 2, _) => 0x3b,
            (false, _, _) => 0x36,
        }
    };
    (code, align)
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
///
/// flags 的第 0 位表示"有最大值"，第 2 位（`0x04`）表示这是 64 位内存
/// （memory64 提案）。wasm64 目标用 i64 指针与 i64 数据段偏移，内存必须
/// 声明成 64 位，否则模块自相矛盾、任何运行时都无法加载。
fn encode_limits(min: u32, max: Option<u32>, memory64: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mem64 = if memory64 { 0x04 } else { 0x00 };
    match max {
        Some(max) => {
            out.push(0x01 | mem64);
            uleb(u64::from(min), &mut out);
            uleb(u64::from(max), &mut out);
        }
        None => {
            out.push(0x00 | mem64);
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
        WasmInstr::I32Const(v) => {
            out.push(0x41);
            sleb(i64::from(*v), out);
        }
        WasmInstr::F32Const(f) => {
            out.push(0x43);
            out.extend_from_slice(&f.to_le_bytes());
        }
        WasmInstr::F64Const(f) => {
            out.push(0x44);
            out.extend_from_slice(&f.to_le_bytes());
        }
        WasmInstr::I64Eqz => out.push(0x50),
        WasmInstr::Binary(ty, op) => out.push(op.code(*ty)),
        WasmInstr::Unary(op) => out.push(op.code()),
        WasmInstr::Call(idx) => {
            out.push(0x10);
            uleb(u64::from(*idx), out);
        }
        WasmInstr::Return => out.push(0x0f),
        WasmInstr::Unreachable => out.push(0x00),
        WasmInstr::Block => {
            out.push(0x02);
            out.push(0x40); // 空结果类型
        }
        WasmInstr::Loop => {
            out.push(0x03);
            out.push(0x40);
        }
        WasmInstr::If => {
            out.push(0x04);
            out.push(0x40);
        }
        WasmInstr::Else => out.push(0x05),
        WasmInstr::End => out.push(0x0b),
        WasmInstr::Br(depth) => {
            out.push(0x0c);
            uleb(u64::from(*depth), out);
        }
        WasmInstr::BrTable(targets, default) => {
            out.push(0x0e);
            uleb(targets.len() as u64, out);
            for t in targets {
                uleb(u64::from(*t), out);
            }
            uleb(u64::from(*default), out);
        }
        WasmInstr::Load { op, addr64 } => {
            let (code, align) = mem_opcode(true, *op, *addr64);
            out.push(code);
            uleb(u64::from(align), out);
            uleb(0, out); // offset
        }
        WasmInstr::Store { op, addr64 } => {
            let (code, align) = mem_opcode(false, *op, *addr64);
            out.push(code);
            uleb(u64::from(align), out);
            uleb(0, out); // offset
        }
    }
}

/// Data 段内容：一个偏移 0 的活跃数据段 + 字节串。
fn encode_data_section(data: &[u8], addr64: bool) -> Vec<u8> {
    let mut segment = Vec::new();
    segment.push(0x00); // flags：活跃、内存 0
    if addr64 {
        segment.push(0x42); // i64.const
        sleb(0, &mut segment);
    } else {
        segment.push(0x41); // i32.const
        sleb(0, &mut segment);
    }
    segment.push(0x0b); // end（offset 表达式结束）
    uleb(data.len() as u64, &mut segment);
    segment.extend_from_slice(data);
    encode_vec(&[segment])
}

/// 把字节转成 WAT 字符串字面量转义形式。
fn wat_escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:02x}")),
        }
    }
    out
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
    fn float_unary_opcodes_are_not_shifted() {
        // 这是一处曾经的回归：f32/f64 一元指令表整体错位，
        // 导致 f64.neg 被编成 f32.neg，模块校验直接失败。
        assert_eq!(WasmUnaryOp::F32Abs.code(), 0x8B);
        assert_eq!(WasmUnaryOp::F32Neg.code(), 0x8C);
        assert_eq!(WasmUnaryOp::F32Sqrt.code(), 0x91);
        assert_eq!(WasmUnaryOp::F64Abs.code(), 0x99);
        assert_eq!(WasmUnaryOp::F64Neg.code(), 0x9A);
        assert_eq!(WasmUnaryOp::F64Sqrt.code(), 0x9F);
    }

    #[test]
    fn integer_opcodes_use_i64_forms() {
        assert_eq!(WasmBinaryOp::GeS.code(WasmType::I64), 0x59);
        assert_eq!(WasmBinaryOp::Sub.code(WasmType::I64), 0x7D);
        assert_eq!(WasmBinaryOp::Mul.code(WasmType::I64), 0x7E);
        assert_eq!(WasmBinaryOp::ShrU.code(WasmType::I64), 0x88);
        // 浮点比较/算术用独立操作码，而不是前缀。
        assert_eq!(WasmBinaryOp::FAdd.code(WasmType::F64), 0xA0);
        assert_eq!(WasmBinaryOp::FGe.code(WasmType::F64), 0x66);
    }

    /// 扫描顶层段的 id（跳过 magic/version 与各段的长度前缀）。
    pub fn section_ids(bytes: &[u8]) -> Vec<u8> {
        let mut ids = Vec::new();
        let mut i = 8;
        while i < bytes.len() {
            let id = bytes[i];
            i += 1;
            // 读 uleb128 长度
            let mut len = 0u32;
            let mut shift = 0;
            loop {
                let b = bytes[i];
                i += 1;
                len |= u32::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            ids.push(id);
            i += len as usize;
        }
        ids
    }

    #[test]
    fn wasm64_memory_declares_the_64_bit_flag() {
        // wasm64 用 i64 指针与 i64 数据偏移，memory 段必须带 memory64
        // （flags 第 2 位 0x04），否则模块自相矛盾。
        let mut wasm = minimal_module();
        wasm.data_addr64 = true;
        wasm.data = b"hi\0".to_vec();
        let bytes = wasm.to_binary();
        // 找到 memory 段（id 5）的 flags 字节。
        let mut i = 8;
        let mut flags = None;
        while i < bytes.len() {
            let id = bytes[i];
            i += 1;
            let mut len = 0u32;
            let mut shift = 0;
            loop {
                let b = bytes[i];
                i += 1;
                len |= u32::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            if id == 5 {
                // 段内容：vec(count) 后紧跟 limits.flags
                flags = Some(bytes[i + 1]);
            }
            i += len as usize;
        }
        assert_eq!(flags.expect("memory 段") & 0x04, 0x04);
    }

    #[test]
    fn data_section_is_emitted_when_data_present() {
        let mut wasm = minimal_module();
        wasm.data = b"hello\0".to_vec();
        let bytes = wasm.to_binary();
        let ids = section_ids(&bytes);
        let code = ids.iter().position(|id| *id == 10).expect("code 段");
        let data = ids.iter().position(|id| *id == 11).expect("data 段");
        assert!(data > code, "data 段必须排在 code 段之后");
        assert!(bytes.windows(6).any(|w| w == b"hello\0"));
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
