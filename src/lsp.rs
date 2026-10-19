//==============================================================================
// lsp.rs — 语言服务器（LSP）
//
// 在 stdio 上讲 JSON-RPC 2.0，实现编辑器和 IDE 真正会用到的那几个能力：
//
//   initialize / initialized / shutdown / exit
//   textDocument/didOpen / didChange / didClose   -> 诊断
//   textDocument/documentSymbol                  -> 文档符号
//   textDocument/definition                       -> 跳转到定义
//   textDocument/hover                            -> 悬停信息
//   textDocument/formatting                       -> 格式化（复用 zr fmt）
//
// 诊断只做到**语法**级别：这个编译器目前的类型检查需要完整的模块环境，
// 在 LSP 里对单个文件跑类型检查会把 `mod`/`use` 引不到的符号全报一遍，
// 满屏假错误反而没法用。语义诊断等模块系统到位后再接。
//
// 符号从**记号流**里扫出来，而不是 AST：AST 目前不保留标识符的名字 span，
// 为了符号视图去改一大片 AST 不划算；记号流本来就有精确的行列，顺带还能
// 容忍尚未支持的语法。
//==============================================================================

use std::collections::HashMap;
use std::io::{Read, Write};

use crate::json::Json;
use crate::lexer::{Lexer, TokenKind};

/// 协议版本。
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 一个符号的定义位置。
#[derive(Debug, Clone)]
struct Symbol {
    name: String,
    /// 0=函数 12=常量 5=类(struct) 10=枚举 23=结构体成员
    kind: i64,
    /// 名字所在的行（0 基）与列（0 基）。
    line: u32,
    character: u32,
    /// 整条定义的结束行（0 基），用于选中范围。
    end_line: u32,
}

/// 服务器状态。
pub struct Server {
    /// 已打开的文档：`uri -> 文本`。
    docs: HashMap<String, String>,
    /// 客户端是否已经收到过 `initialize`。
    initialized: bool,
    /// 收到 `exit` 后置位。
    should_exit: bool,
}

impl Server {
    pub fn new() -> Server {
        Server {
            docs: HashMap::new(),
            initialized: false,
            should_exit: false,
        }
    }

