//! Differential tests for changeset rebase — [`graphitesql::Rebaser`] and
//! [`graphitesql::Connection::changeset_apply_rebase`] (roadmap D5).
//!
//! Flow: two peers start from a common `base`. Peer A makes `remote_dml`, peer B
//! makes `local_dml`. B applies A's changeset onto its own (already-modified) DB
//! with a conflict policy, capturing a rebase blob; B then rebases its local
//! changeset so it can be replayed on top of A's changes. graphite's rebased
//! changeset is compared byte-for-byte against the SQLite oracle `sesrebase`
//! (apply_v2 + sqlite3_rebaser), when `GRAPHITE_SESREBASE` is configured.

#![cfg(feature = "std")]

use graphitesql::{ConflictAction as A, ConflictType as T, Connection, Rebaser};
use std::process::Command;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A changeset recording `dml` run over `base` (all tables attached).
fn changeset(base: &str, dml: &str) -> Vec<u8> {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(base).unwrap();
    let s = c.create_session();
    s.attach();
    c.execute_batch(dml).unwrap();
    c.session_changeset(&s).unwrap()
}

fn policy(name: &str) -> impl FnMut(T) -> A + '_ {
    move |k| match name {
        "replace" => match k {
            T::Data | T::Conflict => A::Replace,
            _ => A::Omit,
        },
        _ => A::Omit,
    }
}

/// graphite's rebased local changeset, as hex.
fn graphite_rebase(base: &str, remote_dml: &str, local_dml: &str, pol: &str) -> String {
    let remote = changeset(base, remote_dml);
    let local = changeset(base, local_dml);

    // B's DB = base + B's local change already applied.
    let mut b = Connection::open_memory().unwrap();
    b.execute_batch(base).unwrap();
    b.execute_batch(local_dml).unwrap();

    let blob = b.changeset_apply_rebase(&remote, policy(pol)).unwrap();

    let mut reb = Rebaser::new();
    reb.configure(&blob).unwrap();
    hex(&reb.rebase(&local).unwrap())
}

/// The SQLite oracle's rebased local changeset, or `None` if not configured.
fn oracle_rebase(base: &str, remote_dml: &str, local_dml: &str, pol: &str) -> Option<String> {
    let bin = std::env::var("GRAPHITE_SESREBASE").ok()?;
    let remote = hex(&changeset(base, remote_dml));
    let local = hex(&changeset(base, local_dml));
    let bdb = format!("{base} {local_dml}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(bdb)
        .arg(remote)
        .arg(local)
        .arg(pol)
        .output()
        .expect("run sesrebase");
    assert!(
        out.status.success(),
        "sesrebase failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Assert graphite's rebase matches the oracle (when configured).
fn check(base: &str, remote_dml: &str, local_dml: &str, pol: &str) {
    let got = graphite_rebase(base, remote_dml, local_dml, pol);
    if let Some(reference) = oracle_rebase(base, remote_dml, local_dml, pol) {
        assert_eq!(
            got, reference,
            "rebase vs oracle\n base={base}\n remote={remote_dml}\n local={local_dml}\n policy={pol}"
        );
    }
}

const S: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b);";

// Rule: Local UPDATE vs remote UPDATE.
#[test]
fn update_vs_update_omit() {
    check(
        &format!("{S} INSERT INTO t VALUES(1,'base');"),
        "UPDATE t SET b='A' WHERE a=1;",
        "UPDATE t SET b='B' WHERE a=1;",
        "omit",
    );
}

#[test]
fn update_vs_update_replace() {
    check(
        &format!("{S} INSERT INTO t VALUES(1,'base');"),
        "UPDATE t SET b='A' WHERE a=1;",
        "UPDATE t SET b='B' WHERE a=1;",
        "replace",
    );
}

// Rule: Local INSERT vs remote INSERT (same PK).
#[test]
fn insert_vs_insert_omit() {
    check(
        S,
        "INSERT INTO t VALUES(1,'A');",
        "INSERT INTO t VALUES(1,'B');",
        "omit",
    );
}

#[test]
fn insert_vs_insert_replace() {
    check(
        S,
        "INSERT INTO t VALUES(1,'A');",
        "INSERT INTO t VALUES(1,'B');",
        "replace",
    );
}

// Rule: Local UPDATE vs remote DELETE → UPDATE becomes INSERT (OMIT).
#[test]
fn update_vs_delete_omit() {
    check(
        &format!("{S} INSERT INTO t VALUES(1,'base');"),
        "DELETE FROM t WHERE a=1;",
        "UPDATE t SET b='B' WHERE a=1;",
        "omit",
    );
}

// Rule: Local DELETE vs remote UPDATE → old.* rebased (OMIT).
#[test]
fn delete_vs_update_omit() {
    check(
        &format!("{S} INSERT INTO t VALUES(1,'base');"),
        "UPDATE t SET b='A' WHERE a=1;",
        "DELETE FROM t WHERE a=1;",
        "omit",
    );
}

// Rule: Local DELETE vs remote DELETE → dropped.
#[test]
fn delete_vs_delete_omit() {
    check(
        &format!("{S} INSERT INTO t VALUES(1,'base');"),
        "DELETE FROM t WHERE a=1;",
        "DELETE FROM t WHERE a=1;",
        "omit",
    );
}

// No conflict: the local change touches a different row → unchanged.
#[test]
fn no_conflict_passthrough() {
    check(
        &format!("{S} INSERT INTO t VALUES(1,'x'),(2,'y');"),
        "UPDATE t SET b='A' WHERE a=1;",
        "UPDATE t SET b='Y' WHERE a=2;",
        "omit",
    );
}

// Multi-column: a partial local UPDATE rebased against a remote UPDATE of a
// different column (per-field rebase).
#[test]
fn multicol_partial_update_omit() {
    let s = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);";
    check(
        &format!("{s} INSERT INTO t VALUES(1,'b0','c0');"),
        "UPDATE t SET b='bA' WHERE a=1;",
        "UPDATE t SET c='cB' WHERE a=1;",
        "omit",
    );
}

#[test]
fn multicol_partial_update_replace() {
    let s = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);";
    // Local updates both b and c; remote updates b. REPLACE removes the local
    // update to b (also updated remotely), leaving only c.
    check(
        &format!("{s} INSERT INTO t VALUES(1,'b0','c0');"),
        "UPDATE t SET b='bA' WHERE a=1;",
        "UPDATE t SET b='bB', c='cB' WHERE a=1;",
        "replace",
    );
}

