//! Coverage for the percentile aggregate family — `median(Y)`,
//! `percentile(Y,P)`, `percentile_cont(Y,P)`, and `percentile_disc(Y,P)` — a
//! port of SQLite's `ext/misc/percentile.c` (compiled into the sqlite3 3.50.4
//! CLI, so it is the differential oracle here).
//!
//! Each collects the non-NULL numeric inputs, sorts them, and reads off the
//! value at fractional position `P·(N−1)`; the continuous forms interpolate,
//! the discrete form returns the lower straddling sample. The result is always a
//! REAL (or NULL for no input). All four also work as window functions.
//!
//! The first test diffs a broad matrix (values, errors, GROUP BY, windows) byte
//! for byte against the real CLI; the second asserts exact library values and
//! error-message strings in-process (no CLI needed).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run one (possibly multi-statement) SQL string through a CLI, capturing both
/// stdout and stderr so value output *and* error text are diffed.
fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s
}

/// Value results, GROUP BY, typeof, mixed/negative inputs, and every window
/// shape — plus the error paths (non-numeric value, out-of-range fraction,
/// per-row-varying fraction, wrong arity). Each is diffed against the CLI.
const QUERIES: &[&str] = &[
    // --- median ---
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4);SELECT median(x) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3);SELECT median(x) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(42);SELECT median(x) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(NULL),(NULL);SELECT median(x) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(NULL),(7),(NULL);SELECT median(x) FROM t;",
    "CREATE TABLE t(x);SELECT median(x) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2.5),(3),(4.5);SELECT median(x) FROM t;",
    // --- percentile / cont / disc ---
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,0) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,100) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,25) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,50) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,75) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile_cont(x,0.5) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,50)=percentile_cont(x,0.5) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile_disc(x,0.5) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile_disc(x,0.25) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5),(6),(7),(8),(9),(10);SELECT percentile(x,33.333) FROM t;",
    // --- typeof: always real (even percentile_disc, which returns an input) ---
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT typeof(median(x)) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT typeof(percentile(x,50)) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT typeof(percentile_cont(x,0.5)),typeof(percentile_disc(x,0.5)) FROM t;",
    // --- negatives, reals, larger N ---
    "CREATE TABLE t(x);INSERT INTO t VALUES(-5),(-1),(-3),(-2);SELECT median(x),percentile(x,25) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1.1),(2.2),(3.3),(4.4);SELECT median(x),percentile(x,10),percentile(x,90) FROM t;",
    "WITH r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<100) SELECT median(n),percentile(n,90) FROM r;",
    // --- fraction argument as (numeric-coercible) text is accepted ---
    "CREATE TABLE t(x,p TEXT);INSERT INTO t VALUES(1,'50'),(2,'50'),(3,'50');SELECT percentile(x,p) FROM t;",
    // --- value storage class: INTEGER-affinity text is stored numeric (ok) ---
    "CREATE TABLE t(x INTEGER);INSERT INTO t VALUES('1'),('2'),('3');SELECT median(x) FROM t;",
    // --- GROUP BY ---
    "CREATE TABLE t(g,x);INSERT INTO t VALUES(1,10),(1,20),(1,30),(2,5),(2,15);SELECT g,median(x) FROM t GROUP BY g;",
    "CREATE TABLE t(g,x);INSERT INTO t VALUES(1,10),(1,20),(1,30),(2,5),(2,15);SELECT g,percentile(x,50) FROM t GROUP BY g;",
    "CREATE TABLE t(g,x);INSERT INTO t VALUES(1,10),(1,20),(2,5);SELECT g,median(x) FROM t GROUP BY g HAVING median(x)>10;",
    // --- FILTER + DISTINCT ---
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5),(6);SELECT median(x) FILTER(WHERE x>2) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(1),(2),(3),(3);SELECT median(DISTINCT x) FROM t;",
    // --- windows ---
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT x, median(x) OVER () FROM t ORDER BY x;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT x, median(x) OVER (ORDER BY x ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY x;",
    "CREATE TABLE t(g,x);INSERT INTO t VALUES(1,10),(1,20),(1,30),(2,5),(2,15);SELECT g,x,percentile(x,50) OVER (PARTITION BY g) FROM t ORDER BY g,x;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT x, percentile_disc(x,0.5) OVER () FROM t ORDER BY x;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT x, percentile_cont(x,0.25) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY x;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4);SELECT x, median(x) FILTER(WHERE x<>2) OVER () FROM t ORDER BY x;",
    // --- error paths ---
    "CREATE TABLE t(x);INSERT INTO t VALUES('a'),('b');SELECT percentile(x,50) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES('a');SELECT median(x) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(x'31'),(x'32');SELECT median(x) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,200) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,-5) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile_cont(x,2) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile_disc(x,2) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x,'z') FROM t;",
    "CREATE TABLE t(x,p);INSERT INTO t VALUES(1,50),(2,60),(3,50);SELECT percentile(x,p) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(9e999),(1);SELECT median(x) FROM t;",
    // --- arity errors ---
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT median(x,2) FROM t;",
    "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3),(4),(5);SELECT percentile(x) FROM t;",
];

