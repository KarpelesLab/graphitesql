//! A minimal JSON value model with the parser, serializer, and path navigation
//! behind SQLite's core JSON functions (`json`, `json_extract`, `json_type`, …).
//!
//! The parser accepts **JSON5** (a superset of RFC-8259) on input, matching the
//! `sqlite3` CLI: unquoted/`'single-quoted'` object keys, single-quoted strings,
//! one trailing comma, `// line` and `/* block */` comments, hexadecimal and
//! `+`/leading-or-trailing-`.` numbers, and the `Infinity`/`NaN` literals. The
//! canonical *output* (`serialize`/`to_sql`) stays strict minified JSON — JSON5
//! in, canonical JSON out — exactly like sqlite, where `json('{a:1}')` yields
//! `{"a":1}`. `Infinity` renders as `9e999` and `NaN` parses to JSON `null`.
//!
//! SQLite's conventions for the scalar mapping back to SQL values: JSON
//! `true`/`false` become integers `1`/`0`, `null` becomes SQL `NULL`, numbers
//! become INTEGER or REAL, strings become TEXT, and objects/arrays are returned
//! as their minified JSON text.

use crate::value::Value;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// An integral number that fits in `i64`.
    Int(i64),
    /// Any other number.
    Real(f64),
    /// A string (unescaped).
    Str(String),
    /// An array.
    Array(Vec<Json>),
    /// An object, preserving member order.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// SQLite's `json_type` label for this value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(true) => "true",
            Json::Bool(false) => "false",
            Json::Int(_) => "integer",
            Json::Real(_) => "real",
            Json::Str(_) => "text",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }

    /// Map a JSON scalar back to a SQL [`Value`]; objects and arrays serialize to
    /// their minified JSON text (matching `json_extract`).
    pub fn to_sql(&self) -> Value {
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Integer(*b as i64),
            Json::Int(i) => Value::Integer(*i),
            Json::Real(r) => Value::Real(*r),
            Json::Str(s) => Value::Text(s.clone()),
            Json::Array(_) | Json::Object(_) => Value::Text(self.serialize()),
        }
    }

    /// Serialize to compact (minified) JSON text.
    pub fn serialize(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }

    /// Serialize to pretty-printed JSON using `indent` as the per-level unit
    /// (SQLite's `json_pretty`). Empty arrays/objects stay on one line; scalars
    /// render compactly.
    pub fn pretty(&self, indent: &str) -> String {
        let mut s = String::new();
        self.write_pretty(&mut s, indent, 0);
        s
    }

    fn write_pretty(&self, out: &mut String, indent: &str, depth: usize) {
        let pad = |out: &mut String, n: usize| {
            for _ in 0..n {
                out.push_str(indent);
            }
        };
        match self {
            Json::Array(items) if !items.is_empty() => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    pad(out, depth + 1);
                    it.write_pretty(out, indent, depth + 1);
                }
                out.push('\n');
                pad(out, depth);
                out.push(']');
            }
            Json::Object(members) if !members.is_empty() => {
                out.push('{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    pad(out, depth + 1);
                    write_json_string(k, out);
                    out.push_str(": ");
                    v.write_pretty(out, indent, depth + 1);
                }
                out.push('\n');
                pad(out, depth);
                out.push('}');
            }
            // Scalars and empty containers render the same as the compact form.
            _ => self.write(out),
        }
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::Real(r) if r.is_infinite() => {
                // JSON has no infinity literal; sqlite renders it as `±9e999`
                // (a value that round-trips to f64 infinity).
                out.push_str(if *r < 0.0 { "-9e999" } else { "9e999" });
            }
            Json::Real(r) => out.push_str(&crate::exec::eval::format_real(*r)),
            Json::Str(s) => write_json_string(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    it.write(out);
                }
                out.push(']');
            }
            Json::Object(members) => {
                out.push('{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Append a JSON-escaped string literal (with surrounding quotes).
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u");
                for shift in [12, 8, 4, 0] {
                    let nib = (c as u32 >> shift) & 0xf;
                    out.push(char::from_digit(nib, 16).unwrap());
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse JSON text into a [`Json`], or `None` if it is not valid JSON (trailing
/// non-whitespace also fails).
pub fn parse(text: &str) -> Option<Json> {
    parse_with_error_position(text).ok()
}

/// Parse JSON text, returning the value on success or, on failure, the 0-based
/// byte offset of the first syntax error.
///
/// The offset is chosen to track the position the `sqlite3` CLI reports from
/// `json_error_position(X)` (which is the 1-based form, i.e. this value + 1)
/// for the common malformed-JSON shapes: truncated objects/arrays, missing
/// separators, bad tokens in value position, and unterminated strings.
pub fn parse_with_error_position(text: &str) -> Result<Json, usize> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    // An empty or whitespace-only document has no value at all; sqlite3 reports
    // position 1 (offset 0) here rather than the post-whitespace offset.
    if p.peek().is_none() {
        return Err(0);
    }
    let v = p.value()?;
    p.skip_ws();
    if p.pos == p.bytes.len() {
        Ok(v)
    } else {
        // Trailing non-whitespace: report the start of the document, matching
        // sqlite3's reporting for the common trailing-junk case.
        Err(0)
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// The current position as a parse error.
    fn err<T>(&self) -> Result<T, usize> {
        Err(self.pos)
    }

    /// Skip whitespace and JSON5 comments (`// line` and `/* block */`). An
    /// unterminated block comment is left for the caller to surface as an error
    /// at the value position.
    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'/') => match self.bytes.get(self.pos + 1) {
                    Some(b'/') => {
                        self.pos += 2;
                        while let Some(c) = self.peek() {
                            self.pos += 1;
                            if c == b'\n' {
                                break;
                            }
                        }
                    }
                    Some(b'*') => {
                        self.pos += 2;
                        // Scan to the closing `*/`; stop at EOF if unterminated.
                        while self.pos < self.bytes.len() {
                            if self.bytes[self.pos] == b'*'
                                && self.bytes.get(self.pos + 1) == Some(&b'/')
                            {
                                self.pos += 2;
                                break;
                            }
                            self.pos += 1;
                        }
                    }
                    _ => break,
                },
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Result<Json, usize> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            // JSON5 allows single-quoted strings as well as double-quoted.
            Some(b'"') => self.string(b'"').map(Json::Str),
            Some(b'\'') => self.string(b'\'').map(Json::Str),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            // JSON5 `Infinity` / `NaN` (a leading `+`/`-` is consumed by
            // `number`). `NaN` maps to JSON `null`, matching sqlite.
            Some(b'I') => self.literal("Infinity", Json::Real(f64::INFINITY)),
            Some(b'N') => self.literal("NaN", Json::Null),
            // JSON5 numbers: leading `+`, hex (`0x…`), a leading or trailing
            // decimal point, and the signed `Infinity`/`NaN` forms.
            Some(b'-' | b'+' | b'.' | b'0'..=b'9') => self.number(),
            _ => self.err(),
        }
    }

    fn literal(&mut self, word: &str, val: Json) -> Result<Json, usize> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(val)
        } else {
            self.err()
        }
    }

    fn object(&mut self) -> Result<Json, usize> {
        self.pos += 1; // '{'
        let mut members = Vec::new();
        loop {
            self.skip_ws();
            // Empty object, or a single trailing comma before `}`.
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Ok(Json::Object(members));
            }
            let key = self.object_key()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return self.err();
            }
            self.pos += 1;
            let val = self.value()?;
            members.push((key, val));
            self.skip_ws();
            match self.peek() {
                // A comma may be followed by another member or, in JSON5, by the
                // closing `}` (one trailing comma); the loop re-checks for `}`.
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(members));
                }
                _ => return self.err(),
            }
        }
    }

    /// Parse an object key: a double- or single-quoted string, or a JSON5 bare
    /// identifier (`[A-Za-z_$][A-Za-z0-9_$]*`).
    fn object_key(&mut self) -> Result<String, usize> {
        match self.peek() {
            Some(b'"') => self.string(b'"'),
            Some(b'\'') => self.string(b'\''),
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                // ASCII-only run, so this is always valid UTF-8.
                Ok(core::str::from_utf8(&self.bytes[start..self.pos])
                    .unwrap_or("")
                    .to_string())
            }
            // Not a key: scan any identifier run so the reported position matches
            // sqlite3, then fail.
            _ => {
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.err()
            }
        }
    }

    fn array(&mut self) -> Result<Json, usize> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            // Empty array, or a single trailing comma before `]`.
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Ok(Json::Array(items));
            }
            let val = self.value()?;
            items.push(val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return self.err(),
            }
        }
    }

    /// Parse a string literal opened by `quote` (`"` or, in JSON5, `'`). Both
    /// quote styles share the same escapes; `\'` and `\"` are always accepted.
    fn string(&mut self, quote: u8) -> Result<String, usize> {
        self.pos += 1; // opening quote
        let mut s = String::new();
        loop {
            let Some(c) = self.peek() else {
                return self.err();
            };
            if c == quote {
                self.pos += 1;
                return Ok(s);
            }
            self.pos += 1;
            match c {
                b'\\' => {
                    // The escape introducer position, used to report bad escapes
                    // at the escape character itself (matching sqlite3).
                    let esc_pos = self.pos;
                    let Some(esc) = self.peek() else {
                        return self.err();
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => s.push('"'),
                        b'\'' => s.push('\''),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        // JSON5: `\v` maps to U+0009 in sqlite (not U+000B).
                        b't' | b'v' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'b' => s.push('\u{08}'),
                        b'f' => s.push('\u{0c}'),
                        // JSON5: `\0` is a NUL.
                        b'0' => s.push('\0'),
                        // JSON5: a backslash before a line terminator is a line
                        // continuation (the newline is dropped). `\r\n` counts
                        // as one terminator.
                        b'\n' => {}
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        // JSON5: `\xHH` is a two-digit hex escape.
                        b'x' => {
                            let hi = self.hex_digit()?;
                            let lo = self.hex_digit()?;
                            s.push((hi * 16 + lo) as u8 as char);
                        }
                        b'u' => {
                            let cp = self.hex4()?;
                            // Surrogate pair?
                            if (0xD800..=0xDBFF).contains(&cp) {
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    if self.peek() == Some(b'u') {
                                        self.pos += 1;
                                        let lo = self.hex4()?;
                                        let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                        match char::from_u32(c) {
                                            Some(ch) => s.push(ch),
                                            None => return self.err(),
                                        }
                                    } else {
                                        return self.err();
                                    }
                                } else {
                                    return self.err();
                                }
                            } else {
                                match char::from_u32(cp) {
                                    Some(ch) => s.push(ch),
                                    None => return self.err(),
                                }
                            }
                        }
                        // An unrecognized escape: report at the escape character.
                        _ => return Err(esc_pos),
                    }
                }
                // A raw multi-byte UTF-8 sequence: copy the bytes through.
                0x80..=0xFF => {
                    let start = self.pos - 1;
                    while self.peek().is_some_and(|b| b >= 0x80) {
                        self.pos += 1;
                    }
                    match core::str::from_utf8(&self.bytes[start..self.pos]) {
                        Ok(chunk) => s.push_str(chunk),
                        Err(_) => return self.err(),
                    }
                }
                c => s.push(c as char),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, usize> {
        let mut v = 0u32;
        for _ in 0..4 {
            v = v * 16 + self.hex_digit()?;
        }
        Ok(v)
    }

    /// Consume one hexadecimal digit (`0-9A-Fa-f`), or fail at the current
    /// position.
    fn hex_digit(&mut self) -> Result<u32, usize> {
        let Some(c) = self.peek() else {
            return self.err();
        };
        let Some(d) = (c as char).to_digit(16) else {
            return self.err();
        };
        self.pos += 1;
        Ok(d)
    }

    /// Parse a number. Beyond RFC-8259 this accepts the JSON5 forms: a leading
    /// `+`, hexadecimal (`0x…`), a leading or trailing decimal point, and the
    /// `Infinity`/`NaN` literals (with an optional sign). `NaN` becomes JSON
    /// `null`, matching sqlite.
    fn number(&mut self) -> Result<Json, usize> {
        let start = self.pos;
        let neg = match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                true
            }
            Some(b'+') => {
                self.pos += 1;
                false
            }
            _ => false,
        };
        // Signed `Infinity` / `NaN`.
        match self.peek() {
            Some(b'I') => {
                return self.literal(
                    "Infinity",
                    Json::Real(if neg {
                        f64::NEG_INFINITY
                    } else {
                        f64::INFINITY
                    }),
                );
            }
            Some(b'N') => return self.literal("NaN", Json::Null),
            _ => {}
        }
        // Hexadecimal integer (`0x…` / `0X…`).
        if self.peek() == Some(b'0') && matches!(self.bytes.get(self.pos + 1), Some(b'x' | b'X')) {
            self.pos += 2;
            let digits_start = self.pos;
            let mut v: u64 = 0;
            while let Some(c) = self.peek() {
                let Some(d) = (c as char).to_digit(16) else {
                    break;
                };
                v = v.wrapping_mul(16).wrapping_add(d as u64);
                self.pos += 1;
            }
            if self.pos == digits_start {
                return Err(start); // `0x` with no digits
            }
            // SQLite stores the hex value modulo 2^64 as a signed integer.
            let i = v as i64;
            return Ok(Json::Int(if neg { i.wrapping_neg() } else { i }));
        }
        let mut is_real = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'+' | b'-' => {
                    is_real = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        if self.pos == start || (neg && self.pos == start + 1) {
            return Err(start); // no digits consumed (e.g. bare `+`/`-`/`.`)
        }
        let Ok(tok) = core::str::from_utf8(&self.bytes[start..self.pos]) else {
            return Err(start);
        };
        if !is_real {
            if let Ok(i) = tok.parse::<i64>() {
                return Ok(Json::Int(i));
            }
        }
        // `f64::parse` rejects a leading `+` and a leading/trailing `.`; build a
        // normalized form so JSON5 numbers like `+.5` / `5.` parse.
        match parse_json5_real(tok) {
            Some(r) => Ok(Json::Real(r)),
            None => Err(start),
        }
    }
}

