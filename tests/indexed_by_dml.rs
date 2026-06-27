//! `UPDATE`/`DELETE` accept the same `INDEXED BY name` / `NOT INDEXED` planner
//! hint as `SELECT`. The hint never changes the statement's result — it only
//! constrains the planner — so the sole observable effect is a prepare-time
//! `no such index: name` when the named index does not exist on the target
//! table (case-insensitively). graphite previously rejected the syntax outright
//! with `near "INDEXED": syntax error`. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn indexed_by_hint_runs_and_is_a_noop_on_results() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE INDEX ix ON t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 10), (2, 20), (3, 30)")
        .unwrap();

    // INDEXED BY an existing index, NOT INDEXED, and a case-folded name all run
    // and affect exactly the rows the WHERE selects.
    c.execute("UPDATE t INDEXED BY ix SET b = 99 WHERE a = 1")
        .unwrap();
    c.execute("UPDATE t NOT INDEXED SET b = 88 WHERE a = 2")
        .unwrap();
    c.execute("DELETE FROM t INDEXED BY IX WHERE a = 3")
        .unwrap();

    let rows = c.query("SELECT a, b FROM t ORDER BY a").unwrap();
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[0][1], Value::Integer(99));
    assert_eq!(rows.rows[1][1], Value::Integer(88));
}

#[test]
fn indexed_by_unknown_index_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE INDEX ix ON t(a)").unwrap();
    c.execute("CREATE TABLE u(x)").unwrap();
    c.execute("CREATE INDEX ux ON u(x)").unwrap();

    for sql in [
        "UPDATE t INDEXED BY nope SET b = 1",
        "DELETE FROM t INDEXED BY nope",
        // An index that exists, but on a different table, is still unknown here.
        "UPDATE t INDEXED BY ux SET b = 1",
        "DELETE FROM t INDEXED BY ux",
    ] {
        let e = c.execute(sql).unwrap_err().to_string();
        assert!(e.contains("no such index"), "for {sql}: {e}");
    }
}

#[test]
fn unknown_index_outranks_unknown_column() {
    // sqlite resolves the index hint before the WHERE columns, so a statement
    // with both a bad index and a bad column reports the index first.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    let e = c
        .execute("DELETE FROM t INDEXED BY nope WHERE zz = 1")
        .unwrap_err()
        .to_string();
    assert!(e.contains("no such index: nope"), "{e}");
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
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    let b = "CREATE TABLE t(a,b); CREATE INDEX ix ON t(a); \
             CREATE TABLE u(x); CREATE INDEX ux ON u(x); \
             INSERT INTO t VALUES(1,2),(3,4);";
    for tail in [
        // Existing index / NOT INDEXED / case-folded name: run, identical rows.
        "DELETE FROM t INDEXED BY ix WHERE a=1; SELECT count(*) FROM t",
        "DELETE FROM t NOT INDEXED WHERE a=1; SELECT count(*) FROM t",
        "DELETE FROM t INDEXED BY IX WHERE a=1; SELECT count(*) FROM t",
        "UPDATE t INDEXED BY ix SET b=9 WHERE a=1; SELECT b FROM t WHERE a=1",
        "UPDATE t NOT INDEXED SET b=9 WHERE a=1; SELECT b FROM t WHERE a=1",
        // Unknown / wrong-table index: prepare-time error.
        "DELETE FROM t INDEXED BY nope WHERE a=1",
        "DELETE FROM t INDEXED BY ux WHERE a=1",
        "UPDATE t INDEXED BY nope SET b=9 WHERE a=1",
        "UPDATE t INDEXED BY ux SET b=9 WHERE a=1",
        // Ordering: bad index outranks a bad column; bad column still reported
        // when the index is fine.
        "DELETE FROM t INDEXED BY nope WHERE zz=1",
        "UPDATE t INDEXED BY ix SET z=9 WHERE a=1",
    ] {
        let sql = format!("{b} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {sql}");
    }
}