    /// 主循环：从 stdin 读 LSP 帧，处理并写回 stdout。
    pub fn run(&mut self) {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            match read_message(&mut reader) {
                Ok(Some(text)) => {
                    let reply = self.handle_raw(&text);
                    if let Some(r) = reply {
                        if write_message(&r).is_err() {
                            break;
                        }
                    }
                }
                // 连接关闭
                Ok(None) => break,
                Err(_) => break,
            }
            if self.should_exit {
                break;
            }
        }
    }

    /// 处理一条消息；通知返回 `None`，请求返回响应。
    fn handle_raw(&mut self, text: &str) -> Option<Json> {
        let msg = match Json::parse(text) {
            Ok(m) => m,
            Err(e) => {
                // 解析不了就无法拿到 id，只能当成错误响应
                return Some(error_response(&Json::Null, -32700, &e));
            }
        };
        let method = msg
            .get("method")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Json::Null);

        // 请求（带 id）需要回响应，通知（无 id）不需要
        let reply = match method.as_str() {
            "initialize" => Some(self.on_initialize(&msg)),
            "initialized" => None,
            "shutdown" => {
                self.initialized = false;
                id.map(|i| result_response(i, Json::Null))
            }
            "exit" => {
                self.should_exit = true;
                None
            }
            "textDocument/didOpen" => {
                self.on_did_open(&params);
                None
            }
            "textDocument/didChange" => {
                self.on_did_change(&params);
                None
            }
            "textDocument/didClose" => {
                if let Some(uri) = doc_uri(&params) {
                    self.docs.remove(&uri);
                }
                None
            }
            "textDocument/documentSymbol" => {
                id.map(|i| result_response(i, self.on_document_symbol(&params)))
            }
            "textDocument/definition" => {
                id.map(|i| result_response(i, self.on_definition(&params)))
            }
            "textDocument/hover" => id.map(|i| result_response(i, self.on_hover(&params))),
            "textDocument/formatting" => {
                id.map(|i| result_response(i, self.on_formatting(&params)))
            }
            "textDocument/completion" => {
                id.map(|i| result_response(i, self.on_completion(&params)))
            }
            // 未知方法：按 LSP 规范回 MethodNotFound，不要让客户端一直等
            _ => id.map(|i| error_response(&i, -32601, &format!("unsupported method `{method}`"))),
        };
        reply
    }

    fn on_initialize(&mut self, msg: &Json) -> Json {
        self.initialized = true;
        result_response(
            msg.get("id").cloned().unwrap_or(Json::Null),
            Json::obj(vec![
                (
                    "capabilities",
                    Json::obj(vec![
                        // 文档符号用层级结构
                        ("documentSymbolProvider", Json::Bool(true)),
                        ("definitionProvider", Json::Bool(true)),
                        ("hoverProvider", Json::Bool(true)),
                        ("documentFormattingProvider", Json::Bool(true)),
                        ("completionProvider", Json::Bool(true)),
                        // 增量同步：只发改动的部分，文本由客户端算好后整段送来
                        (
                            "textDocumentSync",
                            Json::obj(vec![
                                ("openClose", Json::Bool(true)),
                                ("change", Json::int(1)),
                            ]),
                        ),
                    ]),
                ),
                (
                    "serverInfo",
                    Json::obj(vec![
                        ("name", Json::str("zr-lsp")),
                        ("version", Json::str(SERVER_VERSION)),
                    ]),
                ),
            ]),
        )
    }

    fn on_completion(&self, params: &Json) -> Json {
        let (Some(uri), Some(pos)) = (doc_uri(params), position_of(params)) else {
            return Json::Arr(vec![]);
        };
        let Some(text) = self.docs.get(&uri) else {
            return Json::Arr(vec![]);
        };
        let Some(prefix) = word_at(text, pos.0, pos.1) else {
            return Json::Arr(vec![]);
        };
        let syms = symbols_of(text);
        let items: Vec<Json> = syms
            .into_iter()
            .filter(|s| s.name.starts_with(&prefix))
            .map(|s| {
                let kind = match s.kind {
                    kind::FUNCTION => 3, // CompletionItemKind.Function
                    kind::CONSTANT => 14, // CompletionItemKind.Constant
                    kind::STRUCT => 7,   // CompletionItemKind.Struct
                    kind::ENUM => 13,    // CompletionItemKind.Enum
                    kind::PARAMETER => 9, // CompletionItemKind.Variable
                    _ => 1,              // CompletionItemKind.Text
                };
                Json::obj(vec![
                    ("label", Json::str(&s.name)),
                    ("kind", Json::int(kind)),
                    ("detail", Json::str(match s.kind {
                        kind::FUNCTION => "fn",
                        kind::CONSTANT => "const",
                        kind::STRUCT => "struct",
                        kind::ENUM => "enum",
                        kind::PARAMETER => "param",
                        _ => "symbol",
                    })),
                    ("sortText", Json::str(&s.name)),
                ])
            })
            .collect();
        Json::Arr(items)
    }

    fn on_did_open(&mut self, params: &Json) {
        let (Some(uri), Some(text)) = (doc_uri(params), doc_text(params)) else {
            return;
        };
        self.docs.insert(uri.clone(), text);
        self.publish_diagnostics(&uri);
    }

    fn on_did_change(&mut self, params: &Json) {
        let Some(uri) = doc_uri(params) else {
            return;
        };
        // `change` 数组的最后一项是整份新文本（TextDocumentSyncKind.Full）
        let Some(changes) = params.get("contentChanges").and_then(Json::as_arr) else {
            return;
        };
        if let Some(last) = changes.last() {
            if let Some(text) = last.get("text").and_then(Json::as_str) {
                self.docs.insert(uri.clone(), text.to_string());
                self.publish_diagnostics(&uri);
            }
        }
    }

    /// 重新解析并推送诊断。
    fn publish_diagnostics(&self, uri: &str) {
        let Some(text) = self.docs.get(uri) else {
            return;
        };
        let diags = diagnostics(text);
        let msg = Json::obj(vec![
            ("jsonrpc", Json::str("2.0")),
            ("method", Json::str("textDocument/publishDiagnostics")),
            (
                "params",
                Json::obj(vec![
                    ("uri", Json::str(uri)),
                    ("diagnostics", Json::Arr(diags)),
                ]),
            ),
        ]);
        // 通知没有 id，回给客户端就行；写失败由调用方处理
        let _ = write_message(&msg);
    }

    fn on_document_symbol(&self, params: &Json) -> Json {
        let Some(uri) = doc_uri(params) else {
            return Json::Arr(vec![]);
        };
        let Some(text) = self.docs.get(&uri) else {
            return Json::Arr(vec![]);
        };
        Json::Arr(
            symbols_of(text)
                .into_iter()
                .map(|s| symbol_json(&s))
                .collect(),
        )
    }

    fn on_definition(&self, params: &Json) -> Json {
        let (Some(uri), Some(pos)) = (doc_uri(params), position_of(params)) else {
            return Json::Null;
        };
        let Some(text) = self.docs.get(&uri) else {
            return Json::Null;
        };
        // 光标处的标识符
        let Some(word) = word_at(text, pos.0, pos.1) else {
            return Json::Null;
        };
        let syms = symbols_of(text);
        let Some(s) = syms.iter().find(|s| s.name == word) else {
            return Json::Null;
        };
        Json::obj(vec![
            ("uri", Json::str(uri)),
            ("range", range_json(s.line, s.character, s.end_line, 0)),
        ])
    }

    fn on_hover(&self, params: &Json) -> Json {
        let (Some(uri), Some(pos)) = (doc_uri(params), position_of(params)) else {
            return Json::Null;
        };
        let Some(text) = self.docs.get(&uri) else {
            return Json::Null;
        };
        let Some(word) = word_at(text, pos.0, pos.1) else {
            return Json::Null;
        };
        let Some(s) = symbols_of(text).into_iter().find(|s| s.name == word) else {
            return Json::Null;
        };
        // 必须用和 symbols_of 同一套 LSP SymbolKind 数值。早先这里写的是
        // 一套旧的临时编号，于是 `hover` 把函数显示成 `const`。
        let kind = match s.kind {
            kind::FUNCTION => "fn",
            kind::CONSTANT => "const",
            kind::STRUCT => "struct",
            kind::ENUM => "enum",
            _ => "symbol",
        };
        Json::obj(vec![
            (
                "contents",
                Json::obj(vec![
                    ("kind", Json::str("markdown")),
                    ("value", Json::str(format!("```zr\n{kind} {}\n```", s.name))),
                ]),
            ),
            ("range", range_json(s.line, s.character, s.end_line, 0)),
        ])
    }

    fn on_formatting(&self, params: &Json) -> Json {
        let Some(uri) = doc_uri(params) else {
            return Json::Arr(vec![]);
        };
        let Some(text) = self.docs.get(&uri) else {
            return Json::Arr(vec![]);
        };
        let formatted = crate::fmt::format_source(text);
        if formatted == *text {
            return Json::Arr(vec![]);
        }
        // 全文替换：返回覆盖整篇文档的一条编辑
        let lines = formatted.lines().count() as u32;
        Json::Arr(vec![Json::obj(vec![
            (
                "range",
                Json::obj(vec![
                    (
                        "start",
                        Json::obj(vec![("line", Json::int(0)), ("character", Json::int(0))]),
                    ),
                    ("end", range_json(lines, 0, lines, 0)),
                ]),
            ),
            ("newText", Json::str(formatted)),
        ])])
    }
}

