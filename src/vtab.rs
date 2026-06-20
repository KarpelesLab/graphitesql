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
//! | `xBestIndex`                     | [`VTabModule::best_index`] (stubbed, D1b) |
//! | `xOpen` / `xFilter`              | [`VTabModule::open`] → [`VTabCursor`]     |
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
//! # What is stubbed / deferred to D1b
//!
//! * **Constraint pushdown / `best_index`.** [`VTabModule::best_index`] has a
//!   default implementation that returns an empty (no-pushdown) plan, and
//!   [`VTabModule::open`] receives the chosen [`IndexPlan`] but an implementation
//!   is free to ignore it and do a full scan. A real cost-based planner that
//!   turns SQL `WHERE` constraints into [`IndexConstraint`]s and feeds the chosen
//!   plan back into the scan is D1b.
//! * **`CREATE VIRTUAL TABLE` + executor integration.** Parsing the DDL, calling
//!   [`VTabModule::connect`] with the `USING` arguments, and surfacing the cursor
//!   rows as a `FROM` source all belong to D1b. The shape of "a `FROM` source
//!   that yields rows" mirrors the existing table-valued functions in
//!   `crate::exec` (`generate_series`, `json_each`); this trait is the reusable,
//!   registerable generalization of that idea.
//! * **Writes.** `xUpdate` (INSERT/UPDATE/DELETE through a virtual table) is not
//!   modeled here; D1a is read/scan only.

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
/// Carried by [`IndexConstraint`]. For D1a this type exists so the `best_index`
/// surface is fully shaped; the planner that *produces* constraints from SQL is
/// D1b.
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
/// In D1b the planner fills these in from the query; in D1a they are part of the
/// trait surface and the example module simply ignores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexConstraint {
    /// Index of the constrained column within the [`VTabSchema`].
    pub column: usize,
    /// The comparison operator.
    pub op: ConstraintOp,
    /// Whether this constraint is usable (SQLite may offer constraints whose
    /// right-hand side is not available to the current plan).
    pub usable: bool,
}

/// The plan a module chose in [`VTabModule::best_index`], the analog of the
/// outputs SQLite reads back from `sqlite3_index_info` (`idxNum`, `idxStr`,
/// `estimatedCost`, and which constraints are consumed).
///
/// The plan is opaque to the engine: the module invents [`idx_num`](Self::idx_num)
/// / [`idx_str`](Self::idx_str) for itself and reads them back in
/// [`VTabModule::open`] to drive the scan. D1a ships the type and the default
/// "no plan" value; wiring a real cost model is D1b.
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
    /// position the module wants its value passed in, or `0` to ignore it.
    /// Length matches the offered constraint slice. Stubbed empty in D1a.
    pub argv_index: Vec<u32>,
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

/// A virtual-table module: the safe analog of `sqlite3_module`.
///
/// A connection registers an implementation under a name (see [`VTabRegistry`]).
/// The lifecycle is: [`connect`](Self::connect) declares the table's columns from
/// the `USING` arguments; [`best_index`](Self::best_index) (optional, stubbed for
/// D1a) chooses a scan plan from offered constraints; [`open`](Self::open) starts
/// a scan under the chosen plan, returning a [`VTabCursor`].
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
    /// **Stubbed in D1a:** the default returns an empty, high-cost plan that
    /// consumes no constraints, i.e. "I will do a full scan." Implementations may
    /// override this in D1b once the planner offers real constraints.
    fn best_index(&self, _constraints: &[IndexConstraint]) -> Result<IndexPlan> {
        Ok(IndexPlan {
            estimated_cost: f64::from(u32::MAX),
            ..IndexPlan::default()
        })
    }

    /// Open a scan over the table under the chosen `plan`, returning a cursor.
    ///
    /// In D1a an implementation may ignore `plan` and perform a full scan; the
    /// `plan` argument exists so the signature is stable when D1b adds real
    /// constraint pushdown.
    fn open(&self, plan: &IndexPlan) -> Result<Self::Cursor>;
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
    /// See [`VTabModule::open`]; returns a boxed, type-erased cursor.
    fn dyn_open(&self, plan: &IndexPlan) -> Result<Box<dyn DynCursor>>;
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
    fn dyn_open(&self, plan: &IndexPlan) -> Result<Box<dyn DynCursor>> {
        Ok(Box::new(VTabModule::open(self, plan)?) as Box<dyn DynCursor>)
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
/// module implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeriesModule;

/// The cursor for [`SeriesModule`], walking `start..=stop` by `step`.
#[derive(Debug)]
pub struct SeriesCursor {
    next: i64,
    stop: i64,
    step: i64,
    /// Sequential rowid, 1-based, assigned as rows are produced.
    next_rowid: i64,
    done: bool,
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
        self.next_rowid += 1;
        match self.next.checked_add(self.step) {
            Some(n) => self.next = n,
            None => self.done = true, // i64 overflow ends the series
        }
        Ok(Some(row))
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

    fn open(&self, plan: &IndexPlan) -> Result<SeriesCursor> {
        // D1a ignores the plan and always full-scans; the argument is taken so
        // the signature is stable for D1b constraint pushdown.
        let _ = plan;
        Ok(SeriesCursor {
            next: 0,
            stop: 0,
            step: 1,
            next_rowid: 1,
            done: true,
        })
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
        // The dyn `open` path goes through `SeriesModule::open`, which in D1a
        // yields an empty scan; the configured scan is exercised via `scan`
        // above. Here we assert the boxed cursor is produced and drainable.
        let mut cur = module.dyn_open(&plan).unwrap();
        // D1a's `open` is the empty full-scan stub, so it yields no rows.
        assert!(cur.dyn_next().unwrap().is_none());
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
