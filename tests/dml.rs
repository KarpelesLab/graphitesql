//! Phase 7: end-to-end DDL/DML through `Connection::execute` + `query`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-dml-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

fn ints(rows: &[Vec<Value>], i: usize) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[i] {
            Value::Integer(v) => *v,
            o => panic!("not int: {o:?}"),
        })
        .collect()
}

#[test]
fn memory_create_insert_select() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, age INT)")
        .unwrap();
    assert_eq!(
        c.execute("INSERT INTO users(name, age) VALUES ('ada', 36), ('grace', 45)")
            .unwrap(),
        2
    );
    // Explicit rowid via the INTEGER PRIMARY KEY column.
    c.execute("INSERT INTO users(id, name, age) VALUES (10, 'edsger', 75)")
        .unwrap();

    let r = c.query("SELECT id, name FROM users ORDER BY id").unwrap();
    assert_eq!(ints(&r.rows, 0), vec![1, 2, 10]);
    assert_eq!(r.rows[0][1], Value::Text("ada".into()));
    assert_eq!(r.rows[2][1], Value::Text("edsger".into()));

    let agg = c.query("SELECT count(*), avg(age) FROM users").unwrap();
    assert_eq!(agg.rows[0][0], Value::Integer(3));
}

#[test]
fn update_and_delete() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("INSERT INTO t(v) VALUES (10),(20),(30),(40)")
        .unwrap();

    assert_eq!(
        c.execute("UPDATE t SET v = v + 1 WHERE id >= 3").unwrap(),
        2
    );
    let r = c.query("SELECT v FROM t ORDER BY id").unwrap();
    assert_eq!(ints(&r.rows, 0), vec![10, 20, 31, 41]);

    assert_eq!(c.execute("DELETE FROM t WHERE v > 25").unwrap(), 2);
    let r = c.query("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(ints(&r.rows, 0), vec![1, 2]);

    assert_eq!(c.execute("DELETE FROM t").unwrap(), 2); // delete all
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn pragma_table_info_and_page_size() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT NOT NULL, n INT DEFAULT 7)")
        .unwrap();
    let info = c.query("PRAGMA table_info(t)").unwrap();
    assert_eq!(info.columns[0], "cid");
    assert_eq!(info.rows.len(), 3);
    // id: pk=1, notnull=1 (INTEGER PRIMARY KEY)
    assert_eq!(info.rows[0][1], Value::Text("id".into()));
    assert_eq!(info.rows[0][5], Value::Integer(1)); // pk
                                                    // name: notnull=1
    assert_eq!(info.rows[1][3], Value::Integer(1));
    // n: default '7'
    assert_eq!(info.rows[2][4], Value::Text("7".into()));

    let ps = c.query("PRAGMA page_size").unwrap();
    assert_eq!(ps.rows[0][0], Value::Integer(4096));
}

#[test]
fn not_null_constraint_enforced() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT)")
        .unwrap();
    // A NULL into a NOT NULL column is rejected.
    assert!(c.execute("INSERT INTO t(name) VALUES (NULL)").is_err());
    assert!(c.execute("INSERT INTO t(note) VALUES ('x')").is_err()); // name omitted -> NULL
                                                                     // A valid row is accepted; nullable columns may be NULL.
    c.execute("INSERT INTO t(name) VALUES ('ok')").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(1)
    );
    // UPDATE that would violate NOT NULL is rejected.
    assert!(c.execute("UPDATE t SET name = NULL WHERE id = 1").is_err());
}

#[test]
fn additional_scalar_functions() {
    let c = Connection::open_memory().unwrap();
    let r = c
        .query("SELECT sign(-5), sign(0), sign(9), concat('a', NULL, 'b'), concat_ws('-', 1, 2, 3)")
        .unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Value::Integer(-1));
    assert_eq!(row[1], Value::Integer(0));
    assert_eq!(row[2], Value::Integer(1));
    assert_eq!(row[3], Value::Text("ab".into()));
    assert_eq!(row[4], Value::Text("1-2-3".into()));

    let r = c
        .query("SELECT zeroblob(3), unhex('41FF'), quote('a''b')")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Blob(vec![0, 0, 0]));
    assert_eq!(r.rows[0][1], Value::Blob(vec![0x41, 0xff]));
    assert_eq!(r.rows[0][2], Value::Text("'a''b'".into()));
}

