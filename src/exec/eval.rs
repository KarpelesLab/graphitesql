//! Expression evaluation with SQLite value semantics.
//!
//! This is the operational heart of query execution: given a current row, it
//! evaluates an [`Expr`] to a [`Value`], applying SQLite's comparison order,
//! truthiness, numeric coercion, and a core set of built-in scalar functions.
//! Aggregate functions are *not* handled here — they span rows and are computed
//! by the executor (the [`super`] module).
//!
//! The rules implemented mirror SQLite's documented behavior
//! (`lang_expr.html`, `datatype3.html`): `NULL` sorts first; values compare
//! within the class order NULL < numeric < text < blob; `=`/`<>` against `NULL`
//! yield `NULL`; arithmetic coerces operands to numbers. Column *affinity*
//! driven conversion is refined in Phase 9.

use crate::error::{Error, Result};
use crate::sql::ast::{BinaryOp, Expr, Literal, Select, UnaryOp};
use crate::sql::token::Param;
use crate::value::Value;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;

/// Runs subqueries on behalf of expression evaluation. Implemented by the
/// executor; lets `eval` resolve `(SELECT …)` and `IN (SELECT …)` without
/// depending on the executor's concrete types.
pub trait Subqueries {
    /// First column of the first row (NULL if no rows) — a scalar subquery.
    fn scalar(&self, select: &Select) -> Result<Value>;
    /// First column of every row — the candidate set for `IN (SELECT …)`.
    fn column(&self, select: &Select) -> Result<Vec<Value>>;
}

/// Describes a column available to expression evaluation.
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    /// The column's name.
    pub name: String,
    /// The table (or alias) the column belongs to.
    pub table: String,
}

/// Bound parameter values, by position (1-based) and by name.
#[derive(Debug, Default, Clone)]
pub struct Params {
    /// Positional parameters; index 0 is `?1`.
    pub positional: Vec<Value>,
    /// Named parameters, including the sigil (e.g. `:id`).
    pub named: Vec<(String, Value)>,
}

impl Params {
    fn get(&self, p: &Param, anon_index: usize) -> Result<Value> {
        match p {
            Param::Anonymous => self
                .positional
                .get(anon_index)
                .cloned()
                .ok_or_else(|| Error::Error(format_args_unbound(anon_index + 1))),
            Param::Numbered(n) => self
                .positional
                .get((*n as usize).wrapping_sub(1))
                .cloned()
                .ok_or_else(|| Error::Error(format_args_unbound(*n as usize))),
            Param::Named(name) => self
                .named
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| Error::Error(alloc::format!("unbound parameter {name}"))),
        }
    }
}

fn format_args_unbound(n: usize) -> String {
    alloc::format!("unbound parameter ?{n}")
}

/// The row context an expression is evaluated against.
pub struct EvalCtx<'a> {
    /// Column values for the current row (with rowid aliasing already applied).
    pub row: &'a [Value],
    /// Metadata for each column in `row`, by the same index.
    pub columns: &'a [ColumnInfo],
    /// The current row's rowid, if scanning a table.
    pub rowid: Option<i64>,
    /// Bound parameters.
    pub params: &'a Params,
    /// Running counter of anonymous `?` parameters seen so far.
    pub anon_counter: core::cell::Cell<usize>,
    /// Subquery runner, if the executor supplied one.
    pub subqueries: Option<&'a dyn Subqueries>,
}

