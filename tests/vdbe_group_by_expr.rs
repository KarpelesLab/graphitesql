//! `GROUP BY <expression>` (a computed grouping key, not a bare column) now runs
//! on the VDBE, where before any non-column key bailed with "GROUP BY column refs
//! only". The grouped fold evaluates each key expression per row to identify the
//! group; the projection, `HAVING`, and `ORDER BY` resolve a structurally-equal
//! key expression through the binding table, exactly like a bare-column key.
//!
//! A computed key forces the binding-driven general grouped path (it can't take
//! the compact column-index `GroupEmit` shortcut), so these queries exercise that
//! path with and without `HAVING`/`ORDER BY`. Mixed key sets (a bare column plus a
//! computed key), non-grouped representative columns, and function-valued keys are
//! all covered.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE ran the
//! computed-key grouping itself. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query groups by a computed expression (or a mix of a column and an
// expression). ORDER BY pins the row order so the comparison is deterministic.
const QUERIES: &[&str] = &[
    // Arithmetic key.
    "SELECT n*2, count(*) FROM t GROUP BY n*2 ORDER BY n*2",
    // Key that collapses distinct columns into the same group.
    "SELECT g/2, count(*) FROM t GROUP BY g/2 ORDER BY g/2",
    // Modulo key with a non-grouped representative column.
    "SELECT n%2, a, count(*) FROM t GROUP BY n%2 ORDER BY n%2",
    // Boolean-valued key.
    "SELECT n>10, count(*) FROM t GROUP BY n>10 ORDER BY n>10",
    // Function-valued key.
    "SELECT substr(a,1,1), count(*) FROM t GROUP BY substr(a,1,1) ORDER BY 1",
    // Mixed: a bare column plus a computed key.
    "SELECT g, n>10, count(*) FROM t GROUP BY g, n>10 ORDER BY g, n>10",
    // Computed key with HAVING over an aggregate.
    "SELECT n%2 AS p, count(*) AS c FROM t GROUP BY n%2 HAVING count(*)>1 ORDER BY p",
    // Computed key with an aggregate over a different column.
    "SELECT g+0, sum(n), max(n) FROM t GROUP BY g+0 ORDER BY g+0",
    // Plain computed key (no HAVING / ORDER BY): emission order is the key order.
    "SELECT n*2, count(*) FROM t GROUP BY n*2",
    // Computed key with LIMIT.
    "SELECT g*10, count(*) FROM t GROUP BY g*10 ORDER BY g*10 LIMIT 2",
    // Function-folded text key: `upper(a)` loses the column collation, so BINARY
    // grouping of the uppercased result is correct (an explicit `COLLATE` would
    // not be — see `non_binary_collation_key_falls_back`).
    "SELECT upper(a), count(*) FROM t GROUP BY upper(a) ORDER BY 1",
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
fn computed_group_key_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE grouped on the
        // computed key itself.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn group_by_output_alias_still_falls_back() {
    let c = conn();
    // `GROUP BY d` names an output alias, not a source column or the key
    // expression; the VDBE does not rewrite aliases, so it defers to the
    // tree-walker (which resolves the alias).
    let q = "SELECT n*2 AS d, count(*) FROM t GROUP BY d ORDER BY d";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert!(c.query(q).is_ok(), "tree-walker should run {q}");
}

#[test]
fn constant_group_key_falls_back() {
    let c = conn();
    // A column-free key is a positional reference (a signed-integer literal) or a
    // constant grouping the whole table into one row; both are left to the
    // tree-walker, which draws SQLite's positional-vs-constant distinction exactly.
    for q in [
        "SELECT n FROM t GROUP BY -1",  // out-of-range positional -> error
        "SELECT n FROM t GROUP BY 1+0", // constant -> one group
        "SELECT n FROM t GROUP BY 'k'", // constant -> one group
    ] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    }
}

#[test]
fn non_binary_collation_key_falls_back() {
    let c = conn();
    // An explicit `COLLATE NOCASE` on the key changes group matching away from the
    // VDBE's BINARY `GroupStep`, so it must defer to the tree-walker.
    let q = "SELECT a COLLATE NOCASE, count(*) FROM t GROUP BY a COLLATE NOCASE ORDER BY 1";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert!(c.query(q).is_ok(), "tree-walker should run {q}");
}

#[test]
fn computed_group_key_matches_sqlite3() {
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
