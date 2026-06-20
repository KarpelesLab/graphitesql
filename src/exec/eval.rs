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
    /// First column of the first row (NULL if no rows) — a scalar subquery. The
    /// enclosing row context `outer` is made available so correlated subqueries
    /// can resolve outer columns.
    fn scalar(&self, select: &Select, outer: &EvalCtx) -> Result<Value>;
    /// First column of every row — the candidate set for `IN (SELECT …)`.
    fn column(&self, select: &Select, outer: &EvalCtx) -> Result<Vec<Value>>;
    /// Every row in full — the candidate set for a row-value `(a,b) IN (SELECT …)`.
    fn rows(&self, select: &Select, outer: &EvalCtx) -> Result<Vec<Vec<Value>>>;
    /// Whether the subquery returns at least one row — for `EXISTS`.
    fn exists(&self, select: &Select, outer: &EvalCtx) -> Result<bool>;
    /// Resolve a column against the enclosing (outer) query rows, if any are in
    /// scope. Returns `None` when there is no such outer column.
    fn resolve_outer(&self, table: Option<&str>, name: &str) -> Option<Value>;
}

/// A column's type affinity (SQLite, `datatype3.html` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// No conversion (BLOB / untyped).
    Blob,
    /// Prefer text.
    Text,
    /// Prefer a number, falling back to the original value.
    Numeric,
    /// Like Numeric, also turning integral reals into integers.
    Integer,
    /// Prefer a real.
    Real,
}

impl Affinity {
    /// Determine a column's affinity from its declared type name (the rules in
    /// `datatype3.html`: the first matching substring wins).
    pub fn from_type(type_name: Option<&str>) -> Affinity {
        let Some(t) = type_name else {
            return Affinity::Blob; // no datatype => BLOB (NONE) affinity
        };
        let t = t.to_ascii_uppercase();
        if t.contains("INT") {
            Affinity::Integer
        } else if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
            Affinity::Text
        } else if t.contains("BLOB") {
            Affinity::Blob
        } else if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
            Affinity::Real
        } else {
            Affinity::Numeric
        }
    }

    /// Apply this affinity to a value for *storage* (SQLite coerces values to the
    /// column's affinity on insert/update).
    pub fn coerce(self, v: Value) -> Value {
        match self {
            Affinity::Blob => v,
            Affinity::Text => match v {
                Value::Integer(_) | Value::Real(_) => Value::Text(to_text(&v)),
                other => other,
            },
            Affinity::Real => match v {
                Value::Null | Value::Blob(_) => v,
                _ => match to_number_strict(&v) {
                    Some(n) => Value::Real(number_as_f64(&n)),
                    None => v,
                },
            },
            Affinity::Integer | Affinity::Numeric => match v {
                Value::Null | Value::Blob(_) => v,
                Value::Real(r) => {
                    // Reduce an integral real to an integer.
                    if r == crate::util::float::trunc(r)
                        && r.is_finite()
                        && self == Affinity::Integer
                    {
                        Value::Integer(r as i64)
                    } else {
                        Value::Real(r)
                    }
                }
                Value::Integer(_) => v,
                Value::Text(_) => match to_number_strict(&v) {
                    Some(Value::Real(r))
                        if r == crate::util::float::trunc(r)
                            && r.is_finite()
                            && self == Affinity::Integer =>
                    {
                        Value::Integer(r as i64)
                    }
                    Some(n) => n,
                    None => v,
                },
            },
        }
    }
}

