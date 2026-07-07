//! SQLite resolves a UNIQUE / PRIMARY KEY conflict before the child-side foreign
//! key check, so a row skipped by `INSERT OR IGNORE` (or an upsert `DO NOTHING`)
//! on a PK conflict never trips its FK — even if its FK value has no parent.
//! graphite checked the FK first and reported a spurious `FOREIGN KEY constraint
//! failed` for a row it was about to skip anyway. A pure FK violation (no
//! conflict) still errors under OR IGNORE, since OR IGNORE does not suppress FK
//! violations. Verified against sqlite3 3.50.4 by value and by exit status.

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
fn insert_conflict_resolved_before_fk_check() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
        CREATE TABLE c(cid INTEGER PRIMARY KEY, pid REFERENCES p(id));INSERT INTO p VALUES(1),(2);";
    let cases = [
        // PK conflict + FK violation under OR IGNORE → skipped, no error
        format!("{base}INSERT INTO c VALUES(6,2);INSERT OR IGNORE INTO c VALUES(6,4);SELECT * FROM c;"),
        // upsert DO NOTHING on the conflict, FK value dangling → skipped
        format!("{base}INSERT INTO c VALUES(6,2);INSERT INTO c VALUES(6,4) ON CONFLICT(cid) DO NOTHING;SELECT * FROM c;"),
        // pure FK violation, no conflict, OR IGNORE → still errors
        format!("{base}INSERT OR IGNORE INTO c VALUES(10,4);SELECT * FROM c;"),
        // plain INSERT FK violation → errors
        format!("{base}INSERT INTO c VALUES(10,4);SELECT * FROM c;"),
        // OR REPLACE deletes the conflict then inserts a valid FK → succeeds
        format!("{base}INSERT INTO c VALUES(6,1);INSERT OR REPLACE INTO c VALUES(6,2);SELECT * FROM c;"),
        // OR REPLACE with a dangling FK → errors
        format!("{base}INSERT INTO c VALUES(6,1);INSERT OR REPLACE INTO c VALUES(6,9);SELECT * FROM c;"),
    ];
    for sql in &cases {
        let (s_out, s_ok) = run("sqlite3", sql);
        let (g_out, g_ok) = run(g, sql);
        assert_eq!(s_ok, g_ok, "success/error status differs for `{sql}`");
        if s_ok {
            assert_eq!(s_out, g_out, "rows differ for `{sql}`");
        }
    }
}
