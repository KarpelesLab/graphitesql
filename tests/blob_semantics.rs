//! Differential testing of BLOB semantics: storage-class ordering (NULL <
//! numbers < text < blob), blob comparison/equality (a blob never equals text
//! with the same bytes), and blob-returning functions. Checked against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(path: &str, sql: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
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
fn blob_semantics_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-blob-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE x(id INTEGER PRIMARY KEY, v);\
        INSERT INTO x(v) VALUES (x'01'),(x'0102'),(x'00'),('text'),(42),(3.5),(NULL),(x'ff'),('');";
    sqlite3(&path, setup);
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    let queries = [
        "SELECT id, typeof(v), quote(v) FROM x ORDER BY v, id",
        "SELECT id FROM x WHERE v > x'00' ORDER BY id",
        "SELECT id FROM x WHERE typeof(v) = 'blob' ORDER BY id",
        "SELECT id FROM x WHERE v = x'01' ORDER BY id",
        "SELECT id FROM x WHERE v < 'text' ORDER BY id",
        "SELECT DISTINCT typeof(v) FROM x ORDER BY 1",
        // Constant blob ops.
        "SELECT x'4142' = 'AB'",
        "SELECT x'41' < x'42', x'42' < x'41', x'41' = x'41'",
        "SELECT length(x'010203'), hex(x'abcd'), quote(x'00ff')",
        "SELECT typeof(x'00'), typeof(cast('AB' as blob))",
        "SELECT hex(x'41' || x'42')",
        // substr on a blob slices bytes and returns a blob.
        "SELECT quote(substr(x'41424344', 2, 2))",
        "SELECT quote(substr(x'0102030405', -2))",
        "SELECT typeof(substr(x'4142', 1, 1))",
        "SELECT quote(min(v)), quote(max(v)) FROM x",
        "SELECT count(*) FROM x WHERE v IS NOT NULL",
    ];
    for q in queries {
        let want = sqlite3(&path, &format!("{q};"));
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "blob query diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}
