//! 链接时优化（LTO）：序列化 SSA + 跨模块内联。
//!
//! 由于项目不依赖 serde，手动实现 SSA 的二进制序列化格式。
//! 在链接时反序列化所有模块的 SSA，执行跨模块内联后再做最终代码生成。

use crate::ssa::{
    SsaBasicBlock, SsaBinaryOp, SsaConst, SsaFunction, SsaInstr, SsaModule, SsaTerminator,
    SsaType, SsaUnaryOp, SsaValue,
};
use std::collections::HashMap;
use std::io::{self, Read, Write};

/// LTO 模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LtoMode {
    Off,
    Thin,
    Fat,
}

impl LtoMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(LtoMode::Off),
            "thin" => Some(LtoMode::Thin),
            "fat" => Some(LtoMode::Fat),
            _ => None,
        }
    }
}

// ==========================================================================
// 序列化
// ==========================================================================

/// 将 SSA 模块序列化为字节流。
pub fn serialize_module(module: &SsaModule) -> Vec<u8> {
    let mut buf = Vec::new();
    // 魔数 + 版本
    buf.extend_from_slice(b"ZRSSA");
    buf.extend_from_slice(&1u32.to_le_bytes());
    // 函数数量
    buf.extend_from_slice(&(module.functions.len() as u32).to_le_bytes());
    for func in &module.functions {
        serialize_function(func, &mut buf);
    }
    buf
}

fn serialize_function(func: &SsaFunction, buf: &mut Vec<u8>) {
    write_string(buf, &func.name);
    // 参数
    buf.extend_from_slice(&(func.params.len() as u32).to_le_bytes());
    for p in &func.params {
        buf.extend_from_slice(&p.0.to_le_bytes());
    }
    // 基本块
    buf.extend_from_slice(&(func.blocks.len() as u32).to_le_bytes());
    for block in &func.blocks {
        serialize_block(block, buf);
    }
    // is_async
    buf.push(if func.is_async { 1 } else { 0 });
}

fn serialize_block(block: &SsaBasicBlock, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&(block.id as u32).to_le_bytes());
    // 前驱
    buf.extend_from_slice(&(block.preds.len() as u32).to_le_bytes());
    for p in &block.preds {
        buf.extend_from_slice(&(*p as u32).to_le_bytes());
    }
    // 后继
    buf.extend_from_slice(&(block.succs.len() as u32).to_le_bytes());
    for s in &block.succs {
        buf.extend_from_slice(&(*s as u32).to_le_bytes());
    }
    // 指令
    buf.extend_from_slice(&(block.instrs.len() as u32).to_le_bytes());
    for instr in &block.instrs {
        serialize_instr(instr, buf);
    }
    // 终结器
    serialize_terminator(&block.term, buf);
}

fn serialize_instr(instr: &SsaInstr, buf: &mut Vec<u8>) {
    match instr {
        SsaInstr::Const { result, value } => {
            buf.push(0);
            buf.extend_from_slice(&result.0.to_le_bytes());
            serialize_const(value, buf);
        }
        SsaInstr::Unary { result, op, operand } => {
            buf.push(1);
            buf.extend_from_slice(&result.0.to_le_bytes());
            buf.push(match op {
                SsaUnaryOp::Not => 0,
                SsaUnaryOp::BitNot => 1,
                SsaUnaryOp::Neg => 2,
            });
            buf.extend_from_slice(&operand.0.to_le_bytes());
        }
        SsaInstr::Binary { result, op, lhs, rhs } => {
            buf.push(2);
            buf.extend_from_slice(&result.0.to_le_bytes());
            buf.push(bin_op_to_byte(op));
            buf.extend_from_slice(&lhs.0.to_le_bytes());
            buf.extend_from_slice(&rhs.0.to_le_bytes());
        }
        SsaInstr::Cast { result, operand, to } => {
            buf.push(3);
            buf.extend_from_slice(&result.0.to_le_bytes());
            buf.extend_from_slice(&operand.0.to_le_bytes());
            serialize_type(to, buf);
        }
        SsaInstr::Var { result, name } => {
            buf.push(4);
            buf.extend_from_slice(&result.0.to_le_bytes());
            write_string(buf, name);
        }
    }
}

