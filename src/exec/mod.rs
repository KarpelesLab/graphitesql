//! Query execution: the `Connection` API and the read-query executor.
//!
//! This layer ties the pieces together: parse SQL ([`crate::sql`]), resolve
//! names against the schema catalog ([`crate::schema`]), scan b-trees
//! ([`crate::btree`]), decode records ([`crate::format::record`]), and evaluate
//! expressions ([`eval`]) to produce result rows.
//!
//! It implements an *operational, iterator-style* executor rather than emitting
//! VDBE bytecode. The observable semantics (row order, type coercion, NULL
//! handling) follow SQLite; the bytecode representation the roadmap describes is
//! an internal-representation refactor we can layer in later without changing
//! results. The [`Connection`] reads (`query`) and writes (`execute`) over a
//! writable pager, an in-memory database, or — read-only — a WAL-mode database
//! (the `-wal` overlay is detected automatically).

pub mod datetime;
pub mod eval;
pub mod func;
pub mod json;
pub mod vdbe;
mod window;

use crate::btree::{
    clear_index, clear_table, create_index_root, create_table_root, delete_table, free_tree,
    insert_index, insert_table, table_has_empty_leaf, IndexCursor, TableCursor,
};
use crate::error::{Error, Result};
use crate::format::record::{decode_record, encode_record};
use crate::pager::{AutoVacuum, PageSource, WritePager};
use crate::schema::Schema;
use crate::sql::ast::*;
use crate::sql::{self};
use crate::value::Value;
use crate::vfs::{OpenFlags, Vfs};
use crate::vtab::{
    ConstraintOp, DynVTabModule, IndexConstraint, IndexPlan, VTabChange, VTabRegistry, VTabStore,
};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use eval::{ColumnInfo, EvalCtx, Params};

/// The result of a query: column labels and the materialized rows.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Result column labels, in order.
    pub columns: Vec<String>,
    /// Result rows, each with one value per column.
    pub rows: Vec<Vec<Value>>,
}

/// The storage backing a connection: a writable pager, or a read-only page
/// source (e.g. a WAL-mode database opened read-only).
enum Backend {
    Write(Box<WritePager>),
    Read(Box<dyn PageSource>),
}

impl Backend {
    fn source(&self) -> &dyn PageSource {
        match self {
            Backend::Write(w) => w.as_ref(),
            Backend::Read(r) => r.as_ref(),
        }
    }
    fn writer(&mut self) -> Result<&mut WritePager> {
        match self {
            Backend::Write(w) => Ok(w),
            Backend::Read(_) => Err(Error::Error("database is read-only".into())),
        }
    }
    fn wal_mode(&self) -> bool {
        matches!(self, Backend::Write(w) if w.wal_mode())
    }
}

/// A database connection. Supports reading (`query`) and writing (`execute`),
/// over a file or in memory.
pub struct Connection {
    backend: Backend,
    schema: Schema,
    /// The `main` database's file path (empty for an in-memory database), as
    /// reported by `PRAGMA database_list`.
    main_file: String,
    /// Attached databases (`ATTACH … AS name`), in attachment order, each with
    /// its own backend and schema. The `main` database is the fields above; this
    /// list holds everything attached after it.
    attached: Vec<AttachedDb>,
    /// The `temp` database (`CREATE TEMP …`), created lazily on first use and
    /// invisible to other connections. Reported at seq 1 by `database_list`.
    temp_db: Option<AttachedDb>,
    /// True between `BEGIN` and `COMMIT`/`ROLLBACK`; suppresses autocommit.
    in_tx: bool,
    /// A stack of materialized `WITH` common table expressions in scope, innermost
    /// last. Resolved by name during `FROM` scanning before the schema is
    /// consulted; this is also how a recursive CTE sees its own working table.
    cte_env: core::cell::RefCell<Vec<CteBinding>>,
    /// A stack of enclosing query rows, innermost last. A correlated subquery
    /// pushes its evaluation row here so its body can resolve outer columns.
    outer_scope: core::cell::RefCell<Vec<OuterFrame>>,
    /// Whether foreign-key constraints are enforced (`PRAGMA foreign_keys`).
    /// Off by default, matching SQLite.
    foreign_keys: bool,
    /// Re-entrancy depth of trigger firing.
    trigger_depth: core::cell::Cell<usize>,
    /// Set by an `OR FAIL` conflict before it raises: tells the statement-level
    /// atomicity wrapper to keep the rows changed before the failure (rather than
    /// rolling the statement back, which is the `OR ABORT` default).
    stmt_keep_partial: core::cell::Cell<bool>,
    /// Set by an `OR ROLLBACK` conflict before it raises: the surrounding
    /// transaction must be unwound, not just the current statement.
    stmt_rollback_tx: core::cell::Cell<bool>,
    /// Set when a `BEFORE` trigger runs `SELECT RAISE(IGNORE)`: the row operation
    /// that fired the trigger is silently abandoned (no error). The firing caller
    /// reads and clears it.
    raise_ignore: core::cell::Cell<bool>,
    /// Whether triggers may fire other triggers (`PRAGMA recursive_triggers`).
    /// Off by default, matching SQLite: triggers then fire only at the top level.
    recursive_triggers: bool,
    /// Rows projected by the most recent `RETURNING` clause, drained by
    /// [`execute_returning`](Self::execute_returning). Populated as a side effect
    /// of `INSERT`/`UPDATE`/`DELETE` execution when the statement has a
    /// `RETURNING` list.
    returning_rows: core::cell::RefCell<Vec<Vec<Value>>>,
    /// Count of open savepoints. Like `in_tx`, a non-zero count suppresses
    /// autocommit so changes accumulate until the outermost savepoint is released.
    open_savepoints: usize,
    /// The rowid of the most recently inserted row (`last_insert_rowid()`).
    last_insert_rowid: core::cell::Cell<i64>,
    /// Rows modified by the most recent INSERT/UPDATE/DELETE (`changes()`).
    changes: core::cell::Cell<i64>,
    /// Rows modified since the connection opened (`total_changes()`).
    total_changes: core::cell::Cell<i64>,
    /// During a cross-database view read, the database whose catalog unqualified
    /// table names resolve against (so a view's body reads its own database's
    /// tables). `Main` at all other times; nested subqueries inherit it. Set and
    /// restored around [`scan_db_view`](Self::scan_db_view).
    read_default: core::cell::Cell<DbRef>,
    /// Virtual-table modules registered on this connection, keyed by the name
    /// that follows `USING` in `CREATE VIRTUAL TABLE`. Seeded with the built-in
    /// `series` module; a public registration API is roadmap D4.
    vtab_registry: VTabRegistry,
    /// State for `random()`/`randomblob()`, advanced one SplitMix64 step per
    /// value. Seeded from the system clock under `std` (so each process run
    /// differs, like SQLite reseeding from the OS) and from a fixed constant in
    /// `no_std` builds (which have no entropy source) — non-determinism that no
    /// differential test can observe either way.
    rng_state: core::cell::Cell<u64>,
    /// `PRAGMA cache_size` setting, round-tripped verbatim (a positive value is a
    /// page count, a negative value is KiB; default −2000). graphite keeps every
    /// page resident, so this is reported back but does not bound a real cache.
    cache_size: core::cell::Cell<i64>,
    /// `PRAGMA analysis_limit` — the row sample cap `ANALYZE` would use (0 =
    /// unlimited). graphite always analyzes fully, so this is advisory; it is
    /// stored and reported back like sqlite (which clamps a negative value to 0).
    analysis_limit: core::cell::Cell<i64>,
    /// `PRAGMA busy_timeout` — the lock-wait timeout in ms (0 = no wait). graphite
    /// has no cross-process lock manager, so this never blocks; it is stored and
    /// reported back like sqlite (which clamps a negative value to 0).
    busy_timeout: core::cell::Cell<i64>,
    /// `PRAGMA secure_delete` (0=off, 1=on, 2=fast), round-tripped like sqlite.
    /// When non-zero, freed pages are zeroed (the pager honors it); a
    /// per-connection runtime setting, not persisted in the file.
    secure_delete: core::cell::Cell<i64>,
    /// User-defined scalar functions registered via
    /// [`register_function`](Self::register_function), keyed by lowercased name.
    /// Built-in functions take precedence; these fill otherwise-unknown names.
    functions: alloc::collections::BTreeMap<String, ScalarFunction>,
    /// User-defined aggregate functions registered via
    /// [`register_aggregate_function`](Self::register_aggregate_function), keyed by
    /// lowercased name. Built-in aggregates take precedence.
    aggregates: alloc::collections::BTreeMap<String, AggregateFactory>,
    /// Per-query FTS5 state ([`Fts5QueryCtx`]: the MATCH query plus, when ranking
    /// is referenced, the bm25 corpus), set by `run_core` while executing a
    /// `SELECT … MATCH …` over an `fts5` table and read by the `rank`/`bm25()`/
    /// `highlight()` special forms. `None` outside such a query.
    #[cfg(feature = "fts5")]
    fts5_rank: core::cell::RefCell<Option<Fts5QueryCtx>>,
    /// Whether `SELECT` execution tries the VDBE engine first, falling back
    /// transparently to the tree-walker for any query shape it does not support.
    /// **On by default** (Track B, B7b): the VDBE is the primary engine, parity-
    /// validated across the full test suite and the differential corpus. Toggled
    /// by [`set_use_vdbe`](Self::set_use_vdbe) — turn it off to force the
    /// tree-walker. The result is identical either way; this only chooses which
    /// engine produces it.
    use_vdbe: core::cell::Cell<bool>,
}

/// A user-defined scalar function: it receives its evaluated argument values and
/// returns a result [`Value`] (or an error). Registered with
/// [`Connection::register_function`].
pub type ScalarFunction = Box<dyn Fn(&[Value]) -> Result<Value>>;

/// A user-defined aggregate's accumulator: `step` is called once per group row
/// with the evaluated argument values, then `finalize` produces the result.
/// A fresh accumulator is created (by the registered factory) for each group.
pub trait AggregateFunction {
    /// Fold one row's argument values into the accumulator.
    fn step(&mut self, args: &[Value]) -> Result<()>;
    /// Produce the aggregate's value for the group.
    fn finalize(&mut self) -> Result<Value>;
}

/// Builds a fresh [`AggregateFunction`] accumulator per group. Registered with
/// [`Connection::register_aggregate_function`].
pub type AggregateFactory = Box<dyn Fn() -> Box<dyn AggregateFunction>>;

/// Initial seed for a connection's `random()` generator. Under `std` it mixes
/// the wall clock so repeated invocations of the binary produce different
/// sequences; `no_std` builds, lacking any entropy source, fall back to a fixed
/// constant (the SplitMix64 golden-ratio increment).
fn initial_rng_seed() -> u64 {
    #[cfg(feature = "std")]
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        nanos ^ 0x9E37_79B9_7F4A_7C15
    }
    #[cfg(not(feature = "std"))]
    {
        0x9E37_79B9_7F4A_7C15
    }
}

/// Which database an operation targets: `main`, the lazily-created `temp`
/// database, or an attached database by index.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DbRef {
    Main,
    Temp,
    Attached(usize),
}

/// An attached database (`ATTACH 'file' AS name`): its own storage and catalog.
struct AttachedDb {
    /// The schema name given in `ATTACH … AS name`.
    name: String,
    /// The file path it was attached from (empty for an in-memory attachment).
    file: String,
    backend: Backend,
    schema: Schema,
}

/// A materialized common table expression: a named, in-memory relation.
struct CteBinding {
    name: String,
    columns: Vec<ColumnInfo>,
    rows: Vec<InputRow>,
}

/// A snapshot of an enclosing query's current row, for correlated subqueries.
struct OuterFrame {
    columns: Vec<ColumnInfo>,
    row: Vec<Value>,
    rowid: Option<i64>,
}

/// The kind of data-change event, for trigger matching.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TrigEvent {
    Insert,
    Update,
    Delete,
}

impl Connection {
    fn from_pager(db: WritePager) -> Result<Connection> {
        let backend = Backend::Write(Box::new(db));
        let schema = Schema::read(backend.source())?;
        Ok(Connection {
            backend,
            schema,
            main_file: String::new(),
            attached: Vec::new(),
            temp_db: None,
            in_tx: false,
            cte_env: core::cell::RefCell::new(Vec::new()),
            outer_scope: core::cell::RefCell::new(Vec::new()),
            foreign_keys: false,
            trigger_depth: core::cell::Cell::new(0),
            stmt_keep_partial: core::cell::Cell::new(false),
            stmt_rollback_tx: core::cell::Cell::new(false),
            raise_ignore: core::cell::Cell::new(false),
            recursive_triggers: false,
            returning_rows: core::cell::RefCell::new(Vec::new()),
            open_savepoints: 0,
            last_insert_rowid: core::cell::Cell::new(0),
            changes: core::cell::Cell::new(0),
            total_changes: core::cell::Cell::new(0),
            read_default: core::cell::Cell::new(DbRef::Main),
            vtab_registry: VTabRegistry::with_builtins(),
            rng_state: core::cell::Cell::new(initial_rng_seed()),
            cache_size: core::cell::Cell::new(-2000),
            analysis_limit: core::cell::Cell::new(0),
            busy_timeout: core::cell::Cell::new(0),
            secure_delete: core::cell::Cell::new(0),
            functions: alloc::collections::BTreeMap::new(),
            aggregates: alloc::collections::BTreeMap::new(),
            #[cfg(feature = "fts5")]
            fts5_rank: core::cell::RefCell::new(None),
            use_vdbe: core::cell::Cell::new(true),
        })
    }

    fn from_read_backend(backend: Box<dyn PageSource>) -> Result<Connection> {
        let backend = Backend::Read(backend);
        let schema = Schema::read(backend.source())?;
        Ok(Connection {
            backend,
            schema,
            main_file: String::new(),
            attached: Vec::new(),
            temp_db: None,
            in_tx: false,
            cte_env: core::cell::RefCell::new(Vec::new()),
            outer_scope: core::cell::RefCell::new(Vec::new()),
            foreign_keys: false,
            trigger_depth: core::cell::Cell::new(0),
            stmt_keep_partial: core::cell::Cell::new(false),
            stmt_rollback_tx: core::cell::Cell::new(false),
            raise_ignore: core::cell::Cell::new(false),
            recursive_triggers: false,
            returning_rows: core::cell::RefCell::new(Vec::new()),
            open_savepoints: 0,
            last_insert_rowid: core::cell::Cell::new(0),
            changes: core::cell::Cell::new(0),
            total_changes: core::cell::Cell::new(0),
            read_default: core::cell::Cell::new(DbRef::Main),
            vtab_registry: VTabRegistry::with_builtins(),
            rng_state: core::cell::Cell::new(initial_rng_seed()),
            cache_size: core::cell::Cell::new(-2000),
            analysis_limit: core::cell::Cell::new(0),
            busy_timeout: core::cell::Cell::new(0),
            secure_delete: core::cell::Cell::new(0),
            functions: alloc::collections::BTreeMap::new(),
            aggregates: alloc::collections::BTreeMap::new(),
            #[cfg(feature = "fts5")]
            fts5_rank: core::cell::RefCell::new(None),
            use_vdbe: core::cell::Cell::new(true),
        })
    }

    /// Open an existing database for reading and writing through `vfs`. Creates
    /// (and recovers from) a `<path>-journal` companion file.
    pub fn open_vfs(vfs: &dyn Vfs, path: &str) -> Result<Connection> {
        let main = vfs.open(path, OpenFlags::READ_WRITE)?;
        let journal = vfs.open(&journal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        let wal = vfs.open(&wal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        let mut c = Connection::from_pager(WritePager::open_wal(main, Some(journal), Some(wal))?)?;
        c.main_file = path.to_string();
        Ok(c)
    }

    /// Open an existing database read-only through `vfs`. If a `<path>-wal` file
    /// is present, its committed frames are overlaid so WAL-mode databases read
    /// correctly.
    pub fn open_readonly_vfs(vfs: &dyn Vfs, path: &str) -> Result<Connection> {
        let main = vfs.open(path, OpenFlags::READ_ONLY)?;
        let wal_path = wal_path(path);
        if vfs.exists(&wal_path)? {
            let mut wal = vfs.open(&wal_path, OpenFlags::READ_ONLY)?;
            let reader = crate::pager::WalReader::open(main, wal.as_mut())?;
            let mut c = Connection::from_read_backend(Box::new(reader))?;
            c.main_file = path.to_string();
            return Ok(c);
        }
        let mut c = Connection::from_read_backend(Box::new(WritePager::open(main, None)?))?;
        c.main_file = path.to_string();
        Ok(c)
    }

    /// Create a new, empty database through `vfs`.
    pub fn create_vfs(vfs: &dyn Vfs, path: &str, page_size: u32) -> Result<Connection> {
        let main = vfs.open(path, OpenFlags::READ_WRITE_CREATE)?;
        let journal = vfs.open(&journal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        let wal = vfs.open(&wal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        let mut db = WritePager::create_wal(main, Some(journal), Some(wal), page_size)?;
        db.commit()?;
        let mut c = Connection::from_pager(db)?;
        c.main_file = path.to_string();
        Ok(c)
    }

    /// Open an existing database file for reading and writing (requires `std`).
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    pub fn open(path: &str) -> Result<Connection> {
        Connection::open_vfs(&crate::vfs::std_file::StdVfs::new(), path)
    }

    /// Open an existing database file read-only (requires `std`).
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    pub fn open_readonly(path: &str) -> Result<Connection> {
        Connection::open_readonly_vfs(&crate::vfs::std_file::StdVfs::new(), path)
    }

    /// Create a new database file with the default 4096-byte page size (`std`).
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    pub fn create(path: &str) -> Result<Connection> {
        Connection::create_vfs(&crate::vfs::std_file::StdVfs::new(), path, 4096)
    }

    /// Create a fresh in-memory database (`:memory:`), always available.
    pub fn open_memory() -> Result<Connection> {
        let vfs = crate::vfs::memory::MemoryVfs::new();
        let main = vfs.open("main", OpenFlags::READ_WRITE_CREATE)?;
        let mut db = WritePager::create(main, None, 4096)?;
        db.commit()?;
        Connection::from_pager(db)
    }

    /// The schema catalog.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Run a single `SELECT` and return all rows.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        self.query_params(sql, &Params::default())
    }

    /// Run `sql` through the experimental VDBE engine instead of the tree-walker.
    /// Supports constant projections and plain single-table scans
    /// (`SELECT <exprs> FROM <table>` with no `WHERE`/joins/aggregates/`ORDER BY`);
    /// returns `Unsupported` otherwise so callers can fall back to
    /// [`query`](Self::query).
    pub fn query_vdbe(&self, sql: &str) -> Result<QueryResult> {
        let Statement::Select(sel) = sql::parse_one(sql)? else {
            return Err(Error::Unsupported("query_vdbe expects SELECT"));
        };
        self.run_select_vdbe(&sel)
    }

    /// Enable or disable the VDBE engine for `SELECT` (Track B). When on (the
    /// default), [`query`](Self::query) runs through the VDBE and falls back
    /// transparently to the tree-walker for any query shape it does not handle;
    /// turning it off forces the tree-walker. The result is identical either way.
    pub fn set_use_vdbe(&self, on: bool) {
        self.use_vdbe.set(on);
    }

    /// Compile a `SELECT` to a VDBE program *without running it*, gathering only
    /// the schema (column names / qualifiers / affinities) it needs — no row
    /// scan. Used by plain `EXPLAIN`. Covers the constant and single-table cases;
    /// joins and other shapes return `Unsupported`.
    fn compile_select_program(&self, sel: &Select) -> Result<vdbe::Program> {
        let Some(from) = &sel.from else {
            return vdbe::compile_const_select(sel);
        };
        if !from.joins.is_empty() {
            return Err(Error::Unsupported(
                "EXPLAIN: VDBE join programs not yet listed",
            ));
        }
        if from.first.subquery.is_some() || from.first.tvf_args.is_some() {
            return Err(Error::Unsupported("EXPLAIN: only plain table sources"));
        }
        let meta = self.table_meta(&from.first.name, from.first.alias.as_deref())?;
        let cols: Vec<String> = meta.columns.iter().map(|c| c.name.clone()).collect();
        let qualifier = from
            .first
            .alias
            .clone()
            .unwrap_or_else(|| from.first.name.clone());
        let tables: Vec<String> = meta.columns.iter().map(|_| qualifier.clone()).collect();
        let affinities: Vec<eval::Affinity> = meta.columns.iter().map(|c| c.affinity).collect();
        let collations: Vec<crate::value::Collation> =
            meta.columns.iter().map(|c| c.collation).collect();
        // A rowid table can carry `rowid`/`_rowid_`/`oid` references; expose the
        // hidden rowid slot so EXPLAIN compiles the same program execution uses.
        vdbe::compile_table_select(
            sel,
            &cols,
            &tables,
            &affinities,
            &collations,
            !meta.without_rowid,
        )
    }

    /// Plain `EXPLAIN <select>` (Track B, B8): compile the query to graphite's
    /// VDBE bytecode and return the program listing as `(addr, opcode, detail)`
    /// rows. Returns `Unsupported` for a query shape the VDBE cannot compile.
    fn explain_bytecode(&self, stmt: &Statement) -> Result<QueryResult> {
        let Statement::Select(sel) = stmt else {
            return Err(Error::Unsupported(
                "EXPLAIN: only SELECT is compiled to bytecode",
            ));
        };
        let prog = self.compile_select_program(sel)?;
        let rows = prog
            .explain_rows()
            .into_iter()
            .map(|(addr, opcode, detail)| {
                alloc::vec![
                    Value::Integer(addr as i64),
                    Value::Text(opcode),
                    Value::Text(detail),
                ]
            })
            .collect();
        Ok(QueryResult {
            columns: alloc::vec!["addr".into(), "opcode".into(), "detail".into()],
            rows,
        })
    }

    /// Rewrite `sel`'s top-level expressions, replacing every provably
    /// non-correlated scalar or `EXISTS` subquery with the constant it evaluates
    /// to. Returns `Some(rewritten)` when at least one subquery was folded, or
    /// `None` when there was nothing to fold (the caller keeps the original).
    ///
    /// Only the *top-level* expression positions are touched — a subquery that is
    /// itself a `FROM` source is its own scope and is materialized separately. A
    /// subquery is folded only when [`Self::vdbe_subquery_foldable`] proves it is
    /// self-contained; everything else is left untouched, so the result is never
    /// affected (the compiler simply falls back when an unfoldable subquery
    /// remains).
    fn fold_vdbe_subqueries(&self, sel: &Select) -> Option<Select> {
        let mut changed = false;
        let mut out = sel.clone();
        for rc in &mut out.columns {
            if let sql::ast::ResultColumn::Expr { expr, .. } = rc {
                *expr = self.fold_subquery_expr(expr, &mut changed);
            }
        }
        if let Some(w) = out.where_clause.take() {
            out.where_clause = Some(self.fold_subquery_expr(&w, &mut changed));
        }
        if let Some(h) = out.having.take() {
            out.having = Some(self.fold_subquery_expr(&h, &mut changed));
        }
        for g in &mut out.group_by {
            *g = self.fold_subquery_expr(g, &mut changed);
        }
        for o in &mut out.order_by {
            o.expr = self.fold_subquery_expr(&o.expr, &mut changed);
        }
        if let Some(from) = &mut out.from {
            for j in &mut from.joins {
                if let Some(on) = j.on.take() {
                    j.on = Some(self.fold_subquery_expr(&on, &mut changed));
                }
            }
        }
        if changed {
            Some(out)
        } else {
            None
        }
    }

    /// Recursively rebuild `e`, folding any foldable scalar/`EXISTS` subquery into
    /// a literal and otherwise descending into sub-expressions. A subquery that is
    /// not foldable is left in place (so the VDBE compiler still falls back).
    fn fold_subquery_expr(&self, e: &Expr, changed: &mut bool) -> Expr {
        use sql::ast::Expr as E;
        match e {
            E::Subquery(sel2) => match self.eval_foldable_scalar(sel2) {
                Some(v) => {
                    *changed = true;
                    E::Literal(value_to_literal(v))
                }
                None => e.clone(),
            },
            E::Exists { select, negated } => match self.eval_foldable_exists(select) {
                Some(found) => {
                    *changed = true;
                    E::Literal(Literal::Integer((found ^ *negated) as i64))
                }
                None => e.clone(),
            },
            E::Unary { op, expr } => E::Unary {
                op: *op,
                expr: alloc::boxed::Box::new(self.fold_subquery_expr(expr, changed)),
            },
            E::Binary { op, left, right } => E::Binary {
                op: *op,
                left: alloc::boxed::Box::new(self.fold_subquery_expr(left, changed)),
                right: alloc::boxed::Box::new(self.fold_subquery_expr(right, changed)),
            },
            E::IsNull { expr, negated } => E::IsNull {
                expr: alloc::boxed::Box::new(self.fold_subquery_expr(expr, changed)),
                negated: *negated,
            },
            E::InList {
                expr,
                list,
                negated,
            } => E::InList {
                expr: alloc::boxed::Box::new(self.fold_subquery_expr(expr, changed)),
                list: list
                    .iter()
                    .map(|x| self.fold_subquery_expr(x, changed))
                    .collect(),
                negated: *negated,
            },
            E::Between {
                expr,
                low,
                high,
                negated,
            } => E::Between {
                expr: alloc::boxed::Box::new(self.fold_subquery_expr(expr, changed)),
                low: alloc::boxed::Box::new(self.fold_subquery_expr(low, changed)),
                high: alloc::boxed::Box::new(self.fold_subquery_expr(high, changed)),
                negated: *negated,
            },
            E::Case {
                operand,
                when_then,
                else_result,
            } => E::Case {
                operand: operand
                    .as_ref()
                    .map(|o| alloc::boxed::Box::new(self.fold_subquery_expr(o, changed))),
                when_then: when_then
                    .iter()
                    .map(|(w, t)| {
                        (
                            self.fold_subquery_expr(w, changed),
                            self.fold_subquery_expr(t, changed),
                        )
                    })
                    .collect(),
                else_result: else_result
                    .as_ref()
                    .map(|x| alloc::boxed::Box::new(self.fold_subquery_expr(x, changed))),
            },
            E::Cast { expr, type_name } => E::Cast {
                expr: alloc::boxed::Box::new(self.fold_subquery_expr(expr, changed)),
                type_name: type_name.clone(),
            },
            E::Paren(inner) => E::Paren(alloc::boxed::Box::new(
                self.fold_subquery_expr(inner, changed),
            )),
            E::Collate { expr, collation } => E::Collate {
                expr: alloc::boxed::Box::new(self.fold_subquery_expr(expr, changed)),
                collation: collation.clone(),
            },
            E::RowValue(items) => E::RowValue(
                items
                    .iter()
                    .map(|x| self.fold_subquery_expr(x, changed))
                    .collect(),
            ),
            // A function call: fold within ordinary arguments and the `FILTER`
            // predicate. A windowed call (`OVER (…)`) is left untouched (its frame
            // exprs are not in the VDBE's grammar anyway).
            E::Function {
                name,
                distinct,
                args,
                star,
                filter,
                order_by,
                over,
            } if over.is_none() => E::Function {
                name: name.clone(),
                distinct: *distinct,
                args: args
                    .iter()
                    .map(|a| self.fold_subquery_expr(a, changed))
                    .collect(),
                star: *star,
                filter: filter
                    .as_ref()
                    .map(|f| alloc::boxed::Box::new(self.fold_subquery_expr(f, changed))),
                order_by: order_by.clone(),
                over: None,
            },
            // Literals, parameters, columns, `IN (SELECT …)`, windowed calls: no
            // top-level scalar/EXISTS subquery to fold here. (`IN (SELECT)` is
            // deliberately not folded — its candidate-set comparison uses the
            // subquery column's collation/affinity, which `IN (list)` would not.)
            _ => e.clone(),
        }
    }

    /// Evaluate a scalar subquery to its constant value when it is foldable, else
    /// `None`. Foldable means [`Self::vdbe_subquery_foldable`] (self-contained) AND
    /// the single result column is a *computed* expression, not a bare column
    /// reference — so the resulting literal has the same NONE affinity / BINARY
    /// collation the subquery operand would have had, making the substitution
    /// exact for the enclosing comparison.
    fn eval_foldable_scalar(&self, sel2: &Select) -> Option<Value> {
        if !self.vdbe_subquery_foldable(sel2) {
            return None;
        }
        if sel2.columns.len() != 1 {
            return None;
        }
        let sql::ast::ResultColumn::Expr { expr, .. } = &sel2.columns[0] else {
            return None;
        };
        if is_bare_column_expr(expr) {
            return None;
        }
        let r = self.run_select(sel2, &Params::default()).ok()?;
        Some(
            r.rows
                .first()
                .and_then(|row| row.first())
                .cloned()
                .unwrap_or(Value::Null),
        )
    }

    /// Evaluate `EXISTS (sel2)` to a constant truth value when `sel2` is
    /// self-contained (non-correlated), else `None`.
    fn eval_foldable_exists(&self, sel2: &Select) -> Option<bool> {
        if !self.vdbe_subquery_foldable(sel2) {
            return None;
        }
        let r = self.run_select(sel2, &Params::default()).ok()?;
        Some(!r.rows.is_empty())
    }

    /// Conservatively decide whether `sel2` is self-contained — i.e. references no
    /// column outside its own `FROM` sources (non-correlated), takes no bound
    /// parameter, and contains no further nested subquery. Such a query yields the
    /// same value evaluated in isolation as it would in any outer row, so its
    /// result can be folded to a constant. Bails (returns `false`) on anything it
    /// cannot prove: compound/CTE bodies, non-base-table sources, etc.
    fn vdbe_subquery_foldable(&self, sel2: &Select) -> bool {
        if !sel2.compound.is_empty() || !sel2.ctes.is_empty() {
            return false;
        }
        let Some(from) = &sel2.from else {
            // A `FROM`-less scalar (`(SELECT 1)`) is trivially constant, but only
            // if it carries no column reference at all.
            return self.expr_positions_internal(sel2, &[], &[]);
        };
        // Collect the source qualifiers and the union of their columns; every
        // source must be a plain base table so the column set is known.
        let mut quals: Vec<String> = Vec::new();
        let mut cols: Vec<String> = Vec::new();
        let mut collect = |tr: &sql::ast::TableRef| -> bool {
            if tr.subquery.is_some()
                || tr.tvf_args.is_some()
                || tr.schema.is_some()
                || tr.name.is_empty()
            {
                return false;
            }
            let Ok(meta) = self.table_meta(&tr.name, None) else {
                return false;
            };
            quals.push(tr.name.clone());
            if let Some(a) = &tr.alias {
                quals.push(a.clone());
            }
            for c in &meta.columns {
                cols.push(c.name.clone());
            }
            true
        };
        if !collect(&from.first) {
            return false;
        }
        for j in &from.joins {
            if !collect(&j.table) {
                return false;
            }
        }
        self.expr_positions_internal(sel2, &quals, &cols)
    }

    /// True when every column reference in every top-level expression of `sel2`
    /// resolves to one of `quals`/`cols` (its own sources) and no expression
    /// contains a parameter or a nested subquery — see [`expr_is_internal`].
    fn expr_positions_internal(&self, sel2: &Select, quals: &[String], cols: &[String]) -> bool {
        let ok = |e: &Expr| expr_is_internal(e, quals, cols);
        for rc in &sel2.columns {
            if let sql::ast::ResultColumn::Expr { expr, .. } = rc {
                if !ok(expr) {
                    return false;
                }
            }
        }
        if let Some(w) = &sel2.where_clause {
            if !ok(w) {
                return false;
            }
        }
        if let Some(h) = &sel2.having {
            if !ok(h) {
                return false;
            }
        }
        if !sel2.group_by.iter().all(&ok) {
            return false;
        }
        if !sel2.order_by.iter().all(|t| ok(&t.expr)) {
            return false;
        }
        if let Some(from) = &sel2.from {
            for j in &from.joins {
                if let Some(on) = &j.on {
                    if !ok(on) {
                        return false;
                    }
                }
            }
        }
        if let Some(l) = &sel2.limit {
            if !ok(l) {
                return false;
            }
        }
        if let Some(o) = &sel2.offset {
            if !ok(o) {
                return false;
            }
        }
        true
    }

    /// Compile and run a parsed `SELECT` through the VDBE engine, or `Unsupported`
    /// when its shape is outside the spike's grammar (so callers fall back).
    fn run_select_vdbe(&self, sel: &Select) -> Result<QueryResult> {
        // The VDBE resolves table names in the `main` schema only
        // (`table_meta`). Whenever an attached or `temp` database is in scope, or
        // a non-main database is the current resolution default, or a source is
        // schema-qualified, defer to the tree-walker so the right schema is used.
        if self.temp_db.is_some()
            || !self.attached.is_empty()
            || self.read_default.get() != DbRef::Main
        {
            return Err(Error::Unsupported("VDBE: non-main schema in scope"));
        }
        if let Some(f) = &sel.from {
            if f.first.schema.is_some() || f.joins.iter().any(|j| j.table.schema.is_some()) {
                return Err(Error::Unsupported("VDBE: schema-qualified source"));
            }
        }
        // When the tree-walker satisfies `ORDER BY` via an index/rowid/seek scan,
        // its tie/NULL order follows that (possibly reversed) scan; the VDBE
        // sorter would emit a different — valid, but SQL-unspecified — tie order.
        // Defer such queries to the tree-walker so the observable order matches.
        if sel.from.is_some()
            && !sel.order_by.is_empty()
            && self
                .order_satisfied_by_scan(sel, &eval::Params::default())
                .is_some()
        {
            return Err(Error::Unsupported("VDBE: ORDER BY satisfied by a scan"));
        }
        // Fold provably non-correlated scalar / `EXISTS` subqueries that appear in
        // the top-level expressions to the constant they evaluate to, so the VDBE
        // (which cannot open a cursor for a nested query) can run the rest. Only
        // self-contained subqueries are folded; anything correlated, parameterized,
        // or itself containing a nested subquery is left in place and the compiler
        // falls back as before — so this only widens what the VDBE accepts, never
        // changes a result.
        let folded;
        let sel = match self.fold_vdbe_subqueries(sel) {
            Some(s) => {
                folded = s;
                &folded
            }
            None => sel,
        };
        // Constant SELECT (no FROM): compile and run directly.
        let Some(from) = &sel.from else {
            let prog = vdbe::compile_const_select(sel)?;
            let rows = vdbe::run(&prog)?;
            return Ok(QueryResult {
                columns: prog.columns,
                rows,
            });
        };
        // Materialize a plain table source's column names and rows. Subqueries
        // and table-valued functions are out of the spike's scope.
        // (column names, owning-table qualifier, affinities, collations, rows, and
        // the per-row rowids — `None` for a `WITHOUT ROWID` table, which has none).
        type ScanOut = (
            Vec<String>,
            Vec<String>,
            Vec<eval::Affinity>,
            Vec<crate::value::Collation>,
            Vec<Vec<Value>>,
            Option<Vec<i64>>,
        );
        let scan_one = |tr: &sql::ast::TableRef| -> Result<ScanOut> {
            // A derived table (FROM subquery), conservatively: a single-block
            // subquery over a single all-BINARY base table. Then every output
            // column has BINARY collation and its affinity comes from the resolved
            // output type — so the materialized rows compare in the outer query
            // exactly like the tree-walker. Anything else defers.
            if let Some(sub) = &tr.subquery {
                if tr.tvf_args.is_some() || !sub.compound.is_empty() {
                    return Err(Error::Unsupported("VDBE: complex subquery source"));
                }
                let sfrom = sub
                    .from
                    .as_ref()
                    .ok_or(Error::Unsupported("VDBE: subquery source without FROM"))?;
                if !sfrom.joins.is_empty()
                    || sfrom.first.subquery.is_some()
                    || sfrom.first.tvf_args.is_some()
                    || sfrom.first.index_hint.is_some()
                    || sfrom.first.schema.is_some()
                    || self.schema.table(&sfrom.first.name).is_none()
                {
                    return Err(Error::Unsupported("VDBE: complex subquery source"));
                }
                let base = self.table_meta(&sfrom.first.name, None)?;
                if base
                    .columns
                    .iter()
                    .any(|c| c.collation != crate::value::Collation::default())
                {
                    return Err(Error::Unsupported(
                        "VDBE: subquery over a non-BINARY column",
                    ));
                }
                let named = self
                    .resolved_view_columns(sub)
                    .ok_or(Error::Unsupported("VDBE: subquery columns unresolved"))?;
                let result = self.run_select(sub, &eval::Params::default())?;
                if result.columns.len() != named.len() {
                    return Err(Error::Unsupported("VDBE: subquery column count mismatch"));
                }
                let qualifier = tr.alias.clone().unwrap_or_default();
                let tables = result.columns.iter().map(|_| qualifier.clone()).collect();
                let affinities = named
                    .iter()
                    .map(|(_, t)| eval::Affinity::from_type(t.as_deref()))
                    .collect();
                let collations = result
                    .columns
                    .iter()
                    .map(|_| crate::value::Collation::default())
                    .collect();
                return Ok((
                    result.columns,
                    tables,
                    affinities,
                    collations,
                    result.rows,
                    None,
                ));
            }
            if tr.tvf_args.is_some() {
                return Err(Error::Unsupported("VDBE: only plain table sources"));
            }
            // The VDBE always full-scans and does not model `INDEXED BY`/`NOT
            // INDEXED` (and an invalid `INDEXED BY` must error); defer to the
            // tree-walker so the hint is honoured or rejected.
            if tr.index_hint.is_some() {
                return Err(Error::Unsupported("VDBE: index hint"));
            }
            let meta = self.table_meta(&tr.name, tr.alias.as_deref())?;
            let cols = meta.columns.iter().map(|c| c.name.clone()).collect();
            let collations = meta.columns.iter().map(|c| c.collation).collect();
            // The qualifier a `t.col` reference must use: the alias if present,
            // else the table name.
            let qualifier = tr.alias.clone().unwrap_or_else(|| tr.name.clone());
            let tables = meta.columns.iter().map(|_| qualifier.clone()).collect();
            let affinities = meta.columns.iter().map(|c| c.affinity).collect();
            let (rows, rowids): (Vec<Vec<Value>>, Option<Vec<i64>>) = if meta.without_rowid {
                (self.scan_without_rowid(&meta)?, None)
            } else {
                let scanned = self.scan_table(&meta)?;
                let ids = scanned.iter().map(|(r, _)| *r).collect();
                (scanned.into_iter().map(|(_, v)| v).collect(), Some(ids))
            };
            Ok((cols, tables, affinities, collations, rows, rowids))
        };

        // A single RIGHT / FULL outer join (two tables). SQLite drives both from the
        // LEFT table: emit the matched pairs (left-table order, right matches in
        // right order), for FULL also null-extend an unmatched left row, and then —
        // for both RIGHT and FULL — append every right row that matched nothing as
        // a null-left row. (RIGHT differs from FULL only in dropping unmatched-left
        // rows.) Only the final WHERE is handed to the VDBE. Chains containing
        // RIGHT/FULL, NATURAL/USING, and `t.*` bail (→ tree-walker).
        if from.joins.len() == 1
            && matches!(
                from.joins[0].kind,
                sql::ast::JoinKind::Right | sql::ast::JoinKind::Full
            )
        {
            let j = &from.joins[0];
            if j.natural || !j.using.is_empty() {
                return Err(Error::Unsupported("VDBE: NATURAL/USING outer join"));
            }
            // `t.*` over a join expands by qualifier inside `compile_table_select`.
            let is_full = matches!(j.kind, sql::ast::JoinKind::Full);
            let left = scan_one(&from.first)?;
            let right = scan_one(&j.table)?;
            // Combined schema: left columns then right columns.
            let mut names = left.0.clone();
            names.extend(right.0.iter().cloned());
            let mut tabs = left.1.clone();
            tabs.extend(right.1.iter().cloned());
            let mut affs = left.2.clone();
            affs.extend(right.2.iter().copied());
            let mut colls = left.3.clone();
            colls.extend(right.3.iter().copied());
            let cinfos: Vec<ColumnInfo> = (0..names.len())
                .map(|i| ColumnInfo {
                    name: names[i].clone(),
                    table: tabs[i].clone(),
                    affinity: affs[i],
                    collation: colls[i],
                })
                .collect();
            let on_params = eval::Params::default();
            let lw = left.0.len();
            let rw = right.0.len();
            let mut matched_right = alloc::vec![false; right.4.len()];
            let mut rows: Vec<Vec<Value>> = Vec::new();
            for a in &left.4 {
                let mut any = false;
                for (rj, b) in right.4.iter().enumerate() {
                    let mut row = a.clone();
                    row.extend(b.iter().cloned());
                    let keep = match &j.on {
                        Some(p) => {
                            let ir = InputRow {
                                values: row.clone(),
                                rowid: None,
                            };
                            let ctx = ir.ctx(&cinfos, &on_params).with_subqueries(self);
                            eval::truth(&eval::eval(p, &ctx)?) == Some(true)
                        }
                        None => true,
                    };
                    if keep {
                        rows.push(row);
                        matched_right[rj] = true;
                        any = true;
                    }
                }
                // FULL keeps an unmatched left row (null-extended); RIGHT drops it.
                if is_full && !any {
                    let mut row = a.clone();
                    row.extend(core::iter::repeat_n(Value::Null, rw));
                    rows.push(row);
                }
            }
            // Both RIGHT and FULL: append every unmatched right row (null left).
            for (rj, b) in right.4.iter().enumerate() {
                if !matched_right[rj] {
                    let mut row = alloc::vec![Value::Null; lw];
                    row.extend(b.iter().cloned());
                    rows.push(row);
                }
            }
            let prog = vdbe::compile_table_select(sel, &names, &tabs, &affs, &colls, false)?;
            let result = vdbe::run_rows(&prog, &rows)?;
            return Ok(QueryResult {
                columns: prog.columns,
                rows: result,
            });
        }

        // LEFT join(s): a filtered cross-product cannot model the NULL-extension of
        // unmatched left rows, so when any join is `LEFT` build the joined rows by a
        // real nested loop — for each accumulated left row, emit a row per right
        // match (`ON` evaluated against the partial combined row) and, if none
        // matched, one row padded with NULLs for the right table. Only the *final*
        // `WHERE` is then handed to the VDBE (the `ON`s are already applied). Pure
        // INNER-join queries keep the proven cross-product path below. Bails (→
        // tree-walker) on RIGHT/FULL/NATURAL/USING and `t.*`.
        if from
            .joins
            .iter()
            .any(|j| matches!(j.kind, sql::ast::JoinKind::Left))
        {
            if from.joins.iter().any(|j| {
                !matches!(j.kind, sql::ast::JoinKind::Inner | sql::ast::JoinKind::Left)
                    || j.natural
                    || !j.using.is_empty()
            }) {
                return Err(Error::Unsupported("VDBE: only INNER/LEFT joins"));
            }
            // `t.*` over a join expands by qualifier inside `compile_table_select`.
            // The VDBE path is param-less (explicit params were substituted
            // upstream); evaluate each ON against an empty parameter set.
            let on_params = eval::Params::default();
            let first = scan_one(&from.first)?;
            let mut names = first.0;
            let mut tabs = first.1;
            let mut affs = first.2;
            let mut colls = first.3;
            let mut rows: Vec<Vec<Value>> = first.4;
            for j in &from.joins {
                let src = scan_one(&j.table)?;
                // Combined schema after adding this source (for ON resolution).
                let mut n_names = names.clone();
                n_names.extend(src.0.iter().cloned());
                let mut n_tabs = tabs.clone();
                n_tabs.extend(src.1.iter().cloned());
                let mut n_affs = affs.clone();
                n_affs.extend(src.2.iter().copied());
                let mut n_colls = colls.clone();
                n_colls.extend(src.3.iter().copied());
                let cinfos: Vec<ColumnInfo> = (0..n_names.len())
                    .map(|i| ColumnInfo {
                        name: n_names[i].clone(),
                        table: n_tabs[i].clone(),
                        affinity: n_affs[i],
                        collation: n_colls[i],
                    })
                    .collect();
                let is_left = matches!(j.kind, sql::ast::JoinKind::Left);
                let width = src.0.len();
                let mut next: Vec<Vec<Value>> = Vec::new();
                for a in &rows {
                    let mut matched = false;
                    for b in &src.4 {
                        let mut row = a.clone();
                        row.extend(b.iter().cloned());
                        let keep = match &j.on {
                            Some(p) => {
                                let ir = InputRow {
                                    values: row.clone(),
                                    rowid: None,
                                };
                                let ctx = ir.ctx(&cinfos, &on_params).with_subqueries(self);
                                eval::truth(&eval::eval(p, &ctx)?) == Some(true)
                            }
                            None => true,
                        };
                        if keep {
                            next.push(row);
                            matched = true;
                        }
                    }
                    if is_left && !matched {
                        let mut row = a.clone();
                        row.extend(core::iter::repeat_n(Value::Null, width));
                        next.push(row);
                    }
                }
                rows = next;
                names = n_names;
                tabs = n_tabs;
                affs = n_affs;
                colls = n_colls;
            }
            let prog = vdbe::compile_table_select(sel, &names, &tabs, &affs, &colls, false)?;
            let result = vdbe::run_rows(&prog, &rows)?;
            return Ok(QueryResult {
                columns: prog.columns,
                rows: result,
            });
        }

        // Inner join(s) (B5a): an inner join is a filtered cross-product, so
        // materialize `t1 × t2 × … × tN` (leftmost source outermost, matching the
        // tree-walker's and sqlite's nested-loop row order), fold every `ON` into
        // the `WHERE`, and reuse the single-cursor scan compiler. Every join must
        // be a plain `INNER`/`CROSS`/comma join (no `NATURAL`/`USING`/outer).
        if !from.joins.is_empty() {
            if from
                .joins
                .iter()
                .any(|j| j.kind != sql::ast::JoinKind::Inner || j.natural || !j.using.is_empty())
            {
                return Err(Error::Unsupported("VDBE: only plain inner joins"));
            }
            // `t.*` over a join expands by qualifier inside `compile_table_select`.
            // Scan every source (the first table, then each joined table) in
            // declaration order.
            let mut sources = alloc::vec![scan_one(&from.first)?];
            for j in &from.joins {
                sources.push(scan_one(&j.table)?);
            }
            // Combined schema = each source's columns concatenated in order. Shared
            // bare names are allowed: a qualified `t.col` disambiguates them, and an
            // ambiguous *bare* reference makes the compiler bail (→ tree-walker).
            let mut combined: Vec<String> = Vec::new();
            let mut combined_tables: Vec<String> = Vec::new();
            let mut combined_aff: Vec<eval::Affinity> = Vec::new();
            let mut combined_coll: Vec<crate::value::Collation> = Vec::new();
            for (c, t, a, l, _, _) in &sources {
                combined.extend(c.iter().cloned());
                combined_tables.extend(t.iter().cloned());
                combined_aff.extend(a.iter().copied());
                combined_coll.extend(l.iter().copied());
            }
            // N-way cross-product, leftmost source outermost.
            let mut rows: Vec<Vec<Value>> = sources[0].4.clone();
            for src in &sources[1..] {
                let mut next = Vec::with_capacity(rows.len().saturating_mul(src.4.len()));
                for a in &rows {
                    for b in &src.4 {
                        let mut row = a.clone();
                        row.extend(b.iter().cloned());
                        next.push(row);
                    }
                }
                rows = next;
            }
            // Merge the existing WHERE with every join's ON predicate (AND).
            let mut merged = sel.where_clause.clone();
            for j in &from.joins {
                if let Some(on) = &j.on {
                    merged = Some(match merged {
                        Some(w) => sql::ast::Expr::Binary {
                            op: sql::ast::BinaryOp::And,
                            left: alloc::boxed::Box::new(w),
                            right: alloc::boxed::Box::new(on.clone()),
                        },
                        None => on.clone(),
                    });
                }
            }
            let mut joined = sel.clone();
            joined.where_clause = merged;
            let prog = vdbe::compile_table_select(
                &joined,
                &combined,
                &combined_tables,
                &combined_aff,
                &combined_coll,
                // rowid over a join is ambiguous across tables; not modeled here.
                false,
            )?;
            let result = vdbe::run_rows(&prog, &rows)?;
            return Ok(QueryResult {
                columns: prog.columns,
                rows: result,
            });
        }

        // Single source — a plain table or a derived table (`scan_one` materializes
        // a safe FROM subquery; a table-valued function is out of scope).
        if from.first.tvf_args.is_some() {
            return Err(Error::Unsupported("VDBE: table-valued function source"));
        }
        let (col_names, col_tables, col_aff, col_coll, mut rows, rowids) = scan_one(&from.first)?;
        // Append each row's rowid as a hidden trailing value so a `rowid`/`_rowid_`/
        // `oid` reference resolves (a `WITHOUT ROWID` table has none → `rowids` is
        // `None`, and such references fall back to the tree-walker, which errors).
        let has_rowid = rowids.is_some();
        if let Some(ids) = rowids {
            for (row, id) in rows.iter_mut().zip(ids) {
                row.push(Value::Integer(id));
            }
        }
        // A `t.*` projection is only handled when its qualifier names this single
        // table (by name or alias); any other qualifier falls back so the
        // tree-walker can resolve or reject it.
        for rc in &sel.columns {
            if let sql::ast::ResultColumn::TableWildcard(q) = rc {
                let matches = q.eq_ignore_ascii_case(&from.first.name)
                    || from
                        .first
                        .alias
                        .as_deref()
                        .is_some_and(|a| q.eq_ignore_ascii_case(a));
                if !matches {
                    return Err(Error::Unsupported("VDBE: unknown table.* qualifier"));
                }
            }
        }
        let prog = vdbe::compile_table_select(
            sel,
            &col_names,
            &col_tables,
            &col_aff,
            &col_coll,
            has_rowid,
        )?;
        let result = vdbe::run_rows(&prog, &rows)?;
        Ok(QueryResult {
            columns: prog.columns,
            rows: result,
        })
    }

    /// Like [`query`](Self::query) but with bound parameters.
    pub fn query_params(&self, sql: &str, params: &Params) -> Result<QueryResult> {
        match sql::parse_one(sql)? {
            Statement::Select(sel) => self.run_select(&sel, params),
            Statement::Pragma(p) => self.run_pragma(&p),
            Statement::Explain { query_plan, stmt } => {
                if query_plan {
                    self.explain_query_plan(&stmt, params)
                } else {
                    self.explain_bytecode(&stmt)
                }
            }
            _ => Err(Error::Unsupported(
                "use execute() for non-SELECT statements",
            )),
        }
    }

    /// Evaluate the read-only `PRAGMA`s that return a result set.
    fn run_pragma(&self, p: &Pragma) -> Result<QueryResult> {
        let name = p.name.to_ascii_lowercase();
        let header = self.backend.source().header();
        let single = |col: &str, v: Value| QueryResult {
            columns: alloc::vec![String::from(col)],
            rows: alloc::vec![alloc::vec![v]],
        };
        match name.as_str() {
            "page_size" => Ok(single("page_size", Value::Integer(header.page_size as i64))),
            "page_count" => Ok(single(
                "page_count",
                Value::Integer(self.backend.source().page_count() as i64),
            )),
            "user_version" => Ok(single(
                // Stored as a 32-bit value; SQLite reports it signed.
                "user_version",
                Value::Integer(header.user_version as i32 as i64),
            )),
            "schema_version" => Ok(single(
                "schema_version",
                Value::Integer(header.schema_cookie as i64),
            )),
            "encoding" => Ok(single(
                "encoding",
                Value::Text(
                    match header.text_encoding {
                        crate::format::TextEncoding::Utf8 => "UTF-8",
                        crate::format::TextEncoding::Utf16Le => "UTF-16le",
                        crate::format::TextEncoding::Utf16Be => "UTF-16be",
                    }
                    .into(),
                ),
            )),
            "freelist_count" => Ok(single(
                "freelist_count",
                Value::Integer(header.freelist_count as i64),
            )),
            // 0 = NONE, 1 = FULL, 2 = INCREMENTAL. Auto-vacuum is on when the
            // header's largest-root-page field is non-zero; the incremental flag
            // then distinguishes the two modes.
            "auto_vacuum" => Ok(single(
                "auto_vacuum",
                Value::Integer(auto_vacuum_mode(header) as i64),
            )),
            "application_id" => Ok(single(
                "application_id",
                Value::Integer(header.application_id as i32 as i64),
            )),
            "data_version" => Ok(single("data_version", Value::Integer(1))),
            "table_info" => self.pragma_table_info(p, false),
            "table_xinfo" => self.pragma_table_info(p, true),
            "index_list" => self.pragma_index_list(p),
            "index_info" => self.pragma_index_info(p, false),
            "index_xinfo" => self.pragma_index_info(p, true),
            "database_list" => Ok(self.pragma_database_list()),
            "table_list" => self.pragma_table_list(p),
            // The collating sequences graphite implements (built-ins only; it
            // registers no custom collations).
            "collation_list" => Ok(QueryResult {
                columns: alloc::vec!["seq".into(), "name".into()],
                rows: ["RTRIM", "NOCASE", "BINARY"]
                    .iter()
                    .enumerate()
                    .map(|(i, n)| alloc::vec![Value::Integer(i as i64), Value::Text((*n).into())])
                    .collect(),
            }),
            "foreign_key_list" => self.pragma_foreign_key_list(p),
            "foreign_key_check" => self.pragma_foreign_key_check(p),
            "integrity_check" | "quick_check" => self.pragma_integrity_check(),
            "foreign_keys" => Ok(single(
                "foreign_keys",
                Value::Integer(self.foreign_keys as i64),
            )),
            "recursive_triggers" => Ok(single(
                "recursive_triggers",
                Value::Integer(self.recursive_triggers as i64),
            )),
            "journal_mode" => {
                // An in-memory database (empty main file) uses the `memory`
                // journal, like sqlite; a file database defaults to `delete`.
                let mode = if self.backend.wal_mode() {
                    "wal"
                } else if self.main_file.is_empty() {
                    "memory"
                } else {
                    "delete"
                };
                Ok(single("journal_mode", Value::Text(mode.into())))
            }
            // Read-only getters for tuning knobs graphite does not expose. It
            // has no configurable page cache, durability mode, or lock manager
            // beyond what it already implements, so each reports SQLite's fixed
            // default — what an unconfigured connection observes. This keeps the
            // shell drop-in for tools/ORMs that probe these on connect.
            "cache_size" => Ok(single("cache_size", Value::Integer(self.cache_size.get()))),
            // The reference sqlite build has memory-mapped I/O disabled
            // (SQLITE_MAX_MMAP_SIZE = 0), so `PRAGMA mmap_size` yields no rows.
            "mmap_size" => Ok(QueryResult {
                columns: alloc::vec![String::from("mmap_size")],
                rows: Vec::new(),
            }),
            "synchronous" => Ok(single("synchronous", Value::Integer(2))),
            "temp_store" => Ok(single("temp_store", Value::Integer(0))),
            "secure_delete" => Ok(single(
                "secure_delete",
                Value::Integer(self.secure_delete.get()),
            )),
            "read_uncommitted" => Ok(single("read_uncommitted", Value::Integer(0))),
            "cell_size_check" => Ok(single("cell_size_check", Value::Integer(0))),
            "checkpoint_fullfsync" => Ok(single("checkpoint_fullfsync", Value::Integer(0))),
            "fullfsync" => Ok(single("fullfsync", Value::Integer(0))),
            // `busy_timeout` round-trips the lock-wait timeout (graphite never
            // blocks, so it is advisory). The set form clamps a negative value to 0
            // and echoes it; the plain form reads it back — like sqlite. The result
            // column is named "timeout".
            "busy_timeout" => {
                if let Some(e) = &p.value {
                    let v = eval::to_i64(&eval::eval(e, &EvalCtx::rowless(&Params::default()))?);
                    self.busy_timeout.set(v.max(0));
                }
                Ok(single("timeout", Value::Integer(self.busy_timeout.get())))
            }
            // `wal_checkpoint[(mode)]` — graphite never runs in WAL mode (its
            // journal is delete/memory), so a checkpoint is a no-op. sqlite still
            // returns one `(busy, log, checkpointed)` row; for a non-WAL database
            // that is always `0, -1, -1`.
            "wal_checkpoint" => Ok(QueryResult {
                columns: alloc::vec![
                    String::from("busy"),
                    String::from("log"),
                    String::from("checkpointed"),
                ],
                rows: alloc::vec![alloc::vec![
                    Value::Integer(0),
                    Value::Integer(-1),
                    Value::Integer(-1),
                ]],
            }),
            "wal_autocheckpoint" => Ok(single("wal_autocheckpoint", Value::Integer(1000))),
            "max_page_count" => Ok(single("max_page_count", Value::Integer(4294967294))),
            "locking_mode" => Ok(single("locking_mode", Value::Text("normal".into()))),
            // Recognized boolean / legacy no-op pragmas: graphite does not act on
            // them, but reports SQLite's fixed default so a probing tool/ORM sees a
            // normal connection. `legacy_file_format` and `case_sensitive_like`
            // (a setter-only spelling) yield no rows, as in SQLite.
            "legacy_file_format" | "case_sensitive_like" => Ok(QueryResult {
                columns: alloc::vec![name.clone()],
                rows: Vec::new(),
            }),
            // `analysis_limit` stores/reports the ANALYZE sample cap. The set form
            // (`PRAGMA analysis_limit = N`) clamps a negative N to 0 and echoes the
            // resulting value, exactly like sqlite; the plain form reads it back.
            "analysis_limit" => {
                if let Some(e) = &p.value {
                    let v = eval::to_i64(&eval::eval(e, &EvalCtx::rowless(&Params::default()))?);
                    self.analysis_limit.set(v.max(0));
                }
                Ok(single(
                    "analysis_limit",
                    Value::Integer(self.analysis_limit.get()),
                ))
            }
            // `optimize` runs recommended maintenance; graphite keeps its stats
            // current, so there is nothing to do and — like sqlite in its default,
            // non-verbose mode — it returns no rows.
            "optimize" => Ok(QueryResult {
                columns: alloc::vec![name.clone()],
                rows: Vec::new(),
            }),
            "short_column_names" | "automatic_index" => Ok(single(&name, Value::Integer(1))),
            "legacy_alter_table"
            | "count_changes"
            | "full_column_names"
            | "empty_result_callbacks"
            | "defer_foreign_keys"
            | "ignore_check_constraints"
            | "reverse_unordered_selects"
            | "query_only"
            | "writable_schema"
            | "threads"
            | "soft_heap_limit"
            | "hard_heap_limit" => Ok(single(&name, Value::Integer(0))),
            _ => Err(Error::Unsupported("this PRAGMA")),
        }
    }

    /// `PRAGMA database_list` → `(seq, name, file)` for `main`, then each
    /// attached database in attachment order. In-memory databases report an
    /// empty file path, as in SQLite.
    /// `PRAGMA table_list [(name)]`: one row per table/view across every
    /// database — `(schema, name, type, ncol, wr, strict)` — plus each
    /// database's synthetic schema table. Row order is unspecified in sqlite
    /// (hash order); we emit database order, then catalog order within each.
    fn pragma_table_list(&self, p: &Pragma) -> Result<QueryResult> {
        use crate::schema::ObjectType;
        let filter = match &p.value {
            Some(Expr::Column { column, .. }) => Some(column.clone()),
            Some(Expr::Literal(Literal::Str(s))) => Some(s.clone()),
            _ => None,
        };
        let params = Params::default();
        // (display name, which database, that database's schema-table name).
        // `temp` is always listed here (matching sqlite) even before it exists —
        // unlike `database_list`, which omits it until first use.
        let mut dbs: Vec<(String, DbRef, &str)> = alloc::vec![
            ("main".into(), DbRef::Main, "sqlite_schema"),
            ("temp".into(), DbRef::Temp, "sqlite_temp_schema"),
        ];
        for (i, d) in self.attached.iter().enumerate() {
            dbs.push((d.name.clone(), DbRef::Attached(i), "sqlite_schema"));
        }
        let matches = |n: &str| filter.as_deref().is_none_or(|f| f.eq_ignore_ascii_case(n));
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for (db_name, db, schema_tab) in &dbs {
            // `temp` may be listed before it has been created (no user objects).
            let objects: &[crate::schema::SchemaObject] =
                if matches!(db, DbRef::Temp) && self.temp_db.is_none() {
                    &[]
                } else {
                    self.db_parts(*db).0.objects()
                };
            for obj in objects {
                let typ = match obj.obj_type {
                    ObjectType::Table => "table",
                    ObjectType::View => "view",
                    _ => continue,
                };
                if !matches(&obj.name) {
                    continue;
                }
                let (ncol, wr, strict) = self.table_list_dims(*db, obj, &params);
                rows.push(alloc::vec![
                    Value::Text(db_name.clone()),
                    Value::Text(obj.name.clone()),
                    Value::Text(typ.into()),
                    Value::Integer(ncol),
                    Value::Integer(wr),
                    Value::Integer(strict),
                ]);
            }
            // The database's own schema table (also matchable as `sqlite_master`).
            if matches(schema_tab)
                || filter
                    .as_deref()
                    .is_some_and(|f| f.eq_ignore_ascii_case("sqlite_master"))
            {
                rows.push(alloc::vec![
                    Value::Text(db_name.clone()),
                    Value::Text((*schema_tab).into()),
                    Value::Text("table".into()),
                    Value::Integer(5),
                    Value::Integer(0),
                    Value::Integer(0),
                ]);
            }
        }
        Ok(QueryResult {
            columns: alloc::vec![
                "schema".into(),
                "name".into(),
                "type".into(),
                "ncol".into(),
                "wr".into(),
                "strict".into(),
            ],
            rows,
        })
    }

    /// `(ncol, wr, strict)` for one `table_list` row: a table's column count,
    /// WITHOUT ROWID flag, and STRICT flag; a view's output-column count (its
    /// `wr`/`strict` are always 0). Best-effort — an unreadable object yields 0s.
    fn table_list_dims(
        &self,
        db: DbRef,
        obj: &crate::schema::SchemaObject,
        params: &Params,
    ) -> (i64, i64, i64) {
        use crate::schema::ObjectType;
        match obj.obj_type {
            ObjectType::Table => {
                let (schema, _) = self.db_parts(db);
                match self.table_meta_in(schema, &obj.name, None) {
                    Ok(m) => (
                        m.columns.len() as i64,
                        m.without_rowid as i64,
                        m.strict_types.is_some() as i64,
                    ),
                    Err(_) => (0, 0, 0),
                }
            }
            ObjectType::View => {
                let ncol = self
                    .scan_db_view(db, &obj.name, None, params)
                    .ok()
                    .flatten()
                    .map_or(0, |(c, _)| c.len() as i64);
                (ncol, 0, 0)
            }
            _ => (0, 0, 0),
        }
    }

    fn pragma_database_list(&self) -> QueryResult {
        let mut rows = alloc::vec![alloc::vec![
            Value::Integer(0),
            Value::Text("main".into()),
            Value::Text(self.main_file.clone()),
        ]];
        // `temp` occupies seq 1 once it exists; attached databases begin at seq 2.
        if self.temp_db.is_some() {
            rows.push(alloc::vec![
                Value::Integer(1),
                Value::Text("temp".into()),
                Value::Text(String::new()),
            ]);
        }
        for (i, db) in self.attached.iter().enumerate() {
            rows.push(alloc::vec![
                Value::Integer((i + 2) as i64),
                Value::Text(db.name.clone()),
                Value::Text(db.file.clone()),
            ]);
        }
        QueryResult {
            columns: alloc::vec!["seq".into(), "name".into(), "file".into()],
            rows,
        }
    }

    /// `PRAGMA table_info(name)` → one row per column
    /// `(cid, name, type, notnull, dflt_value, pk)`.
    fn pragma_table_info(&self, p: &Pragma, extended: bool) -> Result<QueryResult> {
        let table = match &p.value {
            Some(Expr::Column { column, .. }) => column.clone(),
            Some(Expr::Literal(Literal::Str(s))) => s.clone(),
            _ => {
                return Err(Error::Error(
                    "PRAGMA table_info requires a table name".into(),
                ))
            }
        };
        // The schema catalog is queryable but has no stored CREATE statement;
        // report its fixed five columns, as SQLite does for `sqlite_master` /
        // `sqlite_schema` (and their `sqlite_temp_*` aliases).
        if matches!(
            table.to_ascii_lowercase().as_str(),
            "sqlite_master" | "sqlite_schema" | "sqlite_temp_master" | "sqlite_temp_schema"
        ) {
            let cols = [
                ("type", "TEXT"),
                ("name", "TEXT"),
                ("tbl_name", "TEXT"),
                ("rootpage", "INT"),
                ("sql", "TEXT"),
            ];
            let mut rows = Vec::new();
            for (i, (name, ty)) in cols.iter().enumerate() {
                let mut row = alloc::vec![
                    Value::Integer(i as i64),
                    Value::Text((*name).into()),
                    Value::Text((*ty).into()),
                    Value::Integer(0),
                    Value::Null,
                    Value::Integer(0),
                ];
                if extended {
                    row.push(Value::Integer(0));
                }
                rows.push(row);
            }
            let columns = table_info_columns(extended);
            return Ok(QueryResult { columns, rows });
        }
        // A VIEW also answers table_info: its columns with their resolved types
        // (notnull/dflt/pk are always 0/empty for a view).
        if let Some(vobj) = self.schema.objects().iter().find(|o| {
            o.obj_type == crate::schema::ObjectType::View && o.name.eq_ignore_ascii_case(&table)
        }) {
            if let Some(sql) = &vobj.sql {
                if let Statement::CreateView(cv) = sql::parse_one(sql)? {
                    return self.view_table_info(&cv, &table, extended);
                }
            }
        }
        // A virtual table answers table_info with its module's declared columns
        // and (optionally) their types; notnull / default / pk are 0/empty (the
        // safe module interface carries no such info).
        if self.is_virtual_table(&table) {
            let (_, _, schema) = self.vtab_meta(&table)?;
            let rows = schema
                .columns
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let ty = schema.types.get(i).cloned().unwrap_or_default();
                    let mut row = alloc::vec![
                        Value::Integer(i as i64),
                        Value::Text(name.clone()),
                        Value::Text(ty),
                        Value::Integer(0),
                        Value::Null,
                        Value::Integer(0),
                    ];
                    if extended {
                        row.push(Value::Integer(0));
                    }
                    row
                })
                .collect();
            return Ok(QueryResult {
                columns: table_info_columns(extended),
                rows,
            });
        }
        let obj = self
            .schema
            .table(&table)
            .ok_or_else(|| Error::Error(format!("no such table: {table}")))?;
        let sql = obj.sql.as_deref().unwrap_or("");
        let Statement::CreateTable(ct) = sql::parse_one(sql)? else {
            return Err(Error::Corrupt("schema sql is not CREATE TABLE".into()));
        };
        // The `pk` column is the 1-based position of the column within the
        // PRIMARY KEY (0 if not part of it) — so a composite `PRIMARY KEY(b,a)`
        // reports b=1, a=2, matching SQLite. A single-column or INTEGER PK is 1.
        let pk_positions = primary_key_positions(&ct);

        let mut rows = Vec::new();
        for (i, col) in ct.columns.iter().enumerate() {
            // A generated column's storage kind (`Some(stored)`), or `None`.
            let generated = col.constraints.iter().find_map(|c| match c {
                ColumnConstraint::Generated { stored, .. } => Some(*stored),
                _ => None,
            });
            // `table_info` hides generated columns; `table_xinfo` includes them
            // with a `hidden` flag (2 = virtual, 3 = stored generated; 0 = normal).
            if generated.is_some() && !extended {
                continue;
            }
            let hidden = match generated {
                None => 0,
                Some(false) => 2,
                Some(true) => 3,
            };
            // SQLite reports `notnull` from an explicit `NOT NULL` only — an
            // INTEGER PRIMARY KEY (the rowid) is shown as notnull=0.
            let notnull = col
                .constraints
                .iter()
                .any(|c| matches!(c, ColumnConstraint::NotNull(_)));
            // `dflt_value` is the SQL text of the default expression (SQLite
            // preserves the literal as written — e.g. a string keeps its quotes,
            // `DEFAULT NULL` shows `NULL`), so reprint rather than evaluate it.
            let dflt = col.constraints.iter().find_map(|c| match c {
                ColumnConstraint::Default(e) => Some(sql::print::expr(e)),
                _ => None,
            });
            let pk = pk_positions
                .iter()
                .position(|&pos| pos == i)
                .map_or(0, |n| n as i64 + 1);
            let mut row = alloc::vec![
                Value::Integer(i as i64),
                Value::Text(col.name.clone()),
                Value::Text(col.type_name.clone().unwrap_or_default()),
                Value::Integer(notnull as i64),
                dflt.map(Value::Text).unwrap_or(Value::Null),
                Value::Integer(pk),
            ];
            if extended {
                row.push(Value::Integer(hidden));
            }
            rows.push(row);
        }
        Ok(QueryResult {
            columns: table_info_columns(extended),
            rows,
        })
    }

    /// `table_info` for a VIEW: its output columns, each with the declared type
    /// SQLite reports — a direct column reference takes its origin column's type
    /// (an untyped origin shows `BLOB`), and any other expression shows an empty
    /// type. notnull/dflt/pk are always 0/NULL/0.
    fn view_table_info(
        &self,
        cv: &CreateView,
        view_name: &str,
        extended: bool,
    ) -> Result<QueryResult> {
        // (name, declared type) per output column. Prefer the static resolver;
        // fall back to running the view for names (with empty types) when the
        // body is too complex to resolve column origins statically.
        let mut cols: NamedColumns = match self.resolved_view_columns(&cv.select) {
            Some(c) => c,
            None => self
                .view_columns(view_name, &Params::default())?
                .into_iter()
                .map(|c| (c.name, None))
                .collect(),
        };
        // An explicit `CREATE VIEW v(x, y)` column list overrides the names.
        if !cv.columns.is_empty() && cv.columns.len() == cols.len() {
            for (slot, name) in cols.iter_mut().zip(&cv.columns) {
                slot.0 = name.clone();
            }
        }
        let rows = cols
            .into_iter()
            .enumerate()
            .map(|(i, (name, ty))| {
                let mut row = alloc::vec![
                    Value::Integer(i as i64),
                    Value::Text(name),
                    Value::Text(ty.unwrap_or_default()),
                    Value::Integer(0),
                    Value::Null,
                    Value::Integer(0),
                ];
                if extended {
                    row.push(Value::Integer(0));
                }
                row
            })
            .collect();
        Ok(QueryResult {
            columns: table_info_columns(extended),
            rows,
        })
    }

    /// Resolve a SELECT's output columns to `(name, declared-type)` pairs for
    /// `view_table_info`, recursing through subqueries and views. Returns `None`
    /// when a source cannot be resolved statically (a table-valued function, or a
    /// wildcard over a NATURAL/USING join whose column coalescing isn't modelled),
    /// so the caller can fall back to names-only.
    fn resolved_view_columns(&self, select: &Select) -> Option<NamedColumns> {
        // Resolve each FROM source to its labelled (name, type) columns.
        let mut sources: Vec<(String, NamedColumns)> = Vec::new();
        if let Some(fc) = &select.from {
            let mut refs = alloc::vec![&fc.first];
            let mut coalesced = false;
            for j in &fc.joins {
                refs.push(&j.table);
                if j.natural || !j.using.is_empty() {
                    coalesced = true;
                }
            }
            let has_wild = select
                .columns
                .iter()
                .any(|c| matches!(c, ResultColumn::Wildcard | ResultColumn::TableWildcard(_)));
            if coalesced && has_wild {
                return None; // `*` over coalesced columns — don't guess.
            }
            for tref in refs {
                let label = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
                sources.push((label, self.source_columns_of(tref)?));
            }
        }
        let lookup = |table: Option<&str>, col: &str| -> Option<String> {
            for (label, cols) in &sources {
                if table.is_some_and(|t| !t.eq_ignore_ascii_case(label)) {
                    continue;
                }
                if let Some((_, ty)) = cols.iter().find(|(n, _)| n.eq_ignore_ascii_case(col)) {
                    return ty.clone();
                }
            }
            None
        };
        let mut out = Vec::new();
        for rc in &select.columns {
            match rc {
                ResultColumn::Wildcard => {
                    for (_, cols) in &sources {
                        out.extend(cols.iter().cloned());
                    }
                }
                ResultColumn::TableWildcard(t) => {
                    let (_, cols) = sources.iter().find(|(l, _)| l.eq_ignore_ascii_case(t))?;
                    out.extend(cols.iter().cloned());
                }
                ResultColumn::Expr {
                    expr,
                    alias,
                    source,
                } => {
                    let name = result_column_label(expr, alias, source);
                    // Only a bare column reference carries a type through.
                    let ty = match expr {
                        Expr::Column { table, column } => lookup(table.as_deref(), column),
                        _ => None,
                    };
                    out.push((name, ty));
                }
            }
        }
        Some(out)
    }

    /// The `(name, declared-type)` columns a FROM source contributes. A base
    /// table's untyped columns report `BLOB` (as SQLite does for a view); views
    /// and subqueries recurse; TVFs return `None` (unresolved).
    fn source_columns_of(&self, tref: &TableRef) -> Option<NamedColumns> {
        if tref.tvf_args.is_some() {
            return None;
        }
        if let Some(sub) = &tref.subquery {
            return self.resolved_view_columns(sub);
        }
        // A named source: a view recurses; otherwise a base table's columns.
        if let Some(o) = self.schema.objects().iter().find(|o| {
            o.obj_type == crate::schema::ObjectType::View && o.name.eq_ignore_ascii_case(&tref.name)
        }) {
            if let Some(Ok(Statement::CreateView(cv))) = o.sql.as_deref().map(sql::parse_one) {
                return self.resolved_view_columns(&cv.select);
            }
            return None;
        }
        let obj = self.schema.table(&tref.name)?;
        let Ok(Statement::CreateTable(ct)) = sql::parse_one(obj.sql.as_deref()?) else {
            return None;
        };
        Some(
            ct.columns
                .iter()
                .map(|c| {
                    // A direct reference to an untyped column shows `BLOB`.
                    let ty = c.type_name.clone().unwrap_or_else(|| String::from("BLOB"));
                    (c.name.clone(), Some(ty))
                })
                .collect(),
        )
    }

    /// The single name argument of a `PRAGMA foo(name)` / `PRAGMA foo = name`.
    fn pragma_arg_name(p: &Pragma) -> Result<String> {
        match &p.value {
            Some(Expr::Column { column, .. }) => Ok(column.clone()),
            Some(Expr::Literal(Literal::Str(s))) => Ok(s.clone()),
            _ => Err(Error::Error("PRAGMA requires a name argument".into())),
        }
    }

    /// `PRAGMA index_list(table)` → `(seq, name, unique, origin, partial)`, newest
    /// index first (as SQLite lists them).
    fn pragma_index_list(&self, p: &Pragma) -> Result<QueryResult> {
        let table = Self::pragma_arg_name(p)?;
        let objs: Vec<_> = self.schema.indexes_on(&table).collect();

        // To label an automatic index's origin `pk` vs `u`, find the PRIMARY KEY's
        // column set. An INTEGER PRIMARY KEY is the rowid (no auto-index), so only
        // a non-integer / composite PK yields a `pk`-origin auto-index. The set
        // matches one of `collect_unique_sets`, which mirrors SQLite's auto-index
        // numbering.
        let pk_set: Vec<usize> = self
            .schema
            .table(&table)
            .and_then(|o| o.sql.as_deref())
            .and_then(|sql| sql::parse_one(sql).ok())
            .and_then(|st| match st {
                Statement::CreateTable(ct) => {
                    let ipk = find_integer_primary_key(&ct);
                    let pk = primary_key_positions(&ct);
                    // A single integer-PK column is the rowid, not an auto-index;
                    // a table with no PK has no `pk`-origin auto-index either.
                    if pk.is_empty() || (pk.len() == 1 && Some(pk[0]) == ipk) {
                        None
                    } else {
                        Some(pk)
                    }
                }
                _ => None,
            })
            .unwrap_or_default();
        let tmeta = self.table_meta(&table, None).ok();

        let mut rows = Vec::new();
        for obj in objs.iter().rev() {
            let (unique, origin, partial) = match &obj.sql {
                Some(sql) => match sql::parse_one(sql) {
                    Ok(Statement::CreateIndex(ci)) => {
                        (ci.unique as i64, "c", ci.where_clause.is_some() as i64)
                    }
                    _ => (0, "c", 0),
                },
                None => {
                    // Automatic index: `pk` when its column set is the PRIMARY
                    // KEY's, otherwise a plain UNIQUE (`u`).
                    let cols = autoindex_number(&obj.name, &table)
                        .and_then(|n| tmeta.as_ref().and_then(|m| m.unique.get(n - 1)))
                        .map(|s| s.0.clone())
                        .unwrap_or_default();
                    let origin = if !pk_set.is_empty() && cols == pk_set {
                        "pk"
                    } else {
                        "u"
                    };
                    (1, origin, 0)
                }
            };
            rows.push(alloc::vec![
                Value::Integer(rows.len() as i64),
                Value::Text(obj.name.clone()),
                Value::Integer(unique),
                Value::Text(origin.into()),
                Value::Integer(partial),
            ]);
        }
        // A WITHOUT ROWID table's PRIMARY KEY is the table b-tree itself; SQLite
        // still reports it as `sqlite_autoindex_<t>_1` (origin `pk`) — and, being
        // auto-index #1 (the oldest), it comes *last* in this newest-first list.
        // graphite keeps no separate index object for it, so synthesize the row.
        if tmeta.as_ref().is_some_and(|m| m.without_rowid) && !pk_set.is_empty() {
            rows.push(alloc::vec![
                Value::Integer(rows.len() as i64),
                Value::Text(alloc::format!("sqlite_autoindex_{table}_1")),
                Value::Integer(1),
                Value::Text("pk".into()),
                Value::Integer(0),
            ]);
        }
        Ok(QueryResult {
            columns: ["seq", "name", "unique", "origin", "partial"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
            rows,
        })
    }

    /// `PRAGMA index_info(index)` → `(seqno, cid, name)` for each indexed column.
    fn pragma_index_info(&self, p: &Pragma, extended: bool) -> Result<QueryResult> {
        let index = Self::pragma_arg_name(p)?;
        let obj = self
            .schema
            .index(&index)
            .ok_or_else(|| Error::Error(format!("no such index: {index}")))?;
        let tmeta = self.table_meta(&obj.tbl_name, None)?;
        // Per key column: (cid, name, descending, collation). A bare column takes
        // its position + name; an EXPRESSION column is `cid = -2` with a NULL name,
        // as SQLite reports (its collation defaults to BINARY unless COLLATE-d).
        type Key = (i64, Option<String>, bool, crate::value::Collation);
        let keys: Vec<Key> = match &obj.sql {
            Some(sql) => match sql::parse_one(sql)? {
                Statement::CreateIndex(ci) => ci
                    .columns
                    .iter()
                    .map(|term| {
                        let (inner, explicit) = match &term.expr {
                            Expr::Collate { expr, collation } => {
                                (expr.as_ref(), crate::value::Collation::parse(collation))
                            }
                            e => (e, None),
                        };
                        match inner {
                            Expr::Column { column, .. } => {
                                match tmeta
                                    .columns
                                    .iter()
                                    .position(|c| c.name.eq_ignore_ascii_case(column))
                                {
                                    Some(p) => (
                                        p as i64,
                                        Some(tmeta.columns[p].name.clone()),
                                        term.descending,
                                        explicit.unwrap_or(tmeta.columns[p].collation),
                                    ),
                                    None => {
                                        (-2, None, term.descending, explicit.unwrap_or_default())
                                    }
                                }
                            }
                            _ => (-2, None, term.descending, explicit.unwrap_or_default()),
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            },
            None => autoindex_number(&obj.name, &obj.tbl_name)
                .and_then(|n| tmeta.unique.get(n - 1))
                .map(|s| s.0.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|cid| {
                    (
                        cid as i64,
                        Some(tmeta.columns[cid].name.clone()),
                        false,
                        tmeta.columns[cid].collation,
                    )
                })
                .collect(),
        };
        let coll_name = |c: crate::value::Collation| match c {
            crate::value::Collation::NoCase => "NOCASE",
            crate::value::Collation::RTrim => "RTRIM",
            crate::value::Collation::Binary => "BINARY",
        };
        let mut rows = Vec::new();
        for (seqno, (cid, name, desc, coll)) in keys.iter().enumerate() {
            let name_val = name.clone().map_or(Value::Null, Value::Text);
            if extended {
                rows.push(alloc::vec![
                    Value::Integer(seqno as i64),
                    Value::Integer(*cid),
                    name_val,
                    Value::Integer(*desc as i64),
                    Value::Text(coll_name(*coll).into()),
                    Value::Integer(1), // key column
                ]);
            } else {
                rows.push(alloc::vec![
                    Value::Integer(seqno as i64),
                    Value::Integer(*cid),
                    name_val
                ]);
            }
        }
        // index_xinfo appends the index's implicit trailing auxiliary (non-key)
        // columns: the rowid for an ordinary table, or the PRIMARY KEY columns (in
        // key order, those not already index keys) for a WITHOUT ROWID table.
        if extended {
            if tmeta.without_rowid {
                let key_cids: Vec<i64> = keys.iter().map(|(cid, ..)| *cid).collect();
                let mut seqno = keys.len();
                for &pcid in &tmeta.storage_order[..tmeta.pk_len] {
                    if key_cids.contains(&(pcid as i64)) {
                        continue;
                    }
                    rows.push(alloc::vec![
                        Value::Integer(seqno as i64),
                        Value::Integer(pcid as i64),
                        Value::Text(tmeta.columns[pcid].name.clone()),
                        Value::Integer(0),
                        Value::Text(coll_name(tmeta.columns[pcid].collation).into()),
                        Value::Integer(0), // auxiliary, non-key
                    ]);
                    seqno += 1;
                }
            } else {
                rows.push(alloc::vec![
                    Value::Integer(keys.len() as i64),
                    Value::Integer(-1),
                    Value::Null,
                    Value::Integer(0),
                    Value::Text("BINARY".into()),
                    Value::Integer(0),
                ]);
            }
        }
        let columns: Vec<String> = if extended {
            ["seqno", "cid", "name", "desc", "coll", "key"]
                .iter()
                .map(|s| String::from(*s))
                .collect()
        } else {
            ["seqno", "cid", "name"]
                .iter()
                .map(|s| String::from(*s))
                .collect()
        };
        Ok(QueryResult { columns, rows })
    }

    /// `PRAGMA foreign_key_list(table)` →
    /// `(id, seq, table, from, to, on_update, on_delete, match)`.
    fn pragma_foreign_key_list(&self, p: &Pragma) -> Result<QueryResult> {
        let table = Self::pragma_arg_name(p)?;
        let obj = self
            .schema
            .table(&table)
            .ok_or_else(|| Error::Error(format!("no such table: {table}")))?;
        let Statement::CreateTable(ct) = sql::parse_one(obj.sql.as_deref().unwrap_or(""))? else {
            // A virtual table (non-CREATE-TABLE schema) has no foreign keys.
            return Ok(QueryResult {
                columns: [
                    "id",
                    "seq",
                    "table",
                    "from",
                    "to",
                    "on_update",
                    "on_delete",
                    "match",
                ]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
                rows: Vec::new(),
            });
        };
        let action = |a: FkAction| -> &'static str {
            match a {
                FkAction::NoAction => "NO ACTION",
                FkAction::Restrict => "RESTRICT",
                FkAction::Cascade => "CASCADE",
                FkAction::SetNull => "SET NULL",
                FkAction::SetDefault => "SET DEFAULT",
            }
        };
        // Collect (from-cols, fk) pairs from column-level and table-level FKs.
        let mut fks: Vec<(Vec<String>, &ForeignKey)> = Vec::new();
        for col in &ct.columns {
            for c in &col.constraints {
                if let ColumnConstraint::References(fk) = c {
                    fks.push((alloc::vec![col.name.clone()], fk));
                }
            }
        }
        for c in &ct.constraints {
            if let TableConstraint::ForeignKey(fk) = c {
                fks.push((fk.columns.clone(), fk));
            }
        }
        let mut rows = Vec::new();
        // SQLite numbers foreign keys from the last declared (id 0) backward, and
        // lists them by id ascending — so iterate in reverse declaration order.
        let n = fks.len();
        for (i, (from_cols, fk)) in fks.iter().enumerate().rev() {
            let id = (n - 1 - i) as i64;
            for (seq, from) in from_cols.iter().enumerate() {
                let to = fk.ref_columns.get(seq).cloned().unwrap_or_default();
                rows.push(alloc::vec![
                    Value::Integer(id),
                    Value::Integer(seq as i64),
                    Value::Text(fk.ref_table.clone()),
                    Value::Text(from.clone()),
                    if to.is_empty() {
                        Value::Null
                    } else {
                        Value::Text(to)
                    },
                    Value::Text(action(fk.on_update).into()),
                    Value::Text(action(fk.on_delete).into()),
                    Value::Text("NONE".into()),
                ]);
            }
        }
        Ok(QueryResult {
            columns: [
                "id",
                "seq",
                "table",
                "from",
                "to",
                "on_update",
                "on_delete",
                "match",
            ]
            .iter()
            .map(|s| String::from(*s))
            .collect(),
            rows,
        })
    }

    /// `PRAGMA foreign_key_check[(table)]` → one `(table, rowid, parent, fkid)`
    /// row per child row that references a missing parent key. `fkid` matches the
    /// `id` reported by `foreign_key_list`.
    fn pragma_foreign_key_check(&self, p: &Pragma) -> Result<QueryResult> {
        use crate::schema::ObjectType;
        let tables: Vec<String> = match &p.value {
            Some(_) => alloc::vec![Self::pragma_arg_name(p)?],
            None => self
                .schema
                .objects()
                .iter()
                .filter(|o| o.obj_type == ObjectType::Table && !o.name.starts_with("sqlite_"))
                .map(|o| o.name.clone())
                .collect(),
        };
        let mut rows = Vec::new();
        for table in &tables {
            let meta = self.table_meta(table, None)?;
            if meta.without_rowid {
                continue; // rowid-less FK reporting not modeled yet
            }
            let fks = self.foreign_keys_of(table)?;
            if fks.is_empty() {
                continue;
            }
            let n = fks.len();
            for (rowid, values) in self.scan_table(&meta)? {
                for (i, fk) in fks.iter().enumerate() {
                    let Some(key) = self.child_key_values(&meta, fk, &values) else {
                        continue; // a NULL key column => satisfied
                    };
                    if !self.parent_has_key(fk, &key)? {
                        rows.push(alloc::vec![
                            Value::Text(table.clone()),
                            Value::Integer(rowid),
                            Value::Text(fk.ref_table.clone()),
                            Value::Integer((n - 1 - i) as i64),
                        ]);
                    }
                }
            }
        }
        Ok(QueryResult {
            columns: ["table", "rowid", "parent", "fkid"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
            rows,
        })
    }

    /// `PRAGMA integrity_check` / `quick_check`: walk every table and index
    /// b-tree and verify each index holds exactly the entries its table implies
    /// (honoring partial-index predicates). Returns the single value `ok` when the
    /// database is consistent, else one row per detected problem.
    fn pragma_integrity_check(&self) -> Result<QueryResult> {
        use crate::schema::ObjectType;
        let single = |v: Value| QueryResult {
            columns: alloc::vec![String::from("integrity_check")],
            rows: alloc::vec![alloc::vec![v]],
        };
        let tables: Vec<String> = self
            .schema
            .objects()
            .iter()
            // Skip virtual tables: they have no b-tree of their own (a persistent
            // module's rows live in its `<name>_data` backing table, itself an
            // ordinary table that is checked here).
            .filter(|o| {
                o.obj_type == ObjectType::Table
                    && !o.name.starts_with("sqlite_")
                    && !matches!(
                        o.sql.as_deref().map(sql::parse_one),
                        Some(Ok(Statement::CreateVirtualTable(_)))
                    )
            })
            .map(|o| o.name.clone())
            .collect();

        let mut problems = Vec::new();
        for table in &tables {
            let meta = self.table_meta(table, None)?;
            // The rows that physically exist, and how many each index should hold.
            let rows: Vec<Vec<Value>> = if meta.without_rowid {
                self.scan_without_rowid(&meta)?
            } else {
                self.scan_table(&meta)?
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect()
            };
            let no_params = Params::default();
            for idx in self.indexes_of(table)? {
                let expected = rows
                    .iter()
                    .filter_map(|r| self.row_in_index(&idx, &meta, r, None, &no_params).ok())
                    .filter(|&keep| keep)
                    .count();
                // Count the index b-tree's entries.
                let mut cur = crate::btree::IndexCursor::new(self.backend.source(), idx.root);
                let mut got = 0usize;
                while cur.next()?.is_some() {
                    got += 1;
                }
                if got != expected {
                    problems.push(alloc::format!("wrong # of entries in index {}", idx.name));
                }
            }
        }

        if problems.is_empty() {
            Ok(single(Value::Text("ok".into())))
        } else {
            Ok(QueryResult {
                columns: alloc::vec![String::from("integrity_check")],
                rows: problems
                    .into_iter()
                    .map(|p| alloc::vec![Value::Text(p)])
                    .collect(),
            })
        }
    }

    /// Execute a single non-`SELECT` statement, returning the number of rows
    /// affected (0 for DDL and transaction control).
    pub fn execute(&mut self, sql: &str) -> Result<usize> {
        self.execute_params(sql, &Params::default())
    }

    /// Register a virtual-table [`module`](crate::vtab::VTabModule) under `name`,
    /// the identifier used after `USING` in `CREATE VIRTUAL TABLE … USING <name>`.
    /// A module implementing [`VTabModule::update`](crate::vtab::VTabModule::update)
    /// makes its tables writable; the default leaves them read-only. Fails if a
    /// module is already registered under that name (case-insensitively).
    pub fn register_module(
        &mut self,
        name: &str,
        module: impl DynVTabModule + 'static,
    ) -> Result<()> {
        self.vtab_registry.register(name, Box::new(module))
    }

    /// Register a user-defined scalar function callable from SQL by `name`. `f`
    /// receives the evaluated argument values and returns a result [`Value`]. A
    /// built-in function of the same name takes precedence; registering an existing
    /// user function replaces it. The callback should validate its own argument
    /// count and types (returning an error otherwise), like SQLite's
    /// `sqlite3_create_function` callbacks.
    pub fn register_function(
        &mut self,
        name: &str,
        f: impl Fn(&[Value]) -> Result<Value> + 'static,
    ) {
        self.functions
            .insert(name.to_ascii_lowercase(), Box::new(f));
    }

    /// Register a user-defined aggregate function callable from SQL by `name`.
    /// `factory` builds a fresh [`AggregateFunction`] accumulator for each group;
    /// the engine calls `step` once per group row (with the evaluated arguments)
    /// then `finalize`. Built-in aggregates of the same name take precedence.
    pub fn register_aggregate_function(
        &mut self,
        name: &str,
        factory: impl Fn() -> Box<dyn AggregateFunction> + 'static,
    ) {
        self.aggregates
            .insert(name.to_ascii_lowercase(), Box::new(factory));
    }

    /// Execute a `;`-separated script of one or more statements, like SQLite's
    /// `sqlite3_exec`. Each statement runs in order through the normal
    /// single-statement path (so per-statement `CREATE` text is preserved and
    /// each autocommits unless the script opens its own transaction); execution
    /// stops at the first error. `;` inside string literals, `--`/`/* */`
    /// comments, and `BEGIN…END` / `CASE…END` blocks does not split a statement.
    /// A `SELECT` runs and its rows are discarded (as `sqlite3_exec` does without
    /// a callback). [`execute`](Self::execute) stays single-statement.
    pub fn execute_batch(&mut self, sql: &str) -> Result<()> {
        for stmt in split_sql_script(sql) {
            if matches!(sql::parse_one(stmt), Ok(Statement::Select(_))) {
                self.query(stmt)?;
            } else {
                self.execute_params(stmt, &Params::default())?;
            }
        }
        Ok(())
    }

    /// Like [`execute`](Self::execute) but with bound parameters.
    pub fn execute_params(&mut self, sql: &str, params: &Params) -> Result<usize> {
        let stmt = sql::parse_one(sql)?;
        // Transaction control is handled directly (no autocommit around it).
        match &stmt {
            Statement::Begin => {
                if self.in_tx {
                    return Err(Error::Error(
                        "cannot start a transaction within a transaction".into(),
                    ));
                }
                self.in_tx = true;
                return Ok(0);
            }
            Statement::Commit => {
                if !self.in_tx && self.open_savepoints == 0 {
                    return Err(Error::Error(
                        "cannot commit - no transaction is active".into(),
                    ));
                }
                // Deferred foreign keys are verified here. On violation the
                // transaction stays open (SQLite leaves it active so the caller
                // can repair the data and COMMIT again) — nothing is committed.
                self.check_deferred_fks()?;
                self.backend.writer()?.commit()?;
                // Cross-database transaction: commit the temp + attached
                // databases alongside main (a clean pager commit is a no-op).
                self.commit_attached()?;
                self.in_tx = false;
                self.open_savepoints = 0;
                return Ok(0);
            }
            Statement::Savepoint(name) => {
                self.backend.writer()?.savepoint(name);
                self.savepoint_attached(name)?;
                self.open_savepoints += 1;
                return Ok(0);
            }
            Statement::Release(name) => {
                self.backend.writer()?.release_savepoint(name)?;
                self.release_attached(name)?;
                self.open_savepoints = self.backend.writer()?.savepoint_depth();
                // Releasing the outermost savepoint of an implicit transaction
                // finalizes it — verify deferred foreign keys first.
                if self.open_savepoints == 0 && !self.in_tx {
                    self.check_deferred_fks()?;
                    self.backend.writer()?.commit()?;
                    self.commit_attached()?;
                    self.schema = Schema::read(self.backend.source())?;
                }
                return Ok(0);
            }
            Statement::RollbackTo(name) => {
                self.backend.writer()?.rollback_to_savepoint(name)?;
                self.rollback_to_attached(name)?;
                self.open_savepoints = self.backend.writer()?.savepoint_depth();
                // The schema may have reverted to the savepoint's state.
                self.schema = Schema::read(self.backend.source())?;
                return Ok(0);
            }
            Statement::Rollback => {
                if !self.in_tx && self.open_savepoints == 0 {
                    return Err(Error::Error(
                        "cannot rollback - no transaction is active".into(),
                    ));
                }
                self.backend.writer()?.rollback();
                // Cross-database transaction: roll back the temp + attached
                // databases too, discarding their staged changes.
                self.rollback_attached()?;
                self.in_tx = false;
                self.open_savepoints = 0;
                self.schema = Schema::read(self.backend.source())?;
                return Ok(0);
            }
            _ => {}
        }

        // A DDL/DML statement targeting a non-main database (`… aux.t`,
        // `CREATE TEMP …`, or an unqualified name that a temp table shadows) runs
        // against that database: a single write touches exactly one database, so
        // we make it the active `main` for the duration (swapping back
        // afterwards, even on error). Cross-database *joins* are handled
        // separately in the read path.
        let target = self.target_db(&stmt)?;
        if target == DbRef::Temp {
            self.ensure_temp()?;
        }
        match target {
            DbRef::Main => self.exec_parsed(stmt, sql, params),
            other => {
                self.swap_db(other);
                let r = self.exec_parsed(stmt, sql, params);
                self.swap_db(other);
                r
            }
        }
    }

    /// The database a DDL/DML statement targets: an explicit `schema.` qualifier
    /// (including `CREATE TEMP …` → `Temp`), else — for DML/`DROP` — the temp
    /// database when it shadows the unqualified name, else `main`.
    fn target_db(&self, stmt: &Statement) -> Result<DbRef> {
        let resolved = |s: Option<&str>, name: &str| -> Result<DbRef> {
            match s {
                Some(_) => self.resolve_db(s),
                None => Ok(self.unqualified_db(name)),
            }
        };
        match stmt {
            // CREATE never temp-shadows: a bare `CREATE TABLE t` goes to main.
            Statement::CreateTable(s) => self.resolve_db(s.schema.as_deref()),
            Statement::Insert(s) => resolved(s.schema.as_deref(), &s.table),
            Statement::Update(s) => resolved(s.schema.as_deref(), &s.table),
            Statement::Delete(s) => resolved(s.schema.as_deref(), &s.table),
            Statement::Drop(s) => resolved(s.schema.as_deref(), &s.name),
            Statement::Alter(a) => resolved(a.schema.as_deref(), &a.table),
            // The index lives in the schema named on the index (or, unqualified,
            // wherever its table lives — so a temp table's index goes to temp).
            Statement::CreateIndex(ci) => resolved(ci.schema.as_deref(), &ci.table),
            // A view lives in the schema named on it (`CREATE TEMP VIEW` → temp);
            // an unqualified `CREATE VIEW` stays in main.
            Statement::CreateView(cv) => self.resolve_db(cv.schema.as_deref()),
            // A trigger lives in the schema named on it (or, unqualified,
            // wherever the table it fires on lives).
            Statement::CreateTrigger(ct) => resolved(ct.schema.as_deref(), &ct.table),
            // A virtual table lives in the schema named on it; bare → main.
            Statement::CreateVirtualTable(cvt) => self.resolve_db(cvt.schema.as_deref()),
            _ => Ok(DbRef::Main),
        }
    }

    /// Commit pending changes in the temp + attached databases, refreshing each
    /// catalog from its committed image. Part of a cross-database transaction
    /// commit; a clean pager commit is a no-op.
    fn commit_attached(&mut self) -> Result<()> {
        if let Some(t) = &mut self.temp_db {
            t.backend.writer()?.commit()?;
            t.schema = Schema::read(t.backend.source())?;
        }
        for d in &mut self.attached {
            d.backend.writer()?.commit()?;
            d.schema = Schema::read(d.backend.source())?;
        }
        Ok(())
    }

    /// Roll back staged changes in the temp + attached databases and reload each
    /// catalog. Part of a cross-database transaction rollback.
    fn rollback_attached(&mut self) -> Result<()> {
        if let Some(t) = &mut self.temp_db {
            t.backend.writer()?.rollback();
            t.schema = Schema::read(t.backend.source())?;
        }
        for d in &mut self.attached {
            d.backend.writer()?.rollback();
            d.schema = Schema::read(d.backend.source())?;
        }
        Ok(())
    }

    /// Open a savepoint in the temp + attached databases too, so a later
    /// `ROLLBACK TO`/`RELEASE` reaches their staged changes.
    fn savepoint_attached(&mut self, name: &str) -> Result<()> {
        if let Some(t) = &mut self.temp_db {
            t.backend.writer()?.savepoint(name);
        }
        for d in &mut self.attached {
            d.backend.writer()?.savepoint(name);
        }
        Ok(())
    }

    /// Release a savepoint in the temp + attached databases. A database attached
    /// after the savepoint was opened has no such savepoint; that is not an error
    /// here (it simply had nothing staged at that point).
    fn release_attached(&mut self, name: &str) -> Result<()> {
        if let Some(t) = &mut self.temp_db {
            let _ = t.backend.writer()?.release_savepoint(name);
        }
        for d in &mut self.attached {
            let _ = d.backend.writer()?.release_savepoint(name);
        }
        Ok(())
    }

    /// Roll the temp + attached databases back to a savepoint, reloading the
    /// catalog of each that actually had it (see [`release_attached`]).
    fn rollback_to_attached(&mut self, name: &str) -> Result<()> {
        if let Some(t) = &mut self.temp_db {
            let did = t.backend.writer()?.rollback_to_savepoint(name).is_ok();
            if did {
                t.schema = Schema::read(t.backend.source())?;
            }
        }
        for d in &mut self.attached {
            let did = d.backend.writer()?.rollback_to_savepoint(name).is_ok();
            if did {
                d.schema = Schema::read(d.backend.source())?;
            }
        }
        Ok(())
    }

    /// Make `db` the active `main` (or swap it back) by exchanging the backend
    /// and schema. Used around a write to a non-main database.
    fn swap_db(&mut self, db: DbRef) {
        match db {
            DbRef::Main => {}
            DbRef::Temp => {
                let t = self.temp_db.as_mut().expect("temp db exists");
                core::mem::swap(&mut self.backend, &mut t.backend);
                core::mem::swap(&mut self.schema, &mut t.schema);
            }
            DbRef::Attached(i) => self.swap_attached(i),
        }
    }

    fn swap_attached(&mut self, i: usize) {
        core::mem::swap(&mut self.backend, &mut self.attached[i].backend);
        core::mem::swap(&mut self.schema, &mut self.attached[i].schema);
    }

    /// Execute a parsed non-transaction-control statement on the active database.
    fn exec_parsed(&mut self, stmt: Statement, sql: &str, params: &Params) -> Result<usize> {
        // `changes()`/`total_changes()` track only INSERT/UPDATE/DELETE.
        let is_dml = matches!(
            stmt,
            Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_)
        );
        // Writes to an `auto_vacuum` database are now supported: the write-side
        // pager maintains the pointer-map pages on commit (see
        // `WritePager::rebuild_ptrmap`), so the C6a guard that used to refuse
        // such writes has been lifted. auto_vacuum=NONE databases take the
        // unchanged plain write path.
        // An INSERT/UPDATE/DELETE is atomic: if it fails partway (a constraint
        // violation, a trigger `RAISE(ABORT)`, …) the rows it already changed are
        // undone, leaving the database as if the statement never ran — unless the
        // failing conflict policy was `OR FAIL`, which keeps the partial change.
        // We realise this with an internal savepoint snapshotting the writer
        // overlay(s) before the statement and rolling back to it on an
        // abort-class error. (A no-op for DDL, which doesn't set `is_dml`.)
        if is_dml {
            self.stmt_keep_partial.set(false);
            self.stmt_rollback_tx.set(false);
            return self.run_dml_atomic(stmt, params);
        }
        let affected = match stmt {
            Statement::CreateTable(ct) => {
                self.exec_create_table(&ct, ddl_text(sql))?;
                0
            }
            Statement::Insert(_) | Statement::Delete(_) | Statement::Update(_) => unreachable!(),
            Statement::CreateIndex(ci) => {
                self.exec_create_index(&ci, ddl_text(sql))?;
                0
            }
            Statement::CreateView(cv) => {
                self.exec_create_view(&cv, ddl_text(sql))?;
                0
            }
            Statement::CreateTrigger(ct) => {
                self.exec_create_trigger(&ct, ddl_text(sql))?;
                0
            }
            Statement::CreateVirtualTable(cvt) => {
                self.exec_create_virtual_table(&cvt, ddl_text(sql))?;
                0
            }
            Statement::Drop(d) => {
                self.exec_drop(&d)?;
                0
            }
            Statement::Alter(a) => {
                self.exec_alter(&a)?;
                0
            }
            Statement::Pragma(p) => {
                self.exec_pragma(&p, params)?;
                0
            }
            Statement::Vacuum(into) => {
                self.exec_vacuum(into.as_deref())?;
                0
            }
            // Indexes are kept current on every write, so REINDEX is a no-op.
            Statement::Reindex => 0,
            Statement::Analyze(target) => {
                self.exec_analyze(target.as_deref())?;
                0
            }
            Statement::Attach { file, name } => {
                self.exec_attach(&file, &name, params)?;
                0
            }
            Statement::Detach(name) => {
                self.exec_detach(&name)?;
                0
            }
            Statement::Select(_) => return Err(Error::Unsupported("use query() for SELECT")),
            Statement::Explain { .. } => return Err(Error::Unsupported("use query() for EXPLAIN")),
            Statement::Begin
            | Statement::Commit
            | Statement::Rollback
            | Statement::Savepoint(_)
            | Statement::Release(_)
            | Statement::RollbackTo(_) => unreachable!(),
        };

        if !self.in_tx && self.open_savepoints == 0 {
            self.backend.writer()?.commit()?;
            // Refresh the catalog from the committed image.
            self.schema = Schema::read(self.backend.source())?;
        }
        Ok(affected)
    }

    /// Execute one INSERT/UPDATE/DELETE under an internal savepoint so it is
    /// atomic: on an abort-class failure (a constraint violation, a trigger
    /// `RAISE(ABORT)`, …) the writer overlay(s) are rolled back to the
    /// pre-statement snapshot, so no partial change survives. `OR FAIL` keeps the
    /// rows changed before the failure; `OR ROLLBACK` unwinds the whole
    /// transaction.
    /// Build the constraint error for a conflict under conflict policy `oc`,
    /// arming the statement-atomicity flags so `run_dml_atomic` keeps partial
    /// changes (`OR FAIL`) or unwinds the transaction (`OR ROLLBACK`).
    fn conflict_error(&self, oc: OnConflict, msg: &str) -> Error {
        match oc {
            OnConflict::Fail => self.stmt_keep_partial.set(true),
            OnConflict::Rollback => self.stmt_rollback_tx.set(true),
            _ => {}
        }
        Error::Constraint(String::from(msg))
    }

    /// Resolve `NOT NULL` violations for an INSERT/UPDATE row under its conflict
    /// mode, mutating `values` as needed. For each `NOT NULL` column that is NULL,
    /// the effective action is the statement's `OR <action>` (when it wrote one)
    /// else the column's declared `ON CONFLICT` action: `REPLACE` substitutes the
    /// column's DEFAULT (erroring if there is none, like SQLite), `IGNORE` skips
    /// the whole row (returns `Ok(false)`), and `ABORT`/`FAIL`/`ROLLBACK` error
    /// with the action's rollback semantics. `Ok(true)` means the row may proceed.
    fn resolve_not_null(
        &self,
        meta: &TableMeta,
        values: &mut [Value],
        stmt_oc: OnConflict,
        stmt_explicit: bool,
        params: &Params,
    ) -> Result<bool> {
        for (i, slot) in values.iter_mut().enumerate() {
            if !matches!(slot, Value::Null) {
                continue;
            }
            let Some(col_oc) = meta.not_null[i] else {
                continue;
            };
            let oc = if stmt_explicit { stmt_oc } else { col_oc };
            let fail = || {
                let msg = format!(
                    "NOT NULL constraint failed: {}.{}",
                    meta.columns[i].table, meta.columns[i].name
                );
                self.conflict_error(oc, &msg)
            };
            match oc {
                OnConflict::Ignore => return Ok(false),
                OnConflict::Replace => {
                    // Substitute the column's DEFAULT; a missing or NULL default
                    // leaves the violation, which then errors.
                    let v = match &meta.defaults[i] {
                        Some(e) => eval::eval(e, &EvalCtx::rowless(params)).unwrap_or(Value::Null),
                        None => Value::Null,
                    };
                    if matches!(v, Value::Null) {
                        return Err(fail());
                    }
                    *slot = v;
                }
                _ => return Err(fail()),
            }
        }
        Ok(true)
    }

    fn run_dml_atomic(&mut self, stmt: Statement, params: &Params) -> Result<usize> {
        const SP: &str = "\u{0}graphite_stmt";
        self.backend.writer()?.savepoint(SP);
        self.savepoint_attached(SP)?;
        let result = match stmt {
            Statement::Insert(ins) => self.exec_insert(&ins, params),
            Statement::Delete(del) => self.exec_delete(&del, params),
            Statement::Update(upd) => self.exec_update(&upd, params),
            _ => unreachable!("run_dml_atomic only handles DML"),
        };
        match result {
            Ok(affected) => {
                let _ = self.backend.writer()?.release_savepoint(SP);
                let _ = self.release_attached(SP);
                self.changes.set(affected as i64);
                self.total_changes
                    .set(self.total_changes.get() + affected as i64);
                if !self.in_tx && self.open_savepoints == 0 {
                    self.backend.writer()?.commit()?;
                    self.schema = Schema::read(self.backend.source())?;
                }
                Ok(affected)
            }
            Err(e) => {
                if self.stmt_rollback_tx.get() {
                    // `OR ROLLBACK`: discard the entire (implicit or explicit)
                    // transaction's staged changes.
                    self.backend.writer()?.rollback();
                    self.rollback_attached()?;
                    self.in_tx = false;
                    self.open_savepoints = 0;
                    self.schema = Schema::read(self.backend.source())?;
                } else if self.stmt_keep_partial.get() {
                    // `OR FAIL`: keep what was changed before the failure.
                    let _ = self.backend.writer()?.release_savepoint(SP);
                    let _ = self.release_attached(SP);
                    if !self.in_tx && self.open_savepoints == 0 {
                        self.backend.writer()?.commit()?;
                        self.schema = Schema::read(self.backend.source())?;
                    }
                } else {
                    // `OR ABORT` (the default): undo just this statement.
                    let _ = self.backend.writer()?.rollback_to_savepoint(SP);
                    let _ = self.backend.writer()?.release_savepoint(SP);
                    let _ = self.rollback_to_attached(SP);
                    let _ = self.release_attached(SP);
                    if !self.in_tx && self.open_savepoints == 0 {
                        // Outside a transaction the rolled-back statement leaves
                        // nothing to commit; drop any other staged state too.
                        self.backend.writer()?.rollback();
                        self.rollback_attached()?;
                        self.schema = Schema::read(self.backend.source())?;
                    }
                }
                Err(e)
            }
        }
    }

    /// Execute an `INSERT`/`UPDATE`/`DELETE` with a `RETURNING` clause, returning
    /// the projected rows as a [`QueryResult`]. Without a `RETURNING` list the
    /// result has no columns and no rows (the statement still runs for its
    /// effects). Errors on `SELECT`/DDL — use [`query`](Self::query) or
    /// [`execute`](Self::execute) for those.
    pub fn execute_returning(&mut self, sql: &str, params: &Params) -> Result<QueryResult> {
        let stmt = sql::parse_one(sql)?;
        let returning: &[ResultColumn] = match &stmt {
            Statement::Insert(i) => &i.returning,
            Statement::Update(u) => &u.returning,
            Statement::Delete(d) => &d.returning,
            _ => {
                return Err(Error::Unsupported(
                    "execute_returning expects INSERT/UPDATE/DELETE",
                ))
            }
        };
        if returning.is_empty() {
            self.execute_params(sql, params)?;
            return Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
            });
        }
        let table = match &stmt {
            Statement::Insert(i) => &i.table,
            Statement::Update(u) => &u.table,
            Statement::Delete(d) => &d.table,
            _ => unreachable!(),
        };
        let meta = self.table_meta(table, None)?;
        let columns = returning_labels(returning, &meta.columns);
        self.returning_rows.borrow_mut().clear();
        self.execute_params(sql, params)?;
        let rows = core::mem::take(&mut *self.returning_rows.borrow_mut());
        Ok(QueryResult { columns, rows })
    }

    /// `ATTACH <expr> AS <name>`: open another database under `name`. An empty
    /// or `:memory:` path creates a fresh in-memory database; a real file path is
    /// not yet supported (track piece C5).
    fn exec_attach(&mut self, file: &Expr, name: &str, params: &Params) -> Result<()> {
        let path = {
            let ctx = EvalCtx::rowless(params).with_subqueries(self);
            eval::to_text(&eval::eval(file, &ctx)?)
        };
        if name.eq_ignore_ascii_case("main")
            || name.eq_ignore_ascii_case("temp")
            || self
                .attached
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(name))
        {
            return Err(Error::Error(alloc::format!(
                "database {name} is already in use"
            )));
        }
        let (backend, file) = if path.is_empty() || path.eq_ignore_ascii_case(":memory:") {
            // A fresh in-memory database (same pattern as `open_memory`).
            let vfs = crate::vfs::memory::MemoryVfs::new();
            let f = vfs.open(name, OpenFlags::READ_WRITE_CREATE)?;
            let mut db = WritePager::create(f, None, 4096)?;
            db.commit()?;
            (Backend::Write(Box::new(db)), String::new())
        } else {
            (self.open_attached_file(&path)?, path)
        };
        let schema = Schema::read(backend.source())?;
        self.attached.push(AttachedDb {
            name: name.to_string(),
            file,
            backend,
            schema,
        });
        Ok(())
    }

    /// Open (or create, if absent/empty) a real file as an attached database's
    /// backend. Requires the `std` file VFS.
    #[cfg(feature = "std")]
    fn open_attached_file(&self, path: &str) -> Result<Backend> {
        let vfs = crate::vfs::std_file::StdVfs::new();
        let main = vfs.open(path, OpenFlags::READ_WRITE_CREATE)?;
        let journal = vfs.open(&journal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        // Rollback-journal (non-WAL) mode: commits land directly in the main
        // file, so the attached database is immediately readable by sqlite3
        // without needing a WAL checkpoint when the connection closes.
        let db = if main.size()? == 0 {
            let mut db = WritePager::create(main, Some(journal), 4096)?;
            db.commit()?;
            db
        } else {
            WritePager::open(main, Some(journal))?
        };
        Ok(Backend::Write(Box::new(db)))
    }

    #[cfg(not(feature = "std"))]
    fn open_attached_file(&self, _path: &str) -> Result<Backend> {
        Err(Error::Unsupported("ATTACH of a file database requires std"))
    }

    /// `DETACH <name>`: close an attached database. `main`/`temp` cannot be
    /// detached; an unknown name is an error.
    fn exec_detach(&mut self, name: &str) -> Result<()> {
        if name.eq_ignore_ascii_case("main") || name.eq_ignore_ascii_case("temp") {
            return Err(Error::Error(alloc::format!(
                "cannot detach database {name}"
            )));
        }
        match self
            .attached
            .iter()
            .position(|d| d.name.eq_ignore_ascii_case(name))
        {
            Some(i) => {
                self.attached.remove(i);
                Ok(())
            }
            None => Err(Error::Error(alloc::format!("no such database: {name}"))),
        }
    }

    /// `VACUUM`: rebuild the database into a fresh, compact image (no free pages,
    /// defragmented b-trees) and replace the file. Implemented by replaying the
    /// stored `CREATE` statements and re-inserting all rows into a throwaway
    /// in-memory database, then copying its pages over. A no-op for read-only
    /// backends.
    fn exec_vacuum(&mut self, into: Option<&Expr>) -> Result<()> {
        use crate::schema::ObjectType;
        // In-place VACUUM on a read-only backend is a no-op; `VACUUM … INTO`
        // only reads the source, so it proceeds regardless of the backend.
        if into.is_none() && !matches!(self.backend, Backend::Write(_)) {
            return Ok(());
        }
        // Flush any WAL frames into the main image first (in-place rewrite only).
        if into.is_none() && self.backend.wal_mode() {
            self.backend.writer()?.checkpoint()?;
        }
        let user_version = self.backend.source().header().user_version;

        // Snapshot the catalog: (type, name, sql), preserving creation order.
        let objs: Vec<(ObjectType, String, Option<String>)> = self
            .schema
            .objects()
            .iter()
            .map(|o| (o.obj_type, o.name.clone(), o.sql.clone()))
            .collect();

        let quote = |n: &str| alloc::format!("\"{}\"", n.replace('"', "\"\""));

        // Virtual tables and their `<name>_data` backing tables need special care:
        // recreating the `CREATE VIRTUAL TABLE` already creates the backing table,
        // so the backing table must not be created (or its rows copied) separately
        // — a persistent vtab's rows are repopulated by re-inserting through the
        // vtab itself, and a computed (non-persistent) vtab has no rows to copy.
        let is_vtab = |sql: &Option<String>| {
            matches!(
                sql.as_deref().map(sql::parse_one),
                Some(Ok(Statement::CreateVirtualTable(_)))
            )
        };
        let vtab_names: alloc::collections::BTreeSet<String> = objs
            .iter()
            .filter(|(ty, _, sql)| *ty == ObjectType::Table && is_vtab(sql))
            .map(|(_, n, _)| n.clone())
            .collect();
        let table_names: alloc::collections::BTreeSet<String> = objs
            .iter()
            .filter(|(ty, _, _)| *ty == ObjectType::Table)
            .map(|(_, n, _)| n.clone())
            .collect();
        let is_backing = |name: &str| {
            [
                "_data", "_node", "_rowid", "_parent", "_content", "_docsize", "_config", "_idx",
            ]
            .iter()
            .any(|sfx| {
                name.strip_suffix(sfx)
                    .is_some_and(|p| vtab_names.contains(p))
            })
        };
        // A vtab is persistent (has rows to copy through it) iff a backing table
        // exists — the generic `_data` or an R-Tree's `_node`.
        let persistent_vtab = |name: &str| {
            vtab_names.contains(name)
                && (table_names.contains(&alloc::format!("{name}_data"))
                    || table_names.contains(&alloc::format!("{name}_node")))
        };

        // Build a compact copy in a throwaway in-memory database.
        let mut tmp = Connection::open_memory()?;
        // 1. Tables (this also recreates their automatic indexes). Skip a vtab's
        //    backing table — its `CREATE VIRTUAL TABLE` recreates it.
        for (ty, name, sql) in &objs {
            if *ty == ObjectType::Table && !is_backing(name) {
                if let Some(s) = sql {
                    tmp.execute(s)?;
                }
            }
        }
        // 2. Explicit secondary indexes (auto-indexes have no SQL).
        for (ty, _, sql) in &objs {
            if *ty == ObjectType::Index {
                if let Some(s) = sql {
                    tmp.execute(s)?;
                }
            }
        }
        // 3. Re-insert every table's rows (before triggers exist, so none fire).
        //    Skip a vtab's backing table (repopulated through the vtab) and a
        //    computed vtab (no rows); a persistent vtab is copied via the vtab,
        //    whose INSERTs rewrite the backing table.
        for (ty, name, _) in &objs {
            if *ty != ObjectType::Table
                || is_backing(name)
                || (vtab_names.contains(name) && !persistent_vtab(name))
            {
                continue;
            }
            let result = self.query(&alloc::format!("SELECT * FROM {}", quote(name)))?;
            let ncols = result.columns.len();
            if ncols == 0 {
                continue;
            }
            let placeholders = (1..=ncols)
                .map(|i| alloc::format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let stmt = alloc::format!("INSERT INTO {} VALUES ({placeholders})", quote(name));
            for row in result.rows {
                let params = Params {
                    positional: row,
                    named: Vec::new(),
                };
                tmp.execute_params(&stmt, &params)?;
            }
        }
        // 4. Views, then 5. triggers (last, so loading data didn't fire them).
        for (ty, _, sql) in &objs {
            if *ty == ObjectType::View {
                if let Some(s) = sql {
                    tmp.execute(s)?;
                }
            }
        }
        for (ty, _, sql) in &objs {
            if *ty == ObjectType::Trigger {
                if let Some(s) = sql {
                    tmp.execute(s)?;
                }
            }
        }

        // Snapshot the compact image's pages.
        let count = tmp.backend.source().page_count();
        let mut image = Vec::with_capacity(count as usize);
        for n in 1..=count {
            image.push(tmp.backend.source().page(n)?.data().to_vec());
        }

        // `VACUUM … INTO <file>`: write the image to a new database file.
        if let Some(expr) = into {
            return self.vacuum_write_into(expr, image);
        }

        // Plain `VACUUM`: copy the compact image's pages over the current file.
        self.backend.writer()?.replace_image(image)?;

        // Preserve user_version across the rebuild.
        if user_version != 0 {
            self.backend.writer()?.header_mut().user_version = user_version;
            // Re-stamp page 1 via a commit.
            let mut page1 = self.backend.writer()?.read_page(1)?;
            self.backend.writer()?.header().write_to(&mut page1)?;
            self.backend.writer()?.write_page(1, page1)?;
            self.backend.writer()?.commit()?;
        }
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    /// Write a freshly-built compact page `image` to a NEW database file for
    /// `VACUUM … INTO <file>`. The target path comes from evaluating `expr`; it
    /// must not already exist (matching SQLite). `std`-only — creating a file
    /// needs the OS VFS.
    #[cfg(feature = "std")]
    fn vacuum_write_into(&self, expr: &Expr, image: Vec<Vec<u8>>) -> Result<()> {
        let params = Params::default();
        let path = match eval::eval(expr, &EvalCtx::rowless(&params))? {
            Value::Null => return Err(Error::Error("VACUUM INTO target is NULL".into())),
            Value::Text(s) => s,
            other => eval::to_text(&other),
        };
        if std::path::Path::new(&path).exists() {
            return Err(Error::Error(alloc::format!(
                "output file already exists: {path}"
            )));
        }
        let user_version = self.backend.source().header().user_version;
        let mut dst = Connection::create(&path)?;
        dst.backend.writer()?.replace_image(image)?;
        if user_version != 0 {
            dst.backend.writer()?.header_mut().user_version = user_version;
        }
        // Stamp page 1 (the header, incl. user_version) and flush to disk.
        let mut page1 = dst.backend.writer()?.read_page(1)?;
        dst.backend.writer()?.header().write_to(&mut page1)?;
        dst.backend.writer()?.write_page(1, page1)?;
        dst.backend.writer()?.commit()?;
        Ok(())
    }

    /// Without `std` there is no file VFS to create the target, so `VACUUM …
    /// INTO` is unsupported (the in-place form still works).
    #[cfg(not(feature = "std"))]
    fn vacuum_write_into(&self, _expr: &Expr, _image: Vec<Vec<u8>>) -> Result<()> {
        Err(Error::Error(
            "VACUUM INTO requires the std feature (file I/O)".into(),
        ))
    }

    /// `ANALYZE`: gather index selectivity statistics into the `sqlite_stat1`
    /// table. The `stat` string for an index is `nRow avgEq1 avgEq2 …`, where
    /// `avgEqK = (nRow + dK/2) / dK` and `dK` is the number of distinct values of
    /// the index's leftmost `K` columns — the same integers SQLite records. A
    /// table with no index gets a single `(tbl, NULL, nRow)` row.
    fn exec_analyze(&mut self, target: Option<&str>) -> Result<()> {
        use crate::schema::ObjectType;
        // Which user tables to (re)analyze.
        let analyze: Vec<String> = match target {
            None => self
                .schema
                .objects()
                .iter()
                .filter(|o| o.obj_type == ObjectType::Table && !o.name.starts_with("sqlite_"))
                .map(|o| o.name.clone())
                .collect(),
            Some(name) => {
                if let Some(t) = self.schema.table(name) {
                    alloc::vec![t.name.clone()]
                } else if let Some(ix) = self.schema.index(name) {
                    alloc::vec![ix.tbl_name.clone()]
                } else {
                    return Ok(()); // unknown object: no-op, like SQLite
                }
            }
        };

        // Compute the new stat rows up front (read-only phase).
        let mut new_rows: Vec<(String, Option<String>, String)> = Vec::new();
        for tname in &analyze {
            let meta = self.table_meta(tname, None)?;
            let rows: Vec<Vec<Value>> = if meta.without_rowid {
                self.scan_without_rowid(&meta)?
            } else {
                self.scan_table(&meta)?
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect()
            };
            let n = rows.len();
            let indexes = self.indexes_of(tname)?;
            if indexes.is_empty() {
                if n > 0 {
                    new_rows.push((tname.clone(), None, alloc::format!("{n}")));
                }
            } else {
                for idx in &indexes {
                    if n == 0 {
                        continue; // SQLite records nothing for an empty index
                    }
                    let stat = index_stat_string(&idx.cols, &idx.collations, &rows);
                    new_rows.push((tname.clone(), Some(idx.name.clone()), stat));
                }
            }
        }

        // Ensure the sqlite_stat1 catalog table exists.
        if self.schema.table("sqlite_stat1").is_none() {
            const STAT1_SQL: &str = "CREATE TABLE sqlite_stat1(tbl,idx,stat)";
            let Statement::CreateTable(ct) = sql::parse_one(STAT1_SQL)? else {
                unreachable!()
            };
            self.exec_create_table(&ct, STAT1_SQL)?;
        }
        let stat_root = self.schema.table("sqlite_stat1").unwrap().rootpage;

        // Replace existing rows for the analyzed tables.
        let stat_meta = self.table_meta("sqlite_stat1", None)?;
        let victims: Vec<i64> = self
            .scan_table(&stat_meta)?
            .into_iter()
            .filter(
                |(_, vals)| matches!(&vals[0], Value::Text(t) if analyze.iter().any(|a| a == t)),
            )
            .map(|(rid, _)| rid)
            .collect();
        for rid in victims {
            delete_table(self.backend.writer()?, stat_root, rid)?;
        }

        let base = self.next_rowid(stat_root)?;
        for (i, (tbl, idx, stat)) in new_rows.into_iter().enumerate() {
            let rec = encode_record(&[
                Value::Text(tbl),
                idx.map_or(Value::Null, Value::Text),
                Value::Text(stat),
            ]);
            insert_table(self.backend.writer()?, stat_root, base + i as i64, &rec)?;
        }
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    // ---- DDL / DML ----------------------------------------------------------

    fn exec_create_table(&mut self, ct: &CreateTable, sql_text: &str) -> Result<()> {
        if let Some(select) = &ct.as_select {
            return self.exec_create_table_as_select(ct, select);
        }
        // The schema-qualified form (`CREATE TABLE aux.t …`) must be stored in the
        // target database's catalog WITHOUT the `schema.` prefix — otherwise the
        // stored SQL is invalid in that database's own namespace (and unreadable
        // by sqlite3). Reprint the bare-name form when a qualifier was present.
        let reprinted;
        let sql_text = if ct.schema.is_some() {
            reprinted = sql::print::create_table(ct);
            reprinted.as_str()
        } else {
            sql_text
        };
        if self.schema.table(&ct.name).is_some() {
            if ct.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("table {} already exists", ct.name)));
        }
        // STRICT tables restrict column types to the six rigid types; reject any
        // other (or missing) declared type at CREATE, like SQLite.
        if ct.strict {
            for c in &ct.columns {
                if strict_column_type(c.type_name.as_deref()).is_none() {
                    return Err(match &c.type_name {
                        Some(t) => Error::Error(format!(
                            "unknown datatype for {}.{}: \"{t}\"",
                            ct.name, c.name
                        )),
                        None => {
                            Error::Error(format!("missing datatype for {}.{}", ct.name, c.name))
                        }
                    });
                }
            }
        }
        // SQLite forbids subqueries in CHECK constraints and generated columns.
        for c in &ct.columns {
            for k in &c.constraints {
                match k {
                    ColumnConstraint::Check(e, _) if expr_has_subquery(e) => {
                        return Err(Error::Error(
                            "subqueries prohibited in CHECK constraints".into(),
                        ));
                    }
                    ColumnConstraint::Generated { expr, .. } if expr_has_subquery(expr) => {
                        return Err(Error::Error(
                            "subqueries prohibited in generated columns".into(),
                        ));
                    }
                    ColumnConstraint::Generated { expr, .. } if expr_is_nondeterministic(expr) => {
                        return Err(Error::Error(
                            "non-deterministic functions prohibited in generated columns".into(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        for tc in &ct.constraints {
            if let TableConstraint::Check(e, _) = tc {
                if expr_has_subquery(e) {
                    return Err(Error::Error(
                        "subqueries prohibited in CHECK constraints".into(),
                    ));
                }
            }
        }
        // A CHECK / generated-column expression may reference only the table's own
        // columns, like SQLite (which rejects an unknown column at CREATE). A
        // generated column additionally may not reference the rowid; a CHECK may.
        let known: Vec<String> = ct.columns.iter().map(|c| c.name.clone()).collect();
        for c in &ct.columns {
            for k in &c.constraints {
                let bad = match k {
                    ColumnConstraint::Check(e, _) => unknown_column_ref(e, &known, true),
                    ColumnConstraint::Generated { expr, .. } => {
                        unknown_column_ref(expr, &known, false)
                    }
                    _ => None,
                };
                if let Some(col) = bad {
                    return Err(Error::Error(format!("no such column: {col}")));
                }
            }
        }
        for tc in &ct.constraints {
            if let TableConstraint::Check(e, _) = tc {
                if let Some(col) = unknown_column_ref(e, &known, true) {
                    return Err(Error::Error(format!("no such column: {col}")));
                }
            }
        }
        // A table must have at least one non-generated (real) column, as in SQLite.
        if !ct.columns.is_empty()
            && ct.columns.iter().all(|c| {
                c.constraints
                    .iter()
                    .any(|k| matches!(k, ColumnConstraint::Generated { .. }))
            })
        {
            return Err(Error::Error(
                "must have at least one non-generated column".into(),
            ));
        }
        // Duplicate column names are rejected.
        for (i, c) in ct.columns.iter().enumerate() {
            if ct.columns[..i]
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(&c.name))
            {
                return Err(Error::Error(alloc::format!(
                    "duplicate column name: {}",
                    c.name
                )));
            }
        }
        // At most one PRIMARY KEY (column-level + table-level).
        let pk_count = ct
            .columns
            .iter()
            .flat_map(|c| &c.constraints)
            .filter(|k| matches!(k, ColumnConstraint::PrimaryKey { .. }))
            .count()
            + ct.constraints
                .iter()
                .filter(|tc| matches!(tc, TableConstraint::PrimaryKey(..)))
                .count();
        if pk_count > 1 {
            return Err(Error::Error(alloc::format!(
                "table {} has more than one primary key",
                ct.name
            )));
        }
        // Table-level PRIMARY KEY/UNIQUE column lists must name real columns.
        for tc in &ct.constraints {
            let cols = match tc {
                TableConstraint::PrimaryKey(cols, _) | TableConstraint::Unique(cols, _) => cols,
                _ => continue,
            };
            for name in cols {
                if !ct.columns.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
                    return Err(Error::Error(alloc::format!("no such column: {name}")));
                }
            }
        }
        // AUTOINCREMENT is only valid on a rowid `INTEGER PRIMARY KEY` column.
        let ipk = find_integer_primary_key(ct);
        let has_autoinc = |i: usize| {
            ct.columns[i].constraints.iter().any(|k| {
                matches!(
                    k,
                    ColumnConstraint::PrimaryKey {
                        autoincrement: true,
                        ..
                    }
                )
            })
        };
        if (0..ct.columns.len()).any(has_autoinc) {
            if ct.without_rowid {
                return Err(Error::Error(
                    "AUTOINCREMENT not allowed on WITHOUT ROWID tables".into(),
                ));
            }
            if !(0..ct.columns.len()).any(|i| has_autoinc(i) && Some(i) == ipk) {
                return Err(Error::Error(
                    "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY".into(),
                ));
            }
        }
        // A WITHOUT ROWID table is stored as a PK-clustered index b-tree; an
        // ordinary table uses a rowid table b-tree.
        let root = if ct.without_rowid {
            // A WITHOUT ROWID table must have a PRIMARY KEY (it is the b-tree key).
            if primary_key_positions(ct).is_empty() {
                return Err(Error::Error(
                    "WITHOUT ROWID table must have a PRIMARY KEY".into(),
                ));
            }
            create_index_root(self.backend.writer()?)?
        } else {
            create_table_root(self.backend.writer()?)?
        };
        let next = self.next_rowid(crate::schema::SCHEMA_ROOT_PAGE)?;
        let row = encode_record(&[
            Value::Text("table".into()),
            Value::Text(ct.name.clone()),
            Value::Text(ct.name.clone()),
            Value::Integer(root as i64),
            Value::Text(sql_text.into()),
        ]);
        insert_table(
            self.backend.writer()?,
            crate::schema::SCHEMA_ROOT_PAGE,
            next,
            &row,
        )?;

        // Create the automatic indexes SQLite implies for UNIQUE / non-rowid
        // PRIMARY KEY constraints, so the file is a valid SQLite database (it
        // otherwise reports "wrong # of entries in index sqlite_autoindex_*").
        // For a WITHOUT ROWID table the PRIMARY KEY *is* the table (no separate
        // b-tree), but it still consumes its `sqlite_autoindex_<t>_<n>` slot.
        let ipk = if ct.without_rowid {
            None
        } else {
            find_integer_primary_key(ct)
        };
        let unique = collect_unique_sets(ct, ipk);
        let pk = if ct.without_rowid {
            primary_key_positions(ct)
        } else {
            Vec::new()
        };
        let mut schema_rowid = next + 1;
        for (n, (set, _)) in unique.iter().enumerate() {
            // The clustered PRIMARY KEY of a WITHOUT ROWID table gets no b-tree.
            if ct.without_rowid && *set == pk {
                continue;
            }
            let idx_root = create_index_root(self.backend.writer()?)?;
            let idx_row = encode_record(&[
                Value::Text("index".into()),
                Value::Text(alloc::format!("sqlite_autoindex_{}_{}", ct.name, n + 1)),
                Value::Text(ct.name.clone()),
                Value::Integer(idx_root as i64),
                Value::Null, // automatic indexes carry no CREATE SQL
            ]);
            insert_table(
                self.backend.writer()?,
                crate::schema::SCHEMA_ROOT_PAGE,
                schema_rowid,
                &idx_row,
            )?;
            schema_rowid += 1;
        }

        // An `AUTOINCREMENT` table requires the `sqlite_sequence` catalog, which
        // SQLite creates (empty) the first time such a table is created.
        let is_autoinc = ipk.is_some_and(|i| {
            ct.columns[i].constraints.iter().any(|k| {
                matches!(
                    k,
                    ColumnConstraint::PrimaryKey {
                        autoincrement: true,
                        ..
                    }
                )
            })
        });
        if is_autoinc && self.schema.table("sqlite_sequence").is_none() {
            const SEQ_SQL: &str = "CREATE TABLE sqlite_sequence(name,seq)";
            let Statement::CreateTable(seq_ct) = sql::parse_one(SEQ_SQL)? else {
                unreachable!()
            };
            self.exec_create_table(&seq_ct, SEQ_SQL)?;
        }

        let cookie = self
            .backend
            .writer()?
            .header()
            .schema_cookie
            .wrapping_add(1);
        self.backend.writer()?.header_mut().schema_cookie = cookie;
        // Make the new table visible to subsequent statements in this tx.
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    /// `CREATE TABLE name AS SELECT …`: create a table whose columns are the
    /// query's output labels (no declared types/constraints), then populate it
    /// with the query's rows.
    fn exec_create_table_as_select(&mut self, ct: &CreateTable, select: &Select) -> Result<()> {
        if self.schema.table(&ct.name).is_some() {
            if ct.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("table {} already exists", ct.name)));
        }
        let result = self.run_select(select, &Params::default())?;
        // Build and create the resolved table `name(col1, col2, …)`.
        let cols = result
            .columns
            .iter()
            .map(|c| crate::sql::print::ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let create_sql = format!(
            "CREATE TABLE {}({cols})",
            crate::sql::print::ident(&ct.name)
        );
        let Statement::CreateTable(syn) = sql::parse_one(&create_sql)? else {
            return Err(Error::Corrupt("generated CTAS schema is invalid".into()));
        };
        self.exec_create_table(&syn, &create_sql)?;
        // Populate it with the query's rows via the normal insert path.
        if !result.rows.is_empty() {
            let value_rows: Vec<Vec<Expr>> = result
                .rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|v| Expr::Literal(value_to_literal(v)))
                        .collect()
                })
                .collect();
            let ins = Insert {
                table: ct.name.clone(),
                schema: None,
                columns: Vec::new(),
                source: InsertSource::Values(value_rows),
                on_conflict: OnConflict::Abort,
                // A VACUUM re-insert of already-valid rows keeps the plain default.
                on_conflict_explicit: true,
                upsert: Vec::new(),
                returning: Vec::new(),
            };
            self.exec_insert(&ins, &Params::default())?;
        }
        Ok(())
    }

    /// Handle a settable `PRAGMA` (currently only `foreign_keys`). Unknown
    /// pragmas are accepted as no-ops, matching SQLite's leniency.
    fn exec_pragma(&mut self, p: &Pragma, params: &Params) -> Result<()> {
        if p.name.eq_ignore_ascii_case("foreign_keys") {
            if let Some(e) = &p.value {
                self.foreign_keys = pragma_truth(e, params);
            }
        } else if p.name.eq_ignore_ascii_case("recursive_triggers") {
            if let Some(e) = &p.value {
                self.recursive_triggers = pragma_truth(e, params);
            }
        } else if p.name.eq_ignore_ascii_case("cache_size") {
            // Round-trip the value verbatim (graphite keeps all pages resident, so
            // it changes nothing) — `PRAGMA cache_size` then reports it back.
            if let Some(e) = &p.value {
                self.cache_size
                    .set(eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?));
            }
        } else if p.name.eq_ignore_ascii_case("analysis_limit") {
            // The ANALYZE sample cap (advisory here); store it, clamping a negative
            // value to 0 like sqlite, so a later `PRAGMA analysis_limit` reads back.
            if let Some(e) = &p.value {
                let v = eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?);
                self.analysis_limit.set(v.max(0));
            }
        } else if p.name.eq_ignore_ascii_case("busy_timeout") {
            // Advisory (graphite never blocks on a lock); store it, clamping a
            // negative value to 0, so a later `PRAGMA busy_timeout` reads it back.
            if let Some(e) = &p.value {
                let v = eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?);
                self.busy_timeout.set(v.max(0));
            }
        } else if p.name.eq_ignore_ascii_case("secure_delete") {
            // sqlite maps the argument to 0 (off), 2 (the `fast` keyword only), or
            // 1 (any other true / non-zero value). The pager zeroes freed pages
            // when the setting is non-zero.
            if let Some(e) = &p.value {
                let v = match pragma_text(e).to_ascii_lowercase().as_str() {
                    "fast" => 2,
                    _ if pragma_truth(e, params) => 1,
                    _ => 0,
                };
                self.secure_delete.set(v);
                if let Backend::Write(w) = &mut self.backend {
                    w.set_secure_delete(v != 0);
                }
            }
        } else if p.name.eq_ignore_ascii_case("journal_mode") {
            if let Some(e) = &p.value {
                if pragma_text(e).eq_ignore_ascii_case("wal") {
                    self.backend.writer()?.set_wal_mode()?;
                }
                // Other modes (delete/truncate/persist/memory/off) keep the
                // rollback-journal path; switching back out of WAL is a no-op.
            }
        } else if p.name.eq_ignore_ascii_case("wal_checkpoint") {
            self.backend.writer()?.checkpoint()?;
        } else if p.name.eq_ignore_ascii_case("user_version") {
            if let Some(e) = &p.value {
                let v = eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?) as u32;
                self.backend.writer()?.header_mut().user_version = v;
            }
        } else if p.name.eq_ignore_ascii_case("application_id") {
            if let Some(e) = &p.value {
                let v = eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?) as u32;
                self.backend.writer()?.header_mut().application_id = v;
            }
        } else if p.name.eq_ignore_ascii_case("auto_vacuum") {
            if let Some(e) = &p.value {
                // Accept the symbolic and numeric spellings.
                let mode = match pragma_text(e).to_ascii_lowercase().as_str() {
                    "none" => 0,
                    "full" => 1,
                    "incremental" => 2,
                    _ => eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?),
                };
                // SQLite only honours a change of auto-vacuum mode on an *empty*
                // database (before any table is created); afterwards it is a
                // no-op until the next VACUUM. graphite mirrors that: on an empty
                // database we stamp the header into the requested mode and the
                // pager maintains pointer-map pages from then on; on a non-empty
                // database the pragma is silently ignored.
                let target = match mode {
                    0 => AutoVacuum::None,
                    1 => AutoVacuum::Full,
                    2 => AutoVacuum::Incremental,
                    _ => return Err(Error::Error(format!("invalid auto_vacuum mode {mode}"))),
                };
                self.backend.writer()?.set_auto_vacuum_if_empty(target)?;
            }
        } else if p.name.eq_ignore_ascii_case("incremental_vacuum") {
            // `PRAGMA incremental_vacuum` (or `= N` / `(N)`): reclaim up to N free
            // pages off the end of an `auto_vacuum=INCREMENTAL` database. With no
            // argument (or N <= 0) reclaim as many as possible. The pager makes it
            // a no-op for NONE/FULL, mirroring SQLite. The reclamation is staged
            // like any other write; the caller's normal commit (the implicit
            // auto-commit when not in a transaction, or an explicit COMMIT) flushes
            // the now-smaller file to disk.
            let n = match &p.value {
                Some(e) => eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?),
                None => 0,
            };
            self.backend.writer()?.incremental_vacuum(n)?;
        }
        Ok(())
    }

    /// The foreign keys declared by `table`, with child columns resolved and
    /// parent columns defaulted to the parent's primary key when omitted.
    fn foreign_keys_of(&self, table: &str) -> Result<Vec<ForeignKey>> {
        let Some(obj) = self.schema.table(table) else {
            return Ok(Vec::new());
        };
        let Some(sql) = &obj.sql else {
            return Ok(Vec::new());
        };
        let Statement::CreateTable(ct) = sql::parse_one(sql)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for col in &ct.columns {
            for c in &col.constraints {
                if let ColumnConstraint::References(fk) = c {
                    out.push(self.resolve_fk(fk)?);
                }
            }
        }
        for c in &ct.constraints {
            if let TableConstraint::ForeignKey(fk) = c {
                out.push(self.resolve_fk(fk)?);
            }
        }
        Ok(out)
    }

    /// Fill in a foreign key's parent columns from the parent's primary key when
    /// the `REFERENCES` clause omitted them.
    fn resolve_fk(&self, fk: &ForeignKey) -> Result<ForeignKey> {
        let mut fk = fk.clone();
        if fk.ref_columns.is_empty() {
            fk.ref_columns = self.primary_key_columns(&fk.ref_table)?;
        }
        Ok(fk)
    }

    /// The primary-key column names of `table` (the INTEGER PRIMARY KEY, or a
    /// declared PRIMARY KEY constraint).
    fn primary_key_columns(&self, table: &str) -> Result<Vec<String>> {
        let Some(obj) = self.schema.table(table) else {
            return Err(Error::Error(format!("no such table: {table}")));
        };
        let sql = obj.sql.as_deref().unwrap_or("");
        let Statement::CreateTable(ct) = sql::parse_one(sql)? else {
            return Ok(Vec::new());
        };
        for col in &ct.columns {
            if col
                .constraints
                .iter()
                .any(|c| matches!(c, ColumnConstraint::PrimaryKey { .. }))
            {
                return Ok(alloc::vec![col.name.clone()]);
            }
        }
        for c in &ct.constraints {
            if let TableConstraint::PrimaryKey(cols, _) = c {
                return Ok(cols.clone());
            }
        }
        Ok(Vec::new())
    }

    /// Verify, for a row being inserted/updated into `table`, that every foreign
    /// key it declares points at an existing parent row. NULL key columns are
    /// skipped (MATCH SIMPLE).
    fn check_fk_child(&self, table: &str, meta: &TableMeta, values: &[Value]) -> Result<()> {
        if !self.foreign_keys {
            return Ok(());
        }
        for fk in self.foreign_keys_of(table)? {
            // A `DEFERRABLE INITIALLY DEFERRED` key is checked at COMMIT, not now
            // — but only inside an explicit transaction. In autocommit the
            // statement *is* the transaction, so its implicit commit is immediate.
            if fk.initially_deferred && self.in_tx {
                continue;
            }
            let key = match self.child_key_values(meta, &fk, values) {
                Some(k) => k,
                None => continue, // a NULL column => constraint satisfied
            };
            if !self.parent_has_key(&fk, &key)? {
                return Err(Error::Constraint("FOREIGN KEY constraint failed".into()));
            }
        }
        Ok(())
    }

    /// Verify every `DEFERRABLE INITIALLY DEFERRED` foreign key across all tables
    /// — run at `COMMIT` to catch a constraint that was temporarily violated
    /// inside the transaction and never repaired.
    fn check_deferred_fks(&self) -> Result<()> {
        if !self.foreign_keys {
            return Ok(());
        }
        for obj in self.schema.objects() {
            if obj.obj_type != crate::schema::ObjectType::Table {
                continue;
            }
            let fks: Vec<ForeignKey> = self
                .foreign_keys_of(&obj.name)?
                .into_iter()
                .filter(|fk| fk.initially_deferred)
                .collect();
            if fks.is_empty() {
                continue;
            }
            let meta = self.table_meta(&obj.name, None)?;
            for (_, row) in self.scan_table(&meta)? {
                for fk in &fks {
                    if let Some(key) = self.child_key_values(&meta, fk, &row) {
                        if !self.parent_has_key(fk, &key)? {
                            return Err(Error::Constraint("FOREIGN KEY constraint failed".into()));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The child key values for `fk` from a child row, or `None` if any is NULL.
    fn child_key_values(
        &self,
        meta: &TableMeta,
        fk: &ForeignKey,
        values: &[Value],
    ) -> Option<Vec<Value>> {
        let mut key = Vec::with_capacity(fk.columns.len());
        for cname in &fk.columns {
            let pos = meta
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(cname))?;
            let v = values.get(pos)?;
            if matches!(v, Value::Null) {
                return None;
            }
            key.push(v.clone());
        }
        Some(key)
    }

    /// Whether the parent table of `fk` has a row whose referenced columns equal
    /// `key`.
    fn parent_has_key(&self, fk: &ForeignKey, key: &[Value]) -> Result<bool> {
        let pmeta = self.table_meta(&fk.ref_table, None)?;
        let positions = self.column_positions(&pmeta, &fk.ref_columns)?;
        for (_, row) in self.scan_table(&pmeta)? {
            if positions
                .iter()
                .zip(key)
                .all(|(&p, k)| eval::compare(&row[p], k) == core::cmp::Ordering::Equal)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Column positions in `meta` for the given names.
    fn column_positions(&self, meta: &TableMeta, names: &[String]) -> Result<Vec<usize>> {
        names
            .iter()
            .map(|n| {
                meta.columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(n))
                    .ok_or_else(|| Error::Error(format!("no such column: {n}")))
            })
            .collect()
    }

    /// Enforce referential actions when a parent row changes. `old_key` is the
    /// parent row's referenced-column values before the change; `new_key` is the
    /// values after (for `UPDATE`), or `None` for `DELETE`.
    fn enforce_parent_change(
        &mut self,
        parent_table: &str,
        old_vals: &[Value],
        new_vals: Option<&[Value]>,
        params: &Params,
    ) -> Result<()> {
        if !self.foreign_keys {
            return Ok(());
        }
        // Find every (child table, fk) that references this parent.
        let table_names: Vec<String> = self
            .schema
            .objects()
            .iter()
            .filter(|o| o.obj_type == crate::schema::ObjectType::Table)
            .map(|o| o.name.clone())
            .collect();
        let mut referencing: Vec<(String, ForeignKey)> = Vec::new();
        for name in table_names {
            for fk in self.foreign_keys_of(&name)? {
                if fk.ref_table.eq_ignore_ascii_case(parent_table) {
                    referencing.push((name.clone(), fk));
                }
            }
        }
        if referencing.is_empty() {
            return Ok(());
        }
        let pmeta = self.table_meta(parent_table, None)?;
        for (child_table, fk) in referencing {
            let ppos = self.column_positions(&pmeta, &fk.ref_columns)?;
            let old_key: Vec<Value> = ppos.iter().map(|&p| old_vals[p].clone()).collect();
            // A NULL parent key can't be referenced.
            if old_key.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            let is_delete = new_vals.is_none();
            let action = if is_delete {
                fk.on_delete
            } else {
                fk.on_update
            };
            // A deferred FK's NO ACTION orphan check waits for COMMIT (inside an
            // explicit transaction); RESTRICT and the data-changing actions
            // (CASCADE / SET NULL / SET DEFAULT) always run now.
            if action == FkAction::NoAction && fk.initially_deferred && self.in_tx {
                continue;
            }
            // If this is an UPDATE that didn't change the referenced key, skip.
            if let Some(nv) = new_vals {
                let new_key: Vec<Value> = ppos.iter().map(|&p| nv[p].clone()).collect();
                if new_key
                    .iter()
                    .zip(&old_key)
                    .all(|(a, b)| eval::compare(a, b) == core::cmp::Ordering::Equal)
                {
                    continue;
                }
            }
            self.apply_fk_action(&child_table, &fk, &old_key, new_vals, &ppos, action, params)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_fk_action(
        &mut self,
        child_table: &str,
        fk: &ForeignKey,
        old_key: &[Value],
        new_parent: Option<&[Value]>,
        parent_pos: &[usize],
        action: FkAction,
        params: &Params,
    ) -> Result<()> {
        let cmeta = self.table_meta(child_table, None)?;
        let cpos = self.column_positions(&cmeta, &fk.columns)?;
        // Find child rowids whose key matches old_key.
        let mut matches: Vec<i64> = Vec::new();
        for (rowid, row) in self.scan_table(&cmeta)? {
            if cpos
                .iter()
                .zip(old_key)
                .all(|(&p, k)| eval::compare(&row[p], k) == core::cmp::Ordering::Equal)
            {
                matches.push(rowid);
            }
        }
        if matches.is_empty() {
            return Ok(());
        }
        match action {
            FkAction::NoAction | FkAction::Restrict => {
                Err(Error::Constraint("FOREIGN KEY constraint failed".into()))
            }
            FkAction::Cascade if new_parent.is_none() => {
                // DELETE CASCADE: delete the matching child rows (recursively).
                for rowid in matches {
                    self.delete_row_cascade(child_table, &cmeta, rowid, params)?;
                }
                Ok(())
            }
            FkAction::Cascade => {
                // UPDATE CASCADE: set child key columns to the new parent key.
                let new_parent = new_parent.unwrap();
                let new_key: Vec<Value> =
                    parent_pos.iter().map(|&p| new_parent[p].clone()).collect();
                for rowid in matches {
                    self.update_child_key(&cmeta, child_table, rowid, &cpos, &new_key)?;
                }
                Ok(())
            }
            FkAction::SetNull => {
                let nulls = alloc::vec![Value::Null; cpos.len()];
                for rowid in matches {
                    self.update_child_key(&cmeta, child_table, rowid, &cpos, &nulls)?;
                }
                Ok(())
            }
            FkAction::SetDefault => {
                let defaults: Vec<Value> = cpos
                    .iter()
                    .map(|&p| match &cmeta.defaults[p] {
                        Some(e) => eval::eval(e, &EvalCtx::rowless(params)).unwrap_or(Value::Null),
                        None => Value::Null,
                    })
                    .collect();
                for rowid in matches {
                    self.update_child_key(&cmeta, child_table, rowid, &cpos, &defaults)?;
                }
                Ok(())
            }
        }
    }

    /// Delete one child row by rowid, first cascading to its own children.
    fn delete_row_cascade(
        &mut self,
        table: &str,
        meta: &TableMeta,
        rowid: i64,
        params: &Params,
    ) -> Result<()> {
        // Read the row so its own dependents can be enforced.
        let old = self.read_row(meta, rowid)?;
        if let Some(old) = old {
            self.enforce_parent_change(table, &old, None, params)?;
        }
        delete_table(self.backend.writer()?, meta.root, rowid)?;
        let indexes = self.indexes_of(table)?;
        if !indexes.is_empty() {
            self.rebuild_indexes(meta, &indexes)?;
        }
        Ok(())
    }

    /// Set specific columns of a child row (by position) to new values.
    fn update_child_key(
        &mut self,
        meta: &TableMeta,
        table: &str,
        rowid: i64,
        positions: &[usize],
        new_vals: &[Value],
    ) -> Result<()> {
        let Some(mut row) = self.read_row(meta, rowid)? else {
            return Ok(());
        };
        for (&p, v) in positions.iter().zip(new_vals) {
            row[p] = v.clone();
        }
        // Re-encode and rewrite the row (rowid unchanged here).
        let mut stored = row.clone();
        if let Some(ipk) = meta.ipk {
            stored[ipk] = Value::Null;
        }
        let record = encode_record(&stored);
        insert_table(self.backend.writer()?, meta.root, rowid, &record)?;
        let indexes = self.indexes_of(table)?;
        if !indexes.is_empty() {
            self.rebuild_indexes(meta, &indexes)?;
        }
        Ok(())
    }

    /// Read a single row's full column values by rowid (IPK filled in), or None.
    fn read_row(&self, meta: &TableMeta, rowid: i64) -> Result<Option<Vec<Value>>> {
        let encoding = self.backend.source().header().text_encoding;
        let mut cur = TableCursor::new(self.backend.source(), meta.root);
        if cur.seek(rowid)? {
            let values = self.decode_full_row(meta, rowid, &cur.payload()?, encoding)?;
            Ok(Some(values))
        } else {
            Ok(None)
        }
    }

    /// Store a `CREATE TRIGGER` in `sqlite_schema` (type `trigger`, no b-tree).
    fn exec_create_trigger(&mut self, ct: &CreateTrigger, sql_text: &str) -> Result<()> {
        // A schema-qualified `CREATE TRIGGER aux.tr …` stores its SQL bare-named.
        let stripped;
        let sql_text = match ct.schema.as_deref() {
            Some(s) => {
                stripped = strip_schema_qualifier(sql_text, s)?;
                stripped.as_str()
            }
            None => sql_text,
        };
        if self
            .schema
            .objects()
            .iter()
            .any(|o| o.name.eq_ignore_ascii_case(&ct.name))
        {
            if ct.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("trigger {} already exists", ct.name)));
        }
        // The target may be a table or (for INSTEAD OF triggers) a view. A temp
        // trigger may fire on a main table, so when the temp database is the active
        // schema also consult the swapped-out catalog (which then holds main).
        let table_in_other = self
            .temp_db
            .as_ref()
            .is_some_and(|t| t.schema.table(&ct.table).is_some());
        if self.schema.table(&ct.table).is_none() && !table_in_other && !self.is_view(&ct.table) {
            return Err(Error::Error(format!("no such table: {}", ct.table)));
        }
        let next = self.next_rowid(crate::schema::SCHEMA_ROOT_PAGE)?;
        let row = encode_record(&[
            Value::Text("trigger".into()),
            Value::Text(ct.name.clone()),
            Value::Text(ct.table.clone()),
            Value::Integer(0),
            Value::Text(sql_text.into()),
        ]);
        insert_table(
            self.backend.writer()?,
            crate::schema::SCHEMA_ROOT_PAGE,
            next,
            &row,
        )?;
        let cookie = self
            .backend
            .writer()?
            .header()
            .schema_cookie
            .wrapping_add(1);
        self.backend.writer()?.header_mut().schema_cookie = cookie;
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    /// Triggers on `table` matching `kind`/`timing`, parsed from their schema SQL.
    fn triggers_for(
        &self,
        table: &str,
        kind: TrigEvent,
        timing: TriggerTiming,
    ) -> Result<Vec<CreateTrigger>> {
        let mut out = Vec::new();
        // The active schema plus the temp catalog: a temp trigger fires on writes
        // to its (possibly main) table, and a main trigger fires even while a temp
        // database is swapped in. `swap_db` exchanges `self.schema` with the temp
        // db's, so these two catalogs are always exactly {main, temp}.
        self.collect_triggers(self.schema.objects(), table, kind, timing, &mut out);
        if let Some(t) = &self.temp_db {
            self.collect_triggers(t.schema.objects(), table, kind, timing, &mut out);
        }
        // SQLite keeps a per-table trigger list that prepends on creation, so
        // triggers of the same event/timing fire in REVERSE creation order
        // (most-recently-created first). `objects()` is in creation order, so
        // reverse to match.
        out.reverse();
        Ok(out)
    }

    /// Append triggers from `objects` matching `table`/`kind`/`timing` to `out`.
    fn collect_triggers(
        &self,
        objects: &[crate::schema::SchemaObject],
        table: &str,
        kind: TrigEvent,
        timing: TriggerTiming,
        out: &mut Vec<CreateTrigger>,
    ) {
        for obj in objects {
            if obj.obj_type != crate::schema::ObjectType::Trigger
                || !obj.tbl_name.eq_ignore_ascii_case(table)
            {
                continue;
            }
            let Some(sql) = &obj.sql else { continue };
            let Ok(Statement::CreateTrigger(ct)) = sql::parse_one(sql) else {
                continue;
            };
            let event_ok = matches!(
                (&ct.event, kind),
                (TriggerEvent::Insert, TrigEvent::Insert)
                    | (TriggerEvent::Delete, TrigEvent::Delete)
                    | (TriggerEvent::Update(_), TrigEvent::Update)
            );
            if ct.timing == timing && event_ok {
                out.push(ct);
            }
        }
    }

    /// Fire row triggers for one row change. `old`/`new` carry the affected row's
    /// values and rowid before/after the change. Non-recursive: triggers fire
    /// only at the top level (matching `recursive_triggers = OFF`).
    #[allow(clippy::too_many_arguments)]
    fn fire_triggers(
        &mut self,
        table: &str,
        kind: TrigEvent,
        timing: TriggerTiming,
        columns: &[ColumnInfo],
        old: Option<(&[Value], i64)>,
        new: Option<(&[Value], i64)>,
        params: &Params,
        changed_cols: Option<&[String]>,
    ) -> Result<bool> {
        // Non-recursive by default; with PRAGMA recursive_triggers a trigger may
        // fire others, bounded to avoid runaway recursion (SQLite caps at 1000).
        let depth = self.trigger_depth.get();
        let limit = if self.recursive_triggers { 1000 } else { 1 };
        if depth >= limit {
            return if self.recursive_triggers {
                Err(Error::Error("too many levels of trigger recursion".into()))
            } else {
                Ok(false)
            };
        }
        let mut trigs = self.triggers_for(table, kind, timing)?;
        // An `UPDATE OF col, …` trigger fires only when one of its named columns
        // appears in the UPDATE's SET list (SQLite semantics).
        if let Some(changed) = changed_cols {
            trigs.retain(|t| match &t.event {
                TriggerEvent::Update(cols) if !cols.is_empty() => cols
                    .iter()
                    .any(|c| changed.iter().any(|ch| ch.eq_ignore_ascii_case(c))),
                _ => true,
            });
        }
        if trigs.is_empty() {
            return Ok(false);
        }
        self.trigger_depth.set(depth + 1);
        let base = self.outer_scope.borrow().len();
        if let Some((vals, rid)) = old {
            self.push_row_frame("old", columns, vals, rid);
        }
        if let Some((vals, rid)) = new {
            self.push_row_frame("new", columns, vals, rid);
        }
        let result = self.run_trigger_bodies(&trigs, params);
        self.outer_scope.borrow_mut().truncate(base);
        self.trigger_depth.set(depth);
        result.map(|()| true)
    }

    fn push_row_frame(&self, label: &str, columns: &[ColumnInfo], values: &[Value], rowid: i64) {
        let columns = columns
            .iter()
            .map(|c| ColumnInfo {
                name: c.name.clone(),
                table: String::from(label),
                affinity: c.affinity,
                collation: c.collation,
            })
            .collect();
        self.outer_scope.borrow_mut().push(OuterFrame {
            columns,
            row: values.to_vec(),
            rowid: Some(rowid),
        });
    }

    /// Whether `name` is a view in main (or a temp view, which shadows main).
    fn is_view(&self, name: &str) -> bool {
        self.temp_has_view(name)
            || self.schema.objects().iter().any(|o| {
                o.obj_type == crate::schema::ObjectType::View && o.name.eq_ignore_ascii_case(name)
            })
    }

    /// Whether the temp database holds a view named `name`.
    fn temp_has_view(&self, name: &str) -> bool {
        self.temp_db.as_ref().is_some_and(|t| {
            t.schema.objects().iter().any(|o| {
                o.obj_type == crate::schema::ObjectType::View && o.name.eq_ignore_ascii_case(name)
            })
        })
    }

    /// The output columns of a view (labeled with the view name).
    fn view_columns(&self, name: &str, params: &Params) -> Result<Vec<ColumnInfo>> {
        match self.try_view(name, None, params)? {
            Some((cols, _)) => Ok(cols),
            None => Err(Error::Error(format!("no such view: {name}"))),
        }
    }

    /// `INSERT` into a view: fire its `INSTEAD OF INSERT` triggers (per row), or
    /// error if none exist.
    fn exec_view_insert(
        &mut self,
        ins: &Insert,
        rows: &[Vec<Expr>],
        params: &Params,
    ) -> Result<usize> {
        let cols = self.view_columns(&ins.table, params)?;
        if self
            .triggers_for(&ins.table, TrigEvent::Insert, TriggerTiming::InsteadOf)?
            .is_empty()
        {
            return Err(Error::Error(format!(
                "cannot modify {} — it is a view",
                ins.table
            )));
        }
        let target: Vec<usize> = if ins.columns.is_empty() {
            (0..cols.len()).collect()
        } else {
            ins.columns
                .iter()
                .map(|name| {
                    cols.iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| Error::Error(format!("no such column: {name}")))
                })
                .collect::<Result<_>>()?
        };
        let mut affected = 0;
        for row_exprs in rows {
            let ctx = EvalCtx::rowless(params).with_subqueries(self);
            let mut new = alloc::vec![Value::Null; cols.len()];
            for (i, e) in row_exprs.iter().enumerate() {
                new[target[i]] = eval::eval(e, &ctx)?;
            }
            self.fire_triggers(
                &ins.table,
                TrigEvent::Insert,
                TriggerTiming::InsteadOf,
                &cols,
                None,
                Some((&new, 0)),
                params,
                None,
            )?;
            if self.raise_ignore.replace(false) {
                continue;
            }
            affected += 1;
        }
        Ok(affected)
    }

    /// `DELETE` from a view: fire `INSTEAD OF DELETE` triggers for each row that
    /// the view yields and the `WHERE` selects.
    fn exec_view_delete(&mut self, del: &Delete, params: &Params) -> Result<usize> {
        let (cols, rows) = self
            .try_view(&del.table, None, params)?
            .ok_or_else(|| Error::Error(format!("no such view: {}", del.table)))?;
        if self
            .triggers_for(&del.table, TrigEvent::Delete, TriggerTiming::InsteadOf)?
            .is_empty()
        {
            return Err(Error::Error(format!(
                "cannot modify {} — it is a view",
                del.table
            )));
        }
        let mut affected = 0;
        for row in rows {
            if let Some(p) = &del.where_clause {
                let ctx = row_ctx(&row.values, &cols, None, params).with_subqueries(self);
                if eval::truth(&eval::eval(p, &ctx)?) != Some(true) {
                    continue;
                }
            }
            self.fire_triggers(
                &del.table,
                TrigEvent::Delete,
                TriggerTiming::InsteadOf,
                &cols,
                Some((&row.values, 0)),
                None,
                params,
                None,
            )?;
            if self.raise_ignore.replace(false) {
                continue;
            }
            affected += 1;
        }
        Ok(affected)
    }

    /// `UPDATE` a view: fire `INSTEAD OF UPDATE` triggers with OLD/NEW for each
    /// selected row.
    /// Apply `SET (cols) = (SELECT …)` row-value-subquery assignments for one
    /// target row: run each subquery once against `ctx` (the caller's original-row
    /// context, so it is a correlated, simultaneous read) and write its first
    /// row's columns into `target` at the positions named by the assignment's
    /// column list (no row → NULLs; a column-count mismatch errors). `meta`, when
    /// given, rejects assigning to a generated column.
    fn apply_row_subquery_assignments(
        &self,
        row_assignments: &[(Vec<String>, Box<Select>)],
        cols: &[ColumnInfo],
        meta: Option<&TableMeta>,
        ctx: &EvalCtx,
        target: &mut [Value],
    ) -> Result<()> {
        for (targets, select) in row_assignments {
            let mut positions = Vec::with_capacity(targets.len());
            for c in targets {
                let pos = cols
                    .iter()
                    .position(|mc| mc.name.eq_ignore_ascii_case(c))
                    .ok_or_else(|| Error::Error(format!("no such column: {c}")))?;
                if meta.is_some_and(|m| m.is_generated(pos)) {
                    return Err(Error::Error(format!(
                        "cannot UPDATE generated column \"{c}\""
                    )));
                }
                positions.push(pos);
            }
            let produced = eval::Subqueries::rows(self, select, ctx)?;
            let first = produced.into_iter().next();
            if let Some(r) = &first {
                if r.len() != positions.len() {
                    return Err(Error::Error(format!(
                        "{} columns assigned {} values",
                        positions.len(),
                        r.len()
                    )));
                }
            }
            for (i, &pos) in positions.iter().enumerate() {
                target[pos] = first.as_ref().map_or(Value::Null, |r| r[i].clone());
            }
        }
        Ok(())
    }

    fn exec_view_update(&mut self, upd: &Update, params: &Params) -> Result<usize> {
        let (cols, rows) = self
            .try_view(&upd.table, None, params)?
            .ok_or_else(|| Error::Error(format!("no such view: {}", upd.table)))?;
        if self
            .triggers_for(&upd.table, TrigEvent::Update, TriggerTiming::InsteadOf)?
            .is_empty()
        {
            return Err(Error::Error(format!(
                "cannot modify {} — it is a view",
                upd.table
            )));
        }
        let mut changed: Vec<String> = upd.assignments.iter().map(|(c, _)| c.clone()).collect();
        for (rcols, _) in &upd.row_assignments {
            changed.extend(rcols.iter().cloned());
        }
        let mut affected = 0;
        for row in rows {
            let old = row.values.clone();
            if let Some(p) = &upd.where_clause {
                let ctx = row_ctx(&old, &cols, None, params).with_subqueries(self);
                if eval::truth(&eval::eval(p, &ctx)?) != Some(true) {
                    continue;
                }
            }
            let mut new = old.clone();
            for (col, expr) in &upd.assignments {
                let pos = cols
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col))
                    .ok_or_else(|| Error::Error(format!("no such column: {col}")))?;
                // Simultaneous assignment: evaluate against the original row.
                let ctx = row_ctx(&old, &cols, None, params).with_subqueries(self);
                new[pos] = eval::eval(expr, &ctx)?;
            }
            if !upd.row_assignments.is_empty() {
                let ctx = row_ctx(&old, &cols, None, params).with_subqueries(self);
                self.apply_row_subquery_assignments(
                    &upd.row_assignments,
                    &cols,
                    None,
                    &ctx,
                    &mut new,
                )?;
            }
            self.fire_triggers(
                &upd.table,
                TrigEvent::Update,
                TriggerTiming::InsteadOf,
                &cols,
                Some((&old, 0)),
                Some((&new, 0)),
                params,
                Some(&changed),
            )?;
            if self.raise_ignore.replace(false) {
                continue;
            }
            affected += 1;
        }
        Ok(affected)
    }

    fn run_trigger_bodies(&mut self, trigs: &[CreateTrigger], params: &Params) -> Result<()> {
        for trig in trigs {
            if let Some(when) = &trig.when {
                let fires = {
                    let ctx = EvalCtx::rowless(params).with_subqueries(self);
                    eval::truth(&eval::eval(when, &ctx)?) == Some(true)
                };
                if !fires {
                    continue;
                }
            }
            for stmt in &trig.body {
                match stmt {
                    Statement::Insert(ins) => {
                        self.exec_insert(ins, params)?;
                    }
                    Statement::Update(u) => {
                        self.exec_update(u, params)?;
                    }
                    Statement::Delete(d) => {
                        self.exec_delete(d, params)?;
                    }
                    // A `SELECT` in a trigger body is side-effect free *except* for
                    // a `RAISE(…)`, which aborts or ignores the firing operation.
                    Statement::Select(sel) => {
                        self.run_trigger_select(sel, params)?;
                        // `RAISE(IGNORE)` abandons the row: stop running the rest of
                        // this (and later) trigger program(s).
                        if self.raise_ignore.get() {
                            return Ok(());
                        }
                    }
                    _ => return Err(Error::Unsupported("statement type in trigger body")),
                }
            }
        }
        Ok(())
    }

    /// Evaluate a trigger-body `SELECT` for a `RAISE(…)` call. A bare
    /// `SELECT RAISE(…)` (optionally wrapped in a single `CASE`) is the standard
    /// form; we evaluate each projected expression so any `RAISE` that the row
    /// reaches takes effect. `RAISE(ABORT|FAIL|ROLLBACK, msg)` raises a constraint
    /// error (arming the statement-atomicity flags); `RAISE(IGNORE)` sets
    /// `raise_ignore` so the firing row operation is silently skipped.
    fn run_trigger_select(&self, sel: &Select, params: &Params) -> Result<()> {
        for col in &sel.columns {
            if let ResultColumn::Expr { expr, .. } = col {
                self.eval_raise_expr(expr, params)?;
                if self.raise_ignore.get() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Evaluate `expr` looking for a `RAISE(…)` that the row reaches: a direct
    /// `RAISE(…)` call, or one selected by a `CASE` branch. Other expressions are
    /// side-effect free here and are skipped.
    fn eval_raise_expr(&self, expr: &Expr, params: &Params) -> Result<()> {
        match expr {
            Expr::Function { name, args, .. } if name.eq_ignore_ascii_case("raise") => {
                self.fire_raise(args, params)
            }
            Expr::Paren(inner) => self.eval_raise_expr(inner, params),
            Expr::Case {
                operand,
                when_then,
                else_result,
            } => {
                let ctx = EvalCtx::rowless(params).with_subqueries(self);
                let base = match operand {
                    Some(op) => Some(eval::eval(op, &ctx)?),
                    None => None,
                };
                for (when, then) in when_then {
                    let hit = match &base {
                        // `CASE x WHEN v …`: the branch fires when x == v.
                        Some(b) => {
                            let w = eval::eval(when, &ctx)?;
                            crate::value::cmp_values(b, &w) == core::cmp::Ordering::Equal
                        }
                        // `CASE WHEN cond …`: the branch fires when cond is true.
                        None => eval::truth(&eval::eval(when, &ctx)?) == Some(true),
                    };
                    if hit {
                        return self.eval_raise_expr(then, params);
                    }
                }
                if let Some(e) = else_result {
                    return self.eval_raise_expr(e, params);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Apply a parsed `RAISE(action[, msg])`. `action` is the lower-cased keyword
    /// stored as the first argument; `msg` (when present) is the second.
    fn fire_raise(&self, args: &[Expr], params: &Params) -> Result<()> {
        let action = match args.first() {
            Some(Expr::Literal(Literal::Str(s))) => s.as_str(),
            _ => return Err(Error::Error("malformed RAISE()".into())),
        };
        if action == "ignore" {
            self.raise_ignore.set(true);
            return Ok(());
        }
        let ctx = EvalCtx::rowless(params).with_subqueries(self);
        let msg = match args.get(1) {
            Some(e) => match eval::eval(e, &ctx)? {
                Value::Null => String::new(),
                Value::Text(s) => s,
                Value::Integer(i) => {
                    let mut s = String::new();
                    let _ = core::fmt::write(&mut s, format_args!("{i}"));
                    s
                }
                Value::Real(r) => eval::format_real(r),
                Value::Blob(_) => String::new(),
            },
            None => String::new(),
        };
        match action {
            "fail" => self.stmt_keep_partial.set(true),
            "rollback" => self.stmt_rollback_tx.set(true),
            _ => {} // "abort" — the default statement rollback
        }
        Err(Error::Constraint(msg))
    }

    /// The AUTOINCREMENT high-water mark stored for `table` in `sqlite_sequence`,
    /// or `None` if that catalog or row is absent.
    fn sequence_value(&self, table: &str) -> Result<Option<i64>> {
        if self.schema.table("sqlite_sequence").is_none() {
            return Ok(None);
        }
        let meta = self.table_meta("sqlite_sequence", None)?;
        for (_, vals) in self.scan_table(&meta)? {
            if matches!(&vals[0], Value::Text(t) if t == table) {
                return Ok(Some(eval::to_i64(&vals[1])));
            }
        }
        Ok(None)
    }

    /// Persist the AUTOINCREMENT high-water mark `seq` for `table` into
    /// `sqlite_sequence` — updating the existing row in place (same rowid) or
    /// inserting a new one — like SQLite. A no-op if the catalog is absent.
    fn set_sequence(&mut self, table: &str, seq: i64) -> Result<()> {
        let Some(seq_obj) = self.schema.table("sqlite_sequence") else {
            return Ok(());
        };
        let root = seq_obj.rootpage;
        let meta = self.table_meta("sqlite_sequence", None)?;
        let existing: Option<i64> = self
            .scan_table(&meta)?
            .into_iter()
            .find(|(_, v)| matches!(&v[0], Value::Text(t) if t == table))
            .map(|(rid, _)| rid);
        let rec = encode_record(&[Value::Text(table.into()), Value::Integer(seq)]);
        let rid = match existing {
            Some(rid) => {
                delete_table(self.backend.writer()?, root, rid)?;
                rid
            }
            None => self.next_rowid(root)?,
        };
        insert_table(self.backend.writer()?, root, rid, &rec)?;
        Ok(())
    }

    fn exec_insert(&mut self, ins: &Insert, params: &Params) -> Result<usize> {
        reject_schema_write(&ins.table)?;
        // A virtual table routes INSERT to its module's `update` (xUpdate); only
        // the `VALUES`/`SELECT` source needs materializing first.
        if self.is_virtual_table(&ins.table) {
            let rows: Vec<Vec<Expr>> = match &ins.source {
                InsertSource::Values(rows) => rows.clone(),
                InsertSource::DefaultValues => alloc::vec![Vec::new()],
                InsertSource::Select(sel) => self
                    .run_select(sel, params)?
                    .rows
                    .into_iter()
                    .map(|row| row.into_iter().map(value_to_literal_expr).collect())
                    .collect(),
            };
            return self.exec_vtab_insert(ins, &rows, params);
        }
        // `INSERT … SELECT` is evaluated to a snapshot of value rows first (so
        // `INSERT INTO t SELECT … FROM t` reads the pre-insert state), then each
        // row flows through the normal VALUES path as literal expressions.
        // A multi-row `INSERT … VALUES (…),(…)` must have rows of equal arity.
        // SQLite rejects a mismatch up front ("all VALUES must have the same
        // number of terms"); validate before any row is written so a short row
        // never half-completes the insert.
        if let InsertSource::Values(rows) = &ins.source {
            if let Some(first) = rows.first() {
                if rows.iter().any(|r| r.len() != first.len()) {
                    return Err(Error::Error(
                        "all VALUES must have the same number of terms".into(),
                    ));
                }
            }
        }
        let (rows, is_default_values) = match &ins.source {
            InsertSource::Values(rows) => (rows.clone(), false),
            InsertSource::DefaultValues => (alloc::vec![Vec::new()], true),
            InsertSource::Select(sel) => {
                let result = self.run_select(sel, params)?;
                let rows = result
                    .rows
                    .into_iter()
                    .map(|row| row.into_iter().map(value_to_literal_expr).collect())
                    .collect();
                (rows, false)
            }
        };
        if self.is_view(&ins.table) {
            return self.exec_view_insert(ins, &rows, params);
        }
        let meta = self.table_meta(&ins.table, None)?;
        if meta.without_rowid {
            if !ins.upsert.is_empty() || !ins.returning.is_empty() {
                return Err(Error::Unsupported(
                    "UPSERT / RETURNING on WITHOUT ROWID tables",
                ));
            }
            return self.exec_insert_without_rowid(ins, &meta, &rows, params);
        }
        let n_cols = meta.columns.len();

        // Map the provided column list (or all columns) to table positions.
        let target: Vec<usize> = if ins.columns.is_empty() {
            (0..n_cols).collect()
        } else {
            let mut t = Vec::new();
            for name in &ins.columns {
                let pos = meta
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| Error::Error(format!("no such column: {name}")))?;
                t.push(pos);
            }
            t
        };

        let indexes = self.indexes_of(&ins.table)?;
        let mut next_auto = self.next_rowid(meta.root)?;
        // AUTOINCREMENT never reuses a rowid at or below the persisted high-water
        // mark, so seed the counter past it (a deleted maximum is not recycled).
        if meta.autoincrement {
            if let Some(seq) = self.sequence_value(&ins.table)? {
                next_auto = next_auto.max(seq + 1);
            }
        }
        let mut affected = 0;
        let mut replaced = false;
        for row_exprs in &rows {
            // Every supplied row must match the target column count (DEFAULT
            // VALUES is the one exception — it supplies an empty row meaning
            // "all defaults").
            if !is_default_values && row_exprs.len() != target.len() {
                return Err(Error::Error("INSERT column/value count mismatch".into()));
            }
            // Start every column at its DEFAULT (or NULL), then apply provided.
            // Subqueries are attached so INSERT … VALUES can use scalar subqueries
            // and trigger bodies can read NEW/OLD via the outer scope.
            let ctx = EvalCtx::rowless(params).with_subqueries(self);
            let mut values: Vec<Value> = meta
                .defaults
                .iter()
                .map(|d| match d {
                    Some(e) => eval::eval(e, &ctx),
                    None => Ok(Value::Null),
                })
                .collect::<Result<_>>()?;
            for (i, e) in row_exprs.iter().enumerate() {
                if meta.is_generated(target[i]) {
                    return Err(Error::Error(format!(
                        "cannot INSERT into generated column \"{}\"",
                        meta.columns[target[i]].name
                    )));
                }
                values[target[i]] = eval::eval(e, &ctx)?;
            }
            apply_column_affinity(&meta, &mut values);
            self.materialize_generated(&meta, &mut values, params)?;

            // Determine the rowid (explicit INTEGER PRIMARY KEY value or auto).
            let rowid = match meta.ipk {
                Some(ipk) if !matches!(values[ipk], Value::Null) => {
                    let r = eval::to_i64(&values[ipk]);
                    next_auto = next_auto.max(r + 1);
                    r
                }
                _ => {
                    let r = next_auto;
                    next_auto += 1;
                    r
                }
            };
            // Capture column values (with the IPK = rowid) for index keys, then
            // NULL the IPK column in the stored record (it aliases the rowid).
            if let Some(ipk) = meta.ipk {
                values[ipk] = Value::Integer(rowid);
            }
            // NOT NULL / STRICT-type / CHECK constraints. `INSERT OR IGNORE`
            // skips a row that violates any of these (rather than failing the
            // statement); every other conflict policy lets the error propagate.
            {
                // NOT NULL honors the column's (or statement's) ON CONFLICT action;
                // a skipped row (IGNORE) drops out here, a REPLACE substitutes the
                // column default into `values`.
                if !self.resolve_not_null(
                    &meta,
                    &mut values,
                    ins.on_conflict,
                    ins.on_conflict_explicit,
                    params,
                )? {
                    continue;
                }
                let r = self
                    .check_strict_types(&meta, &values)
                    .and_then(|()| self.check_constraints(&meta, &values, Some(rowid), params));
                match r {
                    Ok(()) => {}
                    Err(Error::Constraint(_)) if ins.on_conflict == OnConflict::Ignore => continue,
                    Err(Error::Constraint(m)) => {
                        return Err(self.conflict_error(ins.on_conflict, &m))
                    }
                    Err(e) => return Err(e),
                }
            }
            self.check_fk_child(&ins.table, &meta, &values)?;

            // Resolve UNIQUE / PRIMARY KEY (incl. rowid) conflicts.
            let (conflicts, constraint_oc) =
                self.find_conflicts(&ins.table, &meta, rowid, &values, None, params)?;
            // A statement-level `OR <action>` overrides the constraint's declared
            // `ON CONFLICT <action>`; a plain `INSERT` uses the constraint's action.
            let effective_oc = if ins.on_conflict_explicit {
                ins.on_conflict
            } else {
                constraint_oc
            };
            if !conflicts.is_empty() {
                // An `ON CONFLICT … DO …` upsert clause intercepts the conflict,
                // but only when the conflict is on the index it targets (a bare
                // `ON CONFLICT` with no target matches any unique conflict). A
                // conflict on a *different* index is a hard error, exactly as in
                // SQLite.
                let mut matched = None;
                for up in &ins.upsert {
                    if self.upsert_target_matches(&meta, up, &conflicts, &values, rowid, params)? {
                        matched = Some(up);
                        break;
                    }
                }
                if let Some(up) = matched {
                    match &up.action {
                        UpsertAction::Nothing => continue, // skip the conflicting row
                        UpsertAction::Update {
                            assignments,
                            where_clause,
                        } => {
                            if self.upsert_do_update(
                                &ins.table,
                                &meta,
                                conflicts[0],
                                &values,
                                assignments,
                                where_clause.as_ref(),
                                &ins.returning,
                                params,
                            )? {
                                affected += 1;
                                replaced = true; // index entries changed; rebuild
                            }
                            continue;
                        }
                    }
                }
                match effective_oc {
                    oc @ (OnConflict::Abort | OnConflict::Fail | OnConflict::Rollback) => {
                        let m = self.unique_violation_message(
                            &ins.table, &meta, rowid, &values, None, params,
                        );
                        return Err(self.conflict_error(oc, &m));
                    }
                    OnConflict::Ignore => continue, // skip this row
                    OnConflict::Replace => {
                        // Deleting the conflicting rows to make room fires their FK
                        // `ON DELETE` actions (CASCADE / SET NULL / …) via
                        // `delete_row_cascade`, exactly like sqlite — but NOT DELETE
                        // triggers (sqlite gates those on `recursive_triggers`, off
                        // by default, and `delete_row_cascade` fires none).
                        for cr in conflicts {
                            self.delete_row_cascade(&ins.table, &meta, cr, params)?;
                        }
                        replaced = true;
                    }
                }
            }

            let index_values = values.clone();
            self.fire_triggers(
                &ins.table,
                TrigEvent::Insert,
                TriggerTiming::Before,
                &meta.columns,
                None,
                Some((&index_values, rowid)),
                params,
                None,
            )?;
            // A `BEFORE INSERT` trigger's `RAISE(IGNORE)` abandons just this row.
            if self.raise_ignore.replace(false) {
                continue;
            }
            let record = self.encode_table_record(&meta, &index_values);
            insert_table(self.backend.writer()?, meta.root, rowid, &record)?;
            // `last_insert_rowid()` tracks the most recent insert (a later insert
            // from an AFTER trigger overwrites this, matching SQLite).
            self.last_insert_rowid.set(rowid);
            for idx in &indexes {
                if !self.row_in_index(idx, &meta, &index_values, Some(rowid), params)? {
                    continue; // partial index excludes this row
                }
                let key = self.index_key_bytes(idx, &meta, &index_values, rowid, params)?;
                insert_index(self.backend.writer()?, idx.root, &key, &idx.collations)?;
            }
            self.fire_triggers(
                &ins.table,
                TrigEvent::Insert,
                TriggerTiming::After,
                &meta.columns,
                None,
                Some((&index_values, rowid)),
                params,
                None,
            )?;
            if !ins.returning.is_empty() {
                self.collect_returning(&ins.returning, &meta, &index_values, Some(rowid), params)?;
            }
            affected += 1;
        }
        // Persist the AUTOINCREMENT high-water mark: `next_auto - 1` is the largest
        // rowid assigned or seen this statement. Only advance `sqlite_sequence`
        // (never lower it), matching SQLite.
        if meta.autoincrement && affected > 0 {
            let high = next_auto - 1;
            if high > self.sequence_value(&ins.table)?.unwrap_or(i64::MIN) {
                self.set_sequence(&ins.table, high)?;
            }
        }
        // REPLACE removed rows whose index entries were maintained incrementally;
        // rebuild from the final table state to be safe.
        if replaced {
            self.rebuild_indexes(&meta, &indexes)?;
        }
        Ok(affected)
    }

    /// Apply an `ON CONFLICT … DO UPDATE` action to the existing conflicting
    /// row `existing_rowid`. `proposed` is the row the `INSERT` would have added,
    /// exposed to the `SET`/`WHERE` expressions as the `excluded` pseudo-table.
    /// Returns whether a row was actually updated (the optional `WHERE` can veto).
    #[allow(clippy::too_many_arguments)]
    fn upsert_do_update(
        &mut self,
        table: &str,
        meta: &TableMeta,
        existing_rowid: i64,
        proposed: &[Value],
        assignments: &[(String, Expr)],
        where_clause: Option<&Expr>,
        returning: &[ResultColumn],
        params: &Params,
    ) -> Result<bool> {
        let Some(old_row) = self.read_row(meta, existing_rowid)? else {
            return Ok(false);
        };
        let changed: Vec<String> = assignments.iter().map(|(c, _)| c.clone()).collect();
        // Column scope for the SET/WHERE expressions: the target table's columns,
        // then the same columns again under the `excluded` table label.
        let mut cols: Vec<ColumnInfo> = meta.columns.clone();
        cols.extend(meta.columns.iter().map(|c| ColumnInfo {
            name: c.name.clone(),
            table: String::from("excluded"),
            affinity: c.affinity,
            collation: c.collation,
        }));
        // Evaluate the DO UPDATE WHERE and SET right-hand sides against the
        // combined (existing row + excluded) scope, then drop the borrow.
        let mut values = old_row.clone();
        {
            let mut combined = old_row.clone();
            combined.extend_from_slice(proposed);
            let ctx = EvalCtx {
                row: &combined,
                columns: &cols,
                rowid: Some(existing_rowid),
                params,
                anon_counter: core::cell::Cell::new(0),
                subqueries: None,
            }
            .with_subqueries(self);
            if let Some(w) = where_clause {
                if eval::truth(&eval::eval(w, &ctx)?) != Some(true) {
                    return Ok(false);
                }
            }
            for (col, e) in assignments {
                let pos = meta
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col))
                    .ok_or_else(|| Error::Error(format!("no such column: {col}")))?;
                if meta.is_generated(pos) {
                    return Err(Error::Error(format!(
                        "cannot UPDATE generated column \"{col}\""
                    )));
                }
                values[pos] = eval::eval(e, &ctx)?;
            }
        }
        apply_column_affinity(meta, &mut values);
        self.materialize_generated(meta, &mut values, params)?;
        check_not_null(meta, &values)?;
        self.check_strict_types(meta, &values)?;
        self.check_constraints(meta, &values, Some(existing_rowid), params)?;
        self.check_fk_child(table, meta, &values)?;
        if self.foreign_keys {
            self.enforce_parent_change(table, &old_row, Some(&values), params)?;
        }
        let new_rowid = match meta.ipk {
            Some(ipk) => eval::to_i64(&values[ipk]),
            None => existing_rowid,
        };
        self.fire_triggers(
            table,
            TrigEvent::Update,
            TriggerTiming::Before,
            &meta.columns,
            Some((&old_row, existing_rowid)),
            Some((&values, new_rowid)),
            params,
            Some(&changed),
        )?;
        if !self
            .find_conflicts(
                table,
                meta,
                new_rowid,
                &values,
                Some(existing_rowid),
                params,
            )?
            .0
            .is_empty()
        {
            return Err(Error::Constraint(self.unique_violation_message(
                table,
                meta,
                new_rowid,
                &values,
                Some(existing_rowid),
                params,
            )));
        }
        let new_full = values.clone();
        let record = self.encode_table_record(meta, &new_full);
        delete_table(self.backend.writer()?, meta.root, existing_rowid)?;
        insert_table(self.backend.writer()?, meta.root, new_rowid, &record)?;
        self.fire_triggers(
            table,
            TrigEvent::Update,
            TriggerTiming::After,
            &meta.columns,
            Some((&old_row, existing_rowid)),
            Some((&new_full, new_rowid)),
            params,
            Some(&changed),
        )?;
        if !returning.is_empty() {
            self.collect_returning(returning, meta, &new_full, Some(new_rowid), params)?;
        }
        Ok(true)
    }

    /// Project a `RETURNING` row from `values` (a full table row) and stash it in
    /// [`returning_rows`](Self::returning_rows) for `execute_returning` to drain.
    fn collect_returning(
        &self,
        returning: &[ResultColumn],
        meta: &TableMeta,
        values: &[Value],
        rowid: Option<i64>,
        params: &Params,
    ) -> Result<()> {
        let ctx = row_ctx(values, &meta.columns, rowid, params).with_subqueries(self);
        let mut out = Vec::new();
        for col in returning {
            project_column(col, &meta.columns, &ctx, &mut out)?;
        }
        self.returning_rows.borrow_mut().push(out);
        Ok(())
    }

    /// Load `ANALYZE` statistics, mapping each index name to its parsed `stat`
    /// integers (`[nRow, avgEq1, avgEq2, …]`). Empty when the database has not
    /// been analyzed. Used by the cost-based index chooser.
    fn stat1_map(&self) -> alloc::collections::BTreeMap<String, Vec<u64>> {
        let mut map = alloc::collections::BTreeMap::new();
        if self.schema.table("sqlite_stat1").is_none() {
            return map;
        }
        let Ok(meta) = self.table_meta("sqlite_stat1", None) else {
            return map;
        };
        let Ok(rows) = self.scan_table(&meta) else {
            return map;
        };
        for (_, vals) in rows {
            if let (Some(Value::Text(idx)), Some(Value::Text(stat))) = (vals.get(1), vals.get(2)) {
                let nums: Vec<u64> = stat
                    .split_whitespace()
                    .filter_map(|t| t.parse().ok())
                    .collect();
                if !nums.is_empty() {
                    map.insert(idx.clone(), nums);
                }
            }
        }
        map
    }

    /// Rowids of existing rows that conflict with a candidate row on the rowid
    /// or any UNIQUE/PRIMARY KEY column set (NULLs are considered distinct).
    fn find_conflicts(
        &self,
        table: &str,
        meta: &TableMeta,
        rowid: i64,
        values: &[Value],
        exclude: Option<i64>,
        params: &Params,
    ) -> Result<(Vec<i64>, OnConflict)> {
        // Unique standalone indexes (named `CREATE UNIQUE INDEX`, incl. partial
        // and expression indexes) are not represented in `meta.unique` — those
        // sets come only from inline CREATE TABLE constraints (whose automatic
        // indexes we therefore skip here). Precompute each such index's key
        // values for the new row; a NULL key term or an excluding partial
        // predicate means the new row can't collide on that index.
        let uniq_idx: Vec<(IndexMeta, Vec<Value>)> = self
            .indexes_of(table)?
            .into_iter()
            .filter(|i| i.unique && autoindex_number(&i.name, table).is_none())
            .filter_map(|i| {
                if !self
                    .row_in_index(&i, meta, values, Some(rowid), params)
                    .unwrap_or(false)
                {
                    return None;
                }
                let key = self
                    .index_key_values(&i, meta, values, rowid, params)
                    .ok()?;
                if key.iter().any(|v| matches!(v, Value::Null)) {
                    return None; // a NULL makes the key distinct
                }
                Some((i, key))
            })
            .collect();

        let mut out = Vec::new();
        // The declared `ON CONFLICT` action of the first inline UNIQUE/PRIMARY KEY
        // set the new row collides on (used when the statement has no `OR <action>`).
        let mut action: Option<OnConflict> = None;
        for (er, ev) in self.scan_table(meta)? {
            if Some(er) == exclude {
                continue;
            }
            if er == rowid {
                out.push(er);
                continue;
            }
            let mut conflicted = false;
            for (set, set_oc) in &meta.unique {
                let new_tuple: Vec<&Value> = set.iter().map(|&i| &values[i]).collect();
                if new_tuple.iter().any(|v| matches!(v, Value::Null)) {
                    continue; // a NULL makes the key distinct
                }
                let conflict = set.iter().zip(&new_tuple).all(|(&i, nv)| {
                    crate::value::cmp_values_coll(&ev[i], nv, meta.columns[i].collation)
                        == core::cmp::Ordering::Equal
                });
                if conflict {
                    out.push(er);
                    action.get_or_insert(*set_oc);
                    conflicted = true;
                    break;
                }
            }
            if conflicted {
                continue;
            }
            // Then the unique standalone/partial/expression indexes.
            for (idx, new_key) in &uniq_idx {
                if !self.row_in_index(idx, meta, &ev, Some(er), params)? {
                    continue; // existing row not in this partial index
                }
                let ex_key = self.index_key_values(idx, meta, &ev, er, params)?;
                let conflict = ex_key.len() == new_key.len()
                    && ex_key
                        .iter()
                        .zip(new_key)
                        .zip(&idx.collations)
                        .all(|((a, b), &coll)| {
                            crate::value::cmp_values_coll(a, b, coll) == core::cmp::Ordering::Equal
                        });
                if conflict {
                    out.push(er);
                    break;
                }
            }
        }
        Ok((out, action.unwrap_or(OnConflict::Abort)))
    }

    /// SQLite's UNIQUE-violation message for the *first* unique constraint the new
    /// row collides on: `UNIQUE constraint failed: t.a[, t.b]` (or `: index 'name'`
    /// for an expression index). Checks the rowid/INTEGER PRIMARY KEY, then inline
    /// `UNIQUE`/`PRIMARY KEY` sets, then standalone unique indexes — falling back to
    /// the bare message if none can be pinpointed. Runs only on the (cold) error
    /// path, so the extra table scans are immaterial.
    fn unique_violation_message(
        &self,
        table: &str,
        meta: &TableMeta,
        rowid: i64,
        values: &[Value],
        exclude: Option<i64>,
        params: &Params,
    ) -> String {
        let bare = String::from("UNIQUE constraint failed");
        let qualify = |cols: &[usize]| {
            cols.iter()
                .map(|&i| alloc::format!("{}.{}", meta.columns[i].table, meta.columns[i].name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let rows = match self.scan_table(meta) {
            Ok(r) => r,
            Err(_) => return bare,
        };
        // A rowid / INTEGER PRIMARY KEY collision.
        if let Some(ipk) = meta.ipk {
            if rows
                .iter()
                .any(|(er, _)| *er == rowid && Some(*er) != exclude)
            {
                return alloc::format!("UNIQUE constraint failed: {}", qualify(&[ipk]));
            }
        }
        // Inline UNIQUE / PRIMARY KEY constraint sets, in declaration order.
        for (set, _) in &meta.unique {
            if set.iter().any(|&i| matches!(values[i], Value::Null)) {
                continue;
            }
            let hit = rows.iter().any(|(er, ev)| {
                Some(*er) != exclude
                    && set.iter().all(|&i| {
                        crate::value::cmp_values_coll(&ev[i], &values[i], meta.columns[i].collation)
                            == core::cmp::Ordering::Equal
                    })
            });
            if hit {
                return alloc::format!("UNIQUE constraint failed: {}", qualify(set));
            }
        }
        // Standalone unique indexes (a `CREATE UNIQUE INDEX`; the inline sets'
        // automatic indexes are already covered above and skipped here).
        if let Ok(idxs) = self.indexes_of(table) {
            for idx in idxs
                .iter()
                .filter(|i| i.unique && autoindex_number(&i.name, table).is_none())
            {
                if !self
                    .row_in_index(idx, meta, values, Some(rowid), params)
                    .unwrap_or(false)
                {
                    continue;
                }
                let Ok(new_key) = self.index_key_values(idx, meta, values, rowid, params) else {
                    continue;
                };
                if new_key.iter().any(|v| matches!(v, Value::Null)) {
                    continue;
                }
                let hit = rows.iter().any(|(er, ev)| {
                    Some(*er) != exclude
                        && self
                            .row_in_index(idx, meta, ev, Some(*er), params)
                            .unwrap_or(false)
                        && self
                            .index_key_values(idx, meta, ev, *er, params)
                            .map(|ek| {
                                ek.iter().zip(&new_key).enumerate().all(|(k, (a, b))| {
                                    crate::value::cmp_values_coll(a, b, idx.collations[k])
                                        == core::cmp::Ordering::Equal
                                })
                            })
                            .unwrap_or(false)
                });
                if hit {
                    let detail = if idx.key_exprs.is_some() {
                        alloc::format!("index '{}'", idx.name)
                    } else {
                        qualify(&idx.cols)
                    };
                    return alloc::format!("UNIQUE constraint failed: {detail}");
                }
            }
        }
        bare
    }

    /// Does an `ON CONFLICT (target…) DO …` upsert clause apply to the conflict
    /// that just occurred? A bare `ON CONFLICT` (no target) absorbs any unique
    /// conflict. A targeted clause applies only when the proposed row actually
    /// collides with a conflicting row on the **target** columns — a conflict on
    /// a different unique index is a hard error, exactly as SQLite behaves.
    #[allow(clippy::too_many_arguments)]
    fn upsert_target_matches(
        &self,
        meta: &TableMeta,
        up: &Upsert,
        conflicts: &[i64],
        values: &[Value],
        rowid: i64,
        params: &Params,
    ) -> Result<bool> {
        if up.target.is_empty() {
            return Ok(true);
        }
        // Resolve the target column names to column indices.
        let target_cols: Vec<usize> = up
            .target
            .iter()
            .map(|name| {
                meta.columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| Error::Error(format!("no such column: {name}")))
            })
            .collect::<Result<_>>()?;
        // The target names the rowid / INTEGER PRIMARY KEY: it matches when a
        // conflicting row shares the candidate rowid.
        if let Some(ipk) = meta.ipk {
            if target_cols == [ipk] {
                return Ok(conflicts.contains(&rowid));
            }
        }
        // The conflict matches the target only if the proposed row equals some
        // conflicting row on every target column (NULLs never match — a NULL key
        // is distinct, so it could not have produced this conflict). The target
        // must also actually be a UNIQUE/PK constraint, but if the rows collide on
        // those columns there necessarily is one.
        for &er in conflicts {
            let Some(existing) = self.read_row(meta, er)? else {
                continue;
            };
            let collide = target_cols.iter().all(|&c| {
                !matches!(values[c], Value::Null)
                    && crate::value::cmp_values_coll(
                        &existing[c],
                        &values[c],
                        meta.columns[c].collation,
                    ) == core::cmp::Ordering::Equal
            });
            if collide {
                return Ok(true);
            }
        }
        let _ = params;
        Ok(false)
    }

    /// The key values for a row under `idx` (excluding the trailing rowid): the
    /// indexed column values, or the evaluated key expressions for an expression
    /// index. Used for uniqueness comparison (collation applied by the caller).
    fn index_key_values(
        &self,
        idx: &IndexMeta,
        meta: &TableMeta,
        values: &[Value],
        rowid: i64,
        params: &Params,
    ) -> Result<Vec<Value>> {
        match &idx.key_exprs {
            None => Ok(idx.cols.iter().map(|&c| values[c].clone()).collect()),
            Some(exprs) => {
                let ctx = row_ctx(values, &meta.columns, Some(rowid), params).with_subqueries(self);
                exprs.iter().map(|e| eval::eval(e, &ctx)).collect()
            }
        }
    }

    /// Whether rows `a` and `b` collide on any unique *standalone* index of
    /// `table` (plain or partial — expression indexes are rejected on WITHOUT
    /// ROWID tables). Complements [`unique_match`], which covers only the inline
    /// PRIMARY KEY / UNIQUE constraints; used by the WITHOUT ROWID write paths.
    fn wr_index_collision(
        &self,
        table: &str,
        meta: &TableMeta,
        a: &[Value],
        b: &[Value],
        params: &Params,
    ) -> Result<bool> {
        for idx in self
            .indexes_of(table)?
            .iter()
            .filter(|i| i.unique && autoindex_number(&i.name, table).is_none())
        {
            if !self.row_in_index(idx, meta, a, None, params)?
                || !self.row_in_index(idx, meta, b, None, params)?
            {
                continue;
            }
            let ka = self.index_key_values(idx, meta, a, 0, params)?;
            if ka.iter().any(|v| matches!(v, Value::Null)) {
                continue; // a NULL makes the key distinct
            }
            let kb = self.index_key_values(idx, meta, b, 0, params)?;
            let eq = ka.len() == kb.len()
                && ka.iter().zip(&kb).zip(&idx.collations).all(|((x, y), &c)| {
                    crate::value::cmp_values_coll(x, y, c) == core::cmp::Ordering::Equal
                });
            if eq {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn exec_delete(&mut self, del: &Delete, params: &Params) -> Result<usize> {
        // A leading `WITH` makes its CTEs visible to the WHERE subqueries; push
        // them for the duration of the statement, then restore the scope.
        if del.ctes.is_empty() {
            return self.exec_delete_inner(del, params);
        }
        let base = self.cte_env.borrow().len();
        let pushed = self.push_ctes(&del.ctes, params, None);
        let result = pushed.and_then(|()| self.exec_delete_inner(del, params));
        self.cte_env.borrow_mut().truncate(base);
        result
    }

    fn exec_delete_inner(&mut self, del: &Delete, params: &Params) -> Result<usize> {
        reject_schema_write(&del.table)?;
        if self.is_virtual_table(&del.table) {
            return self.exec_vtab_delete(del, params);
        }
        if self.is_view(&del.table) {
            return self.exec_view_delete(del, params);
        }
        let meta = self.table_meta(&del.table, None)?;
        if meta.without_rowid {
            if !del.returning.is_empty() {
                return Err(Error::Unsupported("RETURNING on WITHOUT ROWID tables"));
            }
            return self.exec_delete_without_rowid(del, &meta, params);
        }
        let indexes = self.indexes_of(&del.table)?;
        let mut victims = self.matching_rowids(&meta, del.where_clause.as_ref(), params)?;
        if !del.order_by.is_empty() || del.limit.is_some() || del.offset.is_some() {
            victims = self.order_limit_rowids(
                &meta,
                victims,
                &del.order_by,
                del.limit.as_ref(),
                del.offset.as_ref(),
                params,
            )?;
        }
        let mut deleted = 0;
        for rowid in &victims {
            let old = self.read_row(&meta, *rowid)?;
            if let Some(old) = &old {
                self.fire_triggers(
                    &del.table,
                    TrigEvent::Delete,
                    TriggerTiming::Before,
                    &meta.columns,
                    Some((old, *rowid)),
                    None,
                    params,
                    None,
                )?;
                // A `BEFORE DELETE` trigger's `RAISE(IGNORE)` spares this row.
                if self.raise_ignore.replace(false) {
                    continue;
                }
                if !del.returning.is_empty() {
                    self.collect_returning(&del.returning, &meta, old, Some(*rowid), params)?;
                }
            }
            // Enforce referential actions on dependent child tables.
            if self.foreign_keys {
                if let Some(old) = &old {
                    self.enforce_parent_change(&del.table, old, None, params)?;
                }
            }
            delete_table(self.backend.writer()?, meta.root, *rowid)?;
            deleted += 1;
            if let Some(old) = &old {
                self.fire_triggers(
                    &del.table,
                    TrigEvent::Delete,
                    TriggerTiming::After,
                    &meta.columns,
                    Some((old, *rowid)),
                    None,
                    params,
                    None,
                )?;
            }
        }
        if deleted > 0 {
            self.compact_table(&meta)?;
            self.rebuild_indexes(&meta, &indexes)?;
        }
        Ok(deleted)
    }

    /// Reclaim empty/underfull table b-tree pages left by deletes: if the table
    /// has any empty leaf page, rebuild the b-tree compactly in place (root page
    /// number preserved), freeing the slack to the freelist. This is graphitesql's
    /// page-merging-on-delete — using the well-tested insert path rather than
    /// in-place sibling rebalancing — and keeps the tree balanced and compact.
    fn compact_table(&mut self, meta: &TableMeta) -> Result<()> {
        if !table_has_empty_leaf(self.backend.source(), meta.root)? {
            return Ok(());
        }
        // Collect every surviving (rowid, raw payload) in key order.
        let mut rows: Vec<(i64, Vec<u8>)> = Vec::new();
        {
            let mut cur = TableCursor::new(self.backend.source(), meta.root);
            let mut ok = cur.first()?;
            while ok {
                rows.push((cur.rowid()?, cur.payload()?));
                ok = cur.next()?;
            }
        }
        let w = self.backend.writer()?;
        clear_table(w, meta.root)?;
        for (rowid, payload) in &rows {
            insert_table(w, meta.root, *rowid, payload)?;
        }
        Ok(())
    }

    fn exec_update(&mut self, upd: &Update, params: &Params) -> Result<usize> {
        // A leading `WITH` exposes its CTEs to the SET/WHERE/FROM subqueries.
        if upd.ctes.is_empty() {
            return self.exec_update_inner(upd, params);
        }
        let base = self.cte_env.borrow().len();
        let pushed = self.push_ctes(&upd.ctes, params, None);
        let result = pushed.and_then(|()| self.exec_update_inner(upd, params));
        self.cte_env.borrow_mut().truncate(base);
        result
    }

    fn exec_update_inner(&mut self, upd: &Update, params: &Params) -> Result<usize> {
        reject_schema_write(&upd.table)?;
        if self.is_virtual_table(&upd.table) {
            return self.exec_vtab_update(upd, params);
        }
        if self.is_view(&upd.table) {
            return self.exec_view_update(upd, params);
        }
        let meta = self.table_meta(&upd.table, None)?;
        if meta.without_rowid {
            if !upd.returning.is_empty() {
                return Err(Error::Unsupported("RETURNING on WITHOUT ROWID tables"));
            }
            if upd.from.is_some() {
                return Err(Error::Unsupported("UPDATE … FROM on WITHOUT ROWID tables"));
            }
            return self.exec_update_without_rowid(upd, &meta, params);
        }
        let indexes = self.indexes_of(&upd.table)?;
        // Columns named in the SET list — drives `UPDATE OF col,…` trigger firing.
        let mut changed: Vec<String> = upd.assignments.iter().map(|(c, _)| c.clone()).collect();
        for (rcols, _) in &upd.row_assignments {
            changed.extend(rcols.iter().cloned());
        }
        // UPDATE … FROM: materialize the extra tables once. Each target row is
        // joined to the first FROM-row combination satisfying WHERE, and that
        // row's columns are visible to SET/WHERE. Without FROM, `from_rows` is
        // empty and the target is matched against WHERE directly.
        let from_data = match &upd.from {
            Some(fc) => {
                // A from-only synthetic SELECT to reuse the join scanner. Its WHERE
                // stays empty (the UPDATE's WHERE references the target too and is
                // applied per target row below), so the scan is a plain superset.
                let synth = Select {
                    ctes: Vec::new(),
                    compound: Vec::new(),
                    distinct: false,
                    columns: Vec::new(),
                    from: Some(fc.clone()),
                    where_clause: None,
                    group_by: Vec::new(),
                    having: None,
                    window_defs: Vec::new(),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                };
                let (cols, rows) = self.scan_source(&synth, params)?;
                Some((cols, rows.into_iter().map(|r| r.values).collect::<Vec<_>>()))
            }
            None => None,
        };
        let combined_columns: Vec<ColumnInfo> = match &from_data {
            Some((cols, _)) => meta.columns.iter().chain(cols).cloned().collect(),
            None => Vec::new(),
        };
        // Collect (rowid, current values, matched FROM row) for matching rows.
        let mut targets: Vec<(i64, Vec<Value>, Option<Vec<Value>>)> = Vec::new();
        {
            let mut cur = TableCursor::new(self.backend.source(), meta.root);
            let encoding = self.backend.source().header().text_encoding;
            let mut ok = cur.first()?;
            while ok {
                let rowid = cur.rowid()?;
                let values = self.decode_full_row(&meta, rowid, &cur.payload()?, encoding)?;
                match &from_data {
                    // UPDATE … FROM: find the first joined row passing WHERE.
                    Some((_, from_rows)) => {
                        let mut matched = None;
                        for fr in from_rows {
                            let mut combined = values.clone();
                            combined.extend_from_slice(fr);
                            let ok = match &upd.where_clause {
                                Some(p) => {
                                    let ctx =
                                        row_ctx(&combined, &combined_columns, Some(rowid), params)
                                            .with_subqueries(self);
                                    eval::truth(&eval::eval(p, &ctx)?) == Some(true)
                                }
                                None => true,
                            };
                            if ok {
                                matched = Some(fr.clone());
                                break;
                            }
                        }
                        if let Some(fr) = matched {
                            targets.push((rowid, values, Some(fr)));
                        }
                    }
                    None => {
                        let matches = match &upd.where_clause {
                            Some(p) => {
                                let ctx = row_ctx(&values, &meta.columns, Some(rowid), params)
                                    .with_subqueries(self);
                                eval::truth(&eval::eval(p, &ctx)?) == Some(true)
                            }
                            None => true,
                        };
                        if matches {
                            targets.push((rowid, values, None));
                        }
                    }
                }
                ok = cur.next()?;
            }
        }
        // `ORDER BY … LIMIT …` selects which matching rows to update.
        if !upd.order_by.is_empty() || upd.limit.is_some() || upd.offset.is_some() {
            let rowids: Vec<i64> = targets.iter().map(|(r, _, _)| *r).collect();
            let kept = self.order_limit_rowids(
                &meta,
                rowids,
                &upd.order_by,
                upd.limit.as_ref(),
                upd.offset.as_ref(),
                params,
            )?;
            // Reorder/filter `targets` to the kept rowids, preserving kept order.
            let mut by_id: alloc::collections::BTreeMap<i64, (Vec<Value>, Option<Vec<Value>>)> =
                targets.into_iter().map(|(r, v, f)| (r, (v, f))).collect();
            targets = kept
                .into_iter()
                .filter_map(|r| by_id.remove(&r).map(|(v, f)| (r, v, f)))
                .collect();
        }

        // Evaluate every target row's SET assignments against the table as it is
        // BEFORE any write, so a subquery in a SET expression sees a consistent
        // snapshot — `UPDATE t SET b=(SELECT sum(b) FROM t)` uses the original sum
        // for every row, exactly like sqlite — rather than observing rows updated
        // earlier in the same statement. Writes happen in the second pass below.
        let mut prepared: Vec<(i64, Vec<Value>, Vec<Value>)> = Vec::with_capacity(targets.len());
        for (rowid, mut values, matched_from) in targets {
            let old_row = values.clone();
            for (col, expr) in &upd.assignments {
                let pos = meta
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col))
                    .ok_or_else(|| Error::Error(format!("no such column: {col}")))?;
                if meta.is_generated(pos) {
                    return Err(Error::Error(format!(
                        "cannot UPDATE generated column \"{col}\""
                    )));
                }
                // SQLite evaluates every SET expression against the ORIGINAL row
                // (assignments are simultaneous): `SET a=b, b=a` swaps. Evaluate
                // against `old_row`, not the progressively-mutated `values`.
                let new = match &matched_from {
                    Some(fr) => {
                        let mut combined = old_row.clone();
                        combined.extend_from_slice(fr);
                        let ctx = row_ctx(&combined, &combined_columns, Some(rowid), params)
                            .with_subqueries(self);
                        eval::eval(expr, &ctx)?
                    }
                    None => {
                        let ctx = row_ctx(&old_row, &meta.columns, Some(rowid), params)
                            .with_subqueries(self);
                        eval::eval(expr, &ctx)?
                    }
                };
                values[pos] = new;
            }
            if !upd.row_assignments.is_empty() {
                // Build the same (possibly FROM-combined) original-row context the
                // per-expr assignments used, then run each row-value subquery.
                let combined_row;
                let (ctx_row, ctx_cols): (&[Value], &[ColumnInfo]) = match &matched_from {
                    Some(fr) => {
                        let mut c = old_row.clone();
                        c.extend_from_slice(fr);
                        combined_row = c;
                        (&combined_row, &combined_columns)
                    }
                    None => (&old_row, &meta.columns),
                };
                let ctx = row_ctx(ctx_row, ctx_cols, Some(rowid), params).with_subqueries(self);
                self.apply_row_subquery_assignments(
                    &upd.row_assignments,
                    &meta.columns,
                    Some(&meta),
                    &ctx,
                    &mut values,
                )?;
            }
            apply_column_affinity(&meta, &mut values);
            self.materialize_generated(&meta, &mut values, params)?;
            prepared.push((rowid, old_row, values));
        }

        let mut affected = 0;
        for (rowid, old_row, mut values) in prepared {
            // NOT NULL / CHECK / STRICT-type constraints. `UPDATE OR IGNORE` skips
            // a row that violates one rather than failing the statement.
            {
                if !self.resolve_not_null(
                    &meta,
                    &mut values,
                    upd.on_conflict,
                    upd.on_conflict_explicit,
                    params,
                )? {
                    continue;
                }
                let r = self
                    .check_strict_types(&meta, &values)
                    .and_then(|()| self.check_constraints(&meta, &values, Some(rowid), params));
                match r {
                    Ok(()) => {}
                    Err(Error::Constraint(_)) if upd.on_conflict == OnConflict::Ignore => continue,
                    Err(Error::Constraint(m)) => {
                        return Err(self.conflict_error(upd.on_conflict, &m))
                    }
                    Err(e) => return Err(e),
                }
            }
            // Foreign keys: this row as a child must still point at a parent, and
            // as a parent it must propagate referenced-key changes to children.
            self.check_fk_child(&upd.table, &meta, &values)?;
            if self.foreign_keys {
                self.enforce_parent_change(&upd.table, &old_row, Some(&values), params)?;
            }
            // New rowid if the IPK column was changed, else unchanged.
            let new_rowid = match meta.ipk {
                Some(ipk) => eval::to_i64(&values[ipk]),
                None => rowid,
            };
            self.fire_triggers(
                &upd.table,
                TrigEvent::Update,
                TriggerTiming::Before,
                &meta.columns,
                Some((&old_row, rowid)),
                Some((&values, new_rowid)),
                params,
                Some(&changed),
            )?;
            // A `BEFORE UPDATE` trigger's `RAISE(IGNORE)` leaves this row alone.
            if self.raise_ignore.replace(false) {
                continue;
            }
            // UNIQUE/PK conflict against any other row. `UPDATE OR IGNORE` skips
            // this row; `UPDATE OR REPLACE` deletes the conflicting rows first.
            let (conflicts, constraint_oc) =
                self.find_conflicts(&upd.table, &meta, new_rowid, &values, Some(rowid), params)?;
            let effective_oc = if upd.on_conflict_explicit {
                upd.on_conflict
            } else {
                constraint_oc
            };
            if !conflicts.is_empty() {
                match effective_oc {
                    OnConflict::Ignore => continue,
                    OnConflict::Replace => {
                        for cr in conflicts {
                            delete_table(self.backend.writer()?, meta.root, cr)?;
                        }
                    }
                    oc @ (OnConflict::Abort | OnConflict::Fail | OnConflict::Rollback) => {
                        let m = self.unique_violation_message(
                            &upd.table,
                            &meta,
                            new_rowid,
                            &values,
                            Some(rowid),
                            params,
                        );
                        return Err(self.conflict_error(oc, &m));
                    }
                }
            }
            let new_full = values.clone();
            let record = self.encode_table_record(&meta, &new_full);
            delete_table(self.backend.writer()?, meta.root, rowid)?;
            insert_table(self.backend.writer()?, meta.root, new_rowid, &record)?;
            self.fire_triggers(
                &upd.table,
                TrigEvent::Update,
                TriggerTiming::After,
                &meta.columns,
                Some((&old_row, rowid)),
                Some((&new_full, new_rowid)),
                params,
                Some(&changed),
            )?;
            if !upd.returning.is_empty() {
                self.collect_returning(&upd.returning, &meta, &new_full, Some(new_rowid), params)?;
            }
            affected += 1;
        }
        if affected > 0 {
            self.compact_table(&meta)?;
            self.rebuild_indexes(&meta, &indexes)?;
        }
        Ok(affected)
    }

    // ---- index DDL & maintenance --------------------------------------------

    fn exec_create_index(&mut self, ci: &CreateIndex, sql_text: &str) -> Result<()> {
        // A schema-qualified `CREATE INDEX aux.idx …` stores its SQL bare-named
        // in the target catalog (the `aux.` prefix is invalid there). Reprint.
        let reprinted;
        let sql_text = if ci.schema.is_some() {
            reprinted = sql::print::create_index(ci);
            reprinted.as_str()
        } else {
            sql_text
        };
        if self.schema.index(&ci.name).is_some() {
            if ci.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("index {} already exists", ci.name)));
        }
        if self.is_virtual_table(&ci.table) {
            return Err(Error::Error("virtual tables may not be indexed".into()));
        }
        let tmeta = self.table_meta(&ci.table, None)?;
        // SQLite rejects non-deterministic functions in an index expression (or a
        // partial-index predicate): the stored key could never match a recomputed
        // probe. Reject before building anything.
        if ci.columns.iter().any(|t| expr_is_nondeterministic(&t.expr))
            || ci
                .where_clause
                .as_ref()
                .is_some_and(expr_is_nondeterministic)
        {
            return Err(Error::Error(
                "non-deterministic functions prohibited in index expressions".into(),
            ));
        }
        let (cols, key_exprs, colls) = self.index_key_spec(&tmeta, ci)?;
        if key_exprs.is_some() && tmeta.without_rowid {
            return Err(Error::Unsupported(
                "expression indexes on WITHOUT ROWID tables",
            ));
        }
        let schema_next = self.next_rowid(crate::schema::SCHEMA_ROOT_PAGE)?;

        // A partial index (`CREATE INDEX … WHERE p`) only stores rows for which
        // the predicate holds; evaluate it up front (before the writer borrow).
        let no_params = Params::default();
        let keep_row = |values: &[Value], rowid: Option<i64>| -> Result<bool> {
            match &ci.where_clause {
                None => Ok(true),
                Some(p) => {
                    let ctx =
                        row_ctx(values, &tmeta.columns, rowid, &no_params).with_subqueries(self);
                    Ok(eval::truth(&eval::eval(p, &ctx)?) == Some(true))
                }
            }
        };

        // WITHOUT ROWID secondary indexes are keyed by (indexed cols, PK cols)
        // instead of (indexed cols, rowid).
        let root = if tmeta.without_rowid {
            let rows = self.scan_without_rowid(&tmeta)?;
            let keep: Vec<bool> = rows
                .iter()
                .map(|row| keep_row(row, None))
                .collect::<Result<_>>()?;
            let pk_cols = tmeta.storage_order[..tmeta.pk_len].to_vec();
            let mut key_colls = colls.clone();
            key_colls.extend(self.col_collations(&tmeta, &pk_cols));
            let w = self.backend.writer()?;
            let root = create_index_root(w)?;
            for (row, &k) in rows.iter().zip(&keep) {
                if k {
                    insert_index(w, root, &wr_index_key(&cols, &pk_cols, row), &key_colls)?;
                }
            }
            root
        } else {
            let rows = self.scan_table(&tmeta)?;
            // Precompute the key bytes of every included row (column values, or
            // evaluated expressions for an expression index) before the writer
            // borrow.
            let mut keys: Vec<Vec<u8>> = Vec::new();
            for (rowid, values) in &rows {
                if !keep_row(values, Some(*rowid))? {
                    continue;
                }
                keys.push(match &key_exprs {
                    None => index_key(&cols, values, *rowid),
                    Some(exprs) => {
                        let ctx = row_ctx(values, &tmeta.columns, Some(*rowid), &no_params)
                            .with_subqueries(self);
                        let mut k: Vec<Value> = exprs
                            .iter()
                            .map(|e| eval::eval(e, &ctx))
                            .collect::<Result<_>>()?;
                        k.push(Value::Integer(*rowid));
                        encode_record(&k)
                    }
                });
            }
            let w = self.backend.writer()?;
            let root = create_index_root(w)?;
            for key in &keys {
                insert_index(w, root, key, &colls)?;
            }
            root
        };
        let w = self.backend.writer()?;
        let schema_row = encode_record(&[
            Value::Text("index".into()),
            Value::Text(ci.name.clone()),
            Value::Text(ci.table.clone()),
            Value::Integer(root as i64),
            Value::Text(sql_text.into()),
        ]);
        insert_table(w, crate::schema::SCHEMA_ROOT_PAGE, schema_next, &schema_row)?;
        let cookie = w.header().schema_cookie.wrapping_add(1);
        w.header_mut().schema_cookie = cookie;
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    fn exec_create_view(&mut self, cv: &CreateView, sql_text: &str) -> Result<()> {
        // A schema-qualified `CREATE VIEW aux.v …` stores its SQL bare-named.
        let stripped;
        let sql_text = match cv.schema.as_deref() {
            Some(s) => {
                stripped = strip_schema_qualifier(sql_text, s)?;
                stripped.as_str()
            }
            None => sql_text,
        };
        let exists = self.schema.objects().iter().any(|o| o.name == cv.name);
        if exists {
            if cv.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("table {} already exists", cv.name)));
        }
        let next = self.next_rowid(crate::schema::SCHEMA_ROOT_PAGE)?;
        let row = encode_record(&[
            Value::Text("view".into()),
            Value::Text(cv.name.clone()),
            Value::Text(cv.name.clone()),
            Value::Integer(0), // views have no b-tree root
            Value::Text(sql_text.into()),
        ]);
        insert_table(
            self.backend.writer()?,
            crate::schema::SCHEMA_ROOT_PAGE,
            next,
            &row,
        )?;
        let cookie = self
            .backend
            .writer()?
            .header()
            .schema_cookie
            .wrapping_add(1);
        self.backend.writer()?.header_mut().schema_cookie = cookie;
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    /// Execute `CREATE VIRTUAL TABLE … USING module(args)`: look the module up in
    /// the registry, validate the arguments by connecting (so a bad CREATE fails
    /// now, not at first query), and persist a `sqlite_schema` row with
    /// `type='table'`, `rootpage=0`, and `sql` = the original CREATE text.
    fn exec_create_virtual_table(
        &mut self,
        cvt: &CreateVirtualTable,
        sql_text: &str,
    ) -> Result<()> {
        // A schema-qualified `CREATE VIRTUAL TABLE aux.v …` stores its SQL
        // bare-named, like CREATE TABLE/VIEW.
        let stripped;
        let sql_text = match cvt.schema.as_deref() {
            Some(s) => {
                stripped = strip_schema_qualifier(sql_text, s)?;
                stripped.as_str()
            }
            None => sql_text,
        };
        let exists = self.schema.objects().iter().any(|o| o.name == cvt.name);
        if exists {
            if cvt.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("table {} already exists", cvt.name)));
        }
        // The module must be registered, and must accept these arguments.
        let module = self
            .vtab_registry
            .get(&cvt.module)
            .ok_or_else(|| Error::Error(format!("no such module: {}", cvt.module)))?;
        let arg_refs: Vec<&str> = cvt.args.iter().map(String::as_str).collect();
        let schema = module.dyn_connect(&arg_refs)?;
        let persistent = module.dyn_persistent();
        let cols = schema.columns;
        // An R-Tree with no auxiliary columns uses SQLite's byte-compatible node
        // format (`_node`/`_rowid`/`_parent`) so its file round-trips through
        // sqlite3; an aux-column R-Tree and all other persistent modules keep the
        // generic `<name>_data` backing table.
        let rtree_n_coord = (cvt.module.eq_ignore_ascii_case("rtree")
            || cvt.module.eq_ignore_ascii_case("rtree_i32"))
        .then(|| crate::vtab::RTreeModule::n_coords(&arg_refs))
        .filter(|n| cols.len() == 1 + n);
        #[cfg(feature = "fts5")]
        let is_fts5 = cvt.module.eq_ignore_ascii_case("fts5");
        #[cfg(not(feature = "fts5"))]
        let is_fts5 = false;
        if let Some(n_coord) = rtree_n_coord {
            let integer = cvt.module.eq_ignore_ascii_case("rtree_i32");
            self.rtree_create_storage(&cvt.name, n_coord, integer)?;
        } else if is_fts5 {
            // FTS5 uses sqlite's five shadow tables (so the file round-trips
            // through stock sqlite), not the generic `<name>_data` store.
            #[cfg(feature = "fts5")]
            self.fts5_create_storage(&cvt.name, cols.len())?;
        } else if persistent {
            let coldefs = cols
                .iter()
                .map(|c| sql::print::ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            let backing_sql = format!(
                "CREATE TABLE {}({coldefs})",
                sql::print::ident(&format!("{}_data", cvt.name))
            );
            let Statement::CreateTable(ct) = sql::parse_one(&backing_sql)? else {
                unreachable!("constructed a CREATE TABLE");
            };
            self.exec_create_table(&ct, &backing_sql)?;
        }

        let next = self.next_rowid(crate::schema::SCHEMA_ROOT_PAGE)?;
        let row = encode_record(&[
            Value::Text("table".into()),
            Value::Text(cvt.name.clone()),
            Value::Text(cvt.name.clone()),
            Value::Integer(0), // virtual tables have no b-tree root
            Value::Text(sql_text.into()),
        ]);
        insert_table(
            self.backend.writer()?,
            crate::schema::SCHEMA_ROOT_PAGE,
            next,
            &row,
        )?;
        let cookie = self
            .backend
            .writer()?
            .header()
            .schema_cookie
            .wrapping_add(1);
        self.backend.writer()?.header_mut().schema_cookie = cookie;
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    /// Whether the named object is a virtual table (a `type='table'` schema row
    /// whose stored SQL is a `CREATE VIRTUAL TABLE`). Such a table has no b-tree
    /// (`rootpage = 0`) and is scanned through its registered module instead.
    fn is_virtual_table(&self, name: &str) -> bool {
        self.schema
            .objects()
            .iter()
            .filter(|o| {
                o.obj_type == crate::schema::ObjectType::Table && o.name.eq_ignore_ascii_case(name)
            })
            .any(|o| {
                matches!(
                    o.sql.as_deref().map(sql::parse_one),
                    Some(Ok(Statement::CreateVirtualTable(_)))
                )
            })
    }

    /// The module name, `USING` arguments, and declared column names of a virtual
    /// table — by reparsing its stored `CREATE VIRTUAL TABLE` and asking the
    /// module to `connect`. Used by the write path.
    fn vtab_meta(&self, name: &str) -> Result<(String, Vec<String>, crate::vtab::VTabSchema)> {
        use crate::schema::ObjectType;
        let obj = self
            .schema
            .objects()
            .iter()
            .find(|o| o.obj_type == ObjectType::Table && o.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::Error(format!("no such table: {name}")))?;
        let Some(Ok(Statement::CreateVirtualTable(cvt))) = obj.sql.as_deref().map(sql::parse_one)
        else {
            return Err(Error::Error(format!("{name} is not a virtual table")));
        };
        let module = self
            .vtab_registry
            .get(&cvt.module)
            .ok_or_else(|| Error::Error(format!("no such module: {}", cvt.module)))?;
        let arg_refs: Vec<&str> = cvt.args.iter().map(String::as_str).collect();
        let schema = module.dyn_connect(&arg_refs)?;
        Ok((cvt.module.clone(), cvt.args.clone(), schema))
    }

    /// `INSERT` into a virtual table: evaluate each row's values into the module's
    /// declared column order and hand them to its
    /// [`update`](crate::vtab::VTabModule::update) (SQLite's `xUpdate` insert).
    /// A read-only module's default `update` rejects the write.
    /// Run `f` with the named module taken out of the registry and a [`VTabStore`]
    /// over its `<table>_data` backing table, re-registering the module afterward.
    /// Taking the module out lets the store hold `&mut Connection` without aliasing
    /// the borrowed module. Callers do all read-only work (evaluating values,
    /// scanning rows) *before* this, then only persist inside `f`.
    fn with_vtab_store<F>(
        &mut self,
        module_name: &str,
        args: &[String],
        table: &str,
        f: F,
    ) -> Result<usize>
    where
        F: FnOnce(&dyn DynVTabModule, &mut dyn VTabStore, &[&str]) -> Result<usize>,
    {
        let module = self
            .vtab_registry
            .unregister(module_name)
            .ok_or_else(|| Error::Error(format!("no such module: {module_name}")))?;
        // FTS5 keeps its documents in `<name>_content` (sqlite's layout); every
        // other persistent module uses the generic `<name>_data` store.
        let backing = if module_name.eq_ignore_ascii_case("fts5") {
            format!("{table}_content")
        } else {
            format!("{table}_data")
        };
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = {
            let mut store = ExecVTabStore {
                conn: self,
                backing: &backing,
                ipk_prefix: module_name.eq_ignore_ascii_case("fts5"),
            };
            f(&*module, &mut store, &arg_refs)
        };
        self.vtab_registry.register(module_name, module)?;
        result
    }

    fn exec_vtab_insert(
        &mut self,
        ins: &Insert,
        rows: &[Vec<Expr>],
        params: &Params,
    ) -> Result<usize> {
        if !ins.upsert.is_empty() || !ins.returning.is_empty() {
            return Err(Error::Unsupported("UPSERT / RETURNING on a virtual table"));
        }
        let (module_name, args, schema) = self.vtab_meta(&ins.table)?;
        let col_names = schema.columns;
        let ncols = col_names.len();
        // FTS5 exposes a hidden column named after the table that accepts special
        // commands: `INSERT INTO t(t) VALUES('rebuild'|'optimize')` issues a
        // maintenance command rather than inserting a row. graphite's `fts5` index
        // is scan-based, so `rebuild` and `optimize` are no-ops (there is no
        // separate index to rebuild); other commands fall through to the usual
        // column resolution (and its "no such column" error), matching SQLite,
        // which rejects `delete`/`delete-all` on an ordinary content table.
        #[cfg(feature = "fts5")]
        if module_name.eq_ignore_ascii_case("fts5")
            && ins.columns.len() == 1
            && ins.columns[0].eq_ignore_ascii_case(&ins.table)
        {
            let commands = rows
                .iter()
                .map(|row| {
                    let ctx = EvalCtx::rowless(params).with_subqueries(self);
                    Ok(eval::to_text(&eval::eval(&row[0], &ctx)?))
                })
                .collect::<Result<Vec<_>>>()?;
            if commands
                .iter()
                .all(|c| matches!(c.as_str(), "rebuild" | "optimize"))
            {
                return Ok(0);
            }
        }
        // Map the (possibly explicit) column list onto declared column positions.
        // `None` marks a `rowid`/`_rowid_`/`oid` term (a vtab's hidden rowid),
        // whose value becomes the inserted row's explicit rowid.
        let target: Vec<Option<usize>> = if ins.columns.is_empty() {
            (0..ncols).map(Some).collect()
        } else {
            ins.columns
                .iter()
                .map(
                    |name| match col_names.iter().position(|c| c.eq_ignore_ascii_case(name)) {
                        Some(p) => Ok(Some(p)),
                        None if matches!(
                            name.to_ascii_lowercase().as_str(),
                            "rowid" | "_rowid_" | "oid"
                        ) =>
                        {
                            Ok(None)
                        }
                        None => Err(Error::Error(format!("no such column: {name}"))),
                    },
                )
                .collect::<Result<_>>()?
        };
        // Evaluate every row up front (a read-only borrow of self), then persist.
        let mut changes: Vec<(Option<i64>, Vec<Value>)> = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != target.len() {
                return Err(Error::Error(format!(
                    "{} values for {} columns",
                    row.len(),
                    target.len()
                )));
            }
            let mut values = alloc::vec![Value::Null; ncols];
            let mut rowid = None;
            for (j, expr) in row.iter().enumerate() {
                let ctx = EvalCtx::rowless(params).with_subqueries(self);
                let v = eval::eval(expr, &ctx)?;
                match target[j] {
                    Some(col) => values[col] = v,
                    None => rowid = Some(eval::to_i64(&v)),
                }
            }
            changes.push((rowid, values));
        }
        let on_conflict = ins.on_conflict;
        let table = ins.table.clone();
        let id_col = col_names
            .first()
            .cloned()
            .unwrap_or_else(|| String::from("rowid"));
        // R-Tree with no aux columns: store in SQLite's byte-compatible node tree.
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let rtree_nc = (module_name.eq_ignore_ascii_case("rtree")
            || module_name.eq_ignore_ascii_case("rtree_i32"))
        .then(|| crate::vtab::RTreeModule::n_coords(&arg_refs))
        .filter(|n| ncols == 1 + n);
        if let Some(n_coord) = rtree_nc {
            let integer = module_name.eq_ignore_ascii_case("rtree_i32");
            let mut existing: alloc::collections::BTreeSet<i64> = self
                .rtree_entries(&table, n_coord, integer)?
                .iter()
                .map(|c| c.key)
                .collect();
            let mut next_auto = existing.iter().max().copied().unwrap_or(0) + 1;
            let mut cells: Vec<RtreeCell> = Vec::new();
            let mut n = 0;
            for (rowid, values) in &changes {
                let rid = rowid
                    .or(match values.first() {
                        Some(Value::Integer(i)) => Some(*i),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        let r = next_auto;
                        next_auto += 1;
                        r
                    });
                if existing.contains(&rid) {
                    match on_conflict {
                        OnConflict::Replace => {}
                        OnConflict::Ignore => continue,
                        _ => {
                            return Err(Error::Constraint(format!(
                                "UNIQUE constraint failed: {table}.{id_col}"
                            )))
                        }
                    }
                }
                existing.insert(rid);
                cells.retain(|c| c.key != rid); // OR REPLACE within this batch
                cells.push(rtree_cell_from_values(rid, values, n_coord, integer)?);
                n += 1;
            }
            self.rtree_apply(&table, n_coord, integer, cells, &[])?;
            return Ok(n);
        }
        let inserted = self.with_vtab_store(
            &module_name,
            &args,
            &ins.table,
            |module, store, arg_refs| {
                // An explicit rowid that already exists is a UNIQUE conflict on the
                // implicit rowid — error (or skip/replace per `OR IGNORE`/`REPLACE`),
                // matching sqlite, rather than silently overwriting the row. Only a
                // store-backed (persistent) vtab is checked here; a non-persistent
                // module (no `<name>_data` table → `rows()` errors) manages its own.
                let mut existing: alloc::collections::BTreeSet<i64> = store
                    .rows()
                    .map(|rows| rows.iter().map(|(r, _)| *r).collect())
                    .unwrap_or_default();
                // The effective rowid is the explicit `rowid` term, or — for a
                // module with a rowid-alias column (rtree's `id`) — that column's
                // value when not NULL.
                let rowid_col = module.dyn_rowid_column();
                let mut n = 0;
                for (rowid, values) in &changes {
                    let effective = rowid.or_else(|| {
                        let v = values.get(rowid_col?)?;
                        (!matches!(v, Value::Null)).then(|| eval::to_i64(v))
                    });
                    if let Some(id) = effective {
                        if existing.contains(&id) {
                            match on_conflict {
                                OnConflict::Replace => {}
                                OnConflict::Ignore => continue,
                                _ => {
                                    return Err(Error::Constraint(format!(
                                        "UNIQUE constraint failed: {table}.{id_col}"
                                    )))
                                }
                            }
                        }
                    }
                    let assigned = module.dyn_update(
                        arg_refs,
                        VTabChange::Insert {
                            rowid: *rowid,
                            values,
                        },
                        store,
                    )?;
                    existing.insert(assigned);
                    n += 1;
                }
                Ok(n)
            },
        )?;
        self.fts5_maybe_rebuild(&module_name, &ins.table)?;
        Ok(inserted)
    }

    /// `DELETE` from a virtual table: scan it for rows matching the `WHERE`, then
    /// call the module's [`update`](crate::vtab::VTabModule::update) with
    /// [`VTabChange::Delete`] for each (over a materialized snapshot, so deleting
    /// during iteration is safe).
    fn exec_vtab_delete(&mut self, del: &Delete, params: &Params) -> Result<usize> {
        if !del.returning.is_empty() {
            return Err(Error::Unsupported("RETURNING on a virtual table"));
        }
        let (module_name, args, _) = self.vtab_meta(&del.table)?;
        let (columns, rows) = self
            .try_virtual_table(&del.table, None, None)?
            .ok_or_else(|| Error::Error(format!("{} is not a virtual table", del.table)))?;
        // Collect the matching rowids first (read-only), then persist.
        let mut victims: Vec<i64> = Vec::new();
        for r in &rows {
            if let Some(pred) = &del.where_clause {
                let ctx = r.ctx(&columns, params).with_subqueries(self);
                if eval::truth(&eval::eval(pred, &ctx)?) != Some(true) {
                    continue;
                }
            }
            victims.push(
                r.rowid
                    .ok_or_else(|| Error::Error("virtual-table row has no rowid".into()))?,
            );
        }
        // R-Tree with no aux columns: rebuild the node tree without the victims.
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let rtree_nc = (module_name.eq_ignore_ascii_case("rtree")
            || module_name.eq_ignore_ascii_case("rtree_i32"))
        .then(|| crate::vtab::RTreeModule::n_coords(&arg_refs))
        .filter(|n| columns.len() == 1 + n);
        if let Some(n_coord) = rtree_nc {
            let integer = module_name.eq_ignore_ascii_case("rtree_i32");
            self.rtree_apply(&del.table, n_coord, integer, Vec::new(), &victims)?;
            return Ok(victims.len());
        }
        let deleted = self.with_vtab_store(
            &module_name,
            &args,
            &del.table,
            |module, store, arg_refs| {
                for rowid in &victims {
                    module.dyn_update(arg_refs, VTabChange::Delete { rowid: *rowid }, store)?;
                }
                Ok(victims.len())
            },
        )?;
        self.fts5_maybe_rebuild(&module_name, &del.table)?;
        Ok(deleted)
    }

    /// `UPDATE` of a virtual table: scan for rows matching the `WHERE`, evaluate
    /// the `SET` assignments against each, and call the module's
    /// [`update`](crate::vtab::VTabModule::update) with [`VTabChange::Update`].
    fn exec_vtab_update(&mut self, upd: &Update, params: &Params) -> Result<usize> {
        if !upd.returning.is_empty() {
            return Err(Error::Unsupported("RETURNING on a virtual table"));
        }
        if !upd.row_assignments.is_empty() {
            return Err(Error::Unsupported(
                "UPDATE SET (…) = (SELECT …) on a virtual table",
            ));
        }
        if upd.from.is_some() {
            return Err(Error::Unsupported("UPDATE … FROM on a virtual table"));
        }
        let (module_name, args, schema) = self.vtab_meta(&upd.table)?;
        let col_names = schema.columns;
        // Resolve each SET target to a declared column position.
        let assigns: Vec<(usize, &Expr)> = upd
            .assignments
            .iter()
            .map(|(name, value)| {
                col_names
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(name))
                    .map(|pos| (pos, value))
                    .ok_or_else(|| Error::Error(format!("no such column: {name}")))
            })
            .collect::<Result<_>>()?;
        let (columns, rows) = self
            .try_virtual_table(&upd.table, None, None)?
            .ok_or_else(|| Error::Error(format!("{} is not a virtual table", upd.table)))?;
        // Compute the new (rowid, values) for each matching row first, then persist.
        let mut changes: Vec<(i64, Vec<Value>)> = Vec::new();
        for r in &rows {
            let ctx = r.ctx(&columns, params).with_subqueries(self);
            if let Some(pred) = &upd.where_clause {
                if eval::truth(&eval::eval(pred, &ctx)?) != Some(true) {
                    continue;
                }
            }
            // Every SET RHS evaluates against the original row (simultaneous).
            let mut values = r.values.clone();
            for (pos, expr) in &assigns {
                values[*pos] = eval::eval(expr, &ctx)?;
            }
            let rowid = r
                .rowid
                .ok_or_else(|| Error::Error("virtual-table row has no rowid".into()))?;
            changes.push((rowid, values));
        }
        // R-Tree with no aux columns: rebuild the node tree (delete old + insert
        // new; the `id` column may move the rowid).
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let rtree_nc = (module_name.eq_ignore_ascii_case("rtree")
            || module_name.eq_ignore_ascii_case("rtree_i32"))
        .then(|| crate::vtab::RTreeModule::n_coords(&arg_refs))
        .filter(|n| columns.len() == 1 + n);
        if let Some(n_coord) = rtree_nc {
            let integer = module_name.eq_ignore_ascii_case("rtree_i32");
            let mut deletes = Vec::with_capacity(changes.len());
            let mut inserts = Vec::with_capacity(changes.len());
            for (old_rowid, values) in &changes {
                deletes.push(*old_rowid);
                let new_rid = match values.first() {
                    Some(Value::Null) | None => *old_rowid,
                    Some(v) => eval::to_i64(v),
                };
                inserts.push(rtree_cell_from_values(new_rid, values, n_coord, integer)?);
            }
            self.rtree_apply(&upd.table, n_coord, integer, inserts, &deletes)?;
            return Ok(changes.len());
        }
        let updated = self.with_vtab_store(
            &module_name,
            &args,
            &upd.table,
            |module, store, arg_refs| {
                for (rowid, values) in &changes {
                    module.dyn_update(
                        arg_refs,
                        VTabChange::Update {
                            rowid: *rowid,
                            new_rowid: *rowid,
                            values,
                        },
                        store,
                    )?;
                }
                Ok(changes.len())
            },
        )?;
        self.fts5_maybe_rebuild(&module_name, &upd.table)?;
        Ok(updated)
    }

    /// Produce the columns and rows of a virtual table used as a `FROM` source:
    /// reparse its stored `CREATE VIRTUAL TABLE`, look the module up in the
    /// registry, `connect` for its column schema, then `open` a cursor and drain
    /// it. Returns `Ok(None)` when `name` is not a virtual table.
    ///
    /// `pushdown`, when given as `Some((sel, params))`, lets the module restrict
    /// what it produces from the query's `WHERE` (constraint pushdown via
    /// [`best_index`](crate::vtab::VTabModule::best_index) /
    /// [`filter`](crate::vtab::VTabModule::filter)). The plan is always a superset:
    /// the caller's `run_core` re-applies the full `WHERE`, so even a partially
    /// consumed or ignored constraint stays correct.
    fn try_virtual_table(
        &self,
        name: &str,
        alias: Option<&str>,
        pushdown: Option<(&Select, &Params)>,
    ) -> Result<Option<(Vec<ColumnInfo>, Vec<InputRow>)>> {
        use crate::schema::ObjectType;
        let obj = match self
            .schema
            .objects()
            .iter()
            .find(|o| o.obj_type == ObjectType::Table && o.name.eq_ignore_ascii_case(name))
        {
            Some(o) => o,
            None => return Ok(None),
        };
        let sql = match obj.sql.as_deref() {
            Some(s) => s,
            None => return Ok(None),
        };
        let cvt = match sql::parse_one(sql) {
            Ok(Statement::CreateVirtualTable(cvt)) => cvt,
            _ => return Ok(None),
        };
        // `fts5vocab` is derived from another FTS5 table's documents; compute it
        // here (the module's cursor has no database access).
        #[cfg(feature = "fts5")]
        if cvt.module.eq_ignore_ascii_case("fts5vocab") {
            return Ok(Some(self.scan_fts5vocab(&cvt.args, name, alias)?));
        }
        let module = self
            .vtab_registry
            .get(&cvt.module)
            .ok_or_else(|| Error::Error(format!("no such module: {}", cvt.module)))?;
        let arg_refs: Vec<&str> = cvt.args.iter().map(String::as_str).collect();
        let schema = module.dyn_connect(&arg_refs)?;
        let label = alias.unwrap_or(name).to_string();
        let columns: Vec<ColumnInfo> = schema
            .columns
            .iter()
            .map(|n| ColumnInfo {
                name: n.clone(),
                table: label.clone(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            })
            .collect();
        // A persistent module keeps its rows in the `<vtab>_data` backing table;
        // scan that directly (run_core re-applies the full WHERE, so the rows are
        // a valid superset). Computed modules go through the cursor path below.
        if module.dyn_persistent() {
            // An R-Tree written by SQLite keeps its entries in the `<name>_node`
            // b-tree of nodes (byte-compatible on-disk format), not graphite's
            // generic `<name>_data` backing table. Read the node tree directly so
            // graphite can query a sqlite-written R-Tree. (No-aux R-Trees only;
            // aux columns live in `<name>_rowid` — handled when graphite also
            // writes the node format.)
            let rtree = cvt.module.eq_ignore_ascii_case("rtree")
                || cvt.module.eq_ignore_ascii_case("rtree_i32");
            if rtree
                && self.schema.table(&format!("{name}_node")).is_some()
                && self.schema.table(&format!("{name}_data")).is_none()
            {
                let n_coords = crate::vtab::RTreeModule::n_coords(&arg_refs);
                if columns.len() == 1 + n_coords {
                    let integer = cvt.module.eq_ignore_ascii_case("rtree_i32");
                    // Spatial pushdown: turn the query's coordinate comparisons into
                    // per-dimension bounds the node walk uses to prune subtrees.
                    // Column 0 is the rowid/id; columns 1.. are the coordinates.
                    let bbox: Vec<(usize, ConstraintOp, f64)> = match pushdown {
                        Some((sel, params)) => {
                            let (cs, vs) = collect_vtab_constraints(sel, &columns, params);
                            cs.iter()
                                .zip(vs)
                                .filter_map(|(c, v)| {
                                    let ci = c.column.checked_sub(1)?;
                                    if ci >= n_coords {
                                        return None;
                                    }
                                    let fv = match v {
                                        Value::Integer(i) => i as f64,
                                        Value::Real(r) => r,
                                        _ => return None,
                                    };
                                    matches!(
                                        c.op,
                                        ConstraintOp::Eq
                                            | ConstraintOp::Gt
                                            | ConstraintOp::Le
                                            | ConstraintOp::Lt
                                            | ConstraintOp::Ge
                                    )
                                    .then_some((ci, c.op, fv))
                                })
                                .collect()
                        }
                        None => Vec::new(),
                    };
                    let rows = self.scan_rtree_nodes(name, n_coords, integer, &bbox)?;
                    return Ok(Some((columns, rows)));
                }
            }
            // A SQLite-written FTS5 keeps its documents in `<name>_content`
            // (`id, c0, c1, …`), with the inverted index in `<name>_data`/`_idx`.
            // graphite answers queries — including `MATCH` — from the documents via
            // its scan-based matcher, so reading the content is sufficient.
            // (graphite's own FTS5 has no `_content`; it stores docs in `_data`.)
            #[cfg(feature = "fts5")]
            if cvt.module.eq_ignore_ascii_case("fts5")
                && self.schema.table(&format!("{name}_content")).is_some()
            {
                let cmeta = self.table_meta(&format!("{name}_content"), None)?;
                let rows = self
                    .scan_table(&cmeta)?
                    .into_iter()
                    .map(|(rowid, mut values)| {
                        // Drop the leading `id` column; the rest are the fts5 columns.
                        if !values.is_empty() {
                            values.remove(0);
                        }
                        InputRow {
                            values,
                            rowid: Some(rowid),
                        }
                    })
                    .collect();
                return Ok(Some((columns, rows)));
            }
            let backing = format!("{name}_data");
            let bmeta = self.table_meta(&backing, None)?;
            let rows = self
                .scan_table(&bmeta)?
                .into_iter()
                .map(|(rowid, values)| InputRow {
                    values,
                    rowid: Some(rowid),
                })
                .collect();
            return Ok(Some((columns, rows)));
        }
        // Constraint pushdown: offer the WHERE's usable comparisons to the module,
        // let it choose a plan, then hand back the bound values it requested.
        let (constraints, bound_values) = match pushdown {
            Some((sel, params)) => collect_vtab_constraints(sel, &columns, params),
            None => (Vec::new(), Vec::new()),
        };
        let plan = module.dyn_best_index(&constraints)?;
        let argv = order_vtab_argv(&plan, &bound_values);
        let mut cursor = module.dyn_open(&arg_refs, &plan, &argv)?;
        let ncols = columns.len();
        let mut rows = Vec::new();
        while let Some(row) = cursor.dyn_next()? {
            let values = (0..ncols).map(|i| row.dyn_column(i)).collect();
            rows.push(InputRow {
                values,
                rowid: Some(row.dyn_rowid()),
            });
        }
        Ok(Some((columns, rows)))
    }

    /// Materialize each `WITH` CTE of `sel` into the environment, in declaration
    /// order (so a later CTE may reference an earlier one). Recursive CTEs are
    /// evaluated with the fixed-point loop.
    /// Materialize `ctes` into the environment. `outer_cap` (the consuming query's
    /// `LIMIT`+`OFFSET`, set only when that query streams the CTE 1:1 — see
    /// `recursive_cte_outer_cap`) bounds an otherwise-infinite recursive CTE so a
    /// `SELECT … FROM rcte LIMIT k` over an unterminated recursion yields `k` rows
    /// like sqlite instead of running to the runaway guard.
    fn push_ctes(&self, ctes: &[Cte], params: &Params, outer_cap: Option<usize>) -> Result<()> {
        for cte in ctes {
            let binding = if references_name(&cte.select, &cte.name) {
                self.eval_recursive_cte(cte, params, outer_cap)?
            } else {
                self.materialize_plain_cte(cte, params)?
            };
            self.cte_env.borrow_mut().push(binding);
        }
        Ok(())
    }

    /// Look up a CTE by name in the current environment (innermost first),
    /// returning a copy of its columns + rows relabeled to `alias` if given.
    fn lookup_cte(
        &self,
        name: &str,
        alias: Option<&str>,
    ) -> Option<(Vec<ColumnInfo>, Vec<InputRow>)> {
        let env = self.cte_env.borrow();
        let b = env
            .iter()
            .rev()
            .find(|b| b.name.eq_ignore_ascii_case(name))?;
        let label = alias.unwrap_or(&b.name);
        let columns = b
            .columns
            .iter()
            .map(|c| ColumnInfo {
                name: c.name.clone(),
                table: label.to_string(),
                affinity: c.affinity,
                collation: c.collation,
            })
            .collect();
        Some((columns, b.rows.clone()))
    }

    /// Build the column metadata for a CTE from its body's output labels (or its
    /// explicit `(col, …)` list), labeled with the CTE name.
    fn cte_columns(&self, cte: &Cte, body_cols: &[String]) -> Result<Vec<ColumnInfo>> {
        let names = if cte.columns.is_empty() {
            body_cols.to_vec()
        } else {
            // An explicit column list must match the body's column count, as in
            // SQLite (`table t has N values for M columns`).
            if cte.columns.len() != body_cols.len() {
                return Err(Error::Error(alloc::format!(
                    "table {} has {} values for {} columns",
                    cte.name,
                    body_cols.len(),
                    cte.columns.len()
                )));
            }
            cte.columns.clone()
        };
        Ok(names
            .into_iter()
            .map(|n| ColumnInfo {
                name: n,
                table: cte.name.clone(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            })
            .collect())
    }

    /// A non-recursive CTE: run its body once.
    fn materialize_plain_cte(&self, cte: &Cte, params: &Params) -> Result<CteBinding> {
        let result = self.run_select(&cte.select, params)?;
        let columns = self.cte_columns(cte, &result.columns)?;
        let rows = result
            .rows
            .into_iter()
            .map(|values| InputRow {
                values,
                rowid: None,
            })
            .collect();
        Ok(CteBinding {
            name: cte.name.clone(),
            columns,
            rows,
        })
    }

    /// A recursive CTE: `anchor [UNION [ALL] recursive]`. Evaluate the anchor,
    /// then repeatedly evaluate the recursive term against the rows produced by
    /// the previous step (bound to the CTE's name) until no new rows appear.
    fn eval_recursive_cte(
        &self,
        cte: &Cte,
        params: &Params,
        outer_cap: Option<usize>,
    ) -> Result<CteBinding> {
        // Flatten the body into arms: (op-before-this-arm, select). The first
        // arm has no preceding op.
        let mut arms: Vec<(Option<CompoundOp>, Select)> = Vec::new();
        let mut base = (*cte.select).clone();
        // A LIMIT/OFFSET on the CTE definition bounds the rows it produces — and
        // crucially terminates an otherwise-infinite recursion. Capture them
        // before stripping the per-arm clauses below. (A negative LIMIT means
        // "no limit", as elsewhere in SQLite.)
        let rec_limit = match &base.limit {
            Some(e) => {
                let n = must_be_int(eval::eval(
                    e,
                    &EvalCtx::rowless(params).with_subqueries(self),
                )?)?;
                (n >= 0).then_some(n as usize)
            }
            None => None,
        };
        let rec_offset = match &base.offset {
            Some(e) => must_be_int(eval::eval(
                e,
                &EvalCtx::rowless(params).with_subqueries(self),
            )?)?
            .max(0) as usize,
            None => 0,
        };
        let compound = core::mem::take(&mut base.compound);
        base.order_by.clear();
        base.limit = None;
        base.offset = None;
        arms.push((None, base));
        for (op, mut s) in compound {
            s.order_by.clear();
            s.limit = None;
            s.offset = None;
            arms.push((Some(op), s));
        }

        // Partition into leading anchor arms and trailing recursive arms.
        let mut anchor: Vec<Select> = Vec::new();
        let mut recursive: Vec<Select> = Vec::new();
        let mut rec_distinct = false;
        let mut in_rec = false;
        for (op, s) in arms {
            if !in_rec && references_name_select(&s, &cte.name) {
                in_rec = true;
                rec_distinct = matches!(op, Some(CompoundOp::Union));
            }
            if in_rec {
                recursive.push(s);
            } else {
                anchor.push(s);
            }
        }
        if anchor.is_empty() || recursive.is_empty() {
            return Err(Error::Unsupported(
                "recursive CTE must have a non-recursive anchor and a recursive term",
            ));
        }

        // Evaluate the anchor (a compound of the anchor arms).
        let mut anchor_rows: Vec<Vec<Value>> = Vec::new();
        for a in &anchor {
            let r = self.run_select(a, params)?;
            anchor_rows.extend(r.rows);
        }
        let body_cols = self.run_select(&anchor[0], params)?.columns;
        let columns = self.cte_columns(cte, &body_cols)?;

        if rec_distinct {
            dedup_rows(&mut anchor_rows);
        }
        let mut all_rows = anchor_rows.clone();
        let mut working = anchor_rows;

        // Push a working binding the recursive term resolves against; update it
        // each iteration. Guard against runaway recursion.
        let slot = self.cte_env.borrow().len();
        self.cte_env.borrow_mut().push(CteBinding {
            name: cte.name.clone(),
            columns: columns.clone(),
            rows: Vec::new(),
        });
        let mut guard = 0usize;
        let result = loop {
            guard += 1;
            if guard > 1_000_000 {
                self.cte_env.borrow_mut().truncate(slot);
                return Err(Error::Error("recursive CTE did not terminate".into()));
            }
            // Bind the working set.
            self.cte_env.borrow_mut()[slot].rows = working
                .iter()
                .cloned()
                .map(|values| InputRow {
                    values,
                    rowid: None,
                })
                .collect();

            let mut produced: Vec<Vec<Value>> = Vec::new();
            for r in &recursive {
                match self.run_select(r, params) {
                    Ok(res) => produced.extend(res.rows),
                    Err(e) => {
                        self.cte_env.borrow_mut().truncate(slot);
                        return Err(e);
                    }
                }
            }
            // Keep only genuinely new rows (for UNION; UNION ALL keeps all but
            // still must terminate — SQLite requires the recursive query to
            // eventually produce nothing).
            let mut fresh: Vec<Vec<Value>> = Vec::new();
            for row in produced {
                if rec_distinct && all_rows.iter().any(|s| rows_equal(s, &row)) {
                    continue;
                }
                fresh.push(row);
            }
            if fresh.is_empty() {
                break Ok(());
            }
            all_rows.extend(fresh.iter().cloned());
            working = fresh;
            // Stop once the CTE's LIMIT (after OFFSET) is satisfied.
            if let Some(lim) = rec_limit {
                if all_rows.len() >= rec_offset.saturating_add(lim) {
                    break Ok(());
                }
            }
            // Stop once the consuming query's LIMIT (+OFFSET) is satisfied — this
            // terminates an otherwise-infinite recursion `SELECT … FROM rcte LIMIT k`.
            if let Some(cap) = outer_cap {
                if all_rows.len() >= cap {
                    break Ok(());
                }
            }
        };
        self.cte_env.borrow_mut().truncate(slot);
        result?;

        // Apply the CTE definition's OFFSET/LIMIT to the produced rows.
        if rec_offset > 0 {
            all_rows.drain(..rec_offset.min(all_rows.len()));
        }
        if let Some(lim) = rec_limit {
            all_rows.truncate(lim);
        }

        let rows = all_rows
            .into_iter()
            .map(|values| InputRow {
                values,
                rowid: None,
            })
            .collect();
        Ok(CteBinding {
            name: cte.name.clone(),
            columns,
            rows,
        })
    }

    /// If `name` is a view, run its `SELECT` and return its columns + rows.
    /// A temp view shadows a main view of the same name (like a temp table), and
    /// is read through its own (temp) database via [`scan_db_view`](Self::scan_db_view).
    fn try_view(
        &self,
        name: &str,
        alias: Option<&str>,
        params: &Params,
    ) -> Result<Option<(Vec<ColumnInfo>, Vec<InputRow>)>> {
        use crate::schema::ObjectType;
        if self.temp_has_view(name) {
            return self.scan_db_view(DbRef::Temp, name, alias, params);
        }
        let obj = match self
            .schema
            .objects()
            .iter()
            .find(|o| o.obj_type == ObjectType::View && o.name.eq_ignore_ascii_case(name))
        {
            Some(o) => o.clone(),
            None => return Ok(None),
        };
        let sql = obj
            .sql
            .as_deref()
            .ok_or_else(|| Error::Corrupt("view has no CREATE statement".into()))?;
        let Statement::CreateView(cv) = sql::parse_one(sql)? else {
            return Err(Error::Corrupt("schema sql is not CREATE VIEW".into()));
        };
        let result = self.run_select(&cv.select, params)?;
        let label = alias.unwrap_or(name).to_string();
        // Column names: explicit view columns, else the SELECT's output labels.
        let names = if cv.columns.is_empty() {
            result.columns.clone()
        } else {
            cv.columns.clone()
        };
        let columns: Vec<ColumnInfo> = names
            .into_iter()
            .map(|n| ColumnInfo {
                name: n,
                table: label.clone(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            })
            .collect();
        let rows = result
            .rows
            .into_iter()
            .map(|values| InputRow {
                values,
                rowid: None,
            })
            .collect();
        Ok(Some((columns, rows)))
    }

    fn exec_drop(&mut self, d: &Drop) -> Result<()> {
        use crate::schema::ObjectType;
        // Dropping a persistent virtual table also drops its shadow tables, as
        // sqlite does: the generic `<name>_data` backing, or an R-Tree's
        // `_node`/`_rowid`/`_parent` node tables.
        if matches!(d.kind, DropKind::Table) && self.is_virtual_table(&d.name) {
            for suffix in [
                "_data", "_node", "_rowid", "_parent", "_content", "_docsize", "_config", "_idx",
            ] {
                let backing = format!("{}{suffix}", d.name);
                if self.schema.table(&backing).is_some() {
                    self.exec_drop(&Drop {
                        kind: DropKind::Table,
                        if_exists: false,
                        name: backing,
                        schema: d.schema.clone(),
                    })?;
                }
            }
        }
        let want = match d.kind {
            DropKind::Table => ObjectType::Table,
            DropKind::Index => ObjectType::Index,
            DropKind::View => ObjectType::View,
            DropKind::Trigger => ObjectType::Trigger,
        };
        // Find the object (and, for a table, its dependent indexes) to remove.
        let target = self
            .schema
            .objects()
            .iter()
            .find(|o| o.obj_type == want && o.name == d.name)
            .cloned();
        let Some(obj) = target else {
            // SQLite's table↔view confusion hint when a same-named object of the
            // other kind exists. This fires even with `IF EXISTS` — that clause
            // suppresses a *missing* object, not a *wrong-type* one.
            if let Some(other) = self.schema.objects().iter().find(|o| o.name == d.name) {
                match (d.kind, other.obj_type) {
                    (DropKind::Table, ObjectType::View) => {
                        return Err(Error::Error(format!(
                            "use DROP VIEW to delete view {}",
                            d.name
                        )))
                    }
                    (DropKind::View, ObjectType::Table) => {
                        return Err(Error::Error(format!(
                            "use DROP TABLE to delete table {}",
                            d.name
                        )))
                    }
                    _ => {}
                }
            }
            if d.if_exists {
                return Ok(());
            }
            let kind = match d.kind {
                DropKind::Table => "table",
                DropKind::Index => "index",
                DropKind::View => "view",
                DropKind::Trigger => "trigger",
            };
            return Err(Error::Error(format!("no such {kind}: {}", d.name)));
        };

        // Collect the schema rows (by rowid) and b-tree roots to drop.
        let mut roots_to_free = Vec::new();
        let mut names_to_remove = Vec::new();
        roots_to_free.push(obj.rootpage);
        names_to_remove.push(obj.name.clone());
        if want == ObjectType::Table {
            for idx in self.schema.indexes_on(&obj.name) {
                roots_to_free.push(idx.rootpage);
                names_to_remove.push(idx.name.clone());
            }
            // Triggers on the table are dropped with it (SQLite cascades these).
            for o in self.schema.objects() {
                if o.obj_type == ObjectType::Trigger && o.tbl_name.eq_ignore_ascii_case(&obj.name) {
                    roots_to_free.push(o.rootpage); // triggers have rootpage 0
                    names_to_remove.push(o.name.clone());
                }
            }
        }
        // Map names -> sqlite_schema rowids (scan page 1).
        let victim_rowids = self.schema_rowids_for(&names_to_remove)?;

        let w = self.backend.writer()?;
        for root in roots_to_free {
            if root != 0 {
                free_tree(w, root)?;
            }
        }
        for rid in victim_rowids {
            delete_table(w, crate::schema::SCHEMA_ROOT_PAGE, rid)?;
        }
        let cookie = w.header().schema_cookie.wrapping_add(1);
        w.header_mut().schema_cookie = cookie;
        self.schema = Schema::read(self.backend.source())?;
        // Dropping a table also removes its AUTOINCREMENT row from
        // `sqlite_sequence`, like SQLite.
        if want == ObjectType::Table && self.schema.table("sqlite_sequence").is_some() {
            let root = self.schema.table("sqlite_sequence").unwrap().rootpage;
            let meta = self.table_meta("sqlite_sequence", None)?;
            let victims: Vec<i64> = self
                .scan_table(&meta)?
                .into_iter()
                .filter(|(_, v)| matches!(&v[0], Value::Text(t) if t == &obj.name))
                .map(|(rid, _)| rid)
                .collect();
            for rid in victims {
                delete_table(self.backend.writer()?, root, rid)?;
            }
        }
        Ok(())
    }

    fn exec_alter(&mut self, a: &Alter) -> Result<()> {
        // A virtual table can be renamed (sqlite renames its backing tables too),
        // but not otherwise altered — and it isn't a CREATE TABLE, so it must not
        // reach the regular path below.
        if self.is_virtual_table(&a.table) {
            if let AlterAction::RenameTable(new_name) = &a.action {
                return self.rename_virtual_table(&a.table, new_name);
            }
            return Err(Error::Error("virtual tables may not be altered".into()));
        }
        let obj = self
            .schema
            .table(&a.table)
            .cloned()
            .ok_or_else(|| Error::Error(format!("no such table: {}", a.table)))?;
        let sql = obj
            .sql
            .as_deref()
            .ok_or_else(|| Error::Corrupt("table has no CREATE statement".into()))?;
        let Statement::CreateTable(mut ct) = sql::parse_one(sql)? else {
            return Err(Error::Corrupt("schema sql is not CREATE TABLE".into()));
        };

        if let AlterAction::DropColumn(name) = &a.action {
            return self.exec_drop_column(a, ct, name);
        }
        match &a.action {
            AlterAction::DropColumn(_) => unreachable!("handled above"),
            AlterAction::AddColumn(cd, col_text) => {
                if ct
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&cd.name))
                {
                    return Err(Error::Error(format!("duplicate column name: {}", cd.name)));
                }
                // SQLite forbids a few constraints on ADD COLUMN: a UNIQUE or
                // PRIMARY KEY column is always rejected; a NOT NULL column whose
                // default is NULL is rejected only when the table already has
                // rows (which would otherwise hold a NULL).
                for k in &cd.constraints {
                    match k {
                        ColumnConstraint::Unique(_) => {
                            return Err(Error::Error("Cannot add a UNIQUE column".into()));
                        }
                        ColumnConstraint::PrimaryKey { .. } => {
                            return Err(Error::Error("Cannot add a PRIMARY KEY column".into()));
                        }
                        _ => {}
                    }
                }
                let not_null = cd
                    .constraints
                    .iter()
                    .any(|k| matches!(k, ColumnConstraint::NotNull(_)));
                if not_null {
                    let default = cd.constraints.iter().find_map(|k| match k {
                        ColumnConstraint::Default(e) => Some(e),
                        _ => None,
                    });
                    let no_params = Params::default();
                    let default_is_null = match default {
                        None => true,
                        Some(e) => {
                            let ctx = EvalCtx::rowless(&no_params).with_subqueries(self);
                            matches!(eval::eval(e, &ctx), Ok(Value::Null) | Err(_))
                        }
                    };
                    if default_is_null && !self.table_is_empty(&a.table)? {
                        return Err(Error::Error(
                            "Cannot add a NOT NULL column with default value NULL".into(),
                        ));
                    }
                }
                ct.columns.push(cd.clone());
                // Append the new column's verbatim text to the stored CREATE (like
                // sqlite); fall back to reprinting from the AST if its source or
                // the column-list close can't be located.
                let reprint = sql::print::create_table(&ct);
                let table = a.table.clone();
                let col_text = col_text.clone();
                self.rewrite_schema_rows(|cols| {
                    if is_text(&cols[0], "table") && is_text(&cols[1], &table) {
                        let updated = match (&col_text, cols.get(4)) {
                            (Some(t), Some(Value::Text(old))) => {
                                append_column_to_create(old, t).unwrap_or_else(|| reprint.clone())
                            }
                            _ => reprint.clone(),
                        };
                        cols[4] = Value::Text(updated);
                        true
                    } else {
                        false
                    }
                })?;
            }
            AlterAction::RenameTable(new_name) => {
                // The new name must not collide with any existing table or index,
                // including renaming a table to its own name, as in SQLite.
                if self
                    .schema
                    .objects()
                    .iter()
                    .any(|o| o.name.eq_ignore_ascii_case(new_name))
                {
                    return Err(Error::Error(format!(
                        "there is already another table or index with this name: {new_name}"
                    )));
                }
                let old = a.table.clone();
                let new_name = new_name.clone();
                self.rewrite_schema_rows(|cols| {
                    if is_text(&cols[0], "table") && is_text(&cols[1], &old) {
                        cols[1] = Value::Text(new_name.clone());
                        cols[2] = Value::Text(new_name.clone());
                        // Edit the table name in the stored CREATE text in place
                        // (preserving the body verbatim), like SQLite — rather than
                        // reprinting the whole definition from the AST.
                        if let Some(Value::Text(old_sql)) = cols.get(4).cloned() {
                            // Rename the table token itself, and any self-referential
                            // foreign key (`REFERENCES <old>`) in its own body.
                            let renamed = rename_table_token_after(&old_sql, "table", &new_name);
                            cols[4] = Value::Text(rewrite_fk_references(&renamed, &old, &new_name));
                        }
                        true
                    } else if is_text(&cols[2], &old) {
                        // Dependent index/trigger/view: repoint, and rewrite an
                        // index's `ON` clause / a trigger's body to the new name.
                        cols[2] = Value::Text(new_name.clone());
                        if is_text(&cols[0], "index") {
                            // Repoint the index's `ON <table>` to the new name in
                            // place (preserving the rest), like SQLite.
                            if let Some(Value::Text(isql)) = cols.get(4).cloned() {
                                cols[4] =
                                    Value::Text(rename_table_token_after(&isql, "on", &new_name));
                            }
                        } else if is_text(&cols[0], "trigger") {
                            // A trigger ON the renamed table: rewrite the renamed
                            // name throughout its stored text (the `ON` clause and
                            // any body references), like SQLite.
                            if let Some(Value::Text(tsql)) = cols.get(4).cloned() {
                                cols[4] = Value::Text(rewrite_ident_tokens(
                                    &tsql,
                                    &old,
                                    &sql::print::ident(&new_name),
                                ));
                            }
                        }
                        true
                    } else if is_text(&cols[0], "view") {
                        // A view whose SELECT references the renamed table: rewrite
                        // the table name throughout its stored body (formatting
                        // preserved), so `SELECT … FROM v` keeps working.
                        match cols.get(4).cloned() {
                            Some(Value::Text(vsql)) if view_uses_table(&vsql, &old) => {
                                cols[4] = Value::Text(rewrite_ident_tokens(
                                    &vsql,
                                    &old,
                                    &sql::print::ident(&new_name),
                                ));
                                true
                            }
                            _ => false,
                        }
                    } else if is_text(&cols[0], "trigger") {
                        // A trigger on ANOTHER table whose body references the
                        // renamed table (e.g. `INSERT INTO <table> …`): rewrite the
                        // renamed name throughout its stored text.
                        match cols.get(4).cloned() {
                            Some(Value::Text(tsql)) if trigger_uses_table(&tsql, &old) => {
                                cols[4] = Value::Text(rewrite_ident_tokens(
                                    &tsql,
                                    &old,
                                    &sql::print::ident(&new_name),
                                ));
                                true
                            }
                            _ => false,
                        }
                    } else if is_text(&cols[0], "table") {
                        // Another table whose foreign key targets the renamed table:
                        // repoint its `REFERENCES <old>` to the new name (leaving its
                        // own name and any references to other tables untouched).
                        match cols.get(4).cloned() {
                            Some(Value::Text(tsql)) => {
                                let rewritten = rewrite_fk_references(&tsql, &old, &new_name);
                                if rewritten != tsql {
                                    cols[4] = Value::Text(rewritten);
                                    true
                                } else {
                                    false
                                }
                            }
                            _ => false,
                        }
                    } else {
                        false
                    }
                })?;
            }
            AlterAction::RenameColumn { old, new, new_text } => {
                let pos = ct
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(old))
                    .ok_or_else(|| Error::Error(format!("no such column: {old}")))?;
                // Renaming onto an existing column name is rejected, like SQLite.
                if ct
                    .columns
                    .iter()
                    .enumerate()
                    .any(|(i, c)| i != pos && c.name.eq_ignore_ascii_case(new))
                {
                    return Err(Error::Error(format!("duplicate column name: {new}")));
                }
                ct.columns[pos].name = new.clone();
                // Propagate the rename into the table's own expressions and
                // column lists, which still reference the old name (otherwise the
                // CHECK / generated / default would break after the rename).
                let rename = |e: &mut Expr| rename_column_ref(e, &a.table, old, new);
                for col in &mut ct.columns {
                    for k in &mut col.constraints {
                        match k {
                            ColumnConstraint::Check(e, _) | ColumnConstraint::Default(e) => {
                                rename(e)
                            }
                            ColumnConstraint::Generated { expr, .. } => rename(expr),
                            _ => {}
                        }
                    }
                }
                for tc in &mut ct.constraints {
                    match tc {
                        TableConstraint::PrimaryKey(n, _) | TableConstraint::Unique(n, _) => {
                            for nm in n {
                                if nm.eq_ignore_ascii_case(old) {
                                    *nm = new.clone();
                                }
                            }
                        }
                        TableConstraint::Check(e, _) => rename(e),
                        TableConstraint::ForeignKey(fk) => {
                            for nm in &mut fk.columns {
                                if nm.eq_ignore_ascii_case(old) {
                                    *nm = new.clone();
                                }
                            }
                        }
                    }
                }
                // The AST reprint is only a fallback; normally we edit the stored
                // text in place so the column's formatting is preserved like sqlite.
                let reprint = sql::print::create_table(&ct);
                let table = a.table.clone();
                let old = old.clone();
                let new_text = new_text.clone();
                // Snapshot every base table's column names, so a multi-source view
                // rewrite (A-rn3) can tell whether the renamed column name is
                // unique across a join's sources.
                let table_cols: alloc::collections::BTreeMap<String, Vec<String>> = self
                    .schema
                    .objects()
                    .iter()
                    .filter(|o| o.obj_type == crate::schema::ObjectType::Table)
                    .filter_map(|o| {
                        self.table_meta(&o.name, None).ok().map(|m| {
                            (
                                o.name.clone(),
                                m.columns.iter().map(|c| c.name.clone()).collect(),
                            )
                        })
                    })
                    .collect();
                self.rewrite_schema_rows(|cols| {
                    if is_text(&cols[0], "table") && is_text(&cols[1], &table) {
                        cols[4] = Value::Text(match cols.get(4) {
                            Some(Value::Text(s)) => rewrite_ident_tokens(s, &old, &new_text),
                            _ => reprint.clone(),
                        });
                        true
                    } else if is_text(&cols[0], "index") && is_text(&cols[2], &table) {
                        // Rewrite an index over this table if it names the column.
                        if let Some(Value::Text(isql)) = cols.get(4).cloned() {
                            let rewritten = rewrite_ident_tokens(&isql, &old, &new_text);
                            if rewritten != isql {
                                cols[4] = Value::Text(rewritten);
                                return true;
                            }
                        }
                        false
                    } else if is_text(&cols[0], "table") {
                        // Another table whose foreign key references the renamed
                        // parent column: rewrite `REFERENCES <table>(old)` only.
                        if let Some(Value::Text(csql)) = cols.get(4).cloned() {
                            let rewritten =
                                rewrite_fk_parent_column(&csql, &table, &old, &new_text);
                            if rewritten != csql {
                                cols[4] = Value::Text(rewritten);
                                return true;
                            }
                        }
                        false
                    } else if is_text(&cols[0], "view") {
                        // A single-source view (only the renamed table) rewrites
                        // every reference (bare + qualified). A multi-source view
                        // (a join of base tables) rewrites `<renamed-table>.old`
                        // always, and a bare `old` only when that name is unique
                        // across the sources (A-rn3). Views with subqueries/CTEs/
                        // non-base sources are still left untouched.
                        match cols.get(4).cloned() {
                            Some(Value::Text(vsql)) => {
                                let rewritten = if let Some(quals) =
                                    view_single_source_column_quals(&vsql, &table, &old)
                                {
                                    rewrite_column_tokens(&vsql, &quals, &old, &new_text, true)
                                } else if let Some((quals, bare)) =
                                    view_multi_source_quals(&vsql, &table, &old, &table_cols)
                                {
                                    rewrite_column_tokens(&vsql, &quals, &old, &new_text, bare)
                                } else {
                                    vsql.clone()
                                };
                                if rewritten != vsql {
                                    cols[4] = Value::Text(rewritten);
                                    return true;
                                }
                                false
                            }
                            _ => false,
                        }
                    } else if is_text(&cols[0], "trigger") {
                        // A trigger ON the renamed table whose body references ONLY
                        // that table: NEW/OLD and bare/qualified column refs all
                        // resolve to it, so a full token rewrite is safe and
                        // complete. When the body also touches other tables, the
                        // bare refs are ambiguous, but `NEW.old`/`OLD.old` still
                        // bind to the renamed table, so rewrite just those. (The
                        // remaining multi-source bare/`UPDATE OF` refs are the
                        // A-rn3 remainder.)
                        match cols.get(4).cloned() {
                            Some(Value::Text(tsql)) => {
                                let rewritten = if let Some(quals) =
                                    trigger_single_source_quals(&tsql, &table, &old)
                                {
                                    rewrite_column_tokens(&tsql, &quals, &old, &new_text, true)
                                } else if trigger_on_renamed_table(&tsql, &table, &old) {
                                    rewrite_column_tokens(
                                        &tsql,
                                        &[String::from("NEW"), String::from("OLD")],
                                        &old,
                                        &new_text,
                                        false,
                                    )
                                } else if trigger_body_single_source_over(&tsql, &table, &old) {
                                    // A trigger on ANOTHER table whose body reads/
                                    // writes only the renamed table: every bare and
                                    // `<table>.`-qualified ref binds to it (its own
                                    // NEW/OLD belong to a different table and are
                                    // left alone, as `<table>` is the only qual).
                                    rewrite_column_tokens(
                                        &tsql,
                                        core::slice::from_ref(&table),
                                        &old,
                                        &new_text,
                                        true,
                                    )
                                } else {
                                    tsql.clone()
                                };
                                if rewritten != tsql {
                                    cols[4] = Value::Text(rewritten);
                                    return true;
                                }
                                false
                            }
                            _ => false,
                        }
                    } else {
                        false
                    }
                })?;
            }
        }

        let cookie = self
            .backend
            .writer()?
            .header()
            .schema_cookie
            .wrapping_add(1);
        self.backend.writer()?.header_mut().schema_cookie = cookie;
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    /// `ALTER TABLE … RENAME TO` for a virtual table: rename its persistent
    /// `<name>_data` backing table (a normal table) and rewrite its own schema row
    /// (name, tbl_name, and the stored `CREATE VIRTUAL TABLE` text), matching
    /// sqlite, which renames a vtab and its shadow tables.
    fn rename_virtual_table(&mut self, old: &str, new: &str) -> Result<()> {
        if self
            .schema
            .objects()
            .iter()
            .any(|o| o.name.eq_ignore_ascii_case(new))
        {
            return Err(Error::Error(format!(
                "there is already another table or index with this name: {new}"
            )));
        }
        // Rename the persistent shadow tables first (ordinary tables): the
        // generic `<name>_data`, or an R-Tree's `_node`/`_rowid`/`_parent`.
        for suffix in [
            "_data", "_node", "_rowid", "_parent", "_content", "_docsize", "_config", "_idx",
        ] {
            let backing_old = format!("{old}{suffix}");
            if self.schema.table(&backing_old).is_some() {
                self.exec_alter(&Alter {
                    schema: None,
                    table: backing_old,
                    action: AlterAction::RenameTable(format!("{new}{suffix}")),
                })?;
            }
        }
        let old_s = old.to_string();
        let new_s = new.to_string();
        self.rewrite_schema_rows(|cols| {
            if is_text(&cols[0], "table") && is_text(&cols[1], &old_s) {
                cols[1] = Value::Text(new_s.clone());
                cols[2] = Value::Text(new_s.clone());
                if let Some(Value::Text(s)) = cols.get(4).cloned() {
                    cols[4] =
                        Value::Text(rewrite_ident_tokens(&s, &old_s, &sql::print::ident(&new_s)));
                }
                true
            } else {
                false
            }
        })?;
        let cookie = self
            .backend
            .writer()?
            .header()
            .schema_cookie
            .wrapping_add(1);
        self.backend.writer()?.header_mut().schema_cookie = cookie;
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    /// `ALTER TABLE … DROP COLUMN name`: remove the column from the schema and
    /// rewrite every row without it, then rebuild the indexes. To stay correct,
    /// columns that participate in the structure (PRIMARY KEY, UNIQUE, an index,
    /// a foreign key, a CHECK, or generation) are refused — matching SQLite, which
    /// rejects dropping such columns.
    fn exec_drop_column(&mut self, a: &Alter, mut ct: CreateTable, name: &str) -> Result<()> {
        let pos = ct
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::Error(format!("no such column: \"{name}\"")))?;
        if ct.columns.len() <= 1 {
            return Err(Error::Error(format!(
                "cannot drop column \"{name}\": no other columns exist"
            )));
        }
        let cannot = |why: &str| {
            Err(Error::Error(format!(
                "cannot drop column \"{name}\": {why}"
            )))
        };
        // The dropped column must not be structural.
        for c in &ct.columns[pos].constraints {
            match c {
                ColumnConstraint::PrimaryKey { .. } => return cannot("PRIMARY KEY"),
                ColumnConstraint::Unique(_) => return cannot("UNIQUE"),
                ColumnConstraint::Check(..) => return cannot("CHECK"),
                ColumnConstraint::References(_) => return cannot("FOREIGN KEY"),
                ColumnConstraint::Generated { .. } => return cannot("generated"),
                _ => {}
            }
        }
        // Table-level constraints / other generated columns force a refusal too
        // (conservatively, any of these on the table that could reference it).
        for tc in &ct.constraints {
            match tc {
                TableConstraint::PrimaryKey(n, _) | TableConstraint::Unique(n, _) => {
                    if n.iter().any(|x| x.eq_ignore_ascii_case(name)) {
                        return cannot("PRIMARY KEY or UNIQUE");
                    }
                }
                TableConstraint::Check(..) => return cannot("a table CHECK constraint exists"),
                TableConstraint::ForeignKey(_) => {
                    return cannot("a table FOREIGN KEY constraint exists")
                }
            }
        }
        if ct.columns.iter().enumerate().any(|(i, c)| {
            i != pos
                && c.constraints
                    .iter()
                    .any(|x| matches!(x, ColumnConstraint::Generated { .. }))
        }) {
            return cannot("a generated column exists");
        }
        let meta = self.table_meta(&a.table, None)?;
        if meta.ipk == Some(pos) {
            return cannot("PRIMARY KEY");
        }
        let indexes = self.indexes_of(&a.table)?;
        if indexes
            .iter()
            .any(|i| i.cols.contains(&pos) || i.key_exprs.is_some() || i.partial.is_some())
        {
            return cannot("it is indexed");
        }

        // Read the rows, drop the column's value from each.
        let new_rows: Vec<(i64, Vec<Value>)> = self
            .scan_table(&meta)?
            .into_iter()
            .map(|(rid, mut vals)| {
                vals.remove(pos);
                (rid, vals)
            })
            .collect();

        // Update the schema's CREATE TABLE text.
        ct.columns.remove(pos);
        // Remove the column from the stored CREATE text in place (preserving the
        // other columns verbatim), like sqlite; fall back to an AST reprint.
        let reprint = sql::print::create_table(&ct);
        let table = a.table.clone();
        let dropped = name.to_string();
        self.rewrite_schema_rows(|cols| {
            if is_text(&cols[0], "table") && is_text(&cols[1], &table) {
                let updated = match cols.get(4) {
                    Some(Value::Text(old)) => {
                        drop_column_from_create(old, &dropped).unwrap_or_else(|| reprint.clone())
                    }
                    _ => reprint.clone(),
                };
                cols[4] = Value::Text(updated);
                true
            } else {
                false
            }
        })?;
        self.schema = Schema::read(self.backend.source())?;
        let new_meta = self.table_meta(&a.table, None)?;

        // Rewrite the table b-tree with the narrowed rows.
        clear_table(self.backend.writer()?, new_meta.root)?;
        for (rid, vals) in &new_rows {
            let mut stored = vals.clone();
            if let Some(ipk) = new_meta.ipk {
                stored[ipk] = Value::Null;
            }
            let record = encode_record(&stored);
            insert_table(self.backend.writer()?, new_meta.root, *rid, &record)?;
        }
        // Index column positions shifted; rebuild them.
        let new_indexes = self.indexes_of(&a.table)?;
        self.rebuild_indexes(&new_meta, &new_indexes)?;

        let cookie = self
            .backend
            .writer()?
            .header()
            .schema_cookie
            .wrapping_add(1);
        self.backend.writer()?.header_mut().schema_cookie = cookie;
        self.schema = Schema::read(self.backend.source())?;
        Ok(())
    }

    /// Scan `sqlite_schema`, let `f` mutate each decoded 5-column row in place,
    /// and rewrite (delete + re-insert at the same rowid) the rows it changed.
    fn rewrite_schema_rows(&mut self, mut f: impl FnMut(&mut Vec<Value>) -> bool) -> Result<()> {
        let encoding = self.backend.source().header().text_encoding;
        let mut changes: Vec<(i64, Vec<u8>)> = Vec::new();
        {
            let mut cur = TableCursor::new(self.backend.source(), crate::schema::SCHEMA_ROOT_PAGE);
            let mut ok = cur.first()?;
            while ok {
                let mut cols = decode_record(&cur.payload()?, encoding)?;
                cols.resize(5, Value::Null);
                if f(&mut cols) {
                    changes.push((cur.rowid()?, encode_record(&cols)));
                }
                ok = cur.next()?;
            }
        }
        let w = self.backend.writer()?;
        for (rid, rec) in changes {
            delete_table(w, crate::schema::SCHEMA_ROOT_PAGE, rid)?;
            insert_table(w, crate::schema::SCHEMA_ROOT_PAGE, rid, &rec)?;
        }
        Ok(())
    }

    /// Resolve the `sqlite_schema` rowids of the objects named in `names`.
    fn schema_rowids_for(&self, names: &[String]) -> Result<Vec<i64>> {
        let encoding = self.backend.source().header().text_encoding;
        let mut out = Vec::new();
        let mut cur = TableCursor::new(self.backend.source(), crate::schema::SCHEMA_ROOT_PAGE);
        let mut ok = cur.first()?;
        while ok {
            let cols = decode_record(&cur.payload()?, encoding)?;
            if let Some(Value::Text(name)) = cols.get(1) {
                if names.iter().any(|n| n == name) {
                    out.push(cur.rowid()?);
                }
            }
            ok = cur.next()?;
        }
        Ok(out)
    }

    /// Seek a `WITHOUT ROWID` table's clustered PRIMARY KEY b-tree for the rows
    /// whose leading PK columns the `WHERE` constrains by equality (`… WHERE
    /// pk = ?`), instead of scanning. The b-tree entries are the rows themselves,
    /// stored PK-first, so an equality-prefix seek yields them directly.
    /// `run_core` re-applies the full `WHERE`, so returning a superset is fine.
    /// Returns `None` (→ caller scans) when no leading-PK equality is usable.
    fn try_without_rowid_pk_seek(
        &self,
        meta: &TableMeta,
        sel: &Select,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let Some(where_expr) = &sel.where_clause else {
            return Ok(None);
        };
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        let pk = &meta.storage_order[..meta.pk_len];
        if pk.is_empty() {
            return Ok(None);
        }
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
        // Build the seek key from the longest leading-PK prefix the WHERE
        // constrains by `= const`, with the b-tree's storage collations.
        let storage_colls = wr_storage_collations(meta);
        let mut key = Vec::new();
        let mut colls = Vec::new();
        for (i, &c) in pk.iter().enumerate() {
            let Some((_, v)) = eqs.iter().find(|(col, _)| *col == c) else {
                break;
            };
            if matches!(v, Value::Null) {
                break; // PK columns are NOT NULL; `pk = NULL` matches nothing
            }
            key.push(meta.columns[c].affinity.coerce(v.clone()));
            colls.push(storage_colls[i]);
        }
        if key.is_empty() {
            return Ok(None);
        }
        let records =
            crate::btree::index_seek_records(self.backend.source(), meta.root, &key, &colls)?;
        let mut out = Vec::with_capacity(records.len());
        for storage in records {
            let mut row = unpermute_row(meta, storage);
            self.compute_generated(meta, &mut row, params)?;
            out.push(InputRow {
                values: row,
                rowid: None,
            });
        }
        Ok(Some(out))
    }

    /// Range variant of [`try_without_rowid_pk_seek`](Self::try_without_rowid_pk_seek):
    /// a `< / <= / > / >= / BETWEEN` bound on the *leading* PK column walks the
    /// clustered b-tree between bounds instead of scanning. A superset is fine
    /// (`run_core` re-applies the full `WHERE`). Returns `None` (→ scan) when the
    /// leading PK column has no range bound.
    fn try_without_rowid_pk_range(
        &self,
        meta: &TableMeta,
        sel: &Select,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let Some(where_expr) = &sel.where_clause else {
            return Ok(None);
        };
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        let pk = &meta.storage_order[..meta.pk_len];
        let Some(&lead) = pk.first() else {
            return Ok(None);
        };
        let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
            alloc::collections::BTreeMap::new();
        collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
        let Some(b) = ranges.get(&lead) else {
            return Ok(None);
        };
        let aff = meta.columns[lead].affinity;
        let coll = wr_storage_collations(meta)[0];
        let lower = b.lower.as_ref().map(|(v, i)| (aff.coerce(v.clone()), *i));
        let upper = b.upper.as_ref().map(|(v, i)| (aff.coerce(v.clone()), *i));
        let colls = [coll];
        let lower_arg = lower
            .as_ref()
            .map(|(v, inc)| (core::slice::from_ref(v), *inc));
        let upper_arg = upper
            .as_ref()
            .map(|(v, inc)| (core::slice::from_ref(v), *inc));
        let records = crate::btree::index_range_records(
            self.backend.source(),
            meta.root,
            lower_arg,
            upper_arg,
            &colls,
        )?;
        let mut out = Vec::with_capacity(records.len());
        for storage in records {
            let mut row = unpermute_row(meta, storage);
            self.compute_generated(meta, &mut row, params)?;
            out.push(InputRow {
                values: row,
                rowid: None,
            });
        }
        Ok(Some(out))
    }

    /// Seek a *secondary* index of a WITHOUT ROWID table on an equality of its
    /// leading column(s). A WITHOUT ROWID index record is `(indexed cols…, PK
    /// cols…)`, so when the index plus the PK covers every referenced column the
    /// row is read straight from the index record; otherwise the PK columns from
    /// each record seek the clustered b-tree for the full row. `run_core`
    /// re-applies the full WHERE, so a superset is fine.
    fn try_without_rowid_index_seek(
        &self,
        meta: &TableMeta,
        table_name: &str,
        sel: &Select,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let Some(where_expr) = &sel.where_clause else {
            return Ok(None);
        };
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
        eqs.retain(|(_, v)| !matches!(v, Value::Null));
        if eqs.is_empty() {
            return Ok(None);
        }
        let pk: Vec<usize> = meta.storage_order[..meta.pk_len].to_vec();
        let indexes = self.indexes_of(table_name)?;
        if let Some(IndexHint::IndexedBy(n)) = hint {
            if !indexes.iter().any(|i| i.name.eq_ignore_ascii_case(n)) {
                return Err(Error::Error(alloc::format!("no such index: {n}")));
            }
        }
        let src = self.backend.source();
        for idx in &indexes {
            if let Some(IndexHint::IndexedBy(n)) = hint {
                if !idx.name.eq_ignore_ascii_case(n) {
                    continue;
                }
            }
            if idx.partial.is_some() || idx.key_exprs.is_some() {
                continue;
            }
            // Equality prefix over the index's leading columns.
            let mut key = Vec::new();
            let mut colls = Vec::new();
            for (i, &c) in idx.cols.iter().enumerate() {
                let Some((_, v)) = eqs.iter().find(|(col, _)| *col == c) else {
                    break;
                };
                key.push(meta.columns[c].affinity.coerce(v.clone()));
                colls.push(idx.collations.get(i).copied().unwrap_or_default());
            }
            if key.is_empty() {
                continue;
            }
            let records = crate::btree::index_seek_records(src, idx.root, &key, &colls)?;
            let covering = self.wr_index_covers(idx, &pk, meta, sel, where_expr);
            return Ok(Some(
                self.wr_index_rows(meta, idx, &pk, records, covering, params)?,
            ));
        }
        Ok(None)
    }

    /// Whether a WITHOUT ROWID secondary index covers the query (so its rows can
    /// be read straight from the index records). A *named* index counts as holding
    /// its columns plus the trailing PK columns; an implicit UNIQUE/PK autoindex
    /// (`sqlite_autoindex_*`) counts only its own — matching SQLite's `COVERING
    /// INDEX` vs `INDEX` wording.
    fn wr_index_covers(
        &self,
        idx: &IndexMeta,
        pk: &[usize],
        meta: &TableMeta,
        sel: &Select,
        where_expr: &Expr,
    ) -> bool {
        let mut avail = idx.cols.clone();
        if !idx.name.starts_with("sqlite_autoindex_") {
            for &p in pk {
                if !avail.contains(&p) {
                    avail.push(p);
                }
            }
        }
        self.seek_index_covers(sel, meta, &avail, where_expr)
    }

    /// Build rows from a WITHOUT ROWID secondary index's seeked/scanned records.
    /// Each record is `(indexed cols…, PK cols…)`: when `covering`, reconstruct
    /// the referenced columns straight from it (the rest are unreferenced, left
    /// NULL); otherwise the trailing PK columns seek the clustered b-tree for the
    /// full row.
    fn wr_index_rows(
        &self,
        meta: &TableMeta,
        idx: &IndexMeta,
        pk: &[usize],
        records: Vec<Vec<Value>>,
        covering: bool,
        params: &Params,
    ) -> Result<Vec<InputRow>> {
        let mut out = Vec::with_capacity(records.len());
        if covering {
            for rec in &records {
                let mut values = alloc::vec![Value::Null; meta.columns.len()];
                for (i, &mc) in idx.cols.iter().enumerate() {
                    values[mc] = rec[i].clone();
                }
                for (j, &pc) in pk.iter().enumerate() {
                    values[pc] = rec[idx.cols.len() + j].clone();
                }
                out.push(InputRow {
                    values,
                    rowid: None,
                });
            }
        } else {
            let src = self.backend.source();
            let pk_colls: Vec<crate::value::Collation> =
                wr_storage_collations(meta)[..pk.len()].to_vec();
            for rec in &records {
                let pk_key: Vec<Value> = (0..pk.len())
                    .map(|j| rec[idx.cols.len() + j].clone())
                    .collect();
                for storage in crate::btree::index_seek_records(src, meta.root, &pk_key, &pk_colls)?
                {
                    let mut row = unpermute_row(meta, storage);
                    self.compute_generated(meta, &mut row, params)?;
                    out.push(InputRow {
                        values: row,
                        rowid: None,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Range variant of [`try_without_rowid_index_seek`](Self::try_without_rowid_index_seek):
    /// a bound on the *leading* column of a WITHOUT ROWID secondary index walks
    /// the index between bounds (covering or PK-fetching, as above).
    fn try_without_rowid_index_range(
        &self,
        meta: &TableMeta,
        table_name: &str,
        sel: &Select,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let Some(where_expr) = &sel.where_clause else {
            return Ok(None);
        };
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
            alloc::collections::BTreeMap::new();
        collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
        if ranges.is_empty() {
            return Ok(None);
        }
        let pk: Vec<usize> = meta.storage_order[..meta.pk_len].to_vec();
        let indexes = self.indexes_of(table_name)?;
        if let Some(IndexHint::IndexedBy(n)) = hint {
            if !indexes.iter().any(|i| i.name.eq_ignore_ascii_case(n)) {
                return Err(Error::Error(alloc::format!("no such index: {n}")));
            }
        }
        for idx in &indexes {
            if let Some(IndexHint::IndexedBy(n)) = hint {
                if !idx.name.eq_ignore_ascii_case(n) {
                    continue;
                }
            }
            if idx.partial.is_some() || idx.key_exprs.is_some() {
                continue;
            }
            let Some(&lead) = idx.cols.first() else {
                continue;
            };
            let Some(b) = ranges.get(&lead) else {
                continue;
            };
            let aff = meta.columns[lead].affinity;
            let coll = idx.collations.first().copied().unwrap_or_default();
            let lower = b.lower.as_ref().map(|(v, i)| (aff.coerce(v.clone()), *i));
            let upper = b.upper.as_ref().map(|(v, i)| (aff.coerce(v.clone()), *i));
            let colls = [coll];
            let lower_arg = lower
                .as_ref()
                .map(|(v, inc)| (core::slice::from_ref(v), *inc));
            let upper_arg = upper
                .as_ref()
                .map(|(v, inc)| (core::slice::from_ref(v), *inc));
            let records = crate::btree::index_range_records(
                self.backend.source(),
                idx.root,
                lower_arg,
                upper_arg,
                &colls,
            )?;
            let covering = self.wr_index_covers(idx, &pk, meta, sel, where_expr);
            return Ok(Some(
                self.wr_index_rows(meta, idx, &pk, records, covering, params)?,
            ));
        }
        Ok(None)
    }

    /// The index metadata (root + indexed column positions) for `table`.
    /// Try to satisfy a single-table query with an index equality lookup instead
    /// of a full scan: pick the index whose longest leftmost column prefix is
    /// covered by `col = const` predicates in the `WHERE`, seek it, and fetch the
    /// matching rows by rowid. Returns `None` (→ full scan) if no index applies.
    fn try_index_lookup(
        &self,
        meta: &TableMeta,
        table_name: &str,
        sel: &Select,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let Some(where_expr) = &sel.where_clause else {
            return Ok(None);
        };
        // `NOT INDEXED` forbids any index for this table; `INDEXED BY name`
        // restricts to one named index (validated below).
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        // `rowid` / `_rowid_` / `oid` `= N` or `IN (list)`: seek the rowid table
        // b-tree directly — works with or without an explicit INTEGER PRIMARY KEY
        // column, and is cheaper than any secondary index. `INDEXED BY` names a
        // specific index, so it forbids this fast path. (`run_core` re-applies the
        // full WHERE, so the seeked rows are a valid superset.)
        if !matches!(hint, Some(IndexHint::IndexedBy(_))) {
            if let Some(rowids) = rowid_seek_constraint(where_expr, &meta.columns, params) {
                let encoding = self.backend.source().header().text_encoding;
                let mut cur = TableCursor::new(self.backend.source(), meta.root);
                let mut out = Vec::new();
                let mut seen: Vec<i64> = Vec::new();
                for rid in rowids {
                    if seen.contains(&rid) {
                        continue;
                    }
                    seen.push(rid);
                    if cur.seek(rid)? {
                        let values = self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                        out.push(InputRow {
                            values,
                            rowid: Some(rid),
                        });
                    }
                }
                return Ok(Some(out));
            }
        }
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
        if eqs.is_empty() || eqs.iter().any(|(_, v)| matches!(v, Value::Null)) {
            // No usable column equality (`col = NULL` is never true). A plain or
            // partial *column* index can't seek, but an *expression* index might
            // (e.g. `lower(x) = 'b'` leaves no column eq behind). Try that, then
            // let the scan handle the rest.
            return self.partial_expr_lookup(meta, table_name, sel, where_expr, params);
        }

        // Rowid (INTEGER PRIMARY KEY) equality: seek the table b-tree directly
        // by rowid. run_core re-applies the full WHERE, so returning the single
        // candidate row is a valid superset even when the literal isn't an exact
        // integer (e.g. `id = 5.5` seeks rowid 5, then gets filtered out).
        // The rowid (INTEGER PRIMARY KEY) is not a named index, so `INDEXED BY`
        // forbids this fast path.
        if !matches!(hint, Some(IndexHint::IndexedBy(_))) {
            if let Some(ipk) = meta.ipk {
                if let Some((_, v)) = eqs.iter().find(|(c, _)| *c == ipk) {
                    let rid = eval::to_i64(v);
                    let encoding = self.backend.source().header().text_encoding;
                    let mut cur = TableCursor::new(self.backend.source(), meta.root);
                    let mut out = Vec::new();
                    if cur.seek(rid)? {
                        let values = self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                        out.push(InputRow {
                            values,
                            rowid: Some(rid),
                        });
                    }
                    return Ok(Some(out));
                }
            }
        }

        // Choose the index to seek. Each candidate covers the longest leftmost
        // prefix of the index's columns that the WHERE constrains by equality.
        // With `ANALYZE` statistics we pick the most selective (fewest estimated
        // rows); absent stats we fall back to the longest matched prefix.
        let indexes = self.indexes_of(table_name)?;
        // `INDEXED BY name` must name a real index of this table.
        if let Some(IndexHint::IndexedBy(n)) = hint {
            if !indexes.iter().any(|i| i.name.eq_ignore_ascii_case(n)) {
                return Err(Error::Error(alloc::format!("no such index: {n}")));
            }
        }
        let stats = self.stat1_map();
        #[allow(clippy::type_complexity)]
        let mut best: Option<(
            u32,
            Vec<Value>,
            Vec<crate::value::Collation>,
            Vec<usize>,
            u64,
        )> = None;
        for idx in &indexes {
            // Honor `INDEXED BY`: consider only the named index.
            if let Some(IndexHint::IndexedBy(n)) = hint {
                if !idx.name.eq_ignore_ascii_case(n) {
                    continue;
                }
            }
            // A partial index covers only some rows; an expression index is keyed
            // by computed values, not columns. Neither is used for a plain
            // column-equality seek — leave those to the table scan.
            if idx.partial.is_some() || idx.key_exprs.is_some() {
                continue;
            }
            let mut key = Vec::new();
            for &c in &idx.cols {
                match eqs.iter().find(|(col, _)| *col == c) {
                    Some((_, v)) => key.push(meta.columns[c].affinity.coerce(v.clone())),
                    None => break,
                }
            }
            if key.is_empty() {
                continue;
            }
            // Estimated rows returned: the stat's avgEq at the matched prefix
            // length when available, else a sentinel that prefers longer prefixes.
            let est = stats
                .get(&idx.name)
                .and_then(|s| s.get(key.len()).copied())
                .unwrap_or(u64::MAX - key.len() as u64);
            let better = match &best {
                None => true,
                Some((_, bk, _, _, be)) => est < *be || (est == *be && key.len() > bk.len()),
            };
            if better {
                // Carry the index's full collation vector so a trailing range on
                // the column after the equality prefix can be seeked too.
                best = Some((idx.root, key, idx.collations.clone(), idx.cols.clone(), est));
            }
        }
        // Plain column indexes take priority. If none applied, try a partial or
        // expression index whose eligibility we can prove from the `WHERE`
        // structure (see `partial_expr_seek`). This keeps plain-index behavior
        // byte-identical while extending seeks to the new index kinds.
        let (root, key, full_colls, idx_cols) = match best {
            Some((root, key, colls, idx_cols, _)) => (root, key, colls, idx_cols),
            None => {
                return self.partial_expr_lookup(meta, table_name, sel, where_expr, params);
            }
        };
        if key.is_empty() {
            return Ok(None);
        }

        // Covering seek: when the chosen index holds every referenced column (the
        // result columns, the `WHERE` columns, and any `ORDER BY`), read straight
        // from the index — `eqp_access` reports `USING COVERING INDEX` for the
        // same decision. Stays in lockstep with the table-fetch path below
        // (`run_core` re-applies the full `WHERE` to the superset of index rows).
        if self.seek_index_covers(sel, meta, &idx_cols, where_expr) {
            return Ok(Some(self.covering_seek_rows(meta, root, &idx_cols)?));
        }

        // Equality prefix followed by a range on the *next* index column
        // (`x=? AND y>?`): extend the exact-prefix seek to a bounded range over
        // `[eq…, low] .. [eq…, high]`, matching SQLite (and reported the same way
        // by `eqp_access`). Falls through to the plain prefix seek otherwise.
        let next_pos = key.len();
        if let Some(&next_col) = idx_cols.get(next_pos) {
            let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
                alloc::collections::BTreeMap::new();
            collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
            if let Some(b) = ranges.get(&next_col) {
                let aff = meta.columns[next_col].affinity;
                let colls = full_colls[..=next_pos].to_vec();
                let mut lo_key = key.clone();
                let lo_inc = match b.lower.as_ref() {
                    Some((v, inc)) => {
                        lo_key.push(aff.coerce(v.clone()));
                        *inc
                    }
                    None => true,
                };
                let mut hi_key = key.clone();
                let hi_inc = match b.upper.as_ref() {
                    Some((v, inc)) => {
                        hi_key.push(aff.coerce(v.clone()));
                        *inc
                    }
                    None => true,
                };
                let rowids = crate::btree::index_range_rowids(
                    self.backend.source(),
                    root,
                    Some((lo_key.as_slice(), lo_inc)),
                    Some((hi_key.as_slice(), hi_inc)),
                    &colls,
                )?;
                let encoding = self.backend.source().header().text_encoding;
                let mut cur = TableCursor::new(self.backend.source(), meta.root);
                let mut out = Vec::new();
                for rid in rowids {
                    if cur.seek(rid)? {
                        let values = self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                        out.push(InputRow {
                            values,
                            rowid: Some(rid),
                        });
                    }
                }
                return Ok(Some(out));
            }
        }

        self.index_seek_fetch(meta, root, &key, &full_colls[..key.len()])
    }

    /// Fetch table rows for an equality index seek: collect the matching rowids
    /// from the index, then read each row from the table b-tree. Returns a
    /// superset (`run_core` re-applies the full `WHERE`).
    fn index_seek_fetch(
        &self,
        meta: &TableMeta,
        root: u32,
        key: &[Value],
        colls: &[crate::value::Collation],
    ) -> Result<Option<Vec<InputRow>>> {
        let rowids = crate::btree::index_seek_rowids(self.backend.source(), root, key, colls)?;
        let encoding = self.backend.source().header().text_encoding;
        let mut cur = TableCursor::new(self.backend.source(), meta.root);
        let mut out = Vec::new();
        for rid in rowids {
            if cur.seek(rid)? {
                let values = self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                out.push(InputRow {
                    values,
                    rowid: Some(rid),
                });
            }
        }
        Ok(Some(out))
    }

    /// Equality-seek fallback for partial / expression indexes, used when no
    /// plain column index applied. Picks the first index (honoring `INDEXED BY`)
    /// for which [`partial_expr_seek`](Self::partial_expr_seek) proves a seek is
    /// valid, fetches its rows, and returns the superset. Returns `None` (→ scan)
    /// when none qualifies. `eqp_access` mirrors this exact choice.
    fn partial_expr_lookup(
        &self,
        meta: &TableMeta,
        table_name: &str,
        sel: &Select,
        where_expr: &Expr,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        for idx in self.indexes_of(table_name)? {
            if let Some(IndexHint::IndexedBy(n)) = hint {
                if !idx.name.eq_ignore_ascii_case(n) {
                    continue;
                }
            }
            if let Some((key, colls)) = self.partial_expr_seek(&idx, where_expr, meta, params)? {
                // Expression indexes don't map keys back to table columns, so they
                // are never a covering seek here; fetch table rows by rowid (a
                // superset re-filtered by `run_core`).
                return self.index_seek_fetch(meta, idx.root, &key, &colls);
            }
        }
        Ok(None)
    }

    /// Decide whether a *partial* or *expression* index can serve an equality
    /// seek for `where_expr`, and if so return the seek `(key, collations)`.
    ///
    /// The rules are deliberately conservative (no general implication):
    ///
    /// * **Partial index** (`CREATE INDEX … WHERE pred`): usable only when `pred`
    ///   appears verbatim (modulo redundant parens) as a top-level `AND` conjunct
    ///   of the query's `WHERE`, so every row the seek can return is one the index
    ///   actually stores. A partial index over plain columns then seeks like an
    ///   ordinary column index; a partial *expression* index must additionally
    ///   satisfy the expression rule below.
    /// * **Expression index** (`CREATE INDEX … (expr)`): usable when a top-level
    ///   `AND` conjunct is `<indexed-expr> = <const>` (either operand order), with
    ///   `<indexed-expr>` structurally equal to the index's single key expression.
    ///   The seek key is the evaluated constant; the index stores that same value
    ///   per row, so the seek finds a superset.
    ///
    /// Returns `None` for plain column indexes (handled by the caller's main
    /// loop) and whenever the proof above fails. `eqp_access` calls this same
    /// helper, keeping the plan string in lockstep with what executes.
    fn partial_expr_seek(
        &self,
        idx: &IndexMeta,
        where_expr: &Expr,
        meta: &TableMeta,
        params: &Params,
    ) -> Result<Option<(Vec<Value>, Vec<crate::value::Collation>)>> {
        // Plain column index: not our concern.
        if idx.partial.is_none() && idx.key_exprs.is_none() {
            return Ok(None);
        }
        let mut conjuncts = Vec::new();
        and_conjuncts(where_expr, &mut conjuncts);

        // A partial predicate must be guaranteed by a top-level conjunct.
        if let Some(pred) = &idx.partial {
            if !conjuncts.iter().any(|c| expr_eq_modulo_parens(c, pred)) {
                return Ok(None);
            }
        }

        match &idx.key_exprs {
            // Partial index over plain columns: seek as an ordinary column index.
            None => {
                let mut key = Vec::new();
                let mut colls = Vec::new();
                let mut eqs: Vec<(usize, Value)> = Vec::new();
                collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
                for (pos, &c) in idx.cols.iter().enumerate() {
                    match eqs
                        .iter()
                        .find(|(col, v)| *col == c && !matches!(v, Value::Null))
                    {
                        Some((_, v)) => {
                            key.push(meta.columns[c].affinity.coerce(v.clone()));
                            colls.push(idx.collations[pos]);
                        }
                        None => break,
                    }
                }
                if key.is_empty() {
                    return Ok(None);
                }
                Ok(Some((key, colls)))
            }
            // Expression index: match a conjunct `<key_expr> = <const>`. Only a
            // single-term key is supported (the common `lower(x)` shape).
            Some(exprs) => {
                let [key_expr] = exprs.as_slice() else {
                    return Ok(None);
                };
                for c in &conjuncts {
                    let Expr::Binary {
                        op: BinaryOp::Eq,
                        left,
                        right,
                    } = unparen(c)
                    else {
                        continue;
                    };
                    // `<key_expr> = <const>` or `<const> = <key_expr>`.
                    let val = if expr_eq_modulo_parens(left, key_expr) {
                        const_value(right, params)
                    } else if expr_eq_modulo_parens(right, key_expr) {
                        const_value(left, params)
                    } else {
                        None
                    };
                    if let Some(v) = val {
                        if matches!(v, Value::Null) {
                            continue; // `expr = NULL` is never true
                        }
                        let coll = idx.collations.first().copied().unwrap_or_default();
                        return Ok(Some((alloc::vec![v], alloc::vec![coll])));
                    }
                }
                Ok(None)
            }
        }
    }

    /// Try to satisfy a single-table query with an index *range* scan: pick an
    /// index whose leading column is constrained by a `<`/`<=`/`>`/`>=`/`BETWEEN`
    /// predicate, walk the index between those bounds, and fetch the rows by
    /// rowid. Like [`try_index_lookup`](Self::try_index_lookup) this returns a
    /// superset — `run_core` re-applies the full `WHERE`. Returns `None` (→ scan)
    /// when no index applies.
    fn try_index_range(
        &self,
        meta: &TableMeta,
        table_name: &str,
        sel: &Select,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let Some(where_expr) = &sel.where_clause else {
            return Ok(None);
        };
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
            alloc::collections::BTreeMap::new();
        collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
        if ranges.is_empty() {
            return Ok(None);
        }

        // Rowid (INTEGER PRIMARY KEY) range: walk the table b-tree between integer
        // bounds. `INDEXED BY` forbids this (the rowid is not a named index). Only
        // integer bounds are taken (a non-integer literal falls to the scan); the
        // returned span is a superset, so the boundary rows are filtered by the
        // re-applied WHERE.
        if !matches!(hint, Some(IndexHint::IndexedBy(_))) {
            if let Some(ipk) = meta.ipk {
                if let Some(b) = ranges.get(&ipk) {
                    let int_bound = |o: &Option<(Value, bool)>| match o {
                        Some((Value::Integer(i), _)) => Some(*i),
                        None => None,
                        _ => Some(i64::MAX), // sentinel: a non-integer bound disables it
                    };
                    let lo = int_bound(&b.lower);
                    let hi = int_bound(&b.upper);
                    // Disable when a present bound is non-integer (sentinel hit on
                    // the wrong side).
                    let lo_ok =
                        b.lower.is_none() || matches!(b.lower, Some((Value::Integer(_), _)));
                    let hi_ok =
                        b.upper.is_none() || matches!(b.upper, Some((Value::Integer(_), _)));
                    if lo_ok && hi_ok {
                        let start = lo.unwrap_or(i64::MIN);
                        let stop = hi.unwrap_or(i64::MAX);
                        let encoding = self.backend.source().header().text_encoding;
                        let mut cur = TableCursor::new(self.backend.source(), meta.root);
                        let mut out = Vec::new();
                        let mut ok = if start == i64::MIN {
                            cur.first()?
                        } else {
                            cur.seek(start)?;
                            cur.is_valid()
                        };
                        while ok {
                            let rid = cur.rowid()?;
                            if rid > stop {
                                break;
                            }
                            let values =
                                self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                            out.push(InputRow {
                                values,
                                rowid: Some(rid),
                            });
                            ok = cur.next()?;
                        }
                        return Ok(Some(out));
                    }
                }
            }
        }

        // Pick the first plain (non-partial, non-expression) index whose leading
        // column has a range bound, honoring `INDEXED BY`.
        let indexes = self.indexes_of(table_name)?;
        if let Some(IndexHint::IndexedBy(n)) = hint {
            if !indexes.iter().any(|i| i.name.eq_ignore_ascii_case(n)) {
                return Err(Error::Error(alloc::format!("no such index: {n}")));
            }
        }
        let mut chosen: Option<(u32, RangeBound, crate::value::Collation, Vec<usize>)> = None;
        for idx in &indexes {
            if let Some(IndexHint::IndexedBy(n)) = hint {
                if !idx.name.eq_ignore_ascii_case(n) {
                    continue;
                }
            }
            if idx.partial.is_some() || idx.key_exprs.is_some() {
                continue;
            }
            let Some(&lead) = idx.cols.first() else {
                continue;
            };
            if let Some(b) = ranges.get(&lead) {
                let coll = idx.collations.first().copied().unwrap_or_default();
                let aff = meta.columns[lead].affinity;
                let b = RangeBound {
                    lower: b.lower.as_ref().map(|(v, i)| (aff.coerce(v.clone()), *i)),
                    upper: b.upper.as_ref().map(|(v, i)| (aff.coerce(v.clone()), *i)),
                };
                chosen = Some((idx.root, b, coll, idx.cols.clone()));
                break;
            }
        }
        match chosen {
            Some((root, bound, coll, idx_cols)) => {
                // Covering range seek: read from the index when it holds every
                // referenced column (lockstep with `eqp_access`'s `COVERING INDEX`).
                if self.seek_index_covers(sel, meta, &idx_cols, where_expr) {
                    return Ok(Some(self.covering_seek_rows(meta, root, &idx_cols)?));
                }
                Ok(Some(self.range_seek_fetch(meta, root, &bound, coll)?))
            }
            None => {
                // A3b: a partial or expression index whose key column / expression
                // has a range bound (and, for a partial index, whose predicate the
                // WHERE guarantees). Always a non-covering fetch — `eqp_access`
                // mirrors this in its partial/expression range fallback.
                for idx in &indexes {
                    if let Some(IndexHint::IndexedBy(n)) = hint {
                        if !idx.name.eq_ignore_ascii_case(n) {
                            continue;
                        }
                    }
                    if let Some((bound, coll)) =
                        self.partial_expr_range(idx, where_expr, meta, params)
                    {
                        return Ok(Some(self.range_seek_fetch(meta, idx.root, &bound, coll)?));
                    }
                }
                Ok(None)
            }
        }
    }

    /// Walk an index between `bound`'s lower/upper keys (single leading column,
    /// under `coll`) and fetch each matching row from the table by rowid. Returns
    /// a superset — `run_core` re-applies the full `WHERE`.
    fn range_seek_fetch(
        &self,
        meta: &TableMeta,
        root: u32,
        bound: &RangeBound,
        coll: crate::value::Collation,
    ) -> Result<Vec<InputRow>> {
        let colls = [coll];
        let lower_key = bound.lower.as_ref().map(|(v, _)| core::slice::from_ref(v));
        let upper_key = bound.upper.as_ref().map(|(v, _)| core::slice::from_ref(v));
        let lower = lower_key.map(|k| (k, bound.lower.as_ref().unwrap().1));
        let upper = upper_key.map(|k| (k, bound.upper.as_ref().unwrap().1));
        let rowids =
            crate::btree::index_range_rowids(self.backend.source(), root, lower, upper, &colls)?;

        let encoding = self.backend.source().header().text_encoding;
        let mut cur = TableCursor::new(self.backend.source(), meta.root);
        let mut out = Vec::new();
        for rid in rowids {
            if cur.seek(rid)? {
                let values = self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                out.push(InputRow {
                    values,
                    rowid: Some(rid),
                });
            }
        }
        Ok(out)
    }

    /// Seek each key through an index (single leading column/expression, under
    /// `colls`), union the matching rowids, and fetch each row from the table.
    /// Shared by the plain, partial, and expression `IN`-list seek paths. Returns
    /// a superset (`run_core` re-applies the full `WHERE`).
    fn in_seek_fetch(
        &self,
        meta: &TableMeta,
        root: u32,
        colls: &[crate::value::Collation],
        keys: &[Vec<Value>],
    ) -> Result<Vec<InputRow>> {
        let src = self.backend.source();
        let encoding = src.header().text_encoding;
        let mut rowids: Vec<i64> = Vec::new();
        for key in keys {
            for rid in crate::btree::index_seek_rowids(src, root, key, colls)? {
                if !rowids.contains(&rid) {
                    rowids.push(rid);
                }
            }
        }
        let mut cur = TableCursor::new(src, meta.root);
        let mut out = Vec::new();
        for rid in rowids {
            if cur.seek(rid)? {
                let values = self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                out.push(InputRow {
                    values,
                    rowid: Some(rid),
                });
            }
        }
        Ok(out)
    }

    /// A3b range analogue of [`partial_expr_seek`](Self::partial_expr_seek): for a
    /// partial or expression index, return the range bound (and collation) to seek
    /// — a `<`/`<=`/`>`/`>=` constraint on the partial index's leading column (with
    /// its predicate guaranteed by the `WHERE`), or on an expression index's keyed
    /// expression. `None` when the index doesn't apply.
    fn partial_expr_range(
        &self,
        idx: &IndexMeta,
        where_expr: &Expr,
        meta: &TableMeta,
        params: &Params,
    ) -> Option<(RangeBound, crate::value::Collation)> {
        if idx.partial.is_none() && idx.key_exprs.is_none() {
            return None;
        }
        let mut conjuncts = Vec::new();
        and_conjuncts(where_expr, &mut conjuncts);
        if let Some(pred) = &idx.partial {
            if !conjuncts.iter().any(|c| expr_eq_modulo_parens(c, pred)) {
                return None;
            }
        }
        let coll = idx.collations.first().copied().unwrap_or_default();
        match &idx.key_exprs {
            // Partial index over plain columns: a range on the leading column.
            None => {
                let lead = *idx.cols.first()?;
                let mut ranges = alloc::collections::BTreeMap::new();
                collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
                let b = ranges.get(&lead)?;
                let aff = meta.columns[lead].affinity;
                Some((
                    RangeBound {
                        lower: b.lower.as_ref().map(|(v, i)| (aff.coerce(v.clone()), *i)),
                        upper: b.upper.as_ref().map(|(v, i)| (aff.coerce(v.clone()), *i)),
                    },
                    coll,
                ))
            }
            // Expression index: collect range conjuncts `<key_expr> <op> <const>`.
            Some(exprs) => {
                let [key_expr] = exprs.as_slice() else {
                    return None;
                };
                let mut bound = RangeBound {
                    lower: None,
                    upper: None,
                };
                for c in &conjuncts {
                    let Expr::Binary { op, left, right } = unparen(c) else {
                        continue;
                    };
                    // Normalize to `key_expr <op> const`, mirroring the operator
                    // when the expression is on the right.
                    let (val, op) = if expr_eq_modulo_parens(left, key_expr) {
                        (const_value(right, params), *op)
                    } else if expr_eq_modulo_parens(right, key_expr) {
                        (const_value(left, params), mirror_comparison(*op))
                    } else {
                        continue;
                    };
                    let Some(v) = val else { continue };
                    if matches!(v, Value::Null) {
                        continue;
                    }
                    match op {
                        BinaryOp::Gt => bound.lower = Some((v, false)),
                        BinaryOp::GtEq => bound.lower = Some((v, true)),
                        BinaryOp::Lt => bound.upper = Some((v, false)),
                        BinaryOp::LtEq => bound.upper = Some((v, true)),
                        _ => {}
                    }
                }
                if bound.lower.is_none() && bound.upper.is_none() {
                    return None;
                }
                Some((bound, coll))
            }
        }
    }

    /// Try to satisfy a single-table query with per-value index seeks for a
    /// `column IN (const, …)` predicate: seek each list value through an index on
    /// that column (or the rowid b-tree for an `INTEGER PRIMARY KEY`), union the
    /// rowids, and fetch the rows. Returns a superset (`run_core` re-applies the
    /// full `WHERE`), or `None` (→ scan) when no index applies.
    fn try_index_in(
        &self,
        meta: &TableMeta,
        table_name: &str,
        sel: &Select,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let Some(where_expr) = &sel.where_clause else {
            return Ok(None);
        };
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        let indexes = self.indexes_of(table_name)?;
        if let Some(IndexHint::IndexedBy(n)) = hint {
            if !indexes.iter().any(|i| i.name.eq_ignore_ascii_case(n)) {
                return Err(Error::Error(alloc::format!("no such index: {n}")));
            }
        }
        let by_name = |idx: &IndexMeta| match hint {
            Some(IndexHint::IndexedBy(n)) => idx.name.eq_ignore_ascii_case(n),
            _ => true,
        };

        // Column `IN (…)`: rowid b-tree, a plain index, or a partial index whose
        // leading column is the IN column (and whose predicate the WHERE proves).
        if let Some((col, values)) = find_in_constraint(where_expr, &meta.columns, params) {
            // `x IN (NULL)` is never true/usable as a seek key.
            if !values.iter().any(|v| matches!(v, Value::Null)) {
                let encoding = self.backend.source().header().text_encoding;
                let aff = meta.columns[col].affinity;

                // Rowid IN-list: seek the table b-tree directly for each value.
                if !matches!(hint, Some(IndexHint::IndexedBy(_))) {
                    if let Some(ipk) = meta.ipk {
                        if col == ipk {
                            let mut cur = TableCursor::new(self.backend.source(), meta.root);
                            let mut out = Vec::new();
                            let mut seen: Vec<i64> = Vec::new();
                            for v in &values {
                                let rid = eval::to_i64(v);
                                if seen.contains(&rid) {
                                    continue;
                                }
                                seen.push(rid);
                                if cur.seek(rid)? {
                                    let values =
                                        self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                                    out.push(InputRow {
                                        values,
                                        rowid: Some(rid),
                                    });
                                }
                            }
                            return Ok(Some(out));
                        }
                    }
                }

                let keys: Vec<Vec<Value>> = values
                    .iter()
                    .map(|v| alloc::vec![aff.coerce(v.clone())])
                    .collect();
                // A plain index whose leading column is the IN column.
                for idx in &indexes {
                    if !by_name(idx) || idx.partial.is_some() || idx.key_exprs.is_some() {
                        continue;
                    }
                    if idx.cols.first() == Some(&col) {
                        if self.seek_index_covers(sel, meta, &idx.cols, where_expr) {
                            return Ok(Some(self.covering_seek_rows(meta, idx.root, &idx.cols)?));
                        }
                        let coll = idx.collations.first().copied().unwrap_or_default();
                        return Ok(Some(self.in_seek_fetch(meta, idx.root, &[coll], &keys)?));
                    }
                }
                // A3b: a partial index on the IN column with its predicate proven.
                for idx in &indexes {
                    if !by_name(idx) || idx.key_exprs.is_some() || idx.partial.is_none() {
                        continue;
                    }
                    if idx.cols.first() == Some(&col) && partial_pred_guaranteed(idx, where_expr) {
                        let coll = idx.collations.first().copied().unwrap_or_default();
                        return Ok(Some(self.in_seek_fetch(meta, idx.root, &[coll], &keys)?));
                    }
                }
            }
        }

        // A3b: an expression index keyed by `<expr>` with `<expr> IN (…)`.
        for idx in &indexes {
            if !by_name(idx) {
                continue;
            }
            let Some(exprs) = &idx.key_exprs else {
                continue;
            };
            let [key_expr] = exprs.as_slice() else {
                continue;
            };
            if !partial_pred_guaranteed(idx, where_expr) {
                continue;
            }
            let Some(values) = find_expr_in_values(key_expr, where_expr, params) else {
                continue;
            };
            if values.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            let coll = idx.collations.first().copied().unwrap_or_default();
            let keys: Vec<Vec<Value>> = values.iter().map(|v| alloc::vec![v.clone()]).collect();
            return Ok(Some(self.in_seek_fetch(meta, idx.root, &[coll], &keys)?));
        }

        Ok(None)
    }

    /// Find a plain (non-partial, non-expression) index whose leading column is
    /// `col`, returning its root page and leading collation. Honors `INDEXED BY`.
    fn leading_index_for(
        &self,
        table_name: &str,
        col: usize,
        hint: Option<&IndexHint>,
    ) -> Result<Option<(u32, crate::value::Collation)>> {
        for idx in &self.indexes_of(table_name)? {
            if let Some(IndexHint::IndexedBy(n)) = hint {
                if !idx.name.eq_ignore_ascii_case(n) {
                    continue;
                }
            }
            if idx.partial.is_some() || idx.key_exprs.is_some() {
                continue;
            }
            if idx.cols.first() == Some(&col) {
                return Ok(Some((
                    idx.root,
                    idx.collations.first().copied().unwrap_or_default(),
                )));
            }
        }
        Ok(None)
    }

    /// Rowids matching `col IN values` (or `col = v` with a one-element slice) via
    /// the rowid b-tree or an index, or `None` when neither applies.
    fn seek_col_values(
        &self,
        meta: &TableMeta,
        table_name: &str,
        hint: Option<&IndexHint>,
        col: usize,
        values: &[Value],
    ) -> Result<Option<Vec<i64>>> {
        let mut rowids: Vec<i64> = Vec::new();
        // Rowid column: each value is itself a candidate rowid.
        if !matches!(hint, Some(IndexHint::IndexedBy(_))) && meta.ipk == Some(col) {
            for v in values {
                let rid = eval::to_i64(v);
                if !rowids.contains(&rid) {
                    rowids.push(rid);
                }
            }
            return Ok(Some(rowids));
        }
        let Some((root, coll)) = self.leading_index_for(table_name, col, hint)? else {
            return Ok(None);
        };
        let aff = meta.columns[col].affinity;
        let colls = [coll];
        for v in values {
            let key = [aff.coerce(v.clone())];
            for rid in crate::btree::index_seek_rowids(self.backend.source(), root, &key, &colls)? {
                if !rowids.contains(&rid) {
                    rowids.push(rid);
                }
            }
        }
        Ok(Some(rowids))
    }

    /// Rowids matching a range `bound` on `col` via the rowid b-tree (integer
    /// bounds) or an index, or `None` when neither applies.
    fn seek_col_range(
        &self,
        meta: &TableMeta,
        table_name: &str,
        hint: Option<&IndexHint>,
        col: usize,
        bound: &RangeBound,
    ) -> Result<Option<Vec<i64>>> {
        // Rowid integer range: walk the table b-tree between bounds.
        if !matches!(hint, Some(IndexHint::IndexedBy(_))) && meta.ipk == Some(col) {
            let lo_int =
                bound.lower.is_none() || matches!(bound.lower, Some((Value::Integer(_), _)));
            let hi_int =
                bound.upper.is_none() || matches!(bound.upper, Some((Value::Integer(_), _)));
            if !(lo_int && hi_int) {
                return Ok(None);
            }
            let start = match &bound.lower {
                Some((Value::Integer(i), _)) => *i,
                _ => i64::MIN,
            };
            let stop = match &bound.upper {
                Some((Value::Integer(i), _)) => *i,
                _ => i64::MAX,
            };
            let mut cur = TableCursor::new(self.backend.source(), meta.root);
            let mut rowids = Vec::new();
            let mut ok = if start == i64::MIN {
                cur.first()?
            } else {
                cur.seek(start)?;
                cur.is_valid()
            };
            while ok {
                let rid = cur.rowid()?;
                if rid > stop {
                    break;
                }
                rowids.push(rid);
                ok = cur.next()?;
            }
            return Ok(Some(rowids));
        }
        let Some((root, coll)) = self.leading_index_for(table_name, col, hint)? else {
            return Ok(None);
        };
        let aff = meta.columns[col].affinity;
        let lo = bound
            .lower
            .as_ref()
            .map(|(v, i)| (aff.coerce(v.clone()), *i));
        let hi = bound
            .upper
            .as_ref()
            .map(|(v, i)| (aff.coerce(v.clone()), *i));
        let colls = [coll];
        let lower = lo.as_ref().map(|(v, i)| (core::slice::from_ref(v), *i));
        let upper = hi.as_ref().map(|(v, i)| (core::slice::from_ref(v), *i));
        let rowids =
            crate::btree::index_range_rowids(self.backend.source(), root, lower, upper, &colls)?;
        Ok(Some(rowids))
    }

    /// Rowids for one seekable predicate atom (`col = c`, `col IN (…)`, or a range
    /// on `col`), or `None` if it is not index/rowid-seekable. Superset semantics:
    /// the caller re-applies the full `WHERE`.
    fn predicate_rowids(
        &self,
        meta: &TableMeta,
        table_name: &str,
        hint: Option<&IndexHint>,
        pred: &Expr,
        params: &Params,
    ) -> Result<Option<Vec<i64>>> {
        if let Some((col, vals)) = find_in_constraint(pred, &meta.columns, params) {
            if vals.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(None);
            }
            return self.seek_col_values(meta, table_name, hint, col, &vals);
        }
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(pred, &meta.columns, params, &mut eqs);
        eqs.retain(|(_, v)| !matches!(v, Value::Null));
        if let Some((col, v)) = eqs.into_iter().next() {
            return self.seek_col_values(meta, table_name, hint, col, &[v]);
        }
        let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
            alloc::collections::BTreeMap::new();
        collect_range_constraints(pred, &meta.columns, params, &mut ranges);
        if let Some((&col, bound)) = ranges.iter().next() {
            return self.seek_col_range(meta, table_name, hint, col, bound);
        }
        Ok(None)
    }

    /// Try to satisfy a single-table query whose `WHERE` is a top-level `OR` of
    /// individually-seekable predicates: seek each disjunct, union the rowids, and
    /// fetch the rows once. Returns `None` (→ scan) unless *every* disjunct is
    /// seekable. Superset semantics — `run_core` re-applies the full `WHERE`.
    fn try_index_or(
        &self,
        meta: &TableMeta,
        table_name: &str,
        sel: &Select,
        params: &Params,
    ) -> Result<Option<Vec<InputRow>>> {
        let Some(where_expr) = &sel.where_clause else {
            return Ok(None);
        };
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        // Flatten the top-level OR chain; require at least two disjuncts.
        let mut disjuncts: Vec<&Expr> = Vec::new();
        flatten_or(where_expr, &mut disjuncts);
        if disjuncts.len() < 2 {
            return Ok(None);
        }
        // Every disjunct must be seekable, else a scan is needed regardless.
        let mut rowids: Vec<i64> = Vec::new();
        for d in disjuncts {
            match self.predicate_rowids(meta, table_name, hint, d, params)? {
                Some(rs) => {
                    for r in rs {
                        if !rowids.contains(&r) {
                            rowids.push(r);
                        }
                    }
                }
                None => return Ok(None),
            }
        }
        let encoding = self.backend.source().header().text_encoding;
        let mut cur = TableCursor::new(self.backend.source(), meta.root);
        let mut out = Vec::new();
        for rid in rowids {
            if cur.seek(rid)? {
                let values = self.decode_full_row(meta, rid, &cur.payload()?, encoding)?;
                out.push(InputRow {
                    values,
                    rowid: Some(rid),
                });
            }
        }
        Ok(Some(out))
    }

    /// `EXPLAIN QUERY PLAN <stmt>` -> the `(id, parent, notused, detail)` rows
    /// that SQLite's API returns. The detail strings describe graphitesql's
    /// *actual* execution plan (it does not reorder joins), matching SQLite's
    /// format for the single-table SCAN/SEARCH cases.
    fn explain_query_plan(&self, stmt: &Statement, params: &Params) -> Result<QueryResult> {
        let mut details: Vec<(i64, i64, String)> = Vec::new();
        let mut next_id = 1i64;
        match stmt {
            Statement::Select(sel) => {
                self.eqp_select(sel, 0, &mut next_id, &mut details, params)?
            }
            Statement::Delete(d) => {
                let meta = self.table_meta(&d.table, None)?;
                let detail = self.eqp_access(
                    &d.table,
                    &d.table,
                    &meta,
                    d.where_clause.as_ref(),
                    None,
                    params,
                )?;
                details.push((next_id, 0, detail));
            }
            Statement::Update(u) => {
                let meta = self.table_meta(&u.table, None)?;
                let detail = self.eqp_access(
                    &u.table,
                    &u.table,
                    &meta,
                    u.where_clause.as_ref(),
                    None,
                    params,
                )?;
                details.push((next_id, 0, detail));
            }
            Statement::Insert(ins) => {
                if let InsertSource::Select(sel) = &ins.source {
                    self.eqp_select(sel, 0, &mut next_id, &mut details, params)?;
                }
            }
            _ => return Err(Error::Unsupported("EXPLAIN QUERY PLAN for this statement")),
        }
        Ok(QueryResult {
            columns: alloc::vec![
                String::from("id"),
                String::from("parent"),
                String::from("notused"),
                String::from("detail"),
            ],
            rows: details
                .into_iter()
                .map(|(id, parent, detail)| {
                    alloc::vec![
                        Value::Integer(id),
                        Value::Integer(parent),
                        Value::Integer(0),
                        Value::Text(detail),
                    ]
                })
                .collect(),
        })
    }

    /// Emit query-plan nodes for one SELECT under `parent`.
    /// EXPLAIN QUERY PLAN detail for a virtual-table scan: sqlite's
    /// `SCAN <label> VIRTUAL TABLE INDEX <idxNum>:<idxStr>`. The module's
    /// `best_index` chooses the plan from the offered `WHERE` constraints; a
    /// persistent module (which scans its backing table) reports a plain scan.
    fn eqp_vtab_detail(
        &self,
        name: &str,
        label: &str,
        sel: &Select,
        params: &Params,
    ) -> Result<String> {
        use crate::schema::ObjectType;
        let plain = || alloc::format!("SCAN {label} VIRTUAL TABLE INDEX 0:");
        let cvt = self
            .schema
            .objects()
            .iter()
            .find(|o| o.obj_type == ObjectType::Table && o.name.eq_ignore_ascii_case(name))
            .and_then(|o| o.sql.as_deref())
            .and_then(|s| match sql::parse_one(s) {
                Ok(Statement::CreateVirtualTable(cvt)) => Some(cvt),
                _ => None,
            });
        let Some(cvt) = cvt else { return Ok(plain()) };
        let Some(module) = self.vtab_registry.get(&cvt.module) else {
            return Ok(plain());
        };
        // The module's `best_index` chooses the reported plan from the offered
        // `WHERE` constraints — even for a persistent module, whose execution scans
        // `<name>_data` but whose reported `idxNum:idxStr` should still match SQLite
        // (e.g. rtree's spatial encoding). A module with no pushdown returns the
        // default plan, rendering the plain `INDEX 0:`.
        let arg_refs: Vec<&str> = cvt.args.iter().map(String::as_str).collect();
        let schema = module.dyn_connect(&arg_refs)?;
        let columns: Vec<ColumnInfo> = schema
            .columns
            .iter()
            .map(|n| ColumnInfo {
                name: n.clone(),
                table: label.to_string(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            })
            .collect();
        // FTS5's plan is driven by `MATCH` (a desugared `match()` function the
        // generic constraint collector doesn't see) and `ORDER BY rank`, so report
        // it directly to match sqlite's `xBestIndex`: `MATCH` is `M<col>` (the
        // matched column's 0-based index, or the column count for a table-wide
        // match), a rowid equality is `=`, and `ORDER BY rank` sets the
        // order-by-consumed bit (32) in idxNum.
        #[cfg(feature = "fts5")]
        if cvt.module.eq_ignore_ascii_case("fts5") {
            let mut idx_str = String::new();
            let mut matched = false;
            if let Some(where_expr) = &sel.where_clause {
                if let Some((_, operand)) = self.fts5_match_query(where_expr, params) {
                    let col = schema
                        .columns
                        .iter()
                        .position(|c| c.eq_ignore_ascii_case(&operand))
                        .unwrap_or(schema.columns.len());
                    idx_str = alloc::format!("M{col}");
                    matched = true;
                } else if fts5_rowid_eq(where_expr, params) {
                    idx_str.push('=');
                }
            }
            // With a MATCH, FTS5 can return rows already ordered by `rank` (idxNum
            // bit 32) or by `rowid` (bit 64), consuming the ORDER BY.
            let order_bit = if matched && sel.order_by.len() == 1 && !sel.order_by[0].descending {
                match &sel.order_by[0].expr {
                    Expr::Column {
                        table: None,
                        column,
                    } if column.eq_ignore_ascii_case("rank") => 32,
                    Expr::Column {
                        table: None,
                        column,
                    } if matches!(
                        column.to_ascii_lowercase().as_str(),
                        "rowid" | "_rowid_" | "oid"
                    ) =>
                    {
                        64
                    }
                    _ => 0,
                }
            } else {
                0
            };
            return Ok(alloc::format!(
                "SCAN {label} VIRTUAL TABLE INDEX {order_bit}:{idx_str}"
            ));
        }
        let (constraints, _) = collect_vtab_constraints(sel, &columns, params);
        let plan = module.dyn_best_index(&constraints)?;
        Ok(alloc::format!(
            "SCAN {label} VIRTUAL TABLE INDEX {}:{}",
            plan.idx_num,
            plan.idx_str.as_deref().unwrap_or("")
        ))
    }

    fn eqp_select(
        &self,
        sel: &Select,
        parent: i64,
        next_id: &mut i64,
        out: &mut Vec<(i64, i64, String)>,
        params: &Params,
    ) -> Result<()> {
        // Mirror run_core's comma-join → ON promotion so the plan reflects how the
        // query actually runs.
        let rewritten;
        let sel = match promote_comma_join_ons(sel) {
            Some(r) => {
                rewritten = r;
                &rewritten
            }
            None => sel,
        };
        let Some(from) = &sel.from else {
            return Ok(()); // SELECT with no FROM => no scan node
        };
        let label = eqp_label(&from.first);
        // A virtual table scans through its module, not a b-tree — render sqlite's
        // `VIRTUAL TABLE INDEX <n>:<str>` node and skip the regular-table planning
        // (which would otherwise parse the CREATE VIRTUAL TABLE as a CREATE TABLE
        // and fail).
        if from.joins.is_empty()
            && from.first.subquery.is_none()
            && from.first.tvf_args.is_none()
            && self.lookup_cte(&from.first.name, None).is_none()
            && self.is_virtual_table(&from.first.name)
        {
            let detail = self.eqp_vtab_detail(&from.first.name, &label, sel, params)?;
            let id = *next_id;
            *next_id += 1;
            out.push((id, parent, detail));
            return Ok(());
        }
        // First source.
        let meta = self.table_meta(&from.first.name, from.first.alias.as_deref())?;
        // A top-level OR of seekable disjuncts is a MULTI-INDEX OR plan (multiple
        // rows); otherwise a single SCAN/SEARCH node.
        if from.joins.is_empty()
            && self.eqp_or_plan(
                &label,
                &from.first.name,
                &meta,
                sel.where_clause.as_ref(),
                parent,
                next_id,
                out,
                params,
            )?
        {
            // rows already emitted
        } else {
            let detail = if from.joins.is_empty() {
                // `SELECT count(*)` answered by counting a full secondary index
                // (B2b) reads as `USING COVERING INDEX`. Kept in lockstep with
                // `run_core` via the shared `count_covering_index` helper.
                if let Some((name, _)) = self.count_covering_index(sel) {
                    alloc::format!("SCAN {label} USING COVERING INDEX {name}")
                }
                // A full index scanned to satisfy ORDER BY reads as `USING INDEX`,
                // or `USING COVERING INDEX` when it holds every referenced column.
                else if let Some(s) = self.order_index_scan(sel) {
                    let kind = if s.covering {
                        "COVERING INDEX"
                    } else {
                        "INDEX"
                    };
                    alloc::format!("SCAN {label} USING {kind} {}", s.name)
                }
                // A covered query with no seek reads from a covering index (B2),
                // in lockstep with `run_core`'s `covering_scan`.
                else if let Some((name, _, _)) = self.covering_scan(sel, &meta, params) {
                    alloc::format!("SCAN {label} USING COVERING INDEX {name}")
                } else {
                    self.eqp_access(
                        &label,
                        &from.first.name,
                        &meta,
                        sel.where_clause.as_ref(),
                        Some(sel),
                        params,
                    )?
                }
            } else {
                // Joins run in FROM order as nested-loop scans (no reordering).
                alloc::format!("SCAN {label}")
            };
            let id = *next_id;
            *next_id += 1;
            out.push((id, parent, detail));
        }
        // Fold each join in FROM order, tracking the accumulated left columns so
        // the rowid-seek decision (shared with the executor via `rowid_join_seek`)
        // can print `SEARCH … USING INTEGER PRIMARY KEY (rowid=?)` in lockstep
        // with how it actually runs.
        if !from.joins.is_empty() {
            let mut left_columns = self.resolve_join_source(&from.first, params)?.0;
            for join in &from.joins {
                let label = eqp_label(&join.table);
                // Most joins emit one plan row; an automatic-index (hash) join
                // emits two (a BLOOM FILTER then the SEARCH), so collect details.
                let (details, jcols): (Vec<String>, Vec<ColumnInfo>) = if let Some((
                    _,
                    inner_meta,
                )) =
                    self.rowid_join_seek(join, &left_columns)
                {
                    (
                        alloc::vec![alloc::format!(
                            "SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)"
                        )],
                        inner_meta.columns,
                    )
                } else if let Some((_, inner_meta, idx)) = self.index_join_seek(join, &left_columns)
                {
                    let col = &inner_meta.columns[idx.cols[0]].name;
                    (
                        alloc::vec![alloc::format!(
                            "SEARCH {label} USING INDEX {} ({col}=?)",
                            idx.name
                        )],
                        inner_meta.columns,
                    )
                } else if let Some((_, inner_meta)) =
                    self.without_rowid_pk_join_seek(join, &left_columns)
                {
                    let col = &inner_meta.columns[inner_meta.storage_order[0]].name;
                    let suffix = if matches!(join.kind, JoinKind::Left) {
                        " LEFT-JOIN"
                    } else {
                        ""
                    };
                    (
                        alloc::vec![alloc::format!(
                            "SEARCH {label} USING PRIMARY KEY ({col}=?){suffix}"
                        )],
                        inner_meta.columns,
                    )
                } else {
                    let jcols = self.resolve_join_source(&join.table, params)?.0;
                    // The executor builds a transient hash index for an INNER/LEFT
                    // equi-join (`ON l.x = r.y`) on an otherwise-unindexed inner
                    // table; SQLite reports that as a BLOOM FILTER + AUTOMATIC
                    // COVERING INDEX seek (NATURAL/USING and non-equi joins stay a
                    // plain SCAN, as graphite runs them with a nested loop).
                    let auto_col = if join.natural
                        || !join.using.is_empty()
                        || !matches!(join.kind, JoinKind::Inner | JoinKind::Left)
                    {
                        None
                    } else {
                        join.on.as_ref().and_then(|on| {
                            let mut combined = left_columns.clone();
                            combined.extend(jcols.iter().cloned());
                            join_equi_cols(on, &combined, left_columns.len())
                                .map(|(_, ri)| jcols[ri].name.clone())
                        })
                    };
                    match auto_col {
                        Some(col) => {
                            let suffix = if matches!(join.kind, JoinKind::Left) {
                                " LEFT-JOIN"
                            } else {
                                ""
                            };
                            (
                                alloc::vec![
                                    alloc::format!("BLOOM FILTER ON {label} ({col}=?)"),
                                    alloc::format!(
                                        "SEARCH {label} USING AUTOMATIC COVERING INDEX ({col}=?){suffix}"
                                    ),
                                ],
                                jcols,
                            )
                        }
                        None => (alloc::vec![alloc::format!("SCAN {label}")], jcols),
                    }
                };
                for detail in details {
                    let id = *next_id;
                    *next_id += 1;
                    out.push((id, parent, detail));
                }
                let left_width = left_columns.len();
                left_columns.extend(jcols);
                // Mirror the executor's NATURAL / USING coalescing: each join
                // column folds into its left output position and the right
                // duplicate is dropped, so a later join's `left_width` stays
                // aligned (a rowid-seek join never uses NATURAL / USING).
                if join.natural || !join.using.is_empty() {
                    let mut drop: Vec<usize> = if join.natural {
                        (left_width..left_columns.len())
                            .filter(|&rl| {
                                left_columns[..left_width]
                                    .iter()
                                    .any(|c| c.name.eq_ignore_ascii_case(&left_columns[rl].name))
                            })
                            .collect()
                    } else {
                        join.using
                            .iter()
                            .filter_map(|name| {
                                (left_width..left_columns.len())
                                    .find(|&rl| left_columns[rl].name.eq_ignore_ascii_case(name))
                            })
                            .collect()
                    };
                    drop.sort_unstable();
                    drop.dedup();
                    for &d in drop.iter().rev() {
                        left_columns.remove(d);
                    }
                }
            }
        }
        // ORDER BY that we satisfy with an in-memory sort — unless the scan already
        // yields the requested order (no temp b-tree then, like sqlite). When a
        // seek walks a *prefix* of the ORDER BY in order, only the trailing terms
        // are sorted, which sqlite reports as "LAST n TERM[S] OF ORDER BY".
        if !sel.order_by.is_empty() && self.order_satisfied_by_scan(sel, params).is_none() {
            let n = sel.order_by.len();
            // Only the trailing terms are sorted when the access walks a prefix of
            // the ORDER BY in order: a non-covering index walk (mixed direction,
            // `order_index_scan.sorted_suffix`), a WHERE seek (`seek_order_prefix`),
            // or a no-WHERE covering-index scan (`scan_order_prefix`).
            let sorted = if let Some(s) = self.order_index_scan(sel) {
                s.sorted_suffix.min(n)
            } else if let Some((k, _)) = self.seek_order_prefix(sel, params) {
                n - k.min(n)
            } else {
                n - self.scan_order_prefix(sel, params).min(n)
            };
            let detail = match sorted {
                _ if sorted >= n => String::from("USE TEMP B-TREE FOR ORDER BY"),
                1 => String::from("USE TEMP B-TREE FOR LAST TERM OF ORDER BY"),
                _ => alloc::format!("USE TEMP B-TREE FOR LAST {sorted} TERMS OF ORDER BY"),
            };
            let id = *next_id;
            *next_id += 1;
            out.push((id, parent, detail));
        }
        Ok(())
    }

    /// Emit a SQLite-style `MULTI-INDEX OR` plan when `where_clause` is a
    /// top-level `OR` whose every disjunct is index/rowid-seekable (i.e. each
    /// disjunct's [`eqp_access`](Self::eqp_access) yields a `SEARCH`). Returns
    /// `true` (rows pushed) when it applies, else `false` (caller emits the plain
    /// node). Mirrors [`try_index_or`](Self::try_index_or)'s applicability.
    #[allow(clippy::too_many_arguments)]
    fn eqp_or_plan(
        &self,
        label: &str,
        table: &str,
        meta: &TableMeta,
        where_clause: Option<&Expr>,
        parent: i64,
        next_id: &mut i64,
        out: &mut Vec<(i64, i64, String)>,
        params: &Params,
    ) -> Result<bool> {
        let Some(where_expr) = where_clause else {
            return Ok(false);
        };
        let mut disjuncts: Vec<&Expr> = Vec::new();
        flatten_or(where_expr, &mut disjuncts);
        if disjuncts.len() < 2 {
            return Ok(false);
        }
        // Each disjunct must seek (its eqp_access is a SEARCH, not a SCAN).
        let mut details = Vec::with_capacity(disjuncts.len());
        for d in &disjuncts {
            let detail = self.eqp_access(label, table, meta, Some(d), None, params)?;
            if !detail.starts_with("SEARCH") {
                return Ok(false);
            }
            details.push(detail);
        }
        let or_id = *next_id;
        *next_id += 1;
        out.push((or_id, parent, String::from("MULTI-INDEX OR")));
        for (i, detail) in details.into_iter().enumerate() {
            let idx_id = *next_id;
            *next_id += 1;
            out.push((idx_id, or_id, alloc::format!("INDEX {}", i + 1)));
            let search_id = *next_id;
            *next_id += 1;
            out.push((search_id, idx_id, detail));
        }
        Ok(true)
    }

    /// The SCAN/SEARCH detail string for accessing one table given its WHERE.
    /// `label` is the display name (alias if any); `table` is the real table
    /// name used to look up its indexes. `sel`, when present, is the enclosing
    /// `SELECT`: a seek whose index covers every referenced column reads as
    /// `USING COVERING INDEX` (B2b), kept in lockstep with the executor's
    /// [`seek_index_covers`](Self::seek_index_covers) decision. `None` (DELETE /
    /// UPDATE / OR-plan disjuncts, which all touch the table) never covers.
    fn eqp_access(
        &self,
        label: &str,
        table: &str,
        meta: &TableMeta,
        where_clause: Option<&Expr>,
        sel: Option<&Select>,
        params: &Params,
    ) -> Result<String> {
        let Some(where_expr) = where_clause else {
            return Ok(alloc::format!("SCAN {label}"));
        };
        // `INDEX` vs `COVERING INDEX` for a seek through `idx_cols`: the same
        // decision the executor's seek paths make via `seek_index_covers`.
        let index_kw = |idx_cols: &[usize]| -> &'static str {
            match sel {
                Some(s) if self.seek_index_covers(s, meta, idx_cols, where_expr) => {
                    "COVERING INDEX"
                }
                _ => "INDEX",
            }
        };
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
        eqs.retain(|(_, v)| !matches!(v, Value::Null));
        // WITHOUT ROWID: the executor seeks the clustered PRIMARY KEY b-tree on a
        // leading-PK equality (`try_without_rowid_pk_seek`) and otherwise scans —
        // it never uses a secondary index — so report exactly that.
        if meta.without_rowid {
            let pk = &meta.storage_order[..meta.pk_len];
            // A leading-PK equality prefix (matches try_without_rowid_pk_seek).
            let mut names = Vec::new();
            for &c in pk {
                if eqs.iter().any(|(col, _)| *col == c) {
                    names.push(alloc::format!("{}=?", meta.columns[c].name));
                } else {
                    break;
                }
            }
            if !names.is_empty() {
                return Ok(alloc::format!(
                    "SEARCH {label} USING PRIMARY KEY ({})",
                    names.join(" AND ")
                ));
            }
            // Else a range bound on the leading PK column (try_without_rowid_pk_range).
            if let Some(&lead) = pk.first() {
                let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
                    alloc::collections::BTreeMap::new();
                collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
                if let Some(b) = ranges.get(&lead) {
                    let name = &meta.columns[lead].name;
                    // SQLite renders bounds as `>`/`<` regardless of inclusivity.
                    let cond = match (&b.lower, &b.upper) {
                        (Some(_), Some(_)) => alloc::format!("{name}>? AND {name}<?"),
                        (Some(_), None) => alloc::format!("{name}>?"),
                        (None, Some(_)) => alloc::format!("{name}<?"),
                        (None, None) => String::new(),
                    };
                    if !cond.is_empty() {
                        return Ok(alloc::format!("SEARCH {label} USING PRIMARY KEY ({cond})"));
                    }
                }
            }
            // A secondary index whose leading column(s) the WHERE constrains by
            // equality (matches try_without_rowid_index_seek). Its records carry
            // the PK columns, so covering accounts for idx.cols ∪ pk.
            for idx in self.indexes_of(table)? {
                if idx.partial.is_some() || idx.key_exprs.is_some() {
                    continue;
                }
                let mut matched = Vec::new();
                for &c in &idx.cols {
                    if eqs.iter().any(|(col, _)| *col == c) {
                        matched.push(c);
                    } else {
                        break;
                    }
                }
                if matched.is_empty() {
                    continue;
                }
                let mut avail = idx.cols.clone();
                if !idx.name.starts_with("sqlite_autoindex_") {
                    for &p in pk {
                        if !avail.contains(&p) {
                            avail.push(p);
                        }
                    }
                }
                let kw = match sel {
                    Some(s) if self.seek_index_covers(s, meta, &avail, where_expr) => {
                        "COVERING INDEX"
                    }
                    _ => "INDEX",
                };
                let cond = matched
                    .iter()
                    .map(|&c| alloc::format!("{}=?", meta.columns[c].name))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                return Ok(alloc::format!(
                    "SEARCH {label} USING {kw} {} ({cond})",
                    idx.name
                ));
            }
            // Else a range bound on a secondary index's leading column
            // (matches try_without_rowid_index_range).
            let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
                alloc::collections::BTreeMap::new();
            collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
            for idx in self.indexes_of(table)? {
                if idx.partial.is_some() || idx.key_exprs.is_some() {
                    continue;
                }
                let Some(&lead) = idx.cols.first() else {
                    continue;
                };
                let Some(b) = ranges.get(&lead) else {
                    continue;
                };
                let name = &meta.columns[lead].name;
                let cond = match (&b.lower, &b.upper) {
                    (Some(_), Some(_)) => alloc::format!("{name}>? AND {name}<?"),
                    (Some(_), None) => alloc::format!("{name}>?"),
                    (None, Some(_)) => alloc::format!("{name}<?"),
                    (None, None) => continue,
                };
                let mut avail = idx.cols.clone();
                if !idx.name.starts_with("sqlite_autoindex_") {
                    for &p in pk {
                        if !avail.contains(&p) {
                            avail.push(p);
                        }
                    }
                }
                let kw = match sel {
                    Some(s) if self.seek_index_covers(s, meta, &avail, where_expr) => {
                        "COVERING INDEX"
                    }
                    _ => "INDEX",
                };
                return Ok(alloc::format!(
                    "SEARCH {label} USING {kw} {} ({cond})",
                    idx.name
                ));
            }
            return Ok(alloc::format!("SCAN {label}"));
        }
        // A `rowid`/`_rowid_`/`oid` `= N` or `IN (list)` seek wins (matches the
        // rowid fast path at the top of try_index_lookup), with or without an IPK.
        if rowid_seek_constraint(where_expr, &meta.columns, params).is_some() {
            return Ok(alloc::format!(
                "SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)"
            ));
        }
        // Rowid equality wins, as in try_index_lookup.
        if let Some(ipk) = meta.ipk {
            if eqs.iter().any(|(c, _)| *c == ipk) {
                return Ok(alloc::format!(
                    "SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)"
                ));
            }
        }
        // Index covering the longest leftmost prefix of equalities, preferring
        // the most selective one when `ANALYZE` statistics are available (kept in
        // step with the cost-based chooser in try_index_lookup).
        let stats = self.stat1_map();
        #[allow(clippy::type_complexity)]
        let mut best: Option<(String, Vec<usize>, Vec<usize>, u64)> = None;
        // Iterate the SAME index set `try_index_lookup` does (via `indexes_of`,
        // which includes the implicit `sqlite_autoindex_*` PK/UNIQUE indexes) so
        // the EQP reports the seek the executor actually performs — e.g. a
        // non-integer PRIMARY KEY or UNIQUE column reads as `SEARCH … USING INDEX
        // sqlite_autoindex_…`, not `SCAN`. Partial/expression indexes are handled
        // by the separate fallback below.
        for idx in self.indexes_of(table)? {
            if idx.partial.is_some() || idx.key_exprs.is_some() {
                continue;
            }
            let mut matched = Vec::new();
            for &c in &idx.cols {
                if eqs.iter().any(|(col, _)| *col == c) {
                    matched.push(c);
                } else {
                    break;
                }
            }
            if matched.is_empty() {
                continue;
            }
            let est = stats
                .get(&idx.name)
                .and_then(|s| s.get(matched.len()).copied())
                .unwrap_or(u64::MAX - matched.len() as u64);
            let better = match &best {
                None => true,
                Some((_, bm, _, be)) => est < *be || (est == *be && matched.len() > bm.len()),
            };
            if better {
                best = Some((idx.name.clone(), matched, idx.cols.clone(), est));
            }
        }
        if let Some((idx_name, matched, idx_cols, _)) = best {
            if !matched.is_empty() {
                let mut conds = matched
                    .iter()
                    .map(|&c| alloc::format!("{}=?", meta.columns[c].name))
                    .collect::<Vec<_>>();
                // A range on the column after the equality prefix is seeked too
                // (matches the eq-prefix + range path in try_index_lookup).
                if let Some(&next_col) = idx_cols.get(matched.len()) {
                    let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
                        alloc::collections::BTreeMap::new();
                    collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
                    if let Some(b) = ranges.get(&next_col) {
                        let name = &meta.columns[next_col].name;
                        if b.lower.is_some() {
                            conds.push(alloc::format!("{name}>?"));
                        }
                        if b.upper.is_some() {
                            conds.push(alloc::format!("{name}<?"));
                        }
                    }
                }
                let kw = index_kw(&idx_cols);
                return Ok(alloc::format!(
                    "SEARCH {label} USING {kw} {idx_name} ({})",
                    conds.join(" AND ")
                ));
            }
        }

        // Partial / expression equality seek — in lockstep with the same
        // fallback in `try_index_lookup` (plain column indexes win first; this
        // fires only when none applied). `partial_expr_seek` proves eligibility.
        for idx in self.indexes_of(table)? {
            if self
                .partial_expr_seek(&idx, where_expr, meta, params)?
                .is_some()
            {
                let cond = match &idx.key_exprs {
                    // Partial column index: render the matched leading columns.
                    None => idx
                        .cols
                        .iter()
                        .take_while(|&&c| eqs.iter().any(|(col, _)| *col == c))
                        .map(|&c| alloc::format!("{}=?", meta.columns[c].name))
                        .collect::<Vec<_>>()
                        .join(" AND "),
                    // Expression index: the indexed expression compared to a value.
                    Some(_) => "<expr>=?".into(),
                };
                return Ok(alloc::format!(
                    "SEARCH {label} USING INDEX {} ({cond})",
                    idx.name
                ));
            }
        }

        // No equality index applied. Mirror run_core's remaining fast paths
        // (range, then IN) so the plan reflects what actually executes. Find the
        // name/columns of a plain index by its leading column.
        let leading_index = |target: usize| -> Option<(String, Vec<usize>)> {
            for obj in self.schema.indexes_on(table) {
                let sql = obj.sql.as_ref()?;
                let Ok(Statement::CreateIndex(ci)) = sql::parse_one(sql) else {
                    continue;
                };
                if ci.where_clause.is_some() {
                    continue;
                }
                let Ok(cols) = self.index_columns(meta, &ci) else {
                    continue;
                };
                if cols.first() == Some(&target) {
                    return Some((obj.name.clone(), cols));
                }
            }
            None
        };

        // Range scan: rowid (integer bounds) walks the table b-tree; an indexed
        // leading column seeks its index.
        let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
            alloc::collections::BTreeMap::new();
        collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
        if let Some(ipk) = meta.ipk {
            if let Some(b) = ranges.get(&ipk) {
                let lo_int = b.lower.is_none() || matches!(b.lower, Some((Value::Integer(_), _)));
                let hi_int = b.upper.is_none() || matches!(b.upper, Some((Value::Integer(_), _)));
                if lo_int && hi_int {
                    let cond = match (&b.lower, &b.upper) {
                        (Some(_), Some(_)) => "rowid>? AND rowid<?",
                        (Some(_), None) => "rowid>?",
                        (None, Some(_)) => "rowid<?",
                        (None, None) => "",
                    };
                    if !cond.is_empty() {
                        return Ok(alloc::format!(
                            "SEARCH {label} USING INTEGER PRIMARY KEY ({cond})"
                        ));
                    }
                }
            }
        }
        for (&col, bound) in &ranges {
            if let Some((idx_name, idx_cols)) = leading_index(col) {
                let name = &meta.columns[col].name;
                // SQLite's EQP renders bounds as `>`/`<` regardless of inclusivity.
                let cond = match (&bound.lower, &bound.upper) {
                    (Some(_), Some(_)) => alloc::format!("{name}>? AND {name}<?"),
                    (Some(_), None) => alloc::format!("{name}>?"),
                    (None, Some(_)) => alloc::format!("{name}<?"),
                    (None, None) => continue,
                };
                let kw = index_kw(&idx_cols);
                return Ok(alloc::format!(
                    "SEARCH {label} USING {kw} {idx_name} ({cond})"
                ));
            }
        }

        // A3b: a partial or expression index range seek (mirrors the
        // `partial_expr_range` fallback in try_index_range; always non-covering).
        for idx in self.indexes_of(table)? {
            if let Some((bound, _)) = self.partial_expr_range(&idx, where_expr, meta, params) {
                let cond = |name: &str| match (&bound.lower, &bound.upper) {
                    (Some(_), Some(_)) => alloc::format!("{name}>? AND {name}<?"),
                    (Some(_), None) => alloc::format!("{name}>?"),
                    (None, Some(_)) => alloc::format!("{name}<?"),
                    (None, None) => String::new(),
                };
                let rendered = match &idx.key_exprs {
                    None => cond(&meta.columns[idx.cols[0]].name),
                    Some(_) => cond("<expr>"),
                };
                if !rendered.is_empty() {
                    return Ok(alloc::format!(
                        "SEARCH {label} USING INDEX {} ({rendered})",
                        idx.name
                    ));
                }
            }
        }

        // IN-list seek: rowid b-tree, a plain/partial index on the IN column, or
        // an expression index keyed by the IN'd expression (mirrors try_index_in).
        if let Some((col, _)) = find_in_constraint(where_expr, &meta.columns, params) {
            if meta.ipk == Some(col) {
                return Ok(alloc::format!(
                    "SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)"
                ));
            }
            if let Some((idx_name, idx_cols)) = leading_index(col) {
                let name = &meta.columns[col].name;
                let kw = index_kw(&idx_cols);
                return Ok(alloc::format!(
                    "SEARCH {label} USING {kw} {idx_name} ({name}=?)"
                ));
            }
            // A3b: a partial index on the IN column with its predicate proven.
            for idx in self.indexes_of(table)? {
                if idx.key_exprs.is_some() || idx.partial.is_none() {
                    continue;
                }
                if idx.cols.first() == Some(&col) && partial_pred_guaranteed(&idx, where_expr) {
                    let name = &meta.columns[col].name;
                    return Ok(alloc::format!(
                        "SEARCH {label} USING INDEX {} ({name}=?)",
                        idx.name
                    ));
                }
            }
        }
        // A3b: an expression index keyed by `<expr>` with `<expr> IN (…)`.
        for idx in self.indexes_of(table)? {
            let Some(exprs) = &idx.key_exprs else {
                continue;
            };
            let [key_expr] = exprs.as_slice() else {
                continue;
            };
            if partial_pred_guaranteed(&idx, where_expr)
                && find_expr_in_values(key_expr, where_expr, params).is_some()
            {
                return Ok(alloc::format!(
                    "SEARCH {label} USING INDEX {} (<expr>=?)",
                    idx.name
                ));
            }
        }

        Ok(alloc::format!("SCAN {label}"))
    }

    fn indexes_of(&self, table: &str) -> Result<Vec<IndexMeta>> {
        let tmeta = match self.schema.table(table) {
            Some(_) => self.table_meta(table, None)?,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for obj in self.schema.indexes_on(table) {
            match &obj.sql {
                Some(sql) => {
                    let Statement::CreateIndex(ci) = sql::parse_one(sql)? else {
                        continue;
                    };
                    let (cols, key_exprs, collations) = self.index_key_spec(&tmeta, &ci)?;
                    out.push(IndexMeta {
                        name: obj.name.clone(),
                        root: obj.rootpage,
                        cols,
                        collations,
                        partial: ci.where_clause.clone(),
                        key_exprs,
                        unique: ci.unique,
                    });
                }
                // Automatic index: its columns are the n-th UNIQUE/PK set.
                None => {
                    if let Some(n) = autoindex_number(&obj.name, table) {
                        if let Some((cols, _)) = tmeta.unique.get(n - 1) {
                            let collations = self.col_collations(&tmeta, cols);
                            out.push(IndexMeta {
                                name: obj.name.clone(),
                                root: obj.rootpage,
                                cols: cols.clone(),
                                collations,
                                partial: None,
                                key_exprs: None,
                                unique: true,
                            });
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn index_columns(&self, tmeta: &TableMeta, ci: &CreateIndex) -> Result<Vec<usize>> {
        Ok(self.index_columns_coll(tmeta, ci)?.0)
    }

    /// Resolve an index's columns to `(positions, collations)`. A column may
    /// carry an explicit `COLLATE name`; otherwise it inherits the table
    /// column's declared collation.
    fn index_columns_coll(
        &self,
        tmeta: &TableMeta,
        ci: &CreateIndex,
    ) -> Result<(Vec<usize>, Vec<crate::value::Collation>)> {
        let mut cols = Vec::new();
        let mut colls = Vec::new();
        for term in &ci.columns {
            // Peel an explicit COLLATE off the index column expression.
            let (inner, explicit) = match &term.expr {
                Expr::Collate { expr, collation } => {
                    (expr.as_ref(), crate::value::Collation::parse(collation))
                }
                e => (e, None),
            };
            let Expr::Column { column, .. } = inner else {
                return Err(Error::Unsupported("expression indexes"));
            };
            let pos = tmeta
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(column))
                .ok_or_else(|| Error::Error(format!("no such column: {column}")))?;
            cols.push(pos);
            colls.push(explicit.unwrap_or(tmeta.columns[pos].collation));
        }
        Ok((cols, colls))
    }

    /// Resolve an index's key terms to `(cols, key_exprs, collations)`. When every
    /// term is a plain column, `key_exprs` is `None` and `cols` holds the column
    /// positions. When any term is an expression (`lower(x)`, `a + b`, …), it is
    /// an expression index: `key_exprs` holds the COLLATE-peeled term expressions
    /// (evaluated per row to form the key) and `cols` is empty.
    #[allow(clippy::type_complexity)]
    fn index_key_spec(
        &self,
        tmeta: &TableMeta,
        ci: &CreateIndex,
    ) -> Result<(Vec<usize>, Option<Vec<Expr>>, Vec<crate::value::Collation>)> {
        let mut cols = Vec::new();
        let mut exprs = Vec::new();
        let mut colls = Vec::new();
        let mut is_expr = false;
        for term in &ci.columns {
            let (inner, explicit) = match &term.expr {
                Expr::Collate { expr, collation } => {
                    (expr.as_ref(), crate::value::Collation::parse(collation))
                }
                e => (e, None),
            };
            exprs.push(inner.clone());
            match inner {
                Expr::Column { column, .. } => {
                    let pos = tmeta
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(column))
                        .ok_or_else(|| Error::Error(format!("no such column: {column}")))?;
                    cols.push(pos);
                    colls.push(explicit.unwrap_or(tmeta.columns[pos].collation));
                }
                _ => {
                    is_expr = true;
                    colls.push(explicit.unwrap_or_default());
                }
            }
        }
        if is_expr {
            Ok((Vec::new(), Some(exprs), colls))
        } else {
            Ok((cols, None, colls))
        }
    }

    /// The on-disk index key bytes for `idx` over a table row: evaluated key
    /// expressions for an expression index, else the column values.
    fn index_key_bytes(
        &self,
        idx: &IndexMeta,
        meta: &TableMeta,
        values: &[Value],
        rowid: i64,
        params: &Params,
    ) -> Result<Vec<u8>> {
        match &idx.key_exprs {
            None => Ok(index_key(&idx.cols, values, rowid)),
            Some(exprs) => {
                let ctx = row_ctx(values, &meta.columns, Some(rowid), params).with_subqueries(self);
                let mut key: Vec<Value> = exprs
                    .iter()
                    .map(|e| eval::eval(e, &ctx))
                    .collect::<Result<_>>()?;
                key.push(Value::Integer(rowid));
                Ok(encode_record(&key))
            }
        }
    }

    /// The declared collations of a set of table columns (for autoindexes).
    fn col_collations(&self, tmeta: &TableMeta, cols: &[usize]) -> Vec<crate::value::Collation> {
        cols.iter().map(|&c| tmeta.columns[c].collation).collect()
    }

    /// Whether a row belongs in `idx`: always for a full index, else whether the
    /// partial-index predicate holds for the row.
    fn row_in_index(
        &self,
        idx: &IndexMeta,
        tmeta: &TableMeta,
        values: &[Value],
        rowid: Option<i64>,
        params: &Params,
    ) -> Result<bool> {
        match &idx.partial {
            None => Ok(true),
            Some(pred) => {
                let ctx = row_ctx(values, &tmeta.columns, rowid, params).with_subqueries(self);
                Ok(eval::truth(&eval::eval(pred, &ctx)?) == Some(true))
            }
        }
    }

    /// Rebuild every index of a table in place (used after DELETE/UPDATE).
    fn rebuild_indexes(&mut self, tmeta: &TableMeta, indexes: &[IndexMeta]) -> Result<()> {
        if indexes.is_empty() {
            return Ok(());
        }
        let rows = self.scan_table(tmeta)?;
        let no_params = Params::default();
        // Precompute, per index, the key bytes for each included row (partial
        // predicate + expression evaluation) before taking the writer borrow.
        let mut per_index: Vec<Vec<Vec<u8>>> = Vec::with_capacity(indexes.len());
        for idx in indexes {
            let mut keys = Vec::new();
            for (rowid, values) in &rows {
                if self.row_in_index(idx, tmeta, values, Some(*rowid), &no_params)? {
                    keys.push(self.index_key_bytes(idx, tmeta, values, *rowid, &no_params)?);
                }
            }
            per_index.push(keys);
        }
        let w = self.backend.writer()?;
        for (idx, keys) in indexes.iter().zip(&per_index) {
            clear_index(w, idx.root)?;
            for key in keys {
                insert_index(w, idx.root, key, &idx.collations)?;
            }
        }
        Ok(())
    }

    /// Rowids of rows in `meta` satisfying `pred` (all rows if `None`).
    /// Reduce candidate rowids by an `UPDATE`/`DELETE` `ORDER BY … LIMIT …`
    /// clause (the SQLite update/delete-limit extension): order the rows by the
    /// terms, then apply `OFFSET`/`LIMIT` (a negative limit means no limit). With
    /// no `ORDER BY`, the candidates keep their scan (rowid) order.
    fn order_limit_rowids(
        &self,
        meta: &TableMeta,
        rowids: Vec<i64>,
        order_by: &[OrderTerm],
        limit: Option<&Expr>,
        offset: Option<&Expr>,
        params: &Params,
    ) -> Result<Vec<i64>> {
        let mut rowids = rowids;
        if !order_by.is_empty() {
            let mut keyed: Vec<(i64, Vec<Value>)> = Vec::with_capacity(rowids.len());
            for rid in rowids {
                let row = self.read_row(meta, rid)?.unwrap_or_default();
                let ctx = row_ctx(&row, &meta.columns, Some(rid), params).with_subqueries(self);
                let keys = order_by
                    .iter()
                    .map(|t| eval::eval(&t.expr, &ctx))
                    .collect::<Result<Vec<_>>>()?;
                keyed.push((rid, keys));
            }
            keyed.sort_by(|a, b| {
                for (i, t) in order_by.iter().enumerate() {
                    let o = cmp_order(
                        &a.1[i],
                        &b.1[i],
                        t.descending,
                        t.nulls_first,
                        crate::value::Collation::Binary,
                    );
                    if o != core::cmp::Ordering::Equal {
                        return o;
                    }
                }
                core::cmp::Ordering::Equal
            });
            rowids = keyed.into_iter().map(|(r, _)| r).collect();
        }
        let off = match offset {
            Some(e) => must_be_int(eval::eval(
                e,
                &EvalCtx::rowless(params).with_subqueries(self),
            )?)?
            .max(0) as usize,
            None => 0,
        };
        if off > 0 {
            rowids.drain(0..off.min(rowids.len()));
        }
        if let Some(e) = limit {
            let n = eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?);
            if n >= 0 {
                rowids.truncate(n as usize);
            }
        }
        Ok(rowids)
    }

    fn matching_rowids(
        &self,
        meta: &TableMeta,
        pred: Option<&Expr>,
        params: &Params,
    ) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        let mut cur = TableCursor::new(self.backend.source(), meta.root);
        let encoding = self.backend.source().header().text_encoding;
        let mut ok = cur.first()?;
        while ok {
            let rowid = cur.rowid()?;
            let values = self.decode_full_row(meta, rowid, &cur.payload()?, encoding)?;
            let keep = match pred {
                Some(p) => {
                    let ctx =
                        row_ctx(&values, &meta.columns, Some(rowid), params).with_subqueries(self);
                    eval::truth(&eval::eval(p, &ctx)?) == Some(true)
                }
                None => true,
            };
            if keep {
                out.push(rowid);
            }
            ok = cur.next()?;
        }
        Ok(out)
    }

    /// The next rowid to assign for the table b-tree at `root` (max + 1, or 1).
    fn next_rowid(&self, root: u32) -> Result<i64> {
        let mut cur = TableCursor::new(self.backend.source(), root);
        if cur.last()? {
            Ok(cur.rowid()? + 1)
        } else {
            Ok(1)
        }
    }

    // ---- SELECT execution ---------------------------------------------------

    /// The cap (`LIMIT`+`OFFSET`) to bound a recursive CTE by, when `sel` streams a
    /// single recursive CTE 1:1 — `SELECT <cols> FROM <rcte> LIMIT k [OFFSET o]`
    /// with no WHERE / ORDER BY / GROUP BY / DISTINCT / join / aggregate / compound.
    /// Then an unterminated recursion still yields `k` rows, as sqlite (which
    /// evaluates the CTE lazily) does; the outer LIMIT/OFFSET still slice as usual.
    fn recursive_cte_outer_cap(&self, sel: &Select, params: &Params) -> Option<usize> {
        if sel.ctes.len() != 1
            || !sel.compound.is_empty()
            || sel.distinct
            || !sel.group_by.is_empty()
            || !sel.order_by.is_empty()
            || sel.where_clause.is_some()
            || sel.having.is_some()
            || self.has_aggregate(sel)
        {
            return None;
        }
        let cte = &sel.ctes[0];
        if !references_name(&cte.select, &cte.name) {
            return None; // not a recursive CTE
        }
        let from = sel.from.as_ref()?;
        if !from.joins.is_empty()
            || from.first.subquery.is_some()
            || from.first.tvf_args.is_some()
            || !from.first.name.eq_ignore_ascii_case(&cte.name)
        {
            return None;
        }
        let ctx = EvalCtx::rowless(params).with_subqueries(self);
        let n = must_be_int(eval::eval(sel.limit.as_ref()?, &ctx).ok()?).ok()?;
        if n < 0 {
            return None; // a negative LIMIT is unbounded — nothing to cap with
        }
        let offset = match &sel.offset {
            Some(e) => must_be_int(eval::eval(e, &ctx).ok()?).ok()?.max(0) as usize,
            None => 0,
        };
        Some((n as usize).saturating_add(offset))
    }

    fn run_select(&self, sel: &Select, params: &Params) -> Result<QueryResult> {
        // Materialize this query's `WITH` CTEs into the environment for the
        // duration of the query, then restore the previous scope. (The opt-in
        // VDBE fast path is attempted per query block inside `run_core`, so it
        // also covers each arm of a compound query.)
        let base = self.cte_env.borrow().len();
        let outer_cap = self.recursive_cte_outer_cap(sel, params);
        let pushed = self.push_ctes(&sel.ctes, params, outer_cap);
        let result = pushed.and_then(|()| self.run_select_compound(sel, params));
        self.cte_env.borrow_mut().truncate(base);
        result
    }

    fn run_select_compound(&self, sel: &Select, params: &Params) -> Result<QueryResult> {
        if sel.compound.is_empty() {
            return self.run_core(sel, params);
        }
        // Compound query: run the first core (without the trailing ORDER BY/LIMIT
        // and compound tail), then fold in each operand, then order/limit the whole.
        let mut first = sel.clone();
        first.compound = Vec::new();
        first.order_by = Vec::new();
        first.limit = None;
        first.offset = None;
        let mut result = self.run_core(&first, params)?;
        // Compound set operations (UNION/INTERSECT/EXCEPT) compare rows under the
        // left SELECT's per-column collations.
        let colls = {
            let (cols, _) = self.scan_source(&first, params)?;
            self.output_collations(&first, &cols, params)
        };
        // A multi-row `VALUES (…),(…)` desugars to a `UNION ALL` chain whose
        // operands are bare projections (no FROM); a real set operation joins
        // genuine SELECTs. SQLite rejects a column-count mismatch in either case,
        // but with different wording, so pick the message by which kind this is.
        let is_values = sel.from.is_none()
            && sel.where_clause.is_none()
            && sel.group_by.is_empty()
            && sel
                .compound
                .iter()
                .all(|(op, c)| *op == CompoundOp::UnionAll && c.from.is_none());
        for (op, operand) in &sel.compound {
            // Run the operand fully: a `VALUES (…),(…)` operand desugars to a
            // SELECT carrying its extra rows in its *own* compound tail, so it
            // must be expanded (not just its first core) or those rows are lost.
            let r = self.run_select_compound(operand, params)?;
            // Every operand of a compound query (and every row of a multi-row
            // `VALUES`) must project the same number of columns. SQLite rejects a
            // mismatch; match that (errors-vs-succeeds, not exact text).
            if r.columns.len() != result.columns.len() {
                return Err(Error::Error(if is_values {
                    "all VALUES must have the same number of terms".into()
                } else {
                    "SELECTs to the left and right of the compound operator do not have the same \
                     number of result columns"
                        .into()
                }));
            }
            result.rows = apply_compound(*op, result.rows, r.rows, &colls);
        }
        // A dedup set operation (UNION/INTERSECT/EXCEPT) yields rows in sorted
        // order in SQLite — its dedup is implemented via a sorter — whereas
        // UNION ALL preserves order. With no explicit ORDER BY, sort the combined
        // result by all output columns (ascending, under each column's collation;
        // NULLs first) to match. An explicit ORDER BY is applied below instead.
        if sel.order_by.is_empty()
            && sel
                .compound
                .iter()
                .any(|(op, _)| *op != CompoundOp::UnionAll)
        {
            result.rows.sort_by(|a, b| {
                for (i, va) in a.iter().enumerate() {
                    let coll = colls.get(i).copied().unwrap_or_default();
                    let ord = crate::value::cmp_values_coll(va, &b[i], coll);
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }
        self.compound_order_limit(&mut result, sel, params, &colls)?;
        Ok(result)
    }

    /// Apply a compound query's overall `ORDER BY` / `LIMIT` / `OFFSET` to the
    /// already-combined rows (terms must reference output columns by position or
    /// name).
    fn compound_order_limit(
        &self,
        result: &mut QueryResult,
        sel: &Select,
        params: &Params,
        colls: &[crate::value::Collation],
    ) -> Result<()> {
        if !sel.order_by.is_empty() {
            // A positional ORDER BY term must name an output column (SQLite).
            check_positional_terms(&[], &sel.order_by, result.columns.len())?;
            let mut keys = Vec::new();
            for term in &sel.order_by {
                let idx = resolve_order_index(&term.expr, &result.columns, result.columns.len())
                    .ok_or(Error::Unsupported(
                        "ORDER BY term must be an output column in a compound query",
                    ))?;
                // The output column's collation (from the left SELECT) applies.
                let coll = colls.get(idx).copied().unwrap_or_default();
                keys.push((idx, term.descending, term.nulls_first, coll));
            }
            result.rows.sort_by(|a, b| {
                for (idx, desc, nf, coll) in &keys {
                    let ord = cmp_order(&a[*idx], &b[*idx], *desc, *nf, *coll);
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }
        let offset = match &sel.offset {
            Some(e) => must_be_int(eval::eval(
                e,
                &EvalCtx::rowless(params).with_subqueries(self),
            )?)?
            .max(0) as usize,
            None => 0,
        };
        // A negative LIMIT means "no limit" in SQLite (OFFSET still applies).
        let limit = match &sel.limit {
            Some(e) => {
                let n = must_be_int(eval::eval(
                    e,
                    &EvalCtx::rowless(params).with_subqueries(self),
                )?)?;
                if n < 0 {
                    None
                } else {
                    Some(n as usize)
                }
            }
            None => None,
        };
        if offset > 0 {
            result.rows.drain(0..offset.min(result.rows.len()));
        }
        if let Some(n) = limit {
            result.rows.truncate(n);
        }
        Ok(())
    }

    /// Compute every window function in `sel` over `rows`, append each result as
    /// a synthetic column on `columns`/`rows`, and return a rewritten `SELECT`
    /// whose projection/ORDER BY reference those columns.
    fn apply_windows(
        &self,
        sel: &Select,
        columns: &mut Vec<ColumnInfo>,
        rows: &mut [InputRow],
        params: &Params,
    ) -> Result<Select> {
        let wins = window::collect_window_exprs(sel);
        let mut new_sel = sel.clone();
        for (k, wexpr) in wins.iter().enumerate() {
            // Resolve `OVER name` against the query's WINDOW definitions, then
            // compute with the resolved spec (but replace the original node).
            let resolved = resolve_window_ref(wexpr, &sel.window_defs)?;
            let values = self.compute_window(&resolved, columns, rows, params)?;
            let col_name = alloc::format!("__win{k}");
            columns.push(ColumnInfo {
                name: col_name.clone(),
                table: String::new(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            });
            for (row, v) in rows.iter_mut().zip(values) {
                row.values.push(v);
            }
            let repl = Expr::Column {
                table: None,
                column: col_name,
            };
            window::replace_window_expr(&mut new_sel, wexpr, &repl);
        }
        Ok(new_sel)
    }

    /// Compute one window function across all `rows`, returning a value per row
    /// (aligned with `rows`).
    fn compute_window(
        &self,
        wexpr: &Expr,
        columns: &[ColumnInfo],
        rows: &[InputRow],
        params: &Params,
    ) -> Result<Vec<Value>> {
        let Expr::Function {
            name,
            distinct,
            args,
            star,
            filter,
            over: Some(spec),
            ..
        } = wexpr
        else {
            return Err(Error::Error("not a window function".into()));
        };
        // SQLite rejects DISTINCT in a window function.
        if *distinct {
            return Err(Error::Error(
                "DISTINCT is not supported for window functions".into(),
            ));
        }
        let lname = name.to_ascii_lowercase();
        let n = rows.len();

        // Per-row partition keys, order keys, argument values, and FILTER mask.
        let mut part_keys: Vec<Vec<Value>> = Vec::with_capacity(n);
        let mut ord_keys: Vec<Vec<Value>> = Vec::with_capacity(n);
        let mut arg_vals: Vec<Vec<Value>> = Vec::with_capacity(n);
        let mut passes: Vec<bool> = Vec::with_capacity(n);
        for r in rows {
            let ctx = r.ctx(columns, params).with_subqueries(self);
            part_keys.push(
                spec.partition_by
                    .iter()
                    .map(|e| eval::eval(e, &ctx))
                    .collect::<Result<_>>()?,
            );
            ord_keys.push(
                spec.order_by
                    .iter()
                    .map(|t| eval::eval(&t.expr, &ctx))
                    .collect::<Result<_>>()?,
            );
            arg_vals.push(
                args.iter()
                    .map(|e| eval::eval(e, &ctx))
                    .collect::<Result<_>>()?,
            );
            // FILTER (WHERE …) restricts which rows the aggregate sees.
            passes.push(match filter {
                Some(pred) => eval::truth(&eval::eval(pred, &ctx)?) == Some(true),
                None => true,
            });
        }
        let descending: Vec<bool> = spec.order_by.iter().map(|t| t.descending).collect();

        // Partition rows by partition key, preserving first-seen order.
        let mut partitions: Vec<Vec<usize>> = Vec::new();
        let mut part_of: Vec<usize> = Vec::new();
        for i in 0..n {
            let p = partitions
                .iter()
                .position(|members| rows_equal(&part_keys[members[0]], &part_keys[i]));
            match p {
                Some(idx) => {
                    partitions[idx].push(i);
                    part_of.push(idx);
                }
                None => {
                    part_of.push(partitions.len());
                    partitions.push(alloc::vec![i]);
                }
            }
        }

        let mut result = alloc::vec![Value::Null; n];
        for members in &partitions {
            // Order the partition's rows (stable).
            let mut ordered = members.clone();
            ordered.sort_by(|&a, &b| cmp_keys(&ord_keys[a], &ord_keys[b], &descending));
            self.fill_window_partition(
                &lname,
                *star,
                &ordered,
                &ord_keys,
                &arg_vals,
                &passes,
                spec,
                &mut result,
            )?;
        }
        Ok(result)
    }

    /// Fill `result` for one ordered partition `ordered` (indices into the row
    /// arrays), honoring `spec`'s frame (or the default frame).
    #[allow(clippy::too_many_arguments)]
    fn fill_window_partition(
        &self,
        lname: &str,
        star: bool,
        ordered: &[usize],
        ord_keys: &[Vec<Value>],
        arg_vals: &[Vec<Value>],
        passes: &[bool],
        spec: &WindowSpec,
        result: &mut [Value],
    ) -> Result<()> {
        let m = ordered.len();
        // Peer-group id per ordered position (for RANGE/GROUPS frames).
        let mut gid = alloc::vec![0usize; m];
        for q in 1..m {
            gid[q] = gid[q - 1]
                + usize::from(
                    !cmp_keys(&ord_keys[ordered[q - 1]], &ord_keys[ordered[q]], &[]).is_eq(),
                );
        }
        // The single ORDER BY value per ordered position, for RANGE value
        // offsets (`RANGE n PRECEDING/FOLLOWING`, which SQLite restricts to one
        // ordering term), and its direction.
        let ovals: Vec<Value> = if spec.order_by.len() == 1 {
            ordered
                .iter()
                .map(|&i| ord_keys[i].first().cloned().unwrap_or(Value::Null))
                .collect()
        } else {
            Vec::new()
        };
        let desc = spec.order_by.first().map(|t| t.descending).unwrap_or(false);
        // The frame's EXCLUDE clause (default NO OTHERS).
        let exclude = spec
            .frame
            .as_ref()
            .map(|f| f.exclude)
            .unwrap_or(FrameExclude::NoOthers);
        // Ranking values per ordered position.
        for p in 0..m {
            let idx = ordered[p];
            let (fstart, fend) = frame_bounds(p, m, &gid, spec, &ovals, desc);
            // Positions of the frame after applying EXCLUDE.
            let fpos: Vec<usize> = (fstart..fend)
                .filter(|&k| match exclude {
                    FrameExclude::NoOthers => true,
                    FrameExclude::CurrentRow => k != p,
                    FrameExclude::Group => gid[k] != gid[p],
                    FrameExclude::Ties => gid[k] != gid[p] || k == p,
                })
                .collect();
            let val = match lname {
                "row_number" => Value::Integer(p as i64 + 1),
                "rank" => {
                    // 1 + number of strictly-preceding rows by order key.
                    let mut r = p;
                    while r > 0 && cmp_keys(&ord_keys[ordered[r - 1]], &ord_keys[idx], &[]).is_eq()
                    {
                        r -= 1;
                    }
                    Value::Integer(r as i64 + 1)
                }
                "dense_rank" => {
                    let mut dr = 1i64;
                    for q in 1..=p {
                        if !cmp_keys(&ord_keys[ordered[q - 1]], &ord_keys[ordered[q]], &[]).is_eq()
                        {
                            dr += 1;
                        }
                    }
                    Value::Integer(dr)
                }
                "percent_rank" => {
                    // (rank - 1) / (rows - 1); 0 for a single-row partition.
                    let mut r = p;
                    while r > 0 && cmp_keys(&ord_keys[ordered[r - 1]], &ord_keys[idx], &[]).is_eq()
                    {
                        r -= 1;
                    }
                    if m > 1 {
                        Value::Real(r as f64 / (m - 1) as f64)
                    } else {
                        Value::Real(0.0)
                    }
                }
                "cume_dist" => {
                    // (# rows ordered <= current, incl. peers) / rows.
                    let mut last = p;
                    while last + 1 < m
                        && cmp_keys(&ord_keys[idx], &ord_keys[ordered[last + 1]], &[]).is_eq()
                    {
                        last += 1;
                    }
                    Value::Real((last + 1) as f64 / m as f64)
                }
                "ntile" => {
                    let buckets = arg_vals[idx].first().map(eval::to_i64).unwrap_or(1).max(1);
                    Value::Integer(ntile_bucket(p, m, buckets))
                }
                "lag" | "lead" => {
                    let offset = arg_vals[idx].get(1).map(eval::to_i64).unwrap_or(1);
                    let default = arg_vals[idx].get(2).cloned().unwrap_or(Value::Null);
                    let target = if lname == "lag" {
                        p as i64 - offset
                    } else {
                        p as i64 + offset
                    };
                    if target >= 0 && (target as usize) < m {
                        arg_vals[ordered[target as usize]]
                            .first()
                            .cloned()
                            .unwrap_or(Value::Null)
                    } else {
                        default
                    }
                }
                "first_value" => fpos
                    .first()
                    .and_then(|&k| arg_vals[ordered[k]].first().cloned())
                    .unwrap_or(Value::Null),
                "last_value" => fpos
                    .last()
                    .and_then(|&k| arg_vals[ordered[k]].first().cloned())
                    .unwrap_or(Value::Null),
                "nth_value" => {
                    let nth = arg_vals[idx].get(1).map(eval::to_i64).unwrap_or(1);
                    // nth row within the (post-EXCLUDE) frame (1-based).
                    if nth >= 1 {
                        fpos.get((nth - 1) as usize)
                            .and_then(|&k| arg_vals[ordered[k]].first().cloned())
                            .unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                // Aggregate windows over the frame (honoring any FILTER mask).
                _ => {
                    let frame: Vec<&Vec<Value>> = fpos
                        .iter()
                        .filter(|&&k| passes[ordered[k]])
                        .map(|&k| &arg_vals[ordered[k]])
                        .collect();
                    window_aggregate(lname, star, &frame)?
                }
            };
            result[idx] = val;
        }
        Ok(())
    }

    /// The collating sequence to apply to each `ORDER BY` term (an explicit
    /// `COLLATE`, else the underlying column's collation, else `BINARY`).
    fn order_collations(
        &self,
        sel: &Select,
        columns: &[ColumnInfo],
        params: &Params,
    ) -> Vec<crate::value::Collation> {
        let ctx = row_ctx(&[], columns, None, params);
        sel.order_by
            .iter()
            .map(|t| eval::key_collation(&t.expr, &ctx))
            .collect()
    }

    /// The collation of each projected output column (a column's collation, an
    /// explicit `COLLATE`, else `BINARY`). Wildcards expand to the source columns.
    fn output_collations(
        &self,
        sel: &Select,
        columns: &[ColumnInfo],
        params: &Params,
    ) -> Vec<crate::value::Collation> {
        let ctx = row_ctx(&[], columns, None, params);
        let mut out = Vec::new();
        for col in &sel.columns {
            match col {
                ResultColumn::Expr { expr, .. } => out.push(eval::key_collation(expr, &ctx)),
                ResultColumn::Wildcard => {
                    out.extend(columns.iter().map(|c| c.collation));
                }
                ResultColumn::TableWildcard(t) => out.extend(
                    columns
                        .iter()
                        .filter(|c| c.table.eq_ignore_ascii_case(t))
                        .map(|c| c.collation),
                ),
            }
        }
        out
    }

    /// When a query's sole `ORDER BY` term is the rowid / INTEGER PRIMARY KEY of
    /// a single plain table that is scanned in full (no `WHERE`, no grouping,
    /// aggregate, window, or `DISTINCT`), the table b-tree already yields rows in
    /// rowid order — so the sort is redundant. Returns `Some(descending)` in that
    /// case (the caller reverses for `DESC`), else `None` (sort normally). Shared
    /// by `run_core` and `eqp_access` so execution and `EXPLAIN QUERY PLAN` agree.
    fn rowid_ordered_scan(&self, sel: &Select) -> Option<bool> {
        let from = sel.from.as_ref()?;
        if !from.joins.is_empty() {
            return None;
        }
        let t = &from.first;
        if t.subquery.is_some() || t.tvf_args.is_some() || t.schema.is_some() {
            return None;
        }
        if sel.where_clause.is_some()
            || !sel.group_by.is_empty()
            || sel.having.is_some()
            || sel.distinct
            || sel.order_by.len() != 1
        {
            return None;
        }
        if self.has_aggregate(sel) || window::has_window(sel) {
            return None;
        }
        // The single ORDER BY term must be a plain (un-COLLATE'd) reference to the
        // rowid or the INTEGER PRIMARY KEY column of this table.
        let term = &sel.order_by[0];
        let (tbl, col) = match &term.expr {
            Expr::Column { table, column } => (table.as_deref(), column.as_str()),
            _ => return None,
        };
        // A CTE/view of the same name is not a rowid table scan.
        if self.lookup_cte(&t.name, None).is_some() || self.is_view(&t.name) {
            return None;
        }
        let label = t.alias.as_deref().unwrap_or(&t.name);
        if tbl.is_some_and(|tn| !tn.eq_ignore_ascii_case(label)) {
            return None;
        }
        let meta = self.table_meta(&t.name, t.alias.as_deref()).ok()?;
        if meta.without_rowid {
            return None;
        }
        let shadowed = meta
            .columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(col));
        let is_rowid_alias = matches!(
            col.to_ascii_lowercase().as_str(),
            "rowid" | "_rowid_" | "oid"
        ) && !shadowed;
        let is_ipk = meta
            .ipk
            .is_some_and(|i| meta.columns[i].name.eq_ignore_ascii_case(col));
        if is_rowid_alias || is_ipk {
            Some(term.descending)
        } else {
            None
        }
    }

    /// The secondary-index analogue of [`rowid_ordered_scan`]: when the same
    /// single-table full-scan shape has its sole `ORDER BY` term as a plain
    /// column that is the leading column of a full (non-partial, non-expression)
    /// index whose collation matches the column's, scanning that index in key
    /// order yields rows in `ORDER BY` order. Returns `(index name, root,
    /// collations, descending)`. NULLs sort first in the index (ascending),
    /// matching `ORDER BY col ASC`; reversing for `DESC` puts them last, matching
    /// `ORDER BY col DESC` — so both directions are exact.
    fn order_index_scan(&self, sel: &Select) -> Option<OrderIndexScan> {
        let from = sel.from.as_ref()?;
        if !from.joins.is_empty() {
            return None;
        }
        let t = &from.first;
        if t.subquery.is_some() || t.tvf_args.is_some() || t.schema.is_some() {
            return None;
        }
        if sel.where_clause.is_some()
            || !sel.group_by.is_empty()
            || sel.having.is_some()
            || sel.distinct
            || sel.order_by.is_empty()
        {
            return None;
        }
        if self.has_aggregate(sel) || window::has_window(sel) {
            return None;
        }
        if self.lookup_cte(&t.name, None).is_some() || self.is_view(&t.name) {
            return None;
        }
        let label = t.alias.as_deref().unwrap_or(&t.name);
        let meta = self.table_meta(&t.name, t.alias.as_deref()).ok()?;
        if meta.without_rowid {
            return None;
        }
        // Resolve every `ORDER BY` term to a plain table column. A secondary index
        // (stored ascending; reversed for a leading DESC) walks its columns in ONE
        // direction, so it satisfies a uniform leading PREFIX of the ORDER BY;
        // trailing terms that change direction are sorted by the caller (`sorted_
        // suffix`). The default-NULLs walk can't honour an explicit `NULLS
        // FIRST`/`LAST`, and a `COLLATE`/non-column term isn't a plain column —
        // both still disqualify the scan entirely.
        let descending = sel.order_by[0].descending;
        let mut cols: Vec<usize> = Vec::with_capacity(sel.order_by.len());
        let mut uniform_prefix = 0usize;
        let mut prefix_open = true;
        for term in &sel.order_by {
            if term.nulls_first.is_some() {
                return None;
            }
            let (tbl, col_name) = match &term.expr {
                Expr::Column { table, column } => (table.as_deref(), column.as_str()),
                _ => return None,
            };
            if tbl.is_some_and(|tn| !tn.eq_ignore_ascii_case(label)) {
                return None;
            }
            let col = meta
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name))?;
            cols.push(col);
            if prefix_open && term.descending == descending {
                uniform_prefix += 1;
            } else {
                prefix_open = false;
            }
        }
        let sorted_suffix = sel.order_by.len() - uniform_prefix;
        // A lone rowid/IPK term is the `rowid_ordered_scan` case.
        if cols.len() == 1 && meta.ipk == Some(cols[0]) {
            return None;
        }
        // A full index whose leading columns are exactly `cols` (in order), each
        // with the column's own collation (so index order == ORDER BY order for
        // the uniform prefix). When the ORDER BY is fully uniform (`sorted_suffix
        // == 0`) the walk needs no sort; a mixed-direction ORDER BY is taken only
        // for the NON-covering case (the covered one is `covering_scan` +
        // `scan_order_prefix`, which already reads in order).
        for idx in self.indexes_of(&t.name).ok()? {
            if idx.partial.is_some() || idx.key_exprs.is_some() {
                continue;
            }
            if idx.cols.len() < cols.len() || idx.cols[..cols.len()] != cols[..] {
                continue;
            }
            let coll_ok = cols
                .iter()
                .enumerate()
                .all(|(i, &c)| idx.collations[i] == meta.columns[c].collation);
            if !coll_ok {
                continue;
            }
            let covering = self.index_covers_query(sel, &meta, &idx.cols);
            if sorted_suffix > 0 && covering {
                continue;
            }
            return Some(OrderIndexScan {
                name: idx.name,
                root: idx.root,
                colls: idx.collations,
                cols: idx.cols,
                descending,
                covering,
                sorted_suffix,
            });
        }
        None
    }

    /// For a no-`WHERE` query whose access is a covering-index scan
    /// ([`covering_scan`]) but whose `ORDER BY` is NOT fully satisfied by that
    /// walk (mixed directions), the number of LEADING `ORDER BY` terms the index
    /// already yields in order. The walk direction is fixed by the first term;
    /// each further term must stay in that direction and continue matching the
    /// index's columns/collations, else the prefix ends there. sqlite sorts only
    /// the remaining terms — "USE TEMP B-TREE FOR LAST n TERMS OF ORDER BY". Zero
    /// when no covering scan applies or the first term already breaks.
    fn scan_order_prefix(&self, sel: &Select, params: &Params) -> usize {
        if sel.order_by.is_empty() {
            return 0;
        }
        let Some(from) = sel.from.as_ref() else {
            return 0;
        };
        if !from.joins.is_empty() {
            return 0;
        }
        let Ok(meta) = self.table_meta(&from.first.name, from.first.alias.as_deref()) else {
            return 0;
        };
        // The index `covering_scan` reads from (its choice must match the EQP).
        let Some((name, _, _)) = self.covering_scan(sel, &meta, params) else {
            return 0;
        };
        let Ok(indexes) = self.indexes_of(&from.first.name) else {
            return 0;
        };
        let Some(idx) = indexes
            .into_iter()
            .find(|i| i.name.eq_ignore_ascii_case(&name))
        else {
            return 0;
        };
        let label = from.first.alias.as_deref().unwrap_or(&from.first.name);
        let backward = sel.order_by[0].descending;
        let mut k = 0usize;
        for (i, term) in sel.order_by.iter().enumerate() {
            if i >= idx.cols.len() || term.nulls_first.is_some() || term.descending != backward {
                break;
            }
            let (tbl, col_name) = match &term.expr {
                Expr::Column { table, column } => (table.as_deref(), column.as_str()),
                _ => break,
            };
            if tbl.is_some_and(|tn| !tn.eq_ignore_ascii_case(label)) {
                break;
            }
            let Some(col) = meta
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name))
            else {
                break;
            };
            if col != idx.cols[i] || idx.collations[i] != meta.columns[col].collation {
                break;
            }
            k += 1;
        }
        k
    }

    /// Covering check for a WHERE-driven *seek* (B2b, seek case): on top of
    /// [`index_covers_query`](Self::index_covers_query) (result columns + `ORDER
    /// BY`), every column the `WHERE` clause references must also be covered by
    /// `idx_cols` or be the rowid. The seek's own index column is covered by
    /// construction, but a residual predicate on some *other* column (e.g.
    /// `WHERE c=5 AND b>0`) would still need the table unless that column is in
    /// the index too. Conservative: any construct whose referenced columns can't
    /// be enumerated (a subquery/`EXISTS`/`IN (SELECT …)`) makes this `false`, so
    /// the caller falls back to the always-correct table-fetch path.
    fn seek_index_covers(
        &self,
        sel: &Select,
        meta: &TableMeta,
        idx_cols: &[usize],
        where_expr: &Expr,
    ) -> bool {
        if !self.index_covers_query(sel, meta, idx_cols) {
            return false;
        }
        where_cols_covered(where_expr, meta, idx_cols)
    }

    /// Build the input rows of a covering seek by walking the chosen index and
    /// keeping every record (a superset — `run_core` re-applies the full `WHERE`,
    /// so the seek's own predicate filters out non-matching keys). Each record is
    /// `(indexed col values…, rowid)`; indexed columns are mapped onto their table
    /// positions and the rowid fills the `INTEGER PRIMARY KEY` column, exactly as
    /// the ordered covering scan does. Reads only the index b-tree — never the
    /// table.
    fn covering_seek_rows(
        &self,
        meta: &TableMeta,
        root: u32,
        idx_cols: &[usize],
    ) -> Result<Vec<InputRow>> {
        let src = self.backend.source();
        let encoding = src.header().text_encoding;
        let mut icur = IndexCursor::new(src, root);
        let mut out = Vec::new();
        while let Some(payload) = icur.next()? {
            let rec = decode_record(&payload, encoding)?;
            let rowid = match rec.get(idx_cols.len()) {
                Some(Value::Integer(r)) => *r,
                _ => return Err(Error::Corrupt("index record missing rowid".into())),
            };
            let mut values = alloc::vec![Value::Null; meta.columns.len()];
            for (i, &mc) in idx_cols.iter().enumerate() {
                values[mc] = rec[i].clone();
            }
            if let Some(ipk) = meta.ipk {
                values[ipk] = Value::Integer(rowid);
            }
            out.push(InputRow {
                values,
                rowid: Some(rowid),
            });
        }
        Ok(out)
    }

    /// Conservative covering check (B2): every column the query references
    /// (result columns + `ORDER BY`) is an indexed column or the rowid, which is
    /// present in every index record. Returns `false` on anything it cannot prove
    /// covered — an expression/function/subquery result column, a wildcard over a
    /// non-covered column, or any generated column on the table.
    fn index_covers_query(&self, sel: &Select, meta: &TableMeta, idx_cols: &[usize]) -> bool {
        if meta.generated.iter().any(|g| g.is_some()) {
            return false;
        }
        let covered = |ci: usize| idx_cols.contains(&ci) || meta.ipk == Some(ci);
        let col_ok = |expr: &Expr| -> bool {
            match expr {
                Expr::Column { column, .. } => match meta
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(column))
                {
                    Some(ci) => covered(ci),
                    None => matches!(
                        column.to_ascii_lowercase().as_str(),
                        "rowid" | "_rowid_" | "oid"
                    ),
                },
                _ => false,
            }
        };
        for rc in &sel.columns {
            match rc {
                ResultColumn::Wildcard | ResultColumn::TableWildcard(_) => {
                    if !(0..meta.columns.len()).all(covered) {
                        return false;
                    }
                }
                ResultColumn::Expr { expr, .. } => {
                    if !col_ok(expr) {
                        return false;
                    }
                }
            }
        }
        sel.order_by.iter().all(|t| col_ok(&t.expr))
    }

    /// Thorough covering test for a *full-table covering scan*: every column the
    /// query references anywhere — result projection (including aggregate
    /// arguments), `GROUP BY`, `HAVING`, `ORDER BY`, and `WHERE` — is held by
    /// `idx_cols` or is the rowid. Conservative: a wildcard over an uncovered
    /// column, a generated column, a window function, or a subquery makes it
    /// `false`. Unlike [`index_covers_query`](Self::index_covers_query) (plain
    /// projections only) this recurses through function calls, so an aggregate
    /// like `count(*)` / `sum(b)` over covered columns qualifies.
    fn query_cols_covered(&self, sel: &Select, meta: &TableMeta, idx_cols: &[usize]) -> bool {
        if meta.generated.iter().any(|g| g.is_some()) {
            return false;
        }
        let covered_all =
            (0..meta.columns.len()).all(|ci| idx_cols.contains(&ci) || meta.ipk == Some(ci));
        for rc in &sel.columns {
            match rc {
                ResultColumn::Wildcard | ResultColumn::TableWildcard(_) => {
                    if !covered_all {
                        return false;
                    }
                }
                ResultColumn::Expr { expr, .. } => {
                    if !where_cols_covered(expr, meta, idx_cols) {
                        return false;
                    }
                }
            }
        }
        sel.group_by
            .iter()
            .all(|e| where_cols_covered(e, meta, idx_cols))
            && sel
                .having
                .as_ref()
                .is_none_or(|h| where_cols_covered(h, meta, idx_cols))
            && sel
                .order_by
                .iter()
                .all(|t| where_cols_covered(&t.expr, meta, idx_cols))
            && sel
                .where_clause
                .as_ref()
                .is_none_or(|w| where_cols_covered(w, meta, idx_cols))
    }

    /// Choose a full secondary index to satisfy a query by a *covering scan* —
    /// reading every needed column from the index instead of the table — when no
    /// `WHERE` seek and no ORDER-BY index walk applies. Restricted to the
    /// no-`WHERE` case so no seek competes for the plan (keeping `eqp_select` and
    /// `run_core` trivially in lockstep), to ordinary rowid tables, and — like
    /// [`count_covering_index`](Self::count_covering_index) — to the *unambiguous*
    /// case of **exactly one** covering index, so the chosen name matches sqlite
    /// without replicating its cost-based tie-break. Returns `(name, root, cols)`.
    fn covering_scan(
        &self,
        sel: &Select,
        meta: &TableMeta,
        params: &Params,
    ) -> Option<(String, u32, Vec<usize>)> {
        let from = sel.from.as_ref()?;
        if !from.joins.is_empty() {
            return None;
        }
        let t = &from.first;
        if t.subquery.is_some() || t.tvf_args.is_some() || t.schema.is_some() {
            return None;
        }
        if sel.where_clause.is_some() || window::has_window(sel) || meta.without_rowid {
            return None;
        }
        if self.lookup_cte(&t.name, None).is_some() || self.is_view(&t.name) {
            return None;
        }
        // If the ORDER BY is already satisfied by a scan's natural order — the
        // rowid order of a table scan (`rowid_ordered_scan`) or an index walk
        // (`order_index_scan`) — leave it alone. A covering scan reads in index
        // order, which would silently break a `rowid_ordered_scan` that assumed
        // the rows arrive in rowid order (and the ordered-index case already
        // renders as covering).
        if self.order_satisfied_by_scan(sel, params).is_some() {
            return None;
        }
        let mut covering = self.indexes_of(&t.name).ok()?.into_iter().filter(|idx| {
            idx.partial.is_none()
                && idx.key_exprs.is_none()
                && self.query_cols_covered(sel, meta, &idx.cols)
        });
        let chosen = covering.next()?;
        // Ambiguous (two or more covering indexes): keep the plain scan rather
        // than guess which one sqlite's cost model would pick.
        if covering.next().is_some() {
            return None;
        }
        Some((chosen.name, chosen.root, chosen.cols))
    }

    /// `SELECT count(*) FROM <single rowid table>` can be answered by counting a
    /// full secondary index's entries instead of scanning the table — a full,
    /// non-partial index has exactly one entry per table row, and its b-tree is
    /// usually smaller (B2b). This returns `Some((index name, root))` only in the
    /// unambiguous case so execution and `EXPLAIN QUERY PLAN` agree:
    ///
    /// * the query is exactly one bare `count(*)` projection — no `WHERE`,
    ///   `GROUP BY`, `HAVING`, `DISTINCT`, `ORDER BY`, joins, subquery, or TVF;
    /// * the source is an ordinary rowid table (not `WITHOUT ROWID`, view, or CTE);
    /// * the table has **exactly one** full (non-partial, non-expression)
    ///   secondary index, so the chosen name is unambiguous and matches sqlite.
    ///
    /// Zero or multiple such indexes → `None` (fall back to the plain `SCAN t`),
    /// never guessing. Shared by `run_core` and `eqp_select`.
    fn count_covering_index(&self, sel: &Select) -> Option<(String, u32)> {
        let from = sel.from.as_ref()?;
        if !from.joins.is_empty() {
            return None;
        }
        let t = &from.first;
        if t.subquery.is_some() || t.tvf_args.is_some() || t.schema.is_some() {
            return None;
        }
        if sel.where_clause.is_some()
            || !sel.group_by.is_empty()
            || sel.having.is_some()
            || sel.distinct
            || !sel.order_by.is_empty()
        {
            return None;
        }
        if window::has_window(sel) {
            return None;
        }
        // The projection must be exactly a single bare `count(*)`.
        if sel.columns.len() != 1 {
            return None;
        }
        let ResultColumn::Expr { expr, .. } = &sel.columns[0] else {
            return None;
        };
        match expr {
            Expr::Function {
                name,
                distinct: false,
                star: true,
                filter: None,
                over: None,
                ..
            } if name.eq_ignore_ascii_case("count") => {}
            _ => return None,
        }
        // The source must be an ordinary rowid table (not a view or CTE).
        if self.lookup_cte(&t.name, None).is_some() || self.is_view(&t.name) {
            return None;
        }
        let meta = self.table_meta(&t.name, t.alias.as_deref()).ok()?;
        if meta.without_rowid {
            return None;
        }
        // Exactly one full (non-partial, non-expression) secondary index.
        let mut chosen: Option<(String, u32)> = None;
        for idx in self.indexes_of(&t.name).ok()? {
            if idx.partial.is_some() || idx.key_exprs.is_some() {
                continue;
            }
            if chosen.is_some() {
                return None; // ambiguous: more than one candidate
            }
            chosen = Some((idx.name, idx.root));
        }
        chosen
    }

    /// Whether a single-table scan already yields rows in the query's `ORDER BY`
    /// order (so `run_core` can skip the sort, reversing for `DESC`). Combines the
    /// rowid/IPK and secondary-index cases; shared with `eqp_access`.
    fn order_satisfied_by_scan(&self, sel: &Select, params: &Params) -> Option<bool> {
        if let Some(d) = self.rowid_ordered_scan(sel) {
            return Some(d);
        }
        if let Some(s) = self.order_index_scan(sel) {
            // A mixed-direction walk only orders the leading prefix; the caller
            // still sorts, so the ORDER BY is not fully satisfied by the scan.
            if s.sorted_suffix == 0 {
                return Some(s.descending);
            }
        }
        // A `WHERE` seek that walks an index in key order satisfies the ORDER BY
        // when *every* term matches the walked columns (B0b-iii).
        match self.seek_order_prefix(sel, params) {
            Some((k, descending)) if k == sel.order_by.len() => Some(descending),
            _ => None,
        }
    }

    /// How many leading `ORDER BY` terms a `WHERE` seek already produces in order,
    /// and the walk direction — the shared core of B0b-iii (full match → skip the
    /// sort) and the partial-sort EXPLAIN label. A seek walks its index in key
    /// order, so the rows arrive ordered by the index columns that follow any
    /// equality prefix; this returns `(k, descending)` where `k` of the ORDER BY
    /// terms match that walk (uniform direction, matching collation, default
    /// NULLs). `k == order_by.len()` means no sort is needed; `0 < k < n` is a
    /// partial sort. Returns `None` when no unambiguous seek applies.
    ///
    /// Mirrors `try_index_lookup` / `try_index_range`'s index choice conservatively
    /// so it never claims an order the executor will not produce: an equality seek
    /// needs exactly one plain secondary index whose leading column the `WHERE`
    /// constrains by equality (and no rowid equality); a range seek needs no column
    /// equality at all (so `try_index_lookup` declines), no partial/expression
    /// index on the table, no range on the rowid, and exactly one plain secondary
    /// index whose leading column is range-constrained. Any looseness only mislabels
    /// EXPLAIN (the sort still runs), and the ORDER-BY differential corpus catches it.
    fn seek_order_prefix(&self, sel: &Select, params: &Params) -> Option<(usize, bool)> {
        let from = sel.from.as_ref()?;
        if !from.joins.is_empty() {
            return None;
        }
        let t = &from.first;
        if t.subquery.is_some()
            || t.tvf_args.is_some()
            || t.schema.is_some()
            || from.first.index_hint.is_some()
        {
            return None;
        }
        let where_expr = sel.where_clause.as_ref()?;
        if sel.order_by.is_empty()
            || !sel.group_by.is_empty()
            || sel.having.is_some()
            || sel.distinct
            || self.has_aggregate(sel)
            || window::has_window(sel)
        {
            return None;
        }
        if self.lookup_cte(&t.name, None).is_some() || self.is_view(&t.name) {
            return None;
        }
        let label = t.alias.as_deref().unwrap_or(&t.name);
        let meta = self.table_meta(&t.name, t.alias.as_deref()).ok()?;
        if meta.without_rowid {
            return None;
        }
        let indexes = self.indexes_of(&t.name).ok()?;
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
        eqs.retain(|(_, v)| !matches!(v, Value::Null));
        // The columns the chosen seek walks in index-ascending order (those after
        // any equality prefix), with their index collations.
        let (walk_cols, walk_colls): (&[usize], &[crate::value::Collation]) = if !eqs.is_empty() {
            // Equality seek (try_index_lookup). A rowid/IPK equality returns at most
            // one row — a different path; bail to the cheap, correct sort.
            if meta
                .ipk
                .is_some_and(|ipk| eqs.iter().any(|(c, _)| *c == ipk))
            {
                return None;
            }
            let mut seekable = indexes.iter().filter(|idx| {
                idx.partial.is_none()
                    && idx.key_exprs.is_none()
                    && idx
                        .cols
                        .first()
                        .is_some_and(|c| eqs.iter().any(|(col, _)| col == c))
            });
            let idx = seekable.next()?;
            if seekable.next().is_some() {
                return None;
            }
            let prefix = idx
                .cols
                .iter()
                .take_while(|&&c| eqs.iter().any(|(col, _)| *col == c))
                .count();
            (&idx.cols[prefix..], &idx.collations[prefix..])
        } else {
            // Range seek (try_index_range). Guard so the chosen index is exactly the
            // one the executor walks (see the doc comment).
            let mut ranges: alloc::collections::BTreeMap<usize, RangeBound> =
                alloc::collections::BTreeMap::new();
            collect_range_constraints(where_expr, &meta.columns, params, &mut ranges);
            if ranges.is_empty() {
                return None;
            }
            if meta.ipk.is_some_and(|ipk| ranges.contains_key(&ipk)) {
                return None;
            }
            if indexes
                .iter()
                .any(|idx| idx.partial.is_some() || idx.key_exprs.is_some())
            {
                return None;
            }
            let mut seekable = indexes
                .iter()
                .filter(|idx| idx.cols.first().is_some_and(|c| ranges.contains_key(c)));
            let idx = seekable.next()?;
            if seekable.next().is_some() {
                return None;
            }
            (&idx.cols[..], &idx.collations[..])
        };
        // Count the leading ORDER BY terms the walk already produces: each must be a
        // plain column of this table, in the walk's uniform direction (default
        // NULLs), matching the next walked column under its own collation.
        let descending = sel.order_by[0].descending;
        let mut k = 0;
        for term in &sel.order_by {
            if k >= walk_cols.len() || term.descending != descending || term.nulls_first.is_some() {
                break;
            }
            let (tbl, col_name) = match &term.expr {
                Expr::Column { table, column } => (table.as_deref(), column.as_str()),
                _ => break,
            };
            if tbl.is_some_and(|tn| !tn.eq_ignore_ascii_case(label)) {
                break;
            }
            let Some(oc) = meta
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name))
            else {
                break;
            };
            if walk_cols[k] != oc || walk_colls[k] != meta.columns[oc].collation {
                break;
            }
            k += 1;
        }
        Some((k, descending))
    }

    /// The first `match(query, operand)` call in a WHERE clause's `AND`/`OR` tree,
    /// as `(query text, operand column name)`. The operand names either the table
    /// (a table-wide match) or a single column (`col MATCH …`, which scopes the
    /// score to that column).
    #[cfg(feature = "fts5")]
    fn fts5_match_query(&self, expr: &Expr, params: &Params) -> Option<(String, String)> {
        match expr {
            Expr::Function { name, args, .. }
                if name.eq_ignore_ascii_case("match") && args.len() == 2 =>
            {
                let v = eval::eval(&args[0], &eval::EvalCtx::rowless(params)).ok()?;
                let operand = match &args[1] {
                    Expr::Column { column, .. } => column.clone(),
                    _ => return None,
                };
                Some((eval::to_text(&v), operand))
            }
            Expr::Binary { left, right, .. } => self
                .fts5_match_query(left, params)
                .or_else(|| self.fts5_match_query(right, params)),
            Expr::Unary { expr, .. } | Expr::Paren(expr) => self.fts5_match_query(expr, params),
            _ => None,
        }
    }

    /// Build the per-query [`Fts5QueryCtx`] for an FTS5 `MATCH` query over a single
    /// `fts5` table that references `rank`/`bm25()`/`highlight()`, or `None`. The
    /// bm25 corpus is computed (over `input_rows`, the whole scanned corpus) only
    /// when `rank`/`bm25()` is referenced — `highlight()` needs just the query.
    #[cfg(feature = "fts5")]
    fn fts5_query_ctx(
        &self,
        sel: &Select,
        columns: &[ColumnInfo],
        input_rows: &[InputRow],
        params: &Params,
    ) -> Option<Fts5QueryCtx> {
        const AUX: &[&str] = &["rank", "bm25", "highlight", "snippet"];
        const RANK: &[&str] = &["rank", "bm25"];
        if !select_mentions(sel, AUX) {
            return None;
        }
        let from = sel.from.as_ref()?;
        if !from.joins.is_empty()
            || from.first.subquery.is_some()
            || from.first.tvf_args.is_some()
            || from.first.schema.is_some()
        {
            return None;
        }
        // The source must be an `fts5` virtual table.
        let (module, vargs, _) = self.vtab_meta(&from.first.name).ok()?;
        if !module.eq_ignore_ascii_case("fts5") {
            return None;
        }
        // Columns declared `UNINDEXED` are excluded from matching/ranking; `None`
        // when every column is searchable (avoids per-row name checks).
        let arg_refs: Vec<&str> = vargs.iter().map(String::as_str).collect();
        let all = crate::vtab::fts5_indexed_columns(&arg_refs);
        let indexed = (all.len() != columns.len()).then_some(all);
        let tok = crate::vtab::fts5_tok_config(&arg_refs);
        let (query, operand) = self.fts5_match_query(sel.where_clause.as_ref()?, params)?;
        let col_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        // A `col MATCH …` operand scopes the query to that column; a table-wide
        // `t MATCH …` (operand names the table, not a column) does not.
        let scope = col_names
            .iter()
            .find(|n| n.eq_ignore_ascii_case(&operand))
            .cloned();
        // Score the corpus only when ranking is actually referenced.
        let bm25 = select_mentions(sel, RANK).then(|| {
            let docs: Vec<Vec<String>> = input_rows
                .iter()
                .map(|r| r.values.iter().map(eval::to_text).collect())
                .collect();
            let corpus = crate::vtab::fts5_bm25_corpus(
                &query,
                &col_names,
                &docs,
                scope.as_deref(),
                indexed.as_deref(),
                tok,
            );
            let index = input_rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| Some((r.rowid?, i)))
                .collect();
            (corpus, index)
        });
        Some(Fts5QueryCtx {
            col_names,
            query,
            scope,
            indexed,
            tok,
            bm25,
        })
    }

    fn run_core(&self, sel: &Select, params: &Params) -> Result<QueryResult> {
        // Opt-in VDBE fast path (Track B, B7a): when enabled and this block takes
        // no bound parameters, try the experimental engine first and use its
        // result only on success — every unsupported shape, and every error, is
        // left to the tree-walker, which remains the source of truth. The VDBE
        // never alters state, so a failed attempt is side-effect-free. Routing
        // here (per query block) rather than at the whole-query level means each
        // arm of a compound query is accelerated too, while the tree-walker still
        // performs the set combination. Skipped inside a correlated/nested scope
        // (non-empty `outer_scope`): the spike resolves columns by bare name and
        // would mis-resolve an outer-qualified reference to a same-named inner
        // column.
        if self.use_vdbe.get() && self.outer_scope.borrow().is_empty() {
            // No params → run the VDBE on `sel` directly. With params, substitute
            // the explicit (`?N`/`:name`) ones into the compiled expressions so the
            // param-less VDBE can run the query; an anonymous `?` (or no explicit
            // param in those expressions) returns None → fall through.
            let substituted;
            let vsel = if params.positional.is_empty() && params.named.is_empty() {
                Some(sel)
            } else {
                match substitute_params(sel, params) {
                    Some(s) => {
                        substituted = s;
                        Some(&substituted)
                    }
                    None => None,
                }
            };
            if let Some(vsel) = vsel {
                if let Ok(result) = self.run_select_vdbe(vsel) {
                    return Ok(result);
                }
            }
        }
        // Promote `FROM a, b WHERE a.x = b.y` to an explicit join `ON` so the join
        // fold can seek/hash it (the equality stays in WHERE, so results are
        // identical). All later uses of `sel` see the rewritten form.
        let rewritten;
        let sel = match promote_comma_join_ons(sel) {
            Some(r) => {
                rewritten = r;
                &rewritten
            }
            None => sel,
        };
        // `SELECT count(*) FROM t` over a single rowid table with exactly one full
        // secondary index counts that index's entries instead of scanning the
        // table (B2b). Kept in lockstep with `eqp_select` via the shared
        // `count_covering_index` helper so EQP reports `USING COVERING INDEX`.
        if let Some((_, root)) = self.count_covering_index(sel) {
            let mut cur = IndexCursor::new(self.backend.source(), root);
            let mut n = 0i64;
            while cur.next()?.is_some() {
                n += 1;
            }
            let label = self.output_labels(sel, &[]).pop().unwrap_or_default();
            return Ok(QueryResult {
                columns: alloc::vec![label],
                rows: alloc::vec![alloc::vec![Value::Integer(n)]],
            });
        }

        let (mut columns, input_rows) = self.scan_source(sel, params)?;

        // FTS5 relevance: if this query references `rank` / `bm25()` over an `fts5`
        // table, build its query context (and bm25 corpus, if ranked) now and
        // expose it to `rank`/`bm25()`/`highlight()` during projection and ORDER BY.
        // The guard restores any outer query's context (and clears it for a
        // non-FTS5 query) when this scope ends.
        #[cfg(feature = "fts5")]
        let _fts5_rank_guard = Fts5RankGuard {
            conn: self,
            prev: core::mem::replace(
                &mut *self.fts5_rank.borrow_mut(),
                self.fts5_query_ctx(sel, &columns, &input_rows, params),
            ),
        };

        // SQLite lets WHERE/GROUP BY/HAVING reference a SELECT-list alias, with a
        // real column of the same name taking precedence. Rewrite those clauses
        // by substituting each unshadowed alias with its defining expression.
        let alias_rewritten;
        let sel = match alias_substituted(sel, &columns) {
            Some(s) => {
                alias_rewritten = s;
                &alias_rewritten
            }
            None => sel,
        };

        // A positional `GROUP BY` / `ORDER BY` term (an integer literal) must name
        // an output column (1..=ncols); SQLite rejects one out of range. The count
        // is taken after wildcard expansion.
        let ncols = self.output_labels(sel, &columns).len();
        check_positional_terms(&sel.group_by, &sel.order_by, ncols)?;

        // Apply WHERE.
        let mut rows: Vec<InputRow> = Vec::new();
        for r in input_rows {
            if let Some(pred) = &sel.where_clause {
                let ctx = r.ctx(&columns, params).with_subqueries(self);
                if eval::truth(&eval::eval(pred, &ctx)?) != Some(true) {
                    continue;
                }
            }
            rows.push(r);
        }

        // A window function combined with GROUP BY / aggregates: SQLite applies
        // the window *after* grouping (it runs over the post-aggregation rows, and
        // an aggregate inside a window argument or spec is the group's aggregate).
        // `eval_windowed_aggregate` handles grouping, the windows, and projection,
        // returning rows + sort keys just like the other eval paths — so it feeds
        // the same DISTINCT / ORDER BY / LIMIT post-processing below.
        let windowed_agg =
            window::has_window(sel) && (!sel.group_by.is_empty() || self.has_aggregate(sel));

        // Plain window functions (no GROUP BY/aggregate): compute over the
        // post-WHERE rows, append the results as synthetic columns, and rewrite the
        // projection to reference them. Capture the output labels from the ORIGINAL
        // projection first — `apply_windows` rewrites each window call to a `__winN`
        // column reference, which would otherwise name the output column `__winN`
        // instead of its source text (`sum(a) OVER ()`).
        let window_labels = if window::has_window(sel) && !windowed_agg {
            Some(self.output_labels(sel, &columns))
        } else {
            None
        };
        let rewritten;
        let sel = if window::has_window(sel) && !windowed_agg {
            rewritten = self.apply_windows(sel, &mut columns, &mut rows, params)?;
            &rewritten
        } else {
            sel
        };

        let aggregated = !sel.group_by.is_empty() || self.has_aggregate(sel);
        // A HAVING clause requires an aggregate context (a GROUP BY or any
        // aggregate function); SQLite rejects it on a plain query.
        if sel.having.is_some() && !aggregated {
            return Err(Error::Error(
                "HAVING clause on a non-aggregate query".into(),
            ));
        }
        let (mut out_labels, mut out) = if windowed_agg {
            self.eval_windowed_aggregate(sel, &columns, rows, params)?
        } else if aggregated {
            self.eval_aggregated(sel, &columns, rows, params)?
        } else {
            self.eval_simple(sel, &columns, rows, params)?
        };
        // Restore the pre-rewrite labels for a plain windowed query (above).
        if let Some(labels) = window_labels {
            out_labels = labels;
        }

        // DISTINCT (dedupe on output values, preserving first occurrence), each
        // output column compared under its collation.
        if sel.distinct {
            let colls = self.output_collations(sel, &columns, params);
            let mut seen: Vec<Vec<Value>> = Vec::new();
            out.retain(|row| {
                if seen.iter().any(|s| rows_equal_coll(s, &row.values, &colls)) {
                    false
                } else {
                    seen.push(row.values.clone());
                    true
                }
            });
        }

        // ORDER BY. A query already produced in rowid order by the table scan
        // (sole ORDER BY term = rowid / INTEGER PRIMARY KEY) skips the sort —
        // just reversing for DESC — matching sqlite's plain SCAN with no temp
        // b-tree.
        if !sel.order_by.is_empty() {
            match self.order_satisfied_by_scan(sel, params) {
                Some(true) => out.reverse(),
                Some(false) => {}
                None => {
                    let colls = self.order_collations(sel, &columns, params);
                    // Stable sort by the precomputed sort keys, each under its collation.
                    out.sort_by(|a, b| {
                        for (i, term) in sel.order_by.iter().enumerate() {
                            let ord = cmp_order(
                                &a.sort_keys[i],
                                &b.sort_keys[i],
                                term.descending,
                                term.nulls_first,
                                colls[i],
                            );
                            if ord != core::cmp::Ordering::Equal {
                                return ord;
                            }
                        }
                        core::cmp::Ordering::Equal
                    });
                }
            }
        }

        // OFFSET / LIMIT.
        let offset = match &sel.offset {
            Some(e) => must_be_int(eval::eval(
                e,
                &EvalCtx::rowless(params).with_subqueries(self),
            )?)?
            .max(0) as usize,
            None => 0,
        };
        // A negative LIMIT means "no limit" in SQLite (OFFSET still applies).
        let limit = match &sel.limit {
            Some(e) => {
                let n = must_be_int(eval::eval(
                    e,
                    &EvalCtx::rowless(params).with_subqueries(self),
                )?)?;
                if n < 0 {
                    None
                } else {
                    Some(n as usize)
                }
            }
            None => None,
        };
        let mut final_rows: Vec<Vec<Value>> =
            out.into_iter().skip(offset).map(|r| r.values).collect();
        if let Some(n) = limit {
            final_rows.truncate(n);
        }

        Ok(QueryResult {
            columns: out_labels,
            rows: final_rows,
        })
    }

    /// Scan the `FROM` source into column metadata and decoded input rows.
    fn scan_source(
        &self,
        sel: &Select,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<InputRow>)> {
        let Some(from) = &sel.from else {
            // No FROM: a single empty row (e.g. `SELECT 1+1`).
            return Ok((
                Vec::new(),
                alloc::vec![InputRow {
                    values: Vec::new(),
                    rowid: None
                }],
            ));
        };
        if from.joins.is_empty() && from.first.subquery.is_none() && from.first.tvf_args.is_none() {
            // An explicit qualifier picks the database; an unqualified name may be
            // shadowed by a temp table. A non-main database is read by
            // materializing the table through its own backend.
            // `sqlite_temp_master`/`sqlite_temp_schema` read the temp catalog
            // (empty when no temp database exists).
            if from.first.schema.is_none() && is_temp_schema_table(&from.first.name) {
                let alias = from.first.alias.as_deref();
                return match &self.temp_db {
                    Some(_) => self.scan_db_table(DbRef::Temp, "sqlite_master", alias),
                    None => Ok((
                        schema_table_meta(alias.unwrap_or(&from.first.name)).columns,
                        Vec::new(),
                    )),
                };
            }
            // `dbstat`: an eponymous read-only virtual table reporting per-page
            // b-tree storage statistics. A real user table named `dbstat` wins.
            if from.first.schema.is_none()
                && from.first.name.eq_ignore_ascii_case("dbstat")
                && self.schema.table("dbstat").is_none()
            {
                return self.scan_dbstat(from.first.alias.as_deref());
            }
            let db = match from.first.schema.as_deref() {
                Some(_) => self.resolve_db(from.first.schema.as_deref())?,
                // Don't let a temp table shadow a CTE or view of the same name.
                None if self.lookup_cte(&from.first.name, None).is_none()
                    && !self.is_view(&from.first.name) =>
                {
                    self.unqualified_db(&from.first.name)
                }
                None => DbRef::Main,
            };
            if db != DbRef::Main {
                let alias = from.first.alias.as_deref();
                if let Some(r) = self.scan_db_view(db, &from.first.name, alias, params)? {
                    return Ok(r);
                }
                return self.scan_db_table(db, &from.first.name, alias);
            }
        }
        // A table-valued function used as the sole source.
        if from.joins.is_empty()
            && (from.first.tvf_args.is_some() || self.is_pragma_tvf(&from.first))
        {
            let (columns, rows) = self.tvf_rows(&from.first, params)?;
            let input = rows
                .into_iter()
                .map(|values| InputRow {
                    values,
                    rowid: None,
                })
                .collect();
            return Ok((columns, input));
        }
        // A derived-table subquery used as the sole source.
        if from.joins.is_empty() {
            if let Some(sub) = &from.first.subquery {
                let (columns, rows) =
                    self.run_subquery_source(sub, from.first.alias.as_deref(), params)?;
                let input = rows
                    .into_iter()
                    .map(|values| InputRow {
                        values,
                        rowid: None,
                    })
                    .collect();
                return Ok((columns, input));
            }
        }
        // A `WITH` common table expression used as the sole source.
        if from.joins.is_empty() {
            if let Some((columns, rows)) =
                self.lookup_cte(&from.first.name, from.first.alias.as_deref())
            {
                return Ok((columns, rows));
            }
        }
        // A view as the sole source: run its SELECT in place.
        if from.joins.is_empty() {
            if let Some((columns, rows)) =
                self.try_view(&from.first.name, from.first.alias.as_deref(), params)?
            {
                return Ok((columns, rows));
            }
        }
        // A virtual table as the sole source: drain its module's cursor, pushing
        // the query's WHERE constraints into the module (it may restrict what it
        // produces; run_core still re-applies the full WHERE, so this is a
        // superset — never wrong).
        if from.joins.is_empty() && from.first.schema.is_none() {
            if let Some((columns, rows)) = self.try_virtual_table(
                &from.first.name,
                from.first.alias.as_deref(),
                Some((sel, params)),
            )? {
                return Ok((columns, rows));
            }
        }

        // Single-table fast path. Try an index-driven equality lookup first; the
        // full WHERE is still applied by run_core, so the index only needs to
        // return a superset of matching rows.
        if from.joins.is_empty() {
            let first_meta = self.table_meta(&from.first.name, from.first.alias.as_deref())?;
            if first_meta.without_rowid {
                // A leading-PK equality or range seeks the clustered b-tree; else
                // scan.
                if let Some(rows) = self.try_without_rowid_pk_seek(&first_meta, sel, params)? {
                    return Ok((first_meta.columns, rows));
                }
                if let Some(rows) = self.try_without_rowid_pk_range(&first_meta, sel, params)? {
                    return Ok((first_meta.columns, rows));
                }
                if let Some(rows) =
                    self.try_without_rowid_index_seek(&first_meta, &from.first.name, sel, params)?
                {
                    return Ok((first_meta.columns, rows));
                }
                if let Some(rows) =
                    self.try_without_rowid_index_range(&first_meta, &from.first.name, sel, params)?
                {
                    return Ok((first_meta.columns, rows));
                }
                let input_rows = self
                    .scan_without_rowid(&first_meta)?
                    .into_iter()
                    .map(|values| InputRow {
                        values,
                        rowid: None,
                    })
                    .collect();
                return Ok((first_meta.columns, input_rows));
            }
            if let Some(rows) = self.try_index_lookup(&first_meta, &from.first.name, sel, params)? {
                return Ok((first_meta.columns, rows));
            }
            if let Some(rows) = self.try_index_range(&first_meta, &from.first.name, sel, params)? {
                return Ok((first_meta.columns, rows));
            }
            if let Some(rows) = self.try_index_in(&first_meta, &from.first.name, sel, params)? {
                return Ok((first_meta.columns, rows));
            }
            if let Some(rows) = self.try_index_or(&first_meta, &from.first.name, sel, params)? {
                return Ok((first_meta.columns, rows));
            }
            // ORDER BY satisfied by a full secondary index (B0): walk that index
            // in key order, so `run_core` can skip the sort. Must stay in lockstep
            // with `order_satisfied_by_scan`/`eqp_access`. When the index covers
            // every referenced column (B2), build rows from the index records and
            // skip the table b-tree entirely; otherwise fetch each row by rowid.
            if let Some(s) = self.order_index_scan(sel) {
                let src = self.backend.source();
                let encoding = src.header().text_encoding;
                if s.covering {
                    let mut icur = IndexCursor::new(src, s.root);
                    let mut input_rows = Vec::new();
                    while let Some(payload) = icur.next()? {
                        let rec = decode_record(&payload, encoding)?;
                        // The record is `(indexed col values…, rowid)`.
                        let rowid = match rec.get(s.cols.len()) {
                            Some(Value::Integer(r)) => *r,
                            _ => return Err(Error::Corrupt("index record missing rowid".into())),
                        };
                        let mut values = alloc::vec![Value::Null; first_meta.columns.len()];
                        for (i, &mc) in s.cols.iter().enumerate() {
                            values[mc] = rec[i].clone();
                        }
                        if let Some(ipk) = first_meta.ipk {
                            values[ipk] = Value::Integer(rowid);
                        }
                        input_rows.push(InputRow {
                            values,
                            rowid: Some(rowid),
                        });
                    }
                    return Ok((first_meta.columns, input_rows));
                }
                let rowids = crate::btree::index_range_rowids(src, s.root, None, None, &s.colls)?;
                let mut cur = TableCursor::new(src, first_meta.root);
                let mut input_rows = Vec::with_capacity(rowids.len());
                for rid in rowids {
                    if cur.seek(rid)? {
                        let values =
                            self.decode_full_row(&first_meta, rid, &cur.payload()?, encoding)?;
                        input_rows.push(InputRow {
                            values,
                            rowid: Some(rid),
                        });
                    }
                }
                return Ok((first_meta.columns, input_rows));
            }
            // Covering scan (B2): no seek and no ORDER-BY index walk applied, but a
            // full index holds every column the query needs — read the rows from
            // that index instead of the table. `eqp_select` reports the matching
            // `SCAN … USING COVERING INDEX`.
            if let Some((_, root, cols)) = self.covering_scan(sel, &first_meta, params) {
                return Ok((
                    first_meta.columns.clone(),
                    self.covering_seek_rows(&first_meta, root, &cols)?,
                ));
            }
            let input_rows = self
                .scan_table(&first_meta)?
                .into_iter()
                .map(|(rowid, values)| InputRow {
                    values,
                    rowid: Some(rowid),
                })
                .collect();
            return Ok((first_meta.columns, input_rows));
        }

        // Join case: resolve the first source (CTE, view, or table), then fold
        // in joins.
        let (mut columns, mut rows) = self.resolve_join_source(&from.first, params)?;

        // Fold each join in with a nested-loop, evaluating its ON predicate
        // against the columns accumulated so far plus the joined table's.
        for join in &from.joins {
            // Roadmap B1a: when the inner table's join column is its rowid IPK,
            // seek the one inner row by rowid per outer row instead of
            // materializing and nested-looping it. Identical results to the
            // materialize path (the full `ON` is re-evaluated on the seeked row).
            if let Some((outer_col, inner_meta)) = self.rowid_join_seek(join, &columns) {
                let (new_columns, joined) = self.exec_rowid_join_seek(
                    join,
                    &columns,
                    &rows,
                    outer_col,
                    &inner_meta,
                    params,
                )?;
                columns = new_columns;
                rows = joined;
                continue;
            }

            // Roadmap B1a² (index case): when the inner join column is the
            // leading column of a usable secondary index, seek that index per
            // outer row instead of materializing the inner table. A non-unique
            // key may fan out to several inner rows. Identical results to the
            // materialize path (the full `ON` is re-evaluated on each seeked row).
            if let Some((outer_col, inner_meta, idx)) = self.index_join_seek(join, &columns) {
                let (new_columns, joined) = self.exec_index_join_seek(
                    join,
                    &columns,
                    &rows,
                    outer_col,
                    &inner_meta,
                    &idx,
                    params,
                )?;
                columns = new_columns;
                rows = joined;
                continue;
            }

            // WITHOUT ROWID inner table joined on its leading PRIMARY KEY: seek the
            // clustered b-tree per outer row instead of materializing it.
            if let Some((outer_col, inner_meta)) = self.without_rowid_pk_join_seek(join, &columns) {
                let (new_columns, joined) = self.exec_without_rowid_pk_join_seek(
                    join,
                    &columns,
                    &rows,
                    outer_col,
                    &inner_meta,
                    params,
                )?;
                columns = new_columns;
                rows = joined;
                continue;
            }

            let (jcols, jrows) = self.resolve_join_source(&join.table, params)?;

            let left_width = columns.len();
            // `NATURAL` / `USING` join columns, as `(left index, right local
            // index)` pairs: the join matches on equality of these and coalesces
            // each into the single left-side output column. `NATURAL` pairs every
            // commonly-named column; with no common column it degrades to a cross
            // join (empty `pairs`), matching SQLite.
            let pairs: Vec<(usize, usize)> = if join.natural {
                jcols
                    .iter()
                    .enumerate()
                    .filter_map(|(rl, rc)| {
                        columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(&rc.name))
                            .map(|li| (li, rl))
                    })
                    .collect()
            } else if !join.using.is_empty() {
                let mut v = Vec::with_capacity(join.using.len());
                for name in &join.using {
                    let li = columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name));
                    let rl = jcols.iter().position(|c| c.name.eq_ignore_ascii_case(name));
                    match (li, rl) {
                        (Some(li), Some(rl)) => v.push((li, rl)),
                        _ => {
                            return Err(Error::Error(format!(
                            "cannot join using column {name} - column not present in both tables"
                        )))
                        }
                    }
                }
                v
            } else {
                Vec::new()
            };

            let mut new_columns = columns.clone();
            new_columns.extend(jcols.iter().cloned());
            let n_jcols = jcols.len();

            let mut joined: Vec<Vec<Value>> = Vec::new();
            let mut right_matched = alloc::vec![false; jrows.len()];

            // Build a hash index on the joined table when the ON predicate has an
            // equi-join `left.col = right.col`, turning the O(n*m) nested loop into
            // a probe. The full ON is still evaluated on each candidate (the hash
            // only narrows which right rows to test), so semantics are unchanged.
            // `NATURAL`/`USING` joins evaluate their equality directly (below) and
            // use the nested loop.
            let equi = if pairs.is_empty() {
                join.on
                    .as_ref()
                    .and_then(|on| join_equi_cols(on, &new_columns, left_width))
            } else {
                None
            };
            let hash: Option<(usize, alloc::collections::BTreeMap<JoinKey, Vec<usize>>)> = equi
                .map(|(li, ri_local)| {
                    let mut map: alloc::collections::BTreeMap<JoinKey, Vec<usize>> =
                        alloc::collections::BTreeMap::new();
                    for (ri, right) in jrows.iter().enumerate() {
                        for k in join_keys_of(&right[ri_local]) {
                            map.entry(k).or_default().push(ri);
                        }
                    }
                    (li, map)
                });

            for left in &rows {
                let mut matched = false;
                // Right rows to test: the hash candidates (sorted, deduped, so the
                // output order matches the nested loop) or every right row.
                let candidates: Vec<usize> = match &hash {
                    Some((li, map)) => {
                        let mut c: Vec<usize> = Vec::new();
                        for k in join_keys_of(&left[*li]) {
                            if let Some(idxs) = map.get(&k) {
                                c.extend_from_slice(idxs);
                            }
                        }
                        c.sort_unstable();
                        c.dedup();
                        c
                    }
                    None => (0..jrows.len()).collect(),
                };
                for ri in candidates {
                    let right = &jrows[ri];
                    let mut combined = left.clone();
                    combined.extend(right.iter().cloned());
                    let keep = if !pairs.is_empty() {
                        // NATURAL / USING: all join columns must be `=` equal (a
                        // NULL on either side is not a match), each under the left
                        // column's collation.
                        pairs.iter().all(|&(li, rl)| {
                            let coll = new_columns[li].collation;
                            eval::truth(&eval::compare_op(
                                BinaryOp::Eq,
                                &combined[li],
                                &combined[left_width + rl],
                                coll,
                            )) == Some(true)
                        })
                    } else {
                        match &join.on {
                            Some(on) => {
                                let ctx = row_ctx(&combined, &new_columns, None, params);
                                eval::truth(&eval::eval(on, &ctx)?) == Some(true)
                            }
                            None => true, // CROSS / comma join
                        }
                    };
                    if keep {
                        joined.push(combined);
                        matched = true;
                        right_matched[ri] = true;
                    }
                }
                // LEFT/FULL: emit the left row with NULLs when nothing matched.
                if !matched && matches!(join.kind, JoinKind::Left | JoinKind::Full) {
                    let mut combined = left.clone();
                    combined.extend(core::iter::repeat_n(Value::Null, n_jcols));
                    joined.push(combined);
                }
            }
            // RIGHT/FULL: emit each unmatched right row with NULLs for the left.
            if matches!(join.kind, JoinKind::Right | JoinKind::Full) {
                for (ri, right) in jrows.iter().enumerate() {
                    if !right_matched[ri] {
                        let mut combined = alloc::vec![Value::Null; left_width];
                        combined.extend(right.iter().cloned());
                        joined.push(combined);
                    }
                }
            }

            // NATURAL / USING: coalesce each join column into its left output
            // position (`COALESCE(left, right)` — the left value, or the right's
            // when the left side is NULL from an outer join), then drop the right
            // duplicate columns so each join column appears once.
            if !pairs.is_empty() {
                let mut drop: Vec<usize> = pairs.iter().map(|&(_, rl)| left_width + rl).collect();
                drop.sort_unstable();
                drop.dedup();
                for row in &mut joined {
                    for &(li, rl) in &pairs {
                        if matches!(row[li], Value::Null) {
                            row[li] = row[left_width + rl].clone();
                        }
                    }
                    for &d in drop.iter().rev() {
                        row.remove(d);
                    }
                }
                for &d in drop.iter().rev() {
                    new_columns.remove(d);
                }
            }

            columns = new_columns;
            rows = joined;
        }

        let input_rows = rows
            .into_iter()
            .map(|values| InputRow {
                values,
                rowid: None, // ambiguous across joined tables
            })
            .collect();
        Ok((columns, input_rows))
    }

    /// Resolve one table reference in a join to its columns + row values,
    /// consulting the CTE environment before the schema (so a CTE — including a
    /// recursive one — can appear as a join source).
    /// Run a derived-table subquery (`FROM (SELECT …) AS alias`) into column
    /// metadata (labeled with the alias) and row values.
    /// A bare `pragma_<name>` (no parentheses) used as a `FROM` source is the
    /// zero-argument table-valued form of that PRAGMA — unless a real table,
    /// view, or CTE of the same name shadows it.
    fn is_pragma_tvf(&self, tref: &TableRef) -> bool {
        tref.tvf_args.is_none()
            && tref.subquery.is_none()
            && tref.schema.is_none()
            && tref.name.to_ascii_lowercase().starts_with("pragma_")
            && self.lookup_cte(&tref.name, None).is_none()
            && !self.is_view(&tref.name)
            && self.unqualified_db(&tref.name) == DbRef::Main
            && self.schema.table(&tref.name).is_none()
    }

    /// Produce the rows of a table-valued function (`generate_series`, `json_each`,
    /// `json_tree`) used as a `FROM` source.
    fn tvf_rows(
        &self,
        tref: &TableRef,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        let args = tref.tvf_args.as_deref().unwrap_or(&[]);
        let lname = tref.name.to_ascii_lowercase();
        let label = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
        let ctx = EvalCtx::rowless(params).with_subqueries(self);
        let col = |name: &str, affinity| ColumnInfo {
            name: String::from(name),
            table: label.clone(),
            affinity,
            collation: crate::value::Collation::default(),
        };
        match lname.as_str() {
            "generate_series" => {
                if args.is_empty() {
                    return Err(Error::Error("generate_series() requires arguments".into()));
                }
                let nums: Vec<i64> = args
                    .iter()
                    .map(|a| eval::eval(a, &ctx).map(|v| eval::to_i64(&v)))
                    .collect::<Result<_>>()?;
                let start = nums[0];
                let stop = nums.get(1).copied().unwrap_or(start);
                // SQLite's generate_series treats a step of 0 as 1.
                let step = match nums.get(2).copied().unwrap_or(1) {
                    0 => 1,
                    s => s,
                };
                let mut rows = Vec::new();
                if step != 0 {
                    let mut v = start;
                    loop {
                        let in_range = if step > 0 { v <= stop } else { v >= stop };
                        if !in_range {
                            break;
                        }
                        rows.push(alloc::vec![Value::Integer(v)]);
                        match v.checked_add(step) {
                            Some(n) => v = n,
                            None => break, // i64 overflow ends the series
                        }
                    }
                }
                Ok((alloc::vec![col("value", eval::Affinity::Integer)], rows))
            }
            "json_each" | "json_tree" => {
                let columns = alloc::vec![
                    col("key", eval::Affinity::Blob),
                    col("value", eval::Affinity::Blob),
                    col("type", eval::Affinity::Text),
                    col("atom", eval::Affinity::Blob),
                    col("id", eval::Affinity::Integer),
                    col("parent", eval::Affinity::Integer),
                    col("fullkey", eval::Affinity::Text),
                    col("path", eval::Affinity::Text),
                ];
                let Some(doc_arg) = args.first() else {
                    return Err(Error::Error(format!("{lname}() requires a JSON argument")));
                };
                let doc = eval::eval(doc_arg, &ctx)?;
                if matches!(doc, Value::Null) {
                    return Ok((columns, Vec::new()));
                }
                let Some(root) = crate::exec::json::parse(&eval::to_text(&doc)) else {
                    return Err(Error::Error("malformed JSON".into()));
                };
                // An optional second argument is a path to navigate to first; the
                // walk is then rooted at that element (e.g. `json_each(x, '$.a')`
                // iterates `$.a`'s children, with `$.a…` paths). A path that does
                // not resolve yields no rows.
                let (target, root_path) = match args.get(1) {
                    Some(path_arg) => {
                        let p = eval::to_text(&eval::eval(path_arg, &ctx)?);
                        match crate::exec::json::navigate(&root, &p) {
                            Some(sub) => (sub, p),
                            None => return Ok((columns, Vec::new())),
                        }
                    }
                    None => (&root, String::from("$")),
                };
                let mut rows = Vec::new();
                let mut next_id = 0i64;
                if lname == "json_tree" {
                    // The root row carries the path's final component as its key and
                    // its parent path in the `path` column.
                    let (parent_path, key) = split_json_path(&root_path);
                    json_tree_walk(
                        target,
                        key,
                        &root_path,
                        &parent_path,
                        None,
                        &mut next_id,
                        &mut rows,
                    );
                } else {
                    json_each_children(target, &root_path, &mut next_id, &mut rows);
                }
                Ok((columns, rows))
            }
            // `pragma_<name>(arg)` is the table-valued form of a PRAGMA, usable in
            // a FROM clause (e.g. `SELECT name FROM pragma_table_info('t')`).
            pragma if pragma.starts_with("pragma_") => {
                let p = Pragma {
                    name: String::from(&pragma["pragma_".len()..]),
                    value: args.first().cloned(),
                };
                let result = self.run_pragma(&p)?;
                let columns = result
                    .columns
                    .iter()
                    .map(|n| col(n, eval::Affinity::Blob))
                    .collect();
                Ok((columns, result.rows))
            }
            _ => Err(Error::Error(format!(
                "no such table-valued function: {}",
                tref.name
            ))),
        }
    }

    fn run_subquery_source(
        &self,
        select: &Select,
        alias: Option<&str>,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        let result = self.run_select(select, params)?;
        let label = alias.unwrap_or("").to_string();
        // A derived column inherits the affinity AND collation of its origin (a
        // direct column reference, transparent through parens / an explicit
        // `COLLATE`), matching sqlite; an expression column has NONE affinity and
        // BINARY collation. Resolved for a single-base-table subquery; a join /
        // nested subquery / TVF source leaves the conservative NONE/BINARY default.
        let origins = self.subquery_column_origins(select);
        let columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let (affinity, collation) = origins
                    .as_ref()
                    .and_then(|o| o.get(i).copied())
                    .unwrap_or((eval::Affinity::Blob, crate::value::Collation::default()));
                ColumnInfo {
                    name: n.clone(),
                    table: label.clone(),
                    affinity,
                    collation,
                }
            })
            .collect();
        Ok((columns, result.rows))
    }

    /// The `(affinity, collation)` each output column of a single-base-table
    /// subquery inherits from its origin — a direct column reference (through
    /// parens / `COLLATE`) takes its base column's affinity and collation (an
    /// explicit `COLLATE` overrides the collation); any other expression is
    /// `(BLOB, BINARY)`. Returns `None` (caller defaults all to `BLOB`/`BINARY`)
    /// for a compound / join / nested / TVF subquery, or a count mismatch.
    fn subquery_column_origins(&self, select: &Select) -> Option<Vec<ColOrigin>> {
        if !select.compound.is_empty() {
            return None;
        }
        let from = select.from.as_ref()?;
        if !from.joins.is_empty() {
            return None;
        }
        // The single source's named columns, each with its `(affinity, collation)`.
        // A base table reads them from its meta; a nested subquery recurses, so a
        // collation/affinity flows through any depth of single-source derived tables.
        let src = self.named_source_origins(&from.first)?;
        let label = from
            .first
            .alias
            .clone()
            .unwrap_or_else(|| from.first.name.clone());
        let base = |table: Option<&str>, col: &str| -> Option<ColOrigin> {
            if table.is_some_and(|t| !t.eq_ignore_ascii_case(&label)) {
                return None;
            }
            src.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(col))
                .map(|(_, o)| *o)
        };
        fn origin(e: &Expr, base: &dyn Fn(Option<&str>, &str) -> Option<ColOrigin>) -> ColOrigin {
            match e {
                Expr::Paren(inner) => origin(inner, base),
                Expr::Column { table, column } => base(table.as_deref(), column)
                    .unwrap_or((eval::Affinity::Blob, crate::value::Collation::default())),
                Expr::Collate { expr, collation } => {
                    let (aff, base_coll) = origin(expr, base);
                    (
                        aff,
                        crate::value::Collation::parse(collation).unwrap_or(base_coll),
                    )
                }
                _ => (eval::Affinity::Blob, crate::value::Collation::default()),
            }
        }
        let mut out = Vec::new();
        for rc in &select.columns {
            match rc {
                ResultColumn::Wildcard => out.extend(src.iter().map(|(_, o)| *o)),
                ResultColumn::TableWildcard(t) if t.eq_ignore_ascii_case(&label) => {
                    out.extend(src.iter().map(|(_, o)| *o))
                }
                ResultColumn::TableWildcard(_) => return None,
                ResultColumn::Expr { expr, .. } => out.push(origin(expr, &base)),
            }
        }
        Some(out)
    }

    /// A single FROM source's `(name, (affinity, collation))` per column. A base
    /// table reads its meta; a nested subquery recurses through
    /// `subquery_column_origins` (its names from `resolved_view_columns`), so an
    /// inherited affinity/collation flows through nested single-source derived
    /// tables. A view / CTE / TVF / join-or-compound subquery returns `None`.
    fn named_source_origins(&self, tref: &TableRef) -> Option<Vec<(String, ColOrigin)>> {
        if tref.tvf_args.is_some() {
            return None;
        }
        if let Some(sub) = &tref.subquery {
            let names = self.resolved_view_columns(sub)?;
            let origins = self.subquery_column_origins(sub)?;
            if names.len() != origins.len() {
                return None;
            }
            return Some(
                names
                    .into_iter()
                    .zip(origins)
                    .map(|((n, _), o)| (n, o))
                    .collect(),
            );
        }
        // A base table only — a view/CTE source defers to the conservative default.
        self.schema.table(&tref.name)?;
        let meta = self.table_meta(&tref.name, tref.alias.as_deref()).ok()?;
        Some(
            meta.columns
                .iter()
                .map(|c| (c.name.clone(), (c.affinity, c.collation)))
                .collect(),
        )
    }

    /// The single shared decision for the rowid-seek join optimization (roadmap
    /// B1a): when a `JOIN`'s `ON` is a lone equi-join `outer.col = u.ipk` (or the
    /// mirror) whose right side is the inner table `u`'s INTEGER PRIMARY KEY, the
    /// inner row can be fetched by rowid per outer row instead of materializing
    /// and nested-looping `u`. Returns `(outer_col_index, inner_meta)` when it
    /// applies; `None` otherwise (the caller falls back to materialize/hash).
    ///
    /// Used by BOTH the executor (to seek) and the join EQP emitter (to print
    /// `SEARCH … USING INTEGER PRIMARY KEY (rowid=?)` instead of `SCAN`), so the
    /// two never diverge. `left_columns` is the column list accumulated for the
    /// left side so far (its width is where the inner table's columns begin).
    fn rowid_join_seek(
        &self,
        join: &Join,
        left_columns: &[ColumnInfo],
    ) -> Option<(usize, TableMeta)> {
        // Only plain INNER / LEFT joins with a single `ON` equality — never
        // NATURAL / USING / CROSS / RIGHT / FULL.
        if !matches!(join.kind, JoinKind::Inner | JoinKind::Left)
            || join.natural
            || !join.using.is_empty()
        {
            return None;
        }
        let on = join.on.as_ref()?;
        let tref = &join.table;
        // The inner table must be a plain base table in `main`: not a subquery /
        // CTE / view / TVF, and not schema-qualified.
        if tref.subquery.is_some()
            || tref.tvf_args.is_some()
            || self.is_pragma_tvf(tref)
            || tref.schema.is_some()
            || self.lookup_cte(&tref.name, tref.alias.as_deref()).is_some()
            || self.is_view(&tref.name)
            || self.unqualified_db(&tref.name) != DbRef::Main
        {
            return None;
        }
        let meta = self.table_meta(&tref.name, tref.alias.as_deref()).ok()?;
        // Must have a rowid IPK (rules out WITHOUT ROWID, which has `ipk == None`).
        let ipk = meta.ipk?;
        // The `ON` must be a single top-level `=` (after unwrapping parens), one
        // side the inner table's IPK column and the other a left-side column.
        let mut on = on;
        while let Expr::Paren(inner) = on {
            on = inner;
        }
        let left_width = left_columns.len();
        let mut combined = left_columns.to_vec();
        combined.extend(meta.columns.iter().cloned());
        let (a, b) = match on {
            Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } => (col_index(left, &combined)?, col_index(right, &combined)?),
            _ => return None,
        };
        // Identify which side is the inner IPK (`left_width + ipk`) and which is
        // the outer column (a left-side index).
        let inner_ipk = left_width + ipk;
        let outer = if a == inner_ipk && b < left_width {
            b
        } else if b == inner_ipk && a < left_width {
            a
        } else {
            return None;
        };
        Some((outer, meta))
    }

    /// The index-seek companion of [`rowid_join_seek`](Self::rowid_join_seek)
    /// (roadmap B1a², index case): when a `JOIN`'s `ON` is a lone equi-join
    /// `outer.col = u.k` whose right side `u.k` is the *leading column of a full
    /// (non-partial, non-expression) secondary index* on the inner plain base
    /// table `u`, the matching inner rows can be found by seeking that index per
    /// outer row instead of materializing and nested-looping `u`. Returns the
    /// outer column index, the inner table meta, and the chosen index when it
    /// applies; `None` otherwise.
    ///
    /// The rowid/IPK case is preferred — callers must consult
    /// [`rowid_join_seek`](Self::rowid_join_seek) first and only fall through to
    /// this when that returns `None`. Shared by BOTH the executor (to seek) and
    /// the join EQP emitter (to print `SEARCH … USING INDEX <name> (<col>=?)`),
    /// so the two never diverge.
    fn index_join_seek(
        &self,
        join: &Join,
        left_columns: &[ColumnInfo],
    ) -> Option<(usize, TableMeta, IndexMeta)> {
        if !matches!(join.kind, JoinKind::Inner | JoinKind::Left)
            || join.natural
            || !join.using.is_empty()
        {
            return None;
        }
        let on = join.on.as_ref()?;
        let tref = &join.table;
        // The inner table must be a plain base table in `main`: not a subquery /
        // CTE / view / TVF, and not schema-qualified.
        if tref.subquery.is_some()
            || tref.tvf_args.is_some()
            || self.is_pragma_tvf(tref)
            || tref.schema.is_some()
            || self.lookup_cte(&tref.name, tref.alias.as_deref()).is_some()
            || self.is_view(&tref.name)
            || self.unqualified_db(&tref.name) != DbRef::Main
        {
            return None;
        }
        let meta = self.table_meta(&tref.name, tref.alias.as_deref()).ok()?;
        if meta.without_rowid {
            return None;
        }
        // The `ON` must be a single top-level `=` (after unwrapping parens), one
        // side an inner-table column and the other a left-side column.
        let mut on = on;
        while let Expr::Paren(inner) = on {
            on = inner;
        }
        let left_width = left_columns.len();
        let mut combined = left_columns.to_vec();
        combined.extend(meta.columns.iter().cloned());
        let (a, b) = match on {
            Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } => (col_index(left, &combined)?, col_index(right, &combined)?),
            _ => return None,
        };
        // One side must be an inner column (>= left_width) and the other a
        // left-side column (< left_width).
        let (inner_idx, outer) = if a >= left_width && b < left_width {
            (a - left_width, b)
        } else if b >= left_width && a < left_width {
            (b - left_width, a)
        } else {
            return None;
        };
        // The inner join column must be the *leading* column of a full index (not
        // partial, not expression). Pick the first such index by catalog order so
        // the choice is deterministic and matches the EQP emitter.
        let indexes = self.indexes_of(&tref.name).ok()?;
        let idx = indexes.into_iter().find(|i| {
            i.partial.is_none() && i.key_exprs.is_none() && i.cols.first() == Some(&inner_idx)
        })?;
        Some((outer, meta, idx))
    }

    /// The WITHOUT ROWID companion of [`index_join_seek`](Self::index_join_seek):
    /// when the inner table is WITHOUT ROWID and the `ON` equates an outer column
    /// with its *leading* PRIMARY KEY column, the inner row is found by seeking
    /// the clustered b-tree per outer row (`SEARCH … USING PRIMARY KEY (col=?)`)
    /// instead of scanning. Callers consult this after `rowid_join_seek` and
    /// `index_join_seek` (which both decline WITHOUT ROWID tables). Returns
    /// `(outer column index, inner meta)`.
    fn without_rowid_pk_join_seek(
        &self,
        join: &Join,
        left_columns: &[ColumnInfo],
    ) -> Option<(usize, TableMeta)> {
        if !matches!(join.kind, JoinKind::Inner | JoinKind::Left)
            || join.natural
            || !join.using.is_empty()
        {
            return None;
        }
        let on = join.on.as_ref()?;
        let tref = &join.table;
        if tref.subquery.is_some()
            || tref.tvf_args.is_some()
            || self.is_pragma_tvf(tref)
            || tref.schema.is_some()
            || self.lookup_cte(&tref.name, tref.alias.as_deref()).is_some()
            || self.is_view(&tref.name)
            || self.unqualified_db(&tref.name) != DbRef::Main
        {
            return None;
        }
        let meta = self.table_meta(&tref.name, tref.alias.as_deref()).ok()?;
        if !meta.without_rowid || meta.pk_len == 0 {
            return None;
        }
        let lead_pk = meta.storage_order[0];
        let mut on = on;
        while let Expr::Paren(inner) = on {
            on = inner;
        }
        let left_width = left_columns.len();
        let mut combined = left_columns.to_vec();
        combined.extend(meta.columns.iter().cloned());
        let (a, b) = match on {
            Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } => (col_index(left, &combined)?, col_index(right, &combined)?),
            _ => return None,
        };
        let (inner_idx, outer) = if a >= left_width && b < left_width {
            (a - left_width, b)
        } else if b >= left_width && a < left_width {
            (b - left_width, a)
        } else {
            return None;
        };
        if inner_idx != lead_pk {
            return None;
        }
        Some((outer, meta))
    }

    /// Execute a WITHOUT ROWID PK-seek join (decided by
    /// [`without_rowid_pk_join_seek`](Self::without_rowid_pk_join_seek)): for each
    /// outer row, seek the inner table's clustered b-tree by the join key, decode
    /// each matching record to a row, combine, and re-evaluate the full `ON`.
    /// INNER drops an unmatched outer row; LEFT NULL-extends it.
    fn exec_without_rowid_pk_join_seek(
        &self,
        join: &Join,
        columns: &[ColumnInfo],
        rows: &[Vec<Value>],
        outer_col: usize,
        inner_meta: &TableMeta,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        let mut new_columns = columns.to_vec();
        new_columns.extend(inner_meta.columns.iter().cloned());
        let n_jcols = inner_meta.columns.len();
        let on = join.on.as_ref();
        let is_left = matches!(join.kind, JoinKind::Left);
        let lead = inner_meta.storage_order[0];
        let coll = wr_storage_collations(inner_meta)[0];
        let src = self.backend.source();
        let mut joined: Vec<Vec<Value>> = Vec::new();
        for left in rows {
            let mut matched = false;
            if !matches!(left[outer_col], Value::Null) {
                let key = [inner_meta.columns[lead]
                    .affinity
                    .coerce(left[outer_col].clone())];
                let records =
                    crate::btree::index_seek_records(src, inner_meta.root, &key, &[coll])?;
                for storage in records {
                    let mut inner = unpermute_row(inner_meta, storage);
                    self.compute_generated(inner_meta, &mut inner, params)?;
                    let mut row = left.clone();
                    row.extend(inner);
                    let keep = match on {
                        Some(on) => {
                            let ctx = row_ctx(&row, &new_columns, None, params);
                            eval::truth(&eval::eval(on, &ctx)?) == Some(true)
                        }
                        None => true,
                    };
                    if keep {
                        joined.push(row);
                        matched = true;
                    }
                }
            }
            if !matched && is_left {
                let mut combined = left.clone();
                combined.extend(core::iter::repeat_n(Value::Null, n_jcols));
                joined.push(combined);
            }
        }
        Ok((new_columns, joined))
    }

    /// Execute one index-seek join (decided by
    /// [`index_join_seek`](Self::index_join_seek)): for each outer row, take the
    /// join-key value, seek the chosen secondary index for matching rowids, fetch
    /// each inner row by rowid, combine, and re-evaluate the full `ON` so results
    /// are byte-identical to the materialize/hash path. A non-unique index key may
    /// match multiple inner rows — one combined row is emitted per match. INNER
    /// drops an outer row with no inner match; LEFT NULL-extends it. A NULL key
    /// (or one with no index match) yields no inner rows.
    #[allow(clippy::too_many_arguments)]
    fn exec_index_join_seek(
        &self,
        join: &Join,
        columns: &[ColumnInfo],
        rows: &[Vec<Value>],
        outer_col: usize,
        inner_meta: &TableMeta,
        idx: &IndexMeta,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        let encoding = self.backend.source().header().text_encoding;
        let mut new_columns = columns.to_vec();
        new_columns.extend(inner_meta.columns.iter().cloned());
        let n_jcols = inner_meta.columns.len();
        let on = join.on.as_ref();
        let is_left = matches!(join.kind, JoinKind::Left);

        let lead = idx.cols[0];
        let coll = idx.collations[0];
        let src = self.backend.source();
        let mut cur = TableCursor::new(self.backend.source(), inner_meta.root);
        let mut joined: Vec<Vec<Value>> = Vec::new();
        for left in rows {
            let mut matched = false;
            // A NULL outer key never equi-joins; skip the seek (no inner match).
            if !matches!(left[outer_col], Value::Null) {
                // Coerce the key to the leading column's affinity, mirroring
                // `try_index_lookup` so the index comparison is identical.
                let key = [inner_meta.columns[lead]
                    .affinity
                    .coerce(left[outer_col].clone())];
                let colls = [coll];
                let rowids = crate::btree::index_seek_rowids(src, idx.root, &key, &colls)?;
                for rid in rowids {
                    if cur.seek(rid)? {
                        let inner =
                            self.decode_full_row(inner_meta, rid, &cur.payload()?, encoding)?;
                        let mut row = left.clone();
                        row.extend(inner);
                        let keep = match on {
                            Some(on) => {
                                let ctx = row_ctx(&row, &new_columns, None, params);
                                eval::truth(&eval::eval(on, &ctx)?) == Some(true)
                            }
                            None => true,
                        };
                        if keep {
                            joined.push(row);
                            matched = true;
                        }
                    }
                }
            }
            // LEFT: emit the outer row NULL-extended when nothing matched.
            if !matched && is_left {
                let mut combined = left.clone();
                combined.extend(core::iter::repeat_n(Value::Null, n_jcols));
                joined.push(combined);
            }
        }
        Ok((new_columns, joined))
    }

    /// Execute one rowid-seek join (decided by [`rowid_join_seek`](Self::rowid_join_seek)):
    /// for each outer row, coerce its join column to an integer rowid, seek the
    /// inner table's b-tree, and combine. The full `ON` is re-evaluated on the
    /// fetched row so results are identical to the materialize/hash path. INNER
    /// drops an outer row with no inner match; LEFT NULL-extends it.
    fn exec_rowid_join_seek(
        &self,
        join: &Join,
        columns: &[ColumnInfo],
        rows: &[Vec<Value>],
        outer_col: usize,
        inner_meta: &TableMeta,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        let encoding = self.backend.source().header().text_encoding;
        let mut new_columns = columns.to_vec();
        new_columns.extend(inner_meta.columns.iter().cloned());
        let n_jcols = inner_meta.columns.len();
        let on = join.on.as_ref();
        let is_left = matches!(join.kind, JoinKind::Left);

        let mut cur = TableCursor::new(self.backend.source(), inner_meta.root);
        let mut joined: Vec<Vec<Value>> = Vec::new();
        for left in rows {
            // Coerce the outer join value to a candidate rowid. A NULL (or any
            // value that isn't an exact integer) never equi-joins; the `ON`
            // re-eval below rejects a spurious truncation (e.g. `2.5` → 2).
            let key = &left[outer_col];
            let candidate = match key {
                Value::Integer(i) => Some(*i),
                Value::Real(_) | Value::Text(_) => match eval::to_number(key) {
                    Value::Integer(i) => Some(i),
                    Value::Real(r) if r == (r as i64) as f64 => Some(r as i64),
                    _ => None,
                },
                Value::Null | Value::Blob(_) => None,
            };
            let mut matched = false;
            if let Some(rid) = candidate {
                if cur.seek(rid)? {
                    let inner = self.decode_full_row(inner_meta, rid, &cur.payload()?, encoding)?;
                    let mut combined = left.clone();
                    combined.extend(inner);
                    let keep = match on {
                        Some(on) => {
                            let ctx = row_ctx(&combined, &new_columns, None, params);
                            eval::truth(&eval::eval(on, &ctx)?) == Some(true)
                        }
                        None => true,
                    };
                    if keep {
                        joined.push(combined);
                        matched = true;
                    }
                }
            }
            // LEFT: emit the outer row NULL-extended when nothing matched.
            if !matched && is_left {
                let mut combined = left.clone();
                combined.extend(core::iter::repeat_n(Value::Null, n_jcols));
                joined.push(combined);
            }
        }
        Ok((new_columns, joined))
    }

    fn resolve_join_source(
        &self,
        tref: &TableRef,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        if tref.tvf_args.is_some() || self.is_pragma_tvf(tref) {
            return self.tvf_rows(tref, params);
        }
        if let Some(sub) = &tref.subquery {
            return self.run_subquery_source(sub, tref.alias.as_deref(), params);
        }
        if let Some((cols, rows)) = self.lookup_cte(&tref.name, tref.alias.as_deref()) {
            return Ok((cols, rows.into_iter().map(|r| r.values).collect()));
        }
        if let Some((cols, rows)) = self.try_view(&tref.name, tref.alias.as_deref(), params)? {
            return Ok((cols, rows.into_iter().map(|r| r.values).collect()));
        }
        if tref.schema.is_none() {
            // In a join, the WHERE may reference other tables, so no pushdown here
            // (full scan + the join's re-applied WHERE keeps it correct).
            if let Some((cols, rows)) =
                self.try_virtual_table(&tref.name, tref.alias.as_deref(), None)?
            {
                return Ok((cols, rows.into_iter().map(|r| r.values).collect()));
            }
        }
        // Cross-database join source: an explicit qualifier (`aux.t`) picks the
        // database; an unqualified name may be shadowed by a temp table. Either
        // way a non-main source is materialized through its own backend.
        let db = match tref.schema.as_deref() {
            Some(_) => self.resolve_db(tref.schema.as_deref())?,
            None => self.unqualified_db(&tref.name),
        };
        if db != DbRef::Main {
            if let Some((cols, input)) =
                self.scan_db_view(db, &tref.name, tref.alias.as_deref(), params)?
            {
                return Ok((cols, input.into_iter().map(|r| r.values).collect()));
            }
            let (cols, input) = self.scan_db_table(db, &tref.name, tref.alias.as_deref())?;
            return Ok((cols, input.into_iter().map(|r| r.values).collect()));
        }
        let meta = self.table_meta(&tref.name, tref.alias.as_deref())?;
        let rows = if meta.without_rowid {
            self.scan_without_rowid(&meta)?
        } else {
            self.scan_table(&meta)?
                .into_iter()
                .map(|(_, v)| v)
                .collect()
        };
        Ok((meta.columns, rows))
    }

    /// Scan a `WITHOUT ROWID` table's clustered index b-tree, decoding each entry
    /// (stored PK-first) back into declared column order.
    fn scan_without_rowid(&self, meta: &TableMeta) -> Result<Vec<Vec<Value>>> {
        let encoding = self.backend.source().header().text_encoding;
        let mut cur = IndexCursor::new(self.backend.source(), meta.root);
        let params = Params::default();
        let mut out = Vec::new();
        while let Some(payload) = cur.next()? {
            let storage = decode_record(&payload, encoding)?;
            let mut row = unpermute_row(meta, storage);
            self.compute_generated(meta, &mut row, &params)?;
            out.push(row);
        }
        Ok(out)
    }

    /// Build a row (declared order) from an INSERT's column list + value exprs,
    /// applying defaults and affinity. Shared by the WITHOUT ROWID insert path.
    fn build_insert_row(
        &self,
        meta: &TableMeta,
        target: &[usize],
        row_exprs: &[Expr],
        params: &Params,
    ) -> Result<Vec<Value>> {
        let ctx = EvalCtx::rowless(params).with_subqueries(self);
        let mut values: Vec<Value> = meta
            .defaults
            .iter()
            .map(|d| match d {
                Some(e) => eval::eval(e, &ctx),
                None => Ok(Value::Null),
            })
            .collect::<Result<_>>()?;
        for (i, e) in row_exprs.iter().enumerate() {
            if meta.is_generated(target[i]) {
                return Err(Error::Error(format!(
                    "cannot INSERT into generated column \"{}\"",
                    meta.columns[target[i]].name
                )));
            }
            values[target[i]] = eval::eval(e, &ctx)?;
        }
        apply_column_affinity(meta, &mut values);
        self.materialize_generated(meta, &mut values, params)?;
        self.check_strict_types(meta, &values)?;
        Ok(values)
    }

    /// INSERT into a WITHOUT ROWID (PK-clustered) table.
    fn exec_insert_without_rowid(
        &mut self,
        ins: &Insert,
        meta: &TableMeta,
        rows: &[Vec<Expr>],
        params: &Params,
    ) -> Result<usize> {
        let n_cols = meta.columns.len();
        let target: Vec<usize> = if ins.columns.is_empty() {
            (0..n_cols).collect()
        } else {
            ins.columns
                .iter()
                .map(|name| {
                    meta.columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| Error::Error(format!("no such column: {name}")))
                })
                .collect::<Result<_>>()?
        };
        let pk = &meta.storage_order[..meta.pk_len];
        let mut affected = 0;
        for row_exprs in rows {
            if !ins.columns.is_empty() && row_exprs.len() != target.len() {
                return Err(Error::Error("INSERT column/value count mismatch".into()));
            }
            let values = self.build_insert_row(meta, &target, row_exprs, params)?;
            // PRIMARY KEY / NOT NULL / CHECK constraints. `INSERT OR IGNORE`
            // skips a violating row; any other policy lets the error propagate.
            {
                let r = (|| {
                    // PRIMARY KEY columns are implicitly NOT NULL in a WITHOUT
                    // ROWID table.
                    for &c in pk {
                        if matches!(values[c], Value::Null) {
                            return Err(Error::Constraint("NOT NULL constraint failed".into()));
                        }
                    }
                    check_not_null(meta, &values)?;
                    self.check_constraints(meta, &values, None, params)
                })();
                match r {
                    Ok(()) => {}
                    Err(Error::Constraint(_)) if ins.on_conflict == OnConflict::Ignore => continue,
                    Err(e) => return Err(e),
                }
            }

            // Reject a duplicate primary key, an inline UNIQUE constraint, or a
            // standalone UNIQUE index (incl. partial). Collect colliding rows so
            // REPLACE can rebuild without them.
            let existing = self.scan_without_rowid(meta)?;
            let mut collide = Vec::new();
            for (i, r) in existing.iter().enumerate() {
                if unique_match(meta, r, &values)
                    || self.wr_index_collision(&ins.table, meta, r, &values, params)?
                {
                    collide.push(i);
                }
            }
            if !collide.is_empty() {
                match ins.on_conflict {
                    oc @ (OnConflict::Abort | OnConflict::Fail | OnConflict::Rollback) => {
                        let m = wr_unique_message(meta, &existing[collide[0]], &values);
                        return Err(self.conflict_error(oc, &m));
                    }
                    OnConflict::Ignore => continue,
                    OnConflict::Replace => {
                        // Rebuild without the conflicting row(s), then insert.
                        let kept: Vec<Vec<Value>> = existing
                            .into_iter()
                            .enumerate()
                            .filter(|(i, _)| !collide.contains(i))
                            .map(|(_, r)| r)
                            .collect();
                        self.rewrite_without_rowid(meta, kept.into_iter())?;
                    }
                }
            }
            let record = encode_record(&permute_row(meta, &values));
            let scolls = wr_storage_collations(meta);
            insert_index(self.backend.writer()?, meta.root, &record, &scolls)?;
            affected += 1;
        }
        if affected > 0 {
            self.rebuild_wr_indexes(meta, &ins.table)?;
        }
        Ok(affected)
    }

    /// DELETE from a WITHOUT ROWID table: keep non-matching rows, rebuild.
    fn exec_delete_without_rowid(
        &mut self,
        del: &Delete,
        meta: &TableMeta,
        params: &Params,
    ) -> Result<usize> {
        let all = self.scan_without_rowid(meta)?;
        let mut kept = Vec::new();
        let mut deleted = 0;
        for row in all {
            let keep = match &del.where_clause {
                Some(p) => {
                    let ctx = row_ctx(&row, &meta.columns, None, params).with_subqueries(self);
                    eval::truth(&eval::eval(p, &ctx)?) != Some(true)
                }
                None => false,
            };
            if keep {
                kept.push(row);
            } else {
                deleted += 1;
            }
        }
        if deleted > 0 {
            self.rewrite_without_rowid(meta, kept.into_iter())?;
            self.rebuild_wr_indexes(meta, &del.table)?;
        }
        Ok(deleted)
    }

    /// UPDATE a WITHOUT ROWID table: recompute matching rows, rebuild.
    fn exec_update_without_rowid(
        &mut self,
        upd: &Update,
        meta: &TableMeta,
        params: &Params,
    ) -> Result<usize> {
        let all = self.scan_without_rowid(meta)?;
        let mut out = Vec::with_capacity(all.len());
        let mut affected = 0;
        for mut row in all {
            let matches = match &upd.where_clause {
                Some(p) => {
                    let ctx = row_ctx(&row, &meta.columns, None, params).with_subqueries(self);
                    eval::truth(&eval::eval(p, &ctx)?) == Some(true)
                }
                None => true,
            };
            if matches {
                // Assignments are simultaneous: evaluate every SET expression
                // against the original row, not the progressively-mutated one.
                let original = row.clone();
                for (col, expr) in &upd.assignments {
                    let pos = meta
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(col))
                        .ok_or_else(|| Error::Error(format!("no such column: {col}")))?;
                    if meta.is_generated(pos) {
                        return Err(Error::Error(format!(
                            "cannot UPDATE generated column \"{col}\""
                        )));
                    }
                    let ctx = row_ctx(&original, &meta.columns, None, params).with_subqueries(self);
                    row[pos] = eval::eval(expr, &ctx)?;
                }
                if !upd.row_assignments.is_empty() {
                    let ctx = row_ctx(&original, &meta.columns, None, params).with_subqueries(self);
                    self.apply_row_subquery_assignments(
                        &upd.row_assignments,
                        &meta.columns,
                        Some(meta),
                        &ctx,
                        &mut row,
                    )?;
                }
                apply_column_affinity(meta, &mut row);
                self.materialize_generated(meta, &mut row, params)?;
                check_not_null(meta, &row)?;
                self.check_strict_types(meta, &row)?;
                self.check_constraints(meta, &row, None, params)?;
                affected += 1;
            }
            out.push(row);
        }
        // Reject duplicate primary keys or UNIQUE values produced by the update
        // (inline constraints and standalone unique indexes alike).
        for i in 0..out.len() {
            for j in (i + 1)..out.len() {
                if unique_match(meta, &out[i], &out[j])
                    || self.wr_index_collision(&upd.table, meta, &out[i], &out[j], params)?
                {
                    return Err(Error::Constraint(wr_unique_message(meta, &out[i], &out[j])));
                }
            }
        }
        if affected > 0 {
            self.rewrite_without_rowid(meta, out.into_iter())?;
            self.rebuild_wr_indexes(meta, &upd.table)?;
        }
        Ok(affected)
    }

    /// Replace a WITHOUT ROWID table's entire contents with `rows` (declared
    /// order), re-encoding each into PK-first storage order.
    fn rewrite_without_rowid(
        &mut self,
        meta: &TableMeta,
        rows: impl Iterator<Item = Vec<Value>>,
    ) -> Result<()> {
        let records: Vec<Vec<u8>> = rows
            .map(|r| encode_record(&permute_row(meta, &r)))
            .collect();
        let scolls = wr_storage_collations(meta);
        let w = self.backend.writer()?;
        clear_index(w, meta.root)?;
        for rec in &records {
            insert_index(w, meta.root, rec, &scolls)?;
        }
        Ok(())
    }

    /// Rebuild every secondary index of a `WITHOUT ROWID` table from its current
    /// rows, keying entries by (indexed cols, PK cols).
    fn rebuild_wr_indexes(&mut self, meta: &TableMeta, table: &str) -> Result<()> {
        let indexes = self.indexes_of(table)?;
        if indexes.is_empty() {
            return Ok(());
        }
        let rows = self.scan_without_rowid(meta)?;
        let pk_cols = meta.storage_order[..meta.pk_len].to_vec();
        let pk_colls: Vec<crate::value::Collation> =
            pk_cols.iter().map(|&c| meta.columns[c].collation).collect();
        // Precompute partial-index membership before the writer borrow.
        let mut keep: Vec<Vec<usize>> = Vec::with_capacity(indexes.len());
        for idx in &indexes {
            let mut ks = Vec::new();
            for (i, row) in rows.iter().enumerate() {
                if self.row_in_index(idx, meta, row, None, &Params::default())? {
                    ks.push(i);
                }
            }
            keep.push(ks);
        }
        let w = self.backend.writer()?;
        for (idx, ks) in indexes.iter().zip(&keep) {
            let mut key_colls = idx.collations.clone();
            key_colls.extend(pk_colls.iter().copied());
            clear_index(w, idx.root)?;
            for &i in ks {
                insert_index(
                    w,
                    idx.root,
                    &wr_index_key(&idx.cols, &pk_cols, &rows[i]),
                    &key_colls,
                )?;
            }
        }
        Ok(())
    }

    /// Scan a whole table into `(rowid, column values)`.
    /// Whether the named table currently holds no rows (handles both rowid and
    /// WITHOUT ROWID storage).
    fn table_is_empty(&self, table: &str) -> Result<bool> {
        let meta = self.table_meta(table, None)?;
        if meta.without_rowid {
            Ok(self.scan_without_rowid(&meta)?.is_empty())
        } else {
            Ok(self.scan_table(&meta)?.is_empty())
        }
    }

    /// Resolve a `schema.` qualifier to a database: `None`/`main` → `Main`;
    /// `temp`/`temporary` → `Temp`; an attached name → `Attached(index)`; an
    /// unknown name is an error.
    fn resolve_db(&self, schema: Option<&str>) -> Result<DbRef> {
        match schema {
            None => Ok(DbRef::Main),
            Some(s) if s.eq_ignore_ascii_case("main") => Ok(DbRef::Main),
            Some(s) if s.eq_ignore_ascii_case("temp") || s.eq_ignore_ascii_case("temporary") => {
                Ok(DbRef::Temp)
            }
            Some(s) => self
                .attached
                .iter()
                .position(|d| d.name.eq_ignore_ascii_case(s))
                .map(DbRef::Attached)
                .ok_or_else(|| Error::Error(alloc::format!("unknown database {s}"))),
        }
    }

    /// The schema catalog and backend for a resolved database. `Temp` requires
    /// the temp database to exist (created by [`ensure_temp`](Self::ensure_temp)).
    fn db_parts(&self, db: DbRef) -> (&Schema, &Backend) {
        match db {
            DbRef::Main => (&self.schema, &self.backend),
            DbRef::Temp => {
                let t = self.temp_db.as_ref().expect("temp db exists");
                (&t.schema, &t.backend)
            }
            DbRef::Attached(i) => (&self.attached[i].schema, &self.attached[i].backend),
        }
    }

    /// The database an *unqualified* table name resolves to: the `temp` database
    /// when it holds the table (temp shadows main), else `main`.
    fn unqualified_db(&self, name: &str) -> DbRef {
        if let Some(t) = &self.temp_db {
            if t.schema.table(name).is_some() {
                return DbRef::Temp;
            }
        }
        // Inside a cross-database view read, unqualified names resolve in the
        // view's own database (when it has the table) before falling back to
        // main; nested subqueries inherit this via the shared cell.
        let def = self.read_default.get();
        if def != DbRef::Main {
            let (schema, _) = self.db_parts(def);
            if schema.table(name).is_some() {
                return def;
            }
        }
        DbRef::Main
    }

    /// Create the `temp` database if it does not yet exist (a fresh in-memory
    /// database, like an attachment).
    fn ensure_temp(&mut self) -> Result<()> {
        if self.temp_db.is_some() {
            return Ok(());
        }
        let vfs = crate::vfs::memory::MemoryVfs::new();
        let f = vfs.open("temp", OpenFlags::READ_WRITE_CREATE)?;
        let mut db = WritePager::create(f, None, 4096)?;
        db.commit()?;
        let backend = Backend::Write(Box::new(db));
        let schema = Schema::read(backend.source())?;
        self.temp_db = Some(AttachedDb {
            name: "temp".into(),
            file: String::new(),
            backend,
            schema,
        });
        Ok(())
    }

    /// Materialize a rowid table from a non-main database into `(columns, rows)`
    /// — the cross-database read path (C3/C4). Reads through that database's own
    /// backend, so its page numbers resolve correctly.
    fn scan_db_table(
        &self,
        db: DbRef,
        name: &str,
        alias: Option<&str>,
    ) -> Result<(Vec<ColumnInfo>, Vec<InputRow>)> {
        let (schema, backend) = self.db_parts(db);
        let meta = self.table_meta_in(schema, name, alias)?;
        let source = backend.source();
        let encoding = source.header().text_encoding;
        // WITHOUT ROWID: walk the clustered index b-tree (records stored
        // PK-first) and decode each entry back into declared column order.
        if meta.without_rowid {
            let params = Params::default();
            let mut rows = Vec::new();
            let mut cur = IndexCursor::new(source, meta.root);
            while let Some(payload) = cur.next()? {
                let storage = decode_record(&payload, encoding)?;
                let mut values = unpermute_row(&meta, storage);
                self.compute_generated(&meta, &mut values, &params)?;
                rows.push(InputRow {
                    values,
                    rowid: None,
                });
            }
            return Ok((meta.columns, rows));
        }
        let mut rows = Vec::new();
        let mut cur = TableCursor::new(source, meta.root);
        let mut ok = cur.first()?;
        while ok {
            let rowid = cur.rowid()?;
            let values = self.decode_full_row(&meta, rowid, &cur.payload()?, encoding)?;
            rows.push(InputRow {
                values,
                rowid: Some(rowid),
            });
            ok = cur.next()?;
        }
        Ok((meta.columns, rows))
    }

    /// Read a view from a non-main database: run its body with unqualified
    /// table names resolving in that database (via `read_default`, restored
    /// afterwards). Returns `None` when `name` is not a view in `db`, so the
    /// caller can fall back to reading it as a table.
    fn scan_db_view(
        &self,
        db: DbRef,
        name: &str,
        alias: Option<&str>,
        params: &Params,
    ) -> Result<Option<(Vec<ColumnInfo>, Vec<InputRow>)>> {
        use crate::schema::ObjectType;
        let (schema, _) = self.db_parts(db);
        let obj = match schema
            .objects()
            .iter()
            .find(|o| o.obj_type == ObjectType::View && o.name.eq_ignore_ascii_case(name))
        {
            Some(o) => o.clone(),
            None => return Ok(None),
        };
        let sql = obj
            .sql
            .as_deref()
            .ok_or_else(|| Error::Corrupt("view has no CREATE statement".into()))?;
        let Statement::CreateView(cv) = sql::parse_one(sql)? else {
            return Err(Error::Corrupt("schema sql is not CREATE VIEW".into()));
        };
        // Resolve the view body's unqualified names in `db`; restore on the way
        // out (even on error) so an outer query's resolution is unaffected.
        let prev = self.read_default.get();
        self.read_default.set(db);
        let run = self.run_select(&cv.select, params);
        self.read_default.set(prev);
        let result = run?;
        let label = alias.unwrap_or(name).to_string();
        let names = if cv.columns.is_empty() {
            result.columns.clone()
        } else {
            cv.columns.clone()
        };
        let columns: Vec<ColumnInfo> = names
            .into_iter()
            .map(|n| ColumnInfo {
                name: n,
                table: label.clone(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            })
            .collect();
        let rows = result
            .rows
            .into_iter()
            .map(|values| InputRow {
                values,
                rowid: None,
            })
            .collect();
        Ok(Some((columns, rows)))
    }

    /// The `dbstat` eponymous read-only virtual table: one row per b-tree page
    /// (plus one per overflow page), reporting SQLite-compatible per-page storage
    /// statistics (`name, path, pageno, pagetype, ncell, payload, unused,
    /// mx_payload, pgoffset, pgsize`). Byte-compatible with SQLite's dbstat
    /// extension: `unused` is derived from the page header's free-space pointer,
    /// fragmented-bytes count, and freeblock chain; `payload` sums the locally
    /// stored cell bytes; `mx_payload` is the largest total cell payload. The
    /// `path` strings use SQLite's `/<hex-child>/` and `+<hex-overflow>` format.
    fn scan_dbstat(&self, alias: Option<&str>) -> Result<(Vec<ColumnInfo>, Vec<InputRow>)> {
        use crate::btree::page::{BtreePage, PageType};
        use eval::Affinity::{Integer, Text};

        let label = alias.unwrap_or("dbstat").to_string();
        let col = |name: &str, affinity| ColumnInfo {
            name: String::from(name),
            table: label.clone(),
            affinity,
            collation: crate::value::Collation::default(),
        };
        let columns = alloc::vec![
            col("name", Text),
            col("path", Text),
            col("pageno", Integer),
            col("pagetype", Text),
            col("ncell", Integer),
            col("payload", Integer),
            col("unused", Integer),
            col("mx_payload", Integer),
            col("pgoffset", Integer),
            col("pgsize", Integer),
        ];

        let src = self.backend.source();
        let usable = src.usable_size();
        let page_size = src.header().page_size as i64;
        let be16 = |d: &[u8], off: usize| u16::from_be_bytes([d[off], d[off + 1]]) as usize;

        // The b-trees to walk: `sqlite_schema` (page 1) first, then every object
        // that owns a root page (tables and indexes), in catalog order.
        let mut btrees: Vec<(String, u32)> = alloc::vec![(String::from("sqlite_schema"), 1)];
        for obj in self.schema.objects() {
            if obj.rootpage != 0 {
                btrees.push((obj.name.clone(), obj.rootpage));
            }
        }

        let mut rows: Vec<InputRow> = Vec::new();
        for (name, root) in btrees {
            // Pre-order DFS; child order does not affect per-page stats.
            let mut stack = alloc::vec![(root, String::from("/"))];
            while let Some((pgno, path)) = stack.pop() {
                let page = src.page(pgno)?;
                let bp = BtreePage::parse(page)?;
                let data = bp.data();
                let body = if pgno == 1 { 100 } else { 0 };
                let ncell = bp.num_cells();
                let is_leaf = bp.page_type().is_leaf();
                let nhdr = body + if is_leaf { 8 } else { 12 };
                let ptype = if is_leaf { "leaf" } else { "internal" };

                let mut payload = 0i64;
                let mut mx = 0i64;
                // SQLite's dbstat reports an overflow page's `pgoffset` as the
                // offset of the *previously visited* page (the owning leaf for a
                // chain's first page, the prior chain page after) — an off-by-one
                // in its statSizeAndOffset. `prev_pgno` reproduces that lag; it
                // starts at the leaf and carries across this page's cells.
                let mut prev_pgno = pgno;
                // Sum local payload and emit overflow-page rows.
                for i in 0..ncell {
                    let pl = match bp.page_type() {
                        PageType::LeafTable => bp.table_leaf_cell(i, usable)?.payload,
                        PageType::LeafIndex | PageType::InteriorIndex => {
                            bp.index_cell(i, usable)?.payload
                        }
                        // Interior-table cells carry no payload.
                        PageType::InteriorTable => continue,
                    };
                    payload += pl.local_len as i64;
                    mx = mx.max(pl.total_len as i64);

                    // Walk this cell's overflow chain, one row per overflow page.
                    let mut ovfl = pl.overflow;
                    let mut remaining = pl.total_len - pl.local_len;
                    let mut iovfl = 0usize;
                    while ovfl != 0 {
                        let opage = src.page(ovfl)?;
                        let odata = opage.data();
                        let next = u32::from_be_bytes([odata[0], odata[1], odata[2], odata[3]]);
                        let cap = usable - 4;
                        let (opayload, ounused) = if remaining <= cap {
                            (remaining as i64, (cap - remaining) as i64)
                        } else {
                            (cap as i64, 0)
                        };
                        rows.push(InputRow {
                            values: alloc::vec![
                                Value::Text(name.clone()),
                                Value::Text(alloc::format!("{path}{i:03x}+{iovfl:06x}")),
                                Value::Integer(ovfl as i64),
                                Value::Text(String::from("overflow")),
                                Value::Integer(0),
                                Value::Integer(opayload),
                                Value::Integer(ounused),
                                Value::Integer(0),
                                Value::Integer((prev_pgno as i64 - 1) * page_size),
                                Value::Integer(page_size),
                            ],
                            rowid: None,
                        });
                        remaining = remaining.saturating_sub(cap);
                        iovfl += 1;
                        prev_pgno = ovfl;
                        ovfl = next;
                    }
                }

                // Free space: (cell-content-area-start - header - cell-pointer
                // array) + fragmented free bytes + the freeblock chain.
                let cc = match be16(data, body + 5) {
                    0 => 65536,
                    n => n,
                };
                let mut unused = cc as i64 - nhdr as i64 - 2 * ncell as i64 + data[body + 7] as i64;
                let mut fb = be16(data, body + 1);
                while fb != 0 && fb + 4 <= data.len() {
                    unused += be16(data, fb + 2) as i64;
                    fb = be16(data, fb);
                }

                rows.push(InputRow {
                    values: alloc::vec![
                        Value::Text(name.clone()),
                        Value::Text(path.clone()),
                        Value::Integer(pgno as i64),
                        Value::Text(String::from(ptype)),
                        Value::Integer(ncell as i64),
                        Value::Integer(payload),
                        Value::Integer(unused),
                        Value::Integer(mx),
                        Value::Integer((pgno as i64 - 1) * page_size),
                        Value::Integer(page_size),
                    ],
                    rowid: None,
                });

                // Descend into children of an interior page.
                if !is_leaf {
                    for i in 0..=ncell {
                        let child = bp.child_pointer(i)?;
                        if child != 0 {
                            stack.push((child, alloc::format!("{path}{i:03x}/")));
                        }
                    }
                }
            }
        }

        Ok((columns, rows))
    }

    /// The `fts5vocab` virtual table: a read-only view over another FTS5 table's
    /// vocabulary. `args` is the `USING fts5vocab(...)` list; `vocab_name`/`alias`
    /// label the result. Tokenizes the referenced table's documents (with the
    /// same `fts5_tokenize` used for indexing) and aggregates per the requested
    /// form — `row` (term, doc, cnt), `col` (term, col, doc, cnt), or `instance`
    /// (term, doc, col, offset) — byte-compatible with SQLite's fts5vocab.
    #[cfg(feature = "fts5")]
    fn scan_fts5vocab(
        &self,
        args: &[String],
        vocab_name: &str,
        alias: Option<&str>,
    ) -> Result<(Vec<ColumnInfo>, Vec<InputRow>)> {
        use alloc::collections::{BTreeMap, BTreeSet};

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (ft_name, form) = crate::vtab::fts5vocab_args(&arg_refs)?;

        let label = alias.unwrap_or(vocab_name).to_string();
        let colnames: &[&str] = match form.as_str() {
            "row" => &["term", "doc", "cnt"],
            "col" => &["term", "col", "doc", "cnt"],
            _ => &["term", "doc", "col", "offset"],
        };
        let columns: Vec<ColumnInfo> = colnames
            .iter()
            .map(|n| ColumnInfo {
                name: String::from(*n),
                table: label.clone(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            })
            .collect();

        // The referenced FTS5 table: its column names + documents (the persistent
        // `<ft>_data` backing table holds one row per document, column-ordered).
        let (ft_module, ft_args, ft_schema) = self.vtab_meta(&ft_name)?;
        if !ft_module.eq_ignore_ascii_case("fts5") {
            return Err(Error::Error(format!("no such fts5 table: {ft_name}")));
        }
        let ft_cols = ft_schema.columns;
        // Tokenize with the referenced table's own tokenizer (porter / diacritics).
        let ft_refs: Vec<&str> = ft_args.iter().map(String::as_str).collect();
        let ft_tok = crate::vtab::fts5_tok_config(&ft_refs);
        // Documents live in `<ft>_content` (sqlite's layout): `(id, c0, c1, …)`.
        // Drop the leading `id` so `vals` is the column-ordered document.
        let bmeta = self.table_meta(&format!("{ft_name}_content"), None)?;
        let docs: Vec<(i64, Vec<Value>)> = self
            .scan_table(&bmeta)?
            .into_iter()
            .map(|(rowid, mut vals)| {
                if !vals.is_empty() {
                    vals.remove(0);
                }
                (rowid, vals)
            })
            .collect();

        // FTS5 columns store text; coerce other stored types the way SQLite does
        // (NULL/blob contribute no tokens).
        let to_text = |v: &Value| -> Option<String> {
            match v {
                Value::Text(s) => Some(s.clone()),
                Value::Integer(i) => Some(i.to_string()),
                Value::Real(r) => Some(eval::format_real(*r)),
                Value::Null | Value::Blob(_) => None,
            }
        };

        let mut rows: Vec<InputRow> = Vec::new();
        match form.as_str() {
            "row" => {
                // term → (distinct documents, total occurrences)
                let mut map: BTreeMap<String, (BTreeSet<i64>, i64)> = BTreeMap::new();
                for (rowid, vals) in &docs {
                    for v in vals.iter().take(ft_cols.len()) {
                        if let Some(t) = to_text(v) {
                            for tok in crate::vtab::fts5_tokenize(&t, ft_tok) {
                                let e = map.entry(tok).or_default();
                                e.0.insert(*rowid);
                                e.1 += 1;
                            }
                        }
                    }
                }
                for (term, (ds, cnt)) in map {
                    rows.push(InputRow {
                        values: alloc::vec![
                            Value::Text(term),
                            Value::Integer(ds.len() as i64),
                            Value::Integer(cnt),
                        ],
                        rowid: None,
                    });
                }
            }
            "col" => {
                // (term, column index) → (distinct documents, total occurrences)
                let mut map: BTreeMap<(String, usize), (BTreeSet<i64>, i64)> = BTreeMap::new();
                for (rowid, vals) in &docs {
                    for (ci, v) in vals.iter().take(ft_cols.len()).enumerate() {
                        if let Some(t) = to_text(v) {
                            for tok in crate::vtab::fts5_tokenize(&t, ft_tok) {
                                let e = map.entry((tok, ci)).or_default();
                                e.0.insert(*rowid);
                                e.1 += 1;
                            }
                        }
                    }
                }
                for ((term, ci), (ds, cnt)) in map {
                    rows.push(InputRow {
                        values: alloc::vec![
                            Value::Text(term),
                            Value::Text(ft_cols[ci].clone()),
                            Value::Integer(ds.len() as i64),
                            Value::Integer(cnt),
                        ],
                        rowid: None,
                    });
                }
            }
            _ => {
                // instance: one row per token occurrence (term, doc, col, offset),
                // offset being the 0-based token position within that column.
                let mut insts: Vec<(String, i64, usize, i64)> = Vec::new();
                for (rowid, vals) in &docs {
                    for (ci, v) in vals.iter().take(ft_cols.len()).enumerate() {
                        if let Some(t) = to_text(v) {
                            for (off, tok) in crate::vtab::fts5_tokenize(&t, ft_tok)
                                .into_iter()
                                .enumerate()
                            {
                                insts.push((tok, *rowid, ci, off as i64));
                            }
                        }
                    }
                }
                insts.sort();
                for (term, rowid, ci, off) in insts {
                    rows.push(InputRow {
                        values: alloc::vec![
                            Value::Text(term),
                            Value::Integer(rowid),
                            Value::Text(ft_cols[ci].clone()),
                            Value::Integer(off),
                        ],
                        rowid: None,
                    });
                }
            }
        }
        Ok((columns, rows))
    }

    /// Read a SQLite-format R-Tree's entries by walking its `<name>_node` b-tree
    /// of nodes. Each node blob is a 2-byte BE depth (meaningful in the root) +
    /// 2-byte BE cell count, then cells of an 8-byte BE rowid (leaf) / child
    /// node-number (interior) followed by `n_coords` 4-byte BE coordinates (f32
    /// for `rtree`, i32 for `rtree_i32`). Yields one `InputRow` per leaf entry:
    /// `[id, coord0, …]`. The traversal collects a superset; `run_core` re-applies
    /// the full WHERE.
    fn scan_rtree_nodes(
        &self,
        name: &str,
        n_coords: usize,
        integer: bool,
        bbox: &[(usize, ConstraintOp, f64)],
    ) -> Result<Vec<InputRow>> {
        use alloc::collections::BTreeMap;
        let node_meta = self.table_meta(&format!("{name}_node"), None)?;
        let mut nodes: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
        for (nodeno, vals) in self.scan_table(&node_meta)? {
            // `<name>_node` is `(nodeno INTEGER PRIMARY KEY, data)`; the blob is
            // the `data` column (the first value is the rowid/nodeno itself).
            if let Some(Value::Blob(b)) = vals.into_iter().find(|v| matches!(v, Value::Blob(_))) {
                nodes.insert(nodeno, b);
            }
        }
        let cell_size = 8 + n_coords * 4;
        // Read coordinate `j` (0-based) of the cell whose 8-byte key starts at `off`.
        let coord_at = |blob: &[u8], off: usize, j: usize| -> f64 {
            let p = off + 8 + j * 4;
            let b: [u8; 4] = blob[p..p + 4].try_into().expect("4 bytes");
            if integer {
                f64::from(i32::from_be_bytes(b))
            } else {
                f64::from(f32::from_be_bytes(b))
            }
        };
        // Spatial pushdown: a subtree's stored cell is the MBR of its entries —
        // `[lo, hi]` per dimension — so a constraint on either coordinate column of
        // dimension `d` can be satisfied by some entry only if the MBR overlaps it.
        // The on-disk MBR is a superset (f32 rounds min down / max up), so this
        // prune never drops a matching entry; `run_core` re-applies the full WHERE,
        // making the visited rows a correct superset. Constraints whose dimension
        // can't possibly be satisfied prune the whole subtree.
        let subtree_matches = |blob: &[u8], off: usize| -> bool {
            bbox.iter().all(|&(ci, op, v)| {
                let d = ci / 2;
                let lo = coord_at(blob, off, 2 * d);
                let hi = coord_at(blob, off, 2 * d + 1);
                match op {
                    ConstraintOp::Ge => hi >= v,
                    ConstraintOp::Gt => hi > v,
                    ConstraintOp::Le => lo <= v,
                    ConstraintOp::Lt => lo < v,
                    ConstraintOp::Eq => lo <= v && v <= hi,
                    _ => true,
                }
            })
        };
        let mut out = Vec::new();
        let Some(root) = nodes.get(&1) else {
            return Ok(out);
        };
        if root.len() < 4 {
            return Ok(out);
        }
        // The root header's depth field is the tree height; descend that many
        // levels to reach the leaves.
        let depth = i64::from(u16::from_be_bytes([root[0], root[1]]));
        let mut stack: Vec<(i64, i64)> = alloc::vec![(1, depth)];
        while let Some((nodeno, level)) = stack.pop() {
            let Some(blob) = nodes.get(&nodeno) else {
                continue;
            };
            if blob.len() < 4 {
                continue;
            }
            let ncell = u16::from_be_bytes([blob[2], blob[3]]) as usize;
            for i in 0..ncell {
                let off = 4 + i * cell_size;
                if off + cell_size > blob.len() {
                    break;
                }
                let key = i64::from_be_bytes(blob[off..off + 8].try_into().expect("8 bytes"));
                if level > 0 {
                    // Interior cell: the 8-byte field is a child node number. Skip
                    // the whole subtree when its MBR can't satisfy the constraints.
                    if !bbox.is_empty() && !subtree_matches(blob, off) {
                        continue;
                    }
                    stack.push((key, level - 1));
                    continue;
                }
                // Leaf cell: the 8-byte field is the entry's rowid.
                let mut row = Vec::with_capacity(1 + n_coords);
                row.push(Value::Integer(key));
                for c in 0..n_coords {
                    let p = off + 8 + c * 4;
                    let b: [u8; 4] = blob[p..p + 4].try_into().expect("4 bytes");
                    row.push(if integer {
                        Value::Integer(i64::from(i32::from_be_bytes(b)))
                    } else {
                        Value::Real(f64::from(f32::from_be_bytes(b)))
                    });
                }
                out.push(InputRow {
                    values: row,
                    rowid: Some(key),
                });
            }
        }
        Ok(out)
    }

    /// The fixed R-Tree node size for this database's page size.
    fn rtree_node_size_for(&self, n_coord: usize) -> usize {
        rtree_node_size(n_coord, self.backend.source().header().page_size as usize)
    }

    /// The current entries of an R-Tree as `(rowid, coords)` cells (via the M1
    /// node reader; coords come back as the stored f32/i32 values widened to f64).
    fn rtree_entries(&self, name: &str, n_coord: usize, integer: bool) -> Result<Vec<RtreeCell>> {
        Ok(self
            .scan_rtree_nodes(name, n_coord, integer, &[])?
            .into_iter()
            .map(|r| {
                let key = match r.values.first() {
                    Some(Value::Integer(i)) => *i,
                    _ => 0,
                };
                let coords = r.values[1..1 + n_coord]
                    .iter()
                    .map(|v| match v {
                        Value::Integer(i) => *i as f64,
                        Value::Real(f) => *f,
                        _ => 0.0,
                    })
                    .collect();
                RtreeCell { key, coords }
            })
            .collect())
    }

    /// Replace an R-Tree's three shadow tables with a freshly bulk-built tree.
    fn rtree_write_build(&mut self, name: &str, build: &RtreeBuild) -> Result<()> {
        let node_t = sql::print::ident(&format!("{name}_node"));
        let rowid_t = sql::print::ident(&format!("{name}_rowid"));
        let parent_t = sql::print::ident(&format!("{name}_parent"));
        let pv = |vals: Vec<Value>| Params {
            positional: vals,
            named: Vec::new(),
        };
        self.execute(&format!("DELETE FROM {node_t}"))?;
        self.execute(&format!("DELETE FROM {rowid_t}"))?;
        self.execute(&format!("DELETE FROM {parent_t}"))?;
        for (nodeno, blob) in &build.nodes {
            self.execute_params(
                &format!("INSERT INTO {node_t} VALUES(?1,?2)"),
                &pv(alloc::vec![
                    Value::Integer(*nodeno),
                    Value::Blob(blob.clone())
                ]),
            )?;
        }
        for (rowid, nodeno) in &build.rowids {
            self.execute_params(
                &format!("INSERT INTO {rowid_t} VALUES(?1,?2)"),
                &pv(alloc::vec![Value::Integer(*rowid), Value::Integer(*nodeno)]),
            )?;
        }
        for (child, parent) in &build.parents {
            self.execute_params(
                &format!("INSERT INTO {parent_t} VALUES(?1,?2)"),
                &pv(alloc::vec![Value::Integer(*child), Value::Integer(*parent)]),
            )?;
        }
        Ok(())
    }

    /// Create an R-Tree's storage: the `_node`/`_rowid`/`_parent` shadow tables
    /// (byte-compatible with SQLite) plus an empty root node.
    fn rtree_create_storage(&mut self, name: &str, n_coord: usize, integer: bool) -> Result<()> {
        for (suffix, cols) in [
            ("_node", "nodeno INTEGER PRIMARY KEY, data"),
            ("_rowid", "rowid INTEGER PRIMARY KEY, nodeno"),
            ("_parent", "nodeno INTEGER PRIMARY KEY, parentnode"),
        ] {
            let sql = format!(
                "CREATE TABLE {}({cols})",
                sql::print::ident(&format!("{name}{suffix}"))
            );
            let Statement::CreateTable(ct) = sql::parse_one(&sql)? else {
                unreachable!("constructed a CREATE TABLE")
            };
            self.exec_create_table(&ct, &sql)?;
        }
        let build = rtree_bulk_build(
            Vec::new(),
            n_coord,
            integer,
            self.rtree_node_size_for(n_coord),
        );
        self.rtree_write_build(name, &build)
    }

    /// Apply inserts and/or a delete to an R-Tree by rebuilding its node tree
    /// (read all entries, apply, bulk-build, rewrite). `inserts` carry coords
    /// already rounded to the conservative f32/i32 form.
    fn rtree_apply(
        &mut self,
        name: &str,
        n_coord: usize,
        integer: bool,
        inserts: Vec<RtreeCell>,
        deletes: &[i64],
    ) -> Result<()> {
        let mut entries = self.rtree_entries(name, n_coord, integer)?;
        let removed: alloc::collections::BTreeSet<i64> = deletes
            .iter()
            .copied()
            .chain(inserts.iter().map(|c| c.key))
            .collect();
        entries.retain(|c| !removed.contains(&c.key));
        entries.extend(inserts);
        let build = rtree_bulk_build(entries, n_coord, integer, self.rtree_node_size_for(n_coord));
        self.rtree_write_build(name, &build)
    }

    /// After a write to an FTS5 table, rebuild its inverted index from the
    /// updated `<name>_content` documents. A no-op for every other module.
    fn fts5_maybe_rebuild(&mut self, module_name: &str, table: &str) -> Result<()> {
        #[cfg(feature = "fts5")]
        if module_name.eq_ignore_ascii_case("fts5") {
            return self.fts5_rebuild_index(table);
        }
        let _ = (module_name, table);
        Ok(())
    }

    /// Create an FTS5 table's storage: SQLite's five shadow tables
    /// (`_content`/`_docsize`/`_config`/`_idx`/`_data`) instead of graphite's
    /// generic `<name>_data` store, so a graphite-written FTS5 table is readable
    /// (and `MATCH`-able) by stock sqlite. `_content` holds the documents (same
    /// `(id, c0, c1, …)` shape graphite already reads); the inverted index in
    /// `_data`/`_idx` is rebuilt from `_content` on every write.
    #[cfg(feature = "fts5")]
    fn fts5_create_storage(&mut self, name: &str, ncols: usize) -> Result<()> {
        let content_cols: Vec<String> = (0..ncols).map(|c| format!("c{c}")).collect();
        let q = |s: &str| sql::print::ident(s);
        let defs = [
            (
                format!("{name}_content"),
                format!("id INTEGER PRIMARY KEY, {}", content_cols.join(", ")),
                "",
            ),
            (
                format!("{name}_docsize"),
                "id INTEGER PRIMARY KEY, sz BLOB".to_string(),
                "",
            ),
            (
                format!("{name}_config"),
                "k PRIMARY KEY, v".to_string(),
                " WITHOUT ROWID",
            ),
            (
                format!("{name}_idx"),
                "segid, term, pgno, PRIMARY KEY(segid, term)".to_string(),
                " WITHOUT ROWID",
            ),
            (
                format!("{name}_data"),
                "id INTEGER PRIMARY KEY, block BLOB".to_string(),
                "",
            ),
        ];
        for (tname, cols, tail) in &defs {
            let sql = format!("CREATE TABLE {}({cols}){tail}", q(tname));
            let Statement::CreateTable(ct) = sql::parse_one(&sql)? else {
                unreachable!("constructed a CREATE TABLE")
            };
            self.exec_create_table(&ct, &sql)?;
        }
        // The configuration version row, then the empty segment index. The vtab's
        // own schema row is not inserted yet, so write the initial `_data` rows
        // directly (the index is rebuilt from `_content` on the first write).
        self.execute_params(
            &format!(
                "INSERT INTO {} VALUES('version', 4)",
                q(&format!("{name}_config"))
            ),
            &Params::default(),
        )?;
        let seg = crate::fts5_index::build_segment(&[], 0, &alloc::vec![0u64; ncols], &[], 4050, 0);
        let data_t = q(&format!("{name}_data"));
        for (id, block) in &seg.data {
            self.execute_params(
                &format!("INSERT INTO {data_t} VALUES(?1,?2)"),
                &Params {
                    positional: alloc::vec![Value::Integer(*id), Value::Blob(block.clone())],
                    named: Vec::new(),
                },
            )?;
        }
        Ok(())
    }

    /// Rebuild an FTS5 table's `%_data`/`%_idx`/`%_docsize` from the documents in
    /// `<name>_content` (a bulk rebuild, like the R-Tree). Tokenizes each column
    /// with the table's tokenizer and writes a byte-compatible segment index.
    #[cfg(feature = "fts5")]
    fn fts5_rebuild_index(&mut self, name: &str) -> Result<()> {
        use crate::fts5_index::{self, IdxRow, Posting};
        use alloc::collections::BTreeMap;
        let (_module, args, schema) = self.vtab_meta(name)?;
        let ncols = schema.columns.len();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let tok = crate::vtab::fts5_tok_config(&arg_refs);

        let cmeta = self.table_meta(&format!("{name}_content"), None)?;
        let docs = self.scan_table(&cmeta)?;

        // term bytes -> rowid -> per-column positions
        let mut index: BTreeMap<Vec<u8>, BTreeMap<i64, Vec<Vec<u32>>>> = BTreeMap::new();
        let mut col_totals = alloc::vec![0u64; ncols];
        let mut doc_sizes: Vec<(i64, Vec<u64>)> = Vec::new();
        for (rowid, values) in &docs {
            let mut sizes = alloc::vec![0u64; ncols];
            for c in 0..ncols {
                let text = match values.get(c + 1) {
                    Some(v) if !matches!(v, Value::Null) => eval::to_text(v),
                    _ => String::new(),
                };
                let toks = crate::vtab::fts5_tokenize(&text, tok);
                sizes[c] = toks.len() as u64;
                col_totals[c] += toks.len() as u64;
                for (pos, tok) in toks.iter().enumerate() {
                    index
                        .entry(tok.as_bytes().to_vec())
                        .or_default()
                        .entry(*rowid)
                        .or_insert_with(|| alloc::vec![Vec::new(); ncols])[c]
                        .push(pos as u32);
                }
            }
            doc_sizes.push((*rowid, sizes));
        }
        let terms: Vec<(Vec<u8>, Vec<Posting>)> = index
            .into_iter()
            .map(|(term, per_doc)| {
                let postings = per_doc
                    .into_iter()
                    .map(|(rowid, cols)| Posting { rowid, cols })
                    .collect();
                (term, postings)
            })
            .collect();

        let seg =
            fts5_index::build_segment(&terms, docs.len() as u64, &col_totals, &doc_sizes, 4050, 0);

        let q = |s: &str| sql::print::ident(s);
        let pv = |vals: Vec<Value>| Params {
            positional: vals,
            named: Vec::new(),
        };
        self.execute(&format!("DELETE FROM {}", q(&format!("{name}_data"))))?;
        self.execute(&format!("DELETE FROM {}", q(&format!("{name}_idx"))))?;
        self.execute(&format!("DELETE FROM {}", q(&format!("{name}_docsize"))))?;
        let data_t = q(&format!("{name}_data"));
        for (id, block) in &seg.data {
            self.execute_params(
                &format!("INSERT INTO {data_t} VALUES(?1,?2)"),
                &pv(alloc::vec![Value::Integer(*id), Value::Blob(block.clone())]),
            )?;
        }
        let idx_t = q(&format!("{name}_idx"));
        for IdxRow { segid, term, pgno } in &seg.idx {
            self.execute_params(
                &format!("INSERT INTO {idx_t} VALUES(?1,?2,?3)"),
                &pv(alloc::vec![
                    Value::Integer(*segid),
                    Value::Blob(term.clone()),
                    Value::Integer(*pgno)
                ]),
            )?;
        }
        let docsize_t = q(&format!("{name}_docsize"));
        for (rowid, sz) in &seg.docsize {
            self.execute_params(
                &format!("INSERT INTO {docsize_t} VALUES(?1,?2)"),
                &pv(alloc::vec![Value::Integer(*rowid), Value::Blob(sz.clone())]),
            )?;
        }
        Ok(())
    }

    fn scan_table(&self, meta: &TableMeta) -> Result<Vec<(i64, Vec<Value>)>> {
        let encoding = self.backend.source().header().text_encoding;
        let mut rows = Vec::new();
        let mut cur = TableCursor::new(self.backend.source(), meta.root);
        let mut ok = cur.first()?;
        while ok {
            let rowid = cur.rowid()?;
            let values = self.decode_full_row(meta, rowid, &cur.payload()?, encoding)?;
            rows.push((rowid, values));
            ok = cur.next()?;
        }
        Ok(rows)
    }

    /// Decode a stored row into full column values: pad missing trailing columns
    /// with their `DEFAULT` (or NULL), and fill the INTEGER PRIMARY KEY column
    /// from the rowid. This is how `ALTER TABLE ADD COLUMN` defaults show up for
    /// rows written before the column existed.
    fn decode_full_row(
        &self,
        meta: &TableMeta,
        rowid: i64,
        payload: &[u8],
        encoding: crate::format::TextEncoding,
    ) -> Result<Vec<Value>> {
        let record = decode_record(payload, encoding)?;
        let n = meta.columns.len();
        let mut values = alloc::vec![Value::Null; n];
        let p = Params::default();
        // Map stored record values onto declared columns, skipping VIRTUAL
        // generated columns (which occupy no record slot). A record shorter than
        // the stored-column count means columns added by ALTER use their default.
        let mut ri = 0usize;
        for (i, def) in meta.defaults.iter().enumerate() {
            if meta.is_virtual(i) {
                continue;
            }
            if ri < record.len() {
                values[i] = record[ri].clone();
            } else if let Some(e) = def {
                values[i] = eval::eval(e, &EvalCtx::rowless(&p))?;
            }
            ri += 1;
        }
        if let Some(ipk) = meta.ipk {
            values[ipk] = Value::Integer(rowid);
        }
        self.compute_generated(meta, &mut values, &p)?;
        Ok(values)
    }

    /// Fill in the VIRTUAL generated columns of `values` (computed on read).
    /// STORED generated columns are read back from the record, not recomputed.
    fn compute_generated(
        &self,
        meta: &TableMeta,
        values: &mut [Value],
        params: &Params,
    ) -> Result<()> {
        if meta.generated.iter().all(|g| g.is_none()) {
            return Ok(());
        }
        for i in 0..meta.columns.len() {
            if let Some((expr, stored)) = &meta.generated[i] {
                if !stored {
                    let ctx = row_ctx(values, &meta.columns, None, params).with_subqueries(self);
                    let v = eval::eval(expr, &ctx)?;
                    values[i] = meta.columns[i].affinity.coerce(v);
                }
            }
        }
        Ok(())
    }

    /// Materialize all generated columns (STORED and VIRTUAL) into `values`,
    /// applied on the write path so CHECK/UNIQUE/indexes see their values.
    fn materialize_generated(
        &self,
        meta: &TableMeta,
        values: &mut [Value],
        params: &Params,
    ) -> Result<()> {
        if meta.generated.iter().all(|g| g.is_none()) {
            return Ok(());
        }
        for i in 0..meta.columns.len() {
            if let Some((expr, _)) = &meta.generated[i] {
                let ctx = row_ctx(values, &meta.columns, None, params).with_subqueries(self);
                let v = eval::eval(expr, &ctx)?;
                values[i] = meta.columns[i].affinity.coerce(v);
            }
        }
        Ok(())
    }

    /// Encode a table record from `values`, omitting VIRTUAL generated columns
    /// (not stored) and nulling the rowid-aliased `INTEGER PRIMARY KEY`.
    fn encode_table_record(&self, meta: &TableMeta, values: &[Value]) -> Vec<u8> {
        let stored: Vec<Value> = (0..meta.columns.len())
            .filter(|&i| !meta.is_virtual(i))
            .map(|i| {
                if Some(i) == meta.ipk {
                    Value::Null
                } else {
                    values[i].clone()
                }
            })
            .collect();
        encode_record(&stored)
    }

    /// Non-aggregated projection: one output row per input row.
    fn eval_simple(
        &self,
        sel: &Select,
        columns: &[ColumnInfo],
        rows: Vec<InputRow>,
        params: &Params,
    ) -> Result<(Vec<String>, Vec<OutRow>)> {
        let labels = self.output_labels(sel, columns);
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let ctx = r.ctx(columns, params).with_subqueries(self);
            let mut values = Vec::new();
            for col in &sel.columns {
                project_column(col, columns, &ctx, &mut values)?;
            }
            // ORDER BY: resolve by position/alias against the output, else
            // evaluate against the input row (allows ordering by unselected cols).
            let mut sort_keys = Vec::new();
            for term in &sel.order_by {
                match resolve_order_index(&term.expr, &labels, values.len()) {
                    Some(idx) => sort_keys.push(values[idx].clone()),
                    None => sort_keys.push(eval::eval(&term.expr, &ctx)?),
                }
            }
            out.push(OutRow { values, sort_keys });
        }
        Ok((labels, out))
    }

    /// Aggregated/grouped projection.
    fn eval_aggregated(
        &self,
        sel: &Select,
        columns: &[ColumnInfo],
        rows: Vec<InputRow>,
        params: &Params,
    ) -> Result<(Vec<String>, Vec<OutRow>)> {
        // Expand any `*` / `table.*` into explicit column references so the
        // bare-column rule below applies to them (SQLite allows `SELECT *,
        // count(*) …`, each bare column taking the representative row's value).
        let expanded;
        let sel = if sel
            .columns
            .iter()
            .any(|c| matches!(c, ResultColumn::Wildcard | ResultColumn::TableWildcard(_)))
        {
            expanded = expand_agg_wildcards(sel, columns);
            &expanded
        } else {
            sel
        };

        // Resolve a positional `GROUP BY N` (an integer literal) to the N-th
        // output column's expression, matching sqlite — `GROUP BY 1` groups by
        // the first result column, not by the constant 1. (Range was already
        // validated upstream by `check_positional_terms`.)
        let group_by: Vec<Expr> = sel
            .group_by
            .iter()
            .map(|g| {
                if let Expr::Literal(Literal::Integer(n)) = g {
                    if *n >= 1 {
                        if let Some(ResultColumn::Expr { expr, .. }) =
                            sel.columns.get((*n - 1) as usize)
                        {
                            return expr.clone();
                        }
                    }
                }
                g.clone()
            })
            .collect();

        // Partition rows into groups (first-seen order), comparing each grouping
        // key under its column collation.
        let group_colls: Vec<crate::value::Collation> = {
            let cctx = row_ctx(&[], columns, None, params);
            group_by
                .iter()
                .map(|g| eval::key_collation(g, &cctx))
                .collect()
        };
        let mut group_keys: Vec<Vec<Value>> = Vec::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let ctx = r.ctx(columns, params).with_subqueries(self);
            let mut key = Vec::new();
            for g in &group_by {
                key.push(eval::eval(g, &ctx)?);
            }
            match group_keys
                .iter()
                .position(|k| rows_equal_coll(k, &key, &group_colls))
            {
                Some(idx) => groups[idx].push(i),
                None => {
                    group_keys.push(key);
                    groups.push(alloc::vec![i]);
                }
            }
        }
        // No GROUP BY but aggregates present => a single group over all rows
        // (which yields one row even when there are zero input rows).
        if sel.group_by.is_empty() {
            groups = alloc::vec![(0..rows.len()).collect()];
        } else {
            // SQLite emits grouped rows ordered by the GROUP BY keys (ascending,
            // under each key's collation, NULLs first) — its grouping is done via
            // a sort. An explicit ORDER BY re-sorts later; with none, this is the
            // order. Reorder the groups (and their keys) to match.
            let mut order: Vec<usize> = (0..groups.len()).collect();
            order.sort_by(|&i, &j| {
                for (k, coll) in group_colls.iter().enumerate() {
                    let ord =
                        crate::value::cmp_values_coll(&group_keys[i][k], &group_keys[j][k], *coll);
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
            let mut sorted = Vec::with_capacity(groups.len());
            for i in order {
                sorted.push(core::mem::take(&mut groups[i]));
            }
            groups = sorted;
        }

        let labels = self.output_labels(sel, columns);
        // SQLite's bare-column rule: with exactly one min()/max(), bare columns
        // come from the row achieving that extreme (else the group's first row).
        let minmax = single_minmax_arg(sel);
        let mut out = Vec::new();
        for group in &groups {
            // Representative row context for bare column references.
            let repr_idx = match &minmax {
                Some((is_max, arg)) => {
                    self.argextreme_row(group, columns, &rows, arg, *is_max, params)?
                }
                None => group.first().copied(),
            };
            let repr = repr_idx.map(|i| &rows[i]);
            let empty = InputRow {
                values: alloc::vec![Value::Null; columns.len()],
                rowid: None,
            };
            let repr_ctx = repr
                .unwrap_or(&empty)
                .ctx(columns, params)
                .with_subqueries(self);

            // Compute the output row, substituting aggregate calls with values.
            let mut values = Vec::new();
            for col in &sel.columns {
                let ResultColumn::Expr { expr, .. } = col else {
                    unreachable!("wildcards rejected above")
                };
                let substituted =
                    self.substitute_aggregates(expr, columns, &rows, group, params)?;
                values.push(eval::eval(&substituted, &repr_ctx)?);
            }

            // HAVING (aggregate-aware). It may reference SELECT-output aliases, so
            // evaluate against a context that also exposes the output columns by
            // their labels (table columns still take precedence).
            if let Some(having) = &sel.having {
                let h = self.substitute_aggregates(having, columns, &rows, group, params)?;
                let mut aug_cols = columns.to_vec();
                for label in &labels {
                    aug_cols.push(ColumnInfo {
                        name: label.clone(),
                        table: String::new(),
                        affinity: eval::Affinity::Blob,
                        collation: crate::value::Collation::Binary,
                    });
                }
                let mut aug_vals = repr.unwrap_or(&empty).values.clone();
                aug_vals.extend(values.iter().cloned());
                let aug_row = InputRow {
                    values: aug_vals,
                    rowid: repr.and_then(|r| r.rowid),
                };
                let actx = aug_row.ctx(&aug_cols, params).with_subqueries(self);
                if eval::truth(&eval::eval(&h, &actx)?) != Some(true) {
                    continue;
                }
            }

            // Sort keys (aggregate-aware) for ORDER BY.
            let mut sort_keys = Vec::new();
            for term in &sel.order_by {
                if let Some(idx) = resolve_order_index(&term.expr, &labels, values.len()) {
                    sort_keys.push(values[idx].clone());
                } else {
                    let s =
                        self.substitute_aggregates(&term.expr, columns, &rows, group, params)?;
                    sort_keys.push(eval::eval(&s, &repr_ctx)?);
                }
            }
            out.push(OutRow { values, sort_keys });
        }
        Ok((labels, out))
    }

    /// Replace every aggregate call (an aggregate function with no `OVER`) inside
    /// `e` — including ones nested in window-function arguments and in a window's
    /// `PARTITION BY` / `ORDER BY` — with a reference to a synthetic `__aggN`
    /// column, recording each original aggregate expression in `aggs` (its index
    /// = N). The rewritten expression has no aggregates, only window functions and
    /// column references, so it evaluates against the per-group rows that carry the
    /// materialized aggregate values.
    fn extract_aggregates(&self, e: &Expr, aggs: &mut Vec<Expr>) -> Expr {
        let is_agg = matches!(e, Expr::Function { name, args, star, over: None, .. }
            if func::is_aggregate_call(name, args.len(), *star)
                || self.aggregates.contains_key(&name.to_ascii_lowercase()));
        if is_agg {
            let idx = aggs.len();
            aggs.push(e.clone());
            return Expr::Column {
                table: None,
                column: alloc::format!("__agg{idx}"),
            };
        }
        match e {
            Expr::Function {
                name,
                distinct,
                args,
                star,
                filter,
                order_by,
                over,
            } => {
                let new_args = args
                    .iter()
                    .map(|a| self.extract_aggregates(a, aggs))
                    .collect();
                let new_filter = filter
                    .as_ref()
                    .map(|f| Box::new(self.extract_aggregates(f, aggs)));
                // Recurse into the window spec's PARTITION/ORDER expressions, which
                // may themselves contain aggregates (`row_number() OVER (ORDER BY
                // sum(v))`).
                let new_over = over.as_ref().map(|spec| {
                    let mut s = spec.clone();
                    s.partition_by = spec
                        .partition_by
                        .iter()
                        .map(|p| self.extract_aggregates(p, aggs))
                        .collect();
                    s.order_by = spec
                        .order_by
                        .iter()
                        .map(|t| OrderTerm {
                            expr: self.extract_aggregates(&t.expr, aggs),
                            descending: t.descending,
                            nulls_first: t.nulls_first,
                        })
                        .collect();
                    s
                });
                Expr::Function {
                    name: name.clone(),
                    distinct: *distinct,
                    args: new_args,
                    star: *star,
                    filter: new_filter,
                    order_by: order_by.clone(),
                    over: new_over,
                }
            }
            Expr::Binary { op, left, right } => Expr::Binary {
                op: *op,
                left: Box::new(self.extract_aggregates(left, aggs)),
                right: Box::new(self.extract_aggregates(right, aggs)),
            },
            Expr::Unary { op, expr } => Expr::Unary {
                op: *op,
                expr: Box::new(self.extract_aggregates(expr, aggs)),
            },
            Expr::Paren(x) => Expr::Paren(Box::new(self.extract_aggregates(x, aggs))),
            Expr::Cast { expr, type_name } => Expr::Cast {
                expr: Box::new(self.extract_aggregates(expr, aggs)),
                type_name: type_name.clone(),
            },
            Expr::Collate { expr, collation } => Expr::Collate {
                expr: Box::new(self.extract_aggregates(expr, aggs)),
                collation: collation.clone(),
            },
            Expr::IsNull { expr, negated } => Expr::IsNull {
                expr: Box::new(self.extract_aggregates(expr, aggs)),
                negated: *negated,
            },
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => Expr::Between {
                expr: Box::new(self.extract_aggregates(expr, aggs)),
                low: Box::new(self.extract_aggregates(low, aggs)),
                high: Box::new(self.extract_aggregates(high, aggs)),
                negated: *negated,
            },
            Expr::InList {
                expr,
                list,
                negated,
            } => Expr::InList {
                expr: Box::new(self.extract_aggregates(expr, aggs)),
                list: list
                    .iter()
                    .map(|x| self.extract_aggregates(x, aggs))
                    .collect(),
                negated: *negated,
            },
            Expr::Case {
                operand,
                when_then,
                else_result,
            } => Expr::Case {
                operand: operand
                    .as_ref()
                    .map(|o| Box::new(self.extract_aggregates(o, aggs))),
                when_then: when_then
                    .iter()
                    .map(|(w, t)| {
                        (
                            self.extract_aggregates(w, aggs),
                            self.extract_aggregates(t, aggs),
                        )
                    })
                    .collect(),
                else_result: else_result
                    .as_ref()
                    .map(|x| Box::new(self.extract_aggregates(x, aggs))),
            },
            Expr::RowValue(items) => Expr::RowValue(
                items
                    .iter()
                    .map(|x| self.extract_aggregates(x, aggs))
                    .collect(),
            ),
            // Literals, columns, parameters, and subqueries pass through (a
            // subquery's own aggregates belong to that subquery's scope).
            other => other.clone(),
        }
    }

    /// Evaluate a query that combines `GROUP BY`/aggregates with window functions.
    /// SQLite applies window functions *after* grouping — each window operates on
    /// the post-aggregation rows, and an aggregate inside a window argument or
    /// spec is the group's aggregate. We materialize each group into one row
    /// carrying its aggregate values (as `__aggN` columns), rewrite the query to
    /// reference those columns, apply `HAVING`, run the windows over the grouped
    /// rows, then project. Returns `(labels, rows)` like the other eval paths.
    fn eval_windowed_aggregate(
        &self,
        sel: &Select,
        columns: &[ColumnInfo],
        rows: Vec<InputRow>,
        params: &Params,
    ) -> Result<(Vec<String>, Vec<OutRow>)> {
        // `*` over a grouped+windowed query is rare and would need representative-
        // row expansion alongside the synthetic columns; defer it (errors as
        // before) rather than risk a wrong column set.
        if sel
            .columns
            .iter()
            .any(|c| matches!(c, ResultColumn::Wildcard | ResultColumn::TableWildcard(_)))
        {
            return Err(Error::Unsupported(
                "SELECT * with window functions over GROUP BY",
            ));
        }
        // Output labels reflect the ORIGINAL expressions (e.g. the verbatim
        // `sum(sum(v)) OVER ()`), so compute them before any rewrite.
        let labels = self.output_labels(sel, columns);

        // --- Partition rows into groups (mirrors eval_aggregated). ---
        let group_by: Vec<Expr> = sel
            .group_by
            .iter()
            .map(|g| {
                if let Expr::Literal(Literal::Integer(n)) = g {
                    if *n >= 1 {
                        if let Some(ResultColumn::Expr { expr, .. }) =
                            sel.columns.get((*n - 1) as usize)
                        {
                            return expr.clone();
                        }
                    }
                }
                g.clone()
            })
            .collect();
        let group_colls: Vec<crate::value::Collation> = {
            let cctx = row_ctx(&[], columns, None, params);
            group_by
                .iter()
                .map(|g| eval::key_collation(g, &cctx))
                .collect()
        };
        let mut group_keys: Vec<Vec<Value>> = Vec::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let ctx = r.ctx(columns, params).with_subqueries(self);
            let mut key = Vec::new();
            for g in &group_by {
                key.push(eval::eval(g, &ctx)?);
            }
            match group_keys
                .iter()
                .position(|k| rows_equal_coll(k, &key, &group_colls))
            {
                Some(idx) => groups[idx].push(i),
                None => {
                    group_keys.push(key);
                    groups.push(alloc::vec![i]);
                }
            }
        }
        if sel.group_by.is_empty() {
            groups = alloc::vec![(0..rows.len()).collect()];
        } else {
            let mut order: Vec<usize> = (0..groups.len()).collect();
            order.sort_by(|&i, &j| {
                for (k, coll) in group_colls.iter().enumerate() {
                    let ord =
                        crate::value::cmp_values_coll(&group_keys[i][k], &group_keys[j][k], *coll);
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
            let mut sorted = Vec::with_capacity(groups.len());
            for i in order {
                sorted.push(core::mem::take(&mut groups[i]));
            }
            groups = sorted;
        }

        // --- Rewrite the query so each aggregate becomes a `__aggN` column. ---
        let mut aggs: Vec<Expr> = Vec::new();
        let mut rsel = sel.clone();
        for col in &mut rsel.columns {
            if let ResultColumn::Expr { expr, .. } = col {
                *expr = self.extract_aggregates(expr, &mut aggs);
            }
        }
        if let Some(h) = rsel.having.take() {
            rsel.having = Some(self.extract_aggregates(&h, &mut aggs));
        }
        for t in &mut rsel.order_by {
            t.expr = self.extract_aggregates(&t.expr, &mut aggs);
        }
        // Named WINDOW definitions (`WINDOW w AS (ORDER BY sum(v))`) referenced via
        // `OVER w` carry their PARTITION/ORDER expressions here, not in the call's
        // own spec, so rewrite their aggregates too.
        for (_, ws) in &mut rsel.window_defs {
            for p in &mut ws.partition_by {
                *p = self.extract_aggregates(p, &mut aggs);
            }
            for t in &mut ws.order_by {
                t.expr = self.extract_aggregates(&t.expr, &mut aggs);
            }
        }

        // --- Augment the column set with one synthetic column per aggregate. ---
        let mut cols: Vec<ColumnInfo> = columns.to_vec();
        for i in 0..aggs.len() {
            cols.push(ColumnInfo {
                name: alloc::format!("__agg{i}"),
                table: String::new(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            });
        }

        // --- One grouped row per group: representative base values ++ aggregate
        //     values (computed over the group via the existing machinery). ---
        let empty = InputRow {
            values: alloc::vec![Value::Null; columns.len()],
            rowid: None,
        };
        let mut grows: Vec<InputRow> = Vec::with_capacity(groups.len());
        for group in &groups {
            let repr_idx = group.first().copied();
            let repr = repr_idx.map(|i| &rows[i]).unwrap_or(&empty);
            let repr_ctx = repr.ctx(columns, params).with_subqueries(self);
            let mut vals = repr.values.clone();
            for agg in &aggs {
                let sub = self.substitute_aggregates(agg, columns, &rows, group, params)?;
                vals.push(eval::eval(&sub, &repr_ctx)?);
            }
            grows.push(InputRow {
                values: vals,
                rowid: repr_idx.and_then(|i| rows[i].rowid),
            });
        }

        // --- HAVING (now over the grouped rows; references `__aggN`). ---
        if let Some(having) = &rsel.having {
            let mut kept = Vec::with_capacity(grows.len());
            for r in grows {
                let ctx = r.ctx(&cols, params).with_subqueries(self);
                if eval::truth(&eval::eval(having, &ctx)?) == Some(true) {
                    kept.push(r);
                }
            }
            grows = kept;
        }

        // --- Window functions over the grouped rows, then project. ---
        let mut wcols = cols;
        let win_sel = self.apply_windows(&rsel, &mut wcols, &mut grows, params)?;
        let mut out = Vec::with_capacity(grows.len());
        for r in &grows {
            let ctx = r.ctx(&wcols, params).with_subqueries(self);
            let mut values = Vec::new();
            for col in &win_sel.columns {
                project_column(col, &wcols, &ctx, &mut values)?;
            }
            let mut sort_keys = Vec::new();
            for term in &win_sel.order_by {
                match resolve_order_index(&term.expr, &labels, values.len()) {
                    Some(idx) => sort_keys.push(values[idx].clone()),
                    None => sort_keys.push(eval::eval(&term.expr, &ctx)?),
                }
            }
            out.push(OutRow { values, sort_keys });
        }
        Ok((labels, out))
    }

    /// The index (into `rows`) of the group member achieving the maximum (or
    /// minimum) value of `arg`, ignoring NULLs; falls back to the group's first
    /// row when every value is NULL. Implements SQLite's bare-column min/max rule.
    fn argextreme_row(
        &self,
        group: &[usize],
        columns: &[ColumnInfo],
        rows: &[InputRow],
        arg: &Expr,
        is_max: bool,
        params: &Params,
    ) -> Result<Option<usize>> {
        let mut best: Option<(usize, Value)> = None;
        for &i in group {
            let ctx = rows[i].ctx(columns, params).with_subqueries(self);
            let v = eval::eval(arg, &ctx)?;
            if matches!(v, Value::Null) {
                continue;
            }
            let take = match &best {
                None => true,
                Some((_, bv)) => {
                    let ord = eval::compare(&v, bv);
                    if is_max {
                        ord == core::cmp::Ordering::Greater
                    } else {
                        ord == core::cmp::Ordering::Less
                    }
                }
            };
            if take {
                best = Some((i, v));
            }
        }
        Ok(best.map(|(i, _)| i).or_else(|| group.first().copied()))
    }

    /// Replace aggregate function calls in `expr` with their computed values for
    /// the given group, returning an aggregate-free expression.
    fn substitute_aggregates(
        &self,
        expr: &Expr,
        columns: &[ColumnInfo],
        rows: &[InputRow],
        group: &[usize],
        params: &Params,
    ) -> Result<Expr> {
        Ok(match expr {
            Expr::Function {
                name,
                distinct,
                args,
                star,
                filter,
                order_by,
                over: None,
            } if func::is_aggregate_call(name, args.len(), *star)
                || self.aggregates.contains_key(&name.to_ascii_lowercase()) =>
            {
                // `FILTER (WHERE …)` narrows the group's rows before aggregating.
                let filtered;
                let group = match filter {
                    Some(pred) => {
                        filtered = self.filter_group(pred, columns, rows, group, params)?;
                        &filtered[..]
                    }
                    None => group,
                };
                let v = self.compute_aggregate(
                    name, *distinct, args, *star, order_by, columns, rows, group, params,
                )?;
                let lit = Expr::Literal(value_to_literal(v));
                // The JSON aggregates emit a value carrying SQLite's JSON subtype.
                // Substitution to a bare literal would drop that, so an enclosing
                // json_quote/json_array/json_object would re-quote it. Re-wrap in
                // json() (idempotent on valid JSON) so the subtype marker — which
                // func::produces_json keys off the expression — survives.
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "json_group_array" | "json_group_object"
                ) {
                    Expr::Function {
                        name: String::from("json"),
                        distinct: false,
                        args: alloc::vec![lit],
                        star: false,
                        filter: None,
                        order_by: Vec::new(),
                        over: None,
                    }
                } else {
                    lit
                }
            }
            Expr::Function {
                name,
                distinct,
                args,
                star,
                filter,
                order_by,
                over,
            } => {
                let mut new_args = Vec::with_capacity(args.len());
                for a in args {
                    new_args.push(self.substitute_aggregates(a, columns, rows, group, params)?);
                }
                Expr::Function {
                    name: name.clone(),
                    distinct: *distinct,
                    args: new_args,
                    star: *star,
                    filter: filter.clone(),
                    order_by: order_by.clone(),
                    over: over.clone(),
                }
            }
            Expr::Binary { op, left, right } => Expr::Binary {
                op: *op,
                left: Box::new(self.substitute_aggregates(left, columns, rows, group, params)?),
                right: Box::new(self.substitute_aggregates(right, columns, rows, group, params)?),
            },
            Expr::Unary { op, expr } => Expr::Unary {
                op: *op,
                expr: Box::new(self.substitute_aggregates(expr, columns, rows, group, params)?),
            },
            Expr::Paren(e) => Expr::Paren(Box::new(
                self.substitute_aggregates(e, columns, rows, group, params)?,
            )),
            Expr::Cast { expr, type_name } => Expr::Cast {
                expr: Box::new(self.substitute_aggregates(expr, columns, rows, group, params)?),
                type_name: type_name.clone(),
            },
            Expr::IsNull { expr, negated } => Expr::IsNull {
                expr: Box::new(self.substitute_aggregates(expr, columns, rows, group, params)?),
                negated: *negated,
            },
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => Expr::Between {
                expr: Box::new(self.substitute_aggregates(expr, columns, rows, group, params)?),
                low: Box::new(self.substitute_aggregates(low, columns, rows, group, params)?),
                high: Box::new(self.substitute_aggregates(high, columns, rows, group, params)?),
                negated: *negated,
            },
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let mut new_list = Vec::with_capacity(list.len());
                for e in list {
                    new_list.push(self.substitute_aggregates(e, columns, rows, group, params)?);
                }
                Expr::InList {
                    expr: Box::new(self.substitute_aggregates(expr, columns, rows, group, params)?),
                    list: new_list,
                    negated: *negated,
                }
            }
            Expr::Case {
                operand,
                when_then,
                else_result,
            } => {
                let operand = match operand {
                    Some(o) => Some(Box::new(
                        self.substitute_aggregates(o, columns, rows, group, params)?,
                    )),
                    None => None,
                };
                let mut new_wt = Vec::with_capacity(when_then.len());
                for (w, t) in when_then {
                    new_wt.push((
                        self.substitute_aggregates(w, columns, rows, group, params)?,
                        self.substitute_aggregates(t, columns, rows, group, params)?,
                    ));
                }
                let else_result = match else_result {
                    Some(e) => Some(Box::new(
                        self.substitute_aggregates(e, columns, rows, group, params)?,
                    )),
                    None => None,
                };
                Expr::Case {
                    operand,
                    when_then: new_wt,
                    else_result,
                }
            }
            // Literals, columns, parameters, and subqueries are left as-is
            // (a subquery's own aggregates belong to that subquery).
            other => other.clone(),
        })
    }

    /// The subset of `group`'s row indices for which `pred` (an aggregate
    /// `FILTER (WHERE …)`) evaluates true.
    fn filter_group(
        &self,
        pred: &Expr,
        columns: &[ColumnInfo],
        rows: &[InputRow],
        group: &[usize],
        params: &Params,
    ) -> Result<Vec<usize>> {
        let mut out = Vec::new();
        for &i in group {
            let ctx = rows[i].ctx(columns, params).with_subqueries(self);
            if eval::truth(&eval::eval(pred, &ctx)?) == Some(true) {
                out.push(i);
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn compute_aggregate(
        &self,
        name: &str,
        distinct: bool,
        args: &[Expr],
        star: bool,
        order_by: &[OrderTerm],
        columns: &[ColumnInfo],
        rows: &[InputRow],
        group: &[usize],
        params: &Params,
    ) -> Result<Value> {
        let lname = name.to_ascii_lowercase();

        // Arity guards: every aggregate but `count(*)` needs at least one
        // argument, and `json_group_object` needs two. SQLite rejects a short
        // call ("wrong number of arguments"); without this we would index
        // `args[…]` out of bounds and panic (e.g. `group_concat()`).
        if !star && args.is_empty() {
            return Err(Error::Error(format!(
                "wrong number of arguments to function {lname}()"
            )));
        }
        if (lname == "json_group_object" || lname == "jsonb_group_object") && args.len() < 2 {
            return Err(Error::Error(format!(
                "wrong number of arguments to function {lname}()"
            )));
        }

        // An `ORDER BY` inside the aggregate (`group_concat(x ORDER BY y)`) sorts
        // the group's rows before the values are gathered.
        let ordered_group;
        let group = if order_by.is_empty() {
            group
        } else {
            let mut g = group.to_vec();
            let mut err = None;
            g.sort_by(|&a, &b| {
                for term in order_by {
                    let ca = rows[a].ctx(columns, params).with_subqueries(self);
                    let cb = rows[b].ctx(columns, params).with_subqueries(self);
                    let (va, vb) = match (eval::eval(&term.expr, &ca), eval::eval(&term.expr, &cb))
                    {
                        (Ok(x), Ok(y)) => (x, y),
                        (Err(e), _) | (_, Err(e)) => {
                            err.get_or_insert(e);
                            return core::cmp::Ordering::Equal;
                        }
                    };
                    let coll = eval::key_collation(&term.expr, &ca);
                    let ord = cmp_order(&va, &vb, term.descending, term.nulls_first, coll);
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
            if let Some(e) = err {
                return Err(e);
            }
            ordered_group = g;
            &ordered_group[..]
        };

        // JSON aggregates build their result directly from the (NULL-inclusive,
        // possibly multi-argument) per-row values, so they bypass the NULL-
        // stripping single-value collection used by the other aggregates.
        if lname == "json_group_array" || lname == "jsonb_group_array" {
            let mut vals = Vec::new();
            for &i in group {
                let ctx = rows[i].ctx(columns, params).with_subqueries(self);
                vals.push(eval::eval(&args[0], &ctx)?);
            }
            // `json_group_array(DISTINCT x)` dedupes the values (first-seen order),
            // like other DISTINCT aggregates, before serializing.
            if distinct {
                dedup_values(&mut vals, crate::value::Collation::default());
            }
            let items: Vec<_> = vals
                .iter()
                .map(|v| func::arg_to_json(v, args.first()))
                .collect();
            let arr = json::Json::Array(items);
            return Ok(if lname.starts_with("jsonb") {
                Value::Blob(arr.to_jsonb())
            } else {
                Value::Text(arr.serialize())
            });
        }
        if lname == "json_group_object" || lname == "jsonb_group_object" {
            let mut pairs = Vec::new();
            for &i in group {
                let ctx = rows[i].ctx(columns, params).with_subqueries(self);
                let k = eval::eval(&args[0], &ctx)?;
                let v = eval::eval(&args[1], &ctx)?;
                pairs.push((eval::to_text(&k), func::arg_to_json(&v, args.get(1))));
            }
            let obj = json::Json::Object(pairs);
            return Ok(if lname.starts_with("jsonb") {
                Value::Blob(obj.to_jsonb())
            } else {
                Value::Text(obj.serialize())
            });
        }

        // Gather the (non-NULL for most) argument values across the group.
        let mut vals: Vec<Value> = Vec::new();
        let mut count_rows = 0usize; // for count(*)
        for &i in group {
            count_rows += 1;
            if star {
                continue;
            }
            let ctx = rows[i].ctx(columns, params).with_subqueries(self);
            let v = eval::eval(&args[0], &ctx)?;
            if !matches!(v, Value::Null) {
                vals.push(v);
            }
        }
        if distinct {
            let coll = if star || args.is_empty() {
                crate::value::Collation::default()
            } else {
                let cctx = row_ctx(&[], columns, None, params);
                eval::key_collation(&args[0], &cctx)
            };
            dedup_values(&mut vals, coll);
        }

        // `min`/`max` compare under the argument's collation (e.g. a NOCASE
        // column), not plain BINARY.
        let arg_coll = if args.is_empty() {
            crate::value::Collation::default()
        } else {
            let cctx = row_ctx(&[], columns, None, params);
            eval::key_collation(&args[0], &cctx)
        };

        Ok(match lname.as_str() {
            "count" => {
                if star {
                    Value::Integer(count_rows as i64)
                } else {
                    Value::Integer(vals.len() as i64)
                }
            }
            "sum" => {
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
                        // Like SQLite: an integer `sum()` that overflows i64 is an
                        // error (use `total()` for a non-failing real sum).
                        return Err(Error::Error("integer overflow".into()));
                    } else {
                        Value::Integer(acc)
                    }
                } else {
                    Value::Real(vals.iter().map(eval::to_f64).sum())
                }
            }
            "total" => Value::Real(vals.iter().map(eval::to_f64).sum()),
            "avg" => {
                if vals.is_empty() {
                    Value::Null
                } else {
                    let sum: f64 = vals.iter().map(eval::to_f64).sum();
                    Value::Real(sum / vals.len() as f64)
                }
            }
            "min" => vals
                .into_iter()
                .reduce(|a, b| {
                    if crate::value::cmp_values_coll(&b, &a, arg_coll) == core::cmp::Ordering::Less
                    {
                        b
                    } else {
                        a
                    }
                })
                .unwrap_or(Value::Null),
            "max" => vals
                .into_iter()
                .reduce(|a, b| {
                    if crate::value::cmp_values_coll(&b, &a, arg_coll)
                        == core::cmp::Ordering::Greater
                    {
                        b
                    } else {
                        a
                    }
                })
                .unwrap_or(Value::Null),
            // `string_agg` is SQLite's standard-SQL alias for `group_concat`.
            "group_concat" | "string_agg" => {
                if vals.is_empty() {
                    Value::Null
                } else {
                    let sep = if args.len() >= 2 {
                        let ctx = EvalCtx::rowless(params);
                        eval::to_text(&eval::eval(&args[1], &ctx)?)
                    } else {
                        ",".to_string()
                    };
                    let parts: Vec<String> = vals.iter().map(eval::to_text).collect();
                    Value::Text(parts.join(&sep))
                }
            }
            _ => {
                // A user-defined aggregate registered via
                // `register_aggregate_function`: build a fresh accumulator, step
                // it over the group's evaluated argument values, then finalize.
                if let Some(factory) = self.aggregates.get(&lname) {
                    let mut acc = factory();
                    let mut seen: Vec<Vec<Value>> = Vec::new();
                    for &i in group {
                        let ctx = rows[i].ctx(columns, params).with_subqueries(self);
                        let vals: Vec<Value> = args
                            .iter()
                            .map(|a| eval::eval(a, &ctx))
                            .collect::<Result<_>>()?;
                        if distinct {
                            if seen.contains(&vals) {
                                continue;
                            }
                            seen.push(vals.clone());
                        }
                        acc.step(&vals)?;
                    }
                    return acc.finalize();
                }
                return Err(Error::Error(format!("no such function: {name}")));
            }
        })
    }

    fn has_aggregate(&self, sel: &Select) -> bool {
        // Recognize both built-in and user-registered aggregate names.
        let is_agg = |name: &str, n: usize, star: bool| {
            func::is_aggregate_call(name, n, star)
                || self.aggregates.contains_key(&name.to_ascii_lowercase())
        };
        sel.columns.iter().any(|c| match c {
            ResultColumn::Expr { expr, .. } => expr_contains_agg(expr, &is_agg),
            _ => false,
        }) || sel
            .having
            .as_ref()
            .is_some_and(|h| expr_contains_agg(h, &is_agg))
    }

    fn output_labels(&self, sel: &Select, columns: &[ColumnInfo]) -> Vec<String> {
        let mut labels = Vec::new();
        for col in &sel.columns {
            match col {
                ResultColumn::Wildcard => {
                    for c in columns {
                        labels.push(c.name.clone());
                    }
                }
                // `t.*` names only that table's columns (by owning-table qualifier),
                // matching the projected data — over a join a bare `*` lists every
                // column but `t.*` must not.
                ResultColumn::TableWildcard(t) => {
                    for c in columns.iter().filter(|c| c.table.eq_ignore_ascii_case(t)) {
                        labels.push(c.name.clone());
                    }
                }
                ResultColumn::Expr {
                    expr,
                    alias,
                    source,
                } => {
                    labels.push(result_column_label(expr, alias, source));
                }
            }
        }
        labels
    }

    fn table_meta(&self, name: &str, alias: Option<&str>) -> Result<TableMeta> {
        self.table_meta_in(&self.schema, name, alias)
    }

    /// Like [`table_meta`](Self::table_meta) but resolving `name` in an explicit
    /// schema catalog (the `main` schema or an attached database's).
    fn table_meta_in(&self, schema: &Schema, name: &str, alias: Option<&str>) -> Result<TableMeta> {
        // The schema catalog itself is queryable as `sqlite_schema` /
        // `sqlite_master` (a 5-column rowid table rooted at page 1).
        if is_main_schema_table(name) {
            return Ok(schema_table_meta(alias.unwrap_or(name)));
        }
        let obj = schema
            .table(name)
            .ok_or_else(|| Error::Error(alloc::format!("no such table: {name}")))?;
        let sql = obj
            .sql
            .as_ref()
            .ok_or_else(|| Error::Corrupt("table has no CREATE statement".into()))?;
        let Statement::CreateTable(ct) = sql::parse_one(sql)? else {
            return Err(Error::Corrupt("schema sql is not CREATE TABLE".into()));
        };
        let table_label = alias.unwrap_or(name).to_string();
        let columns: Vec<ColumnInfo> = ct
            .columns
            .iter()
            .map(|c| ColumnInfo {
                name: c.name.clone(),
                table: table_label.clone(),
                affinity: eval::Affinity::from_type(c.type_name.as_deref()),
                collation: column_collation(c),
            })
            .collect();
        let defaults: Vec<Option<Expr>> = ct
            .columns
            .iter()
            .map(|c| {
                c.constraints.iter().find_map(|k| match k {
                    ColumnConstraint::Default(e) => Some(e.clone()),
                    _ => None,
                })
            })
            .collect();
        // A WITHOUT ROWID table has no rowid, so `INTEGER PRIMARY KEY` is an
        // ordinary column there (no rowid aliasing).
        let ipk = if ct.without_rowid {
            None
        } else {
            find_integer_primary_key(&ct)
        };
        // `None` = nullable; `Some(action)` = NOT NULL with that conflict action.
        let not_null: Vec<Option<OnConflict>> = ct
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                // The INTEGER PRIMARY KEY (rowid alias) is implicitly NOT NULL.
                if Some(i) == ipk {
                    return Some(OnConflict::Abort);
                }
                c.constraints.iter().find_map(|k| match k {
                    ColumnConstraint::NotNull(oc) => Some(*oc),
                    _ => None,
                })
            })
            .collect();
        // Generated columns: `… AS (expr) [STORED|VIRTUAL]`.
        let generated: Vec<Option<(Expr, bool)>> = ct
            .columns
            .iter()
            .map(|c| {
                c.constraints.iter().find_map(|k| match k {
                    ColumnConstraint::Generated { expr, stored } => Some((expr.clone(), *stored)),
                    _ => None,
                })
            })
            .collect();
        // CHECK constraints (column-level + table-level); each is evaluated
        // against the full row on INSERT/UPDATE.
        let mut checks: Vec<(Expr, Option<String>)> = Vec::new();
        for col in &ct.columns {
            for k in &col.constraints {
                if let ColumnConstraint::Check(e, label) = k {
                    checks.push((e.clone(), label.clone()));
                }
            }
        }
        for tc in &ct.constraints {
            if let TableConstraint::Check(e, label) = tc {
                checks.push((e.clone(), label.clone()));
            }
        }
        // UNIQUE / PRIMARY KEY column sets that must be unique (the rowid IPK is
        // handled separately). Order matches SQLite's auto-index numbering.
        let unique = collect_unique_sets(&ct, ipk);

        // WITHOUT ROWID: derive the PK-first storage order.
        let (without_rowid, storage_order, pk_len) = if ct.without_rowid {
            let pk = primary_key_positions(&ct);
            if pk.is_empty() {
                return Err(Error::Error(
                    "WITHOUT ROWID table must have a PRIMARY KEY".into(),
                ));
            }
            // Storage order: PK columns first, then the remaining *stored*
            // columns (VIRTUAL generated columns are never written).
            let mut order = pk.clone();
            for (i, gen) in generated.iter().enumerate() {
                let is_virtual = matches!(gen, Some((_, false)));
                if !pk.contains(&i) && !is_virtual {
                    order.push(i);
                }
            }
            let pk_len = pk.len();
            (true, order, pk_len)
        } else {
            (false, Vec::new(), 0)
        };

        // STRICT tables: record each column's rigid type for write-time checking,
        // and give `ANY` columns no affinity (values stored exactly as supplied).
        let strict_types: Option<Vec<(StrictType, String)>> = if ct.strict {
            let mut v = Vec::with_capacity(columns.len());
            for c in &ct.columns {
                let st = strict_column_type(c.type_name.as_deref()).unwrap_or(StrictType::Any);
                let decl = c.type_name.clone().unwrap_or_default();
                v.push((st, decl));
            }
            Some(v)
        } else {
            None
        };
        let mut columns = columns;
        if let Some(st) = &strict_types {
            for (col, (ty, _)) in columns.iter_mut().zip(st) {
                if *ty == StrictType::Any {
                    col.affinity = eval::Affinity::Blob; // ANY: store as-is
                }
            }
        }

        Ok(TableMeta {
            root: obj.rootpage,
            columns,
            defaults,
            not_null,
            checks,
            unique,
            ipk,
            generated,
            without_rowid,
            storage_order,
            pk_len,
            strict_types,
            autoincrement: ipk.is_some_and(|i| {
                ct.columns[i].constraints.iter().any(|k| {
                    matches!(
                        k,
                        ColumnConstraint::PrimaryKey {
                            autoincrement: true,
                            ..
                        }
                    )
                })
            }),
        })
    }

    /// Enforce a `STRICT` table's column types against a row whose affinity has
    /// already been applied. NULL always passes; otherwise the stored value's
    /// storage class must match the column's rigid type (`ANY` accepts anything).
    /// `INT`/`REAL` columns accept their numeric class after affinity coercion
    /// (an integer in a `REAL` column has been turned into a real already).
    fn check_strict_types(&self, meta: &TableMeta, values: &[Value]) -> Result<()> {
        let Some(stypes) = &meta.strict_types else {
            return Ok(());
        };
        for (i, (st, decl)) in stypes.iter().enumerate() {
            let v = &values[i];
            let ok = matches!(
                (st, v),
                (_, Value::Null)
                    | (StrictType::Any, _)
                    | (StrictType::Int, Value::Integer(_))
                    | (StrictType::Real, Value::Real(_))
                    | (StrictType::Text, Value::Text(_))
                    | (StrictType::Blob, Value::Blob(_))
            );
            if !ok {
                let class = match v {
                    Value::Integer(_) => "INT",
                    Value::Real(_) => "REAL",
                    Value::Text(_) => "TEXT",
                    Value::Blob(_) => "BLOB",
                    Value::Null => unreachable!(),
                };
                return Err(Error::Constraint(format!(
                    "cannot store {class} value in {decl} column {}.{}",
                    meta.columns[i].table, meta.columns[i].name
                )));
            }
        }
        Ok(())
    }

    /// Evaluate CHECK constraints against a fully-built row (with the IPK column
    /// holding the rowid). A constraint fails only when it evaluates to false;
    /// NULL (unknown) passes, matching SQLite.
    fn check_constraints(
        &self,
        meta: &TableMeta,
        values: &[Value],
        rowid: Option<i64>,
        params: &Params,
    ) -> Result<()> {
        for (expr, label) in &meta.checks {
            let ctx = row_ctx(values, &meta.columns, rowid, params).with_subqueries(self);
            if eval::truth(&eval::eval(expr, &ctx)?) == Some(false) {
                let msg = match label {
                    Some(l) => alloc::format!("CHECK constraint failed: {l}"),
                    None => String::from("CHECK constraint failed"),
                };
                return Err(Error::Constraint(msg));
            }
        }
        Ok(())
    }
}

struct TableMeta {
    root: u32,
    columns: Vec<ColumnInfo>,
    /// Per-column `DEFAULT` expression, if declared (aligned with `columns`).
    defaults: Vec<Option<Expr>>,
    /// Per-column `NOT NULL` flag (aligned with `columns`).
    /// `None` = nullable; `Some(action)` = `NOT NULL` with its `ON CONFLICT` action.
    not_null: Vec<Option<OnConflict>>,
    /// CHECK constraint expressions (column-level and table-level).
    /// CHECK constraints with their error-message label (name or source text).
    checks: Vec<(Expr, Option<String>)>,
    /// Column-index sets that must be UNIQUE (excludes the rowid IPK), each with
    /// its declared `ON CONFLICT` action (default `Abort`).
    unique: Vec<(Vec<usize>, OnConflict)>,
    ipk: Option<usize>,
    /// Per-column generated-column spec `(expr, stored)`, if the column is
    /// `… AS (expr) [STORED|VIRTUAL]`. `VIRTUAL` (stored = false) columns are not
    /// written to disk; `STORED` ones are. Aligned with `columns`.
    generated: Vec<Option<(Expr, bool)>>,
    /// `true` for a `WITHOUT ROWID` table (stored as a PK-clustered index b-tree
    /// rather than a rowid table b-tree).
    without_rowid: bool,
    /// For a `WITHOUT ROWID` table, the on-disk column order: PRIMARY KEY columns
    /// first (in key order), then the remaining columns in declared order. Empty
    /// for ordinary rowid tables. `pk_len` is how many leading entries are PK.
    storage_order: Vec<usize>,
    pk_len: usize,
    /// For a `STRICT` table, each column's rigid type and its declared type name
    /// (aligned with `columns`); `None` for an ordinary table. Drives write-time
    /// type checking.
    strict_types: Option<Vec<(StrictType, String)>>,
    /// `true` when the `INTEGER PRIMARY KEY` is declared `AUTOINCREMENT`: assigned
    /// rowids never reuse a value below the high-water mark persisted in
    /// `sqlite_sequence`, matching SQLite.
    autoincrement: bool,
}

/// Return a copy of `sel` with any `*` / `table.*` result column expanded to
/// explicit table-qualified column references drawn from `columns`. Used by the
/// aggregate path so bare wildcards follow the same representative-row rule as
/// named bare columns.
fn expand_agg_wildcards(sel: &Select, columns: &[ColumnInfo]) -> Select {
    let col_ref = |c: &ColumnInfo| ResultColumn::Expr {
        expr: Expr::Column {
            table: Some(c.table.clone()),
            column: c.name.clone(),
        },
        alias: None,
        source: None,
    };
    let mut new_cols = Vec::new();
    for col in &sel.columns {
        match col {
            ResultColumn::Wildcard => new_cols.extend(columns.iter().map(&col_ref)),
            ResultColumn::TableWildcard(t) => new_cols.extend(
                columns
                    .iter()
                    .filter(|c| c.table.eq_ignore_ascii_case(t))
                    .map(&col_ref),
            ),
            other => new_cols.push(other.clone()),
        }
    }
    let mut s = sel.clone();
    s.columns = new_cols;
    s
}

/// If `sel`'s WHERE/GROUP BY/HAVING reference any SELECT-list alias that is not
/// shadowed by a real input column, return a copy of `sel` with those alias
/// references replaced by their defining expressions (SQLite resolves aliases in
/// these clauses, with real columns winning). Returns `None` when no rewrite is
/// needed, so the common path clones nothing.
fn alias_substituted(sel: &Select, columns: &[ColumnInfo]) -> Option<Select> {
    // Explicit `AS` aliases that don't collide with a real input column name.
    let mut aliases: Vec<(String, Expr)> = Vec::new();
    for c in &sel.columns {
        if let ResultColumn::Expr {
            expr,
            alias: Some(name),
            ..
        } = c
        {
            if !columns
                .iter()
                .any(|col| col.name.eq_ignore_ascii_case(name))
                && !aliases.iter().any(|(a, _)| a.eq_ignore_ascii_case(name))
            {
                aliases.push((name.clone(), expr.clone()));
            }
        }
    }
    if aliases.is_empty() {
        return None;
    }
    // Only rewrite if a clause actually references one of those aliases.
    let mentions = |e: &Expr| -> bool {
        let mut found = false;
        window::visit(e, &mut |n| {
            if let Expr::Column {
                table: None,
                column,
            } = n
            {
                if aliases.iter().any(|(a, _)| a.eq_ignore_ascii_case(column)) {
                    found = true;
                }
            }
        });
        found
    };
    let used = sel.where_clause.as_ref().is_some_and(&mentions)
        || sel.group_by.iter().any(&mentions)
        || sel.having.as_ref().is_some_and(&mentions);
    if !used {
        return None;
    }
    let mut out = sel.clone();
    let apply = |e: &mut Expr| {
        for (name, repl) in &aliases {
            let target = Expr::Column {
                table: None,
                column: name.clone(),
            };
            window::replace_expr(e, &target, repl);
        }
    };
    if let Some(w) = &mut out.where_clause {
        apply(w);
    }
    for g in &mut out.group_by {
        apply(g);
    }
    if let Some(h) = &mut out.having {
        apply(h);
    }
    Some(out)
}

/// Wrap a runtime [`Value`] as a literal [`Expr`], so rows produced by an
/// `INSERT … SELECT` can flow through the ordinary VALUES insert path.
fn value_to_literal_expr(v: Value) -> Expr {
    Expr::Literal(match v {
        Value::Null => Literal::Null,
        Value::Integer(i) => Literal::Integer(i),
        Value::Real(r) => Literal::Real(r),
        Value::Text(s) => Literal::Str(s),
        Value::Blob(b) => Literal::Blob(b),
    })
}

/// Whether `name` refers to the main schema catalog table, which SQLite exposes
/// under both the modern `sqlite_schema` and the historical `sqlite_master`.
fn is_main_schema_table(name: &str) -> bool {
    name.eq_ignore_ascii_case("sqlite_schema") || name.eq_ignore_ascii_case("sqlite_master")
}

/// Whether `name` is the temp-database catalog (`sqlite_temp_schema` /
/// `sqlite_temp_master`), which reads the `temp` database's `sqlite_master`.
fn is_temp_schema_table(name: &str) -> bool {
    name.eq_ignore_ascii_case("sqlite_temp_schema")
        || name.eq_ignore_ascii_case("sqlite_temp_master")
}

/// Reject a direct DML write to the schema catalog, as SQLite does (the catalog
/// is maintained by DDL, not by `INSERT`/`UPDATE`/`DELETE`).
fn reject_schema_write(table: &str) -> Result<()> {
    if is_main_schema_table(table) {
        return Err(Error::Error(alloc::format!(
            "table {table} may not be modified"
        )));
    }
    Ok(())
}

/// A synthetic [`TableMeta`] for the schema catalog (`sqlite_schema`): the
/// 5-column rowid table physically rooted at page 1. Read-only — writes are
/// rejected before reaching here.
fn schema_table_meta(label: &str) -> TableMeta {
    let col = |n: &str, aff: eval::Affinity| ColumnInfo {
        name: n.to_string(),
        table: label.to_string(),
        affinity: aff,
        collation: crate::value::Collation::default(),
    };
    let columns = alloc::vec![
        col("type", eval::Affinity::Text),
        col("name", eval::Affinity::Text),
        col("tbl_name", eval::Affinity::Text),
        col("rootpage", eval::Affinity::Integer),
        col("sql", eval::Affinity::Text),
    ];
    let n = columns.len();
    TableMeta {
        root: crate::schema::SCHEMA_ROOT_PAGE,
        columns,
        defaults: alloc::vec![None; n],
        not_null: alloc::vec![None; n],
        checks: Vec::new(),
        unique: Vec::new(),
        ipk: None,
        generated: alloc::vec![None; n],
        without_rowid: false,
        storage_order: Vec::new(),
        pk_len: 0,
        strict_types: None,
        autoincrement: false,
    }
}

/// The first column reference in `e` that names neither a column in `known` nor
/// (when `allow_rowid`) a rowid alias — the unknown column SQLite rejects at
/// `CREATE` in a CHECK constraint or generated-column expression. Generated
/// columns may not reference the rowid (`allow_rowid=false`); a CHECK may.
fn unknown_column_ref(e: &Expr, known: &[String], allow_rowid: bool) -> Option<String> {
    let mut bad: Option<String> = None;
    window::visit(e, &mut |n| {
        if let Expr::Column { column, .. } = n {
            if bad.is_none()
                && !known.iter().any(|c| c.eq_ignore_ascii_case(column))
                && !(allow_rowid && eval::is_rowid_alias(column))
            {
                bad = Some(column.clone());
            }
        }
    });
    bad
}

/// Whether `e` contains a subquery (scalar `(SELECT …)`, `EXISTS`, or `IN
/// (SELECT …)`) anywhere — SQLite forbids these in CHECK constraints and
/// generated-column expressions.
fn expr_has_subquery(e: &Expr) -> bool {
    let mut found = false;
    window::visit(e, &mut |n| {
        if matches!(
            n,
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSelect { .. }
        ) {
            found = true;
        }
    });
    found
}

/// Whether `e` calls a non-deterministic function — one that can return a
/// different value for the same inputs. SQLite prohibits these in contexts that
/// must be reproducible (index expressions, generated columns): an index built
/// over `random()` would never match a recomputed probe. Only the unambiguous
/// per-call-varying builtins are flagged here.
fn expr_is_nondeterministic(e: &Expr) -> bool {
    let mut found = false;
    window::visit(e, &mut |n| {
        if let Expr::Function { name, .. } = n {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "random" | "randomblob" | "last_insert_rowid" | "changes" | "total_changes"
            ) {
                found = true;
            }
        }
    });
    found
}

/// The rigid column type of a `STRICT` table column.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StrictType {
    Int,
    Real,
    Text,
    Blob,
    Any,
}

/// The `STRICT` rigid type for a declared type name, or `None` if the name is
/// not one of the six allowed (`INT`/`INTEGER`/`REAL`/`TEXT`/`BLOB`/`ANY`) — in
/// which case a `STRICT` table rejects the `CREATE`.
fn strict_column_type(type_name: Option<&str>) -> Option<StrictType> {
    let t = type_name?.trim();
    if t.eq_ignore_ascii_case("INT") || t.eq_ignore_ascii_case("INTEGER") {
        Some(StrictType::Int)
    } else if t.eq_ignore_ascii_case("REAL") {
        Some(StrictType::Real)
    } else if t.eq_ignore_ascii_case("TEXT") {
        Some(StrictType::Text)
    } else if t.eq_ignore_ascii_case("BLOB") {
        Some(StrictType::Blob)
    } else if t.eq_ignore_ascii_case("ANY") {
        Some(StrictType::Any)
    } else {
        None
    }
}

impl TableMeta {
    /// Whether column `i` is a VIRTUAL generated column (computed, never stored).
    fn is_virtual(&self, i: usize) -> bool {
        matches!(self.generated[i], Some((_, false)))
    }

    /// Whether column `i` is generated (STORED or VIRTUAL).
    fn is_generated(&self, i: usize) -> bool {
        self.generated[i].is_some()
    }
}

/// An index's b-tree root and the table column positions it covers.
/// A planner decision to satisfy `ORDER BY` by scanning a secondary index in key
/// order (B0), shared by `scan_source`, `run_core`, and `eqp_access`.
/// Apply `f` to each expression the VDBE actually compiles for a single-block
/// query: projections, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`/`OFFSET`
/// and join `ON`s. (Not CTEs/compound/subqueries — the VDBE bails on those.)
fn vdbe_block_exprs<'a>(sel: &'a Select, f: &mut impl FnMut(&'a Expr)) {
    for c in &sel.columns {
        if let ResultColumn::Expr { expr, .. } = c {
            f(expr);
        }
    }
    sel.where_clause.iter().for_each(&mut *f);
    sel.group_by.iter().for_each(&mut *f);
    sel.having.iter().for_each(&mut *f);
    for t in &sel.order_by {
        f(&t.expr);
    }
    sel.limit.iter().for_each(&mut *f);
    sel.offset.iter().for_each(&mut *f);
    if let Some(from) = &sel.from {
        for j in &from.joins {
            if let Some(on) = &j.on {
                f(on);
            }
        }
    }
}

/// Mutable counterpart of [`vdbe_block_exprs`].
fn vdbe_block_exprs_mut(sel: &mut Select, f: &mut impl FnMut(&mut Expr)) {
    for c in &mut sel.columns {
        if let ResultColumn::Expr { expr, .. } = c {
            f(expr);
        }
    }
    sel.where_clause.iter_mut().for_each(&mut *f);
    sel.group_by.iter_mut().for_each(&mut *f);
    sel.having.iter_mut().for_each(&mut *f);
    for t in &mut sel.order_by {
        f(&mut t.expr);
    }
    sel.limit.iter_mut().for_each(&mut *f);
    sel.offset.iter_mut().for_each(&mut *f);
    if let Some(from) = &mut sel.from {
        for j in &mut from.joins {
            if let Some(on) = &mut j.on {
                f(on);
            }
        }
    }
}

/// Substitute bound parameters into the expressions the VDBE compiles so a
/// PARAMETERIZED query can run on the (otherwise param-less) VDBE engine.
/// Returns the rewritten `Select`, or `None` to leave the query to the
/// tree-walker when an ANONYMOUS `?` is present — its index is assigned at eval
/// time (`EvalCtx::anon_counter`, affected by AND/OR short-circuit), so a static
/// substitution could diverge — or when those expressions hold no explicit
/// (`?N`/`:name`) parameter to substitute.
fn substitute_params(sel: &Select, params: &Params) -> Option<Select> {
    use crate::sql::token::Param;
    let mut anon = false;
    let mut explicit: Vec<Param> = Vec::new();
    vdbe_block_exprs(sel, &mut |e| {
        window::visit(e, &mut |x| {
            if let Expr::Parameter(p) = x {
                if matches!(p, Param::Anonymous) {
                    anon = true;
                } else if !explicit.contains(p) {
                    explicit.push(p.clone());
                }
            }
        });
    });
    if anon || explicit.is_empty() {
        return None;
    }
    let mut out = sel.clone();
    for p in &explicit {
        let v = match p {
            Param::Numbered(n) => params
                .positional
                .get((*n as usize).checked_sub(1)?)?
                .clone(),
            Param::Named(name) => params
                .named
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())?,
            Param::Anonymous => return None,
        };
        let target = Expr::Parameter(p.clone());
        let repl = Expr::Literal(value_to_literal(v));
        vdbe_block_exprs_mut(&mut out, &mut |e| window::replace_expr(e, &target, &repl));
    }
    Some(out)
}

struct OrderIndexScan {
    /// The index name (for `EXPLAIN QUERY PLAN`).
    name: String,
    /// Root page of the index b-tree.
    root: u32,
    /// Collations of the index's columns (for the b-tree walk).
    colls: Vec<crate::value::Collation>,
    /// Table-column index of each index column (record layout `cols…, rowid`).
    cols: Vec<usize>,
    /// `ORDER BY … DESC` (the ascending scan is reversed).
    descending: bool,
    /// The index holds every column the query references (B2): rows can be built
    /// from index records without touching the table b-tree.
    covering: bool,
    /// Number of trailing `ORDER BY` terms the index walk does NOT order (because
    /// they change direction): the walk yields the uniform leading prefix, then
    /// the caller still sorts. 0 means the walk fully satisfies the ORDER BY (no
    /// sort). Only set (>0) for the NON-covering mixed-direction case — the
    /// covered mixed case is handled by `covering_scan` + `scan_order_prefix`.
    sorted_suffix: usize,
}

struct IndexMeta {
    /// The index name (as in `sqlite_schema`), used by `ANALYZE`.
    name: String,
    root: u32,
    cols: Vec<usize>,
    /// Collating sequence for each indexed column (aligned with `cols`).
    collations: Vec<crate::value::Collation>,
    /// `CREATE INDEX … WHERE <predicate>` — a partial index only stores rows for
    /// which the predicate is true. `None` for a full index.
    partial: Option<Expr>,
    /// For an expression index (`CREATE INDEX … (lower(x))`), the per-term key
    /// expressions evaluated against each row to form the key. `None` for an
    /// ordinary column index (which uses `cols`).
    key_exprs: Option<Vec<Expr>>,
    /// `true` for a `UNIQUE` index (or an automatic UNIQUE/PK index). Drives
    /// uniqueness enforcement for standalone/partial/expression indexes, which
    /// the inline-constraint `TableMeta::unique` sets do not cover.
    unique: bool,
}

impl Connection {
    /// Run `body` with `outer`'s row pushed as a correlation frame, then pop it
    /// (even on error). The subquery runs with the outer query's parameters.
    fn with_outer_frame<T>(
        &self,
        outer: &EvalCtx,
        body: impl FnOnce(&Params) -> Result<T>,
    ) -> Result<T> {
        self.outer_scope.borrow_mut().push(OuterFrame {
            columns: outer.columns.to_vec(),
            row: outer.row.to_vec(),
            rowid: outer.rowid,
        });
        let params_ptr = outer.params;
        let out = body(params_ptr);
        self.outer_scope.borrow_mut().pop();
        out
    }
}

impl eval::Subqueries for Connection {
    fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid.get()
    }
    fn changes(&self) -> i64 {
        self.changes.get()
    }
    fn total_changes(&self) -> i64 {
        self.total_changes.get()
    }
    fn next_random(&self) -> i64 {
        // SplitMix64: advance a 64-bit counter by the golden-ratio increment,
        // then avalanche. Good distribution, tiny state, no_std-friendly, and
        // works from any seed (including 0).
        let s = self.rng_state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.rng_state.set(s);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        z as i64
    }
    fn call_udf(&self, name: &str, args: &[Value]) -> Option<Result<Value>> {
        self.functions.get(name).map(|f| f(args))
    }
    #[cfg(feature = "fts5")]
    fn fts5_bm25(&self, rowid: i64, weights: &[f64]) -> Option<f64> {
        let cell = self.fts5_rank.borrow();
        let (corpus, index) = cell.as_ref()?.bm25.as_ref()?;
        Some(corpus.score(*index.get(&rowid)?, weights))
    }
    #[cfg(feature = "fts5")]
    fn fts5_highlight(&self, col: usize, text: &str, open: &str, close: &str) -> Option<String> {
        let cell = self.fts5_rank.borrow();
        let ctx = cell.as_ref()?;
        // An `UNINDEXED` column carries no matches, so it is returned verbatim.
        if ctx.col_names.get(col).is_some_and(|n| !ctx.col_indexed(n)) {
            return Some(String::from(text));
        }
        Some(crate::vtab::fts5_highlight(
            &ctx.query,
            &ctx.col_names,
            ctx.scope.as_deref(),
            col,
            text,
            ctx.tok,
            open,
            close,
        ))
    }
    #[cfg(feature = "fts5")]
    fn fts5_indexed_columns(&self, table: &str) -> Option<Vec<String>> {
        let (module, args, _) = self.vtab_meta(table).ok()?;
        if !module.eq_ignore_ascii_case("fts5") {
            return None;
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Some(crate::vtab::fts5_indexed_columns(&refs))
    }
    #[cfg(feature = "fts5")]
    fn fts5_tok(&self, table: &str) -> crate::vtab::Fts5Tok {
        let Ok((module, args, _)) = self.vtab_meta(table) else {
            return crate::vtab::Fts5Tok::default();
        };
        if !module.eq_ignore_ascii_case("fts5") {
            return crate::vtab::Fts5Tok::default();
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::vtab::fts5_tok_config(&refs)
    }
    #[cfg(feature = "fts5")]
    fn fts5_snippet(
        &self,
        col: i64,
        cols: &[String],
        open: &str,
        close: &str,
        ellipsis: &str,
        ntokens: usize,
    ) -> Option<String> {
        let cell = self.fts5_rank.borrow();
        let ctx = cell.as_ref()?;
        Some(crate::vtab::fts5_snippet(
            &ctx.query,
            &ctx.col_names,
            ctx.scope.as_deref(),
            col,
            cols,
            ctx.indexed.as_deref(),
            ctx.tok,
            open,
            close,
            ellipsis,
            ntokens,
        ))
    }
    fn scalar(&self, select: &Select, outer: &EvalCtx) -> Result<Value> {
        self.with_outer_frame(outer, |params| {
            let r = self.run_select(select, params)?;
            Ok(r.rows
                .first()
                .and_then(|row| row.first())
                .cloned()
                .unwrap_or(Value::Null))
        })
    }

    fn column(&self, select: &Select, outer: &EvalCtx) -> Result<Vec<Value>> {
        self.with_outer_frame(outer, |params| {
            let r = self.run_select(select, params)?;
            Ok(r.rows
                .into_iter()
                .map(|mut row| {
                    if row.is_empty() {
                        Value::Null
                    } else {
                        row.swap_remove(0)
                    }
                })
                .collect())
        })
    }

    fn rows(&self, select: &Select, outer: &EvalCtx) -> Result<Vec<Vec<Value>>> {
        self.with_outer_frame(outer, |params| Ok(self.run_select(select, params)?.rows))
    }

    fn exists(&self, select: &Select, outer: &EvalCtx) -> Result<bool> {
        self.with_outer_frame(outer, |params| {
            Ok(!self.run_select(select, params)?.rows.is_empty())
        })
    }

    fn resolve_outer(&self, table: Option<&str>, name: &str) -> Option<Value> {
        let scope = self.outer_scope.borrow();
        for frame in scope.iter().rev() {
            // A rowid alias, optionally qualified by the frame's label (e.g.
            // `NEW.rowid`/`OLD.rowid` in a trigger, or `t.rowid` in a correlated
            // subquery). A real column of that name in the frame wins.
            if eval::is_rowid_alias(name) {
                let qualifies = match table {
                    None => true,
                    Some(t) => frame
                        .columns
                        .iter()
                        .any(|c| c.table.eq_ignore_ascii_case(t)),
                };
                let has_real = frame.columns.iter().any(|c| {
                    c.name.eq_ignore_ascii_case(name)
                        && table.is_none_or(|t| c.table.eq_ignore_ascii_case(t))
                });
                if qualifies && !has_real {
                    if let Some(r) = frame.rowid {
                        return Some(Value::Integer(r));
                    }
                }
            }
            for (i, col) in frame.columns.iter().enumerate() {
                let name_ok = col.name.eq_ignore_ascii_case(name);
                let table_ok = table.is_none_or(|t| col.table.eq_ignore_ascii_case(t));
                if name_ok && table_ok {
                    return Some(frame.row[i].clone());
                }
            }
        }
        None
    }
}

/// Whether a value is the given text (used to match `sqlite_schema` columns).
fn is_text(v: &Value, s: &str) -> bool {
    matches!(v, Value::Text(t) if t == s)
}

/// Collect `column = constant` equalities from the top-level `AND` conjuncts of a
/// `WHERE` clause, as `(column index, constant value)` pairs. Used to drive
/// index selection; non-equality and non-constant terms are ignored (the full
/// `WHERE` is still applied afterward).
/// Does `select` (across all its compound arms) read from a source named `name`?
fn references_name(select: &Select, name: &str) -> bool {
    if references_name_select(select, name) {
        return true;
    }
    select
        .compound
        .iter()
        .any(|(_, s)| references_name_select(s, name))
}

/// Does this single `SELECT` arm read from a source named `name` (first table or
/// any joined table)?
fn references_name_select(select: &Select, name: &str) -> bool {
    let Some(from) = &select.from else {
        return false;
    };
    if from.first.name.eq_ignore_ascii_case(name) {
        return true;
    }
    from.joins
        .iter()
        .any(|j| j.table.name.eq_ignore_ascii_case(name))
}

/// Is `e` a bare column reference (ignoring transparent `(…)`/`COLLATE` wrappers)?
/// A scalar subquery projecting one is only foldable with care — it would carry
/// that column's affinity/collation, which a plain literal does not — so the
/// scalar fold excludes this case.
fn is_bare_column_expr(e: &Expr) -> bool {
    match e {
        Expr::Column { .. } => true,
        Expr::Paren(inner) | Expr::Collate { expr: inner, .. } => is_bare_column_expr(inner),
        _ => false,
    }
}

/// Does `e` reference only columns of `quals`/`cols` (its own query's sources),
/// with no bound parameter and no nested subquery? Used to prove a subquery is
/// non-correlated before folding it to a constant. Conservative: any nested
/// subquery, parameter, or out-of-scope column makes it return `false`.
fn expr_is_internal(e: &Expr, quals: &[String], cols: &[String]) -> bool {
    let rec = |x: &Expr| expr_is_internal(x, quals, cols);
    match e {
        Expr::Literal(_) => true,
        // A parameter would need the statement's bindings to evaluate; the fold
        // runs with empty params, so bail and let the normal path handle it.
        Expr::Parameter(_) => false,
        // A nested subquery may itself reference our outer scope; without deeper
        // scope tracking, refuse to prove non-correlation.
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSelect { .. } => false,
        Expr::Column { table, column } => match table {
            Some(q) => quals.iter().any(|x| x.eq_ignore_ascii_case(q)),
            None => {
                cols.iter().any(|c| c.eq_ignore_ascii_case(column))
                    || column.eq_ignore_ascii_case("rowid")
                    || column.eq_ignore_ascii_case("_rowid_")
                    || column.eq_ignore_ascii_case("oid")
            }
        },
        Expr::Unary { expr, .. } => rec(expr),
        Expr::Binary { left, right, .. } => rec(left) && rec(right),
        Expr::IsNull { expr, .. } => rec(expr),
        Expr::InList { expr, list, .. } => rec(expr) && list.iter().all(rec),
        Expr::Between {
            expr, low, high, ..
        } => rec(expr) && rec(low) && rec(high),
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            operand.as_deref().map(rec).unwrap_or(true)
                && when_then.iter().all(|(w, t)| rec(w) && rec(t))
                && else_result.as_deref().map(rec).unwrap_or(true)
        }
        Expr::Cast { expr, .. } => rec(expr),
        Expr::Paren(inner) => rec(inner),
        Expr::Collate { expr, .. } => rec(expr),
        Expr::RowValue(items) => items.iter().all(rec),
        // A window function would not compile on the VDBE anyway; a non-windowed
        // call is internal when its arguments and `FILTER` are.
        Expr::Function {
            args,
            filter,
            order_by,
            over,
            ..
        } => {
            over.is_none()
                && args.iter().all(rec)
                && filter.as_deref().map(rec).unwrap_or(true)
                && order_by.iter().all(|t| rec(&t.expr))
        }
    }
}

/// Compare two ordering-key vectors with per-position `descending` flags
/// (missing flags default to ascending).
fn cmp_keys(a: &[Value], b: &[Value], desc: &[bool]) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let o = cmp_order(
            x,
            y,
            desc.get(i).copied().unwrap_or(false),
            None,
            crate::value::Collation::Binary,
        );
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Compare two `ORDER BY` key values honoring `DESC` and NULL placement. NULL
/// ordering follows the explicit `NULLS FIRST`/`LAST` when given, else SQLite's
/// default (NULLs first under `ASC`, last under `DESC`); the non-NULL comparison
/// uses the column collation and is reversed by `DESC`.
pub(crate) fn cmp_order(
    a: &Value,
    b: &Value,
    descending: bool,
    nulls_first: Option<bool>,
    coll: crate::value::Collation,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let a_null = matches!(a, Value::Null);
    let b_null = matches!(b, Value::Null);
    let nulls_first = nulls_first.unwrap_or(!descending);
    match (a_null, b_null) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let ord = crate::value::cmp_values_coll(a, b, coll);
            if descending {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// The `[start, end)` frame indices (into the ordered partition) for position
/// `p`, given peer-group ids `gid` and the window `spec`.
///
/// With no explicit frame: the whole partition when there is no `ORDER BY`, else
/// `UNBOUNDED PRECEDING` through the current row's last peer — SQLite's default.
/// `ROWS` frames use physical offsets; `RANGE`/`GROUPS` use peer-group offsets.
/// Resolve a window function's `OVER name` (or `OVER (name …)`) reference against
/// the query's `WINDOW name AS (…)` definitions, returning a clone of `wexpr`
/// whose spec is the effective one. A spec with no `base_name` is returned as-is.
fn resolve_window_ref(wexpr: &Expr, defs: &[(String, WindowSpec)]) -> Result<Expr> {
    let Expr::Function {
        name,
        distinct,
        args,
        star,
        filter,
        order_by,
        over: Some(spec),
    } = wexpr
    else {
        return Ok(wexpr.clone());
    };
    let Some(base) = &spec.base_name else {
        return Ok(wexpr.clone());
    };
    let def = defs
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(base))
        .map(|(_, s)| s)
        .ok_or_else(|| Error::Error(alloc::format!("no such window: {base}")))?;
    // The named window provides PARTITION BY; the referencing use may add ORDER BY
    // and a frame when the base omits them.
    let effective = WindowSpec {
        partition_by: def.partition_by.clone(),
        order_by: if spec.order_by.is_empty() {
            def.order_by.clone()
        } else {
            spec.order_by.clone()
        },
        frame: spec.frame.clone().or_else(|| def.frame.clone()),
        base_name: None,
    };
    Ok(Expr::Function {
        name: name.clone(),
        distinct: *distinct,
        args: args.clone(),
        star: *star,
        filter: filter.clone(),
        order_by: order_by.clone(),
        over: Some(effective),
    })
}

/// Emit one `json_each`/`json_tree` row for `node` and return its assigned id.
/// `key` is the member name / array index (None for a top-level scalar or the
/// `json_tree` root); `fullkey`/`path` are the element's path and its parent's.
fn json_emit_node(
    node: &crate::exec::json::Json,
    key: Option<Value>,
    fullkey: &str,
    path: &str,
    parent: Option<i64>,
    next_id: &mut i64,
    rows: &mut Vec<Vec<Value>>,
) -> i64 {
    use crate::exec::json::Json;
    let id = *next_id;
    *next_id += 1;
    let is_container = matches!(node, Json::Object(_) | Json::Array(_));
    let value = node.to_sql();
    let atom = if is_container {
        Value::Null
    } else {
        value.clone()
    };
    rows.push(alloc::vec![
        key.unwrap_or(Value::Null),
        value,
        Value::Text(String::from(node.type_name())),
        atom,
        Value::Integer(id),
        parent.map(Value::Integer).unwrap_or(Value::Null),
        Value::Text(String::from(fullkey)),
        Value::Text(String::from(path)),
    ]);
    id
}

/// `json_each`: emit a row for each *direct* child of `root` (or a single row for
/// a scalar root). `root_path` is the document path `root` sits at (`"$"`, or the
/// `json_each(x, path)` argument), used as the prefix of each child's `fullkey`.
fn json_each_children(
    root: &crate::exec::json::Json,
    root_path: &str,
    next_id: &mut i64,
    rows: &mut Vec<Vec<Value>>,
) {
    use crate::exec::json::Json;
    match root {
        Json::Object(members) => {
            for (k, v) in members {
                let fullkey = alloc::format!("{root_path}.{k}");
                json_emit_node(
                    v,
                    Some(Value::Text(k.clone())),
                    &fullkey,
                    root_path,
                    None,
                    next_id,
                    rows,
                );
            }
        }
        Json::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                let fullkey = alloc::format!("{root_path}[{i}]");
                json_emit_node(
                    v,
                    Some(Value::Integer(i as i64)),
                    &fullkey,
                    root_path,
                    None,
                    next_id,
                    rows,
                );
            }
        }
        scalar => {
            json_emit_node(scalar, None, root_path, root_path, None, next_id, rows);
        }
    }
}

/// Split a JSON path (`$.a.b`, `$.a[2]`, `$[0]`) into its parent path and the
/// final component rendered as a `json_tree` root key (a text member name or an
/// integer array index). The bare root `$` yields `("$", None)`.
fn split_json_path(path: &str) -> (alloc::string::String, Option<Value>) {
    if path.ends_with(']') {
        if let Some(open) = path.rfind('[') {
            if let Ok(i) = path[open + 1..path.len() - 1].parse::<i64>() {
                return (String::from(&path[..open]), Some(Value::Integer(i)));
            }
        }
    }
    if let Some(dot) = path.rfind('.') {
        let name = path[dot + 1..].trim_matches('"');
        return (
            String::from(&path[..dot]),
            Some(Value::Text(String::from(name))),
        );
    }
    (String::from(path), None)
}

/// `json_tree`: emit `node` then recurse depth-first into its children.
fn json_tree_walk(
    node: &crate::exec::json::Json,
    key: Option<Value>,
    fullkey: &str,
    path: &str,
    parent: Option<i64>,
    next_id: &mut i64,
    rows: &mut Vec<Vec<Value>>,
) {
    use crate::exec::json::Json;
    let id = json_emit_node(node, key, fullkey, path, parent, next_id, rows);
    match node {
        Json::Object(members) => {
            for (k, v) in members {
                let child = alloc::format!("{fullkey}.{k}");
                json_tree_walk(
                    v,
                    Some(Value::Text(k.clone())),
                    &child,
                    fullkey,
                    Some(id),
                    next_id,
                    rows,
                );
            }
        }
        Json::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                let child = alloc::format!("{fullkey}[{i}]");
                json_tree_walk(
                    v,
                    Some(Value::Integer(i as i64)),
                    &child,
                    fullkey,
                    Some(id),
                    next_id,
                    rows,
                );
            }
        }
        _ => {}
    }
}

fn frame_bounds(
    p: usize,
    m: usize,
    gid: &[usize],
    spec: &WindowSpec,
    ovals: &[Value],
    desc: bool,
) -> (usize, usize) {
    let Some(frame) = &spec.frame else {
        if spec.order_by.is_empty() {
            return (0, m);
        }
        // Default: UNBOUNDED PRECEDING .. CURRENT ROW (peers included).
        let mut e = p + 1;
        while e < m && gid[e] == gid[p] {
            e += 1;
        }
        return (0, e);
    };
    let (start, end) = match frame.mode {
        FrameMode::Rows => (
            row_bound(&frame.start, p, m, true),
            row_bound(&frame.end, p, m, false),
        ),
        // RANGE with a numeric offset bounds the frame by the ORDER BY *value*
        // (within `value ± n`); CURRENT ROW / UNBOUNDED still use peer groups.
        FrameMode::Range
            if !ovals.is_empty()
                && (matches!(
                    frame.start,
                    FrameBound::Preceding(_) | FrameBound::Following(_)
                ) || matches!(
                    frame.end,
                    FrameBound::Preceding(_) | FrameBound::Following(_)
                )) =>
        {
            (
                range_value_bound(&frame.start, p, m, gid, ovals, desc, true),
                range_value_bound(&frame.end, p, m, gid, ovals, desc, false),
            )
        }
        FrameMode::Range | FrameMode::Groups => (
            group_bound(&frame.start, p, m, gid, true),
            group_bound(&frame.end, p, m, gid, false),
        ),
    };
    let start = start.min(m);
    (start, end.min(m).max(start))
}

/// A `RANGE` frame bound measured by the ORDER BY value: the frame includes rows
/// whose value is within `[value - start_n, value + end_n]` (signs flipped for a
/// `DESC` ordering). `CURRENT ROW` and `UNBOUNDED` fall back to peer-group edges.
/// Falls back to peer-group edges if the current value is not numeric.
fn range_value_bound(
    b: &FrameBound,
    p: usize,
    m: usize,
    gid: &[usize],
    ovals: &[Value],
    desc: bool,
    is_start: bool,
) -> usize {
    // Non-numeric/NULL current value: fall back to peer-group edges.
    if matches!(
        b,
        FrameBound::CurrentRow | FrameBound::Preceding(_) | FrameBound::Following(_)
    ) && matches!(ovals[p], Value::Null)
    {
        return group_bound(b, p, m, gid, is_start);
    }
    let val = eval::to_f64(&ovals[p]);
    // The frame edge as an ORDER BY value. Under ASC, PRECEDING subtracts and
    // FOLLOWING adds; under DESC the sequence decreases so the signs flip.
    let threshold = match b {
        FrameBound::UnboundedPreceding => return 0,
        FrameBound::UnboundedFollowing => return m,
        FrameBound::CurrentRow => val,
        FrameBound::Preceding(n) => {
            if desc {
                val + *n as f64
            } else {
                val - *n as f64
            }
        }
        FrameBound::Following(n) => {
            if desc {
                val - *n as f64
            } else {
                val + *n as f64
            }
        }
    };
    // Values run ascending (ASC) or descending (DESC) across positions. The frame
    // is the contiguous span of rows on the inclusive side of `threshold`.
    let inside = |vk: f64, edge: f64| if desc { vk >= edge } else { vk <= edge };
    if is_start {
        // First row at/after the start edge.
        (0..m)
            .find(|&k| {
                !matches!(ovals[k], Value::Null) && {
                    let vk = eval::to_f64(&ovals[k]);
                    if desc {
                        vk <= threshold
                    } else {
                        vk >= threshold
                    }
                }
            })
            .unwrap_or(m)
    } else {
        // One past the last row at/before the end edge.
        let mut e = m;
        for (k, ov) in ovals.iter().enumerate().take(m) {
            if matches!(ov, Value::Null) {
                continue;
            }
            if !inside(eval::to_f64(ov), threshold) {
                e = k;
                break;
            }
        }
        e
    }
}

/// A `ROWS` frame bound as an index; `is_start` selects inclusive-start vs
/// exclusive-end semantics.
fn row_bound(b: &FrameBound, p: usize, m: usize, is_start: bool) -> usize {
    match (b, is_start) {
        (FrameBound::UnboundedPreceding, _) => 0,
        (FrameBound::UnboundedFollowing, _) => m,
        (FrameBound::CurrentRow, true) => p,
        (FrameBound::CurrentRow, false) => p + 1,
        (FrameBound::Preceding(n), true) => p.saturating_sub(*n as usize),
        (FrameBound::Preceding(n), false) => (p + 1).saturating_sub(*n as usize),
        (FrameBound::Following(n), true) => (p + *n as usize).min(m),
        (FrameBound::Following(n), false) => (p + 1 + *n as usize).min(m),
    }
}

/// A `RANGE`/`GROUPS` frame bound, measured in peer groups.
fn group_bound(b: &FrameBound, p: usize, m: usize, gid: &[usize], is_start: bool) -> usize {
    let maxg = if m == 0 { 0 } else { gid[m - 1] as i64 };
    let target = |g: i64| -> i64 { gid[p] as i64 + g };
    // First ordered index of peer-group `g` (clamped: below 0 -> 0, above max -> m).
    let first_of = |g: i64| -> usize {
        if g < 0 {
            0
        } else if g > maxg {
            m
        } else {
            (0..m).find(|&i| gid[i] as i64 == g).unwrap_or(m)
        }
    };
    // One past the last ordered index of peer-group `g` (same clamping).
    let after_last_of = |g: i64| -> usize {
        if g < 0 {
            0
        } else if g > maxg {
            m
        } else {
            (0..m)
                .rev()
                .find(|&i| gid[i] as i64 == g)
                .map_or(0, |i| i + 1)
        }
    };
    match (b, is_start) {
        (FrameBound::UnboundedPreceding, _) => 0,
        (FrameBound::UnboundedFollowing, _) => m,
        (FrameBound::CurrentRow, true) => first_of(target(0)),
        (FrameBound::CurrentRow, false) => after_last_of(target(0)),
        (FrameBound::Preceding(n), true) => first_of(target(-(*n))),
        (FrameBound::Preceding(n), false) => after_last_of(target(-(*n))),
        (FrameBound::Following(n), true) => first_of(target(*n)),
        (FrameBound::Following(n), false) => after_last_of(target(*n)),
    }
}

/// The 1-based `ntile` bucket for ordered position `p` of `m` rows split into
/// `buckets` groups (earlier groups absorb the remainder).
fn ntile_bucket(p: usize, m: usize, buckets: i64) -> i64 {
    let buckets = (buckets.max(1) as usize).min(m.max(1));
    let size = m / buckets;
    let rem = m % buckets;
    let big = rem * (size + 1);
    if p < big {
        (p / (size + 1)) as i64 + 1
    } else {
        (rem + (p - big) / size.max(1)) as i64 + 1
    }
}

/// Evaluate an aggregate window function over a frame of per-row argument
/// values, matching `compute_aggregate`'s numeric semantics.
fn window_aggregate(lname: &str, star: bool, frame: &[&Vec<Value>]) -> Result<Value> {
    let mut vals: Vec<Value> = Vec::new();
    for row in frame {
        if star {
            continue;
        }
        if let Some(v) = row.first() {
            if !matches!(v, Value::Null) {
                vals.push(v.clone());
            }
        }
    }
    Ok(match lname {
        "count" => {
            if star {
                Value::Integer(frame.len() as i64)
            } else {
                Value::Integer(vals.len() as i64)
            }
        }
        "sum" => {
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
        "total" => Value::Real(vals.iter().map(eval::to_f64).sum()),
        "avg" => {
            if vals.is_empty() {
                Value::Null
            } else {
                let sum: f64 = vals.iter().map(eval::to_f64).sum();
                Value::Real(sum / vals.len() as f64)
            }
        }
        "min" => vals
            .into_iter()
            .reduce(|a, b| {
                if eval::compare(&b, &a) == core::cmp::Ordering::Less {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(Value::Null),
        "max" => vals
            .into_iter()
            .reduce(|a, b| {
                if eval::compare(&b, &a) == core::cmp::Ordering::Greater {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(Value::Null),
        "group_concat" | "string_agg" => {
            if vals.is_empty() {
                Value::Null
            } else {
                // The optional 2nd argument is the separator (default ","), the
                // same for every row of the frame.
                let sep = frame
                    .first()
                    .and_then(|r| r.get(1))
                    .map(eval::to_text)
                    .unwrap_or_else(|| String::from(","));
                let parts: Vec<String> = vals.iter().map(eval::to_text).collect();
                Value::Text(parts.join(&sep))
            }
        }
        _ => return Err(Error::Unsupported("window function")),
    })
}

/// Dedupe rows in place, preserving first-occurrence order.
fn dedup_rows(rows: &mut Vec<Vec<Value>>) {
    let mut seen: Vec<Vec<Value>> = Vec::new();
    rows.retain(|row| {
        if seen.iter().any(|s| rows_equal(s, row)) {
            false
        } else {
            seen.push(row.clone());
            true
        }
    });
}

/// A `PRAGMA name = value` argument as text (a bare keyword like `WAL` or a
/// quoted string).
fn pragma_text(e: &Expr) -> String {
    match e {
        Expr::Column { column, .. } => column.clone(),
        Expr::Literal(Literal::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Interpret a `PRAGMA name = value` argument as a boolean, accepting
/// `1`/`0`, `on`/`off`, `yes`/`no`, `true`/`false`.
fn pragma_truth(e: &Expr, params: &Params) -> bool {
    match e {
        Expr::Column { column, .. } => {
            matches!(column.to_ascii_lowercase().as_str(), "on" | "yes" | "true")
        }
        Expr::Literal(Literal::Str(s)) => {
            matches!(s.to_ascii_lowercase().as_str(), "on" | "yes" | "true" | "1")
        }
        _ => eval::eval(e, &EvalCtx::rowless(params))
            .map(|v| eval::to_i64(&v) != 0)
            .unwrap_or(false),
    }
}

/// The EXPLAIN QUERY PLAN display label for a table reference: SQLite shows
/// `name AS alias` when an alias is present, else just the name.
fn eqp_label(t: &TableRef) -> String {
    match &t.alias {
        Some(a) => alloc::format!("{} AS {}", t.name, a),
        None => t.name.clone(),
    }
}

/// Whether every column the `WHERE` expression references is covered by the
/// index (`idx_cols`) or is the rowid — the seek-covering precondition. Walks the
/// expression tree and returns `false` the moment it finds an uncovered column,
/// an unknown column name that is not a rowid alias, or a construct whose columns
/// can't be enumerated locally (a scalar subquery / `EXISTS` / `IN (SELECT …)`),
/// so the caller conservatively falls back to the table-fetch path.
/// Is a partial index's predicate guaranteed by a top-level conjunct of the
/// `WHERE`? Always true for a non-partial index.
fn partial_pred_guaranteed(idx: &IndexMeta, where_expr: &Expr) -> bool {
    match &idx.partial {
        None => true,
        Some(pred) => {
            let mut conjuncts = Vec::new();
            and_conjuncts(where_expr, &mut conjuncts);
            conjuncts.iter().any(|c| expr_eq_modulo_parens(c, pred))
        }
    }
}

/// Find a conjunct `<key_expr> IN (const, …)` (walking top-level `AND`s) and
/// return the evaluated list values — the expression-index analogue of
/// [`find_in_constraint`].
fn find_expr_in_values(key_expr: &Expr, e: &Expr, params: &Params) -> Option<Vec<Value>> {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => find_expr_in_values(key_expr, left, params)
            .or_else(|| find_expr_in_values(key_expr, right, params)),
        Expr::Paren(inner) => find_expr_in_values(key_expr, inner, params),
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            if list.is_empty() || !expr_eq_modulo_parens(expr, key_expr) {
                return None;
            }
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                vals.push(const_value(item, params)?);
            }
            Some(vals)
        }
        _ => None,
    }
}

/// The executor's [`VTabStore`] implementation: a persistent virtual table's
/// backing `<vtab>_data` regular table, read/written through the normal table
/// machinery. Built (with the module taken out of the registry, so
/// `&mut Connection` doesn't alias the borrowed module) for one `update` call.
struct ExecVTabStore<'a> {
    conn: &'a mut Connection,
    backing: &'a str,
    /// The backing table leads with an `INTEGER PRIMARY KEY` `id` column (FTS5's
    /// `_content`), stored as a NULL placeholder serial (the rowid is the b-tree
    /// key). Module values are the columns after `id`, so prepend a NULL on write
    /// and drop the leading value on read.
    ipk_prefix: bool,
}

impl VTabStore for ExecVTabStore<'_> {
    fn rows(&self) -> Result<Vec<(i64, Vec<Value>)>> {
        let meta = self.conn.table_meta(self.backing, None)?;
        let mut rows = self.conn.scan_table(&meta)?;
        if self.ipk_prefix {
            for (_, values) in &mut rows {
                if !values.is_empty() {
                    values.remove(0);
                }
            }
        }
        Ok(rows)
    }
    fn put(&mut self, rowid: i64, values: &[Value]) -> Result<()> {
        let root = self.conn.table_meta(self.backing, None)?.root;
        let payload = if self.ipk_prefix {
            let mut row = alloc::vec![Value::Null];
            row.extend_from_slice(values);
            encode_record(&row)
        } else {
            encode_record(values)
        };
        let w = self.conn.backend.writer()?;
        // Replace semantics: drop any existing row, then insert.
        crate::btree::delete_table(w, root, rowid)?;
        crate::btree::insert_table(w, root, rowid, &payload)?;
        Ok(())
    }
    fn delete(&mut self, rowid: i64) -> Result<()> {
        let root = self.conn.table_meta(self.backing, None)?.root;
        let w = self.conn.backend.writer()?;
        crate::btree::delete_table(w, root, rowid)?;
        Ok(())
    }
}

/// Flip a comparison operator for a swapped operand order: `a < b` ⇔ `b > a`.
/// Non-ordering operators are returned unchanged.
fn mirror_comparison(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

fn where_cols_covered(e: &Expr, meta: &TableMeta, idx_cols: &[usize]) -> bool {
    let covered = |ci: usize| idx_cols.contains(&ci) || meta.ipk == Some(ci);
    match e {
        Expr::Literal(_) | Expr::Parameter(_) => true,
        Expr::Column { column, .. } => match meta
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(column))
        {
            Some(ci) => covered(ci),
            None => matches!(
                column.to_ascii_lowercase().as_str(),
                "rowid" | "_rowid_" | "oid"
            ),
        },
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Paren(expr) => where_cols_covered(expr, meta, idx_cols),
        Expr::Binary { left, right, .. } => {
            where_cols_covered(left, meta, idx_cols) && where_cols_covered(right, meta, idx_cols)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            where_cols_covered(expr, meta, idx_cols)
                && where_cols_covered(low, meta, idx_cols)
                && where_cols_covered(high, meta, idx_cols)
        }
        Expr::InList { expr, list, .. } => {
            where_cols_covered(expr, meta, idx_cols)
                && list.iter().all(|x| where_cols_covered(x, meta, idx_cols))
        }
        Expr::RowValue(items) => items.iter().all(|x| where_cols_covered(x, meta, idx_cols)),
        Expr::Function {
            args, filter, over, ..
        } => {
            over.is_none()
                && filter.is_none()
                && args.iter().all(|x| where_cols_covered(x, meta, idx_cols))
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            operand
                .as_deref()
                .map(|o| where_cols_covered(o, meta, idx_cols))
                .unwrap_or(true)
                && when_then.iter().all(|(w, t)| {
                    where_cols_covered(w, meta, idx_cols) && where_cols_covered(t, meta, idx_cols)
                })
                && else_result
                    .as_deref()
                    .map(|x| where_cols_covered(x, meta, idx_cols))
                    .unwrap_or(true)
        }
        // A subquery may read other tables/columns we can't enumerate here; bail.
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSelect { .. } => false,
    }
}

/// Gather the virtual-table constraints to offer `best_index` from a query's
/// `WHERE`, plus, in lockstep, each constraint's bound right-hand [`Value`].
///
/// Walks the top-level `AND` conjuncts looking for `col <op> const` comparisons
/// (and `BETWEEN`, expanded to a `>=`/`<=` pair) where `col` is one of this
/// table's `columns` and the other side is row-independent. The returned
/// `(constraints, values)` vectors are parallel: `values[i]` is the evaluated
/// bound of `constraints[i]`. Only the comparison *shape* goes to the module (as
/// SQLite does); the values are held back and handed to `filter` per the plan's
/// `argv_index`.
/// Whether a WHERE clause contains a `rowid = <const>` term (rowid/`_rowid_`/`oid`)
/// in its `AND` tree — used to report FTS5's `INDEX 0:=` rowid-lookup plan.
#[cfg(feature = "fts5")]
fn fts5_rowid_eq(expr: &Expr, params: &Params) -> bool {
    let is_rowid = |e: &Expr| {
        matches!(e, Expr::Column { column, .. }
            if matches!(column.to_ascii_lowercase().as_str(), "rowid" | "_rowid_" | "oid"))
    };
    match expr {
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            (is_rowid(left) && const_value(right, params).is_some())
                || (is_rowid(right) && const_value(left, params).is_some())
        }
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => fts5_rowid_eq(left, params) || fts5_rowid_eq(right, params),
        Expr::Paren(e) => fts5_rowid_eq(e, params),
        _ => false,
    }
}

fn collect_vtab_constraints(
    sel: &Select,
    columns: &[ColumnInfo],
    params: &Params,
) -> (Vec<IndexConstraint>, Vec<Value>) {
    let mut constraints = Vec::new();
    let mut values = Vec::new();
    let Some(where_expr) = &sel.where_clause else {
        return (constraints, values);
    };
    let mut conjuncts = Vec::new();
    and_conjuncts(where_expr, &mut conjuncts);
    let mut push = |col: usize, op: ConstraintOp, v: Value| {
        constraints.push(IndexConstraint {
            column: col,
            op,
            usable: true,
        });
        values.push(v);
    };
    for c in conjuncts {
        match c {
            Expr::Binary { op, left, right }
                if matches!(
                    op,
                    BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
                ) =>
            {
                if let (Some(ci), Some(v)) = (col_index(left, columns), const_value(right, params))
                {
                    if let Some(cop) = binop_to_constraint(*op) {
                        push(ci, cop, v);
                    }
                } else if let (Some(ci), Some(v)) =
                    (col_index(right, columns), const_value(left, params))
                {
                    if let Some(cop) = binop_to_constraint(flip_cmp(*op)) {
                        push(ci, cop, v);
                    }
                }
            }
            Expr::Between {
                expr,
                low,
                high,
                negated: false,
            } => {
                if let Some(ci) = col_index(expr, columns) {
                    if let Some(v) = const_value(low, params) {
                        push(ci, ConstraintOp::Ge, v);
                    }
                    if let Some(v) = const_value(high, params) {
                        push(ci, ConstraintOp::Le, v);
                    }
                }
            }
            _ => {}
        }
    }
    (constraints, values)
}

/// Map a comparison [`BinaryOp`] to a vtab [`ConstraintOp`], or `None` for a
/// non-comparison operator.
fn binop_to_constraint(op: BinaryOp) -> Option<ConstraintOp> {
    Some(match op {
        BinaryOp::Eq => ConstraintOp::Eq,
        BinaryOp::Lt => ConstraintOp::Lt,
        BinaryOp::LtEq => ConstraintOp::Le,
        BinaryOp::Gt => ConstraintOp::Gt,
        BinaryOp::GtEq => ConstraintOp::Ge,
        _ => return None,
    })
}

/// Order the bound constraint `values` by the plan's 1-based `argv_index`, the
/// argument vector handed to [`crate::vtab::VTabModule::filter`].
///
/// `argv_index[i]` is the position (1-based) the module wants `values[i]` passed
/// at, or `0` to drop it. A robust pass: collect `(pos, value)` for every nonzero
/// entry, sort by `pos`, and emit the values. Gaps or duplicate positions are
/// tolerated (the module decides what its own positions mean).
fn order_vtab_argv(plan: &IndexPlan, values: &[Value]) -> Vec<Value> {
    let mut slots: Vec<(u32, Value)> = plan
        .argv_index
        .iter()
        .zip(values.iter())
        .filter(|(pos, _)| **pos != 0)
        .map(|(pos, v)| (*pos, v.clone()))
        .collect();
    slots.sort_by_key(|(pos, _)| *pos);
    slots.into_iter().map(|(_, v)| v).collect()
}

fn collect_eq_constraints(
    e: &Expr,
    columns: &[ColumnInfo],
    params: &Params,
    out: &mut Vec<(usize, Value)>,
) {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect_eq_constraints(left, columns, params, out);
            collect_eq_constraints(right, columns, params, out);
        }
        Expr::Paren(inner) => collect_eq_constraints(inner, columns, params, out),
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            if let (Some(ci), Some(v)) = (col_index(left, columns), const_value(right, params)) {
                out.push((ci, v));
            } else if let (Some(ci), Some(v)) =
                (col_index(right, columns), const_value(left, params))
            {
                out.push((ci, v));
            }
        }
        _ => {}
    }
}

/// Strip redundant outer parentheses from an expression, so structural
/// comparison ignores grouping (`(active = 1)` ≡ `active = 1`).
fn unparen(e: &Expr) -> &Expr {
    let mut cur = e;
    while let Expr::Paren(inner) = cur {
        cur = inner;
    }
    cur
}

/// Two expressions are equal modulo redundant parentheses. Used to match a
/// partial-index predicate (or an expression-index key) against a query's
/// `WHERE` structurally — this is the conservative rule (no general implication),
/// so it only recurses through `Paren`; everything else uses derived `PartialEq`.
fn expr_eq_modulo_parens(a: &Expr, b: &Expr) -> bool {
    unparen(a) == unparen(b)
}

/// Collect the top-level `AND` conjuncts of `e` (descending through `Paren` and
/// `AND` nodes), pushing each non-`AND` leaf as a borrowed reference.
fn and_conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match unparen(e) {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            and_conjuncts(left, out);
            and_conjuncts(right, out);
        }
        other => out.push(other),
    }
}

/// A hash-join bucket key. Over-keying (one value yielding several keys) is safe:
/// the join's full `ON` predicate is re-evaluated on every candidate, so extra
/// keys only cost comparisons — they never drop a real match.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum JoinKey {
    /// Numeric value, keyed by canonical `f64` bits (so `5` and `5.0` collide).
    Num(u64),
    /// Text value (exact bytes).
    Text(String),
    /// Blob value.
    Blob(Vec<u8>),
}

/// Canonical bits for a number, normalizing `-0.0` to `0.0` so the two compare
/// equal (as they do in SQL).
fn num_bits(f: f64) -> u64 {
    (if f == 0.0 { 0.0 } else { f }).to_bits()
}

/// The set of hash-join keys a value participates in. A numeric value keys by its
/// number *and* its text form; text that parses as a number keys by both too — so
/// affinity-driven cross-type equality (`5 = '5'`) never misses (the `ON` re-eval
/// rejects the spurious ones). `NULL` keys nothing (it never equi-joins).
fn join_keys_of(v: &Value) -> Vec<JoinKey> {
    match v {
        Value::Null => Vec::new(),
        Value::Integer(i) => alloc::vec![
            JoinKey::Num(num_bits(*i as f64)),
            JoinKey::Text(i.to_string())
        ],
        Value::Real(r) => {
            alloc::vec![
                JoinKey::Num(num_bits(*r)),
                JoinKey::Text(eval::format_real(*r))
            ]
        }
        Value::Text(s) => {
            let mut keys = alloc::vec![JoinKey::Text(s.clone())];
            match eval::to_number(&Value::Text(s.clone())) {
                Value::Integer(i) => keys.push(JoinKey::Num(num_bits(i as f64))),
                Value::Real(r) => keys.push(JoinKey::Num(num_bits(r))),
                _ => {}
            }
            keys
        }
        Value::Blob(b) => alloc::vec![JoinKey::Blob(b.clone())],
    }
}

/// Promote a comma join's filtering equality from `WHERE` into its `ON`, so the
/// common `FROM a, b WHERE a.x = b.y` pattern can use the same hash/index seek
/// path (and EXPLAIN QUERY PLAN node) as `a JOIN b ON a.x = b.y`. The equality is
/// *copied*, not moved — it stays in `WHERE` — so the result is unchanged: the
/// `ON` is a subset of `WHERE`, applied redundantly. Only a qualified
/// `t.col = u.col` equality linking the joined table to an already-introduced one
/// is promoted. Returns the rewritten `Select`, or `None` if nothing applied.
fn promote_comma_join_ons(sel: &Select) -> Option<Select> {
    let from = sel.from.as_ref()?;
    let where_clause = sel.where_clause.as_ref()?;
    let promotable = |j: &Join| {
        j.on.is_none() && !j.natural && j.using.is_empty() && matches!(j.kind, JoinKind::Inner)
    };
    if !from.joins.iter().any(promotable) {
        return None;
    }
    let mut conjuncts: Vec<&Expr> = Vec::new();
    and_conjuncts(where_clause, &mut conjuncts);
    let label = |t: &TableRef| t.alias.clone().unwrap_or_else(|| t.name.clone());
    let mut available: Vec<String> = alloc::vec![label(&from.first)];
    let mut new_joins = from.joins.clone();
    let mut changed = false;
    for (i, join) in from.joins.iter().enumerate() {
        let jlabel = label(&join.table);
        if promotable(join) {
            if let Some(cond) = conjuncts
                .iter()
                .find_map(|c| eligible_join_equi(c, &jlabel, &available))
            {
                new_joins[i].on = Some(cond);
                changed = true;
            }
        }
        available.push(jlabel);
    }
    if !changed {
        return None;
    }
    let mut new_sel = sel.clone();
    new_sel.from = Some(FromClause {
        first: from.first.clone(),
        joins: new_joins,
    });
    Some(new_sel)
}

/// A qualified `A.x = B.y` equality where one qualifier is `jlabel` and the other
/// is in `available` — eligible to become a comma join's `ON`. Returns the cloned
/// equality (with any enclosing parens stripped).
fn eligible_join_equi(c: &Expr, jlabel: &str, available: &[String]) -> Option<Expr> {
    let mut c = c;
    while let Expr::Paren(inner) = c {
        c = inner;
    }
    let (l, r) = match c {
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => (left.as_ref(), right.as_ref()),
        _ => return None,
    };
    let lt = column_qualifier(l)?;
    let rt = column_qualifier(r)?;
    let here = |t: &str| t.eq_ignore_ascii_case(jlabel);
    let earlier = |t: &str| available.iter().any(|a| a.eq_ignore_ascii_case(t));
    if (here(lt) && earlier(rt)) || (here(rt) && earlier(lt)) {
        Some(c.clone())
    } else {
        None
    }
}

/// The table qualifier of a qualified column reference (`t.col` → `t`).
fn column_qualifier(e: &Expr) -> Option<&str> {
    match e {
        Expr::Column { table: Some(t), .. } => Some(t),
        Expr::Paren(inner) => column_qualifier(inner),
        _ => None,
    }
}

/// Extract a single equi-join `left.col = right.col` from the top-level `AND`
/// conjuncts of an `ON` predicate, returning `(left column index, right column
/// index within the joined table)`. Both columns must use `BINARY` collation
/// (otherwise text equality is collation-sensitive and a hash on exact bytes
/// could miss a match — fall back to the nested loop). `cols` is the combined
/// left+right column list; `left_width` is the number of left columns.
fn join_equi_cols(on: &Expr, cols: &[ColumnInfo], left_width: usize) -> Option<(usize, usize)> {
    match on {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => join_equi_cols(left, cols, left_width)
            .or_else(|| join_equi_cols(right, cols, left_width)),
        Expr::Paren(inner) => join_equi_cols(inner, cols, left_width),
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            let a = col_index(left, cols)?;
            let b = col_index(right, cols)?;
            let binary = |i: usize| cols[i].collation == crate::value::Collation::Binary;
            let (l, r) = if a < left_width && b >= left_width {
                (a, b)
            } else if b < left_width && a >= left_width {
                (b, a)
            } else {
                return None;
            };
            if binary(l) && binary(r) {
                Some((l, r - left_width))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Flatten a top-level `OR` chain into its disjuncts (unwrapping parentheses),
/// e.g. `a OR (b OR c)` → `[a, b, c]`. A non-`OR` expression yields itself.
fn flatten_or<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::Binary {
            op: BinaryOp::Or,
            left,
            right,
        } => {
            flatten_or(left, out);
            flatten_or(right, out);
        }
        Expr::Paren(inner) => flatten_or(inner, out),
        other => out.push(other),
    }
}

/// Find a top-level `column IN (const, const, …)` conjunct (not `NOT IN`, all
/// list entries constant), returning the column index and the constant values.
/// Used to drive per-value index seeks; only the first such term is returned.
fn find_in_constraint(
    e: &Expr,
    columns: &[ColumnInfo],
    params: &Params,
) -> Option<(usize, Vec<Value>)> {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => find_in_constraint(left, columns, params)
            .or_else(|| find_in_constraint(right, columns, params)),
        Expr::Paren(inner) => find_in_constraint(inner, columns, params),
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            let ci = col_index(expr, columns)?;
            if list.is_empty() {
                return None;
            }
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                vals.push(const_value(item, params)?);
            }
            Some((ci, vals))
        }
        _ => None,
    }
}

// ─── R-Tree byte-compatible on-disk node format (D3c) ───────────────────────
//
// SQLite stores an R-Tree as a b-tree of fixed-size nodes in `<name>_node`
// (`nodeno INTEGER PRIMARY KEY, data`), with `<name>_rowid` (rowid → leaf nodeno)
// and `<name>_parent` (node → parent node) maps. A node blob is: 2-byte BE depth
// (the tree height; meaningful only in the root, nodeno 1, else 0) + 2-byte BE
// cell count, then cells, zero-padded to the node size. Each cell is an 8-byte BE
// key (leaf: rowid; interior: child nodeno) followed by `n_coord` 4-byte BE
// coordinates (f32 for `rtree`, i32 for `rtree_i32`), laid out per dimension as
// (min, max).
//
// graphite reuses its M1 reader to get the current entries, applies the
// insert/delete, then BULK-REBUILDS a valid tree and rewrites the three shadow
// tables. SQLite reads any structurally-valid R-Tree (rtreecheck does not require
// a particular shape), so a simple balanced bulk build is byte-readable without
// reproducing SQLite's incremental quadratic-split tree shape.

/// One R-Tree entry / cell: an 8-byte key (rowid or child nodeno) and `2*nDim`
/// coordinates as f64 (exact for both the f32 and i32 on-disk forms).
#[derive(Clone)]
struct RtreeCell {
    key: i64,
    coords: Vec<f64>,
}

/// The fixed node size SQLite uses: `min(page_size - 64, 4 + 51*cell_size)`,
/// `cell_size = 8 + n_coord*4`, `51 = RTREE_MAXCELLS`.
fn rtree_node_size(n_coord: usize, page_size: usize) -> usize {
    let cell = 8 + n_coord * 4;
    page_size.saturating_sub(64).min(4 + 51 * cell)
}

/// Encode one node to its zero-padded blob. `is_root` puts the tree `depth` in
/// the header; non-root nodes carry 0 there.
fn rtree_encode_node(
    cells: &[RtreeCell],
    n_coord: usize,
    is_root: bool,
    depth: u16,
    integer: bool,
    node_size: usize,
) -> Vec<u8> {
    let mut b = alloc::vec![0u8; node_size];
    b[0..2].copy_from_slice(&(if is_root { depth } else { 0 }).to_be_bytes());
    b[2..4].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    let cell_size = 8 + n_coord * 4;
    for (i, c) in cells.iter().enumerate() {
        let off = 4 + i * cell_size;
        b[off..off + 8].copy_from_slice(&c.key.to_be_bytes());
        for (d, &v) in c.coords.iter().enumerate() {
            let p = off + 8 + d * 4;
            let bytes = if integer {
                (v as i32).to_be_bytes()
            } else {
                (v as f32).to_be_bytes()
            };
            b[p..p + 4].copy_from_slice(&bytes);
        }
    }
    b
}

/// The bounding box (per-dimension min/max, in coordinate-column order) of a set
/// of cells: union of their boxes.
fn rtree_union(cells: &[RtreeCell], n_coord: usize) -> Vec<f64> {
    let mut bb = alloc::vec![0.0f64; n_coord];
    for (ci, c) in cells.iter().enumerate() {
        for (d, slot) in bb.iter_mut().enumerate() {
            let v = c.coords.get(d).copied().unwrap_or(0.0);
            if ci == 0 {
                *slot = v;
            } else if d % 2 == 0 {
                *slot = slot.min(v); // a `min` coordinate column
            } else {
                *slot = slot.max(v); // a `max` coordinate column
            }
        }
    }
    bb
}

/// A bulk-built R-Tree, ready to write to the shadow tables.
struct RtreeBuild {
    /// `(nodeno, encoded blob)` for every node.
    nodes: Vec<(i64, Vec<u8>)>,
    /// `(rowid, leaf nodeno)` for every entry.
    rowids: Vec<(i64, i64)>,
    /// `(child nodeno, parent nodeno)` for every non-root node.
    parents: Vec<(i64, i64)>,
}

/// Bulk-build a balanced R-Tree from `entries`. The root is always nodeno 1.
fn rtree_bulk_build(
    entries: Vec<RtreeCell>,
    n_coord: usize,
    integer: bool,
    node_size: usize,
) -> RtreeBuild {
    let max_cells = ((node_size - 4) / (8 + n_coord * 4)).max(1);
    // Empty tree: a single empty leaf root.
    if entries.is_empty() {
        return RtreeBuild {
            nodes: alloc::vec![(
                1,
                rtree_encode_node(&[], n_coord, true, 0, integer, node_size)
            )],
            rowids: Vec::new(),
            parents: Vec::new(),
        };
    }
    // Build levels bottom-up. A node is its list of cells; an interior cell's key
    // is a placeholder index into the child level, resolved to a nodeno later.
    // levels[0] = leaves; cells there carry the real rowid keys.
    let mut levels: Vec<Vec<Vec<RtreeCell>>> = Vec::new();
    levels.push(entries.chunks(max_cells).map(<[_]>::to_vec).collect());
    while levels.last().map_or(0, Vec::len) > 1 {
        let child_level = levels.len() - 1;
        let children = &levels[child_level];
        // Each parent cell summarizes one child: key = child index (placeholder).
        let parent_cells: Vec<RtreeCell> = (0..children.len())
            .map(|idx| RtreeCell {
                key: idx as i64,
                coords: rtree_union(&children[idx], n_coord),
            })
            .collect();
        levels.push(parent_cells.chunks(max_cells).map(<[_]>::to_vec).collect());
    }
    let root_level = levels.len() - 1;
    let depth = root_level as u16;

    // Assign node numbers: the root (top level, node 0) is 1; everything else
    // follows. Record nodeno for each (level, node-index).
    let mut nodeno_of: alloc::collections::BTreeMap<(usize, usize), i64> =
        alloc::collections::BTreeMap::new();
    nodeno_of.insert((root_level, 0), 1);
    let mut next = 2i64;
    for level in (0..levels.len()).rev() {
        for idx in 0..levels[level].len() {
            nodeno_of.entry((level, idx)).or_insert_with(|| {
                let n = next;
                next += 1;
                n
            });
        }
    }

    let mut nodes = Vec::new();
    let mut rowids = Vec::new();
    let mut parents = Vec::new();
    for level in 0..levels.len() {
        let is_leaf = level == 0;
        for (idx, cells) in levels[level].iter().enumerate() {
            let nodeno = nodeno_of[&(level, idx)];
            let is_root = level == root_level;
            // Resolve interior placeholder keys to child nodenos, and record the
            // parent + rowid maps.
            let resolved: Vec<RtreeCell> = cells
                .iter()
                .map(|c| {
                    if is_leaf {
                        rowids.push((c.key, nodeno));
                        c.clone()
                    } else {
                        let child = nodeno_of[&(level - 1, c.key as usize)];
                        parents.push((child, nodeno));
                        RtreeCell {
                            key: child,
                            coords: c.coords.clone(),
                        }
                    }
                })
                .collect();
            nodes.push((
                nodeno,
                rtree_encode_node(&resolved, n_coord, is_root, depth, integer, node_size),
            ));
        }
    }
    RtreeBuild {
        nodes,
        rowids,
        parents,
    }
}

/// Build a leaf cell from an R-Tree INSERT's column values `[id, c0, c1, …]`,
/// rounding each coordinate to the conservative f32 form (min columns down, max
/// columns up — SQLite's rtreeValueDown/Up) or clamping to i32 for `rtree_i32`.
/// Rejects a coordinate pair with `min > max`, like SQLite.
fn rtree_cell_from_values(
    rowid: i64,
    values: &[Value],
    n_coord: usize,
    integer: bool,
) -> Result<RtreeCell> {
    for d in 0..n_coord / 2 {
        let mn = values.get(1 + 2 * d).map_or(0.0, crate::vtab::coord_f64);
        let mx = values.get(2 + 2 * d).map_or(0.0, crate::vtab::coord_f64);
        if mn > mx {
            return Err(Error::Error("rtree constraint failed".into()));
        }
    }
    let coords = (0..n_coord)
        .map(|d| {
            let v = values.get(1 + d).map_or(0.0, crate::vtab::coord_f64);
            if integer {
                (v as i64).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as f64
            } else if d % 2 == 0 {
                crate::vtab::round_min_f32(v)
            } else {
                crate::vtab::round_max_f32(v)
            }
        })
        .collect();
    Ok(RtreeCell { key: rowid, coords })
}

/// Whether `e` is a `rowid` / `_rowid_` / `oid` reference (case-insensitive,
/// optionally table-qualified) that is NOT shadowed by a real column of that
/// name — i.e. it denotes the table's rowid, seekable directly in the table
/// b-tree whether or not the table has an explicit INTEGER PRIMARY KEY column.
fn is_rowid_ref(e: &Expr, columns: &[ColumnInfo]) -> bool {
    matches!(e, Expr::Column { column, .. }
        if matches!(column.to_ascii_lowercase().as_str(), "rowid" | "_rowid_" | "oid")
            && !columns.iter().any(|c| c.name.eq_ignore_ascii_case(column)))
}

/// Detect a `rowid = const` equality or `rowid IN (list)` in `where_expr` (the
/// rowid alias not shadowed by a real column), returning the candidate rowids to
/// seek directly in the table b-tree. `run_core` re-applies the full WHERE, so a
/// non-integer literal (`rowid = 5.5`) is a harmless superset.
fn rowid_seek_constraint(
    where_expr: &Expr,
    columns: &[ColumnInfo],
    params: &Params,
) -> Option<Vec<i64>> {
    match where_expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => rowid_seek_constraint(left, columns, params)
            .or_else(|| rowid_seek_constraint(right, columns, params)),
        Expr::Paren(inner) => rowid_seek_constraint(inner, columns, params),
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            let other = if is_rowid_ref(left, columns) {
                right
            } else if is_rowid_ref(right, columns) {
                left
            } else {
                return None;
            };
            Some(alloc::vec![eval::to_i64(&const_value(other, params)?)])
        }
        Expr::InList {
            expr,
            list,
            negated: false,
        } if is_rowid_ref(expr, columns) && !list.is_empty() => {
            let mut out = Vec::with_capacity(list.len());
            for item in list {
                out.push(eval::to_i64(&const_value(item, params)?));
            }
            Some(out)
        }
        _ => None,
    }
}

/// A per-column range constraint gathered from `WHERE`: optional lower and upper
/// bounds, each `(value, inclusive)`.
#[derive(Default, Clone)]
struct RangeBound {
    lower: Option<(Value, bool)>,
    upper: Option<(Value, bool)>,
}

/// Fold one comparison `column <op> value` into a [`RangeBound`]. Overwriting an
/// existing bound is safe: the index range scan only needs to return a superset
/// (the full `WHERE` is re-applied), and either of two bounds on the same side
/// yields a valid superset.
fn apply_bound(b: &mut RangeBound, op: BinaryOp, v: Value) {
    match op {
        BinaryOp::Gt => b.lower = Some((v, false)),
        BinaryOp::GtEq => b.lower = Some((v, true)),
        BinaryOp::Lt => b.upper = Some((v, false)),
        BinaryOp::LtEq => b.upper = Some((v, true)),
        _ => {}
    }
}

/// The comparison with its operands swapped (`a < b` ⇔ `b > a`).
fn flip_cmp(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

/// Collect per-column range bounds (`<`/`<=`/`>`/`>=`/`BETWEEN`) from the
/// top-level `AND` conjuncts of `WHERE`, keyed by column index. Drives an index
/// range scan; non-range and non-constant terms are ignored (the full `WHERE` is
/// re-applied afterward).
fn collect_range_constraints(
    e: &Expr,
    columns: &[ColumnInfo],
    params: &Params,
    out: &mut alloc::collections::BTreeMap<usize, RangeBound>,
) {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect_range_constraints(left, columns, params, out);
            collect_range_constraints(right, columns, params, out);
        }
        Expr::Paren(inner) => collect_range_constraints(inner, columns, params, out),
        Expr::Binary { op, left, right }
            if matches!(
                op,
                BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
            ) =>
        {
            if let (Some(ci), Some(v)) = (col_index(left, columns), const_value(right, params)) {
                apply_bound(out.entry(ci).or_default(), *op, v);
            } else if let (Some(ci), Some(v)) =
                (col_index(right, columns), const_value(left, params))
            {
                apply_bound(out.entry(ci).or_default(), flip_cmp(*op), v);
            }
        }
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            if let Some(ci) = col_index(expr, columns) {
                let b = out.entry(ci).or_default();
                if let Some(v) = const_value(low, params) {
                    apply_bound(b, BinaryOp::GtEq, v);
                }
                if let Some(v) = const_value(high, params) {
                    apply_bound(b, BinaryOp::LtEq, v);
                }
            }
        }
        _ => {}
    }
}

/// The column index a bare/qualified column expression resolves to, if any.
fn col_index(e: &Expr, columns: &[ColumnInfo]) -> Option<usize> {
    if let Expr::Column { table, column } = e {
        columns.iter().position(|c| {
            c.name.eq_ignore_ascii_case(column)
                && table
                    .as_deref()
                    .is_none_or(|t| c.table.eq_ignore_ascii_case(t))
        })
    } else {
        None
    }
}

/// Evaluate `e` as a constant (no column references), or `None` if it depends on
/// a row.
fn const_value(e: &Expr, params: &Params) -> Option<Value> {
    eval::eval(e, &EvalCtx::rowless(params)).ok()
}

/// Coerce each value to its column's type affinity (SQLite storage affinity).
fn apply_column_affinity(meta: &TableMeta, values: &mut [Value]) {
    for (i, v) in values.iter_mut().enumerate() {
        let taken = core::mem::replace(v, Value::Null);
        *v = meta.columns[i].affinity.coerce(taken);
    }
}

/// Enforce declared `NOT NULL` column constraints over a fully-built row.
fn check_not_null(meta: &TableMeta, values: &[Value]) -> Result<()> {
    for (i, v) in values.iter().enumerate() {
        if meta.not_null[i].is_some() && matches!(v, Value::Null) {
            return Err(Error::Constraint(format!(
                "NOT NULL constraint failed: {}.{}",
                meta.columns[i].table, meta.columns[i].name
            )));
        }
    }
    Ok(())
}

/// Build an index key record: the indexed column values followed by the trailing
/// rowid (which makes every index key unique and supports lookups).
fn index_key(cols: &[usize], values: &[Value], rowid: i64) -> Vec<u8> {
    let mut key: Vec<Value> = cols.iter().map(|&p| values[p].clone()).collect();
    key.push(Value::Integer(rowid));
    encode_record(&key)
}

/// Whether rows `a` and `b` collide on any UNIQUE/PRIMARY KEY column set (all
/// columns of the set equal and none NULL — NULLs are distinct, as in SQLite).
/// Build the `sqlite_stat1` `stat` string for an index over `rows`: `nRow`
/// followed by, for each leftmost prefix length `K`, `(nRow + dK/2) / dK` where
/// `dK` is the number of distinct prefixes of length `K` (collation-aware).
fn index_stat_string(
    cols: &[usize],
    colls: &[crate::value::Collation],
    rows: &[Vec<Value>],
) -> String {
    let n = rows.len();
    let mut tuples: Vec<Vec<Value>> = rows
        .iter()
        .map(|r| cols.iter().map(|&c| r[c].clone()).collect())
        .collect();
    tuples.sort_by(|a, b| stat_prefix_cmp(a, b, colls, cols.len()));
    let mut s = alloc::format!("{n}");
    for k in 1..=cols.len() {
        let mut distinct = 1usize; // n > 0 guaranteed by the caller
        for w in tuples.windows(2) {
            if stat_prefix_cmp(&w[0], &w[1], colls, k) != core::cmp::Ordering::Equal {
                distinct += 1;
            }
        }
        let avg = (n + distinct / 2) / distinct;
        s.push(' ');
        s.push_str(&avg.to_string());
    }
    s
}

/// Compare the leftmost `len` columns of two index tuples under per-column
/// collations (used to count distinct prefixes for `ANALYZE`).
fn stat_prefix_cmp(
    a: &[Value],
    b: &[Value],
    colls: &[crate::value::Collation],
    len: usize,
) -> core::cmp::Ordering {
    for i in 0..len {
        let coll = colls.get(i).copied().unwrap_or_default();
        let ord = crate::value::cmp_values_coll(&a[i], &b[i], coll);
        if ord != core::cmp::Ordering::Equal {
            return ord;
        }
    }
    core::cmp::Ordering::Equal
}

fn unique_match(meta: &TableMeta, a: &[Value], b: &[Value]) -> bool {
    meta.unique.iter().any(|(set, _)| {
        set.iter().all(|&c| {
            !matches!(a[c], Value::Null)
                && !matches!(b[c], Value::Null)
                && crate::value::cmp_values_coll(&a[c], &b[c], meta.columns[c].collation).is_eq()
        })
    })
}

/// SQLite's UNIQUE-violation message for two WITHOUT ROWID rows that collide on
/// an inline `UNIQUE`/`PRIMARY KEY` set (`UNIQUE constraint failed: t.a[, t.b]`),
/// or the bare message when the collision is on a standalone unique index.
fn wr_unique_message(meta: &TableMeta, a: &[Value], b: &[Value]) -> String {
    meta.unique
        .iter()
        .find(|(set, _)| {
            set.iter().all(|&c| {
                !matches!(a[c], Value::Null)
                    && !matches!(b[c], Value::Null)
                    && crate::value::cmp_values_coll(&a[c], &b[c], meta.columns[c].collation)
                        .is_eq()
            })
        })
        .map(|(set, _)| {
            let cols = set
                .iter()
                .map(|&i| alloc::format!("{}.{}", meta.columns[i].table, meta.columns[i].name))
                .collect::<Vec<_>>()
                .join(", ");
            alloc::format!("UNIQUE constraint failed: {cols}")
        })
        .unwrap_or_else(|| String::from("UNIQUE constraint failed"))
}

/// An index record for a `WITHOUT ROWID` table: the indexed columns followed by
/// the table's PRIMARY KEY columns (which make the entry unique), as SQLite does.
fn wr_index_key(cols: &[usize], pk_cols: &[usize], values: &[Value]) -> Vec<u8> {
    let mut key: Vec<Value> = cols.iter().map(|&p| values[p].clone()).collect();
    key.extend(pk_cols.iter().map(|&p| values[p].clone()));
    encode_record(&key)
}

#[derive(Clone)]
struct InputRow {
    values: Vec<Value>,
    rowid: Option<i64>,
}

impl InputRow {
    fn ctx<'a>(&'a self, columns: &'a [ColumnInfo], params: &'a Params) -> EvalCtx<'a> {
        EvalCtx {
            row: &self.values,
            columns,
            rowid: self.rowid,
            params,
            anon_counter: core::cell::Cell::new(0),
            subqueries: None,
        }
    }
}

/// Build an evaluation context for a standalone `(values, rowid)` row.
fn row_ctx<'a>(
    values: &'a [Value],
    columns: &'a [ColumnInfo],
    rowid: Option<i64>,
    params: &'a Params,
) -> EvalCtx<'a> {
    EvalCtx {
        row: values,
        columns,
        rowid,
        params,
        anon_counter: core::cell::Cell::new(0),
        subqueries: None,
    }
}

/// The conventional `<path>-journal` companion file name.
fn journal_path(path: &str) -> String {
    let mut p = String::from(path);
    p.push_str("-journal");
    p
}

/// The conventional `<path>-wal` companion file name.
fn wal_path(path: &str) -> String {
    let mut p = String::from(path);
    p.push_str("-wal");
    p
}

struct OutRow {
    values: Vec<Value>,
    sort_keys: Vec<Value>,
}

/// Output column labels for a `RETURNING` projection (mirrors a `SELECT` list:
/// `*`/`tbl.*` expand to table column names, expressions use their alias or a
/// derived label).
fn returning_labels(returning: &[ResultColumn], columns: &[ColumnInfo]) -> Vec<String> {
    let mut labels = Vec::new();
    for col in returning {
        match col {
            ResultColumn::Wildcard => {
                for c in columns {
                    labels.push(c.name.clone());
                }
            }
            ResultColumn::TableWildcard(t) => {
                for c in columns {
                    if c.table.eq_ignore_ascii_case(t) {
                        labels.push(c.name.clone());
                    }
                }
            }
            ResultColumn::Expr {
                expr,
                alias,
                source,
            } => {
                labels.push(result_column_label(expr, alias, source));
            }
        }
    }
    labels
}

fn project_column(
    col: &ResultColumn,
    columns: &[ColumnInfo],
    ctx: &EvalCtx,
    out: &mut Vec<Value>,
) -> Result<()> {
    match col {
        ResultColumn::Wildcard => {
            for v in ctx.row {
                out.push(v.clone());
            }
        }
        ResultColumn::TableWildcard(table) => {
            for (i, c) in columns.iter().enumerate() {
                if c.table.eq_ignore_ascii_case(table) {
                    out.push(ctx.row[i].clone());
                }
            }
        }
        ResultColumn::Expr { expr, .. } => {
            out.push(eval::eval(expr, ctx)?);
        }
    }
    Ok(())
}

/// If `expr` is a positional reference — a (possibly negated) integer literal,
/// optionally wrapped in parentheses or a `COLLATE` clause — return its signed
/// value. SQLite reads such a term in `GROUP BY` / `ORDER BY` as a 1-based output
/// column index; an expression like `1+1` is *not* positional. Used only for
/// range validation: in-range resolution still goes through
/// [`resolve_order_index`].
fn positional_int(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(Literal::Integer(n)) => Some(*n),
        Expr::Unary {
            op: UnaryOp::Negate,
            expr,
        } => match expr.as_ref() {
            Expr::Literal(Literal::Integer(n)) => Some(n.wrapping_neg()),
            _ => None,
        },
        Expr::Collate { expr, .. } | Expr::Paren(expr) => positional_int(expr),
        _ => None,
    }
}

/// Reject any `GROUP BY` / `ORDER BY` positional term that falls outside
/// `1..=ncols`, matching SQLite's "Nth GROUP BY/ORDER BY term out of range".
/// `ncols` is the query's output-column count.
fn check_positional_terms(group_by: &[Expr], order_by: &[OrderTerm], ncols: usize) -> Result<()> {
    for g in group_by {
        if let Some(n) = positional_int(g) {
            if n < 1 || (n as u64) > ncols as u64 {
                return Err(Error::Error("GROUP BY term out of range".into()));
            }
        }
    }
    for t in order_by {
        if let Some(n) = positional_int(&t.expr) {
            if n < 1 || (n as u64) > ncols as u64 {
                return Err(Error::Error("ORDER BY term out of range".into()));
            }
        }
    }
    Ok(())
}

/// Apply SQLite's `OP_MustBeInt` to a `LIMIT`/`OFFSET` value: it must be an
/// integer, or a real / fully-numeric text string that is exactly integer-valued
/// and in range. A non-integral real (`1.9`), text with trailing garbage
/// (`'2abc'`), NULL, or a blob is a `datatype mismatch` error — SQLite does not
/// silently truncate or treat NULL as zero here.
fn must_be_int(v: Value) -> Result<i64> {
    fn real_exact(r: f64) -> Result<i64> {
        if r.is_finite()
            && r == crate::util::float::trunc(r)
            && r >= i64::MIN as f64
            && r < 9_223_372_036_854_775_808.0
        {
            Ok(r as i64)
        } else {
            Err(Error::Error("datatype mismatch".into()))
        }
    }
    match v {
        Value::Integer(i) => Ok(i),
        Value::Real(r) => real_exact(r),
        Value::Text(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                Ok(i)
            } else if let Ok(r) = t.parse::<f64>() {
                real_exact(r)
            } else {
                Err(Error::Error("datatype mismatch".into()))
            }
        }
        Value::Null | Value::Blob(_) => Err(Error::Error("datatype mismatch".into())),
    }
}

/// Resolve an `ORDER BY` term to an output-column index when it refers to one:
/// a positive integer literal `N` (1-based position), or a bare column name that
/// matches a result-column label/alias. Returns `None` for general expressions,
/// which are evaluated against the row instead.
fn resolve_order_index(expr: &Expr, labels: &[String], ncols: usize) -> Option<usize> {
    match expr {
        Expr::Literal(Literal::Integer(n)) => {
            let idx = (*n as usize).checked_sub(1)?;
            (idx < ncols).then_some(idx)
        }
        Expr::Column {
            table: None,
            column,
        } => labels.iter().position(|l| l.eq_ignore_ascii_case(column)),
        // `ORDER BY <alias> COLLATE …` (or a parenthesized term) still resolves to
        // the output column; the explicit collation is applied by the sort
        // comparison via `order_collations`/`key_collation`.
        Expr::Collate { expr, .. } | Expr::Paren(expr) => resolve_order_index(expr, labels, ncols),
        _ => None,
    }
}

/// Invoke `f(is_max, arg)` for each plain (non-window) single-argument `min()` /
/// `max()` aggregate call in `expr`. Used to detect SQLite's bare-column rule:
/// a query with exactly one `min`/`max` takes bare columns from the extreme row.
fn for_each_minmax(expr: &Expr, f: &mut dyn FnMut(bool, &Expr)) {
    match expr {
        Expr::Function {
            over: Some(_),
            args,
            ..
        } => {
            for a in args {
                for_each_minmax(a, f);
            }
        }
        Expr::Function {
            name,
            args,
            star: false,
            ..
        } => {
            if args.len() == 1 {
                let l = name.to_ascii_lowercase();
                if l == "min" || l == "max" {
                    f(l == "max", &args[0]);
                }
            }
            for a in args {
                for_each_minmax(a, f);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                for_each_minmax(a, f);
            }
        }
        Expr::Binary { left, right, .. } => {
            for_each_minmax(left, f);
            for_each_minmax(right, f);
        }
        Expr::Unary { expr, .. }
        | Expr::Paren(expr)
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. } => for_each_minmax(expr, f),
        Expr::Between {
            expr, low, high, ..
        } => {
            for_each_minmax(expr, f);
            for_each_minmax(low, f);
            for_each_minmax(high, f);
        }
        Expr::InList { expr, list, .. } => {
            for_each_minmax(expr, f);
            for l in list {
                for_each_minmax(l, f);
            }
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            if let Some(o) = operand {
                for_each_minmax(o, f);
            }
            for (w, t) in when_then {
                for_each_minmax(w, f);
                for_each_minmax(t, f);
            }
            if let Some(e) = else_result {
                for_each_minmax(e, f);
            }
        }
        _ => {}
    }
}

/// If a grouped query references exactly one `min()`/`max()` aggregate (anywhere
/// in its result columns, `HAVING`, or `ORDER BY`), return `(is_max, arg)`: bare
/// columns then take their values from the row achieving that extreme, per
/// SQLite. `min(a,b)`/`max(a,b)` (scalar, 2-arg) and window forms don't qualify.
fn single_minmax_arg(sel: &Select) -> Option<(bool, Expr)> {
    let mut hits: Vec<(bool, Expr)> = Vec::new();
    let mut collect =
        |e: &Expr| for_each_minmax(e, &mut |is_max, arg| hits.push((is_max, arg.clone())));
    for col in &sel.columns {
        if let ResultColumn::Expr { expr, .. } = col {
            collect(expr);
        }
    }
    if let Some(h) = &sel.having {
        collect(h);
    }
    for term in &sel.order_by {
        collect(&term.expr);
    }
    if hits.len() == 1 {
        hits.pop()
    } else {
        None
    }
}

/// Whether `expr` contains an aggregate-function call, using a caller-supplied
/// predicate to decide whether a function name (with its arg count / `*` flag) is
/// an aggregate — so `has_aggregate` can recognize built-in *and* user-registered
/// aggregate functions. A window call (`f(…) OVER (…)`) is not itself an aggregate.
/// Per-query FTS5 state for the aux columns/functions, built by `run_core` for a
/// `MATCH` query over a single `fts5` table and read by `rank`/`bm25()`/
/// `highlight()` during projection and `ORDER BY`.
#[cfg(feature = "fts5")]
struct Fts5QueryCtx {
    /// The fts5 table's column names.
    col_names: Vec<String>,
    /// The literal `MATCH` query string.
    query: String,
    /// A `col MATCH …` operand column (whole-query scope), if any.
    scope: Option<String>,
    /// The searchable (indexed) column names — every column except those declared
    /// `UNINDEXED`. `None` when all columns are indexed (the common case).
    indexed: Option<Vec<String>>,
    /// The table's resolved tokenizer config (Porter stemming + `remove_diacritics`
    /// level), so `highlight()`/`snippet()` fold exactly like the indexed docs.
    tok: crate::vtab::Fts5Tok,
    /// The bm25 corpus + rowid→document-index map — present only when `rank` /
    /// `bm25()` is referenced (`highlight()` needs only the query, not the corpus).
    bm25: Option<(
        crate::vtab::Fts5Bm25,
        alloc::collections::BTreeMap<i64, usize>,
    )>,
}

#[cfg(feature = "fts5")]
impl Fts5QueryCtx {
    /// Whether `col` is searchable (not `UNINDEXED`).
    fn col_indexed(&self, col: &str) -> bool {
        self.indexed
            .as_ref()
            .is_none_or(|cols| cols.iter().any(|n| n.eq_ignore_ascii_case(col)))
    }
}

/// Restores [`Connection::fts5_rank`] when a `run_core` invocation ends, so a
/// nested query's FTS5 state never leaks into the caller (or vice versa).
#[cfg(feature = "fts5")]
struct Fts5RankGuard<'a> {
    conn: &'a Connection,
    prev: Option<Fts5QueryCtx>,
}

#[cfg(feature = "fts5")]
impl core::ops::Drop for Fts5RankGuard<'_> {
    fn drop(&mut self) {
        *self.conn.fts5_rank.borrow_mut() = self.prev.take();
    }
}

/// Whether an expression references one of `names` as an unqualified column (the
/// FTS5 `rank` column) or as a function call (`bm25(…)`, `highlight(…)`, …).
#[cfg(feature = "fts5")]
fn expr_mentions_any(expr: &Expr, names: &[&str]) -> bool {
    let rec = |e: &Expr| expr_mentions_any(e, names);
    match expr {
        Expr::Column {
            table: None,
            column,
        } => names.iter().any(|n| column.eq_ignore_ascii_case(n)),
        Expr::Function { name, args, .. } => {
            names.iter().any(|n| name.eq_ignore_ascii_case(n)) || args.iter().any(rec)
        }
        Expr::Binary { left, right, .. } => rec(left) || rec(right),
        Expr::Unary { expr, .. } | Expr::Paren(expr) => rec(expr),
        Expr::IsNull { expr, .. } => rec(expr),
        Expr::Between {
            expr, low, high, ..
        } => rec(expr) || rec(low) || rec(high),
        Expr::InList { expr, list, .. } => rec(expr) || list.iter().any(rec),
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            operand.as_deref().is_some_and(rec)
                || when_then.iter().any(|(w, t)| rec(w) || rec(t))
                || else_result.as_deref().is_some_and(rec)
        }
        Expr::Cast { expr, .. } => rec(expr),
        _ => false,
    }
}

/// Whether a SELECT's projection, `ORDER BY`, or `HAVING` references any of
/// `names` — the cheap gate before building FTS5 query state.
#[cfg(feature = "fts5")]
fn select_mentions(sel: &Select, names: &[&str]) -> bool {
    sel.columns
        .iter()
        .any(|c| matches!(c, ResultColumn::Expr { expr, .. } if expr_mentions_any(expr, names)))
        || sel
            .order_by
            .iter()
            .any(|t| expr_mentions_any(&t.expr, names))
        || sel
            .having
            .as_ref()
            .is_some_and(|h| expr_mentions_any(h, names))
}

fn expr_contains_agg(expr: &Expr, is_agg: &dyn Fn(&str, usize, bool) -> bool) -> bool {
    let rec = |e: &Expr| expr_contains_agg(e, is_agg);
    match expr {
        // A window function (`f(…) OVER (…)`) is not a plain aggregate, even when
        // `f` is an aggregate name; only its arguments might contain aggregates.
        Expr::Function {
            over: Some(_),
            args,
            ..
        } => args.iter().any(rec),
        Expr::Function {
            name, args, star, ..
        } => is_agg(name, args.len(), *star) || args.iter().any(rec),
        Expr::Binary { left, right, .. } => rec(left) || rec(right),
        Expr::Unary { expr, .. } | Expr::Paren(expr) => rec(expr),
        Expr::IsNull { expr, .. } => rec(expr),
        Expr::Between {
            expr, low, high, ..
        } => rec(expr) || rec(low) || rec(high),
        Expr::InList { expr, list, .. } => rec(expr) || list.iter().any(rec),
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            operand.as_deref().is_some_and(rec)
                || when_then.iter().any(|(w, t)| rec(w) || rec(t))
                || else_result.as_deref().is_some_and(rec)
        }
        Expr::Cast { expr, .. } => rec(expr),
        _ => false,
    }
}

/// Combine two compound-query operand row sets per the operator.
fn apply_compound(
    op: CompoundOp,
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
    colls: &[crate::value::Collation],
) -> Vec<Vec<Value>> {
    // Set comparison uses the left SELECT's per-column collations (SQLite).
    let eq = |a: &[Value], b: &[Value]| rows_equal_coll(a, b, colls);
    // Deduplicate, keeping the *last* occurrence's representation: when two rows
    // are equal but differ in type (e.g. `1` vs `1.0`), SQLite's compound dedup
    // keeps the later one (`SELECT 1 UNION SELECT 1.0` yields `1.0`).
    let dedup = |rows: Vec<Vec<Value>>| -> Vec<Vec<Value>> {
        let mut seen: Vec<Vec<Value>> = Vec::new();
        for r in rows {
            match seen.iter().position(|s| eq(s, &r)) {
                Some(i) => seen[i] = r,
                None => seen.push(r),
            }
        }
        seen
    };
    match op {
        CompoundOp::UnionAll => {
            let mut out = left;
            out.extend(right);
            out
        }
        CompoundOp::Union => {
            let mut out = left;
            out.extend(right);
            dedup(out)
        }
        CompoundOp::Intersect => dedup(
            left.into_iter()
                .filter(|l| right.iter().any(|r| eq(l, r)))
                .collect(),
        ),
        CompoundOp::Except => dedup(
            left.into_iter()
                .filter(|l| !right.iter().any(|r| eq(l, r)))
                .collect(),
        ),
    }
}

fn rows_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| eval::compare(x, y) == core::cmp::Ordering::Equal)
}

/// Like [`rows_equal`] but comparing column `i` under collation `colls[i]`
/// (missing entries default to `BINARY`).
fn rows_equal_coll(a: &[Value], b: &[Value], colls: &[crate::value::Collation]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).enumerate().all(|(i, (x, y))| {
            let c = colls.get(i).copied().unwrap_or_default();
            crate::value::cmp_values_coll(x, y, c) == core::cmp::Ordering::Equal
        })
}

fn dedup_values(vals: &mut Vec<Value>, coll: crate::value::Collation) {
    let mut seen: Vec<Value> = Vec::new();
    vals.retain(|v| {
        if seen
            .iter()
            .any(|s| crate::value::cmp_values_coll(s, v, coll) == core::cmp::Ordering::Equal)
        {
            false
        } else {
            seen.push(v.clone());
            true
        }
    });
}

fn value_to_literal(v: Value) -> Literal {
    match v {
        Value::Null => Literal::Null,
        Value::Integer(i) => Literal::Integer(i),
        Value::Real(r) => Literal::Real(r),
        Value::Text(s) => Literal::Str(s),
        Value::Blob(b) => Literal::Blob(b),
    }
}

/// A list of `(column name, declared type)` pairs — a resolved column set for
/// `view_table_info` (the type is `None` for an expression column).
type NamedColumns = Vec<(String, Option<String>)>;

/// A column's inherited `(affinity, collating sequence)` — what a derived-table
/// column takes from its origin column (see `subquery_column_origins`).
type ColOrigin = (eval::Affinity, crate::value::Collation);

/// The column headers for `PRAGMA table_info` / `table_xinfo`.
fn table_info_columns(extended: bool) -> Vec<String> {
    let mut c: Vec<String> = ["cid", "name", "type", "notnull", "dflt_value", "pk"]
        .iter()
        .map(|s| String::from(*s))
        .collect();
    if extended {
        c.push(String::from("hidden"));
    }
    c
}

/// Rename every reference to column `old` (of table `table`) to `new` within an
/// expression — both unqualified (`old`) and table-qualified (`table.old`) forms.
/// Used to keep CHECK / generated / DEFAULT expressions valid across an
/// `ALTER TABLE … RENAME COLUMN`. (CHECK/generated/default forbid subqueries, so
/// the non-recursing `replace_expr` covers them.)
fn rename_column_ref(e: &mut Expr, table: &str, old: &str, new: &str) {
    window::replace_expr(
        e,
        &Expr::Column {
            table: None,
            column: String::from(old),
        },
        &Expr::Column {
            table: None,
            column: String::from(new),
        },
    );
    window::replace_expr(
        e,
        &Expr::Column {
            table: Some(String::from(table)),
            column: String::from(old),
        },
        &Expr::Column {
            table: Some(String::from(table)),
            column: String::from(new),
        },
    );
}

/// Rename every reference to table `old` → `new` throughout a `Select`: its
/// `FROM` table references and every table-qualified `old.col` / `old.*`, recursing
/// into subqueries, CTE bodies, and compound parts. Used to keep a dependent view
/// body valid across `ALTER TABLE … RENAME TO`. A same-level CTE named `old`
/// shadows the table, so `FROM old`/`old.*` there is left alone.
fn rename_table_in_select(sel: &mut Select, old: &str, new: &str) {
    let shadowed = sel.ctes.iter().any(|c| c.name.eq_ignore_ascii_case(old));
    for cte in &mut sel.ctes {
        rename_table_in_select(&mut cte.select, old, new);
    }
    if let Some(from) = &mut sel.from {
        rename_table_in_ref(&mut from.first, old, new, shadowed);
        for j in &mut from.joins {
            rename_table_in_ref(&mut j.table, old, new, shadowed);
            if let Some(on) = &mut j.on {
                rename_table_in_expr(on, old, new);
            }
        }
    }
    for rc in &mut sel.columns {
        match rc {
            ResultColumn::Expr { expr, .. } => rename_table_in_expr(expr, old, new),
            ResultColumn::TableWildcard(t) if !shadowed && t.eq_ignore_ascii_case(old) => {
                *t = String::from(new);
            }
            _ => {}
        }
    }
    if let Some(w) = &mut sel.where_clause {
        rename_table_in_expr(w, old, new);
    }
    for e in &mut sel.group_by {
        rename_table_in_expr(e, old, new);
    }
    if let Some(h) = &mut sel.having {
        rename_table_in_expr(h, old, new);
    }
    for t in &mut sel.order_by {
        rename_table_in_expr(&mut t.expr, old, new);
    }
    for (_, ws) in &mut sel.window_defs {
        rename_table_in_window(ws, old, new);
    }
    if let Some(e) = &mut sel.limit {
        rename_table_in_expr(e, old, new);
    }
    if let Some(e) = &mut sel.offset {
        rename_table_in_expr(e, old, new);
    }
    for (_, comp) in &mut sel.compound {
        rename_table_in_select(comp, old, new);
    }
}

/// Rename a table reference within a `FROM` source (recursing into a derived
/// subquery). A real table named `old` (not schema-qualified, not shadowed by a
/// same-level CTE) is repointed to `new`.
fn rename_table_in_ref(tref: &mut TableRef, old: &str, new: &str, shadowed: bool) {
    if let Some(sub) = &mut tref.subquery {
        rename_table_in_select(sub, old, new);
    } else if tref.schema.is_none() && !shadowed && tref.name.eq_ignore_ascii_case(old) {
        tref.name = String::from(new);
    }
}

/// Rename `old` → `new` in a window spec's `PARTITION BY` / `ORDER BY` expressions.
fn rename_table_in_window(ws: &mut WindowSpec, old: &str, new: &str) {
    for e in &mut ws.partition_by {
        rename_table_in_expr(e, old, new);
    }
    for t in &mut ws.order_by {
        rename_table_in_expr(&mut t.expr, old, new);
    }
}

/// Rename a table qualifier `old.col` → `new.col` throughout an expression,
/// recursing into every sub-expression and nested subquery.
fn rename_table_in_expr(e: &mut Expr, old: &str, new: &str) {
    match e {
        Expr::Column { table: Some(t), .. } if t.eq_ignore_ascii_case(old) => {
            *t = String::from(new)
        }
        Expr::Column { .. } | Expr::Literal(_) | Expr::Parameter(_) => {}
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Paren(expr)
        | Expr::Collate { expr, .. } => rename_table_in_expr(expr, old, new),
        Expr::Binary { left, right, .. } => {
            rename_table_in_expr(left, old, new);
            rename_table_in_expr(right, old, new);
        }
        Expr::Function {
            args,
            filter,
            order_by,
            over,
            ..
        } => {
            for a in args {
                rename_table_in_expr(a, old, new);
            }
            if let Some(f) = filter {
                rename_table_in_expr(f, old, new);
            }
            for t in order_by {
                rename_table_in_expr(&mut t.expr, old, new);
            }
            if let Some(w) = over {
                rename_table_in_window(w, old, new);
            }
        }
        Expr::InList { expr, list, .. } => {
            rename_table_in_expr(expr, old, new);
            for a in list {
                rename_table_in_expr(a, old, new);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            rename_table_in_expr(expr, old, new);
            rename_table_in_expr(low, old, new);
            rename_table_in_expr(high, old, new);
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            if let Some(o) = operand {
                rename_table_in_expr(o, old, new);
            }
            for (w, t) in when_then {
                rename_table_in_expr(w, old, new);
                rename_table_in_expr(t, old, new);
            }
            if let Some(el) = else_result {
                rename_table_in_expr(el, old, new);
            }
        }
        Expr::RowValue(items) => {
            for i in items {
                rename_table_in_expr(i, old, new);
            }
        }
        Expr::Subquery(s) => rename_table_in_select(s, old, new),
        Expr::Exists { select, .. } => rename_table_in_select(select, old, new),
        Expr::InSelect { expr, select, .. } => {
            rename_table_in_expr(expr, old, new);
            rename_table_in_select(select, old, new);
        }
    }
}

/// Split a `;`-separated SQL script into trimmed statement slices for
/// [`Connection::execute_batch`]. Reuses the tokenizer, so string literals and
/// `--`/`/* */` comments never split a statement, and tracks `BEGIN…END` /
/// `CASE…END` nesting so a `;` inside a trigger body or `CASE` expression is not
/// a boundary. A leading `BEGIN` (transaction control) does not open a block —
/// only a mid-statement one (e.g. `CREATE TRIGGER … BEGIN`) does. Comment-only
/// and empty segments are dropped.
fn split_sql_script(sql: &str) -> Vec<&str> {
    let toks = match sql::token::tokenize(sql) {
        Ok(t) => t,
        // Let the caller surface the real parse error on the whole input.
        Err(_) => return alloc::vec![sql.trim()],
    };
    let mut out = Vec::new();
    let mut depth: u32 = 0;
    let mut seg_start = 0usize;
    let mut seen = false;
    for sp in &toks {
        match &sp.token {
            sql::token::Token::Semicolon if depth == 0 => {
                if seen {
                    out.push(sql[seg_start..sp.start].trim());
                }
                seg_start = sp.end;
                seen = false;
            }
            sql::token::Token::Word(w) => {
                match w.to_ascii_uppercase().as_str() {
                    "BEGIN" if seen => depth += 1,
                    "CASE" => depth += 1,
                    "END" => depth = depth.saturating_sub(1),
                    _ => {}
                }
                seen = true;
            }
            _ => seen = true,
        }
    }
    if seen {
        out.push(sql[seg_start..].trim());
    }
    out
}

/// The text stored in `sqlite_master.sql` for a DDL statement: the source from
/// its first real token (skipping leading comments and whitespace) to the trimmed
/// end. SQLite records the schema text from the `CREATE` keyword onward, so an
/// inter-statement `-- comment` preceding the statement is not captured.
fn ddl_text(sql: &str) -> &str {
    match sql::token::tokenize(sql) {
        Ok(toks) if !toks.is_empty() => sql[toks[0].start..].trim_end(),
        _ => sql.trim(),
    }
}

/// Does this stored `CREATE VIEW` body reference table `name`? Used to decide
/// whether an `ALTER TABLE name RENAME TO` must rewrite the view to stay valid —
/// we parse and run the table-rename walker against a sentinel and see if it
/// touched anything, so unrelated views are left byte-for-byte untouched.
/// Whether a `SELECT` references base table `name` anywhere (FROM/joins/
/// subqueries/CTEs/compound) — detected by probe-renaming it to a sentinel and
/// checking the AST changed (reuses `rename_table_in_select`'s full walk).
fn select_reads_table(sel: &Select, name: &str) -> bool {
    let mut probe = sel.clone();
    rename_table_in_select(
        &mut probe,
        name,
        "\u{1}\u{1}graphite_rename_probe\u{1}\u{1}",
    );
    probe != *sel
}

/// Whether a `FROM` clause names table `name` as its first source or a join.
fn from_refs_table(f: &FromClause, name: &str) -> bool {
    f.first.name.eq_ignore_ascii_case(name)
        || f.joins
            .iter()
            .any(|j| j.table.name.eq_ignore_ascii_case(name))
}

/// Whether a `CREATE TRIGGER` references table `name` — either it is attached to
/// it (`ON name`) or a body statement targets/reads it. Used to decide whether a
/// `RENAME TABLE` must rewrite the renamed name inside the trigger's stored text.
fn trigger_uses_table(trigger_sql: &str, name: &str) -> bool {
    let Ok(Statement::CreateTrigger(ct)) = sql::parse_one(trigger_sql) else {
        return false;
    };
    if ct.table.eq_ignore_ascii_case(name) {
        return true;
    }
    ct.body.iter().any(|s| match s {
        Statement::Select(sel) => select_reads_table(sel, name),
        Statement::Insert(i) => {
            i.table.eq_ignore_ascii_case(name)
                || matches!(&i.source, InsertSource::Select(sel) if select_reads_table(sel, name))
        }
        Statement::Update(u) => {
            u.table.eq_ignore_ascii_case(name)
                || u.from.as_ref().is_some_and(|f| from_refs_table(f, name))
        }
        Statement::Delete(d) => d.table.eq_ignore_ascii_case(name),
        _ => false,
    })
}

fn view_uses_table(view_sql: &str, name: &str) -> bool {
    match sql::parse_one(view_sql) {
        Ok(Statement::CreateView(cv)) => {
            let mut probe = cv.select.clone();
            rename_table_in_select(
                &mut probe,
                name,
                "\u{1}\u{1}graphite_rename_probe\u{1}\u{1}",
            );
            *probe != *cv.select
        }
        _ => false,
    }
}

/// Rewrite stored DDL text, repointing every bare or double-quoted identifier
/// token equal to `old` (case-insensitively) to the already-rendered `rendered`
/// text while preserving all other source text — whitespace, comments, and
/// string/blob literals (which tokenize as `Str`/`Blob`, never identifiers, so
/// their contents are never touched). This mirrors SQLite's text-preserving
/// rename rather than reprinting from the AST. `rendered` is the replacement as
/// it should appear (a table rename passes the double-quoted name; a column
/// rename passes the new name bare or quoted exactly as the user wrote it).
/// Rewrite a foreign-key parent-column reference after the parent's column is
/// renamed: in `sql` (another table's `CREATE`), rename `old` → `rendered` but
/// only inside a `REFERENCES <parent>(…)` column list — so a child column that
/// happens to share the old name is left untouched. Used for cross-object
/// `ALTER TABLE … RENAME COLUMN` propagation into foreign keys.
fn rewrite_fk_parent_column(sql: &str, parent: &str, old: &str, rendered: &str) -> String {
    use sql::token::Token;
    let toks = match sql::token::tokenize(sql) {
        Ok(t) => t,
        Err(_) => return String::from(sql),
    };
    let is_word = |t: &Token, w: &str| matches!(t, Token::Word(x) | Token::Ident(x) if x.eq_ignore_ascii_case(w));
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        // `REFERENCES <parent> ( … )` — rename `old` within the column list.
        if is_word(&toks[i].token, "references")
            && toks.get(i + 1).is_some_and(|p| is_word(&p.token, parent))
            && toks
                .get(i + 2)
                .is_some_and(|l| matches!(l.token, Token::LParen))
        {
            let mut m = i + 3;
            while m < toks.len() && !matches!(toks[m].token, Token::RParen) {
                if is_word(&toks[m].token, old) {
                    spans.push((toks[m].start, toks[m].end));
                }
                m += 1;
            }
            i = m;
            continue;
        }
        i += 1;
    }
    if spans.is_empty() {
        return String::from(sql);
    }
    let mut out = String::new();
    let mut cursor = 0;
    for (s, e) in spans {
        out.push_str(&sql[cursor..s]);
        out.push_str(rendered);
        cursor = e;
    }
    out.push_str(&sql[cursor..]);
    out
}

/// For a `CREATE VIEW` whose `SELECT` draws from exactly one source — the
/// renamed `table`, with no joins, subqueries, CTEs, or compound parts — return
/// the qualifiers under which that table's columns can appear (its name plus any
/// alias) so a column rename can be applied by a token rewrite. Returns `None`
/// when a rewrite could be unsafe (multi-source, a subquery that could reach
/// another table, the renamed column's name collides with the table or an alias)
/// — those views are left unchanged, the remaining scope-aware A-rn3 work.
fn view_single_source_column_quals(view_sql: &str, table: &str, old: &str) -> Option<Vec<String>> {
    let Ok(Statement::CreateView(cv)) = sql::parse_one(view_sql) else {
        return None;
    };
    let sel = &cv.select;
    if !sel.ctes.is_empty() || !sel.compound.is_empty() {
        return None;
    }
    // A column named the same as its table would make the table-name token in
    // `FROM <table>` indistinguishable from a column reference — bail.
    if old.eq_ignore_ascii_case(table) {
        return None;
    }
    let from = sel.from.as_ref()?;
    if !from.joins.is_empty() || from.first.subquery.is_some() || from.first.tvf_args.is_some() {
        return None;
    }
    if !from.first.name.eq_ignore_ascii_case(table) {
        return None;
    }
    let mut quals = alloc::vec![table.to_string()];
    if let Some(a) = &from.first.alias {
        if a.eq_ignore_ascii_case(old) {
            return None; // alias collides with the renamed column name
        }
        quals.push(a.clone());
    }
    // Any subquery could reference another table (breaking the single-source
    // guarantee); a result-column alias equal to `old` would be wrongly renamed.
    for rc in &sel.columns {
        if let ResultColumn::Expr { expr, alias, .. } = rc {
            if expr_has_subquery(expr) {
                return None;
            }
            if alias
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(old))
            {
                return None;
            }
        }
    }
    let mut clean = true;
    for e in sel
        .where_clause
        .iter()
        .chain(sel.group_by.iter())
        .chain(sel.having.iter())
    {
        clean &= !expr_has_subquery(e);
    }
    for t in &sel.order_by {
        clean &= !expr_has_subquery(&t.expr);
    }
    if !clean {
        return None;
    }
    Some(quals)
}

/// A-rn3: column-rename rewrite plan for a MULTI-source view (a join of plain
/// base tables). Returns `(quals, rewrite_bare)`: SQLite always renames a
/// `<renamed-table>.old` reference (so `quals` is the renamed table's name +
/// alias), and renames a *bare* `old` only when that column name is unique across
/// all the join's sources (else a bare `old` would be ambiguous — an invalid view
/// anyway). Bails (→ None, leaving the view untouched) on any subquery/CTE/
/// compound, a NATURAL/USING join, a non-base-table source, the renamed table
/// appearing other than exactly once, or a result alias colliding with `old`.
/// `table_cols` maps each base table's name to its column names.
fn view_multi_source_quals(
    view_sql: &str,
    table: &str,
    old: &str,
    table_cols: &alloc::collections::BTreeMap<String, Vec<String>>,
) -> Option<(Vec<String>, bool)> {
    let Ok(Statement::CreateView(cv)) = sql::parse_one(view_sql) else {
        return None;
    };
    let sel = &cv.select;
    if !sel.ctes.is_empty() || !sel.compound.is_empty() || old.eq_ignore_ascii_case(table) {
        return None;
    }
    let from = sel.from.as_ref()?;
    if from.joins.is_empty() {
        return None; // single-source is handled separately
    }
    // Collect every source; each must be a plain base table (no subquery/tvf/
    // schema-qualifier), and a NATURAL/USING join's column coalescing is bailed.
    let mut srcs: Vec<(String, Option<String>)> = Vec::new();
    let mut push = |tr: &crate::sql::ast::TableRef| -> bool {
        if tr.subquery.is_some() || tr.tvf_args.is_some() || tr.schema.is_some() {
            return false;
        }
        srcs.push((tr.name.clone(), tr.alias.clone()));
        true
    };
    if !push(&from.first) {
        return None;
    }
    for j in &from.joins {
        if j.natural || !j.using.is_empty() || !push(&j.table) {
            return None;
        }
    }
    // The renamed table must be a source exactly once; its name+alias qualify it.
    let renamed: Vec<&(String, Option<String>)> = srcs
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(table))
        .collect();
    if renamed.len() != 1 {
        return None;
    }
    let mut quals = alloc::vec![renamed[0].0.clone()];
    if let Some(a) = &renamed[0].1 {
        if a.eq_ignore_ascii_case(old) {
            return None;
        }
        quals.push(a.clone());
    }
    // `old` is safe to rename as a bare reference only if exactly one source has a
    // column of that name. Every source must be a known base table.
    let has_old = |name: &str| -> Option<bool> {
        let cols = table_cols
            .iter()
            .find(|(t, _)| t.eq_ignore_ascii_case(name))
            .map(|(_, c)| c)?;
        Some(cols.iter().any(|c| c.eq_ignore_ascii_case(old)))
    };
    let mut count = 0usize;
    for (n, _) in &srcs {
        if has_old(n)? {
            count += 1;
        }
    }
    let rewrite_bare = count == 1;
    // A subquery anywhere could reach another table (breaking the analysis); a
    // result alias equal to `old` would be wrongly renamed.
    for rc in &sel.columns {
        if let ResultColumn::Expr { expr, alias, .. } = rc {
            if expr_has_subquery(expr)
                || alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(old))
            {
                return None;
            }
        }
    }
    for e in sel
        .where_clause
        .iter()
        .chain(sel.group_by.iter())
        .chain(sel.having.iter())
    {
        if expr_has_subquery(e) {
            return None;
        }
    }
    for t in &sel.order_by {
        if expr_has_subquery(&t.expr) {
            return None;
        }
    }
    Some((quals, rewrite_bare))
}

/// Whether a `SELECT` references at most the single source `table` (its `FROM`,
/// if any, is exactly `table` with no alias, joins, subquery source, CTEs,
/// compound parts, or any subquery expression). Conservative: a `false` result
/// just means "don't token-rewrite", never corruption.
fn select_single_source_ok(sel: &Select, table: &str) -> bool {
    if !sel.ctes.is_empty() || !sel.compound.is_empty() {
        return false;
    }
    if let Some(from) = &sel.from {
        if !from.joins.is_empty()
            || from.first.subquery.is_some()
            || from.first.tvf_args.is_some()
            || from.first.alias.is_some()
            || !from.first.name.eq_ignore_ascii_case(table)
        {
            return false;
        }
    }
    let mut ok = true;
    for rc in &sel.columns {
        if let ResultColumn::Expr { expr, alias, .. } = rc {
            ok &= !expr_has_subquery(expr) && alias.is_none();
        }
    }
    for e in sel
        .where_clause
        .iter()
        .chain(sel.group_by.iter())
        .chain(sel.having.iter())
    {
        ok &= !expr_has_subquery(e);
    }
    for t in &sel.order_by {
        ok &= !expr_has_subquery(&t.expr);
    }
    ok
}

/// For a `CREATE TRIGGER` ON the renamed `table` whose body and `WHEN` reference
/// ONLY that table (every body statement targets `table`, draws from at most
/// `table`, and contains no subquery), return the qualifiers under which the
/// renamed column can appear (`table`, `NEW`, `OLD`) so a column rename can be
/// token-rewritten. Returns `None` (leave the trigger unchanged) on anything
/// outside this provably-safe shape — the multi-table / scope-aware remainder.
fn trigger_single_source_quals(trigger_sql: &str, table: &str, old: &str) -> Option<Vec<String>> {
    let Ok(Statement::CreateTrigger(ct)) = sql::parse_one(trigger_sql) else {
        return None;
    };
    // Only triggers attached to the renamed table (so NEW/OLD are its rows). A
    // column named like the table or like the NEW/OLD aliases is ambiguous.
    if !ct.table.eq_ignore_ascii_case(table)
        || old.eq_ignore_ascii_case(table)
        || old.eq_ignore_ascii_case("new")
        || old.eq_ignore_ascii_case("old")
    {
        return None;
    }
    if ct.when.as_ref().is_some_and(expr_has_subquery) {
        return None;
    }
    // A subquery anywhere could reach another table, breaking the single-source
    // guarantee; `expr_has_subquery` is a plain fn so it passes by value freely.
    for stmt in &ct.body {
        let safe = match stmt {
            Statement::Select(sel) => select_single_source_ok(sel, table),
            Statement::Insert(i) => {
                i.schema.is_none()
                    && i.returning.is_empty()
                    && i.upsert.is_empty()
                    && i.table.eq_ignore_ascii_case(table)
                    && match &i.source {
                        InsertSource::DefaultValues => true,
                        InsertSource::Values(rows) => {
                            !rows.iter().any(|r| r.iter().any(expr_has_subquery))
                        }
                        InsertSource::Select(sel) => select_single_source_ok(sel, table),
                    }
            }
            Statement::Update(u) => {
                u.schema.is_none()
                    && u.from.is_none()
                    && u.returning.is_empty()
                    && u.table.eq_ignore_ascii_case(table)
                    && u.row_assignments.is_empty()
                    && !u.assignments.iter().any(|(_, e)| expr_has_subquery(e))
                    && !u.where_clause.as_ref().is_some_and(expr_has_subquery)
                    && !u.order_by.iter().any(|t| expr_has_subquery(&t.expr))
                    && !u.limit.as_ref().is_some_and(expr_has_subquery)
                    && !u.offset.as_ref().is_some_and(expr_has_subquery)
            }
            Statement::Delete(d) => {
                d.schema.is_none()
                    && d.returning.is_empty()
                    && d.table.eq_ignore_ascii_case(table)
                    && !d.where_clause.as_ref().is_some_and(expr_has_subquery)
                    && !d.order_by.iter().any(|t| expr_has_subquery(&t.expr))
                    && !d.limit.as_ref().is_some_and(expr_has_subquery)
                    && !d.offset.as_ref().is_some_and(expr_has_subquery)
            }
            _ => false,
        };
        if !safe {
            return None;
        }
    }
    Some(alloc::vec![
        table.to_string(),
        String::from("NEW"),
        String::from("OLD"),
    ])
}

/// Whether `trigger_sql`'s body+WHEN reference `table` as their ONLY base table
/// (every body statement targets/reads just `table`, no other table, no
/// subquery, no alias/CTE/compound) — regardless of which table the trigger is
/// attached to. When true, every bare and `table.`-qualified column reference in
/// the body binds to `table`, so a rename can be token-rewritten safely. Used for
/// a trigger on ANOTHER table whose body reads/writes the renamed table (the
/// cross-object case `trigger_single_source_quals` does not cover, since that
/// one also rewrites `NEW`/`OLD`, which here belong to the trigger's own table).
/// Conservative: any construct it cannot prove single-source makes it `false`.
fn trigger_body_single_source_over(trigger_sql: &str, table: &str, old: &str) -> bool {
    let Ok(Statement::CreateTrigger(ct)) = sql::parse_one(trigger_sql) else {
        return false;
    };
    // `old` colliding with NEW/OLD would make a bare-vs-pseudo-column ambiguous.
    if old.eq_ignore_ascii_case("new") || old.eq_ignore_ascii_case("old") {
        return false;
    }
    if ct.when.as_ref().is_some_and(expr_has_subquery) {
        return false;
    }
    for stmt in &ct.body {
        let safe = match stmt {
            Statement::Select(sel) => select_single_source_ok(sel, table),
            Statement::Insert(i) => {
                i.schema.is_none()
                    && i.returning.is_empty()
                    && i.upsert.is_empty()
                    && i.table.eq_ignore_ascii_case(table)
                    && match &i.source {
                        InsertSource::DefaultValues => true,
                        InsertSource::Values(rows) => {
                            !rows.iter().any(|r| r.iter().any(expr_has_subquery))
                        }
                        InsertSource::Select(sel) => select_single_source_ok(sel, table),
                    }
            }
            Statement::Update(u) => {
                u.schema.is_none()
                    && u.from.is_none()
                    && u.returning.is_empty()
                    && u.table.eq_ignore_ascii_case(table)
                    && u.row_assignments.is_empty()
                    && !u.assignments.iter().any(|(_, e)| expr_has_subquery(e))
                    && !u.where_clause.as_ref().is_some_and(expr_has_subquery)
            }
            Statement::Delete(d) => {
                d.schema.is_none()
                    && d.returning.is_empty()
                    && d.table.eq_ignore_ascii_case(table)
                    && !d.where_clause.as_ref().is_some_and(expr_has_subquery)
            }
            _ => false,
        };
        if !safe {
            return false;
        }
    }
    // Require at least one statement (an empty body has nothing to rewrite).
    !ct.body.is_empty()
}

/// Whether `trigger_sql` is a trigger attached to `table`, with `old` not an
/// ambiguous name (the table itself or the `NEW`/`OLD` aliases). When true, the
/// trigger's `NEW.old` / `OLD.old` references unambiguously bind to `table`'s
/// renamed column — safe to rewrite even when the body touches other tables
/// (unlike [`trigger_single_source_quals`], which also needs bare refs to resolve).
fn trigger_on_renamed_table(trigger_sql: &str, table: &str, old: &str) -> bool {
    matches!(sql::parse_one(trigger_sql), Ok(Statement::CreateTrigger(ct))
        if ct.table.eq_ignore_ascii_case(table)
            && !old.eq_ignore_ascii_case(table)
            && !old.eq_ignore_ascii_case("new")
            && !old.eq_ignore_ascii_case("old"))
}

/// Token-rewrite a column rename in DDL where every reference to `old` is known
/// to belong to one of `quals` (a single-source object's table name / aliases):
/// rename a qualified `<q>.old` whose qualifier `q` is in `quals`, preserving all
/// other text. When `rewrite_bare` is true, an unqualified `old` ident is also
/// renamed (safe only when every bare reference provably resolves to the renamed
/// table — i.e. a single-source object); when false, only qualified references
/// are touched (e.g. a multi-source trigger where only `NEW.old`/`OLD.old` are
/// provably the renamed column). A function name (`old(`) and a column tail
/// qualified by anything else are left intact.
fn rewrite_column_tokens(
    sql: &str,
    quals: &[String],
    old: &str,
    rendered: &str,
    rewrite_bare: bool,
) -> String {
    use sql::token::Token;
    let toks = match sql::token::tokenize(sql) {
        Ok(t) => t,
        Err(_) => return String::from(sql),
    };
    let mut out = String::new();
    let mut cursor = 0usize;
    for (i, sp) in toks.iter().enumerate() {
        let hit =
            matches!(&sp.token, Token::Word(w) | Token::Ident(w) if w.eq_ignore_ascii_case(old));
        if !hit {
            continue;
        }
        // A function name (`old(`) is never a column reference.
        if toks
            .get(i + 1)
            .is_some_and(|n| matches!(n.token, Token::LParen))
        {
            continue;
        }
        let after_dot = i > 0 && matches!(toks[i - 1].token, Token::Dot);
        if after_dot {
            // Rename only `<qualifier>.old` where the qualifier is the table or an
            // alias; leave any other `x.old` untouched.
            let qual_ok = i >= 2
                && matches!(&toks[i - 2].token, Token::Word(q) | Token::Ident(q)
                    if quals.iter().any(|t| t.eq_ignore_ascii_case(q)));
            if !qual_ok {
                continue;
            }
        } else if !rewrite_bare {
            // A bare reference is only provably the renamed column in a
            // single-source context; skip it otherwise.
            continue;
        }
        out.push_str(&sql[cursor..sp.start]);
        out.push_str(rendered);
        cursor = sp.end;
    }
    out.push_str(&sql[cursor..]);
    out
}

fn rewrite_ident_tokens(sql: &str, old: &str, rendered: &str) -> String {
    let toks = match sql::token::tokenize(sql) {
        Ok(t) => t,
        Err(_) => return String::from(sql),
    };
    let mut out = String::new();
    let mut cursor = 0usize;
    for (i, sp) in toks.iter().enumerate() {
        let hit = matches!(
            &sp.token,
            sql::token::Token::Word(w) | sql::token::Token::Ident(w) if w.eq_ignore_ascii_case(old)
        );
        if !hit {
            continue;
        }
        // A token equal to the table name is only a *table reference* worth
        // renaming when it is neither a column-name tail (`x.old`) nor a function
        // name (`old(`). Skipping those keeps a like-named column or function
        // (e.g. a table named `count` vs the `count()` function) intact.
        let after_dot = i > 0 && matches!(toks[i - 1].token, sql::token::Token::Dot);
        let before_lparen = toks
            .get(i + 1)
            .is_some_and(|n| matches!(n.token, sql::token::Token::LParen));
        if after_dot || before_lparen {
            continue;
        }
        out.push_str(&sql[cursor..sp.start]);
        out.push_str(rendered);
        cursor = sp.end;
    }
    out.push_str(&sql[cursor..]);
    out
}

/// Replace the table-name token that follows the `anchor` keyword (`TABLE` for a
/// `CREATE TABLE`, `ON` for a `CREATE INDEX`) with `new` (double-quoted, as
/// SQLite does), preserving the rest of the text verbatim — so a `RENAME TO`
/// keeps the original formatting rather than reprinting from the AST. Returns the
/// input unchanged if the name token can't be located.
fn rename_table_token_after(sql: &str, anchor: &str, new: &str) -> String {
    use sql::token::Token;
    let toks = match sql::token::tokenize(sql) {
        Ok(t) => t,
        Err(_) => return String::from(sql),
    };
    let kw = |t: &Token, k: &str| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case(k));
    let mut i = 0;
    while i < toks.len() && !kw(&toks[i].token, anchor) {
        i += 1;
    }
    i += 1;
    // Optional `IF NOT EXISTS` (only after TABLE).
    if i + 2 < toks.len()
        && kw(&toks[i].token, "if")
        && kw(&toks[i + 1].token, "not")
        && kw(&toks[i + 2].token, "exists")
    {
        i += 3;
    }
    // Optional `schema.` qualifier before the table name.
    if i + 1 < toks.len() && matches!(toks[i + 1].token, Token::Dot) {
        i += 2;
    }
    let Some(sp) = toks.get(i) else {
        return String::from(sql);
    };
    let mut out = String::with_capacity(sql.len() + new.len());
    out.push_str(&sql[..sp.start]);
    out.push_str(&sql::print::ident(new));
    out.push_str(&sql[sp.end..]);
    out
}

/// Rewrite the target of every `REFERENCES <old>` clause in a `CREATE TABLE`
/// text to `new` (double-quoted), preserving the rest verbatim — so an
/// `ALTER TABLE … RENAME TO` updates the foreign keys of OTHER tables (and any
/// self-reference) that point at the renamed table, as SQLite does. Only the
/// table-name token immediately after `REFERENCES` is touched, so references to
/// other tables — and a column that happens to share the old name — are left
/// intact. (SQLite forbids a schema qualifier after `REFERENCES`, so the target
/// is always a single bare/quoted name.)
fn rewrite_fk_references(sql: &str, old: &str, new: &str) -> String {
    use sql::token::Token;
    let toks = match sql::token::tokenize(sql) {
        Ok(t) => t,
        Err(_) => return String::from(sql),
    };
    let mut out = String::new();
    let mut cursor = 0usize;
    for (i, sp) in toks.iter().enumerate() {
        if !matches!(&sp.token, Token::Word(w) if w.eq_ignore_ascii_case("references")) {
            continue;
        }
        let Some(target) = toks.get(i + 1) else {
            continue;
        };
        if matches!(&target.token, Token::Word(w) | Token::Ident(w) if w.eq_ignore_ascii_case(old))
        {
            out.push_str(&sql[cursor..target.start]);
            out.push_str(&sql::print::ident(new));
            cursor = target.end;
        }
    }
    out.push_str(&sql[cursor..]);
    out
}

/// Insert `, <col_text>` before the column-list's closing paren of a `CREATE
/// TABLE` statement's text, preserving everything else verbatim — how SQLite
/// records an `ADD COLUMN`. Returns `None` if the column list can't be located.
fn append_column_to_create(sql: &str, col_text: &str) -> Option<String> {
    use sql::token::Token;
    let toks = sql::token::tokenize(sql).ok()?;
    let open = toks.iter().position(|t| matches!(t.token, Token::LParen))?;
    let mut depth = 0i32;
    let mut close = None;
    for (i, sp) in toks.iter().enumerate().skip(open) {
        match sp.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let pos = toks[close?].start;
    let mut out = String::with_capacity(sql.len() + col_text.len() + 2);
    out.push_str(&sql[..pos]);
    out.push_str(", ");
    out.push_str(col_text.trim());
    out.push_str(&sql[pos..]);
    Some(out)
}

/// Remove the column named `col` (and one adjacent comma) from a `CREATE TABLE`
/// statement's text, preserving everything else verbatim — how SQLite records a
/// `DROP COLUMN`. Returns `None` if the column or list can't be located.
fn drop_column_from_create(sql: &str, col: &str) -> Option<String> {
    use sql::token::Token;
    let toks = sql::token::tokenize(sql).ok()?;
    let open = toks.iter().position(|t| matches!(t.token, Token::LParen))?;
    // The matching close of the column list, and the top-level comma separators.
    let mut depth = 0i32;
    let mut close = None;
    let mut seps = Vec::new();
    for (i, sp) in toks.iter().enumerate().skip(open) {
        match sp.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            Token::Comma if depth == 1 => seps.push(i),
            _ => {}
        }
    }
    let close = close?;
    // Segment boundaries: the opener, each top-level comma, then the closer. The
    // first token after each boundary begins a column def or table constraint.
    let mut bounds = alloc::vec![open];
    bounds.extend_from_slice(&seps);
    bounds.push(close);
    let n = bounds.len() - 1; // number of segments
    let is_named = |i: usize| {
        matches!(&toks.get(i).map(|t| &t.token),
            Some(Token::Word(w) | Token::Ident(w)) if w.eq_ignore_ascii_case(col))
    };
    let j = (0..n).find(|&j| bounds[j] + 1 < bounds[j + 1] && is_named(bounds[j] + 1))?;
    let (del_start, del_end) = if j < n - 1 {
        // Not the last segment: drop it and the comma that follows.
        (toks[bounds[j] + 1].start, toks[bounds[j + 1] + 1].start)
    } else {
        // The last segment: drop the comma that precedes it through its last token.
        (toks[bounds[j]].start, toks[close - 1].end)
    };
    let mut out = String::with_capacity(sql.len());
    out.push_str(&sql[..del_start]);
    out.push_str(&sql[del_end..]);
    Some(out)
}

/// Best-effort label for an unaliased result expression.
fn expr_label(expr: &Expr) -> String {
    match expr {
        Expr::Column { column, .. } => column.clone(),
        Expr::Literal(Literal::Integer(i)) => i.to_string(),
        Expr::Literal(Literal::Str(s)) => s.clone(),
        Expr::Function { name, .. } => name.clone(),
        Expr::Paren(e) => expr_label(e),
        _ => "expr".to_string(),
    }
}

/// The name of a result column, matching SQLite: an `AS` alias wins; a bare
/// column reference uses the column name; any other expression is named after
/// its verbatim source span (`SELECT a+b` → `a+b`), falling back to
/// [`expr_label`] when no span was captured (synthetic columns).
fn result_column_label(expr: &Expr, alias: &Option<String>, source: &Option<String>) -> String {
    if let Some(a) = alias {
        return a.clone();
    }
    match expr {
        Expr::Column { column, .. } => column.clone(),
        _ => source.clone().unwrap_or_else(|| expr_label(expr)),
    }
}

/// Detect an `INTEGER PRIMARY KEY` rowid alias column (must be declared exactly
/// `INTEGER`, per SQLite — `INT PRIMARY KEY` does not alias the rowid).
/// The collating sequences for a `WITHOUT ROWID` table's stored columns, in
/// on-disk (PK-first) order — used to order its clustered b-tree.
fn wr_storage_collations(meta: &TableMeta) -> Vec<crate::value::Collation> {
    meta.storage_order
        .iter()
        .map(|&c| meta.columns[c].collation)
        .collect()
}

/// The declared collating sequence of a column (`COLLATE name`), `BINARY` if
/// none or unrecognized.
fn column_collation(col: &ColumnDef) -> crate::value::Collation {
    col.constraints
        .iter()
        .find_map(|c| match c {
            ColumnConstraint::Collate(name) => crate::value::Collation::parse(name),
            _ => None,
        })
        .unwrap_or_default()
}

/// The UNIQUE / non-rowid PRIMARY KEY column-index sets of a table, in
/// declaration order (column-level constraints first, in column order, then
/// table-level constraints). This is exactly the order SQLite numbers its
/// `sqlite_autoindex_<table>_<n>` automatic indexes.
fn collect_unique_sets(ct: &CreateTable, ipk: Option<usize>) -> Vec<(Vec<usize>, OnConflict)> {
    let col_pos = |name: &str| {
        ct.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    };
    // Each unique set carries its declared `ON CONFLICT` action (default `Abort`),
    // applied when an INSERT/UPDATE without its own `OR <action>` violates it.
    let mut unique: Vec<(Vec<usize>, OnConflict)> = Vec::new();
    for (i, c) in ct.columns.iter().enumerate() {
        for k in &c.constraints {
            match k {
                ColumnConstraint::Unique(oc) => unique.push((alloc::vec![i], *oc)),
                ColumnConstraint::PrimaryKey { on_conflict, .. } if Some(i) != ipk => {
                    unique.push((alloc::vec![i], *on_conflict))
                }
                _ => {}
            }
        }
    }
    for tc in &ct.constraints {
        let (names, oc) = match tc {
            TableConstraint::Unique(n, oc) | TableConstraint::PrimaryKey(n, oc) => (n, *oc),
            _ => continue,
        };
        let idxs: Option<Vec<usize>> = names.iter().map(|n| col_pos(n)).collect();
        if let Some(set) = idxs {
            // Skip a single-column PK that is the rowid alias.
            if !(set.len() == 1 && Some(set[0]) == ipk) {
                unique.push((set, oc));
            }
        }
    }
    unique
}

/// Convert a `WITHOUT ROWID` row from declared column order to on-disk storage
/// order (PK columns first, then the rest).
fn permute_row(meta: &TableMeta, declared: &[Value]) -> Vec<Value> {
    meta.storage_order
        .iter()
        .map(|&i| declared[i].clone())
        .collect()
}

/// The auto-vacuum mode recorded in a database header: 0 = NONE, 1 = FULL,
/// 2 = INCREMENTAL. Auto-vacuum is on iff the largest-root-page field is
/// non-zero; the incremental-vacuum flag then selects the mode.
fn auto_vacuum_mode(header: &crate::format::DatabaseHeader) -> u32 {
    if header.largest_root_page == 0 {
        0
    } else if header.incremental_vacuum == 0 {
        1
    } else {
        2
    }
}

/// Remove an explicit `schema.` qualifier from a qualified `CREATE` statement's
/// text so the SQL stored in the target catalog is bare-named (the `schema.`
/// prefix is invalid in that database's own namespace, and sqlite3 rejects it).
///
/// In an *explicitly* qualified CREATE the first `.` token is the object-name
/// qualifier (only keywords precede the name). `schema` is the resolved
/// qualifier; when it came from the `TEMP` keyword rather than the text (so the
/// first `.` is something else, e.g. `NEW.col` in a trigger body) the leading
/// identifier won't match and the text is returned unchanged.
fn strip_schema_qualifier(sql: &str, schema: &str) -> Result<String> {
    use crate::sql::token::Token;
    let toks = crate::sql::token::tokenize(sql)?;
    for (i, t) in toks.iter().enumerate() {
        if i == 0 || !matches!(t.token, Token::Dot) {
            continue;
        }
        let lead = match &toks[i - 1].token {
            Token::Word(s) | Token::Ident(s) => Some(s.as_str()),
            _ => None,
        };
        if lead.is_some_and(|s| s.eq_ignore_ascii_case(schema)) {
            let schema_start = toks[i - 1].start;
            let name_start = toks.get(i + 1).map_or(sql.len(), |s| s.start);
            let mut out = String::with_capacity(sql.len());
            out.push_str(&sql[..schema_start]);
            out.push_str(&sql[name_start..]);
            return Ok(out);
        }
        // The first `.` is not the object qualifier — nothing to strip.
        break;
    }
    Ok(sql.into())
}

/// The inverse of [`permute_row`]: storage order back to declared column order.
fn unpermute_row(meta: &TableMeta, storage: Vec<Value>) -> Vec<Value> {
    let mut row = alloc::vec![Value::Null; meta.columns.len()];
    for (k, &col) in meta.storage_order.iter().enumerate() {
        if let Some(v) = storage.get(k) {
            row[col] = v.clone();
        }
    }
    row
}

/// The column positions of a table's PRIMARY KEY, in key order (column-level
/// `PRIMARY KEY` or a table-level `PRIMARY KEY(...)`). Empty if none.
fn primary_key_positions(ct: &CreateTable) -> Vec<usize> {
    for (i, c) in ct.columns.iter().enumerate() {
        if c.constraints
            .iter()
            .any(|k| matches!(k, ColumnConstraint::PrimaryKey { .. }))
        {
            return alloc::vec![i];
        }
    }
    for tc in &ct.constraints {
        if let TableConstraint::PrimaryKey(names, _) = tc {
            let pos: Option<Vec<usize>> = names
                .iter()
                .map(|n| {
                    ct.columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(n))
                })
                .collect();
            if let Some(pos) = pos {
                return pos;
            }
        }
    }
    Vec::new()
}

/// Parse the `<n>` from `sqlite_autoindex_<table>_<n>` (1-based), if `name` is an
/// automatic index for `table`.
fn autoindex_number(name: &str, table: &str) -> Option<usize> {
    let prefix = alloc::format!("sqlite_autoindex_{table}_");
    name.strip_prefix(&prefix)?.parse::<usize>().ok()
}

fn find_integer_primary_key(ct: &CreateTable) -> Option<usize> {
    for (i, c) in ct.columns.iter().enumerate() {
        let is_integer = c
            .type_name
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("integer"));
        // A column-level `INTEGER PRIMARY KEY` is the rowid alias — EXCEPT when it
        // carries the `DESC` keyword, which sqlite treats as an ordinary table
        // (the column gets its own index and the rowid is auto-assigned). `ASC`
        // and the table-level `PRIMARY KEY(col)` form remain aliases.
        let is_pk_alias = c.constraints.iter().any(|k| {
            matches!(
                k,
                ColumnConstraint::PrimaryKey {
                    descending: false,
                    ..
                }
            )
        });
        if is_integer && is_pk_alias {
            return Some(i);
        }
    }
    // Table-level single-column PRIMARY KEY over an INTEGER column.
    for tc in &ct.constraints {
        if let TableConstraint::PrimaryKey(cols, _) = tc {
            if cols.len() == 1 {
                if let Some(i) = ct.columns.iter().position(|c| c.name == cols[0]) {
                    if ct.columns[i]
                        .type_name
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case("integer"))
                    {
                        return Some(i);
                    }
                }
            }
        }
    }
    None
}
