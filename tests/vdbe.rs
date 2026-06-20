//! Track B: the VDBE bytecode IR spike. For constant `SELECT` projections, the
//! compiled-and-interpreted program must produce the same rows as the
//! tree-walking executor and as `sqlite3`.

#![cfg(feature = "std")]

use graphitesql::exec::vdbe;
use graphitesql::sql::{ast::Statement, parse_one};
use graphitesql::{Connection, Value};
use std::process::Command;

fn vdbe_run(sql: &str) -> Vec<Vec<Value>> {
    let Statement::Select(sel) = parse_one(sql).unwrap() else {
        panic!("not a select")
    };
    let prog = vdbe::compile_const_select(&sel).unwrap();
    vdbe::run(&prog).unwrap()
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
fn vdbe_matches_tree_walker() {
    let c = Connection::open_memory().unwrap();
    let queries = [
        "SELECT 1 + 2 * 3",
        "SELECT 10 - 4, 8 / 2, 17 % 5",
        "SELECT 'a' || 'b' || 'c'",
        "SELECT -5, 3.5, 2 + 3.5",
        "SELECT (1 + 2) * (3 + 4)",
        "SELECT 100 / 7, 100.0 / 7",
        "SELECT 1 < 2, 2 <= 2, 3 > 4, 5 = 5, 5 <> 6",
        "SELECT 1 AND 1, 1 AND 0, 0 OR 1, 0 OR 0",
        "SELECT NOT 0, NOT 1, NULL IS NULL, 1 IS NOT NULL",
        "SELECT (1 < 2) AND (3 < 4), (1 > 2) OR (5 = 5)",
        "SELECT CASE WHEN 1 > 2 THEN 'a' WHEN 3 > 2 THEN 'b' ELSE 'c' END",
        "SELECT CASE 5 WHEN 1 THEN 'one' WHEN 5 THEN 'five' ELSE '?' END",
        "SELECT CASE WHEN 0 THEN 1 END, CASE WHEN 1 THEN 2 ELSE 3 END",
    ];
    for q in queries {
        let walker = c.query(q).unwrap().rows;
        let vdbe = vdbe_run(q);
        assert_eq!(vdbe, walker, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn vdbe_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let queries = [
        "SELECT 1 + 2 * 3",
        "SELECT 'x' || 'y'",
        "SELECT 10 - 4, 8 / 2",
        "SELECT (2 + 3) * 4",
    ];
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(q)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = vdbe_run(q)
            .iter()
            .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "VDBE vs sqlite3 diverged on {q}");
    }
}

#[test]
fn falls_back_for_non_constant() {
    // Anything beyond a constant SELECT list is Unsupported, so the engine can
    // fall back to the tree-walker.
    for q in ["SELECT * FROM t", "SELECT 1 WHERE 1=1", "SELECT count(*)"] {
        let Statement::Select(sel) = parse_one(q).unwrap() else {
            panic!()
        };
        assert!(
            vdbe::compile_const_select(&sel).is_err(),
            "expected {q} to be unsupported by the spike"
        );
    }
}
