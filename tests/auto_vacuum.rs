//! Track C6: `auto_vacuum` awareness. graphite reads auto_vacuum databases
//! created by sqlite3 and now also writes them, maintaining the pointer-map
//! pages on commit (C6b-2). Ordinary (auto_vacuum=NONE) databases are
//! unaffected.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

#[test]
fn ordinary_database_reports_none_and_writes() {
    let mut c = Connection::open_memory().unwrap();
    assert_eq!(
        c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
        Value::Integer(0)
    );
    // Writes to a NONE database are unaffected.
    c.execute("CREATE TABLE q(x)").unwrap();
    c.execute("INSERT INTO q VALUES(1),(2)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM q").unwrap().rows[0][0],
        Value::Integer(2)
    );
    // This database is non-empty now, so changing auto_vacuum is a no-op
    // (matches sqlite); NONE stays NONE.
    assert!(c.execute("PRAGMA auto_vacuum=NONE").is_ok());
    assert!(c.execute("PRAGMA auto_vacuum=0").is_ok());
    assert!(c.execute("PRAGMA auto_vacuum=FULL").is_ok());
    assert_eq!(
        c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn enabling_auto_vacuum_on_empty_db_then_writing() {
    // On an empty database, PRAGMA auto_vacuum=FULL switches the mode, and
    // subsequent writes are maintained with a correct pointer map.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA auto_vacuum=FULL").unwrap();
    assert_eq!(
        c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
        Value::Integer(1)
    );
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    for i in 1..=300i64 {
        c.execute(&format!("INSERT INTO t VALUES({i}, 'x{i}')"))
            .unwrap();
    }
    c.execute("DELETE FROM t WHERE a % 2 = 0").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(150)
    );
    // graphite's own integrity check is happy with the in-memory result.
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn writes_a_sqlite_created_auto_vacuum_file() {
    if std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-av-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    // sqlite3 builds a FULL auto_vacuum database.
    let st = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA auto_vacuum=FULL; CREATE TABLE t(a,b); INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z'); CREATE INDEX i ON t(a);")
        .status()
        .unwrap();
    assert!(st.success());

    {
        let mut c = Connection::open(&path).unwrap();
        // graphite reads it and reports the mode.
        assert_eq!(
            c.query("SELECT b FROM t WHERE a=3").unwrap().rows[0][0],
            Value::Text("z".into())
        );
        assert_eq!(
            c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
            Value::Integer(1)
        );
        // graphite now writes the auto_vacuum file, maintaining the ptrmap.
        c.execute("INSERT INTO t VALUES(4,'w')").unwrap();
        c.execute("DELETE FROM t WHERE a=1").unwrap();
        c.execute("CREATE TABLE t2(z)").unwrap();
        c.execute("INSERT INTO t2 VALUES('hello')").unwrap();
        assert_eq!(
            c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
            Value::Integer(3)
        );
    }

    // sqlite3 finds the graphite-written file intact and sees the new data.
    let out = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    let out = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg("SELECT b FROM t WHERE a=4; SELECT z FROM t2;")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "w\nhello");
    let _ = std::fs::remove_file(&path);
}
