//! Track B (EQP): a positional `ORDER BY` ordinal over a `SELECT *` / `SELECT t.*`
//! wildcard resolves to the column it names, exactly as SQLite resolves it against the
//! *expanded* output list before planning. So when an index-ordered scan already yields
//! that column in order, no sorter is planned — `SELECT * FROM t ORDER BY 1` walks an
//! index on the first column instead of building a temp b-tree. graphite previously left
//! a wildcard ordinal unresolved (there is no `ResultColumn::Expr` to borrow), so it
//! spuriously emitted `USE TEMP B-TREE FOR ORDER BY`.
//!
//! The wildcard is expanded in place (so a mixed `SELECT a, * … ORDER BY n` counts the
//! expanded columns), and the coverage check resolves the same way — an all-columns-
//! covered wildcard (`SELECT * FROM s ORDER BY 1` where the sole column is indexed) is
//! recognised as a *covering* scan, matching sqlite.
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): `SELECT *` / `t.*` over a
//! single- and multi-column covering index, the `DESC` walk, a `WHERE`-seek whose walk
//! serves the wildcard ordinal, a mixed projection, and the rowid / INTEGER PRIMARY KEY
//! wildcard ordinal.
//!
//! Out of scope (separate, still divergent — pre-existing, not regressed by this slice):
//! a `WITHOUT ROWID` table's PK-clustered scan order (graphite's single-table order
//! paths all decline a `WITHOUT ROWID` table, wildcard or named alike).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

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

fn norm(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).trim_end())
        .collect::<Vec<_>>()
        .join(" | ")
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

/// A wildcard ordinal that names an index's leading column rides that index in order —
/// no sorter — exactly like the directly-written column.
#[test]
fn wildcard_ordinal_elides_sorter() {
    if !have_sqlite() {
        return;
    }
    // `*` ordinal 1 → column a → leading column of iac; `*` ordinal 2 → column b → ib.
    assert_eq!(
        g_eqp(DC, "SELECT * FROM t ORDER BY 1"),
        "SCAN t USING INDEX iac"
    );
    assert_eq!(
        g_eqp(DC, "SELECT * FROM t ORDER BY 2"),
        "SCAN t USING INDEX ib"
    );
    for q in [
        "SELECT * FROM t ORDER BY 1",
        "SELECT * FROM t ORDER BY 2",
        "SELECT t.* FROM t ORDER BY 1",
        "SELECT * FROM t ORDER BY 1 DESC",
        "SELECT * FROM t AS x ORDER BY 1",
        "SELECT x.* FROM t AS x ORDER BY 2",
        // A mixed projection counts the expanded output columns: `a, a, b, c` → 2 = a.
        "SELECT a, * FROM t ORDER BY 2",
        // A `WHERE` seek whose post-equality walk serves the wildcard ordinal.
        "SELECT * FROM t WHERE a=1 ORDER BY 3",
    ] {
        let plan = g_eqp(DC, q);
        assert!(
            !plan.contains("ORDER BY"),
            "expected no ORDER BY sorter for {q}, got {plan}"
        );
        check(DC, q);
    }
}

/// When the only columns a `SELECT *` projects are all held by the index it scans in
/// order, the scan is recognised as *covering* — `USING COVERING INDEX`, like sqlite.
#[test]
fn wildcard_ordinal_recognises_covering_scan() {
    if !have_sqlite() {
        return;
    }
    let s1 = "CREATE TABLE s(a); CREATE INDEX isa ON s(a); INSERT INTO s VALUES(3),(1),(2);";
    assert_eq!(
        g_eqp(s1, "SELECT * FROM s ORDER BY 1"),
        "SCAN s USING COVERING INDEX isa"
    );
    check(s1, "SELECT * FROM s ORDER BY 1");
    check(s1, "SELECT s.* FROM s ORDER BY 1");
    let p =
        "CREATE TABLE p(a,b); CREATE INDEX iab ON p(a,b); INSERT INTO p VALUES(2,9),(1,8),(1,7);";
    assert_eq!(
        g_eqp(p, "SELECT * FROM p ORDER BY 1, 2"),
        "SCAN p USING COVERING INDEX iab"
    );
    check(p, "SELECT * FROM p ORDER BY 1");
    check(p, "SELECT * FROM p ORDER BY 1, 2");
}

/// A wildcard ordinal that names the rowid or INTEGER PRIMARY KEY rides the table's own
/// rowid order, no sorter.
#[test]
fn wildcard_ordinal_names_ipk() {
    if !have_sqlite() {
        return;
    }
    let dpk = "CREATE TABLE u(id INTEGER PRIMARY KEY, v); \
        INSERT INTO u VALUES(3,'x'),(1,'y'),(2,'z');";
    assert_eq!(g_eqp(dpk, "SELECT * FROM u ORDER BY 1"), "SCAN u");
    check(dpk, "SELECT * FROM u ORDER BY 1");
}
