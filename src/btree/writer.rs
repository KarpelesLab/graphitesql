//! Writing to table b-trees: cell construction (with overflow), insertion, and
//! node splitting.
//!
//! The strategy is **whole-page rewrite**: to modify a page we read its cells
//! into a logical list, change the list, and re-serialize the page from scratch
//! with a canonical packed layout (cell-pointer array right after the header,
//! cell content packed against the end of the page, no freeblocks). SQLite
//! accepts any valid free-space layout, so canonical pages pass
//! `PRAGMA integrity_check`. This is simpler and less error-prone than in-place
//! edits, at some cost in write amplification.
//!
//! Insertion is the classic B-tree recursion: descend to the target leaf,
//! insert, and split bottom-up when a page overflows, propagating a separator to
//! the parent and growing a new root when the root itself splits. Index-tree
//! maintenance is handled in Phase 7.

use super::page::{payload_split, BtreePage, PageType};
use crate::error::{Error, Result};
use crate::pager::{PageSource, WritePager};
use crate::util::varint;
use alloc::vec;
use alloc::vec::Vec;

/// A leaf cell in logical form: its rowid and its raw on-page bytes.
type LeafCell = (i64, Vec<u8>);

/// A node split bubbling up to the parent: a separator key and the new right
/// sibling page that the parent must adopt.
struct Split {
    key: i64,
    right_page: u32,
}

/// Allocate a fresh, empty table b-tree (a single leaf page) and return its
/// root page number. Use this when creating a table.
pub fn create_table_root(wp: &mut WritePager) -> Result<u32> {
    let page_size = wp.usable_size() + wp.header().reserved_space as usize;
    let root = wp.allocate_page()?;
    let buf = serialize_leaf(page_size, 0, &[], None);
    wp.write_page(root, buf)?;
    Ok(root)
}

/// Insert (or replace) a row `(rowid, payload)` into the table b-tree at `root`.
pub fn insert_table(wp: &mut WritePager, root: u32, rowid: i64, payload: &[u8]) -> Result<()> {
    let cell = build_leaf_cell(wp, rowid, payload)?;
    if let Some(split) = insert_rec(wp, root, rowid, cell)? {
        grow_root(wp, root, split)?;
    }
    Ok(())
}

/// Delete the row with `rowid` from the table b-tree at `root`, if present.
/// Returns whether a row was removed.
///
/// Rewrites the containing leaf without the cell and returns any overflow pages
/// the row used to the freelist. (Leaf/interior pages are not merged on delete,
/// so an emptied leaf is left in place — valid, just not maximally compact.)
pub fn delete_table(wp: &mut WritePager, root: u32, rowid: i64) -> Result<bool> {
    // Locate the leaf containing rowid by descending the tree.
    let mut page_no = root;
    loop {
        let page = wp.page(page_no)?;
        let body = page.body_offset();
        let bt = BtreePage::parse(page)?;
        let usable = wp.usable_size();
        let page_size = usable + wp.header().reserved_space as usize;
        match bt.page_type() {
            PageType::LeafTable => {
                let mut cells = read_leaf_cells(&bt, usable)?;
                let Some(pos) = cells.iter().position(|(r, _)| *r == rowid) else {
                    return Ok(false);
                };
                // Collect this row's overflow chain so we can reclaim it.
                let mut overflow = 0u32;
                for i in 0..bt.num_cells() {
                    let cell = bt.table_leaf_cell(i, usable)?;
                    if cell.rowid == rowid {
                        overflow = cell.payload.overflow;
                        break;
                    }
                }
                cells.remove(pos);
                let header_prefix = page_one_prefix(page_no, &bt);
                let buf = serialize_leaf(page_size, body, &cells, header_prefix.as_deref());
                wp.write_page(page_no, buf)?;
                free_overflow_chain(wp, overflow)?;
                return Ok(true);
            }
            PageType::InteriorTable => {
                let n = bt.num_cells();
                let mut next = bt.right_pointer();
                for i in 0..n {
                    if rowid <= bt.table_interior_key(i)? {
                        next = bt.child_pointer(i)?;
                        break;
                    }
                }
                page_no = next;
            }
            _ => return Err(Error::Corrupt("delete from a non-table b-tree".into())),
        }
    }
}

