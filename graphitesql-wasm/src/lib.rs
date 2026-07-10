//! WebAssembly (browser) bindings for [graphitesql](https://crates.io/crates/graphitesql).
//!
//! This crate is the D6 "wasm" track: it exposes graphitesql's engine to
//! JavaScript through [`wasm-bindgen`], with two storage backends:
//!
//! * **in-memory** — [`Database::new`], always available, needs no host support;
//! * **OPFS** — [`Database::open_opfs`], persistent, backed by the browser's
//!   Origin-Private File System *synchronous access handles*.
//!
//! # Why this is a separate crate
//!
//! The core `graphitesql` crate is `#![forbid(unsafe_code)]`, `#![no_std]`+alloc
//! and zero-dependency. `wasm-bindgen` generates `unsafe` glue and pulls in
//! `js-sys`/`web-sys`, so the JS boundary lives here, out of the core, exactly as
//! the ROADMAP requires. The core is consumed with `default-features = false`
//! (its `std` VFS and OS file locks do not exist on `wasm32-unknown-unknown`).
//!
//! # OPFS and the synchronous-VFS trick
//!
//! graphitesql's [`Vfs`]/[`File`] traits are **synchronous**. Browser file APIs
//! are overwhelmingly async — *except* `FileSystemSyncAccessHandle`, whose
//! `read`/`write`/`truncate`/`flush`/`getSize` are synchronous and only available
//! inside a **Web Worker**. Acquiring a handle is still async, so the pattern is:
//! the worker's JS acquires a handle per database file up front, hands the set to
//! [`Database::open_opfs`], and from then on all I/O is synchronous Rust calling
//! synchronous JS. No async rework of the engine is needed.
//!
//! See `worker.js` and `index.html` in this crate for a runnable browser example.

// wasm-bindgen's generated glue is `unsafe`; opt out of the workspace-wide ban.
#![allow(unsafe_code)]

use graphitesql::vfs::{File, OpenFlags, Vfs};
use graphitesql::{Connection, Error, Value};
use js_sys::{Array, Object, Reflect, Uint8Array};
use std::cell::RefCell;
use std::collections::BTreeMap;
use wasm_bindgen::prelude::*;
use web_sys::{FileSystemReadWriteOptions, FileSystemSyncAccessHandle};

/// Convert a graphitesql [`Value`] to a JavaScript value.
///
/// `NULL` → `null`, `Real` → `number`, `Text` → `string`, `Blob` → `Uint8Array`.
/// An `Integer` becomes a `number` when it is exactly representable as an IEEE-754
/// double (`|n| < 2^53`) and a `BigInt` otherwise, so no precision is silently lost.
fn value_to_js(v: &Value) -> JsValue {
    match v {
        Value::Null => JsValue::NULL,
        Value::Integer(i) => {
            if *i >= -(1i64 << 53) && *i <= (1i64 << 53) {
                JsValue::from_f64(*i as f64)
            } else {
                JsValue::from(*i) // i64 -> BigInt
            }
        }
        Value::Real(f) => JsValue::from_f64(*f),
        Value::Text(s) => JsValue::from_str(s),
        Value::Blob(b) => Uint8Array::from(b.as_slice()).into(),
    }
}

/// Build the `{ columns: string[], rows: any[][] }` object returned by
/// [`Database::query`].
fn query_result_to_js(qr: &graphitesql::QueryResult) -> Result<JsValue, JsValue> {
    let obj = Object::new();
    let cols = Array::new();
    for c in &qr.columns {
        cols.push(&JsValue::from_str(c));
    }
    Reflect::set(&obj, &JsValue::from_str("columns"), &cols)?;
    let rows = Array::new();
    for row in &qr.rows {
        let jr = Array::new();
        for v in row {
            jr.push(&value_to_js(v));
        }
        rows.push(&jr);
    }
    Reflect::set(&obj, &JsValue::from_str("rows"), &rows)?;
    Ok(obj.into())
}