impl<'a> EvalCtx<'a> {
    /// A context with no row (for `SELECT <expr>` without a `FROM`).
    pub fn rowless(params: &'a Params) -> EvalCtx<'a> {
        EvalCtx {
            row: &[],
            columns: &[],
            rowid: None,
            params,
            anon_counter: core::cell::Cell::new(0),
            subqueries: None,
        }
    }

    /// Attach a subquery runner (builder style).
    pub fn with_subqueries(mut self, s: &'a dyn Subqueries) -> EvalCtx<'a> {
        self.subqueries = Some(s);
        self
    }

    fn resolve_column(&self, table: Option<&str>, name: &str) -> Result<Value> {
        // Special rowid aliases.
        if table.is_none() && is_rowid_alias(name) {
            if let Some(r) = self.rowid {
                return Ok(Value::Integer(r));
            }
        }
        for (i, col) in self.columns.iter().enumerate() {
            let name_ok = col.name.eq_ignore_ascii_case(name);
            let table_ok = table.is_none_or(|t| col.table.eq_ignore_ascii_case(t));
            if name_ok && table_ok {
                return Ok(self.row[i].clone());
            }
        }
        Err(Error::Error(alloc::format!("no such column: {name}")))
    }
}

fn is_rowid_alias(name: &str) -> bool {
    name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid")
}

/// Evaluate `expr` against `ctx`.
pub fn eval(expr: &Expr, ctx: &EvalCtx) -> Result<Value> {
    match expr {
        Expr::Literal(lit) => Ok(literal_value(lit)),
        Expr::Parameter(p) => {
            let idx = ctx.anon_counter.get();
            if matches!(p, Param::Anonymous) {
                ctx.anon_counter.set(idx + 1);
            }
            ctx.params.get(p, idx)
        }
        Expr::Column { table, column } => ctx.resolve_column(table.as_deref(), column),
        Expr::Paren(e) => eval(e, ctx),
        Expr::Unary { op, expr } => eval_unary(*op, eval(expr, ctx)?),
        Expr::Binary { op, left, right } => {
            // Short-circuit AND/OR per SQLite's three-valued logic.
            match op {
                BinaryOp::And => return eval_and(left, right, ctx),
                BinaryOp::Or => return eval_or(left, right, ctx),
                _ => {}
            }
            eval_binary(*op, eval(left, ctx)?, eval(right, ctx)?)
        }
        Expr::IsNull { expr, negated } => {
            let v = eval(expr, ctx)?;
            let is_null = matches!(v, Value::Null);
            Ok(bool_value(is_null != *negated))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => eval_in(expr, list, *negated, ctx),
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval(expr, ctx)?;
            let lo = eval(low, ctx)?;
            let hi = eval(high, ctx)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let ge = matches!(compare(&v, &lo), Ordering::Greater | Ordering::Equal);
            let le = matches!(compare(&v, &hi), Ordering::Less | Ordering::Equal);
            let within = ge && le;
            Ok(bool_value(within != *negated))
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => eval_case(operand.as_deref(), when_then, else_result.as_deref(), ctx),
        Expr::Cast { expr, type_name } => Ok(cast(eval(expr, ctx)?, type_name)),
        Expr::Function {
            name, args, star, ..
        } => super::func::eval_scalar(name, args, *star, ctx),
        Expr::Subquery(select) => match ctx.subqueries {
            Some(s) => s.scalar(select),
            None => Err(Error::Unsupported("subquery in this context")),
        },
        Expr::InSelect {
            expr,
            select,
            negated,
        } => {
            let v = eval(expr, ctx)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let set = match ctx.subqueries {
                Some(s) => s.column(select)?,
                None => return Err(Error::Unsupported("IN (SELECT …) in this context")),
            };
            let mut saw_null = false;
            for iv in &set {
                if matches!(iv, Value::Null) {
                    saw_null = true;
                } else if compare(&v, iv) == Ordering::Equal {
                    return Ok(bool_value(!negated));
                }
            }
            if saw_null {
                Ok(Value::Null)
            } else {
                Ok(bool_value(*negated))
            }
        }
    }
}

fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Integer(i) => Value::Integer(*i),
        Literal::Real(r) => Value::Real(*r),
        Literal::Str(s) => Value::Text(s.clone()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Boolean(b) => Value::Integer(*b as i64),
    }
}

fn eval_unary(op: UnaryOp, v: Value) -> Result<Value> {
    Ok(match op {
        UnaryOp::Identity => v,
        UnaryOp::Negate => match to_number(&v) {
            Value::Integer(i) => Value::Integer(i.wrapping_neg()),
            Value::Real(r) => Value::Real(-r),
            _ => Value::Null,
        },
        UnaryOp::Not => match truth(&v) {
            None => Value::Null,
            Some(b) => bool_value(!b),
        },
        UnaryOp::BitNot => match &v {
            Value::Null => Value::Null,
            _ => Value::Integer(!to_i64(&v)),
        },
    })
}

fn eval_and(left: &Expr, right: &Expr, ctx: &EvalCtx) -> Result<Value> {
    let l = truth(&eval(left, ctx)?);
    if l == Some(false) {
        return Ok(bool_value(false)); // FALSE AND x = FALSE
    }
    let r = truth(&eval(right, ctx)?);
    Ok(match (l, r) {
        (Some(true), Some(true)) => bool_value(true),
        (_, Some(false)) => bool_value(false),
        _ => Value::Null,
    })
}

fn eval_or(left: &Expr, right: &Expr, ctx: &EvalCtx) -> Result<Value> {
    let l = truth(&eval(left, ctx)?);
    if l == Some(true) {
        return Ok(bool_value(true)); // TRUE OR x = TRUE
    }
    let r = truth(&eval(right, ctx)?);
    Ok(match (l, r) {
        (Some(false), Some(false)) => bool_value(false),
        (_, Some(true)) => bool_value(true),
        _ => Value::Null,
    })
}

fn eval_in(expr: &Expr, list: &[Expr], negated: bool, ctx: &EvalCtx) -> Result<Value> {
    let v = eval(expr, ctx)?;
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for item in list {
        let iv = eval(item, ctx)?;
        if matches!(iv, Value::Null) {
            saw_null = true;
            continue;
        }
        if compare(&v, &iv) == Ordering::Equal {
            return Ok(bool_value(!negated));
        }
    }
    // No match: NULL if any comparand was NULL, else definite false/true.
    if saw_null {
        Ok(Value::Null)
    } else {
        Ok(bool_value(negated))
    }
}

fn eval_case(
    operand: Option<&Expr>,
    when_then: &[(Expr, Expr)],
    else_result: Option<&Expr>,
    ctx: &EvalCtx,
) -> Result<Value> {
    let base = match operand {
        Some(e) => Some(eval(e, ctx)?),
        None => None,
    };
    for (when, then) in when_then {
        let matched = match &base {
            // `CASE x WHEN y` matches when x = y.
            Some(b) => {
                let w = eval(when, ctx)?;
                !matches!(b, Value::Null)
                    && !matches!(w, Value::Null)
                    && compare(b, &w) == Ordering::Equal
            }
            // `CASE WHEN cond` matches when cond is true.
            None => truth(&eval(when, ctx)?) == Some(true),
        };
        if matched {
            return eval(then, ctx);
        }
    }
    match else_result {
        Some(e) => eval(e, ctx),
        None => Ok(Value::Null),
    }
}

fn eval_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value> {
    use BinaryOp::*;
    Ok(match op {
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                Value::Null
            } else {
                let ord = compare(&l, &r);
                let res = match op {
                    Eq => ord == Ordering::Equal,
                    NotEq => ord != Ordering::Equal,
                    Lt => ord == Ordering::Less,
                    LtEq => ord != Ordering::Greater,
                    Gt => ord == Ordering::Greater,
                    GtEq => ord != Ordering::Less,
                    _ => unreachable!(),
                };
                bool_value(res)
            }
        }
        Is | IsNot => {
            // IS / IS NOT treat NULL as a comparable value.
            let eq = match (&l, &r) {
                (Value::Null, Value::Null) => true,
                (Value::Null, _) | (_, Value::Null) => false,
                _ => compare(&l, &r) == Ordering::Equal,
            };
            bool_value(eq == matches!(op, Is))
        }
        Add | Sub | Mul | Div | Mod => arithmetic(op, l, r),
        Concat => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                Value::Null
            } else {
                let mut s = to_text(&l);
                s.push_str(&to_text(&r));
                Value::Text(s)
            }
        }
        BitAnd | BitOr | LShift | RShift => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                Value::Null
            } else {
                let a = to_i64(&l);
                let b = to_i64(&r);
                Value::Integer(match op {
                    BitAnd => a & b,
                    BitOr => a | b,
                    LShift => shift_left(a, b),
                    RShift => shift_right(a, b),
                    _ => unreachable!(),
                })
            }
        }
        Like | Glob => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                Value::Null
            } else {
                let text = to_text(&l);
                let pat = to_text(&r);
                let m = if matches!(op, Like) {
                    like_match(&pat, &text)
                } else {
                    glob_match(&pat, &text)
                };
                bool_value(m)
            }
        }
        And | Or => unreachable!("handled with short-circuiting"),
    })
}

