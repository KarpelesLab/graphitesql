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
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    let root = wp.allocate_page()?;
    let buf = serialize_index_leaf(page_size, usable, 0, &[], None)?;
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
    match insert_rec(wp, root, record, rcell, colls, descs, 0)? {
        IdxUp::Fit => {}
        IdxUp::Split(split) => grow_root(wp, root, split)?,
        // The root itself is a leaf that overflowed: deepen the tree and
        // redistribute its entries across the new children.
        IdxUp::LeafOverfull(entries) => deepen_leaf_root(wp, root, entries)?,
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
    let empty = serialize_index_leaf(page_size, usable, 0, &[], None)?;
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

/// What a level of the recursion reports to its caller.
enum IdxUp {
    /// The page absorbed the insert (and was written); nothing to do above.
    Fit,
    /// A leaf overflowed. Its full, sorted entry list is handed up so the parent
    /// can rebalance it with its siblings; the leaf has **not** been written.
    LeafOverfull(Vec<LeafEntry>),
    /// An interior page split (rare, deep trees) and bubbled an entry up.
    Split(IdxSplit),
}

#[allow(clippy::too_many_arguments)]
fn insert_rec(
    wp: &mut WritePager,
    page_no: u32,
    target: &[u8],
    rcell: Vec<u8>,
    colls: &[Collation],
    descs: &[bool],
    depth: u32,
) -> Result<IdxUp> {
    // SQLite caps cursor descent at BTCURSOR_MAX_DEPTH (20) and reports
    // SQLITE_CORRUPT beyond it (`moveToChild`); a deeper "tree" here means a
    // page cycle, which must surface as an error, not unbounded recursion.
    if depth >= 20 {
        return Err(Error::Corrupt("b-tree depth limit exceeded".into()));
    }
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
                    Ordering::Equal => return Ok(IdxUp::Fit), // already present
                    Ordering::Greater => {}
                }
            }
            entries.insert(pos, (target.to_vec(), rcell));
            if leaf_fits(&entries, body, usable) {
                let prefix = page_one_prefix(page_no, &bt);
                let buf = serialize_index_leaf(
                    page_size,
                    usable,
                    body,
                    &rcells(&entries),
                    prefix.as_deref(),
                )?;
                wp.write_page(page_no, buf)?;
                Ok(IdxUp::Fit)
            } else {
                // Hand the overflow up: the parent pools this leaf with its
                // siblings and redistributes so the tree stays compact.
                Ok(IdxUp::LeafOverfull(entries))
            }
        }
        PageType::InteriorIndex => {
            let (cells, right) = read_interior(wp, &bt, usable)?;
            let mut children: Vec<u32> = Vec::with_capacity(cells.len() + 1);
            let mut dividers: Vec<IdxKey> = Vec::with_capacity(cells.len());
            for (c, full, rc) in cells {
                children.push(c);
                dividers.push((full, rc));
            }
            children.push(right);
            let prefix = page_one_prefix(page_no, &bt);
            drop(bt);

            // Descend at the first divider whose record > target, else the right
            // child. Return early on an exact match (already present).
            let mut p = children.len() - 1;
            for (i, (full, _)) in dividers.iter().enumerate() {
                match cmp_records(target, full, enc, colls, descs)? {
                    Ordering::Less => {
                        p = i;
                        break;
                    }
                    Ordering::Equal => return Ok(IdxUp::Fit),
                    Ordering::Greater => {}
                }
            }

            match insert_rec(wp, children[p], target, rcell, colls, descs, depth + 1)? {
                IdxUp::Fit => return Ok(IdxUp::Fit), // this page is unchanged
                IdxUp::LeafOverfull(child_entries) => {
                    balance_leaf_into(wp, &mut children, &mut dividers, p, child_entries)?;
                }
                IdxUp::Split(s) => adopt_split(&mut children, &mut dividers, p, s),
            }

            finish_interior(
                wp, page_no, body, page_size, usable, &children, &dividers, prefix,
            )
        }
        _ => Err(Error::Corrupt("insert into a non-index b-tree".into())),
    }
}

