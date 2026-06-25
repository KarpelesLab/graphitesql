//! Roadmap D2b-1: decode the FTS5 `%_data` segment index for a single-term
//! doclist lookup, cross-checked against `sqlite3`.
//!
//! `src/fts5_index.rs` carries the in-crate decoder (`decode_term`) and verifies
//! it against its own byte-verified writer in unit tests. That decoder is
//! `pub(crate)`, so this integration test cannot call it directly; instead it
//! ports the SAME single-leaf decoding logic here (just as `tests/fts5_segment.rs`
//! ports the encoder) and proves the FORMAT UNDERSTANDING correct against bytes
//! that real `sqlite3` 3.50.4 wrote:
//!
//!  1. build a tiny fts5 index with `sqlite3` (small enough that the whole
//!     segment is a single height-0 leaf),
//!  2. pull the leaf blob out of the `%_data` shadow table,
//!  3. decode each probed term's doclist (docids + positions) from those raw
//!     bytes, and
//!  4. assert the docids match what `sqlite3`'s own `MATCH` reports for the same
//!     index, sweeping every term `fts5vocab(..., 'row')` lists — covering
//!     single-occurrence, multi-doc, and multi-occurrence-in-one-doc terms, plus
//!     an absent term.
//!
//! If `sqlite3` is not on PATH the test skips gracefully.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use std::collections::BTreeSet;
use std::process::Command;

// ---------------------------------------------------------------------------
// The decoder under test (the single-leaf inverse of the segment writer). This
// mirrors `src/fts5_index.rs::decode_term` exactly; the in-crate copy is the one
// that ships, this copy lets the integration test exercise it on sqlite's bytes.

