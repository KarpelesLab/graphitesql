//! Sibling-page rebalancing math, ported faithfully from SQLite's
//! `balance_nonroot` (btree.c).
//!
//! When a b-tree page overflows, SQLite does *not* split that single page in
//! isolation. Instead it pools the overflowing page together with up to two of
//! its immediate siblings under the same parent, concatenates all their cells
//! (plus, for non-table-leaf trees, the divider cells that separate them), and
//! redistributes the whole pool across the right number of pages so each is
//! well-filled (~⅔+). This keeps the tree compact regardless of insertion order
//! — a purely greedy left-packer instead spawns 1-cell sibling pages on every
//! middle insert into a full page, fragmenting the file 10-20×.
//!
//! [`distribute`] is the size-only heart of that algorithm: given the on-page
//! byte size of every pooled cell and how the pool was split across the old
//! siblings, it returns the cell-count boundaries of the new pages. It is a
//! direct port of the `szNew[]`/`cntNew[]` computation (the greedy fill, the
//! split-more / add-more passes, and the right-to-left rebalance pass), so the
//! resulting page fill — and hence the on-disk byte layout — matches SQLite.

use alloc::vec;
use alloc::vec::Vec;

/// Faithful port of the `szNew[]`/`cntNew[]` redistribution in
/// `balance_nonroot`.
///
/// * `sz[i]` — on-page byte size of pooled cell `i`, **without** the 2-byte
///   cell-pointer (i.e. SQLite's `cachedCellSize`).
/// * `cnt_old[i]` — cumulative count of pooled cells belonging to old sibling
///   `i`, **not** counting the divider cell that follows it (for non-leaf-data
///   trees). `cnt_old.len()` is the number of old siblings (`nOld`).
/// * `leaf` — true when the siblings are leaf pages (SQLite's `leafCorrection`
///   adds 4 to the usable space in that case).
/// * `leaf_data` — true only for a **table** b-tree leaf balance, where the
///   divider cells are regenerated as rowid separators rather than pooled.
/// * `usable` — the usable page size (`pageSize - reserved`).
///
/// Returns `cnt_new`: the cumulative pooled-cell boundary of each new page.
/// `cnt_new.len()` is the number of new pages (`k`) and `cnt_new[k-1]` equals
/// `sz.len()`. Page `i` owns pooled cells `[start_i, cnt_new[i])`, where
/// `start_i` is `0` for `i == 0`, else `cnt_new[i-1]` (table leaves) or
/// `cnt_new[i-1] + 1` (the divider at `cnt_new[i-1]` is promoted to the parent,
/// not stored on a leaf).
pub(super) fn distribute(
    sz: &[usize],
    cnt_old: &[usize],
    leaf: bool,
    leaf_data: bool,
    usable: usize,
) -> Vec<usize> {
    let n_cell = sz.len();
    let n_old = cnt_old.len();
    debug_assert!(n_cell > 0 && n_old > 0);
    // usableSpace = pBt->usableSize - 12 + leafCorrection.
    let usable_space = (usable + if leaf { 4 } else { 0 }) as i64 - 12;
    let ld = usize::from(leaf_data);
    let cell = |i: usize| -> i64 { sz[i] as i64 };

    // Room for at most n_old plus a couple of extra pages.
    let cap = n_cell + 2;
    let mut sz_new = vec![0i64; cap];
    let mut cnt_new = vec![0usize; cap];

    // Seed szNew[]/cntNew[] from the old-sibling boundaries. These are only a
    // starting point; the passes below fully recompute the distribution.
    let mut k = n_old;
    for i in 0..n_old {
        // Old page i owns pooled cells [start, cnt_old[i]). For non-leaf-data
        // trees the cell at cnt_old[i-1] is the divider (promoted, not on a
        // page), so page i starts one later; table leaves have no such divider.
        let start = if i == 0 { 0 } else { cnt_old[i - 1] + (1 - ld) };
        let mut s = 0i64;
        for j in start..cnt_old[i] {
            s += cell(j) + 2;
        }
        sz_new[i] = s;
        cnt_new[i] = cnt_old[i];
    }

    // Greedy pack, biased left: fill each page to capacity, spilling to the
    // right and growing `k` when a page cannot be made to fit.
    let mut i = 0usize;
    while i < k {
        while sz_new[i] > usable_space {
            if i + 1 >= k {
                k = i + 2;
                debug_assert!(k <= cap);
                sz_new[k - 1] = 0;
                cnt_new[k - 1] = n_cell;
            }
            let mut s = 2 + cell(cnt_new[i] - 1);
            sz_new[i] -= s;
            if ld == 0 {
                s = if cnt_new[i] < n_cell {
                    2 + cell(cnt_new[i])
                } else {
                    0
                };
            }
            sz_new[i + 1] += s;
            cnt_new[i] -= 1;
        }
        while cnt_new[i] < n_cell {
            let mut s = 2 + cell(cnt_new[i]);
            if sz_new[i] + s > usable_space {
                break;
            }
            sz_new[i] += s;
            cnt_new[i] += 1;
            if ld == 0 {
                s = if cnt_new[i] < n_cell {
                    2 + cell(cnt_new[i])
                } else {
                    0
                };
            }
            sz_new[i + 1] -= s;
        }
        if cnt_new[i] >= n_cell {
            k = i + 1;
        }
        i += 1;
    }

    // Right-to-left rebalance: the greedy pack overfills the left siblings and
    // may leave the right-most nearly empty (even illegally empty). Shift cells
    // between each adjacent pair until balanced. `bBulk` is always false here.
    let mut i = k - 1;
    while i > 0 {
        let mut sz_right = sz_new[i];
        let mut sz_left = sz_new[i - 1];
        let mut r = cnt_new[i - 1] as isize - 1;
        let mut d = r + 1 - ld as isize;
        loop {
            if r < 0 {
                break;
            }
            let sz_r = cell(r as usize);
            let sz_d = cell(d as usize);
            if sz_right != 0
                && sz_right + sz_d + 2 > sz_left - (sz_r + if i == k - 1 { 0 } else { 2 })
            {
                break;
            }
            sz_right += sz_d + 2;
            sz_left -= sz_r + 2;
            cnt_new[i - 1] = r as usize;
            r -= 1;
            d -= 1;
        }
        sz_new[i] = sz_right;
        sz_new[i - 1] = sz_left;
        i -= 1;
    }

    cnt_new.truncate(k);
    cnt_new
}

