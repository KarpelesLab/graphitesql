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
//! Not (yet) covered: incremental BLOB I/O, online backup, and the
//! authorizer/hooks.

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
// C ABI names are fixed by SQLite; silence Rust's "fn arg" style lints on them.
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_double, c_int, c_longlong, c_uchar, c_void};
use graphitesql::exec::eval::Params;
use graphitesql::{Connection, QueryResult, UpdateOp, Value};
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
    /// Scratch for `sqlite3_errmsg16`'s returned pointer (UTF-16, NUL-terminated).
    errmsg16: Option<Vec<u16>>,
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
    /// Scratch for `sqlite3_sql`'s returned pointer.
    sql_cstr: Option<CString>,
    /// Backing storage keeping `column_text`/`column_blob` pointers valid until the
    /// next `step`/`reset`/`finalize`, per SQLite's lifetime contract.
    text_scratch: Vec<Option<CString>>,
    blob_scratch: Vec<Option<Vec<u8>>>,
    /// Same, for `column_text16` (UTF-16, NUL-terminated).
    text16_scratch: Vec<Option<Vec<u16>>>,
}

impl sqlite3_stmt {
    fn reset_run(&mut self) {
        self.result = None;
        self.cur = None;
        self.next = 0;
        self.executed = false;
        self.text_scratch.clear();
        self.blob_scratch.clear();
        self.text16_scratch.clear();
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
                errmsg16: None,
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
                errmsg16: None,
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_total_changes(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return 0;
    }
    unsafe { &*db }.conn.total_changes() as c_int
}

/// 1 in autocommit mode, 0 inside an explicit `BEGIN`…`COMMIT` transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_get_autocommit(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return 1;
    }
    unsafe { &*db }.conn.is_autocommit() as c_int
}

/// This shim does not model SQLite's extended result codes, so this returns the
/// primary code (identical to `sqlite3_errcode`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_extended_errcode(db: *mut sqlite3) -> c_int {
    unsafe { sqlite3_errcode(db) }
}

/// No-op: locking is process-local, so a connection never sees a busy file lock
/// held by another OS process to wait on. Accepted for API compatibility.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_busy_timeout(_db: *mut sqlite3, _ms: c_int) -> c_int {
    SQLITE_OK
}

/// No-op: this shim runs statements to completion synchronously, so there is no
/// in-flight step to interrupt.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_interrupt(_db: *mut sqlite3) {}

/// English text for a primary result code (like `sqlite3_errstr`).
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_errstr(rc: c_int) -> *const c_char {
    let s: &CStr = match rc {
        SQLITE_OK => c"not an error",
        SQLITE_ERROR => c"SQL logic error",
        SQLITE_NOMEM => c"out of memory",
        SQLITE_RANGE => c"column index out of range",
        SQLITE_ROW => c"another row available",
        SQLITE_DONE => c"no more rows available",
        _ => c"unknown error",
    };
    s.as_ptr()
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

/// Like `sqlite3_prepare_v2` with a `prepFlags` argument; the flags (persistent,
/// no-vtab, normalize) don't affect this shim's materialized model, so it simply
/// delegates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_prepare_v3(
    db: *mut sqlite3,
    sql: *const c_char,
    n_byte: c_int,
    _prep_flags: core::ffi::c_uint,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    unsafe { sqlite3_prepare_v2(db, sql, n_byte, pp_stmt, pz_tail) }
}

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
        sql_cstr: None,
        text_scratch: Vec::new(),
        blob_scratch: Vec::new(),
        text16_scratch: Vec::new(),
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

/// The connection that prepared this statement (`sqlite3_db_handle`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3 {
    if stmt.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { &*stmt }.db
}

/// The original SQL text of the prepared statement (`sqlite3_sql`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_sql(stmt: *mut sqlite3_stmt) -> *const c_char {
    if stmt.is_null() {
        return core::ptr::null();
    }
    let stmt = unsafe { &mut *stmt };
    let c = CString::new(stmt.sql.as_str()).unwrap_or_default();
    let p = c.as_ptr();
    stmt.sql_cstr = Some(c);
    p
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

// --- user-defined scalar functions -------------------------------------------

/// A protected value passed to a UDF (`sqlite3_value*`). Owns its `Value` so
/// `sqlite3_value_text`/`_blob` can hand back a stable pointer for the call.
pub struct sqlite3_value {
    v: Value,
    scratch: Option<CString>,
}

/// The call context of a UDF (`sqlite3_context*`): the pending result / error, the
/// application pointer registered with the function, and — for an aggregate — a
/// back-pointer to the accumulator so `sqlite3_aggregate_context` can reach its
/// per-group buffer. `agg` is null in a scalar context.
pub struct sqlite3_context {
    result: Value,
    error: Option<String>,
    user_data: *mut c_void,
    agg: *mut CAggregate,
}