/// Parse a JSON5 real-number token. `f64::from_str` already accepts a leading or
/// trailing decimal point (`.5`, `5.`, `5.e2`); the only extra JSON5 form is a
/// leading `+`, which it rejects, so strip it first.
fn parse_json5_real(tok: &str) -> Option<f64> {
    let tok = tok.strip_prefix('+').unwrap_or(tok);
    tok.parse::<f64>().ok()
}

/// Navigate a JSON value by a SQLite path expression (`$`, `.key`, `[index]`).
/// Returns `None` if the path does not resolve. Only the common subset is
/// supported: object keys via `.name` or `."name"`, array elements via `[n]`.
pub fn navigate<'a>(root: &'a Json, path: &str) -> Option<&'a Json> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }
    let mut cur = root;
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                let (key, next) = parse_key(bytes, i)?;
                i = next;
                let Json::Object(members) = cur else {
                    return None;
                };
                cur = members.iter().find(|(k, _)| *k == key).map(|(_, v)| v)?;
            }
            b'[' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                let idx_str = core::str::from_utf8(&bytes[start..i]).ok()?;
                i += 1; // ']'
                let Json::Array(items) = cur else {
                    return None;
                };
                let n: usize = idx_str.trim().parse().ok()?;
                cur = items.get(n)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// Parse an object-key path segment starting at `i`, returning `(key, next_i)`.
