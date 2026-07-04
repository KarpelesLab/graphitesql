//! A bad column reference in a window `OVER` clause's `PARTITION BY` / `ORDER BY`
//! (or a named `WINDOW …` definition) is a prepare-time `no such column` in
//! SQLite — raised even over an empty or fully-filtered table. graphite's eager
//! column validators bailed on any window query, and the VDBE window path also
//! bypasses them, so this was caught only lazily. A window partition/order term
//! binds strictly to a base column of the `FROM` (never an output alias), so it
//! resolves against the source schema. Verified against sqlite3 3.50.4.

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
fn window_over_column_validation_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let empty = "CREATE TABLE t(a, b);";
    let full = "CREATE TABLE t(a, b); INSERT INTO t VALUES(1, 2), (3, 4);\
                CREATE TABLE u(c, d); INSERT INTO u VALUES(1, 9);";
    let cases = [
        // Bad column in an OVER clause → rejected (empty table, so caught eagerly).
        (empty, "SELECT sum(a) OVER (PARTITION BY zzz) FROM t"),
        (empty, "SELECT sum(a) OVER (ORDER BY zzz) FROM t"),
        (
            empty,
            "SELECT sum(a) OVER (PARTITION BY a ORDER BY zzz) FROM t",
        ),
        (
            empty,
            "SELECT sum(a) OVER w FROM t WINDOW w AS (PARTITION BY zzz)",
        ),
        (empty, "SELECT 1 + sum(a) OVER (PARTITION BY zzz) FROM t"),
        // An output alias is NOT a valid window partition/order term.
        (full, "SELECT a AS x, sum(b) OVER (PARTITION BY x) FROM t"),
        // A bad column in a window function's argument, its FILTER, or the WHERE
        // of a window query is likewise rejected at prepare time.
        (empty, "SELECT sum(zzz) OVER () FROM t"),
        (empty, "SELECT sum(a) FILTER(WHERE zzz > 0) OVER () FROM t"),
        (
            empty,
            "SELECT count(*) OVER (PARTITION BY a ORDER BY b), sum(zzz) OVER () FROM t",
        ),
        (
            empty,
            "SELECT a FROM t WHERE zzz > 0 AND sum(a) OVER () > 0",
        ),
        // Valid — must not be a false positive.
        (
            full,
            "SELECT sum(a) OVER (PARTITION BY b ORDER BY a) FROM t",
        ),
        (full, "SELECT sum(a) OVER (PARTITION BY a + 0) FROM t"),
        (full, "SELECT sum(b) OVER (PARTITION BY rowid) FROM t"),
        (full, "SELECT sum(b) OVER w FROM t WINDOW w AS (ORDER BY a)"),
        (
            full,
            "SELECT t.a, sum(d) OVER (PARTITION BY t.b) FROM t JOIN u ON t.a = u.c",
        ),
        (
            full,
            "SELECT row_number() OVER (ORDER BY a), a FROM t ORDER BY a",
        ),
        (full, "SELECT sum(a) FILTER(WHERE b > 0) OVER () FROM t"),
        (
            full,
            "SELECT a AS x, sum(b) OVER (ORDER BY a) FROM t ORDER BY x",
        ),
        (full, "SELECT sum(a + b) OVER () FROM t"),
        (
            full,
            "SELECT a, row_number() OVER (ORDER BY b) AS rn FROM t ORDER BY rn",
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
