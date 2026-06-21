//! Read-only getters for tuning PRAGMAs that graphite does not expose as knobs
//! (page cache, durability, lock manager). Each reports SQLite's fixed default —
//! what an unconfigured connection observes — so the shell stays drop-in for
//! tools/ORMs that probe these on connect. Previously these errored with
//! "not yet implemented: this PRAGMA".

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(c: &Connection, sql: &str) -> Value {
    let r = c.query(sql).unwrap();
    assert_eq!(r.rows.len(), 1, "{sql} should return exactly one row");
    r.rows[0][0].clone()
}

#[test]
fn tuning_pragma_getters_report_sqlite_defaults() {
    let c = Connection::open_memory().unwrap();
    // (pragma, expected value as observed from sqlite3 3.50.4 on a fresh db)
    let int_cases: &[(&str, i64)] = &[
        ("cache_size", -2000),
        ("synchronous", 2),
        ("temp_store", 0),
        ("secure_delete", 0),
        ("read_uncommitted", 0),
        ("cell_size_check", 0),
        ("checkpoint_fullfsync", 0),
        ("fullfsync", 0),
        ("busy_timeout", 0),
        ("wal_autocheckpoint", 1000),
        ("max_page_count", 4294967294),
    ];
    for (name, want) in int_cases {
        assert_eq!(
            one(&c, &alloc_pragma(name)),
            Value::Integer(*want),
            "PRAGMA {name}"
        );
    }
    assert_eq!(one(&c, "PRAGMA locking_mode"), Value::Text("normal".into()));
}

#[test]
fn setting_an_unexposed_tuning_pragma_is_accepted_and_ignored() {
    // The setter form must not error: a tool that opens with `PRAGMA cache_size=N`
    // should proceed. graphite has no such knob, so the write is a silent no-op.
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "PRAGMA cache_size=8000",
        "PRAGMA synchronous=OFF",
        "PRAGMA temp_store=MEMORY",
        "PRAGMA busy_timeout=5000",
        "PRAGMA locking_mode=EXCLUSIVE",
        "PRAGMA secure_delete=ON",
    ] {
        c.execute(s)
            .unwrap_or_else(|e| panic!("{s} should be accepted: {e:?}"));
    }
    // The connection is still fully usable afterward.
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    assert_eq!(one(&c, "SELECT a FROM t"), Value::Integer(1));
}

fn alloc_pragma(name: &str) -> String {
    format!("PRAGMA {name}")
}
