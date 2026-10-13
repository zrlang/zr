//! 栈帧布局（Stack Frame Layout）。
//!
//! 负责：把局部变量、溢出槽、调用参数归位到栈上，
//! 计算 RBP/RSP 偏移，生成 prologue/epilogue 的栈调整指令。

/// 栈槽种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// 局部变量。
    Local,
    /// 寄存器溢出槽。
    Spill,
    /// 调用参数（超出寄存器数量时入栈）。
    Argument,
    /// 保存的 callee-save 寄存器。
    CalleeSave,
}

/// 栈帧描述。
#[derive(Debug, Clone, Default)]
pub struct StackFrame {
    /// 栈槽列表。
    pub slots: Vec<StackSlot>,
    /// 帧大小（字节，向下增长）。
    pub size: usize,
    /// 对齐要求（字节，默认 16）。
    pub align: usize,
}

/// 单个栈槽。
#[derive(Debug, Clone)]
pub struct StackSlot {
    pub kind: SlotKind,
    pub offset: i64, // 相对于 RBP（负数 = 栈内）
    pub size: usize,
    pub align: usize,
}

impl StackFrame {
    /// 创建空栈帧。
    pub fn new() -> Self {
        Self {
            align: 16,
            ..Default::default()
        }
    }

    /// 分配一个栈槽，返回其偏移。
    pub fn alloc(&mut self, kind: SlotKind, size: usize, align: usize) -> i64 {
        // 向下对齐
        let pad = (self.size) % align;
        if pad != 0 {
            self.size += align - pad;
        }
        self.size += size;
        let offset = -(self.size as i64);
        self.slots.push(StackSlot {
            kind,
            offset,
            size,
            align,
        });
        offset
    }

    /// 按对齐要求修整帧大小（保证 RSP 对齐）。
    pub fn finalize(&mut self) {
        let pad = self.size % self.align;
        if pad != 0 {
            self.size += self.align - pad;
        }
    }
}

/// 生成函数序言（prologue）的字节码。
/// `push rbp; mov rbp, rsp; sub rsp, frame_size`
pub fn emit_prologue(frame: &StackFrame) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&[0x55]); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xe5]); // mov rbp, rsp
    if frame.size > 0 {
        // sub rsp, imm32
        code.extend_from_slice(&[0x48, 0x81, 0xec]);
        code.extend_from_slice(&(frame.size as u32).to_le_bytes());
    }
    code
}

/// 生成函数尾声（epilogue）的字节码。
/// `mov rsp, rbp; pop rbp; ret`
pub fn emit_epilogue(_frame: &StackFrame) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&[0x48, 0x89, 0xec]); // mov rsp, rbp
    code.extend_from_slice(&[0x5d]); // pop rbp
    code.extend_from_slice(&[0xc3]); // ret
    code
}
