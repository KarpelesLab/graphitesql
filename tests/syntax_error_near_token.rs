//! A syntax error at a real token is reported by SQLite as
//! `near "TOKEN": syntax error`, where `TOKEN` is the verbatim source text of
//! the offending token. graphite previously surfaced its recursive-descent
//! parser's internal expectation (`expected keyword key (near byte 25, found
//! RParen)`, `unrecognized statement`, `expected an expression, found …`); every
//! such site now renders the SQLite-compatible `near "TOKEN": syntax error`.
//! Premature end-of-input remains `incomplete input` (see `incomplete_input.rs`).
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The library renders a parse error as `SQL error: <msg>`; return just `<msg>`.
fn parse_msg(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("SQL error: ")
        .to_string()
}

#[test]
fn syntax_errors_point_at_the_offending_token() {
    let c = Connection::open_memory().unwrap();
    // (sql, the token SQLite names). The token text is the verbatim source slice
    // — `)` for a `RParen`, the keyword as typed for a word.
    for (sql, tok) in [
        ("SELECT 1 2", "2"),
        ("CREATE TABLE t(a, FOREIGN)", ")"),
        ("CREATE TABLE t(a, CONSTRAINT)", ")"),
        ("FOO bar baz", "FOO"),
        ("SELECT CASE WHEN 1 END", "END"),
        ("CREATE FOO", "FOO"),
        ("DROP FOObar", "FOObar"),
        ("SELECT * FROM t t2 t3", "t3"),
        // A bare `VALUES` core takes no trailing ORDER BY / LIMIT (SQLite's
        // grammar attaches those only to the SELECT form of a query core).
        ("VALUES (1),(2) ORDER BY 1", "ORDER"),
        ("VALUES (1),(2) LIMIT 1", "LIMIT"),
        ("VALUES (1) UNION ALL VALUES (2) ORDER BY 1", "ORDER"),
        ("SELECT 1 UNION VALUES (2) ORDER BY 1", "ORDER"),
        (
            "WITH t AS (VALUES (1),(2) ORDER BY 1) SELECT * FROM t",
            "ORDER",
        ),
    ] {
        assert_eq!(
            parse_msg(&c, sql),
            format!("near \"{tok}\": syntax error"),
            "for {sql}"
        );
    }
}

#[test]
fn natural_join_with_on_or_using_is_one_message() {
    // SQLite collapses both the ON and USING cases into a single semantic
    // message (not a near-token syntax error). Real tables are needed so the
    // parse-time check is reached before any table-resolution error.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("CREATE TABLE b(x)").unwrap();
    let want = "a NATURAL join may not have an ON or USING clause";
    assert_eq!(
        parse_msg(&c, "SELECT * FROM a NATURAL JOIN b ON a.x=b.x"),
        want
    );
    assert_eq!(
        parse_msg(&c, "SELECT * FROM a NATURAL JOIN b USING (x)"),
        want
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
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT 1 2",
        "CREATE TABLE t(a, FOREIGN)",
        "CREATE TABLE t(a, CONSTRAINT)",
        "FOO bar baz",
        "SELECT CASE WHEN 1 END",
        "CREATE FOO",
        "DROP FOObar",
        "EXPLAIN QUERY x",
        "ALTER TABLE t FOO",
        "SELECT * FROM t t2 t3",
        // `VALUES` core rejects a trailing ORDER BY / LIMIT, including after a
        // compound whose last core is `VALUES`.
        "VALUES (1),(2) ORDER BY 1",
        "VALUES (1),(2) LIMIT 1",
        "VALUES (1) UNION ALL VALUES (2) ORDER BY 1",
        "SELECT 1 UNION VALUES (2) ORDER BY 1",
        "WITH t AS (VALUES (1),(2) ORDER BY 1) SELECT * FROM t",
        // …but these remain valid (last core is a SELECT, or the ORDER BY binds
        // to an outer SELECT) — no false syntax error.
        "VALUES (3),(1),(2) UNION SELECT 4 ORDER BY 1",
        "SELECT * FROM (VALUES (3),(1),(2)) ORDER BY 1",
        "VALUES (1),(2)",
        // a complete statement still parses (no false syntax error)
        "SELECT 1",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
