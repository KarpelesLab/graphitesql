//! A recursive CTE may carry an `ORDER BY` (with an optional `LIMIT`) on its
//! recursive `SELECT`. SQLite uses it to order the work *queue*: the plain
//! breadth-first FIFO becomes a **priority queue** that always extracts the row
//! sorting first under the `ORDER BY` next — its documented depth-/breadth-first
//! search control (<https://sqlite.org/lang_with.html>). graphite used to ignore
//! the clause and always run breadth-first; `eval_recursive_cte` now honours it.
//!
//! Two behaviours are pinned against the sqlite3 3.50.4 CLI:
//!   1. row order — the priority-queue extraction changes the output order;
//!   2. error parity — like any compound `ORDER BY`, each term must name an
//!      output column (by 1-based position, or the recursive `SELECT`'s intrinsic
//!      result-column name); a CTE-renamed name, a base column, or an expression
//!      is rejected. graphite used to silently run those.

#![cfg(feature = "std")]

use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn rows(bin: &str, sql: &str) -> String {
    // Feed the SQL on stdin (not as an argument) so both CLIs report a prepare
    // error with the same `<prefix>: <message>` shape — passing it as an argument
    // makes the sqlite CLI insert an extra `in prepare, ` context token.
    let mut child = Command::new(bin)
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    // Fold stdout+stderr so an error (which sqlite prints to stderr) is compared
    // too, but strip each CLI's own error prefix so only the message text counts
    // (`Parse error near line 1:` / `Runtime error near line 1:` vs `Error:`).
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    let e = String::from_utf8_lossy(&o.stderr).into_owned();
    // Keep only the message line(s) — those carrying a known CLI error prefix.
    // Skip the sqlite CLI's echoed-SQL + caret context lines (which start with
    // neither prefix), so only the library-level error text is compared.
    let prefixes = [
        "Parse error near line 1: ",
        "Runtime error near line 1: ",
        "Error: error: ",
        "Error: ",
    ];
    for line in e.lines() {
        if let Some(p) = prefixes.iter().find(|p| line.starts_with(**p)) {
            s.push_str(&line[p.len()..]);
            s.push('\n');
        }
    }
    s
}

#[test]
fn recursive_cte_order_by_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    // A branching recursion where the extraction order is observable.
    let e = "CREATE TABLE e(a,b);INSERT INTO e VALUES(1,2),(1,3),(2,4),(3,5);";
    // An org chart for the classic depth-first-vs-breadth-first example.
    let org = "CREATE TABLE org(name TEXT PRIMARY KEY, boss TEXT);\
               INSERT INTO org VALUES('Alice',NULL),('Bob','Alice'),('Cindy','Alice'),\
               ('Dave','Bob'),('Emma','Bob'),('Fred','Cindy'),('Gail','Cindy');";
    let p = "CREATE TABLE p(id,par,w);\
             INSERT INTO p VALUES(1,NULL,5),(2,1,3),(3,1,9),(4,2,1),(5,2,7);";

    let cases: &[&str] = &[
        // --- row order: priority queue vs FIFO ---
        &format!(
            "{e}WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT b FROM e JOIN r ON a=x \
             ORDER BY 1 DESC LIMIT 20) SELECT group_concat(x) FROM (SELECT x FROM r);"
        ),
        &format!(
            "{e}WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT b FROM e JOIN r ON a=x \
             ORDER BY 1 ASC LIMIT 20) SELECT group_concat(x) FROM (SELECT x FROM r);"
        ),
        // no ORDER BY — breadth-first FIFO (batch path, unchanged)
        &format!(
            "{e}WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT b FROM e JOIN r ON a=x) \
             SELECT group_concat(x) FROM (SELECT x FROM r);"
        ),
        // org chart: breadth-first (no order) then depth-first (ORDER BY lvl DESC)
        &format!(
            "{org}WITH RECURSIVE u(n,lvl) AS (SELECT name,0 FROM org WHERE boss IS NULL \
             UNION ALL SELECT org.name,u.lvl+1 FROM org,u WHERE org.boss=u.n) SELECT n FROM u;"
        ),
        &format!(
            "{org}WITH RECURSIVE u(n,lvl) AS (SELECT name,0 FROM org WHERE boss IS NULL \
             UNION ALL SELECT org.name,u.lvl+1 FROM org,u WHERE org.boss=u.n ORDER BY 2 DESC) \
             SELECT n FROM u;"
        ),
        // order by the intrinsic name (name ASC / DESC)
        &format!(
            "{org}WITH RECURSIVE u(name) AS (SELECT name FROM org WHERE boss IS NULL \
             UNION ALL SELECT org.name FROM org,u WHERE org.boss=u.name ORDER BY 1 DESC) \
             SELECT name FROM u;"
        ),
        // UNION (distinct) with a priority queue
        &format!(
            "{e}WITH RECURSIVE r(x) AS (SELECT 1 UNION SELECT b FROM e JOIN r ON a=x \
             ORDER BY 1 DESC LIMIT 30) SELECT group_concat(x) FROM (SELECT x FROM r);"
        ),
        // multi-key ORDER BY (depth desc, then weight asc)
        &format!(
            "{p}WITH RECURSIVE t(id,w,d) AS (SELECT id,w,0 FROM p WHERE par IS NULL \
             UNION ALL SELECT p.id,p.w,t.d+1 FROM p JOIN t ON p.par=t.id ORDER BY 3 DESC, 2 ASC) \
             SELECT id FROM t;"
        ),
        // ORDER BY + outer LIMIT terminates an infinite recursion
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c ORDER BY 1 DESC LIMIT 50) \
         SELECT sum(n) FROM c;",
        // --- error parity: an unresolvable ORDER BY term is rejected ---
        // the CTE-renamed column name is NOT an intrinsic result-column name
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<3 ORDER BY n) \
         SELECT * FROM c;",
        // an expression is not a result column
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<3 ORDER BY n+0 \
         LIMIT 9) SELECT * FROM c;",
        // a base-table column of the recursive arm is not a result column
        "CREATE TABLE t(v);INSERT INTO t VALUES(5),(3);\
         WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<3 ORDER BY v \
         LIMIT 9) SELECT * FROM c;",
        // out-of-range positional term
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<3 ORDER BY 2 \
         LIMIT 9) SELECT * FROM c;",
        // --- valid terms still run ---
        // aliasing the anchor column makes ORDER BY <alias> resolve
        "WITH RECURSIVE c(n) AS (SELECT 1 AS x UNION ALL SELECT n+1 FROM c WHERE n<3 \
         ORDER BY x LIMIT 9) SELECT * FROM c;",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5 \
         ORDER BY 1 DESC LIMIT 20) SELECT * FROM c;",
    ];

    for q in cases {
        assert_eq!(rows("sqlite3", q), rows(g, q), "mismatch for `{q}`");
    }
}
