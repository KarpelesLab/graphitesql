//! FTS5 special maintenance/config commands written through the table-named
//! command column (`INSERT INTO t(t) VALUES('<cmd>')` / `INSERT INTO t(t, rank)
//! VALUES('<cmd>', <n>)`). graphite used to recognise only `rebuild`/`optimize`/
//! `delete`/`delete-all`/`rank` and rejected the rest with `no such column: t`.
//! It now matches sqlite3 3.50.4: the segment-tuning / flush / integrity-check
//! commands are accepted (no-ops on graphite's bulk-rebuilt single-segment index,
//! and the check always passes), an unrecognised command and a `delete` on a
//! self-content table both report a bare `SQL logic error`, and `delete-all` on a
//! self-content table names no table in its message.

#![cfg(all(feature = "std", feature = "fts5"))]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `sql` through a CLI and return stdout plus any error message, with each
/// CLI's own error-line prefix stripped so only the library-level text compares.
fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    for line in String::from_utf8_lossy(&o.stderr).lines() {
        let msg = line
            .trim_start_matches("Parse error near line 1: ")
            .trim_start_matches("Runtime error near line 1: ")
            .trim_start_matches("Error: error: ")
            .trim_start_matches("Error: ")
            // The sqlite CLI prefixes a step-time error with `stepping, `; the
            // library message that follows is what compares.
            .trim_start_matches("stepping, ");
        s.push_str(msg);
        s.push('\n');
    }
    s
}

#[test]
fn fts5_special_commands_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let f = "CREATE VIRTUAL TABLE ft USING fts5(title, body);\
             INSERT INTO ft VALUES('a','b language'),('c','d language');";
    let cases: &[String] = &[
        // accepted no-op / verify commands (single-column form)
        format!("{f}INSERT INTO ft(ft) VALUES('integrity-check');SELECT 'done';"),
        format!("{f}INSERT INTO ft(ft) VALUES('merge');SELECT 'done';"),
        format!("{f}INSERT INTO ft(ft) VALUES('flush');SELECT 'done';"),
        // accepted config values (t, rank) form
        format!("{f}INSERT INTO ft(ft,rank) VALUES('automerge',4);SELECT 'done';"),
        format!("{f}INSERT INTO ft(ft,rank) VALUES('usermerge',4);SELECT 'done';"),
        format!("{f}INSERT INTO ft(ft,rank) VALUES('crisismerge',16);SELECT 'done';"),
        format!("{f}INSERT INTO ft(ft,rank) VALUES('pgsz',1000);SELECT 'done';"),
        format!("{f}INSERT INTO ft(ft,rank) VALUES('deletemerge',10);SELECT 'done';"),
        format!("{f}INSERT INTO ft(ft,rank) VALUES('secure-delete',1);SELECT 'done';"),
        format!("{f}INSERT INTO ft(ft,rank) VALUES('integrity-check',0);SELECT 'done';"),
        // errors
        format!("{f}INSERT INTO ft(ft) VALUES('delete-all');"),
        format!("{f}INSERT INTO ft(ft,rowid,title,body) VALUES('delete',1,'a','b language');"),
        format!("{f}INSERT INTO ft(ft) VALUES('nonsense');"),
        // still-working commands (must not regress)
        format!("{f}INSERT INTO ft(ft) VALUES('rebuild');SELECT count(*) FROM ft;"),
        format!("{f}INSERT INTO ft(ft) VALUES('optimize');SELECT count(*) FROM ft;"),
        format!(
            "{f}INSERT INTO ft(ft,rank) VALUES('rank','bm25(2.0,1.0)');\
             SELECT rowid FROM ft WHERE ft MATCH 'language' ORDER BY rank;"
        ),
    ];
    for q in cases {
        assert_eq!(run("sqlite3", q), run(g, q), "mismatch for `{q}`");
    }
}
