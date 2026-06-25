//! FTS5 `%_data`/`%_idx` segment-index encoder (roadmap D2e-M2) and the
//! multi-leaf doclist reader (roadmap D2b-1/D2b-3).
//!
//! graphite stores its FTS5 documents in the `<name>_content` shadow table and
//! rebuilds the inverted index from them on each write — a bulk rebuild, like the
//! R-Tree. This module turns a set of documents into the byte-compatible segment
//! records sqlite's FTS5 reads: the structure record, the averages record, the
//! leaf pages (with prefix-compressed terms, doclists, and multi-column position
//! lists), and the `%_idx` term→leaf index.
//!
//! The leaf/doclist byte format is verified against sqlite 3.50.4 in
//! `tests/fts5_segment.rs`. Functional `MATCH` compatibility needs a structurally
//! valid index, not byte-identical pages, so the leaf-fill heuristic here is the
//! simple `pgsz` rule (sqlite's exact heuristic differs at some page sizes — that
//! only affects byte-identity, not readability). Large single-term doclists that
//! would need a doclist-index page are not yet emitted.
//!
//! Wired into the executor: `fts5_create_storage` builds the five shadow tables,
//! the vtab store's backing table is `_content` for fts5, and `fts5_rebuild_index`
//! re-derives the segment from the documents after every write.
//!
//! The read path (`decode_term`, D2b-1/D2b-3) is the exact inverse of the writer:
//! given a segment's leaf blobs (in page order) and a term, it walks the
//! page-index, reconstructs the prefix-compressed term key, and decodes the
//! matching doclist into postings (docids + per-column positions). It covers
//! multi-leaf height-0 segments — term pagination AND a doclist that spans
//! leaves — and is wired into `MATCH` (D2b-2) via `lookup_term_rowids`, which the
//! executor calls to index-route a single bare-term query; see the section
//! comment below.

use alloc::vec::Vec;

use crate::util::varint;

/// Test-only counter of how many times [`lookup_term_rowids`] actually SERVED a
/// query from the index (returned `Some`). In-crate unit tests read it to prove a
/// bare-term `MATCH` took the index route rather than the document scan.
#[cfg(test)]
pub(crate) static INDEX_ROUTE_HITS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// `FTS5_MAIN_PREFIX` — every term in the main index is stored prefixed with '0'.
const MAIN_PREFIX: u8 = b'0';

/// `%_data` rowid of the averages record.
pub(crate) const AVERAGES_ROWID: i64 = 1;
/// `%_data` rowid of the structure record.
pub(crate) const STRUCTURE_ROWID: i64 = 10;

/// The `%_data` rowid of leaf page `pgno` in segment `segid` (height 0).
pub(crate) fn segment_leaf_rowid(segid: i64, pgno: i64) -> i64 {
    (segid << 37) | pgno
}

/// Append the sqlite varint encoding of `v` to `out`.
fn put_varint(out: &mut Vec<u8>, v: u64) {
    let mut buf = [0u8; varint::MAX_LEN];
    let n = varint::encode(v, &mut buf);
    out.extend_from_slice(&buf[..n]);
}

/// One document's contribution to a term: its rowid and, per column, the sorted
/// token positions (`cols[c]` empty if the term does not occur in column `c`).
pub(crate) struct Posting {
    pub rowid: i64,
    pub cols: Vec<Vec<u32>>,
}

/// `[ (first offset + 2), (delta + 2)… ]` for one column's positions.
fn collist(positions: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0u32;
    for (i, &pos) in positions.iter().enumerate() {
        put_varint(
            &mut out,
            ((if i == 0 { pos } else { pos - prev }) as u64) + 2,
        );
        prev = pos;
    }
    out
}

/// A posting's position list: `[size][col0 collist]([0x01][col][collist])*`,
/// where `size` is the content byte length × 2. Positions are per-column.
fn poslist(p: &Posting) -> Vec<u8> {
    let mut content = Vec::new();
    for (c, positions) in p.cols.iter().enumerate() {
        if positions.is_empty() {
            continue;
        }
        if c != 0 {
            content.push(0x01);
            put_varint(&mut content, c as u64);
        }
        content.extend_from_slice(&collist(positions));
    }
    let mut out = Vec::new();
    put_varint(&mut out, (content.len() as u64) * 2);
    out.extend_from_slice(&content);
    out
}

/// A term's doclist: `[first rowid][ (rowid delta)(poslist) ]*` (deltas from 0).
fn doclist(postings: &[Posting]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    for (i, p) in postings.iter().enumerate() {
        put_varint(
            &mut out,
            (if i == 0 { p.rowid } else { p.rowid - prev }) as u64,
        );
        out.extend_from_slice(&poslist(p));
        prev = p.rowid;
    }
    out
}

/// The '0'-prefixed key stored for a term.
fn term_key(term: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(term.len() + 1);
    key.push(MAIN_PREFIX);
    key.extend_from_slice(term);
    key
}

/// The page-index footer: the first term's absolute offset then deltas.
fn pgidx(offsets: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0usize;
    for (i, &off) in offsets.iter().enumerate() {
        put_varint(&mut out, (if i == 0 { off } else { off - prev }) as u64);
        prev = off;
    }
    out
}

/// The shortest prefix of `first` strictly greater than `prev_last` (the `%_idx`
/// separator) — `first` truncated just past the first byte where they differ.
fn separator(prev_last: &[u8], first: &[u8]) -> Vec<u8> {
    let mut i = 0;
    while i < prev_last.len() && i < first.len() && prev_last[i] == first[i] {
        i += 1;
    }
    first[..=i.min(first.len() - 1)].to_vec()
}

/// A `%_idx` row: `(segid, term-separator, pgno_field)` where
/// `pgno_field = (leaf_pgno << 1) | doclist_index_flag`.
pub(crate) struct IdxRow {
    pub segid: i64,
    pub term: Vec<u8>,
    pub pgno: i64,
}

