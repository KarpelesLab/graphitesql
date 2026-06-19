# graphitesql roadmap

This document is the plan for building **graphitesql**: a single-crate, pure,
safe, `no_std` Rust implementation of SQLite with byte-for-byte compatibility
with the SQLite 3 file format.

It is meant to be read top-to-bottom once, then used as a checklist. Each phase
lists its **deliverable**, its **done criterion**, and the **upstream SQLite
files** that serve as the specification for that phase (fetch them with
`reference/fetch.sh`).

---

## 1. Architecture

SQLite has a famously clean layered design. We mirror it, because the layering is
what makes the file format and the SQL semantics tractable to re-implement
independently. Data flows top-to-bottom on writes and bottom-to-top on reads:

```
            ┌──────────────────────────────────────────────┐
  SQL text  │  api          Connection / Statement / Row    │  public API
            ├──────────────────────────────────────────────┤
            │  sql::token   tokenizer                        │
            │  sql::parser  parser  ──►  sql::ast            │  front end
            ├──────────────────────────────────────────────┤
            │  planner      query planning (join/index)      │
            │  codegen      AST  ──►  VDBE bytecode          │  compiler
            ├──────────────────────────────────────────────┤
            │  vdbe         register virtual machine         │  execution
            │  func collate built-in functions, collations   │
            ├──────────────────────────────────────────────┤
            │  btree        table & index B-trees, cursors   │  data model
            ├──────────────────────────────────────────────┤
            │  pager        page cache, transactions,        │  storage
            │               rollback journal, WAL, locking   │
            ├──────────────────────────────────────────────┤
            │  format       on-disk byte layout (the spec)   │  format
            ├──────────────────────────────────────────────┤
            │  vfs          Vfs / File traits (mem, std, …)  │  OS boundary
            └──────────────────────────────────────────────┘
```

### Module ↔ upstream map

graphitesql is one crate; these are its modules and the SQLite files that
specify each. Reading the C file is the fastest way to get a phase right.

| graphitesql module | responsibility | upstream reference |
|--------------------|----------------|--------------------|
| `vfs`              | OS abstraction: open/read/write/sync/lock | `os_unix.c`, `os_win.c`, `os.c` |
| `format`           | byte layout of header, pages, cells, records, freelist | `fileformat2.html`, `btree.c` (comments), `btreeInt.h` |
| `pager`            | page cache, atomic commit, journal, WAL, locking | `pager.c`, `wal.c`, `pcache.c`, `pcache1.c` |
| `btree`            | table/index B-trees, cursors, balancing | `btree.c`, `btreeInt.h` |
| `value` / record   | storage classes, serial types, affinity | `vdbemem.c`, `vdbeaux.c` |
| `sql::token`       | tokenizer | `tokenize.c`, `keywordhash.h` |
| `sql::parser`/`ast`| grammar → parse tree | `parse.y`, `expr.c`, `resolve.c` |
| `codegen`          | parse tree → bytecode | `build.c`, `insert.c`, `update.c`, `delete.c`, `select.c` |
| `planner`          | join order, index selection, query flattening | `where.c`, `wherecode.c`, `whereexpr.c`, `analyze.c` |
| `vdbe`             | the register VM and its opcodes | `vdbe.c`, `vdbeaux.c`, `vdbeapi.c`, `opcodes.h` |
| `func` / `collate` | scalar/aggregate funcs, collations | `func.c`, `date.c`, `callback.c` |
| `schema`           | parse `sqlite_schema`, build the catalog | `build.c`, `prepare.c` |
| `api`              | `Connection`/`Statement` and (later) C-API shim | `main.c`, `vdbeapi.c`, `legacy.c` |

### Why a VDBE (and not a tree-walker)

The VDBE bytecode is **not** stored in the database file, so file compatibility
does not require it. We adopt it anyway because:

1. SQLite's observable semantics (evaluation order, type coercion, `NULL`
   handling, the exact rows a query returns) are defined operationally by what
   the VDBE does. Matching the model is the surest path to matching behavior.
2. It cleanly separates "what to compute" (codegen/planner) from "how to run it"
   (vdbe), which keeps each piece independently testable.
3. `EXPLAIN` output becomes directly comparable to SQLite's for differential
   testing.

