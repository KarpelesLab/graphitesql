//! Parsing of a single b-tree page: its header, cell pointer array, and cells.
//!
//! A SQLite database is a forest of b-trees, each a tree of pages. There are
//! four page kinds (file-format spec, "B-tree Pages"):
//!
//! * **table leaf** (0x0d) — rows, keyed by 64-bit rowid; cells hold rowid +
//!   payload.
//! * **table interior** (0x05) — navigation; cells hold a left-child pointer +
//!   a rowid separator key. Rows live only in the leaves.
//! * **index leaf** (0x0a) — index entries; cells hold a payload (the key).
//! * **index interior** (0x02) — cells hold a left-child pointer + a payload.
//!   In index trees the interior cells are *real entries*, not just separators.
//!
//! The page header is 8 bytes on leaves, 12 on interior pages (the extra 4 are
//! the right-most child pointer). Offsets in the header and cell-pointer array
//! are measured from the start of the *page*, not the b-tree content area —
//! which matters for page 1, whose content starts 100 bytes in.

use crate::error::{Error, Result};
use crate::pager::Page;
use crate::util::varint;
use alloc::format;

/// The kind of a b-tree page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    /// Interior index page (0x02).
    InteriorIndex,
    /// Interior table page (0x05).
    InteriorTable,
    /// Leaf index page (0x0a).
    LeafIndex,
    /// Leaf table page (0x0d).
    LeafTable,
}

impl PageType {
    fn from_byte(b: u8) -> Result<PageType> {
        Ok(match b {
            2 => PageType::InteriorIndex,
            5 => PageType::InteriorTable,
            10 => PageType::LeafIndex,
            13 => PageType::LeafTable,
            other => return Err(Error::Corrupt(format!("invalid b-tree page type {other}"))),
        })
    }

    /// Whether this is a leaf (no children).
    pub fn is_leaf(self) -> bool {
        matches!(self, PageType::LeafIndex | PageType::LeafTable)
    }

    /// Whether this page belongs to a table b-tree (vs. an index b-tree).
    pub fn is_table(self) -> bool {
        matches!(self, PageType::InteriorTable | PageType::LeafTable)
    }

    fn header_len(self) -> usize {
        if self.is_leaf() {
            8
        } else {
            12
        }
    }
}

#[inline]
fn be_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([buf[at], buf[at + 1]])
}

#[inline]
fn be_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// Read a big-endian `u32` at `at`, returning [`Error::Corrupt`] (rather than
/// panicking) when the four bytes would run past the end of `buf`. Used for
/// reads at attacker-controlled cell offsets.
#[inline]
fn be_u32_checked(buf: &[u8], at: usize) -> Result<u32> {
    let bytes = buf
        .get(at..at + 4)
        .ok_or_else(|| Error::Corrupt("4-byte read past end of page".into()))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// How a cell's payload is split between this page and overflow pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payload {
    /// Total payload length in bytes (across all overflow pages).
    pub total_len: usize,
    /// Offset within the *page bytes* where the local payload begins.
    pub local_offset: usize,
    /// Number of payload bytes stored locally on this page.
    pub local_len: usize,
    /// First overflow page number, or 0 if the payload fits locally.
    pub overflow: u32,
}

/// A parsed table-leaf cell.
#[derive(Debug, Clone, Copy)]
pub struct TableLeafCell {
    /// The row's integer key.
    pub rowid: i64,
    /// Where the row record lives.
    pub payload: Payload,
}

/// A parsed index cell (leaf or interior).
#[derive(Debug, Clone, Copy)]
pub struct IndexCell {
    /// Left child page (0 for leaf cells).
    pub left_child: u32,
    /// Where the index-key record lives.
    pub payload: Payload,
}

/// A parsed b-tree page, borrowing the underlying [`Page`] bytes (cheaply, via
/// the page's reference-counted buffer).
#[derive(Debug, Clone)]
pub struct BtreePage {
    page: Page,
    page_type: PageType,
    body_offset: usize,
    num_cells: usize,
    right_pointer: u32,
}

