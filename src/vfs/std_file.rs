//! A [`Vfs`] backed by real files via `std::fs` (feature `std`).
//!
//! Positioned I/O is implemented portably with `Seek` + `Read`/`Write` behind a
//! `Mutex`, so reads can take `&self` while remaining correct across platforms
//! and without any `unsafe` or platform-specific `FileExt`.

use super::{File, OpenFlags, Vfs};
use crate::error::{Error, Result};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

/// A virtual file system over the real local file system.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdVfs;

impl StdVfs {
    /// Create a `StdVfs`.
    pub fn new() -> StdVfs {
        StdVfs
    }
}

fn io_err<E: core::fmt::Display>(e: E) -> Error {
    Error::Io(e.to_string())
}

impl Vfs for StdVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> Result<Box<dyn File>> {
        let mut opts = fs::OpenOptions::new();
        opts.read(true);
        if flags.write {
            opts.write(true);
        }
        if flags.create {
            opts.create(true);
        }
        let file = opts.open(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::CantOpen(String::from(path)),
            _ => io_err(e),
        })?;
        Ok(Box::new(StdFile {
            inner: Mutex::new(file),
        }))
    }

    fn delete(&self, path: &str) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }

    fn exists(&self, path: &str) -> Result<bool> {
        Ok(fs::metadata(path).is_ok())
    }
}

/// A handle to one real file.
pub struct StdFile {
    inner: Mutex<fs::File>,
}

impl StdFile {
    fn with<R>(&self, f: impl FnOnce(&mut fs::File) -> std::io::Result<R>) -> Result<R> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| Error::Io("poisoned file lock".into()))?;
        f(&mut guard).map_err(io_err)
    }
}

impl File for StdFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.with(|f| {
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(buf)
        })
    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        self.with(|f| {
            f.seek(SeekFrom::Start(offset))?;
            f.write_all(buf)
        })
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        self.with(|f| f.set_len(size))
    }

    fn sync(&mut self) -> Result<()> {
        self.with(|f| f.sync_all())
    }

    fn size(&self) -> Result<u64> {
        self.with(|f| Ok(f.metadata()?.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    fn temp_path(name: &str) -> String {
        let mut p = std::env::temp_dir();
        // Disambiguate within the test process without needing Date/random.
        p.push(format!("graphitesql-test-{}-{name}", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn write_read_truncate_roundtrip() {
        let vfs = StdVfs::new();
        let path = temp_path("rt");
        let _ = vfs.delete(&path);

        let mut f = vfs.open(&path, OpenFlags::READ_WRITE_CREATE).unwrap();
        f.write_all_at(b"graphite", 0).unwrap();
        f.sync().unwrap();
        assert_eq!(f.size().unwrap(), 8);

        let mut buf = [0u8; 4];
        f.read_exact_at(&mut buf, 4).unwrap();
        assert_eq!(&buf, b"hite");

        f.truncate(4).unwrap();
        assert_eq!(f.size().unwrap(), 4);

        vfs.delete(&path).unwrap();
        assert!(!vfs.exists(&path).unwrap());
    }

    #[test]
    fn open_missing_readonly_errors() {
        let vfs = StdVfs::new();
        let path = temp_path("missing");
        let _ = vfs.delete(&path);
        assert!(matches!(
            vfs.open(&path, OpenFlags::READ_ONLY),
            Err(Error::CantOpen(_))
        ));
    }
}
