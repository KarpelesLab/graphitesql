//! SQLite resolves a SELECT-list alias in WHERE, GROUP BY, and HAVING (a real
//! column of the same name takes precedence). Matched against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,2),(3,4),(1,9)").unwrap();
    c
}

#[test]
fn alias_in_where_group_having() {
    let c = setup();
    assert_eq!(
        render(&c, "SELECT a+b AS s FROM t WHERE s>3 ORDER BY s"),
        "7\n10"
    );
    assert_eq!(
        render(&c, "SELECT a%2 AS m, count(*) FROM t GROUP BY m"),
        "1|3"
    );
    assert_eq!(
        render(
            &c,
            "SELECT a AS x, count(*) c FROM t GROUP BY x HAVING c>=2 ORDER BY x"
        ),
        "1|2"
    );
}

#[test]
fn real_column_takes_precedence_over_alias() {
    let c = setup();
    // `a AS b` does not shadow the real column b in WHERE.
    assert_eq!(render(&c, "SELECT a AS b FROM t WHERE b=4"), "3");
    // GROUP BY a groups by the real column a, not the alias `b AS a`.
    assert_eq!(
        render(&c, "SELECT b AS a, count(*) FROM t GROUP BY a ORDER BY a"),
        "2|2\n4|1"
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let setup_sql = "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4),(1,9);";
    let queries = [
        "SELECT a+b AS s FROM t WHERE s>3 ORDER BY s",
        "SELECT a%2 AS m, count(*) FROM t GROUP BY m ORDER BY m",
        "SELECT a AS x, count(*) c FROM t GROUP BY x HAVING c>=2 ORDER BY x",
        "SELECT a AS b FROM t WHERE b=4",
        "SELECT b AS a, count(*) FROM t GROUP BY a ORDER BY a",
    ];
    let c = setup();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!("{setup_sql} {q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        assert_eq!(render(&c, q), want, "diverged on: {q}");
    }
}
