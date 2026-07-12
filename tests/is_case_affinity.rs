//! `IS`/`IS NOT` and `CASE x WHEN y` apply the same pre-comparison affinity as
//! `=` (regression: IS compared raw storage classes; CASE-WHEN missed affinity on
//! the tree-walker path). Verified against the sqlite3 CLI, covering both the
//! VDBE (single-table query) and tree-walker (no-FROM / subquery) paths.
#![cfg(feature = "std")]
use graphitesql::{Connection, Value};
use std::process::Command;

fn render(r: &graphitesql::QueryResult) -> String {
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(x) => graphitesql::exec::eval::format_real(*x),
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn is_and_case_when_affinity_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-iscase-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE t(i INTEGER, s TEXT, n);\
        INSERT INTO t VALUES(1,'1',1),(5,'5',5);\
        CREATE TABLE u(v TEXT); INSERT INTO u VALUES('5');";
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(setup)
        .output()
        .unwrap();
    assert!(o.status.success());
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    let queries = [
        // IS / IS NOT affinity, VDBE path (single-table query)
        "SELECT i IS s, i IS NOT s FROM t ORDER BY i",
        "SELECT i IS '1', i IS '5' FROM t ORDER BY i",
        "SELECT n IS s FROM t ORDER BY i", // untyped vs text → no coerce
        // IS with NULL semantics intact
        "SELECT 1 IS NULL, NULL IS NULL, '1' IS 1, 1 IS 1.0",
        // IS via tree-walker (scalar subquery)
        "SELECT 5 IS (SELECT v FROM u), 5 IS NOT (SELECT v FROM u)",
        // CASE x WHEN y affinity (VDBE + subquery/tree-walker)
        "SELECT CASE i WHEN s THEN 'y' ELSE 'n' END FROM t ORDER BY i",
        "SELECT CASE 5 WHEN (SELECT v FROM u) THEN 'y' ELSE 'n' END",
        "SELECT CASE i WHEN '5' THEN 'hit' ELSE 'miss' END FROM t ORDER BY i",
    ];
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(&path)
                .arg(format!("{q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        assert_eq!(render(&g.query(q).unwrap()), want, "diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}
