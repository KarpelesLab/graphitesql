//! Foreign-key referential actions and existence checks across WITHOUT ROWID
//! (index-organized) tables — on both sides of the relationship.
//!
//! The FK machinery used to assume rowid identity everywhere: it scanned tables
//! with the rowid `TableCursor` (`scan_table`) and identified / deleted / updated
//! rows by rowid. Run against a WITHOUT ROWID table — whose b-tree is keyed by
//! the PK, not a rowid — that cursor misreads its index pages as table-leaf
//! pages and errors `table-leaf cell on non-table-leaf page`, so the action
//! failed to apply (or the child's existence check errored on insert).
//!
//! Both directions were broken:
//!   * child side — an action (CASCADE / SET NULL / SET DEFAULT / UPDATE CASCADE)
//!     targeting a WITHOUT ROWID *child*;
//!   * parent side — the existence check (`parent_has_key`) scanning a WITHOUT
//!     ROWID *parent*, and a WITHOUT ROWID parent's own DELETE/UPDATE never
//!     firing referential actions at all.
//!
//! Each case is differential vs sqlite3 3.50.4 with deterministic data: graphite
//! `integrity_check` = ok, sqlite3 `quick_check` on graphite's file = ok, and the
//! surviving row set byte-identical to a sqlite3 run of the same script.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn load(bin: &str, db: &str, sql: &str) {
    let _ = std::fs::remove_file(db);
    let out = Command::new(bin).arg(db).arg(sql).output().expect("run");
    assert!(
        out.status.success(),
        "load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn query(bin: &str, db: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(db).arg(sql).output().expect("query");
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

/// Run `sql` fully (may include multiple statements) and return combined
/// stdout+stderr, so an expected FK-violation error is part of the comparison.
fn run_full(bin: &str, db: &str, sql: &str) -> String {
    let _ = std::fs::remove_file(db);
    let out = Command::new(bin).arg(db).arg(sql).output().expect("run");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s.trim_end().to_owned()
}

fn tmp(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphite_fkwr_{}_{}.db", std::process::id(), name));
    p.to_string_lossy().into_owned()
}

/// Load `sql` with graphite; assert graphite integrity_check = ok, sqlite3
/// quick_check on graphite's file = ok, and each `probe` row set matches a
/// sqlite3 run of the same script.
fn assert_valid_and_matching(name: &str, sql: &str, probes: &[&str]) {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let gdb = tmp(&format!("g_{name}"));
    load(g, &gdb, sql);
    assert_eq!(
        query(g, &gdb, "PRAGMA integrity_check"),
        "ok",
        "[{name}] graphite integrity_check"
    );
    if sqlite3_available() {
        assert_eq!(
            query("sqlite3", &gdb, "PRAGMA quick_check"),
            "ok",
            "[{name}] sqlite3 quick_check on graphite file"
        );
        let sdb = tmp(&format!("s_{name}"));
        load("sqlite3", &sdb, sql);
        for probe in probes {
            assert_eq!(
                query(g, &gdb, probe),
                query("sqlite3", &sdb, probe),
                "[{name}] row set diverged from sqlite3 for `{probe}`"
            );
        }
        let _ = std::fs::remove_file(&sdb);
    }
    let _ = std::fs::remove_file(&gdb);
}

/// Assert graphite's full output for a script (including any FK-violation error)
/// matches sqlite3's, and — when the script leaves a valid file — that graphite's
/// file passes sqlite3 quick_check.
fn assert_full_matching(name: &str, sql: &str) {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let gdb = tmp(&format!("gf_{name}"));
    let gout = run_full(g, &gdb, sql);
    if sqlite3_available() {
        let sdb = tmp(&format!("sf_{name}"));
        let sout = run_full("sqlite3", &sdb, sql);
        assert_eq!(gout, sout, "[{name}] output diverged from sqlite3");
        // If sqlite accepted the script (no error), graphite's file must be valid.
        if !sout.to_lowercase().contains("error") {
            assert_eq!(
                query("sqlite3", &gdb, "PRAGMA quick_check"),
                "ok",
                "[{name}] sqlite3 quick_check on graphite file"
            );
        }
        let _ = std::fs::remove_file(&sdb);
    }
    let _ = std::fs::remove_file(&gdb);
}

// ---------------------------------------------------------------------------
// Child side: action targets a WITHOUT ROWID child.
// ---------------------------------------------------------------------------

/// The original repro: `ON DELETE CASCADE` into a composite-PK WITHOUT ROWID
/// child large enough to span many leaves (so the rowid cursor would misread
/// index pages). Deleting parents must cascade to the clustered child b-tree.
#[test]
fn on_delete_cascade_into_wr_child() {
    let sql = "PRAGMA foreign_keys=ON;\
        PRAGMA page_size=4096;\
        CREATE TABLE p(id INTEGER PRIMARY KEY);\
        CREATE TABLE ch(k TEXT, pid INT REFERENCES p(id) ON DELETE CASCADE, data BLOB, PRIMARY KEY(k,pid)) WITHOUT ROWID;\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<50) INSERT INTO p SELECT x FROM c;\
        WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<400)\
          INSERT INTO ch SELECT 'k'||x,(x%49)+1,zeroblob(1500) FROM c;\
        DELETE FROM p WHERE id<25;";
    assert_valid_and_matching(
        "wr_cascade_del",
        sql,
        &[
            "SELECT count(*) FROM p",
            "SELECT count(*), coalesce(sum(length(data)),0) FROM ch",
            "SELECT k,pid FROM ch ORDER BY k,pid",
        ],
    );
}

/// `ON DELETE SET NULL` into a WITHOUT ROWID child (FK column outside the PK).
#[test]
fn on_delete_set_null_into_wr_child() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(id INTEGER PRIMARY KEY);\
        CREATE TABLE ch(k TEXT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE SET NULL, tag TEXT) WITHOUT ROWID;\
        INSERT INTO p VALUES(1),(2),(3);\
        INSERT INTO ch VALUES('a',1,'x'),('b',1,'y'),('c',2,'z');\
        DELETE FROM p WHERE id=1;";
    assert_valid_and_matching(
        "wr_set_null",
        sql,
        &[
            "SELECT id FROM p ORDER BY id",
            "SELECT k,pid,tag FROM ch ORDER BY k",
        ],
    );
}

