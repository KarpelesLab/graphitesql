//! The abstract syntax tree produced by the parser.
//!
//! These types model the subset of SQLite's grammar graphitesql currently
//! parses. They are deliberately close to SQLite's own parse structures so the
//! code generator (Phase 5/7) can map them onto VDBE programs directly. The
//! grammar source of truth is `parse.y`.

use crate::sql::token::Param;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// A complete parsed statement.
// Variants differ in size (Select is the largest); boxing every variant would
// hurt ergonomics more than the size gap costs, and statements are short-lived.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// A `SELECT` query.
    Select(Select),
    /// An `INSERT` statement.
    Insert(Insert),
    /// An `UPDATE` statement.
    Update(Update),
    /// A `DELETE` statement.
    Delete(Delete),
    /// A `CREATE TABLE` statement.
    CreateTable(CreateTable),
    /// A `CREATE INDEX` statement.
    CreateIndex(CreateIndex),
    /// A `CREATE VIEW` statement.
    CreateView(CreateView),
    /// A `CREATE VIRTUAL TABLE … USING module(args)` statement.
    CreateVirtualTable(CreateVirtualTable),
    /// A `CREATE TRIGGER` statement.
    CreateTrigger(CreateTrigger),
    /// A `DROP TABLE`/`DROP INDEX`/… statement.
    Drop(Drop),
    /// An `ALTER TABLE` statement.
    Alter(Alter),
    /// `BEGIN [TRANSACTION]`.
    Begin,
    /// `COMMIT`/`END`.
    Commit,
    /// `ROLLBACK`.
    Rollback,
    /// `SAVEPOINT name`: open a named savepoint (nested transaction).
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] name`: keep changes since the savepoint, drop it.
    Release(String),
    /// `ROLLBACK [TRANSACTION] TO [SAVEPOINT] name`: undo changes since the
    /// savepoint, keeping it open.
    RollbackTo(String),
    /// A `PRAGMA` statement.
    Pragma(Pragma),
    /// A `VACUUM` statement (accepted; a no-op compaction in this build).
    Vacuum,
    /// `REINDEX [name]` — accepted as a no-op: graphitesql rebuilds an index
    /// whenever the underlying rows change, so indexes are always current.
    Reindex,
    /// `ANALYZE [name]`: gather statistics into `sqlite_stat1`. `None` analyzes
    /// the whole database; `Some(name)` a single table or index.
    Analyze(Option<String>),
    /// `ATTACH [DATABASE] <expr> AS <name>`: open another database under `name`.
    Attach {
        /// The file path expression (`':memory:'`/`''` → a new in-memory db).
        file: Expr,
        /// The schema name to attach as.
        name: String,
    },
    /// `DETACH [DATABASE] <name>`: close an attached database.
    Detach(String),
    /// `EXPLAIN [QUERY PLAN] <stmt>`.
    Explain {
        /// `EXPLAIN QUERY PLAN` (true) vs plain `EXPLAIN` (false, VDBE bytecode,
        /// which this engine does not produce).
        query_plan: bool,
        /// The statement being explained.
        stmt: Box<Statement>,
    },
}

/// A common table expression (`WITH name AS (select)`).
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    /// The CTE's name.
    pub name: String,
    /// Optional explicit column names.
    pub columns: Vec<String>,
    /// The CTE's query.
    pub select: Box<Select>,
}

/// A window-function `OVER (…)` specification.
///
/// Explicit frame clauses (`ROWS`/`RANGE BETWEEN …`) are not yet modeled; the
/// executor applies SQLite's default frame (RANGE UNBOUNDED PRECEDING to CURRENT
/// ROW when `ORDER BY` is present, the whole partition otherwise).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowSpec {
    /// `PARTITION BY` expressions.
    pub partition_by: Vec<Expr>,
    /// `ORDER BY` terms within each partition.
    pub order_by: Vec<OrderTerm>,
    /// An explicit frame clause, if given (else the default frame applies).
    pub frame: Option<WindowFrame>,
    /// For `OVER window_name`, the referenced named window (resolved against the
    /// query's `WINDOW name AS (…)` definitions before computation).
    pub base_name: Option<String>,
}

/// A window frame: a mode (`ROWS`/`RANGE`/`GROUPS`), start/end bounds, and an
/// optional `EXCLUDE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowFrame {
    /// `ROWS`, `RANGE`, or `GROUPS`.
    pub mode: FrameMode,
    /// The frame's starting bound.
    pub start: FrameBound,
    /// The frame's ending bound (`CURRENT ROW` when no `BETWEEN` is given).
    pub end: FrameBound,
    /// The `EXCLUDE` clause (default `NO OTHERS`).
    pub exclude: FrameExclude,
}