fn arithmetic(op: BinaryOp, l: Value, r: Value) -> Value {
    use BinaryOp::*;
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Value::Null;
    }
    let ln = to_number(&l);
    let rn = to_number(&r);
    // Integer arithmetic when both operands are integers (except division which
    // can stay integer too, matching SQLite's `5/2 = 2`).
    if let (Value::Integer(a), Value::Integer(b)) = (&ln, &rn) {
        let (a, b) = (*a, *b);
        return match op {
            Add => a
                .checked_add(b)
                .map(Value::Integer)
                .unwrap_or(Value::Real(a as f64 + b as f64)),
            Sub => a
                .checked_sub(b)
                .map(Value::Integer)
                .unwrap_or(Value::Real(a as f64 - b as f64)),
            Mul => a
                .checked_mul(b)
                .map(Value::Integer)
                .unwrap_or(Value::Real(a as f64 * b as f64)),
            Div => {
                if b == 0 {
                    Value::Null
                } else {
                    Value::Integer(a.wrapping_div(b))
                }
            }
            Mod => {
                if b == 0 {
                    Value::Null
                } else {
                    Value::Integer(a.wrapping_rem(b))
                }
            }
            _ => unreachable!(),
        };
    }
    let a = number_as_f64(&ln);
    let b = number_as_f64(&rn);
    match op {
        Add => Value::Real(a + b),
        Sub => Value::Real(a - b),
        Mul => Value::Real(a * b),
        Div => {
            if b == 0.0 {
                Value::Null
            } else {
                Value::Real(a / b)
            }
        }
        Mod => {
            if b == 0.0 {
                Value::Null
            } else {
                Value::Real(crate::util::float::fmod(a, b))
            }
        }
        _ => unreachable!(),
    }
}

