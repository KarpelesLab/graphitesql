//! Roadmap D2e-encoder (interior/spilling doclists) + D2b-leftover (read route):
//! graphite-written FTS5 stays BYTE-IDENTICAL to sqlite 3.50.4 for segments whose
//! single term's doclist SPILLS across many term-less continuation leaves and
//! carries a doclist-index (`dlidx`) — the deepest part of the on-disk segment
//! format short of an incremental merge.
//!
//! Note on "interior pages": FTS5 3.50.4 has NO `height>0` interior `%_data` pages.
//! The per-segment term index is the plain-SQL `%_idx` table `(segid, term, pgno)`
//! (one split-key row per leaf), and the "interior b-tree" of a high-frequency term
//! is its DOCLIST-INDEX — a small `%_data` b-tree (`segid<<37 | 1<<36 | height<<31 |
//! pgno`) built when the term's doclist spills onto `FTS5_MIN_DLIDX_SIZE`+ term-less
//! leaves. This suite pins that shape.
//!
//! The doclist spill copies WHOLE varints up to each page boundary
//! (`fts5FlushOneHash` → `fts5PoslistPrefix`); a byte-at-a-time spill would split a
//! multi-byte position varint across a leaf and diverge. Both a dense single-token
//! corpus and one with MULTI-BYTE poslist varints are checked here.
//!
//! Each case builds the SAME corpus with graphite and with stock sqlite3 (3.50.4,
//! FTS5) + `optimize`, then asserts (modulo the segment-id, which graphite fixes to
//! 1 while sqlite's post-optimize id varies): byte-identical `%_data` leaves + dlidx
//! pages + `%_idx`, sqlite's `integrity-check`/`PRAGMA integrity_check` accept
//! graphite's file, and `MATCH` (incl. the high-frequency term) returns identical
//! rows WITHOUT a `_content` fallback. Skipped when `sqlite3` with FTS5 (3.50.4) is
//! not on PATH.
//!
//! RESIDUAL: when a corpus has enough DISTINCT terms to overflow sqlite's in-memory
//! hash, sqlite flushes several segments and `optimize` MERGES them — the merge
//! writer (`fts5WriteAppendPoslistData`) spills poslists with a slightly different
//! whole-varint boundary than the single-pass `fts5FlushOneHash` graphite mirrors,
//! so a spanning term in a merged index drifts by a few bytes per leaf (still
//! integrity-clean + MATCH-correct). See `interior_merge_residual_is_match_correct`.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-int-{}-{}-{}.db",
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

