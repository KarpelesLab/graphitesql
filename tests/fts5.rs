//! Roadmap D2: the built-in `fts5` virtual-table module. This first slice is the
//! *document store* — `CREATE VIRTUAL TABLE … USING fts5(col, …)` declares the
//! text columns and documents round-trip through the persistent `<name>_data`
//! backing table. The tokenizer and `MATCH` querying build on top of this.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn stores_and_retrieves_documents() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    c.execute(
        "INSERT INTO t(title, body) VALUES \
         ('Hello','the quick brown fox'),('Bye','the lazy dog')",
    )
    .unwrap();
    // Documents come back in rowid order, with an implicit 1-based rowid.
    assert_eq!(
        rows(&c, "SELECT rowid, title, body FROM t ORDER BY rowid"),
        [
            vec![
                Value::Integer(1),
                text("Hello"),
                text("the quick brown fox")
            ],
            vec![Value::Integer(2), text("Bye"), text("the lazy dog")],
        ]
    );
    // `*` expands to the declared columns only (no hidden rowid).
    assert_eq!(
        rows(&c, "SELECT * FROM t WHERE rowid = 1"),
        [vec![text("Hello"), text("the quick brown fox")]]
    );
}

#[test]
fn insert_without_a_column_list() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('x','y'), ('p','q')")
        .unwrap();
    assert_eq!(
        rows(&c, "SELECT a, b FROM t ORDER BY rowid"),
        [vec![text("x"), text("y")], vec![text("p"), text("q")],]
    );
}

#[test]
fn update_and_delete_documents() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('one'),('two'),('three')")
        .unwrap();
    c.execute("UPDATE t SET body = 'TWO' WHERE rowid = 2")
        .unwrap();
    c.execute("DELETE FROM t WHERE rowid = 1").unwrap();
    assert_eq!(
        rows(&c, "SELECT rowid, body FROM t ORDER BY rowid"),
        [
            vec![Value::Integer(2), text("TWO")],
            vec![Value::Integer(3), text("three")],
        ]
    );
}

#[test]
fn table_info_columns_are_untyped() {
    // FTS5 declares its columns with no type — PRAGMA table_info reports an empty
    // type string, byte-for-byte like sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    let r = c.query("PRAGMA table_info(t)").unwrap();
    assert_eq!(r.rows[0][1], text("title"));
    assert_eq!(r.rows[0][2], text("")); // empty type
    assert_eq!(r.rows[1][1], text("body"));
    assert_eq!(r.rows[1][2], text(""));
}

#[test]
fn config_options_are_ignored_only_columns_declared() {
    // A `tokenize = …` option arg is not a column; only `a` and `b` are declared.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(a, b, tokenize = 'porter')")
        .unwrap();
    assert_eq!(
        c.query("PRAGMA table_info(t)").unwrap().rows.len(),
        2,
        "the tokenize option must not become a column"
    );
    c.execute("INSERT INTO t VALUES ('hi','there')").unwrap();
    assert_eq!(
        rows(&c, "SELECT a, b FROM t"),
        [vec![text("hi"), text("there")]]
    );
}

#[test]
fn match_queries_tokens() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('Hello World','the quick brown fox'),\
         ('Goodbye','the lazy dog'),\
         ('Mixed','Fox and Dog')",
    )
    .unwrap();

    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };

    // `t MATCH x` searches across all columns; column refs scope to one column.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'fox' ORDER BY rowid"),
        [1, 3]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE body MATCH 'dog' ORDER BY rowid"),
        [2, 3]
    );
    assert_eq!(ids("SELECT rowid FROM t WHERE title MATCH 'hello'"), [1]);
    // Space-separated tokens are AND-ed; matching is case-insensitive.
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'quick fox'"), [1]);
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'FOX' ORDER BY rowid"),
        [1, 3]
    );
    // No match, and a column-scoped token that lives in a different column.
    assert!(ids("SELECT rowid FROM t WHERE t MATCH 'zebra'").is_empty());
    assert!(ids("SELECT rowid FROM t WHERE title MATCH 'fox'").is_empty());
}

