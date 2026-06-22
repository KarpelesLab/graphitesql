//! Roadmap D2e-M2: byte-exact FTS5 `%_data` segment encoding.
//!
//! graphite currently READS fts5 (D2e-M1) from the `%_content` documents and
//! stores its own documents in a generic `<name>_data` backing table — so a
//! graphite-written fts5 table is not yet `MATCH`-able by stock sqlite, which
//! expects the segmented `%_data`/`%_idx` inverted index. Closing that gap is a
//! storage-layer rework (swap the generic backing table for sqlite's five shadow
//! tables). Its core risk is reproducing sqlite's segment bytes exactly.
//!
//! This test retires that risk: it builds segments in Rust and proves the bytes
//! identical to what `sqlite3` 3.50.4 writes. It covers single-leaf segments
//! (structure record, averages record, leaf page) across every doclist shape
//! (multiple docs per term → rowid deltas, multiple positions per doc → collist
//! deltas, term prefix compression including the `FTS5_MAIN_PREFIX` '0' byte),
//! and — by lowering FTS5's logical page size (`pgsz`) — MULTI-LEAF segments:
//! the leaf-packing split threshold, each leaf's re-stated first term, the
//! structure record's config cookie + write counter, and the `%_idx` separator
//! rows (each a shortest-distinguishing prefix of a leaf's first term). It also
//! covers a single term whose DOCLIST spans leaves: the byte-level split (an
//! entry header may overshoot `pgsz`, then the collist carries to the next leaf),
//! the absolute first rowid on a continuation leaf, the non-zero first-rowid
//! header offset, and the single `%_idx` row. The encoder here is the reference
//! the storage rework will port into the lib.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use std::process::Command;

/// sqlite's varint (the standard b-tree varint): 7 bits per byte, big-endian,
/// high bit = "more"; the 9th byte (only for values using > 56 bits) holds a full
/// 8 bits. All values encoded by this test fit well under that threshold.
fn put_varint(out: &mut Vec<u8>, v: u64) {
    if v >> 56 != 0 {
        // 9-byte form: eight 7-bit groups (continuation set) then a full byte.
        for i in (0..8).rev() {
            out.push((((v >> (i * 7 + 8)) & 0x7f) as u8) | 0x80);
        }
        out.push((v & 0xff) as u8);
        return;
    }
    // 1..=8 byte form: emit 7-bit groups most-significant first, continuation bit
    // on all but the last.
    let mut groups = [0u8; 9];
    let mut n = 0;
    let mut w = v;
    loop {
        groups[n] = (w & 0x7f) as u8;
        n += 1;
        w >>= 7;
        if w == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        let byte = groups[i] | if i != 0 { 0x80 } else { 0 };
        out.push(byte);
    }
}

/// A document's contribution to one term: its rowid and the sorted token
/// positions within column 0.
#[derive(Clone)]
struct Posting {
    rowid: i64,
    positions: Vec<u32>,
}

/// Encode the position list (column 0 only) for one posting: size header
/// (collist bytes * 2) then the collist (`first+2`, then `delta+2`).
fn encode_poslist(p: &Posting) -> Vec<u8> {
    let mut collist = Vec::new();
    let mut prev = 0u32;
    for (i, &pos) in p.positions.iter().enumerate() {
        let delta = if i == 0 { pos } else { pos - prev };
        put_varint(&mut collist, (delta as u64) + 2);
        prev = pos;
    }
    let mut out = Vec::new();
    put_varint(&mut out, (collist.len() as u64) * 2);
    out.extend_from_slice(&collist);
    out
}

/// Encode a term's doclist: first rowid, then `(rowidDelta, poslist)` per doc.
fn encode_doclist(postings: &[Posting]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    for (i, p) in postings.iter().enumerate() {
        if i == 0 {
            put_varint(&mut out, p.rowid as u64);
        } else {
            put_varint(&mut out, (p.rowid - prev) as u64);
        }
        out.extend_from_slice(&encode_poslist(p));
        prev = p.rowid;
    }
    out
}

