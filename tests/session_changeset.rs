//! Differential tests for [`graphitesql::Session`] changeset generation.
//!
//! graphite's changeset bytes are compared, byte-for-byte, against the SQLite
//! session extension. SQLite's `sqlite3` CLI does not expose sessions, so the
//! oracle is a tiny C harness (`sesdump`) built against the amalgamation with
//! `SQLITE_ENABLE_SESSION`. Point `GRAPHITE_SESDUMP` at that binary to run the
//! differential half; the byte-literal assertions always run.
//!
//! `sesdump` usage: `sesdump <db> <sql> [setup-sql]` — it creates a session on
//! `main`, attaches all tables, runs `<sql>`, and prints the changeset as hex.
//! `[setup-sql]` (schema + seed rows) runs *before* the session is created, so
//! those rows are not themselves recorded.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

/// Run `sql` (optionally after `setup`) on a fresh in-memory graphite
/// connection with a session attached, and return the changeset as lowercase
/// hex.
fn graphite_changeset(setup: &str, sql: &str) -> String {
    let mut conn = Connection::open_memory().unwrap();
    if !setup.is_empty() {
        conn.execute_batch(setup).unwrap();
    }
    let session = conn.create_session();
    session.attach();
    conn.execute_batch(sql).unwrap();
    let bytes = conn.session_changeset(&session).unwrap();
    hex(&bytes)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Ask the SQLite oracle for the reference changeset hex, or `None` if the
/// oracle binary is not configured.
fn oracle(setup: &str, sql: &str) -> Option<String> {
    let bin = std::env::var("GRAPHITE_SESDUMP").ok()?;
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(sql)
        .arg(setup)
        .output()
        .expect("run sesdump");
    assert!(
        out.status.success(),
        "sesdump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Assert graphite's changeset equals the oracle's (when configured) and, when
/// `expect` is `Some`, equals that byte literal too.
fn check(setup: &str, sql: &str, expect: Option<&str>) {
    let got = graphite_changeset(setup, sql);
    if let Some(reference) = oracle(setup, sql) {
        assert_eq!(got, reference, "vs oracle\n setup={setup}\n sql={sql}");
    }
    if let Some(exp) = expect {
        assert_eq!(got, exp, "vs literal\n setup={setup}\n sql={sql}");
    }
}

const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b);";

#[test]
fn insert_int() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        Some("5402010074001200010000000000000001010000000000000002"),
    );
}

#[test]
fn insert_text() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,'hi');",
        Some("540201007400120001000000000000000103026869"),
    );
}

#[test]
fn insert_real() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,3.5);",
        Some("540201007400120001000000000000000102400c000000000000"),
    );
}

#[test]
fn insert_null() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,NULL);",
        Some("540201007400120001000000000000000105"),
    );
}

#[test]
fn insert_blob() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,x'aabb');",
        Some("54020100740012000100000000000000010402aabb"),
    );
}

#[test]
fn insert_two_rows() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2),(3,4);",
        Some(
            "540201007400120001000000000000000101000000000000000212000100000000\
             00000003010000000000000004",
        ),
    );
}

#[test]
fn delete_row() {
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "DELETE FROM t WHERE a=1;",
        Some("5402010074000900010000000000000001010000000000000002"),
    );
}

#[test]
fn update_int() {
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "UPDATE t SET b=99 WHERE a=1;",
        Some("540201007400170001000000000000000101000000000000000200010000000000000063"),
    );
}

#[test]
fn update_text() {
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "UPDATE t SET b='xyz' WHERE a=1;",
        Some("540201007400170001000000000000000101000000000000000200030378797a"),
    );
}

// The following exercise coalescing and multi-row hash ordering; the exact
// bytes are validated against the oracle (when configured). Literals are
// omitted for the ordering cases — the oracle is authoritative there.

#[test]
fn insert_then_update_coalesces_to_insert() {
    check(
        SCHEMA,
        "INSERT INTO t VALUES(1,2); UPDATE t SET b=99 WHERE a=1;",
        // final INSERT of (1, 99)
        Some("5402010074001200010000000000000001010000000000000063"),
    );
}

#[test]
fn insert_then_delete_coalesces_to_nothing() {
    check(
        SCHEMA,
        "INSERT INTO t VALUES(1,2); DELETE FROM t WHERE a=1;",
        Some(""),
    );
}