/// A window frame's `EXCLUDE` clause: which rows of the computed frame to drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameExclude {
    /// `EXCLUDE NO OTHERS` (the default): keep the whole frame.
    #[default]
    NoOthers,
    /// `EXCLUDE CURRENT ROW`: drop the current row.
    CurrentRow,
    /// `EXCLUDE GROUP`: drop the current row's entire peer group.
    Group,
    /// `EXCLUDE TIES`: drop the current row's peers but keep the current row.
    Ties,
}

/// The unit a window frame is measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMode {
    /// `ROWS` — physical row offsets.
    Rows,
    /// `RANGE` — logical (peer) ranges.
    Range,
    /// `GROUPS` — peer-group offsets.
    Groups,
}

/// One bound of a window frame.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameBound {
    /// `UNBOUNDED PRECEDING`.
    UnboundedPreceding,
    /// `<n> PRECEDING`.
    Preceding(i64),
    /// `CURRENT ROW`.
    CurrentRow,
    /// `<n> FOLLOWING`.
    Following(i64),
    /// `UNBOUNDED FOLLOWING`.
    UnboundedFollowing,
}

/// A compound-query operator joining two `SELECT`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOp {
    /// `UNION` (distinct).
    Union,
    /// `UNION ALL`.
    UnionAll,
    /// `INTERSECT`.
    Intersect,
    /// `EXCEPT`.
    Except,
}

/// A `SELECT` query.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// `WITH` common table expressions, in declaration order.
    pub ctes: Vec<Cte>,
    /// Compound continuations (`UNION`/`INTERSECT`/`EXCEPT` …), left-associative.
    /// The outer `order_by`/`limit`/`offset` apply to the whole compound.
    pub compound: Vec<(CompoundOp, Select)>,
    /// `SELECT DISTINCT`?
    pub distinct: bool,
    /// The projected result columns.
    pub columns: Vec<ResultColumn>,
    /// The `FROM` clause, if any.
    pub from: Option<FromClause>,
    /// The `WHERE` predicate, if any.
    pub where_clause: Option<Expr>,
    /// `GROUP BY` expressions.
    pub group_by: Vec<Expr>,
    /// `HAVING` predicate.
    pub having: Option<Expr>,
    /// `WINDOW name AS (spec)` named-window definitions.
    pub window_defs: Vec<(String, WindowSpec)>,
    /// `ORDER BY` terms.
    pub order_by: Vec<OrderTerm>,
    /// `LIMIT` expression.
    pub limit: Option<Expr>,
    /// `OFFSET` expression.
    pub offset: Option<Expr>,
}

/// A single result column in a `SELECT`.
// `Expr` is by far the most common variant and carries a full `Expr`; boxing it
// to shrink the unit `Wildcard`/`TableWildcard` variants would pessimise the
// common case for no real gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ResultColumn {
    /// `*`
    Wildcard,
    /// `table.*`
    TableWildcard(String),
    /// An expression with an optional alias.
    Expr {
        /// The projected expression.
        expr: Expr,
        /// `AS alias`, if present.
        alias: Option<String>,
        /// The verbatim source text of `expr` (whitespace preserved), captured by
        /// the parser. SQLite names an unaliased non-column result column after
        /// this span — `SELECT a  +  b` yields a column literally named `a  +  b`.
        /// `None` for synthetically constructed columns (no source span).
        source: Option<String>,
    },
}

