//! Differential sweep of DML / conflict-resolution / UPSERT / RETURNING /
//! triggers (incl. `RAISE`) / ALTER, verified against the `sqlite3` 3.50.4 CLI.
//!
//! These error / side-effect behaviors are exactly what the result-only
//! differential corpus misses, so the assertions probe behavior (does the
//! statement succeed or fail? what rows survive?) rather than SELECT output.
//! sqlite's error *message* wording differs from graphite's, so the helpers
//! compare success-vs-failure and surviving rows, never message text.

#![cfg(feature = "std")]

use graphitesql::exec::eval::Params;
use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

/// Run a `;`-separated script in the `sqlite3` CLI against a fresh in-memory db,
/// using a *file* db so we can inspect post-error state (the CLI bails out of a
/// multi-statement batch on the first error). Returns the final SELECT's text
/// (one row per line, `|`-joined) and whether every statement succeeded.
fn sqlite_run(setup: &[&str], probe: &str, readback: &str) -> (String, bool) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "graphite-dts-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let run = |sql: &str| -> (String, bool) {
        let out = Command::new("sqlite3")
            .arg(path.to_str().unwrap())
            .arg(sql)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            out.status.success(),
        )
    };
    for s in setup {
        run(s);
    }
    let (_, probe_ok) = run(probe);
    let (text, _) = run(readback);
    let _ = std::fs::remove_file(&path);
    (text, probe_ok)
}

/// graphite equivalent: returns (readback text, probe-succeeded).
fn graphite_run(setup: &[&str], probe: &str, readback: &str) -> (String, bool) {
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        c.execute(s).unwrap();
    }
    let probe_ok = c.execute(probe).is_ok();
    let text = c
        .query(readback)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n");
    (text, probe_ok)
}

/// Assert graphite and sqlite agree on both success-vs-failure of `probe` and
/// the rows left behind (read by `readback`).
fn agree(setup: &[&str], probe: &str, readback: &str) {
    let g = graphite_run(setup, probe, readback);
    if !sqlite3_available() {
        return;
    }
    let s = sqlite_run(setup, probe, readback);
    assert_eq!(
        g.1, s.1,
        "success mismatch on {probe:?}: graphite_ok={}, sqlite_ok={}",
        g.1, s.1
    );
    assert_eq!(g.0, s.0, "row mismatch on {probe:?}");
}

// ---------------------------------------------------------------------------
// INSERT conflict resolution.
// ---------------------------------------------------------------------------

#[test]
fn insert_or_ignore_skips_unique_notnull_check() {
    // OR IGNORE skips UNIQUE, NOT NULL and CHECK violations alike (the whole
    // statement still succeeds, the bad row is simply dropped).
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE)",
            "INSERT INTO t VALUES(1,'a')",
        ],
        "INSERT OR IGNORE INTO t VALUES(2,'a')",
        "SELECT count(*) FROM t",
    );
    agree(
        &["CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT NOT NULL)"],
        "INSERT OR IGNORE INTO t VALUES(1,NULL)",
        "SELECT count(*) FROM t",
    );
    agree(
        &["CREATE TABLE t(id INTEGER PRIMARY KEY, k INT CHECK(k>0))"],
        "INSERT OR IGNORE INTO t VALUES(1,-5)",
        "SELECT count(*) FROM t",
    );
}

#[test]
fn insert_conflict_policies_atomicity() {
    // A multi-row insert where the middle row conflicts: OR ABORT (and a plain
    // INSERT) roll the whole statement back, keeping only the pre-existing row;
    // OR FAIL keeps the rows inserted before the failure.
    let setup: &[&str] = &[
        "CREATE TABLE t(id INTEGER PRIMARY KEY)",
        "INSERT INTO t VALUES(2)",
    ];
    agree(
        setup,
        "INSERT OR ABORT INTO t VALUES(1),(2),(3)",
        "SELECT id FROM t ORDER BY id",
    );
    agree(
        setup,
        "INSERT INTO t VALUES(1),(2),(3)",
        "SELECT id FROM t ORDER BY id",
    );
    agree(
        setup,
        "INSERT OR FAIL INTO t VALUES(1),(2),(3)",
        "SELECT id FROM t ORDER BY id",
    );
    agree(
        setup,
        "INSERT OR IGNORE INTO t VALUES(1),(2),(3)",
        "SELECT id FROM t ORDER BY id",
    );
}

