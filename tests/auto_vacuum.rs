//! Track C6: `auto_vacuum` awareness. graphite does not yet maintain pointer-map
//! pages, so it reads auto_vacuum databases (created by sqlite3) but refuses to
//! write them rather than corrupt their ptrmap. Ordinary (auto_vacuum=NONE)
//! databases are unaffected.

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
    // Setting NONE is a no-op; FULL/INCREMENTAL are rejected (can't maintain ptrmap).
    assert!(c.execute("PRAGMA auto_vacuum=NONE").is_ok());
    assert!(c.execute("PRAGMA auto_vacuum=0").is_ok());
    assert!(c.execute("PRAGMA auto_vacuum=FULL").is_err());
    assert!(c.execute("PRAGMA auto_vacuum=INCREMENTAL").is_err());
    assert!(c.execute("PRAGMA auto_vacuum=2").is_err());
}

#[test]
fn reads_sqlite_auto_vacuum_file_but_refuses_writes() {
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
        // graphite reads it (ptrmap pages are not part of any b-tree, so scans
        // and seeks skip them naturally) and reports the mode.
        assert_eq!(
            c.query("SELECT b FROM t WHERE a=3").unwrap().rows[0][0],
            Value::Text("z".into())
        );
        assert_eq!(
            c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
            Value::Integer(1)
        );
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into())
        );
        // A write is refused rather than silently corrupting the ptrmap.
        assert!(c.execute("INSERT INTO t VALUES(4,'w')").is_err());
        assert!(c.execute("DELETE FROM t WHERE a=1").is_err());
        assert!(c.execute("CREATE TABLE t2(z)").is_err());
    }

    // sqlite3 still finds the file intact (graphite only read it).
    let out = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    let _ = std::fs::remove_file(&path);
}