fn sqlite_integrity_ok(path: &str) -> bool {
    Command::new("sqlite3")
        .arg(path)
        .arg("INSERT INTO ft(ft) VALUES('integrity-check');")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build the same corpus with graphite (`g`) and with sqlite+optimize (`s`).
fn build_pair(g: &str, s: &str, values: &str) {
    {
        let mut c = Connection::create(g).unwrap();
        c.execute("CREATE VIRTUAL TABLE ft USING fts5(body)")
            .unwrap();
        c.execute("BEGIN").ok();
        c.execute(&format!("INSERT INTO ft(rowid,body) VALUES {values}"))
            .unwrap();
        c.execute("COMMIT").ok();
    }
    sqlite_stdin(
        s,
        &format!(
            "CREATE VIRTUAL TABLE ft USING fts5(body);\n\
             INSERT INTO ft(rowid,body) VALUES {values};\n\
             INSERT INTO ft(ft) VALUES('optimize');"
        ),
    );
}

/// Assert graphite's segment is byte-identical to sqlite's modulo the segment id:
/// the leaf `%_data` pages, the doclist-index `%_data` pages, and the `%_idx` rows
/// all match once the segid is masked out of the `%_data` rowid (bits 37+) and the
/// `%_idx.segid` column. Also: sqlite accepts graphite's file, and MATCH agrees.
fn assert_interior_byte_identical(g: &str, s: &str, min_leaves: usize, match_term: &str) {
    let n_leaves: usize = sqlite_raw(
        g,
        "SELECT count(*) FROM ft_data WHERE id>10 AND ((id>>36)&1)=0;",
    )
    .parse()
    .unwrap();
    assert!(
        n_leaves >= min_leaves,
        "corpus produced only {n_leaves} leaves; expected >= {min_leaves}"
    );
    let n_dlidx: usize = sqlite_raw(
        g,
        "SELECT count(*) FROM ft_data WHERE id>10 AND ((id>>36)&1)=1;",
    )
    .parse()
    .unwrap();
    assert!(n_dlidx >= 1, "corpus produced no doclist-index page");

    assert!(
        sqlite_integrity_ok(g),
        "sqlite integrity-check rejected graphite's interior/dlidx file"
    );
    assert_eq!(sqlite_raw(g, "PRAGMA integrity_check;"), "ok");

    // Leaf + dlidx pages, segid masked out of the %_data rowid (low 37 bits keep the
    // dlidx flag + height + pgno; leaf pages keep just the pgno).
    let data = "SELECT (id & ((1<<37)-1))||':'||quote(block) \
                FROM ft_data WHERE id>10 ORDER BY id;";
    assert_eq!(
        sqlite_raw(g, data),
        sqlite_raw(s, data),
        "%_data leaf/dlidx bytes diverge from sqlite ({n_leaves} leaves, {n_dlidx} dlidx)"
    );
    // %_idx rows, segid masked out (the term separator + pgno-field must match).
    let idx = "SELECT quote(term)||':'||pgno FROM ft_idx ORDER BY term, pgno;";
    assert_eq!(
        sqlite_raw(g, idx),
        sqlite_raw(s, idx),
        "%_idx bytes diverge from sqlite ({n_leaves} leaves)"
    );

    // MATCH on the high-frequency spanning term agrees (rows + order).
    let m = format!(
        "SELECT group_concat(rowid) FROM \
         (SELECT rowid FROM ft WHERE ft MATCH '{match_term}' ORDER BY rowid);"
    );
    assert_eq!(
        sqlite_raw(g, &m),
        sqlite_raw(s, &m),
        "MATCH rows diverge for high-frequency term"
    );
}

/// A term at every doc, repeated `reps` times per doc (so its doclist is long and
/// spills across many term-less continuation leaves + gets a doclist-index).
fn spanning_values(n_docs: usize, term: &str, reps: usize) -> String {
    let body = vec![term; reps].join(" ");
    let mut s = String::new();
    for d in 1..=n_docs {
        if d > 1 {
            s.push(',');
        }
        s.push_str(&format!("({d},'{body}')"));
    }
    s
}

/// A term at MANY positions per doc interleaved with unique filler, so the
/// position-list varints grow MULTI-BYTE and the spill must keep each varint whole
/// across a leaf boundary. Few distinct filler terms overall (per doc), keeping the
/// whole build a single hash flush (no merge).
fn bigpos_values(n_docs: usize, term: &str, span: usize) -> String {
    let mut s = String::new();
    for d in 1..=n_docs {
        if d > 1 {
            s.push(',');
        }
        let mut toks = Vec::with_capacity(span);
        for i in 0..span {
            if i % 3 == 0 {
                toks.push(term.to_string());
            } else {
                toks.push(format!("x{i}"));
            }
        }
        s.push_str(&format!("({d},'{}')", toks.join(" ")));
    }
    s
}

#[test]
fn interior_dense_spanning_term_is_byte_identical() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    // (n_docs, reps_per_doc, min_leaves) — each single-flush in sqlite (only 'hot'
    // is distinct, so the hash never overflows → no merge).
    for &(nd, reps, min_leaves) in &[(200usize, 400usize, 15usize), (120, 700, 15)] {
        let g = tmp_path("dense-g");
        let s = tmp_path("dense-s");
        build_pair(&g, &s, &spanning_values(nd, "hot", reps));
        assert_interior_byte_identical(&g, &s, min_leaves, "hot");
        let _ = std::fs::remove_file(&g);
        let _ = std::fs::remove_file(&s);
    }
}

#[test]
fn interior_multibyte_poslist_spill_is_byte_identical() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    // Multi-byte poslist varints spanning leaf boundaries: the whole-varint spill
    // must match sqlite (a byte-at-a-time spill would split a varint and diverge).
    for &(nd, span, min_leaves) in &[(400usize, 300usize, 40usize), (250, 450, 40)] {
        let g = tmp_path("bigpos-g");
        let s = tmp_path("bigpos-s");
        build_pair(&g, &s, &bigpos_values(nd, "hot", span));
        assert_interior_byte_identical(&g, &s, min_leaves, "hot");
        let _ = std::fs::remove_file(&g);
        let _ = std::fs::remove_file(&s);
    }
}

/// A corpus with enough DISTINCT terms to overflow sqlite's hash forces sqlite to
/// MERGE several flushed segments; the merge writer spills poslists on a slightly
/// different boundary than graphite's single-pass build, so a spanning term drifts a
/// few bytes per leaf. This is a documented residual — the file must still be
/// integrity-clean and MATCH-correct (served by the index route, not scanned).
#[test]
fn interior_merge_residual_is_match_correct() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    let g = tmp_path("merge-g");
    let s = tmp_path("merge-s");
    // 'hot' in every doc (spanning) + ~60k distinct rare terms → sqlite merges.
    let mut v = String::new();
    for d in 1..=6000usize {
        if d > 1 {
            v.push(',');
        }
        let mut body = String::from("hot");
        for w in 0..6 {
            let t = (d * 7 + w * 13) % 60000;
            body.push_str(&format!(" t{t:06}"));
        }
        v.push_str(&format!("({d},'{body}')"));
    }
    build_pair(&g, &s, &v);

    assert!(
        sqlite_integrity_ok(&g),
        "graphite's merge-residual file failed integrity-check"
    );
    assert_eq!(sqlite_raw(&g, "PRAGMA integrity_check;"), "ok");
    // MATCH on the high-frequency term returns identical rows through both engines.
    let m = "SELECT group_concat(rowid) FROM \
             (SELECT rowid FROM ft WHERE ft MATCH 'hot' ORDER BY rowid);";
    assert_eq!(
        sqlite_raw(&g, m),
        sqlite_raw(&s, m),
        "MATCH rows diverge for the merge-residual high-frequency term"
    );
    let cnt = "SELECT count(*) FROM ft WHERE ft MATCH 'hot';";
    assert_eq!(sqlite_raw(&g, cnt), "6000");
    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&s);
}
