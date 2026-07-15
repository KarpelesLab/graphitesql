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
//! only affects byte-identity, not readability). A single term whose doclist
//! spills onto `FTS5_MIN_DLIDX_SIZE`+ term-less continuation leaves DOES get a
//! doclist-index (`dlidx`) b-tree — see [`SegWriter::finish_term_dlidx`] — which
//! sqlite's `integrity-check` requires; without it a large-scale index is rejected
//! as corrupt. That path is byte-verified against the sqlite oracle in
//! `tests/fts5_scale.rs`.
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

/// `fts5_dri(segid, dlidx=1, height, pgno)` — the `%_data` rowid of a
/// doclist-index page. The rowid layout is
/// `segid<<37 | dlidx<<36 | height<<31 | pgno`
/// (`FTS5_DATA_PAGE_B=31`, `FTS5_DATA_HEIGHT_B=5`, `FTS5_DATA_DLI_B=1`).
pub(crate) fn dlidx_rowid(segid: i64, height: i64, pgno: i64) -> i64 {
    (segid << 37) | (1 << 36) | (height << 31) | pgno
}

/// `FTS5_MIN_DLIDX_SIZE` — a term's doclist gets a doclist-index only when it
/// spills onto at least this many term-less continuation leaves.
const MIN_DLIDX_SIZE: usize = 4;

/// The first rowid recorded on a doclist-index page: skip the flag byte and the
/// leaf/child pgno varint, then read the rowid varint (matches sqlite's
/// `fts5DlidxExtractFirstRowid`). `0` if the page is malformed/too short.
fn dlidx_first_rowid(page: &[u8]) -> i64 {
    let mut pos = 1usize; // skip flag byte
    if read_varint(page, &mut pos).is_none() {
        return 0;
    }
    read_varint(page, &mut pos).unwrap_or(0) as i64
}

/// Append the sqlite varint encoding of `v` to `out`.
fn put_varint(out: &mut Vec<u8>, v: u64) {
    let mut buf = [0u8; varint::MAX_LEN];
    let n = varint::encode(v, &mut buf);
    out.extend_from_slice(&buf[..n]);
}

/// One document's contribution to a term: its rowid and, per column, the sorted
/// token positions (`cols[c]` empty if the term does not occur in column `c`).
///
/// A `del` posting is a DELETE marker (sqlite's tombstone): it carries no
/// positions and its poslist is written as `size2 = 1` (content length 0 with the
/// low delete-flag bit set), shadowing an older segment's entry for `rowid` when
/// the segments are later merged. See [`poslist`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Posting {
    pub rowid: i64,
    pub cols: Vec<Vec<u32>>,
    /// True for a delete marker (tombstone). Normal insert postings are `false`.
    pub del: bool,
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
/// where `size` is `content_len × 2 + delete_flag` (sqlite's `nPos = nSz*2 +
/// bDel`, see `fts5HashAddPoslistSize`). Positions are per-column.
///
/// A DELETE marker (`p.del`) sets the low bit. A pure tombstone (old row's term,
/// no new positions) is `size2 = 1` with no content. An UPDATE that re-inserts a
/// term the old row also had keeps `del = true` AND carries the new positions:
/// `size2 = content_len*2 + 1` followed by the content — exactly how sqlite's hash
/// flush encodes a docid that was deleted then re-written in one transaction.
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
    put_varint(&mut out, (content.len() as u64) * 2 + u64::from(p.del));
    out.extend_from_slice(&content);
    out
}

/// Port of sqlite's `fts5PoslistPrefix`: the byte length of the largest run of
/// WHOLE varints at the start of `buf` whose total length is `<= n_max`. The first
/// varint is always included (even if it alone exceeds `n_max`), matching sqlite's
/// `ret = varint_len(buf[0]); if( ret < n_max ){ … }`. `n_max` may be ≤ 0.
fn poslist_prefix(buf: &[u8], n_max: isize) -> usize {
    let mut ret = varint::decode(buf).map(|(_, n)| n).unwrap_or(1);
    if (ret as isize) < n_max {
        while let Some((_, i)) = varint::decode(&buf[ret..]) {
            if (ret + i) as isize > n_max {
                break;
            }
            ret += i;
        }
    }
    ret
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

/// A [`SegWriter::finish`] result: the leaf pages, the `%_idx` rows, and the
/// doclist-index `%_data` pages `(rowid, block)`.
type SegParts = (Vec<Vec<u8>>, Vec<IdxRow>, Vec<(i64, Vec<u8>)>);

/// The tokenized inputs the segment builder consumes for a document set: the
/// ascending `terms` (term bytes → per-doc postings), each column's total token
/// count, and the per-document `(rowid, per-column token counts)`.
pub(crate) type TokenizedDocs = (Vec<(Vec<u8>, Vec<Posting>)>, Vec<u64>, Vec<(i64, Vec<u64>)>);

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
    /// Accumulated doclist-index `%_data` pages `(rowid, block)` for terms whose
    /// doclists spilled onto `MIN_DLIDX_SIZE`+ term-less continuation leaves.
    dlidx_data: Vec<(i64, Vec<u8>)>,
    /// While streaming one term's doclist, `(leaf_pgno, first_absolute_rowid)` for
    /// each CONTINUATION leaf (the leaves after the term-start leaf that begin a
    /// new doclist entry at their `first_rowid_off`). Cleared per term.
    span_pages: Vec<(i64, i64)>,
    /// True while the next rowid written to the current leaf will be the FIRST
    /// rowid on that leaf (sqlite's `bFirstRowidInPage`). A first-in-page rowid is
    /// stored ABSOLUTE and sets the leaf's `first_rowid_off` pointer.
    first_rowid_in_page: bool,
    /// True while the next rowid is the FIRST of the current doclist (sqlite's
    /// `bFirstRowidInDoclist`). A first-in-doclist rowid is stored ABSOLUTE.
    first_rowid_in_doclist: bool,
    /// When true, use sqlite's INCREMENTAL-MERGE writer semantics
    /// (`fts5WriteAppendRowid`/`fts5WriteAppendPoslistData`) rather than the bulk
    /// `fts5FlushOneHash` writer: a leaf-full check runs BEFORE each rowid, and the
    /// poslist-spill loop copies whole varints until the page is full (rather than
    /// stopping just short). These diverge from the bulk writer only when a merge
    /// output spans multiple leaves, but then they are byte-exact vs sqlite's
    /// `fts5IndexMergeLevel`. See [`build_merged_segment_block`].
    merge_mode: bool,
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
            dlidx_data: Vec::new(),
            span_pages: Vec::new(),
            first_rowid_in_page: true,
            first_rowid_in_doclist: true,
            merge_mode: false,
        }
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
        // sqlite's `fts5WriteFlushLeaf`: the fresh leaf holds no terms or rowids.
        self.first_rowid_in_page = true;
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

    /// Current serialized page-index footer length (bytes already committed for
    /// the terms on the in-progress leaf) — sqlite's `pPage->pgidx.n`.
    fn pgidx_len(&self) -> usize {
        pgidx(&self.term_offsets).len()
    }

    /// Port of the rowid step of sqlite's `fts5FlushOneHash` doclist writer — the
    /// single-pass bulk-build path graphite's one-shot segment build mirrors (NOT the
    /// incremental-merge `fts5WriteAppendRowid`, which sqlite only reaches when a
    /// large corpus is flushed into several segments and later compacted). Appends
    /// one doclist rowid. A rowid that is the FIRST on its leaf (`bFirstRowidInPage`)
    /// is stored ABSOLUTE, its byte offset becomes the leaf's `first_rowid_off`
    /// header pointer, and — on a term-less continuation leaf — it seeds a
    /// doclist-index entry; a rowid that is first-in-doclist is also ABSOLUTE; every
    /// other rowid is a delta from the previous. `fts5FlushOneHash` does NOT
    /// flush-check before the rowid — a fresh leaf is only ever reached from the
    /// poslist-spill loop below, which flushes and re-arms `first_rowid_in_page`.
    fn append_rowid(&mut self, rowid: i64, term_start_leaf: i64) {
        // Incremental-merge writer only: `fts5WriteAppendRowid` flushes a full leaf
        // BEFORE the rowid — `if( (buf.n + pgidx.n) >= pgsz )`. The bulk writer has
        // no such check (it only flushes while streaming a poslist), so a rowid can
        // ride along on a leaf the merge writer would already have closed. This is
        // the sole difference for merge outputs whose per-doc poslists fit a leaf.
        if self.merge_mode && 4 + self.body.len() + self.pgidx_len() >= self.pgsz {
            self.flush();
        }
        if self.first_rowid_in_page {
            self.first_rowid_off = 4 + self.body.len();
            if self.pgno != term_start_leaf {
                self.span_pages.push((self.pgno, rowid));
            }
        }
        if self.first_rowid_in_doclist || self.first_rowid_in_page {
            put_varint(&mut self.body, rowid as u64);
        } else {
            put_varint(&mut self.body, (rowid - self.prev_rowid) as u64);
        }
        self.prev_rowid = rowid;
        self.first_rowid_in_doclist = false;
        self.first_rowid_in_page = false;
    }

    /// Port of the poslist step of sqlite's `fts5FlushOneHash` doclist writer. `data`
    /// is one posting's position-list (`size` varint + content bytes). If it fits
    /// whole on the current leaf (`buf.n + pgidx.n + nCopy <= pgsz`) it is copied in
    /// one go; otherwise it is broken into sections, each the largest run of WHOLE
    /// varints fitting the remaining page space (`fts5PoslistPrefix`), with a leaf
    /// flush whenever the page reaches `pgsz`.
    fn append_poslist_data(&mut self, data: &[u8]) {
        let n_copy = data.len();
        if self.merge_mode {
            // Port of `fts5WriteAppendPoslistData`: while the poslist would overflow
            // the page, copy the largest run of WHOLE varints that reaches the page
            // limit (`nCopy` accumulates until `nCopy >= nReq`), then flush.
            let mut a = data;
            while 4 + self.body.len() + self.pgidx_len() + a.len() >= self.pgsz {
                let n_req = self.pgsz as isize - (4 + self.body.len() + self.pgidx_len()) as isize;
                let mut n_c = 0usize;
                while (n_c as isize) < n_req {
                    match varint::decode(&a[n_c..]) {
                        Some((_, len)) => n_c += len,
                        None => break,
                    }
                }
                // `n_c` is 0 when the page is already exactly (or over) full
                // (`n_req <= 0`). sqlite's `fts5WriteAppendPoslistData` still
                // FLUSHES in that case — copying nothing — which SPLITS the
                // poslist across the leaf boundary (its trailing varints land on
                // the next continuation leaf, whose `first_rowid_off` then points
                // PAST them). Riding the whole poslist onto the current page
                // instead diverges from sqlite by one leaf boundary. Clamp is a
                // safety net only: the outer guard ensures `a.len() >= n_req`, so
                // `n_c <= a.len()`. After a `n_c == 0` flush the body resets to 4
                // bytes (< pgsz), so `n_req` becomes positive and the loop makes
                // progress — no infinite loop.
                if n_c > a.len() {
                    n_c = a.len();
                }
                self.body.extend_from_slice(&a[..n_c]);
                a = &a[n_c..];
                self.flush();
            }
            if !a.is_empty() {
                self.body.extend_from_slice(a);
            }
            return;
        }
        if 4 + self.body.len() + self.pgidx_len() + n_copy <= self.pgsz {
            self.body.extend_from_slice(data);
            return;
        }
        let mut i_pos = 0usize;
        loop {
            let n_space = self.pgsz as isize - (4 + self.body.len() + self.pgidx_len()) as isize;
            let n = if (n_copy - i_pos) as isize <= n_space {
                n_copy - i_pos
            } else {
                // `fts5PoslistPrefix`: the largest whole-varint prefix of the
                // remaining poslist whose byte length is `<= n_space` (always at least
                // the first varint). `n_space` may be small/≤0 here.
                poslist_prefix(&data[i_pos..], n_space)
            };
            self.body.extend_from_slice(&data[i_pos..i_pos + n]);
            i_pos += n;
            if 4 + self.body.len() + self.pgidx_len() >= self.pgsz {
                self.flush();
            }
            if i_pos >= n_copy {
                break;
            }
        }
    }

    fn add_term(&mut self, term: &[u8], postings: &[Posting]) {
        let key = term_key(term);
        self.add_key(&key, postings);
    }

    /// Append a term whose FULL stored key (index-prefix byte + term bytes) is
    /// already built. The main index calls [`add_term`] (key = `'0'` + term); the
    /// prefix indexes call this directly with a `'1'`/`'2'`/… prefix byte. Keys
    /// must arrive in ascending order across the whole segment (main `'0'` terms
    /// first, then each prefix index in turn), matching sqlite's single merged
    /// term stream.
    fn add_key(&mut self, key: &[u8], postings: &[Posting]) {
        // Port of `fts5WriteAppendTerm`'s leaf-fill boundary. sqlite decides to end
        // the current leaf purely from the header + committed pgidx + the FULL
        // (uncompressed) term length + a fixed 2-byte slack — the doclist size is
        // NOT part of this decision (overflow past the page is handled by streaming
        // the doclist across continuation leaves, below). `key.len()` is the stored
        // term length INCLUDING the 0x30 main-index prefix byte, matching sqlite's
        // `nTerm` (the hash/merge term is stored prefixed).
        //   if( (pPage->buf.n + pPgidx->n + nTerm + 2) >= pgsz ) flush-if-buf.n>4
        if 4 + self.body.len() + self.pgidx_len() + key.len() + 2 >= self.pgsz
            && !self.body.is_empty()
        {
            self.flush();
        }
        let rec = self.term_record(key);
        self.term_offsets.push(4 + self.body.len());
        if self.leaf_first_term.is_none() {
            self.leaf_first_term = Some(key.to_vec());
        }
        self.leaf_last_term = Some(key.to_vec());
        self.body.extend_from_slice(&rec);
        self.prev_term_key = Some(key.to_vec());
        // sqlite's `fts5WriteAppendTerm` clears `bFirstRowidInPage` after writing a
        // term: the leaf's `first_rowid_off` pointer is set ONLY on a term-less
        // continuation leaf, so a rowid written right after a term on this page is
        // NOT the page's recorded "first rowid" (it stays 0 for the term-start leaf).
        // It also arms `bFirstRowidInDoclist` for this term's first rowid.
        self.first_rowid_in_page = false;
        self.first_rowid_in_doclist = true;
        // Stream the doclist through the `fts5FlushOneHash` ports (`append_rowid` +
        // `append_poslist_data`): each posting's rowid then its position-list data,
        // flushing whole leaves as the page fills so a doclist that overflows the page
        // spills onto term-less CONTINUATION leaves — and, when it spills far enough,
        // gets a doclist-index.
        let term_start_leaf = self.pgno;
        self.span_pages.clear();
        self.prev_rowid = 0;
        for p in postings {
            self.append_rowid(p.rowid, term_start_leaf);
            let pl = poslist(p);
            if self.merge_mode {
                // sqlite's incremental-merge writer (`fts5IndexMergeLevel`) does
                // NOT hand the whole poslist to `fts5WriteAppendPoslistData`. It
                // appends the position-list SIZE varint directly to the leaf
                // buffer with NO page-full check (`fts5BufferAppendVarint(&buf,
                // nPos)` — this can push the leaf PAST pgsz), then streams only the
                // position DATA (the bytes after the size) through
                // `fts5WriteAppendPoslistData` (via `fts5MergeChunkCallback`). The
                // bulk `fts5FlushOneHash` writer instead copies size+content as one
                // unit — which is why the two paths split spanning leaves at
                // different byte offsets. Mirror the merge split so the incremental
                // merge output is byte-identical to sqlite's.
                let size_len = varint::decode(&pl).map(|(_, n)| n).unwrap_or(pl.len());
                self.body.extend_from_slice(&pl[..size_len]);
                if size_len < pl.len() {
                    self.append_poslist_data(&pl[size_len..]);
                }
            } else {
                self.append_poslist_data(&pl);
            }
        }
        self.finish_term_dlidx(term_start_leaf);
    }

    /// After a spanning term's doclist is fully written, emit a doclist-index for
    /// it if it spilled onto `MIN_DLIDX_SIZE`+ term-less continuation leaves, and
    /// set the dlidx flag on that term's `%_idx` entry (the b-tree page).
    ///
    /// A doclist-index is a small b-tree of its own (`FTS5_DLIDX_ROWID` pages),
    /// built here as a faithful port of sqlite's `fts5WriteDlidxAppend`. Level 0
    /// maps each continuation leaf that BEGINS a rowid to that first rowid; a
    /// continuation leaf with no rowid of its own (a pure poslist tail) contributes
    /// a `0x00` padding byte. When a level's page fills to `pgsz` it is flushed
    /// (flag byte `0x01`, "not the root") and a copy of the appended rowid is
    /// cascaded up to the next level of the hierarchy.
    fn finish_term_dlidx(&mut self, term_start_leaf: i64) {
        // `span_pages` holds `(leaf_pgno, first_rowid)` for every continuation leaf
        // that begins a rowid. Below `MIN_DLIDX_SIZE` term-less leaves, no dlidx.
        if self.span_pages.is_empty() {
            return;
        }
        let last_leaf = self.span_pages.last().map(|&(pg, _)| pg).unwrap_or(0);
        let n_empty = (last_leaf - term_start_leaf) as usize;
        if n_empty < MIN_DLIDX_SIZE {
            self.span_pages.clear();
            return;
        }
        let span = core::mem::take(&mut self.span_pages);
        let pgsz = self.pgsz;
        // One accumulator per b-tree level. `pgno` is the page number this level is
        // currently writing (level 0 starts at the term-start leaf, matching
        // sqlite's `aDlidx[0].pgno = pPage->pgno`); `prev`/`prev_valid` drive the
        // rowid delta encoding.
        struct DlidxLvl {
            buf: Vec<u8>,
            pgno: i64,
            prev: i64,
            prev_valid: bool,
        }
        let new_lvl = |pgno: i64| DlidxLvl {
            buf: Vec::new(),
            pgno,
            prev: 0,
            prev_valid: false,
        };
        let mut lvls: Vec<DlidxLvl> = alloc::vec![new_lvl(term_start_leaf)];
        // Emitted pages `(height, pgno, bytes)`.
        let mut out: Vec<(i64, i64, Vec<u8>)> = Vec::new();

        // Port of `fts5WriteDlidxAppend`: record one rowid, cascading up levels as
        // pages fill. `leaf_pgno` is the level-0 leaf (used only to seed a fresh
        // level-0 page's pgno reference). Before each level-0 append, emit one
        // `0x00` padding byte for every intervening term-less leaf that carried NO
        // rowid of its own (a gap in the recorded leaf pgnos).
        let mut prev_leaf = term_start_leaf;
        for &(leaf_pgno, rowid) in &span {
            for _ in 0..(leaf_pgno - prev_leaf - 1).max(0) {
                lvls[0].buf.push(0x00);
            }
            prev_leaf = leaf_pgno;
            let mut i = 0usize;
            let mut b_done = false;
            while !b_done {
                if i >= lvls.len() {
                    lvls.push(new_lvl(0));
                }
                if lvls[i].buf.len() >= pgsz {
                    // Page full: flush it (flag 0x01 = not the root) and grow the
                    // hierarchy so level i+1 exists.
                    lvls[i].buf[0] = 0x01;
                    let flushed = core::mem::take(&mut lvls[i].buf);
                    let flushed_pgno = lvls[i].pgno;
                    out.push((i as i64, flushed_pgno, flushed.clone()));
                    if i + 1 >= lvls.len() {
                        lvls.push(new_lvl(0));
                    }
                    // If the parent level is empty, this flushed node was the root;
                    // seed the new parent with the flushed page's first rowid.
                    if lvls[i + 1].buf.is_empty() {
                        let first = dlidx_first_rowid(&flushed);
                        lvls[i + 1].pgno = flushed_pgno;
                        let parent = &mut lvls[i + 1];
                        parent.buf.push(0x00);
                        put_varint(&mut parent.buf, flushed_pgno as u64);
                        put_varint(&mut parent.buf, first as u64);
                        parent.prev = first;
                        parent.prev_valid = true;
                    }
                    lvls[i].prev_valid = false;
                    lvls[i].pgno += 1;
                } else {
                    b_done = true;
                }
                if lvls[i].prev_valid {
                    let d = (rowid - lvls[i].prev) as u64;
                    put_varint(&mut lvls[i].buf, d);
                } else {
                    // Fresh page for this level: [flag=!bDone][pgno-ref][rowid].
                    let ref_pgno = if i == 0 { leaf_pgno } else { lvls[i - 1].pgno };
                    let flag = u8::from(!b_done);
                    let lvl = &mut lvls[i];
                    lvl.buf.push(flag);
                    put_varint(&mut lvl.buf, ref_pgno as u64);
                    put_varint(&mut lvl.buf, rowid as u64);
                }
                lvls[i].prev = rowid;
                lvls[i].prev_valid = true;
                i += 1;
            }
        }
        // Flush every still-open page. The highest non-empty level's open page is
        // the root (flag 0x00); any lower open page is non-root (0x01).
        let top = lvls.iter().rposition(|l| !l.buf.is_empty()).unwrap_or(0);
        for (i, lvl) in lvls.iter_mut().enumerate() {
            if lvl.buf.is_empty() {
                continue;
            }
            let mut page = core::mem::take(&mut lvl.buf);
            page[0] = if i == top { 0x00 } else { 0x01 };
            out.push((i as i64, lvl.pgno, page));
        }
        for (height, pgno, page) in out {
            self.dlidx_data
                .push((dlidx_rowid(self.segid, height, pgno), page));
        }
        // Mark the dlidx flag on the `%_idx` entry for the term-start leaf.
        let want = term_start_leaf << 1;
        for row in self.idx.iter_mut() {
            if row.pgno == want {
                row.pgno |= 1;
                break;
            }
        }
    }

    fn finish(mut self) -> SegParts {
        self.flush();
        (self.leaves, self.idx, self.dlidx_data)
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

/// Encode the `%_data` AVERAGES record (rowid [`AVERAGES_ROWID`]): empty when
/// `n_rows == 0`, else `nRow` followed by each column's total token count. Shared
/// by the bulk builder and the incremental append.
pub(crate) fn encode_averages(n_rows: u64, col_totals: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    if n_rows > 0 {
        put_varint(&mut out, n_rows);
        for &t in col_totals {
            put_varint(&mut out, t);
        }
    }
    out
}

/// Encode the `%_data` AVERAGES record ALWAYS as `nRow` followed by each column's
/// total token count — even for `n_rows == 0` (`00 00…`). Used by the incremental
/// DELETE path: a table tombstoned down to zero rows keeps its averages record
/// present (sqlite only omits it before the first document is ever written),
/// unlike [`encode_averages`], which returns empty for a fresh, never-populated
/// index.
pub(crate) fn encode_averages_full(n_rows: u64, col_totals: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, n_rows);
    for &t in col_totals {
        put_varint(&mut out, t);
    }
    out
}

