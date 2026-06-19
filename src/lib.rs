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
pub use value::{cmp_values, SerialType, Value, ValueRef};

pub mod btree;
pub mod exec;
pub mod format;
pub mod pager;
pub mod schema;
pub mod sql;
pub mod vfs;

pub use error::{Error, Result};
pub use exec::{Connection, QueryResult};

/// The version of the SQLite file format graphitesql targets.
///
/// graphitesql reads and writes file-format version 3, which has been stable
/// and forward/backward compatible across every SQLite 3.x release.
pub const SQLITE_FILE_FORMAT: u32 = 3;

/// The SQLite release whose documented behavior graphitesql tracks as its
/// compatibility target. See `ATTRIBUTION.md`.
pub const TARGET_SQLITE_VERSION: &str = "3.53.2";