#[test]
fn explicit_duplicate_rowid_is_rejected() {
    // Inserting an explicit rowid that already exists is a UNIQUE conflict on the
    // implicit rowid — it errors (and `OR IGNORE`/`OR REPLACE` skip/replace),
    // matching sqlite, rather than silently overwriting the row.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(x)").unwrap();
    c.execute("INSERT INTO t(rowid, x) VALUES (1, 'a')")
        .unwrap();
    // A plain INSERT with the same rowid fails and leaves the row untouched.
    assert!(c
        .execute("INSERT INTO t(rowid, x) VALUES (1, 'b')")
        .is_err());
    assert_eq!(rows(&c, "SELECT x FROM t"), [vec![text("a")]]);
    // OR IGNORE skips the conflicting row.
    c.execute("INSERT OR IGNORE INTO t(rowid, x) VALUES (1, 'b')")
        .unwrap();
    assert_eq!(rows(&c, "SELECT x FROM t"), [vec![text("a")]]);
    // OR REPLACE overwrites it.
    c.execute("INSERT OR REPLACE INTO t(rowid, x) VALUES (1, 'b')")
        .unwrap();
    assert_eq!(rows(&c, "SELECT x FROM t"), [vec![text("b")]]);
    // A duplicate within a single multi-row INSERT also fails.
    c.execute("DELETE FROM t").unwrap();
    assert!(c
        .execute("INSERT INTO t(rowid, x) VALUES (1, 'a'), (1, 'b')")
        .is_err());
}

#[test]
fn porter_tokenizer_stems_tokens() {
    // `tokenize='porter'` Porter-stems every token at index and query time, so a
    // query matches any inflected form, byte-for-byte like sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(x, tokenize='porter')")
        .unwrap();
    c.execute("INSERT INTO t VALUES('the runners were running quickly')")
        .unwrap();
    c.execute("INSERT INTO t VALUES('a national connection')")
        .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not a rowid"),
            })
            .collect::<Vec<_>>()
    };
    let one = |sql: &str| match &c.query(sql).unwrap().rows[0][0] {
        Value::Text(s) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    // Every inflected form of "run" stems to the same root and matches row 1.
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'run'"), [1]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'running'"), [1]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'runner'"), [1]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'nation'"), [2]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'connect'"), [2]);
    // highlight() wraps the original (unstemmed) token text of a stemmed match
    // (`running` stems to `run`; `runners` stems to `runner`, so it is not a match).
    assert_eq!(
        one("SELECT highlight(t, 0, '[', ']') FROM t WHERE t MATCH 'run'"),
        "the runners were [running] quickly"
    );
}

#[test]
fn rebuild_and_optimize_commands_are_noops() {
    // FTS5's table-named hidden column accepts maintenance commands; for graphite's
    // scan-based index `rebuild`/`optimize` change nothing and insert no row,
    // matching sqlite3's success-with-no-change.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(x)").unwrap();
    c.execute("INSERT INTO t VALUES('alpha'),('beta')").unwrap();
    c.execute("INSERT INTO t(t) VALUES('rebuild')").unwrap();
    c.execute("INSERT INTO t(t) VALUES('optimize')").unwrap();
    assert_eq!(
        rows(&c, "SELECT count(*) FROM t"),
        [vec![Value::Integer(2)]]
    );
    // The documents are still searchable after the no-op commands.
    assert_eq!(
        rows(&c, "SELECT rowid FROM t WHERE t MATCH 'alpha'"),
        [vec![Value::Integer(1)]]
    );
}

#[test]
fn unindexed_columns_are_not_searched() {
    // A column declared `UNINDEXED` is stored and retrievable but excluded from
    // the full-text index — matching, `bm25()`, `highlight()`, and `snippet()` all
    // ignore it, byte-for-byte like sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(a, b UNINDEXED)")
        .unwrap();
    c.execute("INSERT INTO t VALUES('cat dog','cat bird')")
        .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not a rowid"),
            })
            .collect::<Vec<_>>()
    };
    let one = |sql: &str| match &c.query(sql).unwrap().rows[0][0] {
        Value::Text(s) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    // `bird` only appears in the unindexed column → no match.
    assert!(ids("SELECT rowid FROM t WHERE t MATCH 'bird'").is_empty());
    // `dog` is in the indexed column → matches.
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'dog'"), [1]);
    // A column-scoped MATCH on the unindexed column matches nothing.
    assert!(ids("SELECT rowid FROM t WHERE b MATCH 'cat'").is_empty());
    // highlight()/snippet() leave the unindexed column verbatim even though the
    // query term also appears there.
    assert_eq!(
        one("SELECT highlight(t, 1, '[', ']') FROM t WHERE t MATCH 'cat'"),
        "cat bird"
    );
    assert_eq!(
        one("SELECT highlight(t, 0, '[', ']') FROM t WHERE t MATCH 'cat'"),
        "[cat] dog"
    );
}

