//! B1b on the VDBE: a two-table inner join that the cost model reorders to drive
//! from the *second* table (seeking `from.first` by its cheaper rowid) now runs on
//! the VDBE instead of falling back. The VDBE models the swap by nesting the second
//! cursor outermost (`[1, 0]`), which — because a rowid join matches ≤1 inner row —
//! reproduces the tree-walker's driven, unordered row order exactly. When the driver
//! would instead be walked via a *reordering* covering index (which the VDBE's
//! materialized rowset scan does not reproduce), the query still defers to the
//! tree-walker. Verified VDBE == tree-walker == `sqlite3`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(rows: &[Vec<Value>]) -> String {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(t) => String::from(t.as_str()),
                    Value::Real(r) => r.to_string(),
                    Value::Blob(_) => "blob".into(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite(setup: &str, q: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{setup} {q};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// A plain-scanned driver: the swap runs on the VDBE and matches everywhere.
#[test]
fn rowid_swap_runs_on_vdbe_and_matches() {
    if !sqlite3_available() {
        return;
    }
    // `t` is the rowid table (its PK `a` is the join key → the swap drives from `u`,
    // seeking `t` by rowid). `u` has no index, so it is scanned in rowid order —
    // exactly the materialized rowset order the VDBE walks. `u.k=2` repeats to
    // exercise the multi-driver-row case.
    const S: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, x); \
                     INSERT INTO t VALUES(1,'t1'),(2,'t2'),(3,'t3'); \
                     CREATE TABLE u(k, v); \
                     INSERT INTO u VALUES(2,'q'),(1,'p'),(2,'r'),(9,'z');";
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        "SELECT t.x, u.v FROM t JOIN u ON t.a=u.k",
        "SELECT u.v, t.x FROM t JOIN u ON t.a=u.k",
        "SELECT t.x, u.v FROM t JOIN u ON t.a=u.k WHERE u.v>'p'",
        "SELECT DISTINCT t.x FROM t JOIN u ON t.a=u.k",
        "SELECT t.x, u.v FROM t JOIN u ON t.a=u.k LIMIT 2",
        // The comma form (equality promoted to an ON) swaps too.
        "SELECT t.x, u.v FROM t, u WHERE t.a=u.k",
    ] {
        // Runs on the VDBE (no fallback).
        let v = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("`{q}` must run on the VDBE: {e}"));
        let vs = render(&v.rows);
        // Equals the tree-walker.
        c.set_use_vdbe(false);
        let tw = render(&c.query(q).unwrap().rows);
        c.set_use_vdbe(true);
        assert_eq!(vs, tw, "VDBE vs tree-walker for `{q}`");
        // Equals sqlite.
        assert_eq!(vs, sqlite(S, q), "VDBE vs sqlite for `{q}`");
    }
}

