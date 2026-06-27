//! `sqlite_source_id()` returns the source-control identifier of the engine
//! build. SQLite reports its exact C source id; graphitesql is an independent
//! reimplementation, so it returns its own identifier in the same
//! `YYYY-MM-DD HH:MM:SS <hash>` shape (see `TARGET_SQLITE_SOURCE_ID`). The
//! exact bytes are build-specific and can't be compared across engines, but the
//! *contract* is build-invariant and is what we check here: a zero-arg call
//! yields non-NULL `text` whose leading field is a `YYYY-MM-DD HH:MM:SS`
//! timestamp, and any argument is an arity error — all matched against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn one(c: &Connection, sql: &str) -> String {
    let r = c.query(sql).unwrap();
    format!("{:?}", r.rows[0][0])
}

#[test]
fn returns_nonnull_text_in_the_source_id_shape() {
    let c = Connection::open_memory().unwrap();
    // typeof = text, non-NULL, non-empty.
    assert_eq!(
        one(&c, "SELECT typeof(sqlite_source_id())"),
        "Text(\"text\")"
    );
    assert_eq!(
        one(&c, "SELECT sqlite_source_id() IS NOT NULL"),
        "Integer(1)"
    );
    assert_eq!(
        one(&c, "SELECT length(sqlite_source_id()) > 0"),
        "Integer(1)"
    );

    // The leading field is a `YYYY-MM-DD HH:MM:SS` timestamp: positions 5/8 are
    // `-`, 11 is a space, 14/17 are `:` (1-based, as substr() counts).
    for (pos, sep) in [(5, "-"), (8, "-"), (11, " "), (14, ":"), (17, ":")] {
        assert_eq!(
            one(&c, &format!("SELECT substr(sqlite_source_id(),{pos},1)")),
            format!("Text({sep:?})"),
            "separator at position {pos}"
        );
    }
}

#[test]
fn any_argument_is_an_arity_error() {
    let c = Connection::open_memory().unwrap();
    let err = c
        .query("SELECT sqlite_source_id(1)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("wrong number of arguments to function sqlite_source_id()"),
        "got: {err}"
    );
}

#[test]
fn matches_sqlite_cli_on_the_build_invariant_contract() {
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
            // graphite's CLI doubles the prefix for the Error::Error variant
            // (`Error: error: …`); strip the inner one so the library messages
            // compare equal.
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    // Build-invariant queries only (never the literal source id, which differs
    // per build): the type/null/shape contract and the arity-error message.
    for sql in [
        "SELECT typeof(sqlite_source_id())",
        "SELECT sqlite_source_id() IS NOT NULL",
        "SELECT length(sqlite_source_id()) > 0",
        "SELECT substr(sqlite_source_id(),5,1)",
        "SELECT substr(sqlite_source_id(),8,1)",
        "SELECT substr(sqlite_source_id(),11,1)",
        "SELECT substr(sqlite_source_id(),14,1)",
        "SELECT substr(sqlite_source_id(),17,1)",
        "SELECT sqlite_source_id(1)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
