//! The write-side pager: buffered page mutations with atomic, journaled commit.
//!
//! All mutations during a transaction are buffered in an in-memory *overlay*
//! (page number → full page image). Nothing touches the database file until
//! [`commit`](WritePager::commit):
//!
//! * **ROLLBACK** is therefore trivial and always correct — drop the overlay.
//! * **COMMIT** is made crash-safe with a rollback journal: the original
//!   contents of every page about to be overwritten are written to a journal
//!   file and synced *before* the database file is modified; the journal is
//!   cleared only after the database is synced. A crash mid-commit leaves a
//!   journal that the next [`open`](WritePager::open) replays via recovery.
//!
//! Reads consult the overlay first, so within a transaction the pager is
//! read-your-writes consistent. It implements [`PageSource`], so the existing
//! b-tree cursors and schema reader work over it unchanged.

use super::pcache::{self, PageCache};
use super::{Page, PageSource};
use crate::btree::page::{BtreePage, PageType};
use crate::btree::ptrmap::{self, PtrmapType};
use crate::error::{Error, Result};
use crate::format::header::HEADER_LEN;
use crate::format::{DatabaseHeader, TextEncoding};
use crate::vfs::File;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

/// The 8-byte magic that opens every SQLite-format rollback journal.
///
/// SQLite writes these exact bytes at offset 0; a journal is only "well-formed"
/// (and therefore a candidate for hot-journal recovery) if they are present.
const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// Bytes occupied by the fixed fields of the journal header (magic, record
/// count, checksum nonce, initial page count, sector size, page size). The
/// header is then zero-padded out to [`JOURNAL_SECTOR`].
const JOURNAL_HDR_FIELDS: usize = 28;

/// Sector size assumed for the rollback journal. SQLite hard-codes 512 bytes
/// (there is no portable way to discover the true sector size), so we record the
/// same value at header offset 20 and pad the header out to this boundary. Each
/// page record begins on this boundary so a torn write of the header sector
/// cannot damage the page records that follow.
const JOURNAL_SECTOR: u64 = 512;

/// Compute the 4-byte page checksum exactly as SQLite does: seed with the
/// per-transaction `nonce`, then add the unsigned byte value at offsets
/// `len-200, len-400, …` down to the first non-negative index. The sum wraps in
/// `u32`. A sparse sample (every 200th byte) is intentional — it is what SQLite
/// uses, so the value must match byte-for-byte for cross-recovery.
fn journal_page_checksum(nonce: u32, page: &[u8]) -> u32 {
    let mut cksum = nonce;
    // X starts at len-200 and steps down by 200 while >= 0.
    let mut x = page.len() as isize - 200;
    while x >= 0 {
        cksum = cksum.wrapping_add(page[x as usize] as u32);
        x -= 200;
    }
    cksum
}

/// Fixed file offset of the SQLite "lock byte". The page that contains this byte
/// is never used to store data; on databases larger than 1 GiB it falls on a
/// page after page 1 that the allocator skips.
const PENDING_BYTE: u64 = 0x4000_0000;

/// A pointer-map entry: the page's type and its parent page number.
type PtrmapEntry = (PtrmapType, u32);

/// Auto-vacuum mode of a database, as recorded in the file header.
///
/// SQLite encodes the mode in two header fields: `largest_root_page` (offset 52)
/// is non-zero iff auto-vacuum is enabled, and `incremental_vacuum` (offset 64)
/// distinguishes FULL (0) from INCREMENTAL (non-zero). See the file-format spec,
/// "The Database Header".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoVacuum {
    /// Auto-vacuum disabled (the default). `PRAGMA auto_vacuum` reports `0`.
    None,
    /// Full auto-vacuum: free pages are reclaimed automatically on each commit.
    /// `PRAGMA auto_vacuum` reports `1`.
    Full,
    /// Incremental auto-vacuum: free pages are tracked but only reclaimed on
    /// `PRAGMA incremental_vacuum`. `PRAGMA auto_vacuum` reports `2`.
    Incremental,
}

/// A writable pager over a database file, with an optional journal file.
pub struct WritePager {
    file: Box<dyn File>,
    journal: Option<Box<dyn File>>,
    header: DatabaseHeader,
    page_size: usize,
    /// Pages currently on disk (the durable page count).
    disk_pages: u32,
    /// Logical page count including not-yet-flushed allocations.
    page_count: u32,
    overlay: BTreeMap<u32, Vec<u8>>,
    /// The `-wal` file handle, when one was supplied (file-backed databases).
    wal_file: Option<Box<dyn File>>,
    /// WAL runtime state; `Some` when the database is in WAL mode.
    wal: Option<WalRuntime>,
    /// The write lock currently held on the main file. A write transaction takes
    /// `Reserved` on its first staged page and upgrades to `Exclusive` while
    /// flushing at commit, so a second writer to the same file gets
    /// [`Error::Busy`](crate::Error::Busy). Released on commit/rollback.
    held: crate::vfs::LockLevel,
    /// Whether an explicit read transaction is open on this connection. When set,
    /// the pager holds a persistent `Shared` lock for the whole read transaction
    /// (taken by [`begin_read_txn`](Self::begin_read_txn)) rather than only
    /// transiently per page. This makes the reader visible to a concurrent writer
    /// on the same file: the writer's commit-time upgrade to `Exclusive` BUSYs
    /// until the reader ends its transaction, matching SQLite's locking model
    /// (`pager.c` `PAGER_SHARED` held across a read txn). Multiple such readers
    /// still coexist (`Shared` is a counted lock). Cleared and the lock released
    /// by [`end_read_txn`](Self::end_read_txn).
    read_txn: bool,
    /// Open savepoints (innermost last); each snapshots the staged state so
    /// `ROLLBACK TO` can restore it.
    savepoints: Vec<Savepoint>,
    /// `PRAGMA secure_delete`: when set, the content of a page handed to the
    /// freelist is overwritten with zeros (so deleted data does not linger on
    /// disk). A per-connection runtime setting, not persisted in the file.
    secure_delete: bool,
    /// Bounded LRU cache of **clean** pages read from the main file (ROADMAP
    /// C8c). It only ever holds pages served straight from disk — never an
    /// overlay (dirty) or WAL page, both of which `read_page` consults first — so
    /// no dirty page can be evicted from here. A long read-heavy scan evicts
    /// least-recently-used clean pages instead of growing without bound; an
    /// evicted page is simply re-read from disk. Wrapped in a `RefCell` because
    /// `read_page` takes `&self`.
    read_cache: RefCell<PageCache>,
}

/// A snapshot of the pager's staged state captured by `SAVEPOINT`.
struct Savepoint {
    name: String,
    overlay: BTreeMap<u32, Vec<u8>>,
    header: DatabaseHeader,
    page_count: u32,
}

/// Live WAL state: the committed frames overlaid on the main file plus the
/// append cursor, running checksum, salts, and last-commit page count.
struct WalRuntime {
    /// Committed frame contents (page number → page bytes).
    frames: BTreeMap<u32, Vec<u8>>,
    /// Byte offset in the `-wal` file at which the next frame is appended.
    offset: u64,
    /// Running checksum after the last appended frame (or the WAL header).
    cksum: (u32, u32),
    /// The two 4-byte salts written into the WAL header and every frame.
    salt: [u8; 8],
    /// Database size in pages recorded by the last commit frame.
    db_size: u32,
}

const WAL_MAGIC_LE: u32 = 0x377f_0682; // little-endian checksum variant
const WAL_HDR_LEN: usize = 32;
const WAL_FRAME_HDR_LEN: usize = 24;

impl WritePager {
    /// Open an existing database file for writing. Replays the journal first if a
    /// previous commit was interrupted.
    pub fn open(file: Box<dyn File>, journal: Option<Box<dyn File>>) -> Result<WritePager> {
        Self::open_wal(file, journal, None)
    }

    /// Like [`open`](Self::open), but also given the `-wal` companion file so the
    /// database can be opened (and reopened) in WAL mode.
    pub fn open_wal(
        mut file: Box<dyn File>,
        mut journal: Option<Box<dyn File>>,
        mut wal_file: Option<Box<dyn File>>,
    ) -> Result<WritePager> {
        if let Some(j) = journal.as_mut() {
            Self::recover(file.as_mut(), j.as_mut())?;
        }
        let file_size = file.size()?;
        if file_size < HEADER_LEN as u64 {
            return Err(Error::Corrupt("file too small to be a database".into()));
        }
        let mut head = [0u8; HEADER_LEN];
        file.read_exact_at(&mut head, 0)?;
        let header = DatabaseHeader::parse(&head)?;
        let page_size = header.page_size as usize;
        if file_size % page_size as u64 != 0 {
            return Err(Error::Corrupt(
                "file size not a multiple of page size".into(),
            ));
        }
        let pages = (file_size / page_size as u64) as u32;
        // If the database is in WAL mode and the -wal file carries committed
        // frames, load them so reads see the latest data after a reopen.
        let wal = if header.read_version == 2 {
            match wal_file.as_mut() {
                Some(w) => Self::load_wal(w.as_mut(), page_size)?,
                None => None,
            }
        } else {
            None
        };
        let page_count = wal.as_ref().map(|w| w.db_size).unwrap_or(pages);
        Ok(WritePager {
            file,
            journal,
            header,
            page_size,
            disk_pages: pages,
            page_count,
            overlay: BTreeMap::new(),
            wal_file,
            wal,
            held: crate::vfs::LockLevel::Unlocked,
            read_txn: false,
            savepoints: Vec::new(),
            secure_delete: false,
            read_cache: RefCell::new(PageCache::new(pcache::DEFAULT_CACHE_SIZE, page_size)),
        })
    }

    /// Create a brand-new, empty database (a single `sqlite_schema` leaf page).
    pub fn create(
        file: Box<dyn File>,
        journal: Option<Box<dyn File>>,
        page_size: u32,
    ) -> Result<WritePager> {
        Self::create_wal(file, journal, None, page_size)
    }

    /// Like [`create`](Self::create), with the `-wal` companion file available.
    pub fn create_wal(
        file: Box<dyn File>,
        journal: Option<Box<dyn File>>,
        wal_file: Option<Box<dyn File>>,
        page_size: u32,
    ) -> Result<WritePager> {
        Self::create_auto_vacuum(file, journal, wal_file, page_size, AutoVacuum::None)
    }

