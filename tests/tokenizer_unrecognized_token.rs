//! A lexing failure — a stray character, an unterminated literal, a malformed
//! blob, or a number run with a trailing identifier — is reported by SQLite as
//! `unrecognized token: "X"`, where `X` is the verbatim source text from the
//! token's start to where the lexer gave up. graphite previously surfaced its
//! own lexer state (`unexpected character '^' at byte 7`, `unterminated string
//! literal at byte 11`, `invalid hex in blob literal at byte 12`, …); every
//! such site now renders the SQLite-compatible message. An unterminated block
//! comment runs off the end and is `incomplete input`, like any premature end.
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
fn lexing_failures_report_the_offending_source_run() {
    let c = Connection::open_memory().unwrap();
    // (sql, the verbatim token text SQLite echoes).
    for (sql, tok) in [
        ("SELECT ^", "^"),
        ("SELECT #", "#"),
        ("SELECT @", "@"),
        ("SELECT $", "$"),
        ("SELECT :", ":"),
        ("SELECT 'abc", "'abc"),
        ("SELECT \"abc", "\"abc"),
        ("SELECT x'zz'", "x'zz'"),
        ("SELECT x'1", "x'1"),
        ("SELECT x'1'", "x'1'"),
        ("SELECT 0xGG", "0xGG"),
        ("SELECT 0x", "0x"),
        ("SELECT 1e", "1e"),
        ("SELECT 123abc", "123abc"),
        ("SELECT 1.5xyz", "1.5xyz"),
        ("SELECT 12e3f", "12e3f"),
    ] {
        assert_eq!(
            parse_msg(&c, sql),
            format!("unrecognized token: \"{tok}\""),
            "for {sql}"
        );
    }
}

#[test]
fn hex_literal_too_big_is_its_own_message() {
    // A hex literal is a *recognized* token even when it overflows 64 bits;
    // SQLite then rejects it with a dedicated `hex literal too big: <literal>`
    // rather than the generic `unrecognized token`. The literal is echoed
    // verbatim (case preserved, `_` separators kept).
    let c = Connection::open_memory().unwrap();
    for (sql, lit) in [
        ("SELECT 0x10000000000000000", "0x10000000000000000"),
        ("SELECT 0x1ffffffffffffffff", "0x1ffffffffffffffff"),
        ("SELECT 0xFFFFFFFFFFFFFFFFF", "0xFFFFFFFFFFFFFFFFF"),
        ("SELECT 0x1_0000000000000000", "0x1_0000000000000000"),
        ("SELECT 0x10000000000000000 + 0", "0x10000000000000000"),
    ] {
        assert_eq!(
            parse_msg(&c, sql),
            format!("hex literal too big: {lit}"),
            "for {sql}"
        );
    }
    // The boundary value (exactly 64 bits) still lexes — it is -1 as i64.
    assert!(c.query("SELECT 0xffffffffffffffff").is_ok());
}

#[test]
fn unterminated_block_comment_is_incomplete_input() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(parse_msg(&c, "SELECT /* unterminated"), "incomplete input");
    assert_eq!(parse_msg(&c, "SELECT 1 /* x"), "incomplete input");
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
        "SELECT ^",
        "SELECT #",
        "SELECT @",
        "SELECT $",
        "SELECT :",
        "SELECT 'abc",
        "SELECT \"abc",
        "SELECT x'zz'",
        "SELECT x'1",
        "SELECT x'1'",
        "SELECT x'abc'",
        "SELECT 0xGG",
        "SELECT 0x",
        // A too-big hex literal is recognized then rejected with its own
        // message; the positive form matches sqlite verbatim. (A leading
        // unary minus, `-0x1…`, is a separate token to the lexer, so sqlite's
        // sign-prefixed `-0x1…` echo is a documented residual and not listed.)
        "SELECT 0x10000000000000000",
        "SELECT 0x1ffffffffffffffff",
        "SELECT 0xFFFFFFFFFFFFFFFFF",
        "SELECT 1e",
        "SELECT 123abc",
        "SELECT 1.5xyz",
        "SELECT 12e3f",
        "SELECT /* unterminated",
        // valid tokens still lex (no false rejection); typeof() keeps the
        // compared output to a type name, avoiding CLI value-rendering quirks
        // (e.g. a control-byte blob) that are unrelated to lexing.
        "SELECT typeof(x'1f')",
        "SELECT typeof(0x1f)",
        "SELECT typeof(1.5e3)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
