//! SQLite's `quote()` reads a text value through `sqlite3_value_text` — a
//! NUL-terminated C string — so an embedded NUL truncates the rendered literal,
//! even though the stored value keeps all its bytes. graphite quoted the full
//! byte sequence. Verified against the sqlite3 3.50.4 CLI (comparing `hex()` of
//! the result so the NUL survives the shell).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn hexout(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn quote_truncates_text_at_embedded_nul() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // The stored value keeps every byte …
        "SELECT hex(CAST(x'00' AS TEXT));",
        "SELECT hex('a'||CAST(x'00' AS TEXT)||'b');",
        // … but quote() stops at the first NUL.
        "SELECT hex(quote(CAST(x'00' AS TEXT)));",
        "SELECT hex(quote(x'00'||'x'));",
        "SELECT hex(quote('a'||CAST(x'00' AS TEXT)||'b'));",
        "SELECT hex(quote(char(97,0,98)));",
        // Ordinary text quoting is unaffected.
        "SELECT hex(quote('plain'));",
        "SELECT hex(quote('it''s a test'));",
    ];
    for sql in cases {
        assert_eq!(hexout("sqlite3", sql), hexout(g, sql), "for `{sql}`");
    }
}
