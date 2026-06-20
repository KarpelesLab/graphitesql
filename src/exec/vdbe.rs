//! A minimal register-machine (VDBE) IR and interpreter.
//!
//! This is the first concrete step of the Track B "executor → VDBE" migration: a
//! self-contained bytecode IR plus a compiler for constant `SELECT` projections
//! and an interpreter that runs them. It does **not** replace the tree-walking
//! executor — it runs alongside it so the IR can be grown incrementally (table
//! cursors, filters, joins) while the existing engine keeps serving queries.
//!
//! The design mirrors SQLite's VDBE shape (`vdbe.c`): a flat instruction array
//! over a register file driven by a program counter, where each op reads/writes
//! registers by index, `Goto`/`IfFalse` branch, and a `ResultRow` op emits a span
//! of registers as an output row. graphitesql's ops are a small, safe-Rust subset
//! covering constant `SELECT` projections (literals, arithmetic, concat,
//! comparison, three-valued boolean logic, `IS NULL`, `CASE`, `CAST`).

use crate::error::{Error, Result};
use crate::sql::ast::{BinaryOp, Expr, Literal, ResultColumn, Select};
use crate::value::Value;
use alloc::string::String;
use alloc::vec::Vec;

/// One VDBE instruction. Registers are addressed by index into the register file.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // field roles are described by each variant's doc comment
pub enum Op {
    /// Load an integer constant into `dest`.
    Integer { value: i64, dest: usize },
    /// Load a real constant into `dest`.
    Real { value: f64, dest: usize },
    /// Load a text constant into `dest`.
    Str { value: String, dest: usize },
    /// Load `NULL` into `dest`.
    Null { dest: usize },
    /// `dest = lhs <op> rhs` for an arithmetic `BinaryOp` (Add/Sub/Mul/Div/Mod).
    Arith {
        op: BinaryOp,
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// `dest = lhs || rhs` (text concatenation).
    Concat { lhs: usize, rhs: usize, dest: usize },
    /// `dest = lhs <op> rhs` for a comparison `BinaryOp` (Eq/NotEq/Lt/…), with
    /// SQLite's NULL-yields-NULL three-valued result (1/0/NULL).
    Compare {
        op: BinaryOp,
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// `dest = lhs AND rhs` (three-valued).
    And { lhs: usize, rhs: usize, dest: usize },
    /// `dest = lhs OR rhs` (three-valued).
    Or { lhs: usize, rhs: usize, dest: usize },
    /// `dest = NOT reg` (three-valued; NULL stays NULL).
    Not { reg: usize, dest: usize },
    /// `dest = reg IS [NOT] NULL` (1/0).
    IsNull {
        reg: usize,
        negated: bool,
        dest: usize,
    },
    /// Copy `src` into `dest`.
    Copy { src: usize, dest: usize },
    /// `dest = CAST(reg AS type_name)`.
    Cast {
        reg: usize,
        type_name: String,
        dest: usize,
    },
    /// Unconditional jump to instruction index `target`.
    Goto { target: usize },
    /// Jump to `target` when `reg` is false or NULL (i.e. not true).
    IfFalse { reg: usize, target: usize },
    /// Position the table cursor at the first row; jump to `target` (the loop
    /// exit) when the table is empty.
    Rewind { target: usize },
    /// Load column `col` of the cursor's current row into `dest`.
    Column { col: usize, dest: usize },
    /// Advance the cursor; jump back to `target` (the loop body) if a row remains,
    /// else fall through.
    Next { target: usize },
    /// Decrement `reg`; jump to `target` once it reaches zero (a `LIMIT` counter).
    DecrJumpZero { reg: usize, target: usize },
    /// If `reg` is positive, decrement it and jump to `target` (an `OFFSET` skip).
    IfPosDecr { reg: usize, target: usize },
    /// `dest = -reg` (numeric negation).
    Negate { reg: usize, dest: usize },
    /// Emit registers `[start, start+count)` as one output row.
    ResultRow { start: usize, count: usize },
    /// `DISTINCT` gate: if the row in `[start, start+count)` was already seen,
    /// jump to `target` (skip it); otherwise record it and fall through.
    DistinctCheck {
        start: usize,
        count: usize,
        target: usize,
    },
    /// Append a row to the sorter: the output values in `[row_start, row_start+
    /// row_count)` keyed by `[key_start, key_start+key_count)`.
    SorterInsert {
        row_start: usize,
        row_count: usize,
        key_start: usize,
        key_count: usize,
    },
    /// Sort the accumulated sorter rows by their keys (per `keys`, in order).
    SorterSort { keys: Vec<SortKey> },
    /// Position the sorter cursor at the first sorted row; jump to `target` (the
    /// emit-loop exit) when the sorter is empty.
    SorterRewind { target: usize },
    /// Load the current sorter row's stored output values into `[start, start+
    /// count)`.
    SorterRow { start: usize, count: usize },
    /// Advance the sorter cursor; jump back to `target` if a row remains.
    SorterNext { target: usize },
    /// Fold the current row into aggregate `slot`: for `CountStar` bump the row
    /// counter, otherwise collect `arg` (when non-NULL).
    AggStep {
        slot: usize,
        kind: AggKind,
        arg: Option<usize>,
    },
    /// Finalize aggregate `slot` into `dest`.
    AggFinal {
        slot: usize,
        kind: AggKind,
        dest: usize,
    },
    /// Stop execution.
    Halt,
}

/// The aggregate functions the VDBE can fold over a single-table scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum AggKind {
    CountStar,
    Count,
    Sum,
    Total,
    Avg,
    Min,
    Max,
    GroupConcat,
}

/// One `ORDER BY` key for [`Op::SorterSort`]: direction and NULL placement
/// (collation is binary in the VDBE scan path).
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// `DESC` when true.
    pub descending: bool,
    /// Explicit `NULLS FIRST`/`LAST`; `None` uses SQLite's default.
    pub nulls_first: Option<bool>,
}