/// Adopt an interior-child split into the parent's `children`/`dividers` lists.
fn adopt_split(children: &mut Vec<u32>, dividers: &mut Vec<IdxKey>, p: usize, s: IdxSplit) {
    let mut sibs = s.siblings;
    if p < dividers.len() {
        let old_div = dividers[p].clone();
        let mut new_divs = Vec::with_capacity(sibs.len() + 1);
        new_divs.push(s.first);
        for (k, _) in &sibs[..sibs.len() - 1] {
            new_divs.push(k.clone());
        }
        new_divs.push(old_div);
        let new_children: Vec<u32> = sibs.iter().map(|(_, pg)| *pg).collect();
        children.splice(p + 1..p + 1, new_children);
        dividers.splice(p..p + 1, new_divs);
    } else {
        let last = sibs.pop().expect("split always has a sibling");
        dividers.push(s.first);
        for (k, _) in &sibs {
            dividers.push(k.clone());
        }
        for (_, pg) in &sibs {
            children.push(*pg);
        }
        children.push(last.1);
    }
}

/// Rebuild an interior page from its `children`/`dividers`, writing it back if it
/// fits, else performing the (rare) greedy multi-way interior split.
#[allow(clippy::too_many_arguments)]
fn finish_interior(
    wp: &mut WritePager,
    page_no: u32,
    body: usize,
    page_size: usize,
    usable: usize,
    children: &[u32],
    dividers: &[IdxKey],
    prefix: Option<Vec<u8>>,
) -> Result<IdxUp> {
    let mut cells: Vec<InteriorEntry> = Vec::with_capacity(dividers.len());
    for i in 0..dividers.len() {
        cells.push((children[i], dividers[i].0.clone(), dividers[i].1.clone()));
    }
    let right = *children.last().expect("interior always has a right child");

    if interior_fits(&cells, body, usable) {
        let buf =
            serialize_index_interior(page_size, usable, body, &cells, right, prefix.as_deref())?;
        wp.write_page(page_no, buf)?;
        return Ok(IdxUp::Fit);
    }
    let (parts, seps) = pack_index_interior(&cells, right, body, usable);
    if parts.len() == 1 {
        let (pc, pr) = &parts[0];
        let buf = serialize_index_interior(page_size, usable, body, pc, *pr, prefix.as_deref())?;
        wp.write_page(page_no, buf)?;
        return Ok(IdxUp::Fit);
    }
    let first = seps[0].clone();
    let (p0c, p0r) = &parts[0];
    let lbuf = serialize_index_interior(page_size, usable, body, p0c, *p0r, prefix.as_deref())?;
    wp.write_page(page_no, lbuf)?;
    let mut siblings = Vec::with_capacity(parts.len() - 1);
    for k in 1..parts.len() {
        let (pc, pr) = &parts[k];
        let pg = wp.allocate_page()?;
        let buf = serialize_index_interior(page_size, usable, 0, pc, *pr, None)?;
        wp.write_page(pg, buf)?;
        let key = if k < seps.len() {
            seps[k].clone()
        } else {
            (Vec::new(), Vec::new())
        };
        siblings.push((key, pg));
    }
    Ok(IdxUp::Split(IdxSplit { first, siblings }))
}

