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
use crate::exec::eval::Affinity;
use crate::sql::ast::{BinaryOp, Expr, Literal, ResultColumn, Select};
use crate::value::{Collation, Value};
use alloc::string::{String, ToString};
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
    /// Load a blob constant into `dest`.
    Blob { value: Vec<u8>, dest: usize },
    /// Load `NULL` into `dest`.
    Null { dest: usize },
    /// `dest = lhs <op> rhs` for an arithmetic `BinaryOp` (Add/Sub/Mul/Div/Mod).
    Arith {
        op: BinaryOp,
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// `dest = lhs <op> rhs` for a bitwise `BinaryOp` (BitAnd/BitOr/LShift/RShift),
    /// with SQLite's NULL-yields-NULL semantics.
    Bitwise {
        op: BinaryOp,
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// `dest = lhs IS rhs` (`is` true) or `lhs IS NOT rhs` (`is` false); treats
    /// NULL as comparable, always 1/0.
    Is {
        is: bool,
        lhs: usize,
        rhs: usize,
        dest: usize,
        /// Comparison affinities of the operands — `IS`/`IS NOT` apply the same
        /// pre-comparison affinity as `=`, then compare with NULL as comparable.
        la: Option<Affinity>,
        ra: Option<Affinity>,
    },
    /// `dest = (truth(operand) == Some(want)) XOR not` as 0/1 — the `x IS [NOT]
    /// TRUE|FALSE` truthiness test (a NULL operand is neither true nor false).
    /// `want` is the boolean operand; `not` selects the `IS NOT` form.
    Truthy {
        want: bool,
        not: bool,
        operand: usize,
        dest: usize,
    },
    /// `dest = lhs LIKE rhs` (`glob` false) or `lhs GLOB rhs` (`glob` true);
    /// NULL on either side yields NULL.
    Like {
        glob: bool,
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// `dest = lhs -> rhs` (`as_text` false) or `lhs ->> rhs` (`as_text` true):
    /// the JSON extraction operators.
    Json {
        as_text: bool,
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// `dest = name(reg[arg_start], …, reg[arg_start+arg_count-1])`: a pure,
    /// context-free scalar function call, evaluated by re-using the tree-walker's
    /// `func::eval_scalar` over literal-reconstructed argument values.
    Func {
        name: String,
        arg_start: usize,
        arg_count: usize,
        dest: usize,
    },
    /// `dest = lhs || rhs` (text concatenation).
    Concat { lhs: usize, rhs: usize, dest: usize },
    /// `dest = lhs <op> rhs` for a comparison `BinaryOp` (Eq/NotEq/Lt/…), with
    /// SQLite's NULL-yields-NULL three-valued result (1/0/NULL). `la`/`ra` are the
    /// operands' comparison affinities (from their source expressions), applied
    /// before comparing exactly as the tree-walker does.
    Compare {
        op: BinaryOp,
        lhs: usize,
        rhs: usize,
        dest: usize,
        la: Option<Affinity>,
        ra: Option<Affinity>,
        /// The comparison's collating sequence (column-derived; `BINARY` default).
        coll: Collation,
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
    /// Multi-cursor `Rewind` (B5b nested-loop join): position cursor `cursor` at
    /// its first row; jump to `target` when that cursor's row-set is empty.
    RewindC { cursor: usize, target: usize },
    /// Multi-cursor `Column`: load column `col` of cursor `cursor`'s current row.
    ColumnC {
        cursor: usize,
        col: usize,
        dest: usize,
    },
    /// Multi-cursor `Next`: advance cursor `cursor`; jump back to `target` if a row
    /// remains, else fall through.
    NextC { cursor: usize, target: usize },
    /// Mark cursor `cursor` as the NULL row (LEFT JOIN null-padding): every
    /// subsequent `ColumnC` on it reads NULL until the next `RewindC` on that
    /// cursor clears the mark.
    NullRow { cursor: usize },
    /// FULL JOIN bookkeeping: record that cursor `cursor`'s current row has been
    /// matched, in a per-row bitmap that survives `RewindC` (unlike the NULL flag).
    MarkMatched { cursor: usize },
    /// FULL JOIN anti-join pass: jump to `target` when cursor `cursor`'s current
    /// row was already marked matched (so it is skipped in the second pass).
    IfMatched { cursor: usize, target: usize },
    /// Decrement `reg`; jump to `target` once it reaches zero (a `LIMIT` counter).
    DecrJumpZero { reg: usize, target: usize },
    /// If `reg` is positive, decrement it and jump to `target` (an `OFFSET` skip).
    IfPosDecr { reg: usize, target: usize },
    /// `dest = -reg` (numeric negation).
    Negate { reg: usize, dest: usize },
    /// `dest = ~reg` (bitwise NOT; NULL stays NULL).
    BitNot { reg: usize, dest: usize },
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
    /// counter, otherwise collect `arg` (when non-NULL). When `distinct`, a value
    /// equal (BINARY) to one already collected for this slot is skipped, so the
    /// slot folds only over distinct argument values.
    AggStep {
        slot: usize,
        kind: AggKind,
        arg: Option<usize>,
        distinct: bool,
    },
    /// Finalize aggregate `slot` into `dest`.
    AggFinal {
        slot: usize,
        kind: AggKind,
        dest: usize,
    },
    /// `GROUP BY` fold: find-or-create the group for the key in `[key_start,
    /// key_start+key_count)` (first-seen order, NULLs group together) and step
    /// each per-group aggregate.
    GroupStep {
        key_start: usize,
        key_count: usize,
        aggs: Vec<AggSpec>,
    },
    /// Emit one row per group (in first-seen order): each output is either a
    /// group-key value or a finalized per-group aggregate.
    GroupEmit {
        outputs: Vec<GroupOut>,
        agg_kinds: Vec<AggKind>,
    },
    /// Finalize the accumulated groups (computing each slot's value per group)
    /// into an emit list, then position the group cursor at the first group;
    /// jump to `target` (the emit-loop exit) when there are no groups. Used by
    /// the `HAVING`/`ORDER BY` grouped path, where each group's keys and
    /// aggregates are loaded into registers so arbitrary predicates / sort keys
    /// can be computed by ordinary ops.
    GroupFinalize {
        agg_kinds: Vec<AggKind>,
        target: usize,
    },
    /// Load group-key value at index `key` of the current group into `dest`.
    GroupKey { key: usize, dest: usize },
    /// Load the finalized aggregate at `slot` of the current group into `dest`.
    GroupAgg { slot: usize, dest: usize },
    /// Advance the group cursor; jump back to `target` if a group remains.
    GroupNext { target: usize },
    /// Stop execution.
    Halt,
}

/// One per-group aggregate in a [`Op::GroupStep`]: its function and the register
/// holding the (already-evaluated) argument (`None` for `count(*)`).
#[derive(Debug, Clone, PartialEq)]
pub struct AggSpec {
    /// Which aggregate to fold.
    pub kind: AggKind,
    /// Argument register, or `None` for `count(*)`.
    pub arg: Option<usize>,
    /// When set, fold only over distinct argument values (BINARY equality), so
    /// the slot computes e.g. `count(DISTINCT x)` per group.
    pub distinct: bool,
}

/// One output column of a [`Op::GroupEmit`]: a group-key value (by key index) or
/// a finalized aggregate (by slot index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupOut {
    /// The group-key value at this index.
    Key(usize),
    /// The finalized aggregate at this slot.
    Agg(usize),
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

/// One `ORDER BY` key for [`Op::SorterSort`]: direction, NULL placement, and the
/// key's collating sequence (column-derived; `BINARY` default).
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// `DESC` when true.
    pub descending: bool,
    /// Explicit `NULLS FIRST`/`LAST`; `None` uses SQLite's default.
    pub nulls_first: Option<bool>,
    /// The collating sequence to compare this key under.
    pub collation: Collation,
}

/// One aggregate accumulator: collected non-NULL argument values plus a row
/// counter (used by `count(*)`).
type AggAcc = (Vec<Value>, i64);
/// One `GROUP BY` group: its key values and one accumulator per aggregate slot.
type Group = (Vec<Value>, Vec<AggAcc>);

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

impl Program {
    /// A human-readable listing of the program for `EXPLAIN` (Track B, B8): one
    /// `(address, opcode, detail)` triple per instruction. The opcode is the
    /// `Op` variant name and the detail is its operand dump. This is graphite's
    /// own register-machine IR — it intentionally does not mirror SQLite's
    /// `vdbe.c` opcode set, which is documented as unstable and implementation-
    /// specific (and is never compared in the differential corpus).
    pub fn explain_rows(&self) -> Vec<(usize, String, String)> {
        self.ops
            .iter()
            .enumerate()
            .map(|(addr, op)| {
                let detail = alloc::format!("{op:?}");
                // The variant name is the token before the first ` ` or `{`.
                let opcode = detail
                    .split([' ', '{'])
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                (addr, opcode, detail)
            })
            .collect()
    }
}

/// Whether `name`/`argc` is a pure, context-free scalar function the VDBE spike
/// can evaluate by deferring to `func::eval_scalar` over literal argument values.
///
/// The list is deliberately conservative: it excludes every function that reads
/// row or connection state (`random`, `randomblob`, `last_insert_rowid`,
/// `changes`, the date/time family with its `'now'` source, FTS5 helpers, and
/// user-defined functions), since those would silently misread the spike's empty
/// context. Anything not listed falls back to the tree-walker. Each entry is
/// covered by a differential test in `tests/vdbe.rs`.
fn is_pure_scalar_fn(name: &str, argc: usize) -> bool {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        // String functions.
        "lower" | "upper" | "length" | "octet_length" | "trim" | "ltrim" | "rtrim" | "substr"
        | "substring" | "replace" | "instr" | "hex" | "unhex" | "quote" | "char" | "unicode"
        | "concat" | "concat_ws" | "soundex" => true,
        // Numeric functions.
        "abs" | "round" | "sign" | "ceil" | "ceiling" | "floor" | "trunc" | "exp" | "ln"
        | "log" | "log2" | "log10" | "sqrt" | "pow" | "power" | "mod" | "sin" | "cos" | "tan"
        | "asin" | "acos" | "atan" | "atan2" | "sinh" | "cosh" | "tanh" | "radians" | "degrees"
        | "pi" => true,
        // Type / null helpers.
        "typeof" | "nullif" | "zeroblob" | "likelihood" | "likely" | "unlikely" => true,
        "coalesce" | "ifnull" => argc >= 1,
        // Pattern predicates in function form.
        "glob" | "like" => argc == 2,
        // JSON functions that operate purely on their argument *values*. The
        // subtype-aware ones (`json_array`/`json_object`/`json_quote`, which embed
        // a `json(...)`-typed argument as JSON rather than quoting it) are
        // excluded: the VDBE passes `eval_scalar` literal-reconstructed values, so
        // the argument's JSON subtype — carried by its source expression — is
        // lost. They fall back to the tree-walker, which sees the real expression.
        "json" | "json_valid" | "json_type" | "json_array_length" | "json_extract" | "jsonb" => {
            true
        }
        // Variadic scalar min/max need at least two args (one arg is the aggregate).
        "min" | "max" => argc >= 2,
        _ => false,
    }
}

/// Compile a constant-projection `SELECT` (no `FROM`/`WHERE`/aggregates) into a
/// program. Returns `Unsupported` for anything outside the spike's grammar so the
/// caller can fall back to the tree-walking executor.
pub fn compile_const_select(sel: &Select) -> Result<Program> {
    if sel.from.is_some()
        || sel.where_clause.is_some()
        || !sel.group_by.is_empty()
        || sel.having.is_some()
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
        tables: Vec::new(),
        affinities: Vec::new(),
        collations: Vec::new(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: None,
    };
    let mut columns = Vec::new();
    for (i, rc) in sel.columns.iter().enumerate() {
        let ResultColumn::Expr {
            expr,
            alias,
            source,
        } = rc
        else {
            return Err(Error::Unsupported("VDBE spike: only scalar result columns"));
        };
        c.compile_expr_into(expr, i)?;
        columns.push(result_label(expr, alias, source, i));
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
/// The output-column label for a result column, matching the tree-walker's
/// `result_column_label`: an explicit alias, else a bare column's name, else the
/// projection's verbatim source span, else a positional `colN` fallback.
fn result_label(
    expr: &Expr,
    alias: &Option<String>,
    source: &Option<String>,
    idx: usize,
) -> String {
    if let Some(a) = alias {
        return a.clone();
    }
    match expr {
        Expr::Column { column, .. } => column.clone(),
        _ => source
            .clone()
            .unwrap_or_else(|| alloc::format!("col{}", idx + 1)),
    }
}

/// Strip enclosing parentheses from an expression.
fn unparen(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(inner) => unparen(inner),
        _ => e,
    }
}

/// Fold a constant `LIMIT`/`OFFSET` expression — a bare literal, `-5`, `(3)`,
/// `2 + 3`, `CAST('4' AS INT)`, … — to its integer value, or `None` when it
/// references row/connection state (a column, parameter, subquery, or function)
/// so the tree-walker must handle it. Only literal/paren/cast/unary/binary trees
/// are folded; the rowless tree-walker `eval` evaluates those deterministically.
fn fold_const_int(e: &Expr) -> Option<i64> {
    fn pure(e: &Expr) -> bool {
        match e {
            Expr::Literal(_) => true,
            Expr::Paren(x) | Expr::Cast { expr: x, .. } => pure(x),
            Expr::Unary { expr, .. } => pure(expr),
            Expr::Binary { left, right, .. } => pure(left) && pure(right),
            _ => false,
        }
    }
    if !pure(e) {
        return None;
    }
    let v = crate::exec::eval::eval(
        e,
        &crate::exec::eval::EvalCtx::rowless(&crate::exec::eval::Params::default()),
    )
    .ok()?;
    Some(crate::exec::eval::to_i64(&v))
}

fn is_aggregate_expr(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Function { name, args, star, .. }
            if crate::exec::func::is_aggregate_call(name, args.len(), *star)
    )
}

/// Map a 1-arg-or-star aggregate call to its [`AggKind`] (binding the argument
/// register expression). Returns `None` for unsupported call shapes, including
/// any `DISTINCT` aggregate — use [`agg_kind_distinct`] where DISTINCT is
/// handled.
fn agg_kind(expr: &Expr) -> Option<(AggKind, Option<Expr>)> {
    let (kind, arg, distinct) = agg_kind_distinct(expr)?;
    if distinct {
        return None;
    }
    Some((kind, arg))
}

/// Like [`agg_kind`] but also accepts a `DISTINCT` aggregate, reporting whether
/// the call carried `DISTINCT` as the third tuple element. Only the bare
/// single-table aggregate path (`compile_aggregate_select`) dedups the collected
/// argument values, so other callers use [`agg_kind`] (which bails on DISTINCT).
/// `FILTER`/`ORDER BY`/`OVER` still bail.
fn agg_kind_distinct(expr: &Expr) -> Option<(AggKind, Option<Expr>, bool)> {
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
    if filter.is_some() || !order_by.is_empty() || over.is_some() {
        return None;
    }
    let arg = args.first().cloned();
    let kind = match name.to_ascii_lowercase().as_str() {
        // `count(DISTINCT *)` is not valid SQL; a DISTINCT star bails.
        "count" if *star => return (!*distinct).then_some((AggKind::CountStar, None, false)),
        "count" if args.len() == 1 => AggKind::Count,
        "sum" if args.len() == 1 => AggKind::Sum,
        "total" if args.len() == 1 => AggKind::Total,
        "avg" if args.len() == 1 => AggKind::Avg,
        "min" if args.len() == 1 => AggKind::Min,
        "max" if args.len() == 1 => AggKind::Max,
        "group_concat" if args.len() == 1 => AggKind::GroupConcat,
        _ => return None,
    };
    Some((kind, arg, *distinct))
}

/// Compile `SELECT <aggregates> FROM <table> [WHERE …]` (no GROUP BY): the scan
/// folds every aggregate slot, then a single `ResultRow` emits the finalized
/// values. Returns `Unsupported` for shapes outside this grammar (so the caller
/// falls back); `ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT` on an aggregate query are
/// left to the tree-walker.
fn compile_aggregate_select(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    projections: &[(Expr, String)],
) -> Result<Program> {
    if !sel.order_by.is_empty()
        || sel.limit.is_some()
        || sel.offset.is_some()
        || sel.distinct
        || sel.having.is_some()
    {
        return Err(Error::Unsupported("VDBE: bare aggregate only"));
    }
    // Aggregate reduction (min/max) compares under BINARY; a non-BINARY column
    // collation would diverge, so defer the whole query to the tree-walker.
    if collations.iter().any(|c| *c != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation in aggregate",
        ));
    }
    // Every projection must be exactly one supported aggregate call. DISTINCT is
    // supported here (the collected values are deduped at fold time).
    let mut slots: Vec<(AggKind, Option<Expr>, bool)> = Vec::new();
    for (e, _) in projections {
        match agg_kind_distinct(e) {
            Some(spec) => slots.push(spec),
            None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
        }
    }
    let count = projections.len();
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        collations: collations.to_vec(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: None,
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
    for (slot, (kind, arg, distinct)) in slots.iter().enumerate() {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        c.ops.push(Op::AggStep {
            slot,
            kind: *kind,
            arg: arg_reg,
            distinct: *distinct,
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
    for (slot, (kind, _, _)) in slots.iter().enumerate() {
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

/// Collect every top-level aggregate call in `expr` into `out` (deduplicated by
/// structural equality), so each distinct aggregate folds into one slot. Does not
/// recurse into a nested aggregate's arguments (aggregates can't nest).
fn collect_aggregates(expr: &Expr, out: &mut Vec<Expr>) {
    if is_aggregate_expr(expr) {
        if !out.iter().any(|e| e == expr) {
            out.push(expr.clone());
        }
        return;
    }
    match expr {
        Expr::Paren(inner)
        | Expr::Unary { expr: inner, .. }
        | Expr::IsNull { expr: inner, .. }
        | Expr::Cast { expr: inner, .. } => collect_aggregates(inner, out),
        Expr::Binary { left, right, .. } => {
            collect_aggregates(left, out);
            collect_aggregates(right, out);
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_aggregates(a, out);
            }
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            if let Some(o) = operand {
                collect_aggregates(o, out);
            }
            for (w, t) in when_then {
                collect_aggregates(w, out);
                collect_aggregates(t, out);
            }
            if let Some(e) = else_result {
                collect_aggregates(e, out);
            }
        }
        _ => {}
    }
}

/// Emit the GROUP-BY fold loop into `c`: allocate the contiguous group-key
/// registers (one per grouping column), scan the row source, apply `WHERE`, load
/// each grouping key and aggregate argument, and `GroupStep`. The row source is a
/// single cursor when `c.cursor_boundaries` is `None`, else an N-deep nested loop
/// over the cursors (cursor 0 outermost, matching the cross-product fold order).
/// All scan/exit edges are backpatched so control falls through to whatever emit
/// phase the caller appends next. Shared by the single-table and nested-loop-join
/// grouped compilers, so a `GROUP BY` join inherits the full grouped grammar.
fn emit_group_fold(
    c: &mut Compiler,
    sel: &Select,
    group_cols: &[usize],
    agg_specs: &[(AggKind, Option<Expr>, bool)],
) -> Result<()> {
    let bounds = c.cursor_boundaries.clone();
    // Contiguous key registers, loaded per row from the grouping columns.
    let key_start = c.next_reg;
    for _ in group_cols {
        c.alloc();
    }
    // Prologue: position the row source. A join opens N cursors (outermost first);
    // a single table opens one.
    let mut rewind_at: Vec<usize> = Vec::new();
    let mut single_rewind: Option<usize> = None;
    match &bounds {
        Some(b) => {
            rewind_at = alloc::vec![0usize; b.len()];
            for (i, slot) in rewind_at.iter_mut().enumerate() {
                *slot = c.ops.len();
                c.ops.push(Op::RewindC {
                    cursor: i,
                    target: 0,
                });
            }
        }
        None => {
            single_rewind = Some(c.ops.len());
            c.ops.push(Op::Rewind { target: 0 });
        }
    }
    let body = c.ops.len();
    // WHERE (already merged with any join `ON`): skip the row when not true.
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
    // Load each grouping key into its register (per-cursor `ColumnC` for a join).
    for (k, &ci) in group_cols.iter().enumerate() {
        match &bounds {
            Some(b) => {
                let (cursor, col) = Compiler::cursor_of(b, ci);
                c.ops.push(Op::ColumnC {
                    cursor,
                    col,
                    dest: key_start + k,
                });
            }
            None => c.ops.push(Op::Column {
                col: ci,
                dest: key_start + k,
            }),
        }
    }
    // Evaluate each aggregate argument into a register for this row.
    let mut aggs: Vec<AggSpec> = Vec::new();
    for (kind, arg, distinct) in agg_specs {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        aggs.push(AggSpec {
            kind: *kind,
            arg: arg_reg,
            distinct: *distinct,
        });
    }
    c.ops.push(Op::GroupStep {
        key_start,
        key_count: group_cols.len(),
        aggs,
    });
    // Epilogue: advance the loop(s) and backpatch the empty/exit edges so an empty
    // source lands at `end` (no groups → the caller emits no rows).
    match &bounds {
        Some(b) => {
            let n = b.len();
            let mut next_at = alloc::vec![0usize; n];
            for i in (0..n).rev() {
                next_at[i] = c.ops.len();
                let target = if i == n - 1 { body } else { rewind_at[i + 1] };
                c.ops.push(Op::NextC { cursor: i, target });
            }
            let end = c.ops.len();
            for i in 0..n {
                let target = if i == 0 { end } else { next_at[i - 1] };
                if let Op::RewindC { target: t, .. } = &mut c.ops[rewind_at[i]] {
                    *t = target;
                }
            }
            if let Some(at) = skip {
                if let Op::IfFalse { target, .. } = &mut c.ops[at] {
                    *target = next_at[n - 1];
                }
            }
        }
        None => {
            let next = c.ops.len();
            c.ops.push(Op::Next { target: body });
            if let Some(at) = skip {
                if let Op::IfFalse { target, .. } = &mut c.ops[at] {
                    *target = next;
                }
            }
            let end = c.ops.len();
            if let Some(rw) = single_rewind {
                if let Op::Rewind { target } = &mut c.ops[rw] {
                    *target = end;
                }
            }
        }
    }
    Ok(())
}

/// Compile `SELECT <group cols / aggregates> FROM <table or join> [WHERE …] GROUP
/// BY <cols> [HAVING …] [ORDER BY …] [LIMIT/OFFSET]`. Each grouping key must be a
/// bare column. Output columns, the `HAVING` predicate, and `ORDER BY` keys may
/// reference grouping columns and aggregate calls (in arbitrary scalar
/// expressions, via the compiler's binding table). The scan folds per-group
/// aggregates (first-seen group order); a second pass then walks the groups,
/// applies `HAVING`, projects, and (optionally) feeds a sorter for `ORDER BY`.
/// `DISTINCT` and non-grouped output columns still fall back.
fn compile_group_select(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    projections: &[(Expr, String)],
    boundaries: Option<&[usize]>,
) -> Result<Program> {
    if sel.distinct {
        return Err(Error::Unsupported("VDBE: GROUP BY + DISTINCT"));
    }
    // Group-key matching and min/max reduction compare under BINARY; a non-BINARY
    // column collation would diverge, so defer to the tree-walker.
    if collations.iter().any(|c| *c != Collation::Binary) {
        return Err(Error::Unsupported("VDBE: non-BINARY collation in GROUP BY"));
    }
    let has_having = sel.having.is_some();
    let has_order = !sel.order_by.is_empty();
    let has_limit = sel.limit.is_some() || sel.offset.is_some();

    // The compiler is created up front so grouping keys (and, on the plain path,
    // outputs) resolve through `resolve_column` — qualifier-aware, so a join's
    // ambiguous bare name bails while a qualified `t.col` picks the right table.
    // `cursor_boundaries` drives single-cursor vs nested-loop emission downstream.
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: 0,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        collations: collations.to_vec(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: boundaries.map(|b| b.to_vec()),
    };
    // Resolve each grouping key to a (combined) column index (column refs only).
    let mut group_cols: Vec<usize> = Vec::new();
    for g in &sel.group_by {
        match g {
            Expr::Column { table, column } => {
                group_cols.push(c.resolve_column(table.as_deref(), column)?)
            }
            _ => return Err(Error::Unsupported("VDBE: GROUP BY column refs only")),
        }
    }

    // The plain path (no HAVING / ORDER BY / LIMIT) keeps its compact `GroupEmit`.
    if !has_having && !has_order && !has_limit {
        return compile_group_emit(
            sel,
            columns,
            tables,
            affinities,
            collations,
            projections,
            &group_cols,
            boundaries,
        );
    }

    // General path: gather every distinct aggregate referenced by the projection,
    // HAVING, and ORDER BY into slots; the rest of each expression is compiled
    // against per-group registers holding the grouping keys and aggregate finals.
    let mut agg_exprs: Vec<Expr> = Vec::new();
    for (e, _) in projections {
        collect_aggregates(e, &mut agg_exprs);
    }
    if let Some(h) = &sel.having {
        collect_aggregates(h, &mut agg_exprs);
    }
    for term in &sel.order_by {
        collect_aggregates(&term.expr, &mut agg_exprs);
    }
    // Each aggregate must be a shape the VDBE can fold. DISTINCT is supported (the
    // per-group collected values are deduped at fold time, BINARY only — a
    // non-BINARY collation already bailed above).
    let mut agg_specs: Vec<(AggKind, Option<Expr>, bool)> = Vec::new();
    for e in &agg_exprs {
        match agg_kind_distinct(e) {
            Some(spec) => agg_specs.push(spec),
            None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
        }
    }

    // Fold the row source (single cursor or nested-loop join) into per-group
    // aggregates, allocating the grouping-key registers.
    emit_group_fold(&mut c, sel, &group_cols, &agg_specs)?;

    // Per-group registers: one for each grouping key value, one for each
    // aggregate final. These feed the bindings so HAVING/projection/ORDER-BY
    // expressions resolve grouping-column refs and aggregate calls to registers.
    let gkey_start = c.next_reg;
    for _ in &group_cols {
        c.alloc();
    }
    let gagg_start = c.next_reg;
    for _ in &agg_specs {
        c.alloc();
    }
    // Grouping-column reference → its key register. Bind both the bare form
    // (`g`) and the qualified form (`t.g`) so a qualified reference in the
    // projection / HAVING / ORDER BY resolves to the key register too (otherwise
    // it would compile to a scan-column read that is invalid during emit).
    for (k, &ci) in group_cols.iter().enumerate() {
        c.bindings.push((
            Expr::Column {
                table: None,
                column: columns[ci].clone(),
            },
            gkey_start + k,
        ));
        if let Some(t) = tables.get(ci) {
            c.bindings.push((
                Expr::Column {
                    table: Some(t.clone()),
                    column: columns[ci].clone(),
                },
                gkey_start + k,
            ));
        }
    }
    // Each aggregate call → its final register.
    for (j, e) in agg_exprs.iter().enumerate() {
        c.bindings.push((e.clone(), gagg_start + j));
    }

    // Output registers for the projected row (a contiguous block).
    let count = projections.len();
    let out_start = c.next_reg;
    for _ in 0..count {
        c.alloc();
    }

    // Optional LIMIT/OFFSET counters (constant integers only).
    let limit_reg = match &sel.limit {
        None => None,
        Some(e) => match fold_const_int(e) {
            // A negative LIMIT means "unlimited" in SQLite.
            Some(n) if n < 0 => None,
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer LIMIT")),
        },
    };
    let offset_reg = match &sel.offset {
        None => None,
        Some(e) => match fold_const_int(e) {
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer OFFSET")),
        },
    };

    // Resolve each ORDER BY term to an output-column expression where it is a
    // bare ordinal or output alias (mirroring the scan path / tree-walker).
    let mut key_specs: Vec<(Expr, SortKey)> = Vec::new();
    for term in &sel.order_by {
        let expr = match &term.expr {
            Expr::Literal(Literal::Integer(k)) if *k >= 1 && (*k as usize) <= count => {
                projections[*k as usize - 1].0.clone()
            }
            // An out-of-range positional ORDER BY term is an error in SQLite.
            Expr::Literal(Literal::Integer(_)) => {
                return Err(Error::Unsupported("VDBE: ORDER BY ordinal out of range"))
            }
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
        let collation = c.col_collation(&expr).unwrap_or_default();
        key_specs.push((
            expr,
            SortKey {
                descending: term.descending,
                nulls_first: term.nulls_first,
                collation,
            },
        ));
    }
    let key_start2 = c.next_reg;
    for _ in &key_specs {
        c.alloc();
    }

    // `LIMIT 0` emits nothing: skip the whole emit phase when the counter starts
    // at zero (target backpatched to the emit-loop exit).
    let limit_skip = limit_reg.map(|r| {
        let at = c.ops.len();
        c.ops.push(Op::IfFalse { reg: r, target: 0 });
        at
    });

    // Finalize groups and position the group cursor; the emit loop body follows.
    let gfin = c.ops.len();
    c.ops.push(Op::GroupFinalize {
        agg_kinds: agg_specs.iter().map(|(k, _, _)| *k).collect(),
        target: 0,
    });
    let gbody = c.ops.len();
    // Load this group's keys and aggregate finals into their registers.
    for k in 0..group_cols.len() {
        c.ops.push(Op::GroupKey {
            key: k,
            dest: gkey_start + k,
        });
    }
    for j in 0..agg_specs.len() {
        c.ops.push(Op::GroupAgg {
            slot: j,
            dest: gagg_start + j,
        });
    }
    // The emit phase has no current scan row: every column reference must be a
    // grouping key or aggregate (resolved via a binding). A bare non-grouped
    // column (e.g. from `*`) is the "use a representative row" case SQLite allows
    // but the VDBE does not model — forbid raw column reads so it bails here and
    // the tree-walker handles it.
    c.forbid_raw_columns = true;
    // Project the output row into the contiguous output block first, so HAVING
    // (and ORDER BY) can reference output aliases — matching the tree-walker,
    // where a HAVING/ORDER-BY name that isn't a table column resolves to the
    // SELECT-list label. Table columns still take precedence (those bindings are
    // searched first).
    for (i, (expr, _)) in projections.iter().enumerate() {
        c.compile_expr_into(expr, out_start + i)?;
    }
    for (i, (expr, label)) in projections.iter().enumerate() {
        // Only non-(table-column) aliases participate, and never shadow a real
        // column name. A bare-column projection already resolves directly.
        let is_bare_col = matches!(expr, Expr::Column { .. });
        if !is_bare_col && !columns.iter().any(|c| c.eq_ignore_ascii_case(label)) {
            c.bindings.push((
                Expr::Column {
                    table: None,
                    column: label.clone(),
                },
                out_start + i,
            ));
        }
    }
    // HAVING: skip the group (advance) when the predicate is not true.
    let having_skip = match &sel.having {
        Some(h) => {
            let preg = c.compile_expr(h)?;
            let at = c.ops.len();
            c.ops.push(Op::IfFalse {
                reg: preg,
                target: 0,
            });
            Some(at)
        }
        None => None,
    };
    let mut limit_done = None;
    let mut offset_skip = None;
    if has_order {
        for (j, (expr, _)) in key_specs.iter().enumerate() {
            c.compile_expr_into(expr, key_start2 + j)?;
        }
        c.ops.push(Op::SorterInsert {
            row_start: out_start,
            row_count: count,
            key_start: key_start2,
            key_count: key_specs.len(),
        });
    } else {
        // No ORDER BY: apply OFFSET then LIMIT directly to the group stream.
        offset_skip = offset_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::IfPosDecr { reg: r, target: 0 });
            at
        });
        c.ops.push(Op::ResultRow {
            start: out_start,
            count,
        });
        if let Some(r) = limit_reg {
            limit_done = Some(c.ops.len());
            c.ops.push(Op::DecrJumpZero { reg: r, target: 0 });
        }
    }
    let gnext = c.ops.len();
    c.ops.push(Op::GroupNext { target: gbody });
    if let Some(at) = having_skip {
        if let Op::IfFalse { target, .. } = &mut c.ops[at] {
            *target = gnext;
        }
    }
    if let Some(at) = offset_skip {
        // A skipped (offset) group advances to the next without emitting.
        if let Op::IfPosDecr { target, .. } = &mut c.ops[at] {
            *target = gnext;
        }
    }
    let gend = c.ops.len();
    if let Op::GroupFinalize { target, .. } = &mut c.ops[gfin] {
        *target = gend;
    }
    if let Some(at) = limit_done {
        if let Op::DecrJumpZero { target, .. } = &mut c.ops[at] {
            *target = gend;
        }
    }

    // Sorted emit loop for ORDER BY.
    if has_order {
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
        c.ops.push(Op::SorterRow {
            start: out_start,
            count,
        });
        c.ops.push(Op::ResultRow {
            start: out_start,
            count,
        });
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
                *target = snext;
            }
        }
        if let Some(at) = elimit {
            if let Op::DecrJumpZero { target, .. } = &mut c.ops[at] {
                *target = eend;
            }
        }
    }

    // `LIMIT 0` jumps past the whole emit phase (here, just before Halt).
    let final_end = c.ops.len();
    if let Some(at) = limit_skip {
        if let Op::IfFalse { target, .. } = &mut c.ops[at] {
            *target = final_end;
        }
    }

    c.ops.push(Op::Halt);
    Ok(Program {
        ops: c.ops,
        n_registers: c.next_reg,
        columns: projections.iter().map(|(_, l)| l.clone()).collect(),
    })
}

/// The compact plain-GROUP-BY path: every output column is a grouping key or a
/// bare aggregate, with no HAVING/ORDER BY/LIMIT. Folds the scan and emits one
/// row per group via [`Op::GroupEmit`].
#[allow(clippy::too_many_arguments)] // cohesive: the combined column space + boundaries
fn compile_group_emit(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    projections: &[(Expr, String)],
    group_cols: &[usize],
    boundaries: Option<&[usize]>,
) -> Result<Program> {
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: 0,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        collations: collations.to_vec(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: boundaries.map(|b| b.to_vec()),
    };
    // Classify each output column as a grouping-key reference or an aggregate.
    // Column refs resolve through `resolve_column` (qualifier-aware) so a join's
    // qualified `t.col` and ambiguous bare names are handled like the tree-walker.
    let mut outputs: Vec<GroupOut> = Vec::new();
    let mut agg_specs: Vec<(AggKind, Option<Expr>, bool)> = Vec::new();
    for (e, _) in projections {
        if is_aggregate_expr(e) {
            match agg_kind_distinct(e) {
                Some(spec) => {
                    outputs.push(GroupOut::Agg(agg_specs.len()));
                    agg_specs.push(spec);
                }
                None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
            }
        } else if let Expr::Column { table, column } = e {
            // Must be one of the grouping columns (by combined column index).
            let ci = c.resolve_column(table.as_deref(), column)?;
            match group_cols.iter().position(|&g| g == ci) {
                Some(k) => outputs.push(GroupOut::Key(k)),
                None => return Err(Error::Unsupported("VDBE: non-grouped column")),
            }
        } else {
            return Err(Error::Unsupported(
                "VDBE: GROUP BY output must be key or aggregate",
            ));
        }
    }

    // Fold the row source (single cursor or nested-loop join) into per-group
    // aggregates, allocating the grouping-key registers.
    emit_group_fold(&mut c, sel, group_cols, &agg_specs)?;

    c.ops.push(Op::GroupEmit {
        outputs,
        agg_kinds: agg_specs.iter().map(|(k, _, _)| *k).collect(),
    });
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
pub fn compile_table_select(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    rowid: bool,
) -> Result<Program> {
    if !sel.compound.is_empty() {
        return Err(Error::Unsupported("VDBE: only plain table projections"));
    }
    // Expand the projection list to concrete expressions/labels (supporting `*`).
    let projections = expand_projections(sel, columns, tables)?;
    // A constant `LIMIT 0` yields no rows for any query shape: emit a program
    // that halts immediately (the column labels are still reported).
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            n_registers: projections.len(),
            columns: projections.iter().map(|(_, l)| l.clone()).collect(),
        });
    }
    // GROUP BY folds the scan into one row per group.
    if !sel.group_by.is_empty() {
        return compile_group_select(
            sel,
            columns,
            tables,
            affinities,
            collations,
            &projections,
            None,
        );
    }
    // An all-aggregate projection (no GROUP BY) folds the scan into one row.
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return compile_aggregate_select(
            sel,
            columns,
            tables,
            affinities,
            collations,
            &projections,
        );
    }
    // A `HAVING` on a non-grouped, non-aggregate query is either an aggregate
    // filter or invalid; either way the plain scan path does not model it, so
    // defer to the tree-walker (which evaluates or rejects it).
    if sel.having.is_some() {
        return Err(Error::Unsupported("VDBE: HAVING without GROUP BY"));
    }
    let count = projections.len();
    // When the caller appends a hidden rowid value to each row (single-table
    // scans only), expose it as the slot after the visible columns with INTEGER
    // affinity / BINARY collation, so a `rowid`/`_rowid_`/`oid` reference resolves.
    let mut affinities = affinities.to_vec();
    let mut collations = collations.to_vec();
    let rowid_index = if rowid {
        affinities.push(Affinity::Integer);
        collations.push(Collation::Binary);
        Some(columns.len())
    } else {
        None
    };
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities,
        collations,
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index,
        cursor_boundaries: None,
    };
    // Optional LIMIT (constant integer only): a counter register decremented
    // after each emitted row, halting the loop at zero.
    let limit_reg = match &sel.limit {
        None => None,
        Some(e) => match fold_const_int(e) {
            // A negative LIMIT means "unlimited" in SQLite.
            Some(n) if n < 0 => None,
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer LIMIT")),
        },
    };
    // Optional OFFSET (constant integer only): a counter decremented while it is
    // positive, skipping that many qualifying rows before any are emitted.
    let offset_reg = match &sel.offset {
        None => None,
        Some(e) => match fold_const_int(e) {
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer OFFSET")),
        },
    };
    // With `ORDER BY`, the scan feeds a sorter and `LIMIT`/`OFFSET` apply to the
    // sorted emit loop instead of to the scan itself.
    let ordering = !sel.order_by.is_empty();
    // Reserve contiguous sort-key registers (one per `ORDER BY` term).
    let key_specs = if ordering {
        build_sort_keys(&c, sel, columns, &projections, count)?
    } else {
        Vec::new()
    };
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
    // DISTINCT compares rows under BINARY, so a non-BINARY column collation would
    // diverge — defer to the tree-walker.
    if sel.distinct && c.collations.iter().any(|cl| *cl != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation with DISTINCT",
        ));
    }
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