/// A compiled VDBE program: the instruction stream and the register-file size.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// The instruction stream.
    pub ops: Vec<Op>,
    /// Number of registers the program uses.
    pub n_registers: usize,
    /// Output column labels (parallel to each `ResultRow`'s register span).
    pub columns: Vec<String>,
}

/// Compile a constant-projection `SELECT` (no `FROM`/`WHERE`/aggregates) into a
/// program. Returns `Unsupported` for anything outside the spike's grammar so the
/// caller can fall back to the tree-walking executor.
pub fn compile_const_select(sel: &Select) -> Result<Program> {
    if sel.from.is_some()
        || sel.where_clause.is_some()
        || !sel.group_by.is_empty()
        || !sel.compound.is_empty()
        || !sel.order_by.is_empty()
        || sel.limit.is_some()
        || sel.offset.is_some()
        || sel.distinct
    {
        return Err(Error::Unsupported("VDBE spike: only constant SELECT lists"));
    }
    if sel.columns.is_empty() {
        return Err(Error::Unsupported("VDBE spike: empty SELECT list"));
    }
    // Reserve a contiguous output block [0, n) for the result registers, then
    // compile each projection straight into its slot (scratch registers for
    // sub-expressions are allocated above the output block).
    let count = sel.columns.len();
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: Vec::new(),
    };
    let mut columns = Vec::new();
    for (i, rc) in sel.columns.iter().enumerate() {
        let ResultColumn::Expr { expr, alias } = rc else {
            return Err(Error::Unsupported("VDBE spike: only scalar result columns"));
        };
        c.compile_expr_into(expr, i)?;
        columns.push(
            alias
                .clone()
                .unwrap_or_else(|| alloc::format!("col{}", i + 1)),
        );
    }
    c.ops.push(Op::ResultRow { start: 0, count });
    c.ops.push(Op::Halt);
    Ok(Program {
        ops: c.ops,
        n_registers: c.next_reg,
        columns,
    })
}

/// Is `expr` a (top-level) aggregate function call the VDBE can fold?
fn is_aggregate_expr(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Function { name, args, star, .. }
            if crate::exec::func::is_aggregate_call(name, args.len(), *star)
    )
}

/// Map a 1-arg-or-star aggregate call to its [`AggKind`] (binding the argument
/// register expression). Returns `None` for unsupported call shapes.
fn agg_kind(expr: &Expr) -> Option<(AggKind, Option<Expr>)> {
    let Expr::Function {
        name,
        distinct,
        args,
        star,
        filter,
        order_by,
        over,
    } = expr
    else {
        return None;
    };
    // Plain aggregates only: no DISTINCT/FILTER/ORDER BY/OVER in the VDBE path.
    if *distinct || filter.is_some() || !order_by.is_empty() || over.is_some() {
        return None;
    }
    let arg = args.first().cloned();
    let kind = match name.to_ascii_lowercase().as_str() {
        "count" if *star => return Some((AggKind::CountStar, None)),
        "count" if args.len() == 1 => AggKind::Count,
        "sum" if args.len() == 1 => AggKind::Sum,
        "total" if args.len() == 1 => AggKind::Total,
        "avg" if args.len() == 1 => AggKind::Avg,
        "min" if args.len() == 1 => AggKind::Min,
        "max" if args.len() == 1 => AggKind::Max,
        "group_concat" if args.len() == 1 => AggKind::GroupConcat,
        _ => return None,
    };
    Some((kind, arg))
}