impl BtreePage {
    /// Parse the b-tree header of `page`.
    pub fn parse(page: Page) -> Result<BtreePage> {
        let body = page.body_offset();
        let data = page.data();
        if body + 8 > data.len() {
            return Err(Error::Corrupt("page too small for b-tree header".into()));
        }
        let page_type = PageType::from_byte(data[body])?;
        if body + page_type.header_len() > data.len() {
            return Err(Error::Corrupt("page too small for interior header".into()));
        }
        let num_cells = be_u16(data, body + 3) as usize;
        let right_pointer = if page_type.is_leaf() {
            0
        } else {
            be_u32(data, body + 8)
        };
        Ok(BtreePage {
            page,
            page_type,
            body_offset: body,
            num_cells,
            right_pointer,
        })
    }

    /// This page's type.
    pub fn page_type(&self) -> PageType {
        self.page_type
    }

    /// Number of cells on this page.
    pub fn num_cells(&self) -> usize {
        self.num_cells
    }

    /// The right-most child pointer (interior pages only; 0 on leaves).
    pub fn right_pointer(&self) -> u32 {
        self.right_pointer
    }

    /// The raw page bytes.
    pub fn data(&self) -> &[u8] {
        self.page.data()
    }

    /// Byte offset of cell `i` within the page.
    fn cell_offset(&self, i: usize) -> Result<usize> {
        if i >= self.num_cells {
            return Err(Error::Corrupt(format!(
                "cell index {i} out of range (num_cells={})",
                self.num_cells
            )));
        }
        let ptr = self.body_offset + self.page_type.header_len() + 2 * i;
        let data = self.data();
        if ptr + 2 > data.len() {
            return Err(Error::Corrupt("cell pointer past end of page".into()));
        }
        let off = be_u16(data, ptr) as usize;
        if off >= data.len() {
            return Err(Error::Corrupt("cell offset past end of page".into()));
        }
        Ok(off)
    }

    /// The child pointer to descend at position `i`: the left child of cell `i`
    /// for `i < num_cells`, or the right-most pointer for `i == num_cells`.
    /// Interior pages only.
    pub fn child_pointer(&self, i: usize) -> Result<u32> {
        if self.page_type.is_leaf() {
            return Err(Error::Corrupt(
                "child pointer requested on a leaf page".into(),
            ));
        }
        if i >= self.num_cells {
            return Ok(self.right_pointer);
        }
        let off = self.cell_offset(i)?;
        be_u32_checked(self.data(), off)
    }

