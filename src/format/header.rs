//! The 100-byte database header at the start of every SQLite file.
//!
//! Layout (file-format spec, "The Database Header"). All multi-byte integers are
//! big-endian:
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0  | 16 | magic string `"SQLite format 3\0"` |
//! | 16 | 2  | page size in bytes (1 means 65536) |
//! | 18 | 1  | file format write version (1 = legacy, 2 = WAL) |
//! | 19 | 1  | file format read version (1 = legacy, 2 = WAL) |
//! | 20 | 1  | bytes of reserved space at end of each page |
//! | 21 | 1  | max embedded payload fraction (must be 64) |
//! | 22 | 1  | min embedded payload fraction (must be 32) |
//! | 23 | 1  | leaf payload fraction (must be 32) |
//! | 24 | 4  | file change counter |
//! | 28 | 4  | database size in pages ("in-header size") |
//! | 32 | 4  | first freelist trunk page (0 = none) |
//! | 36 | 4  | number of freelist pages |
//! | 40 | 4  | schema cookie |
//! | 44 | 4  | schema format number (1..=4) |
//! | 48 | 4  | default page cache size |
//! | 52 | 4  | largest root b-tree page (auto/incremental vacuum) |
//! | 56 | 4  | text encoding (1 = UTF-8, 2 = UTF-16le, 3 = UTF-16be) |
//! | 60 | 4  | user version |
//! | 64 | 4  | incremental-vacuum mode flag |
//! | 68 | 4  | application id |
//! | 72 | 20 | reserved, must be zero |
//! | 92 | 4  | version-valid-for number |
//! | 96 | 4  | `SQLITE_VERSION_NUMBER` of the last writer |

use crate::error::{Error, Result};
use alloc::format;

/// The length of the database header, in bytes.
pub const HEADER_LEN: usize = 100;

/// The 16-byte magic string that begins every SQLite database file.
pub const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Text encoding of string values in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// UTF-8 (encoding code 1). The only encoding graphitesql writes.
    Utf8,
    /// UTF-16 little-endian (code 2).
    Utf16Le,
    /// UTF-16 big-endian (code 3).
    Utf16Be,
}

impl TextEncoding {
    fn from_code(code: u32) -> Result<TextEncoding> {
        match code {
            1 => Ok(TextEncoding::Utf8),
            2 => Ok(TextEncoding::Utf16Le),
            3 => Ok(TextEncoding::Utf16Be),
            other => Err(Error::Corrupt(format!("invalid text encoding {other}"))),
        }
    }

    fn code(self) -> u32 {
        match self {
            TextEncoding::Utf8 => 1,
            TextEncoding::Utf16Le => 2,
            TextEncoding::Utf16Be => 3,
        }
    }
}

/// A parsed SQLite database header.
///
/// Fields are named to match the file-format spec. Construct one by [`parse`]-ing
/// the first 100 bytes of a database file, and serialize it back with
/// [`write_to`].
///
/// [`parse`]: DatabaseHeader::parse
/// [`write_to`]: DatabaseHeader::write_to
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseHeader {
    /// Page size in bytes; a power of two in `512..=65536`.
    pub page_size: u32,
    /// File format write version (1 = legacy rollback journal, 2 = WAL).
    pub write_version: u8,
    /// File format read version (1 = legacy, 2 = WAL).
    pub read_version: u8,
    /// Bytes reserved at the end of each page (usually 0).
    pub reserved_space: u8,
    /// File change counter, bumped on each transaction.
    pub change_counter: u32,
    /// Database size in pages, valid only when it equals the change counter's
    /// companion ("version-valid-for"); see [`Self::size_in_pages_valid`].
    pub size_in_pages: u32,
    /// Page number of the first freelist trunk page (0 = empty freelist).
    pub freelist_trunk: u32,
    /// Total number of pages on the freelist.
    pub freelist_count: u32,
    /// Schema cookie; changes whenever the schema changes.
    pub schema_cookie: u32,
    /// Schema format number (1..=4).
    pub schema_format: u32,
    /// Suggested default page-cache size.
    pub default_cache_size: u32,
    /// Page number of the largest root b-tree page when in auto/incremental
    /// vacuum mode; 0 otherwise.
    pub largest_root_page: u32,
    /// Text encoding for string values.
    pub text_encoding: TextEncoding,
    /// User version, set via `PRAGMA user_version`.
    pub user_version: u32,
    /// Nonzero if the database is in incremental-vacuum mode.
    pub incremental_vacuum: u32,
    /// Application id, set via `PRAGMA application_id`.
    pub application_id: u32,
    /// The change counter value for which `size_in_pages` is valid.
    pub version_valid_for: u32,
    /// `SQLITE_VERSION_NUMBER` of the library that last wrote the file.
    pub sqlite_version_number: u32,
}

