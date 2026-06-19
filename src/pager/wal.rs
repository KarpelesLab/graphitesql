//! Read support for the SQLite Write-Ahead Log (WAL).
//!
//! When a database is in WAL mode, the newest committed version of a page may
//! live in the `-wal` file rather than the main database file. To read such a
//! database correctly we parse the WAL, validate it, and overlay its frames on
//! top of the main file. The WAL format and its checksum algorithm are specified
//! in the file-format docs ("Write-Ahead Log Format") and `wal.c`.
//!
//! WAL layout:
//!
//! * **32-byte header**: magic (`0x377f0682`/`0x377f0683` — LSB selects checksum
//!   byte order), format version, page size, checkpoint sequence, two salt
//!   values, and the header checksum.
//! * **frames**: a 24-byte header (page number; db size in pages for a *commit*
//!   frame else 0; the two salts; the running checksum) followed by one page of
//!   data.
//!
//! A frame is valid only if its salts match the header's and the running
//! checksum (seeded from the previous frame, or the header checksum for the
//! first frame) matches. We honor frames up to the last valid **commit** frame;
//! the page→frame map keeps the most recent frame per page within that range.
//!
//! This is the **read** half of WAL. Writing WAL (and the `-shm` wal-index) is
//! tracked for later; reading is what compatibility with WAL-mode databases
//! requires.

use super::{Page, PageSource};
use crate::error::{Error, Result};
use crate::format::header::HEADER_LEN;
use crate::format::DatabaseHeader;
use crate::vfs::File;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

const WAL_HEADER_LEN: usize = 32;
const FRAME_HEADER_LEN: usize = 24;
const WAL_MAGIC_BE: u32 = 0x377f_0682; // base magic; LSB carries the endianness

/// A parsed, validated WAL overlaid on a main database file, presented as a
/// [`PageSource`] that returns WAL frames where they exist and main-file pages
/// otherwise.
pub struct WalReader {
    main: Box<dyn File>,
    header: DatabaseHeader,
    page_size: usize,
    /// Database size in pages at the snapshot (from the last commit frame, or
    /// the main file if the WAL has no committed frames).
    db_size: u32,
    /// Page number → page bytes, for pages whose newest version is in the WAL.
    frames: BTreeMap<u32, Vec<u8>>,
}

