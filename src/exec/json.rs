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
    /// A string. The first field is the decoded (unescaped) value used for all
    /// SQL value semantics. The optional second field carries the verbatim
    /// *escaped* source body (without the surrounding quotes) for provenance,
    /// when the string was parsed from a double-quoted JSON literal — see
    /// [`StrSrc`]. `None` means no provenance: a plain unescaped string
    /// (`TEXT`), a single-quoted string, a string built programmatically, or one
    /// whose source contained a non-reproducible feature (a `\v` escape or a
    /// literal control byte), all of which are rendered from the decoded value.
    Str(String, Option<StrSrc>),
    /// An array.
    Array(Vec<Json>),
    /// An object, preserving member order.
    Object(Vec<(String, Option<StrSrc>, Json)>),
}

/// The verbatim *escaped* source body of a parsed JSON string (without quotes),
/// retained so `json()` text and JSONB output byte-match SQLite. SQLite tags a
/// string by escape class: `TEXTJ` when every escape is standard JSON, `TEXT5`
/// when a JSON5-only escape is present. Both store the body verbatim in JSONB;
/// they differ only in `json()` *text* rendering — `TEXTJ` is emitted verbatim,
/// while `TEXT5` converts its JSON5-only escapes to standard JSON
/// (`\xHH`→`\u00HH`, `\0`→`\u0000`, `\'`→`'`, a `\`-newline continuation is
/// dropped) at render time.
#[derive(Clone, Debug, PartialEq)]
pub enum StrSrc {
    /// `TEXTJ`: standard-JSON escapes only; body emitted verbatim in text.
    TextJ(String),
    /// `TEXT5`: at least one JSON5-only escape; body converted on text render.
    Text5(String),
    /// `TEXTRAW`: raw SQL text stored verbatim (unescaped) in JSONB, needing
    /// JSON escaping only when rendered as text. SQLite produces this for a
    /// plain (non-JSON-subtype) SQL text *value* argument to
    /// `json_set`/`json_insert`/`json_replace` (`jsonFunctionArgToBlob`), and
    /// for a *new* object key those functions create from a bare (or
    /// backslash-free quoted) path label (`jsonLookupStep`, `rawKey`). Unlike
    /// `TextJ`/`Text5`, the JSONB payload is the decoded string itself (there
    /// is no separate escaped body), so this variant carries no body — the
    /// enclosing [`Json::Str`] value (or the object key it tags) *is* the
    /// payload.
    Raw,
}

