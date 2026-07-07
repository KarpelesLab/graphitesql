//! The FTS5 `bm25()` / `rank` *value* is byte-exact with stock sqlite3.
//!
//! SQLite's Okapi BM25 corpus statistics span the WHOLE table: `avgdl` is the
//! total token count divided by the total row count, and each phrase's IDF uses
//! `nHit` = the number of rows in the table containing the phrase. graphite
//! scores `MATCH` queries from the rows the scan returns — which, because the
//! `MATCH` predicate is pushed down, is only the *matched* subset. Computing
//! `avgdl`/`nHit` over that subset produced scores that ranked correctly but were
//! numerically wrong (e.g. the two-column `fox` corpus below returned
//! `-9.56521739130435e-07` instead of sqlite's `-9.07216494845361e-07`). This
//! test drives both the `graphitesql` shell and stock `sqlite3` and asserts the
//! rendered `bm25()`/`rank` strings match character-for-character across a range
//! of corpora (doc count, doc length, term rarity, single/multi column, the
//! weighted `bm25(t, w0, …)` form, and `ORDER BY rank LIMIT k`).

#![cfg(all(feature = "std", feature = "fts5"))]

use std::process::Command;

/// Run a (possibly multi-statement) script through the `graphitesql` shell
/// against an in-memory database and return its raw stdout.
fn graphite(script: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_graphitesql"))
        .arg(":memory:")
        .arg(script)
        .output()
        .expect("run graphitesql shell");
    assert!(
        out.status.success(),
        "graphitesql failed for {script:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run the same script through stock `sqlite3`; `None` if the CLI is absent.
fn sqlite(script: &str) -> Option<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(script)
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "sqlite3 failed for {script:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Assert graphite's rendered output for `script` is byte-for-byte sqlite's.
/// Skips (with a note) when `sqlite3` is not installed.
fn assert_matches_sqlite(script: &str) {
    let Some(want) = sqlite(script) else {
        eprintln!("sqlite3 not found; skipping bm25-value check");
        return;
    };
    let got = graphite(script);
    assert_eq!(got, want, "bm25 value diverged for script:\n{script}");
}

/// The exact corpus from the bug report — two columns, the common term `fox`
/// (idf clamped to 1e-6), three rows of which two match. The pre-fix values were
/// `-9.56521739130435e-07` / `-1.04761904761905e-06`; sqlite returns
/// `-9.07216494845361e-07` / `-1.0e-06`. The divergence came from `avgdl` being
/// computed over the 2 matched rows (avgdl 4.5) instead of all 3 rows (avgdl 4).
#[test]
fn bug_corpus_two_column_common_term() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(a,b);\n\
         INSERT INTO ft VALUES('the quick brown fox','jumps'),('lazy dog','sleeps'),('quick red fox','runs');\n\
         SELECT rowid, bm25(ft) FROM ft WHERE ft MATCH 'fox' ORDER BY rowid;",
    );
}

/// A single-document table already matched pre-fix (there is no "unmatched" row
/// to change `avgdl`); assert it still does.
#[test]
fn single_document_regression_guard() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(x);\n\
         INSERT INTO ft VALUES('a b c a');\n\
         SELECT bm25(ft) FROM ft WHERE ft MATCH 'a';",
    );
}

/// Two documents, only one matches — `avgdl` must still span both rows.
#[test]
fn two_documents_one_matches() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(x);\n\
         INSERT INTO ft VALUES('a b'),('a c');\n\
         SELECT rowid, bm25(ft) FROM ft WHERE ft MATCH 'b' ORDER BY rowid;",
    );
}

/// A rare term (real, un-clamped IDF) in a single-column many-document corpus.
#[test]
fn many_documents_rare_term() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(x);\n\
         INSERT INTO ft VALUES('apple banana'),('apple apple cherry'),('banana'),('date apple'),('cherry cherry cherry apple');\n\
         SELECT rowid, bm25(ft) FROM ft WHERE ft MATCH 'date' ORDER BY rowid;",
    );
}

/// A common single-column term (idf clamped) across many varied-length docs.
#[test]
fn many_documents_common_term() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(x);\n\
         INSERT INTO ft VALUES('apple banana'),('apple apple cherry'),('banana'),('date apple'),('cherry cherry cherry apple');\n\
         SELECT rowid, bm25(ft) FROM ft WHERE ft MATCH 'apple' ORDER BY rowid;",
    );
}

/// The weighted `bm25(t, w0, w1)` form — per-column weights applied over the
/// full-table `avgdl`.
#[test]
fn weighted_two_column() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(a,b);\n\
         INSERT INTO ft VALUES('the quick brown fox','jumps'),('lazy dog','sleeps'),('quick red fox','runs');\n\
         SELECT rowid, bm25(ft, 2.0, 5.0) FROM ft WHERE ft MATCH 'fox' ORDER BY rowid;",
    );
}

/// `ORDER BY rank LIMIT k` returning the `rank` value itself.
#[test]
fn order_by_rank_limit_returns_rank() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(a,b);\n\
         INSERT INTO ft VALUES('the quick brown fox','jumps'),('lazy dog','sleeps'),('quick red fox','runs');\n\
         SELECT rowid, rank FROM ft WHERE ft MATCH 'fox' ORDER BY rank LIMIT 1;",
    );
}

/// A multi-term `OR` query — every phrase contributes its own full-table IDF.
#[test]
fn or_query_multiple_phrases() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(a,b);\n\
         INSERT INTO ft VALUES('the quick brown fox','jumps'),('lazy dog','sleeps'),('quick red fox','runs');\n\
         SELECT rowid, bm25(ft) FROM ft WHERE ft MATCH 'fox OR dog' ORDER BY rowid;",
    );
}

/// A weighted `OR` query ordered by `rank` — combines per-column weights, a
/// multi-phrase full-table IDF, and the ranking sort.
#[test]
fn weighted_or_ordered_by_rank() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(a,b);\n\
         INSERT INTO ft VALUES('the quick brown fox','jumps'),('lazy dog','sleeps'),('quick red fox','runs');\n\
         SELECT rowid, bm25(ft, 0.5, 3.0) FROM ft WHERE ft MATCH 'quick OR jumps' ORDER BY rank;",
    );
}

/// An `UNINDEXED` column: its tokens count toward neither the document length
/// nor `avgdl`, so the score depends only on the indexed columns.
#[test]
fn unindexed_column_excluded_from_length() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(a,b UNINDEXED);\n\
         INSERT INTO ft VALUES('the quick brown fox','jumps here now for a while'),('lazy dog','sleeps'),('quick red fox','runs');\n\
         SELECT rowid, bm25(ft) FROM ft WHERE ft MATCH 'fox' ORDER BY rowid;",
    );
}

/// A larger single-column corpus — more rows, a term hitting a mid subset.
#[test]
fn eight_document_corpus() {
    assert_matches_sqlite(
        "CREATE VIRTUAL TABLE ft USING fts5(t);\n\
         INSERT INTO ft VALUES('one two three'),('two three four'),('three four five'),('four five six'),('five six seven'),('six seven eight'),('seven eight nine'),('eight nine ten');\n\
         SELECT rowid, bm25(ft) FROM ft WHERE ft MATCH 'four' ORDER BY rowid;",
    );
}