#[test]
fn update_then_delete_coalesces_to_delete_of_original() {
    // (1,2) is seeded outside the session; the session then updates and deletes
    // it, which must coalesce to a DELETE carrying the *original* values.
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "UPDATE t SET b=5 WHERE a=1; DELETE FROM t WHERE a=1;",
        Some("5402010074000900010000000000000001010000000000000002"),
    );
}

#[test]
fn multi_row_hash_order() {
    // Ten rows inserted out of order — the changeset lists them in SQLite's
    // hash-bucket order, not insertion or rowid order. Oracle-checked.
    check(
        SCHEMA,
        "INSERT INTO t VALUES(5,0),(1,0),(9,0),(3,0),(7,0),(2,0),(8,0),(4,0),(6,0),(10,0);",
        None,
    );
}

#[test]
fn delete_then_insert_coalesces_to_update() {
    // A row seeded outside the session, then deleted and re-inserted with a new
    // value inside it, coalesces to an UPDATE (old = pre-delete, new = final) —
    // matching SQLite, which decides the emitted op from the live row.
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "DELETE FROM t WHERE a=1; INSERT INTO t VALUES(1,9);",
        Some("540201007400170001000000000000000101000000000000000200010000000000000009"),
    );
}

#[test]
fn insert_or_replace_same_pk_is_update() {
    // `INSERT OR REPLACE` over an existing row (same PK, seeded outside the
    // session) is recorded as an UPDATE, like SQLite.
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "INSERT OR REPLACE INTO t VALUES(1,9);",
        Some("540201007400170001000000000000000101000000000000000200010000000000000009"),
    );
}

#[test]
fn is_empty_when_no_changes() {
    let conn = Connection::open_memory().unwrap();
    let session = conn.create_session();
    session.attach();
    assert!(session.is_empty());
    assert_eq!(conn.session_changeset(&session).unwrap(), Vec::<u8>::new());
}

#[test]
fn no_op_update_produces_nothing() {
    // Updating a column to its current value is a no-op change: SQLite emits
    // nothing for it.
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "UPDATE t SET b=2 WHERE a=1;",
        Some(""),
    );
}

// ---------------------------------------------------------------------------
// `Connection::changeset_apply` — apply the changeset back into a database.
// ---------------------------------------------------------------------------

/// Dump `SELECT * FROM t ORDER BY a` as a deterministic string.
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
                Value::Blob(b) => {
                    format!(
                        "x'{}'",
                        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
                    )
                }
            })
            .collect();
        out.push_str(&cells.join("|"));
        out.push('\n');
    }
    out
}

/// Round-trip: run `dml` on DB_A (recording a session), then apply the
/// resulting changeset to a fresh DB_B holding DB_A's *pre-DML* state. DB_B
/// must end up identical to DB_A.
fn roundtrip(setup: &str, dml: &str) {
    let mut a = Connection::open_memory().unwrap();
    a.execute_batch(setup).unwrap();
    let session = a.create_session();
    session.attach();
    a.execute_batch(dml).unwrap();
    let cs = a.session_changeset(&session).unwrap();
    let post_a = dump_t(&a);

    let mut b = Connection::open_memory().unwrap();
    b.execute_batch(setup).unwrap();
    b.changeset_apply(&cs).unwrap();
    let post_b = dump_t(&b);

    assert_eq!(
        post_a, post_b,
        "round-trip mismatch\n setup={setup}\n dml={dml}"
    );
}

#[test]
fn apply_roundtrip_insert() {
    roundtrip(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(1,'x',1.5);",
        "INSERT INTO t VALUES(3,'z',x'aabb');",
    );
}

#[test]
fn apply_roundtrip_update() {
    roundtrip(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(1,'x',1.5),(2,'y',NULL);",
        "UPDATE t SET b='X2', c=9.5 WHERE a=1;",
    );
}

#[test]
fn apply_roundtrip_delete() {
    roundtrip(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(1,'x',1.5),(2,'y',NULL);",
        "DELETE FROM t WHERE a=2;",
    );
}

#[test]
fn apply_roundtrip_mixed() {
    roundtrip(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(1,'x',1.5),(2,'y',NULL);",
        "INSERT INTO t VALUES(3,'z',7); UPDATE t SET b='X2' WHERE a=1; DELETE FROM t WHERE a=2;",
    );
}

