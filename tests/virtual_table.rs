//! `CREATE VIRTUAL TABLE` + executor integration (roadmap D1b).
//!
//! These tests are graphite-only: the example `series` module is engine-specific
//! and a real `sqlite3` would not understand `USING series(…)`, so there is no
//! differential cross-read here (and no `integrity_check` / sqlite3 round-trip on
//! a database that contains a virtual table).
#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

/// Collect a query's rows into a `Vec<Vec<Value>>`.
fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    conn.query(sql).expect("query").rows
}

#[test]
fn create_and_select_series() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 5)")
        .unwrap();
    let got = rows(&conn, "SELECT value FROM v");
    assert_eq!(
        got,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)],
            vec![Value::Integer(5)],
        ]
    );
}

#[test]
fn aggregate_over_series_with_where() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 5)")
        .unwrap();
    // WHERE is re-applied by run_core over the full scan (D1a best_index default).
    let got = rows(&conn, "SELECT count(*), max(value) FROM v WHERE value > 2");
    assert_eq!(got, vec![vec![Value::Integer(3), Value::Integer(5)]]);
}

#[test]
fn series_with_step_and_descending() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE evens USING series(0, 10, 2)")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT value FROM evens"),
        vec![0, 2, 4, 6, 8, 10]
            .into_iter()
            .map(|n| vec![Value::Integer(n)])
            .collect::<Vec<_>>()
    );
    conn.execute("CREATE VIRTUAL TABLE down USING series(3, 1, -1)")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT value FROM down"),
        vec![3, 2, 1]
            .into_iter()
            .map(|n| vec![Value::Integer(n)])
            .collect::<Vec<_>>()
    );
}

#[test]
fn vtab_appears_in_sqlite_master() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 5)")
        .unwrap();
    let names = rows(&conn, "SELECT name FROM sqlite_master WHERE type='table'");
    assert!(names.contains(&vec![Value::Text("v".into())]));
    // The stored sql is the original CREATE VIRTUAL TABLE text.
    let sql = rows(&conn, "SELECT sql FROM sqlite_master WHERE name='v'");
    assert_eq!(sql.len(), 1);
    match &sql[0][0] {
        Value::Text(t) => assert!(t.to_ascii_uppercase().contains("VIRTUAL TABLE")),
        other => panic!("expected text sql, got {other:?}"),
    }
}

#[test]
fn vtab_in_a_join() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE TABLE t(x INTEGER)").unwrap();
    conn.execute("INSERT INTO t VALUES (2), (4)").unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 5)")
        .unwrap();
    let got = rows(
        &conn,
        "SELECT t.x FROM t JOIN v ON v.value = t.x ORDER BY t.x",
    );
    assert_eq!(got, vec![vec![Value::Integer(2)], vec![Value::Integer(4)]]);
}

#[test]
fn duplicate_and_if_not_exists() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 5)")
        .unwrap();
    // A bare re-create is an error.
    assert!(conn
        .execute("CREATE VIRTUAL TABLE v USING series(1, 9)")
        .is_err());
    // IF NOT EXISTS makes it a silent no-op; the original definition stands.
    conn.execute("CREATE VIRTUAL TABLE IF NOT EXISTS v USING series(1, 9)")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT count(*) FROM v"),
        vec![vec![Value::Integer(5)]]
    );
}

#[test]
fn unknown_module_is_rejected() {
    let mut conn = Connection::open_memory().unwrap();
    let err = conn
        .execute("CREATE VIRTUAL TABLE v USING nosuchmodule(1)")
        .unwrap_err();
    assert!(format!("{err:?}").to_lowercase().contains("module"));
}

#[test]
fn bad_arguments_fail_at_create() {
    let mut conn = Connection::open_memory().unwrap();
    // The series module validates its integer args at connect time.
    assert!(conn
        .execute("CREATE VIRTUAL TABLE v USING series(notanint)")
        .is_err());
    // And the table must not have been created.
    let names = rows(&conn, "SELECT name FROM sqlite_master WHERE name='v'");
    assert!(names.is_empty());
}

#[test]
fn writes_are_rejected() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 5)")
        .unwrap();
    assert!(conn.execute("INSERT INTO v VALUES (6)").is_err());
    assert!(conn.execute("UPDATE v SET value = 0").is_err());
    assert!(conn.execute("DELETE FROM v").is_err());
}

#[test]
fn drop_removes_the_vtab() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 5)")
        .unwrap();
    conn.execute("DROP TABLE v").unwrap();
    let names = rows(&conn, "SELECT name FROM sqlite_master WHERE name='v'");
    assert!(names.is_empty());
    // Querying it now fails (no such table).
    assert!(conn.query("SELECT value FROM v").is_err());
}

#[test]
fn file_roundtrip_reopen_and_query() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("graphite_vtab_{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    {
        let mut conn = Connection::create(path).unwrap();
        conn.execute("CREATE VIRTUAL TABLE v USING series(2, 8, 2)")
            .unwrap();
        assert_eq!(
            rows(&conn, "SELECT value FROM v"),
            vec![2, 4, 6, 8]
                .into_iter()
                .map(|n| vec![Value::Integer(n)])
                .collect::<Vec<_>>()
        );
    }

    // Reopen: the schema entry must reload and the module re-instantiate.
    {
        let conn = Connection::open(path).unwrap();
        let names = rows(&conn, "SELECT name FROM sqlite_master WHERE type='table'");
        assert!(names.contains(&vec![Value::Text("v".into())]));
        assert_eq!(
            rows(&conn, "SELECT value FROM v"),
            vec![2, 4, 6, 8]
                .into_iter()
                .map(|n| vec![Value::Integer(n)])
                .collect::<Vec<_>>()
        );
    }

    let _ = std::fs::remove_file(path);
}
