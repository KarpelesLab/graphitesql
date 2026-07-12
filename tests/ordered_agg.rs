//! Track A: ordered aggregates — `group_concat(x ORDER BY y)`. Verified against
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
        Value::Text(s) => String::from(s.as_str()),
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
fn basic() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(g,v,id INTEGER PRIMARY KEY)")
        .unwrap();
    c.execute("INSERT INTO t(g,v) VALUES('a',3),('a',1),('a',2),('b',9),('b',5)")
        .unwrap();
    assert_eq!(
        rows_str(
            &c,
            "SELECT g, group_concat(v ORDER BY v) FROM t GROUP BY g ORDER BY g"
        ),
        "a|1,2,3\nb|5,9"
    );
    assert_eq!(
        rows_str(&c, "SELECT group_concat(v, '-' ORDER BY v DESC) FROM t"),
        "9-5-3-2-1"
    );
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(g TEXT, v INT, w TEXT, id INTEGER PRIMARY KEY);\
                 INSERT INTO t(g,v,w) VALUES \
                 ('a',3,'x'),('a',1,'z'),('a',2,'y'),('b',9,'m'),('b',5,'n'),('b',5,'a')";
    let queries = [
        "SELECT g, group_concat(v ORDER BY v) FROM t GROUP BY g ORDER BY g",
        "SELECT g, group_concat(w ORDER BY w DESC) FROM t GROUP BY g ORDER BY g",
        "SELECT group_concat(v, '|' ORDER BY v) FROM t",
        "SELECT g, group_concat(w ORDER BY v) FROM t GROUP BY g ORDER BY g",
    ];

    let path = std::env::temp_dir().join(format!("gsql-ga-{}.db", std::process::id()));
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
        "{} ordered-aggregate queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
