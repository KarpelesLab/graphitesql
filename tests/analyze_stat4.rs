//! Differential test for `ANALYZE`'s `sqlite_stat4` generation.
//!
//! `sqlite_stat4` is only written by a `sqlite3` built with
//! `-DSQLITE_ENABLE_STAT4`. The plain `sqlite3` on `PATH` usually lacks it, so
//! this test shells out to a **STAT4-enabled oracle** binary located via the
//! `GRAPHITE_STAT4_ORACLE` environment variable (falling back to the in-tree
//! build path). The test is skipped when that binary is not present.
//!
//! For each schema it: builds the DB with the oracle and (separately) with
//! graphitesql, runs `ANALYZE` in both, then compares the full `sqlite_stat1`
//! and `sqlite_stat4` contents — reading *graphite's* database file back with
//! the oracle, which also proves the file is byte-compatible and STAT4-readable.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::path::PathBuf;
use std::process::Command;

/// Absolute path to a STAT4-enabled `sqlite3` oracle, or `None` if unavailable.
fn oracle() -> Option<String> {
    if let Ok(p) = std::env::var("GRAPHITE_STAT4_ORACLE")
        && Command::new(&p)
            .arg(":memory:")
            .arg("SELECT 1")
            .output()
            .is_ok()
    {
        return Some(p);
    }
    // In-tree default (the ANALYZE/STAT4 worktree oracle).
    let default = "/tmp/claude-1000/-home-magicaltux-projects-graphitesql/\
faf0a91b-ae7e-4ff2-9c4e-2e8b1eed5c39/scratchpad/sqlite-src/\
sqlite-amalgamation-3500400/sqlite3-oracle";
    if Command::new(default)
        .arg(":memory:")
        .arg("SELECT 1")
        .output()
        .is_ok()
    {
        return Some(default.to_string());
    }
    None
}

/// Confirm the oracle actually has STAT4 enabled (writes a non-empty
/// `sqlite_stat4`); otherwise skip.
fn oracle_has_stat4(orc: &str) -> bool {
    let out = Command::new(orc)
        .arg(":memory:")
        .arg(
            "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3); CREATE INDEX i ON t(a); \
             ANALYZE; SELECT count(*) FROM sqlite_stat4;",
        )
        .output();
    matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().unwrap_or(0) > 0)
}

fn orc_query(orc: &str, db: &str, sql: &str) -> String {
    let o = Command::new(orc).arg(db).arg(sql).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn tmp(name: &str) -> String {
    let mut p: PathBuf = std::env::temp_dir();
    p.push(format!("gsql-stat4-{}-{}.db", std::process::id(), name));
    let s = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&s);
    s
}

/// Build `setup` (a `;`-joined statement list) plus `ANALYZE` with both engines
/// and assert the `sqlite_stat1` and `sqlite_stat4` contents are byte-identical.
fn check(orc: &str, tag: &str, setup: &str) {
    let script = format!("{setup}; ANALYZE;");

    // Oracle DB.
    let odb = tmp(&format!("orc-{tag}"));
    orc_query(orc, &odb, &script);

    // Graphite DB (built independently, then read back with the oracle).
    let gdb = tmp(&format!("gra-{tag}"));
    {
        let mut c = Connection::create(&gdb).unwrap();
        for stmt in script.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                c.execute(s)
                    .unwrap_or_else(|e| panic!("[{tag}] graphite failed on `{s}`: {e:?}"));
            }
        }
    }

    // integrity_check must pass and the file must be oracle-readable.
    let ic = orc_query(orc, &gdb, "PRAGMA integrity_check;");
    assert_eq!(ic, "ok", "[{tag}] integrity_check on graphite DB");

    let q1 = "SELECT tbl,idx,stat FROM sqlite_stat1 ORDER BY tbl,idx,stat";
    let q4 = "SELECT tbl,idx,neq,nlt,ndlt,quote(sample) FROM sqlite_stat4 \
              ORDER BY tbl,idx,nlt,ndlt";
    assert_eq!(
        orc_query(orc, &gdb, q1),
        orc_query(orc, &odb, q1),
        "[{tag}] sqlite_stat1 mismatch"
    );
    assert_eq!(
        orc_query(orc, &gdb, q4),
        orc_query(orc, &odb, q4),
        "[{tag}] sqlite_stat4 mismatch"
    );

    let _ = std::fs::remove_file(&odb);
    let _ = std::fs::remove_file(&gdb);
}

