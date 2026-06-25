//! FTS5 `%_data`/`%_idx` segment-index encoder (roadmap D2e-M2) and the
//! single-leaf doclist reader (roadmap D2b-1).
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
//! The read path (`decode_term`, D2b-1) is the exact inverse of the writer: given
//! a segment's leaf blobs and a term, it walks the page-index, reconstructs the
//! prefix-compressed term key, and decodes the matching doclist into postings
//! (docids + per-column positions). It is scoped to single-leaf segments and is
//! not yet wired into `MATCH` (that is D2b-2); see the section comment below.

use alloc::vec::Vec;

use crate::util::varint;

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
// D2b-1: the read path — decode a single-term doclist from the `%_data` leaves.
//
// This is the byte-for-byte inverse of the writer above. It reads the segment's
// leaf pages (height-0 `%_data` rows), walks each leaf's term records (the
// prefix-compressed '0'-prefixed keys, located via the page-index footer), and
// for a matching term decodes its doclist into postings (rowid + per-column
// positions). It is the exact inverse of `add_term`/`doclist`/`poslist`.
//
// Scope: SINGLE-LEAF segments — a small index whose every term and its whole
// doclist fit in one leaf (the common small-table case). Multi-leaf segments
// (a term whose doclist spans leaves, or interior/doclist-index pages) are NOT
// decoded here; that is the larger D2b reader (see the module doc and ROADMAP
// D2b/D2e). `decode_term` returns `None` when it would need to cross a leaf
// boundary, so a caller can fall back to the `_content` scan rather than read a
// truncated doclist.
//
// The reader is verified end-to-end here (writer→decoder round-trips) and in
// `tests/fts5_decode.rs` (decode what `sqlite3` itself wrote). It is not wired
// into `MATCH` yet — that is D2b-2, which lives in another module — so these
// items have no non-test in-crate caller and carry `#[allow(dead_code)]` until
// that wiring lands.

/// Read a varint at `buf[*pos..]`, advancing `pos`. `None` on a truncated/empty
/// slice.
#[allow(dead_code)]
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
#[allow(dead_code)]
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

/// Decode a contiguous doclist occupying `buf[start..end]` into postings. The
/// inverse of [`doclist`]: `[first rowid][ (rowid delta)(poslist) ]*` with rowid
/// deltas accumulated from 0.
#[allow(dead_code)]
fn decode_doclist(buf: &[u8], start: usize, end: usize) -> Option<Vec<DecodedPosting>> {
    let mut pos = start;
    let mut out = Vec::new();
    let mut rowid = 0i64;
    let mut first = true;
    while pos < end {
        let d = read_varint(buf, &mut pos)? as i64;
        rowid = if first { d } else { rowid.wrapping_add(d) };
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

/// Decode a single leaf page into its `(term_key, doclist_start, doclist_end)`
/// records, where `term_key` is the full '0'-prefixed key and the range is
/// `[doclist_start, doclist_end)` within the leaf. Returns `None` if the leaf is
/// malformed or a doclist runs off the end of the page (a doclist that spans into
/// the next leaf — out of single-leaf scope).
///
/// Layout (inverse of [`SegWriter::finish_leaf`]):
/// `[u16 first_rowid_off][u16 footer_off][body][pgidx]`; the body holds the term
/// records, `pgidx` holds each record's absolute page offset (first absolute,
/// then deltas).
#[allow(dead_code)]
fn decode_leaf_terms(leaf: &[u8]) -> Option<Vec<(Vec<u8>, usize, usize)>> {
    if leaf.len() < 4 {
        return None;
    }
    let footer_off = u16::from_be_bytes([leaf[2], leaf[3]]) as usize;
    if footer_off < 4 || footer_off > leaf.len() {
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
    if term_offs.is_empty() {
        // A continuation leaf (no terms of its own) — out of single-leaf scope.
        return None;
    }
    let mut out = Vec::new();
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
        // The doclist runs from here to the next term's offset (or the footer).
        let doclist_end = term_offs.get(i + 1).copied().unwrap_or(footer_off);
        if doclist_end < p || doclist_end > footer_off {
            return None;
        }
        out.push((key.clone(), p, doclist_end));
        prev_key = key;
    }
    Some(out)
}

/// Look up `term` in a set of segment leaf pages and return its decoded postings
/// (docids with per-column positions), or `None` if the term is absent.
///
/// `leaves` are the height-0 `%_data` leaf blobs of a single segment. This is the
/// single-leaf reader: it decodes a term whose record and whole doclist live in
/// one leaf. A term whose doclist spans leaves, or a segment with interior /
/// doclist-index pages, is out of scope and yields `None` (the caller falls back
/// to the document scan). The empty result `Some(vec![])` never occurs — a
/// present term always has at least one posting.
#[allow(dead_code)]
pub(crate) fn decode_term(leaves: &[&[u8]], term: &[u8]) -> Option<Vec<DecodedPosting>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec};

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
}
