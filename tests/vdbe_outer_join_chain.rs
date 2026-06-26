//! B5b-1 (N-table generalization): a left-deep chain of ≥ 2 `LEFT`/`INNER` joins
//! (at least one `LEFT`, no `NATURAL`/`USING`) runs on the VDBE as one N-table
//! null-padding nested loop (`compile_left_join_n`) — no `t1 × … × tN`
//! cross-product is materialized. Each join's `ON` gates matches at its own level
//! (a `LEFT` level null-pads an unmatched outer row and still recurses into the
//! inner cursors; an `INNER` level drops it); `WHERE` filters the assembled row.
//! `query_vdbe` errors on any fallback to the tree-walker, so a passing query
//! proves the VDBE compiled the chain. Results match the tree-walker and sqlite
//! 3.50.4.
//!
//! The tables carry disjoint column names (the VDBE join resolver rejects a name
//! shared across tables) and the join keys are distinct names, so a qualified
//! reference is unambiguous. Each query orders by the unique per-table rowids so
//! the differential row order is total (NULL rowids of a null-padded side sort
//! first, matching SQLite).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

// a3 has no matching b; b2 (bv=200) has no matching c; so the chain exercises a
// null at the middle level (b), a null at the tail level (c), and a fully-null
// tail (a3 → null b → null c). d matches only c1 (ck=100).
const SETUP: &str = "\
    CREATE TABLE a(aid INTEGER PRIMARY KEY, ak INT, an TEXT);\n\
    INSERT INTO a(ak,an) VALUES (1,'a1'),(2,'a2'),(3,'a3');\n\
    CREATE TABLE b(bid INTEGER PRIMARY KEY, bk INT, bv INT, bn TEXT);\n\
    INSERT INTO b(bk,bv,bn) VALUES (1,100,'b1'),(1,200,'b2'),(2,100,'b3');\n\
    CREATE TABLE c(cid INTEGER PRIMARY KEY, ck INT, cn TEXT);\n\
    INSERT INTO c(ck,cn) VALUES (100,'c1');\n\
    CREATE TABLE d(did INTEGER PRIMARY KEY, dk INT, dn TEXT);\n\
    INSERT INTO d(dk,dn) VALUES (100,'d1');\n";

const QUERIES: &[&str] = &[
    // 3-table all-LEFT chain: every left row preserved, b then c null-padded.
    "SELECT a.an, b.bn, c.cn FROM a \
       LEFT JOIN b ON a.ak=b.bk LEFT JOIN c ON b.bv=c.ck \
       ORDER BY a.aid, b.bid, c.cid",
    // INNER then LEFT: a3 (no b) is dropped; b2 (no c) keeps a null c.
    "SELECT a.an, b.bn, c.cn FROM a \
       JOIN b ON a.ak=b.bk LEFT JOIN c ON b.bv=c.ck \
       ORDER BY a.aid, b.bid",
    // LEFT then INNER: only (a,b) pairs with a c match survive the inner level.
    "SELECT a.an, b.bn, c.cn FROM a \
       LEFT JOIN b ON a.ak=b.bk JOIN c ON b.bv=c.ck \
       ORDER BY a.aid, b.bid",
    // WHERE over the assembled row: keep the null-c rows (the middle/tail nulls).
    "SELECT a.an, b.bn FROM a \
       LEFT JOIN b ON a.ak=b.bk LEFT JOIN c ON b.bv=c.ck \
       WHERE c.cn IS NULL ORDER BY a.aid, b.bid",
    // DISTINCT spanning the base key and the null-padded tail column.
    "SELECT DISTINCT a.ak, c.cn FROM a \
       LEFT JOIN b ON a.ak=b.bk LEFT JOIN c ON b.bv=c.ck \
       ORDER BY a.ak, c.cn",
    // Computed projection spanning three tables + COALESCE over the nulls.
    "SELECT a.an || '/' || coalesce(b.bn,'-') || '/' || coalesce(c.cn,'-') AS path FROM a \
       LEFT JOIN b ON a.ak=b.bk LEFT JOIN c ON b.bv=c.ck \
       ORDER BY a.aid, b.bid, c.cid",
    // LIMIT/OFFSET over the ordered chain.
    "SELECT a.an, b.bn, c.cn FROM a \
       LEFT JOIN b ON a.ak=b.bk LEFT JOIN c ON b.bv=c.ck \
       ORDER BY a.aid, b.bid, c.cid LIMIT 3 OFFSET 1",
    // 4-table all-LEFT chain: d hangs off c, so it is null wherever c is null.
    "SELECT a.an, b.bn, c.cn, d.dn FROM a \
       LEFT JOIN b ON a.ak=b.bk LEFT JOIN c ON b.bv=c.ck LEFT JOIN d ON c.ck=d.dk \
       ORDER BY a.aid, b.bid, c.cid",
    // 4-table mixed chain (LEFT, INNER, LEFT).
    "SELECT a.an, b.bn, c.cn, d.dn FROM a \
       LEFT JOIN b ON a.ak=b.bk JOIN c ON b.bv=c.ck LEFT JOIN d ON c.ck=d.dk \
       ORDER BY a.aid, b.bid, c.cid",
];

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c
}

fn sqlite3_rows(query: &str) -> Vec<Vec<String>> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg("-ascii")
        .arg(format!("{SETUP}{query};"))
        .output()
        .unwrap();
    assert!(out.status.success(), "sqlite3 failed on {query}");
    let text = String::from_utf8(out.stdout).unwrap();
    text.split('\u{1e}')
        .filter(|r| !r.is_empty())
        .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
        .collect()
}

#[test]
fn chain_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the chain compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn chain_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in QUERIES {
        let vdbe: Vec<Vec<String>> = c
            .query_vdbe(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect();
        assert_eq!(vdbe, sqlite3_rows(q), "VDBE vs sqlite3 diverged on {q}");
    }
}

#[test]
fn right_and_full_chains_stay_correct() {
    // `compile_left_join_n` deliberately handles only LEFT/INNER chains; a chain
    // containing a RIGHT or FULL join takes another path. Whichever path runs, the
    // result must still match sqlite — a regression guard for the routing.
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in &[
        "SELECT a.an, b.bn, c.cn FROM a \
           LEFT JOIN b ON a.ak=b.bk RIGHT JOIN c ON b.bv=c.ck \
           ORDER BY c.cid, a.aid, b.bid",
        "SELECT a.an, b.bn, c.cn FROM a \
           LEFT JOIN b ON a.ak=b.bk FULL JOIN c ON b.bv=c.ck \
           ORDER BY c.cid, a.aid, b.bid",
    ] {
        let got: Vec<Vec<String>> = c
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect();
        assert_eq!(got, sqlite3_rows(q), "graphite vs sqlite3 diverged on {q}");
    }
}
