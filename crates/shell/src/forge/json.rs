//! A small JSON reader for what the forge APIs send back.
//!
//! Pure: text in, value out. It exists so the forge clients need no JSON
//! crate (none is in the PRD's build-vs-library table). Strict about the
//! grammar, bounded in depth, and it never panics on any input.

/// Nesting deeper than this is refused (a hostile server could otherwise
/// overflow the stack).
const MAX_DEPTH: usize = 48;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<Json>),
    /// Keys keep their order; the first of a repeated key wins in `get`.
    Object(Vec<(String, Json)>),
}

/// Why a text is not JSON. Carries no part of the text, so it is safe to
/// show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub at: usize,
    pub what: &'static str,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.at, self.what)
    }
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, JsonError> {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            at: 0,
        };
        parser.skip_space();
        let value = parser.value(0)?;
        parser.skip_space();
        if parser.at != parser.bytes.len() {
            return Err(parser.fail("trailing characters"));
        }
        Ok(value)
    }

    /// The value of `key` when this is an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// A whole, non-negative number.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Number(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 9.0e15 => Some(*n as u64),
            _ => None,
        }
    }

    /// `get(key)` as text; `null` and a missing key are both `None`.
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Json::as_str)
    }

    /// Serializes this JSON value into compact RFC 8259 text.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        encode_value(self, &mut out);
        out
    }
}

fn encode_value(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => {
            if n.is_finite() {
                if n.fract() == 0.0 && *n >= 0.0 && *n < 9.0e15 {
                    out.push_str(&format!("{}", *n as u64));
                } else {
                    out.push_str(&format!("{n}"));
                }
            } else {
                out.push_str("null");
            }
        }
        Json::Str(s) => encode_string(s, out),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_value(item, out);
            }
            out.push(']');
        }
        Json::Object(fields) => {
            out.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_string(key, out);
                out.push(':');
                encode_value(item, out);
            }
            out.push('}');
        }
    }
}

