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

    /// The index of the declared column that is an alias for the table's rowid
    /// (like an `INTEGER PRIMARY KEY`), if any — e.g. the `rtree` `id` column. When
    /// set, an `INSERT` providing that column gives the row's rowid, so a duplicate
    /// is a UNIQUE conflict. `None` (the default) means the rowid is implicit (only
    /// an explicit `rowid`/`_rowid_`/`oid` term sets it).
    fn rowid_column(&self) -> Option<usize> {
        None
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
    /// See [`VTabModule::rowid_column`].
    fn dyn_rowid_column(&self) -> Option<usize>;
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
    fn dyn_rowid_column(&self) -> Option<usize> {
        VTabModule::rowid_column(self)
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
        reg.register("rtree", Box::new(RTreeModule { integer: false }))
            .expect("fresh registry has no name collisions");
        reg.register("rtree_i32", Box::new(RTreeModule { integer: true }))
            .expect("fresh registry has no name collisions");
        #[cfg(feature = "fts5")]
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
pub struct RTreeModule {
    /// `true` for the `rtree_i32` variant: coordinates are 32-bit integers
    /// (floats truncate toward zero) rather than 32-bit floats.
    pub integer: bool,
}

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
    /// The number of coordinate columns declared by a `USING rtree(…)` arg list:
    /// the id is column 0, the coordinates follow, and any trailing `+name`
    /// columns are auxiliary (non-spatial) data. Returns the coordinate count.
    fn n_coords(args: &[&str]) -> usize {
        let aux_start = args
            .iter()
            .skip(1)
            .position(|a| a.trim_start().starts_with('+'))
            .map_or(args.len(), |p| p + 1);
        aux_start.saturating_sub(1)
    }

    /// The stored record: an integer id, then `n_coords` coordinates — for the
    /// float `rtree` each min (odd column index) rounded down and each max (even
    /// index) rounded up; for `rtree_i32` each coordinate truncated toward zero to
    /// a 32-bit integer — then any auxiliary column values verbatim. Errors like
    /// sqlite if a coordinate pair has `min > max`.
    fn record(&self, values: &[Value], n_coords: usize) -> Result<Vec<Value>> {
        let mut rec = Vec::with_capacity(values.len());
        rec.push(Value::Integer(rtree_i64(
            values.first().unwrap_or(&Value::Null),
        )));
        for (i, v) in values.iter().enumerate().skip(1) {
            if i > n_coords {
                // Auxiliary column: stored as-is (not a coordinate).
                rec.push(v.clone());
            } else if self.integer {
                rec.push(Value::Integer(coord_i32(v)));
            } else {
                let x = coord_f64(v);
                rec.push(Value::Real(if i % 2 == 1 {
                    round_min_f32(x)
                } else {
                    round_max_f32(x)
                }));
            }
        }
        let mut k = 1;
        while k < n_coords && k + 1 < rec.len() {
            let (lo, hi) = (coord_f64(&rec[k]), coord_f64(&rec[k + 1]));
            if lo > hi {
                return Err(Error::Error(alloc::string::String::from(
                    "rtree constraint failed",
                )));
            }
            k += 2;
        }
        Ok(rec)
    }
}

/// Coerce a value to an `rtree_i32` coordinate: truncate toward zero (the C `int`
/// cast) and clamp to the signed 32-bit range.
fn coord_i32(v: &Value) -> i64 {
    // An `f64 as i64` cast truncates toward zero and maps NaN to 0; clamp the
    // result to the signed 32-bit range.
    (coord_f64(v) as i64).clamp(i32::MIN as i64, i32::MAX as i64)
}

impl VTabModule for RTreeModule {
    type Cursor = RTreeCursor;

    fn connect(&self, args: &[&str]) -> Result<VTabSchema> {
        // One id column + an even number (≥ 2) of coordinate columns, optionally
        // followed by `+name [type]` auxiliary columns.
        let n_coords = RTreeModule::n_coords(args);
        if n_coords < 2 || !n_coords.is_multiple_of(2) {
            return Err(Error::Error(alloc::string::String::from(
                "rtree requires an odd number of columns (id + 2N coordinates), \
                 at least 3",
            )));
        }
        // The id column is an integer; every coordinate is a 32-bit float (REAL);
        // an auxiliary `+name [type]` column keeps its declared type (or none).
        Ok(VTabSchema::typed(args.iter().enumerate().map(|(i, s)| {
            if i == 0 {
                (String::from(*s), String::from("INT"))
            } else if i <= n_coords {
                let ty = if self.integer { "INT" } else { "REAL" };
                (String::from(*s), String::from(ty))
            } else {
                // An auxiliary `+name [type]` column: SQLite reports an empty type
                // for it (the declared type is not retained), and stores values
                // verbatim with no affinity.
                let a = s.trim_start().strip_prefix('+').unwrap_or(s).trim();
                let name = a.split_once(char::is_whitespace).map_or(a, |(n, _)| n);
                (String::from(name), String::new())
            }
        })))
    }

    fn open(&self, _args: &[&str], _plan: &IndexPlan) -> Result<RTreeCursor> {
        Ok(RTreeCursor)
    }

    fn persistent(&self) -> bool {
        true
    }

    fn rowid_column(&self) -> Option<usize> {
        // The first column (`id`) is the rowid alias.
        Some(0)
    }

    /// Choose a plan from the offered constraints, matching SQLite's rtree
    /// `xBestIndex` so `EXPLAIN QUERY PLAN` reads identically. A usable `id =`
    /// (rowid) equality is a single-row lookup (`idxNum` 1, no `idxStr`, the
    /// coordinate constraints ignored). Otherwise (`idxNum` 2) each usable
    /// coordinate comparison is encoded as a two-character pair: an op letter
    /// (`A`=`=`, `B`=`<=`, `C`=`<`, `D`=`>=`, `E`=`>`) followed by the coordinate
    /// column's 0-based digit (`minX`→`0`, `maxX`→`1`, …) — e.g. `minX>=? AND
    /// maxX<=?` is `idxNum` 2, `idxStr` `D0B1`. (Execution still scans the backing
    /// table and re-applies the `WHERE`, so this only drives the reported plan.)
    fn best_index(&self, constraints: &[IndexConstraint]) -> Result<IndexPlan> {
        let mut argv_index = alloc::vec![0u32; constraints.len()];
        // A rowid (id, column 0) equality: a one-row lookup, coords dropped.
        if let Some(i) = constraints
            .iter()
            .position(|c| c.usable && c.column == 0 && c.op == ConstraintOp::Eq)
        {
            argv_index[i] = 1;
            return Ok(IndexPlan {
                idx_num: 1,
                idx_str: None,
                estimated_cost: 1.0,
                argv_index,
                omit: Vec::new(),
                order_by_consumed: false,
            });
        }
        // Otherwise: encode each usable coordinate (column ≥ 1) comparison.
        let mut idx_str = String::new();
        let mut argc = 0u32;
        for (i, c) in constraints.iter().enumerate() {
            if !c.usable || c.column == 0 {
                continue;
            }
            let op = match c.op {
                ConstraintOp::Eq => 'A',
                ConstraintOp::Le => 'B',
                ConstraintOp::Lt => 'C',
                ConstraintOp::Ge => 'D',
                ConstraintOp::Gt => 'E',
                _ => continue,
            };
            idx_str.push(op);
            idx_str.push(char::from(b'0' + (c.column - 1) as u8));
            argc += 1;
            argv_index[i] = argc;
        }
        Ok(IndexPlan {
            idx_num: 2,
            idx_str: (!idx_str.is_empty()).then_some(idx_str),
            estimated_cost: if argc > 0 { 10.0 } else { 1e9 },
            argv_index,
            omit: Vec::new(),
            order_by_consumed: false,
        })
    }

    fn update(&self, args: &[&str], change: VTabChange, store: &mut dyn VTabStore) -> Result<i64> {
        let n_coords = RTreeModule::n_coords(args);
        match change {
            VTabChange::Insert { values, .. } => {
                let mut rec = self.record(values, n_coords)?;
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
                let mut rec = self.record(values, n_coords)?;
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
#[cfg(feature = "fts5")]
pub struct Fts5Module;

/// Unused cursor — FTS5 is persistent, so reads scan the backing table rather
/// than a module cursor (see [`RTreeCursor`]).
#[cfg(feature = "fts5")]
pub struct Fts5Cursor;
/// Unused row type for [`Fts5Cursor`].
#[cfg(feature = "fts5")]
pub struct Fts5Row;

#[cfg(feature = "fts5")]
impl VTabRow for Fts5Row {
    fn column(&self, _i: usize) -> Value {
        Value::Null
    }
    fn rowid(&self) -> i64 {
        0
    }
}

#[cfg(feature = "fts5")]
impl VTabCursor for Fts5Cursor {
    type Row = Fts5Row;
    fn next(&mut self) -> Result<Option<Fts5Row>> {
        Ok(None)
    }
}

/// Whether `b[i]` is a consonant under the Porter rules: any letter except a
/// vowel, where `y` is a consonant at the start or after a vowel and a vowel
/// after a consonant.
#[cfg(feature = "fts5")]
fn porter_cons(b: &[u8], i: usize) -> bool {
    match b[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => i == 0 || !porter_cons(b, i - 1),
        _ => true,
    }
}

/// The Porter "measure" of `b[0..len]`: the number of vowel→consonant transitions
/// (`[C](VC)^m[V]` → `m`).
#[cfg(feature = "fts5")]
fn porter_m(b: &[u8], len: usize) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < len && porter_cons(b, i) {
        i += 1;
    }
    while i < len {
        while i < len && !porter_cons(b, i) {
            i += 1;
        }
        if i >= len {
            break;
        }
        n += 1;
        while i < len && porter_cons(b, i) {
            i += 1;
        }
    }
    n
}

/// Whether `b[0..len]` contains a vowel.
#[cfg(feature = "fts5")]
fn porter_vowel_in_stem(b: &[u8], len: usize) -> bool {
    (0..len).any(|i| !porter_cons(b, i))
}

/// Whether `b[0..len]` ends in a doubled consonant.
#[cfg(feature = "fts5")]
fn porter_doublec(b: &[u8], len: usize) -> bool {
    len >= 2 && b[len - 1] == b[len - 2] && porter_cons(b, len - 1)
}

/// The Porter `*o` test: `b[0..len]` ends consonant-vowel-consonant where the
/// final consonant is not `w`, `x`, or `y`.
#[cfg(feature = "fts5")]
fn porter_cvc(b: &[u8], len: usize) -> bool {
    len >= 3
        && porter_cons(b, len - 1)
        && !porter_cons(b, len - 2)
        && porter_cons(b, len - 3)
        && !matches!(b[len - 1], b'w' | b'x' | b'y')
}

/// In `b`, if it ends with `suf` and the measure of the preceding stem is `>
/// min_m`, replace `suf` with `rep`. Returns whether `suf` matched at all (so the
/// caller stops trying further rules in the step, as the Porter dispatch does).
#[cfg(feature = "fts5")]
fn porter_r(b: &mut Vec<u8>, suf: &[u8], rep: &[u8], min_m: usize) -> bool {
    if b.ends_with(suf) {
        let pre = b.len() - suf.len();
        if porter_m(b, pre) > min_m {
            b.truncate(pre);
            b.extend_from_slice(rep);
        }
        return true;
    }
    false
}

/// The Porter (1980) stemming algorithm as implemented by SQLite's FTS5 `porter`
/// tokenizer. Only all-ASCII-lowercase tokens of length 3..=64 are stemmed;
/// anything else is returned unchanged.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_porter_stem(word: &str) -> String {
    let raw = word.as_bytes();
    if !(3..=64).contains(&word.len()) || !raw.iter().all(u8::is_ascii_lowercase) {
        return String::from(word);
    }
    let mut b = raw.to_vec();

    // Step 1a.
    if b.ends_with(b"sses") || b.ends_with(b"ies") {
        let n = b.len();
        b.truncate(n - 2);
    } else if b.ends_with(b"s") && !b.ends_with(b"ss") {
        b.pop();
    }

    // Step 1b.
    let mut step1b2 = false;
    if b.ends_with(b"eed") {
        let pre = b.len() - 3;
        if porter_m(&b, pre) > 0 {
            b.pop(); // eed -> ee
        }
    } else if b.ends_with(b"ed") {
        let pre = b.len() - 2;
        if porter_vowel_in_stem(&b, pre) {
            b.truncate(pre);
            step1b2 = true;
        }
    } else if b.ends_with(b"ing") {
        let pre = b.len() - 3;
        if porter_vowel_in_stem(&b, pre) {
            b.truncate(pre);
            step1b2 = true;
        }
    }
    if step1b2 {
        if b.ends_with(b"at") || b.ends_with(b"bl") || b.ends_with(b"iz") {
            b.push(b'e');
        } else if porter_doublec(&b, b.len()) && !matches!(b[b.len() - 1], b'l' | b's' | b'z') {
            b.pop();
        } else if porter_m(&b, b.len()) == 1 && porter_cvc(&b, b.len()) {
            b.push(b'e');
        }
    }

    // Step 1c: (*v*) y -> i.
    if b.ends_with(b"y") && porter_vowel_in_stem(&b, b.len() - 1) {
        let n = b.len();
        b[n - 1] = b'i';
    }

    // Step 2 (m>0), longest suffix first.
    for (suf, rep) in [
        (b"ational".as_ref(), b"ate".as_ref()),
        (b"tional", b"tion"),
        (b"enci", b"ence"),
        (b"anci", b"ance"),
        (b"izer", b"ize"),
        (b"logi", b"log"),
        (b"bli", b"ble"),
        (b"alli", b"al"),
        (b"entli", b"ent"),
        (b"eli", b"e"),
        (b"ousli", b"ous"),
        (b"ization", b"ize"),
        (b"ation", b"ate"),
        (b"ator", b"ate"),
        (b"alism", b"al"),
        (b"iveness", b"ive"),
        (b"fulness", b"ful"),
        (b"ousness", b"ous"),
        (b"aliti", b"al"),
        (b"iviti", b"ive"),
        (b"biliti", b"ble"),
    ] {
        if porter_r(&mut b, suf, rep, 0) {
            break;
        }
    }

    // Step 3 (m>0).
    for (suf, rep) in [
        (b"icate".as_ref(), b"ic".as_ref()),
        (b"ative", b""),
        (b"alize", b"al"),
        (b"iciti", b"ic"),
        (b"ical", b"ic"),
        (b"ful", b""),
        (b"ness", b""),
    ] {
        if porter_r(&mut b, suf, rep, 0) {
            break;
        }
    }

    // Step 4 (m>1); `ion` only when the stem ends in `s` or `t`.
    let step4: [&[u8]; 19] = [
        b"al", b"ance", b"ence", b"er", b"ic", b"able", b"ible", b"ant", b"ement", b"ment", b"ent",
        b"ou", b"ism", b"ate", b"iti", b"ous", b"ive", b"ize", b"ion",
    ];
    // Longest-first so `ement` wins over `ment`/`ent`.
    let mut order: Vec<&[u8]> = step4.to_vec();
    order.sort_by_key(|s| core::cmp::Reverse(s.len()));
    for suf in order {
        if b.ends_with(suf) {
            let pre = b.len() - suf.len();
            if suf == b"ion" {
                if porter_m(&b, pre) > 1 && matches!(b.get(pre - 1), Some(b's') | Some(b't')) {
                    b.truncate(pre);
                }
            } else if porter_m(&b, pre) > 1 {
                b.truncate(pre);
            }
            break;
        }
    }

    // Step 5a: drop final `e`.
    if b.ends_with(b"e") {
        let pre = b.len() - 1;
        let m = porter_m(&b, pre);
        if m > 1 || (m == 1 && !porter_cvc(&b, pre)) {
            b.truncate(pre);
        }
    }
    // Step 5b: `ll` -> `l` when m>1.
    if porter_doublec(&b, b.len()) && b.ends_with(b"l") && porter_m(&b, b.len()) > 1 {
        b.pop();
    }

    String::from_utf8(b).unwrap_or_else(|_| String::from(word))
}

/// Split `text` into FTS5 tokens: maximal runs of alphanumeric characters, each
/// folded to lowercase. This is a faithful approximation of SQLite's default
/// `unicode61` tokenizer for ASCII and basic text — it splits on every
/// non-alphanumeric byte and case-folds, so `"The quick-brown Fox!"` yields
/// `["the", "quick", "brown", "fox"]`. (Diacritic removal and the full Unicode
/// category tables are not modeled; ASCII text matches sqlite byte-for-byte.)
/// With `stem`, each token is then reduced by the Porter stemmer (the `porter`
/// tokenizer).
#[cfg(feature = "fts5")]
pub(crate) fn fts5_tokenize(text: &str, stem: bool) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            let t = core::mem::take(&mut cur);
            tokens.push(if stem { fts5_porter_stem(&t) } else { t });
        }
    }
    if !cur.is_empty() {
        tokens.push(if stem { fts5_porter_stem(&cur) } else { cur });
    }
    tokens
}

