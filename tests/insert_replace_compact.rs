//! Regression: `INSERT OR REPLACE` on a table with a secondary index left the
//! table b-tree with empty non-root leaf pages.
//!
//! An `INSERT OR REPLACE` that lands on existing rows deletes each conflicting
//! row (via `delete_row_cascade`) before inserting the replacement. When the
//! replacement values are larger than the originals (a fragment-cache pattern:
//! overwrite a key with a bigger value), the deletes can empty out table-b-tree
//! leaf pages that SQLite's balancer would merge away. The plain UPDATE and
//! DELETE paths already ran `compact_table` for exactly this reason, but the
//! INSERT-OR-REPLACE path only rebuilt the indexes — so the emptied leaves
//! survived, producing a file that graphite's own `integrity_check` reported as
//! `empty non-root leaf page N` and that sqlite3 rejected as
//! `database disk image is malformed`. The integer-PK case did not reproduce it
//! (no secondary index / different conflict path); a TEXT PRIMARY KEY (which
//! creates `sqlite_autoindex`) did.
//!
//! The fix compacts the table b-tree after a replace, matching the UPDATE/DELETE
//! paths. These tests drive the minimal repro plus a realistic mixed cache
//! workload and assert the file stays valid under graphite's `integrity_check`,
//! under sqlite3's `quick_check` (when available), and that the surviving row
//! set matches sqlite3 byte-for-byte (all values are deterministic — no
//! `randomblob`).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Load `sql` into a fresh db file with `bin` and return nothing; the db path is
/// `dir/name.db`.
fn load(bin: &str, db: &str, sql: &str) {
    let _ = std::fs::remove_file(db);
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("run binary");
    assert!(
        out.status.success(),
        "load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn query(bin: &str, db: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(db).arg(sql).output().expect("query");
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

fn tmp(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphite_irc_{}_{}.db", std::process::id(), name));
    p.to_string_lossy().into_owned()
}

/// 300 rows keyed by a TEXT PRIMARY KEY, then `n` of them replaced with a
/// `sz`-byte blob — the minimal shape that produced empty leaves.
fn replace_script(base: usize, n: usize, sz: usize) -> String {
    format!(
        "PRAGMA page_size=4096;\
         CREATE TABLE t(k TEXT PRIMARY KEY, v BLOB);\
         WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{base})\
           INSERT INTO t SELECT 'key'||x, zeroblob(120) FROM c;\
         INSERT OR REPLACE INTO t \
           SELECT 'key'||x, zeroblob({sz}) \
           FROM (WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{n}) SELECT x FROM c);"
    )
}

fn assert_valid_and_matching(name: &str, sql: &str) {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let gdb = tmp(&format!("g_{name}"));
    load(g, &gdb, sql);

    let integ = query(g, &gdb, "PRAGMA integrity_check");
    assert_eq!(integ, "ok", "[{name}] graphite integrity_check: {integ}");

    if sqlite3_available() {
        // sqlite3 must accept graphite's file …
        let qc = query("sqlite3", &gdb, "PRAGMA quick_check");
        assert_eq!(
            qc, "ok",
            "[{name}] sqlite3 quick_check on graphite file: {qc}"
        );

        // … and the surviving row set must match a sqlite3 run of the same script.
        let sdb = tmp(&format!("s_{name}"));
        load("sqlite3", &sdb, sql);
        let probe = "SELECT k, length(v) FROM t ORDER BY k";
        let gr = query(g, &gdb, probe);
        let sr = query("sqlite3", &sdb, probe);
        assert_eq!(gr, sr, "[{name}] row set diverged from sqlite3");
        let _ = std::fs::remove_file(&sdb);
    }
    let _ = std::fs::remove_file(&gdb);
}

#[test]
fn insert_or_replace_growing_rows_keeps_btree_valid() {
    // Below and above the ~50-replace threshold, small and overflow-sized values.
    assert_valid_and_matching("r50_6k", &replace_script(300, 50, 6000));
    assert_valid_and_matching("r150_6k", &replace_script(300, 150, 6000));
    assert_valid_and_matching("r150_2k", &replace_script(300, 150, 2000));
    assert_valid_and_matching("r299_4k", &replace_script(300, 299, 4000));
}

#[test]
fn insert_or_replace_mixed_cache_workload_stays_valid() {
    // The workload that first surfaced it: auto_vacuum, delete-half via a
    // self-subquery, then INSERT OR REPLACE half the keys to 6 KB, then prune.
    let sql = "PRAGMA auto_vacuum=FULL;\
        PRAGMA page_size=4096;\
        CREATE TABLE cache(k TEXT PRIMARY KEY, v BLOB);\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<1200)\
          INSERT INTO cache(k,v) SELECT 'key'||x, zeroblob(100+x*5) FROM c;\
        DELETE FROM cache WHERE k IN(SELECT k FROM cache WHERE rowid%2=0);\
        INSERT OR REPLACE INTO cache(k,v) \
          SELECT 'key'||x, zeroblob(6000) \
          FROM (WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<600) SELECT x FROM c);\
        DELETE FROM cache WHERE length(v)<500;";
    // Same probe under a different table name.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let gdb = tmp("g_cache");
    load(g, &gdb, sql);
    let integ = query(g, &gdb, "PRAGMA integrity_check");
    assert_eq!(integ, "ok", "graphite integrity_check: {integ}");
    if sqlite3_available() {
        let qc = query("sqlite3", &gdb, "PRAGMA quick_check");
        assert_eq!(qc, "ok", "sqlite3 quick_check on graphite file: {qc}");
        let sdb = tmp("s_cache");
        load("sqlite3", &sdb, sql);
        let probe = "SELECT k, length(v) FROM cache ORDER BY k";
        assert_eq!(
            query(g, &gdb, probe),
            query("sqlite3", &sdb, probe),
            "cache row set diverged from sqlite3"
        );
        let _ = std::fs::remove_file(&sdb);
    }
    let _ = std::fs::remove_file(&gdb);
}
