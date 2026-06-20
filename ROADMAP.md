# graphitesql roadmap

This document is the plan for **graphitesql**: a single-crate, pure, safe,
`no_std` Rust implementation of SQLite with byte-for-byte compatibility with the
SQLite 3 file format.

The foundation (the file format, the storage/btree/pager stack, and a broad SQL
engine) is **done** — see §3 for a capability summary. The rest of this document
is the forward plan for closing the remaining gap with SQLite: §4 the work
tracks, §5 the cross-cutting concerns, §6 the testing strategy.

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
            │  exec         iterator executor (+ future VDBE)│  execution
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

| graphitesql module | responsibility | upstream reference |
|--------------------|----------------|--------------------|
| `vfs`              | OS abstraction: open/read/write/sync/lock | `os_unix.c`, `os.c` |
| `format`           | byte layout of header, pages, cells, records, freelist | `fileformat2.html`, `btreeInt.h` |
| `pager`            | page cache, atomic commit, journal, WAL, locking | `pager.c`, `wal.c`, `pcache.c` |
| `btree`            | table/index B-trees, cursors, balancing | `btree.c`, `btreeInt.h` |
| `value` / `record` | storage classes, serial types, affinity | `vdbemem.c`, `vdbeaux.c` |
| `sql::token`       | tokenizer | `tokenize.c`, `keywordhash.h` |
| `sql::parser`/`ast`| grammar → parse tree | `parse.y`, `expr.c`, `resolve.c` |
| `exec`             | name resolution, execution, DDL/DML, triggers, functions | `select.c`, `where.c`, `insert.c`, `vdbe.c` |
| `planner` *(in `exec`)* | index selection, join order (future: cost-based) | `where.c`, `analyze.c` |
| `func` / `collate` | scalar/aggregate funcs, collations | `func.c`, `date.c`, `callback.c` |
| `schema`           | parse `sqlite_schema`, build the catalog | `build.c`, `prepare.c` |
| `api`              | `Connection`/`Statement` and (later) C-API shim | `main.c`, `vdbeapi.c` |

**Executor vs. bytecode.** The engine today is an *operational, iterator-style
executor* with the same observable semantics as SQLite, not a VDBE bytecode VM.
That was the pragmatic path to a correct, testable engine. Adopting a VDBE IR is
now an internal refactor (it changes how queries are represented, not their
results) and is scheduled in Track B — it unblocks real `EXPLAIN` output and a
cost-based planner.

---

## 2. Design principles

- **`#![forbid(unsafe_code)]`, no exceptions.** Enforced in `Cargo.toml` lints.
- **`no_std` + `alloc` is the baseline.** `std` is an additive feature (real
  files, `std::error::Error`). Nothing core may depend on `std`.
- **Near-zero dependencies.** No crates in the default build. The one sanctioned
  exception is the in-house `timezone-data` crate, behind an opt-in feature, for
  `localtime`/`utc` date modifiers. Optional dev/test deps behind `cfg(test)` are
  fine.
- **The VFS is the only I/O boundary.** All file access goes through the `Vfs`
  and `File` traits — what makes `:memory:`, std files, and wasm uniform.
- **Compatibility is verified, not assumed.** Every feature lands with a
  differential test against the real `sqlite3` CLI, and anything we write must
  pass `PRAGMA integrity_check` (see §6).
- **Fail loud while young.** Unimplemented paths return `Error::Unsupported`
  rather than silently producing wrong results.

---

## 3. Foundation ✅ *(done)*

The layered foundation and a broad SQL engine are complete and differentially
verified against `sqlite3` (a 1,600+ query corpus plus ~80 focused test suites).
Detailed history lives in `CHANGELOG.md`; in summary, graphitesql today:

**Reads & writes real SQLite files.** Opens `sqlite3`-written databases
(including WAL-mode) and **creates** databases whose files `sqlite3` opens with
`PRAGMA integrity_check = ok`. Storage covers rowid and **`WITHOUT ROWID`**
tables, automatic/secondary/`UNIQUE` indexes (incl. `sqlite_autoindex_*`),
overflow pages, the freelist with **page merging on delete**, real **`VACUUM`**,
and the **WAL read *and* write** path (`journal_mode=WAL`, `wal_checkpoint`).