// ---------------------------------------------------------------------------
// Incremental (multi-segment) write support — the structure-record model.
//
// sqlite's fts5 appends a NEW level-0 segment per write transaction rather than
// rebuilding the whole index. The on-disk `%_data` STRUCTURE record (rowid
// [`STRUCTURE_ROWID`]) tracks the current shape: a config cookie, a write
// counter, and per level `{nMerge, [ {iSegid, pgnoFirst, pgnoLast} … ]}`. These
// helpers parse and re-emit that record so the executor can append, promote, and
// crisis-merge exactly like `fts5_index.c` — see `SegStructure`.

/// One segment reference inside a level of the [`SegStructure`] record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructSeg {
    pub segid: i64,
    pub pgno_first: i64,
    pub pgno_last: i64,
}

impl StructSeg {
    /// Segment size in leaf pages (`fts5SegmentSize`).
    pub(crate) fn size(&self) -> i64 {
        1 + self.pgno_last - self.pgno_first
    }
}

/// One level of the [`SegStructure`] record: a merge counter and its segments,
/// newest last (level 0 is the youngest data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructLevel {
    pub n_merge: i64,
    pub segs: Vec<StructSeg>,
}

/// The parsed `%_data` STRUCTURE record (`fts5StructureRead`/`Write`): the config
/// cookie, the running write counter (total leaf pages ever flushed), and the
/// per-level segment lists. The port of `Fts5Structure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegStructure {
    pub cookie: u32,
    pub write_counter: u64,
    pub levels: Vec<StructLevel>,
}

impl SegStructure {
    /// Parse a raw STRUCTURE-record blob, or `None` if malformed/unsupported.
    pub(crate) fn parse(buf: &[u8]) -> Option<SegStructure> {
        if buf.len() < 4 {
            return None;
        }
        let cookie = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let mut pos = 4usize;
        let n_level = read_varint(buf, &mut pos)?;
        let n_segment = read_varint(buf, &mut pos)?;
        let write_counter = read_varint(buf, &mut pos)?;
        let mut levels = Vec::with_capacity(n_level as usize);
        let mut total = 0u64;
        for _ in 0..n_level {
            let n_merge = read_varint(buf, &mut pos)? as i64;
            let n_seg = read_varint(buf, &mut pos)?;
            let mut segs = Vec::with_capacity(n_seg as usize);
            for _ in 0..n_seg {
                let segid = read_varint(buf, &mut pos)? as i64;
                let pgno_first = read_varint(buf, &mut pos)? as i64;
                let pgno_last = read_varint(buf, &mut pos)? as i64;
                segs.push(StructSeg {
                    segid,
                    pgno_first,
                    pgno_last,
                });
            }
            total += n_seg;
            levels.push(StructLevel { n_merge, segs });
        }
        if total != n_segment {
            return None;
        }
        Some(SegStructure {
            cookie,
            write_counter,
            levels,
        })
    }

    /// Re-emit the record byte-for-byte in `fts5StructureWrite` form.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = self.cookie.to_be_bytes().to_vec();
        let n_segment: u64 = self.levels.iter().map(|l| l.segs.len() as u64).sum();
        put_varint(&mut out, self.levels.len() as u64);
        put_varint(&mut out, n_segment);
        put_varint(&mut out, self.write_counter);
        for lvl in &self.levels {
            put_varint(&mut out, lvl.n_merge as u64);
            put_varint(&mut out, lvl.segs.len() as u64);
            for s in &lvl.segs {
                put_varint(&mut out, s.segid as u64);
                put_varint(&mut out, s.pgno_first as u64);
                put_varint(&mut out, s.pgno_last as u64);
            }
        }
        out
    }

    /// Allocate the lowest unused positive segid (`fts5AllocateSegid`): the
    /// smallest id not currently held by any segment in any level.
    pub(crate) fn allocate_segid(&self) -> i64 {
        let mut used = alloc::collections::BTreeSet::new();
        for lvl in &self.levels {
            for s in &lvl.segs {
                if s.segid > 0 {
                    used.insert(s.segid);
                }
            }
        }
        let mut id = 1;
        while used.contains(&id) {
            id += 1;
        }
        id
    }

    /// Append a freshly-written level-0 segment (`iSegid`, `n_leaves` pages) and
    /// run the post-append promote — the `fts5FlushOneHash` tail. `n_leaves`
    /// also advances the write counter. This is the below-threshold path;
    /// crisis-merge (16+ segments on a level) is handled by the caller.
    pub(crate) fn append_level0(&mut self, segid: i64, n_leaves: i64) {
        if self.levels.is_empty() {
            self.levels.push(StructLevel {
                n_merge: 0,
                segs: Vec::new(),
            });
        }
        self.levels[0].segs.push(StructSeg {
            segid,
            pgno_first: 1,
            pgno_last: n_leaves,
        });
        self.write_counter += n_leaves as u64;
        self.promote(0);
    }

    /// Run `fts5StructurePromote` for a level `i_lvl` that just received a merged
    /// segment (crisis/auto merge output), a no-op when the level is out of range.
    pub(crate) fn promote_after_merge(&mut self, i_lvl: usize) {
        if i_lvl < self.levels.len() {
            self.promote(i_lvl);
        }
    }

    /// Port of `fts5StructurePromote`: after a segment is written to `i_lvl`,
    /// possibly move it (or, via promote-to, pull equal-or-smaller segments from
    /// higher levels down) so each level holds like-sized segments. Faithful to
    /// the two conditions in the C source.
    fn promote(&mut self, i_lvl: usize) {
        if self.levels[i_lvl].segs.is_empty() {
            return;
        }
        let sz_seg = self.levels[i_lvl].segs.last().unwrap().size();

        // Condition (a): a NON-EMPTY lower level whose largest segment is >= the
        // just-written one → promote the new segment DOWN into that lower level.
        let mut i_tst: isize = i_lvl as isize - 1;
        while i_tst >= 0 && self.levels[i_tst as usize].segs.is_empty() {
            i_tst -= 1;
        }
        let mut i_promote = i_lvl;
        let mut sz_promote = sz_seg;
        if i_tst >= 0 {
            let tst = &self.levels[i_tst as usize];
            let sz_max = tst.segs.iter().map(|s| s.size()).max().unwrap_or(0);
            if sz_max >= sz_seg {
                i_promote = i_tst as usize;
                sz_promote = sz_max;
            }
        }
        self.promote_to(i_promote, sz_promote);
    }

    /// Port of `fts5StructurePromoteTo`: while `aLevel[iPromote].nMerge==0`, pull
    /// segments no larger than `sz_promote` from the levels ABOVE down into
    /// `iPromote` (newest first within each source level).
    fn promote_to(&mut self, i_promote: usize, sz_promote: i64) {
        if self.levels[i_promote].n_merge != 0 {
            return;
        }
        let mut il = i_promote + 1;
        while il < self.levels.len() {
            if self.levels[il].n_merge != 0 {
                return;
            }
            // Iterate source segments newest-first (index high→low). sqlite's
            // `fts5StructureExtendLevel(bInsert=1)` inserts each promoted segment
            // at the FRONT of the target level (memmove existing right), so a
            // promoted segment precedes the level's prior (newer-appended) ones.
            let mut is = self.levels[il].segs.len();
            while is > 0 {
                is -= 1;
                let sz = self.levels[il].segs[is].size();
                if sz > sz_promote {
                    return;
                }
                let seg = self.levels[il].segs.remove(is);
                self.levels[i_promote].segs.insert(0, seg);
            }
            il += 1;
        }
    }
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
///
/// This builds ONLY the main terms index (no prefix indexes). Callers that
/// configured `prefix='…'` use [`build_segment_prefixed`].
pub(crate) fn build_segment(
    terms: &[(Vec<u8>, Vec<Posting>)],
    n_docs: u64,
    col_totals: &[u64],
    doc_sizes: &[(i64, Vec<u64>)],
    pgsz: usize,
    cookie: u32,
) -> Segment {
    build_segment_prefixed(terms, n_docs, col_totals, doc_sizes, pgsz, cookie, &[])
}

/// The number of BYTES occupied by the first `n_char` unicode characters of the
/// UTF-8 term `t`, or `None` when `t` has fewer than `n_char` characters — the
/// port of sqlite's `sqlite3Fts5IndexCharlenToBytelen` (`fts5_index.c`). A `None`
/// result means the term is too short to contribute to that prefix index.
fn prefix_bytelen(t: &[u8], n_char: usize) -> Option<usize> {
    let mut n = 0usize;
    for i in 0..n_char {
        if n >= t.len() {
            return None; // fewer than n_char chars
        }
        if t[n] >= 0xc0 {
            n += 1;
            if n >= t.len() {
                return None;
            }
            while t[n] & 0xc0 == 0x80 {
                n += 1;
                if n >= t.len() {
                    // A final multi-byte character that runs to the end of the
                    // term still counts as the n_char-th character.
                    if i + 1 == n_char {
                        break;
                    }
                    return None;
                }
            }
        } else {
            n += 1;
        }
    }
    Some(n)
}

/// Merge several full terms' postings into one prefix term's doclist. `groups`
/// are the per-full-term posting slices sharing this prefix, in ascending term
/// order (so within a rowid the earlier term's positions come first). Postings
/// are unioned by rowid; per column the positions are merged and sorted (a
/// document listing the prefix once per contributing term keeps every position),
/// matching sqlite's hash-merge of the several `'N'`-prefixed doclists.
///
/// The DELETE flag propagates: sqlite's `sqlite3Fts5IndexWrite` writes a delete
/// marker into the prefix index for every deleted token, and `bDel` is a
/// per-`(prefix key, rowid)` flag committed with the poslist size. So the merged
/// prefix posting for a rowid is a tombstone iff ANY contributing main-term
/// posting for that rowid is a delete — a pure delete keeps `del=true` with no
/// positions, and an update whose old and new tokens share a prefix keeps
/// `del=true` WITH the new positions (size field `content_len*2 + 1`). For a
/// pure-insert flush every contributor has `del=false`, so this is unchanged.
fn merge_prefix_postings(groups: &[&[Posting]]) -> Vec<Posting> {
    use alloc::collections::BTreeMap;
    // rowid -> (delete flag, per-column positions kept sorted+deduplicated).
    let mut by_rowid: BTreeMap<i64, (bool, Vec<Vec<u32>>)> = BTreeMap::new();
    for postings in groups {
        for p in *postings {
            let entry = by_rowid
                .entry(p.rowid)
                .or_insert_with(|| (false, alloc::vec![Vec::new(); p.cols.len()]));
            entry.0 |= p.del;
            if entry.1.len() < p.cols.len() {
                entry.1.resize(p.cols.len(), Vec::new());
            }
            for (c, positions) in p.cols.iter().enumerate() {
                entry.1[c].extend_from_slice(positions);
            }
        }
    }
    by_rowid
        .into_iter()
        .map(|(rowid, (del, mut cols))| {
            for col in &mut cols {
                col.sort_unstable();
                col.dedup();
            }
            Posting { rowid, cols, del }
        })
        .collect()
}

/// Build the segment index for the main terms index plus, for each configured
/// prefix length in `prefixes` (character counts, in declared order), a prefix
/// index. sqlite writes all of a bulk rebuild's indexes into a SINGLE segment
/// (segid 1): the merged term stream is the main `'0'`-prefixed terms followed by
/// each prefix index's terms keyed `'1'`, `'2'`, … (the prefix byte is
/// `FTS5_MAIN_PREFIX + i + 1`). A prefix index's term for byte-prefix `p` is the
/// merge of every main term's doclist whose term starts with `p` (positions
/// preserved). Since `'0' < '1' < '2' < …`, appending main terms then each prefix
/// index in turn yields the globally ascending key order the writer requires.
pub(crate) fn build_segment_prefixed(
    terms: &[(Vec<u8>, Vec<Posting>)],
    n_docs: u64,
    col_totals: &[u64],
    doc_sizes: &[(i64, Vec<u64>)],
    pgsz: usize,
    cookie: u32,
    prefixes: &[usize],
) -> Segment {
    let segid = 1;
    let (leaves, idx, dlidx) = build_segment_leaves(terms, pgsz, segid, prefixes);

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
    // Doclist-index pages (dlidx=1 rowids) for spanning terms, if any.
    data.extend(dlidx);

    let docsize = build_docsize(doc_sizes);
    Segment { data, idx, docsize }
}

/// Encode the per-document `%_docsize` rows (`(rowid, per-column token counts as a
/// varint list)`) — shared by the bulk builder and the incremental append path.
pub(crate) fn build_docsize(doc_sizes: &[(i64, Vec<u64>)]) -> Vec<(i64, Vec<u8>)> {
    doc_sizes
        .iter()
        .map(|(rowid, sizes)| {
            let mut sz = Vec::new();
            for &s in sizes {
                put_varint(&mut sz, s);
            }
            (*rowid, sz)
        })
        .collect()
}

/// Build ONLY the leaf pages, `%_idx` rows, and doclist-index pages for a segment
/// with the given `segid` (the main `'0'` terms followed by each prefix index).
/// This is the segid-parameterized core shared by [`build_segment_prefixed`] (the
/// bulk single-segment rebuild, always `segid = 1`) and the incremental
/// level-0-append path (which allocates a fresh segid). It carries no
/// averages/structure record — those are index-global and owned by the caller.
fn build_segment_leaves(
    terms: &[(Vec<u8>, Vec<Posting>)],
    pgsz: usize,
    segid: i64,
    prefixes: &[usize],
) -> SegParts {
    build_segment_leaves_mode(terms, pgsz, segid, prefixes, false)
}

/// Core of [`build_segment_leaves`] with an explicit writer mode. `merge_mode ==
/// false` uses sqlite's bulk `fts5FlushOneHash` leaf packing (a fresh level-0
/// append or a one-shot bulk rebuild); `merge_mode == true` uses the
/// incremental-merge `fts5IndexMergeLevel` packing (a crisis/automerge OUTPUT
/// segment), which splits a spanning term's doclist across leaves at a different
/// byte boundary. The two agree byte-for-byte for single-leaf outputs and diverge
/// only when a term's doclist spans several leaves. Both derive the prefix
/// indexes (`'1'`/`'2'`/…) from the main `'0'` terms identically.
fn build_segment_leaves_mode(
    terms: &[(Vec<u8>, Vec<Posting>)],
    pgsz: usize,
    segid: i64,
    prefixes: &[usize],
    merge_mode: bool,
) -> SegParts {
    use alloc::collections::BTreeMap;
    let mut w = SegWriter::new(pgsz.max(16), segid);
    w.merge_mode = merge_mode;
    for (term, postings) in terms {
        w.add_term(term, postings);
    }
    // Each prefix index i is keyed with byte MAIN_PREFIX + i + 1 ('1', '2', …).
    for (i, &n_char) in prefixes.iter().enumerate() {
        let idx_byte = MAIN_PREFIX + (i as u8) + 1;
        // Group full terms by their n_char-character byte-prefix (BTreeMap keeps
        // the prefixes ascending, matching the required key order).
        let mut groups: BTreeMap<&[u8], Vec<&[Posting]>> = BTreeMap::new();
        for (term, postings) in terms {
            if let Some(nb) = prefix_bytelen(term, n_char) {
                groups.entry(&term[..nb]).or_default().push(postings);
            }
        }
        for (pfx, parts) in groups {
            let merged = merge_prefix_postings(&parts);
            let mut key = Vec::with_capacity(pfx.len() + 1);
            key.push(idx_byte);
            key.extend_from_slice(pfx);
            w.add_key(&key, &merged);
        }
    }
    if terms.is_empty() {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        w.finish()
    }
}

/// The `%_data`/`%_idx`/`%_docsize` rows for ONE segment written with a given
/// segid — the incremental-append (and crisis-merge) product. Unlike [`Segment`]
/// it carries NO averages/structure record (those are index-global, owned by the
/// caller which merges the [`SegStructure`]).
pub(crate) struct SegmentBlock {
    /// Leaf `%_data` rows `(rowid, block)` for this segid (rowid = segid<<37|pgno).
    pub data: Vec<(i64, Vec<u8>)>,
    /// `%_idx` rows (each carries this segment's segid).
    pub idx: Vec<IdxRow>,
    /// `%_docsize` rows for this write's documents.
    pub docsize: Vec<(i64, Vec<u8>)>,
    /// Number of leaf pages (= pgnoLast for the structure record).
    pub n_leaves: i64,
}