/// Handles a bare identifier or a `"quoted"` key.
fn parse_key(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    if bytes.get(i) == Some(&b'"') {
        // Quoted key: read until the closing quote.
        let mut j = i + 1;
        let mut s = String::new();
        while j < bytes.len() && bytes[j] != b'"' {
            s.push(bytes[j] as char);
            j += 1;
        }
        if j >= bytes.len() {
            return None;
        }
        Some((s, j + 1))
    } else {
        let start = i;
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'.' && bytes[j] != b'[' {
            j += 1;
        }
        if j == start {
            return None;
        }
        let key = core::str::from_utf8(&bytes[start..j]).ok()?.to_string();
        Some((key, j))
    }
}

/// Convert a SQL [`Value`] to a [`Json`] for `json_array`/`json_object`. A text
/// value that is itself valid JSON is *not* re-parsed here (SQLite only treats
/// arguments as JSON when wrapped in `json()`); plain text becomes a JSON string.
pub fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Integer(i) => Json::Int(*i),
        Value::Real(r) => Json::Real(*r),
        Value::Text(s) => Json::Str(s.clone()),
        Value::Blob(_) => Json::Str(String::new()),
    }
}

/// The `->` / `->>` operators. `doc` is the JSON document, `path_arg` the right
/// operand (a JSON path, a bare object label, or an integer array index). With
/// `as_text` (the `->>` form) the result is the SQL value at the path; otherwise
/// (`->`) it is that node re-rendered as JSON text. NULL doc or a missing path
/// yields SQL NULL.
pub fn arrow(doc: &Value, path_arg: &Value, as_text: bool) -> Value {
    if matches!(doc, Value::Null) {
        return Value::Null;
    }
    let text = crate::exec::eval::to_text(doc);
    let Some(root) = parse(&text) else {
        return Value::Null;
    };
    let path = arrow_path(path_arg);
    match navigate(&root, &path) {
        None => Value::Null,
        Some(node) => {
            if as_text {
                node.to_sql()
            } else {
                Value::Text(node.serialize())
            }
        }
    }
}

