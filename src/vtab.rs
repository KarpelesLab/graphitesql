//! Virtual-table modules — a safe Rust analog of SQLite's `sqlite3_module`.
//!
//! # What this is (roadmap D1a)
//!
//! This module is the **foundation** for virtual tables: a safe, idiomatic Rust
//! trait that an extension implements to expose an arbitrary data source as a
//! table, plus a connection-scoped registry that maps a module *name* (the name
//! that would later appear in `CREATE VIRTUAL TABLE … USING <name>`) to a module
//! implementation.
//!
//! It is deliberately self-contained. There is **no** SQL, parser, or executor
//! wiring here — that integration is the follow-up, **D1b** (see below). What
//! lands in D1a is purely the Rust-level contract: a trait others can implement
//! and a registry the engine can later consult, exercised end-to-end by unit
//! tests that register a module, connect it, scan its cursor, and read back
//! columns and rowids — all without going through any SQL.
//!
//! # Mapping to SQLite's `sqlite3_module`
//!
//! SQLite's virtual-table interface is a struct of C function pointers operating
//! on raw `sqlite3_vtab` / `sqlite3_vtab_cursor` pointers. Rather than mimic that
//! pointer machinery, this module models the same lifecycle with safe Rust types:
//!
//! | SQLite C concept                 | graphitesql analog                        |
//! |----------------------------------|-------------------------------------------|
//! | `sqlite3_module`                 | [`VTabModule`] (a trait object)           |
//! | `xConnect` / `xCreate`           | [`VTabModule::connect`] → [`VTabSchema`]  |
//! | `xBestIndex`                     | [`VTabModule::best_index`] → [`IndexPlan`] |
//! | `xOpen`                          | [`VTabModule::open`] → [`VTabCursor`]     |
//! | `xFilter`                        | [`VTabModule::filter`] (seeds the cursor) |
//! | `xNext` / `xEof`                 | [`VTabCursor::next`] returning `Option`   |
//! | `xColumn`                        | [`VTabRow::column`]                       |
//! | `xRowid`                         | [`VTabRow::rowid`]                         |
//! | registration of a module name   | [`VTabRegistry`]                          |
//!
//! The cursor is shaped like a Rust iterator: [`VTabCursor::next`] yields
//! `Result<Option<VTabRow>>` — `Ok(None)` is end-of-table (SQLite's `xEof`),
//! `Err(_)` is a fault. Each [`VTabRow`] answers [`column`](VTabRow::column) and
//! [`rowid`](VTabRow::rowid), folding `xColumn`/`xRowid` into the row value so
//! implementors never juggle a separate "current row" pointer.
//!
//! # Wired up in D1b; what is still stubbed
//!
//! `CREATE VIRTUAL TABLE … USING <name>(<args>)` is parsed and executed by
//! `crate::exec`: it persists a `sqlite_schema` row (`type='table'`,
//! `rootpage=0`) and, on a `FROM`-clause read, looks the module up in the
//! connection's [`VTabRegistry`], calls [`connect`](VTabModule::connect) for the
//! column schema, [`open`](VTabModule::open)s a cursor with the `USING`
//! arguments, and drains it like any other source. The built-in
//! [`SeriesModule`] is registered under `"series"` on every connection.
//!
//! # Constraint pushdown (roadmap D1b)
//!
//! [`VTabModule::best_index`] is the analog of SQLite's `xBestIndex`: the planner
//! offers the `WHERE`-clause comparisons that reference the table as a slice of
//! [`IndexConstraint`]s (column + operator, with no right-hand value yet — exactly
//! as SQLite presents `sqlite3_index_info.aConstraint`), and the module returns an
//! [`IndexPlan`] choosing how it will scan. The plan records, per offered
//! constraint, the 1-based [`argv_index`](IndexPlan::argv_index) position at which
//! the module wants that constraint's bound value handed back, plus whether each
//! consumed constraint is [`omit`](IndexPlan::omit)table (fully handled, so the
//! executor may skip re-checking it).
//!
//! The executor then evaluates the bound values, orders them by `argv_index`, and
//! passes them to [`VTabModule::filter`] (SQLite's `xFilter`), which seeds the
//! cursor — e.g. [`SeriesModule`] narrows its `start`/`stop` so
//! `WHERE value BETWEEN 3 AND 5` *generates* only `3..=5` instead of its whole
//! range.
//!
//! **Correctness invariant.** A constraint is dropped from the re-applied SQL
//! `WHERE` *only* when the module marks it [`omit`](IndexPlan::omit). Anything the
//! module did not fully consume is still filtered by the executor, so the plan is
//! always a superset of the answer and pushdown can never produce a wrong row.
//!
//! Still stubbed:
//!
//! * **Writes.** `xUpdate` (INSERT/UPDATE/DELETE through a virtual table) is not
//!   modeled; virtual tables are read/scan only, and the executor rejects writes
//!   to them.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::value::Value;

/// The declared shape of a virtual table's columns, returned by
/// [`VTabModule::connect`].
///
/// This is the analog of the `CREATE TABLE` statement an `xConnect`/`xCreate`
/// implementation passes to `sqlite3_declare_vtab`: it tells the engine the
/// column names (and, in this safe model, nothing more for D1a — affinities and
/// constraints can be layered on in D1b without breaking the trait).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VTabSchema {
    /// The column names, in declaration order. `column(i)` on a row refers to
    /// the column at this index.
    pub columns: Vec<String>,
}

