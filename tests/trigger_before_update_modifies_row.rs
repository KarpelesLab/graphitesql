//! A BEFORE UPDATE trigger may modify the very row being updated, via a nested
//! `UPDATE t SET … WHERE id = OLD.id`. SQLite keeps the trigger's changes to
//! columns the outer UPDATE does not itself SET, then overlays the SET
//! assignments (computed from the original row) on top — so `UPDATE t SET a=1`
//! with a `BEFORE` trigger doing `UPDATE t SET b=99 …` yields `a=1, b=99`.
//! graphite wrote a snapshot captured before the trigger ran, silently dropping
//! the trigger's change to `b`. It now re-reads the row after the BEFORE trigger
//! and merges. Verified byte-for-byte against the sqlite3 3.50.4 CLI (found by a
//! randomized trigger fuzzer).

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
fn before_update_trigger_row_modification_persists() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // trigger sets a non-SET column → its change persists
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);INSERT INTO t VALUES(1,10,20);\
         CREATE TRIGGER tr BEFORE UPDATE ON t BEGIN UPDATE t SET b=99 WHERE id=OLD.id; END;\
         UPDATE t SET a=1 WHERE id=1;SELECT * FROM t;",
        // the outer SET RHS still reads the ORIGINAL (pre-trigger) value
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);INSERT INTO t VALUES(1,10,20);\
         CREATE TRIGGER tr BEFORE UPDATE ON t BEGIN UPDATE t SET b=99 WHERE id=OLD.id; END;\
         UPDATE t SET a=b WHERE id=1;SELECT * FROM t;",
        // trigger sets the SAME column the outer UPDATE sets → outer value wins
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);INSERT INTO t VALUES(1,10,20);\
         CREATE TRIGGER tr BEFORE UPDATE ON t BEGIN UPDATE t SET a=99 WHERE id=OLD.id; END;\
         UPDATE t SET a=1 WHERE id=1;SELECT * FROM t;",
        // a generated column is recomputed over the merged row
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b,c AS (a+b));INSERT INTO t(id,a,b) VALUES(1,10,20);\
         CREATE TRIGGER tr BEFORE UPDATE ON t BEGIN UPDATE t SET b=100 WHERE id=OLD.id; END;\
         UPDATE t SET a=5 WHERE id=1;SELECT id,a,b,c FROM t;",
        // multi-row UPDATE, each row's trigger bumps its own b
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);INSERT INTO t VALUES(1,10,20),(2,30,40);\
         CREATE TRIGGER tr BEFORE UPDATE ON t BEGIN UPDATE t SET b=b+1 WHERE id=OLD.id; END;\
         UPDATE t SET a=a*2;SELECT * FROM t ORDER BY id;",
        // regression: a BEFORE trigger touching a DIFFERENT row leaves this one as SET
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);INSERT INTO t VALUES(1,10,20),(2,30,40);\
         CREATE TRIGGER tr BEFORE UPDATE ON t BEGIN UPDATE t SET b=b+1 WHERE id=2; END;\
         UPDATE t SET a=7 WHERE id=1;SELECT * FROM t ORDER BY id;",
        // regression: no trigger at all
        "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);INSERT INTO t VALUES(1,10,20);\
         UPDATE t SET a=5 WHERE id=1;SELECT * FROM t;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "mismatch for `{sql}`");
    }
}
