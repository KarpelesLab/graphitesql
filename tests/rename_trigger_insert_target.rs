//! `ALTER TABLE … RENAME TO` rewrites the renamed table's name throughout a
//! dependent trigger's stored text — including an `INSERT INTO <old>(col-list)`
//! target inside the trigger body. graphite's token rewriter skipped any
//! `<old>(` token as a presumed function call, so `INSERT INTO t(a) …` left the
//! `t` target un-renamed (while the trigger's `ON` clause and a `FROM t` were
//! renamed correctly), producing a `sqlite_schema.sql` that no longer matched
//! SQLite and a trigger pointing at the now-nonexistent old name. A token that
//! immediately follows `INTO` is a table reference with a column list, not a
//! function, so it must be renamed.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

#[test]
fn rename_rewrites_insert_target_in_trigger_body() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // INSERT INTO <renamed>(col-list) ... SELECT FROM <renamed> in the body.
        "CREATE TABLE t(a); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
           INSERT INTO t(a) SELECT a FROM t WHERE a>0; END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // INSERT INTO <renamed>(col-list) VALUES.
        "CREATE TABLE t(a); \
         CREATE TRIGGER tr AFTER DELETE ON t BEGIN \
           INSERT INTO t(a) VALUES(old.a); END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Trigger on ANOTHER table inserting into the renamed table with a list.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           INSERT INTO t(a) VALUES(new.b); END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Regression guard: a like-named function call (count) is left intact,
        // and an INSERT INTO with no column list still renames.
        "CREATE TABLE t(a); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
           INSERT INTO t VALUES(1); SELECT count(a) FROM t; END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // The whole trigger still fires correctly after the rename (round trip).
        "CREATE TABLE t(a); \
         CREATE TRIGGER tr AFTER INSERT ON t WHEN new.a=1 BEGIN \
           INSERT INTO t(a) VALUES(99); END; \
         ALTER TABLE t RENAME TO t2; \
         INSERT INTO t2(a) VALUES(1); \
         SELECT a FROM t2 ORDER BY a",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
