//! A secondary-index join seek must respect SQLite's `sqlite3IndexAffinityOk`:
//! an index on a column can only be seeked for an equi-join when the comparison's
//! affinity is compatible with the index column's affinity. graphite used to seek
//! the index unconditionally, so a NUMERIC comparison against an untyped (or TEXT)
//! index column — whose entries are stored as text — MISSED matches that sqlite's
//! affinity-correct comparison finds, silently returning wrong (fewer/empty) rows.
//! Now such a seek is declined (graphite scans + filters, like sqlite, which drives
//! from that table). Verified byte-for-byte against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn rows(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    // Sort so we compare the multiset (result set), independent of join order.
    let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(String::from)
        .collect();
    v.sort();
    v.join("\n")
}

#[test]
fn cross_type_index_join_seek_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // The original wrong-result repro: an INTEGER PRIMARY KEY equated to an untyped
    // (BLOB-affinity) indexed column that stores its values as text.
    let bug = "CREATE TABLE a(x0 INTEGER PRIMARY KEY, v0);\
               CREATE TABLE b(x1 INTEGER, v1 REAL);CREATE INDEX i1 ON b(x1);\
               CREATE TABLE c(x2, v2);CREATE INDEX i2 ON c(x2);\
               INSERT INTO a VALUES('2',1),('4',5);\
               INSERT INTO b VALUES('2',8),('2',2),(2,5),('3',2),('2',5);\
               INSERT INTO c VALUES('1',6),('3',1),('2',6),('4',5),('2',3);";
    let cases: &[&str] = &[
        // the bug: 3-table join, numeric IPK vs untyped index → must return all rows
        &format!("{bug}SELECT a.v0,b.v1,c.v2 FROM a JOIN b ON a.x0=b.x1 JOIN c ON a.x0=c.x2"),
        // a plain 2-table numeric-vs-untyped-index join
        "CREATE TABLE t(k INTEGER, v);CREATE TABLE u(k, w);CREATE INDEX iu ON u(k);\
         INSERT INTO t VALUES(1,'a'),(2,'b'),(3,'c');\
         INSERT INTO u VALUES('1','p'),('2','q'),('1','r'),('9','s');\
         SELECT t.v,u.w FROM t JOIN u ON t.k=u.k",
        // numeric vs TEXT index column (also declines the seek)
        "CREATE TABLE t(k INTEGER, v);CREATE TABLE u(k TEXT, w);CREATE INDEX iu ON u(k);\
         INSERT INTO t VALUES(1,'a'),(2,'b');INSERT INTO u VALUES('1','p'),('2','q'),('1','r');\
         SELECT t.v,u.w FROM t JOIN u ON t.k=u.k",
        // the 2-table REORDER-swap variant: the untyped index is on `from.first`
        // (`a`), the numeric driver is the second table (`d`) — the swap path must
        // also decline the unsound seek.
        "CREATE TABLE a(x, v);CREATE INDEX ia ON a(x);CREATE TABLE d(y INTEGER, w);\
         INSERT INTO a VALUES('1','p'),('2','q'),('1','r');INSERT INTO d VALUES(1,'m'),(2,'n');\
         SELECT a.v,d.w FROM a JOIN d ON a.x=d.y",
        // --- must NOT regress: sound seeks still used and correct ---
        // numeric = numeric index
        "CREATE TABLE t(k);CREATE TABLE u(k INTEGER, w);CREATE INDEX iu ON u(k);\
         INSERT INTO t VALUES(1),(2),(1);INSERT INTO u VALUES(1,'m'),(2,'n'),(1,'z');\
         SELECT t.k,u.w FROM t JOIN u ON t.k=u.k",
        // text = text index
        "CREATE TABLE t(k TEXT);CREATE TABLE u(k TEXT, w);CREATE INDEX iu ON u(k);\
         INSERT INTO t VALUES('x'),('y');INSERT INTO u VALUES('x','m'),('y','n'),('x','z');\
         SELECT t.k,u.w FROM t JOIN u ON t.k=u.k",
        // untyped = untyped index
        "CREATE TABLE t(k);CREATE TABLE u(k, w);CREATE INDEX iu ON u(k);\
         INSERT INTO t VALUES(1),('2');INSERT INTO u VALUES(1,'m'),('2','n');\
         SELECT t.k,u.w FROM t JOIN u ON t.k=u.k",
    ];
    for q in cases {
        let sql = if q.trim_end().ends_with(';') {
            q.to_string()
        } else {
            format!("{q};")
        };
        assert_eq!(rows("sqlite3", &sql), rows(g, &sql), "mismatch for `{q}`");
    }
}
