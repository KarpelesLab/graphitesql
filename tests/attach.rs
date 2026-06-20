//! Track C (multi-schema): the database registry and `ATTACH`/`DETACH`.
//! Built up piece by piece (C1: `PRAGMA database_list`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

#[test]
fn database_list_reports_main() {
    // In-memory main has an empty file path.
    let c = Connection::open_memory().unwrap();
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Integer(0),
            Value::Text("main".into()),
            Value::Text("".into())
        ]]
    );

    // A file-backed main reports its path.
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(a)").unwrap();
    }
    let c = Connection::open(&path).unwrap();
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(r.rows[0][1], Value::Text("main".into()));
    assert_eq!(r.rows[0][2], Value::Text(path.clone()));
    let _ = std::fs::remove_file(&path);
}
