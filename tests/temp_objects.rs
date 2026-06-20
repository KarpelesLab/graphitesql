//! Track C (multi-schema): `CREATE TEMP VIEW` / `CREATE TEMP TRIGGER` live in
//! the temp catalog (`sqlite_temp_master`), not main, while reads/firing still
//! work — matching sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn names(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            other => panic!("expected text name, got {other:?}"),
        })
        .collect()
}

#[test]
fn temp_view_lives_in_temp_master_and_reads() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE base(a, b)").unwrap();
    c.execute("INSERT INTO base VALUES (1, 10), (2, 20)")
        .unwrap();
    c.execute("CREATE TEMP VIEW v AS SELECT a, b FROM base WHERE a = 2")
        .unwrap();

    // The view is in temp, not main.
    assert_eq!(names(&c, "SELECT name FROM sqlite_temp_master"), vec!["v"]);
    let main = names(&c, "SELECT name FROM sqlite_master");
    assert!(
        !main.iter().any(|n| n == "v"),
        "view leaked into main: {main:?}"
    );

    // And it still reads correctly (body resolves `base` in main).
    let r = c.query("SELECT * FROM v").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(2), Value::Integer(20)]]);
}

#[test]
fn temp_view_shadows_main_view() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIEW v AS SELECT 1 AS x").unwrap();
    c.execute("CREATE TEMP VIEW v AS SELECT 2 AS x").unwrap();
    // Temp shadows main, like a temp table.
    let r = c.query("SELECT x FROM v").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(2)]]);
    // Main still holds its own view.
    assert_eq!(names(&c, "SELECT name FROM sqlite_master"), vec!["v"]);
    assert_eq!(names(&c, "SELECT name FROM sqlite_temp_master"), vec!["v"]);
}

#[test]
fn plain_view_stays_in_main() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIEW v AS SELECT 1 AS x").unwrap();
    assert_eq!(names(&c, "SELECT name FROM sqlite_master"), vec!["v"]);
    let temp = names(&c, "SELECT name FROM sqlite_temp_master");
    assert!(temp.is_empty(), "plain view leaked into temp: {temp:?}");
    let r = c.query("SELECT * FROM v").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn temp_trigger_lives_in_temp_master_and_fires_on_main_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("CREATE TABLE log(a)").unwrap();
    c.execute("CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (NEW.a); END")
        .unwrap();

    // The trigger is in temp, not main.
    assert_eq!(names(&c, "SELECT name FROM sqlite_temp_master"), vec!["tr"]);
    let main = names(&c, "SELECT name FROM sqlite_master");
    assert!(
        !main.iter().any(|n| n == "tr"),
        "trigger leaked into main: {main:?}"
    );

    // It still fires on a write to the main table.
    c.execute("INSERT INTO t VALUES (42)").unwrap();
    let r = c.query("SELECT a FROM log").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(42)]]);
}

#[test]
fn plain_trigger_stays_in_main() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("CREATE TABLE log(a)").unwrap();
    c.execute("CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (NEW.a); END")
        .unwrap();
    assert!(names(&c, "SELECT name FROM sqlite_master")
        .iter()
        .any(|n| n == "tr"));
    let temp = names(&c, "SELECT name FROM sqlite_temp_master");
    assert!(temp.is_empty(), "plain trigger leaked into temp: {temp:?}");
    c.execute("INSERT INTO t VALUES (7)").unwrap();
    assert_eq!(
        c.query("SELECT a FROM log").unwrap().rows,
        vec![vec![Value::Integer(7)]]
    );
}