fn shift_left(a: i64, b: i64) -> i64 {
    if b <= -64 || b >= 64 {
        0
    } else if b < 0 {
        ((a as u64) >> (-b) as u32) as i64
    } else {
        ((a as u64) << b as u32) as i64
    }
}

fn shift_right(a: i64, b: i64) -> i64 {
    if b < 0 {
        shift_left(a, -b)
    } else if b >= 64 {
        if a < 0 {
            -1
        } else {
            0
        }
    } else {
        a >> b as u32
    }
}

/// Cast `v` to the affinity implied by a SQL type name.
pub fn cast(v: Value, type_name: &str) -> Value {
    let aff = type_name.to_ascii_uppercase();
    if aff.contains("INT") {
        Value::Integer(to_i64(&v))
    } else if aff.contains("CHAR") || aff.contains("CLOB") || aff.contains("TEXT") {
        match v {
            Value::Null => Value::Null,
            other => Value::Text(to_text(&other)),
        }
    } else if aff.contains("REAL") || aff.contains("FLOA") || aff.contains("DOUB") {
        match v {
            Value::Null => Value::Null,
            other => Value::Real(number_as_f64(&to_number(&other))),
        }
    } else if aff.contains("BLOB") || aff.is_empty() {
        v
    } else {
        // NUMERIC affinity: prefer integer if exact, else real.
        to_number(&v)
    }
}

// ---- value semantics --------------------------------------------------------

/// SQLite's total comparison order across storage classes (see
/// [`crate::value::cmp_values`]).
pub fn compare(a: &Value, b: &Value) -> Ordering {
    crate::value::cmp_values(a, b)
}

/// Three-valued truthiness: `None` is SQL `NULL`.
pub fn truth(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        Value::Integer(i) => Some(*i != 0),
        Value::Real(r) => Some(*r != 0.0),
        Value::Text(_) | Value::Blob(_) => Some(number_as_f64(&to_number(v)) != 0.0),
    }
}

fn bool_value(b: bool) -> Value {
    Value::Integer(b as i64)
}

/// Coerce a value to a number (Integer or Real), or `Value::Null`.
pub fn to_number(v: &Value) -> Value {
    match v {
        Value::Integer(_) | Value::Real(_) => v.clone(),
        Value::Null => Value::Null,
        Value::Text(s) => parse_number(s),
        Value::Blob(_) => Value::Integer(0),
    }
}

fn parse_number(s: &str) -> Value {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Value::Integer(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::Real(f);
    }
    // SQLite uses the longest valid numeric prefix; approximate that.
    let mut end = 0;
    let bytes = t.as_bytes();
    let mut seen_dot = false;
    let mut seen_digit = false;
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_digit() {
            seen_digit = true;
        } else if c == b'.' && !seen_dot {
            seen_dot = true;
        } else if (c == b'-' || c == b'+') && end == 0 {
            // leading sign ok
        } else {
            break;
        }
        end += 1;
    }
    if !seen_digit {
        return Value::Integer(0);
    }
    let prefix = &t[..end];
    if let Ok(i) = prefix.parse::<i64>() {
        Value::Integer(i)
    } else if let Ok(f) = prefix.parse::<f64>() {
        Value::Real(f)
    } else {
        Value::Integer(0)
    }
}

