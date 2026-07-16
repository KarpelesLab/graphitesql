//! Regression: a second `Connection` over the same database file used to keep
//! the header snapshot (freelist head/count, page count) it parsed at open
//! forever. After a *foreign* connection's commit changed the file, its next
//! write ran over that stale snapshot: `free_page` appended freelist leaf
//! entries through a stale trunk pointer — a read-modify-write of what was by
//! then a **live b-tree page**, overwriting its cell-pointer array (real
//! sqlite3 then reports `Offset X out of range` / `database disk image is
//! malformed`) — and `allocate_page` handed out page numbers the other
//! connection's trees already owned, while commit truncated the file to the
//! stale page count.
//!
//! SQLite never caches these fields across transactions: `pagerSharedLock`
//! compares the file-version bytes of page 1 on every transaction start and,
//! on a change, drops the page cache and re-reads page 1, from which
//! `lockBtree` / `allocateBtreePage` / `freePage2` read the freelist head and
//! page count fresh. The fix ports that: `WritePager::refresh_foreign_state`
//! re-derives the committed snapshot at the start of every write transaction
//! (and the statement-boundary revalidation refreshes the durable page bound
//! for reads).
#![cfg(feature = "std")]

use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::vfs::std_file::StdVfs;
use graphitesql::{Connection, Value};
use std::process::Command;

fn gcheck(c: &Connection) -> String {
    match &c.query("PRAGMA integrity_check").unwrap().rows[0][0] {
        Value::Text(s) => String::from(s.as_str()),
        _ => "?".into(),
    }
}

fn count(c: &Connection, sql: &str) -> i64 {
    match &c.query(sql).unwrap().rows[0][0] {
        Value::Integer(n) => *n,
        other => panic!("count: {other:?}"),
    }
}

/// Connection B opens while the file is small; connection A then grows the
/// file by many pages and commits. B's next INSERT must see A's grown page
/// count — a stale one made B allocate page numbers that were already live
/// b-tree pages of A's data (and truncate the file at commit).
#[test]
fn stale_page_count_two_connections() {
    let vfs = MemoryVfs::new();
    {
        let mut a = Connection::create_vfs(&vfs, "db", 4096).unwrap();
        a.execute("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        a.execute("INSERT INTO t VALUES(1, 'seed')").unwrap();
        // B opens NOW: header snapshot = tiny file.
        let mut b = Connection::open_vfs(&vfs, "db").unwrap();

        // A grows the file by hundreds of pages and commits.
        let big = "x".repeat(900);
        for k in 2..=800i64 {
            a.execute(&format!("INSERT INTO t VALUES({k}, '{big}')"))
                .unwrap();
        }
        assert_eq!(gcheck(&a), "ok", "A after growth");

        // B writes one large row (forces page allocation in B's pager).
        let payload = "y".repeat(9000);
        b.execute(&format!("INSERT INTO t VALUES(100000, '{payload}')"))
            .unwrap();

        // Both connections' views must still be intact.
        assert_eq!(gcheck(&b), "ok", "B integrity after B's insert");
        assert_eq!(gcheck(&a), "ok", "A integrity after B's insert");
        assert_eq!(count(&a, "SELECT count(*) FROM t"), 801, "A sees all rows");
    }
    // A fresh connection over the final file: nothing lost, structure valid.
    let c = Connection::open_vfs(&vfs, "db").unwrap();
    assert_eq!(gcheck(&c), "ok", "fresh connection integrity");
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 801, "no rows lost");
}

/// Freelist variant — the exact production-corruption shape: B snapshots a
/// header whose freelist head A later drains and reuses as a live b-tree
/// page. B's next DELETE (a `free_page`) used to append its freed page's
/// number into what was by then A's live page, clobbering its cell area.
#[test]
fn stale_freelist_two_connections() {
    let vfs = MemoryVfs::new();
    {
        let mut a = Connection::create_vfs(&vfs, "db", 4096).unwrap();
        a.execute("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        a.execute("CREATE TABLE u(k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        let big = "x".repeat(900);
        for k in 1..=200i64 {
            a.execute(&format!("INSERT INTO t VALUES({k}, '{big}')"))
                .unwrap();
            a.execute(&format!("INSERT INTO u VALUES({k}, '{big}')"))
                .unwrap();
        }
        // A frees pages: delete most of u -> pages hit the freelist.
        a.execute("DELETE FROM u WHERE k > 3").unwrap();
        // B opens NOW: header snapshot includes A's current freelist head.
        let mut b = Connection::open_vfs(&vfs, "db").unwrap();

        // A reuses the freelist: refill u so the freed pages are live again.
        for k in 201..=400i64 {
            a.execute(&format!("INSERT INTO u VALUES({k}, '{big}')"))
                .unwrap();
        }
        assert_eq!(gcheck(&a), "ok", "A after refill");

        // B deletes rows -> B's pager calls free_page; with the stale freelist
        // head this wrote leaf entries into one of A's live pages.
        b.execute("DELETE FROM t WHERE k <= 50").unwrap();

        assert_eq!(gcheck(&b), "ok", "B integrity after B's delete");
        assert_eq!(gcheck(&a), "ok", "A integrity after B's delete");
    }
    let c = Connection::open_vfs(&vfs, "db").unwrap();
    assert_eq!(gcheck(&c), "ok", "fresh connection integrity");
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 150, "t rows");
    assert_eq!(count(&c, "SELECT count(*) FROM u"), 203, "u rows");
}

/// On-disk end-to-end: alternate writes between two connections around
/// freelist churn, then let real sqlite3 vet the final file.
#[test]
fn alternating_writers_sqlite3_clean() {
    let dir = std::env::temp_dir();
    let path = dir
        .join(format!("gsql-stalehdr-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let vfs = StdVfs::new();
        let mut a = Connection::create_vfs(&vfs, &path, 4096).unwrap();
        a.execute("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        a.execute("CREATE INDEX iv ON t(v)").unwrap();
        let mut b = Connection::open_vfs(&vfs, &path).unwrap();
        let big = "z".repeat(700);
        let mut k = 0i64;
        for round in 0..6 {
            // A inserts a batch, then deletes half (feeding the freelist).
            for _ in 0..60 {
                k += 1;
                a.execute(&format!("INSERT INTO t VALUES({k}, '{big}_{k}')"))
                    .unwrap();
            }
            a.execute(&format!("DELETE FROM t WHERE k > {} AND k % 2 = 0", k - 60))
                .unwrap();
            // B (stale until it refreshes) inserts and deletes too.
            for _ in 0..40 {
                k += 1;
                b.execute(&format!("INSERT INTO t VALUES({k}, '{big}_{k}')"))
                    .unwrap();
            }
            b.execute(&format!("DELETE FROM t WHERE k > {} AND k % 3 = 0", k - 40))
                .unwrap();
            assert_eq!(gcheck(&a), "ok", "A integrity round {round}");
            assert_eq!(gcheck(&b), "ok", "B integrity round {round}");
        }
    }
    if Command::new("sqlite3").arg("--version").output().is_ok() {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check;")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ok",
            "sqlite3 integrity_check on the alternating-writer file"
        );
    }
    let _ = std::fs::remove_file(&path);
}
