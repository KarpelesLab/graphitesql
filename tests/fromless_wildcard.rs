//! A wildcard result column needs a `FROM` clause. SQLite rejects a FROM-less
//! wildcard at *prepare* time — a bare `*` is `no tables specified`, a qualified
//! `X.*` is `no such table: X` — and gives it the highest resolution precedence:
//! it wins over a missing `LIMIT` column, a wrong-arity aggregate, and a compound
//! column-count mismatch. graphite used to expand the wildcard to nothing and
//! return a row; it now rejects eagerly, matching SQLite over the whole query
//! tree (compound arms, derived-table and expression-position subqueries), while
//! leaving an unreferenced CTE body — which SQLite analyzes lazily — accepted.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

/// FROM-less wildcards that must be rejected, paired with the exact message tail
/// SQLite reports.
const REJECTED: &[(&str, &str)] = &[
    ("SELECT *", "no tables specified"),
    ("SELECT *, 1", "no tables specified"),
    ("SELECT 1, *", "no tables specified"),
    ("SELECT t.*", "no such table: t"),
    ("SELECT * WHERE 1", "no tables specified"),
    ("SELECT * LIMIT 0", "no tables specified"),
    // Precedence: the wildcard wins over every other resolution error.
    ("SELECT *, nope", "no tables specified"),
    ("SELECT abs(1,2), *", "no tables specified"),
    ("SELECT * LIMIT nope", "no tables specified"),
    ("SELECT * ORDER BY nope", "no tables specified"),
    ("SELECT * UNION SELECT 1", "no tables specified"),
    // Recursion into subqueries / derived tables.
    ("SELECT (SELECT *)", "no tables specified"),
    ("SELECT EXISTS(SELECT *)", "no tables specified"),
    ("SELECT 1 FROM (SELECT *)", "no tables specified"),
    ("SELECT 1 FROM (SELECT t.*)", "no such table: t"),
    ("SELECT x.*", "no such table: x"),
];

/// FROM-less queries that are still valid — no wildcard, or a lazily-analyzed
/// (unreferenced) CTE body.
const ACCEPTED: &[&str] = &[
    "SELECT 1",
    "SELECT count(*)", // the `*` is a function argument, not a projection
    "VALUES (1)",      //
    "WITH c AS (SELECT *) SELECT 1", // unreferenced CTE: analyzed lazily
    "SELECT * FROM (SELECT 1)", // the wildcard has a FROM (the derived table)
];

#[test]
fn fromless_wildcard_is_rejected() {
    let c = Connection::open_memory().unwrap();
    for (q, tail) in REJECTED {
        let err = c.query(q).expect_err(&format!("expected rejection of {q}"));
        let msg = format!("{err}");
        assert!(
            msg.contains(tail),
            "on {q}: expected message ending in {tail:?}, got {msg:?}",
        );
    }
}

#[test]
fn valid_fromless_queries_are_accepted() {
    let c = Connection::open_memory().unwrap();
    for q in ACCEPTED {
        assert!(c.query(q).is_ok(), "expected {q} to be accepted");
    }
}

#[test]
fn fromless_wildcard_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Every rejected query must fail in sqlite too, with the same message tail.
    for (q, tail) in REJECTED {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!("{q};"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !out.status.success() || stderr.contains(tail),
            "sqlite3 unexpectedly accepted {q} (out={stdout:?} err={stderr:?})",
        );
        assert!(
            stderr.contains(tail),
            "sqlite3 message for {q} lacks {tail:?}: {stderr:?}",
        );
    }
    // Every accepted query must succeed in sqlite too.
    for q in ACCEPTED {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!("{q};"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "sqlite3 rejected {q}: {:?}",
            String::from_utf8_lossy(&out.stderr),
        );
    }
}
