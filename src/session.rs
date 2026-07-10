//! Change-tracking sessions that produce SQLite-compatible **changesets**.
//!
//! A [`Session`] records the row changes an [`Connection`](crate::Connection)
//! makes to its attached tables between the moment the session is created and
//! the moment [`Connection::session_changeset`](crate::Connection::session_changeset)
//! is called, then serializes them into the documented SQLite *changeset*
//! binary format — byte-for-byte compatible with the output of
//! `sqlite3session_changeset()` from SQLite's session extension.
//!
//! # Model
//!
//! This mirrors SQLite's session module:
//!
//! * A session is attached to a database (currently always `main`) and, via
//!   [`Session::attach`], to every table in it.
//! * As the connection runs `INSERT`/`UPDATE`/`DELETE` on an attached table,
//!   the session records the change, keyed by the row's **primary key** (which
//!   may be a single column or several — see below). Only the **first**
//!   operation on a given row within the session's lifetime is remembered (its
//!   op and the row's *original* values); subsequent edits to the same row are
//!   folded in at changeset time by reading the row's current value from the
//!   database. This reproduces SQLite's coalescing rules:
//!   * `INSERT` then `UPDATE` of the same row → a single `INSERT` of the final
//!     values.
//!   * `INSERT` then `DELETE` of the same row → nothing.
//!   * `UPDATE` then `UPDATE` → one `UPDATE` from the original to the final
//!     values.
//! * [`Connection::session_changeset`](crate::Connection::session_changeset)
//!   walks the attached tables and produces the blob. An empty session yields
//!   an empty changeset ([`Session::is_empty`]).
//!
//! # Scope
//!
//! The recorder tracks tables that have a declared **PRIMARY KEY**, matching
//! the shapes SQLite's session module records under its default configuration:
//!
//! * a single `INTEGER PRIMARY KEY` (rowid alias),
//! * a single non-integer primary key (`TEXT`/`BLOB`/`REAL`/numeric),
//! * a **composite** primary key (`PRIMARY KEY(a, b, …)`),
//! * a `WITHOUT ROWID` table (whose primary key *is* the row key).
//!
//! A table **with no declared primary key** (an implicit-rowid table) is *not*
//! recorded — exactly as SQLite's session module skips it under the default
//! configuration (the `_rowid_`-keyed behaviour is only enabled by the
//! opt-in `SQLITE_SESSION_OBJCONFIG_ROWID` object flag, which the changeset
//! API does not turn on). Such a table simply does not contribute to the
//! changeset. Values of every storage class (INTEGER/REAL/TEXT/BLOB/NULL) are
//! supported.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::error::Result;
use crate::value::Value;

/// Changeset op-code for an inserted row (`SQLITE_INSERT`).
pub(crate) const OP_INSERT: u8 = 18;
/// Changeset op-code for an updated row (`SQLITE_UPDATE`).
pub(crate) const OP_UPDATE: u8 = 23;
/// Changeset op-code for a deleted row (`SQLITE_DELETE`).
pub(crate) const OP_DELETE: u8 = 9;

/// Serial-type marker bytes used inside a changeset record. These match
/// SQLite's `SQLITE_*` value-type constants, which the changeset format reuses.
const T_INT: u8 = 1;
const T_FLOAT: u8 = 2;
const T_TEXT: u8 = 3;
const T_BLOB: u8 = 4;
const T_NULL: u8 = 5;
/// The "field omitted" marker written for an unchanged, non-PK column of an
/// `UPDATE` change.
const T_OMIT: u8 = 0;

/// The op recorded for the *first* change seen for a row within a session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ChangeOp {
    Insert,
    Update,
    Delete,
}

/// One tracked row change. `pk` holds the row's primary-key column values (in
/// column order, only the PK columns), used both to look up the current row at
/// changeset time and to compute the bucket hash. `old` holds the row's values
/// as they were *before* the first change: the full old row for `UPDATE`/
/// `DELETE`, and (for byte-compatibility with SQLite, which stores only the PK
/// for an `INSERT`) an all-`Null`-but-PK row for `INSERT` (never emitted — the
/// live row is re-read for an insert at changeset time).
#[derive(Clone, Debug)]
struct Change {
    op: ChangeOp,
    pk: Vec<Value>,
    old: Vec<Value>,
}

/// Hash one primary-key value into `h` following SQLite's `sessionPreupdateHash`
/// (type byte, then the value's bytes). A `NULL` PK value contributes only its
/// type byte here; the presence of a NULL PK is handled by the caller (which
/// skips such rows, matching SQLite).
fn hash_pk_value(h: u32, v: &Value) -> u32 {
    match v {
        Value::Null => TableChanges::hash_append(h, u32::from(T_NULL)),
        Value::Integer(i) => {
            let h = TableChanges::hash_append(h, u32::from(T_INT));
            TableChanges::hash_i64(h, *i)
        }
        Value::Real(r) => {
            let h = TableChanges::hash_append(h, u32::from(T_FLOAT));
            TableChanges::hash_i64(h, r.to_bits() as i64)
        }
        Value::Text(s) => {
            let h = TableChanges::hash_append(h, u32::from(T_TEXT));
            TableChanges::hash_blob(h, s.as_bytes())
        }
        Value::Blob(b) => {
            let h = TableChanges::hash_append(h, u32::from(T_BLOB));
            TableChanges::hash_blob(h, b)
        }
    }
}

/// Per-table recorded changes, laid out to reproduce SQLite's hash-bucket
/// iteration order exactly (which determines the order of change records in
/// the serialized changeset).
#[derive(Debug)]
struct TableChanges {
    /// Table name.
    name: String,
    /// Number of columns.
    ncol: usize,
    /// Per-column primary-key marker (aligned with columns), matching SQLite's
    /// `abPK`: `0` for a non-PK column, else the column's **1-based position
    /// within the primary key** (as `PRAGMA table_xinfo`'s `pk` column reports).
    /// A single-column PK is `1`; `PRIMARY KEY(b, a)` marks `b`→`1`, `a`→`2`.
    pk_flags: Vec<u8>,
    /// Hash buckets. Each bucket is a LIFO chain (newest first), matching
    /// SQLite's linked-list prepend, holding the change for each distinct row.
    buckets: Vec<Vec<Change>>,
    /// Number of distinct rows recorded (drives bucket growth).
    nentry: usize,
}

impl TableChanges {
    fn new(name: String, ncol: usize, pk_flags: Vec<u8>) -> TableChanges {
        TableChanges {
            name,
            ncol,
            pk_flags,
            buckets: Vec::new(),
            nentry: 0,
        }
    }

    /// SQLite's `HASH_APPEND` step.
    #[inline]
    fn hash_append(h: u32, add: u32) -> u32 {
        (h << 3) ^ h ^ add
    }

    /// Hash an `i64` the way SQLite's `sessionHashAppendI64` does (low 32 bits
    /// then high 32 bits).
    #[inline]
    fn hash_i64(h: u32, i: i64) -> u32 {
        let u = i as u64;
        let h = Self::hash_append(h, (u & 0xFFFF_FFFF) as u32);
        Self::hash_append(h, ((u >> 32) & 0xFFFF_FFFF) as u32)
    }

    /// Hash a blob byte-by-byte (`sessionHashAppendBlob`).
    #[inline]
    fn hash_blob(mut h: u32, bytes: &[u8]) -> u32 {
        for &b in bytes {
            h = Self::hash_append(h, u32::from(b));
        }
        h
    }

    /// Raw hash of a primary-key tuple (type byte + value for each PK column),
    /// before reduction modulo the bucket count. Mirrors `sessionPreupdateHash`
    /// / `sessionChangeHash` for a table with an explicit (non-rowid) primary
    /// key of one or more columns.
    fn hash_pk(pk: &[Value]) -> u32 {
        let mut h = 0u32;
        for v in pk {
            h = hash_pk_value(h, v);
        }
        h
    }

    /// Bucket index for a primary-key tuple, mod the bucket count.
    fn bucket(&self, pk: &[Value]) -> usize {
        (Self::hash_pk(pk) % self.buckets.len() as u32) as usize
    }

