//! A bare (non-grouped) column in a plain `GROUP BY` projection now runs on the
//! VDBE. SQLite emits, for such a column, the value from the row that first
//! creates each group — a "representative". The VDBE captures it as an extra
//! slot on the group-key vector (set once, at group creation, so it keeps the
//! first-seen row's value) and reads it back in `GroupEmit`.
//!
//! This holds *only* when the query has no `min()`/`max()` aggregate: with one,
//! SQLite instead pulls bare columns from the min/max companion row. That shape
//! still falls back to the tree-walker, which `query_vdbe` reports as an error —
//! asserted separately.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE
//! compiled the projection. Results match the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// Plain (unindexed) columns: the grouping column `g` is scanned by rowid, so the
// first-seen row per group is the lowest-rowid row — matching sqlite's scan.
const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'first-1',10),(1,'second-1',20),\n\
      (2,'first-2',5),(2,'second-2',7),(2,'third-2',9),\n\
      (3,'only-3',100);\n";

// No `ORDER BY` (that routes to the HAVING/ORDER grouped path, which binds bare
// columns through registers and is out of scope here). Output order is first-seen
// group order; the sqlite comparison sorts both sides to stay order-insensitive.
const QUERIES: &[&str] = &[
    // Bare representative column plus an aggregate.
    "SELECT a, count(*) FROM t GROUP BY g",
    // Grouping key, representative, and aggregate together.
    "SELECT g, a, count(*) FROM t GROUP BY g",
    // Two representatives (a and n) alongside an aggregate over n.
    "SELECT a, n, sum(n) FROM t GROUP BY g",
    // Representative only — no aggregate in the projection.
    "SELECT a FROM t GROUP BY g",
    // Aggregate before the representative (output ordering).
    "SELECT count(*), a FROM t GROUP BY g",
];

// General path (HAVING / ORDER BY / LIMIT): the representative is captured the
// same way and loaded back via `GroupKey`. ORDER BY keys are deterministic (the
// grouping key, or a unique value) so the sqlite comparison has a stable order.
const GENERAL_QUERIES: &[&str] = &[
    // Representative + ORDER BY the grouping key.
    "SELECT a, count(*) FROM t GROUP BY g ORDER BY g",
    // Representative with HAVING and a deterministic ORDER BY.
    "SELECT a, count(*) AS c FROM t GROUP BY g HAVING count(*) >= 1 ORDER BY g",
    // Representative + ORDER BY + LIMIT/OFFSET.
    "SELECT a, count(*) FROM t GROUP BY g ORDER BY g LIMIT 2 OFFSET 1",
    // A bare column inside a scalar function in the projection.
    "SELECT upper(a), count(*) FROM t GROUP BY g ORDER BY g",
    // Two representatives (a and n) plus an aggregate, general path.
    "SELECT a, n, sum(n) FROM t GROUP BY g ORDER BY g",
    // ORDER BY references the representative column itself.
    "SELECT g, count(*) FROM t GROUP BY g ORDER BY a DESC",
];

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

#[test]
fn group_bare_column_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the projection compiled.
        // Both engines emit first-seen group order, so compare rows directly.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn group_bare_column_general_path_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in GENERAL_QUERIES {
        // Deterministic ORDER BY, so both engines emit the same row order.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn group_bare_column_over_join_runs_on_vdbe() {
    // The plain grouped-join compiler shares the representative path, so a
    // non-grouped bare column over a join also runs on the VDBE.
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE a(x INTEGER, av TEXT)",
        "CREATE TABLE b(p TEXT, bv INTEGER)",
        "INSERT INTO a VALUES (1,'a1'),(1,'a2'),(2,'a3')",
        "INSERT INTO b VALUES ('P',10),('Q',20)",
    ] {
        c.execute(s).unwrap();
    }
    for q in [
        "SELECT a.x, b.p FROM a, b GROUP BY a.x",
        "SELECT a.x, av, count(*) FROM a JOIN b GROUP BY a.x",
    ] {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn group_bare_column_min_max_companion_falls_back() {
    let c = conn();
    // With a min()/max() aggregate, the bare column must come from the companion
    // row, not the first-seen row — the VDBE declines to compile it (falls back).
    for q in [
        "SELECT a, max(n) FROM t GROUP BY g",
        "SELECT a, min(n) FROM t GROUP BY g",
        // The same rule applies on the general (ORDER BY) path.
        "SELECT a, max(n) FROM t GROUP BY g ORDER BY g",
        "SELECT a, min(n) AS m FROM t GROUP BY g HAVING m > 0 ORDER BY g",
    ] {
        assert!(
            c.query_vdbe(q).is_err(),
            "expected VDBE fallback for min/max companion query {q}"
        );
        // The tree-walker still handles it.
        assert!(c.query(q).is_ok(), "tree-walker should run {q}");
    }
}

#[test]
fn group_bare_column_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in QUERIES.iter().chain(GENERAL_QUERIES) {
        let mut vdbe: Vec<Vec<String>> = c
            .query_vdbe(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect();
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg("-ascii")
            .arg(format!("{SETUP}{q};"))
            .output()
            .unwrap();
        assert!(out.status.success(), "sqlite3 failed on {q}");
        let text = String::from_utf8(out.stdout).unwrap();
        let mut want: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        // GROUP BY row order is engine-defined (and an ORDER BY tie is unspecified);
        // sort both sides so the comparison is order-insensitive.
        vdbe.sort();
        want.sort();
        assert_eq!(vdbe, want, "VDBE vs sqlite3 diverged on {q}");
    }
}
