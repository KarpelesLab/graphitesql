//! B9h sort-avoidance: a single-table query whose `WHERE` is not served by any
//! index but whose `ORDER BY` is, walks the ORDER-BY index to avoid a temp-b-tree
//! sort — `SCAN t USING INDEX i_b`, matching sqlite, instead of
//! `SCAN t` + `USE TEMP B-TREE FOR ORDER BY`. When the `WHERE` *is* served by a
//! seek index, that seek is planned instead (the sort stays), also matching
//! sqlite. Verified differentially against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "CREATE TABLE t(a, b, c); CREATE INDEX i_b ON t(b); \
                     INSERT INTO t VALUES(1,3,'x'),(2,1,'y'),(3,2,'z'),(4,1,'w');";

fn graphite_eqp(sql: &str) -> Vec<String> {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c.query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.last() {
            Some(graphitesql::Value::Text(t)) => t.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn sqlite_eqp(sql: &str) -> Option<Vec<String>> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{SETUP} EXPLAIN QUERY PLAN {sql};"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            // sqlite renders EQP as a tree; strip the leading `|--` / `` `-- `` /
            // indentation markers to leave just each node's detail, and drop the
            // `QUERY PLAN` header (graphite's `query()` returns the bare details).
            .map(|l| l.trim_start_matches(['`', '|', '-', ' ']).to_string())
            .filter(|s| !s.is_empty() && s != "QUERY PLAN")
            .collect(),
    )
}

#[test]
fn order_by_index_avoids_sort_matching_sqlite() {
    if sqlite_eqp("SELECT 1").is_none() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Each query is `WHERE <not-indexed predicate> ORDER BY <indexed>` (sort
    // avoided via the index), a seek predicate (seek used, sort kept), or a
    // not-order-indexable ORDER BY (sort kept) — the graphite EQP must equal
    // sqlite's in every case.
    for q in [
        "SELECT * FROM t WHERE a>0 ORDER BY b",
        "SELECT * FROM t WHERE c='x' ORDER BY b",
        "SELECT a FROM t WHERE a>0 ORDER BY b",
        "SELECT * FROM t WHERE b=2 ORDER BY b",
        "SELECT * FROM t ORDER BY b",
        "SELECT * FROM t WHERE a>0 ORDER BY c",
        "SELECT * FROM t WHERE a>0 ORDER BY b DESC",
    ] {
        let want = sqlite_eqp(q).unwrap();
        let got = graphite_eqp(q);
        assert_eq!(got, want, "EQP diverged from sqlite on `{q}`");
    }
}

#[test]
fn order_by_collate_uses_matching_collation_index() {
    if sqlite_eqp("SELECT 1").is_none() {
        return;
    }
    // `ORDER BY b COLLATE NOCASE` walks the NOCASE index (`ib`), while a plain
    // `ORDER BY b` uses the BINARY index (`ib2`) — an index serves the term only
    // when its stored collation equals the term's effective collation (B9j).
    const S: &str = "CREATE TABLE t(a, b TEXT); \
                     CREATE INDEX ib ON t(b COLLATE NOCASE); CREATE INDEX ib2 ON t(b); \
                     INSERT INTO t VALUES(1,'Apple'),(2,'banana'),(3,'CHERRY');";
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
    let graphite = |sql: &str| -> Vec<String> {
        let mut c = Connection::open_memory().unwrap();
        for stmt in S.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                c.execute(s).unwrap();
            }
        }
        c.query(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .rows
            .iter()
            .map(|r| match r.last() {
                Some(graphitesql::Value::Text(t)) => t.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    };
    for q in [
        "SELECT * FROM t ORDER BY b COLLATE NOCASE",
        "SELECT * FROM t ORDER BY b",
        "SELECT * FROM t ORDER BY b COLLATE NOCASE DESC",
        // WHERE-comparison collation: `= 'x' COLLATE NOCASE` seeks the NOCASE
        // index (`ib`); a plain `= 'x'` uses the BINARY one (B9j WHERE slice).
        "SELECT * FROM t WHERE b = 'apple' COLLATE NOCASE",
        "SELECT * FROM t WHERE b = 'apple'",
        "SELECT * FROM t WHERE b = 'Apple'",
        // Range comparisons likewise: a single `> 'x' COLLATE NOCASE` bound seeks
        // the NOCASE index; a mixed-collation `BETWEEN` keeps the gated per-bound
        // behaviour and uses the BINARY index (B9j WHERE range slice).
        "SELECT * FROM t WHERE b > 'a' COLLATE NOCASE",
        "SELECT * FROM t WHERE b > 'a'",
        "SELECT * FROM t WHERE b <= 'c' COLLATE NOCASE",
        "SELECT * FROM t WHERE b BETWEEN 'a' AND 'd' COLLATE NOCASE",
        "SELECT * FROM t WHERE b BETWEEN 'a' AND 'd'",
        // A NOCASE range seek also earns the ORDER-BY-collation credit, so no temp
        // b-tree is emitted (matching sqlite).
        "SELECT * FROM t WHERE b > 'a' COLLATE NOCASE ORDER BY b COLLATE NOCASE",
        "SELECT * FROM t WHERE b >= 'a' COLLATE NOCASE ORDER BY b COLLATE NOCASE DESC",
    ] {
        assert_eq!(graphite(q), eqp(q), "EQP diverged on `{q}`");
    }
    // The NOCASE seek returns the case-insensitive match.
    let mut cc = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            cc.execute(s).unwrap();
        }
    }
    assert_eq!(
        cc.query("SELECT a FROM t WHERE b = 'apple' COLLATE NOCASE")
            .unwrap()
            .rows,
        vec![vec![graphitesql::Value::Integer(1)]],
    );
    // And the NOCASE-ordered rows come out case-insensitively sorted.
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    let rows = c
        .query("SELECT b FROM t ORDER BY b COLLATE NOCASE")
        .unwrap()
        .rows;
    let bs: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            graphitesql::Value::Text(t) => t.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(bs, vec!["Apple", "banana", "CHERRY"]);
}

