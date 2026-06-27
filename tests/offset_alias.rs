//! `OFFSET` is not a reserved keyword in SQLite: it only introduces the optional
//! offset count *inside* a `LIMIT` clause. Everywhere else — as a result-column
//! alias, a table alias, or a column name — it is an ordinary identifier.
//!
//! graphite previously treated `offset` as reserved at the end of an expression
//! (the same class as `limit`/`order`/`where`), so `SELECT 1 offset` (an implicit
//! result-column alias) was rejected even though sqlite accepts it. Removing
//! `offset` from that set fixes the implicit-alias case while keeping
//! `SELECT 1 OFFSET 2` an error (the `2` is unexpected once `offset` is consumed
//! as the alias) — exactly matching the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run one SQL string against `bin :memory:` and return stdout (trimmed) or the
/// first non-caret error line with the CLI framing stripped.
fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        return stdout.trim_end().to_string();
    }
    let line = String::from_utf8_lossy(&out.stderr)
        .lines()
        .find(|l| !l.trim_start().starts_with('^'))
        .unwrap_or("")
        .trim_start_matches("Error: in prepare, ")
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .trim_end()
        .to_string();
    // Strip a trailing ` (NN)` result-code suffix that sqlite's CLI appends.
    match (line.rfind(" ("), line.ends_with(')')) {
        (Some(i), true)
            if line[i + 2..line.len() - 1]
                .chars()
                .all(|c| c.is_ascii_digit()) =>
        {
            line[..i].to_string()
        }
        _ => line,
    }
}

#[test]
fn offset_is_a_valid_implicit_alias() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    assert_eq!(run(g, "SELECT 1 offset"), "1");
    assert_eq!(run(g, "SELECT a offset FROM (SELECT 1 a)"), "1");
}

#[test]
fn offset_inside_limit_still_parses() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // 5 rows, skip 3, take 2 → rows 4 and 5.
    assert_eq!(
        run(
            g,
            "WITH t(x) AS (VALUES(1),(2),(3),(4),(5)) \
             SELECT x FROM t ORDER BY x LIMIT 2 OFFSET 3"
        ),
        "4\n5"
    );
}

#[test]
fn bare_offset_after_expr_consumes_it_as_alias() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // `offset` is taken as the column alias, leaving `2` unexpected.
    assert_eq!(run(g, "SELECT 1 OFFSET 2"), "near \"2\": syntax error");
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        "SELECT 1 offset",
        "SELECT 1 OFFSET 2",
        "SELECT 1 AS offset",
        "SELECT a offset FROM (SELECT 1 a)",
        "SELECT * FROM (SELECT 1) offset",
        "CREATE TABLE t(offset)",
        "WITH t(x) AS (VALUES(1),(2),(3),(4),(5)) \
         SELECT x FROM t ORDER BY x LIMIT 2 OFFSET 3",
    ];
    for sql in cases {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql:?}");
    }
}
