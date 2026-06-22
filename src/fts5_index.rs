//! FTS5 `%_data`/`%_idx` segment-index encoder (roadmap D2e-M2).
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
}