#[test]
fn insert_or_replace_and_bare_replace() {
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE)",
            "INSERT INTO t VALUES(1,'a')",
        ],
        "INSERT OR REPLACE INTO t VALUES(2,'a')",
        "SELECT id,k FROM t ORDER BY id",
    );
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE)",
            "INSERT INTO t VALUES(1,'a')",
        ],
        "REPLACE INTO t VALUES(2,'a')",
        "SELECT id,k FROM t ORDER BY id",
    );
}

#[test]
fn insert_default_values() {
    agree(
        &["CREATE TABLE t(id INTEGER PRIMARY KEY, a INT DEFAULT 7, b TEXT DEFAULT 'x')"],
        "INSERT INTO t DEFAULT VALUES",
        "SELECT id,a,b FROM t",
    );
}

// ---------------------------------------------------------------------------
// UPSERT.
// ---------------------------------------------------------------------------

#[test]
fn upsert_do_nothing_and_do_update() {
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, n INT DEFAULT 0)",
            "INSERT INTO t VALUES(1,'a',1)",
        ],
        "INSERT INTO t VALUES(2,'a',9) ON CONFLICT(k) DO NOTHING",
        "SELECT id,k,n FROM t ORDER BY id",
    );
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, n INT DEFAULT 0)",
            "INSERT INTO t VALUES(1,'a',1)",
        ],
        "INSERT INTO t VALUES(2,'a',9) ON CONFLICT(k) DO UPDATE SET n=n+excluded.n",
        "SELECT id,k,n FROM t ORDER BY id",
    );
}

#[test]
fn upsert_wrong_target_is_a_hard_error() {
    // The clause targets `j`, but the row conflicts on `k`: SQLite raises the
    // UNIQUE error (the upsert does *not* absorb a conflict on a different index).
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, j TEXT UNIQUE)",
            "INSERT INTO t VALUES(1,'a','x')",
        ],
        "INSERT INTO t VALUES(2,'a','y') ON CONFLICT(j) DO NOTHING",
        "SELECT id,k,j FROM t ORDER BY id",
    );
}

#[test]
fn upsert_multiple_on_conflict_clauses() {
    // Chained clauses with distinct targets: the conflict on `k` selects the
    // first clause (DO UPDATE SET n=99).
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, j TEXT UNIQUE, n INT)",
            "INSERT INTO t VALUES(1,'a','x',1)",
        ],
        "INSERT INTO t VALUES(2,'a','y',9) ON CONFLICT(k) DO UPDATE SET n=99 ON CONFLICT(j) DO NOTHING",
        "SELECT id,n FROM t ORDER BY id",
    );
}

#[test]
fn upsert_do_update_where_and_violation() {
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, n INT)",
            "INSERT INTO t VALUES(1,'a',1)",
        ],
        "INSERT INTO t VALUES(2,'a',9) ON CONFLICT(k) DO UPDATE SET n=excluded.n WHERE n<0",
        "SELECT id,k,n FROM t ORDER BY id",
    );
    // DO UPDATE that itself violates a UNIQUE constraint is an error.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE)",
            "INSERT INTO t VALUES(1,'a')",
            "INSERT INTO t VALUES(2,'b')",
        ],
        "INSERT INTO t VALUES(3,'b') ON CONFLICT(k) DO UPDATE SET k='a'",
        "SELECT id,k FROM t ORDER BY id",
    );
}

// ---------------------------------------------------------------------------
// RETURNING.
// ---------------------------------------------------------------------------

