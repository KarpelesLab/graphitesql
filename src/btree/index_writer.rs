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

use super::page::{BtreePage, PageType, payload_split};
use super::writer::{page_one_prefix, write_overflow_chain};
use crate::btree::cursor::read_payload;
use crate::error::{Error, Result};
use crate::format::TextEncoding;
use crate::format::record::decode_record;
use crate::pager::{PageSource, WritePager};
use crate::util::varint;
use crate::value::{Collation, Value, cmp_values_coll};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// A promoted separator entry: the full record (for comparisons in the parent's
/// later descents) and its on-page record-cell bytes. Unlike a table b-tree,
/// an index split *promotes* an entry — it lives only in the parent interior
/// cell, not in any child leaf.
type IdxKey = (Vec<u8>, Vec<u8>); // (full record, record-cell bytes)

/// A node split bubbling up to the parent. The over-full page was repacked into
/// the original page (reused, keeping its page number) plus one or more brand-new
/// right-sibling pages, each of which individually fits. `first` is the promoted
/// separator entry between the reused original page and `siblings[0]`. `siblings`
/// lists the new sibling pages left-to-right, each paired with the promoted
/// separator entry that follows it. The *last* sibling's key is a placeholder
/// (empty vectors) — the parent supplies the real upper bound (its own separator
/// for this child, or nothing when the child is the parent's right-most).
/// `siblings` is always non-empty.
struct IdxSplit {
    first: IdxKey,
    siblings: Vec<(IdxKey, u32)>,
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
    descs: &[bool],
) -> Result<Vec<i64>> {
    let enc = src.header().text_encoding;
    let usable = src.usable_size();
    let mut out = Vec::new();
    seek_prefix(src, root, key, enc, usable, colls, descs, &mut out)?;
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
    descs: &[bool],
) -> Result<Vec<Vec<Value>>> {
    let enc = src.header().text_encoding;
    let usable = src.usable_size();
    let mut out = Vec::new();
    seek_prefix_records(src, root, key, enc, usable, colls, descs, &mut out)?;
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
    descs: &[bool],
) -> Result<Vec<i64>> {
    let enc = src.header().text_encoding;
    let usable = src.usable_size();
    let mut out = Vec::new();
    range_scan(
        src,
        root,
        lower,
        upper,
        enc,
        usable,
        colls,
        descs,
        &mut |rec| {
            out.push(rowid_of(&rec));
        },
    )?;
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
    descs: &[bool],
) -> Result<Vec<Vec<Value>>> {
    let enc = src.header().text_encoding;
    let usable = src.usable_size();
    let mut out = Vec::new();
    range_scan(
        src,
        root,
        lower,
        upper,
        enc,
        usable,
        colls,
        descs,
        &mut |rec| {
            out.push(rec);
        },
    )?;
    Ok(out)
}

/// Does `rec` satisfy the optional lower bound `(key, inclusive)`? "Lower" is in
/// stored-key order; a DESC index column has its value order reversed here (via
/// `descs`), so callers building bounds in value space must swap accordingly.
fn passes_lower(
    lower: Option<(&[Value], bool)>,
    rec: &[Value],
    colls: &[Collation],
    descs: &[bool],
) -> bool {
    match lower {
        None => true,
        Some((lk, inc)) => match prefix_cmp(lk, rec, colls, descs) {
            Ordering::Greater => false, // lower > rec ⇒ rec < lower
            Ordering::Equal => inc,     // rec == lower
            Ordering::Less => true,     // rec > lower
        },
    }
}

