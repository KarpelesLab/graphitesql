//! A `col = <const>` WHERE equality pins that column to a constant, so sqlite drops
//! a *leading* `ORDER BY` term on it and sorts only the remaining terms —
//! `USE TEMP B-TREE FOR LAST [N TERMS OF] ORDER BY` (or none, when every term is
//! constant/ordered). graphite previously sorted the whole `ORDER BY`. Now the
//! `seek_order_prefix` order-credit also counts equality-constant leading terms, so
//! the temp-b-tree node matches sqlite. Verified differentially against the sqlite3
//! CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, x, y, z); \
                     CREATE INDEX tx ON t(x); \
                     INSERT INTO t VALUES(1,5,3,'a'),(2,5,4,'b'),(3,7,1,'c');";

// Same data but NO index on `x` — the equality `WHERE x = <const>` runs as a plain
// SCAN, exercising the scan-path constant credit (`order_const_lead`).
const SETUP_SCAN: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, x, y, z); \
                          INSERT INTO t VALUES(1,5,3,'a'),(2,5,4,'b'),(3,7,1,'c');";

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite_eqp_setup(setup: &str, sql: &str) -> Vec<String> {
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
            Some(Value::Text(t)) => String::from(t.as_str()),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn constant_leading_order_by_terms_are_dropped_like_sqlite() {
    if !sqlite_available() {
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        // `x` constant → dropped; only `y` (and `z`) need the temp b-tree.
        "SELECT * FROM t WHERE x=5 ORDER BY x, y",
        "SELECT * FROM t WHERE x=5 ORDER BY x, y, z",
        // Two constants dropped → only `z` sorted.
        "SELECT * FROM t WHERE x=5 AND y=3 ORDER BY x, y, z",
        // Every term constant/ordered → no temp b-tree at all.
        "SELECT * FROM t WHERE x=5 ORDER BY x",
        "SELECT * FROM t WHERE x=5 AND y=3 ORDER BY x, y",
        // A *trailing* constant after a non-constant leading term does NOT shrink the
        // sort (the leading term must be satisfied first) → whole ORDER BY sorted.
        "SELECT * FROM t WHERE x=5 ORDER BY y, x",
        // Regression guards: a range seek (walk-ordered, not constant), a DESC term,
        // a no-WHERE scan, and an ORDER BY on a non-constant column only.
        "SELECT * FROM t WHERE x>1 ORDER BY x, y",
        "SELECT * FROM t WHERE x=5 ORDER BY x DESC, y",
        "SELECT * FROM t ORDER BY x, y",
        "SELECT * FROM t WHERE x=5 ORDER BY y",
    ] {
        assert_eq!(sqlite_eqp(q), graphite_eqp(&c, q), "EQP diverged on `{q}`");
    }
}

#[test]
fn constant_terms_dropped_on_a_plain_scan_too() {
    if !sqlite_available() {
        return;
    }
    // No index on `x`, so `WHERE x = <const>` runs as a plain SCAN — the constant
    // credit must still drop the leading `x`.
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP_SCAN.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        "SELECT * FROM t WHERE x=5 ORDER BY x, y",
        "SELECT * FROM t WHERE x=5 AND y=3 ORDER BY x, y, z",
        "SELECT * FROM t WHERE x=5 ORDER BY x",
        // A non-constant leading term still sorts the whole clause.
        "SELECT * FROM t WHERE x=5 ORDER BY y, x",
        "SELECT * FROM t WHERE x=5 ORDER BY y",
        "SELECT * FROM t ORDER BY x, y",
    ] {
        assert_eq!(
            sqlite_eqp_setup(SETUP_SCAN, q),
            graphite_eqp(&c, q),
            "EQP diverged on `{q}`"
        );
    }
}

