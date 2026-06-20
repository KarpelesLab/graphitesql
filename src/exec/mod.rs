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
    /// Rows projected by the most recent `RETURNING` clause, drained by
    /// [`execute_returning`](Self::execute_returning). Populated as a side effect
    /// of `INSERT`/`UPDATE`/`DELETE` execution when the statement has a
    /// `RETURNING` list.
    returning_rows: core::cell::RefCell<Vec<Vec<Value>>>,
    /// Count of open savepoints. Like `in_tx`, a non-zero count suppresses
    /// autocommit so changes accumulate until the outermost savepoint is released.
    open_savepoints: usize,
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
            in_tx: false,
            cte_env: core::cell::RefCell::new(Vec::new()),
            outer_scope: core::cell::RefCell::new(Vec::new()),
            foreign_keys: false,
            trigger_depth: core::cell::Cell::new(0),
            recursive_triggers: false,
            returning_rows: core::cell::RefCell::new(Vec::new()),
            open_savepoints: 0,
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
            returning_rows: core::cell::RefCell::new(Vec::new()),
            open_savepoints: 0,
        })
    }

    /// Open an existing database for reading and writing through `vfs`. Creates
    /// (and recovers from) a `<path>-journal` companion file.
    pub fn open_vfs(vfs: &dyn Vfs, path: &str) -> Result<Connection> {
        let main = vfs.open(path, OpenFlags::READ_WRITE)?;
        let journal = vfs.open(&journal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        let wal = vfs.open(&wal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        Connection::from_pager(WritePager::open_wal(main, Some(journal), Some(wal))?)
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
        let wal = vfs.open(&wal_path(path), OpenFlags::READ_WRITE_CREATE)?;
        let mut db = WritePager::create_wal(main, Some(journal), Some(wal), page_size)?;
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

    /// Run `sql` through the experimental VDBE engine instead of the tree-walker.
    /// Supports constant projections and plain single-table scans
    /// (`SELECT <exprs> FROM <table>` with no `WHERE`/joins/aggregates/`ORDER BY`);
    /// returns `Unsupported` otherwise so callers can fall back to
    /// [`query`](Self::query).
    pub fn query_vdbe(&self, sql: &str) -> Result<QueryResult> {
        let Statement::Select(sel) = sql::parse_one(sql)? else {
            return Err(Error::Unsupported("query_vdbe expects SELECT"));
        };
        // Constant SELECT (no FROM): compile and run directly.
        let Some(from) = &sel.from else {
            let prog = vdbe::compile_const_select(&sel)?;
            let rows = vdbe::run(&prog)?;
            return Ok(QueryResult {
                columns: prog.columns,
                rows,
            });
        };
        // Single plain table: materialize its rows and run a cursor program.
        if !from.joins.is_empty() || from.first.subquery.is_some() || from.first.tvf_args.is_some()
        {
            return Err(Error::Unsupported("VDBE: only a single plain table"));
        }
        let meta = self.table_meta(&from.first.name, from.first.alias.as_deref())?;
        let col_names: Vec<String> = meta.columns.iter().map(|c| c.name.clone()).collect();
        let prog = vdbe::compile_table_select(&sel, &col_names)?;
        let rows: Vec<Vec<Value>> = if meta.without_rowid {
            self.scan_without_rowid(&meta)?
        } else {
            self.scan_table(&meta)?
                .into_iter()
                .map(|(_, v)| v)
                .collect()
        };
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
            "freelist_count" => Ok(single(
                "freelist_count",
                Value::Integer(header.freelist_count as i64),
            )),
            "application_id" => Ok(single(
                "application_id",
                Value::Integer(header.application_id as i64),
            )),
            "data_version" => Ok(single("data_version", Value::Integer(1))),
            "table_info" => self.pragma_table_info(p),
            "index_list" => self.pragma_index_list(p),
            "index_info" => self.pragma_index_info(p),
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
                let mode = if self.backend.wal_mode() {
                    "wal"
                } else {
                    "delete"
                };
                Ok(single("journal_mode", Value::Text(mode.into())))
            }
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
            // `table_info` hides generated columns (they appear in `table_xinfo`).
            if col
                .constraints
                .iter()
                .any(|c| matches!(c, ColumnConstraint::Generated { .. }))
            {
                continue;
            }
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
        let mut rows = Vec::new();
        for (seq, obj) in objs.iter().rev().enumerate() {
            let (unique, origin, partial) = match &obj.sql {
                Some(sql) => match sql::parse_one(sql) {
                    Ok(Statement::CreateIndex(ci)) => {
                        (ci.unique as i64, "c", ci.where_clause.is_some() as i64)
                    }
                    _ => (0, "c", 0),
                },
                None => (1, "u", 0), // an automatic UNIQUE/PK index
            };
            rows.push(alloc::vec![
                Value::Integer(seq as i64),
                Value::Text(obj.name.clone()),
                Value::Integer(unique),
                Value::Text(origin.into()),
                Value::Integer(partial),
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
    fn pragma_index_info(&self, p: &Pragma) -> Result<QueryResult> {
        let index = Self::pragma_arg_name(p)?;
        let obj = self
            .schema
            .index(&index)
            .ok_or_else(|| Error::Error(format!("no such index: {index}")))?;
        let tmeta = self.table_meta(&obj.tbl_name, None)?;
        let cols: Vec<usize> = match &obj.sql {
            Some(sql) => match sql::parse_one(sql)? {
                Statement::CreateIndex(ci) => self.index_columns(&tmeta, &ci)?,
                _ => Vec::new(),
            },
            None => autoindex_number(&obj.name, &obj.tbl_name)
                .and_then(|n| tmeta.unique.get(n - 1).cloned())
                .unwrap_or_default(),
        };
        let rows = cols
            .iter()
            .enumerate()
            .map(|(seqno, &cid)| {
                alloc::vec![
                    Value::Integer(seqno as i64),
                    Value::Integer(cid as i64),
                    Value::Text(tmeta.columns[cid].name.clone()),
                ]
            })
            .collect();
        Ok(QueryResult {
            columns: ["seqno", "cid", "name"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
            rows,
        })
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
            return Err(Error::Corrupt("schema sql is not CREATE TABLE".into()));
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
        // SQLite numbers foreign keys from the last declared (id 0) backward.
        let n = fks.len();
        for (i, (from_cols, fk)) in fks.iter().enumerate() {
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
            .filter(|o| o.obj_type == ObjectType::Table && !o.name.starts_with("sqlite_"))
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
                self.open_savepoints = 0;
                return Ok(0);
            }
            Statement::Savepoint(name) => {
                self.backend.writer()?.savepoint(name);
                self.open_savepoints += 1;
                return Ok(0);
            }
            Statement::Release(name) => {
                self.backend.writer()?.release_savepoint(name)?;
                self.open_savepoints = self.backend.writer()?.savepoint_depth();
                // Releasing the outermost savepoint of an implicit transaction
                // finalizes it.
                if self.open_savepoints == 0 && !self.in_tx {
                    self.backend.writer()?.commit()?;
                    self.schema = Schema::read(self.backend.source())?;
                }
                return Ok(0);
            }
            Statement::RollbackTo(name) => {
                self.backend.writer()?.rollback_to_savepoint(name)?;
                self.open_savepoints = self.backend.writer()?.savepoint_depth();
                // The schema may have reverted to the savepoint's state.
                self.schema = Schema::read(self.backend.source())?;
                return Ok(0);
            }
            Statement::Rollback => {
                self.backend.writer()?.rollback();
                self.in_tx = false;
                self.open_savepoints = 0;
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
            Statement::Vacuum => {
                self.exec_vacuum()?;
                0
            }
            Statement::Analyze(target) => {
                self.exec_analyze(target.as_deref())?;
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

    /// `VACUUM`: rebuild the database into a fresh, compact image (no free pages,
    /// defragmented b-trees) and replace the file. Implemented by replaying the
    /// stored `CREATE` statements and re-inserting all rows into a throwaway
    /// in-memory database, then copying its pages over. A no-op for read-only
    /// backends.
    fn exec_vacuum(&mut self) -> Result<()> {
        use crate::schema::ObjectType;
        if !matches!(self.backend, Backend::Write(_)) {
            return Ok(());
        }
        // Flush any WAL frames into the main image first.
        if self.backend.wal_mode() {
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

        // Build a compact copy in a throwaway in-memory database.
        let mut tmp = Connection::open_memory()?;
        // 1. Tables (this also recreates their automatic indexes).
        for (ty, _, sql) in &objs {
            if *ty == ObjectType::Table {
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
        for (ty, name, _) in &objs {
            if *ty != ObjectType::Table {
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

        // Copy the compact image's pages over the current database file.
        let count = tmp.backend.source().page_count();
        let mut image = Vec::with_capacity(count as usize);
        for n in 1..=count {
            image.push(tmp.backend.source().page(n)?.data().to_vec());
        }
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
        if self.schema.table(&ct.name).is_some() {
            if ct.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("table {} already exists", ct.name)));
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
        for (n, set) in unique.iter().enumerate() {
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
                columns: Vec::new(),
                source: InsertSource::Values(value_rows),
                on_conflict: OnConflict::Abort,
                upsert: None,
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
                collation: c.collation,
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
            if ins.upsert.is_some() || !ins.returning.is_empty() {
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
            check_not_null(&meta, &values)?;
            self.check_constraints(&meta, &values, Some(rowid), params)?;
            self.check_fk_child(&ins.table, &meta, &values)?;

            // Resolve UNIQUE / PRIMARY KEY (incl. rowid) conflicts.
            let conflicts = self.find_conflicts(&meta, rowid, &values, None)?;
            if !conflicts.is_empty() {
                // An `ON CONFLICT … DO …` upsert clause intercepts the conflict.
                if let Some(up) = &ins.upsert {
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
            let record = self.encode_table_record(&meta, &index_values);
            insert_table(self.backend.writer()?, meta.root, rowid, &record)?;
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
            )?;
            if !ins.returning.is_empty() {
                self.collect_returning(&ins.returning, &meta, &index_values, Some(rowid), params)?;
            }
            affected += 1;
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
        )?;
        if !self
            .find_conflicts(meta, new_rowid, &values, Some(existing_rowid))?
            .is_empty()
        {
            return Err(Error::Constraint("UNIQUE constraint failed".into()));
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
                    crate::value::cmp_values_coll(&ev[i], nv, meta.columns[i].collation)
                        == core::cmp::Ordering::Equal
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
            if !del.returning.is_empty() {
                return Err(Error::Unsupported("RETURNING on WITHOUT ROWID tables"));
            }
            return self.exec_delete_without_rowid(del, &meta, params);
        }
        let indexes = self.indexes_of(&del.table)?;
        let victims = self.matching_rowids(&meta, del.where_clause.as_ref(), params)?;
        for rowid in &victims {
            let old = self.read_row(&meta, *rowid)?;
            if let Some(old) = &old {
                if !del.returning.is_empty() {
                    self.collect_returning(&del.returning, &meta, old, Some(*rowid), params)?;
                }
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
            self.compact_table(&meta)?;
            self.rebuild_indexes(&meta, &indexes)?;
        }
        Ok(victims.len())
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
        if self.is_view(&upd.table) {
            return self.exec_view_update(upd, params);
        }
        let meta = self.table_meta(&upd.table, None)?;
        if meta.without_rowid {
            if !upd.returning.is_empty() {
                return Err(Error::Unsupported("RETURNING on WITHOUT ROWID tables"));
            }
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
                if meta.is_generated(pos) {
                    return Err(Error::Error(format!(
                        "cannot UPDATE generated column \"{col}\""
                    )));
                }
                let ctx =
                    row_ctx(&values, &meta.columns, Some(rowid), params).with_subqueries(self);
                values[pos] = eval::eval(expr, &ctx)?;
            }
            apply_column_affinity(&meta, &mut values);
            self.materialize_generated(&meta, &mut values, params)?;
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
        if self.schema.index(&ci.name).is_some() {
            if ci.if_not_exists {
                return Ok(());
            }
            return Err(Error::Error(format!("index {} already exists", ci.name)));
        }
        let tmeta = self.table_meta(&ci.table, None)?;
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
                collation: c.collation,
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
                collation: crate::value::Collation::default(),
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
        // `NOT INDEXED` forbids any index for this table; `INDEXED BY name`
        // restricts to one named index (validated below).
        let hint = sel.from.as_ref().and_then(|f| f.first.index_hint.as_ref());
        if matches!(hint, Some(IndexHint::NotIndexed)) {
            return Ok(None);
        }
        let mut eqs: Vec<(usize, Value)> = Vec::new();
        collect_eq_constraints(where_expr, &meta.columns, params, &mut eqs);
        if eqs.is_empty() || eqs.iter().any(|(_, v)| matches!(v, Value::Null)) {
            return Ok(None); // `col = NULL` is never true; let the scan handle it
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
        let mut best: Option<(u32, Vec<Value>, Vec<crate::value::Collation>, u64)> = None;
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
            let mut colls = Vec::new();
            for (pos, &c) in idx.cols.iter().enumerate() {
                match eqs.iter().find(|(col, _)| *col == c) {
                    Some((_, v)) => {
                        key.push(meta.columns[c].affinity.coerce(v.clone()));
                        colls.push(idx.collations[pos]);
                    }
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
                Some((_, bk, _, be)) => est < *be || (est == *be && key.len() > bk.len()),
            };
            if better {
                best = Some((idx.root, key, colls, est));
            }
        }
        let Some((root, key, seek_colls, _)) = best else {
            return Ok(None);
        };
        if key.is_empty() {
            return Ok(None);
        }

        let rowids =
            crate::btree::index_seek_rowids(self.backend.source(), root, &key, &seek_colls)?;
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
        // Index covering the longest leftmost prefix of equalities, preferring
        // the most selective one when `ANALYZE` statistics are available (kept in
        // step with the cost-based chooser in try_index_lookup).
        let stats = self.stat1_map();
        let mut best: Option<(String, Vec<usize>, u64)> = None;
        for obj in self.schema.indexes_on(table) {
            let Some(sql) = &obj.sql else { continue };
            let Ok(Statement::CreateIndex(ci)) = sql::parse_one(sql) else {
                continue;
            };
            if ci.where_clause.is_some() {
                continue; // partial indexes are not used for seeks (see try_index_lookup)
            }
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
            if matched.is_empty() {
                continue;
            }
            let est = stats
                .get(&obj.name)
                .and_then(|s| s.get(matched.len()).copied())
                .unwrap_or(u64::MAX - matched.len() as u64);
            let better = match &best {
                None => true,
                Some((_, bm, be)) => est < *be || (est == *be && matched.len() > bm.len()),
            };
            if better {
                best = Some((obj.name.clone(), matched, est));
            }
        }
        if let Some((idx_name, matched, _)) = best {
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
                    let (cols, key_exprs, collations) = self.index_key_spec(&tmeta, &ci)?;
                    out.push(IndexMeta {
                        name: obj.name.clone(),
                        root: obj.rootpage,
                        cols,
                        collations,
                        partial: ci.where_clause.clone(),
                        key_exprs,
                    });
                }
                // Automatic index: its columns are the n-th UNIQUE/PK set.
                None => {
                    if let Some(n) = autoindex_number(&obj.name, table) {
                        if let Some(cols) = tmeta.unique.get(n - 1) {
                            let collations = self.col_collations(&tmeta, cols);
                            out.push(IndexMeta {
                                name: obj.name.clone(),
                                root: obj.rootpage,
                                cols: cols.clone(),
                                collations,
                                partial: None,
                                key_exprs: None,
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
                keys.push((idx, term.descending, term.nulls_first));
            }
            result.rows.sort_by(|a, b| {
                for (idx, desc, nf) in &keys {
                    let ord = cmp_order(
                        &a[*idx],
                        &b[*idx],
                        *desc,
                        *nf,
                        crate::value::Collation::Binary,
                    );
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

        // ORDER BY.
        if !sel.order_by.is_empty() {
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
        // A table-valued function used as the sole source.
        if from.joins.is_empty() && from.first.tvf_args.is_some() {
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

            let left_width = columns.len();
            let mut joined: Vec<Vec<Value>> = Vec::new();
            let mut right_matched = alloc::vec![false; jrows.len()];
            for left in &rows {
                let mut matched = false;
                for (ri, right) in jrows.iter().enumerate() {
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
                let step = nums.get(2).copied().unwrap_or(1);
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
                let mut rows = Vec::new();
                let mut next_id = 0i64;
                if lname == "json_tree" {
                    json_tree_walk(&root, None, "$", "$", None, &mut next_id, &mut rows);
                } else {
                    json_each_children(&root, &mut next_id, &mut rows);
                }
                Ok((columns, rows))
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
        let columns = result
            .columns
            .iter()
            .map(|n| ColumnInfo {
                name: n.clone(),
                table: label.clone(),
                affinity: eval::Affinity::Blob,
                collation: crate::value::Collation::default(),
            })
            .collect();
        Ok((columns, result.rows))
    }

    fn resolve_join_source(
        &self,
        tref: &TableRef,
        params: &Params,
    ) -> Result<(Vec<ColumnInfo>, Vec<Vec<Value>>)> {
        if tref.tvf_args.is_some() {
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

            // Reject a duplicate primary key or any other UNIQUE constraint.
            let existing = self.scan_without_rowid(meta)?;
            let dup = existing.iter().any(|r| unique_match(meta, r, &values));
            if dup {
                match ins.on_conflict {
                    OnConflict::Abort => {
                        return Err(Error::Constraint("UNIQUE constraint failed".into()))
                    }
                    OnConflict::Ignore => continue,
                    OnConflict::Replace => {
                        // Rebuild without the conflicting row(s), then insert.
                        self.rewrite_without_rowid(
                            meta,
                            existing
                                .into_iter()
                                .filter(|r| !unique_match(meta, r, &values)),
                        )?;
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
                    let ctx = row_ctx(&row, &meta.columns, None, params).with_subqueries(self);
                    row[pos] = eval::eval(expr, &ctx)?;
                }
                apply_column_affinity(meta, &mut row);
                self.materialize_generated(meta, &mut row, params)?;
                check_not_null(meta, &row)?;
                self.check_constraints(meta, &row, None, params)?;
                affected += 1;
            }
            out.push(row);
        }
        // Reject duplicate primary keys or UNIQUE values produced by the update.
        for i in 0..out.len() {
            for j in (i + 1)..out.len() {
                if unique_match(meta, &out[i], &out[j]) {
                    return Err(Error::Constraint("UNIQUE constraint failed".into()));
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
        // Wildcards make no sense with aggregation unless trivially grouped; keep
        // it simple and reject for now.
        if sel
            .columns
            .iter()
            .any(|c| matches!(c, ResultColumn::Wildcard | ResultColumn::TableWildcard(_)))
        {
            return Err(Error::Unsupported("'*' with aggregation"));
        }

        // Partition rows into groups (first-seen order), comparing each grouping
        // key under its column collation.
        let group_colls: Vec<crate::value::Collation> = {
            let cctx = row_ctx(&[], columns, None, params);
            sel.group_by
                .iter()
                .map(|g| eval::key_collation(g, &cctx))
                .collect()
        };
        let mut group_keys: Vec<Vec<Value>> = Vec::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let ctx = r.ctx(columns, params).with_subqueries(self);
            let mut key = Vec::new();
            for g in &sel.group_by {
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
                filter,
                order_by,
                over: None,
            } if func::is_aggregate_call(name, args.len(), *star) => {
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
                Expr::Literal(value_to_literal(v))
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
fn cmp_order(
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
/// a scalar root).
fn json_each_children(
    root: &crate::exec::json::Json,
    next_id: &mut i64,
    rows: &mut Vec<Vec<Value>>,
) {
    use crate::exec::json::Json;
    match root {
        Json::Object(members) => {
            for (k, v) in members {
                let fullkey = alloc::format!("$.{k}");
                json_emit_node(
                    v,
                    Some(Value::Text(k.clone())),
                    &fullkey,
                    "$",
                    None,
                    next_id,
                    rows,
                );
            }
        }
        Json::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                let fullkey = alloc::format!("$[{i}]");
                json_emit_node(
                    v,
                    Some(Value::Integer(i as i64)),
                    &fullkey,
                    "$",
                    None,
                    next_id,
                    rows,
                );
            }
        }
        scalar => {
            json_emit_node(scalar, None, "$", "$", None, next_id, rows);
        }
    }
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
    meta.unique.iter().any(|set| {
        set.iter().all(|&c| {
            !matches!(a[c], Value::Null)
                && !matches!(b[c], Value::Null)
                && crate::value::cmp_values_coll(&a[c], &b[c], meta.columns[c].collation).is_eq()
        })
    })
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
            ResultColumn::Expr { expr, alias } => {
                labels.push(alias.clone().unwrap_or_else(|| expr_label(expr)));
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
