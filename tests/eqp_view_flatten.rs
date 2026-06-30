//! `EXPLAIN QUERY PLAN` over a VIEW source. A view has no b-tree of its own:
//! SQLite flattens the view body into the outer plan exactly as it does a derived
//! table — `SELECT * FROM v` over `CREATE VIEW v AS SELECT a,b FROM t` reads as the
//! body's own `SCAN t` (a covering index when only its columns are needed), and an
//! outer `WHERE` tightens that into a `SEARCH`.
//!
//! graphite previously crashed EQP on *any* view source with a malformed
//! `no such table: <view>` (the view name fell through to a base-table lookup). It
//! now rewrites `FROM v` into `FROM (<view body>) AS v` and reuses the derived-table
//! flatten machinery: the flattenable shapes render byte-exactly, and the rest
//! (an aggregate / join / compound view body, or a view combined with a join —
//! which SQLite cost-reorders) decline cleanly instead of crashing. Verified vs the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// `EXPLAIN QUERY PLAN sql` → `#`-joined bare node labels.
fn plan(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|c: char| " |`*+_-".contains(c)))
        .collect::<Vec<_>>()
        .join("#")
}

/// graphite's stderr error line, prefix-stripped, for the decline cases.
fn err(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

const BASE: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX tb ON t(b); \
                    CREATE TABLE u(x,y); \
                    CREATE VIEW v AS SELECT a,b FROM t; \
                    CREATE VIEW v2 AS SELECT * FROM t; \
                    CREATE VIEW vw AS SELECT a,b FROM t WHERE c>0; \
                    CREATE VIEW vagg AS SELECT count(*) c FROM t; \
                    CREATE VIEW vj AS SELECT t.a,u.y FROM t JOIN u ON t.a=u.x;";

#[test]
fn flattenable_view_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM v",
        "SELECT * FROM v WHERE b=5",
        "SELECT * FROM v2",
        "SELECT * FROM v2 WHERE b=5",
        "SELECT a FROM v WHERE b=5",
        "SELECT * FROM v WHERE a=3",
        // The view name is the bind qualifier once flattened, alias or not.
        "SELECT * FROM v AS x WHERE x.b=5",
        "SELECT v.a FROM v WHERE v.b=5",
        // An inner WHERE in the view body carries through.
        "SELECT * FROM vw",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "plan for {q}");
    }
}

#[test]
fn non_flattenable_view_shapes_decline_without_crashing() {
    // The pre-fix bug surfaced as a malformed `no such table: <view>`. An aggregate
    // / join view body (SQLite renders a CO-ROUTINE or a cost-reordered join) and a
    // view combined with a join must now decline cleanly instead.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM vagg",                // aggregate view body → CO-ROUTINE
        "SELECT * FROM vj",                  // join view body
        "SELECT * FROM v JOIN u ON v.a=u.x", // view combined with a join
        "SELECT * FROM u JOIN v ON v.a=u.x", // view in the join position
        "SELECT * FROM u, v",                // comma-join with a view
    ] {
        let got = err(g, BASE, q);
        assert!(
            !got.contains("no such table"),
            "{q} regressed to the malformed crash: {got:?}"
        );
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}