We will not be bug-for-bug identical in bytecode, but we will be result-identical.

---

## 2. Design principles

- **`#![forbid(unsafe_code)]`, no exceptions.** Enforced in `Cargo.toml` lints.
  If a hot path seems to need `unsafe`, redesign or accept the safe cost.
- **`no_std` + `alloc` is the baseline.** `std` is an additive feature (real
  files, `std::error::Error`). Nothing core may depend on `std`.
- **Zero dependencies.** No crates in the default build. Optional dev/test-only
  dependencies (e.g. for property testing) are acceptable behind `cfg(test)`.
- **The VFS is the only I/O boundary.** All file access goes through the `Vfs`
  and `File` traits. This is what makes `:memory:`, std files, and wasm uniform.
- **Compatibility is verified, not assumed.** Every format phase ends with a
  differential test against the real `sqlite3` library/CLI (see §4).
- **Fail loud while young.** Unimplemented paths return `Error::Unsupported`
  rather than silently producing wrong results.
- **Document with the spec.** Each format module carries the relevant
  file-format table in its doc comment, so the code and the spec live together.

---

## 3. Phases

Phases are ordered so that each builds only on completed lower layers, and so
that something useful and testable exists early (read-only access to real
databases lands well before write support).

### Phase 0 — Foundation ✅ *(done)*

- **Deliverable:** crate scaffold; varints; value/serial-type model; database
  header parse/serialize; error type; CI-ready `cargo test`/`clippy`.
- **Done:** `no_std` builds; header round-trips a real `sqlite3` file
  byte-for-byte; all unit tests green.
- **Files:** `src/util/varint.rs`, `src/value.rs`, `src/format/header.rs`,
  `src/error.rs`.

### Phase 1 — VFS & raw paging *(read side)* ✅ *(done)*

- **Deliverable:** `Vfs`/`File` traits; in-memory VFS; std-file VFS (feature
  `std`); a read-only pager that maps page numbers → byte slices and validates
  page size against the header.
- **Done:** opens the committed real-SQLite fixtures (`tests/fixtures/*.db`)
  through the std VFS + pager, re-derives the header, and reads every page;
  page 1's 100-byte header offset is handled via `Page::body_offset`.
- **Files:** `src/vfs/{mod,memory,std_file}.rs`, `src/pager/mod.rs`,
  `tests/read_fixtures.rs`.
- **Reference:** `os_unix.c` (the VFS contract), `pager.c` (page lifecycle).

### Phase 2 — B-tree reader ✅ *(done)*

- **Deliverable:** parse table & index, interior & leaf b-tree pages; cell
  parsing including overflow-page chains; a forward/seek cursor over a table
  b-tree keyed by rowid, and over an index b-tree keyed by record.
- **Done:** table scan of `nums` (2000 rows, interior pages) sums correctly;
  20 KB blob row reassembles across overflow pages; rowid `seek` (exact / past-
  end / before-first) works; index scan yields every entry. All verified
  against real fixtures. *(Index seek-by-key awaits record comparison in
  Phase 3.)*
- **Files:** `src/btree/{mod,page,cursor}.rs`; tests in `tests/read_fixtures.rs`.
- **Reference:** `btree.c`, `btreeInt.h`; `fileformat2.html` (B-tree pages,
  cell format, overflow).

### Phase 3 — Record decoding & the schema catalog ✅ *(done)*

- **Deliverable:** decode records (header of serial types + bodies) into
  `Value`s; read and parse `sqlite_schema` (table 1) into an in-memory catalog
  of tables/indexes with their root pages and SQL.
- **Done:** full record decode incl. all integer widths (sign-extended),
  REAL, TEXT (UTF-8/16), BLOB; `Schema::read` enumerates `sqlite_schema`,
  resolves a table name → root page; end-to-end test decodes every row of
  `basic.db` to typed values (incl. INTEGER-PRIMARY-KEY → NULL aliasing).
- **Files:** `src/format/record.rs`, `src/schema/mod.rs`.
- **Reference:** `fileformat2.html` (record format, schema table), `build.c`.

### Phase 4 — SQL front end (tokenizer + parser + AST) ✅ *(done)*

