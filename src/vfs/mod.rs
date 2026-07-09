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

use crate::error::{Error, Result};
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
    pub const READ_ONLY: OpenFlags = OpenFlags {
        write: false,
        create: false,
    };
    /// Open an existing file for reading and writing.
    pub const READ_WRITE: OpenFlags = OpenFlags {
        write: true,
        create: false,
    };
    /// Open for reading and writing, creating the file if absent.
    pub const READ_WRITE_CREATE: OpenFlags = OpenFlags {
        write: true,
        create: true,
    };
}

/// File lock levels, matching SQLite's locking model. Ordered from weakest to
/// strongest; [`LockState`] enforces the compatibility rules between them.
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

/// The aggregate lock state of a single file, shared by every open handle to it.
///
/// This encodes SQLite's lock-compatibility rules (`os.c` / `pager.c`): many
/// `SHARED` readers may coexist; a single `RESERVED` write-intent lock coexists
/// with readers; `PENDING` stops *new* readers while a writer drains the
/// existing ones; and `EXCLUSIVE` excludes everything else. A VFS shares one
/// `LockState` per path among its handles so concurrent connections in the same
/// process serialize exactly as separate SQLite connections would.
#[derive(Debug, Default)]
pub struct LockState {
    /// Number of handles currently holding a `SHARED` (read) lock.
    shared: usize,
    /// A `RESERVED` (write-intent) lock is held.
    reserved: bool,
    /// A `PENDING` lock is held (a writer is waiting to go `EXCLUSIVE`).
    pending: bool,
    /// An `EXCLUSIVE` (write) lock is held.
    exclusive: bool,
}

impl LockState {
    /// Move a handle from lock level `from` up to `to`, mutating the shared
    /// state. Returns [`Error::Busy`] if another handle holds an incompatible
    /// lock. A request at or below the current level is a no-op success.
    pub fn acquire(&mut self, from: LockLevel, to: LockLevel) -> Result<()> {
        if to <= from {
            return Ok(());
        }
        match to {
            LockLevel::Unlocked => {}
            LockLevel::Shared => {
                // No new readers while a writer is pending or exclusive.
                if self.pending || self.exclusive {
                    return Err(Error::Busy);
                }
                self.shared += 1;
            }
            LockLevel::Reserved => {
                // At most one writer-intent lock; incompatible with EXCLUSIVE.
                if self.reserved || self.exclusive {
                    return Err(Error::Busy);
                }
                self.reserved = true;
            }
            LockLevel::Pending => {
                if self.pending || self.exclusive {
                    return Err(Error::Busy);
                }
                self.pending = true;
            }
            LockLevel::Exclusive => {
                if self.exclusive {
                    return Err(Error::Busy);
                }
                // Every other reader must have drained; this handle's own SHARED
                // lock (if held) does not block its upgrade.
                let own_shared = usize::from(from >= LockLevel::Shared);
                if self.shared > own_shared {
                    return Err(Error::Busy);
                }
                self.pending = true;
                self.exclusive = true;
            }
        }
        Ok(())
    }

    /// Release a handle from `from` down to `to`, clearing whatever it held above
    /// `to`. Never fails.
    pub fn release(&mut self, from: LockLevel, to: LockLevel) {
        if from >= LockLevel::Exclusive && to < LockLevel::Exclusive {
            self.exclusive = false;
        }
        if from >= LockLevel::Pending && to < LockLevel::Pending {
            self.pending = false;
        }
        if from >= LockLevel::Reserved && to < LockLevel::Reserved {
            self.reserved = false;
        }
        if from >= LockLevel::Shared && to < LockLevel::Shared {
            self.shared = self.shared.saturating_sub(1);
        }
    }
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

