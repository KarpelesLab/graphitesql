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
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    let root = wp.allocate_page()?;
    let buf = serialize_leaf(page_size, usable, 0, &[], None)?;
    wp.write_page(root, buf)?;
    Ok(root)
}

/// Insert (or replace) a row `(rowid, payload)` into the table b-tree at `root`.
pub fn insert_table(wp: &mut WritePager, root: u32, rowid: i64, payload: &[u8]) -> Result<()> {
    let cell = build_leaf_cell(wp, rowid, payload)?;
    match insert_rec(wp, root, rowid, cell)? {
        Up::Fit => {}
        Up::Split(split) => grow_root(wp, root, split)?,
        // The root itself is a leaf that overflowed: deepen the tree (SQLite's
        // `balance_deeper`) and redistribute its cells across the new children.
        Up::LeafOverfull(cells) => deepen_leaf_root(wp, root, cells)?,
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
                let buf =
                    serialize_leaf(page_size, usable, body, &cells, header_prefix.as_deref())?;
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
    let empty = serialize_leaf(page_size, usable, body, &[], prefix.as_deref())?;
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

/// What a level of the recursion reports to its caller.
enum Up {
    /// The page absorbed the insert (and was written); nothing to do above.
    Fit,
    /// A leaf overflowed. The full, still-sorted cell list is handed up so the
    /// parent can rebalance this leaf together with its siblings (SQLite's
    /// `balance_nonroot`); the overflowing leaf has **not** been written.
    LeafOverfull(Vec<LeafCell>),
    /// An interior page split (rare, deep trees) and bubbled a separator up.
    Split(Split),
}

fn insert_rec(wp: &mut WritePager, page_no: u32, rowid: i64, cell: Vec<u8>) -> Result<Up> {
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
            if leaf_fits(&cells, body, usable) {
                let header_prefix = page_one_prefix(page_no, &bt);
                let buf =
                    serialize_leaf(page_size, usable, body, &cells, header_prefix.as_deref())?;
                wp.write_page(page_no, buf)?;
                Ok(Up::Fit)
            } else {
                // Hand the overflow up: the parent pools this leaf with its
                // siblings and redistributes so the tree stays compact.
                Ok(Up::LeafOverfull(cells))
            }
        }
        PageType::InteriorTable => {
            let n = bt.num_cells();
            let mut children: Vec<u32> = Vec::with_capacity(n + 1);
            let mut dividers: Vec<i64> = Vec::with_capacity(n);
            for i in 0..n {
                children.push(bt.child_pointer(i)?);
                dividers.push(bt.table_interior_key(i)?);
            }
            children.push(bt.right_pointer());
            let prefix = page_one_prefix(page_no, &bt);
            drop(bt);

            // Descend at the first divider whose key >= rowid, else the right child.
            let mut p = n;
            for (i, d) in dividers.iter().enumerate() {
                if rowid <= *d {
                    p = i;
                    break;
                }
            }

            match insert_rec(wp, children[p], rowid, cell)? {
                Up::Fit => return Ok(Up::Fit), // this page is unchanged
                Up::LeafOverfull(child_cells) => {
                    balance_leaf_into(wp, &mut children, &mut dividers, p, child_cells)?;
                }
                Up::Split(s) => adopt_split(&mut children, &mut dividers, p, s),
            }

            finish_interior(
                wp, page_no, body, page_size, usable, &children, &dividers, prefix,
            )
        }
        _ => Err(Error::Corrupt("insert into a non-table b-tree".into())),
    }
}

/// Adopt an interior-child split into the parent's `children`/`dividers` lists.
/// The reused child keeps its slot but gets a new upper separator (`first_key`);
/// each fresh sibling is spliced in after it with its own separator.
fn adopt_split(children: &mut Vec<u32>, dividers: &mut Vec<i64>, p: usize, s: Split) {
    let mut sibs = s.siblings;
    if p < dividers.len() {
        // Descended into a non-right child: replace divider[p] with the reused
        // child's new separator, then the new siblings' separators, then the old
        // separator (which bounds the last new sibling).
        let old_div = dividers[p];
        let mut new_divs = Vec::with_capacity(sibs.len() + 1);
        new_divs.push(s.first_key);
        for (k, _) in &sibs[..sibs.len() - 1] {
            new_divs.push(*k);
        }
        new_divs.push(old_div);
        let new_children: Vec<u32> = sibs.iter().map(|(_, pg)| *pg).collect();
        children.splice(p + 1..p + 1, new_children);
        dividers.splice(p..p + 1, new_divs);
    } else {
        // Descended into the right-most child: it keeps its page (now a normal
        // cell with separator `first_key`); the last new sibling becomes the new
        // right-most pointer.
        let last = sibs.pop().expect("split always has a sibling");
        dividers.push(s.first_key);
        for (k, _) in &sibs {
            dividers.push(*k);
        }
        for (_, pg) in &sibs {
            children.push(*pg);
        }
        children.push(last.1);
    }
}

