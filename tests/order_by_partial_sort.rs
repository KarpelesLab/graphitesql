//! Track B (B0b-i remainder): a no-`WHERE` query whose `ORDER BY` shares a
//! uniform leading prefix with a covering index but then changes direction is
//! scanned via that index and only its trailing terms are sorted — reported like
//! sqlite as `USE TEMP B-TREE FOR LAST n TERM[S] OF ORDER BY` (not a full sort),
//! with identical rows. Verified differentially against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// Distinct (a,b) prefixes so every tested ORDER BY fully determines row order.
const DDL: &str = "CREATE TABLE t(a,b,c); CREATE INDEX i ON t(a,b,c); \
                   INSERT INTO t VALUES (1,1,1),(1,2,2),(2,1,3),(2,2,4),(3,5,5);";

fn g_eqp(c: &Connection, q: &str) -> String {
    c.query(&format!("EXPLAIN QUERY PLAN {q}"))
        .unwrap()
        .rows
        .iter()
        .filter_map(|r| match r.last() {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn g_rows(c: &Connection, q: &str) -> String {
    c.query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => s.clone(),
                    Value::Null => String::new(),
                    _ => "?".into(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite_out(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

#[test]
fn mixed_direction_partial_sort_over_covering_index() {
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let mut c = Connection::open_memory().unwrap();
    for stmt in DDL.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let queries = [
        "SELECT a,b,c FROM t ORDER BY a, b DESC",
        "SELECT a,b,c FROM t ORDER BY a DESC, b",
        "SELECT a,b,c FROM t ORDER BY a,b,c DESC",
        "SELECT a,b,c FROM t ORDER BY a, b DESC, c",
        "SELECT a,b,c FROM t ORDER BY a DESC, b DESC, c",
    ];
    for q in queries {
        let g = g_eqp(&c, q);
        assert!(
            g.contains("USING COVERING INDEX i") && g.contains("LAST"),
            "expected covering index + partial sort, got: {g} ({q})"
        );
        if have_sqlite {
            // sqlite EQP details (strip the tree-drawing prefix).
            let want_eqp = sqlite_out(&format!("{DDL} EXPLAIN QUERY PLAN {q};"))
                .lines()
                .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
                .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
                .collect::<Vec<_>>()
                .join(" | ");
            assert_eq!(g, want_eqp, "EQP diverged for: {q}");
            let want_rows = sqlite_out(&format!("{DDL} {q};"));
            assert_eq!(
                g_rows(&c, q).trim(),
                want_rows.trim(),
                "rows diverged for: {q}"
            );
        }
    }
}

#[test]
fn group_by_order_by_same_prefix_emits_no_temp_btree() {
    // `GROUP BY a ORDER BY a` answered by a covering index on `a`: the group-by
    // access already yields every ORDER BY term in order, so zero terms need
    // sorting. SQLite emits a bare `SCAN … USING COVERING INDEX` with NO
    // temp-btree node — graphite must not over-emit a nonsensical
    // `USE TEMP B-TREE FOR LAST 0 TERMS OF ORDER BY`.
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    const GB: &str = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); \
                      INSERT INTO t VALUES (3,1),(1,2),(2,3),(1,4),(3,5);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in GB.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let q = "SELECT a, count(*) FROM t GROUP BY a ORDER BY a";
    let g = g_eqp(&c, q);
    assert!(
        g.contains("USING COVERING INDEX it") && !g.contains("TEMP B-TREE"),
        "expected covering index with no temp-btree, got: {g}"
    );
    if have_sqlite {
        let want_eqp = sqlite_out(&format!("{GB} EXPLAIN QUERY PLAN {q};"))
            .lines()
            .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
            .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert_eq!(g, want_eqp, "EQP diverged for: {q}");
        let want_rows = sqlite_out(&format!("{GB} {q};"));
        assert_eq!(
            g_rows(&c, q).trim(),
            want_rows.trim(),
            "rows diverged for: {q}"
        );
    }
}

#[test]
fn order_by_longer_than_index_walks_the_prefix() {
    // index on `(a)` only, but `ORDER BY a, b` (or a, b, c): the index walks the
    // `a` prefix in order and sqlite sorts only the trailing terms —
    // `SCAN t USING INDEX it` + `USE TEMP B-TREE FOR LAST TERM[S] OF ORDER BY`.
    // graphite previously rejected any index shorter than the ORDER BY and fell
    // back to a plain `SCAN t` + full sort.
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    const NC: &str = "CREATE TABLE t(a,b,c); CREATE INDEX it ON t(a); \
                      INSERT INTO t VALUES (3,9,1),(1,5,2),(2,7,3),(1,2,4),(3,1,5),(2,8,6);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in NC.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let queries = [
        "SELECT a,b,c FROM t ORDER BY a, b",
        "SELECT a,b,c FROM t ORDER BY a, b, c",
        "SELECT a,b,c FROM t ORDER BY a DESC, b",
        "SELECT a,b,c FROM t ORDER BY a, b DESC",
    ];
    for q in queries {
        let g = g_eqp(&c, q);
        assert!(
            g.contains("USING INDEX it") && g.contains("LAST"),
            "expected index-prefix walk + partial sort, got: {g} ({q})"
        );
        if have_sqlite {
            let want_eqp = sqlite_out(&format!("{NC} EXPLAIN QUERY PLAN {q};"))
                .lines()
                .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
                .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
                .collect::<Vec<_>>()
                .join(" | ");
            assert_eq!(g, want_eqp, "EQP diverged for: {q}");
            let want_rows = sqlite_out(&format!("{NC} {q};"));
            assert_eq!(
                g_rows(&c, q).trim(),
                want_rows.trim(),
                "rows diverged for: {q}"
            );
        }
    }
}

#[test]
fn mixed_direction_partial_sort_over_noncovering_index() {
    // index (a,b) but the projection needs `d` (not in the index) → non-covering.
    // sqlite walks the index for the leading prefix then sorts the trailing term:
    // `SCAN t USING INDEX i` + `USE TEMP B-TREE FOR LAST TERM OF ORDER BY`.
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    const NC: &str = "CREATE TABLE t(a,b,d); CREATE INDEX i ON t(a,b); \
                      INSERT INTO t VALUES (1,1,9),(1,2,8),(2,1,7),(2,2,6),(3,5,5);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in NC.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let queries = [
        "SELECT a,b,d FROM t ORDER BY a, b DESC",
        "SELECT a,b,d FROM t ORDER BY a DESC, b",
    ];
    for q in queries {
        let g = g_eqp(&c, q);
        assert!(
            g.contains("USING INDEX i") && g.contains("LAST TERM"),
            "expected non-covering index walk + partial sort, got: {g} ({q})"
        );
        if have_sqlite {
            let want_eqp = sqlite_out(&format!("{NC} EXPLAIN QUERY PLAN {q};"))
                .lines()
                .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
                .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
                .collect::<Vec<_>>()
                .join(" | ");
            assert_eq!(g, want_eqp, "EQP diverged for: {q}");
            let want_rows = sqlite_out(&format!("{NC} {q};"));
            assert_eq!(
                g_rows(&c, q).trim(),
                want_rows.trim(),
                "rows diverged for: {q}"
            );
        }
    }
}