    /// Acquire (or upgrade to) the given lock level, returning
    /// [`Error::Busy`] if an incompatible lock is held.
    ///
    /// Takes `&self` so the pager's `&self` read path can lazily take the
    /// persistent `SHARED` lock of an open read transaction (ROADMAP C9a); the
    /// per-handle lock level is tracked with interior mutability. The built-in
    /// VFSs enforce the [`LockState`] rules across all handles to a path within
    /// the process; the default here is a no-op success for trivial or
    /// host-provided files that do their own coordination.
    fn lock(&self, _level: LockLevel) -> Result<()> {
        Ok(())
    }

    /// Release down to the given lock level. Never fails for the built-in VFSs.
    /// Takes `&self` for the same reason as [`lock`](File::lock).
    fn unlock(&self, _level: LockLevel) -> Result<()> {
        Ok(())
    }

    /// The shared **wal-index** for this file's path, if the VFS provides one
    /// (ROADMAP C9c).
    ///
    /// In WAL mode the newest committed version of a page may live in the `-wal`
    /// file; a coherent page→latest-frame map lets *multiple* in-process
    /// `Connection`s reading the same file each resolve the correct latest
    /// committed frame, and lets a reader hold a stable snapshot across a
    /// concurrent writer's commits. The built-in VFSs return a per-path handle
    /// (shared by every open handle to the same path, exactly like the
    /// [`LockState`] registry); a host-provided file that does its own WAL
    /// coordination — or any non-`-wal` file — returns `None` (the default), in
    /// which case the pager falls back to a private per-connection index.
    ///
    /// This is called on the `-wal` companion file handle, which is the natural
    /// per-path carrier for the index.
    fn wal_index(&self) -> Option<crate::pager::SharedWalIndex> {
        None
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

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn many_readers_coexist() {
        let mut s = LockState::default();
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap();
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap();
        assert_eq!(s.shared, 2);
    }

    #[test]
    fn reserved_coexists_with_readers_but_is_exclusive_among_writers() {
        let mut s = LockState::default();
        // Two readers and one of them takes RESERVED.
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap();
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap();
        s.acquire(LockLevel::Shared, LockLevel::Reserved).unwrap();
        // A new reader is still allowed under RESERVED.
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap();
        // A second RESERVED is refused.
        assert!(matches!(
            s.acquire(LockLevel::Shared, LockLevel::Reserved),
            Err(Error::Busy)
        ));
    }

    #[test]
    fn exclusive_requires_other_readers_to_drain() {
        let mut s = LockState::default();
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap(); // writer's own
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap(); // a second reader
        s.acquire(LockLevel::Shared, LockLevel::Reserved).unwrap();
        // Cannot go EXCLUSIVE while the other reader holds SHARED.
        assert!(matches!(
            s.acquire(LockLevel::Reserved, LockLevel::Exclusive),
            Err(Error::Busy)
        ));
        // Other reader drops; now the upgrade succeeds.
        s.release(LockLevel::Shared, LockLevel::Unlocked);
        s.acquire(LockLevel::Reserved, LockLevel::Exclusive)
            .unwrap();
        assert!(s.exclusive);
    }

    #[test]
    fn pending_blocks_new_readers() {
        let mut s = LockState::default();
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap();
        s.acquire(LockLevel::Shared, LockLevel::Pending).unwrap();
        assert!(matches!(
            s.acquire(LockLevel::Unlocked, LockLevel::Shared),
            Err(Error::Busy)
        ));
    }

    #[test]
    fn release_clears_all_held_levels() {
        let mut s = LockState::default();
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap();
        s.acquire(LockLevel::Shared, LockLevel::Reserved).unwrap();
        s.acquire(LockLevel::Reserved, LockLevel::Exclusive)
            .unwrap();
        s.release(LockLevel::Exclusive, LockLevel::Unlocked);
        assert_eq!(s.shared, 0);
        assert!(!s.reserved && !s.pending && !s.exclusive);
        // The file is fully unlocked again: a fresh writer can take everything.
        s.acquire(LockLevel::Unlocked, LockLevel::Shared).unwrap();
        s.acquire(LockLevel::Shared, LockLevel::Exclusive).unwrap();
        assert!(s.exclusive);
    }
}
