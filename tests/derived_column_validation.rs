//! A query whose sole FROM source is a derived table (a parenthesized subquery)
//! must resolve its top-level column references against that derived table's
//! output columns at prepare time, like sqlite — so a reference to a column the
//! subquery does not expose (`SELECT a FROM (SELECT a FROM t) WHERE zzz = 1`)
//! errors even when the derived table yields no rows. graphite previously
//! resolved these per row, so an empty or fully-filtered derived table silently
//! accepted the bad name. Unlike a base table a subquery has no `rowid`, so even
//! `rowid` is "no such column". Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

/// Run `sql` and return the error message with the library's `error: ` framing
/// stripped, so it compares to sqlite's bare text.
fn err(c: &Connection, sql: &str) -> String {
    let e = c.query(sql).unwrap_err().to_string();
    e.strip_prefix("error: ").unwrap_or(&e).to_string()
}

#[test]
fn rejects_unknown_column_over_empty_derived_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // Empty derived table: the bad name must still be caught at prepare time.
    assert_eq!(
        err(&c, "SELECT a FROM (SELECT a FROM t) WHERE zzz = 1"),
        "no such column: zzz"
    );
    // A base column the derived table dropped is not in scope.
    assert_eq!(
        err(&c, "SELECT a FROM (SELECT a FROM t) WHERE b = 1"),
        "no such column: b"
    );
    // Result-list, ORDER BY, and GROUP BY positions are all checked.
    assert_eq!(
        err(&c, "SELECT zzz FROM (SELECT a FROM t)"),
        "no such column: zzz"
    );
    assert_eq!(
        err(&c, "SELECT a FROM (SELECT a FROM t) ORDER BY zzz"),
        "no such column: zzz"
    );
    assert_eq!(
        err(&c, "SELECT a FROM (SELECT a FROM t) GROUP BY zzz"),
        "no such column: zzz"
    );
}

#[test]
fn qualifier_and_rowid_rules() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    // A qualifier must name the derived table's alias.
    assert_eq!(
        err(&c, "SELECT d.zzz FROM (SELECT a FROM t) d"),
        "no such column: d.zzz"
    );
    assert_eq!(
        err(&c, "SELECT x.a FROM (SELECT a FROM t) d"),
        "no such column: x.a"
    );
    // A `tbl.*` whose qualifier names no source is `no such table`.
    assert_eq!(
        err(&c, "SELECT q.* FROM (SELECT a FROM t) d"),
        "no such table: q"
    );
    // A subquery has no rowid, so `rowid` is just another missing column.
    assert_eq!(
        err(&c, "SELECT rowid FROM (SELECT a FROM t)"),
        "no such column: rowid"
    );
}

#[test]
fn does_not_reject_valid_references() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 2)").unwrap();
    // Plain, aliased, qualified, wildcard, and output-alias references all resolve.
    assert!(
        c.query("SELECT a, b FROM (SELECT a, b FROM t) WHERE a = 1")
            .is_ok()
    );
    assert!(
        c.query("SELECT x FROM (SELECT a AS x FROM t) WHERE x = 1")
            .is_ok()
    );
    assert!(
        c.query("SELECT a FROM (SELECT a FROM t) d WHERE d.a = 1")
            .is_ok()
    );
    assert!(c.query("SELECT * FROM (SELECT a FROM t)").is_ok());
    assert!(
        c.query("SELECT a + b AS s FROM (SELECT a, b FROM t) ORDER BY s")
            .is_ok()
    );
    // A derived `SELECT *` exposes every base column by name.
    assert!(
        c.query("SELECT b FROM (SELECT * FROM t) WHERE a = 1")
            .is_ok()
    );
    // A VALUES-derived table exposes column1, column2, ….
    assert!(c.query("SELECT column1 FROM (VALUES (1), (2))").is_ok());
}

#[test]
fn matches_sqlite_cli() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // sqlite's one-shot CLI appends a caret annotation line to a prepare error;
    // keep only the first line (the message itself), as the differential corpus does.
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        let e = String::from_utf8_lossy(&out.stderr);
        let first = format!("{s}{e}");
        first
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Parse error: ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    for sql in [
        "CREATE TABLE t(a,b); SELECT a FROM (SELECT a FROM t) WHERE zzz=1;",
        "CREATE TABLE t(a,b); SELECT a FROM (SELECT a FROM t) WHERE b=1;",
        "CREATE TABLE t(a); SELECT d.zzz FROM (SELECT a FROM t) d;",
        "CREATE TABLE t(a); SELECT q.* FROM (SELECT a FROM t) d;",
        "CREATE TABLE t(a); SELECT rowid FROM (SELECT a FROM t);",
        "CREATE TABLE t(a); SELECT a FROM (SELECT a FROM t) ORDER BY zzz;",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT b FROM (SELECT * FROM t) WHERE a=1;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
