//! Pointer-map (ptrmap) page math and entry format for `auto_vacuum`.
//!
//! This module is pure, self-contained groundwork for the auto-vacuum write
//! path (roadmap C6b). It implements the page arithmetic and the 5-byte entry
//! encoding exactly as SQLite defines them in `btree.c`, but performs no I/O and
//! is not yet wired into the executor.
//!
//! # Pointer-map layout
//!
//! When auto-vacuum is enabled, the database interleaves *pointer-map* pages
//! among the data pages. Each ptrmap page holds an array of 5-byte entries, one
//! per page it tracks, recording the *type* and *parent* of that page so the
//! vacuum can relocate pages without walking the whole tree.
//!
//! Let `usable_size` be the page size minus the per-page reserved bytes (see
//! [`DatabaseHeader::usable_size`](crate::format::header::DatabaseHeader::usable_size)). Each
//! ptrmap page covers `n = usable_size / 5` data pages.
//!
//! - The first ptrmap page is **page 2**, tracking pages `3 ..= n + 2`.
//! - The next ptrmap page is `n + 3`, tracking `n + 4 ..= 2n + 3`, and so on:
//!   ptrmap pages recur every `n + 1` pages.
//! - Page 1 (the database header page) is never tracked and is never a ptrmap
//!   page.
//!
//! # Entry format
//!
//! A 5-byte entry is `[type_byte, parent_be_u32]`: one type byte
//! ([`PtrmapType`]) followed by a 4-byte big-endian parent page number (`0` when
//! the type has no parent).

/// Number of bytes in a single pointer-map entry.
pub const ENTRY_SIZE: usize = 5;

/// The type of a page as recorded in its pointer-map entry.
///
/// Mirrors the `PTRMAP_*` constants in SQLite's `btree.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtrmapType {
    /// The page is a table or index b-tree root page. Parent is `0`.
    RootPage,
    /// The page is on the freelist. Parent is `0`.
    FreePage,
    /// The page is the first page of an overflow chain. Parent is the b-tree
    /// page holding the cell that owns the chain.
    Overflow1,
    /// The page is a later page in an overflow chain. Parent is the previous
    /// page in the chain.
    Overflow2,
    /// The page is a non-root b-tree page. Parent is its parent b-tree page.
    Btree,
}

impl PtrmapType {
    /// Returns the on-disk type byte for this entry type.
    pub fn as_u8(self) -> u8 {
        match self {
            PtrmapType::RootPage => 1,
            PtrmapType::FreePage => 2,
            PtrmapType::Overflow1 => 3,
            PtrmapType::Overflow2 => 4,
            PtrmapType::Btree => 5,
        }
    }

    /// Parses an on-disk type byte, returning `None` for unknown values.
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(PtrmapType::RootPage),
            2 => Some(PtrmapType::FreePage),
            3 => Some(PtrmapType::Overflow1),
            4 => Some(PtrmapType::Overflow2),
            5 => Some(PtrmapType::Btree),
            _ => None,
        }
    }
}

/// Number of data pages tracked by a single ptrmap page.
///
/// This is `usable_size / ENTRY_SIZE`, the number of 5-byte entries that fit on
/// one ptrmap page.
fn entries_per_page(usable_size: u32) -> u32 {
    usable_size / ENTRY_SIZE as u32
}

/// Returns the page number of the ptrmap page that holds the entry for `pgno`.
///
/// `pgno` is assumed to be a tracked data page (i.e. not page 1 and not a
/// ptrmap page itself). Mirrors SQLite's `ptrmapPageno`.
///
/// The result is always page 2 or the start of a later `n + 1`-page cycle.
pub fn ptrmap_pageno(usable_size: u32, pgno: u32) -> u32 {
    let n = entries_per_page(usable_size);
    // `n + 1` is the length of one cycle: one ptrmap page plus the `n` pages it
    // tracks. Page numbering is 1-based with page 1 excluded, so we work in
    // `pgno - 1` space (matching SQLite's `iPtrMap = (pgno - 2) / (nPagesPerMapPage)`
    // formulation). The ptrmap page for `pgno` is the first page of the cycle
    // that `pgno` falls in.
    let cycle = n + 1;
    // Offset of pgno within the post-header region, 0-based: page 2 -> 0.
    let offset = pgno - 2;
    let cycle_index = offset / cycle;
    cycle_index * cycle + 2
}