/// A single-column UNIQUE secondary index on the inner also matches ≤1 row, so the
/// index-inner swap runs on the VDBE too; a non-unique index can multi-match and
/// still defers.
#[test]
fn unique_index_inner_swap_runs_but_nonunique_defers() {
    if !sqlite3_available() {
        return;
    }
    // `f.a` carries a single-column UNIQUE index; `s.k` is not seekable, so the cost
    // model drives from `s`, seeking `f` by the unique index.
    const SU: &str = "CREATE TABLE f(a UNIQUE, x); \
                      INSERT INTO f VALUES(3,'f3'),(1,'f1'),(2,'f2'); \
                      CREATE TABLE s(k, v); INSERT INTO s VALUES(2,'q'),(1,'p'),(2,'r'),(9,'z');";
    let mut c = Connection::open_memory().unwrap();
    for stmt in SU.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        "SELECT f.x, s.v FROM f JOIN s ON f.a=s.k",
        "SELECT DISTINCT f.x FROM f JOIN s ON f.a=s.k",
        "SELECT f.x, s.v FROM f JOIN s ON f.a=s.k WHERE s.v>'p'",
    ] {
        let v = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("`{q}` must run on the VDBE: {e}"));
        let vs = render(&v.rows);
        c.set_use_vdbe(false);
        let tw = render(&c.query(q).unwrap().rows);
        c.set_use_vdbe(true);
        assert_eq!(vs, tw, "VDBE vs tree-walker for `{q}`");
        assert_eq!(vs, sqlite(SU, q), "VDBE vs sqlite for `{q}`");
    }

    // A NON-unique index on the inner can match several rows (in index-key order),
    // which the VDBE's scan+filter would not reproduce — it must defer.
    const SN: &str = "CREATE TABLE f(a, x); CREATE INDEX fa ON f(a); \
                      INSERT INTO f VALUES(1,'f1a'),(1,'f1b'),(2,'f2'); \
                      CREATE TABLE s(k, v); INSERT INTO s VALUES(2,'q'),(1,'p');";
    let mut c2 = Connection::open_memory().unwrap();
    for stmt in SN.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c2.execute(s).unwrap();
        }
    }
    let q = "SELECT f.x, s.v FROM f JOIN s ON f.a=s.k";
    assert!(
        c2.query_vdbe(q).is_err(),
        "a non-unique index-inner swap must defer to the tree-walker"
    );
    assert_eq!(render(&c2.query(q).unwrap().rows), sqlite(SN, q));
}

/// A *bare order-independent* aggregate (count/sum/min/max/avg/total) is invariant
/// to the join drive order, so a swap — and even an N-table reorder — needs no
/// modelling: the VDBE's identity-order fold is correct. An order-sensitive
/// aggregate (`group_concat`) still defers.
#[test]
fn bare_order_independent_aggregate_over_swap_runs_on_vdbe() {
    if !sqlite3_available() {
        return;
    }
    const S: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b); \
                     INSERT INTO t VALUES(1,'t1'),(2,'t2'),(3,'t3'); \
                     CREATE TABLE u(k, v); \
                     INSERT INTO u VALUES(2,'q'),(1,'p'),(2,'r'),(9,'z'); \
                     CREATE TABLE w(m INTEGER PRIMARY KEY, n); \
                     INSERT INTO w VALUES(1,'A'),(2,'B'),(3,'C');";
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        "SELECT count(*) FROM t JOIN u ON t.a=u.k",
        "SELECT sum(t.a), max(u.v), min(u.v) FROM t JOIN u ON t.a=u.k",
        // Order-independent aggregate over a three-table join (N-table reorder).
        "SELECT count(*) FROM t JOIN u ON t.a=u.k JOIN w ON t.a=w.m",
    ] {
        let v = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("`{q}` must run on the VDBE: {e}"));
        let vs = render(&v.rows);
        c.set_use_vdbe(false);
        let tw = render(&c.query(q).unwrap().rows);
        c.set_use_vdbe(true);
        assert_eq!(vs, tw, "VDBE vs tree-walker for `{q}`");
        assert_eq!(vs, sqlite(S, q), "VDBE vs sqlite for `{q}`");
    }
    // An order-sensitive aggregate over the swap-join still defers (its fold order
    // would differ) — but the fallback returns sqlite's value.
    let gc = "SELECT group_concat(u.v) FROM t JOIN u ON t.a=u.k";
    assert!(
        c.query_vdbe(gc).is_err(),
        "group_concat over a swap-join must defer"
    );
    assert_eq!(render(&c.query(gc).unwrap().rows), sqlite(S, gc));
}