    /// Grow (or first-allocate) the bucket array following SQLite's
    /// `sessionGrowHash`: allocate on the first entry and double when the load
    /// factor reaches ½, rehashing existing chains (which prepend, reversing
    /// collision order — reproduced here).
    fn maybe_grow(&mut self) {
        let n = self.buckets.len();
        if n == 0 || self.nentry >= n / 2 {
            // First growth is `2 * 128`, then doubling.
            let new_n = if n == 0 { 256 } else { n * 2 };
            let mut new_buckets: Vec<Vec<Change>> = (0..new_n).map(|_| Vec::new()).collect();
            for bucket in &self.buckets {
                // Iterate head→tail (as stored), prepending into the new bucket.
                for change in bucket {
                    let idx = (Self::hash_pk(&change.pk) % new_n as u32) as usize;
                    new_buckets[idx].insert(0, change.clone());
                }
            }
            self.buckets = new_buckets;
        }
    }

    /// Record one row operation, applying SQLite's coalescing.
    fn record(&mut self, op: ChangeOp, pk: Vec<Value>, old: Vec<Value>) {
        self.maybe_grow();
        let idx = self.bucket(&pk);
        if self.buckets[idx].iter().any(|c| pk_eq(&c.pk, &pk)) {
            // A change already exists for this row: SQLite keeps the original
            // op and original old.* values, folding later edits in at
            // changeset time via the live row read. Nothing to update here.
            return;
        }
        self.nentry += 1;
        // Prepend (LIFO), matching SQLite's linked-list insertion.
        self.buckets[idx].insert(0, Change { op, pk, old });
    }

    fn is_empty(&self) -> bool {
        self.nentry == 0
    }
}

/// Compare two primary-key tuples for the session's row-identity test. Values
/// must match in both storage class and content (SQLite's `sessionPreupdateEqual`
/// / `sessionChangeEqual` compares the raw serialized bytes, so e.g. an integer
/// `1` and a real `1.0` are distinct keys).
fn pk_eq(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).all(|(x, y)| match (x, y) {
        (Value::Null, Value::Null) => true,
        (Value::Integer(i), Value::Integer(j)) => i == j,
        // Compare reals by their bit pattern (the serialized form), matching
        // SQLite's byte comparison of the stored record.
        (Value::Real(i), Value::Real(j)) => i.to_bits() == j.to_bits(),
        (Value::Text(i), Value::Text(j)) => i == j,
        (Value::Blob(i), Value::Blob(j)) => i == j,
        _ => false,
    })
}

/// Shared recorder state. Held by both the [`Session`] and (while active) the
/// [`Connection`](crate::Connection) via a clone of the `Rc`, so the write path
/// can push changes into the session the caller holds.
#[derive(Debug, Default)]
pub(crate) struct SessionState {
    /// `true` once any table is attached (all or by name) and until the session
    /// is dropped/disabled. When `false` the write hook is a no-op.
    pub(crate) enabled: bool,
    /// `true` when every table is attached ([`Session::attach`], i.e.
    /// `sqlite3session_attach(p, NULL)`). When `false`, only the tables named in
    /// `attached` are recorded.
    attach_all: bool,
    /// The specific tables attached by name ([`Session::attach_table`]). Ignored
    /// when `attach_all` is set.
    attached: Vec<String>,
    /// Recorded changes, keyed by table name, in first-touch order (which is
    /// the order tables appear in the changeset).
    tables: Vec<TableChanges>,
}

impl SessionState {
    /// Called from the write path for a single-row operation on a table with a
    /// declared primary key. `op`, the row's primary-key column values (`pk`,
    /// in column order — only the PK columns), and the row's old values (for
    /// UPDATE/DELETE) are recorded. For an INSERT, `old` is ignored (only the
    /// PK matters — the live row is re-read at changeset time).
    ///
    /// A row whose primary key contains a `NULL` is ignored, matching SQLite
    /// (which records no change for such a row).
    pub(crate) fn record(
        &mut self,
        table: &str,
        ncol: usize,
        pk_flags: &[u8],
        op: ChangeOp,
        pk: Vec<Value>,
        old: Vec<Value>,
    ) {
        if !self.enabled {
            return;
        }
        // Per-table attach: when not every table is attached, only record a
        // table that was explicitly attached by name.
        if !self.attach_all && !self.attached.iter().any(|t| t == table) {
            return;
        }
        // SQLite skips rows whose primary key contains a NULL value.
        if pk.iter().any(|v| matches!(v, Value::Null)) {
            return;
        }
        let tbl = match self.tables.iter_mut().position(|t| t.name == table) {
            Some(i) => &mut self.tables[i],
            None => {
                self.tables.push(TableChanges::new(
                    String::from(table),
                    ncol,
                    pk_flags.to_vec(),
                ));
                self.tables.last_mut().unwrap()
            }
        };
        tbl.record(op, pk, old);
    }
}

/// A change-tracking session over a [`Connection`](crate::Connection).
///
/// Create one with [`Connection::create_session`](crate::Connection::create_session),
/// attach the database's tables with [`Session::attach`], run some DML on the
/// connection, then call
/// [`Connection::session_changeset`](crate::Connection::session_changeset) to
/// obtain the serialized changeset. See the [module documentation](self) for
/// the model and current scope.
///
/// A session shares its recorder with the connection through reference
/// counting; dropping the [`Session`] disables recording on the connection.
#[derive(Clone)]
pub struct Session {
    pub(crate) state: Rc<RefCell<SessionState>>,
}

impl Session {
    pub(crate) fn new(state: Rc<RefCell<SessionState>>) -> Session {
        Session { state }
    }

    /// Attach every table in the session's database to the session, so changes
    /// to any of them are recorded. Mirrors `sqlite3session_attach(p, NULL)`.
    ///
    /// Attaching all tables overrides any prior per-table
    /// [`attach_table`](Self::attach_table) restriction.
    pub fn attach(&self) {
        let mut s = self.state.borrow_mut();
        s.enabled = true;
        s.attach_all = true;
    }

    /// Attach a single table by name, so only changes to that table are
    /// recorded (unless [`attach`](Self::attach) is also called, which attaches
    /// all tables). Mirrors `sqlite3session_attach(p, "table")`. Multiple calls
    /// accumulate; a table may be attached before it exists (it is recorded once
    /// it is created and written).
    pub fn attach_table(&self, table: &str) {
        let mut s = self.state.borrow_mut();
        s.enabled = true;
        if !s.attached.iter().any(|t| t == table) {
            s.attached.push(String::from(table));
        }
    }

    /// Returns `true` if no changes have been recorded (so the changeset would
    /// be an empty blob). Mirrors `sqlite3session_isempty`.
    pub fn is_empty(&self) -> bool {
        self.state
            .borrow()
            .tables
            .iter()
            .all(TableChanges::is_empty)
    }
}