#[test]
fn apply_roundtrip_all_value_types() {
    roundtrip(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b);",
        "INSERT INTO t VALUES(1,NULL),(2,42),(3,-7.25),(4,'text'),(5,x'00ff10');",
    );
}

#[test]
fn apply_empty_changeset_is_noop() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t VALUES(1,2);")
        .unwrap();
    conn.changeset_apply(&[]).unwrap();
    assert_eq!(dump_t(&conn), "1|2\n");
}

/// Build a changeset for `dml` over `setup` (via a graphite session).
fn make_changeset(setup: &str, dml: &str) -> Vec<u8> {
    let mut a = Connection::open_memory().unwrap();
    a.execute_batch(setup).unwrap();
    let session = a.create_session();
    session.attach();
    a.execute_batch(dml).unwrap();
    a.session_changeset(&session).unwrap()
}

#[test]
fn apply_conflict_notfound_delete_is_omitted() {
    // DELETE of a row absent from the target → NOTFOUND → omitted, no error.
    let cs = make_changeset(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t VALUES(2,'y');",
        "DELETE FROM t WHERE a=2;",
    );
    let mut b = Connection::open_memory().unwrap();
    b.execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t VALUES(1,'x');")
        .unwrap();
    b.changeset_apply(&cs).unwrap();
    assert_eq!(dump_t(&b), "1|'x'\n");
}

#[test]
fn apply_conflict_data_mismatch_update_is_omitted() {
    // UPDATE whose recorded old.* no longer matches the live row → DATA →
    // omitted, no error, row left untouched.
    let cs = make_changeset(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t VALUES(1,'orig');",
        "UPDATE t SET b='new' WHERE a=1;",
    );
    let mut b = Connection::open_memory().unwrap();
    b.execute_batch(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t VALUES(1,'DIFFERENT');",
    )
    .unwrap();
    b.changeset_apply(&cs).unwrap();
    assert_eq!(dump_t(&b), "1|'DIFFERENT'\n");
}

#[test]
fn apply_conflict_insert_pk_collision_aborts_and_rolls_back() {
    // INSERT whose PK already exists → CONFLICT → default ABORT: the whole
    // apply rolls back, so an earlier change in the same changeset is undone
    // too.
    let cs = make_changeset(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t VALUES(1,'x'),(2,'y');",
        "DELETE FROM t WHERE a=1; INSERT INTO t VALUES(3,'z');",
    );
    let mut b = Connection::open_memory().unwrap();
    b.execute_batch(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); \
         INSERT INTO t VALUES(1,'x'),(3,'exists');",
    )
    .unwrap();
    let err = b.changeset_apply(&cs);
    assert!(err.is_err(), "expected abort on PK collision");
    // Rolled back: row 1 still present, row 3 unchanged.
    assert_eq!(dump_t(&b), "1|'x'\n3|'exists'\n");
}

#[test]
fn apply_schema_mismatch_table_absent_is_skipped() {
    // A changeset naming a table the target lacks is a schema mismatch: its
    // changes are skipped (no error), like sqlite's log-and-continue.
    let cs = make_changeset(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t VALUES(1,'x');",
        "INSERT INTO t VALUES(2,'y');",
    );
    let mut b = Connection::open_memory().unwrap();
    b.execute_batch("CREATE TABLE other(a INTEGER PRIMARY KEY, b);")
        .unwrap();
    // No table `t` — apply is a no-op and succeeds.
    b.changeset_apply(&cs).unwrap();
    assert_eq!(
        b.query("SELECT count(*) FROM other").unwrap().rows[0][0],
        graphitesql::Value::Integer(0)
    );
}

