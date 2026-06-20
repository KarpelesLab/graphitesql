//! Pre-comparison type affinity: a typeless column (BLOB/NONE affinity) must not
//! be text-coerced against a TEXT column, while a literal (no affinity) is. These
//! are checked byte-for-byte against real sqlite3.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3(path: &str, sql: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

#[test]
fn comparison_affinity_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-aff-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE x(none_col, t TEXT, i INTEGER, r REAL);\
                 INSERT INTO x VALUES (1, '1', 1, 1.0), (5, '5', 5, 5.0);";
    sqlite3(&path, setup);
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    // Column-vs-column (affinity pairs) and column-vs-literal cases.
    let exprs = [
        "none_col = t",  // BLOB vs TEXT: no coercion -> 1 = '1' false
        "t = none_col",  // symmetric
        "i = t",         // INTEGER vs TEXT: numeric applied -> true
        "none_col = i",  // BLOB vs INTEGER: numeric applied -> true
        "r = t",         // REAL vs TEXT: numeric applied -> true
        "none_col = '1'", // BLOB col vs text literal: no coercion (col holds int)
        "none_col = 1",  // BLOB col vs int literal: int vs int -> true
        "t = 1",         // TEXT col vs int literal: text-coerce literal -> true
        "t = '1'",       // TEXT vs text literal -> true
        "i = '5'",       // INTEGER col vs text literal -> numeric -> true
    ];
    for e in exprs {
        let q = format!("SELECT {e} FROM x ORDER BY rowid");
        let want = sqlite3(&path, &format!("{q};"));
        let got = g
            .query(&q)
            .unwrap()
            .rows
            .iter()
            .map(|row| match &row[0] {
                graphitesql::Value::Null => String::new(),
                graphitesql::Value::Integer(i) => i.to_string(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "affinity comparison diverged: {e}");
    }
    let _ = std::fs::remove_file(&path);
}
