//! Roadmap D2b-3: decode a MULTI-LEAF FTS5 `%_data` segment (term pagination and
//! doclist spanning), cross-checked against `sqlite3` 3.50.4.
//!
//! `src/fts5_index.rs` carries the in-crate decoder (`decode_term`) and verifies
//! it against its own byte-verified writer in unit tests. That decoder is
//! `pub(crate)`, so this integration test cannot call it directly; instead it
//! ports the SAME multi-leaf decoding logic here (just as `tests/fts5_decode.rs`
//! does for the single-leaf case) and proves the FORMAT UNDERSTANDING correct
//! against bytes that real `sqlite3` wrote:
//!
//!  1. build an fts5 index with `sqlite3`, forcing a small logical page size
//!     (`INSERT INTO t(t, rank) VALUES('pgsz', N)`) so the segment SPLITS into
//!     several height-0 leaves — both term pagination (many distinct terms) and
//!     a doclist that spans leaves (one term over many docs),
//!  2. pull the segment's leaf blobs out of the `%_data` shadow table (in page
//!     order, for the single segment a bulk insert produces),
//!  3. decode each probed term's doclist (docids + positions) from those raw
//!     bytes via the multi-leaf decoder, and
//!  4. assert the docids match what `sqlite3`'s own `MATCH` reports, sweeping
//!     every term `fts5vocab(..., 'row')` lists.
//!
//! If `sqlite3` is not on PATH the test skips gracefully.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use std::collections::BTreeSet;
use std::process::Command;

// ---------------------------------------------------------------------------
// The multi-leaf decoder under test. This mirrors `src/fts5_index.rs` exactly
// (the in-crate copy is the one that ships; this copy lets the integration test
// exercise it on sqlite's bytes).

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
                cols[col].last().copied()?.checked_add(delta)?
            };
            cols[col].push(next);
        }
    }
    *pos = end;
    Some(cols)
}

struct TermRec {
    key: Vec<u8>,
    rec_start: usize,
    doclist_start: usize,
}

struct LeafView {
    first_rowid_off: usize,
    footer_off: usize,
    terms: Vec<TermRec>,
}

/// Parse a leaf page into its header offsets and term records.
fn parse_leaf(leaf: &[u8]) -> Option<LeafView> {
    if leaf.len() < 4 {
        return None;
    }
    let first_rowid_off = u16::from_be_bytes([leaf[0], leaf[1]]) as usize;
    let footer_off = u16::from_be_bytes([leaf[2], leaf[3]]) as usize;
    if footer_off < 4 || footer_off > leaf.len() {
        return None;
    }
    if first_rowid_off != 0 && (first_rowid_off < 4 || first_rowid_off > footer_off) {
        return None;
    }
    let mut term_offs: Vec<usize> = Vec::new();
    {
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
    }
    let mut terms = Vec::with_capacity(term_offs.len());
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
        if p > footer_off {
            return None;
        }
        terms.push(TermRec {
            key: key.clone(),
            rec_start: off,
            doclist_start: p,
        });
        prev_key = key;
    }
    Some(LeafView {
        first_rowid_off,
        footer_off,
        terms,
    })
}

struct DoclistRun<'a> {
    bytes: &'a [u8],
    abs_start: bool,
}