/// Parse a value as a number only if it is *entirely* a valid numeric literal
/// (no trailing junk), else `None`. Used for affinity coercion, where SQLite
/// only converts text that fully looks like a number.
fn to_number_strict(v: &Value) -> Option<Value> {
    match v {
        Value::Integer(_) | Value::Real(_) => Some(v.clone()),
        Value::Text(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                Some(Value::Integer(i))
            } else if let Ok(f) = t.parse::<f64>() {
                Some(Value::Real(f))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Describes a column available to expression evaluation.
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    /// The column's name.
    pub name: String,
    /// The table (or alias) the column belongs to.
    pub table: String,
    /// The column's type affinity (Blob/NONE when unknown).
    pub affinity: Affinity,
    /// The column's declared collating sequence (`BINARY` by default).
    pub collation: crate::value::Collation,
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

    /// The declared collation of a column reference, if it resolves to one of
    /// the current row's columns.
    fn column_collation(&self, table: Option<&str>, name: &str) -> Option<Collation> {
        self.columns.iter().find_map(|col| {
            let name_ok = col.name.eq_ignore_ascii_case(name);
            let table_ok = table.is_none_or(|t| col.table.eq_ignore_ascii_case(t));
            (name_ok && table_ok).then_some(col.collation)
        })
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
        // Fall back to an enclosing query's row (a correlated reference).
        if let Some(s) = self.subqueries {
            if let Some(v) = s.resolve_outer(table, name) {
                return Ok(v);
            }
        }
        Err(Error::Error(alloc::format!("no such column: {name}")))
    }
}

/// The affinity of an expression for comparison purposes: a column's declared
/// affinity, a CAST's target affinity, transparent through parentheses, else
/// none (BLOB).
fn expr_affinity(expr: &Expr, ctx: &EvalCtx) -> Affinity {
    match expr {
        Expr::Column { table, column } => {
            for col in ctx.columns {
                let name_ok = col.name.eq_ignore_ascii_case(column);
                let table_ok = table
                    .as_deref()
                    .is_none_or(|t| col.table.eq_ignore_ascii_case(t));
                if name_ok && table_ok {
                    return col.affinity;
                }
            }
            Affinity::Blob
        }
        Expr::Cast { type_name, .. } => Affinity::from_type(Some(type_name)),
        Expr::Paren(e) => expr_affinity(e, ctx),
        _ => Affinity::Blob,
    }
}

/// Apply SQLite comparison affinity to a pair of operands before comparing.
fn apply_comparison_affinity(l: Value, la: Affinity, r: Value, ra: Affinity) -> (Value, Value) {
    let numeric = |a: Affinity| matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric);
    let texty = |a: Affinity| matches!(a, Affinity::Text | Affinity::Blob);
    if numeric(la) && texty(ra) {
        (l, Affinity::Numeric.coerce(r))
    } else if numeric(ra) && texty(la) {
        (Affinity::Numeric.coerce(l), r)
    } else if la == Affinity::Text && ra == Affinity::Blob {
        (l, Affinity::Text.coerce(r))
    } else if ra == Affinity::Text && la == Affinity::Blob {
        (Affinity::Text.coerce(l), r)
    } else {
        (l, r)
    }
}

pub(crate) fn is_rowid_alias(name: &str) -> bool {
    name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid")
}

use crate::value::Collation;

/// Apply comparison operator `op` to `l`/`r` under collation `coll` (NULL if
/// either operand is NULL).
pub fn compare_op(op: BinaryOp, l: &Value, r: &Value, coll: Collation) -> Value {
    use BinaryOp::*;
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Value::Null;
    }
    let ord = crate::value::cmp_values_coll(l, r, coll);
    let res = match op {
        Eq => ord == Ordering::Equal,
        NotEq => ord != Ordering::Equal,
        Lt => ord == Ordering::Less,
        LtEq => ord != Ordering::Greater,
        Gt => ord == Ordering::Greater,
        GtEq => ord != Ordering::Less,
        _ => unreachable!("compare_op on non-comparison operator"),
    };
    bool_value(res)
}

/// Resolve the collating sequence of a binary comparison: an explicit `COLLATE`
/// on the left, else on the right, else the left column's declared collation,
/// else the right column's, else `BINARY`. (Mirrors SQLite's rules.)
fn resolve_collation(left: &Expr, right: &Expr, ctx: &EvalCtx) -> Collation {
    explicit_collation(left)
        .or_else(|| explicit_collation(right))
        .or_else(|| column_collation_of(left, ctx))
        .or_else(|| column_collation_of(right, ctx))
        .unwrap_or_default()
}

fn explicit_collation(e: &Expr) -> Option<Collation> {
    match e {
        Expr::Collate { collation, .. } => Collation::parse(collation),
        Expr::Paren(inner) => explicit_collation(inner),
        _ => None,
    }
}

fn column_collation_of(e: &Expr, ctx: &EvalCtx) -> Option<Collation> {
    match e {
        Expr::Column { table, column } => ctx.column_collation(table.as_deref(), column),
        Expr::Paren(inner) | Expr::Collate { expr: inner, .. } => column_collation_of(inner, ctx),
        _ => None,
    }
}