**Runs a broad SQL dialect.** `SELECT` with `WHERE`/`GROUP BY`/`HAVING` (incl.
without `GROUP BY`)/`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT` and SELECT-list
aliases resolved in WHERE/GROUP BY/HAVING; `INNER`/`LEFT`/`RIGHT`/`FULL`/cross/
comma **joins** plus **`NATURAL`** and **`USING`** (with column coalescing),
nested-loop + a hash join for equi-joins; compound queries
(`UNION`/`INTERSECT`/`EXCEPT`, collation-aware); (recursive) **CTEs** with
`LIMIT`; correlated subqueries, `[NOT] EXISTS`, derived tables; views & CTEs as
sources; **window functions** (`ROWS`/`RANGE`/`GROUPS`, `EXCLUDE`, `FILTER`,
named windows); `INSERT … SELECT`, `UPDATE … FROM`, `UPDATE OR
IGNORE/REPLACE/…`, UPSERT, `RETURNING`, row values, `STRICT` tables, generated
columns; a broad scalar/aggregate function library incl. **date/time**
(+ `CURRENT_DATE`/`TIME`/`TIMESTAMP`), `printf`/`format`, **JSON** (`json_*`,
`json_group_array`/`object`, `json_pretty`, `json_each`/`json_tree`), `iif`/`if`,
`sqlite_version`, math (pure-`core`); **type affinity** and SQLite-exact real
formatting; collation (`BINARY`/`NOCASE`/`RTRIM`) honored across comparisons,
`IN`/`BETWEEN`/`CASE`, `min`/`max`, set ops,
`ORDER BY`/`GROUP BY`/`DISTINCT`/`UNIQUE`/index keys; `EXPLAIN QUERY PLAN` with
an index-driven planner (equality/range/`IN`/OR-union seeks, **index-driven
`ORDER BY`** with **covering-index reads**, stats-driven choice via
`ANALYZE`/`sqlite_stat1`); constraint enforcement (`NOT NULL`, `CHECK`,
`UNIQUE`/`PK`, **foreign keys** with all actions, standalone/partial/expression
UNIQUE indexes); **triggers** (`BEFORE`/`AFTER`/`INSTEAD OF`, `UPDATE OF`,
`WHEN`, recursive, `NEW`/`OLD` incl. rowid); `SAVEPOINT`/`RELEASE`/`ROLLBACK TO`;
**`ATTACH`/`DETACH`/`TEMP`** multi-schema (cross-database reads, writes, joins,
qualified DDL, view reads, transactions & savepoints); DDL with full CREATE-time
and ALTER validation; the schema catalog queryable as
`sqlite_schema`/`sqlite_master`, with `table_list`/`collation_list`/`database_list`
and bare `pragma_*` table-valued functions.

What remains is breadth and depth toward full SQLite parity, below.

---

## 4. Forward plan — closing the gap with SQLite

Four tracks. Completed work is summarized; **remaining work is broken into
numbered, independently-shippable pieces** (each one lands with a differential
test and keeps `master` green). Tracks can progress in parallel.

### Track A — SQL language & functions breadth  *(substantially complete)*

Done: outer joins + `NATURAL`/`USING` (with coalescing), generated columns,
collations, UPSERT, `UPDATE OR IGNORE/REPLACE/…`, `RETURNING`, row values,
`ORDER BY` modifiers (`NULLS FIRST/LAST`, `IS [NOT] DISTINCT FROM`), `STRICT`
tables, `CREATE TABLE … AS SELECT`, `INSERT … SELECT`, `UPDATE … SET … FROM`,
`*`/`table.*` with aggregates, HAVING without GROUP BY, SELECT-list aliases in
WHERE/GROUP BY/HAVING, `CURRENT_DATE`/`TIME`/`TIMESTAMP`, `iif`/`if`,
`sqlite_version`, `json_pretty`, the window-function suite, the math + JSON +
`printf` libraries, partial/expression/UNIQUE-index breadth, and full DDL
validation.

**Remaining pieces** (small, each one function/clause-scoped):

