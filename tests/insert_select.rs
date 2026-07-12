//! `INSERT INTO … SELECT …` — populate a table from a query. The SELECT is
//! evaluated to a snapshot first (so a self-insert terminates), then each row
//! flows through the ordinary insert path (defaults, constraints, triggers,
//! indexes). Matched against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => panic!("not an int"),
        })
        .collect()
}

#[test]
fn basic_insert_select() {
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE s(a INT, b TEXT)",
        "INSERT INTO s VALUES(1,'x'),(2,'y'),(3,'z')",
        "CREATE TABLE t(a INT, b TEXT)",
    ] {
        c.execute(s).unwrap();
    }
    assert_eq!(
        c.execute("INSERT INTO t SELECT a, b FROM s WHERE a < 3")
            .unwrap(),
        2
    );
    assert_eq!(ints(&c, "SELECT a FROM t ORDER BY a"), vec![1, 2]);
}

#[test]
fn insert_select_with_column_list_and_exprs() {
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE s(a INT, b TEXT)",
        "INSERT INTO s VALUES(3,'z')",
        "CREATE TABLE t(a INT, b TEXT)",
    ] {
        c.execute(s).unwrap();
    }
    // Target column list reorders, and SELECT may compute expressions.
    c.execute("INSERT INTO t(b, a) SELECT upper(b), a * 10 FROM s")
        .unwrap();
    let r = c.query("SELECT a, b FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(30));
    assert_eq!(r.rows[0][1], Value::Text("Z".into()));
}

#[test]
fn self_insert_uses_a_snapshot() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    // Reads the pre-insert 3 rows, not the rows it is adding — terminates.
    assert_eq!(c.execute("INSERT INTO t SELECT a FROM t").unwrap(), 3);
    assert_eq!(ints(&c, "SELECT count(*) FROM t"), vec![6]);
}

#[test]
fn insert_select_fires_triggers_and_defaults() {
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE s(a INT)",
        "INSERT INTO s VALUES(1),(3)",
        "CREATE TABLE t(a INT, log TEXT DEFAULT 'ins')",
        "CREATE TABLE audit(x INT)",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO audit VALUES(NEW.a); END",
    ] {
        c.execute(s).unwrap();
    }
    c.execute("INSERT INTO t(a) SELECT a FROM s").unwrap();
    // DEFAULT filled, and the AFTER trigger ran once per inserted row.
    assert_eq!(
        c.query("SELECT log FROM t LIMIT 1").unwrap().rows[0][0],
        Value::Text("ins".into())
    );
    assert_eq!(ints(&c, "SELECT x FROM audit ORDER BY x"), vec![1, 3]);
}

#[test]
fn count_mismatch_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE s(a INT, b INT)").unwrap();
    c.execute("INSERT INTO s VALUES(1, 2)").unwrap();
    c.execute("CREATE TABLE t(a INT, b INT)").unwrap();
    assert!(c.execute("INSERT INTO t SELECT a FROM s").is_err());
    // The same exact-count rule applies to a bare VALUES list with implicit cols.
    assert!(c.execute("INSERT INTO t VALUES(1)").is_err());
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let script = "CREATE TABLE s(a,b); INSERT INTO s VALUES(1,'x'),(2,'y'); \
                  CREATE TABLE t(a,b); INSERT INTO t SELECT b, a FROM s ORDER BY a; \
                  SELECT a||'/'||b FROM t ORDER BY a;";
    let want = {
        let o = Command::new("sqlite3")
            .arg(":memory:")
            .arg(script)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE s(a,b)",
        "INSERT INTO s VALUES(1,'x'),(2,'y')",
        "CREATE TABLE t(a,b)",
        "INSERT INTO t SELECT b, a FROM s ORDER BY a",
    ] {
        c.execute(s).unwrap();
    }
    let got: Vec<String> = c
        .query("SELECT a||'/'||b FROM t ORDER BY a")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(t) => String::from(t.as_str()),
            _ => String::new(),
        })
        .collect();
    assert_eq!(got.join("\n"), want);
}

/// `INSERT … SELECT … RETURNING …` — a `RETURNING` clause following a `SELECT`
/// data source (rather than `VALUES`). `RETURNING` is a reserved word, so the
/// SELECT-source parse must stop at it and let the INSERT consume it; previously
/// `opt_alias` tried to read `RETURNING` as an implicit column/table alias and
/// the parse failed at `near "RETURNING"`. Verified against the sqlite3 CLI,
/// including a leading `WITH`, an `ORDER BY`/`LIMIT` and `UNION` in the source,
/// and that a bare `returning` used as an alias still errors identically.
#[test]
fn insert_select_returning_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let out = |bin: &str, sql: &str| -> String {
        let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        s.trim_end().to_string()
    };
    let base = "CREATE TABLE u(a INTEGER PRIMARY KEY, b); \
                CREATE TABLE t(a, b); INSERT INTO t VALUES(10,'x'),(20,'y');";
    for sql in [
        "INSERT INTO u SELECT * FROM t RETURNING *;",
        "INSERT INTO u SELECT * FROM t RETURNING a, b, a*2;",
        "INSERT INTO u(b) SELECT b FROM t WHERE a>10 RETURNING a, b;",
        "INSERT INTO u SELECT * FROM t ORDER BY a DESC LIMIT 1 RETURNING b AS bee;",
        "WITH s AS (SELECT * FROM t) INSERT INTO u SELECT * FROM s RETURNING rowid, *;",
        "INSERT INTO u SELECT * FROM (SELECT * FROM t) RETURNING a;",
        // `RETURNING` is reserved: a bare/aliased use still errors identically.
        "INSERT INTO u SELECT * FROM t RETURNING nosuchcol;",
        "SELECT 2 returning;",
        "SELECT 2 AS returning;",
    ] {
        let full = format!("{base} {sql}");
        assert_eq!(out("sqlite3", &full), out(g, &full), "for {sql}");
    }
}
