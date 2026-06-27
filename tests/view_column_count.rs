//! A `CREATE VIEW v(c1, …) AS SELECT …` with an explicit column list must
//! declare exactly as many columns as its body produces. SQLite accepts the
//! `CREATE VIEW` regardless, then errors `expected N columns for 'v' but got M`
//! whenever the view is *used* (in a FROM clause, a subquery, or a join).
//! graphite silently ignored the mismatch and returned the body's columns
//! unrenamed. A matching count — and a view with no explicit list — is
//! unaffected. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &mut Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn column_count_mismatch_errors_on_use() {
    let mut c = Connection::open_memory().unwrap();
    // Too few body columns.
    c.execute("CREATE VIEW a(x, y) AS SELECT 1").unwrap();
    assert_eq!(
        err(&mut c, "SELECT * FROM a"),
        "expected 2 columns for 'a' but got 1"
    );
    // Too many body columns.
    c.execute("CREATE VIEW b(x) AS SELECT 1, 2").unwrap();
    assert_eq!(
        err(&mut c, "SELECT * FROM b"),
        "expected 1 columns for 'b' but got 2"
    );
    // The error fires in a subquery and a join too.
    assert_eq!(
        err(&mut c, "SELECT (SELECT count(*) FROM a)"),
        "expected 2 columns for 'a' but got 1"
    );
    c.execute("CREATE TABLE t(z)").unwrap();
    assert_eq!(
        err(&mut c, "SELECT * FROM t, a"),
        "expected 2 columns for 'a' but got 1"
    );
}

#[test]
fn matching_or_absent_column_list_is_fine() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIEW v(x, y) AS SELECT 1, 2").unwrap();
    c.execute("CREATE VIEW w AS SELECT 1, 2, 3").unwrap();
    // Declared count matches the body; the names rename.
    let r = c.query("SELECT y, x FROM v").unwrap();
    assert_eq!(r.columns, ["y", "x"]);
    // No explicit list: the body's labels stand, any arity is fine.
    c.query("SELECT * FROM w").unwrap();
}

#[test]
fn temp_view_mismatch_also_errors() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TEMP VIEW v(x, y) AS SELECT 1").unwrap();
    assert_eq!(
        err(&mut c, "SELECT * FROM v"),
        "expected 2 columns for 'v' but got 1"
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
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "CREATE VIEW v(x,y) AS SELECT 1; SELECT * FROM v;",
        "CREATE VIEW v(x) AS SELECT 1,2; SELECT * FROM v;",
        "CREATE VIEW v(x,y) AS SELECT 1,2; SELECT * FROM v;",
        "CREATE VIEW v(x,y) AS SELECT 1,2,3; SELECT x FROM v;",
        "CREATE VIEW v(x,y) AS SELECT * FROM (SELECT 1 a); SELECT * FROM v;",
        "CREATE TABLE t(a,b,c); CREATE VIEW v(x,y) AS SELECT * FROM t; SELECT * FROM v;",
        "CREATE VIEW v(x,y) AS SELECT 1; SELECT (SELECT count(*) FROM v);",
        "CREATE TABLE t(a); CREATE VIEW v(x,y) AS SELECT 1; SELECT * FROM t,v;",
        "CREATE VIEW v(x,y) AS SELECT 1,2; SELECT x,y FROM v;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
