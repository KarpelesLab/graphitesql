//! The CLI line reader accumulates input lines and executes the buffer once it
//! forms a complete statement. It previously decided "complete" by a bare
//! `buffer.ends_with(';')`, which cut a multi-line `CREATE TRIGGER … BEGIN … END`
//! at the first `;` inside its body — so loading a schema with triggers via
//! `graphitesql db < schema.sql` (or any piped/`.read` input) failed with
//! `Parse error: incomplete input`. The single-argument form (`graphitesql db
//! "…"`) was unaffected because the whole string goes straight to the parser.
//!
//! The fix makes the completion check `BEGIN … END` / `CASE … END` aware, exactly
//! like `split_statements`. These tests drive multi-line trigger bodies through
//! STDIN (the affected path) and check the trigger is created and fires, matching
//! the sqlite3 3.50.4 CLI where available.

#![cfg(feature = "std")]

use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Feed `script` to `bin <db>` over stdin (the piped, line-by-line reader path),
/// then return the stdout of a follow-up `probe` query on the resulting db.
fn stdin_then_query(bin: &str, script: &str, probe: &str) -> String {
    let dir = std::env::temp_dir();
    let db = dir.join(format!(
        "graphite_mlt_{}_{}.db",
        std::process::id(),
        // vary per call so parallel tests don't collide
        script.len() * 131 + probe.len()
    ));
    let _ = std::fs::remove_file(&db);

    let mut child = Command::new(bin)
        .arg(&db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let q = Command::new(bin).arg(&db).arg(probe).output().unwrap();
    let _ = std::fs::remove_file(&db);
    String::from_utf8_lossy(&q.stdout).trim_end().to_owned()
}

const TRIGGER_SCHEMA: &str = "\
CREATE TABLE a(k TEXT PRIMARY KEY, v INT);
CREATE TABLE b(k TEXT, v INT);
CREATE TRIGGER t AFTER INSERT ON a BEGIN
  INSERT INTO b(k,v) VALUES(NEW.k, NEW.v);
  UPDATE b SET v = v + 1 WHERE k = NEW.k;
END;
INSERT INTO a VALUES('x', 10);
";

#[test]
fn multiline_trigger_over_stdin_is_created_and_fires() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // The trigger exists …
    assert_eq!(
        stdin_then_query(
            g,
            TRIGGER_SCHEMA,
            "SELECT count(*) FROM sqlite_master WHERE type='trigger'"
        ),
        "1",
        "multi-line CREATE TRIGGER over stdin was not created"
    );
    // … and it fired (both body statements ran: insert then increment).
    assert_eq!(
        stdin_then_query(g, TRIGGER_SCHEMA, "SELECT k || '=' || v FROM b"),
        "x=11"
    );
    if sqlite3_available() {
        assert_eq!(
            stdin_then_query("sqlite3", TRIGGER_SCHEMA, "SELECT k || '=' || v FROM b"),
            "x=11",
            "oracle sanity"
        );
    }
}

const CASE_SCHEMA: &str = "\
CREATE TABLE t(id INTEGER PRIMARY KEY, s INT);
CREATE TABLE audit(id INT, note TEXT);
CREATE TRIGGER au AFTER UPDATE ON t BEGIN
  INSERT INTO audit VALUES(NEW.id, CASE WHEN NEW.s > 10 THEN 'hi' ELSE 'lo' END);
  SELECT 1;
END;
INSERT INTO t VALUES(1, 5);
UPDATE t SET s = 20 WHERE id = 1;
UPDATE t SET s = 3 WHERE id = 1;
";

#[test]
fn trigger_with_case_body_over_stdin_matches_sqlite() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let probe = "SELECT id || ':' || note FROM audit ORDER BY rowid";
    let got = stdin_then_query(g, CASE_SCHEMA, probe);
    assert_eq!(got, "1:hi\n1:lo");
    if sqlite3_available() {
        assert_eq!(stdin_then_query("sqlite3", CASE_SCHEMA, probe), got);
    }
}

#[test]
fn plain_multi_statement_batches_over_stdin_still_split() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Semicolons at end of line and mid-line must still separate statements.
    let script = "CREATE TABLE x(a);\nINSERT INTO x VALUES(1);\nINSERT INTO x VALUES(2); INSERT INTO x VALUES(3);\n";
    assert_eq!(
        stdin_then_query(g, script, "SELECT group_concat(a) FROM x"),
        "1,2,3"
    );
}

#[test]
fn comments_do_not_break_statement_splitting() {
    // The statement splitter must skip `--` and `/* */` comments: a comment
    // before a transaction `BEGIN` (or containing a `;`, or an apostrophe) used
    // to make the splitter treat `BEGIN` as a trigger body — swallowing the
    // transaction and raising `expected a single statement`.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases: &[(&str, &str, &str)] = &[
        (
            "comment before BEGIN",
            "CREATE TABLE t(a);\n-- a comment\nBEGIN;\nINSERT INTO t VALUES(1);\nCOMMIT;\n",
            "1",
        ),
        (
            "apostrophe in line comment",
            "CREATE TABLE t(a);\n-- don't break on this\nINSERT INTO t VALUES(2);\n",
            "2",
        ),
        (
            "semicolon in block comment",
            "CREATE TABLE t(a);\n/* has ; inside */\nINSERT INTO t VALUES(3);\n",
            "3",
        ),
        (
            "block comment before BEGIN",
            "CREATE TABLE t(a);\n/* note */ BEGIN;\nINSERT INTO t VALUES(4);\nCOMMIT;\n",
            "4",
        ),
    ];
    for (name, script, expected) in cases {
        assert_eq!(
            &stdin_then_query(g, script, "SELECT group_concat(a) FROM t"),
            expected,
            "case: {name}"
        );
    }

    // A comment inside a trigger body must not end the body early.
    let trig = "CREATE TABLE t(a);\nCREATE TABLE log(x);\n\
        CREATE TRIGGER tr AFTER INSERT ON t BEGIN\n  -- record it\n  INSERT INTO log VALUES(NEW.a);\nEND;\n\
        INSERT INTO t VALUES(7);\n";
    assert_eq!(stdin_then_query(g, trig, "SELECT x FROM log"), "7");
}