- **A1 — `random()` / `randomblob()`.** *(Intentionally deferred: non-
  deterministic, untestable differentially without an RNG; revisit if a seedable
  RNG lands. `zeroblob` already works.)*
- **A2 — DESC index columns honored in seeks.** A `DESC` index gives correct
  results today by scan/superset; teach the seek paths
  (`try_index_lookup`/`try_index_range`, `index_range_rowids`) to walk a `DESC`
  b-tree in the right direction. *Perf-only; acceptance: `EXPLAIN QUERY PLAN`
  shows the seek and results are unchanged.*
- **A3 — partial/expression index use in the planner.** The planner scans for
  these today; let the index chooser (`try_index_*` + `eqp_access` in lockstep)
  use a *partial* index when the query's `WHERE` implies its predicate, and an
  *expression* index when a `WHERE`/`ORDER BY` term matches the indexed
  expression. Acceptance: `USING INDEX` in EQP + unchanged results.
- **A4 — literal-left collation fallback.** A literal compared to a collated
  column/subquery (`'x' = nocase_col`, `'x' IN (SELECT nocase_col)`) should adopt
  the *right* side's collation when the left has none; also window-frame
  `min`/`max` collation. *Ref:* `resolve.c` collation-of rules.
- **A5 — `timediff(A,B)`.** Returns the calendar delta `±YYYY-MM-DD HH:MM:SS.SSS`.
  Needs SQLite's iterative breakdown: add Y years to B, back off if it overshoots
  A, then months, then take the residual via Julian-day difference for
  `D HH:MM:SS.SSS`. *Ref:* `date.c` `timediffFunc`. Acceptance: byte-match
  sqlite3 across sign, leap years, and month-length boundaries.
- **A6 — `json_error_position(X)`.** Returns the 1-based byte position of the
  first JSON parse error (0 if valid). Thread the failing offset out of
  `exec::json::parse` (currently it returns `Option`, discarding position) and
  match sqlite3's reported positions.
- **A7 — multi-statement `execute_batch(sql)`.** A new API that runs a
  `;`-separated script (like `sqlite3_exec`). Needs a statement splitter aware
  that trigger bodies (`BEGIN … END`) and `CASE … END` contain inner `;` — track
  `BEGIN`/`CASE`/`END` depth over the token stream, then run each slice through
  `execute_params` so per-statement CREATE text is preserved. *(`execute()`
  stays single-statement.)*

### Track B — Query planner, statistics & the VDBE

Done: `ANALYZE` + `sqlite_stat1` (byte-compatible) with stats-driven index
choice; range/`IN`/OR-union seeks; a hash join for equi-joins; **B0
index-driven `ORDER BY`** (rowid/IPK *and* secondary-index cases, ASC+DESC, in
lockstep across `scan_source`/`run_core`/`eqp_access`); **B2 covering-index
reads** for that ordered scan (rows built from index records, EQP reports
`USING COVERING INDEX`); and the VDBE spike (`exec::vdbe`) covering constant
projections, single-table scan + `WHERE` + `ORDER BY` + `DISTINCT` +
`LIMIT`/`OFFSET`, whole-table aggregates, and single-table `GROUP BY`, all
matching the tree-walker via `query_vdbe`.

**Remaining pieces.** The optimizer ones are *perf-only* (results already
correct) and share one acceptance gate: the chosen plan must match sqlite3's
`EXPLAIN QUERY PLAN` *and* execution must stay in lockstep with what EQP claims.
Open EQP divergences (verified against sqlite3):

| query | graphite today | sqlite3 (target) | piece |
|-------|----------------|------------------|-------|
| `… FROM t JOIN u ON t.c=u.x` (`x` = `u` PK) | `SCAN t` + `SCAN u` | `SCAN t` + `SEARCH u USING INTEGER PRIMARY KEY (rowid=?)` | **B1a** |
| `… FROM t JOIN u ON t.c=u.k` (`k` indexed) | `SCAN t` + `SCAN u` | `SCAN t` + `SEARCH u USING INDEX …` | **B1a/B3** |
| `SELECT count(*) FROM t` (any index `ic`) | `SCAN t` | `SCAN t USING COVERING INDEX ic` | **B2b** |

