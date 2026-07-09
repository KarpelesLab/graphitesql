# graphitesql roadmap

This document is the plan for **graphitesql**: a single-crate, pure, safe,
`no_std` Rust implementation of SQLite with byte-for-byte compatibility with the
SQLite 3 file format.

The foundation (the file format, the storage/btree/pager stack, and a broad SQL
engine) is **done** — see §3 for a capability summary. The rest of this document
is the forward plan for closing the remaining gap with SQLite: §4 the work
tracks, §5 the cross-cutting concerns, §6 the testing strategy, §7 a suggested
order. **Completed work lives in `CHANGELOG.md` and git history; this file tracks
only what remains.**

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
            │  exec         iterator executor + VDBE          │  execution
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
| `exec`             | name resolution, execution, DDL/DML, triggers, functions, VDBE | `select.c`, `where.c`, `insert.c`, `vdbe.c` |
| `planner` *(in `exec`)* | index selection, join order (partly cost-based) | `where.c`, `analyze.c` |
| `func` / `collate` | scalar/aggregate funcs, collations | `func.c`, `date.c`, `callback.c` |
| `schema`           | parse `sqlite_schema`, build the catalog | `build.c`, `prepare.c` |
| `api`              | `Connection`/`Statement` | `main.c`, `vdbeapi.c` |