#[inline]
fn be_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

impl DatabaseHeader {
    /// Parse the 100-byte header from the start of `buf`.
    ///
    /// Validates everything the spec requires a *reader* to check: the magic
    /// string, that the page size is a power of two in range, the payload
    /// fractions, and the text encoding. Returns [`Error::Corrupt`] otherwise.
    pub fn parse(buf: &[u8]) -> Result<DatabaseHeader> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Corrupt(format!(
                "header is {} bytes, need {HEADER_LEN}",
                buf.len()
            )));
        }
        if &buf[0..16] != MAGIC.as_slice() {
            return Err(Error::Corrupt("bad magic string".into()));
        }

        // Page size: stored as u16; the value 1 means 65536.
        let raw_page_size = u16::from_be_bytes([buf[16], buf[17]]);
        let page_size: u32 = if raw_page_size == 1 {
            65536
        } else {
            u32::from(raw_page_size)
        };
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(Error::Corrupt(format!("invalid page size {page_size}")));
        }

        // Payload fractions are fixed by the format and must hold exactly.
        if buf[21] != 64 || buf[22] != 32 || buf[23] != 32 {
            return Err(Error::Corrupt("invalid payload fractions".into()));
        }

        let reserved_space = buf[20];
        // Usable space per page must stay positive and leave room for a cell.
        if u32::from(reserved_space) >= page_size {
            return Err(Error::Corrupt("reserved space exceeds page size".into()));
        }

        Ok(DatabaseHeader {
            page_size,
            write_version: buf[18],
            read_version: buf[19],
            reserved_space,
            change_counter: be_u32(buf, 24),
            size_in_pages: be_u32(buf, 28),
            freelist_trunk: be_u32(buf, 32),
            freelist_count: be_u32(buf, 36),
            schema_cookie: be_u32(buf, 40),
            schema_format: be_u32(buf, 44),
            default_cache_size: be_u32(buf, 48),
            largest_root_page: be_u32(buf, 52),
            text_encoding: TextEncoding::from_code(be_u32(buf, 56))?,
            user_version: be_u32(buf, 60),
            incremental_vacuum: be_u32(buf, 64),
            application_id: be_u32(buf, 68),
            version_valid_for: be_u32(buf, 92),
            sqlite_version_number: be_u32(buf, 96),
        })
    }

    /// Serialize this header into the first 100 bytes of `buf`.
    ///
    /// `buf` must be at least [`HEADER_LEN`] bytes. Bytes 72..92 are written as
    /// the required zero padding. Returns [`Error::Error`] if `buf` is too short.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<()> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Error(format!(
                "output buffer is {} bytes, need {HEADER_LEN}",
                buf.len()
            )));
        }
        buf[0..16].copy_from_slice(MAGIC);
        let raw_page_size: u16 = if self.page_size == 65536 {
            1
        } else {
            self.page_size as u16
        };
        buf[16..18].copy_from_slice(&raw_page_size.to_be_bytes());
        buf[18] = self.write_version;
        buf[19] = self.read_version;
        buf[20] = self.reserved_space;
        buf[21] = 64;
        buf[22] = 32;
        buf[23] = 32;
        buf[24..28].copy_from_slice(&self.change_counter.to_be_bytes());
        buf[28..32].copy_from_slice(&self.size_in_pages.to_be_bytes());
        buf[32..36].copy_from_slice(&self.freelist_trunk.to_be_bytes());
        buf[36..40].copy_from_slice(&self.freelist_count.to_be_bytes());
        buf[40..44].copy_from_slice(&self.schema_cookie.to_be_bytes());
        buf[44..48].copy_from_slice(&self.schema_format.to_be_bytes());
        buf[48..52].copy_from_slice(&self.default_cache_size.to_be_bytes());
        buf[52..56].copy_from_slice(&self.largest_root_page.to_be_bytes());
        buf[56..60].copy_from_slice(&self.text_encoding.code().to_be_bytes());
        buf[60..64].copy_from_slice(&self.user_version.to_be_bytes());
        buf[64..68].copy_from_slice(&self.incremental_vacuum.to_be_bytes());
        buf[68..72].copy_from_slice(&self.application_id.to_be_bytes());
        buf[72..92].fill(0);
        buf[92..96].copy_from_slice(&self.version_valid_for.to_be_bytes());
        buf[96..100].copy_from_slice(&self.sqlite_version_number.to_be_bytes());
        Ok(())
    }

    /// The number of usable bytes per page (page size minus reserved space).
    pub fn usable_size(&self) -> u32 {
        self.page_size - u32::from(self.reserved_space)
    }

    /// Whether [`Self::size_in_pages`] can be trusted (it is only authoritative
    /// when it was written by the same transaction as the change counter).
    pub fn size_in_pages_valid(&self) -> bool {
        self.size_in_pages != 0 && self.version_valid_for == self.change_counter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 100-byte header produced by `sqlite3` 3.46.1 for:
    /// `CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL);
    ///  INSERT INTO t VALUES (1,'hello',3.14),(2,'world',2.71);`
    const REAL_HEADER: [u8; 100] = [
        0x53, 0x51, 0x4c, 0x69, 0x74, 0x65, 0x20, 0x66, 0x6f, 0x72, 0x6d, 0x61, 0x74, 0x20, 0x33,
        0x00, 0x10, 0x00, 0x01, 0x01, 0x00, 0x40, 0x20, 0x20, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x2e, 0x7a, 0x71,
    ];

    #[test]
    fn parses_real_sqlite_header() {
        let h = DatabaseHeader::parse(&REAL_HEADER).expect("parse real header");
        assert_eq!(h.page_size, 4096);
        assert_eq!(h.write_version, 1); // legacy rollback journal
        assert_eq!(h.read_version, 1);
        assert_eq!(h.reserved_space, 0);
        assert_eq!(h.change_counter, 2);
        assert_eq!(h.size_in_pages, 2);
        assert_eq!(h.freelist_trunk, 0);
        assert_eq!(h.freelist_count, 0);
        assert_eq!(h.schema_cookie, 1);
        assert_eq!(h.schema_format, 4);
        assert_eq!(h.text_encoding, TextEncoding::Utf8);
        assert_eq!(h.version_valid_for, 2);
        assert_eq!(h.sqlite_version_number, 3_046_001); // 3.46.1
        assert_eq!(h.usable_size(), 4096);
        assert!(h.size_in_pages_valid());
    }

    #[test]
    fn round_trips_byte_for_byte() {
        let h = DatabaseHeader::parse(&REAL_HEADER).unwrap();
        let mut out = [0u8; HEADER_LEN];
        h.write_to(&mut out).unwrap();
        assert_eq!(out, REAL_HEADER, "re-serialized header must match input");
    }

    #[test]
    fn page_size_65536_uses_sentinel_one() {
        let mut h = DatabaseHeader::parse(&REAL_HEADER).unwrap();
        h.page_size = 65536;
        let mut out = [0u8; HEADER_LEN];
        h.write_to(&mut out).unwrap();
        assert_eq!(&out[16..18], &[0x00, 0x01]); // stored as 1
        let reparsed = DatabaseHeader::parse(&out).unwrap();
        assert_eq!(reparsed.page_size, 65536);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bad = REAL_HEADER;
        bad[0] = b'X';
        assert!(matches!(
            DatabaseHeader::parse(&bad),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn rejects_non_power_of_two_page_size() {
        let mut bad = REAL_HEADER;
        bad[16..18].copy_from_slice(&3000u16.to_be_bytes());
        assert!(matches!(
            DatabaseHeader::parse(&bad),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn rejects_bad_payload_fraction() {
        let mut bad = REAL_HEADER;
        bad[21] = 63; // must be 64
        assert!(matches!(
            DatabaseHeader::parse(&bad),
            Err(Error::Corrupt(_))
        ));
    }
}