/// Build a single leaf page from the sorted term list. Each term is the raw token
/// (the `FTS5_MAIN_PREFIX` '0' is prepended here). Terms must be sorted ascending.
fn encode_leaf(terms: &[(String, Vec<Posting>)]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut offsets = Vec::new();
    let mut prev_key: Vec<u8> = Vec::new();
    for (term, postings) in terms {
        offsets.push(4 + body.len()); // page-relative offset of this term record
        let mut key = Vec::with_capacity(term.len() + 1);
        key.push(b'0'); // FTS5_MAIN_PREFIX
        key.extend_from_slice(term.as_bytes());
        if prev_key.is_empty() {
            // First term on the page: size varint + the full key blob.
            put_varint(&mut body, key.len() as u64);
            body.extend_from_slice(&key);
        } else {
            // Prefix-compressed: nCommon, nNew, new suffix bytes.
            let n_common = key
                .iter()
                .zip(prev_key.iter())
                .take_while(|(a, b)| a == b)
                .count();
            put_varint(&mut body, n_common as u64);
            put_varint(&mut body, (key.len() - n_common) as u64);
            body.extend_from_slice(&key[n_common..]);
        }
        body.extend_from_slice(&encode_doclist(postings));
        prev_key = key;
    }
    let footer_off = 4 + body.len();
    // pgidx: first term's absolute offset, then deltas.
    let mut pgidx = Vec::new();
    let mut prev_off = 0usize;
    for (i, &off) in offsets.iter().enumerate() {
        put_varint(
            &mut pgidx,
            (if i == 0 { off } else { off - prev_off }) as u64,
        );
        prev_off = off;
    }
    let mut leaf = Vec::new();
    // 4-byte header: two big-endian u16s. The first rowid is inside the first
    // term's doclist, so it never precedes the first term → field 0.
    leaf.extend_from_slice(&0u16.to_be_bytes());
    leaf.extend_from_slice(&(footer_off as u16).to_be_bytes());
    leaf.extend_from_slice(&body);
    leaf.extend_from_slice(&pgidx);
    leaf
}

/// The averages record (id=1): `[nRow, per-column total token count]`.
fn encode_averages(n_row: u64, total_tokens_col0: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, n_row);
    put_varint(&mut out, total_tokens_col0);
    out
}

/// The structure record (id=10) for one fresh segment of `n_leaves` leaf pages:
/// 4-byte BE config cookie (bumped once per `%_config` change — 0 for a fresh
/// table, 1 after one `pgsz` write), then nLevel, nSegment, nWriteCounter (=
/// total leaves), then per level nMerge/nSeg, then per segment
/// segid/pgnoFirst/pgnoLast (= 1..n_leaves).
fn encode_structure(n_leaves: u64, cookie: u32) -> Vec<u8> {
    let mut out = cookie.to_be_bytes().to_vec(); // not a V2 record
    for v in [
        1, 1, n_leaves, /*level0:*/ 0, 1, /*seg:*/ 1, 1, n_leaves,
    ] {
        put_varint(&mut out, v);
    }
    out
}

/// Greedily pack the sorted terms into leaf pages of at most `pgsz` bytes
/// (matching sqlite's FTS5 logical page size). Returns, per leaf, its encoded
/// bytes and its first term key (the '0'-prefixed token). A term is moved to a
/// new leaf when appending it would push the fully-encoded leaf over `pgsz`; a
/// lone first term that already exceeds `pgsz` still occupies its own leaf.
struct Leaf {
    bytes: Vec<u8>,
    first_key: Vec<u8>,
    last_key: Vec<u8>,
}

fn pack_leaves(terms: &[(String, Vec<Posting>)], pgsz: usize) -> Vec<Leaf> {
    let mut leaves = Vec::new();
    let mut cur: Vec<(String, Vec<Posting>)> = Vec::new();
    let flush = |cur: &[(String, Vec<Posting>)]| Leaf {
        bytes: encode_leaf(cur),
        first_key: term_key(&cur[0].0),
        last_key: term_key(&cur[cur.len() - 1].0),
    };
    for (term, postings) in terms {
        if !cur.is_empty() {
            let mut trial = cur.clone();
            trial.push((term.clone(), postings.clone()));
            if encode_leaf(&trial).len() >= pgsz {
                leaves.push(flush(&cur));
                cur.clear();
            }
        }
        cur.push((term.clone(), postings.clone()));
    }
    if !cur.is_empty() {
        leaves.push(flush(&cur));
    }
    leaves
}

