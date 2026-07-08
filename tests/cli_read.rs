//! `.read FILE` executes the SQL (and dot-commands) in `FILE` exactly as if
//! typed, matching the `sqlite3` shell. Verified byte-for-byte against the
//! sqlite3 3.50.4 CLI.

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
fn read_executes_file_like_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let uniq = std::process::id();
    let script = dir.join(format!("gsql_read_{uniq}.sql"));
    std::fs::write(
        &script,
        "CREATE TABLE t(a,b);\n\
         INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z');\n\
         SELECT a,b FROM t ORDER BY a;\n\
         .tables\n\
         SELECT count(*), sum(a) FROM t;\n\
         CREATE VIEW v AS SELECT a FROM t;\n\
         .schema\n",
    )
    .unwrap();
    let script = script.to_str().unwrap();

    let da = dir.join(format!("gsql_read_a_{uniq}.db"));
    let db = dir.join(format!("gsql_read_b_{uniq}.db"));
    let (da, dbp) = (da.to_str().unwrap(), db.to_str().unwrap());
    let _ = std::fs::remove_file(da);
    let _ = std::fs::remove_file(dbp);

    let input = format!(".read {script}\n");
    assert_eq!(run("sqlite3", da, &input), run(g, dbp, &input));

    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(da);
    let _ = std::fs::remove_file(dbp);
}