/// Append a SQLite varint (1..=9 bytes) encoding of `v` (a non-negative value)
/// to `out`. Matches SQLite's `putVarint32`/`sqlite3PutVarint` for values that
/// fit in the changeset's field-length usage.
fn append_varint(out: &mut Vec<u8>, v: u64) {
    if v <= 0x7f {
        out.push(v as u8);
        return;
    }
    // General SQLite varint: up to 9 bytes, big-endian 7-bit groups, high bit
    // set on all but the last; a full 9th byte carries 8 bits.
    let mut buf = [0u8; 10];
    let mut n = 0;
    let mut val = v;
    if val & 0xff00_0000_0000_0000 != 0 {
        buf[9] = (val & 0xff) as u8;
        val >>= 8;
        for i in (0..9).rev() {
            buf[i] = ((val & 0x7f) as u8) | 0x80;
            val >>= 7;
        }
        out.extend_from_slice(&buf[..10]);
        return;
    }
    while val != 0 {
        buf[n] = (val & 0x7f) as u8;
        val >>= 7;
        n += 1;
    }
    for i in (0..n).rev() {
        let mut byte = buf[i];
        if i != 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

/// Append one value in the changeset's per-field encoding (type byte then
/// payload). `NULL`/int/float/text/blob only. Mirrors `sessionAppendCol`.
fn append_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(T_NULL),
        Value::Integer(i) => {
            out.push(T_INT);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::Real(r) => {
            out.push(T_FLOAT);
            // SQLite stores the raw IEEE-754 bits, big-endian.
            out.extend_from_slice(&r.to_bits().to_be_bytes());
        }
        Value::Text(s) => {
            out.push(T_TEXT);
            append_varint(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Blob(b) => {
            out.push(T_BLOB);
            append_varint(out, b.len() as u64);
            out.extend_from_slice(b);
        }
    }
}

/// Serialize the recorded changes into a changeset blob, given a callback that
/// reads the *current* values of a row by its primary-key column values from
/// table `name` (returning `None` if the row no longer exists). This is called
/// by [`crate::Connection::session_changeset`], which supplies the live read.
pub(crate) fn serialize(
    state: &SessionState,
    read_row: impl FnMut(&str, &[Value]) -> Option<Vec<Value>>,
) -> Vec<u8> {
    serialize_impl(state, false, read_row)
}

/// Serialize the recorded changes into a **patchset** blob (mirrors
/// `sqlite3session_patchset`). A patchset is the changeset format with three
/// differences: the table-header op byte is `'P'` (not `'T'`); a `DELETE`
/// record carries **only** the primary-key columns (each PK field verbatim,
/// non-PK columns omitted entirely — not even a `0x00` placeholder); and an
/// `UPDATE` record carries a **single** record (no `old.*` half) holding the PK
/// columns plus the changed non-PK columns, with unchanged non-PK columns as the
/// `0x00` omitted marker. `INSERT` records are byte-identical to a changeset's.
pub(crate) fn serialize_patchset(
    state: &SessionState,
    read_row: impl FnMut(&str, &[Value]) -> Option<Vec<Value>>,
) -> Vec<u8> {
    serialize_impl(state, true, read_row)
}

/// Shared body of [`serialize`] and [`serialize_patchset`]: `patchset` selects
/// the changeset (`false`) or patchset (`true`) record layout.
fn serialize_impl(
    state: &SessionState,
    patchset: bool,
    mut read_row: impl FnMut(&str, &[Value]) -> Option<Vec<Value>>,
) -> Vec<u8> {
    let mut out = Vec::new();
    for tbl in &state.tables {
        if tbl.is_empty() {
            continue;
        }
        // Table header: 'T' (changeset) / 'P' (patchset), ncol (varint),
        // pk-flag bytes, NUL-terminated name.
        let hdr_start = out.len();
        out.push(if patchset { b'P' } else { b'T' });
        append_varint(&mut out, tbl.ncol as u64);
        // The PK-flag bytes are SQLite's `abPK` verbatim: 0 for a non-PK column,
        // else the column's 1-based position within the primary key.
        out.extend_from_slice(&tbl.pk_flags);
        out.extend_from_slice(tbl.name.as_bytes());
        out.push(0);

        let mut wrote_any = false;
        for bucket in &tbl.buckets {
            for change in bucket {
                // SQLite reads the row's *current* value at changeset time and
                // decides the emitted op from (recorded op, is-row-present):
                //   present:  INSERT → INSERT; UPDATE/DELETE → UPDATE
                //   absent:   INSERT → nothing; UPDATE/DELETE → DELETE
                // This is what makes DELETE-then-INSERT of the same row emit an
                // UPDATE, and INSERT-then-DELETE emit nothing.
                let current = read_row(&tbl.name, &change.pk);
                match (change.op, current) {
                    (ChangeOp::Insert, Some(row)) => {
                        out.push(OP_INSERT);
                        out.push(0); // not indirect
                        for v in &row {
                            append_value(&mut out, v);
                        }
                        wrote_any = true;
                    }
                    (ChangeOp::Insert, None) => {
                        // INSERT then DELETE → nothing.
                    }
                    (ChangeOp::Update | ChangeOp::Delete, Some(row)) => {
                        if append_update(&mut out, &change.old, &row, &tbl.pk_flags, patchset) {
                            wrote_any = true;
                        }
                    }
                    (ChangeOp::Update | ChangeOp::Delete, None) => {
                        append_delete(&mut out, &change.old, &tbl.pk_flags, patchset);
                        wrote_any = true;
                    }
                }
            }
        }

        if !wrote_any {
            // A table whose changes all coalesced away contributes nothing —
            // drop the header we optimistically wrote.
            out.truncate(hdr_start);
        }
    }
    out
}

/// Append a DELETE change. For a changeset (`patchset == false`): op byte,
/// indirect byte, then the full old record. For a patchset (`patchset == true`):
/// op byte, indirect byte, then **only** the primary-key columns (each PK field
/// verbatim, in column order); non-PK columns are omitted entirely (no `0x00`
/// placeholder). Mirrors `sessionAppendDelete`.
fn append_delete(out: &mut Vec<u8>, old: &[Value], pk_flags: &[u8], patchset: bool) {
    out.push(OP_DELETE);
    out.push(0);
    for (i, v) in old.iter().enumerate() {
        let is_pk = pk_flags.get(i).copied().unwrap_or(0) != 0;
        if patchset {
            // Patchset: emit only PK columns; skip non-PK columns entirely.
            if is_pk {
                append_value(out, v);
            }
        } else {
            append_value(out, v);
        }
    }
}

/// Append an UPDATE change. For a changeset (`patchset == false`): op byte,
/// indirect byte, old record (PK columns and changed columns present, unchanged
/// non-PK columns as `OMIT`), then new record (changed columns present,
/// unchanged as `OMIT`). For a patchset (`patchset == true`): op byte, indirect
/// byte, then a **single** record (no old.* half) with PK columns and changed
/// non-PK columns present, unchanged non-PK columns as `OMIT`. Returns `false`
/// (writing nothing) if no non-PK column changed — matching SQLite, which
/// rewinds the buffer for a no-op update.
fn append_update(
    out: &mut Vec<u8>,
    old: &[Value],
    new: &[Value],
    pk_flags: &[u8],
    patchset: bool,
) -> bool {
    let start = out.len();
    out.push(OP_UPDATE);
    out.push(0);

    let ncol = old.len().min(new.len());
    let mut new_rec = Vec::new();
    let mut changed_any = false;
    for i in 0..ncol {
        let is_pk = pk_flags.get(i).copied().unwrap_or(0) != 0;
        let changed = old[i] != new[i];
        if changed {
            changed_any = true;
        }
        // old.* record: only emitted for a changeset. Present if changed or PK,
        // else OMIT. A patchset omits the old.* record entirely.
        if !patchset {
            if changed || is_pk {
                append_value(out, &old[i]);
            } else {
                out.push(T_OMIT);
            }
        }
        // new.* record (the only record for a patchset): present if changed, or
        // (patchset only) if it is a PK column; else OMIT.
        if changed || (patchset && is_pk) {
            append_value(&mut new_rec, &new[i]);
        } else {
            new_rec.push(T_OMIT);
        }
    }

    if !changed_any {
        out.truncate(start);
        return false;
    }
    out.extend_from_slice(&new_rec);
    true
}

// ---------------------------------------------------------------------------
// Changeset parsing (the read side, consumed by `Connection::changeset_apply`).
// ---------------------------------------------------------------------------

/// One parsed change record from a changeset: its op and the old/new field
/// vectors. Each vector has one entry per table column; a `None` entry is the
/// changeset's "field omitted" marker (`0x00`) — present for the `old.*` record
/// of an `INSERT` (all columns) and for unchanged non-PK columns of an
/// `UPDATE`. `DELETE`/`INSERT` carry a full record in `old`/`new` respectively.
#[derive(Debug, Clone)]
pub(crate) struct ChangeRecord {
    pub(crate) op: ChangeOp,
    /// `old.*` values (present for UPDATE/DELETE; empty for INSERT).
    pub(crate) old: Vec<Option<Value>>,
    /// `new.*` values (present for UPDATE/INSERT; empty for DELETE).
    pub(crate) new: Vec<Option<Value>>,
}

/// One table's worth of parsed changes from a changeset.
#[derive(Debug, Clone)]
pub(crate) struct TableChangeset {
    pub(crate) name: String,
    pub(crate) ncol: usize,
    /// SQLite's `abPK` bytes (0 = non-PK; else 1-based PK ordinal).
    pub(crate) pk_flags: Vec<u8>,
    pub(crate) changes: Vec<ChangeRecord>,
}

/// Cursor over a changeset byte buffer.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn u8(&mut self) -> Result<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| corrupt("unexpected end of changeset"))?;
        self.pos += 1;
        Ok(b)
    }

    fn peek(&self) -> Result<u8> {
        self.data
            .get(self.pos)
            .copied()
            .ok_or_else(|| corrupt("unexpected end of changeset"))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.data.len())
            .ok_or_else(|| corrupt("truncated changeset field"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// Read a SQLite varint (the same encoding `append_varint` writes). Returns
    /// the value; supports the full 1..=9 byte range.
    fn varint(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        for i in 0..9 {
            let byte = self.u8()?;
            if i == 8 {
                // Ninth byte contributes all 8 bits.
                result = (result << 8) | u64::from(byte);
                return Ok(result);
            }
            result = (result << 7) | u64::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Ok(result)
    }

    /// Read one changeset value (type byte then payload). A `0x00` type byte is
    /// the "field omitted" marker and yields `None`.
    fn value(&mut self) -> Result<Option<Value>> {
        let t = self.u8()?;
        match t {
            T_OMIT => Ok(None),
            T_NULL => Ok(Some(Value::Null)),
            T_INT => {
                let bytes = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(bytes);
                Ok(Some(Value::Integer(i64::from_be_bytes(a))))
            }
            T_FLOAT => {
                let bytes = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(bytes);
                Ok(Some(Value::Real(f64::from_bits(u64::from_be_bytes(a)))))
            }
            T_TEXT => {
                let n = self.varint()? as usize;
                let bytes = self.take(n)?;
                let s = core::str::from_utf8(bytes)
                    .map_err(|_| corrupt("non-UTF-8 text in changeset"))?;
                Ok(Some(Value::Text(String::from(s))))
            }
            T_BLOB => {
                let n = self.varint()? as usize;
                let bytes = self.take(n)?;
                Ok(Some(Value::Blob(bytes.to_vec())))
            }
            other => Err(corrupt(&alloc::format!(
                "unknown changeset value type {other}"
            ))),
        }
    }

    /// Read `ncol` values (a full record), each possibly omitted.
    fn record(&mut self, ncol: usize) -> Result<Vec<Option<Value>>> {
        let mut out = Vec::with_capacity(ncol);
        for _ in 0..ncol {
            out.push(self.value()?);
        }
        Ok(out)
    }

    /// Read a patchset PK-only record: one value for each column whose `pk_flags`
    /// byte is non-zero (in column order), returning a full `ncol`-length vector
    /// with the PK slots filled and every non-PK slot `None`. Non-PK columns are
    /// **not** present in the byte stream at all (no `0x00` placeholder), so only
    /// the PK fields are consumed.
    fn pk_only_record(&mut self, ncol: usize, pk_flags: &[u8]) -> Result<Vec<Option<Value>>> {
        let mut out = alloc::vec![None; ncol];
        for (i, slot) in out.iter_mut().enumerate().take(ncol) {
            if pk_flags.get(i).copied().unwrap_or(0) != 0 {
                *slot = self.value()?;
            }
        }
        Ok(out)
    }
}

fn corrupt(msg: &str) -> crate::error::Error {
    crate::error::Error::Corrupt(alloc::format!("changeset: {msg}"))
}

/// Parse a changeset **or patchset** blob into per-table change groups.
/// Supports the formats [`serialize`]/[`serialize_patchset`] produce (`'T'` or
/// `'P'` table headers followed by `INSERT`/`UPDATE`/`DELETE` change records).
/// An empty blob yields an empty vector.
///
/// Patchset records are normalized into the same [`ChangeRecord`] shape a
/// changeset yields (so [`crate::Connection::changeset_apply`] handles both):
/// a patchset `DELETE`'s PK-only record and a patchset `UPDATE`'s single record
/// are expanded to full `ncol`-length vectors, with the missing `old.*` fields
/// left as `None` (the apply matches the target row by PK, which is exactly the
/// patchset's "match on PK, ignore old values" semantics).
pub(crate) fn parse_changeset(data: &[u8]) -> Result<Vec<TableChangeset>> {
    let mut r = Reader::new(data);
    let mut tables: Vec<TableChangeset> = Vec::new();
    // Whether the table currently being read used a `'P'` (patchset) header.
    let mut patchset = false;
    while !r.eof() {
        let marker = r.peek()?;
        match marker {
            b'T' | b'P' => {
                patchset = marker == b'P';
                r.u8()?; // consume 'T' / 'P'
                let ncol = r.varint()? as usize;
                if ncol == 0 {
                    return Err(corrupt("table has zero columns"));
                }
                let mut pk_flags = Vec::with_capacity(ncol);
                for _ in 0..ncol {
                    pk_flags.push(r.u8()?);
                }
                // NUL-terminated table name.
                let start = r.pos;
                loop {
                    let b = r.u8()?;
                    if b == 0 {
                        break;
                    }
                }
                let name_bytes = &r.data[start..r.pos - 1];
                let name = String::from(
                    core::str::from_utf8(name_bytes)
                        .map_err(|_| corrupt("non-UTF-8 table name"))?,
                );
                tables.push(TableChangeset {
                    name,
                    ncol,
                    pk_flags,
                    changes: Vec::new(),
                });
            }
            OP_INSERT | OP_UPDATE | OP_DELETE => {
                let tbl = tables
                    .last_mut()
                    .ok_or_else(|| corrupt("change record before any table header"))?;
                let ncol = tbl.ncol;
                let pk_flags = tbl.pk_flags.clone();
                let op = r.u8()?;
                let _indirect = r.u8()?;
                let rec = match op {
                    OP_INSERT => ChangeRecord {
                        op: ChangeOp::Insert,
                        old: Vec::new(),
                        new: r.record(ncol)?,
                    },
                    OP_DELETE if patchset => {
                        // Patchset DELETE: only PK columns are present, in
                        // column order. Expand to a full old.* record with the
                        // PK values filled and non-PK columns left `None`.
                        let old = r.pk_only_record(ncol, &pk_flags)?;
                        ChangeRecord {
                            op: ChangeOp::Delete,
                            old,
                            new: Vec::new(),
                        }
                    }
                    OP_DELETE => ChangeRecord {
                        op: ChangeOp::Delete,
                        old: r.record(ncol)?,
                        new: Vec::new(),
                    },
                    _ if patchset => {
                        // Patchset UPDATE: a single record with PK columns and
                        // changed non-PK columns present (unchanged as `0x00`).
                        // Use it as new.*; synthesize old.* holding just the PK
                        // values (so apply matches the row by PK only).
                        let new = r.record(ncol)?;
                        let mut old = alloc::vec![None; ncol];
                        for (i, flag) in pk_flags.iter().enumerate().take(ncol) {
                            if *flag != 0 {
                                old[i] = new[i].clone();
                            }
                        }
                        ChangeRecord {
                            op: ChangeOp::Update,
                            old,
                            new,
                        }
                    }
                    _ => {
                        // Changeset UPDATE: old record then new record.
                        let old = r.record(ncol)?;
                        let new = r.record(ncol)?;
                        ChangeRecord {
                            op: ChangeOp::Update,
                            old,
                            new,
                        }
                    }
                };
                tbl.changes.push(rec);
            }
            other => {
                return Err(corrupt(&alloc::format!(
                    "unexpected marker byte {other:#x}"
                )));
            }
        }
    }
    Ok(tables)
}

// ---------------------------------------------------------------------------
// Changeset → changeset transforms: `invert` and `concat`.
//
// These are *pure* byte transforms over the changeset format — they need no
// database and no exec-layer support. They reproduce SQLite's
// `sqlite3changeset_invert` and `sqlite3changeset_concat` (the latter via an
// in-memory change-group), byte-for-byte.
// ---------------------------------------------------------------------------

/// Length in bytes of the serialized field beginning at `a[0]` (SQLite's
/// `sessionSerialLen`). The leading byte is the type marker: `0x00` (omitted)
/// and `0xFF` (an internal "undefined" marker, never present in a well-formed
/// changeset but handled for completeness) are one byte; `T_NULL` is one byte;
/// `T_INT`/`T_FLOAT` are nine (type + 8-byte payload); text/blob are the type
/// byte, a varint length, and that many payload bytes.
fn serial_len(a: &[u8]) -> Result<usize> {
    let e = *a.first().ok_or_else(|| corrupt("truncated record field"))?;
    match e {
        0x00 | 0xFF | T_NULL => Ok(1),
        T_INT | T_FLOAT => {
            if a.len() < 9 {
                return Err(corrupt("truncated int/float field"));
            }
            Ok(9)
        }
        T_TEXT | T_BLOB => {
            // Read the varint length starting at a[1], then add its own byte
            // count plus the type byte and payload.
            let mut r = Reader::new(a);
            r.pos = 1;
            let n = r.varint()? as usize;
            let nvar = r.pos - 1;
            r.pos
                .checked_add(n)
                .filter(|e| *e <= a.len())
                .ok_or_else(|| corrupt("truncated text/blob field"))?;
            Ok(1 + nvar + n)
        }
        other => Err(corrupt(&alloc::format!(
            "unknown changeset value type {other}"
        ))),
    }
}

/// Split the record of `ncol` serialized fields at the start of `a` into a
/// vector of per-field byte slices, and return them together with the total
/// number of bytes consumed. Each slice includes the field's type byte and any
/// payload (an omitted field is the single byte `0x00`).
fn split_record(a: &[u8], ncol: usize) -> Result<(Vec<&[u8]>, usize)> {
    let mut fields = Vec::with_capacity(ncol);
    let mut off = 0usize;
    for _ in 0..ncol {
        let n = serial_len(&a[off..])?;
        fields.push(&a[off..off + n]);
        off += n;
    }
    Ok((fields, off))
}

/// A parsed table header from a changeset: the `'T'`-record's column count,
/// `abPK` flag bytes, and name — plus the full serialized header bytes (from
/// the `'T'` byte through the name's NUL terminator) so it can be re-emitted
/// verbatim.
struct TableHdr {
    ncol: usize,
    pk_flags: Vec<u8>,
    name: Vec<u8>,
    /// The verbatim header bytes `T <ncol-varint> <abPK…> <name> 00`.
    raw: Vec<u8>,
}

/// Read a `'T'` table header at the reader's current position (which must be on
/// the `'T'` byte). Advances past the NUL-terminated name.
fn read_table_hdr(r: &mut Reader<'_>) -> Result<TableHdr> {
    let start = r.pos;
    let t = r.u8()?;
    debug_assert_eq!(t, b'T');
    let ncol = r.varint()? as usize;
    if ncol == 0 {
        return Err(corrupt("table has zero columns"));
    }
    let mut pk_flags = Vec::with_capacity(ncol);
    for _ in 0..ncol {
        pk_flags.push(r.u8()?);
    }
    let name_start = r.pos;
    loop {
        if r.u8()? == 0 {
            break;
        }
    }
    let name = r.data[name_start..r.pos - 1].to_vec();
    let raw = r.data[start..r.pos].to_vec();
    Ok(TableHdr {
        ncol,
        pk_flags,
        name,
        raw,
    })
}

/// Invert a changeset, mirroring `sqlite3changeset_invert`.
///
/// `INSERT` records become `DELETE` and vice-versa (op byte flipped, indirect
/// flag and the record copied verbatim). For an `UPDATE`, the op and indirect
/// flag are preserved and the two records are rebuilt: the inverted **old.\***
/// record takes the primary-key columns from the original old.\* record and the
/// other columns from the original new.\* record; the inverted **new.\*** record
/// copies the original old.\* record except the primary-key columns, which
/// become the "undefined"/omitted marker. Table headers pass through unchanged.
///
/// Returns [`crate::error::Error::Corrupt`] if `changeset` is not a well-formed
/// changeset.
pub(crate) fn invert(changeset: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader::new(changeset);
    let mut out = Vec::with_capacity(changeset.len());
    let mut ncol = 0usize;
    let mut pk_flags: Vec<u8> = Vec::new();

    while !r.eof() {
        match r.peek()? {
            b'T' => {
                let hdr = read_table_hdr(&mut r)?;
                out.extend_from_slice(&hdr.raw);
                ncol = hdr.ncol;
                pk_flags = hdr.pk_flags;
            }
            op @ (OP_INSERT | OP_DELETE) => {
                r.u8()?;
                let indirect = r.u8()?;
                let (_, consumed) = split_record(&r.data[r.pos..], ncol)?;
                let rec = r.take(consumed)?;
                out.push(if op == OP_INSERT {
                    OP_DELETE
                } else {
                    OP_INSERT
                });
                out.push(indirect);
                out.extend_from_slice(rec);
            }
            OP_UPDATE => {
                r.u8()?;
                let indirect = r.u8()?;
                let (old, n_old) = {
                    let (f, n) = split_record(&r.data[r.pos..], ncol)?;
                    (f.into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>(), n)
                };
                r.pos += n_old;
                let (new, n_new) = {
                    let (f, n) = split_record(&r.data[r.pos..], ncol)?;
                    (f.into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>(), n)
                };
                r.pos += n_new;

                out.push(OP_UPDATE);
                out.push(indirect);
                // New old.*: PK columns from old old.*, others from old new.*.
                for i in 0..ncol {
                    let is_pk = pk_flags.get(i).copied().unwrap_or(0) != 0;
                    out.extend_from_slice(if is_pk { &old[i] } else { &new[i] });
                }
                // New new.*: old old.* except PK columns → omitted (0x00).
                for (i, field) in old.iter().enumerate().take(ncol) {
                    let is_pk = pk_flags.get(i).copied().unwrap_or(0) != 0;
                    if is_pk {
                        out.push(T_OMIT);
                    } else {
                        out.extend_from_slice(field);
                    }
                }
            }
            other => {
                return Err(corrupt(&alloc::format!(
                    "unexpected marker byte {other:#x}"
                )));
            }
        }
    }
    Ok(out)
}

// --- concat (in-memory change-group) --------------------------------------

/// One coalesced change held in a [`ConcatTable`]'s hash: its op, indirect
/// flag, and the raw record bytes. For `INSERT`/`DELETE` the record is the
/// single value vector; for `UPDATE` it is the old.\* record followed by the
/// new.\* record (`2 * ncol` fields).
#[derive(Clone)]
struct ConcatChange {
    op: u8,
    indirect: u8,
    record: Vec<u8>,
}

/// Per-table hash of coalesced changes for [`concat`], reproducing SQLite's
/// change-group bucket layout so the output order is byte-identical.
struct ConcatTable {
    ncol: usize,
    pk_flags: Vec<u8>,
    /// Table name (used to match a later changeset's header to this table).
    name: Vec<u8>,
    /// Verbatim table-header bytes, emitted before this table's changes.
    hdr: Vec<u8>,
    /// Hash buckets; each bucket is a LIFO chain (newest first).
    buckets: Vec<Vec<ConcatChange>>,
    nentry: usize,
}

impl ConcatTable {
    fn new(hdr: &TableHdr) -> ConcatTable {
        ConcatTable {
            ncol: hdr.ncol,
            pk_flags: hdr.pk_flags.clone(),
            name: hdr.name.clone(),
            hdr: hdr.raw.clone(),
            buckets: Vec::new(),
            nentry: 0,
        }
    }

    /// Hash the primary-key fields of `record` into a bucket index, mirroring
    /// SQLite's `sessionChangeHash` (which hashes only the PK columns, using the
    /// same append steps as the recorder's [`TableChanges::hash_pk`]).
    fn hash(&self, record: &[u8], nbucket: usize) -> Result<usize> {
        let mut h = 0u32;
        let mut off = 0usize;
        for i in 0..self.ncol {
            let n = serial_len(&record[off..])?;
            let field = &record[off..off + n];
            if self.pk_flags.get(i).copied().unwrap_or(0) != 0 {
                let ty = field[0];
                h = TableChanges::hash_append(h, u32::from(ty));
                match ty {
                    T_INT | T_FLOAT => {
                        let mut a = [0u8; 8];
                        a.copy_from_slice(&field[1..9]);
                        h = TableChanges::hash_i64(h, i64::from_be_bytes(a));
                    }
                    T_TEXT | T_BLOB => {
                        // Skip the varint length; hash only the payload bytes.
                        let mut rr = Reader::new(field);
                        rr.pos = 1;
                        let nb = rr.varint()? as usize;
                        h = TableChanges::hash_blob(h, &field[rr.pos..rr.pos + nb]);
                    }
                    // T_NULL contributes only its type byte (already appended).
                    _ => {}
                }
            }
            off += n;
        }
        Ok((h % nbucket as u32) as usize)
    }

    /// Whether two records refer to the same row (equal PK fields), comparing
    /// the raw serialized bytes as SQLite's `sessionChangeEqual` does.
    fn pk_equal(&self, a: &[u8], b: &[u8]) -> Result<bool> {
        let mut oa = 0usize;
        let mut ob = 0usize;
        for i in 0..self.ncol {
            let na = serial_len(&a[oa..])?;
            let nb = serial_len(&b[ob..])?;
            if self.pk_flags.get(i).copied().unwrap_or(0) != 0
                && (na != nb || a[oa..oa + na] != b[ob..ob + nb])
            {
                return Ok(false);
            }
            oa += na;
            ob += nb;
        }
        Ok(true)
    }

    /// Grow (or first-allocate) the bucket array, mirroring `sessionGrowHash`:
    /// allocate 256 buckets on the first entry, then double whenever the entry
    /// count reaches half the bucket count, rehashing (prepend, LIFO).
    fn maybe_grow(&mut self) -> Result<()> {
        let n = self.buckets.len();
        if n == 0 || self.nentry >= n / 2 {
            let new_n = if n == 0 { 256 } else { n * 2 };
            let mut nb: Vec<Vec<ConcatChange>> = (0..new_n).map(|_| Vec::new()).collect();
            for bucket in &self.buckets {
                for change in bucket {
                    let idx = self.hash(&change.record, new_n)?;
                    nb[idx].insert(0, change.clone());
                }
            }
            self.buckets = nb;
        }
        Ok(())
    }

    /// Add one change (op/indirect/record) to the hash, coalescing with any
    /// existing change for the same row per SQLite's `sessionChangeMerge`.
    fn add(&mut self, op: u8, indirect: u8, record: Vec<u8>) -> Result<()> {
        self.maybe_grow()?;
        let nbucket = self.buckets.len();
        let idx = self.hash(&record, nbucket)?;
        // Remove any existing change for this row (may be re-linked below).
        let existing = {
            let mut found = None;
            for (j, c) in self.buckets[idx].iter().enumerate() {
                if self.pk_equal(&c.record, &record)? {
                    found = Some(j);
                    break;
                }
            }
            found.map(|j| {
                self.nentry -= 1;
                self.buckets[idx].remove(j)
            })
        };

        let merged = self.merge(existing, op, indirect, record)?;
        if let Some(c) = merged {
            self.buckets[idx].insert(0, c);
            self.nentry += 1;
        }
        Ok(())
    }

    /// Coalesce an existing change (if any) with the incoming one, returning the
    /// merged change or `None` if the pair annihilates. Reproduces
    /// `sessionChangeMerge`'s changeset (non-patchset, non-rebase) rules.
    fn merge(
        &self,
        existing: Option<ConcatChange>,
        op2: u8,
        indirect2: u8,
        rec2: Vec<u8>,
    ) -> Result<Option<ConcatChange>> {
        let Some(pexist) = existing else {
            return Ok(Some(ConcatChange {
                op: op2,
                indirect: indirect2,
                record: rec2,
            }));
        };
        let op1 = pexist.op;
        // Unsupported / discard-op2 combinations keep the existing change.
        if (op1 == OP_INSERT && op2 == OP_INSERT)
            || (op1 == OP_UPDATE && op2 == OP_INSERT)
            || (op1 == OP_DELETE && op2 == OP_UPDATE)
            || (op1 == OP_DELETE && op2 == OP_DELETE)
        {
            return Ok(Some(pexist));
        }
        // INSERT then DELETE → annihilate.
        if op1 == OP_INSERT && op2 == OP_DELETE {
            return Ok(None);
        }

        // Every remaining merge sets the new indirect flag to the AND of the two
        // (SQLite: `bIndirect && pExist->bIndirect`).
        let indirect = u8::from(indirect2 != 0 && pexist.indirect != 0);

        let (op, record) = if op1 == OP_INSERT {
            // INSERT + UPDATE → INSERT of merged new values. The INSERT record
            // is a full row; the UPDATE's new.* record (second half of rec2)
            // supplies changed columns.
            debug_assert!(op2 == OP_UPDATE);
            let (_, half) = split_record(&rec2, self.ncol)?;
            let new_part = &rec2[half..];
            let mut out = Vec::new();
            self.merge_record(&mut out, &pexist.record, new_part)?;
            (OP_INSERT, out)
        } else if op1 == OP_DELETE {
            // DELETE + INSERT → UPDATE from the deleted row to the inserted row.
            debug_assert!(op2 == OP_INSERT);
            let mut out = Vec::new();
            let ok = self.merge_update(&mut out, &pexist.record, None, &rec2, None)?;
            if !ok {
                return Ok(None);
            }
            (OP_UPDATE, out)
        } else if op2 == OP_UPDATE {
            // UPDATE + UPDATE. SQLite calls
            //   sessionMergeUpdate(aRec, aExist, aExist.new, aRec.new)
            // where the merged old.* prefers the *existing* (earlier) change's
            // old value and the merged new.* prefers the *incoming* change's new
            // value — see `merge_update`, which always prefers its "two" record.
            debug_assert!(op1 == OP_UPDATE);
            let (_, half_e) = split_record(&pexist.record, self.ncol)?;
            let (exist_old, exist_new) = pexist.record.split_at(half_e);
            let (_, half_i) = split_record(&rec2, self.ncol)?;
            let (in_old, in_new) = rec2.split_at(half_i);
            let mut out = Vec::new();
            // old1 = incoming.old, old2 = existing.old (prefer existing.old);
            // new1 = existing.new, new2 = incoming.new (prefer incoming.new).
            let ok =
                self.merge_update(&mut out, in_old, Some(exist_old), exist_new, Some(in_new))?;
            if !ok {
                return Ok(None);
            }
            (OP_UPDATE, out)
        } else {
            // UPDATE + DELETE → DELETE of the original row.
            debug_assert!(op1 == OP_UPDATE && op2 == OP_DELETE);
            let (_, half1) = split_record(&pexist.record, self.ncol)?;
            let old1 = &pexist.record[..half1];
            let mut out = Vec::new();
            self.merge_record(&mut out, &rec2, old1)?;
            (OP_DELETE, out)
        };

        Ok(Some(ConcatChange {
            op,
            indirect,
            record,
        }))
    }

    /// Merge two single records field-by-field: take the right field when it is
    /// present (non-`0x00`), else the left. Mirrors `sessionMergeRecord`.
    fn merge_record(&self, out: &mut Vec<u8>, left: &[u8], right: &[u8]) -> Result<()> {
        let mut lo = 0usize;
        let mut ro = 0usize;
        for _ in 0..self.ncol {
            let nl = serial_len(&left[lo..])?;
            let nr = serial_len(&right[ro..])?;
            if right[ro] != 0 {
                out.extend_from_slice(&right[ro..ro + nr]);
            } else {
                out.extend_from_slice(&left[lo..lo + nl]);
            }
            lo += nl;
            ro += nr;
        }
        Ok(())
    }

    /// Merge two UPDATE changes on the same row, writing the combined old.\* and
    /// new.\* records. Mirrors `sessionMergeUpdate` (changeset form). Returns
    /// `false` (writing nothing) when the merged update has no effective change.
    fn merge_update(
        &self,
        out: &mut Vec<u8>,
        old1: &[u8],
        old2: Option<&[u8]>,
        new1: &[u8],
        new2: Option<&[u8]>,
    ) -> Result<bool> {
        let start = out.len();

        // old.* vector.
        let mut co1 = 0usize;
        let mut co2 = 0usize;
        let mut cn1 = 0usize;
        let mut cn2 = 0usize;
        let mut required = false;
        for i in 0..self.ncol {
            let old = merge_value(old1, &mut co1, old2, &mut co2)?;
            let new = merge_value(new1, &mut cn1, new2, &mut cn2)?;
            let is_pk = self.pk_flags.get(i).copied().unwrap_or(0) != 0;
            if is_pk || old != new {
                if !is_pk {
                    required = true;
                }
                out.extend_from_slice(old);
            } else {
                out.push(T_OMIT);
            }
        }
        if !required {
            out.truncate(start);
            return Ok(false);
        }

        // new.* vector.
        let mut co1 = 0usize;
        let mut co2 = 0usize;
        let mut cn1 = 0usize;
        let mut cn2 = 0usize;
        for i in 0..self.ncol {
            let old = merge_value(old1, &mut co1, old2, &mut co2)?;
            let new = merge_value(new1, &mut cn1, new2, &mut cn2)?;
            let is_pk = self.pk_flags.get(i).copied().unwrap_or(0) != 0;
            if is_pk || old == new {
                out.push(T_OMIT);
            } else {
                out.extend_from_slice(new);
            }
        }
        Ok(true)
    }
}

/// Advance through two parallel records, returning the field to use for the
/// current column: the "two" field if present (non-`0x00`), else the "one"
/// field. Both cursors advance past their current field. Mirrors
/// `sessionMergeValue`. When `two` is `None`, always yields the "one" field.
fn merge_value<'a>(
    one: &'a [u8],
    co: &mut usize,
    two: Option<&'a [u8]>,
    ct: &mut usize,
) -> Result<&'a [u8]> {
    let n1 = serial_len(&one[*co..])?;
    let f1 = &one[*co..*co + n1];
    let mut ret: Option<&[u8]> = None;
    if let Some(two) = two {
        let n2 = serial_len(&two[*ct..])?;
        let f2 = &two[*ct..*ct + n2];
        if f2[0] != 0 {
            ret = Some(f2);
        }
        *ct += n2;
    }
    *co += n1;
    Ok(ret.unwrap_or(f1))
}

/// Read every change of a changeset into the per-table concat hashes,
/// preserving table first-encounter order across the whole call. `tables` is
/// keyed by table name.
fn concat_absorb(changeset: &[u8], tables: &mut Vec<ConcatTable>) -> Result<()> {
    let mut r = Reader::new(changeset);
    let mut cur: Option<usize> = None;
    while !r.eof() {
        match r.peek()? {
            b'T' => {
                let hdr = read_table_hdr(&mut r)?;
                let idx = tables.iter().position(|t| t.name == hdr.name);
                cur = Some(match idx {
                    Some(i) => {
                        // A later changeset must agree on shape.
                        if tables[i].ncol != hdr.ncol || tables[i].pk_flags != hdr.pk_flags {
                            return Err(corrupt("incompatible table definition in concat"));
                        }
                        i
                    }
                    None => {
                        tables.push(ConcatTable::new(&hdr));
                        tables.len() - 1
                    }
                });
            }
            op @ (OP_INSERT | OP_DELETE | OP_UPDATE) => {
                let ti = cur.ok_or_else(|| corrupt("change record before any table header"))?;
                let ncol = tables[ti].ncol;
                r.u8()?;
                let indirect = r.u8()?;
                let nfields = if op == OP_UPDATE { ncol * 2 } else { ncol };
                let (_, consumed) = split_record(&r.data[r.pos..], nfields)?;
                let record = r.take(consumed)?.to_vec();
                tables[ti].add(op, indirect, record)?;
            }
            other => {
                return Err(corrupt(&alloc::format!(
                    "unexpected marker byte {other:#x}"
                )));
            }
        }
    }
    Ok(())
}

/// Concatenate two changesets, mirroring `sqlite3changeset_concat`: the result
/// is equivalent to applying `a` and then `b`. Per-table, per-row changes are
/// coalesced exactly as an in-memory change-group would (INSERT+UPDATE→INSERT,
/// INSERT+DELETE→nothing, UPDATE+UPDATE→UPDATE, UPDATE+DELETE→DELETE,
/// DELETE+INSERT→UPDATE, and the "unsupported, discard the second" cases).
///
/// Returns [`crate::error::Error::Corrupt`] if either input is malformed or the
/// two disagree on a table's column count or primary-key layout.
pub(crate) fn concat(a: &[u8], b: &[u8]) -> Result<Vec<u8>> {
    let mut tables: Vec<ConcatTable> = Vec::new();
    concat_absorb(a, &mut tables)?;
    concat_absorb(b, &mut tables)?;

    let mut out = Vec::new();
    for tbl in &tables {
        if tbl.nentry == 0 {
            continue;
        }
        out.extend_from_slice(&tbl.hdr);
        for bucket in &tbl.buckets {
            for change in bucket {
                out.push(change.op);
                out.push(change.indirect);
                out.extend_from_slice(&change.record);
            }
        }
    }
    Ok(out)
}

/// The kind of conflict encountered while applying a changeset or patchset,
/// passed to the conflict handler of
/// [`Connection::changeset_apply_with`](crate::Connection::changeset_apply_with).
/// Mirrors SQLite's `SQLITE_CHANGESET_*` `eConflict` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictType {
    /// A `DELETE`/`UPDATE` found a row with the required primary key, but its
    /// non-primary-key values differ from the `old.*` values in the change
    /// (`SQLITE_CHANGESET_DATA`). May be resolved with `Omit`, `Replace`, or
    /// `Abort`.
    Data,
    /// A `DELETE`/`UPDATE` found no row with the required primary key
    /// (`SQLITE_CHANGESET_NOTFOUND`). May be resolved with `Omit` or `Abort`
    /// (a `Replace` is treated as `Abort`, matching SQLite's misuse handling).
    NotFound,
    /// An `INSERT` could not proceed because a row with the same primary key
    /// already exists (`SQLITE_CHANGESET_CONFLICT`). May be resolved with
    /// `Omit`, `Replace` (delete the existing row, then insert), or `Abort`.
    Conflict,
    /// A change violated another constraint — a secondary `UNIQUE` index,
    /// `NOT NULL`, or `CHECK` (`SQLITE_CHANGESET_CONSTRAINT`). May be resolved
    /// with `Omit` or `Abort` (a `Replace` is treated as `Abort`).
    Constraint,
}

/// How to resolve a changeset-apply conflict, returned by the conflict handler
/// of [`Connection::changeset_apply_with`](crate::Connection::changeset_apply_with).
/// Mirrors SQLite's `SQLITE_CHANGESET_OMIT`/`_REPLACE`/`_ABORT` return codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    /// Skip the conflicting change and continue applying the rest.
    Omit,
    /// Force the change through: for [`ConflictType::Conflict`] delete the
    /// existing row and insert; for [`ConflictType::Data`] apply the
    /// `UPDATE`/`DELETE` matched by primary key alone. Only valid for `Data`
    /// and `Conflict`; for `NotFound`/`Constraint` it is treated as `Abort`.
    Replace,
    /// Abort the whole apply: every change made so far is rolled back and
    /// [`changeset_apply_with`](crate::Connection::changeset_apply_with) returns
    /// an error.
    Abort,
}