/// LSP 的符号种类常量。
mod kind {
    pub const FUNCTION: i64 = 12; // SymbolKind.Function
    pub const CONSTANT: i64 = 14; // SymbolKind.Constant
    pub const STRUCT: i64 = 23; // SymbolKind.Struct
    pub const PARAMETER: i64 = 13; // SymbolKind.Variable（形参用 Variable）
    pub const ENUM: i64 = 10; // SymbolKind.Enum
}

fn symbol_json(s: &Symbol) -> Json {
    Json::obj(vec![
        ("name", Json::str(&s.name)),
        ("kind", Json::int(s.kind)),
        ("range", range_json(s.line, s.character, s.end_line, 0)),
        (
            "selectionRange",
            range_json(
                s.line,
                s.character,
                s.line,
                s.character + s.name.chars().count() as u32,
            ),
        ),
    ])
}

fn range_json(sl: u32, sc: u32, el: u32, ec: u32) -> Json {
    Json::obj(vec![
        (
            "start",
            Json::obj(vec![
                ("line", Json::int(sl as i64)),
                ("character", Json::int(sc as i64)),
            ]),
        ),
        (
            "end",
            Json::obj(vec![
                ("line", Json::int(el as i64)),
                ("character", Json::int(ec as i64)),
            ]),
        ),
    ])
}

