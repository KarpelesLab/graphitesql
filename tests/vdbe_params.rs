//! Track B (B5/B7): parameterized queries run on the VDBE engine. Explicit
//! params (`?N`, `:name`) are substituted into the compiled expressions so the
//! (otherwise param-less) VDBE can run them; the result must match the
//! tree-walker exactly. Anonymous `?` still falls back (eval-order indexing).

#![cfg(feature = "std")]

use graphitesql::exec::eval::Params;
use graphitesql::{Connection, Value};

fn rows_params(c: &Connection, sql: &str, ps: Vec<Value>) -> Vec<Vec<Value>> {
    let params = Params {
        positional: ps,
        named: Vec::new(),
    };
    c.query_params(sql, &params).unwrap().rows
}

#[test]
fn vdbe_runs_explicit_parameterized_queries() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER, b TEXT)").unwrap();
    for (a, b) in [(1, "x"), (2, "y"), (3, "x"), (4, "z")] {
        c.execute(&format!("INSERT INTO t VALUES ({a}, '{b}')"))
            .unwrap();
    }
    let cases: &[(&str, Vec<Value>)] = &[
        ("SELECT a FROM t WHERE a = ?1", vec![Value::Integer(3)]),
        (
            "SELECT a, b FROM t WHERE a > ?1 ORDER BY a",
            vec![Value::Integer(1)],
        ),
        (
            "SELECT a FROM t WHERE b = ?1 ORDER BY a",
            vec![Value::Text("x".into())],
        ),
        (
            "SELECT a FROM t ORDER BY a LIMIT ?1",
            vec![Value::Integer(2)],
        ),
        (
            "SELECT count(*) FROM t WHERE a >= ?1",
            vec![Value::Integer(2)],
        ),
        (
            "SELECT a FROM t WHERE a BETWEEN ?1 AND ?2 ORDER BY a",
            vec![Value::Integer(2), Value::Integer(3)],
        ),
    ];
    for (sql, ps) in cases {
        // VDBE (default) vs tree-walker must agree.
        c.set_use_vdbe(true);
        let vdbe = rows_params(&c, sql, ps.clone());
        c.set_use_vdbe(false);
        let walker = rows_params(&c, sql, ps.clone());
        c.set_use_vdbe(true);
        assert_eq!(vdbe, walker, "VDBE vs tree-walker diverged for: {sql}");
    }
}
