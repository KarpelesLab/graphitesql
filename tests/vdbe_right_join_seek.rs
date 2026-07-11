//! B1c: a two-table `RIGHT JOIN` with an explicit projection runs on the VDBE by
//! rewriting `a RIGHT JOIN b ON …` to the equivalent `b LEFT JOIN a ON …` (the
//! right table is the preserved side), which lets the seek path drive the
//! now-inner left table by rowid / unique index instead of materializing it. The
//! rewrite is a semantic identity, so the rows — including null-padding of the
//! left side for an unmatched right row — are byte-identical to the tree-walker
//! and to `sqlite3`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(k INTEGER PRIMARY KEY, x)")
        .unwrap();
    c.execute("INSERT INTO a VALUES(1,'a1'),(2,'a2'),(3,'a3')")
        .unwrap();
    c.execute("CREATE TABLE b(p, y)").unwrap();
    c.execute("INSERT INTO b VALUES(1,'b1'),(2,'b2'),(9,'b9')")
        .unwrap();
    c
}

/// VDBE (no fallback) == tree-walker == `expected`.
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

fn i(n: i64) -> Value {
    Value::Integer(n)
}
fn t(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn right_join_seek_matches_expected() {
    let c = setup();
    // The preserved right row `b9` (p=9) has no matching `a`, so `a.x` is NULL.
    both(
        &c,
        "SELECT a.x, b.y FROM a RIGHT JOIN b ON a.k = b.p ORDER BY b.p",
        vec![
            vec![t("a1"), t("b1")],
            vec![t("a2"), t("b2")],
            vec![Value::Null, t("b9")],
        ],
    );
    // A reordered projection (right columns first, plus the sought key) is correct
    // — output columns resolve by name regardless of the internal FROM swap.
    both(
        &c,
        "SELECT b.y, a.x, a.k FROM a RIGHT JOIN b ON a.k = b.p ORDER BY b.p",
        vec![
            vec![t("b1"), t("a1"), i(1)],
            vec![t("b2"), t("a2"), i(2)],
            vec![t("b9"), Value::Null, Value::Null],
        ],
    );
    // A WHERE over the joined row filters the null-padded row out.
    both(
        &c,
        "SELECT a.x FROM a RIGHT JOIN b ON a.k = b.p WHERE a.x IS NOT NULL ORDER BY b.p",
        vec![vec![t("a1")], vec![t("a2")]],
    );
}

#[test]
fn right_join_wildcard_runs_on_vdbe_with_correct_order() {
    // A bare `SELECT *` also runs on the VDBE: the swapped `(right, left)` columns
    // are rotated back to `(left, right)`, so the output order matches sqlite's
    // `(a.k, a.x, b.p, b.y)` exactly.
    let c = setup();
    let q = "SELECT * FROM a RIGHT JOIN b ON a.k = b.p ORDER BY b.p";
    let v = c
        .query_vdbe(q)
        .expect("SELECT * RIGHT join runs on the VDBE");
    assert_eq!(v.columns, vec!["k", "x", "p", "y"]);
    let expected = vec![
        vec![i(1), t("a1"), i(1), t("b1")],
        vec![i(2), t("a2"), i(2), t("b2")],
        vec![Value::Null, Value::Null, i(9), t("b9")],
    ];
    assert_eq!(v.rows, expected, "VDBE mismatch");
    c.set_use_vdbe(false);
    let tw = c.query(q).unwrap();
    c.set_use_vdbe(true);
    assert_eq!(tw.rows, expected, "tree-walker mismatch");
}