- **Deliverable:** tokenizer matching SQLite's lexical rules and keyword set; a
  recursive-descent parser producing an AST for the core grammar: `SELECT`
  (with `WHERE`/`GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`/joins), `INSERT`,
  `UPDATE`, `DELETE`, `CREATE TABLE/INDEX`, `DROP`, transactions, and `PRAGMA`.
- **Done:** tokenizer covers strings/blobs/quoted-idents/comments/params/numbers
  (UTF-8 safe); Pratt expression parser with SQLite precedence incl.
  `IS [NOT] NULL`, `IN`, `BETWEEN`, `LIKE`/`GLOB`, `CASE`, `CAST`; reserved-word
  checks reject `SELECT FROM`; ~20 parser/tokenizer tests. *(Subqueries & CTEs
  deferred to Phase 9.)*
- **Files:** `src/sql/{mod,token,ast,parser}.rs`.
- **Reference:** `tokenize.c`, `parse.y`, `expr.c`. (We hand-write the parser
  rather than port the Lemon grammar, but `parse.y` is the source of truth for
  precedence and accepted forms.)

### Phase 5 — Read-query execution engine ✅ *(done)*

- **Deliverable:** a `Connection` that parses, resolves names against the schema,
  scans b-trees, decodes records, and evaluates expressions to produce result
  rows; `NULL` three-valued logic, SQLite comparison order & numeric coercion,
  `LIKE`/`GLOB`, `CASE`/`CAST`, a core of scalar functions, and the aggregates
  `count`/`sum`/`avg`/`min`/`max`/`total`/`group_concat` with `GROUP BY`/`HAVING`,
  plus `WHERE`, `ORDER BY` (by position/alias/expr), `LIMIT`/`OFFSET`, `DISTINCT`.
- **Done:** `Connection::open(path).query(sql)` returns correct rows on the real
  fixtures, verified differentially against `sqlite3` (e.g. `GROUP BY id%3`
  counts 666/667/667). INTEGER-PRIMARY-KEY rowid aliasing handled.
- **Files:** `src/exec/{mod,eval,func}.rs`, `src/util/float.rs`,
  `tests/query.rs`.
- **Implementation note — executor vs. bytecode:** the roadmap originally
  specified a VDBE bytecode VM. graphitesql instead ships an **operational,
  iterator-style executor** with the *same observable semantics*. This was the
  pragmatic path to a correct, testable read engine; adopting a VDBE bytecode IR
  is now an internal refactor (it changes how queries are represented, not their
  results) and is tracked for a later pass. *(Single-table only; joins/subqueries
  in Phase 9.)*
- **Reference:** `vdbe.c`, `select.c`, `where.c`, `func.c`.

### Phase 6 — B-tree writer + pager transactions *(write side)* ✅ *(core done)*

- **Deliverable:** insert cells with page splitting; the rollback journal with
  correct fsync ordering and crash-safe atomic commit; create-from-scratch.
- **Done:** `WritePager` buffers all mutations in an overlay and commits
  atomically through a hash-verified rollback journal (originals → sync → write
  → sync → clear); journal **recovery on open** replays an interrupted commit;
  `ROLLBACK` = drop overlay. The b-tree writer inserts rows (with overflow-page
  allocation) and splits leaves/interiors bottom-up, growing a new root in place.
  **Cross-engine gate met:** a database built by graphitesql (incl. 200- and
  1500-row tables) is opened by the real `sqlite3` CLI and
  `PRAGMA integrity_check` returns `ok`; values round-trip through SQLite's
  decoder (`tests/write_compat.rs`).
- **Files:** `src/pager/write.rs`, `src/btree/writer.rs`, `src/format/record.rs`
  (`encode_record`), `tests/write_compat.rs`.
- **Carried to Phase 7/9:** `DELETE`/`UPDATE` at the b-tree level (need freelist
  page reclamation to keep `integrity_check` happy), freelist management, page
  merging/rebalancing on delete, and the SQLite-format journal (ours is a
  private, recoverable format). Locking is single-connection (no-op).
- **Reference:** `btree.c` (balance), `pager.c` (journal, commit),
  `fileformat2.html` (freelist, rollback journal format).

### Phase 7 — DDL & DML through the `Connection` API ✅ *(core done)*

