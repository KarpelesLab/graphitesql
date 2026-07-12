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
use crate::sql::ast::{BinaryOp, Expr, JoinKind, Literal, OrderTerm, ResultColumn, Select, Span};
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
    /// `collations[i]` is the collating sequence for output column `i` (empty =>
    /// all `BINARY`); a `NOCASE`/`RTRIM`/custom column dedups under its collation.
    DistinctCheck {
        start: usize,
        count: usize,
        target: usize,
        collations: alloc::vec::Vec<Collation>,
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
    /// slot folds only over distinct argument values. When `filter` is set, a row
    /// whose predicate register is not true (`FILTER (WHERE …)`) contributes to
    /// neither the count nor the collected values. When `order` is non-empty
    /// (`group_concat(x ORDER BY …)`), each collected value also records its
    /// `ORDER BY` key row so finalization can sort before concatenating.
    AggStep {
        slot: usize,
        kind: AggKind,
        arg: Option<usize>,
        /// Second argument register — the value of `json_group_object`; `None`
        /// otherwise.
        arg2: Option<usize>,
        distinct: bool,
        filter: Option<usize>,
        order: Vec<AggOrderKey>,
        /// Constant `group_concat`/`string_agg` separator (`None` = default `,`).
        sep: Option<String>,
        /// The argument's collating sequence (declared column collation, else
        /// BINARY), driving the `DISTINCT` dedup and the `min`/`max` reduction.
        collation: Collation,
    },
    /// Finalize aggregate `slot` into `dest`.
    AggFinal {
        slot: usize,
        kind: AggKind,
        dest: usize,
    },
    /// `GROUP BY` fold: find-or-create the group for the key in `[key_start,
    /// key_start+key_count)` (first-seen order, NULLs group together) and step
    /// each per-group aggregate. The `repr_count` registers immediately after the
    /// keys hold bare-column "representatives" — captured from the row that first
    /// creates the group (first-seen semantics) and not compared for group
    /// identity; a later [`Op::GroupEmit`] reads them via `Key(key_count + r)`.
    ///
    /// `companion` switches the representatives to SQLite's min()/max() rule: when
    /// the query has exactly one min()/max() aggregate, bare columns take values
    /// from the row that holds that extreme. It is `Some((arg_reg, is_max))` — the
    /// register holding the governing aggregate's argument and whether it is max().
    /// The running extreme is kept in a hidden trailing slot of the key vector (at
    /// index `key_count + repr_count`); each row that beats it overwrites both the
    /// extreme and the representative slots.
    GroupStep {
        key_start: usize,
        key_count: usize,
        repr_count: usize,
        companion: Option<(usize, bool)>,
        aggs: Vec<AggSpec>,
        /// Collating sequence for each group key (`key_collations[i]` for key `i`;
        /// empty => all BINARY). A `NOCASE`/`RTRIM`/custom-collation `GROUP BY` key
        /// groups under its collation.
        key_collations: alloc::vec::Vec<Collation>,
        /// Collation of the min()/max() companion's argument (BINARY when there is
        /// no companion), so the extreme-row tracking compares text under it.
        companion_collation: Collation,
    },
    /// Emit one row per group, ordered by the GROUP BY keys (the first
    /// `key_count` slots of each group vector, BINARY ascending, NULLs first —
    /// SQLite groups via a sort): each output is either a group-key value or a
    /// finalized per-group aggregate.
    GroupEmit {
        outputs: Vec<GroupOut>,
        key_count: usize,
        /// Group-key collations (parallel to the first `key_count` slots; empty =>
        /// all BINARY), so the group emit order matches SQLite's collation-sorted
        /// grouping.
        key_collations: alloc::vec::Vec<Collation>,
        agg_kinds: Vec<AggKind>,
        /// Source-column index of each group key (parallel to the first
        /// `key_count` slots of every group's key vector), used to place the
        /// group's key values into the synthetic row a [`GroupOut::Sub`] /
        /// [`GroupOut::SubExists`] correlated subquery is evaluated against.
        /// Empty unless the projection holds such a subquery.
        group_cols: Vec<usize>,
        /// Width of that synthetic row (the number of source columns), so an
        /// outer reference resolves to the same index the compiler assigned.
        n_cols: usize,
    },
    /// Finalize the accumulated groups (computing each slot's value per group)
    /// into an emit list ordered by the GROUP BY keys (the first `key_count`
    /// slots, BINARY ascending, NULLs first), then position the group cursor at
    /// the first group; jump to `target` (the emit-loop exit) when there are no
    /// groups. Used by the `HAVING`/`ORDER BY` grouped path, where each group's
    /// keys and aggregates are loaded into registers so arbitrary predicates /
    /// sort keys can be computed by ordinary ops. (A later explicit `ORDER BY`
    /// re-sorts the output; this key order is what SQLite emits without one.)
    GroupFinalize {
        agg_kinds: Vec<AggKind>,
        key_count: usize,
        target: usize,
        /// Group-key collations (parallel to the first `key_count` slots; empty =>
        /// all BINARY), for the collation-sorted group emit order.
        key_collations: alloc::vec::Vec<Collation>,
    },
    /// Load group-key value at index `key` of the current group into `dest`.
    GroupKey { key: usize, dest: usize },
    /// Load the finalized aggregate at `slot` of the current group into `dest`.
    GroupAgg { slot: usize, dest: usize },
    /// Advance the group cursor; jump back to `target` if a group remains.
    GroupNext { target: usize },
    /// Evaluate a *correlated* scalar subquery (`(SELECT … WHERE inner = outer)`)
    /// against the current cursor-0 row, storing its single-column value (or NULL
    /// for an empty result) in `dest`. `sub` indexes [`Program::subqueries`]. The
    /// interpreter re-runs the subquery through the executor's tree-walker with the
    /// current outer row bound as an outer frame (via the [`SubqueryEval`]
    /// callback), so the value matches the tree-walker and SQLite exactly. Only
    /// emitted for a live single-table scan (which supplies the callback).
    CorrelatedScalar { sub: usize, dest: usize },
    /// Evaluate a *correlated* `[NOT] EXISTS (SELECT … WHERE inner = outer)`
    /// against the current cursor-0 row, storing `1`/`0` in `dest` (inverted when
    /// `negated`). Same callback mechanism as [`Op::CorrelatedScalar`].
    CorrelatedExists {
        sub: usize,
        negated: bool,
        dest: usize,
    },
    /// Like [`Op::CorrelatedScalar`], but emitted in the *general* grouped path's
    /// second-pass body (over finalized groups, no scan row). The interpreter
    /// builds a synthetic outer row from the current group's key values — placed at
    /// their source-column positions (`Compiler::group_emit_keys`, captured in the
    /// op), all else NULL — and evaluates the subquery against it. Admitted only
    /// when every outer reference is a group key, so that synthetic row is complete.
    GroupCorrelatedScalar {
        sub: usize,
        dest: usize,
        /// Source-column index of each group key (parallel to the group's key
        /// vector); the key value is placed here in the synthetic row.
        group_cols: Vec<usize>,
        /// Width of the synthetic row (the source column count).
        n_cols: usize,
    },
    /// The `[NOT] EXISTS` analogue of [`Op::GroupCorrelatedScalar`].
    GroupCorrelatedExists {
        sub: usize,
        negated: bool,
        dest: usize,
        group_cols: Vec<usize>,
        n_cols: usize,
    },
    /// Stop execution.
    Halt,
}

/// A correlated subquery captured at compile time, re-evaluated per outer row by
/// the interpreter through the [`SubqueryEval`] callback. The stored `Select` is
/// the subquery body verbatim; an outer-column reference inside it resolves to the
/// current outer row that the callback pushes as an outer frame.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrelatedSub {
    /// The subquery body (its own `FROM`/`WHERE`/projection).
    pub select: Select,
}

/// One per-group aggregate in a [`Op::GroupStep`]: its function and the register
/// holding the (already-evaluated) argument (`None` for `count(*)`).
#[derive(Debug, Clone, PartialEq)]
pub struct AggSpec {
    /// Which aggregate to fold.
    pub kind: AggKind,
    /// Argument register, or `None` for `count(*)`. For `json_group_object` this
    /// is the *key* register.
    pub arg: Option<usize>,
    /// Second argument register — the *value* register of `json_group_object`;
    /// `None` for every one-argument aggregate.
    pub arg2: Option<usize>,
    /// When set, fold only over distinct argument values (BINARY equality), so
    /// the slot computes e.g. `count(DISTINCT x)` per group.
    pub distinct: bool,
    /// When set, the register holding this aggregate's `FILTER (WHERE …)`
    /// predicate for the current row; a row whose predicate is not true is
    /// skipped for this aggregate only.
    pub filter: Option<usize>,
    /// `ORDER BY` keys for an ordered `group_concat`; empty when unordered.
    pub order: Vec<AggOrderKey>,
    /// Constant `group_concat`/`string_agg` separator (`None` = default `,`).
    pub sep: Option<String>,
    /// The argument's collating sequence (declared column collation, else BINARY),
    /// driving the `DISTINCT` dedup and `min`/`max` reduction. BINARY reproduces the
    /// previous behaviour exactly.
    pub collation: Collation,
}

/// One output column of a [`Op::GroupEmit`]: a group-key value (by key index) or
/// a finalized aggregate (by slot index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupOut {
    /// The group-key value at this index.
    Key(usize),
    /// The finalized aggregate at this slot.
    Agg(usize),
    /// A correlated scalar subquery (by index into [`Program::subqueries`]),
    /// evaluated against a synthetic row carrying this group's key values at
    /// their source-column positions (B5c-2 over a grouped projection; only the
    /// group keys are well-defined per group, so the guard admits it solely when
    /// every outer reference is a group key).
    Sub(usize),
    /// A correlated `EXISTS`/`NOT EXISTS` subquery (subquery index, `negated`),
    /// evaluated like [`GroupOut::Sub`].
    SubExists(usize, bool),
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
    /// `json_group_array(x)` / `jsonb_group_array(x)` — collect every value
    /// (including NULLs, as JSON `null`) into a JSON array; `jsonb` selects the
    /// binary JSONB encoding. Only admitted when the argument does not statically
    /// carry the JSON subtype (see `func::produces_json`), so `value_to_json`
    /// reproduces the tree-walker's `arg_to_json` exactly.
    JsonGroupArray {
        jsonb: bool,
    },
    /// `json_group_object(k, v)` / `jsonb_group_object(k, v)` — collect every
    /// key/value pair (the key coerced to text like the tree-walker, the value
    /// kept even when NULL) into a JSON object; `jsonb` selects the binary
    /// encoding. Admitted only when the *value* argument does not statically carry
    /// the JSON subtype. The key register is `AggSpec::arg`, the value register
    /// `AggSpec::arg2`.
    JsonGroupObject {
        jsonb: bool,
    },
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

/// One aggregate accumulator: the collected non-NULL argument values, a row
/// counter (used by `count(*)`), and — for an ordered `group_concat(x ORDER BY
/// …)` — the parallel `ORDER BY` key rows plus their per-key sort directions.
#[derive(Debug, Clone, Default)]
struct AggAcc {
    /// Collected non-NULL argument values. For `json_group_object` these are the
    /// keys, parallel to `vals2`.
    vals: Vec<Value>,
    /// Collected `json_group_object` values, parallel to `vals` (empty for every
    /// other aggregate).
    vals2: Vec<Value>,
    /// Row counter, used by `count(*)`.
    count: i64,
    /// Parallel to `vals`: each collected value's `ORDER BY` key row (empty when
    /// the aggregate carries no `ORDER BY`).
    keys: Vec<Vec<Value>>,
    /// Per-key `(descending, nulls_first, collation)` sort specs, captured once on
    /// the first ordered push (empty when unordered).
    dirs: Vec<(bool, Option<bool>, Collation)>,
    /// The `group_concat`/`string_agg` separator, captured on the first folded
    /// value (`None` = the default `,`).
    sep: Option<String>,
    /// The argument's collating sequence, captured on the first folded value
    /// (default `BINARY`). Drives the `min`/`max` reduction in `finalize_agg` and a
    /// `DISTINCT` dedup — a `NOCASE`/`RTRIM`/custom-collation argument folds under it.
    collation: Collation,
}
/// One `GROUP BY` group: its key values and one accumulator per aggregate slot.
type Group = (Vec<Value>, Vec<AggAcc>);

/// A compile-time aggregate spec from [`agg_kind_distinct`]: the kind, the
/// argument expression, the `DISTINCT` flag, the optional `FILTER (WHERE …)`
/// predicate, the `ORDER BY` terms (only meaningful for `group_concat`), the
/// constant `group_concat`/`string_agg` separator (`None` = the default `,`), and
/// the optional *second* argument expression — the value of `json_group_object`
/// (`None` for every one-argument aggregate).
type AggCallSpec = (
    AggKind,
    Option<Expr>,
    bool,
    Option<Expr>,
    Vec<OrderTerm>,
    Option<String>,
    Option<Expr>,
);

/// One `ORDER BY` key inside an ordered aggregate (`group_concat(x ORDER BY …)`):
/// the register holding the key value for the current row, plus its sort
/// direction and NULL placement (`None` = SQLite's default: NULLs first under
/// `ASC`, last under `DESC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggOrderKey {
    /// Register holding this key's value for the current row.
    pub reg: usize,
    /// `DESC`?
    pub descending: bool,
    /// Explicit `NULLS FIRST` (`Some(true)`) / `NULLS LAST` (`Some(false)`).
    pub nulls_first: Option<bool>,
    /// The collation this key sorts under: an explicit `COLLATE` on the term, else
    /// the key column's own collation, else `BINARY`.
    pub collation: Collation,
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
    /// Correlated subqueries referenced by [`Op::CorrelatedScalar`] /
    /// [`Op::CorrelatedExists`] (by index). Non-empty only for the live
    /// single-table scan path that supplies a [`SubqueryEval`] callback; every
    /// other construction path leaves it empty.
    pub subqueries: Vec<CorrelatedSub>,
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
        | "concat" | "concat_ws" | "soundex" | "unistr" | "unistr_quote" => true,
        // Build-constant strings — graphite's reported version / source identifier.
        // Both are nullary and read no row or connection state, so the VDBE's
        // `Op::Func` reproduces them exactly.
        "sqlite_version" | "sqlite_source_id" => argc == 0,
        // Numeric functions.
        "abs" | "round" | "sign" | "ceil" | "ceiling" | "floor" | "trunc" | "exp" | "ln"
        | "log" | "log2" | "log10" | "sqrt" | "pow" | "power" | "mod" | "sin" | "cos" | "tan"
        | "asin" | "acos" | "atan" | "atan2" | "sinh" | "cosh" | "tanh" | "asinh" | "acosh"
        | "atanh" | "radians" | "degrees" | "pi" => true,
        // Type / null helpers.
        // `likelihood` is excluded (unlike the no-op `likely`/`unlikely`): its
        // prepare-time check that the second argument is a floating-point literal
        // in 0.0..=1.0 needs the source expression, which the VDBE has already
        // reduced to a value here. It falls back to the tree-walker, which sees
        // the real AST.
        "typeof" | "nullif" | "zeroblob" | "likely" | "unlikely" => true,
        "coalesce" | "ifnull" => argc >= 1,
        // Pattern predicates in function form. `glob(pattern, text)` is always
        // two-argument; `like` also takes the three-argument `ESCAPE` form
        // (`text LIKE pattern ESCAPE c` desugars to `like(pattern, text, c)`).
        // Both route through `Op::Func` → `func::eval_scalar`, which applies the
        // escape character and rejects a non-single-character escape exactly as
        // the tree-walker does.
        "glob" => argc == 2,
        "like" => argc == 2 || argc == 3,
        // JSON functions that operate purely on their argument *values*. The
        // subtype-aware ones (`json_array`/`json_object`/`json_quote`, which embed
        // a `json(...)`-typed argument as JSON rather than quoting it) are
        // excluded: the VDBE passes `eval_scalar` literal-reconstructed values, so
        // the argument's JSON subtype — carried by its source expression — is
        // lost. They fall back to the tree-walker, which sees the real expression.
        // `json_error_position(X)` parses X's *text* and reports the first syntax
        // error offset; it reads no JSON subtype off the source expression, so the
        // VDBE's reconstructed value reproduces it exactly (unlike the
        // subtype-aware constructors above, it is safe here).
        "json"
        | "json_valid"
        | "json_type"
        | "json_array_length"
        | "json_extract"
        | "jsonb"
        | "json_error_position" => true,
        // `printf`/`format` are pure string formatting over their argument values
        // (no row/connection state): a format string plus zero or more values.
        // They route through `Op::Func` → `func::eval_scalar` → `datetime::printf`.
        "printf" | "format" => argc >= 1,
        // Date/time functions operate purely on their argument *values* — each
        // dispatches to `datetime::<fn>(&values)` with no `ctx` access, so the
        // VDBE's value round-trip reproduces the tree-walker exactly. The current
        // time (`'now'`, or the no-time-value default forms) is read from the wall
        // clock inside `datetime` — identically on both paths — not from the
        // connection, so it stays out of the `ctx`-free contract (it is merely
        // non-deterministic, never wrong). `strftime` needs at least its format
        // argument; `timediff` is exactly two-argument.
        "date" | "time" | "datetime" | "julianday" | "unixepoch" => true,
        "strftime" => argc >= 1,
        "timediff" => argc == 2,
        // Variadic scalar min/max need at least two args (one arg is the aggregate).
        "min" | "max" => argc >= 2,
        _ => false,
    }
}

