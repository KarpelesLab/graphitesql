//! Track A: named windows — `WINDOW w AS (…)` and `OVER w`. Verified against
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
                 ('a',1),('a',3),('a',2),('b',5),('b',4)";
    let queries = [
        // Bare OVER name.
        "SELECT id, sum(v) OVER w FROM t WINDOW w AS (PARTITION BY g) ORDER BY id",
        // Two functions sharing a named window.
        "SELECT id, count(*) OVER w, avg(v) OVER w FROM t WINDOW w AS (PARTITION BY g ORDER BY v) ORDER BY id",
        // OVER (name ORDER BY …) extending a base window.
        "SELECT id, row_number() OVER (w ORDER BY v) FROM t WINDOW w AS (PARTITION BY g) ORDER BY id",
        // Multiple named windows.
        "SELECT id, rank() OVER w1, sum(v) OVER w2 FROM t \
         WINDOW w1 AS (PARTITION BY g ORDER BY v), w2 AS (ORDER BY id) ORDER BY id",
    ];

    let path = std::env::temp_dir().join(format!("gsql-nw-{}.db", std::process::id()));
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
        "{} named-window queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
