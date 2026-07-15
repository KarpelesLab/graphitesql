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

use super::page::{BtreePage, PageType, payload_split};
use crate::error::{Error, Result};
use crate::pager::{PageSource, WritePager};
use crate::util::varint;
use alloc::vec;
use alloc::vec::Vec;

/// A leaf cell in logical form: its rowid and its raw on-page bytes.
type LeafCell = (i64, Vec<u8>);

/// An interior-table cell in logical form: a left-child page and its separator
/// rowid.
type InteriorCell = (u32, i64);

/// One part of a multi-way interior split: its on-page cells and its right-most
/// child pointer.
type InteriorPart = (Vec<InteriorCell>, u32);

/// A node split bubbling up to the parent. The over-full page was repacked into
/// the original page (reused, keeping its page number) plus one or more brand-new
/// right-sibling pages, each of which individually fits. `first_key` is the
/// separator key (the largest rowid in the subtree) of the reused original page.
/// `siblings` lists the new sibling pages left-to-right, each paired with the
/// largest rowid in that sibling's subtree. For an interior split the *last*
/// sibling's key is a placeholder ([`i64::MAX`]) — the parent supplies the real
/// upper bound (its own separator for this child, or nothing when the child is
/// the parent's right-most). `siblings` is always non-empty.
struct Split {
    first_key: i64,
    siblings: Vec<(i64, u32)>,
}

