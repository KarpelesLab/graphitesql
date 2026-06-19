//! Phase 9: multi-table INNER / LEFT joins.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    c.execute("CREATE TABLE orders(id INTEGER PRIMARY KEY, user_id INT, amount INT)")
        .unwrap();
    c.execute("INSERT INTO users(id, name) VALUES (1,'ada'),(2,'grace'),(3,'edsger')")
        .unwrap();
    // ada has 2 orders, grace 1, edsger none.
    c.execute("INSERT INTO orders(user_id, amount) VALUES (1,10),(1,20),(2,30)")
        .unwrap();
    c
}

#[test]
fn inner_join_on() {
    let c = setup();
    let r = c
        .query(
            "SELECT users.name, orders.amount FROM users JOIN orders ON users.id = orders.user_id \
             ORDER BY orders.amount",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3); // edsger excluded (no orders)
    assert_eq!(r.rows[0][0], Value::Text("ada".into()));
    assert_eq!(r.rows[0][1], Value::Integer(10));
    assert_eq!(r.rows[2][0], Value::Text("grace".into()));
    assert_eq!(r.rows[2][1], Value::Integer(30));
}

#[test]
fn inner_join_with_aggregate() {
    let c = setup();
    let r = c
        .query(
            "SELECT users.name, sum(orders.amount) AS total \
             FROM users JOIN orders ON users.id = orders.user_id \
             GROUP BY users.name ORDER BY total DESC",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], Value::Text("ada".into()));
    assert_eq!(r.rows[0][1], Value::Integer(30)); // 10 + 20
    assert_eq!(r.rows[1][0], Value::Text("grace".into()));
    assert_eq!(r.rows[1][1], Value::Integer(30));
}

#[test]
fn left_join_keeps_unmatched() {
    let c = setup();
    let r = c
        .query(
            "SELECT users.name, orders.amount FROM users LEFT JOIN orders \
             ON users.id = orders.user_id ORDER BY users.name, orders.amount",
        )
        .unwrap();
    // ada(10), ada(20), edsger(NULL), grace(30) -> 4 rows
    assert_eq!(r.rows.len(), 4);
    // edsger has a NULL amount from the left join.
    let edsger = r
        .rows
        .iter()
        .find(|row| row[0] == Value::Text("edsger".into()))
        .unwrap();
    assert_eq!(edsger[1], Value::Null);
}

#[test]
fn comma_join_is_cross_product_filtered_by_where() {
    let c = setup();
    let r = c
        .query(
            "SELECT users.name, orders.amount FROM users, orders \
             WHERE users.id = orders.user_id AND orders.amount >= 20 ORDER BY orders.amount",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2); // amounts 20 and 30
    assert_eq!(r.rows[0][1], Value::Integer(20));
    assert_eq!(r.rows[1][1], Value::Integer(30));
}

#[test]
fn aliased_join() {
    let c = setup();
    let r = c
        .query(
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id \
             WHERE o.amount = 30",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("grace".into()));
}