    /// The rowid separator key of interior-table cell `i`.
    pub fn table_interior_key(&self, i: usize) -> Result<i64> {
        if self.page_type != PageType::InteriorTable {
            return Err(Error::Corrupt(
                "interior-table key on non-interior page".into(),
            ));
        }
        let off = self.cell_offset(i)?;
        // Layout: 4-byte left child, then varint rowid.
        let data = self.data();
        let key_off = off
            .checked_add(4)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| Error::Corrupt("interior cell past end of page".into()))?;
        let (rowid, _) = varint::decode_i64(&data[key_off..])
            .ok_or_else(|| Error::Corrupt("truncated interior cell rowid".into()))?;
        Ok(rowid)
    }

    /// Parse table-leaf cell `i`, given the database's usable page size.
    pub fn table_leaf_cell(&self, i: usize, usable: usize) -> Result<TableLeafCell> {
        if self.page_type != PageType::LeafTable {
            return Err(Error::Corrupt(
                "table-leaf cell on non-table-leaf page".into(),
            ));
        }
        let off = self.cell_offset(i)?;
        let data = self.data();
        // Layout: varint payload length, varint rowid, then payload.
        let (plen, n1) = varint::decode(&data[off..])
            .ok_or_else(|| Error::Corrupt("truncated leaf payload length".into()))?;
        let (rowid, n2) = varint::decode_i64(&data[off + n1..])
            .ok_or_else(|| Error::Corrupt("truncated leaf rowid".into()))?;
        let payload_start = off + n1 + n2;
        let payload = self.payload_at(payload_start, plen as usize, usable)?;
        Ok(TableLeafCell { rowid, payload })
    }

    /// The raw bytes of table-leaf cell `i`, including any 4-byte overflow
    /// pointer. Used by the writer to move cells between pages while preserving
    /// their overflow chains.
    pub fn raw_table_leaf_cell(&self, i: usize, usable: usize) -> Result<&[u8]> {
        if self.page_type != PageType::LeafTable {
            return Err(Error::Corrupt(
                "raw table-leaf cell on non-table-leaf page".into(),
            ));
        }
        let off = self.cell_offset(i)?;
        let data = self.data();
        let (plen, n1) = varint::decode(&data[off..])
            .ok_or_else(|| Error::Corrupt("truncated leaf payload length".into()))?;
        let (_, n2) = varint::decode_i64(&data[off + n1..])
            .ok_or_else(|| Error::Corrupt("truncated leaf rowid".into()))?;
        let (local_len, has_overflow) = payload_split(self.page_type, usable, plen as usize);
        let len = n1 + n2 + local_len + if has_overflow { 4 } else { 0 };
        if off + len > data.len() {
            return Err(Error::Corrupt("leaf cell extends past page".into()));
        }
        Ok(&data[off..off + len])
    }

    /// The raw "record cell" bytes of index cell `i`: the `varint(len)` + local
    /// payload (+ overflow pointer), i.e. everything after the optional 4-byte
    /// left-child pointer. Used by the writer to move index entries between pages.
    pub fn raw_index_record_cell(&self, i: usize, usable: usize) -> Result<&[u8]> {
        if self.page_type.is_table() {
            return Err(Error::Corrupt("index record cell on a table page".into()));
        }
        let off = self.cell_offset(i)?;
        let data = self.data();
        let key_off = if self.page_type == PageType::InteriorIndex {
            off + 4
        } else {
            off
        };
        let key_bytes = data
            .get(key_off..)
            .ok_or_else(|| Error::Corrupt("index cell key past end of page".into()))?;
        let (plen, n1) = varint::decode(key_bytes)
            .ok_or_else(|| Error::Corrupt("truncated index payload length".into()))?;
        let (local_len, has_overflow) = payload_split(self.page_type, usable, plen as usize);
        let len = n1 + local_len + if has_overflow { 4 } else { 0 };
        if key_off + len > data.len() {
            return Err(Error::Corrupt("index cell extends past page".into()));
        }
        Ok(&data[key_off..key_off + len])
    }

    /// Parse index cell `i` (works for both leaf and interior index pages).
    pub fn index_cell(&self, i: usize, usable: usize) -> Result<IndexCell> {
        if self.page_type.is_table() {
            return Err(Error::Corrupt("index cell on a table page".into()));
        }
        let off = self.cell_offset(i)?;
        let data = self.data();
        let (left_child, key_off) = if self.page_type == PageType::InteriorIndex {
            (be_u32_checked(data, off)?, off + 4)
        } else {
            (0, off)
        };
        let key_bytes = data
            .get(key_off..)
            .ok_or_else(|| Error::Corrupt("index cell key past end of page".into()))?;
        let (plen, n1) = varint::decode(key_bytes)
            .ok_or_else(|| Error::Corrupt("truncated index payload length".into()))?;
        let payload = self.payload_at(key_off + n1, plen as usize, usable)?;
        Ok(IndexCell {
            left_child,
            payload,
        })
    }

    /// For an interior page: the byte offset within the page of the 4-byte child
    /// pointer for child position `i` (`i < num_cells` → the cell's left child;
    /// `i == num_cells` → the page header's right-most child pointer). Used by the
    /// auto-vacuum relocator to rewrite a parent's pointer to a moved child page.
    pub fn child_pointer_offset(&self, i: usize) -> Result<usize> {
        if self.page_type.is_leaf() {
            return Err(Error::Corrupt(
                "child pointer offset requested on a leaf page".into(),
            ));
        }
        if i >= self.num_cells {
            // Right-most child pointer lives at body_offset + 8.
            return Ok(self.body_offset + 8);
        }
        self.cell_offset(i)
    }

    /// For a cell that owns an overflow chain, the byte offset within the page of
    /// the 4-byte "first overflow page" pointer at the tail of its local payload.
    /// Returns `None` if the cell has no overflow chain. Used by the relocator to
    /// rewrite an `Overflow1` holder's link to a moved overflow page.
    pub fn cell_overflow_offset(&self, i: usize, usable: usize) -> Result<Option<usize>> {
        let payload = match self.page_type {
            PageType::LeafTable => self.table_leaf_cell(i, usable)?.payload,
            PageType::LeafIndex | PageType::InteriorIndex => self.index_cell(i, usable)?.payload,
            PageType::InteriorTable => return Ok(None),
        };
        if payload.overflow == 0 {
            Ok(None)
        } else {
            Ok(Some(payload.local_offset + payload.local_len))
        }
    }

    /// Compute the [`Payload`] descriptor for a payload of `total` bytes whose
    /// local portion begins at `payload_start` within the page.
    fn payload_at(&self, payload_start: usize, total: usize, usable: usize) -> Result<Payload> {
        let (local_len, has_overflow) = payload_split(self.page_type, usable, total);
        let data = self.data();
        let need = payload_start + local_len + if has_overflow { 4 } else { 0 };
        if need > data.len() {
            return Err(Error::Corrupt("cell payload past end of page".into()));
        }
        let overflow = if has_overflow {
            be_u32(data, payload_start + local_len)
        } else {
            0
        };
        Ok(Payload {
            total_len: total,
            local_offset: payload_start,
            local_len,
            overflow,
        })
    }
}

