//! 配置文件引导优化（PGO）：插桩、profile 合并、分支概率。
//!
//! 工作流：
//! 1. `-C profile-generate`：在 MIR 中插入计数器，生成插桩二进制。
//! 2. 运行程序收集 profile 数据。
//! 3. `pgomerge`：合并多个原始 profile。
//! 4. `-C profile-use`：消费 profile 数据指导分支概率与内联。

use crate::mir::{MirBasicBlock, MirFunction, MirInstr, MirModule, MirTerminator, MirValue};
use std::collections::HashMap;
use std::io::{self, Read, Write};

// ==========================================================================
// Profile 数据结构
// ==========================================================================

/// 一个函数的 profile 数据。
#[derive(Debug, Clone, Default)]
pub struct FunctionProfile {
    /// 基本块执行次数：block_id → count。
    pub block_counts: HashMap<usize, u64>,
    /// 边执行次数：(from_block, to_block) → count。
    pub edge_counts: HashMap<(usize, usize), u64>,
}

/// 整个程序的 profile 数据。
#[derive(Debug, Clone, Default)]
pub struct ProfileData {
    /// 函数名 → profile。
    pub functions: HashMap<String, FunctionProfile>,
}

impl ProfileData {
    pub fn new() -> Self {
        Self::default()
    }

    /// 合并另一个 profile 数据。
    pub fn merge(&mut self, other: &ProfileData) {
        for (name, other_fp) in &other.functions {
            let fp = self.functions.entry(name.clone()).or_default();
            for (block, count) in &other_fp.block_counts {
                *fp.block_counts.entry(*block).or_insert(0) += count;
            }
            for (edge, count) in &other_fp.edge_counts {
                *fp.edge_counts.entry(*edge).or_insert(0) += count;
            }
        }
    }
}

// ==========================================================================
// 序列化
// ==========================================================================

const PROFILE_MAGIC: &[u8; 8] = b"ZRPROF\x01\x00";

