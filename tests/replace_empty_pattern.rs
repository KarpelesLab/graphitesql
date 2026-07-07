//! `replace(X, Y, Z)` short-circuits in a specific order in SQLite: a NULL subject
//! `X` or a NULL pattern `Y` yields NULL, but an EMPTY pattern `Y` returns the
//! subject (converted to text) *before* the replacement `Z` is examined — so
//! `replace('ab','',NULL)` is `'ab'`, and `replace(123,'',NULL)` is `'123'`, not
//! NULL. graphite checked all three arguments for NULL up front, so any NULL
//! replacement with an empty pattern wrongly returned NULL. Verified byte-for-byte
//! against the sqlite3 3.50.4 CLI.

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
fn replace_argument_null_ordering_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        "replace('ab','',NULL)",    // empty pattern short-circuits → 'ab'
        "replace('ab','','y')",     // empty pattern → 'ab' (unchanged)
        "replace('','',NULL)",      // empty subject, empty pattern → ''
        "replace(123,'',NULL)",     // integer subject → text '123'
        "replace(1.5,'',NULL)",     // real subject → text '1.5'
        "replace(x'6162','',NULL)", // blob subject → its text bytes 'ab'
        "replace('ab',NULL,'x')",   // NULL pattern → NULL
        "replace('ab',NULL,NULL)",  // NULL pattern → NULL
        "replace(NULL,'','x')",     // NULL subject → NULL
        "replace('ab','a',NULL)",   // non-empty pattern, NULL replacement → NULL
        "replace('ab','b',NULL)",   // non-empty pattern, NULL replacement → NULL
        "replace('aXbXc','X','_')", // ordinary replacement
        "replace('ab','','')",      // empty pattern, empty replacement → 'ab'
    ];
    let mut sql = String::new();
    for c in cases {
        sql.push_str(&format!("SELECT typeof({c}),quote({c});"));
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