type XFunc = Option<unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value)>;
type XStep = Option<unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value)>;
type XFinal = Option<unsafe extern "C" fn(*mut sqlite3_context)>;

/// Register a user-defined function callable from SQL: **scalar** (`xFunc` set,
/// `xStep`/`xFinal` NULL) or **aggregate** (`xStep`+`xFinal` set, `xFunc` NULL).
/// Any other combination yields `SQLITE_ERROR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_create_function(
    db: *mut sqlite3,
    name: *const c_char,
    _n_arg: c_int,
    _e_text_rep: c_int,
    p_app: *mut c_void,
    x_func: XFunc,
    x_step: XStep,
    x_final: XFinal,
) -> c_int {
    if db.is_null() {
        return SQLITE_ERROR;
    }
    let db = unsafe { &mut *db };
    let name = unsafe { cstr(name) }.to_string();
    match (x_func, x_step, x_final) {
        (Some(func), None, None) => {
            // The closure must be 'static; capture the C fn pointer (Copy) and the
            // app pointer as an integer so the closure body reconstitutes it.
            let app = p_app as usize;
            db.conn.register_function(&name, move |args: &[Value]| {
                let mut vals: Vec<sqlite3_value> = args
                    .iter()
                    .map(|v| sqlite3_value {
                        v: v.clone(),
                        scratch: None,
                    })
                    .collect();
                let mut ptrs: Vec<*mut sqlite3_value> =
                    vals.iter_mut().map(|p| p as *mut sqlite3_value).collect();
                let mut ctx = sqlite3_context {
                    result: Value::Null,
                    error: None,
                    user_data: app as *mut c_void,
                    agg: core::ptr::null_mut(),
                };
                unsafe {
                    func(
                        &mut ctx as *mut sqlite3_context,
                        args.len() as c_int,
                        ptrs.as_mut_ptr(),
                    );
                }
                match ctx.error {
                    Some(e) => Err(graphitesql::Error::Error(e)),
                    None => Ok(ctx.result),
                }
            });
            SQLITE_OK
        }
        (None, Some(step), Some(final_)) => {
            // Aggregate: a fresh accumulator per group wraps the C callbacks.
            let app = p_app as usize;
            db.conn.register_aggregate_function(&name, move || {
                Box::new(CAggregate {
                    step,
                    final_,
                    user_data: app,
                    agg_buf: Vec::new(),
                })
            });
            SQLITE_OK
        }
        // Any other combination (e.g. a no-op, or only one of step/final) is invalid.
        _ => SQLITE_ERROR,
    }
}

/// Register an aggregate usable as a **window function**. graphitesql drives
/// window aggregates by recomputing over each frame (`xStep`/`xFinal` with a
/// fresh accumulator per frame), so the sliding-frame optimization callbacks
/// `xValue`/`xInverse` are accepted for API compatibility but not required; the
/// same registration also serves plain `GROUP BY` aggregation. Requires
/// `xStep`+`xFinal`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn sqlite3_create_window_function(
    db: *mut sqlite3,
    name: *const c_char,
    _n_arg: c_int,
    _e_text_rep: c_int,
    p_app: *mut c_void,
    x_step: XStep,
    x_final: XFinal,
    _x_value: XFinal,
    _x_inverse: XStep,
    _x_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    if db.is_null() {
        return SQLITE_ERROR;
    }
    let db = unsafe { &mut *db };
    let name = unsafe { cstr(name) }.to_string();
    match (x_step, x_final) {
        (Some(step), Some(final_)) => {
            let app = p_app as usize;
            db.conn.register_aggregate_function(&name, move || {
                Box::new(CAggregate {
                    step,
                    final_,
                    user_data: app,
                    agg_buf: Vec::new(),
                })
            });
            SQLITE_OK
        }
        _ => SQLITE_ERROR,
    }
}

/// A C aggregate accumulator: one per group. Holds the C `xStep`/`xFinal`
/// callbacks and the persistent per-group buffer handed out by
/// `sqlite3_aggregate_context`.
struct CAggregate {
    step: unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value),
    final_: unsafe extern "C" fn(*mut sqlite3_context),
    user_data: usize,
    agg_buf: Vec<u8>,
}