/// Expand a `SELECT` projection list to concrete `(expr, label)` pairs: `*`
/// becomes every column, `t.*` the columns whose owning-table qualifier matches
/// `t`. Shared by the single-table scan compiler and the nested-loop join
/// compiler. Returns `Unsupported` on an unknown `t.*` qualifier or an empty list.
fn expand_projections(
    sel: &Select,
    columns: &[String],
    tables: &[String],
) -> Result<Vec<(Expr, String)>> {
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
            ResultColumn::Expr {
                expr,
                alias,
                source,
            } => {
                let label = result_label(expr, alias, source, projections.len());
                projections.push((expr.clone(), label));
            }
            ResultColumn::TableWildcard(q) => {
                let mut any = false;
                for (i, name) in columns.iter().enumerate() {
                    if tables.get(i).is_some_and(|t| t.eq_ignore_ascii_case(q)) {
                        projections.push((
                            Expr::Column {
                                table: Some(q.clone()),
                                column: name.clone(),
                            },
                            name.clone(),
                        ));
                        any = true;
                    }
                }
                if !any {
                    return Err(Error::Unsupported("VDBE: unknown table.* qualifier"));
                }
            }
        }
    }
    if projections.is_empty() {
        return Err(Error::Unsupported("VDBE: empty projection"));
    }
    Ok(projections)
}

