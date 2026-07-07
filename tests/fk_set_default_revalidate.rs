//! `ON DELETE/UPDATE SET DEFAULT` sets the child's foreign-key columns to their
//! column defaults, and SQLite then re-checks the constraint: unless the default
//! key contains a NULL (MATCH SIMPLE ⇒ satisfied), it must itself reference an
//! existing parent row, else the child now dangles and the statement fails.
//! graphite applied the default without re-validating, leaving a dangling child.
//! Verified against the sqlite3 3.50.4 CLI: value outcomes byte-for-byte, and the
//! error case by exit status (the CLI's error *text* prefix differs by design).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> (String, bool) {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        o.status.success(),
    )
}

#[test]
fn set_default_revalidates_foreign_key() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // default parent (1) MISSING → both error, child unchanged
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid DEFAULT 1 REFERENCES p(id) ON DELETE SET DEFAULT);\
         INSERT INTO p VALUES(2),(3);INSERT INTO c VALUES(10,2);\
         DELETE FROM p WHERE id=2;SELECT * FROM c;",
        // ON UPDATE SET DEFAULT, default (5) missing → error
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid DEFAULT 5 REFERENCES p(id) ON UPDATE SET DEFAULT);\
         INSERT INTO p VALUES(2),(3);INSERT INTO c VALUES(10,2);\
         UPDATE p SET id=8 WHERE id=2;SELECT * FROM c;",
        // default parent (1) EXISTS → child set to 1, no error
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid DEFAULT 1 REFERENCES p(id) ON DELETE SET DEFAULT);\
         INSERT INTO p VALUES(1),(2);INSERT INTO c VALUES(10,2);\
         DELETE FROM p WHERE id=2;SELECT * FROM c;",
        // no explicit default → NULL default satisfies the FK (MATCH SIMPLE)
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid REFERENCES p(id) ON DELETE SET DEFAULT);\
         INSERT INTO p VALUES(2),(3);INSERT INTO c VALUES(10,2);\
         DELETE FROM p WHERE id=2;SELECT * FROM c;",
    ];
    for sql in cases {
        let (s_out, s_ok) = run("sqlite3", sql);
        let (g_out, g_ok) = run(g, sql);
        assert_eq!(s_ok, g_ok, "success/error status differs for `{sql}`");
        if s_ok {
            assert_eq!(s_out, g_out, "rows differ for `{sql}`");
        }
    }
}