impl graphitesql::AggregateFunction for CAggregate {
    fn step(&mut self, args: &[Value]) -> graphitesql::Result<()> {
        let mut vals: Vec<sqlite3_value> = args
            .iter()
            .map(|v| sqlite3_value {
                v: v.clone(),
                scratch: None,
            })
            .collect();
        let mut ptrs: Vec<*mut sqlite3_value> =
            vals.iter_mut().map(|p| p as *mut sqlite3_value).collect();
        // Copy out what we need so `self` isn't borrowed during the callback (the C
        // side reaches the accumulator only through the raw `agg` pointer).
        let step_fn = self.step;
        let ud = self.user_data;
        let self_ptr = self as *mut CAggregate;
        let mut ctx = sqlite3_context {
            result: Value::Null,
            error: None,
            user_data: ud as *mut c_void,
            agg: self_ptr,
        };
        unsafe { step_fn(&mut ctx, args.len() as c_int, ptrs.as_mut_ptr()) };
        match ctx.error {
            Some(e) => Err(graphitesql::Error::Error(e)),
            None => Ok(()),
        }
    }

    fn finalize(&mut self) -> graphitesql::Result<Value> {
        let final_fn = self.final_;
        let ud = self.user_data;
        let self_ptr = self as *mut CAggregate;
        let mut ctx = sqlite3_context {
            result: Value::Null,
            error: None,
            user_data: ud as *mut c_void,
            agg: self_ptr,
        };
        unsafe { final_fn(&mut ctx) };
        match ctx.error {
            Some(e) => Err(graphitesql::Error::Error(e)),
            None => Ok(ctx.result),
        }
    }
}