/// Like [`fts5_tokenize`], but also returns each token's `[start, end)` byte range
/// in the original `text` — so [`fts5_highlight`] can wrap matched tokens while
/// preserving the surrounding original characters. The span is over the original
/// text even when the token itself is Porter-stemmed.
#[cfg(feature = "fts5")]
fn fts5_tokenize_spans(text: &str, stem: bool) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    let push = |cur: &mut String, start: usize, end: usize, out: &mut Vec<_>| {
        let t = core::mem::take(cur);
        out.push((if stem { fts5_porter_stem(&t) } else { t }, start, end));
    };
    for (i, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if cur.is_empty() {
                start = i;
            }
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            push(&mut cur, start, i, &mut out);
        }
    }
    if !cur.is_empty() {
        push(&mut cur, start, text.len(), &mut out);
    }
    out
}

/// SQLite's `highlight(t, col, open, close)`: return column `col`'s `text` with
/// every token that is part of a `query` phrase match wrapped in `open`…`close`.
/// Adjacent matched tokens (e.g. a matched phrase) share one pair of markers, and
/// the original inter-token characters are preserved. `scope` is the operand
/// column of a `col MATCH …` query (the whole query is restricted to it).
#[cfg(feature = "fts5")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn fts5_highlight(
    query: &str,
    col_names: &[String],
    scope: Option<&str>,
    col: usize,
    text: &str,
    stem: bool,
    open: &str,
    close: &str,
) -> String {
    // A column outside the query's scope has nothing highlighted.
    if scope.is_some_and(|s| {
        col_names
            .get(col)
            .is_none_or(|n| !n.eq_ignore_ascii_case(s))
    }) {
        return String::from(text);
    }
    let toks = fts5_lex(query);
    let parsed = match (Fts5Parser {
        toks: &toks,
        pos: 0,
    })
    .parse()
    {
        Some(q) => q,
        None => return String::from(text),
    };
    let mut terms = Vec::new();
    fts5_collect_terms(&parsed, &mut terms);

    let spans = fts5_tokenize_spans(text, stem);
    let col_tokens: Vec<String> = spans.iter().map(|(t, _, _)| t.clone()).collect();
    // Each phrase *instance* is one highlight span `[start, end)` (token indices);
    // SQLite wraps each separately, so two adjacent single-token matches become
    // `[fox] [fox]`, while a matched two-word phrase is one `[quick brown]`.
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for term in &terms {
        // Skip a term scoped to a different column.
        if term.column.as_deref().is_some_and(|c| {
            col_names
                .get(col)
                .is_none_or(|n| !n.eq_ignore_ascii_case(c))
        }) {
            continue;
        }
        for start in fts5_term_starts(term, &col_tokens, stem) {
            hits.push((start, (start + term.phrase.len()).min(spans.len())));
        }
    }
    hits.sort_unstable();
    // Merge only genuinely overlapping instances (a shared token); adjacent ones
    // (`end == next start`) stay separate, matching SQLite.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in hits {
        match merged.last_mut() {
            Some(last) if s < last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }

    // Rebuild the text, wrapping each span's token run in the markers.
    let mut out = String::new();
    let mut last = 0;
    for (s, e) in merged {
        out.push_str(&text[last..spans[s].1]);
        out.push_str(open);
        out.push_str(&text[spans[s].1..spans[e - 1].2]);
        out.push_str(close);
        last = spans[e - 1].2;
    }
    out.push_str(&text[last..]);
    out
}

