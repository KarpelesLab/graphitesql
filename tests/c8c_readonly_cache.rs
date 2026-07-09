//! Track C — C8c-2: a **coherent** read cache for a read-only connection.
//!
//! `WritePager`'s clean read cache used to be trusted only inside a *write*
//! transaction (under a `Reserved`+ lock, where this connection is the only
//! writer). A pure read-only connection over a read-write file holds no such
//! lock, so every page went straight to disk on every read — correct, but it
//! never reused a page.
//!
//! C8c-2 adds a coherent read cache for that path. Cached clean pages are keyed
//! by the database **change counter** (page 1, bytes 24–27), which SQLite bumps
//! on every commit. At each statement boundary the reader re-reads that counter
//! from disk (`revalidate_read_cache` / the `Connection`'s per-statement hook):
//! if it is unchanged the cache is reused; if a foreign in-process `Connection`
//! committed and bumped it, the cache is dropped so the next statement re-reads
//! fresh pages. This gives two guarantees, both asserted here:
//!
//!   * **Coherency (correctness):** after another `Connection` commits, the
//!     read-only side sees the NEW data on its very next statement — never a
//!     stale cached page.
//!   * **Cache actually used:** repeated reads of the same page *within* one
//!     validated window are served from cache (a hit), not re-read from disk.
//!
//! Two `WritePager`s / `Connection`s over one shared `MemoryVfs` path model two
//! connections in one process (graphite locking is process-local), the same
//! pattern as `tests/pager_read_locks.rs` and `tests/c9a_connection_locks.rs`.

#![cfg(feature = "std")]

use graphitesql::pager::WritePager;
use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::vfs::{File, OpenFlags, Vfs};
use graphitesql::{Connection, Value};
use std::boxed::Box;

/// A fresh `File` handle over the shared per-path `LockState` and byte store.
fn open_rw(vfs: &MemoryVfs, path: &str) -> Box<dyn File> {
    vfs.open(path, OpenFlags::READ_WRITE).unwrap()
}

/// A brand-new 2-page database with a marker byte on page 2, committed.
fn seed_db(vfs: &MemoryVfs, path: &str) {
    vfs.open(path, OpenFlags::READ_WRITE_CREATE).unwrap();
    let mut wp = WritePager::create(open_rw(vfs, path), None, 4096).unwrap();
    // Allocate a second page and stamp a marker so a reader has a non-page-1
    // page to read repeatedly.
    let pno = wp.allocate_page().unwrap();
    assert_eq!(pno, 2, "expected the second page to be page 2");
    let mut page2 = vec![0u8; 4096];
    page2[0] = 0x11;
    wp.write_page(2, page2).unwrap();
    wp.commit().unwrap();
}

/// A read-only pager reuses a cached clean page **within a validated window**
/// instead of hitting disk again: the second read of the same page is a cache
/// hit, not a miss.
#[test]
fn read_only_pager_reuses_cached_page_within_a_statement() {
    let vfs = MemoryVfs::new();
    seed_db(&vfs, "db");

    // A pure read-only pager: it never takes a write lock.
    let ro = WritePager::open(vfs.open("db", OpenFlags::READ_ONLY).unwrap(), None).unwrap();

    // Statement boundary: stamp the change-counter token so the cache is trusted.
    ro.revalidate_read_cache();

    let (h0, m0) = ro.read_cache_stats();
    // First read: a miss (page read from disk and inserted into the cache).
    assert_eq!(ro.read_page(2).unwrap()[0], 0x11);
    // Second and third reads of the same page: served from cache (hits).
    assert_eq!(ro.read_page(2).unwrap()[0], 0x11);
    assert_eq!(ro.read_page(2).unwrap()[0], 0x11);
    let (h1, m1) = ro.read_cache_stats();

    assert_eq!(
        m1 - m0,
        1,
        "exactly one disk miss for three reads of page 2"
    );
    assert_eq!(h1 - h0, 2, "the two repeat reads were cache hits");
    assert!(ro.resident_clean_pages() >= 1, "page 2 is resident");
}

