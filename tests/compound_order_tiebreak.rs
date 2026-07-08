//! A deduplicating compound (`UNION` / `INTERSECT` / `EXCEPT`) with an `ORDER BY`
//! that does not cover every column breaks ties on the remaining columns in
//! ascending order (NULLs first) — regardless of the ORDER BY's own direction —
//! because sqlite materializes the result through a sorter keyed by the ORDER BY
//! terms followed by all other columns (its duplicate-elimination key). graphite
//! sorted by only the ORDER BY terms, leaving tied rows in an arbitrary order
//! (which also changed `LIMIT` results). `UNION ALL` keeps input order. Verified
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
fn compound_order_by_ties_break_on_remaining_columns() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let setup = "CREATE TABLE t(a,b);INSERT INTO t VALUES(5,3),(5,1),(5,2),(1,9);";
    let cases = [
        // Tie on a=5: remaining column b ascending (1,2,3).
        "SELECT a,b FROM t UNION SELECT a,b FROM t ORDER BY a;",
        // Same, even when the ORDER BY key is DESC — ties still ascend by b.
        "SELECT a,b FROM t UNION SELECT a,b FROM t ORDER BY a DESC;",
        // INTERSECT and EXCEPT dedup the same way.
        "SELECT a,b FROM t INTERSECT SELECT a,b FROM t ORDER BY a;",
        "SELECT a,b FROM t EXCEPT SELECT a,b FROM t WHERE b=1 ORDER BY a DESC;",
        // LIMIT depends on the tie order being correct.
        "SELECT a,b FROM t UNION SELECT a,b FROM t ORDER BY a LIMIT 2;",
        // UNION ALL does not dedup: ties keep input order.
        "SELECT a,b FROM t UNION ALL SELECT a,b FROM t WHERE 0 ORDER BY a;",
        // A NULL in the ORDER BY column, and a three-arm uniform chain.
        "SELECT a,b FROM t UNION SELECT NULL,7 UNION SELECT NULL,4 ORDER BY a;",
    ];
    for sql in cases {
        let full = format!("{setup}{sql}");
        assert_eq!(out("sqlite3", &full), out(g, &full), "for `{sql}`");
    }
}