/// SQLite's `snippet(t, col, open, close, ellipsis, n)`: a window of up to `n`
/// tokens from column `col` chosen to best cover the query's phrases, with the
/// matched tokens wrapped in `open`…`close` and `ellipsis` prepended/appended when
/// the window doesn't reach the column's start/end. The window is the candidate
/// (centered on a phrase instance, or snapped to a sentence/column start) maximizing
/// distinct phrase coverage — matching fts5's `snippet` aux function. A negative
/// `col` auto-selects the highest-scoring column (`cols` holds every column's text).
#[cfg(feature = "fts5")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn fts5_snippet(
    query: &str,
    col_names: &[String],
    scope: Option<&str>,
    col: i64,
    cols: &[String],
    indexed: Option<&[String]>,
    stem: bool,
    open: &str,
    close: &str,
    ellipsis: &str,
    ntokens: usize,
) -> String {
    let ntok = ntokens.max(1);
    // A column is searchable unless declared `UNINDEXED`; an unindexed column has
    // no matches, and SQLite renders it verbatim.
    let searchable = |ci: usize| -> bool {
        match (indexed, col_names.get(ci)) {
            (Some(cols), Some(name)) => cols.iter().any(|c| c.eq_ignore_ascii_case(name)),
            _ => true,
        }
    };
    // An explicit `UNINDEXED` column is returned as-is (no window, no markers).
    if col >= 0 && !searchable(col as usize) {
        return cols.get(col as usize).cloned().unwrap_or_default();
    }
    let lexed = fts5_lex(query);
    let parsed = (Fts5Parser {
        toks: &lexed,
        pos: 0,
    })
    .parse();
    let mut terms = Vec::new();
    if let Some(p) = &parsed {
        fts5_collect_terms(p, &mut terms);
    }

    // Per-column window selection → (score, ws, we, spans, instances).
    type Spans = Vec<(String, usize, usize)>;
    type Inst = Vec<(usize, usize, usize)>;
    let select = |ci: usize| -> (i64, usize, usize, Spans, Inst) {
        let text = cols.get(ci).map(String::as_str).unwrap_or("");
        let spans = fts5_tokenize_spans(text, stem);
        let n = spans.len();
        // Operand-level scope (`col MATCH …`) and per-term `col:token` filters both
        // gate which instances count toward this column.
        let in_scope = searchable(ci)
            && scope.is_none_or(|s| {
                col_names
                    .get(ci)
                    .is_some_and(|nm| nm.eq_ignore_ascii_case(s))
            });
        let mut inst: Inst = Vec::new();
        if in_scope {
            let col_tokens: Vec<String> = spans.iter().map(|(t, _, _)| t.clone()).collect();
            for (ti, term) in terms.iter().enumerate() {
                if term.column.as_deref().is_some_and(|c| {
                    col_names
                        .get(ci)
                        .is_none_or(|nm| !nm.eq_ignore_ascii_case(c))
                }) {
                    continue;
                }
                for start in fts5_term_starts(term, &col_tokens, stem) {
                    inst.push((start, (start + term.phrase.len()).min(n), ti));
                }
            }
        }
        inst.sort_unstable();
        // Score window `[a, a+ntok)`; return (score, iFirst, iLast) of its cluster.
        let win = |a: usize| -> (i64, Option<(usize, usize)>) {
            let e = a + ntok;
            let mut seen = alloc::vec![false; terms.len()];
            let mut sc = 0;
            let (mut first, mut last) = (None, 0);
            for &(p, pe, ti) in inst.iter().filter(|(p, _, _)| *p >= a && *p < e) {
                sc += if seen[ti] { 1 } else { 1000 };
                seen[ti] = true;
                first.get_or_insert(p);
                last = pe;
            }
            (sc, first.map(|f| (f, last)))
        };
        if n <= ntok {
            // The whole column fits; its score still ranks it for auto-selection.
            return (win(0).0, 0, n, spans, inst);
        }
        if inst.is_empty() {
            return (0, 0, ntok, spans, inst);
        }
        // For each phrase instance, score the window starting at it and consider two
        // starts — the *centered* `iAdj`, and the enclosing *sentence boundary* (with
        // a +120/+100 bonus favoring a sentence/column start). Best score wins, first
        // anchor breaking ties; `iLast - iFirst` spans the cluster inside the window.
        let max_start = (n - ntok) as isize;
        // Sentence starts (token indices): token 0, plus any token immediately
        // preceded by whitespace whose nearest non-space byte before it is `.`/`:`.
        let bytes = text.as_bytes();
        let mut sentences = alloc::vec![0usize];
        for (t, &(_, start_off, _)) in spans.iter().enumerate().skip(1) {
            let mut i = start_off as isize - 1;
            while i >= 0 && matches!(bytes[i as usize], b' ' | b'\t' | b'\n' | b'\r') {
                i -= 1;
            }
            if i != start_off as isize - 1 && i >= 0 && matches!(bytes[i as usize], b'.' | b':') {
                sentences.push(t);
            }
        }
        let mut best_score = 0;
        let mut best_start = 0;
        for &(io, _, _) in &inst {
            let (score, cluster) = win(io);
            if score > best_score {
                best_score = score;
                let (f, l) = cluster.unwrap_or((io, io));
                let adj = (f as isize - (ntok as isize - (l - f) as isize) / 2).min(max_start);
                best_start = adj.max(0) as usize;
            }
            let mut jj = 0;
            while jj + 1 < sentences.len() && sentences[jj + 1] <= io {
                jj += 1;
            }
            let s = sentences[jj];
            if s < io {
                let score = win(s).0 + if s == 0 { 120 } else { 100 };
                if score > best_score {
                    best_score = score;
                    best_start = s;
                }
            }
        }
        // A sentence-boundary start is not clamped, so the window may run to the end.
        (
            best_score,
            best_start,
            (best_start + ntok).min(n),
            spans,
            inst,
        )
    };

    // A negative `col` picks the highest-scoring column (first on a tie, matching
    // SQLite's `nScore > nBestScore`); otherwise the requested column (out of range
    // → empty result).
    let ncol = col_names.len();
    let (chosen, picked) = if col >= 0 {
        let ci = col as usize;
        if ci >= ncol {
            return String::new();
        }
        (ci, select(ci))
    } else if ncol == 0 {
        return String::new();
    } else {
        let mut best_ci = 0;
        let mut best = select(0);
        for ci in 1..ncol {
            let r = select(ci);
            if r.0 > best.0 {
                best = r;
                best_ci = ci;
            }
        }
        (best_ci, best)
    };
    let text = cols.get(chosen).map(String::as_str).unwrap_or("");
    let (_, ws, we, spans, inst) = picked;
    let n = spans.len();
    if n == 0 {
        return String::from(text);
    }

    // Build the snippet: ellipsis, then the window's original text with matched
    // tokens wrapped (instances merged like `highlight`), then ellipsis.
    let mut hits: Vec<(usize, usize)> = inst
        .iter()
        .filter(|(s, _, _)| *s >= ws && *s < we)
        .map(|(s, e, _)| (*s, (*e).min(we)))
        .collect();
    hits.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in hits {
        match merged.last_mut() {
            Some(l) if s < l.1 => l.1 = l.1.max(e),
            _ => merged.push((s, e)),
        }
    }

    let mut out = String::new();
    if ws > 0 {
        out.push_str(ellipsis);
    }
    // When the window reaches the last token, SQLite appends the remaining input
    // text (so a trailing `.` survives) instead of an ellipsis.
    let reaches_end = we == n;
    let win_end = if reaches_end {
        text.len()
    } else {
        spans[we - 1].2
    };
    let mut last = spans[ws].1;
    for (s, e) in merged {
        out.push_str(&text[last..spans[s].1]);
        out.push_str(open);
        out.push_str(&text[spans[s].1..spans[e - 1].2]);
        out.push_str(close);
        last = spans[e - 1].2;
    }
    out.push_str(&text[last..win_end]);
    if !reaches_end {
        out.push_str(ellipsis);
    }
    out
}

