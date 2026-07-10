//! A `libsqlite3`-compatible C ABI (subset) over [graphitesql](https://crates.io/crates/graphitesql).
//!
//! This is the ROADMAP **D7 (C-API shim)** track. It exports `extern "C"`
//! functions with the same names, signatures and result codes as SQLite's C API,
//! so existing C/C++/FFI code that links `libsqlite3` can link this instead and
//! drive graphitesql's engine.
//!
//! # Why a separate crate
//!
//! The core `graphitesql` crate is `#![forbid(unsafe_code)]`, `no_std`+alloc and
//! zero-dependency. A C ABI needs `extern "C"`, raw pointers and `unsafe`, so this
//! shim lives in its own workspace and opts out — exactly like `graphitesql-wasm`.
//!
//! # Scope
//!
//! The connection/statement lifecycle and the common read/write path:
//! `open`/`open_v2`/`close`, `exec`, `prepare_v2`/`step`/`reset`/`finalize`,
//! `bind_*`, `column_*`, `errmsg`/`errcode`, `changes`, `last_insert_rowid`,
//! `libversion`. Prepared statements are emulated over graphitesql's materialized
//! query model (a `step` walks the already-computed rows), which is behaviourally
//! equivalent to SQLite's incremental VDBE stepping for these entry points.
//!
//! Not (yet) covered: the `_v3` prepare variant flags, incremental BLOB I/O,
//! backup, the authorizer/hooks, and UTF-16 entry points.

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
// C ABI names are fixed by SQLite; silence Rust's "fn arg" style lints on them.
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_double, c_int, c_longlong, c_uchar, c_void};
use graphitesql::exec::eval::Params;
use graphitesql::{Connection, QueryResult, Value};
use std::ffi::{CStr, CString};

// --- Result codes (subset of sqlite3.h) ---------------------------------------
pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ERROR: c_int = 1;
pub const SQLITE_NOMEM: c_int = 7;
pub const SQLITE_RANGE: c_int = 25;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;

// --- Fundamental datatypes ----------------------------------------------------
pub const SQLITE_INTEGER: c_int = 1;
pub const SQLITE_FLOAT: c_int = 2;
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_BLOB: c_int = 4;
pub const SQLITE_NULL: c_int = 5;

// Text encodings (only UTF-8 is meaningful here).
pub const SQLITE_UTF8: c_int = 1;

/// Destructor sentinels for `bind_text`/`bind_blob`.
pub const SQLITE_STATIC: isize = 0;
pub const SQLITE_TRANSIENT: isize = -1;

const LIBVERSION: &CStr = c"3.50.4";
const LIBVERSION_NUMBER: c_int = 3_050_004;

/// A database connection. Mirrors the opaque `sqlite3` handle.
pub struct sqlite3 {
    conn: Connection,
    errmsg: Option<CString>,
    errcode: c_int,
    changes: c_int,
    last_insert_rowid: c_longlong,
}

impl sqlite3 {
    fn set_error(&mut self, code: c_int, msg: &str) {
        self.errcode = code;
        self.errmsg = Some(CString::new(msg).unwrap_or_default());
    }
    fn clear_error(&mut self) {
        self.errcode = SQLITE_OK;
        self.errmsg = None;
    }
}

/// A prepared statement. Mirrors the opaque `sqlite3_stmt` handle.
pub struct sqlite3_stmt {
    db: *mut sqlite3,
    sql: String,
    params: Params,
    /// Materialized rows for a row-producing statement (set on first `step`).
    result: Option<QueryResult>,
    /// Index of the row a subsequent `column_*` reads (the last row `step` returned).
    cur: Option<usize>,
    /// Next row `step` will return.
    next: usize,
    executed: bool,
    /// Per-parameter slot metadata by SQLite numbering (index 0 = parameter 1):
    /// `Some(":name")` for a named parameter (bind routes to `params.named`),
    /// `None` for an anonymous `?` or numbered `?N` (routes to `params.positional`).
    /// Length is `sqlite3_bind_parameter_count`.
    param_names: Vec<Option<String>>,
    /// Scratch for `sqlite3_bind_parameter_name`'s returned pointer.
    param_name_scratch: Option<CString>,
    /// Backing storage keeping `column_text`/`column_blob` pointers valid until the
    /// next `step`/`reset`/`finalize`, per SQLite's lifetime contract.
    text_scratch: Vec<Option<CString>>,
    blob_scratch: Vec<Option<Vec<u8>>>,
}

