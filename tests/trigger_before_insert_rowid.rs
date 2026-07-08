//! A `BEFORE INSERT` trigger fires before SQLite assigns an automatic rowid, so
//! `NEW.<rowid>` — an `INTEGER PRIMARY KEY` alias, or the bare `rowid`/`_rowid_`/
//! `oid`, or the implicit rowid of a table with no alias — reads back as `-1` (the
//! "not yet allocated" sentinel) whenever the row will take an auto rowid. An
//! explicitly supplied rowid is visible as itself, and `AFTER INSERT`/`BEFORE
//! UPDATE` see the real value. graphite exposed the eventually-assigned rowid to
//! the BEFORE INSERT trigger. Verified against the sqlite3 3.50.4 CLI.

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
fn before_insert_new_rowid_is_minus_one_when_auto() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // INTEGER PRIMARY KEY alias: NULL / omitted -> -1; explicit -> itself.
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b);CREATE TABLE log(v);\
         CREATE TRIGGER tr BEFORE INSERT ON t BEGIN INSERT INTO log VALUES(NEW.a); END;\
         INSERT INTO t VALUES(NULL,'x');INSERT INTO t VALUES(5,'y');INSERT INTO t(b) VALUES('z');\
         INSERT INTO t VALUES(1000,'w');INSERT INTO t(b) VALUES('q');\
         SELECT v FROM log ORDER BY rowid;",
        // Bare rowid reference in a table with an INTEGER PRIMARY KEY.
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b);CREATE TABLE log(v);\
         CREATE TRIGGER tr BEFORE INSERT ON t BEGIN INSERT INTO log VALUES(NEW.rowid); END;\
         INSERT INTO t(b) VALUES('x');INSERT INTO t VALUES(9,'y');SELECT v FROM log ORDER BY rowid;",
        // Table with no INTEGER PRIMARY KEY: NEW.rowid of an auto row is -1.
        "CREATE TABLE t(a, b);CREATE TABLE log(v);\
         CREATE TRIGGER tr BEFORE INSERT ON t BEGIN INSERT INTO log VALUES(NEW.rowid); END;\
         INSERT INTO t VALUES(5,'x');INSERT INTO t VALUES(6,'y');SELECT v FROM log ORDER BY rowid;",
        // AFTER INSERT sees the assigned rowid; BEFORE UPDATE sees the real value.
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b);CREATE TABLE log(v);\
         CREATE TRIGGER ta AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.a); END;\
         INSERT INTO t VALUES(NULL,'x');SELECT v FROM log;",
        // A WHEN clause referencing NEW.rowid on the auto row.
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b);CREATE TABLE log(v);\
         CREATE TRIGGER tr BEFORE INSERT ON t WHEN NEW.a < 0 BEGIN INSERT INTO log VALUES('neg'); END;\
         INSERT INTO t VALUES(NULL,'x');INSERT INTO t VALUES(3,'y');SELECT v FROM log;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for `{sql}`");
    }
}
