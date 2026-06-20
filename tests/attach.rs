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
        c.query("SELECT m.b FROM main.t AS m WHERE m.a = 2")
            .unwrap()
            .rows[0][0],
        Value::Text("y".into())
    );
    // Unknown database / cross-database join are clear errors (not silent).
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    assert!(c.query("SELECT * FROM zzz.t").is_err());
    assert!(c.query("SELECT * FROM t, aux.t").is_err());
}

#[test]
fn cross_database_create_read_write() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(99)").unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();

    // CREATE / INSERT into the attached database, then read it back.
    c.execute("CREATE TABLE aux.t(id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    assert_eq!(
        c.execute("INSERT INTO aux.t VALUES(1,'alice'),(2,'bob')")
            .unwrap(),
        2
    );
    let r = c.query("SELECT id, name FROM aux.t ORDER BY id").unwrap();
    assert_eq!(r.rows[0][1], Value::Text("alice".into()));
    assert_eq!(r.rows[1][1], Value::Text("bob".into()));

    // The two databases are isolated: main.t still has its own single row, and
    // each catalog lists only its own table.
    assert_eq!(
        c.query("SELECT a FROM t").unwrap().rows[0][0],
        Value::Integer(99)
    );
    assert_eq!(
        c.query("SELECT count(*) FROM main.t").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT count(*) FROM aux.sqlite_master")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );

    // UPDATE / DELETE / DROP against the attached database.
    c.execute("UPDATE aux.t SET name='ALICE' WHERE id=1")
        .unwrap();
    c.execute("DELETE FROM aux.t WHERE id=2").unwrap();
    let r = c.query("SELECT id, name FROM aux.t").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Integer(1), Value::Text("ALICE".into())]]
    );
    c.execute("DROP TABLE aux.t").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM aux.sqlite_master")
            .unwrap()
            .rows[0][0],
        Value::Integer(0)
    );
    // main.t is untouched by the DROP in aux.
    assert_eq!(
        c.query("SELECT count(*) FROM main.sqlite_master")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn temp_tables() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(99)").unwrap();

    // CREATE TEMP TABLE goes to the temp database, not main's catalog.
    c.execute("CREATE TEMP TABLE tmp(id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM sqlite_master").unwrap().rows[0][0],
        Value::Integer(1) // just `t`
    );
    // database_list shows temp at seq 1.
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(
        r.rows[1],
        vec![
            Value::Integer(1),
            Value::Text("temp".into()),
            Value::Text("".into())
        ]
    );
    // sqlite_temp_master lists temp objects.
    assert_eq!(
        c.query("SELECT name FROM sqlite_temp_master").unwrap().rows[0][0],
        Value::Text("tmp".into())
    );

    // Unqualified DML/reads resolve to the temp table.
    c.execute("INSERT INTO tmp VALUES(1,'a'),(2,'b')").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM tmp").unwrap().rows[0][0],
        Value::Integer(2)
    );
    c.execute("UPDATE tmp SET v='Z' WHERE id=1").unwrap();
    c.execute("DELETE FROM tmp WHERE id=2").unwrap();
    let r = c.query("SELECT id, v FROM tmp").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Integer(1), Value::Text("Z".into())]]
    );

    // A temp table shadows a same-named main table for unqualified names.
    c.execute("CREATE TEMP TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    c.execute("INSERT INTO t VALUES(2)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(2)
    );
    assert_eq!(
        c.query("SELECT count(*) FROM main.t").unwrap().rows[0][0],
        Value::Integer(1)
    );

    // DROP of the temp table leaves the main table intact.
    c.execute("DROP TABLE t").unwrap();
    assert_eq!(
        c.query("SELECT a FROM t").unwrap().rows[0][0],
        Value::Integer(99)
    );
}

#[test]
fn temp_tables_do_not_persist_to_a_file() {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-temp-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE persist(a)").unwrap();
        c.execute("CREATE TEMP TABLE ephemeral(b)").unwrap();
        c.execute("INSERT INTO ephemeral VALUES(1)").unwrap();
    }
    // Reopening the file shows only the persistent table.
    let c = Connection::open(&path).unwrap();
    let names: Vec<_> = c
        .query("SELECT name FROM sqlite_master")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(names, vec![Value::Text("persist".into())]);
    let _ = std::fs::remove_file(&path);
}