/// Resolve a query's `ORDER BY` terms to `(key expression, SortKey)` pairs: a bare
/// positive integer is a 1-based output-column ordinal, a bare name matching an
/// output alias (and not a table column) is that projection, anything else is the
/// term itself. Each key's collation comes from the column it resolves to (via the
/// compiler `c`). Shared by the single-table scan compiler and the nested-loop
/// join compiler. An out-of-range ordinal returns `Unsupported`.
fn build_sort_keys(
    c: &Compiler,
    sel: &Select,
    columns: &[String],
    projections: &[(Expr, String)],
    count: usize,
) -> Result<Vec<(Expr, SortKey)>> {
    let mut key_specs: Vec<(Expr, SortKey)> = Vec::new();
    for term in &sel.order_by {
        let expr = match &term.expr {
            Expr::Literal(Literal::Integer(k)) if *k >= 1 && (*k as usize) <= count => {
                projections[*k as usize - 1].0.clone()
            }
            Expr::Literal(Literal::Integer(_)) => {
                return Err(Error::Unsupported("VDBE: ORDER BY ordinal out of range"))
            }
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
        let collation = c.col_collation(&expr).unwrap_or_default();
        key_specs.push((
            expr,
            SortKey {
                descending: term.descending,
                nulls_first: term.nulls_first,
                collation,
            },
        ));
    }
    Ok(key_specs)
}

/// Compile an N-table inner join (`SELECT … FROM t1 JOIN t2 … JOIN tN …`) into an
/// N-deep nested-loop program — one cursor per table, cursor 0 outermost —
/// instead of materializing the full `t1 × … × tN` cross-product (B5b-1).
/// `columns`/`tables`/`affinities`/`collations` are the tables' arrays
/// concatenated left-to-right; `boundaries` holds the cumulative per-cursor column
/// counts (`boundaries[i]` = end of cursor `i`'s columns, `boundaries.last()` =
/// total), so a combined column index resolves to its `(cursor, local col)`.
/// `sel.where_clause` must already fold in the `ON` predicates (merged by the
/// caller). Supports projection + WHERE + DISTINCT (BINARY collation) + ORDER BY
/// (staged through a sorter) + constant LIMIT/OFFSET; returns `Unsupported` for
/// GROUP BY / aggregates / HAVING so the caller falls back to the cross-product
/// (or tree-walker) path. Without ORDER BY the row order (innermost cursor
/// advancing fastest, cursor 0 outermost) matches the cross-product and SQLite's
/// nested-loop order.
pub fn compile_join2(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    boundaries: &[usize],
) -> Result<Program> {
    let n = boundaries.len();
    debug_assert!(n >= 2 && boundaries[n - 1] == columns.len());
    if !sel.compound.is_empty() || !sel.group_by.is_empty() || sel.having.is_some() {
        return Err(Error::Unsupported("VDBE: join shape not nested-loopable"));
    }
    // DISTINCT compares output rows under BINARY; a non-BINARY column collation
    // would diverge, so defer those to the tree-walker (as the single-table path
    // does).
    if sel.distinct && collations.iter().any(|cl| *cl != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation with DISTINCT",
        ));
    }
    let projections = expand_projections(sel, columns, tables)?;
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return Err(Error::Unsupported("VDBE: aggregate over a join"));
    }
    let count = projections.len();
    // A constant `LIMIT 0` yields nothing (labels still reported).
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            n_registers: count,
            columns: projections.into_iter().map(|(_, l)| l).collect(),
        });
    }
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        collations: collations.to_vec(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: Some(boundaries.to_vec()),
    };
    // LIMIT / OFFSET counters (constant integer only; negative LIMIT = unlimited,
    // non-positive OFFSET = none).
    let limit_reg = match &sel.limit {
        None => None,
        Some(e) => match fold_const_int(e) {
            Some(n) if n < 0 => None,
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer LIMIT")),
        },
    };
    let offset_reg = match &sel.offset {
        None => None,
        Some(e) => match fold_const_int(e) {
            Some(n) if n <= 0 => None,
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer OFFSET")),
        },
    };
    // With ORDER BY, the nested loop stages each surviving row + its sort keys
    // into a sorter; LIMIT/OFFSET then apply to the sorted emit loop, not the
    // scan. Reserve contiguous key registers (one per ORDER BY term).
    let ordering = !sel.order_by.is_empty();
    let key_specs = if ordering {
        build_sort_keys(&c, sel, columns, &projections, count)?
    } else {
        Vec::new()
    };
    let key_start = c.next_reg;
    for _ in &key_specs {
        c.alloc();
    }
    // N-deep nested loop: `RewindC 0 [ RewindC 1 [ … [ RewindC n-1 <body>
    // NextC n-1 ] … NextC 1 ] NextC 0` — cursor 0 outermost, matching the
    // cross-product's leftmost-outermost row order.
    let mut rewind_at = alloc::vec![0usize; n];
    for (i, slot) in rewind_at.iter_mut().enumerate() {
        *slot = c.ops.len();
        c.ops.push(Op::RewindC {
            cursor: i,
            target: 0,
        });
    }
    let body = c.ops.len();
    // WHERE (already merged with ON): skip to the innermost Next when not true.
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
    for (i, (expr, _)) in projections.iter().enumerate() {
        c.compile_expr_into(expr, i)?;
    }
    // DISTINCT gates on the projected row and runs before OFFSET/LIMIT/sorter (a
    // duplicate must not consume the budget nor enter the sorter).
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
    // Body tail: with ORDER BY, stage into the sorter; otherwise emit directly,
    // applying OFFSET then LIMIT inline.
    let (offset_skip, limit_done) = if ordering {
        for (j, (expr, _)) in key_specs.iter().enumerate() {
            c.compile_expr_into(expr, key_start + j)?;
        }
        c.ops.push(Op::SorterInsert {
            row_start: 0,
            row_count: count,
            key_start,
            key_count: key_specs.len(),
        });
        (None, None)
    } else {
        let offset_skip = offset_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::IfPosDecr { reg: r, target: 0 });
            at
        });
        c.ops.push(Op::ResultRow { start: 0, count });
        let limit_done = limit_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::DecrJumpZero { reg: r, target: 0 });
            at
        });
        (offset_skip, limit_done)
    };
    // Emit the `NextC`s innermost-first; `next_at[i]` is the address of cursor
    // `i`'s advance. The innermost re-runs the body; an outer one re-runs the
    // next-inner cursor's `RewindC`.
    let mut next_at = alloc::vec![0usize; n];
    for i in (0..n).rev() {
        next_at[i] = c.ops.len();
        let target = if i == n - 1 { body } else { rewind_at[i + 1] };
        c.ops.push(Op::NextC { cursor: i, target });
    }
    // Sorted emit loop (ORDER BY): sort, then walk the sorter applying OFFSET then
    // LIMIT to the ordered output.
    let elimit = if ordering {
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
        // A skipped (OFFSET) ordered row advances to the next sorter row.
        if let Some(at) = eoffset {
            if let Op::IfPosDecr { target, .. } = &mut c.ops[at] {
                *target = snext;
            }
        }
        elimit
    } else {
        None
    };
    let end = c.ops.len();
    c.ops.push(Op::Halt);
    // Backpatch. An empty cursor `i` jumps to its parent's advance (cursor 0 to
    // the end → no rows); the WHERE/DISTINCT/OFFSET skip advances the innermost
    // cursor.
    for i in 0..n {
        let target = if i == 0 { end } else { next_at[i - 1] };
        if let Op::RewindC { target: t, .. } = &mut c.ops[rewind_at[i]] {
            *t = target;
        }
    }
    let inner_next = next_at[n - 1];
    if let Some(at) = skip {
        if let Op::IfFalse { target, .. } = &mut c.ops[at] {
            *target = inner_next;
        }
    }
    if let Some(at) = distinct_skip {
        if let Op::DistinctCheck { target, .. } = &mut c.ops[at] {
            *target = inner_next; // a duplicate row advances the innermost cursor
        }
    }
    if let Some(at) = offset_skip {
        if let Op::IfPosDecr { target, .. } = &mut c.ops[at] {
            *target = inner_next;
        }
    }
    if let Some(at) = limit_done {
        if let Op::DecrJumpZero { target, .. } = &mut c.ops[at] {
            *target = end;
        }
    }
    // The sorted emit loop's LIMIT halts the whole program (its OFFSET was
    // backpatched inline above).
    if let Some(at) = elimit {
        if let Op::DecrJumpZero { target, .. } = &mut c.ops[at] {
            *target = end;
        }
    }
    Ok(Program {
        ops: c.ops,
        n_registers: c.next_reg,
        columns: projections.into_iter().map(|(_, l)| l).collect(),
    })
}

