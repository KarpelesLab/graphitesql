//! PRAGMA introspection details that previously diverged from sqlite3 3.50.4:
//! the `table_info.pk` ordinal for composite primary keys, `index_list` origin
//! (`pk` vs `u`) including a WITHOUT ROWID table's implicit PK index, and
//! `table_info` over the schema catalog (`sqlite_master`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

fn i(n: i64) -> Value {
    Value::Integer(n)
}
fn t(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn table_info_pk_is_the_one_based_ordinal() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b, PRIMARY KEY(b, a))")
        .unwrap();
    // pk column (index 5): b is PK position 1, a is position 2.
    let r = rows(&c, "PRAGMA table_info(t)");
    assert_eq!(r[0][1], t("a"));
    assert_eq!(r[0][5], i(2), "a is the 2nd PK column");
    assert_eq!(r[1][1], t("b"));
    assert_eq!(r[1][5], i(1), "b is the 1st PK column");

    // A single / INTEGER primary key is ordinal 1.
    c.execute("CREATE TABLE s(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    let r = rows(&c, "PRAGMA table_info(s)");
    assert_eq!(r[0][5], i(1));
    assert_eq!(r[1][5], i(0));
}

#[test]
fn index_list_origin_distinguishes_pk_from_unique() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a UNIQUE, b PRIMARY KEY)")
        .unwrap();
    // Newest-first: b's PK auto-index (origin pk), then a's UNIQUE (origin u).
    let r = rows(&c, "PRAGMA index_list(t)");
    assert_eq!(r[0][1], t("sqlite_autoindex_t_2"));
    assert_eq!(r[0][3], t("pk"));
    assert_eq!(r[1][1], t("sqlite_autoindex_t_1"));
    assert_eq!(r[1][3], t("u"));
}

#[test]
fn without_rowid_pk_index_is_listed_last_with_pk_origin() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a PRIMARY KEY, b UNIQUE) WITHOUT ROWID")
        .unwrap();
    let r = rows(&c, "PRAGMA index_list(t)");
    // The UNIQUE auto-index comes first; the table-as-PK index is #1, listed last.
    assert_eq!(r[0][1], t("sqlite_autoindex_t_2"));
    assert_eq!(r[0][3], t("u"));
    assert_eq!(r[1][1], t("sqlite_autoindex_t_1"));
    assert_eq!(r[1][3], t("pk"));

    // A WITHOUT ROWID table with only a PK still reports its synthesized index.
    c.execute("CREATE TABLE p(k PRIMARY KEY, v) WITHOUT ROWID")
        .unwrap();
    let r = rows(&c, "PRAGMA index_list(p)");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], t("sqlite_autoindex_p_1"));
    assert_eq!(r[0][3], t("pk"));
}

#[test]
fn without_rowid_pk_columns_report_notnull() {
    let mut c = Connection::open_memory().unwrap();
    // In a WITHOUT ROWID table every PRIMARY KEY column is implicitly NOT NULL,
    // so table_info shows notnull=1 (column index 3) — even a table-level
    // composite key and even an INTEGER PRIMARY KEY. The non-PK column stays 0.
    c.execute("CREATE TABLE t(a INT, b, c TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID")
        .unwrap();
    let r = rows(&c, "PRAGMA table_info(t)");
    assert_eq!((r[0][1].clone(), r[0][3].clone()), (t("a"), i(1)));
    assert_eq!((r[1][1].clone(), r[1][3].clone()), (t("b"), i(1)));
    assert_eq!((r[2][1].clone(), r[2][3].clone()), (t("c"), i(0)));

    // INTEGER PRIMARY KEY in a WITHOUT ROWID table is also NOT NULL (unlike a
    // rowid table, where it stays 0).
    c.execute("CREATE TABLE k(id INTEGER PRIMARY KEY, v) WITHOUT ROWID")
        .unwrap();
    let r = rows(&c, "PRAGMA table_info(k)");
    assert_eq!(r[0][3], i(1));
    assert_eq!(r[1][3], i(0));

    // A rowid table's PK columns remain nullable (notnull=0) — the historical
    // SQLite behavior this fix must not regress.
    c.execute("CREATE TABLE r(a, b, PRIMARY KEY(a, b))")
        .unwrap();
    let r = rows(&c, "PRAGMA table_info(r)");
    assert_eq!(r[0][3], i(0));
    assert_eq!(r[1][3], i(0));
    c.execute("CREATE TABLE r2(a INTEGER PRIMARY KEY)").unwrap();
    assert_eq!(rows(&c, "PRAGMA table_info(r2)")[0][3], i(0));
}

#[test]
fn table_info_over_the_schema_catalog() {
    let c = Connection::open_memory().unwrap();
    for name in ["sqlite_master", "sqlite_schema"] {
        let r = rows(&c, &format!("PRAGMA table_info({name})"));
        let names: Vec<_> = r.iter().map(|row| row[1].clone()).collect();
        assert_eq!(
            names,
            vec![t("type"), t("name"), t("tbl_name"), t("rootpage"), t("sql")]
        );
        // rootpage is INT, the rest TEXT; none are NOT NULL or PK.
        assert_eq!(r[3][2], t("INT"));
        assert_eq!(r[0][2], t("TEXT"));
        assert!(r.iter().all(|row| row[3] == i(0) && row[5] == i(0)));
    }
}