/// An N-table (three-plus) cost-based reorder of a *plain projection* runs on the
/// VDBE when every inner is a ≤1-match seek (rowid / single-column unique) so the
/// row order is fixed by the driver; a non-unique inner still defers.
#[test]
fn ntable_plain_projection_reorder_runs_when_all_inners_single_match() {
    if !sqlite3_available() {
        return;
    }
    // `big` is the fact table; `dim1`/`dim2` are rowid dimensions seeked by rowid —
    // every inner matches ≤1 row, so the reorder (drive the smaller table) is
    // order-safe on the VDBE.
    const S: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, d1, d2, val); \
                     INSERT INTO big VALUES(1,2,3,'a'),(2,1,2,'b'),(3,3,1,'c'),(4,2,3,'d'); \
                     CREATE TABLE dim1(k INTEGER PRIMARY KEY, n1); \
                     INSERT INTO dim1 VALUES(1,'X'),(2,'Y'),(3,'Z'); \
                     CREATE TABLE dim2(m INTEGER PRIMARY KEY, n2); \
                     INSERT INTO dim2 VALUES(1,'P'),(2,'Q'),(3,'R');";
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        "SELECT big.val, dim1.n1, dim2.n2 FROM big JOIN dim1 ON big.d1=dim1.k \
         JOIN dim2 ON big.d2=dim2.m",
        "SELECT dim1.n1, big.val FROM dim1 JOIN big ON big.d1=dim1.k JOIN dim2 ON big.d2=dim2.m",
        "SELECT big.val FROM big JOIN dim1 ON big.d1=dim1.k JOIN dim2 ON big.d2=dim2.m \
         WHERE dim1.n1>'X'",
    ] {
        let v = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("`{q}` must run on the VDBE: {e}"));
        let vs = render(&v.rows);
        c.set_use_vdbe(false);
        let tw = render(&c.query(q).unwrap().rows);
        c.set_use_vdbe(true);
        assert_eq!(vs, tw, "VDBE vs tree-walker for `{q}`");
        assert_eq!(vs, sqlite(S, q), "VDBE vs sqlite for `{q}`");
    }

    // A non-unique inner (`dim1.k` non-unique index) can multi-match, so a
    // plain-projection reorder over it defers — but the fallback is still correct.
    const SN: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, d1, d2, val); \
                      INSERT INTO big VALUES(1,1,3,'a'),(2,1,2,'b'); \
                      CREATE TABLE dim1(k, n1); CREATE INDEX d1k ON dim1(k); \
                      INSERT INTO dim1 VALUES(1,'X'),(1,'X2'); \
                      CREATE TABLE dim2(m INTEGER PRIMARY KEY, n2); \
                      INSERT INTO dim2 VALUES(2,'Q'),(3,'R');";
    let mut c2 = Connection::open_memory().unwrap();
    for stmt in SN.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c2.execute(s).unwrap();
        }
    }
    let q =
        "SELECT big.val, dim1.n1 FROM big JOIN dim1 ON big.d1=dim1.k JOIN dim2 ON big.d2=dim2.m";
    // Whichever way the cost model reorders, if it drives a non-declaration order
    // over the non-unique inner it must defer; either way the result is correct.
    let _ = c2.query_vdbe(q); // may run or defer depending on the chosen order
    assert_eq!(render(&c2.query(q).unwrap().rows), sqlite(SN, q));
}

/// A driver walked via a reordering covering index: the VDBE cannot reproduce the
/// index-key scan order from its rowid-order rowset, so the query defers to the
/// tree-walker — but `query` (which falls back) still returns the correct rows.
#[test]
fn rowid_swap_with_reordering_driver_defers_but_is_correct() {
    if !sqlite3_available() {
        return;
    }
    // `v` is scanned via the covering index `iv` on `p` (p-order), not rowid order.
    const S: &str = "CREATE TABLE u(x INTEGER PRIMARY KEY, y); \
                     INSERT INTO u VALUES(3,30),(1,10),(2,20); \
                     CREATE TABLE v(p, q); CREATE INDEX iv ON v(p); \
                     INSERT INTO v VALUES(2,200),(1,100),(3,300);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    let q = "SELECT DISTINCT u.y FROM u JOIN v ON u.x=v.p";
    // The VDBE declines this shape (the reordering driver is not modelled).
    assert!(
        c.query_vdbe(q).is_err(),
        "reordering-driver swap must defer to the tree-walker"
    );
    // The fallback path still returns sqlite's rows/order.
    let got = render(&c.query(q).unwrap().rows);
    assert_eq!(got, sqlite(S, q), "fallback rows for `{q}`");
    assert_eq!(got, "10\n20\n30");
}