#[test]
fn match_column_filter_syntax() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('Hello World','the quick brown fox'),\
         ('Goodbye','the lazy dog'),\
         ('Mixed Fox','and Dog')",
    )
    .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    // `col:token` in a table-wide MATCH restricts the token to that column.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'body:dog' ORDER BY rowid"),
        [2, 3]
    );
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'title:fox'"), [3]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'title:hello'"), [1]);
    // Column-filtered terms AND together across columns.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'body:dog title:mixed'"),
        [3]
    );
    // SQLite allows whitespace around the `:` — all forms are equivalent.
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'title : fox'"), [3]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'title: fox'"), [3]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'title :fox'"), [3]);
}

#[test]
fn match_phrase_and_prefix_queries() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('the quick brown fox'),('the brown quick cat'),('quick foxes run')",
    )
    .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    // A quoted phrase requires the tokens to be adjacent and in order.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH '\"quick brown\"'"),
        [1]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH '\"brown quick\"'"),
        [2]
    );
    // A `token*` prefix matches any token starting with it.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'fox*' ORDER BY rowid"),
        [1, 3]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'qu*' ORDER BY rowid"),
        [1, 2, 3]
    );
}

#[test]
fn match_boolean_operators() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('apple banana'),('banana cherry'),('cherry date'),('apple date')",
    )
    .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'apple OR cherry' ORDER BY rowid"),
        [1, 2, 3, 4]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'apple AND date'"),
        [4]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'banana NOT cherry'"),
        [1]
    );
    // AND binds tighter than OR.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'apple OR banana AND cherry' ORDER BY rowid"),
        [1, 2, 4]
    );
    // Parentheses override precedence.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH '(apple OR banana) AND date'"),
        [4]
    );
}

#[test]
fn match_near_proximity() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('the quick brown fox jumps'),\
         ('quick the lazy brown'),\
         ('brown then many words later quick')",
    )
    .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'NEAR(quick brown)' ORDER BY rowid"),
        [1, 2, 3]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'NEAR(quick brown, 2)' ORDER BY rowid"),
        [1, 2]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'NEAR(quick brown, 1)'"),
        [1]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'NEAR(quick brown, 0)'"),
        [1]
    );
}

#[test]
fn match_anchor_first_token() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('quick brown fox'),('the quick fox'),('brown quick')")
        .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    // `^token` matches only rows where the token is the first in the column.
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH '^quick'"), [1]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH '^brown'"), [3]);
}

#[test]
fn bm25_rank_orders_by_relevance() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('the quick brown fox'),('quick quick fox'),\
         ('a slow green turtle'),('fox fox fox jumps')",
    )
    .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    // `ORDER BY rank` returns the most relevant rows first (row 4 has three
    // `fox` hits, then row 2, then row 1) — byte-for-byte sqlite's bm25 order.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'fox' ORDER BY rank"),
        [4, 2, 1]
    );
    // `bm25(t)` and the `rank` column expose the same (negative) score.
    let r = c
        .query("SELECT bm25(t), rank FROM t WHERE t MATCH 'fox' ORDER BY rowid")
        .unwrap();
    for row in &r.rows {
        match (&row[0], &row[1]) {
            (Value::Real(a), Value::Real(b)) => {
                assert!((a - b).abs() < 1e-12 && *a < 0.0, "bm25={a} rank={b}")
            }
            o => panic!("not reals: {o:?}"),
        }
    }
}

#[test]
fn bm25_column_weights() {
    // `bm25(t, w1, w2)` weights each column's contribution; a title-heavy weight
    // ranks a title hit above a doubled body hit — byte-for-byte like sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('apple','nothing here'),('nothing','apple apple')")
        .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    // Title weighted 10×: the title hit (row 1) outranks the double body hit.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'apple' ORDER BY bm25(t, 10.0, 1.0)"),
        [1, 2]
    );
    // Body weighted 5×: the body hits (row 2) win instead.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'apple' ORDER BY bm25(t, 1.0, 5.0)"),
        [2, 1]
    );
}

