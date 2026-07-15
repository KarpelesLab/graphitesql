//! Roadmap D2e — FTS5 `detail='none'` / `detail='column'` write path. A table
//! declared with a non-default `detail=` mode writes a differently-encoded segment
//! doclist: `detail=none` stores bare delta rowids with NO position list at all,
//! and `detail=column` stores only which columns a term occurs in (a delta-coded
//! `(col+2)` marker list, no in-column offsets). graphite used to ignore the mode
//! and always emit a full-detail poslist, which stock sqlite3 3.50.4 rejected as a
//! `malformed inverted index`.
//!
//! This test drives the SAME insert (and delete) sequences through graphite and
//! stock `sqlite3` (3.50.4, FTS5) and asserts:
//!   * the raw `%_data` / `%_idx` / `%_docsize` bytes are byte-identical (for the
//!     pure-insert shapes: single, multi-row, multiple segments, and a ≥16-batch
//!     crisis merge),
//!   * `PRAGMA quick_check` on graphite's file returns `ok`,
//!   * graphite's own `PRAGMA integrity_check` returns `ok`,
//!   * `MATCH` returns the same rows (respecting each mode's query limits).
//!
//! A DELETE/UPDATE on a `detail=none`/`detail=column` table appends a
//! detail-aware incremental TOMBSTONE (delete-marker) segment — byte-identical to
//! sqlite's `fts5FlushOneHash` delete path — so the delete/update shapes are
//! asserted for byte-parity as well (delete-one/many/all, update-one/many,
//! delete-then-reinsert, multi-column, second-segment, and post-crisis-merge).
//! Skipped when `sqlite3` w/ FTS5 is not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::{Connection, Value};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-detail-{}-{}-{}.db",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

/// `sqlite3` with FTS5 available on PATH?
fn have_fts5_sqlite() -> bool {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg("CREATE VIRTUAL TABLE t USING fts5(a); SELECT 1;")
        .output();
    matches!(o, Ok(o) if o.status.success())
}

