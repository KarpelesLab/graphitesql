//! `GROUP BY` / `HAVING` / `ORDER BY` over a base table must reject a reference
//! to a column that does not exist — SQLite resolves every reference at prepare
//! time, so `SELECT count(*) FROM t GROUP BY a HAVING zzz` is `no such column:
//! zzz` whether or not the table is empty. graphite resolved these clauses
//! lazily: it either reported the wrong error (`misuse of aggregate function
//! count()`) or, for an empty/grouped result, silently returned no rows.
//!
//! An output alias (resolved ahead of a base column) and a positional ordinal
//! remain valid in these clauses, and a base column the projection does not
//! select is still reachable. The eager check covers plain base-table FROM
//! sources only; a derived-table (`FROM (SELECT …)`) source stays lazy, as
//! before. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn missing_column_in_group_having_order_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    for sql in [
        "SELECT count(*) FROM t GROUP BY a HAVING zzz",
        "SELECT count(*) FROM t GROUP BY a HAVING count(zzz) > 0",
        "SELECT count(*) FROM t GROUP BY zzz",
        "SELECT a FROM t GROUP BY a ORDER BY zzz",
        "SELECT a FROM t ORDER BY zzz",
        "SELECT a FROM t WHERE a > 0 ORDER BY zzz",
    ] {
        assert_eq!(err(&c, sql), "no such column: zzz", "for {sql}");
    }
}

#[test]
fn valid_alias_ordinal_and_base_column_refs_still_work() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 2), (1, 3), (2, 9)")
        .unwrap();
    // Each of these must prepare and run without error.
    for sql in [
        // base column not in the projection, bare and qualified
        "SELECT count(*) FROM t GROUP BY a HAVING a > 0",
        "SELECT count(*) FROM t GROUP BY a HAVING t.a > 0",
        "SELECT count(*) FROM t GROUP BY a HAVING sum(b) > 4",
        // output alias resolved ahead of a base column
        "SELECT count(*) AS c FROM t GROUP BY a HAVING c > 1",
        "SELECT a + 1 AS x FROM t GROUP BY x ORDER BY x",
        "SELECT a AS x FROM t ORDER BY x",
        "SELECT count(*) c FROM t GROUP BY a ORDER BY c DESC",
        // positional ordinal
        "SELECT a FROM t GROUP BY 1 ORDER BY 1",
        "SELECT max(b) m FROM t HAVING m > 0",
    ] {
        c.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
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
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    let setup = "CREATE TABLE t(a, b); INSERT INTO t VALUES (1, 2), (1, 3), (2, 9);";
    for tail in [
        "SELECT count(*) FROM t GROUP BY a HAVING zzz",
        "SELECT count(*) FROM t GROUP BY a HAVING count(zzz) > 0",
        "SELECT count(*) FROM t GROUP BY zzz",
        "SELECT a FROM t GROUP BY a ORDER BY zzz",
        "SELECT a FROM t ORDER BY zzz",
        "SELECT count(*) FROM t GROUP BY a HAVING a > 0",
        "SELECT count(*) AS c FROM t GROUP BY a HAVING c > 1",
        "SELECT a + 1 AS x FROM t GROUP BY x ORDER BY x",
        "SELECT count(*) c FROM t GROUP BY a ORDER BY c DESC",
        "SELECT a FROM t GROUP BY 1 ORDER BY 1",
        "SELECT max(b) m FROM t HAVING m > 0",
    ] {
        let sql = format!("{setup}{tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {tail}");
    }
}