/// A `FROM` clause: a left table joined with zero or more others.
#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    /// The first table source.
    pub first: TableRef,
    /// Subsequent joins.
    pub joins: Vec<Join>,
}

/// A reference to a table in a `FROM` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    /// The table name (empty when this source is a subquery).
    pub name: String,
    /// A `schema.` qualifier (`FROM aux.t`), if any — the database to resolve
    /// `name` in (`main`/`temp`/an attached database).
    pub schema: Option<String>,
    /// An optional alias (`AS x` or bare `x`).
    pub alias: Option<String>,
    /// A derived-table subquery (`FROM (SELECT …) [AS] alias`), if any.
    pub subquery: Option<Box<Select>>,
    /// `INDEXED BY name` / `NOT INDEXED` query-planner hint, if given.
    pub index_hint: Option<IndexHint>,
    /// Table-valued-function arguments (`FROM generate_series(1, 10)`), if this
    /// source is a TVF call rather than a table.
    pub tvf_args: Option<Vec<Expr>>,
}

/// A `FROM` table's index hint.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexHint {
    /// `NOT INDEXED` — forbid using any index for this table (force a scan).
    NotIndexed,
    /// `INDEXED BY name` — require that the named index be used.
    IndexedBy(String),
}

/// The kind of join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `,` or `CROSS JOIN` or `INNER JOIN`.
    Inner,
    /// `LEFT [OUTER] JOIN`.
    Left,
    /// `RIGHT [OUTER] JOIN`.
    Right,
    /// `FULL [OUTER] JOIN`.
    Full,
}

/// A join onto a table.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// The kind of join.
    pub kind: JoinKind,
    /// The joined table.
    pub table: TableRef,
    /// An `ON` predicate, if present.
    pub on: Option<Expr>,
    /// `NATURAL` join: join on equality of all columns common to both sides
    /// (mutually exclusive with `on`/`using`).
    pub natural: bool,
    /// `USING (col, …)`: join on equality of the named columns, which are then
    /// coalesced into a single output column each (mutually exclusive with `on`).
    pub using: Vec<String>,
}

/// One `ORDER BY` term.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderTerm {
    /// The ordering expression.
    pub expr: Expr,
    /// `DESC`?
    pub descending: bool,
    /// Explicit `NULLS FIRST` (`Some(true)`) / `NULLS LAST` (`Some(false)`).
    /// `None` uses SQLite's default: NULLs sort first under `ASC`, last under
    /// `DESC`.
    pub nulls_first: Option<bool>,
}

/// An `INSERT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// Target table.
    pub table: String,
    /// A `schema.` qualifier (`INSERT INTO aux.t`), if any.
    pub schema: Option<String>,
    /// Explicit column list, if given.
    pub columns: Vec<String>,
    /// The data source.
    pub source: InsertSource,
    /// Conflict resolution (`INSERT OR …` / `REPLACE`).
    pub on_conflict: OnConflict,
    /// `ON CONFLICT … DO …` upsert clauses, in order. SQLite allows several
    /// chained clauses with distinct conflict targets; the one whose target the
    /// conflict matches wins (a final target-less clause is the catch-all).
    pub upsert: Vec<Upsert>,
    /// `RETURNING` projection, empty when absent.
    pub returning: Vec<ResultColumn>,
}

/// An `ON CONFLICT [(target)] DO …` upsert clause.
#[derive(Debug, Clone, PartialEq)]
pub struct Upsert {
    /// Conflict-target column names (empty for a bare `ON CONFLICT`).
    pub target: Vec<String>,
    /// Optional `WHERE` on the conflict target (partial-index match).
    pub target_where: Option<Expr>,
    /// What to do on conflict.
    pub action: UpsertAction,
}

