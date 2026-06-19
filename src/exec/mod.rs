//! Query execution: the `Connection` API and the read-query executor.
//!
//! This layer ties the pieces together: parse SQL ([`crate::sql`]), resolve
//! names against the schema catalog ([`crate::schema`]), scan b-trees
//! ([`crate::btree`]), decode records ([`crate::format::record`]), and evaluate
//! expressions ([`eval`]) to produce result rows.
//!
//! It implements an *operational, iterator-style* executor rather than emitting
//! VDBE bytecode. The observable semantics (row order, type coercion, NULL
//! handling) follow SQLite; the bytecode representation the roadmap describes is
//! an internal-representation refactor we can layer in later without changing
//! results. This phase is read-only — `SELECT` works against real databases;
//! writes arrive in Phase 6/7.

pub mod eval;
pub mod func;

use crate::btree::TableCursor;
use crate::error::{Error, Result};
use crate::format::record::decode_record;
use crate::pager::Pager;
use crate::schema::Schema;
use crate::sql::ast::*;
use crate::sql::{self};
use crate::value::Value;
use crate::vfs::{File, OpenFlags, Vfs};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use eval::{ColumnInfo, EvalCtx, Params};

/// The result of a query: column labels and the materialized rows.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Result column labels, in order.
    pub columns: Vec<String>,
    /// Result rows, each with one value per column.
    pub rows: Vec<Vec<Value>>,
}

/// A read-only database connection.
pub struct Connection {
    pager: Pager,
    schema: Schema,
}

impl Connection {
    /// Open a connection over an already-opened file.
    pub fn from_file(file: Box<dyn File>) -> Result<Connection> {
        let pager = Pager::open(file)?;
        let schema = Schema::read(&pager)?;
        Ok(Connection { pager, schema })
    }

    /// Open a read-only connection to `path` using the given VFS.
    pub fn open_vfs(vfs: &dyn Vfs, path: &str) -> Result<Connection> {
        let file = vfs.open(path, OpenFlags::READ_ONLY)?;
        Connection::from_file(file)
    }

    /// Open a read-only connection to a file on disk (requires `std`).
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    pub fn open(path: &str) -> Result<Connection> {
        let vfs = crate::vfs::std_file::StdVfs::new();
        Connection::open_vfs(&vfs, path)
    }