/// Build the `%_data`/`%_idx`/`%_docsize` rows for a single segment with `segid`
/// from `terms` (sorted ascending by raw term bytes) and this write's
/// `doc_sizes`, honoring configured `prefixes`. Used both to APPEND a fresh
/// level-0 segment (incremental write) and to write the merged output of a
/// crisis-merge (a fresh segid over the whole corpus). The leaf CONTENT is
/// identical to what [`build_segment_prefixed`] produces — only the segid (and so
/// the `%_data` rowids / `%_idx` segid column) differs.
pub(crate) fn build_segment_block(
    terms: &[(Vec<u8>, Vec<Posting>)],
    doc_sizes: &[(i64, Vec<u64>)],
    pgsz: usize,
    segid: i64,
    prefixes: &[usize],
) -> SegmentBlock {
    let (leaves, idx, dlidx) = build_segment_leaves(terms, pgsz, segid, prefixes);
    let mut data: Vec<(i64, Vec<u8>)> = Vec::new();
    for (i, leaf) in leaves.iter().enumerate() {
        data.push((segment_leaf_rowid(segid, i as i64 + 1), leaf.clone()));
    }
    data.extend(dlidx);
    SegmentBlock {
        data,
        idx,
        docsize: build_docsize(doc_sizes),
        n_leaves: leaves.len() as i64,
    }
}

/// Build the merged-output segment of an automerge/crisis `fts5IndexMergeLevel`
/// step, using sqlite's INCREMENTAL-MERGE writer semantics (see
/// [`SegWriter::merge_mode`]). Main-index terms only (no `prefixes`) and no
/// `%_docsize` (merges do not touch per-doc sizes). The leaf CONTENT is identical
/// to [`build_segment_block`] for single-leaf outputs but diverges — matching
/// sqlite — at leaf boundaries when the merged segment spans several leaves.
pub(crate) fn build_merged_segment_block(
    terms: &[(Vec<u8>, Vec<Posting>)],
    pgsz: usize,
    segid: i64,
) -> SegmentBlock {
    let (leaves, idx, dlidx) = {
        let mut w = SegWriter::new(pgsz.max(16), segid);
        w.merge_mode = true;
        for (term, postings) in terms {
            w.add_term(term, postings);
        }
        if terms.is_empty() {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            w.finish()
        }
    };
    let mut data: Vec<(i64, Vec<u8>)> = Vec::new();
    for (i, leaf) in leaves.iter().enumerate() {
        data.push((segment_leaf_rowid(segid, i as i64 + 1), leaf.clone()));
    }
    data.extend(dlidx);
    SegmentBlock {
        data,
        idx,
        docsize: Vec::new(),
        n_leaves: leaves.len() as i64,
    }
}

/// The prefix-aware sibling of [`build_merged_segment_block`]: the input `terms`
/// carry FULL stored keys (the `'0'`/`'1'`/`'2'`… index-prefix byte included, as
/// produced by [`merge_segments_keepdel_full`]), so they are written verbatim with
/// [`SegWriter::add_key`] rather than re-deriving prefixes from the main terms.
/// Keys must already be in globally ascending order (main terms, then each prefix
/// index) — which the merge preserves.
pub(crate) fn build_merged_segment_block_full(
    terms: &[(Vec<u8>, Vec<Posting>)],
    pgsz: usize,
    segid: i64,
) -> SegmentBlock {
    let (leaves, idx, dlidx) = {
        let mut w = SegWriter::new(pgsz.max(16), segid);
        w.merge_mode = true;
        for (key, postings) in terms {
            w.add_key(key, postings);
        }
        if terms.is_empty() {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            w.finish()
        }
    };
    let mut data: Vec<(i64, Vec<u8>)> = Vec::new();
    for (i, leaf) in leaves.iter().enumerate() {
        data.push((segment_leaf_rowid(segid, i as i64 + 1), leaf.clone()));
    }
    data.extend(dlidx);
    SegmentBlock {
        data,
        idx,
        docsize: Vec::new(),
        n_leaves: leaves.len() as i64,
    }
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
    // The poslist size varint is `(content_len << 1) | delete_flag`: its low bit is
    // FTS5's per-doc DELETE marker (a tombstone shadowing an older segment's entry).
    // The writer here never emits a tombstone (it always bulk-rebuilds from
    // `_content`), so a set low bit means we are reading an `sqlite3`-written delete
    // entry — reject it so every caller (single- and multi-segment) bails to the
    // `_content` scan rather than silently treating a delete as a zero-position
    // posting. The multi-segment merge ([`decode_term_strict`]) relies on this to
    // honor segment-layering precedence (newest segment wins) by bailing.
    if size2 & 1 != 0 {
        return None;
    }
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
///
/// Now used only by the in-crate decoder unit tests: the live phrase/NEAR routes
/// decode per-segment via [`decode_term_strict`] (which distinguishes an absent term
/// from an unservable segment), and the bare-term/prefix/boolean routes go through
/// [`merge_segments`]. It is retained as the readable single-segment reference the
/// strict decoder mirrors.
#[cfg(test)]
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

/// Decode EVERY term whose key begins with `prefix` in ONE segment's leaf pages,
/// distinguishing a SERVABLE result (the union of the matching terms' postings,
/// possibly empty when no term matches) from a BAIL (unparseable leaf / interior
/// page / tombstone). The prefix-query analogue of [`decode_term_strict`].
///
/// It matches every term key that STARTS with the prefixed key (`MAIN_PREFIX` then
/// `prefix` bytes), which — because the leaf term keys are stored in ascending
/// sorted order — is exactly the contiguous run of indexed terms with that prefix.
/// Each matching term's doclist (which may itself span leaves) is decoded and the
/// postings are concatenated in term order (postings within one term are ascending
/// by rowid, but two different terms may share a rowid, so the caller merges/dedups
/// by rowid — see [`prefix_rowids`]/[`merge_segments`]). Returning [`SegDecode`]
/// (rather than `Option`) keeps "no prefix match in this segment" apart from "this
/// segment is unservable", which the multi-segment merge needs.
///
/// An empty `prefix` matches every term (every key starts with the lone
/// `MAIN_PREFIX` byte); callers reject the empty-prefix shape upstream, matching
/// sqlite (which rejects a bare `'*'`).
fn decode_prefix_strict(leaves: &[&[u8]], prefix: &[u8]) -> SegDecode {
    let want = term_key(prefix);
    let mut views: Vec<LeafView> = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        match parse_leaf(leaf) {
            Some(v) => views.push(v),
            None => return SegDecode::Bail,
        }
    }
    let mut out: Vec<DecodedPosting> = Vec::new();
    for (li, view) in views.iter().enumerate() {
        for (ti, rec) in view.terms.iter().enumerate() {
            if !rec.key.starts_with(&want) {
                continue;
            }
            match gather_doclist_runs(leaves, li, ti, rec.doclist_start, &views)
                .and_then(|runs| decode_spanning_doclist(&runs))
            {
                Some(postings) => out.extend(postings),
                None => return SegDecode::Bail, // tombstone / malformed doclist
            }
        }
    }
    SegDecode::Postings(out)
}

/// One segment's identity, parsed from the structure record: its `segid` and the
/// inclusive range of height-0 leaf page numbers (`pgno_first..=pgno_last`).
struct SegmentLoc {
    segid: i64,
    pgno_first: i64,
    pgno_last: i64,
}

/// Parse the structure record (the inverse of [`structure`]) into the locations of
/// EVERY height-0 segment, across all levels, or `None` for an empty index or a
/// malformed record. This does not require a single
/// segment: an `sqlite3`-written FTS5 table accumulates several segments per level
/// (and several levels) after many inserts until an `'optimize'`/merge collapses
/// them, so the multi-segment bare-term/boolean/prefix routes ([`decode_term_strict`])
/// need every segment's leaf range.
///
/// Layout: 4-byte BE cookie, then varints `nLevel`, `nSegment`, `nWriteCounter`,
/// then per level `nMerge`, `nSeg`, then per segment `segid`, `pgnoFirst`,
/// `pgnoLast`. The `(segid, pgnoFirst, pgnoLast)` triples are returned in the order
/// they appear in the record (newest level/segment first); the merge that consumes
/// them is order-independent (it bails on any docid shared across segments), so the
/// order only matters for the documentation of layering precedence. An empty index
/// (`nLevel == 0`/`nSegment == 0`) yields `None` so the caller scans — there is
/// nothing to index-route.
fn all_segments(structure: &[u8]) -> Option<Vec<SegmentLoc>> {
    let mut pos = 4usize; // skip the 4-byte config cookie
    let n_level = read_varint(structure, &mut pos)?;
    let n_segment = read_varint(structure, &mut pos)?;
    let _n_write_counter = read_varint(structure, &mut pos)?;
    if n_level == 0 || n_segment == 0 {
        return None; // empty index: nothing to route
    }
    let mut segs: Vec<SegmentLoc> = Vec::new();
    for _ in 0..n_level {
        let _n_merge = read_varint(structure, &mut pos)?;
        let n_seg = read_varint(structure, &mut pos)?;
        for _ in 0..n_seg {
            let segid = read_varint(structure, &mut pos)? as i64;
            let pgno_first = read_varint(structure, &mut pos)? as i64;
            let pgno_last = read_varint(structure, &mut pos)? as i64;
            if segid <= 0 || pgno_first < 1 || pgno_last < pgno_first {
                return None;
            }
            segs.push(SegmentLoc {
                segid,
                pgno_first,
                pgno_last,
            });
        }
    }
    // The triple count must match the header's `nSegment` (a structurally sound
    // record); a mismatch means a malformed/unsupported record → caller scans.
    if segs.len() as u64 != n_segment {
        return None;
    }
    Some(segs)
}

/// Gather the leaf blobs (in page order) of EVERY height-0 segment in a `%_data`
/// index, as one leaf set per segment, or `None` if the shape is unservable
/// (empty index, a missing leaf, or a malformed structure record). Shared by the
/// multi-segment bare-term/boolean/prefix lookups; the per-segment grouping lets
/// the merge enforce the "each docid in at most one segment" safety check.
///
/// Bumps the test-only [`INDEX_ROUTE_HITS`] counter once per servable call, so a
/// multi-segment query that takes the index route counts as one hit regardless of
/// how many segments (or terms) it decodes.
fn segments_leaves(data: &[(i64, Vec<u8>)]) -> Option<Vec<Vec<&[u8]>>> {
    let structure = &data.iter().find(|(id, _)| *id == STRUCTURE_ROWID)?.1;
    let locs = all_segments(structure)?;
    let mut out: Vec<Vec<&[u8]>> = Vec::with_capacity(locs.len());
    for loc in &locs {
        let mut leaves: Vec<&[u8]> = Vec::new();
        for pgno in loc.pgno_first..=loc.pgno_last {
            let rid = segment_leaf_rowid(loc.segid, pgno);
            let blob = &data.iter().find(|(id, _)| *id == rid)?.1;
            leaves.push(blob.as_slice());
        }
        out.push(leaves);
    }
    #[cfg(test)]
    INDEX_ROUTE_HITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    Some(out)
}

/// The outcome of decoding a term/prefix/tree in ONE segment of a multi-segment
/// index. `Bail` means the segment is unservable for a safe merge — an
/// interior/doclist-index page, a malformed leaf, OR a DELETE tombstone (a set
/// poslist low bit; see [`decode_poslist`]) whose layering precedence this slice
/// does not resolve — so the whole query must fall back to the `_content` scan.
/// `Postings` is a servable result (possibly empty when the term is absent in this
/// segment).
enum SegDecode {
    Bail,
    Postings(Vec<DecodedPosting>),
}

/// Decode `term`'s doclist in one segment's `leaves`, distinguishing a SERVABLE
/// result (term found → its postings; term absent → empty) from a BAIL
/// (unparseable leaf / interior page / tombstone). This is the multi-segment
/// analogue of [`decode_term`], which collapses "absent" and "unservable" into a
/// single `None`; the merge needs them apart because an absent term in one segment
/// is fine (union with the others) while an unservable segment forces a scan.
fn decode_term_strict(leaves: &[&[u8]], term: &[u8]) -> SegDecode {
    let key = term_key(term);
    let mut views: Vec<LeafView> = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        match parse_leaf(leaf) {
            Some(v) => views.push(v),
            None => return SegDecode::Bail, // interior/dlidx/malformed leaf
        }
    }
    for (li, view) in views.iter().enumerate() {
        for (ti, rec) in view.terms.iter().enumerate() {
            if rec.key != key {
                continue;
            }
            return match gather_doclist_runs(leaves, li, ti, rec.doclist_start, &views)
                .and_then(|runs| decode_spanning_doclist(&runs))
            {
                Some(postings) => SegDecode::Postings(postings),
                // A tombstone (rejected poslist) or a malformed doclist: bail.
                None => SegDecode::Bail,
            };
        }
    }
    SegDecode::Postings(Vec::new()) // servable: term absent in this segment
}

/// Merge the same `decode` over EVERY segment of `data` into one ascending,
/// deduplicated posting list, or `None` if the merge is not provably correct (so
/// the caller falls back to the `_content` scan). `decode` runs the per-segment
/// term/prefix/tree decode (returning [`SegDecode`]).
///
/// SAFETY — this honors FTS5 segment layering WITHOUT a full
/// tombstone/precedence merge by bailing whenever precedence could matter:
///
/// * any segment returns [`SegDecode::Bail`] (interior page / tombstone /
///   malformed) → `None`;
/// * a docid appears in MORE THAN ONE segment (an update wrote a newer, shadowing
///   entry, so a naive union could keep a stale or duplicated doc) → `None`.
///
/// Otherwise the index is a pure-insert history: each docid lives in exactly one
/// segment, no entry is a tombstone, so the UNION of the segments' postings is
/// exactly the live document set — identical to the `_content` scan. The returned
/// postings are sorted ascending by rowid and deduplicated (the no-overlap check
/// guarantees there is nothing to dedup, but the sort makes the order canonical).
fn merge_segments(
    data: &[(i64, Vec<u8>)],
    decode: impl Fn(&[&[u8]]) -> SegDecode,
) -> Option<Vec<DecodedPosting>> {
    let segments = segments_leaves(data)?;
    let mut all: Vec<DecodedPosting> = Vec::new();
    let mut seen_segidx: Vec<(i64, usize)> = Vec::new(); // (rowid, which segment)
    for (si, leaves) in segments.iter().enumerate() {
        match decode(leaves) {
            SegDecode::Bail => return None,
            SegDecode::Postings(postings) => {
                for p in postings {
                    seen_segidx.push((p.rowid, si));
                    all.push(p);
                }
            }
        }
    }
    // Bail if any docid appears in more than one segment (ambiguous precedence).
    seen_segidx.sort_unstable();
    for w in seen_segidx.windows(2) {
        if w[0].0 == w[1].0 && w[0].1 != w[1].1 {
            return None;
        }
    }
    all.sort_by_key(|p| p.rowid);
    all.dedup_by_key(|p| p.rowid);
    Some(all)
}

// ---------------------------------------------------------------------------
// Tombstone-PRESERVING reader + merge (D2e-1 automerge).
//
// The reader above ([`decode_poslist`], [`merge_segments`]) BAILS on a DELETE
// marker (poslist low bit set) because the query paths cannot resolve layering
// precedence. The automerge/crisis WRITE path, by contrast, must reproduce
// sqlite's `fts5IndexMergeLevel` byte-for-byte, which means it must READ every
// posting (tombstones included), MERGE the input segments in term+rowid order
// with newest-segment-wins precedence, and re-apply sqlite's key-annihilation
// rule. These variants keep the delete flag instead of bailing.

/// Like [`decode_poslist`] but returns the DELETE flag (`size2 & 1`) rather than
/// bailing on a tombstone. The inverse of [`poslist`] including its low bit.
fn decode_poslist_keepdel(buf: &[u8], pos: &mut usize) -> Option<(Vec<Vec<u32>>, bool)> {
    let size2 = read_varint(buf, pos)?;
    let del = (size2 & 1) != 0;
    let content_len = (size2 / 2) as usize;
    let end = pos.checked_add(content_len)?;
    if end > buf.len() {
        return None;
    }
    let mut cols: Vec<Vec<u32>> = Vec::new();
    let mut col = 0usize;
    let mut p = *pos;
    cols.push(Vec::new());
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
    Some((cols, del))
}

/// Like [`decode_spanning_doclist`] but builds [`Posting`]s (carrying the DELETE
/// flag via [`decode_poslist_keepdel`]) instead of position-only [`DecodedPosting`]s.
fn decode_doclist_keepdel(runs: &[DoclistRun]) -> Option<Vec<Posting>> {
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
        let (cols, del) = decode_poslist_keepdel(&buf, &mut pos)?;
        if pos > end {
            return None;
        }
        out.push(Posting { rowid, cols, del });
    }
    if pos != end {
        return None;
    }
    Some(out)
}

/// Parse EVERY term of a simple main-index segment into `(term, postings)` with
/// the term's `MAIN_PREFIX` byte stripped (ready to feed back to
/// [`build_segment_block`]). `None` (bail) on an interior/doclist-index page (an
/// unparseable leaf) or a non-`MAIN_PREFIX` term (a prefix-index key) — the
/// automerge path only services the main index. Postings preserve DELETE markers.
pub(crate) fn read_segment_postings(leaves: &[&[u8]]) -> Option<Vec<(Vec<u8>, Vec<Posting>)>> {
    let mut views: Vec<LeafView> = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        views.push(parse_leaf(leaf)?);
    }
    let mut out: Vec<(Vec<u8>, Vec<Posting>)> = Vec::new();
    for (li, view) in views.iter().enumerate() {
        for (ti, rec) in view.terms.iter().enumerate() {
            // Only main-index terms (key '0'…). A prefix-index term ('1'…) means
            // this is a prefixed segment the automerge path does not service.
            if rec.key.first() != Some(&MAIN_PREFIX) {
                return None;
            }
            let term = rec.key.get(1..)?.to_vec();
            let runs = gather_doclist_runs(leaves, li, ti, rec.doclist_start, &views)?;
            let postings = decode_doclist_keepdel(&runs)?;
            out.push((term, postings));
        }
    }
    Some(out)
}

/// Like [`read_segment_postings`] but keeps the FULL stored key (the index-prefix
/// byte plus the term bytes) for every term, so a prefix-configured segment (main
/// `'0'` terms followed by each prefix index's `'1'`/`'2'`/… terms) round-trips
/// through the tombstone-preserving merge path. Feed the result back to
/// [`build_merged_segment_block_full`], which re-emits the keys verbatim.
/// Postings preserve DELETE markers. `None` (bail) on an interior/doclist-index
/// page (an unparseable leaf).
pub(crate) fn read_segment_postings_full(leaves: &[&[u8]]) -> Option<Vec<(Vec<u8>, Vec<Posting>)>> {
    let mut views: Vec<LeafView> = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        views.push(parse_leaf(leaf)?);
    }
    let mut out: Vec<(Vec<u8>, Vec<Posting>)> = Vec::new();
    for (li, view) in views.iter().enumerate() {
        for (ti, rec) in view.terms.iter().enumerate() {
            if rec.key.is_empty() {
                return None; // empty key: unexpected, bail
            }
            let key = rec.key.clone();
            let runs = gather_doclist_runs(leaves, li, ti, rec.doclist_start, &views)?;
            let postings = decode_doclist_keepdel(&runs)?;
            out.push((key, postings));
        }
    }
    Some(out)
}

