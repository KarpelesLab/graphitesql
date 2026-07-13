//! Roadmap D2e-encoder: graphite-written FTS5 stays BYTE-IDENTICAL to sqlite when
//! the table configures PREFIX indexes (`prefix='…'`).
//!
//! For `CREATE VIRTUAL TABLE ft USING fts5(body, prefix='2 3')` sqlite writes, in
//! ADDITION to the main terms index (terms keyed with the `'0'` byte), one prefix
//! index per configured character length: prefix index `i` keys every term's
//! `n_char`-character prefix with the byte `'1' + i` (`FTS5_MAIN_PREFIX + i + 1`),
//! and the doclist for a prefix term is the merge of all full-term doclists
//! sharing that prefix (positions preserved). In a single bulk rebuild all of
//! these land in ONE segment (segid 1), the merged term stream being the `'0'`
//! main terms followed by each prefix index's `'1'`/`'2'`/… terms.
//!
//! graphite previously wrote only the main index, so `%_data`/`%_idx` diverged
//! from sqlite whenever `prefix=` was set (the file stayed MATCH-correct and
//! integrity-clean because prefix queries scan the main index). This test pins the
//! prefix-segment encoder: for several `prefix=` configs and corpora the raw
//! `%_data`/`%_idx` bytes are byte-identical to sqlite's own single-segment index,
//! sqlite's `integrity-check` accepts graphite's file, and a prefix `MATCH 'x*'`
//! returns the same rows. Skipped when `sqlite3` with FTS5 is not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-px-{}-{}-{}.db",
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

