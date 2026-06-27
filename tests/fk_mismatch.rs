//! A structurally malformed foreign key — referenced parent columns that do not
//! exist, are not collectively a PRIMARY KEY / non-partial UNIQUE index, or whose
//! count differs from the child side — is SQLite's `foreign key mismatch -
//! "<child>" referencing "<parent>"`. It fires from `PRAGMA foreign_key_check`
//! (independent of the `foreign_keys` setting) and from INSERT/UPDATE enforcement
//! when foreign keys are ON, in preference to the row-level checks. graphite
//! previously returned no rows from `foreign_key_check` and a misleading `no such
//! column` / `FOREIGN KEY constraint failed` at write time. Matched to the
//! `sqlite3` CLI (3.50.4).
//!
//! A missing parent *table* is deliberately out of scope here (SQLite surfaces it
//! as an ordinary row violation / `no such table`, not a mismatch).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// In-process error string, stripped of the `error: ` Display prefix.
fn err(setup: &[&str], stmt: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        // setup statements may be DDL/DML or a SELECT-less pragma; route by kind.
        if s.trim_start().to_ascii_uppercase().starts_with("PRAGMA") {
            let _ = c.execute(s);
        } else {
            c.execute(s).unwrap();
        }
    }
    let e = if stmt.trim_start().to_ascii_uppercase().starts_with("PRAGMA") {
        c.query(stmt).err().map(|e| e.to_string())
    } else {
        c.execute(stmt).err().map(|e| e.to_string())
    };
    e.unwrap_or_default()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn mismatch_from_foreign_key_check() {
    // Parent column missing, non-unique parent column, arity mismatch, omitted
    // columns with no parent PK, and a subset of a composite PK — all mismatch.
    let cases: &[(&[&str], &str)] = &[
        (
            &[
                "CREATE TABLE p(id PRIMARY KEY)",
                "CREATE TABLE c(pid REFERENCES p(nope))",
            ],
            "PRAGMA foreign_key_check",
        ),
        (
            &[
                "CREATE TABLE p(id PRIMARY KEY, x)",
                "CREATE TABLE c(pid REFERENCES p(x))",
            ],
            "PRAGMA foreign_key_check",
        ),
        (
            &[
                "CREATE TABLE p(a, b, PRIMARY KEY(a,b))",
                "CREATE TABLE c(x REFERENCES p)",
            ],
            "PRAGMA foreign_key_check",
        ),
        (
            &["CREATE TABLE p(id)", "CREATE TABLE c(pid REFERENCES p)"],
            "PRAGMA foreign_key_check",
        ),
        (
            &[
                "CREATE TABLE p(a, b, PRIMARY KEY(a,b))",
                "CREATE TABLE c(x REFERENCES p(a))",
            ],
            "PRAGMA foreign_key_check",
        ),
    ];
    for (setup, stmt) in cases {
        let e = err(setup, stmt);
        assert_eq!(
            e, "foreign key mismatch - \"c\" referencing \"p\"",
            "{setup:?}"
        );
    }
    // The table-scoped form reports the same.
    let e = err(
        &[
            "CREATE TABLE p(id PRIMARY KEY)",
            "CREATE TABLE c(pid REFERENCES p(nope))",
        ],
        "PRAGMA foreign_key_check(c)",
    );
    assert_eq!(e, "foreign key mismatch - \"c\" referencing \"p\"");
}

#[test]
fn mismatch_from_write_enforcement() {
    // With foreign keys ON, INSERT and UPDATE report the mismatch ahead of the
    // row-level lookup.
    let e = err(
        &[
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE p(id PRIMARY KEY)",
            "CREATE TABLE c(pid REFERENCES p(nope))",
        ],
        "INSERT INTO c VALUES(1)",
    );
    assert_eq!(e, "foreign key mismatch - \"c\" referencing \"p\"");

    let e = err(
        &[
            "CREATE TABLE p(id PRIMARY KEY)",
            "CREATE TABLE c(pid REFERENCES p(nope))",
            // Seed the row with FK enforcement off, then enable it for the UPDATE.
            "INSERT INTO c VALUES(1)",
            "PRAGMA foreign_keys=ON",
        ],
        "UPDATE c SET pid=2",
    );
    assert_eq!(e, "foreign key mismatch - \"c\" referencing \"p\"");
}