/// The streaming leaf writer (see the module doc and `tests/fts5_segment.rs`).
struct SegWriter {
    pgsz: usize,
    segid: i64,
    leaves: Vec<Vec<u8>>,
    idx: Vec<IdxRow>,
    body: Vec<u8>,
    term_offsets: Vec<usize>,
    first_rowid_off: usize,
    prev_term_key: Option<Vec<u8>>,
    prev_rowid: i64,
    leaf_first_term: Option<Vec<u8>>,
    leaf_last_term: Option<Vec<u8>>,
    prev_leaf_last_term: Option<Vec<u8>>,
    pgno: i64,
}

impl SegWriter {
    fn new(pgsz: usize, segid: i64) -> Self {
        SegWriter {
            pgsz,
            segid,
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
        4 + self.body.len() + pgidx(&self.term_offsets).len()
    }

    fn finish_leaf(&self) -> Vec<u8> {
        let footer_off = 4 + self.body.len();
        let mut leaf = Vec::new();
        leaf.extend_from_slice(&(self.first_rowid_off as u16).to_be_bytes());
        leaf.extend_from_slice(&(footer_off as u16).to_be_bytes());
        leaf.extend_from_slice(&self.body);
        leaf.extend_from_slice(&pgidx(&self.term_offsets));
        leaf
    }

    fn flush(&mut self) {
        self.leaves.push(self.finish_leaf());
        if let Some(ft) = self.leaf_first_term.take() {
            let term = match &self.prev_leaf_last_term {
                Some(p) => separator(p, &ft),
                None => Vec::new(),
            };
            self.idx.push(IdxRow {
                segid: self.segid,
                term,
                pgno: self.pgno << 1,
            });
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

    fn pgidx_with(&self) -> usize {
        let mut probe = self.term_offsets.clone();
        probe.push(4 + self.body.len());
        pgidx(&probe).len()
    }

    fn add_term(&mut self, term: &[u8], postings: &[Posting]) {
        let key = term_key(term);
        let dl = doclist(postings);
        let rec = self.term_record(&key);
        if !self.body.is_empty()
            && 4 + self.body.len() + rec.len() + dl.len() + self.pgidx_with() >= self.pgsz
        {
            self.flush();
        }
        let rec = self.term_record(&key);
        let fits_whole =
            4 + self.body.len() + rec.len() + dl.len() + self.pgidx_with() <= self.pgsz;
        self.term_offsets.push(4 + self.body.len());
        if self.leaf_first_term.is_none() {
            self.leaf_first_term = Some(key.clone());
        }
        self.leaf_last_term = Some(key.clone());
        self.body.extend_from_slice(&rec);
        self.prev_term_key = Some(key);
        if fits_whole {
            self.body.extend_from_slice(&dl);
            return;
        }
        // Stream the doclist across leaves.
        self.prev_rowid = 0;
        for p in postings {
            if self.leaf_size() > self.pgsz && !self.body.is_empty() {
                self.flush();
            }
            if self.term_offsets.is_empty() && self.first_rowid_off == 0 {
                self.first_rowid_off = 4 + self.body.len();
            }
            let pl = poslist(p);
            let size_len = varint::decode(&pl).map(|(_, n)| n).unwrap_or(1);
            put_varint(&mut self.body, (p.rowid - self.prev_rowid) as u64);
            self.body.extend_from_slice(&pl[..size_len]);
            self.prev_rowid = p.rowid;
            for &b in &pl[size_len..] {
                if self.leaf_size() >= self.pgsz {
                    self.flush();
                }
                self.body.push(b);
            }
        }
    }

    fn finish(mut self) -> (Vec<Vec<u8>>, Vec<IdxRow>) {
        self.flush();
        (self.leaves, self.idx)
    }
}

/// The structure record for one fresh segment of `n_leaves` leaves, with config
/// `cookie`. Empty segment (`n_leaves == 0`) → just the cookie + three zero
/// varints (no level/segment), matching an empty fts5 table.
fn structure(n_leaves: i64, cookie: u32) -> Vec<u8> {
    let mut out = cookie.to_be_bytes().to_vec();
    if n_leaves == 0 {
        out.extend_from_slice(&[0, 0, 0]); // nLevel=0, nSegment=0, nWriteCounter=0
        return out;
    }
    for v in [1, 1, n_leaves as u64, 0, 1, 1, 1, n_leaves as u64] {
        put_varint(&mut out, v);
    }
    out
}

/// The result of building a segment index from a document set.
pub(crate) struct Segment {
    /// `%_data` rows `(id, block)`: averages (id 1), structure (id 10), leaves.
    pub data: Vec<(i64, Vec<u8>)>,
    /// `%_idx` rows.
    pub idx: Vec<IdxRow>,
    /// Per-document `(rowid, docsize-blob)` for `%_docsize` (per-column token
    /// counts as a varint list).
    pub docsize: Vec<(i64, Vec<u8>)>,
}

/// Build the full segment index for `terms` (sorted ascending by raw term bytes)
/// over `n_docs` documents with `ncols` columns. `col_totals[c]` is the total
/// token count in column `c` across all documents; `doc_sizes` is per-document
/// `(rowid, per-column token counts)`. `cookie` is the `%_config` change count.
pub(crate) fn build_segment(
    terms: &[(Vec<u8>, Vec<Posting>)],
    n_docs: u64,
    col_totals: &[u64],
    doc_sizes: &[(i64, Vec<u64>)],
    pgsz: usize,
    cookie: u32,
) -> Segment {
    let segid = 1;
    let (leaves, idx) = {
        let mut w = SegWriter::new(pgsz.max(16), segid);
        for (term, postings) in terms {
            w.add_term(term, postings);
        }
        if terms.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            w.finish()
        }
    };

    let mut data: Vec<(i64, Vec<u8>)> = Vec::new();
    // Averages (id 1): empty when there are no documents, else [nRow, per-col].
    let mut avg = Vec::new();
    if n_docs > 0 {
        put_varint(&mut avg, n_docs);
        for &t in col_totals {
            put_varint(&mut avg, t);
        }
    }
    data.push((AVERAGES_ROWID, avg));
    data.push((STRUCTURE_ROWID, structure(leaves.len() as i64, cookie)));
    for (i, leaf) in leaves.iter().enumerate() {
        data.push((segment_leaf_rowid(segid, i as i64 + 1), leaf.clone()));
    }

    let docsize = doc_sizes
        .iter()
        .map(|(rowid, sizes)| {
            let mut sz = Vec::new();
            for &s in sizes {
                put_varint(&mut sz, s);
            }
            (*rowid, sz)
        })
        .collect();

    Segment { data, idx, docsize }
}

// ---------------------------------------------------------------------------
// D2b: the read path — decode a single-term doclist from the `%_data` leaves.
//
// This is the byte-for-byte inverse of the writer above. It reads the segment's
// leaf pages (height-0 `%_data` rows), walks each leaf's term records (the
// prefix-compressed '0'-prefixed keys, located via the page-index footer), and
// for a matching term decodes its doclist into postings (rowid + per-column
// positions). It is the exact inverse of `add_term`/`doclist`/`poslist`.
//
// Scope: MULTI-LEAF height-0 segments (D2b-3). The decoder handles
//   * a small index whose term and whole doclist fit in one leaf (D2b-1);
//   * TERM PAGINATION — terms spread across several leaves, each leaf with its
//     own term records and page-index footer (found by scanning leaves in page
//     order, which is equivalent to the `%_idx`-guided seek the writer feeds);
//   * DOCLIST SPANNING — a single term whose doclist overflows a leaf and
//     continues on one or more CONTINUATION leaves (no term records of their
//     own: the carried poslist tail leads, then the first WHOLE rowid is written
//     as an ABSOLUTE varint at the leaf header's `first_rowid_off`, after which
//     deltas resume). Postings accumulate across the spanned leaves up to the
//     next term boundary.
//
// Still out of scope (→ `None`, caller falls back to the `_content` scan):
// segment-b-tree INTERIOR pages (`height > 0`) and DOCLIST-INDEX (`dlidx`)
// pages — only reached by a single term spanning ~16+ leaves. `decode_term`
// returns `None` rather than a truncated/wrong doclist for anything it cannot
// fully reconstruct.
//
// The reader is verified end-to-end here (writer→decoder round-trips, incl.
// forced-small `pgsz` multi-leaf segments) and in `tests/fts5_decode.rs` /
// `tests/fts5_decode_multileaf.rs` (decode what `sqlite3` itself wrote). It is
// wired into `MATCH` (D2b-2) via `lookup_term_rowids` below, which the executor
// calls to index-route a single bare-term query (see `fts5_try_index_match` in
// `src/exec/mod.rs`).

/// Read a varint at `buf[*pos..]`, advancing `pos`. `None` on a truncated/empty
/// slice.
fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let (v, n) = varint::decode(buf.get(*pos..)?)?;
    *pos += n;
    Some(v)
}

/// A decoded posting: a document rowid and its per-column token positions
/// (`cols[c]` empty if the term does not occur in column `c`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedPosting {
    pub rowid: i64,
    pub cols: Vec<Vec<u32>>,
}