/// `ON DELETE SET DEFAULT` into a WITHOUT ROWID child, plus the re-check: a
/// default naming a still-present parent succeeds; one naming a missing parent
/// fails identically to sqlite (statement rolled back, child unchanged).
#[test]
fn on_delete_set_default_into_wr_child() {
    let ok = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(id INTEGER PRIMARY KEY);\
        CREATE TABLE ch(k TEXT PRIMARY KEY, pid INT DEFAULT 3 REFERENCES p(id) ON DELETE SET DEFAULT) WITHOUT ROWID;\
        INSERT INTO p VALUES(1),(2),(3);\
        INSERT INTO ch VALUES('a',1),('b',1),('c',2);\
        DELETE FROM p WHERE id=1;";
    assert_valid_and_matching(
        "wr_set_default",
        ok,
        &[
            "SELECT id FROM p ORDER BY id",
            "SELECT k,pid FROM ch ORDER BY k",
        ],
    );

    // Default value (9) names a parent that never existed → FK violation.
    let bad = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(id INTEGER PRIMARY KEY);\
        CREATE TABLE ch(k TEXT PRIMARY KEY, pid INT DEFAULT 9 REFERENCES p(id) ON DELETE SET DEFAULT) WITHOUT ROWID;\
        INSERT INTO p VALUES(1),(2);\
        INSERT INTO ch VALUES('a',1);\
        DELETE FROM p WHERE id=1;\
        SELECT k,pid FROM ch ORDER BY k;";
    assert_full_matching("wr_set_default_reject", bad);
}

/// `ON UPDATE CASCADE` into a WITHOUT ROWID child where the FK column is part of
/// the clustered PK — the key change re-clusters the b-tree (delete-old +
/// insert-new), exercised via the whole-table rewrite.
#[test]
fn on_update_cascade_into_wr_child_pk_recluster() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(id INTEGER PRIMARY KEY);\
        CREATE TABLE ch(k TEXT, pid INT REFERENCES p(id) ON UPDATE CASCADE, PRIMARY KEY(k,pid)) WITHOUT ROWID;\
        INSERT INTO p VALUES(1),(2);\
        INSERT INTO ch VALUES('a',1),('b',1),('c',2);\
        UPDATE p SET id=100 WHERE id=1;";
    assert_valid_and_matching(
        "wr_update_cascade",
        sql,
        &[
            "SELECT id FROM p ORDER BY id",
            "SELECT k,pid FROM ch ORDER BY k,pid",
        ],
    );
}

