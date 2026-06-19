//! Phase 9: CREATE VIEW / querying views / DROP VIEW.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, kind TEXT, amount INT)")
        .unwrap();
    c.execute(
        "INSERT INTO t(kind, amount) VALUES \
         ('a', 10), ('b', 20), ('a', 30), ('b', 40), ('a', 50)",
    )
    .unwrap();
    c
}

#[test]
fn query_view_with_filter() {
    let mut c = setup();
    c.execute("CREATE VIEW a_only AS SELECT id, amount FROM t WHERE kind = 'a'")
        .unwrap();

    let r = c
        .query("SELECT amount FROM a_only ORDER BY amount")
        .unwrap();
    let amounts: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(amounts, vec![10, 30, 50]);

    // Further filtering and aggregation on top of the view.
    let r = c
        .query("SELECT count(*), sum(amount) FROM a_only WHERE amount > 10")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(2));
    assert_eq!(r.rows[0][1], Value::Integer(80));
}

#[test]
fn view_with_explicit_columns_and_drop() {
    let mut c = setup();
    c.execute("CREATE VIEW totals(k, total) AS SELECT kind, sum(amount) FROM t GROUP BY kind")
        .unwrap();

    let r = c.query("SELECT k, total FROM totals ORDER BY k").unwrap();
    assert_eq!(r.columns, vec!["k", "total"]);
    assert_eq!(r.rows[0][0], Value::Text("a".into()));
    assert_eq!(r.rows[0][1], Value::Integer(90)); // 10+30+50
    assert_eq!(r.rows[1][0], Value::Text("b".into()));
    assert_eq!(r.rows[1][1], Value::Integer(60)); // 20+40

    c.execute("DROP VIEW totals").unwrap();
    assert!(c.query("SELECT * FROM totals").is_err());
}

#[test]
fn view_in_join() {
    let mut c = setup();
    c.execute("CREATE TABLE k(kind TEXT, label TEXT)").unwrap();
    c.execute("INSERT INTO k VALUES ('a','Apple'),('b','Banana')")
        .unwrap();
    c.execute("CREATE VIEW a_only AS SELECT id, kind, amount FROM t WHERE kind = 'a'")
        .unwrap();
    // Join a view against a base table (the previously-unsupported case).
    let r = c
        .query(
            "SELECT a_only.amount, k.label FROM a_only \
             JOIN k ON a_only.kind = k.kind ORDER BY a_only.amount",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0][0], Value::Integer(10));
    assert_eq!(r.rows[0][1], Value::Text("Apple".into()));
    // And a view as the joined (right) side.
    let r = c
        .query("SELECT count(*) FROM k JOIN a_only ON k.kind = a_only.kind")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(3));
}

#[test]
fn create_view_if_not_exists() {
    let mut c = setup();
    c.execute("CREATE VIEW v AS SELECT id FROM t").unwrap();
    // Re-creating without IF NOT EXISTS errors; with it, no-op.
    assert!(c.execute("CREATE VIEW v AS SELECT id FROM t").is_err());
    c.execute("CREATE VIEW IF NOT EXISTS v AS SELECT id FROM t")
        .unwrap();
}
