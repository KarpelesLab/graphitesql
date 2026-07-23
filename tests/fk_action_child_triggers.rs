//! A foreign-key action fires the child row's own triggers, as SQLite does:
//! `ON DELETE CASCADE` fires the child's BEFORE/AFTER DELETE triggers (once per
//! cascaded row, in order); `ON DELETE SET NULL` / `ON UPDATE CASCADE` fire the
//! child's BEFORE/AFTER UPDATE triggers. This holds even with the default
//! `recursive_triggers=OFF` (an FK action is not a self-re-entry). graphite
//! applied the row change but never fired the child triggers. `changes()` still
//! reports only the outer statement's direct rows.
//!
//! (rowid tables; WITHOUT ROWID tables do not yet fire any triggers — a separate,
//! broader gap.) Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn log_of(setup: &str, action: &str) -> Vec<String> {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(&format!(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE log(msg);
         INSERT INTO p VALUES(1);
         {setup}"
    ))
    .unwrap_or_else(|e| panic!("setup failed: {e}"));
    c.execute_batch(action)
        .unwrap_or_else(|e| panic!("action failed: {e}"));
    c.query("SELECT msg FROM log ORDER BY rowid")
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
fn delete_cascade_fires_child_before_and_after() {
    let log = log_of(
        "CREATE TABLE c(px REFERENCES p ON DELETE CASCADE, v);
         CREATE TRIGGER cb BEFORE DELETE ON c BEGIN INSERT INTO log VALUES('b'||OLD.v); END;
         CREATE TRIGGER ca AFTER DELETE ON c BEGIN INSERT INTO log VALUES('a'||OLD.v); END;
         INSERT INTO c VALUES(1,'1'),(1,'2');",
        "DELETE FROM p;",
    );
    // Per row, BEFORE then AFTER, in row order.
    assert_eq!(log, ["b1", "a1", "b2", "a2"]);
}

#[test]
fn delete_set_null_fires_child_update_trigger() {
    let log = log_of(
        "CREATE TABLE c(px REFERENCES p ON DELETE SET NULL, v);
         CREATE TRIGGER cu AFTER UPDATE ON c
           BEGIN INSERT INTO log VALUES('u'||OLD.v||IFNULL(NEW.px,'-')); END;
         INSERT INTO c VALUES(1,'a');",
        "DELETE FROM p;",
    );
    assert_eq!(log, ["ua-"]);
}

#[test]
fn update_cascade_fires_child_update_trigger() {
    let log = log_of(
        "CREATE TABLE c(px REFERENCES p ON UPDATE CASCADE, v);
         CREATE TRIGGER cu AFTER UPDATE ON c
           BEGIN INSERT INTO log VALUES('u'||OLD.px||'>'||NEW.px); END;
         INSERT INTO c VALUES(1,'a');",
        "UPDATE p SET id=9;",
    );
    assert_eq!(log, ["u1>9"]);
}

#[test]
fn cascade_and_triggers_do_not_inflate_changes() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE log(m);
         CREATE TABLE c(px REFERENCES p ON DELETE CASCADE, v);
         CREATE TRIGGER ca AFTER DELETE ON c BEGIN INSERT INTO log VALUES(OLD.v); END;
         INSERT INTO p VALUES(1);
         INSERT INTO c VALUES(1,'a'),(1,'b');",
    )
    .unwrap();
    let n = c.execute("DELETE FROM p").unwrap();
    assert_eq!(n, 1, "changes() is the direct parent delete only");
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}
