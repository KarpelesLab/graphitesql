//! A CTE whose recursive table appears already in the *first* arm — with no
//! leading non-recursive anchor to seed the recursion — is rejected by SQLite
//! as `circular reference: <name>`. This holds whether or not the `RECURSIVE`
//! keyword is present, and regardless of how the self-reference is shaped (a
//! plain self-`FROM`, a self-join, or a recursive arm placed before the
//! anchor in a compound). graphite previously surfaced its internal
//! "recursive CTE must have a non-recursive anchor and a recursive term"
//! message here; it now matches SQLite.
//!
//! Valid recursive CTEs (anchor first) must still run unchanged.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// First non-caret output line, with graphite's/SQLite's error prefixes peeled
/// off so the two CLIs are directly comparable.
fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    for line in s.lines() {
        let mut t = line.trim_end();
        if t.trim_start().starts_with('^') {
            continue;
        }
        // Peel the CLI error wrappers ("Error: in prepare, error: …",
        // "Error: stepping, …", "Error: SQL error: …") uniformly.
        for prefix in [
            "Error: ",
            "in prepare, ",
            "stepping, ",
            "error: ",
            "SQL error: ",
        ] {
            t = t.strip_prefix(prefix).unwrap_or(t);
        }
        return t.to_string();
    }
    String::new()
}

#[test]
fn circular_reference_ctes_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // Non-RECURSIVE self-reference.
        "WITH c AS (SELECT * FROM c) SELECT * FROM c",
        // RECURSIVE keyword, but the recursive table is the whole (first) arm.
        "WITH RECURSIVE c(n) AS (SELECT n FROM c) SELECT * FROM c",
        // Self-join in the first arm.
        "WITH c AS (SELECT 1 FROM c JOIN c) SELECT * FROM c",
        // Recursive arm precedes the anchor in the compound.
        "WITH RECURSIVE c(n) AS (SELECT n FROM c UNION ALL SELECT 1) SELECT * FROM c",
        // Every arm is recursive (no anchor anywhere).
        "WITH RECURSIVE c(n) AS (SELECT n FROM c UNION ALL SELECT n FROM c) SELECT * FROM c",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}

#[test]
fn valid_recursive_ctes_still_run() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // Plain anchor, no recursion.
        "WITH RECURSIVE c AS (SELECT 1) SELECT * FROM c",
        // Classic count-up: anchor first, then the recursive arm.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<3) SELECT * FROM c",
        // Multiple anchors before the recursive arm.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT n+10 FROM c WHERE n<2) SELECT * FROM c ORDER BY n",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
