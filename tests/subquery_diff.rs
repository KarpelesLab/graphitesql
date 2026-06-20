//! Differential testing of correlated subqueries, EXISTS/NOT EXISTS, scalar
//! subqueries, and IN/NOT IN (subquery) with NULL semantics, against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn render(result: &graphitesql::QueryResult) -> String {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => s.clone(),
                    Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn subqueries_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-sub-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE emp(id INTEGER PRIMARY KEY, dept INT, sal INT);\
        CREATE TABLE dept(id INTEGER PRIMARY KEY, budget INT);\
        INSERT INTO emp(dept,sal) VALUES (1,100),(1,200),(2,150),(2,50),(3,NULL);\
        INSERT INTO dept VALUES (1,250),(2,300),(3,100);";
    {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(setup)
            .output()
            .unwrap();
        assert!(o.status.success());
    }
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    let queries = [
        "SELECT id FROM emp e WHERE sal > (SELECT avg(sal) FROM emp WHERE dept=e.dept) ORDER BY id",
        "SELECT id FROM dept d WHERE EXISTS (SELECT 1 FROM emp WHERE dept=d.id AND sal>100) ORDER BY id",
        "SELECT id FROM dept d WHERE NOT EXISTS (SELECT 1 FROM emp WHERE dept=d.id) ORDER BY id",
        "SELECT id, (SELECT budget FROM dept WHERE id=e.dept) AS b FROM emp e ORDER BY id",
        "SELECT id FROM emp WHERE dept IN (SELECT id FROM dept WHERE budget>200) ORDER BY id",
        "SELECT id FROM emp WHERE sal = (SELECT max(sal) FROM emp) ORDER BY id",
        "SELECT id FROM emp WHERE sal NOT IN (SELECT sal FROM emp WHERE sal IS NOT NULL) ORDER BY id",
        "SELECT id FROM emp WHERE sal NOT IN (SELECT sal FROM emp) ORDER BY id",
        "SELECT d.id, (SELECT count(*) FROM emp WHERE dept=d.id) FROM dept d ORDER BY d.id",
        "SELECT id FROM emp WHERE dept = (SELECT min(id) FROM dept) ORDER BY id",
        "SELECT (SELECT count(*) FROM emp), (SELECT sum(budget) FROM dept)",
        "SELECT id FROM emp e WHERE (SELECT budget FROM dept WHERE id=e.dept) > sal*2 ORDER BY id",
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
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "subquery diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}
