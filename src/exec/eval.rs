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
    /// The declared affinity of the subquery's first output column, for applying
    /// `IN (SELECT …)` comparison affinity. `None` when it has no affinity (a
    /// computed expression) or cannot be determined.
    fn column_affinity(&self, select: &Select) -> Option<Affinity>;
    /// The declared affinity of *each* of the subquery's output columns, for a
    /// row-value `(a, b, …) IN (SELECT …)`. Same per-column rule as
    /// [`Self::column_affinity`]; the vector may be empty if it cannot be determined.
    fn row_column_affinities(&self, select: &Select) -> alloc::vec::Vec<Option<Affinity>>;
    /// Every row in full — the candidate set for a row-value `(a,b) IN (SELECT …)`.
    fn rows(&self, select: &Select, outer: &EvalCtx) -> Result<Vec<Vec<Value>>>;
    /// Whether the subquery returns at least one row — for `EXISTS`.
    fn exists(&self, select: &Select, outer: &EvalCtx) -> Result<bool>;
    /// Resolve a column against the enclosing (outer) query rows, if any are in
    /// scope. Returns `None` when there is no such outer column.
    fn resolve_outer(&self, table: Option<&str>, name: &str) -> Option<Value>;
    /// The declared affinity of an outer (correlated) column, mirroring
    /// [`Self::resolve_outer`]. A correlated reference carries its source column's
    /// affinity, so a comparison against it in the subquery applies the correct
    /// (two-column) comparison affinity — e.g. `text_col = outer_untyped_col` uses
    /// BLOB (no coercion), not the TEXT-coerce-the-literal rule. `None` when there
    /// is no such outer column.
    fn resolve_outer_affinity(&self, table: Option<&str>, name: &str) -> Option<Affinity> {
        let _ = (table, name);
        None
    }
    /// `last_insert_rowid()` — rowid of the most recently inserted row.
    fn last_insert_rowid(&self) -> i64 {
        0
    }
    /// `changes()` — rows modified by the most recent INSERT/UPDATE/DELETE.
    fn changes(&self) -> i64 {
        0
    }
    /// Whether `PRAGMA case_sensitive_like` is ON for this connection, making the
    /// `LIKE` operator and the `like()` function compare ASCII case-sensitively
    /// (`GLOB` is always case-sensitive regardless). Default `false` — SQLite's
    /// case-insensitive default — for the rowless/connection-less contexts.
    fn case_sensitive_like(&self) -> bool {
        false
    }
    /// `total_changes()` — rows modified since the connection opened.
    fn total_changes(&self) -> i64 {
        0
    }
    /// One pseudo-random `i64`, advancing the connection's generator — backs
    /// `random()` and `randomblob()`. The default (no connection in scope, e.g.
    /// rowless constant evaluation) is a fixed 0.
    fn next_random(&self) -> i64 {
        0
    }
    /// Invoke a user-defined scalar function registered on the connection (via
    /// `Connection::register_function`) with its evaluated argument values.
    /// Returns `None` when no function is registered under `name` (lowercased), so
    /// the caller can fall back to "no such function". The default is `None`.
    fn call_udf(&self, _name: &str, _args: &[Value]) -> Option<Result<Value>> {
        None
    }
    /// The FTS5 relevance score (`bm25()` / `rank`) of the row with this `rowid`,
    /// with optional per-column `weights` (empty → all 1.0), when the current query
    /// is a full-text `MATCH` over an `fts5` table. `None` otherwise (so
    /// `bm25()`/`rank` fall back to the usual unknown-name error).
    fn fts5_bm25(&self, _rowid: i64, _weights: &[f64]) -> Option<f64> {
        None
    }
    /// The FTS5 bare `rank` column's value for the row with this `rowid`: the
    /// table's configured default ranking function (set via `INSERT INTO t(t,
    /// rank) VALUES('rank', …)`), or the default `bm25()` when unconfigured.
    /// `None` when no `MATCH` over an `fts5` table is in scope (so `rank` falls
    /// back to the usual unknown-column error); `Some(Err)` when the configured
    /// rank function is invalid (e.g. `nosuchfunc()`), matching SQLite's deferred
    /// `no such function` / arity error at query time.
    fn fts5_rank(&self, _rowid: i64) -> Option<Result<f64>> {
        None
    }
    /// FTS5 `highlight(t, col, open, close)`: column `col`'s `text` with each
    /// matched token wrapped in `open`…`close`, when a `MATCH` over an `fts5` table
    /// is in scope. `None` otherwise.
    fn fts5_highlight(
        &self,
        _col: usize,
        _text: &str,
        _open: &str,
        _close: &str,
    ) -> Option<String> {
        None
    }
    /// FTS5 `snippet(t, col, open, close, ellipsis, n)`: an up-to-`n`-token window
    /// of `text` covering the query's phrases, matched tokens wrapped and trimmed
    /// ends marked with `ellipsis`, when a `MATCH` over an `fts5` table is in scope.
    /// `None` otherwise.
    #[allow(clippy::too_many_arguments)]
    fn fts5_snippet(
        &self,
        _col: i64,
        _cols: &[String],
        _open: &str,
        _close: &str,
        _ellipsis: &str,
        _ntokens: usize,
    ) -> Option<String> {
        None
    }
    /// The searchable (indexed) column names of the `fts5` table `table`, i.e.
    /// every declared column except those marked `UNINDEXED`. `None` when `table`
    /// is not a known `fts5` virtual table (so callers fall back to all columns).
    fn fts5_indexed_columns(&self, _table: &str) -> Option<Vec<String>> {
        None
    }
    /// The `fts5` table `table`'s resolved tokenizer config (Porter stemming +
    /// `remove_diacritics` level), so a `MATCH` query folds exactly like the
    /// indexed documents. The default (`remove_diacritics 1`, no stemming) is
    /// returned when `table` is not a known `fts5` virtual table.
    #[cfg(feature = "fts5")]
    fn fts5_tok(&self, _table: &str) -> crate::vtab::Fts5Tok {
        crate::vtab::Fts5Tok::default()
    }
    /// Whether the document with `rowid` matches the full-text `query` in the
    /// **contentless** `fts5` table `table`, evaluated against the inverted index
    /// (a contentless row keeps no column text, so `MATCH` can't be re-checked from
    /// the row like it is for a self/external-content table). Returns `None` when
    /// `table` is not a contentless fts5 table (so the caller uses the column-text
    /// path); `Some(true)`/`Some(false)` when it is.
    #[cfg(feature = "fts5")]
    fn fts5_contentless_match(&self, _table: &str, _query: &str, _rowid: i64) -> Option<bool> {
        None
    }
    /// Whether `table` is a *contentless* (`content=''`) `fts5` table. Used by
    /// `highlight`/`snippet` to return NULL (a contentless table stores no text to
    /// render). `false` for a non-fts5 or non-contentless table.
    #[cfg(feature = "fts5")]
    fn fts5_is_contentless_table(&self, _table: &str) -> bool {
        false
    }
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
                Value::Integer(_) | Value::Real(_) => Value::Text(to_text(&v).into()),
                other => other,
            },
            Affinity::Real => match v {
                Value::Null | Value::Blob(_) => v,
                _ => match to_number_strict(&v) {
                    Some(n) => Value::Real(number_as_f64(&n)),
                    None => v,
                },
            },
            // INTEGER and NUMERIC affinity behave identically for storage: a
            // value (or fully-numeric text) is converted to INTEGER when it is an
            // integer that fits in i64, else REAL. So `'10.0'`, `'2e2'`, and the
            // real `10.0` all store as integers, while `10.5` and an out-of-range
            // integral real stay REAL — matching SQLite.
            Affinity::Integer | Affinity::Numeric => match v {
                Value::Null | Value::Blob(_) => v,
                Value::Real(r) => integral_real_to_int(r).unwrap_or(Value::Real(r)),
                Value::Integer(_) => v,
                Value::Text(_) => match to_number_strict(&v) {
                    Some(Value::Real(r)) => integral_real_to_int(r).unwrap_or(Value::Real(r)),
                    Some(n) => n,
                    None => v,
                },
            },
        }
    }
}

