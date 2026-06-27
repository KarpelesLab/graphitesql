//! Differential tests for an aliased UPDATE/DELETE target table
//! (`UPDATE t AS x SET … WHERE x.a=…`, `DELETE FROM t AS x …`), plus the
//! related rule that `RETURNING` forbids a `TABLE.*` wildcard.
//!
//! SQLite lets a single-table UPDATE/DELETE give its target an alias with the
//! `AS` keyword; the alias then becomes the *sole* qualifier for the target's
//! columns in SET/WHERE/ORDER BY — the real table name no longer resolves
//! there. RETURNING is the documented quirk: it still resolves against the
//! real table name, not the alias.
#![cfg(feature = "std")]

use std::process::Command;

/// Run `sql` against `bin` on a fresh in-memory database and return the first
/// line of output — stdout if any, otherwise the (prefix-stripped) error.
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
        // graphite's CLI frames a library error as `Error: <msg>` where the
        // message itself already begins `error: ` — strip that inner frame too
        // so we compare message content (a pre-existing CLI cosmetic; sqlite
        // has only the single `Error: ` frame).
        let s = s.strip_prefix("error: ").unwrap_or(s);
        // Drop a trailing SQLite error code like " (1)".
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

/// Assert graphite and sqlite3 agree on `sql`.
fn same(sql: &str) {
    let g = run(env!("CARGO_BIN_EXE_graphitesql"), sql);
    let s = run("sqlite3", sql);
    assert_eq!(g, s, "mismatch for SQL: {sql}");
}

#[test]
fn update_delete_alias_parity() {
    if !sqlite3_available() {
        return;
    }

    // Alias as the qualifier in SET, WHERE, ORDER BY.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4); UPDATE t AS x SET b=x.a*10 WHERE x.a=1; SELECT a,b FROM t ORDER BY a;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4); UPDATE t AS x SET b=x.b+1 WHERE x.a>0 ORDER BY x.a LIMIT 1; SELECT a,b FROM t ORDER BY a;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4); DELETE FROM t AS x WHERE x.a=1; SELECT a,b FROM t ORDER BY a;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4),(5,6); DELETE FROM t AS x WHERE x.a>0 ORDER BY x.a DESC LIMIT 1; SELECT a,b FROM t ORDER BY a;");

    // The real table name no longer resolves once an alias is given.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS x SET b=t.a;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS x SET b=1 WHERE t.a=1;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); DELETE FROM t AS x WHERE t.a=1;");

    // A missing aliased column is rejected at prepare time (with the alias
    // qualifier preserved), over both populated and empty tables.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS x SET b=x.nope WHERE x.a=1;");
    same("CREATE TABLE t(a,b); UPDATE t AS x SET b=x.nope;");
    same("CREATE TABLE t(a,b); DELETE FROM t AS x WHERE x.nope=1;");

    // The `rowid` family resolves through the alias.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS x SET b=x.rowid WHERE x.a=1; SELECT a,b FROM t;");

    // RETURNING quirk: it resolves against the real name, not the alias.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS x SET b=99 WHERE x.a=1 RETURNING t.a;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS x SET b=99 WHERE x.a=1 RETURNING x.a;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); DELETE FROM t AS x WHERE x.a=1 RETURNING t.b;");

    // RETURNING forbids a `TABLE.*` wildcard (bare `*` is fine) for all three
    // statement kinds.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2) RETURNING t.*;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2) RETURNING *;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS x SET b=3 WHERE x.a=1 RETURNING x.*;");
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); DELETE FROM t AS x WHERE x.a=1 RETURNING t.*;");

    // Correlated subquery in the SET/WHERE may reference the alias.
    same("CREATE TABLE t(a,b); CREATE TABLE u(k,v); INSERT INTO t VALUES(1,0),(2,0); INSERT INTO u VALUES(1,11),(2,22); UPDATE t AS x SET b=(SELECT v FROM u WHERE u.k=x.a); SELECT a,b FROM t ORDER BY a;");
    same("CREATE TABLE t(a,b); CREATE TABLE u(k); INSERT INTO t VALUES(1,2),(3,4); INSERT INTO u VALUES(1); DELETE FROM t AS x WHERE EXISTS(SELECT 1 FROM u WHERE u.k=x.a); SELECT a,b FROM t ORDER BY a;");

    // A subquery that re-binds the alias name shadows the target (left alone).
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4); UPDATE t AS x SET b=(SELECT max(b) FROM t AS x) WHERE x.a=1; SELECT a,b FROM t ORDER BY a;");

    // Row-value assignment from a subquery, qualified by the alias in WHERE.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS x SET (a,b)=(SELECT 9,8) WHERE x.a=1; SELECT a,b FROM t;");

    // WITHOUT ROWID target.
    same("CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES(1,2),(3,4); UPDATE t AS x SET b=x.a*2 WHERE x.a=1; SELECT a,b FROM t ORDER BY a;");

    // Schema-qualified target with an alias.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE main.t AS x SET b=x.a*3 WHERE x.a=1; SELECT a,b FROM t;");

    // An alias that collides with a column name still resolves correctly.
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t AS b SET a=b.a+1 WHERE b.b=2; SELECT a,b FROM t;");

    // A bare (no `AS`) alias is a syntax error, same as SQLite.
    same("CREATE TABLE t(a,b); UPDATE t x SET b=1;");
    same("CREATE TABLE t(a,b); DELETE FROM t x WHERE a=1;");
}
