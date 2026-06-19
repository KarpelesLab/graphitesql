//! Built-in scalar functions.
//!
//! Aggregate functions (`count`, `sum`, …) are handled by the executor, which
//! folds over rows; this module covers the per-row scalar functions. The set is
//! a useful core and grows toward SQLite's full library (`func.c`, `date.c`).

use super::eval::{self, EvalCtx};
use crate::error::{Error, Result};
use crate::sql::ast::Expr;
use crate::value::Value;
use alloc::string::String;
use alloc::vec::Vec;

/// Names that *can* be aggregates (used for catalog/name checks).
pub fn is_aggregate(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "total" | "avg" | "min" | "max" | "group_concat"
    )
}

/// Whether a *specific call* is an aggregate. `min`/`max` are scalar with 2+
/// arguments and aggregate with exactly one (or `*`), matching SQLite.
pub fn is_aggregate_call(name: &str, nargs: usize, star: bool) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "count" | "sum" | "total" | "avg" | "group_concat" => true,
        "min" | "max" => star || nargs == 1,
        _ => false,
    }
}

/// Evaluate a scalar function call.
pub fn eval_scalar(name: &str, args: &[Expr], star: bool, ctx: &EvalCtx) -> Result<Value> {
    let lname = name.to_ascii_lowercase();
    if is_aggregate_call(&lname, args.len(), star) {
        return Err(Error::Error(alloc::format!(
            "aggregate function {name} used outside an aggregate context"
        )));
    }
    if star {
        return Err(Error::Error(alloc::format!(
            "{name}(*) is not a scalar call"
        )));
    }

    // Functions whose NULL-handling is special are done before arg evaluation.
    match lname.as_str() {
        "coalesce" => {
            for a in args {
                let v = eval::eval(a, ctx)?;
                if !matches!(v, Value::Null) {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        "ifnull" => {
            arity(&lname, args, 2)?;
            let a = eval::eval(&args[0], ctx)?;
            return if matches!(a, Value::Null) {
                eval::eval(&args[1], ctx)
            } else {
                Ok(a)
            };
        }
        _ => {}
    }

    let v: Vec<Value> = args
        .iter()
        .map(|a| eval::eval(a, ctx))
        .collect::<Result<_>>()?;

    Ok(match lname.as_str() {
        "abs" => {
            arity(&lname, args, 1)?;
            match eval::to_number(&v[0]) {
                Value::Integer(i) => Value::Integer(i.wrapping_abs()),
                Value::Real(r) => Value::Real(crate::util::float::abs(r)),
                _ => Value::Null,
            }
        }
        "length" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                Value::Blob(b) => Value::Integer(b.len() as i64),
                other => Value::Integer(eval::to_text(other).chars().count() as i64),
            }
        }
        "lower" => {
            arity(&lname, args, 1)?;
            str_map(&v[0], |s| s.to_lowercase())
        }
        "upper" => {
            arity(&lname, args, 1)?;
            str_map(&v[0], |s| s.to_uppercase())
        }
        "trim" => trim_fn(&v, true, true),
        "ltrim" => trim_fn(&v, true, false),
        "rtrim" => trim_fn(&v, false, true),
        "typeof" => Value::Text(String::from(type_name(&v[0]))),
        "nullif" => {
            arity(&lname, args, 2)?;
            if eval::compare(&v[0], &v[1]) == core::cmp::Ordering::Equal {
                Value::Null
            } else {
                v[0].clone()
            }
        }
        "n/a" => unreachable!(),
        "substr" | "substring" => substr(&v)?,
        "instr" => instr(&v)?,
        "replace" => replace(&v)?,
        "round" => round(&v)?,
        "min" => scalar_min_max(&v, true),
        "max" => scalar_min_max(&v, false),
        "hex" => Value::Text(hex_encode(&v[0])),
        "char" => char_fn(&v),
        "unicode" => match &v[0] {
            Value::Null => Value::Null,
            other => eval::to_text(other)
                .chars()
                .next()
                .map(|c| Value::Integer(c as i64))
                .unwrap_or(Value::Null),
        },
        "iif" => {
            arity(&lname, args, 3)?;
            if eval::truth(&v[0]) == Some(true) {
                v[1].clone()
            } else {
                v[2].clone()
            }
        }
        "zeroblob" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                other => {
                    let n = eval::to_i64(other).max(0) as usize;
                    Value::Blob(alloc::vec![0u8; n])
                }
            }
        }
        "quote" => {
            arity(&lname, args, 1)?;
            Value::Text(quote_value(&v[0]))
        }
        "sign" => {
            arity(&lname, args, 1)?;
            match eval::to_number(&v[0]) {
                Value::Integer(i) => Value::Integer(i.signum()),
                Value::Real(r) => Value::Integer(if r > 0.0 {
                    1
                } else if r < 0.0 {
                    -1
                } else {
                    0
                }),
                _ => Value::Null,
            }
        }
        "concat" => {
            // SQLite 3.44+: concatenate all args, treating NULL as empty.
            let mut s = String::new();
            for x in &v {
                if !matches!(x, Value::Null) {
                    s.push_str(&eval::to_text(x));
                }
            }
            Value::Text(s)
        }
        "concat_ws" => {
            if v.is_empty() {
                return Err(Error::Error("concat_ws() needs a separator".into()));
            }
            if matches!(v[0], Value::Null) {
                Value::Null
            } else {
                let sep = eval::to_text(&v[0]);
                let parts: alloc::vec::Vec<String> = v[1..]
                    .iter()
                    .filter(|x| !matches!(x, Value::Null))
                    .map(eval::to_text)
                    .collect();
                Value::Text(parts.join(&sep))
            }
        }
        "unhex" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                other => match unhex(&eval::to_text(other)) {
                    Some(b) => Value::Blob(b),
                    None => Value::Null,
                },
            }
        }
        // Date/time functions (see `super::datetime`).
        "date" => super::datetime::date(&v),
        "time" => super::datetime::time(&v),
        "datetime" => super::datetime::datetime(&v),
        "julianday" => super::datetime::julianday(&v),
        "unixepoch" => super::datetime::unixepoch(&v),
        "strftime" => super::datetime::strftime(&v),
        "printf" | "format" => super::datetime::printf(&v),
        _ => return Err(Error::Unsupported("unknown scalar function")),
    })
}

