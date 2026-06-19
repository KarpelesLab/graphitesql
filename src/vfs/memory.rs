//! An in-memory [`Vfs`] backed by `Vec<u8>` buffers.
//!
//! This is always available — `no_std`, wasm, anywhere — and backs the
//! `:memory:` database. File contents persist in the VFS across open/close so a
//! database can be written, closed, and reopened within the same `MemoryVfs`.
//!
//! It is single-threaded (uses `Rc`/`RefCell`); sharing a `MemoryVfs` across
//! threads is out of scope until the concurrency model is settled (see
//! `ROADMAP.md`).

use super::{File, OpenFlags, Vfs};
use crate::error::{Error, Result};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

/// Shared, mutable file storage. Cloning shares the same underlying bytes, which
/// is how the VFS and any open handles see each other's writes.
type SharedBytes = Rc<RefCell<Vec<u8>>>;

/// An in-memory virtual file system.
#[derive(Default, Clone)]
pub struct MemoryVfs {
    files: Rc<RefCell<BTreeMap<String, SharedBytes>>>,
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
        let bytes = match files.get(path) {
            Some(b) => Rc::clone(b),
            None => {
                if !flags.create {
                    return Err(Error::CantOpen(String::from(path)));
                }
                let b: SharedBytes = Rc::new(RefCell::new(Vec::new()));
                files.insert(String::from(path), Rc::clone(&b));
                b
            }
        };
        Ok(Box::new(MemoryFile { bytes }))
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
