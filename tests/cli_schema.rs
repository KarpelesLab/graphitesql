//! The `.schema` shell command prints each object's `CREATE` statement exactly
//! as the `sqlite3` shell does, including two `printSchemaLine` quirks: a quoted
//! table name gets an `IF NOT EXISTS`, and a view is annotated with its output
//! column names on a trailing `/* view(cols) */` comment line. Verified
//! byte-for-byte against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn schema(bin: &str, db: &str) -> String {
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b".schema\n").unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn schema_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let uniq = std::process::id();
    let db = dir.join(format!("gsql_schema_{uniq}.db"));
    let db = db.to_str().unwrap();
    let _ = std::fs::remove_file(db);
    let schema_sql = "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b REAL, c TEXT);\
        CREATE TABLE \"my tbl\"(x UNIQUE, y);\
        CREATE INDEX ix ON t(a);\
        CREATE VIEW v1 AS SELECT id,a FROM t;\
        CREATE VIEW v2 AS SELECT a AS x, b+1 AS y, c FROM t;\
        CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END;";
    assert!(Command::new("sqlite3")
        .arg(db)
        .arg(schema_sql)
        .status()
        .unwrap()
        .success());
    assert_eq!(schema("sqlite3", db), schema(g, db));
    let _ = std::fs::remove_file(db);
}