fn decode_spanning_doclist(runs: &[DoclistRun]) -> Option<Vec<DecodedPosting>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut abs_at: Vec<usize> = Vec::new();
    for run in runs {
        if run.abs_start && !run.bytes.is_empty() {
            abs_at.push(buf.len());
        }
        buf.extend_from_slice(run.bytes);
    }
    let end = buf.len();
    let mut pos = 0usize;
    let mut out = Vec::new();
    let mut rowid = 0i64;
    let mut first = true;
    while pos < end {
        let absolute = first || abs_at.contains(&pos);
        let d = read_varint(&buf, &mut pos)? as i64;
        rowid = if absolute { d } else { rowid.wrapping_add(d) };
        first = false;
        let cols = decode_poslist(&buf, &mut pos)?;
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

fn gather_doclist_runs<'a>(
    leaves: &'a [Vec<u8>],
    start_leaf: usize,
    start_ti: usize,
    start_off: usize,
    leaf_views: &[LeafView],
) -> Option<Vec<DoclistRun<'a>>> {
    let mut runs: Vec<DoclistRun<'a>> = Vec::new();
    let first_view = &leaf_views[start_leaf];
    let first_next_term = first_view.terms.get(start_ti + 1).map(|r| r.rec_start);
    let first_end = first_next_term.unwrap_or(first_view.footer_off);
    if first_end < start_off || first_end > first_view.footer_off {
        return None;
    }
    runs.push(DoclistRun {
        bytes: leaves[start_leaf].get(start_off..first_end)?,
        abs_start: true,
    });
    if first_next_term.is_some() {
        return Some(runs);
    }
    let mut li = start_leaf + 1;
    while li < leaves.len() {
        let view = &leaf_views[li];
        let next_term = view.terms.first().map(|r| r.rec_start);
        let boundary = next_term.unwrap_or(view.footer_off);
        let tail_end = if view.first_rowid_off == 0 {
            boundary
        } else {
            view.first_rowid_off
        };
        if tail_end < 4 || tail_end > boundary || boundary > view.footer_off {
            return None;
        }
        runs.push(DoclistRun {
            bytes: leaves[li].get(4..tail_end)?,
            abs_start: false,
        });
        if view.first_rowid_off != 0 {
            runs.push(DoclistRun {
                bytes: leaves[li].get(view.first_rowid_off..boundary)?,
                abs_start: true,
            });
        }
        if next_term.is_some() {
            break;
        }
        li += 1;
    }
    Some(runs)
}

fn term_key(term: &str) -> Vec<u8> {
    let mut k = vec![b'0'];
    k.extend_from_slice(term.as_bytes());
    k
}

