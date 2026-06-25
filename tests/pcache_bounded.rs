//! ROADMAP C8c — the bounded page cache with LRU eviction.
//!
//! These tests prove the pager's resident page set stays at/under the configured
//! `cache_size` while a workload touches far more pages than the limit, that the
//! results are unchanged by eviction+re-read (so eviction is transparent), and
//! that a *dirty* (uncommitted) page is never evicted.

#![cfg(feature = "std")]

use graphitesql::pager::{PageSource, Pager, WritePager};
use graphitesql::vfs::{std_file::StdVfs, OpenFlags, Vfs};
use graphitesql::{Connection, Value};

/// A per-PID, per-test scratch directory under `/tmp`, created fresh. The `tag`
/// keeps parallel tests from clobbering each other's files (and cleanup).
fn scratch_dir(tag: &str) -> String {
    let dir = format!("/tmp/gsql-c8c-{}/{tag}", std::process::id());
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Build a multi-page database file with `rows` rows and return its path plus the
/// directory to clean up.
fn build_db(tag: &str, rows: i64) -> (String, String) {
    let dir = scratch_dir(tag);
    let path = format!("{dir}/{tag}.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));

    let mut conn = Connection::create(&path).expect("create db");
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT, n INTEGER)")
        .expect("create table");
    conn.execute("BEGIN").expect("begin");
    for i in 1..=rows {
        conn.execute(&format!(
            "INSERT INTO t(id, v, n) VALUES({i}, 'row-{i}-payload-padding-to-make-pages-fill', {})",
            i * 7
        ))
        .expect("insert");
    }
    conn.execute("COMMIT").expect("commit");
    (path, dir)
}

#[test]
fn resident_set_stays_under_cache_size_during_scans() {
    let rows = 5000;
    let (path, dir) = build_db("scan", rows);

    // Sanity: the database is genuinely many pages, and integrity holds.
    {
        let conn = Connection::open(&path).expect("open");
        let q = conn.query("PRAGMA page_count").expect("page_count");
        let page_count = match &q.rows[0][0] {
            Value::Integer(n) => *n,
            other => panic!("page_count not an integer: {other:?}"),
        };
        assert!(
            page_count > 50,
            "expected a multi-page db, got {page_count} pages"
        );
        let ic = conn.query("PRAGMA integrity_check").expect("integrity");
        assert_eq!(ic.rows[0][0], Value::Text("ok".into()));
    }

    // Open the read pager over the file and pin its bounded cache to a tiny bound.
    let vfs = StdVfs::new();
    let file = vfs.open(&path, OpenFlags::READ_ONLY).expect("open ro");
    let pager = Pager::open(file).expect("open pager");
    let total = pager.page_count();
    assert!(total > 50);

    let cap_pages = 16i64;
    pager.set_cache_size(cap_pages);

    // Reference images read once.
    let reference: Vec<Vec<u8>> = (1..=total)
        .map(|n| pager.page(n).expect("page").data().to_vec())
        .collect();
    // The cache holds at most the bound even after reading every page once.
    assert!(pager.resident_pages() <= cap_pages as usize);

    // Repeated full scans + pseudo-random point lookups, far more accesses than
    // the bound. After every access the resident set must respect the bound, and
    // every page must read back byte-identically (eviction+re-read transparent).
    let mut probe = 1u32;
    for _pass in 0..5 {
        for n in 1..=total {
            let got = pager.page(n).expect("scan page");
            assert_eq!(
                got.data(),
                reference[(n - 1) as usize].as_slice(),
                "page {n} bytes changed under eviction+re-read"
            );
            assert!(
                pager.resident_pages() <= cap_pages as usize,
                "resident pages {} exceeded bound {cap_pages}",
                pager.resident_pages()
            );
        }
        for _ in 0..200 {
            probe = (probe.wrapping_mul(1103515245).wrapping_add(12345)) % total + 1;
            let got = pager.page(probe).expect("point page");
            assert_eq!(got.data(), reference[(probe - 1) as usize].as_slice());
            assert!(pager.resident_pages() <= cap_pages as usize);
        }
    }

    // The cache actually filled up to (but not past) the bound.
    assert_eq!(
        pager.resident_pages(),
        cap_pages as usize,
        "a hot scan should fill the cache to capacity"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn correct_under_extreme_single_page_cache() {
    let rows = 3000;
    let (path, dir) = build_db("tiny", rows);

    let vfs = StdVfs::new();
    let file = vfs.open(&path, OpenFlags::READ_ONLY).expect("open ro");
    let pager = Pager::open(file).expect("open pager");
    let total = pager.page_count();

    // Capture ground truth before clamping the cache.
    let reference: Vec<Vec<u8>> = (1..=total)
        .map(|n| pager.page(n).expect("page").data().to_vec())
        .collect();

    // The most adversarial bound: a single resident page. Every access but the
    // immediate repeat is a miss + re-read, yet results must be identical.
    pager.set_cache_size(1);
    assert_eq!(
        pager.resident_pages(),
        1,
        "shrinking evicts down to the bound"
    );
    for n in 1..=total {
        let got = pager.page(n).expect("page");
        assert_eq!(got.data(), reference[(n - 1) as usize].as_slice());
        assert!(
            pager.resident_pages() <= 1,
            "single-page cache held {} pages",
            pager.resident_pages()
        );
    }

    // SQL-level correctness over the same file (a fresh connection) is unchanged.
    let conn = Connection::open(&path).expect("open conn");
    let agg = conn.query("SELECT COUNT(*), SUM(n) FROM t").expect("agg");
    let want_sum: i64 = (1..=rows).map(|i| i * 7).sum();
    assert_eq!(agg.rows[0][0], Value::Integer(rows));
    assert_eq!(agg.rows[0][1], Value::Integer(want_sum));
    let ic = conn.query("PRAGMA integrity_check").expect("integrity");
    assert_eq!(ic.rows[0][0], Value::Text("ok".into()));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn dirty_page_is_never_evicted_midtransaction() {
    let rows = 4000;
    let (path, dir) = build_db("dirty", rows);

    let vfs = StdVfs::new();
    let file = vfs.open(&path, OpenFlags::READ_WRITE).expect("open rw");
    let journal = vfs
        .open(&format!("{path}-journal"), OpenFlags::READ_WRITE_CREATE)
        .expect("open journal");
    let mut pager = WritePager::open(file, Some(journal)).expect("open pager");
    let total = pager.page_count();

    // Tiny clean cache so a scan thrashes it.
    pager.set_cache_size(8);

    // Stage a distinctive dirty page mid-transaction (we will ROLLBACK, so the
    // file on disk is never corrupted). Take an existing page and flip its bytes
    // to a recognizable pattern.
    let dirty_pgno = total; // last page
    let original = pager
        .page(dirty_pgno)
        .expect("read original")
        .data()
        .to_vec();
    let mut dirty = original.clone();
    for b in dirty.iter_mut() {
        *b ^= 0xFF;
    }
    pager
        .write_page(dirty_pgno, dirty.clone())
        .expect("stage dirty page");

    let dirty_count = pager.resident_dirty_pages();
    assert!(dirty_count >= 1);

    // Thrash the clean cache by reading many *other* pages, far more than the
    // bound. The dirty page must remain readable with the staged content the
    // whole time — it lives in the overlay, which the cache never touches.
    for _pass in 0..3 {
        for n in 1..dirty_pgno {
            let _ = pager.page(n).expect("scan");
            assert!(
                pager.resident_clean_pages() <= 8,
                "clean cache exceeded bound while a dirty page was pending"
            );
            let d = pager.page(dirty_pgno).expect("dirty page");
            assert_eq!(
                d.data(),
                dirty.as_slice(),
                "the uncommitted dirty page was lost / evicted"
            );
            assert_eq!(
                pager.resident_dirty_pages(),
                dirty_count,
                "the dirty overlay must not shrink under read pressure"
            );
        }
    }

    // The clean cache really did fill to its bound during the scan (the bound is
    // non-vacuous) — and never counted the dirty page among its evictable set.
    assert_eq!(
        pager.resident_clean_pages(),
        8,
        "the clean read cache should be full at its bound after a big scan"
    );

    // Roll back: the on-disk page is untouched, so the file stays valid.
    pager.rollback();
    let restored = pager.page(dirty_pgno).expect("post-rollback read");
    assert_eq!(
        restored.data(),
        original.as_slice(),
        "rollback should restore the original page bytes"
    );
    drop(pager);

    let conn = Connection::open(&path).expect("open for integrity");
    let ic = conn.query("PRAGMA integrity_check").expect("integrity");
    assert_eq!(
        ic.rows[0][0],
        Value::Text("ok".into()),
        "database remained valid after staging + rolling back a dirty page"
    );

    let _ = std::fs::remove_dir_all(dir);
}