/// How many payload bytes are stored on the page itself, and whether any spill
/// onto overflow pages. Implements the spill algorithm from the file-format
/// spec ("the initial portion of the payload that does not spill to overflow").
pub(crate) fn payload_split(page_type: PageType, usable: usize, p: usize) -> (usize, bool) {
    // Saturating arithmetic throughout: on a *valid* database `usable` is always
    // at least 480 (page size >= 512), so every subtraction below is identical to
    // plain `-`. On a *corrupt* header `usable` can be tiny, which would otherwise
    // underflow these `usize` subtractions and panic; saturating keeps the reader
    // returning an `Err` from the surrounding bounds checks instead of crashing.
    // Maximum bytes of payload kept locally before overflow is used.
    let max_local = match page_type {
        PageType::LeafTable => usable.saturating_sub(35),
        PageType::LeafIndex | PageType::InteriorIndex => {
            (usable.saturating_sub(12) * 64 / 255).saturating_sub(23)
        }
        // Interior table cells carry no payload; never called.
        PageType::InteriorTable => return (0, false),
    };
    if p <= max_local {
        return (p, false);
    }
    let min_local = (usable.saturating_sub(12) * 32 / 255).saturating_sub(23);
    // `usable - 4` is the overflow-page payload capacity; guard the modulus so a
    // corrupt `usable <= 4` cannot divide by zero.
    let span = usable.saturating_sub(4).max(1);
    let k = min_local + p.saturating_sub(min_local) % span;
    let local = if k <= max_local { k } else { min_local };
    (local, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_split_no_overflow_when_small() {
        // 4096 usable, table leaf: max_local = 4061. A 100-byte payload is local.
        let (local, ov) = payload_split(PageType::LeafTable, 4096, 100);
        assert_eq!((local, ov), (100, false));
    }

    #[test]
    fn payload_split_overflow_for_large_table_leaf() {
        let usable = 4096;
        let p = 20000;
        let (local, ov) = payload_split(PageType::LeafTable, usable, p);
        assert!(ov);
        // local must be within [min_local, max_local].
        let max_local = usable - 35;
        let min_local = (usable - 12) * 32 / 255 - 23;
        assert!(local >= min_local && local <= max_local);
        // The K formula keeps (p - local) a multiple of (usable-4) so the tail
        // fills whole overflow pages — verify the residue rule held.
        let k = min_local + (p - min_local) % (usable - 4);
        let expect = if k <= max_local { k } else { min_local };
        assert_eq!(local, expect);
    }
}