#[test]
fn stat4_matches_stat4_oracle() {
    let Some(orc) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("oracle lacks STAT4; skipping");
        return;
    }

    // 1..=100 generator for large tables.
    let series = |lo: i64, hi: i64, expr: &str| {
        format!(
            "WITH RECURSIVE r(x) AS (SELECT {lo} UNION ALL SELECT x+1 FROM r WHERE x<{hi}) \
             INSERT INTO t SELECT {expr} FROM r"
        )
    };

    let cases: Vec<(&str, String)> = vec![
        (
            "multicol-small",
            "CREATE TABLE t(a,b); \
             INSERT INTO t VALUES(1,10),(1,20),(2,30),(2,40),(3,50); \
             CREATE INDEX i ON t(a,b)"
                .into(),
        ),
        (
            "reservoir-100",
            format!(
                "CREATE TABLE t(a); {}; CREATE INDEX i ON t(a)",
                series(1, 100, "x")
            ),
        ),
        (
            "heavy-dup",
            format!(
                "CREATE TABLE t(a,b); {}; CREATE INDEX i ON t(a)",
                series(1, 250, "x/5, x")
            ),
        ),
        (
            "nulls",
            "CREATE TABLE t(a,b); INSERT INTO t VALUES(NULL,1),(NULL,2),(1,3),(1,4),(2,5); \
             CREATE INDEX i ON t(a)"
                .into(),
        ),
        (
            "desc-index",
            "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,1),(2,2),(3,3),(2,4); \
             CREATE INDEX i ON t(a DESC)"
                .into(),
        ),
        (
            "unique-notnull",
            "CREATE TABLE t(a NOT NULL,b); INSERT INTO t VALUES(1,10),(2,20),(3,30); \
             CREATE UNIQUE INDEX i ON t(a)"
                .into(),
        ),
        (
            "text-nocase",
            "CREATE TABLE t(a TEXT COLLATE NOCASE,b); \
             INSERT INTO t VALUES('A',1),('a',2),('B',3),('b',4),('c',5); \
             CREATE INDEX i ON t(a)"
                .into(),
        ),
        (
            "two-indexes",
            format!(
                "CREATE TABLE t(a,b,c); {}; CREATE INDEX i ON t(a,b); CREATE INDEX j ON t(c)",
                series(1, 60, "x%3, x%9, x")
            ),
        ),
        (
            "wr-pk-only",
            "CREATE TABLE t(a,b,PRIMARY KEY(a,b)) WITHOUT ROWID; \
             INSERT INTO t VALUES(1,10),(1,20),(2,30),(3,40)"
                .into(),
        ),
        (
            "wr-pk-and-secondary",
            "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID; \
             INSERT INTO t VALUES(1,2,100),(3,4,200),(5,6,100),(7,8,300); \
             CREATE INDEX i ON t(c)"
                .into(),
        ),
        (
            "wr-desc-pk",
            "CREATE TABLE t(a,b,PRIMARY KEY(a DESC,b)) WITHOUT ROWID; \
             INSERT INTO t VALUES(1,1),(2,2),(3,3),(4,4)"
                .into(),
        ),
        (
            "wr-single-pk-large",
            format!(
                "CREATE TABLE t(k PRIMARY KEY, v) WITHOUT ROWID; {}",
                series(1, 80, "x, x*2")
            ),
        ),
        (
            "empty-table",
            "CREATE TABLE t(a,b); CREATE INDEX i ON t(a)".into(),
        ),
    ];

    for (tag, setup) in &cases {
        check(&orc, tag, setup);
    }
}
