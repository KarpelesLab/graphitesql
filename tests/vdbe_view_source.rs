//! A `FROM` reference naming a **view** now runs on the VDBE. `scan_one` materializes
//! the view exactly as the tree-walker does (running its stored body), then exposes the
//! view's output columns — each column's `(affinity, collation)` comes from `try_view`'s
//! origin resolution, so an outer `WHERE` / `ORDER BY` over a view column coerces just as
//! it would over the body. Projection / WHERE / aggregate / GROUP BY / HAVING / DISTINCT /
//! ORDER BY / LIMIT over the view, a view joined to a base table, and a join- or
//! compound-bodied view all run without falling back.
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * a view column carrying a *non-BINARY* collation — the VDBE compare / group paths
//!     assume BINARY keys, so a `NOCASE` view column defers.
//!
//! A view has no rowid, so a `rowid` reference over the view resolves to nothing and the
//! query defers (like a derived table) — but a view body that *projects* `rowid` under an
//! alias exposes an ordinary column that runs.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled the
//! view source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(3,'z',30);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n\
    CREATE VIEW vt AS SELECT g, n FROM t;\n\
    CREATE VIEW vj AS SELECT t.g AS g, u.m AS m FROM t JOIN u ON t.g=u.g;\n\
    CREATE VIEW vc AS SELECT g FROM t UNION SELECT m FROM u;\n\
    CREATE VIEW vrowid AS SELECT rowid AS r, g FROM t;\n";

// Each query names a view in its FROM clause. ORDER BY (or a deterministic aggregate)
// pins the row order for a direct comparison.
const QUERIES: &[&str] = &[
    // Affinity-sensitive outer WHERE: the view column `g` keeps INTEGER affinity, so a
    // text literal coerces numerically and matches.
    "SELECT g, n FROM vt WHERE g = '2' ORDER BY g",
    // Wildcard over a view expands its columns in declaration order.
    "SELECT * FROM vt ORDER BY 1, 2",
    // Affinity-sensitive ORDER BY + LIMIT.
    "SELECT n FROM vt ORDER BY n DESC LIMIT 2",
    // Whole-view aggregate.
    "SELECT count(*), sum(n) FROM vt",
    // GROUP BY / HAVING over the view.
    "SELECT g FROM vt GROUP BY g HAVING count(*) >= 1 ORDER BY g",
    // DISTINCT over the view.
    "SELECT DISTINCT g FROM vt ORDER BY 1",
    // A view joined to a base table (qualified refs disambiguate the shared name `g`).
    "SELECT vt.g, u.m FROM vt JOIN u ON vt.g = u.g ORDER BY 1, 2",
    // A join-bodied view as the source.
    "SELECT g, m FROM vj ORDER BY 1, 2",
    // A compound-bodied (UNION) view as the source.
    "SELECT g FROM vc ORDER BY 1",
    // A view body that projects an aliased rowid is an ordinary column.
    "SELECT r, g FROM vrowid ORDER BY r",
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
fn view_source_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE scanned the view.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn view_source_matches_sqlite3() {
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
fn nocase_view_column_defers() {
    let mut c = conn();
    c.execute("CREATE TABLE w(k TEXT COLLATE NOCASE, val INTEGER)")
        .unwrap();
    c.execute("INSERT INTO w VALUES ('A',1),('b',2)").unwrap();
    c.execute("CREATE VIEW vw AS SELECT k, val FROM w").unwrap();
    // The view column `k` carries NOCASE, which the VDBE compare path can't honour —
    // the view source defers to the tree-walker (which matches SQLite: `k='a'` finds 'A').
    let q = "SELECT k FROM vw WHERE k = 'a'";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert_eq!(
        c.query(q).unwrap().rows,
        vec![vec![Value::Text("A".into())]]
    );
}