impl VTabSchema {
    /// Build a schema from a list of column names.
    pub fn new<I, S>(columns: I) -> VTabSchema
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        VTabSchema {
            columns: columns.into_iter().map(Into::into).collect(),
        }
    }

    /// The number of declared columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether the schema declares no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// A comparison operator a virtual table may be asked to satisfy, the analog of
/// SQLite's `SQLITE_INDEX_CONSTRAINT_*` op codes.
///
/// Carried by [`IndexConstraint`]. The planner produces these from a query's
/// `WHERE` clause and offers them to [`VTabModule::best_index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintOp {
    /// `=`
    Eq,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `<`
    Lt,
    /// `>=`
    Ge,
    /// `MATCH`
    Match,
    /// `LIKE`
    Like,
    /// `GLOB`
    Glob,
}

/// A single `WHERE`-clause constraint offered to [`VTabModule::best_index`],
/// the analog of one `sqlite3_index_info.aConstraint` entry.
///
/// As in SQLite, only the *shape* of the comparison is offered here — the column,
/// the operator, and whether it is usable — never the right-hand value. A module
/// that wants a constraint's value asks for it by assigning an
/// [`argv_index`](IndexPlan::argv_index); the executor then evaluates the bound
/// expression and passes it to [`VTabModule::filter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexConstraint {
    /// Index of the constrained column within the [`VTabSchema`].
    pub column: usize,
    /// The comparison operator.
    pub op: ConstraintOp,
    /// Whether this constraint is usable (SQLite may offer constraints whose
    /// right-hand side is not available to the current plan, e.g. it references
    /// another table). An unusable constraint must not be consumed by a plan.
    pub usable: bool,
}

/// The plan a module chose in [`VTabModule::best_index`], the analog of the
/// outputs SQLite reads back from `sqlite3_index_info` (`idxNum`, `idxStr`,
/// `estimatedCost`, and which constraints are consumed).
///
/// The plan is opaque to the engine: the module invents [`idx_num`](Self::idx_num)
/// / [`idx_str`](Self::idx_str) for itself and reads them back in
/// [`VTabModule::filter`] to drive the scan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IndexPlan {
    /// Module-private plan selector (SQLite's `idxNum`).
    pub idx_num: i32,
    /// Module-private plan string (SQLite's `idxStr`), if any.
    pub idx_str: Option<String>,
    /// The estimated cost of this plan; lower is cheaper. The default plan
    /// reports a large cost so any real plan is preferred.
    pub estimated_cost: f64,
    /// For each constraint offered to `best_index`, the 1-based argument
    /// position the module wants its value passed in (SQLite's
    /// `aConstraintUsage[i].argvIndex`), or `0` to ignore it. Length must match
    /// the offered constraint slice. The executor evaluates each requested
    /// constraint's right-hand value and passes them, ordered by this index, to
    /// [`VTabModule::filter`].
    pub argv_index: Vec<u32>,
    /// For each constraint offered to `best_index`, `true` if the module fully
    /// handles it and the executor may **omit** re-checking it in the SQL `WHERE`
    /// (SQLite's `aConstraintUsage[i].omit`). Length must match the offered
    /// constraint slice (an empty vec means "omit nothing"). A constraint may
    /// only be omitted if it was also assigned an `argv_index`; everything not
    /// marked here is still re-applied by the executor, preserving the superset
    /// invariant.
    pub omit: Vec<bool>,
    /// Whether the module's scan already returns rows in the query's `ORDER BY`
    /// order, so the executor may skip its own sort (SQLite's
    /// `orderByConsumed`). Defaults to `false`; the executor sorts as usual.
    pub order_by_consumed: bool,
}

/// A single produced row of a virtual table.
///
/// Folds SQLite's `xColumn` and `xRowid` into one value: [`column`](Self::column)
/// returns the cell at a column index (out-of-range yields [`Value::Null`], as
/// SQLite does for an unbound column), and [`rowid`](Self::rowid) returns the
/// 64-bit row identifier.
pub trait VTabRow {
    /// The value of the `i`-th declared column (0-based). Out-of-range indices
    /// return [`Value::Null`].
    fn column(&self, i: usize) -> Value;

    /// The 64-bit rowid of this row (SQLite's `xRowid`).
    fn rowid(&self) -> i64;
}

/// A scan over a virtual table, shaped like a fallible Rust iterator.
///
/// [`next`](Self::next) advances the cursor and yields the current row:
/// `Ok(Some(row))` for a row, `Ok(None)` at end-of-table (SQLite's `xEof`), and
/// `Err(_)` on a fault. Calling `next` again after `Ok(None)` should keep
/// returning `Ok(None)`.
pub trait VTabCursor {
    /// The row type this cursor yields.
    type Row: VTabRow;

    /// Advance to and return the next row, or `Ok(None)` at end-of-table.
    fn next(&mut self) -> Result<Option<Self::Row>>;
}