/// Run `q` through stock sqlite3 on `path`; assert success; return trimmed stdout.
fn sqlite_raw(path: &str, q: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// Dump the three shadow tables as byte-comparable text.
fn dump_shadows_sqlite(path: &str) -> String {
    let data = sqlite_raw(path, "SELECT id, quote(block) FROM f_data ORDER BY id;");
    let idx = sqlite_raw(
        path,
        "SELECT segid, quote(term), pgno FROM f_idx ORDER BY segid, term, pgno;",
    );
    let ds = sqlite_raw(path, "SELECT id, quote(sz) FROM f_docsize ORDER BY id;");
    format!("DATA\n{data}\nIDX\n{idx}\nDOCSIZE\n{ds}")
}

fn fmt_rows(r: graphitesql::QueryResult) -> String {
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Integer(i) => i.to_string(),
                    Value::Text(t) => String::from(t.as_str()),
                    Value::Null => String::new(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn dump_shadows_graphite(c: &Connection) -> String {
    let data = fmt_rows(
        c.query("SELECT id, quote(block) FROM f_data ORDER BY id")
            .unwrap(),
    );
    let idx = fmt_rows(
        c.query("SELECT segid, quote(term), pgno FROM f_idx ORDER BY segid, term, pgno")
            .unwrap(),
    );
    let ds = fmt_rows(
        c.query("SELECT id, quote(sz) FROM f_docsize ORDER BY id")
            .unwrap(),
    );
    format!("DATA\n{data}\nIDX\n{idx}\nDOCSIZE\n{ds}")
}

/// `PRAGMA quick_check` on `path` via stock sqlite3 must be exactly `ok` (a
/// malformed FTS5 index reports `malformed inverted index for FTS5 table …`).
fn sqlite_quick_check_ok(path: &str) -> bool {
    sqlite_raw(path, "PRAGMA quick_check;") == "ok"
}

/// graphite's own integrity check must be `ok`.
fn graphite_integrity_ok(c: &Connection) -> bool {
    c.query("PRAGMA integrity_check")
        .map(|r| fmt_rows(r) == "ok")
        .unwrap_or(false)
}

fn graphite_match_rowids(c: &Connection, query: &str) -> Vec<i64> {
    c.query(&format!(
        "SELECT rowid FROM f WHERE f MATCH '{query}' ORDER BY rowid"
    ))
    .unwrap()
    .rows
    .iter()
    .map(|r| match &r[0] {
        Value::Integer(i) => *i,
        _ => -1,
    })
    .collect()
}

fn sqlite_match_rowids(path: &str, query: &str) -> Vec<i64> {
    sqlite_raw(
        path,
        &format!("SELECT rowid FROM f WHERE f MATCH '{query}' ORDER BY rowid;"),
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(|l| l.parse().unwrap())
    .collect()
}

/// Apply the SAME `create` + per-statement (autocommit) `inserts` to graphite and
/// sqlite; assert byte-identical shadow tables, `quick_check`=ok, graphite
/// integrity=ok, and matching rows for every query in `matches`.
fn assert_identical(tag: &str, create: &str, inserts: &[String], matches: &[&str]) {
    let g = tmp_path(&format!("{tag}-g"));
    let s = tmp_path(&format!("{tag}-s"));

    let mut c = Connection::create(&g).unwrap();
    c.execute(create).unwrap();
    for ins in inserts {
        c.execute(ins).unwrap(); // each autocommit → its own segment
    }
    sqlite_raw(&s, &format!("{create};"));
    for ins in inserts {
        sqlite_raw(&s, &format!("{ins};"));
    }

    assert_eq!(
        dump_shadows_graphite(&c),
        dump_shadows_sqlite(&s),
        "shadow-table bytes diverge for {tag}"
    );
    assert!(
        sqlite_quick_check_ok(&g),
        "sqlite quick_check rejected graphite's file for {tag}"
    );
    assert!(
        graphite_integrity_ok(&c),
        "graphite integrity_check not ok for {tag}"
    );
    for q in matches {
        assert_eq!(
            graphite_match_rowids(&c, q),
            sqlite_match_rowids(&s, q),
            "MATCH {q:?} rows diverge for {tag}"
        );
    }
}

// ---------------------------------------------------------------------------
// detail=none
// ---------------------------------------------------------------------------

#[test]
fn detail_none_single_insert_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-single",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &[String::from(
            "INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo hello'),(3,'world foo')",
        )],
        &["hello", "foo", "world"],
    );
}

#[test]
fn detail_none_multi_column_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-multicol",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, c, detail='none')",
        &[String::from(
            "INSERT INTO f(rowid,a,b,c) VALUES\
             (1,'hello world','foo bar','x y z'),\
             (2,'a b c','d e','f g'),\
             (3,'quick brown','fox jumps','over lazy')",
        )],
        &["hello", "fox", "z", "a"],
    );
}

#[test]
fn detail_none_multiple_segments_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    let inserts: Vec<String> = (1..=5)
        .map(|i| format!("INSERT INTO f(rowid,a) VALUES({i},'word{i} shared alpha')"))
        .collect();
    assert_identical(
        "none-multiseg",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &inserts,
        &["shared", "word3", "alpha"],
    );
}

#[test]
fn detail_none_crisis_merge_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    // 20 autocommit batches → 20 level-0 segments → a crisis merge at 16.
    let inserts: Vec<String> = (1..=20)
        .map(|i| {
            format!(
                "INSERT INTO f(rowid,a) VALUES({i},'word{i} shared beta gamma{}')",
                i % 5
            )
        })
        .collect();
    assert_identical(
        "none-crisis",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &inserts,
        &["shared", "word17", "gamma0"],
    );
}

