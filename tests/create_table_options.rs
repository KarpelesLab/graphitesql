//! Table options after the column list — `WITHOUT ROWID` and `STRICT`, a
//! possibly-empty comma-separated list in any order. SQLite reports any *other*
//! name in option position as `unknown table option: NAME` (rendered verbatim,
//! including quotes); a non-name token there is a plain `near "TOKEN"` syntax
//! error. The unrecognized name is surfaced *after* a STRICT table's
//! missing/invalid-datatype check, so e.g. `CREATE TABLE t(a) STRICT, FOO`
//! reports the missing datatype on `a`, not the bad option. graphite previously
//! rejected any unknown option with a generic `near "NAME"` syntax error.
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The library's error message for `sql`, with the outer Display tag stripped.
fn err_msg(sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    let e = c.execute(sql).unwrap_err().to_string();
    e.trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn unknown_table_option_is_named() {
    // A bare word, a quoted identifier and a string literal are all "names" and
    // are echoed verbatim (with their quotes).
    assert_eq!(
        err_msg("CREATE TABLE t(a) FOO"),
        "unknown table option: FOO"
    );
    assert_eq!(
        err_msg("CREATE TABLE t(a) \"FOO\""),
        "unknown table option: \"FOO\""
    );
    assert_eq!(
        err_msg("CREATE TABLE t(a) 'str'"),
        "unknown table option: 'str'"
    );
    // `WITHOUT` must be followed by `ROWID`; any other name is unknown (and the
    // partner name, not `WITHOUT X`, is what's reported).
    assert_eq!(
        err_msg("CREATE TABLE t(a) WITHOUT FOO"),
        "unknown table option: FOO"
    );
}

#[test]
fn first_bad_option_wins_and_suppresses_later_flags() {
    // `FOO` is reported even though a valid `STRICT` follows — the table never
    // enters STRICT mode, so there is no missing-datatype error.
    assert_eq!(
        err_msg("CREATE TABLE t(a) FOO, STRICT"),
        "unknown table option: FOO"
    );
    assert_eq!(
        err_msg("CREATE TABLE t(a) FOO, WITHOUT ROWID"),
        "unknown table option: FOO"
    );
    assert_eq!(
        err_msg("CREATE TABLE t(a) FOO, BAR"),
        "unknown table option: FOO"
    );
}

#[test]
fn strict_datatype_check_precedes_a_trailing_bad_option() {
    // STRICT is set first, so the untyped column `a` is rejected before the bad
    // option is reached.
    assert_eq!(
        err_msg("CREATE TABLE t(a) STRICT, FOO"),
        "missing datatype for t.a"
    );
    // With a typed column the STRICT check passes and the bad option surfaces.
    assert_eq!(
        err_msg("CREATE TABLE t(a INT) STRICT, FOO"),
        "unknown table option: FOO"
    );
}

#[test]
fn trailing_comma_is_incomplete_input() {
    assert_eq!(err_msg("CREATE TABLE t(a) STRICT,"), "incomplete input");
    assert_eq!(
        err_msg("CREATE TABLE t(a PRIMARY KEY) WITHOUT ROWID,"),
        "incomplete input"
    );
}

#[test]
fn valid_option_lists_still_parse() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x PRIMARY KEY) WITHOUT ROWID")
        .unwrap();
    c.execute("CREATE TABLE b(x INT) STRICT").unwrap();
    c.execute("CREATE TABLE c(x INT PRIMARY KEY) STRICT, WITHOUT ROWID")
        .unwrap();
    c.execute("CREATE TABLE d(x INT PRIMARY KEY) WITHOUT ROWID, STRICT")
        .unwrap();
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
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "CREATE TABLE t(a) FOO",
        "CREATE TABLE t(a) \"FOO\"",
        "CREATE TABLE t(a) 'str'",
        "CREATE TABLE t(a) WITHOUT FOO",
        "CREATE TABLE t(a) FOO, STRICT",
        "CREATE TABLE t(a) STRICT, FOO",
        "CREATE TABLE t(a INT) STRICT, FOO",
        "CREATE TABLE t(a) STRICT,",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
