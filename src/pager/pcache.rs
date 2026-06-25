//! A bounded page cache with LRU eviction (ROADMAP **C8c-1**).
//!
//! The pager historically kept *every* page it had read in an unbounded map, so
//! a long read-heavy workload grew the resident page set without bound. This
//! module is the abstraction that fixes that: a fixed-capacity map of
//! `page number → page bytes` that **evicts the least-recently-used entry** once
//! a new insert would exceed the configured capacity.
//!
//! # Safety model (matches SQLite)
//!
//! Only ever store **clean** pages here. A clean page can be dropped at any time
//! and simply re-read from disk on the next access, so eviction is completely
//! transparent — results are unchanged. A *dirty* page (modified in the current
//! transaction, not yet committed) must never live here, because evicting it
//! would lose the write; the write pager keeps dirty pages in a separate overlay
//! that this cache never touches.
//!
//! # Capacity
//!
//! The capacity follows the `cache_size` PRAGMA convention:
//!
//! * a **positive** value is a number of pages;
//! * a **negative** value is an amount of memory in **KiB**, converted to a page
//!   count using the page size (`KiB * 1024 / page_size`);
//! * SQLite's default is `-2000` (≈ 2 MiB).
//!
//! The capacity is always at least one page, so a single hot page is never
//! thrashed.
//!
//! # LRU policy
//!
//! Recency is tracked with a monotonically increasing tick stamped on every
//! access (insert or hit). Eviction scans for the entry with the smallest tick.
//! Capacities are small (hundreds to a few thousand pages by default), so a
//! linear scan on the rare eviction path is cheap and keeps the structure
//! dependency-free and `no_std`-friendly.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;

/// SQLite's default `cache_size`: `-2000`, i.e. ~2 MiB of page cache.
pub const DEFAULT_CACHE_SIZE: i64 = -2000;

/// One cached page: its bytes plus the recency tick of its last access.
struct Entry {
    data: Rc<Vec<u8>>,
    last_used: u64,
}

/// A bounded LRU cache of clean page images, keyed by 1-based page number.
pub struct PageCache {
    map: BTreeMap<u32, Entry>,
    /// Maximum number of resident pages. Always `>= 1`.
    capacity: usize,
    /// Monotonic recency clock; each access takes the next value.
    clock: u64,
}

impl PageCache {
    /// Create a cache sized from a `cache_size` value and the page size.
    ///
    /// See the module docs for the sign convention. `page_size` is the database
    /// page size in bytes (used to turn a negative, KiB-denominated `cache_size`
    /// into a page count).
    pub fn new(cache_size: i64, page_size: usize) -> PageCache {
        PageCache {
            map: BTreeMap::new(),
            capacity: capacity_for(cache_size, page_size),
            clock: 0,
        }
    }

    /// Reconfigure the capacity from a new `cache_size` value, evicting the
    /// least-recently-used entries immediately if the cache is now over budget.
    pub fn set_cache_size(&mut self, cache_size: i64, page_size: usize) {
        self.capacity = capacity_for(cache_size, page_size);
        self.evict_to_capacity();
    }

    /// The maximum number of resident pages.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of pages currently resident.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Next recency tick.
    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// Look up page `number`, marking it most-recently-used on a hit.
    pub fn get(&mut self, number: u32) -> Option<Rc<Vec<u8>>> {
        let t = self.tick();
        let entry = self.map.get_mut(&number)?;
        entry.last_used = t;
        Some(Rc::clone(&entry.data))
    }

    /// Insert (or refresh) page `number` with `data`, evicting the
    /// least-recently-used page first if the cache is at capacity.
    ///
    /// Only clean pages may be inserted — see the module docs.
    pub fn insert(&mut self, number: u32, data: Rc<Vec<u8>>) {
        let t = self.tick();
        // Make room for a genuinely new key (refreshing an existing key does not
        // grow the resident set).
        if !self.map.contains_key(&number) {
            while self.map.len() >= self.capacity {
                if !self.evict_one() {
                    break;
                }
            }
        }
        self.map.insert(number, Entry { data, last_used: t });
    }

    /// Drop a single cached page if present (e.g. it just became dirty, or its
    /// on-disk contents changed). A no-op if the page is not resident.
    pub fn invalidate(&mut self, number: u32) {
        self.map.remove(&number);
    }