#[test]
fn single_row_driver_join_inner_rowid_with_unrelated_index() {
    if !sqlite_available() {
        return;
    }
    // `small` has an index on `k`, but the join is on `v` (unindexed) — so `small` is
    // still scanned in rowid order, and `ORDER BY small.id` needs no sort. The index
    // being unrelated to the join must not defeat the credit.
    const S: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, k, v); \
                     CREATE TABLE small(id INTEGER PRIMARY KEY, k, v); \
                     CREATE INDEX sk ON small(k); \
                     INSERT INTO big VALUES(7,2,'b7'); \
                     INSERT INTO small VALUES(1,8,2),(2,7,2),(3,6,1);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    let eqp = |sql: &str| -> Vec<String> {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!("{S} EXPLAIN QUERY PLAN {sql};"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| l.trim_start_matches(['`', '|', '-', ' ']).to_string())
            .filter(|s| !s.is_empty() && s != "QUERY PLAN")
            .collect()
    };
    let q = "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.id";
    assert_eq!(eqp(q), graphite_eqp(&c, q), "EQP diverged on `{q}`");
    // Rows come out ascending by small.id despite the skipped sort.
    let ids: Vec<i64> = c
        .query(
            "SELECT small.id FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.id",
        )
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn single_row_driver_join_all_constant_order_by() {
    if !sqlite_available() {
        return;
    }
    // `big.id=7` is a single-row rowid seek, so `big.*` is constant and `small.v`
    // (equated to `big.k` by the ON) is constant too — an ORDER BY on any of those
    // needs no sort. A non-constant inner column (`small.k`) still sorts.
    // `small.v` is numeric so `big.k = small.v` actually joins (big.id=7 → k=2 →
    // matches small rows 1 and 2, whose v=2); `small.k` varies so it is genuinely
    // non-constant.
    const S: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, k, v); \
                     CREATE TABLE small(id INTEGER PRIMARY KEY, k, v); \
                     INSERT INTO big VALUES(5,1,'b5'),(7,2,'b7'),(9,2,'b9'); \
                     INSERT INTO small VALUES(1,8,2),(2,7,2),(3,6,1);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    let eqp = |sql: &str| -> Vec<String> {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!("{S} EXPLAIN QUERY PLAN {sql};"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| l.trim_start_matches(['`', '|', '-', ' ']).to_string())
            .filter(|s| !s.is_empty() && s != "QUERY PLAN")
            .collect()
    };
    for q in [
        // Join-equated inner column → constant → no sort.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.v",
        // Driver column → constant → no sort.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY big.v",
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY big.v, small.v",
        // A non-constant inner column still needs the sort.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.k",
        // The inner's own rowid (ascending) is satisfied by the inner's plain
        // rowid-order scan (small has no secondary index) — no sort.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.id",
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.v, small.id",
    ] {
        assert_eq!(eqp(q), graphite_eqp(&c, q), "EQP diverged on `{q}`");
    }
    // The rows (as a set) are unchanged when the sort is skipped.
    let mut got: Vec<i64> = c
        .query("SELECT small.id FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY big.v")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);

    // The inner-rowid ORDER BY skips the sort, but the inner's plain rowid scan
    // already yields ascending small.id — so the rows come out ordered.
    let ordered: Vec<i64> = c
        .query(
            "SELECT small.id FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.id",
        )
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(ordered, vec![1, 2]);
}

#[test]
fn single_row_driver_join_on_indexed_inner_column() {
    if !sqlite_available() {
        return;
    }
    // The join is ON an *indexed* inner column. For the single driver row, `small` is
    // seeked for one key value. A SINGLE-column index over that column yields rowid
    // order (all matches share the key, tie-broken by rowid) → `ORDER BY small.id`
    // needs no sort, covering or not. A MULTI-column index orders by its key suffix →
    // it still sorts. `w` is anti-correlated with the rowid so the key order (3,1,2)
    // and rowid order (1,2,3) genuinely differ — the multi-col case must really sort.
    const SINGLE: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, v); \
                          CREATE TABLE small(id INTEGER PRIMARY KEY, k, w); \
                          CREATE INDEX sk ON small(k); \
                          INSERT INTO big VALUES(7,5); \
                          INSERT INTO small VALUES(3,5,'a'),(1,5,'b'),(2,5,'c'),(4,9,'z');";
    const MULTI: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, v); \
                         CREATE TABLE small(id INTEGER PRIMARY KEY, k, w); \
                         CREATE INDEX skw ON small(k,w); \
                         INSERT INTO big VALUES(7,5); \
                         INSERT INTO small VALUES(3,5,'a'),(1,5,'b'),(2,5,'c'),(4,9,'z');";
    let open = |setup: &str| {
        let mut c = Connection::open_memory().unwrap();
        for stmt in setup.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                c.execute(s).unwrap();
            }
        }
        c
    };
    // Single-column index: covering (SELECT small.id) and non-covering (SELECT
    // small.w) projections both skip the sort, exactly like sqlite.
    let cs = open(SINGLE);
    for q in [
        "SELECT small.id FROM big JOIN small ON big.v=small.k WHERE big.id=7 ORDER BY small.id",
        "SELECT small.id, small.w FROM big JOIN small ON big.v=small.k WHERE big.id=7 ORDER BY small.id",
    ] {
        assert_eq!(
            sqlite_eqp_setup(SINGLE, q),
            graphite_eqp(&cs, q),
            "EQP diverged on `{q}`"
        );
    }
    // Multi-column index over the join column: sqlite (and now graphite) keep the sort.
    let cm = open(MULTI);
    let q = "SELECT small.id FROM big JOIN small ON big.v=small.k WHERE big.id=7 ORDER BY small.id";
    assert_eq!(
        sqlite_eqp_setup(MULTI, q),
        graphite_eqp(&cm, q),
        "EQP diverged on `{q}`"
    );

    // Rows come out ascending by small.id in both — the single-col case is already
    // rowid-ordered (sort skipped), the multi-col case genuinely sorts (its key order
    // 3,1,2 differs from rowid order 1,2,3).
    let ids = |c: &Connection| -> Vec<i64> {
        c.query(
            "SELECT small.id FROM big JOIN small ON big.v=small.k WHERE big.id=7 ORDER BY small.id",
        )
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect()
    };
    assert_eq!(ids(&cs), vec![1, 2, 3]);
    assert_eq!(ids(&cm), vec![1, 2, 3]);
}

