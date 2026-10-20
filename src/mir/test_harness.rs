//! `#[test]` 测试桩。
//!
//! 语言里目前还没有断言宏（见 `tests/README.md`），所以约定是：
//! 测试函数不带参数，正常跑完返回 `()` 就算通过；返回 `i32` 时
//! 非 0 表示失败——和 `tests/cases/*.zr` 里 `main` 用返回编号指认失败
//! 断言的做法一致。
//!
//! 桩函数在 **MIR 层**合成。放在 MIR 层而不是文本层，是因为再往上就得
//! 把生成的代码重新过一遍类型检查，而 MIR 层已经有全部需要的信息
//! （测试函数名、要调用的函数都已降级完成）。

use crate::mir::NumTy;
use crate::mir::{
    MirBasicBlock, MirBinaryOp, MirFunction, MirInstr, MirModule, MirTerminator, MirValue,
};

/// 生成跑这些测试的 `main`。
///
/// 块布局是一条直线，失败计数靠 `failed` 变量在块间传递：
///
/// ```text
/// bb_entry:  failed = 0            -> bb_t0
/// bb_t0:     r = call t0(); bad = r != 0; failed = failed + bad -> bb_t1
/// bb_t1:     r = call t1(); ...                              -> bb_done
/// bb_done:   return failed
/// ```
///
/// `failed` 在多个块里被赋值，SSA 阶段会自动为它插 φ；因为块是链式的，
/// 每个前驱只有一个，φ 的 incoming 就是紧邻的前驱块，退化成一次拷贝。
pub fn build_harness(test_names: &[String]) -> MirFunction {
    let mut blocks: Vec<MirBasicBlock> = Vec::new();

    // bb_entry: failed = 0
    blocks.push(MirBasicBlock {
        id: 0,
        instrs: vec![MirInstr::Assign("failed".into(), MirValue::Int(0))],
        term: None, // 稍后回填
    });

    // 每个测试一块
    let test_start = blocks.len();
    for (i, name) in test_names.iter().enumerate() {
        let id = test_start + i;
        blocks.push(MirBasicBlock {
            id,
            instrs: vec![
                MirInstr::Call("r".into(), name.clone(), Vec::new(), NumTy::Int(64)),
                MirInstr::Binary(
                    "bad".into(),
                    MirValue::Var("r".into()),
                    MirBinaryOp::Ne,
                    MirValue::Int(0),
                    NumTy::Int(64),
                ),
                MirInstr::Binary(
                    "failed".into(),
                    MirValue::Var("failed".into()),
                    MirBinaryOp::Add,
                    MirValue::Var("bad".into()),
                    NumTy::Int(64),
                ),
            ],
            term: None,
        });
    }

    // bb_done: return failed
    let done = blocks.len();
    blocks.push(MirBasicBlock {
        id: done,
        instrs: Vec::new(),
        term: Some(MirTerminator::Return(Some(MirValue::Var("failed".into())))),
    });

    // 回填跳转：entry -> 第一个测试；最后一个测试 -> done
    for (i, block) in blocks.iter_mut().enumerate() {
        if block.term.is_some() {
            continue;
        }
        let next = if i == 0 {
            test_start
        } else if i + 1 == done {
            done
        } else {
            i + 1
        };
        block.term = Some(MirTerminator::Goto(next));
    }

    MirFunction {
        name: "main".to_string(),
        is_async: false,
        params: Vec::new(),
        param_types: Vec::new(),
        blocks,
        inline_never: false,
        inline_always: false,
        aggregate_ret: None,
        returns_future: false,
    }
}

/// 如果模块里有 `#[test]` 函数，就把合成的测试桩装进 MIR 模块。
///
/// 用户自己写的 `main` 会被移除——`#[test]` 程序里 `main` 只是被生成物，
/// 留着会和桩函数抢同一个符号。
pub fn install(module: &mut MirModule, names: &[String]) {
    if names.is_empty() {
        return;
    }
    module.functions.retain(|f| f.name != "main");
    module.functions.push(build_harness(names));
}
