//! WITHOUT ROWID tables fire BEFORE/AFTER row triggers on INSERT/UPDATE/DELETE
//! (per row, interleaved), including `UPDATE OF col` filtering, `RAISE(IGNORE)`,
//! and the child triggers fired by an FK action (CASCADE / SET NULL / UPDATE
//! CASCADE) on a WITHOUT ROWID child. graphite previously fired no triggers at
//! all for WITHOUT ROWID DML. Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn log(setup: &str, action: &str) -> Vec<String> {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(&format!(
        "CREATE TABLE t(k PRIMARY KEY, v) WITHOUT ROWID;
         CREATE TABLE log(m);
         {setup}"
    ))
    .unwrap();
    c.execute_batch(action).unwrap();
    c.query("SELECT m FROM log ORDER BY rowid")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.as_str().to_string(),
            v => panic!("{v:?}"),
        })
        .collect()
}

#[test]
fn insert_before_after_interleaved() {
    let l = log(
        "CREATE TRIGGER b BEFORE INSERT ON t BEGIN INSERT INTO log VALUES('b'||NEW.v); END;
         CREATE TRIGGER a AFTER INSERT ON t BEGIN INSERT INTO log VALUES('a'||NEW.v); END;",
        "INSERT INTO t VALUES(1,'1'),(2,'2');",
    );
    assert_eq!(l, ["b1", "a1", "b2", "a2"]);
}

#[test]
fn delete_before_after_interleaved() {
    let l = log(
        "CREATE TRIGGER b BEFORE DELETE ON t BEGIN INSERT INTO log VALUES('b'||OLD.v); END;
         CREATE TRIGGER a AFTER DELETE ON t BEGIN INSERT INTO log VALUES('a'||OLD.v); END;
         INSERT INTO t VALUES(1,'1'),(2,'2');",
        "DELETE FROM t;",
    );
    assert_eq!(l, ["b1", "a1", "b2", "a2"]);
}

#[test]
fn update_before_after_interleaved() {
    let l = log(
        "CREATE TRIGGER b BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES('b'||OLD.v||NEW.v); END;
         CREATE TRIGGER a AFTER UPDATE ON t BEGIN INSERT INTO log VALUES('a'||OLD.v||NEW.v); END;
         INSERT INTO t VALUES(1,'1'),(2,'2');",
        "UPDATE t SET v=v||'x';",
    );
    assert_eq!(l, ["b11x", "a11x", "b22x", "a22x"]);
}

#[test]
fn update_of_column_filter() {
    // Fires only when the named column is assigned.
    let fired = log(
        "CREATE TRIGGER a AFTER UPDATE OF v ON t BEGIN INSERT INTO log VALUES('v'); END;
         INSERT INTO t VALUES(1,'a');",
        "UPDATE t SET v='z';",
    );
    assert_eq!(fired, ["v"]);
    let not_fired = log(
        "CREATE TRIGGER a AFTER UPDATE OF v ON t BEGIN INSERT INTO log VALUES('v'); END;
         INSERT INTO t VALUES(1,'a');",
        "UPDATE t SET k=9;",
    );
    assert!(not_fired.is_empty());
}

#[test]
fn before_insert_raise_ignore_skips_row() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(k PRIMARY KEY, v) WITHOUT ROWID;
         CREATE TRIGGER b BEFORE INSERT ON t WHEN NEW.k=2 BEGIN SELECT RAISE(IGNORE); END;
         INSERT INTO t VALUES(1,'a'),(2,'b'),(3,'c');",
    )
    .unwrap();
    let ks: Vec<i64> = c
        .query("SELECT k FROM t ORDER BY k")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(ks, [1, 3]);
}

#[test]
fn upsert_do_update_fires_update_triggers() {
    // `INSERT … ON CONFLICT DO UPDATE` that converts to an update fires the
    // BEFORE INSERT trigger (for the attempted insert) then BEFORE/AFTER UPDATE —
    // never AFTER INSERT. Matches sqlite's `bib, buab, auab` sequence.
    let l = log(
        "CREATE TRIGGER bi BEFORE INSERT ON t BEGIN INSERT INTO log VALUES('bi'||NEW.v); END;
         CREATE TRIGGER ai AFTER INSERT ON t BEGIN INSERT INTO log VALUES('ai'||NEW.v); END;
         CREATE TRIGGER bu BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES('bu'||OLD.v||NEW.v); END;
         CREATE TRIGGER au AFTER UPDATE ON t BEGIN INSERT INTO log VALUES('au'||OLD.v||NEW.v); END;
         INSERT INTO t VALUES(1,'a');",
        "INSERT INTO t VALUES(1,'b') ON CONFLICT(k) DO UPDATE SET v=excluded.v;",
    );
    assert_eq!(l, ["bia", "aia", "bib", "buab", "auab"]);
}

#[test]
fn upsert_do_update_before_raise_ignore_keeps_old_row() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(k PRIMARY KEY, v) WITHOUT ROWID;
         INSERT INTO t VALUES(1,'a');
         CREATE TRIGGER bu BEFORE UPDATE ON t BEGIN SELECT RAISE(IGNORE); END;",
    )
    .unwrap();
    c.execute("INSERT INTO t VALUES(1,'b') ON CONFLICT(k) DO UPDATE SET v=excluded.v")
        .unwrap();
    assert_eq!(
        c.query("SELECT v FROM t").unwrap().rows[0][0],
        Value::Text("a".into())
    );
}

#[test]
fn fk_cascade_into_wr_child_fires_child_triggers() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE log(m);
         CREATE TABLE c(px, v, PRIMARY KEY(px, v),
                        FOREIGN KEY(px) REFERENCES p ON DELETE CASCADE) WITHOUT ROWID;
         CREATE TRIGGER ct AFTER DELETE ON c BEGIN INSERT INTO log VALUES('d'||OLD.v); END;
         INSERT INTO p VALUES(1);
         INSERT INTO c VALUES(1,'a'),(1,'b');",
    )
    .unwrap();
    c.execute("DELETE FROM p").unwrap();
    let msgs: Vec<String> = c
        .query("SELECT m FROM log ORDER BY rowid")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.as_str().to_string(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(msgs, ["da", "db"]);
    assert_eq!(
        c.query("SELECT count(*) FROM c").unwrap().rows[0][0],
        Value::Integer(0)
    );
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}
