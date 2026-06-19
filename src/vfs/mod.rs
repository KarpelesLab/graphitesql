//! The OS abstraction layer: graphitesql's only I/O boundary.
//!
//! Everything the engine does to durable storage goes through the [`Vfs`] and
//! [`File`] traits, exactly as SQLite routes all I/O through its `sqlite3_vfs`.
//! This is what lets the same engine run against in-memory storage, real files,
//! or a host-provided WebAssembly backend without changing a line above this
//! layer.
//!
//! Two implementations ship in the crate:
//!
//! * [`memory::MemoryVfs`] — `Vec<u8>`-backed, always available (incl. `no_std`
//!   and wasm). Backs `:memory:` and is the default for tests.
//! * [`std_file::StdVfs`] — real files via `std::fs` (feature `std`).
//!
//! Positioned reads/writes (`*_at`) are used throughout rather than a stateful
//! cursor, mirroring `pread`/`pwrite`; this keeps the pager's intent explicit
//! and avoids a shared seek offset.

use crate::error::Result;
use alloc::boxed::Box;

pub mod memory;

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
pub mod std_file;

/// How a database file should be opened.
///
/// SQLite's VFS has a richer flag set; graphitesql starts with the three modes
/// the engine actually needs and will extend this as journal/WAL files arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags {
    /// Whether writes are permitted.
    pub write: bool,
    /// Whether the file should be created if it does not exist.
    pub create: bool,
}

impl OpenFlags {
    /// Open an existing file for reading only.
    pub const READ_ONLY: OpenFlags = OpenFlags { write: false, create: false };
    /// Open an existing file for reading and writing.
    pub const READ_WRITE: OpenFlags = OpenFlags { write: true, create: false };
    /// Open for reading and writing, creating the file if absent.
    pub const READ_WRITE_CREATE: OpenFlags = OpenFlags { write: true, create: true };
}

/// File lock levels, matching SQLite's locking model.
///
/// Lock transitions are a Phase 6/8 concern; the trait carries them now so the
/// interface is stable, with single-process no-op defaults until real locking
/// lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockLevel {
    /// No lock held.
    Unlocked,
    /// A shared (read) lock; multiple readers may hold it.
    Shared,
    /// Intent-to-write lock; at most one, coexists with readers.
    Reserved,
    /// Transitional lock taken while waiting to go exclusive.
    Pending,
    /// Exclusive (write) lock.
    Exclusive,
}

/// An open file: positioned reads/writes plus durability and locking.
///
/// Reads take `&self` so multiple logical reads can be issued without exclusive
/// access; mutating operations take `&mut self`. Implementations that need
/// interior mutability for reads (e.g. seek-based std files) handle that
/// internally.
pub trait File {
    /// Fill `buf` from the file starting at byte `offset`.
    ///
    /// Must read exactly `buf.len()` bytes or return [`crate::Error::Io`] (a
    /// short read past EOF is an error, matching SQLite's expectation that the
    /// pager only reads bytes it knows exist).
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()>;

    /// Write all of `buf` to the file starting at byte `offset`, extending the
    /// file if necessary.
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()>;

    /// Truncate or extend the file to exactly `size` bytes.
    fn truncate(&mut self, size: u64) -> Result<()>;

    /// Flush buffered writes to durable storage.
    fn sync(&mut self) -> Result<()>;

    /// The current size of the file in bytes.
    fn size(&self) -> Result<u64>;

    /// Acquire (or upgrade to) the given lock level.
    ///
    /// Default: a no-op success, suitable for single-process, single-connection
    /// use. Real cross-process locking is layered in later.
    fn lock(&mut self, _level: LockLevel) -> Result<()> {
        Ok(())
    }

    /// Release down to the given lock level.
    fn unlock(&mut self, _level: LockLevel) -> Result<()> {
        Ok(())
    }
}

/// A virtual file system: opens, deletes, and probes files by name.
pub trait Vfs {
    /// Open the file at `path` with the given `flags`.
    fn open(&self, path: &str, flags: OpenFlags) -> Result<Box<dyn File>>;

    /// Delete the file at `path`. Deleting a missing file is not an error.
    fn delete(&self, path: &str) -> Result<()>;

    /// Whether a file exists at `path`.
    fn exists(&self, path: &str) -> Result<bool>;
}
