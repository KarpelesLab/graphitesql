//! Roadmap D3a: the built-in `rtree` virtual-table module (on top of the W1/W2
//! writable+persistent vtab infrastructure). Functionally correct spatial index —
//! rows persist in the backing table and queries are answered by scan + the
//! re-applied WHERE. Coordinates are stored as 32-bit floats (min rounded down,
//! max rounded up) and the id as an integer, byte-for-byte like sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn spatial_filter_and_rowid_alias() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, minX, maxX, minY, maxY)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0,1, 0,1), (2, 5,6, 5,6), (3, 0.5,2, 0.5,2)")
        .unwrap();

    // An overlap query returns the boxes that intersect the search window.
    assert_eq!(
        rows(
            &c,
            "SELECT id FROM r WHERE minX <= 1.5 AND maxX >= 0.5 ORDER BY id"
        ),
        [vec![Value::Integer(1)], vec![Value::Integer(3)]]
    );
    // The first column is the rowid.
    assert_eq!(
        rows(&c, "SELECT rowid, id FROM r WHERE id = 2"),
        [vec![Value::Integer(2), Value::Integer(2)]]
    );
    // Integer-valued coordinates read back as REAL.
    assert_eq!(
        rows(&c, "SELECT minX, maxX FROM r WHERE id = 1"),
        [vec![Value::Real(0.0), Value::Real(1.0)]]
    );
}

#[test]
fn coordinates_round_to_f32_like_sqlite() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, lo, hi)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (10, 0.1, 0.3)").unwrap();
    // min `0.1` rounds DOWN to the f32 below it, max `0.3` rounds UP — the exact
    // values sqlite3 3.50.4 stores and returns.
    assert_eq!(
        rows(&c, "SELECT lo, hi FROM r"),
        [vec![
            Value::Real(0.09999998658895493),
            Value::Real(0.30000001192092896),
        ]]
    );
}

#[test]
fn update_and_delete() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0,10), (2, 20,30)")
        .unwrap();
    c.execute("UPDATE r SET b = 100 WHERE id = 1").unwrap();
    assert_eq!(
        rows(&c, "SELECT id, b FROM r WHERE id = 1"),
        [vec![Value::Integer(1), Value::Real(100.0)]]
    );
    c.execute("DELETE FROM r WHERE id = 2").unwrap();
    assert_eq!(
        rows(&c, "SELECT id FROM r ORDER BY id"),
        [vec![Value::Integer(1)]]
    );
}

#[test]
fn rejects_min_greater_than_max_and_bad_arity() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    // min > max is rejected (like sqlite's "rtree constraint failed").
    assert!(c.execute("INSERT INTO r VALUES (1, 5, 2)").is_err());
    // An even column count (no id + 2N coordinates) is rejected.
    assert!(c
        .execute("CREATE VIRTUAL TABLE bad USING rtree(id, a)")
        .is_err());
}

#[test]
fn rows_persist_in_the_backing_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (7, 1, 2)").unwrap();
    // The persistent backing table really holds the row.
    assert_eq!(
        rows(&c, "SELECT id, a, b FROM r_data"),
        [vec![Value::Integer(7), Value::Real(1.0), Value::Real(2.0)]]
    );
}

#[test]
fn alter_and_index_on_a_virtual_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5), (2, 10, 15)")
        .unwrap();

    // ADD COLUMN / CREATE INDEX on a vtab are rejected (matching sqlite), not the
    // old confusing "schema sql is not CREATE TABLE".
    assert!(c.execute("ALTER TABLE r ADD COLUMN z").is_err());
    assert!(c.execute("CREATE INDEX i ON r(a)").is_err());

    // RENAME works: the vtab and its `<name>_data` backing table are both renamed,
    // and the rows survive.
    c.execute("ALTER TABLE r RENAME TO r2").unwrap();
    assert_eq!(
        c.query("SELECT id, a, b FROM r2 ORDER BY id").unwrap().rows,
        [
            vec![Value::Integer(1), Value::Real(0.0), Value::Real(5.0)],
            vec![Value::Integer(2), Value::Real(10.0), Value::Real(15.0)],
        ]
    );
    // The old name is gone; the backing table moved too.
    assert!(c.query("SELECT * FROM r").is_err());
    assert!(c.query("SELECT * FROM r2_data").is_ok());
}

#[test]
fn drop_removes_the_backing_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5)").unwrap();
    assert!(c.query("SELECT * FROM r_data").is_ok());
    c.execute("DROP TABLE r").unwrap();
    // Both the vtab and its backing table are gone.
    assert!(c.query("SELECT * FROM r").is_err());
    assert!(c.query("SELECT * FROM r_data").is_err());
    assert_eq!(
        c.query("SELECT count(*) FROM sqlite_master").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn integrity_check_passes_with_a_virtual_table() {
    // integrity_check used to error on a vtab (it has no b-tree of its own);
    // it now skips the vtab and still validates the regular tables + the
    // `<name>_data` backing table.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    c.execute("CREATE INDEX iy ON t(y)").unwrap();
    c.execute("INSERT INTO t VALUES (1,'a'),(2,'b')").unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5)").unwrap();
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn vacuum_preserves_a_persistent_virtual_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5), (2, 3, 8)")
        .unwrap();
    c.execute("VACUUM").unwrap();
    // Both the regular table and the vtab (via its backing table) survive.
    assert_eq!(
        rows(&c, "SELECT x FROM t ORDER BY x"),
        [vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(&c, "SELECT id, a, b FROM r ORDER BY id"),
        [
            vec![Value::Integer(1), Value::Real(0.0), Value::Real(5.0)],
            vec![Value::Integer(2), Value::Real(3.0), Value::Real(8.0)],
        ]
    );
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn foreign_key_list_on_a_vtab_is_empty() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    assert!(c
        .query("PRAGMA foreign_key_list(r)")
        .unwrap()
        .rows
        .is_empty());
}
