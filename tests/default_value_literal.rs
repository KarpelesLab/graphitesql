//! A column `DEFAULT` value written *without* parentheses is a single literal
//! term — an optionally-signed number, string, blob, `NULL`, `TRUE`/`FALSE`, or a
//! `CURRENT_*` keyword — not a general expression. So a trailing `NOT NULL` /
//! `COLLATE` / … is the *next column constraint*, and a compound value
//! (`DEFAULT 1+1`, `DEFAULT abs(1)`) is a syntax error (parentheses required).
//!
//! graphite used to parse a full expression here, which greedily swallowed the
//! following constraint as a postfix operator — `DEFAULT 'x' NOT NULL` became
//! `DEFAULT ('x' IS NOT NULL)` with the `NOT NULL` constraint silently dropped —
//! and wrongly accepted `DEFAULT 1+1`. This checks the fix differentially against
//! sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

/// Strip each CLI's error-line prefix / caret so the shared message compares equal.
fn norm(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim_start().starts_with("CREATE TABLE") && !l.contains("error here"))
        .map(|l| {
            l.strip_prefix("Error: in prepare, ")
                .or_else(|| l.strip_prefix("Error: stepping, "))
                .or_else(|| l.strip_prefix("Error: error: "))
                .or_else(|| l.strip_prefix("Error: SQL error: "))
                .or_else(|| l.strip_prefix("Error: "))
                .unwrap_or(l)
                // drop a trailing ` (NN)` sqlite result-code suffix
                .trim_end_matches(|c: char| c.is_ascii_digit())
                .trim_end_matches(" (")
                .trim_end()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn unparenthesized_default_is_a_literal_term() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    // A trailing constraint after the default value is preserved (notnull column of
    // table_info flips to 1, the default stays the literal) — byte-exact vs sqlite.
    let matching = [
        "CREATE TABLE t(b TEXT DEFAULT 'x' NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(b INT DEFAULT 5 NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(b DEFAULT -5 NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(b DEFAULT -3.5 NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(b DEFAULT +5 NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(b DEFAULT x'ab' NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(b DEFAULT NULL NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(b DEFAULT -9223372036854775808 NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(b DEFAULT 'x' UNIQUE); PRAGMA table_info(t)",
        "CREATE TABLE t(b DEFAULT 'x' COLLATE NOCASE); PRAGMA table_info(t)",
        // The parenthesized form still allows a compound expression + a constraint.
        "CREATE TABLE t(b DEFAULT ('x') NOT NULL); PRAGMA table_info(t)",
        // The default value is actually applied and the NOT NULL is actually enforced.
        "CREATE TABLE t(a, b TEXT DEFAULT 'x' NOT NULL); INSERT INTO t(a) VALUES(1); SELECT * FROM t",
    ];
    for sql in matching {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }

    // An unparenthesized compound value is a syntax error at the same token in both.
    let errors = [
        "CREATE TABLE t(b DEFAULT 1+1)",
        "CREATE TABLE t(b DEFAULT abs(1))",
        "CREATE TABLE t(b DEFAULT 'a' 'b')",
    ];
    for sql in errors {
        let s = norm(&out("sqlite3", sql));
        let gr = norm(&out(g, sql));
        assert!(
            s.contains("syntax error"),
            "sqlite should reject {sql}: {s}"
        );
        assert_eq!(s, gr, "error mismatch for {sql}");
    }
}

#[test]
fn table_info_reproduces_default_source_text() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // `PRAGMA table_info`'s `dflt_value` reproduces the default's verbatim source
    // text, not a re-rendering of the parsed expression — so a hex literal stays
    // `0x1F` (not `31`), scientific notation stays `-1.5e3` (not `-1500.0`), a
    // boolean stays `TRUE` (not `1`), `CURRENT_TIMESTAMP` is not rewritten to
    // `datetime('now')`, and a parenthesized value keeps its inner text with exactly
    // one outer-paren layer stripped and inner whitespace preserved.
    let cases = [
        "CREATE TABLE t(a DEFAULT 0x1F); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT -1.5e3 NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT TRUE); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT FALSE NOT NULL); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT CURRENT_TIMESTAMP); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT current_date); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT (1+1)); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT ( 1  +  1 )); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT (( 1 ))); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT (abs(-5))); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT ('x' || 'y')); PRAGMA table_info(t)",
        "CREATE TABLE t(a DEFAULT +5); PRAGMA table_info(t)",
        // The stored schema text is preserved verbatim too.
        "CREATE TABLE t(a DEFAULT 0x1F, b DEFAULT (1+1)); SELECT sql FROM sqlite_master",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