/// A write delivered to [`VTabModule::update`] (SQLite's `xUpdate`). The engine
/// resolves the affected row and evaluates its column values before the call.
#[derive(Debug, Clone)]
pub enum VTabChange<'a> {
    /// Delete the row with this rowid.
    Delete {
        /// The rowid of the row to remove.
        rowid: i64,
    },
    /// Insert a row. `rowid` is the explicit rowid the statement supplied (an
    /// `INTEGER PRIMARY KEY` / `rowid` value), or `None` for the module to assign
    /// one. `values` are the column values in the table's declared column order.
    Insert {
        /// The explicit rowid, if any.
        rowid: Option<i64>,
        /// One value per declared column, in order.
        values: &'a [Value],
    },
    /// Replace the row `rowid` with `values`, moving it to `new_rowid` when the
    /// statement changed the rowid.
    Update {
        /// The existing rowid of the row being changed.
        rowid: i64,
        /// The rowid after the change (equal to `rowid` unless it was set).
        new_rowid: i64,
        /// One value per declared column, in order.
        values: &'a [Value],
    },
}

/// A virtual-table module: the safe analog of `sqlite3_module`.
///
/// A connection registers an implementation under a name (see [`VTabRegistry`]).
/// The lifecycle is: [`connect`](Self::connect) declares the table's columns from
/// the `USING` arguments; [`best_index`](Self::best_index) chooses a scan plan
/// from offered constraints; [`open`](Self::open) starts a scan, returning a
/// [`VTabCursor`]; [`filter`](Self::filter) seeds that cursor with the chosen
/// plan's bound argument values.
pub trait VTabModule {
    /// The cursor type returned by [`open`](Self::open).
    type Cursor: VTabCursor;

    /// Connect to (declare) the virtual table from its `USING` arguments.
    ///
    /// `args` are the comma-separated module arguments from
    /// `CREATE VIRTUAL TABLE … USING <name>(<args>)`. Returns the column
    /// declaration the engine will use for the table.
    fn connect(&self, args: &[&str]) -> Result<VTabSchema>;

    /// Choose a scan plan from the offered constraints (SQLite's `xBestIndex`).
    ///
    /// The default returns an empty, high-cost plan that consumes no constraints,
    /// i.e. "I will do a full scan." A module overrides this to push usable
    /// constraints into its scan: assign each constraint it wants an
    /// [`argv_index`](IndexPlan::argv_index), and optionally mark it
    /// [`omit`](IndexPlan::omit) when it fully enforces the comparison. The
    /// `argv_index`/`omit` vectors, when non-empty, must have the same length as
    /// `constraints`.
    fn best_index(&self, _constraints: &[IndexConstraint]) -> Result<IndexPlan> {
        Ok(IndexPlan {
            estimated_cost: f64::from(u32::MAX),
            ..IndexPlan::default()
        })
    }

    /// Open a scan over the table, returning a cursor (SQLite's `xOpen`).
    ///
    /// `args` are the same `USING <name>(<args>)` arguments that were passed to
    /// [`connect`](Self::connect); they configure the scan (e.g. a `series`'
    /// `start`/`stop`/`step`). The cursor is not yet positioned — the executor
    /// calls [`filter`](Self::filter) before reading rows.
    fn open(&self, args: &[&str], plan: &IndexPlan) -> Result<Self::Cursor>;

    /// Seed `cursor` for the chosen `plan` (SQLite's `xFilter`), then return it
    /// ready to iterate.
    ///
    /// `argv` holds the bound right-hand values of the constraints the plan
    /// requested, ordered by their `argv_index` (so `argv[0]` is the value for
    /// `argvIndex == 1`, and so on). A module reads `plan.idx_num`/`plan.idx_str`
    /// to learn which plan was chosen and consumes `argv` accordingly. The
    /// default is a no-op (full scan): the cursor is returned unchanged.
    fn filter(
        &self,
        cursor: Self::Cursor,
        _plan: &IndexPlan,
        _argv: &[Value],
    ) -> Result<Self::Cursor> {
        Ok(cursor)
    }

    /// Apply a write to the table (SQLite's `xUpdate`), returning the rowid of the
    /// inserted/updated row (ignored for a delete).
    ///
    /// `args` are the `USING <name>(<args>)` arguments (as for [`connect`](Self::connect)).
    /// The default makes the table **read-only** — it returns an error — so an
    /// existing read-only module needs no change. A writable module overrides this
    /// to service [`VTabChange::Insert`]/`Delete`/`Update`.
    fn update(&self, _args: &[&str], _change: VTabChange) -> Result<i64> {
        Err(Error::Error(alloc::string::String::from(
            "table is read-only",
        )))
    }
}

/// An object-safe erasure of [`VTabModule`] so heterogeneous modules can live in
/// one [`VTabRegistry`].
///
/// [`VTabModule`] has associated types, which makes `dyn VTabModule` impossible.
/// This trait hides them behind boxed trait objects ([`DynCursor`] /
/// [`DynRow`]), letting the registry store `Box<dyn DynVTabModule>`. A blanket
/// impl wires every [`VTabModule`] up automatically, so implementors only write
/// the typed trait.
/// The methods carry a `dyn_` prefix so they never collide with the identically
/// purposed typed-trait methods when a concrete type (which implements both via
/// the blanket impls below) has both traits in scope.
pub trait DynVTabModule {
    /// See [`VTabModule::connect`].
    fn dyn_connect(&self, args: &[&str]) -> Result<VTabSchema>;
    /// See [`VTabModule::best_index`].
    fn dyn_best_index(&self, constraints: &[IndexConstraint]) -> Result<IndexPlan>;
    /// [`VTabModule::open`] followed by [`VTabModule::filter`]; returns a boxed,
    /// type-erased cursor already seeded with `plan`/`argv` and ready to iterate.
    fn dyn_open(
        &self,
        args: &[&str],
        plan: &IndexPlan,
        argv: &[Value],
    ) -> Result<Box<dyn DynCursor>>;
    /// See [`VTabModule::update`].
    fn dyn_update(&self, args: &[&str], change: VTabChange) -> Result<i64>;
}

