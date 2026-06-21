//! Writing to index b-trees, and freeing whole b-trees.
//!
//! Index b-trees differ from table b-trees in two ways that matter here:
//!
//! * keys are *records* (the indexed columns followed by the rowid), compared
//!   field-by-field in SQLite's value order — not 64-bit rowids; and
//! * they are true B-trees: interior cells hold real entries, so a split
//!   *promotes* the middle entry up to the parent (it no longer lives below).
//!
//! The split/grow-root mechanics otherwise mirror [`super::writer`]. We reuse
//! the whole-page-rewrite strategy: read a node into a logical entry list,
//! modify it, and re-serialize canonically.
//!
//! Deletion from a B-tree interior is intricate; instead, index maintenance on
//! `DELETE`/`UPDATE` rebuilds the affected index ([`free_tree`] + repopulate),
//! which is simple and keeps `integrity_check` happy at some cost in work.

use super::page::{payload_split, BtreePage, PageType};
use super::writer::{page_one_prefix, write_overflow_chain};
use crate::btree::cursor::read_payload;
use crate::error::{Error, Result};
use crate::format::record::decode_record;
use crate::format::TextEncoding;
use crate::pager::{PageSource, WritePager};
use crate::util::varint;
use crate::value::{cmp_values_coll, Collation, Value};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// A promoted entry bubbling up from a split: the record (for comparisons in the
/// parent's later descents), its on-page record-cell bytes, and the new right
/// sibling page.
struct IdxSplit {
    full: Vec<u8>,
    rcell: Vec<u8>,
    right_page: u32,
}

/// Find the rowids of all index entries whose leading columns equal `key`
/// (an equality-prefix lookup), descending the index b-tree in `O(height)` plus
/// the number of matches. The index record is `(indexed cols…, rowid)`, so the
/// trailing rowid is returned for each match. This is what lets the planner use
/// an index instead of a full table scan.
pub fn index_seek_rowids(
    src: &dyn PageSource,
    root: u32,
    key: &[Value],
    colls: &[Collation],
) -> Result<Vec<i64>> {
    let enc = src.header().text_encoding;
    let usable = src.usable_size();
    let mut out = Vec::new();
    seek_prefix(src, root, key, enc, usable, colls, &mut out)?;
    Ok(out)
}

/// Like [`index_seek_rowids`], but returns the full decoded *records* of the
/// matching entries rather than a trailing rowid. This is what a `WITHOUT ROWID`
/// table needs: its b-tree entries are the rows themselves (stored PK-first), so
/// an equality-prefix seek on the PK yields the rows directly.
pub fn index_seek_records(
    src: &dyn PageSource,
    root: u32,
    key: &[Value],
    colls: &[Collation],
) -> Result<Vec<Vec<Value>>> {
    let enc = src.header().text_encoding;
    let usable = src.usable_size();
    let mut out = Vec::new();
    seek_prefix_records(src, root, key, enc, usable, colls, &mut out)?;
    Ok(out)
}

/// Range variant of [`index_seek_rowids`]: return the rowids of every index
/// entry whose leading column(s) fall within the given bounds, in index order.
/// `lower`/`upper` are optional `(key_prefix, inclusive)` constraints compared
/// against the leading columns. The result may be a *superset* of the rows the
/// caller wants (callers re-apply the full `WHERE`), but never drops a matching
/// row; the in-order traversal stops as soon as the upper bound is passed.
pub fn index_range_rowids(
    src: &dyn PageSource,
    root: u32,
    lower: Option<(&[Value], bool)>,
    upper: Option<(&[Value], bool)>,
    colls: &[Collation],
) -> Result<Vec<i64>> {
    let enc = src.header().text_encoding;
    let usable = src.usable_size();
    let mut out = Vec::new();
    range_scan(src, root, lower, upper, enc, usable, colls, &mut |rec| {
        out.push(rowid_of(&rec));
    })?;
    Ok(out)
}

