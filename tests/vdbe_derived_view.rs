//! A derived table (`FROM (SELECT … FROM <view>) alias`) whose body reads a **view**
//! now resolves the view's per-column `(affinity, collation)` and runs on the VDBE.
//! `named_source_origins` parses the view's stored `CREATE VIEW` and threads its body
//! through `subquery_column_origins` — the same origins `try_view` assigns when it
//! materializes the view — so an affinity-sensitive outer `WHERE` / `ORDER BY` over a
//! derived view column coerces exactly as SQLite does.
//!
//! This also fixes a real **tree-walker** divergence: before, a derived table over a
//! view fell back to the conservative BLOB default, so `(SELECT g AS v FROM vt)
//! WHERE v = '2'` returned no rows where SQLite (and now graphite) returns `[2]` — the
//! view column `g`'s INTEGER affinity was lost across the wrapper.
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * a view whose column carries a *non-BINARY* collation — the VDBE compare/group
//!     paths assume BINARY keys, so a `NOCASE` view column body defers.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled the
//! view-bodied derived source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(3,'z',30);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n\
    CREATE VIEW vt AS SELECT g, n FROM t;\n\
    CREATE VIEW vr(gg, nn) AS SELECT g, n FROM t;\n\
    CREATE VIEW vj AS SELECT t.g, u.m FROM t JOIN u ON t.g=u.g;\n";

// Each query's FROM source is a derived table whose body reads a view. ORDER BY (or a
// deterministic aggregate) pins the row order for a direct comparison.
const QUERIES: &[&str] = &[
    // Affinity-sensitive outer WHERE: the derived `v` inherits the view column `g`'s
    // INTEGER affinity, so `'2'` coerces and matches (BLOB default would not — the
    // divergence this fixes).
    "SELECT v FROM (SELECT g AS v FROM vt) x WHERE v = '2' ORDER BY 1",
    // Affinity-sensitive ORDER BY over a different view column.
    "SELECT v FROM (SELECT n AS v FROM vt) x ORDER BY v",
    // Wildcard over the view body expands the view's columns in order.
    "SELECT * FROM (SELECT g, n FROM vt) x ORDER BY 1, 2",
    // An explicit `CREATE VIEW vr(gg,nn)` rename — origins are positional from the body.
    "SELECT gg FROM (SELECT gg, nn FROM vr) x WHERE gg = '3' ORDER BY 1",
    // A join-bodied view: each output column still resolves to one base source.
    "SELECT v FROM (SELECT g AS v FROM vj) x WHERE v = '2' ORDER BY 1",
    // GROUP BY / aggregate over the view-bodied derived source.
    "SELECT v, count(*) FROM (SELECT g AS v FROM vt) x GROUP BY v ORDER BY 1",
    // A same-affinity compound one of whose arms reads a view.
    "SELECT v FROM (SELECT g AS v FROM vt UNION SELECT m FROM u) x WHERE v = '2' ORDER BY 1",
    // The outer query joins the view-bodied derived source to a base table.
    "SELECT x.v, t2.a FROM (SELECT g AS v FROM vt) x JOIN t t2 ON t2.g = x.v ORDER BY 1, 2",
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
        Value::Text(s) => String::from(s.as_str()),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

#[test]
fn derived_view_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE scanned the body.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn derived_view_matches_sqlite3() {
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

// Regression guard: a derived table over a view now inherits the view column's
// INTEGER affinity, so the outer text predicate coerces and matches. Before the fix
// the tree-walker used the BLOB default and returned no rows.
#[test]
fn derived_over_view_coerces_outer_predicate() {
    let c = conn();
    let q = "SELECT v FROM (SELECT g AS v FROM vt) x WHERE v = '2'";
    let rows = c.query(q).unwrap().rows;
    assert_eq!(
        rows,
        vec![vec![Value::Integer(2)]],
        "the view column's INTEGER affinity must coerce the text '2' to match"
    );
    assert_eq!(c.query_vdbe(q).unwrap().rows, rows);
}

#[test]
fn nocase_view_column_body_defers() {
    let mut c = conn();
    c.execute("CREATE TABLE w(k TEXT COLLATE NOCASE, v INTEGER)")
        .unwrap();
    c.execute("INSERT INTO w VALUES ('A',1),('b',2)").unwrap();
    c.execute("CREATE VIEW vw AS SELECT k, v FROM w").unwrap();
    // The view column `k` carries NOCASE, which the VDBE compare path can't honour —
    // the derived body defers to the tree-walker, which still runs it (and matches
    // SQLite: `k = 'a'` finds 'A' under NOCASE).
    let q = "SELECT k FROM (SELECT k FROM vw) x WHERE k = 'a'";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert_eq!(
        c.query(q).unwrap().rows,
        vec![vec![Value::Text("A".into())]]
    );
}