fn number_as_f64(v: &Value) -> f64 {
    match v {
        Value::Integer(i) => *i as f64,
        Value::Real(r) => *r,
        _ => 0.0,
    }
}

/// Coerce a value to i64.
pub fn to_i64(v: &Value) -> i64 {
    match to_number(v) {
        Value::Integer(i) => i,
        Value::Real(r) => r as i64,
        _ => 0,
    }
}

/// Coerce a value to f64.
pub fn to_f64(v: &Value) -> f64 {
    number_as_f64(&to_number(v))
}

/// Render a value as text (for `||`, CAST to TEXT, etc.).
pub fn to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => format_real(*r),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Format a real the way SQLite renders doubles in text contexts.
pub fn format_real(r: f64) -> String {
    use crate::util::float;
    if r == float::trunc(r) && r.is_finite() && float::abs(r) < 1e15 {
        // Whole-valued doubles print as `N.0`.
        alloc::format!("{:.1}", r)
    } else {
        let mut s = alloc::format!("{r}");
        if !s.contains('.') && !s.contains('e') && !s.contains("inf") && !s.contains("nan") {
            s.push_str(".0");
        }
        s
    }
}

// ---- LIKE / GLOB ------------------------------------------------------------

/// SQLite `LIKE`: `%` matches any run, `_` any single char; case-insensitive for
/// ASCII (SQLite's default).
fn like_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    like_rec(&p, &t)
}

fn like_rec(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    match p[0] {
        '%' => {
            // Collapse consecutive %.
            let rest = &p[1..];
            if like_rec(rest, t) {
                return true;
            }
            for i in 0..t.len() {
                if like_rec(rest, &t[i + 1..]) {
                    return true;
                }
            }
            false
        }
        '_' => !t.is_empty() && like_rec(&p[1..], &t[1..]),
        pc => !t.is_empty() && pc.eq_ignore_ascii_case(&t[0]) && like_rec(&p[1..], &t[1..]),
    }
}

/// SQLite `GLOB`: `*`/`?`/`[set]`, case-sensitive.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_rec(&p, &t)
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    match p[0] {
        '*' => {
            let rest = &p[1..];
            if glob_rec(rest, t) {
                return true;
            }
            for i in 0..t.len() {
                if glob_rec(rest, &t[i + 1..]) {
                    return true;
                }
            }
            false
        }
        '?' => !t.is_empty() && glob_rec(&p[1..], &t[1..]),
        '[' => {
            if t.is_empty() {
                return false;
            }
            // Parse a character class up to ']'.
            let mut i = 1;
            let mut negate = false;
            if i < p.len() && (p[i] == '^') {
                negate = true;
                i += 1;
            }
            let mut matched = false;
            while i < p.len() && p[i] != ']' {
                if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                    if t[0] >= p[i] && t[0] <= p[i + 2] {
                        matched = true;
                    }
                    i += 3;
                } else {
                    if t[0] == p[i] {
                        matched = true;
                    }
                    i += 1;
                }
            }
            if i >= p.len() {
                return false; // unterminated class
            }
            (matched != negate) && glob_rec(&p[i + 1..], &t[1..])
        }
        pc => !t.is_empty() && pc == t[0] && glob_rec(&p[1..], &t[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_class_order() {
        assert_eq!(compare(&Value::Null, &Value::Integer(0)), Ordering::Less);
        assert_eq!(
            compare(&Value::Integer(5), &Value::Text("a".into())),
            Ordering::Less
        );
        assert_eq!(
            compare(&Value::Integer(2), &Value::Real(2.0)),
            Ordering::Equal
        );
        assert_eq!(
            compare(&Value::Text("abc".into()), &Value::Text("abd".into())),
            Ordering::Less
        );
    }

    #[test]
    fn like_and_glob() {
        assert!(like_match("a%", "apple"));
        assert!(like_match("%ple", "apple"));
        assert!(like_match("a_ple", "apple"));
        assert!(like_match("APPLE", "apple")); // case-insensitive
        assert!(!like_match("a%", "banana"));
        assert!(glob_match("a*", "apple"));
        assert!(glob_match("a[pq]ple", "apple"));
        assert!(!glob_match("A*", "apple")); // case-sensitive
    }

    #[test]
    fn real_formatting() {
        assert_eq!(format_real(3.0), "3.0");
        assert_eq!(format_real(2.5), "2.5");
    }
}