fn to_js_err(e: Error) -> JsValue {
    JsValue::from(js_sys::Error::new(&e.to_string()))
}

/// A graphitesql database handle, exported to JavaScript.
///
/// Create one with [`new`](Database::new) (in-memory), [`open_opfs`](Database::open_opfs)
/// (persistent), or [`deserialize`](Database::deserialize) (from a database image).
#[wasm_bindgen]
pub struct Database {
    conn: Connection,
}

#[wasm_bindgen]
impl Database {
    /// Create a fresh in-memory database (`:memory:`). Always available — needs
    /// no host filesystem support, so it works on the main thread too.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<Database, JsValue> {
        Connection::open_memory()
            .map(|conn| Database { conn })
            .map_err(to_js_err)
    }

    /// Open an in-memory database from a complete SQLite database image (the
    /// equivalent of `sqlite3_deserialize`). Useful for loading a `.sqlite` file
    /// fetched over the network.
    pub fn deserialize(bytes: &[u8]) -> Result<Database, JsValue> {
        Connection::deserialize(bytes)
            .map(|conn| Database { conn })
            .map_err(to_js_err)
    }

    /// Open (or, when `create` is true, create) a persistent OPFS-backed database.
    ///
    /// `files` is a JS object mapping each database file name to an open
    /// `FileSystemSyncAccessHandle`: the main file under `path`, plus `<path>-journal`
    /// (rollback mode). The caller — running in a Web Worker — is responsible for
    /// acquiring the handles asynchronously before calling this. See `worker.js`.
    ///
    /// # Errors
    /// If the handles are missing/unusable or the file is not a valid database.
    #[wasm_bindgen(js_name = openOpfs)]
    pub fn open_opfs(files: &Object, path: &str, create: bool) -> Result<Database, JsValue> {
        let vfs = OpfsVfs::from_js(files)?;
        let conn = if create {
            Connection::create_vfs(&vfs, path, 4096)
        } else {
            Connection::open_vfs(&vfs, path)
        }
        .map_err(to_js_err)?;
        Ok(Database { conn })
    }

    /// Run one or more statements that return no rows (DDL/DML), returning the
    /// number of rows changed by the last statement.
    pub fn exec(&mut self, sql: &str) -> Result<usize, JsValue> {
        self.conn.execute(sql).map_err(to_js_err)
    }

    /// Run a query and return `{ columns: string[], rows: any[][] }`.
    pub fn query(&self, sql: &str) -> Result<JsValue, JsValue> {
        let qr = self.conn.query(sql).map_err(to_js_err)?;
        query_result_to_js(&qr)
    }

    /// Serialize the whole database to a byte image (the equivalent of
    /// `sqlite3_serialize`) — e.g. to download an in-memory database as a file.
    pub fn serialize(&self) -> Result<Vec<u8>, JsValue> {
        self.conn.serialize().map_err(to_js_err)
    }
}

// ---------------------------------------------------------------------------
// OPFS-backed VFS
// ---------------------------------------------------------------------------

/// A file backed by an OPFS `FileSystemSyncAccessHandle`. All of the handle's
/// I/O methods are synchronous (Worker-only), which is exactly what the sync
/// [`File`] trait needs.
struct OpfsFile {
    handle: FileSystemSyncAccessHandle,
}

impl OpfsFile {
    fn opts_at(offset: u64) -> FileSystemReadWriteOptions {
        let o = FileSystemReadWriteOptions::new();
        o.set_at(offset as f64);
        o
    }
}