/// A type-erased [`VTabRow`] yielded by a [`DynCursor`].
pub trait DynRow {
    /// See [`VTabRow::column`].
    fn dyn_column(&self, i: usize) -> Value;
    /// See [`VTabRow::rowid`].
    fn dyn_rowid(&self) -> i64;
}

impl<R: VTabRow> DynRow for R {
    fn dyn_column(&self, i: usize) -> Value {
        VTabRow::column(self, i)
    }
    fn dyn_rowid(&self) -> i64 {
        VTabRow::rowid(self)
    }
}

/// A type-erased [`VTabCursor`] returned by [`DynVTabModule::dyn_open`].
pub trait DynCursor {
    /// See [`VTabCursor::next`]; yields a boxed, type-erased row.
    fn dyn_next(&mut self) -> Result<Option<Box<dyn DynRow>>>;
}

impl<C: VTabCursor> DynCursor for C
where
    C::Row: 'static,
{
    fn dyn_next(&mut self) -> Result<Option<Box<dyn DynRow>>> {
        Ok(VTabCursor::next(self)?.map(|r| Box::new(r) as Box<dyn DynRow>))
    }
}

impl<M> DynVTabModule for M
where
    M: VTabModule,
    M::Cursor: 'static,
{
    fn dyn_connect(&self, args: &[&str]) -> Result<VTabSchema> {
        VTabModule::connect(self, args)
    }
    fn dyn_best_index(&self, constraints: &[IndexConstraint]) -> Result<IndexPlan> {
        VTabModule::best_index(self, constraints)
    }
    fn dyn_open(
        &self,
        args: &[&str],
        plan: &IndexPlan,
        argv: &[Value],
    ) -> Result<Box<dyn DynCursor>> {
        let cursor = VTabModule::open(self, args, plan)?;
        let cursor = VTabModule::filter(self, cursor, plan, argv)?;
        Ok(Box::new(cursor) as Box<dyn DynCursor>)
    }
    fn dyn_update(&self, args: &[&str], change: VTabChange) -> Result<i64> {
        VTabModule::update(self, args, change)
    }
}

/// A connection-scoped registry mapping module names to implementations.
///
/// The name is the one that would appear after `USING` in
/// `CREATE VIRTUAL TABLE … USING <name>`. Lookups are case-insensitive (names are
/// folded to lowercase on insert and lookup), matching SQLite, whose module names
/// are ASCII-case-insensitive.
#[derive(Default)]
pub struct VTabRegistry {
    modules: BTreeMap<String, Box<dyn DynVTabModule>>,
}

impl VTabRegistry {
    /// Create an empty registry.
    pub fn new() -> VTabRegistry {
        VTabRegistry {
            modules: BTreeMap::new(),
        }
    }

    /// Register `module` under `name`.
    ///
    /// Returns [`Error::Constraint`] if a module is already registered under that
    /// name (case-insensitively), mirroring SQLite's refusal to redefine a module.
    pub fn register(&mut self, name: &str, module: Box<dyn DynVTabModule>) -> Result<()> {
        let key = name.to_ascii_lowercase();
        if self.modules.contains_key(&key) {
            return Err(Error::Constraint(alloc::format!(
                "virtual table module \"{name}\" is already registered"
            )));
        }
        self.modules.insert(key, module);
        Ok(())
    }

    /// Look up the module registered under `name` (case-insensitively).
    pub fn get(&self, name: &str) -> Option<&dyn DynVTabModule> {
        self.modules
            .get(&name.to_ascii_lowercase())
            .map(AsRef::as_ref)
    }

    /// Remove and return the module registered under `name`, if any.
    pub fn unregister(&mut self, name: &str) -> Option<Box<dyn DynVTabModule>> {
        self.modules.remove(&name.to_ascii_lowercase())
    }