    /// The schema catalog.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Run a single `SELECT` and return all rows. Non-`SELECT` statements are
    /// not yet supported (writes land in Phase 6/7).
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        self.query_params(sql, &Params::default())
    }

    /// Like [`query`](Self::query) but with bound parameters.
    pub fn query_params(&self, sql: &str, params: &Params) -> Result<QueryResult> {
        match sql::parse_one(sql)? {
            Statement::Select(sel) => self.run_select(&sel, params),
            _ => Err(Error::Unsupported("only SELECT is supported in this build")),
        }
    }

    // ---- SELECT execution ---------------------------------------------------

    fn run_select(&self, sel: &Select, params: &Params) -> Result<QueryResult> {
        let (columns, input_rows) = self.scan_source(sel)?;

        // Apply WHERE.
        let mut rows: Vec<InputRow> = Vec::new();
        for r in input_rows {
            if let Some(pred) = &sel.where_clause {
                let ctx = r.ctx(&columns, params);
                if eval::truth(&eval::eval(pred, &ctx)?) != Some(true) {
                    continue;
                }
            }
            rows.push(r);
        }

        let aggregated = !sel.group_by.is_empty() || self.has_aggregate(sel);
        let (out_labels, mut out) = if aggregated {
            self.eval_aggregated(sel, &columns, rows, params)?
        } else {
            self.eval_simple(sel, &columns, rows, params)?
        };

        // DISTINCT (dedupe on output values, preserving first occurrence).
        if sel.distinct {
            let mut seen: Vec<Vec<Value>> = Vec::new();
            out.retain(|row| {
                if seen.iter().any(|s| rows_equal(s, &row.values)) {
                    false
                } else {
                    seen.push(row.values.clone());
                    true
                }
            });
        }

        // ORDER BY.
        if !sel.order_by.is_empty() {
            // Stable sort by the precomputed sort keys.
            out.sort_by(|a, b| {
                for (i, term) in sel.order_by.iter().enumerate() {
                    let ord = eval::compare(&a.sort_keys[i], &b.sort_keys[i]);
                    let ord = if term.descending { ord.reverse() } else { ord };
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }

        // OFFSET / LIMIT.
        let offset = match &sel.offset {
            Some(e) => eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?).max(0) as usize,
            None => 0,
        };
        let limit = match &sel.limit {
            Some(e) => {
                Some(eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?).max(0) as usize)
            }
            None => None,
        };
        let mut final_rows: Vec<Vec<Value>> =
            out.into_iter().skip(offset).map(|r| r.values).collect();
        if let Some(n) = limit {
            final_rows.truncate(n);
        }

        Ok(QueryResult {
            columns: out_labels,
            rows: final_rows,
        })
    }

    /// Scan the `FROM` source into column metadata and decoded input rows.
    fn scan_source(&self, sel: &Select) -> Result<(Vec<ColumnInfo>, Vec<InputRow>)> {
        let Some(from) = &sel.from else {
            // No FROM: a single empty row (e.g. `SELECT 1+1`).
            return Ok((
                Vec::new(),
                alloc::vec![InputRow {
                    values: Vec::new(),
                    rowid: None
                }],
            ));
        };
        if !from.joins.is_empty() {
            return Err(Error::Unsupported("joins"));
        }

        let meta = self.table_meta(&from.first.name, from.first.alias.as_deref())?;
        let encoding = self.pager.header().text_encoding;
        let mut rows = Vec::new();
        let mut cur = TableCursor::new(&self.pager, meta.root);
        let mut ok = cur.first()?;
        while ok {
            let rowid = cur.rowid()?;
            let mut values = decode_record(&cur.payload()?, encoding)?;
            // Normalize column count to the schema (short records read as NULL).
            values.resize(meta.columns.len(), Value::Null);
            // Substitute the rowid for an INTEGER PRIMARY KEY column.
            if let Some(ipk) = meta.ipk {
                values[ipk] = Value::Integer(rowid);
            }
            rows.push(InputRow {
                values,
                rowid: Some(rowid),
            });
            ok = cur.next()?;
        }
        Ok((meta.columns, rows))
    }

    /// Non-aggregated projection: one output row per input row.
    fn eval_simple(
        &self,
        sel: &Select,
        columns: &[ColumnInfo],
        rows: Vec<InputRow>,
        params: &Params,
    ) -> Result<(Vec<String>, Vec<OutRow>)> {
        let labels = self.output_labels(sel, columns);
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let ctx = r.ctx(columns, params);
            let mut values = Vec::new();
            for col in &sel.columns {
                project_column(col, columns, &ctx, &mut values)?;
            }
            // ORDER BY: resolve by position/alias against the output, else
            // evaluate against the input row (allows ordering by unselected cols).
            let mut sort_keys = Vec::new();
            for term in &sel.order_by {
                match resolve_order_index(&term.expr, &labels, values.len()) {
                    Some(idx) => sort_keys.push(values[idx].clone()),
                    None => sort_keys.push(eval::eval(&term.expr, &ctx)?),
                }
            }
            out.push(OutRow { values, sort_keys });
        }
        Ok((labels, out))
    }

    /// Aggregated/grouped projection.
    fn eval_aggregated(
        &self,
        sel: &Select,
        columns: &[ColumnInfo],
        rows: Vec<InputRow>,
        params: &Params,
    ) -> Result<(Vec<String>, Vec<OutRow>)> {
        // Wildcards make no sense with aggregation unless trivially grouped; keep
        // it simple and reject for now.
        if sel
            .columns
            .iter()
            .any(|c| matches!(c, ResultColumn::Wildcard | ResultColumn::TableWildcard(_)))
        {
            return Err(Error::Unsupported("'*' with aggregation"));
        }

        // Partition rows into groups (first-seen order).
        let mut group_keys: Vec<Vec<Value>> = Vec::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let ctx = r.ctx(columns, params);
            let mut key = Vec::new();
            for g in &sel.group_by {
                key.push(eval::eval(g, &ctx)?);
            }
            match group_keys.iter().position(|k| rows_equal(k, &key)) {
                Some(idx) => groups[idx].push(i),
                None => {
                    group_keys.push(key);
                    groups.push(alloc::vec![i]);
                }
            }
        }
        // No GROUP BY but aggregates present => a single group over all rows
        // (which yields one row even when there are zero input rows).
        if sel.group_by.is_empty() {
            groups = alloc::vec![(0..rows.len()).collect()];
        }

        let labels = self.output_labels(sel, columns);
        let mut out = Vec::new();
        for group in &groups {
            // Representative row context for bare column references.
            let repr = group.first().map(|&i| &rows[i]);
            let empty = InputRow {
                values: alloc::vec![Value::Null; columns.len()],
                rowid: None,
            };
            let repr_ctx = repr.unwrap_or(&empty).ctx(columns, params);

            // Compute the output row, substituting aggregate calls with values.
            let mut values = Vec::new();
            for col in &sel.columns {
                let ResultColumn::Expr { expr, .. } = col else {
                    unreachable!("wildcards rejected above")
                };
                let substituted =
                    self.substitute_aggregates(expr, columns, &rows, group, params)?;
                values.push(eval::eval(&substituted, &repr_ctx)?);
            }

            // HAVING (aggregate-aware).
            if let Some(having) = &sel.having {
                let h = self.substitute_aggregates(having, columns, &rows, group, params)?;
                if eval::truth(&eval::eval(&h, &repr_ctx)?) != Some(true) {
                    continue;
                }
            }

            // Sort keys (aggregate-aware) for ORDER BY.
            let mut sort_keys = Vec::new();
            for term in &sel.order_by {
                if let Some(idx) = resolve_order_index(&term.expr, &labels, values.len()) {
                    sort_keys.push(values[idx].clone());
                } else {
                    let s =
                        self.substitute_aggregates(&term.expr, columns, &rows, group, params)?;
                    sort_keys.push(eval::eval(&s, &repr_ctx)?);
                }
            }
            out.push(OutRow { values, sort_keys });
        }
        Ok((labels, out))
    }

    /// Replace aggregate function calls in `expr` with their computed values for
    /// the given group, returning an aggregate-free expression.
    fn substitute_aggregates(
        &self,
        expr: &Expr,
        columns: &[ColumnInfo],
        rows: &[InputRow],
        group: &[usize],
        params: &Params,
    ) -> Result<Expr> {
        Ok(match expr {
            Expr::Function {
                name,
                distinct,
                args,
                star,
            } if func::is_aggregate(name) => {
                let v = self.compute_aggregate(
                    name, *distinct, args, *star, columns, rows, group, params,
                )?;
                Expr::Literal(value_to_literal(v))
            }
            Expr::Function {
                name,
                distinct,
                args,
                star,
            } => {
                let mut new_args = Vec::with_capacity(args.len());
                for a in args {
                    new_args.push(self.substitute_aggregates(a, columns, rows, group, params)?);
                }
                Expr::Function {
                    name: name.clone(),
                    distinct: *distinct,
                    args: new_args,
                    star: *star,
                }
            }
            Expr::Binary { op, left, right } => Expr::Binary {
                op: *op,
                left: Box::new(self.substitute_aggregates(left, columns, rows, group, params)?),
                right: Box::new(self.substitute_aggregates(right, columns, rows, group, params)?),
            },
            Expr::Unary { op, expr } => Expr::Unary {
                op: *op,
                expr: Box::new(self.substitute_aggregates(expr, columns, rows, group, params)?),
            },
            Expr::Paren(e) => Expr::Paren(Box::new(
                self.substitute_aggregates(e, columns, rows, group, params)?,
            )),
            other => other.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_aggregate(
        &self,
        name: &str,
        distinct: bool,
        args: &[Expr],
        star: bool,
        columns: &[ColumnInfo],
        rows: &[InputRow],
        group: &[usize],
        params: &Params,
    ) -> Result<Value> {
        let lname = name.to_ascii_lowercase();

        // Gather the (non-NULL for most) argument values across the group.
        let mut vals: Vec<Value> = Vec::new();
        let mut count_rows = 0usize; // for count(*)
        for &i in group {
            count_rows += 1;
            if star {
                continue;
            }
            let ctx = rows[i].ctx(columns, params);
            let v = eval::eval(&args[0], &ctx)?;
            if !matches!(v, Value::Null) {
                vals.push(v);
            }
        }
        if distinct {
            dedup_values(&mut vals);
        }

        Ok(match lname.as_str() {
            "count" => {
                if star {
                    Value::Integer(count_rows as i64)
                } else {
                    Value::Integer(vals.len() as i64)
                }
            }
            "sum" => {
                if vals.is_empty() {
                    Value::Null
                } else if vals.iter().all(|v| matches!(v, Value::Integer(_))) {
                    let mut acc: i64 = 0;
                    let mut overflow = false;
                    for v in &vals {
                        if let Value::Integer(i) = v {
                            match acc.checked_add(*i) {
                                Some(s) => acc = s,
                                None => {
                                    overflow = true;
                                    break;
                                }
                            }
                        }
                    }
                    if overflow {
                        Value::Real(vals.iter().map(eval::to_f64).sum())
                    } else {
                        Value::Integer(acc)
                    }
                } else {
                    Value::Real(vals.iter().map(eval::to_f64).sum())
                }
            }
            "total" => Value::Real(vals.iter().map(eval::to_f64).sum()),
            "avg" => {
                if vals.is_empty() {
                    Value::Null
                } else {
                    let sum: f64 = vals.iter().map(eval::to_f64).sum();
                    Value::Real(sum / vals.len() as f64)
                }
            }
            "min" => vals
                .into_iter()
                .reduce(|a, b| {
                    if eval::compare(&b, &a) == core::cmp::Ordering::Less {
                        b
                    } else {
                        a
                    }
                })
                .unwrap_or(Value::Null),
            "max" => vals
                .into_iter()
                .reduce(|a, b| {
                    if eval::compare(&b, &a) == core::cmp::Ordering::Greater {
                        b
                    } else {
                        a
                    }
                })
                .unwrap_or(Value::Null),
            "group_concat" => {
                if vals.is_empty() {
                    Value::Null
                } else {
                    let sep = if args.len() >= 2 {
                        let ctx = EvalCtx::rowless(params);
                        eval::to_text(&eval::eval(&args[1], &ctx)?)
                    } else {
                        ",".to_string()
                    };
                    let parts: Vec<String> = vals.iter().map(eval::to_text).collect();
                    Value::Text(parts.join(&sep))
                }
            }
            _ => return Err(Error::Unsupported("aggregate function")),
        })
    }

    fn has_aggregate(&self, sel: &Select) -> bool {
        sel.columns.iter().any(|c| match c {
            ResultColumn::Expr { expr, .. } => contains_aggregate(expr),
            _ => false,
        }) || sel.having.as_ref().is_some_and(contains_aggregate)
    }

    fn output_labels(&self, sel: &Select, columns: &[ColumnInfo]) -> Vec<String> {
        let mut labels = Vec::new();
        for col in &sel.columns {
            match col {
                ResultColumn::Wildcard | ResultColumn::TableWildcard(_) => {
                    for c in columns {
                        labels.push(c.name.clone());
                    }
                }
                ResultColumn::Expr { expr, alias } => {
                    labels.push(alias.clone().unwrap_or_else(|| expr_label(expr)));
                }
            }
        }
        labels
    }

    fn table_meta(&self, name: &str, alias: Option<&str>) -> Result<TableMeta> {
        let obj = self
            .schema
            .table(name)
            .ok_or_else(|| Error::Error(alloc::format!("no such table: {name}")))?;
        let sql = obj
            .sql
            .as_ref()
            .ok_or_else(|| Error::Corrupt("table has no CREATE statement".into()))?;
        let Statement::CreateTable(ct) = sql::parse_one(sql)? else {
            return Err(Error::Corrupt("schema sql is not CREATE TABLE".into()));
        };
        if ct.without_rowid {
            return Err(Error::Unsupported("WITHOUT ROWID tables"));
        }
        let table_label = alias.unwrap_or(name).to_string();
        let columns: Vec<ColumnInfo> = ct
            .columns
            .iter()
            .map(|c| ColumnInfo {
                name: c.name.clone(),
                table: table_label.clone(),
            })
            .collect();
        let ipk = find_integer_primary_key(&ct);
        Ok(TableMeta {
            root: obj.rootpage,
            columns,
            ipk,
        })
    }
}

struct TableMeta {
    root: u32,
    columns: Vec<ColumnInfo>,
    ipk: Option<usize>,
}

struct InputRow {
    values: Vec<Value>,
    rowid: Option<i64>,
}

impl InputRow {
    fn ctx<'a>(&'a self, columns: &'a [ColumnInfo], params: &'a Params) -> EvalCtx<'a> {
        EvalCtx {
            row: &self.values,
            columns,
            rowid: self.rowid,
            params,
            anon_counter: core::cell::Cell::new(0),
        }
    }
}

