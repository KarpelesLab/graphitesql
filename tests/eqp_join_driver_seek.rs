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

// No index on `big.k` — so a `big.k = small.<col>` join does not trigger the
// cost-based index-inner swap, and a `WHERE big.id = <const>` cleanly drives the
// rowid seek (the secondary-index cases use `SETUP_IDX` below, which adds `bk`).
const SETUP: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, k, v); \
                     CREATE TABLE small(id INTEGER PRIMARY KEY, k, v); \
                     CREATE INDEX sk ON small(k); \
                     INSERT INTO big VALUES(5,1,'b5'),(7,2,'b7'),(9,2,'b9'); \
                     INSERT INTO small VALUES(1,2,'s1'),(2,2,'s2'),(3,1,'s3');";

// Adds a single-column secondary index on `big.k` for the secondary-index driver
// seek. The join columns here are chosen so no rowid/index swap competes.
const SETUP_IDX: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, k, v); \
                         CREATE INDEX bk ON big(k); \
                         CREATE TABLE small(id INTEGER PRIMARY KEY, k, v); \
                         INSERT INTO big VALUES(5,1,'b5'),(7,2,'b7'),(9,2,'b9'); \
                         INSERT INTO small VALUES(1,0,'b7'),(2,0,'b9'),(3,0,'zz');";

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// EQP node details (tree markers and the `QUERY PLAN` header stripped).
fn sqlite_eqp(setup: &str, sql: &str) -> Vec<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{setup} EXPLAIN QUERY PLAN {sql};"))
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

fn seeded(setup: &str) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in setup.split(';') {
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
    let c = seeded(SETUP);
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
        assert_eq!(
            sqlite_eqp(SETUP, q),
            graphite_eqp(&c, q),
            "EQP diverged on `{q}`"
        );
    }
}

#[test]
fn rowid_seek_beats_index_inner_swap() {
    if !sqlite_available() {
        return;
    }
    // `bk` on `big.k` makes `big.k = small.v` index-inner-swappable (drive small,
    // seek big by `bk`), but the driver's own `big.id = 7` is a single-row rowid
    // seek — the more selective plan — so sqlite (and now graphite) drives that
    // instead: `SEARCH big USING INTEGER PRIMARY KEY (rowid=?)` + `SCAN small`.
    let c = seeded(SETUP_IDX);
    for q in [
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7",
        // `ORDER BY` on the driver's rowid is still satisfied without a temp b-tree.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY big.id",
    ] {
        assert_eq!(
            sqlite_eqp(SETUP_IDX, q),
            graphite_eqp(&c, q),
            "EQP diverged on `{q}`"
        );
    }
    // Rows are correct regardless of the chosen plan.
    let rows = c
        .query("SELECT big.id FROM big JOIN small ON big.k=small.v WHERE big.id=7")
        .unwrap()
        .rows;
    for r in &rows {
        assert_eq!(r[0], Value::Integer(7));
    }
}

#[test]
fn driver_secondary_index_seek_eqp_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    // `bk` is a single-column secondary index on `big.k`; the join is on `v` so no
    // rowid/index swap competes with the `big.k = <const>` driver seek.
    let c = seeded(SETUP_IDX);
    for q in [
        // Secondary-index driver seek; the unindexed inner scans (no auto-index).
        "SELECT * FROM big JOIN small ON big.v=small.v WHERE big.k=2",
        // A driver-only extra predicate still seeks the index.
        "SELECT big.v, small.v FROM big JOIN small ON big.v=small.v WHERE big.k=2 AND big.v>'a'",
    ] {
        assert_eq!(
            sqlite_eqp(SETUP_IDX, q),
            graphite_eqp(&c, q),
            "EQP diverged on `{q}`"
        );
    }
}

#[test]
fn secondary_index_driver_seek_rows_match_sqlite() {
    if !sqlite_available() {
        return;
    }
    let c = seeded(SETUP_IDX);
    let q = "SELECT big.v, small.v FROM big JOIN small ON big.v=small.v WHERE big.k=2 \
             ORDER BY big.v, small.v";
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
        .arg(format!("{SETUP_IDX} {q};"))
        .output()
        .unwrap();
    let want: Vec<Vec<String>> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect();
    assert_eq!(got, want);
}

#[test]
fn driver_rowid_seek_rows_match_sqlite() {
    if !sqlite_available() {
        return;
    }
    let c = seeded(SETUP);
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