/// Decode one position list (`[size][col0 collist]([0x01][col][collist])*`)
/// starting at `buf[*pos..]`, advancing `pos` past it. Returns the per-column
/// positions (index = column number). The inverse of [`poslist`].
fn decode_poslist(buf: &[u8], pos: &mut usize) -> Option<Vec<Vec<u32>>> {
    let size2 = read_varint(buf, pos)?;
    let content_len = (size2 / 2) as usize;
    let end = pos.checked_add(content_len)?;
    if end > buf.len() {
        return None;
    }
    let mut cols: Vec<Vec<u32>> = Vec::new();
    let mut col = 0usize;
    let mut p = *pos;
    // Ensure column 0 exists even if the content is empty.
    cols.push(Vec::new());
    while p < end {
        if buf[p] == 0x01 {
            // Column switch: 0x01, varint(column).
            p += 1;
            let c = read_varint(buf, &mut p)? as usize;
            col = c;
            while cols.len() <= col {
                cols.push(Vec::new());
            }
        } else {
            // A collist entry: (pos+2) then (delta+2)*, terminated by the column
            // switch byte 0x01 or the end of the poslist content.
            let raw = read_varint(buf, &mut p)?;
            // 0 and 1 are reserved (0 = end sentinel in a streamed list, 1 =
            // column switch); a stored collist value is always >= 2.
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

/// One term record on a leaf: its full '0'-prefixed `key`, the byte offset where
/// the record itself begins (= the doclist of the PREVIOUS term ends here), and
/// the offset where this term's own doclist begins (just past the record bytes).
struct TermRec {
    key: Vec<u8>,
    rec_start: usize,
    doclist_start: usize,
}

/// A parsed leaf page (inverse of [`SegWriter::finish_leaf`]). Layout:
/// `[u16 first_rowid_off][u16 footer_off][body][pgidx]`; the body holds the
/// doclist bytes and term records, `pgidx` (footer) holds each term record's
/// absolute page offset (first absolute, then deltas).
struct LeafView {
    /// Offset of the first WHOLE rowid on the leaf (`0` = none; a leaf that opens
    /// with a carried poslist tail and resumes the doclist points here).
    first_rowid_off: usize,
    /// Offset where the page-index footer begins (= end of body content).
    footer_off: usize,
    /// The term records on this leaf, in order. Empty ⇒ a CONTINUATION leaf that
    /// only carries the spill of the previous leaf's last term's doclist.
    terms: Vec<TermRec>,
}

/// Parse a leaf page into its header offsets and term records, or `None` if the
/// leaf is structurally malformed. The inverse of [`SegWriter::finish_leaf`]:
/// the two u16 header words, the page-index footer (absolute offset, then
/// deltas), and the prefix-compressed term keys.
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
    // The page-index footer gives each term record's absolute offset.
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
            // First term: [varint keylen][key bytes].
            let keylen = read_varint(leaf, &mut p)? as usize;
            let end = p.checked_add(keylen)?;
            if end > footer_off {
                return None;
            }
            let key = leaf.get(p..end)?.to_vec();
            p = end;
            key
        } else {
            // Prefix-compressed: [varint nCommon][varint nNew][suffix].
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

/// One contiguous run of doclist bytes drawn from a single leaf, plus whether the
/// run BEGINS a fresh (absolute) rowid. A spanning term's doclist is a sequence
/// of these: the originating-leaf tail (`abs_start = false`), then one per
/// continuation leaf (`abs_start = true`, the leaf's first whole rowid is written
/// absolute at `first_rowid_off`; the bytes before it are the carried poslist
/// tail and belong to the *previous* run).
struct DoclistRun<'a> {
    bytes: &'a [u8],
    /// `true` if `bytes` starts at a leaf's `first_rowid_off` (an absolute rowid).
    abs_start: bool,
}

/// Decode a doclist that may span several leaves into postings. `runs` is the
/// ordered list of byte runs (see [`DoclistRun`]); the bytes are logically
/// concatenated, but at the start of every `abs_start` run the rowid resets to
/// absolute (the writer resets `prev_rowid` to 0 on each continuation leaf).
///
/// The inverse of the writer's streamed `doclist`: `[rowid][poslist]` repeated,
/// rowids delta-coded within a run and absolute at each run boundary. A poslist
/// (and even a single collist varint) may straddle a run boundary, so this
/// flattens the runs into one buffer and tracks the byte offsets at which an
/// absolute rowid begins.
fn decode_spanning_doclist(runs: &[DoclistRun]) -> Option<Vec<DecodedPosting>> {
    // Flatten into one buffer, recording the offsets where a rowid is absolute.
    let mut buf: Vec<u8> = Vec::new();
    let mut abs_at: Vec<usize> = Vec::new();
    for run in runs {
        // Only a non-empty absolute run marks a rowid reset; an empty resumed run
        // (e.g. a leaf whose tail reached exactly the next term) carries nothing.
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
        // A rowid is absolute on the first entry of the doclist or at any run
        // boundary recorded in `abs_at`; otherwise it is a delta from the running
        // rowid. (`abs_at` offsets always fall on an entry boundary because the
        // writer only resets `prev_rowid` between whole entries.)
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

/// Gather the byte runs of the doclist for the term at (`start_leaf`, term index
/// `start_ti`) whose doclist begins at `start_off`. The doclist runs forward —
/// possibly spilling across several leaves — until the NEXT term record in the
/// segment (or the end of the segment).
///
/// On the originating leaf the doclist runs from `start_off` to the next term
/// record on that leaf (if any) else its footer. If it reaches the footer the
/// doclist spills. Each spill leaf carries NO term until the boundary leaf:
///
/// * `[4 .. first_rowid_off]` continues the CURRENT entry (carried poslist tail,
///   no new rowid). When `first_rowid_off == 0` the carried tail fills the whole
///   leaf body and `first_rowid_off` is read as the footer.
/// * `[first_rowid_off .. boundary]` resumes with an ABSOLUTE rowid, where
///   `boundary` is the leaf's first term record offset (the spill ends there) or
///   its footer (the spill continues).
///
/// The spill ends at the first term record after ours, anywhere in the segment.
fn gather_doclist_runs<'a>(
    leaves: &'a [&'a [u8]],
    start_leaf: usize,
    start_ti: usize,
    start_off: usize,
    leaf_views: &[LeafView],
) -> Option<Vec<DoclistRun<'a>>> {
    let mut runs: Vec<DoclistRun<'a>> = Vec::new();
    let first_view = &leaf_views[start_leaf];
    // The originating run ends at the next term record on THIS leaf, if any.
    let first_next_term = first_view.terms.get(start_ti + 1).map(|r| r.rec_start);
    let first_end = first_next_term.unwrap_or(first_view.footer_off);
    if first_end < start_off || first_end > first_view.footer_off {
        return None;
    }
    runs.push(DoclistRun {
        bytes: leaves[start_leaf].get(start_off..first_end)?,
        abs_start: true, // first entry of a doclist is always absolute
    });
    if first_next_term.is_some() {
        return Some(runs); // a later term on this leaf bounds the doclist
    }
    // Spill onto following leaves until a term record appears.
    let mut li = start_leaf + 1;
    while li < leaves.len() {
        let view = &leaf_views[li];
        // The carried poslist tail runs from 4 to `first_rowid_off`; when there is
        // no resumed rowid on this leaf (`first_rowid_off == 0`) the tail occupies
        // the whole body up to the boundary (a term record or the footer).
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
            // The resumed absolute-rowid run up to the boundary.
            runs.push(DoclistRun {
                bytes: leaves[li].get(view.first_rowid_off..boundary)?,
                abs_start: true,
            });
        }
        // A term record on this leaf ends the spill.
        if next_term.is_some() {
            break;
        }
        li += 1;
    }
    Some(runs)
}

