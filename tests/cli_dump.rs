//! The `.dump` shell command emits the whole database as SQL text, byte-for-byte
//! as the `sqlite3` shell does: a `PRAGMA foreign_keys=OFF;` / `BEGIN
//! TRANSACTION;` header, each table's `CREATE` followed by an `INSERT` per row,
//! then the `sqlite_sequence` rows (for AUTOINCREMENT), then the views, triggers,
//! and indexes (`ORDER BY type COLLATE NOCASE DESC`), and a closing `COMMIT;`.
//! Values use dump quoting (round-trip `%!.20g` reals, lowercase-hex blobs);
//! generated columns are excluded from the `INSERT`; a quoted table name gets an
//! `IF NOT EXISTS`. Verified byte-for-byte against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `.dump` against `db` through `bin`'s shell (piping the command on stdin).
fn dump(bin: &str, db: &str) -> String {
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b".dump\n").unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn dump_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let uniq = std::process::id();

    let schemas: &[(&str, &str)] = &[
        (
            "mixed",
            "CREATE TABLE t(id INTEGER PRIMARY KEY, x REAL, s TEXT, b BLOB);\
             INSERT INTO t VALUES(1,2.0/3.0,'a''b',x'00ff'),(2,0.1,'hi',NULL),(3,100.0,'',x'deadBEEF');\
             CREATE INDEX ix ON t(x);\
             CREATE VIEW v AS SELECT id,x FROM t WHERE x>0;\
             CREATE TABLE u(a,b);INSERT INTO u VALUES(1,'two'),(3.5,x'01');\
             CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END;",
        ),
        (
            "autoinc",
            "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a, b REAL);\
             INSERT INTO t(a,b) VALUES('x',5.0),('y',2.5);",
        ),
        (
            "generated",
            "CREATE TABLE t(a REAL, b, g REAL AS (a+1) STORED, v AS (a*2));\
             INSERT INTO t(a,b) VALUES(5.0,'x'),(2.5,'y');",
        ),
        (
            "without_rowid",
            "CREATE TABLE t(k TEXT PRIMARY KEY, a REAL) WITHOUT ROWID;\
             INSERT INTO t VALUES('a',100.0),('b',2.5);",
        ),
        (
            "quoted_name",
            "CREATE TABLE \"my tbl\"(a UNIQUE, b);INSERT INTO \"my tbl\" VALUES(1,2.0);",
        ),
        ("empty", "CREATE TABLE t(a,b);"),
    ];

    for (name, schema) in schemas {
        let db = dir.join(format!("gsql_dump_{uniq}_{name}.db"));
        let db = db.to_str().unwrap();
        let _ = std::fs::remove_file(db);
        assert!(Command::new("sqlite3")
            .arg(db)
            .arg(schema)
            .status()
            .unwrap()
            .success());
        assert_eq!(
            dump("sqlite3", db),
            dump(g, db),
            "dump mismatch for `{name}`"
        );
        let _ = std::fs::remove_file(db);
    }
}
