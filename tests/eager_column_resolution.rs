//! SQLite resolves every column reference at prepare time, so a reference to a
//! non-existent column errors with `no such column: …` *regardless of the data*
//! — even when the table is empty, or when a `WHERE` clause filters out every
//! row, so projection/`WHERE` evaluation never actually touches the bad name.
//!
//! graphite's tree-walker resolves columns lazily, during per-row evaluation, so
//! a result that reaches no row used to silently swallow the error and return an
//! empty result set. `Executor::validate_columns_exist` closes that gap for the
//! cases it can settle without any chance of a false positive: a bare/qualified
//! reference in the projection or `WHERE` of a top-level, window-free block whose
//! every `FROM` source is a plain (non-virtual, non-subquery, non-TVF) base
//! table/view joined only by `ON`/cross joins (no `NATURAL`/`USING`).
//!
//! Each case is checked against the real sqlite3 CLI: graphite must error exactly
//! when sqlite does, and a *valid* query over the same empty/filtered data must
//! still succeed (the check must never reject a real column).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE e(a INT, b TEXT);\n\
    CREATE TABLE t(a INT, b TEXT);\n\
    INSERT INTO t VALUES (1,'x'),(2,'y');\n\
    CREATE TABLE u(c INT, d TEXT);\n\
    INSERT INTO u VALUES (1,'p');\n\
    CREATE VIEW v AS SELECT a AS va, b AS vb FROM t;\n";

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c
}