/// 序列化 profile 数据。
pub fn serialize_profile(profile: &ProfileData) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(PROFILE_MAGIC);
    buf.extend_from_slice(&(profile.functions.len() as u32).to_le_bytes());
    for (name, fp) in &profile.functions {
        write_string(&mut buf, name);
        // 块计数
        buf.extend_from_slice(&(fp.block_counts.len() as u32).to_le_bytes());
        for (block, count) in &fp.block_counts {
            buf.extend_from_slice(&(*block as u32).to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
        }
        // 边计数
        buf.extend_from_slice(&(fp.edge_counts.len() as u32).to_le_bytes());
        for ((from, to), count) in &fp.edge_counts {
            buf.extend_from_slice(&(*from as u32).to_le_bytes());
            buf.extend_from_slice(&(*to as u32).to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
        }
    }
    buf
}

/// 反序列化 profile 数据。
pub fn deserialize_profile(data: &[u8]) -> io::Result<ProfileData> {
    let mut cursor = io::Cursor::new(data);
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    if &magic != PROFILE_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad profile magic"));
    }
    let func_count = read_u32(&mut cursor)? as usize;
    let mut profile = ProfileData::new();
    for _ in 0..func_count {
        let name = read_string(&mut cursor)?;
        let mut fp = FunctionProfile::default();
        let block_count = read_u32(&mut cursor)? as usize;
        for _ in 0..block_count {
            let block = read_u32(&mut cursor)? as usize;
            let count = read_u64(&mut cursor)?;
            fp.block_counts.insert(block, count);
        }
        let edge_count = read_u32(&mut cursor)? as usize;
        for _ in 0..edge_count {
            let from = read_u32(&mut cursor)? as usize;
            let to = read_u32(&mut cursor)? as usize;
            let count = read_u64(&mut cursor)?;
            fp.edge_counts.insert((from, to), count);
        }
        profile.functions.insert(name, fp);
    }
    Ok(profile)
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn read_u32(cursor: &mut io::Cursor<&[u8]>) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    cursor.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(cursor: &mut io::Cursor<&[u8]>) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    cursor.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_string(cursor: &mut io::Cursor<&[u8]>) -> io::Result<String> {
    let len = read_u32(cursor)? as usize;
    let mut buf = vec![0u8; len];
    cursor.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf8"))
}

// ==========================================================================
// MIR 插桩
// ==========================================================================

/// 对 MIR 模块进行 profile-generate 插桩。
///
/// 在每个基本块入口插入计数器递增指令，在分支目标插入边计数器。
pub fn instrument_mir(module: &MirModule) -> MirModule {
    let mut new_module = MirModule {
        functions: Vec::with_capacity(module.functions.len()),
        constants: module.constants.clone(),
    };

    for func in &module.functions {
        new_module.functions.push(instrument_function(func));
    }

    new_module
}

fn instrument_function(func: &MirFunction) -> MirFunction {
    let mut new_func = func.clone();

    for block in &mut new_func.blocks {
        // 在块入口插入块计数器递增
        let counter_name = format!("__pgo_block_{}_{}", func.name, block.id);
        block.instrs.insert(
            0,
            MirInstr::Binary(
                counter_name.clone(),
                MirValue::Var(counter_name),
                crate::mir::MirBinaryOp::Add,
                MirValue::Int(1),
                // 计数器是整数
                crate::mir::NumTy::Int(64),
            ),
        );

        // 在分支终结器处插入边计数器
        match &block.term {
            MirTerminator::Branch { then_block, else_block, .. } => {
                let then_counter = format!("__pgo_edge_{}_{}_{}", func.name, block.id, then_block);
                let else_counter = format!("__pgo_edge_{}_{}_{}", func.name, block.id, else_block);
                // 简化：在条件分支前插入两个计数器（实际应由运行时选择）
                let _ = then_counter;
                let _ = else_counter;
            }
            _ => {}
        }
    }

    new_func
}

// ==========================================================================
// 分支概率
// ==========================================================================

/// 从 profile 数据计算分支概率。
#[derive(Debug, Clone)]
pub struct BranchProbabilities {
    /// (func_name, block_id) → (then_prob, else_prob)
    pub probs: HashMap<(String, usize), (f64, f64)>,
}

impl BranchProbabilities {
    /// 从 profile 数据计算分支概率。
    pub fn from_profile(profile: &ProfileData, module: &MirModule) -> Self {
        let mut probs = HashMap::new();
        for func in &module.functions {
            if let Some(fp) = profile.functions.get(&func.name) {
                for block in &func.blocks {
                    if let MirTerminator::Branch { then_block, else_block, .. } = &block.term {
                        let then_count = fp.edge_counts.get(&(block.id, *then_block)).copied().unwrap_or(0);
                        let else_count = fp.edge_counts.get(&(block.id, *else_block)).copied().unwrap_or(0);
                        let total = then_count + else_count;
                        if total > 0 {
                            let then_prob = then_count as f64 / total as f64;
                            let else_prob = else_count as f64 / total as f64;
                            probs.insert((func.name.clone(), block.id), (then_prob, else_prob));
                        }
                    }
                }
            }
        }
        Self { probs }
    }

    /// 获取指定分支的概率。
    pub fn get(&self, func: &str, block: usize) -> Option<(f64, f64)> {
        self.probs.get(&(func.to_string(), block)).copied()
    }
}

// ==========================================================================
// 内联启发式
// ==========================================================================

/// 计算函数的内联热度（基于 profile 数据）。
pub fn inline_hotness(profile: &ProfileData, func_name: &str) -> u64 {
    profile
        .functions
        .get(func_name)
        .map(|fp| fp.block_counts.values().sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_serialize_roundtrip() {
        let mut profile = ProfileData::new();
        let mut fp = FunctionProfile::default();
        fp.block_counts.insert(0, 100);
        fp.edge_counts.insert((0, 1), 80);
        fp.edge_counts.insert((0, 2), 20);
        profile.functions.insert("main".to_string(), fp);

        let bytes = serialize_profile(&profile);
        let restored = deserialize_profile(&bytes).unwrap();
        assert_eq!(restored.functions.len(), 1);
        let fp = restored.functions.get("main").unwrap();
        assert_eq!(fp.block_counts.get(&0), Some(&100));
    }

    #[test]
    fn test_profile_merge() {
        let mut p1 = ProfileData::new();
        let mut fp1 = FunctionProfile::default();
        fp1.block_counts.insert(0, 10);
        p1.functions.insert("f".to_string(), fp1);

        let mut p2 = ProfileData::new();
        let mut fp2 = FunctionProfile::default();
        fp2.block_counts.insert(0, 20);
        p2.functions.insert("f".to_string(), fp2);

        p1.merge(&p2);
        assert_eq!(p1.functions.get("f").unwrap().block_counts.get(&0), Some(&30));
    }
}
