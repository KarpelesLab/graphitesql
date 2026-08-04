//! Differential test for `regexp()` / the `X REGEXP Y` operator against the
//! sqlite3 3.50.4 CLI.
//!
//! graphite ports SQLite's own regular-expression engine (`ext/misc/regexp.c`),
//! which is *not* POSIX ERE or PCRE. `X REGEXP Y` desugars to `regexp(Y, X)`
//! (pattern first, subject second), returns 1/0, NULL if either arg is NULL, and
//! raises SQLite's exact error text for an invalid pattern. Every case below must
//! be byte-identical to the sqlite3 CLI (both stdout and stderr).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run one SQL statement and capture stdout+stderr merged, so both match results
/// (stdout) and pattern-compile errors (stderr) are compared.
fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s
}

fn sql_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[test]
fn regexp_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    let patterns = [
        "a.c",
        "^abc$",
        "a+",
        "a*b",
        "[a-z]+",
        "a{2,3}",
        "(foo|bar)",
        r"\d+",
        r"\w+",
        "[^0-9]",
        ".*",
        "",
        "colou?r",
        r"^\d{3}-\d{4}$",
        r"\bcat\b",
        "a|b|c",
        "[A-Za-z_][A-Za-z0-9_]*",
        "x{0,2}y",
        r"\s+",
        r"\S+",
        "a.*z",
        "^$",
        ".",
        r"\D+",
        r"\W",
        "[abc]",
        "(ab)+",
        r"^a\.c$",
        r"\x41",
        r"A",
        "é",
        "ca.é",
        "^",
        "$",
        "[a-fA-F0-9]",
        "a{2,}",
        "a{2}",
        ".{3}",
    ];
    let subjects = [
        "abc",
        "axc",
        "ac",
        "zzabczz",
        "aaab",
        "b",
        "foo",
        "bar",
        "baz",
        "123",
        "abc123",
        "a b",
        "color",
        "colour",
        "555-1234",
        "the cat sat",
        "category",
        "",
        "Hello_9",
        "aaaa",
        "a__z",
        "café",
        "A",
        "aa",
        "aaaaa",
        "\t",
        "A1b2",
    ];

    let mut sql = String::new();
    for p in patterns {
        for s in subjects {
            sql.push_str(&format!(
                "SELECT CASE WHEN {} REGEXP {} THEN 1 ELSE 0 END;",
                sql_lit(s),
                sql_lit(p)
            ));
        }
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}

#[test]
fn regexp_function_form_and_nulls_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let sql = "\
        SELECT regexp('a.c','abc');\
        SELECT regexp('^x','abc');\
        SELECT NULL REGEXP 'a';\
        SELECT 'a' REGEXP NULL;\
        SELECT NULL REGEXP NULL;\
        SELECT regexp(NULL,'a');\
        SELECT regexp('a',NULL);\
        SELECT 123 REGEXP '2';\
        SELECT 12.5 REGEXP '2\\.5';\
        SELECT typeof('abc' REGEXP 'a');\
        ";
    assert_eq!(out("sqlite3", sql), out(g, sql));
}

#[test]
fn regexp_invalid_patterns_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Each invalid pattern must produce the same error text as sqlite.
    let bad = [
        "(",
        "*",
        "+",
        "?",
        "[abc",
        "a{3,2}",
        r"\q",
        "a{0,0}",
        "{2}",
        "a{",
        "[[:alpha:]]",
        "a)",
    ];
    for p in bad {
        let sql = format!("SELECT 'abc' REGEXP {};", sql_lit(p));
        assert_eq!(out("sqlite3", &sql), out(g, &sql), "pattern {p:?}");
    }
}