/// Byte-transform entry points for changesets: [`Changeset::invert`] and
/// [`Changeset::concat`], mirroring SQLite's `sqlite3changeset_invert` and
/// `sqlite3changeset_concat`. This is a zero-sized namespace type — the
/// transforms need no database state.
#[derive(Debug, Clone, Copy)]
pub struct Changeset;

impl Changeset {
    /// Return the inverse of `changeset`: applying the inverse undoes applying
    /// the original. Mirrors `sqlite3changeset_invert` — `INSERT`↔`DELETE`, and
    /// an `UPDATE`'s old and new values are swapped (primary-key columns
    /// unchanged); table headers pass through.
    ///
    /// # Errors
    /// [`crate::error::Error::Corrupt`] if `changeset` is malformed.
    pub fn invert(changeset: &[u8]) -> Result<Vec<u8>> {
        invert(changeset)
    }

    /// Concatenate two changesets into one whose effect equals applying `a`
    /// then `b`. Mirrors `sqlite3changeset_concat`: per-table, per-row changes
    /// are coalesced exactly as an in-memory change-group would.
    ///
    /// # Errors
    /// [`crate::error::Error::Corrupt`] if either input is malformed or they
    /// disagree on a table's shape.
    pub fn concat(a: &[u8], b: &[u8]) -> Result<Vec<u8>> {
        concat(a, b)
    }
}