/// Look up `term` across the segment's leaf blobs (in page order) and return its
/// decoded postings, or `None` if absent / unsupported.
fn decode_term(leaves: &[Vec<u8>], term: &str) -> Option<Vec<DecodedPosting>> {
    let key = term_key(term);
    let mut views: Vec<LeafView> = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        views.push(parse_leaf(leaf)?);
    }
    for (li, view) in views.iter().enumerate() {
        for (ti, rec) in view.terms.iter().enumerate() {
            if rec.key != key {
                continue;
            }
            let runs = gather_doclist_runs(leaves, li, ti, rec.doclist_start, &views)?;
            return decode_spanning_doclist(&runs);
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
    let dir = std::env::temp_dir().join(format!("gsql-d2b3-{}", std::process::id()));
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

/// The single segment's height-0 leaf blobs of the fts5 table `t` in `path`, in
/// page order. Asserts a single segment (the leaves all share one segid in the
/// top `id` bits) so the page-ordered blob list is exactly the segment.
fn leaf_blobs(path: &str) -> Vec<Vec<u8>> {
    // (segid, pgno, blob) for every height-0 leaf row.
    let rows = run_sql(
        path,
        "SELECT (id>>37), (id & ((1<<37)-1)), quote(block) \
         FROM t_data WHERE id>100 ORDER BY id;",
    );
    let mut segids = BTreeSet::new();
    let mut out: Vec<(i64, Vec<u8>)> = Vec::new();
    for line in rows.lines().filter(|l| !l.trim().is_empty()) {
        let mut it = line.splitn(3, '|');
        let segid: i64 = it.next().unwrap().trim().parse().unwrap();
        let pgno: i64 = it.next().unwrap().trim().parse().unwrap();
        let blob = parse_hex(it.next().unwrap());
        segids.insert(segid);
        out.push((pgno, blob));
    }
    assert_eq!(
        segids.len(),
        1,
        "expected a single segment, got segids {segids:?}"
    );
    out.sort_by_key(|(pgno, _)| *pgno);
    out.into_iter().map(|(_, b)| b).collect()
}

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

/// Build a fresh single-column fts5 table `t` at logical page size `pgsz` with the
/// given docs, returning the db path. A single multi-VALUES insert keeps it to
/// one segment.
fn build_fts5(pgsz: u32, docs: &[(i64, String)]) -> String {
    let path = tmp_db("ml");
    let values: Vec<String> = docs.iter().map(|(r, b)| format!("({r},'{b}')")).collect();
    let script = format!(
        "CREATE VIRTUAL TABLE t USING fts5(body);\
         INSERT INTO t(t, rank) VALUES('pgsz', {pgsz});\
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

fn decoded_docids(leaves: &[Vec<u8>], term: &str) -> BTreeSet<i64> {
    decode_term(leaves, term)
        .map(|ps| ps.iter().map(|p| p.rowid).collect())
        .unwrap_or_default()
}

fn assert_multi_leaf(leaves: &[Vec<u8>]) {
    assert!(
        leaves.len() > 1,
        "expected a multi-leaf segment, got {}",
        leaves.len()
    );
}

// ---------------------------------------------------------------------------
// Tests.

#[test]
fn decode_multi_leaf_term_pagination_vs_sqlite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // 40 distinct single-occurrence terms in one doc → pure TERM pagination at a
    // tiny pgsz: many leaves, each with its own term records.
    let body: String = (0..40)
        .map(|i| format!("term{i:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    let docs = vec![(1i64, body)];
    let path = build_fts5(64, &docs);
    let leaves = leaf_blobs(&path);
    assert_multi_leaf(&leaves);

    for i in 0..40 {
        let term = format!("term{i:03}");
        let decoded = decoded_docids(&leaves, &term);
        let oracle = sqlite_match_docids(&path, &term);
        assert_eq!(decoded, oracle, "docids for {term:?}");
        assert_eq!(decoded, BTreeSet::from([1]), "{term:?} → doc 1");
    }
    assert!(decoded_docids(&leaves, "term999").is_empty());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn decode_doclist_spanning_vs_sqlite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // One term "x" across 60 docs → a single doclist that SPANS many leaves at a
    // tiny pgsz. Decoder must stitch the spanned runs back into all 60 docids.
    let docs: Vec<(i64, String)> = (1..=60).map(|r| (r, "x".to_string())).collect();
    let path = build_fts5(64, &docs);
    let leaves = leaf_blobs(&path);
    assert_multi_leaf(&leaves);

    let decoded = decoded_docids(&leaves, "x");
    let oracle = sqlite_match_docids(&path, "x");
    assert_eq!(decoded, oracle, "spanning docids for 'x'");
    assert_eq!(decoded.len(), 60, "all 60 docs");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn decode_multi_leaf_all_terms_agree_with_fts5vocab() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A larger mixed corpus that spans many leaves: a sweep over EVERY term
    // sqlite's fts5vocab reports must match sqlite's MATCH, at several pgsz values
    // (each forcing different leaf-fill / spill boundaries).
    let words = [
        "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "alpha", "beta", "gamma",
        "delta", "epsilon", "zeta", "eta", "theta",
    ];
    // 50 docs, each a few words drawn deterministically so terms recur across
    // docs (multi-doc doclists that spill) and within docs (multi-position).
    let docs: Vec<(i64, String)> = (1..=50)
        .map(|r| {
            let n = 3 + (r as usize % 4);
            let body: Vec<&str> = (0..n)
                .map(|k| words[(r as usize * 7 + k * 3) % words.len()])
                .collect();
            (r, body.join(" "))
        })
        .collect();

    for pgsz in [64u32, 80, 128] {
        let path = build_fts5(pgsz, &docs);
        let leaves = leaf_blobs(&path);
        assert_multi_leaf(&leaves);

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
            assert_eq!(decoded, oracle, "pgsz {pgsz}: docids for {term:?}");
        }
        // An absent term decodes to nothing.
        assert!(decoded_docids(&leaves, "notpresentword").is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
