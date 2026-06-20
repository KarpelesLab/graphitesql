//! RANGE window frames with numeric value offsets (`RANGE n PRECEDING/FOLLOWING`),
//! which bound the frame by the ORDER BY value, checked against sqlite3.

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
fn range_value_frames_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-wr-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, g INT, v INT);\
        INSERT INTO t(g,v) VALUES (1,10),(1,20),(1,20),(1,30),(2,5),(2,15),(2,100);";
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
    let q = |frame: &str| format!("SELECT id, sum(v) OVER (ORDER BY v {frame}) FROM t ORDER BY id");
    let mut queries: Vec<String> = vec![
        q("RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING"),
        q("RANGE BETWEEN 10 PRECEDING AND CURRENT ROW"),
        q("RANGE BETWEEN CURRENT ROW AND 10 FOLLOWING"),
        q("RANGE BETWEEN 10 PRECEDING AND 5 PRECEDING"),
        q("RANGE BETWEEN UNBOUNDED PRECEDING AND 5 FOLLOWING"),
        q("RANGE 15 PRECEDING"),
    ];
    queries.push("SELECT id, sum(v) OVER (ORDER BY v DESC RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING) FROM t ORDER BY id".into());
    queries.push("SELECT id, count(*) OVER (PARTITION BY g ORDER BY v RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING) FROM t ORDER BY id".into());
    queries.push("SELECT id, avg(v) OVER (ORDER BY v RANGE BETWEEN 20 PRECEDING AND 20 FOLLOWING) FROM t ORDER BY id".into());
    // EXCLUDE clauses (CURRENT ROW / GROUP / TIES) over aggregates and first_value.
    queries.push("SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW) FROM t ORDER BY id".into());
    queries.push("SELECT id, sum(v) OVER (ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) FROM t ORDER BY id".into());
    queries.push("SELECT id, sum(v) OVER (ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) FROM t ORDER BY id".into());
    queries.push("SELECT id, count(*) OVER (ORDER BY id GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP) FROM t ORDER BY id".into());
    queries.push("SELECT id, first_value(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE CURRENT ROW) FROM t ORDER BY id".into());
    for qq in &queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(&path)
                .arg(format!("{qq};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = render(&g.query(qq).unwrap());
        assert_eq!(got, want, "RANGE frame diverged: {qq}");
    }
    let _ = std::fs::remove_file(&path);
}