/// The action of an `ON CONFLICT … DO …` clause.
#[derive(Debug, Clone, PartialEq)]
// `DO UPDATE` carries assignments + an optional predicate; boxing them would hurt
// ergonomics more than the size gap costs (see the module note), and upsert
// statements are short-lived.
#[allow(clippy::large_enum_variant)]
pub enum UpsertAction {
    /// `DO NOTHING`: silently skip the conflicting row.
    Nothing,
    /// `DO UPDATE SET … [WHERE …]`: update the conflicting row. The assignments
    /// and predicate may reference the existing row by column name and the
    /// would-be-inserted row via the `excluded` pseudo-table.
    Update {
        /// `SET col = expr` assignments.
        assignments: Vec<(String, Expr)>,
        /// Optional `WHERE` filtering which conflicts get updated.
        where_clause: Option<Expr>,
    },
}

/// Conflict resolution policy for `INSERT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    /// Default: fail the statement and roll back the changes it made so far
    /// (but keep changes from earlier statements in the transaction).
    Abort,
    /// `OR FAIL`: fail the statement but keep the rows it already changed before
    /// the failure (no statement-level rollback).
    Fail,
    /// `OR ROLLBACK`: fail and roll back the entire surrounding transaction.
    Rollback,
    /// Skip the conflicting row.
    Ignore,
    /// Replace the conflicting row(s).
    Replace,
}

/// Where an `INSERT` gets its rows.
#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    /// `VALUES (…), (…)`.
    Values(Vec<Vec<Expr>>),
    /// `INSERT … SELECT …`.
    Select(Box<Select>),
    /// `DEFAULT VALUES`.
    DefaultValues,
}

/// An `UPDATE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// Target table.
    pub table: String,
    /// A `schema.` qualifier (`UPDATE aux.t`), if any.
    pub schema: Option<String>,
    /// `UPDATE OR <action>` conflict resolution (default `Abort`).
    pub on_conflict: OnConflict,
    /// `SET col = expr` assignments.
    pub assignments: Vec<(String, Expr)>,
    /// `UPDATE … SET … FROM <sources>` — extra tables joined to the target so
    /// the `SET`/`WHERE` expressions can read their columns (SQLite extension).
    pub from: Option<FromClause>,
    /// `WHERE` predicate.
    pub where_clause: Option<Expr>,
    /// `ORDER BY` for a `LIMIT`ed update (empty when absent).
    pub order_by: Vec<OrderTerm>,
    /// `LIMIT` row cap (with the SQLite update/delete-limit extension).
    pub limit: Option<Expr>,
    /// `OFFSET` skip count.
    pub offset: Option<Expr>,
    /// `RETURNING` projection, empty when absent.
    pub returning: Vec<ResultColumn>,
}

/// A `DELETE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    /// Target table.
    pub table: String,
    /// A `schema.` qualifier (`DELETE FROM aux.t`), if any.
    pub schema: Option<String>,
    /// `WHERE` predicate.
    pub where_clause: Option<Expr>,
    /// `ORDER BY` for a `LIMIT`ed delete (empty when absent).
    pub order_by: Vec<OrderTerm>,
    /// `LIMIT` row cap (with the SQLite update/delete-limit extension).
    pub limit: Option<Expr>,
    /// `OFFSET` skip count.
    pub offset: Option<Expr>,
    /// `RETURNING` projection, empty when absent.
    pub returning: Vec<ResultColumn>,
}

/// A `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    /// `IF NOT EXISTS`?
    pub if_not_exists: bool,
    /// Table name.
    pub name: String,
    /// A `schema.` qualifier (`CREATE TABLE aux.t`), if any.
    pub schema: Option<String>,
    /// Column definitions.
    pub columns: Vec<ColumnDef>,
    /// Table-level constraints (raw, for now).
    pub constraints: Vec<TableConstraint>,
    /// `WITHOUT ROWID`?
    pub without_rowid: bool,
    /// `STRICT`? A strict table restricts column types to the six rigid types
    /// (`INT`/`INTEGER`/`REAL`/`TEXT`/`BLOB`/`ANY`) and type-checks every stored
    /// value against its column's declared type.
    pub strict: bool,
    /// `CREATE TABLE … AS SELECT …` — the table's columns and rows come from the
    /// query. When present, `columns`/`constraints` are empty until materialized.
    pub as_select: Option<Box<Select>>,
}

