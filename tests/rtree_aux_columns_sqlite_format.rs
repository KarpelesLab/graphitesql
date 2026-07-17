//! An aux-column (`+col`) R-Tree persists in SQLite's byte-compatible
//! `_node`/`_parent`/`_rowid` shadow-table format — the `_rowid` table widened
//! with one `aK` column per aux column — instead of graphite's generic
//! `<name>_data` backing. This is what makes a graphite-created aux R-Tree
//! readable by stock `sqlite3` (verified out-of-band in the differential CI
//! corpus); here we assert the graphite-visible half: the shadow-table set, the
//! `_rowid(rowid,nodeno,a0,…)` schema, the aux round-trip across
//! insert/update/delete, and `PRAGMA integrity_check = ok`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

fn table_names(c: &Connection) -> Vec<String> {
    c.query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("expected text name, got {other:?}"),
        })
        .collect()
}

#[test]
fn aux_rtree_uses_sqlite_shadow_tables_not_data() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, minx, maxx, miny, maxy, +label TEXT, +score INTEGER)")
        .unwrap();
    // The byte-compatible node shadows exist; the generic `_data` fallback does not.
    assert_eq!(
        table_names(&c),
        vec![
            "r".to_string(),
            "r_node".to_string(),
            "r_parent".to_string(),
            "r_rowid".to_string(),
        ]
    );
    assert!(c.query("SELECT * FROM r_data").is_err());
    // `_rowid` is widened with one `aK` column per aux column: `rowid,nodeno,a0,a1`.
    let info = c.query("PRAGMA table_info(r_rowid)").unwrap();
    let cols: Vec<String> = info
        .rows
        .iter()
        .map(|r| match &r[1] {
            Value::Text(s) => s.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(cols, vec!["rowid", "nodeno", "a0", "a1"]);
}

#[test]
fn aux_values_round_trip_through_dml() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, minx, maxx, +label TEXT, +score INTEGER)")
        .unwrap();
    c.execute(
        "INSERT INTO r VALUES (1, 0,1, 'hello', 7), (2, 5,6, 'world', 42), (3, 10,11, 'foo', 99)",
    )
    .unwrap();
    // Aux values stored verbatim in `_rowid`'s `a0`/`a1`.
    assert_eq!(
        rows(&c, "SELECT id, label, score FROM r ORDER BY id"),
        [
            vec![
                Value::Integer(1),
                Value::Text("hello".into()),
                Value::Integer(7)
            ],
            vec![
                Value::Integer(2),
                Value::Text("world".into()),
                Value::Integer(42)
            ],
            vec![
                Value::Integer(3),
                Value::Text("foo".into()),
                Value::Integer(99)
            ],
        ]
    );
    // UPDATE rewrites the aux row.
    c.execute("UPDATE r SET label='WORLD', score=43 WHERE id=2")
        .unwrap();
    assert_eq!(
        rows(&c, "SELECT label, score FROM r WHERE id=2"),
        [vec![Value::Text("WORLD".into()), Value::Integer(43)]]
    );
    // DELETE drops the node cell and the `_rowid` aux row together.
    c.execute("DELETE FROM r WHERE id=3").unwrap();
    assert_eq!(
        rows(&c, "SELECT id FROM r ORDER BY id"),
        [vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    // A spatial prune combined with an aux predicate returns the right row.
    assert_eq!(
        rows(
            &c,
            "SELECT id FROM r WHERE minx >= 4 AND maxx <= 7 AND label = 'WORLD'"
        ),
        [vec![Value::Integer(2)]]
    );
    // The database is valid.
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn integer_rtree_with_aux_columns() {
    // `rtree_i32` also persists aux columns in the sqlite `_rowid` shadow.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree_i32(id, x0, x1, +tag TEXT)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0,10, 'x'), (2, 20,30, 'y')")
        .unwrap();
    assert!(c.query("SELECT * FROM r_data").is_err());
    assert_eq!(
        rows(&c, "SELECT id, tag FROM r ORDER BY id"),
        [
            vec![Value::Integer(1), Value::Text("x".into())],
            vec![Value::Integer(2), Value::Text("y".into())],
        ]
    );
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}
