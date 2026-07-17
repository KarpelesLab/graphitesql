//! Whole-file page accounting for `PRAGMA integrity_check` — a port of the
//! `IntegrityCk.aPgRef` bitmap protocol from SQLite's `btree.c`
//! (`sqlite3BtreeIntegrityCheck`, `checkTreePage`, `checkList`, `checkRef`,
//! `checkPtrmap`).
//!
//! A per-tree structural walk alone cannot see *cross-tree* damage: a page
//! referenced by two b-trees, a live page that is also on the freelist, a page
//! no tree references at all (an orphan), or a freelist whose traversal
//! disagrees with the header's count. SQLite catches all of these with one
//! shared reference bitmap over pages `1..=page_count`: every reachable page —
//! the b-tree interior/leaf pages of every root, the overflow chain of every
//! spilled cell, the freelist trunk and leaf pages, and the locking page — must
//! be marked **exactly once**. A second mark reports `2nd reference to page N`,
//! an out-of-range pointer reports `invalid page number N`, and any page left
//! unmarked at the end reports `Page N: never used` (pointer-map pages are the
//! expected exception under auto-vacuum). The message strings follow SQLite's;
//! tree-context messages carry graphite's `<name>: ` label prefix in place of
//! SQLite's `Tree N page M [cell C]: ` prefix.
//!
//! [`super::Connection::pragma_integrity_check`] drives this module once per
//! check, walking page 1 (the `sqlite_schema` tree) plus every root page in the
//! catalog through one shared [`PageAccounting`].

use crate::btree::ptrmap::{self, PtrmapType};
use crate::btree::{BtreePage, PageType};
use crate::pager::PageSource;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// SQLite's `PENDING_BYTE`: the file offset (1 GiB) whose page is reserved for
/// the pending-lock byte and is never used to store content.
const PENDING_BYTE: u64 = 0x4000_0000;

/// Read a big-endian `u32`, returning 0 (never panicking) past the buffer end.
#[inline]
fn be_u32(buf: &[u8], at: usize) -> u32 {
    match buf.get(at..at + 4) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

/// The shared page-reference bitmap and the checks built on it — SQLite's
/// `IntegrityCk` accounting state.
///
/// Construct one per `PRAGMA integrity_check`, feed it the freelist, the
/// auto-vacuum header expectations, and every b-tree root, then finish with
/// [`check_never_used`](Self::check_never_used). All problem messages append to
/// the caller's list, capped at its `max_err` (SQLite's `mxErr`).
pub(super) struct PageAccounting<'a> {
    src: &'a dyn PageSource,
    /// Total pages in the database (`IntegrityCk.nCkPage`).
    n_page: u32,
    /// Usable bytes per page (page size minus reserved space).
    usable: usize,
    /// Whether the file is in auto-vacuum mode (pointer-map pages present).
    auto_vacuum: bool,
    /// First freelist trunk page, from the live page-1 header (offset 32).
    freelist_trunk: u32,
    /// Total freelist pages, from the live page-1 header (offset 36).
    freelist_count: u32,
    /// The header's largest-root-page field (offset 52; non-zero = auto-vacuum).
    largest_root_page: u32,
    /// The header's incremental-vacuum flag (offset 64).
    incremental_vacuum: u32,
    /// One bit per page in `1..=n_page` (`IntegrityCk.aPgRef`).
    refs: Vec<u8>,
    /// Stop appending messages once the caller's list holds this many.
    max_err: usize,
}