/// The collation of an `ORDER BY`/`GROUP BY` key expression: an explicit
/// `COLLATE`, else the underlying column's collation, else `BINARY`.
pub fn key_collation(e: &Expr, ctx: &EvalCtx) -> Collation {
    explicit_collation(e)
        .or_else(|| column_collation_of(e, ctx))
        .unwrap_or_default()
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
            use BinaryOp::*;
            match op {
                // Short-circuit AND/OR per SQLite's three-valued logic.
                And => eval_and(left, right, ctx),
                Or => eval_or(left, right, ctx),
                // Comparisons apply operand affinity, then the resolved collation.
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    // Row-value comparison `(a,b) OP (c,d)` is lexicographic.
                    if let (Some(ls), Some(rs)) = (as_row_value(left), as_row_value(right)) {
                        return compare_row_values(*op, ls, rs, ctx);
                    }
                    let l = eval(left, ctx)?;
                    let r = eval(right, ctx)?;
                    let (l, r) = apply_comparison_affinity(
                        l,
                        expr_affinity(left, ctx),
                        r,
                        expr_affinity(right, ctx),
                    );
                    let coll = resolve_collation(left, right, ctx);
                    Ok(compare_op(*op, &l, &r, coll))
                }
                _ => eval_binary(*op, eval(left, ctx)?, eval(right, ctx)?),
            }
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
        } => {
            if let Some(ls) = as_row_value(expr) {
                return eval_row_in(ls, list, *negated, ctx);
            }
            eval_in(expr, list, *negated, ctx)
        }
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
        // COLLATE only affects comparisons; the value itself is the operand's.
        Expr::Collate { expr, .. } => eval(expr, ctx),
        Expr::Function {
            name, args, star, ..
        } => super::func::eval_scalar(name, args, *star, ctx),
        Expr::Subquery(select) => match ctx.subqueries {
            Some(s) => s.scalar(select, ctx),
            None => Err(Error::Unsupported("subquery in this context")),
        },
        Expr::Exists { select, negated } => match ctx.subqueries {
            Some(s) => Ok(bool_value(s.exists(select, ctx)? != *negated)),
            None => Err(Error::Unsupported("EXISTS in this context")),
        },
        Expr::InSelect {
            expr,
            select,
            negated,
        } => {
            if let Some(ls) = as_row_value(expr) {
                return eval_row_in_select(ls, select, *negated, ctx);
            }
            let v = eval(expr, ctx)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let set = match ctx.subqueries {
                Some(s) => s.column(select, ctx)?,
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
        Expr::RowValue(_) => Err(Error::Error(
            "row value used where a single value is expected".into(),
        )),
    }
}

/// View an expression as a row value's element list, if it is one (transparently
/// through a redundant parenthesization).
fn as_row_value(e: &Expr) -> Option<&[Expr]> {
    match e {
        Expr::RowValue(items) => Some(items),
        Expr::Paren(inner) => as_row_value(inner),
        _ => None,
    }
}

/// Per-element comparison of two row values: `Some(Ordering)` when both sides are
/// non-NULL, `None` when either is NULL (an undecidable element).
fn row_element_cmps(
    lefts: &[Expr],
    rights: &[Expr],
    ctx: &EvalCtx,
) -> Result<Vec<Option<Ordering>>> {
    if lefts.len() != rights.len() {
        return Err(Error::Error(alloc::format!(
            "row values have a different number of columns ({} vs {})",
            lefts.len(),
            rights.len()
        )));
    }
    let mut out = Vec::with_capacity(lefts.len());
    for (le, re) in lefts.iter().zip(rights) {
        let l = eval(le, ctx)?;
        let r = eval(re, ctx)?;
        if matches!(l, Value::Null) || matches!(r, Value::Null) {
            out.push(None);
            continue;
        }
        let (l, r) =
            apply_comparison_affinity(l, expr_affinity(le, ctx), r, expr_affinity(re, ctx));
        let coll = resolve_collation(le, re, ctx);
        out.push(Some(crate::value::cmp_values_coll(&l, &r, coll)));
    }
    Ok(out)
}

/// Lexicographic comparison of two row values under SQLite's three-valued logic.
fn compare_row_values(
    op: BinaryOp,
    lefts: &[Expr],
    rights: &[Expr],
    ctx: &EvalCtx,
) -> Result<Value> {
    let cmps = row_element_cmps(lefts, rights, ctx)?;
    Ok(fold_row_comparison(op, &cmps))
}

