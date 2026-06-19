//! Cursors that walk table and index b-trees, plus overflow-chain reassembly.
//!
//! [`TableCursor`] iterates a table b-tree in rowid order and can seek to a
//! rowid in `O(height)`. [`IndexCursor`] iterates an index b-tree in key order;
//! because index interior cells are themselves entries, its traversal yields
//! interior payloads interleaved with child subtrees.
//!
//! Both reassemble payloads that spill onto overflow pages.

use super::page::{BtreePage, Payload};
use crate::error::{Error, Result};
use crate::pager::PageSource;
use alloc::vec::Vec;

/// Reassemble a full payload, following the overflow-page chain if needed.
///
/// Overflow pages are a 4-byte "next page" pointer followed by up to
/// `usable - 4` payload bytes.
pub fn read_payload(
    pager: &dyn PageSource,
    page_bytes: &[u8],
    payload: &Payload,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(payload.total_len);
    out.extend_from_slice(
        &page_bytes[payload.local_offset..payload.local_offset + payload.local_len],
    );

    let usable = pager.usable_size();
    let mut next = payload.overflow;
    while next != 0 && out.len() < payload.total_len {
        let page = pager.page(next)?;
        let data = page.data();
        let next_ptr = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let remaining = payload.total_len - out.len();
        let take = remaining.min(usable - 4);
        if 4 + take > data.len() {
            return Err(Error::Corrupt("overflow page too small".into()));
        }
        out.extend_from_slice(&data[4..4 + take]);
        next = next_ptr;
    }

    if out.len() != payload.total_len {
        return Err(Error::Corrupt("overflow chain shorter than payload".into()));
    }
    Ok(out)
}

struct Frame {
    page: BtreePage,
    /// For a leaf: index of the current cell. For an interior page: the child
    /// position we descended into (so ascending can advance to the next).
    idx: usize,
}

/// A cursor over a table b-tree, iterating rows in ascending rowid order.
pub struct TableCursor<'p> {
    pager: &'p dyn PageSource,
    root: u32,
    stack: Vec<Frame>,
    exhausted: bool,
}