- **B0b — Index-driven `ORDER BY`/`GROUP BY`, remaining cases.** Extend the
  shared `order_index_scan` decision to: (a) a multi-term `ORDER BY` that is a
  prefix of a multi-column index; (b) `GROUP BY` over an indexed prefix (consume
  groups in index order, no hash); (c) `ORDER BY` satisfied by the index a
  `WHERE` seek already chose (today B0 only fires with no `WHERE`).
- **B1a — Seek the inner table of a join.** In a nested-loop join, when the inner
  table's join column is its rowid/IPK or a usable index, seek it per outer row
  instead of scanning (today every joined table is materialized + hashed). Report
  `SEARCH … USING INTEGER PRIMARY KEY`/`USING INDEX`. The highest-value planner
  win; foundation for B1b.
- **B1b — Join order.** Reorder `FROM` tables by a simple cost model (smallest
  estimated cardinality / most-selective indexed table first) rather than textual
  order; results identical, order verified via EQP. *Ref:* `where.c`.
- **B2b — Covering reads beyond the ordered scan.** Reuse `index_covers_query`
  on (a) `WHERE`-driven index seeks (range/IN/equality) and (b) the
  `count(*)`-with-no-`WHERE` case (count index entries of the smallest full
  index). EQP: `USING COVERING INDEX`.
- **B3 — Automatic indexes for unindexed joins.** Build a transient hash/sorted
  index on a join's inner table when no usable persistent index exists (the
  `auto-index` optimization); report `USING AUTOMATIC … INDEX`. Pairs with B1a.
- **B4 — `sqlite_stat4` histograms.** Extend `ANALYZE` to gather per-index sample
  histograms (byte-compatible `sqlite_stat4` rows) and use them for range
  selectivity. *Ref:* `analyze.c`.

*VDBE migration* (the largest internal refactor — changes representation, not
results; keep the differential corpus green at every step):

- **B5 — VDBE: joins.** Nested-loop join opcodes (`OpenRead`/`Rewind`/`Column`/
  `Next` per cursor) so a two-table join runs on the register machine.
- **B6 — VDBE: `HAVING` + aggregate `ORDER BY`** on the grouped path.
- **B7 — VDBE becomes the execution path.** Migrate `Connection::query` onto the
  VDBE behind a flag, then by default.
- **B8 — Real `EXPLAIN` (bytecode).** Emit the `addr|opcode|p1|p2|p3|p4|p5`
  listing from a compiled `Program` (today `Error::Unsupported`). *Ref:*
  `vdbe.c`, `opcodes.h`.

### Track C — Storage engine, transactions, concurrency & multi-schema

