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
    assert!(
        c.query("SELECT a, sum(b) OVER () FROM t ORDER BY a")
            .is_ok()
    );
    assert!(c.query("SELECT a FROM t ORDER BY sum(b) OVER ()").is_ok());
    assert!(
        c.query("SELECT avg(a) OVER (PARTITION BY b) FROM t")
            .is_ok()
    );
}

/// Run a statement through the method that accepts it (`query` for `SELECT`,
/// `execute` otherwise) and return the resulting error string.
fn err_of(c: &mut Connection, sql: &str) -> String {
    let r = if sql.trim_start()[..6].eq_ignore_ascii_case("select") {
        c.query(sql).map(|_| ())
    } else {
        c.execute(sql).map(|_| ())
    };
    r.unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn window_misuse_is_rejected_even_on_empty_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // The table is empty, so lazy per-row evaluation would never fire — these
    // must still be rejected at prepare time (a GROUP BY/HAVING/WHERE position,
    // and an UPDATE/DELETE expression, all forbid a window function).
    for (sql, name) in [
        (
            "SELECT a FROM t WHERE row_number() OVER () > 0",
            "row_number",
        ),
        (
            "SELECT a FROM t GROUP BY a HAVING rank() OVER (ORDER BY a) > 0",
            "rank",
        ),
        (
            "SELECT a FROM t WHERE a > 0 GROUP BY row_number() OVER ()",
            "row_number",
        ),
        ("DELETE FROM t WHERE rank() OVER (ORDER BY a) > 0", "rank"),
        ("UPDATE t SET a = row_number() OVER ()", "row_number"),
        (
            "UPDATE t SET a = 1 WHERE dense_rank() OVER (ORDER BY a) > 0",
            "dense_rank",
        ),
    ] {
        assert_eq!(
            err_of(&mut c, sql),
            format!("misuse of window function {name}()"),
            "for {sql}"
        );
    }
    // A window function in a subquery belongs to that level — not a misuse here.
    c.query("SELECT a FROM t WHERE a IN (SELECT row_number() OVER () FROM t)")
        .unwrap();
}

#[test]
fn matches_sqlite_cli_empty_table_and_dml() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    // The table is left empty: the point is that the rejection is at prepare
    // time, identical between graphite and the CLI even when no row is scanned.
    let g_bin = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let full = format!("CREATE TABLE t(a, b); {sql}");
        let out = Command::new(bin)
            .arg(":memory:")
            .arg(&full)
            .output()
            .unwrap();
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
            .trim_end()
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches('(')
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT a FROM t WHERE row_number() OVER () > 0",
        "SELECT a FROM t GROUP BY a HAVING rank() OVER (ORDER BY a) > 0",
        "SELECT a FROM t WHERE a > 0 GROUP BY row_number() OVER ()",
        "DELETE FROM t WHERE rank() OVER (ORDER BY a) > 0",
        "UPDATE t SET a = row_number() OVER ()",
        "UPDATE t SET a = 1 WHERE dense_rank() OVER (ORDER BY a) > 0",
        // legitimate — runs (no output) in both
        "SELECT a, row_number() OVER (ORDER BY a) FROM t",
        "SELECT a FROM t ORDER BY row_number() OVER ()",
    ] {
        assert_eq!(run("sqlite3", sql), run(g_bin, sql), "for {sql}");
    }
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
