//! Track B (EQP): an `ORDER BY` term written as a **1-based positional ordinal**
//! (`ORDER BY 1`) or a bare **output alias** (`SELECT b AS x … ORDER BY x`) names the
//! same column as if it had been written directly. SQLite resolves it before planning,
//! so when an index-ordered scan (full covering scan, or a `WHERE` seek) already yields
//! that column in order, *no* sorter is planned. graphite previously only resolved a
//! directly-written `ORDER BY col`, so the ordinal/alias forms spuriously emitted
//! `USE TEMP B-TREE FOR ORDER BY` — and the alias form even missed the covering index
//! entirely. Both the sorter elision and the covering-index recognition now apply to the
//! resolved column.
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): a covering scan over a
//! single- or multi-column index (`ORDER BY 1`, `ORDER BY 1, 2`), the alias form, the
//! `DESC` walk, a partial (mixed-direction) sort label, a `WHERE`-seek (`SEARCH …`)
//! whose remaining walk serves the ordinal, and the rowid / INTEGER PRIMARY KEY ordinal.
//!
//! Out of scope (separate, still divergent — pre-existing, not regressed by this slice):
//! an ordinal over `SELECT *` (the wildcard column map), and an alias that shadows a
//! table column but projects an *expression* (`SELECT b+1 AS a … ORDER BY a`), which is
//! an executor alias-resolution corner, not an index/ordinal-elision one.

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

const DC: &str = "CREATE TABLE t(a,b,c); CREATE INDEX ib ON t(b); CREATE INDEX iac ON t(a,c); \
    INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9),(2,2,2);";

/// A covering-index scan that already yields the ordinal/alias column in order emits no
/// sorter — and is still recognised as *covering*.
#[test]
fn ordinal_and_alias_elide_sorter_on_covering_scan() {
    if !have_sqlite() {
        return;
    }
    // The headline shapes resolve to the exact covering scan, no ORDER BY node.
    assert_eq!(
        g_eqp(DC, "SELECT b FROM t ORDER BY 1"),
        "SCAN t USING COVERING INDEX ib"
    );
    assert_eq!(
        g_eqp(DC, "SELECT b AS x FROM t ORDER BY x"),
        "SCAN t USING COVERING INDEX ib"
    );
    assert_eq!(
        g_eqp(DC, "SELECT a, c FROM t ORDER BY 1, 2"),
        "SCAN t USING COVERING INDEX iac"
    );
    for q in [
        "SELECT b FROM t ORDER BY 1",
        "SELECT b AS x FROM t ORDER BY x",
        "SELECT b FROM t ORDER BY 1 DESC",
        "SELECT b AS x FROM t ORDER BY x DESC",
        "SELECT a, c FROM t ORDER BY 1, 2",
        "SELECT a AS p, c AS q FROM t ORDER BY p, q",
    ] {
        let plan = g_eqp(DC, q);
        assert!(
            !plan.contains("ORDER BY"),
            "expected no ORDER BY sorter for {q}, got {plan}"
        );
        check(DC, q);
    }
}

/// A `WHERE` seek (`SEARCH …`) whose post-equality walk serves the ordinal/alias also
/// drops the sorter; a mixed-direction list keeps only the partial "LAST n TERMS" label.
#[test]
fn ordinal_serves_where_seek_and_partial_sort() {
    if !have_sqlite() {
        return;
    }
    for q in [
        "SELECT a, c FROM t WHERE a=1 ORDER BY 1",
        "SELECT a, c FROM t WHERE a=1 ORDER BY 2",
        "SELECT b AS x FROM t WHERE b>1 ORDER BY x",
        // Mixed direction: the leading ordinal is served by the walk, the trailing one
        // sorted — the partial-sort label, resolved through the ordinals.
        "SELECT a, c FROM t ORDER BY 1, 2 DESC",
        "SELECT a, c FROM t ORDER BY 1 DESC, 2",
    ] {
        check(DC, q);
    }
}

/// An ordinal/alias that names the rowid or INTEGER PRIMARY KEY rides the table's own
/// rowid order, no sorter — exactly like the directly-written column.
#[test]
fn ordinal_names_rowid_or_ipk() {
    if !have_sqlite() {
        return;
    }
    check(DC, "SELECT rowid, b FROM t ORDER BY 1");
    let dpk = "CREATE TABLE u(id INTEGER PRIMARY KEY, v); \
        INSERT INTO u VALUES(3,'x'),(1,'y'),(2,'z');";
    assert_eq!(g_eqp(dpk, "SELECT id, v FROM u ORDER BY 1"), "SCAN u");
    check(dpk, "SELECT id, v FROM u ORDER BY 1");
    check(dpk, "SELECT id AS k FROM u ORDER BY k");
}
