//! An `ORDER BY` *expression* — not just a bare term — may reference a
//! SELECT-output alias: `SELECT a AS x FROM t ORDER BY x+0`. SQLite resolves the
//! name to the computed output value, with a real input column of the same name
//! taking precedence, and the alias is in scope for aggregate, window, and
//! `DISTINCT` queries too. graphite's tree-walker previously resolved a bare
//! alias term (and ordinals) but raised `no such column` once the alias appeared
//! inside a larger expression. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const SETUP: &str =
    "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z'),(1,4,'w');";

/// First-column integers of a query, for pinning behavior without an oracle.
fn col0(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|row| match row[0] {
            Value::Integer(i) => i,
            ref v => panic!("expected int, got {v:?} from {sql}"),
        })
        .collect()
}

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b,c)").unwrap();
    c.execute("INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z'),(1,4,'w')")
        .unwrap();
    c
}

#[test]
fn alias_inside_order_by_expression_resolves() {
    let c = conn();
    // `x` is `a`; ordering by `x+0` is ordering by `a`.
    assert_eq!(
        col0(&c, "SELECT a AS x FROM t ORDER BY x+0"),
        vec![1, 1, 2, 3],
    );
    // Negation, arithmetic, and a function over the alias.
    assert_eq!(
        col0(&c, "SELECT a AS x FROM t ORDER BY -x"),
        vec![3, 2, 1, 1],
    );
    assert_eq!(
        col0(&c, "SELECT a AS x FROM t ORDER BY abs(x) DESC"),
        vec![3, 2, 1, 1],
    );
    // Two aliases combined.
    assert_eq!(
        col0(&c, "SELECT a AS x, b AS y FROM t ORDER BY x+y"),
        vec![1, 3, 2, 1],
    );
}

#[test]
fn real_column_takes_precedence_over_alias() {
    // `SELECT a AS b` names the output `b`, but a real column `b` already
    // exists, so `ORDER BY b+0` orders by the *column* b (1,2,3,4), not by a.
    let c = conn();
    assert_eq!(
        col0(&c, "SELECT a AS b FROM t ORDER BY b+0"),
        vec![3, 1, 2, 1],
    );
}

#[test]
fn alias_in_order_by_expression_for_aggregate_window_distinct() {
    let c = conn();
    // Aggregate without GROUP BY: the alias is the scalar aggregate result.
    assert_eq!(
        col0(&c, "SELECT count(*) AS n FROM t ORDER BY n+0"),
        vec![4]
    );
    // Window-function alias inside an ORDER BY expression.
    assert_eq!(
        col0(
            &c,
            "SELECT a FROM t ORDER BY row_number() OVER (ORDER BY a)+0",
        ),
        vec![1, 1, 2, 3],
    );
    // DISTINCT, alias inside an expression.
    assert_eq!(
        col0(&c, "SELECT DISTINCT a AS x FROM t ORDER BY x*x"),
        vec![1, 2, 3],
    );
    // Grouped: alias mixed with an aggregate in the ORDER BY expression.
    assert_eq!(
        col0(
            &c,
            "SELECT a AS x, count(*) AS n FROM t GROUP BY a ORDER BY n*10+x",
        ),
        vec![2, 3, 1],
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_start_matches("stepping, ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT a AS x FROM t ORDER BY x+0",
        "SELECT a AS x FROM t ORDER BY -x",
        "SELECT a AS x FROM t ORDER BY x*x DESC",
        "SELECT a AS x FROM t ORDER BY abs(x)",
        "SELECT a AS x, b AS y FROM t ORDER BY x+y",
        "SELECT a AS x FROM t ORDER BY x||c",
        "SELECT a AS b FROM t ORDER BY b+0",
        "SELECT a AS x FROM t WHERE b>1 ORDER BY x*-1",
        "SELECT a AS x FROM t ORDER BY CASE WHEN x>1 THEN 0 ELSE 1 END, x",
        "SELECT upper(c) AS u FROM t ORDER BY u || 'q'",
        "SELECT count(*) AS n FROM t ORDER BY n+0",
        "SELECT max(b) AS m FROM t ORDER BY m+0",
        "SELECT a AS x, count(*) AS n FROM t GROUP BY a ORDER BY n*10+x",
        "SELECT DISTINCT a AS x FROM t ORDER BY x*x",
        "SELECT a FROM t ORDER BY row_number() OVER (ORDER BY a)+0",
        // Still an error: a name that is neither a column nor an alias.
        "SELECT a AS x FROM t ORDER BY nonexist+0",
    ] {
        let full = format!("{SETUP} {sql}");
        assert_eq!(run("sqlite3", &full), run(g, &full), "for {sql}");
    }
}
