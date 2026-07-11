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
                    Value::Text(t) => t.clone(),
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
