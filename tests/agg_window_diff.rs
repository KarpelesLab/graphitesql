//! Differential testing of DISTINCT aggregates and window functions over a small
//! dataset with duplicates and NULLs — checked against sqlite3.

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
fn distinct_agg_and_window_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-aw-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, g INT, v INT);\
        INSERT INTO t(g,v) VALUES (1,10),(1,10),(1,20),(2,5),(2,NULL),(2,5),(3,NULL);";
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
        "SELECT count(DISTINCT v) FROM t",
        "SELECT g, count(DISTINCT v), sum(DISTINCT v), avg(DISTINCT v) FROM t GROUP BY g ORDER BY g",
        "SELECT group_concat(DISTINCT v) FROM t WHERE v IS NOT NULL",
        "SELECT g, group_concat(v, '/') FROM t GROUP BY g ORDER BY g",
        "SELECT total(v), sum(v) FROM t WHERE g=3",
        "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY id) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, lag(v) OVER (ORDER BY id), lead(v) OVER (ORDER BY id) FROM t ORDER BY id",
        "SELECT id, ntile(3) OVER (ORDER BY id) FROM t ORDER BY id",
        "SELECT id, first_value(v) OVER (PARTITION BY g ORDER BY id) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // `*` / `table.*` mixed with aggregates: the bare columns take the
        // representative row's value (the min/max row when a lone min/max is
        // present, else the group's first row).
        "SELECT *, count(*) FROM t",
        "SELECT count(*), * FROM t",
        "SELECT *, max(v) FROM t",
        "SELECT t.*, count(*) FROM t",
        "SELECT g, count(*), * FROM t GROUP BY g ORDER BY g",
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
        assert_eq!(got, want, "diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}
