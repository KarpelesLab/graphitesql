//! A window function — any call carrying an `OVER` clause, including an
//! aggregate such as `sum(x) OVER ()` — is only valid in the SELECT list or
//! ORDER BY of a windowed query. Used in `WHERE` or `HAVING`, SQLite reports
//! `misuse of window function NAME()`. Previously graphite reported this only
//! for the ranking window functions (`row_number()` etc.); an aggregate with an
//! `OVER` clause fell through to the generic "aggregate … used outside an
//! aggregate context". Matched to the `sqlite3` CLI (3.50.4).

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
fn window_aggregate_in_where_or_having_is_misuse() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 2), (3, 4)").unwrap();

    for (sql, name) in [
        ("SELECT a FROM t WHERE sum(b) OVER () > 0", "sum"),
        (
            "SELECT a FROM t WHERE count(*) OVER (ORDER BY a) > 0",
            "count",
        ),
        ("SELECT a FROM t WHERE avg(b) OVER () > 0", "avg"),
        (
            "SELECT a FROM t GROUP BY a HAVING sum(b) OVER () > 0",
            "sum",
        ),
        // Ranking window functions were already covered; keep them green.
        (
            "SELECT a FROM t WHERE row_number() OVER () > 0",
            "row_number",
        ),
    ] {
        assert_eq!(
            graphite_err(&c, sql),
            format!("misuse of window function {name}()"),
            "for {sql}"
        );
    }

    // Valid window usage in the SELECT list and ORDER BY still works.
    assert!(c
        .query("SELECT a, sum(b) OVER () FROM t ORDER BY a")
        .is_ok());
    assert!(c.query("SELECT a FROM t ORDER BY sum(b) OVER ()").is_ok());
    assert!(c
        .query("SELECT avg(a) OVER (PARTITION BY b) FROM t")
        .is_ok());
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-winmis-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,2),(3,4);";
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
        "SELECT a FROM t WHERE sum(b) OVER () > 0",
        "SELECT a FROM t WHERE count(*) OVER (ORDER BY a) > 0",
        "SELECT a FROM t GROUP BY a HAVING sum(b) OVER () > 0",
        "SELECT a FROM t WHERE row_number() OVER () > 0",
    ] {
        assert_eq!(graphite_err(&g, sql), sqlite_err(sql), "for {sql}");
    }
    let _ = std::fs::remove_file(&path);
}
