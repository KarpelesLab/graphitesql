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
    /// An integral number that fits in `i64`. `Some` carries the verbatim source
    /// text of a JSON5-only integer form (a hexadecimal `0x…` literal), which
    /// SQLite stores under the JSONB `INT5` tag; `None` is a strict decimal
    /// integer (`INT`, rendered canonically).
    Int(i64, Option<String>),
    /// Any other number. The optional string is the verbatim source text of the
    /// number as it appeared in the input. SQLite preserves a *strict* number's
    /// text in JSON output (`json('1e2')` → `1e2`, `json('1.50')` → `1.50`) under
    /// the `FLOAT` tag, and keeps a JSON5-only form (a leading/trailing `.`, e.g.
    /// `.5` / `5.`) verbatim under the `FLOAT5` tag while rendering it canonically
    /// in `json()`. `None` is a number built programmatically (a SQL REAL,
    /// arithmetic, `Infinity`) or one normalized from a leading-`+` form.
    Real(f64, Option<String>),
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
            Json::Int(..) => "integer",
            Json::Real(..) => "real",
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
            Json::Int(i, _) => Value::Integer(*i),
            Json::Real(r, _) => Value::Real(*r),
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

    /// Serialize as `json_quote(X)` does. Identical to [`serialize`](Self::serialize)
    /// except that a non-finite REAL value renders as SQLite's quoted-literal form
    /// `±9.0e+999` (whereas a JSON-literal `9e999` round-trips verbatim through
    /// [`serialize`](Self::serialize)). `json_quote` only ever sees a scalar.
    pub fn quote(&self) -> String {
        if let Json::Real(r, _) = self {
            if r.is_infinite() {
                return String::from(if *r < 0.0 { "-9.0e+999" } else { "9.0e+999" });
            }
        }
        self.serialize()
    }

    /// Encode to SQLite's **JSONB** binary format (the on-disk representation
    /// behind `jsonb()` and the `jsonb_*` functions). Each element is a header
    /// byte — low nibble = type, high nibble = payload-size class — followed by
    /// the payload: ASCII digits for numbers, the (possibly escaped) bytes for
    /// strings, and concatenated child elements for arrays/objects.
    pub fn to_jsonb(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_jsonb(&mut out);
        out
    }

    fn write_jsonb(&self, out: &mut Vec<u8>) {
        match self {
            Json::Null => push_jsonb(out, JSONB_NULL, &[]),
            Json::Bool(true) => push_jsonb(out, JSONB_TRUE, &[]),
            Json::Bool(false) => push_jsonb(out, JSONB_FALSE, &[]),
            // A hexadecimal source form is stored verbatim under INT5; a strict
            // decimal integer is the canonical text under INT.
            Json::Int(_, Some(raw)) => push_jsonb(out, JSONB_INT5, raw.as_bytes()),
            Json::Int(i, None) => push_jsonb(out, JSONB_INT, i.to_string().as_bytes()),
            Json::Real(r, src) => {
                // The payload is the number's source text when known, so a JSONB
                // round-trip preserves it like SQLite. A *strict* number uses the
                // FLOAT tag; a JSON5-only form (a leading/trailing `.`) keeps its
                // raw text under FLOAT5.
                match src {
                    Some(t) if is_strict_json_number(t) => {
                        push_jsonb(out, JSONB_FLOAT, t.as_bytes())
                    }
                    Some(t) => push_jsonb(out, JSONB_FLOAT5, t.as_bytes()),
                    None if r.is_infinite() => {
                        let s = if *r < 0.0 { "-9e999" } else { "9e999" };
                        push_jsonb(out, JSONB_FLOAT, s.as_bytes());
                    }
                    None => push_jsonb(
                        out,
                        JSONB_FLOAT,
                        crate::exec::eval::format_real(*r).as_bytes(),
                    ),
                }
            }
            Json::Str(s) => {
                // A string with no characters needing escapes is stored raw
                // (TEXT); otherwise its JSON-escaped body is stored as TEXTJ.
                if json_needs_escape(s) {
                    push_jsonb(out, JSONB_TEXTJ, json_escape_body(s).as_bytes());
                } else {
                    push_jsonb(out, JSONB_TEXT, s.as_bytes());
                }
            }
            Json::Array(items) => {
                let mut body = Vec::new();
                for it in items {
                    it.write_jsonb(&mut body);
                }
                push_jsonb(out, JSONB_ARRAY, &body);
            }
            Json::Object(members) => {
                let mut body = Vec::new();
                for (k, v) in members {
                    if json_needs_escape(k) {
                        push_jsonb(&mut body, JSONB_TEXTJ, json_escape_body(k).as_bytes());
                    } else {
                        push_jsonb(&mut body, JSONB_TEXT, k.as_bytes());
                    }
                    v.write_jsonb(&mut body);
                }
                push_jsonb(out, JSONB_OBJECT, &body);
            }
        }
    }

    /// Decode a complete JSONB value, returning `None` on malformed input or
    /// trailing bytes.
    pub fn from_jsonb(bytes: &[u8]) -> Option<Json> {
        let (j, rest) = decode_jsonb(bytes)?;
        rest.is_empty().then_some(j)
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
            Json::Int(i, _) => out.push_str(&i.to_string()),
            Json::Real(r, _) if r.is_infinite() => {
                // JSON has no infinity literal; sqlite renders it as `±9e999`
                // (a value that round-trips to f64 infinity).
                out.push_str(if *r < 0.0 { "-9e999" } else { "9e999" });
            }
            // Preserve a *strict* number's verbatim source text (`1e2`, `1.50`,
            // `-0.0`); a JSON5-only form (`.5`, `5.`) and a computed value render
            // in canonical form (`.5` → `0.5`), matching sqlite's `json()`.
            Json::Real(_, Some(src)) if is_strict_json_number(src) => out.push_str(src),
            Json::Real(r, _) => out.push_str(&crate::exec::eval::format_real(*r)),
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

// JSONB element type tags (the low nibble of each element's header byte).
const JSONB_NULL: u8 = 0;
const JSONB_TRUE: u8 = 1;
const JSONB_FALSE: u8 = 2;
const JSONB_INT: u8 = 3;
const JSONB_INT5: u8 = 4;
const JSONB_FLOAT: u8 = 5;
const JSONB_FLOAT5: u8 = 6;
const JSONB_TEXT: u8 = 7;
const JSONB_TEXTJ: u8 = 8;
const JSONB_TEXT5: u8 = 9;
const JSONB_TEXTRAW: u8 = 10;
const JSONB_ARRAY: u8 = 11;
const JSONB_OBJECT: u8 = 12;

/// Append one JSONB element: the header byte (type in the low nibble, payload-
/// size class in the high nibble) plus any size bytes, then the payload. Sizes
/// 0–11 fit in the nibble; larger ones spill into 1/2/4/8 big-endian bytes.
fn push_jsonb(out: &mut Vec<u8>, ty: u8, payload: &[u8]) {
    let n = payload.len();
    if n <= 11 {
        out.push(((n as u8) << 4) | ty);
    } else if n <= 0xFF {
        out.push((12 << 4) | ty);
        out.push(n as u8);
    } else if n <= 0xFFFF {
        out.push((13 << 4) | ty);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= 0xFFFF_FFFF {
        out.push((14 << 4) | ty);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push((15 << 4) | ty);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
}

/// Decode one JSONB element, returning it and the remaining bytes.
fn decode_jsonb(b: &[u8]) -> Option<(Json, &[u8])> {
    let header = *b.first()?;
    let ty = header & 0x0F;
    let mut rest = &b[1..];
    let n = match header >> 4 {
        d @ 0..=11 => d as usize,
        12 => {
            let v = *rest.first()? as usize;
            rest = &rest[1..];
            v
        }
        13 => {
            let v = u16::from_be_bytes([*rest.first()?, *rest.get(1)?]) as usize;
            rest = &rest[2..];
            v
        }
        14 => {
            let mut a = [0u8; 4];
            a.copy_from_slice(rest.get(..4)?);
            rest = &rest[4..];
            u32::from_be_bytes(a) as usize
        }
        _ => {
            let mut a = [0u8; 8];
            a.copy_from_slice(rest.get(..8)?);
            rest = &rest[8..];
            u64::from_be_bytes(a) as usize
        }
    };
    let payload = rest.get(..n)?;
    let after = &rest[n..];
    let text = || core::str::from_utf8(payload).ok().map(String::from);
    let j = match ty {
        JSONB_NULL => Json::Null,
        JSONB_TRUE => Json::Bool(true),
        JSONB_FALSE => Json::Bool(false),
        JSONB_INT | JSONB_INT5 | JSONB_FLOAT | JSONB_FLOAT5 => {
            // The number's text payload is reparsed through the JSON number path.
            let t = core::str::from_utf8(payload).ok()?;
            match parse(t) {
                Some(j @ (Json::Int(..) | Json::Real(..))) => j,
                _ => return None,
            }
        }
        JSONB_TEXT | JSONB_TEXTRAW => Json::Str(text()?),
        JSONB_TEXTJ | JSONB_TEXT5 => {
            // Reparse the escaped body as a quoted JSON string.
            let mut quoted = String::with_capacity(n + 2);
            quoted.push('"');
            quoted.push_str(core::str::from_utf8(payload).ok()?);
            quoted.push('"');
            match parse(&quoted)? {
                Json::Str(s) => Json::Str(s),
                _ => return None,
            }
        }
        JSONB_ARRAY => {
            let mut items = Vec::new();
            let mut p = payload;
            while !p.is_empty() {
                let (it, next) = decode_jsonb(p)?;
                items.push(it);
                p = next;
            }
            Json::Array(items)
        }
        JSONB_OBJECT => {
            let mut members = Vec::new();
            let mut p = payload;
            while !p.is_empty() {
                let (label, next) = decode_jsonb(p)?;
                let (val, next2) = decode_jsonb(next)?;
                let key = match label {
                    Json::Str(s) => s,
                    _ => return None,
                };
                members.push((key, val));
                p = next2;
            }
            Json::Object(members)
        }
        _ => return None,
    };
    Some((j, after))
}

/// Whether a string contains any character requiring a JSON escape.
fn json_needs_escape(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '"' | '\\') || (c as u32) < 0x20)
}

/// The JSON-escaped body of a string *without* the surrounding quotes (the
/// TEXTJ payload form).
fn json_escape_body(s: &str) -> String {
    let mut q = String::new();
    write_json_string(s, &mut q);
    // Strip the surrounding quotes `write_json_string` adds.
    q[1..q.len() - 1].to_string()
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

/// Validate that `text` is well-formed *strict* RFC-8259 JSON (no JSON5
/// extensions). `json_valid(X)` (the 1-argument form) is restricted to strict
/// JSON in sqlite, whereas the JSON *functions* (`json`, `json_extract`, …)
/// accept the JSON5 superset via [`parse`]. Kept as a separate, self-contained
/// validator so the lenient JSON5 parser is unaffected.
pub fn is_strict_json(text: &str) -> bool {
    let mut p = StrictParser {
        b: text.as_bytes(),
        i: 0,
    };
    p.ws();
    p.value() && {
        p.ws();
        p.i == p.b.len()
    }
}

struct StrictParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl StrictParser<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn lit(&mut self, w: &str) -> bool {
        if self.b[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            true
        } else {
            false
        }
    }
    fn value(&mut self) -> bool {
        self.ws();
        match self.b.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string(),
            Some(b't') => self.lit("true"),
            Some(b'f') => self.lit("false"),
            Some(b'n') => self.lit("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => false,
        }
    }
    fn object(&mut self) -> bool {
        self.i += 1; // '{'
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return true;
        }
        loop {
            self.ws();
            // Strict: keys are double-quoted strings only (no trailing comma).
            if self.b.get(self.i) != Some(&b'"') || !self.string() {
                return false;
            }
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return false;
            }
            self.i += 1;
            if !self.value() {
                return false;
            }
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1, // must be followed by another member
                Some(b'}') => {
                    self.i += 1;
                    return true;
                }
                _ => return false,
            }
        }
    }
    fn array(&mut self) -> bool {
        self.i += 1; // '['
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return true;
        }
        loop {
            if !self.value() {
                return false;
            }
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1, // must be followed by another element
                Some(b']') => {
                    self.i += 1;
                    return true;
                }
                _ => return false,
            }
        }
    }
    fn string(&mut self) -> bool {
        self.i += 1; // opening '"'
        loop {
            match self.b.get(self.i) {
                None => return false,
                Some(b'"') => {
                    self.i += 1;
                    return true;
                }
                Some(b'\\') => {
                    self.i += 1;
                    match self.b.get(self.i) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 1,
                        Some(b'u') => {
                            self.i += 1;
                            for _ in 0..4 {
                                match self.b.get(self.i) {
                                    Some(c) if c.is_ascii_hexdigit() => self.i += 1,
                                    _ => return false,
                                }
                            }
                        }
                        _ => return false,
                    }
                }
                // Unescaped control characters are not allowed in strict JSON.
                Some(&c) if c < 0x20 => return false,
                Some(_) => self.i += 1,
            }
        }
    }
    fn number(&mut self) -> bool {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1, // a leading zero allows no more int digits
            Some(b'1'..=b'9') => {
                while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                    self.i += 1;
                }
            }
            _ => return false,
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                return false;
            }
            while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                return false;
            }
            while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        self.i > start
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
            Some(b'I') => self.literal("Infinity", Json::Real(f64::INFINITY, None)),
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
                    Json::Real(
                        if neg {
                            f64::NEG_INFINITY
                        } else {
                            f64::INFINITY
                        },
                        None,
                    ),
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
            // SQLite stores the hex value modulo 2^64 as a signed integer, and
            // keeps the verbatim `0x…` text for the JSONB INT5 tag.
            let i = v as i64;
            let raw = core::str::from_utf8(&self.bytes[start..self.pos])
                .ok()
                .map(String::from);
            return Ok(Json::Int(if neg { i.wrapping_neg() } else { i }, raw));
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
                return Ok(Json::Int(i, None));
            }
        }
        // `f64::parse` rejects a leading `+` and a leading/trailing `.`; build a
        // normalized form so JSON5 numbers like `+.5` / `5.` parse.
        match parse_json5_real(tok) {
            // Keep the verbatim text for both strict numbers (`1e2`, `1.50`) and
            // JSON5 leading/trailing-`.` forms (`.5`, `5.`) — the encoders pick the
            // FLOAT vs FLOAT5 tag and the canonical-vs-verbatim `json()` rendering
            // by re-checking `is_strict_json_number`. A leading-`+` form is dropped
            // to `None` so it normalizes (`+0.5` → `0.5`), matching sqlite.
            Some(r) => Ok(Json::Real(
                r,
                (!tok.starts_with('+')).then(|| String::from(tok)),
            )),
            None => Err(start),
        }
    }
}

