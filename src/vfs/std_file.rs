//! A [`Vfs`] backed by real files via `std::fs` (feature `std`).
//!
//! Positioned I/O is implemented portably with `Seek` + `Read`/`Write` behind a
//! `Mutex`, so reads can take `&self` while remaining correct across platforms
//! and without any `unsafe` or platform-specific `FileExt`.

use super::{File, LockLevel, LockState, OpenFlags, Vfs};
use crate::error::{Error, Result};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use core::sync::atomic::{AtomicU8, Ordering};
use std::collections::HashMap;
use std::fs;

/// The current lock level of a [`StdFile`] handle, behind an atomic so `lock`/
/// `unlock` can update it through `&self` (see [`File::lock`]) while keeping the
/// public `StdFile` `Send + Sync + RefUnwindSafe` — a `Cell` here would silently
/// make `StdFile` neither (an observable, semver-breaking auto-trait regression).
struct AtomicLockLevel(AtomicU8);

impl AtomicLockLevel {
    fn new(level: LockLevel) -> Self {
        AtomicLockLevel(AtomicU8::new(level as u8))
    }

    fn get(&self) -> LockLevel {
        match self.0.load(Ordering::Relaxed) {
            0 => LockLevel::Unlocked,
            1 => LockLevel::Shared,
            2 => LockLevel::Reserved,
            3 => LockLevel::Pending,
            _ => LockLevel::Exclusive,
        }
    }

    fn set(&self, level: LockLevel) {
        self.0.store(level as u8, Ordering::Relaxed);
    }
}
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, OnceLock};

/// Per-path cross-process lock coordinator: the process-local aggregate
/// [`LockState`] plus **one** process-wide OS advisory lock (`std::fs::File`'s
/// 1.89 `lock`/`try_lock`/`unlock`), driven off the aggregate so all in-process
/// `Connection`s share a single OS lock (no intra-process self-conflict).
///
/// The mapping is **pessimistic**: the process holds an OS *exclusive* lock for
/// the whole duration of any write intent (`RESERVED` and up), and an OS *shared*
/// lock while only readers are active. This serialises cross-process writers
/// race-free (a single `try_lock`, never a release-then-reacquire that could drop
/// a writer's exclusivity), at the cost of blocking cross-process readers during a
/// write transaction — SQLite's byte-range `RESERVED` lock keeps readers during
/// writes, but std's whole-file locks cannot express that split (ROADMAP C9b). OS
/// locking is best-effort: a platform that errors (not `WouldBlock`) degrades to
/// the process-local behaviour; genuine contention surfaces as [`Error::Busy`].
struct CpLock {
    /// Process-local aggregate lock state (the source of truth within a process).
    state: LockState,
    /// The file path, for opening the OS-lock handle lazily.
    path: String,
    /// The process-wide OS-lock handle (opened on first lock).
    os: Option<fs::File>,
    /// The OS lock level the process currently holds (only `Unlocked`/`Shared`/
    /// `Exclusive` occur).
    os_level: LockLevel,
    /// Set once the platform reports the OS lock API is unusable — thereafter the
    /// coordinator is purely process-local.
    os_disabled: bool,
}

impl CpLock {
    fn new(path: &str) -> Self {
        CpLock {
            state: LockState::default(),
            path: String::from(path),
            os: None,
            os_level: LockLevel::Unlocked,
            os_disabled: false,
        }
    }

    /// The OS lock the process should hold given the aggregate `state`.
    fn desired_os(&self) -> LockLevel {
        if self.state.has_write_intent() {
            LockLevel::Exclusive
        } else if self.state.reader_count() > 0 {
            LockLevel::Shared
        } else {
            LockLevel::Unlocked
        }
    }

    /// Reconcile the process-wide OS lock with [`desired_os`](Self::desired_os).
    /// Returns [`Error::Busy`] only on genuine cross-process contention (the OS
    /// reports `WouldBlock`); any other OS error disables OS locking and degrades
    /// to process-local coordination.
    fn reconcile(&mut self) -> Result<()> {
        if self.os_disabled {
            return Ok(());
        }
        let want = self.desired_os();
        if want == self.os_level {
            return Ok(());
        }
        if want != LockLevel::Unlocked && self.os.is_none() {
            // Open the data file purely for OS locking (it already exists — the
            // db is open). Fall back to a read-only handle for a read-only db.
            let opened = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)
                .or_else(|_| fs::OpenOptions::new().read(true).open(&self.path));
            match opened {
                Ok(f) => self.os = Some(f),
                Err(_) => {
                    self.os_disabled = true;
                    return Ok(());
                }
            }
        }
        let Some(f) = self.os.as_ref() else {
            return Ok(());
        };
        let res: core::result::Result<(), std::fs::TryLockError> = match want {
            LockLevel::Unlocked => {
                let _ = f.unlock();
                Ok(())
            }
            LockLevel::Shared if self.os_level == LockLevel::Exclusive => {
                // Downgrade EX → SH (blocking; there are no cross-process holders
                // to wait on since we hold exclusive).
                f.lock_shared().map_err(std::fs::TryLockError::Error)
            }
            LockLevel::Shared => f.try_lock_shared(),
            LockLevel::Exclusive => f.try_lock(),
            _ => Ok(()),
        };
        match res {
            Ok(()) => {
                self.os_level = want;
                Ok(())
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::Busy),
            Err(std::fs::TryLockError::Error(_)) => {
                // The platform can't advisory-lock — degrade to process-local.
                self.os_disabled = true;
                self.os = None;
                self.os_level = LockLevel::Unlocked;
                Ok(())
            }
        }
    }
}

