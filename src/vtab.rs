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
        #[cfg(feature = "fts5")]
        reg.register("fts5vocab", Box::new(Fts5VocabModule))
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
pub(crate) fn coord_f64(v: &Value) -> f64 {
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
pub(crate) fn round_min_f32(d: f64) -> f64 {
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
pub(crate) fn round_max_f32(d: f64) -> f64 {
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
    pub(crate) fn n_coords(args: &[&str]) -> usize {
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

#[cfg(feature = "fts5")]
/// Fold a precomposed accented Latin letter to its ASCII base, matching SQLite's
/// `unicode61` default tokenizer (`remove_diacritics=1`), which strips combining
/// marks (`café`→`cafe`, `résumé`→`resume`, `über`→`uber`, `Việt`→`viet`). The
/// table covers the Latin-1 Supplement, Latin Extended-A, the accented part of
/// Latin Extended-B, and Latin Extended Additional (U+1E00–U+1EFF). It is derived
/// byte-exactly from `sqlite3` 3.50.4 via `fts5vocab` — every entry is a codepoint
/// that the C tokenizer folds to a single ASCII letter.
///
/// Distinct letters that are NOT merely an accented base — `Æ`/`æ`, `Ø`/`ø`, `ß`,
/// `Þ`/`þ`, `Ð`/`ð`, `Ł`/`ł`, `Đ`/`đ`, `Œ`/`œ`, … — and the double-accented chars
/// `remove_diacritics=1` leaves alone (e.g. `Ḉ`→`ḉ`) are returned unchanged; their
/// case folding is then handled by `char::to_lowercase`, which matches sqlite. The
/// non-letter `×`/`÷` are not token characters in either engine. Without this fold
/// an accented token diverges from `unicode61` and a graphite-written FTS5 index
/// reads as "malformed inverted index" to sqlite.
#[cfg(feature = "fts5")]
pub(crate) fn fold_diacritic(ch: char) -> char {
    match ch {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'Ā' | 'ā' | 'Ă'
        | 'ă' | 'Ą' | 'ą' | 'Ǎ' | 'ǎ' | 'Ȁ' | 'ȁ' | 'Ȃ' | 'ȃ' | 'Ȧ' | 'ȧ' | 'Ḁ' | 'ḁ' | 'Ạ'
        | 'ạ' | 'Ả' | 'ả' => 'a',
        'Ḃ' | 'ḃ' | 'Ḅ' | 'ḅ' | 'Ḇ' | 'ḇ' => 'b',
        'Ç' | 'ç' | 'Ć' | 'ć' | 'Ĉ' | 'ĉ' | 'Ċ' | 'ċ' | 'Č' | 'č' => 'c',
        'Ď' | 'ď' | 'Ḋ' | 'ḋ' | 'Ḍ' | 'ḍ' | 'Ḏ' | 'ḏ' | 'Ḑ' | 'ḑ' | 'Ḓ' | 'ḓ' => {
            'd'
        }
        'È' | 'É' | 'Ê' | 'Ë' | 'è' | 'é' | 'ê' | 'ë' | 'Ē' | 'ē' | 'Ĕ' | 'ĕ' | 'Ė' | 'ė' | 'Ę'
        | 'ę' | 'Ě' | 'ě' | 'Ȅ' | 'ȅ' | 'Ȇ' | 'ȇ' | 'Ȩ' | 'ȩ' | 'Ḙ' | 'ḙ' | 'Ḛ' | 'ḛ' | 'Ẹ'
        | 'ẹ' | 'Ẻ' | 'ẻ' | 'Ẽ' | 'ẽ' => 'e',
        'Ḟ' | 'ḟ' => 'f',
        'Ĝ' | 'ĝ' | 'Ğ' | 'ğ' | 'Ġ' | 'ġ' | 'Ģ' | 'ģ' | 'Ǧ' | 'ǧ' | 'Ǵ' | 'ǵ' | 'Ḡ' | 'ḡ' => {
            'g'
        }
        'Ĥ' | 'ĥ' | 'Ȟ' | 'ȟ' | 'Ḣ' | 'ḣ' | 'Ḥ' | 'ḥ' | 'Ḧ' | 'ḧ' | 'Ḩ' | 'ḩ' | 'Ḫ' | 'ḫ' | 'ẖ' => {
            'h'
        }
        'Ì' | 'Í' | 'Î' | 'Ï' | 'ì' | 'í' | 'î' | 'ï' | 'Ĩ' | 'ĩ' | 'Ī' | 'ī' | 'Ĭ' | 'ĭ' | 'Į'
        | 'į' | 'İ' | 'Ǐ' | 'ǐ' | 'Ȉ' | 'ȉ' | 'Ȋ' | 'ȋ' | 'Ḭ' | 'ḭ' | 'Ỉ' | 'ỉ' | 'Ị' | 'ị' => {
            'i'
        }
        'Ĵ' | 'ĵ' | 'ǰ' => 'j',
        'Ķ' | 'ķ' | 'Ǩ' | 'ǩ' | 'Ḱ' | 'ḱ' | 'Ḳ' | 'ḳ' | 'Ḵ' | 'ḵ' => 'k',
        'Ĺ' | 'ĺ' | 'Ļ' | 'ļ' | 'Ľ' | 'ľ' | 'Ḷ' | 'ḷ' | 'Ḻ' | 'ḻ' | 'Ḽ' | 'ḽ' => {
            'l'
        }
        'Ḿ' | 'ḿ' | 'Ṁ' | 'ṁ' | 'Ṃ' | 'ṃ' => 'm',
        'Ñ' | 'ñ' | 'Ń' | 'ń' | 'Ņ' | 'ņ' | 'Ň' | 'ň' | 'Ǹ' | 'ǹ' | 'Ṅ' | 'ṅ' | 'Ṇ' | 'ṇ' | 'Ṉ'
        | 'ṉ' | 'Ṋ' | 'ṋ' => 'n',
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'Ō' | 'ō' | 'Ŏ' | 'ŏ' | 'Ő'
        | 'ő' | 'Ơ' | 'ơ' | 'Ǒ' | 'ǒ' | 'Ǫ' | 'ǫ' | 'Ȍ' | 'ȍ' | 'Ȏ' | 'ȏ' | 'Ȯ' | 'ȯ' | 'Ọ'
        | 'ọ' | 'Ỏ' | 'ỏ' => 'o',
        'Ṕ' | 'ṕ' | 'Ṗ' | 'ṗ' => 'p',
        'Ŕ' | 'ŕ' | 'Ŗ' | 'ŗ' | 'Ř' | 'ř' | 'Ȑ' | 'ȑ' | 'Ȓ' | 'ȓ' | 'Ṙ' | 'ṙ' | 'Ṛ' | 'ṛ' | 'Ṟ'
        | 'ṟ' => 'r',
        'Ś' | 'ś' | 'Ŝ' | 'ŝ' | 'Ş' | 'ş' | 'Š' | 'š' | 'ſ' | 'Ș' | 'ș' | 'Ṡ' | 'ṡ' | 'Ṣ' | 'ṣ'
        | 'ẛ' => 's',
        'Ţ' | 'ţ' | 'Ť' | 'ť' | 'Ț' | 'ț' | 'Ṫ' | 'ṫ' | 'Ṭ' | 'ṭ' | 'Ṯ' | 'ṯ' | 'Ṱ' | 'ṱ' | 'ẗ' => {
            't'
        }
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'ù' | 'ú' | 'û' | 'ü' | 'Ũ' | 'ũ' | 'Ū' | 'ū' | 'Ŭ' | 'ŭ' | 'Ů'
        | 'ů' | 'Ű' | 'ű' | 'Ų' | 'ų' | 'Ư' | 'ư' | 'Ǔ' | 'ǔ' | 'Ȕ' | 'ȕ' | 'Ȗ' | 'ȗ' | 'Ṳ'
        | 'ṳ' | 'Ṵ' | 'ṵ' | 'Ṷ' | 'ṷ' | 'Ụ' | 'ụ' | 'Ủ' | 'ủ' => 'u',
        'Ṽ' | 'ṽ' | 'Ṿ' | 'ṿ' => 'v',
        'Ŵ' | 'ŵ' | 'Ẁ' | 'ẁ' | 'Ẃ' | 'ẃ' | 'Ẅ' | 'ẅ' | 'Ẇ' | 'ẇ' | 'Ẉ' | 'ẉ' | 'ẘ' => {
            'w'
        }
        'Ẋ' | 'ẋ' | 'Ẍ' | 'ẍ' => 'x',
        'Ý' | 'ý' | 'ÿ' | 'Ŷ' | 'ŷ' | 'Ÿ' | 'Ȳ' | 'ȳ' | 'Ẏ' | 'ẏ' | 'ẙ' | 'Ỳ' | 'ỳ' | 'Ỵ' | 'ỵ'
        | 'Ỷ' | 'ỷ' | 'Ỹ' | 'ỹ' => 'y',
        'Ź' | 'ź' | 'Ż' | 'ż' | 'Ž' | 'ž' | 'Ẑ' | 'ẑ' | 'Ẓ' | 'ẓ' | 'Ẕ' | 'ẕ' => {
            'z'
        }
        other => other,
    }
}

/// The extra precomposed letters that `unicode61 remove_diacritics 2` folds to an
/// ASCII base but the default level 1 keeps verbatim — chiefly the double-accented
/// Vietnamese vowels (`ệ`→`e`, `ố`→`o`, `ự`→`u`) and a few combining-mark stacks
/// (`Ḉ`→`c`, `Ḯ`→`i`). Derived byte-exactly from `sqlite3` 3.50.4 by diffing the
/// `fts5vocab` term at `remove_diacritics` 1 vs 2 over U+00C0–U+1EFF. Disjoint from
/// [`fold_diacritic`]; the stroke letters (`ł`, `đ`, …) stay kept even at level 2.
#[cfg(feature = "fts5")]
fn fold_diacritic2(ch: char) -> char {
    match ch {
        'Ǟ' | 'ǟ' | 'Ǻ' | 'ǻ' | 'Ấ' | 'ấ' | 'Ầ' | 'ầ' | 'Ẩ' | 'ẩ' | 'Ẫ' | 'ẫ' | 'Ậ' | 'ậ' | 'Ắ'
        | 'ắ' | 'Ằ' | 'ằ' | 'Ẳ' | 'ẳ' | 'Ẵ' | 'ẵ' | 'Ặ' | 'ặ' => 'a',
        'Ḉ' | 'ḉ' => 'c',
        'Ḕ' | 'ḕ' | 'Ḗ' | 'ḗ' | 'Ḝ' | 'ḝ' | 'Ế' | 'ế' | 'Ề' | 'ề' | 'Ể' | 'ể' | 'Ễ' | 'ễ' | 'Ệ'
        | 'ệ' => 'e',
        'Ḯ' | 'ḯ' => 'i',
        'Ḹ' | 'ḹ' => 'l',
        'Ǭ' | 'ǭ' | 'Ȫ' | 'ȫ' | 'Ȭ' | 'ȭ' | 'Ȱ' | 'ȱ' | 'Ṍ' | 'ṍ' | 'Ṏ' | 'ṏ' | 'Ṑ' | 'ṑ' | 'Ṓ'
        | 'ṓ' | 'Ố' | 'ố' | 'Ồ' | 'ồ' | 'Ổ' | 'ổ' | 'Ỗ' | 'ỗ' | 'Ộ' | 'ộ' | 'Ớ' | 'ớ' | 'Ờ'
        | 'ờ' | 'Ở' | 'ở' | 'Ỡ' | 'ỡ' | 'Ợ' | 'ợ' => 'o',
        'Ṝ' | 'ṝ' => 'r',
        'Ṥ' | 'ṥ' | 'Ṧ' | 'ṧ' | 'Ṩ' | 'ṩ' => 's',
        'Ǖ' | 'ǖ' | 'Ǘ' | 'ǘ' | 'Ǚ' | 'ǚ' | 'Ǜ' | 'ǜ' | 'Ṹ' | 'ṹ' | 'Ṻ' | 'ṻ' | 'Ứ' | 'ứ' | 'Ừ'
        | 'ừ' | 'Ử' | 'ử' | 'Ữ' | 'ữ' | 'Ự' | 'ự' => 'u',
        other => other,
    }
}

/// A resolved FTS5 tokenizer configuration, parsed once from the table's
/// `CREATE VIRTUAL TABLE` args by `fts5_tok_config` and threaded through every
/// tokenization (document indexing, query parsing, `MATCH`, `highlight`,
/// `snippet`) so the doc and query sides always fold identically.
#[cfg(feature = "fts5")]
#[derive(Clone, Copy)]
pub struct Fts5Tok {
    /// The `porter` tokenizer wraps another tokenizer and Porter-stems each token.
    pub stem: bool,
    /// `unicode61`'s `remove_diacritics` level: 0 (keep all), 1 (default), 2.
    pub diacritics: u8,
    /// `tokenchars '…'`: extra ASCII characters that count as part of a token even
    /// though they aren't alphanumeric. A bitmap over codepoints 0..128 (bit `c` =
    /// char `c`). Non-ASCII `tokenchars` are not represented (rare in practice).
    pub tokenchars: u128,
    /// `separators '…'`: ASCII characters that split tokens even though they are
    /// alphanumeric. Same bitmap layout; takes precedence over `tokenchars`.
    pub separators: u128,
}

#[cfg(feature = "fts5")]
impl Default for Fts5Tok {
    fn default() -> Self {
        // The `unicode61` default: no stemming, `remove_diacritics=1`, no custom
        // token/separator characters.
        Fts5Tok {
            stem: false,
            diacritics: 1,
            tokenchars: 0,
            separators: 0,
        }
    }
}

#[cfg(feature = "fts5")]
impl Fts5Tok {
    /// Whether `ch` is part of a token. `separators` win over `tokenchars`, which
    /// win over the default classification (`unicode61`: any folded alphanumeric).
    fn is_token_char(&self, ch: char) -> bool {
        let bit = |map: u128, ch: char| (ch as u32) < 128 && (map >> (ch as u32)) & 1 == 1;
        if bit(self.separators, ch) {
            false
        } else if bit(self.tokenchars, ch) {
            true
        } else {
            fold_for(ch, self.diacritics).is_alphanumeric()
        }
    }
}

/// Fold `ch` for diacritics according to the tokenizer's `remove_diacritics`
/// level: 0 leaves it unchanged, 1 applies [`fold_diacritic`], 2 additionally
/// applies [`fold_diacritic2`] (the disjoint extra table).
#[cfg(feature = "fts5")]
fn fold_for(ch: char, level: u8) -> char {
    match level {
        0 => ch,
        1 => fold_diacritic(ch),
        _ => {
            let f = fold_diacritic(ch);
            if f != ch {
                f
            } else {
                fold_diacritic2(ch)
            }
        }
    }
}

/// Split `text` into lowercased tokens on every non-alphanumeric character,
/// folding accents per `tok.diacritics` (`fold_for`) like `unicode61`. With
/// `tok.stem`, each token is Porter-stemmed (the `porter` tokenizer).
#[cfg(feature = "fts5")]
pub(crate) fn fts5_tokenize(text: &str, tok: Fts5Tok) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if tok.is_token_char(ch) {
            cur.extend(fold_for(ch, tok.diacritics).to_lowercase());
        } else if !cur.is_empty() {
            let t = core::mem::take(&mut cur);
            tokens.push(if tok.stem { fts5_porter_stem(&t) } else { t });
        }
    }
    if !cur.is_empty() {
        tokens.push(if tok.stem {
            fts5_porter_stem(&cur)
        } else {
            cur
        });
    }
    tokens
}

/// Like [`fts5_tokenize`], but also returns each token's `[start, end)` byte range
/// in the original `text` — so [`fts5_highlight`] can wrap matched tokens while
/// preserving the surrounding original characters. The span is over the original
/// text even when the token itself is Porter-stemmed.
#[cfg(feature = "fts5")]
fn fts5_tokenize_spans(text: &str, tok: Fts5Tok) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    let push = |cur: &mut String, start: usize, end: usize, out: &mut Vec<_>| {
        let t = core::mem::take(cur);
        out.push((if tok.stem { fts5_porter_stem(&t) } else { t }, start, end));
    };
    for (i, ch) in text.char_indices() {
        // Classify on the ORIGINAL char (so `tokenchars`/`separators` match), then
        // fold accents for the token content like `fts5_tokenize`; the span
        // [start, i) stays over the ORIGINAL bytes.
        if tok.is_token_char(ch) {
            if cur.is_empty() {
                start = i;
            }
            cur.extend(fold_for(ch, tok.diacritics).to_lowercase());
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
    tok: Fts5Tok,
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
    let toks = fts5_lex(query, tok);
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

    let spans = fts5_tokenize_spans(text, tok);
    let col_tokens: Vec<String> = spans.iter().map(|(t, _, _)| t.clone()).collect();
    // Each phrase *instance* is one highlight span `[start, end)` (token indices);
    // SQLite wraps each separately, so two adjacent single-token matches become
    // `[fox] [fox]`, while a matched two-word phrase is one `[quick brown]`.
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for term in &terms {
        // Skip a term whose column filter excludes this column.
        if col_names.get(col).is_none_or(|n| !term.admits_column(n)) {
            continue;
        }
        for start in fts5_term_starts(term, &col_tokens, tok) {
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
    tok: Fts5Tok,
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
    let lexed = fts5_lex(query, tok);
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
        let spans = fts5_tokenize_spans(text, tok);
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
                if col_names.get(ci).is_none_or(|nm| !term.admits_column(nm)) {
                    continue;
                }
                for start in fts5_term_starts(term, &col_tokens, tok) {
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

/// A single FTS5 column-set filter (`col:`, `{c0 c1 …}:`, or their negated
/// `-col:` / `-{…}:` forms). It restricts the phrase it prefixes to a set of
/// columns (or, when `negated`, to the complement of that set). A term admits a
/// column `c` iff `names` contains `c` XOR `negated` — so `{a b}:` admits `a`/`b`
/// and `-{a}:` admits every column except `a`. Comparison is case-insensitive.
#[derive(Clone)]
#[cfg(feature = "fts5")]
struct Fts5ColSet {
    /// The listed column names (raw; compared with `eq_ignore_ascii_case`).
    names: Vec<String>,
    /// Whether the filter selects the COMPLEMENT of `names` (the `-{…}:`/`-col:`
    /// form) rather than the set itself.
    negated: bool,
}

#[cfg(feature = "fts5")]
impl Fts5ColSet {
    /// Whether this filter admits the column named `col`.
    fn admits(&self, col: &str) -> bool {
        self.names.iter().any(|n| n.eq_ignore_ascii_case(col)) != self.negated
    }
}

/// One term of an FTS5 query: a phrase of one or more consecutive tokens,
/// optionally restricted to a set of columns (`col:phrase`, `{c0 c1}:phrase`,
/// and their negated forms), anchored to the start of the column (`^token`),
/// and/or ending in a prefix token (`token*`).
#[derive(Clone)]
#[cfg(feature = "fts5")]
struct Fts5Term {
    /// The column-set filters constraining the term. Empty means "any column";
    /// each filter must admit a column for the term to be searched there, so
    /// nested filters (`{a b}:(c:x)`) intersect by appending. Built from the
    /// `col:` / `{…}:` / `-…:` prefixes.
    columns: Vec<Fts5ColSet>,
    /// The tokens that must appear consecutively and in order. A bare token is a
    /// one-element phrase; `"quick brown"` is a two-element phrase.
    phrase: Vec<String>,
    /// Whether the *last* token of the phrase is a prefix match (`token*`).
    prefix: bool,
    /// Whether the phrase is anchored to the first token of the column (`^token`).
    anchored: bool,
}

#[cfg(feature = "fts5")]
impl Fts5Term {
    /// Whether the term is allowed to match in the column named `col` (every
    /// column-set filter must admit it; an unfiltered term admits every column).
    fn admits_column(&self, col: &str) -> bool {
        self.columns.iter().all(|cs| cs.admits(col))
    }

    /// If the term is filtered to exactly ONE positive column (a lone,
    /// non-negated single-name `col:` filter), its name — the only shape the
    /// segment reader routes as a column-scoped lookup. Any braced set, negated
    /// filter, or nested/intersected filter yields `None`.
    fn single_column(&self) -> Option<&str> {
        match self.columns.as_slice() {
            [cs] if !cs.negated => match cs.names.as_slice() {
                [name] => Some(name.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether the term carries no column filter at all (matches any column).
    fn any_column(&self) -> bool {
        self.columns.is_empty()
    }
}

/// The start offsets at which `term` matches in a column's `tokens`, honoring an
/// `^` anchor (which keeps only a match at offset 0).
#[cfg(feature = "fts5")]
fn fts5_term_starts(term: &Fts5Term, tokens: &[String], tok: Fts5Tok) -> Vec<usize> {
    // Under the `porter` tokenizer the document tokens are stemmed, so the query
    // phrase must be stemmed the same way to match (including the prefix token —
    // SQLite runs query tokens through the tokenizer too).
    let phrase: Vec<String> = if tok.stem {
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

/// A lexed token of an FTS5 query: a boolean operator, a parenthesis, a term, a
/// `NEAR(phrase … , n)` group (its phrases and distance, default 10), a
/// column-set filter (`{c0 c1}:` / `-{…}:` / `-col:`) that binds to the following
/// primary, or a lex-time syntax error.
#[cfg(feature = "fts5")]
enum Fts5Lex {
    Or,
    And,
    Not,
    LParen,
    RParen,
    Term(Fts5Term),
    Near(Vec<Fts5Term>, usize),
    /// A braced/negated column-set filter prefix; binds to the next primary in
    /// the parser (`{a b}:phrase`, `-{a}:phrase`, `-col:phrase`).
    ColFilter(Fts5ColSet),
    /// A lex-time syntax error (e.g. an empty `{}:` or an unterminated brace),
    /// mirroring SQLite's `fts5: syntax error`. Surfaced by `fts5_query_matches`.
    Error(String),
}

/// Lex an FTS5 query string into operators, parentheses, and terms. `OR`/`AND`/
/// `NOT` are operators only as bare uppercase words (a lowercase `and` or a
/// `col:and` is an ordinary token, as in SQLite). A term is `[column:]body`,
/// where `body` is a `"quoted phrase"` or a bare word optionally ending in `*`.
#[cfg(feature = "fts5")]
fn fts5_lex(pattern: &str, tok: Fts5Tok) -> Vec<Fts5Lex> {
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
        // A column-set filter that binds to the following primary:
        //   `{c0 c1 …}:`  — restrict to the listed columns;
        //   `-{c0 c1 …}:` — restrict to the COMPLEMENT of the listed columns;
        //   `-col:`       — the negated single-column form.
        // The leading `-` negates; a `{` opens a whitespace-separated name list,
        // and a bare identifier (no brace) is the single-column form. In every
        // case a `:` must follow the (closing brace of the) set.
        {
            let mut p = i;
            let negated = chars[p] == '-';
            if negated {
                p += 1;
                while p < n && chars[p].is_whitespace() {
                    p += 1;
                }
            }
            if p < n && chars[p] == '{' {
                p += 1;
                let mut names = Vec::new();
                loop {
                    while p < n && chars[p].is_whitespace() {
                        p += 1;
                    }
                    if p < n && chars[p] == '}' {
                        p += 1;
                        break;
                    }
                    let start = p;
                    while p < n
                        && (chars[p].is_alphanumeric() || chars[p] == '_' || chars[p] == '.')
                    {
                        p += 1;
                    }
                    if p == start {
                        // Neither a name nor the closing brace: `{}:` or `{a,b}` etc.
                        out.push(Fts5Lex::Error(alloc::format!(
                            "fts5: syntax error near \"{}\"",
                            chars.get(p).copied().unwrap_or('}')
                        )));
                        return out;
                    }
                    names.push(chars[start..p].iter().collect::<String>());
                }
                // A `:` (optionally after whitespace) must follow the `}`.
                let mut q = p;
                while q < n && chars[q].is_whitespace() {
                    q += 1;
                }
                if q < n && chars[q] == ':' {
                    i = q + 1;
                    while i < n && chars[i].is_whitespace() {
                        i += 1;
                    }
                    if names.is_empty() {
                        out.push(Fts5Lex::Error(String::from(
                            "fts5: syntax error near \"}\"",
                        )));
                        return out;
                    }
                    // A column filter must bind to a phrase or `(`, not another
                    // brace — SQLite rejects a chained `{a}:{b}:x` (`near "{"`).
                    if i < n && chars[i] == '{' {
                        out.push(Fts5Lex::Error(String::from(
                            "fts5: syntax error near \"{\"",
                        )));
                        return out;
                    }
                    out.push(Fts5Lex::ColFilter(Fts5ColSet { names, negated }));
                    continue;
                }
                // A `{…}` not followed by `:` is a syntax error; SQLite names the
                // next token (a run of identifier chars, else the single character).
                let mut e = q;
                while e < n && (chars[e].is_alphanumeric() || chars[e] == '_') {
                    e += 1;
                }
                let near: String = if e > q {
                    chars[q..e].iter().collect()
                } else {
                    chars.get(q).copied().map(String::from).unwrap_or_default()
                };
                out.push(Fts5Lex::Error(alloc::format!(
                    "fts5: syntax error near \"{near}\""
                )));
                return out;
            }
            // A leading `-` that is NOT a braced set: the negated single-column
            // form `-col:`. Only treat it as a filter when a `col:` prefix follows;
            // otherwise fall through (a bare `-` is an ordinary token char).
            if negated {
                let mut e = p;
                while e < n && (chars[e].is_alphanumeric() || chars[e] == '_') {
                    e += 1;
                }
                let mut c = e;
                while c < n && chars[c].is_whitespace() {
                    c += 1;
                }
                if e > p && c < n && chars[c] == ':' {
                    let name: String = chars[p..e].iter().collect();
                    i = c + 1;
                    while i < n && chars[i].is_whitespace() {
                        i += 1;
                    }
                    out.push(Fts5Lex::ColFilter(Fts5ColSet {
                        names: alloc::vec![name],
                        negated: true,
                    }));
                    continue;
                }
            }
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
            // `col:` must bind to a phrase, not a brace — SQLite rejects the
            // chained `a:{b}:x` shape (`near "{"`).
            if i < n && chars[i] == '{' {
                out.push(Fts5Lex::Error(String::from(
                    "fts5: syntax error near \"{\"",
                )));
                return out;
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
                            let (phrases, dist) = fts5_parse_near(&inside, tok);
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
        // Diacritics ARE folded at the table's level so the query matches the docs.
        let phrase = fts5_tokenize(&text, Fts5Tok { stem: false, ..tok });
        if !phrase.is_empty() {
            // An inline `col:` prefix is the single-column filter; the braced/negated
            // forms come through as `ColFilter` tokens applied by the parser.
            let columns = match column {
                Some(name) => alloc::vec![Fts5ColSet {
                    names: alloc::vec![name],
                    negated: false,
                }],
                None => Vec::new(),
            };
            out.push(Fts5Lex::Term(Fts5Term {
                columns,
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
fn fts5_parse_near(inside: &str, tok: Fts5Tok) -> (Vec<Fts5Term>, usize) {
    let (phrases_part, distance) = match inside.rsplit_once(',') {
        Some((left, right))
            if !right.trim().is_empty() && right.trim().bytes().all(|b| b.is_ascii_digit()) =>
        {
            (left, right.trim().parse::<usize>().unwrap_or(10))
        }
        _ => (inside, 10),
    };
    let phrases = fts5_lex(phrases_part, tok)
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
                // Juxtaposition (a term, NEAR group, column filter, or `(`) is an
                // implicit AND.
                Some(
                    Fts5Lex::Term(_) | Fts5Lex::Near(..) | Fts5Lex::LParen | Fts5Lex::ColFilter(_),
                ) => {}
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
            // A column-set filter (`{a b}:`, `-{a}:`, `-col:`) binds to the
            // immediately-following primary; the filter is pushed down onto every
            // term/phrase within it (nested filters intersect by appending).
            Some(Fts5Lex::ColFilter(cs)) => {
                let cs = cs.clone();
                self.pos += 1;
                let mut inner = self.parse_primary()?;
                fts5_apply_col_filter(&mut inner, &cs);
                Some(inner)
            }
            _ => None,
        }
    }
}

/// Push a column-set filter down onto every term and NEAR phrase in `q`,
/// intersecting with any filter already present (nested `{a b}:(c:x)` — the
/// inner `c:` and the outer `{a b}:` must both admit a column). This is how a
/// braced/negated filter that prefixes a parenthesised sub-expression restricts
/// all of its terms.
#[cfg(feature = "fts5")]
fn fts5_apply_col_filter(q: &mut Fts5Query, cs: &Fts5ColSet) {
    match q {
        Fts5Query::Term(t) => t.columns.push(cs.clone()),
        Fts5Query::Near(phrases, _) => {
            for p in phrases {
                p.columns.push(cs.clone());
            }
        }
        Fts5Query::And(a, b) | Fts5Query::Or(a, b) | Fts5Query::Not(a, b) => {
            fts5_apply_col_filter(a, cs);
            fts5_apply_col_filter(b, cs);
        }
    }
}

/// Whether a single term matches any in-scope column (respecting the term's
/// `col:`/`{…}:` column-set filter and the `^` anchor).
#[cfg(feature = "fts5")]
fn fts5_term_matches(term: &Fts5Term, cols: &[(&str, Vec<String>)], tok: Fts5Tok) -> bool {
    cols.iter().any(|(name, tokens)| {
        term.admits_column(name) && !fts5_term_starts(term, tokens, tok).is_empty()
    })
}

/// Whether a `NEAR` group is satisfied: some single in-scope column contains all
/// of its phrases within the distance window.
#[cfg(feature = "fts5")]
fn fts5_near_group_matches(
    phrases: &[Fts5Term],
    dist: usize,
    cols: &[(&str, Vec<String>)],
    tok: Fts5Tok,
) -> bool {
    cols.iter().any(|(name, tokens)| {
        // A column filter on the group (`{a}:NEAR(…)`, distributed to each phrase)
        // restricts the whole proximity search to the admitted columns.
        if !phrases.iter().all(|p| p.admits_column(name)) {
            return false;
        }
        let positioned: Vec<(Vec<usize>, usize)> = phrases
            .iter()
            .map(|p| (fts5_term_starts(p, tokens, tok), p.phrase.len()))
            .collect();
        fts5_near_matches(&positioned, dist)
    })
}

/// Evaluate a parsed query tree against the tokenized in-scope columns.
#[cfg(feature = "fts5")]
fn fts5_eval(query: &Fts5Query, cols: &[(&str, Vec<String>)], tok: Fts5Tok) -> bool {
    match query {
        Fts5Query::Term(t) => fts5_term_matches(t, cols, tok),
        Fts5Query::Near(phrases, dist) => fts5_near_group_matches(phrases, *dist, cols, tok),
        Fts5Query::And(a, b) => fts5_eval(a, cols, tok) && fts5_eval(b, cols, tok),
        Fts5Query::Or(a, b) => fts5_eval(a, cols, tok) || fts5_eval(b, cols, tok),
        Fts5Query::Not(a, b) => fts5_eval(a, cols, tok) && !fts5_eval(b, cols, tok),
    }
}

/// Collect every column name referenced by a `col:` / `{…}:` filter in the term.
#[cfg(feature = "fts5")]
fn fts5_term_filter_names<'a>(term: &'a Fts5Term, out: &mut Vec<&'a str>) {
    for cs in &term.columns {
        out.extend(cs.names.iter().map(String::as_str));
    }
}

/// Validate a `MATCH` query `pattern` before it is run: report a lex-time syntax
/// error (an empty/unterminated `{…}:` brace) or an unknown column named in any
/// `col:` / `{…}:` filter, in SQLite's message form — `fts5: syntax error …` or
/// `no such column: NAME`. `all_columns` is the table's FULL declared column list
/// (indexed and `UNINDEXED`, since an `UNINDEXED` column is a valid filter target
/// that simply matches nothing). Returns `None` when the query is well-formed.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_query_column_error(
    pattern: &str,
    all_columns: &[String],
    tok: Fts5Tok,
) -> Option<String> {
    let toks = fts5_lex(pattern, tok);
    // A lex-time syntax error (malformed brace) is reported first, as SQLite does.
    for t in &toks {
        if let Fts5Lex::Error(msg) = t {
            return Some(msg.clone());
        }
    }
    let query = (Fts5Parser {
        toks: &toks,
        pos: 0,
    })
    .parse()?;
    // Walk the tree collecting every filtered column name.
    fn walk<'a>(q: &'a Fts5Query, names: &mut Vec<&'a str>) {
        match q {
            Fts5Query::Term(t) => fts5_term_filter_names(t, names),
            Fts5Query::Near(phrases, _) => {
                for p in phrases {
                    fts5_term_filter_names(p, names);
                }
            }
            Fts5Query::And(a, b) | Fts5Query::Or(a, b) | Fts5Query::Not(a, b) => {
                walk(a, names);
                walk(b, names);
            }
        }
    }
    let mut names = Vec::new();
    walk(&query, &mut names);
    for name in names {
        if !all_columns.iter().any(|c| c.eq_ignore_ascii_case(name)) {
            return Some(alloc::format!("no such column: {name}"));
        }
    }
    None
}

/// Whether the in-scope columns satisfy the FTS5 query `pattern`. `cols` is the
/// `(name, text)` of each searchable column (one entry for a column-scoped
/// `col MATCH …`, every column for a table-wide `tbl MATCH …`). The query
/// supports bare tokens, `token*` prefixes, `"quoted phrases"`, `col:…` /
/// `{c0 c1}:…` column filters (and their negated `-col:` / `-{…}:` forms), and
/// the boolean operators `AND` (explicit or implicit by juxtaposition), `OR`,
/// and `NOT` (binding tightest to loosest: `NOT`, `AND`, `OR`) with parentheses —
/// matching SQLite's default precedence — and the `NEAR(p1 p2 …, n)` proximity
/// group. A query with no tokens matches nothing.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_query_matches(pattern: &str, cols: &[(String, String)], tok: Fts5Tok) -> bool {
    let toks = fts5_lex(pattern, tok);
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
        .map(|(name, text)| (name.as_str(), fts5_tokenize(text, tok)))
        .collect();
    fts5_eval(&query, &tokenized, tok)
}

/// If `pattern` is a SINGLE BARE TERM whose match set is exactly "the documents
/// in which one indexed token appears in any column", return that token's bytes
/// (the index key, including Porter stemming when configured). Otherwise `None`.
///
/// This is the only shape `MATCH` index-routes through the segment reader, because
/// only here is the index doclist provably identical to the [`fts5_query_matches`]
/// scan: a lone, uncolumned, unanchored, non-prefix one-word term ANDs to itself
/// and is present in a document iff the token appears in some column — which is
/// exactly the term's doclist. Anything else (a column filter, `^` anchor, `tok*`
/// prefix, a multi-word `"phrase"`, `NEAR`, or any boolean combination) stays on
/// the document scan. An empty/no-token query returns `None` (it matches nothing;
/// the scan already yields no rows). The returned bytes feed
/// [`crate::fts5_index::lookup_term_rowids`].
#[cfg(feature = "fts5")]
pub(crate) fn fts5_single_bare_term(pattern: &str, tok: Fts5Tok) -> Option<Vec<u8>> {
    let toks = fts5_lex(pattern, tok);
    // Exactly one lexed item, and it must be a plain Term (no operators/parens,
    // no NEAR group).
    let term = match toks.as_slice() {
        [Fts5Lex::Term(t)] => t,
        _ => return None,
    };
    if !term.any_column() || term.prefix || term.anchored {
        return None;
    }
    // A bare word lexes to a one-token phrase; a quoted multi-word phrase does not.
    let [word] = term.phrase.as_slice() else {
        return None;
    };
    // The index stores the fully tokenized (and, under `porter`, stemmed) form.
    // Re-tokenize the query word through the table's tokenizer so the lookup key
    // matches the indexed token exactly; it must yield exactly one token.
    let key = fts5_tokenize(word, tok);
    match key.as_slice() {
        [single] => Some(single.as_bytes().to_vec()),
        _ => None,
    }
}

/// If `pattern` is a SINGLE COLUMN-SCOPED BARE TERM (`colname : word`), return the
/// `(column name, token bytes)` pair; otherwise `None`.
///
/// This is the column-filtered sibling of [`fts5_single_bare_term`]: a lone term
/// scoped to ONE column (`col:word`), with no prefix/`^`-anchor, whose body is a
/// single-token bare word. Its scan predicate ([`fts5_term_matches`]) is true for
/// a document iff the token occurs in the named column — exactly the subset of the
/// term's index doclist whose posting carries a position in that column. The
/// caller resolves the column name to its index and keeps only those postings.
///
/// Anything else (any boolean/`NEAR`/phrase/prefix/anchor shape, more than one
/// column filter, or a query word that does not tokenize to exactly one token)
/// returns `None` and stays on the document scan. The column name is returned raw
/// (case-insensitive resolution is the caller's job, matching the scan's
/// `eq_ignore_ascii_case`).
#[cfg(feature = "fts5")]
pub(crate) fn fts5_single_bare_term_column(
    pattern: &str,
    tok: Fts5Tok,
) -> Option<(String, Vec<u8>)> {
    let toks = fts5_lex(pattern, tok);
    let term = match toks.as_slice() {
        [Fts5Lex::Term(t)] => t,
        _ => return None,
    };
    // Must be scoped to exactly one positive column (a braced set/negated filter
    // stays on the scan), and otherwise a plain single-token bare word.
    let column = String::from(term.single_column()?);
    if term.prefix || term.anchored {
        return None;
    }
    let [word] = term.phrase.as_slice() else {
        return None;
    };
    let key = fts5_tokenize(word, tok);
    match key.as_slice() {
        [single] => Some((column, single.as_bytes().to_vec())),
        _ => None,
    }
}

/// The index key of a [`Fts5Term`] iff it is a SINGLE BARE PREFIX TERM the segment
/// reader can serve identically to the scan: a one-token `tok*` body that is
/// unanchored (the caller decides table-wide vs column-scoped). Returns the token
/// stemmed EXACTLY as the scan stems a prefix token at match time
/// ([`fts5_term_starts`] stems the phrase, including the prefix token, before the
/// `starts_with` test), so the key equals the prefix of the indexed token forms.
///
/// A prefix term matches a document iff some indexed token starts with this key —
/// the index reader enumerates exactly those terms and unions their doclists, the
/// same set the scan's `doc_token.starts_with(key)` predicate matches. An empty
/// key (a bare `*`) returns `None` — sqlite rejects that query and the scan yields
/// nothing, so there is nothing to route. Anything not a one-token unanchored
/// prefix body returns `None` and stays on the scan.
#[cfg(feature = "fts5")]
fn fts5_prefix_term_key(term: &Fts5Term, tok: Fts5Tok) -> Option<Vec<u8>> {
    if !term.prefix || term.anchored {
        return None;
    }
    let [word] = term.phrase.as_slice() else {
        return None;
    };
    // The prefix token is stored unstemmed by the lexer; the scan stems it at match
    // time (under `porter`), so the index key is the stemmed form (identity when the
    // table isn't `porter`). Match `fts5_term_starts` exactly.
    let key = if tok.stem {
        fts5_porter_stem(word).into_bytes()
    } else {
        word.clone().into_bytes()
    };
    if key.is_empty() {
        return None;
    }
    Some(key)
}

/// If `pattern` is a SINGLE TABLE-WIDE BARE PREFIX TERM (`'word*'`), return the
/// prefix's index key; otherwise `None`. The prefix sibling of
/// [`fts5_single_bare_term`] — uncolumned, unanchored, one-token `tok*` — feeding
/// [`crate::fts5_index::lookup_prefix_rowids`].
#[cfg(feature = "fts5")]
pub(crate) fn fts5_single_prefix_term(pattern: &str, tok: Fts5Tok) -> Option<Vec<u8>> {
    let toks = fts5_lex(pattern, tok);
    let term = match toks.as_slice() {
        [Fts5Lex::Term(t)] => t,
        _ => return None,
    };
    if !term.any_column() {
        return None;
    }
    fts5_prefix_term_key(term, tok)
}

/// If `pattern` is a SINGLE COLUMN-SCOPED BARE PREFIX TERM (`'col : word*'`),
/// return `(column name, prefix key)`; otherwise `None`. The column-scoped sibling
/// of [`fts5_single_prefix_term`], feeding
/// [`crate::fts5_index::lookup_prefix_rowids_in_column`].
#[cfg(feature = "fts5")]
pub(crate) fn fts5_single_prefix_term_column(
    pattern: &str,
    tok: Fts5Tok,
) -> Option<(String, Vec<u8>)> {
    let toks = fts5_lex(pattern, tok);
    let term = match toks.as_slice() {
        [Fts5Lex::Term(t)] => t,
        _ => return None,
    };
    let column = String::from(term.single_column()?);
    let key = fts5_prefix_term_key(term, tok)?;
    Some((column, key))
}

/// The K index keys (K ≥ 2) of a [`Fts5Term`] iff it is a K-TOKEN PHRASE that the
/// segment reader can serve identically to the scan: uncolumned (the caller decides
/// table-wide vs column-scoped), unanchored, non-prefix, and lexing to at least two
/// query tokens. Returns each token stemmed exactly as the scan stems it at match
/// time ([`fts5_term_starts`]), so the keys equal the indexed tokens.
///
/// A phrase matches a
/// document iff its tokens occur at CONSECUTIVE positions in one column — the index
/// reader checks exactly that over the K terms' per-column positions, so the routed
/// result is identical to the scan. Anything else (an anchor, a `tok*` prefix on
/// the last token, or a phrase of fewer than two tokens) returns `None` and stays
/// on the scan. The anchor/prefix flags come from the lexer; a `^` or `*` *inside*
/// the quotes is folded by the tokenizer exactly as the scan folds it, so the same
/// `Fts5Term` drives both paths.
#[cfg(feature = "fts5")]
fn fts5_phrase_keys(term: &Fts5Term, tok: Fts5Tok) -> Option<Vec<Vec<u8>>> {
    if term.prefix || term.anchored {
        return None;
    }
    if term.phrase.len() < 2 {
        return None;
    }
    // The phrase tokens are stored unstemmed; the scan stems them at match time, so
    // the index key is the stemmed form (identity when the table isn't `porter`).
    let keys = term
        .phrase
        .iter()
        .map(|w| {
            if tok.stem {
                fts5_porter_stem(w).into_bytes()
            } else {
                w.clone().into_bytes()
            }
        })
        .collect();
    Some(keys)
}

/// If `pattern` is a SINGLE TABLE-WIDE K-TERM PHRASE (`"t0 t1 …"`, K ≥ 2), return
/// the tokens' index keys; otherwise `None`. The phrase sibling of
/// [`fts5_single_bare_term`] — see [`fts5_phrase_keys`] for the exact shape —
/// feeding [`crate::fts5_index::lookup_phrase_rowids_k`].
#[cfg(feature = "fts5")]
pub(crate) fn fts5_phrase_terms(pattern: &str, tok: Fts5Tok) -> Option<Vec<Vec<u8>>> {
    let toks = fts5_lex(pattern, tok);
    let term = match toks.as_slice() {
        [Fts5Lex::Term(t)] => t,
        _ => return None,
    };
    if !term.any_column() {
        return None;
    }
    fts5_phrase_keys(term, tok)
}

/// If `pattern` is a SINGLE COLUMN-SCOPED K-TERM PHRASE (`col : "t0 t1 …"`, K ≥ 2),
/// return the `(column name, token keys)`; otherwise `None`. The column-scoped
/// sibling of [`fts5_phrase_terms`], feeding
/// [`crate::fts5_index::lookup_phrase_rowids_in_column_k`].
#[cfg(feature = "fts5")]
pub(crate) fn fts5_phrase_terms_column(
    pattern: &str,
    tok: Fts5Tok,
) -> Option<(String, Vec<Vec<u8>>)> {
    let toks = fts5_lex(pattern, tok);
    let term = match toks.as_slice() {
        [Fts5Lex::Term(t)] => t,
        _ => return None,
    };
    let column = String::from(term.single_column()?);
    let keys = fts5_phrase_keys(term, tok)?;
    Some((column, keys))
}

/// If `pattern` is a SINGLE TWO-SINGLE-TOKEN BARE-TERM `NEAR` GROUP
/// (`NEAR(a b, n)`, or `NEAR(a b)` with the default distance 10), return
/// `(token a key, token b key, n)`; otherwise `None`.
///
/// This is the `NEAR` sibling of [`fts5_phrase_terms`], feeding
/// [`crate::fts5_index::lookup_near_rowids`]. The shape it accepts is narrow on
/// purpose, so the index result is provably identical to the
/// [`fts5_query_matches`] scan: the whole query lexes to exactly ONE `NEAR` group
/// (no surrounding boolean/term — `a AND NEAR(...)` and a trailing term stay on the
/// scan), the group has exactly TWO operands, and EACH operand is a plain,
/// uncolumned, unanchored, non-prefix, single-token bare word (via
/// [`fts5_bare_term_key`]). The two tokens are re-tokenized through the table's
/// tokenizer (matching the scan's [`fts5_term_starts`], which stems query tokens
/// under `porter`) so the keys equal the indexed tokens; an operand that does not
/// tokenize to exactly one token is rejected.
///
/// For two single tokens the scan's NEAR rule `max_end − min_start < n + total_len`
/// (`total_len = 2`) reduces to `|pa − pb| <= n + 1`, which
/// [`crate::fts5_index::lookup_near_rowids`] applies to the two terms' per-column
/// positions — exactly what [`fts5_near_matches`] computes. Anything else (a column
/// filter, anchor, prefix, multi-word phrase, ≠2 operands, or any boolean wrapper)
/// returns `None` and stays on the scan.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_two_term_near(pattern: &str, tok: Fts5Tok) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    let toks = fts5_lex(pattern, tok);
    // The entire query must be exactly one NEAR group (no surrounding operators or
    // juxtaposed terms — those are boolean shapes that stay on the scan).
    let (phrases, dist) = match toks.as_slice() {
        [Fts5Lex::Near(phrases, dist)] => (phrases, *dist),
        _ => return None,
    };
    // Exactly two operands, each a plain single-token bare word.
    let [a, b] = phrases.as_slice() else {
        return None;
    };
    Some((
        fts5_bare_term_key(a, tok)?,
        fts5_bare_term_key(b, tok)?,
        dist,
    ))
}

/// The boolean connective of a bare-term `MATCH` node the index can serve via a
/// sorted-merge set-op on its children's rowid lists ([`Fts5BoolTree`]).
#[cfg(feature = "fts5")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fts5BoolOp {
    /// `a AND b` (also the implicit-AND `a b`): documents containing BOTH terms.
    And,
    /// `a OR b`: documents containing EITHER term.
    Or,
    /// `a NOT b`: documents containing `a` but NOT `b`.
    Not,
}

/// The index key of an [`Fts5Term`] iff it is a TABLE-WIDE SINGLE-TOKEN BARE WORD —
/// the exact shape [`fts5_single_bare_term`] routes (uncolumned, unanchored,
/// non-prefix, one query token), tokenized through the table's tokenizer so the key
/// equals the indexed token. `None` for anything else.
#[cfg(feature = "fts5")]
fn fts5_bare_term_key(term: &Fts5Term, tok: Fts5Tok) -> Option<Vec<u8>> {
    if !term.any_column() || term.prefix || term.anchored {
        return None;
    }
    let [word] = term.phrase.as_slice() else {
        return None;
    };
    match fts5_tokenize(word, tok).as_slice() {
        [single] => Some(single.as_bytes().to_vec()),
        _ => None,
    }
}

/// An N-operand boolean TREE of table-wide bare terms the index can serve via
/// sorted-merge doclist set-ops — the general form of [`Fts5BoolOp`], recognized
/// by [`fts5_bare_term_bool_tree`] and evaluated by
/// [`crate::fts5_index::lookup_bool_tree_rowids`].
///
/// A `Leaf` is one operand's indexed token bytes (a table-wide single-token bare
/// word); an `Op` node combines its two children with the matching set-op
/// (`And`→intersection, `Or`→union, `Not`→difference). The tree's *shape* mirrors
/// the parsed [`Fts5Query`] exactly — same operator nodes, same nesting — so its
/// precedence/associativity is whatever [`Fts5Parser`] produced (FTS5's
/// `NOT` > `AND` > `OR`), and a bottom-up rowid evaluation matches the scan's
/// [`fts5_eval`] node-for-node.
#[cfg(feature = "fts5")]
pub(crate) enum Fts5BoolTree {
    /// One operand: the indexed token bytes of a table-wide bare term.
    Leaf(Vec<u8>),
    /// A binary boolean node combining its two children with `op`.
    Op(Fts5BoolOp, Box<Fts5BoolTree>, Box<Fts5BoolTree>),
}

/// Walk a parsed [`Fts5Query`] into an [`Fts5BoolTree`] of table-wide bare-term
/// leaves, or `None` if ANY leaf is not a plain table-wide single-token bare word
/// (a phrase, `tok*` prefix, `^` anchor, `col:` filter, or a `NEAR` group). This
/// is the recursive bare-term check used by [`fts5_bare_term_bool_tree`]:
/// each `Term` must pass [`fts5_bare_term_key`]; each `And`/`Or`/`Not` recurses
/// into both children, preserving the node and its position so the resulting tree
/// is structurally identical to the query AST the scan's [`fts5_eval`] walks.
///
/// Because the tree mirrors the AST exactly, evaluating it bottom-up with the
/// per-node set-op ([`And`]→intersection, [`Or`]→union, [`Not`]→difference of the
/// children's ascending rowid lists) yields exactly the documents `fts5_eval`
/// accepts — `fts5_eval` itself is `&&`/`||`/`&& !` of the children's any-column
/// match sets, and a table-wide bare term's any-column match set is precisely its
/// doclist's rowids. Whenever every leaf is index-servable the route is therefore
/// provably identical to the scan; any non-bare leaf forces the whole query back
/// to the document scan (a partial route could change the set).
///
/// [`And`]: Fts5BoolOp::And
/// [`Or`]: Fts5BoolOp::Or
/// [`Not`]: Fts5BoolOp::Not
#[cfg(feature = "fts5")]
fn fts5_query_to_bool_tree(query: &Fts5Query, tok: Fts5Tok) -> Option<Fts5BoolTree> {
    match query {
        Fts5Query::Term(t) => Some(Fts5BoolTree::Leaf(fts5_bare_term_key(t, tok)?)),
        Fts5Query::Near(..) => None,
        Fts5Query::And(a, b) => Some(Fts5BoolTree::Op(
            Fts5BoolOp::And,
            Box::new(fts5_query_to_bool_tree(a, tok)?),
            Box::new(fts5_query_to_bool_tree(b, tok)?),
        )),
        Fts5Query::Or(a, b) => Some(Fts5BoolTree::Op(
            Fts5BoolOp::Or,
            Box::new(fts5_query_to_bool_tree(a, tok)?),
            Box::new(fts5_query_to_bool_tree(b, tok)?),
        )),
        Fts5Query::Not(a, b) => Some(Fts5BoolTree::Op(
            Fts5BoolOp::Not,
            Box::new(fts5_query_to_bool_tree(a, tok)?),
            Box::new(fts5_query_to_bool_tree(b, tok)?),
        )),
    }
}

/// If `pattern` is a BOOLEAN TREE whose every leaf is a table-wide single-token
/// bare word — `a AND b AND c`, `a OR b OR c`, `a AND b OR c`,
/// `(a OR b) AND NOT c`, the implicit-AND `a b c`, and any nesting thereof —
/// return the [`Fts5BoolTree`] for [`crate::fts5_index::lookup_bool_tree_rowids`].
/// Otherwise `None` (the query stays on the document scan).
///
/// This subsumes the former two-operand `a <op> b` recognizer (a two-leaf tree is
/// just the smallest such tree) and generalizes it to an arbitrary boolean tree —
/// the two-operand fast path is now this tree walker. The result is the
/// parse tree built by [`Fts5Parser`] — so FTS5's `NOT` > `AND` > `OR`
/// precedence and associativity are exactly the scan's — with every `Term`
/// replaced by its index key. A degenerate query with no operator yields a lone
/// `Leaf`; the dedicated [`fts5_single_bare_term`] path already covers that shape,
/// so the executor tries this recognizer only after the single-term ones. Any
/// non-bare leaf (phrase / prefix / anchor / column filter / `NEAR`) anywhere in
/// the tree returns `None`.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_bare_term_bool_tree(pattern: &str, tok: Fts5Tok) -> Option<Fts5BoolTree> {
    let toks = fts5_lex(pattern, tok);
    let query = (Fts5Parser {
        toks: &toks,
        pos: 0,
    })
    .parse()?;
    fts5_query_to_bool_tree(&query, tok)
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
    tok: Fts5Tok,
) -> Fts5Bm25 {
    let n = docs.len();
    // A column is searchable unless it is declared `UNINDEXED`.
    let searchable = |ci: usize| -> bool {
        match (indexed, col_names.get(ci)) {
            (Some(cols), Some(name)) => cols.iter().any(|c| c.eq_ignore_ascii_case(name)),
            _ => true,
        }
    };
    let toks = fts5_lex(query, tok);
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
        .map(|cols| cols.iter().map(|t| fts5_tokenize(t, tok)).collect())
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
                    || name.is_none_or(|nm| !term.admits_column(nm))
                {
                    continue;
                }
                let c = fts5_term_starts(term, ctoks, tok).len();
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

/// The value of an `fts5` `key = value` option, unquoted (one matching pair of
/// surrounding `'…'` / `"…"` / `[…]` stripped), or `None` if no option with that
/// key is present. Keys are matched case-insensitively.
#[cfg(feature = "fts5")]
fn fts5_option_value(args: &[&str], key: &str) -> Option<String> {
    args.iter().find_map(|a| {
        let (k, v) = a.split_once('=')?;
        if !k.trim().eq_ignore_ascii_case(key) {
            return None;
        }
        let v = v.trim();
        let b = v.as_bytes();
        // Strip one matching pair of surrounding quotes: `'…'`, `"…"`, or `[…]`.
        let quoted = b.len() >= 2
            && ((b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0]
                || b[0] == b'[' && b[b.len() - 1] == b']');
        let inner = if quoted { &v[1..v.len() - 1] } else { v };
        Some(String::from(inner))
    })
}

/// The external-content configuration of an `fts5` table, or `None` when the table
/// stores its own documents. Returns `Some((content_table, content_rowid_col))`
/// when a non-empty `content='<table>'` option is present. `content=''` is the
/// *contentless* mode (not external content), so it returns `None`. The rowid
/// column defaults to `rowid` when `content_rowid=` is omitted, matching SQLite.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_external_content(args: &[&str]) -> Option<(String, String)> {
    let table = fts5_option_value(args, "content")?;
    if table.is_empty() {
        return None; // contentless, not external content
    }
    let rowid = fts5_option_value(args, "content_rowid").unwrap_or_else(|| String::from("rowid"));
    Some((table, rowid))
}

/// Whether an `fts5` table is *contentless* (`content=''`): it stores no copy of
/// the document columns — only the inverted index and rowids. Reading an indexed
/// column back yields NULL (SQLite has nothing to return), and the index is kept
/// current by direct DML (`INSERT`/`'delete'`) rather than a content source.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_is_contentless(args: &[&str]) -> bool {
    fts5_option_value(args, "content").is_some_and(|v| v.is_empty())
}

/// Whether an `fts5` table keeps NO local document copy — either *external
/// content* (`content='<table>'`, columns read from that table) or *contentless*
/// (`content=''`, columns read back as NULL). Both maintain their index by direct
/// DML deltas (via `<name>_gpost`) instead of a bulk rebuild from a `_content`
/// shadow. Self-content tables (no `content=` option) return `false`.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_no_local_content(args: &[&str]) -> bool {
    fts5_external_content(args).is_some() || fts5_is_contentless(args)
}

/// Parse an fts5 `rank` configuration string like `bm25(10.0)` or
/// `bm25(2.0, 1.0)` into `(function-name, arguments-string)`, matching SQLite's
/// `sqlite3Fts5ConfigParseRank` (`fts5_config.c`). The value must be a bareword
/// function name, optional whitespace, `(`, a comma-separated argument list, and
/// a closing `)`. The argument slice is the raw text between the parentheses
/// (empty for `bm25()`); trailing text after `)` is ignored. Returns `None` for
/// any malformed shape (SQLite reports `SQLITE_ERROR` → "SQL logic error"):
/// missing name, missing `(`, or an unterminated / malformed argument list.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_parse_rank(value: &str) -> Option<(String, String)> {
    let bytes = value.as_bytes();
    let mut i = 0;
    let skip_ws = |b: &[u8], mut i: usize| {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        i
    };
    // A bareword: alphanumeric, `_`, or `.` (SQLite's `sqlite3Fts5IsBareword`).
    let is_bareword = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'.';
    i = skip_ws(bytes, i);
    let name_start = i;
    while i < bytes.len() && is_bareword(bytes[i]) {
        i += 1;
    }
    if i == name_start {
        return None; // empty function name
    }
    let name = String::from(&value[name_start..i]);
    i = skip_ws(bytes, i);
    if i >= bytes.len() || bytes[i] != b'(' {
        return None; // no `(`
    }
    i += 1;
    i = skip_ws(bytes, i);
    let args_start = i;
    // Scan a comma-separated argument list terminated by `)`. Each argument is a
    // literal (number / quoted string / bareword); we only need to find the
    // matching close paren while respecting single/double-quoted strings.
    if i < bytes.len() && bytes[i] == b')' {
        return Some((name, String::new())); // `bm25()`
    }
    loop {
        // Skip one literal argument up to a `,` or `)` at the top level.
        while i < bytes.len() {
            match bytes[i] {
                b'\'' | b'"' => {
                    let quote = bytes[i];
                    i += 1;
                    // Consume until the matching quote (doubled quote = escape).
                    loop {
                        if i >= bytes.len() {
                            return None; // unterminated string literal
                        }
                        if bytes[i] == quote {
                            if i + 1 < bytes.len() && bytes[i + 1] == quote {
                                i += 2; // escaped quote
                                continue;
                            }
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                }
                b',' | b')' => break,
                _ => i += 1,
            }
        }
        i = skip_ws(bytes, i);
        match bytes.get(i) {
            Some(b')') => {
                let args = String::from(value[args_start..i].trim());
                return Some((name, args));
            }
            Some(b',') => i += 1,
            _ => return None, // unterminated / malformed argument list
        }
    }
}

/// Parse an `fts5` table's `tokenize = '…'` option into a resolved [`Fts5Tok`].
/// The value is a whitespace-separated tokenizer chain: an optional leading
/// `porter` (⇒ Porter stemming, possibly wrapping a base tokenizer), then a base
/// tokenizer (`unicode61` default, or `ascii`), then base-tokenizer options. We
/// honor `unicode61`'s `remove_diacritics 0|1|2` (default 1); the `ascii`
/// tokenizer keeps all bytes verbatim, so it maps to level 0 (no diacritic fold).
/// Unknown options are ignored, as the default tokenizer config (`remove_diacritics
/// 1`, no stemming) is returned when there is no `tokenize` option at all.
#[cfg(feature = "fts5")]
pub(crate) fn fts5_tok_config(args: &[&str]) -> Fts5Tok {
    // Strip ONE matching pair of surrounding quotes (the value's own quotes),
    // leaving any inner `tokenchars '…'` quotes intact for the per-option scan.
    fn unquote(s: &str) -> &str {
        let s = s.trim();
        let b = s.as_bytes();
        if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
            &s[1..s.len() - 1]
        } else {
            s
        }
    }
    let value = args.iter().find_map(|a| {
        a.split_once('=').and_then(|(k, v)| {
            k.trim()
                .eq_ignore_ascii_case("tokenize")
                .then(|| unquote(v))
        })
    });
    let value = match value {
        Some(v) => v,
        None => return Fts5Tok::default(),
    };
    let words: Vec<&str> = value.split_whitespace().collect();
    let mut i = 0;
    let stem = words
        .first()
        .is_some_and(|w| w.eq_ignore_ascii_case("porter"));
    if stem {
        i += 1; // skip `porter`; what follows is the wrapped base tokenizer
    }
    // The base tokenizer name (if present); default `unicode61`. The `ascii`
    // tokenizer keeps all bytes verbatim ⇒ no diacritic fold (level 0).
    let base_ascii = words
        .get(i)
        .is_some_and(|w| w.eq_ignore_ascii_case("ascii"));
    let mut diacritics = if base_ascii { 0 } else { 1 };
    // The ASCII bitmap of a `tokenchars '…'` / `separators '…'` value word.
    let bitmap = |word: &str| -> u128 {
        let mut m = 0u128;
        for ch in unquote(word).chars() {
            if (ch as u32) < 128 {
                m |= 1u128 << (ch as u32);
            }
        }
        m
    };
    let mut tokenchars = 0u128;
    let mut separators = 0u128;
    // Scan the base tokenizer's options: `remove_diacritics <N>` (unicode61 only),
    // `tokenchars '…'`, `separators '…'`.
    let mut j = i;
    while j < words.len() {
        let w = words[j];
        if !base_ascii && w.eq_ignore_ascii_case("remove_diacritics") {
            if let Some(n) = words.get(j + 1).and_then(|x| x.parse::<u8>().ok()) {
                diacritics = n.min(2);
            }
            j += 2;
        } else if w.eq_ignore_ascii_case("tokenchars") {
            if let Some(x) = words.get(j + 1) {
                tokenchars |= bitmap(x);
            }
            j += 2;
        } else if w.eq_ignore_ascii_case("separators") {
            if let Some(x) = words.get(j + 1) {
                separators |= bitmap(x);
            }
            j += 2;
        } else {
            j += 1;
        }
    }
    Fts5Tok {
        stem,
        diacritics,
        tokenchars,
        separators,
    }
}

#[cfg(feature = "fts5")]
impl VTabModule for Fts5Module {
    type Cursor = Fts5Cursor;

    fn connect(&self, args: &[&str]) -> Result<VTabSchema> {
        // Reject any unrecognized `key = value` column option, matching SQLite's
        // `unrecognized option: "<name>"`. SQLite's fts5 recognizes exactly these
        // option keywords (`fts5_config.c`); notably `rank` is NOT a CREATE option
        // in this version (the default rank is configured via the special
        // `INSERT INTO t(t, rank) VALUES('rank', …)` command instead). Column
        // declarations (no `=`) are skipped by `column_name`.
        for a in args {
            if Fts5Module::column_name(a).is_some() {
                continue; // a column declaration, not a `key = value` option
            }
            let Some((k, _)) = a.split_once('=') else {
                continue;
            };
            let key = k.trim();
            const KNOWN: &[&str] = &[
                "prefix",
                "tokenize",
                "content",
                "contentless_delete",
                "content_rowid",
                "columnsize",
                "detail",
                "tokendata",
                "locale",
            ];
            if !KNOWN.iter().any(|o| key.eq_ignore_ascii_case(o)) {
                return Err(Error::Error(alloc::format!(
                    "unrecognized option: \"{key}\""
                )));
            }
        }
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

/// The `fts5vocab` module: a read-only view over another FTS5 table's
/// vocabulary. `connect` validates the `(fts5-table, type)` arguments and
/// declares the columns for the requested `type`. The rows are computed by the
/// executor (`scan_fts5vocab`), which has access to the referenced table's
/// documents — so this module's cursor is never drained (like [`Fts5Cursor`]).
#[cfg(feature = "fts5")]
pub struct Fts5VocabModule;

/// Parse an `fts5vocab` argument list into `(referenced-table, form)`. Accepts
/// `(table, type)` and `(db, table, type)`; the `type` token is unquoted and
/// case-folded to one of `row` / `col` / `instance`.
#[cfg(feature = "fts5")]
pub(crate) fn fts5vocab_args(
    args: &[&str],
) -> Result<(alloc::string::String, alloc::string::String)> {
    let strip = |s: &str| {
        alloc::string::String::from(s.trim().trim_matches(|c| c == '\'' || c == '"' || c == '`'))
    };
    let (table, form) = match args.len() {
        2 => (strip(args[0]), strip(args[1])),
        3 => (strip(args[1]), strip(args[2])),
        _ => {
            return Err(Error::Error(alloc::string::String::from(
                "fts5vocab: expected (fts5-table, type)",
            )))
        }
    };
    let form = form.to_ascii_lowercase();
    if !matches!(form.as_str(), "row" | "col" | "instance") {
        return Err(Error::Error(alloc::format!(
            "fts5vocab: unknown table type: {form:?}"
        )));
    }
    Ok((table, form))
}

#[cfg(feature = "fts5")]
impl VTabModule for Fts5VocabModule {
    type Cursor = Fts5Cursor;

    fn connect(&self, args: &[&str]) -> Result<VTabSchema> {
        let (_table, form) = fts5vocab_args(args)?;
        let columns: &[&str] = match form.as_str() {
            "row" => &["term", "doc", "cnt"],
            "col" => &["term", "col", "doc", "cnt"],
            // instance
            _ => &["term", "doc", "col", "offset"],
        };
        Ok(VTabSchema::new(columns.iter().copied()))
    }

    fn open(&self, _args: &[&str], _plan: &IndexPlan) -> Result<Fts5Cursor> {
        Ok(Fts5Cursor)
    }

    // Derived from the referenced table — no backing storage of its own.
    fn persistent(&self) -> bool {
        false
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
            fts5_tokenize("The quick-brown Fox!", Fts5Tok::default()),
            vec![
                String::from("the"),
                String::from("quick"),
                String::from("brown"),
                String::from("fox"),
            ]
        );
        // Digits are tokens; runs of punctuation/whitespace are separators only.
        assert_eq!(
            fts5_tokenize("  a1  b2,c3 ", Fts5Tok::default()),
            vec![String::from("a1"), String::from("b2"), String::from("c3")]
        );
        assert!(fts5_tokenize("   ,. !", Fts5Tok::default()).is_empty());
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_tok_config_parses_tokenize_option() {
        let cfg = |opt: &str| fts5_tok_config(&[opt]);
        // No tokenize option → the unicode61 default: no stemming, level 1.
        let d = fts5_tok_config(&["body"]);
        assert!(!d.stem && d.diacritics == 1);
        // remove_diacritics levels (quoting and spacing as sqlite writes them).
        assert_eq!(
            cfg("tokenize='unicode61 remove_diacritics 0'").diacritics,
            0
        );
        assert_eq!(
            cfg("tokenize = 'unicode61 remove_diacritics 2'").diacritics,
            2
        );
        assert_eq!(cfg("tokenize='unicode61'").diacritics, 1);
        // porter wraps a base tokenizer → stemming on, base level preserved.
        let p = cfg("tokenize='porter unicode61 remove_diacritics 2'");
        assert!(p.stem && p.diacritics == 2);
        assert!(cfg("tokenize=porter").stem);
        // ascii keeps all bytes verbatim → level 0 (no diacritic fold).
        assert_eq!(cfg("tokenize=ascii").diacritics, 0);
        assert!(cfg("tokenize='porter ascii'").stem);
        assert_eq!(cfg("tokenize='porter ascii'").diacritics, 0);
        // An out-of-range level is clamped to 2 (matching our fold table).
        assert_eq!(
            cfg("tokenize='unicode61 remove_diacritics 9'").diacritics,
            2
        );

        // tokenchars / separators: the nested-quoted value's ASCII chars set bits;
        // the outer double-quotes are stripped without eating the inner quotes.
        let tc = cfg("tokenize=\"unicode61 tokenchars '-_.'\"");
        assert!(tc.is_token_char('-') && tc.is_token_char('_') && tc.is_token_char('.'));
        assert!(!tc.is_token_char('@')); // not listed → default classification
        assert!(tc.is_token_char('a')); // alphanumerics still tokens
        let sp = cfg("tokenize=\"unicode61 separators 'x'\"");
        assert!(!sp.is_token_char('x')); // alphanumeric, but a separator
        assert!(sp.is_token_char('y'));
        // separators win over tokenchars for the same char.
        let both = cfg("tokenize=\"unicode61 tokenchars '-' separators '-'\"");
        assert!(!both.is_token_char('-'));
        // Combined with remove_diacritics, and on the ascii base tokenizer.
        let combo = cfg("tokenize=\"unicode61 remove_diacritics 0 tokenchars '-'\"");
        assert!(combo.diacritics == 0 && combo.is_token_char('-'));
        assert!(cfg("tokenize=\"ascii tokenchars '-'\"").is_token_char('-'));
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_query_matches_are_token_anded() {
        let doc = [(String::from("body"), String::from("the quick brown fox"))];
        assert!(fts5_query_matches("fox", &doc, Fts5Tok::default()));
        assert!(fts5_query_matches("QUICK fox", &doc, Fts5Tok::default())); // case-insensitive AND
        assert!(!fts5_query_matches("quick zebra", &doc, Fts5Tok::default())); // one token missing
        assert!(!fts5_query_matches("", &doc, Fts5Tok::default())); // empty query matches nothing
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_column_filters_scope_tokens() {
        let cols = [
            (String::from("title"), String::from("Mixed Fox")),
            (String::from("body"), String::from("and the dog")),
        ];
        // A bare token matches in any column; `col:token` only in that column.
        assert!(fts5_query_matches("fox", &cols, Fts5Tok::default()));
        assert!(fts5_query_matches("title:fox", &cols, Fts5Tok::default()));
        assert!(!fts5_query_matches("body:fox", &cols, Fts5Tok::default())); // fox is in title, not body
        assert!(fts5_query_matches(
            "title:mixed body:dog",
            &cols,
            Fts5Tok::default()
        )); // AND across columns
        assert!(!fts5_query_matches("title:dog", &cols, Fts5Tok::default())); // dog is in body, not title
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_phrase_and_prefix_queries() {
        let doc = [(
            String::from("body"),
            String::from("the quick brown fox runs"),
        )];
        // A quoted phrase requires consecutive, ordered tokens.
        assert!(fts5_query_matches(
            "\"quick brown\"",
            &doc,
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "\"brown quick\"",
            &doc,
            Fts5Tok::default()
        )); // wrong order
        assert!(!fts5_query_matches(
            "\"quick fox\"",
            &doc,
            Fts5Tok::default()
        )); // not adjacent
            // A `token*` prefix matches any token starting with it.
        assert!(fts5_query_matches("fo*", &doc, Fts5Tok::default())); // fox
        assert!(fts5_query_matches("run*", &doc, Fts5Tok::default())); // runs
        assert!(!fts5_query_matches("cat*", &doc, Fts5Tok::default()));
        // Column-scoped phrase / prefix.
        assert!(fts5_query_matches(
            "body:\"quick brown\"",
            &doc,
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches("body:ru*", &doc, Fts5Tok::default()));
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_phrase_recognizer_shapes() {
        let tok = Fts5Tok::default();
        let v = |ws: &[&[u8]]| ws.iter().map(|w| w.to_vec()).collect::<Vec<Vec<u8>>>();
        // A bare two-word phrase is recognized, table-wide (the K = 2 case).
        assert_eq!(
            fts5_phrase_terms("\"quick brown\"", tok),
            Some(v(&[b"quick", b"brown"]))
        );
        // A K-word phrase (K = 3, 4) is recognized, table-wide.
        assert_eq!(
            fts5_phrase_terms("\"one two three\"", tok),
            Some(v(&[b"one", b"two", b"three"]))
        );
        assert_eq!(
            fts5_phrase_terms("\"a b c d\"", tok),
            Some(v(&[b"a", b"b", b"c", b"d"]))
        );
        // Column-scoped K-word phrase.
        assert_eq!(
            fts5_phrase_terms_column("body : \"quick brown fox\"", tok),
            Some((String::from("body"), v(&[b"quick", b"brown", b"fox"])))
        );
        // A column-scoped phrase is NOT a table-wide phrase, and vice versa.
        assert_eq!(fts5_phrase_terms("body:\"quick brown\"", tok), None);
        assert_eq!(fts5_phrase_terms_column("\"quick brown\"", tok), None);
        // Repeated words still recognize (the index check handles self-adjacency).
        assert_eq!(
            fts5_phrase_terms("\"go go\"", tok),
            Some(v(&[b"go", b"go"]))
        );
        assert_eq!(
            fts5_phrase_terms("\"go go go\"", tok),
            Some(v(&[b"go", b"go", b"go"]))
        );
        // Rejected shapes: a single token, anchor, boolean, NEAR, bare terms.
        assert_eq!(fts5_phrase_terms("\"only\"", tok), None);
        assert_eq!(fts5_phrase_terms("^\"quick brown\"", tok), None);
        assert_eq!(fts5_phrase_terms("quick brown", tok), None); // implicit AND, not a phrase
        assert_eq!(fts5_phrase_terms("word", tok), None);
        assert_eq!(fts5_phrase_terms("\"a b\" OR \"c d\"", tok), None);
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_phrase_recognizer_stems() {
        // Under the porter tokenizer the phrase tokens are stemmed to their index
        // keys (matching what the writer stored and the scan re-stems at match).
        let tok = Fts5Tok {
            stem: true,
            ..Fts5Tok::default()
        };
        let v = |ws: &[&[u8]]| ws.iter().map(|w| w.to_vec()).collect::<Vec<Vec<u8>>>();
        assert_eq!(
            fts5_phrase_terms("\"running shoes\"", tok),
            Some(v(&[b"run", b"shoe"]))
        );
        // K = 3 stems every token.
        assert_eq!(
            fts5_phrase_terms("\"running shoes quickly\"", tok),
            Some(v(&[b"run", b"shoe", b"quickli"]))
        );
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_prefix_term_recognizer_shapes() {
        let tok = Fts5Tok::default();
        // A bare single prefix term is recognized table-wide.
        assert_eq!(fts5_single_prefix_term("wor*", tok), Some(b"wor".to_vec()));
        // Case folds to the indexed (lowercase) form under unicode61.
        assert_eq!(fts5_single_prefix_term("WoR*", tok), Some(b"wor".to_vec()));
        // Column-scoped prefix.
        assert_eq!(
            fts5_single_prefix_term_column("body : wor*", tok),
            Some((String::from("body"), b"wor".to_vec()))
        );
        assert_eq!(
            fts5_single_prefix_term_column("body:wor*", tok),
            Some((String::from("body"), b"wor".to_vec()))
        );
        // Table-wide and column-scoped are mutually exclusive.
        assert_eq!(fts5_single_prefix_term("body:wor*", tok), None);
        assert_eq!(fts5_single_prefix_term_column("wor*", tok), None);
        // Rejected: a non-prefix bare term, an anchored prefix, a phrase, a boolean,
        // a bare `*` (empty prefix), and a multi-token body.
        assert_eq!(fts5_single_prefix_term("word", tok), None);
        assert_eq!(fts5_single_prefix_term("^wor*", tok), None);
        assert_eq!(fts5_single_prefix_term("\"a b\"*", tok), None);
        assert_eq!(fts5_single_prefix_term("a* OR b*", tok), None);
        assert_eq!(fts5_single_prefix_term("*", tok), None);
        // A bare term is NOT a prefix term, and vice versa.
        assert_eq!(fts5_single_bare_term("wor*", tok), None);
        assert_eq!(fts5_single_prefix_term("word", tok), None);
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_prefix_term_recognizer_stems() {
        // Under porter, the prefix token is stemmed to its index-key form — exactly
        // what the scan does at match time (`fts5_term_starts` stems the prefix
        // token before `starts_with`). `running` → `run`, `connecting` → `connect`.
        let tok = Fts5Tok {
            stem: true,
            ..Fts5Tok::default()
        };
        assert_eq!(
            fts5_single_prefix_term("running*", tok),
            Some(b"run".to_vec())
        );
        assert_eq!(
            fts5_single_prefix_term("connecting*", tok),
            Some(b"connect".to_vec())
        );
        // `run*` already stems to `run`.
        assert_eq!(fts5_single_prefix_term("run*", tok), Some(b"run".to_vec()));
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_boolean_operators_and_precedence() {
        let doc = |s: &str| [(String::from("body"), String::from(s))];
        // OR / AND / NOT.
        assert!(fts5_query_matches(
            "apple OR cherry",
            &doc("apple banana"),
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "apple AND date",
            &doc("apple banana"),
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches(
            "apple AND date",
            &doc("apple date"),
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches(
            "banana NOT cherry",
            &doc("apple banana"),
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "banana NOT cherry",
            &doc("banana cherry"),
            Fts5Tok::default()
        ));
        // AND binds tighter than OR: `apple OR banana AND cherry`.
        assert!(fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("apple only"),
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("banana cherry"),
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "apple OR banana AND cherry",
            &doc("banana only"),
            Fts5Tok::default()
        ));
        // Parentheses override precedence.
        assert!(fts5_query_matches(
            "(apple OR banana) AND date",
            &doc("apple date"),
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "(apple OR banana) AND date",
            &doc("apple only"),
            Fts5Tok::default()
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
        assert!(fts5_query_matches(
            "NEAR(quick brown)",
            &adjacent,
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches(
            "NEAR(quick brown)",
            &gap4,
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches(
            "NEAR(quick brown, 2)",
            &gap2,
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "NEAR(quick brown, 1)",
            &gap2,
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches(
            "NEAR(quick brown, 0)",
            &adjacent,
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "NEAR(quick brown, 0)",
            &gap2,
            Fts5Tok::default()
        ));
        // A missing phrase never matches; NEAR composes with the boolean operators.
        assert!(!fts5_query_matches(
            "NEAR(quick zebra, 5)",
            &adjacent,
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches(
            "NEAR(quick brown, 2) AND fox",
            &adjacent,
            Fts5Tok::default()
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
        let score = |q: &str, i: usize| {
            fts5_bm25_corpus(q, &names, &docs, None, None, Fts5Tok::default()).score(i, &[])
        };

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
        let corpus = fts5_bm25_corpus("apple", &names, &docs, None, None, Fts5Tok::default());
        assert!(corpus.score(0, &[10.0]) < corpus.score(0, &[]));
        assert_eq!(corpus.score(3, &[10.0]), 0.0); // still 0 where the term is absent
    }

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_highlight_wraps_matched_tokens() {
        let names = [String::from("body")];
        let hl = |q: &str, text: &str| {
            fts5_highlight(q, &names, None, 0, text, Fts5Tok::default(), "[", "]")
        };
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
        assert!(fts5_query_matches(
            "^quick",
            &doc("quick brown fox"),
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "^quick",
            &doc("the quick fox"),
            Fts5Tok::default()
        ));
        // Anchored phrases too; an unanchored token still matches anywhere.
        assert!(fts5_query_matches(
            "^\"quick brown\"",
            &doc("quick brown fox"),
            Fts5Tok::default()
        ));
        assert!(!fts5_query_matches(
            "^\"quick brown\"",
            &doc("a quick brown"),
            Fts5Tok::default()
        ));
        assert!(fts5_query_matches(
            "quick",
            &doc("the quick fox"),
            Fts5Tok::default()
        ));
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

    #[test]
    #[cfg(feature = "fts5")]
    fn fts5_parse_rank_splits_name_and_args() {
        let ok = |s: &str| fts5_parse_rank(s).unwrap();
        // Name + a single argument, with the argument slice trimmed.
        assert_eq!(
            ok("bm25(10.0)"),
            (String::from("bm25"), String::from("10.0"))
        );
        // Multiple comma-separated arguments preserve their internal spacing.
        assert_eq!(
            ok("bm25(2.0, 1.0)"),
            (String::from("bm25"), String::from("2.0, 1.0"))
        );
        // Empty argument list (`bm25()` — the reset-to-default form).
        assert_eq!(ok("bm25()"), (String::from("bm25"), String::new()));
        // Surrounding whitespace and trailing text after `)` are ignored, matching
        // SQLite's `sqlite3Fts5ConfigParseRank`.
        assert_eq!(
            ok("  bm25(5.0)  "),
            (String::from("bm25"), String::from("5.0"))
        );
        assert_eq!(
            ok("bm25(5.0)extra"),
            (String::from("bm25"), String::from("5.0"))
        );
        // A quoted string argument is kept intact (the `)` inside is not a close).
        assert_eq!(ok("f('a)b')"), (String::from("f"), String::from("'a)b'")));
        // Malformed shapes are rejected (SQLite → SQLITE_ERROR).
        assert!(fts5_parse_rank("bm25").is_none()); // no `(`
        assert!(fts5_parse_rank("bm25(").is_none()); // unterminated
        assert!(fts5_parse_rank("bm25(10.0").is_none()); // no close `)`
        assert!(fts5_parse_rank("()").is_none()); // empty name
        assert!(fts5_parse_rank("(10.0)").is_none()); // empty name
        assert!(fts5_parse_rank("").is_none()); // empty
    }
}