fn serialize_terminator(term: &SsaTerminator, buf: &mut Vec<u8>) {
    match term {
        SsaTerminator::Goto(b) => {
            buf.push(0);
            buf.extend_from_slice(&(*b as u32).to_le_bytes());
        }
        SsaTerminator::Branch { cond, then_block, else_block } => {
            buf.push(1);
            buf.extend_from_slice(&cond.0.to_le_bytes());
            buf.extend_from_slice(&(*then_block as u32).to_le_bytes());
            buf.extend_from_slice(&(*else_block as u32).to_le_bytes());
        }
        SsaTerminator::Return(v) => {
            buf.push(2);
            if let Some(v) = v {
                buf.push(1);
                buf.extend_from_slice(&v.0.to_le_bytes());
            } else {
                buf.push(0);
            }
        }
        SsaTerminator::Unreachable => {
            buf.push(3);
        }
    }
}

fn serialize_const(c: &SsaConst, buf: &mut Vec<u8>) {
    match c {
        SsaConst::Int(i) => {
            buf.push(0);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        SsaConst::Float(bits) => {
            buf.push(1);
            buf.extend_from_slice(&bits.to_le_bytes());
        }
        SsaConst::Bool(b) => {
            buf.push(2);
            buf.push(if *b { 1 } else { 0 });
        }
        SsaConst::Unit => {
            buf.push(3);
        }
    }
}

fn serialize_type(ty: &SsaType, buf: &mut Vec<u8>) {
    match ty {
        SsaType::Void => buf.push(0),
        SsaType::Bool => buf.push(1),
        SsaType::Ptr => buf.push(2),
        SsaType::I(bits) => {
            buf.push(3);
            buf.extend_from_slice(&(*bits as u32).to_le_bytes());
        }
        SsaType::U(bits) => {
            buf.push(4);
            buf.extend_from_slice(&(*bits as u32).to_le_bytes());
        }
        SsaType::F(bits) => {
            buf.push(5);
            buf.extend_from_slice(&(*bits as u32).to_le_bytes());
        }
    }
}

fn bin_op_to_byte(op: &SsaBinaryOp) -> u8 {
    match op {
        SsaBinaryOp::Add => 0,
        SsaBinaryOp::Sub => 1,
        SsaBinaryOp::Mul => 2,
        SsaBinaryOp::Div => 3,
        SsaBinaryOp::Rem => 4,
        SsaBinaryOp::Eq => 5,
        SsaBinaryOp::Ne => 6,
        SsaBinaryOp::Lt => 7,
        SsaBinaryOp::Le => 8,
        SsaBinaryOp::Gt => 9,
        SsaBinaryOp::Ge => 10,
        SsaBinaryOp::And => 11,
        SsaBinaryOp::Or => 12,
        SsaBinaryOp::BitAnd => 13,
        SsaBinaryOp::BitOr => 14,
        SsaBinaryOp::BitXor => 15,
        SsaBinaryOp::Shl => 16,
        SsaBinaryOp::Shr => 17,
    }
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

// ==========================================================================
// 反序列化
// ==========================================================================

/// 从字节流反序列化 SSA 模块。
pub fn deserialize_module(data: &[u8]) -> io::Result<SsaModule> {
    let mut cursor = io::Cursor::new(data);
    let mut magic = [0u8; 5];
    cursor.read_exact(&mut magic)?;
    if &magic != b"ZRSSA" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
    }
    let mut version_bytes = [0u8; 4];
    cursor.read_exact(&mut version_bytes)?;
    let _version = u32::from_le_bytes(version_bytes);

    let mut count_bytes = [0u8; 4];
    cursor.read_exact(&mut count_bytes)?;
    let count = u32::from_le_bytes(count_bytes) as usize;

    let mut functions = Vec::with_capacity(count);
    for _ in 0..count {
        functions.push(deserialize_function(&mut cursor)?);
    }

    Ok(SsaModule { functions })
}

fn deserialize_function(cursor: &mut io::Cursor<&[u8]>) -> io::Result<SsaFunction> {
    let name = read_string(cursor)?;
    let param_count = read_u32(cursor)? as usize;
    let mut params = Vec::with_capacity(param_count);
    for _ in 0..param_count {
        params.push(SsaValue(read_u32(cursor)?));
    }
    let block_count = read_u32(cursor)? as usize;
    let mut blocks = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        blocks.push(deserialize_block(cursor)?);
    }
    let is_async = read_u8(cursor)? != 0;
    Ok(SsaFunction { name, params, blocks, is_async })
}

