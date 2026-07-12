//! Differential test for a `main.`-qualified single-table / join `SELECT` running
//! on the VDBE. The VDBE runs only in a `main`-only context (no attached / temp
//! database, `main` the resolution default), so a `main.`-qualified *source* is
//! unambiguous and equivalent to the bare name — it is now stripped and routed on
//! the VDBE instead of deferring to the tree-walker with every schema qualifier.
//! Results must match the real `sqlite3` CLI, for which `main.t` == `t`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(setup: &[&str], query: &str) -> String {
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push(';');
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(&script)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn render(result: &graphitesql::QueryResult) -> String {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => String::from(s.as_str()),
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
fn main_qualified_source_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }

    let setup = [
        "CREATE TABLE t(a INTEGER, b TEXT)",
        "CREATE TABLE u(a INTEGER, c TEXT)",
        "INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z')",
        "INSERT INTO u VALUES(1,'p'),(2,'q')",
    ];

    let mut g = Connection::open_memory().unwrap();
    for s in &setup {
        g.execute(s).unwrap();
    }

    let queries = [
        "SELECT a, b FROM main.t WHERE a >= 1 ORDER BY a", // main-qualified scan
        "SELECT b FROM main.t AS t WHERE t.a = 2",         // alias + two-part col
        "SELECT count(*), sum(a), max(b) FROM main.t",     // aggregates
        "SELECT DISTINCT a FROM main.t ORDER BY 1",        // DISTINCT
        // main-qualified join of two main tables
        "SELECT t.b, u.c FROM main.t JOIN main.u ON t.a = u.a ORDER BY t.a",
        "SELECT a FROM main.t NOT INDEXED WHERE a > 1", // main + NOT INDEXED
        // a three-part `main.t.col` column still defers, but must be correct
        "SELECT main.t.a FROM main.t ORDER BY 1",
    ];

    for q in queries {
        let want = sqlite3(&setup, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "main-qualified query diverged: {q}");
    }
}
