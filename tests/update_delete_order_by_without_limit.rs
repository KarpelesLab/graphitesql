//! `ORDER BY` on an UPDATE/DELETE is only meaningful as a companion to `LIMIT`
//! (the update/delete-limit extension): the order decides *which* rows the cap
//! keeps. SQLite rejects an `ORDER BY` with no `LIMIT` at prepare time with
//! `ORDER BY without LIMIT on UPDATE` / `... on DELETE`. graphite used to accept
//! it silently and do nothing. The check fires after the target's
//! existence / view checks but ahead of column resolution, so a bogus
//! `ORDER BY` / `SET` column never shadows the message. Verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// First non-caret line of combined stdout/stderr, error-prefix stripped.
fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next().unwrap_or("").trim_end().to_string();
    if !line.is_empty() {
        return line;
    }
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find(|l| !l.trim_start().starts_with('^'))
        .unwrap_or("")
        .trim_start_matches("Error: in prepare, ")
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .trim_end()
        .to_string()
}

#[test]
fn order_by_without_limit_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a);";
    for sql in [
        // UPDATE: ORDER BY with no LIMIT is rejected …
        &format!("{s} UPDATE t SET a=1 ORDER BY a"),
        &format!("{s} UPDATE t SET a=1 WHERE a>0 ORDER BY a"),
        &format!("{s} UPDATE t SET a=1 ORDER BY a DESC, a"),
        // … but ORDER BY *with* LIMIT, or LIMIT alone, is fine.
        &format!("{s} UPDATE t SET a=1 ORDER BY a LIMIT 1"),
        &format!("{s} UPDATE t SET a=1 LIMIT 1"),
        // DELETE: same rule.
        &format!("{s} DELETE FROM t ORDER BY a"),
        &format!("{s} DELETE FROM t WHERE a>0 ORDER BY a"),
        &format!("{s} DELETE FROM t ORDER BY a LIMIT 1"),
        &format!("{s} DELETE FROM t LIMIT 1"),
        // Precedence: the ORDER-BY/LIMIT check beats column resolution, so a
        // bogus ORDER BY or SET column still reports the limit error first.
        &format!("{s} UPDATE t SET a=1 ORDER BY zzz"),
        &format!("{s} UPDATE t SET zzz=1 ORDER BY a"),
        &format!("{s} DELETE FROM t ORDER BY zzz"),
        // … but a missing table and a view are diagnosed ahead of it.
        "UPDATE nope SET a=1 ORDER BY a",
        "DELETE FROM nope ORDER BY a",
        &format!("{s} CREATE VIEW v AS SELECT * FROM t; UPDATE v SET a=1 ORDER BY a"),
        // RETURNING does not change the rule.
        &format!("{s} UPDATE t SET a=1 ORDER BY a RETURNING a"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