/// Whether `t` is a *strict* RFC-8259 number — `-?(0|[1-9]\d*)(\.\d+)?([eE][-+]?\d+)?`.
/// Only such numbers keep their verbatim text in JSON output; JSON5-only forms
/// (leading `+`, leading/trailing `.`, hex) are normalized by the caller.
fn is_strict_json_number(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0;
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    match b.get(i) {
        Some(b'0') => i += 1, // a lone leading zero (no further integer digits)
        Some(c) if c.is_ascii_digit() => {
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return false, // leading `+`/`.` or empty integer part
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !b.get(i).is_some_and(u8::is_ascii_digit) {
            return false; // trailing `.`
        }
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !b.get(i).is_some_and(u8::is_ascii_digit) {
            return false;
        }
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    i == b.len()
}

/// Parse a JSON5 real-number token. `f64::from_str` already accepts a leading or
/// trailing decimal point (`.5`, `5.`, `5.e2`); the only extra JSON5 form is a
/// leading `+`, which it rejects, so strip it first.
fn parse_json5_real(tok: &str) -> Option<f64> {
    let tok = tok.strip_prefix('+').unwrap_or(tok);
    tok.parse::<f64>().ok()
}

/// Navigate a JSON value by a SQLite path expression to a node for reading, or
/// `None` if the path is syntactically bad **or** does not resolve. Callers that
/// must distinguish a bad path (an error) from a merely-missing one (SQL `NULL`)
/// validate the path first with [`path_is_valid`].
///
/// Supports object keys (`.name`, `."name"`), array indices (`[n]`), and the
/// SQLite end-relative forms `[#]` (one past the last element) and `[#-n]`
/// (counting back from the end).
pub fn navigate<'a>(root: &'a Json, path: &str) -> Option<&'a Json> {
    let segs = parse_path(path)?;
    let mut cur = root;
    for seg in &segs {
        match (cur, seg) {
            (Json::Object(members), Seg::Key(k)) => {
                cur = members.iter().find(|(kk, _)| kk == k).map(|(_, v)| v)?;
            }
            (Json::Array(items), Seg::Index(idx)) => {
                let n = idx.resolve_read(items.len())?;
                cur = items.get(n)?;
            }
            // A `.key` step into a non-object, or an `[i]` step into a
            // non-array, simply does not resolve (NULL).
            _ => return None,
        }
    }
    Some(cur)
}