impl File for OpfsFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> graphitesql::Result<()> {
        let opts = Self::opts_at(offset);
        let n = self
            .handle
            .read_with_u8_array_and_options(buf, &opts)
            .map_err(|_| Error::Io("OPFS read failed".into()))? as usize;
        if n < buf.len() {
            // SQLite treats a short read past EOF as reading zeros.
            for b in &mut buf[n..] {
                *b = 0;
            }
        }
        Ok(())
    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> graphitesql::Result<()> {
        let opts = Self::opts_at(offset);
        let n = self
            .handle
            .write_with_u8_array_and_options(buf, &opts)
            .map_err(|_| Error::Io("OPFS write failed".into()))? as usize;
        if n != buf.len() {
            return Err(Error::Io("OPFS short write".into()));
        }
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> graphitesql::Result<()> {
        self.handle
            .truncate_with_f64(size as f64)
            .map_err(|_| Error::Io("OPFS truncate failed".into()))
    }

    fn sync(&mut self) -> graphitesql::Result<()> {
        self.handle
            .flush()
            .map_err(|_| Error::Io("OPFS flush failed".into()))
    }

    fn size(&self) -> graphitesql::Result<u64> {
        self.handle
            .get_size()
            .map(|s| s as u64)
            .map_err(|_| Error::Io("OPFS getSize failed".into()))
    }
    // lock/unlock use the default no-op impls: OPFS sync-access handles are
    // already exclusive per file (acquiring one is the lock), and a Worker owns
    // its database, so the process-local lock model is a no-op here.
}

/// Registry entry for one pre-acquired OPFS handle.
struct OpfsEntry {
    handle: FileSystemSyncAccessHandle,
    /// A journal that graphitesql has `delete`d is logically absent until the
    /// next `open`; we keep the handle (re-acquisition is async) but report it
    /// gone from `exists` and hand back a zero-length view on re-open.
    deleted: bool,
}

/// A VFS over a fixed set of OPFS handles supplied by JavaScript. Because
/// acquiring a handle is asynchronous but the engine's `open` is synchronous,
/// every file the connection may touch (main + `-journal`) must be registered
/// before the connection is opened.
struct OpfsVfs {
    files: RefCell<BTreeMap<String, OpfsEntry>>,
}

impl OpfsVfs {
    /// Build from a JS object mapping file name → `FileSystemSyncAccessHandle`.
    fn from_js(obj: &Object) -> Result<OpfsVfs, JsValue> {
        let files = RefCell::new(BTreeMap::new());
        let entries = Object::entries(obj);
        for entry in entries.iter() {
            let pair: Array = entry.into();
            let key = pair.get(0).as_string().ok_or_else(|| {
                JsValue::from(js_sys::Error::new("OPFS file map key must be a string"))
            })?;
            let handle: FileSystemSyncAccessHandle = pair.get(1).dyn_into().map_err(|_| {
                JsValue::from(js_sys::Error::new(
                    "OPFS file map value must be a FileSystemSyncAccessHandle",
                ))
            })?;
            files.borrow_mut().insert(
                key,
                OpfsEntry {
                    handle,
                    deleted: false,
                },
            );
        }
        Ok(OpfsVfs { files })
    }
}

impl Vfs for OpfsVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> graphitesql::Result<Box<dyn File>> {
        let mut files = self.files.borrow_mut();
        match files.get_mut(path) {
            Some(entry) => {
                if entry.deleted {
                    if !flags.create {
                        return Err(Error::CantOpen(format!("no such file: {path}")));
                    }
                    // Re-created: start empty.
                    entry
                        .handle
                        .truncate_with_f64(0.0)
                        .map_err(|_| Error::Io("OPFS truncate-on-create failed".into()))?;
                    entry.deleted = false;
                }
                Ok(Box::new(OpfsFile {
                    handle: entry.handle.clone(),
                }))
            }
            None => Err(Error::CantOpen(format!(
                "OPFS handle not registered for {path}"
            ))),
        }
    }

    fn delete(&self, path: &str) -> graphitesql::Result<()> {
        if let Some(entry) = self.files.borrow_mut().get_mut(path) {
            entry
                .handle
                .truncate_with_f64(0.0)
                .map_err(|_| Error::Io("OPFS delete (truncate) failed".into()))?;
            entry.deleted = true;
        }
        Ok(())
    }

    fn exists(&self, path: &str) -> graphitesql::Result<bool> {
        Ok(self
            .files
            .borrow()
            .get(path)
            .map(|e| !e.deleted)
            .unwrap_or(false))
    }
}