/// Build the on-page cell bytes for a table-leaf row, allocating overflow pages
/// for any payload that does not fit locally.
fn build_leaf_cell(wp: &mut WritePager, rowid: i64, payload: &[u8]) -> Result<Vec<u8>> {
    let usable = wp.usable_size();
    let (local_len, has_overflow) = payload_split(PageType::LeafTable, usable, payload.len());

    let mut cell = Vec::new();
    let mut vbuf = [0u8; varint::MAX_LEN];
    let n = varint::encode(payload.len() as u64, &mut vbuf);
    cell.extend_from_slice(&vbuf[..n]);
    let n = varint::encode_i64(rowid, &mut vbuf);
    cell.extend_from_slice(&vbuf[..n]);
    cell.extend_from_slice(&payload[..local_len]);

    if has_overflow {
        let first = write_overflow_chain(wp, &payload[local_len..])?;
        cell.extend_from_slice(&first.to_be_bytes());
    }
    Ok(cell)
}

/// Return an overflow-page chain (starting at `first`, 0 = none) to the freelist.
fn free_overflow_chain(wp: &mut WritePager, mut first: u32) -> Result<()> {
    while first != 0 {
        let page = wp.read_page(first)?;
        let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        wp.free_page(first)?;
        first = next;
    }
    Ok(())
}

/// Write `data` across a chain of overflow pages, returning the first page.
fn write_overflow_chain(wp: &mut WritePager, data: &[u8]) -> Result<u32> {
    let cap = wp.usable_size() - 4; // bytes of payload per overflow page
                                    // Allocate all pages first so we know each page's successor.
    let n_pages = data.len().div_ceil(cap);
    let mut pages = Vec::with_capacity(n_pages);
    for _ in 0..n_pages {
        pages.push(wp.allocate_page()?);
    }
    let page_size = wp.usable_size() + wp.header().reserved_space as usize;
    for (i, &pno) in pages.iter().enumerate() {
        let next = if i + 1 < pages.len() { pages[i + 1] } else { 0 };
        let start = i * cap;
        let end = (start + cap).min(data.len());
        let mut buf = vec![0u8; page_size];
        buf[0..4].copy_from_slice(&next.to_be_bytes());
        buf[4..4 + (end - start)].copy_from_slice(&data[start..end]);
        wp.write_page(pno, buf)?;
    }
    Ok(pages[0])
}

