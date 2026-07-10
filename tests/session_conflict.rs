//! Differential + semantic tests for custom conflict handlers on changeset
//! apply — [`graphitesql::Connection::changeset_apply_with`] (roadmap D5 tail).
//!
//! The default [`changeset_apply`](graphitesql::Connection::changeset_apply)
//! disposition (omit DATA/NOTFOUND, abort CONFLICT/CONSTRAINT) is already
//! covered by `session_changeset.rs`. Here we drive the *handler*: OMIT
//! everything, REPLACE a conflict, and ABORT a chosen conflict type — checked
//! both as pure semantic assertions and, when `GRAPHITE_SESAPPLY_POLICY` points
//! at the policy-parameterised `sesapply_policy` oracle, byte-for-byte against
//! sqlite's `sqlite3changeset_apply` with a matching `xConflict`.

#![cfg(feature = "std")]

use graphitesql::{ConflictAction as A, ConflictType as T, Connection};
use std::process::Command;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Build a changeset for `dml` over `setup` via a graphite session.
fn make_changeset(setup: &str, dml: &str) -> Vec<u8> {
    let mut a = Connection::open_memory().unwrap();
    a.execute_batch(setup).unwrap();
    let session = a.create_session();
    session.attach();
    a.execute_batch(dml).unwrap();
    a.session_changeset(&session).unwrap()
}

/// Dump `SELECT * FROM t ORDER BY a` deterministically.
fn dump_t(conn: &Connection) -> String {
    use graphitesql::Value;
    let r = conn.query("SELECT * FROM t ORDER BY a").unwrap();
    let mut out = String::new();
    for row in &r.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                Value::Null => "NULL".to_string(),
                Value::Integer(i) => format!("{i}"),
                Value::Real(f) => format!("R{f}"),
                Value::Text(t) => format!("'{t}'"),
                Value::Blob(b) => format!(
                    "x'{}'",
                    b.iter().map(|x| format!("{x:02x}")).collect::<String>()
                ),
            })
            .collect();
        out.push_str(&cells.join("|"));
        out.push('\n');
    }
    out.trim().to_string()
}

/// Apply `cs` to `conn` with the graphite handler for `policy` (mirroring the
/// oracle's `xConflict` for the same policy name).
fn apply_with_policy(conn: &mut Connection, cs: &[u8], policy: &str) -> graphitesql::Result<()> {
    match policy {
        "omit" => conn.changeset_apply_with(cs, |_| A::Omit),
        "replace" => conn.changeset_apply_with(cs, |k| match k {
            T::Data | T::Conflict => A::Replace,
            _ => A::Omit,
        }),
        "abort_data" => {
            conn.changeset_apply_with(cs, |k| if k == T::Data { A::Abort } else { A::Omit })
        }
        "abort_conflict" => {
            conn.changeset_apply_with(cs, |k| if k == T::Conflict { A::Abort } else { A::Omit })
        }
        // "default"
        _ => conn.changeset_apply_with(cs, |k| match k {
            T::Data | T::NotFound => A::Omit,
            _ => A::Abort,
        }),
    }
}