/// A column definition in `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// Column name.
    pub name: String,
    /// Declared type name (e.g. `INTEGER`), if any.
    pub type_name: Option<String>,
    /// Column constraints.
    pub constraints: Vec<ColumnConstraint>,
}

/// A referential action for a foreign key (`ON DELETE`/`ON UPDATE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FkAction {
    /// `NO ACTION` (the default) — reject if dependent rows remain at statement end.
    #[default]
    NoAction,
    /// `RESTRICT` — reject immediately.
    Restrict,
    /// `CASCADE` — propagate the delete/update to child rows.
    Cascade,
    /// `SET NULL` — null the child's referencing columns.
    SetNull,
    /// `SET DEFAULT` — reset the child's referencing columns to their defaults.
    SetDefault,
}

/// A foreign-key definition (column- or table-level).
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKey {
    /// Child columns that make up the key.
    pub columns: Vec<String>,
    /// Referenced (parent) table.
    pub ref_table: String,
    /// Referenced (parent) columns; empty means the parent's primary key.
    pub ref_columns: Vec<String>,
    /// `ON DELETE` action.
    pub on_delete: FkAction,
    /// `ON UPDATE` action.
    pub on_update: FkAction,
    /// `DEFERRABLE INITIALLY DEFERRED` — the constraint is checked at `COMMIT`
    /// rather than at statement time. `false` for immediate / `NOT DEFERRABLE` /
    /// `DEFERRABLE INITIALLY IMMEDIATE`.
    pub initially_deferred: bool,
}

/// A column-level constraint.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    /// `PRIMARY KEY [ASC|DESC] [AUTOINCREMENT]`.
    PrimaryKey {
        /// Descending primary key?
        descending: bool,
        /// `AUTOINCREMENT` present (only valid on an `INTEGER PRIMARY KEY`).
        autoincrement: bool,
    },
    /// `NOT NULL`.
    NotNull,
    /// `UNIQUE`.
    Unique,
    /// `DEFAULT <expr>`.
    Default(Expr),
    /// `COLLATE <name>`.
    Collate(String),
    /// `CHECK (<expr>)`. The second field is the constraint's *label* for error
    /// messages — its name if written `CONSTRAINT <name> CHECK …`, else the
    /// verbatim source text of `<expr>` — matching SQLite's
    /// `CHECK constraint failed: <label>`.
    Check(Expr, Option<String>),
    /// `REFERENCES parent(cols) …` — a column-level foreign key.
    References(ForeignKey),
    /// `[GENERATED ALWAYS] AS (expr) [STORED|VIRTUAL]` — a generated column.
    Generated {
        /// The generation expression.
        expr: Expr,
        /// `STORED` (true) materializes the value on disk; `VIRTUAL` (false, the
        /// default) computes it on read and is not stored.
        stored: bool,
    },
}

/// A table-level constraint.
#[derive(Debug, Clone, PartialEq)]
pub enum TableConstraint {
    /// `PRIMARY KEY (cols…)`.
    PrimaryKey(Vec<String>),
    /// `UNIQUE (cols…)`.
    Unique(Vec<String>),
    /// `CHECK (<expr>)`. The second field is the constraint's *label* (see
    /// [`ColumnConstraint::Check`]).
    Check(Expr, Option<String>),
    /// `FOREIGN KEY (cols) REFERENCES parent(cols) …`.
    ForeignKey(ForeignKey),
}

/// A `CREATE INDEX` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    /// `UNIQUE`?
    pub unique: bool,
    /// `IF NOT EXISTS`?
    pub if_not_exists: bool,
    /// Optional `schema.` (database) qualifier on the index name.
    pub schema: Option<String>,
    /// Index name.
    pub name: String,
    /// Indexed table.
    pub table: String,
    /// Indexed columns, with direction.
    pub columns: Vec<OrderTerm>,
    /// Partial-index `WHERE`.
    pub where_clause: Option<Expr>,
}