/// A WITHOUT ROWID child carrying secondary indexes: a cascade delete must keep
/// every index consistent (probe by each indexed column).
#[test]
fn cascade_keeps_wr_child_secondary_indexes() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(id INTEGER PRIMARY KEY);\
        CREATE TABLE ch(k TEXT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE, tag TEXT) WITHOUT ROWID;\
        CREATE INDEX ich_tag ON ch(tag);\
        CREATE INDEX ich_pid ON ch(pid);\
        INSERT INTO p VALUES(1),(2),(3);\
        INSERT INTO ch VALUES('a',1,'red'),('b',1,'green'),('c',2,'blue'),('d',3,'red');\
        DELETE FROM p WHERE id=1;";
    assert_valid_and_matching(
        "wr_secondary_idx",
        sql,
        &[
            "SELECT k,pid,tag FROM ch ORDER BY k",
            "SELECT k FROM ch WHERE tag='red' ORDER BY k",
            "SELECT k FROM ch WHERE pid=2",
            "SELECT count(*) FROM ch WHERE tag='green'",
        ],
    );
}

/// Recursive cascade through WITHOUT ROWID tables: p → WR child → WR grandchild.
#[test]
fn recursive_cascade_through_wr_tables() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(id INTEGER PRIMARY KEY);\
        CREATE TABLE ch(k TEXT, pid INT REFERENCES p(id) ON DELETE CASCADE, PRIMARY KEY(k)) WITHOUT ROWID;\
        CREATE TABLE gc(g TEXT, ck TEXT REFERENCES ch(k) ON DELETE CASCADE, PRIMARY KEY(g)) WITHOUT ROWID;\
        INSERT INTO p VALUES(1),(2);\
        INSERT INTO ch VALUES('a',1),('b',1),('c',2);\
        INSERT INTO gc VALUES('g1','a'),('g2','a'),('g3','b'),('g4','c');\
        DELETE FROM p WHERE id=1;";
    assert_valid_and_matching(
        "wr_recursive",
        sql,
        &[
            "SELECT k,pid FROM ch ORDER BY k",
            "SELECT g,ck FROM gc ORDER BY g",
        ],
    );
}

/// Self-referential WITHOUT ROWID cascade: deleting a subtree root removes its
/// transitive descendants (exercises the live re-scan after each row's own
/// dependents are enforced).
#[test]
fn self_referential_wr_cascade() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE n(id TEXT PRIMARY KEY, parent TEXT REFERENCES n(id) ON DELETE CASCADE) WITHOUT ROWID;\
        INSERT INTO n VALUES('root',NULL),('a','root'),('b','root'),('a1','a'),('a2','a'),('b1','b');\
        DELETE FROM n WHERE id='a';";
    assert_valid_and_matching("wr_self_ref", sql, &["SELECT id,parent FROM n ORDER BY id"]);
}

// ---------------------------------------------------------------------------
// Parent side: the referenced parent is WITHOUT ROWID.
// ---------------------------------------------------------------------------

/// A rowid child referencing a WITHOUT ROWID parent: the insert existence check
/// (`parent_has_key`) scans the WR parent — it must accept a present key and
/// reject an absent one, matching sqlite (including the error text).
#[test]
fn insert_existence_check_scans_wr_parent() {
    let ok = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(k TEXT PRIMARY KEY) WITHOUT ROWID;\
        CREATE TABLE c(id INTEGER PRIMARY KEY, pk TEXT REFERENCES p(k));\
        INSERT INTO p VALUES('a'),('b'),('c');\
        INSERT INTO c VALUES(1,'a'),(2,'b'),(3,'a');\
        SELECT id,pk FROM c ORDER BY id;";
    assert_full_matching("wr_parent_exists_ok", ok);

    let bad = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(k TEXT PRIMARY KEY) WITHOUT ROWID;\
        CREATE TABLE c(id INTEGER PRIMARY KEY, pk TEXT REFERENCES p(k));\
        INSERT INTO p VALUES('a'),('b');\
        INSERT INTO c VALUES(1,'a');\
        INSERT INTO c VALUES(2,'zzz');\
        SELECT id,pk FROM c ORDER BY id;";
    assert_full_matching("wr_parent_exists_reject", bad);
}