/// Look up `term` in a set of segment leaf pages and return its decoded postings
/// (docids with per-column positions), or `None` if the term is absent.
///
/// `leaves` are the height-0 `%_data` leaf blobs of a single segment, in page
/// order. The reader decodes a term whether its doclist fits in one leaf or
/// SPANS several (the originating leaf's tail plus continuation leaves; see
/// [`gather_doclist_runs`]). It returns `None` for anything still out of scope —
/// segment-b-tree interior pages or doclist-index pages — so the caller falls
/// back to the document scan rather than reading a truncated doclist. The empty
/// result `Some(vec![])` never occurs — a present term always has at least one
/// posting.
pub(crate) fn decode_term(leaves: &[&[u8]], term: &[u8]) -> Option<Vec<DecodedPosting>> {
    let key = term_key(term);
    // Parse every leaf once. A malformed/unsupported leaf (e.g. an interior page)
    // aborts the whole decode → fall back to the scan.
    let mut views: Vec<LeafView> = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        views.push(parse_leaf(leaf)?);
    }
    for (li, view) in views.iter().enumerate() {
        for (ti, rec) in view.terms.iter().enumerate() {
            if rec.key != key {
                continue;
            }
            // Gather the doclist (which may spill across leaves) up to the next
            // term record in the segment.
            let runs = gather_doclist_runs(leaves, li, ti, rec.doclist_start, &views)?;
            return decode_spanning_doclist(&runs);
        }
    }
    None
}