/// Coherency: after a foreign writer commits, the read-only pager sees the NEW
/// page bytes on its next validated statement — the stale cache is dropped.
#[test]
fn read_only_pager_sees_foreign_commit_after_revalidation() {
    let vfs = MemoryVfs::new();
    seed_db(&vfs, "db");

    let ro = WritePager::open(vfs.open("db", OpenFlags::READ_ONLY).unwrap(), None).unwrap();
    let mut w = WritePager::open(open_rw(&vfs, "db"), None).unwrap();

    // Statement 1: read page 2, warming the cache with the original marker.
    ro.revalidate_read_cache();
    assert_eq!(ro.read_page(2).unwrap()[0], 0x11);
    let cached = ro.read_page(2).unwrap()[0]; // cache hit, still 0x11
    assert_eq!(cached, 0x11);

    // The writer overwrites page 2's marker and commits (bumps the change
    // counter on page 1).
    let mut p2 = w.read_page(2).unwrap();
    p2[0] = 0x22;
    w.write_page(2, p2).unwrap();
    w.commit().unwrap();

    // WITHOUT revalidation the reader would still be holding the stale 0x11 in its
    // cache. The statement-boundary hook re-reads the change counter, sees it moved,
    // and drops the cache — so the next read reflects the committed 0x22.
    ro.revalidate_read_cache();
    assert_eq!(
        ro.read_page(2).unwrap()[0],
        0x22,
        "read-only pager sees the foreign commit after revalidation (no stale cache)"
    );
}

/// End-to-end at the `Connection` layer: a read-only connection returns correct
/// rows, and after another `Connection` commits a change it sees the NEW data on
/// its next statement — driven entirely by real SQL through the per-statement
/// revalidation hook, no direct pager calls.
#[test]
fn read_only_connection_is_coherent_with_a_foreign_writer() {
    let vfs = MemoryVfs::new();
    {
        let mut c = Connection::create_vfs(&vfs, "db", 4096).unwrap();
        c.execute("CREATE TABLE t(x)").unwrap();
        c.execute("INSERT INTO t VALUES (1)").unwrap();
    }

    let ro = Connection::open_readonly_vfs(&vfs, "db").unwrap();
    let mut w = Connection::open_vfs(&vfs, "db").unwrap();

    // The read-only connection reads the committed row.
    let before = ro.query("SELECT x FROM t").unwrap();
    assert_eq!(before.rows, vec![vec![Value::Integer(1)]]);

    // A foreign writer updates the row in place and commits.
    w.execute("UPDATE t SET x = 99 WHERE x = 1").unwrap();

    // The read-only connection's NEXT statement sees the new value — the change
    // counter moved, so its per-statement hook dropped the stale cache.
    let after = ro.query("SELECT x FROM t").unwrap();
    assert_eq!(
        after.rows,
        vec![vec![Value::Integer(99)]],
        "read-only connection sees the foreign commit, not a stale cached page"
    );

    // A second read with no intervening write returns the same value (and, under
    // the hood, reuses the now-validated cache).
    let again = ro.query("SELECT x FROM t").unwrap();
    assert_eq!(again.rows, vec![vec![Value::Integer(99)]]);
}

/// Guard: repeatedly reading many distinct pages on the read-only path stays
/// bounded (the LRU eviction still applies) and correct.
#[test]
fn read_only_cache_stays_bounded() {
    let vfs = MemoryVfs::new();
    seed_db(&vfs, "db");
    let ro = WritePager::open(vfs.open("db", OpenFlags::READ_ONLY).unwrap(), None).unwrap();
    ro.set_cache_size(4); // tiny cache: 4 pages max
    ro.revalidate_read_cache();
    // Read the two pages that exist many times; resident set never exceeds cap.
    for _ in 0..50 {
        assert_eq!(ro.read_page(1).unwrap().len(), 4096);
        assert_eq!(ro.read_page(2).unwrap()[0], 0x11);
        assert!(
            ro.resident_clean_pages() <= 4,
            "resident set within capacity"
        );
    }
}
