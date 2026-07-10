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
//! tokens (owned by another scope) are left alone. A genuinely *mixed* body — the
//! same bare `old` name binding to the renamed table in one scope and to another
//! table in a different scope of the same statement — is now handled too: each
//! bare `Expr::Column` carries its source byte span, so the rewrite renames
//! exactly the occurrences that bind to the renamed table and leaves the rest
//! (A-rn3-edge closed for views). Compound (UNION/INTERSECT/EXCEPT) and `WITH` CTE
//! bodies are handled too: each compound arm and each CTE body is an independent
//! scope, so only the refs that bind to the renamed base table rewrite; an outer
//! reference to a CTE's renamed output column stays unresolved and bails the whole
//! view (matching SQLite, which rejects such a rename). The remaining residual is
//! a derived-table/TVF/NATURAL-USING source, a compound `ORDER BY`, or a CTE
//! consumed inside a compound arm — those bail the whole view untouched.
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
        // Mixed body (A-rn3-edge, now handled via per-occurrence spans): the same
        // bare `a` binds to `t` in the outer scope and to `u` in the subquery of
        // one statement. Renaming `u.a` rewrites only the inner bare `a`; the
        // outer bare `a` (bound to `t`) stays. Byte-matches SQLite's per-scope
        // resolution instead of declining.
        "CREATE TABLE t(a,b); CREATE TABLE u(a); \
         CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT a FROM u); \
         ALTER TABLE u RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Mixed, other direction: renaming `t.a` rewrites the outer bare `a`
        // (bound to `t`) and leaves the subquery's bare `a` (bound to `u`).
        "CREATE TABLE t(a,b); CREATE TABLE u(a); \
         CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT a FROM u); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Mixed with multiple outer occurrences + a scalar subquery: both outer
        // bare `a` rewrite, the subquery's `a` (bound to `u`) stays.
        "CREATE TABLE t(a,b); CREATE TABLE u(a); \
         CREATE VIEW v AS SELECT a, a+1 AS a2, (SELECT a FROM u LIMIT 1) AS ux FROM t; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Mixed body but renaming an unrelated column leaves the view unchanged.
        "CREATE TABLE t(a,b); CREATE TABLE u(a); \
         CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT a FROM u); \
         ALTER TABLE t RENAME COLUMN b TO bb; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Compound (UNION/INTERSECT/EXCEPT): each arm is an independent scope, so
        // only the arm bound to the renamed table rewrites. Here `a` in the first
        // arm binds to `t`, the second to `u` — renaming `t.a` rewrites just the
        // first arm's `a`.
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE VIEW v AS SELECT a FROM t UNION SELECT a FROM u; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Compound, three arms, only the bound arm rewrites.
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); CREATE TABLE w(a,d); \
         CREATE VIEW v AS SELECT a FROM t UNION SELECT a FROM u UNION ALL SELECT a FROM w; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Compound whose arm nests a mixed subquery: the first arm's outer `a`
        // (bound to `u`) stays, its subquery `a` (bound to `t`) and the second
        // arm's `a` (bound to `t`) rewrite. Renaming `t.a`.
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE VIEW v AS SELECT c FROM u WHERE c IN (SELECT a FROM t) UNION SELECT a FROM t; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Compound with a qualified ref in each arm.
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE VIEW v AS SELECT t.a FROM t UNION SELECT u.a FROM u; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Compound nested inside a scalar subquery.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c FROM u WHERE c IN (SELECT a FROM t UNION SELECT a FROM t); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // CTE body over the renamed table, consumed by `SELECT *`: the body rewrites
        // (`a`→`aa`), the outer `*` is unaffected. The CTE's exposed column changes,
        // but no consumer references it by the old name, so SQLite (and graphite)
        // succeed.
        "CREATE TABLE t(a,b); \
         CREATE VIEW v AS WITH x AS (SELECT a FROM t) SELECT * FROM x; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // CTE with an explicit column list fixes the exposed name, so the body
        // rewrites and the outer `SELECT k` is unaffected.
        "CREATE TABLE t(a,b); \
         CREATE VIEW v AS WITH x(k) AS (SELECT a FROM t) SELECT k FROM x; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // CTE body with a join.
        "CREATE TABLE t(a,b); CREATE TABLE u(c,d); \
         CREATE VIEW v AS WITH x AS (SELECT a FROM t JOIN u ON t.b=u.c) SELECT * FROM x; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Two CTE bodies, both over the renamed table.
        "CREATE TABLE t(a,b); \
         CREATE VIEW v AS WITH x AS (SELECT a FROM t), y AS (SELECT a FROM t) SELECT * FROM x,y; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // CTE body nesting a mixed subquery (renamed `t.a` inside, another `u.a`
        // stays).
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE VIEW v AS WITH x AS (SELECT a FROM t WHERE a IN (SELECT a FROM u)) SELECT * FROM x; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
        // Renaming a column the CTE view never references leaves it byte-unchanged.
        "CREATE TABLE t(a,b); \
         CREATE VIEW v AS WITH x AS (SELECT a FROM t) SELECT * FROM x; \
         ALTER TABLE t RENAME COLUMN b TO bb; \
         SELECT sql FROM sqlite_schema WHERE name='v'",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }

    // CTE bail: an outer reference to a CTE's renamed OUTPUT column (`SELECT a FROM
    // x` where `x`'s body projects the renamed `t.a`) makes the reference
    // unresolvable — graphite leaves the stored view byte-identical. SQLite rewrites
    // the CTE body and then rejects the whole rename (the CTE's exposed column
    // became `aa`, so the outer `a` no longer resolves), so its stored view is *also*
    // left unchanged; only SQLite additionally errors (that reject is A-alter-
    // rollback's job). Here the stored SQL matches; the invariant guards against a
    // wrong rewrite.
    let cte_bail = [(
        "CREATE TABLE t(a,b); \
         CREATE VIEW v AS WITH x AS (SELECT a FROM t) SELECT a FROM x; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
        "CREATE VIEW v AS WITH x AS (SELECT a FROM t) SELECT a FROM x",
    )];
    for (sql, unchanged) in cte_bail {
        assert_eq!(out(g, sql), unchanged, "cte bail for {sql}");
    }

    // Compound bail: a compound-level `ORDER BY` binds to the FIRST arm's OUTPUT
    // column, which a base-table token rewrite can't model, so graphite declines
    // and leaves the view byte-identical (SQLite rewrites both the arm and the
    // matching `ORDER BY` term — a known gap, not a wrong rewrite). Guards against
    // a corrupting partial rewrite.
    let compound_bail = [(
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE VIEW v AS SELECT a FROM t UNION SELECT a FROM u ORDER BY a; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT sql FROM sqlite_schema WHERE name='v'",
        "CREATE VIEW v AS SELECT a FROM t UNION SELECT a FROM u ORDER BY a",
    )];
    for (sql, unchanged) in compound_bail {
        assert_eq!(out(g, sql), unchanged, "compound ORDER BY bail for {sql}");
    }

    // Functional check: the mixed-body view stays queryable after the rename (the
    // rewrite kept each bare `a` bound to the right table).
    if sqlite3_available() {
        let q = "CREATE TABLE t(a,b); CREATE TABLE u(a); \
                 INSERT INTO t VALUES(5,1); INSERT INTO u VALUES(5); \
                 CREATE VIEW v AS SELECT a FROM t WHERE a IN (SELECT a FROM u); \
                 ALTER TABLE u RENAME COLUMN a TO aa; SELECT * FROM v";
        assert_eq!(out("sqlite3", q), out(g, q), "mixed body stays queryable");
    }
}