/// Compile a constant-projection `SELECT` (no `FROM`/aggregates) into a program,
/// optionally filtered by a `WHERE` over rowless expressions. Returns
/// `Unsupported` for anything outside the spike's grammar so the caller can fall
/// back to the tree-walking executor.
pub fn compile_const_select(sel: &Select) -> Result<Program> {
    if sel.from.is_some()
        || !sel.group_by.is_empty()
        || sel.having.is_some()
        || !sel.compound.is_empty()
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
        correlated_subqueries: false,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
    };
    // A FROM-less `SELECT … WHERE <pred>` evaluates the predicate once over the
    // single (rowless) row and emits the projection only when it is true; a NULL
    // or false predicate yields zero rows. Compile and gate on the predicate
    // *before* the projections so a filtered row never evaluates the SELECT list
    // (matching SQLite, e.g. `SELECT abs('x') WHERE 0` produces no row, no work).
    // The predicate's scratch registers sit above the output block, so the gating
    // `IfFalse` reads its register before any projection op can reuse it.
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
    // A FROM-less SELECT yields at most one row, so `ORDER BY` cannot reorder
    // anything — it is a no-op. Each term is still VALIDATED, though, so an
    // invalid one defers to the tree-walker (which raises SQLite's exact error)
    // rather than being silently accepted: a positional ordinal (`ORDER BY 1`)
    // needs range-checking against the output arity — owned by the tree-walker —
    // so every positional form defers; any other term is compiled then discarded
    // purely to force column/alias resolution, so an unresolved reference
    // (`ORDER BY x`, or an output alias the const compiler can't see) makes
    // `compile_expr` defer too. A constant key (`ORDER BY abs(-1)`) resolves and
    // runs as the no-op it is.
    for term in &sel.order_by {
        if super::positional_int(&term.expr).is_some() {
            return Err(Error::Unsupported(
                "VDBE: positional ORDER BY in const SELECT",
            ));
        }
        let saved_ops = c.ops.len();
        let saved_reg = c.next_reg;
        c.compile_expr(&term.expr)?;
        c.ops.truncate(saved_ops);
        c.next_reg = saved_reg;
    }
    // A FROM-less SELECT yields at most one row, so `LIMIT`/`OFFSET` reduce to a
    // compile-time emit/suppress decision and `DISTINCT` is a no-op (one row is
    // trivially distinct). Fold both bounds to constants — a non-constant form
    // defers exactly as the scan path does — and suppress the row when `LIMIT` is
    // exactly 0 or a positive `OFFSET` skips past it. A negative `LIMIT` is
    // unlimited and a non-positive `OFFSET` skips nothing, both as in SQLite. The
    // projection (and any `WHERE`) is still compiled even when suppressed, so a
    // bad column reference defers to the tree-walker's error rather than being
    // silently dropped (e.g. `SELECT x LIMIT 0` must still raise `no such column`).
    let limit_zero = match &sel.limit {
        None => false,
        Some(e) => match fold_const_int(e) {
            Some(n) => n == 0,
            None => return Err(Error::Unsupported("VDBE: only constant integer LIMIT")),
        },
    };
    let offset_skips = match &sel.offset {
        None => false,
        Some(e) => match fold_const_int(e) {
            Some(n) => n > 0,
            None => return Err(Error::Unsupported("VDBE: only constant integer OFFSET")),
        },
    };
    if !(limit_zero || offset_skips) {
        c.ops.push(Op::ResultRow { start: 0, count });
    }
    let halt = c.ops.len();
    c.ops.push(Op::Halt);
    if let Some(at) = skip
        && let Op::IfFalse { target, .. } = &mut c.ops[at]
    {
        *target = halt;
    }
    Ok(Program {
        ops: c.ops,
        subqueries: core::mem::take(&mut c.subqueries),
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
/// Scalar functions safe to evaluate at **compile time** for constant folding:
/// deterministic, side-effect-free, and independent of the wall clock and
/// connection state. This is deliberately STRICTER than the VDBE's run-time
/// function allowlist — which includes `date`/`time`/`datetime`/`strftime`
/// because those read the clock identically on both paths *at run time*. Folding
/// a clock/random/state function here (at compile time) could diverge from the
/// tree-walker, so they are excluded and left to bail.
fn is_const_pure_fn(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "abs"
            | "coalesce"
            | "ifnull"
            | "iif"
            | "nullif"
            | "sign"
            | "round"
            | "length"
            | "lower"
            | "upper"
            | "hex"
            | "unhex"
            | "quote"
            | "char"
            | "unicode"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "replace"
            | "substr"
            | "substring"
            | "instr"
            | "typeof"
            | "printf"
            | "format"
            | "sqrt"
            | "pow"
            | "power"
            | "mod"
            | "exp"
            | "ln"
            | "log"
            | "log10"
            | "log2"
            | "ceil"
            | "ceiling"
            | "floor"
            | "trunc"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "sinh"
            | "cosh"
            | "tanh"
            | "radians"
            | "degrees"
            | "pi"
    )
}

fn fold_const_int(e: &Expr) -> Option<i64> {
    fn pure(e: &Expr) -> bool {
        match e {
            Expr::Literal(_) => true,
            Expr::Paren(x) | Expr::Cast { expr: x, .. } => pure(x),
            Expr::Unary { expr, .. } => pure(expr),
            Expr::Binary { left, right, .. } => pure(left) && pure(right),
            // A deterministic, stateless scalar function of pure arguments — folds
            // at compile time. Aggregates/`*`/`DISTINCT`, clock/random/state
            // functions, and anything with a column/subquery argument are excluded
            // (they bail to the tree-walker, which is never wrong).
            Expr::Function {
                name,
                args,
                distinct,
                star,
                filter,
                over,
                ..
            } => {
                !*distinct
                    && !*star
                    && filter.is_none()
                    && over.is_none()
                    && is_const_pure_fn(name)
                    && args.iter().all(pure)
            }
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
    // A LIMIT/OFFSET value must convert *exactly* to an integer (SQLite's
    // `OP_MustBeInt`): an integer, an integer-valued real, or text that parses
    // as one. A fractional real, non-numeric text, NULL, or blob is a datatype
    // mismatch — return `None` so the caller bails to the interpreter path,
    // which raises `datatype mismatch` rather than silently truncating.
    fn exact_int(r: f64) -> Option<i64> {
        (r.is_finite()
            && r == crate::util::float::trunc(r)
            && r >= i64::MIN as f64
            && r < 9_223_372_036_854_775_808.0)
            .then_some(r as i64)
    }
    match v {
        Value::Integer(i) => Some(i),
        Value::Real(r) => exact_int(r),
        Value::Text(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                Some(i)
            } else {
                t.parse::<f64>().ok().and_then(exact_int)
            }
        }
        Value::Null | Value::Blob(_) => None,
    }
}

fn is_aggregate_expr(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Function { name, args, star, .. }
            if crate::exec::func::is_aggregate_call(name, args.len(), *star)
    )
}

/// True if an aggregate call or a window (`OVER …`) function appears anywhere
/// within `e` (recursively, not just at the top level).
fn expr_has_aggregate_or_window(e: &Expr) -> bool {
    let mut found = false;
    crate::exec::window::visit(e, &mut |node| {
        if let Expr::Function {
            name,
            args,
            star,
            over,
            ..
        } = node
            && (over.is_some() || crate::exec::func::is_aggregate_call(name, args.len(), *star))
        {
            found = true;
        }
    });
    found
}

/// Defer a join to the tree-walker when an aggregate or window function appears in
/// any join `ON` predicate or in the `WHERE` clause. Both are misuses (there is no
/// grouping context at the row-filter level); the tree-walker reports them with the
/// proper "misuse of aggregate/window function" error, whereas a join compiler that
/// never evaluates the predicate (e.g. an empty outer table) would silently succeed.
pub(crate) fn reject_aggregate_or_window_in_predicates(sel: &Select) -> Result<()> {
    if let Some(from) = &sel.from {
        for j in &from.joins {
            if let Some(on) = &j.on
                && expr_has_aggregate_or_window(on)
            {
                return Err(Error::Unsupported(
                    "VDBE: aggregate/window in join ON predicate",
                ));
            }
        }
    }
    if let Some(w) = &sel.where_clause
        && expr_has_aggregate_or_window(w)
    {
        return Err(Error::Unsupported(
            "VDBE: aggregate/window in WHERE predicate",
        ));
    }
    Ok(())
}

/// Map a 1-arg-or-star aggregate call to its [`AggKind`] (binding the argument
/// register expression), reporting whether the call carried `DISTINCT` (third
/// tuple element), its `FILTER (WHERE …)` predicate if any (fourth element), and
/// its `ORDER BY` terms if any (fifth element). The aggregate compilers fold over
/// the collected argument values, dedup them when `distinct` is set, skip a row
/// whose `filter` predicate is not true, and — for `group_concat` — sort the
/// collected values by the `ORDER BY` keys before concatenating. Returns `None`
/// for unsupported call shapes; `OVER` (window) calls always bail, as does an
/// `ORDER BY` on any other aggregate kind or combined with `DISTINCT`.
/// Compile each `ORDER BY` term of an ordered aggregate into an [`AggOrderKey`],
/// evaluating its key expression into a register for the current row.
fn compile_order_keys(c: &mut Compiler, order: &[OrderTerm]) -> Result<Vec<AggOrderKey>> {
    order
        .iter()
        .map(|t| {
            let reg = c.compile_expr(&t.expr)?;
            // An explicit `COLLATE` on the term wins; else the key column's own
            // collation; else BINARY — matching the tree-walker's `key_collation`.
            let collation = c
                .explicit_collation(&t.expr)
                .or_else(|| c.col_collation(&t.expr))
                .unwrap_or_default();
            Ok(AggOrderKey {
                reg,
                descending: t.descending,
                nulls_first: t.nulls_first,
                collation,
            })
        })
        .collect()
}

fn agg_kind_distinct(expr: &Expr) -> Option<AggCallSpec> {
    let Expr::Function {
        name,
        distinct,
        args,
        star,
        filter,
        order_by,
        over,
        ..
    } = expr
    else {
        return None;
    };
    if over.is_some() {
        return None;
    }
    let filter = filter.as_ref().map(|f| (**f).clone());
    let order = order_by.clone();
    let arg = args.first().cloned();
    let kind = match name.to_ascii_lowercase().as_str() {
        // `count(*)` takes no ORDER BY; `count(DISTINCT *)` is not valid SQL.
        "count" if *star => {
            return (!*distinct && order.is_empty()).then_some((
                AggKind::CountStar,
                None,
                false,
                filter,
                Vec::new(),
                None,
                None,
            ));
        }
        "count" if args.len() == 1 => (AggKind::Count, None),
        "sum" if args.len() == 1 => (AggKind::Sum, None),
        "total" if args.len() == 1 => (AggKind::Total, None),
        "avg" if args.len() == 1 => (AggKind::Avg, None),
        "min" if args.len() == 1 => (AggKind::Min, None),
        "max" if args.len() == 1 => (AggKind::Max, None),
        // One-argument `group_concat` uses the default `,` separator.
        "group_concat" if args.len() == 1 => (AggKind::GroupConcat, None),
        // The two-argument `group_concat(x, sep)` and its standard-SQL alias
        // `string_agg(x, sep)` take an explicit separator. The tree-walker
        // evaluates that separator in a *rowless* context (it is a constant), so
        // the VDBE accepts it only when it is a literal — otherwise it falls back.
        // `DISTINCT` with two arguments is a tree-walker error ("DISTINCT
        // aggregates must have exactly one argument"), so leave it to fall back.
        "group_concat" | "string_agg" if args.len() == 2 && !*distinct => {
            (AggKind::GroupConcat, Some(const_sep_text(&args[1])?))
        }
        // `json_group_array(x)` collects every value (NULLs included) into a JSON
        // array. Admitted only when the argument does not statically carry the JSON
        // subtype: then the tree-walker's `arg_to_json` reduces to `value_to_json`,
        // which the finalizer uses — so a `json(x)`/`->` argument (whose text must
        // be spliced in unquoted) defers to the tree-walker.
        "json_group_array" | "jsonb_group_array"
            if args.len() == 1 && !args.first().is_some_and(super::func::produces_json) =>
        {
            (
                AggKind::JsonGroupArray {
                    jsonb: name.eq_ignore_ascii_case("jsonb_group_array"),
                },
                None,
            )
        }
        // `json_group_object(k, v)` collects key/value pairs into a JSON object.
        // Admitted only when the *value* argument does not statically carry the
        // JSON subtype (the key is always text-coerced). `DISTINCT` is not
        // meaningful here — leave it to the tree-walker.
        "json_group_object" | "jsonb_group_object"
            if args.len() == 2 && !*distinct && !super::func::produces_json(&args[1]) =>
        {
            (
                AggKind::JsonGroupObject {
                    jsonb: name.eq_ignore_ascii_case("jsonb_group_object"),
                },
                None,
            )
        }
        _ => return None,
    };
    let (kind, sep) = kind;
    // `ORDER BY` inside an aggregate only changes `group_concat` output; defer
    // any other ordered aggregate, and `DISTINCT` + `ORDER BY`, to the tree-walker.
    if !order.is_empty() && (*distinct || kind != AggKind::GroupConcat) {
        return None;
    }
    // A non-BINARY argument collation — an explicit `COLLATE` (`min(a COLLATE
    // NOCASE)`, `count(DISTINCT a COLLATE NOCASE)`) or a column's declared collation
    // — drives the VDBE's `DISTINCT` dedup and `min`/`max` fold: the compile paths
    // resolve it into `AggStep.collation`/`AggSpec.collation`, so no deferral is
    // needed here.
    // `json_group_object`'s value argument (the key is `arg`).
    let arg2 = matches!(kind, AggKind::JsonGroupObject { .. })
        .then(|| args.get(1).cloned())
        .flatten();
    Some((kind, arg, *distinct, filter, order, sep, arg2))
}

/// True if any `DISTINCT` output projection carries an explicit non-BINARY
/// `COLLATE` *anywhere* within it. A row-level `DISTINCT` dedups under each
/// output column's collation; the VDBE's `DistinctCheck` compares under BINARY,
/// and the per-site bail only inspects a column's *declared* collation — so an
/// explicit `COLLATE` (`SELECT DISTINCT a COLLATE NOCASE`) would otherwise dedup
/// under the wrong sequence. The walk is conservative (a nested `COLLATE` that
/// does not reach the top-level result collation still defers), which is safe:
/// the tree-walker handles every such row correctly.
fn projections_have_explicit_collation(projections: &[(Expr, String)]) -> bool {
    let mut found = false;
    for (e, _) in projections {
        crate::exec::window::visit(e, &mut |node| {
            if let Expr::Collate { collation, .. } = node
                && crate::value::resolve_collation_name(collation)
                    .is_some_and(|c| c != Collation::Binary)
            {
                found = true;
            }
        });
    }
    found
}

/// The text of a constant `group_concat`/`string_agg` separator, matching the
/// tree-walker (which evaluates the separator argument in a rowless context and
/// renders it with `to_text`). Returns `None` for a non-literal separator so the
/// caller falls back to the tree-walker, which sees the real expression.
fn const_sep_text(expr: &Expr) -> Option<String> {
    use crate::exec::eval;
    let Expr::Literal(l) = unparen(expr) else {
        return None;
    };
    let v = match l {
        Literal::Null => Value::Null,
        Literal::Integer(i) => Value::Integer(*i),
        Literal::Real(r) => Value::Real(*r),
        Literal::Str(s) => Value::Text(s.clone().into()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Boolean(b) => Value::Integer(*b as i64),
    };
    Some(eval::to_text(&v))
}

/// Compile `SELECT <aggregates> FROM <table> [WHERE …]` (no GROUP BY): the scan
/// folds every aggregate slot, then a single `ResultRow` emits the finalized
/// values. Returns `Unsupported` for shapes outside this grammar (so the caller
/// falls back); `ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT` on an aggregate query are
/// left to the tree-walker.
/// Whether `e` is constant with respect to the scanned rows — it references no
/// column (nor any per-row state), so it evaluates to the same value for every
/// row. A constant `GROUP BY` key thus forms a single group. Deliberately
/// conservative: only literals and constant compositions of them are recognized;
/// an unrecognized shape (a column, function, subquery, …) returns `false`, so
/// the query simply falls back rather than risking a mis-grouped result.
fn is_row_constant(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) => true,
        Expr::Paren(x)
        | Expr::Cast { expr: x, .. }
        | Expr::Collate { expr: x, .. }
        | Expr::Unary { expr: x, .. } => is_row_constant(x),
        Expr::Binary { left, right, .. } => is_row_constant(left) && is_row_constant(right),
        _ => false,
    }
}

