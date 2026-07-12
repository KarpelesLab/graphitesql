//! Differential testing of the FTS5 `rank` configuration, matching sqlite3
//! 3.50.4.
//!
//! Gap A — the `rank` config command. `INSERT INTO t(t, rank) VALUES('rank',
//! '<rankfunc>')` stores `<rankfunc>` (e.g. `bm25(10.0)`) in the `_config` shadow
//! under key `rank`; thereafter the bare `rank` column and `ORDER BY rank`
//! evaluate that weighted function instead of the default `bm25()`. Reset is via
//! the value `'bm25()'` (a `NULL` value is a hard error, matching sqlite). The
//! config persists across queries and on disk (a graphite-written table reopens
//! with the rank still applied, and sqlite reads the `_config` `rank` row).
//!
//! Gap B — `rank='…'` is NOT a valid CREATE option in this sqlite version:
//! `CREATE VIRTUAL TABLE t USING fts5(x, rank='…')` errors `unrecognized option:
//! "rank"`; the known option keywords (`tokenize`/`prefix`/`content`/…) still work.

#![cfg(all(feature = "std", feature = "fts5"))]

use graphitesql::{Connection, Value};
use std::process::Command;

/// Run `sql` through the sqlite3 CLI over `path`, returning trimmed stdout.
fn sqlite3(path: &str, sql: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// Whether the sqlite3 CLI errors on `sql` (non-empty stderr).
fn sqlite3_errs(path: &str, sql: &str) -> bool {
    let o = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    !o.stderr.is_empty()
}

fn render(result: &graphitesql::QueryResult) -> String {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn have_sqlite3() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn tmp(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!("gsql-fts5rank-{tag}-{}.db", std::process::id()));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

/// Open a fresh graphite in-memory connection and run each `;`-separated batch.
fn graphite_mem(batch: &str) -> Connection {
    let mut g = Connection::open_memory().unwrap();
    for s in batch.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    g
}

/// Gap A: the bare `rank` column and `ORDER BY rank` reflect the configured rank
/// function; multi-column weights; reset to the default; the config persists
/// across queries; and it is byte-exact vs sqlite3.
#[test]
fn rank_config_matches_sqlite3() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }

    // A one-column table: set `bm25(10.0)`, then a default (reset via `bm25()`),
    // then re-set. A two-column table for weighted `bm25(2.0, 1.0)`.
    struct Case {
        setup: &'static str,
        query: &'static str,
    }
    let cases = [
        // Bare `rank` under a configured weight.
        Case {
            setup: "CREATE VIRTUAL TABLE ft USING fts5(x);\
                    INSERT INTO ft VALUES('a a b'),('a b b');\
                    INSERT INTO ft(ft, rank) VALUES('rank', 'bm25(10.0)');",
            query: "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rowid",
        },
        // `ORDER BY rank` under a configured weight (rows ordered by relevance).
        Case {
            setup: "CREATE VIRTUAL TABLE ft USING fts5(x);\
                    INSERT INTO ft VALUES('a a b'),('a b b');\
                    INSERT INTO ft(ft, rank) VALUES('rank', 'bm25(10.0)');",
            query: "SELECT rowid FROM ft WHERE ft MATCH 'a' ORDER BY rank",
        },
        // No config at all: the default bm25().
        Case {
            setup: "CREATE VIRTUAL TABLE ft USING fts5(x);\
                    INSERT INTO ft VALUES('a a b'),('a b b');",
            query: "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rowid",
        },
        // Reset to default via `bm25()` after a non-default config.
        Case {
            setup: "CREATE VIRTUAL TABLE ft USING fts5(x);\
                    INSERT INTO ft VALUES('a a b'),('a b b');\
                    INSERT INTO ft(ft, rank) VALUES('rank', 'bm25(10.0)');\
                    INSERT INTO ft(ft, rank) VALUES('rank', 'bm25()');",
            query: "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rowid",
        },
        // Single-column `bm25(1.0)` equals the default (weight of 1.0).
        Case {
            setup: "CREATE VIRTUAL TABLE ft USING fts5(x);\
                    INSERT INTO ft VALUES('a a b'),('a b b');\
                    INSERT INTO ft(ft, rank) VALUES('rank', 'bm25(1.0)');",
            query: "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rowid",
        },
        // Integer-form weight `bm25(10)` equals `bm25(10.0)`.
        Case {
            setup: "CREATE VIRTUAL TABLE ft USING fts5(x);\
                    INSERT INTO ft VALUES('a a b'),('a b b');\
                    INSERT INTO ft(ft, rank) VALUES('rank', 'bm25(10)');",
            query: "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rowid",
        },
        // Two columns with per-column weights `bm25(2.0, 1.0)`.
        Case {
            setup: "CREATE VIRTUAL TABLE ft USING fts5(a, b);\
                    INSERT INTO ft VALUES('x y', 'x x'),('x','y y x');\
                    INSERT INTO ft(ft, rank) VALUES('rank', 'bm25(2.0, 1.0)');",
            query: "SELECT rowid, rank FROM ft WHERE ft MATCH 'x' ORDER BY rowid",
        },
        // Two-column default (no config).
        Case {
            setup: "CREATE VIRTUAL TABLE ft USING fts5(a, b);\
                    INSERT INTO ft VALUES('x y', 'x x'),('x','y y x');",
            query: "SELECT rowid, rank FROM ft WHERE ft MATCH 'x' ORDER BY rowid",
        },
    ];

    for (i, c) in cases.iter().enumerate() {
        let path = tmp(&format!("a{i}"));
        sqlite3(&path, c.setup);
        let want = sqlite3(&path, c.query);
        let g = graphite_mem(c.setup);
        let got = render(&g.query(c.query).unwrap());
        assert_eq!(got, want, "mismatch for case {i} query `{}`", c.query);
        let _ = std::fs::remove_file(&path);
    }
}

