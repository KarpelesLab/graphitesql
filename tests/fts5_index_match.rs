//! Roadmap D2b-2: a single bare-term `MATCH` is answered from the FTS5 segment
//! index (term → doclist via `decode_term`) instead of scanning + tokenizing
//! every `_content` document. This is a performance/scale change, not a semantics
//! change, so the rows it returns must be byte-identical to the old document
//! scan. These tests assert that graphite's bare-term `MATCH` returns exactly
//! what stock `sqlite3`'s own `MATCH` returns for the same data, in two
//! directions:
//!
//!  * graphite WRITES the table, then both graphite and sqlite3 run `MATCH`
//!    against the same file (graphite's index-routed read vs sqlite's reader);
//!  * sqlite3 WRITES the table (its own index), then graphite reads it and runs
//!    the same bare-term `MATCH` (graphite decodes sqlite's leaves).
//!
//! Both single-leaf (default `pgsz`) and multi-leaf (forced small `pgsz`) indexes
//! are covered, with single-occurrence, multi-doc, repeated-in-one-doc, and
//! absent terms.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::{Connection, Value};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("gsql-d2b2-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("idx-{}.db", SEQ.fetch_add(1, Ordering::Relaxed)));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// graphite's `SELECT rowid FROM t WHERE t MATCH '<term>' ORDER BY rowid`, as a
/// sorted `,`-joined string of rowids.
fn graphite_match(c: &Connection, term: &str) -> String {
    let sql = format!("SELECT rowid FROM t WHERE t MATCH '{term}' ORDER BY rowid");
    let mut v: Vec<i64> = c
        .query(&sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("non-integer rowid: {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// sqlite3's MATCH over the same file, sorted `,`-joined rowids.
fn sqlite_match(path: &str, term: &str) -> String {
    let q = format!("SELECT rowid FROM t WHERE t MATCH '{term}' ORDER BY rowid;");
    let o = Command::new("sqlite3").arg(path).arg(&q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mut v: Vec<i64> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap())
        .collect();
    v.sort_unstable();
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// sqlite3 helper: run a statement, asserting success.
fn sqlite_exec(path: &str, sql: &str) {
    let o = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {sql:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// The corpus: enough documents that a small `pgsz` produces a multi-leaf index.
/// Each tuple is `(rowid, body)`. "fox" appears once in some docs, several times
/// in one, and in many docs; "zebra" never appears.
const DOCS: &[(i64, &str)] = &[
    (1, "the quick brown fox jumps"),
    (2, "a lazy dog sleeps soundly"),
    (3, "fox fox fox in the henhouse"),
    (4, "nothing relevant in this line"),
    (5, "another fox runs across the field"),
    (6, "the dog chases the fox again"),
    (7, "quick thinking saves the day"),
    (8, "brown bears and brown foxes differ"),
    (9, "the fox is quick and clever"),
    (10, "no animals mentioned at all here"),
];

/// Bare terms exercised: single-occurrence, multi-doc, repeated-in-one-doc, an
/// absent term, and a term in exactly one document.
const TERMS: &[&str] = &["fox", "quick", "dog", "brown", "henhouse", "zebra"];

/// Create the fts5 table + DOCS in graphite at `pgsz` (0 ⇒ default page size).
fn build_graphite(path: &str) {
    let mut c = Connection::create(path).unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    for (rowid, body) in DOCS {
        c.execute(&format!(
            "INSERT INTO t(rowid, body) VALUES({rowid}, '{body}')"
        ))
        .unwrap();
    }
}

#[test]
fn graphite_written_bare_term_match_equals_sqlite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    build_graphite(&path);
    let c = Connection::open(&path).unwrap();
    for term in TERMS {
        let g = graphite_match(&c, term);
        let s = sqlite_match(&path, term);
        assert_eq!(g, s, "term {term:?}: graphite {g:?} != sqlite {s:?}");
    }
    // A table ALIAS in the FROM clause (`t AS x` / `x MATCH …`) is still
    // table-wide and index-routed; results must match sqlite.
    for term in TERMS {
        let sql = format!("SELECT rowid FROM t AS x WHERE x MATCH '{term}' ORDER BY rowid");
        let mut g: Vec<i64> = c
            .query(&sql)
            .unwrap()
            .rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                ref o => panic!("non-integer rowid: {o:?}"),
            })
            .collect();
        g.sort_unstable();
        let g = g
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let s = sqlite_match(&path, term);
        assert_eq!(
            g, s,
            "aliased term {term:?}: graphite {g:?} != sqlite {s:?}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sqlite_written_bare_term_match_read_by_graphite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    // sqlite builds the table and its own index (a possibly multi-leaf segment at
    // the default pgsz).
    sqlite_exec(&path, "CREATE VIRTUAL TABLE t USING fts5(body);");
    for (rowid, body) in DOCS {
        sqlite_exec(
            &path,
            &format!("INSERT INTO t(rowid, body) VALUES({rowid}, '{body}');"),
        );
    }
    let c = Connection::open(&path).unwrap();
    for term in TERMS {
        let g = graphite_match(&c, term);
        let s = sqlite_match(&path, term);
        assert_eq!(
            g, s,
            "sqlite-written, term {term:?}: graphite {g:?} != sqlite {s:?}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Column-scoped single bare term (`tbl MATCH 'col : word'`) — D2b-2 extension.
// A word appears in different columns in different rows; `col MATCH 'word'` must
// return exactly the rows where the word is in THAT column, identical to sqlite3.
// ---------------------------------------------------------------------------

/// A two-column corpus where "fox"/"dog" appear in `title`, `body`, both, or
/// neither across rows, so a column filter genuinely partitions the doclist.
const MC_DOCS: &[(i64, &str, &str)] = &[
    (1, "the fox sleeps", "a lazy dog rests"),
    (2, "quiet night here", "a fox runs by"),
    (3, "fox and hound", "dog chases fox"),
    (4, "fox tracks found", "snow everywhere now"),
    (5, "nothing notable", "nothing here either"),
    (6, "dog day afternoon", "the fox returns home"),
    (7, "brown fox leaps", "brown dog barks loud"),
    (8, "henhouse raided", "by a sneaky fox tonight"),
    (9, "the dog and fox", "play in the yard now"),
    (10, "calm waters flow", "across the wide river"),
];

/// graphite's `SELECT rowid FROM t WHERE t MATCH 'col : term'`, sorted rowids.
fn graphite_col_match(c: &Connection, col: &str, term: &str) -> String {
    let sql = format!("SELECT rowid FROM t WHERE t MATCH '{col} : {term}' ORDER BY rowid");
    let mut v: Vec<i64> = c
        .query(&sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("non-integer rowid: {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// sqlite3's column-scoped MATCH over the same file, sorted rowids.
fn sqlite_col_match(path: &str, col: &str, term: &str) -> String {
    let q = format!("SELECT rowid FROM t WHERE t MATCH '{col} : {term}' ORDER BY rowid;");
    let o = Command::new("sqlite3").arg(path).arg(&q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mut v: Vec<i64> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap())
        .collect();
    v.sort_unstable();
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn graphite_written_column_scoped_match_equals_sqlite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    for (rowid, title, body) in MC_DOCS {
        c.execute(&format!(
            "INSERT INTO t(rowid, title, body) VALUES({rowid}, '{title}', '{body}')"
        ))
        .unwrap();
    }
    drop(c);
    let c = Connection::open(&path).unwrap();
    for col in ["title", "body"] {
        for term in ["fox", "dog", "brown", "henhouse", "zebra"] {
            let g = graphite_col_match(&c, col, term);
            let s = sqlite_col_match(&path, col, term);
            assert_eq!(g, s, "{col}:{term}: graphite {g:?} != sqlite {s:?}");
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sqlite_written_column_scoped_match_read_by_graphite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Single-leaf (default pgsz) and multi-leaf (forced pgsz 64) sqlite-written
    // indexes, both read by graphite's column-scoped index route.
    for pgsz in [0u32, 64] {
        let path = tmp_path();
        if pgsz == 0 {
            sqlite_exec(&path, "CREATE VIRTUAL TABLE t USING fts5(title, body);");
        } else {
            sqlite_exec(
                &path,
                "CREATE VIRTUAL TABLE t USING fts5(title, body);\
                 INSERT INTO t(t, rank) VALUES('pgsz', 64);",
            );
        }
        for (rowid, title, body) in MC_DOCS {
            sqlite_exec(
                &path,
                &format!("INSERT INTO t(rowid, title, body) VALUES({rowid}, '{title}', '{body}');"),
            );
        }
        if pgsz != 0 {
            // Merge to a single segment graphite's reader serves, at pgsz 64.
            sqlite_exec(&path, "INSERT INTO t(t) VALUES('optimize');");
        }
        let c = Connection::open(&path).unwrap();
        for col in ["title", "body"] {
            for term in ["fox", "dog", "brown", "henhouse", "zebra"] {
                let g = graphite_col_match(&c, col, term);
                let s = sqlite_col_match(&path, col, term);
                assert_eq!(
                    g, s,
                    "pgsz {pgsz}, {col}:{term}: graphite {g:?} != sqlite {s:?}"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn sqlite_written_multileaf_bare_term_match_read_by_graphite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    // Force a tiny page size so the single segment spans MANY leaves (term
    // pagination + doclist spanning) — exactly the decoder's multi-leaf path.
    sqlite_exec(
        &path,
        "CREATE VIRTUAL TABLE t USING fts5(body);\
         INSERT INTO t(t, rank) VALUES('pgsz', 64);",
    );
    // A larger corpus so 64-byte pages genuinely split.
    for i in 1..=60i64 {
        let body = if i % 3 == 0 {
            format!("fox number {i} runs fast")
        } else if i % 5 == 0 {
            format!("the lazy dog {i} sleeps")
        } else {
            format!("filler word{i:03} content here")
        };
        sqlite_exec(
            &path,
            &format!("INSERT INTO t(rowid, body) VALUES({i}, '{body}');"),
        );
    }
    // Optimize so sqlite merges into a single segment graphite's reader serves
    // (and rebuilds the byte layout at pgsz=64).
    sqlite_exec(&path, "INSERT INTO t(t) VALUES('optimize');");

    let c = Connection::open(&path).unwrap();
    for term in ["fox", "dog", "runs", "filler", "missing"] {
        let g = graphite_match(&c, term);
        let s = sqlite_match(&path, term);
        assert_eq!(
            g, s,
            "sqlite multileaf, term {term:?}: graphite {g:?} != sqlite {s:?}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Two-term phrase (`tbl MATCH '"a b"'`) — D2b extension. A phrase matches a
// document iff its two tokens occur at ADJACENT positions in the same column.
// graphite now answers this from the segment index (intersect the two terms'
// doclists, keep docs with adjacent per-column positions). The routed rows must
// be byte-identical to stock sqlite3's MATCH on the same file.
// ---------------------------------------------------------------------------

/// A single-column corpus where "quick brown" appears adjacent in some rows, the
/// same words non-adjacent / reversed in others, and a repeated-word phrase
/// ("very very") and a never-adjacent pair exercise the edges.
const PH_DOCS: &[(i64, &str)] = &[
    (1, "the quick brown fox jumps"),         // quick brown adjacent
    (2, "a quick red brown hare runs"),       // quick … brown NOT adjacent
    (3, "brown then quick reversed order"),   // brown quick, not quick brown
    (4, "nothing relevant in this row"),      // neither word
    (5, "quick brown quick brown again"),     // adjacent twice
    (6, "only quick here no other color"),    // quick only
    (7, "only brown here no speed at all"),   // brown only
    (8, "very very enthusiastic about cats"), // "very very" adjacent
    (9, "very calm and very collected mind"), // "very … very" not adjacent
    (10, "the brown quick fox is back"),      // brown quick (reversed) again
];

/// graphite's `SELECT rowid FROM t WHERE t MATCH '"<a> <b>"'`, sorted rowids.
fn graphite_phrase(c: &Connection, a: &str, b: &str) -> String {
    let sql = format!("SELECT rowid FROM t WHERE t MATCH '\"{a} {b}\"' ORDER BY rowid");
    let mut v: Vec<i64> = c
        .query(&sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("non-integer rowid: {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// sqlite3's phrase MATCH over the same file, sorted rowids.
fn sqlite_phrase(path: &str, a: &str, b: &str) -> String {
    let q = format!("SELECT rowid FROM t WHERE t MATCH '\"{a} {b}\"' ORDER BY rowid;");
    let o = Command::new("sqlite3").arg(path).arg(&q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mut v: Vec<i64> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap())
        .collect();
    v.sort_unstable();
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The phrase pairs exercised: adjacent, reversed, repeated word, and a pair that
/// is never adjacent anywhere.
const PHRASES: &[(&str, &str)] = &[
    ("quick", "brown"),
    ("brown", "quick"),
    ("very", "very"),
    ("brown", "fox"),
    ("fox", "brown"),
    ("zebra", "stripes"), // neither word present
];

#[test]
fn graphite_written_two_term_phrase_equals_sqlite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    for (rowid, body) in PH_DOCS {
        c.execute(&format!(
            "INSERT INTO t(rowid, body) VALUES({rowid}, '{body}')"
        ))
        .unwrap();
    }
    drop(c);
    let c = Connection::open(&path).unwrap();
    for (a, b) in PHRASES {
        let g = graphite_phrase(&c, a, b);
        let s = sqlite_phrase(&path, a, b);
        assert_eq!(g, s, "phrase \"{a} {b}\": graphite {g:?} != sqlite {s:?}");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sqlite_written_two_term_phrase_read_by_graphite_single_and_multi_leaf() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Single-leaf (default pgsz) and multi-leaf (forced pgsz 64, then optimized to
    // a single segment) sqlite-written indexes, both read by graphite's phrase route.
    for pgsz in [0u32, 64] {
        let path = tmp_path();
        if pgsz == 0 {
            sqlite_exec(&path, "CREATE VIRTUAL TABLE t USING fts5(body);");
        } else {
            sqlite_exec(
                &path,
                "CREATE VIRTUAL TABLE t USING fts5(body);\
                 INSERT INTO t(t, rank) VALUES('pgsz', 64);",
            );
        }
        // A larger corpus so 64-byte pages genuinely split the doclists.
        for i in 1..=60i64 {
            let body = match i % 4 {
                0 => format!("row {i} has quick brown adjacency here"),
                1 => format!("row {i} keeps quick apart from brown words"),
                2 => format!("row {i} only mentions brown not the speed"),
                _ => format!("filler word{i:03} unrelated tokens around"),
            };
            sqlite_exec(
                &path,
                &format!("INSERT INTO t(rowid, body) VALUES({i}, '{body}');"),
            );
        }
        if pgsz != 0 {
            sqlite_exec(&path, "INSERT INTO t(t) VALUES('optimize');");
        }
        let c = Connection::open(&path).unwrap();
        for (a, b) in [("quick", "brown"), ("brown", "quick"), ("quick", "apart")] {
            let g = graphite_phrase(&c, a, b);
            let s = sqlite_phrase(&path, a, b);
            assert_eq!(
                g, s,
                "pgsz {pgsz}, phrase \"{a} {b}\": graphite {g:?} != sqlite {s:?}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn column_scoped_two_term_phrase_equals_sqlite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    // Two columns: "quick brown" is adjacent in title for some rows, in body for
    // others, split across columns in one (must NOT match a single-column phrase).
    let docs: &[(i64, &str, &str)] = &[
        (1, "the quick brown fox", "calm waters here"), // title
        (2, "plain title only", "a quick brown hare runs"), // body
        (3, "quick brown both ways", "quick brown twice"), // both
        (4, "quick alone in title", "brown alone in body"), // split → no match
        (5, "nothing notable now", "nothing here either"), // neither
    ];
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    for (rowid, title, body) in docs {
        c.execute(&format!(
            "INSERT INTO t(rowid, title, body) VALUES({rowid}, '{title}', '{body}')"
        ))
        .unwrap();
    }
    drop(c);
    let c = Connection::open(&path).unwrap();
    for col in ["title", "body"] {
        // graphite column-scoped phrase.
        let sql =
            format!("SELECT rowid FROM t WHERE t MATCH '{col} : \"quick brown\"' ORDER BY rowid");
        let mut g: Vec<i64> = c
            .query(&sql)
            .unwrap()
            .rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                ref o => panic!("non-integer rowid: {o:?}"),
            })
            .collect();
        g.sort_unstable();
        let g = g
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // sqlite column-scoped phrase.
        let q =
            format!("SELECT rowid FROM t WHERE t MATCH '{col} : \"quick brown\"' ORDER BY rowid;");
        let o = Command::new("sqlite3").arg(&path).arg(&q).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let mut sv: Vec<i64> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.parse().unwrap())
            .collect();
        sv.sort_unstable();
        let s = sv
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            g, s,
            "{col}:\"quick brown\": graphite {g:?} != sqlite {s:?}"
        );
    }
    let _ = std::fs::remove_file(&path);
}