    /// Drop every cached page (e.g. after a commit or a geometry change makes the
    /// whole snapshot potentially stale).
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Evict the single least-recently-used entry. Returns `false` if the cache
    /// is empty (nothing to evict).
    fn evict_one(&mut self) -> bool {
        let victim = self
            .map
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(&n, _)| n);
        match victim {
            Some(n) => {
                self.map.remove(&n);
                true
            }
            None => false,
        }
    }

    /// Evict least-recently-used entries until the resident set fits the
    /// capacity (used after the capacity is lowered).
    fn evict_to_capacity(&mut self) {
        while self.map.len() > self.capacity {
            if !self.evict_one() {
                break;
            }
        }
    }
}

/// Convert a `cache_size` value (SQLite convention) and a page size into a page
/// capacity of at least one page.
fn capacity_for(cache_size: i64, page_size: usize) -> usize {
    let pages = if cache_size >= 0 {
        cache_size as u64
    } else {
        // Negative: |cache_size| KiB of memory, divided by the page size.
        let kib = (cache_size as i128).unsigned_abs() as u64;
        let bytes = kib.saturating_mul(1024);
        let ps = page_size.max(1) as u64;
        bytes / ps
    };
    (pages as usize).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn page(byte: u8, size: usize) -> Rc<Vec<u8>> {
        Rc::new(vec![byte; size])
    }

    #[test]
    fn positive_cache_size_is_a_page_count() {
        let c = PageCache::new(10, 4096);
        assert_eq!(c.capacity(), 10);
    }

    #[test]
    fn negative_cache_size_is_kib_of_memory() {
        // -2000 KiB / 4096 bytes per page = 500 pages.
        let c = PageCache::new(-2000, 4096);
        assert_eq!(c.capacity(), 500);
        // -2000 KiB / 1024 bytes per page = 2000 pages.
        let c = PageCache::new(-2000, 1024);
        assert_eq!(c.capacity(), 2000);
    }

    #[test]
    fn capacity_is_at_least_one() {
        assert_eq!(PageCache::new(0, 4096).capacity(), 1);
        assert_eq!(PageCache::new(-1, 65536).capacity(), 1);
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut c = PageCache::new(2, 4096);
        c.insert(1, page(1, 4096));
        c.insert(2, page(2, 4096));
        // Touch page 1 so page 2 is now the LRU victim.
        assert!(c.get(1).is_some());
        c.insert(3, page(3, 4096));
        assert_eq!(c.len(), 2);
        assert!(c.get(1).is_some(), "recently used page 1 survived");
        assert!(c.get(2).is_none(), "LRU page 2 was evicted");
        assert!(c.get(3).is_some(), "newly inserted page 3 present");
    }

    #[test]
    fn never_exceeds_capacity() {
        let mut c = PageCache::new(3, 4096);
        for n in 1..=100u32 {
            c.insert(n, page(n as u8, 4096));
            assert!(c.len() <= 3, "resident set stayed within capacity");
        }
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn refresh_does_not_grow_or_evict() {
        let mut c = PageCache::new(2, 4096);
        c.insert(1, page(1, 4096));
        c.insert(2, page(2, 4096));
        // Re-inserting an existing key must not evict the other resident page.
        c.insert(1, page(9, 4096));
        assert_eq!(c.len(), 2);
        assert!(c.get(2).is_some());
        assert_eq!(c.get(1).unwrap()[0], 9);
    }

    #[test]
    fn shrinking_capacity_evicts_immediately() {
        let mut c = PageCache::new(5, 4096);
        for n in 1..=5u32 {
            c.insert(n, page(n as u8, 4096));
        }
        assert_eq!(c.len(), 5);
        c.set_cache_size(2, 4096);
        assert_eq!(c.capacity(), 2);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn invalidate_and_clear() {
        let mut c = PageCache::new(4, 4096);
        c.insert(1, page(1, 4096));
        c.insert(2, page(2, 4096));
        c.invalidate(1);
        assert!(c.get(1).is_none());
        assert!(c.get(2).is_some());
        c.clear();
        assert!(c.is_empty());
    }
}