/// Compile a bare-aggregate N-table inner join (`SELECT <aggregates> FROM t1 JOIN
/// … JOIN tN [WHERE …]`, no GROUP BY) into an N-deep nested loop that folds every
/// surviving combined row into the aggregate slots, then emits one finalized row —
/// instead of materializing the full `t1 × … × tN` cross-product and folding *that*
/// (B5b-1, perf-only: the fallback path already produces the same answer). Mirrors
/// `compile_aggregate_select`'s grammar — every projection is exactly one
/// supported aggregate call, BINARY collations only, no ORDER BY / LIMIT / OFFSET /
/// DISTINCT / HAVING (those defer to the caller). The fold order (cursor 0
/// outermost, innermost fastest) is identical to the cross-product's row order, so
/// order-sensitive `group_concat` matches too. An empty cursor still emits one row
/// (`count` → 0, the rest NULL), as SQLite does. `columns`/`tables`/`affinities`/
/// `collations`/`boundaries` are as for [`compile_join2`].
pub fn compile_aggregate_join(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    boundaries: &[usize],
) -> Result<Program> {
    let n = boundaries.len();
    debug_assert!(n >= 2 && boundaries[n - 1] == columns.len());
    if !sel.compound.is_empty()
        || !sel.group_by.is_empty()
        || sel.having.is_some()
        || !sel.order_by.is_empty()
        || sel.limit.is_some()
        || sel.offset.is_some()
        || sel.distinct
    {
        return Err(Error::Unsupported("VDBE: bare aggregate join only"));
    }
    // min/max reduce under BINARY; a non-BINARY column collation would diverge.
    if collations.iter().any(|cl| *cl != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation in aggregate",
        ));
    }
    let projections = expand_projections(sel, columns, tables)?;
    // Every projection must be exactly one supported aggregate call.
    let mut slots: Vec<(AggKind, Option<Expr>)> = Vec::new();
    for (e, _) in &projections {
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
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        collations: collations.to_vec(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: Some(boundaries.to_vec()),
    };
    // N-deep nested loop (cursor 0 outermost), as in `compile_join2`.
    let mut rewind_at = alloc::vec![0usize; n];
    for (i, slot) in rewind_at.iter_mut().enumerate() {
        *slot = c.ops.len();
        c.ops.push(Op::RewindC {
            cursor: i,
            target: 0,
        });
    }
    let body = c.ops.len();
    // WHERE (already merged with ON): skip to the innermost `NextC` when not true.
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
    // Fold the surviving combined row into every aggregate slot. The join path
    // bails on DISTINCT aggregates (via `agg_kind`), so this is never distinct.
    for (slot, (kind, arg)) in slots.iter().enumerate() {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        c.ops.push(Op::AggStep {
            slot,
            kind: *kind,
            arg: arg_reg,
            distinct: false,
        });
    }
    // `NextC` chain, innermost-first (as in `compile_join2`).
    let mut next_at = alloc::vec![0usize; n];
    for i in (0..n).rev() {
        next_at[i] = c.ops.len();
        let target = if i == n - 1 { body } else { rewind_at[i + 1] };
        c.ops.push(Op::NextC { cursor: i, target });
    }
    // `end` is the finalize point: an empty cursor 0 (or an exhausted outer loop)
    // lands here, still finalizing the slots (count → 0) and emitting one row.
    let end = c.ops.len();
    for i in 0..n {
        let target = if i == 0 { end } else { next_at[i - 1] };
        if let Op::RewindC { target: t, .. } = &mut c.ops[rewind_at[i]] {
            *t = target;
        }
    }
    if let Some(at) = skip {
        if let Op::IfFalse { target, .. } = &mut c.ops[at] {
            *target = next_at[n - 1];
        }
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
        columns: projections.into_iter().map(|(_, l)| l).collect(),
    })
}

/// Compile a `GROUP BY` over an N-table inner join (`SELECT <keys/aggregates> FROM
/// t1 JOIN … GROUP BY <cols> [HAVING …] [ORDER BY …] [LIMIT/OFFSET]`) onto an N-deep
/// nested loop that folds each surviving combined row into its group — instead of
/// materializing the `t1 × … × tN` cross-product and grouping *that* (B5b-1,
/// perf-only: the fallback already gives the same answer). This is a thin adapter:
/// it expands the projection and hands the combined column space to the shared
/// `compile_group_select`, which drives the row source from `cursor_boundaries`,
/// so the join inherits the full single-table grouped grammar — plain `GroupEmit`,
/// or the general `HAVING` / `ORDER BY` / `LIMIT` path. The fold order (cursor 0
/// outermost) matches the cross-product, so the first-seen group order is
/// byte-identical; an empty cursor yields no groups (no rows), as SQLite does.
/// Grouping/output column refs are qualifier-aware. `DISTINCT`, a non-column key, a
/// non-grouped output, or a non-BINARY collation return `Unsupported` (the caller
/// falls back).
pub fn compile_group_join(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    boundaries: &[usize],
) -> Result<Program> {
    let n = boundaries.len();
    debug_assert!(n >= 2 && boundaries[n - 1] == columns.len());
    // The compound case is handled per-arm elsewhere; a grouped *join* arm with a
    // tail compound is out of scope here.
    if !sel.compound.is_empty() || sel.group_by.is_empty() {
        return Err(Error::Unsupported("VDBE: GROUP BY join requires GROUP BY"));
    }
    let projections = expand_projections(sel, columns, tables)?;
    compile_group_select(
        sel,
        columns,
        tables,
        affinities,
        collations,
        &projections,
        Some(boundaries),
    )
}