fn compile_aggregate_select(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    projections: &[(Expr, String)],
    // `gate_nonempty` is set when this compiles a constant-`GROUP BY` query
    // (`GROUP BY <constant>`), which yields one group over a *non-empty* table but
    // NO rows over an empty one (unlike a bare aggregate, which always emits one
    // row). A hidden `count(*)` slot then gates the output row on at least one
    // scanned row.
    gate_nonempty: bool,
) -> Result<Program> {
    if !sel.order_by.is_empty() || sel.limit.is_some() || sel.offset.is_some() || sel.distinct {
        return Err(Error::Unsupported("VDBE: bare aggregate only"));
    }
    // The aggregate min/max reduction and DISTINCT dedup now fold under the
    // argument's collation (`AggStep.collation` → `AggAcc.collation`), so a
    // non-BINARY declared column collation runs on the VDBE. An explicit `COLLATE`
    // argument still defers via `agg_kind_distinct` below.
    // Every projection must be exactly one supported aggregate call. DISTINCT is
    // supported here (the collected values are deduped at fold time), as is
    // `FILTER (WHERE …)` (a non-passing row is skipped for that aggregate).
    let mut slots: Vec<AggCallSpec> = Vec::new();
    for (e, _) in projections {
        match agg_kind_distinct(e) {
            Some(spec) => slots.push(spec),
            None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
        }
    }
    let count = projections.len();
    // The hidden non-empty gate is a `count(*)` folded over the same scan; its
    // finalized value lands in register `count` (the slot after the projections).
    let gate_reg = if gate_nonempty {
        slots.push((
            AggKind::CountStar,
            None,
            false,
            None,
            Vec::new(),
            None,
            None,
        ));
        Some(count)
    } else {
        None
    };
    let mut c = Compiler {
        ops: Vec::new(),
        next_reg: slots.len(),
        columns: columns.to_vec(),
        tables: tables.to_vec(),
        affinities: affinities.to_vec(),
        collations: collations.to_vec(),
        bindings: Vec::new(),
        forbid_raw_columns: false,
        rowid_index: None,
        cursor_boundaries: None,
        correlated_subqueries: false,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
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
    for (slot, (kind, arg, distinct, filter, order, sep, arg2)) in slots.iter().enumerate() {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let arg2_reg = match arg2 {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let filter_reg = match filter {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let order_keys = compile_order_keys(&mut c, order)?;
        let collation = arg
            .as_ref()
            .and_then(|e| c.explicit_collation(e).or_else(|| c.col_collation(e)))
            .unwrap_or_default();
        c.ops.push(Op::AggStep {
            slot,
            kind: *kind,
            arg: arg_reg,
            arg2: arg2_reg,
            distinct: *distinct,
            filter: filter_reg,
            order: order_keys,
            sep: sep.clone(),
            collation,
        });
    }
    let next = c.ops.len();
    c.ops.push(Op::Next { target: body });
    if let Some(at) = skip
        && let Op::IfFalse { target, .. } = &mut c.ops[at]
    {
        *target = next;
    }
    let end = c.ops.len();
    if let Op::Rewind { target } = &mut c.ops[rewind] {
        *target = end;
    }
    // Finalize each slot into its output register, then emit the single row.
    for (slot, (kind, _, _, _, _, _, _)) in slots.iter().enumerate() {
        c.ops.push(Op::AggFinal {
            slot,
            kind: *kind,
            dest: slot,
        });
    }
    // The single output row is gated by zero or more `IfFalse` checks (each
    // backpatched to jump past `ResultRow` to the final `Halt`):
    //   * the hidden non-empty gate: a constant-`GROUP BY` emits no row over an
    //     empty table, so skip when the row count (register `gate_reg`) is 0;
    //   * a `HAVING`: bind each projection aggregate's source expression to its
    //     finalized register, then compile the predicate (a HAVING aggregate that
    //     also appears in the projection resolves to its slot; one that does not is
    //     unbound, so `compile_expr` raises "misuse of aggregate" (Err) and the
    //     whole query defers to the tree-walker — never a wrong result).
    let mut gates: Vec<usize> = Vec::new();
    if let Some(reg) = gate_reg {
        gates.push(c.ops.len());
        c.ops.push(Op::IfFalse { reg, target: 0 });
    }
    if let Some(having) = &sel.having {
        for (i, (e, _)) in projections.iter().enumerate() {
            c.bindings.push((e.clone(), i));
        }
        let hreg = c.compile_expr(having)?;
        gates.push(c.ops.len());
        c.ops.push(Op::IfFalse {
            reg: hreg,
            target: 0,
        });
    }
    c.ops.push(Op::ResultRow { start: 0, count });
    let halt = c.ops.len();
    c.ops.push(Op::Halt);
    for g in gates {
        if let Op::IfFalse { target, .. } = &mut c.ops[g] {
            *target = halt;
        }
    }
    Ok(Program {
        ops: c.ops,
        subqueries: core::mem::take(&mut c.subqueries),
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

/// Collect the distinct bare column references in `expr` (as `(table, column)`
/// pairs), *not* descending into aggregate-call arguments — those are folded, not
/// projected as bare columns. Used by the grouped HAVING/ORDER path to find
/// non-grouped columns that need a first-seen-row representative. The walk is
/// deliberately partial: a column inside an expression shape this does not visit
/// simply isn't pre-bound, so it later bails via `forbid_raw_columns` (a safe
/// fall-back, never a wrong result).
fn collect_bare_columns(expr: &Expr, out: &mut Vec<(Option<String>, String)>) {
    if is_aggregate_expr(expr) {
        return;
    }
    match expr {
        Expr::Column { table, column, .. } => {
            let key = (table.clone(), column.clone());
            if !out.contains(&key) {
                out.push(key);
            }
        }
        Expr::Paren(inner)
        | Expr::Unary { expr: inner, .. }
        | Expr::IsNull { expr: inner, .. }
        | Expr::Cast { expr: inner, .. }
        | Expr::Collate { expr: inner, .. } => collect_bare_columns(inner, out),
        Expr::Binary { left, right, .. } => {
            collect_bare_columns(left, out);
            collect_bare_columns(right, out);
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_bare_columns(a, out);
            }
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            if let Some(o) = operand {
                collect_bare_columns(o, out);
            }
            for (w, t) in when_then {
                collect_bare_columns(w, out);
                collect_bare_columns(t, out);
            }
            if let Some(e) = else_result {
                collect_bare_columns(e, out);
            }
        }
        _ => {}
    }
}

/// A resolved `GROUP BY` key: either a bare table column (kept as a combined
/// column index, so the compact `GroupEmit` plain path and representative-column
/// bookkeeping can index it directly) or an arbitrary scalar expression evaluated
/// per row in the fold. A computed key forces the general grouped path, where the
/// projection resolves the same expression through the binding table.
enum GroupKeySpec {
    Col(usize),
    // Boxed to keep the enum small — `Expr` is a large variant.
    Expr(alloc::boxed::Box<Expr>),
}

/// The collating sequence each group key is compared/ordered under: a bare
/// column's declared collation (from the source-indexed `c.collations`), else a
/// computed key's resolved collation (explicit `COLLATE`, else its column's, else
/// BINARY). All-BINARY output reproduces the previous BINARY-only grouping.
fn group_key_collations(c: &Compiler, group_keys: &[GroupKeySpec]) -> Vec<Collation> {
    group_keys
        .iter()
        .map(|k| match k {
            GroupKeySpec::Col(ci) => c.collations.get(*ci).copied().unwrap_or_default(),
            GroupKeySpec::Expr(e) => c
                .explicit_collation(e)
                .or_else(|| c.col_collation(e))
                .unwrap_or_default(),
        })
        .collect()
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
    group_keys: &[GroupKeySpec],
    repr_cols: &[usize],
    agg_specs: &[AggCallSpec],
) -> Result<()> {
    let bounds = c.cursor_boundaries.clone();
    // Contiguous registers: one per grouping column, then one per bare-column
    // representative (loaded per row, but only the group's first-seen row's values
    // are retained — see `Op::GroupStep`).
    let key_start = c.next_reg;
    for _ in group_keys {
        c.alloc();
    }
    for _ in repr_cols {
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
    // Load each grouping key into its register. A bare column reads the scan row
    // (per-cursor `ColumnC` for a join); a computed key compiles its expression
    // into the key register (bindings are still empty here, so column refs read
    // the live scan row — exactly the per-row key value we want).
    for (k, key) in group_keys.iter().enumerate() {
        match key {
            GroupKeySpec::Col(ci) => match &bounds {
                Some(b) => {
                    let (cursor, col) = Compiler::cursor_of(b, *ci);
                    c.ops.push(Op::ColumnC {
                        cursor,
                        col,
                        dest: key_start + k,
                    });
                }
                None => c.ops.push(Op::Column {
                    col: *ci,
                    dest: key_start + k,
                }),
            },
            GroupKeySpec::Expr(e) => {
                c.compile_expr_into(e, key_start + k)?;
            }
        }
    }
    // Load each bare-column representative right after the keys. `GroupStep` keeps
    // the value from the row that first creates the group (first-seen semantics).
    let kn = group_keys.len();
    for (r, &ci) in repr_cols.iter().enumerate() {
        match &bounds {
            Some(b) => {
                let (cursor, col) = Compiler::cursor_of(b, ci);
                c.ops.push(Op::ColumnC {
                    cursor,
                    col,
                    dest: key_start + kn + r,
                });
            }
            None => c.ops.push(Op::Column {
                col: ci,
                dest: key_start + kn + r,
            }),
        }
    }
    // Evaluate each aggregate argument (and its FILTER predicate) into a register
    // for this row.
    let mut aggs: Vec<AggSpec> = Vec::new();
    for (kind, arg, distinct, filter, order, sep, arg2) in agg_specs {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let arg2_reg = match arg2 {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let filter_reg = match filter {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let order_keys = compile_order_keys(c, order)?;
        // The argument's collation for the DISTINCT dedup / min-max reduction. An
        // explicit `COLLATE` argument already defers (via `agg_kind_distinct`), so
        // only a column's declared collation reaches here; BINARY otherwise.
        let collation = arg
            .as_ref()
            .and_then(|e| c.explicit_collation(e).or_else(|| c.col_collation(e)))
            .unwrap_or_default();
        aggs.push(AggSpec {
            kind: *kind,
            arg: arg_reg,
            arg2: arg2_reg,
            distinct: *distinct,
            filter: filter_reg,
            order: order_keys,
            sep: sep.clone(),
            collation,
        });
    }
    // With bare-column representatives and exactly one min()/max() aggregate,
    // SQLite pulls bare columns from that aggregate's extreme row (the "companion"
    // row) rather than the first-seen row. Identify the governing aggregate's
    // argument register so `GroupStep` can track the extreme. (Callers bail when
    // more than one min/max is present, so the companion is unambiguous here.)
    let companion = if repr_cols.is_empty() {
        None
    } else {
        let mm: Vec<usize> = aggs
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a.kind, AggKind::Min | AggKind::Max))
            .map(|(i, _)| i)
            .collect();
        match mm.as_slice() {
            [j] => aggs[*j]
                .arg
                .map(|areg| (areg, aggs[*j].kind == AggKind::Max)),
            _ => None,
        }
    };
    let key_collations = group_key_collations(c, group_keys);
    // Collation of the single min()/max() companion's argument (BINARY otherwise),
    // so the extreme-row comparison matches under a collated text argument.
    let companion_collation = if companion.is_none() {
        Collation::Binary
    } else {
        let mm: Vec<usize> = aggs
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a.kind, AggKind::Min | AggKind::Max))
            .map(|(i, _)| i)
            .collect();
        match mm.as_slice() {
            [j] => agg_specs[*j]
                .1
                .as_ref()
                .and_then(|e| c.explicit_collation(e).or_else(|| c.col_collation(e)))
                .unwrap_or_default(),
            _ => Collation::Binary,
        }
    };
    c.ops.push(Op::GroupStep {
        key_start,
        key_count: group_keys.len(),
        repr_count: repr_cols.len(),
        companion,
        aggs,
        key_collations,
        companion_collation,
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
            if let Some(at) = skip
                && let Op::IfFalse { target, .. } = &mut c.ops[at]
            {
                *target = next_at[n - 1];
            }
        }
        None => {
            let next = c.ops.len();
            c.ops.push(Op::Next { target: body });
            if let Some(at) = skip
                && let Op::IfFalse { target, .. } = &mut c.ops[at]
            {
                *target = next;
            }
            let end = c.ops.len();
            if let Some(rw) = single_rewind
                && let Op::Rewind { target } = &mut c.ops[rw]
            {
                *target = end;
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
#[allow(clippy::too_many_arguments)] // cohesive: the column space + boundaries + correlated gate
fn compile_group_select(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    projections: &[(Expr, String)],
    boundaries: Option<&[usize]>,
    allow_correlated: bool,
) -> Result<Program> {
    // Group-key matching/ordering, the min()/max() companion, and each aggregate's
    // DISTINCT dedup / min-max reduction are now collation-aware (per-key collations
    // on `GroupStep`/`sort_groups_by_key`; the argument collation on `AggSpec`). The
    // one comparison still done under BINARY is a `SELECT DISTINCT … GROUP BY`'s
    // post-grouping dedup (the grouped `DistinctCheck` carries no collations), so
    // defer only that case when a projected column carries a non-BINARY collation.
    if sel.distinct && collations.iter().any(|c| *c != Collation::Binary) {
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
        correlated_subqueries: false,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
    };
    // Resolve each grouping key. A bare column becomes a (combined) column index;
    // any other expression is kept and evaluated per row in the fold (its value
    // identifies the group, and the projection resolves the same expression
    // through the binding table). A column-only key set can still take the compact
    // `GroupEmit` plain path; a computed key forces the general path below.
    //
    // SQLite resolves a bare `GROUP BY <name>` to a SOURCE column first, then to a
    // SELECT-list output alias. The VDBE column resolver only knows source columns,
    // so a bare name that is NOT a source column but IS an output alias is rewritten
    // here to that output column's expression (mirroring the positional rewrite in
    // `run_select_vdbe` and the ORDER BY alias handling below). A name matching a
    // source column is left untouched (source precedence — it resolves normally); an
    // alias bound to an aggregate is left untouched so the tree-walker raises
    // SQLite's "aggregate functions are not allowed in the GROUP BY clause" error.
    let effective_group: Vec<Expr> = sel
        .group_by
        .iter()
        .map(|g| {
            if let Expr::Column {
                table: None,
                column,
                ..
            } = g
                && !columns.iter().any(|n| n.eq_ignore_ascii_case(column))
                && let Some((e, _)) = projections
                    .iter()
                    .find(|(_, l)| l.eq_ignore_ascii_case(column))
            {
                let mut aggs = Vec::new();
                collect_aggregates(e, &mut aggs);
                if aggs.is_empty() {
                    return e.clone();
                }
            }
            g.clone()
        })
        .collect();
    let mut group_keys: Vec<GroupKeySpec> = Vec::new();
    for g in &effective_group {
        match g {
            Expr::Column { table, column, .. } => {
                group_keys.push(GroupKeySpec::Col(
                    c.resolve_column(table.as_deref(), column)?,
                ));
            }
            other => {
                // A *constant* (column-free) key is left to the tree-walker. SQLite
                // treats a signed-integer literal key as a positional reference
                // (`GROUP BY -1` is "out of range", not "group by −1"), while other
                // constants (`'x'`, `1+0`) collapse every row into one group; the
                // tree-walker already draws that distinction exactly, and a constant
                // key has no grouping value on the VDBE. Only a key that reads at
                // least one column becomes a computed VDBE key.
                let mut refs = Vec::new();
                collect_bare_columns(other, &mut refs);
                if refs.is_empty() {
                    return Err(Error::Unsupported("VDBE: constant GROUP BY key"));
                }
                // A computed key groups under its resolved collation — an explicit
                // `COLLATE`, else the underlying column's — via `group_key_collations`,
                // so an explicit `COLLATE NOCASE` on the key runs on the VDBE too.
                group_keys.push(GroupKeySpec::Expr(alloc::boxed::Box::new(other.clone())));
            }
        }
    }
    let all_col_keys = group_keys.iter().all(|k| matches!(k, GroupKeySpec::Col(_)));
    // Column indices of the bare-column keys: used to exclude a grouping column
    // from the representative set and (on the plain path) to classify outputs.
    let key_col_indices: Vec<usize> = group_keys
        .iter()
        .filter_map(|k| match k {
            GroupKeySpec::Col(i) => Some(*i),
            GroupKeySpec::Expr(_) => None,
        })
        .collect();

    // `SELECT DISTINCT` dedups output rows under BINARY (`DistinctCheck` below). An
    // output expression carrying a non-BINARY collation — an explicit `COLLATE` on
    // an otherwise-BINARY column — would dedup case-insensitively in SQLite (a
    // non-BINARY *declared* column already bailed via the table-wide guard above),
    // so defer that case to the tree-walker.
    if sel.distinct
        && projections
            .iter()
            .any(|(e, _)| c.col_collation(e).is_some_and(|co| co != Collation::Binary))
    {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation in DISTINCT output",
        ));
    }

    // The plain path (no HAVING / ORDER BY / LIMIT, all keys bare columns) keeps
    // its compact `GroupEmit`. A computed key needs the binding-driven general
    // path, so it falls through even without HAVING/ORDER BY/LIMIT. `SELECT
    // DISTINCT` likewise needs the general path's post-grouping `DistinctCheck`.
    if !has_having && !has_order && !has_limit && all_col_keys && !sel.distinct {
        return compile_group_emit(
            sel,
            columns,
            tables,
            affinities,
            collations,
            projections,
            &key_col_indices,
            boundaries,
            allow_correlated,
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
    let mut agg_specs: Vec<AggCallSpec> = Vec::new();
    for e in &agg_exprs {
        match agg_kind_distinct(e) {
            Some(spec) => agg_specs.push(spec),
            None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
        }
    }

    // Bare (non-grouped) columns referenced in the projection / HAVING / ORDER BY
    // get a first-seen-row representative (SQLite's rule), captured by the fold as
    // extra key-vector slots and loaded back via `GroupKey` in the emit body.
    // Anything not collected here still bails via `forbid_raw_columns`, so an
    // unhandled expression shape simply falls back rather than misbehaving.
    let mut bare_refs: Vec<(Option<String>, String)> = Vec::new();
    for (e, _) in projections {
        collect_bare_columns(e, &mut bare_refs);
    }
    if let Some(h) = &sel.having {
        collect_bare_columns(h, &mut bare_refs);
    }
    for term in &sel.order_by {
        collect_bare_columns(&term.expr, &mut bare_refs);
    }
    // SQLite's single-min/max bare-column rule fires on exactly one min/max across
    // projection + HAVING + ORDER BY — all of which the fold's companion tracks
    // here. Under it, even a projected GROUP BY key follows the extreme row (so a
    // group with numerically-equal but differently-typed keys prints the extreme
    // row's representation), so grouping columns must become representatives too.
    let single_minmax = super::single_minmax_arg(sel).is_some();
    let mut repr_cols: Vec<usize> = Vec::new();
    for (t, col) in &bare_refs {
        // A HAVING/ORDER-BY ref that doesn't name a real table column is an output
        // alias or ordinal, resolved through other bindings — not a representative.
        let Ok(ci) = c.resolve_column(t.as_deref(), col) else {
            continue;
        };
        // Grouping columns are normally bound to their key register below; but with
        // a single min/max active the displayed key follows the companion row, so
        // they become representatives instead.
        if !single_minmax && key_col_indices.contains(&ci) {
            continue;
        }
        if !repr_cols.contains(&ci) {
            repr_cols.push(ci);
        }
    }
    // Bare columns track exactly one min()/max() aggregate's companion row (handled
    // by `emit_group_fold`); with more than one, the companion is ambiguous and
    // SQLite leaves it unspecified — bail there rather than pick arbitrarily.
    if !repr_cols.is_empty()
        && agg_specs
            .iter()
            .filter(|(k, ..)| matches!(k, AggKind::Min | AggKind::Max))
            .count()
            > 1
    {
        return Err(Error::Unsupported(
            "VDBE: bare column with multiple min/max companions",
        ));
    }

    // Fold the row source (single cursor or nested-loop join) into per-group
    // aggregates, allocating the grouping-key and representative registers.
    emit_group_fold(&mut c, sel, &group_keys, &repr_cols, &agg_specs)?;

    // Per-group registers: one for each grouping key value, one for each
    // aggregate final. These feed the bindings so HAVING/projection/ORDER-BY
    // expressions resolve grouping-column refs and aggregate calls to registers.
    let gkey_start = c.next_reg;
    for _ in &group_keys {
        c.alloc();
    }
    let gagg_start = c.next_reg;
    for _ in &agg_specs {
        c.alloc();
    }
    // Grouping key → its key register. For a bare-column key bind both the bare
    // form (`g`) and the qualified form (`t.g`) so a qualified reference in the
    // projection / HAVING / ORDER BY resolves to the key register too (otherwise
    // it would compile to a scan-column read that is invalid during emit). For a
    // computed key bind the whole expression: a projection/HAVING/ORDER-BY term
    // structurally equal to the key resolves to the key register, and anything
    // else bails via `forbid_raw_columns` (so it simply falls back).
    for (k, key) in group_keys.iter().enumerate() {
        let ci = match key {
            GroupKeySpec::Expr(e) => {
                c.bindings.push(((**e).clone(), gkey_start + k));
                continue;
            }
            GroupKeySpec::Col(ci) => *ci,
        };
        // Under the single-min/max rule this column is a representative (bound to
        // its repr register below), so it must NOT also bind to the stored-key
        // register — that binding would shadow the repr one and print the group's
        // key value instead of the extreme row's.
        if single_minmax && repr_cols.contains(&ci) {
            continue;
        }
        c.bindings.push((
            Expr::Column {
                schema: None,
                table: None,
                column: columns[ci].clone(),
                quoted: false,
                span: Span::none(),
            },
            gkey_start + k,
        ));
        if let Some(t) = tables.get(ci) {
            c.bindings.push((
                Expr::Column {
                    schema: None,
                    table: Some(t.clone()),
                    column: columns[ci].clone(),
                    quoted: false,
                    span: Span::none(),
                },
                gkey_start + k,
            ));
        }
    }
    // Each aggregate call → its final register.
    for (j, e) in agg_exprs.iter().enumerate() {
        c.bindings.push((e.clone(), gagg_start + j));
    }
    // Representative registers for bare non-grouped columns, bound to both the
    // bare and qualified column forms (mirroring the grouping-key bindings). The
    // values are loaded from the group's key vector (after the keys) in the emit
    // body via `GroupKey`.
    let grepr_start = c.next_reg;
    for _ in &repr_cols {
        c.alloc();
    }
    for (r, &ci) in repr_cols.iter().enumerate() {
        c.bindings.push((
            Expr::Column {
                schema: None,
                table: None,
                column: columns[ci].clone(),
                quoted: false,
                span: Span::none(),
            },
            grepr_start + r,
        ));
        if let Some(t) = tables.get(ci) {
            c.bindings.push((
                Expr::Column {
                    schema: None,
                    table: Some(t.clone()),
                    column: columns[ci].clone(),
                    quoted: false,
                    span: Span::none(),
                },
                grepr_start + r,
            ));
        }
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

    // Resolve each ORDER BY term to an output-column expression where it names one
    // — by position, a result-column alias, or such a reference wrapped in
    // `COLLATE`/parens (mirroring the tree-walker's `resolve_order_index`). SQLite
    // matches an `ORDER BY` name against the result aliases FIRST, even when a base
    // table column shares the name, so `SELECT b AS a FROM t ORDER BY a` sorts by
    // `b`. An explicit `COLLATE` on the term sets the collation, else the resolved
    // column's own collation applies.
    let labels: Vec<String> = projections.iter().map(|(_, l)| l.clone()).collect();
    let mut key_specs: Vec<(Expr, SortKey)> = Vec::new();
    for term in &sel.order_by {
        // A positional ordinal that is not a bare, in-range integer literal — out
        // of range, or wrapped in a unary `+`/`-`, parenthesis, or `COLLATE` that
        // SQLite still reads as a position — defers to the tree-walker, which owns
        // the exact range error and never sorts by a constant.
        if super::positional_int(&term.expr).is_some()
            && !matches!(&term.expr, Expr::Literal(Literal::Integer(k)) if *k >= 1 && (*k as usize) <= count)
        {
            return Err(Error::Unsupported("VDBE: positional ORDER BY term"));
        }
        let expr = match super::resolve_order_index(&term.expr, &labels, count) {
            Some(idx) => projections[idx].0.clone(),
            None => term.expr.clone(),
        };
        let collation = c
            .explicit_collation(&term.expr)
            .or_else(|| c.col_collation(&expr))
            .unwrap_or_default();
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
    let gfin_key_colls = group_key_collations(&c, &group_keys);
    c.ops.push(Op::GroupFinalize {
        agg_kinds: agg_specs.iter().map(|(k, _, _, _, _, _, _)| *k).collect(),
        key_count: group_keys.len(),
        target: 0,
        key_collations: gfin_key_colls,
    });
    let gbody = c.ops.len();
    // Load this group's keys and aggregate finals into their registers.
    for k in 0..group_keys.len() {
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
    // Load each bare-column representative from the group's key vector (stored
    // right after the real keys by the fold, holding the first-seen row's value).
    for r in 0..repr_cols.len() {
        c.ops.push(Op::GroupKey {
            key: group_keys.len() + r,
            dest: grepr_start + r,
        });
    }
    // The emit phase has no current scan row: every column reference must be a
    // grouping key, aggregate, or bound representative (resolved via a binding). A
    // bare non-grouped column the collector did not pre-bind cannot be read here —
    // forbid raw column reads so it bails and the tree-walker handles it.
    c.forbid_raw_columns = true;
    // Admit a group-key-only correlated scalar/`EXISTS` subquery in the
    // projection / HAVING / ORDER BY, evaluated against a synthetic row of the
    // group's key values (`Op::GroupCorrelatedScalar` / `…Exists`). Requires every
    // group key to be a bare source column (so a key position maps to a source
    // column) and the single-min/max representative rule to be inactive (so the
    // stored key vector holds each group's true key values); otherwise the
    // subquery arm bails and the tree-walker handles it.
    if allow_correlated && all_col_keys && !single_minmax {
        c.group_emit_keys = key_col_indices.clone();
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
                    schema: None,
                    table: None,
                    column: label.clone(),
                    quoted: false,
                    span: Span::none(),
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
    // `SELECT DISTINCT`: drop a group whose projected output row duplicates one
    // already emitted (BINARY compare — non-BINARY output bailed above). This runs
    // after HAVING and before OFFSET/LIMIT and the sorter, so dedup precedes both
    // ordering and the row counters, matching SQLite.
    let distinct_skip = if sel.distinct {
        let at = c.ops.len();
        c.ops.push(Op::DistinctCheck {
            start: out_start,
            count,
            target: 0,
            collations: alloc::vec::Vec::new(),
        });
        Some(at)
    } else {
        None
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
    if let Some(at) = having_skip
        && let Op::IfFalse { target, .. } = &mut c.ops[at]
    {
        *target = gnext;
    }
    if let Some(at) = distinct_skip {
        // A duplicate group advances to the next without emitting (and without
        // entering the sorter on the ORDER BY path).
        if let Op::DistinctCheck { target, .. } = &mut c.ops[at] {
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
    if let Some(at) = limit_done
        && let Op::DecrJumpZero { target, .. } = &mut c.ops[at]
    {
        *target = gend;
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
        if let Some(at) = eoffset
            && let Op::IfPosDecr { target, .. } = &mut c.ops[at]
        {
            *target = snext;
        }
        if let Some(at) = elimit
            && let Op::DecrJumpZero { target, .. } = &mut c.ops[at]
        {
            *target = eend;
        }
    }

    // `LIMIT 0` jumps past the whole emit phase (here, just before Halt).
    let final_end = c.ops.len();
    if let Some(at) = limit_skip
        && let Op::IfFalse { target, .. } = &mut c.ops[at]
    {
        *target = final_end;
    }

    c.ops.push(Op::Halt);
    Ok(Program {
        ops: c.ops,
        subqueries: core::mem::take(&mut c.subqueries),
        n_registers: c.next_reg,
        columns: projections.iter().map(|(_, l)| l.clone()).collect(),
    })
}

/// Collect every column reference in `e` (recursing into nested subqueries) into
/// `out` as `(schema, table, name)`. Exhaustive over [`Expr`]; returns `Err` on a
/// windowed function (its `OVER` partition/order columns are not walked) so the
/// caller conservatively declines rather than silently missing a reference.
fn collect_all_columns(
    e: &Expr,
    out: &mut Vec<(Option<String>, Option<String>, String)>,
) -> core::result::Result<(), ()> {
    match e {
        Expr::Literal(_) | Expr::Parameter(_) => {}
        Expr::Column {
            schema,
            table,
            column,
            ..
        } => out.push((schema.clone(), table.clone(), column.clone())),
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Paren(expr)
        | Expr::Collate { expr, .. } => collect_all_columns(expr, out)?,
        Expr::Binary { left, right, .. } => {
            collect_all_columns(left, out)?;
            collect_all_columns(right, out)?;
        }
        Expr::Function {
            args,
            filter,
            order_by,
            over,
            ..
        } => {
            if over.is_some() {
                return Err(());
            }
            for a in args {
                collect_all_columns(a, out)?;
            }
            if let Some(f) = filter {
                collect_all_columns(f, out)?;
            }
            for o in order_by {
                collect_all_columns(&o.expr, out)?;
            }
        }
        Expr::InList { expr, list, .. } => {
            collect_all_columns(expr, out)?;
            for x in list {
                collect_all_columns(x, out)?;
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_all_columns(expr, out)?;
            collect_all_columns(low, out)?;
            collect_all_columns(high, out)?;
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            if let Some(o) = operand {
                collect_all_columns(o, out)?;
            }
            for (w, t) in when_then {
                collect_all_columns(w, out)?;
                collect_all_columns(t, out)?;
            }
            if let Some(x) = else_result {
                collect_all_columns(x, out)?;
            }
        }
        Expr::RowValue(xs) => {
            for x in xs {
                collect_all_columns(x, out)?;
            }
        }
        Expr::Subquery(s) => collect_select_columns(s, out)?,
        Expr::Exists { select, .. } => collect_select_columns(select, out)?,
        Expr::InSelect { expr, select, .. } => {
            collect_all_columns(expr, out)?;
            collect_select_columns(select, out)?;
        }
    }
    Ok(())
}

/// Collect every column reference in a (sub)query `sel` into `out`. Returns `Err`
/// on a compound/CTE/window-carrying/`FROM`-subquery shape the walker does not
/// fully account for, so the caller conservatively declines.
fn collect_select_columns(
    sel: &Select,
    out: &mut Vec<(Option<String>, Option<String>, String)>,
) -> core::result::Result<(), ()> {
    if !sel.compound.is_empty() || !sel.ctes.is_empty() || !sel.window_defs.is_empty() {
        return Err(());
    }
    for rc in &sel.columns {
        match rc {
            ResultColumn::Expr { expr, .. } => collect_all_columns(expr, out)?,
            ResultColumn::Wildcard | ResultColumn::TableWildcard(_) => {}
        }
    }
    if let Some(f) = &sel.from {
        // A subquery/TVF FROM source introduces its own scope the walker does not
        // model; decline. A plain table reference contributes no columns here.
        if f.first.subquery.is_some() {
            return Err(());
        }
        for j in &f.joins {
            if j.table.subquery.is_some() {
                return Err(());
            }
            if let Some(on) = &j.on {
                collect_all_columns(on, out)?;
            }
        }
    }
    if let Some(w) = &sel.where_clause {
        collect_all_columns(w, out)?;
    }
    for g in &sel.group_by {
        collect_all_columns(g, out)?;
    }
    if let Some(h) = &sel.having {
        collect_all_columns(h, out)?;
    }
    for o in &sel.order_by {
        collect_all_columns(&o.expr, out)?;
    }
    Ok(())
}

/// If `e` is a correlated scalar / `EXISTS` subquery whose every outer reference
/// resolves to a group-key column, return the matching [`GroupOut`] (the subquery
/// is to be stored at index `sub_index`); otherwise `None`, deferring to the
/// tree-walker. Conservative on every axis: any structure
/// [`collect_select_columns`] cannot fully account for, a three-part reference, or
/// any outer reference to a non-key column yields `None`. An inner column (bound
/// to the subquery's own `FROM`) does not resolve against the outer scope, so it
/// is correctly ignored; a name that also collides with a non-key outer column
/// resolves outward and conservatively declines.
fn group_correlated_output(
    c: &Compiler,
    e: &Expr,
    group_cols: &[usize],
    sub_index: usize,
) -> Option<GroupOut> {
    let (sel, out) = match e {
        Expr::Subquery(s) => (s.as_ref(), GroupOut::Sub(sub_index)),
        Expr::Exists { select, negated } => {
            (select.as_ref(), GroupOut::SubExists(sub_index, *negated))
        }
        _ => return None,
    };
    subquery_refs_only_group_keys(c, sel, group_cols).then_some(out)
}

/// Whether every column reference in subquery `sel` either stays inside the
/// subquery's own scope (does not resolve against the outer `c.columns`) or
/// resolves to one of the `group_cols` group-key columns — the condition under
/// which the subquery's value is well-defined for each group (evaluable against a
/// synthetic row of the group's keys). Conservative: a shape
/// [`collect_select_columns`] cannot fully account for, a three-part reference, or
/// any outer reference to a non-key column returns `false`.
fn subquery_refs_only_group_keys(c: &Compiler, sel: &Select, group_cols: &[usize]) -> bool {
    let mut cols = Vec::new();
    if collect_select_columns(sel, &mut cols).is_err() {
        return false;
    }
    for (schema, table, name) in &cols {
        if schema.is_some() {
            return false;
        }
        if let Ok(idx) = c.resolve_column(table.as_deref(), name)
            && !group_cols.contains(&idx)
        {
            return false;
        }
    }
    true
}

/// The compact plain-GROUP-BY path: every output column is a grouping key, a bare
/// aggregate, or a group-key-only correlated subquery (via
/// [`group_correlated_output`]), with no HAVING/ORDER BY/LIMIT. Folds the scan and
/// emits one row per group via [`Op::GroupEmit`].
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
    allow_correlated: bool,
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
        correlated_subqueries: allow_correlated,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
    };
    // Classify each output column as a grouping-key reference or an aggregate.
    // Column refs resolve through `resolve_column` (qualifier-aware) so a join's
    // qualified `t.col` and ambiguous bare names are handled like the tree-walker.
    // SQLite's single-min()/max() bare-column rule also governs the *displayed*
    // GROUP BY key: `SELECT a, min(b) … GROUP BY a` shows `a` from the min(b) row,
    // so when a group holds numerically-equal but differently-typed keys (`3` and
    // `3.0`) the printed key follows the extreme row. Route the projected key
    // column through the representative mechanism (which tracks the min/max
    // companion) in that case. With no min/max the representative is the group's
    // first-seen row, whose key value equals the stored key — so nothing changes;
    // with 2+ min/max the companion is ambiguous (SQLite unspecified) and the
    // stored key is kept.
    // This plain path has no HAVING/ORDER BY, so the lone min/max (if any) is in
    // the projection and its companion row is tracked by the fold. When present,
    // the displayed GROUP BY key follows that row too (see the key branch below).
    let single_minmax = super::single_minmax_arg(sel).is_some();
    let mut outputs: Vec<GroupOut> = Vec::new();
    let mut agg_specs: Vec<AggCallSpec> = Vec::new();
    let mut repr_cols: Vec<usize> = Vec::new();
    for (e, _) in projections {
        if is_aggregate_expr(e) {
            match agg_kind_distinct(e) {
                Some(spec) => {
                    outputs.push(GroupOut::Agg(agg_specs.len()));
                    agg_specs.push(spec);
                }
                None => return Err(Error::Unsupported("VDBE: unsupported aggregate")),
            }
        } else if let Expr::Column { table, column, .. } = e {
            let ci = c.resolve_column(table.as_deref(), column)?;
            match group_cols.iter().position(|&g| g == ci) {
                // A group-key column normally emits the stored key value, but when
                // a single min/max is active it must follow that extreme row (see
                // above), so it becomes a representative like a non-grouped column.
                Some(k) if !single_minmax => outputs.push(GroupOut::Key(k)),
                // A non-grouped bare column (or a key column under the single
                // min/max rule). SQLite emits the value from the group's
                // representative row — the first-seen row, or the min()/max()
                // companion when exactly one is present (tracked in the fold; >1
                // is gated after the loop).
                _ => {
                    let r = repr_cols.len();
                    repr_cols.push(ci);
                    outputs.push(GroupOut::Key(group_cols.len() + r));
                }
            }
        } else if allow_correlated
            && let Some(out) = group_correlated_output(&c, e, group_cols, c.subqueries.len())
        {
            // A correlated scalar / EXISTS subquery in the projection, admitted
            // only when every outer reference is a group key (see
            // `group_correlated_output`): its value is then well-defined per group
            // (the group holds one value for each key), and the interpreter
            // evaluates it against a synthetic row of the group's keys.
            match e {
                Expr::Subquery(sel) => c.subqueries.push(CorrelatedSub {
                    select: (**sel).clone(),
                }),
                Expr::Exists { select, .. } => c.subqueries.push(CorrelatedSub {
                    select: (**select).clone(),
                }),
                _ => unreachable!("group_correlated_output only matches subquery/exists"),
            }
            outputs.push(out);
        } else {
            return Err(Error::Unsupported(
                "VDBE: GROUP BY output must be key or aggregate",
            ));
        }
    }

    // Bare columns track exactly one min()/max() aggregate's companion row (handled
    // by `emit_group_fold`); with more than one, the companion is ambiguous and
    // SQLite leaves it unspecified — bail there rather than pick arbitrarily.
    if !repr_cols.is_empty()
        && agg_specs
            .iter()
            .filter(|(k, ..)| matches!(k, AggKind::Min | AggKind::Max))
            .count()
            > 1
    {
        return Err(Error::Unsupported(
            "VDBE: bare column with multiple min/max companions",
        ));
    }

    // Fold the row source (single cursor or nested-loop join) into per-group
    // aggregates, allocating the grouping-key and representative registers. The
    // plain path only ever has bare-column keys, lifted into `GroupKeySpec::Col`.
    let group_keys: Vec<GroupKeySpec> = group_cols.iter().map(|&i| GroupKeySpec::Col(i)).collect();
    emit_group_fold(&mut c, sel, &group_keys, &repr_cols, &agg_specs)?;

    let needs_sub = outputs
        .iter()
        .any(|o| matches!(o, GroupOut::Sub(_) | GroupOut::SubExists(..)));
    let emit_key_colls = group_key_collations(&c, &group_keys);
    c.ops.push(Op::GroupEmit {
        outputs,
        key_count: group_cols.len(),
        key_collations: emit_key_colls,
        agg_kinds: agg_specs.iter().map(|(k, _, _, _, _, _, _)| *k).collect(),
        group_cols: if needs_sub {
            group_cols.to_vec()
        } else {
            Vec::new()
        },
        n_cols: if needs_sub { columns.len() } else { 0 },
    });
    c.ops.push(Op::Halt);
    Ok(Program {
        ops: c.ops,
        subqueries: core::mem::take(&mut c.subqueries),
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
    compile_table_select_opts(sel, columns, tables, affinities, collations, rowid, false)
}

/// Like [`compile_table_select`], but `allow_correlated` opts the single-cursor
/// scan into compiling a correlated scalar / `EXISTS` subquery to an
/// [`Op::CorrelatedScalar`] / [`Op::CorrelatedExists`] callback op (B5c-2). Only
/// the live single-table scan sets it, because only that path supplies the
/// [`SubqueryEval`] callback the op needs at run time; every other caller leaves
/// it `false`, so an unfoldable subquery defers to the tree-walker exactly as
/// before.
pub fn compile_table_select_opts(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    rowid: bool,
    allow_correlated: bool,
) -> Result<Program> {
    if !sel.compound.is_empty() {
        return Err(Error::Unsupported("VDBE: only plain table projections"));
    }
    // Expand the projection list to concrete expressions/labels (supporting `*`).
    let projections = expand_projections(sel, columns, tables)?;
    // A row-level DISTINCT dedups under each output column's collation. The plain
    // single-table scan resolves an explicit projection `COLLATE` into its
    // `DistinctCheck` (below), so `SELECT DISTINCT a COLLATE NOCASE FROM t` runs on
    // the VDBE. The grouped / aggregate DISTINCT paths still dedup under BINARY, so
    // an explicit `COLLATE` there defers to the tree-walker.
    let grouped_or_aggregate =
        !sel.group_by.is_empty() || projections.iter().any(|(e, _)| is_aggregate_expr(e));
    if sel.distinct && grouped_or_aggregate && projections_have_explicit_collation(&projections) {
        return Err(Error::Unsupported("VDBE: explicit COLLATE with DISTINCT"));
    }
    // A constant `LIMIT 0` yields no rows for any query shape: emit a program
    // that halts immediately (the column labels are still reported).
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            subqueries: Vec::new(),
            n_registers: projections.len(),
            columns: projections.iter().map(|(_, l)| l.clone()).collect(),
        });
    }
    // GROUP BY folds the scan into one row per group.
    if !sel.group_by.is_empty() {
        // A `GROUP BY <constant>` (every key a non-positional row-constant, e.g.
        // `GROUP BY 1+1` / `'x'` / `NULL`) forms a single group over a non-empty
        // table and no rows over an empty one. With an all-aggregate projection
        // that is a bare aggregate gated on a non-empty scan, so route it there.
        // (A bare positional integer `GROUP BY 1` is excluded — it groups by an
        // output column — as is a non-aggregate projection.)
        if sel
            .group_by
            .iter()
            .all(|e| super::positional_int(e).is_none() && is_row_constant(e))
            && projections.iter().all(|(e, _)| is_aggregate_expr(e))
        {
            return compile_aggregate_select(
                sel,
                columns,
                tables,
                affinities,
                collations,
                &projections,
                true,
            );
        }
        return compile_group_select(
            sel,
            columns,
            tables,
            affinities,
            collations,
            &projections,
            None,
            allow_correlated,
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
            false,
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
        correlated_subqueries: allow_correlated,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
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
        build_sort_keys(&c, sel, &projections, count)?
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
    // Each projected column dedups under its *resolved* collation — an explicit
    // `COLLATE` if present, else the underlying column's declared collation, else
    // BINARY — exactly as `build_sort_keys` resolves an ORDER BY term's collation.
    // (`c.collations` is indexed by the *source* columns, not the projected output,
    // so it cannot be used directly here.) This lets a `NOCASE`/`RTRIM`/custom-
    // collation column — and an explicit projection `COLLATE` — run on the VDBE.
    let distinct_skip = if sel.distinct {
        let colls: Vec<Collation> = projections
            .iter()
            .map(|(e, _)| {
                c.explicit_collation(e)
                    .or_else(|| c.col_collation(e))
                    .unwrap_or_default()
            })
            .collect();
        let at = c.ops.len();
        c.ops.push(Op::DistinctCheck {
            start: 0,
            count,
            target: 0,
            collations: colls,
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
    if let Some(at) = skip
        && let Op::IfFalse { target, .. } = &mut c.ops[at]
    {
        *target = next; // a filtered-out row advances to the next
    }
    if let Some(at) = offset_skip
        && let Op::IfPosDecr { target, .. } = &mut c.ops[at]
    {
        *target = next; // a skipped (offset) row advances to the next
    }
    if let Some(at) = distinct_skip
        && let Op::DistinctCheck { target, .. } = &mut c.ops[at]
    {
        *target = next; // a duplicate row advances to the next
    }
    let end = c.ops.len();
    if let Op::Rewind { target } = &mut c.ops[rewind] {
        *target = end;
    }
    if let Some(at) = limit_skip
        && let Op::IfFalse { target, .. } = &mut c.ops[at]
    {
        *target = end;
    }
    if let Some(at) = limit_done
        && let Op::DecrJumpZero { target, .. } = &mut c.ops[at]
    {
        *target = end;
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
        if let Some(at) = eoffset
            && let Op::IfPosDecr { target, .. } = &mut c.ops[at]
        {
            *target = snext; // a skipped (offset) row advances to the next
        }
        if let Some(at) = elimit
            && let Op::DecrJumpZero { target, .. } = &mut c.ops[at]
        {
            *target = eend;
        }
    }
    c.ops.push(Op::Halt);
    Ok(Program {
        ops: c.ops,
        subqueries: core::mem::take(&mut c.subqueries),
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
                // Qualify each expanded column with its source table. Across a join
                // whose sources share a column name (`SELECT * FROM t JOIN u` where
                // both have `g`), a bare reference would be ambiguous; the qualifier
                // (from the parallel `tables` slice) picks each source's own column,
                // so `*` expands to every column the way the tree-walker does. The
                // output *label* stays the bare column name, matching SQLite. A
                // source with no table qualifier (an empty string) is left unqualified.
                for (i, name) in columns.iter().enumerate() {
                    let table = tables.get(i).filter(|t| !t.is_empty()).cloned();
                    projections.push((
                        Expr::Column {
                            schema: None,
                            table,
                            column: name.clone(),
                            quoted: false,
                            span: Span::none(),
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
                                schema: None,
                                table: Some(q.clone()),
                                column: name.clone(),
                                quoted: false,
                                span: Span::none(),
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
    projections: &[(Expr, String)],
    count: usize,
) -> Result<Vec<(Expr, SortKey)>> {
    let labels: Vec<String> = projections.iter().map(|(_, l)| l.clone()).collect();
    let mut key_specs: Vec<(Expr, SortKey)> = Vec::new();
    for term in &sel.order_by {
        // A positional ordinal that is not a bare, in-range integer literal — out
        // of range, or wrapped in a unary `+`/`-`, parenthesis, or `COLLATE` that
        // SQLite still reads as a position (`ORDER BY -1`) — defers to the
        // tree-walker, which owns the exact range error and never sorts by a
        // constant.
        if super::positional_int(&term.expr).is_some()
            && !matches!(&term.expr, Expr::Literal(Literal::Integer(k)) if *k >= 1 && (*k as usize) <= count)
        {
            return Err(Error::Unsupported("VDBE: positional ORDER BY term"));
        }
        // Resolve the term to an output column where it names one — by position,
        // a result-column alias, or such a reference wrapped in `COLLATE`/parens
        // (`resolve_order_index` mirrors the tree-walker). SQLite matches an
        // `ORDER BY` name against the result aliases FIRST, even when a base table
        // column shares the name, so `SELECT b AS a FROM t ORDER BY a` sorts by
        // `b`. The sort value comes from that output column's expression; an
        // explicit `COLLATE` on the term still sets the collation, otherwise the
        // resolved column's own collation applies.
        let expr = match super::resolve_order_index(&term.expr, &labels, count) {
            Some(idx) => projections[idx].0.clone(),
            None => term.expr.clone(),
        };
        let collation = c
            .explicit_collation(&term.expr)
            .or_else(|| c.col_collation(&expr))
            .unwrap_or_default();
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
#[allow(clippy::too_many_arguments)]
pub fn compile_join2(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    boundaries: &[usize],
    allow_correlated: bool,
    loop_order: &[usize],
) -> Result<Program> {
    let n = boundaries.len();
    debug_assert!(n >= 2 && boundaries[n - 1] == columns.len());
    // `loop_order[p]` is the cursor nested at loop position `p` (0 = outermost).
    // Empty means the identity `[0, 1, …, n-1]` (leftmost source outermost). A
    // non-identity permutation drives from a different table — used for the
    // cost-based two-table rowid-inner swap (drive the second table, the first
    // becomes the inner) — which only reorders row *emission*; the cursor→column
    // mapping (via `boundaries`) is unchanged, so projections/WHERE are unaffected.
    let order: Vec<usize> = if loop_order.is_empty() {
        (0..n).collect()
    } else {
        debug_assert_eq!(loop_order.len(), n);
        loop_order.to_vec()
    };
    if !sel.compound.is_empty() || !sel.group_by.is_empty() || sel.having.is_some() {
        return Err(Error::Unsupported("VDBE: join shape not nested-loopable"));
    }
    reject_aggregate_or_window_in_predicates(sel)?;
    let projections = expand_projections(sel, columns, tables)?;
    // A row-level DISTINCT dedups each projected column under its resolved
    // collation — this nested-loop join's `DistinctCheck` resolves an explicit
    // projection `COLLATE` (below), exactly like the single-table scan path, so
    // `SELECT DISTINCT a COLLATE NOCASE FROM a JOIN b …` runs on the VDBE.
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return Err(Error::Unsupported("VDBE: aggregate over a join"));
    }
    let count = projections.len();
    // A constant `LIMIT 0` yields nothing (labels still reported).
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            subqueries: Vec::new(),
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
        correlated_subqueries: allow_correlated,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
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
        build_sort_keys(&c, sel, &projections, count)?
    } else {
        Vec::new()
    };
    let key_start = c.next_reg;
    for _ in &key_specs {
        c.alloc();
    }
    // N-deep nested loop: `RewindC [ RewindC [ … [ RewindC <body> NextC ] …
    // NextC ] NextC` — position 0 outermost. The cursor at each position is
    // `order[p]` (identity `[0,1,…]` unless a swap drives a different table).
    let mut rewind_at = alloc::vec![0usize; n];
    for (p, slot) in rewind_at.iter_mut().enumerate() {
        *slot = c.ops.len();
        c.ops.push(Op::RewindC {
            cursor: order[p],
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
    // duplicate must not consume the budget nor enter the sorter). Each projected
    // column dedups under its resolved collation (declared column collation or an
    // explicit `COLLATE BINARY`; a non-BINARY explicit `COLLATE` deferred above),
    // exactly like the single-table scan path.
    let distinct_skip = if sel.distinct {
        let colls: Vec<Collation> = projections
            .iter()
            .map(|(e, _)| {
                c.explicit_collation(e)
                    .or_else(|| c.col_collation(e))
                    .unwrap_or_default()
            })
            .collect();
        let at = c.ops.len();
        c.ops.push(Op::DistinctCheck {
            start: 0,
            count,
            target: 0,
            collations: colls,
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
    // Emit the `NextC`s innermost-first; `next_at[p]` is the address of the
    // position-`p` cursor's advance. The innermost re-runs the body; an outer one
    // re-runs the next-inner position's `RewindC`.
    let mut next_at = alloc::vec![0usize; n];
    for p in (0..n).rev() {
        next_at[p] = c.ops.len();
        let target = if p == n - 1 { body } else { rewind_at[p + 1] };
        c.ops.push(Op::NextC {
            cursor: order[p],
            target,
        });
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
        if let Some(at) = eoffset
            && let Op::IfPosDecr { target, .. } = &mut c.ops[at]
        {
            *target = snext;
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
    if let Some(at) = skip
        && let Op::IfFalse { target, .. } = &mut c.ops[at]
    {
        *target = inner_next;
    }
    if let Some(at) = distinct_skip
        && let Op::DistinctCheck { target, .. } = &mut c.ops[at]
    {
        *target = inner_next; // a duplicate row advances the innermost cursor
    }
    if let Some(at) = offset_skip
        && let Op::IfPosDecr { target, .. } = &mut c.ops[at]
    {
        *target = inner_next;
    }
    if let Some(at) = limit_done
        && let Op::DecrJumpZero { target, .. } = &mut c.ops[at]
    {
        *target = end;
    }
    // The sorted emit loop's LIMIT halts the whole program (its OFFSET was
    // backpatched inline above).
    if let Some(at) = elimit
        && let Op::DecrJumpZero { target, .. } = &mut c.ops[at]
    {
        *target = end;
    }
    Ok(Program {
        ops: c.ops,
        subqueries: core::mem::take(&mut c.subqueries),
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
    // The aggregate min/max reduction and DISTINCT dedup fold under the argument's
    // collation (`AggStep.collation`), so a non-BINARY declared column collation runs
    // on the VDBE; an explicit `COLLATE` argument still defers via `agg_kind_distinct`.
    let projections = expand_projections(sel, columns, tables)?;
    // A row-level DISTINCT dedups under each output column's collation; the VDBE's
    // DistinctCheck compares under BINARY, so an explicit `COLLATE` on a projection
    // (`SELECT DISTINCT a COLLATE NOCASE`) must defer to the tree-walker.
    if sel.distinct && projections_have_explicit_collation(&projections) {
        return Err(Error::Unsupported("VDBE: explicit COLLATE with DISTINCT"));
    }
    // Every projection must be exactly one supported aggregate call. DISTINCT is
    // supported (the collected values are deduped at fold time, BINARY only — a
    // non-BINARY collation already bailed above).
    let mut slots: Vec<AggCallSpec> = Vec::new();
    for (e, _) in &projections {
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
        cursor_boundaries: Some(boundaries.to_vec()),
        correlated_subqueries: false,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
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
    // Fold the surviving combined row into every aggregate slot.
    for (slot, (kind, arg, distinct, filter, order, sep, arg2)) in slots.iter().enumerate() {
        let arg_reg = match arg {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let arg2_reg = match arg2 {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let filter_reg = match filter {
            Some(expr) => Some(c.compile_expr(expr)?),
            None => None,
        };
        let order_keys = compile_order_keys(&mut c, order)?;
        let collation = arg
            .as_ref()
            .and_then(|e| c.explicit_collation(e).or_else(|| c.col_collation(e)))
            .unwrap_or_default();
        c.ops.push(Op::AggStep {
            slot,
            kind: *kind,
            arg: arg_reg,
            arg2: arg2_reg,
            distinct: *distinct,
            filter: filter_reg,
            order: order_keys,
            sep: sep.clone(),
            collation,
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
    if let Some(at) = skip
        && let Op::IfFalse { target, .. } = &mut c.ops[at]
    {
        *target = next_at[n - 1];
    }
    // Finalize each slot into its output register, then emit the single row.
    for (slot, (kind, _, _, _, _, _, _)) in slots.iter().enumerate() {
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
        subqueries: core::mem::take(&mut c.subqueries),
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
#[allow(clippy::too_many_arguments)] // cohesive: the combined column space + boundaries + gate
pub fn compile_group_join(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    boundaries: &[usize],
    allow_correlated: bool,
) -> Result<Program> {
    let n = boundaries.len();
    debug_assert!(n >= 2 && boundaries[n - 1] == columns.len());
    // The compound case is handled per-arm elsewhere; a grouped *join* arm with a
    // tail compound is out of scope here.
    if !sel.compound.is_empty() || sel.group_by.is_empty() {
        return Err(Error::Unsupported("VDBE: GROUP BY join requires GROUP BY"));
    }
    let projections = expand_projections(sel, columns, tables)?;
    // A row-level DISTINCT dedups under each output column's collation; the VDBE's
    // DistinctCheck compares under BINARY, so an explicit `COLLATE` on a projection
    // (`SELECT DISTINCT a COLLATE NOCASE`) must defer to the tree-walker.
    if sel.distinct && projections_have_explicit_collation(&projections) {
        return Err(Error::Unsupported("VDBE: explicit COLLATE with DISTINCT"));
    }
    compile_group_select(
        sel,
        columns,
        tables,
        affinities,
        collations,
        &projections,
        Some(boundaries),
        // A group-key-correlated subquery in the projection is admissible over a
        // join too: `group_cols` index into the combined column space and the
        // synthetic per-group row is built at combined width, so the same guard and
        // `GroupEmit` machinery apply. The caller supplies a `SubqueryEval` over the
        // combined columns when the compiled program carries a subquery.
        allow_correlated,
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
    reject_aggregate_or_window_in_predicates(sel)?;
    // DISTINCT compares output rows under BINARY; a non-BINARY column collation
    // would diverge, so defer those to the tree-walker (as the inner-join and
    // single-table paths do).
    if sel.distinct && collations.iter().any(|cl| *cl != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation with DISTINCT",
        ));
    }
    let projections = expand_projections(sel, columns, tables)?;
    // A row-level DISTINCT dedups under each output column's collation; the VDBE's
    // DistinctCheck compares under BINARY, so an explicit `COLLATE` on a projection
    // (`SELECT DISTINCT a COLLATE NOCASE`) must defer to the tree-walker.
    if sel.distinct && projections_have_explicit_collation(&projections) {
        return Err(Error::Unsupported("VDBE: explicit COLLATE with DISTINCT"));
    }
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return Err(Error::Unsupported("VDBE: aggregate over a left join"));
    }
    let count = projections.len();
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            subqueries: Vec::new(),
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
        correlated_subqueries: false,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
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
        build_sort_keys(&c, sel, &projections, count)?
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
            collations: alloc::vec::Vec::new(),
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
            collations: alloc::vec::Vec::new(),
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
        if let Some(at) = eoffset
            && let Op::IfPosDecr { target, .. } = &mut c.ops[at]
        {
            *target = snext;
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
        subqueries: core::mem::take(&mut c.subqueries),
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
            collations: alloc::vec::Vec::new(),
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
    reject_aggregate_or_window_in_predicates(sel)?;
    // DISTINCT compares output rows under BINARY; a non-BINARY column collation
    // would diverge, so defer those to the tree-walker.
    if sel.distinct && collations.iter().any(|cl| *cl != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation with DISTINCT",
        ));
    }
    let projections = expand_projections(sel, columns, tables)?;
    // A row-level DISTINCT dedups under each output column's collation; the VDBE's
    // DistinctCheck compares under BINARY, so an explicit `COLLATE` on a projection
    // (`SELECT DISTINCT a COLLATE NOCASE`) must defer to the tree-walker.
    if sel.distinct && projections_have_explicit_collation(&projections) {
        return Err(Error::Unsupported("VDBE: explicit COLLATE with DISTINCT"));
    }
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return Err(Error::Unsupported("VDBE: aggregate over a full join"));
    }
    let count = projections.len();
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            subqueries: Vec::new(),
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
        correlated_subqueries: false,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
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
        build_sort_keys(&c, sel, &projections, count)?
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
        if let Some(at) = eoffset
            && let Op::IfPosDecr { target, .. } = &mut c.ops[at]
        {
            *target = snext;
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
        subqueries: core::mem::take(&mut c.subqueries),
        n_registers: c.next_reg,
        columns: projections.into_iter().map(|(_, l)| l).collect(),
    })
}

/// Shared context for the recursive [`emit_join_level`] emitter of an N-table
/// outer-join chain. The per-cursor `kinds`/`ons`/`matched_regs` are indexed by
/// `k - 1` for cursor `k` (cursor 0 is the always-preserved base table, so it has
/// no join kind, `ON`, or matched flag).
struct LeftJoinNCtx<'a> {
    sel: &'a Select,
    projections: &'a [(Expr, String)],
    count: usize,
    distinct: bool,
    offset_reg: Option<usize>,
    limit_reg: Option<usize>,
    key_start: usize,
    key_specs: &'a [(Expr, SortKey)],
    ordering: bool,
    /// `kinds[k - 1]` is the join (`Left` or `Inner`) that brings in cursor `k`.
    kinds: &'a [JoinKind],
    /// `ons[k - 1]` is cursor `k`'s `ON` predicate (`None` ⇒ always matches).
    ons: &'a [Option<Expr>],
    /// `matched_regs[k - 1]` is cursor `k`'s per-outer-row "any inner row matched"
    /// flag register; reset to 0 on each entry, set to 1 by a matching inner row.
    matched_regs: &'a [usize],
    n_cursors: usize,
}

/// Recursively emit the nested loop for cursor `k` (1-based) of a left-deep
/// outer-join chain, then — at `k == n_cursors` — the leaf row emit (WHERE gate +
/// projection + DISTINCT + sorter/OFFSET/LIMIT). For a `Left` level a fully
/// unmatched outer row null-pads cursor `k` (`NullRow`) and still recurses into the
/// inner cursors (whose `ON`s may reference the now-NULL columns); an `Inner` level
/// simply yields nothing when no inner row matches. The fold order (cursor 0
/// outermost, innermost advancing fastest, each level's matches in scan order then
/// its null-padded row) matches SQLite's left-deep join row order. `limit_fixups`
/// collects the leaf `DecrJumpZero` op indices, which the caller backpatches to the
/// program's `Halt`.
fn emit_join_level(
    c: &mut Compiler,
    k: usize,
    ctx: &LeftJoinNCtx,
    limit_fixups: &mut Vec<usize>,
) -> Result<()> {
    fn bp(ops: &mut [Op], at: usize, tgt: usize) {
        match &mut ops[at] {
            Op::IfFalse { target, .. }
            | Op::IfPosDecr { target, .. }
            | Op::DecrJumpZero { target, .. }
            | Op::Goto { target }
            | Op::DistinctCheck { target, .. }
            | Op::RewindC { target, .. }
            | Op::NextC { target, .. } => *target = tgt,
            _ => {}
        }
    }
    if k == ctx.n_cursors {
        // Leaf: gate the fully-assembled row on WHERE, then emit it.
        let where_skip = match &ctx.sel.where_clause {
            Some(pred) => {
                let r = c.compile_expr(pred)?;
                let at = c.ops.len();
                c.ops.push(Op::IfFalse { reg: r, target: 0 });
                Some(at)
            }
            None => None,
        };
        let sorter = if ctx.ordering {
            Some((ctx.key_start, ctx.key_specs))
        } else {
            None
        };
        let (offset_skip, limit_done, distinct_skip) = emit_output_row(
            c,
            ctx.projections,
            ctx.count,
            ctx.offset_reg,
            ctx.limit_reg,
            sorter,
            ctx.distinct,
        )?;
        let cont = c.ops.len();
        if let Some(at) = where_skip {
            bp(&mut c.ops, at, cont);
        }
        if let Some(at) = offset_skip {
            bp(&mut c.ops, at, cont);
        }
        if let Some(at) = distinct_skip {
            bp(&mut c.ops, at, cont);
        }
        if let Some(at) = limit_done {
            limit_fixups.push(at);
        }
        return Ok(());
    }
    let mreg = ctx.matched_regs[k - 1];
    let kind = ctx.kinds[k - 1];
    let on = &ctx.ons[k - 1];
    c.ops.push(Op::Integer {
        value: 0,
        dest: mreg,
    });
    let rewind = c.ops.len();
    c.ops.push(Op::RewindC {
        cursor: k,
        target: 0,
    }); // empty cursor → null-check
    let loopk = c.ops.len();
    // ON gate: a non-matching inner row advances without recursing.
    let on_skip = match on {
        Some(pred) => {
            let r = c.compile_expr(pred)?;
            let at = c.ops.len();
            c.ops.push(Op::IfFalse { reg: r, target: 0 });
            Some(at)
        }
        None => None,
    };
    c.ops.push(Op::Integer {
        value: 1,
        dest: mreg,
    });
    emit_join_level(c, k + 1, ctx, limit_fixups)?;
    let nextk = c.ops.len();
    c.ops.push(Op::NextC {
        cursor: k,
        target: loopk,
    });
    if let Some(at) = on_skip {
        bp(&mut c.ops, at, nextk);
    }
    // Null-check (also the empty-cursor target).
    let nullcheck = c.ops.len();
    bp(&mut c.ops, rewind, nullcheck);
    if matches!(kind, JoinKind::Left) {
        let if_unmatched = c.ops.len();
        c.ops.push(Op::IfFalse {
            reg: mreg,
            target: 0,
        }); // not matched → null-pad
        let goto_after = c.ops.len();
        c.ops.push(Op::Goto { target: 0 }); // matched → after
        let null_body = c.ops.len();
        bp(&mut c.ops, if_unmatched, null_body);
        c.ops.push(Op::NullRow { cursor: k });
        emit_join_level(c, k + 1, ctx, limit_fixups)?;
        let after = c.ops.len();
        bp(&mut c.ops, goto_after, after);
    }
    // An `Inner` level needs no null-pad: an unmatched outer combination simply
    // produces no row (control falls through to the caller).
    Ok(())
}

/// Compile an N-table left-deep chain of `LEFT`/`INNER` joins
/// (`SELECT … FROM a LEFT JOIN b ON … [LEFT|INNER] JOIN c ON … …`) into a single
/// null-padding nested-loop program (B5b-1, N-table generalization of
/// [`compile_left_join2`]). `columns`/`tables`/`affinities`/`collations` are the
/// tables' arrays concatenated left-to-right; `boundaries` holds the cumulative
/// per-cursor column counts. `kinds[i]`/`ons[i]` describe the join that brings in
/// cursor `i + 1` (cursor 0 is the always-preserved base). Each join's `ON` gates
/// matches at its own level (it is NOT merged into `WHERE`, since a `LEFT` level's
/// unmatched row must still be null-padded); `WHERE` filters the fully-assembled
/// row at the leaf. Supports projection + WHERE + DISTINCT (BINARY) + ORDER BY
/// (staged through one sorter) + constant LIMIT/OFFSET; returns `Unsupported` for
/// GROUP BY / aggregates / HAVING. The caller routes a chain of ≥ 2 joins, all
/// `LEFT`/`INNER` with at least one `LEFT`, no `NATURAL`/`USING`, here; any other
/// shape (or a too-deep chain) falls back to the tree-walker.
#[allow(clippy::too_many_arguments)]
pub fn compile_left_join_n(
    sel: &Select,
    columns: &[String],
    tables: &[String],
    affinities: &[Affinity],
    collations: &[Collation],
    boundaries: &[usize],
    kinds: &[JoinKind],
    ons: &[Option<Expr>],
) -> Result<Program> {
    let n_cursors = boundaries.len();
    debug_assert!(n_cursors >= 2 && boundaries[n_cursors - 1] == columns.len());
    debug_assert!(kinds.len() == n_cursors - 1 && ons.len() == n_cursors - 1);
    if !sel.compound.is_empty() || !sel.group_by.is_empty() || sel.having.is_some() {
        return Err(Error::Unsupported(
            "VDBE: outer-join shape not nested-loopable",
        ));
    }
    reject_aggregate_or_window_in_predicates(sel)?;
    if sel.distinct && collations.iter().any(|cl| *cl != Collation::Binary) {
        return Err(Error::Unsupported(
            "VDBE: non-BINARY collation with DISTINCT",
        ));
    }
    let projections = expand_projections(sel, columns, tables)?;
    // A row-level DISTINCT dedups under each output column's collation; the VDBE's
    // DistinctCheck compares under BINARY, so an explicit `COLLATE` on a projection
    // (`SELECT DISTINCT a COLLATE NOCASE`) must defer to the tree-walker.
    if sel.distinct && projections_have_explicit_collation(&projections) {
        return Err(Error::Unsupported("VDBE: explicit COLLATE with DISTINCT"));
    }
    if projections.iter().any(|(e, _)| is_aggregate_expr(e)) {
        return Err(Error::Unsupported("VDBE: aggregate over an outer join"));
    }
    let count = projections.len();
    if matches!(&sel.limit, Some(Expr::Literal(Literal::Integer(0)))) {
        return Ok(Program {
            ops: alloc::vec![Op::Halt],
            subqueries: Vec::new(),
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
        correlated_subqueries: false,
        subqueries: Vec::new(),
        group_emit_keys: Vec::new(),
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
    let matched_regs: Vec<usize> = (0..n_cursors - 1).map(|_| c.alloc()).collect();
    let ordering = !sel.order_by.is_empty();
    let key_specs = if ordering {
        build_sort_keys(&c, sel, &projections, count)?
    } else {
        Vec::new()
    };
    let key_start = c.next_reg;
    for _ in &key_specs {
        c.alloc();
    }
    let mut limit_fixups: Vec<usize> = Vec::new();
    let ctx = LeftJoinNCtx {
        sel,
        projections: &projections,
        count,
        distinct: sel.distinct,
        offset_reg,
        limit_reg,
        key_start,
        key_specs: &key_specs,
        ordering,
        kinds,
        ons,
        matched_regs: &matched_regs,
        n_cursors,
    };
    // Outermost cursor 0 loop (the always-preserved base table).
    let rewind0 = c.ops.len();
    c.ops.push(Op::RewindC {
        cursor: 0,
        target: 0,
    });
    let loop0 = c.ops.len();
    emit_join_level(&mut c, 1, &ctx, &mut limit_fixups)?;
    c.ops.push(Op::NextC {
        cursor: 0,
        target: loop0,
    });
    let scan_done = c.ops.len();
    // With ORDER BY, sort then walk the sorter applying OFFSET then LIMIT to the
    // ordered output; without it, `scan_done` is just the Halt point.
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
        if let Some(at) = eoffset
            && let Op::IfPosDecr { target, .. } = &mut c.ops[at]
        {
            *target = snext;
        }
        elimit
    } else {
        None
    };
    let end = c.ops.len();
    c.ops.push(Op::Halt);
    if let Op::RewindC { target, .. } = &mut c.ops[rewind0] {
        *target = scan_done; // empty base table → no rows (drains the empty sorter)
    }
    for at in limit_fixups {
        if let Op::DecrJumpZero { target, .. } = &mut c.ops[at] {
            *target = end;
        }
    }
    if let Some(at) = elimit
        && let Op::DecrJumpZero { target, .. } = &mut c.ops[at]
    {
        *target = end;
    }
    Ok(Program {
        ops: c.ops,
        subqueries: core::mem::take(&mut c.subqueries),
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
    /// When `true`, a scalar / `EXISTS` subquery the inline const path cannot fold
    /// (a real `FROM`, a correlated reference, …) is compiled to an
    /// [`Op::CorrelatedScalar`] / [`Op::CorrelatedExists`] callback op instead of
    /// deferring — the interpreter re-runs it per outer row through the tree-walker
    /// (B5c-2). Enabled only for the live single-table scan (which supplies the
    /// [`SubqueryEval`] callback); every other path leaves it `false` so behaviour
    /// is unchanged.
    correlated_subqueries: bool,
    /// Correlated subqueries registered by the compiler (parallel to the `sub`
    /// index in the emitted ops); moved into [`Program::subqueries`].
    subqueries: Vec<CorrelatedSub>,
    /// When non-empty, the compiler is emitting the *general* grouped path's
    /// second-pass body (over finalized groups, no scan row), and these are the
    /// source-column indices of the group keys (all bare columns). A correlated
    /// scalar/`EXISTS` subquery referencing only those keys then compiles to a
    /// [`Op::GroupCorrelatedScalar`] / [`Op::GroupCorrelatedExists`] callback op,
    /// which the interpreter evaluates against a synthetic row built from the
    /// current group's key values. Empty everywhere else, so the ordinary
    /// [`Op::CorrelatedScalar`] path (or a bail) applies.
    group_emit_keys: Vec<usize>,
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
            if let Some(t) = table
                && !self
                    .tables
                    .get(i)
                    .is_some_and(|tn| tn.eq_ignore_ascii_case(t))
            {
                continue;
            }
            if found.is_some() {
                // A bare name matching two tables is ambiguous.
                return Err(Error::Unsupported("VDBE: ambiguous column reference"));
            }
            found = Some(i);
        }
        // A rowid alias (`rowid`/`_rowid_`/`oid`) resolves to the hidden rowid slot
        // of a single-table scan — unless a real column already shadows the name.
        if found.is_none()
            && let Some(ri) = self.rowid_index
        {
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
        found.ok_or(Error::Unsupported("VDBE: unresolved column reference"))
    }

    /// The comparison affinity an expression contributes (mirrors the
    /// tree-walker's `expr_affinity`): a column's declared affinity, a `CAST`'s
    /// target affinity, transparent through parentheses, else `None` (a literal
    /// or computed value has no affinity).
    fn expr_affinity(&self, expr: &Expr) -> Option<Affinity> {
        match expr {
            Expr::Column { table, column, .. } => self
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
            Expr::Collate { expr, collation } => crate::value::resolve_collation_name(collation)
                .or_else(|| self.explicit_collation(expr)),
            _ => None,
        }
    }

    /// The IMPLICIT (declared) collation of the column this operand resolves to
    /// (through parentheses / a `COLLATE`), if any.
    fn implicit_collation(&self, expr: &Expr) -> Option<Collation> {
        match expr {
            Expr::Column { table, column, .. } => self
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
            Expr::Column { table, column, .. } => {
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
                ..
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
            // An uncorrelated, FROM-less scalar subquery `(SELECT <e> [WHERE <p>])`
            // yields its single column's value for the one (rowless) row, or NULL
            // when a `WHERE` predicate filters that row out. Inline it: default the
            // result register to NULL, gate on the optional predicate, then
            // overwrite it with the projected value when the row qualifies.
            // Anything outside this grammar — a real `FROM`, `WITH`/window defs,
            // `GROUP BY`/`HAVING`/compound, `ORDER BY`/`LIMIT`/`OFFSET`, a
            // multi-column or aggregate projection, or a correlated column
            // reference (which fails to resolve in this rowless scope) — propagates
            // `Unsupported` and defers to the tree-walker, which evaluates or
            // rejects it exactly as SQLite does.
            Expr::Subquery(sel) => {
                // Live single-table scan (B5c-2): re-evaluate the subquery per outer
                // row through the [`SubqueryEval`] callback (which pushes the outer
                // row as an outer frame and re-runs the tree-walker), so a
                // *correlated* reference resolves to the outer value. Every subquery
                // reaching compilation here is already unfoldable (the non-correlated
                // ones were folded to constants before this program was compiled), so
                // routing them all through the callback is both correct and identical
                // to the tree-walker. Only enabled on the live path (`allow_correlated`),
                // where the callback is supplied; every other path keeps the inline
                // const grammar below.
                // General grouped path second-pass body: evaluate against a
                // synthetic row of the current group's keys, admitted only when
                // every outer reference is a group key (see `group_emit_keys`).
                if !self.group_emit_keys.is_empty() {
                    if !subquery_refs_only_group_keys(self, sel, &self.group_emit_keys) {
                        return Err(Error::Unsupported(
                            "VDBE: grouped correlated subquery references a non-key column",
                        ));
                    }
                    let sub = self.subqueries.len();
                    self.subqueries.push(CorrelatedSub {
                        select: (**sel).clone(),
                    });
                    self.ops.push(Op::GroupCorrelatedScalar {
                        sub,
                        dest,
                        group_cols: self.group_emit_keys.clone(),
                        n_cols: self.columns.len(),
                    });
                    return Ok(());
                }
                if self.correlated_subqueries {
                    let sub = self.subqueries.len();
                    self.subqueries.push(CorrelatedSub {
                        select: (**sel).clone(),
                    });
                    self.ops.push(Op::CorrelatedScalar { sub, dest });
                    return Ok(());
                }
                if sel.from.is_some()
                    || !sel.ctes.is_empty()
                    || !sel.window_defs.is_empty()
                    || !sel.group_by.is_empty()
                    || sel.having.is_some()
                    || !sel.compound.is_empty()
                    || !sel.order_by.is_empty()
                    || sel.limit.is_some()
                    || sel.offset.is_some()
                {
                    return Err(Error::Unsupported("VDBE: scalar subquery shape"));
                }
                // A scalar subquery must yield exactly one column; a multi-column
                // inner (`(SELECT 1, 2)`) is an error SQLite raises at prepare —
                // defer so the tree-walker reports it.
                let [ResultColumn::Expr { expr: inner, .. }] = &sel.columns[..] else {
                    return Err(Error::Unsupported("VDBE: scalar subquery arity"));
                };
                // An aggregate projection (`(SELECT max(5))`) needs the aggregate
                // machinery the const path lacks — defer.
                if is_aggregate_expr(inner) {
                    return Err(Error::Unsupported("VDBE: aggregate scalar subquery"));
                }
                // Default to NULL: a `WHERE`-filtered (zero-row) subquery is NULL.
                self.ops.push(Op::Null { dest });
                let skip = match &sel.where_clause {
                    Some(pred) => {
                        let preg = self.compile_expr(pred)?;
                        let at = self.ops.len();
                        self.ops.push(Op::IfFalse {
                            reg: preg,
                            target: 0,
                        });
                        Some(at)
                    }
                    None => None,
                };
                self.compile_expr_into(inner, dest)?;
                let end = self.ops.len();
                if let Some(at) = skip
                    && let Op::IfFalse { target, .. } = &mut self.ops[at]
                {
                    *target = end;
                }
                Ok(())
            }
            // `[NOT] EXISTS (SELECT … [WHERE p])` over a FROM-less body: the inner
            // has exactly one rowless row, so the result is whether an optional
            // `WHERE` keeps it — `EXISTS` is 1 with no predicate or a true one,
            // else 0 (inverted for `NOT EXISTS`). `EXISTS` never evaluates the
            // projection, but each term is still compiled-and-discarded to force
            // column/type resolution so an unresolved column (`EXISTS(SELECT zzz)`)
            // or a `SELECT *` (no tables) defers and the tree-walker rejects it as
            // SQLite does. A real `FROM`, an aggregate projection (which yields a
            // row even over a false-`WHERE` empty input, so `EXISTS` is always 1),
            // a wildcard, or any clause the const path lacks defers too.
            Expr::Exists {
                select: sel,
                negated,
            } => {
                // General grouped path second-pass body (see the `Expr::Subquery`
                // arm): a group-key-only correlated EXISTS over a synthetic group row.
                if !self.group_emit_keys.is_empty() {
                    if !subquery_refs_only_group_keys(self, sel, &self.group_emit_keys) {
                        return Err(Error::Unsupported(
                            "VDBE: grouped correlated subquery references a non-key column",
                        ));
                    }
                    let sub = self.subqueries.len();
                    self.subqueries.push(CorrelatedSub {
                        select: (**sel).clone(),
                    });
                    self.ops.push(Op::GroupCorrelatedExists {
                        sub,
                        negated: *negated,
                        dest,
                        group_cols: self.group_emit_keys.clone(),
                        n_cols: self.columns.len(),
                    });
                    return Ok(());
                }
                // Live single-table scan (B5c-2): re-evaluate per outer row through
                // the callback, so a correlated `EXISTS`/`NOT EXISTS` resolves its
                // outer reference (see the `Expr::Subquery` arm above).
                if self.correlated_subqueries {
                    let sub = self.subqueries.len();
                    self.subqueries.push(CorrelatedSub {
                        select: (**sel).clone(),
                    });
                    self.ops.push(Op::CorrelatedExists {
                        sub,
                        negated: *negated,
                        dest,
                    });
                    return Ok(());
                }
                if sel.from.is_some()
                    || !sel.ctes.is_empty()
                    || !sel.window_defs.is_empty()
                    || !sel.group_by.is_empty()
                    || sel.having.is_some()
                    || !sel.compound.is_empty()
                    || !sel.order_by.is_empty()
                    || sel.limit.is_some()
                    || sel.offset.is_some()
                {
                    return Err(Error::Unsupported("VDBE: exists subquery shape"));
                }
                for rc in &sel.columns {
                    let ResultColumn::Expr { expr: inner, .. } = rc else {
                        return Err(Error::Unsupported("VDBE: exists wildcard projection"));
                    };
                    if is_aggregate_expr(inner) {
                        return Err(Error::Unsupported("VDBE: aggregate exists subquery"));
                    }
                    // Compile the term to force resolution, then discard its ops.
                    let saved_ops = self.ops.len();
                    let saved_reg = self.next_reg;
                    self.compile_expr(inner)?;
                    self.ops.truncate(saved_ops);
                    self.next_reg = saved_reg;
                }
                match &sel.where_clause {
                    // No predicate: the rowless row always survives.
                    None => self.ops.push(Op::Integer {
                        value: if *negated { 0 } else { 1 },
                        dest,
                    }),
                    // Default to "filtered out"; flip when the predicate holds.
                    Some(pred) => {
                        let (default_v, match_v) = if *negated { (1, 0) } else { (0, 1) };
                        self.ops.push(Op::Integer {
                            value: default_v,
                            dest,
                        });
                        let preg = self.compile_expr(pred)?;
                        let at = self.ops.len();
                        self.ops.push(Op::IfFalse {
                            reg: preg,
                            target: 0,
                        });
                        self.ops.push(Op::Integer {
                            value: match_v,
                            dest,
                        });
                        let end = self.ops.len();
                        if let Op::IfFalse { target, .. } = &mut self.ops[at] {
                            *target = end;
                        }
                    }
                }
                Ok(())
            }
            // A correlated `expr [NOT] IN (SELECT …)` (the non-correlated bare-column
            // form is folded to an `IN (list)` by the router before compilation).
            // Re-evaluate it per outer row through the callback: wrap the whole
            // predicate in a FROM-less `SELECT <expr IN (SELECT …)>` and route it
            // through the scalar-subquery op, so the tree-walker applies the exact
            // NULL-aware `IN` semantics (true / false / NULL) against the outer
            // frame. Only enabled on the live path (`allow_correlated`), where the
            // callback is supplied; every other path bails at the catch-all below.
            Expr::InSelect {
                expr,
                select,
                negated,
            } if self.correlated_subqueries => {
                let wrapper = Select {
                    ctes: Vec::new(),
                    compound: Vec::new(),
                    distinct: false,
                    columns: alloc::vec![ResultColumn::Expr {
                        expr: Expr::InSelect {
                            expr: expr.clone(),
                            select: select.clone(),
                            negated: *negated,
                        },
                        alias: None,
                        source: None,
                    }],
                    from: None,
                    where_clause: None,
                    group_by: Vec::new(),
                    having: None,
                    window_defs: Vec::new(),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                    values_rows: 0,
                };
                let sub = self.subqueries.len();
                self.subqueries.push(CorrelatedSub { select: wrapper });
                self.ops.push(Op::CorrelatedScalar { sub, dest });
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

/// The row source backing cursor 0's single-cursor ops (`Rewind` / `Column` /
/// `Next`). A single-table scan drives these three ops; the interpreter reads the
/// current row through this trait so the same program runs over either a
/// materialized row-set ([`MaterializedCursor0`]) or a *live* b-tree cursor that
/// decodes rows lazily (B5b-2 / B8 — implemented in `exec::mod`). The multi-cursor
/// join ops (`RewindC` / `ColumnC` / `NextC`) never use this trait; they index
/// `rowsets` directly, and a live-scan program emits only single-cursor ops.
pub trait Cursor0Source {
    /// Position at the first row. Returns `false` when the source is empty (the
    /// `Rewind` op then jumps over the loop body).
    fn rewind(&mut self) -> Result<bool>;
    /// Advance to the next row. Returns `false` once past the last row (the `Next`
    /// op then falls through and ends the loop).
    fn advance(&mut self) -> Result<bool>;
    /// The value of column `col` of the current row (the hidden trailing rowid
    /// slot included, when the scan appends one). Out-of-range reads yield `NULL`.
    fn column(&self, col: usize) -> Value;
}

/// Callback the live-scan interpreter uses to evaluate a correlated subquery
/// against the current outer row. The single implementor (in `exec::mod`) reads
/// the current outer row's columns from `cur` (it knows the outer scan's column
/// layout), pushes them as an outer frame, and re-runs the subquery through the
/// tree-walker — so the result is identical to the tree-walker's own correlated
/// evaluation (and thus to SQLite).
pub trait SubqueryEval {
    /// Evaluate correlated `(SELECT …)` to its single scalar value (NULL for an
    /// empty result), binding the current row of `cur` as the outer frame.
    fn scalar(&self, sel: &Select, cur: &dyn Cursor0Source) -> Result<Value>;
    /// Evaluate correlated `EXISTS (SELECT …)` to a boolean, binding the current
    /// row of `cur` as the outer frame.
    fn exists(&self, sel: &Select, cur: &dyn Cursor0Source) -> Result<bool>;
}

/// A [`Cursor0Source`] over an already-materialized row-set — the classic path
/// used by [`run_rows`] / [`run_rows_multi`]. Cheap positional indexing; no I/O.
pub struct MaterializedCursor0<'a> {
    rows: &'a [Vec<Value>],
    pos: usize,
}

impl<'a> MaterializedCursor0<'a> {
    /// Wrap a materialized row-set as cursor 0's source.
    pub fn new(rows: &'a [Vec<Value>]) -> Self {
        MaterializedCursor0 { rows, pos: 0 }
    }
}

impl Cursor0Source for MaterializedCursor0<'_> {
    fn rewind(&mut self) -> Result<bool> {
        self.pos = 0;
        Ok(!self.rows.is_empty())
    }
    fn advance(&mut self) -> Result<bool> {
        self.pos += 1;
        Ok(self.pos < self.rows.len())
    }
    fn column(&self, col: usize) -> Value {
        self.rows
            .get(self.pos)
            .and_then(|r| r.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

/// Run a compiled program over `table_rows` (the materialized rows of the single
/// table the program scans, if any). A program counter walks the instruction
/// array so jumps and the `Rewind`/`Next` loop can branch; `Column` reads from
/// the cursor's current row.
pub fn run_rows(program: &Program, table_rows: &[Vec<Value>]) -> Result<Vec<Vec<Value>>> {
    run_rows_multi(program, &[table_rows])
}

/// Assemble the current combined multi-cursor row for a correlated subquery in a
/// join: each cursor contributes its row at `positions[i]` (NULLs when the cursor
/// is null-padded for an outer join, or its position is past the end).
fn combined_join_row(
    rowsets: &[&[Vec<Value>]],
    positions: &[usize],
    null_flags: &[bool],
) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, rs) in rowsets.iter().enumerate() {
        let width = rs.first().map(|r| r.len()).unwrap_or(0);
        let nulled = null_flags.get(i).copied().unwrap_or(false);
        match (nulled, rs.get(positions.get(i).copied().unwrap_or(0))) {
            (false, Some(r)) => out.extend(r.iter().cloned()),
            _ => out.extend(core::iter::repeat_n(Value::Null, width)),
        }
    }
    out
}

/// Like [`run_rows_multi`] but with a [`SubqueryEval`] so a join program may emit
/// [`Op::CorrelatedScalar`] / [`Op::CorrelatedExists`], evaluated against the
/// current combined multi-cursor row (B5c-2 over joins).
pub fn run_rows_multi_with_subqueries(
    program: &Program,
    rowsets: &[&[Vec<Value>]],
    eval: &dyn SubqueryEval,
) -> Result<Vec<Vec<Value>>> {
    let mut c0 = MaterializedCursor0::new(rowsets.first().copied().unwrap_or(&[]));
    run_rows_multi_impl(program, rowsets, &mut c0, Some(eval))
}

/// Run a compiled single-cursor program driving cursor 0 from a live row source
/// (a b-tree cursor, B5b-2 / B8) instead of a materialized row-set. Only the
/// single-cursor `Rewind` / `Column` / `Next` ops read from `src`; a program that
/// emits multi-cursor join ops must use [`run_rows_multi`] with materialized
/// row-sets. The projection, filter, sort, DISTINCT, LIMIT, aggregate and
/// GROUP BY machinery is shared verbatim with [`run_rows_multi`].
pub fn run_live_scan(program: &Program, src: &mut dyn Cursor0Source) -> Result<Vec<Vec<Value>>> {
    run_rows_multi_impl(program, &[], src, None)
}

/// Like [`run_live_scan`] but with a [`SubqueryEval`] callback so the program may
/// emit [`Op::CorrelatedScalar`] / [`Op::CorrelatedExists`] — a correlated
/// subquery is re-evaluated per outer row against the current cursor-0 row (B5c-2).
pub fn run_live_scan_with_subqueries(
    program: &Program,
    src: &mut dyn Cursor0Source,
    eval: &dyn SubqueryEval,
) -> Result<Vec<Vec<Value>>> {
    run_rows_multi_impl(program, &[], src, Some(eval))
}

/// Run a compiled program over several cursors' materialized row-sets (the
/// nested-loop join path, B5b): `rowsets[i]` is cursor `i`'s rows. Cursor 0 also
/// backs the single-cursor `Rewind`/`Column`/`Next` opcodes, so the single-table
/// entry point [`run_rows`] is just this with one row-set.
pub fn run_rows_multi(program: &Program, rowsets: &[&[Vec<Value>]]) -> Result<Vec<Vec<Value>>> {
    // Cursor 0's single-cursor ops read from a materialized view over `rowsets[0]`;
    // the multi-cursor join ops still index `rowsets` directly.
    let mut c0 = MaterializedCursor0::new(rowsets.first().copied().unwrap_or(&[]));
    run_rows_multi_impl(program, rowsets, &mut c0, None)
}

/// Shared interpreter body for both the materialized ([`run_rows_multi`]) and the
/// live-cursor ([`run_live_scan`]) entry points. Cursor 0's single-cursor ops
/// (`Rewind` / `Column` / `Next`) read through `c0`; the multi-cursor join ops
/// (`RewindC` / `ColumnC` / `NextC`) index `rowsets`. A single program uses one
/// family or the other, never both — a live-scan program passes an empty
/// `rowsets`.
fn run_rows_multi_impl(
    program: &Program,
    rowsets: &[&[Vec<Value>]],
    c0: &mut dyn Cursor0Source,
    subquery_eval: Option<&dyn SubqueryEval>,
) -> Result<Vec<Vec<Value>>> {
    let mut regs: Vec<Value> = alloc::vec![Value::Null; program.n_registers];
    let mut out = Vec::new();
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
                if !c0.rewind()? {
                    pc = *target;
                }
            }
            Op::Column { col, dest } => {
                regs[*dest] = c0.column(*col);
            }
            Op::Next { target } => {
                if c0.advance()? {
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
            Op::Str { value, dest } => regs[*dest] = Value::Text(value.clone().into()),
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
                    .map(|v| match v {
                        Value::Null => Expr::Literal(Literal::Null),
                        Value::Integer(i) => Expr::Literal(Literal::Integer(*i)),
                        Value::Real(r) => Expr::Literal(Literal::Real(*r)),
                        // Valid UTF-8 text reconstructs as a string literal; non-UTF-8
                        // text (which a `String` literal cannot hold) reconstructs as
                        // `CAST(<blob> AS TEXT)`, which the byte-preserving cast turns
                        // back into the same byte-backed text — so `hex(x'ff'||x'00')`
                        // and friends stay byte-exact through the VDBE `Op::Func` path.
                        Value::Text(s) => match core::str::from_utf8(s.as_bytes()) {
                            Ok(valid) => Expr::Literal(Literal::Str(valid.to_string())),
                            Err(_) => Expr::Cast {
                                expr: alloc::boxed::Box::new(Expr::Literal(Literal::Blob(
                                    s.as_bytes().to_vec(),
                                ))),
                                type_name: alloc::string::String::from("TEXT"),
                            },
                        },
                        Value::Blob(b) => Expr::Literal(Literal::Blob(b.clone())),
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
                collations,
            } => {
                let row = &regs[*start..*start + *count];
                let dup = seen.iter().any(|prev| {
                    prev.len() == row.len()
                        && prev.iter().zip(row).enumerate().all(|(i, (a, b))| {
                            let coll = collations.get(i).copied().unwrap_or(Collation::Binary);
                            distinct_eq_coll(a, b, coll)
                        })
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
                arg2,
                distinct,
                filter,
                order,
                sep,
                collation,
            } => {
                if *slot >= agg.len() {
                    agg.resize(*slot + 1, AggAcc::default());
                }
                agg[*slot].collation = *collation;
                if sep.is_some() && agg[*slot].sep.is_none() {
                    agg[*slot].sep = sep.clone();
                }
                // FILTER (WHERE …): a row whose predicate is not true contributes
                // to neither the count nor the collected values for this slot.
                let pass = filter.is_none_or(|f| crate::exec::eval::truth(&regs[f]) == Some(true));
                if !pass {
                    // fall through without folding this row into the slot
                } else if *kind == AggKind::CountStar {
                    agg[*slot].count += 1;
                } else if let (AggKind::JsonGroupObject { .. }, Some(k), Some(v)) =
                    (*kind, arg, arg2)
                {
                    // Collect the key/value pair (NULLs kept — the object keeps
                    // every pair; the key is text-coerced at finalize).
                    agg[*slot].vals.push(regs[*k].clone());
                    agg[*slot].vals2.push(regs[*v].clone());
                } else if let Some(r) = arg
                    && (matches!(*kind, AggKind::JsonGroupArray { .. })
                        || !matches!(regs[*r], Value::Null))
                {
                    let v = regs[*r].clone();
                    // A DISTINCT aggregate folds each distinct argument value once,
                    // under the argument's collation (BINARY reproduces the old
                    // behaviour exactly).
                    if !*distinct
                        || !agg[*slot]
                            .vals
                            .iter()
                            .any(|p| distinct_eq_coll(p, &v, *collation))
                    {
                        agg[*slot].vals.push(v);
                        push_order_key(&mut agg[*slot], order, &regs);
                    }
                }
            }
            Op::AggFinal { slot, kind, dest } => {
                let acc = match agg.get_mut(*slot) {
                    Some(e) => core::mem::take(e),
                    None => AggAcc::default(),
                };
                regs[*dest] = finalize_agg(*kind, acc)?;
            }
            Op::GroupStep {
                key_start,
                key_count,
                repr_count,
                companion,
                aggs,
                key_collations,
                companion_collation,
            } => {
                // The stored vector is the grouping keys followed by any bare-column
                // representatives; group identity compares only the first `key_count`
                // entries, each under its collation (`key_collations`). Without a
                // companion the representatives keep the first-seen row's values; with
                // one, a hidden trailing slot holds the running extreme and the
                // representatives track the extreme row.
                let total = *key_count + *repr_count;
                let gi = match groups.iter().position(|(k, _)| {
                    k.iter()
                        .zip(regs[*key_start..].iter())
                        .take(*key_count)
                        .enumerate()
                        .all(|(i, (a, b))| {
                            let coll = key_collations.get(i).copied().unwrap_or(Collation::Binary);
                            distinct_eq_coll(a, b, coll)
                        })
                }) {
                    Some(i) => {
                        if let Some((arg, is_max)) = companion {
                            let gv = &regs[*arg];
                            if !matches!(gv, Value::Null) {
                                let best = &groups[i].0[total];
                                let beats = matches!(best, Value::Null) || {
                                    let o = crate::value::cmp_values_coll(
                                        gv,
                                        best,
                                        *companion_collation,
                                    );
                                    if *is_max {
                                        o == core::cmp::Ordering::Greater
                                    } else {
                                        o == core::cmp::Ordering::Less
                                    }
                                };
                                if beats {
                                    groups[i].0[total] = gv.clone();
                                    for r in 0..*repr_count {
                                        groups[i].0[*key_count + r] =
                                            regs[*key_start + *key_count + r].clone();
                                    }
                                }
                            }
                        }
                        i
                    }
                    None => {
                        let mut key = regs[*key_start..*key_start + total].to_vec();
                        // Hidden companion slot: seed the extreme with this first row's
                        // governing value (NULL if absent — any real value then beats).
                        if let Some((arg, _)) = companion {
                            key.push(regs[*arg].clone());
                        }
                        groups.push((key, alloc::vec![AggAcc::default(); aggs.len()]));
                        groups.len() - 1
                    }
                };
                for (j, spec) in aggs.iter().enumerate() {
                    if spec.sep.is_some() && groups[gi].1[j].sep.is_none() {
                        groups[gi].1[j].sep = spec.sep.clone();
                    }
                    // FILTER (WHERE …) gates each aggregate independently per row.
                    let pass = spec
                        .filter
                        .is_none_or(|f| crate::exec::eval::truth(&regs[f]) == Some(true));
                    if !pass {
                        continue;
                    }
                    if spec.kind == AggKind::CountStar {
                        groups[gi].1[j].count += 1;
                    } else if let (AggKind::JsonGroupObject { .. }, Some(k), Some(v)) =
                        (spec.kind, spec.arg, spec.arg2)
                    {
                        // Collect the key/value pair (NULLs kept).
                        groups[gi].1[j].vals.push(regs[k].clone());
                        groups[gi].1[j].vals2.push(regs[v].clone());
                    } else if let Some(r) = spec.arg
                        && (matches!(spec.kind, AggKind::JsonGroupArray { .. })
                            || !matches!(regs[r], Value::Null))
                    {
                        let v = regs[r].clone();
                        // Record the argument collation (for the finalize-time
                        // min/max reduction) and, for a DISTINCT aggregate, fold each
                        // distinct value once under it.
                        groups[gi].1[j].collation = spec.collation;
                        if !spec.distinct
                            || !groups[gi].1[j]
                                .vals
                                .iter()
                                .any(|p| distinct_eq_coll(p, &v, spec.collation))
                        {
                            groups[gi].1[j].vals.push(v);
                            push_order_key(&mut groups[gi].1[j], &spec.order, &regs);
                        }
                    }
                }
            }
            Op::GroupEmit {
                outputs,
                key_count,
                key_collations,
                agg_kinds,
                group_cols,
                n_cols,
            } => {
                sort_groups_by_key(&mut groups, *key_count, key_collations);
                for (key, accs) in groups.drain(..) {
                    let finals: Vec<Value> = agg_kinds
                        .iter()
                        .zip(accs)
                        .map(|(k, acc)| finalize_agg(*k, acc))
                        .collect::<Result<_>>()?;
                    // Lazily build the synthetic per-group outer row a correlated
                    // subquery output evaluates against: the group's key values at
                    // their source-column positions, every other column NULL (only
                    // group keys are admitted as outer references, so no non-key
                    // column is ever read). Built once per group, reused by each
                    // subquery output.
                    let mut group_row: Option<Vec<Value>> = None;
                    let mut row: Vec<Value> = Vec::with_capacity(outputs.len());
                    for o in outputs {
                        let v = match o {
                            GroupOut::Key(i) => key[*i].clone(),
                            GroupOut::Agg(j) => finals[*j].clone(),
                            GroupOut::Sub(sub) | GroupOut::SubExists(sub, _) => {
                                let eval = subquery_eval.ok_or(Error::Unsupported(
                                    "VDBE: correlated subquery without evaluator",
                                ))?;
                                let gr = group_row.get_or_insert_with(|| {
                                    let mut r = alloc::vec![Value::Null; *n_cols];
                                    for (j, &sc) in group_cols.iter().enumerate() {
                                        r[sc] = key[j].clone();
                                    }
                                    r
                                });
                                let cur = MaterializedCursor0::new(core::slice::from_ref(gr));
                                let sel = &program.subqueries[*sub].select;
                                match o {
                                    GroupOut::SubExists(_, negated) => {
                                        let found = eval.exists(sel, &cur)?;
                                        Value::Integer((found ^ *negated) as i64)
                                    }
                                    _ => eval.scalar(sel, &cur)?,
                                }
                            }
                        };
                        row.push(v);
                    }
                    out.push(row);
                }
            }
            Op::GroupFinalize {
                agg_kinds,
                key_count,
                target,
                key_collations,
            } => {
                // Finalize each group's aggregates once, into `emit_groups`,
                // ordered by the GROUP BY keys; position the group cursor at the
                // first. (An explicit ORDER BY re-sorts the output downstream.)
                sort_groups_by_key(&mut groups, *key_count, key_collations);
                emit_groups.clear();
                for (key, accs) in groups.drain(..) {
                    let finals: Vec<Value> = agg_kinds
                        .iter()
                        .zip(accs)
                        .map(|(k, acc)| finalize_agg(*k, acc))
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
            // A correlated subquery: re-evaluate it against the current outer row
            // via the callback (which pushes the row as an outer frame and re-runs
            // the tree-walker). Emitted only on the live-scan path, so the callback
            // is always present when these ops appear; a missing callback is a
            // compiler bug, surfaced as an internal error rather than a panic.
            Op::CorrelatedScalar { sub, dest } => {
                let eval = subquery_eval.ok_or(Error::Unsupported(
                    "VDBE: correlated subquery without evaluator",
                ))?;
                let sel = &program.subqueries[*sub].select;
                // A SINGLE-cursor program (`rowsets` empty for a live scan, or one
                // materialized row-set — including a *materialized* join whose
                // combined columns are cursor 0) advances only cursor 0, so `c0` IS
                // the current outer row. A MULTI-cursor nested-loop join (≥ 2
                // row-sets) advances the per-cursor `positions`, so the outer row is
                // the combined row assembled from them (a null-padded cursor
                // contributes NULLs).
                regs[*dest] = if rowsets.len() < 2 {
                    eval.scalar(sel, c0)?
                } else {
                    let combined = combined_join_row(rowsets, &positions, &null_flags);
                    let cur = MaterializedCursor0::new(core::slice::from_ref(&combined));
                    eval.scalar(sel, &cur)?
                };
            }
            Op::CorrelatedExists { sub, negated, dest } => {
                let eval = subquery_eval.ok_or(Error::Unsupported(
                    "VDBE: correlated subquery without evaluator",
                ))?;
                let sel = &program.subqueries[*sub].select;
                let found = if rowsets.len() < 2 {
                    eval.exists(sel, c0)?
                } else {
                    let combined = combined_join_row(rowsets, &positions, &null_flags);
                    let cur = MaterializedCursor0::new(core::slice::from_ref(&combined));
                    eval.exists(sel, &cur)?
                };
                regs[*dest] = Value::Integer((found ^ *negated) as i64);
            }
            Op::GroupCorrelatedScalar {
                sub,
                dest,
                group_cols,
                n_cols,
            } => {
                let eval = subquery_eval.ok_or(Error::Unsupported(
                    "VDBE: correlated subquery without evaluator",
                ))?;
                let row = group_emit_synthetic_row(&emit_groups, gcursor, group_cols, *n_cols);
                let cur = MaterializedCursor0::new(core::slice::from_ref(&row));
                regs[*dest] = eval.scalar(&program.subqueries[*sub].select, &cur)?;
            }
            Op::GroupCorrelatedExists {
                sub,
                negated,
                dest,
                group_cols,
                n_cols,
            } => {
                let eval = subquery_eval.ok_or(Error::Unsupported(
                    "VDBE: correlated subquery without evaluator",
                ))?;
                let row = group_emit_synthetic_row(&emit_groups, gcursor, group_cols, *n_cols);
                let cur = MaterializedCursor0::new(core::slice::from_ref(&row));
                let found = eval.exists(&program.subqueries[*sub].select, &cur)?;
                regs[*dest] = Value::Integer((found ^ *negated) as i64);
            }
            Op::Halt => break,
        }
    }
    Ok(out)
}

/// Build the synthetic outer row a general-grouped-path correlated subquery is
/// evaluated against: the current group's key values (`emit_groups[gcursor].0`)
/// placed at their source-column positions (`group_cols`), every other column
/// NULL. Width `n_cols` (the source column count). Only group keys are admitted as
/// outer references, so no NULL column is ever read.
fn group_emit_synthetic_row(
    emit_groups: &[(Vec<Value>, Vec<Value>)],
    gcursor: usize,
    group_cols: &[usize],
    n_cols: usize,
) -> Vec<Value> {
    let mut row = alloc::vec![Value::Null; n_cols];
    if let Some((key, _)) = emit_groups.get(gcursor) {
        for (k, &sc) in group_cols.iter().enumerate() {
            if let (Some(v), true) = (key.get(k), sc < n_cols) {
                row[sc] = v.clone();
            }
        }
    }
    row
}

/// Finalize an aggregate slot, matching the tree-walker's semantics exactly:
/// `count` is 0/`n`, `sum` stays integer until it overflows then promotes to
/// real (NULL over no rows), `total` is always real, `avg` is real (NULL over no
/// rows), `min`/`max` reduce by value comparison (NULL over no rows), and
/// `group_concat` joins with `,` (NULL over no rows).
/// Order accumulated groups by their GROUP BY keys — the first `key_count` slots
/// of each group vector — under each key's collation (`collations[k]`, BINARY when
/// absent), ascending with NULLs first. SQLite emits grouped output in this order
/// (its grouping is done via a sort). An explicit `ORDER BY` re-sorts downstream,
/// so this pre-order is harmless then. With all-BINARY keys this is byte-identical
/// to the previous `cmp_values` comparison.
fn sort_groups_by_key(
    groups: &mut [(Vec<Value>, Vec<AggAcc>)],
    key_count: usize,
    collations: &[Collation],
) {
    groups.sort_by(|a, b| {
        for k in 0..key_count {
            let coll = collations.get(k).copied().unwrap_or(Collation::Binary);
            let ord = crate::value::cmp_values_coll(&a.0[k], &b.0[k], coll);
            if ord != core::cmp::Ordering::Equal {
                return ord;
            }
        }
        core::cmp::Ordering::Equal
    });
}

/// Record the current row's `ORDER BY` key values into `acc`, parallel to the
/// just-pushed argument value, capturing the per-key sort directions on the first
/// ordered push. A no-op when the aggregate carries no `ORDER BY`.
fn push_order_key(acc: &mut AggAcc, order: &[AggOrderKey], regs: &[Value]) {
    if order.is_empty() {
        return;
    }
    if acc.dirs.is_empty() {
        acc.dirs = order
            .iter()
            .map(|k| (k.descending, k.nulls_first, k.collation))
            .collect();
    }
    acc.keys
        .push(order.iter().map(|k| regs[k.reg].clone()).collect());
}

/// Compare two `ORDER BY` key rows under the captured
/// `(descending, nulls_first, collation)` specs, for sorting an ordered
/// `group_concat`. NULL placement follows SQLite: by default NULLs sort first
/// under `ASC` and last under `DESC`, overridable by explicit `NULLS FIRST`/
/// `NULLS LAST`; each key compares under its own collation (an explicit `COLLATE`,
/// the key column's collation, else `BINARY`).
fn cmp_key_rows(
    a: &[Value],
    b: &[Value],
    dirs: &[(bool, Option<bool>, Collation)],
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (k, &(descending, nulls_first, collation)) in dirs.iter().enumerate() {
        let (x, y) = (&a[k], &b[k]);
        let nulls_first = nulls_first.unwrap_or(!descending);
        let ord = match (x, y) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => {
                return if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (_, Value::Null) => {
                return if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            _ => {
                let base = crate::value::cmp_values_coll(x, y, collation);
                if descending { base.reverse() } else { base }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn finalize_agg(kind: AggKind, acc: AggAcc) -> Result<Value> {
    use crate::exec::eval;
    use core::cmp::Ordering;
    let AggAcc {
        vals,
        vals2,
        count: star,
        keys,
        dirs,
        sep,
        collation,
    } = acc;
    let sep = sep.as_deref().unwrap_or(",");
    Ok(match kind {
        AggKind::CountStar => Value::Integer(star),
        AggKind::Count => Value::Integer(vals.len() as i64),
        AggKind::Sum => eval::sum_values(&vals)?,
        AggKind::Total => Value::Real(eval::total_value(&vals)),
        AggKind::Avg => match eval::avg_value(&vals) {
            Some(r) => Value::Real(r),
            None => Value::Null,
        },
        AggKind::Min => vals
            .into_iter()
            .reduce(|a, b| {
                if crate::value::cmp_values_coll(&b, &a, collation) == Ordering::Less {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(Value::Null),
        AggKind::Max => vals
            .into_iter()
            .reduce(|a, b| {
                if crate::value::cmp_values_coll(&b, &a, collation) == Ordering::Greater {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(Value::Null),
        AggKind::GroupConcat => {
            if vals.is_empty() {
                Value::Null
            } else if keys.is_empty() {
                let parts: Vec<String> = vals.iter().map(eval::to_text).collect();
                Value::Text(parts.join(sep).into())
            } else {
                // Ordered `group_concat(x ORDER BY …)`: sort the collected values
                // by their parallel key rows (stable, so ties keep first-seen
                // order), then concatenate.
                let mut idx: Vec<usize> = (0..vals.len()).collect();
                idx.sort_by(|&i, &j| cmp_key_rows(&keys[i], &keys[j], &dirs));
                let parts: Vec<String> = idx.iter().map(|&i| eval::to_text(&vals[i])).collect();
                Value::Text(parts.join(sep).into())
            }
        }
        AggKind::JsonGroupArray { jsonb } => {
            // Every collected value (NULLs included — the fold keeps them for this
            // kind) becomes a JSON element via the same `value_to_json` the
            // tree-walker's `arg_to_json` uses for a non-subtype argument, so the
            // serialized array is byte-identical. An empty group yields `[]` (not
            // NULL). `ORDER BY` inside the call already bailed at compile time.
            let items: Vec<_> = vals.iter().map(crate::exec::json::value_to_json).collect();
            let arr = crate::exec::json::Json::Array(items);
            if jsonb {
                Value::Blob(arr.to_jsonb())
            } else {
                Value::Text(arr.serialize().into())
            }
        }
        AggKind::JsonGroupObject { jsonb } => {
            // Each collected (key, value) pair becomes an object member: the key is
            // coerced to text (matching the tree-walker's `to_text`), the value
            // serialized via the same `value_to_json` (NULL → JSON `null`). An
            // empty group yields `{}`.
            let pairs: Vec<_> = vals
                .iter()
                .zip(vals2.iter())
                .map(|(k, v)| (eval::to_text(k), None, crate::exec::json::value_to_json(v)))
                .collect();
            let obj = crate::exec::json::Json::Object(pairs);
            if jsonb {
                Value::Blob(obj.to_jsonb())
            } else {
                Value::Text(obj.serialize().into())
            }
        }
    })
}

/// `DISTINCT`/`GROUP BY` equality under a collating sequence: two `NULL`s are
/// equal (unlike `=`), a `NULL` differs from any value, and two non-NULL values
/// compare via `cmp_values_coll` (so `TEXT` under `NOCASE`/`RTRIM`/a custom
/// collation dedups case/space-insensitively).
fn distinct_eq_coll(a: &Value, b: &Value, coll: Collation) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => crate::value::cmp_values_coll(a, b, coll) == core::cmp::Ordering::Equal,
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
        let prog =
            compile_join2(&sel, &columns, &tables, &aff, &coll, &[2, 4], false, &[]).unwrap();
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
                compile_join2(&sel, &cols, &tabs, &aff, &coll, &[1, 2], false, &[]).is_err(),
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
                compile_join2(&sel, &cols, &tabs, &aff, &coll, &[1, 2], false, &[]).is_ok(),
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
            // DISTINCT aggregates over a join now fold-and-dedup on this path.
            "SELECT count(DISTINCT a.x) FROM a, b",
            "SELECT sum(DISTINCT b.p) FROM a, b WHERE a.x = b.p",
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
            // A non-grouped bare column emits a first-seen-row representative (no
            // min/max present), on the plain path.
            "SELECT a.x, b.p FROM a, b GROUP BY a.x",
            "SELECT a.x, count(*) FROM a, b GROUP BY a.x HAVING count(*) > 1",
            "SELECT a.x, count(*) FROM a, b GROUP BY a.x ORDER BY a.x",
            "SELECT a.x, count(*) FROM a, b GROUP BY a.x LIMIT 1",
            "SELECT a.x, count(*) AS n FROM a, b GROUP BY a.x ORDER BY n DESC LIMIT 2 OFFSET 1",
            // A bare column with exactly one min/max tracks that aggregate's
            // companion row.
            "SELECT a.x, b.p, max(b.p) FROM a, b GROUP BY a.x",
            // DISTINCT dedups the grouped output rows (general path, BINARY).
            "SELECT DISTINCT a.x, count(*) FROM a, b GROUP BY a.x",
        ] {
            let Statement::Select(sel) = parse_one(sql).unwrap() else {
                panic!()
            };
            assert!(
                compile_group_join(&sel, &cols, &tabs, &aff, &coll, &[1, 2], false).is_ok(),
                "{sql} should compile as a GROUP BY join"
            );
        }
        // A bare column alongside more than one min/max (ambiguous companion) /
        // no GROUP BY still bail.
        for sql in [
            "SELECT a.x, b.p, max(b.p), min(b.p) FROM a, b GROUP BY a.x", // >1 min/max
            "SELECT count(*) FROM a, b",                                  // no GROUP BY
        ] {
            let Statement::Select(sel) = parse_one(sql).unwrap() else {
                panic!()
            };
            assert!(
                compile_group_join(&sel, &cols, &tabs, &aff, &coll, &[1, 2], false).is_err(),
                "{sql} should bail from the GROUP BY join path"
            );
        }
    }
}
