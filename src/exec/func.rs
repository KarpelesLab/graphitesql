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

/// SQLite's default `SQLITE_MAX_LENGTH`: the largest string/blob a single value
/// may hold. `zeroblob`/`randomblob` error with "string or blob too big" past it
/// rather than attempting a multi-gigabyte allocation.
const MAX_BLOB_LEN: usize = 1_000_000_000;

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
        "count" | "sum" | "total" | "avg" | "group_concat" | "string_agg" => true,
        "json_group_array" | "jsonb_group_array" => nargs == 1,
        "json_group_object" | "jsonb_group_object" => nargs == 2,
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
        "random" => {
            arity(&lname, args, 0)?;
            return Ok(Value::Integer(
                ctx.subqueries.map_or(0, |s| s.next_random()),
            ));
        }
        "randomblob" => {
            arity(&lname, args, 1)?;
            // SQLite coerces the argument to an integer (NULL/non-numeric text
            // become 0, reals truncate) and clamps N < 1 to a single byte — so
            // randomblob(NULL) is a 1-byte blob, not NULL.
            let n = eval::to_i64(&eval::eval(&args[0], ctx)?);
            let len = if n < 1 { 1 } else { n as usize };
            if len > MAX_BLOB_LEN {
                return Err(Error::Error("string or blob too big".into()));
            }
            let mut bytes = Vec::new();
            if let Some(s) = ctx.subqueries {
                while bytes.len() < len {
                    bytes.extend_from_slice(&s.next_random().to_le_bytes());
                }
                bytes.truncate(len);
            } else {
                bytes.resize(len, 0);
            }
            return Ok(Value::Blob(bytes));
        }
        _ => {}
    }

    // Functions whose NULL-handling is special are done before arg evaluation.
    match lname.as_str() {
        "coalesce" => {
            // SQLite requires at least two arguments.
            if args.len() < 2 {
                return Err(Error::Error(alloc::format!(
                    "wrong number of arguments to function coalesce() (want at least 2, got {})",
                    args.len()
                )));
            }
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
                // `abs(-9223372036854775808)` has no i64 result — SQLite errors.
                Value::Integer(i) => match i.checked_abs() {
                    Some(a) => Value::Integer(a),
                    None => return Err(Error::Error("integer overflow".into())),
                },
                // `+ 0.0` normalises a negative-zero result to `0.0`, matching
                // SQLite (`abs(-0.0)` is `0.0`, not `-0.0`).
                Value::Real(r) => Value::Real(crate::util::float::abs(*r) + 0.0),
                // A text/blob argument is coerced to a real (SQLite gives
                // `abs('5')` = 5.0, not 5).
                other => Value::Real(crate::util::float::abs(eval::to_f64(other)) + 0.0),
            }
        }
        "length" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                Value::Blob(b) => Value::Integer(b.len() as i64),
                // For a string, SQLite counts characters up to (not including)
                // the first NUL — `length('A'||char(0)||'B')` is 1, not 3.
                // Numbers stringify without NULs, so they are unaffected.
                other => {
                    let t = eval::to_text(other);
                    let n = match t.find('\0') {
                        Some(i) => t[..i].chars().count(),
                        None => t.chars().count(),
                    };
                    Value::Integer(n as i64)
                }
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
        "soundex" => {
            arity(&lname, args, 1)?;
            // NULL/non-alpha input yields "?000" (SQLite does not propagate NULL).
            Value::Text(soundex(&c_text(&v[0])))
        }
        "typeof" => Value::Text(String::from(type_name(&v[0]))),
        "nullif" => {
            arity(&lname, args, 2)?;
            // The comparison follows the standard binary-comparison collation
            // rule: an explicit `COLLATE` on either operand wins (left-preferred),
            // else a column's declared collation, else BINARY — so
            // `NULLIF('a','A' COLLATE NOCASE)` is NULL.
            let coll = eval::resolve_collation(&args[0], &args[1], ctx);
            if crate::value::cmp_values_coll(&v[0], &v[1], coll) == core::cmp::Ordering::Equal {
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
        "min" => scalar_min_max(&v, true)?,
        "max" => scalar_min_max(&v, false)?,
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
        // `if` is SQLite's alias for `iif`. Both take 2 or 3 arguments: the
        // 2-argument form yields NULL when the condition is not true.
        "iif" | "if" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(Error::Error(alloc::format!(
                    "wrong number of arguments to function {lname}() (want 2 or 3, got {})",
                    args.len()
                )));
            }
            if eval::truth(&v[0]) == Some(true) {
                v[1].clone()
            } else {
                v.get(2).cloned().unwrap_or(Value::Null)
            }
        }
        // The SQLite release graphitesql tracks and writes into new file headers
        // (`SQLITE_VERSION_NUMBER` 3_053_002 = 3.53.2).
        "sqlite_version" => {
            arity(&lname, args, 0)?;
            Value::Text(crate::TARGET_SQLITE_VERSION.into())
        }
        "zeroblob" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                other => {
                    let n = eval::to_i64(other).max(0) as usize;
                    if n > MAX_BLOB_LEN {
                        return Err(Error::Error("string or blob too big".into()));
                    }
                    Value::Blob(alloc::vec![0u8; n])
                }
            }
        }
        "quote" => {
            arity(&lname, args, 1)?;
            Value::Text(quote_value(&v[0]))
        }
        "unistr" => {
            // Decode `\uXXXX` / `\UXXXXXXXX` / `\\` escapes in the argument's
            // text. NULL passes through; any other escape errors, as in SQLite.
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                other => Value::Text(unistr_decode(&eval::to_text(other))?),
            }
        }
        "unistr_quote" => {
            // Like quote(), except a text value containing a control character
            // (< U+0020) is rendered as `unistr('…')` with those characters
            // escaped `\uXXXX`. Non-text values match quote() exactly.
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Text(s) if s.chars().any(|c| (c as u32) < 0x20) => {
                    Value::Text(unistr_quote_text(s))
                }
                other => Value::Text(quote_value(other)),
            }
        }
        "subtype" => {
            // The value's subtype. graphite tracks no subtypes, so this is
            // always 0 (SQLite also returns 0 for ordinary, non-JSON values).
            arity(&lname, args, 1)?;
            Value::Integer(0)
        }
        "sign" => {
            arity(&lname, args, 1)?;
            // Numeric (or losslessly-numeric text) only; otherwise NULL.
            let num = match &v[0] {
                Value::Integer(i) => Some(*i as f64),
                Value::Real(r) => Some(*r),
                Value::Text(s) => eval::parse_decimal_f64(s.trim()),
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
            // At least one argument is required.
            if v.is_empty() {
                return Err(Error::Error(
                    "wrong number of arguments to function concat() (want at least 1, got 0)"
                        .into(),
                ));
            }
            let mut s = String::new();
            for x in &v {
                if !matches!(x, Value::Null) {
                    s.push_str(&eval::to_text(x));
                }
            }
            Value::Text(s)
        }
        "concat_ws" => {
            // A separator plus at least one value argument are required.
            if v.len() < 2 {
                return Err(Error::Error(alloc::format!(
                    "wrong number of arguments to function concat_ws() (want at least 2, got {})",
                    v.len()
                )));
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
            // A NULL escape, like a NULL pattern/text, yields NULL.
            if v.iter().any(|x| matches!(x, Value::Null)) {
                Value::Null
            } else {
                // An explicit ESCAPE must be exactly one character (SQLite
                // raises "ESCAPE expression must be a single character" for
                // empty or multi-character escapes).
                let escape = match v.get(2) {
                    Some(e) => {
                        let s = eval::to_text(e);
                        let mut it = s.chars();
                        match (it.next(), it.next()) {
                            (Some(c), None) => Some(c),
                            _ => {
                                return Err(Error::Error(
                                    "ESCAPE expression must be a single character".into(),
                                ));
                            }
                        }
                    }
                    None => None,
                };
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
            // A NULL hex string or NULL ignore-set both yield NULL.
            let ignore = match v.get(1) {
                Some(Value::Null) => return Ok(Value::Null),
                // The ignore set is itself coerced as a C string (NUL-terminated).
                Some(set) => Some(c_text(set)),
                None => None,
            };
            match &v[0] {
                Value::Null => Value::Null,
                // The hex input is read as a NUL-terminated C string, matching
                // SQLite's `unhexFunc` (`sqlite3_value_text`).
                other => match unhex(&c_text(other), ignore.as_deref()) {
                    Some(b) => Value::Blob(b),
                    None => Value::Null,
                },
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
                    // `log(B, X)` is NULL unless `B > 0`, `B != 1`, and `X > 0`
                    // (SQLite). A base of 1 makes `ln(B) == 0`, which the bare
                    // division would turn into ±Inf instead of NULL, so guard it.
                    (Some(1.0), Some(_)) => Value::Null,
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
            match json_root(&v[0])? {
                None => Value::Null,
                Some(j) => Value::Text(j.serialize()),
            }
        }
        // `jsonb(X)` — the JSONB (binary) form of `json(X)`: parse the JSON/JSON5
        // text (or pass a JSONB blob through) and return its JSONB encoding.
        "jsonb" => {
            arity(&lname, args, 1)?;
            match json_root(&v[0])? {
                None => Value::Null,
                Some(j) => Value::Blob(j.to_jsonb()),
            }
        }
        "json_valid" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                // The 1-argument form is restricted to strict RFC-8259 JSON
                // (sqlite reserves JSON5 acceptance for the 2-argument flag form),
                // unlike `json()`/`json_extract()` which accept the JSON5 superset.
                other => {
                    let ok = super::json::is_strict_json(&eval::to_text(other));
                    Value::Integer(ok as i64)
                }
            }
        }
        // `json_error_position(X)` — the 1-based byte position of the first JSON
        // syntax error in X, or 0 when X is well-formed JSON. NULL yields NULL,
        // matching sqlite3.
        "json_error_position" => {
            arity(&lname, args, 1)?;
            match &v[0] {
                Value::Null => Value::Null,
                other => {
                    let pos = match super::json::parse_with_error_position(&eval::to_text(other)) {
                        Ok(_) => 0,
                        Err(off) => off as i64 + 1,
                    };
                    Value::Integer(pos)
                }
            }
        }
        // `json_pretty(X [, indent])` — reformat with indentation (default 4
        // spaces). Empty arrays/objects and scalars stay compact, like SQLite.
        "json_pretty" => {
            if v.is_empty() || v.len() > 2 {
                return Err(Error::Error("json_pretty() takes 1 or 2 arguments".into()));
            }
            match &v[0] {
                Value::Null => Value::Null,
                other => {
                    let indent = match v.get(1) {
                        Some(Value::Null) | None => alloc::string::String::from("    "),
                        Some(iv) => eval::to_text(iv),
                    };
                    let _ = other;
                    match json_root(&v[0])? {
                        None => Value::Null,
                        Some(j) => Value::Text(j.pretty(&indent)),
                    }
                }
            }
        }
        "json_quote" => {
            arity(&lname, args, 1)?;
            reject_blob(&v[0])?;
            Value::Text(super::json::value_to_json(&v[0]).quote())
        }
        "json_type" => {
            if v.is_empty() || v.len() > 2 {
                return Err(Error::Error("json_type() takes 1 or 2 arguments".into()));
            }
            match json_root(&v[0])? {
                None => Value::Null,
                Some(root) => {
                    let target = if v.len() == 2 {
                        check_path(&v[1])?;
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
                        check_path(&v[1])?;
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
        "json_extract" | "jsonb_extract" => {
            if v.len() < 2 {
                return Err(Error::Error(
                    "json_extract() requires at least 2 arguments".into(),
                ));
            }
            match json_root(&v[0])? {
                None => Value::Null,
                Some(root) => json_extract(&root, &v[1..], lname.starts_with("jsonb"))?,
            }
        }
        "json_array" | "jsonb_array" => {
            let mut items = Vec::with_capacity(v.len());
            for (i, val) in v.iter().enumerate() {
                items.push(json_value_arg(val, args.get(i))?);
            }
            json_doc_result(&lname, &super::json::Json::Array(items))
        }
        "json_object" | "jsonb_object" => {
            if !v.len().is_multiple_of(2) {
                return Err(Error::Error(
                    "json_object() requires an even number of arguments".into(),
                ));
            }
            let mut members = Vec::with_capacity(v.len() / 2);
            for pair in v.chunks(2).enumerate() {
                let (i, kv) = pair;
                // SQLite requires object labels to be TEXT (a NULL, numeric, or
                // BLOB key is an error), and rejects BLOB values.
                let Value::Text(key) = &kv[0] else {
                    return Err(Error::Error("json_object() labels must be TEXT".into()));
                };
                let val = json_value_arg(&kv[1], args.get(2 * i + 1))?;
                members.push((key.clone(), val));
            }
            json_doc_result(&lname, &super::json::Json::Object(members))
        }
        "json_set" | "json_insert" | "json_replace" | "jsonb_set" | "jsonb_insert"
        | "jsonb_replace" => {
            if v.len() < 3 || v.len().is_multiple_of(2) {
                return Err(Error::Error(alloc::format!(
                    "{lname}() requires a document and (path, value) pairs"
                )));
            }
            let mode = if lname.ends_with("set") {
                super::json::SetMode::Set
            } else if lname.ends_with("insert") {
                super::json::SetMode::Insert
            } else {
                super::json::SetMode::Replace
            };
            match json_root(&v[0])? {
                None => Value::Null,
                Some(mut root) => {
                    let mut i = 1;
                    while i + 1 < v.len() {
                        check_path(&v[i])?;
                        let path = eval::to_text(&v[i]);
                        let val = json_value_arg(&v[i + 1], args.get(i + 1))?;
                        super::json::set_path(&mut root, &path, val, mode);
                        i += 2;
                    }
                    json_doc_result(&lname, &root)
                }
            }
        }
        "json_remove" | "jsonb_remove" => {
            if v.is_empty() {
                return Err(Error::Error("json_remove() requires a document".into()));
            }
            match json_root(&v[0])? {
                None => Value::Null,
                Some(mut root) => {
                    let mut removed = Value::Text(String::new());
                    for p in &v[1..] {
                        check_path(p)?;
                        // Removing the whole document (`$`) yields SQL NULL.
                        if matches!(p, Value::Text(s) if s == "$") {
                            removed = Value::Null;
                            continue;
                        }
                        super::json::remove_path(&mut root, &eval::to_text(p));
                    }
                    if matches!(removed, Value::Null) {
                        Value::Null
                    } else {
                        json_doc_result(&lname, &root)
                    }
                }
            }
        }
        "json_patch" | "jsonb_patch" => {
            arity(&lname, args, 2)?;
            match (json_root(&v[0])?, json_root(&v[1])?) {
                (Some(mut root), Some(patch)) => {
                    super::json::merge_patch(&mut root, &patch);
                    json_doc_result(&lname, &root)
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
        "timediff" => {
            arity(&lname, args, 2)?;
            super::datetime::timediff(&v[0], &v[1])
        }
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

/// Decode SQLite `unistr()` escapes: `\uXXXX` (4 hex), `\UXXXXXXXX` (8 hex), and
/// `\\` (a literal backslash). Any other backslash sequence — including a `\u`/
/// `\U` with too few hex digits or a trailing `\` — is an error, matching
/// SQLite's "invalid Unicode escape". A code point Rust cannot represent (a lone
/// surrogate or one past U+10FFFF) becomes the replacement char `U+FFFD`; SQLite
/// emits raw WTF-8 there, an extreme edge graphite cannot store in a `String`.
fn unistr_decode(s: &str) -> Result<String> {
    let cs: alloc::vec::Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let invalid = || Error::Error(String::from("invalid Unicode escape"));
    let hex_char = |cs: &[char], at: usize, n: usize| -> Option<u32> {
        let slice = cs.get(at..at + n)?;
        if !slice.iter().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let s: String = slice.iter().collect();
        u32::from_str_radix(&s, 16).ok()
    };
    let mut i = 0;
    while i < cs.len() {
        if cs[i] != '\\' {
            out.push(cs[i]);
            i += 1;
            continue;
        }
        match cs.get(i + 1) {
            Some('\\') => {
                out.push('\\');
                i += 2;
            }
            Some('u') => {
                let cp = hex_char(&cs, i + 2, 4).ok_or_else(invalid)?;
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                i += 6;
            }
            Some('U') => {
                let cp = hex_char(&cs, i + 2, 8).ok_or_else(invalid)?;
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                i += 10;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(out)
}

/// Render a control-character-bearing text value as SQLite's `unistr('…')`: each
/// character below U+0020 becomes `\uXXXX`, a backslash doubles, a single quote
/// doubles, everything else (including non-ASCII) is kept literal.
fn unistr_quote_text(s: &str) -> String {
    let mut out = String::from("unistr('");
    for c in s.chars() {
        let cp = c as u32;
        if cp < 0x20 {
            out.push_str(&alloc::format!("\\u{cp:04x}"));
        } else if c == '\\' {
            out.push_str("\\\\");
        } else if c == '\'' {
            out.push_str("''");
        } else {
            out.push(c);
        }
    }
    out.push_str("')");
    out
}

/// Decode a hex string to bytes, returning `None` on malformed input. A faithful
/// port of SQLite's `unhexFunc`:
///
/// - with no ignore set (`ignore` is `None`), the input must be an even number of
///   hex digits and nothing else;
/// - with an ignore set (the 2-argument form), characters from `ignore` may
///   appear *before*, *between*, and *after* complete byte pairs, but never
///   *within* a pair — so `unhex('AB CD', ' ')` is `X'ABCD'` while
///   `unhex('A BCD', ' ')` is `NULL`. Any character that is neither a hex digit
///   nor in the ignore set fails the whole decode.
fn unhex(s: &str, ignore: Option<&str>) -> Option<alloc::vec::Vec<u8>> {
    let hexval = |c: char| -> Option<u8> {
        match c {
            '0'..='9' => Some(c as u8 - b'0'),
            'a'..='f' => Some(c as u8 - b'a' + 10),
            'A'..='F' => Some(c as u8 - b'A' + 10),
            _ => None,
        }
    };
    let ignored = |c: char| -> bool { ignore.is_some_and(|set| set.contains(c)) };
    let mut out = alloc::vec::Vec::new();
    let mut it = s.chars();
    loop {
        // Skip any leading/inter-pair ignore characters. A non-ignored,
        // non-hex-digit character (or running out of input) ends this phase.
        let hi = loop {
            match it.next() {
                None => return Some(out),
                Some(c) if hexval(c).is_some() => break c,
                Some(c) if ignored(c) => continue,
                Some(_) => return None,
            }
        };
        // The second nibble must immediately follow the first.
        let lo = it.next()?;
        out.push((hexval(hi)? << 4) | hexval(lo)?);
    }
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

/// SQLite's `soundex(X)`: the phonetic code of the first word in `X` — the first
/// letter followed by up to three digits, padded with `0`. Input with no letters
/// (including NULL, which `to_text` maps to "") yields `"?000"`. Faithful port of
/// `soundexFunc`: each letter maps to a digit; a digit is emitted only when it is
/// nonzero and differs from the previous letter's code; a zero-code character
/// (vowel, `H`/`W`/`Y`, or non-letter) resets the running code.
fn soundex(s: &str) -> String {
    // Code for a-z (index `c.to_ascii_lowercase() - b'a'`); 0 = not coded.
    const CODE: [u8; 26] = [
        0, 1, 2, 3, 0, 1, 2, 0, 0, 2, 2, 4, 5, 5, 0, 1, 2, 6, 2, 3, 0, 1, 0, 2, 0, 2,
    ];
    let code_of = |c: u8| -> u8 {
        if c.is_ascii_alphabetic() {
            CODE[(c.to_ascii_lowercase() - b'a') as usize]
        } else {
            0
        }
    };
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && !b[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i >= b.len() {
        return String::from("?000");
    }
    let mut out = String::with_capacity(4);
    out.push(b[i].to_ascii_uppercase() as char);
    let mut prev = code_of(b[i]);
    let mut j = 1;
    while j < 4 && i < b.len() {
        let code = code_of(b[i]);
        if code > 0 {
            if code != prev {
                prev = code;
                out.push((b'0' + code) as char);
                j += 1;
            }
        } else {
            prev = 0;
        }
        i += 1;
    }
    while j < 4 {
        out.push('0');
        j += 1;
    }
    out
}

/// Coerce a value to text for the string functions (`trim`/`upper`/`lower`/
/// `replace`/`substr`/`soundex`).
///
/// When the value is a BLOB, SQLite reads it as a NUL-terminated C string
/// (`sqlite3_value_text`), so an embedded `NUL` byte truncates the coerced
/// text: `trim(X'00200041…')` is `''`, not `'   A   '`. We reproduce that for
/// the blob path. A genuine TEXT value keeps its embedded NULs: this engine's
/// TEXT model is NUL-preserving (e.g. the JSON5 `\0` escape stores a real NUL
/// character — see `tests/json5.rs`), and `length`/`unicode` count through it,
/// so truncating TEXT here would be inconsistent with the rest of the engine.
fn c_text(v: &Value) -> String {
    match v {
        Value::Blob(b) => {
            let end = b.iter().position(|&x| x == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
        other => eval::to_text(other),
    }
}

fn str_map(v: &Value, f: impl Fn(&str) -> String) -> Value {
    match v {
        Value::Null => Value::Null,
        other => Value::Text(f(&c_text(other))),
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

/// Wrap a computed math result, matching SQLite's split between a *domain error*
/// and an *overflow*:
///
/// - a missing argument or a `NaN` result (`sqrt(-1)`, `ln(0)`, `acos(2)`, …)
///   becomes SQL `NULL`;
/// - a `±∞` result is kept as a `REAL` — both genuine overflow (`exp(710)`,
///   `pow(2,2000)`) and poles (`pow(0,-1)`, `atanh(1)`) render as `Inf`/`-Inf`,
///   exactly as SQLite reports them.
///
/// Each underlying `float` routine is responsible for returning `NaN` (not `±∞`)
/// on a domain error, so that the NULL-vs-Inf decision is made consistently here.
fn math_finite(r: Option<f64>) -> Value {
    match r {
        Some(x) if x.is_nan() => Value::Null,
        Some(x) => Value::Real(x),
        None => Value::Null,
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
/// Render a JSON document result as text (`json_*`) or as a JSONB blob
/// (`jsonb_*`), chosen by the function name.
fn json_doc_result(lname: &str, j: &super::json::Json) -> Value {
    if lname.starts_with("jsonb") {
        Value::Blob(j.to_jsonb())
    } else {
        Value::Text(j.serialize())
    }
}

fn json_root(v: &Value) -> Result<Option<super::json::Json>> {
    match v {
        Value::Null => Ok(None),
        // A BLOB document is JSONB (SQLite's binary JSON); text is JSON/JSON5.
        Value::Blob(b) => match super::json::Json::from_jsonb(b) {
            Some(j) => Ok(Some(j)),
            None => Err(Error::Error("malformed JSON".into())),
        },
        other => match super::json::parse(&eval::to_text(other)) {
            Some(j) => Ok(Some(j)),
            None => Err(Error::Error("malformed JSON".into())),
        },
    }
}

/// Validate a JSON path argument, raising SQLite's `bad JSON path: '<path>'`
/// error if it is malformed. A `NULL` path is left for the caller to treat as a
/// missing lookup (SQLite returns NULL rather than erroring on a NULL path).
fn check_path(p: &Value) -> Result<()> {
    if let Value::Text(s) = p {
        if !super::json::path_is_valid(s) {
            return Err(Error::Error(alloc::format!("bad JSON path: '{s}'")));
        }
    }
    Ok(())
}

/// Reject a BLOB argument to a JSON constructor/mutator: SQLite cannot store a
/// BLOB in JSON and raises `JSON cannot hold BLOB values`.
fn reject_blob(v: &Value) -> Result<()> {
    if matches!(v, Value::Blob(_)) {
        return Err(Error::Error("JSON cannot hold BLOB values".into()));
    }
    Ok(())
}

/// Convert a *value* argument of a JSON/JSONB constructor or mutator to JSON. A
/// BLOB is embedded as its JSON when it decodes as JSONB (so `jsonb_*` results
/// compose, e.g. `jsonb_object('a', jsonb_array(1,2))`) and otherwise rejected —
/// graphite has no value subtypes, so it falls back to "does it parse as JSONB".
fn json_value_arg(val: &Value, expr: Option<&Expr>) -> Result<super::json::Json> {
    if let Value::Blob(b) = val {
        return super::json::Json::from_jsonb(b)
            .ok_or_else(|| Error::Error("JSON cannot hold BLOB values".into()));
    }
    Ok(arg_to_json(val, expr))
}

/// `json_extract`: one path returns the SQL value at that path (objects/arrays as
/// minified JSON text); multiple paths return a JSON array of the extracted
/// elements (missing paths become JSON `null`). `jsonb_extract` is the same but
/// returns object/array results (and the multi-path array) as JSONB blobs.
fn json_extract(root: &super::json::Json, paths: &[Value], jsonb: bool) -> Result<Value> {
    for p in paths {
        check_path(p)?;
    }
    // A non-scalar single-path result is JSONB under jsonb_extract; scalars are
    // returned as their SQL value either way.
    let scalar_or_doc = |j: &super::json::Json| -> Value {
        match j {
            super::json::Json::Array(_) | super::json::Json::Object(_) if jsonb => {
                Value::Blob(j.to_jsonb())
            }
            _ => j.to_sql(),
        }
    };
    if paths.len() == 1 {
        return Ok(
            match super::json::navigate(root, &eval::to_text(&paths[0])) {
                Some(j) => scalar_or_doc(j),
                None => Value::Null,
            },
        );
    }
    let items = paths
        .iter()
        .map(|p| match super::json::navigate(root, &eval::to_text(p)) {
            Some(j) => j.clone(),
            None => super::json::Json::Null,
        })
        .collect();
    let arr = super::json::Json::Array(items);
    Ok(if jsonb {
        Value::Blob(arr.to_jsonb())
    } else {
        Value::Text(arr.serialize())
    })
}

/// Convert a constructor argument to JSON. If the source expression is itself a
/// JSON-producing call (`json`, `json_array`, `json_object`), its text value is
/// embedded as parsed JSON — mirroring SQLite's JSON subtype propagation — rather
/// than quoted as a string.
pub(crate) fn arg_to_json(val: &Value, expr: Option<&Expr>) -> super::json::Json {
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
    let s = c_text(&v[0]);
    let trim_chars: Vec<char> = if v.len() >= 2 {
        c_text(&v[1]).chars().collect()
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
    // Faithful port of SQLite's `substrFunc` (src/func.c): `p1` is a 1-based
    // start (negative counts from the end), `p2` a signed length (negative
    // means "the |p2| units ending at p1"). The default `p2` for the 2-arg form
    // is the LENGTH limit (1e9), which we clamp to `len` below. All arithmetic
    // saturates so pathological i64 inputs (e.g. i64::MIN) cannot overflow,
    // matching SQLite's behaviour where the window collapses to empty or the
    // whole string.
    let mut p1 = eval::to_i64(&v[1]);
    let mut p2 = if v.len() == 3 {
        // `substr(x, p1, NULL)` returns NULL (the length argument is required
        // to be non-NULL to produce a value).
        if matches!(v[2], Value::Null) {
            return Ok(Value::Null);
        }
        eval::to_i64(&v[2])
    } else {
        1_000_000_000
    };
    if p1 < 0 {
        p1 = p1.saturating_add(len);
        if p1 < 0 {
            if p2 < 0 {
                p2 = 0;
            } else {
                p2 = p2.saturating_add(p1);
            }
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 -= 1;
    } else if p2 > 0 {
        p2 -= 1;
    }
    if p2 < 0 {
        if p2 < -p1 {
            p2 = p1;
        } else {
            p2 = -p2;
        }
        p1 = p1.saturating_sub(p2);
    }
    // `p1 >= 0 && p2 >= 0` now holds. Clamp the window to the available units.
    let start = (p1.max(0) as usize).min(units.len());
    let take = (p2.max(0) as usize).min(units.len() - start);
    let slice = &units[start..start + take];
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
    let s = c_text(&v[0]);
    let from = c_text(&v[1]);
    let to = c_text(&v[2]);
    if from.is_empty() {
        return Ok(Value::Text(s));
    }
    Ok(Value::Text(s.replace(&from, &to)))
}

fn round(v: &[Value]) -> Result<Value> {
    if v.is_empty() || v.len() > 2 {
        return Err(Error::Error("round() takes 1 or 2 arguments".into()));
    }
    // A NULL value or NULL precision both yield NULL.
    if matches!(v[0], Value::Null) || matches!(v.get(1), Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let x = eval::to_f64(&v[0]);
    let digits = if v.len() == 2 {
        eval::to_i64(&v[1]).clamp(0, 30) as u32
    } else {
        0
    };
    let r = round_half_away(x, digits);
    // SQLite normalises a negative-zero result to positive zero
    // (`round(-0.4)` is `0.0`, not `-0.0`).
    Ok(Value::Real(if r == 0.0 { 0.0 } else { r }))
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

fn scalar_min_max(v: &[Value], want_min: bool) -> Result<Value> {
    // Scalar min()/max() take 2+ args (the 1-arg/`*` forms are aggregates,
    // routed elsewhere); 0 args is an error in SQLite.
    if v.is_empty() {
        let name = if want_min { "min" } else { "max" };
        return Err(Error::Error(alloc::format!(
            "wrong number of arguments to function {name}()"
        )));
    }
    // NULL if any arg is NULL.
    if v.iter().any(|x| matches!(x, Value::Null)) {
        return Ok(Value::Null);
    }
    // Mirror SQLite's `minmaxFunc`: scan left-to-right keeping the best so far.
    // For `min`, replace whenever `best >= candidate` (so on a tie the *later*
    // argument wins — `min(1.0,1)` is the integer `1`); for `max`, replace only
    // when `best < candidate` (so on a tie the *earlier* argument wins —
    // `max(1.0,1)` is the real `1.0`). This preserves the storage class of the
    // exact argument SQLite would return.
    let mut best = v[0].clone();
    for x in &v[1..] {
        let ord = eval::compare(&best, x);
        let take = if want_min {
            ord != core::cmp::Ordering::Less
        } else {
            ord == core::cmp::Ordering::Less
        };
        if take {
            best = x.clone();
        }
    }
    Ok(best)
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
        // SQLite's `charFunc` reads each argument as a code point and substitutes
        // U+FFFD for an out-of-range value (negative or > U+10FFFF) rather than
        // dropping the character. Surrogates (U+D800..=U+DFFF) cannot be held in
        // Rust's UTF-8 `String`, so they too fall back to U+FFFD (SQLite emits
        // their raw 3-byte encoding — a faithful match would need invalid UTF-8,
        // which this engine's TEXT representation forbids).
        let cp = eval::to_i64(x);
        let c = u32::try_from(cp)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{FFFD}');
        s.push(c);
    }
    Value::Text(s)
}