/// Returns `true` if `pgno` is itself a pointer-map page.
///
/// Page 1 is never a ptrmap page.
pub fn is_ptrmap_page(usable_size: u32, pgno: u32) -> bool {
    if pgno < 2 {
        return false;
    }
    ptrmap_pageno(usable_size, pgno) == pgno
}

/// Returns the byte offset of `pgno`'s 5-byte entry within its ptrmap page.
///
/// `pgno` must be a tracked data page (not page 1 and not a ptrmap page).
pub fn ptrmap_entry_offset(usable_size: u32, pgno: u32) -> usize {
    let map = ptrmap_pageno(usable_size, pgno);
    // The page immediately after the ptrmap page (`map + 1`) is the first page
    // it tracks, occupying entry slot 0.
    let slot = pgno - (map + 1);
    slot as usize * ENTRY_SIZE
}

/// Encodes a pointer-map entry: one type byte plus a big-endian parent page
/// number.
pub fn encode_entry(kind: PtrmapType, parent: u32) -> [u8; ENTRY_SIZE] {
    let p = parent.to_be_bytes();
    [kind.as_u8(), p[0], p[1], p[2], p[3]]
}

/// Decodes a pointer-map entry into its type and parent page number.
///
/// Returns `None` if the type byte is not a recognized [`PtrmapType`].
pub fn decode_entry(entry: &[u8; ENTRY_SIZE]) -> Option<(PtrmapType, u32)> {
    let kind = PtrmapType::from_u8(entry[0])?;
    let parent = u32::from_be_bytes([entry[1], entry[2], entry[3], entry[4]]);
    Some((kind, parent))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 4096-byte usable size -> n = 819 entries per ptrmap page.
    const USABLE_4K: u32 = 4096;
    const N_4K: u32 = 819; // 4096 / 5

    #[test]
    fn entries_per_page_matches_spec() {
        assert_eq!(entries_per_page(USABLE_4K), N_4K);
        assert_eq!(entries_per_page(512), 102); // 512 / 5 = 102
    }

    #[test]
    fn page2_is_first_ptrmap_page() {
        assert!(is_ptrmap_page(USABLE_4K, 2));
    }

    #[test]
    fn page1_and_page3_not_ptrmap() {
        assert!(!is_ptrmap_page(USABLE_4K, 1));
        assert!(!is_ptrmap_page(USABLE_4K, 3));
    }

    #[test]
    fn page3_entry_lives_on_page2_at_offset_0() {
        assert_eq!(ptrmap_pageno(USABLE_4K, 3), 2);
        assert_eq!(ptrmap_entry_offset(USABLE_4K, 3), 0);
    }

    #[test]
    fn first_ptrmap_covers_run_then_recurs() {
        // Page 2 tracks pages 3 ..= n + 2.
        let last_tracked = N_4K + 2;
        assert_eq!(ptrmap_pageno(USABLE_4K, last_tracked), 2);
        assert_eq!(
            ptrmap_entry_offset(USABLE_4K, last_tracked),
            (N_4K as usize - 1) * ENTRY_SIZE
        );
        // The next page (n + 3) is itself the next ptrmap page.
        let next_map = N_4K + 3;
        assert!(is_ptrmap_page(USABLE_4K, next_map));
        assert_eq!(ptrmap_pageno(USABLE_4K, next_map), next_map);
    }

    #[test]
    fn second_ptrmap_run() {
        // The second ptrmap page is n + 3; it tracks n + 4 ..= 2n + 3.
        let map2 = N_4K + 3;
        let first = map2 + 1; // n + 4
        let last = 2 * N_4K + 3;
        assert_eq!(ptrmap_pageno(USABLE_4K, first), map2);
        assert_eq!(ptrmap_entry_offset(USABLE_4K, first), 0);
        assert_eq!(ptrmap_pageno(USABLE_4K, last), map2);
        assert_eq!(
            ptrmap_entry_offset(USABLE_4K, last),
            (N_4K as usize - 1) * ENTRY_SIZE
        );
        // The page after that run is the third ptrmap page (2n + 4).
        let map3 = 2 * N_4K + 4;
        assert!(is_ptrmap_page(USABLE_4K, map3));
    }

    #[test]
    fn straddling_a_ptrmap_boundary() {
        // Small usable size to make boundaries easy to reason about.
        // usable = 20 -> n = 4 entries per page, cycle = 5.
        let usable = 20;
        assert_eq!(entries_per_page(usable), 4);
        // Cycle: ptrmap page 2 tracks 3,4,5,6; page 7 is next ptrmap tracking
        // 8,9,10,11; page 12 next, etc.
        assert!(is_ptrmap_page(usable, 2));
        for p in 3..=6 {
            assert_eq!(ptrmap_pageno(usable, p), 2, "page {p}");
            assert!(!is_ptrmap_page(usable, p));
        }
        assert!(is_ptrmap_page(usable, 7));
        for p in 8..=11 {
            assert_eq!(ptrmap_pageno(usable, p), 7, "page {p}");
            assert!(!is_ptrmap_page(usable, p));
        }
        assert!(is_ptrmap_page(usable, 12));
        // Offsets within page 7's run.
        assert_eq!(ptrmap_entry_offset(usable, 8), 0);
        assert_eq!(ptrmap_entry_offset(usable, 9), ENTRY_SIZE);
        assert_eq!(ptrmap_entry_offset(usable, 11), 3 * ENTRY_SIZE);
    }

    #[test]
    fn type_byte_round_trip() {
        for kind in [
            PtrmapType::RootPage,
            PtrmapType::FreePage,
            PtrmapType::Overflow1,
            PtrmapType::Overflow2,
            PtrmapType::Btree,
        ] {
            assert_eq!(PtrmapType::from_u8(kind.as_u8()), Some(kind));
        }
    }

    #[test]
    fn type_byte_values_match_sqlite() {
        assert_eq!(PtrmapType::RootPage.as_u8(), 1);
        assert_eq!(PtrmapType::FreePage.as_u8(), 2);
        assert_eq!(PtrmapType::Overflow1.as_u8(), 3);
        assert_eq!(PtrmapType::Overflow2.as_u8(), 4);
        assert_eq!(PtrmapType::Btree.as_u8(), 5);
    }

    #[test]
    fn unknown_type_byte_rejected() {
        assert_eq!(PtrmapType::from_u8(0), None);
        assert_eq!(PtrmapType::from_u8(6), None);
        assert_eq!(PtrmapType::from_u8(255), None);
    }

    #[test]
    fn entry_encode_decode_round_trip() {
        let parents = [
            0u32,
            1,
            2,
            255,
            256,
            65535,
            65536,
            819,
            0x0123_4567,
            u32::MAX,
        ];
        for kind in [
            PtrmapType::RootPage,
            PtrmapType::FreePage,
            PtrmapType::Overflow1,
            PtrmapType::Overflow2,
            PtrmapType::Btree,
        ] {
            for &parent in &parents {
                let enc = encode_entry(kind, parent);
                assert_eq!(decode_entry(&enc), Some((kind, parent)));
            }
        }
    }

    #[test]
    fn encode_layout_is_type_then_be_parent() {
        let enc = encode_entry(PtrmapType::Overflow1, 0x0102_0304);
        assert_eq!(enc, [3, 0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn decode_rejects_bad_type() {
        let bad = [9u8, 0, 0, 0, 1];
        assert_eq!(decode_entry(&bad), None);
    }
}
