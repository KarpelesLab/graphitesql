//! A derived table (`FROM (SELECT …) alias`) whose body is a *plain* join now runs
//! on the VDBE. The body is materialized through `run_select` (the same rows the
//! tree-walker produces), and each output column's `(affinity, collation)` is
//! resolved across the join's sources by `subquery_column_origins` — so an
//! affinity-sensitive outer `WHERE` / `ORDER BY` over the derived column matches
//! sqlite (a numeric base column keeps NUMERIC affinity, not the BLOB default).
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * a NATURAL / USING join body — its shared column is coalesced, which a
//!     bare-name origin lookup across both sources can't disambiguate.
//!   * a body carrying a *non-BINARY* collation column — the VDBE grouped / aggregate
//!     paths assume BINARY keys.
//!   * a compound (UNION/…) body — no single positional origin per column.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled
//! the derived join source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(3,'z',30);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n";

// Each query's FROM source is a derived table whose body is a plain join. ORDER BY
// (or a deterministic aggregate) pins the row order for a direct comparison.
const QUERIES: &[&str] = &[
    // Plain projection over an INNER-join body.
    "SELECT x.g, x.m FROM (SELECT t.g, u.m FROM t JOIN u ON t.g=u.g) x ORDER BY 1,2",
    // Affinity-sensitive outer WHERE: the derived `g` keeps INTEGER affinity, so a
    // text literal coerces numerically and matches (BLOB default would not).
    "SELECT x.g FROM (SELECT t.g, u.m FROM t JOIN u ON t.g=u.g) x WHERE x.g = '2' ORDER BY 1",
    // Affinity-sensitive ORDER BY: numeric affinity sorts numerically.
    "SELECT x.g FROM (SELECT t.g FROM t JOIN u ON t.g=u.g) x ORDER BY x.g",
    // Wildcard over the join body expands all sources' columns in declaration order.
    "SELECT * FROM (SELECT t.g, u.m FROM t JOIN u ON t.g=u.g) x ORDER BY 1,2",
    // A CROSS join body.
    "SELECT x.g, x.gg FROM (SELECT t.g, u.g AS gg FROM t CROSS JOIN u) x ORDER BY 1,2",
    // A comma (implicit cross) join body, counted.
    "SELECT count(*) FROM (SELECT t.g FROM t, u) x",
    // GROUP BY / aggregate over the join body.
    "SELECT x.g, sum(x.m) FROM (SELECT t.g, u.m FROM t JOIN u ON t.g=u.g) x \
     GROUP BY x.g ORDER BY 1",
    // A LEFT join body (null-padded rows flow through unchanged).
    "SELECT x.g, x.m FROM (SELECT t.g, u.m FROM t LEFT JOIN u ON t.g=u.g) x ORDER BY 1,2",
    // The outer query joins the derived join body to a base table.
    "SELECT x.g, t2.a FROM (SELECT t.g, u.m FROM t JOIN u ON t.g=u.g) x \
     JOIN t t2 ON t2.g = x.g ORDER BY 1, 2",
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
fn derived_join_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE scanned the body.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn derived_join_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in QUERIES {
        let vdbe: Vec<Vec<String>> = c
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
        let want: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(vdbe, want, "VDBE vs sqlite3 diverged on {q}");
    }
}

#[test]
fn natural_using_and_collation_bodies_defer() {
    let mut c = conn();
    c.execute("CREATE TABLE w(k TEXT COLLATE NOCASE, v INTEGER)")
        .unwrap();
    c.execute("INSERT INTO w VALUES ('A',1),('b',2)").unwrap();
    // A NATURAL / USING join body (coalesced shared column) and a non-BINARY
    // collation body each defer to the tree-walker, which still runs them. (A
    // same-affinity compound body now runs — see `vdbe_derived_compound.rs`.)
    for q in [
        "SELECT g FROM (SELECT g FROM t NATURAL JOIN u) x ORDER BY 1",
        "SELECT g FROM (SELECT g FROM t JOIN u USING(g)) x ORDER BY 1",
        "SELECT x.k FROM (SELECT w.k, t.g FROM w JOIN t ON w.v=t.g) x WHERE x.k='a'",
    ] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
        assert!(c.query(q).is_ok(), "tree-walker should run {q}");
    }
}