/// Record-returning variant of [`index_range_rowids`] — for a `WITHOUT ROWID`
/// table's clustered b-tree, whose entries are the rows. Returns every record
/// whose leading column(s) fall within the bounds, in index (PK) order.
pub fn index_range_records(
    src: &dyn PageSource,
    root: u32,
    lower: Option<(&[Value], bool)>,
    upper: Option<(&[Value], bool)>,
    colls: &[Collation],
) -> Result<Vec<Vec<Value>>> {
    let enc = src.header().text_encoding;
    let usable = src.usable_size();
    let mut out = Vec::new();
    range_scan(src, root, lower, upper, enc, usable, colls, &mut |rec| {
        out.push(rec);
    })?;
    Ok(out)
}

/// Does `rec` satisfy the optional lower bound `(key, inclusive)`?
fn passes_lower(lower: Option<(&[Value], bool)>, rec: &[Value], colls: &[Collation]) -> bool {
    match lower {
        None => true,
        Some((lk, inc)) => match prefix_cmp(lk, rec, colls) {
            Ordering::Greater => false, // lower > rec ⇒ rec < lower
            Ordering::Equal => inc,     // rec == lower
            Ordering::Less => true,     // rec > lower
        },
    }
}

/// Is `rec` past the optional upper bound `(key, inclusive)`? In ascending index
/// order, the first `true` ends the scan.
fn beyond_upper(upper: Option<(&[Value], bool)>, rec: &[Value], colls: &[Collation]) -> bool {
    match upper {
        None => false,
        Some((uk, inc)) => match prefix_cmp(uk, rec, colls) {
            Ordering::Less => true,     // upper < rec ⇒ rec > upper
            Ordering::Equal => !inc,    // rec == upper: past it only when exclusive
            Ordering::Greater => false, // rec < upper
        },
    }
}

/// In-order range traversal. Returns `Ok(false)` once the upper bound is passed
/// (so the caller stops descending further-right subtrees), else `Ok(true)`.
#[allow(clippy::too_many_arguments)]
fn range_scan(
    src: &dyn PageSource,
    page_no: u32,
    lower: Option<(&[Value], bool)>,
    upper: Option<(&[Value], bool)>,
    enc: TextEncoding,
    usable: usize,
    colls: &[Collation],
    collect: &mut dyn FnMut(Vec<Value>),
) -> Result<bool> {
    let page = src.page(page_no)?;
    let bt = BtreePage::parse(page)?;
    let record = |i: usize| -> Result<Vec<Value>> {
        let cell = bt.index_cell(i, usable)?;
        let full = read_payload(src, bt.data(), &cell.payload)?;
        decode_record(&full, enc)
    };
    match bt.page_type() {
        PageType::LeafIndex => {
            for i in 0..bt.num_cells() {
                let rec = record(i)?;
                if beyond_upper(upper, &rec, colls) {
                    return Ok(false);
                }
                if passes_lower(lower, &rec, colls) {
                    collect(rec);
                }
            }
            Ok(true)
        }
        PageType::InteriorIndex => {
            let n = bt.num_cells();
            for k in 0..n {
                // Left child of cell k, then the interior cell (itself an entry).
                if !range_scan(
                    src,
                    bt.child_pointer(k)?,
                    lower,
                    upper,
                    enc,
                    usable,
                    colls,
                    collect,
                )? {
                    return Ok(false);
                }
                let rec = record(k)?;
                if beyond_upper(upper, &rec, colls) {
                    return Ok(false);
                }
                if passes_lower(lower, &rec, colls) {
                    collect(rec);
                }
            }
            // Right-most child.
            range_scan(
                src,
                bt.child_pointer(n)?,
                lower,
                upper,
                enc,
                usable,
                colls,
                collect,
            )
        }
        _ => Err(Error::Corrupt(
            "index range scan on a non-index b-tree".into(),
        )),
    }
}

