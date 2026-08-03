//! When one row violates several UNIQUE / PRIMARY KEY constraints that declare
//! DIFFERENT `ON CONFLICT` actions, SQLite resolves it by constraint-CHECK order,
//! not by a simple priority: rowid/IPK first, then indexes in reverse creation
//! order, with REPLACE constraints effectively ordered last. The first violated
//! constraint whose action halts (ABORT/FAIL/ROLLBACK) or skips (IGNORE) wins;
//! REPLACE only applies if every violated constraint resolves to REPLACE. The
//! ABORT error names the deciding constraint. graphite previously used a
//! first-wins model. Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

/// Insert a row conflicting on both `a` and `b` (different existing rows) into
/// `t(a UNIQUE ON CONFLICT <A>, b UNIQUE ON CONFLICT <B>)` seeded with (1,10),
/// (2,20). Returns the surviving rows as "a:b,…" or the error message.
fn outcome(a: &str, b: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(&format!(
        "CREATE TABLE t(a UNIQUE ON CONFLICT {a}, b UNIQUE ON CONFLICT {b});
         INSERT INTO t VALUES(1,10),(2,20);"
    ))
    .unwrap();
    match c.execute("INSERT INTO t VALUES(1,20)") {
        Err(e) => e.to_string(),
        Ok(_) => c
            .query("SELECT a, b FROM t ORDER BY a")
            .unwrap()
            .rows
            .iter()
            .map(|r| match (&r[0], &r[1]) {
                (Value::Integer(a), Value::Integer(b)) => format!("{a}:{b}"),
                o => panic!("{o:?}"),
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}

#[test]
fn abort_beats_replace_naming_the_abort_constraint() {
    // b is checked first (reverse declaration). a=ABORT, b=REPLACE: b replaces
    // (deferred), a aborts -> error on t.a.
    assert!(outcome("ABORT", "REPLACE").contains("UNIQUE constraint failed: t.a"));
    // a=REPLACE, b=ABORT: b (checked first) aborts -> error on t.b.
    assert!(outcome("REPLACE", "ABORT").contains("UNIQUE constraint failed: t.b"));
}

#[test]
fn ignore_wins_only_when_checked_before_the_other() {
    // b checked first. a=ABORT, b=IGNORE -> b ignores, row skipped (unchanged).
    assert_eq!(outcome("ABORT", "IGNORE"), "1:10,2:20");
    // a=IGNORE, b=ABORT -> b aborts first.
    assert!(outcome("IGNORE", "ABORT").contains("UNIQUE constraint failed: t.b"));
}

#[test]
fn replace_then_ignore_deletes_nothing() {
    // In both orders the IGNORE constraint sorts ahead of the REPLACE one, so the
    // REPLACE delete never executes.
    assert_eq!(outcome("REPLACE", "IGNORE"), "1:10,2:20");
    assert_eq!(outcome("IGNORE", "REPLACE"), "1:10,2:20");
}

#[test]
fn all_replace_deletes_all_conflicts() {
    // a=REPLACE, b=REPLACE: both conflicting rows deleted, new row inserted.
    assert_eq!(outcome("REPLACE", "REPLACE"), "1:20");
}

#[test]
fn abort_names_last_declared_of_several() {
    // a,b,c all UNIQUE (ABORT): checked reverse-declaration, so c names the error.
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch("CREATE TABLE t(a UNIQUE, b UNIQUE, c UNIQUE); INSERT INTO t VALUES(1,1,1);")
        .unwrap();
    let e = c
        .execute("INSERT INTO t VALUES(1,1,1)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("UNIQUE constraint failed: t.c"), "got: {e}");
}

#[test]
fn rowid_ipk_checked_before_unique() {
    // pk=REPLACE, a=ABORT (same row): pk replaces (deferred), a aborts -> t.a.
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(k INTEGER PRIMARY KEY ON CONFLICT REPLACE, a UNIQUE ON CONFLICT ABORT);
         INSERT INTO t VALUES(1,10);",
    )
    .unwrap();
    let e = c
        .execute("INSERT INTO t VALUES(1,10)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("UNIQUE constraint failed: t.a"), "got: {e}");
}
