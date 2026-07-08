//! `.databases` lists each attached database as `<name>: <file-or-""> r/w`,
//! matching the `sqlite3` shell (from `PRAGMA database_list`; the shell opens
//! read/write and no transaction is active during a dot-command). Verified
//! byte-for-byte against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, db: &str, input: &str) -> String {
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn databases_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let uniq = std::process::id();
    let a = dir.join(format!("gsql_db_a_{uniq}.db"));
    let b = dir.join(format!("gsql_db_b_{uniq}.db"));
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
    for f in [a, b] {
        let _ = std::fs::remove_file(f);
        assert!(
            Command::new("sqlite3")
                .arg(f)
                .arg("CREATE TABLE t(x);")
                .status()
                .unwrap()
                .success()
        );
    }

    // main only, and main + an attached database.
    assert_eq!(run("sqlite3", a, ".databases\n"), run(g, a, ".databases\n"));
    let attach = format!("ATTACH '{b}' AS other;\n.databases\n");
    assert_eq!(run("sqlite3", a, &attach), run(g, a, &attach));
    // in-memory: the file column is `""`.
    assert_eq!(
        run("sqlite3", ":memory:", ".databases\n"),
        run(g, ":memory:", ".databases\n")
    );
    for f in [a, b] {
        let _ = std::fs::remove_file(f);
    }
}
