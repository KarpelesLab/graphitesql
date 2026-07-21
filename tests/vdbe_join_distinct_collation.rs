//! VDBE track: `SELECT DISTINCT` over a LEFT / FULL / multi-table-LEFT join with a
//! non-BINARY collation (a declared `COLLATE NOCASE` column or an explicit
//! projection `COLLATE`). The join `DistinctCheck` now carries the per-output
//! collations (`distinct_collations`), so these dedup under their collation on the
//! VDBE instead of deferring to the tree-walker. `query_vdbe` forces the VDBE.
#![cfg(feature = "std")]
use graphitesql::{Connection, Value};

fn texts(c: &Connection, sql: &str) -> Vec<String> {
    c.query_vdbe(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.to_string(),
            Value::Null => "<null>".to_string(),
            o => panic!("not text: {o:?}"),
        })
        .collect()
}

#[test]
fn distinct_collate_over_left_full_and_n_joins() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x TEXT)").unwrap();
    c.execute("CREATE TABLE b(x TEXT)").unwrap();
    c.execute("CREATE TABLE d(x TEXT)").unwrap();
    c.execute("INSERT INTO a VALUES('P'),('p'),('q')").unwrap();
    c.execute("INSERT INTO b VALUES('p'),('r')").unwrap();
    c.execute("INSERT INTO d VALUES('p')").unwrap();

    // LEFT JOIN: explicit COLLATE NOCASE dedups P/p.
    assert_eq!(
        texts(
            &c,
            "SELECT DISTINCT a.x COLLATE NOCASE FROM a LEFT JOIN b ON a.x=b.x ORDER BY 1"
        ),
        ["P", "q"]
    );
    // FULL OUTER JOIN: the null-padded right side contributes its own group.
    assert_eq!(
        texts(
            &c,
            "SELECT DISTINCT a.x COLLATE NOCASE FROM a FULL OUTER JOIN b ON a.x=b.x ORDER BY 1"
        ),
        ["<null>", "P", "q"]
    );
    // Multi-table LEFT JOIN.
    assert_eq!(
        texts(
            &c,
            "SELECT DISTINCT a.x COLLATE NOCASE FROM a LEFT JOIN b ON a.x=b.x \
             LEFT JOIN d ON a.x=d.x ORDER BY 1"
        ),
        ["P", "q"]
    );
}

#[test]
fn distinct_declared_nocase_column_over_join() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT COLLATE NOCASE)").unwrap();
    c.execute("CREATE TABLE u(a TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES('A'),('a'),('B')").unwrap();
    c.execute("INSERT INTO u VALUES('a')").unwrap();
    // The declared NOCASE collation on t.a rides through the join's DistinctCheck.
    assert_eq!(
        texts(
            &c,
            "SELECT DISTINCT t.a FROM t LEFT JOIN u ON t.a=u.a ORDER BY 1"
        ),
        ["A", "B"]
    );
    assert_eq!(
        texts(
            &c,
            "SELECT DISTINCT t.a FROM t FULL OUTER JOIN u ON t.a=u.a ORDER BY 1"
        ),
        ["A", "B"]
    );
}
