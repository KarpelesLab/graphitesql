//! The `*` wildcard is a legal function argument only for `count(*)`. Every
//! other function — aggregate or scalar — rejects it at prepare time with
//! `wrong number of arguments to function NAME()` (even over an empty input,
//! before any row is evaluated). graphite used to accept the aggregate forms
//! silently (running `sum(*)` as a no-arg aggregate) and reported a different
//! message (`abs(*) is not a scalar call`) for the scalar forms.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn err(sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.query(sql)
        .map(|_| ())
        .map_err(|e| {
            e.to_string()
                .trim_start_matches("error: ")
                .trim_start_matches("SQL error: ")
                .to_string()
        })
        .expect_err(sql)
}

fn arity(name: &str, sql: &str) {
    assert_eq!(
        err(sql),
        format!("wrong number of arguments to function {name}()"),
        "{sql:?}"
    );
}

#[test]
fn star_on_aggregates_other_than_count_is_rejected() {
    arity("sum", "SELECT sum(*) FROM t");
    arity("total", "SELECT total(*) FROM t");
    arity("avg", "SELECT avg(*) FROM t");
    arity("min", "SELECT min(*) FROM t");
    arity("max", "SELECT max(*) FROM t");
    arity("group_concat", "SELECT group_concat(*) FROM t");
}

#[test]
fn star_on_scalar_functions_is_rejected() {
    arity("abs", "SELECT abs(*) FROM t");
    arity("length", "SELECT length(*) FROM t");
    arity("upper", "SELECT upper(*) FROM t");
    arity("typeof", "SELECT typeof(*) FROM t");
}

#[test]
fn star_is_rejected_in_every_clause_position() {
    arity("abs", "SELECT a FROM t WHERE abs(*) > 0");
    arity("abs", "SELECT a FROM t GROUP BY abs(*)");
    arity("abs", "SELECT a FROM t ORDER BY abs(*)");
    // In a genuine aggregate context (GROUP BY) the HAVING arity error fires.
    arity("sum", "SELECT a FROM t GROUP BY a HAVING sum(*) > 0");
}

#[test]
fn having_validity_outranks_star_arity_in_a_non_aggregate_query() {
    // No GROUP BY and no result-column aggregate: SQLite rejects the HAVING
    // clause itself before it ever looks at the `*` arity.
    assert_eq!(
        err("SELECT a FROM t HAVING sum(*) > 0"),
        "HAVING clause on a non-aggregate query"
    );
}

#[test]
fn count_star_stays_valid() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 2), (3, 4)").unwrap();
    let ok = |sql: &str| c.query(sql).unwrap_or_else(|e| panic!("{sql:?}: {e}"));
    ok("SELECT count(*) FROM t");
    ok("SELECT count(*) FROM t HAVING count(*) > 0");
    // `count()` (no args) is also accepted, as a synonym for `count(*)`.
    ok("SELECT count() FROM t");
}