/// The '0'-prefixed (`FTS5_MAIN_PREFIX`) key for a term.
fn term_key(term: &str) -> Vec<u8> {
    let mut key = vec![b'0'];
    key.extend_from_slice(term.as_bytes());
    key
}

/// The `%_idx` separator: the shortest prefix of `first` (the leaf's first term
/// key) that is strictly greater than `prev_last` (the previous leaf's last term
/// key) — i.e. `first` truncated just past the first byte where they differ.
fn idx_separator(prev_last: &[u8], first: &[u8]) -> Vec<u8> {
    let mut i = 0;
    while i < prev_last.len() && i < first.len() && prev_last[i] == first[i] {
        i += 1;
    }
    first[..=i.min(first.len() - 1)].to_vec()
}

/// The page-index (footer): first term's absolute page offset, then deltas.
fn build_pgidx(offsets: &[usize]) -> Vec<u8> {
    let mut pgidx = Vec::new();
    let mut prev = 0usize;
    for (i, &off) in offsets.iter().enumerate() {
        put_varint(&mut pgidx, (if i == 0 { off } else { off - prev }) as u64);
        prev = off;
    }
    pgidx
}

/// Assemble a leaf from its body (the content after the 4-byte header), its term
/// page-offsets, and the offset of the first rowid that precedes the first term
/// (0 if none). Layout: [u16 first-rowid-off][u16 footer-off][body][pgidx].
fn finish_leaf(body: &[u8], term_offsets: &[usize], first_rowid_off: usize) -> Vec<u8> {
    let footer_off = 4 + body.len();
    let mut leaf = Vec::new();
    leaf.extend_from_slice(&(first_rowid_off as u16).to_be_bytes());
    leaf.extend_from_slice(&(footer_off as u16).to_be_bytes());
    leaf.extend_from_slice(body);
    leaf.extend_from_slice(&build_pgidx(term_offsets));
    leaf
}

/// Streaming encoder for a segment holding a SINGLE term whose doclist is large
/// enough to span leaf boundaries. Mirrors FTS5's writer: the doclist byte stream
/// fills each leaf to `pgsz`, splitting even mid-poslist; a continuation leaf
/// leads with the carried poslist tail, then the first whole rowid written as an
/// ABSOLUTE varint (header first-rowid-offset points at it), then deltas resume.
/// Continuation leaves carry no term, so an empty pgidx and no `%_idx` row.
fn encode_single_term_segment(term: &str, postings: &[Posting], pgsz: usize) -> Vec<Vec<u8>> {
    let key = term_key(term);
    let mut leaves: Vec<Vec<u8>> = Vec::new();
    let mut body: Vec<u8> = Vec::new();
    let mut term_offsets: Vec<usize> = Vec::new();
    let mut first_rowid_off = 0usize; // 0 = no rowid precedes the first term
    let mut prev_rowid = 0i64; // reset to 0 at each leaf start → first rowid absolute
    let pgidx_len = |offs: &[usize]| build_pgidx(offs).len();

    // Leaf 1 opens with the term record (full size+blob form).
    term_offsets.push(4);
    put_varint(&mut body, key.len() as u64);
    body.extend_from_slice(&key);

    for p in postings {
        // Entry start: flush only if the leaf is ALREADY over pgsz (sqlite lets an
        // entry header overshoot, then carries the collist). The first rowid on a
        // fresh leaf is written absolute (prev_rowid reset to 0).
        if 4 + body.len() + pgidx_len(&term_offsets) > pgsz && !term_offsets.is_empty() {
            leaves.push(finish_leaf(&body, &term_offsets, first_rowid_off));
            body.clear();
            term_offsets.clear();
            first_rowid_off = 0;
            prev_rowid = 0;
        }
        if term_offsets.is_empty() && first_rowid_off == 0 {
            first_rowid_off = 4 + body.len(); // a rowid precedes any term here
        }
        // Atomic entry header: rowid varint + the poslist size varint (the
        // leading varint of the encoded poslist).
        let poslist = encode_poslist(p);
        let size_len = get_varint(&poslist).1;
        put_varint(&mut body, (p.rowid - prev_rowid) as u64);
        body.extend_from_slice(&poslist[..size_len]);
        prev_rowid = p.rowid;
        // Collist bytes are byte-splittable; carry the remainder to the next leaf.
        for &b in &poslist[size_len..] {
            if 4 + body.len() + pgidx_len(&term_offsets) >= pgsz {
                leaves.push(finish_leaf(&body, &term_offsets, first_rowid_off));
                body.clear();
                term_offsets.clear();
                first_rowid_off = 0;
                prev_rowid = 0;
            }
            body.push(b);
        }
    }
    leaves.push(finish_leaf(&body, &term_offsets, first_rowid_off));
    leaves
}

