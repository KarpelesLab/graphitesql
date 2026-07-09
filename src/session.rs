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
    /// `true` between [`Session::attach`] (all tables) and the session being
    /// dropped/disabled. When `false` the write hook is a no-op.
    pub(crate) enabled: bool,
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
    /// (Per-table attach is not yet exposed; this attaches all tables.)
    pub fn attach(&self) {
        self.state.borrow_mut().enabled = true;
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
    mut read_row: impl FnMut(&str, &[Value]) -> Option<Vec<Value>>,
) -> Vec<u8> {
    let mut out = Vec::new();
    for tbl in &state.tables {
        if tbl.is_empty() {
            continue;
        }
        // Table header: 'T', ncol (varint), pk-flag bytes, NUL-terminated name.
        let hdr_start = out.len();
        out.push(b'T');
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
                        if append_update(&mut out, &change.old, &row, &tbl.pk_flags) {
                            wrote_any = true;
                        }
                    }
                    (ChangeOp::Update | ChangeOp::Delete, None) => {
                        append_delete(&mut out, &change.old);
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

/// Append a DELETE change: op byte, indirect byte, then the full old record.
fn append_delete(out: &mut Vec<u8>, old: &[Value]) {
    out.push(OP_DELETE);
    out.push(0);
    for v in old {
        append_value(out, v);
    }
}

/// Append an UPDATE change: op byte, indirect byte, old record (PK columns and
/// changed columns present, unchanged non-PK columns as `OMIT`), then new
/// record (changed columns present, unchanged as `OMIT`). Returns `false`
/// (writing nothing) if no non-PK column changed — matching SQLite, which
/// rewinds the buffer for a no-op update.
fn append_update(out: &mut Vec<u8>, old: &[Value], new: &[Value], pk_flags: &[u8]) -> bool {
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
        // old.* record: present if changed or PK, else OMIT.
        if changed || is_pk {
            append_value(out, &old[i]);
        } else {
            out.push(T_OMIT);
        }
        // new.* record: present if changed, else OMIT.
        if changed {
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
}

fn corrupt(msg: &str) -> crate::error::Error {
    crate::error::Error::Corrupt(alloc::format!("changeset: {msg}"))
}

/// Parse a changeset blob into per-table change groups. Supports the format
/// [`serialize`] produces (`'T'` table headers followed by `INSERT`/`UPDATE`/
/// `DELETE` change records). An empty blob yields an empty vector.
pub(crate) fn parse_changeset(data: &[u8]) -> Result<Vec<TableChangeset>> {
    let mut r = Reader::new(data);
    let mut tables: Vec<TableChangeset> = Vec::new();
    while !r.eof() {
        let marker = r.peek()?;
        match marker {
            b'T' => {
                r.u8()?; // consume 'T'
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
                let op = r.u8()?;
                let _indirect = r.u8()?;
                let rec = match op {
                    OP_INSERT => ChangeRecord {
                        op: ChangeOp::Insert,
                        old: Vec::new(),
                        new: r.record(ncol)?,
                    },
                    OP_DELETE => ChangeRecord {
                        op: ChangeOp::Delete,
                        old: r.record(ncol)?,
                        new: Vec::new(),
                    },
                    _ => {
                        // UPDATE: old record then new record.
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
}
