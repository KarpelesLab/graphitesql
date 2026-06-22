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

/// One `ORDER BY` key for [`Op::SorterSort`]: direction and NULL placement
/// (collation is binary in the VDBE scan path).
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// `DESC` when true.
    pub descending: bool,
    /// Explicit `NULLS FIRST`/`LAST`; `None` uses SQLite's default.
    pub nulls_first: Option<bool>,
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
        // JSON (operate purely on their value arguments).
        "json" | "json_valid" | "json_type" | "json_quote" | "json_array_length"
        | "json_extract" | "json_array" | "json_object" | "jsonb" => true,
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
        bindings: Vec::new(),
    };
    let mut columns = Vec::new();
    for (i, rc) in sel.columns.iter().enumerate() {
        let ResultColumn::Expr { expr, alias, .. } = rc else {
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
/// Strip enclosing parentheses from an expression.
fn unparen(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(inner) => unparen(inner),
        _ => e,
    }
}

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
    tables: &[String],
    affinities: &[Affinity],
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
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        bindings: Vec::new(),
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

/// Compile `SELECT <group cols / aggregates> FROM <table> [WHERE …] GROUP BY
/// <cols> [HAVING …] [ORDER BY …] [LIMIT/OFFSET]`. Each grouping key must be a
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
    projections: &[(Expr, String)],
) -> Result<Program> {
    if sel.distinct {
        return Err(Error::Unsupported("VDBE: GROUP BY + DISTINCT"));
    }
    let has_having = sel.having.is_some();
    let has_order = !sel.order_by.is_empty();
    let has_limit = sel.limit.is_some() || sel.offset.is_some();
    // Resolve each grouping key to a table column index (bare columns only).
    let col_index = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let mut group_cols: Vec<usize> = Vec::new();
    for g in &sel.group_by {
        match g {
            Expr::Column { column, .. } => match col_index(column) {
                Some(i) => group_cols.push(i),
                None => return Err(Error::Unsupported("VDBE: GROUP BY unknown column")),
            },
            _ => return Err(Error::Unsupported("VDBE: GROUP BY column refs only")),
        }
    }

    // The plain path (no HAVING / ORDER BY / LIMIT) keeps its compact `GroupEmit`.
    if !has_having && !has_order && !has_limit {
        return compile_group_emit(sel, columns, tables, affinities, projections, &group_cols);
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
    // Each aggregate must be a shape the VDBE can fold.
    let mut agg_specs: Vec<(AggKind, Option<Expr>)> = Vec::new();
    for e in &agg_exprs {
        match agg_kind(e) {
            Some(spec) => agg_specs.push(spec),
            None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
        }
    }

    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: 0,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        bindings: Vec::new(),
    };
    // Contiguous key registers, loaded per row from the grouping columns.
    let key_start = c.next_reg;
    for _ in &group_cols {
        c.alloc();
    }
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
    for (k, &ci) in group_cols.iter().enumerate() {
        c.ops.push(Op::Column {
            col: ci,
            dest: key_start + k,
        });
    }
    // Evaluate each aggregate argument into a register for this row.
    let mut aggs: Vec<AggSpec> = Vec::new();
    for (kind, arg) in &agg_specs {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        aggs.push(AggSpec {
            kind: *kind,
            arg: arg_reg,
        });
    }
    c.ops.push(Op::GroupStep {
        key_start,
        key_count: group_cols.len(),
        aggs,
    });
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
        Some(Expr::Literal(Literal::Integer(n))) => {
            let r = c.alloc();
            c.ops.push(Op::Integer { value: *n, dest: r });
            Some(r)
        }
        Some(_) => return Err(Error::Unsupported("VDBE: only constant integer LIMIT")),
    };
    let offset_reg = match &sel.offset {
        None => None,
        Some(Expr::Literal(Literal::Integer(n))) => {
            let r = c.alloc();
            c.ops.push(Op::Integer { value: *n, dest: r });
            Some(r)
        }
        Some(_) => return Err(Error::Unsupported("VDBE: only constant integer OFFSET")),
    };

    // Resolve each ORDER BY term to an output-column expression where it is a
    // bare ordinal or output alias (mirroring the scan path / tree-walker).
    let mut key_specs: Vec<(Expr, SortKey)> = Vec::new();
    for term in &sel.order_by {
        let expr = match &term.expr {
            Expr::Literal(Literal::Integer(k)) if *k >= 1 && (*k as usize) <= count => {
                projections[*k as usize - 1].0.clone()
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
        key_specs.push((
            expr,
            SortKey {
                descending: term.descending,
                nulls_first: term.nulls_first,
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
        agg_kinds: agg_specs.iter().map(|(k, _)| *k).collect(),
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
fn compile_group_emit(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    projections: &[(Expr, String)],
    group_cols: &[usize],
) -> Result<Program> {
    let col_index = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    // Classify each output column as a grouping-key reference or an aggregate.
    let mut outputs: Vec<GroupOut> = Vec::new();
    let mut agg_specs: Vec<(AggKind, Option<Expr>)> = Vec::new();
    for (e, _) in projections {
        if is_aggregate_expr(e) {
            match agg_kind(e) {
                Some(spec) => {
                    outputs.push(GroupOut::Agg(agg_specs.len()));
                    agg_specs.push(spec);
                }
                None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
            }
        } else if let Expr::Column { column, .. } = e {
            // Must be one of the grouping columns (by table-column index).
            match col_index(column).and_then(|ci| group_cols.iter().position(|&g| g == ci)) {
                Some(k) => outputs.push(GroupOut::Key(k)),
                None => return Err(Error::Unsupported("VDBE: non-grouped column")),
            }
        } else {
            return Err(Error::Unsupported(
                "VDBE: GROUP BY output must be key or aggregate",
            ));
        }
    }

    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: 0,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        bindings: Vec::new(),
    };
    // Contiguous key registers, loaded per row from the grouping columns.
    let key_start = c.next_reg;
    for _ in group_cols {
        c.alloc();
    }
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
    for (k, &ci) in group_cols.iter().enumerate() {
        c.ops.push(Op::Column {
            col: ci,
            dest: key_start + k,
        });
    }
    // Evaluate each aggregate argument into a register for this row.
    let mut aggs: Vec<AggSpec> = Vec::new();
    for (kind, arg) in &agg_specs {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        aggs.push(AggSpec {
            kind: *kind,
            arg: arg_reg,
        });
    }
    c.ops.push(Op::GroupStep {
        key_start,
        key_count: group_cols.len(),
        aggs,
    });
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
    c.ops.push(Op::GroupEmit {
        outputs,
        agg_kinds: agg_specs.iter().map(|(k, _)| *k).collect(),
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
) -> Result<Program> {
    if !sel.compound.is_empty() {
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
            ResultColumn::Expr { expr, alias, .. } => {
                let label = alias.clone().unwrap_or_else(|| match expr {
                    Expr::Column { column, .. } => column.clone(),
                    _ => alloc::format!("col{}", projections.len() + 1),
                });
                projections.push((expr.clone(), label));
            }
            // In a single-table scan the only valid `t.*` qualifier is this
            // table (the caller verifies the name matches before using the VDBE),
            // so it expands to every column exactly like a bare `*`.
            ResultColumn::TableWildcard(_) => {
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
        }
    }
    if projections.is_empty() {
        return Err(Error::Unsupported("VDBE: empty projection"));
    }
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
        return compile_group_select(sel, columns, tables, affinities, &projections);
    }
    // An all-aggregate projection (no GROUP BY) folds the scan into one row.
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return compile_aggregate_select(sel, columns, tables, affinities, &projections);
    }
    let count = projections.len();
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: count,
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        bindings: Vec::new(),
    };
    // Optional LIMIT (constant integer only): a counter register decremented
    // after each emitted row, halting the loop at zero.
    let limit_reg = match &sel.limit {
        None => None,
        // A negative LIMIT means "unlimited" in SQLite.
        Some(Expr::Literal(Literal::Integer(n))) if *n < 0 => None,
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
    /// Each column's owning-table qualifier (alias if present, else table name),
    /// parallel to `columns`. Empty for a constant `SELECT`. Used to resolve a
    /// qualified `t.col` reference and to disambiguate a join's shared names.
    tables: Vec<String>,
    /// Each column's comparison affinity, parallel to `columns` (empty for a
    /// constant `SELECT` with no table). Used to apply SQLite's pre-comparison
    /// affinity in `Op::Compare`, matching the tree-walker.
    affinities: Vec<Affinity>,
    /// Expression → register overrides consulted before normal compilation.
    /// Used by the grouped `HAVING`/`ORDER BY` path to resolve aggregate calls
    /// and grouping-column references to per-group registers (so an arbitrary
    /// predicate / sort-key expression over them compiles to ordinary ops).
    bindings: Vec<(Expr, usize)>,
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
            _ => None,
        }
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
        let la = self.expr_affinity(left);
        let ra = self.expr_affinity(right);
        self.ops.push(Op::Compare {
            op,
            lhs,
            rhs,
            dest,
            la,
            ra,
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
                let idx = self.resolve_column(table.as_deref(), column)?;
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
                        // `x IS TRUE`/`IS FALSE` is a truthiness test, not value
                        // equality (SQLite special-cases a boolean-literal operand);
                        // bail so the tree-walker handles it.
                        let is_bool =
                            |e: &Expr| matches!(unparen(e), Expr::Literal(Literal::Boolean(_)));
                        if is_bool(left) || is_bool(right) {
                            return Err(Error::Unsupported("VDBE spike: IS TRUE/FALSE"));
                        }
                        self.ops.push(Op::Is {
                            is: matches!(op, Is),
                            lhs: l,
                            rhs: r,
                            dest,
                        });
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
            } => {
                let x = self.compile_expr(inner)?;
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
                    self.push_compare(BinaryOp::Eq, x, inner, e, elem, eq);
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
                    Value::Integer(i) => Value::Integer(i.wrapping_neg()),
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
            Op::Is { is, lhs, rhs, dest } => {
                regs[*dest] = crate::exec::eval::is_values(*is, &regs[*lhs], &regs[*rhs]);
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
                regs[*dest] = crate::exec::json::arrow(&regs[*lhs], &regs[*rhs], *as_text);
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
                regs[*dest] =
                    if matches!(regs[*lhs], Value::Null) || matches!(regs[*rhs], Value::Null) {
                        Value::Null
                    } else {
                        let mut s = crate::exec::eval::to_text(&regs[*lhs]);
                        s.push_str(&crate::exec::eval::to_text(&regs[*rhs]));
                        Value::Text(s)
                    };
            }
            Op::Compare {
                op,
                lhs,
                rhs,
                dest,
                la,
                ra,
            } => {
                // Apply SQLite's pre-comparison affinity to the operands (exactly
                // as the tree-walker does) before comparing.
                let (l, r) = crate::exec::eval::apply_comparison_affinity(
                    regs[*lhs].clone(),
                    *la,
                    regs[*rhs].clone(),
                    *ra,
                );
                regs[*dest] =
                    crate::exec::eval::compare_op(*op, &l, &r, crate::value::Collation::Binary);
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
                            groups[gi].1[j].0.push(v);
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
}
