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
//! # Writes (roadmap W1/W2)
//!
//! [`VTabModule::update`] is the analog of SQLite's `xUpdate`: the executor routes
//! `INSERT`/`UPDATE`/`DELETE` on a virtual table to it as a [`VTabChange`]. The
//! default leaves a module read-only (it errors), so a scan-only module needs no
//! change. A [`persistent`](VTabModule::persistent) module keeps its rows in a real
//! `<vtab>_data` backing table; the engine creates it at `CREATE VIRTUAL TABLE`,
//! scans it for reads, and hands the module a [`VTabStore`] over it in `update`, so
//! the rows are transactional and survive reopening the database. Register a custom
//! module with `Connection::register_module`.

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
    /// The declared type of each column (parallel to `columns`; an empty string
    /// is untyped). Reported by `PRAGMA table_info`. Empty when built with
    /// [`new`](Self::new).
    pub types: Vec<String>,
}

impl VTabSchema {
    /// Build a schema from a list of (untyped) column names.
    pub fn new<I, S>(columns: I) -> VTabSchema
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let columns: Vec<String> = columns.into_iter().map(Into::into).collect();
        let types = alloc::vec![String::new(); columns.len()];
        VTabSchema { columns, types }
    }

    /// Build a schema from `(name, type)` pairs (a type may be empty for an
    /// untyped column). The declared types are reported by `PRAGMA table_info`.
    pub fn typed<I, S, T>(columns: I) -> VTabSchema
    where
        I: IntoIterator<Item = (S, T)>,
        S: Into<String>,
        T: Into<String>,
    {
        let mut names = Vec::new();
        let mut types = Vec::new();
        for (n, t) in columns {
            names.push(n.into());
            types.push(t.into());
        }
        VTabSchema {
            columns: names,
            types,
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

/// Persistent backing storage handed to a writable module's
/// [`update`](VTabModule::update). A persistent module's rows live in a real
/// regular table (`<vtab>_data`) — exactly how SQLite's FTS5 / R-Tree keep their
/// shadow tables — so writes go through the engine's normal, transactional table
/// machinery and the stored bytes are ordinary b-trees.
pub trait VTabStore {
    /// Every `(rowid, column values)` row currently in the backing table.
    fn rows(&self) -> Result<alloc::vec::Vec<(i64, alloc::vec::Vec<Value>)>>;
    /// Insert (or replace) the row `rowid` with `values`.
    fn put(&mut self, rowid: i64, values: &[Value]) -> Result<()>;
    /// Remove the row `rowid` (a no-op if absent).
    fn delete(&mut self, rowid: i64) -> Result<()>;
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

    /// Whether this module keeps its rows in a persistent backing table (see
    /// [`VTabStore`]). When `true`, the engine creates a `<vtab>_data` table at
    /// `CREATE VIRTUAL TABLE`, scans it to answer queries, and hands the module a
    /// [`VTabStore`] over it in [`update`](Self::update). The default `false` is a
    /// computed/read-only module (e.g. `series`).
    fn persistent(&self) -> bool {
        false
    }

    /// Apply a write to the table (SQLite's `xUpdate`), returning the rowid of the
    /// inserted/updated row (ignored for a delete).
    ///
    /// `args` are the `USING <name>(<args>)` arguments (as for [`connect`](Self::connect)).
    /// `store` is the module's persistent backing table (meaningful only when
    /// [`persistent`](Self::persistent) is `true`). The default makes the table
    /// **read-only** — it returns an error — so an existing read-only module needs
    /// no change. A writable module overrides this to service
    /// [`VTabChange::Insert`]/`Delete`/`Update`, persisting through `store`.
    fn update(
        &self,
        _args: &[&str],
        _change: VTabChange,
        _store: &mut dyn VTabStore,
    ) -> Result<i64> {
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
    /// See [`VTabModule::persistent`].
    fn dyn_persistent(&self) -> bool;
    /// See [`VTabModule::update`].
    fn dyn_update(
        &self,
        args: &[&str],
        change: VTabChange,
        store: &mut dyn VTabStore,
    ) -> Result<i64>;
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
    fn dyn_persistent(&self) -> bool {
        VTabModule::persistent(self)
    }
    fn dyn_update(
        &self,
        args: &[&str],
        change: VTabChange,
        store: &mut dyn VTabStore,
    ) -> Result<i64> {
        VTabModule::update(self, args, change, store)
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
        reg.register("rtree", Box::new(RTreeModule))
            .expect("fresh registry has no name collisions");
        reg.register("fts5", Box::new(Fts5Module))
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

// ---------------------------------------------------------------------------
// Built-in module: `rtree` — an N-dimensional spatial index (roadmap D3a).
// ---------------------------------------------------------------------------

/// Built-in `rtree` module: `USING rtree(id, minX, maxX[, minY, maxY, …])`
/// declares an integer `id` (which is the rowid) plus 2·N coordinate columns for
/// an N-dimensional bounding box. Rows persist in the `<name>_data` backing table
/// ([`persistent`](VTabModule::persistent) is `true`); spatial queries are answered
/// by scanning that table and re-applying the `WHERE` — functionally correct (D3a).
/// Efficient node-tree pushdown and byte-compatible `_node`/`_rowid`/`_parent`
/// shadow formats are later increments (D3b/D3c). Coordinates are stored as 32-bit
/// floats and the id as an integer, matching sqlite's value semantics.
#[derive(Debug, Default, Clone, Copy)]
pub struct RTreeModule;

/// Unused cursor — a persistent module's reads scan the backing table, not a
/// cursor. Present only to satisfy [`VTabModule`].
pub struct RTreeCursor;
/// Unused row type for [`RTreeCursor`].
pub struct RTreeRow;

impl VTabRow for RTreeRow {
    fn column(&self, _i: usize) -> Value {
        Value::Null
    }
    fn rowid(&self) -> i64 {
        0
    }
}

impl VTabCursor for RTreeCursor {
    type Row = RTreeRow;
    fn next(&mut self) -> Result<Option<RTreeRow>> {
        Ok(None)
    }
}

/// Coerce a value to the rtree id (a signed integer).
fn rtree_i64(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        Value::Real(r) => *r as i64,
        Value::Text(t) => t.parse().unwrap_or(0),
        _ => 0,
    }
}

/// The raw numeric value of a coordinate expression.
fn coord_f64(v: &Value) -> f64 {
    match v {
        Value::Integer(i) => *i as f64,
        Value::Real(r) => *r,
        Value::Text(t) => t.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `1 ∓ 2⁻²³` (2²³ = the f32 mantissa size) — SQLite's rtree rounding nudges.
const RTREE_RND_TOWARDS: f64 = 1.0 - 1.0 / 8388608.0;
const RTREE_RND_AWAY: f64 = 1.0 + 1.0 / 8388608.0;

/// A *minimum* coordinate, rounded to a 32-bit float **not greater** than it
/// (toward −∞) so the stored box never excludes the true point — byte-for-byte
/// SQLite's `rtreeValueDown` (nudge the magnitude before the f32 cast).
fn round_min_f32(d: f64) -> f64 {
    let f = d as f32;
    let f = if f64::from(f) > d {
        (d * if d < 0.0 {
            RTREE_RND_AWAY
        } else {
            RTREE_RND_TOWARDS
        }) as f32
    } else {
        f
    };
    f64::from(f)
}

/// A *maximum* coordinate, rounded to a 32-bit float **not less** than it (toward
/// +∞) — SQLite's `rtreeValueUp`.
fn round_max_f32(d: f64) -> f64 {
    let f = d as f32;
    let f = if f64::from(f) < d {
        (d * if d < 0.0 {
            RTREE_RND_TOWARDS
        } else {
            RTREE_RND_AWAY
        }) as f32
    } else {
        f
    };
    f64::from(f)
}

impl RTreeModule {
    /// The stored record: an integer id, then f32 coordinates — each min (odd
    /// column index) rounded down and each max (even index) rounded up. Errors
    /// like sqlite if any pair has `min > max`.
    fn record(values: &[Value]) -> Result<Vec<Value>> {
        let mut rec = Vec::with_capacity(values.len());
        rec.push(Value::Integer(rtree_i64(
            values.first().unwrap_or(&Value::Null),
        )));
        for (i, v) in values.iter().enumerate().skip(1) {
            let x = coord_f64(v);
            rec.push(Value::Real(if i % 2 == 1 {
                round_min_f32(x)
            } else {
                round_max_f32(x)
            }));
        }
        let mut k = 1;
        while k + 1 < rec.len() {
            if let (Value::Real(lo), Value::Real(hi)) = (&rec[k], &rec[k + 1]) {
                if lo > hi {
                    return Err(Error::Error(alloc::string::String::from(
                        "rtree constraint failed",
                    )));
                }
            }
            k += 2;
        }
        Ok(rec)
    }
}

impl VTabModule for RTreeModule {
    type Cursor = RTreeCursor;

    fn connect(&self, args: &[&str]) -> Result<VTabSchema> {
        // One id column + an even number (≥ 2) of coordinate columns.
        if args.len() < 3 || args.len().is_multiple_of(2) {
            return Err(Error::Error(alloc::string::String::from(
                "rtree requires an odd number of columns (id + 2N coordinates), \
                 at least 3",
            )));
        }
        // The id column is an integer; every coordinate is a 32-bit float (REAL),
        // matching sqlite's declared rtree column types.
        Ok(VTabSchema::typed(args.iter().enumerate().map(|(i, s)| {
            let ty = if i == 0 { "INT" } else { "REAL" };
            (String::from(*s), ty)
        })))
    }

    fn open(&self, _args: &[&str], _plan: &IndexPlan) -> Result<RTreeCursor> {
        Ok(RTreeCursor)
    }

    fn persistent(&self) -> bool {
        true
    }

    fn update(&self, _args: &[&str], change: VTabChange, store: &mut dyn VTabStore) -> Result<i64> {
        match change {
            VTabChange::Insert { values, .. } => {
                let mut rec = RTreeModule::record(values)?;
                // The id column is the rowid; a NULL id auto-assigns max+1.
                let id = if matches!(values.first(), Some(Value::Null) | None) {
                    store.rows()?.iter().map(|(r, _)| *r).max().unwrap_or(0) + 1
                } else {
                    rtree_i64(&values[0])
                };
                rec[0] = Value::Integer(id);
                store.put(id, &rec)?;
                Ok(id)
            }
            VTabChange::Delete { rowid } => {
                store.delete(rowid)?;
                Ok(rowid)
            }
            VTabChange::Update { rowid, values, .. } => {
                let mut rec = RTreeModule::record(values)?;
                let id = rtree_i64(&values[0]);
                rec[0] = Value::Integer(id);
                if id != rowid {
                    store.delete(rowid)?;
                }
                store.put(id, &rec)?;
                Ok(id)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in module: `fts5` — full-text search (roadmap D2). This first slice is
// the *document store*: `CREATE VIRTUAL TABLE … USING fts5(col, …)` declares the
// text columns, and `INSERT`/`UPDATE`/`DELETE`/`SELECT` round-trip documents
// through the persistent `<name>_data` backing table (W2). The tokenizer and the
// `MATCH` query (the inverted index) build on top of this in a follow-up.
// ---------------------------------------------------------------------------

/// SQLite's [FTS5](https://www.sqlite.org/fts5.html) full-text-search module.
///
/// `USING fts5(<col>, <col>, …)` declares one untyped column per name (FTS5
/// columns carry no type, like SQLite). Documents are stored in the persistent
/// `<name>_data` backing table keyed by an implicit integer rowid, so the table
/// behaves like an ordinary table for storage and retrieval. Column *options*
/// (e.g. `tokenize = 'porter'`, `prefix = '2'`) and per-column modifiers
/// (`col UNINDEXED`) are accepted and ignored by this slice — only the column
/// names are honored. `MATCH` querying is not yet implemented.
#[derive(Debug, Default, Clone, Copy)]
pub struct Fts5Module;

/// Unused cursor — FTS5 is persistent, so reads scan the backing table rather
/// than a module cursor (see [`RTreeCursor`]).
pub struct Fts5Cursor;
/// Unused row type for [`Fts5Cursor`].
pub struct Fts5Row;

impl VTabRow for Fts5Row {
    fn column(&self, _i: usize) -> Value {
        Value::Null
    }
    fn rowid(&self) -> i64 {
        0
    }
}

impl VTabCursor for Fts5Cursor {
    type Row = Fts5Row;
    fn next(&mut self) -> Result<Option<Fts5Row>> {
        Ok(None)
    }
}

/// Split `text` into FTS5 tokens: maximal runs of alphanumeric characters, each
/// folded to lowercase. This is a faithful approximation of SQLite's default
/// `unicode61` tokenizer for ASCII and basic text — it splits on every
/// non-alphanumeric byte and case-folds, so `"The quick-brown Fox!"` yields
/// `["the", "quick", "brown", "fox"]`. (Diacritic removal and the full Unicode
/// category tables are not modeled; ASCII text matches sqlite byte-for-byte.)
pub(crate) fn fts5_tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            tokens.push(core::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// One term of an FTS5 query: a phrase of one or more consecutive tokens,
/// optionally restricted to a named column (`col:token`) and/or ending in a
/// prefix token (`token*`).
#[derive(Clone)]
struct Fts5Term {
    /// The column the term is scoped to (`col:…`), or `None` for any column.
    column: Option<String>,
    /// The tokens that must appear consecutively and in order. A bare token is a
    /// one-element phrase; `"quick brown"` is a two-element phrase.
    phrase: Vec<String>,
    /// Whether the *last* token of the phrase is a prefix match (`token*`).
    prefix: bool,
}

/// Whether `phrase` occurs in `doc` as a run of consecutive tokens (in order).
/// When `prefix` is set, the final phrase token matches any document token that
/// starts with it (`fox*` matches `foxes`).
fn fts5_phrase_in(phrase: &[String], prefix: bool, doc: &[String]) -> bool {
    if phrase.is_empty() || doc.len() < phrase.len() {
        return false;
    }
    let last = phrase.len() - 1;
    (0..=doc.len() - phrase.len()).any(|start| {
        phrase.iter().enumerate().all(|(k, want)| {
            let got = &doc[start + k];
            if k == last && prefix {
                got.starts_with(want.as_str())
            } else {
                got == want
            }
        })
    })
}

/// A lexed token of an FTS5 query: a boolean operator, a parenthesis, or a term.
enum Fts5Lex {
    Or,
    And,
    Not,
    LParen,
    RParen,
    Term(Fts5Term),
}

/// Lex an FTS5 query string into operators, parentheses, and terms. `OR`/`AND`/
/// `NOT` are operators only as bare uppercase words (a lowercase `and` or a
/// `col:and` is an ordinary token, as in SQLite). A term is `[column:]body`,
/// where `body` is a `"quoted phrase"` or a bare word optionally ending in `*`.
fn fts5_lex(pattern: &str) -> Vec<Fts5Lex> {
    let chars: Vec<char> = pattern.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut out = Vec::new();
    while i < n {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '(' {
            out.push(Fts5Lex::LParen);
            i += 1;
            continue;
        }
        if ch == ')' {
            out.push(Fts5Lex::RParen);
            i += 1;
            continue;
        }
        // An optional `column:` prefix: a run of identifier chars then a colon.
        let mut column = None;
        let mut j = i;
        while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        if j > i && j < n && chars[j] == ':' {
            column = Some(chars[i..j].iter().collect());
            i = j + 1;
        }
        // The body: a quoted phrase, or a bare word that may end in `*`.
        let (text, prefix) = if i < n && chars[i] == '"' {
            i += 1;
            let start = i;
            while i < n && chars[i] != '"' {
                i += 1;
            }
            let body: String = chars[start..i].iter().collect();
            if i < n {
                i += 1; // closing quote
            }
            (body, false)
        } else {
            let start = i;
            while i < n && !chars[i].is_whitespace() && chars[i] != '(' && chars[i] != ')' {
                i += 1;
            }
            let raw: String = chars[start..i].iter().collect();
            // A bare, uncolumned uppercase keyword is a boolean operator.
            if column.is_none() {
                match raw.as_str() {
                    "OR" => {
                        out.push(Fts5Lex::Or);
                        continue;
                    }
                    "AND" => {
                        out.push(Fts5Lex::And);
                        continue;
                    }
                    "NOT" => {
                        out.push(Fts5Lex::Not);
                        continue;
                    }
                    _ => {}
                }
            }
            let mut body = raw;
            let prefix = body.ends_with('*');
            if prefix {
                body.pop();
            }
            (body, prefix)
        };
        let phrase = fts5_tokenize(&text);
        if !phrase.is_empty() {
            out.push(Fts5Lex::Term(Fts5Term {
                column,
                phrase,
                prefix,
            }));
        }
    }
    out
}

/// A parsed FTS5 boolean query tree (`A NOT B` means "A and not B").
enum Fts5Query {
    Term(Fts5Term),
    And(Box<Fts5Query>, Box<Fts5Query>),
    Or(Box<Fts5Query>, Box<Fts5Query>),
    Not(Box<Fts5Query>, Box<Fts5Query>),
}

/// Recursive-descent parser for the FTS5 boolean grammar, lowest precedence
/// (`OR`) outermost: `OR` of `AND`s (explicit or implicit by juxtaposition) of
/// `NOT`s of primaries, where a primary is a parenthesized query or a term.
struct Fts5Parser<'a> {
    toks: &'a [Fts5Lex],
    pos: usize,
}

impl Fts5Parser<'_> {
    fn parse(&mut self) -> Option<Fts5Query> {
        let q = self.parse_or();
        // A trailing unmatched operator/paren is simply ignored.
        q
    }

    fn parse_or(&mut self) -> Option<Fts5Query> {
        let mut left = self.parse_and()?;
        while matches!(self.toks.get(self.pos), Some(Fts5Lex::Or)) {
            self.pos += 1;
            match self.parse_and() {
                Some(right) => left = Fts5Query::Or(Box::new(left), Box::new(right)),
                None => break,
            }
        }
        Some(left)
    }

    fn parse_and(&mut self) -> Option<Fts5Query> {
        let mut left = self.parse_not()?;
        loop {
            match self.toks.get(self.pos) {
                Some(Fts5Lex::And) => self.pos += 1,
                // Juxtaposition (a term or `(`) is an implicit AND.
                Some(Fts5Lex::Term(_) | Fts5Lex::LParen) => {}
                _ => break,
            }
            match self.parse_not() {
                Some(right) => left = Fts5Query::And(Box::new(left), Box::new(right)),
                None => break,
            }
        }
        Some(left)
    }

    fn parse_not(&mut self) -> Option<Fts5Query> {
        let mut left = self.parse_primary()?;
        while matches!(self.toks.get(self.pos), Some(Fts5Lex::Not)) {
            self.pos += 1;
            match self.parse_primary() {
                Some(right) => left = Fts5Query::Not(Box::new(left), Box::new(right)),
                None => break,
            }
        }
        Some(left)
    }

    fn parse_primary(&mut self) -> Option<Fts5Query> {
        match self.toks.get(self.pos) {
            Some(Fts5Lex::LParen) => {
                self.pos += 1;
                let inner = self.parse_or();
                if matches!(self.toks.get(self.pos), Some(Fts5Lex::RParen)) {
                    self.pos += 1;
                }
                inner
            }
            Some(Fts5Lex::Term(t)) => {
                let t = t.clone();
                self.pos += 1;
                Some(Fts5Query::Term(t))
            }
            _ => None,
        }
    }
}

/// Whether a single term matches any in-scope column (respecting `col:` scoping).
fn fts5_term_matches(term: &Fts5Term, cols: &[(&str, Vec<String>)]) -> bool {
    cols.iter().any(|(name, tokens)| {
        term.column
            .as_deref()
            .is_none_or(|c| name.eq_ignore_ascii_case(c))
            && fts5_phrase_in(&term.phrase, term.prefix, tokens)
    })
}

/// Evaluate a parsed query tree against the tokenized in-scope columns.
fn fts5_eval(query: &Fts5Query, cols: &[(&str, Vec<String>)]) -> bool {
    match query {
        Fts5Query::Term(t) => fts5_term_matches(t, cols),
        Fts5Query::And(a, b) => fts5_eval(a, cols) && fts5_eval(b, cols),
        Fts5Query::Or(a, b) => fts5_eval(a, cols) || fts5_eval(b, cols),
        Fts5Query::Not(a, b) => fts5_eval(a, cols) && !fts5_eval(b, cols),
    }
}

/// Whether the in-scope columns satisfy the FTS5 query `pattern`. `cols` is the
/// `(name, text)` of each searchable column (one entry for a column-scoped
/// `col MATCH …`, every column for a table-wide `tbl MATCH …`). The query
/// supports bare tokens, `token*` prefixes, `"quoted phrases"`, `col:…` column
/// filters, and the boolean operators `AND` (explicit or implicit by
/// juxtaposition), `OR`, and `NOT` (binding tightest to loosest: `NOT`, `AND`,
/// `OR`) with parentheses — matching SQLite's default precedence. A query with no
/// tokens matches nothing. `NEAR` is not yet supported.
pub(crate) fn fts5_query_matches(pattern: &str, cols: &[(String, String)]) -> bool {
    let toks = fts5_lex(pattern);
    let query = match (Fts5Parser {
        toks: &toks,
        pos: 0,
    })
    .parse()
    {
        Some(q) => q,
        None => return false,
    };
    let tokenized: Vec<(&str, Vec<String>)> = cols
        .iter()
        .map(|(name, text)| (name.as_str(), fts5_tokenize(text)))
        .collect();
    fts5_eval(&query, &tokenized)
}

impl Fts5Module {
    /// The column name declared by one `USING fts5(…)` argument, or `None` if the
    /// argument is a configuration option (`key = value`) rather than a column.
    ///
    /// A column may carry a modifier (`title UNINDEXED`); only the leading
    /// identifier is the name. An option arg contains an `=` outside of quotes —
    /// for this slice the simple "`contains('=')`" test is enough, since column
    /// names never contain one.
    fn column_name(arg: &str) -> Option<String> {
        let arg = arg.trim();
        if arg.is_empty() || arg.contains('=') {
            return None;
        }
        // Strip a trailing modifier like `UNINDEXED`; keep the first token, and
        // unquote a `"quoted"` / `'quoted'` / `[bracketed]` identifier.
        let first = arg.split_whitespace().next().unwrap_or(arg);
        let name = first.trim_matches(|c| c == '"' || c == '\'' || c == '`');
        let name = name.strip_prefix('[').unwrap_or(name);
        let name = name.strip_suffix(']').unwrap_or(name);
        Some(String::from(name))
    }
}

impl VTabModule for Fts5Module {
    type Cursor = Fts5Cursor;

    fn connect(&self, args: &[&str]) -> Result<VTabSchema> {
        let columns: Vec<String> = args
            .iter()
            .filter_map(|a| Fts5Module::column_name(a))
            .collect();
        if columns.is_empty() {
            return Err(Error::Error(alloc::string::String::from(
                "fts5: no columns specified",
            )));
        }
        // FTS5 columns are untyped (PRAGMA table_info reports an empty type).
        Ok(VTabSchema::new(columns))
    }

    fn open(&self, _args: &[&str], _plan: &IndexPlan) -> Result<Fts5Cursor> {
        Ok(Fts5Cursor)
    }

    fn persistent(&self) -> bool {
        true
    }

    fn update(&self, _args: &[&str], change: VTabChange, store: &mut dyn VTabStore) -> Result<i64> {
        match change {
            VTabChange::Insert { rowid, values } => {
                // An explicit rowid is honored; otherwise assign max+1 (a fresh
                // table starts at 1), matching SQLite's implicit rowid.
                let id = match rowid {
                    Some(r) => r,
                    None => store.rows()?.iter().map(|(r, _)| *r).max().unwrap_or(0) + 1,
                };
                store.put(id, values)?;
                Ok(id)
            }
            VTabChange::Delete { rowid } => {
                store.delete(rowid)?;
                Ok(rowid)
            }
            VTabChange::Update {
                rowid,
                new_rowid,
                values,
            } => {
                if new_rowid != rowid {
                    store.delete(rowid)?;
                }
                store.put(new_rowid, values)?;
                Ok(new_rowid)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn fts5_tokenizer_splits_and_folds() {
        assert_eq!(
            fts5_tokenize("The quick-brown Fox!"),
            vec![
                String::from("the"),
                String::from("quick"),
                String::from("brown"),
                String::from("fox"),
            ]
        );
        // Digits are tokens; runs of punctuation/whitespace are separators only.
        assert_eq!(
            fts5_tokenize("  a1  b2,c3 "),
            vec![String::from("a1"), String::from("b2"), String::from("c3")]
        );
        assert!(fts5_tokenize("   ,. !").is_empty());
    }

    #[test]
    fn fts5_query_matches_are_token_anded() {
        let doc = [(String::from("body"), String::from("the quick brown fox"))];
        assert!(fts5_query_matches("fox", &doc));
        assert!(fts5_query_matches("QUICK fox", &doc)); // case-insensitive AND
        assert!(!fts5_query_matches("quick zebra", &doc)); // one token missing
        assert!(!fts5_query_matches("", &doc)); // empty query matches nothing
    }

    #[test]
    fn fts5_column_filters_scope_tokens() {
        let cols = [
            (String::from("title"), String::from("Mixed Fox")),
            (String::from("body"), String::from("and the dog")),
        ];
        // A bare token matches in any column; `col:token` only in that column.
        assert!(fts5_query_matches("fox", &cols));
        assert!(fts5_query_matches("title:fox", &cols));
        assert!(!fts5_query_matches("body:fox", &cols)); // fox is in title, not body
        assert!(fts5_query_matches("title:mixed body:dog", &cols)); // AND across columns
        assert!(!fts5_query_matches("title:dog", &cols)); // dog is in body, not title
    }

    #[test]
    fn fts5_phrase_and_prefix_queries() {
        let doc = [(
            String::from("body"),
            String::from("the quick brown fox runs"),
        )];
        // A quoted phrase requires consecutive, ordered tokens.
        assert!(fts5_query_matches("\"quick brown\"", &doc));
        assert!(!fts5_query_matches("\"brown quick\"", &doc)); // wrong order
        assert!(!fts5_query_matches("\"quick fox\"", &doc)); // not adjacent
                                                             // A `token*` prefix matches any token starting with it.
        assert!(fts5_query_matches("fo*", &doc)); // fox
        assert!(fts5_query_matches("run*", &doc)); // runs
        assert!(!fts5_query_matches("cat*", &doc));
        // Column-scoped phrase / prefix.
        assert!(fts5_query_matches("body:\"quick brown\"", &doc));
        assert!(fts5_query_matches("body:ru*", &doc));
    }

    #[test]
    fn fts5_boolean_operators_and_precedence() {
        let doc = |s: &str| [(String::from("body"), String::from(s))];
        // OR / AND / NOT.
        assert!(fts5_query_matches("apple OR cherry", &doc("apple banana")));
        assert!(!fts5_query_matches("apple AND date", &doc("apple banana")));
        assert!(fts5_query_matches("apple AND date", &doc("apple date")));
        assert!(fts5_query_matches(
            "banana NOT cherry",
            &doc("apple banana")
        ));
        assert!(!fts5_query_matches(
            "banana NOT cherry",
            &doc("banana cherry")
        ));
        // AND binds tighter than OR: `apple OR banana AND cherry`.
        assert!(fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("apple only")
        ));
        assert!(fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("banana cherry")
        ));
        assert!(!fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("banana only")
        ));
        // Parentheses override precedence.
        assert!(fts5_query_matches(
            "(apple OR banana) AND date",
            &doc("apple date")
        ));
        assert!(!fts5_query_matches(
            "(apple OR banana) AND date",
            &doc("apple only")
        ));
    }

    #[test]
    fn fts5_column_name_skips_options_and_modifiers() {
        assert_eq!(
            Fts5Module::column_name("title"),
            Some(String::from("title"))
        );
        assert_eq!(
            Fts5Module::column_name("body UNINDEXED"),
            Some(String::from("body"))
        );
        assert_eq!(Fts5Module::column_name("tokenize = 'porter'"), None);
        assert_eq!(
            Fts5Module::column_name("\"quoted\""),
            Some(String::from("quoted"))
        );
    }

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
