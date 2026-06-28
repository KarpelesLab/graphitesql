//! `CREATE UNIQUE INDEX` over a table that already contains duplicate keys must
//! fail with `UNIQUE constraint failed: …`, exactly as SQLite does — graphite
//! previously built the index silently (the trailing rowid made every encoded
//! key distinct, so the btree insert never detected the clash), leaving a
//! "unique" index that did not actually enforce uniqueness yet still passed
//! `PRAGMA integrity_check`. A genuine silent-corruption bug.
//!
//! graphite now pre-checks the indexed key tuples (NULLs distinct, collation
//! aware, the index's WHERE predicate applied) before writing the btree and
//! raises the same message: `t.col[, …]` for a column index, `index '<name>'`
//! for an expression index.
//!
//! Verified against the sqlite3 3.50.4 CLI. The CLI's contextual error prefix
//! (`stepping, `) and trailing extended-result-code (` (19)`) are normalised
//! away — the library message itself is byte-identical.

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
        // Strip a trailing extended-result-code like " (19)" that the stock CLI
        // appends to runtime errors; the graphite CLI does not render it.
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
fn create_unique_index_over_duplicates_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Single-column duplicate -> rejected, names the column.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(1,3); CREATE UNIQUE INDEX i ON t(a)",
        // Two-column composite duplicate -> names both columns.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(1,2); CREATE UNIQUE INDEX i ON t(a,b)",
        // Three columns.
        "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(1,2,3); CREATE UNIQUE INDEX i ON t(a,b,c)",
        // Composite where only the full tuple clashes (partial overlap is fine).
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(1,3),(2,3); CREATE UNIQUE INDEX i ON t(a,b)",
        // Expression index -> names the index, not the columns.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(1,3); CREATE UNIQUE INDEX i ON t(a+0)",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(2,1),(1,2); CREATE UNIQUE INDEX i ON t(a+b)",
        // NULLs are always distinct -> the index builds successfully.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(NULL,2),(NULL,3); CREATE UNIQUE INDEX i ON t(a)",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,NULL),(1,NULL); CREATE UNIQUE INDEX i ON t(a,b)",
        // Partial index: predicate excludes the duplicate -> builds.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(1,3); CREATE UNIQUE INDEX i ON t(a) WHERE b=2",
        // Partial index: predicate includes both duplicates -> rejected.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(1,2); CREATE UNIQUE INDEX i ON t(a) WHERE b=2",
        // Collation: NOCASE makes 'A'/'a' clash; BINARY keeps them distinct.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES('A',1),('a',2); CREATE UNIQUE INDEX i ON t(a COLLATE NOCASE)",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES('A',1),('a',2); CREATE UNIQUE INDEX i ON t(a)",
        // WITHOUT ROWID table, secondary unique index over a duplicate column.
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,9),(2,9); CREATE UNIQUE INDEX i ON t(b)",
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,9),(2,8); CREATE UNIQUE INDEX i ON t(b)",
        // No duplicates -> builds, and the index actually works afterwards.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(2,3); CREATE UNIQUE INDEX i ON t(a); SELECT count(*) FROM t",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(2,3); CREATE UNIQUE INDEX i ON t(a); PRAGMA integrity_check",
        // After a clean build the index enforces uniqueness on later inserts.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); CREATE UNIQUE INDEX i ON t(a); INSERT INTO t VALUES(1,3)",
        // Empty table -> trivially builds.
        "CREATE TABLE t(a,b); CREATE UNIQUE INDEX i ON t(a)",
        // Non-unique index over duplicates is always fine.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(1,3); CREATE INDEX i ON t(a)",
    ];
    for sql in cases {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