/// The sibling window to balance, mirroring the `nxDiv`/`nOld` selection in
/// `balance_nonroot`. `p` is the child slot (0..=n) the overflow was inserted
/// under; `n_children` is the parent's current child count (`n + 1`). Returns
/// `(w0, n_old)`: the balance pools children `[w0, w0 + n_old)` and the
/// `n_old - 1` divider cells between them.
pub(super) fn sibling_window(p: usize, n_children: usize) -> (usize, usize) {
    let i = n_children - 1; // parent nCell
    if i < 2 {
        (0, n_children)
    } else if p == 0 {
        (0, 3)
    } else if p == i {
        (i - 2, 3)
    } else {
        (p - 1, 3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table-leaf balance of uniform cells packs pages ~evenly and never leaves
    /// a 1-cell straggler (the fragmentation bug).
    #[test]
    fn table_leaf_even_fill() {
        // Two old pages plus an overfull one; leaf_data => no dividers pooled.
        let sz: Vec<usize> = vec![9; 730];
        let cnt_old = vec![361, 550, 730];
        let cnt_new = distribute(&sz, &cnt_old, true, true, 4096);
        assert_eq!(cnt_new.last().copied(), Some(730));
        let mut prev = 0;
        let counts: Vec<usize> = cnt_new
            .iter()
            .map(|&c| {
                let n = c - prev;
                prev = c;
                n
            })
            .collect();
        let max = *counts.iter().max().unwrap();
        let min = *counts.iter().min().unwrap();
        assert!(max - min <= 1, "uneven fill: {counts:?}");
    }

    /// Index-leaf balance (dividers promoted) partitions exactly with strictly
    /// increasing boundaries.
    #[test]
    fn index_leaf_partition_exact() {
        let sz: Vec<usize> = vec![20; 400];
        let cnt_old = vec![133, 266, 400];
        let cnt_new = distribute(&sz, &cnt_old, true, false, 4096);
        assert_eq!(cnt_new.last().copied(), Some(400));
        for w in cnt_new.windows(2) {
            assert!(w[1] > w[0]);
        }
    }

    /// A single overfull page (root deepen: n_old == 1) still splits into >= 2
    /// fitting pages.
    #[test]
    fn single_page_splits() {
        let sz: Vec<usize> = vec![50; 100];
        let cnt_old = vec![100];
        let cnt_new = distribute(&sz, &cnt_old, true, true, 4096);
        assert!(cnt_new.len() >= 2);
        assert_eq!(cnt_new.last().copied(), Some(100));
    }

    #[test]
    fn window_selection() {
        assert_eq!(sibling_window(0, 1), (0, 1)); // single child (root deepen)
        assert_eq!(sibling_window(0, 2), (0, 2)); // two children
        assert_eq!(sibling_window(1, 2), (0, 2));
        assert_eq!(sibling_window(0, 5), (0, 3)); // left edge
        assert_eq!(sibling_window(4, 5), (2, 3)); // right edge
        assert_eq!(sibling_window(2, 5), (1, 3)); // middle
    }
}
