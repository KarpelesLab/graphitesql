//! Phase 9: ALTER TABLE (ADD COLUMN, RENAME TO).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-alter-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

#[test]
fn add_column_applies_default_to_existing_rows() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT)")
        .unwrap();
    c.execute("INSERT INTO t(a) VALUES ('x'),('y')").unwrap();

    // Add a column with a default; pre-existing rows must read the default.
    c.execute("ALTER TABLE t ADD COLUMN n INT DEFAULT 42")
        .unwrap();
    c.execute("ALTER TABLE t ADD COLUMN note TEXT").unwrap(); // default NULL

    let r = c.query("SELECT id, a, n, note FROM t ORDER BY id").unwrap();
    assert_eq!(r.columns, vec!["id", "a", "n", "note"]);
    assert_eq!(r.rows[0][2], Value::Integer(42)); // default for old row
    assert_eq!(r.rows[0][3], Value::Null);

    // New rows can populate the added columns.
    c.execute("INSERT INTO t(a, n, note) VALUES ('z', 7, 'hi')")
        .unwrap();
    let r = c.query("SELECT n, note FROM t WHERE a = 'z'").unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(7));
    assert_eq!(r.rows[0][1], Value::Text("hi".into()));
}

#[test]
fn rename_column_updates_table_and_index() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let path = temp_path("renamecol.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, old_name TEXT)")
            .unwrap();
        c.execute("CREATE INDEX i ON t(old_name)").unwrap();
        c.execute("INSERT INTO t(old_name) VALUES ('x'),('y')")
            .unwrap();

        c.execute("ALTER TABLE t RENAME COLUMN old_name TO new_name")
            .unwrap();

        // Old name is gone; data is intact under the new name.
        assert!(c.query("SELECT old_name FROM t").is_err());
        let r = c.query("SELECT new_name FROM t ORDER BY new_name").unwrap();
        assert_eq!(r.rows[0][0], Value::Text("x".into()));
        assert_eq!(r.rows[1][0], Value::Text("y".into()));
    }
    if sqlite {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check; SELECT new_name FROM t ORDER BY new_name;")
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("ok"), "integrity: {s}");
        assert!(s.contains('x') && s.contains('y'));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn rename_table_updates_catalog_and_indexes() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let path = temp_path("rename.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE old(id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        c.execute("CREATE INDEX idx_v ON old(v)").unwrap();
        c.execute("INSERT INTO old(v) VALUES ('a'),('b'),('c')")
            .unwrap();

        c.execute("ALTER TABLE old RENAME TO renamed").unwrap();

        // Old name gone, new name works.
        assert!(c.schema().table("old").is_none());
        assert!(c.schema().table("renamed").is_some());
        let r = c.query("SELECT count(*) FROM renamed").unwrap();
        assert_eq!(r.rows[0][0], Value::Integer(3));
        // The index now belongs to the renamed table.
        assert_eq!(c.schema().index("idx_v").unwrap().tbl_name, "renamed");
    }
    if sqlite {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check; SELECT count(*) FROM renamed;")
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("ok"), "integrity: {s}");
        assert!(s.contains('3'));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn drop_column_rewrites_rows_and_keeps_integrity() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let path = temp_path("dropcol.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT, c INT)")
            .unwrap();
        c.execute("CREATE INDEX it_c ON t(c)").unwrap();
        c.execute("INSERT INTO t(a,b,c) VALUES (1,'x',10),(2,'y',20),(3,'z',30)")
            .unwrap();

        c.execute("ALTER TABLE t DROP COLUMN b").unwrap();

        let r = c.query("SELECT * FROM t ORDER BY id").unwrap();
        assert_eq!(r.columns, ["id", "a", "c"]);
        assert_eq!(r.rows.len(), 3);
        assert_eq!(
            r.rows[0],
            [Value::Integer(1), Value::Integer(1), Value::Integer(10)]
        );
        // The index on the surviving column still works (and its position shifted).
        let q = c.query("SELECT id FROM t WHERE c = 20").unwrap();
        assert_eq!(q.rows[0][0], Value::Integer(2));

        // Structural columns cannot be dropped.
        assert!(c.execute("ALTER TABLE t DROP COLUMN id").is_err()); // PRIMARY KEY
        assert!(c.execute("ALTER TABLE t DROP COLUMN c").is_err()); // indexed
        assert!(c.execute("ALTER TABLE t DROP COLUMN nope").is_err()); // missing
    }
    if sqlite {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check; SELECT a,c FROM t ORDER BY id;")
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("ok"), "integrity: {s}");
        assert!(s.contains("2|20"));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn create_temp_table_works() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TEMP TABLE t1(x INT)").unwrap();
    c.execute("CREATE TEMPORARY TABLE t2(y TEXT)").unwrap();
    c.execute("INSERT INTO t1 VALUES (1),(2)").unwrap();
    c.execute("INSERT INTO t2 VALUES ('a')").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t1").unwrap().rows[0][0],
        Value::Integer(2)
    );
    assert_eq!(
        c.query("SELECT y FROM t2").unwrap().rows[0][0],
        Value::Text("a".into())
    );
}