struct OutRow {
    values: Vec<Value>,
    sort_keys: Vec<Value>,
}

fn project_column(
    col: &ResultColumn,
    columns: &[ColumnInfo],
    ctx: &EvalCtx,
    out: &mut Vec<Value>,
) -> Result<()> {
    match col {
        ResultColumn::Wildcard => {
            for v in ctx.row {
                out.push(v.clone());
            }
        }
        ResultColumn::TableWildcard(table) => {
            for (i, c) in columns.iter().enumerate() {
                if c.table.eq_ignore_ascii_case(table) {
                    out.push(ctx.row[i].clone());
                }
            }
        }
        ResultColumn::Expr { expr, .. } => {
            out.push(eval::eval(expr, ctx)?);
        }
    }
    Ok(())
}

/// Resolve an `ORDER BY` term to an output-column index when it refers to one:
/// a positive integer literal `N` (1-based position), or a bare column name that
/// matches a result-column label/alias. Returns `None` for general expressions,
/// which are evaluated against the row instead.
fn resolve_order_index(expr: &Expr, labels: &[String], ncols: usize) -> Option<usize> {
    match expr {
        Expr::Literal(Literal::Integer(n)) => {
            let idx = (*n as usize).checked_sub(1)?;
            (idx < ncols).then_some(idx)
        }
        Expr::Column {
            table: None,
            column,
        } => labels.iter().position(|l| l.eq_ignore_ascii_case(column)),
        _ => None,
    }
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, args, .. } => {
            func::is_aggregate(name) || args.iter().any(contains_aggregate)
        }
        Expr::Binary { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::Unary { expr, .. } | Expr::Paren(expr) => contains_aggregate(expr),
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || when_then
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_result.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        _ => false,
    }
}

