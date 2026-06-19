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
//! Early scaffolding. The architecture and the build plan live in
//! `ROADMAP.md`. Today the crate provides the foundational, fully-specified
//! primitives that everything else is built on (variable-length integers, the
//! database header, value/serial-type model). See the roadmap for what lands
//! next.
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
pub use value::{SerialType, Value, ValueRef};

pub mod format;
pub mod pager;
pub mod vfs;

pub use error::{Error, Result};

/// The version of the SQLite file format graphitesql targets.
///
/// graphitesql reads and writes file-format version 3, which has been stable
/// and forward/backward compatible across every SQLite 3.x release.
pub const SQLITE_FILE_FORMAT: u32 = 3;

/// The SQLite release whose documented behavior graphitesql tracks as its
/// compatibility target. See `ATTRIBUTION.md`.
pub const TARGET_SQLITE_VERSION: &str = "3.53.2";
