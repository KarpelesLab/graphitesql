//! Track B (EQP): folding an `ORDER BY` into a `GROUP BY` / `DISTINCT` transient
//! b-tree. When a bare-`SCAN` aggregate spills its grouping key through a temp b-tree
//! (`USE TEMP B-TREE FOR GROUP BY` / `… FOR DISTINCT`), that b-tree already delivers
//! the rows in key order. SQLite reuses it for the `ORDER BY` — emitting *no* separate
//! sorter — exactly when every `ORDER BY` term names a grouping key column and the term
//! list matches the key list:
//!
//!   * a term may name the key **directly** (`ORDER BY a`), by **1-based position**
//!     (`ORDER BY 1`), or through an **output alias** (`SELECT a AS x … ORDER BY x`);
//!   * a `GROUP BY` b-tree can be walked to honor any per-column `ASC`/`DESC`, even a
//!     mix (`ORDER BY 1 DESC, 2 ASC`); a `DISTINCT` b-tree is ascending-only, so any
//!     `DESC` term brings the sorter back;
//!   * the `NULLS` placement must be the default for the term's direction — `ASC` ⇒
//!     `NULLS FIRST`, `DESC` ⇒ `NULLS LAST`; the opposite, explicit placement defeats
//!     the fold.
//!
//! Any other shape just keeps the grouping node *and* the `ORDER BY` node: a term that
//! resolves to an aggregate or a non-key column (`ORDER BY count(*)`, `ORDER BY 3`), a
//! reordered key list (`ORDER BY 2, 1`), or a non-default `NULLS`. Previously graphite
//! declined the *grouping* node entirely whenever an `ORDER BY` term was not a plain
//! column, emitting a lone (wrong) `ORDER BY` sorter; now the grouping node always
//! stands and only the fold decision varies. Verified byte-exact against sqlite3 3.50.4,
//! both the plan and the result rows.
//!
//! Out of scope (separate slices, still divergent): a grouping key that is an
//! *expression* (`GROUP BY a+0`), and an `ORDER BY` ordinal over an *index-ordered*
//! scan (`GROUP BY b ORDER BY 1` with an index on `b`), where the access path — not this
//! b-tree — provides the order.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn norm(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).trim_end())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn g_eqp(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let rows = c.query(&format!("EXPLAIN QUERY PLAN {q}")).unwrap().rows;
    let mut lines = Vec::new();
    for r in &rows {
        if let Some(graphitesql::Value::Text(s)) = r.last() {
            lines.push(String::from(s.as_str()));
        }
    }
    lines.join(" | ")
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .output()
        .unwrap();
    norm(&String::from_utf8_lossy(&o.stdout))
}

fn g_rows(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let r = c.query(q).unwrap();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    graphitesql::Value::Null => "".to_string(),
                    graphitesql::Value::Integer(i) => i.to_string(),
                    graphitesql::Value::Real(f) => {
                        let s = format!("{f}");
                        if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                            format!("{s}.0")
                        } else {
                            s
                        }
                    }
                    graphitesql::Value::Text(s) => String::from(s.as_str()),
                    graphitesql::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite_rows(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} {q};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn check(ddl: &str, q: &str) {
    assert_eq!(g_eqp(ddl, q), sqlite_eqp(ddl, q), "EQP diverged for {q}");
    assert_eq!(g_rows(ddl, q), sqlite_rows(ddl, q), "rows diverged for {q}");
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const DC: &str = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9),(2,2,2);";

/// The grouping b-tree absorbs the ORDER BY — no separate sorter — when each term
/// names the key by position, alias, or directly, and the lists match.
#[test]
fn order_by_folds_into_group_btree() {
    if !have_sqlite() {
        return;
    }
    // The folded shape is exactly the grouping node, no ORDER BY node.
    assert_eq!(
        g_eqp(DC, "SELECT a FROM t GROUP BY a ORDER BY 1"),
        "SCAN t | USE TEMP B-TREE FOR GROUP BY"
    );
    for q in [
        "SELECT a FROM t GROUP BY a ORDER BY 1",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY 1",
        "SELECT a FROM t GROUP BY a HAVING count(*)>1 ORDER BY 1",
        "SELECT a AS x FROM t GROUP BY a ORDER BY x",
        "SELECT a AS x, count(*) FROM t GROUP BY a ORDER BY x",
        "SELECT a, b FROM t GROUP BY a, b ORDER BY 1, 2",
        "SELECT a, b, count(*) FROM t GROUP BY a, b ORDER BY 1, 2",
        // GROUP BY walks any direction, even mixed, and a default-for-direction NULLS.
        "SELECT a FROM t GROUP BY a ORDER BY 1 DESC",
        "SELECT a, b FROM t GROUP BY a, b ORDER BY 1 DESC, 2 ASC",
        "SELECT a FROM t GROUP BY a ORDER BY 1 NULLS FIRST",
        "SELECT a FROM t GROUP BY a ORDER BY 1 DESC NULLS LAST",
    ] {
        let plan = g_eqp(DC, q);
        assert!(
            !plan.contains("ORDER BY"),
            "expected folded ORDER BY for {q}, got {plan}"
        );
        check(DC, q);
    }
}

/// The sorter stands alongside the grouping node when a term escapes the key — an
/// aggregate or non-key column, a reordered key list, or a non-default NULLS.
#[test]
fn order_by_keeps_sorter_when_term_escapes_key() {
    if !have_sqlite() {
        return;
    }
    for q in [
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY 2",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY count(*)",
        "SELECT a, b, count(*) FROM t GROUP BY a, b ORDER BY 3",
        "SELECT a, b FROM t GROUP BY a, b ORDER BY 2, 1",
        "SELECT a FROM t GROUP BY a ORDER BY 1 NULLS LAST",
        "SELECT a FROM t GROUP BY a ORDER BY 1 DESC NULLS FIRST",
    ] {
        check(DC, q);
    }
}

/// A `DISTINCT` b-tree is ascending-only: an ascending ordinal/position folds, but any
/// `DESC` term brings the sorter back.
#[test]
fn distinct_btree_folds_ascending_only() {
    if !have_sqlite() {
        return;
    }
    assert_eq!(
        g_eqp(DC, "SELECT DISTINCT a FROM t ORDER BY 1"),
        "SCAN t | USE TEMP B-TREE FOR DISTINCT"
    );
    for q in [
        "SELECT DISTINCT a FROM t ORDER BY 1",
        "SELECT DISTINCT a, b FROM t ORDER BY 1, 2",
        "SELECT DISTINCT a FROM t ORDER BY 1 DESC",
        "SELECT DISTINCT a, b FROM t ORDER BY 1 DESC, 2 DESC",
    ] {
        check(DC, q);
    }
}