/// Convert a SQL [`Value`] to a [`Json`] for `json_array`/`json_object`. A text
/// value that is itself valid JSON is *not* re-parsed here (SQLite only treats
/// arguments as JSON when wrapped in `json()`); plain text becomes a JSON string.
pub fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Integer(i) => Json::Int(*i, None),
        Value::Real(r) => Json::Real(*r, None),
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
/// index (a non-negative integer is `$[n]`; a negative integer `-k` is the
/// end-relative `$[#-k]`, matching SQLite), a string starting with `$` is used
/// verbatim, any other string is an object label `$.label`.
fn arrow_path(v: &Value) -> String {
    match v {
        Value::Integer(i) if *i < 0 => alloc::format!("$[#-{}]", i.unsigned_abs()),
        Value::Integer(i) => alloc::format!("$[{i}]"),
        Value::Text(s) if s.starts_with('$') => s.clone(),
        Value::Text(s) => alloc::format!("$.{s}"),
        other => alloc::format!("$.{}", crate::exec::eval::to_text(other)),
    }
}

/// An array-index path step. SQLite supports a plain index `[n]`, plus the
/// end-relative forms `[#]` (one past the last element — the append slot) and
/// `[#-k]` (the `k`-th element counting back from the end, `[#-1]` being the
/// last). Negative *literal* indices (`[-1]`) are **not** valid paths.
#[derive(Clone, Copy)]
enum Idx {
    /// A literal index `[n]`.
    Abs(usize),
    /// `[#-k]`; `[#]` is `FromEnd(0)`, i.e. the one-past-end append slot.
    FromEnd(usize),
}