    /// Create a brand-new, empty database in the given auto-vacuum `mode`.
    ///
    /// For an *empty* database (just page 1, no user tables) SQLite records the
    /// mode purely in the header: `largest_root_page` (offset 52) is set to `1`
    /// when auto-vacuum is enabled (FULL or INCREMENTAL) and `0` when it is off,
    /// and `incremental_vacuum` (offset 64) is `1` for INCREMENTAL, `0`
    /// otherwise. The file is still a single page — no pointer-map page exists
    /// yet, so there is no ptrmap maintenance to do here. (Those values were
    /// confirmed empirically against `sqlite3 3.50.4`.)
    ///
    /// This is the storage foundation for auto-vacuum; the ptrmap *write*
    /// maintenance that keeps the map current as pages are allocated/freed is a
    /// separate concern layered on top.
    pub fn create_auto_vacuum(
        file: Box<dyn File>,
        journal: Option<Box<dyn File>>,
        wal_file: Option<Box<dyn File>>,
        page_size: u32,
        mode: AutoVacuum,
    ) -> Result<WritePager> {
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(Error::Error(format!("invalid page size {page_size}")));
        }
        // Empty auto-vacuum db: page 1 is its own largest (and only) root.
        let (largest_root_page, incremental_vacuum) = match mode {
            AutoVacuum::None => (0, 0),
            AutoVacuum::Full => (1, 0),
            AutoVacuum::Incremental => (1, 1),
        };
        let header = DatabaseHeader {
            page_size,
            write_version: 1,
            read_version: 1,
            reserved_space: 0,
            change_counter: 1,
            size_in_pages: 1,
            freelist_trunk: 0,
            freelist_count: 0,
            schema_cookie: 0,
            schema_format: 4,
            default_cache_size: 0,
            largest_root_page,
            text_encoding: TextEncoding::Utf8,
            user_version: 0,
            incremental_vacuum,
            application_id: 0,
            version_valid_for: 1,
            sqlite_version_number: 3_053_002,
        };
        let mut wp = WritePager {
            file,
            journal,
            header,
            page_size: page_size as usize,
            disk_pages: 0,
            page_count: 1,
            overlay: BTreeMap::new(),
            wal_file,
            wal: None,
            held: crate::vfs::LockLevel::Unlocked,
            read_txn: false,
            savepoints: Vec::new(),
            secure_delete: false,
            read_cache: RefCell::new(PageCache::new(
                pcache::DEFAULT_CACHE_SIZE,
                page_size as usize,
            )),
        };
        // Page 1: db header (0..100) + an empty table-leaf b-tree at offset 100.
        let mut page1 = vec![0u8; page_size as usize];
        wp.header.write_to(&mut page1)?;
        write_empty_leaf_header(&mut page1, HEADER_LEN, page_size);
        wp.overlay.insert(1, page1);
        Ok(wp)
    }

    /// The database header (reflects in-transaction changes once committed).
    /// Touching it marks page 1 dirty so a header-only change (e.g.
    /// `PRAGMA user_version=…`) is still flushed by `commit`, which otherwise
    /// short-circuits when no pages changed.
    pub fn header_mut(&mut self) -> &mut DatabaseHeader {
        if !self.overlay.contains_key(&1)
            && let Ok(p) = self.read_page(1)
        {
            self.overlay.insert(1, p);
        }
        &mut self.header
    }

    /// Read the full bytes of page `number` (overlay first, then WAL, then disk).
    ///
    /// Clean pages read from the main file are served through a bounded LRU cache
    /// (the `read_cache` field) so a long scan does not grow the resident set
    /// without bound. The overlay (dirty pages) and the WAL are consulted *before*
    /// the cache, so the cache only ever holds — and only ever returns — clean
    /// on-disk pages; a dirty page is therefore never evictable from it.
    ///
    /// The cache is only consulted while this connection holds a write lock on the
    /// file (i.e. inside its own write transaction). With no lock held, a *foreign*
    /// connection over the same VFS may have committed under us, so the cache could
    /// be stale: we then read straight from disk and keep the cache cold. Within a
    /// write transaction this connection is the only writer (it holds at least
    /// `Reserved`), so the on-disk clean pages cannot change beneath the cache.
    pub fn read_page(&self, number: u32) -> Result<Vec<u8>> {
        if let Some(bytes) = self.overlay.get(&number) {
            return Ok(bytes.clone());
        }
        // In WAL mode, the newest version of a page may live in the WAL.
        if let Some(w) = &self.wal
            && let Some(bytes) = w.frames.get(&number)
        {
            return Ok(bytes.clone());
        }
        if number == 0 || number > self.disk_pages {
            return Err(Error::Corrupt(format!("page {number} out of range")));
        }
        // Only trust the cache while we hold a write lock; otherwise a foreign
        // writer could have changed the file out from under it (see method docs).
        let cacheable = self.held >= crate::vfs::LockLevel::Reserved;
        if cacheable && let Some(bytes) = self.read_cache.borrow_mut().get(number) {
            return Ok(bytes.as_ref().clone());
        }
        let mut buf = vec![0u8; self.page_size];
        self.file
            .read_exact_at(&mut buf, (number as u64 - 1) * self.page_size as u64)?;
        if cacheable {
            let data = Rc::new(buf);
            self.read_cache
                .borrow_mut()
                .insert(number, Rc::clone(&data));
            return Ok(data.as_ref().clone());
        }
        Ok(buf)
    }

    /// Reconfigure the bounded clean-page read cache from a `cache_size` value
    /// (the `cache_size` PRAGMA convention: a positive value is a page count, a
    /// negative value is KiB of memory). Lowering it evicts the
    /// least-recently-used clean pages immediately. Dirty pages live in the
    /// overlay and are unaffected.
    pub fn set_cache_size(&self, cache_size: i64) {
        self.read_cache
            .borrow_mut()
            .set_cache_size(cache_size, self.page_size);
    }

    /// The number of **clean** pages currently resident in the read cache.
    /// Read-only accessor used to assert the LRU bound holds; not part of the
    /// stable API. Dirty (overlay) pages are not counted here — they are tracked
    /// separately and never evicted.
    #[doc(hidden)]
    pub fn resident_clean_pages(&self) -> usize {
        self.read_cache.borrow().len()
    }

    /// The number of dirty (staged, not-yet-committed) pages held in the overlay.
    /// These are never evictable. Read-only accessor for tests; not stable API.
    #[doc(hidden)]
    pub fn resident_dirty_pages(&self) -> usize {
        self.overlay.len()
    }

    /// Stage a full page image into the overlay.
    pub fn write_page(&mut self, number: u32, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() != self.page_size {
            return Err(Error::Error("page image has wrong size".into()));
        }
        self.acquire_write_intent()?;
        self.overlay.insert(number, bytes);
        Ok(())
    }

    /// Begin an explicit read transaction: acquire a **persistent** `Shared`
    /// (read) lock and hold it until [`end_read_txn`](Self::end_read_txn).
    ///
    /// The pager otherwise only touches `Shared` on the path to a write lock;
    /// pure reads are served without holding one. That is fine for autocommit
    /// reads, but an *open* read transaction (`BEGIN; SELECT …`) must keep the
    /// database's committed snapshot stable and — like SQLite — make itself
    /// visible to a concurrent writer, so the writer's commit-time upgrade to
    /// `Exclusive` BUSYs until this reader finishes. This method installs that
    /// persistent lock.
    ///
    /// - Idempotent: calling it again inside the same read transaction is a no-op.
    /// - If a *write* lock is already held (a read followed by a write in the same
    ///   transaction, or a write transaction that also reads), the existing
    ///   `Reserved`/`Exclusive` lock already excludes other writers, so this just
    ///   records that a read transaction is open without weakening the lock.
    /// - Multiple connections may hold the persistent `Shared` lock at once
    ///   (`Shared` is a counted lock); readers never block each other.
    ///
    /// Returns [`Error::Busy`] if a foreign writer already holds `Pending`/
    /// `Exclusive` on the file (a reader cannot start while a writer is draining
    /// readers, matching SQLite).
    pub fn begin_read_txn(&mut self) -> Result<()> {
        use crate::vfs::LockLevel;
        if self.held < LockLevel::Shared {
            self.file.lock(LockLevel::Shared)?;
            self.held = LockLevel::Shared;
        }
        self.read_txn = true;
        Ok(())
    }

    /// End an explicit read transaction opened by
    /// [`begin_read_txn`](Self::begin_read_txn), releasing the persistent
    /// `Shared` lock so a waiting writer can now upgrade to `Exclusive`.
    ///
    /// Only the read-transaction bookkeeping and a *pure* `Shared` lock are
    /// dropped here. If the transaction turned into a write (the pager now holds
    /// `Reserved`/`Exclusive`), the lock is left to the write path's
    /// commit/rollback (`release_locks`) — ending the read side must not strand a
    /// half-committed writer. Idempotent when no read transaction is open.
    pub fn end_read_txn(&mut self) {
        use crate::vfs::LockLevel;
        self.read_txn = false;
        if self.held == LockLevel::Shared {
            let _ = self.file.unlock(LockLevel::Unlocked);
            self.held = LockLevel::Unlocked;
            // A foreign writer may now change the file, so anything cached under
            // the read lock must not be served to a later read.
            self.read_cache.borrow_mut().clear();
        }
    }

    /// Whether an explicit read transaction (persistent `Shared` lock) is open.
    /// Read-only accessor for tests and the exec layer's txn bookkeeping.
    pub fn in_read_txn(&self) -> bool {
        self.read_txn
    }

    /// Take the write-intent (`RESERVED`) lock on the main file before staging
    /// changes, so a concurrent writer to the same file is rejected with
    /// [`Error::Busy`] rather than corrupting it. Idempotent within a transaction.
    fn acquire_write_intent(&mut self) -> Result<()> {
        use crate::vfs::LockLevel;
        let entry = self.held;
        if self.held < LockLevel::Shared {
            self.file.lock(LockLevel::Shared)?;
            self.held = LockLevel::Shared;
        }
        if self.held < LockLevel::Reserved {
            if let Err(e) = self.file.lock(LockLevel::Reserved) {
                // A rejected first write must not keep the SHARED lock it just
                // took, or it would block the current writer from committing.
                if entry == LockLevel::Unlocked {
                    self.release_locks();
                }
                return Err(e);
            }
            self.held = LockLevel::Reserved;
            // Entering a write transaction: a foreign connection over the same
            // VFS may have committed since we last held a lock, so anything in
            // the clean read cache could be stale. Drop it; pages are re-read
            // from disk and re-cached fresh under the write lock.
            self.read_cache.borrow_mut().clear();
        }
        Ok(())
    }

    /// Upgrade to the `EXCLUSIVE` lock for the flush phase of a commit.
    fn acquire_exclusive(&mut self) -> Result<()> {
        use crate::vfs::LockLevel;
        self.acquire_write_intent()?;
        if self.held < LockLevel::Exclusive {
            self.file.lock(LockLevel::Exclusive)?;
            self.held = LockLevel::Exclusive;
        }
        Ok(())
    }

    /// Drop all locks at the end of a transaction — the persistent read
    /// (`Shared`) lock as well as any write (`Reserved`/`Exclusive`) lock.
    fn release_locks(&mut self) {
        use crate::vfs::LockLevel;
        self.read_txn = false;
        if self.held != LockLevel::Unlocked {
            let _ = self.file.unlock(LockLevel::Unlocked);
            self.held = LockLevel::Unlocked;
            // Once unlocked, a foreign writer may change the file, so anything
            // we cached under the lock must not be served to a later read.
            self.read_cache.borrow_mut().clear();
        }
    }

    /// Allocate a page, reusing one from the freelist if available, otherwise
    /// extending the file. The returned page is staged zeroed in the overlay.
    ///
    /// In auto-vacuum mode the file is interleaved with pointer-map pages; when
    /// extending the file we must never hand out a ptrmap page number. If the
    /// next page would be a ptrmap page (or the lock-byte page), we allocate and
    /// zero it in place and advance to the following page.
    pub fn allocate_page(&mut self) -> Result<u32> {
        if self.header.freelist_count > 0 && self.header.freelist_trunk != 0 {
            return self.alloc_from_freelist();
        }
        let auto_vacuum = self.auto_vacuum_on();
        let usable = self.usable_size() as u32;
        loop {
            self.page_count += 1;
            let n = self.page_count;
            if auto_vacuum && self.is_reserved_layout_page(n, usable) {
                // Materialize the ptrmap (or lock-byte) page zeroed and keep
                // going; its contents are filled in by `rebuild_ptrmap` at commit.
                self.overlay.insert(n, vec![0u8; self.page_size]);
                continue;
            }
            self.overlay.insert(n, vec![0u8; self.page_size]);
            return Ok(n);
        }
    }

    /// Whether the database is in any auto-vacuum mode (FULL or INCREMENTAL).
    fn auto_vacuum_on(&self) -> bool {
        self.header.largest_root_page != 0
    }

    /// Whether `pgno` is a structural page that must not be handed out as data:
    /// a pointer-map page, or the page that contains the lock byte (only when the
    /// page size makes that byte fall on a page other than page 1).
    fn is_reserved_layout_page(&self, pgno: u32, usable: u32) -> bool {
        if ptrmap::is_ptrmap_page(usable, pgno) {
            return true;
        }
        // The lock byte lives at fixed file offset 2^30. The page holding it is
        // skipped by the allocator. For page sizes ≤ 1 GiB on small databases
        // this never triggers, but we stay correct for large files.
        let lock_page = (PENDING_BYTE / self.page_size as u64) as u32 + 1;
        lock_page > 1 && pgno == lock_page
    }

    /// Pop a page off the freelist (file-format spec, "The Freelist"). The
    /// freelist is trunk pages, each holding a next-trunk pointer, a leaf count,
    /// and that many free-page numbers.
    fn alloc_from_freelist(&mut self) -> Result<u32> {
        let trunk = self.header.freelist_trunk;
        let mut tbytes = self.read_page(trunk)?;
        let leaf_count = be32(&tbytes, 4);
        if leaf_count > 0 {
            // Reuse the last leaf; the trunk stays on the list.
            let idx = 8 + 4 * (leaf_count as usize - 1);
            let leaf = be32(&tbytes, idx);
            put32(&mut tbytes, 4, leaf_count - 1);
            self.write_page(trunk, tbytes)?;
            self.header.freelist_count -= 1;
            self.overlay.insert(leaf, vec![0u8; self.page_size]);
            Ok(leaf)
        } else {
            // No leaves: consume the trunk page itself; its successor heads the list.
            let next = be32(&tbytes, 0);
            self.header.freelist_trunk = next;
            self.header.freelist_count -= 1;
            self.overlay.insert(trunk, vec![0u8; self.page_size]);
            Ok(trunk)
        }
    }

    /// Set `PRAGMA secure_delete`: when `on`, the content of a freed page is
    /// zeroed before it joins the freelist.
    pub fn set_secure_delete(&mut self, on: bool) {
        self.secure_delete = on;
    }

    /// Return `page` to the freelist (appending it as a leaf of the first trunk
    /// if there is room, else making it a new trunk page).
    pub fn free_page(&mut self, page: u32) -> Result<()> {
        let trunk = self.header.freelist_trunk;
        // SQLite caps trunk leaves at usable/4 - 2 (integrity_check enforces it).
        let max_leaves = (self.usable_size() / 4).saturating_sub(2) as u32;
        if trunk != 0 {
            let mut tbytes = self.read_page(trunk)?;
            let leaf_count = be32(&tbytes, 4);
            if leaf_count < max_leaves {
                let idx = 8 + 4 * leaf_count as usize;
                put32(&mut tbytes, idx, page);
                put32(&mut tbytes, 4, leaf_count + 1);
                self.write_page(trunk, tbytes)?;
                // With secure_delete, overwrite the freed page's old content. As a
                // freelist *leaf* it carries no required bytes, so a zero page is
                // valid (integrity_check ignores freelist-leaf content).
                if self.secure_delete {
                    self.write_page(page, vec![0u8; self.page_size])?;
                }
                self.header.freelist_count += 1;
                return Ok(());
            }
        }
        // Make `page` a new trunk pointing at the previous head.
        let mut nb = vec![0u8; self.page_size];
        put32(&mut nb, 0, trunk); // next trunk
        put32(&mut nb, 4, 0); // leaf count
        self.write_page(page, nb)?;
        self.header.freelist_trunk = page;
        self.header.freelist_count += 1;
        Ok(())
    }

    /// Rebuild every pointer-map page from the current logical page contents.
    ///
    /// Only called in auto-vacuum mode, just before a commit flushes pages. It
    /// derives the (type, parent) of every tracked page by walking the b-tree
    /// forest (roots from `sqlite_schema`, plus page 1's own schema tree) and the
    /// freelist, then re-stages the affected ptrmap pages into the overlay so the
    /// flush writes them. This whole-map rebuild keeps the ptrmap correct across
    /// arbitrary structural changes (splits, merges, overflow chains, frees)
    /// without threading parent bookkeeping through every b-tree writer.
    ///
    /// It also stamps `largest_root_page` = max root page number, which
    /// `integrity_check` requires to match exactly.
    /// Derive the desired pointer-map entries `(type, parent)` for every tracked
    /// page from the current logical page contents, plus the largest root page
    /// number. Shared by [`rebuild_ptrmap`](Self::rebuild_ptrmap) and the
    /// FULL-mode commit-time relocator, which needs the same parent bookkeeping
    /// to fix references to moved pages.
    fn compute_want(&self) -> Result<(BTreeMap<u32, PtrmapEntry>, u32)> {
        let mut want: BTreeMap<u32, PtrmapEntry> = BTreeMap::new();
        // 1. Discover all b-tree roots: the schema tree (page 1) and every
        //    `rootpage` recorded in sqlite_schema. Roots get a RootPage entry
        //    (page 1 is never tracked). Then walk each tree for Btree/overflow.
        let mut roots: Vec<u32> = Vec::new();
        roots.push(1);
        let mut max_root = 1u32;
        for root in self.schema_roots()? {
            if root >= 2 {
                want.insert(root, (PtrmapType::RootPage, 0));
            }
            if root > max_root {
                max_root = root;
            }
            roots.push(root);
        }
        for root in roots {
            self.walk_btree_ptrmap(root, &mut want)?;
        }
        // 2. Freelist pages (trunk + leaves) are FreePage with parent 0.
        self.walk_freelist_ptrmap(&mut want)?;
        Ok((want, max_root))
    }

    fn rebuild_ptrmap(&mut self) -> Result<()> {
        let usable = self.usable_size() as u32;
        // Desired (type, parent) for every tracked page, plus the max root page.
        let (want, max_root) = self.compute_want()?;

        // Keep the header's largest_root_page in sync.
        self.header.largest_root_page = max_root;

        // Materialize the ptrmap pages: gather the entries that fall on each,
        //    and write them. Pages with no live entry keep their slot's previous
        //    bytes (integrity_check never reads an unreferenced slot).
        let mut by_map: BTreeMap<u32, Vec<(u32, PtrmapEntry)>> = BTreeMap::new();
        for (&pgno, &ent) in &want {
            let map = ptrmap::ptrmap_pageno(usable, pgno);
            by_map.entry(map).or_default().push((pgno, ent));
        }
        for (map_pg, entries) in by_map {
            let mut page = if self.has_page(map_pg) {
                self.read_page(map_pg)?
            } else {
                vec![0u8; self.page_size]
            };
            for (pgno, (kind, parent)) in entries {
                let off = ptrmap::ptrmap_entry_offset(usable, pgno);
                let enc = ptrmap::encode_entry(kind, parent);
                page[off..off + ptrmap::ENTRY_SIZE].copy_from_slice(&enc);
            }
            self.overlay.insert(map_pg, page);
        }
        Ok(())
    }

    /// Run FULL-mode commit-time truncation, best-effort: only in FULL auto-vacuum
    /// mode, and any error rolls back to the pre-relocation staged state so the
    /// commit proceeds via the sound "leave freed pages in place" path. A
    /// sound-but-not-maximally-compact file is always acceptable; a corrupt one
    /// never is.
    fn maybe_autovacuum_truncate(&mut self) {
        if self.auto_vacuum() != AutoVacuum::Full {
            return;
        }
        let saved_overlay = self.overlay.clone();
        let saved_header = self.header.clone();
        let saved_count = self.page_count;
        // `limit == 0` ⇒ reclaim as much as possible (full compaction).
        if self.autovacuum_truncate(0).is_err() {
            self.overlay = saved_overlay;
            self.header = saved_header;
            self.page_count = saved_count;
        }
    }

    /// `PRAGMA incremental_vacuum(n)`: reclaim up to `n` free pages off the end of
    /// the file for an `auto_vacuum=INCREMENTAL` database, returning the number of
    /// pages actually removed. When `n <= 0` it reclaims as many as possible (full
    /// compaction). The reclaimed pages are dropped from the staged image; a
    /// subsequent [`commit`](Self::commit) makes the smaller file durable.
    ///
    /// Outside INCREMENTAL mode this is a no-op that reclaims nothing, matching
    /// SQLite (which does nothing for `auto_vacuum` NONE/FULL). Like the FULL-mode
    /// commit-time truncation it reuses, it is best-effort: any internal relocation
    /// error rolls the staged state back to its pre-reclamation form, so the worst
    /// case is a sound-but-less-compact file, never a corrupt one.
    pub fn incremental_vacuum(&mut self, n: i64) -> Result<u32> {
        if self.auto_vacuum() != AutoVacuum::Incremental {
            return Ok(0);
        }
        // `n <= 0` ⇒ unbounded (`limit == 0`); otherwise cap the pages truncated.
        let limit = if n <= 0 {
            0
        } else {
            n.min(u32::MAX as i64) as u32
        };
        let before = self.page_count;
        let saved_overlay = self.overlay.clone();
        let saved_header = self.header.clone();
        let saved_count = self.page_count;
        if self.autovacuum_truncate(limit).is_err() {
            self.overlay = saved_overlay;
            self.header = saved_header;
            self.page_count = saved_count;
            return Ok(0);
        }
        Ok(before - self.page_count)
    }

    /// FULL `auto_vacuum` commit-time truncation (SQLite's `autoVacuumCommit`).
    ///
    /// Relocates pages from the end of the file into lower-numbered free slots and
    /// shrinks `page_count`, so the file stays compact after deletes. Runs only in
    /// FULL mode, just before [`rebuild_ptrmap`](Self::rebuild_ptrmap) regenerates
    /// the pointer map and `largest_root_page` from the relocated structure.
    ///
    /// The relocation moves each trailing data page into the lowest free page,
    /// then fixes the single pointer that referenced it (a parent's child pointer,
    /// an overflow holder's link, or the previous overflow page's next-pointer),
    /// looked up from the freshly-derived `want` map. B-tree **root** pages are
    /// never relocated: their page number is cached in the executor's in-memory
    /// schema, which this layer must not invalidate. When the lowest movable page
    /// is a root (or no free slot remains, or a structural page blocks the way),
    /// the loop stops and leaves the remaining pages in place — still sound, just
    /// less compact. Freed pages that survive below the new size are rebuilt onto
    /// the freelist; everything above it is simply truncated away.
    ///
    /// Any error leaves the staged state untouched-enough that the caller can fall
    /// back to the in-place path; callers treat a relocation error as "skip
    /// truncation" rather than failing the commit.
    ///
    /// `limit` bounds how many pages are truncated off the end: `0` means
    /// unbounded (full compaction, the FULL-mode commit behavior), while a
    /// non-zero `limit` stops once that many trailing pages have been removed —
    /// the bound `PRAGMA incremental_vacuum(n)` relies on. The relocation,
    /// reference-fixing, freelist-rebuild and root/lock-byte safety rules are
    /// identical in both cases.
    fn autovacuum_truncate(&mut self, limit: u32) -> Result<()> {
        let usable = self.usable_size() as u32;
        let (want, _max_root) = self.compute_want()?;

        // The set of currently-free data pages (freelist trunk + leaves). We empty
        // the freelist and rebuild it at the end from whatever stays free below the
        // truncation point.
        let mut free: alloc::collections::BTreeSet<u32> = alloc::collections::BTreeSet::new();
        {
            let mut probe: BTreeMap<u32, PtrmapEntry> = BTreeMap::new();
            self.walk_freelist_ptrmap(&mut probe)?;
            for (pg, (kind, _)) in probe {
                if kind == PtrmapType::FreePage {
                    free.insert(pg);
                }
            }
        }
        if free.is_empty() {
            return Ok(()); // nothing to reclaim
        }
        // Detach the existing freelist; we rebuild it from `free` at the end.
        self.header.freelist_trunk = 0;
        self.header.freelist_count = 0;

        // Translation from a moved page's old number to its new (lower) number,
        // so a later fixup that names a since-relocated parent/holder/prev follows
        // it to where its bytes now live.
        let mut moved: BTreeMap<u32, u32> = BTreeMap::new();
        let resolve = |moved: &BTreeMap<u32, u32>, p: u32| -> u32 { *moved.get(&p).unwrap_or(&p) };

        let lock_page = (PENDING_BYTE / self.page_size as u64) as u32 + 1;
        let is_lock = |p: u32| lock_page > 1 && p == lock_page;

        let start = self.page_count;
        let mut last = self.page_count;
        while last > 1 {
            // Stop once `limit` trailing pages have been truncated (0 = unbounded).
            // Counting pages removed from the *end* of the file, not relocations,
            // matches `PRAGMA incremental_vacuum(n)`: each pass shrinks the file by
            // one page, whether that tail page was free/structural (dropped) or a
            // live page displaced by relocating it into a lower free slot.
            if limit != 0 && start - last >= limit {
                break;
            }
            // Trailing structural / free pages can be dropped without moving.
            if ptrmap::is_ptrmap_page(usable, last) {
                // A ptrmap page at the very end tracks only now-gone pages.
                self.overlay.remove(&last);
                last -= 1;
                continue;
            }
            if is_lock(last) {
                break; // never relocate across the lock-byte page
            }
            if free.remove(&last) {
                // A free page at the tail: just drop it.
                self.overlay.remove(&last);
                last -= 1;
                continue;
            }
            // `last` is a live data page. Find the lowest free slot below it that
            // is itself usable (not a ptrmap/lock page).
            let dest = free
                .iter()
                .copied()
                .find(|&f| f < last && !ptrmap::is_ptrmap_page(usable, f) && !is_lock(f));
            let Some(dest) = dest else {
                break; // no free slot below: cannot compact further
            };

            let (kind, parent) = match want.get(&last) {
                Some(&e) => e,
                // A live page with no ptrmap entry would be a bug; bail to the
                // safe in-place path rather than risk corruption.
                None => return Err(Error::Corrupt("relocate: page not in ptrmap".into())),
            };
            if kind == PtrmapType::RootPage {
                break; // do not relocate roots (in-memory schema caches them)
            }

            // Move the page contents into `dest`.
            let contents = self.read_page(last)?;
            self.overlay.insert(dest, contents);
            self.overlay.remove(&last);
            free.remove(&dest);
            moved.insert(last, dest);

            // Fix the one reference that pointed at `last`.
            self.fix_reference(kind, resolve(&moved, parent), last, dest, usable)?;

            last -= 1;
        }

        // Rebuild the freelist from the pages that remain free at or below the new
        // size; free pages above it were truncated away.
        let survivors: Vec<u32> = free.iter().copied().filter(|&pg| pg <= last).collect();
        for pg in survivors {
            self.free_page(pg)?;
        }

        // Apply the new page count and drop any overlay pages past the end.
        self.page_count = last;
        let stale: Vec<u32> = self.overlay.keys().copied().filter(|&p| p > last).collect();
        for p in stale {
            self.overlay.remove(&p);
        }
        Ok(())
    }

    /// Rewrite the single on-page pointer that referenced page `old` so it points
    /// at `new`, given the moved page's ptrmap `kind` and its (already
    /// move-resolved) `holder` page. Used by [`autovacuum_truncate`].
    fn fix_reference(
        &mut self,
        kind: PtrmapType,
        holder: u32,
        old: u32,
        new: u32,
        usable: u32,
    ) -> Result<()> {
        match kind {
            PtrmapType::Btree => {
                // `holder` is the parent b-tree page; find the child pointer == old.
                let mut bytes = self.read_page(holder)?;
                let bt = BtreePage::parse(Page::from_bytes(holder, bytes.clone()))?;
                let mut fixed = false;
                for i in 0..=bt.num_cells() {
                    if bt.child_pointer(i)? == old {
                        let off = bt.child_pointer_offset(i)?;
                        bytes[off..off + 4].copy_from_slice(&new.to_be_bytes());
                        fixed = true;
                        break;
                    }
                }
                if !fixed {
                    return Err(Error::Corrupt("relocate: child pointer not found".into()));
                }
                self.overlay.insert(holder, bytes);
            }
            PtrmapType::Overflow1 => {
                // `holder` is a b-tree page; one of its cells owns the chain whose
                // first page is `old`. Rewrite that cell's first-overflow pointer.
                let mut bytes = self.read_page(holder)?;
                let bt = BtreePage::parse(Page::from_bytes(holder, bytes.clone()))?;
                let mut fixed = false;
                for i in 0..bt.num_cells() {
                    if let Some(off) = bt.cell_overflow_offset(i, usable as usize)?
                        && u32::from_be_bytes([
                            bytes[off],
                            bytes[off + 1],
                            bytes[off + 2],
                            bytes[off + 3],
                        ]) == old
                    {
                        bytes[off..off + 4].copy_from_slice(&new.to_be_bytes());
                        fixed = true;
                        break;
                    }
                }
                if !fixed {
                    return Err(Error::Corrupt("relocate: overflow holder not found".into()));
                }
                self.overlay.insert(holder, bytes);
            }
            PtrmapType::Overflow2 => {
                // `holder` is the previous overflow page; its first 4 bytes are the
                // next-pointer that named `old`.
                let mut bytes = self.read_page(holder)?;
                if u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) != old {
                    return Err(Error::Corrupt("relocate: overflow link mismatch".into()));
                }
                bytes[0..4].copy_from_slice(&new.to_be_bytes());
                self.overlay.insert(holder, bytes);
            }
            PtrmapType::RootPage | PtrmapType::FreePage => {
                return Err(Error::Corrupt("relocate: unexpected page kind".into()));
            }
        }
        Ok(())
    }

    /// Whether page `n` currently exists (in the overlay, WAL, or on disk).
    fn has_page(&self, n: u32) -> bool {
        if self.overlay.contains_key(&n) {
            return true;
        }
        if let Some(w) = &self.wal
            && w.frames.contains_key(&n)
        {
            return true;
        }
        n >= 1 && n <= self.disk_pages
    }

    /// Read the `rootpage` of every object in `sqlite_schema` (the table b-tree
    /// rooted at page 1), returning `(rootpage, _)` pairs. Rows with a null/zero
    /// root (views, triggers) are skipped.
    fn schema_roots(&self) -> Result<Vec<u32>> {
        let enc = self.header.text_encoding;
        let mut out = Vec::new();
        let mut stack = alloc::vec![1u32];
        while let Some(pg) = stack.pop() {
            let bt = BtreePage::parse(Page::from_bytes(pg, self.read_page(pg)?))?;
            match bt.page_type() {
                PageType::LeafTable => {
                    let usable = self.usable_size();
                    for i in 0..bt.num_cells() {
                        let cell = bt.table_leaf_cell(i, usable)?;
                        let full =
                            crate::btree::cursor::read_payload(self, bt.data(), &cell.payload)?;
                        let cols = crate::format::record::decode_record(&full, enc)?;
                        // sqlite_schema column 3 is `rootpage`.
                        if let Some(crate::value::Value::Integer(r)) = cols.get(3)
                            && *r > 0
                        {
                            out.push(*r as u32);
                        }
                    }
                }
                PageType::InteriorTable => {
                    for i in 0..bt.num_cells() {
                        stack.push(bt.child_pointer(i)?);
                    }
                    stack.push(bt.right_pointer());
                }
                _ => return Err(Error::Corrupt("sqlite_schema is not a table b-tree".into())),
            }
        }
        Ok(out)
    }

    /// Walk the b-tree rooted at `root`, recording the ptrmap entry of every
    /// non-root page below it: child pages get `Btree(parent)`, and each cell's
    /// overflow chain gets `Overflow1(holding page)` then `Overflow2(prev)`.
    fn walk_btree_ptrmap(&self, root: u32, want: &mut BTreeMap<u32, PtrmapEntry>) -> Result<()> {
        let usable = self.usable_size();
        // Iterative DFS over (page, is_root) so we don't record the root as Btree.
        let mut stack = alloc::vec![root];
        while let Some(pg) = stack.pop() {
            let bt = BtreePage::parse(Page::from_bytes(pg, self.read_page(pg)?))?;
            match bt.page_type() {
                PageType::LeafTable => {
                    for i in 0..bt.num_cells() {
                        let ov = bt.table_leaf_cell(i, usable)?.payload.overflow;
                        self.record_overflow_chain(ov, pg, want)?;
                    }
                }
                PageType::LeafIndex => {
                    for i in 0..bt.num_cells() {
                        let ov = bt.index_cell(i, usable)?.payload.overflow;
                        self.record_overflow_chain(ov, pg, want)?;
                    }
                }
                PageType::InteriorTable => {
                    for i in 0..bt.num_cells() {
                        let child = bt.child_pointer(i)?;
                        want.insert(child, (PtrmapType::Btree, pg));
                        stack.push(child);
                    }
                    let r = bt.right_pointer();
                    want.insert(r, (PtrmapType::Btree, pg));
                    stack.push(r);
                }
                PageType::InteriorIndex => {
                    for i in 0..bt.num_cells() {
                        let ov = bt.index_cell(i, usable)?.payload.overflow;
                        self.record_overflow_chain(ov, pg, want)?;
                        let child = bt.child_pointer(i)?;
                        want.insert(child, (PtrmapType::Btree, pg));
                        stack.push(child);
                    }
                    let r = bt.right_pointer();
                    want.insert(r, (PtrmapType::Btree, pg));
                    stack.push(r);
                }
            }
        }
        Ok(())
    }

    /// Record the ptrmap entries for an overflow chain whose owning cell lives on
    /// `holder`: first page `Overflow1(holder)`, the rest `Overflow2(prev)`.
    fn record_overflow_chain(
        &self,
        first: u32,
        holder: u32,
        want: &mut BTreeMap<u32, PtrmapEntry>,
    ) -> Result<()> {
        if first == 0 {
            return Ok(());
        }
        want.insert(first, (PtrmapType::Overflow1, holder));
        let mut prev = first;
        let mut cur = {
            let pg = self.read_page(first)?;
            u32::from_be_bytes([pg[0], pg[1], pg[2], pg[3]])
        };
        while cur != 0 {
            want.insert(cur, (PtrmapType::Overflow2, prev));
            let pg = self.read_page(cur)?;
            prev = cur;
            cur = u32::from_be_bytes([pg[0], pg[1], pg[2], pg[3]]);
        }
        Ok(())
    }

    /// Record `FreePage(0)` for every page on the freelist (trunk pages and the
    /// leaf pages they list).
    fn walk_freelist_ptrmap(&self, want: &mut BTreeMap<u32, PtrmapEntry>) -> Result<()> {
        let mut trunk = self.header.freelist_trunk;
        let mut guard = 0u32;
        let cap = self.header.freelist_count + 8;
        while trunk != 0 {
            guard += 1;
            if guard > cap {
                return Err(Error::Corrupt("freelist trunk cycle".into()));
            }
            want.insert(trunk, (PtrmapType::FreePage, 0));
            let tb = self.read_page(trunk)?;
            let next = u32::from_be_bytes([tb[0], tb[1], tb[2], tb[3]]);
            let leaf_count = u32::from_be_bytes([tb[4], tb[5], tb[6], tb[7]]);
            for i in 0..leaf_count as usize {
                let idx = 8 + 4 * i;
                let leaf = u32::from_be_bytes([tb[idx], tb[idx + 1], tb[idx + 2], tb[idx + 3]]);
                want.insert(leaf, (PtrmapType::FreePage, 0));
            }
            trunk = next;
        }
        Ok(())
    }

    /// Discard all staged changes (ROLLBACK).
    pub fn rollback(&mut self) {
        self.overlay.clear();
        // The durable page count is the last WAL commit's in WAL mode, else the
        // main file's.
        self.page_count = match &self.wal {
            Some(w) => w.db_size,
            None => self.disk_pages,
        };
        self.savepoints.clear();
        self.release_locks();
    }

    /// Open a savepoint, snapshotting the current staged state.
    pub fn savepoint(&mut self, name: &str) {
        self.savepoints.push(Savepoint {
            name: String::from(name),
            overlay: self.overlay.clone(),
            header: self.header.clone(),
            page_count: self.page_count,
        });
    }

    /// Number of open savepoints.
    pub fn savepoint_depth(&self) -> usize {
        self.savepoints.len()
    }

    /// `RELEASE name`: drop the named savepoint and any nested inside it, keeping
    /// the staged changes. Errors if there is no such savepoint.
    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        match self
            .savepoints
            .iter()
            .rposition(|s| s.name.eq_ignore_ascii_case(name))
        {
            Some(idx) => {
                self.savepoints.truncate(idx);
                Ok(())
            }
            None => Err(Error::Error(format!("no such savepoint: {name}"))),
        }
    }

    /// `ROLLBACK TO name`: restore the staged state to the named savepoint and
    /// discard any nested inside it, but keep the savepoint open.
    pub fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        match self
            .savepoints
            .iter()
            .rposition(|s| s.name.eq_ignore_ascii_case(name))
        {
            Some(idx) => {
                let snap = &self.savepoints[idx];
                self.overlay = snap.overlay.clone();
                self.header = snap.header.clone();
                self.page_count = snap.page_count;
                self.savepoints.truncate(idx + 1);
                Ok(())
            }
            None => Err(Error::Error(format!("no such savepoint: {name}"))),
        }
    }

    /// Atomically flush all staged changes to the database file.
    pub fn commit(&mut self) -> Result<()> {
        if self.overlay.is_empty() {
            self.release_locks();
            return Ok(());
        }
        // In WAL mode, append the dirty pages as WAL frames instead.
        if self.wal.is_some() {
            return self.commit_wal();
        }
        // Take the EXCLUSIVE lock for the flush (no other writer or reader).
        self.acquire_exclusive()?;
        // In auto-vacuum mode, first relocate-and-truncate (FULL only), then bring
        // the pointer-map pages up to date before flushing.
        if self.auto_vacuum_on() {
            self.maybe_autovacuum_truncate();
            self.rebuild_ptrmap()?;
        }
        // Refresh header bookkeeping and re-stamp page 1.
        self.header.change_counter = self.header.change_counter.wrapping_add(1);
        self.header.size_in_pages = self.page_count;
        self.header.version_valid_for = self.header.change_counter;
        let mut page1 = self.overlay.get(&1).cloned().unwrap_or(self.read_page(1)?);
        self.header.write_to(&mut page1)?;
        self.overlay.insert(1, page1);

        // 1. Journal the originals of pages that already exist on disk.
        if self.journal.is_some() {
            self.write_journal()?;
        }

        // 2. Write all dirty pages, then make sure the file is exactly sized.
        let page_size = self.page_size as u64;
        let pages: Vec<(u32, Vec<u8>)> =
            self.overlay.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (n, bytes) in &pages {
            self.file.write_all_at(bytes, (*n as u64 - 1) * page_size)?;
        }
        self.file.truncate(self.page_count as u64 * page_size)?;
        self.file.sync()?;

        // 3. Clear the journal — the commit is now durable.
        if let Some(j) = self.journal.as_mut() {
            j.truncate(0)?;
            j.sync()?;
        }

        self.disk_pages = self.page_count;
        self.overlay.clear();
        // The main file's contents just changed under the clean read cache; drop
        // it so subsequent reads re-fetch the committed bytes.
        self.read_cache.borrow_mut().clear();
        self.savepoints.clear();
        self.release_locks();
        Ok(())
    }

    /// Whether the database is in WAL mode.
    pub fn wal_mode(&self) -> bool {
        self.wal.is_some()
    }

    /// The auto-vacuum mode recorded in the file header.
    ///
    /// Derived from `largest_root_page` (non-zero ⇒ auto-vacuum on) and
    /// `incremental_vacuum` (non-zero ⇒ INCREMENTAL), matching how SQLite
    /// reports `PRAGMA auto_vacuum`.
    pub fn auto_vacuum(&self) -> AutoVacuum {
        if self.header.largest_root_page == 0 {
            AutoVacuum::None
        } else if self.header.incremental_vacuum != 0 {
            AutoVacuum::Incremental
        } else {
            AutoVacuum::Full
        }
    }

    /// Switch an *empty* database (page 1 only, no user tables) into the given
    /// auto-vacuum `mode` by stamping the header fields, mirroring how SQLite
    /// honours `PRAGMA auto_vacuum=FULL|INCREMENTAL` only before any table is
    /// created. Returns `Ok(true)` if the mode was applied, `Ok(false)` if the
    /// database already contains data (in which case SQLite makes the pragma a
    /// no-op until the next `VACUUM`, which we mirror).
    pub fn set_auto_vacuum_if_empty(&mut self, mode: AutoVacuum) -> Result<bool> {
        // "Empty" means a single page whose schema b-tree has no rows.
        if self.page_count != 1 {
            return Ok(false);
        }
        let p1 = self.read_page(1)?;
        let bt = BtreePage::parse(Page::from_bytes(1, p1))?;
        let non_empty = match bt.page_type() {
            PageType::LeafTable => bt.num_cells() != 0,
            _ => true,
        };
        if non_empty {
            return Ok(false);
        }
        let (largest_root_page, incremental_vacuum) = match mode {
            AutoVacuum::None => (0, 0),
            AutoVacuum::Full => (1, 0),
            AutoVacuum::Incremental => (1, 1),
        };
        let h = self.header_mut();
        h.largest_root_page = largest_root_page;
        h.incremental_vacuum = incremental_vacuum;
        Ok(true)
    }

    /// Switch the database into WAL mode (`PRAGMA journal_mode = WAL`). Stamps the
    /// file header's read/write version = 2 via a normal journaled commit, then
    /// initializes empty WAL state. A no-op if already in WAL mode or if no
    /// `-wal` file was supplied (e.g. a bare in-memory database).
    pub fn set_wal_mode(&mut self) -> Result<bool> {
        if self.wal.is_some() {
            return Ok(true);
        }
        if self.wal_file.is_none() {
            return Ok(false);
        }
        // Persist the WAL version bytes in the main header first.
        if self.header.read_version != 2 || self.header.write_version != 2 {
            self.header.read_version = 2;
            self.header.write_version = 2;
            let mut page1 = self.overlay.get(&1).cloned().unwrap_or(self.read_page(1)?);
            self.header.write_to(&mut page1)?;
            self.overlay.insert(1, page1);
            self.commit()?; // journaled commit of the header change
        }
        // Fresh WAL: truncate any stale -wal and start with new salts.
        if let Some(w) = self.wal_file.as_mut() {
            w.truncate(0)?;
            w.sync()?;
        }
        let salt = (self.header.change_counter as u64)
            .wrapping_mul(0x9E37_79B9)
            .to_be_bytes();
        self.wal = Some(WalRuntime {
            frames: BTreeMap::new(),
            offset: 0,
            cksum: (0, 0),
            salt,
            db_size: self.page_count,
        });
        Ok(true)
    }

    /// Commit the overlay as WAL frames appended to the `-wal` file.
    fn commit_wal(&mut self) -> Result<()> {
        if self.auto_vacuum_on() {
            self.maybe_autovacuum_truncate();
            self.rebuild_ptrmap()?;
        }
        self.header.change_counter = self.header.change_counter.wrapping_add(1);
        self.header.size_in_pages = self.page_count;
        self.header.version_valid_for = self.header.change_counter;
        let mut page1 = self.overlay.get(&1).cloned().unwrap_or(self.read_page(1)?);
        self.header.write_to(&mut page1)?;
        self.overlay.insert(1, page1);

        let page_size = self.page_size;
        let pages: Vec<(u32, Vec<u8>)> =
            self.overlay.iter().map(|(k, v)| (*k, v.clone())).collect();
        let wal = self.wal.as_mut().expect("wal mode");
        let salt = wal.salt;
        let file = self.wal_file.as_mut().expect("wal file");

        // Write the 32-byte WAL header if this is the first frame after a reset.
        if wal.offset == 0 {
            let mut hdr = [0u8; WAL_HDR_LEN];
            hdr[0..4].copy_from_slice(&WAL_MAGIC_LE.to_be_bytes());
            hdr[4..8].copy_from_slice(&3_007_000u32.to_be_bytes()); // format version
            hdr[8..12].copy_from_slice(&(page_size as u32).to_be_bytes());
            hdr[12..16].copy_from_slice(&0u32.to_be_bytes()); // checkpoint sequence
            hdr[16..24].copy_from_slice(&salt);
            let (h0, h1) = super::wal::checksum(false, 0, 0, &hdr[0..24]);
            hdr[24..28].copy_from_slice(&h0.to_be_bytes());
            hdr[28..32].copy_from_slice(&h1.to_be_bytes());
            file.write_all_at(&hdr, 0)?;
            wal.offset = WAL_HDR_LEN as u64;
            wal.cksum = (h0, h1);
        }

        let (mut s0, mut s1) = wal.cksum;
        let n = pages.len();
        let frame_len = WAL_FRAME_HDR_LEN + page_size;
        for (i, (page_no, data)) in pages.iter().enumerate() {
            // The last frame of the commit carries the post-commit db size.
            let db_size = if i + 1 == n { self.page_count } else { 0 };
            let mut fhdr = [0u8; WAL_FRAME_HDR_LEN];
            fhdr[0..4].copy_from_slice(&page_no.to_be_bytes());
            fhdr[4..8].copy_from_slice(&db_size.to_be_bytes());
            fhdr[8..16].copy_from_slice(&salt);
            let (c0, c1) = super::wal::checksum(false, s0, s1, &fhdr[0..8]);
            let (c0, c1) = super::wal::checksum(false, c0, c1, data);
            fhdr[16..20].copy_from_slice(&c0.to_be_bytes());
            fhdr[20..24].copy_from_slice(&c1.to_be_bytes());
            let mut frame = Vec::with_capacity(frame_len);
            frame.extend_from_slice(&fhdr);
            frame.extend_from_slice(data);
            file.write_all_at(&frame, wal.offset)?;
            wal.offset += frame_len as u64;
            s0 = c0;
            s1 = c1;
        }
        file.sync()?;
        wal.cksum = (s0, s1);
        wal.db_size = self.page_count;
        for (page_no, data) in pages {
            wal.frames.insert(page_no, data);
        }
        self.overlay.clear();
        self.savepoints.clear();
        // WAL writers serialize via the write-intent lock taken in write_page;
        // readers stay concurrent. Release it now that the frames are durable.
        self.release_locks();
        Ok(())
    }

    /// Checkpoint: write all WAL frames back into the main database file, reset
    /// the `-wal` file, and keep WAL mode active with fresh salts.
    pub fn checkpoint(&mut self) -> Result<()> {
        let Some(wal) = self.wal.as_mut() else {
            return Ok(());
        };
        let page_size = self.page_size as u64;
        let frames: Vec<(u32, Vec<u8>)> = wal.frames.iter().map(|(k, v)| (*k, v.clone())).collect();
        let db_size = wal.db_size;
        for (page_no, data) in &frames {
            self.file
                .write_all_at(data, (*page_no as u64 - 1) * page_size)?;
        }
        self.file.truncate(db_size as u64 * page_size)?;
        self.file.sync()?;
        self.disk_pages = db_size;
        // The checkpoint rewrote the main file; drop the clean read cache.
        self.read_cache.borrow_mut().clear();
        // Reset the WAL: empty it and restart with a new salt.
        if let Some(w) = self.wal_file.as_mut() {
            w.truncate(0)?;
            w.sync()?;
        }
        let new_salt = {
            let mut s = wal.salt;
            let v = u32::from_be_bytes([s[0], s[1], s[2], s[3]]).wrapping_add(1);
            s[0..4].copy_from_slice(&v.to_be_bytes());
            s
        };
        self.wal = Some(WalRuntime {
            frames: BTreeMap::new(),
            offset: 0,
            cksum: (0, 0),
            salt: new_salt,
            db_size,
        });
        Ok(())
    }

    /// Replace the entire database file with a freshly-built, compact `image`
    /// (page 1 first, carrying the new header). Used by `VACUUM`. Bypasses the
    /// rollback journal — the rebuild is all-or-nothing on success. In WAL mode
    /// the image is written to the main file and the WAL is reset empty.
    pub fn replace_image(&mut self, image: Vec<Vec<u8>>) -> Result<()> {
        if image.is_empty() || image[0].len() != self.page_size {
            return Err(Error::Error("invalid VACUUM image".into()));
        }
        let ps = self.page_size as u64;
        for (i, bytes) in image.iter().enumerate() {
            self.file.write_all_at(bytes, i as u64 * ps)?;
        }
        self.file.truncate(image.len() as u64 * ps)?;
        self.file.sync()?;
        let count = image.len() as u32;
        self.header = DatabaseHeader::parse(&image[0])?;
        self.disk_pages = count;
        self.page_count = count;
        self.overlay.clear();
        // The whole file was rewritten; drop the clean read cache.
        self.read_cache.borrow_mut().clear();
        // Reset any WAL: its frames now refer to the pre-VACUUM image.
        if self.wal.is_some() {
            if let Some(w) = self.wal_file.as_mut() {
                w.truncate(0)?;
                w.sync()?;
            }
            self.header.read_version = 2;
            self.header.write_version = 2;
            self.wal = Some(WalRuntime {
                frames: BTreeMap::new(),
                offset: 0,
                cksum: (0, 0),
                salt: (count as u64).wrapping_mul(0x9E37_79B9).to_be_bytes(),
                db_size: count,
            });
        }
        Ok(())
    }

    /// Load committed frames from an existing `-wal` file (used on open). Returns
    /// the runtime state positioned to append after the last valid frame, or
    /// `None` if the WAL is empty/invalid.
    fn load_wal(wal: &mut dyn File, page_size: usize) -> Result<Option<WalRuntime>> {
        let size = wal.size()?;
        if size < WAL_HDR_LEN as u64 {
            return Ok(None);
        }
        let mut hdr = [0u8; WAL_HDR_LEN];
        wal.read_exact_at(&mut hdr, 0)?;
        let magic = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        if magic & 0xFFFF_FFFE != WAL_MAGIC_LE {
            return Ok(None);
        }
        let big_endian = (magic & 1) == 1;
        let wal_ps = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        if wal_ps != page_size {
            return Ok(None);
        }
        let mut salt = [0u8; 8];
        salt.copy_from_slice(&hdr[16..24]);
        let (h0, h1) = super::wal::checksum(big_endian, 0, 0, &hdr[0..24]);
        if h0 != u32::from_be_bytes([hdr[24], hdr[25], hdr[26], hdr[27]])
            || h1 != u32::from_be_bytes([hdr[28], hdr[29], hdr[30], hdr[31]])
        {
            return Ok(None);
        }
        let frame_len = WAL_FRAME_HDR_LEN + page_size;
        let (mut s0, mut s1) = (h0, h1);
        let mut off = WAL_HDR_LEN as u64;
        let mut frames: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut pending: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut db_size = 0u32;
        let mut committed_off = WAL_HDR_LEN as u64;
        let mut committed_cksum = (h0, h1);
        while off + frame_len as u64 <= size {
            let mut fhdr = [0u8; WAL_FRAME_HDR_LEN];
            wal.read_exact_at(&mut fhdr, off)?;
            let mut page = vec![0u8; page_size];
            wal.read_exact_at(&mut page, off + WAL_FRAME_HDR_LEN as u64)?;
            if fhdr[8..16] != salt {
                break;
            }
            let (c0, c1) = super::wal::checksum(big_endian, s0, s1, &fhdr[0..8]);
            let (c0, c1) = super::wal::checksum(big_endian, c0, c1, &page);
            if c0 != u32::from_be_bytes([fhdr[16], fhdr[17], fhdr[18], fhdr[19]])
                || c1 != u32::from_be_bytes([fhdr[20], fhdr[21], fhdr[22], fhdr[23]])
            {
                break;
            }
            s0 = c0;
            s1 = c1;
            let page_no = u32::from_be_bytes([fhdr[0], fhdr[1], fhdr[2], fhdr[3]]);
            let commit = u32::from_be_bytes([fhdr[4], fhdr[5], fhdr[6], fhdr[7]]);
            pending.push((page_no, page));
            off += frame_len as u64;
            if commit != 0 {
                for (p, d) in pending.drain(..) {
                    frames.insert(p, d);
                }
                db_size = commit;
                committed_off = off;
                committed_cksum = (s0, s1);
            }
        }
        if frames.is_empty() {
            return Ok(None);
        }
        Ok(Some(WalRuntime {
            frames,
            offset: committed_off,
            cksum: committed_cksum,
            salt,
            db_size,
        }))
    }

    /// Write the SQLite-format rollback journal: the originals of every page
    /// this commit is about to overwrite, in the on-disk byte layout that the
    /// real `sqlite3` (and our own [`recover`](Self::recover)) plays back to undo
    /// an interrupted transaction.
    ///
    /// Layout (all integers big-endian), matching the file-format spec:
    /// * **Header** (zero-padded out to one [`JOURNAL_SECTOR`]): the 8-byte
    ///   [`JOURNAL_MAGIC`]; record count (offset 8); a per-transaction checksum
    ///   nonce (offset 12); the initial database size in pages (offset 16); the
    ///   sector size (offset 20); and the page size (offset 24).
    /// * **Page records** (packed contiguously after the header sector): for each
    ///   saved page, its 4-byte page number, its full original content, then the
    ///   4-byte [`journal_page_checksum`].
    ///
    /// The record count at offset 8 is written *after* the page records, mirroring
    /// SQLite's two-flush commit: the page bodies are durable before the header
    /// advertises how many records to trust, so a crash before the final sync
    /// leaves a count of 0 (an empty, ignorable journal) rather than a header that
    /// promises records that were never written.
    fn write_journal(&mut self) -> Result<()> {
        // Collect originals of pages being overwritten (those already on disk).
        let mut originals: Vec<(u32, Vec<u8>)> = Vec::new();
        for &n in self.overlay.keys() {
            if n <= self.disk_pages {
                let mut buf = vec![0u8; self.page_size];
                self.file
                    .read_exact_at(&mut buf, (n as u64 - 1) * self.page_size as u64)?;
                originals.push((n, buf));
            }
        }
        // A per-transaction nonce: deterministic (no RNG dependency) but varying
        // commit-to-commit via the change counter, so a stale page left over from
        // a previous journal fails this transaction's checksum.
        let nonce = (self.header.change_counter)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add(self.disk_pages.wrapping_mul(2_654_435_761));
        let page_size = self.page_size as u64;
        let j = self.journal.as_mut().unwrap();
        j.truncate(0)?;

        // Header, padded with zeros out to a full sector.
        let mut hdr = vec![0u8; JOURNAL_SECTOR as usize];
        hdr[0..8].copy_from_slice(&JOURNAL_MAGIC);
        // Record count is filled in after the records are synced (see below).
        hdr[12..16].copy_from_slice(&nonce.to_be_bytes());
        hdr[16..20].copy_from_slice(&self.disk_pages.to_be_bytes());
        hdr[20..24].copy_from_slice(&(JOURNAL_SECTOR as u32).to_be_bytes());
        hdr[24..28].copy_from_slice(&(page_size as u32).to_be_bytes());
        debug_assert_eq!(JOURNAL_HDR_FIELDS, 28);
        j.write_all_at(&hdr, 0)?;

        // Page records begin on the sector boundary.
        let mut off = JOURNAL_SECTOR;
        for (n, bytes) in &originals {
            j.write_all_at(&n.to_be_bytes(), off)?;
            off += 4;
            j.write_all_at(bytes, off)?;
            off += page_size;
            let cksum = journal_page_checksum(nonce, bytes);
            j.write_all_at(&cksum.to_be_bytes(), off)?;
            off += 4;
        }
        // Sync the records, then publish the count and sync the header sector.
        j.sync()?;
        let nrec = originals.len() as u32;
        j.write_all_at(&nrec.to_be_bytes(), 8)?;
        j.sync()?;
        Ok(())
    }

    /// Replay a hot SQLite-format journal onto `file`, restoring the pre-commit
    /// state, then clear it.
    ///
    /// Mirrors SQLite's hot-journal recovery: validate the header magic, read the
    /// initial page count / sector size / page size, then play back each page
    /// record (page number, original content, checksum) into the database file.
    /// Records whose checksum does not match the header nonce are treated as the
    /// torn tail of an interrupted write and stop the replay — everything up to
    /// that point is still rolled back. Finally the database is truncated to its
    /// recorded initial size and the journal is truncated away.
    ///
    /// A journal whose record count is `0` (or whose magic is absent) is not hot
    /// and is ignored. When a segment's count is `-1`/`0xFFFFFFFF` (SQLite's
    /// cache-spill sentinel) or otherwise larger than the remaining file, recovery
    /// reads records until the next segment header or end of file.
    ///
    /// SQLite may append **multiple journal segments** (each a sector-aligned
    /// header followed by its records) when the page cache spills mid-transaction;
    /// every segment shares the same page size and sector size, but carries its
    /// own record count and checksum nonce. Recovery walks each segment in turn,
    /// playing every valid record back, so a large interrupted transaction is
    /// fully rolled back.
    fn recover(file: &mut dyn File, journal: &mut dyn File) -> Result<()> {
        let jsize = journal.size()?;
        if jsize < JOURNAL_SECTOR {
            return Ok(()); // too small to hold a header: nothing to do
        }
        // Read the first header to learn the geometry and the original db size.
        let mut hdr = [0u8; JOURNAL_HDR_FIELDS];
        journal.read_exact_at(&mut hdr, 0)?;
        if hdr[0..8] != JOURNAL_MAGIC {
            return Ok(()); // not a SQLite journal (or zeroed/persist-invalidated)
        }
        let orig_pages = be32(&hdr, 16);
        let sector = be32(&hdr, 20) as u64;
        let page_size = be32(&hdr, 24) as u64;
        if page_size < 512 || !page_size.is_power_of_two() || sector == 0 {
            return Ok(()); // bogus geometry: not a journal we can trust
        }
        // The first segment must carry at least one record to be hot.
        if be32(&hdr, 8) == 0 {
            return Ok(());
        }

        let rec_len = 4 + page_size + 4;
        let mut seg_off = 0u64;
        let mut played_any = false;
        // Walk each journal segment (header + records), sector-aligned.
        while seg_off + JOURNAL_HDR_FIELDS as u64 <= jsize {
            let mut sh = [0u8; JOURNAL_HDR_FIELDS];
            journal.read_exact_at(&mut sh, seg_off)?;
            if sh[0..8] != JOURNAL_MAGIC {
                break; // no further segment
            }
            let nrec = be32(&sh, 8);
            let nonce = be32(&sh, 12);
            // Records begin on the sector boundary after this segment's header.
            let recs_start = seg_off + sector;
            if recs_start > jsize {
                break;
            }
            let avail = (jsize - recs_start) / rec_len;
            // -1/over-large count ⇒ read all that fit before the next thing.
            let want = if nrec as u64 > avail {
                avail
            } else {
                nrec as u64
            };
            let mut off = recs_start;
            let mut played = 0u64;
            for _ in 0..want {
                let mut nb = [0u8; 4];
                journal.read_exact_at(&mut nb, off)?;
                let n = u32::from_be_bytes(nb);
                let mut buf = vec![0u8; page_size as usize];
                journal.read_exact_at(&mut buf, off + 4)?;
                let mut cb = [0u8; 4];
                journal.read_exact_at(&mut cb, off + 4 + page_size)?;
                // A mismatched checksum (or a zero page number) marks the torn
                // tail of an interrupted write: stop here, having rolled back the
                // valid prefix, exactly as SQLite does.
                if n == 0 || u32::from_be_bytes(cb) != journal_page_checksum(nonce, &buf) {
                    break;
                }
                file.write_all_at(&buf, (n as u64 - 1) * page_size)?;
                played_any = true;
                played += 1;
                off += rec_len;
            }
            // Advance to the next segment: past the records, padded up to the
            // next sector boundary. If this segment was truncated early (torn
            // tail) there can be no further valid segment.
            if played < want {
                break;
            }
            let consumed = recs_start + played * rec_len;
            let next = consumed.div_ceil(sector) * sector;
            if next <= seg_off {
                break; // no forward progress: avoid looping
            }
            seg_off = next;
        }

        if !played_any {
            return Ok(());
        }
        // Restore the original length, discarding pages appended by the aborted tx.
        file.truncate(orig_pages as u64 * page_size)?;
        file.sync()?;
        journal.truncate(0)?;
        journal.sync()?;
        Ok(())
    }
}