impl WalReader {
    /// Open `main` overlaid with the WAL bytes from `wal`. If the WAL is empty or
    /// invalid, the main file is used alone.
    pub fn open(main: Box<dyn File>, wal: &mut dyn File) -> Result<WalReader> {
        let wal_size = wal.size()?;
        let mut frames = BTreeMap::new();
        let mut commit_db_size = 0u32;
        let mut wal_page_size = 0usize;

        if wal_size >= WAL_HEADER_LEN as u64 {
            let mut hdr = [0u8; WAL_HEADER_LEN];
            wal.read_exact_at(&mut hdr, 0)?;
            let magic = be32(&hdr, 0);
            if magic & 0xFFFF_FFFE != WAL_MAGIC_BE {
                return Err(Error::Corrupt("bad WAL magic".into()));
            }
            // The magic's least-significant bit selects the checksum byte order:
            // 1 = big-endian, 0 = little-endian (per `wal.c`'s `bigEndCksum`).
            let big_endian = (magic & 1) == 1;
            let page_size = be32(&hdr, 8) as usize;
            if page_size < 512 || !page_size.is_power_of_two() {
                return Err(Error::Corrupt("bad WAL page size".into()));
            }
            wal_page_size = page_size;
            let salt = &hdr[16..24];

            // Validate the header checksum (over the first 24 bytes, seed 0,0).
            let (h0, h1) = checksum(big_endian, 0, 0, &hdr[0..24]);
            if h0 != be32(&hdr, 24) || h1 != be32(&hdr, 28) {
                return Err(Error::Corrupt("WAL header checksum mismatch".into()));
            }

            // Walk frames, keeping a running checksum; record committed frames.
            let frame_len = FRAME_HEADER_LEN + page_size;
            let (mut s0, mut s1) = (h0, h1);
            let mut off = WAL_HEADER_LEN as u64;
            // Tentative frames since the last commit (applied atomically on commit).
            let mut pending: Vec<(u32, Vec<u8>)> = Vec::new();
            while off + frame_len as u64 <= wal_size {
                let mut fhdr = [0u8; FRAME_HEADER_LEN];
                wal.read_exact_at(&mut fhdr, off)?;
                let mut page = vec![0u8; page_size];
                wal.read_exact_at(&mut page, off + FRAME_HEADER_LEN as u64)?;

                // Salts must match the header, else the WAL was restarted here.
                if fhdr[8..16] != *salt {
                    break;
                }
                // Running checksum over frame-header[0..8] then the page data.
                let (c0, c1) = checksum(big_endian, s0, s1, &fhdr[0..8]);
                let (c0, c1) = checksum(big_endian, c0, c1, &page);
                if c0 != be32(&fhdr, 16) || c1 != be32(&fhdr, 20) {
                    break; // corrupt/torn frame: stop here
                }
                s0 = c0;
                s1 = c1;

                let page_no = be32(&fhdr, 0);
                let db_size = be32(&fhdr, 4);
                pending.push((page_no, page));
                if db_size != 0 {
                    // Commit frame: publish all pending frames atomically.
                    for (p, data) in pending.drain(..) {
                        frames.insert(p, data);
                    }
                    commit_db_size = db_size;
                }
                off += frame_len as u64;
            }
        }

        // Determine the authoritative page 1 (it may itself be in the WAL).
        let page1 = match frames.get(&1) {
            Some(p) => p.clone(),
            None => {
                let mut buf = vec![0u8; HEADER_LEN.max(512)];
                let want = main.size()?.min(buf.len() as u64) as usize;
                buf.truncate(want);
                main.read_exact_at(&mut buf, 0)?;
                buf
            }
        };
        let header = DatabaseHeader::parse(&page1)?;
        let page_size = if wal_page_size != 0 {
            wal_page_size
        } else {
            header.page_size as usize
        };

        let db_size = if commit_db_size != 0 {
            commit_db_size
        } else {
            (main.size()? / page_size as u64) as u32
        };

        Ok(WalReader {
            main,
            header,
            page_size,
            db_size,
            frames,
        })
    }
}

impl PageSource for WalReader {
    fn page(&self, number: u32) -> Result<Page> {
        if number == 0 || number > self.db_size {
            return Err(Error::Corrupt(alloc::format!(
                "page {number} out of range 1..={}",
                self.db_size
            )));
        }
        if let Some(data) = self.frames.get(&number) {
            return Ok(Page::from_bytes(number, data.clone()));
        }
        let mut buf = vec![0u8; self.page_size];
        self.main
            .read_exact_at(&mut buf, (number as u64 - 1) * self.page_size as u64)?;
        Ok(Page::from_bytes(number, buf))
    }
    fn header(&self) -> &DatabaseHeader {
        &self.header
    }
    fn usable_size(&self) -> usize {
        self.header.usable_size() as usize
    }
    fn page_count(&self) -> u32 {
        self.db_size
    }
}

#[inline]
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The WAL running checksum. Consumes `data` (length a multiple of 8) as pairs of
/// 32-bit words in the selected byte order, updating the (s0, s1) accumulators.
pub(crate) fn checksum(big_endian: bool, mut s0: u32, mut s1: u32, data: &[u8]) -> (u32, u32) {
    let read = |at: usize| -> u32 {
        if big_endian {
            u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
        } else {
            u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
        }
    };
    let mut i = 0;
    while i + 8 <= data.len() {
        s0 = s0.wrapping_add(read(i)).wrapping_add(s1);
        s1 = s1.wrapping_add(read(i + 4)).wrapping_add(s0);
        i += 8;
    }
    (s0, s1)
}