/// `Some(Integer)` when `r` is a finite integral real that fits exactly in an
/// `i64` (`[-2^63, 2^63)`), else `None`. This is the NUMERIC/INTEGER storage rule:
/// integral reals reduce to integers, but an out-of-range one stays REAL.
fn integral_real_to_int(r: f64) -> Option<Value> {
    if r.is_finite()
        && r == crate::util::float::trunc(r)
        && r >= i64::MIN as f64
        && r < 9_223_372_036_854_775_808.0
    {
        Some(Value::Integer(r as i64))
    } else {
        None
    }
}

/// Parse a value as a number only if it is *entirely* a valid numeric literal
/// (no trailing junk), else `None`. Used for affinity coercion, where SQLite
/// only converts text that fully looks like a number.
pub(crate) fn to_number_strict(v: &Value) -> Option<Value> {
    match v {
        Value::Integer(_) | Value::Real(_) => Some(v.clone()),
        Value::Text(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                Some(Value::Integer(i))
            } else {
                parse_decimal_f64(t).map(Value::Real)
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
    /// The database the column's source table lives in (`main`/`temp`/an attached
    /// name), when it is a real base table. `None` for synthetic/derived/subquery/
    /// CTE/view columns that carry no database origin. Used by the `*`-wildcard
    /// ambiguity check to keep same-named columns from different databases distinct
    /// (`SELECT * FROM t, aux.t`).
    pub schema: Option<String>,
    /// The column's type affinity (Blob/NONE when unknown).
    pub affinity: Affinity,
    /// The column's declared collating sequence (`BINARY` by default).
    pub collation: crate::value::Collation,
    /// A hidden column (e.g. `json_each`'s `json`/`root` input columns) is
    /// resolvable by name but excluded from `*` / `tbl.*` expansion.
    pub hidden: bool,
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
        // An unbound parameter reads as NULL, exactly as SQLite's bind API does: a
        // statement stepped without a value bound to a `?`/`?N`/`:name`/`@name`/`$name`
        // parameter sees NULL there (the shell relies on this for `SELECT ?1`).
        Ok(match p {
            Param::Anonymous => self.positional.get(anon_index).cloned(),
            Param::Numbered(n) => self.positional.get((*n as usize).wrapping_sub(1)).cloned(),
            Param::Named(name) => self
                .named
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone()),
        }
        .unwrap_or(Value::Null))
    }
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

    fn resolve_column(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
        name: &str,
        quoted: bool,
    ) -> Result<Value> {
        // Special rowid aliases (`rowid`/`_rowid_`/`oid`), optionally qualified
        // by a table name in scope (`t.rowid`). A real (non-hidden) column named
        // exactly like the alias always wins, so only fall back to the rowid when
        // no such real column matches.
        if is_rowid_alias(name)
            && !self.columns.iter().any(|col| {
                !col.hidden
                    && col.name.eq_ignore_ascii_case(name)
                    && table.is_none_or(|t| col.table.eq_ignore_ascii_case(t))
            })
        {
            // In a join, each base table contributes its rowid as a hidden column
            // named `rowid` tagged with the table's alias/name; a qualified rowid
            // alias (`t.rowid`/`t._rowid_`/`t.oid`) resolves to that table's hidden
            // rowid. (A `WITHOUT ROWID` table contributes none, so it still errors.)
            for (i, col) in self.columns.iter().enumerate() {
                if col.hidden
                    && col.name.eq_ignore_ascii_case("rowid")
                    && table.is_none_or(|t| col.table.eq_ignore_ascii_case(t))
                {
                    return Ok(self.row[i].clone());
                }
            }
            // Single-table scan: the row carries one rowid in `self.rowid`.
            let qualifies = match table {
                None => true,
                Some(t) => self.columns.iter().any(|c| c.table.eq_ignore_ascii_case(t)),
            };
            if qualifies && let Some(r) = self.rowid {
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
        if let Some(s) = self.subqueries
            && let Some(v) = s.resolve_outer(table, name)
        {
            return Ok(v);
        }
        // The FTS5 `rank` hidden column: the current row's relevance score under
        // the table's configured (or default `bm25()`) ranking function, when a
        // `MATCH` query over an `fts5` table is in scope (else just an unknown name).
        if table.is_none()
            && name.eq_ignore_ascii_case("rank")
            && let Some(score) = self.rowid.and_then(|r| self.subqueries?.fts5_rank(r))
        {
            return Ok(Value::Real(score?));
        }
        Err(no_such_column(schema, table, name, quoted))
    }
}

/// Build SQLite's `no such column` error for an unresolved column reference.
///
/// SQLite reports a qualified reference with its qualifier intact — a two-part
/// `t.c` as `no such column: t.c`, a three-part `s.t.c` as `no such column:
/// s.t.c` (`schema` is only ever set together with `table`) — and a bare
/// reference by name alone (`no such column: c`). When the bare name was written
/// as a *double-quoted* identifier (`"c"`) — which is ambiguous with a string
/// literal — it adds a hint and re-quotes the name: `no such column: "c" -
/// should this be a string literal in single-quotes?`. The hint never appears
/// for a qualified reference, a bare word, or a `[bracket]`/`` `backtick` ``
/// identifier.
pub(crate) fn no_such_column(
    schema: Option<&str>,
    table: Option<&str>,
    name: &str,
    quoted: bool,
) -> Error {
    Error::Error(match (schema, table) {
        (Some(s), Some(t)) => alloc::format!("no such column: {s}.{t}.{name}"),
        (_, Some(t)) => alloc::format!("no such column: {t}.{name}"),
        (_, None) if quoted => alloc::format!(
            "no such column: \"{name}\" - should this be a string literal in single-quotes?"
        ),
        (_, None) => alloc::format!("no such column: {name}"),
    })
}

/// The affinity of an expression for comparison purposes: a column's declared
/// affinity, a CAST's target affinity, transparent through parentheses, else
/// none (BLOB).
/// The affinity of an expression for comparison purposes, or `None` when the
/// expression has *no* affinity. SQLite distinguishes a typeless column (which
/// has BLOB/NONE affinity) from a literal or computed expression (which has no
/// affinity at all): the difference decides whether text coercion applies (see
/// [`apply_comparison_affinity`]).
pub(crate) fn expr_affinity(expr: &Expr, ctx: &EvalCtx) -> Option<Affinity> {
    match expr {
        Expr::Column { table, column, .. } => {
            for col in ctx.columns {
                let name_ok = col.name.eq_ignore_ascii_case(column);
                let table_ok = table
                    .as_deref()
                    .is_none_or(|t| col.table.eq_ignore_ascii_case(t));
                if name_ok && table_ok {
                    return Some(col.affinity);
                }
            }
            // A bare rowid alias (`rowid`/`_rowid_`/`oid`) that does not name a
            // real column resolves to the integer rowid, so it carries INTEGER
            // affinity for comparison — `rowid = '2'` numerically coerces '2'.
            if is_rowid_alias(column) && ctx.rowid.is_some() {
                return Some(Affinity::Integer);
            }
            // A correlated reference to an OUTER column carries that column's
            // affinity (not "no affinity" like a literal), so a comparison against
            // it inside the subquery applies the two-column comparison affinity.
            ctx.subqueries
                .and_then(|s| s.resolve_outer_affinity(table.as_deref(), column))
        }
        Expr::Cast { type_name, .. } => Some(Affinity::from_type(Some(type_name))),
        Expr::Paren(e) => expr_affinity(e, ctx),
        // `COLLATE` changes only the collating sequence, not the affinity — so a
        // `COLLATE`-wrapped column keeps its column affinity. Without this arm a
        // typeless column (`c COLLATE NOCASE`) fell through to `None`, which
        // wrongly flipped the "TEXT vs no-affinity *literal*" rule below on and
        // coerced the other side (e.g. `text_col = none_col COLLATE NOCASE`
        // spuriously matched `'1'` against `1`).
        Expr::Collate { expr, .. } => expr_affinity(expr, ctx),
        // A scalar subquery contributes its single result column's affinity, so a
        // comparison like `1 = (SELECT textcol)` applies `combine(left,
        // candidate_col_aff)` exactly as SQLite does — the same rule `IN (SELECT)`
        // uses. Without this the literal kept NONE affinity and the candidate
        // column's affinity was lost (`1 = (SELECT '1'::text)` wrongly compared
        // int-vs-text). Resolved via the subquery runner (it has schema access);
        // `None` for a computed result column or when no runner is in scope.
        Expr::Subquery(select) => ctx.subqueries.and_then(|sq| sq.column_affinity(select)),
        // Literals and computed expressions carry no affinity.
        _ => None,
    }
}

/// Apply SQLite comparison affinity to a pair of operands before comparing.
/// Apply SQLite's pre-comparison affinity rules to a pair of operands, given each
/// operand's affinity (`None` = no affinity, e.g. a literal):
///
/// * if one side has numeric affinity and the other does not, NUMERIC affinity is
///   applied to the other (text/blob/no-affinity → number where possible);
/// * else if one side has TEXT affinity and the other has *no* affinity (a
///   literal), TEXT affinity is applied to that literal;
/// * otherwise the operands are compared as stored.
///
/// The second rule deliberately does **not** fire when the other side is a
/// typeless column (BLOB/NONE affinity): SQLite compares `none_col = text_col`
/// without coercion, so `1 = '1'` across such columns is false.
pub fn apply_comparison_affinity(
    l: Value,
    la: Option<Affinity>,
    r: Value,
    ra: Option<Affinity>,
) -> (Value, Value) {
    let numeric = |a: Option<Affinity>| {
        matches!(
            a,
            Some(Affinity::Integer | Affinity::Real | Affinity::Numeric)
        )
    };
    if numeric(la) && !numeric(ra) {
        (l, Affinity::Numeric.coerce(r))
    } else if numeric(ra) && !numeric(la) {
        (Affinity::Numeric.coerce(l), r)
    } else if la == Some(Affinity::Text) && ra.is_none() {
        (l, Affinity::Text.coerce(r))
    } else if ra == Some(Affinity::Text) && la.is_none() {
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
pub(crate) fn resolve_collation(left: &Expr, right: &Expr, ctx: &EvalCtx) -> Collation {
    explicit_collation(left)
        .or_else(|| explicit_collation(right))
        .or_else(|| column_collation_of(left, ctx))
        .or_else(|| column_collation_of(right, ctx))
        .unwrap_or_default()
}

fn explicit_collation(e: &Expr) -> Option<Collation> {
    match e {
        Expr::Collate { collation, .. } => crate::value::resolve_collation_name(collation),
        Expr::Paren(inner) => explicit_collation(inner),
        _ => None,
    }
}

fn column_collation_of(e: &Expr, ctx: &EvalCtx) -> Option<Collation> {
    match e {
        Expr::Column { table, column, .. } => ctx.column_collation(table.as_deref(), column),
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
        Expr::Column {
            schema,
            table,
            column,
            quoted,
            ..
        } => ctx.resolve_column(schema.as_deref(), table.as_deref(), column, *quoted),
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
                    // A row value `(a,b,…)` or a multi-column subquery `(SELECT x,y)`
                    // on either side makes this a row (vector) comparison. Equal
                    // arity compares lexicographically (the subquery's first row
                    // becomes literal exprs so the element-wise comparison — operand
                    // affinity + the row side's collation — applies; an empty
                    // subquery makes the whole comparison NULL). Any *other* arity,
                    // including a vector against a scalar, is "row value misused".
                    let la = operand_arity(left, ctx);
                    let ra = operand_arity(right, ctx);
                    if la > 1 || ra > 1 {
                        if la != ra {
                            return Err(Error::Error("row value misused".into()));
                        }
                        return match (operand_row(left, ctx)?, operand_row(right, ctx)?) {
                            (Some(ls), Some(rs)) => compare_row_values(*op, &ls, &rs, ctx),
                            _ => Ok(Value::Null),
                        };
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
                // `x IS TRUE`/`IS FALSE` (and their `IS NOT` forms) test
                // truthiness, not value equality: `2 IS TRUE` is 1, and
                // `NULL IS TRUE` is 0.
                Is | IsNot if matches!(unparen(right), Expr::Literal(Literal::Boolean(_))) => {
                    let want = matches!(unparen(right), Expr::Literal(Literal::Boolean(true)));
                    let is_truthy = truth(&eval(left, ctx)?) == Some(want);
                    let res = if matches!(op, IsNot) {
                        !is_truthy
                    } else {
                        is_truthy
                    };
                    Ok(Value::Integer(res as i64))
                }
                // `x IS y` / `x IS NOT y` (non-boolean) behave like `=`/`<>` but
                // treat NULL as a comparable value — and, like `=`, apply operand
                // comparison affinity and the resolved collation (so
                // `intcol IS textcol` numerically coerces, matching SQLite). Without
                // this, IS compared raw storage classes and missed the coercion.
                // A row value / multi-column subquery on either side: `IS`/`IS NOT`
                // compares element-wise (NULL matches NULL) when arities match, and
                // is "row value misused" for any mismatch (incl. vector-vs-scalar).
                Is | IsNot => {
                    let la = operand_arity(left, ctx);
                    let ra = operand_arity(right, ctx);
                    if la > 1 || ra > 1 {
                        if la != ra {
                            return Err(Error::Error("row value misused".into()));
                        }
                        // An empty subquery operand acts as a row of NULLs.
                        let ls = operand_row(left, ctx)?
                            .unwrap_or_else(|| alloc::vec![Expr::Literal(Literal::Null); la]);
                        let rs = operand_row(right, ctx)?
                            .unwrap_or_else(|| alloc::vec![Expr::Literal(Literal::Null); ra]);
                        return compare_row_values_is(matches!(op, IsNot), &ls, &rs, ctx);
                    }
                    let l = eval(left, ctx)?;
                    let r = eval(right, ctx)?;
                    let (l, r) = apply_comparison_affinity(
                        l,
                        expr_affinity(left, ctx),
                        r,
                        expr_affinity(right, ctx),
                    );
                    let eq = match (&l, &r) {
                        (Value::Null, Value::Null) => true,
                        (Value::Null, _) | (_, Value::Null) => false,
                        _ => {
                            crate::value::cmp_values_coll(
                                &l,
                                &r,
                                resolve_collation(left, right, ctx),
                            ) == Ordering::Equal
                        }
                    };
                    Ok(bool_value(eq == matches!(op, Is)))
                }
                _ => eval_binary(
                    *op,
                    eval(left, ctx)?,
                    eval(right, ctx)?,
                    ctx.subqueries.is_some_and(|s| s.case_sensitive_like()),
                ),
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
            candidate_affinity,
        } => {
            if let Some(ls) = as_row_value(expr) {
                return eval_row_in(ls, list, *negated, ctx);
            }
            eval_in(expr, list, *negated, candidate_affinity.as_deref(), ctx)
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            // `x BETWEEN lo AND hi` desugars to `x >= lo AND x <= hi`. When any
            // operand is a row value / multi-column subquery, all three must share
            // the same arity (a row comparison on each bound); a mismatch — or a
            // vector mixed with scalars — is "row value misused".
            let ea = operand_arity(expr, ctx);
            let la = operand_arity(low, ctx);
            let ha = operand_arity(high, ctx);
            if ea > 1 || la > 1 || ha > 1 {
                if ea != la || ea != ha {
                    return Err(Error::Error("row value misused".into()));
                }
                let (Some(ev), Some(lv), Some(hv)) = (
                    operand_row(expr, ctx)?,
                    operand_row(low, ctx)?,
                    operand_row(high, ctx)?,
                ) else {
                    return Ok(Value::Null);
                };
                let ge = compare_row_values(BinaryOp::GtEq, &ev, &lv, ctx)?;
                let le = compare_row_values(BinaryOp::LtEq, &ev, &hv, ctx)?;
                let within = and3(&ge, &le);
                return Ok(if *negated { not3(within) } else { within });
            }
            let v = eval(expr, ctx)?;
            let lo = eval(low, ctx)?;
            let hi = eval(high, ctx)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            // `x BETWEEN lo AND hi` is `x >= lo AND x <= hi`; each comparison
            // applies pre-comparison affinity (the left operand's affinity is
            // pushed onto a bare literal bound) and resolves its own collation
            // (left operand's, else an explicit `COLLATE` on that bound), as in
            // SQLite — so `i BETWEEN '5' AND '15'` numerically coerces the text
            // bounds when `i` has numeric affinity.
            let ea = expr_affinity(expr, ctx);
            let (vl, lo) = apply_comparison_affinity(v.clone(), ea, lo, expr_affinity(low, ctx));
            let (vh, hi) = apply_comparison_affinity(v, ea, hi, expr_affinity(high, ctx));
            let ge = matches!(
                crate::value::cmp_values_coll(&vl, &lo, resolve_collation(expr, low, ctx)),
                Ordering::Greater | Ordering::Equal
            );
            let le = matches!(
                crate::value::cmp_values_coll(&vh, &hi, resolve_collation(expr, high, ctx)),
                Ordering::Less | Ordering::Equal
            );
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
            name,
            args,
            star,
            over,
            span,
            ..
        } => {
            // A window function — any call carrying an `OVER` clause, including an
            // aggregate like `sum(x) OVER ()` — is only valid in the SELECT list /
            // ORDER BY of a windowed query, where it is pre-computed and
            // substituted out before this per-row evaluator runs. Reaching it here
            // means it sits in a disallowed position (e.g. `WHERE`), which SQLite
            // reports as "misuse of window function NAME()" rather than the generic
            // aggregate-context error.
            if over.is_some() {
                return Err(Error::Error(alloc::format!(
                    "misuse of window function {}()",
                    name.to_ascii_lowercase()
                )));
            }
            // Tag a bare arity error with this call's source offset, so the shell
            // carets the exact function even when the same name appears earlier in a
            // valid call (`abs(a), abs(a,a)`). The arity message is about the call
            // itself (not an argument), and a nested call's own arity error already
            // arrives as `ErrorAt` — so matching a raw `Error::Error` here never
            // re-tags it. Synthetic calls (`Span::none()`) carry no offset.
            match super::func::eval_scalar(name, args, *star, ctx) {
                Err(Error::Error(m))
                    if span.0.is_some()
                        && m.starts_with("wrong number of arguments to function ") =>
                {
                    Err(Error::ErrorAt(
                        m,
                        span.0.expect("checked is_some").0 as usize,
                    ))
                }
                other => other,
            }
        }
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
            let rows = match ctx.subqueries {
                Some(s) => s.rows(select, ctx)?,
                None => return Err(Error::Unsupported("IN (SELECT …) in this context")),
            };
            // A scalar `x IN (SELECT …)` requires the subquery to yield exactly
            // one column; SQLite rejects `1 IN (SELECT 1, 2)` ("sub-select
            // returns 2 columns - expected 1") rather than taking the first.
            let mut set = Vec::with_capacity(rows.len());
            for mut row in rows {
                if row.len() != 1 {
                    return Err(Error::Error(alloc::format!(
                        "sub-select returns {} columns - expected 1",
                        row.len()
                    )));
                }
                set.push(row.swap_remove(0));
            }
            // Membership uses the left operand's collation, as in SQLite.
            let coll = key_collation(expr, ctx);
            // SQLite applies a comparison affinity to each `x IN (SELECT y)` test,
            // derived from x's affinity and the subquery column y's affinity (no
            // conversion if either side has NONE affinity — that is why this is not
            // the same as `IN (list)`, where each literal item has no affinity).
            let la = expr_affinity(expr, ctx);
            let ra = ctx.subqueries.and_then(|s| s.column_affinity(select));
            let mut saw_null = false;
            for iv in &set {
                if matches!(iv, Value::Null) {
                    saw_null = true;
                    continue;
                }
                let (lv, rv) = apply_comparison_affinity(v.clone(), la, iv.clone(), ra);
                if crate::value::cmp_values_coll(&lv, &rv, coll) == Ordering::Equal {
                    return Ok(bool_value(!negated));
                }
            }
            if saw_null {
                Ok(Value::Null)
            } else {
                Ok(bool_value(*negated))
            }
        }
        // A row value (`(a, b, …)`) reaching the scalar evaluator sits in a
        // context that does not permit one — SQLite reports "row value misused".
        Expr::RowValue(_) => Err(Error::Error("row value misused".into())),
    }
}

/// View an expression as a row value's element list, if it is one (transparently
/// through a redundant parenthesization).
/// Peel away `(…)` wrappers to reach the inner expression.
fn unparen(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(inner) => unparen(inner),
        other => other,
    }
}

fn as_row_value(e: &Expr) -> Option<&[Expr]> {
    match e {
        Expr::RowValue(items) => Some(items),
        Expr::Paren(inner) => as_row_value(inner),
        _ => None,
    }
}

/// The first row of a row-returning subquery as literal expressions (so the
/// existing row-value comparison machinery can use it), or `None` when the
/// subquery yields no rows (the comparison is then unknown / NULL).
fn row_subquery_first(select: &Select, ctx: &EvalCtx) -> Result<Option<Vec<Expr>>> {
    let Some(s) = ctx.subqueries else {
        return Err(Error::Unsupported("subquery in this context"));
    };
    Ok(s.rows(select, ctx)?.into_iter().next().map(|vals| {
        vals.into_iter()
            .map(|v| {
                Expr::Literal(match v {
                    Value::Null => Literal::Null,
                    Value::Integer(i) => Literal::Integer(i),
                    Value::Real(r) => Literal::Real(r),
                    Value::Text(t) => Literal::Str(t.as_str().to_string()),
                    Value::Blob(b) => Literal::Blob(b),
                })
            })
            .collect()
    }))
}

/// The structural column count ("arity") of a comparison operand viewed as a row
/// value: a literal `(a, b, …)` row value's length, a scalar subquery's
/// result-column count, or 1 for any ordinary scalar expression. SQLite decides
/// this at prepare time; here it comes from the row value's element count or the
/// subquery's output-column metadata (no rows are evaluated).
fn operand_arity(e: &Expr, ctx: &EvalCtx) -> usize {
    match unparen(e) {
        Expr::RowValue(items) => items.len(),
        Expr::Subquery(select) => ctx
            .subqueries
            .map(|s| s.row_column_affinities(select).len())
            .filter(|&n| n > 0)
            .unwrap_or(1),
        _ => 1,
    }
}

/// Materialise a comparison operand's first row as literal exprs: a row value's
/// elements directly, or a subquery's first row (`None` when the subquery yields
/// no rows). Only called when [`operand_arity`] reported a width above 1, so the
/// operand is always a row value or a subquery.
fn operand_row(e: &Expr, ctx: &EvalCtx) -> Result<Option<Vec<Expr>>> {
    match unparen(e) {
        Expr::RowValue(items) => Ok(Some(items.clone())),
        Expr::Subquery(select) => row_subquery_first(select, ctx),
        other => Ok(Some(alloc::vec![other.clone()])),
    }
}

/// SQLite three-valued AND of two comparison results (`0`/`1`/NULL).
fn and3(a: &Value, b: &Value) -> Value {
    if matches!(a, Value::Integer(0)) || matches!(b, Value::Integer(0)) {
        bool_value(false)
    } else if matches!(a, Value::Null) || matches!(b, Value::Null) {
        Value::Null
    } else {
        bool_value(true)
    }
}

/// SQLite three-valued NOT of a comparison result (`NOT NULL` is NULL).
fn not3(a: Value) -> Value {
    match a {
        Value::Null => Value::Null,
        Value::Integer(0) => bool_value(true),
        _ => bool_value(false),
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

/// Row-value `IS` / `IS NOT`: `(a, …) IS (b, …)` holds when every element is
/// `IS`-equal (NULL matches NULL, NULL never matches a non-NULL). The result is
/// always definite — unlike `=`, a row `IS` never yields NULL. The caller
/// guarantees equal arity.
fn compare_row_values_is(
    is_not: bool,
    lefts: &[Expr],
    rights: &[Expr],
    ctx: &EvalCtx,
) -> Result<Value> {
    let mut all_eq = true;
    for (le, re) in lefts.iter().zip(rights) {
        let l = eval(le, ctx)?;
        let r = eval(re, ctx)?;
        let eq = match (&l, &r) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            _ => {
                let (l, r) =
                    apply_comparison_affinity(l, expr_affinity(le, ctx), r, expr_affinity(re, ctx));
                crate::value::cmp_values_coll(&l, &r, resolve_collation(le, re, ctx))
                    == Ordering::Equal
            }
        };
        if !eq {
            all_eq = false;
        }
    }
    Ok(bool_value(all_eq != is_not))
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
    // Each comparison applies the left element's affinity vs the subquery column's
    // affinity (like the scalar `IN (SELECT …)` path) and the left's collation.
    let lafs: Vec<Option<Affinity>> = lefts.iter().map(|e| expr_affinity(e, ctx)).collect();
    let lcolls: Vec<Collation> = lefts.iter().map(|e| key_collation(e, ctx)).collect();
    let rafs = ctx
        .subqueries
        .map(|s| s.row_column_affinities(select))
        .unwrap_or_default();
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
            .enumerate()
            .map(|(i, (l, r))| {
                if matches!(l, Value::Null) || matches!(r, Value::Null) {
                    None
                } else {
                    let (lv, rv) = apply_comparison_affinity(
                        l.clone(),
                        lafs[i],
                        r.clone(),
                        rafs.get(i).copied().flatten(),
                    );
                    Some(crate::value::cmp_values_coll(&lv, &rv, lcolls[i]))
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
        Literal::Str(s) => Value::Text(s.clone().into()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Boolean(b) => Value::Integer(*b as i64),
    }
}

fn eval_unary(op: UnaryOp, v: Value) -> Result<Value> {
    Ok(match op {
        UnaryOp::Identity => v,
        UnaryOp::Negate => match to_number(&v) {
            // Negating `i64::MIN` overflows; SQLite promotes it to a real
            // (`-(-9223372036854775808)` -> 9.22e18), so fall back to f64.
            Value::Integer(i) => i
                .checked_neg()
                .map(Value::Integer)
                .unwrap_or(Value::Real(-(i as f64))),
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

fn eval_in(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    candidate_affinity: Option<&str>,
    ctx: &EvalCtx,
) -> Result<Value> {
    // An empty IN list is always false (`NOT IN`: always true) — SQLite
    // short-circuits before NULL semantics, so even `NULL IN ()` is 0, not NULL.
    if list.is_empty() {
        return Ok(bool_value(negated));
    }
    let v = eval(expr, ctx)?;
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    // SQLite rewrites a single-element `x IN (y)` to `x = y`, so an explicit
    // `COLLATE` on that one element applies (`'a' IN ('A' COLLATE NOCASE)` is
    // true). A multi-element list instead uses the left operand's collation only
    // — per-element `COLLATE` is ignored there (`'a' IN ('x','A' COLLATE NOCASE)`
    // is false).
    let coll = if list.len() == 1 {
        resolve_collation(expr, &list[0], ctx)
    } else {
        key_collation(expr, ctx)
    };
    // Pre-comparison affinity: the left operand's affinity is pushed onto a bare
    // literal list element, so `i IN ('10','20')` numerically coerces the text
    // elements when `i` has numeric affinity (mirrors `=`).
    let ea = expr_affinity(expr, ctx);
    // SQLite's `x IN (list)` applies ONLY the left operand's affinity (`ea`) to
    // each element — the element's own affinity is ignored (a list element is not
    // a column/subquery operand for affinity purposes). So `ra` is None for an
    // ordinary list. The sole exception is a list folded from a bare-column
    // `x IN (SELECT col)`: there the candidate side contributes the SELECTed
    // column's affinity (carried as a canonical type name) so the comparison
    // reproduces SQLite's `combine(left_aff, col_aff)` exactly.
    let cand_aff = candidate_affinity.map(|t| Affinity::from_type(Some(t)));
    let mut saw_null = false;
    for item in list {
        let iv = eval(item, ctx)?;
        if matches!(iv, Value::Null) {
            saw_null = true;
            continue;
        }
        let ra = cand_aff;
        let (lv, iv) = apply_comparison_affinity(v.clone(), ea, iv, ra);
        if crate::value::cmp_values_coll(&lv, &iv, coll) == Ordering::Equal {
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
        let matched = match (&base, operand) {
            // `CASE x WHEN y` matches when `x = y`, resolving collation per-WHEN
            // like a binary comparison: x's explicit/column collation, else an
            // explicit `COLLATE` on the WHEN expression.
            (Some(b), Some(op_expr)) => {
                let w = eval(when, ctx)?;
                let coll = resolve_collation(op_expr, when, ctx);
                // `CASE x WHEN y` is `x = y`, so apply comparison affinity to the
                // pair just like a binary `=` — the operand's affinity is pushed
                // onto a bare-literal WHEN value, and a column/subquery WHEN value
                // contributes its own affinity (e.g. `CASE 5 WHEN (SELECT t)` over
                // a TEXT column coerces 5→'5'). Without this, a numeric operand
                // never matched a text WHEN value.
                let (bv, wv) = apply_comparison_affinity(
                    b.clone(),
                    expr_affinity(op_expr, ctx),
                    w,
                    expr_affinity(when, ctx),
                );
                !matches!(bv, Value::Null)
                    && !matches!(wv, Value::Null)
                    && crate::value::cmp_values_coll(&bv, &wv, coll) == Ordering::Equal
            }
            // `CASE WHEN cond` (no base operand) matches when cond is true.
            _ => truth(&eval(when, ctx)?) == Some(true),
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

fn eval_binary(op: BinaryOp, l: Value, r: Value, case_sensitive_like: bool) -> Result<Value> {
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
                // SQLite's `||` concatenates the *raw bytes* of each operand's
                // text representation (a blob contributes its bytes verbatim, a
                // number its decimal text), yielding a value whose storage class
                // is always TEXT — even when the bytes are not valid UTF-8
                // (`x'00' || x'ff'` -> the 2-byte text `00FF`, `typeof` = `text`).
                // `Value::Text` is byte-backed, so those bytes are kept as text
                // exactly like SQLite.
                let mut bytes = text_bytes(&l);
                bytes.extend_from_slice(&text_bytes(&r));
                Value::Text(crate::value::Text::from_bytes(bytes))
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
                    // GLOB ignores `case_sensitive_like` (always case-sensitive);
                    // LIKE folds ASCII case unless the pragma is on.
                    like_match_escape(&pat, &text, None, case_sensitive_like)
                } else {
                    glob_match(&pat, &text)
                };
                bool_value(m)
            }
        }
        JsonExtract => crate::exec::json::arrow(&l, &r, false)?,
        JsonExtractText => crate::exec::json::arrow(&l, &r, true)?,
        And | Or => unreachable!("handled with short-circuiting"),
    })
}

/// Apply an arithmetic `BinaryOp` to two values (SQLite numeric semantics).
/// Public wrapper used by the VDBE interpreter.
pub fn arithmetic_values(op: BinaryOp, l: &Value, r: &Value) -> Value {
    arithmetic(op, l.clone(), r.clone())
}

/// Apply `LIKE` (`glob` false) or `GLOB` (`glob` true) to two values, matching
/// SQLite: NULL on either side yields NULL, otherwise both coerce to text and
/// the left value is matched against the right pattern. Public wrapper used by
/// the VDBE interpreter.
pub fn like_glob_values(glob: bool, l: &Value, r: &Value) -> Value {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Value::Null;
    }
    let text = to_text(l);
    let pat = to_text(r);
    let m = if glob {
        glob_match(&pat, &text)
    } else {
        like_match(&pat, &text)
    };
    bool_value(m)
}

/// Concatenate two values for `||`, matching SQLite: NULL on either side yields
/// NULL; otherwise the operands' byte representations are joined (a blob
/// contributes its bytes, a number its decimal text), always yielding TEXT — the
/// bytes are kept as text even when not valid UTF-8 (`x'00' || x'ff'`), matching
/// SQLite's `typeof`. Public wrapper used by the VDBE interpreter.
pub fn concat_values(l: &Value, r: &Value) -> Value {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Value::Null;
    }
    let mut bytes = text_bytes(l);
    bytes.extend_from_slice(&text_bytes(r));
    Value::Text(crate::value::Text::from_bytes(bytes))
}

/// Apply `IS` / `IS NOT` to two values, treating NULL as a comparable value
/// (never returns NULL). Public wrapper used by the VDBE interpreter.
pub fn is_values(is: bool, l: &Value, r: &Value) -> Value {
    let eq = match (l, r) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => compare(l, r) == core::cmp::Ordering::Equal,
    };
    bool_value(eq == is)
}

/// Apply a bitwise `BinaryOp` (`&`, `|`, `<<`, `>>`) to two values, matching
/// SQLite: NULL on either side yields NULL, otherwise both sides coerce to
/// integers. Public wrapper used by the VDBE interpreter.
pub fn bitwise_values(op: BinaryOp, l: &Value, r: &Value) -> Value {
    use BinaryOp::*;
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Value::Null;
    }
    let a = to_i64(l);
    let b = to_i64(r);
    Value::Integer(match op {
        BitAnd => a & b,
        BitOr => a | b,
        LShift => shift_left(a, b),
        RShift => shift_right(a, b),
        _ => return Value::Null,
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
                    // `i64::MIN / -1` overflows i64; SQLite promotes it to a
                    // real (9.22e18). Every other quotient stays integer.
                    a.checked_div(b)
                        .map(Value::Integer)
                        .unwrap_or(Value::Real(a as f64 / b as f64))
                }
            }
            Mod => {
                if b == 0 {
                    Value::Null
                } else {
                    // `i64::MIN % -1` overflows in plain `%`; the true remainder
                    // is 0. `wrapping_rem` already yields 0 here.
                    Value::Integer(a.wrapping_rem(b))
                }
            }
            _ => unreachable!(),
        };
    }
    let a = number_as_f64(&ln);
    let b = number_as_f64(&rn);
    // A NaN result (e.g. `inf - inf`) becomes NULL, as in SQLite. Infinities are
    // kept (and printed as ±9.0e+999).
    let real = |r: f64| {
        if r.is_nan() {
            Value::Null
        } else {
            Value::Real(r)
        }
    };
    match op {
        Add => real(a + b),
        Sub => real(a - b),
        Mul => real(a * b),
        Div => {
            if b == 0.0 {
                Value::Null
            } else {
                real(a / b)
            }
        }
        Mod => {
            // SQLite's `%` truncates both operands to integers, then takes the
            // integer remainder; the result is real because an operand is real
            // (e.g. `10.5 % 3` → `10 % 3` → `1.0`). A divisor that truncates to 0
            // yields NULL.
            let bi = b as i64;
            if bi == 0 {
                Value::Null
            } else {
                Value::Real((a as i64).wrapping_rem(bi) as f64)
            }
        }
        _ => unreachable!(),
    }
}

fn shift_left(a: i64, b: i64) -> i64 {
    if b < 0 {
        // SQLite treats a left shift by a negative amount as a right shift by its
        // magnitude — and crucially an *arithmetic* one (sign-extending, so
        // `-1 << -2 == -1`, not the logical `0x3fff…`). Delegate to `shift_right`
        // rather than shifting `a as u64`. `saturating_neg` guards `i64::MIN` (its
        // magnitude is ≥ 64, which `shift_right` maps to -1/0 by sign).
        shift_right(a, b.saturating_neg())
    } else if b >= 64 {
        0
    } else {
        ((a as u64) << b as u32) as i64
    }
}

fn shift_right(a: i64, b: i64) -> i64 {
    if b <= -64 {
        // A right shift by a magnitude >= 64 is a left shift off the top: 0.
        // (Guards against `-b` overflowing when `b == i64::MIN`.)
        0
    } else if b < 0 {
        shift_left(a, -b)
    } else if b >= 64 {
        if a < 0 { -1 } else { 0 }
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
    // Casting a blob to a non-blob type first reinterprets its bytes as text, so
    // `CAST(x'3132' AS INTEGER)` reads "12" and yields 12 (not 0). The bytes are
    // kept verbatim (byte-backed text), so `CAST(x'ff' AS TEXT)` is non-UTF-8 text
    // rather than a lossy conversion, matching SQLite.
    let v = match v {
        Value::Blob(b) if !aff.contains("BLOB") => Value::Text(crate::value::Text::from_bytes(b)),
        other => other,
    };
    if aff.contains("INT") {
        // CAST to INTEGER takes the leading *integer* prefix of text (stopping at
        // a `.` or exponent), unlike numeric coercion which reads the whole float:
        // `CAST('2e2' AS INTEGER)` is 2, not 200, and `CAST('2.9' AS INTEGER)` is 2.
        Value::Integer(match &v {
            Value::Text(s) => parse_int_prefix(s),
            _ => to_i64(&v),
        })
    } else if aff.contains("CHAR") || aff.contains("CLOB") || aff.contains("TEXT") {
        Value::Text(to_text(&v).into())
    } else if aff.contains("REAL") || aff.contains("FLOA") || aff.contains("DOUB") {
        Value::Real(number_as_f64(&to_number(&v)))
    } else if aff.contains("BLOB") {
        // CAST to BLOB: the value becomes a blob of the bytes of its text form
        // (an existing blob is unchanged). Matches SQLite, e.g. `65` -> X'3635'.
        match v {
            Value::Blob(_) => v,
            Value::Text(s) => Value::Blob(s.as_bytes().to_vec()),
            other => Value::Blob(to_text(&other).into_bytes()),
        }
    } else if aff.is_empty() {
        // A type name with no affinity keyword leaves the value unchanged.
        v
    } else {
        // NUMERIC affinity. Text is converted, reducing an integral real to an
        // integer (`'3.0'` -> 3); an existing REAL/INTEGER value is kept as-is
        // (`CAST(2.0 AS NUMERIC)` stays 2.0 — unlike NUMERIC *storage*).
        match v {
            Value::Text(_) => match to_number(&v) {
                Value::Real(r) => integral_real_to_int(r).unwrap_or(Value::Real(r)),
                other => other,
            },
            other => to_number(&other),
        }
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
        // SQLite reads a blob's bytes *as a text string* and applies the same
        // text→number rule (so `abs(x'3132')` is 12, not 0). The numeric prefix
        // is ASCII, so a lossy UTF-8 view preserves it; trailing invalid bytes
        // become non-digits that end the prefix just as they would in SQLite.
        Value::Blob(b) => parse_number(&String::from_utf8_lossy(b)),
    }
}

/// Parse `t` as an f64 the way SQLite's text→number conversion does: like
/// `f64::from_str` but rejecting the word forms (`inf`, `infinity`, `nan`) that
/// Rust accepts and SQLite does not. A numeric *overflow* such as `1e400` is
/// still a valid number and yields ±Inf.
pub(crate) fn parse_decimal_f64(t: &str) -> Option<f64> {
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    match body.as_bytes().first() {
        // A SQLite numeric literal begins with a digit or a decimal point;
        // anything else ("inf", "nan", "0x…" — Rust rejects hex anyway) is not a
        // number to SQLite.
        Some(c) if c.is_ascii_digit() || *c == b'.' => t.parse::<f64>().ok(),
        _ => None,
    }
}

/// Parse the leading signed-integer prefix of `s` the way SQLite's
/// `sqlite3Atoi64` does for `CAST(text AS INTEGER)`: skip leading ASCII
/// whitespace, an optional sign, then decimal digits only — stopping at the
/// first non-digit (so `.`/`e`/letters end it). No digits yields 0; overflow
/// saturates to `i64::MIN`/`i64::MAX`.
fn parse_int_prefix(s: &str) -> i64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let start = i;
    // Accumulate as a negative magnitude so `i64::MIN` is representable exactly.
    let mut acc: i64 = 0;
    let mut overflow = false;
    while i < b.len() && b[i].is_ascii_digit() {
        let d = (b[i] - b'0') as i64;
        match acc.checked_mul(10).and_then(|x| x.checked_sub(d)) {
            Some(v) => acc = v,
            None => {
                overflow = true;
                break;
            }
        }
        i += 1;
    }
    if i == start {
        return 0;
    }
    if overflow {
        return if neg { i64::MIN } else { i64::MAX };
    }
    if neg {
        acc
    } else {
        acc.checked_neg().unwrap_or(i64::MAX)
    }
}

fn parse_number(s: &str) -> Value {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Value::Integer(i);
    }
    if let Some(f) = parse_decimal_f64(t) {
        return Value::Real(f);
    }
    // SQLite uses the longest valid numeric prefix; approximate that, including a
    // trailing exponent so `'3.5e2xyz'` reads `3.5e2` (350.0), not `3.5`.
    let mut end = 0;
    let bytes = t.as_bytes();
    let mut seen_dot = false;
    let mut seen_digit = false;
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_digit() {
            seen_digit = true;
            end += 1;
        } else if c == b'.' && !seen_dot {
            seen_dot = true;
            end += 1;
        } else if (c == b'-' || c == b'+') && end == 0 {
            // leading sign ok
            end += 1;
        } else if (c == b'e' || c == b'E') && seen_digit {
            // An exponent `[eE][+-]?digit+` extends (and ends) the number. If no
            // digit follows, the `e` is trailing junk and the prefix stops here.
            let mut k = end + 1;
            if k < bytes.len() && (bytes[k] == b'+' || bytes[k] == b'-') {
                k += 1;
            }
            if k < bytes.len() && bytes[k].is_ascii_digit() {
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                end = k;
            }
            break;
        } else {
            break;
        }
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

/// Coerce a value to an i64 the way SQLite's `sqlite3_value_int64`
/// (`sqlite3VdbeIntValue`) does — the conversion used for a scalar function's
/// *integer* argument (`char()`, `substr()` offsets, `zeroblob()`, `printf`
/// `%d`, …). This differs from [`to_i64`] for text/blob operands: it takes the
/// leading **integer** prefix (`sqlite3Atoi64`, stopping at `.`/`e`/any
/// non-digit) rather than parsing a full floating-point literal. So `'1e3'`
/// yields `1` here but `1000` via [`to_i64`]'s numeric parse. INTEGER and REAL
/// values convert identically to [`to_i64`] (a real truncates toward zero).
pub fn to_int_value(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        Value::Real(r) => *r as i64,
        Value::Text(s) => parse_int_prefix(s),
        Value::Blob(b) => parse_int_prefix(&String::from_utf8_lossy(b)),
        Value::Null => 0,
    }
}

/// SQLite-compatible `sum()` over `vals` (the already-collected non-null inputs).
///
/// The result is an exact INTEGER only when *every* value's numeric type is
/// INTEGER, mirroring `sqlite3_value_numeric_type` in SQLite's `sumStep`: an
/// INTEGER storage value — or a TEXT value that is a pure signed integer (with
/// optional surrounding whitespace and no decimal point or exponent, fitting in
/// an `i64`) — counts as integer. A REAL value, or any text/blob that is
/// real-valued or non-numeric, makes the result REAL. An all-integer sum that
/// overflows `i64` is the "integer overflow" error, exactly as SQLite reports
/// for both plain and windowed `sum()`. An empty input is NULL.
/// The running quantities SQLite's `sum`/`avg`/`total` maintain over the
/// non-NULL inputs (`sumStep`): an exact integer sum, frozen the moment a
/// non-integer input appears or the integer sum overflows; whether the result
/// must be approximate (any of those happened); and a Kahan–Babuška–Neumaier
/// compensated double sum of *every* value. The compensated sum is why a large
/// cancellation keeps small contributions — `sum(1e308, 3, -1e308)` is `3.0`,
/// not `0.0`.
struct SumAccum {
    i_sum: i64,
    ovrfl: bool,
    approx: bool,
    r_sum: f64,
    r_err: f64,
}

/// One Kahan–Babuška–Neumaier step: `rSum += r` with the correction term `rErr`
/// recovering the low-order bits the plain add drops when magnitudes differ.
fn kbn_step(a: &mut SumAccum, r: f64) {
    let s = a.r_sum;
    let t = s + r;
    if crate::util::float::abs(s) > crate::util::float::abs(r) {
        a.r_err += (s - t) + r;
    } else {
        a.r_err += (r - t) + s;
    }
    a.r_sum = t;
}

/// Add a possibly-large integer to the compensated sum. A magnitude ≥ 2^52 is
/// split into a high part (a multiple of 2^14) and a small remainder so each
/// piece converts to a double exactly — SQLite's `kahanBabuskaNeumaierStepInt64`.
fn kbn_step_i64(a: &mut SumAccum, v: i64) {
    if v <= -4_503_599_627_370_496 || v >= 4_503_599_627_370_496 {
        let sm = v % 16_384;
        kbn_step(a, (v - sm) as f64);
        kbn_step(a, sm as f64);
    } else {
        kbn_step(a, v as f64);
    }
}

/// Seed the compensated sum from the exact integer running sum when the first
/// non-integer (or an overflow) forces the approximate path — SQLite's
/// `kahanBabuskaNeumaierInit`, with the same 2^52 split for exactness.
fn kbn_init(a: &mut SumAccum, v: i64) {
    if v <= -4_503_599_627_370_496 || v >= 4_503_599_627_370_496 {
        let sm = v % 16_384;
        a.r_sum = (v - sm) as f64;
        a.r_err = sm as f64;
    } else {
        a.r_sum = v as f64;
        a.r_err = 0.0;
    }
}

/// Faithful port of SQLite's `sumStep`: keep an exact integer running sum until
/// the first non-integer input or an integer overflow, then switch to a
/// Kahan–Babuška–Neumaier compensated double sum seeded from that integer sum.
/// This is why `sum(9223372036854775807, -9223372036854775808, 2.5)` is `1.5`
/// (the integers cancel exactly to -1) and `sum(1e308, 3, -1e308)` is `3.0`.
/// The numeric type SQLite assigns each argument (`sqlite3_value_numeric_type`)
/// decides integer-vs-real, not the storage class: a pure-integer text `'1'`
/// stays exact, while `'1.5'`, `'1e2'`, an out-of-range integer string, or
/// non-numeric text each force the approximate path. NULLs are pre-filtered.
fn accumulate(vals: &[Value]) -> SumAccum {
    let mut a = SumAccum {
        i_sum: 0,
        ovrfl: false,
        approx: false,
        r_sum: 0.0,
        r_err: 0.0,
    };
    for v in vals {
        let is_int = matches!(to_number_strict(v), Some(Value::Integer(_)));
        if !a.approx {
            if is_int {
                let iv = match to_number_strict(v) {
                    Some(Value::Integer(i)) => i,
                    _ => 0,
                };
                match a.i_sum.checked_add(iv) {
                    Some(x) => a.i_sum = x,
                    None => {
                        // Integer overflow: freeze the exact sum, seed the
                        // compensated sum from it, and continue approximately.
                        a.ovrfl = true;
                        let seed = a.i_sum;
                        kbn_init(&mut a, seed);
                        a.approx = true;
                        kbn_step_i64(&mut a, iv);
                    }
                }
            } else {
                let seed = a.i_sum;
                kbn_init(&mut a, seed);
                a.approx = true;
                kbn_step(&mut a, to_f64(v));
            }
        } else if is_int {
            kbn_step_i64(
                &mut a,
                match to_number_strict(v) {
                    Some(Value::Integer(i)) => i,
                    _ => 0,
                },
            );
        } else {
            // A real input after an integer overflow rescues the sum: SQLite
            // clears the overflow flag, so `sum` yields a REAL rather than error.
            a.ovrfl = false;
            kbn_step(&mut a, to_f64(v));
        }
    }
    a
}

/// The finalized compensated real: `rSum + rErr`, but only when `rErr` is finite
/// (SQLite drops it when it has itself overflowed to ±∞ or NaN — its
/// `sqlite3IsOverflow` test — leaving the ±∞ `rSum` alone).
fn kbn_result(a: &SumAccum) -> f64 {
    if a.r_err.is_finite() {
        a.r_sum + a.r_err
    } else {
        a.r_sum
    }
}

/// `sum(X)` — an INTEGER when every non-NULL input is an integer and the sum
/// fits i64, a REAL (compensated) once any input is non-integer, an "integer
/// overflow" error when an all-integer sum leaves i64, and NULL for no rows.
pub fn sum_values(vals: &[Value]) -> Result<Value> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    let a = accumulate(vals);
    if !a.approx {
        Ok(Value::Integer(a.i_sum))
    } else if a.ovrfl {
        // An integer overflow that no later real input rescued is an error.
        Err(Error::Error("integer overflow".into()))
    } else {
        Ok(Value::Real(kbn_result(&a)))
    }
}

/// `total(X)` — always a REAL. Uses the compensated sum when any input is
/// non-integer (or the integer sum overflowed), else the exact integer sum cast
/// to double. Never errors on overflow (unlike `sum`).
pub fn total_value(vals: &[Value]) -> f64 {
    let a = accumulate(vals);
    if a.approx {
        kbn_result(&a)
    } else {
        a.i_sum as f64
    }
}

/// `avg(X)` — REAL, or `None` for no non-NULL rows. The dividend follows the same
/// exact-vs-compensated choice as [`total_value`].
pub fn avg_value(vals: &[Value]) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    let a = accumulate(vals);
    let r = if a.approx {
        kbn_result(&a)
    } else {
        a.i_sum as f64
    };
    Some(r / vals.len() as f64)
}