impl Json {
    /// A JSONB **TEXTRAW** string: the given text stored verbatim (unescaped),
    /// as sqlite's `json_set`/`json_insert`/`json_replace` store a plain SQL
    /// text *value* argument (`jsonFunctionArgToBlob`). It renders as a
    /// normally escaped JSON string in text output but encodes to a `TEXTRAW`
    /// element in JSONB.
    pub fn text_raw(s: &str) -> Json {
        Json::Str(String::from(s), Some(StrSrc::Raw))
    }
    /// SQLite's `json_type` label for this value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(true) => "true",
            Json::Bool(false) => "false",
            Json::Int(..) => "integer",
            Json::Real(..) => "real",
            Json::Str(..) => "text",
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
            Json::Str(s, _) => Value::Text(s.clone().into()),
            Json::Array(_) | Json::Object(_) => Value::Text(self.serialize().into()),
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
        if let Json::Real(r, _) = self
            && r.is_infinite()
        {
            return String::from(if *r < 0.0 { "-9.0e+999" } else { "9.0e+999" });
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
            // A retained *strict* decimal form (the sole case being `-0`, whose
            // sign `i64` can't carry) is stored verbatim under INT — sqlite keeps
            // `-0` distinct from `0` (`jsonb('-0') != jsonb('0')`). A JSON5-only
            // hexadecimal source form is stored verbatim under INT5. Otherwise the
            // canonical decimal text goes under INT.
            Json::Int(_, Some(raw)) if is_strict_json_number(raw) => {
                push_jsonb(out, JSONB_INT, raw.as_bytes())
            }
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
            // A string with a retained source body stores it verbatim under its
            // escape-class tag (TEXTJ / TEXT5); otherwise raw bytes (TEXT) or a
            // freshly escaped body (TEXTJ).
            Json::Str(s, raw) => write_str_jsonb(s, raw, out),
            Json::Array(items) => {
                let mut body = Vec::new();
                for it in items {
                    it.write_jsonb(&mut body);
                }
                push_jsonb(out, JSONB_ARRAY, &body);
            }
            Json::Object(members) => {
                let mut body = Vec::new();
                for (k, kraw, v) in members {
                    // Keys carry the same escape provenance as string values.
                    write_str_jsonb(k, kraw, &mut body);
                    v.write_jsonb(&mut body);
                }
                push_jsonb(out, JSONB_OBJECT, &body);
            }
        }
    }

    /// Number of bytes this value occupies in its JSONB encoding (the header
    /// byte(s) plus the payload). Must mirror [`Json::write_jsonb`] byte-for-byte:
    /// the `json_each`/`json_tree` `id` (and `parent`) columns report each node's
    /// byte offset within the document's JSONB blob, and those offsets are
    /// accumulated from these lengths.
    pub(crate) fn jsonb_len(&self) -> usize {
        let payload = self.jsonb_payload_len();
        jsonb_header_len(payload) + payload
    }

    /// The number of header bytes preceding this value's JSONB payload — i.e. the
    /// offset of a container's first child relative to the container's own start.
    pub(crate) fn jsonb_header_bytes(&self) -> usize {
        jsonb_header_len(self.jsonb_payload_len())
    }

    /// The JSONB payload length (excluding the header) for this value.
    fn jsonb_payload_len(&self) -> usize {
        match self {
            Json::Null | Json::Bool(_) => 0,
            Json::Int(_, Some(raw)) => raw.len(),
            Json::Int(i, None) => i.to_string().len(),
            // A float's payload is its source/canonical text (FLOAT or FLOAT5);
            // the tag differs but the byte length does not.
            Json::Real(r, src) => match src {
                Some(t) => t.len(),
                None if r.is_infinite() => {
                    if *r < 0.0 {
                        "-9e999".len()
                    } else {
                        "9e999".len()
                    }
                }
                None => crate::exec::eval::format_real(*r).len(),
            },
            Json::Str(s, raw) => str_prov_payload_len(s, raw),
            Json::Array(items) => items.iter().map(Json::jsonb_len).sum(),
            Json::Object(members) => members
                .iter()
                .map(|(k, kraw, v)| str_prov_jsonb_len(k, kraw) + v.jsonb_len())
                .sum(),
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
                for (i, (k, kraw, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    pad(out, depth + 1);
                    write_str_text(k, kraw, out);
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
            // A retained *strict* decimal integer form (`-0`, whose sign `i64`
            // can't carry) renders verbatim, matching sqlite's `json('-0')` → `-0`.
            // A JSON5-only hex form (`0x1f`) renders in canonical decimal (`31`).
            Json::Int(_, Some(raw)) if is_strict_json_number(raw) => out.push_str(raw),
            Json::Int(i, _) => out.push_str(&i.to_string()),
            // Preserve a *strict* number's verbatim source text (`1e2`, `1.50`,
            // `-0.0`); a JSON5-only form (`.5`, `5.`) and a computed value render
            // in canonical form (`.5` → `0.5`), matching sqlite's `json()`. This
            // precedes the infinity fallback so a parsed-from-text number that
            // overflows f64 (`1e1000`, `9.9e999`) still round-trips verbatim,
            // exactly as `json('1e1000')` → `1e1000` in sqlite.
            Json::Real(_, Some(src)) if is_strict_json_number(src) => out.push_str(src),
            // A JSON5 leading/trailing-`.` form keeps its source shape with the
            // minimal `0` inserted to make it valid JSON (`1.e5` → `1.0e5`,
            // `.5e2` → `0.5e2`), matching sqlite — *not* the computed float
            // (`100000.0`). This precedes the infinity fallback so an overflowing
            // dot-form (`1.e5000`) still renders `1.0e5000`, like sqlite.
            Json::Real(_, Some(src)) => out.push_str(&json5_fixup_number(src)),
            Json::Real(r, _) if r.is_infinite() => {
                // A *computed* infinity (no source text) has no JSON literal;
                // sqlite renders it as `±9e999` (a value that round-trips to f64
                // infinity).
                out.push_str(if *r < 0.0 { "-9e999" } else { "9e999" });
            }
            Json::Real(r, _) => out.push_str(&crate::exec::eval::format_real(*r)),
            // A retained source body is re-emitted between quotes: TEXTJ
            // verbatim, TEXT5 with its JSON5-only escapes converted to standard
            // JSON. Otherwise the decoded value is freshly escaped.
            Json::Str(s, raw) => write_str_text(s, raw, out),
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
                for (i, (k, kraw, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str_text(k, kraw, out);
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

/// Header byte count for a JSONB element whose payload is `n` bytes. Mirrors the
/// size-class thresholds in [`push_jsonb`]: 1 byte for sizes 0–11, then +1/2/4/8
/// big-endian size bytes as the payload grows.
fn jsonb_header_len(n: usize) -> usize {
    if n <= 11 {
        1
    } else if n <= 0xFF {
        2
    } else if n <= 0xFFFF {
        3
    } else if n as u64 <= 0xFFFF_FFFF {
        5
    } else {
        9
    }
}

/// The JSONB payload length of `s` encoded as a string (TEXT/TEXTJ) — its raw
/// bytes, or its JSON-escaped body when any character needs escaping.
fn str_jsonb_payload_len(s: &str) -> usize {
    if json_needs_escape(s) {
        json_escape_body(s).len()
    } else {
        s.len()
    }
}

/// The JSONB payload length of a string carrying optional escape provenance:
/// the verbatim retained body when present (TEXTJ/TEXT5), else `s` encoded as a
/// plain string. Shared by string *values* and object *keys*.
fn str_prov_payload_len(s: &str, raw: &Option<StrSrc>) -> usize {
    match raw {
        Some(StrSrc::TextJ(body) | StrSrc::Text5(body)) => body.len(),
        // TEXTRAW stores the decoded string verbatim; there is no escaped body.
        Some(StrSrc::Raw) => s.len(),
        None => str_jsonb_payload_len(s),
    }
}

/// Total JSONB length (header + payload) of a provenance-carrying string. Used
/// to size object-key nodes when computing `json_each`/`json_tree` byte offsets.
pub(crate) fn str_prov_jsonb_len(s: &str, raw: &Option<StrSrc>) -> usize {
    let payload = str_prov_payload_len(s, raw);
    jsonb_header_len(payload) + payload
}

/// Append a provenance-carrying string as one JSONB element under the tag its
/// escape class implies: TEXTJ/TEXT5 for a retained body, else TEXT (raw bytes)
/// or a freshly-escaped TEXTJ. Shared by string values and object keys.
fn write_str_jsonb(s: &str, raw: &Option<StrSrc>, out: &mut Vec<u8>) {
    match raw {
        Some(StrSrc::TextJ(body)) => push_jsonb(out, JSONB_TEXTJ, body.as_bytes()),
        Some(StrSrc::Text5(body)) => push_jsonb(out, JSONB_TEXT5, body.as_bytes()),
        // TEXTRAW: the decoded string is stored verbatim (unescaped).
        Some(StrSrc::Raw) => push_jsonb(out, JSONB_TEXTRAW, s.as_bytes()),
        None if json_needs_escape(s) => {
            push_jsonb(out, JSONB_TEXTJ, json_escape_body(s).as_bytes());
        }
        None => push_jsonb(out, JSONB_TEXT, s.as_bytes()),
    }
}

/// Render a provenance-carrying string as JSON *text*: a retained TEXTJ body is
/// emitted verbatim, a TEXT5 body is converted to strict JSON, and a plain
/// string is escaped canonically. Shared by string values and object keys.
fn write_str_text(s: &str, raw: &Option<StrSrc>, out: &mut String) {
    match raw {
        Some(StrSrc::TextJ(body)) => {
            out.push('"');
            out.push_str(body);
            out.push('"');
        }
        Some(StrSrc::Text5(body)) => {
            out.push('"');
            out.push_str(&json5_to_json_text(body));
            out.push('"');
        }
        // TEXTRAW is raw SQL text that needs escaping for JSON: render the
        // decoded value with canonical escaping, exactly like a plain string
        // (sqlite's `jsonTranslateBlobToText` runs TEXTRAW through
        // `jsonAppendString`).
        Some(StrSrc::Raw) | None => write_json_string(s, out),
    }
}

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
    } else if n as u64 <= 0xFFFF_FFFF {
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
        JSONB_TEXT => Json::Str(text()?, None),
        // TEXT stores bytes that are already valid JSON string content; TEXTRAW
        // stores raw SQL text that needs escaping on render. Tag TEXTRAW as
        // `Raw` so it re-encodes to a byte-identical TEXTRAW element
        // (round-trip), rather than collapsing to TEXT/TEXTJ.
        JSONB_TEXTRAW => Json::Str(text()?, Some(StrSrc::Raw)),
        JSONB_TEXTJ | JSONB_TEXT5 => {
            // Reparse the escaped body as a quoted JSON string.
            let body = core::str::from_utf8(payload).ok()?;
            let mut quoted = String::with_capacity(n + 2);
            quoted.push('"');
            quoted.push_str(body);
            quoted.push('"');
            // Preserve the verbatim body so it round-trips to the same JSONB
            // bytes, tagged by the stored escape class. (A re-encode of a
            // `\v`/control-byte TEXT5 body that the parser would otherwise
            // refuse to retain still round-trips, since the body came from
            // valid JSONB.) A body the strict parser rejects — e.g. an unknown
            // escape like `\q`, which `json_set('…','$."a\qb"',…)` stores
            // verbatim in a TEXT5 label — is decoded leniently the way
            // sqlite's `jsonReturnFromBlob` does (bad escapes silently
            // dropped), still keeping the verbatim body for text rendering.
            let s = match parse(&quoted) {
                Some(Json::Str(s, _)) => s,
                Some(_) => return None,
                None => lenient_unescape(body),
            };
            let raw = if ty == JSONB_TEXTJ {
                Some(StrSrc::TextJ(String::from(body)))
            } else {
                Some(StrSrc::Text5(String::from(body)))
            };
            Json::Str(s, raw)
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
                let (key, kraw) = match label {
                    Json::Str(s, raw) => (s, raw),
                    _ => return None,
                };
                members.push((key, kraw, val));
                p = next2;
            }
            Json::Object(members)
        }
        _ => return None,
    };
    Some((j, after))
}

/// Decode a `TEXTJ`/`TEXT5` JSONB payload into its SQL string value the way
/// sqlite's `jsonReturnFromBlob` does, used when the strict string parser
/// rejects the body (e.g. an unknown escape like `\q`, which
/// `json_set(…,'$."a\qb"',…)` stores verbatim in a TEXT5 label). Each
/// backslash escape is decoded by the `jsonUnescapeOneChar` rules; an invalid
/// escape (`JSON_INVALID_CHAR`) is silently dropped from the value.
fn lenient_unescape(body: &str) -> String {
    let b = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let hex = |at: usize, k: usize| -> u32 {
        // Like sqlite's jsonHexToInt*, a non-hex digit contributes 0.
        let mut v = 0u32;
        for j in 0..k {
            v = v * 16
                + b.get(at + j)
                    .and_then(|&c| (c as char).to_digit(16))
                    .unwrap_or(0);
        }
        v
    };
    // The byte count of a line-terminator run (`\r\n`, `\r`, `\n`, U+2028,
    // U+2029) starting at `at` — sqlite's `jsonBytesToBypass`.
    let bypass = |mut at: usize| -> usize {
        let start = at;
        loop {
            match b.get(at) {
                Some(b'\r') => at += if b.get(at + 1) == Some(&b'\n') { 2 } else { 1 },
                Some(b'\n') => at += 1,
                Some(0xe2) if b.get(at + 1) == Some(&0x80) => match b.get(at + 2) {
                    Some(0xa8 | 0xa9) => at += 3,
                    _ => break,
                },
                _ => break,
            }
        }
        at - start
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c != b'\\' {
            // Copy the literal UTF-8 run through verbatim.
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'\\' {
                i += 1;
            }
            out.push_str(&body[start..i]);
            continue;
        }
        if i + 1 >= b.len() {
            break; // trailing lone backslash: invalid, dropped
        }
        // `v` is the decoded scalar, `None` for an invalid escape (dropped).
        let (v, consumed): (Option<u32>, usize) = match b[i + 1] {
            b'u' => {
                if b.len() - i < 6 {
                    (None, b.len() - i)
                } else {
                    let hi = hex(i + 2, 4);
                    if (hi & 0xfc00) == 0xd800
                        && b.len() - i >= 12
                        && b[i + 6] == b'\\'
                        && b[i + 7] == b'u'
                        && (hex(i + 8, 4) & 0xfc00) == 0xdc00
                    {
                        let lo = hex(i + 8, 4);
                        (Some(((hi & 0x3ff) << 10) + (lo & 0x3ff) + 0x10000), 12)
                    } else {
                        (Some(hi), 6)
                    }
                }
            }
            b'b' => (Some(0x08), 2),
            b'f' => (Some(0x0c), 2),
            b'n' => (Some(0x0a), 2),
            b'r' => (Some(0x0d), 2),
            b't' => (Some(0x09), 2),
            // The blob-unescape path maps `\v` to U+000B (unlike the text
            // parser's U+0009 — sqlite is inconsistent here on purpose).
            b'v' => (Some(0x0b), 2),
            // `\0` not followed by a digit is a NUL.
            b'0' => {
                if b.get(i + 2).is_some_and(u8::is_ascii_digit) {
                    (None, 2)
                } else {
                    (Some(0), 2)
                }
            }
            e @ (b'\'' | b'"' | b'/' | b'\\') => (Some(e as u32), 2),
            b'x' => {
                if b.len() - i < 4 {
                    (None, b.len() - i)
                } else {
                    (Some(hex(i + 2, 2)), 4)
                }
            }
            // A backslash before a line terminator is a line continuation: the
            // terminator run is skipped and decoding resumes after it.
            0xe2 | b'\r' | b'\n' => {
                let skip = bypass(i + 1);
                if skip == 0 {
                    (None, b.len() - i) // a non-terminator U+E2xx: invalid
                } else if i + 1 + skip == b.len() {
                    (Some(0), b.len() - i)
                } else {
                    (None, 1 + skip) // resume at the next character
                }
            }
            _ => (None, 2), // unknown escape: invalid, dropped
        };
        if let Some(v) = v {
            // A value that is not a valid scalar (a lone surrogate half) cannot
            // live in a Rust string; sqlite emits its WTF-8 bytes, we degrade
            // to U+FFFD.
            out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
        }
        i += consumed;
    }
    out
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

/// Append object key `k` to a `json_each`/`json_tree` path. SQLite renders a
/// "simple" label — non-empty, an ASCII letter followed by ASCII alphanumerics —
/// bare as `.k`; anything else (a leading digit or `_`, spaces, dots, non-ASCII,
/// …) is double-quoted with its JSON-escaped body, e.g. `."a b"`, `."_x"`,
/// `."a\"b"`.
pub(crate) fn push_path_key(path: &str, k: &str) -> String {
    let mut chars = k.chars();
    let simple = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => chars.all(|c| c.is_ascii_alphanumeric()),
        _ => false,
    };
    if simple {
        alloc::format!("{path}.{k}")
    } else {
        alloc::format!("{path}.\"{}\"", json_escape_body(k))
    }
}

/// Append an object key carrying escape provenance to a path. A key parsed from
/// an escaped source (TEXTJ/TEXT5) is rendered from its *verbatim* source body,
/// always double-quoted — so an escaped key whose decoded value is a bare
/// identifier is still quoted, and a TEXT5 key keeps its raw body (e.g. the `x`
/// escape stays `."\x41"`, not the `\u`-converted text form), exactly as SQLite
/// reports in `json_each`/`json_tree`. A key with no provenance falls back to
/// the decoded-value [`push_path_key`] logic.
pub(crate) fn push_path_key_prov(path: &str, k: &str, raw: &Option<StrSrc>) -> String {
    match raw {
        Some(StrSrc::TextJ(body) | StrSrc::Text5(body)) => {
            alloc::format!("{path}.\"{body}\"")
        }
        // A TEXTRAW label's payload is the decoded key itself; SQLite's
        // `jsonAppendPathName` prints the payload *verbatim* (no escaping),
        // bare when it is a simple identifier and double-quoted otherwise.
        Some(StrSrc::Raw) => {
            let mut chars = k.chars();
            let simple = match chars.next() {
                Some(c) if c.is_ascii_alphabetic() => chars.all(|c| c.is_ascii_alphanumeric()),
                _ => false,
            };
            if simple {
                alloc::format!("{path}.{k}")
            } else {
                alloc::format!("{path}.\"{k}\"")
            }
        }
        None => push_path_key(path, k),
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
            Some(b'"') => self.string(b'"').map(|(s, raw)| Json::Str(s, raw)),
            Some(b'\'') => self.string(b'\'').map(|(s, raw)| Json::Str(s, raw)),
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
            let (key, key_raw) = self.object_key()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return self.err();
            }
            self.pos += 1;
            let val = self.value()?;
            members.push((key, key_raw, val));
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
    fn object_key(&mut self) -> Result<(String, Option<StrSrc>), usize> {
        match self.peek() {
            // A double-quoted key keeps its escape provenance exactly like a
            // string value; a single-quoted (JSON5) key decodes to canonical
            // form (raw = None, like a JSON5 string value).
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
                Ok((
                    core::str::from_utf8(&self.bytes[start..self.pos])
                        .unwrap_or("")
                        .to_string(),
                    None,
                ))
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
    /// Parse a quoted string, returning the decoded value and — for a
    /// double-quoted literal containing escapes — its verbatim escaped body
    /// (between the quotes) tagged by escape class for provenance: [`StrSrc`]
    /// `TextJ` when every escape is standard JSON, `Text5` when a JSON5-only
    /// escape (`\x`, `\'`, `\0`, a line continuation) is present. The body is
    /// `None` for a plain unescaped string, a single-quoted string, or one
    /// carrying a feature that can't be faithfully re-rendered from the body
    /// alone (a `\v` escape — sqlite maps it to U+0009 — or a literal control
    /// byte); those render from the decoded value.
    fn string(&mut self, quote: u8) -> Result<(String, Option<StrSrc>), usize> {
        self.pos += 1; // opening quote
        let body_start = self.pos;
        let mut s = String::new();
        // Whether any escape was seen, whether a JSON5-only escape was seen
        // (→ TEXT5), and whether a feature we can't reproduce from the body was
        // seen (→ don't retain provenance at all).
        let mut had_escape = false;
        let mut json5 = false;
        let mut block_raw = false;
        loop {
            let Some(c) = self.peek() else {
                return self.err();
            };
            if c == quote {
                let body_end = self.pos;
                self.pos += 1;
                // Retain the verbatim body only for a double-quoted string with
                // escapes and no unreproducible feature; tag by escape class. By
                // construction it round-trips to the same decoded value.
                let raw = if quote == b'"' && had_escape && !block_raw {
                    core::str::from_utf8(&self.bytes[body_start..body_end])
                        .ok()
                        .map(|body| {
                            let body = String::from(body);
                            if json5 {
                                StrSrc::Text5(body)
                            } else {
                                StrSrc::TextJ(body)
                            }
                        })
                } else {
                    None
                };
                return Ok((s, raw));
            }
            self.pos += 1;
            match c {
                b'\\' => {
                    had_escape = true;
                    // The escape introducer position, used to report bad escapes
                    // at the escape character itself (matching sqlite3).
                    let esc_pos = self.pos;
                    let Some(esc) = self.peek() else {
                        return self.err();
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => s.push('"'),
                        b'\'' => {
                            json5 = true;
                            s.push('\'');
                        }
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        // JSON5: `\v` maps to U+0009 in sqlite (not U+000B). Its
                        // text re-rendering (`\u0009`) isn't locally verifiable
                        // against stock sqlite, so don't retain the body.
                        b'v' => {
                            block_raw = true;
                            s.push('\t');
                        }
                        b'r' => s.push('\r'),
                        b'b' => s.push('\u{08}'),
                        b'f' => s.push('\u{0c}'),
                        // JSON5: `\0` is a NUL.
                        b'0' => {
                            json5 = true;
                            s.push('\0');
                        }
                        // JSON5: a backslash before a line terminator is a line
                        // continuation (the newline is dropped). `\r\n` counts
                        // as one terminator.
                        b'\n' => json5 = true,
                        b'\r' => {
                            json5 = true;
                            if self.peek() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        // JSON5: `\xHH` is a two-digit hex escape.
                        b'x' => {
                            json5 = true;
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
                c => {
                    // A literal control byte can't be reproduced from the
                    // escaped body alone, so don't retain provenance.
                    if c < 0x20 {
                        block_raw = true;
                    }
                    s.push(c as char);
                }
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
        // A JSON number's integer part may not carry a leading zero: `0` must
        // stand alone (`0`, `0.5`, `0e1`, `-0`). `00`, `007`, `01`, `00.5` and
        // `-01` are malformed, exactly as sqlite's parser rejects them. (The
        // `0x…` hex form was already handled above.) sqlite points
        // `json_error_position` at the number token's *second* character (`start
        // + 1`), whether or not the token opened with a sign — `-01` reports the
        // same offset as `01`.
        if self.peek() == Some(b'0') && self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit)
        {
            return Err(start + 1);
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
        if !is_real && let Ok(i) = tok.parse::<i64>() {
            // Retain the verbatim token when it is a *strict* decimal form that
            // does not match the canonical rendering of `i`. The only such case is
            // `-0`, whose sign an `i64` `0` can't carry: sqlite preserves it as a
            // distinct value (`json('-0')` → `-0`, `jsonb('-0') != jsonb('0')`). A
            // JSON5-only leading-`+` form (`+5`) is *not* strict, so it normalizes.
            let raw =
                (is_strict_json_number(tok) && tok != i.to_string()).then(|| String::from(tok));
            return Ok(Json::Int(i, raw));
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

/// Insert the minimal `0` that turns a JSON5 leading/trailing-`.` number into
/// valid JSON, preserving the rest of the source form (`1.e5` → `1.0e5`, `.5e2`
/// → `0.5e2`, `-.5` → `-0.5`) — exactly how sqlite renders such a number in
/// `json()` text output. A number with digits on both sides of the `.` (or none
/// at all) is already valid and returned verbatim.
fn json5_fixup_number(src: &str) -> String {
    let b = src.as_bytes();
    let Some(dot) = src.find('.') else {
        return String::from(src);
    };
    let mut out = String::with_capacity(src.len() + 2);
    out.push_str(&src[..dot]);
    if !(dot > 0 && b[dot - 1].is_ascii_digit()) {
        out.push('0'); // a leading `.` (or `-.`) becomes `0.`
    }
    out.push('.');
    if !b.get(dot + 1).is_some_and(u8::is_ascii_digit) {
        out.push('0'); // a trailing `.` (before `e`/end) becomes `.0`
    }
    out.push_str(&src[dot + 1..]);
    out
}

/// Convert a `TEXT5` (JSON5) escaped string body to its standard-JSON text form,
/// matching how sqlite renders such a string in `json()`: the JSON5-only escapes
/// are rewritten — `\xHH` → `\u00HH`, `\0` → `\u0000`, `\'` → `'`, and a
/// backslash-before-newline line continuation is dropped — while every
/// standard-JSON escape and all literal text pass through verbatim. Verbatim
/// runs are copied as whole `&str` slices, so multi-byte UTF-8 is preserved.
fn json5_to_json_text(body: &str) -> String {
    let b = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    let mut run = 0; // start of the current verbatim (uncopied) run
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            out.push_str(&body[run..i]); // flush the run before this escape
            match b[i + 1] {
                // `\xHH` → `\u00HH` (the two hex digits were validated on parse).
                b'x' => {
                    out.push_str("\\u00");
                    out.push_str(&body[i + 2..i + 4]);
                    i += 4;
                }
                b'0' => {
                    out.push_str("\\u0000");
                    i += 2;
                }
                b'\'' => {
                    out.push('\'');
                    i += 2;
                }
                // A line continuation: drop the backslash and the terminator
                // (`\r\n` counts as one).
                b'\n' => i += 2,
                b'\r' => {
                    i += 2;
                    if b.get(i) == Some(&b'\n') {
                        i += 1;
                    }
                }
                // A standard-JSON escape (incl. `\uXXXX`): copy it verbatim.
                _ => {
                    out.push_str(&body[i..i + 2]);
                    i += 2;
                }
            }
            run = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&body[run..]);
    out
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
            (Json::Object(members), Seg::Key(k, _)) => {
                cur = members
                    .iter()
                    .find(|(kk, _, _)| kk == k)
                    .map(|(_, _, v)| v)?;
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

/// Like [`navigate`], but also returns where the resolved node sits in the
/// document's JSONB encoding: `value_offset` is the byte offset of the node's
/// own element, and `id` is the offset `json_each`/`json_tree` reports for it —
/// the *key* node's offset when the node is an object member, otherwise the
/// value node's own offset. Both are relative to the whole document, so a
/// path-rooted walk numbers its rows exactly as SQLite does.
pub(crate) fn navigate_with_offset<'a>(
    root: &'a Json,
    path: &str,
) -> Option<(&'a Json, usize, usize)> {
    let segs = parse_path(path)?;
    let mut cur = root;
    let mut value_offset = 0usize;
    let mut id = 0usize;
    for seg in &segs {
        let body = value_offset + jsonb_header_len(cur.jsonb_payload_len());
        match (cur, seg) {
            (Json::Object(members), Seg::Key(k, _)) => {
                let mut at = body;
                let mut found = None;
                for (kk, kraw, vv) in members {
                    let klen = str_prov_jsonb_len(kk, kraw);
                    if kk == k {
                        found = Some((vv, at, at + klen));
                        break;
                    }
                    at += klen + vv.jsonb_len();
                }
                let (vv, key_off, val_off) = found?;
                cur = vv;
                value_offset = val_off;
                id = key_off;
            }
            (Json::Array(items), Seg::Index(idx)) => {
                let n = idx.resolve_read(items.len())?;
                let mut at = body;
                for vv in &items[..n] {
                    at += vv.jsonb_len();
                }
                cur = items.get(n)?;
                value_offset = at;
                id = at;
            }
            _ => return None,
        }
    }
    Some((cur, value_offset, id))
}

/// Convert a SQL [`Value`] to a [`Json`] for `json_array`/`json_object`. A text
/// value that is itself valid JSON is *not* re-parsed here (SQLite only treats
/// arguments as JSON when wrapped in `json()`); plain text becomes a JSON string.
pub fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Integer(i) => Json::Int(*i, None),
        Value::Real(r) => Json::Real(*r, None),
        Value::Text(s) => Json::Str(s.as_str().to_string(), None),
        Value::Blob(_) => Json::Str(String::new(), None),
    }
}

/// The `->` / `->>` operators. `doc` is the JSON document, `path_arg` the right
/// operand (a JSON path, a bare object label, or an integer array index). With
/// `as_text` (the `->>` form) the result is the SQL value at the path; otherwise
/// (`->`) it is that node re-rendered as JSON text. NULL doc or a missing path
/// yields SQL NULL.
pub fn arrow(doc: &Value, path_arg: &Value, as_text: bool) -> crate::Result<Value> {
    if matches!(doc, Value::Null) {
        return Ok(Value::Null);
    }
    let text = crate::exec::eval::to_text(doc);
    let Some(root) = parse(&text) else {
        // A text (or numeric) document that is not valid JSON is a hard error,
        // matching sqlite and `json_extract` (which both reject `'' -> 1` and
        // `'notjson' -> 0` as `malformed JSON`). A BLOB document is SQLite's
        // binary JSONB, which the arrow operators do not yet model, so it keeps
        // the existing lenient raw-bytes path rather than gaining a new
        // divergence here.
        if matches!(doc, Value::Blob(_)) {
            return Ok(Value::Null);
        }
        return Err(crate::error::Error::Error("malformed JSON".into()));
    };
    let path = arrow_path(path_arg);
    // An explicit `$`-rooted path operand must be syntactically valid — sqlite
    // raises "bad JSON path" for e.g. `-> '$bad'`. A bare key/index operand is
    // always well-formed (it is wrapped into `$.key` / `$[n]`), so it is not
    // validated (it may legitimately contain spaces or other characters).
    if matches!(path_arg, Value::Text(s) if s.starts_with('$')) && !path_is_valid(&path) {
        return Err(crate::error::Error::Error(alloc::format!(
            "bad JSON path: '{path}'"
        )));
    }
    Ok(match navigate(&root, &path) {
        None => Value::Null,
        Some(node) => {
            if as_text {
                node.to_sql()
            } else {
                Value::Text(node.serialize().into())
            }
        }
    })
}

/// Normalize an `->`/`->>` right operand into a JSON path: an integer is an array
/// index (a non-negative integer is `$[n]`; a negative integer `-k` is the
/// end-relative `$[#-k]`, matching SQLite), a string starting with `$` is used
/// verbatim, any other string is an object label `$.label`.
fn arrow_path(v: &Value) -> String {
    match v {
        Value::Integer(i) if *i < 0 => alloc::format!("$[#-{}]", i.unsigned_abs()),
        Value::Integer(i) => alloc::format!("$[{i}]"),
        Value::Text(s) if s.starts_with('$') => s.as_str().to_string(),
        // A bare key is a single object label, even when it contains `.`/`[`
        // (sqlite's `-> 'a.b'` is the literal key "a.b", not the nested path
        // `$.a.b`). Wrap it as a quoted label so the dots are taken literally.
        Value::Text(s) => alloc::format!("$.\"{}\"", s.as_str()),
        other => alloc::format!("$.\"{}\"", crate::exec::eval::to_text(other)),
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
    /// An object-key step. The first field is the decoded key (used for
    /// lookup); the second is the label provenance SQLite would assign when
    /// *creating* this key (`TEXTRAW` for a bare or backslash-free quoted
    /// label, `TEXT5` — verbatim body — for a quoted label containing a
    /// backslash) — see [`parse_key`] and sqlite's `jsonLookupStep` (`rawKey`).
    Key(String, Option<StrSrc>),
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
                let (key, prov, next) = parse_key(bytes, i + 1)?;
                segs.push(Seg::Key(key, prov));
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
/// Read a `\uXXXX` escape's four hex digits starting at `bytes[at]`.
fn json_path_hex4(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    let mut v = 0u32;
    for &b in slice {
        v = v * 16 + (b as char).to_digit(16)?;
    }
    Some(v)
}

fn parse_key(bytes: &[u8], i: usize) -> Option<(String, Option<StrSrc>, usize)> {
    if bytes.get(i) == Some(&b'"') {
        // Quoted key: read until the closing (unescaped) quote, decoding JSON
        // escapes. SQLite scans past `\<char>` when locating the close and then
        // compares labels with escapes decoded (`jsonUnescapeOneChar`), so
        // `$."a\"b"` selects the key `a"b`.
        //
        // For a *new* key `json_set`/`json_insert` create, SQLite tags the
        // label by whether the verbatim body (between the quotes) contains a
        // backslash: none → TEXTRAW (`rawKey`), otherwise → TEXT5, storing the
        // body verbatim either way (`jsonLookupStep`). Mirror that provenance
        // so a created key encodes byte-identically. (With no backslash the
        // decoded key equals the body, so `Raw` needs no separate body.)
        let start = i + 1;
        let mut j = start;
        let mut s = String::new();
        while let Some(&c) = bytes.get(j) {
            match c {
                b'"' => {
                    let body = core::str::from_utf8(&bytes[start..j]).ok()?;
                    let prov = if body.as_bytes().contains(&b'\\') {
                        Some(StrSrc::Text5(String::from(body)))
                    } else {
                        Some(StrSrc::Raw)
                    };
                    return Some((s, prov, j + 1));
                }
                b'\\' => {
                    let e = *bytes.get(j + 1)?;
                    j += 2;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'b' => s.push('\u{08}'),
                        b'f' => s.push('\u{0c}'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'u' => {
                            let hi = json_path_hex4(bytes, j)?;
                            j += 4;
                            let ch = if (0xD800..=0xDBFF).contains(&hi) {
                                // A high surrogate consumes a following `\uXXXX`
                                // low surrogate; a lone one becomes U+FFFD.
                                if bytes.get(j) == Some(&b'\\') && bytes.get(j + 1) == Some(&b'u') {
                                    let lo = json_path_hex4(bytes, j + 2)?;
                                    j += 6;
                                    char::from_u32(0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                                        .unwrap_or('\u{FFFD}')
                                } else {
                                    '\u{FFFD}'
                                }
                            } else {
                                char::from_u32(hi).unwrap_or('\u{FFFD}')
                            };
                            s.push(ch);
                        }
                        other => s.push(other as char),
                    }
                }
                // A raw multi-byte UTF-8 sequence: copy the bytes through.
                0x80..=0xFF => {
                    let start = j;
                    j += 1;
                    while bytes.get(j).is_some_and(|&x| (0x80..0xC0).contains(&x)) {
                        j += 1;
                    }
                    s.push_str(core::str::from_utf8(&bytes[start..j]).ok()?);
                }
                _ => {
                    s.push(c as char);
                    j += 1;
                }
            }
        }
        // Reached the end without a closing quote.
        None
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
        // A bare identifier label is always `rawKey` → TEXTRAW when created.
        Some((key, Some(StrSrc::Raw), j))
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
            (Json::Object(m), Seg::Key(k, _)) => {
                m.iter_mut().find(|(kk, _, _)| kk == k).map(|(_, _, v)| v)?
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
            Seg::Key(..) => Json::Object(Vec::new()),
            Seg::Index(_) => Json::Array(Vec::new()),
        };
        cur = match (cur, &segs[i]) {
            (Json::Object(m), Seg::Key(k, prov)) => {
                if !m.iter().any(|(kk, _, _)| kk == k) {
                    // A created intermediate key carries the label provenance
                    // SQLite would assign (TEXTRAW for a bare label), so the
                    // JSONB encodes byte-identically (`jsonLookupStep`).
                    m.push((k.clone(), prov.clone(), make()));
                }
                m.iter_mut().find(|(kk, _, _)| kk == k).map(|(_, _, v)| v)?
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
    if let Some(cur) = walk_to_parent_creating(&mut work, &segs)
        && write_leaf(cur, last, value, mode)
    {
        *root = work;
    }
    Some(())
}

/// Write `value` at the leaf segment `last` of the already-resolved parent `cur`
/// under `mode`. Returns whether a write actually happened (so an atomic caller
/// can discard created intermediates on a no-op).
fn write_leaf(cur: &mut Json, last: &Seg, value: Json, mode: SetMode) -> bool {
    match (cur, last) {
        (Json::Object(m), Seg::Key(k, prov)) => {
            let slot = m.iter_mut().find(|(kk, _, _)| kk == k);
            match (slot, mode) {
                (Some(s), SetMode::Set) | (Some(s), SetMode::Replace) => {
                    // Overwriting an existing member leaves its stored key
                    // bytes (and tag) untouched, exactly like sqlite's
                    // `jsonBlobEdit` of just the value.
                    s.2 = value;
                    true
                }
                (None, SetMode::Set) | (None, SetMode::Insert) => {
                    // A freshly created key carries the path label's
                    // provenance (TEXTRAW for a bare or backslash-free quoted
                    // label), matching sqlite's `rawKey`.
                    m.push((k.clone(), prov.clone(), value));
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
        (Json::Object(m), Seg::Key(k, _)) => m.retain(|(kk, _, _)| kk != k),
        (Json::Array(a), Seg::Index(idx)) => {
            if let Some(n) = idx.resolve_read(a.len())
                && n < a.len()
            {
                a.remove(n);
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
    for (k, kraw, pv) in pm {
        if matches!(pv, Json::Null) {
            tm.retain(|(kk, _, _)| kk != k);
        } else if let Some(slot) = tm.iter_mut().find(|(kk, _, _)| kk == k) {
            merge_patch(&mut slot.2, pv);
        } else {
            tm.push((k.clone(), kraw.clone(), Json::Null));
            let slot = tm.last_mut().unwrap();
            merge_patch(&mut slot.2, pv);
        }
    }
}