/// Allocate a fresh, empty table b-tree (a single leaf page) and return its
/// root page number. Use this when creating a table.
pub fn create_table_root(wp: &mut WritePager) -> Result<u32> {
    let page_size = wp.usable_size() + wp.header().reserved_space as usize;
    let root = wp.allocate_page()?;
    let buf = serialize_leaf(page_size, 0, &[], None)?;
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
                let buf = serialize_leaf(page_size, body, &cells, header_prefix.as_deref())?;
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

/// Empty a table b-tree while keeping its root page number stable: free every
/// descendant page (and overflow chain) and reset the root to an empty leaf.
/// Used to compact a table in place after deletes leave empty/underfull pages,
/// without having to rewrite the table's `rootpage` in `sqlite_schema`.
pub fn clear_table(wp: &mut WritePager, root: u32) -> Result<()> {
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    let bt = BtreePage::parse(wp.page(root)?)?;
    let body = if root == 1 {
        crate::format::header::HEADER_LEN
    } else {
        0
    };
    let prefix = page_one_prefix(root, &bt);
    match bt.page_type() {
        PageType::LeafTable => {
            for i in 0..bt.num_cells() {
                free_overflow_chain(wp, bt.table_leaf_cell(i, usable)?.payload.overflow)?;
            }
        }
        PageType::InteriorTable => {
            let n = bt.num_cells();
            for i in 0..n {
                super::free_tree(wp, bt.child_pointer(i)?)?;
            }
            super::free_tree(wp, bt.right_pointer())?;
        }
        _ => return Err(Error::Corrupt("clear of a non-table b-tree".into())),
    }
    let empty = serialize_leaf(page_size, body, &[], prefix.as_deref())?;
    wp.write_page(root, empty)?;
    Ok(())
}

/// Whether the table b-tree at `root` has any empty leaf page below an interior
/// page (i.e. reclaimable waste from deletes). A single-leaf table never does.
pub fn table_has_empty_leaf(wp: &dyn PageSource, root: u32) -> Result<bool> {
    fn walk(wp: &dyn PageSource, page_no: u32) -> Result<bool> {
        let bt = BtreePage::parse(wp.page(page_no)?)?;
        match bt.page_type() {
            PageType::LeafTable => Ok(bt.num_cells() == 0),
            PageType::InteriorTable => {
                for i in 0..bt.num_cells() {
                    if walk(wp, bt.child_pointer(i)?)? {
                        return Ok(true);
                    }
                }
                walk(wp, bt.right_pointer())
            }
            _ => Ok(false),
        }
    }
    let bt = BtreePage::parse(wp.page(root)?)?;
    if bt.page_type() == PageType::LeafTable {
        return Ok(false);
    }
    walk(wp, root)
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
pub(crate) fn write_overflow_chain(wp: &mut WritePager, data: &[u8]) -> Result<u32> {
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
                let buf = serialize_leaf(page_size, body, &cells, header_prefix.as_deref())?;
                wp.write_page(page_no, buf)?;
                Ok(None)
            } else {
                // Repack the over-full cell list into as many consecutive parts as
                // are needed for each part to individually fit its page (a robust
                // multi-way split — a single fixed halving is not enough when one
                // near-page-sized cell skews the byte total). Part 0 reuses this
                // page; the rest become fresh right-sibling pages.
                let ranges = pack_leaf_ranges(&cells, body, page_size);
                if ranges.len() == 1 {
                    // A single cell that will not fit this page's body offset. For
                    // a body-0 page this is impossible (one leaf cell always fits a
                    // body-0 page); it can only be a page-1 leaf (the schema table)
                    // holding an oversized single row. Write it back defensively —
                    // `serialize_leaf` returns `Corrupt` rather than panicking if it
                    // truly cannot fit.
                    let buf = serialize_leaf(page_size, body, &cells, header_prefix.as_deref())?;
                    wp.write_page(page_no, buf)?;
                    return Ok(None);
                }
                let r0 = ranges[0].clone();
                let first_key = cells[r0.end - 1].0;
                let left_buf =
                    serialize_leaf(page_size, body, &cells[r0], header_prefix.as_deref())?;
                wp.write_page(page_no, left_buf)?;
                let mut siblings = Vec::with_capacity(ranges.len() - 1);
                for r in &ranges[1..] {
                    let sep = cells[r.end - 1].0;
                    let pg = wp.allocate_page()?;
                    let buf = serialize_leaf(page_size, 0, &cells[r.clone()], None)?;
                    wp.write_page(pg, buf)?;
                    siblings.push((sep, pg));
                }
                Ok(Some(Split {
                    first_key,
                    siblings,
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
                // The child split into its (reused) page plus one or more new
                // siblings; adopt them all. The reused child keeps its page number
                // but takes a new separator (`first_key`); each new sibling is
                // inserted after it with its own separator.
                let mut sibs = s.siblings;
                if p < n {
                    let old_key = cells[p].1;
                    cells[p].1 = s.first_key;
                    // The last sibling covers the old separator's upper bound.
                    if let Some(last) = sibs.last_mut() {
                        last.0 = old_key;
                    }
                    for (off, (k, pg)) in sibs.into_iter().enumerate() {
                        cells.insert(p + 1 + off, (pg, k));
                    }
                } else {
                    // Descended into the right-most child: it keeps its page as a
                    // now-non-right cell; the last new sibling becomes the new
                    // right-most pointer.
                    cells.push((child, s.first_key));
                    let last = sibs.pop().expect("split always has a sibling");
                    for (k, pg) in sibs {
                        cells.push((pg, k));
                    }
                    right = last.1;
                }
            }

            let header_prefix = page_one_prefix(page_no, &bt);
            if interior_fits(&cells, body, page_size) {
                let buf =
                    serialize_interior(page_size, body, &cells, right, header_prefix.as_deref())?;
                wp.write_page(page_no, buf)?;
                Ok(None)
            } else {
                // Robust multi-way interior split: partition the children into
                // groups that each fit, promoting one separator key between groups.
                let (parts, seps) = pack_interior(&cells, right, body, page_size);
                if parts.len() == 1 {
                    // Interior cells are tiny, so a single un-splittable part is
                    // unreachable in practice; write back defensively.
                    let (pc, pr) = &parts[0];
                    let buf =
                        serialize_interior(page_size, body, pc, *pr, header_prefix.as_deref())?;
                    wp.write_page(page_no, buf)?;
                    return Ok(None);
                }
                let first_key = seps[0];
                let (p0c, p0r) = &parts[0];
                let left_buf =
                    serialize_interior(page_size, body, p0c, *p0r, header_prefix.as_deref())?;
                wp.write_page(page_no, left_buf)?;
                let mut siblings = Vec::with_capacity(parts.len() - 1);
                for k in 1..parts.len() {
                    let (pc, pr) = &parts[k];
                    let pg = wp.allocate_page()?;
                    let buf = serialize_interior(page_size, 0, pc, *pr, None)?;
                    wp.write_page(pg, buf)?;
                    // Separator following this part; the last part has no promoted
                    // key (the parent supplies the upper bound).
                    let key = if k < seps.len() { seps[k] } else { i64::MAX };
                    siblings.push((key, pg));
                }
                Ok(Some(Split {
                    first_key,
                    siblings,
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

    // Move the old root's (reused-page) content to a new page at body offset 0.
    let new_left = wp.allocate_page()?;
    let relocated = reserialize(wp, &bt, usable, page_size)?;
    wp.write_page(new_left, relocated)?;

    // The root becomes an interior node whose children are the relocated old-root
    // content plus every sibling produced by the split: [(new_left, first_key),
    // (sib_1, key_1), …] with the last sibling as the right-most pointer.
    let mut cells: Vec<(u32, i64)> = Vec::with_capacity(split.siblings.len());
    cells.push((new_left, split.first_key));
    let mut sibs = split.siblings;
    let last = sibs.pop().expect("split always has a sibling");
    for (k, pg) in sibs {
        cells.push((pg, k));
    }
    let buf = serialize_interior(page_size, body, &cells, last.1, header_prefix.as_deref())?;
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
            serialize_leaf(page_size, 0, &cells, None)
        }
        PageType::InteriorTable => {
            let n = bt.num_cells();
            let mut cells = Vec::with_capacity(n);
            for i in 0..n {
                cells.push((bt.child_pointer(i)?, bt.table_interior_key(i)?));
            }
            serialize_interior(page_size, 0, &cells, bt.right_pointer(), None)
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
pub(crate) fn page_one_prefix(page_no: u32, bt: &BtreePage) -> Option<Vec<u8>> {
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

/// Greedily pack an over-full leaf's cells into as many consecutive parts as are
/// needed for each part to individually fit a leaf page. Returns the index ranges
/// of the parts, in order. Part 0 is sized against `body0` (its page's body
/// offset); every later part is sized against a body-0 page. Each returned range
/// is non-empty and (except for the degenerate case of one cell too large for the
/// page's body offset — only possible on a page-1 leaf) fits its page.
fn pack_leaf_ranges(
    cells: &[LeafCell],
    body0: usize,
    page_size: usize,
) -> Vec<core::ops::Range<usize>> {
    let n = cells.len();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < n {
        let body = if parts.is_empty() { body0 } else { 0 };
        // `leaf_fits` bound: sum(cell.len() + 2) <= page_size - body - 8.
        let cap = page_size.saturating_sub(body + 8);
        let mut used = 0usize;
        let mut j = i;
        while j < n {
            let need = cells[j].1.len() + 2;
            // Always place at least one cell per part (`j == i`), even if that lone
            // cell exceeds `cap` (unavoidable — a cell cannot be split further).
            if j == i || used + need <= cap {
                used += need;
                j += 1;
            } else {
                break;
            }
        }
        parts.push(i..j);
        i = j;
    }
    parts
}

/// Greedily partition an over-full interior page's children into as many parts as
/// are needed for each part to individually fit. Children are the cells' left
/// pointers plus the page's right pointer. Between two adjacent parts one cell's
/// key is *promoted* (returned in the second tuple element) and its child becomes
/// the left part's right pointer. Returns `(parts, separators)` where each part is
/// `(on-page cells, right pointer)` and `separators.len() == parts.len() - 1`.
/// Part 0 is sized against `body0`; later parts against a body-0 page.
fn pack_interior(
    cells: &[InteriorCell],
    right: u32,
    body0: usize,
    page_size: usize,
) -> (Vec<InteriorPart>, Vec<i64>) {
    let n = cells.len();
    // child(j): the j-th child pointer (j == n is the page's right pointer).
    let child = |j: usize| -> u32 { if j < n { cells[j].0 } else { right } };
    let mut parts: Vec<InteriorPart> = Vec::new();
    let mut seps: Vec<i64> = Vec::new();
    let mut i = 0usize; // first child index of the current group
    loop {
        let body = if parts.is_empty() { body0 } else { 0 };
        // `interior_fits` bound: sum(cell_len + 2) <= page_size - body - 12.
        let cap = page_size.saturating_sub(body + 12);
        let mut used = 0usize;
        let mut b = i;
        while b < n {
            let need = interior_cell_len(cells[b].1) + 2;
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
        // Not the last part: promote cells[b] as the separator; cells[b]'s child
        // becomes this part's right pointer. Guard against leaving the final part
        // with no on-page cells by backing off one cell (unreachable for real
        // page sizes, where a group holds hundreds of tiny interior cells).
        let mut bb = b;
        if bb == n - 1 && bb > i + 1 {
            bb -= 1;
        }
        parts.push((cells[i..bb].to_vec(), child(bb)));
        seps.push(cells[bb].1);
        i = bb + 1;
    }
    (parts, seps)
}

fn serialize_leaf(
    page_size: usize,
    body: usize,
    cells: &[LeafCell],
    header_prefix: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let ptr_base = body + 8;
    // The cell-pointer array grows up from `ptr_base`; cell content is packed down
    // from the page end. They must not meet — otherwise a content byte would land
    // in the pointer array (or the pointer offsets would point past the page end).
    // The caller (`pack_leaf_ranges`) guarantees a fitting list, but check
    // defensively so an invariant violation surfaces as `Corrupt`, never a panic.
    let ptr_end = ptr_base + 2 * cells.len();
    let mut content = page_size;
    for (i, (_, cell)) in cells.iter().enumerate() {
        if content < cell.len() || content - cell.len() < ptr_end {
            return Err(Error::Corrupt(
                "leaf page overflow while serializing".into(),
            ));
        }
        content -= cell.len();
        buf[content..content + cell.len()].copy_from_slice(cell);
        let p = ptr_base + 2 * i;
        buf[p] = (content >> 8) as u8;
        buf[p + 1] = content as u8;
    }
    buf[body] = 0x0d;
    write_u16(&mut buf, body + 3, cells.len() as u16);
    write_cell_content_start(&mut buf, body + 5, content, page_size);
    Ok(buf)
}

fn serialize_interior(
    page_size: usize,
    body: usize,
    cells: &[(u32, i64)],
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
    for (i, (child, key)) in cells.iter().enumerate() {
        let mut cell = Vec::with_capacity(interior_cell_len(*key));
        cell.extend_from_slice(&child.to_be_bytes());
        let mut vbuf = [0u8; varint::MAX_LEN];
        let n = varint::encode_i64(*key, &mut vbuf);
        cell.extend_from_slice(&vbuf[..n]);

        if content < cell.len() || content - cell.len() < ptr_end {
            return Err(Error::Corrupt(
                "interior page overflow while serializing".into(),
            ));
        }
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
    Ok(buf)
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
    use crate::format::TextEncoding;
    use crate::format::record::{decode_record, encode_record};
    use crate::value::Value;
    use crate::vfs::{OpenFlags, Vfs, memory::MemoryVfs};

    fn new_table_root(wp: &mut WritePager) -> u32 {
        // Allocate an empty leaf page to serve as a table root.
        let page_size = wp.usable_size() + wp.header().reserved_space as usize;
        let root = wp.allocate_page().unwrap();
        let buf = serialize_leaf(page_size, 0, &[], None).unwrap();
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
            let rec = encode_record(&[Value::Null, Value::Text(alloc::format!("row{i}").into())]);
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