/// The raw bytes of a value's text representation, used by `||`. A blob
/// contributes its bytes verbatim (no UTF-8 coercion); every other class
/// contributes the bytes of its [`to_text`] form. Unlike `to_text(..).into_bytes()`
/// this never mangles a non-UTF-8 blob through lossy decoding.
fn text_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Blob(b) => b.clone(),
        // A text value contributes its raw bytes verbatim (which may be non-UTF-8).
        Value::Text(s) => s.as_bytes().to_vec(),
        other => to_text(other).into_bytes(),
    }
}

/// Render a value as text (for `||`, CAST to TEXT, etc.).
pub fn to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => format_real(*r),
        Value::Text(s) => s.as_str().to_string(),
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
        // SQLite's text rendering of an infinity is `Inf`/`-Inf` (its `quote()`
        // instead uses `±9.0e+999` — see `quote_value`).
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

    if neg { alloc::format!("-{body}") } else { body }
}

// ---- LIKE / GLOB ------------------------------------------------------------

/// SQLite `LIKE`: `%` matches any run, `_` any single char; case-insensitive for
/// ASCII (SQLite's default — `case_sensitive` false).
fn like_match(pattern: &str, text: &str) -> bool {
    like_match_escape(pattern, text, None, false)
}

/// `LIKE` with an optional `ESCAPE` character: a wildcard (`%`/`_`) or the escape
/// character itself, when preceded by the escape character, matches literally.
/// `case_sensitive` true (`PRAGMA case_sensitive_like = ON`) compares ASCII
/// letters exactly; false folds case, SQLite's default.
pub fn like_match_escape(
    pattern: &str,
    text: &str,
    escape: Option<char>,
    case_sensitive: bool,
) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    like_rec(&p, &t, escape, case_sensitive)
}