/// Render a value as a SQL literal, like SQLite's `quote()`.
fn quote_value(v: &Value) -> String {
    match v {
        Value::Null => String::from("NULL"),
        Value::Integer(i) => alloc::format!("{i}"),
        Value::Real(r) => eval::format_real(*r),
        Value::Text(s) => alloc::format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => {
            let mut s = String::from("x'");
            for byte in b {
                s.push_str(&alloc::format!("{byte:02x}"));
            }
            s.push('\'');
            s
        }
    }
}

/// Decode a hex string to bytes (even length, all hex digits), else `None`.
fn unhex(s: &str) -> Option<alloc::vec::Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let hexval = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = alloc::vec::Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        out.push((hexval(bytes[i])? << 4) | hexval(bytes[i + 1])?);
        i += 2;
    }
    Some(out)
}

fn arity(name: &str, args: &[Expr], n: usize) -> Result<()> {
    if args.len() == n {
        Ok(())
    } else {
        Err(Error::Error(alloc::format!(
            "wrong number of arguments to function {name}() (want {n}, got {})",
            args.len()
        )))
    }
}

fn str_map(v: &Value, f: impl Fn(&str) -> String) -> Value {
    match v {
        Value::Null => Value::Null,
        other => Value::Text(f(&eval::to_text(other))),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Integer(_) => "integer",
        Value::Real(_) => "real",
        Value::Text(_) => "text",
        Value::Blob(_) => "blob",
    }
}

