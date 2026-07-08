//! An unreferenced leading `WITH` CTE on an `UPDATE`/`DELETE` is never analyzed
//! — matching SQLite, which only semantically checks a CTE the statement reaches.
//!
//! graphite used to materialize *every* DML CTE eagerly, so a bad column/table
//! inside an unused one (`WITH u AS (SELECT nope) UPDATE t SET a = 2`) wrongly
//! errored where SQLite succeeds, and an unused infinite recursive CTE would have
//! run. The SELECT path already skipped unused CTEs; this extends the same
//! reachability mask to the UPDATE/DELETE paths. A *used* CTE with a fault still
//! errors, and a duplicate `WITH` name is still rejected even when unreferenced.
#![cfg(feature = "std")]

use std::process::Command;

/// Run `sql` against `bin` on a fresh in-memory database; return the first line
/// of stdout, else the (prefix-stripped) error.
fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(sql)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if !line.is_empty() {
            return line.to_string();
        }
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
        if line.starts_with('^') {
            continue;
        }
        let s = line
            .strip_prefix("Error: in prepare, ")
            .or_else(|| line.strip_prefix("Error: stepping, "))
            .or_else(|| line.strip_prefix("Error: SQL error: "))
            .or_else(|| line.strip_prefix("Error: "))
            .unwrap_or(line);
        let s = s.strip_prefix("error: ").unwrap_or(s);
        let s = s.rsplit_once(" (").map_or(s, |(head, tail)| {
            if tail
                .trim_end_matches(')')
                .chars()
                .all(|c| c.is_ascii_digit())
            {
                head
            } else {
                s
            }
        });
        return s.to_string();
    }
    String::new()
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn same(sql: &str) {
    let g = run(env!("CARGO_BIN_EXE_graphitesql"), sql);
    let s = run("sqlite3", sql);
    assert_eq!(g, s, "mismatch for SQL: {sql}");
}

#[test]
fn dml_unused_cte_not_analyzed_parity() {
    if !sqlite3_available() {
        return;
    }

    // An unreferenced CTE with a bad column is not analyzed (UPDATE & DELETE).
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH u AS (SELECT nope) UPDATE t SET a=2; SELECT a FROM t;",
    );
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH u AS (SELECT nope) DELETE FROM t WHERE a=1; SELECT count(*) FROM t;",
    );
    // An unreferenced bad *table* inside the CTE is likewise ignored.
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH u AS (SELECT * FROM nosuchtbl) UPDATE t SET a=5; SELECT a FROM t;",
    );

    // An unused infinite recursive CTE is never run.
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) UPDATE t SET a=9; SELECT a FROM t;",
    );

    // A *used* CTE with a bad column still errors (regression guards).
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH u AS (SELECT nope AS x) UPDATE t SET a=(SELECT x FROM u);",
    );
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH u AS (SELECT nope AS x) DELETE FROM t WHERE a IN (SELECT x FROM u);",
    );

    // A used CTE produces the right result through every reference site.
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3); WITH u(k) AS (VALUES(2),(3)) DELETE FROM t WHERE a IN (SELECT k FROM u); SELECT a FROM t ORDER BY a;",
    );
    same(
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,0); WITH u(k,v) AS (VALUES(1,7)) UPDATE t SET b=u.v FROM u WHERE t.a=u.k; SELECT a,b FROM t;",
    );
    same(
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,0); WITH u AS (SELECT 5 v) UPDATE t SET b=(SELECT v FROM u); SELECT a,b FROM t;",
    );
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH u AS (SELECT 9 v) UPDATE t SET a=2 RETURNING (SELECT v FROM u);",
    );
    same(
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,1); WITH u AS (SELECT 7 p,8 q) UPDATE t SET (a,b)=(SELECT p,q FROM u); SELECT a,b FROM t;",
    );

    // A duplicate WITH name is rejected even when neither is referenced.
    same("CREATE TABLE t(a); WITH u AS (SELECT 1), u AS (SELECT 2) DELETE FROM t WHERE a=1;");
    same("CREATE TABLE t(a); WITH u AS (SELECT 1), u AS (SELECT 2) UPDATE t SET a=1;");

    // Transitive reachability: an unused CTE that names another unused CTE is
    // dropped wholesale (no analysis); a used chain still validates.
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH v AS (SELECT bad), u AS (SELECT * FROM v) UPDATE t SET a=3; SELECT a FROM t;",
    );
    same(
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); WITH v AS (SELECT bad x), u AS (SELECT x FROM v) UPDATE t SET a=(SELECT x FROM u);",
    );
}