/// One term of an FTS5 query: a phrase of one or more consecutive tokens,
/// optionally restricted to a named column (`col:token`), anchored to the start
/// of the column (`^token`), and/or ending in a prefix token (`token*`).
#[derive(Clone)]
#[cfg(feature = "fts5")]
struct Fts5Term {
    /// The column the term is scoped to (`col:…`), or `None` for any column.
    column: Option<String>,
    /// The tokens that must appear consecutively and in order. A bare token is a
    /// one-element phrase; `"quick brown"` is a two-element phrase.
    phrase: Vec<String>,
    /// Whether the *last* token of the phrase is a prefix match (`token*`).
    prefix: bool,
    /// Whether the phrase is anchored to the first token of the column (`^token`).
    anchored: bool,
}

/// The start offsets at which `term` matches in a column's `tokens`, honoring an
/// `^` anchor (which keeps only a match at offset 0).
#[cfg(feature = "fts5")]
fn fts5_term_starts(term: &Fts5Term, tokens: &[String], stem: bool) -> Vec<usize> {
    // Under the `porter` tokenizer the document tokens are stemmed, so the query
    // phrase must be stemmed the same way to match (including the prefix token —
    // SQLite runs query tokens through the tokenizer too).
    let phrase: Vec<String> = if stem {
        term.phrase.iter().map(|t| fts5_porter_stem(t)).collect()
    } else {
        term.phrase.clone()
    };
    let mut starts = fts5_phrase_starts(&phrase, term.prefix, tokens);
    if term.anchored {
        starts.retain(|&s| s == 0);
    }
    starts
}

