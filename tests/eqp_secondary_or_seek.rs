//! Track B (EQP): a same-column equality `OR`-chain on a *secondary* index column
//! (`a = 1 OR a = 2 OR …`, every disjunct a bare equality on the same non-rowid
//! column) is the equivalent of `a IN (1, 2, …)` — sqlite plans the two identically
//! as a single `SEARCH … USING INDEX` seek, not a `MULTI-INDEX OR`. `find_in_constraint`
//! recognises the chain, so the executor's `try_index_in` seeks one index for it and
//! `eqp_access` renders the matching single node (or a `SCAN` when the column has no
//! index). A mixed-column chain (`a = 1 OR b = 2`) keeps sqlite's `MULTI-INDEX OR`.
//! `run_core` re-applies the full WHERE, so the unioned per-value seek is a valid
//! superset. Verified byte-exact vs sqlite3 — plan and row set (ORDER BY left out so
//! the pre-existing IN/OR sort-elision gap doesn't enter; the seek has no inherent
//! order). The multi-index *covering*-index choice (`ia` vs sqlite's `iab`) is the
//! pre-existing `IN` index-choice gap — the OR case tracks graphite's own `IN` path
//! exactly, introducing no new divergence — so those queries are kept out of here.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn g_eqp(c: &Connection, q: &str) -> String {
    c.query(&format!("EXPLAIN QUERY PLAN {q}"))
        .unwrap()
        .rows
        .iter()
        .filter_map(|r| match r.last() {
            Some(Value::Text(s)) => Some(String::from(s.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Row values as a sorted multiset — order-independent, since a per-value index seek
/// has no inherent row order. The EQP assertions pin the plan; this pins the contents.
fn g_rows_sorted(c: &Connection, q: &str) -> Vec<String> {
    let mut v = c
        .query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|val| match val {
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => format!("{f}"),
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Null => String::new(),
                    _ => "?".into(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>();
    v.sort();
    v
}

fn sqlite_out(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn sqlite_rows_sorted(sql: &str) -> Vec<String> {
    let mut v: Vec<String> = sqlite_out(sql)
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();
    v.sort();
    v
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    sqlite_out(&format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn conn(ddl: &str) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

fn check(full: &str, q: &str) {
    let c = conn(full);
    let g = g_eqp(&c, q);
    assert_eq!(g, sqlite_eqp(full, q), "EQP diverged for {q}");
    assert_eq!(
        g_rows_sorted(&c, q),
        sqlite_rows_sorted(&format!("{full} {q};")),
        "rows diverged for {q}"
    );
}

#[test]
fn secondary_equality_or_chain_collapses_to_single_seek() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // A single secondary index on `a`. `b` is unindexed.
    let full = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a); \
                INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";

    // Each is a same-column equality OR-chain on `a` → one `SEARCH t USING INDEX ia
    // (a=?)`, identical to the equivalent `a IN (…)`. Before the fix graphite emitted
    // a MULTI-INDEX OR for these.
    let collapse: &[&str] = &[
        "SELECT * FROM t WHERE a=5 OR a=1",
        "SELECT * FROM t WHERE a=5 OR a=1 OR a=8",
        // A repeated value is harmless (just a redundant seek key).
        "SELECT * FROM t WHERE a=5 OR a=1 OR a=5",
        // Reversed operand order / nested parens.
        "SELECT * FROM t WHERE 5=a OR (a=1)",
        // A trailing AND-range only narrows the seeked superset.
        "SELECT * FROM t WHERE (a=5 OR a=1) AND b>0",
        // Baseline: the equivalent IN-list, unchanged.
        "SELECT * FROM t WHERE a IN (5,1)",
    ];
    for &q in collapse {
        let c = conn(full);
        let g = g_eqp(&c, q);
        assert!(
            !g.contains("MULTI-INDEX OR"),
            "same-column OR-chain should collapse to one seek for {q}\n  got: {g}"
        );
        assert_eq!(g, "SEARCH t USING INDEX ia (a=?)", "for {q}");
        check(full, q);
    }
}

#[test]
fn unindexed_or_chain_scans() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // `b` has no index: a `b=… OR b=…` chain is one full SCAN in both engines (no
    // seekable index to collapse onto), not a MULTI-INDEX OR.
    let full = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a); \
                INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";
    for q in [
        "SELECT * FROM t WHERE b=9 OR b=2",
        "SELECT * FROM t WHERE b=9 OR b=2 OR b=4",
    ] {
        let c = conn(full);
        let g = g_eqp(&c, q);
        assert_eq!(g, "SCAN t", "for {q}");
        check(full, q);
    }
}

#[test]
fn mixed_column_or_keeps_multi_index_or() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // Two secondary indexes. A *different*-column equality chain is a genuine
    // MULTI-INDEX OR (each disjunct seeks its own index) — the recognition must
    // decline it. Verified byte-exact (plan and rows) vs sqlite.
    let full = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); \
                CREATE INDEX ia ON t(a); CREATE INDEX ib ON t(b); \
                INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";
    for q in [
        "SELECT * FROM t WHERE a=5 OR b=2",
        "SELECT * FROM t WHERE a=5 OR a=1 OR b=7",
    ] {
        let c = conn(full);
        let g = g_eqp(&c, q);
        assert!(
            g.contains("MULTI-INDEX OR"),
            "expected MULTI-INDEX OR for {q}\n  got: {g}"
        );
        check(full, q);
    }
}