/// `true` if the real sqlite3 CLI reports an error for `query` over `SETUP`.
fn sqlite_errors(query: &str) -> bool {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg("-ascii")
        .arg(format!("{SETUP}{query};"))
        .output()
        .unwrap();
    // sqlite3 prints the error to stderr and exits non-zero.
    !out.status.success() || !out.stderr.is_empty()
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The exact error text sqlite3 prints for `sql` (without the `Error: ` prefix
/// or the `in prepare, ` location note), or `None` if it accepts the statement.
fn sqlite_error_text(sql: &str) -> Option<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    if err.is_empty() {
        return None;
    }
    let msg = err.strip_prefix("Error: ").unwrap_or(err);
    let msg = msg.strip_prefix("in prepare, ").unwrap_or(msg);
    Some(msg.to_string())
}

#[test]
fn missing_column_errors_match_sqlite() {
    let queries = &[
        // Empty table: projection references a column that does not exist.
        "SELECT nope FROM e",
        // Empty table: qualified missing column.
        "SELECT e.nope FROM e",
        // Empty table: missing column only in WHERE.
        "SELECT a FROM e WHERE nope = 1",
        // Non-empty table but WHERE filters out every row before projection.
        "SELECT nope FROM t WHERE 0",
        "SELECT a, nope FROM t WHERE a > 100",
        // Missing column inside a function argument.
        "SELECT count(nope) FROM e",
        "SELECT upper(nope) FROM t WHERE 0",
        // Missing column inside CASE / arithmetic / IN list.
        "SELECT CASE WHEN nope THEN 1 END FROM t WHERE 0",
        "SELECT a + nope FROM t WHERE 0",
        "SELECT a FROM t WHERE a IN (nope, 1)",
        // Qualified by a real table but the column is wrong.
        "SELECT t.nope FROM t",
        // Two-table ON join: a missing column in the projection.
        "SELECT t.a, nope FROM t JOIN u ON t.a = u.c WHERE 0",
        // Wrong qualifier on an otherwise-real column name.
        "SELECT u.a FROM t JOIN u ON t.a = u.c",
        // Missing column in the join `ON` predicate itself.
        "SELECT t.a FROM t JOIN u ON t.nope = u.c",
        "SELECT t.a FROM t JOIN u ON t.a = u.nope",
        // Missing column referencing a view's (renamed) output.
        "SELECT b FROM v",
        // `table.*` whose qualifier names no FROM source → `no such table: x`.
        "SELECT x.* FROM t",
        "SELECT t.a, z.* FROM t JOIN u ON t.a = u.c",
        // A *qualified* missing ref in GROUP BY / HAVING / ORDER BY (never an
        // output alias or ordinal, so it must resolve to a base column).
        "SELECT a FROM t GROUP BY t.nope",
        "SELECT a FROM t GROUP BY a HAVING t.nope > 0",
        "SELECT a FROM t ORDER BY t.nope",
    ];
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in queries {
        let graphite_err = c.query(q).is_err();
        let sqlite_err = sqlite_errors(q);
        assert!(
            sqlite_err,
            "test bug: expected sqlite3 to reject `{q}` but it did not"
        );
        assert!(
            graphite_err,
            "graphite accepted `{q}` but sqlite3 rejects it (missing-column not caught eagerly)"
        );
    }
}

#[test]
fn missing_column_in_dml_matches_sqlite() {
    // DELETE/UPDATE resolve their WHERE and SET-value columns eagerly too — a
    // bogus column errors over an empty table, instead of silently affecting 0
    // rows. (`t` here has rows, but `WHERE 0`/`a > 100` filters them all out, so
    // lazy resolution would never reach the bad name.)
    let stmts = &[
        "DELETE FROM t WHERE nope = 1",
        "DELETE FROM t WHERE t.nope = 1",
        "DELETE FROM e WHERE nope = 1",
        "UPDATE t SET a = 1 WHERE nope = 2",
        "UPDATE t SET a = nope WHERE a > 100",
        "UPDATE t SET a = 1 WHERE 0 AND b = upper(nope)",
        "UPDATE t SET nope = 1",
        "UPDATE e SET a = nope",
    ];
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let mut c = conn();
    for q in stmts {
        let graphite_err = c.execute(q).is_err();
        let sqlite_err = sqlite_errors(q);
        assert!(
            sqlite_err,
            "test bug: expected sqlite3 to reject `{q}` but it did not"
        );
        assert!(
            graphite_err,
            "graphite accepted `{q}` but sqlite3 rejects it (missing-column not caught eagerly)"
        );
    }
}

#[test]
fn valid_dml_over_empty_or_filtered_data_still_succeeds() {
    // The DML eager check must never reject a real column: valid DELETE/UPDATE
    // over an empty table, a fully-filtered result, qualified refs, rowid, and a
    // SET value reading another real column must all still run.
    let setup = "\
        CREATE TABLE e(a INT, b TEXT);\n\
        CREATE TABLE t(a INT, b TEXT);\n\
        INSERT INTO t VALUES (1,'x'),(2,'y');\n";
    let mut c = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        let s = s.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    let ok = &[
        "DELETE FROM e WHERE a = 1",
        "DELETE FROM t WHERE 0",
        "DELETE FROM t WHERE t.a > 100",
        "DELETE FROM t WHERE rowid = 999",
        "UPDATE e SET a = 1",
        "UPDATE t SET a = b WHERE 0",
        "UPDATE t SET a = a + 1 WHERE t.b = 'nope'",
        "UPDATE t SET a = 1 WHERE rowid < 0",
    ];
    for q in ok {
        assert!(
            c.execute(q).is_ok(),
            "graphite wrongly rejected the valid statement `{q}`"
        );
    }
}

#[test]
fn valid_queries_over_empty_or_filtered_data_still_succeed() {
    // The eager check must never reject a *real* column, including over an empty
    // table, a fully-filtered result, a view's renamed outputs, rowid aliases,
    // and date/time keyword pseudo-columns.
    let queries = &[
        "SELECT a FROM e",
        "SELECT a, b FROM e",
        "SELECT e.a, e.b FROM e",
        "SELECT a FROM t WHERE 0",
        "SELECT a, b FROM t WHERE a > 100",
        "SELECT count(a) FROM e",
        "SELECT rowid, a FROM e",
        "SELECT _rowid_ FROM t WHERE 0",
        "SELECT va, vb FROM v",
        "SELECT v.va FROM v WHERE 0",
        "SELECT t.a, u.d FROM t JOIN u ON t.a = u.c WHERE 0",
        "SELECT CURRENT_DATE FROM e",
        // `table.*` qualified by a real source (incl. an alias) still resolves.
        "SELECT t.* FROM t WHERE 0",
        "SELECT t.*, u.* FROM t JOIN u ON t.a = u.c WHERE 0",
        "SELECT x.* FROM t AS x WHERE 0",
    ];
    let c = conn();
    for q in queries {
        assert!(
            c.query(q).is_ok(),
            "graphite wrongly rejected the valid query `{q}`"
        );
    }
}

#[test]
fn insert_column_resolution_messages_match_sqlite() {
    // SQLite rejects a bad INSERT at prepare time with one of three messages:
    //   * unknown column        → `table T has no column named C`
    //   * explicit-list count   → `M values for N columns`
    //   * implicit/SELECT count → `table T has N columns but M values were supplied`
    // graphite must produce the byte-identical text (not just *an* error).
    let cases: &[(&str, &str)] = &[
        // Unknown column in an explicit column list (rowid table).
        ("CREATE TABLE t(a,b);", "INSERT INTO t(zz) VALUES(1)"),
        // Count mismatch with an explicit column list.
        ("CREATE TABLE t(a,b);", "INSERT INTO t(a) VALUES(1,2)"),
        // Count mismatch, bare INSERT (implicit column list).
        ("CREATE TABLE t(a,b);", "INSERT INTO t VALUES(1)"),
        // Count mismatch via INSERT … SELECT (implicit).
        ("CREATE TABLE t(a,b);", "INSERT INTO t SELECT 1"),
        // Generated column: the implicit list counts only the 2 stored columns.
        ("CREATE TABLE g(a,b,c AS (a+b));", "INSERT INTO g VALUES(1)"),
        // Same three shapes for a WITHOUT ROWID table.
        (
            "CREATE TABLE w(a,b,PRIMARY KEY(a)) WITHOUT ROWID;",
            "INSERT INTO w(zz) VALUES(1)",
        ),
        (
            "CREATE TABLE w(a,b,PRIMARY KEY(a)) WITHOUT ROWID;",
            "INSERT INTO w(a) VALUES(1,2)",
        ),
        (
            "CREATE TABLE w(a,b,PRIMARY KEY(a)) WITHOUT ROWID;",
            "INSERT INTO w VALUES(1)",
        ),
        (
            "CREATE TABLE w(a,b,PRIMARY KEY(a)) WITHOUT ROWID;",
            "INSERT INTO w SELECT 1",
        ),
    ];
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    for (setup, stmt) in cases {
        let mut c = Connection::open_memory().unwrap();
        c.execute(setup).unwrap();
        let graphite = c
            .execute(stmt)
            .expect_err(&format!("graphite accepted `{stmt}` but it is invalid"))
            .to_string()
            .trim_start_matches("error: ")
            .to_string();
        let sqlite = sqlite_error_text(&format!("{setup} {stmt};"))
            .unwrap_or_else(|| panic!("test bug: sqlite accepted `{stmt}`"));
        assert_eq!(graphite, sqlite, "INSERT error text diverges for `{stmt}`");
    }
}

#[test]
fn subquery_and_natural_join_paths_are_left_to_lazy_resolution() {
    // These shapes are deliberately *outside* the eager check's conservative
    // scope (correlated outer refs, NATURAL/USING coalescing, derived tables).
    // They must still execute correctly — the guards must not make the check fire
    // and wrongly reject a legitimate reference.
    let c = conn();
    let ok = &[
        // USING coalesces `a`; both sides legitimately expose it.
        "SELECT a FROM t JOIN t AS t2 USING (a) WHERE 0",
        // Derived table: the inner alias is the only valid scope for `x`.
        "SELECT x FROM (SELECT a AS x FROM t) WHERE 0",
        // Correlated subquery referencing the outer table by qualifier.
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.c = t.a) AND 0",
    ];
    for q in ok {
        assert!(
            c.query(q).is_ok(),
            "graphite wrongly rejected the valid query `{q}`"
        );
    }
}
