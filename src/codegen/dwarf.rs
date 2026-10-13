//! DWARF 调试信息生成（DWARFv5 简化版）。
//!
//! 生成 `.debug_info`、`.debug_abbrev`、`.debug_str`、`.debug_line` 段。

use std::collections::HashMap;

/// DWARF 标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DwTag {
    CompileUnit = 0x11,
    Subprogram = 0x2e,
    Variable = 0x34,
    BaseType = 0x24,
    FormalParameter = 0x05,
}

/// DWARF 属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DwAt {
    Name = 0x03,
    LowPc = 0x11,
    HighPc = 0x12,
    StmtList = 0x10,
    Type = 0x49,
    External = 0x3f,
    DeclFile = 0x3a,
    DeclLine = 0x3b,
    Language = 0x13,
}

/// DWARF 形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DwForm {
    Addr = 0x01,
    Data2 = 0x05,
    Data4 = 0x06,
    Data8 = 0x07,
    String = 0x08,
    Strp = 0x0e,
    Flag = 0x0c,
    SecOffset = 0x17,
}

/// 一个 DIE（调试信息条目）。
#[derive(Debug, Clone)]
pub struct Die {
    pub tag: DwTag,
    pub attrs: Vec<(DwAt, DwValue)>,
    pub children: Vec<Die>,
}

/// DWARF 属性值。
#[derive(Debug, Clone)]
pub enum DwValue {
    Addr(u64),
    Data2(u16),
    Data4(u32),
    Data8(u64),
    String(String),
    Strp(String),
    Flag(bool),
    SecOffset(u64),
}

impl Die {
    pub fn new(tag: DwTag) -> Self {
        Die { tag, attrs: Vec::new(), children: Vec::new() }
    }

    pub fn attr(mut self, at: DwAt, value: DwValue) -> Self {
        self.attrs.push((at, value));
        self
    }

    pub fn child(mut self, child: Die) -> Self {
        self.children.push(child);
        self
    }
}

/// DWARF 单元。
#[derive(Debug, Default)]
pub struct DwarfUnit {
    pub abbrevs: Vec<Abbrev>,
    pub strings: Vec<String>,
    pub string_offsets: HashMap<String, u32>,
    pub line_programs: Vec<LineProgram>,
    pub root: Option<Die>,
}

/// 缩写定义。
#[derive(Debug, Clone)]
pub struct Abbrev {
    pub code: u32,
    pub tag: DwTag,
    pub has_children: bool,
    pub attrs: Vec<(DwAt, DwForm)>,
}

/// 行号程序。
#[derive(Debug, Clone)]
pub struct LineProgram {
    pub file: String,
    pub lines: Vec<LineEntry>,
}

#[derive(Debug, Clone)]
pub struct LineEntry {
    pub address: u64,
    pub line: u32,
    pub column: u32,
}

impl DwarfUnit {
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加字符串到 `.debug_str` 表，返回偏移。
    pub fn add_string(&mut self, s: &str) -> u32 {
        if let Some(&off) = self.string_offsets.get(s) {
            return off;
        }
        let off = self.strings.iter().map(|s| s.len() as u32 + 1).sum();
        self.strings.push(s.to_string());
        self.string_offsets.insert(s.to_string(), off);
        off
    }

    /// 编码 `.debug_str` 段。
    pub fn encode_debug_str(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for s in &self.strings {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        out
    }

    /// 编码 `.debug_abbrev` 段。
    pub fn encode_debug_abbrev(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for abbrev in &self.abbrevs {
            out.extend(encode_uleb128(abbrev.code));
            out.extend(encode_uleb128(abbrev.tag as u32));
            out.push(if abbrev.has_children { 1 } else { 0 });
            for (at, form) in &abbrev.attrs {
                out.extend(encode_uleb128(*at as u32));
                out.extend(encode_uleb128(*form as u32));
            }
            // 属性列表终止
            out.push(0);
            out.push(0);
        }
        // 缩写表终止
        out.push(0);
        out
    }

    /// 编码 `.debug_info` 段。
    pub fn encode_debug_info(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let root = self.root.clone();
        if let Some(root) = root {
            // 编译单元头
            let unit_length_pos = out.len();
            out.extend_from_slice(&[0u8; 4]); // unit_length (占位)
            out.extend_from_slice(&5u16.to_le_bytes()); // version 5
            out.push(0); // unit_type = compile_unit
            out.push(8); // address_size = 8
            out.extend_from_slice(&0u32.to_le_bytes()); // debug_abbrev_offset
            // DIE 树
            self.encode_die(&root, &mut out);
            // 回填 unit_length
            let length = (out.len() - unit_length_pos - 4) as u32;
            out[unit_length_pos..unit_length_pos + 4].copy_from_slice(&length.to_le_bytes());
        }
        out
    }

    fn encode_die(&mut self, die: &Die, out: &mut Vec<u8>) {
        // 查找或创建缩写
        let abbrev_code = self.get_or_create_abbrev(die);
        out.extend(encode_uleb128(abbrev_code));
        for (_, value) in &die.attrs {
            self.encode_value(value, out);
        }
        if !die.children.is_empty() {
            for child in &die.children {
                self.encode_die(child, out);
            }
            out.push(0); // 子节点列表终止
        }
    }

    fn get_or_create_abbrev(&mut self, die: &Die) -> u32 {
        let attrs: Vec<(DwAt, DwForm)> = die
            .attrs
            .iter()
            .map(|(at, v)| (*at, v.form()))
            .collect();
        if let Some(a) = self.abbrevs.iter().find(|a| {
            a.tag == die.tag
                && a.has_children == !die.children.is_empty()
                && a.attrs == attrs
        }) {
            return a.code;
        }
        let code = (self.abbrevs.len() as u32) + 1;
        self.abbrevs.push(Abbrev {
            code,
            tag: die.tag,
            has_children: !die.children.is_empty(),
            attrs,
        });
        code
    }

    fn encode_value(&mut self, value: &DwValue, out: &mut Vec<u8>) {
        match value {
            DwValue::Addr(a) => out.extend_from_slice(&a.to_le_bytes()),
            DwValue::Data2(v) => out.extend_from_slice(&v.to_le_bytes()),
            DwValue::Data4(v) => out.extend_from_slice(&v.to_le_bytes()),
            DwValue::Data8(v) => out.extend_from_slice(&v.to_le_bytes()),
            DwValue::String(s) => {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            DwValue::Strp(s) => {
                let off = self.add_string(s);
                out.extend_from_slice(&off.to_le_bytes());
            }
            DwValue::Flag(b) => out.push(if *b { 1 } else { 0 }),
            DwValue::SecOffset(o) => out.extend_from_slice(&o.to_le_bytes()),
        }
    }
}

impl DwValue {
    pub fn form(&self) -> DwForm {
        match self {
            DwValue::Addr(_) => DwForm::Addr,
            DwValue::Data2(_) => DwForm::Data2,
            DwValue::Data4(_) => DwForm::Data4,
            DwValue::Data8(_) => DwForm::Data8,
            DwValue::String(_) => DwForm::String,
            DwValue::Strp(_) => DwForm::Strp,
            DwValue::Flag(_) => DwForm::Flag,
            DwValue::SecOffset(_) => DwForm::SecOffset,
        }
    }
}

/// ULEB128 编码。
pub fn encode_uleb128(mut value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}