/// Rebuild an interior page from its `children`/`dividers` lists, writing it back
/// if it fits, else performing the (rare) greedy multi-way interior split and
/// bubbling a [`Split`] up.
#[allow(clippy::too_many_arguments)]
fn finish_interior(
    wp: &mut WritePager,
    page_no: u32,
    body: usize,
    page_size: usize,
    usable: usize,
    children: &[u32],
    dividers: &[i64],
    prefix: Option<Vec<u8>>,
) -> Result<Up> {
    let mut cells: Vec<(u32, i64)> = Vec::with_capacity(dividers.len());
    for i in 0..dividers.len() {
        cells.push((children[i], dividers[i]));
    }
    let right = *children.last().expect("interior always has a right child");

    if interior_fits(&cells, body, usable) {
        let buf = serialize_interior(page_size, usable, body, &cells, right, prefix.as_deref())?;
        wp.write_page(page_no, buf)?;
        return Ok(Up::Fit);
    }
    // Robust multi-way interior split (interiors overflow only in deep trees).
    let (parts, seps) = pack_interior(&cells, right, body, usable);
    if parts.len() == 1 {
        let (pc, pr) = &parts[0];
        let buf = serialize_interior(page_size, usable, body, pc, *pr, prefix.as_deref())?;
        wp.write_page(page_no, buf)?;
        return Ok(Up::Fit);
    }
    let first_key = seps[0];
    let (p0c, p0r) = &parts[0];
    let left_buf = serialize_interior(page_size, usable, body, p0c, *p0r, prefix.as_deref())?;
    wp.write_page(page_no, left_buf)?;
    let mut siblings = Vec::with_capacity(parts.len() - 1);
    for k in 1..parts.len() {
        let (pc, pr) = &parts[k];
        let pg = wp.allocate_page()?;
        let buf = serialize_interior(page_size, usable, 0, pc, *pr, None)?;
        wp.write_page(pg, buf)?;
        let key = if k < seps.len() { seps[k] } else { i64::MAX };
        siblings.push((key, pg));
    }
    Ok(Up::Split(Split {
        first_key,
        siblings,
    }))
}

/// Rebalance an overflowing leaf (`children[p]`, whose cells are `child_cells`)
/// together with up to two of its siblings under the same parent, then splice the
/// resulting pages/dividers back into the parent's `children`/`dividers`. This is
/// the table-b-tree specialization of SQLite's `balance_nonroot`: because a table
/// leaf is a leaf-data node, divider keys are regenerated (largest rowid of each
/// new page) rather than pooled.
fn balance_leaf_into(
    wp: &mut WritePager,
    children: &mut Vec<u32>,
    dividers: &mut Vec<i64>,
    p: usize,
    child_cells: Vec<LeafCell>,
) -> Result<()> {
    let (w0, n_old) = super::balance::sibling_window(p, children.len());
    let usable = wp.usable_size();

    // Pool every window sibling's cells in order. Table leaves are leaf-data, so
    // the parent dividers are not pooled — they are redundant rowid separators.
    let old_pages: Vec<u32> = children[w0..w0 + n_old].to_vec();
    let mut pooled: Vec<LeafCell> = Vec::new();
    let mut cnt_old: Vec<usize> = Vec::with_capacity(n_old);
    for (offset, &pg) in old_pages.iter().enumerate() {
        if w0 + offset == p {
            pooled.extend(child_cells.iter().cloned());
        } else {
            let bt = BtreePage::parse(wp.page(pg)?)?;
            pooled.extend(read_leaf_cells(&bt, usable)?);
        }
        cnt_old.push(pooled.len());
    }

    let (pages, new_dividers) = balance_leaf_pooled(wp, &old_pages, &pooled, &cnt_old)?;

    // Splice: window children -> new pages; the n_old-1 internal window dividers
    // -> the new dividers.
    children.splice(w0..w0 + n_old, pages);
    dividers.splice(w0..w0 + n_old - 1, new_dividers);
    Ok(())
}