impl<'a> PageAccounting<'a> {
    /// Set up the bitmap for `src`, pre-marking the locking page (the page
    /// containing the `PENDING_BYTE` at 1 GiB) exactly as
    /// `sqlite3BtreeIntegrityCheck` does — no b-tree may use it, and the final
    /// sweep must not flag it as never used.
    ///
    /// The freelist head/count and vacuum fields are read from the **current
    /// page 1** (like sqlite, which never trusts a cached header for these):
    /// another in-process connection may have committed since this
    /// connection's parsed-header snapshot was taken, and the page cache — but
    /// not that snapshot — is revalidated at statement boundaries.
    pub(super) fn new(src: &'a dyn PageSource, max_err: usize) -> PageAccounting<'a> {
        let n_page = src.page_count();
        let header = src.header();
        let (freelist_trunk, freelist_count, largest_root_page, incremental_vacuum) =
            match src.page(1) {
                Ok(p) => {
                    let d = p.data();
                    (be_u32(d, 32), be_u32(d, 36), be_u32(d, 52), be_u32(d, 64))
                }
                Err(_) => (
                    header.freelist_trunk,
                    header.freelist_count,
                    header.largest_root_page,
                    header.incremental_vacuum,
                ),
            };
        let mut acct = PageAccounting {
            src,
            n_page,
            usable: src.usable_size(),
            auto_vacuum: largest_root_page != 0,
            freelist_trunk,
            freelist_count,
            largest_root_page,
            incremental_vacuum,
            refs: alloc::vec![0u8; n_page as usize / 8 + 1],
            max_err,
        };
        let pending_page = PENDING_BYTE / u64::from(header.page_size.max(1)) + 1;
        if pending_page <= u64::from(n_page) {
            acct.set(pending_page as u32);
        }
        acct
    }

    #[inline]
    fn get(&self, pg: u32) -> bool {
        self.refs[pg as usize / 8] & (1 << (pg & 7)) != 0
    }

    #[inline]
    fn set(&mut self, pg: u32) {
        self.refs[pg as usize / 8] |= 1 << (pg & 7);
    }

    /// Whether the message budget is exhausted (SQLite's `mxErr==0`).
    fn full(&self, problems: &[String]) -> bool {
        problems.len() >= self.max_err
    }

    /// Append `msg` unless the budget is exhausted (SQLite's `checkAppendMsg`).
    fn push(&self, problems: &mut Vec<String>, msg: String) {
        if !self.full(problems) {
            problems.push(msg);
        }
    }

    /// SQLite's `checkRef`: mark `pg` used, reporting a `2nd reference to page
    /// N` when already marked and `invalid page number N` when out of range.
    /// Returns `true` when the caller must not descend into the page.
    fn check_ref(&mut self, pg: u32, prefix: &str, problems: &mut Vec<String>) -> bool {
        if pg == 0 || pg > self.n_page {
            self.push(problems, format!("{prefix}invalid page number {pg}"));
            return true;
        }
        if self.get(pg) {
            self.push(problems, format!("{prefix}2nd reference to page {pg}"));
            return true;
        }
        self.set(pg);
        false
    }

    /// Walk the freelist from the database header, marking every trunk and
    /// leaf page and verifying the traversal covers exactly the header's
    /// `freelist_count` pages (SQLite's `checkList(isFreeList=1)` call).
    pub(super) fn check_freelist(&mut self, problems: &mut Vec<String>) {
        let (trunk, count) = (self.freelist_trunk, self.freelist_count);
        self.check_list(true, trunk, count, "Freelist: ", problems);
    }

    /// The auto-vacuum header cross-checks from `sqlite3BtreeIntegrityCheck`:
    /// in auto-vacuum mode the header's largest-root-page field must equal the
    /// largest root in the catalog; outside it the incremental-vacuum flag must
    /// be clear.
    pub(super) fn check_rootpage_header(&mut self, max_root: u32, problems: &mut Vec<String>) {
        if self.auto_vacuum {
            if max_root != self.largest_root_page {
                self.push(
                    problems,
                    format!(
                        "max rootpage ({max_root}) disagrees with header ({})",
                        self.largest_root_page
                    ),
                );
            }
        } else if self.incremental_vacuum != 0 {
            self.push(
                problems,
                String::from("incremental_vacuum enabled with a max rootpage of zero"),
            );
        }
    }

    /// SQLite's `checkList`: follow a page chain — the freelist (trunk pages
    /// carrying arrays of leaf page numbers) or a cell's overflow chain —
    /// marking every page, and verify it holds exactly `expected` pages.
    fn check_list(
        &mut self,
        is_freelist: bool,
        first: u32,
        expected: u32,
        prefix: &str,
        problems: &mut Vec<String>,
    ) {
        let mut n = i64::from(expected);
        let err_start = problems.len();
        let mut page_no = first;
        while page_no != 0 && !self.full(problems) {
            if self.check_ref(page_no, prefix, problems) {
                break;
            }
            n -= 1;
            let page = match self.src.page(page_no) {
                Ok(p) => p,
                Err(_) => {
                    self.push(problems, format!("{prefix}failed to get page {page_no}"));
                    break;
                }
            };
            let data = page.data();
            if is_freelist {
                // A trunk page: [next trunk, leaf count, leaf page numbers...].
                let leaves = be_u32(data, 4);
                if self.auto_vacuum {
                    self.check_ptrmap(page_no, PtrmapType::FreePage, 0, prefix, problems);
                }
                if leaves as usize > self.usable / 4 - 2 {
                    self.push(
                        problems,
                        format!("{prefix}freelist leaf count too big on page {page_no}"),
                    );
                    n -= 1;
                } else {
                    for i in 0..leaves as usize {
                        let leaf = be_u32(data, 8 + i * 4);
                        if self.auto_vacuum {
                            self.check_ptrmap(leaf, PtrmapType::FreePage, 0, prefix, problems);
                        }
                        self.check_ref(leaf, prefix, problems);
                    }
                    n -= i64::from(leaves);
                }
            } else if self.auto_vacuum && n > 0 {
                // Not the last overflow page: the next page's ptrmap entry must
                // point back at this one.
                let next = be_u32(data, 0);
                self.check_ptrmap(next, PtrmapType::Overflow2, page_no, prefix, problems);
            }
            page_no = be_u32(data, 0);
        }
        // Only report a length mismatch when the traversal itself was clean
        // (matching sqlite: a broken chain already got its own message).
        if n != 0 && problems.len() == err_start {
            self.push(
                problems,
                format!(
                    "{prefix}{} is {} but should be {expected}",
                    if is_freelist {
                        "size"
                    } else {
                        "overflow list length"
                    },
                    i64::from(expected) - n
                ),
            );
        }
    }

    /// Check one b-tree: structural validity (the pre-existing per-tree checks)
    /// plus whole-file accounting — every page of the tree and every overflow
    /// chain hanging off its cells is marked in the shared bitmap (SQLite's
    /// per-root `checkTreePage` call, including the root ptrmap check).
    pub(super) fn check_tree(&mut self, root: u32, label: &str, problems: &mut Vec<String>) {
        if self.auto_vacuum && root > 1 {
            self.check_ptrmap(
                root,
                PtrmapType::RootPage,
                0,
                &format!("{label}: "),
                problems,
            );
        }
        self.walk(root, true, label, problems);
    }

    /// Recursive page walk (SQLite's `checkTreePage`). Returns the subtree
    /// height (leaves return 0) so sibling depths can be compared; error paths
    /// return 0, like sqlite.
    fn walk(
        &mut self,
        page_no: u32,
        is_root: bool,
        label: &str,
        problems: &mut Vec<String>,
    ) -> i32 {
        if self.full(problems) {
            return 0;
        }
        let prefix = format!("{label}: ");
        if self.check_ref(page_no, &prefix, problems) {
            return 0;
        }
        let page = match self.src.page(page_no) {
            Ok(p) => p,
            Err(_) => {
                self.push(problems, format!("{label}: unable to read page {page_no}"));
                return 0;
            }
        };
        let bt = match BtreePage::parse(page) {
            Ok(b) => b,
            Err(_) => {
                self.push(
                    problems,
                    format!("{label}: page {page_no} is not a valid b-tree page"),
                );
                return 0;
            }
        };

        // An empty non-root leaf — the shape a non-compacting delete leaves —
        // is a malformed sqlite b-tree (its count-based checks would miss it).
        if bt.page_type().is_leaf() && !is_root && bt.num_cells() == 0 {
            self.push(
                problems,
                format!("{label}: empty non-root leaf page {page_no}"),
            );
        }

        // Account for the overflow chain of every payload-bearing cell: the
        // chain's pages are reachable from this cell and from nowhere else.
        // Expected length per sqlite: ceil((payload - local) / (usable - 4)).
        for i in 0..bt.num_cells() {
            if self.full(problems) {
                return 0;
            }
            let payload = match bt.page_type() {
                PageType::LeafTable => match bt.table_leaf_cell(i, self.usable) {
                    Ok(c) => Some(c.payload),
                    Err(_) => {
                        self.push(
                            problems,
                            format!("{label}: invalid cell {i} on page {page_no}"),
                        );
                        None
                    }
                },
                PageType::LeafIndex | PageType::InteriorIndex => {
                    match bt.index_cell(i, self.usable) {
                        Ok(c) => Some(c.payload),
                        Err(_) => {
                            self.push(
                                problems,
                                format!("{label}: invalid cell {i} on page {page_no}"),
                            );
                            None
                        }
                    }
                }
                // Table-interior cells carry no payload.
                PageType::InteriorTable => None,
            };
            if let Some(p) = payload
                && p.overflow != 0
            {
                let expected = (p.total_len.saturating_sub(p.local_len)
                    + self.usable.saturating_sub(5))
                    / self.usable.saturating_sub(4).max(1);
                if self.auto_vacuum {
                    self.check_ptrmap(
                        p.overflow,
                        PtrmapType::Overflow1,
                        page_no,
                        &prefix,
                        problems,
                    );
                }
                self.check_list(false, p.overflow, expected as u32, &prefix, problems);
            }
        }

        // Recurse into children; all subtrees must have the same height.
        let mut depth: i32 = -1;
        if !bt.page_type().is_leaf() {
            let mut first_child = true;
            for i in 0..=bt.num_cells() {
                if self.full(problems) {
                    return 0;
                }
                match bt.child_pointer(i) {
                    Ok(child) => {
                        if self.auto_vacuum {
                            self.check_ptrmap(child, PtrmapType::Btree, page_no, &prefix, problems);
                        }
                        let d2 = self.walk(child, false, label, problems);
                        if !first_child && d2 != depth {
                            self.push(problems, format!("{label}: Child page depth differs"));
                        }
                        depth = d2;
                        first_child = false;
                    }
                    Err(_) => self.push(
                        problems,
                        format!("{label}: unreadable child pointer on page {page_no}"),
                    ),
                }
            }
        }
        depth + 1
    }

    /// The final sweep from `sqlite3BtreeIntegrityCheck`: any page in range
    /// that no traversal marked is reported `Page N: never used` — except
    /// pointer-map pages under auto-vacuum, which are expected to be unmarked
    /// (and conversely must never be referenced by a tree).
    pub(super) fn check_never_used(&mut self, problems: &mut Vec<String>) {
        for pg in 1..=self.n_page {
            if self.full(problems) {
                break;
            }
            let referenced = self.get(pg);
            let is_ptrmap = self.auto_vacuum && ptrmap::is_ptrmap_page(self.usable as u32, pg);
            if !referenced && !is_ptrmap {
                problems.push(format!("Page {pg}: never used"));
            } else if referenced && is_ptrmap {
                problems.push(format!("Page {pg}: pointer map referenced"));
            }
        }
    }

    /// SQLite's `checkPtrmap`: page `key`'s pointer-map entry must record
    /// exactly (`expected_type`, `expected_parent`). Only called under
    /// auto-vacuum.
    fn check_ptrmap(
        &mut self,
        key: u32,
        expected_type: PtrmapType,
        expected_parent: u32,
        prefix: &str,
        problems: &mut Vec<String>,
    ) {
        match self.ptrmap_get(key) {
            None => self.push(problems, format!("{prefix}Failed to read ptrmap key={key}")),
            Some((got_type, got_parent)) => {
                if got_type != expected_type.as_u8() || got_parent != expected_parent {
                    self.push(
                        problems,
                        format!(
                            "{prefix}Bad ptr map entry key={key} expected=({},{expected_parent}) got=({got_type},{got_parent})",
                            expected_type.as_u8()
                        ),
                    );
                }
            }
        }
    }

    /// Read page `key`'s raw pointer-map entry (SQLite's `ptrmapGet`). `None`
    /// for anything unreadable: `key` out of range, `key` itself a ptrmap page
    /// (or page 1), or an entry with an unknown type byte.
    fn ptrmap_get(&self, key: u32) -> Option<(u8, u32)> {
        if key < 2 || key > self.n_page {
            return None;
        }
        let map = ptrmap::ptrmap_pageno(self.usable as u32, key);
        if key <= map {
            return None;
        }
        let off = (key - map - 1) as usize * ptrmap::ENTRY_SIZE;
        if off + ptrmap::ENTRY_SIZE > self.usable {
            return None;
        }
        let page = self.src.page(map).ok()?;
        let data = page.data();
        let entry = data.get(off..off + ptrmap::ENTRY_SIZE)?;
        if entry[0] < 1 || entry[0] > 5 {
            return None;
        }
        Some((entry[0], be_u32(entry, 1)))
    }
}
