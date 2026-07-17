//! Byte-exact `rtree constraint failed` errors on the plain (no-aux-column)
//! rtree write path — the port of `rtree.c`'s `rtreeUpdate` /
//! `rtreeConstraintError`:
//!
//! * the message names the first failing coordinate pair's columns and the
//!   table (`rtree constraint failed: <t>.(<min><=<max>)`), not a bare string;
//! * `min <= max` is validated on the **f32-rounded** stored coordinates, so a
//!   near-equal pair that rounds to the same f32 is accepted;
//! * the coordinate pairs are validated **before** the rowid-uniqueness check,
//!   so a row that violates both reports the coordinate error — and the
//!   coordinate constraint honours the statement's conflict mode (`OR IGNORE`
//!   skips it, `OR REPLACE` still errors).
//!
//! Each assertion is pinned to what `sqlite3 3.50.4` reports for the same SQL.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn err_msg(c: &mut Connection, sql: &str) -> String {
    let e = c.execute(sql).unwrap_err();
    // `Error::Constraint`'s Display renders the bare message verbatim; the CLI
    // adds the `Error: stepping, … (19)` framing around it.
    format!("{e}")
}

#[test]
fn message_names_table_and_columns() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING rtree(id,x0,x1)")
        .unwrap();
    assert_eq!(
        err_msg(&mut c, "INSERT INTO t VALUES(1,2,1)"),
        "rtree constraint failed: t.(x0<=x1)"
    );
}

#[test]
fn message_names_first_failing_pair() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING rtree(id,x0,x1,y0,y1)")
        .unwrap();
    // The x pair (0<=5) is fine; the y pair (9>2) fails and is named.
    assert_eq!(
        err_msg(&mut c, "INSERT INTO t VALUES(1,0,5,9,2)"),
        "rtree constraint failed: t.(y0<=y1)"
    );
}

#[test]
fn near_equal_coords_round_to_same_f32_and_are_accepted() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING rtree(id,x0,x1)")
        .unwrap();
    // 1.000000000001 rounds down to 1.0, 1.0 stays 1.0 — min == max, accepted.
    c.execute("INSERT INTO t VALUES(1,1.000000000001,1.0)")
        .unwrap();
    let r = c.query("SELECT id,x0,x1 FROM t").unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn coordinate_check_precedes_uniqueness_check() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING rtree(id,x0,x1)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,0,5)").unwrap();
    // rowid 1 already exists AND the coords are inverted; sqlite reports the
    // coordinate error, not the UNIQUE one.
    assert_eq!(
        err_msg(&mut c, "INSERT INTO t VALUES(1,9,2)"),
        "rtree constraint failed: t.(x0<=x1)"
    );
    // A pure duplicate (valid coords) still reports the UNIQUE error.
    assert_eq!(
        err_msg(&mut c, "INSERT INTO t VALUES(1,0,5)"),
        "UNIQUE constraint failed: t.id"
    );
}

#[test]
fn conflict_modes_and_bad_coords() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING rtree(id,x0,x1)")
        .unwrap();
    // OR IGNORE silently skips a coordinate violation.
    c.execute("INSERT OR IGNORE INTO t VALUES(1,9,2)").unwrap();
    assert_eq!(c.query("SELECT count(*) FROM t").unwrap().rows.len(), 1);
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        graphitesql::Value::Integer(0)
    );
    // OR REPLACE does NOT resolve a coordinate violation — it still errors.
    assert_eq!(
        err_msg(&mut c, "INSERT OR REPLACE INTO t VALUES(1,9,2)"),
        "rtree constraint failed: t.(x0<=x1)"
    );
}

#[test]
fn update_to_inverted_coords_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING rtree(id,x0,x1)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,0,5)").unwrap();
    assert_eq!(
        err_msg(&mut c, "UPDATE t SET x0=9, x1=2 WHERE id=1"),
        "rtree constraint failed: t.(x0<=x1)"
    );
}
