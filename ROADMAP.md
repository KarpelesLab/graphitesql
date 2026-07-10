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

RENAME COLUMN dependency propagation is now comprehensive — mixed-scope
views+triggers (per-occurrence `Expr::Column` spans), compound (UNION/INTERSECT/
EXCEPT) incl. `ORDER BY`, and `WITH` CTE bodies incl. output-column provenance
(details in git history / `CHANGELOG.md`). What remains:

- **A-alter-1 — derived-table propagation for RENAME COLUMN.** *(small; unblocks
  A-alter-2)* The last propagation gap: a derived-table/TVF subquery consumed in a
  compound arm (`SELECT a FROM t UNION SELECT a FROM (SELECT a FROM u)`) still bails
  (safe — never a wrong rewrite) where sqlite rewrites the sibling base arm. Apply
  the CTE-provenance pattern to `FROM (subquery)`: in `collect_select_base_sources_ctx`
  recurse the derived body (skip its alias as a base source); in
  `collect_bare_old_owners` compute the body's provenance via a `body_exposes_old`
  helper (factor it out of `cte_old_owner`), key the derived source by its alias
  (synthetic key when unaliased), and resolve a ref to its output column the same
  way (bail if it exposes the renamed `old` unaliased, else leave). Verify vs sqlite
  incl. aliased/unaliased/nested and the reject case.
- **A-alter-2 — ALTER-time rejection of a RENAME that breaks a dependent.**
  *(depends on A-alter-1)* sqlite rejects + rolls back a rename that leaves a
  dependent view/trigger unresolvable (`USING(a)` column vanishes, or a
  derived-table-in-main-position); graphite currently mutates and commits. The
  writer-savepoint machinery was built and reverted once — the false-reject bug it
  hit is now fixed by comprehensive propagation (only the A-alter-1 case remains, so
  do that first). Redo: wrap the rename in a `\0graphite_alter` writer savepoint,
  `Schema::read` the *uncommitted* overlay (`rewrite_schema_rows` writes the
  sqlite_master b-tree but not `self.schema`), resolve-check each dependent, and on
  failure `ROLLBACK TO` + emit the byte-exact `error in {view|trigger} NAME after
  rename: …` (graphite's resolver already matches sqlite's detail). Pre-mutation
  `DROP COLUMN` rejection is the existing template.
- **A-misc-1 — structural row-arity error *ordering* vs name resolution.** *(niche;
  cosmetic)* On doubly-malformed input (a row-value misuse *and* a missing column
  in one clause) graphite reports the column error where sqlite sometimes reports
  the structural one — message bodies identical, only first-fault order differs.
  Fix = interleave the arity check with column resolution clause-by-clause
  (`result-set → HAVING → WHERE → GROUP BY/ORDER BY`, first-fault-wins). Fragile for
  cosmetic gain; low priority.
- **A-tvf-bare-series — bare `generate_series` (no parens).** *(belongs to Track B)*
  Its default `stop` is unbounded and the tree-walker materialises every TVF source,
  so it needs the VDBE lazy-cursor path — tracked under Track B, not here.

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

In-process multi-connection coordination is done: read-only clean-page cache
(C8c), persistent deferred read locks (C9a), the process-local WAL wal-index
(C9c), and the documented thread-confined `Connection` model (C9d). See git
history / `CHANGELOG.md`. Remaining:

- **C9b — OS-level cross-process file locks.** *C9b-0/1/2 DONE 2026-07-10.* The
  `StdVfs` now drives **one process-wide OS advisory lock** (`std::fs::File`'s 1.89
  `lock`/`try_lock`/`unlock`) off its per-path aggregate `LockState` (`CpLock` in
  `src/vfs/std_file.rs`): OS *shared* while only readers are active, OS *exclusive*
  for any write intent. So two OS processes over one file serialize their writes
  (and explicit read txns coordinate) — the C9b-0 MSRV bump, the C9b-1 primitive,
  and the C9b-2 two-process test (`tests/c9b_cross_process_locks.rs`, unix-guarded)
  all landed. **Whole-file limitation (documented):** the mapping is *pessimistic*
  — a writer holds OS-exclusive for the whole write txn (from `RESERVED`), so it
  blocks cross-process readers during a write, where SQLite's byte-range `RESERVED`
  lock keeps them. std's whole-file locks cannot express that split. Remaining:
  - **C9b-3 — autocommit reads take a cross-process shared lock.** An *explicit*
    read txn takes the persistent shared lock (C9a) and so coordinates
    cross-process, but a bare autocommit `SELECT` reads without driving the
    aggregate to `Shared`, so it isn't blocked by a foreign exclusive lock (could
    read a torn page during another process's write). Make the autocommit read path
    take (and release) a transient shared lock around the read.

### Track D — Virtual tables & ecosystem extensions

- **D2b-leftover (perf-only).** A ≥3-phrase `NEAR` still falls back to the
  `_content` scan (results correct). The high-frequency-term case is **no longer a
  fallback** — a spanning term's doclist-index segment is served via the index
  route (pinned by `high_frequency_spanning_term_takes_index_route`).
- **D2e-encoder — byte-identical FTS5 at large scale.** The writer is byte-identical
  vs sqlite for the mainline: doclist-index, multi-term leaf-fill, prefix indexes,
  doclist spill, incremental multi-segment writes + automerge, and incremental
  DELETE/UPDATE tombstones incl. delete-crisis merge (git history / `CHANGELOG.md`).
  **Remaining (thin tails — all fall back to the *correct* bulk rebuild, never
  wrong, just not incremental):**
  - **D2e-1 — delete-crisis with a populated higher level** (needs tombstones
    carried into the merge).
  - **D2e-2 — incremental writes inside explicit `BEGIN`/`SAVEPOINT`** (autocommit
    is incremental; explicit txns rebuild).
  - **D2e-3 — incremental path for prefix-index and spanning-dlidx segments.**
