//! The tested expression of `x [NOT] IN (SELECT …)` is a column reference of the
//! enclosing scope, so a bad one (`nope IN (SELECT …)`) is a prepare-time
//! `no such column` in SQLite — raised even over an empty/filtered input, and
//! even when the `IN (SELECT)` is itself nested inside another subquery's body.
//! graphite's shallow-column walker skipped the `InSelect` operand entirely (only
//! `InList`'s operand was visited), so this was caught only lazily. Verified
//! against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

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
fn in_select_lhs_column_validation_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let empty = "CREATE TABLE t(a, b); CREATE TABLE u(c, d);";
    let full = "CREATE TABLE t(a, b); INSERT INTO t VALUES(1, 2); CREATE TABLE u(c, d);";
    let cases = [
        // Bad LHS of an IN (SELECT) — outer and nested inside another subquery.
        (empty, "SELECT x FROM t WHERE nope IN (SELECT c FROM u)"),
        (empty, "SELECT a FROM t WHERE nope NOT IN (SELECT c FROM u)"),
        (
            empty,
            "SELECT a FROM t WHERE a IN (SELECT c FROM u WHERE nope IN (SELECT d FROM u))",
        ),
        // Valid LHS references must still be accepted (no false positive):
        (full, "SELECT a FROM t WHERE a IN (SELECT c FROM u)"),
        (
            full,
            "SELECT a AS x FROM t GROUP BY x HAVING x IN (SELECT c FROM u)",
        ),
        (
            full,
            "SELECT count(*) FROM t GROUP BY a HAVING count(*) IN (SELECT c FROM u)",
        ),
        (
            full,
            "SELECT a FROM t WHERE b IN (SELECT c FROM u WHERE d IN (SELECT c FROM u))",
        ),
    ];
    for (base, q) in cases {
        assert_eq!(
            rejects("sqlite3", base, q),
            rejects(g, base, q),
            "reject-parity for `{q}`"
        );
    }
}
