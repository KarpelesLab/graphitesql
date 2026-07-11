//! `Connection::register_commit_hook` / `register_rollback_hook` — the engine
//! equivalents of `sqlite3_commit_hook` / `sqlite3_rollback_hook`. The commit hook
//! fires just before a *write* transaction commits (an autocommit write, an
//! explicit `COMMIT`, or the finalizing release of an implicit transaction's
//! outermost savepoint); returning non-zero converts the commit into a rollback.
//! The rollback hook fires whenever a transaction rolls back (an explicit
//! `ROLLBACK`, or a commit vetoed by the commit hook).
//!
//! The `sqlite3` CLI cannot exercise these C-API callbacks, so the oracle here is
//! SQLite's documented semantics, asserted directly.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::cell::Cell;
use std::rc::Rc;

fn count(c: &Connection) -> i64 {
    match &c.query("SELECT count(*) FROM t").unwrap().rows[0][0] {
        Value::Integer(n) => *n,
        v => panic!("unexpected {v:?}"),
    }
}

fn setup() -> (Connection, Rc<Cell<u32>>, Rc<Cell<u32>>) {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    let commits = Rc::new(Cell::new(0));
    let rollbacks = Rc::new(Cell::new(0));
    {
        let cc = commits.clone();
        c.register_commit_hook(move || {
            cc.set(cc.get() + 1);
            0
        });
    }
    {
        let rc = rollbacks.clone();
        c.register_rollback_hook(move || rc.set(rc.get() + 1));
    }
    (c, commits, rollbacks)
}

#[test]
fn commit_hook_fires_on_autocommit_and_explicit_commit() {
    let (mut c, commits, rollbacks) = setup();
    // Autocommit write: exactly one commit, regardless of the number of rows.
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    assert_eq!(commits.get(), 1);
    // Explicit transaction: one commit for the whole transaction.
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO t VALUES(4)").unwrap();
    c.execute("INSERT INTO t VALUES(5)").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(commits.get(), 2);
    assert_eq!(rollbacks.get(), 0);
    assert_eq!(count(&c), 5);
}

#[test]
fn empty_transaction_does_not_fire_commit_hook() {
    let (mut c, commits, _r) = setup();
    // A transaction that writes nothing does not fire the commit hook (SQLite
    // only fires it for write transactions).
    c.execute("BEGIN").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(commits.get(), 0);
    // A read is likewise not a commit.
    let _ = c.query("SELECT 1").unwrap();
    assert_eq!(commits.get(), 0);
}

#[test]
fn rollback_hook_fires_on_explicit_rollback() {
    let (mut c, commits, rollbacks) = setup();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    c.execute("ROLLBACK").unwrap();
    assert_eq!(rollbacks.get(), 1);
    assert_eq!(commits.get(), 0);
    assert_eq!(count(&c), 0);
}

#[test]
fn commit_hook_veto_converts_commit_to_rollback() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    let rollbacks = Rc::new(Cell::new(0));
    {
        let rc = rollbacks.clone();
        c.register_rollback_hook(move || rc.set(rc.get() + 1));
    }
    // A commit hook returning non-zero vetoes the commit.
    c.register_commit_hook(|| 1);
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    c.execute("COMMIT").unwrap();
    // The insert was rolled back, and the rollback hook fired.
    assert_eq!(count(&c), 0);
    assert_eq!(rollbacks.get(), 1);
    // An autocommit write is likewise vetoed.
    c.execute("INSERT INTO t VALUES(2)").unwrap();
    assert_eq!(count(&c), 0);
    assert_eq!(rollbacks.get(), 2);
    // Removing the veto lets writes through again.
    c.remove_commit_hook();
    c.execute("INSERT INTO t VALUES(3)").unwrap();
    assert_eq!(count(&c), 1);
}

#[test]
fn removed_hooks_do_not_fire() {
    let (mut c, commits, rollbacks) = setup();
    c.remove_commit_hook();
    c.remove_rollback_hook();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO t VALUES(2)").unwrap();
    c.execute("ROLLBACK").unwrap();
    assert_eq!(commits.get(), 0);
    assert_eq!(rollbacks.get(), 0);
}
