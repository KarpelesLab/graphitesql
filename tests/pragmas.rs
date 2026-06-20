//! Track C: introspection PRAGMAs (`index_list`, `index_info`,
//! `foreign_key_list`, `freelist_count`, …). Verified against the `sqlite3` CLI.

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
    let setup = [
        "CREATE TABLE t(a, b UNIQUE, c)",
        "CREATE INDEX ix ON t(a, c)",
        "CREATE INDEX ixp ON t(a) WHERE c > 0",
        "CREATE TABLE p(id INTEGER PRIMARY KEY, k)",
        "CREATE TABLE ch(x, y, FOREIGN KEY(x) REFERENCES p(id) ON DELETE CASCADE)",
    ];
    let queries = [
        "PRAGMA index_list(t)",
        "PRAGMA index_info(ix)",
        "PRAGMA index_info(ixp)",
        "PRAGMA foreign_key_list(ch)",
        "PRAGMA freelist_count",
        "PRAGMA application_id",
    ];

    let path = std::env::temp_dir().join(format!("gsql-prag-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(setup.join(";"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut g = Connection::open_memory().unwrap();
    for s in setup {
        g.execute(s).unwrap();
    }

    let mut failures = Vec::new();
    // foreign_key_check needs its own fixture with dangling references.
    {
        let fkc = "CREATE TABLE pp(id INTEGER PRIMARY KEY);\
                   CREATE TABLE cc(x, y, FOREIGN KEY(x) REFERENCES pp(id));\
                   INSERT INTO pp VALUES (1);\
                   INSERT INTO cc VALUES (1,'a'),(2,'b'),(9,'c')";
        let fpath = std::env::temp_dir().join(format!("gsql-fkc-{}.db", std::process::id()));
        let fpath = fpath.to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&fpath);
        Command::new("sqlite3")
            .arg(&fpath)
            .arg(fkc)
            .output()
            .unwrap();
        let want = {
            let o = Command::new("sqlite3")
                .arg(&fpath)
                .arg("PRAGMA foreign_key_check")
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let _ = std::fs::remove_file(&fpath);
        let mut gc = Connection::open_memory().unwrap();
        for s in fkc.split(';') {
            if !s.trim().is_empty() {
                gc.execute(s).unwrap();
            }
        }
        let got = rows_str(&gc, "PRAGMA foreign_key_check");
        if got != want {
            failures.push(format!(
                "  foreign_key_check\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
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
        "{} PRAGMA queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