/// One segment's identity, parsed from the structure record: its `segid` and the
/// inclusive range of height-0 leaf page numbers (`pgno_first..=pgno_last`).
struct SegmentLoc {
    segid: i64,
    pgno_first: i64,
    pgno_last: i64,
}

/// Parse the structure record (the inverse of [`structure`]) and, IF the index
/// holds exactly one height-0 segment, return its location. Returns `None` for
/// the empty index, a multi-segment / multi-level index (a merged or
/// incrementally-updated index that the single-segment leaf reader can't serve),
/// or a malformed record — the caller then falls back to the `%_content` scan.
///
/// Layout: 4-byte BE cookie, then varints `nLevel`, `nSegment`, `nWriteCounter`,
/// then per level `nMerge`, `nSeg`, then per segment `segid`, `pgnoFirst`,
/// `pgnoLast`. graphite always writes a single fresh segment, so this recognizes
/// exactly that shape (one level, one segment) and declines anything else.
fn single_segment(structure: &[u8]) -> Option<SegmentLoc> {
    // The 4-byte config cookie precedes the varint body.
    let mut pos = 4usize;
    let n_level = read_varint(structure, &mut pos)?;
    let n_segment = read_varint(structure, &mut pos)?;
    let _n_write_counter = read_varint(structure, &mut pos)?;
    // Only the simple, single-segment shape is served from the leaf reader.
    if n_level != 1 || n_segment != 1 {
        return None;
    }
    let n_merge = read_varint(structure, &mut pos)?;
    let n_seg = read_varint(structure, &mut pos)?;
    if n_merge != 0 || n_seg != 1 {
        return None;
    }
    let segid = read_varint(structure, &mut pos)? as i64;
    let pgno_first = read_varint(structure, &mut pos)? as i64;
    let pgno_last = read_varint(structure, &mut pos)? as i64;
    if segid <= 0 || pgno_first < 1 || pgno_last < pgno_first {
        return None;
    }
    Some(SegmentLoc {
        segid,
        pgno_first,
        pgno_last,
    })
}

/// Look up `term` in an FTS5 index given its `%_data` rows, returning the rowids
/// of the documents that contain the term (ascending), or `None` if the index
/// shape is one the single-segment leaf reader cannot serve.
///
/// `data` is the `(id, block)` rows of the `%_data` shadow table (the structure
/// record at id 10 plus the height-0 leaves). This is the wiring used by `MATCH`:
/// it parses the structure record, and only when the index is a single height-0
/// segment ([`single_segment`]) does it gather that segment's leaves in page
/// order and call [`decode_term`]. A `None` return (multi-segment index, an
/// interior/doclist-index page, a missing leaf, or a malformed record) tells the
/// caller to fall back to the `%_content` document scan. A present term that is
/// genuinely absent from a servable single-segment index returns `Some(vec![])`.
pub(crate) fn lookup_term_rowids(data: &[(i64, Vec<u8>)], term: &[u8]) -> Option<Vec<i64>> {
    decode_term_in_data(data, term).map(|postings| postings.into_iter().map(|p| p.rowid).collect())
}

/// Look up `term` in a single-segment FTS5 index and return only the rowids of the
/// documents in which it occurs in COLUMN `column` (the position of the column in
/// the table's full column list, indexed from 0), ascending — or `None` if the
/// index shape is one the single-segment leaf reader cannot serve.
///
/// This is the column-scoped sibling of [`lookup_term_rowids`]. The per-column
/// token positions in each [`DecodedPosting`] (the writer records them per
/// column) let it keep exactly the postings whose `cols[column]` is non-empty —
/// the same set the scan's `col:term` predicate matches. A servable index whose
/// term is absent (or never occurs in `column`) yields `Some(vec![])`; an
/// unservable shape yields `None` (caller falls back to the document scan).
pub(crate) fn lookup_term_rowids_in_column(
    data: &[(i64, Vec<u8>)],
    term: &[u8],
    column: usize,
) -> Option<Vec<i64>> {
    decode_term_in_data(data, term).map(|postings| {
        postings
            .into_iter()
            .filter(|p| p.cols.get(column).is_some_and(|c| !c.is_empty()))
            .map(|p| p.rowid)
            .collect()
    })
}

