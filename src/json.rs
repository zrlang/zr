//==============================================================================
// json.rs — 极简 JSON 实现（给 LSP 用）
//
// 只实现 JSON-RPC 2.0 需要的那一小块：解析对象/数组/字符串/数字/字面量，
// 以及序列化。不引 serde 之类的依赖——一个语言服务器需要的 JSON 结构
// 种类非常有限，自己写反而更可控。
//
// 对象保持**插入顺序**（用 Vec 而不是 HashMap）：回给客户端的字段顺序稳定，
// 便于比对和调试。
//==============================================================================

use std::fmt::Write as _;

/// JSON 值。
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// 数字。整数与浮点统一按 f64 存；LSP 的 position 就是整数，
    /// 序列化时会去掉 `.0`。
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj(fields: Vec<(&str, Json)>) -> Json {
        Json::Obj(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }
    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }
    pub fn int(i: i64) -> Json {
        Json::Num(i as f64)
    }

    /// 取字段（对象里没有则 `None`）。
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }

    /// 序列化。
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => {
                if n.is_finite() && *n == n.trunc() && n.abs() < 1e15 {
                    let _ = write!(out, "{}", *n as i64);
                } else if n.is_finite() {
                    let _ = write!(out, "{n}");
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_string(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    it.write(out);
                }
                out.push(']');
            }
            Json::Obj(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    /// 解析。出错时返回 `Err` 而不是 panic——LSP 的输入不可信。
    pub fn parse(src: &str) -> Result<Json, String> {
        let b: Vec<char> = src.chars().collect();
        let mut p = Parser { b, i: 0 };
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        Ok(v)
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser {
    b: Vec<char>,
    i: usize,
}

impl Parser {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_whitespace() {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        let Some(&c) = self.b.get(self.i) else {
            return Err("unexpected end of JSON".into());
        };
        match c {
            '{' => self.object(),
            '[' => self.array(),
            '"' => Ok(Json::Str(self.string()?)),
            't' => self.literal("true", Json::Bool(true)),
            'f' => self.literal("false", Json::Bool(false)),
            'n' => self.literal("null", Json::Null),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str, v: Json) -> Result<Json, String> {
        for c in word.chars() {
            if self.b.get(self.i) != Some(&c) {
                return Err(format!("expected `{word}`"));
            }
            self.i += 1;
        }
        Ok(v)
    }

    fn object(&mut self) -> Result<Json, String> {
        self.i += 1; // '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.b.get(self.i) == Some(&'}') {
            self.i += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.b.get(self.i) != Some(&':') {
                return Err("expected `:` in object".into());
            }
            self.i += 1;
            let val = self.value()?;
            fields.push((key, val));
            self.skip_ws();
            match self.b.get(self.i) {
                Some(',') => self.i += 1,
                Some('}') => {
                    self.i += 1;
                    return Ok(Json::Obj(fields));
                }
                _ => return Err("expected `,` or `}` in object".into()),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.i += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.b.get(self.i) == Some(&']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.b.get(self.i) {
                Some(',') => self.i += 1,
                Some(']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err("expected `,` or `]` in array".into()),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&'"') {
            return Err("expected string".into());
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return Err("unterminated string".into());
            };
            self.i += 1;
            match c {
                '"' => return Ok(out),
                '\\' => {
                    let Some(&e) = self.b.get(self.i) else {
                        return Err("unterminated escape".into());
                    };
                    self.i += 1;
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => {
                            let hex: String = self.b[self.i..(self.i + 4).min(self.b.len())]
                                .iter()
                                .collect();
                            self.i += 4;
                            let n = u32::from_str_radix(&hex, 16)
                                .map_err(|_| "bad \\u escape".to_string())?;
                            out.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.b.get(self.i) == Some(&'-') {
            self.i += 1;
        }
        while self.b.get(self.i).is_some_and(|c| {
            c.is_ascii_digit() || *c == '.' || *c == 'e' || *c == 'E' || *c == '+' || *c == '-'
        }) {
            self.i += 1;
        }
        let text: String = self.b[start..self.i].iter().collect();
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid number `{text}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars() {
        assert_eq!(Json::parse("null").unwrap(), Json::Null);
        assert_eq!(Json::parse("true").unwrap(), Json::Bool(true));
        assert_eq!(Json::parse("-12").unwrap(), Json::Num(-12.0));
        assert_eq!(Json::parse("\"hi\"").unwrap(), Json::Str("hi".into()));
    }

    #[test]
    fn parses_nested() {
        let v = Json::parse(r#"{"a":[1,{"b":"x"}],"c":null}"#).unwrap();
        assert_eq!(v.get("c"), Some(&Json::Null));
        let a = v.get("a").unwrap().as_arr().unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[1].get("b").unwrap().as_str(), Some("x"));
    }

    #[test]
    fn handles_escapes() {
        let v = Json::parse(r#""a\"b\nA""#).unwrap();
        assert_eq!(v.as_str(), Some("a\"b\nA"));
    }

    #[test]
    fn round_trips() {
        let v = Json::obj(vec![
            ("id", Json::int(3)),
            ("method", Json::str("shutdown")),
            ("result", Json::Null),
            ("ok", Json::Bool(true)),
        ]);
        let text = v.to_string();
        assert_eq!(Json::parse(&text).unwrap(), v);
    }

    #[test]
    fn integers_serialize_without_decimal() {
        assert_eq!(Json::Num(3.0).to_string(), "3");
        assert_eq!(Json::Num(-7.0).to_string(), "-7");
    }

    #[test]
    fn rejects_malformed() {
        assert!(Json::parse("{").is_err());
        assert!(Json::parse("{\"a\"}").is_err());
        assert!(Json::parse("[1,").is_err());
        assert!(Json::parse("tru").is_err());
    }

    #[test]
    fn non_ascii_passes_through() {
        let v = Json::parse("\"你好\"").unwrap();
        assert_eq!(v.as_str(), Some("你好"));
        assert_eq!(Json::parse(&v.to_string()).unwrap(), v);
    }
}