/// 语法诊断。
///
/// 只报词法错误和语法错误。多报一条假错误的代价很大：IDE 里满屏红波浪线
/// 会让人直接关掉诊断。
fn diagnostics(text: &str) -> Vec<Json> {
    let mut out = Vec::new();

    // 1) 词法错误
    for t in Lexer::new(text).tokenize() {
        if let TokenKind::LexError(msg) = t.kind {
            out.push(diag(&msg, t.span.line, t.span.column, 1));
        }
    }
    if !out.is_empty() {
        return out;
    }

    // 2) 语法错误
    let toks = Lexer::new(text).tokenize();
    let mut parser = crate::parser::Parser::new(toks);
    if let Err(e) = parser.parse() {
        let (line, col) = span_of(&e.to_string()).unwrap_or((1, 1));
        out.push(diag(&e.to_string(), line, col, 1));
    }
    out
}

fn diag(msg: &str, line: usize, column: usize, severity: i64) -> Json {
    let line0 = line.saturating_sub(1);
    let col0 = column.saturating_sub(1);
    Json::obj(vec![
        (
            "range",
            range_json(line0 as u32, col0 as u32, line0 as u32, col0 as u32 + 1),
        ),
        ("severity", Json::int(severity)),
        ("source", Json::str("zr")),
        ("message", Json::str(msg)),
    ])
}

/// 从错误文本里抠出行列（格式是 `line:col: message`）。
fn span_of(msg: &str) -> Option<(usize, usize)> {
    let mut it = msg.split(':');
    let line: usize = it.next()?.trim().parse().ok()?;
    let col: usize = it.next()?.trim().parse().ok()?;
    Some((line, col))
}

/// 从记号流里扫出顶层符号。
///
/// 用记号流而不是 AST：AST 目前不留标识符的名字 span，而记号本来就有精确的
/// 行列，顺带还能容忍编译器还不认识的语法。
fn symbols_of(text: &str) -> Vec<Symbol> {
    let toks = Lexer::new(text).tokenize();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < toks.len() {
        let kw = &toks[i].kind;
        let name_tok = &toks[i + 1];
        let TokenKind::Ident(name) = &name_tok.kind else {
            i += 1;
            continue;
        };
        let kind = match kw {
            TokenKind::KwFn => kind::FUNCTION,
            TokenKind::KwConst => kind::CONSTANT,
            TokenKind::KwStruct => kind::STRUCT,
            TokenKind::KwEnum => kind::ENUM,
            _ => {
                i += 1;
                continue;
            }
        };
        // 结束行：一直扫到行尾的大括号配平（函数/类型），常量到分号
        let end_line = match kw {
            TokenKind::KwConst => {
                let mut j = i;
                while j < toks.len() && !matches!(toks[j].kind, TokenKind::Semicolon) {
                    j += 1;
                }
                toks.get(j)
                    .map(|t| t.span.line)
                    .unwrap_or(name_tok.span.line)
            }
            _ => block_end_line(&toks, i + 1),
        };
        out.push(Symbol {
            name: name.clone(),
            kind,
            line: (name_tok.span.line.saturating_sub(1)) as u32,
            character: (name_tok.span.column.saturating_sub(1)) as u32,
            end_line: (end_line.saturating_sub(1)) as u32,
        });

        // 函数的形参也收进来：`goto definition` 跳到形参是最常见的诉求，
        // 只认顶层符号的话点参数名会什么反应都没有。
        if matches!(kw, TokenKind::KwFn) {
            // i 指向 `fn`，i+1 是函数名，形参从 i+2 才开始
            out.extend(params_of(&toks, i + 2));
        }
        i += 2;
    }
    out
}