    /// The number of registered modules.
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    /// Whether no modules are registered.
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

impl VTabRegistry {
    /// Build a registry pre-populated with the engine's built-in virtual-table
    /// modules. Currently just [`SeriesModule`] under the name `"series"`, so
    /// `CREATE VIRTUAL TABLE … USING series(…)` works out of the box. A
    /// user-facing registration API is roadmap D4.
    pub fn with_builtins() -> VTabRegistry {
        let mut reg = VTabRegistry::new();
        reg.register("series", Box::new(SeriesModule))
            .expect("fresh registry has no name collisions");
        reg
    }
}

// ---------------------------------------------------------------------------
// Example module: `series` — yields the integers `start..=stop` stepping by
// `step`. This mirrors SQLite's built-in `generate_series` eponymous virtual
// table and exists to exercise the trait + registry at the Rust level.
// ---------------------------------------------------------------------------

/// An example [`VTabModule`] yielding an arithmetic sequence of integers.
///
/// `USING series(start, stop, step)`: a single `value` column whose rows are
/// `start, start+step, …` up to (and including) `stop`. `stop` defaults to
/// `start` and `step` to `1`. This is the virtual-table analog of the engine's
/// existing `generate_series` table-valued function, and demonstrates a complete
/// module implementation — including constraint pushdown (see
/// [`best_index`](VTabModule::best_index) / [`filter`](VTabModule::filter) below).
#[derive(Debug, Default, Clone, Copy)]
pub struct SeriesModule;

/// Plan selectors a [`SeriesModule::best_index`] may choose. Encoded into
/// [`IndexPlan::idx_num`] and read back in [`SeriesModule::filter`].
mod series_plan {
    /// Full scan — no usable constraint pushed.
    pub const SCAN: i32 = 0;
    /// A lower bound (`value >= argv` / `value > argv`) was pushed.
    pub const LOWER: i32 = 1 << 0;
    /// An upper bound (`value <= argv` / `value < argv`) was pushed.
    pub const UPPER: i32 = 1 << 1;
}

/// The cursor for [`SeriesModule`], walking `start..=stop` by `step`.
#[derive(Debug)]
pub struct SeriesCursor {
    next: i64,
    stop: i64,
    step: i64,
    /// Sequential rowid, 1-based, assigned as rows are produced.
    next_rowid: i64,
    done: bool,
    /// How many `value`s this cursor has generated so far. With pushdown the
    /// cursor's `start`/`stop` are narrowed, so this stays small (it is the
    /// observable proof that `filter` restricted the scan rather than the
    /// executor filtering a full enumeration afterward).
    generated: usize,
}

impl SeriesCursor {
    /// The number of `value`s this cursor has generated so far.
    pub fn generated(&self) -> usize {
        self.generated
    }
}

/// One row of a [`SeriesModule`] scan: a single integer `value` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesRow {
    value: i64,
    rowid: i64,
}

impl VTabRow for SeriesRow {
    fn column(&self, i: usize) -> Value {
        match i {
            0 => Value::Integer(self.value),
            _ => Value::Null,
        }
    }

    fn rowid(&self) -> i64 {
        self.rowid
    }
}

impl VTabCursor for SeriesCursor {
    type Row = SeriesRow;

    fn next(&mut self) -> Result<Option<SeriesRow>> {
        if self.done {
            return Ok(None);
        }
        let in_range = if self.step > 0 {
            self.next <= self.stop
        } else {
            self.next >= self.stop
        };
        if !in_range {
            self.done = true;
            return Ok(None);
        }
        let row = SeriesRow {
            value: self.next,
            rowid: self.next_rowid,
        };
        self.generated += 1;
        self.next_rowid += 1;
        match self.next.checked_add(self.step) {
            Some(n) => self.next = n,
            None => self.done = true, // i64 overflow ends the series
        }
        Ok(Some(row))
    }
}

/// Advance `start` along the arithmetic grid `start + k*step` (`k >= 0`,
/// `step != 0`) to the first grid point at or past `target` *in the direction of
/// travel*: for `step > 0` the first value `>= target`; for `step < 0` the first
/// value `<= target`. If `start` is already past `target`, it is returned
/// unchanged. On any arithmetic overflow the original `start` is returned (a safe
/// superset — the re-applied `WHERE` still filters).
fn advance_to(start: i64, step: i64, target: i64) -> i64 {
    debug_assert!(step != 0);
    // Steps still needed: k = ceil((target - start) / step), but only when that
    // moves us forward (k > 0). Compute with i128 to dodge i64 overflow.
    let delta = i128::from(target) - i128::from(start);
    let step128 = i128::from(step);
    // Already at/past the target in the travel direction → no advance.
    if (step > 0 && delta <= 0) || (step < 0 && delta >= 0) {
        return start;
    }
    // Ceil-division toward +∞ in the number of whole steps.
    let k = {
        let q = delta / step128;
        let r = delta % step128;
        // `delta` and `step128` have the same sign here (checked above), so the
        // quotient is positive; round up when there is a remainder.
        if r != 0 {
            q + 1
        } else {
            q
        }
    };
    match i128::from(start).checked_add(k * step128) {
        Some(v) if v >= i128::from(i64::MIN) && v <= i128::from(i64::MAX) => v as i64,
        _ => start,
    }
}

impl SeriesModule {
    /// Parse an integer argument, erroring with a clear message on failure.
    fn parse_arg(s: &str) -> Result<i64> {
        s.trim()
            .parse::<i64>()
            .map_err(|_| Error::Error(alloc::format!("series(): invalid integer argument {s:?}")))
    }
}

impl VTabModule for SeriesModule {
    type Cursor = SeriesCursor;

    fn connect(&self, args: &[&str]) -> Result<VTabSchema> {
        if args.is_empty() {
            return Err(Error::Error(
                "series() requires at least a start argument".into(),
            ));
        }
        if args.len() > 3 {
            return Err(Error::Error("series() takes at most 3 arguments".into()));
        }
        // Validate the arguments eagerly so a bad `CREATE VIRTUAL TABLE` fails at
        // connect time rather than at scan time.
        for a in args {
            SeriesModule::parse_arg(a)?;
        }
        Ok(VTabSchema::new(["value"]))
    }

