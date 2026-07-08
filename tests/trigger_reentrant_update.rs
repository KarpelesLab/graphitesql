//! An `AFTER UPDATE` trigger that updates the *same* table (`UPDATE t SET b=b+1
//! WHERE a=NEW.a`) may, while firing for an earlier row of a multi-row UPDATE,
//! modify a later row. Those edits to columns the outer UPDATE does not itself
//! SET must survive the later row's write. graphite wrote each row from a
//! pass-one snapshot taken before any trigger ran, so it clobbered the trigger's
//! change. Verified against the sqlite3 3.50.4 CLI.

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
fn after_update_trigger_edits_a_later_row() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // The AFTER trigger bumps b for every row sharing NEW.a; a second row that
        // the outer UPDATE also touches must keep the bumped b.
        "CREATE TABLE t(a,b);\
         CREATE TRIGGER tr AFTER UPDATE ON t WHEN NEW.a>0 BEGIN UPDATE t SET b=b+1 WHERE a=NEW.a; END;\
         INSERT INTO t VALUES(-1,0),(1,0);\
         UPDATE t SET a=1 WHERE b=0;\
         SELECT quote(a),quote(b) FROM t ORDER BY a,b;",
        // Several rows collapse onto the same key.
        "CREATE TABLE t(a,b);\
         CREATE TRIGGER tr AFTER UPDATE ON t BEGIN UPDATE t SET b=b+1 WHERE a=NEW.a; END;\
         INSERT INTO t VALUES(1,0),(2,0),(3,0);\
         UPDATE t SET a=9;\
         SELECT quote(a),quote(b) FROM t ORDER BY b;",
        // A trigger-free multi-row UPDATE is unaffected by the change.
        "CREATE TABLE t(a,b);INSERT INTO t VALUES(1,10),(2,20);\
         UPDATE t SET a=a*10;SELECT quote(a),quote(b) FROM t ORDER BY a;",
        // A BEFORE UPDATE trigger editing the row still works (unchanged path).
        "CREATE TABLE t(a,b);\
         CREATE TRIGGER tr BEFORE UPDATE ON t BEGIN UPDATE t SET b=99 WHERE a=OLD.a; END;\
         INSERT INTO t VALUES(1,0),(2,0);\
         UPDATE t SET a=a+10;SELECT quote(a),quote(b) FROM t ORDER BY a;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for `{sql}`");
    }
}
