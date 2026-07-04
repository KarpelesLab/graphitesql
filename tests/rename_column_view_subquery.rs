//! `ALTER TABLE … RENAME COLUMN` must rewrite the renamed column everywhere a
//! dependent view reaches it — including references buried in a *nested*
//! subquery whose own `FROM` is the renamed table, even when the view's
//! top-level `FROM` is an unrelated base table (so the single-source and join
//! provers don't apply).
//!
//! graphite's column-rename propagation rewrites a view's stored schema text
//! token-by-token, guarded by provers that decide which `old` tokens are safe to
//! rename. The single-source prover bails because the top-level `FROM` is a
//! different table; the "only table" prover bails because the view names two
//! tables; the join prover bails on any subquery. So a view like
//! `SELECT c FROM u WHERE c IN (SELECT a FROM t)` was left untouched after
//! renaming `t.a`, leaving a stale `SELECT a FROM t` that no longer matched the
//! table — the view then errored `no such column: a` (a real breakage, not just
//! a cosmetic schema diff).
//!
//! The global-uniqueness prover closes this: it collects every base-table source
//! at every nesting level and, when the renamed column name is unique across all
//! of them, a bare `old` can only bind to the renamed table, so every reference
//! (bare and `<table>.old`/`<alias>.old` qualified) is rewritten.
//!
//! When the name is *not* globally unique (several sources own a column `old`) a
//! scope-aware pass resolves each bare `old` innermost-scope-first (A-rn3-edge):
//! if every bare `old` binds to the renamed table the whole-text rewrite is still
//! safe; if none does, only the qualified `renamed.old` refs rewrite and the bare
//! tokens (owned by another scope) are left alone. Only a genuinely *mixed* body
//! — some bare `old` binding to the renamed table and some to another in the same
//! statement — still needs per-occurrence source spans, so that one case declines
//! entirely and leaves the view byte-identical (never a half-renamed body). Any
//! non-base source (a derived subquery/TVF, CTE, NATURAL/USING join) likewise
//! bails the whole view untouched. Those remain the A-rn3-edge residual.
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
fn rename_column_rewrites_nested_subquery_refs_in_view() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // The found case: bare `a` in `IN (SELECT a FROM t)`, top FROM is `u`.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c FROM u WHERE c IN (SELECT a FROM t); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Same view stays *queryable* after the rename (the functional check).
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         INSERT INTO t VALUES(5,9); INSERT INTO u VALUES(5); \
         CREATE VIEW v AS SELECT c FROM u WHERE c IN (SELECT a FROM t); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT * FROM v",
        // Scalar subquery in a result column.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c, (SELECT a FROM t LIMIT 1) FROM u; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // EXISTS subquery in the WHERE, correlated to the outer table.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c FROM u WHERE EXISTS(SELECT 1 FROM t WHERE a=c); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Qualified `t.a` inside the subquery.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c FROM u WHERE c IN (SELECT t.a FROM t); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Aliased renamed table inside the subquery (`t x` → `x.a`).
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c FROM u WHERE c IN (SELECT x.a FROM t x); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Self-join of the renamed table inside the subquery, unique column.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c FROM u \
           WHERE c IN (SELECT t1.a FROM t t1 JOIN t t2 ON t1.b=t2.b); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Two levels of nesting (`u` → `w` → `t`).
        "CREATE TABLE t(a,b); CREATE TABLE u(c); CREATE TABLE w(d); \
         CREATE VIEW v AS SELECT c FROM u \
           WHERE c IN (SELECT d FROM w WHERE d IN (SELECT a FROM t)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Renaming a column the view never references leaves it byte-unchanged.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c FROM u WHERE c IN (SELECT a FROM t); \
         ALTER TABLE t RENAME COLUMN b TO bb; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Scope-aware (A-rn3-edge): both `t` and `u` own an `a`, so the name is
        // not globally unique — yet the only `a` the subquery reaches is the
        // qualified `t.a`, and the outer `u.a` refs stay put. The scope pass
        // rewrites just `t.a` → `t.aa`, matching SQLite's per-scope resolution.
        "CREATE TABLE t(a,b); CREATE TABLE u(a); \
         CREATE VIEW v AS SELECT u.a FROM u WHERE u.a IN (SELECT t.a FROM t); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Scope-aware: bare `a` inside the subquery binds to the inner `t`, while
        // the outer scope is `u` (which also owns `a`). Renaming `t.a` rewrites the
        // inner bare `a`; the outer `u.a` is untouched.
        "CREATE TABLE t(a,b); CREATE TABLE u(a); \
         CREATE VIEW v AS SELECT u.a, (SELECT a FROM t LIMIT 1) AS x FROM u; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Scope-aware, other direction: renaming the *outer* table's `a` must NOT
        // touch the inner bare `a` (which binds to `t`); only the qualified `u.a`
        // references rewrite.
        "CREATE TABLE t(a,b); CREATE TABLE u(a); \
         CREATE VIEW v AS SELECT u.a, (SELECT a FROM t LIMIT 1) AS x FROM u; \
         ALTER TABLE u RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }

    // Bail invariant: a genuinely *mixed* body is the remaining A-rn3-edge
    // residual — the same bare `old` name binds to *different* tables in one
    // statement, so a whole-text token rewrite can't tell them apart without
    // per-occurrence source spans. Here `SELECT a FROM t WHERE a IN (SELECT a
    // FROM u)` has the outer bare `a` bound to `t` and the inner bare `a` bound to
    // `u`; renaming `u.a` should touch only the inner one. graphite declines and
    // leaves the stored view byte-identical — never a half-renamed (corrupting)
    // body. SQLite resolves each ref by scope and rewrites just the inner `a`, so
    // this stays a known gap rather than a differential equality; the invariant
    // guards against a *wrong* (partial/corrupting) rewrite creeping in.
    let bail = [(
        "CREATE TABLE t(a,b); CREATE TABLE u(a); \
         CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT a FROM u); \
         ALTER TABLE u RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
        "CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT a FROM u)",
    )];
    for (sql, unchanged) in bail {
        assert_eq!(out(g, sql), unchanged, "bail invariant for {sql}");
    }
}