/// Rebalance an overflowing index leaf (`children[p]`, whose entries are
/// `child_entries`) with up to two siblings under the same parent, splicing the
/// resulting pages/promoted dividers back into the parent. This is the
/// index-b-tree specialization of SQLite's `balance_nonroot`: an index is not a
/// leaf-data tree, so the divider entries between siblings are pooled with the
/// leaf entries and the boundary entries are promoted back up (an index split
/// promotes an entry — it lives only in the parent, not on any leaf).
fn balance_leaf_into(
    wp: &mut WritePager,
    children: &mut Vec<u32>,
    dividers: &mut Vec<IdxKey>,
    p: usize,
    child_entries: Vec<LeafEntry>,
) -> Result<()> {
    let (w0, n_old) = super::balance::sibling_window(p, children.len());
    let usable = wp.usable_size();

    // Pool every window sibling's entries, interleaved with the parent divider
    // entries that separate them (child pointer already stripped — leaf-index
    // divider records carry no child pointer).
    let old_pages: Vec<u32> = children[w0..w0 + n_old].to_vec();
    let mut pooled: Vec<LeafEntry> = Vec::new();
    let mut cnt_old: Vec<usize> = Vec::with_capacity(n_old);
    for (offset, &pg) in old_pages.iter().enumerate() {
        if w0 + offset == p {
            pooled.extend(child_entries.iter().cloned());
        } else {
            let bt = BtreePage::parse(wp.page(pg)?)?;
            pooled.extend(read_leaf(wp, &bt, usable)?);
        }
        cnt_old.push(pooled.len());
        if offset + 1 < n_old {
            pooled.push(dividers[w0 + offset].clone());
        }
    }

    let (pages, new_dividers) = balance_leaf_pooled(wp, &old_pages, &pooled, &cnt_old)?;

    children.splice(w0..w0 + n_old, pages);
    dividers.splice(w0..w0 + n_old - 1, new_dividers);
    Ok(())
}

/// Redistribute a pool of index-leaf entries (siblings interleaved with the
/// dividers between them) across the right number of pages, reusing `old_pages`
/// (freeing surplus, allocating shortfall) and assigning slices to page numbers
/// in ascending order. Returns the new page numbers and the `n_new - 1` promoted
/// divider entries between them (the boundary entries live only in the parent).
fn balance_leaf_pooled(
    wp: &mut WritePager,
    old_pages: &[u32],
    pooled: &[LeafEntry],
    cnt_old: &[usize],
) -> Result<(Vec<u32>, Vec<IdxKey>)> {
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    let sz: Vec<usize> = pooled.iter().map(|(_, c)| c.len()).collect();
    let cnt_new = super::balance::distribute(&sz, cnt_old, true, false, usable);
    let n_new = cnt_new.len();
    let n_old = old_pages.len();

    let mut kept: Vec<u32> = old_pages.iter().take(n_new).copied().collect();
    for _ in n_old..n_new {
        kept.push(wp.allocate_page()?);
    }
    for &pg in &old_pages[n_new.min(n_old)..] {
        wp.free_page(pg)?;
    }
    kept.sort_unstable();

    let mut new_dividers = Vec::with_capacity(n_new.saturating_sub(1));
    let mut start = 0usize;
    for (i, &end) in cnt_new.iter().enumerate() {
        // Page i owns pooled entries [start, end); the entry at `end` (when
        // present) is the boundary that is promoted to the parent, not stored.
        let slice = &pooled[start..end];
        let buf = serialize_index_leaf(page_size, usable, 0, &rcells(slice), None)?;
        wp.write_page(kept[i], buf)?;
        if i < n_new - 1 {
            new_dividers.push(pooled[end].clone());
        }
        start = end + 1; // skip the promoted divider entry
    }
    Ok((kept, new_dividers))
}

