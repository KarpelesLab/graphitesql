//! A runtime UNIQUE violation on a *standalone* secondary index of a WITHOUT
//! ROWID table must name the offending column(s) — `UNIQUE constraint failed:
//! t.b` — exactly like SQLite. graphite degraded this one case to the bare
//! `UNIQUE constraint failed` (no column detail): the message builder
//! (`wr_unique_message`) consulted only the table's inline UNIQUE/PRIMARY KEY
//! sets, not the separately-created unique indexes that `wr_index_collision`
//! actually enforces. The inline-UNIQUE column, the WITHOUT ROWID primary key,
//! and a rowid-table secondary index were already correct — only this corner was
//! bare.
//!
//! graphite now routes the message through `wr_conflict_message`, which falls
//! through to the standalone unique indexes (collation- and partial-predicate
//! aware) and renders `t.col[, …]` for a column index.
//!
//! Verified against the sqlite3 3.50.4 CLI. The CLI's `stepping, ` prefix and the
//! trailing ` (19)` extended-result-code are normalised away — the library
//! message is byte-identical.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    let mut lines = Vec::new();
    for line in s.lines() {
        let mut t = line.trim_end();
        if t.trim_start().starts_with('^') {
            continue;
        }
        for prefix in [
            "Error: ",
            "in prepare, ",
            "stepping, ",
            "SQL error: ",
            "error: ",
        ] {
            t = t.strip_prefix(prefix).unwrap_or(t);
        }
        let mut t = t.to_string();
        if t.ends_with(')') {
            if let Some(open) = t.rfind(" (") {
                if t[open + 2..t.len() - 1].chars().all(|c| c.is_ascii_digit()) {
                    t.truncate(open);
                }
            }
        }
        lines.push(t);
    }
    lines.join("\n")
}

#[test]
fn without_rowid_secondary_unique_message_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Single-column secondary unique index, INSERT conflict -> names t.b.
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,9); \
         CREATE UNIQUE INDEX i ON t(b); INSERT INTO t VALUES(2,9)",
        // Two-column secondary index -> names both columns.
        "CREATE TABLE t(a PRIMARY KEY,b,c) WITHOUT ROWID; INSERT INTO t VALUES(1,8,9); \
         CREATE UNIQUE INDEX i ON t(b,c); INSERT INTO t VALUES(2,8,9)",
        // UPDATE that creates the collision -> same message.
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,9),(2,8); \
         CREATE UNIQUE INDEX i ON t(b); UPDATE t SET b=9 WHERE a=2",
        // INSERT OR FAIL / OR ABORT / OR ROLLBACK all surface the named message.
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,9); \
         CREATE UNIQUE INDEX i ON t(b); INSERT OR FAIL INTO t VALUES(2,9)",
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,9); \
         CREATE UNIQUE INDEX i ON t(b); INSERT OR ROLLBACK INTO t VALUES(2,9)",
        // Collation-aware: NOCASE makes 'X'/'x' collide on the secondary index.
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,'X'); \
         CREATE UNIQUE INDEX i ON t(b COLLATE NOCASE); INSERT INTO t VALUES(2,'x')",
        // Partial secondary index: conflict inside the predicate -> error.
        "CREATE TABLE t(a PRIMARY KEY,b,d) WITHOUT ROWID; INSERT INTO t VALUES(1,9,1); \
         CREATE UNIQUE INDEX i ON t(b) WHERE d=1; INSERT INTO t VALUES(2,9,1)",
        // Partial secondary index: row outside the predicate -> no conflict.
        "CREATE TABLE t(a PRIMARY KEY,b,d) WITHOUT ROWID; INSERT INTO t VALUES(1,9,1); \
         CREATE UNIQUE INDEX i ON t(b) WHERE d=1; INSERT INTO t VALUES(2,9,0); SELECT count(*) FROM t",
        // NULL key on the secondary index stays distinct -> both rows admitted.
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,NULL); \
         CREATE UNIQUE INDEX i ON t(b); INSERT INTO t VALUES(2,NULL); SELECT count(*) FROM t",
        // Controls that were already correct.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,9); \
         CREATE UNIQUE INDEX i ON t(b); INSERT INTO t VALUES(2,9)",
        "CREATE TABLE t(a PRIMARY KEY,b UNIQUE) WITHOUT ROWID; INSERT INTO t VALUES(1,9); \
         INSERT INTO t VALUES(2,9)",
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,9); \
         INSERT INTO t VALUES(1,8)",
    ];
    for sql in cases {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