/// Ask the SQLite apply-oracle (`sesapply`) to apply `cs_hex` onto `baseline`
/// and dump `SELECT * FROM t ORDER BY a`, or `None` if not configured.
fn apply_oracle(baseline: &str, cs_hex: &str) -> Option<String> {
    let bin = std::env::var("GRAPHITE_SESAPPLY").ok()?;
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(baseline)
        .arg(cs_hex)
        .arg("SELECT * FROM t ORDER BY a;")
        .output()
        .expect("run sesapply");
    assert!(
        out.status.success(),
        "sesapply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Differential apply: graphite's `changeset_apply` result must match the
/// SQLite oracle's apply of the same changeset onto the same baseline (both
/// rows and whether the apply aborted). The changeset is generated over
/// `gen_setup` (which seeds the session) and then applied onto `baseline`
/// (the target). Runs the oracle half only when `GRAPHITE_SESAPPLY` is set;
/// otherwise the pure round-trip tests above still cover apply.
fn check_apply(gen_setup: &str, dml: &str, baseline: &str) {
    let cs = make_changeset(gen_setup, dml);

    // graphite apply.
    let mut g = Connection::open_memory().unwrap();
    g.execute_batch(baseline).unwrap();
    let g_res = g.changeset_apply(&cs);
    let g_rows = dump_t(&g);
    let g_rows: String = g_rows.trim().to_string();

    let Some(s_out) = apply_oracle(baseline, &hex(&cs)) else {
        return;
    };
    let s_aborted = s_out
        .lines()
        .next()
        .map(|l| l.starts_with("APPLY_ERR"))
        .unwrap_or(false);
    // Normalise the oracle's rows (it prints reals with %.17g; graphite prints
    // "R<shortest>") so numeric-equal floats compare equal.
    let s_rows: String = s_out
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

    assert_eq!(
        g_rows, s_rows,
        "apply rows vs oracle\n baseline={baseline}\n dml={dml}"
    );
    assert_eq!(
        g_res.is_err(),
        s_aborted,
        "apply abort vs oracle\n baseline={baseline}\n dml={dml}\n g_res={g_res:?}"
    );
}

#[test]
fn apply_vs_oracle_happy_and_conflicts() {
    let s = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(1,'x',1.5),(2,'y',NULL);";
    // Happy path: apply onto the same baseline the changeset was generated
    // against.
    check_apply(
        s,
        "INSERT INTO t VALUES(3,'z',x'aabb'); UPDATE t SET b='X2' WHERE a=1; DELETE FROM t WHERE a=2;",
        s,
    );
    // NOTFOUND (UPDATE): target lacks the updated row → omit.
    check_apply(
        s,
        "UPDATE t SET b='X2' WHERE a=1;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(2,'y',NULL);",
    );
    // DATA (UPDATE): target's row differs from recorded old.* → omit.
    check_apply(
        s,
        "UPDATE t SET b='X2' WHERE a=1;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(1,'DIFFERENT',9.9);",
    );
    // NOTFOUND (DELETE): target lacks the deleted row → omit.
    check_apply(
        s,
        "DELETE FROM t WHERE a=2;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(1,'x',1.5);",
    );
    // CONFLICT (INSERT PK collision) → ABORT + rollback.
    check_apply(
        s,
        "INSERT INTO t VALUES(3,'z',7);",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); INSERT INTO t VALUES(3,'EXISTING',0);",
    );
}

// ---------------------------------------------------------------------------
// Broader primary-key shapes: composite, non-integer single, WITHOUT ROWID.
//
// SQLite's session module records any table with a declared PRIMARY KEY (under
// its default configuration) — keyed by every PK column, with the changeset's
// per-column PK-flag bytes carrying each PK column's 1-based ordinal. These
// tests assert graphite reproduces that byte-for-byte (via the oracle when
// configured) and round-trips through apply.
// ---------------------------------------------------------------------------

/// Round-trip helper for an arbitrary table `t`, ordering the dump by *all*
/// columns so tables without a single scalar `a` key still compare
/// deterministically.
fn roundtrip_order(setup: &str, dml: &str, order_by: &str) {
    use graphitesql::Value;
    let dump = |conn: &Connection| -> String {
        let r = conn
            .query(&format!("SELECT * FROM t ORDER BY {order_by}"))
            .unwrap();
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
        out
    };

    let mut a = Connection::open_memory().unwrap();
    a.execute_batch(setup).unwrap();
    let session = a.create_session();
    session.attach();
    a.execute_batch(dml).unwrap();
    let cs = a.session_changeset(&session).unwrap();
    let post_a = dump(&a);

    let mut b = Connection::open_memory().unwrap();
    b.execute_batch(setup).unwrap();
    b.changeset_apply(&cs).unwrap();
    let post_b = dump(&b);

    assert_eq!(post_a, post_b, "round-trip\n setup={setup}\n dml={dml}");
}

#[test]
fn composite_pk_insert() {
    check(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b));",
        "INSERT INTO t VALUES(1,2,3);",
        Some("540301020074001200010000000000000001010000000000000002010000000000000003"),
    );
}

