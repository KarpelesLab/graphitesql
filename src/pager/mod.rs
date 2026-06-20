//! The pager: turns a [`File`] into numbered fixed-size pages.
//!
//! This is the boundary between "bytes in a file" and "the b-tree's pages".
//! Page numbers are 1-based, as in SQLite; page *N* lives at byte offset
//! `(N-1) * page_size`. Page 1 is special: its first 100 bytes are the database
//! header, so b-tree content on page 1 starts at offset 100 (see
//! [`Page::body_offset`]).
//!
//! This phase implements the **read side** only: an immutable, cached view of
//! the file. Dirty-page tracking, the rollback journal, WAL, and atomic commit
//! arrive in Phase 6/8. The cache here is intentionally simple (read-through,
//! unbounded) and will be replaced by a bounded page cache then.

use crate::error::{Error, Result};
use crate::format::DatabaseHeader;
use crate::vfs::File;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

pub mod wal;
pub mod write;
pub use wal::WalReader;
pub use write::{AutoVacuum, WritePager};

/// A single database page: its number and its raw bytes.
///
/// The bytes are reference-counted so handing out a page is cheap and the cache
/// can share it with callers.
#[derive(Debug, Clone)]
pub struct Page {
    number: u32,
    data: Rc<Vec<u8>>,
}

impl Page {
    /// Build a page from raw bytes (used by writers and synthetic sources).
    pub fn from_bytes(number: u32, data: Vec<u8>) -> Page {
        Page {
            number,
            data: Rc::new(data),
        }
    }

    /// This page's 1-based number.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The full raw bytes of the page (including the file header on page 1).
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Byte offset within this page where b-tree content begins: 100 on page 1
    /// (after the database header), 0 on every other page.
    pub fn body_offset(&self) -> usize {
        if self.number == 1 {
            crate::format::header::HEADER_LEN
        } else {
            0
        }
    }
}

/// A source of database pages.
///
/// Abstracts "where pages come from" so the b-tree cursors and the schema
/// reader work identically over a read-only [`Pager`] and the write-side pager
/// (which serves dirty in-transaction pages from its overlay).
pub trait PageSource {
    /// Fetch page `number` (1-based).
    fn page(&self, number: u32) -> Result<Page>;
    /// The database header.
    fn header(&self) -> &DatabaseHeader;
    /// Usable bytes per page (page size minus reserved space).
    fn usable_size(&self) -> usize;
    /// Total number of pages.
    fn page_count(&self) -> u32;
}

impl PageSource for Pager {
    fn page(&self, number: u32) -> Result<Page> {
        Pager::page(self, number)
    }
    fn header(&self) -> &DatabaseHeader {
        Pager::header(self)
    }
    fn usable_size(&self) -> usize {
        Pager::usable_size(self)
    }
    fn page_count(&self) -> u32 {
        Pager::page_count(self)
    }
}

/// Reads numbered pages from a database file.
pub struct Pager {
    file: Box<dyn File>,
    header: DatabaseHeader,
    page_size: usize,
    page_count: u32,
    cache: RefCell<BTreeMap<u32, Rc<Vec<u8>>>>,
}

impl Pager {
    /// Open a pager over `file`, reading and validating the database header.
    ///
    /// The page count is derived from the file size (the authoritative source
    /// for a reader); an empty file is rejected as not-a-database.
    pub fn open(file: Box<dyn File>) -> Result<Pager> {
        let file_size = file.size()?;
        if file_size < crate::format::header::HEADER_LEN as u64 {
            return Err(Error::Corrupt(format!(
                "file is {file_size} bytes, too small to be a database"
            )));
        }

        let mut head = [0u8; crate::format::header::HEADER_LEN];
        file.read_exact_at(&mut head, 0)?;
        let header = DatabaseHeader::parse(&head)?;
        let page_size = header.page_size as usize;

        if file_size % page_size as u64 != 0 {
            return Err(Error::Corrupt(format!(
                "file size {file_size} is not a multiple of page size {page_size}"
            )));
        }
        let page_count = (file_size / page_size as u64) as u32;

        Ok(Pager {
            file,
            header,
            page_size,
            page_count,
            cache: RefCell::new(BTreeMap::new()),
        })
    }