/// Compile a two-table `LEFT JOIN` (`SELECT … FROM a LEFT JOIN b ON …`) into a
/// nested-loop program with null-padding (B5b-1). For each left (cursor 0) row,
/// the inner cursor 1 is scanned: a row whose `on` predicate holds is a match and
/// (if it also passes `WHERE`) is emitted; if NO inner row matched the `on`, one
/// row is emitted with cursor 1's columns NULL (via `NullRow`), still subject to
/// `WHERE`. Unlike an inner join the `on` predicate is NOT merged into `WHERE`
/// (the two have different roles for the unmatched row), so the caller passes it
/// separately and leaves `sel.where_clause` as the query's own `WHERE`. Supports
/// projection + WHERE + DISTINCT (BINARY) + ORDER BY (staged through a sorter) +
/// constant LIMIT/OFFSET; returns `Unsupported` for GROUP BY / aggregates / HAVING
/// so the caller falls back. Without ORDER BY the row order matches SQLite (each left
/// row's matches in inner-scan order, else its null row); with ORDER BY both the
/// matched and the null-padded rows are staged into the sorter and emitted in key
/// order. DISTINCT gates each emitted row (matched and null-padded) on uniqueness.
#[allow(clippy::too_many_arguments)]
pub fn compile_left_join2(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    n_left: usize,
    on: &Option<Expr>,
) -> Result<Program> {
    if !sel.compound.is_empty() || !sel.group_by.is_empty() || sel.having.is_some() {
        return Err(Error::Unsupported(
            "VDBE: left-join shape not nested-loopable",
        ));
    }
    // DISTINCT compares output rows under BINARY; a non-BINARY column collation
    // would diverge, so defer those to the tree-walker (as the inner-join and
    // single-table paths do).
    if sel.distinct && collations.iter().any(|cl| *cl != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation with DISTINCT",
        ));
    }
    let projections = expand_projections(sel, columns, tables)?;
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return Err(Error::Unsupported("VDBE: aggregate over a left join"));
    }
    let count = projections.len();
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            n_registers: count,
            columns: projections.into_iter().map(|(_, l)| l).collect(),
        });
    }
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        collations: collations.to_vec(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: Some(alloc::vec![n_left, columns.len()]),
    };
    let limit_reg = match &sel.limit {
        None => None,
        Some(e) => match fold_const_int(e) {
            Some(n) if n < 0 => None,
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer LIMIT")),
        },
    };
    let offset_reg = match &sel.offset {
        None => None,
        Some(e) => match fold_const_int(e) {
            Some(n) if n <= 0 => None,
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer OFFSET")),
        },
    };
    let matched = c.alloc();
    // With ORDER BY, both emission points stage their row + sort keys into a
    // sorter; LIMIT/OFFSET then apply to the sorted emit loop, not the scan.
    // Reserve contiguous key registers (one per ORDER BY term).
    let ordering = !sel.order_by.is_empty();
    let key_specs = if ordering {
        build_sort_keys(&c, sel, columns, &projections, count)?
    } else {
        Vec::new()
    };
    let key_start = c.next_reg;
    for _ in &key_specs {
        c.alloc();
    }
    let rewind0 = c.ops.len();
    c.ops.push(Op::RewindC {
        cursor: 0,
        target: 0,
    });
    let loop0 = c.ops.len();
    c.ops.push(Op::Integer {
        value: 0,
        dest: matched,
    });
    let rewind1 = c.ops.len();
    c.ops.push(Op::RewindC {
        cursor: 1,
        target: 0,
    });
    let body = c.ops.len();
    // ON gate (matched rows only); `None` means "always match".
    let on_skip = match on {
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
    c.ops.push(Op::Integer {
        value: 1,
        dest: matched,
    });
    // WHERE gate over the matched (real) row.
    let where_skip_m = match &sel.where_clause {
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
    for (i, (expr, _)) in projections.iter().enumerate() {
        c.compile_expr_into(expr, i)?;
    }
    // DISTINCT gates the projected row before OFFSET/LIMIT/sorter — a duplicate must
    // not consume the budget nor enter the sorter (as in the inner-join path).
    let distinct_skip_m = if sel.distinct {
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
    let (offset_skip_m, limit_m) = if ordering {
        for (j, (expr, _)) in key_specs.iter().enumerate() {
            c.compile_expr_into(expr, key_start + j)?;
        }
        c.ops.push(Op::SorterInsert {
            row_start: 0,
            row_count: count,
            key_start,
            key_count: key_specs.len(),
        });
        (None, None)
    } else {
        let offset_skip_m = offset_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::IfPosDecr { reg: r, target: 0 });
            at
        });
        c.ops.push(Op::ResultRow { start: 0, count });
        let limit_m = limit_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::DecrJumpZero { reg: r, target: 0 });
            at
        });
        (offset_skip_m, limit_m)
    };
    let next1 = c.ops.len();
    c.ops.push(Op::NextC {
        cursor: 1,
        target: body,
    });
    // After the inner scan: if a match was found, advance the outer cursor;
    // otherwise emit the null-padded row.
    let after_inner = c.ops.len();
    c.ops.push(Op::IfFalse {
        reg: matched,
        target: 0,
    }); // not matched → null pad
    let goto_next0 = c.ops.len();
    c.ops.push(Op::Goto { target: 0 });
    let null_pad = c.ops.len();
    c.ops.push(Op::NullRow { cursor: 1 });
    let where_skip_n = match &sel.where_clause {
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
    for (i, (expr, _)) in projections.iter().enumerate() {
        c.compile_expr_into(expr, i)?;
    }
    let distinct_skip_n = if sel.distinct {
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
    let (offset_skip_n, limit_n) = if ordering {
        for (j, (expr, _)) in key_specs.iter().enumerate() {
            c.compile_expr_into(expr, key_start + j)?;
        }
        c.ops.push(Op::SorterInsert {
            row_start: 0,
            row_count: count,
            key_start,
            key_count: key_specs.len(),
        });
        (None, None)
    } else {
        let offset_skip_n = offset_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::IfPosDecr { reg: r, target: 0 });
            at
        });
        c.ops.push(Op::ResultRow { start: 0, count });
        let limit_n = limit_reg.map(|r| {
            let at = c.ops.len();
            c.ops.push(Op::DecrJumpZero { reg: r, target: 0 });
            at
        });
        (offset_skip_n, limit_n)
    };
    let next0 = c.ops.len();
    c.ops.push(Op::NextC {
        cursor: 0,
        target: loop0,
    });
    // After the scan: with ORDER BY, sort then walk the sorter applying OFFSET then
    // LIMIT to the ordered output; without it, this is just the Halt point.
    let scan_done = c.ops.len();
    let elimit = if ordering {
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
                *target = snext;
            }
        }
        elimit
    } else {
        None
    };
    let end = c.ops.len();
    c.ops.push(Op::Halt);
    // Backpatch.
    let set = |ops: &mut [Op], at: usize, tgt: usize| match &mut ops[at] {
        Op::IfFalse { target, .. }
        | Op::IfPosDecr { target, .. }
        | Op::DecrJumpZero { target, .. }
        | Op::Goto { target }
        | Op::DistinctCheck { target, .. }
        | Op::RewindC { target, .. } => *target = tgt,
        _ => {}
    };
    set(&mut c.ops, rewind0, scan_done); // left empty → no rows (drains the sorter)
    set(&mut c.ops, rewind1, after_inner); // right empty → null-pad path
    if let Some(at) = on_skip {
        set(&mut c.ops, at, next1); // ON false → try next inner row
    }
    if let Some(at) = where_skip_m {
        set(&mut c.ops, at, next1); // matched row filtered → next inner row
    }
    if let Some(at) = distinct_skip_m {
        set(&mut c.ops, at, next1); // duplicate matched row → next inner row
    }
    if let Some(at) = offset_skip_m {
        set(&mut c.ops, at, next1);
    }
    if let Some(at) = limit_m {
        set(&mut c.ops, at, end);
    }
    set(&mut c.ops, after_inner, null_pad); // not matched → null pad
    set(&mut c.ops, goto_next0, next0); // matched → advance outer
    if let Some(at) = where_skip_n {
        set(&mut c.ops, at, next0); // null row filtered → advance outer
    }
    if let Some(at) = distinct_skip_n {
        set(&mut c.ops, at, next0); // duplicate null-padded row → advance outer
    }
    if let Some(at) = offset_skip_n {
        set(&mut c.ops, at, next0);
    }
    if let Some(at) = limit_n {
        set(&mut c.ops, at, end);
    }
    // The sorted emit loop's LIMIT halts the whole program (its OFFSET was
    // backpatched inline above).
    if let Some(at) = elimit {
        set(&mut c.ops, at, end);
    }
    Ok(Program {
        ops: c.ops,
        n_registers: c.next_reg,
        columns: projections.into_iter().map(|(_, l)| l).collect(),
    })
}

