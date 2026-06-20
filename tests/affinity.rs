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
        "none_col = t",   // BLOB vs TEXT: no coercion -> 1 = '1' false
        "t = none_col",   // symmetric
        "i = t",          // INTEGER vs TEXT: numeric applied -> true
        "none_col = i",   // BLOB vs INTEGER: numeric applied -> true
        "r = t",          // REAL vs TEXT: numeric applied -> true
        "none_col = '1'", // BLOB col vs text literal: no coercion (col holds int)
        "none_col = 1",   // BLOB col vs int literal: int vs int -> true
        "t = 1",          // TEXT col vs int literal: text-coerce literal -> true
        "t = '1'",        // TEXT vs text literal -> true
        "i = '5'",        // INTEGER col vs text literal -> numeric -> true
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

/// CAST across types, including blob reinterpretation and NUMERIC's text-vs-value
/// distinction, checked against sqlite3 via quote() so blobs/types are explicit.
#[test]
fn cast_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    for e in [
        "CAST(x'3132' AS TEXT)",
        "CAST(x'3132' AS INTEGER)",
        "CAST(x'3132' AS REAL)",
        "CAST(x'3132' AS NUMERIC)",
        "CAST('3.0' AS NUMERIC)",
        "CAST(2.0 AS NUMERIC)",
        "CAST('2e2' AS NUMERIC)",
        "CAST('3.5' AS NUMERIC)",
        "CAST('abc' AS NUMERIC)",
        "CAST('5' AS NUMERIC)",
        "CAST(65 AS BLOB)",
        "CAST(3.5 AS BLOB)",
        "CAST('1.9' AS INTEGER)",
        "CAST(-3.9 AS INTEGER)",
        "CAST('  12  ' AS INTEGER)",
        "CAST(1e100 AS INTEGER)",
        "typeof(CAST('5' AS NUMERIC))",
        "typeof(CAST('5.5' AS NUMERIC))",
    ] {
        let q = format!("SELECT quote({e})");
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!("{q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = match &c.query(&q).unwrap().rows[0][0] {
            graphitesql::Value::Text(s) => s.clone(),
            graphitesql::Value::Null => String::new(),
            other => format!("{other:?}"),
        };
        assert_eq!(got, want, "CAST diverged: {e}");
    }
}

/// Storage affinity: values are coerced to each column's affinity on INSERT
/// (e.g. text '123' into an INTEGER column becomes the integer 123). Checked
/// against sqlite3 via both `typeof` and the stored value.
#[test]
fn storage_affinity_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-storeaff-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup =
        "CREATE TABLE x(id INTEGER PRIMARY KEY, i INTEGER, r REAL, n NUMERIC, t TEXT, b BLOB);\
        INSERT INTO x(i,r,n,t,b) VALUES ('123','4.5','6',7,8.5);\
        INSERT INTO x(i,r,n,t,b) VALUES (9.0,9,'10.0',11.5,'12');\
        INSERT INTO x(i,r,n,t,b) VALUES ('x','y','3.14','z','w');\
        INSERT INTO x(i,r,n,t,b) VALUES (NULL,NULL,'2.0e2',NULL,NULL);";
    sqlite3(&path, setup);
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    for q in [
        "SELECT id, typeof(i), typeof(r), typeof(n), typeof(t), typeof(b) FROM x ORDER BY id",
        "SELECT id, i, r, n, t, b FROM x ORDER BY id",
        "SELECT id FROM x WHERE n = 6 ORDER BY id",
        "SELECT id FROM x WHERE i = 123 ORDER BY id",
    ] {
        let want = sqlite3(&path, &format!("{q};"));
        let got = render_rows(&g.query(q).unwrap());
        assert_eq!(got, want, "storage affinity diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}

fn render_rows(result: &graphitesql::QueryResult) -> String {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    graphitesql::Value::Null => String::new(),
                    graphitesql::Value::Integer(i) => i.to_string(),
                    graphitesql::Value::Text(s) => s.clone(),
                    graphitesql::Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    graphitesql::Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
