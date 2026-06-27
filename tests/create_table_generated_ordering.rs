//! When `CREATE TABLE` has several validation faults at once, SQLite reports
//! them in a fixed order that mirrors how it builds the schema: the per-column
//! "add column" checks fire first, left to right — duplicate name, then the
//! structural generated-column rules (no second `AS`, no `DEFAULT`, not part of
//! the PRIMARY KEY), then the `COLLATE` sequence — ahead of the end-of-table
//! checks. The very first end-of-table check is "must have at least one
//! non-generated column", which therefore outranks an unknown table option, a
//! prohibited subquery/aggregate, and any `no such column` resolution of a
//! CHECK / generated expression.
//!
//! graphite used to resolve a generated column's expression (`no such column`,
//! `subqueries prohibited`, …) before noticing the table was all-generated, and
//! it ran the duplicate-name / `COLLATE` checks after STRICT's datatype check.
//! Both are fixed here. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// graphite's error message for the `CREATE TABLE` `sql`, Display tag stripped.
fn err(sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    let e = c.execute(sql).unwrap_err().to_string();
    e.trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn all_generated_columns_is_caught_before_resolving_the_expression() {
    // The headline bug: an all-generated table whose expression references a
    // missing column reported `no such column` instead of the table-shape error.
    for sql in [
        "CREATE TABLE t(a GENERATED ALWAYS AS (b) STORED)",
        "CREATE TABLE t(a GENERATED ALWAYS AS (b) VIRTUAL)",
        "CREATE TABLE t(a AS (b), b AS (a))",
        "CREATE TABLE t(a AS (1))",
        "CREATE TABLE t(a AS (nosuchfn()))",
        "CREATE TABLE t(a AS (t.x))",
        "CREATE TABLE t(a AS (sum(1)))",
        "CREATE TABLE t(a AS ((SELECT 1)))",
        "CREATE TABLE t(a AS (1), CHECK(zzz))",
        "CREATE TABLE t(a AS (1)) FOO",
    ] {
        assert_eq!(
            err(sql),
            "must have at least one non-generated column",
            "for {sql}"
        );
    }
}

#[test]
fn per_column_checks_win_over_the_table_shape_and_strict_checks() {
    // Duplicate name / structural-generated / COLLATE all precede both the
    // all-generated check and STRICT's datatype check.
    assert_eq!(
        err("CREATE TABLE t(a AS (1), a AS (2))"),
        "duplicate column name: a"
    );
    assert_eq!(
        err("CREATE TABLE t(a, a) STRICT"),
        "duplicate column name: a"
    );
    assert_eq!(
        err("CREATE TABLE t(a AS (1) PRIMARY KEY) STRICT"),
        "generated columns cannot be part of the PRIMARY KEY"
    );
    assert_eq!(
        err("CREATE TABLE t(a AS (1) DEFAULT 5) STRICT"),
        "cannot use DEFAULT on a generated column"
    );
    assert_eq!(
        err("CREATE TABLE t(a COLLATE nope) STRICT"),
        "no such collation sequence: nope"
    );
    // …but the all-generated check still loses to STRICT's datatype check
    // (that is an end-of-table check that runs ahead of it).
    assert_eq!(
        err("CREATE TABLE t(a AS (1)) STRICT"),
        "missing datatype for t.a"
    );
}

#[test]
fn positional_interleaving_is_preserved() {
    // An earlier column's fault wins over a later column's, but within one
    // column the duplicate name is reported before its own COLLATE.
    assert_eq!(
        err("CREATE TABLE t(a COLLATE nope, a)"),
        "no such collation sequence: nope"
    );
    assert_eq!(
        err("CREATE TABLE t(a, a COLLATE nope)"),
        "duplicate column name: a"
    );
    assert_eq!(
        err("CREATE TABLE t(a AS (1) PRIMARY KEY, a)"),
        "generated columns cannot be part of the PRIMARY KEY"
    );
    assert_eq!(
        err("CREATE TABLE t(a, a AS (1) PRIMARY KEY)"),
        "duplicate column name: a"
    );
}

#[test]
fn legitimate_generated_tables_still_build() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x, a AS (x + 1) STORED, b AS (x * 2) VIRTUAL)")
        .unwrap();
    c.execute("INSERT INTO t(x) VALUES (5)").unwrap();
    let row = c.query("SELECT a, b FROM t").unwrap().rows;
    assert_eq!(row[0][0], graphitesql::Value::Integer(6));
    assert_eq!(row[0][1], graphitesql::Value::Integer(10));
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
        // All-generated outranks expression resolution / option / subquery.
        "CREATE TABLE t(a GENERATED ALWAYS AS (b) STORED)",
        "CREATE TABLE t(a GENERATED ALWAYS AS (b) VIRTUAL)",
        "CREATE TABLE t(a AS (1), b AS (2))",
        "CREATE TABLE t(a AS (nosuchfn()))",
        "CREATE TABLE t(a AS (t.x))",
        "CREATE TABLE t(a AS (sum(1)))",
        "CREATE TABLE t(a AS ((SELECT 1)))",
        "CREATE TABLE t(a AS (nope + (SELECT 1)))",
        "CREATE TABLE t(a AS (1), CHECK(zzz))",
        "CREATE TABLE t(a AS (1), CHECK((SELECT 1)))",
        "CREATE TABLE t(a AS (1), CHECK(sum(a)))",
        "CREATE TABLE t(a AS (1)) FOO",
        // Per-column checks (positional) outrank the all-generated and STRICT checks.
        "CREATE TABLE t(a AS (1), a AS (2))",
        "CREATE TABLE t(a, a) STRICT",
        "CREATE TABLE t(a INT, a INT) STRICT",
        "CREATE TABLE t(a AS (1) PRIMARY KEY) STRICT",
        "CREATE TABLE t(a AS (1) DEFAULT 5) STRICT",
        "CREATE TABLE t(a AS (1) AS (2)) STRICT",
        "CREATE TABLE t(a COLLATE nope) STRICT",
        "CREATE TABLE t(a AS (1) PRIMARY KEY) FOO",
        "CREATE TABLE t(a, a) FOO",
        "CREATE TABLE t(a AS (1) COLLATE nope) FOO",
        "CREATE TABLE t(a AS ((SELECT 1)) PRIMARY KEY)",
        "CREATE TABLE t(a AS ((SELECT 1)) DEFAULT 5)",
        "CREATE TABLE t(a AS (random()) PRIMARY KEY)",
        // Positional interleaving across and within columns.
        "CREATE TABLE t(a COLLATE nope, a)",
        "CREATE TABLE t(a, a COLLATE nope)",
        "CREATE TABLE t(a AS (1) PRIMARY KEY, a)",
        "CREATE TABLE t(a, a AS (1) PRIMARY KEY)",
        "CREATE TABLE t(a AS (nope), b, a)",
        "CREATE TABLE t(x COLLATE nope, a AS (1) PRIMARY KEY)",
        // STRICT datatype still wins over the all-generated check.
        "CREATE TABLE t(a AS (1)) STRICT",
        // A real column present → the generated expression *is* resolved.
        "CREATE TABLE t(x, a AS (b))",
        "CREATE TABLE t(x, a AS (b) COLLATE nope)",
        // Legitimate tables compute the right values.
        "CREATE TABLE t(x, a AS (x+1) STORED); INSERT INTO t(x) VALUES(5); SELECT a FROM t",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