/// Redistribute a pool of table-leaf cells across the right number of pages,
/// reusing `old_pages` (freeing any surplus, allocating any shortfall) and
/// assigning slices to page numbers in ascending order (matching SQLite's
/// rekey-to-ascending step). Returns the new page numbers left-to-right and the
/// `n_new - 1` divider rowids between them (the largest rowid of each page but
/// the last).
fn balance_leaf_pooled(
    wp: &mut WritePager,
    old_pages: &[u32],
    pooled: &[LeafCell],
    cnt_old: &[usize],
) -> Result<(Vec<u32>, Vec<i64>)> {
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    let sz: Vec<usize> = pooled.iter().map(|(_, c)| c.len()).collect();
    let cnt_new = super::balance::distribute(&sz, cnt_old, true, true, usable);
    let n_new = cnt_new.len();
    let n_old = old_pages.len();

    // Reuse old pages where possible, allocate the rest, free the surplus, then
    // assign slices to page numbers in ascending order.
    let mut kept: Vec<u32> = old_pages.iter().take(n_new).copied().collect();
    for _ in n_old..n_new {
        kept.push(wp.allocate_page()?);
    }
    for &pg in &old_pages[n_new.min(n_old)..] {
        wp.free_page(pg)?;
    }
    kept.sort_unstable();

    let mut dividers = Vec::with_capacity(n_new.saturating_sub(1));
    let mut start = 0usize;
    for (i, &end) in cnt_new.iter().enumerate() {
        let slice = &pooled[start..end];
        let buf = serialize_leaf(page_size, usable, 0, slice, None)?;
        wp.write_page(kept[i], buf)?;
        if i < n_new - 1 {
            dividers.push(slice.last().expect("non-empty new page").0);
        }
        start = end;
    }
    Ok((kept, dividers))
}