- **Deliverable:** a writable `Connection` (`execute`/`query`, file & `:memory:`)
  running `CREATE TABLE`, `INSERT`, `UPDATE`, `DELETE`, and `BEGIN`/`COMMIT`/
  `ROLLBACK`; schema mutation writes `sqlite_schema` and bumps the schema cookie;
  `INTEGER PRIMARY KEY` rowid aliasing; `DEFAULT` values; autocommit + explicit
  transactions with read-your-writes.
- **Done:** in-memory and on-disk CREATE/INSERT/UPDATE/DELETE round-trip;
  data persists across reopen; a database built entirely through graphitesql SQL
  (`CREATE TABLE` + 300 parameterized `INSERT`s) is opened by real `sqlite3` with
  `PRAGMA integrity_check = ok` (`tests/dml.rs`).
- **Files:** `src/exec/mod.rs` (`Connection::{open,open_readonly,create,
  open_memory,execute,execute_params}`), `src/btree/writer.rs` (`delete_table`),
  `src/btree/cursor.rs` (`TableCursor::last`).
- **Carried to Phase 9:** `CREATE INDEX` + index maintenance on write, `ALTER`,
  `DROP`, `WITHOUT ROWID`, `UNIQUE`/`CHECK`/`NOT NULL` enforcement,
  `INSERT … SELECT`, and freelist-backed reclamation for overflow-row delete.
- **Reference:** `build.c`, `insert.c`, `update.c`, `delete.c`, `alter.c`.

### Phase 8 — WAL mode ✅ *(read done)*

- **Deliverable:** parse and overlay the `-wal` file so WAL-mode databases read
  correctly, with full frame validation.
