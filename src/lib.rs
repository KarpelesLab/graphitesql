//! # graphitesql
//!
//! A pure, safe, `no_std`-capable Rust re-implementation of [SQLite].
//!
//! graphitesql is a single crate that reads and writes the **SQLite version 3
//! on-disk file format** and speaks a large subset of SQLite's SQL dialect. It
//! contains **no `unsafe`**, depends only on `core` + `alloc`, and is designed
//! to run anywhere from a server to a WebAssembly sandbox.
//!
//! ## Status
//!
//! graphitesql opens real SQLite databases, runs SQL (`SELECT` with joins,
//! aggregates, `GROUP BY`/`ORDER BY`/`LIMIT`; `CREATE TABLE`, `INSERT`,
//! `UPDATE`, `DELETE`; transactions), and **writes databases the real
//! `sqlite3` opens with `PRAGMA integrity_check = ok`**. It reads WAL-mode
//! databases (overlaying the `-wal`). The architecture and remaining breadth
//! work (indexes on write, more SQL) live in `ROADMAP.md`.
//!
//! ## Design goals
//!
//! * **File-format compatible.** A database created by SQLite must be readable
//!   and writable by graphitesql and vice-versa, byte for byte.
//! * **Safe.** `#![deny(unsafe_code)]` — the entire SQL/storage engine is
//!   `unsafe`-free. The only `unsafe` in the crate lives in the two opt-in FFI
//!   binding shims (`capi`, `wasm`), each `#[allow(unsafe_code)]` and off by
//!   default; the default build contains no `unsafe` and no FFI.
//! * **Portable.** `#![no_std]` + `alloc`. Optional `std` feature adds a
//!   file-backed VFS and `std::error::Error` integration.
//! * **Single crate.** Everything (storage, B-tree, SQL, VM, and the optional
//!   C-ABI / WebAssembly bindings) lives here.
//!
//! ## Feature flags
//!
//! * `std` *(default)* — enables the [`std`]-file VFS and `std::error::Error`.
//!   Disable for `no_std` targets; an in-memory VFS is always available.
//! * `fts5` *(default)* — registers the built-in FTS5 full-text-search virtual
//!   table (the `MATCH` query language, `bm25()`/`rank` ranking, `highlight()`).
//!   Disable to drop full-text search and shrink the build.
//! * `capi` — a `libsqlite3`-compatible C ABI (`extern "C"` `sqlite3_*` symbols);
//!   builds the `cdylib`/`staticlib`. Pulls in `std`. Uses `unsafe` (raw pointers).
//! * `wasm` — WebAssembly (browser) bindings via `wasm-bindgen`, with an
//!   OPFS-backed VFS. Uses `unsafe` (wasm-bindgen glue) and `js-sys`/`web-sys`.
//!
//! ## Attribution
//!
//! SQLite is public domain, created by D. Richard Hipp and contributors.
//! graphitesql uses SQLite's source and documentation only as a specification
//! reference; no SQLite code is compiled into this crate. See `NOTICE` and
//! `ATTRIBUTION.md`.
//!
//! [SQLite]: https://www.sqlite.org/

#![no_std]
// The engine is `unsafe`-free. `deny` (not `forbid`) so the two opt-in FFI shims
// below (`capi`, `wasm`) can carry a localized `#[allow(unsafe_code)]` — a C ABI
// needs raw pointers and wasm-bindgen generates `unsafe` glue. The default build
// (neither feature) contains no `unsafe`.
#![deny(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

/// A `libsqlite3`-compatible C ABI (subset) over the engine — `extern "C"`
/// `sqlite3_*` symbols. Opt-in (`capi` feature); uses `unsafe` (raw pointers).
#[cfg(feature = "capi")]
#[allow(unsafe_code)]
pub mod capi;

/// WebAssembly (browser) bindings via `wasm-bindgen`, with an OPFS-backed VFS.
/// Opt-in (`wasm` feature); uses `unsafe` (wasm-bindgen glue).
#[cfg(feature = "wasm")]
#[allow(unsafe_code)]
pub mod wasm;

pub mod error;
// Low-level implementation modules are `pub` only so the FFI binding modules
// (`capi`, `wasm`) and the test suite can reach them;
// they are NOT part of graphitesql's stable, SQLite-compatible public API. Mark
// them `#[doc(hidden)]` so they are excluded from the generated docs and from
// `cargo-semver-checks` — internal churn (e.g. a new field on `vtab::Fts5Tok`)
// must not force a version bump. The curated API is the crate-root re-exports
// below (`Connection`, `Value`, `Error`, the session/changeset types, …), plus
// `error`, `session`, and `vfs` (a documented custom-VFS extension point).
#[doc(hidden)]
pub mod util;

mod value;
pub use value::{
    Collation, SerialType, Text, Value, ValueRef, cmp_text, cmp_values, cmp_values_coll,
};

#[doc(hidden)]
pub mod btree;
#[doc(hidden)]
pub mod exec;
#[doc(hidden)]
pub mod format;
#[cfg(feature = "fts5")]
pub(crate) mod fts5_index;
pub(crate) mod geopoly;
#[doc(hidden)]
pub mod pager;
#[doc(hidden)]
pub mod schema;
pub mod session;
#[doc(hidden)]
pub mod sql;
pub mod vfs;
#[doc(hidden)]
pub mod vtab;

pub use error::{Error, Result};
pub use exec::{
    AggregateFactory, AggregateFunction, Connection, QueryResult, ScalarFunction, UpdateOp,
};
pub use session::{Changeset, ConflictAction, ConflictType, Rebaser, Session};

/// The version of the SQLite file format graphitesql targets.
///
/// graphitesql reads and writes file-format version 3, which has been stable
/// and forward/backward compatible across every SQLite 3.x release.
pub const SQLITE_FILE_FORMAT: u32 = 3;

/// The SQLite release whose documented behavior graphitesql tracks as its
/// compatibility target. See `ATTRIBUTION.md`.
pub const TARGET_SQLITE_VERSION: &str = "3.53.2";

/// The value returned by the `sqlite_source_id()` SQL function.
///
/// SQLite reports the exact source-control identifier of its C build here, in a
/// `YYYY-MM-DD HH:MM:SS <hash>` shape. graphitesql is an independent
/// reimplementation with no SQLite source compiled in, so — like
/// [`TARGET_SQLITE_VERSION`] — this is graphitesql's own identifier in that
/// shape rather than an impersonation of a particular C build. Callers that log
/// or display the source id (many drivers fetch it at startup beside
/// `sqlite_version()`) get a well-formed string instead of an error.
pub const TARGET_SQLITE_SOURCE_ID: &str =
    "2025-01-01 00:00:00 graphitesql00000000000000000000000000000000000000";