/// Process-global registry of per-path [`CpLock`] coordinators. Real files are
/// process-global, so every `StdFile` handle to the same path shares one
/// coordinator (and thus one process-wide OS lock).
fn lock_registry() -> &'static Mutex<HashMap<String, Arc<Mutex<CpLock>>>> {
    static REG: OnceLock<Mutex<HashMap<String, Arc<Mutex<CpLock>>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The shared lock coordinator for `path`, creating it on first use.
fn locks_for(path: &str) -> Arc<Mutex<CpLock>> {
    let mut reg = lock_registry().lock().expect("lock registry poisoned");
    reg.entry(path.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(CpLock::new(path))))
        .clone()
}

/// Process-global registry of per-path wal-indexes (ROADMAP C9c). Real files are
/// process-global, so every `StdFile` handle to the same `-wal` path shares one
/// wal-index, letting multiple in-process `Connection`s read WAL frames
/// coherently. Like [`lock_registry`], this is process-local coordination (no
/// cross-process `-shm`); a host needing multi-process WAL supplies its own VFS.
fn wal_index_registry() -> &'static Mutex<HashMap<String, crate::pager::SharedWalIndex>> {
    static REG: OnceLock<Mutex<HashMap<String, crate::pager::SharedWalIndex>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The shared wal-index for `path`, creating it on first use.
fn wal_index_for(path: &str) -> crate::pager::SharedWalIndex {
    let mut reg = wal_index_registry()
        .lock()
        .expect("wal-index registry poisoned");
    reg.entry(path.to_string()).or_default().clone()
}

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
            locks: locks_for(path),
            wal_index: wal_index_for(path),
            level: AtomicLockLevel::new(LockLevel::Unlocked),
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
    /// The process-shared cross-process lock coordinator for this file's path.
    locks: Arc<Mutex<CpLock>>,
    /// The process-shared wal-index for this file's path (ROADMAP C9c).
    wal_index: crate::pager::SharedWalIndex,
    /// The lock level this handle currently holds. Atomic so `lock`/`unlock` can
    /// take `&self` (see [`File::lock`]) without making `StdFile` `!Sync`.
    level: AtomicLockLevel,
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

impl Drop for StdFile {
    fn drop(&mut self) {
        if let Ok(mut s) = self.locks.lock() {
            s.state.release(self.level.get(), LockLevel::Unlocked);
            let _ = s.reconcile();
        }
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

    fn lock(&self, level: LockLevel) -> Result<()> {
        let from = self.level.get();
        let mut s = self
            .locks
            .lock()
            .map_err(|_| Error::Io("lock state poisoned".into()))?;
        // Process-local first (source of truth within the process), then reconcile
        // the process-wide OS lock; on cross-process contention roll the local
        // acquire back so the handle's state stays consistent.
        s.state.acquire(from, level)?;
        if let Err(e) = s.reconcile() {
            s.state.release(level, from);
            let _ = s.reconcile();
            return Err(e);
        }
        drop(s);
        if level > from {
            self.level.set(level);
        }
        Ok(())
    }

    fn unlock(&self, level: LockLevel) -> Result<()> {
        let from = self.level.get();
        let mut s = self
            .locks
            .lock()
            .map_err(|_| Error::Io("lock state poisoned".into()))?;
        s.state.release(from, level);
        let _ = s.reconcile();
        drop(s);
        if level < from {
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
    use alloc::format;

    // `StdFile` is public API and was `Send + Sync + RefUnwindSafe` in 0.1.0.
    // A `Cell`-based lock level silently regresses those auto-traits (and would
    // block a thread-safe `Connection`); this compile-time check guards it.
    const _: fn() = || {
        fn assert<T: Send + Sync + std::panic::RefUnwindSafe + std::panic::UnwindSafe>() {}
        assert::<StdFile>();
    };

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
