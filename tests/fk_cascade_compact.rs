//! Regression: an `ON DELETE CASCADE` (or `INSERT OR REPLACE` conflict-delete
//! that cascades) left empty non-root leaf pages in the CHILD table b-tree.
//!
//! Deleting parent rows removes matching child rows via `delete_row_cascade`.
//! When the child rows are large, those deletes empty out child-table leaf pages
//! that SQLite's balancer would merge away. The top-level DML compacted only its
//! own target table (the parent), never the cascade-affected children — so the
//! child b-tree kept the emptied leaves: graphite's `integrity_check` reported
//! `<child>: empty non-root leaf page N` and sqlite3 rejected the file as
//! `database disk image is malformed`. (Same class as the INSERT OR REPLACE
//! compaction bug, one level down the FK graph.)
//!
//! The fix records every table touched by a cascading delete and compacts each
//! once when the statement finishes. These tests drive cascades one and three
//! levels deep, plus an INSERT OR REPLACE whose conflict-delete cascades, and
//! assert the file stays valid (graphite integrity_check + sqlite3 quick_check)
//! with a surviving row set identical to sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn load(bin: &str, db: &str, sql: &str) {
    let _ = std::fs::remove_file(db);
    let out = Command::new(bin).arg(db).arg(sql).output().expect("run");
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
    p.push(format!("graphite_fkcc_{}_{}.db", std::process::id(), name));
    p.to_string_lossy().into_owned()
}

/// Load `sql` with graphite; assert graphite integrity_check = ok, sqlite3
/// quick_check on graphite's file = ok, and each `probe` row set matches a
/// sqlite3 run of the same script.
fn assert_valid_and_matching(name: &str, sql: &str, probes: &[&str]) {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let gdb = tmp(&format!("g_{name}"));
    load(g, &gdb, sql);
    assert_eq!(
        query(g, &gdb, "PRAGMA integrity_check"),
        "ok",
        "[{name}] graphite integrity_check"
    );
    if sqlite3_available() {
        assert_eq!(
            query("sqlite3", &gdb, "PRAGMA quick_check"),
            "ok",
            "[{name}] sqlite3 quick_check on graphite file"
        );
        let sdb = tmp(&format!("s_{name}"));
        load("sqlite3", &sdb, sql);
        for probe in probes {
            assert_eq!(
                query(g, &gdb, probe),
                query("sqlite3", &sdb, probe),
                "[{name}] row set diverged from sqlite3 for `{probe}`"
            );
        }
        let _ = std::fs::remove_file(&sdb);
    }
    let _ = std::fs::remove_file(&gdb);
}

#[test]
fn on_delete_cascade_keeps_child_btree_valid() {
    let sql = "PRAGMA foreign_keys=ON;\
        PRAGMA page_size=4096;\
        CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT);\
        CREATE TABLE ch(id INTEGER PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE, data BLOB);\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<100)\
          INSERT INTO p SELECT x, 'p'||x FROM c;\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<800)\
          INSERT INTO ch SELECT x, (x%100)+1, zeroblob(2000+x) FROM c;\
        DELETE FROM p WHERE id%2=0;";
    assert_valid_and_matching(
        "cascade1",
        sql,
        &[
            "SELECT count(*) FROM p",
            "SELECT count(*), coalesce(sum(length(data)),0) FROM ch",
        ],
    );
}

#[test]
fn three_level_cascade_keeps_all_btrees_valid() {
    let sql = "PRAGMA foreign_keys=ON;\
        PRAGMA page_size=4096;\
        CREATE TABLE a(id INTEGER PRIMARY KEY);\
        CREATE TABLE b(id INTEGER PRIMARY KEY, aid INT REFERENCES a(id) ON DELETE CASCADE, v BLOB);\
        CREATE TABLE d(id INTEGER PRIMARY KEY, bid INT REFERENCES b(id) ON DELETE CASCADE, v BLOB);\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<50) INSERT INTO a SELECT x FROM c;\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<400)\
          INSERT INTO b SELECT x, (x%50)+1, zeroblob(1500) FROM c;\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<600)\
          INSERT INTO d SELECT x, (x%400)+1, zeroblob(1800) FROM c;\
        DELETE FROM a WHERE id<40;";
    assert_valid_and_matching(
        "cascade3",
        sql,
        &[
            "SELECT count(*) FROM a",
            "SELECT count(*) FROM b",
            "SELECT count(*) FROM d",
        ],
    );
}

#[test]
fn insert_or_replace_conflict_cascade_keeps_child_valid() {
    // Replacing a parent row (same PK) deletes the old parent, cascading to its
    // big-row children, then re-inserts the parent.
    let sql = "PRAGMA foreign_keys=ON;\
        PRAGMA page_size=4096;\
        CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT);\
        CREATE TABLE ch(id INTEGER PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE, data BLOB);\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<80)\
          INSERT INTO p SELECT x, 'p'||x FROM c;\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<700)\
          INSERT INTO ch SELECT x, (x%80)+1, zeroblob(2500) FROM c;\
        INSERT OR REPLACE INTO p SELECT x, 'r'||x FROM (WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<80) SELECT x FROM c);";
    assert_valid_and_matching(
        "replace_cascade",
        sql,
        &["SELECT count(*), sum(id) FROM p", "SELECT count(*) FROM ch"],
    );
}