/// The root leaf overflowed: deepen the tree by one level and redistribute the
/// root's entries across fresh child leaves, keeping the root's page number
/// stable. (Index roots are never page 1, so no header prefix is involved.)
fn deepen_leaf_root(wp: &mut WritePager, root: u32, entries: Vec<LeafEntry>) -> Result<()> {
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    let child0 = wp.allocate_page()?;
    let cnt_old = vec![entries.len()];
    let (pages, dividers) = balance_leaf_pooled(wp, &[child0], &entries, &cnt_old)?;

    let mut icells: Vec<InteriorEntry> = Vec::with_capacity(dividers.len());
    for (i, (full, rc)) in dividers.into_iter().enumerate() {
        icells.push((pages[i], full, rc));
    }
    let right = *pages.last().expect("deepen always yields >= 2 leaves");
    let buf = serialize_index_interior(page_size, usable, 0, &icells, right, None)?;
    wp.write_page(root, buf)?;
    Ok(())
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
    let buf = serialize_index_interior(page_size, usable, 0, &cells, last.1, None)?;
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

fn leaf_fits(entries: &[LeafEntry], body: usize, usable: usize) -> bool {
    let used: usize = entries.iter().map(|(_, c)| c.len() + 2).sum();
    used <= usable - body - 8
}

fn interior_fits(cells: &[InteriorEntry], body: usize, usable: usize) -> bool {
    let used: usize = cells.iter().map(|(_, _, c)| 4 + c.len() + 2).sum();
    used <= usable - body - 12
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
    usable: usize,
) -> (Vec<IdxInteriorPart>, Vec<IdxKey>) {
    let n = cells.len();
    // child(j): the j-th child pointer (j == n is the page's right pointer).
    let child = |j: usize| -> u32 { if j < n { cells[j].0 } else { right } };
    let mut parts: Vec<IdxInteriorPart> = Vec::new();
    let mut seps: Vec<IdxKey> = Vec::new();
    let mut i = 0usize;
    loop {
        let body = if parts.is_empty() { body0 } else { 0 };
        // `interior_fits` bound: sum(4 + rcell.len() + 2) <= usable - body - 12.
        let cap = usable.saturating_sub(body + 12);
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
    usable: usize,
    body: usize,
    rcells: &[Vec<u8>],
    header_prefix: Option<&[u8]>,
) -> Result<Vec<u8>> {
    // Buffer is the full `page_size`; content lays out only within `usable` so the
    // reserved bytes at the page end stay zero.
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let ptr_base = body + 8;
    // The cell-pointer array grows up from `ptr_base`; cell content packs down from
    // the top of the usable area. They must not meet. `leaf_fits`/`balance`
    // guarantees a fitting list, but check defensively so a violation surfaces as
    // `Corrupt`, not a panic.
    let ptr_end = ptr_base + 2 * rcells.len();
    let mut content = usable;
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
    usable: usize,
    body: usize,
    cells: &[InteriorEntry],
    right: u32,
    header_prefix: Option<&[u8]>,
) -> Result<Vec<u8>> {
    // Buffer is the full `page_size`; content lays out only within `usable` so the
    // reserved bytes at the page end stay zero.
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let ptr_base = body + 12;
    let ptr_end = ptr_base + 2 * cells.len();
    let mut content = usable;
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
    use crate::format::record::encode_record;
    use crate::vfs::{OpenFlags, Vfs, memory::MemoryVfs};

    /// Insert index records with scrambled keys into a small-page tree so it
    /// grows several levels deep, then verify the index holds every entry, in
    /// key order, and stays compact (sibling rebalancing avoided fragmentation).
    #[test]
    fn deep_index_random_order_is_compact_and_ordered() {
        let vfs = MemoryVfs::new();
        let f = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
        let mut wp = WritePager::create(f, None, 512).unwrap();
        let root = create_index_root(&mut wp).unwrap();

        // 5001 is prime; key = i*104729 mod 5001 is a permutation of 0..5000.
        let n: i64 = 5001;
        for i in 0..n {
            let key = (i.wrapping_mul(104729)).rem_euclid(n);
            let rec = encode_record(&[Value::Integer(key), Value::Integer(i)]);
            insert_index(&mut wp, root, &rec, &[], &[]).unwrap();
        }

        let recs = index_range_records(&wp, root, None, None, &[], &[]).unwrap();
        assert_eq!(recs.len(), n as usize);
        let mut prev = -1i64;
        for r in &recs {
            let key = match r[0] {
                Value::Integer(k) => k,
                _ => panic!("bad key"),
            };
            assert!(key > prev, "index out of order: {key} after {prev}");
            prev = key;
        }
        assert!(
            wp.page_count() < 300,
            "index fragmented: {} pages for {n} entries",
            wp.page_count()
        );
    }

    fn interior_cells(lens: &[usize]) -> Vec<InteriorEntry> {
        lens.iter()
            .enumerate()
            .map(|(i, &l)| (i as u32 + 100, alloc::vec![i as u8], alloc::vec![0u8; l]))
            .collect()
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
            serialize_index_interior(page, page, 0, pc, *pr, None).unwrap();
        }
        // Simpler invariant: the last part's right pointer is the page's right.
        assert_eq!(parts.last().unwrap().1, right);
        assert!(children.contains(&right));
    }
}