fn insert_rec(
    wp: &mut WritePager,
    page_no: u32,
    rowid: i64,
    cell: Vec<u8>,
) -> Result<Option<Split>> {
    let page = wp.page(page_no)?;
    let body = page.body_offset();
    let bt = BtreePage::parse(page)?;
    let usable = wp.usable_size();
    let page_size = wp.usable_size() + wp.header().reserved_space as usize;

    match bt.page_type() {
        PageType::LeafTable => {
            let mut cells = read_leaf_cells(&bt, usable)?;
            match cells.binary_search_by(|(r, _)| r.cmp(&rowid)) {
                Ok(pos) => cells[pos] = (rowid, cell), // replace existing rowid
                Err(pos) => cells.insert(pos, (rowid, cell)),
            }
            let header_prefix = page_one_prefix(page_no, &bt);
            if leaf_fits(&cells, body, page_size) {
                let buf = serialize_leaf(page_size, body, &cells, header_prefix.as_deref());
                wp.write_page(page_no, buf)?;
                Ok(None)
            } else {
                let (left, right) = split_cells(cells);
                let sep = left.last().expect("non-empty left after split").0;
                let right_page = wp.allocate_page()?;
                let left_buf = serialize_leaf(page_size, body, &left, header_prefix.as_deref());
                wp.write_page(page_no, left_buf)?;
                let right_buf = serialize_leaf(page_size, 0, &right, None);
                wp.write_page(right_page, right_buf)?;
                Ok(Some(Split {
                    key: sep,
                    right_page,
                }))
            }
        }
        PageType::InteriorTable => {
            let n = bt.num_cells();
            let mut cells: Vec<(u32, i64)> = Vec::with_capacity(n);
            for i in 0..n {
                cells.push((bt.child_pointer(i)?, bt.table_interior_key(i)?));
            }
            let mut right = bt.right_pointer();

            // Descend at the first cell whose key >= rowid, else the right child.
            let mut p = n;
            let mut child = right;
            for (i, c) in cells.iter().enumerate() {
                if rowid <= c.1 {
                    p = i;
                    child = c.0;
                    break;
                }
            }

            if let Some(s) = insert_rec(wp, child, rowid, cell)? {
                if p < n {
                    let old_key = cells[p].1;
                    cells[p].1 = s.key; // left keeps its page, gets the separator
                    cells.insert(p + 1, (s.right_page, old_key));
                } else {
                    cells.push((child, s.key));
                    right = s.right_page;
                }
            }

            let header_prefix = page_one_prefix(page_no, &bt);
            if interior_fits(&cells, body, page_size) {
                let buf =
                    serialize_interior(page_size, body, &cells, right, header_prefix.as_deref());
                wp.write_page(page_no, buf)?;
                Ok(None)
            } else {
                // Promote the middle key; its child becomes the left node's right
                // pointer (standard interior split).
                let m = cells.len() / 2;
                let promote = cells[m].1;
                let left_right = cells[m].0;
                let left_cells = cells[..m].to_vec();
                let right_cells = cells[m + 1..].to_vec();
                let right_page = wp.allocate_page()?;
                let left_buf = serialize_interior(
                    page_size,
                    body,
                    &left_cells,
                    left_right,
                    header_prefix.as_deref(),
                );
                wp.write_page(page_no, left_buf)?;
                let right_buf = serialize_interior(page_size, 0, &right_cells, right, None);
                wp.write_page(right_page, right_buf)?;
                Ok(Some(Split {
                    key: promote,
                    right_page,
                }))
            }
        }
        _ => Err(Error::Corrupt("insert into a non-table b-tree".into())),
    }
}

/// Grow a new root after the old root split, keeping the root page number stable
/// (relocate the old root's content to a fresh page, make the root an interior
/// node with two children).
fn grow_root(wp: &mut WritePager, root: u32, split: Split) -> Result<()> {
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;

    let old = wp.page(root)?;
    let body = old.body_offset();
    let bt = BtreePage::parse(old)?;
    let header_prefix = page_one_prefix(root, &bt);

    // Move the old root's (left-half) content to a new page at body offset 0.
    let new_left = wp.allocate_page()?;
    let relocated = reserialize(wp, &bt, usable, page_size)?;
    wp.write_page(new_left, relocated)?;

    // The root becomes an interior node: [ (new_left, sep) ] right=split.right_page.
    let cells = [(new_left, split.key)];
    let buf = serialize_interior(
        page_size,
        body,
        &cells,
        split.right_page,
        header_prefix.as_deref(),
    );
    wp.write_page(root, buf)?;
    Ok(())
}

/// Re-serialize a page's cells at body offset 0 (used when relocating a root,
/// which may be page 1 with a 100-byte prefix that the new page must not keep).
fn reserialize(
    wp: &WritePager,
    bt: &BtreePage,
    usable: usize,
    page_size: usize,
) -> Result<Vec<u8>> {
    match bt.page_type() {
        PageType::LeafTable => {
            let cells = read_leaf_cells(bt, usable)?;
            Ok(serialize_leaf(page_size, 0, &cells, None))
        }
        PageType::InteriorTable => {
            let n = bt.num_cells();
            let mut cells = Vec::with_capacity(n);
            for i in 0..n {
                cells.push((bt.child_pointer(i)?, bt.table_interior_key(i)?));
            }
            Ok(serialize_interior(
                page_size,
                0,
                &cells,
                bt.right_pointer(),
                None,
            ))
        }
        _ => {
            let _ = wp;
            Err(Error::Corrupt("relocate of non-table page".into()))
        }
    }
}

