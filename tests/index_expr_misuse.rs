//! `CREATE INDEX` rejects aggregate and window functions in a key expression or
//! a partial-index `WHERE` clause, with the exact SQLite diagnostics and
//! precedence.
//!
//! SQLite resolves the key expressions fully, left to right, before it looks at
//! the partial-index predicate, so a fault in any key outranks one in the WHERE.
//! Within a key the order is: unknown column, unknown function, non-determinism
//! (`… prohibited in index expressions`), aggregate misuse, window misuse, dotted
//! reference, unknown collation. The WHERE clause is checked after every key, in
//! the order: subquery, unknown column, unknown function, non-determinism (its
//! own `… partial index WHERE clauses` wording), aggregate, window. graphite used
//! to accept aggregates/windows in either position silently, and used to report a
//! WHERE-clause fault before resolving the key columns.
#![cfg(feature = "std")]

use std::process::Command;

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
fn index_expr_misuse_parity() {
    if !sqlite3_available() {
        return;
    }

    // Aggregate / window in a key expression -> misuse, named by function.
    same("CREATE TABLE t(a); CREATE INDEX i ON t(count(a));");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(sum(a));");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(total(a));");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(a+max(a));");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(row_number() OVER ());");

    // Aggregate / window in a partial WHERE -> misuse.
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE count(b)>0;");
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE row_number() OVER ()>0;");

    // Key-internal precedence: unknown column, unknown function, non-determinism
    // and a dotted reference each outrank a lower-ranked aggregate fault.
    same("CREATE TABLE t(a); CREATE INDEX i ON t(sum(nope));");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(sum(random()));");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(sum(a)+nofunc(a));");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(t.a+sum(a));");

    // Keys are resolved left to right.
    same("CREATE TABLE t(a); CREATE INDEX i ON t(sum(a), nope);");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(nope, sum(a));");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(nope, random());");
    same("CREATE TABLE t(a); CREATE INDEX i ON t(random(), nope);");

    // A key fault outranks any WHERE fault (keys are processed first).
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(sum(a)) WHERE b IN (SELECT 1);");
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(nope) WHERE sum(b)>0;");
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(sum(a)) WHERE nope>0;");

    // WHERE-internal precedence: subquery, unknown column, unknown function and
    // non-determinism each outrank a lower-ranked aggregate fault.
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE sum(b) IN (SELECT 1);");
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE sum(nope)>0;");
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE sum(b)>random();");
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE sum(b)+nofunc(b)>0;");

    // Pre-existing ordering bug, now fixed: an unknown *key* column is reported
    // before a WHERE subquery or a WHERE non-deterministic function.
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(nope) WHERE b IN (SELECT 1);");
    same("CREATE TABLE t(a,b); CREATE INDEX i ON t(nope) WHERE random()>0;");

    // Regression: valid (non-aggregate) indexes still build.
    same("CREATE TABLE t(a,b); CREATE INDEX i1 ON t(a) WHERE b>0; CREATE INDEX i2 ON t(abs(b)); CREATE INDEX i3 ON t(lower(a)) WHERE b IS NOT NULL; SELECT count(*) FROM sqlite_schema WHERE type='index';");
}
