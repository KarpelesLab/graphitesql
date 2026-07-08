//! An aggregate function is not allowed anywhere inside a `GROUP BY` term.
//! SQLite reports a dedicated `aggregate functions are not allowed in the
//! GROUP BY clause` — even when the aggregate is nested in a larger expression
//! (`1 + count(*)`) or reached through an output-alias rewrite
//! (`SELECT count(*) AS c … GROUP BY c`). Previously graphite either reported
//! the generic "aggregate … used outside an aggregate context" or, for a
//! `GROUP BY max(a)` over a real table, accepted it silently. Aggregates remain
//! allowed in `HAVING` and `ORDER BY`. Matched to the `sqlite3` CLI (3.50.4).

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
fn aggregate_in_group_by_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 2), (1, 3), (2, 9)")
        .unwrap();
    const MSG: &str = "aggregate functions are not allowed in the GROUP BY clause";

    for sql in [
        "SELECT 1 GROUP BY count(*)",
        "SELECT 1 GROUP BY sum(1)",
        "SELECT 1 GROUP BY 1 + count(*)",
        "SELECT 1 GROUP BY avg(1) + 1",
        "SELECT a FROM t GROUP BY max(a)",
        "SELECT a, sum(b) FROM t GROUP BY sum(b)",
        // Reached through the output-alias rewrite.
        "SELECT count(*) AS c FROM t GROUP BY c",
    ] {
        assert_eq!(graphite_err(&c, sql), MSG, "for {sql}");
    }

    // Aggregates stay legal in HAVING and ORDER BY, and a plain / positional
    // GROUP BY is unaffected.
    assert!(
        c.query("SELECT a, sum(b) FROM t GROUP BY a HAVING sum(b) > 4 ORDER BY sum(b) DESC")
            .is_ok()
    );
    assert!(c.query("SELECT a FROM t GROUP BY a").is_ok());
    assert!(c.query("SELECT a, count(*) FROM t GROUP BY 1").is_ok());
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-gbagg-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,2),(1,3),(2,9);";
    {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(setup)
            .output()
            .unwrap();
        assert!(o.status.success());
    }
    let sqlite_err = |sql: &str| -> String {
        let out = Command::new("sqlite3")
            .arg(&path)
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
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    for sql in [
        "SELECT 1 GROUP BY count(*)",
        "SELECT 1 GROUP BY 1 + count(*)",
        "SELECT a FROM t GROUP BY max(a)",
        "SELECT count(*) AS c FROM t GROUP BY c",
    ] {
        assert_eq!(graphite_err(&g, sql), sqlite_err(sql), "for {sql}");
    }
    let _ = std::fs::remove_file(&path);
}