/// Combine per-element comparisons into the result of a row comparison `op`.
fn fold_row_comparison(op: BinaryOp, cmps: &[Option<Ordering>]) -> Value {
    use BinaryOp::*;
    match op {
        Eq | NotEq => {
            let mut unknown = false;
            for c in cmps {
                match c {
                    None => unknown = true,
                    Some(Ordering::Equal) => {}
                    Some(_) => return bool_value(matches!(op, NotEq)), // a definite difference
                }
            }
            if unknown {
                Value::Null
            } else {
                bool_value(matches!(op, Eq)) // all elements equal
            }
        }
        Lt | LtEq | Gt | GtEq => {
            for c in cmps {
                match c {
                    None => return Value::Null, // undecidable at this position
                    Some(Ordering::Equal) => continue,
                    Some(Ordering::Less) => return bool_value(matches!(op, Lt | LtEq)),
                    Some(Ordering::Greater) => return bool_value(matches!(op, Gt | GtEq)),
                }
            }
            // All elements equal: `<=`/`>=` hold, strict `<`/`>` do not.
            bool_value(matches!(op, LtEq | GtEq))
        }
        _ => Value::Null,
    }
}

/// `(a, b, …) [NOT] IN (SELECT …)` — row value membership against a subquery.
fn eval_row_in_select(
    lefts: &[Expr],
    select: &Select,
    negated: bool,
    ctx: &EvalCtx,
) -> Result<Value> {
    let lvals: Vec<Value> = lefts.iter().map(|e| eval(e, ctx)).collect::<Result<_>>()?;
    let rows = match ctx.subqueries {
        Some(s) => s.rows(select, ctx)?,
        None => return Err(Error::Unsupported("IN (SELECT …) in this context")),
    };
    let mut saw_null = false;
    for row in &rows {
        if row.len() != lvals.len() {
            return Err(Error::Error(alloc::format!(
                "sub-select returns {} columns - expected {}",
                row.len(),
                lvals.len()
            )));
        }
        let cmps: Vec<Option<Ordering>> = lvals
            .iter()
            .zip(row)
            .map(|(l, r)| {
                if matches!(l, Value::Null) || matches!(r, Value::Null) {
                    None
                } else {
                    Some(compare(l, r))
                }
            })
            .collect();
        match fold_row_comparison(BinaryOp::Eq, &cmps) {
            Value::Integer(1) => return Ok(bool_value(!negated)),
            Value::Null => saw_null = true,
            _ => {}
        }
    }
    if saw_null {
        Ok(Value::Null)
    } else {
        Ok(bool_value(negated))
    }
}

/// `(a, b, …) [NOT] IN ((…), (…))` — row value membership.
fn eval_row_in(lefts: &[Expr], list: &[Expr], negated: bool, ctx: &EvalCtx) -> Result<Value> {
    let mut saw_null = false;
    for item in list {
        let Some(rights) = as_row_value(item) else {
            return Err(Error::Error(
                "row value IN list must contain row values".into(),
            ));
        };
        match fold_row_comparison(BinaryOp::Eq, &row_element_cmps(lefts, rights, ctx)?) {
            Value::Integer(1) => return Ok(bool_value(!negated)),
            Value::Null => saw_null = true,
            _ => {}
        }
    }
    if saw_null {
        Ok(Value::Null)
    } else {
        Ok(bool_value(negated))
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
        JsonExtract => crate::exec::json::arrow(&l, &r, false),
        JsonExtractText => crate::exec::json::arrow(&l, &r, true),
        And | Or => unreachable!("handled with short-circuiting"),
    })
}