#[test]
fn well_formed_keys_are_not_a_mismatch() {
    // A PRIMARY KEY, an inline UNIQUE column, a standalone UNIQUE index, and a
    // composite UNIQUE are all valid referenced keys — no mismatch (the INSERTs
    // fail with the ordinary FOREIGN KEY violation because no parent row exists).
    for setup in [
        &[
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE p(id PRIMARY KEY)",
            "CREATE TABLE c(pid REFERENCES p)",
        ][..],
        &[
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE p(id PRIMARY KEY, x UNIQUE)",
            "CREATE TABLE c(pid REFERENCES p(x))",
        ][..],
        &[
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE p(id PRIMARY KEY, x)",
            "CREATE UNIQUE INDEX px ON p(x)",
            "CREATE TABLE c(pid REFERENCES p(x))",
        ][..],
    ] {
        let e = err(setup, "INSERT INTO c VALUES(1)");
        assert_eq!(e, "FOREIGN KEY constraint failed", "{setup:?}");
    }
    // A clean foreign_key_check over a satisfied key returns no rows (no error).
    let e = err(
        &[
            "CREATE TABLE p(id PRIMARY KEY)",
            "CREATE TABLE c(pid REFERENCES p)",
            "INSERT INTO p VALUES(1)",
            "INSERT INTO c VALUES(1)",
        ],
        "PRAGMA foreign_key_check",
    );
    assert_eq!(e, "");
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        let line = String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string();
        // graphite's CLI appends the SQLite extended result code ` (NN)`; drop it.
        match (line.rfind(" ("), line.ends_with(')')) {
            (Some(i), true)
                if line[i + 2..line.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit()) =>
            {
                line[..i].to_string()
            }
            _ => line,
        }
    };
    for sql in [
        // foreign_key_check: each malformed shape.
        "CREATE TABLE p(id PRIMARY KEY); CREATE TABLE c(pid REFERENCES p(nope)); PRAGMA foreign_key_check;",
        "CREATE TABLE p(id PRIMARY KEY, x); CREATE TABLE c(pid REFERENCES p(x)); PRAGMA foreign_key_check;",
        "CREATE TABLE p(a,b,PRIMARY KEY(a,b)); CREATE TABLE c(x REFERENCES p); PRAGMA foreign_key_check;",
        "CREATE TABLE p(id); CREATE TABLE c(pid REFERENCES p); PRAGMA foreign_key_check;",
        "CREATE TABLE p(a,b,PRIMARY KEY(a,b)); CREATE TABLE c(x REFERENCES p(a)); PRAGMA foreign_key_check;",
        "CREATE TABLE p(id PRIMARY KEY); CREATE TABLE c(pid REFERENCES p(nope)); PRAGMA foreign_key_check(c);",
        // write-time enforcement.
        "PRAGMA foreign_keys=ON; CREATE TABLE p(id PRIMARY KEY); CREATE TABLE c(pid REFERENCES p(nope)); INSERT INTO c VALUES(1);",
        "PRAGMA foreign_keys=ON; CREATE TABLE p(id PRIMARY KEY, x); CREATE TABLE c(pid REFERENCES p(x)); INSERT INTO c VALUES(1);",
        // well-formed keys: no mismatch.
        "PRAGMA foreign_keys=ON; CREATE TABLE p(id PRIMARY KEY); CREATE TABLE c(pid REFERENCES p); INSERT INTO c VALUES(1);",
        "PRAGMA foreign_keys=ON; CREATE TABLE p(id PRIMARY KEY, x UNIQUE); CREATE TABLE c(pid REFERENCES p(x)); INSERT INTO c VALUES(1);",
        "PRAGMA foreign_keys=ON; CREATE TABLE p(a,b,UNIQUE(a,b)); CREATE TABLE c(x,y,FOREIGN KEY(x,y) REFERENCES p(a,b)); INSERT INTO c VALUES(1,2);",
        "CREATE TABLE p(id PRIMARY KEY); CREATE TABLE c(pid REFERENCES p); INSERT INTO p VALUES(1); INSERT INTO c VALUES(1); PRAGMA foreign_key_check;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