#[test]
fn full_unique_index_equality_matches_one_row_no_sort() {
    if !sqlite_available() {
        return;
    }
    // A `WHERE` equality covering *every* column of a non-partial UNIQUE index (or
    // the full composite key) matches at most one row, so ANY `ORDER BY` is vacuous
    // and sqlite drops the temp b-tree. A non-unique index, a partial composite key,
    // or `IS NULL` (NULLs are not unique) all still sort.
    const S: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c, d); \
                     CREATE UNIQUE INDEX tb ON t(b); \
                     CREATE INDEX tc ON t(c); \
                     CREATE UNIQUE INDEX tcd ON t(c,d); \
                     INSERT INTO t VALUES(1,10,'x',5),(2,20,'y',6),(3,30,'z',7);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    // Compare only the sort (`USE TEMP B-TREE`) lines — this feature controls the
    // sort, not which index the seek picks (index choice is an orthogonal, separately
    // tested cost-model concern that can legitimately differ here).
    let sort_lines = |eqp: &[String]| -> Vec<String> {
        eqp.iter()
            .filter(|l| l.contains("TEMP B-TREE"))
            .cloned()
            .collect::<Vec<_>>()
    };
    for q in [
        // Full single-column UNIQUE key pinned → one row → no sort (asc, desc, multi-term).
        "SELECT * FROM t WHERE b=20 ORDER BY c",
        "SELECT * FROM t WHERE b=20 ORDER BY c DESC",
        "SELECT * FROM t WHERE b=20 ORDER BY c, a, d",
        // A UNIQUE column pinned alongside extra constraints → still one row.
        "SELECT * FROM t WHERE b=20 AND c='y' ORDER BY d",
        // Full composite UNIQUE key (c,d) pinned → one row.
        "SELECT * FROM t WHERE c='y' AND d=6 ORDER BY b",
        // Regression guards that must STILL sort:
        //  * a non-unique / partial-key equality (only the leading `tcd` column
        //    pinned, and `tc` is non-unique) → many rows possible,
        "SELECT * FROM t WHERE c='y' ORDER BY b",
        //  * `IS NULL` on the UNIQUE column (NULLs are not unique).
        "SELECT * FROM t WHERE b IS NULL ORDER BY c",
    ] {
        assert_eq!(
            sort_lines(&sqlite_eqp_setup(S, q)),
            sort_lines(&graphite_eqp(&c, q)),
            "sort node diverged on `{q}`"
        );
    }
    // The single matched row is returned correctly with the sort skipped.
    let row: Vec<String> = c
        .query("SELECT c FROM t WHERE b=20 ORDER BY c, a, d")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(t) => String::from(t.as_str()),
            v => format!("{v:?}"),
        })
        .collect();
    assert_eq!(row, vec!["y".to_string()]);
}

#[test]
fn dropped_constant_term_still_returns_correct_rows() {
    if !sqlite_available() {
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    // `WHERE x=5 ORDER BY x, y` must still come out sorted by `y` (x is the constant 5).
    let ys: Vec<i64> = c
        .query("SELECT y FROM t WHERE x=5 ORDER BY x, y")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(ys, vec![3, 4]);
}