**Executor vs. bytecode.** The engine grew as an *operational, iterator-style*
executor and now also has a **register VDBE** that most read queries route to
by default (the tree-walker is the parity oracle and the fallback for shapes the
VDBE hasn't taken over). Track B is finishing that migration — live storage
cursors, correlated subqueries, and windows on the VDBE — plus the cost-model
work that a bytecode planner unlocks.

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
verified against the pinned `sqlite3` 3.50.4 oracle (a 1,600+ query corpus plus
260+ focused test suites). Detail lives in `CHANGELOG.md` and git history; in
summary, graphitesql today:

**Reads & writes real SQLite files.** Opens `sqlite3`-written databases
(including WAL-mode) and **creates** databases whose files `sqlite3` opens with
`PRAGMA integrity_check = ok`. Storage covers rowid and **`WITHOUT ROWID`**
tables, automatic/secondary/`UNIQUE` indexes (incl. `sqlite_autoindex_*`),
overflow pages, the freelist with **page merging on delete**, real **`VACUUM`**
(+ `VACUUM … INTO`), the full **`auto_vacuum`** track (read, write, FULL
auto-truncate, INCREMENTAL reclaim), the **SQLite-format rollback journal** with
hot-journal recovery, and the **WAL read *and* write** path (`journal_mode=WAL`,
`wal_checkpoint`). A whole-number real in a `REAL` column is stored with the
compact integer serial type (`MEM_IntReal`), byte-matching sqlite.

**Runs a broad SQL dialect** — all differentially byte-exact vs the oracle:

- **Queries** — `SELECT` with `WHERE`/`GROUP BY`/`HAVING`/`ORDER BY`
  (`NULLS FIRST/LAST`, `COLLATE`, positional)/`LIMIT`/`OFFSET`/`DISTINCT` and
  SELECT-list aliases; every join kind (`INNER`/`LEFT`/`RIGHT`/`FULL`/cross/comma,
  `NATURAL`/`USING` column coalescing, affinity- & collation-aware keys),
  nested-loop + hash join; compound queries
  (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`, collation-aware, dedup-ordered
  tie-breaking); (recursive/mutual) **CTEs** with `[NOT] MATERIALIZED` and the
  recursive term's `ORDER BY`/`LIMIT` driving the work-queue order; correlated /
  `[NOT] EXISTS` / `IN (SELECT)` / scalar subqueries; derived tables & views as
  sources (inheriting base affinity/collation); **window functions**
  (`ROWS`/`RANGE`/`GROUPS`, `EXCLUDE`, value-offsets, `FILTER`, named windows,
  over `GROUP BY`/aggregates, NULL-correct RANGE frames); row-value comparisons
  and multi-column `IN`.
- **DML** — INSERT (multi-row, `DEFAULT VALUES`, `INSERT … SELECT` snapshot
  semantics), UPSERT (`DO UPDATE/NOTHING`, `excluded.*`, targeted/partial-index,
  incl. on `WITHOUT ROWID`), `RETURNING` (INSERT/UPDATE/DELETE, rowid &
  `WITHOUT ROWID`), UPDATE (simultaneous SET, `UPDATE … FROM`, row-value SET),
  DELETE, all `OR <conflict>` clauses, `AS`-aliased targets, and compensated
  (Kahan) `sum`/`avg`/`total`.
- **DDL** — CREATE/DROP/ALTER TABLE (ADD/DROP/RENAME COLUMN, RENAME TABLE, with
  cross-object propagation into views/FKs/triggers), CREATE/DROP
  VIEW/INDEX/TRIGGER (BEFORE/AFTER/INSTEAD OF, `UPDATE OF`, `WHEN`, `RAISE`,
  recursive, `NEW`/`OLD`), STRICT & WITHOUT ROWID, generated columns
  (VIRTUAL/STORED), AUTOINCREMENT + `sqlite_sequence`, partial/expression/
  collation/DESC indexes, constraint-level `ON CONFLICT`, foreign keys
  (CASCADE/SET NULL/SET DEFAULT/RESTRICT, composite, self-referential,
  DEFERRABLE), and full CREATE-time + ALTER validation with byte-exact error
  parity.
- **Functions & values** — the full scalar/aggregate/date-time
  (`strftime` incl. `subsec`/`%J`)/`printf`+`format` (incl. the `!` alt-form-2
  high-precision flag via the ported `sqlite3FpDecode`)/JSON + JSONB libraries;
  type affinity; collation (BINARY/NOCASE/RTRIM) propagated through
  IN/BETWEEN/CASE/min-max/compound/ordering; `random()`/`randomblob()`;
  blob↔text↔number coercion; verbatim column-name source spans; JSON paths with
  quoted-key backslash escapes.
- **Constraints, triggers, transactions** — `NOT NULL`/`CHECK`/`UNIQUE`/`PK`,
  partial/expression UNIQUE indexes, FK enforcement; triggers incl. reentrant
  same-table edits; the `SAVEPOINT` family; `ATTACH`/`DETACH`/`TEMP` multi-schema.
- **Schema catalog & introspection** — `sqlite_schema`/`sqlite_master` readable
  with sqlite-canonicalised `sql` text; the introspection PRAGMAs and the
  `pragma_*` table-valued-function surface (incl. bare `WHERE arg=…`-driven
  forms); `EXPLAIN QUERY PLAN` shaped byte-exactly across the derived/CTE/view
  flatten & CO-ROUTINE taxonomy, scalar/`IN`-subquery nodes, the seek family, and
  the trailing temp-b-tree / ORDER-BY-elision cases (the whole 2026-06/07 B9
  cluster).
- **Virtual tables & extensions** — a writable/persistent vtab layer with the
  built-in **R-Tree**, **FTS5** (read + write, sqlite-readable on disk, full query
  language + `bm25`/`highlight`/config), **geopoly** (scalar library + R-Tree-
  backed vtab), `dbstat`, read-only `sqlite_dbpage`, and Rust scalar/aggregate
  **UDFs**.

The shell (`graphitesql`) supports `.tables`/`.indexes`/`.schema`/`.databases`/
`.dump`/`.read`/`.headers`, byte-compatible where implemented.

What remains is breadth and depth toward full SQLite parity, below.

---

## 4. Forward plan — remaining work

Five tracks. Each keeps only the **open pieces**, in small independently-shippable
chunks; every chunk lands with a differential test and keeps `master` green.
Tracks can progress in parallel. (Completed items — the vast majority of the SQL
surface — are in git history and `CHANGELOG.md`.)

### Track A — SQL language & functions  *(substantially complete)*

The differential sweep has exhausted the bounded, single-fix work; what remains is
a handful of residuals, each needing an architectural change disproportionate to
its (niche/cosmetic) value:

- **A-misc-1 — structural row-arity error *ordering* vs name resolution.** When a
  clause holds both a row-value misuse (`(nope,a) IN ((1,2,3))`) and a missing
  column, graphite resolves all columns up front then runs the structural checks,
  so it reports the column error where SQLite — a single name-resolution walk in
  clause order (`result-set → HAVING → WHERE → GROUP BY/ORDER BY`, pre-order within
  each tree, first-fault-wins) — sometimes reports the structural one. The message
  *bodies* are identical; only which of two errors fires first differs on
  doubly-malformed input. Fix = interleave the arity check with column resolution
  clause-by-clause; deferred as fragile-for-cosmetic-gain.
- **A-tvf-bare-series — bare `generate_series` (no parens).** `FROM json_each`,
  `FROM pragma_*` (bare, `WHERE arg=…`-driven) are done; `generate_series` is the
  hard one — its default `stop` is unbounded, and graphite's tree-walker
  *materialises* every TVF source, so it can't represent the lazy unbounded stream
  without hanging. Belongs on the VDBE lazy-cursor track (Track B), not the
  materialise path.
- **A-rn3-edge — RENAME COLUMN in a genuinely *mixed* view/trigger body.** The
  scope-aware rewrite handles single-source, nested-subquery, and cross-object
  bodies. The residual is the *same* bare column name binding to *different* tables
  in one statement (`SELECT a FROM t WHERE a IN (SELECT a FROM u)` renaming `u.a`),
  where only the inner `a` should change — needs per-occurrence source spans on
  `Expr::Column`. The pass declines it, leaving the object byte-identical (never a
  half-rename).
- **A-alter-rollback — ALTER-time rejection of a RENAME that breaks a dependent.**
  `DROP COLUMN` already rejects pre-mutation. A `RENAME` whose propagation can't be
  *proven* can leave a dependent view/trigger unresolvable; SQLite rejects and
  rolls back, graphite mutates first. Needs **statement-level DDL rollback** — a
  writer savepoint around `exec_alter`, mirroring `run_dml_atomic`.

### Track B — Query planner, statistics & the VDBE

Every item is gated on VDBE-vs-tree-walker parity (returns the tree-walker's
result or declines to it — never a wrong answer), so this track is
**perf/coverage/EQP-fidelity only**, never a correctness risk.

**Move the last shapes onto the VDBE:**

- **B5b-2 — seek-driven inner cursor over real storage** *(the largest remaining
  VDBE piece)*. Inner rowid seeks (INNER + LEFT, single & N-table left-deep chain,
  compound-`ON`) already run over a live `TableCursor`. *Single-table live scan
  done (2026-07-09, 52116a2):* a plain rowid base-table `SELECT` routed through the
  VDBE now streams rows from a live `TableCursor` in the interpreter via a
  `Cursor0Source` trait (`rewind`/`advance`/`column`) instead of materializing —
  additive, result-identical to the tree-walker + sqlite, falls back for
  WITHOUT ROWID / subquery / view / join / hinted sources. Remaining: the
  in-*interpreter* `OpenRead`/`SeekRowid` opcodes over B5b-1's multi-cursor
  foundation (move the *seek* into bytecode — an internal refactor, no behavior
  change); and seek by a **secondary index** / `WITHOUT ROWID` PK, which is
  *affinity-blocked* (`index_seek_rowids` compares raw keys with the index
  collation and skips the comparison-affinity that `o.x = t.k` applies — routing
  it risks a silent false-negative that wouldn't fall back; needs the tree-walker's
  affinity machinery threaded in first).
- **B5c-2 — correlated subqueries on the VDBE.** Any subquery reading an outer
  column defers to the tree-walker today. *Attempted and reverted (2026-07-09,
  revert 0623182):* a runtime callback re-running the subquery per outer row
  cannot replicate sqlite's **PREPARE-time validation** — an invalid subquery over
  a zero-row outer scan (`(SELECT 1,2)`, `(SELECT unknown.col)` over an empty
  table) is never evaluated, so graphite silently accepted what sqlite rejects
  (regressed `row_value_misuse` + `subquery_body_columns`, caught by CI). The redo
  must **validate the subquery body at compile time** (resolve every column against
  the scope, check scalar arity) and route only provably-valid correlated
  subqueries, bailing the rest to the tree-walker. Low priority — no user-visible
  benefit (results already match via the tree-walker).
- **B1c — RIGHT/FULL join inner seeks.** INNER/LEFT seek; RIGHT/FULL still
  materialize the inner table (correct, just not seek-driven).

**Cost model & EQP fidelity** *(rows already correct — plan/perf only)*:

- **B9h — cost-model single-table index *choice*.** The purely *structural* costs
  are done (no-`WHERE` covering-scan choice; covering-preferred equality/range/
  GROUP-BY/DISTINCT/ORDER-BY seeks via `choose_seek_index`/`choose_range_index`).
  Still open: ORDER BY influencing the index *choice* (sort-avoidance term in the
  cost model); the tiebreak among several non-covering indexes sharing an equality
  prefix (SQLite's full LogEst row-cost, not reducible to narrower/newest); a
  *partial-prefix* covering index for a multi-column ORDER BY (unify
  `order_index_scan`/`covering_scan`); `min`/`max` over a non-leading covered
  column (one-end-seek-vs-full-covering-scan distinction). These are structural; a
  stat4 oracle is only needed for genuinely data-driven choices (B4).
- **B1b — cost-based join reordering.** The structural slices are done (rowid- and
  index-inner swaps, N-table greedy connected-cheapest order, covering-index join
  scans, trailing-node EQP parity). Still open: **selectivity-driven** ordering (a
  single-table WHERE restriction picking the driver) and projection-sensitive
  equal-cost driver ties — the full `whereLoopAddBtree`/`wherePathSolver` formula.
  Divergent EQP where graphite's per-cursor access paths are often cheaper than
  sqlite's cost-reordered scans is *by design* (results correct).
- **B9j — collation-aware index *selection* for a non-default-collation index.**
  An index carrying a non-default collation (`CREATE INDEX ib ON t(b COLLATE
  NOCASE)`) is mis-selected both ways (rows still correct via the WHERE re-apply).
  The correct model — an index serves a comparison iff its per-column collation
  equals the comparison's *effective* collation — must be threaded into selection
  at ~9 `collect_eq_constraints` sites in lockstep; a careful cross-cutting
  refactor.
- **B9b — window-function EQP.** The co-routine *body* is exactly the B9h index
  choice (SQLite picks the index that covers the input **and** serves the
  `PARTITION BY`/window-`ORDER BY`), so this is **blocked on B9h** (plus a
  deterministic model for the multi-window/nested `(subquery-N)` numbering).
- **B4 — `sqlite_stat4` histograms.** *Generation done (2026-07-09).* `ANALYZE`
  emits byte-compatible `sqlite_stat4` (faithful `analyze.c` accumulator port in
  `src/exec/stat4.rs`), verified 0-diff against a `-DSQLITE_ENABLE_STAT4` oracle
  across 300+ fuzzed schemas; a pre-existing `sqlite_stat1` avg-eq divergence was
  fixed in the same pass. *Planner-use — equality done (2026-07-09):* the index
  chooser now consults stat4 samples for `col = ?` selectivity (ports
  `whereKeyStats`/`whereEqualScanEst`/`initAvgEq`), so a rare-value equality flips
  to the selective index — narrowed (only when the index has stat1+stat4 and the
  matched leading prefix is fully bound) so no ANALYZE-less plan moves and the EQP
  corpus stays green. *Equality scan-vs-search done (2391bf4)* and *range
  selectivity done (3317535):* the index-vs-full-`SCAN` `rRun` comparison
  (`full_scan_beats_seek`/`full_scan_beats_range`, LogEst ports of
  `whereLoopAddBtree`/`whereLoopFindLesser`/`wherePathSolver`) now full-scans an
  unselective non-covering equality **or** leading range at sqlite's exact
  boundary, and `whereRangeScanEst`'s STAT4 branch estimates range rows from the
  samples — all gated to stat4-backed non-covering single-candidate cases so the
  EQP corpus stays byte-identical. *Join-order path solver done (7493118):* a
  bounded LogEst `wherePathSolver`/`whereLoopAddBtree` port (`join_scan_cost`/
  `join_seek_cost`/`two_table_second_drives_cheaper`/`ntable_join_order`) now
  picks the join **driver** by cost — `big JOIN small` drives `small`, 3/4-table
  hubs drive the smallest table — matching the oracle, stats-gated so no-ANALYZE
  plans are byte-identical. **Remaining (small):** the tail-table access *label*
  in 3+-table plans (sqlite renders `BLOOM FILTER`/`AUTOMATIC COVERING INDEX` for
  the tail vs graphite's index seek — a separate rendering track, result/order
  unaffected); and choosing among several *candidate indexes on one table* by
  full WhereLoop cost (only the scan-vs-seek leaf + driver order are ported).

### Track C — Storage engine, transactions, concurrency

- **C8c — read-cache for read-only connections. DONE (2026-07-09).** The
  `WritePager` clean-page cache now serves the read-only path too, gated by a
  change-counter validity token: `read_cache_token` records the page-1 change
  counter the cached pages were read under, and `revalidate_read_cache()` (called
  at the `query_params` read-statement boundary via `revalidate_read_caches`)
  re-reads the on-disk counter directly and drops the cache if a foreign in-process
  `Connection` committed (sqlite bumps the counter per commit) — no-op under a
  write lock and in WAL mode. Additive (results byte-identical); verified with two
  in-process connections (`tests/c8c_readonly_cache.rs`: sees a foreign commit on
  the next statement, repeat reads served from cache). Orthogonal residual: a
  foreign commit that *grows* the file still errors "page N out of range" on the
  read-only path (`page_count` frozen at open) — unrelated to the cache.
- **C9a — persistent read locks. DONE (2026-07-09).** `WritePager::begin_read_txn`/
  `end_read_txn` hold a persistent `Shared` lock for an open read transaction, and
  `Connection::ensure_read_txn_lock` acquires it at the **first read within** an
  explicit transaction (matching sqlite's *deferred* `BEGIN` — not eagerly at
  `BEGIN`, so two bare `BEGIN`s don't wrongly block each other's later write).
  Readers coexist via the counted `Shared`; a writer's commit-time `Exclusive`
  upgrade `BUSY`s until readers drain. To let the `&self` read path take the lock,
  `File::lock`/`unlock` became `&self` with a `Cell<LockLevel>` per handle (no
  `unsafe`); the write-path `Shared→Reserved→Exclusive` upgrade is unchanged.
  Verified with two in-process `Connection`s (`tests/c9a_connection_locks.rs`),
  including the deferred guard and autocommit-never-blocks.
- **C9b — OS-level cross-process file locks** (`std::fs::File::lock`, wants MSRV
  1.89) behind the std VFS.
- **C9c — the WAL `-shm` wal-index** for multi-connection WAL readers.
- **C9d — a thread-safe `Connection`** (`Send`/`Sync`, or a documented per-thread
  model).

### Track D — Virtual tables & ecosystem extensions

- **D2b-leftover (perf-only).** A ≥3-phrase `NEAR` still falls back to the
  `_content` scan (results correct). The high-frequency-term case is **no longer a
  fallback** — a spanning term's doclist-index segment is served via the index
  route (pinned by `high_frequency_spanning_term_takes_index_route`).
- **D2e-encoder — byte-identical FTS5 at large scale.** *Doclist-index done
  (2026-07-09):* ported sqlite's `fts5WriteDlidxAppend`, so a term whose doclist
  spills onto ≥ `FTS5_MIN_DLIDX_SIZE` continuation leaves now emits byte-identical
  `dlidx` pages and sets the term's `%_idx` dlidx bit — graphite's file is
  integrity-clean and sqlite-readable at scale (was rejected as "malformed" at
  ~8000 docs before). *Multi-term leaf-fill done (2026-07-09):* ported sqlite's
  `fts5WriteAppendTerm` split rule (`4 + body + pgidx + nTerm + 2 >= pgsz`, full
  uncompressed term length), so multi-term segments are now byte-identical up to
  ~37 leaves incl. variable-length terms. *Prefix indexes done (2026-07-09):*
  the rebuild now emits a prefix-index segment per configured `prefix=` length
  (keyed `FTS5_MAIN_PREFIX + i + 1`), byte-identical for `'1'`/`'2 3'`/`'1 2 3'`/…
  incl. unicode/multi-column/contentless. *Doclist spill fixed (2026-07-09):* the
  spill onto term-less continuation leaves now keeps position varints whole
  (sqlite never splits a varint) and writes absolute first-rowids — a spanning
  corpus went from 62 leaves divergent to byte-identical. (Note: FTS5 3.50.4 has
  **no** `height>0` interior `%_data` pages — that form was removed; a
  high-frequency term's "interior" structure is its doclist-index, and the
  per-segment term index is the plain `%_idx` table.) **Remaining:** a corpus with
  enough distinct terms that sqlite flushes multiple segments + `optimize`-MERGEs
  them spills poslists on a different boundary than graphite's single-pass build
  (`>= nReq` merge fill vs `fts5FlushOneHash`), so a spanning term in a *merged*
  index drifts a few bytes/leaf — inherent to graphite building one segment;
  stays integrity-clean + MATCH-correct. The deeper fix is the same **incremental
  multi-segment automerge** write path (also the O(rows²)-bulk-load perf residual).
- **D4-leftover — window UDFs + custom collations.** The latter needs a user
  variant on the `Collation` enum (invasive).
- **D5 — `sqlite3_session`** changesets/patchsets for replication.
- **D6 — async VFS for wasm** (non-blocking IndexedDB/OPFS I/O).
- **dbpage-2 INSERT leftover.** The writable `sqlite_dbpage` **UPDATE** path is
  done (patch a page's raw bytes; byte-identical to the oracle). The **INSERT**
  form is not: writing a page *beyond* EOF (`INSERT(count+1, …)`) grows the file
  while leaving the header `page_count` unchanged — a deliberately inconsistent
  state (file size ≠ header) that graphite's consistency-maintaining pager (which
  truncates the file to `page_count` at commit) prevents by construction.
  Reproducing it byte-for-byte would mean breaking that invariant to write a
  malformed file for an operation that only ever produces one; parked as an
  architectural boundary, not a blocker. (The read-side `WHERE schema='aux'`
  redirect is likewise still open.)

**Blocked by design:**
- **D7 — C-API shim** (`libsqlite3`-compatible surface). Needs `extern "C"` + raw
  pointers, incompatible with `#![forbid(unsafe_code)]`; would live in a sibling
  crate that opts out.

### Track E — Cross-database write resolution  *(essentially complete)*

A write to an attached/`temp` database swaps that database in as the active `main`
for the whole statement, while a subquery/source reading the *original* main still
resolves there. `INSERT … SELECT`/`… VALUES ((SELECT …))` are pre-materialised in
the original context before the swap.

A **schema-qualified** subquery reference in a cross-db write's WHERE/SET
(`UPDATE aux.u … WHERE a IN (SELECT a FROM main.t)`, or the target's own `aux.u`)
now resolves correctly: `resolve_db` inverts the swapped pair while the swap is
live (a `swap_active` flag; qualified refs only — unqualified keeps the active
slot).

**Remaining:** an *unqualified* name present in **both** the active db and an
attached one, referenced unqualified inside a cross-db write, binds to the active
db (graphite) rather than `main` (sqlite). Realistic schemas qualify such
references. A full fix would mean dropping the read-side swap entirely (resolve
the write target by qualifier, reads by the global `main → temp → attached` order)
— a larger refactor of the pervasive resolution path.

### CLI shell (`graphitesql`)

The shell covers the common introspection/dump commands, and as of 2026-07-09 the
output/import layer: `.mode` (`list`/`csv`/`column`/`line`/`tabs`/`quote`/`insert`/
`json`), `.separator`, `.nullvalue`, `.output`/`.once`, `.import` (CSV), `.echo`,
`.changes` — all byte-verified against `sqlite3` 3.50.4. Still not implemented:
`.backup` (needs an engine DB-copy/serialize API), `.bail` (observable only via the
CLI error-message text, which graphite renders `Error:` rather than sqlite's
`Parse error near line N`), `.print`, `.show`, and the `ascii`/`html`/`markdown`/
`box`/`tcl` `.mode`s. Peripheral (the SQL engine, not the shell, is the project's
purpose), so lower priority.

---

## 5. Cross-cutting concerns

- **Edition** is **Rust 2024** (`let`-chains adopted); **MSRV** is pinned at
  **1.88** (`Cargo.toml`) — revisit before 1.0 (C9b wants 1.89 for `File::lock`).
- **Numeric model** — reals are `f64` to match SQLite; no extended decimal/bignum.
- **Text is UTF-8.** `Value::Text` is a Rust `String`, so an operation that would
  produce *non-UTF-8* "text" (e.g. `zeroblob(2)||x'ff'`) falls back to `Value::Blob`
  to preserve the bytes — the bytes match sqlite, but `typeof`/`quote` differ.
  Fixing means a bytes-backed text value, a pervasive Value-model change; deferred.
- **Parser** stays hand-written (no build-time codegen, friendlier errors);
  `parse.y` remains the source of truth for precedence and accepted forms.
- **Performance** is deliberately secondary to correctness until the VDBE + planner
  work lands; the iterator executor is `O(n)` in places (some constraint and
  `WITHOUT ROWID` paths rebuild on write) that Track B/C will revisit.

---

## 6. File-format compatibility & testing strategy

This is the project's whole reason to exist, so it gets first-class testing.

- **Differential tests.** Run the same SQL through both `sqlite3` and graphitesql
  and diff results; a large generated corpus (`tests/differential.rs`) plus a
  per-feature suite. Every new feature adds to one of these.
- **`integrity_check` as a gate.** Any database graphitesql writes must pass
  `sqlite3`'s `PRAGMA integrity_check` (and, with FKs on, `foreign_key_check`).
- **Round-trip & cross-engine.** graphitesql reads what `sqlite3` writes and vice
  versa, for every storage feature (rowid, `WITHOUT ROWID`, WAL, post-VACUUM,
  attached, R-Tree, FTS5).
- **Probing the corpus blind spots.** The result-diff corpus is blind to
  rejection-based behavior, boundary values, `Error::Unsupported` gaps, and
  introspection/error-message detail; these are covered by targeted suites driven
  by probing each semantic dimension against the `sqlite3` CLI. *(The corpus does
  NOT exercise cross-database writes — see Track E.)*
- **Fuzzing** — a deterministic corruption-robustness harness
  (`tests/fuzz_corruption.rs`, ~50k malformed-file variants; `tests/fuzz_sql.rs`,
  ~3.3k malformed/deeply-nested SQL) asserts the readers return an error and never
  panic. *(Expand toward a coverage-guided fuzzer when a no-dep path exists.)*
- **Crash-recovery** *(Done)* — a fault-injecting `FaultVfs` drives rollback-journal
  and **WAL** recovery suites asserting an `integrity_check = ok` state at every
  injection point, cross-checked with `sqlite3`.
- **SQLite's own suite** *(planned)* — run a curated slice of SQLite's `test/` TCL
  assertions (the SQL-level ones) as an additional oracle.

### Known sources of legitimate file divergence

Two SQLite-compatible writers can produce different bytes for the same logical
content; we document and accept these rather than chase them:

- Free-page reuse order and exact balancing splits; `change_counter` /
  `version_valid_for` / the embedded `SQLITE_VERSION_NUMBER`; unused/reserved bytes
  left from deletions. **Compatibility means both engines read each other's files
  and agree on contents**, not byte-identical independently-built databases.
- **Multi-row `WITHOUT ROWID` & secondary-index b-tree page layout** differs from
  sqlite's cell ordering/free-block placement (a WR table with *no* real column
  diffs identically to one with — i.e. it is layout, not the `MEM_IntReal` record
  encoding, which *does* byte-match). A separate, larger b-tree-layout parity
  project; files remain valid & mutually readable.

### Behaviors we intentionally do NOT match

- **Build-specific oracle quirks.** The pinned oracle is a custom `alt1` build:
  ICU-style full-Unicode `LIKE` case folding (graphite is ASCII-only, like
  documented SQLite); `utc`/`localtime` TZ-dependent modifiers; the read-only
  `sqlite_dbpage` write refusal; `PRAGMA function_list`/`collation_list` enumerating
  the build's loaded extensions; `PRAGMA table_list` schema-hash row order. Matching
  any of these would mean differential-testing a compile-time option.
- **Documented-undefined SQLite behavior.** A bare `ON CONFLICT DO UPDATE` on a row
  violating *multiple* uniqueness constraints ("undefined which constraint
  triggers"); a multi-match `UPDATE … FROM` join ("arbitrarily chosen"). graphite
  handles the well-defined single-match cases exactly; the ambiguous ones are
  plan-dependent and not chased.
- **Cosmetic / last-ULP residuals.** Windowed `sum()`'s sticky integer-vs-real
  *type tag* (per-frame recompute vs SQLite's `xStep`/`xInverse` `approx`
  flag — value identical, tag differs; porting window.c scheduling is high effort
  for a display tag); huge-argument `sin`/`cos` last-ULP (would need reimplementing
  glibc's libm); extreme-exponent (`|exp| ≳ 300`) `quote()` real rendering (a
  compiler-FP parser artifact — graphite's shorter form round-trips in graphite);
  a fractional-second + `-N month` date-modifier rounding edge.

---

## 7. Suggested order

The headline features are done (§3), Tracks A & E are essentially complete, and the
EQP-fidelity thread is largely closed. What remains (§4) is bigger, multi-session
work, each track independently shippable:

1. **B5b-2 — live storage cursors on the VDBE** *(in progress)*. The largest
   remaining VDBE piece; rowid seeks landed, the next sub-steps are the
   in-interpreter `OpenRead`/`SeekRowid` opcodes and the affinity-blocked
   secondary-index / `WITHOUT ROWID` seeks. Perf/coverage, parity-gated, low risk.
2. **B5c-2 — correlated subqueries on the VDBE**, once B5b-2 lands the live-cursor
   machinery. This also unblocks bare `generate_series` (A-tvf-bare-series) via
   lazy row production.
3. **C9a → C9d — the concurrency story** — persistent read locks in `src/pager/`,
   then OS file locks (MSRV 1.89), the WAL `-shm` index, and a thread-safe
   `Connection`.
4. **Ecosystem surfaces** — D2e-encoder (needs the fts5 writer source), D5
   (`sqlite3_session`), D6 (async wasm VFS); dbpage-2 UPDATE is done, its INSERT-grow an architectural boundary (see Track D).
5. **Cost model (Track B)** — the selectivity-driven join order and remaining B9h
   index-choice items; B9b window EQP unblocks once B9h lands. **B4** stat4 needs a
   stat4-enabled oracle first.
6. **Track A leftovers** — an `Expr::Column` source-span enrichment for the
   A-rn3-edge *mixed*-body rewrite; statement-level DDL rollback for A-alter-rollback;
   the A-misc-1 error-ordering interleave (all low-value, deferred by design).

**Deferred / blocked** (documented in §4/§6): **B1b** selectivity-driven join
reordering and **B4** `sqlite_stat4` (unverifiable against the stat1-only oracle);
**B9j** collation-aware index selection; **B1c** RIGHT/FULL inner seeks (correct via
materialization); **D7** the C-API shim (needs `unsafe`; a sibling crate);
the Track E cross-db qualified-subquery resolution;
and the FTS5 large-scale encoder sub-cases (need the fts5 writer source).
