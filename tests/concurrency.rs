//! Track C: the VFS advisory-locking contract. Two connections over one shared
//! (in-process) VFS serialize writers: a second writer is rejected with
//! `Error::Busy` while the first holds an open write transaction, exactly as two
//! SQLite connections would.

#![cfg(feature = "std")]

use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::{Connection, Error, Value};

#[test]
fn second_writer_is_busy_during_open_write_txn() {
    let vfs = MemoryVfs::new();
    // Connection A creates the database and a table.
    let mut a = Connection::create_vfs(&vfs, "db", 4096).unwrap();
    a.execute("CREATE TABLE t(x)").unwrap();

    // Connection B opens the same file through the shared VFS.
    let mut b = Connection::open_vfs(&vfs, "db").unwrap();

    // A opens a write transaction and stages a row (takes the RESERVED lock).
    a.execute("BEGIN").unwrap();
    a.execute("INSERT INTO t VALUES (1)").unwrap();

    // B's write is rejected while A holds the write-intent lock.
    let err = b.execute("INSERT INTO t VALUES (2)").unwrap_err();
    assert!(matches!(err, Error::Busy), "expected Busy, got {err:?}");

    // A commits and releases the lock.
    a.execute("COMMIT").unwrap();

    // Now B can write successfully.
    b.execute("INSERT INTO t VALUES (3)").unwrap();

    // Both rows are visible through the shared storage.
    let rows = a.query("SELECT x FROM t ORDER BY x").unwrap().rows;
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn writer_lock_released_after_autocommit() {
    // After an autocommitted statement the writer holds no lock, so another
    // connection can immediately write.
    let vfs = MemoryVfs::new();
    let mut a = Connection::create_vfs(&vfs, "db", 4096).unwrap();
    a.execute("CREATE TABLE t(x)").unwrap();
    a.execute("INSERT INTO t VALUES (1)").unwrap(); // autocommit: lock released

    let mut b = Connection::open_vfs(&vfs, "db").unwrap();
    // No open transaction on A → B writes without contention.
    b.execute("INSERT INTO t VALUES (2)").unwrap();
    assert_eq!(
        b.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(2)
    );
}

#[test]
fn rolled_back_writer_frees_the_lock() {
    let vfs = MemoryVfs::new();
    let mut a = Connection::create_vfs(&vfs, "db", 4096).unwrap();
    a.execute("CREATE TABLE t(x)").unwrap();
    let mut b = Connection::open_vfs(&vfs, "db").unwrap();

    a.execute("BEGIN").unwrap();
    a.execute("INSERT INTO t VALUES (1)").unwrap();
    // B is locked out...
    assert!(matches!(
        b.execute("INSERT INTO t VALUES (2)"),
        Err(Error::Busy)
    ));
    // ...until A rolls back.
    a.execute("ROLLBACK").unwrap();
    b.execute("INSERT INTO t VALUES (2)").unwrap();
    assert_eq!(
        b.query("SELECT x FROM t").unwrap().rows,
        vec![vec![Value::Integer(2)]]
    );
}
