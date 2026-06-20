//! Track A: aggregate `FILTER (WHERE …)`. Verified against the `sqlite3` CLI.

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
fn basic_filter() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(g, v)").unwrap();
    c.execute("INSERT INTO t VALUES ('a',1),('a',2),('a',-3),('b',5),('b',-1)")
        .unwrap();
    // count/sum with FILTER, grouped.
    assert_eq!(
        rows_str(
            &c,
            "SELECT g, count(*) FILTER (WHERE v > 0), sum(v) FILTER (WHERE v > 0) \
             FROM t GROUP BY g ORDER BY g"
        ),
        "a|2|3\nb|1|5"
    );
    // Ungrouped FILTER over the whole table.
    assert_eq!(
        rows_str(&c, "SELECT count(*) FILTER (WHERE v < 0) FROM t"),
        "2"
    );
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(g TEXT, v INT);\
                 INSERT INTO t VALUES \
                 ('a',1),('a',2),('a',-3),('b',5),('b',-1),('c',NULL)";
    let queries = [
        "SELECT count(*) FILTER (WHERE v > 0) FROM t",
        "SELECT sum(v) FILTER (WHERE v > 0) FROM t",
        "SELECT avg(v) FILTER (WHERE v < 0) FROM t",
        "SELECT g, count(*) FILTER (WHERE v IS NOT NULL) FROM t GROUP BY g ORDER BY g",
        "SELECT g, total(v) FILTER (WHERE v > 0) FROM t GROUP BY g ORDER BY g",
        "SELECT count(v) FILTER (WHERE v <> 1), count(*) FROM t",
        "SELECT group_concat(g) FILTER (WHERE v > 0) FROM t",
    ];

    let path = std::env::temp_dir().join(format!("gsql-filter-{}.db", std::process::id()));
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
        "{} FILTER queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
