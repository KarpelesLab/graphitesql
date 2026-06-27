//! An unreferenced `WITH` common table expression is never semantically analyzed
//! by SQLite: a bad column or missing table inside a CTE that the consuming query
//! does not reach is *not* an error, and an otherwise-infinite recursive CTE that
//! nothing selects from is simply never run. graphite used to eagerly materialize
//! every CTE in a `WITH` clause, so it reported errors (or could loop) for CTEs
//! that SQLite quietly ignores. Reachability is transitive (a used CTE pulls in
//! the siblings it names) and scope-aware (a derived subquery's own nested `WITH`
//! shadows an outer CTE of the same name). Syntax errors and duplicate `WITH`
//! names still fire regardless of use. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The single scalar value of a one-row, one-column query.
fn scalar(sql: &str) -> Value {
    let c = Connection::open_memory().unwrap();
    c.query(sql).unwrap().rows.into_iter().next().unwrap()[0].clone()
}

/// The library's error message for `sql`, Display tag stripped.
fn err_msg(sql: &str) -> String {
    let c = Connection::open_memory().unwrap();
    let e = c.query(sql).unwrap_err().to_string();
    e.trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn an_unreferenced_cte_with_a_bad_column_is_not_an_error() {
    assert_eq!(
        scalar("WITH r AS (SELECT nope) SELECT 1"),
        Value::Integer(1)
    );
    assert_eq!(
        scalar("WITH r AS (SELECT 1 FROM nosuchtable) SELECT 1"),
        Value::Integer(1)
    );
    // Two CTEs, only the second used — the first's bad column is ignored.
    assert_eq!(
        scalar("WITH a AS (SELECT bad), b AS (SELECT 7 y) SELECT y FROM b"),
        Value::Integer(7)
    );
}

#[test]
fn an_unreferenced_recursive_cte_is_never_run() {
    // Without the unused-CTE skip this recursion would run to the runaway guard.
    assert_eq!(
        scalar("WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) SELECT 42"),
        Value::Integer(42)
    );
}

#[test]
fn a_referenced_cte_is_still_analyzed() {
    // Direct, transitive, and via-subquery references all keep validation on.
    assert_eq!(
        err_msg("WITH r AS (SELECT nope) SELECT * FROM r"),
        "no such column: nope"
    );
    assert_eq!(
        err_msg("WITH a AS (SELECT bad), b AS (SELECT * FROM a) SELECT * FROM b"),
        "no such column: bad"
    );
    assert_eq!(
        err_msg("WITH a AS (SELECT bad) SELECT (SELECT count(*) FROM a)"),
        "no such column: bad"
    );
    assert_eq!(
        err_msg("WITH a AS (SELECT bad x) SELECT 1 WHERE EXISTS(SELECT 1 FROM a)"),
        "no such column: bad"
    );
}

#[test]
fn a_nested_with_shadows_an_outer_cte_of_the_same_name() {
    // The derived subquery binds its own `a`, so the outer `a` is unreferenced
    // and its bad column is never analyzed.
    assert_eq!(
        scalar("WITH a AS (SELECT bad) SELECT * FROM (WITH a AS (SELECT 7 x) SELECT x FROM a)"),
        Value::Integer(7)
    );
}

#[test]
fn duplicate_with_names_error_even_when_unused() {
    assert_eq!(
        err_msg("WITH r AS (SELECT 1), r AS (SELECT 2) SELECT 1"),
        "duplicate WITH table name: r"
    );
}

#[test]
fn a_used_cte_still_computes() {
    assert_eq!(
        scalar("WITH a AS (SELECT 5 x) SELECT x*2 FROM a"),
        Value::Integer(10)
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.lines().next().unwrap_or("").trim_end().to_string();
        if !line.is_empty() {
            return line;
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "WITH r AS (SELECT nope) SELECT 1",
        "WITH r AS (SELECT 1 FROM nosuchtable) SELECT 1",
        "WITH a AS (SELECT bad), b AS (SELECT 7 y) SELECT y FROM b",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) SELECT 42",
        "WITH r AS (SELECT nope) SELECT * FROM r",
        "WITH a AS (SELECT bad), b AS (SELECT * FROM a) SELECT * FROM b",
        "WITH a AS (SELECT bad) SELECT (SELECT count(*) FROM a)",
        "WITH a AS (SELECT bad x) SELECT 1 WHERE EXISTS(SELECT 1 FROM a)",
        "WITH a AS (SELECT bad) SELECT * FROM (WITH a AS (SELECT 7 x) SELECT x FROM a)",
        "WITH r AS (SELECT 1), r AS (SELECT 2) SELECT 1",
        "WITH a AS (SELECT 5 x) SELECT x*2 FROM a",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
