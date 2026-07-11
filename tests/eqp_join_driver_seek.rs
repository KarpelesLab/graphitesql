//! B1b driver-seek: when the driver (`from.first`) of an INNER join carries its own
//! `rowid = <const>` equality in the `WHERE`, sqlite drives the join by seeking that
//! one row — `SEARCH <driver> USING INTEGER PRIMARY KEY (rowid=?)` — instead of
//! scanning the whole table, and with only one driver row it *scans* an otherwise-
//! unindexed inner rather than building a transient automatic index. graphite now
//! matches both: the driver renders (and executes) as a rowid seek, and the inner's
//! `AUTOMATIC COVERING INDEX` label is suppressed to a plain `SCAN` (a real index on
//! the inner still renders `SEARCH … USING INDEX`). Verified differentially against
//! the `sqlite3` CLI (EQP) and for row equality.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, k, v); \
                     CREATE TABLE small(id INTEGER PRIMARY KEY, k, v); \
                     CREATE INDEX sk ON small(k); \
                     INSERT INTO big VALUES(5,1,'b5'),(7,2,'b7'),(9,2,'b9'); \
                     INSERT INTO small VALUES(1,2,'s1'),(2,2,'s2'),(3,1,'s3');";

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// EQP node details (tree markers and the `QUERY PLAN` header stripped).
fn sqlite_eqp(sql: &str) -> Vec<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{SETUP} EXPLAIN QUERY PLAN {sql};"))
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim_start_matches(['`', '|', '-', ' ']).to_string())
        .filter(|s| !s.is_empty() && s != "QUERY PLAN")
        .collect()
}

fn graphite_eqp(c: &Connection, sql: &str) -> Vec<String> {
    c.query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.last() {
            Some(Value::Text(t)) => t.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn seeded() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c
}

#[test]
fn driver_rowid_seek_eqp_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let c = seeded();
    for q in [
        // Unindexed inner join column → driver seeks, inner SCANs (no auto-index).
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7",
        // Indexed inner join column → driver seeks, inner uses the real index.
        "SELECT * FROM big JOIN small ON big.k=small.k WHERE big.id=7",
        // No driver seek → unchanged (auto-index + bloom filter still rendered).
        "SELECT * FROM big JOIN small ON big.k=small.v",
        // The reversed projection and an extra WHERE term still seek the driver.
        "SELECT small.v, big.v FROM big JOIN small ON big.k=small.v WHERE big.id=7 AND small.v>'a'",
    ] {
        assert_eq!(sqlite_eqp(q), graphite_eqp(&c, q), "EQP diverged on `{q}`");
    }
}

#[test]
fn driver_rowid_seek_rows_match_sqlite() {
    if !sqlite_available() {
        return;
    }
    let c = seeded();
    let q = "SELECT big.v, small.v FROM big JOIN small ON big.k=small.v WHERE big.id=7 \
             ORDER BY small.v";
    let got: Vec<Vec<String>> = c
        .query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Text(t) => t.clone(),
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    other => format!("{other:?}"),
                })
                .collect()
        })
        .collect();
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .args(["-separator", "|"])
        .arg(format!("{SETUP} {q};"))
        .output()
        .unwrap();
    let want: Vec<Vec<String>> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect();
    assert_eq!(got, want);
}
