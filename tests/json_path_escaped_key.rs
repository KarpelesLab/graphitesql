//! A quoted object-key in a JSON path may contain backslash escapes — most
//! importantly `\"` (an escaped double quote), but also `\\`, `\/`, `\n`, and
//! `\uXXXX`. SQLite scans past `\<char>` when finding the closing quote and
//! compares the label with escapes decoded, so `$."a\"b"` selects the key `a"b`.
//! graphite's path parser stopped at the escaped quote and reported `bad JSON
//! path`. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn json_path_quoted_key_escapes() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Each JSON literal / path pair is written with doubled backslashes so the
    // shell/arg layer delivers a single backslash to the engines.
    let cases = [
        r#"SELECT quote(json_extract('{"a\"b":5}', '$."a\"b"'));"#,
        r#"SELECT quote(json_extract('{"a\\b":3}', '$."a\\b"'));"#,
        r#"SELECT quote(json_set('{}', '$."a\"b"', 9));"#,
        r#"SELECT quote(json_remove('{"a\"b":1,"c":2}', '$."a\"b"'));"#,
        r#"SELECT quote(json_type('{"a\"b":[1,2]}', '$."a\"b"'));"#,
        // Escapes coexisting with ordinary quoted keys.
        r#"SELECT quote(json_extract('{"x y":7}', '$."x y"'));"#,
        r#"SELECT quote(json_extract('{"a.b":8}', '$."a.b"'));"#,
        // A trailing backslash is still a bad path in both engines.
        r#"SELECT json_extract('{"a":1}', '$."a\');"#,
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for `{sql}`");
    }
}