/// Apply an arithmetic `BinaryOp` to two values (SQLite numeric semantics).
/// Public wrapper used by the VDBE interpreter.
pub fn arithmetic_values(op: BinaryOp, l: &Value, r: &Value) -> Value {
    arithmetic(op, l.clone(), r.clone())
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
    // CAST(NULL AS anything) is NULL.
    if matches!(v, Value::Null) {
        return Value::Null;
    }
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

/// Format a real the way SQLite renders doubles in text contexts: the C
/// `%!.15g` style — 15 significant digits, scientific notation when the decimal
/// exponent is `< -4` or `>= 15`, trailing zeros trimmed, always with a decimal
/// point (`N.0`) and a two-digit signed exponent (`1.0e+20`).
pub fn format_real(r: f64) -> String {
    if r.is_nan() {
        return String::new();
    }
    if !r.is_finite() {
        return if r < 0.0 {
            String::from("-Inf")
        } else {
            String::from("Inf")
        };
    }
    if r == 0.0 {
        return String::from("0.0");
    }
    let neg = r < 0.0;
    let a = crate::util::float::abs(r);

    // 15 significant digits in scientific form: "D.DDDDDDDDDDDDDDe<exp>".
    let sci = alloc::format!("{:.14e}", a);
    let (mant, exp_str) = sci.split_once('e').expect("scientific format has 'e'");
    let exp: i32 = exp_str.parse().expect("valid exponent");
    let digits: String = mant.chars().filter(|c| *c != '.').collect(); // 15 digits

    let body = if !(-4..15).contains(&exp) {
        // Scientific.
        let frac = digits[1..].trim_end_matches('0');
        let mantissa = if frac.is_empty() {
            alloc::format!("{}.0", &digits[0..1])
        } else {
            alloc::format!("{}.{}", &digits[0..1], frac)
        };
        let sign = if exp < 0 { '-' } else { '+' };
        alloc::format!("{mantissa}e{sign}{:02}", exp.abs())
    } else if exp >= 0 {
        // Fixed, value >= 1: (exp+1) integer digits.
        let int_len = (exp + 1) as usize;
        let int_part = &digits[..int_len];
        let frac = digits[int_len..].trim_end_matches('0');
        if frac.is_empty() {
            alloc::format!("{int_part}.0")
        } else {
            alloc::format!("{int_part}.{frac}")
        }
    } else {
        // Fixed, value < 1: leading zeros then the digits.
        let lead = (-(exp + 1)) as usize;
        let mut frac = String::new();
        for _ in 0..lead {
            frac.push('0');
        }
        frac.push_str(&digits);
        let frac = frac.trim_end_matches('0');
        alloc::format!("0.{frac}")
    };

    if neg {
        alloc::format!("-{body}")
    } else {
        body
    }
}

// ---- LIKE / GLOB ------------------------------------------------------------

/// SQLite `LIKE`: `%` matches any run, `_` any single char; case-insensitive for
/// ASCII (SQLite's default).
fn like_match(pattern: &str, text: &str) -> bool {
    like_match_escape(pattern, text, None)
}

/// `LIKE` with an optional `ESCAPE` character: a wildcard (`%`/`_`) or the escape
/// character itself, when preceded by the escape character, matches literally.
pub fn like_match_escape(pattern: &str, text: &str, escape: Option<char>) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    like_rec(&p, &t, escape)
}

fn like_rec(p: &[char], t: &[char], esc: Option<char>) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    // An escaped character matches literally.
    if let Some(e) = esc {
        if p[0] == e && p.len() >= 2 {
            let lit = p[1];
            return !t.is_empty()
                && lit.eq_ignore_ascii_case(&t[0])
                && like_rec(&p[2..], &t[1..], esc);
        }
    }
    match p[0] {
        '%' => {
            // Collapse consecutive %.
            let rest = &p[1..];
            if like_rec(rest, t, esc) {
                return true;
            }
            for i in 0..t.len() {
                if like_rec(rest, &t[i + 1..], esc) {
                    return true;
                }
            }
            false
        }
        '_' => !t.is_empty() && like_rec(&p[1..], &t[1..], esc),
        pc => !t.is_empty() && pc.eq_ignore_ascii_case(&t[0]) && like_rec(&p[1..], &t[1..], esc),
    }
}

/// SQLite `GLOB`: `*`/`?`/`[set]`, case-sensitive.
/// SQLite `GLOB` matching: case-sensitive `*`/`?`/`[…]` wildcards.
pub fn glob_match(pattern: &str, text: &str) -> bool {
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
        // %.15g compatibility with sqlite.
        assert_eq!(format_real(35.0 / 3.0), "11.6666666666667");
        assert_eq!(format_real(1.0 / 3.0), "0.333333333333333");
        assert_eq!(format_real(0.1), "0.1");
        assert_eq!(format_real(0.1 + 0.2), "0.3");
        assert_eq!(format_real(1e20), "1.0e+20");
        assert_eq!(format_real(1e-10), "1.0e-10");
        assert_eq!(format_real(1e15), "1.0e+15");
        assert_eq!(format_real(1e14), "100000000000000.0");
        assert_eq!(format_real(0.0001), "0.0001");
        assert_eq!(format_real(0.00001), "1.0e-05");
        assert_eq!(format_real(-2.5), "-2.5");
        assert_eq!(format_real(0.0), "0.0");
    }
}
