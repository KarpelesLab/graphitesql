//! `RAISE(IGNORE)` inside an AFTER trigger stops that trigger program but has no
//! effect on the row operation, which has already completed — so every row of the
//! firing statement is still processed. graphite left the `raise_ignore` flag set
//! after an AFTER trigger, and it leaked into the NEXT row's BEFORE-trigger check
//! (which skips a row when the flag is set), so a multi-row UPDATE/DELETE stopped
//! after the first row and a subsequent INSERT's row was dropped. A BEFORE (or
//! INSTEAD OF) `RAISE(IGNORE)` still abandons its row. Verified byte-for-byte
//! against the sqlite3 3.50.4 CLI (found by a randomized trigger fuzzer).

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
fn raise_ignore_in_after_trigger_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // AFTER INSERT: both rows survive
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a);\
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT RAISE(IGNORE); END;\
         INSERT INTO t(a) VALUES(1);INSERT INTO t(a) VALUES(2);SELECT * FROM t ORDER BY id;",
        // AFTER UPDATE over multiple rows: all update
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a);INSERT INTO t VALUES(1,10),(2,20),(3,30);\
         CREATE TRIGGER tr AFTER UPDATE ON t BEGIN SELECT RAISE(IGNORE); END;\
         UPDATE t SET a=99;SELECT * FROM t ORDER BY id;",
        // AFTER DELETE over multiple rows: all delete
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a);INSERT INTO t VALUES(1,10),(2,20),(3,30);\
         CREATE TRIGGER tr AFTER DELETE ON t BEGIN SELECT RAISE(IGNORE); END;\
         DELETE FROM t WHERE id<3;SELECT * FROM t ORDER BY id;",
        // conditional AFTER RAISE (WHEN) fires on one row, others unaffected
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a);INSERT INTO t VALUES(1,1),(2,2),(3,3);\
         CREATE TRIGGER tr AFTER UPDATE ON t WHEN NEW.a=99 BEGIN SELECT RAISE(IGNORE); END;\
         UPDATE t SET a=99 WHERE id=2;UPDATE t SET a=88;SELECT * FROM t ORDER BY id;",
        // BEFORE INSERT RAISE(IGNORE) still skips its row (must not regress)
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a);\
         CREATE TRIGGER tr BEFORE INSERT ON t BEGIN SELECT RAISE(IGNORE); END;\
         INSERT INTO t(a) VALUES(1);INSERT INTO t(a) VALUES(2);SELECT count(*) FROM t;",
        // AFTER then a later independent statement runs normally
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a);CREATE TABLE u(x);\
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT RAISE(IGNORE); END;\
         INSERT INTO t(a) VALUES(1);INSERT INTO u VALUES(7);SELECT (SELECT count(*) FROM t),(SELECT count(*) FROM u);",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "mismatch for `{sql}`");
    }
}