/// Merge already-parsed segments (`segs[0]` OLDEST … `segs.last()` NEWEST — the
/// `pLvl->aSeg[]` order) into one ascending-term / ascending-rowid stream,
/// newest-segment-wins per rowid, applying sqlite's key-annihilation rule.
///
/// Port of the entry-emitting core of `fts5IndexMergeLevel`: for each distinct
/// `(term, rowid)` only the newest segment's posting survives (older duplicates
/// are shadowed). That surviving posting is then DROPPED
/// (`if( pSegIter->nPos==0 && (bOldest || pSegIter->bDel==0) ) continue;`) when it
/// carries no positions AND (`b_oldest` OR it is not a delete) — so a pure
/// tombstone is annihilated in the oldest output segment but PRESERVED otherwise,
/// where it must still shadow un-merged higher levels.
fn merge_level_postings(
    segs: &[Vec<(Vec<u8>, Vec<Posting>)>],
    b_oldest: bool,
) -> Vec<(Vec<u8>, Vec<Posting>)> {
    use alloc::collections::BTreeMap;
    // term -> (rowid -> Posting); iterating segments oldest→newest and inserting
    // unconditionally lets the newest write win each (term, rowid).
    let mut map: BTreeMap<Vec<u8>, BTreeMap<i64, Posting>> = BTreeMap::new();
    for seg in segs {
        for (term, postings) in seg {
            let e = map.entry(term.clone()).or_default();
            for p in postings {
                e.insert(p.rowid, p.clone());
            }
        }
    }
    let mut out: Vec<(Vec<u8>, Vec<Posting>)> = Vec::new();
    for (term, by_rowid) in map {
        let mut ps: Vec<Posting> = Vec::new();
        for (_rowid, p) in by_rowid {
            let empty = p.cols.iter().all(|c| c.is_empty());
            if empty && (b_oldest || !p.del) {
                continue;
            }
            ps.push(p);
        }
        if !ps.is_empty() {
            out.push((term, ps));
        }
    }
    out
}

/// Read each segment's leaves ([`read_segment_postings`]) then
/// [`merge_level_postings`] them. `seg_leaves[0]` is the OLDEST segment on the
/// level. `None` if any segment is unservable (interior/dlidx page, prefix term).
pub(crate) fn merge_segments_keepdel(
    seg_leaves: &[Vec<&[u8]>],
    b_oldest: bool,
) -> Option<Vec<(Vec<u8>, Vec<Posting>)>> {
    let mut segs: Vec<Vec<(Vec<u8>, Vec<Posting>)>> = Vec::with_capacity(seg_leaves.len());
    for leaves in seg_leaves {
        segs.push(read_segment_postings(leaves)?);
    }
    Some(merge_level_postings(&segs, b_oldest))
}

/// The prefix-aware sibling of [`merge_segments_keepdel`]: reads each segment with
/// [`read_segment_postings_full`] (FULL keys, so the main `'0'` and prefix
/// `'1'`/`'2'`/… term streams are merged together, exactly as sqlite keeps them in
/// one segment) then [`merge_level_postings`] them. Because `merge_level_postings`
/// keys purely on the raw key bytes and `'0' < '1' < '2' < …`, the output stays in
/// the globally ascending key order the writer requires. `None` if any segment is
/// unservable (interior/dlidx page).
pub(crate) fn merge_segments_keepdel_full(
    seg_leaves: &[Vec<&[u8]>],
    b_oldest: bool,
) -> Option<Vec<(Vec<u8>, Vec<Posting>)>> {
    let mut segs: Vec<Vec<(Vec<u8>, Vec<Posting>)>> = Vec::with_capacity(seg_leaves.len());
    for leaves in seg_leaves {
        segs.push(read_segment_postings_full(leaves)?);
    }
    Some(merge_level_postings(&segs, b_oldest))
}

// ---------------------------------------------------------------------------
// integrity_check: decode the whole MAIN inverted index into a comparable
// (term, rowid, per-column positions) multiset so `PRAGMA integrity_check` can
// diff it against a re-tokenization of the `%_content` documents (sqlite's
// `sqlite3Fts5IntegrityCheck`). This is a READ-ONLY path — it never writes.
// ---------------------------------------------------------------------------

/// Like [`read_segment_postings`] but for the integrity checker: it reads only
/// the MAIN-index terms (`FTS5_MAIN_PREFIX` = `'0'`) and simply SKIPS any
/// prefix-index term (key byte `'1'`, `'2'`, …) rather than bailing, so a
/// prefix-configured table's main index can still be checked. `None` (bail) on a
/// genuinely unparseable leaf (an interior/doclist-index page) or a malformed key.
fn read_main_segment_postings(leaves: &[&[u8]]) -> Option<Vec<(Vec<u8>, Vec<Posting>)>> {
    let mut views: Vec<LeafView> = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        views.push(parse_leaf(leaf)?);
    }
    let mut out: Vec<(Vec<u8>, Vec<Posting>)> = Vec::new();
    for (li, view) in views.iter().enumerate() {
        for (ti, rec) in view.terms.iter().enumerate() {
            match rec.key.first() {
                Some(&MAIN_PREFIX) => {}
                // A prefix-index term ('1'…): not part of the main index — skip it.
                Some(_) => continue,
                None => return None, // empty key: unexpected, bail
            }
            let term = rec.key.get(1..)?.to_vec();
            let runs = gather_doclist_runs(leaves, li, ti, rec.doclist_start, &views)?;
            let postings = decode_doclist_keepdel(&runs)?;
            out.push((term, postings));
        }
    }
    Some(out)
}

/// The outcome of decoding an FTS5 table's whole main inverted index for the
/// integrity checker ([`scan_main_index`]).
pub(crate) enum MainIndexScan {
    /// The live main-index postings (term ascending, each term's postings
    /// rowid-ascending), decoded from a PURE-INSERT index — one with no DELETE
    /// tombstones and no docid shared across segments. This is directly comparable
    /// to a fresh re-tokenization of the `%_content` documents.
    Clean(Vec<(Vec<u8>, Vec<Posting>)>),
    /// A VALID FTS5 shape this read-only checker does not fully/safely decode: an
    /// index carrying tombstones or updated docids (delete-precedence layering), a
    /// term spanning onto an unparseable (interior/doclist-index) leaf, or a
    /// structure record we cannot parse. The caller SKIPS the term comparison
    /// (reports the table `ok`) rather than risk a false positive.
    Skip,
    /// The structure record references a height-0 leaf page that is ABSENT from
    /// `%_data` — impossible for a valid file. The caller reports the table
    /// malformed.
    Malformed,
}

/// Decode the MAIN inverted index of a self-content FTS5 table from its `%_data`
/// rows into a [`MainIndexScan`]. Conservative by construction: it returns
/// [`MainIndexScan::Clean`] only when the whole index is a pure-insert history it
/// can decode with certainty, [`MainIndexScan::Malformed`] only for a structurally
/// impossible file (a referenced leaf missing), and [`MainIndexScan::Skip`] for
/// every valid-but-not-fully-decodable shape — so a mismatch reported against a
/// `Clean` result is real.
pub(crate) fn scan_main_index(data: &[(i64, Vec<u8>)]) -> MainIndexScan {
    use alloc::collections::BTreeMap;
    let Some(structure) = data
        .iter()
        .find(|(id, _)| *id == STRUCTURE_ROWID)
        .map(|(_, b)| b.as_slice())
    else {
        return MainIndexScan::Skip; // no structure record: nothing safe to check
    };
    // Parse the structure record into per-level segment locations, keeping EMPTY
    // (nothing indexed) distinct from a real segment list.
    let mut pos = 4usize; // skip the 4-byte config cookie
    let (Some(n_level), Some(n_segment), Some(_wc)) = (
        read_varint(structure, &mut pos),
        read_varint(structure, &mut pos),
        read_varint(structure, &mut pos),
    ) else {
        return MainIndexScan::Skip;
    };
    if n_level == 0 || n_segment == 0 {
        return MainIndexScan::Clean(Vec::new()); // empty index (no documents)
    }
    let mut locs: Vec<SegmentLoc> = Vec::new();
    for _ in 0..n_level {
        let (Some(_n_merge), Some(n_seg)) = (
            read_varint(structure, &mut pos),
            read_varint(structure, &mut pos),
        ) else {
            return MainIndexScan::Skip;
        };
        for _ in 0..n_seg {
            let (Some(segid), Some(pgno_first), Some(pgno_last)) = (
                read_varint(structure, &mut pos),
                read_varint(structure, &mut pos),
                read_varint(structure, &mut pos),
            ) else {
                return MainIndexScan::Skip;
            };
            let (segid, pgno_first, pgno_last) =
                (segid as i64, pgno_first as i64, pgno_last as i64);
            if segid <= 0 || pgno_first < 1 || pgno_last < pgno_first {
                return MainIndexScan::Skip;
            }
            locs.push(SegmentLoc {
                segid,
                pgno_first,
                pgno_last,
            });
        }
    }
    if locs.len() as u64 != n_segment {
        return MainIndexScan::Skip;
    }

    // Read each segment. A referenced leaf that is MISSING from %_data is
    // malformed; an unparseable-but-plausible leaf (interior/dlidx page) is a
    // valid shape we skip; a tombstone or a cross-segment docid means update
    // history whose precedence we do not resolve → skip.
    let mut per_term: BTreeMap<Vec<u8>, Vec<Posting>> = BTreeMap::new();
    let mut seen: Vec<(Vec<u8>, i64, usize)> = Vec::new(); // (term, rowid, seg idx)
    for (si, loc) in locs.iter().enumerate() {
        let mut leaves: Vec<&[u8]> = Vec::new();
        for pgno in loc.pgno_first..=loc.pgno_last {
            let rid = segment_leaf_rowid(loc.segid, pgno);
            match data.iter().find(|(id, _)| *id == rid) {
                Some((_, b)) => leaves.push(b.as_slice()),
                None => return MainIndexScan::Malformed, // structure references a gone leaf
            }
        }
        let Some(terms) = read_main_segment_postings(&leaves) else {
            return MainIndexScan::Skip; // interior/doclist-index/unparseable leaf
        };
        for (term, postings) in terms {
            for p in &postings {
                if p.del {
                    return MainIndexScan::Skip; // tombstone: delete/update history
                }
                seen.push((term.clone(), p.rowid, si));
            }
            per_term.entry(term).or_default().extend(postings);
        }
    }
    // A (term, rowid) present in more than one segment is an update's shadowing
    // write — precedence we do not resolve here, so skip.
    seen.sort();
    for w in seen.windows(2) {
        if w[0].0 == w[1].0 && w[0].1 == w[1].1 && w[0].2 != w[1].2 {
            return MainIndexScan::Skip;
        }
    }
    let mut out: Vec<(Vec<u8>, Vec<Posting>)> = Vec::with_capacity(per_term.len());
    for (term, mut ps) in per_term {
        ps.sort_by_key(|p| p.rowid);
        out.push((term, ps));
    }
    MainIndexScan::Clean(out)
}

/// Look up `term` in an FTS5 index given its `%_data` rows, returning the rowids
/// of the documents that contain the term (ascending), or `None` if the index
/// shape is one the leaf reader cannot serve.
///
/// `data` is the `(id, block)` rows of the `%_data` shadow table (the structure
/// record at id 10 plus the height-0 leaves). This is the wiring used by `MATCH`:
/// it parses the structure record, gathers EVERY height-0 segment's leaves in page
/// order and decodes the term in each, UNIONing the postings via
/// [`merge_segments`] ([`decode_term_in_data`]). A `None` return (an
/// interior/doclist-index page, a missing leaf, a malformed record, a tombstone, or
/// a docid shared across segments) tells the caller to fall back to the `%_content`
/// document scan. A present term genuinely absent from a servable index returns
/// `Some(vec![])`.
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

/// Reduce a bag of prefix-matched postings (the union of several terms' doclists,
/// possibly sharing rowids and NOT globally sorted) to the deduplicated, ascending
/// rowid list a prefix `MATCH` returns. `keep` filters which postings count (the
/// whole posting for a table-wide prefix; only those with a hit in the scoped
/// column for `col : pre*`).
///
/// Sorting + dedup here, rather than an incremental sorted-merge per term, keeps
/// the result correct regardless of term enumeration order: a document with two
/// distinct prefix-matching terms appears once, and the final order matches the
/// ascending order the `_content` scan (and `decode_term`-based routes) produce.
fn prefix_rowids(
    mut postings: Vec<DecodedPosting>,
    keep: impl Fn(&DecodedPosting) -> bool,
) -> Vec<i64> {
    let mut rowids: Vec<i64> = postings
        .drain(..)
        .filter(|p| keep(p))
        .map(|p| p.rowid)
        .collect();
    rowids.sort_unstable();
    rowids.dedup();
    rowids
}

/// Look up a single bare PREFIX term `prefix*` in a single-segment FTS5 index and
/// return the rowids of the documents that contain ANY indexed term beginning with
/// `prefix` (ascending, deduplicated), or `None` if the index shape is one the
/// single-segment leaf reader cannot serve (so the caller falls back to the scan).
///
/// The prefix sibling of [`lookup_term_rowids`]: it gathers the one height-0
/// segment's leaves ONCE and decodes every term whose stored key begins with
/// `prefix` ([`decode_prefix_strict`]), then unions their docids. Because the index stores
/// the tokenized (and, under `porter`, stemmed) term forms and the scan's prefix
/// predicate tests `doc_token.starts_with(query_prefix)` over those SAME forms (the
/// query prefix is tokenized/stemmed identically by the recognizer), the set of
/// documents with a term starting with `prefix` is exactly the scan's match set.
/// A servable segment with no matching term yields `Some(vec![])`.
pub(crate) fn lookup_prefix_rowids(data: &[(i64, Vec<u8>)], prefix: &[u8]) -> Option<Vec<i64>> {
    let postings = merge_segments(data, |leaves| decode_prefix_strict(leaves, prefix))?;
    Some(prefix_rowids(postings, |_| true))
}

/// Column-scoped sibling of [`lookup_prefix_rowids`]: keep only documents where a
/// term beginning with `prefix` occurs in COLUMN `column` (its position in the
/// table's full column list, from 0) — exactly the scan's `col : pre*` set. A
/// posting counts iff its per-column position list for `column` is non-empty.
pub(crate) fn lookup_prefix_rowids_in_column(
    data: &[(i64, Vec<u8>)],
    prefix: &[u8],
    column: usize,
) -> Option<Vec<i64>> {
    let postings = merge_segments(data, |leaves| decode_prefix_strict(leaves, prefix))?;
    Some(prefix_rowids(postings, |p| {
        p.cols.get(column).is_some_and(|c| !c.is_empty())
    }))
}

/// Resolve `term`'s postings from a `%_data` index — across ONE OR MORE height-0
/// segments — or `None` if the shape is unservable. Shared by [`lookup_term_rowids`]
/// and [`lookup_term_rowids_in_column`]: it parses the structure record, gathers
/// every segment's leaves in page order, decodes the term in each, and UNIONs the
/// postings via [`merge_segments`] (which bails — `None` → scan — on a tombstone or
/// a docid shared across segments). A servable index whose term is absent
/// everywhere returns `Some(vec![])`. For the common single-segment index this is
/// exactly the old single-segment decode; for a multi-segment pure-insert index it
/// returns the same set the `_content` scan would.
fn decode_term_in_data(data: &[(i64, Vec<u8>)], term: &[u8]) -> Option<Vec<DecodedPosting>> {
    merge_segments(data, |leaves| decode_term_strict(leaves, term))
}

/// Column `c`'s position list of a posting (empty if the term never occurs there).
fn col(p: &DecodedPosting, c: usize) -> &[u32] {
    p.cols.get(c).map(Vec::as_slice).unwrap_or(&[])
}