/// A `CREATE VIEW` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateView {
    /// `IF NOT EXISTS`?
    pub if_not_exists: bool,
    /// Optional `schema.` (database) qualifier.
    pub schema: Option<String>,
    /// View name.
    pub name: String,
    /// Optional explicit column names.
    pub columns: Vec<String>,
    /// The view's `SELECT`.
    pub select: Box<Select>,
}

/// A `CREATE VIRTUAL TABLE [IF NOT EXISTS] name USING module[(arg, …)]`
/// statement.
///
/// The module name is an identifier; the arguments are captured verbatim as
/// strings (SQLite passes them to the module untouched, never evaluating them as
/// expressions).
#[derive(Debug, Clone, PartialEq)]
pub struct CreateVirtualTable {
    /// `IF NOT EXISTS`?
    pub if_not_exists: bool,
    /// Optional `schema.` (database) qualifier.
    pub schema: Option<String>,
    /// The virtual table's name.
    pub name: String,
    /// The module name following `USING`.
    pub module: String,
    /// The comma-separated module arguments, captured verbatim (empty when no
    /// parenthesized argument list was given).
    pub args: Vec<String>,
}

/// When a trigger fires relative to its event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    /// `BEFORE` the row change.
    Before,
    /// `AFTER` the row change.
    After,
    /// `INSTEAD OF` (views) — parsed but not executed.
    InsteadOf,
}

/// The data-change event a trigger fires on.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerEvent {
    /// `INSERT`.
    Insert,
    /// `UPDATE [OF col, …]`.
    Update(Vec<String>),
    /// `DELETE`.
    Delete,
}

/// A `CREATE TRIGGER` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTrigger {
    /// `IF NOT EXISTS`?
    pub if_not_exists: bool,
    /// Optional `schema.` (database) qualifier.
    pub schema: Option<String>,
    /// Trigger name.
    pub name: String,
    /// `BEFORE`/`AFTER`/`INSTEAD OF`.
    pub timing: TriggerTiming,
    /// The firing event.
    pub event: TriggerEvent,
    /// The table the trigger is attached to.
    pub table: String,
    /// `WHEN <expr>` guard, if any.
    pub when: Option<Expr>,
    /// The trigger body: statements between `BEGIN` and `END`.
    pub body: Vec<Statement>,
}

/// What kind of object a `DROP` targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropKind {
    /// `DROP TABLE`.
    Table,
    /// `DROP INDEX`.
    Index,
    /// `DROP VIEW`.
    View,
    /// `DROP TRIGGER`.
    Trigger,
}

/// A `DROP` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Drop {
    /// What is being dropped.
    pub kind: DropKind,
    /// `IF EXISTS`?
    pub if_exists: bool,
    /// Object name.
    pub name: String,
    /// A `schema.` qualifier (`DROP TABLE aux.t`), if any.
    pub schema: Option<String>,
}

/// An `ALTER TABLE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Alter {
    /// Optional `schema.` (database) qualifier.
    pub schema: Option<String>,
    /// The table being altered.
    pub table: String,
    /// What to do to it.
    pub action: AlterAction,
}

/// The action of an `ALTER TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    /// `RENAME TO new_name`.
    RenameTable(String),
    /// `RENAME [COLUMN] old TO new`.
    RenameColumn {
        /// Existing column name.
        old: String,
        /// New column name.
        new: String,
    },
    /// `ADD [COLUMN] <column-def>`.
    AddColumn(ColumnDef),
    /// `DROP [COLUMN] name`.
    DropColumn(String),
}

/// A `PRAGMA` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Pragma {
    /// Pragma name.
    pub name: String,
    /// `PRAGMA name = value` or `PRAGMA name(value)`.
    pub value: Option<Expr>,
}