impl PageSource for WritePager {
    fn page(&self, number: u32) -> Result<Page> {
        Ok(Page::from_bytes(number, self.read_page(number)?))
    }
    fn header(&self) -> &DatabaseHeader {
        &self.header
    }
    fn usable_size(&self) -> usize {
        self.header.usable_size() as usize
    }
    fn page_count(&self) -> u32 {
        self.page_count
    }
}

#[inline]
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[inline]
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

/// Write an empty table-leaf b-tree header at `offset` within `page`.
fn write_empty_leaf_header(page: &mut [u8], offset: usize, page_size: u32) {
    page[offset] = 0x0d; // leaf table b-tree
    page[offset + 1] = 0; // first freeblock
    page[offset + 2] = 0;
    page[offset + 3] = 0; // num cells = 0
    page[offset + 4] = 0;
    // Cell content area start: top of page (page_size, or 0 if 65536).
    let ccs: u16 = if page_size == 65536 {
        0
    } else {
        page_size as u16
    };
    page[offset + 5] = (ccs >> 8) as u8;
    page[offset + 6] = ccs as u8;
    page[offset + 7] = 0; // fragmented free bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{OpenFlags, Vfs, memory::MemoryVfs};

    fn mem_wp() -> WritePager {
        let vfs = MemoryVfs::new();
        let file = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
        WritePager::create(file, None, 4096).unwrap()
    }

    #[test]
    fn create_yields_valid_empty_db() {
        let mut wp = mem_wp();
        wp.commit().unwrap();
        // Page 1 parses as a header with one page.
        let p1 = wp.read_page(1).unwrap();
        let h = DatabaseHeader::parse(&p1).unwrap();
        assert_eq!(h.page_size, 4096);
        assert_eq!(h.size_in_pages, 1);
        assert_eq!(p1[100], 0x0d); // empty leaf
    }

    #[test]
    fn auto_vacuum_header_bytes_match_sqlite() {
        // Empirically confirmed against sqlite3 3.50.4: an empty auto-vacuum db
        // sets largest_root_page (offset 52) = 1, with incremental_vacuum
        // (offset 64) = 0 for FULL and = 1 for INCREMENTAL. NONE leaves both 0.
        for (mode, lrp, inc, reported) in [
            (AutoVacuum::None, 0u32, 0u32, AutoVacuum::None),
            (AutoVacuum::Full, 1, 0, AutoVacuum::Full),
            (AutoVacuum::Incremental, 1, 1, AutoVacuum::Incremental),
        ] {
            let vfs = MemoryVfs::new();
            let file = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
            let mut wp = WritePager::create_auto_vacuum(file, None, None, 4096, mode).unwrap();
            wp.commit().unwrap();
            let p1 = wp.read_page(1).unwrap();
            assert_eq!(be32(&p1, 52), lrp, "largest_root_page for {mode:?}");
            assert_eq!(be32(&p1, 64), inc, "incremental_vacuum for {mode:?}");
            // The header round-trips and the mode is reported back.
            let h = DatabaseHeader::parse(&p1).unwrap();
            assert_eq!(h.largest_root_page, lrp);
            assert_eq!(h.incremental_vacuum, inc);
            assert_eq!(wp.auto_vacuum(), reported);
            // And a reopen still reports the mode.
            let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
            let wp2 = WritePager::open(file, None).unwrap();
            assert_eq!(wp2.auto_vacuum(), reported);
        }
    }

    #[test]
    fn allocate_and_readback() {
        let mut wp = mem_wp();
        let n = wp.allocate_page().unwrap();
        assert_eq!(n, 2);
        let mut img = vec![0u8; 4096];
        img[0] = 0x0d;
        wp.write_page(n, img).unwrap();
        wp.commit().unwrap();
        assert_eq!(wp.page_count(), 2);
        assert_eq!(wp.read_page(2).unwrap()[0], 0x0d);
    }

    #[test]
    fn rollback_discards_overlay() {
        let mut wp = mem_wp();
        wp.commit().unwrap(); // establish baseline (1 page)
        wp.allocate_page().unwrap();
        wp.rollback();
        assert_eq!(wp.page_count(), 1);
    }

    #[test]
    fn journal_recovery_restores_originals() {
        // Simulate a crash: write a journal, corrupt the file, then recover.
        let vfs = MemoryVfs::new();
        {
            let file = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
            let jf = vfs
                .open("db-journal", OpenFlags::READ_WRITE_CREATE)
                .unwrap();
            let mut wp = WritePager::create(file, Some(jf), 4096).unwrap();
            wp.commit().unwrap(); // 1-page db on disk, journal cleared
        }
        // Manually craft a SQLite-format journal as if a commit had begun: save
        // page 1's original content with a valid header + checksum.
        let orig_p1 = {
            let f = vfs.open("db", OpenFlags::READ_ONLY).unwrap();
            let mut b = vec![0u8; 4096];
            f.read_exact_at(&mut b, 0).unwrap();
            b
        };
        {
            let mut j = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
            let nonce = 0x1234_5678u32;
            let mut hdr = vec![0u8; JOURNAL_SECTOR as usize];
            hdr[0..8].copy_from_slice(&JOURNAL_MAGIC);
            hdr[8..12].copy_from_slice(&1u32.to_be_bytes()); // nrec
            hdr[12..16].copy_from_slice(&nonce.to_be_bytes());
            hdr[16..20].copy_from_slice(&1u32.to_be_bytes()); // orig pages
            hdr[20..24].copy_from_slice(&(JOURNAL_SECTOR as u32).to_be_bytes());
            hdr[24..28].copy_from_slice(&4096u32.to_be_bytes());
            j.write_all_at(&hdr, 0).unwrap();
            let mut off = JOURNAL_SECTOR;
            j.write_all_at(&1u32.to_be_bytes(), off).unwrap();
            off += 4;
            j.write_all_at(&orig_p1, off).unwrap();
            off += 4096;
            let cksum = journal_page_checksum(nonce, &orig_p1);
            j.write_all_at(&cksum.to_be_bytes(), off).unwrap();
            j.sync().unwrap();
        }
        // Corrupt the live file.
        {
            let mut f = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
            f.write_all_at(&[0xFFu8; 16], 0).unwrap();
        }
        // Reopen: recovery should restore page 1 from the journal.
        let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
        let jf = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
        let wp = WritePager::open(file, Some(jf)).unwrap();
        assert_eq!(wp.read_page(1).unwrap(), orig_p1);
    }

    /// A multi-segment journal (two sector-aligned headers, each with its own
    /// records and nonce) is fully played back — covering SQLite's cache-spill
    /// layout.
    #[test]
    fn multi_segment_journal_recovery() {
        let vfs = MemoryVfs::new();
        // Baseline 3-page db.
        {
            let file = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
            let jf = vfs
                .open("db-journal", OpenFlags::READ_WRITE_CREATE)
                .unwrap();
            let mut wp = WritePager::create(file, Some(jf), 4096).unwrap();
            for _ in 0..2 {
                let p = wp.allocate_page().unwrap();
                let mut b = vec![0u8; 4096];
                b[0] = 0x0d;
                wp.write_page(p, b).unwrap();
            }
            wp.commit().unwrap();
        }
        // Originals to restore.
        let originals: Vec<(u32, Vec<u8>)> = (1u32..=3)
            .map(|n| {
                let f = vfs.open("db", OpenFlags::READ_ONLY).unwrap();
                let mut b = vec![0u8; 4096];
                f.read_exact_at(&mut b, (n as u64 - 1) * 4096).unwrap();
                (n, b)
            })
            .collect();
        // Mutate the live file so recovery has something to undo.
        {
            let mut f = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
            for n in 1u32..=3 {
                f.write_all_at(&[0xAA; 64], (n as u64 - 1) * 4096).unwrap();
            }
        }
        // Build a two-segment journal: segment A saves pages 1,2; segment B
        // (its own header on the next sector boundary) saves page 3.
        let write_seg =
            |buf: &mut Vec<u8>, base: u64, nonce: u32, recs: &[(u32, &Vec<u8>)], orig: u32| {
                let mut hdr = vec![0u8; JOURNAL_SECTOR as usize];
                hdr[0..8].copy_from_slice(&JOURNAL_MAGIC);
                hdr[8..12].copy_from_slice(&(recs.len() as u32).to_be_bytes());
                hdr[12..16].copy_from_slice(&nonce.to_be_bytes());
                hdr[16..20].copy_from_slice(&orig.to_be_bytes());
                hdr[20..24].copy_from_slice(&(JOURNAL_SECTOR as u32).to_be_bytes());
                hdr[24..28].copy_from_slice(&4096u32.to_be_bytes());
                let need = (base + JOURNAL_SECTOR) as usize;
                if buf.len() < need {
                    buf.resize(need, 0);
                }
                buf[base as usize..need].copy_from_slice(&hdr);
                for (n, data) in recs {
                    buf.extend_from_slice(&n.to_be_bytes());
                    buf.extend_from_slice(data);
                    buf.extend_from_slice(&journal_page_checksum(nonce, data).to_be_bytes());
                }
                // Pad up to the next sector boundary so the next header is aligned.
                while !(buf.len() as u64).is_multiple_of(JOURNAL_SECTOR) {
                    buf.push(0);
                }
            };
        let mut jbytes = Vec::new();
        write_seg(
            &mut jbytes,
            0,
            0x1111_1111,
            &[(1, &originals[0].1), (2, &originals[1].1)],
            3,
        );
        let seg_b_base = jbytes.len() as u64;
        write_seg(
            &mut jbytes,
            seg_b_base,
            0x2222_2222,
            &[(3, &originals[2].1)],
            3,
        );
        {
            let mut j = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
            j.truncate(0).unwrap();
            j.write_all_at(&jbytes, 0).unwrap();
            j.sync().unwrap();
        }
        // Recover and assert all three pages were restored.
        let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
        let jf = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
        let wp = WritePager::open(file, Some(jf)).unwrap();
        for (n, data) in &originals {
            assert_eq!(&wp.read_page(*n).unwrap(), data, "page {n} restored");
        }
    }
}
