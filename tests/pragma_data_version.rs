//! `PRAGMA data_version` — sqlite's `SQLITE_FCNTL_DATA_VERSION`: a per-connection
//! value that stays constant while *this* connection reads and writes, but
//! changes when *another* connection commits to the same database. graphite's
//! multi-connection WAL sharing (ROADMAP C9c) lets two in-process `Connection`s
//! observe this; the exact integer is arbitrary, the *behaviour* is the contract.

#![cfg(feature = "std")]

use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::{Connection, Value};

fn data_version(c: &Connection) -> i64 {
    match &c.query("PRAGMA data_version").unwrap().rows[0][0] {
        Value::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

/// A fresh connection reports `1`, and its own writes never change it.
#[test]
fn stable_across_own_writes() {
    let mut c = Connection::open_memory().unwrap();
    assert_eq!(data_version(&c), 1);
    c.execute("CREATE TABLE t(x)").unwrap();
    assert_eq!(data_version(&c), 1);
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    c.execute("INSERT INTO t VALUES(2)").unwrap();
    assert_eq!(
        data_version(&c),
        1,
        "same-connection writes must not bump it"
    );
}

/// A commit from another connection over the same database changes this
/// connection's `data_version` (and stays put again until the next foreign
/// commit).
#[test]
fn bumps_on_foreign_commit() {
    let vfs = MemoryVfs::new();
    {
        let mut seed = Connection::create_vfs(&vfs, "db", 4096).unwrap();
        seed.execute("PRAGMA journal_mode=WAL").unwrap();
        seed.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        seed.execute("INSERT INTO t(v) VALUES (1)").unwrap();
    }
    let a = Connection::open_vfs(&vfs, "db").unwrap();
    let mut b = Connection::open_vfs(&vfs, "db").unwrap();

    let v0 = data_version(&a);
    // A re-reading without any foreign write keeps the same value.
    assert_eq!(data_version(&a), v0);

    // B commits; A's next data_version read observes the change.
    b.execute("INSERT INTO t(v) VALUES (2)").unwrap();
    let v1 = data_version(&a);
    assert_ne!(v1, v0, "a foreign commit must change data_version");

    // Stable again until the next foreign commit.
    assert_eq!(data_version(&a), v1);

    // Another foreign commit moves it once more.
    b.execute("INSERT INTO t(v) VALUES (3)").unwrap();
    assert_ne!(data_version(&a), v1);

    // B's own data_version never moved for B's own writes.
    assert_eq!(data_version(&b), 1);
}