fn encode_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn fail(&self, what: &'static str) -> JsonError {
        JsonError { at: self.at, what }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn word(&mut self, word: &str, value: Json) -> Result<Json, JsonError> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.fail("unexpected character"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth > MAX_DEPTH {
            return Err(self.fail("nested too deeply"));
        }
        match self.peek() {
            None => Err(self.fail("unexpected end")),
            Some(b'n') => self.word("null", Json::Null),
            Some(b't') => self.word("true", Json::Bool(true)),
            Some(b'f') => self.word("false", Json::Bool(false)),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.skip_space();
                if self.eat(b']') {
                    return Ok(Json::Array(items));
                }
                loop {
                    self.skip_space();
                    items.push(self.value(depth + 1)?);
                    self.skip_space();
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b']') {
                        return Ok(Json::Array(items));
                    }
                    return Err(self.fail("expected , or ]"));
                }
            }
            Some(b'{') => {
                self.at += 1;
                let mut fields = Vec::new();
                self.skip_space();
                if self.eat(b'}') {
                    return Ok(Json::Object(fields));
                }
                loop {
                    self.skip_space();
                    if self.peek() != Some(b'"') {
                        return Err(self.fail("expected a key"));
                    }
                    let key = self.string()?;
                    self.skip_space();
                    if !self.eat(b':') {
                        return Err(self.fail("expected :"));
                    }
                    self.skip_space();
                    fields.push((key, self.value(depth + 1)?));
                    self.skip_space();
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b'}') {
                        return Ok(Json::Object(fields));
                    }
                    return Err(self.fail("expected , or }"));
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.fail("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.at;
        self.eat(b'-');
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.fail("bad number")),
        }
        if self.eat(b'.') {
            let before = self.at;
            self.digits();
            if self.at == before {
                return Err(self.fail("bad number"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            let before = self.at;
            self.digits();
            if self.at == before {
                return Err(self.fail("bad number"));
            }
        }
        // The slice is ASCII digits and signs only, so it is valid UTF-8.
        let text = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|_| self.fail("bad number"))?;
        text.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| self.fail("bad number"))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let end = self.at + 4;
        let digits = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| self.fail("short \\u escape"))?;
        let mut value = 0;
        for byte in digits {
            let digit = (*byte as char)
                .to_digit(16)
                .ok_or_else(|| self.fail("bad \\u escape"))?;
            value = value * 16 + digit;
        }
        self.at = end;
        Ok(value)
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.at += 1; // the opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.fail("unterminated string"));
            };
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(escape) = self.peek() else {
                        return Err(self.fail("unterminated string"));
                    };
                    self.at += 1;
                    let ch = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let first = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&first) {
                                // A high surrogate must be followed by a low one.
                                if !(self.eat(b'\\') && self.eat(b'u')) {
                                    return Err(self.fail("lone surrogate"));
                                }
                                let second = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&second) {
                                    return Err(self.fail("lone surrogate"));
                                }
                                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&first) {
                                return Err(self.fail("lone surrogate"));
                            } else {
                                first
                            };
                            char::from_u32(code).ok_or_else(|| self.fail("bad \\u escape"))?
                        }
                        _ => return Err(self.fail("bad escape")),
                    };
                    let mut buffer = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                }
                0..=0x1f => return Err(self.fail("control character in string")),
                other => out.push(other),
            }
        }
        // The input was a `&str` and every escape was encoded as UTF-8, so
        // this cannot fail; stay total anyway.
        String::from_utf8(out).map_err(|_| self.fail("invalid UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn write(value: &Json, out: &mut String) {
        match value {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => out.push_str(&format!("{n}")),
            Json::Str(s) => write_string(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            Json::Object(fields) => {
                out.push('{');
                for (i, (key, item)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    write(item, out);
                }
                out.push('}');
            }
        }
    }

    fn write_string(text: &str, out: &mut String) {
        out.push('"');
        for ch in text.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                // Exercise the surrogate-pair path for everything outside the BMP.
                c if (c as u32) >= 0x10000 => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        out.push_str(&format!("\\u{unit:04x}"));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }

    fn json() -> impl Strategy<Value = Json> {
        let leaf = prop_oneof![
            Just(Json::Null),
            any::<bool>().prop_map(Json::Bool),
            (-1_000_000i64..1_000_000).prop_map(|n| Json::Number(n as f64)),
            "\\PC{0,8}".prop_map(Json::Str),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Json::Array),
                proptest::collection::vec(("[a-z]{1,4}", inner), 0..4).prop_map(Json::Object),
            ]
        })
    }

    #[test]
    fn reads_what_the_apis_send() {
        let value = Json::parse(
            r#" {"login":"ana","id":42,"private":false,"description":null,
                 "name":"caf\u00e9 \ud83d\ude00 \"q\"","list":[1,2.5,-3e2]} "#,
        )
        .unwrap();
        assert_eq!(value.str_field("login"), Some("ana"));
        assert_eq!(value.get("id").and_then(Json::as_u64), Some(42));
        assert_eq!(value.get("private").and_then(Json::as_bool), Some(false));
        assert_eq!(value.str_field("description"), None);
        assert_eq!(value.str_field("name"), Some("café 😀 \"q\""));
        assert_eq!(
            value.get("list").and_then(Json::as_array).map(<[_]>::len),
            Some(3)
        );
        assert_eq!(value.get("missing"), None);
    }

    #[test]
    fn refuses_what_is_not_json() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":}",
            "{'a':1}",
            "nul",
            "01",
            "1.",
            "-",
            "\"abc",
            "\"\\x\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"a\nb\"",
            "[1] x",
            "{\"a\" 1}",
            "1e",
        ] {
            assert!(Json::parse(bad).is_err(), "{bad:?}");
        }
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(Json::parse(&deep).is_err());
    }

    #[test]
    fn a_number_is_only_a_u64_when_it_is_whole_and_not_negative() {
        for (text, expected) in [
            ("7", Some(7)),
            ("7.5", None),
            ("-1", None),
            ("1e3", Some(1000)),
        ] {
            assert_eq!(Json::parse(text).unwrap().as_u64(), expected, "{text}");
        }
    }

    proptest! {
        #[test]
        fn what_is_written_reads_back(value in json()) {
            let mut text = String::new();
            write(&value, &mut text);
            prop_assert_eq!(Json::parse(&text), Ok(value));
        }

        #[test]
        fn no_input_makes_the_reader_panic(text in "\\PC{0,64}") {
            let _ = Json::parse(&text);
        }
    }
}
