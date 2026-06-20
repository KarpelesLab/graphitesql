//! Track C: `SAVEPOINT` / `RELEASE` / `ROLLBACK TO` nested transactions.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn col_i64(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => panic!("not an int"),
        })
        .collect()
}

#[test]
fn nested_rollback_and_release() {
    // Mirrors SQLite: SAVEPOINT a; +2; SAVEPOINT b; +3; ROLLBACK TO b; +4;
    // RELEASE a → {1,2,4}.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES (1)").unwrap();
    c.execute("SAVEPOINT a").unwrap();
    c.execute("INSERT INTO t VALUES (2)").unwrap();
    c.execute("SAVEPOINT b").unwrap();
    c.execute("INSERT INTO t VALUES (3)").unwrap();
    c.execute("ROLLBACK TO b").unwrap();
    c.execute("INSERT INTO t VALUES (4)").unwrap();
    c.execute("RELEASE a").unwrap();
    assert_eq!(col_i64(&c, "SELECT x FROM t ORDER BY x"), vec![1, 2, 4]);
}

#[test]
fn rollback_to_is_repeatable() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("SAVEPOINT s").unwrap();
    c.execute("INSERT INTO t VALUES (1)").unwrap();
    c.execute("ROLLBACK TO s").unwrap(); // savepoint still open
    c.execute("INSERT INTO t VALUES (2)").unwrap();
    c.execute("ROLLBACK TO s").unwrap();
    c.execute("INSERT INTO t VALUES (3)").unwrap();
    c.execute("RELEASE s").unwrap();
    assert_eq!(col_i64(&c, "SELECT x FROM t ORDER BY x"), vec![3]);
}

#[test]
fn savepoint_persists_after_release_to_disk() {
    let path = std::env::temp_dir().join(format!("gsql-sp-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(x)").unwrap();
        c.execute("SAVEPOINT s").unwrap();
        c.execute("INSERT INTO t VALUES (10),(20)").unwrap();
        c.execute("RELEASE s").unwrap(); // implicit txn commits to disk
    }
    // Reopen: the released changes are durable.
    let c = Connection::open(&path).unwrap();
    assert_eq!(col_i64(&c, "SELECT x FROM t ORDER BY x"), vec![10, 20]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn schema_change_rolled_back() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("SAVEPOINT s").unwrap();
    c.execute("CREATE TABLE temp_tab(y)").unwrap();
    c.execute("INSERT INTO temp_tab VALUES (1)").unwrap();
    c.execute("ROLLBACK TO s").unwrap();
    // The table created inside the savepoint is gone after rollback.
    assert!(c.query("SELECT * FROM temp_tab").is_err());
    c.execute("RELEASE s").unwrap();
    // The original table is still usable.
    c.execute("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(col_i64(&c, "SELECT x FROM t"), vec![1]);
}

#[test]
fn savepoint_within_begin() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO t VALUES (1)").unwrap();
    c.execute("SAVEPOINT s").unwrap();
    c.execute("INSERT INTO t VALUES (2)").unwrap();
    c.execute("ROLLBACK TO s").unwrap();
    c.execute("RELEASE s").unwrap(); // does not commit (still in BEGIN)
    c.execute("INSERT INTO t VALUES (3)").unwrap();
    c.execute("ROLLBACK").unwrap(); // discards the whole transaction
    assert_eq!(col_i64(&c, "SELECT count(*) FROM t"), vec![0]);
}

#[test]
fn release_unknown_savepoint_errors() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    assert!(c.execute("RELEASE nope").is_err());
    assert!(c.execute("ROLLBACK TO nope").is_err());
}