#[test]
fn check_constraints_enforced() {
    let mut c = Connection::open_memory().unwrap();
    c.execute(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, qty INT CHECK(qty > 0), \
         kind TEXT, CHECK(kind <> 'bad'))",
    )
    .unwrap();
    c.execute("INSERT INTO t(qty, kind) VALUES (5, 'ok')")
        .unwrap();
    // Column-level CHECK violation.
    assert!(c
        .execute("INSERT INTO t(qty, kind) VALUES (0, 'ok')")
        .is_err());
    // Table-level CHECK violation.
    assert!(c
        .execute("INSERT INTO t(qty, kind) VALUES (5, 'bad')")
        .is_err());
    // UPDATE into a violating state is rejected.
    assert!(c.execute("UPDATE t SET qty = -2 WHERE id = 1").is_err());
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn unique_constraint_and_conflict_clauses() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, email TEXT UNIQUE, n INT)")
        .unwrap();
    c.execute("INSERT INTO t(email, n) VALUES ('a', 1)")
        .unwrap();
    // Duplicate UNIQUE value is rejected by default (ABORT).
    assert!(c
        .execute("INSERT INTO t(email, n) VALUES ('a', 2)")
        .is_err());
    // OR IGNORE silently skips the conflicting row (0 rows affected).
    assert_eq!(
        c.execute("INSERT OR IGNORE INTO t(email, n) VALUES ('a', 3)")
            .unwrap(),
        0
    );
    assert_eq!(
        c.query("SELECT n FROM t").unwrap().rows[0][0],
        Value::Integer(1)
    );
    // OR REPLACE replaces the conflicting row.
    c.execute("INSERT OR REPLACE INTO t(email, n) VALUES ('a', 9)")
        .unwrap();
    let r = c.query("SELECT email, n FROM t").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][1], Value::Integer(9));
    // Duplicate explicit rowid (PRIMARY KEY) is rejected.
    c.execute("INSERT INTO t(email, n) VALUES ('b', 1)")
        .unwrap();
    let some_id = match c.query("SELECT id FROM t WHERE email='b'").unwrap().rows[0][0] {
        Value::Integer(v) => v,
        _ => panic!(),
    };
    assert!(c
        .execute(&format!("INSERT INTO t(id, email) VALUES ({some_id}, 'c')"))
        .is_err());
}

#[test]
fn defaults_applied() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, status TEXT DEFAULT 'new', n INT DEFAULT 0)")
        .unwrap();
    c.execute("INSERT INTO t(id) VALUES (1)").unwrap();
    let r = c.query("SELECT status, n FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Text("new".into()));
    assert_eq!(r.rows[0][1], Value::Integer(0));
}

#[test]
fn transactions_commit_and_rollback() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)").unwrap();

    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    // Read-your-writes inside the transaction.
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(2)
    );
    c.execute("ROLLBACK").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(0)
    );

    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2),(3)").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(3)
    );
}

#[test]
fn persists_across_reopen() {
    let path = temp_path("persist.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE kv(k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        c.execute("INSERT INTO kv VALUES (1,'one'),(2,'two')")
            .unwrap();
    }
    {
        let c = Connection::open_readonly(&path).unwrap();
        let r = c.query("SELECT v FROM kv ORDER BY k").unwrap();
        assert_eq!(r.rows[0][0], Value::Text("one".into()));
        assert_eq!(r.rows[1][0], Value::Text("two".into()));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn sqlite3_reads_sql_built_database() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let path = temp_path("sqlbuilt.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT, price REAL)")
            .unwrap();
        for i in 1..=300 {
            c.execute_params("INSERT INTO items(name, price) VALUES (?1, ?2)", &params(i))
                .unwrap();
        }
    }
    let run = |sql: &str| {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg(sql)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(run("PRAGMA integrity_check;"), "ok");
    assert_eq!(run("SELECT count(*) FROM items;"), "300");
    assert_eq!(run("SELECT name FROM items WHERE id = 1;"), "item-1");
    assert_eq!(run("SELECT id FROM items ORDER BY id DESC LIMIT 1;"), "300");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

fn params(i: i64) -> graphitesql::exec::eval::Params {
    graphitesql::exec::eval::Params {
        positional: vec![
            Value::Text(format!("item-{i}")),
            Value::Real(i as f64 * 1.5),
        ],
        named: vec![],
    }
}

#[test]
fn delete_update_with_order_by_limit() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let path = temp_path("dml_limit.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        c.execute("CREATE INDEX iv ON t(v)").unwrap();
        c.execute("INSERT INTO t(v) VALUES (5),(3),(8),(1),(9),(2),(7)")
            .unwrap();

        // UPDATE the two largest to 0.
        c.execute("UPDATE t SET v = 0 ORDER BY v DESC LIMIT 2")
            .unwrap();
        let r = c.query("SELECT count(*) FROM t WHERE v = 0").unwrap();
        assert_eq!(r.rows[0][0], Value::Integer(2));

        // DELETE the two smallest non-zero rows.
        c.execute("DELETE FROM t WHERE v > 0 ORDER BY v LIMIT 2")
            .unwrap();
        // Started with 7 rows; 0 deleted by update, 2 deleted now -> 5 left.
        let r = c.query("SELECT count(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Integer(5));

        // OFFSET form.
        c.execute("DELETE FROM t ORDER BY id LIMIT 1 OFFSET 2")
            .unwrap();
        let r = c.query("SELECT count(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Integer(4));
    }
    if sqlite {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check;")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("ok"));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}
