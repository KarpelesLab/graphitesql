//! `Connection::set_authorizer` — the engine equivalent of
//! `sqlite3_set_authorizer`. graphitesql authorizes the statement-level action of
//! each statement (with its primary object name), enough to build a read-only or
//! per-table/operation sandbox. `SQLITE_DENY` rejects the statement; `SQLITE_OK`
//! allows it. The `sqlite3` CLI cannot exercise the C-API authorizer, so the
//! oracle is SQLite's action-code contract, asserted directly.

#![cfg(feature = "std")]

use graphitesql::Connection;
use graphitesql::exec::auth_action as act;
use std::cell::RefCell;
use std::rc::Rc;

fn seeded() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2)").unwrap();
    c
}

#[test]
fn read_only_sandbox_denies_writes_and_ddl() {
    let mut c = seeded();
    // Deny every mutating action; reads stay allowed.
    c.set_authorizer(|action, _a1, _a2, _db, _tr| match action {
        act::INSERT | act::UPDATE | act::DELETE | act::DROP_TABLE | act::CREATE_TABLE => 1,
        _ => 0,
    });
    assert!(c.query("SELECT * FROM t").is_ok());
    assert!(c.execute("INSERT INTO t VALUES(3)").is_err());
    assert!(c.execute("UPDATE t SET x = 9").is_err());
    assert!(c.execute("DELETE FROM t").is_err());
    assert!(c.execute("DROP TABLE t").is_err());
    assert!(c.execute("CREATE TABLE u(y)").is_err());
    // The denied writes left the data untouched.
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        graphitesql::Value::Integer(2)
    );
}

#[test]
fn clearing_the_authorizer_reallows_everything() {
    let mut c = seeded();
    c.set_authorizer(|_, _, _, _, _| 1);
    assert!(c.execute("INSERT INTO t VALUES(3)").is_err());
    c.clear_authorizer();
    assert!(c.execute("INSERT INTO t VALUES(3)").is_ok());
}

#[test]
fn deny_by_table_name() {
    let mut c = seeded();
    c.execute("CREATE TABLE secret(y)").unwrap();
    // Deny inserts only into `secret` (by the action's arg1 = table name).
    c.set_authorizer(|action, a1, _a2, _db, _tr| {
        if action == act::INSERT && a1 == Some("secret") {
            1
        } else {
            0
        }
    });
    assert!(c.execute("INSERT INTO secret VALUES(1)").is_err());
    assert!(c.execute("INSERT INTO t VALUES(3)").is_ok());
}

#[test]
fn action_codes_are_reported() {
    type Action = (i32, Option<String>, Option<String>);
    let mut c = seeded();
    let log: Rc<RefCell<Vec<Action>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let l = log.clone();
        c.set_authorizer(move |action, a1, a2, _db, _tr| {
            l.borrow_mut()
                .push((action, a1.map(str::to_string), a2.map(str::to_string)));
            0
        });
    }
    let _ = c.query("SELECT x FROM t WHERE x > 0").unwrap();
    let _ = c.execute("UPDATE t SET x = 5 WHERE x = 1").unwrap();
    let seen = log.borrow();
    // A single-table SELECT reports SELECT then a READ naming the table.
    assert!(seen.contains(&(act::SELECT, None, None)));
    assert!(seen.contains(&(act::READ, Some("t".into()), Some(String::new()))));
    // UPDATE reports the table and the assigned column.
    assert!(seen.contains(&(act::UPDATE, Some("t".into()), Some("x".into()))));
}