impl<'p> TableCursor<'p> {
    /// Create a cursor over the table b-tree rooted at page `root`. The cursor
    /// is unpositioned until [`first`](Self::first) or [`seek`](Self::seek).
    pub fn new(pager: &'p dyn PageSource, root: u32) -> TableCursor<'p> {
        TableCursor {
            pager,
            root,
            stack: Vec::new(),
            exhausted: true,
        }
    }

    /// Position at the last (highest-rowid) row. Returns `false` if empty. Useful
    /// for finding the next auto-assigned rowid in `O(height)`.
    pub fn last(&mut self) -> Result<bool> {
        self.stack.clear();
        self.exhausted = false;
        let mut page_no = self.root;
        loop {
            let page = BtreePage::parse(self.pager.page(page_no)?)?;
            if page.page_type().is_leaf() {
                let n = page.num_cells();
                if n == 0 {
                    self.stack.push(Frame { page, idx: 0 });
                    self.exhausted = true;
                    return Ok(false);
                }
                self.stack.push(Frame { page, idx: n - 1 });
                return Ok(true);
            }
            let nc = page.num_cells();
            let child = page.child_pointer(nc)?; // right-most pointer
            self.stack.push(Frame { page, idx: nc });
            page_no = child;
        }
    }

    /// Position at the first (lowest-rowid) row. Returns `false` if the table is
    /// empty.
    pub fn first(&mut self) -> Result<bool> {
        self.stack.clear();
        self.exhausted = false;
        self.descend_left(self.root)?;
        // The leftmost leaf may be empty (e.g. after deletes); settle onto the
        // first real cell, skipping empty leaves in tree order.
        self.settle()
    }

    /// Ensure the cursor's top frame is a leaf positioned at a valid cell,
    /// advancing in tree order past any exhausted or empty leaves. Returns
    /// `false` (and marks the cursor exhausted) if no more rows remain.
    fn settle(&mut self) -> Result<bool> {
        loop {
            if let Some(top) = self.stack.last() {
                if top.idx < top.page.num_cells() {
                    return Ok(true);
                }
            } else {
                self.exhausted = true;
                return Ok(false);
            }
            // No valid cell at the current leaf: pop and walk up to the next
            // child, then descend to its leftmost leaf and re-check.
            self.stack.pop();
            let mut descended = false;
            while let Some(top) = self.stack.last_mut() {
                top.idx += 1;
                if top.idx <= top.page.num_cells() {
                    let child = top.page.child_pointer(top.idx)?;
                    self.descend_left(child)?;
                    descended = true;
                    break;
                }
                self.stack.pop();
            }
            if !descended {
                self.exhausted = true;
                return Ok(false);
            }
        }
    }

    /// Whether the cursor currently points at a valid row.
    pub fn is_valid(&self) -> bool {
        !self.exhausted
    }

    /// The rowid at the cursor.
    pub fn rowid(&self) -> Result<i64> {
        let frame = self.leaf_frame()?;
        Ok(frame
            .page
            .table_leaf_cell(frame.idx, self.pager.usable_size())?
            .rowid)
    }

    /// The full row record (payload) at the cursor.
    pub fn payload(&self) -> Result<Vec<u8>> {
        let frame = self.leaf_frame()?;
        let cell = frame
            .page
            .table_leaf_cell(frame.idx, self.pager.usable_size())?;
        read_payload(self.pager, frame.page.data(), &cell.payload)
    }

    /// Advance to the next row. Returns `false` when the cursor moves past the
    /// last row.
    #[allow(clippy::should_implement_trait)] // a fallible cursor, not an Iterator
    pub fn next(&mut self) -> Result<bool> {
        if self.exhausted {
            return Ok(false);
        }
        // Advance within the current leaf, then settle onto the next real cell
        // (which may require skipping exhausted or empty leaves in tree order).
        self.stack
            .last_mut()
            .expect("positioned cursor has frames")
            .idx += 1;
        self.settle()
    }

    /// Seek to `target` rowid, positioning at the smallest rowid `>= target`.
    /// Returns `true` if an exact match exists.
    pub fn seek(&mut self, target: i64) -> Result<bool> {
        self.stack.clear();
        self.exhausted = false;
        let mut page_no = self.root;
        loop {
            let page = BtreePage::parse(self.pager.page(page_no)?)?;
            if page.page_type().is_leaf() {
                let n = page.num_cells();
                // First cell whose rowid >= target.
                let mut idx = 0;
                let mut exact = false;
                while idx < n {
                    let rid = page.table_leaf_cell(idx, self.pager.usable_size())?.rowid;
                    if rid >= target {
                        exact = rid == target;
                        break;
                    }
                    idx += 1;
                }
                let past_end = idx >= n;
                self.stack.push(Frame { page, idx });
                if past_end {
                    // target is greater than everything in this leaf; advance to
                    // the next entry in tree order (if any).
                    let _ = self.next()?;
                    return Ok(false);
                }
                return Ok(exact);
            } else {
                // Interior table: descend at the first cell whose key >= target,
                // else the right pointer.
                let n = page.num_cells();
                let mut i = 0;
                while i < n {
                    if target <= page.table_interior_key(i)? {
                        break;
                    }
                    i += 1;
                }
                let child = page.child_pointer(i)?;
                self.stack.push(Frame { page, idx: i });
                page_no = child;
            }
        }
    }

    /// Push frames from `page_no` down to the leftmost leaf.
    fn descend_left(&mut self, mut page_no: u32) -> Result<()> {
        loop {
            let page = BtreePage::parse(self.pager.page(page_no)?)?;
            let leaf = page.page_type().is_leaf();
            if leaf {
                self.stack.push(Frame { page, idx: 0 });
                return Ok(());
            }
            let child = page.child_pointer(0)?;
            self.stack.push(Frame { page, idx: 0 });
            page_no = child;
        }
    }

    fn leaf_frame(&self) -> Result<&Frame> {
        if self.exhausted {
            return Err(Error::Error("cursor not positioned on a row".into()));
        }
        self.stack
            .last()
            .ok_or_else(|| Error::Error("cursor has no current frame".into()))
    }
}

/// A cursor over an index b-tree, iterating index keys in tree order.
///
/// Index interior cells are real entries, so the in-order sequence for an
/// interior page with `c` cells is:
/// `child0, cell0, child1, cell1, …, child(c-1), cell(c-1), child(c)`.
/// We encode position with a per-frame step counter: even steps descend a
/// child, odd steps yield a cell.
pub struct IndexCursor<'p> {
    pager: &'p dyn PageSource,
    root: u32,
    stack: Vec<IndexFrame>,
    started: bool,
}

struct IndexFrame {
    page: BtreePage,
    step: usize,
}

impl<'p> IndexCursor<'p> {
    /// Create a cursor over the index b-tree rooted at page `root`.
    pub fn new(pager: &'p dyn PageSource, root: u32) -> IndexCursor<'p> {
        IndexCursor {
            pager,
            root,
            stack: Vec::new(),
            started: false,
        }
    }

    /// Return the next index-key payload in key order, or `None` at the end.
    #[allow(clippy::should_implement_trait)] // a fallible cursor, not an Iterator
    pub fn next(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.started {
            self.started = true;
            let root = BtreePage::parse(self.pager.page(self.root)?)?;
            self.stack.push(IndexFrame {
                page: root,
                step: 0,
            });
        }

        let usable = self.pager.usable_size();
        loop {
            let top = match self.stack.last_mut() {
                Some(t) => t,
                None => return Ok(None),
            };

            if top.page.page_type().is_leaf() {
                if top.step < top.page.num_cells() {
                    let i = top.step;
                    top.step += 1;
                    let cell = top.page.index_cell(i, usable)?;
                    let payload = read_payload(self.pager, top.page.data(), &cell.payload)?;
                    return Ok(Some(payload));
                }
                self.stack.pop();
                continue;
            }

            // Interior index page.
            let c = top.page.num_cells();
            if top.step > 2 * c {
                self.stack.pop();
                continue;
            }
            let step = top.step;
            top.step += 1;
            if step % 2 == 0 {
                // Descend child step/2.
                let k = step / 2;
                let child = top.page.child_pointer(k)?;
                let child_page = BtreePage::parse(self.pager.page(child)?)?;
                self.stack.push(IndexFrame {
                    page: child_page,
                    step: 0,
                });
            } else {
                // Yield cell step/2.
                let k = step / 2;
                let cell = top.page.index_cell(k, usable)?;
                let payload = read_payload(self.pager, top.page.data(), &cell.payload)?;
                return Ok(Some(payload));
            }
        }
    }
}