/// Decode a varint, returning `(value, bytes_consumed)`. `None` if `buf` ends
/// mid-varint.
fn get_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in buf.iter().enumerate().take(9) {
        if i == 8 {
            return Some((v << 8 | b as u64, 9));
        }
        v = (v << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let (v, n) = get_varint(buf.get(*pos..)?)?;
    *pos += n;
    Some(v)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DecodedPosting {
    rowid: i64,
    cols: Vec<Vec<u32>>,
}

/// Decode one position list, advancing `pos`. Inverse of the writer's `poslist`.
fn decode_poslist(buf: &[u8], pos: &mut usize) -> Option<Vec<Vec<u32>>> {
    let size2 = read_varint(buf, pos)?;
    let content_len = (size2 / 2) as usize;
    let end = pos.checked_add(content_len)?;
    if end > buf.len() {
        return None;
    }
    let mut cols: Vec<Vec<u32>> = vec![Vec::new()];
    let mut col = 0usize;
    let mut p = *pos;
    while p < end {
        if buf[p] == 0x01 {
            p += 1;
            let c = read_varint(buf, &mut p)? as usize;
            col = c;
            while cols.len() <= col {
                cols.push(Vec::new());
            }
        } else {
            let raw = read_varint(buf, &mut p)?;
            if raw < 2 {
                return None;
            }
            let delta = (raw - 2) as u32;
            let next = if cols[col].is_empty() {
                delta
            } else {
                cols[col].last().copied()? + delta
            };
            cols[col].push(next);
        }
    }
    *pos = end;
    Some(cols)
}

/// Decode a contiguous doclist `buf[start..end]`. Inverse of the writer's
/// `doclist`.
fn decode_doclist(buf: &[u8], start: usize, end: usize) -> Option<Vec<DecodedPosting>> {
    let mut pos = start;
    let mut out = Vec::new();
    let mut rowid = 0i64;
    let mut first = true;
    while pos < end {
        let d = read_varint(buf, &mut pos)? as i64;
        rowid = if first { d } else { rowid + d };
        first = false;
        let cols = decode_poslist(buf, &mut pos)?;
        if pos > end {
            return None;
        }
        out.push(DecodedPosting { rowid, cols });
    }
    if pos != end {
        return None;
    }
    Some(out)
}

/// Decode a single leaf page into `(term_key, doclist_start, doclist_end)`.
fn decode_leaf_terms(leaf: &[u8]) -> Option<Vec<(Vec<u8>, usize, usize)>> {
    if leaf.len() < 4 {
        return None;
    }
    let footer_off = u16::from_be_bytes([leaf[2], leaf[3]]) as usize;
    if footer_off < 4 || footer_off > leaf.len() {
        return None;
    }
    let mut term_offs: Vec<usize> = Vec::new();
    let mut p = footer_off;
    let mut prev = 0usize;
    let mut first = true;
    while p < leaf.len() {
        let d = read_varint(leaf, &mut p)? as usize;
        let off = if first { d } else { prev.checked_add(d)? };
        first = false;
        if off >= footer_off || off < 4 {
            return None;
        }
        term_offs.push(off);
        prev = off;
    }
    if term_offs.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut prev_key: Vec<u8> = Vec::new();
    for (i, &off) in term_offs.iter().enumerate() {
        let mut p = off;
        let key = if i == 0 {
            let keylen = read_varint(leaf, &mut p)? as usize;
            let end = p.checked_add(keylen)?;
            if end > footer_off {
                return None;
            }
            let key = leaf.get(p..end)?.to_vec();
            p = end;
            key
        } else {
            let n_common = read_varint(leaf, &mut p)? as usize;
            let n_new = read_varint(leaf, &mut p)? as usize;
            let end = p.checked_add(n_new)?;
            if end > footer_off || n_common > prev_key.len() {
                return None;
            }
            let mut key = prev_key.get(..n_common)?.to_vec();
            key.extend_from_slice(leaf.get(p..end)?);
            p = end;
            key
        };
        let doclist_end = term_offs.get(i + 1).copied().unwrap_or(footer_off);
        if doclist_end < p || doclist_end > footer_off {
            return None;
        }
        out.push((key.clone(), p, doclist_end));
        prev_key = key;
    }
    Some(out)
}

/// '0'-prefixed (`FTS5_MAIN_PREFIX`) key for a term.
fn term_key(term: &str) -> Vec<u8> {
    let mut k = vec![b'0'];
    k.extend_from_slice(term.as_bytes());
    k
}

/// Look up `term` across leaf blobs and return its decoded postings, or `None`.
fn decode_term(leaves: &[Vec<u8>], term: &str) -> Option<Vec<DecodedPosting>> {
    let key = term_key(term);
    for leaf in leaves {
        let records = match decode_leaf_terms(leaf) {
            Some(r) => r,
            None => continue,
        };
        for (k, start, end) in records {
            if k == key {
                return decode_doclist(leaf, start, end);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// sqlite3 oracle plumbing.

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn tmp_db(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("gsql-d2b-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{tag}-{}.db", SEQ.fetch_add(1, Ordering::Relaxed)));
    let p = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

fn run_sql(path: &str, sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(path)
        .arg(sql)
        .output()
        .expect("sqlite3 invocation failed");
    assert!(
        o.status.success(),
        "sqlite3 failed for {sql:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn parse_hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    let inner = s
        .strip_prefix("X'")
        .or_else(|| s.strip_prefix("x'"))
        .and_then(|r| r.strip_suffix('\''))
        .unwrap_or("");
    (0..inner.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&inner[i..i + 2], 16).unwrap())
        .collect()
}

/// All height-0 leaf blobs of the single-column fts5 table `t` in `path`.
fn leaf_blobs(path: &str) -> Vec<Vec<u8>> {
    run_sql(
        path,
        "SELECT quote(block) FROM t_data WHERE id>100 ORDER BY id;",
    )
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(parse_hex)
    .collect()
}

/// The docids `sqlite3` returns for `t MATCH term`, as a sorted set.
fn sqlite_match_docids(path: &str, term: &str) -> BTreeSet<i64> {
    run_sql(
        path,
        &format!("SELECT rowid FROM t WHERE t MATCH '{term}' ORDER BY rowid;"),
    )
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| l.trim().parse().unwrap())
    .collect()
}

/// Build a fresh single-column fts5 table `t` with the given docs and return the
/// db path. A single multi-VALUES insert keeps it to one segment.
fn build_fts5(docs: &[(i64, &str)]) -> String {
    let path = tmp_db("decode");
    let values: Vec<String> = docs.iter().map(|(r, b)| format!("({r},'{b}')")).collect();
    let script = format!(
        "CREATE VIRTUAL TABLE t USING fts5(body);\
         INSERT INTO t(rowid,body) VALUES {};",
        values.join(",")
    );
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .expect("sqlite3 build failed");
    assert!(o.status.success(), "build: {o:?}");
    path
}

/// The decoded docids for `term` (single-column → positions live in cols[0]).
fn decoded_docids(leaves: &[Vec<u8>], term: &str) -> BTreeSet<i64> {
    decode_term(leaves, term)
        .map(|ps| ps.iter().map(|p| p.rowid).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests.

/// The leaf blob we read is a single height-0 leaf (the small-index assumption
/// the single-leaf decoder relies on). If a corpus ever splits, the test would
/// surface it here rather than silently decode a partial doclist.
fn assert_single_leaf(leaves: &[Vec<u8>]) {
    assert_eq!(
        leaves.len(),
        1,
        "expected a single-leaf segment for this small corpus, got {}",
        leaves.len()
    );
}

#[test]
fn decode_matches_sqlite_for_probed_terms() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A small corpus exercising every doclist shape:
    //  - "alpha"  → single doc (1)            : single-occurrence term
    //  - "beta"   → docs 1 and 3              : multi-doc term (rowid deltas)
    //  - "gamma"  → doc 2, three times        : multi-occurrence in one doc
    //  - "delta"  → docs 1, 2, 3              : present in every doc
    //  - "omega"  → absent                    : not in the index at all
    let docs = &[
        (1, "alpha beta delta"),
        (2, "gamma gamma gamma delta"),
        (3, "beta delta"),
    ];
    let path = build_fts5(docs);
    let leaves = leaf_blobs(&path);
    assert_single_leaf(&leaves);

    for term in ["alpha", "beta", "gamma", "delta"] {
        let decoded = decoded_docids(&leaves, term);
        let oracle = sqlite_match_docids(&path, term);
        assert_eq!(decoded, oracle, "docids for {term:?}");
        assert!(!decoded.is_empty(), "{term:?} should be present");
        // Cross-check against the documents themselves.
        let expected: BTreeSet<i64> = docs
            .iter()
            .filter(|(_, body)| body.split_whitespace().any(|w| w == term))
            .map(|(r, _)| *r)
            .collect();
        assert_eq!(decoded, expected, "docids vs documents for {term:?}");
    }

    // Absent term → empty (sqlite returns no rows; our decoder returns None).
    assert!(decoded_docids(&leaves, "omega").is_empty());
    assert!(sqlite_match_docids(&path, "omega").is_empty());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn decode_positions_for_multi_occurrence_term() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // "gamma" occurs three times in doc 2 (positions 0,1,2) and once in doc 5.
    let docs = &[(2, "gamma gamma gamma"), (5, "x gamma")];
    let path = build_fts5(docs);
    let leaves = leaf_blobs(&path);
    assert_single_leaf(&leaves);

    let postings = decode_term(&leaves, "gamma").expect("gamma present");
    // Two postings, ascending rowid.
    assert_eq!(postings.len(), 2);
    assert_eq!(postings[0].rowid, 2);
    assert_eq!(
        postings[0].cols[0],
        vec![0, 1, 2],
        "three positions in doc 2"
    );
    assert_eq!(postings[1].rowid, 5);
    assert_eq!(
        postings[1].cols[0],
        vec![1],
        "one position (pos 1) in doc 5"
    );

    // The docids still agree with sqlite's MATCH.
    assert_eq!(
        decoded_docids(&leaves, "gamma"),
        sqlite_match_docids(&path, "gamma")
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn decode_all_terms_agree_with_fts5vocab() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Differential sweep: for EVERY term sqlite reports via fts5vocab, the
    // decoder must return the same set of docids as sqlite's MATCH.
    let docs = &[
        (1, "the quick brown fox"),
        (2, "the lazy dog"),
        (3, "quick quick fox dog"),
        (4, "brown brown brown"),
    ];
    let path = build_fts5(docs);
    let leaves = leaf_blobs(&path);
    assert_single_leaf(&leaves);

    // fts5vocab('row') lists each distinct term once.
    run_sql(&path, "CREATE VIRTUAL TABLE v USING fts5vocab(t, 'row');");
    let terms: Vec<String> = run_sql(&path, "SELECT term FROM v ORDER BY term;")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();
    assert!(!terms.is_empty(), "fts5vocab returned no terms");

    for term in &terms {
        let decoded = decoded_docids(&leaves, term);
        let oracle = sqlite_match_docids(&path, term);
        assert_eq!(decoded, oracle, "docids for {term:?}");
    }

    let _ = std::fs::remove_file(&path);
}