fn seek_prefix(
    src: &dyn PageSource,
    page_no: u32,
    key: &[Value],
    enc: TextEncoding,
    usable: usize,
    colls: &[Collation],
    out: &mut Vec<i64>,
) -> Result<()> {
    let page = src.page(page_no)?;
    let bt = BtreePage::parse(page)?;
    let record = |i: usize| -> Result<Vec<Value>> {
        let cell = bt.index_cell(i, usable)?;
        let full = read_payload(src, bt.data(), &cell.payload)?;
        decode_record(&full, enc)
    };
    match bt.page_type() {
        PageType::LeafIndex => {
            for i in 0..bt.num_cells() {
                let rec = record(i)?;
                match prefix_cmp(key, &rec, colls) {
                    Ordering::Greater => continue,
                    Ordering::Equal => out.push(rowid_of(&rec)),
                    Ordering::Less => break, // sorted: no further matches on this leaf
                }
            }
            Ok(())
        }
        PageType::InteriorIndex => {
            let n = bt.num_cells();
            let mut i = 0;
            // Skip cells strictly less than the key.
            while i < n && prefix_cmp(key, &record(i)?, colls) == Ordering::Greater {
                i += 1;
            }
            // Matches < cell[i] live in its left child.
            seek_prefix(src, bt.child_pointer(i)?, key, enc, usable, colls, out)?;
            // Equal interior cells are themselves matches; descend the child to
            // their right for further matches.
            while i < n && prefix_cmp(key, &record(i)?, colls) == Ordering::Equal {
                out.push(rowid_of(&record(i)?));
                seek_prefix(src, bt.child_pointer(i + 1)?, key, enc, usable, colls, out)?;
                i += 1;
            }
            Ok(())
        }
        _ => Err(Error::Corrupt("index seek on a non-index b-tree".into())),
    }
}

/// As [`seek_prefix`], but collects the full matching records instead of their
/// trailing rowid — for `WITHOUT ROWID` table seeks.
fn seek_prefix_records(
    src: &dyn PageSource,
    page_no: u32,
    key: &[Value],
    enc: TextEncoding,
    usable: usize,
    colls: &[Collation],
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let page = src.page(page_no)?;
    let bt = BtreePage::parse(page)?;
    let record = |i: usize| -> Result<Vec<Value>> {
        let cell = bt.index_cell(i, usable)?;
        let full = read_payload(src, bt.data(), &cell.payload)?;
        decode_record(&full, enc)
    };
    match bt.page_type() {
        PageType::LeafIndex => {
            for i in 0..bt.num_cells() {
                let rec = record(i)?;
                match prefix_cmp(key, &rec, colls) {
                    Ordering::Greater => continue,
                    Ordering::Equal => out.push(rec),
                    Ordering::Less => break,
                }
            }
            Ok(())
        }
        PageType::InteriorIndex => {
            let n = bt.num_cells();
            let mut i = 0;
            while i < n && prefix_cmp(key, &record(i)?, colls) == Ordering::Greater {
                i += 1;
            }
            seek_prefix_records(src, bt.child_pointer(i)?, key, enc, usable, colls, out)?;
            while i < n && prefix_cmp(key, &record(i)?, colls) == Ordering::Equal {
                out.push(record(i)?);
                seek_prefix_records(src, bt.child_pointer(i + 1)?, key, enc, usable, colls, out)?;
                i += 1;
            }
            Ok(())
        }
        _ => Err(Error::Corrupt("index seek on a non-index b-tree".into())),
    }
}

