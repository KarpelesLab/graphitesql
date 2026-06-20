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
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const JOURNAL_MAGIC: &[u8; 8] = b"GSQLJRN1";

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
    /// Open savepoints (innermost last); each snapshots the staged state so
    /// `ROLLBACK TO` can restore it.
    savepoints: Vec<Savepoint>,
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
            savepoints: Vec::new(),
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
            savepoints: Vec::new(),
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
        if !self.overlay.contains_key(&1) {
            if let Ok(p) = self.read_page(1) {
                self.overlay.insert(1, p);
            }
        }
        &mut self.header
    }

    /// Read the full bytes of page `number` (overlay first, then disk).
    pub fn read_page(&self, number: u32) -> Result<Vec<u8>> {
        if let Some(bytes) = self.overlay.get(&number) {
            return Ok(bytes.clone());
        }
        // In WAL mode, the newest version of a page may live in the WAL.
        if let Some(w) = &self.wal {
            if let Some(bytes) = w.frames.get(&number) {
                return Ok(bytes.clone());
            }
        }
        if number == 0 || number > self.disk_pages {
            return Err(Error::Corrupt(format!("page {number} out of range")));
        }
        let mut buf = vec![0u8; self.page_size];
        self.file
            .read_exact_at(&mut buf, (number as u64 - 1) * self.page_size as u64)?;
        Ok(buf)
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

    /// Drop all write locks at the end of a transaction.
    fn release_locks(&mut self) {
        use crate::vfs::LockLevel;
        if self.held != LockLevel::Unlocked {
            let _ = self.file.unlock(LockLevel::Unlocked);
            self.held = LockLevel::Unlocked;
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
    fn rebuild_ptrmap(&mut self) -> Result<()> {
        let usable = self.usable_size() as u32;
        // Desired (type, parent) for every tracked page.
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

        // 3. Keep the header's largest_root_page in sync.
        self.header.largest_root_page = max_root;

        // 4. Materialize the ptrmap pages: gather the entries that fall on each,
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

    /// Whether page `n` currently exists (in the overlay, WAL, or on disk).
    fn has_page(&self, n: u32) -> bool {
        if self.overlay.contains_key(&n) {
            return true;
        }
        if let Some(w) = &self.wal {
            if w.frames.contains_key(&n) {
                return true;
            }
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
                        if let Some(crate::value::Value::Integer(r)) = cols.get(3) {
                            if *r > 0 {
                                out.push(*r as u32);
                            }
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
        // In auto-vacuum mode, bring the pointer-map pages up to date before
        // flushing. (We keep any freed pages in place with a correct FREEPAGE
        // ptrmap entry, which `integrity_check` accepts; FULL-mode commit-time
        // truncation/relocation is a separate later piece, C6b-3.)
        if self.auto_vacuum_on() {
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
        let j = self.journal.as_mut().unwrap();
        j.truncate(0)?;
        // Header: magic, original page count, page size.
        let mut hdr = Vec::with_capacity(16);
        hdr.extend_from_slice(JOURNAL_MAGIC);
        hdr.extend_from_slice(&self.disk_pages.to_be_bytes());
        hdr.extend_from_slice(&(self.page_size as u32).to_be_bytes());
        j.write_all_at(&hdr, 0)?;
        let mut off = hdr.len() as u64;
        for (n, bytes) in &originals {
            j.write_all_at(&n.to_be_bytes(), off)?;
            off += 4;
            j.write_all_at(bytes, off)?;
            off += self.page_size as u64;
        }
        j.sync()?;
        Ok(())
    }

    /// Replay a non-empty journal onto `file`, restoring the pre-commit state.
    fn recover(file: &mut dyn File, journal: &mut dyn File) -> Result<()> {
        let jsize = journal.size()?;
        if jsize < 16 {
            return Ok(()); // empty or absent journal: nothing to do
        }
        let mut hdr = [0u8; 16];
        journal.read_exact_at(&mut hdr, 0)?;
        if &hdr[0..8] != JOURNAL_MAGIC {
            return Ok(()); // not our journal; ignore
        }
        let orig_pages = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        let page_size = u32::from_be_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]) as usize;
        let mut off = 16u64;
        while off + 4 + page_size as u64 <= jsize {
            let mut nb = [0u8; 4];
            journal.read_exact_at(&mut nb, off)?;
            off += 4;
            let n = u32::from_be_bytes(nb);
            let mut buf = vec![0u8; page_size];
            journal.read_exact_at(&mut buf, off)?;
            off += page_size as u64;
            file.write_all_at(&buf, (n as u64 - 1) * page_size as u64)?;
        }
        // Restore the original length, discarding pages appended by the aborted tx.
        file.truncate(orig_pages as u64 * page_size as u64)?;
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
    use crate::vfs::{memory::MemoryVfs, OpenFlags, Vfs};

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
        // Manually craft a journal as if a commit had begun: save page 1 original.
        let orig_p1 = {
            let f = vfs.open("db", OpenFlags::READ_ONLY).unwrap();
            let mut b = vec![0u8; 4096];
            f.read_exact_at(&mut b, 0).unwrap();
            b
        };
        {
            let mut j = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
            let mut hdr = Vec::new();
            hdr.extend_from_slice(JOURNAL_MAGIC);
            hdr.extend_from_slice(&1u32.to_be_bytes()); // orig pages
            hdr.extend_from_slice(&4096u32.to_be_bytes());
            j.write_all_at(&hdr, 0).unwrap();
            j.write_all_at(&1u32.to_be_bytes(), 16).unwrap();
            j.write_all_at(&orig_p1, 20).unwrap();
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
}