- **Done:** `WalReader` parses the WAL header and frames, validates the
  **exact SQLite checksum** (Fibonacci-style, endianness from the magic's LSB)
  and salts, honors frames up to the last valid commit, and overlays them on the
  main file as a `PageSource`. `Connection::open_readonly` auto-detects `-wal`.
  Verified by reading a real WAL-mode database whose table and rows exist
  **only** in an uncheckpointed `-wal` produced by SQLite (`tests/wal.rs`,
  `tests/fixtures/wal.db{,-wal}`).
- **Files:** `src/pager/wal.rs`; `Connection` backend enum in `src/exec/mod.rs`.
- **Carried to Phase 9:** *writing* WAL (appending frames + `-shm` wal-index),
  checkpointing, the WAL locking protocol, and `PRAGMA journal_mode=wal`.
- **Reference:** `wal.c`, `fileformat2.html` (the WAL format).

### Phase 9 — Compatibility hardening & breadth 🔄 *(ongoing track)*

This is the open-ended breadth phase — it grows continuously toward full SQLite
coverage rather than having a single "done".

- **Landed so far:**
  - multi-table **`INNER`/`LEFT`/cross/comma joins** (nested-loop, with `ON`,
    aliases, aggregation over joins) — `tests/joins.rs`;
  - **freelist reclamation** — `free_page`/`allocate_page` reuse pages;
    overflow-row `DELETE` returns pages to the freelist (`tests/write_compat.rs`);
  - **`CREATE INDEX` + index maintenance** — index b-trees (record keys, splits,
    overflow); incremental insert on `INSERT`, in-place rebuild on
    `DELETE`/`UPDATE`; verified by sqlite3 `integrity_check` — `tests/indexes.rs`;
  - **index-driven query planning** — single-table `WHERE col = const` (and
    composite leftmost prefixes) use an index prefix-seek + rowid fetch instead
    of a full scan; the differential suite runs with indexes present so the path
    is exercised across the whole corpus (issue #4);
  - **`DROP TABLE`/`DROP INDEX`** (frees b-trees, removes schema rows, drops a
    table's dependent indexes);
  - **`ALTER TABLE`** `ADD COLUMN` (with `DEFAULT` applied to existing rows on
    read), `RENAME TO` (repointing dependent indexes), and `RENAME COLUMN`
    (updating dependent index definitions) — `tests/alter.rs`;
  - a **differential test harness** (`tests/differential.rs`) that runs 1500+
    generated `SELECT`s through both graphitesql and the real `sqlite3` and
    asserts identical output — currently 1640/1640 match;
  - **`CREATE VIEW` / `DROP VIEW`** and querying views (the view's `SELECT` runs
    as the source) — `tests/views.rs`;
  - **constraint enforcement** — `NOT NULL`, `CHECK` (column + table), and
    `UNIQUE`/`PRIMARY KEY` on `INSERT`/`UPDATE`, with `INSERT OR IGNORE`/
    `OR REPLACE` (and `REPLACE INTO`);
  - **subqueries** — scalar `(SELECT …)` and `expr [NOT] IN (SELECT …)`
    (uncorrelated) — `tests/subquery.rs`;
  - non-recursive **`WITH` / CTEs** used as a query source — `tests/cte.rs`;
  - **compound queries** `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT` with an
    overall `ORDER BY`/`LIMIT`;
  - **type affinity** (comparison + storage) and `substr()` window semantics;
  - the full **`CREATE TABLE` constraint grammar** parses (`CHECK`,
    `REFERENCES`, `FOREIGN KEY`, named `CONSTRAINT`, conflict clauses) so
    real-world schemas load — `tests/realworld.rs`;
  - `VACUUM` accepted; more scalar functions (`concat`, `sign`, `zeroblob`,
    `quote`, `unhex`, …);
  - an AST→SQL printer (`sql::print`) used to regenerate stored `CREATE` text;
  - queryable **`PRAGMA`s** (`table_info`, `page_size`, …) and the `graphitesql`
    **CLI shell**.
  - **date/time functions** (`date`, `time`, `datetime`, `julianday`,
    `unixepoch`, `strftime`) and **`printf`/`format`** — a dependency-free port
    of `date.c`'s Julian-day engine, verified differentially (`tests/datetime.rs`);
  - **`EXPLAIN QUERY PLAN`** (SCAN/SEARCH plan rows in SQLite's detail format)
    plus a **rowid equality fast-path** that seeks the table b-tree directly
    (`tests/explain.rs`);
  - **recursive `WITH RECURSIVE` CTEs** (anchor + fixed-point recursive term,
    `UNION`/`UNION ALL`), CTEs referencing earlier CTEs, and CTEs as a join
    source, via a materialized CTE environment (`tests/cte.rs`);
  - **correlated subqueries** (scalar, `IN (SELECT …)`) and `[NOT] EXISTS` — the
    subquery resolves outer columns through an outer-scope frame stack
    (`tests/subquery.rs`);
  - **window functions** — `row_number`/`rank`/`dense_rank`/`ntile`,
    `lag`/`lead`/`first_value`/`last_value`/`nth_value`, and aggregate windows
    over `PARTITION BY`/`ORDER BY` with the default frame (`tests/window.rs`);
  - **`%!.15g` real formatting** matching SQLite's double-to-text exactly;
  - **foreign-key enforcement** behind `PRAGMA foreign_keys` — child-side
    existence checks plus parent-side referential actions (NO ACTION/RESTRICT,
    CASCADE, SET NULL, SET DEFAULT) on DELETE/UPDATE (`tests/foreign_keys.rs`);
  - **row triggers** — `CREATE TRIGGER`/`DROP TRIGGER` for
    `BEFORE`/`AFTER` `INSERT`/`UPDATE`/`DELETE`, with `OLD`/`NEW` references and a
    `WHEN` guard, fired non-recursively (`tests/triggers.rs`);
  - **views & CTEs as join sources** — either side of a `JOIN` (`tests/views.rs`);
  - **derived tables** — `FROM (SELECT …) [AS] alias` as a sole source or join
    operand (`tests/subquery.rs`);
  - **explicit window frames** — `ROWS`/`RANGE`/`GROUPS BETWEEN … AND …` with the
    full bound set (`tests/window.rs`);
  - **`PRAGMA recursive_triggers`** — recursive trigger firing (bounded), off by
    default (`tests/triggers.rs`);
  - **automatic indexes for `UNIQUE`/non-rowid `PRIMARY KEY`** — the implicit
    `sqlite_autoindex_*` b-trees are now written and maintained, so SQLite
    accepts the files (`tests/indexes.rs`).
- **Deliverable (remaining):** the storage-engine frontier — `WITHOUT ROWID`
  tables; the WAL *write* path (frame append + `-shm` wal-index + checkpoint);
  real `VACUUM` compaction; b-tree page merging/rebalance on delete; plus
  `INSTEAD OF` (view) triggers and writable views.
  (we enforce by scan, not via an index b-tree yet); plain `EXPLAIN` (VDBE
  bytecode); full type-affinity & collation edge cases; WAL *write* path; b-tree
  page merging.
- **Goal:** pass a curated slice of SQLite's own test assertions and a large
  differential corpus.
- **Reference:** `fkey.c`, `trigger.c`, `window.c`, `date.c`, `pragma.c`,
  `vacuum.c`, `where.c`, plus SQLite's `test/` TCL suite as an oracle.

### Phase 10 — Ecosystem & extensions *(post-1.0, behind features)*

- C-API shim crate (`libsqlite3`-compatible surface), virtual tables, FTS5,
  R-Tree, `sqlite3_session`, user-defined functions from Rust, an async VFS for
  wasm. Each is opt-in and out of the core compatibility promise.

---

## 4. File-format compatibility strategy

This is the project's whole reason to exist, so it gets first-class testing.

- **Golden-file tests.** Check small, deterministic databases produced by a
  pinned `sqlite3` into the test suite (as byte arrays or fixtures) and assert
  graphitesql parses them exactly. Phase 0 already does this for the header.
- **Round-trip tests.** Parse → re-serialize → assert identical bytes for every
  format structure (header done; pages, cells, records, journal, WAL to come).
- **Differential tests.** For each phase from 5 on, run the same SQL through
  both `sqlite3` (the C library) and graphitesql and diff the results, and diff
  the resulting database files where determinism allows.
- **`integrity_check` as a gate.** Any database graphitesql writes must pass
  `sqlite3`'s `PRAGMA integrity_check` and `PRAGMA foreign_key_check`.
- **Fuzzing.** Fuzz the readers with malformed pages (must return
  `Error::Corrupt`, never panic) and fuzz SQL parsing.
- **Crash-recovery tests.** A VFS that can inject failures/truncation at chosen
  fsync points, asserting recovery to a consistent state.

### Known sources of legitimate file divergence

Two SQLite-compatible writers can produce different bytes for the same logical
content. We document and accept these rather than chase them:

- free-page reuse order and exact balancing splits,
- `change_counter` / `version_valid_for` values,
- the embedded `SQLITE_VERSION_NUMBER` of the writer,
- unused/reserved bytes left from deletions.

Compatibility means *both engines can read each other's files and agree on
contents*, not that bytes are identical for independently-built databases.

---

## 5. Open decisions

These are deliberately unresolved; we'll settle them with input rather than by
default. Tracked here so they don't get lost.

1. ~~**License.**~~ *Resolved:* **public domain**, mirroring SQLite, with the
   SQLite blessing in place of a legal notice (SPDX `blessing`). See `LICENSE`.
2. **Parser: hand-written vs. generated.** Plan is a hand-written
   recursive-descent parser (no build-time codegen, friendlier errors). The
   alternative is porting the Lemon grammar. → *Leaning hand-written.*
3. **Concurrency model.** Single-connection first. Whether to support multiple
   connections / threads sharing a pager (and how locking maps onto the `Vfs`
   trait) is a Phase 6+ question. → *Deferred.*
4. **Big integers / decimal.** SQLite stores reals as f64; we match that. No
   extended numeric types planned.
5. **MSRV policy.** Pinned at 1.95 today; revisit before 1.0.

---

## 6. Immediate next steps (Phase 1 kickoff)

1. Define `Vfs` and `File` traits in `src/vfs/mod.rs` (open flags, read-at,
   write-at, truncate, sync, file size, lock/unlock).
2. Implement `src/vfs/memory.rs` (a `Vec<u8>`-backed file) — works on wasm and
   `no_std`.
3. Implement `src/vfs/std_file.rs` behind `feature = "std"`.
4. Implement a read-only `Pager` that, given a `File`, validates the header and
   hands out page byte-slices, accounting for page 1's 100-byte header offset.
5. Add the first end-to-end test: open `gtest.db` through the std VFS + pager and
   reconstruct the header — the same `DatabaseHeader` Phase 0 parses directly.