#[test]
fn highlight_wraps_matched_terms() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('Hello World','the quick brown fox jumps over the lazy dog')")
        .unwrap();
    let one = |sql: &str| match &c.query(sql).unwrap().rows[0][0] {
        Value::Text(s) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    // A single matched token in the body, surrounding text preserved.
    assert_eq!(
        one("SELECT highlight(t, 1, '[', ']') FROM t WHERE t MATCH 'fox'"),
        "the quick brown [fox] jumps over the lazy dog"
    );
    // Case-insensitive match, case-preserving output, in the title column.
    assert_eq!(
        one("SELECT highlight(t, 0, '<b>', '</b>') FROM t WHERE t MATCH 'hello'"),
        "<b>Hello</b> World"
    );
    // A matched phrase shares one pair of markers; two separate tokens do not.
    assert_eq!(
        one("SELECT highlight(t, 1, '[', ']') FROM t WHERE t MATCH '\"quick brown\"'"),
        "the [quick brown] fox jumps over the lazy dog"
    );
    assert_eq!(
        one("SELECT highlight(t, 1, '[', ']') FROM t WHERE t MATCH 'the dog'"),
        "[the] quick brown fox jumps over [the] lazy [dog]"
    );
}

#[test]
fn snippet_selects_best_window() {
    // Every expected string was captured from sqlite3 3.50.4 verbatim; the window
    // selection (centering, sentence/column-start snapping, distinct-phrase scoring)
    // and trailing-text rules match `fts5SnippetFunction` byte-for-byte.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)")
        .unwrap();
    let snip = |c: &mut Connection, doc_a: &str, doc_b: &str, sql: &str| -> String {
        c.execute("DELETE FROM t").unwrap();
        // The test corpus contains no single quotes, so inline insertion is safe.
        c.execute(&format!("INSERT INTO t(a,b) VALUES('{doc_a}','{doc_b}')"))
            .unwrap();
        match &c.query(sql).unwrap().rows[0][0] {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    let dog = "the quick brown fox jumps over the lazy dog";
    // A lone hit near the start snaps the window to the column head, with a
    // trailing ellipsis because the window stops short of the end.
    assert_eq!(
        snip(
            &mut c,
            dog,
            "x",
            "SELECT snippet(t,0,'[',']','...',4) FROM t WHERE t MATCH 'fox'"
        ),
        "the quick brown [fox]..."
    );
    // Two distinct phrases: the window maximizing distinct coverage wins.
    assert_eq!(
        snip(
            &mut c,
            "a fox runs river the sat mat fox away jumps over red cat",
            "x",
            "SELECT snippet(t,0,'[',']','...',10) FROM t WHERE t MATCH 'sat OR red'"
        ),
        "...river the [sat] mat fox away jumps over [red] cat"
    );
    // An adjacent cluster outscores an earlier lone hit (the +1-per-repeat term).
    assert_eq!(
        snip(
            &mut c,
            "a a a a near a a a a a a a near near river",
            "x",
            "SELECT snippet(t,0,'[',']','.',4) FROM t WHERE t MATCH 'near'"
        ),
        ".a [near] [near] river"
    );
    // Reaching the column end appends the remaining text (the trailing period).
    assert_eq!(
        snip(
            &mut c,
            "mat over jumps lazy river.",
            "x",
            "SELECT snippet(t,0,'[',']','...',8) FROM t WHERE t MATCH 'river'"
        ),
        "mat over jumps lazy [river]."
    );
    // A `.`-delimited sentence boundary is a candidate window start.
    assert_eq!(
        snip(
            &mut c,
            "one two three. four five fox six seven eight nine ten",
            "x",
            "SELECT snippet(t,0,'[',']','...',4) FROM t WHERE t MATCH 'fox'"
        ),
        "...four five [fox] six..."
    );
    // A column-scoped term highlights nothing in a different column.
    assert_eq!(
        snip(
            &mut c,
            "red fox cat dog",
            "red away of sat",
            "SELECT snippet(t,1,'<','>','.',3) FROM t WHERE t MATCH 'a:red'"
        ),
        "red away of."
    );
    // A matched phrase shares one pair of markers.
    assert_eq!(
        snip(
            &mut c,
            "the quick brown fox jumps over",
            "x",
            "SELECT snippet(t,0,'[',']','...',5) FROM t WHERE t MATCH '\"brown fox\"'"
        ),
        "the quick [brown fox] jumps..."
    );
    // A negative column auto-selects the highest-scoring column…
    assert_eq!(
        snip(
            &mut c,
            "the quick brown fox",
            "lazy dog runs here today ok",
            "SELECT snippet(t,-1,'[',']','...',3) FROM t WHERE t MATCH 'dog'"
        ),
        "lazy [dog] runs..."
    );
    // …and breaks score ties toward the first column.
    assert_eq!(
        snip(
            &mut c,
            "alpha red beta",
            "gamma red delta",
            "SELECT snippet(t,-1,'[',']','.',3) FROM t WHERE t MATCH 'red'"
        ),
        "alpha [red] beta"
    );
}

#[test]
fn explain_query_plan_reports_match_index() {
    // The reported `idxNum:idxStr` matches sqlite's fts5 xBestIndex: a table-wide
    // MATCH is `INDEX 0:M<ncols>`, a column MATCH is `M<colidx>`, a rowid lookup is
    // `=`, and `ORDER BY rank`/`rowid` set the order-consumed bit (32 / 64).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE f USING fts5(a, b, c)")
        .unwrap();
    let plan = |sql: &str| match c.query(sql).unwrap().rows.last().unwrap().last() {
        Some(Value::Text(s)) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    assert_eq!(
        plan("EXPLAIN QUERY PLAN SELECT * FROM f WHERE f MATCH 'x'"),
        "SCAN f VIRTUAL TABLE INDEX 0:M3"
    );
    assert_eq!(
        plan("EXPLAIN QUERY PLAN SELECT * FROM f WHERE b MATCH 'x'"),
        "SCAN f VIRTUAL TABLE INDEX 0:M1"
    );
    assert_eq!(
        plan("EXPLAIN QUERY PLAN SELECT * FROM f WHERE rowid = 5"),
        "SCAN f VIRTUAL TABLE INDEX 0:="
    );
    assert_eq!(
        plan("EXPLAIN QUERY PLAN SELECT * FROM f WHERE f MATCH 'x' ORDER BY rank"),
        "SCAN f VIRTUAL TABLE INDEX 32:M3"
    );
    assert_eq!(
        plan("EXPLAIN QUERY PLAN SELECT * FROM f WHERE f MATCH 'x' ORDER BY rowid"),
        "SCAN f VIRTUAL TABLE INDEX 64:M3"
    );
}

#[test]
fn bm25_outside_an_fts5_match_is_unavailable() {
    // `rank` / `bm25()` only mean something for an fts5 MATCH query; elsewhere
    // they are an ordinary unknown column / function (an error), as in sqlite.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE r(x)").unwrap();
    c.execute("INSERT INTO r VALUES (1)").unwrap();
    assert!(c.query("SELECT rank FROM r").is_err());
    assert!(c.query("SELECT bm25(r) FROM r").is_err());
    // A real column literally named `rank` still works.
    c.execute("CREATE TABLE s(rank)").unwrap();
    c.execute("INSERT INTO s VALUES (3),(1),(2)").unwrap();
    assert_eq!(
        rows(&c, "SELECT rank FROM s ORDER BY rank"),
        [
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)]
        ]
    );
}

#[test]
fn match_against_null_pattern_is_null() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('hello')").unwrap();
    // A NULL query matches nothing (NULL is not true in a WHERE clause).
    assert!(rows(&c, "SELECT rowid FROM t WHERE t MATCH NULL").is_empty());
}

#[test]
fn persists_and_passes_integrity_check() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('alpha'),('beta')")
        .unwrap();
    // The documents live in the real backing table.
    assert_eq!(
        rows(&c, "SELECT rowid, body FROM t_data ORDER BY rowid"),
        [
            vec![Value::Integer(1), text("alpha")],
            vec![Value::Integer(2), text("beta")],
        ]
    );
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        text("ok")
    );
}