#[test]
fn composite_pk_update_nonkey() {
    check(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)); INSERT INTO t VALUES(1,2,3);",
        "UPDATE t SET c=99 WHERE a=1;",
        None,
    );
}

#[test]
fn composite_pk_update_key_is_delete_insert() {
    // Changing a PK column is recorded as DELETE(old key) + INSERT(new key).
    check(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)); INSERT INTO t VALUES(1,2,3);",
        "UPDATE t SET a=99 WHERE a=1;",
        None,
    );
}

#[test]
fn composite_pk_reordered_flags() {
    // PRIMARY KEY(b,a): the PK-flag bytes carry ordinals a→2, b→1.
    check(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(b,a));",
        "INSERT INTO t VALUES(9,8,7);",
        Some("540302010074001200010000000000000009010000000000000008010000000000000007"),
    );
}

#[test]
fn composite_pk_delete() {
    check(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)); INSERT INTO t VALUES(1,2,3);",
        "DELETE FROM t WHERE a=1;",
        None,
    );
}

#[test]
fn text_pk_insert() {
    check(
        "CREATE TABLE t(a TEXT PRIMARY KEY,b);",
        "INSERT INTO t VALUES('k',5);",
        Some("540201007400120003016b010000000000000005"),
    );
}

#[test]
fn blob_pk_insert_update_delete() {
    check(
        "CREATE TABLE t(a BLOB PRIMARY KEY,b);",
        "INSERT INTO t VALUES(x'aa',1); UPDATE t SET b=2 WHERE a=x'aa'; INSERT INTO t VALUES(x'bb',3); DELETE FROM t WHERE a=x'bb';",
        None,
    );
}

#[test]
fn real_pk_insert() {
    check(
        "CREATE TABLE t(a REAL PRIMARY KEY,b);",
        "INSERT INTO t VALUES(1.5,7);",
        None,
    );
}

#[test]
fn without_rowid_insert_update_delete() {
    check(
        "CREATE TABLE t(a TEXT PRIMARY KEY,b) WITHOUT ROWID; INSERT INTO t VALUES('k',5);",
        "UPDATE t SET b=50 WHERE a='k';",
        None,
    );
    check(
        "CREATE TABLE t(a TEXT PRIMARY KEY,b) WITHOUT ROWID;",
        "INSERT INTO t VALUES('k',5); INSERT INTO t VALUES('m',6); DELETE FROM t WHERE a='m';",
        None,
    );
}

#[test]
fn without_rowid_composite() {
    check(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID;",
        "INSERT INTO t VALUES(1,2,3),(1,4,5);",
        None,
    );
    check(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID; INSERT INTO t VALUES(1,2,3);",
        "UPDATE t SET c=9 WHERE a=1 AND b=2;",
        None,
    );
}

#[test]
fn no_primary_key_is_not_recorded() {
    // A table with no declared PRIMARY KEY produces an empty changeset — exactly
    // as SQLite's session module skips it under the default configuration.
    check(
        "CREATE TABLE t(a,b);",
        "INSERT INTO t VALUES(1,2); UPDATE t SET b=3 WHERE a=1;",
        Some(""),
    );
}

#[test]
fn composite_pk_roundtrip() {
    roundtrip_order(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)); INSERT INTO t VALUES(1,2,3),(1,4,5);",
        "INSERT INTO t VALUES(2,2,'z'); UPDATE t SET c=99 WHERE a=1 AND b=2; DELETE FROM t WHERE a=1 AND b=4;",
        "a,b",
    );
}

#[test]
fn composite_pk_roundtrip_key_change() {
    roundtrip_order(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)); INSERT INTO t VALUES(1,2,3);",
        "UPDATE t SET a=9 WHERE a=1 AND b=2;",
        "a,b",
    );
}

#[test]
fn text_pk_roundtrip() {
    roundtrip_order(
        "CREATE TABLE t(a TEXT PRIMARY KEY,b,c); INSERT INTO t VALUES('x',1,2),('y',3,4);",
        "INSERT INTO t VALUES('z',5,6); UPDATE t SET b=99 WHERE a='x'; DELETE FROM t WHERE a='y';",
        "a",
    );
}