fn rows_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| eval::compare(x, y) == core::cmp::Ordering::Equal)
}

fn dedup_values(vals: &mut Vec<Value>) {
    let mut seen: Vec<Value> = Vec::new();
    vals.retain(|v| {
        if seen
            .iter()
            .any(|s| eval::compare(s, v) == core::cmp::Ordering::Equal)
        {
            false
        } else {
            seen.push(v.clone());
            true
        }
    });
}

fn value_to_literal(v: Value) -> Literal {
    match v {
        Value::Null => Literal::Null,
        Value::Integer(i) => Literal::Integer(i),
        Value::Real(r) => Literal::Real(r),
        Value::Text(s) => Literal::Str(s),
        Value::Blob(b) => Literal::Blob(b),
    }
}

/// Best-effort label for an unaliased result expression.
fn expr_label(expr: &Expr) -> String {
    match expr {
        Expr::Column { column, .. } => column.clone(),
        Expr::Literal(Literal::Integer(i)) => i.to_string(),
        Expr::Literal(Literal::Str(s)) => s.clone(),
        Expr::Function { name, .. } => name.clone(),
        Expr::Paren(e) => expr_label(e),
        _ => "expr".to_string(),
    }
}

/// Detect an `INTEGER PRIMARY KEY` rowid alias column (must be declared exactly
/// `INTEGER`, per SQLite — `INT PRIMARY KEY` does not alias the rowid).
fn find_integer_primary_key(ct: &CreateTable) -> Option<usize> {
    for (i, c) in ct.columns.iter().enumerate() {
        let is_integer = c
            .type_name
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("integer"));
        let is_pk = c
            .constraints
            .iter()
            .any(|k| matches!(k, ColumnConstraint::PrimaryKey { .. }));
        if is_integer && is_pk {
            return Some(i);
        }
    }
    // Table-level single-column PRIMARY KEY over an INTEGER column.
    for tc in &ct.constraints {
        if let TableConstraint::PrimaryKey(cols) = tc {
            if cols.len() == 1 {
                if let Some(i) = ct.columns.iter().position(|c| c.name == cols[0]) {
                    if ct.columns[i]
                        .type_name
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case("integer"))
                    {
                        return Some(i);
                    }
                }
            }
        }
    }
    None
}