#[test]
fn percentile_functions_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping differential test");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in QUERIES {
        assert_eq!(out("sqlite3", q), out(g, q), "query mismatch: {q}");
    }
}

/// The library's own values and error strings, asserted directly (no CLI). This
/// pins behavior even where the oracle is unavailable.
#[test]
fn percentile_library_values_and_errors() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3),(4)").unwrap();

    // median of {1,2,3,4} = 2.5, and it is a REAL.
    assert_eq!(
        c.query("SELECT median(x) FROM t").unwrap().rows[0][0],
        Value::Real(2.5)
    );
    // percentile_disc returns an actual input, but still typed REAL.
    assert_eq!(
        c.query("SELECT percentile_disc(x,0.5) FROM t")
            .unwrap()
            .rows[0][0],
        Value::Real(2.0)
    );
    // percentile(_,50) == percentile_cont(_,0.5).
    assert_eq!(
        c.query("SELECT percentile(x,50) FROM t").unwrap().rows[0][0],
        Value::Real(2.5)
    );
    // Endpoints.
    assert_eq!(
        c.query("SELECT percentile(x,0),percentile(x,100) FROM t")
            .unwrap()
            .rows[0],
        vec![Value::Real(1.0), Value::Real(4.0)]
    );

    // All-NULL / empty → NULL.
    c.execute("CREATE TABLE n(x)").unwrap();
    c.execute("INSERT INTO n VALUES(NULL),(NULL)").unwrap();
    assert_eq!(
        c.query("SELECT median(x) FROM n").unwrap().rows[0][0],
        Value::Null
    );

    // --- exact error-message strings (match ext/misc/percentile.c) ---
    // `Display` prefixes runtime errors with "error: "; strip it so the assert
    // pins the message text itself (the byte-exact CLI rendering is covered by
    // the differential test above).
    let err = |c: &mut Connection, sql: &str| {
        c.query(sql)
            .unwrap_err()
            .to_string()
            .trim_start_matches("error: ")
            .to_string()
    };
    assert_eq!(
        err(
            &mut c,
            "SELECT percentile(x,50) FROM (SELECT 'a' AS x UNION ALL SELECT 'b')"
        ),
        "input to percentile() is not numeric"
    );
    assert_eq!(
        err(&mut c, "SELECT median(x) FROM (SELECT 'a' AS x)"),
        "input to median() is not numeric"
    );
    assert_eq!(
        err(&mut c, "SELECT percentile(x,200) FROM t"),
        "the fraction argument to percentile() is not between 0.0 and 100.0"
    );
    assert_eq!(
        err(&mut c, "SELECT percentile_cont(x,2) FROM t"),
        "the fraction argument to percentile_cont() is not between 0.0 and 1.0"
    );
    assert_eq!(
        err(
            &mut c,
            "SELECT percentile(x,p) FROM (SELECT 1 AS x,50 AS p UNION ALL SELECT 2,60)"
        ),
        "the fraction argument to percentile() is not the same for all input rows"
    );
}