// A text primary key.
#[test]
fn text_pk_update_vs_update_omit() {
    let s = "CREATE TABLE t(a TEXT PRIMARY KEY, b);";
    check(
        &format!("{s} INSERT INTO t VALUES('k','base');"),
        "UPDATE t SET b='A' WHERE a='k';",
        "UPDATE t SET b='B' WHERE a='k';",
        "omit",
    );
}

// A mix of conflicting and non-conflicting rows across a multi-row changeset.
#[test]
fn multi_row_mixed() {
    let base = format!("{S} INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z');");
    check(
        &base,
        "UPDATE t SET b='A2' WHERE a=2; DELETE FROM t WHERE a=3;",
        "UPDATE t SET b='B1' WHERE a=1; UPDATE t SET b='B2' WHERE a=2; UPDATE t SET b='B3' WHERE a=3;",
        "omit",
    );
    check(
        &base,
        "UPDATE t SET b='A2' WHERE a=2;",
        "UPDATE t SET b='B2' WHERE a=2;",
        "replace",
    );
}

// Every storage class as the conflicting value (update-vs-update, both policies).
#[test]
fn value_types_update_conflict() {
    let vals = ["42", "-7.5", "'text'", "x'00ff10'", "NULL"];
    for v in vals {
        for pol in ["omit", "replace"] {
            check(
                &format!("{S} INSERT INTO t VALUES(1,'base');"),
                &format!("UPDATE t SET b={v} WHERE a=1;"),
                "UPDATE t SET b='local' WHERE a=1;",
                pol,
            );
        }
    }
}

// Deterministic fuzz: vary remote/local values and policy across the
// update-vs-update and insert-vs-insert rules.
#[test]
fn fuzz_update_and_insert_conflicts() {
    let vs = ["10", "20", "'p'", "'q'", "3.5", "x'ab'"];
    let mut n = 0;
    for (i, rv) in vs.iter().enumerate() {
        for (j, lv) in vs.iter().enumerate() {
            let pol = if (i + j) % 2 == 0 { "omit" } else { "replace" };
            // update vs update
            check(
                &format!("{S} INSERT INTO t VALUES(1,'base');"),
                &format!("UPDATE t SET b={rv} WHERE a=1;"),
                &format!("UPDATE t SET b={lv} WHERE a=1;"),
                pol,
            );
            // insert vs insert (same PK)
            check(
                S,
                &format!("INSERT INTO t VALUES(1,{rv});"),
                &format!("INSERT INTO t VALUES(1,{lv});"),
                pol,
            );
            n += 2;
        }
    }
    assert!(n >= 72);
}
