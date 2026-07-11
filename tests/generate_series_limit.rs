//! A single-argument `generate_series(START)` is unbounded (its default stop is
//! effectively infinite), so it is only useful with a `LIMIT`. graphite
//! materialises a table-valued function's rows eagerly, which used to run the
//! unbounded series to its 4-billion-row default before the `LIMIT` could apply —
//! hanging the query. When the query consumes exactly the first `OFFSET+LIMIT`
//! rows of its sole source in order, the series generation now stops at that
//! bound (`generate_series_scan_cap`), matching SQLite's streamed early-exit.
//! Column-metadata probes (window/subquery scope building) cap at zero rows.
//!
//! These are library-level assertions with well-defined expected values (the
//! locally-installed `sqlite3` may carry a non-standard `generate_series`
//! extension, so this does not compare against the CLI).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref v => panic!("non-int {v:?}"),
        })
        .collect()
}

#[test]
fn unbounded_series_with_limit_terminates() {
    let c = Connection::open_memory().unwrap();
    // The core case: an unbounded single-argument series bounded by LIMIT.
    assert_eq!(
        ints(&c, "SELECT value FROM generate_series(5) LIMIT 3"),
        [5, 6, 7]
    );
    assert_eq!(
        ints(&c, "SELECT value FROM generate_series(100) LIMIT 1"),
        [100]
    );
    // OFFSET is part of the bound.
    assert_eq!(
        ints(&c, "SELECT value FROM generate_series(5) LIMIT 3 OFFSET 2"),
        [7, 8, 9]
    );
    // A per-row projection expression does not disqualify the bound.
    assert_eq!(
        ints(&c, "SELECT value * 2 FROM generate_series(5) LIMIT 3"),
        [10, 12, 14]
    );
    // A non-integer first argument coerces to 0 and is still bounded.
    assert_eq!(
        ints(&c, "SELECT value FROM generate_series('x') LIMIT 4"),
        [0, 1, 2, 3]
    );
    // Nested inside a subquery the inner LIMIT still bounds it.
    assert_eq!(
        ints(
            &c,
            "SELECT max(value) FROM (SELECT value FROM generate_series(5) LIMIT 3)"
        ),
        [7]
    );
}

#[test]
fn bounded_series_and_star_are_unaffected() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        ints(&c, "SELECT value FROM generate_series(1, 5)"),
        [1, 2, 3, 4, 5]
    );
    assert_eq!(
        ints(&c, "SELECT value FROM generate_series(1, 10) LIMIT 3"),
        [1, 2, 3]
    );
    assert_eq!(
        ints(&c, "SELECT value FROM generate_series(2, 10, 2)"),
        [2, 4, 6, 8, 10]
    );
    // `SELECT *` over a bounded series still works (column metadata intact).
    assert_eq!(
        c.query("SELECT * FROM generate_series(1, 3)")
            .unwrap()
            .rows
            .len(),
        3
    );
    // A window/subquery scope over an unbounded series (its column metadata is
    // probed with a zero-row cap) no longer hangs.
    assert_eq!(
        ints(
            &c,
            "SELECT value FROM generate_series(1, 5) g \
             WHERE EXISTS (SELECT 1 FROM generate_series(1, 2) WHERE value = g.value)"
        ),
        [1, 2]
    );
}
