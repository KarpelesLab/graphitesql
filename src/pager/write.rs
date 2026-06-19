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
use crate::error::{Error, Result};
use crate::format::header::HEADER_LEN;
use crate::format::{DatabaseHeader, TextEncoding};
use crate::vfs::File;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

const JOURNAL_MAGIC: &[u8; 8] = b"GSQLJRN1";

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
}

impl WritePager {
    /// Open an existing database file for writing. Replays the journal first if a
    /// previous commit was interrupted.
    pub fn open(mut file: Box<dyn File>, mut journal: Option<Box<dyn File>>) -> Result<WritePager> {
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
        Ok(WritePager {
            file,
            journal,
            header,
            page_size,
            disk_pages: pages,
            page_count: pages,
            overlay: BTreeMap::new(),
        })
    }

    /// Create a brand-new, empty database (a single `sqlite_schema` leaf page).
    pub fn create(
        file: Box<dyn File>,
        journal: Option<Box<dyn File>>,
        page_size: u32,
    ) -> Result<WritePager> {
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(Error::Error(format!("invalid page size {page_size}")));
        }
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
            largest_root_page: 0,
            text_encoding: TextEncoding::Utf8,
            user_version: 0,
            incremental_vacuum: 0,
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
        };
        // Page 1: db header (0..100) + an empty table-leaf b-tree at offset 100.
        let mut page1 = vec![0u8; page_size as usize];
        wp.header.write_to(&mut page1)?;
        write_empty_leaf_header(&mut page1, HEADER_LEN, page_size);
        wp.overlay.insert(1, page1);
        Ok(wp)
    }

    /// The database header (reflects in-transaction changes once committed).
    pub fn header_mut(&mut self) -> &mut DatabaseHeader {
        &mut self.header
    }

    /// Read the full bytes of page `number` (overlay first, then disk).
    pub fn read_page(&self, number: u32) -> Result<Vec<u8>> {
        if let Some(bytes) = self.overlay.get(&number) {
            return Ok(bytes.clone());
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
        self.overlay.insert(number, bytes);
        Ok(())
    }

    /// Allocate a page, reusing one from the freelist if available, otherwise
    /// extending the file. The returned page is staged zeroed in the overlay.
    pub fn allocate_page(&mut self) -> Result<u32> {
        if self.header.freelist_count > 0 && self.header.freelist_trunk != 0 {
            return self.alloc_from_freelist();
        }
        self.page_count += 1;
        let n = self.page_count;
        self.overlay.insert(n, vec![0u8; self.page_size]);
        Ok(n)
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

    /// Discard all staged changes (ROLLBACK).
    pub fn rollback(&mut self) {
        self.overlay.clear();
        self.page_count = self.disk_pages;
    }

    /// Atomically flush all staged changes to the database file.
    pub fn commit(&mut self) -> Result<()> {
        if self.overlay.is_empty() {
            return Ok(());
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
        Ok(())
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
