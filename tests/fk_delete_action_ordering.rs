//! A parent DELETE removes the parent row before enforcing its children's
//! referential actions, so an action that resolves to the just-deleted key sees
//! it gone. In particular `ON DELETE SET DEFAULT` whose default names the row
//! being deleted leaves the child dangling and must fail — graphite used to
//! enforce the action while the parent was still present, so the default's
//! existence check passed and a dangling child survived. CASCADE / SET NULL /
//! RESTRICT when deleting the referenced parent are unaffected. Verified against
//! sqlite3 3.50.4 by value and exit status (found by a foreign-key fuzzer).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> (String, bool) {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        o.status.success(),
    )
}

#[test]
fn delete_parent_fk_action_ordering_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // deleting the SET DEFAULT target parent → child would dangle → error
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid DEFAULT 1 REFERENCES p(id) ON DELETE SET DEFAULT);\
         INSERT INTO p VALUES(1),(2);INSERT INTO c VALUES(17,1);DELETE FROM p WHERE id=1;SELECT * FROM c;",
        // SET DEFAULT to a DIFFERENT surviving parent → succeeds
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid DEFAULT 1 REFERENCES p(id) ON DELETE SET DEFAULT);\
         INSERT INTO p VALUES(1),(2);INSERT INTO c VALUES(17,2);DELETE FROM p WHERE id=2;SELECT * FROM c ORDER BY cid;",
        // ON DELETE CASCADE removes the child (deleting the referenced parent)
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid REFERENCES p(id) ON DELETE CASCADE);\
         INSERT INTO p VALUES(1),(2);INSERT INTO c VALUES(10,1),(11,2);DELETE FROM p WHERE id=1;SELECT * FROM c ORDER BY cid;",
        // ON DELETE SET NULL nulls the child (always FK-valid)
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid REFERENCES p(id) ON DELETE SET NULL);\
         INSERT INTO p VALUES(1),(2);INSERT INTO c VALUES(10,1),(11,2);DELETE FROM p WHERE id=1;SELECT * FROM c ORDER BY cid;",
        // ON DELETE RESTRICT still blocks the delete
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid REFERENCES p(id) ON DELETE RESTRICT);\
         INSERT INTO p VALUES(1),(2);INSERT INTO c VALUES(10,1);DELETE FROM p WHERE id=1;SELECT * FROM c;",
        // deleting an UNreferenced parent leaves children intact
        "PRAGMA foreign_keys=ON;CREATE TABLE p(id INTEGER PRIMARY KEY);\
         CREATE TABLE c(cid INTEGER PRIMARY KEY, pid REFERENCES p(id) ON DELETE SET DEFAULT);\
         INSERT INTO p VALUES(1),(2),(3);INSERT INTO c VALUES(10,1);DELETE FROM p WHERE id=3;SELECT * FROM c;",
    ];
    for sql in cases {
        let (s_out, s_ok) = run("sqlite3", sql);
        let (g_out, g_ok) = run(g, sql);
        assert_eq!(s_ok, g_ok, "success/error status differs for `{sql}`");
        if s_ok {
            assert_eq!(s_out, g_out, "rows differ for `{sql}`");
        }
    }
}
