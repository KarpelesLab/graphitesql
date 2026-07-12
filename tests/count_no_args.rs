//! `count()` with no arguments is a synonym for `count(*)`: it tallies every
//! row of the group, including those whose columns are all NULL. This contrasts
//! with `count(X)`, which only counts the rows where `X` is non-NULL. SQLite
//! accepts the no-argument form in scalar, `GROUP BY`, and windowed positions —
//! matched here against the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn count_no_args_is_count_star() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(NULL),(3)").unwrap();

    // Scalar: every row is tallied, NULLs included — unlike `count(a)`.
    assert_eq!(
        c.query("SELECT count(), count(*), count(a) FROM t")
            .unwrap()
            .rows[0],
        vec![Value::Integer(3), Value::Integer(3), Value::Integer(2)]
    );

    // Empty input still yields 0 (not NULL).
    let mut e = Connection::open_memory().unwrap();
    e.execute("CREATE TABLE t(a)").unwrap();
    assert_eq!(
        e.query("SELECT count() FROM t").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn count_no_args_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-count0-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE t(g INT, a INT);\
        INSERT INTO t VALUES(1,10),(1,NULL),(2,20),(2,30),(3,NULL);";
    {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(setup)
            .output()
            .unwrap();
        assert!(o.status.success());
    }
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    let render = |r: &graphitesql::QueryResult| {
        r.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        Value::Integer(i) => i.to_string(),
                        Value::Real(x) => graphitesql::exec::eval::format_real(*x),
                        Value::Text(s) => String::from(s.as_str()),
                        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    for q in [
        "SELECT count() FROM t",
        "SELECT g, count(), count(a) FROM t GROUP BY g ORDER BY g",
        "SELECT g, count() OVER (PARTITION BY g), count(a) OVER (PARTITION BY g) FROM t ORDER BY g, a",
        "SELECT count() OVER (ORDER BY g) FROM t ORDER BY g, a",
    ] {
        let want = {
            let o = Command::new("sqlite3")
                .arg(&path)
                .arg(format!("{q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}