/// Compare one pattern char to one text char, honoring ASCII case sensitivity.
fn like_char_eq(pc: char, tc: char, case_sensitive: bool) -> bool {
    if case_sensitive {
        pc == tc
    } else {
        pc.eq_ignore_ascii_case(&tc)
    }
}

fn like_rec(p: &[char], t: &[char], esc: Option<char>, cs: bool) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    // The escape character is never a wildcard: it escapes the next character
    // (matched literally), and a trailing escape — with nothing left to escape —
    // matches only the empty remainder, exactly as SQLite does. This is why
    // `'ab' LIKE 'a_' ESCAPE '_'` is false (the `_` is the escape char, not a
    // single-character wildcard) while `'a' LIKE 'a_' ESCAPE '_'` is true.
    if let Some(e) = esc
        && p[0] == e
    {
        return match p.get(1) {
            Some(&lit) => {
                !t.is_empty() && like_char_eq(lit, t[0], cs) && like_rec(&p[2..], &t[1..], esc, cs)
            }
            None => t.is_empty(),
        };
    }
    match p[0] {
        '%' => {
            // Collapse consecutive %.
            let rest = &p[1..];
            if like_rec(rest, t, esc, cs) {
                return true;
            }
            for i in 0..t.len() {
                if like_rec(rest, &t[i + 1..], esc, cs) {
                    return true;
                }
            }
            false
        }
        '_' => !t.is_empty() && like_rec(&p[1..], &t[1..], esc, cs),
        pc => !t.is_empty() && like_char_eq(pc, t[0], cs) && like_rec(&p[1..], &t[1..], esc, cs),
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
            // A `]` as the very first class member (right after `[` or `[^`) is a
            // literal `]`, not the terminator — matching SQLite/GLOB semantics:
            // `'a]c' GLOB 'a[]]c'` is true, and `'x' GLOB '[^]]'` is true.
            if i < p.len() && p[i] == ']' {
                if t[0] == ']' {
                    matched = true;
                }
                i += 1;
            }
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
    use alloc::vec;

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
        // A `]` as the first class member is literal, not the terminator.
        assert!(glob_match("a[]]c", "a]c"));
        assert!(glob_match("[]]", "]"));
        assert!(glob_match("[^]]", "x"));
        assert!(!glob_match("[^]]", "]"));
        assert!(glob_match("[]a]", "a"));
        assert!(!glob_match("[^]a]", "a"));
    }

    #[test]
    fn concat_yields_byte_backed_text() {
        // `||` joins raw bytes and always yields TEXT — even non-UTF-8 bytes,
        // which `Value::Text` now holds directly (sqlite reports `typeof` = text).
        assert_eq!(
            eval_binary(
                BinaryOp::Concat,
                Value::Blob(vec![0x00]),
                Value::Blob(vec![0xff]),
                false
            )
            .unwrap(),
            Value::Text(crate::value::Text::from_bytes(vec![0x00, 0xff]))
        );
        // A UTF-8-valid result is the same text as before.
        assert_eq!(
            eval_binary(
                BinaryOp::Concat,
                Value::Blob(vec![0x61]),
                Value::Text("b".into()),
                false
            )
            .unwrap(),
            Value::Text("ab".into())
        );
    }

    #[test]
    fn shift_and_negate_overflow_do_not_panic() {
        // Extreme shift magnitudes saturate to 0 / sign-fill, never panic.
        assert_eq!(shift_right(-1, i64::MIN), 0);
        assert_eq!(shift_left(1, i64::MIN), 0);
        assert_eq!(shift_right(1, i64::MIN), 0);
        // -(i64::MIN) overflows i64 -> promotes to real.
        assert_eq!(
            eval_unary(UnaryOp::Negate, Value::Integer(i64::MIN)).unwrap(),
            Value::Real(-(i64::MIN as f64))
        );
        // i64::MIN / -1 overflows -> real; remainder is 0.
        assert_eq!(
            arithmetic(BinaryOp::Div, Value::Integer(i64::MIN), Value::Integer(-1)),
            Value::Real(i64::MIN as f64 / -1.0)
        );
        assert_eq!(
            arithmetic(BinaryOp::Mod, Value::Integer(i64::MIN), Value::Integer(-1)),
            Value::Integer(0)
        );
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