    /// Offer the `value` column's comparisons to the planner.
    ///
    /// Recognizes `=` / `>=` / `>` / `<=` / `<` on column 0 (`value`). Each such
    /// usable constraint is assigned a fresh `argv_index` so its bound value is
    /// handed to [`filter`](Self::filter); equality requests its value as both a
    /// lower and an upper bound (collapsing the scan to a single candidate row).
    ///
    /// Constraints are **not** marked [`omit`](IndexPlan::omit): `best_index` does
    /// not see the series' `step` (it arrives with the `USING` args at
    /// [`open`](Self::open)/[`filter`](Self::filter) time), and the bound seeds the
    /// generation range but the inclusive/strict and step-alignment details are
    /// left to the executor's re-applied `WHERE`. The plan is therefore always a
    /// superset — correct by construction.
    fn best_index(&self, constraints: &[IndexConstraint]) -> Result<IndexPlan> {
        let mut argv_index = alloc::vec![0u32; constraints.len()];
        let mut idx_num = series_plan::SCAN;
        let mut next_arg = 1u32;
        for (i, c) in constraints.iter().enumerate() {
            if c.column != 0 || !c.usable {
                continue;
            }
            let bound = match c.op {
                ConstraintOp::Eq => series_plan::LOWER | series_plan::UPPER,
                ConstraintOp::Ge | ConstraintOp::Gt => series_plan::LOWER,
                ConstraintOp::Le | ConstraintOp::Lt => series_plan::UPPER,
                _ => continue,
            };
            argv_index[i] = next_arg;
            next_arg += 1;
            idx_num |= bound;
        }
        if idx_num == series_plan::SCAN {
            // Nothing usable — fall back to the default full-scan plan.
            return Ok(IndexPlan {
                estimated_cost: f64::from(u32::MAX),
                ..IndexPlan::default()
            });
        }
        // Record, per requested arg, which bound side(s) it feeds, so `filter`
        // can apply `argv` in order without re-deriving it. Lower=`<<0`,
        // upper=`<<1`, equality=both, packed into `idx_str` bytes.
        let mut sides = String::new();
        for c in constraints {
            if c.column != 0 || !c.usable {
                continue;
            }
            match c.op {
                ConstraintOp::Eq => sides.push('='),
                ConstraintOp::Ge | ConstraintOp::Gt => sides.push('>'),
                ConstraintOp::Le | ConstraintOp::Lt => sides.push('<'),
                _ => {}
            }
        }
        Ok(IndexPlan {
            idx_num,
            idx_str: Some(sides),
            // A bounded scan is much cheaper than the full range.
            estimated_cost: 100.0,
            argv_index,
            omit: Vec::new(),
            order_by_consumed: false,
        })
    }

    /// Narrow the cursor's `start`/`stop` from the pushed bounds in `argv`.
    ///
    /// `argv` is ordered by `argv_index` and `plan.idx_str` records, per arg, the
    /// bound side it feeds (`>` lower, `<` upper, `=` both). For an ascending
    /// series a lower bound raises `start` and an upper bound lowers `stop`; for a
    /// descending series the roles swap. The narrowed span is always a **superset**
    /// of the answer (strictness/alignment are re-checked by the executor's
    /// `WHERE`), so pushdown can never drop a real row.
    fn filter(
        &self,
        mut cursor: SeriesCursor,
        plan: &IndexPlan,
        argv: &[Value],
    ) -> Result<SeriesCursor> {
        if plan.idx_num == series_plan::SCAN {
            return Ok(cursor);
        }
        let sides = plan.idx_str.as_deref().unwrap_or("");
        let mut lower: Option<i64> = None;
        let mut upper: Option<i64> = None;
        for (side, v) in sides.chars().zip(argv.iter()) {
            // Only integer bounds narrow the range. Any other bound (a real, a
            // string, NULL) is left for the re-applied `WHERE` — skipping it keeps
            // the generated span a superset, so the result stays correct.
            let n = match v {
                Value::Integer(i) => *i,
                _ => continue,
            };
            match side {
                '>' => lower = Some(lower.map_or(n, |cur| cur.max(n))),
                '<' => upper = Some(upper.map_or(n, |cur| cur.min(n))),
                '=' => {
                    lower = Some(lower.map_or(n, |cur| cur.max(n)));
                    upper = Some(upper.map_or(n, |cur| cur.min(n)));
                }
                _ => {}
            }
        }
        // Advancing `start` must keep it on the original arithmetic grid (only
        // values `start + k*step` exist), or pushdown would invent off-grid rows
        // that the re-applied `value = …` WHERE would wrongly keep. Clamping the
        // `stop` end only ever drops rows, so it needs no alignment.
        let ascending = cursor.step > 0;
        if ascending {
            // start = first grid point >= lower; stop = min(stop, upper).
            if let Some(lo) = lower {
                cursor.next = advance_to(cursor.next, cursor.step, lo);
            }
            if let Some(hi) = upper {
                cursor.stop = cursor.stop.min(hi);
            }
        } else {
            // descending: start = first grid point <= upper; stop = max(stop, lower).
            if let Some(hi) = upper {
                cursor.next = advance_to(cursor.next, cursor.step, hi);
            }
            if let Some(lo) = lower {
                cursor.stop = cursor.stop.max(lo);
            }
        }
        Ok(cursor)
    }

    fn open(&self, args: &[&str], plan: &IndexPlan) -> Result<SeriesCursor> {
        // The plan is consulted in `filter`, not here; `open` just builds the
        // unfiltered cursor from the `USING` args.
        let _ = plan;
        // `series(start[, stop[, step]])`: stop defaults to start, step to 1.
        let start = args
            .first()
            .map(|a| SeriesModule::parse_arg(a))
            .transpose()?;
        let Some(start) = start else {
            return Err(Error::Error(
                "series() requires at least a start argument".into(),
            ));
        };
        let stop = match args.get(1) {
            Some(a) => SeriesModule::parse_arg(a)?,
            None => start,
        };
        let step = match args.get(2) {
            Some(a) => SeriesModule::parse_arg(a)?,
            None => 1,
        };
        SeriesModule::scan(start, stop, step)
    }
}

