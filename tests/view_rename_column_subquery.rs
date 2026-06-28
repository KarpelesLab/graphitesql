//! `ALTER TABLE … RENAME COLUMN` must rewrite references to the renamed column
//! everywhere a dependent view names it — including *inside* expression
//! subqueries (a scalar `(SELECT …)`, `EXISTS`, or `x IN (SELECT …)`). graphite
//! previously bailed out of the entire view rewrite the moment the body held any
//! subquery, leaving stale references behind so the view became unqueryable
//! (`no such column: a`).
//!
//! graphite now rewrites a single-source view whose subqueries reference only
//! the renamed table — at every nesting level, bare and `<alias>.`-qualified
//! references alike. It still conservatively leaves the view untouched (a known
//! gap, not a regression) when a token rewrite can't be proven safe: a subquery
//! touching another table, a derived table in a `FROM`, or a result-column alias
//! that collides with the renamed column name. Those bail cases are asserted to
//! leave the stored view SQL byte-identical (no *wrong* rewrite).
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
fn view_rename_column_subquery_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    // Single-source views whose subqueries reference only the renamed table:
    // the rename now rewrites every reference (bare + qualified, nested too), so
    // the stored schema AND the view's rows stay byte-exact with SQLite.
    let matching = [
        // Scalar subquery, bare + `t2.`/`t.` qualified.
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a, (SELECT b FROM t t2 WHERE t2.a=t.a) AS m \
         FROM t; ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
        // EXISTS.
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a FROM t WHERE \
         EXISTS(SELECT 1 FROM t t2 WHERE t2.a=t.a); ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // NOT EXISTS.
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a FROM t WHERE \
         NOT EXISTS(SELECT 1 FROM t t2 WHERE t2.a<t.a); ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // x IN (SELECT …).
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT a FROM t t2); \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
        // Two levels of nesting, with data so the rows are compared too.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,10),(2,20); \
         CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT a FROM t t2 WHERE a IN \
         (SELECT a FROM t t3)); ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'; SELECT * FROM v ORDER BY aa",
        // Subquery in HAVING.
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a FROM t GROUP BY a HAVING a > \
         (SELECT min(a) FROM t t2); ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Correlated scalar subquery + rows.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,10),(2,20); \
         CREATE VIEW v AS SELECT a, (SELECT max(b) FROM t t2 WHERE t2.a<=t.a) AS m FROM t; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'; \
         SELECT * FROM v ORDER BY aa",
        // Renaming the *other* column must not disturb the subquery's `a` refs.
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a, (SELECT b FROM t t2 WHERE t2.a=t.a) AS m \
         FROM t; ALTER TABLE t RENAME COLUMN b TO bb; SELECT sql FROM sqlite_schema WHERE name='v'",
        // A FROM-less subquery (references no table) is fine.
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a, (SELECT 1) AS one FROM t; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
    ];
    for sql in matching {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }

    // Bail cases: a token rewrite can't be proven safe, so graphite leaves the
    // stored view SQL byte-identical. (SQLite does more here — rewriting through
    // the other table, or aborting on a derived table — so these stay known gaps
    // rather than differential equalities. The invariant guards against a *wrong*
    // rewrite creeping in.)
    let bail = [
        // Subquery references another table.
        (
            "CREATE TABLE t(a,b); CREATE TABLE other(id); \
             CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT id FROM other); \
             ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
            "CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT id FROM other)",
        ),
        // Result-column alias collides with the renamed column name.
        (
            "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT b AS a, a FROM t; \
             ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
            "CREATE VIEW v AS SELECT b AS a, a FROM t",
        ),
        // Derived table in the FROM.
        (
            "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a FROM (SELECT a FROM t); \
             ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
            "CREATE VIEW v AS SELECT a FROM (SELECT a FROM t)",
        ),
        // Scalar subquery over another table.
        (
            "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
             CREATE VIEW v AS SELECT a, (SELECT c FROM u WHERE u.a=t.a) FROM t; \
             ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
            "CREATE VIEW v AS SELECT a, (SELECT c FROM u WHERE u.a=t.a) FROM t",
        ),
    ];
    for (sql, unchanged) in bail {
        assert_eq!(run(g, sql), unchanged, "bail invariant for {sql}");
    }
}
