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
mod window;

use crate::btree::{
    clear_index, create_index_root, create_table_root, delete_table, free_tree, insert_index,
    insert_table, IndexCursor, TableCursor,
};
use crate::error::{Error, Result};
use crate::format::record::{decode_record, encode_record};
use crate::pager::{PageSource, WritePager};
use crate::schema::Schema;
use crate::sql::ast::*;
use crate::sql::{self};
use crate::value::Value;
use crate::vfs::{OpenFlags, Vfs};
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
    Write(WritePager),
    Read(Box<dyn PageSource>),
}

impl Backend {
    fn source(&self) -> &dyn PageSource {
        match self {
            Backend::Write(w) => w,
            Backend::Read(r) => r.as_ref(),
        }
    }
    fn writer(&mut self) -> Result<&mut WritePager> {
        match self {
            Backend::Write(w) => Ok(w),
            Backend::Read(_) => Err(Error::Error("database is read-only".into())),
        }
    }
}

/// A database connection. Supports reading (`query`) and writing (`execute`),
/// over a file or in memory.
pub struct Connection {
    backend: Backend,
    schema: Schema,
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
    /// Whether triggers may fire other triggers (`PRAGMA recursive_triggers`).
    /// Off by default, matching SQLite: triggers then fire only at the top level.
    recursive_triggers: bool,
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
        let backend = Backend::Write(db);
        let schema = Schema::read(backend.source())?;
        Ok(Connection {
            backend,
            schema,
            in_tx: false,
            cte_env: core::cell::RefCell::new(Vec::new()),
            outer_scope: core::cell::RefCell::new(Vec::new()),
            foreign_keys: false,
            trigger_depth: core::cell::Cell::new(0),
            recursive_triggers: false,
        })
    }

    fn from_read_backend(backend: Box<dyn PageSource>) -> Result<Connection> {
        let backend = Backend::Read(backend);
        let schema = Schema::read(backend.source())?;
        Ok(Connection {
            backend,
            schema,
            in_tx: false,
            cte_env: core::cell::RefCell::new(Vec::new()),
            outer_scope: core::cell::RefCell::new(Vec::new()),
            foreign_keys: false,
            trigger_depth: core::cell::Cell::new(0),
            recursive_triggers: false,
        })
    }

    /// Open an existing database for reading and writing through `vfs`. Creates
    /// (and recovers from) a `<path>-journal` companion file.
    pub fn open_vfs(vfs: &dyn Vfs, path: &str) -> Result<Connection> {
        let main = vfs.open(path, OpenFlags::READ_WRITE)?;
        let journal = vfs.open(&journal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        Connection::from_pager(WritePager::open(main, Some(journal))?)
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
            return Connection::from_read_backend(Box::new(reader));
        }
        Connection::from_read_backend(Box::new(WritePager::open(main, None)?))
    }

    /// Create a new, empty database through `vfs`.
    pub fn create_vfs(vfs: &dyn Vfs, path: &str, page_size: u32) -> Result<Connection> {
        let main = vfs.open(path, OpenFlags::READ_WRITE_CREATE)?;
        let journal = vfs.open(&journal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        let mut db = WritePager::create(main, Some(journal), page_size)?;
        db.commit()?;
        Connection::from_pager(db)
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

    /// Like [`query`](Self::query) but with bound parameters.
    pub fn query_params(&self, sql: &str, params: &Params) -> Result<QueryResult> {
        match sql::parse_one(sql)? {
            Statement::Select(sel) => self.run_select(&sel, params),
            Statement::Pragma(p) => self.run_pragma(&p),
            Statement::Explain { query_plan, stmt } => {
                if !query_plan {
                    return Err(Error::Unsupported(
                        "plain EXPLAIN (VDBE bytecode); use EXPLAIN QUERY PLAN",
                    ));
                }
                self.explain_query_plan(&stmt, params)
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
                "user_version",
                Value::Integer(header.user_version as i64),
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
            "table_info" => self.pragma_table_info(p),
            "foreign_keys" => Ok(single(
                "foreign_keys",
                Value::Integer(self.foreign_keys as i64),
            )),
            "recursive_triggers" => Ok(single(
                "recursive_triggers",
                Value::Integer(self.recursive_triggers as i64),
            )),
            _ => Err(Error::Unsupported("this PRAGMA")),
        }
    }

    /// `PRAGMA table_info(name)` → one row per column
    /// `(cid, name, type, notnull, dflt_value, pk)`.
    fn pragma_table_info(&self, p: &Pragma) -> Result<QueryResult> {
        let table = match &p.value {
            Some(Expr::Column { column, .. }) => column.clone(),
            Some(Expr::Literal(Literal::Str(s))) => s.clone(),
            _ => {
                return Err(Error::Error(
                    "PRAGMA table_info requires a table name".into(),
                ))
            }
        };
        let obj = self
            .schema
            .table(&table)
            .ok_or_else(|| Error::Error(format!("no such table: {table}")))?;
        let sql = obj.sql.as_deref().unwrap_or("");
        let Statement::CreateTable(ct) = sql::parse_one(sql)? else {
            return Err(Error::Corrupt("schema sql is not CREATE TABLE".into()));
        };
        let ipk = find_integer_primary_key(&ct);

        let mut rows = Vec::new();
        for (i, col) in ct.columns.iter().enumerate() {
            let notnull = col
                .constraints
                .iter()
                .any(|c| matches!(c, ColumnConstraint::NotNull))
                || Some(i) == ipk;
            let dflt = col.constraints.iter().find_map(|c| match c {
                ColumnConstraint::Default(Expr::Literal(l)) => Some(literal_text(l)),
                _ => None,
            });
            let pk = if Some(i) == ipk
                || col
                    .constraints
                    .iter()
                    .any(|c| matches!(c, ColumnConstraint::PrimaryKey { .. }))
            {
                1
            } else {
                0
            };
            rows.push(alloc::vec![
                Value::Integer(i as i64),
                Value::Text(col.name.clone()),
                Value::Text(col.type_name.clone().unwrap_or_default()),
                Value::Integer(notnull as i64),
                dflt.map(Value::Text).unwrap_or(Value::Null),
                Value::Integer(pk),
            ]);
        }
        Ok(QueryResult {
            columns: ["cid", "name", "type", "notnull", "dflt_value", "pk"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
            rows,
        })
    }

    /// Execute a single non-`SELECT` statement, returning the number of rows
    /// affected (0 for DDL and transaction control).
    pub fn execute(&mut self, sql: &str) -> Result<usize> {
        self.execute_params(sql, &Params::default())
    }

    /// Like [`execute`](Self::execute) but with bound parameters.
    pub fn execute_params(&mut self, sql: &str, params: &Params) -> Result<usize> {
        let stmt = sql::parse_one(sql)?;
        // Transaction control is handled directly (no autocommit around it).
        match &stmt {
            Statement::Begin => {
                self.in_tx = true;
                return Ok(0);
            }
            Statement::Commit => {
                self.backend.writer()?.commit()?;
                self.in_tx = false;
                return Ok(0);
            }
            Statement::Rollback => {
                self.backend.writer()?.rollback();
                self.in_tx = false;
                self.schema = Schema::read(self.backend.source())?;
                return Ok(0);
            }
            _ => {}
        }

        let affected = match stmt {
            Statement::CreateTable(ct) => {
                self.exec_create_table(&ct, sql.trim())?;
                0
            }
            Statement::Insert(ins) => self.exec_insert(&ins, params)?,
            Statement::Delete(del) => self.exec_delete(&del, params)?,
            Statement::Update(upd) => self.exec_update(&upd, params)?,
            Statement::CreateIndex(ci) => {
                self.exec_create_index(&ci, sql.trim())?;
                0
            }
            Statement::CreateView(cv) => {
                self.exec_create_view(&cv, sql.trim())?;
                0
            }
            Statement::CreateTrigger(ct) => {
                self.exec_create_trigger(&ct, sql.trim())?;
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
            Statement::Vacuum => 0, // accepted; compaction is a no-op here
            Statement::Select(_) => return Err(Error::Unsupported("use query() for SELECT")),
            Statement::Explain { .. } => return Err(Error::Unsupported("use query() for EXPLAIN")),
            Statement::Begin | Statement::Commit | Statement::Rollback => unreachable!(),
        };

        if !self.in_tx {
            self.backend.writer()?.commit()?;
            // Refresh the catalog from the committed image.
            self.schema = Schema::read(self.backend.source())?;
        }
        Ok(affected)
    }

    // ---- DDL / DML ----------------------------------------------------------

    fn exec_create_table(&mut self, ct: &CreateTable, sql_text: &str) -> Result<()> {
        if self.schema.table(&ct.name).is_some() {
            if ct.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("table {} already exists", ct.name)));
        }
        // A WITHOUT ROWID table is stored as a PK-clustered index b-tree; an
        // ordinary table uses a rowid table b-tree.
        let root = if ct.without_rowid {
            // Validate the supported subset early (PK present, no extra UNIQUE).
            let pk = primary_key_positions(ct);
            if pk.is_empty() {
                return Err(Error::Error(
                    "WITHOUT ROWID table must have a PRIMARY KEY".into(),
                ));
            }
            if collect_unique_sets(ct, None).len() > 1 {
                return Err(Error::Unsupported(
                    "WITHOUT ROWID with additional UNIQUE constraints",
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
        // A WITHOUT ROWID table's PK is the table itself, so it needs none.
        let ipk = find_integer_primary_key(ct);
        let unique = if ct.without_rowid {
            Vec::new()
        } else {
            collect_unique_sets(ct, ipk)
        };
        for (n, schema_rowid) in (next + 1..).enumerate().take(unique.len()) {
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
            if let TableConstraint::PrimaryKey(cols) = c {
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
        // The target may be a table or (for INSTEAD OF triggers) a view.
        if self.schema.table(&ct.table).is_none() && !self.is_view(&ct.table) {
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
        for obj in self.schema.objects() {
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
        Ok(out)
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
        let trigs = self.triggers_for(table, kind, timing)?;
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
            })
            .collect();
        self.outer_scope.borrow_mut().push(OuterFrame {
            columns,
            row: values.to_vec(),
            rowid: Some(rowid),
        });
    }

    /// Whether `name` is a view.
    fn is_view(&self, name: &str) -> bool {
        self.schema.objects().iter().any(|o| {
            o.obj_type == crate::schema::ObjectType::View && o.name.eq_ignore_ascii_case(name)
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
            )?;
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
            )?;
            affected += 1;
        }
        Ok(affected)
    }

    /// `UPDATE` a view: fire `INSTEAD OF UPDATE` triggers with OLD/NEW for each
    /// selected row.
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
                let ctx = row_ctx(&new, &cols, None, params).with_subqueries(self);
                new[pos] = eval::eval(expr, &ctx)?;
            }
            self.fire_triggers(
                &upd.table,
                TrigEvent::Update,
                TriggerTiming::InsteadOf,
                &cols,
                Some((&old, 0)),
                Some((&new, 0)),
                params,
            )?;
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
                    Statement::Select(_) => {} // side-effect free in our engine
                    _ => return Err(Error::Unsupported("statement type in trigger body")),
                }
            }
        }
        Ok(())
    }

    fn exec_insert(&mut self, ins: &Insert, params: &Params) -> Result<usize> {
        let rows = match &ins.source {
            InsertSource::Values(rows) => rows.clone(),
            InsertSource::DefaultValues => alloc::vec![Vec::new()],
            InsertSource::Select(_) => return Err(Error::Unsupported("INSERT ... SELECT")),
        };
        if self.is_view(&ins.table) {
            return self.exec_view_insert(ins, &rows, params);
        }
        let meta = self.table_meta(&ins.table, None)?;
        if meta.without_rowid {
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
        let mut affected = 0;
        let mut replaced = false;
        for row_exprs in &rows {
            if !ins.columns.is_empty() && row_exprs.len() != target.len() {
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
                values[target[i]] = eval::eval(e, &ctx)?;
            }
            apply_column_affinity(&meta, &mut values);

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
            check_not_null(&meta, &values)?;
            self.check_constraints(&meta, &values, Some(rowid), params)?;
            self.check_fk_child(&ins.table, &meta, &values)?;

            // Resolve UNIQUE / PRIMARY KEY (incl. rowid) conflicts.
            let conflicts = self.find_conflicts(&meta, rowid, &values, None)?;
            if !conflicts.is_empty() {
                match ins.on_conflict {
                    OnConflict::Abort => {
                        return Err(Error::Constraint("UNIQUE constraint failed".into()))
                    }
                    OnConflict::Ignore => continue, // skip this row
                    OnConflict::Replace => {
                        for cr in conflicts {
                            delete_table(self.backend.writer()?, meta.root, cr)?;
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
            )?;
            if let Some(ipk) = meta.ipk {
                values[ipk] = Value::Null;
            }
            let record = encode_record(&values);
            insert_table(self.backend.writer()?, meta.root, rowid, &record)?;
            for idx in &indexes {
                let key = index_key(&idx.cols, &index_values, rowid);
                insert_index(self.backend.writer()?, idx.root, &key)?;
            }
            self.fire_triggers(
                &ins.table,
                TrigEvent::Insert,
                TriggerTiming::After,
                &meta.columns,
                None,
                Some((&index_values, rowid)),
                params,
            )?;
            affected += 1;
        }
        // REPLACE removed rows whose index entries were maintained incrementally;
        // rebuild from the final table state to be safe.
        if replaced {
            self.rebuild_indexes(&meta, &indexes)?;
        }
        Ok(affected)
    }

    /// Rowids of existing rows that conflict with a candidate row on the rowid
    /// or any UNIQUE/PRIMARY KEY column set (NULLs are considered distinct).
    fn find_conflicts(
        &self,
        meta: &TableMeta,
        rowid: i64,
        values: &[Value],
        exclude: Option<i64>,
    ) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for (er, ev) in self.scan_table(meta)? {
            if Some(er) == exclude {
                continue;
            }
            if er == rowid {
                out.push(er);
                continue;
            }
            for set in &meta.unique {
                let new_tuple: Vec<&Value> = set.iter().map(|&i| &values[i]).collect();
                if new_tuple.iter().any(|v| matches!(v, Value::Null)) {
                    continue; // a NULL makes the key distinct
                }
                let conflict = set.iter().zip(&new_tuple).all(|(&i, nv)| {
                    crate::value::cmp_values(&ev[i], nv) == core::cmp::Ordering::Equal
                });
                if conflict {
                    out.push(er);
                    break;
                }
            }
        }
        Ok(out)
    }

    fn exec_delete(&mut self, del: &Delete, params: &Params) -> Result<usize> {
        if self.is_view(&del.table) {
            return self.exec_view_delete(del, params);
        }
        let meta = self.table_meta(&del.table, None)?;
        if meta.without_rowid {
            return self.exec_delete_without_rowid(del, &meta, params);
        }
        let indexes = self.indexes_of(&del.table)?;
        let victims = self.matching_rowids(&meta, del.where_clause.as_ref(), params)?;
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
                )?;
            }
            // Enforce referential actions on dependent child tables.
            if self.foreign_keys {
                if let Some(old) = &old {
                    self.enforce_parent_change(&del.table, old, None, params)?;
                }
            }
            delete_table(self.backend.writer()?, meta.root, *rowid)?;
            if let Some(old) = &old {
                self.fire_triggers(
                    &del.table,
                    TrigEvent::Delete,
                    TriggerTiming::After,
                    &meta.columns,
                    Some((old, *rowid)),
                    None,
                    params,
                )?;
            }
        }
        if !victims.is_empty() {
            self.rebuild_indexes(&meta, &indexes)?;
        }
        Ok(victims.len())
    }

    fn exec_update(&mut self, upd: &Update, params: &Params) -> Result<usize> {
        if self.is_view(&upd.table) {
            return self.exec_view_update(upd, params);
        }
        let meta = self.table_meta(&upd.table, None)?;
        if meta.without_rowid {
            return self.exec_update_without_rowid(upd, &meta, params);
        }
        let indexes = self.indexes_of(&upd.table)?;
        // Collect (rowid, current values) for matching rows first.
        let mut targets: Vec<(i64, Vec<Value>)> = Vec::new();
        {
            let mut cur = TableCursor::new(self.backend.source(), meta.root);
            let encoding = self.backend.source().header().text_encoding;
            let mut ok = cur.first()?;
            while ok {
                let rowid = cur.rowid()?;
                let values = self.decode_full_row(&meta, rowid, &cur.payload()?, encoding)?;
                let matches = match &upd.where_clause {
                    Some(p) => {
                        let ctx = row_ctx(&values, &meta.columns, Some(rowid), params)
                            .with_subqueries(self);
                        eval::truth(&eval::eval(p, &ctx)?) == Some(true)
                    }
                    None => true,
                };
                if matches {
                    targets.push((rowid, values));
                }
                ok = cur.next()?;
            }
        }

        let mut affected = 0;
        for (rowid, mut values) in targets {
            let old_row = values.clone();
            // Apply SET assignments evaluated against the current row.
            for (col, expr) in &upd.assignments {
                let pos = meta
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col))
                    .ok_or_else(|| Error::Error(format!("no such column: {col}")))?;
                let ctx =
                    row_ctx(&values, &meta.columns, Some(rowid), params).with_subqueries(self);
                values[pos] = eval::eval(expr, &ctx)?;
            }
            apply_column_affinity(&meta, &mut values);
            check_not_null(&meta, &values)?;
            self.check_constraints(&meta, &values, Some(rowid), params)?;
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
            )?;
            // UNIQUE/PK conflict against any other row.
            if !self
                .find_conflicts(&meta, new_rowid, &values, Some(rowid))?
                .is_empty()
            {
                return Err(Error::Constraint("UNIQUE constraint failed".into()));
            }
            let new_full = values.clone();
            if let Some(ipk) = meta.ipk {
                values[ipk] = Value::Null;
            }
            let record = encode_record(&values);
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
            )?;
            affected += 1;
        }
        if affected > 0 {
            self.rebuild_indexes(&meta, &indexes)?;
        }
        Ok(affected)
    }

    // ---- index DDL & maintenance --------------------------------------------

    fn exec_create_index(&mut self, ci: &CreateIndex, sql_text: &str) -> Result<()> {
        if self.schema.index(&ci.name).is_some() {
            if ci.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("index {} already exists", ci.name)));
        }
        let tmeta = self.table_meta(&ci.table, None)?;
        let cols = self.index_columns(&tmeta, ci)?;
        let schema_next = self.next_rowid(crate::schema::SCHEMA_ROOT_PAGE)?;
        let rows = self.scan_table(&tmeta)?;

        let w = self.backend.writer()?;
        let root = create_index_root(w)?;
        for (rowid, values) in &rows {
            let key = index_key(&cols, values, *rowid);
            insert_index(w, root, &key)?;
        }
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

    /// Materialize each `WITH` CTE of `sel` into the environment, in declaration
    /// order (so a later CTE may reference an earlier one). Recursive CTEs are
    /// evaluated with the fixed-point loop.
    fn push_ctes(&self, sel: &Select, params: &Params) -> Result<()> {
        for cte in &sel.ctes {
            let binding = if references_name(&cte.select, &cte.name) {
                self.eval_recursive_cte(cte, params)?
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
            })
            .collect();
        Some((columns, b.rows.clone()))
    }

    /// Build the column metadata for a CTE from its body's output labels (or its
    /// explicit `(col, …)` list), labeled with the CTE name.
    fn cte_columns(&self, cte: &Cte, body_cols: &[String]) -> Vec<ColumnInfo> {
        let names = if cte.columns.is_empty() {
            body_cols.to_vec()
        } else {
            cte.columns.clone()
        };
        names
            .into_iter()
            .map(|n| ColumnInfo {
                name: n,
                table: cte.name.clone(),
                affinity: eval::Affinity::Blob,
            })
            .collect()
    }

    /// A non-recursive CTE: run its body once.
    fn materialize_plain_cte(&self, cte: &Cte, params: &Params) -> Result<CteBinding> {
        let result = self.run_select(&cte.select, params)?;
        let columns = self.cte_columns(cte, &result.columns);
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
    fn eval_recursive_cte(&self, cte: &Cte, params: &Params) -> Result<CteBinding> {
        // Flatten the body into arms: (op-before-this-arm, select). The first
        // arm has no preceding op.
        let mut arms: Vec<(Option<CompoundOp>, Select)> = Vec::new();
        let mut base = (*cte.select).clone();
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
        let columns = self.cte_columns(cte, &body_cols);

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
        };
        self.cte_env.borrow_mut().truncate(slot);
        result?;

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
    fn try_view(
        &self,
        name: &str,
        alias: Option<&str>,
        params: &Params,
    ) -> Result<Option<(Vec<ColumnInfo>, Vec<InputRow>)>> {
        use crate::schema::ObjectType;
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
            if d.if_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("no such {:?}: {}", d.kind, d.name)));
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
        Ok(())
    }

    fn exec_alter(&mut self, a: &Alter) -> Result<()> {
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

        match &a.action {
            AlterAction::AddColumn(cd) => {
                if ct
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&cd.name))
                {
                    return Err(Error::Error(format!("duplicate column name: {}", cd.name)));
                }
                ct.columns.push(cd.clone());
                let new_sql = sql::print::create_table(&ct);
                let table = a.table.clone();
                self.rewrite_schema_rows(|cols| {
                    if is_text(&cols[0], "table") && is_text(&cols[1], &table) {
                        cols[4] = Value::Text(new_sql.clone());
                        true
                    } else {
                        false
                    }
                })?;
            }
            AlterAction::RenameTable(new_name) => {
                if self.schema.table(new_name).is_some() {
                    return Err(Error::Error(format!("table {new_name} already exists")));
                }
                ct.name = new_name.clone();
                let new_table_sql = sql::print::create_table(&ct);
                let old = a.table.clone();
                let new_name = new_name.clone();
                self.rewrite_schema_rows(|cols| {
                    if is_text(&cols[0], "table") && is_text(&cols[1], &old) {
                        cols[1] = Value::Text(new_name.clone());
                        cols[2] = Value::Text(new_name.clone());
                        cols[4] = Value::Text(new_table_sql.clone());
                        true
                    } else if is_text(&cols[2], &old) {
                        // Dependent index/trigger/view: repoint, and rewrite an
                        // index's CREATE text to reference the new table name.
                        cols[2] = Value::Text(new_name.clone());
                        if is_text(&cols[0], "index") {
                            if let Some(Value::Text(isql)) = cols.get(4).cloned() {
                                if let Ok(Statement::CreateIndex(mut ci)) = sql::parse_one(&isql) {
                                    ci.table = new_name.clone();
                                    cols[4] = Value::Text(sql::print::create_index(&ci));
                                }
                            }
                        }
                        true
                    } else {
                        false
                    }
                })?;
            }
            AlterAction::RenameColumn { old, new } => {
                let pos = ct
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(old))
                    .ok_or_else(|| Error::Error(format!("no such column: {old}")))?;
                ct.columns[pos].name = new.clone();
                // Update table-level PK/UNIQUE column lists that name the column.
                for tc in &mut ct.constraints {
                    let names = match tc {
                        TableConstraint::PrimaryKey(n) | TableConstraint::Unique(n) => n,
                        _ => continue,
                    };
                    for nm in names {
                        if nm.eq_ignore_ascii_case(old) {
                            *nm = new.clone();
                        }
                    }
                }
                let new_table_sql = sql::print::create_table(&ct);
                let table = a.table.clone();
                let old = old.clone();
                let new = new.clone();
                self.rewrite_schema_rows(|cols| {
                    if is_text(&cols[0], "table") && is_text(&cols[1], &table) {
                        cols[4] = Value::Text(new_table_sql.clone());
                        true
                    } else if is_text(&cols[0], "index") && is_text(&cols[2], &table) {
                        // Rewrite an index over this table if it names the column.
                        if let Some(Value::Text(isql)) = cols.get(4).cloned() {
                            if let Ok(Statement::CreateIndex(mut ci)) = sql::parse_one(&isql) {
                                let mut changed = false;
                                for term in &mut ci.columns {
                                    if let Expr::Column { column, .. } = &mut term.expr {
                                        if column.eq_ignore_ascii_case(&old) {
                                            *column = new.clone();
                                            changed = true;
                                        }
                                    }
                                }
                                if changed {
                                    cols[4] = Value::Text(sql::print::create_index(&ci));
                                    return true;
                                }
                            }
                        }
                        false
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
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
        if eqs.is_empty() || eqs.iter().any(|(_, v)| matches!(v, Value::Null)) {
            return Ok(None); // `col = NULL` is never true; let the scan handle it
        }

        // Rowid (INTEGER PRIMARY KEY) equality: seek the table b-tree directly
        // by rowid. run_core re-applies the full WHERE, so returning the single
        // candidate row is a valid superset even when the literal isn't an exact
        // integer (e.g. `id = 5.5` seeks rowid 5, then gets filtered out).
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

        // Choose the index covering the longest leftmost prefix of equalities.
        let indexes = self.indexes_of(table_name)?;
        let mut best: Option<(u32, Vec<Value>)> = None;
        for idx in &indexes {
            let mut key = Vec::new();
            for &c in &idx.cols {
                match eqs.iter().find(|(col, _)| *col == c) {
                    Some((_, v)) => key.push(meta.columns[c].affinity.coerce(v.clone())),
                    None => break,
                }
            }
            if key.len() > best.as_ref().map_or(0, |(_, k)| k.len()) {
                best = Some((idx.root, key));
            }
        }
        let Some((root, key)) = best else {
            return Ok(None);
        };
        if key.is_empty() {
            return Ok(None);
        }

        let rowids = crate::btree::index_seek_rowids(self.backend.source(), root, &key)?;
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
                let detail =
                    self.eqp_access(&d.table, &d.table, &meta, d.where_clause.as_ref(), params)?;
                details.push((next_id, 0, detail));
            }
            Statement::Update(u) => {
                let meta = self.table_meta(&u.table, None)?;
                let detail =
                    self.eqp_access(&u.table, &u.table, &meta, u.where_clause.as_ref(), params)?;
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
    fn eqp_select(
        &self,
        sel: &Select,
        parent: i64,
        next_id: &mut i64,
        out: &mut Vec<(i64, i64, String)>,
        params: &Params,
    ) -> Result<()> {
        let Some(from) = &sel.from else {
            return Ok(()); // SELECT with no FROM => no scan node
        };
        // First source.
        let meta = self.table_meta(&from.first.name, from.first.alias.as_deref())?;
        let label = eqp_label(&from.first);
        let detail = if from.joins.is_empty() {
            self.eqp_access(
                &label,
                &from.first.name,
                &meta,
                sel.where_clause.as_ref(),
                params,
            )?
        } else {
            // Joins run in FROM order as nested-loop scans (no reordering).
            alloc::format!("SCAN {label}")
        };
        let id = *next_id;
        *next_id += 1;
        out.push((id, parent, detail));
        for join in &from.joins {
            let id = *next_id;
            *next_id += 1;
            out.push((
                id,
                parent,
                alloc::format!("SCAN {}", eqp_label(&join.table)),
            ));
        }
        // ORDER BY / GROUP BY that we satisfy with an in-memory sort.
        if !sel.order_by.is_empty() {
            let id = *next_id;
            *next_id += 1;
            out.push((id, parent, String::from("USE TEMP B-TREE FOR ORDER BY")));
        }
        Ok(())
    }

    /// The SCAN/SEARCH detail string for accessing one table given its WHERE.
    /// `label` is the display name (alias if any); `table` is the real table
    /// name used to look up its indexes.
    fn eqp_access(
        &self,
        label: &str,
        table: &str,
        meta: &TableMeta,
        where_clause: Option<&Expr>,
        params: &Params,
    ) -> Result<String> {
        let Some(where_expr) = where_clause else {
            return Ok(alloc::format!("SCAN {label}"));
        };
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
        eqs.retain(|(_, v)| !matches!(v, Value::Null));
        // Rowid equality wins, as in try_index_lookup.
        if let Some(ipk) = meta.ipk {
            if eqs.iter().any(|(c, _)| *c == ipk) {
                return Ok(alloc::format!(
                    "SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)"
                ));
            }
        }
        // Index covering the longest leftmost prefix of equalities.
        let mut best: Option<(String, Vec<usize>)> = None;
        for obj in self.schema.indexes_on(table) {
            let Some(sql) = &obj.sql else { continue };
            let Ok(Statement::CreateIndex(ci)) = sql::parse_one(sql) else {
                continue;
            };
            let Ok(cols) = self.index_columns(meta, &ci) else {
                continue;
            };
            let mut matched = Vec::new();
            for &c in &cols {
                if eqs.iter().any(|(col, _)| *col == c) {
                    matched.push(c);
                } else {
                    break;
                }
            }
            if matched.len() > best.as_ref().map_or(0, |(_, m)| m.len()) {
                best = Some((obj.name.clone(), matched));
            }
        }
        if let Some((idx_name, matched)) = best {
            if !matched.is_empty() {
                let cond = matched
                    .iter()
                    .map(|&c| alloc::format!("{}=?", meta.columns[c].name))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                return Ok(alloc::format!(
                    "SEARCH {label} USING INDEX {idx_name} ({cond})"
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
                    let cols = self.index_columns(&tmeta, &ci)?;
                    out.push(IndexMeta {
                        root: obj.rootpage,
                        cols,
                    });
                }
                // Automatic index: its columns are the n-th UNIQUE/PK set.
                None => {
                    if let Some(n) = autoindex_number(&obj.name, table) {
                        if let Some(cols) = tmeta.unique.get(n - 1) {
                            out.push(IndexMeta {
                                root: obj.rootpage,
                                cols: cols.clone(),
                            });
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn index_columns(&self, tmeta: &TableMeta, ci: &CreateIndex) -> Result<Vec<usize>> {
        let mut cols = Vec::new();
        for term in &ci.columns {
            let Expr::Column { column, .. } = &term.expr else {
                return Err(Error::Unsupported("expression indexes"));
            };
            let pos = tmeta
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(column))
                .ok_or_else(|| Error::Error(format!("no such column: {column}")))?;
            cols.push(pos);
        }
        Ok(cols)
    }

    /// Rebuild every index of a table in place (used after DELETE/UPDATE).
    fn rebuild_indexes(&mut self, tmeta: &TableMeta, indexes: &[IndexMeta]) -> Result<()> {
        if indexes.is_empty() {
            return Ok(());
        }
        let rows = self.scan_table(tmeta)?;
        let w = self.backend.writer()?;
        for idx in indexes {
            clear_index(w, idx.root)?;
            for (rowid, values) in &rows {
                let key = index_key(&idx.cols, values, *rowid);
                insert_index(w, idx.root, &key)?;
            }
        }
        Ok(())
    }

    /// Rowids of rows in `meta` satisfying `pred` (all rows if `None`).
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

    fn run_select(&self, sel: &Select, params: &Params) -> Result<QueryResult> {
        // Materialize this query's `WITH` CTEs into the environment for the
        // duration of the query, then restore the previous scope.
        let base = self.cte_env.borrow().len();
        let pushed = self.push_ctes(sel, params);
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
        for (op, operand) in &sel.compound {
            let r = self.run_core(operand, params)?;
            result.rows = apply_compound(*op, result.rows, r.rows);
        }
        self.compound_order_limit(&mut result, sel, params)?;
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
    ) -> Result<()> {
        if !sel.order_by.is_empty() {
            let mut keys = Vec::new();
            for term in &sel.order_by {
                let idx = resolve_order_index(&term.expr, &result.columns, result.columns.len())
                    .ok_or(Error::Unsupported(
                        "ORDER BY term must be an output column in a compound query",
                    ))?;
                keys.push((idx, term.descending));
            }
            result.rows.sort_by(|a, b| {
                for (idx, desc) in &keys {
                    let ord = eval::compare(&a[*idx], &b[*idx]);
                    let ord = if *desc { ord.reverse() } else { ord };
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }
        let offset = match &sel.offset {
            Some(e) => eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?).max(0) as usize,
            None => 0,
        };
        let limit = match &sel.limit {
            Some(e) => {
                Some(eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?).max(0) as usize)
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
            let values = self.compute_window(wexpr, columns, rows, params)?;
            let col_name = alloc::format!("__win{k}");
            columns.push(ColumnInfo {
                name: col_name.clone(),
                table: String::new(),
                affinity: eval::Affinity::Blob,
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
            args,
            star,
            over: Some(spec),
            ..
        } = wexpr
        else {
            return Err(Error::Error("not a window function".into()));
        };
        let lname = name.to_ascii_lowercase();
        let n = rows.len();

        // Per-row partition keys, order keys, and argument values.
        let mut part_keys: Vec<Vec<Value>> = Vec::with_capacity(n);
        let mut ord_keys: Vec<Vec<Value>> = Vec::with_capacity(n);
        let mut arg_vals: Vec<Vec<Value>> = Vec::with_capacity(n);
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
        // Ranking values per ordered position.
        for p in 0..m {
            let idx = ordered[p];
            let (fstart, fend) = frame_bounds(p, m, &gid, spec);
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
                "first_value" => {
                    if fstart < fend {
                        arg_vals[ordered[fstart]]
                            .first()
                            .cloned()
                            .unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                "last_value" => {
                    if fstart < fend {
                        arg_vals[ordered[fend - 1]]
                            .first()
                            .cloned()
                            .unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                "nth_value" => {
                    let nth = arg_vals[idx].get(1).map(eval::to_i64).unwrap_or(1);
                    // nth row within the frame (1-based).
                    let target = fstart + (nth.max(1) as usize) - 1;
                    if nth >= 1 && target < fend {
                        arg_vals[ordered[target]]
                            .first()
                            .cloned()
                            .unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                // Aggregate windows over the frame.
                _ => {
                    let frame: Vec<&Vec<Value>> = ordered[fstart..fend]
                        .iter()
                        .map(|&i| &arg_vals[i])
                        .collect();
                    window_aggregate(lname, star, &frame)?
                }
            };
            result[idx] = val;
        }
        Ok(())
    }

    fn run_core(&self, sel: &Select, params: &Params) -> Result<QueryResult> {
        let (mut columns, input_rows) = self.scan_source(sel, params)?;

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

        // Window functions: compute over the post-WHERE rows, append the results
        // as synthetic columns, and rewrite the projection to reference them.
        let rewritten;
        let sel = if window::has_window(sel) {
            if !sel.group_by.is_empty() || self.has_aggregate(sel) {
                return Err(Error::Unsupported(
                    "window functions combined with GROUP BY / aggregates",
                ));
            }
            rewritten = self.apply_windows(sel, &mut columns, &mut rows, params)?;
            &rewritten
        } else {
            sel
        };

        let aggregated = !sel.group_by.is_empty() || self.has_aggregate(sel);
        let (out_labels, mut out) = if aggregated {
            self.eval_aggregated(sel, &columns, rows, params)?
        } else {
            self.eval_simple(sel, &columns, rows, params)?
        };

        // DISTINCT (dedupe on output values, preserving first occurrence).
        if sel.distinct {
            let mut seen: Vec<Vec<Value>> = Vec::new();
            out.retain(|row| {
                if seen.iter().any(|s| rows_equal(s, &row.values)) {
                    false
                } else {
                    seen.push(row.values.clone());
                    true
                }
            });
        }

        // ORDER BY.
        if !sel.order_by.is_empty() {
            // Stable sort by the precomputed sort keys.
            out.sort_by(|a, b| {
                for (i, term) in sel.order_by.iter().enumerate() {
                    let ord = eval::compare(&a.sort_keys[i], &b.sort_keys[i]);
                    let ord = if term.descending { ord.reverse() } else { ord };
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }

        // OFFSET / LIMIT.
        let offset = match &sel.offset {
            Some(e) => eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?).max(0) as usize,
            None => 0,
        };
        let limit = match &sel.limit {
            Some(e) => {
                Some(eval::to_i64(&eval::eval(e, &EvalCtx::rowless(params))?).max(0) as usize)
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

        // Single-table fast path. Try an index-driven equality lookup first; the
        // full WHERE is still applied by run_core, so the index only needs to
        // return a superset of matching rows.
        if from.joins.is_empty() {
            let first_meta = self.table_meta(&from.first.name, from.first.alias.as_deref())?;
            if first_meta.without_rowid {
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
            let (jcols, jrows) = self.resolve_join_source(&join.table, params)?;

            let mut new_columns = columns.clone();
            new_columns.extend(jcols.iter().cloned());
            let n_jcols = jcols.len();

            let mut joined: Vec<Vec<Value>> = Vec::new();
            for left in &rows {
                let mut matched = false;
                for right in &jrows {
                    let mut combined = left.clone();
                    combined.extend(right.iter().cloned());
                    let keep = match &join.on {
                        Some(on) => {
                            let ctx = row_ctx(&combined, &new_columns, None, params);
                            eval::truth(&eval::eval(on, &ctx)?) == Some(true)
                        }
                        None => true, // CROSS / comma join
                    };
                    if keep {
                        joined.push(combined);
                        matched = true;
                    }
                }
                // LEFT JOIN: emit the left row with NULLs when nothing matched.
                if !matched && join.kind == JoinKind::Left {
                    let mut combined = left.clone();
                    combined.extend(core::iter::repeat_n(Value::Null, n_jcols));
                    joined.push(combined);
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
    fn run_subquery_source(
        &self,
        select: &Select,
        alias: Option<&str>,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        let result = self.run_select(select, params)?;
        let label = alias.unwrap_or("").to_string();
        let columns = result
            .columns
            .iter()
            .map(|n| ColumnInfo {
                name: n.clone(),
                table: label.clone(),
                affinity: eval::Affinity::Blob,
            })
            .collect();
        Ok((columns, result.rows))
    }

    fn resolve_join_source(
        &self,
        tref: &TableRef,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        if let Some(sub) = &tref.subquery {
            return self.run_subquery_source(sub, tref.alias.as_deref(), params);
        }
        if let Some((cols, rows)) = self.lookup_cte(&tref.name, tref.alias.as_deref()) {
            return Ok((cols, rows.into_iter().map(|r| r.values).collect()));
        }
        if let Some((cols, rows)) = self.try_view(&tref.name, tref.alias.as_deref(), params)? {
            return Ok((cols, rows.into_iter().map(|r| r.values).collect()));
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
        let mut out = Vec::new();
        while let Some(payload) = cur.next()? {
            let storage = decode_record(&payload, encoding)?;
            out.push(unpermute_row(meta, storage));
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
            values[target[i]] = eval::eval(e, &ctx)?;
        }
        apply_column_affinity(meta, &mut values);
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
            // PRIMARY KEY columns are implicitly NOT NULL in a WITHOUT ROWID table.
            for &c in pk {
                if matches!(values[c], Value::Null) {
                    return Err(Error::Constraint("NOT NULL constraint failed".into()));
                }
            }
            check_not_null(meta, &values)?;
            self.check_constraints(meta, &values, None, params)?;

            // Reject a duplicate primary key.
            let existing = self.scan_without_rowid(meta)?;
            let dup = existing
                .iter()
                .any(|r| pk.iter().all(|&c| eval::compare(&r[c], &values[c]).is_eq()));
            if dup {
                match ins.on_conflict {
                    OnConflict::Abort => {
                        return Err(Error::Constraint("UNIQUE constraint failed".into()))
                    }
                    OnConflict::Ignore => continue,
                    OnConflict::Replace => {
                        // Rebuild without the conflicting row, then insert.
                        self.rewrite_without_rowid(
                            meta,
                            existing.into_iter().filter(|r| {
                                !pk.iter().all(|&c| eval::compare(&r[c], &values[c]).is_eq())
                            }),
                        )?;
                    }
                }
            }
            let record = encode_record(&permute_row(meta, &values));
            insert_index(self.backend.writer()?, meta.root, &record)?;
            affected += 1;
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
                for (col, expr) in &upd.assignments {
                    let pos = meta
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(col))
                        .ok_or_else(|| Error::Error(format!("no such column: {col}")))?;
                    let ctx = row_ctx(&row, &meta.columns, None, params).with_subqueries(self);
                    row[pos] = eval::eval(expr, &ctx)?;
                }
                apply_column_affinity(meta, &mut row);
                check_not_null(meta, &row)?;
                self.check_constraints(meta, &row, None, params)?;
                affected += 1;
            }
            out.push(row);
        }
        // Reject duplicate primary keys produced by the update.
        let pk = &meta.storage_order[..meta.pk_len];
        for i in 0..out.len() {
            for j in (i + 1)..out.len() {
                if pk
                    .iter()
                    .all(|&c| eval::compare(&out[i][c], &out[j][c]).is_eq())
                {
                    return Err(Error::Constraint("UNIQUE constraint failed".into()));
                }
            }
        }
        if affected > 0 {
            self.rewrite_without_rowid(meta, out.into_iter())?;
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
        let w = self.backend.writer()?;
        clear_index(w, meta.root)?;
        for rec in &records {
            insert_index(w, meta.root, rec)?;
        }
        Ok(())
    }

    /// Scan a whole table into `(rowid, column values)`.
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
        let mut values = decode_record(payload, encoding)?;
        let stored = values.len();
        values.resize(meta.columns.len(), Value::Null);
        if stored < meta.columns.len() {
            let p = Params::default();
            let ctx = EvalCtx::rowless(&p);
            for (i, def) in meta.defaults.iter().enumerate().skip(stored) {
                if let Some(e) = def {
                    values[i] = eval::eval(e, &ctx)?;
                }
            }
        }
        if let Some(ipk) = meta.ipk {
            values[ipk] = Value::Integer(rowid);
        }
        Ok(values)
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
        // Wildcards make no sense with aggregation unless trivially grouped; keep
        // it simple and reject for now.
        if sel
            .columns
            .iter()
            .any(|c| matches!(c, ResultColumn::Wildcard | ResultColumn::TableWildcard(_)))
        {
            return Err(Error::Unsupported("'*' with aggregation"));
        }

        // Partition rows into groups (first-seen order).
        let mut group_keys: Vec<Vec<Value>> = Vec::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let ctx = r.ctx(columns, params).with_subqueries(self);
            let mut key = Vec::new();
            for g in &sel.group_by {
                key.push(eval::eval(g, &ctx)?);
            }
            match group_keys.iter().position(|k| rows_equal(k, &key)) {
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
        }

        let labels = self.output_labels(sel, columns);
        let mut out = Vec::new();
        for group in &groups {
            // Representative row context for bare column references.
            let repr = group.first().map(|&i| &rows[i]);
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

            // HAVING (aggregate-aware).
            if let Some(having) = &sel.having {
                let h = self.substitute_aggregates(having, columns, &rows, group, params)?;
                if eval::truth(&eval::eval(&h, &repr_ctx)?) != Some(true) {
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
                over: None,
            } if func::is_aggregate_call(name, args.len(), *star) => {
                let v = self.compute_aggregate(
                    name, *distinct, args, *star, columns, rows, group, params,
                )?;
                Expr::Literal(value_to_literal(v))
            }
            Expr::Function {
                name,
                distinct,
                args,
                star,
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

    #[allow(clippy::too_many_arguments)]
    fn compute_aggregate(
        &self,
        name: &str,
        distinct: bool,
        args: &[Expr],
        star: bool,
        columns: &[ColumnInfo],
        rows: &[InputRow],
        group: &[usize],
        params: &Params,
    ) -> Result<Value> {
        let lname = name.to_ascii_lowercase();

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
            dedup_values(&mut vals);
        }

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
            "group_concat" => {
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
            _ => return Err(Error::Unsupported("aggregate function")),
        })
    }

    fn has_aggregate(&self, sel: &Select) -> bool {
        sel.columns.iter().any(|c| match c {
            ResultColumn::Expr { expr, .. } => contains_aggregate(expr),
            _ => false,
        }) || sel.having.as_ref().is_some_and(contains_aggregate)
    }

    fn output_labels(&self, sel: &Select, columns: &[ColumnInfo]) -> Vec<String> {
        let mut labels = Vec::new();
        for col in &sel.columns {
            match col {
                ResultColumn::Wildcard | ResultColumn::TableWildcard(_) => {
                    for c in columns {
                        labels.push(c.name.clone());
                    }
                }
                ResultColumn::Expr { expr, alias } => {
                    labels.push(alias.clone().unwrap_or_else(|| expr_label(expr)));
                }
            }
        }
        labels
    }

    fn table_meta(&self, name: &str, alias: Option<&str>) -> Result<TableMeta> {
        let obj = self
            .schema
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
        let not_null: Vec<bool> = ct
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                // The INTEGER PRIMARY KEY (rowid alias) is implicitly NOT NULL.
                Some(i) == ipk
                    || c.constraints
                        .iter()
                        .any(|k| matches!(k, ColumnConstraint::NotNull))
            })
            .collect();
        // CHECK constraints (column-level + table-level); each is evaluated
        // against the full row on INSERT/UPDATE.
        let mut checks: Vec<Expr> = Vec::new();
        for col in &ct.columns {
            for k in &col.constraints {
                if let ColumnConstraint::Check(e) = k {
                    checks.push(e.clone());
                }
            }
        }
        for tc in &ct.constraints {
            if let TableConstraint::Check(e) = tc {
                checks.push(e.clone());
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
            // Supported subset: the PRIMARY KEY is the only key (no extra UNIQUE).
            if unique.len() > 1 {
                return Err(Error::Unsupported(
                    "WITHOUT ROWID with additional UNIQUE constraints",
                ));
            }
            let mut order = pk.clone();
            for i in 0..columns.len() {
                if !pk.contains(&i) {
                    order.push(i);
                }
            }
            let pk_len = pk.len();
            (true, order, pk_len)
        } else {
            (false, Vec::new(), 0)
        };

        Ok(TableMeta {
            root: obj.rootpage,
            columns,
            defaults,
            not_null,
            checks,
            unique,
            ipk,
            without_rowid,
            storage_order,
            pk_len,
        })
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
        for expr in &meta.checks {
            let ctx = row_ctx(values, &meta.columns, rowid, params).with_subqueries(self);
            if eval::truth(&eval::eval(expr, &ctx)?) == Some(false) {
                return Err(Error::Constraint("CHECK constraint failed".into()));
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
    not_null: Vec<bool>,
    /// CHECK constraint expressions (column-level and table-level).
    checks: Vec<Expr>,
    /// Column-index sets that must be UNIQUE (excludes the rowid IPK).
    unique: Vec<Vec<usize>>,
    ipk: Option<usize>,
    /// `true` for a `WITHOUT ROWID` table (stored as a PK-clustered index b-tree
    /// rather than a rowid table b-tree).
    without_rowid: bool,
    /// For a `WITHOUT ROWID` table, the on-disk column order: PRIMARY KEY columns
    /// first (in key order), then the remaining columns in declared order. Empty
    /// for ordinary rowid tables. `pk_len` is how many leading entries are PK.
    storage_order: Vec<usize>,
    pk_len: usize,
}

/// An index's b-tree root and the table column positions it covers.
struct IndexMeta {
    root: u32,
    cols: Vec<usize>,
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

    fn exists(&self, select: &Select, outer: &EvalCtx) -> Result<bool> {
        self.with_outer_frame(outer, |params| {
            Ok(!self.run_select(select, params)?.rows.is_empty())
        })
    }

    fn resolve_outer(&self, table: Option<&str>, name: &str) -> Option<Value> {
        let scope = self.outer_scope.borrow();
        for frame in scope.iter().rev() {
            if table.is_none() && eval::is_rowid_alias(name) {
                if let Some(r) = frame.rowid {
                    return Some(Value::Integer(r));
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

/// Compare two ordering-key vectors with per-position `descending` flags
/// (missing flags default to ascending).
fn cmp_keys(a: &[Value], b: &[Value], desc: &[bool]) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let mut o = eval::compare(x, y);
        if desc.get(i).copied().unwrap_or(false) {
            o = o.reverse();
        }
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// The `[start, end)` frame indices (into the ordered partition) for position
/// `p`, given peer-group ids `gid` and the window `spec`.
///
/// With no explicit frame: the whole partition when there is no `ORDER BY`, else
/// `UNBOUNDED PRECEDING` through the current row's last peer — SQLite's default.
/// `ROWS` frames use physical offsets; `RANGE`/`GROUPS` use peer-group offsets.
fn frame_bounds(p: usize, m: usize, gid: &[usize], spec: &WindowSpec) -> (usize, usize) {
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
        FrameMode::Range | FrameMode::Groups => (
            group_bound(&frame.start, p, m, gid, true),
            group_bound(&frame.end, p, m, gid, false),
        ),
    };
    let start = start.min(m);
    (start, end.min(m).max(start))
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
        "group_concat" => {
            if vals.is_empty() {
                Value::Null
            } else {
                let parts: Vec<String> = vals.iter().map(eval::to_text).collect();
                Value::Text(parts.join(","))
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
        if meta.not_null[i] && matches!(v, Value::Null) {
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
        _ => None,
    }
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        // A window function (`f(…) OVER (…)`) is not a plain aggregate, even when
        // `f` is an aggregate name; only its arguments might contain aggregates.
        Expr::Function {
            over: Some(_),
            args,
            ..
        } => args.iter().any(contains_aggregate),
        Expr::Function {
            name, args, star, ..
        } => {
            func::is_aggregate_call(name, args.len(), *star) || args.iter().any(contains_aggregate)
        }
        Expr::Binary { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::Unary { expr, .. } | Expr::Paren(expr) => contains_aggregate(expr),
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || when_then
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_result.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        _ => false,
    }
}

/// Combine two compound-query operand row sets per the operator.
fn apply_compound(
    op: CompoundOp,
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
) -> Vec<Vec<Value>> {
    let dedup = |rows: Vec<Vec<Value>>| -> Vec<Vec<Value>> {
        let mut seen: Vec<Vec<Value>> = Vec::new();
        for r in rows {
            if !seen.iter().any(|s| rows_equal(s, &r)) {
                seen.push(r);
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
                .filter(|l| right.iter().any(|r| rows_equal(l, r)))
                .collect(),
        ),
        CompoundOp::Except => dedup(
            left.into_iter()
                .filter(|l| !right.iter().any(|r| rows_equal(l, r)))
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

fn dedup_values(vals: &mut Vec<Value>) {
    let mut seen: Vec<Value> = Vec::new();
    vals.retain(|v| {
        if seen
            .iter()
            .any(|s| eval::compare(s, v) == core::cmp::Ordering::Equal)
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

/// Render a literal as text (for `PRAGMA table_info`'s default-value column).
fn literal_text(l: &Literal) -> String {
    eval::to_text(&match l {
        Literal::Null => Value::Null,
        Literal::Integer(i) => Value::Integer(*i),
        Literal::Real(r) => Value::Real(*r),
        Literal::Str(s) => Value::Text(s.clone()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Boolean(b) => Value::Integer(*b as i64),
    })
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

/// Detect an `INTEGER PRIMARY KEY` rowid alias column (must be declared exactly
/// `INTEGER`, per SQLite — `INT PRIMARY KEY` does not alias the rowid).
/// The UNIQUE / non-rowid PRIMARY KEY column-index sets of a table, in
/// declaration order (column-level constraints first, in column order, then
/// table-level constraints). This is exactly the order SQLite numbers its
/// `sqlite_autoindex_<table>_<n>` automatic indexes.
fn collect_unique_sets(ct: &CreateTable, ipk: Option<usize>) -> Vec<Vec<usize>> {
    let col_pos = |name: &str| {
        ct.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    };
    let mut unique: Vec<Vec<usize>> = Vec::new();
    for (i, c) in ct.columns.iter().enumerate() {
        for k in &c.constraints {
            match k {
                ColumnConstraint::Unique => unique.push(alloc::vec![i]),
                ColumnConstraint::PrimaryKey { .. } if Some(i) != ipk => {
                    unique.push(alloc::vec![i])
                }
                _ => {}
            }
        }
    }
    for tc in &ct.constraints {
        let names = match tc {
            TableConstraint::Unique(n) | TableConstraint::PrimaryKey(n) => n,
            _ => continue,
        };
        let idxs: Option<Vec<usize>> = names.iter().map(|n| col_pos(n)).collect();
        if let Some(set) = idxs {
            // Skip a single-column PK that is the rowid alias.
            if !(set.len() == 1 && Some(set[0]) == ipk) {
                unique.push(set);
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
        if let TableConstraint::PrimaryKey(names) = tc {
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
        let is_pk = c
            .constraints
            .iter()
            .any(|k| matches!(k, ColumnConstraint::PrimaryKey { .. }));
        if is_integer && is_pk {
            return Some(i);
        }
    }
    // Table-level single-column PRIMARY KEY over an INTEGER column.
    for tc in &ct.constraints {
        if let TableConstraint::PrimaryKey(cols) = tc {
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