fn read_leaf_cells(bt: &BtreePage, usable: usize) -> Result<Vec<LeafCell>> {
    let mut cells = Vec::with_capacity(bt.num_cells());
    for i in 0..bt.num_cells() {
        let rowid = bt.table_leaf_cell(i, usable)?.rowid;
        let raw = bt.raw_table_leaf_cell(i, usable)?.to_vec();
        cells.push((rowid, raw));
    }
    Ok(cells)
}

/// For page 1, return its first 100 bytes so a rewrite preserves the database
/// header region (commit later re-stamps it). For other pages, `None`.
fn page_one_prefix(page_no: u32, bt: &BtreePage) -> Option<Vec<u8>> {
    if page_no == 1 {
        Some(bt.data()[..crate::format::header::HEADER_LEN].to_vec())
    } else {
        None
    }
}

fn leaf_fits(cells: &[LeafCell], body: usize, page_size: usize) -> bool {
    let used: usize = cells.iter().map(|(_, c)| c.len() + 2).sum();
    used <= page_size - body - 8
}

fn interior_fits(cells: &[(u32, i64)], body: usize, page_size: usize) -> bool {
    let used: usize = cells.iter().map(|(_, k)| interior_cell_len(*k) + 2).sum();
    used <= page_size - body - 12
}

fn interior_cell_len(key: i64) -> usize {
    4 + varint::len(key as u64)
}

/// Split a cell list into two roughly equal-sized halves (by byte size), each
/// non-empty.
fn split_cells(cells: Vec<LeafCell>) -> (Vec<LeafCell>, Vec<LeafCell>) {
    let total: usize = cells.iter().map(|(_, c)| c.len() + 2).sum();
    let mut acc = 0;
    let mut split_at = 1;
    for (i, (_, c)) in cells.iter().enumerate() {
        acc += c.len() + 2;
        if acc * 2 >= total {
            split_at = (i + 1).min(cells.len() - 1).max(1);
            break;
        }
    }
    let mut left = cells;
    let right = left.split_off(split_at);
    (left, right)
}

fn serialize_leaf(
    page_size: usize,
    body: usize,
    cells: &[LeafCell],
    header_prefix: Option<&[u8]>,
) -> Vec<u8> {
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let mut content = page_size;
    let ptr_base = body + 8;
    for (i, (_, cell)) in cells.iter().enumerate() {
        content -= cell.len();
        buf[content..content + cell.len()].copy_from_slice(cell);
        let p = ptr_base + 2 * i;
        buf[p] = (content >> 8) as u8;
        buf[p + 1] = content as u8;
    }
    buf[body] = 0x0d;
    write_u16(&mut buf, body + 3, cells.len() as u16);
    write_cell_content_start(&mut buf, body + 5, content, page_size);
    buf
}

fn serialize_interior(
    page_size: usize,
    body: usize,
    cells: &[(u32, i64)],
    right: u32,
    header_prefix: Option<&[u8]>,
) -> Vec<u8> {
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let mut content = page_size;
    let ptr_base = body + 12;
    for (i, (child, key)) in cells.iter().enumerate() {
        let mut cell = Vec::with_capacity(interior_cell_len(*key));
        cell.extend_from_slice(&child.to_be_bytes());
        let mut vbuf = [0u8; varint::MAX_LEN];
        let n = varint::encode_i64(*key, &mut vbuf);
        cell.extend_from_slice(&vbuf[..n]);

        content -= cell.len();
        buf[content..content + cell.len()].copy_from_slice(&cell);
        let p = ptr_base + 2 * i;
        buf[p] = (content >> 8) as u8;
        buf[p + 1] = content as u8;
    }
    buf[body] = 0x05;
    write_u16(&mut buf, body + 3, cells.len() as u16);
    write_cell_content_start(&mut buf, body + 5, content, page_size);
    buf[body + 8..body + 12].copy_from_slice(&right.to_be_bytes());
    buf
}