#[test]
fn without_rowid_roundtrip() {
    roundtrip_order(
        "CREATE TABLE t(a TEXT PRIMARY KEY,b,c) WITHOUT ROWID; INSERT INTO t VALUES('x',1,2),('y',3,4);",
        "INSERT INTO t VALUES('z',5,6); UPDATE t SET c=99 WHERE a='x'; DELETE FROM t WHERE a='y';",
        "a",
    );
}

#[test]
fn without_rowid_composite_roundtrip() {
    roundtrip_order(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID; INSERT INTO t VALUES(1,2,3),(1,4,5);",
        "INSERT INTO t VALUES(2,1,'q'); UPDATE t SET c=88 WHERE a=1 AND b=2; DELETE FROM t WHERE a=1 AND b=4;",
        "a,b",
    );
}

#[test]
fn apply_vs_oracle_broader_shapes() {
    // Composite PK: happy path.
    check_apply(
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)); INSERT INTO t VALUES(1,2,3),(1,4,5);",
        "INSERT INTO t VALUES(2,2,9); UPDATE t SET c=99 WHERE a=1 AND b=2; DELETE FROM t WHERE a=1 AND b=4;",
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)); INSERT INTO t VALUES(1,2,3),(1,4,5);",
    );
    // Non-integer single PK.
    check_apply(
        "CREATE TABLE t(a TEXT PRIMARY KEY,b,c); INSERT INTO t VALUES('x',1,2);",
        "INSERT INTO t VALUES('y',3,4); UPDATE t SET b=9 WHERE a='x';",
        "CREATE TABLE t(a TEXT PRIMARY KEY,b,c); INSERT INTO t VALUES('x',1,2);",
    );
    // WITHOUT ROWID.
    check_apply(
        "CREATE TABLE t(a TEXT PRIMARY KEY,b,c) WITHOUT ROWID; INSERT INTO t VALUES('x',1,2);",
        "INSERT INTO t VALUES('y',3,4); UPDATE t SET c=9 WHERE a='x'; DELETE FROM t WHERE a='x';",
        "CREATE TABLE t(a TEXT PRIMARY KEY,b,c) WITHOUT ROWID; INSERT INTO t VALUES('x',1,2);",
    );
}

// ---------------------------------------------------------------------------
// Changeset → changeset transforms: `Changeset::invert` / `Changeset::concat`
// (roadmap D5). Byte-literal assertions always run; when `GRAPHITE_CSTOOL`
// points at the C `cstool` oracle (amalgamation with SQLITE_ENABLE_SESSION),
// the differential half also runs. `cstool` usage:
//   cstool invert <hex>            -> hex of sqlite3changeset_invert
//   cstool concat <hexA> <hexB>    -> hex of sqlite3changeset_concat
// ---------------------------------------------------------------------------

use graphitesql::Changeset;

