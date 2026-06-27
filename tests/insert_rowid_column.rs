//! `rowid` / `_rowid_` / `oid` may be named in an INSERT column list to supply
//! the rowid explicitly, exactly as SQLite allows — provided no real column
//! shadows the name and the table is a rowid table (not WITHOUT ROWID). The
//! supplied value gets INTEGER affinity then an integer check: `'5'` and `5.0`
//! become `5`, `NULL` means "auto-assign", and `1.5` / `'x'` / a blob are a
//! `datatype mismatch`. When the table has an INTEGER PRIMARY KEY the alias
//! targets that column (so a later real-column value wins, "last write"); a
//! real column literally named `rowid` shadows the pseudo-column. graphite
//! previously rejected `rowid` as "table … has no column named rowid".
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &mut Connection, sql: &str) -> String {
    c.execute(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn rowid_alias_as_insert_column_sets_the_rowid() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t(rowid, a, b) VALUES (5, 10, 20)")
        .unwrap();
    c.execute("INSERT INTO t(_rowid_, a) VALUES (7, 1)")
        .unwrap();
    c.execute("INSERT INTO t(oid, a) VALUES (9, 2)").unwrap();
    let rows = c
        .query("SELECT rowid, a FROM t ORDER BY rowid")
        .unwrap()
        .rows;
    let got: Vec<(i64, Value)> = rows
        .into_iter()
        .map(|mut r| {
            let a = r.remove(1);
            match r.remove(0) {
                Value::Integer(i) => (i, a),
                other => panic!("rowid not integer: {other:?}"),
            }
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (5, Value::Integer(10)),
            (7, Value::Integer(1)),
            (9, Value::Integer(2)),
        ]
    );
}

#[test]
fn rowid_value_is_coerced_then_integer_checked() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    // INTEGER affinity: numeric text and integer-valued real collapse to an int.
    c.execute("INSERT INTO t(rowid, a) VALUES ('5', 1)")
        .unwrap();
    c.execute("INSERT INTO t(rowid, a) VALUES (6.0, 2)")
        .unwrap();
    let ids: Vec<Value> = c
        .query("SELECT rowid FROM t ORDER BY rowid")
        .unwrap()
        .rows
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect();
    assert_eq!(ids, vec![Value::Integer(5), Value::Integer(6)]);

    // A NULL rowid means auto-assign (not an error).
    c.execute("INSERT INTO t(rowid, a) VALUES (NULL, 3)")
        .unwrap();

    // Non-integer values are a datatype mismatch.
    for v in ["1.5", "'x'", "x'00'"] {
        let sql = format!("INSERT INTO t(rowid, a) VALUES ({v}, 9)");
        assert_eq!(err(&mut c, &sql), "datatype mismatch", "for {sql}");
    }
}

#[test]
fn rowid_alias_targets_an_integer_primary_key() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    // The alias resolves to the IPK column.
    c.execute("INSERT INTO t(rowid, b) VALUES (3, 9)").unwrap();
    // Duplicate rowid + real IPK column: the real column value wins ("last").
    c.execute("INSERT INTO t(rowid, a, b) VALUES (1, 2, 3)")
        .unwrap();
    let rows = c.query("SELECT a, b FROM t ORDER BY a").unwrap().rows;
    let got: Vec<(Value, Value)> = rows
        .into_iter()
        .map(|mut r| (r.remove(0), r.remove(0)))
        .collect();
    assert_eq!(
        got,
        vec![
            (Value::Integer(2), Value::Integer(3)),
            (Value::Integer(3), Value::Integer(9)),
        ]
    );
}

#[test]
fn a_real_column_named_rowid_shadows_the_pseudo_column() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(rowid, a)").unwrap();
    c.execute("INSERT INTO t(rowid, a) VALUES (5, 1)").unwrap();
    // The stored `rowid` column holds 5; the true rowid auto-assigned to 1.
    let mut row = c.query("SELECT rowid, a FROM t").unwrap().rows.remove(0);
    assert_eq!(row.remove(0), Value::Integer(5));
    assert_eq!(row.remove(0), Value::Integer(1));
}

#[test]
fn rowid_is_rejected_on_a_without_rowid_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID")
        .unwrap();
    let e = err(&mut c, "INSERT INTO t(rowid, a, b) VALUES (1, 2, 3)");
    assert_eq!(e, "table t has no column named rowid");
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        let mut line = String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string();
        // A step-time CLI error carries a trailing result code, e.g. `… (20)`.
        if let Some(open) = line.rfind(" (") {
            if line.ends_with(')')
                && line[open + 2..line.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit())
            {
                line.truncate(open);
            }
        }
        line
    };
    for sql in [
        "CREATE TABLE t(a,b); INSERT INTO t(rowid,a,b) VALUES(5,10,20); SELECT rowid,a,b FROM t;",
        "CREATE TABLE t(a,b); INSERT INTO t(_rowid_,a) VALUES(7,1); SELECT rowid,a FROM t;",
        "CREATE TABLE t(a,b); INSERT INTO t(oid,a) VALUES(9,1); SELECT rowid,a FROM t;",
        "CREATE TABLE t(a); INSERT INTO t(rowid,a) VALUES('5',1); SELECT rowid FROM t;",
        "CREATE TABLE t(a); INSERT INTO t(rowid,a) VALUES(5.0,1); SELECT rowid FROM t;",
        "CREATE TABLE t(a); INSERT INTO t(rowid,a) VALUES(1.5,1);",
        "CREATE TABLE t(a); INSERT INTO t(rowid,a) VALUES('x',1);",
        "CREATE TABLE t(a); INSERT INTO t(rowid,a) VALUES(x'00',1);",
        "CREATE TABLE t(a); INSERT INTO t(rowid,a) VALUES(NULL,1); SELECT typeof(rowid) FROM t;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t(rowid,b) VALUES(3,9); SELECT a,rowid,b FROM t;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t(rowid,a,b) VALUES(1,2,3); SELECT a,b FROM t;",
        "CREATE TABLE t(rowid,a); INSERT INTO t(rowid,a) VALUES(5,1); SELECT rowid,a FROM t;",
        "CREATE TABLE t(a); INSERT INTO t(rowid,a) VALUES(2,1); REPLACE INTO t(rowid,a) VALUES(2,9); SELECT rowid,a FROM t;",
        "CREATE TABLE t(a); INSERT INTO t(rowid,a) SELECT 4,8; SELECT rowid,a FROM t;",
        "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t(rowid,a,b) VALUES(1,2,3);",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
