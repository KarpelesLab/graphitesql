//! A minimal JSON value model with the parser, serializer, and path navigation
//! behind SQLite's core JSON functions (`json`, `json_extract`, `json_type`, …).
//!
//! This is RFC-8259 JSON with SQLite's conventions for the scalar mapping back
//! to SQL values: JSON `true`/`false` become integers `1`/`0`, `null` becomes
//! SQL `NULL`, numbers become INTEGER or REAL, strings become TEXT, and
//! objects/arrays are returned as their minified JSON text.

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
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.pos == p.bytes.len() {
        Some(v)
    } else {
        None
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

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Json::Str),
            b't' => self.literal("true", Json::Bool(true)),
            b'f' => self.literal("false", Json::Bool(false)),
            b'n' => self.literal("null", Json::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn literal(&mut self, word: &str, val: Json) -> Option<Json> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Some(val)
        } else {
            None
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.pos += 1; // '{'
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(Json::Object(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return None;
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return None;
            }
            self.pos += 1;
            let val = self.value()?;
            members.push((key, val));
            self.skip_ws();
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(Json::Object(members));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(Json::Array(items));
        }
        loop {
            let val = self.value()?;
            items.push(val);
            self.skip_ws();
            match self.peek()? {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    return Some(Json::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        self.pos += 1; // opening quote
        let mut s = String::new();
        loop {
            let c = self.peek()?;
            self.pos += 1;
            match c {
                b'"' => return Some(s),
                b'\\' => {
                    let esc = self.peek()?;
                    self.pos += 1;
                    match esc {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'b' => s.push('\u{08}'),
                        b'f' => s.push('\u{0c}'),
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
                                        s.push(char::from_u32(c)?);
                                    } else {
                                        return None;
                                    }
                                } else {
                                    return None;
                                }
                            } else {
                                s.push(char::from_u32(cp)?);
                            }
                        }
                        _ => return None,
                    }
                }
                // A raw multi-byte UTF-8 sequence: copy the bytes through.
                0x80..=0xFF => {
                    let start = self.pos - 1;
                    while self.peek().is_some_and(|b| b >= 0x80) {
                        self.pos += 1;
                    }
                    s.push_str(core::str::from_utf8(&self.bytes[start..self.pos]).ok()?);
                }
                c => s.push(c as char),
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.peek()?;
            let d = (c as char).to_digit(16)?;
            v = v * 16 + d;
            self.pos += 1;
        }
        Some(v)
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
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
        let tok = core::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        if !is_real {
            if let Ok(i) = tok.parse::<i64>() {
                return Some(Json::Int(i));
            }
        }
        tok.parse::<f64>().ok().map(Json::Real)
    }
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