/// Compile `SELECT <aggregates> FROM <table> [WHERE …]` (no GROUP BY): the scan
/// folds every aggregate slot, then a single `ResultRow` emits the finalized
/// values. Returns `Unsupported` for shapes outside this grammar (so the caller
/// falls back); `ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT` on an aggregate query are
/// left to the tree-walker.
fn compile_aggregate_select(
    sel: &Select,
    columns: &[String],
    projections: &[(Expr, String)],
) -> Result<Program> {
    if !sel.order_by.is_empty() || sel.limit.is_some() || sel.offset.is_some() || sel.distinct {
        return Err(Error::Unsupported("VDBE: bare aggregate only"));
    }
    // Every projection must be exactly one supported aggregate call.
    let mut slots: Vec<(AggKind, Option<Expr>)> = Vec::new();
    for (e, _) in projections {
        match agg_kind(e) {
            Some(spec) => slots.push(spec),
            None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
        }
    }
    let count = projections.len();
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: columns.to_vec(),
    };
    let rewind = c.ops.len();
    c.ops.push(Op::Rewind { target: 0 });
    let body = c.ops.len();
    let skip = match &sel.where_clause {
        Some(pred) => {
            let preg = c.compile_expr(pred)?;
            let at = c.ops.len();
            c.ops.push(Op::IfFalse {
                reg: preg,
                target: 0,
            });
            Some(at)
        }
        None => None,
    };
    for (slot, (kind, arg)) in slots.iter().enumerate() {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        c.ops.push(Op::AggStep {
            slot,
            kind: *kind,
            arg: arg_reg,
        });
    }
    let next = c.ops.len();
    c.ops.push(Op::Next { target: body });
    if let Some(at) = skip {
        if let Op::IfFalse { target, .. } = &mut c.ops[at] {
            *target = next;
        }
    }
    let end = c.ops.len();
    if let Op::Rewind { target } = &mut c.ops[rewind] {
        *target = end;
    }
    // Finalize each slot into its output register, then emit the single row.
    for (slot, (kind, _)) in slots.iter().enumerate() {
        c.ops.push(Op::AggFinal {
            slot,
            kind: *kind,
            dest: slot,
        });
    }
    c.ops.push(Op::ResultRow { start: 0, count });
    c.ops.push(Op::Halt);
    Ok(Program {
        ops: c.ops,
        n_registers: c.next_reg,
        columns: projections.iter().map(|(_, l)| l.clone()).collect(),
    })
}