impl SeriesModule {
    /// Open a scan with explicit `start`/`stop`/`step`.
    ///
    /// [`VTabModule::open`] cannot see the `connect` arguments in the D1a trait
    /// shape (binding scan parameters to a connected table is D1b), so this helper
    /// builds a configured cursor directly. The unit tests use it, and a D1b
    /// integration would call something like it after `connect`.
    pub fn scan(start: i64, stop: i64, step: i64) -> Result<SeriesCursor> {
        if step == 0 {
            return Err(Error::Error("series(): step must be non-zero".into()));
        }
        Ok(SeriesCursor {
            next: start,
            stop,
            step,
            next_rowid: 1,
            done: false,
            generated: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Drain a typed cursor into a vec of `(rowid, value)` pairs.
    fn drain(mut cur: SeriesCursor) -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        while let Some(row) = cur.next().unwrap() {
            // `value` is column 0; out-of-range columns are NULL.
            assert_eq!(row.column(0), Value::Integer(row.value));
            assert_eq!(row.column(1), Value::Null);
            out.push((row.rowid(), row.value));
        }
        out
    }

    #[test]
    fn connect_declares_value_column() {
        let m = SeriesModule;
        let schema = m.connect(&["1", "5"]).unwrap();
        assert_eq!(schema.columns, vec![String::from("value")]);
        assert_eq!(schema.len(), 1);
        assert!(!schema.is_empty());
    }

    #[test]
    fn connect_validates_arguments() {
        let m = SeriesModule;
        assert!(m.connect(&[]).is_err()); // no args
        assert!(m.connect(&["1", "2", "3", "4"]).is_err()); // too many
        assert!(m.connect(&["notanint"]).is_err()); // not an integer
        assert!(m.connect(&["10"]).is_ok());
    }

    #[test]
    fn cursor_iterates_ascending() {
        let cur = SeriesModule::scan(1, 5, 1).unwrap();
        assert_eq!(drain(cur), vec![(1, 1), (2, 2), (3, 3), (4, 4), (5, 5)]);
    }

    #[test]
    fn cursor_iterates_with_step() {
        let cur = SeriesModule::scan(0, 10, 3).unwrap();
        assert_eq!(drain(cur), vec![(1, 0), (2, 3), (3, 6), (4, 9)]);
    }

    #[test]
    fn cursor_iterates_descending() {
        let cur = SeriesModule::scan(3, 1, -1).unwrap();
        assert_eq!(drain(cur), vec![(1, 3), (2, 2), (3, 1)]);
    }

    #[test]
    fn empty_range_yields_no_rows() {
        let cur = SeriesModule::scan(5, 1, 1).unwrap();
        assert_eq!(drain(cur), vec![]);
    }

    #[test]
    fn step_zero_is_rejected() {
        assert!(SeriesModule::scan(1, 5, 0).is_err());
    }

    #[test]
    fn next_keeps_returning_none_after_end() {
        let mut cur = SeriesModule::scan(1, 1, 1).unwrap();
        assert!(cur.next().unwrap().is_some());
        assert!(cur.next().unwrap().is_none());
        assert!(cur.next().unwrap().is_none());
    }

    #[test]
    fn default_best_index_is_a_full_scan_plan() {
        let m = SeriesModule;
        let plan = m.best_index(&[]).unwrap();
        assert_eq!(plan.idx_num, 0);
        assert_eq!(plan.idx_str, None);
        assert!(plan.argv_index.is_empty());
        // The stub reports a high cost so any future real plan is preferred.
        assert!(plan.estimated_cost > 1.0);
    }

    #[test]
    fn advance_to_aligns_to_grid() {
        // Ascending grid 0,2,4,…: first point >= 3 is 4 (off-grid target rounds up).
        assert_eq!(advance_to(0, 2, 3), 4);
        // First point >= 4 is 4 (on-grid target stays).
        assert_eq!(advance_to(0, 2, 4), 4);
        // Target before start → unchanged.
        assert_eq!(advance_to(5, 1, 2), 5);
        // Descending grid 10,8,6,…: first point <= 7 is 6.
        assert_eq!(advance_to(10, -2, 7), 6);
        assert_eq!(advance_to(10, -2, 8), 8);
        // step 1 lands exactly.
        assert_eq!(advance_to(1, 1, 3), 3);
    }

    /// `best_index` must assign argv positions for usable `value` constraints and
    /// leave others at 0; an empty/unusable offer falls back to the scan plan.
    #[test]
    fn best_index_pushes_value_constraints() {
        let m = SeriesModule;
        let cons = [
            IndexConstraint {
                column: 0,
                op: ConstraintOp::Ge,
                usable: true,
            },
            IndexConstraint {
                column: 0,
                op: ConstraintOp::Le,
                usable: true,
            },
        ];
        let plan = m.best_index(&cons).unwrap();
        assert_eq!(plan.idx_num, series_plan::LOWER | series_plan::UPPER);
        assert_eq!(plan.argv_index, vec![1, 2]);
        assert_eq!(plan.idx_str.as_deref(), Some("><"));
        assert!(plan.estimated_cost < f64::from(u32::MAX));

        // An unusable constraint is not consumed.
        let unusable = [IndexConstraint {
            column: 0,
            op: ConstraintOp::Ge,
            usable: false,
        }];
        let plan = m.best_index(&unusable).unwrap();
        assert_eq!(plan.idx_num, series_plan::SCAN);
        assert_eq!(plan.argv_index, Vec::<u32>::new());
    }

    /// `filter` narrows the cursor so only in-range values are generated, and the
    /// generated-value counter proves the narrowing happened.
    #[test]
    fn filter_narrows_generation() {
        let m = SeriesModule;
        // series(0, 100): WHERE value BETWEEN 3 AND 5  → plan LOWER|UPPER.
        let cons = [
            IndexConstraint {
                column: 0,
                op: ConstraintOp::Ge,
                usable: true,
            },
            IndexConstraint {
                column: 0,
                op: ConstraintOp::Le,
                usable: true,
            },
        ];
        let plan = m.best_index(&cons).unwrap();
        let cur = SeriesModule::scan(0, 100, 1).unwrap();
        let mut cur = m
            .filter(cur, &plan, &[Value::Integer(3), Value::Integer(5)])
            .unwrap();
        let mut out = Vec::new();
        while let Some(row) = cur.next().unwrap() {
            out.push((row.rowid(), row.value));
        }
        assert_eq!(out, vec![(1, 3), (2, 4), (3, 5)]);
        assert_eq!(cur.generated(), 3, "only 3..=5 generated, not 0..=100");
    }

    /// Equality on a stepped series must not invent off-grid rows: `filter`
    /// collapses to a single grid-aligned candidate, never an off-grid value.
    #[test]
    fn filter_equality_stays_on_grid() {
        let m = SeriesModule;
        let cons = [IndexConstraint {
            column: 0,
            op: ConstraintOp::Eq,
            usable: true,
        }];
        let plan = m.best_index(&cons).unwrap();
        // series(0,10,2) = 0,2,4,6,8,10; WHERE value = 3 must yield nothing — the
        // collapsed start aligns up to 4, and 4 > stop(=3), so no off-grid 3.
        let cur = SeriesModule::scan(0, 10, 2).unwrap();
        let cur = m.filter(cur, &plan, &[Value::Integer(3)]).unwrap();
        assert_eq!(drain(cur), vec![]);
        // WHERE value = 6 collapses to exactly the single grid row 6.
        let cur = SeriesModule::scan(0, 10, 2).unwrap();
        let cur = m.filter(cur, &plan, &[Value::Integer(6)]).unwrap();
        assert_eq!(drain(cur), vec![(1, 6)]);
    }

    #[test]
    fn registry_register_get_roundtrip() {
        let mut reg = VTabRegistry::new();
        assert!(reg.is_empty());
        reg.register("series", Box::new(SeriesModule)).unwrap();
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
        // Lookup is case-insensitive.
        assert!(reg.get("series").is_some());
        assert!(reg.get("SERIES").is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn registry_rejects_duplicate_names() {
        let mut reg = VTabRegistry::new();
        reg.register("series", Box::new(SeriesModule)).unwrap();
        let err = reg.register("SERIES", Box::new(SeriesModule)).unwrap_err();
        assert!(matches!(err, Error::Constraint(_)));
    }

    #[test]
    fn registry_unregister() {
        let mut reg = VTabRegistry::new();
        reg.register("series", Box::new(SeriesModule)).unwrap();
        assert!(reg.unregister("Series").is_some());
        assert!(reg.is_empty());
        assert!(reg.unregister("series").is_none());
    }

    /// End-to-end through the type-erased registry path: register, connect, open
    /// a boxed cursor, and read columns/rowid via the `dyn` traits — proving the
    /// erasure layer is usable, not just the typed traits.
    #[test]
    fn dyn_module_end_to_end() {
        let mut reg = VTabRegistry::new();
        reg.register("series", Box::new(SeriesModule)).unwrap();
        let module = reg.get("series").expect("registered");

        let schema = module.dyn_connect(&["2", "8", "2"]).unwrap();
        assert_eq!(schema.columns, vec![String::from("value")]);

        let plan = module.dyn_best_index(&[]).unwrap();
        // The dyn `open` path goes through `SeriesModule::open`, configuring the
        // scan from the same `USING` arguments passed to `connect`.
        let mut cur = module.dyn_open(&["2", "8", "2"], &plan, &[]).unwrap();
        let mut seen = Vec::new();
        while let Some(row) = cur.dyn_next().unwrap() {
            seen.push(row.dyn_column(0));
        }
        assert_eq!(
            seen,
            vec![
                Value::Integer(2),
                Value::Integer(4),
                Value::Integer(6),
                Value::Integer(8),
            ]
        );
    }

    /// Drive a boxed `dyn` cursor produced from a configured scan, exercising the
    /// `DynCursor` / `DynRow` erasure with real rows.
    #[test]
    fn dyn_cursor_yields_rows() {
        let cur = SeriesModule::scan(10, 12, 1).unwrap();
        let mut dyn_cur: Box<dyn DynCursor> = Box::new(cur);
        let mut seen = Vec::new();
        while let Some(row) = dyn_cur.dyn_next().unwrap() {
            seen.push((row.dyn_rowid(), row.dyn_column(0)));
        }
        assert_eq!(
            seen,
            vec![
                (1, Value::Integer(10)),
                (2, Value::Integer(11)),
                (3, Value::Integer(12)),
            ]
        );
    }
}