    /// The parsed database header.
    pub fn header(&self) -> &DatabaseHeader {
        &self.header
    }

    /// The page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// The number of pages in the database file.
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// The number of usable bytes per page (page size minus reserved space).
    pub fn usable_size(&self) -> usize {
        self.header.usable_size() as usize
    }

    /// Fetch page `number` (1-based), reading it from the file on a cache miss.
    pub fn page(&self, number: u32) -> Result<Page> {
        if number == 0 || number > self.page_count {
            return Err(Error::Corrupt(format!(
                "page {number} out of range 1..={}",
                self.page_count
            )));
        }
        if let Some(data) = self.cache.borrow().get(&number) {
            return Ok(Page {
                number,
                data: Rc::clone(data),
            });
        }

        let mut buf = vec![0u8; self.page_size];
        let offset = (number as u64 - 1) * self.page_size as u64;
        self.file.read_exact_at(&mut buf, offset)?;
        let data = Rc::new(buf);
        self.cache.borrow_mut().insert(number, Rc::clone(&data));
        Ok(Page { number, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::TextEncoding;
    use crate::vfs::{memory::MemoryVfs, OpenFlags, Vfs};

    /// Build a minimal but valid 2-page database in a MemoryVfs and return it.
    fn synthetic_db() -> (MemoryVfs, alloc::string::String) {
        let vfs = MemoryVfs::new();
        let path = alloc::string::String::from("db");
        let page_size = 4096u32;

        let header = DatabaseHeader {
            page_size,
            write_version: 1,
            read_version: 1,
            reserved_space: 0,
            change_counter: 1,
            size_in_pages: 2,
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
            sqlite_version_number: 3_046_001,
        };

        let mut f = vfs.open(&path, OpenFlags::READ_WRITE_CREATE).unwrap();
        let mut page1 = vec![0u8; page_size as usize];
        header.write_to(&mut page1).unwrap();
        page1[100] = 0xAA; // marker in page 1 body
        f.write_all_at(&page1, 0).unwrap();

        let mut page2 = vec![0u8; page_size as usize];
        page2[0] = 0xBB; // marker in page 2
        f.write_all_at(&page2, page_size as u64).unwrap();

        (vfs, path)
    }

    #[test]
    fn opens_and_reports_geometry() {
        let (vfs, path) = synthetic_db();
        let file = vfs.open(&path, OpenFlags::READ_ONLY).unwrap();
        let pager = Pager::open(file).unwrap();
        assert_eq!(pager.page_size(), 4096);
        assert_eq!(pager.page_count(), 2);
        assert_eq!(pager.usable_size(), 4096);
        assert_eq!(pager.header().text_encoding, TextEncoding::Utf8);
    }

    #[test]
    fn page1_body_offset_and_content() {
        let (vfs, path) = synthetic_db();
        let file = vfs.open(&path, OpenFlags::READ_ONLY).unwrap();
        let pager = Pager::open(file).unwrap();

        let p1 = pager.page(1).unwrap();
        assert_eq!(p1.number(), 1);
        assert_eq!(p1.body_offset(), 100);
        assert_eq!(p1.data()[100], 0xAA);
        // The header round-trips back out of page 1's raw bytes.
        let reparsed = DatabaseHeader::parse(p1.data()).unwrap();
        assert_eq!(&reparsed, pager.header());

        let p2 = pager.page(2).unwrap();
        assert_eq!(p2.body_offset(), 0);
        assert_eq!(p2.data()[0], 0xBB);
    }

    #[test]
    fn caching_returns_shared_bytes() {
        let (vfs, path) = synthetic_db();
        let file = vfs.open(&path, OpenFlags::READ_ONLY).unwrap();
        let pager = Pager::open(file).unwrap();
        let a = pager.page(2).unwrap();
        let b = pager.page(2).unwrap();
        assert!(Rc::ptr_eq(&a.data, &b.data)); // same cached allocation
    }

    #[test]
    fn out_of_range_pages_error() {
        let (vfs, path) = synthetic_db();
        let file = vfs.open(&path, OpenFlags::READ_ONLY).unwrap();
        let pager = Pager::open(file).unwrap();
        assert!(pager.page(0).is_err());
        assert!(pager.page(3).is_err());
    }
}