impl Idx {
    /// Resolve to a concrete index for *reading* an array of length `len`, or
    /// `None` if it falls outside `0..len` (`[#]` and `[#-k]` past the start both
    /// miss).
    fn resolve_read(self, len: usize) -> Option<usize> {
        match self {
            Idx::Abs(n) => Some(n),
            // `[#]` (FromEnd(0)) is the append slot — never an existing element.
            Idx::FromEnd(0) => None,
            Idx::FromEnd(k) => len.checked_sub(k),
        }
    }
}

/// One step of a JSON path: an object key or an array index.
enum Seg {
    Key(String),
    Index(Idx),
}

/// Whether `path` is a syntactically valid SQLite JSON path (`$`-rooted, with
/// `.key`/`[n]`/`[#]`/`[#-k]` steps). The JSON scalar functions raise
/// `bad JSON path: '<path>'` when this is false.
pub fn path_is_valid(path: &str) -> bool {
    parse_path(path).is_some()
}

/// Parse a `$`-rooted path into its segments: `.key`, `."key"`, `[n]`, `[#]`,
/// and `[#-k]`. Returns `None` on a malformed path (so the caller can surface
/// SQLite's `bad JSON path` error).
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
                    return None; // unterminated `[`
                }
                let inner = core::str::from_utf8(&bytes[start..j]).ok()?.trim();
                segs.push(Seg::Index(parse_index(inner)?));
                i = j + 1;
            }
            _ => return None,
        }
    }
    Some(segs)
}

