//! When an UPSERT's inserted row conflicts on more than one unique constraint,
//! `ON CONFLICT(cols) DO UPDATE` must edit the row that conflicts on the *named*
//! constraint — not simply the first conflict found. graphite used the first
//! conflict, so it edited the wrong row (and left the targeted one untouched).
//! Verified byte-for-byte against the sqlite3 3.50.4 CLI.

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
fn targeted_upsert_updates_the_named_conflict_row() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Insert conflicts on the PK (a=3) and on UNIQUE b (=-1, held by a=2).
        // ON CONFLICT(a) must update row a=3.
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE, c);\
         INSERT INTO t VALUES(2,-1,'x'),(3,1,9);\
         INSERT INTO t VALUES(3,-1,3) ON CONFLICT(a) DO UPDATE SET b=3;\
         SELECT quote(a),quote(b),quote(c) FROM t ORDER BY a;",
        // Same shape, targeting the UNIQUE b instead: update the b-conflict row.
        "CREATE TABLE t(a UNIQUE, b UNIQUE, c);\
         INSERT INTO t VALUES(1,10,100),(2,20,200);\
         INSERT INTO t VALUES(2,10,999) ON CONFLICT(b) DO UPDATE SET c=excluded.c;\
         SELECT quote(a),quote(b),quote(c) FROM t ORDER BY a;",
        // excluded.* / table.* references resolve against the correct rows.
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE, c);\
         INSERT INTO t VALUES(5,50,'p'),(6,60,'q');\
         INSERT INTO t VALUES(6,50,'z') ON CONFLICT(a) DO UPDATE SET c=excluded.c, b=t.b;\
         SELECT quote(a),quote(b),quote(c) FROM t ORDER BY a;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for `{sql}`");
    }
}