fn write_u16(buf: &mut [u8], at: usize, v: u16) {
    buf[at] = (v >> 8) as u8;
    buf[at + 1] = v as u8;
}

fn write_cell_content_start(buf: &mut [u8], at: usize, content: usize, page_size: usize) {
    // A full page (content == page_size) with no cells stores the sentinel for
    // 65536, else the value; 65536 itself is stored as 0.
    let v = if content >= 65536 { 0 } else { content as u16 };
    write_u16(buf, at, v);
    let _ = page_size;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::TableCursor;
    use crate::format::record::{decode_record, encode_record};
    use crate::format::TextEncoding;
    use crate::value::Value;
    use crate::vfs::{memory::MemoryVfs, OpenFlags, Vfs};

    fn new_table_root(wp: &mut WritePager) -> u32 {
        // Allocate an empty leaf page to serve as a table root.
        let page_size = wp.usable_size() + wp.header().reserved_space as usize;
        let root = wp.allocate_page().unwrap();
        let buf = serialize_leaf(page_size, 0, &[], None);
        wp.write_page(root, buf).unwrap();
        root
    }

    fn wp() -> WritePager {
        let vfs = MemoryVfs::new();
        let f = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
        WritePager::create(f, None, 4096).unwrap()
    }

    #[test]
    fn insert_and_scan_back_small() {
        let mut wp = wp();
        let root = new_table_root(&mut wp);
        for i in 1..=20i64 {
            let rec = encode_record(&[Value::Null, Value::Text(alloc::format!("row{i}"))]);
            insert_table(&mut wp, root, i, &rec).unwrap();
        }
        // Scan back via the read cursor over the same WritePager.
        let mut cur = TableCursor::new(&wp, root);
        let mut seen = Vec::new();
        let mut ok = cur.first().unwrap();
        while ok {
            seen.push(cur.rowid().unwrap());
            ok = cur.next().unwrap();
        }
        assert_eq!(seen, (1..=20).collect::<Vec<_>>());
    }

    #[test]
    fn insert_many_forces_splits() {
        let mut wp = wp();
        let root = new_table_root(&mut wp);
        // Enough rows to require multiple leaves and at least one interior level.
        for i in 1..=1000i64 {
            let rec = encode_record(&[Value::Null, Value::Integer(i * 7)]);
            insert_table(&mut wp, root, i, &rec).unwrap();
        }
        let mut cur = TableCursor::new(&wp, root);
        let mut count = 0i64;
        let mut prev = 0i64;
        let mut ok = cur.first().unwrap();
        while ok {
            let rid = cur.rowid().unwrap();
            assert!(rid > prev);
            prev = rid;
            // Verify the payload decodes and the stored column matches.
            let cols = decode_record(&cur.payload().unwrap(), TextEncoding::Utf8).unwrap();
            assert_eq!(cols[1], Value::Integer(rid * 7));
            count += 1;
            ok = cur.next().unwrap();
        }
        assert_eq!(count, 1000);
    }

    #[test]
    fn insert_with_overflow_payload() {
        let mut wp = wp();
        let root = new_table_root(&mut wp);
        let big = alloc::vec![0xABu8; 10_000];
        let rec = encode_record(&[Value::Null, Value::Blob(big.clone())]);
        insert_table(&mut wp, root, 1, &rec).unwrap();
        let mut cur = TableCursor::new(&wp, root);
        assert!(cur.first().unwrap());
        let cols = decode_record(&cur.payload().unwrap(), TextEncoding::Utf8).unwrap();
        assert_eq!(cols[1], Value::Blob(big));
    }
}
