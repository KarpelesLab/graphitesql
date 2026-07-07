//! `ON UPDATE SET DEFAULT` sets a child's foreign-key columns to their defaults
//! when the referenced parent key changes, and SQLite re-checks the constraint
//! against the parent set *after* the change. The parent whose key is being
//! updated no longer holds its old key, so a default that names that old key
//! dangles and must fail; a default naming the parent's new key, or a different
//! unchanged parent, is valid. graphite checked existence against the pre-update
//! table (where the old key was still present) and so accepted a dangling child.
//! Verified against sqlite3 3.50.4 by value and exit status (found by a foreign-key
//! fuzzer, which now runs clean over tens of thousands of random action mixes).

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
fn update_set_default_revalidates_against_post_update_parents() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // default names the parent whose key is being changed → dangles → error
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid DEFAULT 2 REFERENCES p(id) ON UPDATE SET DEFAULT);\
         INSERT INTO p VALUES(2),(3);INSERT INTO c VALUES(10,2);UPDATE p SET id=8 WHERE id=2;SELECT * FROM c;",
        // default names the parent's NEW key → valid
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid DEFAULT 8 REFERENCES p(id) ON UPDATE SET DEFAULT);\
         INSERT INTO p VALUES(2),(3);INSERT INTO c VALUES(10,2);UPDATE p SET id=8 WHERE id=2;SELECT * FROM c;",
        // default names a DIFFERENT unchanged parent → valid
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid DEFAULT 3 REFERENCES p(id) ON UPDATE SET DEFAULT);\
         INSERT INTO p VALUES(2),(3);INSERT INTO c VALUES(10,2);UPDATE p SET id=8 WHERE id=2;SELECT * FROM c;",
        // ON UPDATE CASCADE follows the new key (regression)
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid REFERENCES p(id) ON UPDATE CASCADE);\
         INSERT INTO p VALUES(2),(3);INSERT INTO c VALUES(10,2);UPDATE p SET id=8 WHERE id=2;SELECT * FROM c ORDER BY cid;",
        // ON UPDATE SET NULL nulls the child (regression)
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid REFERENCES p(id) ON UPDATE SET NULL);\
         INSERT INTO p VALUES(2),(3);INSERT INTO c VALUES(10,2);UPDATE p SET id=8 WHERE id=2;SELECT * FROM c ORDER BY cid;",
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