/// Normalize an `->`/`->>` right operand into a JSON path: an integer is an array
/// index `$[n]`, a string starting with `$` is used verbatim, any other string is
/// an object label `$.label`.
fn arrow_path(v: &Value) -> String {
    match v {
        Value::Integer(i) => alloc::format!("$[{i}]"),
        Value::Text(s) if s.starts_with('$') => s.clone(),
        Value::Text(s) => alloc::format!("$.{s}"),
        other => alloc::format!("$.{}", crate::exec::eval::to_text(other)),
    }
}

/// One step of a JSON path: an object key or an array index.
enum Seg {
    Key(String),
    Index(usize),
}

/// Parse a `$`-rooted path into its segments (the subset used by the mutators:
/// `.key`, `."key"`, `[n]`). Returns `None` on a malformed path.
fn parse_path(path: &str) -> Option<Vec<Seg>> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }
    let mut segs = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                let (key, next) = parse_key(bytes, i + 1)?;
                segs.push(Seg::Key(key));
                i = next;
            }
            b'[' => {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b']' {
                    j += 1;
                }
                if j >= bytes.len() {
                    return None;
                }
                let n: usize = core::str::from_utf8(&bytes[start..j])
                    .ok()?
                    .trim()
                    .parse()
                    .ok()?;
                segs.push(Seg::Index(n));
                i = j + 1;
            }
            _ => return None,
        }
    }
    Some(segs)
}