#[test]
fn detail_none_delete_is_valid_and_correct() {
    // A delete on a detail=none table now appends a byte-identical tombstone
    // segment (see the byte-parity tests below); this one keeps the historical
    // validity+correctness assertions.
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path("none-del-g");
    let s = tmp_path("none-del-s");
    let mut c = Connection::create(&g).unwrap();
    c.execute("CREATE VIRTUAL TABLE f USING fts5(a, detail='none')")
        .unwrap();
    c.execute("INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo hello'),(3,'world foo')")
        .unwrap();
    c.execute("DELETE FROM f WHERE rowid=2").unwrap();

    sqlite_raw(&s, "CREATE VIRTUAL TABLE f USING fts5(a, detail='none');");
    sqlite_raw(
        &s,
        "INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo hello'),(3,'world foo');",
    );
    sqlite_raw(&s, "DELETE FROM f WHERE rowid=2;");

    assert!(sqlite_quick_check_ok(&g), "quick_check after delete");
    assert!(graphite_integrity_ok(&c), "graphite integrity after delete");
    for q in ["hello", "foo", "world"] {
        assert_eq!(
            graphite_match_rowids(&c, q),
            sqlite_match_rowids(&s, q),
            "MATCH {q:?} after delete"
        );
    }
}

// ---------------------------------------------------------------------------
// detail=none / detail=column DELETE + UPDATE byte-parity
//
// A DELETE/UPDATE on a detail=none/column table appends an incremental
// detail-aware TOMBSTONE (delete-marker) segment — byte-identical to sqlite's
// `fts5FlushOneHash` delete path — rather than rebuilding the index. The
// delete-marker encoding is detail-aware: detail=none writes a positionless
// `0x00` delete marker (size2=1 collapsed), detail=column writes the
// column-marker poslist with the delete flag.
// ---------------------------------------------------------------------------

#[test]
fn detail_none_delete_one_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-del-one",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &[
            String::from(
                "INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo hello'),(3,'world foo')",
            ),
            String::from("DELETE FROM f WHERE rowid=2"),
        ],
        &["hello", "foo", "world"],
    );
}

#[test]
fn detail_none_delete_many_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-del-many",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &[
            String::from(
                "INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo hello'),(3,'world foo'),(4,'a b c')",
            ),
            String::from("DELETE FROM f WHERE rowid IN (2,4)"),
        ],
        &["hello", "foo", "world", "a"],
    );
}

#[test]
fn detail_none_update_one_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-upd-one",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &[
            String::from("INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo bar')"),
            String::from("UPDATE f SET a='changed text' WHERE rowid=1"),
        ],
        &["hello", "changed", "text", "foo"],
    );
}

#[test]
fn detail_none_update_many_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-upd-many",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &[
            String::from(
                "INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo bar'),(3,'baz qux')",
            ),
            String::from("UPDATE f SET a='new one' WHERE rowid=1"),
            String::from("UPDATE f SET a='new three' WHERE rowid=3"),
        ],
        &["hello", "new", "one", "three", "foo"],
    );
}

#[test]
fn detail_none_delete_then_reinsert_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-del-reins",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &[
            String::from("INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo hello')"),
            String::from("DELETE FROM f WHERE rowid=2"),
            String::from("INSERT INTO f(rowid,a) VALUES(2,'new text')"),
        ],
        &["hello", "new", "foo", "world"],
    );
}

#[test]
fn detail_none_delete_all_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-del-all",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &[
            String::from("INSERT INTO f(rowid,a) VALUES(1,'hello world'),(2,'foo hello')"),
            String::from("DELETE FROM f"),
        ],
        &["hello", "foo"],
    );
}

#[test]
fn detail_none_delete_crosses_second_segment_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    // Three autocommit inserts → three level-0 segments; deleting a middle rowid
    // appends a tombstone segment (the term `shared` spans all three).
    assert_identical(
        "none-multiseg-del",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &[
            String::from("INSERT INTO f(rowid,a) VALUES(1,'word1 shared alpha')"),
            String::from("INSERT INTO f(rowid,a) VALUES(2,'word2 shared alpha')"),
            String::from("INSERT INTO f(rowid,a) VALUES(3,'word3 shared alpha')"),
            String::from("DELETE FROM f WHERE rowid=2"),
        ],
        &["shared", "alpha", "word2", "word3"],
    );
}

