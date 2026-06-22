//! Anonymous `?` parameters are numbered by PARSE POSITION (SQLite's rule), not
//! by evaluation order — so AND/OR short-circuit can no longer mis-map which
//! bound value a `?` receives. Each parameterized query must return exactly what
//! its literal-substituted form returns (which is itself differentially clean
//! against sqlite3). Covers both the tree-walker and the VDBE engine.

#![cfg(feature = "std")]

use graphitesql::exec::eval::Params;
use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str, ps: &[i64]) -> Vec<Vec<Value>> {
    let params = Params {
        positional: ps.iter().map(|i| Value::Integer(*i)).collect(),
        named: Vec::new(),
    };
    c.query_params(sql, &params).unwrap().rows
}

#[test]
fn anonymous_params_numbered_by_position() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b,c)").unwrap();
    c.execute("INSERT INTO t VALUES (1,2,3),(9,2,3),(1,9,3),(1,2,9),(5,5,5)")
        .unwrap();
    // (param SQL, bound values, literal-equivalent SQL)
    let cases: &[(&str, &[i64], &str)] = &[
        // OR short-circuit used to consume the wrong index for the trailing `?`.
        (
            "SELECT a,b,c FROM t WHERE (a=? OR b=?) AND c=? ORDER BY a,b,c",
            &[1, 2, 3],
            "SELECT a,b,c FROM t WHERE (a=1 OR b=2) AND c=3 ORDER BY a,b,c",
        ),
        // AND short-circuit on the left arm.
        (
            "SELECT a FROM t WHERE (a=? AND b=?) OR c=? ORDER BY a",
            &[1, 2, 9],
            "SELECT a FROM t WHERE (a=1 AND b=2) OR c=9 ORDER BY a",
        ),
        // Repeated structure, several anon params.
        (
            "SELECT a,b FROM t WHERE a=? OR b=? OR c=? ORDER BY a,b",
            &[5, 9, 9],
            "SELECT a,b FROM t WHERE a=5 OR b=9 OR c=9 ORDER BY a,b",
        ),
        // Single anon param (regression: unchanged).
        (
            "SELECT a FROM t WHERE a=? ORDER BY a",
            &[1],
            "SELECT a FROM t WHERE a=1 ORDER BY a",
        ),
    ];
    for vdbe in [false, true] {
        c.set_use_vdbe(vdbe);
        for (psql, vals, lit) in cases {
            let got = rows(&c, psql, vals);
            let want = c.query(lit).unwrap().rows;
            assert_eq!(got, want, "use_vdbe={vdbe}, query: {psql} with {vals:?}");
        }
    }
}