/// Every start offset at which `phrase` occurs in `doc` as a run of consecutive
/// tokens (in order), ascending. When `prefix` is set, the final phrase token
/// matches any document token that starts with it (`fox*` matches `foxes`).
#[cfg(feature = "fts5")]
fn fts5_phrase_starts(phrase: &[String], prefix: bool, doc: &[String]) -> Vec<usize> {
    if phrase.is_empty() || doc.len() < phrase.len() {
        return Vec::new();
    }
    let last = phrase.len() - 1;
    (0..=doc.len() - phrase.len())
        .filter(|&start| {
            phrase.iter().enumerate().all(|(k, want)| {
                let got = &doc[start + k];
                if k == last && prefix {
                    got.starts_with(want.as_str())
                } else {
                    got == want
                }
            })
        })
        .collect()
}

/// Whether a `NEAR(p1 p2 …, n)` group is satisfied within a single column's
/// `tokens`: there is one instance of each phrase such that the span from the
/// first to the last token (inclusive) is at most `n` larger than the phrases'
/// combined length — SQLite's rule `(max_end − min_start + 1) ≤ n + total_len`.
/// Uses the "smallest range covering K sorted lists" sweep: each phrase's start
/// offsets are ascending and (with constant phrase length) so are its ends, so
/// repeatedly advancing the phrase at the current minimum start finds the
/// tightest window.
#[cfg(feature = "fts5")]
fn fts5_near_matches(phrases: &[(Vec<usize>, usize)], n: usize) -> bool {
    if phrases.iter().any(|(starts, _)| starts.is_empty()) {
        return false;
    }
    let total_len: usize = phrases.iter().map(|(_, len)| *len).sum();
    let mut ptr = alloc::vec![0usize; phrases.len()];
    loop {
        let mut min_start = usize::MAX;
        let mut max_end = 0;
        let mut min_phrase = 0;
        for (i, (starts, len)) in phrases.iter().enumerate() {
            let s = starts[ptr[i]];
            let e = s + len - 1;
            if s < min_start {
                min_start = s;
                min_phrase = i;
            }
            max_end = max_end.max(e);
        }
        if max_end - min_start < n + total_len {
            return true;
        }
        // Advance the phrase at the smallest start to try to tighten the window.
        ptr[min_phrase] += 1;
        if ptr[min_phrase] >= phrases[min_phrase].0.len() {
            return false;
        }
    }
}

/// A lexed token of an FTS5 query: a boolean operator, a parenthesis, a term, or
/// a `NEAR(phrase … , n)` group (its phrases and distance, default 10).
#[cfg(feature = "fts5")]
enum Fts5Lex {
    Or,
    And,
    Not,
    LParen,
    RParen,
    Term(Fts5Term),
    Near(Vec<Fts5Term>, usize),
}

/// Lex an FTS5 query string into operators, parentheses, and terms. `OR`/`AND`/
/// `NOT` are operators only as bare uppercase words (a lowercase `and` or a
/// `col:and` is an ordinary token, as in SQLite). A term is `[column:]body`,
/// where `body` is a `"quoted phrase"` or a bare word optionally ending in `*`.
#[cfg(feature = "fts5")]
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
        // SQLite allows whitespace around the colon (`col : token` == `col:token`).
        let mut column = None;
        let mut j = i;
        while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        let mut k = j;
        while k < n && chars[k].is_whitespace() {
            k += 1;
        }
        if j > i && k < n && chars[k] == ':' {
            column = Some(chars[i..j].iter().collect());
            i = k + 1;
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
        }
        // A leading `^` anchors the term to the first token of the column.
        let anchored = i < n && chars[i] == '^';
        if anchored {
            i += 1;
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
                    // `NEAR(p1 p2 …, n)`: a parenthesized phrase group. Without a
                    // following `(`, plain `NEAR` is an ordinary token.
                    "NEAR" => {
                        let mut k = i;
                        while k < n && chars[k].is_whitespace() {
                            k += 1;
                        }
                        if k < n && chars[k] == '(' {
                            let start = k + 1;
                            let mut depth = 1;
                            let mut e = start;
                            while e < n && depth > 0 {
                                match chars[e] {
                                    '(' => depth += 1,
                                    ')' => depth -= 1,
                                    _ => {}
                                }
                                if depth == 0 {
                                    break;
                                }
                                e += 1;
                            }
                            let inside: String = chars[start..e].iter().collect();
                            i = if e < n { e + 1 } else { e };
                            let (phrases, dist) = fts5_parse_near(&inside);
                            if !phrases.is_empty() {
                                out.push(Fts5Lex::Near(phrases, dist));
                            }
                            continue;
                        }
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
        // The query phrase is kept raw here; Porter stemming (if any) is applied at
        // match time in `fts5_term_starts` so the prefix flag is handled correctly.
        let phrase = fts5_tokenize(&text, false);
        if !phrase.is_empty() {
            out.push(Fts5Lex::Term(Fts5Term {
                column,
                phrase,
                prefix,
                anchored,
            }));
        }
    }
    out
}

/// Split the body of a `NEAR(…)` group into its phrases and distance. The
/// distance is the integer after a trailing comma (`NEAR(a b, 5)`); without one
/// it defaults to 10, as in SQLite.
#[cfg(feature = "fts5")]
fn fts5_parse_near(inside: &str) -> (Vec<Fts5Term>, usize) {
    let (phrases_part, distance) = match inside.rsplit_once(',') {
        Some((left, right))
            if !right.trim().is_empty() && right.trim().bytes().all(|b| b.is_ascii_digit()) =>
        {
            (left, right.trim().parse::<usize>().unwrap_or(10))
        }
        _ => (inside, 10),
    };
    let phrases = fts5_lex(phrases_part)
        .into_iter()
        .filter_map(|t| match t {
            Fts5Lex::Term(term) => Some(term),
            _ => None,
        })
        .collect();
    (phrases, distance)
}

/// A parsed FTS5 boolean query tree (`A NOT B` means "A and not B").
#[cfg(feature = "fts5")]
enum Fts5Query {
    Term(Fts5Term),
    /// A `NEAR(phrase … , n)` group: all phrases must appear within `n` tokens.
    Near(Vec<Fts5Term>, usize),
    And(Box<Fts5Query>, Box<Fts5Query>),
    Or(Box<Fts5Query>, Box<Fts5Query>),
    Not(Box<Fts5Query>, Box<Fts5Query>),
}

/// Recursive-descent parser for the FTS5 boolean grammar, lowest precedence
/// (`OR`) outermost: `OR` of `AND`s (explicit or implicit by juxtaposition) of
/// `NOT`s of primaries, where a primary is a parenthesized query or a term.
#[cfg(feature = "fts5")]
struct Fts5Parser<'a> {
    toks: &'a [Fts5Lex],
    pos: usize,
}

