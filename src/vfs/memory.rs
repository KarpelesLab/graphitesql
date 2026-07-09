//! An in-memory [`Vfs`] backed by `Vec<u8>` buffers.
//!
//! This is always available — `no_std`, wasm, anywhere — and backs the
//! `:memory:` database. File contents persist in the VFS across open/close so a
//! database can be written, closed, and reopened within the same `MemoryVfs`.
//!
//! It is single-threaded (uses `Rc`/`RefCell`); sharing a `MemoryVfs` across
//! threads is out of scope until the concurrency model is settled (see
//! `ROADMAP.md`).

use super::{File, LockLevel, LockState, OpenFlags, Vfs};
use crate::error::{Error, Result};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

/// Shared, mutable file storage. Cloning shares the same underlying bytes, which
/// is how the VFS and any open handles see each other's writes.
type SharedBytes = Rc<RefCell<Vec<u8>>>;

/// Shared lock state for one path, coordinating all open handles to it.
type SharedLocks = Rc<RefCell<LockState>>;

/// A named file in the VFS: its bytes and the lock state shared by its handles.
#[derive(Clone)]
struct FileEntry {
    bytes: SharedBytes,
    locks: SharedLocks,
    /// The shared wal-index for this path (ROADMAP C9c), shared by every open
    /// handle to it so multiple in-process connections resolve WAL frames
    /// coherently. Lazily populated by the pager on first WAL use.
    wal_index: crate::pager::SharedWalIndex,
}

/// An in-memory virtual file system.
#[derive(Default, Clone)]
pub struct MemoryVfs {
    files: Rc<RefCell<BTreeMap<String, FileEntry>>>,
}

impl MemoryVfs {
    /// Create an empty in-memory file system.
    pub fn new() -> MemoryVfs {
        MemoryVfs::default()
    }
}

impl Vfs for MemoryVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> Result<Box<dyn File>> {
        let mut files = self.files.borrow_mut();
        let entry = match files.get(path) {
            Some(e) => e.clone(),
            None => {
                if !flags.create {
                    return Err(Error::CantOpen(String::from(path)));
                }
                let e = FileEntry {
                    bytes: Rc::new(RefCell::new(Vec::new())),
                    locks: Rc::new(RefCell::new(LockState::default())),
                    wal_index: crate::pager::SharedWalIndex::new(),
                };
                files.insert(String::from(path), e.clone());
                e
            }
        };
        Ok(Box::new(MemoryFile {
            bytes: entry.bytes,
            locks: entry.locks,
            wal_index: entry.wal_index,
            level: Cell::new(LockLevel::Unlocked),
        }))
    }

    fn delete(&self, path: &str) -> Result<()> {
        self.files.borrow_mut().remove(path);
        Ok(())
    }

    fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.files.borrow().contains_key(path))
    }
}

/// A handle to one in-memory file.
pub struct MemoryFile {
    bytes: SharedBytes,
    /// Lock state shared with every other handle to the same path.
    locks: SharedLocks,
    /// The shared wal-index for this path (ROADMAP C9c), shared with every other
    /// handle to the same path so in-process connections read WAL coherently.
    wal_index: crate::pager::SharedWalIndex,
    /// The lock level this handle currently holds. A `Cell` so `lock`/`unlock`
    /// can take `&self` (see [`File::lock`]).
    level: Cell<LockLevel>,
}

impl Drop for MemoryFile {
    fn drop(&mut self) {
        // Release any locks this handle still holds so closing a connection
        // frees the file for others.
        self.locks
            .borrow_mut()
            .release(self.level.get(), LockLevel::Unlocked);
    }
}

impl File for MemoryFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        let data = self.bytes.borrow();
        let start = offset as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| Error::Io("read offset overflow".into()))?;
        if end > data.len() {
            return Err(Error::Io("read past end of file".into()));
        }
        buf.copy_from_slice(&data[start..end]);
        Ok(())
    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        let mut data = self.bytes.borrow_mut();
        let start = offset as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| Error::Io("write offset overflow".into()))?;
        if end > data.len() {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        self.bytes.borrow_mut().resize(size as usize, 0);
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        Ok(()) // nothing to flush; bytes are already "durable" in RAM
    }

    fn size(&self) -> Result<u64> {
        Ok(self.bytes.borrow().len() as u64)
    }

    fn lock(&self, level: LockLevel) -> Result<()> {
        self.locks.borrow_mut().acquire(self.level.get(), level)?;
        if level > self.level.get() {
            self.level.set(level);
        }
        Ok(())
    }

    fn unlock(&self, level: LockLevel) -> Result<()> {
        self.locks.borrow_mut().release(self.level.get(), level);
        if level < self.level.get() {
            self.level.set(level);
        }
        Ok(())
    }

    fn wal_index(&self) -> Option<crate::pager::SharedWalIndex> {
        Some(self.wal_index.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_write_reopen_roundtrip() {
        let vfs = MemoryVfs::new();
        assert!(!vfs.exists("db").unwrap());

        {
            let mut f = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
            f.write_all_at(b"hello world", 0).unwrap();
            assert_eq!(f.size().unwrap(), 11);
        }
        assert!(vfs.exists("db").unwrap());

        // Reopen: data persists in the VFS.
        let f = vfs.open("db", OpenFlags::READ_ONLY).unwrap();
        let mut buf = [0u8; 5];
        f.read_exact_at(&mut buf, 6).unwrap();
        assert_eq!(&buf, b"world");
    }

    #[test]
    fn write_extends_with_zero_fill() {
        let vfs = MemoryVfs::new();
        let mut f = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
        f.write_all_at(b"x", 10).unwrap();
        assert_eq!(f.size().unwrap(), 11);
        let mut buf = [0xffu8; 11];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf[..10], &[0u8; 10]); // gap is zero-filled
        assert_eq!(buf[10], b'x');
    }

    #[test]
    fn open_missing_without_create_fails() {
        let vfs = MemoryVfs::new();
        assert!(matches!(
            vfs.open("nope", OpenFlags::READ_ONLY),
            Err(Error::CantOpen(_))
        ));
    }

    #[test]
    fn read_past_end_errors() {
        let vfs = MemoryVfs::new();
        let mut f = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
        f.write_all_at(b"abc", 0).unwrap();
        let mut buf = [0u8; 8];
        assert!(matches!(f.read_exact_at(&mut buf, 0), Err(Error::Io(_))));
    }

    #[test]
    fn delete_removes_file() {
        let vfs = MemoryVfs::new();
        vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
        vfs.delete("db").unwrap();
        assert!(!vfs.exists("db").unwrap());
        vfs.delete("db").unwrap(); // deleting absent file is fine
    }
}
