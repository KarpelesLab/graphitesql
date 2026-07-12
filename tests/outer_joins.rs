//! Track A: `RIGHT [OUTER] JOIN` and `FULL [OUTER] JOIN`. Verified against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
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
fn right_and_full() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("CREATE TABLE b(y)").unwrap();
    c.execute("INSERT INTO a VALUES (1),(2)").unwrap();
    c.execute("INSERT INTO b VALUES (2),(3)").unwrap();
    // RIGHT JOIN keeps b's unmatched row (y=3) with NULL x.
    assert_eq!(
        rows_str(
            &c,
            "SELECT a.x, b.y FROM a RIGHT JOIN b ON a.x=b.y ORDER BY b.y"
        ),
        "2|2\nNULL|3"
    );
    // FULL JOIN keeps both unmatched: a.x=1 (NULL y) and b.y=3 (NULL x).
    assert_eq!(
        rows_str(
            &c,
            "SELECT a.x, b.y FROM a FULL OUTER JOIN b ON a.x=b.y ORDER BY a.x, b.y"
        ),
        "NULL|3\n1|NULL\n2|2"
    );
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE emp(id INT, dept INT, name TEXT);\
                 CREATE TABLE dept(id INT, dname TEXT);\
                 INSERT INTO emp VALUES (1,10,'a'),(2,20,'b'),(3,NULL,'c');\
                 INSERT INTO dept VALUES (10,'x'),(30,'z')";
    let queries = [
        "SELECT emp.name, dept.dname FROM emp RIGHT JOIN dept ON emp.dept=dept.id ORDER BY dept.id",
        "SELECT emp.name, dept.dname FROM emp LEFT JOIN dept ON emp.dept=dept.id ORDER BY emp.id",
        "SELECT emp.name, dept.dname FROM emp FULL JOIN dept ON emp.dept=dept.id ORDER BY emp.id, dept.id",
        "SELECT count(*) FROM emp RIGHT JOIN dept ON emp.dept=dept.id",
        "SELECT count(*) FROM emp FULL OUTER JOIN dept ON emp.dept=dept.id",
    ];

    let path = std::env::temp_dir().join(format!("gsql-oj-{}.db", std::process::id()));
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
        // sqlite renders NULL as empty; normalize ours.
        let got = rows_str(&g, q).replace("NULL", "");
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} outer-join queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
