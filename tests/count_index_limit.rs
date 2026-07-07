//! `SELECT count(*)` over a table with a secondary index takes a fast path that
//! counts the index's entries instead of scanning the table. That path returned
//! the single aggregate row unconditionally, ignoring `LIMIT`/`OFFSET` — so
//! `SELECT count(*) FROM t LIMIT 0` wrongly returned the count instead of no
//! rows, and `… OFFSET 1` returned the row instead of skipping it. The fast path
//! now applies LIMIT/OFFSET to its single row. Verified byte-for-byte against the
//! sqlite3 3.50.4 CLI (found by a cross-feature fuzzer).

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
fn count_via_index_honors_limit_offset() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // A single-column (non-covering) index triggers the count-the-index fast path.
    let base = "CREATE TABLE t(a,b,c);CREATE INDEX ix ON t(c);\
        INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);";
    let tails = [
        "SELECT count(*) FROM t LIMIT 0",
        "SELECT count(*) FROM t LIMIT 1",
        "SELECT count(*) FROM t LIMIT 5",
        "SELECT count(*) FROM t LIMIT -1",
        "SELECT count(*) FROM t LIMIT 5 OFFSET 0",
        "SELECT count(*) FROM t LIMIT 5 OFFSET 1",
        "SELECT count(*) FROM t OFFSET 1",
        "SELECT count(*) FROM t LIMIT (SELECT 0)",
        "SELECT count(*) FROM t",
        // covering-index and no-index count paths, same LIMIT rule
        "SELECT count(*) FROM t LIMIT 0 OFFSET 0",
    ];
    let mut sql = String::new();
    for t in tails {
        sql.push_str(base);
        sql.push_str(t);
        sql.push_str(";DROP TABLE t;");
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));

    // covering index over (a,b) — different count path, same LIMIT semantics
    let cov = "CREATE TABLE t(a,b);CREATE INDEX ix ON t(a,b);INSERT INTO t VALUES(1,2),(3,4);\
        SELECT count(*) FROM t LIMIT 0;";
    assert_eq!(out("sqlite3", cov), out(g, cov));
}