#[test]
fn returning_projects_changed_rows() {
    let mut c = Connection::open_memory().unwrap();
    let p = Params::default();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT)")
        .unwrap();
    let r = c
        .execute_returning("INSERT INTO t VALUES(1,'a'),(2,'b') RETURNING *", &p)
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0], vec![Value::Integer(1), Value::Text("a".into())]);
    assert_eq!(r.rows[1], vec![Value::Integer(2), Value::Text("b".into())]);

    let r = c
        .execute_returning("INSERT INTO t(k) VALUES('c') RETURNING rowid", &p)
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(3));

    let r = c
        .execute_returning("UPDATE t SET k='Z' WHERE id=1 RETURNING id, k", &p)
        .unwrap();
    assert_eq!(r.rows[0], vec![Value::Integer(1), Value::Text("Z".into())]);

    let r = c
        .execute_returning("DELETE FROM t WHERE id=2 RETURNING id", &p)
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(2));
}

#[test]
fn returning_with_or_replace() {
    let mut c = Connection::open_memory().unwrap();
    let p = Params::default();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,'a')").unwrap();
    let r = c
        .execute_returning("INSERT OR REPLACE INTO t VALUES(2,'a') RETURNING id,k", &p)
        .unwrap();
    assert_eq!(r.rows[0], vec![Value::Integer(2), Value::Text("a".into())]);
}

// ---------------------------------------------------------------------------
// Triggers — including RAISE.
// ---------------------------------------------------------------------------

#[test]
fn trigger_raise_abort_rolls_back_statement() {
    // A BEFORE trigger RAISE(ABORT) on the middle row fails the statement and
    // rolls back the rows already inserted in it.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "CREATE TRIGGER trg BEFORE INSERT ON t WHEN NEW.n<0 BEGIN SELECT RAISE(ABORT,'neg'); END",
        ],
        "INSERT INTO t VALUES(1,5),(2,-5),(3,7)",
        "SELECT n FROM t ORDER BY id",
    );
}

#[test]
fn trigger_raise_fail_keeps_partial() {
    // RAISE(FAIL) fails the statement but keeps the rows inserted before it.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "CREATE TRIGGER trg BEFORE INSERT ON t WHEN NEW.n<0 BEGIN SELECT RAISE(FAIL,'neg'); END",
        ],
        "INSERT INTO t VALUES(1,5),(2,-5),(3,7)",
        "SELECT n FROM t ORDER BY id",
    );
}

#[test]
fn trigger_raise_ignore_skips_row() {
    // RAISE(IGNORE) abandons just the offending row; the statement carries on.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "CREATE TRIGGER trg BEFORE INSERT ON t WHEN NEW.n<0 BEGIN SELECT RAISE(IGNORE); END",
        ],
        "INSERT INTO t VALUES(1,-5),(2,7),(3,-9),(4,8)",
        "SELECT n FROM t ORDER BY id",
    );
    // RAISE(IGNORE) inside a CASE in a BEFORE UPDATE trigger spares matching rows.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "INSERT INTO t VALUES(1,1),(2,2)",
            "CREATE TRIGGER trg BEFORE UPDATE ON t BEGIN \
                SELECT CASE WHEN NEW.n>10 THEN RAISE(IGNORE) END; END",
        ],
        "UPDATE t SET n=n+100",
        "SELECT n FROM t ORDER BY id",
    );
}

#[test]
fn trigger_when_old_new_update_of_instead_of() {
    // WHEN clause gates firing.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "CREATE TABLE log(n INT)",
            "CREATE TRIGGER trg AFTER INSERT ON t WHEN NEW.n>10 BEGIN INSERT INTO log VALUES(NEW.n); END",
            "INSERT INTO t VALUES(1,5),(2,20)",
        ],
        "INSERT INTO t VALUES(3,30)",
        "SELECT n FROM log ORDER BY n",
    );
    // UPDATE OF <col> fires only for that column.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT)",
            "CREATE TABLE log(m TEXT)",
            "CREATE TRIGGER trg AFTER UPDATE OF a ON t BEGIN INSERT INTO log VALUES('a'); END",
            "INSERT INTO t VALUES(1,1,1)",
            "UPDATE t SET b=9",
        ],
        "UPDATE t SET a=9",
        "SELECT count(*) FROM log",
    );
    // INSTEAD OF INSERT on a view.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "CREATE VIEW v AS SELECT id,n FROM t",
            "CREATE TRIGGER trg INSTEAD OF INSERT ON v BEGIN INSERT INTO t VALUES(NEW.id,NEW.n*2); END",
        ],
        "INSERT INTO v VALUES(1,5)",
        "SELECT id,n FROM t",
    );
}

