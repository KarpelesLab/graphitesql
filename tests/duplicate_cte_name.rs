//! Two CTEs sharing a name within one `WITH` clause are rejected with
//! `duplicate WITH table name: NAME` (case-insensitive, naming the duplicate
//! occurrence's spelling), in `SELECT`/`UPDATE`/`DELETE` and with `RECURSIVE`.
//! A same name in a *nested* `WITH` is a separate scope and stays legal.
//! Previously graphite silently accepted the duplicate. Matched to the `sqlite3`
//! CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_err(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn duplicate_with_name_is_rejected() {
    let c = Connection::open_memory().unwrap();
    for (sql, dup) in [
        ("WITH x AS (SELECT 1), x AS (SELECT 2) SELECT 1", "x"),
        // Case-insensitive; the *second* occurrence's spelling is reported.
        ("WITH x AS (SELECT 1), X AS (SELECT 2) SELECT 1", "X"),
        (
            "WITH RECURSIVE x AS (SELECT 1), x AS (SELECT 2) SELECT 1",
            "x",
        ),
        (
            "WITH a AS (SELECT 1), b AS (SELECT 2), a AS (SELECT 3) SELECT 1",
            "a",
        ),
    ] {
        assert_eq!(
            graphite_err(&c, sql),
            format!("duplicate WITH table name: {dup}"),
            "for {sql}"
        );
    }

    // Distinct names, and a same name re-used in a nested WITH (separate scope),
    // are both legal.
    assert!(c
        .query("WITH a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM a JOIN b")
        .is_ok());
    assert!(c
        .query("WITH a AS (SELECT 1) SELECT * FROM (WITH a AS (SELECT 2) SELECT 1)")
        .is_ok());
}

#[test]
fn duplicate_with_name_in_update_and_delete() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    let exec_err = |c: &mut Connection, sql: &str| -> String {
        c.execute(sql)
            .unwrap_err()
            .to_string()
            .trim_start_matches("error: ")
            .to_string()
    };
    assert_eq!(
        exec_err(
            &mut c,
            "WITH c AS (SELECT 1), c AS (SELECT 2) UPDATE t SET x = 1"
        ),
        "duplicate WITH table name: c"
    );
    assert_eq!(
        exec_err(
            &mut c,
            "WITH c AS (SELECT 1), c AS (SELECT 2) DELETE FROM t"
        ),
        "duplicate WITH table name: c"
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let sqlite_err = |sql: &str| -> String {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(sql)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .to_string()
    };
    let c = Connection::open_memory().unwrap();
    for sql in [
        "WITH x AS (SELECT 1), x AS (SELECT 2) SELECT 1",
        "WITH x AS (SELECT 1), X AS (SELECT 2) SELECT 1",
        "WITH RECURSIVE x AS (SELECT 1), x AS (SELECT 2) SELECT 1",
        "WITH a AS (SELECT 1), b AS (SELECT 2), a AS (SELECT 3) SELECT 1",
    ] {
        assert_eq!(graphite_err(&c, sql), sqlite_err(sql), "for {sql}");
    }
}