/// The config persists across multiple queries in one connection (each query
/// re-reads the `_config` shadow).
#[test]
fn rank_config_persists_across_queries() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE VIRTUAL TABLE ft USING fts5(x);\
                 INSERT INTO ft VALUES('a a b'),('a b b'),('a a a');\
                 INSERT INTO ft(ft, rank) VALUES('rank', 'bm25(5.0)');";
    let path = tmp("persist");
    sqlite3(&path, setup);
    let g = graphite_mem(setup);
    let queries = [
        "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH 'a' ORDER BY rank",
        "SELECT rowid, rank FROM ft WHERE ft MATCH 'b' ORDER BY rowid",
    ];
    for q in queries {
        let want = sqlite3(&path, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "mismatch for `{q}`");
    }
    let _ = std::fs::remove_file(&path);
}

/// On-disk persistence: a graphite-written table with a configured rank reopens
/// with the rank still applied, sqlite reads the same result, and sqlite sees the
/// `_config` `rank` row.
#[test]
fn rank_config_on_disk_and_sqlite_interop() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE VIRTUAL TABLE ft USING fts5(x);\
                 INSERT INTO ft VALUES('a a b'),('a b b');\
                 INSERT INTO ft(ft, rank) VALUES('rank', 'bm25(10.0)');";
    let query = "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rowid";

    // graphite writes the table (with the rank config) to disk.
    let path = tmp("disk");
    {
        let mut g = Connection::create(&path).unwrap();
        for s in setup.split(';') {
            if !s.trim().is_empty() {
                g.execute(s).unwrap();
            }
        }
    }

    // graphite reopens and the rank is still applied.
    let want = {
        // Reference: sqlite computes the same over the graphite-written file.
        sqlite3(&path, query)
    };
    {
        let g = Connection::open(&path).unwrap();
        let got = render(&g.query(query).unwrap());
        assert_eq!(got, want, "graphite reopen mismatch");
    }

    // sqlite reads graphite's `_config` shadow: the `rank` row is present.
    let cfg = sqlite3(&path, "SELECT k, v FROM ft_config WHERE k = 'rank'");
    assert_eq!(
        cfg, "rank|bm25(10.0)",
        "sqlite should see the _config rank row"
    );

    // sqlite's own query over the file must also match.
    let sqlite_result = sqlite3(&path, query);
    assert_eq!(sqlite_result, want);

    // And the file passes sqlite's integrity check.
    let ic = sqlite3(&path, "PRAGMA integrity_check");
    assert_eq!(ic, "ok");
    let _ = std::fs::remove_file(&path);
}