/// Parse the contents between `[` and `]`: a non-negative integer, `#`, or
/// `#-k` (with `k` a non-negative integer). Anything else — a negative literal,
/// `#+k`, or junk — is a bad path.
fn parse_index(inner: &str) -> Option<Idx> {
    if let Some(rest) = inner.strip_prefix('#') {
        let rest = rest.trim_start();
        if rest.is_empty() {
            return Some(Idx::FromEnd(0)); // `[#]`
        }
        let k = rest.strip_prefix('-')?.trim_start();
        // Only `#-k` is valid; `#+k` and bare `#5` are not.
        Some(Idx::FromEnd(k.parse::<usize>().ok()?))
    } else {
        // A plain literal index: digits only (no `-`).
        Some(Idx::Abs(inner.parse::<usize>().ok()?))
    }
}

/// Parse an object-key path segment starting at `i`, returning `(key, next_i)`.
/// Handles a bare identifier or a `"quoted"` key. A bare key may not be empty
/// (so `$.` is a bad path).
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

/// Walk `cur` down the interior (non-final) segments of a path, returning a
/// mutable reference to the parent container of the leaf, or `None` if a step
/// does not resolve. The end-relative array forms resolve against the live
/// length at each step.
fn walk_to_parent<'a>(mut cur: &'a mut Json, segs: &[Seg]) -> Option<&'a mut Json> {
    for seg in segs {
        cur = match (cur, seg) {
            (Json::Object(m), Seg::Key(k)) => {
                m.iter_mut().find(|(kk, _)| kk == k).map(|(_, v)| v)?
            }
            (Json::Array(a), Seg::Index(idx)) => {
                let n = idx.resolve_read(a.len())?;
                a.get_mut(n)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Like [`walk_to_parent`], but for `json_set`/`json_insert`: a missing interior
/// object key or array append slot is *created* — as an object when the next
/// segment is a key, an array when it is an index — so a multi-level path like
/// `$.a.b` materializes the intermediate containers, matching SQLite. A segment
/// that resolves to an existing scalar (which cannot be descended) aborts the
/// whole write (`None`), so `json_set('{"a":5}', '$.a.b', 1)` is a no-op.
fn walk_to_parent_creating<'a>(mut cur: &'a mut Json, segs: &[Seg]) -> Option<&'a mut Json> {
    if segs.is_empty() {
        return Some(cur);
    }
    for i in 0..segs.len() - 1 {
        let make = || match &segs[i + 1] {
            Seg::Key(_) => Json::Object(Vec::new()),
            Seg::Index(_) => Json::Array(Vec::new()),
        };
        cur = match (cur, &segs[i]) {
            (Json::Object(m), Seg::Key(k)) => {
                if !m.iter().any(|(kk, _)| kk == k) {
                    m.push((k.clone(), make()));
                }
                m.iter_mut().find(|(kk, _)| kk == k).map(|(_, v)| v)?
            }
            (Json::Array(a), Seg::Index(idx)) => match idx.resolve_read(a.len()) {
                Some(n) if n < a.len() => a.get_mut(n)?,
                // The immediate append slot (`[#]` or `[len]`) grows the array;
                // any other out-of-range index cannot be created.
                _ if matches!(idx, Idx::FromEnd(0))
                    || matches!(idx, Idx::Abs(n) if *n == a.len()) =>
                {
                    a.push(make());
                    a.last_mut()?
                }
                _ => return None,
            },
            _ => return None,
        };
    }
    Some(cur)
}

/// Apply one `(path, value)` write to `root` under `mode`. For `json_set`/
/// `json_insert`, missing intermediate containers along the path are created;
/// `json_replace` only writes where the parent already resolves. Returns `None`
/// if the path is malformed (a `bad JSON path` for the caller).
pub fn set_path(root: &mut Json, path: &str, value: Json, mode: SetMode) -> Option<()> {
    let segs = parse_path(path)?;
    if segs.is_empty() {
        if mode != SetMode::Insert {
            *root = value; // `$` targets the whole document
        }
        return Some(());
    }
    let (last, parents) = segs.split_last()?;
    if mode == SetMode::Replace {
        // Replace never creates, so it writes directly with no orphan risk.
        if let Some(cur) = walk_to_parent(root, parents) {
            write_leaf(cur, last, value, mode);
        }
        return Some(());
    }
    // json_set / json_insert create missing intermediates, but the write is
    // *atomic*: build it on a clone and commit only if the leaf actually writes,
    // so a no-op leaf (e.g. an out-of-range index) leaves the document — and the
    // would-be intermediates — unchanged, exactly as SQLite does.
    let mut work = root.clone();
    if let Some(cur) = walk_to_parent_creating(&mut work, &segs) {
        if write_leaf(cur, last, value, mode) {
            *root = work;
        }
    }
    Some(())
}

/// Write `value` at the leaf segment `last` of the already-resolved parent `cur`
/// under `mode`. Returns whether a write actually happened (so an atomic caller
/// can discard created intermediates on a no-op).
fn write_leaf(cur: &mut Json, last: &Seg, value: Json, mode: SetMode) -> bool {
    match (cur, last) {
        (Json::Object(m), Seg::Key(k)) => {
            let slot = m.iter_mut().find(|(kk, _)| kk == k);
            match (slot, mode) {
                (Some(s), SetMode::Set) | (Some(s), SetMode::Replace) => {
                    s.1 = value;
                    true
                }
                (None, SetMode::Set) | (None, SetMode::Insert) => {
                    m.push((k.clone(), value));
                    true
                }
                _ => false,
            }
        }
        (Json::Array(a), Seg::Index(idx)) => {
            // An in-range index overwrites; the append slot — `[#]` or a literal
            // `[len]` one past the last element — grows the array for json_set/
            // json_insert. A literal index further past the end, or `[#-k]` past
            // the start, is a no-op (SQLite does not grow the array for it).
            let len = a.len();
            match idx.resolve_read(len) {
                Some(n) if n < len => match mode {
                    SetMode::Set | SetMode::Replace => {
                        a[n] = value;
                        true
                    }
                    SetMode::Insert => false,
                },
                _ => {
                    let is_append =
                        matches!(idx, Idx::FromEnd(0)) || matches!(idx, Idx::Abs(n) if *n == len);
                    if is_append && matches!(mode, SetMode::Set | SetMode::Insert) {
                        a.push(value);
                        true
                    } else {
                        false
                    }
                }
            }
        }
        _ => false,
    }
}

/// Remove the element at `path` from `root` if present. Returns `None` if the
/// path is malformed.
pub fn remove_path(root: &mut Json, path: &str) -> Option<()> {
    let segs = parse_path(path)?;
    let (last, parents) = segs.split_last()?; // `$` (empty) cannot be removed
    let Some(cur) = walk_to_parent(root, parents) else {
        return Some(());
    };
    match (cur, last) {
        (Json::Object(m), Seg::Key(k)) => m.retain(|(kk, _)| kk != k),
        (Json::Array(a), Seg::Index(idx)) => {
            if let Some(n) = idx.resolve_read(a.len()) {
                if n < a.len() {
                    a.remove(n);
                }
            }
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