/// Ask the cstool oracle for `invert`/`concat` of hex changeset(s), or `None`
/// if the oracle binary is not configured.
fn cstool(args: &[&str]) -> Option<String> {
    let bin = std::env::var("GRAPHITE_CSTOOL").ok()?;
    let out = Command::new(bin).args(args).output().expect("run cstool");
    assert!(
        out.status.success(),
        "cstool {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Assert `Changeset::invert(cs)` equals `expect` (byte literal) and, when the
/// oracle is configured, the oracle's invert too.
fn check_invert(cs_hex: &str, expect: &str) {
    let cs: Vec<u8> = (0..cs_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cs_hex[i..i + 2], 16).unwrap())
        .collect();
    let got = hex(&Changeset::invert(&cs).unwrap());
    assert_eq!(got, expect, "invert byte-literal mismatch");
    if let Some(oracle) = cstool(&["invert", cs_hex]) {
        assert_eq!(got, oracle, "invert vs oracle mismatch");
    }
}

/// Assert `Changeset::concat(a, b)` equals `expect` and, when configured, the
/// oracle's concat too.
fn check_concat(a_hex: &str, b_hex: &str, expect: &str) {
    let dec = |h: &str| -> Vec<u8> {
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect()
    };
    let got = hex(&Changeset::concat(&dec(a_hex), &dec(b_hex)).unwrap());
    assert_eq!(got, expect, "concat byte-literal mismatch");
    if let Some(oracle) = cstool(&["concat", a_hex, b_hex]) {
        assert_eq!(got, oracle, "concat vs oracle mismatch");
    }
}

#[test]
fn invert_insert_becomes_delete() {
    // INSERT (1,2) -> DELETE (1,2): only the op byte 0x12 -> 0x09 changes.
    check_invert(
        "5402010074001200010000000000000001010000000000000002",
        "5402010074000900010000000000000001010000000000000002",
    );
}

#[test]
fn invert_delete_becomes_insert() {
    check_invert(
        "5402010074000900010000000000000001010000000000000007",
        "5402010074001200010000000000000001010000000000000007",
    );
}

#[test]
fn invert_update_swaps_old_and_new() {
    // UPDATE b: 2 -> 9 inverts to 9 -> 2 (PK unchanged, unchanged col omitted).
    check_invert(
        "540301000074001700010000000000000001010000000000000002000001000000000000000900",
        "540301000074001700010000000000000001010000000000000009000001000000000000000200",
    );
}

#[test]
fn concat_insert_then_update_is_insert_of_final() {
    check_concat(
        "540301000074001200010000000000000001010000000000000002010000000000000003",
        "540301000074001700010000000000000001010000000000000002000001000000000000000900",
        "540301000074001200010000000000000001010000000000000009010000000000000003",
    );
}

#[test]
fn concat_update_then_delete_is_delete_of_original() {
    check_concat(
        "540301000074001700010000000000000001010000000000000002000001000000000000000900",
        "540301000074000900010000000000000001010000000000000009010000000000000003",
        "540301000074000900010000000000000001010000000000000002010000000000000003",
    );
}

#[test]
fn concat_delete_then_insert_is_update() {
    check_concat(
        "540301000074000900010000000000000001010000000000000002010000000000000003",
        "540301000074001200010000000000000001010000000000000005010000000000000006",
        "54030100007400170001000000000000000101000000000000000201000000000000000300010000000000000005010000000000000006",
    );
}

#[test]
fn concat_insert_then_delete_is_nothing() {
    // A: INSERT (1,2,3); B: DELETE (1,2,3) -> empty changeset.
    check_concat(
        "540301000074001200010000000000000001010000000000000002010000000000000003",
        "540301000074000900010000000000000001010000000000000002010000000000000003",
        "",
    );
}

#[test]
fn invert_roundtrip_apply_undoes_original() {
    // Applying invert(cs) after cs restores the pre-cs state.
    let setup =
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c); INSERT INTO t VALUES(1,10,100),(2,20,200);";
    let cs = make_changeset(
        setup,
        "INSERT INTO t VALUES(3,30,300); UPDATE t SET b=99 WHERE a=1; DELETE FROM t WHERE a=2;",
    );
    let inv = Changeset::invert(&cs).unwrap();

    let mut conn = Connection::open_memory().unwrap();
    conn.execute_batch(setup).unwrap();
    let before = dump_t(&conn);
    conn.changeset_apply(&cs).unwrap();
    conn.changeset_apply(&inv).unwrap();
    assert_eq!(dump_t(&conn), before, "invert did not round-trip");
}

#[test]
fn concat_apply_equals_sequential_apply() {
    let setup =
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c); INSERT INTO t VALUES(1,10,100),(2,20,200);";
    let a = make_changeset(
        setup,
        "UPDATE t SET b=11 WHERE a=1; INSERT INTO t VALUES(3,30,300);",
    );

    // Record B on the state after A.
    let mut conn = Connection::open_memory().unwrap();
    conn.execute_batch(setup).unwrap();
    conn.changeset_apply(&a).unwrap();
    let sess = conn.create_session();
    sess.attach();
    conn.execute_batch(
        "UPDATE t SET c=999 WHERE a=1; DELETE FROM t WHERE a=2; UPDATE t SET b=33 WHERE a=3;",
    )
    .unwrap();
    let b = conn.session_changeset(&sess).unwrap();

    let cat = Changeset::concat(&a, &b).unwrap();

    // apply(concat) vs apply(a) then apply(b).
    let via_cat = {
        let mut c = Connection::open_memory().unwrap();
        c.execute_batch(setup).unwrap();
        c.changeset_apply(&cat).unwrap();
        dump_t(&c)
    };
    let via_seq = {
        let mut c = Connection::open_memory().unwrap();
        c.execute_batch(setup).unwrap();
        c.changeset_apply(&a).unwrap();
        c.changeset_apply(&b).unwrap();
        dump_t(&c)
    };
    assert_eq!(via_cat, via_seq, "concat apply != sequential apply");
}