/// The root leaf overflowed: deepen the tree by one level (SQLite's
/// `balance_deeper`) and redistribute the root's cells across fresh child leaves,
/// keeping the root's page number (and, for page 1, its database header) stable.
fn deepen_leaf_root(wp: &mut WritePager, root: u32, cells: Vec<LeafCell>) -> Result<()> {
    let usable = wp.usable_size();
    let page_size = usable + wp.header().reserved_space as usize;
    let bt = BtreePage::parse(wp.page(root)?)?;
    let body = if root == 1 {
        crate::format::header::HEADER_LEN
    } else {
        0
    };
    let prefix = page_one_prefix(root, &bt);
    drop(bt);

    // A single "old page" holds all the root's cells; balancing splits it into
    // the necessary leaves. Allocate it as the reuse slot (matching balance_deeper
    // allocating the child, then balance_nonroot reusing it).
    let child0 = wp.allocate_page()?;
    let cnt_old = vec![cells.len()];
    let (pages, dividers) = balance_leaf_pooled(wp, &[child0], &cells, &cnt_old)?;

    // The root becomes an interior node over the new leaves.
    let mut icells: Vec<(u32, i64)> = Vec::with_capacity(dividers.len());
    for (i, d) in dividers.iter().enumerate() {
        icells.push((pages[i], *d));
    }
    let right = *pages.last().expect("deepen always yields >= 2 leaves");
    let buf = serialize_interior(page_size, usable, body, &icells, right, prefix.as_deref())?;
    wp.write_page(root, buf)?;
    Ok(())
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
    let buf = serialize_interior(
        page_size,
        usable,
        body,
        &cells,
        last.1,
        header_prefix.as_deref(),
    )?;
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
            serialize_leaf(page_size, usable, 0, &cells, None)
        }
        PageType::InteriorTable => {
            let n = bt.num_cells();
            let mut cells = Vec::with_capacity(n);
            for i in 0..n {
                cells.push((bt.child_pointer(i)?, bt.table_interior_key(i)?));
            }
            serialize_interior(page_size, usable, 0, &cells, bt.right_pointer(), None)
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

fn leaf_fits(cells: &[LeafCell], body: usize, usable: usize) -> bool {
    let used: usize = cells.iter().map(|(_, c)| c.len() + 2).sum();
    used <= usable - body - 8
}

fn interior_fits(cells: &[(u32, i64)], body: usize, usable: usize) -> bool {
    let used: usize = cells.iter().map(|(_, k)| interior_cell_len(*k) + 2).sum();
    used <= usable - body - 12
}

fn interior_cell_len(key: i64) -> usize {
    4 + varint::len(key as u64)
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
    usable: usize,
) -> (Vec<InteriorPart>, Vec<i64>) {
    let n = cells.len();
    // child(j): the j-th child pointer (j == n is the page's right pointer).
    let child = |j: usize| -> u32 { if j < n { cells[j].0 } else { right } };
    let mut parts: Vec<InteriorPart> = Vec::new();
    let mut seps: Vec<i64> = Vec::new();
    let mut i = 0usize; // first child index of the current group
    loop {
        let body = if parts.is_empty() { body0 } else { 0 };
        // `interior_fits` bound: sum(cell_len + 2) <= usable - body - 12.
        let cap = usable.saturating_sub(body + 12);
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
    usable: usize,
    body: usize,
    cells: &[LeafCell],
    header_prefix: Option<&[u8]>,
) -> Result<Vec<u8>> {
    // The on-disk buffer is the full `page_size`; cell content is laid out only in
    // the `usable = page_size - reserved` region so the reserved bytes at the page
    // end stay zero (SQLite rejects any cell that extends into the reserved area).
    let mut buf = vec![0u8; page_size];
    if let Some(h) = header_prefix {
        buf[..h.len()].copy_from_slice(h);
    }
    let ptr_base = body + 8;
    // The cell-pointer array grows up from `ptr_base`; cell content is packed down
    // from the top of the usable area. They must not meet — otherwise a content
    // byte would land in the pointer array (or the pointer offsets would point past
    // the usable end). The caller (`leaf_fits`/`balance`) guarantees a fitting list,
    // but check defensively so an invariant violation surfaces as `Corrupt`, never a
    // panic.
    let ptr_end = ptr_base + 2 * cells.len();
    let mut content = usable;
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
    usable: usize,
    body: usize,
    cells: &[(u32, i64)],
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
        let usable = wp.usable_size();
        let page_size = usable + wp.header().reserved_space as usize;
        let root = wp.allocate_page().unwrap();
        let buf = serialize_leaf(page_size, usable, 0, &[], None).unwrap();
        wp.write_page(root, buf).unwrap();
        root
    }

    fn wp() -> WritePager {
        let vfs = MemoryVfs::new();
        let f = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
        WritePager::create(f, None, 4096).unwrap()
    }

    fn wp_sized(page_size: u32) -> WritePager {
        let vfs = MemoryVfs::new();
        let f = vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
        WritePager::create(f, None, page_size).unwrap()
    }

    /// Insert rowids in a scrambled (non-sequential) order into a small-page tree
    /// so it grows several levels deep, then verify every row is present, in
    /// order, and the tree stays compact (sibling rebalancing avoided the
    /// 1-cell-per-page fragmentation the greedy splitter produced).
    #[test]
    fn deep_tree_random_order_is_compact_and_ordered() {
        let mut wp = wp_sized(512);
        let root = new_table_root(&mut wp);
        // 5001 is prime; i*104729 mod 5001 is a permutation of 0..5000.
        let n: i64 = 5001;
        for i in 0..n {
            let rowid = (i.wrapping_mul(104729)).rem_euclid(n) + 1;
            let rec = encode_record(&[Value::Integer(rowid), Value::Integer(rowid * 3)]);
            insert_table(&mut wp, root, rowid, &rec).unwrap();
        }
        // Scan back: strictly ascending, all present, payloads intact.
        let mut cur = TableCursor::new(&wp, root);
        let mut prev = 0i64;
        let mut count = 0i64;
        let mut ok = cur.first().unwrap();
        while ok {
            let rid = cur.rowid().unwrap();
            assert!(rid > prev, "out of order: {rid} after {prev}");
            prev = rid;
            let cols = decode_record(&cur.payload().unwrap(), TextEncoding::Utf8).unwrap();
            assert_eq!(cols[1], Value::Integer(rid * 3));
            count += 1;
            ok = cur.next().unwrap();
        }
        assert_eq!(count, n);
        // Compactness: a fragmented tree would use thousands of pages for 5001
        // tiny rows on 512-byte pages; a well-filled one uses ~120-150.
        assert!(
            wp.page_count() < 250,
            "tree fragmented: {} pages for {n} rows",
            wp.page_count()
        );
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
