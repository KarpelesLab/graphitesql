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

pub mod eval;
pub mod func;

use crate::btree::{
    clear_index, create_index_root, create_table_root, delete_table, free_tree, insert_index,
    insert_table, TableCursor,
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
}

impl Connection {
    fn from_pager(db: WritePager) -> Result<Connection> {
        let backend = Backend::Write(db);
        let schema = Schema::read(backend.source())?;
        Ok(Connection {
            backend,
            schema,
            in_tx: false,
        })
    }

    fn from_read_backend(backend: Box<dyn PageSource>) -> Result<Connection> {
        let backend = Backend::Read(backend);
        let schema = Schema::read(backend.source())?;
        Ok(Connection {
            backend,
            schema,
            in_tx: false,
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
            Statement::Drop(d) => {
                self.exec_drop(&d)?;
                0
            }
            Statement::Alter(a) => {
                self.exec_alter(&a)?;
                0
            }
            Statement::Pragma(_) => 0, // accepted, no-op for now
            Statement::Vacuum => 0,    // accepted; compaction is a no-op here
            Statement::Select(_) => return Err(Error::Unsupported("use query() for SELECT")),
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
        if ct.without_rowid {
            return Err(Error::Unsupported("WITHOUT ROWID tables"));
        }
        let root = create_table_root(self.backend.writer()?)?;
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

    fn exec_insert(&mut self, ins: &Insert, params: &Params) -> Result<usize> {
        let rows = match &ins.source {
            InsertSource::Values(rows) => rows.clone(),
            InsertSource::DefaultValues => alloc::vec![Vec::new()],
            InsertSource::Select(_) => return Err(Error::Unsupported("INSERT ... SELECT")),
        };
        let meta = self.table_meta(&ins.table, None)?;
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
        for row_exprs in &rows {
            if !ins.columns.is_empty() && row_exprs.len() != target.len() {
                return Err(Error::Error("INSERT column/value count mismatch".into()));
            }
            // Start every column at its DEFAULT (or NULL), then apply provided.
            let ctx = EvalCtx::rowless(params);
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
            let index_values = values.clone();
            if let Some(ipk) = meta.ipk {
                values[ipk] = Value::Null;
            }
            let record = encode_record(&values);
            insert_table(self.backend.writer()?, meta.root, rowid, &record)?;
            for idx in &indexes {
                let key = index_key(&idx.cols, &index_values, rowid);
                insert_index(self.backend.writer()?, idx.root, &key)?;
            }
            affected += 1;
        }
        Ok(affected)
    }

    fn exec_delete(&mut self, del: &Delete, params: &Params) -> Result<usize> {
        let meta = self.table_meta(&del.table, None)?;
        let indexes = self.indexes_of(&del.table)?;
        let victims = self.matching_rowids(&meta, del.where_clause.as_ref(), params)?;
        for rowid in &victims {
            delete_table(self.backend.writer()?, meta.root, *rowid)?;
        }
        if !victims.is_empty() {
            self.rebuild_indexes(&meta, &indexes)?;
        }
        Ok(victims.len())
    }

    fn exec_update(&mut self, upd: &Update, params: &Params) -> Result<usize> {
        let meta = self.table_meta(&upd.table, None)?;
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
            check_not_null(&meta, &values)?;
            // New rowid if the IPK column was changed, else unchanged.
            let new_rowid = match meta.ipk {
                Some(ipk) => eval::to_i64(&values[ipk]),
                None => rowid,
            };
            if let Some(ipk) = meta.ipk {
                values[ipk] = Value::Null;
            }
            let record = encode_record(&values);
            delete_table(self.backend.writer()?, meta.root, rowid)?;
            insert_table(self.backend.writer()?, meta.root, new_rowid, &record)?;
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
            AlterAction::RenameColumn { .. } => {
                return Err(Error::Unsupported("ALTER TABLE RENAME COLUMN"));
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
    fn indexes_of(&self, table: &str) -> Result<Vec<IndexMeta>> {
        let tmeta = match self.schema.table(table) {
            Some(_) => self.table_meta(table, None)?,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for obj in self.schema.indexes_on(table) {
            let Some(sql) = &obj.sql else { continue }; // skip auto-indexes
            let Statement::CreateIndex(ci) = sql::parse_one(sql)? else {
                continue;
            };
            let cols = self.index_columns(&tmeta, &ci)?;
            out.push(IndexMeta {
                root: obj.rootpage,
                cols,
            });
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
        let (columns, input_rows) = self.scan_source(sel, params)?;

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
        // A view as the sole source: run its SELECT in place.
        if from.joins.is_empty() {
            if let Some((columns, rows)) =
                self.try_view(&from.first.name, from.first.alias.as_deref(), params)?
            {
                return Ok((columns, rows));
            }
        } else if self.try_view(&from.first.name, None, params)?.is_some() {
            return Err(Error::Unsupported("views in joins"));
        }

        // Scan the first table.
        let first_meta = self.table_meta(&from.first.name, from.first.alias.as_deref())?;
        let first_rows = self.scan_table(&first_meta)?;

        // Single-table fast path: preserve the rowid for `rowid` references.
        if from.joins.is_empty() {
            let input_rows = first_rows
                .into_iter()
                .map(|(rowid, values)| InputRow {
                    values,
                    rowid: Some(rowid),
                })
                .collect();
            return Ok((first_meta.columns, input_rows));
        }

        let mut columns = first_meta.columns.clone();
        let mut rows: Vec<Vec<Value>> = first_rows.into_iter().map(|(_, v)| v).collect();

        // Fold each join in with a nested-loop, evaluating its ON predicate
        // against the columns accumulated so far plus the joined table's.
        for join in &from.joins {
            let jmeta = self.table_meta(&join.table.name, join.table.alias.as_deref())?;
            let jrows: Vec<Vec<Value>> = self
                .scan_table(&jmeta)?
                .into_iter()
                .map(|(_, v)| v)
                .collect();

            let mut new_columns = columns.clone();
            new_columns.extend(jmeta.columns.iter().cloned());
            let n_jcols = jmeta.columns.len();

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
            } if func::is_aggregate(name) => {
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
        if ct.without_rowid {
            return Err(Error::Unsupported("WITHOUT ROWID tables"));
        }
        let table_label = alias.unwrap_or(name).to_string();
        let columns: Vec<ColumnInfo> = ct
            .columns
            .iter()
            .map(|c| ColumnInfo {
                name: c.name.clone(),
                table: table_label.clone(),
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
        let ipk = find_integer_primary_key(&ct);
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
        Ok(TableMeta {
            root: obj.rootpage,
            columns,
            defaults,
            not_null,
            ipk,
        })
    }
}

struct TableMeta {
    root: u32,
    columns: Vec<ColumnInfo>,
    /// Per-column `DEFAULT` expression, if declared (aligned with `columns`).
    defaults: Vec<Option<Expr>>,
    /// Per-column `NOT NULL` flag (aligned with `columns`).
    not_null: Vec<bool>,
    ipk: Option<usize>,
}

/// An index's b-tree root and the table column positions it covers.
struct IndexMeta {
    root: u32,
    cols: Vec<usize>,
}

impl eval::Subqueries for Connection {
    fn scalar(&self, select: &Select) -> Result<Value> {
        let r = self.run_select(select, &Params::default())?;
        Ok(r.rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(Value::Null))
    }

    fn column(&self, select: &Select) -> Result<Vec<Value>> {
        let r = self.run_select(select, &Params::default())?;
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
    }
}

/// Whether a value is the given text (used to match `sqlite_schema` columns).
fn is_text(v: &Value, s: &str) -> bool {
    matches!(v, Value::Text(t) if t == s)
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
        Expr::Function { name, args, .. } => {
            func::is_aggregate(name) || args.iter().any(contains_aggregate)
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