/// Compile `SELECT <projection> FROM <single table>` (no `WHERE`/joins/aggregates/
/// `ORDER BY`) into a program that scans the table via cursor ops. `columns` are
/// the table's column names, used to resolve column references to indices.
/// Returns `Unsupported` outside this grammar so the caller can fall back.
pub fn compile_table_select(sel: &Select, columns: &[String]) -> Result<Program> {
    if !sel.group_by.is_empty() || !sel.compound.is_empty() {
        return Err(Error::Unsupported("VDBE: only plain table projections"));
    }
    // Expand the projection list to concrete expressions/labels (supporting `*`).
    let mut projections: Vec<(Expr, String)> = Vec::new();
    for rc in &sel.columns {
        match rc {
            ResultColumn::Wildcard => {
                for name in columns {
                    projections.push((
                        Expr::Column {
                            table: None,
                            column: name.clone(),
                        },
                        name.clone(),
                    ));
                }
            }
            ResultColumn::Expr { expr, alias } => {
                let label = alias.clone().unwrap_or_else(|| match expr {
                    Expr::Column { column, .. } => column.clone(),
                    _ => alloc::format!("col{}", projections.len() + 1),
                });
                projections.push((expr.clone(), label));
            }
            ResultColumn::TableWildcard(_) => {
                return Err(Error::Unsupported("VDBE: table.* not yet supported"))
            }
        }
    }
    if projections.is_empty() {
        return Err(Error::Unsupported("VDBE: empty projection"));
    }
    // An all-aggregate projection (no GROUP BY) folds the scan into one row.
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return compile_aggregate_select(sel, columns, &projections);
    }
    let count = projections.len();
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: columns.to_vec(),
    };
    // Optional LIMIT (constant integer only): a counter register decremented
    // after each emitted row, halting the loop at zero.
    let limit_reg = match &sel.limit {
        None => None,
        Some(Expr::Literal(Literal::Integer(n))) => {
            let r = c.alloc();
            c.ops.push(Op::Integer { value: *n, dest: r });
            Some(r)
        }
        Some(_) => return Err(Error::Unsupported("VDBE: only constant integer LIMIT")),
    };
    // Optional OFFSET (constant integer only): a counter decremented while it is
    // positive, skipping that many qualifying rows before any are emitted.
    let offset_reg = match &sel.offset {
        None => None,
        Some(Expr::Literal(Literal::Integer(n))) => {
            let r = c.alloc();
            c.ops.push(Op::Integer { value: *n, dest: r });
            Some(r)
        }
        Some(_) => return Err(Error::Unsupported("VDBE: only constant integer OFFSET")),
    };
    // With `ORDER BY`, the scan feeds a sorter and `LIMIT`/`OFFSET` apply to the
    // sorted emit loop instead of to the scan itself.
    let ordering = !sel.order_by.is_empty();
    // Reserve contiguous sort-key registers (one per `ORDER BY` term). A bare
    // positive integer term is an output-column ordinal (1-based).
    let mut key_specs: Vec<(Expr, SortKey)> = Vec::new();
    if ordering {
        for term in &sel.order_by {
            let expr = match &term.expr {
                // A bare positive integer is a 1-based output-column ordinal.
                Expr::Literal(Literal::Integer(k)) if *k >= 1 && (*k as usize) <= count => {
                    projections[*k as usize - 1].0.clone()
                }
                // A bare name that is an output alias (and not a table column)
                // refers to that projection.
                Expr::Column {
                    table: None,
                    column,
                } if !columns.iter().any(|c| c.eq_ignore_ascii_case(column))
                    && projections
                        .iter()
                        .any(|(_, l)| l.eq_ignore_ascii_case(column)) =>
                {
                    projections
                        .iter()
                        .find(|(_, l)| l.eq_ignore_ascii_case(column))
                        .map(|(e, _)| e.clone())
                        .unwrap()
                }
                other => other.clone(),
            };
            key_specs.push((
                expr,
                SortKey {
                    descending: term.descending,
                    nulls_first: term.nulls_first,
                },
            ));
        }
    }
    let key_start = c.next_reg;
    for _ in &key_specs {
        c.alloc();
    }

    // Rewind (target backpatched to the loop exit), then the loop body.
    let rewind = c.ops.len();
    c.ops.push(Op::Rewind { target: 0 });
    // `LIMIT 0` (no ordering) emits nothing: skip the whole scan loop when the
    // counter starts at 0. (With ordering the counter is consumed in the emit
    // loop, so the scan must still run to populate the sorter.)
    let limit_skip = if ordering {
        None
    } else {
        limit_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::IfFalse { reg: r, target: 0 });
            at
        })
    };
    let body = c.ops.len();
    // Optional WHERE: skip the row (jump to Next) when the predicate is not true.
    let skip = match &sel.where_clause {
        Some(pred) => {
            let preg = c.compile_expr(pred)?;
            let at = c.ops.len();
            c.ops.push(Op::IfFalse {
                reg: preg,
                target: 0,
            });
            Some(at)
        }
        None => None,
    };
    // Compute the projected row first: `DISTINCT` and (non-ordering) `OFFSET` both
    // gate on it, and `DISTINCT` must run before `OFFSET`/`LIMIT`.
    for (i, (expr, _)) in projections.iter().enumerate() {
        c.compile_expr_into(expr, i)?;
    }
    // Optional DISTINCT: skip the row (jump to Next) when an equal one was emitted.
    let distinct_skip = if sel.distinct {
        let at = c.ops.len();
        c.ops.push(Op::DistinctCheck {
            start: 0,
            count,
            target: 0,
        });
        Some(at)
    } else {
        None
    };
    // Scan-loop OFFSET skip (only when not ordering; otherwise OFFSET is applied
    // to the sorted output).
    let offset_skip = if ordering {
        None
    } else {
        offset_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::IfPosDecr { reg: r, target: 0 });
            at
        })
    };
    let mut limit_done = None;
    if ordering {
        // Stage the projected row and its keys into the sorter; emission happens
        // after the scan completes.
        for (j, (expr, _)) in key_specs.iter().enumerate() {
            c.compile_expr_into(expr, key_start + j)?;
        }
        c.ops.push(Op::SorterInsert {
            row_start: 0,
            row_count: count,
            key_start,
            key_count: key_specs.len(),
        });
    } else {
        c.ops.push(Op::ResultRow { start: 0, count });
        // After emitting a row, decrement the LIMIT counter and stop at zero.
        if let Some(r) = limit_reg {
            limit_done = Some(c.ops.len());
            c.ops.push(Op::DecrJumpZero { reg: r, target: 0 });
        }
    }
    let next = c.ops.len();
    c.ops.push(Op::Next { target: body });
    if let Some(at) = skip {
        if let Op::IfFalse { target, .. } = &mut c.ops[at] {
            *target = next; // a filtered-out row advances to the next
        }
    }
    if let Some(at) = offset_skip {
        if let Op::IfPosDecr { target, .. } = &mut c.ops[at] {
            *target = next; // a skipped (offset) row advances to the next
        }
    }
    if let Some(at) = distinct_skip {
        if let Op::DistinctCheck { target, .. } = &mut c.ops[at] {
            *target = next; // a duplicate row advances to the next
        }
    }
    let end = c.ops.len();
    if let Op::Rewind { target } = &mut c.ops[rewind] {
        *target = end;
    }
    if let Some(at) = limit_skip {
        if let Op::IfFalse { target, .. } = &mut c.ops[at] {
            *target = end;
        }
    }
    if let Some(at) = limit_done {
        if let Op::DecrJumpZero { target, .. } = &mut c.ops[at] {
            *target = end;
        }
    }
    // Sorted emit loop: sort, then walk the sorter applying OFFSET then LIMIT.
    if ordering {
        c.ops.push(Op::SorterSort {
            keys: key_specs.iter().map(|(_, k)| k.clone()).collect(),
        });
        let srewind = c.ops.len();
        c.ops.push(Op::SorterRewind { target: 0 });
        let ebody = c.ops.len();
        let eoffset = offset_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::IfPosDecr { reg: r, target: 0 });
            at
        });
        c.ops.push(Op::SorterRow { start: 0, count });
        c.ops.push(Op::ResultRow { start: 0, count });
        let elimit = limit_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::DecrJumpZero { reg: r, target: 0 });
            at
        });
        let snext = c.ops.len();
        c.ops.push(Op::SorterNext { target: ebody });
        let eend = c.ops.len();
        if let Op::SorterRewind { target } = &mut c.ops[srewind] {
            *target = eend;
        }
        if let Some(at) = eoffset {
            if let Op::IfPosDecr { target, .. } = &mut c.ops[at] {
                *target = snext; // a skipped (offset) row advances to the next
            }
        }
        if let Some(at) = elimit {
            if let Op::DecrJumpZero { target, .. } = &mut c.ops[at] {
                *target = eend;
            }
        }
    }
    c.ops.push(Op::Halt);
    Ok(Program {
        ops: c.ops,
        n_registers: c.next_reg,
        columns: projections.into_iter().map(|(_, l)| l).collect(),
    })
}

