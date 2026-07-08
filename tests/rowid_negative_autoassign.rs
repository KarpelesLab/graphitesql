//! An auto-assigned `INTEGER PRIMARY KEY` (or implicit rowid) takes the value
//! `max_rowid + 1`, where the maximum reflects rows already inserted — including
//! *negative* explicit rowids earlier in the same multi-row statement. graphite
//! floored the candidate at the empty-table default of 1, so `INSERT INTO t
//! VALUES(-1),(NULL)` produced rowid 1 for the second row instead of sqlite's 0.
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn negative_explicit_rowid_then_auto() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Explicit -1 then auto: next is 0.
        "CREATE TABLE t(a INTEGER PRIMARY KEY);INSERT INTO t VALUES(-1),(NULL);\
         SELECT a FROM t ORDER BY a;",
        // Several negatives then two autos count up from the max.
        "CREATE TABLE t(a INTEGER PRIMARY KEY);INSERT INTO t VALUES(-5),(NULL),(NULL);\
         SELECT a FROM t ORDER BY a;",
        // OR IGNORE variant.
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b);INSERT OR IGNORE INTO t VALUES(-1,0),(NULL,1);\
         SELECT a FROM t ORDER BY a;",
        // A NULL before the negative: the empty table still starts auto at 1.
        "CREATE TABLE t(a INTEGER PRIMARY KEY);INSERT INTO t VALUES(NULL),(-1),(NULL);\
         SELECT a FROM t ORDER BY a;",
        // A positive explicit rowid still advances the counter.
        "CREATE TABLE t(a INTEGER PRIMARY KEY);INSERT INTO t VALUES(5),(NULL);\
         SELECT a FROM t ORDER BY a;",
        // Implicit rowid (no INTEGER PRIMARY KEY alias), via the rowid pseudo-column.
        "CREATE TABLE t(x);INSERT INTO t(rowid,x) VALUES(-1,'a');INSERT INTO t(x) VALUES('b');\
         SELECT rowid FROM t ORDER BY rowid;",
        // AUTOINCREMENT is unaffected: it never drops to/below the high-water mark.
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT);INSERT INTO t VALUES(-1),(NULL);\
         SELECT a FROM t ORDER BY a;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for `{sql}`");
    }
}
