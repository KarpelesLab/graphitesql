//! B5c-2: a *correlated* scalar / `EXISTS` / `NOT EXISTS` subquery in the
//! projection or `WHERE` of a live single-table scan runs on the VDBE — the
//! subquery is re-evaluated per outer row against the current row (an outer
//! column reference resolves to the outer value), instead of deferring the whole
//! query to the tree-walker.
//!
//! `query_vdbe` errors on any fallback to the tree-walker, so a successful
//! `query_vdbe` proves the query actually routed through the VDBE. Every case is
//! also checked against the tree-walker (`set_use_vdbe(false)`) and against the
//! hard-coded rows the real `sqlite3` 3.50.4 returns for the same query.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn i(n: i64) -> Value {
    Value::Integer(n)
}
fn t(s: &str) -> Value {
    Value::Text(s.into())
}

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x, k)").unwrap();
    c.execute("INSERT INTO a VALUES(1,10),(2,20),(3,30)")
        .unwrap();
    c.execute("CREATE TABLE b(p, v)").unwrap();
    c.execute("INSERT INTO b VALUES(10,'ten'),(20,'twenty'),(20,'again')")
        .unwrap();
    c
}

/// Run `sql` on the VDBE (must not fall back) and on the tree-walker, asserting
/// both equal `expected` (the rows real sqlite returns).
fn both(c: &Connection, sql: &str, expected: Vec<Vec<Value>>) {
    let v = c
        .query_vdbe(sql)
        .expect("must run on the VDBE (no fallback)");
    assert_eq!(v.rows, expected, "VDBE mismatch for `{sql}`");
    c.set_use_vdbe(false);
    let tw = c.query(sql).unwrap();
    c.set_use_vdbe(true);
    assert_eq!(tw.rows, expected, "tree-walker mismatch for `{sql}`");
}

#[test]
fn correlated_scalar_equality() {
    let c = setup();
    // First-matching-row value; a=30 has no match → NULL.
    both(
        &c,
        "SELECT x, (SELECT v FROM b WHERE b.p = a.k) FROM a",
        vec![
            vec![i(1), t("ten")],
            vec![i(2), t("twenty")],
            vec![i(3), Value::Null],
        ],
    );
}

#[test]
fn correlated_scalar_range_aggregate() {
    let c = setup();
    // A correlated range predicate under an aggregate scalar subquery.
    both(
        &c,
        "SELECT x, (SELECT count(*) FROM b WHERE b.p <= a.k) FROM a",
        vec![vec![i(1), i(1)], vec![i(2), i(3)], vec![i(3), i(3)]],
    );
}

#[test]
fn correlated_exists() {
    let c = setup();
    both(
        &c,
        "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.p = a.k)",
        vec![vec![i(1)], vec![i(2)]],
    );
}

#[test]
fn correlated_not_exists() {
    let c = setup();
    both(
        &c,
        "SELECT x FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.p = a.k)",
        vec![vec![i(3)]],
    );
}

#[test]
fn correlated_rowid_reference() {
    let c = setup();
    // The outer row's rowid is correlated into the inner predicate.
    both(
        &c,
        "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.p = a.k AND a.rowid < 3)",
        vec![vec![i(1)], vec![i(2)]],
    );
}

#[test]
fn correlated_scalar_in_where_predicate() {
    let c = setup();
    // A correlated scalar subquery used as a comparison operand in WHERE.
    both(
        &c,
        "SELECT x FROM a WHERE (SELECT v FROM b WHERE b.p = a.k) = 'twenty'",
        vec![vec![i(2)]],
    );
}

#[test]
fn correlated_null_and_empty_inner() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x, k)").unwrap();
    c.execute("INSERT INTO a VALUES(1,10),(2,NULL),(3,30)")
        .unwrap();
    c.execute("CREATE TABLE b(p, v)").unwrap();
    c.execute("INSERT INTO b VALUES(10,'ten'),(30,NULL)")
        .unwrap();
    // A NULL outer key matches nothing; a matched inner value that is itself NULL.
    both(
        &c,
        "SELECT x, (SELECT v FROM b WHERE b.p = a.k) FROM a",
        vec![
            vec![i(1), t("ten")],
            vec![i(2), Value::Null],
            vec![i(3), Value::Null],
        ],
    );
    both(
        &c,
        "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.p = a.k)",
        vec![vec![i(1)], vec![i(3)]],
    );
    both(
        &c,
        "SELECT x FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.p = a.k)",
        vec![vec![i(2)]],
    );
}

#[test]
fn correlated_subquery_over_inner_join() {
    let c = setup();
    // A correlated scalar subquery over an INNER join runs on the VDBE join path:
    // the interpreter assembles the combined multi-cursor row for the callback.
    both(
        &c,
        "SELECT a.x, (SELECT count(*) FROM b c WHERE c.p = a.k) \
         FROM a JOIN b ON a.k = b.p ORDER BY a.x, b.v",
        vec![vec![i(1), i(1)], vec![i(2), i(2)], vec![i(2), i(2)]],
    );
    // A correlated EXISTS in the WHERE of a join, referencing BOTH outer sources
    // (a.k and b.v) — exercises the full combined join row.
    both(
        &c,
        "SELECT a.x FROM a JOIN b ON a.k = b.p \
         WHERE EXISTS (SELECT 1 FROM b d WHERE d.p = a.k AND d.v <> b.v) ORDER BY a.x, b.v",
        vec![vec![i(2)], vec![i(2)]],
    );
}