impl sqlite3_stmt {
    fn reset_run(&mut self) {
        self.result = None;
        self.cur = None;
        self.next = 0;
        self.executed = false;
        self.text_scratch.clear();
        self.blob_scratch.clear();
    }
}

/// A pure read (`step` → `SQLITE_ROW`), dispatched by leading keyword — matching
/// how SQLite classifies at prepare time. An `INSERT/UPDATE/DELETE … RETURNING`
/// also produces rows but is handled separately (see [`has_returning`]).
fn is_row_producer(sql: &str) -> bool {
    let kw: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .flat_map(|c| c.to_uppercase())
        .collect();
    matches!(
        kw.as_str(),
        "SELECT" | "WITH" | "VALUES" | "PRAGMA" | "EXPLAIN"
    )
}

/// True if `sql` is an `INSERT/UPDATE/DELETE` with a non-empty `RETURNING` clause.
/// Determined structurally via the engine's parser (not a text scan), so a column
/// or literal spelled "returning" is never mistaken for the clause. Anything that
/// doesn't parse as an IUD returns false (it'll route to the mutation path, which
/// reports the same parse error).
fn has_returning(sql: &str) -> bool {
    use graphitesql::sql::ast::Statement;
    match graphitesql::sql::parser::parse_one(sql) {
        Ok(Statement::Insert(i)) => !i.returning.is_empty(),
        Ok(Statement::Update(u)) => !u.returning.is_empty(),
        Ok(Statement::Delete(d)) => !d.returning.is_empty(),
        _ => false,
    }
}