#[test]
fn detail_none_crisis_then_delete_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    // 20 level-0 segments → a crisis merge, then a delete/update over the merged
    // structure — exercises the tombstone-preserving merge readers.
    let mut inserts: Vec<String> = (1..=20)
        .map(|i| {
            format!(
                "INSERT INTO f(rowid,a) VALUES({i},'word{i} shared beta gamma{}')",
                i % 5
            )
        })
        .collect();
    inserts.push(String::from("DELETE FROM f WHERE rowid IN (3,7,15,19)"));
    inserts.push(String::from(
        "UPDATE f SET a='replaced totally new' WHERE rowid=10",
    ));
    assert_identical(
        "none-crisis-del",
        "CREATE VIRTUAL TABLE f USING fts5(a, detail='none')",
        &inserts,
        &["shared", "gamma0", "replaced", "word10", "word3"],
    );
}

#[test]
fn detail_none_multi_column_delete_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "none-multicol-del",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, c, detail='none')",
        &[
            String::from(
                "INSERT INTO f(rowid,a,b,c) VALUES\
                 (1,'hello world','foo bar','x y z'),\
                 (2,'a b c','d e','f g'),\
                 (3,'quick brown','fox jumps','over lazy')",
            ),
            String::from("DELETE FROM f WHERE rowid=2"),
            String::from("UPDATE f SET b='new middle' WHERE rowid=1"),
        ],
        &["hello", "fox", "z", "new", "middle"],
    );
}

// ---------------------------------------------------------------------------
// detail=column
// ---------------------------------------------------------------------------

#[test]
fn detail_column_single_insert_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-single",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &[String::from(
            "INSERT INTO f(rowid,a,b) VALUES(1,'hello','world'),(2,'foo','hello world')",
        )],
        &["hello", "foo", "world", "a:hello", "b:hello"],
    );
}

#[test]
fn detail_column_multi_column_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-multicol",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, c, detail='column')",
        &[String::from(
            "INSERT INTO f(rowid,a,b,c) VALUES\
             (1,'red green','blue red','green blue'),\
             (2,'yellow','red','green'),\
             (3,'blue','green yellow','red')",
        )],
        &["red", "green", "blue", "a:red", "b:red", "c:green"],
    );
}

#[test]
fn detail_column_multiple_segments_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    let inserts: Vec<String> = (1..=5)
        .map(|i| format!("INSERT INTO f(rowid,a,b) VALUES({i},'word{i} shared','common tail{i}')"))
        .collect();
    assert_identical(
        "col-multiseg",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &inserts,
        &["shared", "common", "word4", "a:shared", "b:common"],
    );
}

#[test]
fn detail_column_crisis_merge_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    let inserts: Vec<String> = (1..=20)
        .map(|i| {
            format!(
                "INSERT INTO f(rowid,a,b) VALUES({i},'word{i} shared','common g{}')",
                i % 4
            )
        })
        .collect();
    assert_identical(
        "col-crisis",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &inserts,
        &["shared", "common", "word11", "a:word11", "b:common"],
    );
}

#[test]
fn detail_column_delete_is_valid_and_correct() {
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path("col-del-g");
    let s = tmp_path("col-del-s");
    let mut c = Connection::create(&g).unwrap();
    c.execute("CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')")
        .unwrap();
    c.execute("INSERT INTO f(rowid,a,b) VALUES(1,'hello','world'),(2,'foo','hello world'),(3,'bar','baz')")
        .unwrap();
    c.execute("DELETE FROM f WHERE rowid=2").unwrap();

    sqlite_raw(
        &s,
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column');",
    );
    sqlite_raw(
        &s,
        "INSERT INTO f(rowid,a,b) VALUES(1,'hello','world'),(2,'foo','hello world'),(3,'bar','baz');",
    );
    sqlite_raw(&s, "DELETE FROM f WHERE rowid=2;");

    assert!(sqlite_quick_check_ok(&g), "quick_check after delete");
    assert!(graphite_integrity_ok(&c), "graphite integrity after delete");
    for q in ["hello", "world", "bar", "b:hello"] {
        assert_eq!(
            graphite_match_rowids(&c, q),
            sqlite_match_rowids(&s, q),
            "MATCH {q:?} after delete"
        );
    }
}

