//! A small JSON value type with a strict parser and a compact serializer.
//! Objects keep insertion order (response shapes read naturally and the
//! MCP tools list stays byte-stable across calls).

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Parse a complete JSON document.
    pub fn parse(data: &[u8]) -> Result<Json, String> {
        Parser::parse(data)
    }

    pub fn obj(fields: Vec<(&str, Json)>) -> Json {
        Json::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

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

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) if n.fract() == 0.0 && *n >= -9.0e15 && *n <= 9.0e15 => Some(*n as i64),
            Json::Str(s) => s.parse::<i64>().ok(),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Compact serialization (no spaces), the form every surface returns.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        match self {
            Json::Null => out.extend_from_slice(b"null"),
            Json::Bool(true) => out.extend_from_slice(b"true"),
            Json::Bool(false) => out.extend_from_slice(b"false"),
            Json::Num(n) => write_number(*n, out),
            Json::Str(s) => write_string(s, out),
            Json::Arr(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.write_to(out);
                }
                out.push(b']');
            }
            Json::Obj(fields) => {
                out.push(b'{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write_string(key, out);
                    out.push(b':');
                    value.write_to(out);
                }
                out.push(b'}');
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        self.write_to(&mut out);
        out
    }

    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.to_bytes()).into_owned()
    }
}

fn write_number(n: f64, out: &mut Vec<u8>) {
    if n == n.trunc() && n.abs() <= 9.0e15 {
        out.extend_from_slice((n as i64).to_string().as_bytes());
    } else {
        out.extend_from_slice(n.to_string().as_bytes());
    }
}

/// Append a JSON string literal (quoted, escaped) to `out`.
pub fn write_string(value: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for ch in value.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{08}' => out.extend_from_slice(b"\\b"),
            '\u{0c}' => out.extend_from_slice(b"\\f"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// Escape a string for embedding inside an already-quoted context.
pub fn escape_string(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len() + 2);
    write_string(value, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

// ------------------------------------------------------------------ parser

pub struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Parser { data, pos: 0 }
    }

    pub fn parse(data: &'a [u8]) -> Result<Json, String> {
        let mut parser = Parser::new(data);
        parser.skip_ws();
        let value = parser.value()?;
        parser.skip_ws();
        if parser.pos != parser.data.len() {
            return Err(format!("trailing bytes at {}", parser.pos));
        }
        Ok(value)
    }

    fn skip_ws(&mut self) {
        while self.pos < self.data.len() && matches!(self.data[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(format!("unexpected byte at {}", self.pos)),
        }
    }

    fn literal(&mut self, text: &str, value: Json) -> Result<Json, String> {
        if self.data.len() - self.pos >= text.len() && &self.data[self.pos..self.pos + text.len()] == text.as_bytes() {
            self.pos += text.len();
            Ok(value)
        } else {
            Err(format!("bad literal at {}", self.pos))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.pos += 1; // {
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(format!("object key expected at {}", self.pos));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(format!("colon expected at {}", self.pos));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(fields));
                }
                _ => return Err(format!("object separator expected at {}", self.pos)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("array separator expected at {}", self.pos)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let byte = self.peek().ok_or("unterminated string")?;
            self.pos += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xd800..0xdc00).contains(&hi) {
                                // High surrogate: a low one should follow.
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    if self.peek() == Some(b'u') {
                                        self.pos += 1;
                                        let lo = self.hex4()?;
                                        let combined =
                                            0x10000 + (((hi - 0xd800) as u32) << 10) + (lo as u32 - 0xdc00);
                                        out.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                                        continue;
                                    }
                                    return Err("bad surrogate pair".into());
                                }
                                return Err("lone surrogate".into());
                            }
                            out.push(char::from_u32(hi as u32).unwrap_or('\u{fffd}'));
                        }
                        _ => return Err("bad escape".into()),
                    }
                }
                _ => {
                    // Copy the full UTF-8 sequence for this code point.
                    let start = self.pos - 1;
                    let width = utf8_width(byte);
                    if width == 0 || start + width > self.data.len() {
                        return Err("bad utf-8 in string".into());
                    }
                    match std::str::from_utf8(&self.data[start..start + width]) {
                        Ok(s) => out.push_str(s),
                        Err(_) => return Err("bad utf-8 in string".into()),
                    }
                    self.pos = start + width;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.data.len() {
            return Err("short unicode escape".into());
        }
        let mut value = 0u16;
        for i in 0..4 {
            let c = self.data[self.pos + i];
            let digit = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err("bad unicode escape".into()),
            };
            value = (value << 4) | digit as u16;
        }
        self.pos += 4;
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.data[start..self.pos]).map_err(|_| "bad number")?;
        text.parse::<f64>().map(Json::Num).map_err(|_| format!("bad number {text}"))
    }
}

fn utf8_width(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        0
    }
}
