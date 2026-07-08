//! SQLite stores an integer-valued real in a REAL column using an *integer*
//! serial type (the `MEM_IntReal` space optimization): `100.0` in a `REAL`
//! column is on disk as the integer `100`. Reading it back must promote the
//! integer to a real, exactly as SQLite's `OP_Column` realifies a REAL-affinity
//! column — so `typeof` is `real` and the value renders/compares as a float.
//!
//! graphite read such a value as an integer, so reading a database *written by
//! sqlite* returned wrong types/renderings for whole-number reals (`typeof`
//! `integer`, `100` instead of `100.0`). The promotion is now applied at every
//! table-row and covering-index read path. graphite's own writes are unaffected
//! (it stores reals as an 8-byte float, which already reads back as real); the
//! bug only appeared when reading sqlite's compact encoding, so this test builds
//! the database with the `sqlite3` CLI and compares graphite's reads to it.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, db: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(db).arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn real_column_integer_serial_reads_as_real() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let uniq = std::process::id();

    // Build several sqlite databases, each exercising a different read path, then
    // compare graphite's answers to sqlite's for a spread of queries.
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "plain",
            "CREATE TABLE t(a REAL, b REAL, c NUMERIC, d INT);\
             INSERT INTO t VALUES(100.0,2.5,100.0,100.0),(0.0,-5.0,3.0,7.0),(1e10,0.001,2.0,9.0);",
            &[
                "SELECT quote(a),quote(b),quote(c),quote(d) FROM t ORDER BY rowid",
                "SELECT typeof(a),typeof(c),typeof(d) FROM t ORDER BY rowid",
                "SELECT sum(a),avg(a),max(a),min(a),total(a) FROM t",
                "SELECT a+1, a*2 FROM t ORDER BY rowid",
            ],
        ),
        (
            "indexed",
            "CREATE TABLE t(a REAL, b);CREATE INDEX ix ON t(a);\
             INSERT INTO t VALUES(100.0,'x'),(2.5,'y'),(3.0,'z'),(0.0,'w');",
            &[
                "SELECT quote(a) FROM t ORDER BY a", // covering index-order scan
                "SELECT a FROM t WHERE a>1.0 ORDER BY a", // index range seek
                "SELECT a FROM t WHERE a=100.0",     // index equality seek
                "SELECT DISTINCT a FROM t ORDER BY a",
                "SELECT a,count(*) FROM t GROUP BY a ORDER BY a",
            ],
        ),
        (
            "composite",
            "CREATE TABLE t(a REAL, b REAL, c);CREATE INDEX ix ON t(a,b);\
             INSERT INTO t VALUES(100.0,3.0,'x'),(2.5,4.0,'y'),(5.0,6.0,'z');",
            &[
                "SELECT a,b FROM t GROUP BY a,b ORDER BY a",
                "SELECT quote(a),quote(b) FROM t ORDER BY a",
            ],
        ),
        (
            "without_rowid",
            "CREATE TABLE t(a REAL, b TEXT, PRIMARY KEY(a)) WITHOUT ROWID;\
             INSERT INTO t VALUES(100.0,'x'),(2.5,'y'),(3.0,'z');",
            &["SELECT quote(a),typeof(a) FROM t ORDER BY a"],
        ),
    ];

    for (name, schema, queries) in cases {
        let db = dir.join(format!("gsql_realaff_{uniq}_{name}.db"));
        let db = db.to_str().unwrap();
        let _ = std::fs::remove_file(db);
        // Build the database with sqlite3 so whole-number reals get the compact
        // integer serial encoding.
        assert!(Command::new("sqlite3")
            .arg(db)
            .arg(schema)
            .status()
            .unwrap()
            .success());
        for q in *queries {
            assert_eq!(
                run("sqlite3", db, q),
                run(g, db, q),
                "case `{name}` query `{q}`"
            );
        }
        let _ = std::fs::remove_file(db);
    }
}