#[test]
fn detail_column_delete_one_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-del-one",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &[
            String::from(
                "INSERT INTO f(rowid,a,b) VALUES(1,'hello','world'),(2,'foo','hello world'),(3,'bar','baz')",
            ),
            String::from("DELETE FROM f WHERE rowid=2"),
        ],
        &["hello", "world", "bar", "b:hello", "a:foo"],
    );
}

#[test]
fn detail_column_update_one_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-upd-one",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &[
            String::from(
                "INSERT INTO f(rowid,a,b) VALUES(1,'hello world','x y'),(2,'foo bar','p q')",
            ),
            String::from("UPDATE f SET a='changed text' WHERE rowid=1"),
        ],
        &["hello", "changed", "a:changed", "b:x", "foo"],
    );
}

#[test]
fn detail_column_update_both_columns_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-upd-both",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &[
            String::from(
                "INSERT INTO f(rowid,a,b) VALUES(1,'hello world','x y'),(2,'foo bar','p q')",
            ),
            String::from("UPDATE f SET a='new one', b='new two' WHERE rowid=1"),
        ],
        &["hello", "new", "a:new", "b:new", "a:one", "b:two"],
    );
}

#[test]
fn detail_column_delete_many_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-del-many",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &[
            String::from(
                "INSERT INTO f(rowid,a,b) VALUES(1,'hello','world'),(2,'foo','hello'),(3,'world','foo'),(4,'a','b')",
            ),
            String::from("DELETE FROM f WHERE rowid IN (2,4)"),
        ],
        &["hello", "world", "foo", "a:hello", "b:foo"],
    );
}

#[test]
fn detail_column_delete_then_reinsert_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-del-reins",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &[
            String::from("INSERT INTO f(rowid,a,b) VALUES(1,'hello','world'),(2,'foo','baz')"),
            String::from("DELETE FROM f WHERE rowid=2"),
            String::from("INSERT INTO f(rowid,a,b) VALUES(2,'new','text')"),
        ],
        &["hello", "new", "b:text", "a:new", "world"],
    );
}

#[test]
fn detail_column_delete_all_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-del-all",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &[
            String::from("INSERT INTO f(rowid,a,b) VALUES(1,'hello','world'),(2,'foo','baz')"),
            String::from("DELETE FROM f"),
        ],
        &["hello", "world", "foo"],
    );
}

#[test]
fn detail_column_delete_crosses_second_segment_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_identical(
        "col-multiseg-del",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &[
            String::from("INSERT INTO f(rowid,a,b) VALUES(1,'word1 shared','common tail1')"),
            String::from("INSERT INTO f(rowid,a,b) VALUES(2,'word2 shared','common tail2')"),
            String::from("INSERT INTO f(rowid,a,b) VALUES(3,'word3 shared','common tail3')"),
            String::from("DELETE FROM f WHERE rowid=2"),
        ],
        &["shared", "common", "word3", "a:shared", "b:common"],
    );
}

#[test]
fn detail_column_crisis_then_delete_update_byte_identical() {
    if !have_fts5_sqlite() {
        return;
    }
    let mut inserts: Vec<String> = (1..=20)
        .map(|i| {
            format!(
                "INSERT INTO f(rowid,a,b) VALUES({i},'word{i} shared','common g{}')",
                i % 4
            )
        })
        .collect();
    inserts.push(String::from("DELETE FROM f WHERE rowid IN (3,7,15,19)"));
    inserts.push(String::from(
        "UPDATE f SET a='mutated word', b='other col' WHERE rowid=10",
    ));
    assert_identical(
        "col-crisis-del",
        "CREATE VIRTUAL TABLE f USING fts5(a, b, detail='column')",
        &inserts,
        &[
            "shared",
            "common",
            "mutated",
            "a:mutated",
            "b:other",
            "word3",
        ],
    );
}
