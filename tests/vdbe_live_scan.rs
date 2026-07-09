//! B5b-2 / B8 (live storage cursor): a single-table `SELECT … FROM <one rowid
//! table> [WHERE …]` streams rows straight from a *live* b-tree `TableCursor`
//! (`Rewind`/`Column`/`Next` decoding one row at a time) instead of materializing
//! the whole table into a `Vec` before the VDBE runs. The row source is the only
//! thing that changes — projection, `WHERE`, `ORDER BY`, `LIMIT`/`OFFSET`,
//! `DISTINCT`, aggregates and `GROUP BY` are byte-identical to the materialized
//! path, so the result must equal (a) the tree-walker and (b) sqlite3 3.50.4.
//!
//! `query_vdbe` errors on any fallback to the tree-walker, so a passing
//! `query_vdbe` proves the query routed onto a VDBE path — and for a plain rowid
//! base table that path is the live cursor. Each query is checked three ways: it
//! routes on the VDBE, it equals the tree-walker (`set_use_vdbe(false)`), and it
//! equals the sqlite3 CLI (skipped when the CLI is absent). The schema mixes
//! every storage class (INTEGER/TEXT/REAL/BLOB/NULL), an explicit INTEGER PRIMARY
//! KEY, and non-contiguous rowids (an out-of-order / gapped insert plus a delete)
//! so the cursor's tree-order traversal, empty-leaf settling, and rowid decoding
//! are all exercised.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const SCHEMA: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, amt REAL, tag);\
     INSERT INTO t VALUES(5,'e',5.5,x'414243');\
     INSERT INTO t VALUES(1,'a',1.25,NULL);\
     INSERT INTO t VALUES(3,NULL,3.0,'three');\
     INSERT INTO t VALUES(2,'b',-2.5,x'ff00');\
     INSERT INTO t VALUES(9,'i',NULL,42);\
     INSERT INTO t VALUES(4,'d',4.0,'four');\
     INSERT INTO t VALUES(7,'g',7.75,7.75);\
     DELETE FROM t WHERE id=4;\
     INSERT INTO t VALUES(100,'big',1e100,x'');";

/// Queries over a single rowid base table that must route onto the live-cursor
/// scan path (`SELECT … FROM t [WHERE …]`, no join / subquery / ORDER-BY-by-scan).
const VDBE_QUERIES: &[&str] = &[
    // Full projection, column subsets, `*`, and the hidden rowid aliases.
    "SELECT id, name, amt, tag FROM t",
    "SELECT * FROM t",
    "SELECT name FROM t",
    "SELECT rowid, id FROM t",
    "SELECT _rowid_, oid FROM t",
    "SELECT t.* FROM t",
    "SELECT t.name, t.amt FROM t",
    // WHERE filters over each storage class, incl. NULL / blob / real.
    "SELECT id FROM t WHERE id > 3",
    "SELECT id, name FROM t WHERE name IS NULL",
    "SELECT id FROM t WHERE amt IS NOT NULL",
    "SELECT id FROM t WHERE amt < 0",
    "SELECT id FROM t WHERE tag = x'ff00'",
    "SELECT id FROM t WHERE id BETWEEN 2 AND 7",
    "SELECT id FROM t WHERE name IN ('a','g','zzz')",
    "SELECT id FROM t WHERE id % 2 = 1",
    // Computed projections + typeof to pin storage class through the live decode.
    "SELECT id, amt * 2 + 1 AS s FROM t WHERE amt IS NOT NULL",
    "SELECT typeof(tag), typeof(amt), typeof(name) FROM t WHERE id = 9",
    "SELECT id || ':' || coalesce(name,'?') FROM t",
    // LIMIT / OFFSET / DISTINCT.
    "SELECT id FROM t LIMIT 3",
    "SELECT id FROM t LIMIT 2 OFFSET 2",
    "SELECT DISTINCT (id % 3) AS m FROM t",
    "SELECT DISTINCT name IS NULL FROM t",
    // Aggregates (fold the whole scan into one row) and GROUP BY.
    "SELECT count(*) FROM t",
    "SELECT count(*), sum(id), avg(amt), min(id), max(id) FROM t",
    "SELECT count(name), count(amt), count(*) FROM t",
    "SELECT (id % 2) AS parity, count(*) FROM t GROUP BY id % 2",
    "SELECT name IS NULL AS n, count(*) FROM t GROUP BY name IS NULL",
    // A table alias — the qualifier still resolves.
    "SELECT z.id, z.name FROM t AS z WHERE z.id < 5",
    // No matching row (the cursor scans but nothing passes WHERE).
    "SELECT id FROM t WHERE id = 12345",
];

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SCHEMA.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

