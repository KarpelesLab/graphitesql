//! SQLite resolves column references at prepare time, so a reference to a
//! non-existent column in a FROM-less `SELECT` is `no such column` even when it
//! sits in a short-circuited branch the evaluator never reaches (e.g. the
//! never-taken second argument of `IFNULL(1, …)`). graphite resolved a FROM-less
//! query's columns lazily, so it silently returned a row instead. Verified
//! against the sqlite3 3.50.4 CLI.
//!
//! Only *single* unknown-column cases are asserted: when a FROM-less query names
//! several unknown columns, sqlite and graphite may pick a different one to
//! report first (a message-precedence detail), but both still reject the query.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> (String, String) {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn from_less_unknown_column_rejected_at_prepare() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Each errors with `no such column: zzz` — compare that the message body
    // matches (the CLIs differ only in the wrapping prefix).
    let bad = [
        "SELECT IFNULL(1, zzz);",
        "SELECT max('a', IFNULL(1, zzz));",
        "SELECT CASE WHEN 1 THEN 2 ELSE zzz END;",
        "SELECT 1 WHERE zzz;",
        "SELECT coalesce(2, zzz) AS c ORDER BY zzz;",
    ];
    for sql in bad {
        let (so, _) = run("sqlite3", sql);
        let (go, ge) = run(g, sql);
        assert!(so.is_empty(), "sqlite should reject `{sql}`");
        assert!(go.is_empty(), "graphite should reject `{sql}`, got `{go}`");
        assert!(
            ge.contains("no such column: zzz"),
            "graphite message for `{sql}` was `{ge}`"
        );
    }
    // Valid FROM-less queries still run, byte-for-byte.
    let good = [
        "SELECT 1, 2.5, 'x', abs(-3), coalesce(NULL, 7);",
        "SELECT 1 AS x ORDER BY x;",             // an output alias is visible in ORDER BY
        "SELECT IFNULL(1, 2), max(3, 4);",
        "SELECT current_date IS NOT NULL;",      // date/time keyword needs no table
    ];
    for sql in good {
        let (so, _) = run("sqlite3", sql);
        let (go, _) = run(g, sql);
        assert_eq!(so, go, "for `{sql}`");
    }
    // A correlated FROM-less subquery legitimately reads the outer table.
    let corr = "CREATE TABLE t(a);INSERT INTO t VALUES(1),(9);\
                SELECT a FROM t WHERE a > (SELECT 5 WHERE a < 3) ORDER BY a;";
    let (so, _) = run("sqlite3", corr);
    let (go, _) = run(g, corr);
    assert_eq!(so, go, "correlated FROM-less subquery");
}