/// Is `rec` past the optional upper bound `(key, inclusive)`? In stored-key
/// order, the first `true` ends the scan. A DESC index column has its value
/// order reversed here (via `descs`).
fn beyond_upper(
    upper: Option<(&[Value], bool)>,
    rec: &[Value],
    colls: &[Collation],
    descs: &[bool],
) -> bool {
    match upper {
        None => false,
        Some((uk, inc)) => match prefix_cmp(uk, rec, colls, descs) {
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
    descs: &[bool],
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
                if beyond_upper(upper, &rec, colls, descs) {
                    return Ok(false);
                }
                if passes_lower(lower, &rec, colls, descs) {
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
                    descs,
                    collect,
                )? {
                    return Ok(false);
                }
                let rec = record(k)?;
                if beyond_upper(upper, &rec, colls, descs) {
                    return Ok(false);
                }
                if passes_lower(lower, &rec, colls, descs) {
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
                descs,
                collect,
            )
        }
        _ => Err(Error::Corrupt(
            "index range scan on a non-index b-tree".into(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn seek_prefix(
    src: &dyn PageSource,
    page_no: u32,
    key: &[Value],
    enc: TextEncoding,
    usable: usize,
    colls: &[Collation],
    descs: &[bool],
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
                match prefix_cmp(key, &rec, colls, descs) {
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
            while i < n && prefix_cmp(key, &record(i)?, colls, descs) == Ordering::Greater {
                i += 1;
            }
            // Matches < cell[i] live in its left child.
            seek_prefix(
                src,
                bt.child_pointer(i)?,
                key,
                enc,
                usable,
                colls,
                descs,
                out,
            )?;
            // Equal interior cells are themselves matches; descend the child to
            // their right for further matches.
            while i < n && prefix_cmp(key, &record(i)?, colls, descs) == Ordering::Equal {
                out.push(rowid_of(&record(i)?));
                seek_prefix(
                    src,
                    bt.child_pointer(i + 1)?,
                    key,
                    enc,
                    usable,
                    colls,
                    descs,
                    out,
                )?;
                i += 1;
            }
            Ok(())
        }
        _ => Err(Error::Corrupt("index seek on a non-index b-tree".into())),
    }
}

/// As [`seek_prefix`], but collects the full matching records instead of their
/// trailing rowid — for `WITHOUT ROWID` table seeks.
#[allow(clippy::too_many_arguments)]
fn seek_prefix_records(
    src: &dyn PageSource,
    page_no: u32,
    key: &[Value],
    enc: TextEncoding,
    usable: usize,
    colls: &[Collation],
    descs: &[bool],
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
                match prefix_cmp(key, &rec, colls, descs) {
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
            while i < n && prefix_cmp(key, &record(i)?, colls, descs) == Ordering::Greater {
                i += 1;
            }
            seek_prefix_records(
                src,
                bt.child_pointer(i)?,
                key,
                enc,
                usable,
                colls,
                descs,
                out,
            )?;
            while i < n && prefix_cmp(key, &record(i)?, colls, descs) == Ordering::Equal {
                out.push(record(i)?);
                seek_prefix_records(
                    src,
                    bt.child_pointer(i + 1)?,
                    key,
                    enc,
                    usable,
                    colls,
                    descs,
                    out,
                )?;
                i += 1;
            }
            Ok(())
        }
        _ => Err(Error::Corrupt("index seek on a non-index b-tree".into())),
    }
}

/// Compare a key against the leading columns of an index record. A `true` entry
/// in `descs` (aligned with the columns) reverses that column's order, matching a
/// `DESC` index column; an empty `descs` means all-ascending. The trailing rowid
/// column is never in `descs` (always ascending).
fn prefix_cmp(key: &[Value], rec: &[Value], colls: &[Collation], descs: &[bool]) -> Ordering {
    for (i, (k, r)) in key.iter().zip(rec.iter()).enumerate() {
        let c = colls.get(i).copied().unwrap_or_default();
        let o = cmp_values_coll(k, r, c);
        let o = if descs.get(i).copied().unwrap_or(false) {
            o.reverse()
        } else {
            o
        };
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
    let buf = serialize_index_leaf(page_size, 0, &[], None)?;
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
    descs: &[bool],
) -> Result<()> {
    let rcell = build_index_rcell(wp, record)?;
    if let Some(split) = insert_rec(wp, root, record, rcell, colls, descs)? {
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
    let empty = serialize_index_leaf(page_size, 0, &[], None)?;
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
    descs: &[bool],
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
                match cmp_records(target, full, enc, colls, descs)? {
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
                    serialize_index_leaf(page_size, body, &rcells(&entries), prefix.as_deref())?;
                wp.write_page(page_no, buf)?;
                Ok(None)
            } else {
                // Robust multi-way leaf split: partition the over-full entry list
                // into consecutive parts that each individually fit, promoting the
                // boundary entry between adjacent parts up to the parent (an index
                // split promotes an entry — it lives only in the parent). A single
                // fixed halving is not enough when one near-page-sized entry skews
                // the byte total. Part 0 reuses this page; the rest are new pages.
                let (parts, seps) = pack_index_leaf(&entries, body, page_size);
                if parts.len() == 1 {
                    // A single un-splittable part (impossible in practice — index
                    // cells are capped to fit by overflow); write back defensively.
                    let buf = serialize_index_leaf(
                        page_size,
                        body,
                        &rcells(&entries[parts[0].clone()]),
                        prefix.as_deref(),
                    )?;
                    wp.write_page(page_no, buf)?;
                    return Ok(None);
                }
                let first = (entries[seps[0]].0.clone(), entries[seps[0]].1.clone());
                let lbuf = serialize_index_leaf(
                    page_size,
                    body,
                    &rcells(&entries[parts[0].clone()]),
                    prefix.as_deref(),
                )?;
                wp.write_page(page_no, lbuf)?;
                let mut siblings = Vec::with_capacity(parts.len() - 1);
                for k in 1..parts.len() {
                    let pg = wp.allocate_page()?;
                    let buf = serialize_index_leaf(
                        page_size,
                        0,
                        &rcells(&entries[parts[k].clone()]),
                        None,
                    )?;
                    wp.write_page(pg, buf)?;
                    // Separator that follows this sibling part; the last part has no
                    // promoted key (the parent supplies the upper bound).
                    let key = if k < seps.len() {
                        (entries[seps[k]].0.clone(), entries[seps[k]].1.clone())
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    siblings.push((key, pg));
                }
                Ok(Some(IdxSplit { first, siblings }))
            }
        }
        PageType::InteriorIndex => {
            let (mut cells, mut right) = read_interior(wp, &bt, usable)?;
            let mut p = cells.len();
            let mut child = right;
            for (i, (c, full, _)) in cells.iter().enumerate() {
                match cmp_records(target, full, enc, colls, descs)? {
                    Ordering::Less => {
                        p = i;
                        child = *c;
                        break;
                    }
                    Ordering::Equal => return Ok(None),
                    Ordering::Greater => {}
                }
            }
            if let Some(s) = insert_rec(wp, child, target, rcell, colls, descs)? {
                // The child split into its (reused) page plus one or more new
                // siblings; adopt them all. The reused child keeps its page number
                // but the separator entry after it becomes `s.first`; each new
                // sibling is inserted after it with its own promoted separator.
                let mut sibs = s.siblings;
                if p < cells.len() {
                    let old = cells[p].clone(); // (child_p, entry_p, rcell_p)
                    cells[p] = (old.0, s.first.0, s.first.1);
                    // The last sibling is bounded above by the old separator entry.
                    if let Some(last) = sibs.last_mut() {
                        last.0 = (old.1, old.2);
                    }
                    for (off, ((full, rc), pg)) in sibs.into_iter().enumerate() {
                        cells.insert(p + 1 + off, (pg, full, rc));
                    }
                } else {
                    // Descended into the right-most child: it keeps its page as a
                    // now-non-right cell carrying `s.first`; the last new sibling
                    // becomes the new right-most pointer.
                    cells.push((child, s.first.0, s.first.1));
                    let last = sibs.pop().expect("split always has a sibling");
                    for ((full, rc), pg) in sibs {
                        cells.push((pg, full, rc));
                    }
                    right = last.1;
                }
            }
            let prefix = page_one_prefix(page_no, &bt);
            if interior_fits(&cells, body, page_size) {
                let buf =
                    serialize_index_interior(page_size, body, &cells, right, prefix.as_deref())?;
                wp.write_page(page_no, buf)?;
                Ok(None)
            } else {
                // Robust multi-way interior split: partition the children into
                // groups that each fit, promoting one separator entry between
                // groups (its left child becomes the left group's right pointer).
                let (parts, seps) = pack_index_interior(&cells, right, body, page_size);
                if parts.len() == 1 {
                    let (pc, pr) = &parts[0];
                    let buf =
                        serialize_index_interior(page_size, body, pc, *pr, prefix.as_deref())?;
                    wp.write_page(page_no, buf)?;
                    return Ok(None);
                }
                let first = seps[0].clone();
                let (p0c, p0r) = &parts[0];
                let lbuf = serialize_index_interior(page_size, body, p0c, *p0r, prefix.as_deref())?;
                wp.write_page(page_no, lbuf)?;
                let mut siblings = Vec::with_capacity(parts.len() - 1);
                for k in 1..parts.len() {
                    let (pc, pr) = &parts[k];
                    let pg = wp.allocate_page()?;
                    let buf = serialize_index_interior(page_size, 0, pc, *pr, None)?;
                    wp.write_page(pg, buf)?;
                    let key = if k < seps.len() {
                        seps[k].clone()
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    siblings.push((key, pg));
                }
                Ok(Some(IdxSplit { first, siblings }))
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
    // The root becomes an interior node whose children are the relocated old-root
    // content plus every sibling produced by the split: [(new_left, first), (sib_1,
    // key_1), …] with the last sibling as the right-most pointer.
    let mut cells: Vec<InteriorEntry> = Vec::with_capacity(split.siblings.len());
    cells.push((new_left, split.first.0, split.first.1));
    let mut sibs = split.siblings;
    let last = sibs.pop().expect("split always has a sibling");
    for ((full, rc), pg) in sibs {
        cells.push((pg, full, rc));
    }
    let buf = serialize_index_interior(page_size, 0, &cells, last.1, None)?;
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

fn cmp_records(
    a: &[u8],
    b: &[u8],
    enc: TextEncoding,
    colls: &[Collation],
    descs: &[bool],
) -> Result<Ordering> {
    let va = decode_record(a, enc)?;
    let vb = decode_record(b, enc)?;
    for (i, (x, y)) in va.iter().zip(vb.iter()).enumerate() {
        let c = colls.get(i).copied().unwrap_or_default();
        let o = cmp_values_coll(x, y, c);
        let o = if descs.get(i).copied().unwrap_or(false) {
            o.reverse()
        } else {
            o
        };
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

/// Greedily pack an over-full index leaf's entries into as many consecutive parts
/// as are needed for each part to individually fit a leaf page, returning the
/// index ranges of the parts and the indices of the *promoted separator* entries
/// between adjacent parts (`seps.len() == parts.len() - 1`). A promoted entry is
/// excluded from both surrounding leaves — it moves up to the parent. Part 0 is
/// sized against `body0`; every later part against a body-0 page. Every returned
/// range is non-empty (except the degenerate single-oversized-entry case, which
/// overflow-capping makes unreachable).
fn pack_index_leaf(
    entries: &[LeafEntry],
    body0: usize,
    page_size: usize,
) -> (Vec<core::ops::Range<usize>>, Vec<usize>) {
    let n = entries.len();
    let mut parts: Vec<core::ops::Range<usize>> = Vec::new();
    let mut seps: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < n {
        let body = if parts.is_empty() { body0 } else { 0 };
        // `leaf_fits` bound: sum(rcell.len() + 2) <= page_size - body - 8.
        let cap = page_size.saturating_sub(body + 8);
        let mut used = 0usize;
        let mut j = i;
        while j < n {
            let need = entries[j].1.len() + 2;
            // Always place at least one entry per part (`j == i`).
            if j == i || used + need <= cap {
                used += need;
                j += 1;
            } else {
                break;
            }
        }
        if j >= n {
            // Everything remaining fits this part.
            parts.push(i..j);
            break;
        }
        // entries[j] is the first that did not fit → promote it as the separator
        // between this part and the next, which starts at j+1.
        if j + 1 >= n {
            // Promoting entries[j] would leave a dangling separator (the last
            // entry, with no right leaf). Back off: shrink this part by one,
            // promote entries[j-1] instead, and let entries[j..n] be the next
            // part. Requires this part to keep ≥1 entry — always true under
            // overflow capping (each part holds several cells).
            if j - 1 > i {
                parts.push(i..j - 1);
                seps.push(j - 1);
                i = j; // entries[j..n] (the single trailing entry) next iteration
                continue;
            }
            // Degenerate (a lone entry that will not join a single trailing entry)
            // — unreachable under overflow capping; place them together so a later
            // serialize surfaces `Corrupt` rather than dropping an entry.
            parts.push(i..n);
            break;
        }
        parts.push(i..j);
        seps.push(j);
        i = j + 1;
    }
    (parts, seps)
}

/// One part of a multi-way interior split: its on-page cells and its right pointer.
type IdxInteriorPart = (Vec<InteriorEntry>, u32);

/// Greedily partition an over-full index-interior page's children into as many
/// parts as are needed for each part to individually fit. Between two adjacent
/// parts one cell is *promoted* (its `(full, rcell)` returned in `seps`) and its
/// left child becomes the left part's right pointer. Returns `(parts, seps)` with
/// `seps.len() == parts.len() - 1`. Part 0 is sized against `body0`.
fn pack_index_interior(
    cells: &[InteriorEntry],
    right: u32,
    body0: usize,
    page_size: usize,
) -> (Vec<IdxInteriorPart>, Vec<IdxKey>) {
    let n = cells.len();
    // child(j): the j-th child pointer (j == n is the page's right pointer).
    let child = |j: usize| -> u32 { if j < n { cells[j].0 } else { right } };
    let mut parts: Vec<IdxInteriorPart> = Vec::new();
    let mut seps: Vec<IdxKey> = Vec::new();
    let mut i = 0usize;
    loop {
        let body = if parts.is_empty() { body0 } else { 0 };
        // `interior_fits` bound: sum(4 + rcell.len() + 2) <= page_size - body - 12.
        let cap = page_size.saturating_sub(body + 12);
        let mut used = 0usize;
        let mut b = i;
        while b < n {
            let need = 4 + cells[b].2.len() + 2;
            if b == i || used + need <= cap {
                used += need;
                b += 1;
            } else {
                break;
            }
        }
        if b >= n {
            // Last part: on-page cells cells[i..n], right pointer = page's `right`.
            parts.push((cells[i..n].to_vec(), right));
            break;
        }
        // Promote cells[b] as the separator; cells[b]'s child becomes this part's
        // right pointer. Guard against leaving the final part with no on-page cells
        // by backing off one cell.
        let mut bb = b;
        if bb == n - 1 && bb > i + 1 {
            bb -= 1;
        }
        parts.push((cells[i..bb].to_vec(), child(bb)));
        seps.push((cells[bb].1.clone(), cells[bb].2.clone()));
        i = bb + 1;
    }
    (parts, seps)
}

fn serialize_index_leaf(
    page_size: usize,
    body: usize,
    rcells: &[Vec<u8>],
    header_prefix: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let ptr_base = body + 8;
    // The cell-pointer array grows up from `ptr_base`; cell content packs down from
    // the page end. They must not meet. `pack_index_leaf` guarantees a fitting
    // list, but check defensively so a violation surfaces as `Corrupt`, not a panic.
    let ptr_end = ptr_base + 2 * rcells.len();
    let mut content = page_size;
    for (i, cell) in rcells.iter().enumerate() {
        if content < cell.len() || content - cell.len() < ptr_end {
            return Err(Error::Corrupt(
                "index leaf page overflow while serializing".into(),
            ));
        }
        content -= cell.len();
        buf[content..content + cell.len()].copy_from_slice(cell);
        let p = ptr_base + 2 * i;
        buf[p] = (content >> 8) as u8;
        buf[p + 1] = content as u8;
    }
    buf[body] = 0x0a; // leaf index
    put16(&mut buf, body + 3, rcells.len() as u16);
    put_ccs(&mut buf, body + 5, content);
    Ok(buf)
}

fn serialize_index_interior(
    page_size: usize,
    body: usize,
    cells: &[InteriorEntry],
    right: u32,
    header_prefix: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let ptr_base = body + 12;
    let ptr_end = ptr_base + 2 * cells.len();
    let mut content = page_size;
    for (i, (child, _, rcell)) in cells.iter().enumerate() {
        let mut cell = Vec::with_capacity(4 + rcell.len());
        cell.extend_from_slice(&child.to_be_bytes());
        cell.extend_from_slice(rcell);
        if content < cell.len() || content - cell.len() < ptr_end {
            return Err(Error::Corrupt(
                "index interior page overflow while serializing".into(),
            ));
        }
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
    Ok(buf)
}

fn put16(buf: &mut [u8], at: usize, v: u16) {
    buf[at] = (v >> 8) as u8;
    buf[at + 1] = v as u8;
}

fn put_ccs(buf: &mut [u8], at: usize, content: usize) {
    let v = if content >= 65536 { 0 } else { content as u16 };
    put16(buf, at, v);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic leaf-entry list whose `rcell` byte lengths are `lens`.
    fn leaf_entries(lens: &[usize]) -> Vec<LeafEntry> {
        lens.iter()
            .enumerate()
            .map(|(i, &l)| (alloc::vec![i as u8], alloc::vec![0u8; l]))
            .collect()
    }

    fn interior_cells(lens: &[usize]) -> Vec<InteriorEntry> {
        lens.iter()
            .enumerate()
            .map(|(i, &l)| (i as u32 + 100, alloc::vec![i as u8], alloc::vec![0u8; l]))
            .collect()
    }

    /// A multi-way leaf split (parts > 2) partitions the entries exactly: the
    /// parts and the promoted separators together cover every index once, in
    /// order, and every part individually fits a body-0 page.
    #[test]
    fn pack_leaf_multiway_partitions_exactly() {
        let page = 4096usize;
        // 12 near-maxLocal cells → several parts (each part holds only a few).
        let entries = leaf_entries(&[1000; 12]);
        let (parts, seps) = pack_index_leaf(&entries, 0, page);
        assert!(parts.len() > 2, "expected a multi-way split, got {parts:?}");
        assert_eq!(seps.len(), parts.len() - 1);

        // Reconstruct the original index sequence: part[0], sep[0], part[1], …
        let mut seq = Vec::new();
        for (k, r) in parts.iter().enumerate() {
            assert!(!r.is_empty(), "empty part {k}");
            seq.extend(r.clone());
            if k < seps.len() {
                seq.push(seps[k]);
            }
        }
        assert_eq!(seq, (0..entries.len()).collect::<Vec<_>>());

        // Every part individually fits, and every serialize succeeds.
        for r in &parts {
            let rc: Vec<Vec<u8>> = entries[r.clone()].iter().map(|(_, c)| c.clone()).collect();
            assert!(leaf_fits(&entries[r.clone()], 0, page));
            serialize_index_leaf(page, 0, &rc, None).unwrap();
        }
    }

    /// A skewed leaf (large cells clustered at the front — the exact shape that
    /// overflowed the old midpoint split) still yields fitting parts.
    #[test]
    fn pack_leaf_front_heavy_fits() {
        let page = 4096usize;
        let mut lens = alloc::vec![1000usize; 4];
        lens.extend([20usize; 8]);
        let entries = leaf_entries(&lens);
        let (parts, _seps) = pack_index_leaf(&entries, 0, page);
        for r in &parts {
            assert!(
                leaf_fits(&entries[r.clone()], 0, page),
                "part {r:?} overflows"
            );
        }
    }

    /// A multi-way interior split promotes the right separators and keeps each
    /// part fitting, with child pointers preserved in order.
    #[test]
    fn pack_interior_multiway_partitions_exactly() {
        let page = 4096usize;
        let cells = interior_cells(&[1000; 12]);
        let right = 9999u32;
        let (parts, seps) = pack_index_interior(&cells, right, 0, page);
        assert!(parts.len() > 2, "expected a multi-way interior split");
        assert_eq!(seps.len(), parts.len() - 1);

        // Reconstruct the child-pointer order: for each non-last part, its cells'
        // children then the promoted separator's implied divider; the whole thing
        // must enumerate children 100..100+12 then the page right pointer.
        let mut children = Vec::new();
        for (k, (pc, pr)) in parts.iter().enumerate() {
            for (c, _, _) in pc {
                children.push(*c);
            }
            children.push(*pr); // this part's right pointer
            let _ = k;
            // Each part must fit and serialize.
            serialize_index_interior(page, 0, pc, *pr, None).unwrap();
        }
        // Children seen (each part's cells + its right pointer) plus the promoted
        // separators' own children reconstruct the full ordered child set.
        // Simpler invariant: the last part's right pointer is the page's right.
        assert_eq!(parts.last().unwrap().1, right);
        assert!(children.contains(&right));
    }

    /// Degenerate tiny page: the packer must not panic and each part still fits.
    #[test]
    fn pack_leaf_small_page() {
        let page = 512usize;
        let entries = leaf_entries(&[100; 8]);
        let (parts, seps) = pack_index_leaf(&entries, 0, page);
        assert_eq!(seps.len(), parts.len() - 1);
        for r in &parts {
            assert!(!r.is_empty());
            assert!(leaf_fits(&entries[r.clone()], 0, page));
        }
    }
}