/// Per-group aggregate scratch: returns a stable, zero-initialised buffer of at
/// least `n_bytes`, persistent across this group's `xStep` calls and `xFinal`.
/// NULL in a scalar context or when `n_bytes <= 0` before any allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_aggregate_context(
    ctx: *mut sqlite3_context,
    n_bytes: c_int,
) -> *mut c_void {
    let Some(c) = (unsafe { ctx.as_mut() }) else {
        return core::ptr::null_mut();
    };
    if c.agg.is_null() {
        return core::ptr::null_mut();
    }
    let agg = unsafe { &mut *c.agg };
    let n = n_bytes.max(0) as usize;
    if agg.agg_buf.len() < n {
        // Grow only on first request (n is constant per aggregate) so the pointer
        // stays stable across the group's steps.
        agg.agg_buf.resize(n, 0);
    }
    if agg.agg_buf.is_empty() {
        return core::ptr::null_mut();
    }
    agg.agg_buf.as_mut_ptr() as *mut c_void
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_user_data(ctx: *mut sqlite3_context) -> *mut c_void {
    if ctx.is_null() {
        return core::ptr::null_mut();
    }
    unsafe { &*ctx }.user_data
}

// sqlite3_value_* accessors (argument readout inside a UDF).

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_type(v: *mut sqlite3_value) -> c_int {
    if v.is_null() {
        return SQLITE_NULL;
    }
    value_type(&unsafe { &*v }.v)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_int(v: *mut sqlite3_value) -> c_int {
    unsafe { sqlite3_value_int64(v) as c_int }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_int64(v: *mut sqlite3_value) -> c_longlong {
    if v.is_null() {
        return 0;
    }
    value_to_i64(&unsafe { &*v }.v)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_double(v: *mut sqlite3_value) -> c_double {
    if v.is_null() {
        return 0.0;
    }
    value_to_f64(&unsafe { &*v }.v)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_bytes(v: *mut sqlite3_value) -> c_int {
    if v.is_null() {
        return 0;
    }
    value_to_text(&unsafe { &*v }.v)
        .map(|b| b.len() as c_int)
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_text(v: *mut sqlite3_value) -> *const c_uchar {
    if v.is_null() {
        return core::ptr::null();
    }
    let v = unsafe { &mut *v };
    match value_to_text(&v.v) {
        Some(bytes) => {
            let c = CString::new(bytes).unwrap_or_default();
            let p = c.as_ptr() as *const c_uchar;
            v.scratch = Some(c);
            p
        }
        None => core::ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_blob(v: *mut sqlite3_value) -> *const c_void {
    unsafe { sqlite3_value_text(v) as *const c_void }
}

// sqlite3_result_* setters (a UDF's return value).

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_null(ctx: *mut sqlite3_context) {
    if let Some(c) = unsafe { ctx.as_mut() } {
        c.result = Value::Null;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_int(ctx: *mut sqlite3_context, v: c_int) {
    unsafe { sqlite3_result_int64(ctx, v as c_longlong) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_int64(ctx: *mut sqlite3_context, v: c_longlong) {
    if let Some(c) = unsafe { ctx.as_mut() } {
        c.result = Value::Integer(v);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_double(ctx: *mut sqlite3_context, v: c_double) {
    if let Some(c) = unsafe { ctx.as_mut() } {
        c.result = Value::Real(v);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_text(
    ctx: *mut sqlite3_context,
    text: *const c_char,
    n_byte: c_int,
    _destructor: isize,
) {
    let Some(c) = (unsafe { ctx.as_mut() }) else {
        return;
    };
    let s = if text.is_null() {
        String::new()
    } else if n_byte < 0 {
        unsafe { cstr(text) }.to_string()
    } else {
        let bytes = unsafe { core::slice::from_raw_parts(text as *const u8, n_byte as usize) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    c.result = Value::Text(s);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_blob(
    ctx: *mut sqlite3_context,
    data: *const c_void,
    n_byte: c_int,
    _destructor: isize,
) {
    let Some(c) = (unsafe { ctx.as_mut() }) else {
        return;
    };
    let bytes = if data.is_null() || n_byte <= 0 {
        Vec::new()
    } else {
        unsafe { core::slice::from_raw_parts(data as *const u8, n_byte as usize) }.to_vec()
    };
    c.result = Value::Blob(bytes);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_error(
    ctx: *mut sqlite3_context,
    msg: *const c_char,
    _n_byte: c_int,
) {
    if let Some(c) = unsafe { ctx.as_mut() } {
        c.error = Some(unsafe { cstr(msg) }.to_string());
    }
}

// --- custom collating sequences -----------------------------------------------

type XCompare = Option<
    unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
>;

/// Register a custom collating sequence callable as `COLLATE zName` in SQL. The
/// comparison receives the two operands as byte buffers and returns
/// negative/zero/positive like `memcmp`. Only UTF-8 (`eTextRep == SQLITE_UTF8`)
/// is meaningful here. A NULL `xCompare` (deregistration) yields `SQLITE_ERROR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_create_collation(
    db: *mut sqlite3,
    z_name: *const c_char,
    _e_text_rep: c_int,
    p_arg: *mut c_void,
    x_compare: XCompare,
) -> c_int {
    if db.is_null() {
        return SQLITE_ERROR;
    }
    let db = unsafe { &mut *db };
    let name = unsafe { cstr(z_name) }.to_string();
    match x_compare {
        Some(cmp) => {
            // Capture the app pointer as an integer so the closure stays `Send`.
            let arg = p_arg as usize;
            db.conn.register_collation(&name, move |x: &str, y: &str| {
                let xb = x.as_bytes();
                let yb = y.as_bytes();
                let r = unsafe {
                    cmp(
                        arg as *mut c_void,
                        xb.len() as c_int,
                        xb.as_ptr() as *const c_void,
                        yb.len() as c_int,
                        yb.as_ptr() as *const c_void,
                    )
                };
                r.cmp(&0)
            });
            SQLITE_OK
        }
        None => SQLITE_ERROR,
    }
}

/// Like `sqlite3_create_collation` with a destructor for the app pointer (which
/// this shim never invokes, since collations live for the process); delegates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_create_collation_v2(
    db: *mut sqlite3,
    z_name: *const c_char,
    e_text_rep: c_int,
    p_arg: *mut c_void,
    x_compare: XCompare,
    _x_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    unsafe { sqlite3_create_collation(db, z_name, e_text_rep, p_arg, x_compare) }
}

// --- UTF-16 entry points ------------------------------------------------------
//
// SQLite's `*16` API takes/returns UTF-16 in the host's native byte order.
// graphitesql is UTF-8 internally, so these convert at the boundary. `nByte`
// arguments are in BYTES (as in SQLite), so the u16 unit count is `nByte / 2`.

/// Decode a native-endian UTF-16 buffer to a `String`. `n_byte < 0` means
/// NUL-terminated; otherwise it is a byte length.
unsafe fn utf16_to_string(p: *const c_void, n_byte: c_int) -> String {
    if p.is_null() {
        return String::new();
    }
    let p = p as *const u16;
    let units: &[u16] = if n_byte < 0 {
        let mut len = 0usize;
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        unsafe { core::slice::from_raw_parts(p, len) }
    } else {
        unsafe { core::slice::from_raw_parts(p, (n_byte as usize) / 2) }
    };
    String::from_utf16_lossy(units)
}

/// Encode `s` as a NUL-terminated native-endian UTF-16 buffer.
fn str_to_utf16_nul(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_open16(
    filename: *const c_void,
    pp_db: *mut *mut sqlite3,
) -> c_int {
    let name = unsafe { utf16_to_string(filename, -1) };
    let c = CString::new(name).unwrap_or_default();
    unsafe { sqlite3_open(c.as_ptr(), pp_db) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_prepare16_v2(
    db: *mut sqlite3,
    sql: *const c_void,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_void,
) -> c_int {
    // The tail (a pointer into the original UTF-16 buffer) is not tracked here;
    // report "all consumed".
    if !pz_tail.is_null() {
        unsafe { *pz_tail = core::ptr::null() };
    }
    let s = unsafe { utf16_to_string(sql, n_byte) };
    let c = CString::new(s).unwrap_or_default();
    unsafe { sqlite3_prepare_v2(db, c.as_ptr(), -1, pp_stmt, core::ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_text16(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    text: *const c_void,
    n_byte: c_int,
    _destructor: isize,
) -> c_int {
    if stmt.is_null() {
        return SQLITE_ERROR;
    }
    let s = unsafe { utf16_to_string(text, n_byte) };
    bind_at(unsafe { &mut *stmt }, idx, Value::Text(s))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_text16(
    stmt: *mut sqlite3_stmt,
    col: c_int,
) -> *const c_void {
    if stmt.is_null() {
        return core::ptr::null();
    }
    let stmt = unsafe { &mut *stmt };
    let text = match stmt_cell(stmt, col) {
        Some(v) => match value_to_text(v) {
            Some(b) => String::from_utf8_lossy(&b).into_owned(),
            None => return core::ptr::null(), // NULL column
        },
        None => return core::ptr::null(),
    };
    let idx = col as usize;
    if stmt.text16_scratch.len() <= idx {
        stmt.text16_scratch.resize(idx + 1, None);
    }
    let u = str_to_utf16_nul(&text);
    let p = u.as_ptr() as *const c_void;
    stmt.text16_scratch[idx] = Some(u);
    p
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_bytes16(stmt: *mut sqlite3_stmt, col: c_int) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    match stmt_cell(unsafe { &*stmt }, col) {
        Some(v) => value_to_text(v)
            .map(|b| String::from_utf8_lossy(&b).encode_utf16().count() as c_int * 2)
            .unwrap_or(0),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_errmsg16(db: *mut sqlite3) -> *const c_void {
    if db.is_null() {
        return core::ptr::null();
    }
    let db = unsafe { &mut *db };
    let msg = match &db.errmsg {
        Some(m) => m.to_str().unwrap_or("").to_string(),
        None => "not an error".to_string(),
    };
    let u = str_to_utf16_nul(&msg);
    let p = u.as_ptr() as *const c_void;
    db.errmsg16 = Some(u);
    p
}

// --- data-change hook ---------------------------------------------------------

// SQLite update-hook op codes.
const SQLITE_DELETE: c_int = 9;
const SQLITE_INSERT: c_int = 18;
const SQLITE_UPDATE: c_int = 23;

type UpdateHookCb = Option<
    unsafe extern "C" fn(*mut c_void, c_int, *const c_char, *const c_char, c_longlong),
>;

/// Register a data-change notification callback (`sqlite3_update_hook`): invoked
/// once per inserted/updated/deleted row with the op code
/// (`SQLITE_INSERT`/`_UPDATE`/`_DELETE`), the database and table names, and the
/// rowid. A NULL callback removes the hook. Returns the previous `pArg` — not
/// tracked here, so always NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_update_hook(
    db: *mut sqlite3,
    cb: UpdateHookCb,
    arg: *mut c_void,
) -> *mut c_void {
    if db.is_null() {
        return core::ptr::null_mut();
    }
    let db = unsafe { &mut *db };
    match cb {
        Some(f) => {
            let a = arg as usize;
            db.conn
                .register_update_hook(move |op, dbname, table, rowid| {
                    let code = match op {
                        UpdateOp::Insert => SQLITE_INSERT,
                        UpdateOp::Update => SQLITE_UPDATE,
                        UpdateOp::Delete => SQLITE_DELETE,
                    };
                    let db_c = CString::new(dbname).unwrap_or_default();
                    let tb_c = CString::new(table).unwrap_or_default();
                    unsafe {
                        f(a as *mut c_void, code, db_c.as_ptr(), tb_c.as_ptr(), rowid);
                    }
                });
        }
        None => db.conn.remove_update_hook(),
    }
    core::ptr::null_mut()
}

/// The C `sqlite3_commit_hook` callback: returns non-zero to convert the pending
/// commit into a rollback.
type CommitHookCb = Option<unsafe extern "C" fn(*mut c_void) -> c_int>;
/// The C `sqlite3_rollback_hook` callback.
type RollbackHookCb = Option<unsafe extern "C" fn(*mut c_void)>;

/// Register a commit callback (`sqlite3_commit_hook`): invoked just before each
/// write transaction commits; a non-zero return converts the commit into a
/// rollback. A NULL callback removes the hook. Returns the previous `pArg` — not
/// tracked here, so always NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_commit_hook(
    db: *mut sqlite3,
    cb: CommitHookCb,
    arg: *mut c_void,
) -> *mut c_void {
    if db.is_null() {
        return core::ptr::null_mut();
    }
    let db = unsafe { &mut *db };
    match cb {
        Some(f) => {
            let a = arg as usize;
            db.conn
                .register_commit_hook(move || unsafe { f(a as *mut c_void) });
        }
        None => db.conn.remove_commit_hook(),
    }
    core::ptr::null_mut()
}

/// Register a rollback callback (`sqlite3_rollback_hook`): invoked whenever a
/// transaction rolls back. A NULL callback removes the hook. Returns the previous
/// `pArg` — not tracked here, so always NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_rollback_hook(
    db: *mut sqlite3,
    cb: RollbackHookCb,
    arg: *mut c_void,
) -> *mut c_void {
    if db.is_null() {
        return core::ptr::null_mut();
    }
    let db = unsafe { &mut *db };
    match cb {
        Some(f) => {
            let a = arg as usize;
            db.conn
                .register_rollback_hook(move || unsafe { f(a as *mut c_void) });
        }
        None => db.conn.remove_rollback_hook(),
    }
    core::ptr::null_mut()
}

/// The C `sqlite3_set_authorizer` callback: `(pArg, action, arg1, arg2, dbName,
/// triggerName) -> SQLITE_OK | SQLITE_DENY | SQLITE_IGNORE`. NULL string args are
/// passed for absent values.
type AuthorizerCb = Option<
    unsafe extern "C" fn(
        *mut c_void,
        c_int,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
    ) -> c_int,
>;

/// Register an authorizer (`sqlite3_set_authorizer`): consulted while preparing a
/// statement with an action code and up to two action-specific strings; returning
/// `SQLITE_DENY` rejects the statement. A NULL callback removes the authorizer.
/// Always returns `SQLITE_OK`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_set_authorizer(
    db: *mut sqlite3,
    cb: AuthorizerCb,
    arg: *mut c_void,
) -> c_int {
    if db.is_null() {
        return SQLITE_ERROR;
    }
    let db = unsafe { &mut *db };
    match cb {
        Some(f) => {
            let a = arg as usize;
            db.conn
                .set_authorizer(move |action, a1, a2, dbn, trig| {
                    // Convert each Option<&str> to a NUL-terminated C string kept
                    // alive across the call; None → a null pointer.
                    let c1 = a1.map(|s| CString::new(s).unwrap_or_default());
                    let c2 = a2.map(|s| CString::new(s).unwrap_or_default());
                    let c3 = dbn.map(|s| CString::new(s).unwrap_or_default());
                    let c4 = trig.map(|s| CString::new(s).unwrap_or_default());
                    let p = |c: &Option<CString>| {
                        c.as_ref().map_or(core::ptr::null(), |s| s.as_ptr())
                    };
                    unsafe { f(a as *mut c_void, action, p(&c1), p(&c2), p(&c3), p(&c4)) }
                });
        }
        None => db.conn.clear_authorizer(),
    }
    SQLITE_OK
}

// --- online backup ------------------------------------------------------------
//
// SQLite's backup API streams pages from a source database to a destination a
// chunk at a time. graphitesql materializes the whole database image, so this
// copies the source's serialized image into the destination in one step (any
// positive `nPage` — or -1 for "all" — completes it). API-compatible and correct
// for a whole-database backup; the destination holds the copy in memory (persist
// it with a subsequent write if the destination is file-backed). Only the `main`
// database of each side is supported (the schema-name arguments are ignored).

/// An in-progress backup. Mirrors the opaque `sqlite3_backup`.
pub struct sqlite3_backup {
    dest: *mut sqlite3,
    image: Vec<u8>,
    page_count: c_int,
    done: bool,
}

/// Start a backup from `source`'s `main` database to `dest`'s. Snapshots the
/// source image now; returns NULL on a NULL handle or a serialization failure.
/// The schema-name arguments are accepted but only `main` is copied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_init(
    dest: *mut sqlite3,
    _dest_name: *const c_char,
    source: *mut sqlite3,
    _source_name: *const c_char,
) -> *mut sqlite3_backup {
    if dest.is_null() || source.is_null() {
        return core::ptr::null_mut();
    }
    let src = unsafe { &*source };
    let image = match src.conn.serialize() {
        Ok(b) => b,
        Err(_) => return core::ptr::null_mut(),
    };
    // The page size is a big-endian u16 at offset 16 (the value 1 means 65536).
    let page_size = match image.get(16..18) {
        Some(&[hi, lo]) => match u32::from(u16::from_be_bytes([hi, lo])) {
            1 => 65536,
            n => n.max(512) as usize,
        },
        _ => 4096,
    };
    let page_count = (image.len() / page_size) as c_int;
    Box::into_raw(Box::new(sqlite3_backup {
        dest,
        image,
        page_count,
        done: false,
    }))
}

/// Copy the (remaining) database. `n_page` is honored loosely: any positive value
/// or -1 copies the whole image and returns `SQLITE_DONE`; a further call also
/// returns `SQLITE_DONE`. Returns `SQLITE_ERROR` if the destination cannot be
/// restored (e.g. it has an open transaction).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_step(p: *mut sqlite3_backup, _n_page: c_int) -> c_int {
    if p.is_null() {
        return SQLITE_ERROR;
    }
    let p = unsafe { &mut *p };
    if p.done {
        return SQLITE_DONE;
    }
    if p.dest.is_null() {
        return SQLITE_ERROR;
    }
    let dest = unsafe { &mut *p.dest };
    match dest.conn.restore_from(&p.image) {
        Ok(()) => {
            p.done = true;
            SQLITE_DONE
        }
        Err(_) => SQLITE_ERROR,
    }
}

/// Finish a backup, freeing the handle. Returns `SQLITE_OK`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_finish(p: *mut sqlite3_backup) -> c_int {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
    }
    SQLITE_OK
}

/// The number of pages still to be backed up (0 once complete).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_remaining(p: *mut sqlite3_backup) -> c_int {
    if p.is_null() {
        return 0;
    }
    let p = unsafe { &*p };
    if p.done { 0 } else { p.page_count }
}

/// The total number of pages in the source database.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_pagecount(p: *mut sqlite3_backup) -> c_int {
    if p.is_null() {
        return 0;
    }
    unsafe { &*p }.page_count
}

// --- incremental BLOB I/O -----------------------------------------------------
//
// SQLite streams a single cell's bytes from disk; graphitesql's value model
// materializes whole cells, so this handle buffers the cell (fetched on open,
// flushed on close for a writable handle). API-compatible and correct, but
// buffered rather than truly streaming.

/// An open BLOB handle. Mirrors the opaque `sqlite3_blob`.
pub struct sqlite3_blob {
    db: *mut sqlite3,
    table: String,
    column: String,
    rowid: c_longlong,
    buf: Vec<u8>,
    /// True if the source cell held text (write-back preserves the class).
    was_text: bool,
    dirty: bool,
    readonly: bool,
}

/// Quote an identifier for interpolation (`"` doubled), like SQLite.
fn quote_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Fetch the target cell's bytes and text-ness, or `None` if the row is absent.
fn blob_fetch(db: &mut sqlite3, table: &str, column: &str, rowid: c_longlong) -> Option<(Vec<u8>, bool)> {
    let sql = format!(
        "SELECT {} FROM {} WHERE rowid = ?1",
        quote_ident(column),
        quote_ident(table)
    );
    let params = Params {
        positional: vec![Value::Integer(rowid)],
        named: Vec::new(),
    };
    match db.conn.query_params(&sql, &params) {
        Ok(qr) if !qr.rows.is_empty() => {
            let v = &qr.rows[0][0];
            let was_text = matches!(v, Value::Text(_));
            Some((value_to_text(v).unwrap_or_default(), was_text))
        }
        Ok(_) => None,
        Err(e) => {
            db.set_error(SQLITE_ERROR, &e.to_string());
            None
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_blob_open(
    db: *mut sqlite3,
    _z_db: *const c_char,
    z_table: *const c_char,
    z_column: *const c_char,
    i_row: c_longlong,
    flags: c_int,
    pp_blob: *mut *mut sqlite3_blob,
) -> c_int {
    if db.is_null() || pp_blob.is_null() {
        return SQLITE_ERROR;
    }
    unsafe { *pp_blob = core::ptr::null_mut() };
    let db_ref = unsafe { &mut *db };
    let table = unsafe { cstr(z_table) }.to_string();
    let column = unsafe { cstr(z_column) }.to_string();
    let (buf, was_text) = match blob_fetch(db_ref, &table, &column, i_row) {
        Some(x) => x,
        None => {
            db_ref.set_error(SQLITE_ERROR, "no such rowid");
            return SQLITE_ERROR;
        }
    };
    let blob = Box::new(sqlite3_blob {
        db,
        table,
        column,
        rowid: i_row,
        buf,
        was_text,
        dirty: false,
        readonly: flags == 0,
    });
    unsafe { *pp_blob = Box::into_raw(blob) };
    SQLITE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_blob_bytes(blob: *mut sqlite3_blob) -> c_int {
    if blob.is_null() {
        return 0;
    }
    unsafe { &*blob }.buf.len() as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_blob_read(
    blob: *mut sqlite3_blob,
    z: *mut c_void,
    n: c_int,
    offset: c_int,
) -> c_int {
    if blob.is_null() || z.is_null() || n < 0 || offset < 0 {
        return SQLITE_ERROR;
    }
    let blob = unsafe { &*blob };
    let (off, n) = (offset as usize, n as usize);
    if off + n > blob.buf.len() {
        return SQLITE_ERROR;
    }
    unsafe { core::ptr::copy_nonoverlapping(blob.buf[off..].as_ptr(), z as *mut u8, n) };
    SQLITE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_blob_write(
    blob: *mut sqlite3_blob,
    z: *const c_void,
    n: c_int,
    offset: c_int,
) -> c_int {
    if blob.is_null() || z.is_null() || n < 0 || offset < 0 {
        return SQLITE_ERROR;
    }
    let blob = unsafe { &mut *blob };
    if blob.readonly {
        return SQLITE_ERROR;
    }
    let (off, n) = (offset as usize, n as usize);
    // A BLOB handle cannot change the cell's size.
    if off + n > blob.buf.len() {
        return SQLITE_ERROR;
    }
    let src = unsafe { core::slice::from_raw_parts(z as *const u8, n) };
    blob.buf[off..off + n].copy_from_slice(src);
    blob.dirty = true;
    SQLITE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_blob_reopen(blob: *mut sqlite3_blob, i_row: c_longlong) -> c_int {
    if blob.is_null() {
        return SQLITE_ERROR;
    }
    // Flush any pending write for the current row, then re-fetch the new one.
    let _ = unsafe { blob_flush(blob) };
    let blob = unsafe { &mut *blob };
    let db = unsafe { &mut *blob.db };
    match blob_fetch(db, &blob.table, &blob.column, i_row) {
        Some((buf, was_text)) => {
            blob.rowid = i_row;
            blob.buf = buf;
            blob.was_text = was_text;
            blob.dirty = false;
            SQLITE_OK
        }
        None => SQLITE_ERROR,
    }
}

/// Write a dirty writable blob back to its cell, preserving the storage class.
unsafe fn blob_flush(blob: *mut sqlite3_blob) -> c_int {
    let blob = unsafe { &mut *blob };
    if !blob.dirty || blob.readonly {
        return SQLITE_OK;
    }
    let value = if blob.was_text {
        Value::Text(String::from_utf8_lossy(&blob.buf).into_owned())
    } else {
        Value::Blob(blob.buf.clone())
    };
    let sql = format!(
        "UPDATE {} SET {} = ?1 WHERE rowid = ?2",
        quote_ident(&blob.table),
        quote_ident(&blob.column)
    );
    let params = Params {
        positional: vec![value, Value::Integer(blob.rowid)],
        named: Vec::new(),
    };
    let db = unsafe { &mut *blob.db };
    match db.conn.execute_params(&sql, &params) {
        Ok(_) => {
            blob.dirty = false;
            SQLITE_OK
        }
        Err(e) => {
            db.set_error(SQLITE_ERROR, &e.to_string());
            SQLITE_ERROR
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_blob_close(blob: *mut sqlite3_blob) -> c_int {
    if blob.is_null() {
        return SQLITE_OK;
    }
    let rc = unsafe { blob_flush(blob) };
    drop(unsafe { Box::from_raw(blob) });
    rc
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

/// Strip leading whitespace and `--` / `/* */` comments, returning the remainder.
fn strip_ws_comments(s: &str) -> &str {
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'-' && b[i + 1] == b'-' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else {
            break;
        }
    }
    &s[i.min(s.len())..]
}

/// `sqlite3_complete`: true if `sql` is one or more complete statements — i.e. it
/// contains a top-level `;` and only whitespace/comments follow the last one.
/// (The `CREATE TRIGGER … BEGIN … END;` nuance, where inner `;` don't terminate,
/// is not modelled — an inner `;` is treated as a terminator.)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_complete(sql: *const c_char) -> c_int {
    let s = unsafe { cstr(sql) };
    let mut rest = s;
    let mut saw_semi = false;
    while let Some(end) = statement_end(rest) {
        saw_semi = true;
        rest = &rest[end..];
    }
    (saw_semi && strip_ws_comments(rest).is_empty()) as c_int
}

/// `sqlite3_stmt_readonly`: true if the statement makes no direct changes to the
/// database (a `SELECT`/`WITH`/`VALUES`/`EXPLAIN`/read-only `PRAGMA`), false for a
/// mutation or an `INSERT/UPDATE/DELETE … RETURNING`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_stmt_readonly(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return 1;
    }
    let stmt = unsafe { &*stmt };
    (is_row_producer(&stmt.sql) && !has_returning(&stmt.sql)) as c_int
}