fn trim_fn(v: &[Value], left: bool, right: bool) -> Value {
    if v.is_empty() || matches!(v[0], Value::Null) {
        return Value::Null;
    }
    let s = eval::to_text(&v[0]);
    let trim_chars: Vec<char> = if v.len() >= 2 {
        eval::to_text(&v[1]).chars().collect()
    } else {
        alloc::vec![' ']
    };
    let is_trim = |c: char| trim_chars.contains(&c);
    let chars: Vec<char> = s.chars().collect();
    let mut start = 0;
    let mut end = chars.len();
    if left {
        while start < end && is_trim(chars[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && is_trim(chars[end - 1]) {
            end -= 1;
        }
    }
    Value::Text(chars[start..end].iter().collect())
}

fn substr(v: &[Value]) -> Result<Value> {
    if v.len() < 2 || v.len() > 3 {
        return Err(Error::Error("substr() takes 2 or 3 arguments".into()));
    }
    if matches!(v[0], Value::Null) {
        return Ok(Value::Null);
    }
    let s: Vec<char> = eval::to_text(&v[0]).chars().collect();
    let len = s.len() as i64;
    // 1-based start; a negative start counts from the end. Unlike a naive clamp,
    // SQLite keeps the requested window and only the positions in 1..=len are
    // returned, so `substr('hello',0,3)` yields "he" (positions 0,1,2 → 1,2).
    let mut start = eval::to_i64(&v[1]);
    if start < 0 {
        start += len + 1;
    }
    let (wstart, wend) = if v.len() == 3 {
        let z = eval::to_i64(&v[2]);
        if z < 0 {
            (start + z, start)
        } else {
            (start, start + z)
        }
    } else {
        (start, len + 1)
    };
    let b = wstart.max(1);
    let e = wend.min(len + 1);
    if b >= e {
        Ok(Value::Text(String::new()))
    } else {
        Ok(Value::Text(
            s[(b - 1) as usize..(e - 1) as usize].iter().collect(),
        ))
    }
}

fn instr(v: &[Value]) -> Result<Value> {
    if v.len() != 2 {
        return Err(Error::Error("instr() takes 2 arguments".into()));
    }
    if matches!(v[0], Value::Null) || matches!(v[1], Value::Null) {
        return Ok(Value::Null);
    }
    let hay = eval::to_text(&v[0]);
    let needle = eval::to_text(&v[1]);
    // SQLite returns a 1-based character index, 0 if not found.
    match hay.find(&needle) {
        None => Ok(Value::Integer(0)),
        Some(byte_idx) => {
            let char_idx = hay[..byte_idx].chars().count();
            Ok(Value::Integer(char_idx as i64 + 1))
        }
    }
}

fn replace(v: &[Value]) -> Result<Value> {
    if v.len() != 3 {
        return Err(Error::Error("replace() takes 3 arguments".into()));
    }
    if v.iter().any(|x| matches!(x, Value::Null)) {
        return Ok(Value::Null);
    }
    let s = eval::to_text(&v[0]);
    let from = eval::to_text(&v[1]);
    let to = eval::to_text(&v[2]);
    if from.is_empty() {
        return Ok(Value::Text(s));
    }
    Ok(Value::Text(s.replace(&from, &to)))
}

fn round(v: &[Value]) -> Result<Value> {
    if v.is_empty() || v.len() > 2 {
        return Err(Error::Error("round() takes 1 or 2 arguments".into()));
    }
    if matches!(v[0], Value::Null) {
        return Ok(Value::Null);
    }
    let x = eval::to_f64(&v[0]);
    let digits = if v.len() == 2 {
        eval::to_i64(&v[1]).max(0)
    } else {
        0
    };
    let factor = crate::util::float::powi(10.0, digits as i32);
    Ok(Value::Real(crate::util::float::round(x * factor) / factor))
}

fn scalar_min_max(v: &[Value], want_min: bool) -> Value {
    // Scalar min()/max() with 2+ args; NULL if any arg is NULL.
    if v.iter().any(|x| matches!(x, Value::Null)) {
        return Value::Null;
    }
    let mut best = v[0].clone();
    for x in &v[1..] {
        let ord = eval::compare(x, &best);
        let take = if want_min {
            ord == core::cmp::Ordering::Less
        } else {
            ord == core::cmp::Ordering::Greater
        };
        if take {
            best = x.clone();
        }
    }
    best
}

fn hex_encode(v: &Value) -> String {
    let bytes = match v {
        Value::Blob(b) => b.clone(),
        other => eval::to_text(other).into_bytes(),
    };
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(nibble(b >> 4));
        s.push(nibble(b & 0xf));
    }
    s
}

fn nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + n - 10) as char,
    }
}

fn char_fn(v: &[Value]) -> Value {
    let mut s = String::new();
    for x in v {
        if let Some(c) = char::from_u32(eval::to_i64(x) as u32) {
            s.push(c);
        }
    }
    Value::Text(s)
}