/// Resolve `term`'s postings from a single-segment `%_data` index, or `None` if the
/// shape is unservable. Shared by [`lookup_term_rowids`] and
/// [`lookup_term_rowids_in_column`]: it parses the structure record, gathers the
/// single height-0 segment's leaves in page order, and decodes the term. A
/// servable segment whose term is absent returns `Some(vec![])`.
fn decode_term_in_data(data: &[(i64, Vec<u8>)], term: &[u8]) -> Option<Vec<DecodedPosting>> {
    let structure = &data.iter().find(|(id, _)| *id == STRUCTURE_ROWID)?.1;
    let loc = single_segment(structure)?;
    // Gather the segment's leaves in page order; abort (→ scan) if any is missing.
    let mut leaves: Vec<&[u8]> = Vec::new();
    for pgno in loc.pgno_first..=loc.pgno_last {
        let rid = segment_leaf_rowid(loc.segid, pgno);
        let blob = &data.iter().find(|(id, _)| *id == rid)?.1;
        leaves.push(blob.as_slice());
    }
    #[cfg(test)]
    INDEX_ROUTE_HITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // A servable segment whose term is absent: definitively no matches.
    Some(decode_term(&leaves, term).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::ToString, vec};

    fn p(rowid: i64, cols: &[&[u32]]) -> Posting {
        Posting {
            rowid,
            cols: cols.iter().map(|c| c.to_vec()).collect(),
        }
    }

    #[test]
    fn empty_table_structure_and_averages() {
        let seg = build_segment(&[], 0, &[0], &[], 1000, 0);
        // averages empty, structure = cookie(0) + three zero varints.
        assert_eq!(seg.data[0], (AVERAGES_ROWID, Vec::new()));
        assert_eq!(seg.data[1], (STRUCTURE_ROWID, vec![0, 0, 0, 0, 0, 0, 0]));
        assert_eq!(seg.data.len(), 2); // no leaves
        assert!(seg.idx.is_empty());
    }

    #[test]
    fn single_term_single_doc_matches_known_bytes() {
        // "a" at rowid 1, col0 pos0 → leaf X'0000000A 02 3061 01 02 02 04'.
        let terms = vec![(b"a".to_vec(), vec![p(1, &[&[0]])])];
        let seg = build_segment(&terms, 1, &[1], &[(1, vec![1])], 1000, 0);
        let leaf = &seg
            .data
            .iter()
            .find(|(id, _)| *id == segment_leaf_rowid(1, 1))
            .unwrap()
            .1;
        assert_eq!(
            leaf,
            &vec![0, 0, 0, 0x0A, 0x02, 0x30, 0x61, 0x01, 0x02, 0x02, 0x04]
        );
        // averages X'0101', structure cookie0 + 1-leaf, one idx row (empty sep).
        assert_eq!(seg.data[0].1, vec![0x01, 0x01]);
        assert_eq!(seg.idx.len(), 1);
        assert_eq!(seg.idx[0].pgno, 2);
        assert!(seg.idx[0].term.is_empty());
        assert_eq!(seg.docsize, vec![(1, vec![1])]);
    }

    #[test]
    fn multi_column_poslist_bytes() {
        // "hello" in col0 pos0 and col1 pos0 → poslist content `02 01 01 02`.
        let terms = vec![("hello".to_string().into_bytes(), vec![p(1, &[&[0], &[0]])])];
        let seg = build_segment(&terms, 1, &[1, 1], &[(1, vec![1, 1])], 1000, 0);
        let leaf = &seg
            .data
            .iter()
            .find(|(id, _)| *id == segment_leaf_rowid(1, 1))
            .unwrap()
            .1;
        // sqlite 3.50.4: X'00000011 06 3068656C6C6F 0108020101 02 04'
        let expected = vec![
            0, 0, 0, 0x11, 0x06, 0x30, b'h', b'e', b'l', b'l', b'o', 0x01, 0x08, 0x02, 0x01, 0x01,
            0x02, 0x04,
        ];
        assert_eq!(leaf, &expected);
    }

    // ---- D2b-1 decoder round-trips (writer → decoder) ---------------------

    /// Pull a segment's height-0 leaf blobs out of `seg.data` in page order.
    fn leaves_of(seg: &Segment) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut pgno = 1i64;
        loop {
            let rid = segment_leaf_rowid(1, pgno);
            match seg.data.iter().find(|(id, _)| *id == rid) {
                Some((_, blob)) => out.push(blob.clone()),
                None => break,
            }
            pgno += 1;
        }
        out
    }

    /// The decoded postings for `term` over a freshly built segment's leaves.
    fn decode(seg: &Segment, term: &[u8]) -> Option<Vec<DecodedPosting>> {
        let leaves = leaves_of(seg);
        let refs: Vec<&[u8]> = leaves.iter().map(|l| l.as_slice()).collect();
        decode_term(&refs, term)
    }

    fn dp(rowid: i64, cols: &[&[u32]]) -> DecodedPosting {
        DecodedPosting {
            rowid,
            cols: cols.iter().map(|c| c.to_vec()).collect(),
        }
    }

    #[test]
    fn decode_single_term_single_doc() {
        let terms = vec![(b"a".to_vec(), vec![p(1, &[&[0]])])];
        let seg = build_segment(&terms, 1, &[1], &[(1, vec![1])], 1000, 0);
        assert_eq!(decode(&seg, b"a"), Some(vec![dp(1, &[&[0]])]));
        // Absent term → None.
        assert_eq!(decode(&seg, b"z"), None);
        // A prefix of the only term is not the term itself.
        assert_eq!(decode(&seg, b""), None);
    }

    #[test]
    fn decode_multi_doc_rowid_deltas() {
        // "cat" at rowids 1, 3, 7 (non-uniform deltas), one position each.
        let terms = vec![(
            b"cat".to_vec(),
            vec![p(1, &[&[0]]), p(3, &[&[2]]), p(7, &[&[1]])],
        )];
        let seg = build_segment(
            &terms,
            3,
            &[3],
            &[(1, vec![1]), (3, vec![3]), (7, vec![2])],
            1000,
            0,
        );
        assert_eq!(
            decode(&seg, b"cat"),
            Some(vec![dp(1, &[&[0]]), dp(3, &[&[2]]), dp(7, &[&[1]])])
        );
    }

    #[test]
    fn decode_term_multiple_positions_one_doc() {
        // "the" appears at positions 0 and 2 in rowid 1 (collist with a delta).
        let terms = vec![(b"the".to_vec(), vec![p(1, &[&[0, 2]])])];
        let seg = build_segment(&terms, 1, &[3], &[(1, vec![3])], 1000, 0);
        assert_eq!(decode(&seg, b"the"), Some(vec![dp(1, &[&[0, 2]])]));
    }

    #[test]
    fn decode_prefix_compressed_terms() {
        // "apple" and "apply" share "0appl" (nCommon 5) → the decoder must
        // reconstruct the second key from the first.
        let terms = vec![
            (b"apple".to_vec(), vec![p(1, &[&[0]])]),
            (b"apply".to_vec(), vec![p(2, &[&[0]])]),
        ];
        let seg = build_segment(&terms, 2, &[2], &[(1, vec![1]), (2, vec![1])], 1000, 0);
        assert_eq!(decode(&seg, b"apple"), Some(vec![dp(1, &[&[0]])]));
        assert_eq!(decode(&seg, b"apply"), Some(vec![dp(2, &[&[0]])]));
        // "appl" is a shared prefix, not a stored term.
        assert_eq!(decode(&seg, b"appl"), None);
    }

    #[test]
    fn decode_multi_column_positions() {
        // "hello" in col0 pos0 and col1 pos0; "there" only in col1.
        let terms = vec![
            (b"hello".to_vec(), vec![p(1, &[&[0], &[0]])]),
            (b"there".to_vec(), vec![p(1, &[&[], &[1]])]),
        ];
        let seg = build_segment(&terms, 1, &[1, 2], &[(1, vec![1, 2])], 1000, 0);
        assert_eq!(decode(&seg, b"hello"), Some(vec![dp(1, &[&[0], &[0]])]));
        // "there": col0 empty, col1 has pos 1.
        assert_eq!(decode(&seg, b"there"), Some(vec![dp(1, &[&[], &[1]])]));
    }

    #[test]
    fn decode_many_terms_one_leaf() {
        // A handful of distinct terms, each one doc — all fit in one leaf.
        let words: &[&[u8]] = &[b"alpha", b"beta", b"delta", b"gamma", b"omega"];
        let terms: Vec<(Vec<u8>, Vec<Posting>)> = words
            .iter()
            .enumerate()
            .map(|(i, w)| (w.to_vec(), vec![p(i as i64 + 1, &[&[0]])]))
            .collect();
        let doc_sizes: Vec<(i64, Vec<u64>)> =
            (1..=words.len() as i64).map(|r| (r, vec![1])).collect();
        let seg = build_segment(
            &terms,
            words.len() as u64,
            &[words.len() as u64],
            &doc_sizes,
            1000,
            0,
        );
        for (i, w) in words.iter().enumerate() {
            assert_eq!(
                decode(&seg, w),
                Some(vec![dp(i as i64 + 1, &[&[0]])]),
                "{w:?}"
            );
        }
        assert_eq!(decode(&seg, b"missing"), None);
    }

    // ---- D2b-3 multi-leaf round-trips (writer → decoder) ------------------

    /// Count the height-0 leaves a segment produced (page order).
    fn leaf_count(seg: &Segment) -> usize {
        leaves_of(seg).len()
    }

    #[test]
    fn decode_multi_leaf_term_pagination() {
        // Many distinct single-doc terms with a tiny pgsz force TERM pagination:
        // the terms spread across several leaves, each with its own page-index.
        let n = 40usize;
        let words: Vec<Vec<u8>> = (0..n).map(|i| format!("term{i:03}").into_bytes()).collect();
        let terms: Vec<(Vec<u8>, Vec<Posting>)> = words
            .iter()
            .enumerate()
            .map(|(i, w)| (w.clone(), vec![p(i as i64 + 1, &[&[0]])]))
            .collect();
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n as i64).map(|r| (r, vec![1])).collect();
        let seg = build_segment(&terms, n as u64, &[n as u64], &doc_sizes, 64, 0);
        assert!(leaf_count(&seg) > 1, "pgsz 64 must split into many leaves");
        for (i, w) in words.iter().enumerate() {
            assert_eq!(
                decode(&seg, w),
                Some(vec![dp(i as i64 + 1, &[&[0]])]),
                "term {w:?} on leaf pagination"
            );
        }
        assert_eq!(decode(&seg, b"term999"), None);
    }

    #[test]
    fn decode_doclist_spanning_leaves() {
        // One term across many docs → its doclist overflows a leaf and spills onto
        // continuation leaves (absolute first rowid on each). Decoder must stitch
        // the spanned runs back into the full posting list.
        let n = 40i64;
        let postings: Vec<Posting> = (1..=n).map(|r| p(r, &[&[0]])).collect();
        let terms = vec![(b"x".to_vec(), postings)];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n).map(|r| (r, vec![1])).collect();
        let seg = build_segment(&terms, n as u64, &[n as u64], &doc_sizes, 64, 0);
        assert!(leaf_count(&seg) > 1, "pgsz 64 must span the doclist");
        let want: Vec<DecodedPosting> = (1..=n).map(|r| dp(r, &[&[0]])).collect();
        assert_eq!(decode(&seg, b"x"), Some(want));
        assert_eq!(decode(&seg, b"y"), None);
    }

    #[test]
    fn decode_doclist_spanning_multi_position() {
        // A spanning doclist where each doc has several positions (longer
        // poslists → collist bytes straddle leaf boundaries mid-poslist).
        let n = 30i64;
        let postings: Vec<Posting> = (1..=n).map(|r| p(r, &[&[0, 3, 9, 15]])).collect();
        let terms = vec![(b"w".to_vec(), postings)];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n).map(|r| (r, vec![16])).collect();
        let seg = build_segment(&terms, n as u64, &[(16 * n) as u64], &doc_sizes, 48, 0);
        assert!(leaf_count(&seg) > 1, "pgsz 48 must span the doclist");
        let want: Vec<DecodedPosting> = (1..=n).map(|r| dp(r, &[&[0, 3, 9, 15]])).collect();
        assert_eq!(decode(&seg, b"w"), Some(want));
    }

    #[test]
    fn decode_mixed_pagination_and_spanning() {
        // A segment mixing a heavy spanning term with several light terms, at a
        // small pgsz: the heavy term spans, the light terms paginate. Every term
        // must still decode to its exact posting list.
        let mut terms: Vec<(Vec<u8>, Vec<Posting>)> = Vec::new();
        // "heavy" occurs in docs 1..=25 (spans leaves).
        terms.push((b"heavy".to_vec(), (1..=25).map(|r| p(r, &[&[0]])).collect()));
        // A run of light terms after it, each one doc.
        for i in 0..20 {
            let w = format!("light{i:02}").into_bytes();
            terms.push((w, vec![p(100 + i as i64, &[&[1]])]));
        }
        terms.sort_by(|a, b| a.0.cmp(&b.0));
        // doc sizes are irrelevant to decode; supply a plausible set.
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=120).map(|r| (r, vec![1])).collect();
        let seg = build_segment(&terms, 120, &[120], &doc_sizes, 56, 0);
        assert!(leaf_count(&seg) > 2, "expected several leaves");
        // Verify each term decodes to exactly what was written.
        for (term, postings) in &terms {
            let want: Vec<DecodedPosting> = postings
                .iter()
                .map(|p| DecodedPosting {
                    rowid: p.rowid,
                    cols: p.cols.clone(),
                })
                .collect();
            assert_eq!(decode(&seg, term), Some(want), "term {term:?}");
        }
        assert_eq!(decode(&seg, b"absent"), None);
    }

    // ---- D2b-2 lookup_term_rowids (structure-aware top-level lookup) ------

    #[test]
    fn lookup_rowids_single_segment_present_and_absent() {
        // "cat" in docs 1,3,7; the lookup parses the structure record, gathers the
        // single segment's leaves, and returns the rowids ascending.
        let terms = vec![(
            b"cat".to_vec(),
            vec![p(1, &[&[0]]), p(3, &[&[2]]), p(7, &[&[1]])],
        )];
        let seg = build_segment(
            &terms,
            3,
            &[3],
            &[(1, vec![1]), (3, vec![3]), (7, vec![2])],
            1000,
            0,
        );
        assert_eq!(lookup_term_rowids(&seg.data, b"cat"), Some(vec![1, 3, 7]));
        // A servable segment whose term is absent → an empty rowid list (no match),
        // distinct from `None` (the index couldn't be served).
        assert_eq!(lookup_term_rowids(&seg.data, b"dog"), Some(Vec::new()));
    }

    #[test]
    fn lookup_rowids_empty_index_falls_back() {
        // An empty index has no leaves and `nLevel == 0`: not servable → `None`.
        let seg = build_segment(&[], 0, &[0], &[], 1000, 0);
        assert_eq!(lookup_term_rowids(&seg.data, b"anything"), None);
    }

    #[test]
    fn lookup_rowids_multi_leaf_segment() {
        // A doclist that spans several leaves still resolves via the leaf reader.
        let n = 40i64;
        let postings: Vec<Posting> = (1..=n).map(|r| p(r, &[&[0]])).collect();
        let terms = vec![(b"x".to_vec(), postings)];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n).map(|r| (r, vec![1])).collect();
        let seg = build_segment(&terms, n as u64, &[n as u64], &doc_sizes, 64, 0);
        assert!(leaf_count(&seg) > 1, "pgsz 64 must span the doclist");
        let want: Vec<i64> = (1..=n).collect();
        assert_eq!(lookup_term_rowids(&seg.data, b"x"), Some(want));
        assert_eq!(lookup_term_rowids(&seg.data, b"y"), Some(Vec::new()));
    }

    #[test]
    fn lookup_rowids_in_column_filters_by_column() {
        // "word" occurs in: doc1 col0, doc2 col1, doc3 col0+col1, doc4 col0.
        let terms = vec![(
            b"word".to_vec(),
            vec![
                p(1, &[&[0], &[]]),
                p(2, &[&[], &[0]]),
                p(3, &[&[0], &[1]]),
                p(4, &[&[2], &[]]),
            ],
        )];
        let doc_sizes: Vec<(i64, Vec<u64>)> = vec![
            (1, vec![1, 0]),
            (2, vec![0, 1]),
            (3, vec![1, 2]),
            (4, vec![3, 0]),
        ];
        let seg = build_segment(&terms, 4, &[3, 2], &doc_sizes, 1000, 0);
        // Column 0: docs 1, 3, 4. Column 1: docs 2, 3.
        assert_eq!(
            lookup_term_rowids_in_column(&seg.data, b"word", 0),
            Some(vec![1, 3, 4])
        );
        assert_eq!(
            lookup_term_rowids_in_column(&seg.data, b"word", 1),
            Some(vec![2, 3])
        );
        // Any-column lookup is the union, in rowid order.
        assert_eq!(
            lookup_term_rowids(&seg.data, b"word"),
            Some(vec![1, 2, 3, 4])
        );
        // Absent term in any column → empty (servable), and a column index past the
        // table's column count never matches.
        assert_eq!(
            lookup_term_rowids_in_column(&seg.data, b"missing", 0),
            Some(Vec::new())
        );
        assert_eq!(
            lookup_term_rowids_in_column(&seg.data, b"word", 9),
            Some(Vec::new())
        );
    }

    #[test]
    fn lookup_rowids_in_column_multi_leaf() {
        // A multi-leaf, multi-column segment: even-rowid docs carry "x" in col0,
        // odd-rowid docs in col1, so the column filter splits the spanning doclist.
        let n = 40i64;
        let postings: Vec<Posting> = (1..=n)
            .map(|r| {
                if r % 2 == 0 {
                    p(r, &[&[0], &[]])
                } else {
                    p(r, &[&[], &[0]])
                }
            })
            .collect();
        let terms = vec![(b"x".to_vec(), postings)];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n).map(|r| (r, vec![1, 1])).collect();
        let seg = build_segment(&terms, n as u64, &[20, 20], &doc_sizes, 64, 0);
        assert!(leaf_count(&seg) > 1, "pgsz 64 must span the doclist");
        let even: Vec<i64> = (1..=n).filter(|r| r % 2 == 0).collect();
        let odd: Vec<i64> = (1..=n).filter(|r| r % 2 == 1).collect();
        assert_eq!(lookup_term_rowids_in_column(&seg.data, b"x", 0), Some(even));
        assert_eq!(lookup_term_rowids_in_column(&seg.data, b"x", 1), Some(odd));
    }
}