/// 取出 `fn` 的形参名（扫到函数体左花括号为止）。
fn params_of(toks: &[crate::lexer::Token], from: usize) -> Vec<Symbol> {
    let mut out = Vec::new();
    let mut i = from;
    while i < toks.len() {
        match toks[i].kind {
            TokenKind::LParen | TokenKind::Comma | TokenKind::Amp | TokenKind::KwMut => i += 1,
            TokenKind::Ident(ref name) => {
                out.push(Symbol {
                    name: name.clone(),
                    kind: kind::PARAMETER,
                    line: (toks[i].span.line.saturating_sub(1)) as u32,
                    character: (toks[i].span.column.saturating_sub(1)) as u32,
                    end_line: (toks[i].span.line.saturating_sub(1)) as u32,
                });
                i += 1;
            }
            // 遇到 `:` 之后就是类型，跳到下一个 `,` 或 `)`
            TokenKind::Colon => {
                while i < toks.len()
                    && !matches!(toks[i].kind, TokenKind::Comma | TokenKind::RParen)
                {
                    i += 1;
                }
            }
            // 函数体（或返回类型）开始，参数扫完了
            TokenKind::LBrace | TokenKind::Arrow => break,
            TokenKind::RParen => break,
            _ => i += 1,
        }
    }
    out
}

/// 从 `{` 开始找配平的 `}`，返回它的行号。
fn block_end_line(toks: &[crate::lexer::Token], from: usize) -> usize {
    let mut depth = 0i32;
    let mut i = from;
    while i < toks.len() {
        match toks[i].kind {
            TokenKind::LBrace => depth += 1,
            TokenKind::RBrace => {
                depth -= 1;
                if depth <= 0 {
                    return toks[i].span.line;
                }
            }
            TokenKind::Eof => break,
            _ => {}
        }
        i += 1;
    }
    toks.last().map(|t| t.span.line).unwrap_or(1)
}

/// 取出 (行, 列) 处的标识符。
fn word_at(text: &str, line: u32, character: u32) -> Option<String> {
    let src = text.lines().nth(line as usize)?;
    let chars: Vec<char> = src.chars().collect();
    let pos = (character as usize).min(chars.len());
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    if pos < chars.len() && !is_word(chars[pos]) {
        return None;
    }
    let mut start = pos;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = pos;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some(chars[start..end].iter().collect())
}

fn doc_uri(params: &Json) -> Option<String> {
    params
        .get("textDocument")?
        .get("uri")?
        .as_str()
        .map(str::to_string)
}

fn doc_text(params: &Json) -> Option<String> {
    params
        .get("textDocument")?
        .get("text")?
        .as_str()
        .map(str::to_string)
}

fn position_of(params: &Json) -> Option<(u32, u32)> {
    let p = params.get("position")?;
    let line = p.get("line")?.as_i64()?;
    let ch = p.get("character")?.as_i64()?;
    Some((line.max(0) as u32, ch.max(0) as u32))
}

fn result_response(id: Json, result: Json) -> Json {
    Json::obj(vec![
        ("jsonrpc", Json::str("2.0")),
        ("id", id),
        ("result", result),
    ])
}