Done: the `Vfs` locking contract (`SHARED`/`RESERVED`/`PENDING`/`EXCLUSIVE`,
process-local), rollback-journal writer serialization, `SAVEPOINT` family,
transaction-state validation, the introspection PRAGMAs (`index_list`,
`index_info`, `foreign_key_list`/`_check`, `integrity_check`/`quick_check`,
`freelist_count`, `application_id`, `data_version`, `table_list`,
`collation_list`, the `pragma_*` TVFs incl. the no-paren form), and the **entire
`ATTACH`/`DETACH`/`TEMP` multi-schema track** (C1–C5): the multi-database
registry + `PRAGMA database_list`, in-memory and file attachments (cross-engine
both directions), schema-qualified reads/writes/`DROP`, cross-database joins
(+ 3-part `aux.t.c` names, `WITHOUT ROWID` sources), qualified DDL
(`ALTER`/`CREATE INDEX|TRIGGER|VIEW`, stored bare-named), cross-database view
reads (via a `read_default` context cell), `TEMP` tables, and cross-database
transactions + savepoints. **C6a** also done — graphite reads `auto_vacuum`
files sqlite3 created and reports `PRAGMA auto_vacuum`, and *refuses* writes to
them (rather than corrupt the ptrmap it can't yet maintain).

**Remaining pieces.**

*Multi-schema leftovers (small):*

- **C-ms1 — `CREATE TEMP INDEX/VIEW/TRIGGER`.** These still target `main`; route
  them to the `temp` database like `CREATE TEMP TABLE` already does.

*Storage — `auto_vacuum` write path (C6b), split so each lands testable:*

- **C6b-1 — header flag + ptrmap page layout at create.** Honor `PRAGMA
  auto_vacuum=FULL|INCREMENTAL` on an empty database: set the header
  largest-root-page / incremental-vacuum fields and reserve pointer-map pages at
  the right page numbers (`PTRMAP_PAGENO` cadence = `usable/5` data pages per
  ptrmap page). Acceptance: sqlite3 opens the empty db, `integrity_check = ok`,
  `PRAGMA auto_vacuum` agrees.
- **C6b-2 — maintain ptrmap entries on alloc/free/balance.** Every page whose
  parent/type changes (btree child moves on split, overflow-chain links, freelist
  transitions) updates its 5-byte ptrmap entry. Acceptance: graphite writes rows
  + indexes into an auto_vacuum file, sqlite3 reads it `integrity_check = ok`.
  Lift the C6a write-refusal once this holds.
- **C6b-3 — `auto_vacuum=FULL` truncate on commit.** At commit, move trailing
  free pages onto freed slots and truncate the file (updating ptrmap + parent
  pointers for moved pages). *Ref:* `btree.c` `autoVacuumCommit`.
- **C6b-4 — `PRAGMA incremental_vacuum(N)`.** The on-demand shrink for
  `auto_vacuum=INCREMENTAL`.

*Storage / durability / concurrency (each independent):*

- **C7 — SQLite-format rollback journal.** Match the on-disk journal byte layout
  (ours is a private, recoverable format today) so a crash mid-write is
  recoverable by `sqlite3`. Pairs with the crash-recovery harness (§6).
- **C8a — `secure_delete`.** Zero freed cell/page content (`PRAGMA
  secure_delete=ON`).
- **C8b — `PRAGMA cache_size` / `mmap_size`.** Accept and honor (today parsed as
  no-ops); bound the page cache accordingly.
- **C8c — a real `pcache` with LRU eviction.** Replace the keep-everything page
  map with a bounded cache (depends on C8b's size). *Perf, not correctness.*
- **C9a — reader `SHARED`-lock enforcement / multi-reader.** Let multiple readers
  share while a writer is excluded, per the locking contract.
- **C9b — OS-level file locks.** Cross-process locking via `std::fs::File::lock`
  (needs MSRV 1.89) behind the std VFS, or a host-provided VFS.
- **C9c — WAL `-shm` wal-index.** The shared-memory index for multi-connection
  WAL readers.
- **C9d — thread-safe `Connection`.** `Send`/`Sync` story for sharing a
  connection (or a documented per-thread model).

### Track D — Virtual tables & ecosystem extensions

Done: a table-valued-function mechanism (`generate_series`, `json_each`,
`json_tree` as `FROM` sources).

**Remaining pieces:**

- **D1a — `sqlite3_module` analog: the trait + registry.** A safe Rust trait for
  virtual-table modules (`connect`/`best_index`/`filter`/`next`/`column`/`rowid`)
  and a connection-scoped module registry to register implementations under a
  name. No SQL surface yet — unit-tested against an in-crate example module.
  *Ref:* `vtab.c`.
- **D1b — `CREATE VIRTUAL TABLE` + executor integration.** Parse
  `CREATE VIRTUAL TABLE … USING module(args)`, persist it in `sqlite_schema`
  (type `table`, with the `USING` form), instantiate the module's table on
  schema load, and treat a vtab as a `FROM` source via the trait
  (`best_index`→`filter`→`next`→`column`). Acceptance: a built-in example vtab is
  queryable. Foundation for D2–D3. *Ref:* `vdbevtab.c`.
- **D2 — FTS5** full-text search (a module on D1). *Ref:* `fts5*.c`.
- **D3 — R-Tree** spatial index (a module on D1). *Ref:* `rtree.c`.
- **D4 — User-defined functions from Rust.** Register scalar/aggregate/window
  functions and custom collations through a safe API (the read side of what
  `func`/`collate` already do internally).
- **D5 — `sqlite3_session`** — changesets/patchsets for replication.
- **D6 — Async VFS for wasm** — non-blocking I/O over IndexedDB/OPFS.
- **D7 — C-API shim** — a `libsqlite3`-compatible surface as a *separate* crate.
  **Blocked:** requires `extern "C"` + raw pointers, incompatible with this
  crate's `#![forbid(unsafe_code)]`; would live in a sibling crate that opts out.

---

## 5. Cross-cutting concerns

- **MSRV** is pinned at **1.88** (`Cargo.toml`); revisit before 1.0 (C9 wants 1.89
  for `File::lock`).
- **Numeric model** — reals are `f64` to match SQLite; no extended decimal/bignum.
- **Parser** stays hand-written (no build-time codegen, friendlier errors);
  `parse.y` remains the source of truth for precedence and accepted forms.
- **Performance** is deliberately secondary to correctness until the VDBE +
  planner land; the iterator executor is `O(n)` in places (some constraint and
  `WITHOUT ROWID` paths rebuild on write) that the planner work will revisit.

---

## 6. File-format compatibility & testing strategy

This is the project's whole reason to exist, so it gets first-class testing.

- **Differential tests.** Run the same SQL through both `sqlite3` and graphitesql
  and diff results; a large generated corpus (`tests/differential.rs`) plus a
  per-feature suite. Every new feature adds to one of these.
- **`integrity_check` as a gate.** Any database graphitesql writes must pass
  `sqlite3`'s `PRAGMA integrity_check` (and, with FKs on, `foreign_key_check`).
- **Round-trip & cross-engine.** graphitesql reads what `sqlite3` writes and vice
  versa, for every storage feature (rowid, `WITHOUT ROWID`, WAL, post-VACUUM).
- **Probing the corpus blind spots.** The result-diff corpus is blind to
  rejection-based behavior, boundary values, `Error::Unsupported` gaps, and
  introspection/error-message detail; these are covered by targeted suites driven
  by probing each semantic dimension against the `sqlite3` CLI.
- **Fuzzing** *(planned, expand)* — fuzz the readers with malformed pages (must
  return `Error::Corrupt`, never panic) and fuzz SQL parsing.
- **Crash-recovery** *(planned, pairs with C7)* — a fault-injecting `Vfs` that
  truncates / fails at chosen fsync points, asserting recovery to a consistent
  state.
- **SQLite's own suite** *(planned)* — run a curated slice of SQLite's `test/`
  TCL assertions (the SQL-level ones) as an additional oracle.

### Known sources of legitimate file divergence

Two SQLite-compatible writers can produce different bytes for the same logical
content; we document and accept these rather than chase them: free-page reuse
order and exact balancing splits, `change_counter`/`version_valid_for` values,
the embedded `SQLITE_VERSION_NUMBER`, and unused/reserved bytes left from
deletions. **Compatibility means both engines read each other's files and agree
on contents**, not byte-identical independently-built databases.

---

## 7. Immediate next steps

Breadth (Track A) and the multi-schema track (C1–C5) are done. Suggested order,
cheapest-and-highest-leverage first:

1. **Quick wins** — clear the small, self-contained gaps: **A5** (`timediff`),
   **A6** (`json_error_position`), **A7** (`execute_batch`), **C-ms1**
   (`CREATE TEMP INDEX/VIEW/TRIGGER`). Each is one function/clause with a direct
   differential test.
2. **B1a: seek the inner table of a join** — the highest-value planner win and
   the next concrete EQP gap; then **B2b** (covering on seeks + `count(*)`),
   **B0b** (remaining `ORDER BY`/`GROUP BY`), **B1b** (join reordering), **B3**
   (auto-index). All perf-only, EQP-gated.
3. **C6b-1…4: `auto_vacuum` write path** — self-contained in the btree/pager
   layer; lands in four testable steps and lifts the C6a write-refusal.
4. **D1a→D1b: the virtual-table interface** — the trait + registry, then
   `CREATE VIRTUAL TABLE` + executor integration; unblocks FTS5/R-Tree.
5. **B5–B8: the executor→VDBE migration** — the largest internal refactor;
   unblocks real `EXPLAIN` and a cost-based planner.

Deferred (documented above): **A1** (`random*` — needs an RNG), **C7/C9b**
(SQLite-format journal & OS locks — durability/concurrency depth), **D7** (C-API
— blocked by `#![forbid(unsafe_code)]`).