/// Compare a key against the leading columns of an index record.
fn prefix_cmp(key: &[Value], rec: &[Value], colls: &[Collation]) -> Ordering {
    for (i, (k, r)) in key.iter().zip(rec.iter()).enumerate() {
        let c = colls.get(i).copied().unwrap_or_default();
        let o = cmp_values_coll(k, r, c);
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// The trailing rowid of an index record.
fn rowid_of(rec: &[Value]) -> i64 {
    match rec.last() {
        Some(Value::Integer(i)) => *i,
        _ => 0,
    }
}

/// Allocate an empty index b-tree (a single leaf page) and return its root.
pub fn create_index_root(wp: &mut WritePager) -> Result<u32> {
    let page_size = wp.usable_size() + wp.header().reserved_space as usize;
    let root = wp.allocate_page()?;
    let buf = serialize_index_leaf(page_size, 0, &[], None);
    wp.write_page(root, buf)?;
    Ok(root)
}

/// Insert an index key `record` (indexed columns + trailing rowid) into the
/// index b-tree at `root`.
pub fn insert_index(
    wp: &mut WritePager,
    root: u32,
    record: &[u8],
    colls: &[Collation],
) -> Result<()> {
    let rcell = build_index_rcell(wp, record)?;
    if let Some(split) = insert_rec(wp, root, record, rcell, colls)? {
        grow_root(wp, root, split)?;
    }
    Ok(())
}

/// Recursively free every page of the b-tree at `root` (interior, leaf, and all
/// overflow chains), returning them to the freelist. Works for table and index
/// trees alike — used by index rebuild and `DROP`.
pub fn free_tree(wp: &mut WritePager, root: u32) -> Result<()> {
    let usable = wp.usable_size();
    let page = wp.page(root)?;
    let bt = BtreePage::parse(page)?;
    match bt.page_type() {
        PageType::LeafTable => {
            for i in 0..bt.num_cells() {
                let ov = bt.table_leaf_cell(i, usable)?.payload.overflow;
                free_chain(wp, ov)?;
            }
        }
        PageType::LeafIndex => {
            for i in 0..bt.num_cells() {
                let ov = bt.index_cell(i, usable)?.payload.overflow;
                free_chain(wp, ov)?;
            }
        }
        PageType::InteriorTable | PageType::InteriorIndex => {
            let n = bt.num_cells();
            for i in 0..n {
                if bt.page_type() == PageType::InteriorIndex {
                    let ov = bt.index_cell(i, usable)?.payload.overflow;
                    free_chain(wp, ov)?;
                }
                free_tree(wp, bt.child_pointer(i)?)?;
            }
            free_tree(wp, bt.right_pointer())?;
        }
    }
    wp.free_page(root)
}

/// Empty an index b-tree while keeping its root page number stable: free every
/// descendant page (and all overflow chains) and reset the root to an empty leaf.
/// Used to rebuild an index in place on `DELETE`/`UPDATE` without having to
/// update the index's `rootpage` in `sqlite_schema`.
pub fn clear_index(wp: &mut WritePager, root: u32) -> Result<()> {
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    let bt = BtreePage::parse(wp.page(root)?)?;
    match bt.page_type() {
        PageType::LeafIndex => {
            for i in 0..bt.num_cells() {
                free_chain(wp, bt.index_cell(i, usable)?.payload.overflow)?;
            }
        }
        PageType::InteriorIndex => {
            let n = bt.num_cells();
            for i in 0..n {
                free_chain(wp, bt.index_cell(i, usable)?.payload.overflow)?;
                free_tree(wp, bt.child_pointer(i)?)?;
            }
            free_tree(wp, bt.right_pointer())?;
        }
        _ => return Err(Error::Corrupt("clear of a non-index b-tree".into())),
    }
    let empty = serialize_index_leaf(page_size, 0, &[], None);
    wp.write_page(root, empty)?;
    Ok(())
}

fn free_chain(wp: &mut WritePager, mut first: u32) -> Result<()> {
    while first != 0 {
        let page = wp.read_page(first)?;
        let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        wp.free_page(first)?;
        first = next;
    }
    Ok(())
}

fn build_index_rcell(wp: &mut WritePager, record: &[u8]) -> Result<Vec<u8>> {
    let usable = wp.usable_size();
    let (local, has_overflow) = payload_split(PageType::LeafIndex, usable, record.len());
    let mut cell = Vec::new();
    let mut vbuf = [0u8; varint::MAX_LEN];
    let n = varint::encode(record.len() as u64, &mut vbuf);
    cell.extend_from_slice(&vbuf[..n]);
    cell.extend_from_slice(&record[..local]);
    if has_overflow {
        let first = write_overflow_chain(wp, &record[local..])?;
        cell.extend_from_slice(&first.to_be_bytes());
    }
    Ok(cell)
}

type LeafEntry = (Vec<u8>, Vec<u8>); // (full record, record-cell bytes)
type InteriorEntry = (u32, Vec<u8>, Vec<u8>); // (left child, full record, record-cell bytes)

fn insert_rec(
    wp: &mut WritePager,
    page_no: u32,
    target: &[u8],
    rcell: Vec<u8>,
    colls: &[Collation],
) -> Result<Option<IdxSplit>> {
    let enc = wp.header().text_encoding;
    let page = wp.page(page_no)?;
    let body = page.body_offset();
    let bt = BtreePage::parse(page)?;
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;

    match bt.page_type() {
        PageType::LeafIndex => {
            let mut entries = read_leaf(wp, &bt, usable)?;
            let mut pos = entries.len();
            for (i, (full, _)) in entries.iter().enumerate() {
                match cmp_records(target, full, enc, colls)? {
                    Ordering::Less => {
                        pos = i;
                        break;
                    }
                    Ordering::Equal => return Ok(None), // already present (unique w/ rowid)
                    Ordering::Greater => {}
                }
            }
            entries.insert(pos, (target.to_vec(), rcell));
            let prefix = page_one_prefix(page_no, &bt);
            if leaf_fits(&entries, body, page_size) {
                let buf =
                    serialize_index_leaf(page_size, body, &rcells(&entries), prefix.as_deref());
                wp.write_page(page_no, buf)?;
                Ok(None)
            } else {
                let m = entries.len() / 2;
                let promoted = entries[m].clone();
                let left = &entries[..m];
                let right = &entries[m + 1..];
                let right_page = wp.allocate_page()?;
                let lbuf = serialize_index_leaf(page_size, body, &rcells(left), prefix.as_deref());
                wp.write_page(page_no, lbuf)?;
                let rbuf = serialize_index_leaf(page_size, 0, &rcells(right), None);
                wp.write_page(right_page, rbuf)?;
                Ok(Some(IdxSplit {
                    full: promoted.0,
                    rcell: promoted.1,
                    right_page,
                }))
            }
        }
        PageType::InteriorIndex => {
            let (mut cells, mut right) = read_interior(wp, &bt, usable)?;
            let mut p = cells.len();
            let mut child = right;
            for (i, (c, full, _)) in cells.iter().enumerate() {
                match cmp_records(target, full, enc, colls)? {
                    Ordering::Less => {
                        p = i;
                        child = *c;
                        break;
                    }
                    Ordering::Equal => return Ok(None),
                    Ordering::Greater => {}
                }
            }
            if let Some(s) = insert_rec(wp, child, target, rcell, colls)? {
                if p < cells.len() {
                    let old = cells[p].clone();
                    cells[p] = (old.0, s.full, s.rcell);
                    cells.insert(p + 1, (s.right_page, old.1, old.2));
                } else {
                    cells.push((child, s.full, s.rcell));
                    right = s.right_page;
                }
            }
            let prefix = page_one_prefix(page_no, &bt);
            if interior_fits(&cells, body, page_size) {
                let buf =
                    serialize_index_interior(page_size, body, &cells, right, prefix.as_deref());
                wp.write_page(page_no, buf)?;
                Ok(None)
            } else {
                let m = cells.len() / 2;
                let promoted = cells[m].clone();
                let left_right = promoted.0;
                let left = cells[..m].to_vec();
                let right_cells = cells[m + 1..].to_vec();
                let right_page = wp.allocate_page()?;
                let lbuf =
                    serialize_index_interior(page_size, body, &left, left_right, prefix.as_deref());
                wp.write_page(page_no, lbuf)?;
                let rbuf = serialize_index_interior(page_size, 0, &right_cells, right, None);
                wp.write_page(right_page, rbuf)?;
                Ok(Some(IdxSplit {
                    full: promoted.1,
                    rcell: promoted.2,
                    right_page,
                }))
            }
        }
        _ => Err(Error::Corrupt("insert into a non-index b-tree".into())),
    }
}

fn grow_root(wp: &mut WritePager, root: u32, split: IdxSplit) -> Result<()> {
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    // Index roots are never page 1, so the left half can be relocated by a raw
    // copy of the (already body-0) page bytes.
    let left_bytes = wp.read_page(root)?;
    let new_left = wp.allocate_page()?;
    wp.write_page(new_left, left_bytes)?;
    let cells = [(new_left, split.full, split.rcell)];
    let buf = serialize_index_interior(page_size, 0, &cells, split.right_page, None);
    wp.write_page(root, buf)?;
    Ok(())
}

fn read_leaf(wp: &WritePager, bt: &BtreePage, usable: usize) -> Result<Vec<LeafEntry>> {
    let mut out = Vec::with_capacity(bt.num_cells());
    for i in 0..bt.num_cells() {
        let cell = bt.index_cell(i, usable)?;
        let full = read_payload(wp, bt.data(), &cell.payload)?;
        let rcell = bt.raw_index_record_cell(i, usable)?.to_vec();
        out.push((full, rcell));
    }
    Ok(out)
}

fn read_interior(
    wp: &WritePager,
    bt: &BtreePage,
    usable: usize,
) -> Result<(Vec<InteriorEntry>, u32)> {
    let mut out = Vec::with_capacity(bt.num_cells());
    for i in 0..bt.num_cells() {
        let cell = bt.index_cell(i, usable)?;
        let full = read_payload(wp, bt.data(), &cell.payload)?;
        let rcell = bt.raw_index_record_cell(i, usable)?.to_vec();
        out.push((cell.left_child, full, rcell));
    }
    Ok((out, bt.right_pointer()))
}

fn rcells(entries: &[LeafEntry]) -> Vec<Vec<u8>> {
    entries.iter().map(|(_, c)| c.clone()).collect()
}

fn cmp_records(a: &[u8], b: &[u8], enc: TextEncoding, colls: &[Collation]) -> Result<Ordering> {
    let va = decode_record(a, enc)?;
    let vb = decode_record(b, enc)?;
    for (i, (x, y)) in va.iter().zip(vb.iter()).enumerate() {
        let c = colls.get(i).copied().unwrap_or_default();
        let o = cmp_values_coll(x, y, c);
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(va.len().cmp(&vb.len()))
}

fn leaf_fits(entries: &[LeafEntry], body: usize, page_size: usize) -> bool {
    let used: usize = entries.iter().map(|(_, c)| c.len() + 2).sum();
    used <= page_size - body - 8
}

fn interior_fits(cells: &[InteriorEntry], body: usize, page_size: usize) -> bool {
    let used: usize = cells.iter().map(|(_, _, c)| 4 + c.len() + 2).sum();
    used <= page_size - body - 12
}

fn serialize_index_leaf(
    page_size: usize,
    body: usize,
    rcells: &[Vec<u8>],
    header_prefix: Option<&[u8]>,
) -> Vec<u8> {
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let mut content = page_size;
    let ptr_base = body + 8;
    for (i, cell) in rcells.iter().enumerate() {
        content -= cell.len();
        buf[content..content + cell.len()].copy_from_slice(cell);
        let p = ptr_base + 2 * i;
        buf[p] = (content >> 8) as u8;
        buf[p + 1] = content as u8;
    }
    buf[body] = 0x0a; // leaf index
    put16(&mut buf, body + 3, rcells.len() as u16);
    put_ccs(&mut buf, body + 5, content);
    buf
}

fn serialize_index_interior(
    page_size: usize,
    body: usize,
    cells: &[InteriorEntry],
    right: u32,
    header_prefix: Option<&[u8]>,
) -> Vec<u8> {
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let mut content = page_size;
    let ptr_base = body + 12;
    for (i, (child, _, rcell)) in cells.iter().enumerate() {
        let mut cell = Vec::with_capacity(4 + rcell.len());
        cell.extend_from_slice(&child.to_be_bytes());
        cell.extend_from_slice(rcell);
        content -= cell.len();
        buf[content..content + cell.len()].copy_from_slice(&cell);
        let p = ptr_base + 2 * i;
        buf[p] = (content >> 8) as u8;
        buf[p + 1] = content as u8;
    }
    buf[body] = 0x02; // interior index
    put16(&mut buf, body + 3, cells.len() as u16);
    put_ccs(&mut buf, body + 5, content);
    buf[body + 8..body + 12].copy_from_slice(&right.to_be_bytes());
    buf
}

fn put16(buf: &mut [u8], at: usize, v: u16) {
    buf[at] = (v >> 8) as u8;
    buf[at + 1] = v as u8;
}

fn put_ccs(buf: &mut [u8], at: usize, content: usize) {
    let v = if content >= 65536 { 0 } else { content as u16 };
    put16(buf, at, v);
}