#[cfg(feature = "fts5")]
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
                // Juxtaposition (a term, NEAR group, or `(`) is an implicit AND.
                Some(Fts5Lex::Term(_) | Fts5Lex::Near(..) | Fts5Lex::LParen) => {}
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
            Some(Fts5Lex::Near(phrases, dist)) => {
                let q = Fts5Query::Near(phrases.clone(), *dist);
                self.pos += 1;
                Some(q)
            }
            _ => None,
        }
    }
}

/// Whether a single term matches any in-scope column (respecting `col:` scoping
/// and the `^` anchor).
#[cfg(feature = "fts5")]
fn fts5_term_matches(term: &Fts5Term, cols: &[(&str, Vec<String>)], stem: bool) -> bool {
    cols.iter().any(|(name, tokens)| {
        term.column
            .as_deref()
            .is_none_or(|c| name.eq_ignore_ascii_case(c))
            && !fts5_term_starts(term, tokens, stem).is_empty()
    })
}

/// Whether a `NEAR` group is satisfied: some single in-scope column contains all
/// of its phrases within the distance window.
#[cfg(feature = "fts5")]
fn fts5_near_group_matches(
    phrases: &[Fts5Term],
    dist: usize,
    cols: &[(&str, Vec<String>)],
    stem: bool,
) -> bool {
    cols.iter().any(|(_, tokens)| {
        let positioned: Vec<(Vec<usize>, usize)> = phrases
            .iter()
            .map(|p| (fts5_term_starts(p, tokens, stem), p.phrase.len()))
            .collect();
        fts5_near_matches(&positioned, dist)
    })
}

/// Evaluate a parsed query tree against the tokenized in-scope columns.
#[cfg(feature = "fts5")]
fn fts5_eval(query: &Fts5Query, cols: &[(&str, Vec<String>)], stem: bool) -> bool {
    match query {
        Fts5Query::Term(t) => fts5_term_matches(t, cols, stem),
        Fts5Query::Near(phrases, dist) => fts5_near_group_matches(phrases, *dist, cols, stem),
        Fts5Query::And(a, b) => fts5_eval(a, cols, stem) && fts5_eval(b, cols, stem),
        Fts5Query::Or(a, b) => fts5_eval(a, cols, stem) || fts5_eval(b, cols, stem),
        Fts5Query::Not(a, b) => fts5_eval(a, cols, stem) && !fts5_eval(b, cols, stem),
    }
}

/// Whether the in-scope columns satisfy the FTS5 query `pattern`. `cols` is the
/// `(name, text)` of each searchable column (one entry for a column-scoped
/// `col MATCH …`, every column for a table-wide `tbl MATCH …`). The query
/// supports bare tokens, `token*` prefixes, `"quoted phrases"`, `col:…` column
/// filters, and the boolean operators `AND` (explicit or implicit by
/// juxtaposition), `OR`, and `NOT` (binding tightest to loosest: `NOT`, `AND`,
/// `OR`) with parentheses — matching SQLite's default precedence — and the
/// `NEAR(p1 p2 …, n)` proximity group. A query with no tokens matches nothing.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_query_matches(pattern: &str, cols: &[(String, String)], stem: bool) -> bool {
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
        .map(|(name, text)| (name.as_str(), fts5_tokenize(text, stem)))
        .collect();
    fts5_eval(&query, &tokenized, stem)
}

/// Collect every phrase term of a parsed query (flattening the boolean tree),
/// because bm25 sums each phrase's contribution regardless of `AND`/`OR`/`NOT`.
#[cfg(feature = "fts5")]
fn fts5_collect_terms<'a>(q: &'a Fts5Query, out: &mut Vec<&'a Fts5Term>) {
    match q {
        Fts5Query::Term(t) => out.push(t),
        Fts5Query::Near(phrases, _) => out.extend(phrases.iter()),
        Fts5Query::And(a, b) | Fts5Query::Or(a, b) | Fts5Query::Not(a, b) => {
            fts5_collect_terms(a, out);
            fts5_collect_terms(b, out);
        }
    }
}

/// The FTS5 `bm25()` relevance score of every document for `query`, in input
/// order — byte-for-byte SQLite's default-weight `bm25()` (and `rank`). `docs[i]`
/// holds the text of each column of row `i`, parallel to `col_names`.
///
/// SQLite's Okapi BM25 with `k1 = 1.2`, `b = 0.75`: for each query phrase `p`,
/// `idf = ln((N − n_p + 0.5)/(n_p + 0.5))` clamped up to `1e-6` (so a term in most
/// rows still contributes a tiny positive weight), times
/// `freq·(k1+1)/(freq + k1·(1 − b + b·D/avgdl))`, where `freq` is the phrase's
/// occurrence count in the row (across its in-scope columns), `D` the row's total
/// token count, and `avgdl` the mean. The sum is **negated** so that the smallest
/// (most negative) score sorts first, exactly as `ORDER BY rank` expects.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_bm25_corpus(
    query: &str,
    col_names: &[String],
    docs: &[Vec<String>],
    scope: Option<&str>,
    indexed: Option<&[String]>,
    stem: bool,
) -> Fts5Bm25 {
    let n = docs.len();
    // A column is searchable unless it is declared `UNINDEXED`.
    let searchable = |ci: usize| -> bool {
        match (indexed, col_names.get(ci)) {
            (Some(cols), Some(name)) => cols.iter().any(|c| c.eq_ignore_ascii_case(name)),
            _ => true,
        }
    };
    let toks = fts5_lex(query);
    let parsed = (Fts5Parser {
        toks: &toks,
        pos: 0,
    })
    .parse();
    let terms: Vec<&Fts5Term> = match &parsed {
        Some(q) => {
            let mut t = Vec::new();
            fts5_collect_terms(q, &mut t);
            t
        }
        None => Vec::new(),
    };
    let nterms = terms.len();

    // Tokenize each column of each row once; the document length `D` is the total
    // token count across all columns (independent of any `col:` scoping).
    let tok_docs: Vec<Vec<Vec<String>>> = docs
        .iter()
        .map(|cols| cols.iter().map(|t| fts5_tokenize(t, stem)).collect())
        .collect();
    let dl: Vec<f64> = tok_docs
        .iter()
        .map(|cols| {
            cols.iter()
                .enumerate()
                .filter(|(ci, _)| searchable(*ci))
                .map(|(_, c)| c.len())
                .sum::<usize>() as f64
        })
        .collect();
    let avgdl = if n == 0 {
        0.0
    } else {
        dl.iter().sum::<f64>() / n as f64
    };

    // Per document: the occurrence count of each term in each column (already
    // scoped by the `col MATCH …` operand and any `col:` term filter), so a later
    // `bm25(t, w1, …)` call can apply arbitrary per-column weights.
    let mut occ: Vec<Vec<Vec<f64>>> =
        alloc::vec![alloc::vec![alloc::vec![0.0; col_names.len()]; nterms]; n];
    let mut idf = alloc::vec![0.0f64; nterms];
    for (t, term) in terms.iter().enumerate() {
        let mut docfreq = 0usize;
        for (i, cols) in tok_docs.iter().enumerate() {
            let mut any = false;
            for (ci, ctoks) in cols.iter().enumerate() {
                let name = col_names.get(ci);
                if !searchable(ci)
                    || scope.is_some_and(|s| name.is_none_or(|nm| !nm.eq_ignore_ascii_case(s)))
                    || term
                        .column
                        .as_deref()
                        .is_some_and(|c| name.is_none_or(|nm| !nm.eq_ignore_ascii_case(c)))
                {
                    continue;
                }
                let c = fts5_term_starts(term, ctoks, stem).len();
                if c > 0 {
                    occ[i][t][ci] = c as f64;
                    any = true;
                }
            }
            if any {
                docfreq += 1;
            }
        }
        let raw = crate::util::float::ln(((n - docfreq) as f64 + 0.5) / (docfreq as f64 + 0.5));
        idf[t] = if raw <= 0.0 { 1e-6 } else { raw };
    }

    Fts5Bm25 {
        avgdl,
        idf,
        docs: dl
            .into_iter()
            .zip(occ)
            .map(|(dl, occ)| Fts5Bm25Doc { dl, occ })
            .collect(),
    }
}

