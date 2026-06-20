//! Windowed `group_concat`/`string_agg` honor their separator argument (the
//! window path previously hard-coded ","). Matched to the sqlite3 CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn col(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|mut r| match r.remove(0) {
            Value::Text(s) => s,
            Value::Null => String::from("<null>"),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

#[test]
fn windowed_group_concat_separator() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    // Running window with a custom separator.
    assert_eq!(
        col(&c, "SELECT group_concat(x,'-') OVER (ORDER BY x) FROM t"),
        vec!["1", "1-2", "1-2-3"]
    );
    // Whole-partition window with a pipe separator.
    assert_eq!(
        col(&c, "SELECT group_concat(x,'|') OVER () FROM t"),
        vec!["1|2|3", "1|2|3", "1|2|3"]
    );
    // The default separator is still "," when the argument is omitted.
    assert_eq!(
        col(&c, "SELECT group_concat(x) OVER (ORDER BY x) FROM t"),
        vec!["1", "1,2", "1,2,3"]
    );
    // Empty separator and the string_agg alias.
    assert_eq!(
        col(
            &c,
            "SELECT string_agg(CAST(x AS TEXT),'') OVER (ORDER BY x) FROM t"
        ),
        vec!["1", "12", "123"]
    );
}