/// Ask the policy oracle to apply `cs_hex` onto `baseline` under `policy`, or
/// `None` if `GRAPHITE_SESAPPLY_POLICY` is unset. Returns `(rows, aborted)`.
fn oracle_policy(baseline: &str, cs_hex: &str, policy: &str) -> Option<(String, bool)> {
    let bin = std::env::var("GRAPHITE_SESAPPLY_POLICY").ok()?;
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(baseline)
        .arg(cs_hex)
        .arg("SELECT * FROM t ORDER BY a;")
        .arg(policy)
        .output()
        .expect("run sesapply_policy");
    assert!(
        out.status.success(),
        "sesapply_policy failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    let aborted = s
        .lines()
        .next()
        .map(|l| l.starts_with("APPLY_ERR"))
        .unwrap_or(false);
    let rows = s
        .lines()
        .filter(|l| !l.starts_with("APPLY_ERR"))
        .map(|l| {
            l.split('|')
                .map(|f| {
                    if !f.starts_with('\'')
                        && !f.starts_with("x'")
                        && f.contains('.')
                        && let Ok(v) = f.parse::<f64>()
                    {
                        return format!("R{v}");
                    }
                    f.to_string()
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some((rows.trim().to_string(), aborted))
}

/// Full differential check: graphite's `changeset_apply_with(policy)` must
/// match the oracle's rows and abort status. `expect_rows`/`expect_abort` are
/// the semantic assertion that always runs (oracle-independent).
fn check(
    gen_setup: &str,
    dml: &str,
    baseline: &str,
    policy: &str,
    expect_rows: &str,
    expect_abort: bool,
) {
    let cs = make_changeset(gen_setup, dml);

    let mut g = Connection::open_memory().unwrap();
    g.execute_batch(baseline).unwrap();
    let g_res = apply_with_policy(&mut g, &cs, policy);
    let g_rows = dump_t(&g);

    assert_eq!(
        g_rows, expect_rows,
        "semantic rows\n policy={policy}\n baseline={baseline}\n dml={dml}"
    );
    assert_eq!(
        g_res.is_err(),
        expect_abort,
        "semantic abort\n policy={policy}\n g_res={g_res:?}"
    );

    if let Some((s_rows, s_aborted)) = oracle_policy(baseline, &hex(&cs), policy) {
        assert_eq!(
            g_rows, s_rows,
            "rows vs oracle\n policy={policy}\n baseline={baseline}\n dml={dml}"
        );
        assert_eq!(
            g_res.is_err(),
            s_aborted,
            "abort vs oracle\n policy={policy}\n dml={dml}"
        );
    }
}

const S: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);";

// --- OMIT: turn the default aborts into skips --------------------------------

#[test]
fn omit_insert_pk_collision() {
    // Default would ABORT; OMIT skips the colliding INSERT, keeps the old row.
    // The changeset is a *clean* INSERT(1,…) (generated over an empty table);
    // the CONFLICT arises only against the baseline's existing row 1.
    check(
        S,
        "INSERT INTO t VALUES(1,'new',2);",
        &format!("{S} INSERT INTO t VALUES(1,'old',9);"),
        "omit",
        "1|'old'|9",
        false,
    );
}

#[test]
fn omit_notfound_and_data_still_skipped() {
    // OMIT matches the default for DATA/NOTFOUND (already-skipped) conflicts.
    check(
        &format!("{S} INSERT INTO t VALUES(1,'x',1);"),
        "UPDATE t SET b='X' WHERE a=1;",
        &format!("{S} INSERT INTO t VALUES(2,'y',2);"),
        "omit",
        "2|'y'|2",
        false,
    );
}

// --- REPLACE -----------------------------------------------------------------

#[test]
fn replace_insert_pk_collision() {
    // REPLACE deletes the colliding row and inserts the changeset's row.
    check(
        S,
        "INSERT INTO t VALUES(1,'new',2);",
        &format!("{S} INSERT INTO t VALUES(1,'old',9);"),
        "replace",
        "1|'new'|2",
        false,
    );
}

#[test]
fn replace_data_update_forced() {
    // The target's row differs from the recorded old.* (DATA); REPLACE forces
    // the UPDATE through, matched by primary key alone.
    check(
        &format!("{S} INSERT INTO t VALUES(1,'orig',1);"),
        "UPDATE t SET b='updated' WHERE a=1;",
        &format!("{S} INSERT INTO t VALUES(1,'DIFFERENT',9);"),
        "replace",
        "1|'updated'|9",
        false,
    );
}

#[test]
fn replace_data_delete_forced() {
    // DATA conflict on a DELETE; REPLACE deletes the row by primary key.
    check(
        &format!("{S} INSERT INTO t VALUES(1,'orig',1),(2,'keep',2);"),
        "DELETE FROM t WHERE a=1;",
        &format!("{S} INSERT INTO t VALUES(1,'DIFFERENT',9),(2,'keep',2);"),
        "replace",
        "2|'keep'|2",
        false,
    );
}

#[test]
fn replace_notfound_is_omitted() {
    // REPLACE is not valid for NOTFOUND; the handler returns OMIT there, so the
    // missing-target UPDATE is simply skipped (no abort).
    check(
        &format!("{S} INSERT INTO t VALUES(1,'orig',1);"),
        "UPDATE t SET b='updated' WHERE a=1;",
        &format!("{S} INSERT INTO t VALUES(2,'y',2);"),
        "replace",
        "2|'y'|2",
        false,
    );
}

// --- ABORT on a chosen type --------------------------------------------------

#[test]
fn abort_on_data_rolls_back() {
    // A DATA conflict aborts; the whole apply (incl. a prior clean change) rolls
    // back to the baseline.
    check(
        &format!("{S} INSERT INTO t VALUES(1,'orig',1);"),
        "INSERT INTO t VALUES(3,'z',3); UPDATE t SET b='updated' WHERE a=1;",
        &format!("{S} INSERT INTO t VALUES(1,'DIFFERENT',9);"),
        "abort_data",
        "1|'DIFFERENT'|9",
        true,
    );
}

#[test]
fn abort_on_conflict_rolls_back() {
    check(
        S,
        "INSERT INTO t VALUES(1,'new',2);",
        &format!("{S} INSERT INTO t VALUES(1,'old',9);"),
        "abort_conflict",
        "1|'old'|9",
        true,
    );
}

// --- composite / text primary keys (broader shapes) --------------------------

#[test]
fn replace_composite_pk_conflict() {
    let s = "CREATE TABLE t(a, b, c, PRIMARY KEY(a,b));";
    let cs = make_changeset(s, "INSERT INTO t VALUES(1,'k','new');");
    let baseline = format!("{s} INSERT INTO t VALUES(1,'k','old');");

    let mut g = Connection::open_memory().unwrap();
    g.execute_batch(&baseline).unwrap();
    apply_with_policy(&mut g, &cs, "replace").unwrap();
    let rows = g.query("SELECT c FROM t WHERE a=1 AND b='k'").unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0][0], graphitesql::Value::Text("new".into()));

    if let Some((_s_rows, aborted)) = oracle_policy(&baseline, &hex(&cs), "replace") {
        assert!(!aborted, "composite REPLACE should not abort in the oracle");
    }
}

#[test]
fn replace_text_pk_data_update() {
    let s = "CREATE TABLE t(a TEXT PRIMARY KEY, b);";
    let cs = make_changeset(
        &format!("{s} INSERT INTO t VALUES('k','orig');"),
        "UPDATE t SET b='updated' WHERE a='k';",
    );
    let baseline = format!("{s} INSERT INTO t VALUES('k','DIFFERENT');");

    let mut g = Connection::open_memory().unwrap();
    g.execute_batch(&baseline).unwrap();
    apply_with_policy(&mut g, &cs, "replace").unwrap();
    let rows = g.query("SELECT b FROM t WHERE a='k'").unwrap();
    assert_eq!(rows.rows[0][0], graphitesql::Value::Text("updated".into()));
}

// --- default handler parity: changeset_apply == changeset_apply_with(default)

#[test]
fn default_handler_matches_changeset_apply() {
    let cs = make_changeset(S, "INSERT INTO t VALUES(1,'new',2);");
    // changeset_apply (built-in default) aborts on the PK collision.
    let mut a = Connection::open_memory().unwrap();
    a.execute_batch(&format!("{S} INSERT INTO t VALUES(1,'old',9);"))
        .unwrap();
    let ra = a.changeset_apply(&cs);
    // changeset_apply_with(default closure) must behave identically.
    let mut b = Connection::open_memory().unwrap();
    b.execute_batch(&format!("{S} INSERT INTO t VALUES(1,'old',9);"))
        .unwrap();
    let rb = apply_with_policy(&mut b, &cs, "default");
    assert_eq!(ra.is_err(), rb.is_err());
    assert_eq!(dump_t(&a), dump_t(&b));
    assert_eq!(dump_t(&a), "1|'old'|9");
}