fn error_response(id: &Json, code: i64, message: &str) -> Json {
    Json::obj(vec![
        ("jsonrpc", Json::str("2.0")),
        ("id", id.clone()),
        (
            "error",
            Json::obj(vec![
                ("code", Json::int(code)),
                ("message", Json::str(message)),
            ]),
        ),
    ])
}

/// 读一条 LSP 帧：`Content-Length: N\r\n\r\n` + N 字节。
fn read_message<R: Read>(reader: &mut R) -> Result<Option<String>, String> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    // 读头部直到空行
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                header.push(byte[0]);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
                // 头部长度上限，防止畸形输入把内存吃光
                if header.len() > 8192 {
                    return Err("header too long".into());
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let head = String::from_utf8_lossy(&header).to_string();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("Content-Length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .ok_or("missing Content-Length header")?;
    if len > 64 * 1024 * 1024 {
        return Err("content too large".into());
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// 写一条 LSP 帧。
fn write_message(msg: &Json) -> std::io::Result<()> {
    let body = msg.to_string();
    let out = std::io::stdout();
    let mut w = out.lock();
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(body.as_bytes())?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_functions() {
        // 形参也算符号，所以这里期望 2 个：函数本身 + 形参 a
        let syms = symbols_of("fn add(a: i32) -> i32 {\n    return a;\n}\n");
        assert_eq!(syms.len(), 2, "实际: {syms:?}");
        assert_eq!(syms[0].name, "add");
        assert_eq!(syms[1].name, "a");
        assert_eq!(syms[1].kind, kind::PARAMETER);
        assert_eq!(syms[0].kind, kind::FUNCTION);
        assert_eq!(syms[0].line, 0);
        assert_eq!(syms[0].end_line, 2);
    }

    #[test]
    fn finds_structs_and_consts() {
        let syms = symbols_of("struct P {\n    x: i32,\n}\nconst K: i32 = 7;\n");
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"P"));
        assert!(names.contains(&"K"));
    }

    #[test]
    fn clean_source_has_no_diagnostics() {
        assert!(diagnostics("fn main() -> i32 {\n    return 0;\n}\n").is_empty());
    }

    #[test]
    fn syntax_error_is_reported() {
        let d = diagnostics("fn main( {\n}\n");
        assert!(!d.is_empty(), "语法错误应产生诊断");
        assert_eq!(d[0].get("source").unwrap().as_str(), Some("zr"));
    }

    #[test]
    fn lex_error_is_reported() {
        let d = diagnostics("fn main() { let x = @; }\n");
        assert!(!d.is_empty());
    }

    #[test]
    fn word_lookup() {
        let src = "fn add(a: i32) -> i32 { return a; }";
        assert_eq!(word_at(src, 0, 4).as_deref(), Some("add"));
        assert_eq!(word_at(src, 0, 3).as_deref(), Some("add"));
        // 第 6 个字符是 `(`，不是标识符的一部分
        assert_eq!(word_at(src, 0, 6), None);
        assert_eq!(word_at(src, 0, 7).as_deref(), Some("a"));
    }

    #[test]
    fn initialize_advertises_capabilities() {
        let mut s = Server::new();
        let msg = Json::parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
        let out = s.handle_raw(&msg.to_string()).unwrap();
        let caps = out.get("result").unwrap().get("capabilities").unwrap();
        // hoverProvider 是 `true`（Json::Bool）
        assert_eq!(caps.get("hoverProvider"), Some(&Json::Bool(true)));
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let mut s = Server::new();
        let msg =
            Json::parse(r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/references"}"#).unwrap();
        let out = s.handle_raw(&msg.to_string()).unwrap();
        assert_eq!(
            out.get("error").unwrap().get("code").unwrap().as_i64(),
            Some(-32601)
        );
    }

    #[test]
    fn notifications_produce_no_reply() {
        let mut s = Server::new();
        let msg = Json::parse(r#"{"jsonrpc":"2.0","method":"initialized"}"#).unwrap();
        assert!(s.handle_raw(&msg.to_string()).is_none());
    }
}
