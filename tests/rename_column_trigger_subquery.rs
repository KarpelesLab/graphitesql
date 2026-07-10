//! `ALTER TABLE … RENAME COLUMN` must rewrite the renamed column everywhere a
//! dependent trigger reaches it *across objects* — including references buried in
//! a `WHEN` guard's subquery or a body statement that targets / reads a different
//! table, when the renamed column name is globally unique across every base-table
//! source the trigger touches.
//!
//! graphite's column-rename propagation rewrites a trigger's stored schema text
//! token-by-token, guarded by provers. The single-source prover bails when the
//! body touches more than the renamed table; the `NEW`/`OLD`-only prover (for a
//! trigger attached to the renamed table) rewrites just the pseudo-column refs,
//! leaving bare refs stale; the cross-object single-source prover bails on a
//! subquery over another table. So a trigger like
//! `CREATE TRIGGER tr AFTER INSERT ON u BEGIN UPDATE u SET c=1 WHERE c IN
//! (SELECT a FROM t); END` was left untouched after renaming `t.a`, leaving a
//! stale `SELECT a FROM t` that broke the trigger the next time it fired
//! (`no such column: a`).
//!
//! The global-uniqueness prover closes this: it collects every base-table source
//! at every nesting level (the trigger's own target tables, its `WHEN`/body
//! subquery `FROM`s) and, when the renamed column name is unique across all of
//! them, a bare `old` can only bind to the renamed table, so every reference
//! (bare, `<table>.old`/`<alias>.old`, and — when the trigger is on the renamed
//! table — `NEW.old`/`OLD.old`) is rewritten. When the name is *not* globally
//! unique a scope-aware pass resolves each bare `old` innermost-scope-first; and
//! a genuinely *mixed* body — the same bare name binding to the renamed table in
//! one scope and another table in a different scope — is handled via
//! per-occurrence source spans on `Expr::Column`, rewriting exactly the bound
//! occurrences (A-rn3-edge). Any non-base source (a derived subquery/TVF, CTE,
//! `UPDATE … FROM`) still bails the whole trigger untouched — the remaining
//! residual.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

#[test]
fn rename_column_rewrites_cross_object_trigger_refs() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // The found case: a `WHEN` guard's subquery over the renamed table, on a
        // trigger attached to an unrelated table.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE TRIGGER tr BEFORE INSERT ON u WHEN (SELECT a FROM t LIMIT 1)>0 \
           BEGIN SELECT 1; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // A body statement on another table whose subquery reads the renamed one.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           UPDATE u SET c=1 WHERE c IN (SELECT a FROM t); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Trigger ON the renamed table with a multi-source body: the bare `a`
        // binds to the renamed table (globally unique), so it is rewritten — the
        // `NEW`/`OLD`-only branch would have left it stale.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
           UPDATE t SET b=1 WHERE a IN (SELECT c FROM u); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // `NEW.a` in a `WHEN` guard plus a bare `a` over a multi-source body.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE TRIGGER tr AFTER UPDATE ON t WHEN NEW.a>0 BEGIN \
           UPDATE t SET b=1 WHERE a IN (SELECT c FROM u); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // `OLD.a` in a body assignment, trigger on the renamed table, subquery
        // over another table.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE TRIGGER tr AFTER DELETE ON t BEGIN \
           UPDATE t SET b=OLD.a WHERE a IN (SELECT c FROM u); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // `INSERT … VALUES` whose value expression is a subquery over the renamed
        // table, on a trigger attached to another table.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           INSERT INTO u VALUES((SELECT a FROM t LIMIT 1)); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // `INSERT … SELECT` into another table, reading the renamed one at two
        // nesting levels (a self-join alias inside the subquery).
        "CREATE TABLE t(a,b); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
           INSERT INTO log SELECT a FROM t WHERE a=(SELECT max(a) FROM t t2); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Renaming a column the trigger never references leaves it byte-unchanged.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           UPDATE u SET c=1 WHERE c IN (SELECT a FROM t); END; \
         ALTER TABLE t RENAME COLUMN b TO bb; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // The rewritten trigger still fires correctly: compare schema *and* rows.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         INSERT INTO u VALUES(5); INSERT INTO t VALUES(5,0); \
         CREATE TRIGGER tr AFTER UPDATE OF b ON t BEGIN \
           UPDATE t SET b=99 WHERE a IN (SELECT c FROM u) AND a<>NEW.a; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         INSERT INTO t VALUES(7,1); UPDATE t SET b=2 WHERE aa=7; \
         SELECT sql FROM sqlite_schema WHERE name='tr'; SELECT aa,b FROM t ORDER BY aa",
        // Scope-aware (A-rn3-edge): the body's `UPDATE u` target *also* owns an
        // `a`, so the name is not globally unique — yet the only bare `a` reached
        // is the subquery's, binding to `t`. The scope pass rewrites just that one;
        // the `UPDATE u`'s own columns are untouched. Matches SQLite's per-scope
        // resolution.
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           UPDATE u SET c=1 WHERE c IN (SELECT a FROM t); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Mixed scope (A-rn3-edge, now handled for triggers): the same bare `a`
        // binds to the `UPDATE u` target in the `WHERE` and to `t` in the subquery
        // of one statement. Renaming `t.a` rewrites only the inner `a`; the outer
        // `a` (bound to `u`) stays — per-occurrence source spans disambiguate them,
        // byte-matching SQLite.
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           UPDATE u SET c=1 WHERE a IN (SELECT a FROM t); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }

    // Bail invariant: the remaining A-rn3-edge residual and the non-base-source
    // shapes. graphite leaves the stored trigger byte-identical — never a
    // half-renamed body. SQLite does more here (it resolves each ref by scope, or
    // reaches through the CTE), so these stay known gaps rather than differential
    // equalities; the invariant only guards against a *wrong* (partial/corrupting)
    // rewrite creeping in.
    let bail = [
        // A CTE inside a body subquery is a non-base source the rewrite can't
        // reason about.
        (
            "CREATE TABLE t(a,b); CREATE TABLE u(c); \
             CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
               UPDATE u SET c=(WITH x AS (SELECT a FROM t) SELECT count(*) FROM x); END; \
             ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='tr'",
            "CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
               UPDATE u SET c=(WITH x AS (SELECT a FROM t) SELECT count(*) FROM x); END",
        ),
    ];
    for (sql, unchanged) in bail {
        assert_eq!(out(g, sql), unchanged, "bail invariant for {sql}");
    }
}
