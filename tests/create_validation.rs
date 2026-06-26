//! CREATE TABLE validations that SQLite enforces at prepare time: duplicate
//! column names, more than one PRIMARY KEY, a PRIMARY KEY/UNIQUE list naming a
//! missing column, and AUTOINCREMENT only on an INTEGER PRIMARY KEY. Each is
//! also checked against the `sqlite3` CLI's accept/reject decision.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_ok(ddl: &str) -> bool {
    Connection::open_memory().unwrap().execute(ddl).is_ok()
}

fn sqlite_ok(ddl: &str) -> bool {
    Command::new("sqlite3")
        .arg(":memory:")
        .arg(ddl)
        .output()
        .unwrap()
        .status
        .success()
}

fn agree(ddl: &str) {
    let g = graphite_ok(ddl);
    if sqlite3_available() {
        assert_eq!(g, sqlite_ok(ddl), "graphite/sqlite disagree on: {ddl}");
    }
    g.then_some(()); // silence unused in no-sqlite runs
}

#[test]
fn rejected_definitions() {
    assert!(!graphite_ok("CREATE TABLE t(a, a)"));
    assert!(!graphite_ok("CREATE TABLE t(a PRIMARY KEY, b PRIMARY KEY)"));
    assert!(!graphite_ok("CREATE TABLE t(a, b, PRIMARY KEY(a, z))"));
    assert!(!graphite_ok("CREATE TABLE t(a, UNIQUE(zzz))"));
    assert!(!graphite_ok(
        "CREATE TABLE t(a TEXT PRIMARY KEY AUTOINCREMENT)"
    ));
    assert!(!graphite_ok("CREATE TABLE t(a PRIMARY KEY AUTOINCREMENT)"));
    assert!(!graphite_ok(
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT) WITHOUT ROWID"
    ));
    assert!(!graphite_ok(
        "CREATE TABLE t(x INT GENERATED ALWAYS AS (1) VIRTUAL)"
    ));
}

#[test]
fn aggregate_in_generated_or_check_is_rejected() {
    // SQLite rejects an aggregate function in a CHECK or generated-column
    // expression at CREATE ("misuse of aggregate function NAME()"). `min`/`max`
    // are aggregates only at arity one — the two-arg forms are scalar and fine.
    for ddl in [
        // rejected — aggregate in a generated column
        "CREATE TABLE t(a, b AS (sum(a)))",
        "CREATE TABLE t(a, b AS (count(*)))",
        "CREATE TABLE t(a, b AS (max(a) + 1))",
        "CREATE TABLE t(a, b AS (group_concat(a)))",
        // rejected — aggregate in a CHECK (column- and table-level)
        "CREATE TABLE t(a CHECK(sum(a) > 0))",
        "CREATE TABLE t(a, b, CHECK(min(a) > 0))",
        // accepted — scalar functions, incl. the two-arg min/max forms
        "CREATE TABLE t(a, b AS (abs(a)))",
        "CREATE TABLE t(a, b AS (max(a, 1)))",
        "CREATE TABLE t(a, b AS (min(a, 2)))",
        "CREATE TABLE t(a, b AS (length(a)))",
        "CREATE TABLE t(a CHECK(abs(a) > 0))",
    ] {
        agree(ddl);
    }
    // The single-aggregate message is byte-exact (case preserved as written).
    let err = Connection::open_memory()
        .unwrap()
        .execute("CREATE TABLE t(a, b AS (SUM(a)))")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("misuse of aggregate function SUM()"),
        "unexpected message: {err}"
    );
}

#[test]
fn multiple_primary_key_message_is_byte_exact() {
    // SQLite quotes the table name in this prepare-time error:
    // `table "T" has more than one primary key`. graphite dropped the quotes.
    // Covers both the column-level and table-level PRIMARY KEY forms.
    for ddl in [
        "CREATE TABLE t(a PRIMARY KEY, b PRIMARY KEY)",
        "CREATE TABLE t(a, b, PRIMARY KEY(a), PRIMARY KEY(b))",
    ] {
        let err = Connection::open_memory()
            .unwrap()
            .execute(ddl)
            .unwrap_err()
            .to_string();
        let body = err.trim_start_matches("error: ");
        assert_eq!(body, "table \"t\" has more than one primary key");
    }
}

#[test]
fn foreign_key_local_columns_must_exist() {
    // A table-level FOREIGN KEY's *local* columns must each be a declared column;
    // SQLite rejects an unknown one at CREATE ("unknown column … in foreign key
    // definition"). The referenced parent table/columns are resolved lazily, so a
    // missing parent is NOT a CREATE-time error. A generated column counts as a
    // valid local column; `rowid` does not.
    for ddl in [
        // rejected — unknown local column
        "CREATE TABLE t(a, b, FOREIGN KEY(c) REFERENCES x)",
        "CREATE TABLE t(a, b, FOREIGN KEY(c) REFERENCES x(y))",
        "CREATE TABLE t(a, b, FOREIGN KEY(a, c) REFERENCES x(p, q))",
        "CREATE TABLE t(a, b, FOREIGN KEY(rowid) REFERENCES x)",
        // accepted
        "CREATE TABLE t(a, b, FOREIGN KEY(a) REFERENCES x)",
        "CREATE TABLE t(a, b, FOREIGN KEY(a, b) REFERENCES x(p, q))",
        "CREATE TABLE t(a, b AS (a + 1), FOREIGN KEY(b) REFERENCES x)",
        "CREATE TABLE t(a, FOREIGN KEY(a) REFERENCES nonexist(z))",
        // column-level REFERENCES is about the defining column — always fine
        "CREATE TABLE t(a, b REFERENCES x)",
    ] {
        agree(ddl);
    }
}

#[test]
fn accepted_definitions() {
    assert!(graphite_ok("CREATE TABLE t(a, b)"));
    assert!(graphite_ok("CREATE TABLE t(a PRIMARY KEY, b)"));
    assert!(graphite_ok("CREATE TABLE t(a, b, PRIMARY KEY(a, b))"));
    assert!(graphite_ok(
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)"
    ));
    assert!(graphite_ok("CREATE TABLE t(a, b AS (a + 1))"));
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    for ddl in [
        "CREATE TABLE t(a, a)",
        "CREATE TABLE t(a PRIMARY KEY, b PRIMARY KEY)",
        "CREATE TABLE t(a, b, PRIMARY KEY(a, z))",
        "CREATE TABLE t(a TEXT PRIMARY KEY AUTOINCREMENT)",
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)",
        "CREATE TABLE t(x INT GENERATED ALWAYS AS (1) VIRTUAL)",
        "CREATE TABLE t(a, b, PRIMARY KEY(a, b))",
        // A CHECK / generated-column expression referencing an unknown column is
        // rejected at CREATE; a forward reference to a later column is fine, and a
        // CHECK (but not a generated column) may reference the rowid.
        "CREATE TABLE t(a, b AS (a + x))",
        "CREATE TABLE t(a, CHECK(a + x > 0))",
        "CREATE TABLE t(a, b AS (a + rowid))",
        "CREATE TABLE t(a, CHECK(rowid > 0))",
        "CREATE TABLE t(a, b AS (c + 1), c)",
        "CREATE TABLE t(a, b, CHECK(a < b))",
    ] {
        agree(ddl);
    }
}