#[test]
fn raise_outside_trigger_rejected() {
    // RAISE() is only valid in a trigger program; both engines reject it.
    let c = Connection::open_memory().unwrap();
    assert!(c.query("SELECT RAISE(IGNORE)").is_err());
}

// ---------------------------------------------------------------------------
// ALTER TABLE.
// ---------------------------------------------------------------------------

#[test]
fn alter_add_column_constraints() {
    // ADD COLUMN with a DEFAULT backfills; UNIQUE / PRIMARY KEY / bare NOT NULL
    // are rejected; NOT NULL with a DEFAULT is accepted.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "INSERT INTO t VALUES(1,5)",
        ],
        "ALTER TABLE t ADD COLUMN extra TEXT DEFAULT 'd'",
        "SELECT id,n,extra FROM t",
    );
    agree(
        &["CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)"],
        "ALTER TABLE t ADD COLUMN u TEXT UNIQUE",
        "SELECT count(*) FROM t",
    );
    agree(
        &["CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)"],
        "ALTER TABLE t ADD COLUMN p INT PRIMARY KEY",
        "SELECT count(*) FROM t",
    );
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "INSERT INTO t VALUES(1,5)",
        ],
        "ALTER TABLE t ADD COLUMN q TEXT NOT NULL",
        "SELECT count(*) FROM t",
    );
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "INSERT INTO t VALUES(1,5)",
        ],
        "ALTER TABLE t ADD COLUMN q TEXT NOT NULL DEFAULT 'z'",
        "SELECT q FROM t",
    );
}

#[test]
fn alter_rename_and_drop_column() {
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "INSERT INTO t VALUES(1,5)",
        ],
        "ALTER TABLE t RENAME COLUMN n TO m",
        "SELECT m FROM t",
    );
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT)",
            "INSERT INTO t VALUES(1,2,3)",
        ],
        "ALTER TABLE t DROP COLUMN b",
        "SELECT * FROM t",
    );
    // Dropping a PK / indexed column is rejected.
    agree(
        &["CREATE TABLE t(id INTEGER PRIMARY KEY, a INT)"],
        "ALTER TABLE t DROP COLUMN id",
        "SELECT count(*) FROM t",
    );
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT)",
            "CREATE INDEX i ON t(b)",
        ],
        "ALTER TABLE t DROP COLUMN b",
        "SELECT count(*) FROM t",
    );
}

#[test]
fn alter_rename_table_keeps_trigger_firing() {
    // After renaming the table, its trigger still fires on the new name.
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "CREATE TABLE log(m INT)",
            "CREATE TRIGGER trg AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.n); END",
            "ALTER TABLE t RENAME TO t2",
        ],
        "INSERT INTO t2 VALUES(1,99)",
        "SELECT m FROM log",
    );
}

// ---------------------------------------------------------------------------
// DELETE / UPDATE edge cases.
// ---------------------------------------------------------------------------

