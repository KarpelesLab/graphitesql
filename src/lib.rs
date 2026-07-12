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
//! * **Safe.** `#![forbid(unsafe_code)]`. No FFI, no C, no `unsafe` blocks.
//! * **Portable.** `#![no_std]` + `alloc`. Optional `std` feature adds a
//!   file-backed VFS and `std::error::Error` integration.
//! * **Single crate.** Everything (storage, B-tree, SQL, VM) lives here.
//!
//! ## Feature flags
//!
//! * `std` *(default)* — enables the [`std`]-file VFS and `std::error::Error`.
//!   Disable for `no_std` targets; an in-memory VFS is always available.
//! * `fts5` *(default)* — registers the built-in FTS5 full-text-search virtual
//!   table (the `MATCH` query language, `bm25()`/`rank` ranking, `highlight()`).
//!   Disable to drop full-text search and shrink the build.
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
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod error;
pub mod util;

mod value;
pub use value::{
    Collation, SerialType, Text, Value, ValueRef, cmp_text, cmp_values, cmp_values_coll,
};

pub mod btree;
pub mod exec;
pub mod format;
#[cfg(feature = "fts5")]
pub(crate) mod fts5_index;
pub(crate) mod geopoly;
pub mod pager;
pub mod schema;
pub mod session;
pub mod sql;
pub mod vfs;
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
