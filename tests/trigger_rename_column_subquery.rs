//! `ALTER TABLE … RENAME COLUMN` must rewrite references to the renamed column
//! everywhere a dependent trigger names it — including *inside* expression
//! subqueries in the trigger's `WHEN` guard and its body statements (a scalar
//! `(SELECT …)`, `EXISTS`, or `x IN (SELECT …)`). graphite previously bailed out
//! of the trigger rewrite the moment the body or `WHEN` held any subquery,
//! leaving stale references behind so the trigger broke when it next fired
//! (`no such column: a`).
//!
//! graphite now rewrites a trigger attached to the renamed table whose body and
//! `WHEN` target only that table — every body statement targets it and every
//! nested subquery references only it — at every nesting level, bare and
//! `<alias>.`/`NEW.`/`OLD.`-qualified references alike. It still conservatively
//! leaves the trigger untouched (a known gap, not a regression) when a token
//! rewrite can't be proven safe: a body statement writing another table, a
//! subquery touching another table, or a derived table in a `FROM`. Those bail
//! cases are asserted to leave the stored trigger SQL byte-identical (no *wrong*
//! rewrite).
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    let mut lines = Vec::new();
    for line in s.lines() {
        let mut t = line.trim_end();
        if t.trim_start().starts_with('^') {
            continue;
        }
        for prefix in [
            "Error: ",
            "in prepare, ",
            "stepping, ",
            "SQL error: ",
            "error: ",
        ] {
            t = t.strip_prefix(prefix).unwrap_or(t);
        }
        lines.push(t.to_string());
    }
    lines.join("\n")
}

#[test]
fn trigger_rename_column_subquery_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    // A trigger on the renamed table whose body/WHEN nest subqueries over only
    // that table: the rename now rewrites every reference (bare, `NEW.`/`OLD.`,
    // and `<alias>.`-qualified, nested too) so the stored schema stays byte-exact.
    let matching = [
        // Scalar subquery in an UPDATE body.
        "CREATE TABLE t(a,b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
         UPDATE t SET b=b+1 WHERE a=(SELECT max(a) FROM t t2); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Subquery in the WHEN guard, plus NEW.<col> in the body.
        "CREATE TABLE t(a,b); CREATE TRIGGER tr AFTER UPDATE ON t \
         WHEN NEW.a > (SELECT min(a) FROM t t2) BEGIN UPDATE t SET b=0 WHERE a=NEW.a; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Scalar subquery in a DELETE body.
        "CREATE TABLE t(a,b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
         DELETE FROM t WHERE a < (SELECT avg(a) FROM t t2); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // INSERT ... SELECT into the same table with a nested IN(SELECT) subquery.
        "CREATE TABLE t(a,b); CREATE TRIGGER tr AFTER UPDATE ON t BEGIN \
         INSERT INTO t(a,b) SELECT a+100,b FROM t t2 WHERE a IN (SELECT a FROM t t3); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // EXISTS in the WHEN guard.
        "CREATE TABLE t(a,b); CREATE TRIGGER tr AFTER INSERT ON t \
         WHEN EXISTS(SELECT 1 FROM t t2 WHERE t2.a < NEW.a) BEGIN UPDATE t SET b=b+1; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Control: no subquery anywhere (was already rewritten correctly).
        "CREATE TABLE t(a,b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
         UPDATE t SET b=b+1 WHERE a=NEW.a; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Renaming the *other* column must not disturb the subquery's `a` refs.
        "CREATE TABLE t(a,b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
         UPDATE t SET b=b+1 WHERE a=(SELECT max(a) FROM t t2); END; \
         ALTER TABLE t RENAME COLUMN b TO bb; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // The rewritten trigger still fires correctly: compare both the schema
        // and the rows it produces over real data.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,10),(2,20); \
         CREATE TRIGGER tr AFTER UPDATE OF b ON t BEGIN \
         UPDATE t SET b=b+1 WHERE a=(SELECT max(a) FROM t t2) AND a<>NEW.a; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; UPDATE t SET b=99 WHERE aa=1; \
         SELECT sql FROM sqlite_schema WHERE name='tr'; SELECT aa,b FROM t ORDER BY aa",
    ];
    for sql in matching {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }

    // Bail cases: a token rewrite can't be proven safe, so graphite leaves the
    // stored trigger SQL byte-identical. (SQLite does more here — rewriting
    // through the other table — so these stay known gaps rather than differential
    // equalities. The invariant guards against a *wrong* rewrite creeping in.)
    let bail = [
        // Body statement writes another table.
        (
            "CREATE TABLE t(a,b); CREATE TABLE log(x); \
             CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             INSERT INTO log SELECT a FROM t WHERE a=(SELECT max(a) FROM t t2); END; \
             ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             INSERT INTO log SELECT a FROM t WHERE a=(SELECT max(a) FROM t t2); END",
        ),
        // Nested subquery references another table.
        (
            "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
             CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             UPDATE t SET b=b+1 WHERE a IN (SELECT a FROM u); END; \
             ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             UPDATE t SET b=b+1 WHERE a IN (SELECT a FROM u); END",
        ),
        // Derived table in a nested subquery's FROM.
        (
            "CREATE TABLE t(a,b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             UPDATE t SET b=b+1 WHERE a IN (SELECT a FROM (SELECT a FROM t)); END; \
             ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             UPDATE t SET b=b+1 WHERE a IN (SELECT a FROM (SELECT a FROM t)); END",
        ),
    ];
    for (sql, unchanged) in bail {
        assert_eq!(run(g, sql), unchanged, "bail invariant for {sql}");
    }
}