#[test]
fn delete_update_limit_order_from_subquery() {
    // `DELETE`/`UPDATE ... ORDER BY ... LIMIT` cannot be compared differentially:
    // the official sqlite3 build used by CI is compiled WITHOUT
    // SQLITE_ENABLE_UPDATE_DELETE_LIMIT, so it rejects this grammar (some distro
    // builds enable it). graphite supports it, so assert its behavior directly.
    assert_eq!(
        graphite_run(
            &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY)",
                "INSERT INTO t VALUES(1),(2),(3),(4)",
            ],
            "DELETE FROM t ORDER BY id DESC LIMIT 2",
            "SELECT id FROM t ORDER BY id",
        ),
        ("1\n2".to_string(), true),
    );
    assert_eq!(
        graphite_run(
            &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
                "INSERT INTO t VALUES(1,0),(2,0),(3,0)",
            ],
            "UPDATE t SET n=1 ORDER BY id LIMIT 2",
            "SELECT id,n FROM t ORDER BY id",
        ),
        ("1|1\n2|1\n3|0".to_string(), true),
    );
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n INT)",
            "CREATE TABLE s(id INT, n INT)",
            "INSERT INTO t VALUES(1,0),(2,0)",
            "INSERT INTO s VALUES(1,100),(2,200)",
        ],
        "UPDATE t SET n=s.n FROM s WHERE s.id=t.id",
        "SELECT id,n FROM t ORDER BY id",
    );
    agree(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY)",
            "CREATE TABLE bad(id INT)",
            "INSERT INTO t VALUES(1),(2),(3)",
            "INSERT INTO bad VALUES(2)",
        ],
        "DELETE FROM t WHERE id IN (SELECT id FROM bad)",
        "SELECT id FROM t ORDER BY id",
    );
}

#[test]
fn or_rollback_unwinds_transaction() {
    // INSERT OR ROLLBACK inside an explicit transaction discards everything
    // staged in that transaction, not just the failing statement.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO t VALUES(2)").unwrap();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO t VALUES(5)").unwrap();
    let err = c.execute("INSERT OR ROLLBACK INTO t VALUES(2)");
    assert!(err.is_err());
    // The transaction was rolled back: row 5 is gone, only the original row 2
    // (committed before BEGIN) remains, and no transaction is active.
    let ids: Vec<i64> = c
        .query("SELECT id FROM t ORDER BY id")
        .unwrap()
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![2]);
    // A fresh COMMIT now errors because the transaction is already closed.
    assert!(c.execute("COMMIT").is_err());
}

#[test]
fn multiple_triggers_fire_in_reverse_creation_order() {
    // SQLite keeps a per-table trigger list that prepends on creation, so several
    // triggers for the same event/timing fire most-recently-created first. The
    // firing order is captured by the order rows land in `log` (no ORDER BY, so
    // rowid = insertion = firing order). Checked against sqlite for INSERT (AFTER
    // and BEFORE), UPDATE, and DELETE; the name vs creation-order cases
    // disambiguate (newest-first, not alphabetical).
    agree(
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(s)",
            "CREATE TRIGGER aaa AFTER INSERT ON t BEGIN INSERT INTO log VALUES('A'); END",
            "CREATE TRIGGER bbb AFTER INSERT ON t BEGIN INSERT INTO log VALUES('B'); END",
        ],
        "INSERT INTO t VALUES(1)",
        "SELECT s FROM log",
    );
    agree(
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(s)",
            "CREATE TRIGGER zzz BEFORE INSERT ON t BEGIN INSERT INTO log VALUES('Z'); END",
            "CREATE TRIGGER aaa BEFORE INSERT ON t BEGIN INSERT INTO log VALUES('A'); END",
        ],
        "INSERT INTO t VALUES(1)",
        "SELECT s FROM log",
    );
    agree(
        &[
            "CREATE TABLE t(a)",
            "INSERT INTO t VALUES(1)",
            "CREATE TABLE log(s)",
            "CREATE TRIGGER u1 AFTER UPDATE ON t BEGIN INSERT INTO log VALUES('u1'); END",
            "CREATE TRIGGER u2 AFTER UPDATE ON t BEGIN INSERT INTO log VALUES('u2'); END",
        ],
        "UPDATE t SET a=2",
        "SELECT s FROM log",
    );
    agree(
        &[
            "CREATE TABLE t(a)",
            "INSERT INTO t VALUES(1)",
            "CREATE TABLE log(s)",
            "CREATE TRIGGER d1 AFTER DELETE ON t BEGIN INSERT INTO log VALUES('d1'); END",
            "CREATE TRIGGER d2 AFTER DELETE ON t BEGIN INSERT INTO log VALUES('d2'); END",
        ],
        "DELETE FROM t",
        "SELECT s FROM log",
    );
}