/// Run `q` through stock sqlite3 on `path`; assert success; return raw stdout.
fn sqlite_raw(path: &str, q: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// Run `sql` through sqlite3 via stdin (avoids ARG_MAX for large VALUES lists).
fn sqlite_stdin(path: &str, sql: &str) {
    let mut child = Command::new("sqlite3")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// sqlite's FTS5 `integrity-check` must accept the file (no error).
fn sqlite_integrity_ok(path: &str) -> bool {
    Command::new("sqlite3")
        .arg(path)
        .arg("INSERT INTO ft(ft) VALUES('integrity-check');")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A corpus of `n_docs` documents, each `terms_per_doc` space-separated words
/// drawn from a `vocab`-sized pool of `wordNNNN` tokens. The fixed `word` stem
/// gives long shared prefixes so the `prefix='2 3'` indexes have many-term groups.
fn prefix_values(n_docs: usize, terms_per_doc: usize, vocab: usize) -> String {
    let mut s = String::new();
    for d in 1..=n_docs {
        if d > 1 {
            s.push(',');
        }
        let mut body = String::new();
        for w in 0..terms_per_doc {
            if w > 0 {
                body.push(' ');
            }
            let t = (d * 7 + w * 13) % vocab;
            // Vary the stem so the length-2/length-3 prefix groups differ.
            let stem = match t % 4 {
                0 => "app",
                1 => "ban",
                2 => "cat",
                _ => "dog",
            };
            body.push_str(&format!("{stem}{t:04}"));
        }
        s.push_str(&format!("({d},'{body}')"));
    }
    s
}

/// Build the same corpus with graphite (`g`) and stock sqlite (`s`) under the
/// given `prefix=` config. A SINGLE INSERT statement → one flush → segid 1 on
/// level 0 in both engines (graphite always bulk-rebuilds one segment; sqlite's
/// single flush matches without needing `optimize`).
fn build_pair(g: &str, s: &str, prefix_opt: &str, values: &str) {
    {
        let mut c = Connection::create(g).unwrap();
        c.execute(&format!(
            "CREATE VIRTUAL TABLE ft USING fts5(body, prefix='{prefix_opt}')"
        ))
        .unwrap();
        c.execute("BEGIN").ok();
        c.execute(&format!("INSERT INTO ft(rowid,body) VALUES {values}"))
            .unwrap();
        c.execute("COMMIT").ok();
    }
    sqlite_stdin(
        s,
        &format!(
            "CREATE VIRTUAL TABLE ft USING fts5(body, prefix='{prefix_opt}');\n\
             INSERT INTO ft(rowid,body) VALUES {values};"
        ),
    );
}

/// Assert graphite's `%_data` + `%_idx` are byte-identical to sqlite's, that
/// sqlite's integrity-check accepts graphite's file, and that a prefix `MATCH`
/// returns identical rows through both engines.
fn assert_byte_identical(g: &str, s: &str, min_leaves: usize) {
    let n_leaves: usize = sqlite_raw(g, "SELECT count(*) FROM ft_data WHERE id>10;")
        .parse()
        .unwrap();
    assert!(
        n_leaves >= min_leaves,
        "corpus produced only {n_leaves} leaves; expected >= {min_leaves}"
    );

    assert!(
        sqlite_integrity_ok(g),
        "sqlite integrity-check rejected graphite's prefix-index file"
    );
    assert_eq!(sqlite_raw(g, "PRAGMA integrity_check;"), "ok");

    let data = "SELECT id||':'||quote(block) FROM ft_data ORDER BY id;";
    assert_eq!(
        sqlite_raw(g, data),
        sqlite_raw(s, data),
        "%_data bytes diverge from sqlite ({n_leaves} leaves)"
    );
    let idx = "SELECT segid||':'||quote(term)||':'||pgno FROM ft_idx ORDER BY segid, term;";
    assert_eq!(
        sqlite_raw(g, idx),
        sqlite_raw(s, idx),
        "%_idx bytes diverge from sqlite ({n_leaves} leaves)"
    );

    // Prefix MATCH agreement: 'app*' is present in every corpus here.
    let m = "SELECT count(*) FROM ft WHERE ft MATCH 'app*';";
    assert_eq!(sqlite_raw(g, m), sqlite_raw(s, m), "prefix MATCH diverges");
}

#[test]
fn prefix_index_is_byte_identical() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    // (prefix_opt, n_docs, terms_per_doc, vocab, min_expected_leaves)
    for &(pfx, nd, tpd, vocab, min_leaves) in &[
        ("2", 200usize, 10usize, 400usize, 2usize),
        ("2 3", 300, 12, 500, 3),
        ("1 2 3", 400, 10, 700, 4),
        ("3", 800, 8, 1500, 6),
        ("2 4", 1000, 12, 2000, 10),
    ] {
        let g = tmp_path("g");
        let s = tmp_path("s");
        build_pair(&g, &s, pfx, &prefix_values(nd, tpd, vocab));
        assert_byte_identical(&g, &s, min_leaves);
        let _ = std::fs::remove_file(&g);
        let _ = std::fs::remove_file(&s);
    }
}

/// Build a prefix-index corpus as SEPARATE autocommit INSERT statements — each is
/// its own transaction, so both engines append one level-0 segment per insert (the
/// INCREMENTAL multi-segment path, not a single bulk rebuild). Once 16 segments
/// accumulate on a level, both crisis-merge. Asserts graphite's `%_data`/`%_idx`
/// stay byte-identical to sqlite across the whole segmented structure, integrity is
/// clean, and a prefix `MATCH` agrees.
fn build_pair_incremental(g: &str, s: &str, prefix_opt: &str, docs: &[&str]) {
    {
        let mut c = Connection::create(g).unwrap();
        c.execute(&format!(
            "CREATE VIRTUAL TABLE ft USING fts5(body, prefix='{prefix_opt}')"
        ))
        .unwrap();
        for (i, d) in docs.iter().enumerate() {
            // No BEGIN/COMMIT: each INSERT autocommits => one appended segment.
            c.execute(&format!(
                "INSERT INTO ft(rowid,body) VALUES({},'{d}')",
                i + 1
            ))
            .unwrap();
        }
    }
    let mut sql = format!("CREATE VIRTUAL TABLE ft USING fts5(body, prefix='{prefix_opt}');\n");
    for (i, d) in docs.iter().enumerate() {
        sql.push_str(&format!(
            "INSERT INTO ft(rowid,body) VALUES({},'{d}');\n",
            i + 1
        ));
    }
    sqlite_stdin(s, &sql);
}

/// Incremental prefix-index writes (one appended segment per autocommit INSERT,
/// including the crisis merge at 16+) are byte-identical to sqlite. Before the fix
/// graphite bulk-rebuilt a single compacted segment for any `prefix=` table, so its
/// segment structure diverged from sqlite's per-transaction appends.
#[test]
fn prefix_index_incremental_multisegment_is_byte_identical() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    // Distinct docs with long shared stems so the length-2/3 prefix groups are
    // exercised. Counts span below-crisis (2, 3), exactly-crisis (16), and past a
    // crisis with further appends (20).
    let stems = ["app", "ban", "cat", "dog", "elk", "fox"];
    let make = |n: usize| -> Vec<String> {
        (0..n)
            .map(|i| {
                format!(
                    "{}{:04} {}{:04} common",
                    stems[i % 6],
                    i,
                    stems[(i + 3) % 6],
                    i * 2
                )
            })
            .collect()
    };
    for (pfx, n, min_leaves) in [
        ("2", 2usize, 2usize),
        ("2 3", 3, 3),
        ("2 3", 16, 1),
        ("2", 20, 1),
    ] {
        let owned = make(n);
        let docs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let g = tmp_path("inc-g");
        let s = tmp_path("inc-s");
        build_pair_incremental(&g, &s, pfx, &docs);
        assert!(
            sqlite_integrity_ok(&g),
            "sqlite integrity-check rejected graphite's incremental prefix file (n={n})"
        );
        assert_eq!(sqlite_raw(&g, "PRAGMA integrity_check;"), "ok");
        let n_leaves: usize = sqlite_raw(&g, "SELECT count(*) FROM ft_data WHERE id>10;")
            .parse()
            .unwrap();
        assert!(n_leaves >= min_leaves, "n={n}: only {n_leaves} leaves");
        let data = "SELECT id||':'||quote(block) FROM ft_data ORDER BY id;";
        assert_eq!(
            sqlite_raw(&g, data),
            sqlite_raw(&s, data),
            "incremental %_data diverges (pfx={pfx}, n={n})"
        );
        let idx =
            "SELECT segid||':'||quote(term)||':'||pgno FROM ft_idx ORDER BY segid, term, pgno;";
        assert_eq!(
            sqlite_raw(&g, idx),
            sqlite_raw(&s, idx),
            "incremental %_idx diverges (pfx={pfx}, n={n})"
        );
        let m = "SELECT count(*) FROM ft WHERE ft MATCH 'app* OR ban*';";
        assert_eq!(
            sqlite_raw(&g, m),
            sqlite_raw(&s, m),
            "prefix MATCH diverges (n={n})"
        );
        let _ = std::fs::remove_file(&g);
        let _ = std::fs::remove_file(&s);
    }
}

/// Small hand-checked corpus: two docs where two full terms share a 2-char
/// prefix in the SAME document, so the prefix term's doclist merges their
/// positions. Pins the merge (not just pagination).
#[test]
fn prefix_index_merges_positions() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    let values = "(1,'apple apply ant banana'),(2,'apricot append')";
    let g = tmp_path("m-g");
    let s = tmp_path("m-s");
    build_pair(&g, &s, "2 3", values);
    assert!(sqlite_integrity_ok(&g));
    assert_eq!(sqlite_raw(&g, "PRAGMA integrity_check;"), "ok");
    let data = "SELECT id||':'||quote(block) FROM ft_data ORDER BY id;";
    assert_eq!(
        sqlite_raw(&g, data),
        sqlite_raw(&s, data),
        "%_data diverges"
    );
    let idx = "SELECT segid||':'||quote(term)||':'||pgno FROM ft_idx ORDER BY segid, term;";
    assert_eq!(sqlite_raw(&g, idx), sqlite_raw(&s, idx), "%_idx diverges");
    for q in ["ap*", "app*", "an*", "ban*"] {
        let m = format!("SELECT rowid FROM ft WHERE ft MATCH '{q}' ORDER BY rowid;");
        assert_eq!(sqlite_raw(&g, &m), sqlite_raw(&s, &m), "MATCH {q} diverges");
    }
    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&s);
}