struct Compiler {
    ops: Vec<Op>,
    next_reg: usize,
    /// Table column names, for resolving `Expr::Column` to a `Column` op.
    columns: Vec<String>,
}

impl Compiler {
    fn alloc(&mut self) -> usize {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    /// Compile `expr` into a freshly allocated register, returning its index.
    fn compile_expr(&mut self, expr: &Expr) -> Result<usize> {
        let dest = self.alloc();
        self.compile_expr_into(expr, dest)?;
        Ok(dest)
    }

    /// Compile `expr` so its value lands in register `dest`.
    fn compile_expr_into(&mut self, expr: &Expr, dest: usize) -> Result<()> {
        match expr {
            Expr::Literal(l) => {
                let op = match l {
                    Literal::Integer(i) => Op::Integer { value: *i, dest },
                    Literal::Real(r) => Op::Real { value: *r, dest },
                    Literal::Str(s) => Op::Str {
                        value: s.clone(),
                        dest,
                    },
                    Literal::Null => Op::Null { dest },
                    Literal::Boolean(b) => Op::Integer {
                        value: *b as i64,
                        dest,
                    },
                    Literal::Blob(_) => {
                        return Err(Error::Unsupported("VDBE spike: blob literals"))
                    }
                };
                self.ops.push(op);
                Ok(())
            }
            Expr::Column { column, .. } => {
                let idx = self
                    .columns
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(column))
                    .ok_or_else(|| Error::Error(alloc::format!("no such column: {column}")))?;
                self.ops.push(Op::Column { col: idx, dest });
                Ok(())
            }
            Expr::Paren(inner) => self.compile_expr_into(inner, dest),
            Expr::Unary {
                op: crate::sql::ast::UnaryOp::Negate,
                expr: inner,
            } => {
                let r = self.compile_expr(inner)?;
                self.ops.push(Op::Negate { reg: r, dest });
                Ok(())
            }
            Expr::Unary {
                op: crate::sql::ast::UnaryOp::Not,
                expr: inner,
            } => {
                let r = self.compile_expr(inner)?;
                self.ops.push(Op::Not { reg: r, dest });
                Ok(())
            }
            Expr::IsNull {
                expr: inner,
                negated,
            } => {
                let r = self.compile_expr(inner)?;
                self.ops.push(Op::IsNull {
                    reg: r,
                    negated: *negated,
                    dest,
                });
                Ok(())
            }
            Expr::Binary { op, left, right } => {
                let l = self.compile_expr(left)?;
                let r = self.compile_expr(right)?;
                use BinaryOp::*;
                match op {
                    Add | Sub | Mul | Div | Mod => {
                        self.ops.push(Op::Arith {
                            op: *op,
                            lhs: l,
                            rhs: r,
                            dest,
                        });
                        Ok(())
                    }
                    Concat => {
                        self.ops.push(Op::Concat {
                            lhs: l,
                            rhs: r,
                            dest,
                        });
                        Ok(())
                    }
                    Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                        self.ops.push(Op::Compare {
                            op: *op,
                            lhs: l,
                            rhs: r,
                            dest,
                        });
                        Ok(())
                    }
                    And => {
                        self.ops.push(Op::And {
                            lhs: l,
                            rhs: r,
                            dest,
                        });
                        Ok(())
                    }
                    Or => {
                        self.ops.push(Op::Or {
                            lhs: l,
                            rhs: r,
                            dest,
                        });
                        Ok(())
                    }
                    _ => Err(Error::Unsupported("VDBE spike: this operator")),
                }
            }
            Expr::Cast {
                expr: inner,
                type_name,
            } => {
                let r = self.compile_expr(inner)?;
                self.ops.push(Op::Cast {
                    reg: r,
                    type_name: type_name.clone(),
                    dest,
                });
                Ok(())
            }
            Expr::Case {
                operand,
                when_then,
                else_result,
            } => self.compile_case(operand.as_deref(), when_then, else_result.as_deref(), dest),
            _ => Err(Error::Unsupported("VDBE spike: this expression")),
        }
    }

    /// Compile a `CASE` expression using conditional jumps. Each `WHEN` tests its
    /// condition (`= operand` when a `CASE operand` form), jumps over its `THEN`
    /// on failure, and the matched `THEN` (or `ELSE`/NULL) lands in `dest` before
    /// jumping to the end.
    fn compile_case(
        &mut self,
        operand: Option<&Expr>,
        when_then: &[(Expr, Expr)],
        else_result: Option<&Expr>,
        dest: usize,
    ) -> Result<()> {
        // Register holding the CASE operand (for the `CASE x WHEN v` form).
        let operand_reg = match operand {
            Some(o) => Some(self.compile_expr(o)?),
            None => None,
        };
        let mut end_jumps = Vec::new();
        for (when, then) in when_then {
            // Compute the branch condition into a register.
            let cond = match operand_reg {
                Some(oreg) => {
                    let wreg = self.compile_expr(when)?;
                    let c = self.alloc();
                    self.ops.push(Op::Compare {
                        op: BinaryOp::Eq,
                        lhs: oreg,
                        rhs: wreg,
                        dest: c,
                    });
                    c
                }
                None => self.compile_expr(when)?,
            };
            // If the condition is not true, skip this THEN (target backpatched).
            let skip = self.ops.len();
            self.ops.push(Op::IfFalse {
                reg: cond,
                target: 0,
            });
            self.compile_expr_into(then, dest)?;
            end_jumps.push(self.ops.len());
            self.ops.push(Op::Goto { target: 0 });
            // Backpatch the skip to here (the next WHEN / ELSE).
            let here = self.ops.len();
            if let Op::IfFalse { target, .. } = &mut self.ops[skip] {
                *target = here;
            }
        }
        // ELSE (or NULL when absent).
        match else_result {
            Some(e) => self.compile_expr_into(e, dest)?,
            None => self.ops.push(Op::Null { dest }),
        }
        // Backpatch every THEN's exit jump to the instruction after the CASE.
        let end = self.ops.len();
        for j in end_jumps {
            if let Op::Goto { target } = &mut self.ops[j] {
                *target = end;
            }
        }
        Ok(())
    }
}