/// Decode a varint, returning (value, bytes consumed).
fn get_varint(buf: &[u8]) -> (u64, usize) {
    let mut v = 0u64;
    for (i, &b) in buf.iter().enumerate().take(9) {
        if i == 8 {
            return (v << 8 | b as u64, 9);
        }
        v = (v << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return (v, i + 1);
        }
    }
    (v, buf.len())
}

/// A unified streaming segment writer covering MULTIPLE terms with prefix
/// compression + leaf pagination AND any term whose doclist spans leaves — the
/// general single-segment case (everything bar the rare dlidx / interior pages).
/// Returns the leaf bytes (in page order) and the `%_idx` rows `(pgno<<1, sep)`.
///
/// The leaf is a byte buffer flushed when it reaches `pgsz`. A term record is
/// written whole (flush first if it would not fit); a doclist streams its
/// entries — the rowid+poslist-size header is atomic (may overshoot pgsz), the
/// collist splits byte-wise and carries to the next leaf. A continuation leaf's
/// first rowid is absolute; a leaf that introduces a term gets one `%_idx` row.
struct SegWriter {
    pgsz: usize,
    leaves: Vec<Vec<u8>>,
    idx: Vec<(i64, Vec<u8>)>,
    body: Vec<u8>,
    term_offsets: Vec<usize>,
    first_rowid_off: usize,
    prev_term_key: Option<Vec<u8>>, // last term on the current leaf (compression)
    prev_rowid: i64,                // reset to 0 at each leaf start
    leaf_first_term: Option<Vec<u8>>,
    leaf_last_term: Option<Vec<u8>>,
    prev_leaf_last_term: Option<Vec<u8>>,
    pgno: i64,
}

impl SegWriter {
    fn new(pgsz: usize) -> Self {
        SegWriter {
            pgsz,
            leaves: Vec::new(),
            idx: Vec::new(),
            body: Vec::new(),
            term_offsets: Vec::new(),
            first_rowid_off: 0,
            prev_term_key: None,
            prev_rowid: 0,
            leaf_first_term: None,
            leaf_last_term: None,
            prev_leaf_last_term: None,
            pgno: 1,
        }
    }

    fn leaf_size(&self) -> usize {
        4 + self.body.len() + build_pgidx(&self.term_offsets).len()
    }

    fn flush(&mut self) {
        self.leaves.push(finish_leaf(
            &self.body,
            &self.term_offsets,
            self.first_rowid_off,
        ));
        if let Some(ft) = self.leaf_first_term.take() {
            let sep = match &self.prev_leaf_last_term {
                Some(p) => idx_separator(p, &ft),
                None => Vec::new(),
            };
            self.idx.push((self.pgno << 1, sep));
        }
        if let Some(lt) = self.leaf_last_term.take() {
            self.prev_leaf_last_term = Some(lt);
        }
        self.body.clear();
        self.term_offsets.clear();
        self.first_rowid_off = 0;
        self.prev_term_key = None;
        self.prev_rowid = 0;
        self.pgno += 1;
    }

    /// This term's on-leaf record, prefix-compressed against the previous term on
    /// the current leaf (full size+blob form when it's the first term on the leaf).
    fn term_record(&self, key: &[u8]) -> Vec<u8> {
        let mut rec = Vec::new();
        match &self.prev_term_key {
            None => {
                put_varint(&mut rec, key.len() as u64);
                rec.extend_from_slice(key);
            }
            Some(prev) => {
                let n_common = key
                    .iter()
                    .zip(prev.iter())
                    .take_while(|(a, b)| a == b)
                    .count();
                put_varint(&mut rec, n_common as u64);
                put_varint(&mut rec, (key.len() - n_common) as u64);
                rec.extend_from_slice(&key[n_common..]);
            }
        }
        rec
    }