/// Emit one output row: project into registers `[0, count)`, then (when `distinct`)
/// a `DistinctCheck` gating the row on uniqueness. With ORDER BY (`sorter` is
/// `Some((key_start, key_specs))`), evaluate the sort keys and stage the row + keys
/// into the sorter (LIMIT/OFFSET apply to the sorted emit loop); without it, apply
/// the OFFSET skip, `ResultRow`, then the LIMIT decrement. Returns the OFFSET-skip,
/// LIMIT-done, and DISTINCT-skip op indices (their jump targets are backpatched by
/// the caller; the OFFSET/LIMIT pair is `(None, None)` under ORDER BY). Shared by
/// the FULL-join compiler's three emission points.
fn emit_output_row(
    c: &mut Compiler,
    projections: &[(Expr, String)],
    count: usize,
    offset_reg: Option<usize>,
    limit_reg: Option<usize>,
    sorter: Option<(usize, &[(Expr, SortKey)])>,
    distinct: bool,
) -> Result<(Option<usize>, Option<usize>, Option<usize>)> {
    for (i, (expr, _)) in projections.iter().enumerate() {
        c.compile_expr_into(expr, i)?;
    }
    // DISTINCT gates the projected row before OFFSET/LIMIT/sorter — a duplicate must
    // not consume the budget nor enter the sorter.
    let distinct_skip = if distinct {
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
    if let Some((key_start, key_specs)) = sorter {
        for (j, (expr, _)) in key_specs.iter().enumerate() {
            c.compile_expr_into(expr, key_start + j)?;
        }
        c.ops.push(Op::SorterInsert {
            row_start: 0,
            row_count: count,
            key_start,
            key_count: key_specs.len(),
        });
        return Ok((None, None, distinct_skip));
    }
    let offset_skip = offset_reg.map(|r| {
        let at = c.ops.len();
        c.ops.push(Op::IfPosDecr { reg: r, target: 0 });
        at
    });
    c.ops.push(Op::ResultRow { start: 0, count });
    let limit_done = limit_reg.map(|r| {
        let at = c.ops.len();
        c.ops.push(Op::DecrJumpZero { reg: r, target: 0 });
        at
    });
    Ok((offset_skip, limit_done, distinct_skip))
}

/// Compile a two-table `FULL JOIN` (`SELECT … FROM a FULL JOIN b ON …`) into a
/// two-pass null-padding nested loop (B5b-1). Pass 1 is a LEFT join (every left
/// row; matched rows, else a row with the right side NULL) that also records, in
/// a per-row bitmap, which right rows matched (`MarkMatched`). Pass 2 then scans
/// the right table and emits each row NOT matched in pass 1, with the left side
/// NULL (`IfMatched` skips the matched ones). This yields SQLite's FULL-join order
/// (all left-driven rows, then the unmatched-right rows). `ON` is kept separate
/// from `WHERE`; supports projection + WHERE + DISTINCT (BINARY) + ORDER BY (all
/// three emission points stage through one sorter) + constant LIMIT/OFFSET; returns
/// `Unsupported` for GROUP BY / aggregates / HAVING. DISTINCT gates every emitted row
/// (matched, left-null, right-null) on uniqueness across both passes.
#[allow(clippy::too_many_arguments)]
pub fn compile_full_join2(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    n_left: usize,
    on: &Option<Expr>,
) -> Result<Program> {
    if !sel.compound.is_empty() || !sel.group_by.is_empty() || sel.having.is_some() {
        return Err(Error::Unsupported(
            "VDBE: full-join shape not nested-loopable",
        ));
    }
    // DISTINCT compares output rows under BINARY; a non-BINARY column collation
    // would diverge, so defer those to the tree-walker.
    if sel.distinct && collations.iter().any(|cl| *cl != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation with DISTINCT",
        ));
    }
    let projections = expand_projections(sel, columns, tables)?;
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return Err(Error::Unsupported("VDBE: aggregate over a full join"));
    }
    let count = projections.len();
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            n_registers: count,
            columns: projections.into_iter().map(|(_, l)| l).collect(),
        });
    }
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        collations: collations.to_vec(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: Some(alloc::vec![n_left, columns.len()]),
    };
    let limit_reg = match &sel.limit {
        None => None,
        Some(e) => match fold_const_int(e) {
            Some(n) if n < 0 => None,
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer LIMIT")),
        },
    };
    let offset_reg = match &sel.offset {
        None => None,
        Some(e) => match fold_const_int(e) {
            Some(n) if n <= 0 => None,
            Some(n) => {
                let r = c.alloc();
                c.ops.push(Op::Integer { value: n, dest: r });
                Some(r)
            }
            None => return Err(Error::Unsupported("VDBE: only constant integer OFFSET")),
        },
    };
    let matched = c.alloc();
    // With ORDER BY, all three emission points stage their row + sort keys into one
    // sorter; LIMIT/OFFSET then apply to the sorted emit loop, not the two passes.
    let ordering = !sel.order_by.is_empty();
    let key_specs = if ordering {
        build_sort_keys(&c, sel, columns, &projections, count)?
    } else {
        Vec::new()
    };
    let key_start = c.next_reg;
    for _ in &key_specs {
        c.alloc();
    }
    let sorter = if ordering {
        Some((key_start, key_specs.as_slice()))
    } else {
        None
    };
    let set = |ops: &mut [Op], at: usize, tgt: usize| match &mut ops[at] {
        Op::IfFalse { target, .. }
        | Op::IfPosDecr { target, .. }
        | Op::DecrJumpZero { target, .. }
        | Op::Goto { target }
        | Op::IfMatched { target, .. }
        | Op::DistinctCheck { target, .. }
        | Op::RewindC { target, .. } => *target = tgt,
        _ => {}
    };
    // ---- Pass 1: LEFT join, marking matched right rows. ----
    let rewind0 = c.ops.len();
    c.ops.push(Op::RewindC {
        cursor: 0,
        target: 0,
    });
    let loop0 = c.ops.len();
    c.ops.push(Op::Integer {
        value: 0,
        dest: matched,
    });
    let rewind1 = c.ops.len();
    c.ops.push(Op::RewindC {
        cursor: 1,
        target: 0,
    });
    let body = c.ops.len();
    let on_skip = match on {
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
    c.ops.push(Op::Integer {
        value: 1,
        dest: matched,
    });
    c.ops.push(Op::MarkMatched { cursor: 1 });
    let where_m = match &sel.where_clause {
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
    let (offset_m, limit_m, distinct_m) = emit_output_row(
        &mut c,
        &projections,
        count,
        offset_reg,
        limit_reg,
        sorter,
        sel.distinct,
    )?;
    let next1 = c.ops.len();
    c.ops.push(Op::NextC {
        cursor: 1,
        target: body,
    });
    let after_inner = c.ops.len();
    c.ops.push(Op::IfFalse {
        reg: matched,
        target: 0,
    });
    let goto_next0 = c.ops.len();
    c.ops.push(Op::Goto { target: 0 });
    let null_pad = c.ops.len();
    c.ops.push(Op::NullRow { cursor: 1 });
    let where_lnull = match &sel.where_clause {
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
    let (offset_lnull, limit_lnull, distinct_lnull) = emit_output_row(
        &mut c,
        &projections,
        count,
        offset_reg,
        limit_reg,
        sorter,
        sel.distinct,
    )?;
    let next0 = c.ops.len();
    c.ops.push(Op::NextC {
        cursor: 0,
        target: loop0,
    });
    // ---- Pass 2: right rows not matched in pass 1, with the left side NULL. ----
    let pass2 = c.ops.len();
    c.ops.push(Op::NullRow { cursor: 0 });
    let rewind1b = c.ops.len();
    c.ops.push(Op::RewindC {
        cursor: 1,
        target: 0,
    });
    let loop2 = c.ops.len();
    let if_matched = c.ops.len();
    c.ops.push(Op::IfMatched {
        cursor: 1,
        target: 0,
    });
    let where_rnull = match &sel.where_clause {
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
    let (offset_rnull, limit_rnull, distinct_rnull) = emit_output_row(
        &mut c,
        &projections,
        count,
        offset_reg,
        limit_reg,
        sorter,
        sel.distinct,
    )?;
    let next2 = c.ops.len();
    c.ops.push(Op::NextC {
        cursor: 1,
        target: loop2,
    });
    // After both passes: with ORDER BY, sort then walk the sorter applying OFFSET
    // then LIMIT to the ordered output; without it, this is just the Halt point.
    let scan_done = c.ops.len();
    let elimit = if ordering {
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
                *target = snext;
            }
        }
        elimit
    } else {
        None
    };
    let end = c.ops.len();
    c.ops.push(Op::Halt);
    // ---- Backpatch. ----
    set(&mut c.ops, rewind0, pass2); // left empty → straight to pass 2
    set(&mut c.ops, rewind1, after_inner); // right empty → null-pad this left row
    if let Some(at) = on_skip {
        set(&mut c.ops, at, next1);
    }
    if let Some(at) = where_m {
        set(&mut c.ops, at, next1);
    }
    if let Some(at) = distinct_m {
        set(&mut c.ops, at, next1); // duplicate matched row → next inner row
    }
    if let Some(at) = offset_m {
        set(&mut c.ops, at, next1);
    }
    if let Some(at) = limit_m {
        set(&mut c.ops, at, end);
    }
    set(&mut c.ops, after_inner, null_pad);
    set(&mut c.ops, goto_next0, next0);
    if let Some(at) = where_lnull {
        set(&mut c.ops, at, next0);
    }
    if let Some(at) = distinct_lnull {
        set(&mut c.ops, at, next0); // duplicate left-null row → advance outer
    }
    if let Some(at) = offset_lnull {
        set(&mut c.ops, at, next0);
    }
    if let Some(at) = limit_lnull {
        set(&mut c.ops, at, end);
    }
    set(&mut c.ops, rewind1b, scan_done); // right empty in pass 2 → drain the sorter
    set(&mut c.ops, if_matched, next2); // already matched → skip
    if let Some(at) = where_rnull {
        set(&mut c.ops, at, next2);
    }
    if let Some(at) = distinct_rnull {
        set(&mut c.ops, at, next2); // duplicate right-null row → next right row
    }
    if let Some(at) = offset_rnull {
        set(&mut c.ops, at, next2);
    }
    if let Some(at) = limit_rnull {
        set(&mut c.ops, at, end);
    }
    // The sorted emit loop's LIMIT halts the whole program (its OFFSET was
    // backpatched inline above).
    if let Some(at) = elimit {
        set(&mut c.ops, at, end);
    }
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
    /// Each column's owning-table qualifier (alias if present, else table name),
    /// parallel to `columns`. Empty for a constant `SELECT`. Used to resolve a
    /// qualified `t.col` reference and to disambiguate a join's shared names.
    tables: Vec<String>,
    /// Each column's comparison affinity, parallel to `columns` (empty for a
    /// constant `SELECT` with no table). Used to apply SQLite's pre-comparison
    /// affinity in `Op::Compare`, matching the tree-walker.
    affinities: Vec<Affinity>,
    /// Each column's declared collating sequence, parallel to `columns`. Drives
    /// the collation of comparisons / `ORDER BY` / `DISTINCT` / `GROUP BY` over a
    /// column (`BINARY` when unknown), matching the tree-walker.
    collations: Vec<Collation>,
    /// Expression → register overrides consulted before normal compilation.
    /// Used by the grouped `HAVING`/`ORDER BY` path to resolve aggregate calls
    /// and grouping-column references to per-group registers (so an arbitrary
    /// predicate / sort-key expression over them compiles to ordinary ops).
    bindings: Vec<(Expr, usize)>,
    /// When set, an `Expr::Column` that is not satisfied by a `binding` is a
    /// compile error rather than a scan-row read. The grouped emit phase sets
    /// this: there is no current scan row, so a bare non-grouped, non-aggregate
    /// column (e.g. `SELECT g, count(*), * FROM t GROUP BY g`, where SQLite takes
    /// the value from a representative row) must bail to the tree-walker.
    forbid_raw_columns: bool,
    /// The row index of the hidden `rowid` value, when the single-table scan
    /// appends it (a rowid alias — `rowid`/`_rowid_`/`oid` — resolves here). It
    /// sits AFTER the visible columns, so `*` (which spans only `columns`) skips
    /// it. `None` for constant selects, joins, and `WITHOUT ROWID` tables.
    rowid_index: Option<usize>,
    /// Cumulative column counts per cursor for the nested-loop join path (B5b),
    /// e.g. `[n_left, n_left + n_right]`. When `Some`, an `Expr::Column` resolves
    /// its combined index to a `(cursor, local column)` pair and emits a
    /// multi-cursor `Op::ColumnC`; when `None` (every single-cursor path) it emits
    /// the plain `Op::Column` against cursor 0.
    cursor_boundaries: Option<Vec<usize>>,
}

impl Compiler {
    /// Map a combined column index to `(cursor, local column)` using
    /// `cursor_boundaries` (cumulative per-cursor counts). Only called when
    /// `cursor_boundaries` is `Some`.
    fn cursor_of(boundaries: &[usize], idx: usize) -> (usize, usize) {
        let mut prev = 0;
        for (cur, &end) in boundaries.iter().enumerate() {
            if idx < end {
                return (cur, idx - prev);
            }
            prev = end;
        }
        // Past the last boundary: clamp to the final cursor (defensive; the
        // resolver only yields in-range indices for a join with no rowid slot).
        let last = boundaries.len().saturating_sub(1);
        (
            last,
            idx - boundaries.get(last.wrapping_sub(1)).copied().unwrap_or(0),
        )
    }
}

impl Compiler {
    fn alloc(&mut self) -> usize {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    /// Resolve a column reference to its index in the row, honouring an optional
    /// `table.` qualifier. Returns `Unsupported` when the name is unknown or a
    /// bare name is ambiguous across a join's two tables (so the tree-walker —
    /// which would resolve or reject it identically — takes over).
    fn resolve_column(&self, table: Option<&str>, column: &str) -> Result<usize> {
        let mut found = None;
        for (i, c) in self.columns.iter().enumerate() {
            if !c.eq_ignore_ascii_case(column) {
                continue;
            }
            if let Some(t) = table {
                if !self
                    .tables
                    .get(i)
                    .is_some_and(|tn| tn.eq_ignore_ascii_case(t))
                {
                    continue;
                }
            }
            if found.is_some() {
                // A bare name matching two tables is ambiguous.
                return Err(Error::Unsupported("VDBE: ambiguous column reference"));
            }
            found = Some(i);
        }
        // A rowid alias (`rowid`/`_rowid_`/`oid`) resolves to the hidden rowid slot
        // of a single-table scan — unless a real column already shadows the name.
        if found.is_none() {
            if let Some(ri) = self.rowid_index {
                let is_alias = matches!(
                    column.to_ascii_lowercase().as_str(),
                    "rowid" | "_rowid_" | "oid"
                );
                let table_ok = table.is_none_or(|t| {
                    self.tables
                        .first()
                        .is_some_and(|tn| tn.eq_ignore_ascii_case(t))
                });
                if is_alias && table_ok {
                    return Ok(ri);
                }
            }
        }
        found.ok_or(Error::Unsupported("VDBE: unresolved column reference"))
    }

    /// The comparison affinity an expression contributes (mirrors the
    /// tree-walker's `expr_affinity`): a column's declared affinity, a `CAST`'s
    /// target affinity, transparent through parentheses, else `None` (a literal
    /// or computed value has no affinity).
    fn expr_affinity(&self, expr: &Expr) -> Option<Affinity> {
        match expr {
            Expr::Column { table, column } => self
                .resolve_column(table.as_deref(), column)
                .ok()
                .and_then(|i| self.affinities.get(i).copied()),
            Expr::Cast { type_name, .. } => Some(Affinity::from_type(Some(type_name))),
            Expr::Paren(e) => self.expr_affinity(e),
            // `COLLATE` changes only the collation, not the affinity.
            Expr::Collate { expr, .. } => self.expr_affinity(expr),
            _ => None,
        }
    }

    /// An EXPLICIT `COLLATE name` carried by this operand (through parentheses),
    /// if any. An unknown name is ignored (falls back to the implicit collation).
    fn explicit_collation(&self, expr: &Expr) -> Option<Collation> {
        match expr {
            Expr::Paren(e) => self.explicit_collation(e),
            Expr::Collate { expr, collation } => {
                Collation::parse(collation).or_else(|| self.explicit_collation(expr))
            }
            _ => None,
        }
    }

    /// The IMPLICIT (declared) collation of the column this operand resolves to
    /// (through parentheses / a `COLLATE`), if any.
    fn implicit_collation(&self, expr: &Expr) -> Option<Collation> {
        match expr {
            Expr::Column { table, column } => self
                .resolve_column(table.as_deref(), column)
                .ok()
                .and_then(|i| self.collations.get(i).copied()),
            Expr::Paren(e) | Expr::Collate { expr: e, .. } => self.implicit_collation(e),
            _ => None,
        }
    }

    /// The collating sequence a single ORDER BY operand carries: an explicit
    /// `COLLATE` wins, else the column's declared collation, else `None`
    /// (→ the caller defaults to `BINARY`).
    fn col_collation(&self, expr: &Expr) -> Option<Collation> {
        self.explicit_collation(expr)
            .or_else(|| self.implicit_collation(expr))
    }

    /// The collating sequence of a binary comparison, mirroring SQLite's
    /// precedence: an EXPLICIT `COLLATE` on either operand (left first) beats an
    /// IMPLICIT column collation on either operand (left first), else `BINARY`.
    fn compare_collation(&self, left: &Expr, right: &Expr) -> Collation {
        self.explicit_collation(left)
            .or_else(|| self.explicit_collation(right))
            .or_else(|| self.implicit_collation(left))
            .or_else(|| self.implicit_collation(right))
            .unwrap_or_default()
    }

    /// Push an `Op::Compare`, computing each operand's comparison affinity from
    /// its source expression so the runtime coerces operands like the tree-walker.
    fn push_compare(
        &mut self,
        op: BinaryOp,
        lhs: usize,
        left: &Expr,
        rhs: usize,
        right: &Expr,
        dest: usize,
    ) {
        let coll = self.compare_collation(left, right);
        self.push_compare_coll(op, lhs, left, rhs, right, dest, coll);
    }

    /// Like [`push_compare`](Self::push_compare) but with an explicit collation —
    /// used by `IN`, where a multi-element list ignores per-element `COLLATE` and
    /// applies the left operand's collation to every element comparison.
    #[allow(clippy::too_many_arguments)]
    fn push_compare_coll(
        &mut self,
        op: BinaryOp,
        lhs: usize,
        left: &Expr,
        rhs: usize,
        right: &Expr,
        dest: usize,
        coll: Collation,
    ) {
        let ra = self.expr_affinity(right);
        self.push_compare_coll_ra(op, lhs, left, rhs, ra, dest, coll);
    }

    /// Like [`push_compare_coll`](Self::push_compare_coll) but with the
    /// right-operand comparison affinity supplied explicitly — used by a folded
    /// bare-column `IN (SELECT col)`, where the candidate side contributes the
    /// SELECTed column's affinity rather than the literal element's own.
    #[allow(clippy::too_many_arguments)]
    fn push_compare_coll_ra(
        &mut self,
        op: BinaryOp,
        lhs: usize,
        left: &Expr,
        rhs: usize,
        ra: Option<Affinity>,
        dest: usize,
        coll: Collation,
    ) {
        let la = self.expr_affinity(left);
        self.ops.push(Op::Compare {
            op,
            lhs,
            rhs,
            dest,
            la,
            ra,
            coll,
        });
    }

    /// Compile `expr` into a freshly allocated register, returning its index.
    fn compile_expr(&mut self, expr: &Expr) -> Result<usize> {
        let dest = self.alloc();
        self.compile_expr_into(expr, dest)?;
        Ok(dest)
    }

    /// Compile `expr` so its value lands in register `dest`.
    fn compile_expr_into(&mut self, expr: &Expr, dest: usize) -> Result<()> {
        // A bound sub-expression (a grouping-column ref or aggregate call in the
        // grouped HAVING/ORDER BY path) is already materialized in a register.
        if let Some(&(_, src)) = self.bindings.iter().find(|(e, _)| e == expr) {
            if src != dest {
                self.ops.push(Op::Copy { src, dest });
            }
            return Ok(());
        }
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
                    Literal::Blob(b) => Op::Blob {
                        value: b.clone(),
                        dest,
                    },
                };
                self.ops.push(op);
                Ok(())
            }
            Expr::Column { table, column } => {
                // In the grouped emit phase there is no scan row; a column that
                // was not bound to a key/aggregate register cannot be read.
                if self.forbid_raw_columns {
                    return Err(Error::Unsupported("VDBE: bare column in grouped output"));
                }
                let idx = self.resolve_column(table.as_deref(), column)?;
                match &self.cursor_boundaries {
                    Some(bounds) => {
                        let (cursor, col) = Self::cursor_of(bounds, idx);
                        self.ops.push(Op::ColumnC { cursor, col, dest });
                    }
                    None => self.ops.push(Op::Column { col: idx, dest }),
                }
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
            Expr::Unary {
                op: crate::sql::ast::UnaryOp::BitNot,
                expr: inner,
            } => {
                let r = self.compile_expr(inner)?;
                self.ops.push(Op::BitNot { reg: r, dest });
                Ok(())
            }
            // Unary `+` is a no-op: compile the operand directly into `dest`.
            Expr::Unary {
                op: crate::sql::ast::UnaryOp::Identity,
                expr: inner,
            } => self.compile_expr_into(inner, dest),
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
                    BitAnd | BitOr | LShift | RShift => {
                        self.ops.push(Op::Bitwise {
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
                        self.push_compare(*op, l, left, r, right, dest);
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
                    Is | IsNot => {
                        // A boolean literal on the RIGHT makes `x IS [NOT] TRUE|FALSE`
                        // a truthiness test rather than value equality (matching the
                        // tree-walker, which special-cases only the right operand);
                        // `r` already holds the compiled literal but is unused here.
                        if let Expr::Literal(Literal::Boolean(want)) = unparen(right) {
                            self.ops.push(Op::Truthy {
                                want: *want,
                                not: matches!(op, IsNot),
                                operand: l,
                                dest,
                            });
                        } else {
                            self.ops.push(Op::Is {
                                is: matches!(op, Is),
                                lhs: l,
                                rhs: r,
                                dest,
                                la: self.expr_affinity(left),
                                ra: self.expr_affinity(right),
                            });
                        }
                        Ok(())
                    }
                    Like | Glob => {
                        self.ops.push(Op::Like {
                            glob: matches!(op, Glob),
                            lhs: l,
                            rhs: r,
                            dest,
                        });
                        Ok(())
                    }
                    JsonExtract | JsonExtractText => {
                        self.ops.push(Op::Json {
                            as_text: matches!(op, JsonExtractText),
                            lhs: l,
                            rhs: r,
                            dest,
                        });
                        Ok(())
                    }
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
            // `expr COLLATE name` carries the same VALUE as `expr`; the collation
            // only changes which sequence an enclosing comparison / ORDER BY uses,
            // and that is read off the source expr by `col_collation`.
            Expr::Collate { expr: inner, .. } => self.compile_expr_into(inner, dest),
            Expr::Case {
                operand,
                when_then,
                else_result,
            } => self.compile_case(operand.as_deref(), when_then, else_result.as_deref(), dest),
            // `x BETWEEN lo AND hi` desugars to `(x >= lo) AND (x <= hi)`, and the
            // negated form to its `NOT`. `x` is evaluated once and reused.
            Expr::Between {
                expr: inner,
                low,
                high,
                negated,
            } => {
                let x = self.compile_expr(inner)?;
                let lo = self.compile_expr(low)?;
                let hi = self.compile_expr(high)?;
                let ge = self.alloc();
                self.push_compare(BinaryOp::GtEq, x, inner, lo, low, ge);
                let le = self.alloc();
                self.push_compare(BinaryOp::LtEq, x, inner, hi, high, le);
                if *negated {
                    let both = self.alloc();
                    self.ops.push(Op::And {
                        lhs: ge,
                        rhs: le,
                        dest: both,
                    });
                    self.ops.push(Op::Not { reg: both, dest });
                } else {
                    self.ops.push(Op::And {
                        lhs: ge,
                        rhs: le,
                        dest,
                    });
                }
                Ok(())
            }
            // `x IN (a, b, …)` is a three-valued OR-chain of `x = elem`: SQLite's
            // NULL rules fall out exactly (no match with a NULL element → NULL,
            // an empty list → 0). The negated form wraps the result in `NOT`.
            Expr::InList {
                expr: inner,
                list,
                negated,
                candidate_affinity,
            } => {
                let x = self.compile_expr(inner)?;
                // SQLite's `IN` collation rule: a single-element list behaves like
                // `x = elem` (the element's `COLLATE` applies); a multi-element list
                // uses the LEFT operand's collation for every comparison, ignoring
                // any per-element `COLLATE`.
                let in_coll = if list.len() == 1 {
                    self.compare_collation(inner, &list[0])
                } else {
                    self.col_collation(inner).unwrap_or_default()
                };
                // When the router folded a bare-column `x IN (SELECT col)`, the
                // candidate side contributes the SELECTed column's affinity (carried
                // as a canonical type name). Use it as EVERY element's right-operand
                // comparison affinity instead of the literal's own NONE affinity, so
                // `Op::Compare` applies `combine(left_aff, col_aff)` — exactly the
                // `IN (SELECT)` semantics, which a plain `IN (list)` would not get.
                let cand_ra = candidate_affinity
                    .as_deref()
                    .map(|t| Affinity::from_type(Some(t)));
                // Accumulate the running OR into `acc`, seeded with 0 (false) so an
                // empty list yields 0.
                let acc = self.alloc();
                self.ops.push(Op::Integer {
                    value: 0,
                    dest: acc,
                });
                for elem in list {
                    let e = self.compile_expr(elem)?;
                    let eq = self.alloc();
                    // `x IN (list)` applies ONLY the left operand's affinity to each
                    // element (the element's own affinity is ignored), so the
                    // right-operand affinity is `cand_ra` — the folded candidate
                    // column's affinity, or `None` for an ordinary list. Mirrors the
                    // tree-walker's `eval_in` and SQLite.
                    self.push_compare_coll_ra(BinaryOp::Eq, x, inner, e, cand_ra, eq, in_coll);
                    let next = self.alloc();
                    self.ops.push(Op::Or {
                        lhs: acc,
                        rhs: eq,
                        dest: next,
                    });
                    self.ops.push(Op::Copy {
                        src: next,
                        dest: acc,
                    });
                }
                if *negated {
                    self.ops.push(Op::Not { reg: acc, dest });
                } else {
                    self.ops.push(Op::Copy { src: acc, dest });
                }
                Ok(())
            }
            // A pure, context-free scalar function call: evaluate each argument
            // into a contiguous register block and emit `Op::Func`, which defers
            // to the tree-walker's `eval_scalar`. Restricted to the whitelist in
            // `is_pure_scalar_fn` so functions that read row/connection state
            // (random, last_insert_rowid, date('now'), UDFs, …) fall back.
            Expr::Function {
                name,
                distinct,
                args,
                star,
                filter,
                order_by,
                over,
            } if !*distinct
                && !*star
                && filter.is_none()
                && order_by.is_empty()
                && over.is_none()
                && is_pure_scalar_fn(name, args.len()) =>
            {
                // An explicit `COLLATE` on a top-level argument changes the value's
                // comparison semantics, which `Op::Func` (it round-trips each arg
                // through a reconstructed literal into `eval_scalar`) cannot carry —
                // a function that compares its args (`nullif`, `min`/`max`) would
                // ignore it. Defer such calls to the tree-walker.
                if args.iter().any(|a| self.explicit_collation(a).is_some()) {
                    return Err(Error::Unsupported("VDBE: COLLATE in a function argument"));
                }
                // Reserve a contiguous block for the arguments, then compile each
                // argument directly into its slot (sub-expressions may allocate
                // further temps after the block, which is fine).
                let arg_start = self.next_reg;
                for _ in 0..args.len() {
                    self.alloc();
                }
                for (i, a) in args.iter().enumerate() {
                    self.compile_expr_into(a, arg_start + i)?;
                }
                self.ops.push(Op::Func {
                    name: name.clone(),
                    arg_start,
                    arg_count: args.len(),
                    dest,
                });
                Ok(())
            }
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
                    // operand_reg is Some exactly when `operand` is Some.
                    self.push_compare(BinaryOp::Eq, oreg, operand.unwrap(), wreg, when, c);
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
    run_rows_multi(program, &[table_rows])
}

/// Run a compiled program over several cursors' materialized row-sets (the
/// nested-loop join path, B5b): `rowsets[i]` is cursor `i`'s rows. Cursor 0 also
/// backs the single-cursor `Rewind`/`Column`/`Next` opcodes, so the single-table
/// entry point [`run_rows`] is just this with one row-set.
pub fn run_rows_multi(program: &Program, rowsets: &[&[Vec<Value>]]) -> Result<Vec<Vec<Value>>> {
    let mut regs: Vec<Value> = alloc::vec![Value::Null; program.n_registers];
    let mut out = Vec::new();
    let table_rows: &[Vec<Value>] = rowsets.first().copied().unwrap_or(&[]);
    let mut cursor: usize = 0; // index of the current row (single-cursor ops)
                               // Per-cursor positions for the multi-cursor (nested-loop join) ops. A program
                               // uses either the single-cursor ops or the multi-cursor ops, never both.
    let mut positions: Vec<usize> = alloc::vec![0; rowsets.len().max(1)];
    // Per-cursor "this is the NULL row" flags for LEFT JOIN null-padding; set by
    // `NullRow`, cleared by `RewindC`.
    let mut null_flags: Vec<bool> = alloc::vec![false; rowsets.len().max(1)];
    // Per-cursor per-row "matched" bitmaps for the FULL JOIN anti-join pass; set
    // by `MarkMatched`, tested by `IfMatched`, and NOT cleared by `RewindC` (they
    // persist from the first pass into the second).
    let mut matched_rows: Vec<Vec<bool>> = rowsets
        .iter()
        .map(|rs| alloc::vec![false; rs.len()])
        .collect();
    if matched_rows.is_empty() {
        matched_rows.push(Vec::new());
    }
    // The sorter holds `(keys, row)` pairs staged by `SorterInsert`, sorted in
    // place by `SorterSort`, then walked by `SorterRewind`/`SorterNext`.
    let mut sorter: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    let mut scursor: usize = 0;
    // Rows already emitted under DISTINCT (NULLs compare equal here).
    let mut seen: Vec<Vec<Value>> = Vec::new();
    // Aggregate accumulators, one per slot: `(collected non-NULL values, row
    // count for count(*))`.
    let mut agg: Vec<AggAcc> = Vec::new();
    // GROUP BY state: each group is `(key values, per-aggregate accumulators)`,
    // kept in first-seen order.
    let mut groups: Vec<Group> = Vec::new();
    // Finalized groups for the HAVING/ORDER BY emit loop: `(key values, aggregate
    // finals)` per group, walked by `GroupFinalize`/`GroupNext`.
    let mut emit_groups: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    let mut gcursor: usize = 0;
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
            Op::RewindC { cursor: c, target } => {
                positions[*c] = 0;
                null_flags[*c] = false;
                if rowsets.get(*c).is_none_or(|rs| rs.is_empty()) {
                    pc = *target;
                }
            }
            Op::ColumnC {
                cursor: c,
                col,
                dest,
            } => {
                regs[*dest] = if null_flags[*c] {
                    Value::Null
                } else {
                    rowsets
                        .get(*c)
                        .and_then(|rs| rs.get(positions[*c]))
                        .and_then(|r| r.get(*col))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
            }
            Op::NullRow { cursor: c } => {
                null_flags[*c] = true;
            }
            Op::MarkMatched { cursor: c } => {
                if let Some(row) = matched_rows
                    .get_mut(*c)
                    .and_then(|m| m.get_mut(positions[*c]))
                {
                    *row = true;
                }
            }
            Op::IfMatched { cursor: c, target } => {
                if matched_rows
                    .get(*c)
                    .and_then(|m| m.get(positions[*c]))
                    .copied()
                    .unwrap_or(false)
                {
                    pc = *target;
                }
            }
            Op::NextC { cursor: c, target } => {
                positions[*c] += 1;
                if positions[*c] < rowsets.get(*c).map_or(0, |rs| rs.len()) {
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
            Op::Blob { value, dest } => regs[*dest] = Value::Blob(value.clone()),
            Op::Null { dest } => regs[*dest] = Value::Null,
            Op::Negate { reg, dest } => {
                regs[*dest] = match crate::exec::eval::to_number(&regs[*reg]) {
                    // Negating `i64::MIN` overflows; SQLite promotes it to a real
                    // (matching the tree-walker), rather than wrapping.
                    Value::Integer(i) => i
                        .checked_neg()
                        .map(Value::Integer)
                        .unwrap_or(Value::Real(-(i as f64))),
                    Value::Real(r) => Value::Real(-r),
                    _ => Value::Null,
                };
            }
            Op::BitNot { reg, dest } => {
                regs[*dest] = match &regs[*reg] {
                    Value::Null => Value::Null,
                    v => Value::Integer(!crate::exec::eval::to_i64(v)),
                };
            }
            Op::Arith { op, lhs, rhs, dest } => {
                regs[*dest] = crate::exec::eval::arithmetic_values(*op, &regs[*lhs], &regs[*rhs]);
            }
            Op::Bitwise { op, lhs, rhs, dest } => {
                regs[*dest] = crate::exec::eval::bitwise_values(*op, &regs[*lhs], &regs[*rhs]);
            }
            Op::Is {
                is,
                lhs,
                rhs,
                dest,
                la,
                ra,
            } => {
                let (l, r) = crate::exec::eval::apply_comparison_affinity(
                    regs[*lhs].clone(),
                    *la,
                    regs[*rhs].clone(),
                    *ra,
                );
                regs[*dest] = crate::exec::eval::is_values(*is, &l, &r);
            }
            Op::Truthy {
                want,
                not,
                operand,
                dest,
            } => {
                let t = crate::exec::eval::truth(&regs[*operand]) == Some(*want);
                regs[*dest] = Value::Integer((t ^ *not) as i64);
            }
            Op::Like {
                glob,
                lhs,
                rhs,
                dest,
            } => {
                regs[*dest] = crate::exec::eval::like_glob_values(*glob, &regs[*lhs], &regs[*rhs]);
            }
            Op::Json {
                as_text,
                lhs,
                rhs,
                dest,
            } => {
                regs[*dest] = crate::exec::json::arrow(&regs[*lhs], &regs[*rhs], *as_text)?;
            }
            Op::Func {
                name,
                arg_start,
                arg_count,
                dest,
            } => {
                // Reconstruct literal argument expressions from the evaluated
                // registers and run them through the tree-walker's scalar-function
                // evaluator with an empty (row-less) context. The compiler only
                // emits this op for pure, context-free functions, so the missing
                // row/connection state is never consulted.
                use crate::sql::ast::{Expr, Literal};
                let lit_args: Vec<Expr> = regs[*arg_start..*arg_start + *arg_count]
                    .iter()
                    .map(|v| {
                        Expr::Literal(match v {
                            Value::Null => Literal::Null,
                            Value::Integer(i) => Literal::Integer(*i),
                            Value::Real(r) => Literal::Real(*r),
                            Value::Text(s) => Literal::Str(s.clone()),
                            Value::Blob(b) => Literal::Blob(b.clone()),
                        })
                    })
                    .collect();
                let params = crate::exec::eval::Params::default();
                let ctx = crate::exec::eval::EvalCtx::rowless(&params);
                regs[*dest] = crate::exec::func::eval_scalar(name, &lit_args, false, &ctx)?;
            }
            Op::Concat { lhs, rhs, dest } => {
                regs[*dest] = crate::exec::eval::concat_values(&regs[*lhs], &regs[*rhs]);
            }
            Op::Compare {
                op,
                lhs,
                rhs,
                dest,
                la,
                ra,
                coll,
            } => {
                // Apply SQLite's pre-comparison affinity to the operands (exactly
                // as the tree-walker does) before comparing under the resolved
                // collating sequence.
                let (l, r) = crate::exec::eval::apply_comparison_affinity(
                    regs[*lhs].clone(),
                    *la,
                    regs[*rhs].clone(),
                    *ra,
                );
                regs[*dest] = crate::exec::eval::compare_op(*op, &l, &r, *coll);
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
                            k.collation,
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
            Op::AggStep {
                slot,
                kind,
                arg,
                distinct,
            } => {
                if *slot >= agg.len() {
                    agg.resize(*slot + 1, (Vec::new(), 0));
                }
                if *kind == AggKind::CountStar {
                    agg[*slot].1 += 1;
                } else if let Some(r) = arg {
                    if !matches!(regs[*r], Value::Null) {
                        let v = regs[*r].clone();
                        // A DISTINCT aggregate folds each distinct argument value
                        // once: skip a value already collected for this slot
                        // (BINARY equality — the non-BINARY case bails to the
                        // tree-walker before compilation).
                        if !*distinct || !agg[*slot].0.iter().any(|p| distinct_eq(p, &v)) {
                            agg[*slot].0.push(v);
                        }
                    }
                }
            }
            Op::AggFinal { slot, kind, dest } => {
                let (vals, star) = match agg.get_mut(*slot) {
                    Some(e) => core::mem::take(e),
                    None => (Vec::new(), 0),
                };
                regs[*dest] = finalize_agg(*kind, vals, star)?;
            }
            Op::GroupStep {
                key_start,
                key_count,
                aggs,
            } => {
                let key = regs[*key_start..*key_start + *key_count].to_vec();
                let gi = match groups.iter().position(|(k, _)| {
                    k.len() == key.len() && k.iter().zip(&key).all(|(a, b)| distinct_eq(a, b))
                }) {
                    Some(i) => i,
                    None => {
                        groups.push((key, alloc::vec![(Vec::new(), 0); aggs.len()]));
                        groups.len() - 1
                    }
                };
                for (j, spec) in aggs.iter().enumerate() {
                    if spec.kind == AggKind::CountStar {
                        groups[gi].1[j].1 += 1;
                    } else if let Some(r) = spec.arg {
                        if !matches!(regs[r], Value::Null) {
                            let v = regs[r].clone();
                            // A DISTINCT aggregate folds each distinct argument
                            // value once per group (BINARY equality; non-BINARY
                            // bails before compilation).
                            if !spec.distinct
                                || !groups[gi].1[j].0.iter().any(|p| distinct_eq(p, &v))
                            {
                                groups[gi].1[j].0.push(v);
                            }
                        }
                    }
                }
            }
            Op::GroupEmit { outputs, agg_kinds } => {
                for (key, accs) in groups.drain(..) {
                    let finals: Vec<Value> = agg_kinds
                        .iter()
                        .zip(accs)
                        .map(|(k, (vals, star))| finalize_agg(*k, vals, star))
                        .collect::<Result<_>>()?;
                    let row: Vec<Value> = outputs
                        .iter()
                        .map(|o| match o {
                            GroupOut::Key(i) => key[*i].clone(),
                            GroupOut::Agg(j) => finals[*j].clone(),
                        })
                        .collect();
                    out.push(row);
                }
            }
            Op::GroupFinalize { agg_kinds, target } => {
                // Finalize each group's aggregates once, into `emit_groups`, in
                // first-seen order; position the group cursor at the first.
                emit_groups.clear();
                for (key, accs) in groups.drain(..) {
                    let finals: Vec<Value> = agg_kinds
                        .iter()
                        .zip(accs)
                        .map(|(k, (vals, star))| finalize_agg(*k, vals, star))
                        .collect::<Result<_>>()?;
                    emit_groups.push((key, finals));
                }
                gcursor = 0;
                if emit_groups.is_empty() {
                    pc = *target;
                }
            }
            Op::GroupKey { key, dest } => {
                regs[*dest] = emit_groups
                    .get(gcursor)
                    .and_then(|(k, _)| k.get(*key))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            Op::GroupAgg { slot, dest } => {
                regs[*dest] = emit_groups
                    .get(gcursor)
                    .and_then(|(_, a)| a.get(*slot))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            Op::GroupNext { target } => {
                gcursor += 1;
                if gcursor < emit_groups.len() {
                    pc = *target;
                }
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
fn finalize_agg(kind: AggKind, vals: Vec<Value>, star: i64) -> Result<Value> {
    use crate::exec::eval;
    use core::cmp::Ordering;
    Ok(match kind {
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
                    // Match SQLite: integer `sum()` overflow is an error.
                    return Err(Error::Error("integer overflow".into()));
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
    })
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

    #[test]
    fn nested_loop_join_two_cursors() {
        // `compile_join2` reads the predicate from WHERE (the caller merges ON into
        // it) and ignores the FROM clause, using the passed column metadata. Two
        // cursors, left outermost.
        let Statement::Select(sel) =
            parse_one("SELECT a.x, b.q FROM a, b WHERE a.x = b.p").unwrap()
        else {
            panic!()
        };
        let columns = vec![
            "x".to_string(),
            "y".to_string(),
            "p".to_string(),
            "q".to_string(),
        ];
        let tables = vec![
            "a".to_string(),
            "a".to_string(),
            "b".to_string(),
            "b".to_string(),
        ];
        let aff = vec![Affinity::Blob; 4];
        let coll = vec![Collation::Binary; 4];
        let prog = compile_join2(&sel, &columns, &tables, &aff, &coll, &[2, 4]).unwrap();
        let left: Vec<Vec<Value>> = vec![
            vec![Value::Integer(1), Value::Text("a".into())],
            vec![Value::Integer(2), Value::Text("b".into())],
        ];
        let right: Vec<Vec<Value>> = vec![
            vec![Value::Integer(1), Value::Text("P".into())],
            vec![Value::Integer(2), Value::Text("Q".into())],
            vec![Value::Integer(2), Value::Text("R".into())],
        ];
        // Every right row per left row, left outermost — same order as the
        // cross-product, filtered by `a.x = b.p`.
        assert_eq!(
            run_rows_multi(&prog, &[&left, &right]).unwrap(),
            vec![
                vec![Value::Integer(1), Value::Text("P".into())],
                vec![Value::Integer(2), Value::Text("Q".into())],
                vec![Value::Integer(2), Value::Text("R".into())],
            ]
        );
        assert_eq!(prog.columns.len(), 2);
        // An empty inner or outer table yields no rows (and does not panic).
        assert!(run_rows_multi(&prog, &[&left, &[]]).unwrap().is_empty());
        assert!(run_rows_multi(&prog, &[&[], &right]).unwrap().is_empty());
    }

    #[test]
    fn left_join_null_pads_unmatched_rows() {
        let Statement::Select(sel) =
            parse_one("SELECT a.x, b.q FROM a LEFT JOIN b ON a.x = b.p").unwrap()
        else {
            panic!()
        };
        let on = sel.from.as_ref().unwrap().joins[0].on.clone();
        let columns = vec![
            "x".to_string(),
            "y".to_string(),
            "p".to_string(),
            "q".to_string(),
        ];
        let tables = vec![
            "a".to_string(),
            "a".to_string(),
            "b".to_string(),
            "b".to_string(),
        ];
        let aff = vec![Affinity::Blob; 4];
        let coll = vec![Collation::Binary; 4];
        let prog = compile_left_join2(&sel, &columns, &tables, &aff, &coll, 2, &on).unwrap();
        let left: Vec<Vec<Value>> = vec![
            vec![Value::Integer(1), Value::Text("a".into())],
            vec![Value::Integer(2), Value::Text("b".into())],
            vec![Value::Integer(3), Value::Text("c".into())],
        ];
        let right: Vec<Vec<Value>> = vec![
            vec![Value::Integer(1), Value::Text("P".into())],
            vec![Value::Integer(2), Value::Text("Q".into())],
        ];
        // x=1→P, x=2→Q, x=3 has no match → one null-padded row.
        assert_eq!(
            run_rows_multi(&prog, &[&left, &right]).unwrap(),
            vec![
                vec![Value::Integer(1), Value::Text("P".into())],
                vec![Value::Integer(2), Value::Text("Q".into())],
                vec![Value::Integer(3), Value::Null],
            ]
        );
        // An empty right side null-pads every left row.
        assert_eq!(
            run_rows_multi(&prog, &[&left, &[]]).unwrap(),
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Integer(2), Value::Null],
                vec![Value::Integer(3), Value::Null],
            ]
        );
    }

    #[test]
    fn full_join_two_pass_null_pads_both_sides() {
        let Statement::Select(sel) =
            parse_one("SELECT a.x, b.p FROM a FULL JOIN b ON a.x = b.p").unwrap()
        else {
            panic!()
        };
        let on = sel.from.as_ref().unwrap().joins[0].on.clone();
        let columns = vec!["x".to_string(), "p".to_string()];
        let tables = vec!["a".to_string(), "b".to_string()];
        let aff = vec![Affinity::Blob; 2];
        let coll = vec![Collation::Binary; 2];
        let prog = compile_full_join2(&sel, &columns, &tables, &aff, &coll, 1, &on).unwrap();
        let left: Vec<Vec<Value>> = vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ];
        let right: Vec<Vec<Value>> = vec![
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)],
        ];
        // Pass 1 (left order): 1→null, 2→2, 3→3. Pass 2 (unmatched right): 4.
        assert_eq!(
            run_rows_multi(&prog, &[&left, &right]).unwrap(),
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Integer(2), Value::Integer(2)],
                vec![Value::Integer(3), Value::Integer(3)],
                vec![Value::Null, Value::Integer(4)],
            ]
        );
        // Empty left → every right row in pass 2 with left NULL.
        assert_eq!(
            run_rows_multi(&prog, &[&[], &right]).unwrap(),
            vec![
                vec![Value::Null, Value::Integer(2)],
                vec![Value::Null, Value::Integer(3)],
                vec![Value::Null, Value::Integer(4)],
            ]
        );
    }

    #[test]
    fn nested_loop_join_bails_on_aggregate_and_order_by() {
        let cols = vec!["x".to_string(), "p".to_string()];
        let tabs = vec!["a".to_string(), "b".to_string()];
        let aff = vec![Affinity::Blob; 2];
        let coll = vec![Collation::Binary; 2];
        for sql in [
            "SELECT count(*) FROM a, b",
            "SELECT a.x FROM a, b GROUP BY a.x",
            "SELECT a.x, count(*) FROM a, b GROUP BY a.x HAVING count(*) > 1",
        ] {
            let Statement::Select(sel) = parse_one(sql).unwrap() else {
                panic!()
            };
            assert!(
                compile_join2(&sel, &cols, &tabs, &aff, &coll, &[1, 2]).is_err(),
                "{sql} should bail to the cross-product path"
            );
        }
        // DISTINCT and ORDER BY over a join ARE supported now.
        for sql in [
            "SELECT DISTINCT a.x FROM a, b",
            "SELECT a.x FROM a, b ORDER BY a.x",
            "SELECT DISTINCT a.x FROM a, b ORDER BY a.x DESC LIMIT 2",
        ] {
            let Statement::Select(sel) = parse_one(sql).unwrap() else {
                panic!()
            };
            assert!(
                compile_join2(&sel, &cols, &tabs, &aff, &coll, &[1, 2]).is_ok(),
                "{sql} should compile on the VDBE"
            );
        }
    }

    #[test]
    fn bare_aggregate_join_compiles_and_bails_correctly() {
        let cols = vec!["x".to_string(), "p".to_string()];
        let tabs = vec!["a".to_string(), "b".to_string()];
        let aff = vec![Affinity::Blob; 2];
        let coll = vec![Collation::Binary; 2];
        // Bare aggregates (no GROUP BY) compile through the nested-loop fold.
        for sql in [
            "SELECT count(*) FROM a, b",
            "SELECT sum(a.x), max(b.p) FROM a, b",
            "SELECT group_concat(b.p) FROM a, b WHERE a.x = b.p",
        ] {
            let Statement::Select(sel) = parse_one(sql).unwrap() else {
                panic!()
            };
            assert!(
                compile_aggregate_join(&sel, &cols, &tabs, &aff, &coll, &[1, 2]).is_ok(),
                "{sql} should compile as an aggregate join"
            );
        }
        // Shapes outside the bare-aggregate grammar bail to the caller.
        for sql in [
            "SELECT a.x FROM a, b",                   // not an aggregate
            "SELECT count(*) FROM a, b GROUP BY a.x", // GROUP BY
            "SELECT count(*) FROM a, b ORDER BY 1",   // ORDER BY
            "SELECT count(*) FROM a, b LIMIT 1",      // LIMIT
            "SELECT count(DISTINCT a.x) FROM a, b",   // DISTINCT aggregate
        ] {
            let Statement::Select(sel) = parse_one(sql).unwrap() else {
                panic!()
            };
            assert!(
                compile_aggregate_join(&sel, &cols, &tabs, &aff, &coll, &[1, 2]).is_err(),
                "{sql} should bail from the aggregate-join path"
            );
        }
    }

    #[test]
    fn group_by_join_compiles_and_bails_correctly() {
        let cols = vec!["x".to_string(), "p".to_string()];
        let tabs = vec!["a".to_string(), "b".to_string()];
        let aff = vec![Affinity::Blob; 2];
        let coll = vec![Collation::Binary; 2];
        // The full grouped grammar over a join compiles: plain (key + aggregate),
        // plus the general HAVING / ORDER BY / LIMIT path (shared with the
        // single-table scan compiler via `cursor_boundaries`).
        for sql in [
            "SELECT a.x, count(*) FROM a, b GROUP BY a.x",
            "SELECT x, sum(p) FROM a, b GROUP BY x",
            "SELECT a.x FROM a, b GROUP BY a.x",
            "SELECT a.x, count(*) FROM a, b GROUP BY a.x HAVING count(*) > 1",
            "SELECT a.x, count(*) FROM a, b GROUP BY a.x ORDER BY a.x",
            "SELECT a.x, count(*) FROM a, b GROUP BY a.x LIMIT 1",
            "SELECT a.x, count(*) AS n FROM a, b GROUP BY a.x ORDER BY n DESC LIMIT 2 OFFSET 1",
        ] {
            let Statement::Select(sel) = parse_one(sql).unwrap() else {
                panic!()
            };
            assert!(
                compile_group_join(&sel, &cols, &tabs, &aff, &coll, &[1, 2]).is_ok(),
                "{sql} should compile as a GROUP BY join"
            );
        }
        // DISTINCT / a non-grouped output / no GROUP BY still bail.
        for sql in [
            "SELECT DISTINCT a.x, count(*) FROM a, b GROUP BY a.x",
            "SELECT a.x, b.p FROM a, b GROUP BY a.x", // b.p not grouped/aggregated
            "SELECT count(*) FROM a, b",              // no GROUP BY
        ] {
            let Statement::Select(sel) = parse_one(sql).unwrap() else {
                panic!()
            };
            assert!(
                compile_group_join(&sel, &cols, &tabs, &aff, &coll, &[1, 2]).is_err(),
                "{sql} should bail from the GROUP BY join path"
            );
        }
    }
}