#[test]
fn correlated_subquery_over_outer_join() {
    let c = setup();
    // A correlated scalar subquery over a LEFT join runs on the VDBE (the join is
    // materialized, then the subquery re-evaluates per combined row; an unmatched
    // left row still resolves its correlated outer column, e.g. a.k = 30 → 0).
    both(
        &c,
        "SELECT a.x, (SELECT count(*) FROM b c WHERE c.p = a.k) \
         FROM a LEFT JOIN b ON a.k = b.p ORDER BY a.x, b.v",
        vec![
            vec![i(1), i(1)],
            vec![i(2), i(2)],
            vec![i(2), i(2)],
            vec![i(3), i(0)],
        ],
    );
}

#[test]
fn grouped_correlated_scalar_in_projection() {
    let c = setup();
    // A correlated scalar subquery in a GROUP BY projection, correlated ONLY on the
    // group key (`a.k`): its value is well-defined per group, and the VDBE grouped
    // emit evaluates it against a synthetic row of the group's key values.
    both(
        &c,
        "SELECT k, (SELECT count(*) FROM b WHERE b.p = a.k) FROM a GROUP BY k",
        vec![vec![i(10), i(1)], vec![i(20), i(2)], vec![i(30), i(0)]],
    );
    // Alongside a real aggregate (each distinct key is its own single-row group).
    both(
        &c,
        "SELECT k, count(*), (SELECT count(*) FROM b WHERE b.p = a.k) FROM a GROUP BY k",
        vec![
            vec![i(10), i(1), i(1)],
            vec![i(20), i(1), i(2)],
            vec![i(30), i(1), i(0)],
        ],
    );
    // A correlated EXISTS in the projection.
    both(
        &c,
        "SELECT k, EXISTS(SELECT 1 FROM b WHERE b.p = a.k) FROM a GROUP BY k",
        vec![vec![i(10), i(1)], vec![i(20), i(1)], vec![i(30), i(0)]],
    );
    // A correlated scalar returning the first matching value (NULL when none).
    both(
        &c,
        "SELECT k, (SELECT v FROM b WHERE b.p = a.k) FROM a GROUP BY k",
        vec![
            vec![i(10), t("ten")],
            vec![i(20), t("twenty")],
            vec![i(30), Value::Null],
        ],
    );
}

#[test]
fn grouped_correlated_nonkey_ref_falls_back() {
    let c = setup();
    // A correlated subquery in a GROUP BY projection that references a NON-key
    // outer column (`a.x`) has no well-defined per-group value (the group's
    // representative row is unspecified), so the VDBE grouped emit must decline and
    // defer to the tree-walker — `query_vdbe` therefore errors, while the ordinary
    // `query` path still produces sqlite's result.
    let q = "SELECT k, (SELECT count(*) FROM b WHERE b.p = a.x) FROM a GROUP BY k";
    assert!(
        c.query_vdbe(q).is_err(),
        "a non-key correlated ref must not run on the VDBE grouped path"
    );
    // The tree-walker (via the auto-fallback `query`) still answers correctly.
    assert_eq!(
        c.query(q).unwrap().rows,
        vec![vec![i(10), i(0)], vec![i(20), i(0)], vec![i(30), i(0)],],
    );
}

#[test]
fn noncorrelated_subqueries_unregressed() {
    let c = setup();
    // A non-correlated scalar subquery still folds and runs on the VDBE.
    both(
        &c,
        "SELECT x FROM a WHERE k > (SELECT min(p) FROM b)",
        vec![vec![i(2)], vec![i(3)]],
    );
    // A non-correlated EXISTS folds too.
    both(
        &c,
        "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b WHERE p = 10)",
        vec![vec![i(1)], vec![i(2)], vec![i(3)]],
    );
}

#[test]
fn correlated_subquery_over_derived_and_cte_source() {
    let c = setup();
    // A correlated scalar subquery over a *derived-table* source runs on the VDBE
    // materialized single-source path (previously deferred): the derived row is the
    // outer scope, and `d.k` correlates into the inner predicate.
    both(
        &c,
        "SELECT x, (SELECT count(*) FROM b WHERE b.p = d.k) \
         FROM (SELECT * FROM a) d ORDER BY x",
        vec![vec![i(1), i(1)], vec![i(2), i(2)], vec![i(3), i(0)]],
    );
    // A group-key correlated subquery in a GROUP BY projection over a derived source.
    both(
        &c,
        "SELECT k, (SELECT count(*) FROM b WHERE b.p = d.k) \
         FROM (SELECT * FROM a) d GROUP BY k",
        vec![vec![i(10), i(1)], vec![i(20), i(2)], vec![i(30), i(0)]],
    );
    // A correlated EXISTS over a CTE source.
    both(
        &c,
        "WITH d AS (SELECT * FROM a) \
         SELECT x FROM d WHERE EXISTS (SELECT 1 FROM b WHERE b.p = d.k) ORDER BY x",
        vec![vec![i(1)], vec![i(2)]],
    );
}
