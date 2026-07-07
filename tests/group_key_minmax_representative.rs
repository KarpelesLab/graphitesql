//! SQLite's single-`min()`/`max()` bare-column rule governs the *displayed* GROUP
//! BY key too: in a query with exactly one `min()`/`max()` (in the projection,
//! HAVING, or ORDER BY), every bare column — including the projected grouping key
//! — takes its value from the row holding that extreme. So when a group contains
//! numerically-equal but differently-typed keys (`3` and `3.0`), the printed key
//! is the extreme row's representation. graphite emitted the group's stored key
//! (first-seen) value instead, so `SELECT a, min(b) … GROUP BY a` over rows
//! `(3.0,2),(3.0,9),(3,0)` printed `3.0|0` where SQLite prints `3|0` (the `min(b)`
//! row is `(3,0)`). Both VDBE grouped paths now route the key through the
//! min/max representative. With 2+ min/max the choice is unspecified in SQLite, so
//! those are left alone. Verified byte-for-byte against the sqlite3 3.50.4 CLI
//! (found by a randomized aggregate fuzzer).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn group_key_follows_single_minmax_row() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let setup = "CREATE TABLE t(a,b,c);\
        INSERT INTO t VALUES(3.0,2,'x'),(3.0,9,'y'),(3,0,'z'),\
        (5,1,'p'),(5.0,7,'q'),(5,4,'r'),(1,8,'m');";
    let queries = [
        // projection min/max — the plain VDBE grouped path
        "SELECT a,min(b) FROM t GROUP BY a ORDER BY a",
        "SELECT a,max(b) FROM t GROUP BY a ORDER BY a",
        "SELECT a,c,min(b) FROM t GROUP BY a ORDER BY a",
        // min/max in HAVING — the general grouped path
        "SELECT a FROM t GROUP BY a HAVING min(b)<5 ORDER BY a",
        "SELECT a,count(*) FROM t GROUP BY a HAVING max(b)>3 ORDER BY a",
        // min/max in ORDER BY
        "SELECT a FROM t GROUP BY a ORDER BY min(b)",
        // key + non-key bare column together following the same extreme
        "SELECT a,c,min(b) FROM t GROUP BY a HAVING count(*)>0 ORDER BY a",
        // regressions: no min/max (key = stored group value)
        "SELECT a,count(*) FROM t GROUP BY a ORDER BY a",
        "SELECT a,sum(b) FROM t GROUP BY a HAVING sum(b)>1 ORDER BY a",
    ];
    for q in queries {
        let sql = format!("{setup}{q};");
        assert_eq!(out("sqlite3", &sql), out(g, &sql), "mismatch for `{q}`");
    }
}
