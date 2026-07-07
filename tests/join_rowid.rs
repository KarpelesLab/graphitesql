//! Table-qualified rowid references in a JOIN (`t.rowid` / `t._rowid_` / `t.oid`).
//!
//! A joined row carries no single rowid, so graphite contributes each base rowid
//! table's rowid as a hidden, table-tagged column; a qualified rowid alias
//! resolves through it. These checks are differential against real sqlite3 3.50.4
//! (skipped if the binary is absent) and assert the rows on BOTH `set_use_vdbe`
//! modes (the VDBE join path defers a qualified-rowid join to the tree-walker).
//!
//! Coverage: `t.rowid` selected / in `ON` / in `WHERE` / in `ORDER BY`; every
//! join shape (INNER, LEFT, RIGHT, FULL, CROSS, comma, 3-table, self-join with
//! aliases, USING/NATURAL); rowid-seek / index-seek / materialized strategies;
//! `_rowid_` / `oid` aliases; a real `rowid` user column (wins over the alias); a
//! `WITHOUT ROWID` table (still errors); an INTEGER PRIMARY KEY alias table
//! (`t.rowid` == the IPK). Also confirms `SELECT *` over a join yields only the
//! user columns (hidden rowids don't leak).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn have_sqlite3() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite3(script: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(script)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn fmt(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => {
            if *r == (*r as i64) as f64 {
                format!("{:.1}", r)
            } else {
                format!("{r}")
            }
        }
        Value::Text(s) => s.clone(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

fn graphite(setup: &str, use_vdbe: bool) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.set_use_vdbe(use_vdbe);
    for stmt in setup.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

fn graphite_rows(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(fmt).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// graphite's rows for `sql` equal sqlite's EXACTLY (order included), both VDBE modes.
fn assert_rows(setup: &str, sql: &str) {
    let want = sqlite3(&format!("{setup}\n{sql};"));
    for &vdbe in &[true, false] {
        let c = graphite(setup, vdbe);
        let got = graphite_rows(&c, sql);
        assert_eq!(got, want, "rows diverged (use_vdbe={vdbe}) for `{sql}`");
    }
}

/// graphite and sqlite BOTH reject `sql`, and graphite's message contains the
/// expected core text, on both VDBE modes.
fn assert_both_error(setup: &str, sql: &str, core: &str) {
    // sqlite writes its error to stderr; confirm it rejects and mentions `core`.
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{setup}\n{sql};"))
        .output()
        .unwrap();
    let sqlite_err = String::from_utf8_lossy(&o.stderr);
    assert!(
        sqlite_err.contains(core),
        "expected sqlite to reject `{sql}` with `{core}`, got stderr=`{sqlite_err}`"
    );
    for &vdbe in &[true, false] {
        let c = graphite(setup, vdbe);
        let err = c
            .query(sql)
            .err()
            .unwrap_or_else(|| panic!("expected graphite to reject `{sql}` (use_vdbe={vdbe})"));
        assert!(
            format!("{err}").contains(core),
            "error text diverged (use_vdbe={vdbe}) for `{sql}`: graphite=`{err}` wanted core=`{core}`"
        );
    }
}

// Two plain tables: `a` has an INTEGER PRIMARY KEY alias, `b` references a by rowid.
const SETUP: &str = "\
CREATE TABLE a(x);\
CREATE TABLE b(y, aref);\
INSERT INTO a VALUES('p'),('q'),('r');\
INSERT INTO b VALUES(1,1),(2,2),(3,1);\
";

#[test]
fn rowid_selected_on_where_orderby() {
    if !have_sqlite3() {
        return;
    }
    // Selected in the projection.
    assert_rows(
        SETUP,
        "SELECT a.rowid, a.x FROM a JOIN b ON 1=1 ORDER BY a.rowid, b.y",
    );
    // In the ON (rowid-seek join: b.aref = a.rowid seeks a by rowid).
    assert_rows(
        SETUP,
        "SELECT a.x, b.y FROM a JOIN b ON a.rowid=b.aref ORDER BY a.x, b.y",
    );
    assert_rows(
        SETUP,
        "SELECT a.x, b.y FROM b JOIN a ON a.rowid=b.aref ORDER BY b.y",
    );
    // In the WHERE.
    assert_rows(
        SETUP,
        "SELECT a.x, b.y FROM a JOIN b ON 1=1 WHERE a.rowid=1 ORDER BY b.y",
    );
    // In ORDER BY.
    assert_rows(
        SETUP,
        "SELECT a.x, b.y FROM a JOIN b ON a.rowid=b.aref ORDER BY b.rowid DESC",
    );
    // Both tables' rowids projected.
    assert_rows(
        SETUP,
        "SELECT a.rowid, b.rowid FROM a JOIN b ON 1=1 ORDER BY a.rowid, b.rowid",
    );
}

#[test]
fn rowid_aliases_underscore_and_oid() {
    if !have_sqlite3() {
        return;
    }
    assert_rows(
        SETUP,
        "SELECT a._rowid_, b.oid FROM a JOIN b ON a._rowid_=b.aref ORDER BY a._rowid_, b.oid",
    );
    assert_rows(
        SETUP,
        "SELECT a.oid, b._rowid_ FROM a JOIN b ON a.oid=b.aref ORDER BY b._rowid_",
    );
}

#[test]
fn all_join_shapes() {
    if !have_sqlite3() {
        return;
    }
    for kind in [
        "INNER JOIN",
        "LEFT JOIN",
        "RIGHT JOIN",
        "FULL JOIN",
        "CROSS JOIN",
    ] {
        assert_rows(
            SETUP,
            &format!(
                "SELECT a.x, a.rowid, b.y, b.rowid FROM a {kind} b ON a.rowid=b.aref \
                 ORDER BY a.rowid, b.rowid"
            ),
        );
    }
    // Comma join with the equality in WHERE.
    assert_rows(
        SETUP,
        "SELECT a.x, b.y FROM a, b WHERE a.rowid=b.aref ORDER BY a.rowid, b.y",
    );
}

#[test]
fn three_table_join() {
    if !have_sqlite3() {
        return;
    }
    let setup = "\
CREATE TABLE a(x);\
CREATE TABLE b(y);\
CREATE TABLE c(z);\
INSERT INTO a VALUES('a1'),('a2');\
INSERT INTO b VALUES('b1'),('b2');\
INSERT INTO c VALUES('c1'),('c2');\
";
    assert_rows(
        setup,
        "SELECT a.rowid, b.rowid, c.rowid FROM a JOIN b ON b.rowid=a.rowid \
         JOIN c ON c.rowid=a.rowid ORDER BY a.rowid",
    );
    assert_rows(
        setup,
        "SELECT a.x, b.y, c.z FROM a, b, c WHERE a.rowid=b.rowid AND b.rowid=c.rowid \
         ORDER BY a.rowid",
    );
}

#[test]
fn self_join_aliases() {
    if !have_sqlite3() {
        return;
    }
    assert_rows(
        SETUP,
        "SELECT t1.rowid, t2.rowid, t1.x, t2.x FROM a t1 JOIN a t2 ON t2.rowid=t1.rowid+1 \
         ORDER BY t1.rowid",
    );
    assert_rows(
        SETUP,
        "SELECT t1.x, t2.x FROM a t1 JOIN a t2 ON t1.rowid < t2.rowid ORDER BY t1.rowid, t2.rowid",
    );
}

#[test]
fn using_and_natural() {
    if !have_sqlite3() {
        return;
    }
    let setup = "\
CREATE TABLE a(k, x);\
CREATE TABLE b(k, y);\
INSERT INTO a VALUES(1,'ax'),(2,'ay');\
INSERT INTO b VALUES(1,'by'),(2,'bz');\
";
    assert_rows(
        setup,
        "SELECT a.rowid, b.rowid, x, y FROM a JOIN b USING(k) ORDER BY a.rowid",
    );
    assert_rows(
        setup,
        "SELECT a.rowid, b.rowid, k FROM a NATURAL JOIN b ORDER BY a.rowid",
    );
}

#[test]
fn index_seek_join_with_rowid() {
    if !have_sqlite3() {
        return;
    }
    // b.aref is indexed → the join seeks b by that secondary index; a.rowid /
    // b.rowid must still resolve.
    let setup = "\
CREATE TABLE a(x);\
CREATE TABLE b(y, aref);\
CREATE INDEX bi ON b(aref);\
INSERT INTO a VALUES('p'),('q'),('r');\
INSERT INTO b VALUES(10,1),(20,2),(30,1);\
";
    assert_rows(
        setup,
        "SELECT a.rowid, a.x, b.rowid, b.y FROM a JOIN b ON b.aref=a.rowid \
         ORDER BY a.rowid, b.rowid",
    );
}

#[test]
fn materialized_join_with_rowid() {
    if !have_sqlite3() {
        return;
    }
    // A non-equi ON forces the materialize/nested-loop strategy.
    assert_rows(
        SETUP,
        "SELECT a.rowid, b.rowid FROM a JOIN b ON a.rowid < b.rowid ORDER BY a.rowid, b.rowid",
    );
}

#[test]
fn real_rowid_column_wins() {
    if !have_sqlite3() {
        return;
    }
    // A real user column named `rowid` shadows the alias — `a.rowid` is the user
    // column (100/200), not the row's rowid, in both graphite and sqlite.
    let setup = "\
CREATE TABLE a(rowid, v);\
CREATE TABLE b(y);\
INSERT INTO a VALUES(100,'a'),(200,'b');\
INSERT INTO b VALUES('x'),('y');\
";
    assert_rows(
        setup,
        "SELECT a.rowid, a.v, b.y FROM a JOIN b ON 1=1 ORDER BY a.rowid, b.y",
    );
}

#[test]
fn without_rowid_still_errors() {
    if !have_sqlite3() {
        return;
    }
    // A WITHOUT ROWID inner has no rowid; `w.rowid` errors like sqlite.
    let setup = "\
CREATE TABLE a(x);\
CREATE TABLE w(k PRIMARY KEY, v) WITHOUT ROWID;\
INSERT INTO a VALUES('p'),('q');\
INSERT INTO w VALUES(1,'one'),(2,'two');\
";
    assert_both_error(
        setup,
        "SELECT w.rowid FROM a JOIN w ON 1=1",
        "no such column: w.rowid",
    );
    // But the rowid table's rowid in the same join still resolves.
    assert_rows(
        setup,
        "SELECT a.rowid, a.x, w.k FROM a JOIN w ON 1=1 ORDER BY a.rowid, w.k",
    );
}

#[test]
fn ipk_alias_table() {
    if !have_sqlite3() {
        return;
    }
    // `id` is an INTEGER PRIMARY KEY alias — `t.rowid` == `t.id`.
    let setup = "\
CREATE TABLE a(id INTEGER PRIMARY KEY, x);\
CREATE TABLE b(y, aref);\
INSERT INTO a VALUES(5,'p'),(6,'q');\
INSERT INTO b VALUES(1,5),(2,6);\
";
    assert_rows(
        setup,
        "SELECT a.rowid, a.id, a.x, b.y FROM a JOIN b ON a.rowid=b.aref ORDER BY a.rowid",
    );
}

#[test]
fn select_star_does_not_leak_hidden_rowid() {
    if !have_sqlite3() {
        return;
    }
    // `SELECT *` over a join that ALSO references a qualified rowid elsewhere must
    // still expand to exactly the user columns.
    assert_rows(
        SETUP,
        "SELECT * FROM a JOIN b ON a.rowid=b.aref ORDER BY a.rowid, b.y",
    );
    // Column count check: 3 (a.x + b.y + b.aref)... actually a.x=1, b: y,aref=2 → 3.
    let c = graphite(SETUP, false);
    let res = c.query("SELECT * FROM a JOIN b ON a.rowid=b.aref").unwrap();
    assert_eq!(res.columns.len(), 3, "SELECT * leaked hidden rowid columns");
}
