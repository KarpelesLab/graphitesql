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

#[test]
fn attach_and_detach_in_memory() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("ATTACH DATABASE '' AS aux2").unwrap();
    // Attached databases start at seq 2 (seq 1 is reserved for temp).
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![
                Value::Integer(0),
                Value::Text("main".into()),
                Value::Text("".into())
            ],
            vec![
                Value::Integer(2),
                Value::Text("aux".into()),
                Value::Text("".into())
            ],
            vec![
                Value::Integer(3),
                Value::Text("aux2".into()),
                Value::Text("".into())
            ],
        ]
    );

    // Duplicate / reserved names are rejected.
    assert!(c.execute("ATTACH ':memory:' AS aux").is_err());
    assert!(c.execute("ATTACH ':memory:' AS main").is_err());
    assert!(c.execute("ATTACH ':memory:' AS temp").is_err());

    // DETACH removes it; main/temp and unknown names are rejected.
    c.execute("DETACH aux").unwrap();
    assert!(c.execute("DETACH main").is_err());
    assert!(c.execute("DETACH nope").is_err());
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(r.rows.len(), 2); // main + aux2
    assert_eq!(r.rows[1][1], Value::Text("aux2".into()));
}

#[test]
fn schema_qualified_read_main() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,'x'),(2,'y')").unwrap();
    // `main.t` resolves to the main database.
    let r = c.query("SELECT a, b FROM main.t ORDER BY a").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][1], Value::Text("x".into()));
    // A table-qualified alias works too.
    assert_eq!(
        c.query("SELECT m.b FROM main.t AS m WHERE m.a = 2").unwrap().rows[0][0],
        Value::Text("y".into())
    );
    // Unknown database / cross-database join are clear errors (not silent).
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    assert!(c.query("SELECT * FROM zzz.t").is_err());
    assert!(c.query("SELECT * FROM t, aux.t").is_err());
}
