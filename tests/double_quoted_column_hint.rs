//! A double-quoted identifier (`"foo"`) is ambiguous with a string literal, so
//! when it is used where a column is expected and fails to resolve, SQLite both
//! re-quotes the name and appends a hint:
//!
//! ```text
//! no such column: "foo" - should this be a string literal in single-quotes?
//! ```
//!
//! The hint fires only for the **double-quote** form: a bare word, a
//! `[bracketed]` or `` `backtick` `` identifier, and any **table-qualified**
//! reference all keep the plain `no such column: NAME` / `no such column:
//! t.col`. graphite previously rendered every unresolved reference with the
//! plain wording. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The first line of the library's error message for `sql` (the statements are
/// run in order; the error from the last one is returned).
fn err_msg(sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    let mut last = String::new();
    for stmt in sql.split(';').filter(|s| !s.trim().is_empty()) {
        // A SELECT goes through `query`; everything else through `execute`.
        let r = if stmt
            .trim_start()
            .get(..6)
            .is_some_and(|k| k.eq_ignore_ascii_case("select"))
        {
            c.query(stmt).map(|_| ())
        } else {
            c.execute(stmt).map(|_| ())
        };
        if let Err(e) = r {
            last = e.to_string();
        }
    }
    // The library renders parse/exec errors with a leading tag we strip.
    last.trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn double_quoted_unresolved_column_gets_the_hint() {
    let hint = |n: &str| {
        format!("no such column: \"{n}\" - should this be a string literal in single-quotes?")
    };
    // Bare result column, no FROM.
    assert_eq!(err_msg("SELECT \"foo\""), hint("foo"));
    // A name with a space can only be a quoted identifier, never a bare word.
    assert_eq!(err_msg("SELECT \"a b\""), hint("a b"));
    // Against a real table: result column, WHERE, GROUP BY, ORDER BY.
    assert_eq!(err_msg("CREATE TABLE t(a); SELECT \"b\" FROM t"), hint("b"));
    assert_eq!(
        err_msg("CREATE TABLE t(a); SELECT * FROM t WHERE \"x\"=1"),
        hint("x")
    );
    assert_eq!(
        err_msg("CREATE TABLE t(a,b); SELECT b FROM t GROUP BY \"g\""),
        hint("g")
    );
    assert_eq!(
        err_msg("CREATE TABLE t(a); SELECT a FROM t ORDER BY \"o\""),
        hint("o")
    );
    // DML: UPDATE SET value and DELETE WHERE.
    assert_eq!(
        err_msg("CREATE TABLE t(a); UPDATE t SET a=\"z\""),
        hint("z")
    );
    assert_eq!(
        err_msg("CREATE TABLE t(a); DELETE FROM t WHERE \"q\"=1"),
        hint("q")
    );
}

#[test]
fn other_quote_styles_and_qualified_refs_keep_the_plain_wording() {
    // Backtick and bracket identifiers are never string-literal candidates.
    assert_eq!(err_msg("SELECT `foo`"), "no such column: foo");
    assert_eq!(err_msg("SELECT [foo]"), "no such column: foo");
    // A table-qualified reference never gets the hint, even double-quoted.
    assert_eq!(err_msg("SELECT \"m\".\"c\""), "no such column: m.c");
    // A bare word keeps the plain wording.
    assert_eq!(
        err_msg("CREATE TABLE t(a); SELECT zzz FROM t"),
        "no such column: zzz"
    );
}

#[test]
fn a_double_quoted_name_that_resolves_is_unaffected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 2)").unwrap();
    // `"a"` names a real column — resolves normally, no error.
    let rows = c.query("SELECT \"a\", \"b\" FROM t").unwrap();
    assert_eq!(rows.rows.len(), 1);
    // A double-quoted rowid alias still resolves to the rowid.
    c.query("SELECT \"rowid\" FROM t").unwrap();
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // The first non-caret error line, with the CLI's framing stripped.
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            // graphite's CLI doubles the tag for the `Error::Error` variant
            // (`Error: error: …`); the library message is identical.
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT \"foo\"",
        "SELECT \"a b\"",
        "CREATE TABLE t(a); SELECT \"b\" FROM t",
        "CREATE TABLE t(a); SELECT * FROM t WHERE \"x\"=1",
        "CREATE TABLE t(a); UPDATE t SET a=\"z\"",
        "CREATE TABLE t(a); DELETE FROM t WHERE \"q\"=1",
        "SELECT `foo`",
        "SELECT [foo]",
        "SELECT \"m\".\"c\"",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