/// A precomputed bm25 corpus for one `MATCH` query: enough per-document and
/// global statistics to score any row with arbitrary per-column weights.
#[cfg(feature = "fts5")]
pub(crate) struct Fts5Bm25 {
    avgdl: f64,
    /// Inverse document frequency of each query term (already idf-clamped).
    idf: Vec<f64>,
    docs: Vec<Fts5Bm25Doc>,
}

/// One document's bm25 inputs: its length and per-term, per-column occurrences.
#[cfg(feature = "fts5")]
struct Fts5Bm25Doc {
    dl: f64,
    occ: Vec<Vec<f64>>,
}

#[cfg(feature = "fts5")]
impl Fts5Bm25 {
    /// SQLite's `bm25()` for document `i` with per-column `weights` (a missing or
    /// empty weight defaults to 1.0). The score is negated, so the most relevant
    /// row is the smallest — exactly what `ORDER BY rank` expects.
    pub(crate) fn score(&self, i: usize, weights: &[f64]) -> f64 {
        const K1: f64 = 1.2;
        const B: f64 = 0.75;
        let doc = &self.docs[i];
        let mut s = 0.0;
        for (t, occ_cols) in doc.occ.iter().enumerate() {
            let f: f64 = occ_cols
                .iter()
                .enumerate()
                .map(|(c, &o)| weights.get(c).copied().unwrap_or(1.0) * o)
                .sum();
            if f == 0.0 {
                continue;
            }
            let norm = 1.0 - B + B * doc.dl / self.avgdl;
            s += self.idf[t] * (f * (K1 + 1.0)) / (f + K1 * norm);
        }
        -s
    }
}

#[cfg(feature = "fts5")]
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

    /// Whether a column-declaration arg carries the `UNINDEXED` modifier (the
    /// column is stored and retrievable but excluded from the full-text index).
    #[cfg(feature = "fts5")]
    fn is_unindexed(arg: &str) -> bool {
        arg.split_whitespace()
            .skip(1)
            .any(|w| w.eq_ignore_ascii_case("UNINDEXED"))
    }
}

/// The names of the *searchable* (indexed) columns of an `fts5` table, given its
/// `USING fts5(…)` argument list — i.e. every declared column except those marked
/// `UNINDEXED`. A table-wide `MATCH` searches only these.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_indexed_columns(args: &[&str]) -> Vec<String> {
    args.iter()
        .filter_map(|a| {
            let name = Fts5Module::column_name(a)?;
            (!Fts5Module::is_unindexed(a)).then_some(name)
        })
        .collect()
}

/// Whether an `fts5` table uses the `porter` tokenizer (`tokenize = 'porter …'`),
/// in which case every token is Porter-stemmed. The first word of the tokenize
/// option is the tokenizer name; `porter` may wrap another tokenizer.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_uses_porter(args: &[&str]) -> bool {
    args.iter().any(|a| {
        a.split_once('=').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("tokenize")
                && v.trim()
                    .trim_matches(|c| c == '\'' || c == '"')
                    .split_whitespace()
                    .next()
                    .is_some_and(|t| t.eq_ignore_ascii_case("porter"))
        })
    })
}