/// Scan a statement for bind parameters, assigning SQLite's numbering, and return
/// the slot table (index 0 = parameter 1): `Some(":name")` for a named parameter,
/// `None` for an anonymous `?` or a numbered `?N`. The length is the parameter
/// count (the highest number reached). Quoting and comments are skipped.
fn scan_params(sql: &str) -> Vec<Option<String>> {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut next_auto = 1usize;
    let mut slots: Vec<Option<String>> = Vec::new();
    let ensure = |slots: &mut Vec<Option<String>>, num: usize| {
        if num > slots.len() {
            slots.resize(num, None);
        }
    };
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'?' => {
                i += 1;
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i > start {
                    if let Ok(num) = sql[start..i].parse::<usize>()
                        && num > 0
                    {
                        ensure(&mut slots, num);
                        next_auto = next_auto.max(num + 1);
                    }
                } else {
                    ensure(&mut slots, next_auto);
                    next_auto += 1;
                }
            }
            b':' | b'@' | b'$' => {
                let start = i;
                i += 1;
                let nstart = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                if i > nstart {
                    let name = &sql[start..i]; // includes the sigil
                    if !slots.iter().any(|s| s.as_deref() == Some(name)) {
                        ensure(&mut slots, next_auto);
                        slots[next_auto - 1] = Some(name.to_string());
                        next_auto += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    slots
}

// --- helpers ------------------------------------------------------------------

/// Borrow a C string as `&str`; empty on NULL/invalid UTF-8.
unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
}

fn value_type(v: &Value) -> c_int {
    match v {
        Value::Null => SQLITE_NULL,
        Value::Integer(_) => SQLITE_INTEGER,
        Value::Real(_) => SQLITE_FLOAT,
        Value::Text(_) => SQLITE_TEXT,
        Value::Blob(_) => SQLITE_BLOB,
    }
}

/// SQLite's `sqlite3_column_int64` coercion.
fn value_to_i64(v: &Value) -> c_longlong {
    match v {
        Value::Null => 0,
        Value::Integer(i) => *i,
        Value::Real(f) => *f as c_longlong,
        Value::Text(s) => text_prefix_i64(s),
        Value::Blob(b) => text_prefix_i64(&String::from_utf8_lossy(b)),
    }
}

fn value_to_f64(v: &Value) -> c_double {
    match v {
        Value::Null => 0.0,
        Value::Integer(i) => *i as c_double,
        Value::Real(f) => *f,
        Value::Text(s) => s.trim().parse().unwrap_or(0.0),
        Value::Blob(b) => String::from_utf8_lossy(b).trim().parse().unwrap_or(0.0),
    }
}

/// Leading-integer parse (SQLite reads the longest numeric prefix).
fn text_prefix_i64(s: &str) -> c_longlong {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let start_digits = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start_digits {
        return 0;
    }
    t[..i].parse().unwrap_or(0)
}

/// The text SQLite's `sqlite3_column_text` yields (numeric classes stringify).
fn value_to_text(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Integer(i) => Some(i.to_string().into_bytes()),
        Value::Real(f) => Some(graphitesql::exec::eval::format_real(*f).into_bytes()),
        Value::Text(s) => Some(s.clone().into_bytes()),
        Value::Blob(b) => Some(b.clone()),
    }
}

// --- version ------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_libversion() -> *const c_char {
    LIBVERSION.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_libversion_number() -> c_int {
    LIBVERSION_NUMBER
}

// --- open / close -------------------------------------------------------------

fn open_connection(path: &str) -> Result<Connection, String> {
    if path.is_empty() || path == ":memory:" {
        return Connection::open_memory().map_err(|e| e.to_string());
    }
    // Open if it exists, else create — SQLite's default behaviour.
    match Connection::open(path) {
        Ok(c) => Ok(c),
        Err(_) => Connection::create(path).map_err(|e| e.to_string()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_open(filename: *const c_char, pp_db: *mut *mut sqlite3) -> c_int {
    unsafe { sqlite3_open_v2(filename, pp_db, 0, core::ptr::null()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    pp_db: *mut *mut sqlite3,
    _flags: c_int,
    _vfs: *const c_char,
) -> c_int {
    if pp_db.is_null() {
        return SQLITE_ERROR;
    }
    let path = unsafe { cstr(filename) };
    match open_connection(path) {
        Ok(conn) => {
            let db = Box::new(sqlite3 {
                conn,
                errmsg: None,
                errcode: SQLITE_OK,
                changes: 0,
                last_insert_rowid: 0,
            });
            unsafe { *pp_db = Box::into_raw(db) };
            SQLITE_OK
        }
        Err(msg) => {
            // On failure SQLite still allocates a handle carrying the error.
            let mut db = Box::new(sqlite3 {
                conn: Connection::open_memory().expect("in-memory always opens"),
                errmsg: None,
                errcode: SQLITE_ERROR,
                changes: 0,
                last_insert_rowid: 0,
            });
            db.set_error(SQLITE_ERROR, &msg);
            unsafe { *pp_db = Box::into_raw(db) };
            SQLITE_ERROR
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_close(db: *mut sqlite3) -> c_int {
    if !db.is_null() {
        drop(unsafe { Box::from_raw(db) });
    }
    SQLITE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_close_v2(db: *mut sqlite3) -> c_int {
    unsafe { sqlite3_close(db) }
}

// --- error / status -----------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char {
    if db.is_null() {
        return c"out of memory".as_ptr();
    }
    let db = unsafe { &*db };
    match &db.errmsg {
        Some(m) => m.as_ptr(),
        None => c"not an error".as_ptr(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_errcode(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return SQLITE_ERROR;
    }
    unsafe { &*db }.errcode
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_changes(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return 0;
    }
    unsafe { &*db }.changes
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_last_insert_rowid(db: *mut sqlite3) -> c_longlong {
    if db.is_null() {
        return 0;
    }
    unsafe { &*db }.last_insert_rowid
}

// --- exec ---------------------------------------------------------------------

type ExecCallback =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    if db.is_null() {
        return SQLITE_ERROR;
    }
    let db = unsafe { &mut *db };
    db.clear_error();
    let sql = unsafe { cstr(sql) };

    // Run each `;`-separated statement in turn (SQLite's exec loops over the tail).
    for stmt in split_statements(sql) {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        // Three cases: a pure reader (SELECT/… — rows for the callback, `changes`
        // untouched), an IUD…RETURNING (rows for the callback AND `changes`), or a
        // plain mutation (no rows, `changes` only).
        let is_reader = is_row_producer(stmt);
        let outcome = if is_reader {
            db.conn.query(stmt).map(Some)
        } else if has_returning(stmt) {
            db.conn
                .execute_returning(stmt, &Params::default())
                .map(Some)
        } else {
            db.conn.execute(stmt).map(|n| {
                db.changes = n as c_int;
                None
            })
        };
        match outcome {
            Ok(maybe_qr) => {
                db.last_insert_rowid = db.conn.last_insert_rowid();
                if let Some(qr) = maybe_qr {
                    if !is_reader {
                        // IUD…RETURNING: one row per affected row.
                        db.changes = qr.rows.len() as c_int;
                    }
                    if let Some(cb) = callback
                        && invoke_exec_callback(cb, arg, &qr) != SQLITE_OK
                    {
                        db.set_error(SQLITE_ERROR, "callback requested abort");
                        return SQLITE_ERROR;
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                db.set_error(SQLITE_ERROR, &msg);
                unsafe { write_errmsg(errmsg, &msg) };
                return SQLITE_ERROR;
            }
        }
    }
    SQLITE_OK
}

fn invoke_exec_callback(
    cb: unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
    arg: *mut c_void,
    qr: &QueryResult,
) -> c_int {
    // Column names are shared across all rows.
    let names: Vec<CString> = qr
        .columns
        .iter()
        .map(|c| CString::new(c.as_str()).unwrap_or_default())
        .collect();
    let mut name_ptrs: Vec<*mut c_char> = names.iter().map(|c| c.as_ptr() as *mut c_char).collect();
    for row in &qr.rows {
        let cells: Vec<Option<CString>> = row
            .iter()
            .map(|v| value_to_text(v).map(|b| CString::new(b).unwrap_or_default()))
            .collect();
        let mut cell_ptrs: Vec<*mut c_char> = cells
            .iter()
            .map(|c| match c {
                Some(s) => s.as_ptr() as *mut c_char,
                None => core::ptr::null_mut(),
            })
            .collect();
        let rc = unsafe {
            cb(
                arg,
                qr.columns.len() as c_int,
                cell_ptrs.as_mut_ptr(),
                name_ptrs.as_mut_ptr(),
            )
        };
        if rc != SQLITE_OK {
            return SQLITE_ERROR;
        }
    }
    SQLITE_OK
}

/// Write an owned copy of `msg` into `*errmsg` (freed by `sqlite3_free`).
unsafe fn write_errmsg(errmsg: *mut *mut c_char, msg: &str) {
    if errmsg.is_null() {
        return;
    }
    let c = CString::new(msg).unwrap_or_default();
    unsafe { *errmsg = c.into_raw() };
}

// --- prepare / step / finalize ------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    db: *mut sqlite3,
    sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    if db.is_null() || pp_stmt.is_null() {
        return SQLITE_ERROR;
    }
    unsafe { *pp_stmt = core::ptr::null_mut() };
    let db_ref = unsafe { &mut *db };
    db_ref.clear_error();

    // Honour an explicit byte length; otherwise NUL-terminated.
    let full = unsafe { cstr(sql) };
    let text = if n_byte < 0 {
        full
    } else {
        let n = (n_byte as usize).min(full.len());
        &full[..n]
    };

    // SQLite compiles ONE statement and points pz_tail at the rest.
    let (head, tail_off) = first_statement(text);
    if !pz_tail.is_null() {
        // Point into the caller's buffer at the tail offset.
        unsafe { *pz_tail = sql.add(tail_off) };
    }
    if head.trim().is_empty() {
        // Empty statement: a NULL stmt with SQLITE_OK, like SQLite.
        return SQLITE_OK;
    }

    let stmt = Box::new(sqlite3_stmt {
        db,
        param_names: scan_params(head),
        sql: head.to_string(),
        params: Params::default(),
        result: None,
        cur: None,
        next: 0,
        executed: false,
        param_name_scratch: None,
        text_scratch: Vec::new(),
        blob_scratch: Vec::new(),
    });
    unsafe { *pp_stmt = Box::into_raw(stmt) };
    SQLITE_OK
}

/// Run the statement if it hasn't run yet. A row-producer populates `result`
/// (so column metadata is available before the first `step`, like SQLite); a
/// mutation executes and records `changes`/`last_insert_rowid`. Idempotent.
fn ensure_executed(stmt: &mut sqlite3_stmt) -> c_int {
    if stmt.executed {
        return SQLITE_OK;
    }
    stmt.executed = true;
    let db = unsafe { &mut *stmt.db };
    if is_row_producer(&stmt.sql) {
        // Pure read: SELECT/WITH/VALUES/PRAGMA/EXPLAIN.
        match db.conn.query_params(&stmt.sql, &stmt.params) {
            Ok(qr) => {
                stmt.result = Some(qr);
                SQLITE_OK
            }
            Err(e) => {
                db.set_error(SQLITE_ERROR, &e.to_string());
                SQLITE_ERROR
            }
        }
    } else if has_returning(&stmt.sql) {
        // INSERT/UPDATE/DELETE … RETURNING: a mutation that also yields rows.
        match db.conn.execute_returning(&stmt.sql, &stmt.params) {
            Ok(qr) => {
                db.changes = qr.rows.len() as c_int;
                db.last_insert_rowid = db.conn.last_insert_rowid();
                stmt.result = Some(qr);
                SQLITE_OK
            }
            Err(e) => {
                db.set_error(SQLITE_ERROR, &e.to_string());
                SQLITE_ERROR
            }
        }
    } else {
        // Plain mutation/DDL.
        match db.conn.execute_params(&stmt.sql, &stmt.params) {
            Ok(n) => {
                db.changes = n as c_int;
                db.last_insert_rowid = db.conn.last_insert_rowid();
                SQLITE_OK
            }
            Err(e) => {
                db.set_error(SQLITE_ERROR, &e.to_string());
                SQLITE_ERROR
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    let stmt = unsafe { &mut *stmt };
    if ensure_executed(stmt) != SQLITE_OK {
        return SQLITE_ERROR;
    }
    match &stmt.result {
        // Row-producer: walk the materialized rows.
        Some(qr) if stmt.next < qr.rows.len() => {
            stmt.cur = Some(stmt.next);
            stmt.next += 1;
            // Invalidate the previous row's text/blob scratch.
            stmt.text_scratch.clear();
            stmt.blob_scratch.clear();
            SQLITE_ROW
        }
        // Exhausted row-producer, or a mutation (result stays None) — done.
        _ => SQLITE_DONE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    unsafe { &mut *stmt }.reset_run();
    SQLITE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_clear_bindings(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    unsafe { &mut *stmt }.params = Params::default();
    SQLITE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int {
    if !stmt.is_null() {
        drop(unsafe { Box::from_raw(stmt) });
    }
    SQLITE_OK
}

// --- bind ---------------------------------------------------------------------

/// Bind parameter `idx` (1-based). Routes to `params.named` when that slot is a
/// named parameter (`:x`/`@x`/`$x`), else to `params.positional`. Out-of-range
/// against the statement's known parameter count yields `SQLITE_RANGE`.
fn bind_at(stmt: &mut sqlite3_stmt, idx: c_int, v: Value) -> c_int {
    if idx < 1 || idx as usize > stmt.param_names.len() {
        return SQLITE_RANGE;
    }
    let i = (idx - 1) as usize;
    match &stmt.param_names[i] {
        Some(name) => {
            let name = name.clone();
            match stmt.params.named.iter_mut().find(|(k, _)| *k == name) {
                Some(slot) => slot.1 = v,
                None => stmt.params.named.push((name, v)),
            }
        }
        None => {
            if stmt.params.positional.len() <= i {
                stmt.params.positional.resize(i + 1, Value::Null);
            }
            stmt.params.positional[i] = v;
        }
    }
    SQLITE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_int(stmt: *mut sqlite3_stmt, idx: c_int, v: c_int) -> c_int {
    unsafe { sqlite3_bind_int64(stmt, idx, v as c_longlong) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_int64(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    v: c_longlong,
) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    bind_at(unsafe { &mut *stmt }, idx, Value::Integer(v))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_double(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    v: c_double,
) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    bind_at(unsafe { &mut *stmt }, idx, Value::Real(v))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    bind_at(unsafe { &mut *stmt }, idx, Value::Null)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_text(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    text: *const c_char,
    n_byte: c_int,
    _destructor: isize,
) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    let s = if text.is_null() {
        String::new()
    } else if n_byte < 0 {
        unsafe { cstr(text) }.to_string()
    } else {
        let bytes = unsafe { core::slice::from_raw_parts(text as *const u8, n_byte as usize) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    bind_at(unsafe { &mut *stmt }, idx, Value::Text(s))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_blob(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    data: *const c_void,
    n_byte: c_int,
    _destructor: isize,
) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    let bytes = if data.is_null() || n_byte <= 0 {
        Vec::new()
    } else {
        unsafe { core::slice::from_raw_parts(data as *const u8, n_byte as usize) }.to_vec()
    };
    bind_at(unsafe { &mut *stmt }, idx, Value::Blob(bytes))
}

// --- parameter introspection --------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_parameter_count(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    unsafe { &*stmt }.param_names.len() as c_int
}

/// Name (with sigil) of parameter `idx` (1-based), or NULL for an anonymous `?`
/// / numbered `?N` / out-of-range index.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_parameter_name(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    if stmt.is_null() || idx < 1 {
        return core::ptr::null();
    }
    let stmt = unsafe { &mut *stmt };
    match stmt.param_names.get((idx - 1) as usize) {
        Some(Some(name)) => {
            let c = CString::new(name.as_str()).unwrap_or_default();
            let p = c.as_ptr();
            stmt.param_name_scratch = Some(c);
            p
        }
        _ => core::ptr::null(),
    }
}

/// 1-based index of the named parameter `name` (given with its sigil, e.g. `:id`),
/// or 0 if there is no such parameter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_parameter_index(
    stmt: *mut sqlite3_stmt,
    name: *const c_char,
) -> c_int {
    if stmt.is_null() || name.is_null() {
        return 0;
    }
    let want = unsafe { cstr(name) };
    let stmt = unsafe { &*stmt };
    for (i, slot) in stmt.param_names.iter().enumerate() {
        if slot.as_deref() == Some(want) {
            return (i + 1) as c_int;
        }
    }
    0
}

/// Number of columns in the current result row: `column_count` while a row is
/// available (after `step` → `SQLITE_ROW`), else 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_data_count(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    let stmt = unsafe { &*stmt };
    match (&stmt.result, stmt.cur) {
        (Some(qr), Some(_)) => qr.columns.len() as c_int,
        _ => 0,
    }
}

// --- column accessors ---------------------------------------------------------

fn stmt_cell(stmt: &sqlite3_stmt, col: c_int) -> Option<&Value> {
    let cur = stmt.cur?;
    let qr = stmt.result.as_ref()?;
    let row = qr.rows.get(cur)?;
    row.get(col as usize)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    let stmt = unsafe { &mut *stmt };
    // Column metadata is available before the first `step` — but only a
    // row-producer has columns, and materializing it must not run a mutation.
    if is_row_producer(&stmt.sql) {
        let _ = ensure_executed(stmt);
    }
    match &stmt.result {
        Some(qr) => qr.columns.len() as c_int,
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_name(stmt: *mut sqlite3_stmt, col: c_int) -> *const c_char {
    if stmt.is_null() {
        return core::ptr::null();
    }
    let stmt = unsafe { &mut *stmt };
    if is_row_producer(&stmt.sql) {
        let _ = ensure_executed(stmt);
    }
    let name = match &stmt.result {
        Some(qr) => match qr.columns.get(col as usize) {
            Some(n) => n.clone(),
            None => return core::ptr::null(),
        },
        None => return core::ptr::null(),
    };
    // Cache in scratch so the pointer stays valid for the statement's lifetime.
    let idx = col as usize;
    if stmt.text_scratch.len() <= idx {
        stmt.text_scratch.resize(idx + 1, None);
    }
    let c = CString::new(name).unwrap_or_default();
    let p = c.as_ptr();
    stmt.text_scratch[idx] = Some(c);
    p
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_type(stmt: *mut sqlite3_stmt, col: c_int) -> c_int {
    if stmt.is_null() {
        return SQLITE_NULL;
    }
    match stmt_cell(unsafe { &*stmt }, col) {
        Some(v) => value_type(v),
        None => SQLITE_NULL,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_int(stmt: *mut sqlite3_stmt, col: c_int) -> c_int {
    unsafe { sqlite3_column_int64(stmt, col) as c_int }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, col: c_int) -> c_longlong {
    if stmt.is_null() {
        return 0;
    }
    match stmt_cell(unsafe { &*stmt }, col) {
        Some(v) => value_to_i64(v),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_double(stmt: *mut sqlite3_stmt, col: c_int) -> c_double {
    if stmt.is_null() {
        return 0.0;
    }
    match stmt_cell(unsafe { &*stmt }, col) {
        Some(v) => value_to_f64(v),
        None => 0.0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_text(
    stmt: *mut sqlite3_stmt,
    col: c_int,
) -> *const c_uchar {
    if stmt.is_null() {
        return core::ptr::null();
    }
    let stmt = unsafe { &mut *stmt };
    let bytes = match stmt_cell(stmt, col) {
        Some(v) => match value_to_text(v) {
            Some(b) => b,
            None => return core::ptr::null(), // NULL column
        },
        None => return core::ptr::null(),
    };
    let idx = col as usize;
    if stmt.text_scratch.len() <= idx {
        stmt.text_scratch.resize(idx + 1, None);
    }
    // NUL-terminate for C string consumers.
    let c = CString::new(bytes).unwrap_or_default();
    let p = c.as_ptr() as *const c_uchar;
    stmt.text_scratch[idx] = Some(c);
    p
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_blob(stmt: *mut sqlite3_stmt, col: c_int) -> *const c_void {
    if stmt.is_null() {
        return core::ptr::null();
    }
    let stmt = unsafe { &mut *stmt };
    let bytes = match stmt_cell(stmt, col) {
        Some(Value::Blob(b)) => b.clone(),
        Some(Value::Text(s)) => s.clone().into_bytes(),
        Some(Value::Null) | None => return core::ptr::null(),
        Some(other) => match value_to_text(other) {
            Some(b) => b,
            None => return core::ptr::null(),
        },
    };
    let idx = col as usize;
    if stmt.blob_scratch.len() <= idx {
        stmt.blob_scratch.resize(idx + 1, None);
    }
    let p = bytes.as_ptr() as *const c_void;
    stmt.blob_scratch[idx] = Some(bytes);
    p
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_bytes(stmt: *mut sqlite3_stmt, col: c_int) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    match stmt_cell(unsafe { &*stmt }, col) {
        Some(v) => value_to_text(v).map(|b| b.len() as c_int).unwrap_or(0),
        None => 0,
    }
}

// --- memory -------------------------------------------------------------------

/// Free memory handed to the caller by this library (`errmsg` from `exec`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_free(p: *mut c_void) {
    if !p.is_null() {
        drop(unsafe { CString::from_raw(p as *mut c_char) });
    }
}

// --- statement splitting ------------------------------------------------------

/// Return `(first_statement, byte_offset_of_tail)` — the first `;`-terminated
/// statement (respecting string/identifier quoting and comments) and where the
/// rest begins.
fn first_statement(sql: &str) -> (&str, usize) {
    match statement_end(sql) {
        Some(end) => (&sql[..end], end),
        None => (sql, sql.len()),
    }
}

/// Split a batch into `;`-separated statements (quoting/comment aware).
fn split_statements(sql: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = sql;
    while !rest.is_empty() {
        match statement_end(rest) {
            Some(end) => {
                out.push(&rest[..end]);
                rest = &rest[end..];
                // Skip the delimiter itself.
                if rest.starts_with(';') {
                    rest = &rest[1..];
                }
            }
            None => {
                out.push(rest);
                break;
            }
        }
    }
    out
}

/// Byte offset just past the first top-level `;` (INCLUSIVE of the `;`), or None.
fn statement_end(sql: &str) -> Option<usize> {
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        // Doubled quote is an escape.
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b';' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}