/// Walk the docid-aligned intersection of two postings lists (each ascending by
/// rowid, as the doclist is) and call `adj` on every shared document, collecting the
/// rowids for which it returns `true`, ascending. Shared by the table-wide and
/// column-scoped phrase lookups.
fn phrase_intersect(
    postings_a: &[DecodedPosting],
    postings_b: &[DecodedPosting],
    adj: impl Fn(&DecodedPosting, &DecodedPosting) -> bool,
) -> Vec<i64> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < postings_a.len() && j < postings_b.len() {
        let (a, b) = (&postings_a[i], &postings_b[j]);
        match a.rowid.cmp(&b.rowid) {
            core::cmp::Ordering::Less => i += 1,
            core::cmp::Ordering::Greater => j += 1,
            core::cmp::Ordering::Equal => {
                if adj(a, b) {
                    out.push(a.rowid);
                }
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Whether some column `c` of a document holds a consecutive run `p, p+1, …,
/// p+K-1` whose positions belong to `terms[0], terms[1], …, terms[K-1]`
/// respectively. `cols[i][c]` is
/// term `i`'s ascending (possibly empty) position list in column `c`. When `column`
/// is `Some(c)` only that column is examined (column-scoped phrase); when `None`
/// any column may carry the run (table-wide).
///
/// This is the index analogue of the scan's [`crate::vtab`] `fts5_phrase_starts`
/// over a K-token phrase: a phrase matches a single column's token list at start
/// `s` iff `doc[s + i] == terms[i]` for every `i`, i.e. term `i` at position
/// `s + i` in THE SAME column. We walk each candidate start `p` (a position of
/// `terms[0]`) and verify, by binary search on each later term's ascending
/// position list, that `p + i` is present — O(P · K log P) over the document's
/// positions. Repeated words (`terms[i] == terms[j]`) just check the same position
/// list at different offsets, so `"a a a"` matches positions `p, p+1, p+2` of the
/// one term's list, exactly as the scan would.
fn phrase_run_matches(cols: &[Vec<&[u32]>], column: Option<usize>) -> bool {
    debug_assert!(!cols.is_empty());
    let k = cols.len();
    // The number of columns is the max any term's posting carries (absent columns
    // are empty and never start a run).
    let ncols = cols.iter().map(Vec::len).max().unwrap_or(0);
    let in_col = |c: usize| -> bool {
        // term[0]'s positions in this column are the candidate run starts.
        let first = cols[0].get(c).copied().unwrap_or(&[]);
        first.iter().any(|&p| {
            (1..k).all(|i| {
                let want = match p.checked_add(i as u32) {
                    Some(w) => w,
                    None => return false,
                };
                let list = cols[i].get(c).copied().unwrap_or(&[]);
                list.binary_search(&want).is_ok()
            })
        })
    };
    match column {
        Some(c) => in_col(c),
        None => (0..ncols).any(in_col),
    }
}

/// Walk the docid-aligned intersection of K postings lists (each ascending by
/// rowid) and keep the rowids whose per-column positions hold a consecutive run of
/// the K terms (see [`phrase_run_matches`]), ascending. The K-term generalization
/// of [`phrase_intersect`]: it advances every list whose head rowid equals the
/// running minimum, so a document survives only when ALL K terms post it.
fn phrase_intersect_k(postings: &[Vec<DecodedPosting>], column: Option<usize>) -> Vec<i64> {
    let k = postings.len();
    if k == 0 || postings.iter().any(Vec::is_empty) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut idx = alloc::vec![0usize; k];
    loop {
        // The smallest head rowid across the K lists; stop when any list is drained.
        let mut min_rowid = i64::MAX;
        for (i, p) in postings.iter().enumerate() {
            if idx[i] >= p.len() {
                return out;
            }
            min_rowid = min_rowid.min(p[idx[i]].rowid);
        }
        // Do all K lists agree on this rowid? If so it is a shared document.
        let all_equal = postings
            .iter()
            .zip(&idx)
            .all(|(p, &j)| p[j].rowid == min_rowid);
        if all_equal {
            let cols: Vec<Vec<&[u32]>> = postings
                .iter()
                .zip(&idx)
                .map(|(p, &j)| p[j].cols.iter().map(Vec::as_slice).collect())
                .collect();
            if phrase_run_matches(&cols, column) {
                out.push(min_rowid);
            }
        }
        // Advance every list still sitting on the minimum rowid.
        for (i, p) in postings.iter().enumerate() {
            if p[idx[i]].rowid == min_rowid {
                idx[i] += 1;
            }
        }
    }
}

/// Decode each of `terms`' postings ACROSS EVERY height-0 segment of `data`, one
/// unioned ascending-by-rowid posting list per term — or `None` if the position-based
/// merge is not provably correct, in which case the caller falls back to the
/// `%_content` scan. The position-based (phrase / NEAR) analogue of
/// [`merge_segments`]: where that route only needs rowids, this preserves each
/// posting's per-column positions so the existing adjacency/run/NEAR checks
/// (`phrase_run_matches`/`near_matches`) can run unchanged on the merged set.
///
/// SAFETY — the same model as [`merge_segments`]. In a pure-insert history each
/// docid lives in exactly ONE segment, so a doc's per-column POSITIONS for every
/// term come wholly from that one segment — there is no cross-segment position
/// merge to do. We enforce that this holds and bail otherwise:
///
/// * any term in any segment returns [`SegDecode::Bail`] (interior page /
///   tombstone / malformed leaf) → `None`;
/// * any docid appears in MORE THAN ONE segment, considering ALL `terms` together
///   (an update/delete wrote a newer, shadowing entry whose layering precedence
///   this slice does not resolve) → `None`.
///
/// Otherwise each docid is confined to a single segment for every term, so simply
/// concatenating a term's per-segment postings and sorting by rowid yields exactly
/// the positions the `_content` scan sees. The combined-overlap check is stricter
/// than (and subsumes) `merge_segments`' per-term check: a docid posted by two
/// different terms must come from the same segment, which is guaranteed in a
/// pure-insert index, so a violation means the layered case — bail.
fn decode_terms_multiseg(
    data: &[(i64, Vec<u8>)],
    terms: &[&[u8]],
) -> Option<Vec<Vec<DecodedPosting>>> {
    let segments = segments_leaves(data)?;
    let mut per_term: Vec<Vec<DecodedPosting>> = alloc::vec![Vec::new(); terms.len()];
    // (rowid, segment index) for every posting of every term, to detect a docid
    // that straddles two segments (the ambiguous-precedence case → bail).
    let mut seen_segidx: Vec<(i64, usize)> = Vec::new();
    for (si, leaves) in segments.iter().enumerate() {
        for (ti, term) in terms.iter().enumerate() {
            match decode_term_strict(leaves, term) {
                SegDecode::Bail => return None,
                SegDecode::Postings(postings) => {
                    for p in postings {
                        seen_segidx.push((p.rowid, si));
                        per_term[ti].push(p);
                    }
                }
            }
        }
    }
    // Bail if any docid appears in more than one segment (ambiguous precedence).
    seen_segidx.sort_unstable();
    for w in seen_segidx.windows(2) {
        if w[0].0 == w[1].0 && w[0].1 != w[1].1 {
            return None;
        }
    }
    // Each term's postings, concatenated across segments, must be ascending by
    // rowid for the docid-aligned intersections; the no-overlap check guarantees no
    // duplicate rowid within a term, so sorting alone canonicalizes the order.
    for postings in &mut per_term {
        postings.sort_by_key(|p| p.rowid);
    }
    Some(per_term)
}

/// Look up the K-token phrase `"t0 t1 … t(K-1)"` (K ≥ 2) in an FTS5 index — across
/// ONE OR MORE height-0 segments — and return the rowids of the documents where the
/// tokens occur at CONSECUTIVE positions in one column (`terms[i]` at position
/// `p + i`), ascending — or `None` if the index shape is one the leaf reader cannot
/// serve (so the caller falls back to the `%_content` scan).
///
/// The phrase sibling of [`lookup_term_rowids`]: it decodes EVERY term's doclist
/// across every segment ([`decode_terms_multiseg`], which bails on a tombstone or a
/// docid shared across segments), intersects them by docid, and keeps the shared
/// docs whose per-column positions form a consecutive run — the exact set the scan's
/// K-token phrase predicate ([`crate::vtab`] `fts5_phrase_starts`) matches. Any term
/// being absent yields `Some(vec![])` (servable, no match). Repeated-word phrases
/// work because a repeated term decodes the same position list and the run check
/// reads it at successive offsets. For a single-segment index this is exactly the old
/// single-segment decode; for a multi-segment pure-insert index it returns the same
/// set the `_content` scan would.
pub(crate) fn lookup_phrase_rowids_k(data: &[(i64, Vec<u8>)], terms: &[&[u8]]) -> Option<Vec<i64>> {
    if terms.len() < 2 {
        return None;
    }
    let postings = decode_terms_multiseg(data, terms)?;
    Some(phrase_intersect_k(&postings, None))
}

/// Column-scoped sibling of [`lookup_phrase_rowids_k`]: keep only documents where
/// the consecutive K-token run occurs in COLUMN `column` (its position in the
/// table's full column list, from 0) — exactly what the scan's `col : "t0 t1 …"`
/// predicate matches. Multi-segment via [`decode_terms_multiseg`] (bailing to the
/// scan on a tombstone or a docid shared across segments).
pub(crate) fn lookup_phrase_rowids_in_column_k(
    data: &[(i64, Vec<u8>)],
    terms: &[&[u8]],
    column: usize,
) -> Option<Vec<i64>> {
    if terms.len() < 2 {
        return None;
    }
    let postings = decode_terms_multiseg(data, terms)?;
    Some(phrase_intersect_k(&postings, Some(column)))
}

/// Whether some position `pa` of term `a` and `pb` of term `b` are within the NEAR
/// window in THE SAME column: `|pa − pb| <= n + 1`. This is the two-single-token
/// specialization of the scan's [`crate::vtab`] NEAR rule
/// `max_end − min_start < n + total_len`: with two one-token phrases `total_len = 2`
/// and `max_end − min_start = |pa − pb|`, so `|pa − pb| < n + 2`, i.e.
/// `|pa − pb| <= n + 1` (verified against `sqlite3` 3.50.4: `NEAR(a b, 0)` matches a
/// gap of exactly 1, `NEAR(a b, 1)` a gap of 2, …). Both lists are ascending; we
/// sweep with two pointers, advancing whichever side is smaller — the closest pair
/// straddles the pointers, so a single pass decides the window.
fn near_within_in_column(pa: &[u32], pb: &[u32], n: u32) -> bool {
    let limit = n.saturating_add(1);
    let (mut i, mut j) = (0usize, 0usize);
    while i < pa.len() && j < pb.len() {
        let (a, b) = (pa[i], pb[j]);
        let gap = a.abs_diff(b);
        if gap <= limit {
            return true;
        }
        if a < b {
            i += 1;
        } else {
            j += 1;
        }
    }
    false
}

/// Whether term `a` and term `b` satisfy `NEAR(a b, n)` in SOME column of one
/// document: a column with positions `pa` (of `a`) and `pb` (of `b`) such that
/// `|pa − pb| <= n + 1` (see [`near_within_in_column`]). Positions are numbered per
/// column, exactly as the scan tokenizes one column at a time.
fn near_matches(a: &DecodedPosting, b: &DecodedPosting, n: u32) -> bool {
    let ncols = a.cols.len().max(b.cols.len());
    (0..ncols).any(|c| near_within_in_column(col(a, c), col(b, c), n))
}

/// Look up the two-single-token NEAR group `NEAR(term_a term_b, n)` in an FTS5
/// index — across ONE OR MORE height-0 segments — and return the rowids of the
/// documents where the two tokens occur within `n + 1` positions of each other in
/// the same column, ascending — or `None` if the index shape is one the leaf reader
/// cannot serve (so the caller falls back to the `%_content` scan).
///
/// This is the NEAR sibling of [`lookup_phrase_rowids_k`]. It decodes BOTH terms'
/// doclists across every segment ([`decode_terms_multiseg`], which bails on a
/// tombstone or a docid shared across segments), intersects them by docid, and keeps
/// the shared docs whose per-column positions fall inside the NEAR window — the exact
/// set the scan's two-single-token `NEAR` predicate (`fts5_near_matches`) matches.
/// Either term being absent yields `Some(vec![])` (servable, no match). `n` is the
/// query distance (default 10); the inequality is `|pa − pb| <= n + 1`. For a
/// single-segment index this is exactly the old single-segment decode.
pub(crate) fn lookup_near_rowids(
    data: &[(i64, Vec<u8>)],
    term_a: &[u8],
    term_b: &[u8],
    n: u32,
) -> Option<Vec<i64>> {
    let terms: [&[u8]; 2] = [term_a, term_b];
    let postings = decode_terms_multiseg(data, &terms)?;
    Some(phrase_intersect(&postings[0], &postings[1], |a, b| {
        near_matches(a, b, n)
    }))
}

/// Sorted-merge INTERSECTION of two ascending, deduplicated rowid lists.
fn rowids_intersect(a: &[i64], b: &[i64]) -> Vec<i64> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            core::cmp::Ordering::Less => i += 1,
            core::cmp::Ordering::Greater => j += 1,
            core::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Sorted-merge UNION of two ascending, deduplicated rowid lists.
fn rowids_union(a: &[i64], b: &[i64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            core::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            core::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            core::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Sorted-merge DIFFERENCE `a − b` of two ascending, deduplicated rowid lists.
fn rowids_difference(a: &[i64], b: &[i64]) -> Vec<i64> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() {
        if j >= b.len() {
            out.extend_from_slice(&a[i..]);
            break;
        }
        match a[i].cmp(&b[j]) {
            core::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            core::cmp::Ordering::Greater => j += 1,
            core::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Evaluate an [`Fts5BoolTree`] over the `%_data` index into the matching rowids,
/// ascending and deduplicated at every level, or `None` if ANY leaf term's
/// multi-segment merge bails (tombstone / docid shared across segments / interior
/// page) — in which case the whole boolean query falls back to the scan.
///
/// A `Leaf` resolves its term's rowids via [`decode_term_in_data`], the
/// multi-segment merge (so a term whose postings span several pure-insert segments
/// is unioned correctly, and the leaf bails on the layered/deleted case). Because
/// each leaf is resolved against the GLOBAL (across-segment) document set before the
/// boolean ops run, `a AND b` matches a doc even when `a` and `b` were last written
/// in different segments — the per-term sets are already merged. An `Op` combines
/// its two children's ascending rowid lists with the matching sorted-merge set-op
/// (`And`→[`rowids_intersect`], `Or`→[`rowids_union`], `Not`→[`rowids_difference`]),
/// each of which preserves ascending+unique, so the recursion's invariant holds all
/// the way up. Walks the tree bottom-up exactly as the scan's `fts5_eval` walks the
/// parse tree.
#[cfg(feature = "fts5")]
fn eval_bool_tree(data: &[(i64, Vec<u8>)], tree: &crate::vtab::Fts5BoolTree) -> Option<Vec<i64>> {
    use crate::vtab::{Fts5BoolOp, Fts5BoolTree};
    match tree {
        Fts5BoolTree::Leaf(term) => Some(
            decode_term_in_data(data, term)?
                .into_iter()
                .map(|p| p.rowid)
                .collect(),
        ),
        Fts5BoolTree::Op(op, a, b) => {
            let ra = eval_bool_tree(data, a)?;
            let rb = eval_bool_tree(data, b)?;
            Some(match op {
                Fts5BoolOp::And => rowids_intersect(&ra, &rb),
                Fts5BoolOp::Or => rowids_union(&ra, &rb),
                Fts5BoolOp::Not => rowids_difference(&ra, &rb),
            })
        }
    }
}

/// Look up an N-operand BOOLEAN TREE of bare terms ([`Fts5BoolTree`]) in an FTS5
/// index — across ONE OR MORE height-0 segments — returning the matching rowids
/// ascending, or `None` if any leaf term's multi-segment merge bails (tombstone /
/// docid shared across segments / interior page → the caller falls back to the
/// `%_content` scan).
///
/// The boolean sibling of [`lookup_term_rowids`]: it evaluates the tree bottom-up
/// with [`eval_bool_tree`], each leaf resolved through the multi-segment merge
/// ([`decode_term_in_data`]) so a term's postings spread over several pure-insert
/// segments are unioned before the boolean ops apply, and each node set-combines its
/// children (`And`→intersection, `Or`→union, `Not`→difference). Because the tree is
/// the exact parse tree the scan's `fts5_eval` walks (built by the recognizer
/// [`crate::vtab::fts5_bare_term_bool_tree`], preserving FTS5's
/// `NOT` > `AND` > `OR` precedence/associativity) and a table-wide bare term's
/// any-column match set is precisely its (across-segment) doclist's rowids, the
/// routed result is the identical SET — and identical ascending ORDER — the scan
/// produces for the same query.
///
/// [`Fts5BoolTree`]: crate::vtab::Fts5BoolTree
pub(crate) fn lookup_bool_tree_rowids(
    data: &[(i64, Vec<u8>)],
    tree: &crate::vtab::Fts5BoolTree,
) -> Option<Vec<i64>> {
    eval_bool_tree(data, tree)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::ToString, vec};

    #[test]
    fn segstructure_roundtrips_and_allocates_segid() {
        // A two-segment level-0 record (sqlite's 2-insert shape).
        let raw = &[
            0x00, 0x00, 0x00, 0x00, // cookie
            0x01, // nLevel
            0x02, // nSegment
            0x02, // wc
            0x00, // level0 nMerge
            0x02, // level0 nSeg
            0x01, 0x01, 0x01, // seg{1,1,1}
            0x02, 0x01, 0x01, // seg{2,1,1}
        ];
        let s = SegStructure::parse(raw).unwrap();
        assert_eq!(s.levels.len(), 1);
        assert_eq!(s.levels[0].segs.len(), 2);
        assert_eq!(s.write_counter, 2);
        // Byte-exact re-emit.
        assert_eq!(s.encode(), raw);
        // Lowest unused segid is 3.
        assert_eq!(s.allocate_segid(), 3);
    }

    #[test]
    fn segstructure_append_and_promote_down() {
        // Empty index → append one level-0 segment.
        let mut s = SegStructure {
            cookie: 0,
            write_counter: 0,
            levels: Vec::new(),
        };
        s.append_level0(1, 1);
        assert_eq!(s.levels.len(), 1);
        assert_eq!(
            s.levels[0].segs,
            vec![StructSeg {
                segid: 1,
                pgno_first: 1,
                pgno_last: 1
            }]
        );
        assert_eq!(s.write_counter, 1);

        // Simulate the post-crisis state: level0 empty, one size-1 seg at level1.
        // Appending a new size-1 seg to level0 must promote the level-1 seg DOWN
        // and place it BEFORE the freshly appended one (sqlite's front-insert).
        let mut s = SegStructure {
            cookie: 0,
            write_counter: 16,
            levels: vec![
                StructLevel {
                    n_merge: 0,
                    segs: Vec::new(),
                },
                StructLevel {
                    n_merge: 0,
                    segs: vec![StructSeg {
                        segid: 17,
                        pgno_first: 1,
                        pgno_last: 1,
                    }],
                },
            ],
        };
        s.append_level0(1, 1); // new seg id 1
        assert_eq!(
            s.levels[0].segs,
            vec![
                StructSeg {
                    segid: 17,
                    pgno_first: 1,
                    pgno_last: 1
                },
                StructSeg {
                    segid: 1,
                    pgno_first: 1,
                    pgno_last: 1
                },
            ],
            "promoted-down segment precedes the newly appended one"
        );
        assert!(s.levels[1].segs.is_empty());
    }

    fn p(rowid: i64, cols: &[&[u32]]) -> Posting {
        Posting {
            rowid,
            cols: cols.iter().map(|c| c.to_vec()).collect(),
            del: false,
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

    /// All doclist-index `%_data` pages `(dlidx-rowid, block)` in a segment, in
    /// rowid order.
    fn dlidx_pages(seg: &Segment) -> Vec<(i64, Vec<u8>)> {
        let mut out: Vec<(i64, Vec<u8>)> = seg
            .data
            .iter()
            .filter(|(id, _)| (*id & (1 << 36)) != 0)
            .map(|(id, b)| (*id, b.clone()))
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    /// `(terms, n_docs, col_totals, doc_sizes)` — the four `build_segment` inputs.
    type Corpus = (
        Vec<(Vec<u8>, Vec<Posting>)>,
        u64,
        Vec<u64>,
        Vec<(i64, Vec<u64>)>,
    );

    /// A single-column corpus of `n` documents (rowids `1..=n`), each the same
    /// single token `tok`, plus the token totals/doc-sizes `build_segment` wants.
    fn single_token_corpus(tok: &[u8], n: i64) -> Corpus {
        let postings: Vec<Posting> = (1..=n).map(|r| p(r, &[&[0]])).collect();
        let terms = vec![(tok.to_vec(), postings)];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n).map(|r| (r, vec![1])).collect();
        (terms, n as u64, vec![n as u64], doc_sizes)
    }

    /// A term whose doclist fits inside a single leaf gets NO doclist-index.
    #[test]
    fn dlidx_absent_when_doclist_fits_one_leaf() {
        let (terms, ndoc, tot, sizes) = single_token_corpus(b"shared", 50);
        let seg = build_segment(&terms, ndoc, &tot, &sizes, 4050, 0);
        assert!(dlidx_pages(&seg).is_empty());
        // The single `%_idx` entry has no dlidx flag.
        assert_eq!(seg.idx.len(), 1);
        assert_eq!(seg.idx[0].pgno & 1, 0);
    }

    /// A doclist that spills onto a few (< MIN_DLIDX_SIZE) continuation leaves is
    /// still too small for a doclist-index.
    #[test]
    fn dlidx_absent_below_threshold() {
        // ~3 leaves total: term-start + 2 continuation (< 4), so no dlidx.
        let (terms, ndoc, tot, sizes) = single_token_corpus(b"shared", 2500);
        let seg = build_segment(&terms, ndoc, &tot, &sizes, 4050, 0);
        let leaves = leaves_of(&seg);
        assert!(leaves.len() >= 2, "expected a multi-leaf spill");
        assert!(
            dlidx_pages(&seg).is_empty(),
            "fewer than {MIN_DLIDX_SIZE} continuation leaves must not add a dlidx"
        );
        assert_eq!(seg.idx.iter().filter(|r| r.pgno & 1 == 1).count(), 0);
        // Still fully decodable.
        assert_eq!(decode(&seg, b"shared").unwrap().len(), 2500);
    }

    /// A single term whose doclist spills onto MIN_DLIDX_SIZE+ continuation leaves
    /// emits a doclist-index page whose BYTES equal what sqlite 3.50.4 writes for
    /// the same corpus at pgsz=4050 (captured from the FTS5 oracle). The `%_idx`
    /// entry for the term-start leaf gets the dlidx (low) bit set, and the page is
    /// stored at the dlidx rowid `segid<<37 | 1<<36 | 0<<31 | leaf`.
    #[test]
    fn dlidx_bytes_match_sqlite_6000_docs() {
        let (terms, ndoc, tot, sizes) = single_token_corpus(b"shared", 6000);
        let seg = build_segment(&terms, ndoc, &tot, &sizes, 4050, 0);
        // sqlite 3.50.4: 5 leaves, one dlidx page X'00028A438A458A458A45' at
        // dlidx pgno 1, `%_idx` entry X''|leaf=1|dli=1.
        assert_eq!(leaves_of(&seg).len(), 5);
        let dl = dlidx_pages(&seg);
        assert_eq!(dl.len(), 1);
        assert_eq!(dl[0].0, dlidx_rowid(1, 0, 1));
        assert_eq!(dl[0].1, hex("00028A438A458A458A45"));
        assert_eq!(seg.idx.len(), 1);
        assert_eq!(seg.idx[0].pgno, (1 << 1) | 1); // leaf 1, dlidx flag set
        assert!(seg.idx[0].term.is_empty());
        // The doclist still round-trips through the reader (all 6000 rowids).
        let got = decode(&seg, b"shared").unwrap();
        assert_eq!(got.len(), 6000);
        assert_eq!(got.first().unwrap().rowid, 1);
        assert_eq!(got.last().unwrap().rowid, 6000);
    }

    /// Same shape, one leaf larger: sqlite's dlidx grows by exactly one rowid-delta
    /// entry (X'8A45'), still byte-identical.
    #[test]
    fn dlidx_bytes_match_sqlite_8000_docs() {
        let (terms, ndoc, tot, sizes) = single_token_corpus(b"shared", 8000);
        let seg = build_segment(&terms, ndoc, &tot, &sizes, 4050, 0);
        assert_eq!(leaves_of(&seg).len(), 6);
        let dl = dlidx_pages(&seg);
        assert_eq!(dl.len(), 1);
        assert_eq!(dl[0].0, dlidx_rowid(1, 0, 1));
        assert_eq!(dl[0].1, hex("00028A438A458A458A458A45"));
        assert_eq!(seg.idx[0].pgno & 1, 1);
        assert_eq!(decode(&seg, b"shared").unwrap().len(), 8000);
    }

    /// A MATCH on a high-frequency term whose doclist spills across many term-less
    /// continuation leaves AND carries a doclist-index must be served by the segment
    /// index route — NOT bailed to the `_content` scan. This pins the read side: the
    /// term-less continuation leaves parse cleanly (their `first_rowid_off` header
    /// pointer + rowid-less pages), so `lookup_term_rowids` returns every rowid and
    /// bumps [`INDEX_ROUTE_HITS`], and the extra dlidx `%_data` pages (rowids outside
    /// the leaf pgno range) do not confuse the leaf gather.
    #[test]
    fn high_frequency_spanning_term_takes_index_route() {
        let (terms, ndoc, tot, sizes) = single_token_corpus(b"shared", 8000);
        let seg = build_segment(&terms, ndoc, &tot, &sizes, 4050, 0);
        // The segment spills onto MIN_DLIDX_SIZE+ continuation leaves and has a dlidx.
        assert!(leaves_of(&seg).len() > MIN_DLIDX_SIZE);
        assert_eq!(dlidx_pages(&seg).len(), 1);
        assert_eq!(
            seg.idx[0].pgno & 1,
            1,
            "term-start leaf carries a dlidx flag"
        );

        let before = INDEX_ROUTE_HITS.load(core::sync::atomic::Ordering::Relaxed);
        let rowids = lookup_term_rowids(&seg.data, b"shared")
            .expect("a dlidx-bearing single-segment index must be servable, not scanned");
        assert_eq!(rowids.len(), 8000);
        assert_eq!(*rowids.first().unwrap(), 1);
        assert_eq!(*rowids.last().unwrap(), 8000);
        assert!(
            INDEX_ROUTE_HITS.load(core::sync::atomic::Ordering::Relaxed) > before,
            "the high-frequency spanning term must take the index route, not scan _content"
        );
    }

    /// Decode a run of hex digit pairs into bytes (test helper).
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Walk one dlidx b-tree page (height 0 leaf-index page), yielding
    /// `(leaf_pgno, first_rowid)` for every entry, following sqlite's
    /// `fts5DlidxLvlNext`: `[flag][firstPgno][firstRowid]` then, per following
    /// leaf, either a `0x00` (a rowid-less leaf → advance pgno, no entry) or a
    /// varint rowid-delta.
    fn dlidx_level_entries(page: &[u8]) -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        let mut pos = 1usize; // skip flag
        let mut pgno = read_varint(page, &mut pos).unwrap() as i64;
        let mut rowid = read_varint(page, &mut pos).unwrap() as i64;
        out.push((pgno, rowid));
        while pos < page.len() {
            // Skip rowid-less leaves (0x00 padding), counting each as a page.
            while pos < page.len() && page[pos] == 0 {
                pgno += 1;
                pos += 1;
            }
            if pos >= page.len() {
                break;
            }
            let d = read_varint(page, &mut pos).unwrap() as i64;
            pgno += 1;
            rowid += d;
            out.push((pgno, rowid));
        }
        out
    }

    /// Fetch a dlidx page `(segid=1, height, pgno)` from a segment, or `None`.
    fn dlidx_page(seg: &Segment, height: i64, pgno: i64) -> Option<&[u8]> {
        let rid = dlidx_rowid(1, height, pgno);
        seg.data
            .iter()
            .find(|(id, _)| *id == rid)
            .map(|(_, b)| b.as_slice())
    }

    /// The first rowid physically written on each segment LEAF (from its
    /// `first_rowid_off` header, read as an absolute varint), keyed by leaf pgno.
    /// Continuation leaves that carry no whole rowid (`first_rowid_off == 0`) are
    /// omitted.
    fn leaf_first_rowids(seg: &Segment) -> alloc::collections::BTreeMap<i64, i64> {
        let mut out = alloc::collections::BTreeMap::new();
        for (id, blob) in &seg.data {
            // Height-0 leaf pages: no dlidx bit, id > structure rowid.
            if *id <= STRUCTURE_ROWID || (*id & (1 << 36)) != 0 {
                continue;
            }
            let pgno = *id & 0x7fff_ffff;
            let fro = u16::from_be_bytes([blob[0], blob[1]]) as usize;
            if fro == 0 {
                continue;
            }
            let mut pos = fro;
            if let Some(r) = read_varint(blob, &mut pos) {
                out.insert(pgno, r as i64);
            }
        }
        out
    }

    /// A term whose doclist is enormous at a tiny page size builds a MULTI-LEVEL
    /// doclist-index (height-0 index pages plus a height-1 root). The whole dlidx
    /// b-tree must be internally consistent: walking it top-down recovers exactly
    /// the `(leaf_pgno → first_rowid)` map of the segment's leaves, and the doclist
    /// still round-trips through the reader.
    #[test]
    fn dlidx_multi_level_is_consistent() {
        // 4000 rowids spaced 1000 apart so deltas are multi-byte; pgsz 64 forces
        // many small leaves AND a dlidx large enough to need a second level.
        let postings: Vec<Posting> = (1..=4000).map(|i| p(i * 1000, &[&[0]])).collect();
        let terms = vec![(b"w".to_vec(), postings)];
        let sizes: Vec<(i64, Vec<u64>)> = (1..=4000).map(|i| (i * 1000, vec![1])).collect();
        let seg = build_segment(&terms, 4000, &[4000], &sizes, 64, 0);

        // There must be more than one height-0 index page and exactly one root at
        // height 1 (this is what makes it a MULTI-level dlidx).
        let n_h0 = seg
            .data
            .iter()
            .filter(|(id, _)| (*id & (1 << 36)) != 0 && ((*id >> 31) & 0x1f) == 0)
            .count();
        let n_h1 = seg
            .data
            .iter()
            .filter(|(id, _)| (*id & (1 << 36)) != 0 && ((*id >> 31) & 0x1f) == 1)
            .count();
        assert!(
            n_h0 > 1,
            "expected multiple height-0 dlidx pages, got {n_h0}"
        );
        assert_eq!(n_h1, 1, "expected exactly one height-1 root");

        // Walk the root: each entry points to a height-0 page (by pgno) whose first
        // rowid equals the root's recorded rowid for it.
        let root = dlidx_page(&seg, 1, 1).expect("root at height 1, pgno 1");
        let root_entries = dlidx_level_entries(root);
        let mut leaf_index: alloc::collections::BTreeMap<i64, i64> =
            alloc::collections::BTreeMap::new();
        for &(child_pgno, root_rowid) in &root_entries {
            let child = dlidx_page(&seg, 0, child_pgno)
                .unwrap_or_else(|| panic!("height-0 dlidx page {child_pgno} referenced by root"));
            let entries = dlidx_level_entries(child);
            assert_eq!(
                entries[0].1, root_rowid,
                "root's rowid for child {child_pgno} must equal that child's first rowid"
            );
            for (leaf_pgno, rowid) in entries {
                leaf_index.insert(leaf_pgno, rowid);
            }
        }

        // The dlidx's (leaf → first rowid) map must exactly match the leaves' own
        // first rowids (every indexed leaf, same rowid).
        let actual = leaf_first_rowids(&seg);
        for (&leaf_pgno, &rowid) in &leaf_index {
            assert_eq!(
                actual.get(&leaf_pgno),
                Some(&rowid),
                "dlidx entry for leaf {leaf_pgno} must match the leaf's first rowid"
            );
        }
        // The dlidx indexes exactly the continuation leaves that begin a rowid at
        // their `first_rowid_off` — i.e. every leaf EXCEPT the term-start leaf,
        // whose first rowid is inline right after the term record (so its header
        // `first_rowid_off` is 0 and it is not in `actual`). Both sets therefore
        // cover leaves 2..N and must be identical.
        assert_eq!(leaf_index.len(), actual.len());
        assert_eq!(leaf_index.keys().next(), Some(&2));

        // And the whole doclist still decodes to all 4000 rowids in order.
        let got = decode(&seg, b"w").unwrap();
        assert_eq!(got.len(), 4000);
        assert_eq!(got.first().unwrap().rowid, 1000);
        assert_eq!(got.last().unwrap().rowid, 4_000_000);
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
    fn lookup_bool_tree_n_operand_set_ops() {
        use crate::vtab::{Fts5BoolOp, Fts5BoolTree};
        use alloc::boxed::Box;
        // Three terms over docs 1..=8:
        //   a in {1,2,3,4,5}, b in {2,4,6,8}, c in {3,4,5,6}.
        let terms = vec![
            (
                b"a".to_vec(),
                (1..=5).map(|r| p(r, &[&[0]])).collect::<Vec<_>>(),
            ),
            (
                b"b".to_vec(),
                vec![2, 4, 6, 8]
                    .into_iter()
                    .map(|r| p(r, &[&[1]]))
                    .collect(),
            ),
            (
                b"c".to_vec(),
                vec![3, 4, 5, 6]
                    .into_iter()
                    .map(|r| p(r, &[&[2]]))
                    .collect(),
            ),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=8).map(|r| (r, vec![3])).collect();
        let seg = build_segment(&terms, 8, &[8], &doc_sizes, 1000, 0);
        let leaf = |t: &[u8]| Fts5BoolTree::Leaf(t.to_vec());
        let op = |o, l, r| Fts5BoolTree::Op(o, Box::new(l), Box::new(r));

        // a AND b AND c (left-assoc) → {4}.
        let t = op(
            Fts5BoolOp::And,
            op(Fts5BoolOp::And, leaf(b"a"), leaf(b"b")),
            leaf(b"c"),
        );
        assert_eq!(lookup_bool_tree_rowids(&seg.data, &t), Some(vec![4]));

        // a OR b OR c → {1,2,3,4,5,6,8}.
        let t = op(
            Fts5BoolOp::Or,
            op(Fts5BoolOp::Or, leaf(b"a"), leaf(b"b")),
            leaf(b"c"),
        );
        assert_eq!(
            lookup_bool_tree_rowids(&seg.data, &t),
            Some(vec![1, 2, 3, 4, 5, 6, 8])
        );

        // Precedence: `a OR b AND c` parses to `a OR (b AND c)`.
        // b AND c = {4,6}; a OR {4,6} = {1,2,3,4,5,6}.
        let t = op(
            Fts5BoolOp::Or,
            leaf(b"a"),
            op(Fts5BoolOp::And, leaf(b"b"), leaf(b"c")),
        );
        assert_eq!(
            lookup_bool_tree_rowids(&seg.data, &t),
            Some(vec![1, 2, 3, 4, 5, 6])
        );

        // A NOT in the tree: `(a OR b) NOT c`.
        // a OR b = {1,2,3,4,5,6,8}; minus c {3,4,5,6} = {1,2,8}.
        let t = op(
            Fts5BoolOp::Not,
            op(Fts5BoolOp::Or, leaf(b"a"), leaf(b"b")),
            leaf(b"c"),
        );
        assert_eq!(lookup_bool_tree_rowids(&seg.data, &t), Some(vec![1, 2, 8]));

        // An absent leaf is a servable empty operand: `a AND missing` = {}.
        let t = op(Fts5BoolOp::And, leaf(b"a"), leaf(b"missing"));
        assert_eq!(lookup_bool_tree_rowids(&seg.data, &t), Some(Vec::new()));

        // A lone leaf evaluates to that term's doclist.
        assert_eq!(
            lookup_bool_tree_rowids(&seg.data, &leaf(b"c")),
            Some(vec![3, 4, 5, 6])
        );
    }

    #[test]
    fn lookup_bool_tree_empty_index_falls_back() {
        use crate::vtab::Fts5BoolTree;
        // An unservable (empty) index returns None so the caller scans.
        let seg = build_segment(&[], 0, &[0], &[], 1000, 0);
        let t = Fts5BoolTree::Leaf(b"x".to_vec());
        assert_eq!(lookup_bool_tree_rowids(&seg.data, &t), None);
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

    // ---- prefix lookups (union the doclists of every term with the prefix) -

    #[test]
    fn lookup_prefix_rowids_unions_matching_terms() {
        // Terms sorted ascending: "apex"(d3), "apple"(d1), "apply"(d4),
        // "banana"(d2,d5). A prefix unions exactly the matching terms' docids and
        // dedups a doc that holds two prefix-matching terms.
        let terms = vec![
            (b"apex".to_vec(), vec![p(3, &[&[1]])]),
            (b"apple".to_vec(), vec![p(1, &[&[0]]), p(6, &[&[0]])]),
            (b"apply".to_vec(), vec![p(4, &[&[0]]), p(6, &[&[1]])]),
            (b"banana".to_vec(), vec![p(2, &[&[0]]), p(5, &[&[0]])]),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = vec![
            (1, vec![1]),
            (2, vec![1]),
            (3, vec![2]),
            (4, vec![1]),
            (5, vec![1]),
            (6, vec![2]),
        ];
        let seg = build_segment(&terms, 6, &[8], &doc_sizes, 1000, 0);
        // "ap" → apex, apple, apply → docs {1,3,4,6} (6 appears via both apple+apply,
        // deduped to one).
        assert_eq!(
            lookup_prefix_rowids(&seg.data, b"ap"),
            Some(vec![1, 3, 4, 6])
        );
        // "appl" → apple, apply → {1,4,6}.
        assert_eq!(
            lookup_prefix_rowids(&seg.data, b"appl"),
            Some(vec![1, 4, 6])
        );
        // "apple" → exactly that term → {1,6}.
        assert_eq!(lookup_prefix_rowids(&seg.data, b"apple"), Some(vec![1, 6]));
        // "ban" → banana → {2,5}.
        assert_eq!(lookup_prefix_rowids(&seg.data, b"ban"), Some(vec![2, 5]));
        // A prefix matching nothing → empty (servable), not None.
        assert_eq!(lookup_prefix_rowids(&seg.data, b"zzz"), Some(Vec::new()));
        // An empty index is not servable → None.
        let empty = build_segment(&[], 0, &[0], &[], 1000, 0);
        assert_eq!(lookup_prefix_rowids(&empty.data, b"ap"), None);
    }

    #[test]
    fn lookup_prefix_rowids_multi_leaf() {
        // Many distinct terms `word000`..`word039` (each one doc) at a tiny pgsz, so
        // the matching terms span several leaves; the prefix must enumerate across
        // leaf boundaries and union their docids.
        let n = 40usize;
        let terms: Vec<(Vec<u8>, Vec<Posting>)> = (0..n)
            .map(|i| {
                (
                    format!("word{i:03}").into_bytes(),
                    vec![p(i as i64 + 1, &[&[0]])],
                )
            })
            .collect();
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n as i64).map(|r| (r, vec![1])).collect();
        let seg = build_segment(&terms, n as u64, &[n as u64], &doc_sizes, 64, 0);
        assert!(leaf_count(&seg) > 1, "pgsz 64 must split into many leaves");
        // "word" → every doc.
        assert_eq!(
            lookup_prefix_rowids(&seg.data, b"word"),
            Some((1..=n as i64).collect::<Vec<_>>())
        );
        // "word01" → word010..word019 → docs 11..=20.
        assert_eq!(
            lookup_prefix_rowids(&seg.data, b"word01"),
            Some((11..=20).collect::<Vec<_>>())
        );
        assert_eq!(lookup_prefix_rowids(&seg.data, b"zzz"), Some(Vec::new()));
    }

    #[test]
    fn lookup_prefix_rowids_in_column_filters() {
        // "fox"(d1 col0, d2 col1), "fort"(d3 col0), "fox" also in d4 col1.
        let terms = vec![
            (b"fort".to_vec(), vec![p(3, &[&[0], &[]])]),
            (
                b"fox".to_vec(),
                vec![p(1, &[&[0], &[]]), p(2, &[&[], &[0]]), p(4, &[&[], &[1]])],
            ),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = vec![
            (1, vec![1, 0]),
            (2, vec![0, 1]),
            (3, vec![1, 0]),
            (4, vec![0, 2]),
        ];
        let seg = build_segment(&terms, 4, &[2, 3], &doc_sizes, 1000, 0);
        // "fo" any column → {1,2,3,4}.
        assert_eq!(
            lookup_prefix_rowids(&seg.data, b"fo"),
            Some(vec![1, 2, 3, 4])
        );
        // "fo" in col0 → fort(d3) + fox(d1) → {1,3}.
        assert_eq!(
            lookup_prefix_rowids_in_column(&seg.data, b"fo", 0),
            Some(vec![1, 3])
        );
        // "fo" in col1 → fox(d2,d4) → {2,4}.
        assert_eq!(
            lookup_prefix_rowids_in_column(&seg.data, b"fo", 1),
            Some(vec![2, 4])
        );
        // A column index past the table never matches.
        assert_eq!(
            lookup_prefix_rowids_in_column(&seg.data, b"fo", 9),
            Some(Vec::new())
        );
    }

    // ---- two-term phrase lookups (the adjacent-position intersection) -----

    #[test]
    fn lookup_phrase_rowids_adjacency() {
        // Terms "a" and "b" across docs (one column each):
        //   doc1: a@0, b@1            → adjacent ("a b")
        //   doc2: a@0, b@2            → NOT adjacent (gap)
        //   doc3: a@1, b@0            → "b a", not "a b"
        //   doc4: a@0,3  b@1,5        → adjacent at 0/1
        //   doc5: a@2 only            → b absent in doc
        //   doc6: b@1 only            → a absent in doc
        let terms = vec![
            (
                b"a".to_vec(),
                vec![
                    p(1, &[&[0]]),
                    p(2, &[&[0]]),
                    p(3, &[&[1]]),
                    p(4, &[&[0, 3]]),
                    p(5, &[&[2]]),
                ],
            ),
            (
                b"b".to_vec(),
                vec![
                    p(1, &[&[1]]),
                    p(2, &[&[2]]),
                    p(3, &[&[0]]),
                    p(4, &[&[1, 5]]),
                    p(6, &[&[1]]),
                ],
            ),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=6).map(|r| (r, vec![8])).collect::<Vec<_>>();
        let seg = build_segment(&terms, 6, &[40], &doc_sizes, 1000, 0);
        // Only docs 1 and 4 have "a" immediately followed by "b".
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"b"]),
            Some(vec![1, 4])
        );
        // The reverse phrase "b a": doc3 (b@0, a@1).
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"b", b"a"]),
            Some(vec![3])
        );
        // A term absent from the index → servable empty result.
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"zzz"]),
            Some(Vec::new())
        );
    }

    #[test]
    fn lookup_phrase_repeated_word() {
        // The phrase "a a": doc1 has a@0,1 (adjacent self), doc2 has a@0,2 (not).
        let terms = vec![(b"a".to_vec(), vec![p(1, &[&[0, 1]]), p(2, &[&[0, 2]])])];
        let seg = build_segment(&terms, 2, &[4], &[(1, vec![2]), (2, vec![3])], 1000, 0);
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"a"]),
            Some(vec![1])
        );
    }

    #[test]
    fn lookup_phrase_in_column_requires_same_column() {
        // Two columns. The phrase "a b" must be adjacent WITHIN one column:
        //   doc1: col0 a@0,b@1                 → col0 match
        //   doc2: col0 a@0 ; col1 b@1          → split across columns, NO match
        //   doc3: col1 a@2,b@3                 → col1 match
        //   doc4: col0 a@0,b@1 ; col1 a@5,b@6  → both columns match
        let terms = vec![
            (
                b"a".to_vec(),
                vec![
                    p(1, &[&[0], &[]]),
                    p(2, &[&[0], &[]]),
                    p(3, &[&[], &[2]]),
                    p(4, &[&[0], &[5]]),
                ],
            ),
            (
                b"b".to_vec(),
                vec![
                    p(1, &[&[1], &[]]),
                    p(2, &[&[], &[1]]),
                    p(3, &[&[], &[3]]),
                    p(4, &[&[1], &[6]]),
                ],
            ),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=4).map(|r| (r, vec![8, 8])).collect::<Vec<_>>();
        let seg = build_segment(&terms, 4, &[40, 40], &doc_sizes, 1000, 0);
        // Table-wide: any column with the adjacent phrase → docs 1, 3, 4.
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"b"]),
            Some(vec![1, 3, 4])
        );
        // Column 0 only: docs 1 and 4.
        assert_eq!(
            lookup_phrase_rowids_in_column_k(&seg.data, &[b"a", b"b"], 0),
            Some(vec![1, 4])
        );
        // Column 1 only: docs 3 and 4.
        assert_eq!(
            lookup_phrase_rowids_in_column_k(&seg.data, &[b"a", b"b"], 1),
            Some(vec![3, 4])
        );
    }

    #[test]
    fn lookup_phrase_multi_leaf() {
        // A small pgsz forces multi-leaf doclists for both terms; the phrase
        // intersection must still find the adjacent docs. Even rowids have "a b"
        // adjacent in col0; odd rowids have them non-adjacent.
        let n = 40i64;
        let a_post: Vec<Posting> = (1..=n).map(|r| p(r, &[&[0]])).collect();
        let b_post: Vec<Posting> = (1..=n)
            .map(|r| {
                if r % 2 == 0 {
                    p(r, &[&[1]])
                } else {
                    p(r, &[&[3]])
                }
            })
            .collect();
        let terms = vec![(b"a".to_vec(), a_post), (b"b".to_vec(), b_post)];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n).map(|r| (r, vec![8])).collect();
        let seg = build_segment(&terms, n as u64, &[8 * n as u64], &doc_sizes, 64, 0);
        assert!(leaf_count(&seg) > 1, "pgsz 64 must span the doclists");
        let even: Vec<i64> = (1..=n).filter(|r| r % 2 == 0).collect();
        assert_eq!(lookup_phrase_rowids_k(&seg.data, &[b"a", b"b"]), Some(even));
    }

    #[test]
    fn lookup_phrase_empty_index_falls_back() {
        let seg = build_segment(&[], 0, &[0], &[], 1000, 0);
        assert_eq!(lookup_phrase_rowids_k(&seg.data, &[b"a", b"b"]), None);
        assert_eq!(
            lookup_phrase_rowids_in_column_k(&seg.data, &[b"a", b"b"], 0),
            None
        );
    }

    #[test]
    fn lookup_phrase_k_consecutive_run() {
        // Terms a, b, c (one column). A 3-term phrase "a b c" matches iff some
        // column holds positions p, p+1, p+2 of a, b, c respectively.
        //   doc1: a@0 b@1 c@2         → consecutive "a b c"
        //   doc2: a@0 b@1 c@3         → c not at 2, NO match
        //   doc3: a@0 b@2 c@1         → out of order, NO match
        //   doc4: a@0,3 b@1,4 c@2,5   → two runs, matches
        //   doc5: a@0 b@1             → c absent in doc, NO match
        let terms = vec![
            (
                b"a".to_vec(),
                vec![
                    p(1, &[&[0]]),
                    p(2, &[&[0]]),
                    p(3, &[&[0]]),
                    p(4, &[&[0, 3]]),
                    p(5, &[&[0]]),
                ],
            ),
            (
                b"b".to_vec(),
                vec![
                    p(1, &[&[1]]),
                    p(2, &[&[1]]),
                    p(3, &[&[2]]),
                    p(4, &[&[1, 4]]),
                    p(5, &[&[1]]),
                ],
            ),
            (
                b"c".to_vec(),
                vec![
                    p(1, &[&[2]]),
                    p(2, &[&[3]]),
                    p(3, &[&[1]]),
                    p(4, &[&[2, 5]]),
                ],
            ),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=5).map(|r| (r, vec![8])).collect();
        let seg = build_segment(&terms, 5, &[40], &doc_sizes, 1000, 0);
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"b", b"c"]),
            Some(vec![1, 4])
        );
        // The 2-term prefix "a b" matches every doc with a@p, b@p+1: docs 1,2,4,5.
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"b"]),
            Some(vec![1, 2, 4, 5])
        );
        // A term absent from the index → servable empty result.
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"b", b"zzz"]),
            Some(Vec::new())
        );
    }

    #[test]
    fn lookup_phrase_k_repeated_word() {
        // The phrase "a a a": doc1 has a@0,1,2 (run of 3), doc2 has a@0,1 only
        // (run of 2, not 3), doc3 has a@0,2,4 (no consecutive run).
        let terms = vec![(
            b"a".to_vec(),
            vec![p(1, &[&[0, 1, 2]]), p(2, &[&[0, 1]]), p(3, &[&[0, 2, 4]])],
        )];
        let doc_sizes = [(1, vec![3]), (2, vec![2]), (3, vec![5])];
        let seg = build_segment(&terms, 3, &[10], &doc_sizes, 1000, 0);
        // "a a" (K=2): doc1 (0,1) and doc2 (0,1). doc3 has no adjacent pair.
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"a"]),
            Some(vec![1, 2])
        );
        // "a a a" (K=3): only doc1.
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"a", b"a"]),
            Some(vec![1])
        );
    }

    #[test]
    fn lookup_phrase_k_column_boundary() {
        // Two columns. The 3-term phrase "a b c" must be consecutive WITHIN one
        // column; a run split across the column boundary must NOT match.
        //   doc1: col0 a@0 b@1 c@2                  → col0 match
        //   doc2: col0 a@0 b@1 ; col1 c@0           → split across columns, NO match
        //   doc3: col1 a@3 b@4 c@5                  → col1 match
        let terms = vec![
            (
                b"a".to_vec(),
                vec![p(1, &[&[0], &[]]), p(2, &[&[0], &[]]), p(3, &[&[], &[3]])],
            ),
            (
                b"b".to_vec(),
                vec![p(1, &[&[1], &[]]), p(2, &[&[1], &[]]), p(3, &[&[], &[4]])],
            ),
            (
                b"c".to_vec(),
                vec![p(1, &[&[2], &[]]), p(2, &[&[], &[0]]), p(3, &[&[], &[5]])],
            ),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=3).map(|r| (r, vec![8, 8])).collect();
        let seg = build_segment(&terms, 3, &[40, 40], &doc_sizes, 1000, 0);
        // Table-wide: any column with the consecutive run → docs 1 and 3 (NOT 2).
        assert_eq!(
            lookup_phrase_rowids_k(&seg.data, &[b"a", b"b", b"c"]),
            Some(vec![1, 3])
        );
        // Column 0 only: doc 1.
        assert_eq!(
            lookup_phrase_rowids_in_column_k(&seg.data, &[b"a", b"b", b"c"], 0),
            Some(vec![1])
        );
        // Column 1 only: doc 3.
        assert_eq!(
            lookup_phrase_rowids_in_column_k(&seg.data, &[b"a", b"b", b"c"], 1),
            Some(vec![3])
        );
    }

    // ---- two-single-token NEAR lookups (|pa − pb| <= n + 1) ---------------

    #[test]
    fn lookup_near_rowids_distance_boundary() {
        // Single column. a@0 in every doc; b at increasing gaps:
        //   doc1: b@1 (gap 1)   doc2: b@2 (gap 2)   doc3: b@3 (gap 3)
        //   doc4: b@4 (gap 4)   doc5: a only (b absent)   doc6: b@1 (gap 1)
        let terms = vec![
            (
                b"a".to_vec(),
                vec![
                    p(1, &[&[0]]),
                    p(2, &[&[0]]),
                    p(3, &[&[0]]),
                    p(4, &[&[0]]),
                    p(5, &[&[0]]),
                    p(6, &[&[0]]),
                ],
            ),
            (
                b"b".to_vec(),
                vec![
                    p(1, &[&[1]]),
                    p(2, &[&[2]]),
                    p(3, &[&[3]]),
                    p(4, &[&[4]]),
                    p(6, &[&[1]]),
                ],
            ),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=6).map(|r| (r, vec![8])).collect();
        let seg = build_segment(&terms, 6, &[40], &doc_sizes, 1000, 0);
        // n=0 → |gap| <= 1: gap-1 docs only (1, 6).
        assert_eq!(
            lookup_near_rowids(&seg.data, b"a", b"b", 0),
            Some(vec![1, 6])
        );
        // n=1 → |gap| <= 2: docs 1, 2, 6.
        assert_eq!(
            lookup_near_rowids(&seg.data, b"a", b"b", 1),
            Some(vec![1, 2, 6])
        );
        // n=2 → |gap| <= 3: docs 1, 2, 3, 6. Pin the boundary: gap-3 doc3 IN, gap-4
        // doc4 OUT.
        assert_eq!(
            lookup_near_rowids(&seg.data, b"a", b"b", 2),
            Some(vec![1, 2, 3, 6])
        );
        // n=3 → |gap| <= 4: now doc4 (gap 4) joins.
        assert_eq!(
            lookup_near_rowids(&seg.data, b"a", b"b", 3),
            Some(vec![1, 2, 3, 4, 6])
        );
        // The window is symmetric: "b a" (reversed) is the same set.
        assert_eq!(
            lookup_near_rowids(&seg.data, b"b", b"a", 0),
            Some(vec![1, 6])
        );
        // A term absent from the index → servable empty result.
        assert_eq!(
            lookup_near_rowids(&seg.data, b"a", b"zzz", 10),
            Some(Vec::new())
        );
    }

    #[test]
    fn lookup_near_rowids_requires_same_column() {
        // Two columns. The NEAR pair must fall WITHIN one column:
        //   doc1: col0 a@0, b@2          → col0 gap 2
        //   doc2: col0 a@0 ; col1 b@0    → split across columns, NO match
        //   doc3: col1 a@5, b@7          → col1 gap 2
        //   doc4: col0 a@0 ; col1 b@9    → far + split, NO match at any n we test
        let terms = vec![
            (
                b"a".to_vec(),
                vec![
                    p(1, &[&[0], &[]]),
                    p(2, &[&[0], &[]]),
                    p(3, &[&[], &[5]]),
                    p(4, &[&[0], &[]]),
                ],
            ),
            (
                b"b".to_vec(),
                vec![
                    p(1, &[&[2], &[]]),
                    p(2, &[&[], &[0]]),
                    p(3, &[&[], &[7]]),
                    p(4, &[&[], &[9]]),
                ],
            ),
        ];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=4).map(|r| (r, vec![8, 12])).collect();
        let seg = build_segment(&terms, 4, &[40, 60], &doc_sizes, 1000, 0);
        // n=1 → |gap| <= 2 within one column: docs 1 and 3 (doc2/doc4 are split).
        assert_eq!(
            lookup_near_rowids(&seg.data, b"a", b"b", 1),
            Some(vec![1, 3])
        );
    }

    #[test]
    fn lookup_near_rowids_multi_leaf() {
        // A small pgsz forces multi-leaf doclists. Even rowids have the pair within
        // gap 1 in col0; odd rowids have them gap 5 apart.
        let n = 40i64;
        let a_post: Vec<Posting> = (1..=n).map(|r| p(r, &[&[0]])).collect();
        let b_post: Vec<Posting> = (1..=n)
            .map(|r| {
                if r % 2 == 0 {
                    p(r, &[&[1]])
                } else {
                    p(r, &[&[5]])
                }
            })
            .collect();
        let terms = vec![(b"a".to_vec(), a_post), (b"b".to_vec(), b_post)];
        let doc_sizes: Vec<(i64, Vec<u64>)> = (1..=n).map(|r| (r, vec![8])).collect();
        let seg = build_segment(&terms, n as u64, &[8 * n as u64], &doc_sizes, 64, 0);
        assert!(leaf_count(&seg) > 1, "pgsz 64 must span the doclists");
        // n=0 → |gap| <= 1: even rowids (gap 1) only.
        let even: Vec<i64> = (1..=n).filter(|r| r % 2 == 0).collect();
        assert_eq!(lookup_near_rowids(&seg.data, b"a", b"b", 0), Some(even));
        // n=4 → |gap| <= 5: now odd rowids (gap 5) join → all docs.
        assert_eq!(
            lookup_near_rowids(&seg.data, b"a", b"b", 4),
            Some((1..=n).collect::<Vec<_>>())
        );
    }

    #[test]
    fn lookup_near_empty_index_falls_back() {
        let seg = build_segment(&[], 0, &[0], &[], 1000, 0);
        assert_eq!(lookup_near_rowids(&seg.data, b"a", b"b", 10), None);
    }

    // ---- multi-segment merge (D2b multi-segment bare-term/boolean/prefix) -----

    /// One segment's spec for [`multiseg_data`]: its `segid` and its terms (each a
    /// `(term bytes, postings)` pair, the same shape [`build_segment`] takes).
    type SegSpec = (i64, Vec<(Vec<u8>, Vec<Posting>)>);

    /// Build a `%_data` row set holding SEVERAL height-0 segments. Each spec is
    /// `(segid, terms)`; the term doclist for one segment is built via
    /// [`build_segment`] (single-segment), then its leaves are re-keyed under the
    /// spec's `segid`. The combined structure record (id 10) lists all segments at
    /// one level so [`all_segments`] sees the full multi-segment shape. Only the
    /// structure record and the leaf rows are produced — the lookups read nothing
    /// else.
    fn multiseg_data(specs: &[SegSpec]) -> Vec<(i64, Vec<u8>)> {
        let mut data: Vec<(i64, Vec<u8>)> = Vec::new();
        let mut struct_body: Vec<u8> = 0u32.to_be_bytes().to_vec(); // cookie 0
        put_varint(&mut struct_body, 1); // nLevel
        put_varint(&mut struct_body, specs.len() as u64); // nSegment
        put_varint(&mut struct_body, 0); // nWriteCounter
        put_varint(&mut struct_body, 0); // level 0: nMerge
        put_varint(&mut struct_body, specs.len() as u64); // level 0: nSeg
        for (segid, terms) in specs {
            // Build this segment's leaves (build_segment uses segid 1 internally).
            let n_docs = terms.iter().flat_map(|(_, ps)| ps.iter()).count() as u64;
            let seg = build_segment(terms, n_docs.max(1), &[64], &[], 4096, 0);
            // Extract its leaves in page order.
            let mut pgno = 1i64;
            let mut n_leaves = 0i64;
            loop {
                let rid = segment_leaf_rowid(1, pgno);
                match seg.data.iter().find(|(id, _)| *id == rid) {
                    Some((_, blob)) => {
                        data.push((segment_leaf_rowid(*segid, pgno), blob.clone()));
                        n_leaves += 1;
                        pgno += 1;
                    }
                    None => break,
                }
            }
            // Per-segment triple: segid, pgnoFirst=1, pgnoLast=n_leaves.
            put_varint(&mut struct_body, *segid as u64);
            put_varint(&mut struct_body, 1);
            put_varint(&mut struct_body, n_leaves as u64);
        }
        data.push((STRUCTURE_ROWID, struct_body));
        data
    }

    #[test]
    fn multiseg_bare_term_unions_across_segments_and_routes() {
        // "cat" lives in three pure-insert segments, each holding distinct docids;
        // the merge UNIONs them ascending. No overlap, no tombstone → index-routed.
        let specs = vec![
            (
                1i64,
                vec![(b"cat".to_vec(), vec![p(1, &[&[0]]), p(4, &[&[0]])])],
            ),
            (
                2i64,
                vec![(b"cat".to_vec(), vec![p(2, &[&[0]]), p(7, &[&[0]])])],
            ),
            (
                3i64,
                vec![
                    (b"cat".to_vec(), vec![p(5, &[&[0]])]),
                    (b"dog".to_vec(), vec![p(9, &[&[0]])]),
                ],
            ),
        ];
        let data = multiseg_data(&specs);
        let before = INDEX_ROUTE_HITS.load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(lookup_term_rowids(&data, b"cat"), Some(vec![1, 2, 4, 5, 7]));
        assert!(
            INDEX_ROUTE_HITS.load(core::sync::atomic::Ordering::Relaxed) > before,
            "the multi-segment bare term must take the index route"
        );
        // "dog" only in segment 3.
        assert_eq!(lookup_term_rowids(&data, b"dog"), Some(vec![9]));
        // Absent everywhere → servable empty.
        assert_eq!(lookup_term_rowids(&data, b"zzz"), Some(Vec::new()));
    }

    #[test]
    fn multiseg_overlapping_docid_bails_to_scan() {
        // The SAME docid (4) appears in two segments for "cat" — an update wrote a
        // newer, shadowing entry. The merge can't resolve precedence here, so it
        // bails (None) and the caller falls back to the scan.
        let specs = vec![
            (1i64, vec![(b"cat".to_vec(), vec![p(4, &[&[0]])])]),
            (2i64, vec![(b"cat".to_vec(), vec![p(4, &[&[1]])])]),
        ];
        let data = multiseg_data(&specs);
        assert_eq!(lookup_term_rowids(&data, b"cat"), None);
    }

    #[test]
    fn multiseg_boolean_tree_merges_across_segments() {
        // a in seg1 {1,2,3}, b in seg2 {2,3,4}, c in seg3 {3,5}. A bool tree must
        // resolve each leaf against the GLOBAL set (so `a AND b` = {2,3} even though
        // a and b live in different segments).
        use crate::vtab::{Fts5BoolOp, Fts5BoolTree};
        use alloc::boxed::Box;
        let specs = vec![
            (
                1i64,
                vec![(
                    b"a".to_vec(),
                    vec![p(1, &[&[0]]), p(2, &[&[0]]), p(3, &[&[0]])],
                )],
            ),
            (
                2i64,
                vec![(
                    b"b".to_vec(),
                    vec![p(2, &[&[0]]), p(3, &[&[0]]), p(4, &[&[0]])],
                )],
            ),
            (
                3i64,
                vec![(b"c".to_vec(), vec![p(3, &[&[0]]), p(5, &[&[0]])])],
            ),
        ];
        let data = multiseg_data(&specs);
        let leaf = |t: &[u8]| Fts5BoolTree::Leaf(t.to_vec());
        let op = |o, l, r| Fts5BoolTree::Op(o, Box::new(l), Box::new(r));
        // a AND b = {2,3}.
        let t = op(Fts5BoolOp::And, leaf(b"a"), leaf(b"b"));
        assert_eq!(lookup_bool_tree_rowids(&data, &t), Some(vec![2, 3]));
        // a OR c = {1,2,3,5}.
        let t = op(Fts5BoolOp::Or, leaf(b"a"), leaf(b"c"));
        assert_eq!(lookup_bool_tree_rowids(&data, &t), Some(vec![1, 2, 3, 5]));
        // (a OR b) NOT c = {1,2,4}.
        let t = op(
            Fts5BoolOp::Not,
            op(Fts5BoolOp::Or, leaf(b"a"), leaf(b"b")),
            leaf(b"c"),
        );
        assert_eq!(lookup_bool_tree_rowids(&data, &t), Some(vec![1, 2, 4]));
    }

    #[test]
    fn multiseg_boolean_tree_bails_when_a_leaf_overlaps() {
        // If any leaf term overlaps across segments, that leaf's merge bails, so the
        // whole boolean query bails (→ scan).
        use crate::vtab::{Fts5BoolOp, Fts5BoolTree};
        use alloc::boxed::Box;
        let specs = vec![
            (1i64, vec![(b"a".to_vec(), vec![p(1, &[&[0]])])]),
            // "b" overlaps docid 1 across seg2 and seg3.
            (2i64, vec![(b"b".to_vec(), vec![p(1, &[&[0]])])]),
            (3i64, vec![(b"b".to_vec(), vec![p(1, &[&[1]])])]),
        ];
        let data = multiseg_data(&specs);
        let t = Fts5BoolTree::Op(
            Fts5BoolOp::And,
            Box::new(Fts5BoolTree::Leaf(b"a".to_vec())),
            Box::new(Fts5BoolTree::Leaf(b"b".to_vec())),
        );
        assert_eq!(lookup_bool_tree_rowids(&data, &t), None);
    }

    #[test]
    fn multiseg_prefix_unions_across_segments() {
        // "apple"(seg1 d1), "apply"(seg2 d2), "apex"(seg3 d3) — three prefix-"ap"
        // terms in three segments. The prefix route unions their docids.
        let specs = vec![
            (1i64, vec![(b"apple".to_vec(), vec![p(1, &[&[0]])])]),
            (2i64, vec![(b"apply".to_vec(), vec![p(2, &[&[0]])])]),
            (
                3i64,
                vec![
                    (b"apex".to_vec(), vec![p(3, &[&[0]])]),
                    (b"banana".to_vec(), vec![p(4, &[&[0]])]),
                ],
            ),
        ];
        let data = multiseg_data(&specs);
        assert_eq!(lookup_prefix_rowids(&data, b"ap"), Some(vec![1, 2, 3]));
        assert_eq!(lookup_prefix_rowids(&data, b"appl"), Some(vec![1, 2]));
        assert_eq!(lookup_prefix_rowids(&data, b"ban"), Some(vec![4]));
        assert_eq!(lookup_prefix_rowids(&data, b"zzz"), Some(Vec::new()));
    }

    #[test]
    fn multiseg_prefix_same_doc_two_terms_one_segment_is_not_overlap() {
        // A single document holding two prefix-matching terms in the SAME segment is
        // legitimate (not a cross-segment overlap) — it dedups to one rowid and does
        // NOT bail.
        let specs = vec![
            (
                1i64,
                vec![
                    (b"apple".to_vec(), vec![p(1, &[&[0]])]),
                    (b"apply".to_vec(), vec![p(1, &[&[1]])]),
                ],
            ),
            (2i64, vec![(b"apex".to_vec(), vec![p(2, &[&[0]])])]),
        ];
        let data = multiseg_data(&specs);
        assert_eq!(lookup_prefix_rowids(&data, b"ap"), Some(vec![1, 2]));
    }

    #[test]
    fn delete_tombstone_in_poslist_bails() {
        // A doclist entry whose poslist size varint has the low bit set is a DELETE
        // tombstone; decode_poslist rejects it, so the segment decode bails (→ scan).
        // Hand-craft a one-leaf segment whose "cat" doclist is `[rowid 1][size2=1]`.
        let mut body: Vec<u8> = Vec::new();
        // term record: [keylen=4]["0cat"]
        let key = term_key(b"cat");
        put_varint(&mut body, key.len() as u64);
        body.extend_from_slice(&key);
        // doclist: rowid delta 1, then size2 = 1 (delete marker, low bit set).
        let term_off = 4; // body starts at offset 4 in the leaf
        put_varint(&mut body, 1); // rowid 1
        put_varint(&mut body, 1); // size2 = 1 → DELETE
        let footer_off = 4 + body.len();
        let mut leaf: Vec<u8> = Vec::new();
        leaf.extend_from_slice(&0u16.to_be_bytes()); // first_rowid_off = 0
        leaf.extend_from_slice(&(footer_off as u16).to_be_bytes());
        leaf.extend_from_slice(&body);
        // pgidx footer: one term offset (absolute).
        put_varint(&mut leaf, term_off as u64);

        // Wrap it as a single-segment %_data; the tombstone forces a bail.
        let mut struct_body: Vec<u8> = 0u32.to_be_bytes().to_vec();
        for v in [1u64, 1, 0, 0, 1, 1, 1, 1] {
            put_varint(&mut struct_body, v);
        }
        let data = vec![
            (segment_leaf_rowid(1, 1), leaf),
            (STRUCTURE_ROWID, struct_body),
        ];
        assert_eq!(lookup_term_rowids(&data, b"cat"), None);
    }

    // ---- multi-segment phrase / NEAR (D2b multi-segment position-based) -------

    #[test]
    fn multiseg_two_term_phrase_unions_across_segments_and_routes() {
        // The phrase "a b" spread over three pure-insert segments (each docid in
        // exactly one segment). Adjacency is decided per-doc from that doc's one
        // segment; the route must fire and union the matches ascending.
        //   seg1: d1 a@0,b@1 (match)   d2 a@0,b@2 (gap, no)
        //   seg2: d4 a@0,b@1 (match)   d5 b@0,a@1 ("b a", no)
        //   seg3: d7 a@0,3 b@1 (match) d9 a@5 only (b absent in doc, no)
        let specs = vec![
            (
                1i64,
                vec![
                    (b"a".to_vec(), vec![p(1, &[&[0]]), p(2, &[&[0]])]),
                    (b"b".to_vec(), vec![p(1, &[&[1]]), p(2, &[&[2]])]),
                ],
            ),
            (
                2i64,
                vec![
                    (b"a".to_vec(), vec![p(4, &[&[0]]), p(5, &[&[1]])]),
                    (b"b".to_vec(), vec![p(4, &[&[1]]), p(5, &[&[0]])]),
                ],
            ),
            (
                3i64,
                vec![
                    (b"a".to_vec(), vec![p(7, &[&[0, 3]]), p(9, &[&[5]])]),
                    (b"b".to_vec(), vec![p(7, &[&[1]])]),
                ],
            ),
        ];
        let data = multiseg_data(&specs);
        let before = INDEX_ROUTE_HITS.load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            lookup_phrase_rowids_k(&data, &[b"a", b"b"]),
            Some(vec![1, 4, 7])
        );
        assert!(
            INDEX_ROUTE_HITS.load(core::sync::atomic::Ordering::Relaxed) > before,
            "the multi-segment phrase must take the index route"
        );
        // Reverse phrase "b a": only d5 (b@0, a@1).
        assert_eq!(lookup_phrase_rowids_k(&data, &[b"b", b"a"]), Some(vec![5]));
        // A term absent everywhere → servable empty.
        assert_eq!(
            lookup_phrase_rowids_k(&data, &[b"a", b"zzz"]),
            Some(Vec::new())
        );
    }

    #[test]
    fn multiseg_three_term_phrase_across_segments() {
        // "a b c" consecutive run, docids split across two segments.
        //   seg1: d1 a@0,b@1,c@2 (match)   d2 a@0,b@1,c@3 (c not consecutive, no)
        //   seg2: d3 a@5,b@6,c@7 (match)   d4 a@0,b@2,c@3 (b gap, no)
        let specs = vec![
            (
                1i64,
                vec![
                    (b"a".to_vec(), vec![p(1, &[&[0]]), p(2, &[&[0]])]),
                    (b"b".to_vec(), vec![p(1, &[&[1]]), p(2, &[&[1]])]),
                    (b"c".to_vec(), vec![p(1, &[&[2]]), p(2, &[&[3]])]),
                ],
            ),
            (
                2i64,
                vec![
                    (b"a".to_vec(), vec![p(3, &[&[5]]), p(4, &[&[0]])]),
                    (b"b".to_vec(), vec![p(3, &[&[6]]), p(4, &[&[2]])]),
                    (b"c".to_vec(), vec![p(3, &[&[7]]), p(4, &[&[3]])]),
                ],
            ),
        ];
        let data = multiseg_data(&specs);
        assert_eq!(
            lookup_phrase_rowids_k(&data, &[b"a", b"b", b"c"]),
            Some(vec![1, 3])
        );
    }

    #[test]
    fn multiseg_phrase_in_column_across_segments() {
        // Two columns; the run "a b" must be adjacent WITHIN one column.
        //   seg1: d1 col0 a@0,b@1 (col0 match)   d2 col0 a@0 ; col1 b@1 (split, no)
        //   seg2: d3 col1 a@2,b@3 (col1 match)   d4 col0 a@0,b@1; col1 a@5,b@6 (both)
        let specs = vec![
            (
                1i64,
                vec![
                    (b"a".to_vec(), vec![p(1, &[&[0], &[]]), p(2, &[&[0], &[]])]),
                    (b"b".to_vec(), vec![p(1, &[&[1], &[]]), p(2, &[&[], &[1]])]),
                ],
            ),
            (
                2i64,
                vec![
                    (b"a".to_vec(), vec![p(3, &[&[], &[2]]), p(4, &[&[0], &[5]])]),
                    (b"b".to_vec(), vec![p(3, &[&[], &[3]]), p(4, &[&[1], &[6]])]),
                ],
            ),
        ];
        let data = multiseg_data(&specs);
        // Column 0 ("a b" in col0): d1 and d4.
        assert_eq!(
            lookup_phrase_rowids_in_column_k(&data, &[b"a", b"b"], 0),
            Some(vec![1, 4])
        );
        // Column 1 ("a b" in col1): d3 and d4.
        assert_eq!(
            lookup_phrase_rowids_in_column_k(&data, &[b"a", b"b"], 1),
            Some(vec![3, 4])
        );
    }

    #[test]
    fn multiseg_near_unions_across_segments_and_routes() {
        // NEAR(a b, n): |pa - pb| <= n + 1 in the same column, per-doc from its one
        // segment.
        //   seg1: d1 a@0,b@1 (gap1)   d2 a@0,b@5 (gap5)
        //   seg2: d3 a@4,b@2 (gap2)   d4 a@0,b@9 (gap9)
        let specs = vec![
            (
                1i64,
                vec![
                    (b"a".to_vec(), vec![p(1, &[&[0]]), p(2, &[&[0]])]),
                    (b"b".to_vec(), vec![p(1, &[&[1]]), p(2, &[&[5]])]),
                ],
            ),
            (
                2i64,
                vec![
                    (b"a".to_vec(), vec![p(3, &[&[4]]), p(4, &[&[0]])]),
                    (b"b".to_vec(), vec![p(3, &[&[2]]), p(4, &[&[9]])]),
                ],
            ),
        ];
        let data = multiseg_data(&specs);
        let before = INDEX_ROUTE_HITS.load(core::sync::atomic::Ordering::Relaxed);
        // n=1 → window |pa-pb| <= 2: d1 (gap1), d3 (gap2). d2 (gap5), d4 (gap9) out.
        assert_eq!(lookup_near_rowids(&data, b"a", b"b", 1), Some(vec![1, 3]));
        assert!(
            INDEX_ROUTE_HITS.load(core::sync::atomic::Ordering::Relaxed) > before,
            "the multi-segment NEAR must take the index route"
        );
        // n=4 → window <= 5: adds d2 (gap5). d4 (gap9) still out.
        assert_eq!(
            lookup_near_rowids(&data, b"a", b"b", 4),
            Some(vec![1, 2, 3])
        );
        // A term absent everywhere → servable empty.
        assert_eq!(
            lookup_near_rowids(&data, b"a", b"zzz", 10),
            Some(Vec::new())
        );
    }

    #[test]
    fn multiseg_phrase_overlapping_docid_bails_to_scan() {
        // Docid 4 appears in two segments (an update shadowed it). The phrase merge
        // can't resolve precedence, so it bails (None) and the caller scans.
        let specs = vec![
            (
                1i64,
                vec![
                    (b"a".to_vec(), vec![p(4, &[&[0]])]),
                    (b"b".to_vec(), vec![p(4, &[&[1]])]),
                ],
            ),
            (
                2i64,
                vec![
                    (b"a".to_vec(), vec![p(4, &[&[0]])]),
                    (b"b".to_vec(), vec![p(4, &[&[1]])]),
                ],
            ),
        ];
        let data = multiseg_data(&specs);
        assert_eq!(lookup_phrase_rowids_k(&data, &[b"a", b"b"]), None);
        assert_eq!(lookup_near_rowids(&data, b"a", b"b", 5), None);
    }

    #[test]
    fn multiseg_phrase_cross_term_overlap_bails_to_scan() {
        // Even when no single term overlaps itself, a docid shared by DIFFERENT terms
        // across two segments is impossible in a pure-insert history (a doc lives in
        // one segment), so it signals the layered case → bail. Here d4 carries "a" in
        // seg1 and "b" in seg2.
        let specs = vec![
            (1i64, vec![(b"a".to_vec(), vec![p(4, &[&[0]])])]),
            (2i64, vec![(b"b".to_vec(), vec![p(4, &[&[1]])])]),
        ];
        let data = multiseg_data(&specs);
        assert_eq!(lookup_phrase_rowids_k(&data, &[b"a", b"b"]), None);
        assert_eq!(lookup_near_rowids(&data, b"a", b"b", 5), None);
    }

    #[test]
    fn multiseg_phrase_tombstone_bails_to_scan() {
        // A DELETE tombstone in one segment's "a" doclist forces the phrase merge to
        // bail (the strict per-segment decode returns Bail on the rejected poslist).
        // Build seg1 normally, then a seg2 whose "a" doclist is a delete marker.
        let mut tomb_leaf: Vec<u8> = Vec::new();
        let mut body: Vec<u8> = Vec::new();
        let key = term_key(b"a");
        put_varint(&mut body, key.len() as u64);
        body.extend_from_slice(&key);
        let term_off = 4;
        put_varint(&mut body, 1); // rowid 1
        put_varint(&mut body, 1); // size2 = 1 → DELETE tombstone
        let footer_off = 4 + body.len();
        tomb_leaf.extend_from_slice(&0u16.to_be_bytes());
        tomb_leaf.extend_from_slice(&(footer_off as u16).to_be_bytes());
        tomb_leaf.extend_from_slice(&body);
        put_varint(&mut tomb_leaf, term_off as u64);

        // seg1: legitimate "a"/"b" leaves via build_segment.
        let seg1 = build_segment(
            &[
                (b"a".to_vec(), vec![p(3, &[&[0]])]),
                (b"b".to_vec(), vec![p(3, &[&[1]])]),
            ],
            1,
            &[64],
            &[],
            4096,
            0,
        );
        let leaf1 = seg1
            .data
            .iter()
            .find(|(id, _)| *id == segment_leaf_rowid(1, 1))
            .unwrap()
            .1
            .clone();

        // Structure: two segments at one level, each one leaf.
        let mut struct_body: Vec<u8> = 0u32.to_be_bytes().to_vec();
        for v in [1u64, 2, 0, 0, 2, 1, 1, 1, 2, 1, 1] {
            put_varint(&mut struct_body, v);
        }
        let data = vec![
            (segment_leaf_rowid(1, 1), leaf1),
            (segment_leaf_rowid(2, 1), tomb_leaf),
            (STRUCTURE_ROWID, struct_body),
        ];
        assert_eq!(lookup_phrase_rowids_k(&data, &[b"a", b"b"]), None);
        assert_eq!(lookup_near_rowids(&data, b"a", b"b", 5), None);
    }

    // ---- tombstone-preserving reader / merge (automerge) ----------------------

    /// A delete-marker posting (positionless tombstone).
    fn tomb(rowid: i64) -> Posting {
        Posting {
            rowid,
            cols: vec![Vec::new()],
            del: true,
        }
    }

    /// The leaf blobs of a segment built by `build_segment_block`.
    fn seg_leaves_of(terms: &[(Vec<u8>, Vec<Posting>)], segid: i64) -> Vec<Vec<u8>> {
        let block = build_segment_block(terms, &[], 4050, segid, &[]);
        block
            .data
            .into_iter()
            .filter(|(id, _)| (*id & (1 << 36)) == 0) // leaf pages only (no dlidx)
            .map(|(_, b)| b)
            .collect()
    }

    #[test]
    fn read_segment_postings_roundtrips_with_tombstone() {
        // A segment with a normal posting, a multi-position posting, and a
        // tombstone must decode back to exactly the input postings (del flags
        // preserved), terms with the '0' prefix stripped.
        let terms = vec![
            (b"apple".to_vec(), vec![p(1, &[&[0, 3]]), tomb(4)]),
            (b"banana".to_vec(), vec![p(2, &[&[1]]), p(5, &[&[0]])]),
        ];
        let leaves = seg_leaves_of(&terms, 7);
        let refs: Vec<&[u8]> = leaves.iter().map(|l| l.as_slice()).collect();
        let got = read_segment_postings(&refs).expect("servable");
        assert_eq!(got, terms);
    }

    #[test]
    fn merge_precedence_and_annihilation() {
        // Oldest segment inserts docs 1,2 for "x"; newer segment tombstones doc 1
        // and inserts doc 3. Merge (NOT oldest) keeps the tombstone for doc 1 (it
        // must shadow un-merged levels) and the live docs 2,3.
        let old = vec![(b"x".to_vec(), vec![p(1, &[&[0]]), p(2, &[&[0]])])];
        let new = vec![(b"x".to_vec(), vec![tomb(1), p(3, &[&[0]])])];
        let old_l = seg_leaves_of(&old, 1);
        let new_l = seg_leaves_of(&new, 2);
        let segs: Vec<Vec<&[u8]>> = vec![
            old_l.iter().map(|l| l.as_slice()).collect(),
            new_l.iter().map(|l| l.as_slice()).collect(),
        ];
        // Not oldest: tombstone survives, newest wins per rowid.
        let merged = merge_segments_keepdel(&segs, false).expect("servable");
        assert_eq!(
            merged,
            vec![(b"x".to_vec(), vec![tomb(1), p(2, &[&[0]]), p(3, &[&[0]])])]
        );
        // Oldest output: the tombstone (and the doc it shadows) is annihilated.
        let merged_oldest = merge_segments_keepdel(&segs, true).expect("servable");
        assert_eq!(
            merged_oldest,
            vec![(b"x".to_vec(), vec![p(2, &[&[0]]), p(3, &[&[0]])])]
        );
    }
}
