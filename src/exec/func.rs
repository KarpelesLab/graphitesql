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

    // Connection-state functions: read counters off the subquery handler.
    match lname.as_str() {
        "last_insert_rowid" | "changes" | "total_changes" => {
            arity(&lname, args, 0)?;
            let n = ctx.subqueries.map_or(0, |s| match lname.as_str() {
                "last_insert_rowid" => s.last_insert_rowid(),
                "changes" => s.changes(),
                _ => s.total_changes(),
            });
            return Ok(Value::Integer(n));
        }
        _ => {}
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
            match &v[0] {
                Value::Null => Value::Null,
                Value::Integer(i) => Value::Integer(i.wrapping_abs()),
                Value::Real(r) => Value::Real(crate::util::float::abs(*r)),
                // A text/blob argument is coerced to a real (SQLite gives
                // `abs('5')` = 5.0, not 5).
                other => Value::Real(crate::util::float::abs(eval::to_f64(other))),
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
        "octet_length" => {
            arity(&lname, args, 1)?;
            // Number of bytes in the value's encoding: blobs as-is, everything
            // else as the UTF-8 length of its text representation.
            match &v[0] {
                Value::Null => Value::Null,
                Value::Blob(b) => Value::Integer(b.len() as i64),
                other => Value::Integer(eval::to_text(other).len() as i64),
            }
        }
        "glob" => {
            // glob(pattern, text) is the function form of `text GLOB pattern`.
            arity(&lname, args, 2)?;
            if v.iter().take(2).any(|x| matches!(x, Value::Null)) {
                Value::Null
            } else {
                let m = eval::glob_match(&eval::to_text(&v[0]), &eval::to_text(&v[1]));
                Value::Integer(m as i64)
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
            // Numeric (or losslessly-numeric text) only; otherwise NULL.
            let num = match &v[0] {
                Value::Integer(i) => Some(*i as f64),
                Value::Real(r) => Some(*r),
                Value::Text(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            match num {
                Some(r) if r > 0.0 => Value::Integer(1),
                Some(r) if r < 0.0 => Value::Integer(-1),
                Some(_) => Value::Integer(0),
                None => Value::Null,
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
        "like" => {
            // like(pattern, text[, escape]) — the function form of `text LIKE
            // pattern`. NULL operand → NULL.
            if v.len() < 2 || v.len() > 3 {
                return Err(Error::Error("like() takes 2 or 3 arguments".into()));
            }
            if v.iter().take(2).any(|x| matches!(x, Value::Null)) {
                Value::Null
            } else {
                let escape = v.get(2).map(eval::to_text).and_then(|s| s.chars().next());
                let m =
                    eval::like_match_escape(&eval::to_text(&v[0]), &eval::to_text(&v[1]), escape);
                Value::Integer(m as i64)
            }
        }
        // Optimizer hints that are no-ops at the value level (return the operand).
        "likely" | "unlikely" => {
            arity(&lname, args, 1)?;
            v[0].clone()
        }
        "likelihood" => {
            arity(&lname, args, 2)?;
            v[0].clone()
        }
        "unhex" => {
            if v.is_empty() || v.len() > 2 {
                return Err(Error::Error("unhex() takes 1 or 2 arguments".into()));
            }
            // `unhex(X, Y)` first removes any characters of Y from X.
            let cleaned = match v.get(1) {
                Some(Value::Null) => return Ok(Value::Null),
                Some(ignore) => {
                    let set: alloc::vec::Vec<char> = eval::to_text(ignore).chars().collect();
                    Some(
                        eval::to_text(&v[0])
                            .chars()
                            .filter(|c| !set.contains(c))
                            .collect::<String>(),
                    )
                }
                None => None,
            };
            match &v[0] {
                Value::Null => Value::Null,
                other => {
                    let text = cleaned.unwrap_or_else(|| eval::to_text(other));
                    match unhex(&text) {
                        Some(b) => Value::Blob(b),
                        None => Value::Null,
                    }
                }
            }
        }
        // Math functions (SQLite's `-DSQLITE_ENABLE_MATH_FUNCTIONS` set; the CLI
        // ships with these enabled). Each coerces its argument(s) to a real and
        // returns NULL when an argument is NULL or the result is not finite,
        // matching SQLite.
        "pi" => {
            arity(&lname, args, 0)?;
            Value::Real(crate::util::float::PI)
        }
        "ceil" | "ceiling" => math1(&lname, &v, crate::util::float::ceil)?,
        "floor" => math1(&lname, &v, crate::util::float::floor)?,
        "trunc" => math1(&lname, &v, crate::util::float::trunc)?,
        "sqrt" => math1(&lname, &v, crate::util::float::sqrt)?,
        "exp" => math1(&lname, &v, crate::util::float::exp)?,
        "ln" => math1(&lname, &v, crate::util::float::ln)?,
        "log2" => math1(&lname, &v, crate::util::float::log2)?,
        "sin" => math1(&lname, &v, crate::util::float::sin)?,
        "cos" => math1(&lname, &v, crate::util::float::cos)?,
        "tan" => math1(&lname, &v, crate::util::float::tan)?,
        "asin" => math1(&lname, &v, crate::util::float::asin)?,
        "acos" => math1(&lname, &v, crate::util::float::acos)?,
        "atan" => math1(&lname, &v, crate::util::float::atan)?,
        "sinh" => math1(&lname, &v, crate::util::float::sinh)?,
        "cosh" => math1(&lname, &v, crate::util::float::cosh)?,
        "tanh" => math1(&lname, &v, crate::util::float::tanh)?,
        "asinh" => math1(&lname, &v, crate::util::float::asinh)?,
        "acosh" => math1(&lname, &v, crate::util::float::acosh)?,
        "atanh" => math1(&lname, &v, crate::util::float::atanh)?,
        "degrees" => math1(&lname, &v, crate::util::float::degrees)?,
        "radians" => math1(&lname, &v, crate::util::float::radians)?,
        // `log(X)` is base-10; `log(B, X)` is base B. `log10` is base-10.
        "log10" => math1(&lname, &v, crate::util::float::log10)?,
        "log" => {
            if v.len() == 1 {
                math_finite(real_arg(&v[0]).map(crate::util::float::log10))
            } else {
                arity(&lname, args, 2)?;
                match (real_arg(&v[0]), real_arg(&v[1])) {
                    (Some(b), Some(x)) => {
                        math_finite(Some(crate::util::float::ln(x) / crate::util::float::ln(b)))
                    }
                    _ => Value::Null,
                }
            }
        }
        "pow" | "power" => {
            arity(&lname, args, 2)?;
            match (real_arg(&v[0]), real_arg(&v[1])) {
                (Some(b), Some(e)) => math_finite(Some(crate::util::float::pow(b, e))),
                _ => Value::Null,
            }
        }
        "atan2" => {
            arity(&lname, args, 2)?;
            match (real_arg(&v[0]), real_arg(&v[1])) {
                (Some(y), Some(x)) => math_finite(Some(crate::util::float::atan2(y, x))),
                _ => Value::Null,
            }
        }
        "mod" => {
            arity(&lname, args, 2)?;
            match (real_arg(&v[0]), real_arg(&v[1])) {
                (Some(x), Some(y)) => math_finite(Some(crate::util::float::fmod(x, y))),
                _ => Value::Null,
            }
        }
        // JSON functions (see `super::json`).
        "json" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                other => {
                    let text = eval::to_text(other);
                    match super::json::parse(&text) {
                        Some(j) => Value::Text(j.serialize()),
                        None => return Err(Error::Error("malformed JSON".into())),
                    }
                }
            }
        }
        "json_valid" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                other => {
                    let ok = super::json::parse(&eval::to_text(other)).is_some();
                    Value::Integer(ok as i64)
                }
            }
        }
        "json_quote" => {
            arity(&lname, args, 1)?;
            Value::Text(super::json::value_to_json(&v[0]).serialize())
        }
        "json_type" => {
            if v.is_empty() || v.len() > 2 {
                return Err(Error::Error("json_type() takes 1 or 2 arguments".into()));
            }
            match json_root(&v[0])? {
                None => Value::Null,
                Some(root) => {
                    let target = if v.len() == 2 {
                        super::json::navigate(&root, &eval::to_text(&v[1]))
                    } else {
                        Some(&root)
                    };
                    match target {
                        Some(j) => Value::Text(String::from(j.type_name())),
                        None => Value::Null,
                    }
                }
            }
        }
        "json_array_length" => {
            if v.is_empty() || v.len() > 2 {
                return Err(Error::Error(
                    "json_array_length() takes 1 or 2 arguments".into(),
                ));
            }
            match json_root(&v[0])? {
                None => Value::Null,
                Some(root) => {
                    let target = if v.len() == 2 {
                        super::json::navigate(&root, &eval::to_text(&v[1]))
                    } else {
                        Some(&root)
                    };
                    match target {
                        Some(super::json::Json::Array(items)) => Value::Integer(items.len() as i64),
                        Some(_) => Value::Integer(0),
                        None => Value::Null,
                    }
                }
            }
        }
        "json_extract" => {
            if v.len() < 2 {
                return Err(Error::Error(
                    "json_extract() requires at least 2 arguments".into(),
                ));
            }
            match json_root(&v[0])? {
                None => Value::Null,
                Some(root) => json_extract(&root, &v[1..]),
            }
        }
        "json_array" => {
            let mut items = Vec::with_capacity(v.len());
            for (i, val) in v.iter().enumerate() {
                items.push(arg_to_json(val, args.get(i)));
            }
            Value::Text(super::json::Json::Array(items).serialize())
        }
        "json_object" => {
            if !v.len().is_multiple_of(2) {
                return Err(Error::Error(
                    "json_object() requires an even number of arguments".into(),
                ));
            }
            let mut members = Vec::with_capacity(v.len() / 2);
            for pair in v.chunks(2).enumerate() {
                let (i, kv) = pair;
                let key = eval::to_text(&kv[0]);
                let val = arg_to_json(&kv[1], args.get(2 * i + 1));
                members.push((key, val));
            }
            Value::Text(super::json::Json::Object(members).serialize())
        }
        "json_set" | "json_insert" | "json_replace" => {
            if v.len() < 3 || v.len().is_multiple_of(2) {
                return Err(Error::Error(alloc::format!(
                    "{lname}() requires a document and (path, value) pairs"
                )));
            }
            let mode = match lname.as_str() {
                "json_set" => super::json::SetMode::Set,
                "json_insert" => super::json::SetMode::Insert,
                _ => super::json::SetMode::Replace,
            };
            match json_root(&v[0])? {
                None => Value::Null,
                Some(mut root) => {
                    let mut i = 1;
                    while i + 1 < v.len() {
                        let path = eval::to_text(&v[i]);
                        let val = arg_to_json(&v[i + 1], args.get(i + 1));
                        super::json::set_path(&mut root, &path, val, mode);
                        i += 2;
                    }
                    Value::Text(root.serialize())
                }
            }
        }
        "json_remove" => {
            if v.is_empty() {
                return Err(Error::Error("json_remove() requires a document".into()));
            }
            match json_root(&v[0])? {
                None => Value::Null,
                Some(mut root) => {
                    for p in &v[1..] {
                        super::json::remove_path(&mut root, &eval::to_text(p));
                    }
                    Value::Text(root.serialize())
                }
            }
        }
        "json_patch" => {
            arity(&lname, args, 2)?;
            match (json_root(&v[0])?, json_root(&v[1])?) {
                (Some(mut root), Some(patch)) => {
                    super::json::merge_patch(&mut root, &patch);
                    Value::Text(root.serialize())
                }
                _ => Value::Null,
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
        // `quote()` renders an infinity as `±9.0e+999` (unlike plain text output,
        // which prints `Inf`).
        Value::Real(r) if !r.is_finite() => {
            String::from(if *r < 0.0 { "-9.0e+999" } else { "9.0e+999" })
        }
        Value::Real(r) => eval::format_real(*r),
        Value::Text(s) => alloc::format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => {
            // SQLite renders blob literals as `X'ABCD'` — uppercase `X` and
            // uppercase hex digits.
            let mut s = String::from("X'");
            for byte in b {
                s.push_str(&alloc::format!("{byte:02X}"));
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

/// A numeric argument to a math function: `None` for SQL `NULL`, else the value
/// coerced to `f64` (SQLite applies REAL affinity to math-function arguments).
fn real_arg(v: &Value) -> Option<f64> {
    match v {
        Value::Null => None,
        other => Some(eval::to_f64(other)),
    }
}

/// Wrap a computed math result: `NULL` for a missing argument or a non-finite
/// result (domain error), else a `REAL`. Matches SQLite, where e.g. `ln(0)` and
/// `sqrt(-1)` yield `NULL`.
fn math_finite(r: Option<f64>) -> Value {
    match r {
        Some(x) if x.is_finite() => Value::Real(x),
        _ => Value::Null,
    }
}

/// A one-argument math function: arity-check, coerce, apply, finiteness-guard.
fn math1(name: &str, v: &[Value], f: impl Fn(f64) -> f64) -> Result<Value> {
    if v.len() != 1 {
        return Err(Error::Error(alloc::format!(
            "wrong number of arguments to function {name}()"
        )));
    }
    Ok(math_finite(real_arg(&v[0]).map(f)))
}

/// Parse the first argument of a JSON function as a document: `NULL` → `None`;
/// malformed JSON is an error (matching SQLite).
fn json_root(v: &Value) -> Result<Option<super::json::Json>> {
    match v {
        Value::Null => Ok(None),
        other => match super::json::parse(&eval::to_text(other)) {
            Some(j) => Ok(Some(j)),
            None => Err(Error::Error("malformed JSON".into())),
        },
    }
}

/// `json_extract`: one path returns the SQL value at that path (objects/arrays as
/// minified JSON text); multiple paths return a JSON array of the extracted
/// elements (missing paths become JSON `null`).
fn json_extract(root: &super::json::Json, paths: &[Value]) -> Value {
    if paths.len() == 1 {
        return match super::json::navigate(root, &eval::to_text(&paths[0])) {
            Some(j) => j.to_sql(),
            None => Value::Null,
        };
    }
    let items = paths
        .iter()
        .map(|p| match super::json::navigate(root, &eval::to_text(p)) {
            Some(j) => j.clone(),
            None => super::json::Json::Null,
        })
        .collect();
    Value::Text(super::json::Json::Array(items).serialize())
}

/// Convert a constructor argument to JSON. If the source expression is itself a
/// JSON-producing call (`json`, `json_array`, `json_object`), its text value is
/// embedded as parsed JSON — mirroring SQLite's JSON subtype propagation — rather
/// than quoted as a string.
fn arg_to_json(val: &Value, expr: Option<&Expr>) -> super::json::Json {
    if let (Value::Text(s), Some(e)) = (val, expr) {
        if produces_json(e) {
            if let Some(j) = super::json::parse(s) {
                return j;
            }
        }
    }
    super::json::value_to_json(val)
}

/// Whether an expression yields a value carrying SQLite's JSON subtype.
fn produces_json(e: &Expr) -> bool {
    match e {
        Expr::Function { name, .. } => matches!(
            name.to_ascii_lowercase().as_str(),
            "json"
                | "json_array"
                | "json_object"
                | "json_insert"
                | "json_replace"
                | "json_set"
                | "json_patch"
                | "json_remove"
        ),
        Expr::Paren(inner) => produces_json(inner),
        _ => false,
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
    // `substr` of a blob slices bytes and returns a blob; otherwise it slices
    // characters of the text form and returns text.
    let blob = matches!(v[0], Value::Blob(_));
    let units: alloc::vec::Vec<char> = if blob {
        match &v[0] {
            Value::Blob(b) => b.iter().map(|&x| x as char).collect(),
            _ => unreachable!(),
        }
    } else {
        eval::to_text(&v[0]).chars().collect()
    };
    let len = units.len() as i64;
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
    let slice = if b >= e {
        &[][..]
    } else {
        &units[(b - 1) as usize..(e - 1) as usize]
    };
    if blob {
        Ok(Value::Blob(slice.iter().map(|&c| c as u8).collect()))
    } else {
        Ok(Value::Text(slice.iter().collect()))
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
        eval::to_i64(&v[1]).clamp(0, 30) as u32
    } else {
        0
    };
    Ok(Value::Real(round_half_away(x, digits)))
}

/// Round `x` to `n` decimal places, half away from zero, matching SQLite. Instead
/// of `round(x * 10^n) / 10^n` — which loses precision (e.g. `2.675 * 100` rounds
/// *up* to exactly `267.5` in f64, giving 2.68 where SQLite gives 2.67) — this
/// formats `x` to high fixed precision (exposing the true decimal digits) and
/// rounds the digit string, so it sees that `2.675` is really `2.67499…`.
pub(crate) fn round_half_away(x: f64, n: u32) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    // Values at or beyond 2^52 have no fractional part in f64; return unchanged
    // (this also bounds the formatted string length).
    if crate::util::float::abs(x) >= 4_503_599_627_370_496.0 {
        return x;
    }
    let neg = x < 0.0;
    let ax = crate::util::float::abs(x);
    let prec = n as usize + 25;
    let s = alloc::format!("{ax:.prec$}");
    let dot = s.find('.').unwrap_or(s.len());
    let frac = if dot < s.len() { &s[dot + 1..] } else { "" };
    // Round up when the first dropped digit (position `n`) is >= 5.
    let round_up = frac.as_bytes().get(n as usize).is_some_and(|&d| d >= b'5');
    // Kept digits: the integer part followed by the first `n` fractional digits.
    let mut digits: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    digits.extend_from_slice(&s.as_bytes()[..dot]);
    if n > 0 {
        let take = (n as usize).min(frac.len());
        digits.extend_from_slice(&frac.as_bytes()[..take]);
        // Pad with zeros if the formatting produced fewer than n fraction digits.
        digits.resize(dot + n as usize, b'0');
    }
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    // Reassemble with the decimal point `n` digits from the right and parse back.
    let nn = n as usize;
    let s2 = if nn == 0 {
        alloc::string::String::from_utf8(digits).unwrap_or_default()
    } else {
        let point = digits.len() - nn;
        let mut out = alloc::string::String::new();
        out.push_str(core::str::from_utf8(&digits[..point]).unwrap_or("0"));
        out.push('.');
        out.push_str(core::str::from_utf8(&digits[point..]).unwrap_or("0"));
        out
    };
    let mag: f64 = s2.parse().unwrap_or(ax);
    if neg {
        -mag
    } else {
        mag
    }
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