/// A rank value that references a nonexistent function is stored fine (like
/// sqlite) but errors when a query actually evaluates `rank`. A `NULL` value and
/// malformed shapes are rejected at config-set time. All error like sqlite3.
#[test]
fn rank_config_error_parity() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // (setup batch, query-or-empty). Both engines must error at the same step.
    // The command batch is run first; if it succeeds, the query is run.
    let create = "CREATE VIRTUAL TABLE ft USING fts5(x);\
                  INSERT INTO ft VALUES('a a b');";

    // A NULL rank value: rejected at config-set time by both engines.
    {
        let path = tmp("null");
        sqlite3(&path, create);
        let cmd = "INSERT INTO ft(ft, rank) VALUES('rank', NULL)";
        assert!(sqlite3_errs(&path, cmd), "sqlite should reject NULL rank");
        let mut g = graphite_mem(create);
        assert!(g.execute(cmd).is_err(), "graphite should reject NULL rank");
        let _ = std::fs::remove_file(&path);
    }

    // A malformed rank string: rejected at config-set time by both engines.
    for bad in ["bm25(", "bm25", "()", "(1.0)"] {
        let path = tmp("bad");
        sqlite3(&path, create);
        let cmd = format!("INSERT INTO ft(ft, rank) VALUES('rank', '{bad}')");
        assert!(
            sqlite3_errs(&path, &cmd),
            "sqlite should reject rank `{bad}`"
        );
        let mut g = graphite_mem(create);
        assert!(
            g.execute(&cmd).is_err(),
            "graphite should reject rank `{bad}`"
        );
        let _ = std::fs::remove_file(&path);
    }

    // An invalid function name: stored fine, but a query referencing `rank`
    // errors in both engines. graphite reports `no such function: nosuchfunc`.
    {
        let path = tmp("nofunc");
        sqlite3(&path, create);
        let set = "INSERT INTO ft(ft, rank) VALUES('rank', 'nosuchfunc()')";
        assert!(!sqlite3_errs(&path, set), "sqlite stores a bad rank func");
        let mut g = graphite_mem(create);
        g.execute(set)
            .expect("graphite stores a bad rank func without erroring");

        // A query that does NOT reference `rank` still works.
        let no_rank = "SELECT rowid FROM ft WHERE ft MATCH 'a' ORDER BY rowid";
        assert!(!sqlite3_errs(&path, no_rank));
        assert!(g.query(no_rank).is_ok());

        // A query that DOES reference `rank` errors in both engines.
        let with_rank = "SELECT rank FROM ft WHERE ft MATCH 'a'";
        assert!(sqlite3_errs(&path, with_rank), "sqlite errors on bad rank");
        let err = g
            .query(with_rank)
            .expect_err("graphite errors on bad rank")
            .to_string();
        assert!(
            err.contains("no such function: nosuchfunc"),
            "graphite error `{err}` should name the missing function"
        );
        let _ = std::fs::remove_file(&path);
    }
}

/// Gap B: `rank='…'` and any other unrecognized fts5 CREATE option are rejected
/// with sqlite's exact `unrecognized option: "<name>"`; the recognized option
/// keywords still create successfully.
#[test]
fn unrecognized_create_option_rejected() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Unknown options: both engines error, and graphite's message is byte-exact.
    let bad = [
        (
            "rank",
            "CREATE VIRTUAL TABLE ft USING fts5(x, rank='bm25(10.0)')",
        ),
        (
            "foobar",
            "CREATE VIRTUAL TABLE ft USING fts5(x, foobar='y')",
        ),
        ("nope", "CREATE VIRTUAL TABLE ft USING fts5(x, nope=1)"),
    ];
    for (name, ddl) in bad {
        let path = tmp("badopt");
        assert!(
            sqlite3_errs(&path, ddl),
            "sqlite should reject option `{name}`"
        );
        let mut g = Connection::open_memory().unwrap();
        let err = g
            .execute(ddl)
            .expect_err(&format!("graphite should reject option `{name}`"))
            .to_string();
        // graphite's `Display` prepends `error: ` to `SQLITE_ERROR` messages; the
        // inner message is sqlite's exact `unrecognized option: "<name>"`.
        assert_eq!(
            err,
            format!("error: unrecognized option: \"{name}\""),
            "graphite message for `{name}`"
        );
        let _ = std::fs::remove_file(&path);
    }

    // Recognized option keywords still create successfully in both engines.
    let good = [
        "CREATE VIRTUAL TABLE t1 USING fts5(x, prefix='2')",
        "CREATE VIRTUAL TABLE t2 USING fts5(x, tokenize='porter')",
        "CREATE VIRTUAL TABLE t3 USING fts5(x, columnsize='0')",
        "CREATE VIRTUAL TABLE t4 USING fts5(x, detail='none')",
        "CREATE VIRTUAL TABLE t5 USING fts5(x, content_rowid='rowid')",
    ];
    for ddl in good {
        let path = tmp("goodopt");
        assert!(!sqlite3_errs(&path, ddl), "sqlite should accept `{ddl}`");
        let mut g = Connection::open_memory().unwrap();
        g.execute(ddl)
            .unwrap_or_else(|e| panic!("graphite should accept `{ddl}`: {e}"));
        let _ = std::fs::remove_file(&path);
    }

    // `content='<tbl>'` needs the content table to exist in graphite's create
    // path, so exercise it separately (both engines create it).
    {
        let ddl = "CREATE TABLE c(x); CREATE VIRTUAL TABLE t6 USING fts5(x, content='c')";
        let path = tmp("content");
        assert!(!sqlite3_errs(&path, ddl), "sqlite should accept content=");
        let mut g = Connection::open_memory().unwrap();
        for s in ddl.split(';') {
            if !s.trim().is_empty() {
                g.execute(s)
                    .unwrap_or_else(|e| panic!("graphite content= should create: {e}"));
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}
