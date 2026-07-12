//! Track A: UPSERT (`ON CONFLICT … DO NOTHING/UPDATE`) and `RETURNING`.
//! Verified differentially against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::exec::eval::Params;
use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => String::from(s.as_str()),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn result_str(r: &graphitesql::QueryResult) -> String {
    r.rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn upsert_do_nothing_and_do_update() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, hits INT DEFAULT 0)")
        .unwrap();
    c.execute("INSERT INTO t(id,k,hits) VALUES (1,'a',1)")
        .unwrap();
    // DO NOTHING: conflicting insert is silently skipped.
    let n = c
        .execute("INSERT INTO t(id,k,hits) VALUES (2,'a',99) ON CONFLICT(k) DO NOTHING")
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        c.query("SELECT id,hits FROM t WHERE k='a'").unwrap().rows[0],
        vec![Value::Integer(1), Value::Integer(1)]
    );
    // DO UPDATE referencing excluded.* and the existing row.
    let n = c
        .execute(
            "INSERT INTO t(id,k,hits) VALUES (3,'a',10) \
             ON CONFLICT(k) DO UPDATE SET hits = hits + excluded.hits",
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        c.query("SELECT id,hits FROM t WHERE k='a'").unwrap().rows[0],
        vec![Value::Integer(1), Value::Integer(11)]
    );
    // Non-conflicting upsert just inserts.
    c.execute("INSERT INTO t(id,k,hits) VALUES (5,'b',2) ON CONFLICT(k) DO UPDATE SET hits=999")
        .unwrap();
    assert_eq!(
        c.query("SELECT hits FROM t WHERE k='b'").unwrap().rows[0][0],
        Value::Integer(2)
    );
}

#[test]
fn upsert_do_update_where_vetoes() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(k TEXT PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('a', 5)").unwrap();
    // WHERE false → the update is vetoed, row unchanged, no error.
    let n = c
        .execute(
            "INSERT INTO t VALUES ('a', 7) ON CONFLICT(k) DO UPDATE SET v=excluded.v WHERE v > 100",
        )
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        c.query("SELECT v FROM t").unwrap().rows[0][0],
        Value::Integer(5)
    );
}

#[test]
fn returning_insert_update_delete() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT, v INT)")
        .unwrap();
    let r = c
        .execute_returning(
            "INSERT INTO t(n,v) VALUES ('a',1),('b',2) RETURNING id, n, v",
            &Params::default(),
        )
        .unwrap();
    assert_eq!(r.columns, vec!["id", "n", "v"]);
    assert_eq!(result_str(&r), "1|a|1\n2|b|2");

    let r = c
        .execute_returning(
            "UPDATE t SET v = v * 10 WHERE id = 1 RETURNING id, v",
            &Params::default(),
        )
        .unwrap();
    assert_eq!(result_str(&r), "1|10");

    let r = c
        .execute_returning("DELETE FROM t WHERE id = 2 RETURNING *", &Params::default())
        .unwrap();
    assert_eq!(result_str(&r), "2|b|2");

    // RETURNING an expression with an alias.
    let r = c
        .execute_returning(
            "INSERT INTO t(n,v) VALUES ('c',3) RETURNING v + 1 AS w",
            &Params::default(),
        )
        .unwrap();
    assert_eq!(r.columns, vec!["w"]);
    assert_eq!(result_str(&r), "4");
}

#[test]
fn upsert_returning_combined() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(k TEXT PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('a', 1)").unwrap();
    // Upsert that updates, with RETURNING on the resulting row.
    let r = c
        .execute_returning(
            "INSERT INTO t VALUES ('a', 5) ON CONFLICT(k) DO UPDATE SET v = v + excluded.v RETURNING k, v",
            &Params::default(),
        )
        .unwrap();
    assert_eq!(result_str(&r), "a|6");
    // Upsert that inserts, RETURNING the inserted row.
    let r = c
        .execute_returning(
            "INSERT INTO t VALUES ('z', 9) ON CONFLICT(k) DO UPDATE SET v = 0 RETURNING k, v",
            &Params::default(),
        )
        .unwrap();
    assert_eq!(result_str(&r), "z|9");
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // (statement, whether it carries RETURNING we compare row-for-row)
    let steps: &[&str] = &[
        "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, hits INT DEFAULT 0)",
        "INSERT INTO t(id,k,hits) VALUES (1,'a',1),(2,'b',1)",
        "INSERT INTO t(id,k,hits) VALUES (3,'a',5) ON CONFLICT(k) DO UPDATE SET hits=hits+excluded.hits RETURNING id,k,hits",
        "INSERT INTO t(id,k,hits) VALUES (4,'c',7) ON CONFLICT(k) DO UPDATE SET hits=hits+excluded.hits RETURNING id,k,hits",
        "INSERT INTO t(id,k,hits) VALUES (5,'b',100) ON CONFLICT(k) DO NOTHING",
        "UPDATE t SET hits = hits + 1 WHERE k='a' RETURNING id,hits",
        "DELETE FROM t WHERE k='c' RETURNING id,k",
    ];

    let path = std::env::temp_dir().join(format!("gsql-upsert-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    let mut g = Connection::open_memory().unwrap();
    let mut failures = Vec::new();
    for s in steps {
        // sqlite3: run the statement, capture stdout (RETURNING rows, if any).
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(s).output().unwrap();
            assert!(
                o.status.success(),
                "sqlite3 failed on `{s}`: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let upper = s.trim_start().to_ascii_uppercase();
        let got = if upper.contains("RETURNING") {
            let r = g.execute_returning(s, &Params::default()).unwrap();
            result_str(&r)
        } else {
            g.execute(s).unwrap();
            String::new()
        };
        if got != want {
            failures.push(format!(
                "  {s}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    // Final table state must agree.
    let final_q = "SELECT id,k,hits FROM t ORDER BY id";
    let want = {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(final_q)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let got = result_str(&g.query(final_q).unwrap());
    if got != want {
        failures.push(format!(
            "  {final_q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
        ));
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} steps diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