#[test]
fn single_open_range_prefers_order_index_over_seek() {
    if sqlite_eqp("SELECT 1").is_none() {
        return;
    }
    // Two independent indexes: one on the WHERE column (`i_b`), one on the ORDER BY
    // column (`i_c`). When the WHERE is a *single open-ended* range, sqlite walks
    // the ORDER-BY index to avoid the sort (its ~1/4 default selectivity does not
    // pay for losing the ordered walk); an equality / bounded range / `IN` instead
    // seeks `i_b` and sorts. graphite's EQP must match in every case.
    const S: &str = "CREATE TABLE t(a, b, c, d); \
                     CREATE INDEX i_b ON t(b); CREATE INDEX i_c ON t(c); \
                     INSERT INTO t VALUES(1,3,'x',0),(2,1,'y',0),(3,2,'z',0),\
                                        (4,1,'w',0),(5,4,'v',0);";
    let sq = |sql: &str| -> Vec<String> {
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
    let gr = |sql: &str| -> Vec<String> {
        let mut c = Connection::open_memory().unwrap();
        for stmt in S.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                c.execute(s).unwrap();
            }
        }
        c.query(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .rows
            .iter()
            .map(|r| match r.last() {
                Some(graphitesql::Value::Text(t)) => t.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    };
    for q in [
        // Single open-ended range → walk the ORDER-BY index (no sort).
        "SELECT * FROM t WHERE b>1 ORDER BY c",
        "SELECT * FROM t WHERE b>=1 ORDER BY c",
        "SELECT * FROM t WHERE b<4 ORDER BY c",
        "SELECT * FROM t WHERE b<=4 ORDER BY c",
        "SELECT * FROM t WHERE b!=1 ORDER BY c",
        "SELECT c FROM t WHERE b>1 ORDER BY c",
        // Equality / bounded range / IN → seek `i_b` and sort.
        "SELECT * FROM t WHERE b=1 ORDER BY c",
        "SELECT * FROM t WHERE b>1 AND b<4 ORDER BY c",
        "SELECT * FROM t WHERE b IN (1,2) ORDER BY c",
    ] {
        assert_eq!(sq(q), gr(q), "EQP diverged on `{q}`");
    }
    // The sort-avoiding path still yields correctly ordered rows (WHERE re-applied
    // to the ordered walk downstream): `WHERE b>1 ORDER BY c` keeps only b=3,2,4
    // → c in ('x','z','v') sorted ascending → 'v','x','z'.
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    let cs: Vec<String> = c
        .query("SELECT c FROM t WHERE b>1 ORDER BY c")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            graphitesql::Value::Text(t) => t.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(cs, vec!["v", "x", "z"]);
}

#[test]
fn sort_avoided_results_are_correctly_ordered() {
    if sqlite_eqp("SELECT 1").is_none() {
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    // The index-walk path (WHERE filtered downstream) still yields correctly
    // ordered rows.
    let rows = c
        .query("SELECT a, b FROM t WHERE a > 0 ORDER BY b")
        .unwrap()
        .rows;
    let bs: Vec<i64> = rows
        .iter()
        .map(|r| match &r[1] {
            graphitesql::Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(bs, vec![1, 1, 2, 3], "rows must be in ascending b order");
}