/// A scalar expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A literal value.
    Literal(Literal),
    /// A bound parameter.
    Parameter(Param),
    /// A column reference, optionally table-qualified.
    Column {
        /// `table.` qualifier, if any.
        table: Option<String>,
        /// Column name.
        column: String,
    },
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Expr>,
    },
    /// A binary operation.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// A function call.
    Function {
        /// Function name.
        name: String,
        /// `COUNT(DISTINCT …)` etc.
        distinct: bool,
        /// Arguments (`COUNT(*)` has an empty list with `star = true`).
        args: Vec<Expr>,
        /// Whether the argument was `*`.
        star: bool,
        /// `FILTER (WHERE …)` — restricts which rows an aggregate/window function
        /// consumes.
        filter: Option<Box<Expr>>,
        /// `ORDER BY …` inside an aggregate call (`group_concat(x ORDER BY y)`).
        order_by: Vec<OrderTerm>,
        /// `OVER (…)` window specification, making this a window-function call.
        over: Option<WindowSpec>,
    },
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// The tested expression.
        expr: Box<Expr>,
        /// `IS NOT NULL`?
        negated: bool,
    },
    /// `expr [NOT] IN (list)`.
    InList {
        /// The tested expression.
        expr: Box<Expr>,
        /// Candidate list.
        list: Vec<Expr>,
        /// `NOT IN`?
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// The tested expression.
        expr: Box<Expr>,
        /// Lower bound.
        low: Box<Expr>,
        /// Upper bound.
        high: Box<Expr>,
        /// `NOT BETWEEN`?
        negated: bool,
    },
    /// A `CASE` expression.
    Case {
        /// Optional base operand (`CASE x WHEN …`).
        operand: Option<Box<Expr>>,
        /// `(when, then)` pairs.
        when_then: Vec<(Expr, Expr)>,
        /// `ELSE` result.
        else_result: Option<Box<Expr>>,
    },
    /// `CAST(expr AS type)`.
    Cast {
        /// The cast operand.
        expr: Box<Expr>,
        /// The target type name.
        type_name: String,
    },
    /// A parenthesized expression (kept for fidelity; semantically transparent).
    Paren(Box<Expr>),
    /// A row value `(a, b, …)` with two or more elements — used in comparisons
    /// (`(a,b) < (c,d)`) and `IN` lists (`(a,b) IN ((1,2),(3,4))`).
    RowValue(Vec<Expr>),
    /// `expr COLLATE name` — assigns a collating sequence for comparison.
    Collate {
        /// The operand.
        expr: Box<Expr>,
        /// The collation name (`BINARY`/`NOCASE`/`RTRIM`).
        collation: String,
    },
    /// A scalar subquery `(SELECT …)` — yields its first row's first column.
    Subquery(Box<Select>),
    /// `[NOT] EXISTS (SELECT …)`.
    Exists {
        /// The subquery to test for any rows.
        select: Box<Select>,
        /// `NOT EXISTS`?
        negated: bool,
    },
    /// `expr [NOT] IN (SELECT …)`.
    InSelect {
        /// The tested expression.
        expr: Box<Expr>,
        /// The subquery whose first column is the candidate set.
        select: Box<Select>,
        /// `NOT IN`?
        negated: bool,
    },
}

/// A literal value in an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// `NULL`.
    Null,
    /// An integer.
    Integer(i64),
    /// A real.
    Real(f64),
    /// A text string.
    Str(String),
    /// A blob.
    Blob(Vec<u8>),
    /// `TRUE` / `FALSE` (stored as 1/0 in SQLite, kept distinct for clarity).
    Boolean(bool),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-x`
    Negate,
    /// `+x`
    Identity,
    /// `NOT x`
    Not,
    /// `~x`
    BitNot,
}

/// Binary operators, grouped roughly by precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `OR`
    Or,
    /// `AND`
    And,
    /// `=`
    Eq,
    /// `<>` / `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `IS`
    Is,
    /// `IS NOT`
    IsNot,
    /// `LIKE`
    Like,
    /// `GLOB`
    Glob,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `||`
    Concat,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `<<`
    LShift,
    /// `>>`
    RShift,
    /// `->` — JSON extract, returning the result as JSON.
    JsonExtract,
    /// `->>` — JSON extract, returning the result as a SQL text/value.
    JsonExtractText,
}
