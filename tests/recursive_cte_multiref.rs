//! A recursive CTE's recursive term may name the recursive table only once in
//! its FROM clause. A self-join on it (`FROM c, c`) is rejected by SQLite at
//! prepare time with `multiple references to recursive table: <name>`; graphite
//! previously ran the cross-join and reported a misleading `ambiguous column
//! name`. The error echoes the CTE's declared name verbatim (case-preserving).
//! Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn self_join_on_recursive_table_is_rejected() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c, c) SELECT x FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT c.x+1 FROM c JOIN c c2) SELECT x FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c, c d) SELECT x FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c, c, c) SELECT x FROM c",
    ] {
        let e = c.query(sql).unwrap_err().to_string();
        assert!(
            e.contains("multiple references to recursive table: c"),
            "for {sql}: {e}"
        );
    }
}

#[test]
fn name_is_echoed_case_preserving() {
    let c = Connection::open_memory().unwrap();
    let e = c
        .query("WITH RECURSIVE Foo(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM Foo, foo) SELECT x FROM Foo")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("multiple references to recursive table: Foo"),
        "{e}"
    );
}

#[test]
fn single_reference_recursion_still_runs() {
    let c = Connection::open_memory().unwrap();
    // One FROM reference to the recursive table is the normal, legal shape.
    let rows = c
        .query("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<5) SELECT group_concat(x) FROM c")
        .unwrap();
    assert_eq!(
        rows.rows[0][0],
        graphitesql::Value::Text("1,2,3,4,5".into())
    );
    // A non-recursive CTE may still be self-joined freely.
    let rows = c
        .query("WITH c(x) AS (SELECT 1) SELECT count(*) FROM c, c d")
        .unwrap();
    assert_eq!(rows.rows[0][0], graphitesql::Value::Integer(1));
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
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    // Every case here errors at prepare time, so none of them iterate.
    for sql in [
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c, c) SELECT x FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT c.x+1 FROM c JOIN c c2) SELECT x FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c, c d) SELECT x FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c, c, c) SELECT x FROM c",
        "WITH RECURSIVE C(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c, C) SELECT x FROM c",
        // A terminating single-reference recursion: identical rows on both.
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<4) SELECT group_concat(x) FROM c",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