#[cfg(feature = "fts5")]
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
    #[cfg(feature = "fts5")]
    fn fts5_porter_stem_matches_reference() {
        // Classic Porter-algorithm reference outputs.
        for (word, stem) in [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("caress", "caress"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("bled", "bled"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("hopping", "hop"),
            ("tanned", "tan"),
            ("falling", "fall"),
            ("hissing", "hiss"),
            ("fizzed", "fizz"),
            ("failing", "fail"),
            ("filing", "file"),
            ("happy", "happi"),
            ("sky", "sky"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("rational", "ration"),
            ("valenci", "valenc"),
            ("hesitanci", "hesit"),
            ("digitizer", "digit"),
            ("conformabli", "conform"),
            ("radicalli", "radic"),
            ("differentli", "differ"),
            ("vileli", "vile"),
            ("analogousli", "analog"),
            ("vietnamization", "vietnam"),
            ("predication", "predic"),
            ("operator", "oper"),
            ("feudalism", "feudal"),
            ("decisiveness", "decis"),
            ("hopefulness", "hope"),
            ("callousness", "callous"),
            ("formaliti", "formal"),
            ("sensitiviti", "sensit"),
            ("sensibiliti", "sensibl"),
            ("triplicate", "triplic"),
            ("formative", "form"),
            ("formalize", "formal"),
            ("electriciti", "electr"),
            ("electrical", "electr"),
            ("hopeful", "hope"),
            ("goodness", "good"),
            ("revival", "reviv"),
            ("allowance", "allow"),
            ("inference", "infer"),
            ("airliner", "airlin"),
            ("gyroscopic", "gyroscop"),
            ("adjustable", "adjust"),
            ("defensible", "defens"),
            ("irritant", "irrit"),
            ("replacement", "replac"),
            ("adjustment", "adjust"),
            ("dependent", "depend"),
            ("adoption", "adopt"),
            ("homologou", "homolog"),
            ("communism", "commun"),
            ("activate", "activ"),
            ("angulariti", "angular"),
            ("homologous", "homolog"),
            ("effective", "effect"),
            ("bowdlerize", "bowdler"),
            ("probate", "probat"),
            ("rate", "rate"),
            ("cease", "ceas"),
            ("controll", "control"),
            ("roll", "roll"),
        ] {
            assert_eq!(fts5_porter_stem(word), stem, "stemming {word}");
        }
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_tokenizer_splits_and_folds() {
        assert_eq!(
            fts5_tokenize("The quick-brown Fox!", false),
            vec![
                String::from("the"),
                String::from("quick"),
                String::from("brown"),
                String::from("fox"),
            ]
        );
        // Digits are tokens; runs of punctuation/whitespace are separators only.
        assert_eq!(
            fts5_tokenize("  a1  b2,c3 ", false),
            vec![String::from("a1"), String::from("b2"), String::from("c3")]
        );
        assert!(fts5_tokenize("   ,. !", false).is_empty());
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_query_matches_are_token_anded() {
        let doc = [(String::from("body"), String::from("the quick brown fox"))];
        assert!(fts5_query_matches("fox", &doc, false));
        assert!(fts5_query_matches("QUICK fox", &doc, false)); // case-insensitive AND
        assert!(!fts5_query_matches("quick zebra", &doc, false)); // one token missing
        assert!(!fts5_query_matches("", &doc, false)); // empty query matches nothing
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_column_filters_scope_tokens() {
        let cols = [
            (String::from("title"), String::from("Mixed Fox")),
            (String::from("body"), String::from("and the dog")),
        ];
        // A bare token matches in any column; `col:token` only in that column.
        assert!(fts5_query_matches("fox", &cols, false));
        assert!(fts5_query_matches("title:fox", &cols, false));
        assert!(!fts5_query_matches("body:fox", &cols, false)); // fox is in title, not body
        assert!(fts5_query_matches("title:mixed body:dog", &cols, false)); // AND across columns
        assert!(!fts5_query_matches("title:dog", &cols, false)); // dog is in body, not title
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_phrase_and_prefix_queries() {
        let doc = [(
            String::from("body"),
            String::from("the quick brown fox runs"),
        )];
        // A quoted phrase requires consecutive, ordered tokens.
        assert!(fts5_query_matches("\"quick brown\"", &doc, false));
        assert!(!fts5_query_matches("\"brown quick\"", &doc, false)); // wrong order
        assert!(!fts5_query_matches("\"quick fox\"", &doc, false)); // not adjacent
                                                                    // A `token*` prefix matches any token starting with it.
        assert!(fts5_query_matches("fo*", &doc, false)); // fox
        assert!(fts5_query_matches("run*", &doc, false)); // runs
        assert!(!fts5_query_matches("cat*", &doc, false));
        // Column-scoped phrase / prefix.
        assert!(fts5_query_matches("body:\"quick brown\"", &doc, false));
        assert!(fts5_query_matches("body:ru*", &doc, false));
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_boolean_operators_and_precedence() {
        let doc = |s: &str| [(String::from("body"), String::from(s))];
        // OR / AND / NOT.
        assert!(fts5_query_matches(
            "apple OR cherry",
            &doc("apple banana"),
            false
        ));
        assert!(!fts5_query_matches(
            "apple AND date",
            &doc("apple banana"),
            false
        ));
        assert!(fts5_query_matches(
            "apple AND date",
            &doc("apple date"),
            false
        ));
        assert!(fts5_query_matches(
            "banana NOT cherry",
            &doc("apple banana"),
            false
        ));
        assert!(!fts5_query_matches(
            "banana NOT cherry",
            &doc("banana cherry"),
            false
        ));
        // AND binds tighter than OR: `apple OR banana AND cherry`.
        assert!(fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("apple only"),
            false
        ));
        assert!(fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("banana cherry"),
            false
        ));
        assert!(!fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("banana only"),
            false
        ));
        // Parentheses override precedence.
        assert!(fts5_query_matches(
            "(apple OR banana) AND date",
            &doc("apple date"),
            false
        ));
        assert!(!fts5_query_matches(
            "(apple OR banana) AND date",
            &doc("apple only"),
            false
        ));
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_near_proximity_groups() {
        let doc = |s: &str| [(String::from("body"), String::from(s))];
        let adjacent = doc("the quick brown fox");
        let gap2 = doc("quick the lazy brown");
        let gap4 = doc("brown a b c d quick");
        // Default distance (10) catches all; tighter distances exclude wider gaps.
        assert!(fts5_query_matches("NEAR(quick brown)", &adjacent, false));
        assert!(fts5_query_matches("NEAR(quick brown)", &gap4, false));
        assert!(fts5_query_matches("NEAR(quick brown, 2)", &gap2, false));
        assert!(!fts5_query_matches("NEAR(quick brown, 1)", &gap2, false));
        assert!(fts5_query_matches("NEAR(quick brown, 0)", &adjacent, false));
        assert!(!fts5_query_matches("NEAR(quick brown, 0)", &gap2, false));
        // A missing phrase never matches; NEAR composes with the boolean operators.
        assert!(!fts5_query_matches(
            "NEAR(quick zebra, 5)",
            &adjacent,
            false
        ));
        assert!(fts5_query_matches(
            "NEAR(quick brown, 2) AND fox",
            &adjacent,
            false
        ));
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_bm25_matches_sqlite() {
        let names = [String::from("body")];
        let doc = |s: &str| alloc::vec![String::from(s)];
        let docs = [
            doc("apple apple banana"),
            doc("apple cherry"),
            doc("banana date elderberry"),
            doc("fig grape"),
            doc("apple banana cherry date"),
        ];
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let score =
            |q: &str, i: usize| fts5_bm25_corpus(q, &names, &docs, None, None, false).score(i, &[]);

        // A common term (idf clamped to 1e-6): the exact values sqlite3 returns.
        assert!(close(score("apple", 0), -1.347_921_225_382_93e-6));
        assert!(close(score("apple", 1), -1.132_352_941_176_47e-6));
        assert!(close(score("apple", 4), -8.508_287_292_817_68e-7));
        assert_eq!(score("apple", 3), 0.0); // a row without the term scores 0

        // A rare term keeps a real (un-clamped) idf.
        assert!(close(score("elderberry", 2), -1.067_421_403_500_88));

        // Two AND-ed terms sum their contributions.
        assert!(close(score("apple banana", 0), -2.319_530_058_190_5e-6));
        assert!(close(score("apple banana", 4), -1.701_657_458_563_54e-6));

        // A per-column weight scales the effective term frequency, which sits in
        // both the numerator and denominator — so a heavier weight gives a larger
        // magnitude, but not a linear multiple of the unweighted score.
        let corpus = fts5_bm25_corpus("apple", &names, &docs, None, None, false);
        assert!(corpus.score(0, &[10.0]) < corpus.score(0, &[]));
        assert_eq!(corpus.score(3, &[10.0]), 0.0); // still 0 where the term is absent
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_highlight_wraps_matched_tokens() {
        let names = [String::from("body")];
        let hl = |q: &str, text: &str| fts5_highlight(q, &names, None, 0, text, false, "[", "]");
        // A single matched token, preserving the surrounding text.
        assert_eq!(
            hl("fox", "the quick brown fox jumps"),
            "the quick brown [fox] jumps"
        );
        // Two non-adjacent matches get their own markers.
        assert_eq!(
            hl("quick dog", "the quick brown fox and the lazy dog"),
            "the [quick] brown fox and the lazy [dog]"
        );
        // A matched phrase (adjacent tokens) shares one pair of markers.
        assert_eq!(
            hl("\"quick brown\"", "a quick brown fox"),
            "a [quick brown] fox"
        );
        // Two separate single-token matches are wrapped separately (not merged).
        assert_eq!(hl("fox", "fox fox"), "[fox] [fox]");
        // Case-insensitive matching, case-preserving output.
        assert_eq!(hl("hello", "Hello World"), "[Hello] World");
        // No match leaves the text untouched.
        assert_eq!(hl("zebra", "the quick fox"), "the quick fox");
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_anchor_requires_first_token() {
        let doc = |s: &str| [(String::from("body"), String::from(s))];
        // `^token` matches only when the token is at the start of the column.
        assert!(fts5_query_matches("^quick", &doc("quick brown fox"), false));
        assert!(!fts5_query_matches("^quick", &doc("the quick fox"), false));
        // Anchored phrases too; an unanchored token still matches anywhere.
        assert!(fts5_query_matches(
            "^\"quick brown\"",
            &doc("quick brown fox"),
            false
        ));
        assert!(!fts5_query_matches(
            "^\"quick brown\"",
            &doc("a quick brown"),
            false
        ));
        assert!(fts5_query_matches("quick", &doc("the quick fox"), false));
    }

    #[test]
    #[cfg(feature = "fts5")]
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