- **D4-leftover — window UDFs + custom collations.** The latter needs a user
  variant on the `Collation` enum (invasive).
- **D5 — `sqlite3_session`. Essentially complete.** Changeset/patchset generation
  + apply (all PK shapes incl. composite/WITHOUT ROWID), `invert`/`concat`, custom
  conflict handlers (`xConflict`), per-table attach, indirect-change flagging
  (trigger/FK), and changeset rebase (`sqlite3_rebaser`) are all byte-verified vs
  the `SQLITE_ENABLE_SESSION` oracles (git history / `CHANGELOG.md`). Only
  **streaming** (`xInput`/`xOutput`) is unimplemented — an API-shape variant with
  no benefit over the `Vec` API in Rust; effectively won't-do.
- **D6 — async VFS for wasm** (non-blocking IndexedDB/OPFS I/O). *Needs a user
  decision on target/architecture before implementation.* Chunks (pending that):
  - **D6-0 — decide the async model.** Blocking sync-over-async (Atomics.wait in a
    worker) vs a genuinely async `Connection` API; which backend (OPFS sync-access
    handle — itself sync — vs async IndexedDB); wasm target (`wasm32-unknown-unknown`
    + JS glue vs `wasm32-wasi`). This choice drives everything below.
  - **D6-1 — wasm build + a memory-backed VFS smoke test** in the browser (no
    persistence yet), to establish the toolchain and glue.
  - **D6-2 — the persistent async VFS** implementing the chosen backend behind the
    existing `Vfs`/`File` traits.
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

The shell covers the common introspection/dump commands, the full `.mode` family
(`list`/`csv`/`column`/`line`/`tabs`/`quote`/`insert`/`json`/`ascii`/`html`/`tcl`/
`markdown`/`box`/`table` — all byte-verified vs `sqlite3` 3.50.4, incl. the
`SHELL_ESC_ASCII` control-char escaping across every display mode), the output/
import layer (`.separator`/`.nullvalue`/`.output`/`.once`/`.import`/`.echo`/
`.changes`/`.print`/`.show`), and `.backup`/`.save` (backed by
`Connection::serialize()`). See git history / `CHANGELOG.md`. **Remaining (all
peripheral — the SQL engine, not the shell, is the project's purpose):**
- **CLI-1 — `.bail`** (stop on first error).
- **CLI-2 — sqlite-style error text.** Render `Parse error near line N: … (code)`
  with the `error here ---^` caret instead of graphite's bare `Error:`. Needs
  parser token byte-offsets threaded into `Error` — a library-level change, not
  shell-only. (The `Expr::Column` span work is a precedent for carrying offsets.)
- **CLI-3 — `.echo` per-input-line.** Echo each raw dot-command input line, not
  just SQL groups.

---

## 5. Cross-cutting concerns

- **Edition** is **Rust 2024** (`let`-chains adopted); **MSRV** moving to **1.89**
  (approved 2026-07-10) to use `std::fs::File::lock` for C9b cross-process locks
  (tracked as C9b-0).
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
  a fractional-second + `-N month` date-modifier rounding edge; and the single
  pathological hex literal `-0x8000000000000000` (SQLite folds the unary minus
  into the hex literal and rejects the `INT64_MIN` overflow at parse time;
  graphite treats `-` as a runtime operator that overflows to a real — exactly
  as SQLite itself does for `0 - 0x8000000000000000`. Every other hex literal,
  signed or not, matches).

---

## 7. Suggested order

The headline features are done (§3); Tracks A, C, D & E are essentially complete
(RENAME COLUMN propagation, in-process concurrency, FTS5 mainline, `sqlite3_session`
all landed). What remains (§4) is smaller chunks plus a few bigger threads, each
independently shippable. Recommended next order:

1. **C9b — cross-process OS file locks** *(newly unblocked; MSRV 1.89 approved)*.
   Start with **C9b-0** (the MSRV bump), then the lock primitive (C9b-1) and the
   two-process differential test (C9b-2). Well-scoped, high value.
2. **A-alter-1 → A-alter-2 — finish RENAME dependency safety.** Derived-table
   propagation (small, reuses the CTE-provenance pattern), then the savepoint
   rollback + resolve-check re-validation (the reverted machinery, now unblocked).
3. **B5b-2 — live storage cursors on the VDBE** *(in progress)*. The largest VDBE
   piece; next sub-steps are the in-interpreter `OpenRead`/`SeekRowid` opcodes and
   the affinity-blocked secondary-index / `WITHOUT ROWID` seeks. Parity-gated, low
   risk. Then **B5c-2** correlated subqueries (compile-time-validated), which also
   unblocks bare `generate_series` (A-tvf-bare-series).
4. **Cost model (Track B)** — B9h index-choice sub-items, then B9b window EQP
   (blocked on B9h); B1b selectivity-driven join order.
5. **D2e FTS5 tails** (D2e-1/2/3) and **D6 wasm** (pending the D6-0 architecture
   decision).

**Blocked by project constraints** (not effort): **D7** C-API shim (needs `unsafe`;
a sibling crate that opts out); **dbpage-2 INSERT-grow** (would write a deliberately
malformed file, breaking the pager consistency invariant); **B9j** collation-aware
index selection and **B1c** RIGHT/FULL inner seeks (correct via materialization —
plan/perf only); the Track E unqualified cross-db name residual.