/// `ON DELETE CASCADE` from a WITHOUT ROWID parent to a rowid child: deleting a
/// WR-parent row must fire the child action (the WR delete path never did).
#[test]
fn on_delete_cascade_from_wr_parent_to_rowid_child() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(k TEXT PRIMARY KEY) WITHOUT ROWID;\
        CREATE TABLE c(id INTEGER PRIMARY KEY, pk TEXT REFERENCES p(k) ON DELETE CASCADE);\
        INSERT INTO p VALUES('a'),('b'),('c');\
        INSERT INTO c VALUES(1,'a'),(2,'a'),(3,'b');\
        DELETE FROM p WHERE k='a';";
    assert_valid_and_matching(
        "wr_parent_cascade",
        sql,
        &[
            "SELECT k FROM p ORDER BY k",
            "SELECT id,pk FROM c ORDER BY id",
        ],
    );
}

/// A WITHOUT ROWID parent's DELETE under `RESTRICT` must be blocked (child rows
/// remain), matching sqlite's error.
#[test]
fn wr_parent_delete_restrict_blocks() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(k TEXT PRIMARY KEY) WITHOUT ROWID;\
        CREATE TABLE c(id INTEGER PRIMARY KEY, pk TEXT REFERENCES p(k) ON DELETE RESTRICT);\
        INSERT INTO p VALUES('a'),('b');\
        INSERT INTO c VALUES(1,'a');\
        DELETE FROM p WHERE k='a';\
        SELECT (SELECT count(*) FROM p), (SELECT count(*) FROM c);";
    assert_full_matching("wr_parent_restrict", sql);
}

/// `ON UPDATE CASCADE` from a WITHOUT ROWID parent (its PK changes) to a rowid
/// child.
#[test]
fn on_update_cascade_from_wr_parent() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(k TEXT PRIMARY KEY) WITHOUT ROWID;\
        CREATE TABLE c(id INTEGER PRIMARY KEY, pk TEXT REFERENCES p(k) ON UPDATE CASCADE);\
        INSERT INTO p VALUES('a'),('b');\
        INSERT INTO c VALUES(1,'a'),(2,'b'),(3,'a');\
        UPDATE p SET k='z' WHERE k='a';";
    assert_valid_and_matching(
        "wr_parent_update_cascade",
        sql,
        &[
            "SELECT k FROM p ORDER BY k",
            "SELECT id,pk FROM c ORDER BY id",
        ],
    );
}

// ---------------------------------------------------------------------------
// Both sides WITHOUT ROWID.
// ---------------------------------------------------------------------------

/// A WITHOUT ROWID parent AND a WITHOUT ROWID child: cascade delete must scan
/// and rewrite the clustered b-tree on both sides.
#[test]
fn cascade_wr_parent_and_wr_child() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;\
        CREATE TABLE c(ck TEXT, pk TEXT REFERENCES p(k) ON DELETE CASCADE, PRIMARY KEY(ck)) WITHOUT ROWID;\
        INSERT INTO p VALUES('a',1),('b',2),('c',3);\
        INSERT INTO c VALUES('x','a'),('y','a'),('z','b');\
        DELETE FROM p WHERE k='a';";
    assert_valid_and_matching(
        "wr_both_sides",
        sql,
        &[
            "SELECT k,v FROM p ORDER BY k",
            "SELECT ck,pk FROM c ORDER BY ck",
        ],
    );
}

/// A composite-PK WITHOUT ROWID child under a composite FK (all-columns key),
/// cascade delete.
#[test]
fn composite_fk_into_composite_pk_wr_child() {
    let sql = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(a INT, b INT, PRIMARY KEY(a,b));\
        CREATE TABLE ch(k TEXT, fa INT, fb INT, PRIMARY KEY(k), FOREIGN KEY(fa,fb) REFERENCES p(a,b) ON DELETE CASCADE) WITHOUT ROWID;\
        INSERT INTO p VALUES(1,1),(1,2),(2,1);\
        INSERT INTO ch VALUES('x',1,1),('y',1,1),('z',1,2);\
        DELETE FROM p WHERE a=1 AND b=1;";
    assert_valid_and_matching("wr_composite", sql, &["SELECT k,fa,fb FROM ch ORDER BY k"]);
}
