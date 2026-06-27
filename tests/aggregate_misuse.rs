//! An aggregate function used in a clause that filters individual rows — a
//! `WHERE`, an `UPDATE` assignment value, an `IN (…)` list — is a misuse, since
//! aggregation happens after filtering. SQLite rejects it at *prepare* time (so
//! it errors even over an empty or fully-filtered table) with one of two
//! wordings: `misuse of aggregate: f()` when the statement is itself an aggregate
//! query (it has GROUP BY/HAVING or an aggregate in the result columns), and
//! `misuse of aggregate function f()` otherwise. graphite previously evaluated
//! the predicate lazily per row, so it emitted a different message — or, over an
//! empty/filtered table, silently accepted the statement. Matched to the
//! `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run a statement through the method that accepts it (`query` for `SELECT`,
/// `execute` otherwise) and return the resulting error string.
fn err_of(c: &mut Connection, sql: &str) -> String {
    if sql.trim_start()[..6].eq_ignore_ascii_case("select") {
        c.query(sql).unwrap_err().to_string()
    } else {
        c.execute(sql).unwrap_err().to_string()
    }
}

#[test]
fn aggregate_in_where_is_rejected_even_on_empty_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // The table is empty, so lazy per-row evaluation would never fire — these
    // must still be rejected at prepare time.
    for (sql, msg) in [
        (
            "SELECT 1 FROM t WHERE sum(b) > 0",
            "misuse of aggregate function sum()",
        ),
        (
            "DELETE FROM t WHERE max(a) > 0",
            "misuse of aggregate function max()",
        ),
        (
            "UPDATE t SET a = 1 WHERE count(*) > 0",
            "misuse of aggregate function count()",
        ),
        (
            "UPDATE t SET a = sum(b)",
            "misuse of aggregate function sum()",
        ),
        (
            "SELECT a FROM t WHERE a IN (total(b))",
            "misuse of aggregate function total()",
        ),
    ] {
        assert_eq!(err_of(&mut c, sql), format!("error: {msg}"), "for {sql}");
    }
}

#[test]
fn aggregate_query_uses_the_colon_wording() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // When the statement *is* an aggregate query, the misuse wording drops the
    // word "function" and gains a colon.
    for sql in [
        "SELECT sum(a) FROM t WHERE sum(a) > 0", // aggregate in result columns
        "SELECT count(*) FROM t WHERE sum(b) > 0", // ditto
        "SELECT a FROM t WHERE sum(b) > 0 GROUP BY a", // GROUP BY
    ] {
        assert_eq!(
            err_of(&mut c, sql),
            "error: misuse of aggregate: sum()",
            "for {sql}"
        );
    }
}

#[test]
fn legitimate_aggregates_are_not_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 2), (3, 4)").unwrap();
    // An aggregate inside a subquery belongs to that query level — the outer
    // WHERE check must not reach into it.
    c.query("SELECT a FROM t WHERE a IN (SELECT sum(b) FROM t)")
        .unwrap();
    c.query("SELECT a FROM t WHERE a = (SELECT max(b) FROM t)")
        .unwrap();
    c.query("SELECT sum(a) FROM t").unwrap();
    c.query("SELECT a FROM t GROUP BY a HAVING sum(b) > 0")
        .unwrap();
    // A DELETE whose predicate's aggregate is safely nested in a subquery runs.
    c.execute("DELETE FROM t WHERE a IN (SELECT sum(b) FROM t)")
        .unwrap();
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
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            // strip the trailing extended result code, e.g. " (1)"
            .trim_end()
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches('(')
            .trim_end()
            .to_string()
    };
    let b = "CREATE TABLE t(a, b);";
    for tail in [
        // function form (non-aggregate query)
        "SELECT 1 FROM t WHERE sum(b) > 0",
        "SELECT * FROM t WHERE sum(a) > 0",
        "SELECT a FROM t WHERE a IN (sum(b))",
        "SELECT a FROM t WHERE sum(b) > 0 ORDER BY a",
        "DELETE FROM t WHERE max(a) > 0",
        "UPDATE t SET a = 1 WHERE count(*) > 0",
        "UPDATE t SET a = sum(b)",
        // colon form (aggregate query)
        "SELECT sum(a) FROM t WHERE sum(a) > 0",
        "SELECT count(*) FROM t WHERE max(b) > 0",
        "SELECT a FROM t WHERE total(b) > 0 GROUP BY a",
        "SELECT a, sum(b) FROM t WHERE count(a) > 0 GROUP BY a",
        // legitimate — runs in both
        "SELECT a FROM t WHERE a IN (SELECT sum(b) FROM t)",
        "SELECT sum(a) FROM t",
        "SELECT a FROM t GROUP BY a HAVING sum(b) > 0",
        // missing column inside the aggregate (resolution order)
        "DELETE FROM t WHERE sum(nope) > 0",
        "SELECT 1 FROM t WHERE sum(nope) > 0",
    ] {
        let sql = format!("{b} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {sql}");
    }
}
