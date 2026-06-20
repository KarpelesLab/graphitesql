//! Virtual-table constraint pushdown via `best_index` / `filter` (roadmap D1b).
//!
//! The example `series` module is graphite-specific (a real `sqlite3` would not
//! understand `USING series(…)`), so there is no differential cross-read here.
//! The correctness contract proven instead is the one that matters for pushdown:
//!
//! * results **with** pushdown == results **without** == the obvious expected rows
//!   (the executor re-applies the full `WHERE`, so a superset plan is always
//!   correct), and
//! * pushdown actually *happened* — demonstrated by querying a series whose full
//!   enumeration is astronomically large (a billion rows). If the constraints
//!   were not pushed into the module, the executor would try to enumerate the
//!   whole range and the test would never finish; a fast, correct answer is only
//!   possible because `filter` narrowed what the module generates.
//!
//! The direct, race-free proof that `filter` restricts generation (asserting the
//! cursor's generated-value count) lives in the `vtab` unit tests
//! (`filter_narrows_generation`).
#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn ints(conn: &Connection, sql: &str) -> Vec<i64> {
    conn.query(sql)
        .expect("query")
        .rows
        .into_iter()
        .map(|r| match r.into_iter().next() {
            Some(Value::Integer(i)) => i,
            other => panic!("expected an integer column, got {other:?}"),
        })
        .collect()
}

/// `WHERE value BETWEEN lo AND hi` over an enormous series returns just the
/// in-range rows. A full enumeration would be a billion rows; finishing at all
/// proves the BETWEEN bounds were pushed into the module's `filter`.
#[test]
fn between_pushdown_over_huge_series() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE big USING series(1, 1000000000)")
        .unwrap();
    assert_eq!(
        ints(&conn, "SELECT value FROM big WHERE value BETWEEN 3 AND 5"),
        vec![3, 4, 5]
    );
    // A half-open lower bound near the far end is just as cheap.
    assert_eq!(
        ints(
            &conn,
            "SELECT value FROM big WHERE value >= 999999998 AND value <= 1000000000"
        ),
        vec![999999998, 999999999, 1000000000]
    );
}

/// Equality on `value` collapses to a single row, even over a huge series.
#[test]
fn equality_pushdown_collapses_to_one_row() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE big USING series(1, 1000000000)")
        .unwrap();
    assert_eq!(
        ints(&conn, "SELECT value FROM big WHERE value = 500000000"),
        vec![500000000]
    );
    // An equality off the (step-1) grid is impossible here, but on a stepped
    // series it must yield nothing rather than an invented off-grid row.
    conn.execute("CREATE VIRTUAL TABLE evens USING series(0, 1000000000, 2)")
        .unwrap();
    assert!(ints(&conn, "SELECT value FROM evens WHERE value = 3").is_empty());
    assert_eq!(
        ints(&conn, "SELECT value FROM evens WHERE value = 4"),
        vec![4]
    );
}

/// Half-open ranges (`>=`, `>`, `<=`, `<`) push their single bound; the
/// re-applied `WHERE` still enforces strictness exactly.
#[test]
fn half_open_ranges() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 10)")
        .unwrap();
    assert_eq!(
        ints(&conn, "SELECT value FROM v WHERE value >= 8"),
        vec![8, 9, 10]
    );
    assert_eq!(
        ints(&conn, "SELECT value FROM v WHERE value > 8"),
        vec![9, 10]
    );
    assert_eq!(
        ints(&conn, "SELECT value FROM v WHERE value <= 3"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(&conn, "SELECT value FROM v WHERE value < 3"),
        vec![1, 2]
    );
}

/// The no-constraint full scan still works (pushdown falls back to the default
/// plan), and equals the obvious enumeration.
#[test]
fn full_scan_without_constraints() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 6)")
        .unwrap();
    assert_eq!(ints(&conn, "SELECT value FROM v"), vec![1, 2, 3, 4, 5, 6]);
    // count(*) over the whole scan.
    assert_eq!(ints(&conn, "SELECT count(*) FROM v"), vec![6]);
}

/// A constraint the module does NOT consume (`value % 3 = 0`, not a recognized
/// `col <op> const` shape) must still filter correctly — the executor's WHERE is
/// always the source of truth.
#[test]
fn unconsumed_constraint_still_filters() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE v USING series(1, 12)")
        .unwrap();
    // `value % 3 = 0` is not a pushable comparison, so the module full-scans and
    // run_core filters — the result must be exactly the multiples of three.
    assert_eq!(
        ints(&conn, "SELECT value FROM v WHERE value % 3 = 0"),
        vec![3, 6, 9, 12]
    );
    // Mixing a pushable bound with an unconsumed predicate: the bound narrows the
    // scan, the modulo is still applied by run_core.
    assert_eq!(
        ints(
            &conn,
            "SELECT value FROM v WHERE value >= 4 AND value % 3 = 0"
        ),
        vec![6, 9, 12]
    );
}

/// Pushdown == no-pushdown == expected, the central superset invariant, checked
/// across a spread of predicates and a descending series.
#[test]
fn pushdown_matches_plain_scan() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE VIRTUAL TABLE asc USING series(1, 20)")
        .unwrap();
    conn.execute("CREATE VIRTUAL TABLE desc USING series(20, 1, -1)")
        .unwrap();

    // Descending series with a BETWEEN: rows come back in descending order, and
    // only the in-range ones.
    assert_eq!(
        ints(&conn, "SELECT value FROM desc WHERE value BETWEEN 5 AND 8"),
        vec![8, 7, 6, 5]
    );

    // Compare a constrained query against the manual expectation derived from the
    // unconstrained full scan, for several predicates.
    let all: Vec<i64> = ints(&conn, "SELECT value FROM asc");
    type Keep = fn(i64) -> bool;
    let cases: &[(&str, Keep)] = &[
        ("value >= 15", |v| v >= 15),
        ("value > 15", |v| v > 15),
        ("value <= 4", |v| v <= 4),
        ("value < 4", |v| v < 4),
        ("value = 11", |v| v == 11),
        ("value BETWEEN 7 AND 9", |v| (7..=9).contains(&v)),
    ];
    for (pred, keep) in cases {
        let expected: Vec<i64> = all.iter().copied().filter(|v| keep(*v)).collect();
        let got = ints(&conn, &format!("SELECT value FROM asc WHERE {pred}"));
        assert_eq!(got, expected, "predicate `{pred}`");
    }
}