/// Compile-time check on the public [`Session`] type's auto-traits.
///
/// [`Session`] shares its recorder with the connection through `Rc<RefCell<…>>`,
/// so it is intentionally **not** `Send`/`Sync` (a session is bound to the
/// single-threaded connection it was created on, mirroring SQLite's session
/// objects). It must remain `Clone` (cheap handle) but must not expose any
/// broader thread-safety guarantee it cannot honor.
const _: () = {
    fn assert_clone<T: Clone>() {}
    fn checks() {
        assert_clone::<Session>();
    }
    let _ = checks;
};

/// Compile-time check on the public [`Changeset`] namespace type's auto-traits.
///
/// Unlike [`Session`], [`Changeset`] is a stateless zero-sized handle over pure
/// byte transforms, so it is `Send`/`Sync`/`Copy` — these assertions pin that
/// down so a future field addition cannot silently take those away.
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assert_copy<T: Copy>() {}
    fn checks() {
        assert_send::<Changeset>();
        assert_sync::<Changeset>();
        assert_copy::<Changeset>();
    }
    let _ = checks;
};

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        let mut s = String::new();
        for byte in b {
            s.push_str(&alloc::format!("{byte:02x}"));
        }
        s
    }

    /// A single INSERT of (1, 2) into `t(a INTEGER PRIMARY KEY, b)` must equal
    /// SQLite's reference changeset.
    #[test]
    fn insert_matches_oracle() {
        let mut st = SessionState {
            enabled: true,
            attach_all: true,
            attached: Vec::new(),
            tables: Vec::new(),
        };
        st.record(
            "t",
            2,
            &[1, 0],
            ChangeOp::Insert,
            alloc::vec![Value::Integer(1)],
            alloc::vec![Value::Integer(1), Value::Null],
        );
        let out = serialize(&st, |_, pk| {
            assert_eq!(pk, [Value::Integer(1)]);
            Some(alloc::vec![Value::Integer(1), Value::Integer(2)])
        });
        assert_eq!(
            hex(&out),
            "5402010074001200010000000000000001010000000000000002"
        );
    }

    /// A composite-PK INSERT of (1, 2, 3) into `t(a,b,c, PRIMARY KEY(a,b))`.
    #[test]
    fn composite_insert_matches_oracle() {
        let mut st = SessionState {
            enabled: true,
            attach_all: true,
            attached: Vec::new(),
            tables: Vec::new(),
        };
        st.record(
            "t",
            3,
            &[1, 2, 0],
            ChangeOp::Insert,
            alloc::vec![Value::Integer(1), Value::Integer(2)],
            alloc::vec![Value::Integer(1), Value::Integer(2), Value::Null],
        );
        let out = serialize(&st, |_, pk| {
            assert_eq!(pk, [Value::Integer(1), Value::Integer(2)]);
            Some(alloc::vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3)
            ])
        });
        assert_eq!(
            hex(&out),
            "540301020074001200010000000000000001\
             010000000000000002010000000000000003"
        );
    }

    // --- patchset (byte-verified against the SQLite 3.50.4 oracle) ---------

    /// A patchset INSERT is byte-identical to a changeset INSERT except the
    /// table-header op byte is `'P'` (0x50) instead of `'T'` (0x54).
    #[test]
    fn patchset_insert_matches_oracle() {
        let mut st = SessionState {
            enabled: true,
            attach_all: true,
            attached: Vec::new(),
            tables: Vec::new(),
        };
        st.record(
            "t",
            2,
            &[1, 0],
            ChangeOp::Insert,
            alloc::vec![Value::Integer(1)],
            alloc::vec![Value::Integer(1), Value::Null],
        );
        let out = serialize_patchset(&st, |_, _| {
            Some(alloc::vec![Value::Integer(1), Value::Integer(2)])
        });
        assert_eq!(
            hex(&out),
            "5002010074001200010000000000000001010000000000000002"
        );
    }

    /// A patchset UPDATE (single-INTEGER-PK table `t(a pk, b, c)`, setting `b`)
    /// carries a single record: PK present, changed column present, unchanged
    /// column `0x00` — no `old.*` record.
    #[test]
    fn patchset_update_matches_oracle() {
        let mut st = SessionState {
            enabled: true,
            attach_all: true,
            attached: Vec::new(),
            tables: Vec::new(),
        };
        st.record(
            "t",
            3,
            &[1, 0, 0],
            ChangeOp::Update,
            alloc::vec![Value::Integer(1)],
            alloc::vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        );
        let out = serialize_patchset(&st, |_, _| {
            Some(alloc::vec![
                Value::Integer(1),
                Value::Integer(20),
                Value::Integer(3)
            ])
        });
        assert_eq!(
            hex(&out),
            "50030100007400170001000000000000000101000000000000001400"
        );
    }

    /// A patchset DELETE carries only the primary-key columns (non-PK columns
    /// omitted entirely — no `0x00` placeholder).
    #[test]
    fn patchset_delete_matches_oracle() {
        let mut st = SessionState {
            enabled: true,
            attach_all: true,
            attached: Vec::new(),
            tables: Vec::new(),
        };
        st.record(
            "t",
            3,
            &[1, 0, 0],
            ChangeOp::Delete,
            alloc::vec![Value::Integer(1)],
            alloc::vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        );
        // The row is gone at patchset time → DELETE.
        let out = serialize_patchset(&st, |_, _| None);
        assert_eq!(hex(&out), "500301000074000900010000000000000001");
    }

    /// A composite-PK patchset DELETE keeps both PK columns, dropping the non-PK
    /// column.
    #[test]
    fn patchset_composite_delete_matches_oracle() {
        let mut st = SessionState {
            enabled: true,
            attach_all: true,
            attached: Vec::new(),
            tables: Vec::new(),
        };
        st.record(
            "t",
            3,
            &[1, 2, 0],
            ChangeOp::Delete,
            alloc::vec![Value::Integer(1), Value::Integer(2)],
            alloc::vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        );
        let out = serialize_patchset(&st, |_, _| None);
        assert_eq!(
            hex(&out),
            "500301020074000900010000000000000001010000000000000002"
        );
    }

    /// A composite-PK patchset UPDATE (`t(a,b,c, PRIMARY KEY(a,b))`, setting
    /// `c`) keeps both PK columns plus the changed column in the single record.
    #[test]
    fn patchset_composite_update_matches_oracle() {
        let mut st = SessionState {
            enabled: true,
            attach_all: true,
            attached: Vec::new(),
            tables: Vec::new(),
        };
        st.record(
            "t",
            3,
            &[1, 2, 0],
            ChangeOp::Update,
            alloc::vec![Value::Integer(1), Value::Integer(2)],
            alloc::vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        );
        let out = serialize_patchset(&st, |_, _| {
            Some(alloc::vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(30)
            ])
        });
        assert_eq!(
            hex(&out),
            "50030102007400170001000000000000000101000000000000000201000000000000001e"
        );
    }

    /// Parsing a patchset back yields change records the apply layer consumes:
    /// a patchset DELETE fills only PK slots (non-PK `None`); a patchset UPDATE's
    /// single record becomes `new`, with `old` holding just the PK values.
    #[test]
    fn parse_patchset_normalizes_records() {
        // Patchset for `t(a pk,b,c)`: UPDATE set b=20, then a DELETE.
        let update = "50030100007400170001000000000000000101000000000000001400";
        let bytes = from_hex(update);
        let tables = parse_changeset(&bytes).unwrap();
        assert_eq!(tables.len(), 1);
        let t = &tables[0];
        assert_eq!(t.ncol, 3);
        assert_eq!(t.pk_flags, alloc::vec![1, 0, 0]);
        let c = &t.changes[0];
        assert_eq!(c.op, ChangeOp::Update);
        // new: PK present, changed col present, unchanged col omitted.
        assert_eq!(
            c.new,
            alloc::vec![Some(Value::Integer(1)), Some(Value::Integer(20)), None]
        );
        // old: only the PK slot is filled (matches the row by PK on apply).
        assert_eq!(c.old, alloc::vec![Some(Value::Integer(1)), None, None]);

        let del = "500301000074000900010000000000000001";
        let tables = parse_changeset(&from_hex(del)).unwrap();
        let c = &tables[0].changes[0];
        assert_eq!(c.op, ChangeOp::Delete);
        assert_eq!(c.old, alloc::vec![Some(Value::Integer(1)), None, None]);
    }

    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