#[test]
fn live_scan_routes_and_matches_tree_walker() {
    let c = setup();
    for q in VDBE_QUERIES {
        // Routes onto the VDBE (errors on any fallback → proves the live path ran).
        let vdbe = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE routing for `{q}`: {e:?}"));
        // Tree-walker (source of truth).
        c.set_use_vdbe(false);
        let tw = c.query(q).unwrap();
        c.set_use_vdbe(true);
        assert_eq!(vdbe.rows, tw.rows, "VDBE vs tree-walker rows for `{q}`");
        assert_eq!(vdbe.columns, tw.columns, "columns for `{q}`");
    }
}

#[test]
fn live_scan_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    // Compare the graphite CLI binary (VDBE enabled by default, so the live-cursor
    // path runs) against sqlite3 end-to-end — float/blob/NULL rendering included,
    // so no hand-rolled value formatting can mask a divergence.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in VDBE_QUERIES {
        let full = format!("{SCHEMA} {q}");
        let sq = Command::new("sqlite3")
            .arg(":memory:")
            .arg(&full)
            .output()
            .unwrap();
        let gr = Command::new(g).arg(":memory:").arg(&full).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&sq.stdout),
            String::from_utf8_lossy(&gr.stdout),
            "sqlite3 vs graphite `{q}`"
        );
    }
}

#[test]
fn live_scan_empty_table_routes_and_matches() {
    // An empty table: `Rewind` immediately reports no row (jumps over the loop),
    // and an aggregate over it still emits its single all-NULL / zero row.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE e(a INTEGER PRIMARY KEY, b TEXT)")
        .unwrap();
    for q in [
        "SELECT a, b FROM e",
        "SELECT * FROM e WHERE a > 0",
        "SELECT count(*) FROM e",
        "SELECT count(*), sum(a), max(b) FROM e",
        "SELECT DISTINCT b FROM e",
    ] {
        let vdbe = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE routing for `{q}`: {e:?}"));
        c.set_use_vdbe(false);
        let tw = c.query(q).unwrap();
        c.set_use_vdbe(true);
        assert_eq!(vdbe.rows, tw.rows, "empty-table rows for `{q}`");
        assert_eq!(vdbe.columns, tw.columns, "empty-table columns for `{q}`");
    }
}

#[test]
fn live_scan_larger_table_matches_tree_walker() {
    // A row count that spans multiple b-tree leaf pages, so the cursor's
    // interior-page descent and leaf-to-leaf advance (not just a single leaf) are
    // exercised. Values are interleaved so the scan yields rowid order, not
    // insert order.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE big(k INTEGER PRIMARY KEY, v TEXT, w REAL)")
        .unwrap();
    for i in 0..500i64 {
        // Insert in a scrambled key order to force real b-tree structure.
        let k = (i * 137 + 11) % 500;
        c.execute(&format!(
            "INSERT INTO big VALUES({k}, 'row-{k}', {}.5)",
            k * 3
        ))
        .unwrap();
    }
    for q in [
        "SELECT k, v FROM big",
        "SELECT count(*), sum(k), avg(w) FROM big",
        "SELECT k FROM big WHERE k % 7 = 0",
        "SELECT k FROM big WHERE w > 500 LIMIT 10",
        "SELECT DISTINCT (k % 10) FROM big",
        "SELECT rowid FROM big WHERE k < 5",
    ] {
        let vdbe = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE routing for `{q}`: {e:?}"));
        c.set_use_vdbe(false);
        let tw = c.query(q).unwrap();
        c.set_use_vdbe(true);
        assert_eq!(vdbe.rows, tw.rows, "large-table rows for `{q}`");
        assert_eq!(vdbe.columns, tw.columns, "large-table columns for `{q}`");
    }
}