/// Run a compiled constant program (no table cursor), returning its result rows.
pub fn run(program: &Program) -> Result<Vec<Vec<Value>>> {
    run_rows(program, &[])
}

/// Run a compiled program over `table_rows` (the materialized rows of the single
/// table the program scans, if any). A program counter walks the instruction
/// array so jumps and the `Rewind`/`Next` loop can branch; `Column` reads from
/// the cursor's current row.
pub fn run_rows(program: &Program, table_rows: &[Vec<Value>]) -> Result<Vec<Vec<Value>>> {
    let mut regs: Vec<Value> = alloc::vec![Value::Null; program.n_registers];
    let mut out = Vec::new();
    let mut cursor: usize = 0; // index of the current row
                               // The sorter holds `(keys, row)` pairs staged by `SorterInsert`, sorted in
                               // place by `SorterSort`, then walked by `SorterRewind`/`SorterNext`.
    let mut sorter: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    let mut scursor: usize = 0;
    // Rows already emitted under DISTINCT (NULLs compare equal here).
    let mut seen: Vec<Vec<Value>> = Vec::new();
    // Aggregate accumulators, one per slot: `(collected non-NULL values, row
    // count for count(*))`.
    let mut agg: Vec<(Vec<Value>, i64)> = Vec::new();
    let mut pc = 0usize;
    while pc < program.ops.len() {
        let op = &program.ops[pc];
        pc += 1;
        match op {
            Op::Rewind { target } => {
                cursor = 0;
                if table_rows.is_empty() {
                    pc = *target;
                }
            }
            Op::Column { col, dest } => {
                regs[*dest] = table_rows
                    .get(cursor)
                    .and_then(|r| r.get(*col))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            Op::Next { target } => {
                cursor += 1;
                if cursor < table_rows.len() {
                    pc = *target;
                }
            }
            Op::DecrJumpZero { reg, target } => {
                let n = match &regs[*reg] {
                    Value::Integer(i) => *i,
                    other => crate::exec::eval::to_i64(other),
                };
                regs[*reg] = Value::Integer(n - 1);
                if n - 1 <= 0 {
                    pc = *target;
                }
            }
            Op::IfPosDecr { reg, target } => {
                let n = match &regs[*reg] {
                    Value::Integer(i) => *i,
                    other => crate::exec::eval::to_i64(other),
                };
                if n > 0 {
                    regs[*reg] = Value::Integer(n - 1);
                    pc = *target;
                }
            }
            Op::Goto { target } => {
                pc = *target;
            }
            Op::IfFalse { reg, target } => {
                if crate::exec::eval::truth(&regs[*reg]) != Some(true) {
                    pc = *target;
                }
            }
            Op::Copy { src, dest } => regs[*dest] = regs[*src].clone(),
            Op::Cast {
                reg,
                type_name,
                dest,
            } => {
                regs[*dest] = crate::exec::eval::cast(regs[*reg].clone(), type_name);
            }
            Op::Integer { value, dest } => regs[*dest] = Value::Integer(*value),
            Op::Real { value, dest } => regs[*dest] = Value::Real(*value),
            Op::Str { value, dest } => regs[*dest] = Value::Text(value.clone()),
            Op::Null { dest } => regs[*dest] = Value::Null,
            Op::Negate { reg, dest } => {
                regs[*dest] = match crate::exec::eval::to_number(&regs[*reg]) {
                    Value::Integer(i) => Value::Integer(i.wrapping_neg()),
                    Value::Real(r) => Value::Real(-r),
                    _ => Value::Null,
                };
            }
            Op::Arith { op, lhs, rhs, dest } => {
                regs[*dest] = crate::exec::eval::arithmetic_values(*op, &regs[*lhs], &regs[*rhs]);
            }
            Op::Concat { lhs, rhs, dest } => {
                regs[*dest] =
                    if matches!(regs[*lhs], Value::Null) || matches!(regs[*rhs], Value::Null) {
                        Value::Null
                    } else {
                        let mut s = crate::exec::eval::to_text(&regs[*lhs]);
                        s.push_str(&crate::exec::eval::to_text(&regs[*rhs]));
                        Value::Text(s)
                    };
            }
            Op::Compare { op, lhs, rhs, dest } => {
                regs[*dest] = crate::exec::eval::compare_op(
                    *op,
                    &regs[*lhs],
                    &regs[*rhs],
                    crate::value::Collation::Binary,
                );
            }
            Op::And { lhs, rhs, dest } => {
                regs[*dest] = three_valued_and(&regs[*lhs], &regs[*rhs]);
            }
            Op::Or { lhs, rhs, dest } => {
                regs[*dest] = three_valued_or(&regs[*lhs], &regs[*rhs]);
            }
            Op::Not { reg, dest } => {
                regs[*dest] = match crate::exec::eval::truth(&regs[*reg]) {
                    Some(b) => Value::Integer(!b as i64),
                    None => Value::Null,
                };
            }
            Op::IsNull { reg, negated, dest } => {
                let is_null = matches!(regs[*reg], Value::Null);
                regs[*dest] = Value::Integer((is_null != *negated) as i64);
            }
            Op::ResultRow { start, count } => {
                out.push(regs[*start..*start + *count].to_vec());
            }
            Op::DistinctCheck {
                start,
                count,
                target,
            } => {
                let row = &regs[*start..*start + *count];
                let dup = seen.iter().any(|prev| {
                    prev.len() == row.len() && prev.iter().zip(row).all(|(a, b)| distinct_eq(a, b))
                });
                if dup {
                    pc = *target;
                } else {
                    seen.push(row.to_vec());
                }
            }
            Op::SorterInsert {
                row_start,
                row_count,
                key_start,
                key_count,
            } => {
                let row = regs[*row_start..*row_start + *row_count].to_vec();
                let keys = regs[*key_start..*key_start + *key_count].to_vec();
                sorter.push((keys, row));
            }
            Op::SorterSort { keys } => {
                sorter.sort_by(|a, b| {
                    for (i, k) in keys.iter().enumerate() {
                        let ord = crate::exec::cmp_order(
                            &a.0[i],
                            &b.0[i],
                            k.descending,
                            k.nulls_first,
                            crate::value::Collation::Binary,
                        );
                        if ord != core::cmp::Ordering::Equal {
                            return ord;
                        }
                    }
                    core::cmp::Ordering::Equal
                });
            }
            Op::SorterRewind { target } => {
                scursor = 0;
                if sorter.is_empty() {
                    pc = *target;
                }
            }
            Op::SorterRow { start, count } => {
                if let Some((_, row)) = sorter.get(scursor) {
                    for (i, v) in row.iter().take(*count).enumerate() {
                        regs[*start + i] = v.clone();
                    }
                }
            }
            Op::SorterNext { target } => {
                scursor += 1;
                if scursor < sorter.len() {
                    pc = *target;
                }
            }
            Op::AggStep { slot, kind, arg } => {
                if *slot >= agg.len() {
                    agg.resize(*slot + 1, (Vec::new(), 0));
                }
                if *kind == AggKind::CountStar {
                    agg[*slot].1 += 1;
                } else if let Some(r) = arg {
                    if !matches!(regs[*r], Value::Null) {
                        let v = regs[*r].clone();
                        agg[*slot].0.push(v);
                    }
                }
            }
            Op::AggFinal { slot, kind, dest } => {
                let (vals, star) = match agg.get_mut(*slot) {
                    Some(e) => core::mem::take(e),
                    None => (Vec::new(), 0),
                };
                regs[*dest] = finalize_agg(*kind, vals, star);
            }
            Op::Halt => break,
        }
    }
    Ok(out)
}

/// Finalize an aggregate slot, matching the tree-walker's semantics exactly:
/// `count` is 0/`n`, `sum` stays integer until it overflows then promotes to
/// real (NULL over no rows), `total` is always real, `avg` is real (NULL over no
/// rows), `min`/`max` reduce by value comparison (NULL over no rows), and
/// `group_concat` joins with `,` (NULL over no rows).
fn finalize_agg(kind: AggKind, vals: Vec<Value>, star: i64) -> Value {
    use crate::exec::eval;
    use core::cmp::Ordering;
    match kind {
        AggKind::CountStar => Value::Integer(star),
        AggKind::Count => Value::Integer(vals.len() as i64),
        AggKind::Sum => {
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
        AggKind::Total => Value::Real(vals.iter().map(eval::to_f64).sum()),
        AggKind::Avg => {
            if vals.is_empty() {
                Value::Null
            } else {
                let sum: f64 = vals.iter().map(eval::to_f64).sum();
                Value::Real(sum / vals.len() as f64)
            }
        }
        AggKind::Min => vals
            .into_iter()
            .reduce(|a, b| {
                if eval::compare(&b, &a) == Ordering::Less {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(Value::Null),
        AggKind::Max => vals
            .into_iter()
            .reduce(|a, b| {
                if eval::compare(&b, &a) == Ordering::Greater {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(Value::Null),
        AggKind::GroupConcat => {
            if vals.is_empty() {
                Value::Null
            } else {
                let parts: Vec<String> = vals.iter().map(eval::to_text).collect();
                Value::Text(parts.join(","))
            }
        }
    }
}

/// Equality used by `DISTINCT`: two NULLs are equal (unlike `=`), otherwise the
/// usual binary-collation value comparison decides.
fn distinct_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => {
            crate::value::cmp_values_coll(a, b, crate::value::Collation::Binary)
                == core::cmp::Ordering::Equal
        }
    }
}

/// `a AND b` under SQLite three-valued logic: false dominates, else NULL if
/// either is NULL, else true.
fn three_valued_and(a: &Value, b: &Value) -> Value {
    use crate::exec::eval::truth;
    match (truth(a), truth(b)) {
        (Some(false), _) | (_, Some(false)) => Value::Integer(0),
        (Some(true), Some(true)) => Value::Integer(1),
        _ => Value::Null,
    }
}

/// `a OR b` under SQLite three-valued logic: true dominates, else NULL if either
/// is NULL, else false.
fn three_valued_or(a: &Value, b: &Value) -> Value {
    use crate::exec::eval::truth;
    match (truth(a), truth(b)) {
        (Some(true), _) | (_, Some(true)) => Value::Integer(1),
        (Some(false), Some(false)) => Value::Integer(0),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::Statement;
    use crate::sql::parse_one;
    use alloc::vec;

    fn run_sql(sql: &str) -> Vec<Vec<Value>> {
        let Statement::Select(sel) = parse_one(sql).unwrap() else {
            panic!("not a select")
        };
        let prog = compile_const_select(&sel).unwrap();
        run(&prog).unwrap()
    }

    #[test]
    fn arithmetic_and_concat() {
        assert_eq!(run_sql("SELECT 1 + 2 * 3"), vec![vec![Value::Integer(7)]]);
        assert_eq!(
            run_sql("SELECT 10 - 4, 8 / 2"),
            vec![vec![Value::Integer(6), Value::Integer(4)]]
        );
        assert_eq!(
            run_sql("SELECT 'a' || 'b' || 'c'"),
            vec![vec![Value::Text("abc".into())]]
        );
        assert_eq!(
            run_sql("SELECT -5, 3.5"),
            vec![vec![Value::Integer(-5), Value::Real(3.5)]]
        );
    }

    #[test]
    fn rejects_unsupported() {
        let Statement::Select(sel) = parse_one("SELECT * FROM t").unwrap() else {
            panic!()
        };
        assert!(compile_const_select(&sel).is_err());
    }
}
