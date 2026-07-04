//! A bad column reference inside a multi-row `VALUES` list used as the right
//! side of an `IN` operator (`… a IN (VALUES(1),(zzz))`) is a prepare-time
//! `no such column` in SQLite — raised even when the outer table is empty (or
//! every row is filtered), so a lazy per-row resolver never reaches it. A
//! multi-row `VALUES` desugars to a `UNION ALL` compound, and each arm is
//! `FROM`-less, so its column references can only bind to the enclosing scope —
//! safe to resolve eagerly. Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Whether the statement is rejected (prepare-time error) vs accepted.
fn rejects(bin: &str, base: &str, sql: &str) -> bool {
    let full = format!("{base} {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s.to_lowercase().contains("error")
}

#[test]
fn values_in_operator_column_validation_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // `t` is empty, so a lazy resolver would never reach a bad VALUES column.
    let empty = "CREATE TABLE t(a, b);";
    let full = "CREATE TABLE t(a, b); INSERT INTO t VALUES(1, 2);";
    let cases = [
        // Bad column in some VALUES row → rejected (any row position).
        (empty, "SELECT * FROM t WHERE a IN (VALUES(1),(zzz))"),
        (empty, "SELECT * FROM t WHERE a IN (VALUES(zzz),(1))"),
        (empty, "SELECT * FROM t WHERE a IN (VALUES(1),(2),(zzz))"),
        (
            empty,
            "SELECT * FROM t WHERE (a,b) IN (VALUES(1,2),(3,zzz))",
        ),
        // Valid: literal rows, or a row referencing a real outer column.
        (empty, "SELECT * FROM t WHERE a IN (VALUES(1),(2),(3))"),
        (empty, "SELECT * FROM t WHERE a IN (VALUES(1),(b))"),
        (
            full,
            "SELECT a FROM t WHERE a IN (VALUES(1),(b)) ORDER BY a",
        ),
        (full, "SELECT * FROM t WHERE a IN (VALUES(1),(zzz))"),
        // A UNION-of-selects arm (with its own FROM) is still left to the lazy
        // path — must not be a false positive on the empty table.
        (
            empty,
            "SELECT * FROM t WHERE a IN (VALUES(1)) OR a IN (VALUES(2),(3))",
        ),
    ];
    for (base, q) in cases {
        assert_eq!(
            rejects("sqlite3", base, q),
            rejects(g, base, q),
            "reject-parity for `{q}` (base: `{base}`)"
        );
    }
}
