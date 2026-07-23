//! A `WINDOW name AS (base …)` definition that names another window *refines* it:
//! it inherits the base's `PARTITION BY` / `ORDER BY` (SQLite's
//! `sqlite3WindowChain`) and may only add clauses the base leaves open. graphite
//! previously dropped the base's clauses — `OVER w2` where `w2 AS (w1 ORDER BY
//! id)` and `w1 AS (PARTITION BY g)` computed over the whole table instead of per
//! group, and a pure copy `w2 AS (w1)` lost the base's `ORDER BY`. It also
//! silently accepted a refinement that overrides the base's PARTITION/ORDER
//! BY/frame, which SQLite rejects with `cannot override <clause> of window: w1`.
//! Only *earlier* definitions in the same clause are visible; a forward/unknown
//! base is ignored, exactly as SQLite does. Verified against the sqlite3 3.50.4
//! CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Trimmed stdout, or the first non-caret error line with its phase prefix
/// stripped (graphite reports these at step time, sqlite at prepare time).
fn run(bin: &str, sql: &str) -> String {
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
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .trim_end()
        .to_string()
}

const DDL: &str = "CREATE TABLE t(id INT, g TEXT, v INT); \
     INSERT INTO t VALUES(1,'a',10),(2,'a',20),(3,'a',30),\
     (4,'b',30),(5,'b',5),(6,'b',7),(7,'b',15);";

#[test]
fn named_window_refinement_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for tail in [
        // #1: inherit the base's PARTITION BY, add an ORDER BY.
        "SELECT id, g, sum(v) OVER w2 FROM t \
         WINDOW w1 AS (PARTITION BY g), w2 AS (w1 ORDER BY id);",
        // #1: pure copy keeps the base's ORDER BY (running sum, not a constant).
        "SELECT id, sum(v) OVER w2 FROM t WINDOW w1 AS (ORDER BY id), w2 AS (w1);",
        // #1: multi-level chain w1 -> w2 -> w3.
        "SELECT id, g, sum(v) OVER w3 FROM t \
         WINDOW w1 AS (PARTITION BY g), w2 AS (w1), w3 AS (w2 ORDER BY id);",
        // #1: refinement defined before the base is still resolved (backward ref).
        "SELECT id, g, sum(v) OVER w1 FROM t \
         WINDOW w2 AS (PARTITION BY g), w1 AS (w2 ORDER BY id);",
        // #1: a forward/unknown base is ignored (own clauses kept).
        "SELECT id, sum(v) OVER w1 FROM t \
         WINDOW w1 AS (w2 ORDER BY id), w2 AS (PARTITION BY g);",
        // #1: the refinement may add its own frame when the base has none.
        "SELECT id, g, sum(v) OVER w2 FROM t \
         WINDOW w1 AS (PARTITION BY g), w2 AS (w1 ORDER BY id ROWS 1 PRECEDING);",
        // #2: override errors (PARTITION checked first, then ORDER BY, then frame).
        "SELECT id, sum(v) OVER w2 FROM t \
         WINDOW w1 AS (ORDER BY id), w2 AS (w1 ORDER BY v);",
        "SELECT id, sum(v) OVER w2 FROM t \
         WINDOW w1 AS (PARTITION BY g), w2 AS (w1 PARTITION BY id ORDER BY v);",
        "SELECT id, sum(v) OVER w2 FROM t \
         WINDOW w1 AS (ORDER BY id ROWS 1 PRECEDING), w2 AS (w1);",
        // #2: an unused overriding definition is rejected just the same.
        "SELECT id FROM t WINDOW w1 AS (ORDER BY id), w2 AS (w1 ORDER BY v);",
        // #2: PARTITION override wins even when an ORDER BY override is also present.
        "SELECT id FROM t WINDOW w1 AS (ORDER BY id), w2 AS (w1 PARTITION BY g ORDER BY v);",
    ] {
        let sql = format!("{DDL} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {tail}");
    }
}
