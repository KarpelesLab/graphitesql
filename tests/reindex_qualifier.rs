//! `REINDEX [schema.]name` validates its `schema.` qualifier ahead of the
//! object lookup: an unknown database is rejected with `unknown database
//! <name>`, exactly as SQLite does (and as `VACUUM`/`ATTACH` already did).
//! graphite used to drop the qualifier entirely and report the generic
//! `unable to identify the object to be reindexed` for a bad database.
//!
//! Two further parity points fall out of carrying the qualifier:
//!   * a *known* database with an unidentifiable object still reports `unable to
//!     identify the object to be reindexed` (`main.nope`);
//!   * a collation name may only be used *unqualified* — `REINDEX nocase` is a
//!     no-op, but `REINDEX main.nocase` is `unable to identify …` (a collation
//!     is not a per-database object).
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    for line in s.lines() {
        let mut t = line.trim_end();
        if t.trim_start().starts_with('^') {
            continue;
        }
        for prefix in [
            "Error: ",
            "in prepare, ",
            "stepping, ",
            "SQL error: ",
            "error: ",
        ] {
            t = t.strip_prefix(prefix).unwrap_or(t);
        }
        if let Some(rest) = t.strip_prefix("Parse error") {
            t = rest.split_once(": ").map_or(rest, |(_, m)| m);
        }
        if let Some(rest) = t.strip_prefix("Runtime error") {
            t = rest.split_once(": ").map_or(rest, |(_, m)| m);
        }
        return t.to_string();
    }
    String::new()
}

#[test]
fn reindex_qualifier_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let idx = "CREATE TABLE t(a,b); CREATE INDEX i ON t(a);";
    let cases: &[String] = &[
        // Unknown database qualifier → unknown database, ahead of object lookup.
        "REINDEX nope.idx".into(),
        "REINDEX nope.nocase".into(),
        // Known database, unidentifiable object.
        "REINDEX main.nope".into(),
        "REINDEX temp.x".into(),
        // Collation: valid bare, invalid when qualified.
        "REINDEX nocase".into(),
        "REINDEX rtrim".into(),
        "REINDEX BINARY".into(),
        "REINDEX main.nocase".into(),
        // Plain unidentifiable / whole-database / valid targets.
        "REINDEX".into(),
        "REINDEX nope".into(),
        format!("{idx} REINDEX i"),
        format!("{idx} REINDEX main.i"),
        format!("{idx} REINDEX t"),
        format!("{idx} REINDEX main.t"),
    ];
    for sql in cases {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
