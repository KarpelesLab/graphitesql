//! Phase 9: window functions (`f(…) OVER (PARTITION BY … ORDER BY …)`).
//!
//! Verified differentially against the real `sqlite3` CLI where available.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        // Use graphitesql's own canonical real formatting (matches sqlite's %.15g).
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn render_rows(r: &graphitesql::QueryResult) -> String {
    r.rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn row_number_basic() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for (g, v) in [(1, 10), (1, 30), (1, 20), (2, 5), (2, 15)] {
        c.execute(&format!("INSERT INTO t(g,v) VALUES ({g},{v})"))
            .unwrap();
    }
    let r = c
        .query("SELECT v, row_number() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY g, v")
        .unwrap();
    let got: Vec<(i64, i64)> = r
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Integer(a), Value::Integer(b)) => (*a, *b),
            _ => panic!("{row:?}"),
        })
        .collect();
    assert_eq!(got, vec![(10, 1), (20, 2), (30, 3), (5, 1), (15, 2)]);
}

#[test]
fn running_sum() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    for v in [1, 2, 3, 4] {
        c.execute(&format!("INSERT INTO t(v) VALUES ({v})"))
            .unwrap();
    }
    let r = c
        .query("SELECT v, sum(v) OVER (ORDER BY v) FROM t ORDER BY v")
        .unwrap();
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match &row[1] {
            Value::Integer(b) => *b,
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(got, vec![1, 3, 6, 10]); // running sum
}

/// The big differential battery against sqlite3.
#[test]
fn window_against_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, g INT, v INT, s TEXT);";
    let mut inserts = String::new();
    // Deterministic data with partitions, ties, and some NULLs.
    let data = [
        (1, 10, "a"),
        (1, 30, "b"),
        (1, 30, "c"),
        (1, 20, "d"),
        (2, 5, "e"),
        (2, 15, "f"),
        (2, 15, "g"),
        (3, 7, "h"),
        (3, 7, "i"),
        (3, 1, "j"),
    ];
    for (i, (g, v, s)) in data.iter().enumerate() {
        inserts += &format!("INSERT INTO t(id,g,v,s) VALUES ({},{g},{v},'{s}');", i + 1);
    }

    let path = std::env::temp_dir().join(format!("gsql-win-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(format!("{setup}{inserts}"))
        .output()
        .unwrap();
    assert!(out.status.success());

    let mut g = Connection::open_memory().unwrap();
    g.execute(setup.trim_end_matches(';')).unwrap();
    for s in inserts.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let queries = [
        "SELECT id, row_number() OVER (ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, row_number() OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
        "SELECT id, dense_rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (PARTITION BY g) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, avg(v) OVER (PARTITION BY g) FROM t ORDER BY id",
        "SELECT id, min(v) OVER (PARTITION BY g), max(v) OVER (PARTITION BY g) FROM t ORDER BY id",
        "SELECT id, lag(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, lead(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, lag(v, 2, -1) OVER (ORDER BY id) FROM t ORDER BY id",
        "SELECT id, first_value(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, last_value(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, nth_value(v, 2) OVER (PARTITION BY g ORDER BY id) FROM t ORDER BY id",
        "SELECT id, ntile(3) OVER (ORDER BY id) FROM t ORDER BY id",
        "SELECT id, ntile(4) OVER (ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, row_number() OVER (ORDER BY v DESC, id) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (ORDER BY v, id) + 100 FROM t ORDER BY id",
        "SELECT g, v, rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY g, v, id",
    ];

    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        match g.query(q) {
            Ok(r) => {
                let got = render_rows(&r);
                if got != want {
                    failures.push(format!(
                        "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
                    ));
                }
            }
            Err(e) => failures.push(format!("  {q}\n    graphite error: {e}")),
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} window queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