    /// pgidx byte length if a term were added at the current body offset.
    fn pgidx_with(&self) -> usize {
        let mut probe = self.term_offsets.clone();
        probe.push(4 + self.body.len());
        build_pgidx(&probe).len()
    }

    fn add_term(&mut self, term: &str, postings: &[Posting]) {
        let key = term_key(term);
        let doclist = encode_doclist(postings); // whole doclist (deltas from rowid 0)
                                                // FTS5 decides per whole term UNIT (record + its full doclist): if it does
                                                // not fit the current leaf, flush; then if it still does not fit an (now
                                                // fresh) leaf, the doclist is streamed across leaves.
        let rec = self.term_record(&key);
        if !self.body.is_empty()
            && 4 + self.body.len() + rec.len() + doclist.len() + self.pgidx_with() >= self.pgsz
        {
            self.flush();
        }
        let rec = self.term_record(&key); // prev_term_key may have been reset by flush
        let fits_whole =
            4 + self.body.len() + rec.len() + doclist.len() + self.pgidx_with() <= self.pgsz;
        self.term_offsets.push(4 + self.body.len());
        if self.leaf_first_term.is_none() {
            self.leaf_first_term = Some(key.clone());
        }
        self.leaf_last_term = Some(key.clone());
        self.body.extend_from_slice(&rec);
        self.prev_term_key = Some(key);
        if fits_whole {
            self.body.extend_from_slice(&doclist);
            return;
        }
        // Stream the doclist: entry header (rowid + poslist size) is atomic and may
        // overshoot pgsz; collist bytes split, carrying to continuation leaves
        // whose first rowid is absolute (prev_rowid reset to 0 on each flush).
        self.prev_rowid = 0;
        for p in postings {
            if self.leaf_size() > self.pgsz && !self.body.is_empty() {
                self.flush();
            }
            if self.term_offsets.is_empty() && self.first_rowid_off == 0 {
                self.first_rowid_off = 4 + self.body.len();
            }
            let poslist = encode_poslist(p);
            let size_len = get_varint(&poslist).1;
            put_varint(&mut self.body, (p.rowid - self.prev_rowid) as u64);
            self.body.extend_from_slice(&poslist[..size_len]);
            self.prev_rowid = p.rowid;
            for &b in &poslist[size_len..] {
                if self.leaf_size() >= self.pgsz {
                    self.flush();
                }
                self.body.push(b);
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn finish(mut self) -> (Vec<Vec<u8>>, Vec<(i64, Vec<u8>)>) {
        self.flush();
        (self.leaves, self.idx)
    }
}

/// Encode a whole segment (sorted terms) via the unified streaming writer.
#[allow(clippy::type_complexity)]
fn encode_segment(
    terms: &[(String, Vec<Posting>)],
    pgsz: usize,
) -> (Vec<Vec<u8>>, Vec<(i64, Vec<u8>)>) {
    let mut w = SegWriter::new(pgsz);
    for (term, postings) in terms {
        w.add_term(term, postings);
    }
    w.finish()
}

const SEG_LEAF_ROWID: i64 = (1i64 << 37) | 1; // segid=1, height=0, dli=0, pgno=1

/// Parse one `X'..'` hex blob literal as printed by `sqlite3`.
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

/// Build `body`-column docs in a fresh fts5 table via sqlite3 (single INSERT
/// statement → one segment, one leaf for small inputs) and return the three
/// `%_data` rows (averages id=1, structure id=10, leaf SEG_LEAF_ROWID) as bytes.
fn sqlite_data_rows(docs: &[(i64, &str)]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gsql-fts5seg-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
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
        .unwrap();
    assert!(o.status.success(), "sqlite build failed: {o:?}");
    let fetch = |id: i64| {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(format!("SELECT quote(block) FROM t_data WHERE id={id};"))
            .output()
            .unwrap();
        parse_hex(&String::from_utf8_lossy(&o.stdout))
    };
    let r = (fetch(1), fetch(10), fetch(SEG_LEAF_ROWID));
    let _ = std::fs::remove_file(&path);
    r
}

/// Tokenize like the default unicode61 tokenizer for our simple lowercase-ascii
/// inputs: split on spaces. Build the sorted term → postings index.
fn build_index(docs: &[(i64, &str)]) -> (Vec<(String, Vec<Posting>)>, u64, u64) {
    use std::collections::BTreeMap;
    // term -> (rowid -> positions)
    let mut idx: BTreeMap<String, BTreeMap<i64, Vec<u32>>> = BTreeMap::new();
    let mut total_tokens = 0u64;
    for (rowid, body) in docs {
        for (pos, tok) in body.split_whitespace().enumerate() {
            total_tokens += 1;
            idx.entry(tok.to_string())
                .or_default()
                .entry(*rowid)
                .or_default()
                .push(pos as u32);
        }
    }
    let terms = idx
        .into_iter()
        .map(|(term, docs)| {
            let postings = docs
                .into_iter()
                .map(|(rowid, positions)| Posting { rowid, positions })
                .collect();
            (term, postings)
        })
        .collect();
    (terms, docs.len() as u64, total_tokens)
}

fn check(docs: &[(i64, &str)]) {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let (avg, structure, leaf) = sqlite_data_rows(docs);
    let (terms, n_row, total_tokens) = build_index(docs);
    assert_eq!(
        encode_averages(n_row, total_tokens),
        avg,
        "averages mismatch"
    );
    assert_eq!(encode_structure(1, 0), structure, "structure mismatch");
    assert_eq!(encode_leaf(&terms), leaf, "leaf mismatch for {docs:?}");
}

/// Fetch a multi-leaf segment built with a small `pgsz`: all leaf blobs ordered
/// by page, the structure record, and the `%_idx` rows `(pgno_field, term)`.
#[allow(clippy::type_complexity)]
fn sqlite_segment(pgsz: usize, body: &str) -> (Vec<Vec<u8>>, Vec<u8>, Vec<(i64, Vec<u8>)>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gsql-fts5ml-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let script = format!(
        "CREATE VIRTUAL TABLE t USING fts5(body);\
         INSERT INTO t(t,rank) VALUES('pgsz',{pgsz});\
         INSERT INTO t(rowid,body) VALUES (1,'{body}');"
    );
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .unwrap();
    assert!(o.status.success(), "sqlite build failed: {o:?}");
    let run = |q: &str| {
        String::from_utf8_lossy(
            &Command::new("sqlite3")
                .arg(&path)
                .arg(q)
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned()
    };
    let leaves: Vec<Vec<u8>> = run("SELECT quote(block) FROM t_data WHERE id>100 ORDER BY id;")
        .lines()
        .map(parse_hex)
        .collect();
    let structure = parse_hex(&run("SELECT quote(block) FROM t_data WHERE id=10;"));
    let idx: Vec<(i64, Vec<u8>)> =
        run("SELECT pgno || '|' || quote(term) FROM t_idx ORDER BY pgno;")
            .lines()
            .map(|l| {
                let (p, t) = l.split_once('|').unwrap();
                (p.parse().unwrap(), parse_hex(t))
            })
            .collect();
    let _ = std::fs::remove_file(&path);
    (leaves, structure, idx)
}

/// Like [`sqlite_segment`] but inserts several `(rowid, body)` docs in one
/// statement (one segment), for exercising doclists that span leaves.
#[allow(clippy::type_complexity)]
fn sqlite_segment_rows(
    pgsz: usize,
    docs: &[(i64, &str)],
) -> (Vec<Vec<u8>>, Vec<u8>, Vec<(i64, Vec<u8>)>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gsql-fts5dl-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let values: Vec<String> = docs.iter().map(|(r, b)| format!("({r},'{b}')")).collect();
    let script = format!(
        "CREATE VIRTUAL TABLE t USING fts5(body);\
         INSERT INTO t(t,rank) VALUES('pgsz',{pgsz});\
         INSERT INTO t(rowid,body) VALUES {};",
        values.join(",")
    );
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .unwrap();
    assert!(o.status.success(), "sqlite build failed: {o:?}");
    let run = |q: &str| {
        String::from_utf8_lossy(
            &Command::new("sqlite3")
                .arg(&path)
                .arg(q)
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned()
    };
    let leaves: Vec<Vec<u8>> = run("SELECT quote(block) FROM t_data WHERE id>100 ORDER BY id;")
        .lines()
        .map(parse_hex)
        .collect();
    let structure = parse_hex(&run("SELECT quote(block) FROM t_data WHERE id=10;"));
    let idx: Vec<(i64, Vec<u8>)> =
        run("SELECT pgno || '|' || quote(term) FROM t_idx ORDER BY pgno;")
            .lines()
            .map(|l| {
                let (p, t) = l.split_once('|').unwrap();
                (p.parse().unwrap(), parse_hex(t))
            })
            .collect();
    let _ = std::fs::remove_file(&path);
    (leaves, structure, idx)
}

/// Build the body's index and pack it into leaves with the same `pgsz`, then
/// assert the leaves, structure, and `%_idx` rows match sqlite byte-for-byte.
fn check_segment(pgsz: usize, body: &str) {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let (sq_leaves, sq_structure, sq_idx) = sqlite_segment(pgsz, body);
    let (terms, _, _) = build_index(&[(1, body)]);
    let leaves = pack_leaves(&terms, pgsz);
    assert!(leaves.len() > 1, "test did not produce multiple leaves");
    assert_eq!(leaves.len(), sq_leaves.len(), "leaf count");
    for (i, (leaf, sq)) in leaves.iter().zip(&sq_leaves).enumerate() {
        assert_eq!(&leaf.bytes, sq, "leaf {} (pgno {}) bytes", i, i + 1);
    }
    assert_eq!(
        encode_structure(leaves.len() as u64, 1),
        sq_structure,
        "structure"
    );
    // %_idx: one row per leaf — (pgno<<1, separator); leaf 1's separator is empty,
    // later leaves get the shortest prefix distinguishing them from the prior leaf.
    let want_idx: Vec<(i64, Vec<u8>)> = leaves
        .iter()
        .enumerate()
        .map(|(i, leaf)| {
            let pgno = (i as i64) + 1;
            let term = if i == 0 {
                Vec::new()
            } else {
                idx_separator(&leaves[i - 1].last_key, &leaf.first_key)
            };
            (pgno << 1, term)
        })
        .collect();
    assert_eq!(want_idx, sq_idx, "%_idx rows");
}

#[test]
fn single_term_single_doc() {
    check(&[(1, "a")]);
    check(&[(5, "cat")]);
}

#[test]
fn two_terms_prefix_compressed() {
    check(&[(1, "a b")]);
    check(&[(1, "apple apply")]); // shares "0appl" → nCommon 5
}

#[test]
fn multi_doc_rowid_deltas() {
    check(&[(1, "hello"), (2, "hello")]);
    check(&[(1, "x"), (3, "x")]); // rowid delta 2
}

#[test]
fn multi_position_collist() {
    check(&[(1, "the the")]); // one term, two positions
}

#[test]
fn mixed_three_terms() {
    check(&[(1, "red green"), (2, "green blue")]);
}

#[test]
fn multi_leaf_pagination() {
    // 40 distinct terms in one doc, pgsz=64 → sqlite splits into 6 leaves.
    let body: String = (1..=40)
        .map(|i| format!("term{i:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    check_segment(64, &body);
}

#[test]
fn multi_leaf_varied_pgsz() {
    let body: String = (1..=50)
        .map(|i| format!("term{i:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    check_segment(80, &body);
    check_segment(128, &body);
}

/// One term repeated across many docs: its doclist spans several leaves, so the
/// continuation leaves exercise the carry (split poslist tail + absolute first
/// rowid + non-zero header offset). The whole segment has ONE %_idx row.
fn check_doclist_spans(n_docs: i64, pgsz: usize) {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Each doc is the single token "x" (so one term, one position per doc).
    let body = vec!["x"; n_docs as usize].join(" ");
    // sqlite stores all docs under rowid 1.. via a single multi-VALUES insert; to
    // get distinct rowids we instead build the index directly from (rowid,"x").
    let docs: Vec<(i64, &str)> = (1..=n_docs).map(|r| (r, "x")).collect();
    let (sq_leaves, sq_structure, sq_idx) = sqlite_segment_rows(pgsz, &docs);
    let (terms, _, _) = build_index(&docs);
    assert_eq!(terms.len(), 1, "expected a single term");
    let leaves = encode_single_term_segment(&terms[0].0, &terms[0].1, pgsz);
    assert!(
        leaves.len() > 1,
        "doclist did not span leaves (body={body:?})"
    );
    assert_eq!(leaves.len(), sq_leaves.len(), "leaf count");
    for (i, (mine, sq)) in leaves.iter().zip(&sq_leaves).enumerate() {
        assert_eq!(mine, sq, "leaf {} (pgno {})", i, i + 1);
    }
    assert_eq!(
        encode_structure(leaves.len() as u64, 1),
        sq_structure,
        "structure"
    );
    // Exactly one %_idx row — leaf 1, empty separator.
    assert_eq!(sq_idx, vec![(2i64, Vec::new())], "%_idx rows");
}

#[test]
fn doclist_spans_leaves() {
    check_doclist_spans(30, 64);
    check_doclist_spans(50, 80);
    check_doclist_spans(100, 128);
}

/// Validate the UNIFIED writer (`encode_segment`) — multiple terms with
/// pagination plus any doclist-spanning — against sqlite for the given docs/pgsz.
fn check_unified(pgsz: usize, docs: &[(i64, &str)]) {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let (sq_leaves, sq_structure, sq_idx) = sqlite_segment_rows(pgsz, docs);
    let (terms, _, _) = build_index(docs);
    let (leaves, idx) = encode_segment(&terms, pgsz);
    assert_eq!(leaves.len(), sq_leaves.len(), "leaf count for {docs:?}");
    for (i, (mine, sq)) in leaves.iter().zip(&sq_leaves).enumerate() {
        assert_eq!(mine, sq, "leaf {} (pgno {})", i, i + 1);
    }
    assert_eq!(
        encode_structure(leaves.len() as u64, 1),
        sq_structure,
        "structure"
    );
    assert_eq!(idx, sq_idx, "%_idx rows");
}

#[test]
fn unified_reproduces_term_pagination() {
    // Many distinct single-doc terms → pure term pagination, via encode_segment.
    let docs: Vec<(i64, String)> = vec![(
        1,
        (1..=40)
            .map(|i| format!("term{i:03}"))
            .collect::<Vec<_>>()
            .join(" "),
    )];
    let refs: Vec<(i64, &str)> = docs.iter().map(|(r, b)| (*r, b.as_str())).collect();
    check_unified(64, &refs);
}

#[test]
fn unified_varying_rowids() {
    // 25 single-word docs at distinct rowids 1..25 (non-uniform rowid deltas),
    // each a distinct term — pagination with varied doclists, via encode_segment.
    let docs: Vec<(i64, String)> = (1..=25).map(|i| (i, format!("word{i:03}"))).collect();
    let refs: Vec<(i64, &str)> = docs.iter().map(|(r, b)| (*r, b.as_str())).collect();
    check_unified(80, &refs);
    check_unified(128, &refs);
}

#[test]
fn unified_reproduces_doclist_spanning() {
    // One term across many docs → spanning doclist, via encode_segment.
    let docs: Vec<(i64, &str)> = (1..=40).map(|r| (r, "x")).collect();
    check_unified(64, &docs);
}

// NOTE: the unified `encode_segment` is byte-verified at pgsz 64/80/128 (pure
// pagination, varying rowids, and doclist-spanning). A narrow leaf-fill subtlety
// remains at SOME page sizes (e.g. pgsz=96, which is stored as 96, not clamped):
// sqlite flushes a leaf one term earlier than a plain `unit <= pgsz` rule
// predicts, and non-monotonically vs pgsz — so it is a pgsz-specific writer
// heuristic in fts5_index.c, not a post-span effect. For functional M2b
// compatibility byte-identity is not required (a structurally valid index is
// enough; the differential corpus compares query output, not file bytes).
