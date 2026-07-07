//! A window's `ORDER BY … NULLS FIRST/LAST` must place NULLs where the modifier
//! says, not at the direction's default end. graphite's window-partition sort
//! dropped the per-term `NULLS FIRST`/`LAST` (it always used SQLite's default —
//! NULLs first under ASC, last under DESC), so `rank()`/`percent_rank()`/frame
//! results over `ORDER BY x NULLS LAST` (or `DESC NULLS FIRST`) put NULLs on the
//! wrong side and diverged. Verified byte-for-byte against the sqlite3 3.50.4 CLI.

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
fn window_order_by_nulls_placement_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let setup = "CREATE TABLE t(a,b);\
        INSERT INTO t VALUES(1,2),(1,NULL),(2,1),(2,NULL),(3,3),(3,2);";
    let wins = [
        "ORDER BY b NULLS LAST",
        "ORDER BY b NULLS FIRST",
        "ORDER BY b ASC NULLS LAST",
        "ORDER BY b DESC NULLS FIRST",
        "ORDER BY b DESC NULLS LAST",
        "ORDER BY b", // default (NULLS FIRST for ASC)
        "ORDER BY b DESC",
        "PARTITION BY a ORDER BY b NULLS LAST",
        "PARTITION BY a ORDER BY b DESC NULLS FIRST",
    ];
    let fns = [
        "rank()",
        "dense_rank()",
        "percent_rank()",
        "cume_dist()",
        "row_number()",
        "count(b) OVER w2",
        "first_value(b) OVER w2",
    ];
    let mut sql = String::from(setup);
    for w in wins {
        // ranking functions (no frame) — `w2` re-declares the same order with an
        // explicit UNBOUNDED..CURRENT frame for the value functions.
        for f in fns {
            let win = if f.contains("w2") {
                format!("WINDOW w2 AS ({w} ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)")
            } else {
                String::new()
            };
            let call = if f.contains("w2") {
                f.to_string()
            } else {
                format!("{f} OVER ({w})")
            };
            sql.push_str(&format!(
                "SELECT a,b,{call} FROM t {win} ORDER BY a,b NULLS FIRST;"
            ));
        }
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