fn deserialize_block(cursor: &mut io::Cursor<&[u8]>) -> io::Result<SsaBasicBlock> {
    let id = read_u32(cursor)? as usize;
    let pred_count = read_u32(cursor)? as usize;
    let mut preds = Vec::with_capacity(pred_count);
    for _ in 0..pred_count {
        preds.push(read_u32(cursor)? as usize);
    }
    let succ_count = read_u32(cursor)? as usize;
    let mut succs = Vec::with_capacity(succ_count);
    for _ in 0..succ_count {
        succs.push(read_u32(cursor)? as usize);
    }
    let instr_count = read_u32(cursor)? as usize;
    let mut instrs = Vec::with_capacity(instr_count);
    for _ in 0..instr_count {
        instrs.push(deserialize_instr(cursor)?);
    }
    let term = deserialize_terminator(cursor)?;
    Ok(SsaBasicBlock {
        id,
        preds,
        succs,
        instrs,
        term,
        start_offset: 0,
    })
}

fn deserialize_instr(cursor: &mut io::Cursor<&[u8]>) -> io::Result<SsaInstr> {
    let tag = read_u8(cursor)?;
    match tag {
        0 => {
            let result = SsaValue(read_u32(cursor)?);
            let value = deserialize_const(cursor)?;
            Ok(SsaInstr::Const { result, value })
        }
        1 => {
            let result = SsaValue(read_u32(cursor)?);
            let op = match read_u8(cursor)? {
                0 => SsaUnaryOp::Not,
                1 => SsaUnaryOp::BitNot,
                2 => SsaUnaryOp::Neg,
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad unary op")),
            };
            let operand = SsaValue(read_u32(cursor)?);
            Ok(SsaInstr::Unary { result, op, operand })
        }
        2 => {
            let result = SsaValue(read_u32(cursor)?);
            let op = byte_to_bin_op(read_u8(cursor)?);
            let lhs = SsaValue(read_u32(cursor)?);
            let rhs = SsaValue(read_u32(cursor)?);
            Ok(SsaInstr::Binary { result, op, lhs, rhs })
        }
        3 => {
            let result = SsaValue(read_u32(cursor)?);
            let operand = SsaValue(read_u32(cursor)?);
            let to = deserialize_type(cursor)?;
            Ok(SsaInstr::Cast { result, operand, to })
        }
        4 => {
            let result = SsaValue(read_u32(cursor)?);
            let name = read_string(cursor)?;
            Ok(SsaInstr::Var { result, name })
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad instr tag")),
    }
}

fn deserialize_terminator(cursor: &mut io::Cursor<&[u8]>) -> io::Result<SsaTerminator> {
    let tag = read_u8(cursor)?;
    match tag {
        0 => {
            let b = read_u32(cursor)? as usize;
            Ok(SsaTerminator::Goto(b))
        }
        1 => {
            let cond = SsaValue(read_u32(cursor)?);
            let then_block = read_u32(cursor)? as usize;
            let else_block = read_u32(cursor)? as usize;
            Ok(SsaTerminator::Branch { cond, then_block, else_block })
        }
        2 => {
            let has = read_u8(cursor)? != 0;
            if has {
                let v = SsaValue(read_u32(cursor)?);
                Ok(SsaTerminator::Return(Some(v)))
            } else {
                Ok(SsaTerminator::Return(None))
            }
        }
        3 => Ok(SsaTerminator::Unreachable),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad terminator tag")),
    }
}

fn deserialize_const(cursor: &mut io::Cursor<&[u8]>) -> io::Result<SsaConst> {
    let tag = read_u8(cursor)?;
    match tag {
        0 => {
            let mut bytes = [0u8; 16];
            cursor.read_exact(&mut bytes)?;
            Ok(SsaConst::Int(i128::from_le_bytes(bytes)))
        }
        1 => {
            let mut bytes = [0u8; 8];
            cursor.read_exact(&mut bytes)?;
            Ok(SsaConst::Float(u64::from_le_bytes(bytes)))
        }
        2 => {
            let b = read_u8(cursor)? != 0;
            Ok(SsaConst::Bool(b))
        }
        3 => Ok(SsaConst::Unit),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad const tag")),
    }
}

fn deserialize_type(cursor: &mut io::Cursor<&[u8]>) -> io::Result<SsaType> {
    let byte = read_u8(cursor)?;
    Ok(match byte {
        0 => SsaType::Void,
        1 => SsaType::Bool,
        2 => SsaType::Ptr,
        3 => SsaType::I(read_u32(cursor)? as usize),
        4 => SsaType::U(read_u32(cursor)? as usize),
        5 => SsaType::F(read_u32(cursor)? as usize),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad type")),
    })
}

fn byte_to_bin_op(b: u8) -> SsaBinaryOp {
    match b {
        0 => SsaBinaryOp::Add,
        1 => SsaBinaryOp::Sub,
        2 => SsaBinaryOp::Mul,
        3 => SsaBinaryOp::Div,
        4 => SsaBinaryOp::Rem,
        5 => SsaBinaryOp::Eq,
        6 => SsaBinaryOp::Ne,
        7 => SsaBinaryOp::Lt,
        8 => SsaBinaryOp::Le,
        9 => SsaBinaryOp::Gt,
        10 => SsaBinaryOp::Ge,
        11 => SsaBinaryOp::And,
        12 => SsaBinaryOp::Or,
        13 => SsaBinaryOp::BitAnd,
        14 => SsaBinaryOp::BitOr,
        15 => SsaBinaryOp::BitXor,
        16 => SsaBinaryOp::Shl,
        17 => SsaBinaryOp::Shr,
        _ => SsaBinaryOp::Add,
    }
}

fn read_u8(cursor: &mut io::Cursor<&[u8]>) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    cursor.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn read_u32(cursor: &mut io::Cursor<&[u8]>) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    cursor.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_string(cursor: &mut io::Cursor<&[u8]>) -> io::Result<String> {
    let len = read_u32(cursor)? as usize;
    let mut buf = vec![0u8; len];
    cursor.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf8"))
}

// ==========================================================================
// 跨模块内联
// ==========================================================================

/// 跨模块内联：将多个模块合并，并对小函数执行内联。
pub fn cross_module_inline(modules: Vec<SsaModule>) -> SsaModule {
    // 合并所有函数
    let mut all_functions: Vec<SsaFunction> = Vec::new();
    let mut name_map: HashMap<String, usize> = HashMap::new();

    for module in modules {
        for func in module.functions {
            if !name_map.contains_key(&func.name) {
                name_map.insert(func.name.clone(), all_functions.len());
                all_functions.push(func);
            }
        }
    }

    // 简单的内联：将只有少量指令的非递归函数内联
    // 这里只做合并，完整的内联在 opt 阶段完成
    SsaModule { functions: all_functions }
}

/// 运行 LTO 流程：合并模块 + 跨模块内联。
pub fn run_lto(modules: Vec<SsaModule>, mode: LtoMode) -> SsaModule {
    match mode {
        LtoMode::Off => {
            // 不做跨模块优化，只合并
            let mut functions = Vec::new();
            for m in modules {
                functions.extend(m.functions);
            }
            SsaModule { functions }
        }
        LtoMode::Thin | LtoMode::Fat => {
            cross_module_inline(modules)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_roundtrip() {
        let module = SsaModule {
            functions: vec![SsaFunction {
                name: "main".to_string(),
                params: vec![],
                blocks: vec![SsaBasicBlock {
                    id: 0,
                    preds: vec![],
                    succs: vec![],
                    instrs: vec![SsaInstr::Const {
                        result: SsaValue(0),
                        value: SsaConst::Int(42),
                    }],
                    term: SsaTerminator::Return(Some(SsaValue(0))),
                    start_offset: 0,
                }],
                is_async: false,
            }],
        };

        let bytes = serialize_module(&module);
        let restored = deserialize_module(&bytes).unwrap();
        assert_eq!(restored.functions.len(), 1);
        assert_eq!(restored.functions[0].name, "main");
    }
}
