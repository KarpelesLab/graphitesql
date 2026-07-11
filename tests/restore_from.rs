//! `Connection::restore_from` — the destination-side primitive of an online
//! backup. It replaces the connection's `main` database with a serialized image,
//! preserving registered callbacks and settings, and the restored database passes
//! `PRAGMA integrity_check`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn count(c: &Connection, tbl: &str) -> i64 {
    match &c
        .query(&format!("SELECT count(*) FROM {tbl}"))
        .unwrap()
        .rows[0][0]
    {
        Value::Integer(n) => *n,
        v => panic!("unexpected {v:?}"),
    }
}

#[test]
fn restore_replaces_data_and_stays_valid() {
    let mut src = Connection::open_memory().unwrap();
    src.execute("CREATE TABLE t(x)").unwrap();
    src.execute("INSERT INTO t VALUES(1),(2),(3),(4)").unwrap();
    let image = src.serialize().unwrap();

    let mut dst = Connection::open_memory().unwrap();
    dst.execute("CREATE TABLE other(y)").unwrap();
    dst.execute("INSERT INTO other VALUES(99)").unwrap();

    dst.restore_from(&image).unwrap();

    // The source's table is present with all rows…
    assert_eq!(count(&dst, "t"), 4);
    // …and the destination's prior table is gone.
    assert!(dst.query("SELECT * FROM other").is_err());
    // The restored image is a valid SQLite database.
    assert_eq!(
        dst.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
    // The destination is writable after the restore.
    dst.execute("INSERT INTO t VALUES(5)").unwrap();
    assert_eq!(count(&dst, "t"), 5);
}

#[test]
fn restore_preserves_registered_hooks() {
    use std::cell::Cell;
    use std::rc::Rc;

    let mut src = Connection::open_memory().unwrap();
    src.execute("CREATE TABLE t(x)").unwrap();
    src.execute("INSERT INTO t VALUES(1)").unwrap();
    let image = src.serialize().unwrap();

    let mut dst = Connection::open_memory().unwrap();
    let commits = Rc::new(Cell::new(0));
    {
        let cc = commits.clone();
        dst.register_commit_hook(move || {
            cc.set(cc.get() + 1);
            0
        });
    }
    dst.restore_from(&image).unwrap();
    // The commit hook survived the restore and fires on a subsequent write.
    dst.execute("INSERT INTO t VALUES(2)").unwrap();
    assert_eq!(commits.get(), 1);
}

#[test]
fn restore_rejected_inside_transaction() {
    let src = Connection::open_memory().unwrap();
    let image = src.serialize().unwrap();
    let mut dst = Connection::open_memory().unwrap();
    dst.execute("BEGIN").unwrap();
    assert!(
        dst.restore_from(&image).is_err(),
        "restore into an open transaction must be rejected"
    );
}