/// How a `json_set`/`json_insert`/`json_replace` write applies at the leaf.
#[derive(Clone, Copy, PartialEq)]
pub enum SetMode {
    /// `json_set`: create or overwrite.
    Set,
    /// `json_insert`: only create when absent.
    Insert,
    /// `json_replace`: only overwrite when present.
    Replace,
}

/// Apply one `(path, value)` write to `root` under `mode`. Missing parent
/// containers are left untouched (matching SQLite, which only writes the leaf).
pub fn set_path(root: &mut Json, path: &str, value: Json, mode: SetMode) -> Option<()> {
    let segs = parse_path(path)?;
    if segs.is_empty() {
        if mode != SetMode::Insert {
            *root = value; // `$` targets the whole document
        }
        return Some(());
    }
    let mut cur = root;
    for seg in &segs[..segs.len() - 1] {
        cur = match (cur, seg) {
            (Json::Object(m), Seg::Key(k)) => {
                m.iter_mut().find(|(kk, _)| kk == k).map(|(_, v)| v)?
            }
            (Json::Array(a), Seg::Index(i)) => a.get_mut(*i)?,
            _ => return None,
        };
    }
    match (cur, segs.last()?) {
        (Json::Object(m), Seg::Key(k)) => {
            let slot = m.iter_mut().find(|(kk, _)| kk == k);
            match (slot, mode) {
                (Some(s), SetMode::Set) | (Some(s), SetMode::Replace) => s.1 = value,
                (None, SetMode::Set) | (None, SetMode::Insert) => m.push((k.clone(), value)),
                _ => {}
            }
        }
        (Json::Array(a), Seg::Index(i)) => {
            let exists = *i < a.len();
            match mode {
                SetMode::Set if exists => a[*i] = value,
                SetMode::Replace if exists => a[*i] = value,
                SetMode::Set | SetMode::Insert if !exists => a.push(value),
                _ => {}
            }
        }
        _ => return None,
    }
    Some(())
}

/// Remove the element at `path` from `root` if present.
pub fn remove_path(root: &mut Json, path: &str) -> Option<()> {
    let segs = parse_path(path)?;
    if segs.is_empty() {
        return None; // `$` cannot be removed
    }
    let mut cur = root;
    for seg in &segs[..segs.len() - 1] {
        cur = match (cur, seg) {
            (Json::Object(m), Seg::Key(k)) => {
                m.iter_mut().find(|(kk, _)| kk == k).map(|(_, v)| v)?
            }
            (Json::Array(a), Seg::Index(i)) => a.get_mut(*i)?,
            _ => return None,
        };
    }
    match (cur, segs.last()?) {
        (Json::Object(m), Seg::Key(k)) => m.retain(|(kk, _)| kk != k),
        (Json::Array(a), Seg::Index(i)) if *i < a.len() => {
            a.remove(*i);
        }
        _ => {}
    }
    Some(())
}

/// Apply an RFC 7396 JSON merge patch: object members merge recursively, a member
/// whose patch value is `null` is removed, and a non-object patch replaces the
/// target outright.
pub fn merge_patch(target: &mut Json, patch: &Json) {
    let Json::Object(pm) = patch else {
        *target = patch.clone();
        return;
    };
    if !matches!(target, Json::Object(_)) {
        *target = Json::Object(Vec::new());
    }
    let Json::Object(tm) = target else {
        return;
    };
    for (k, pv) in pm {
        if matches!(pv, Json::Null) {
            tm.retain(|(kk, _)| kk != k);
        } else if let Some(slot) = tm.iter_mut().find(|(kk, _)| kk == k) {
            merge_patch(&mut slot.1, pv);
        } else {
            tm.push((k.clone(), Json::Null));
            let slot = tm.last_mut().unwrap();
            merge_patch(&mut slot.1, pv);
        }
    }
}
