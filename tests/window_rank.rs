//! Track A: `percent_rank()` and `cume_dist()` window functions. Verified against
//! the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
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

fn rows_str(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(g TEXT, v INT, id INTEGER PRIMARY KEY);\
                 INSERT INTO t(g,v) VALUES \
                 ('a',1),('a',2),('a',2),('a',3),('b',5),('b',5),('b',9)";
    let queries = [
        "SELECT id, percent_rank() OVER (ORDER BY v) FROM t ORDER BY id",
        "SELECT id, cume_dist() OVER (ORDER BY v) FROM t ORDER BY id",
        "SELECT id, percent_rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
        "SELECT id, cume_dist() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    ];

    let path = std::env::temp_dir().join(format!("gsql-wr-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(setup)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = rows_str(&g, q);
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} window-rank queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
