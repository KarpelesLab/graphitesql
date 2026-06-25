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
results) and is the bulk of Track B — it unblocks real `EXPLAIN` output and a
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
verified against `sqlite3` (a 1,600+ query corpus plus 170+ focused test suites).
Detailed history lives in `CHANGELOG.md`; in summary, graphitesql today:

**Reads & writes real SQLite files.** Opens `sqlite3`-written databases
(including WAL-mode) and **creates** databases whose files `sqlite3` opens with
`PRAGMA integrity_check = ok`. Storage covers rowid and **`WITHOUT ROWID`**
tables, automatic/secondary/`UNIQUE` indexes (incl. `sqlite_autoindex_*`),
overflow pages, the freelist with **page merging on delete**, real **`VACUUM`**
(+ `VACUUM … INTO`), the full **`auto_vacuum`** track (read, write, FULL
auto-truncate, INCREMENTAL reclaim), and the **WAL read *and* write** path
(`journal_mode=WAL`, `wal_checkpoint`).

**Runs a broad SQL dialect.** `SELECT` with `WHERE`/`GROUP BY`/`HAVING`/`ORDER
BY`/`LIMIT`/`OFFSET`/`DISTINCT` and SELECT-list aliases; `INNER`/`LEFT`/`RIGHT`/
`FULL`/cross/comma **joins** plus **`NATURAL`**/**`USING`** (column coalescing,
affinity- and collation-aware keys), nested-loop + hash join; compound queries
(`UNION`/`INTERSECT`/`EXCEPT`, collation-aware); (recursive) **CTEs**; correlated
subqueries, `[NOT] EXISTS`, `IN (SELECT)` with sqlite's candidate-column-affinity
rule, derived tables; views & CTEs as sources (a view column inheriting its base
column's affinity/collation); **window functions** (`ROWS`/`RANGE`/`GROUPS`,
`EXCLUDE`, `FILTER`, named windows, over `GROUP BY`/aggregates, NULL-correct RANGE
frames); `INSERT … SELECT`, `UPDATE … FROM`, `UPDATE OR …`, UPSERT, `RETURNING`,
row values, `STRICT` tables, generated columns; a broad scalar/aggregate library
incl. **date/time** (`strftime` incl. `subsec`/`%J`), `printf`/`format`,
`random`/`randomblob`, `unistr`/`subtype`, **JSON** + the **JSONB binary family**,
math (pure-`core`); **type affinity** and SQLite-exact real formatting; column
names matching sqlite's verbatim source spans; collation (`BINARY`/`NOCASE`/
`RTRIM`) honored across comparisons/`IN`/`BETWEEN`/`CASE`/`min`/`max`/set ops/
ordering keys; `EXPLAIN QUERY PLAN` with an index-driven planner (equality/range/
`IN`/OR-union seeks, the full inner-join + `WITHOUT ROWID` seek family,
automatic-index reporting, index-driven `ORDER BY` + covering reads, stats-driven
choice via `ANALYZE`/`sqlite_stat1`); constraint enforcement (`NOT NULL`, `CHECK`,
`UNIQUE`/`PK`, partial/expression UNIQUE indexes; **foreign keys** with affinity-
and collation-correct parent matching and the full referential-action set;
*deferred:* DEFERRABLE/INITIALLY DEFERRED); **triggers** (`BEFORE`/`AFTER`/
`INSTEAD OF`, `UPDATE OF`, `WHEN`, recursive, `NEW`/`OLD`); `SAVEPOINT` family;
**`ATTACH`/`DETACH`/`TEMP`** multi-schema; DDL with full CREATE-time + ALTER
validation (incl. error-parity rejection of what sqlite rejects, byte-exact
messages, and text-preserving schema edits); a writable/persistent **virtual
table** layer with the built-in **R-Tree**, **FTS5** (read + write, sqlite-
readable on disk), `dbstat`, and Rust scalar/aggregate **UDFs**; the schema
catalog queryable as `sqlite_schema`/`sqlite_master`, with the introspection
PRAGMAs and `pragma_*` TVFs.

What remains is breadth and depth toward full SQLite parity, below.

---

## 4. Forward plan — remaining work

Five tracks. Each lists a one-line "Done" pointer (detail in `CHANGELOG.md`) and
then **only the open pieces, broken into small, independently-shippable chunks**.
Every chunk lands with a differential test and keeps `master` green. Tracks can
progress in parallel.

### Track A — SQL language & functions  *(substantially complete)*

**Done:** the full SELECT/join/CTE/window/subquery surface; UPSERT/`RETURNING`/
row values/`STRICT`/generated columns/`UPDATE … FROM`/`INSERT … SELECT`; the
scalar/aggregate/date-time/`printf`/JSON/JSONB libraries; type affinity,
collation, column-name spans; the standing **error-parity** sweep (reject what
sqlite rejects, byte-exact messages); and cross-object **ALTER** propagation
(**A-rn1…A-rn4**: RENAME TABLE→views/FKs, RENAME COLUMN→FKs/views/triggers,
text-preserving CREATE-text edits).

**Remaining:**

- **A-rn3-edge — RENAME COLUMN in genuinely multi-table view/trigger bodies.**
  The token rewrite bails (leaves the body unchanged — never corrupts) on a bare
  column ref that is ambiguous across multiple base sources, because the AST has
  no per-column-ref source span.
  - **A-rn3-edge-1** — add a source span (byte range) to `Expr::Column`.
  - **A-rn3-edge-2** — use the span for scope-aware rename (resolve each bare ref
    to its owning table, rewrite only the matching ones).
- **Error-parity (standing).** No known leftovers; the sweep continues against new
  construct families and files any new divergence here.

### Track B — Query planner, statistics & the VDBE

**Done:** `ANALYZE`/`sqlite_stat1` stats-driven planning; the full equality/range/
`IN`/OR-union + inner-join + `WITHOUT ROWID` seek family; hash join + automatic-
index EQP; index-driven `ORDER BY` and covering reads (**B0/B0b**); the VDBE spike
+ its scalar-expression compiler; the **VDBE join family** (**B5a**); **VDBE
routing default-on** (**B7a/B7b**, ~93 % of the corpus, tree-walker is the
fallback oracle); bytecode `EXPLAIN` (**B8**); the router's **non-correlated
scalar/`EXISTS` subquery fold** pre-pass (folds such subqueries — in the
projection, `WHERE`/`HAVING`/`GROUP BY`/`ORDER BY`/join-`ON`, **and now
`LIMIT`/`OFFSET`** — to constants so the VDBE runs the rest; never changes a
result).

**Remaining — move each remaining shape onto the VDBE.** Additive, gated on
VDBE-vs-tree-walker parity; results are already correct via the fallback, so this
is *perf/coverage*, not correctness.

- **B5c-1 — `IN (SELECT …)` on the VDBE** (it applies the candidate *column's*
  affinity, unlike `IN (list)` — the tree-walker semantics are already shipped).
  - *Done (fold subset):* a non-correlated `IN (SELECT <computed column>)` is
    materialized and folded to `IN (list)` by the router pre-pass, so it runs on
    the VDBE. Safe only because a *computed* candidate carries NONE affinity /
    BINARY collation, making the two forms equivalent (verified vs sqlite). The
    **bare-column** candidate is the remaining real work — it needs the VDBE to
    apply the candidate column's affinity natively (it can't fold without
    changing results):
  - **B5c-1a** — evaluate the candidate set + its column affinity
    (`Subqueries::column_affinity`) into registers.
  - **B5c-1b** — a membership op applying `apply_comparison_affinity(left_aff,
    candidate_col_aff)` under the left collation.
  - **B5c-1c** — `NOT IN` + NULL-in-set semantics (a NULL candidate → NULL result).
- **B5c-2 — correlated subqueries on the VDBE.**
  - **B5c-2a** — thread the outer row into the subquery's register frame.
  - **B5c-2b** — compile `Subquery`/`Exists`/`InSelect` that read an outer column.
- **B5c-3 — compound `SELECT` (`UNION`/`INTERSECT`/`EXCEPT`) as one program.**
  - **B5c-3a** — compile each arm into the same program.
  - **B5c-3b** — the set-combine + dedup/sort step (collation-aware).
- **B5c-4 — window functions on the VDBE.**
- **B5b — per-cursor nested-loop join + inner seek** (stream the inner side with
  `OpenRead`/`Rewind`/`Column`/`Next` instead of materializing the cross-product;
  *perf-only*).
  - **B5b-1** — cursor opcodes for the inner table.
  - **B5b-2** — seek-driven inner cursor (rowid/PK/index), mirroring the
    tree-walker's inner-join seeks.
- **B1c — RIGHT/FULL join inner seeks** (INNER/LEFT seek; RIGHT/FULL still
  materialize the inner table).

**Blocked / deferred by design:**
- **B1b — cost-based join reordering.** graphite's per-cursor seek/bloom-filter
  choices diverge from sqlite's cost-reordered plain scans *by design*; matching
  the EQP would mean abandoning often-cheaper access paths. Results already correct.
- **B4 — `sqlite_stat4` histograms.** The pinned `sqlite3 3.50.4` oracle is built
  without `SQLITE_ENABLE_STAT4`, so there is nothing to diff against and a
  stat4-driven planner would *diverge* the EQP corpus. Needs a stat4-enabled oracle.

### Track C — Storage engine, transactions, concurrency

**Done:** the `Vfs` locking contract; rollback-journal writer serialization;
`SAVEPOINT`; the introspection PRAGMAs; the **entire multi-schema track** (C1–C5,
`ATTACH`/`DETACH`/`TEMP`); the **whole `auto_vacuum` track** (C6); `VACUUM …
INTO`; `secure_delete` (C8a); `cache_size`/`mmap_size` reporting (C8b).

**Remaining:**

- **C7 — SQLite-format rollback journal** (graphite's journal is a private,
  recoverable format today; match the on-disk byte layout so a crash mid-write is
  recoverable by `sqlite3`).
  - **C7a** — write the sqlite journal header + page records.
  - **C7b** — recover from a sqlite-format journal on open.
  - **C7-harness** — a fault-injecting `Vfs` (truncate / fail at chosen fsync
    points) asserting recovery to a consistent state (the §6 crash-recovery
    harness; lands with C7).
- **C8c — bounded `pcache` with LRU eviction** (honor the C8b `cache_size`;
  replaces the keep-everything page map). *Perf, not correctness.*
  - **C8c-1** — a page-cache abstraction with a capacity + eviction hook.
  - **C8c-2** — wire the pager through it; evict clean pages under pressure.
- **C9 — concurrency** (each independent):
  - **C9a** — reader `SHARED`-lock sharing (multiple readers, writer excluded).
  - **C9b** — OS-level cross-process file locks (`std::fs::File::lock`; wants MSRV
    1.89) behind the std VFS.
  - **C9c** — the WAL `-shm` wal-index for multi-connection WAL readers.
  - **C9d** — a thread-safe `Connection` (`Send`/`Sync`, or a documented
    per-thread model).

### Track D — Virtual tables & ecosystem extensions

**Done:** the read-only vtab foundation (TVFs, the `VTabModule`/`VTabRegistry`
trait, `best_index`/`filter` pushdown — **D1**); the **writable, persistent** vtab
layer (**W1/W2**); both headline modules — the full **R-Tree** (**D3a–D3c**,
byte-compatible nodes) and the full **FTS5** (**D2a–D2e**, read + write,
sqlite-readable on disk, with the `tokenize=` option chain + diacritic folding);
Rust scalar/aggregate **UDFs** (**D4**); the **`dbstat`** vtab.

**Remaining:**

- **D2b — a real FTS5 inverted index.** Today `MATCH` scans `_content`; the
  on-disk segment *format* is already byte-compatible (written by
  `fts5_rebuild_index`), so this is the *read* path that uses the index for scale.
  - **D2b-1** — *Done:* decode the `%_data` segment index (leaf header, page-index
    footer, prefix-compressed term keys, doclists, position lists) for a
    single-term lookup — `decode_term` in `src/fts5_index.rs`, the byte-inverse of
    the writer. Single-leaf segments only; a spanning/interior/doclist-index
    segment returns `None` so the caller falls back to the `_content` scan.
    Verified by writer→decoder round-trips and against `sqlite3`-written leaves
    (`tests/fts5_decode.rs`).
  - **D2b-2** — route `MATCH` through the index (`decode_term`) instead of
    scanning every document; keep the `_content` scan as the fallback (and for the
    multi-leaf/interior cases `decode_term` returns `None` on).
  - **D2b-3** — extend the decoder to spanning doclists + interior /
    doclist-index pages (the multi-leaf cases `D2b-1` leaves on the scan).
- **D2e-encoder — byte-identical FTS5 at large scale** (structural validity holds
  today; these only affect exact-byte parity past a few leaves, and each needs the
  fts5 writer source for the precise split heuristic): the combined
  spanning-doclist-then-paginated-terms leaf-fill boundary; doclist-index (`dli`)
  pages; segment-b-tree interior (`height > 0`) `_data` pages.
- **dbpage — the raw-page vtab** (`sqlite_dbpage`, sibling of `dbstat`).
  Done: **dbpage-1** — read (one row per page: `pgno`, `data`), byte-exact vs
  `sqlite3` on the same file (`tests/dbpage.rs`); both eponymous read-only vtabs
  (`dbstat`, `sqlite_dbpage`) also resolve a `main.`-qualifier, answer
  `PRAGMA table_info`/`table_xinfo` with their fixed column shape (incl. the
  trailing hidden columns). Any schema qualifier (`main.`/`temp.`/`<attached>.`)
  resolves the eponymous tables and — matching SQLite's hidden `schema` column
  default of `main` — reports the **main** database regardless of the qualifier
  (so `aux.dbstat`/`temp.dbstat` report main, not the qualified db). A
  `temp.`-qualified *non-eponymous* read with no temp database now reports the
  name as missing instead of panicking.
  - *Deferred:* a `WHERE schema='aux'` constraint to redirect the report to a
    non-main database (SQLite drives this through the hidden `schema` column;
    graphite has no hidden-column pushdown for it yet). Also unmatched: SQLite
    quirkily reports `main` even for an *unknown* schema qualifier
    (`nope.dbstat`), where graphite errors `unknown database nope`.
  - **dbpage-2** — write (raw page replacement). *Oracle-blocked:* the pinned
    `sqlite3 3.50.4` alt1 build was compiled without the writable-dbpage path —
    every real page write returns `read-only` (deterministically; see §6). With
    no engine to diff against, a writable `sqlite_dbpage` can't satisfy the
    differential-test rule; deferred until a writable-dbpage oracle is available.
- **D4-leftover — window UDFs + custom collations** (the latter needs a user
  variant on the `Collation` enum — invasive).
- **D5 — `sqlite3_session`** changesets/patchsets for replication.
- **D6 — async VFS for wasm** (non-blocking IndexedDB/OPFS I/O).

**Blocked by design:**
- **D7 — C-API shim** (`libsqlite3`-compatible surface). Needs `extern "C"` + raw
  pointers, incompatible with `#![forbid(unsafe_code)]`; would live in a sibling
  crate that opts out.

### Track E — Cross-database write resolution  *(essentially complete)*

**Done:** a write to an attached/`temp` database swaps that database in as the
active `main` for the whole statement, so a subquery/source reading the *original*
main must still resolve there. **E0** built the regression oracle
(`tests/cross_db_writes.rs`, deterministic in-memory ATTACH). **`INSERT … SELECT`**
and **`INSERT … VALUES ((SELECT …))`** are materialized in the original context
before the swap (`prematerialize_insert_source`). **E1/E2/E3 + E-arch-a:**
`unqualified_db` now resolves an unqualified name `main`-first and then falls back
to attached databases (SQLite's `main → temp → attached` order) — which also lets
a cross-db `UPDATE/DELETE aux.t …` subquery resolve a `main` table (the original
`main` is in the target's swapped-out attached slot), and fixes a top-level
`SELECT … FROM s` where `s` lives only in an attached database.

**Remaining (rare residual):** a table name present in **both** the active db and
an attached one, referenced *unqualified inside a cross-database write*, binds to
the active db (graphite) rather than `main` (sqlite). Realistic schemas qualify
such references. A full fix would require dropping the read-side swap entirely
(resolve the write target by qualifier, reads by the global order) — deferred
until something needs byte-exact parity for that edge.

---

## 5. Cross-cutting concerns

- **MSRV** is pinned at **1.88** (`Cargo.toml`); revisit before 1.0 (C9b wants
  1.89 for `File::lock`).
- **Numeric model** — reals are `f64` to match SQLite; no extended decimal/bignum.
- **Parser** stays hand-written (no build-time codegen, friendlier errors);
  `parse.y` remains the source of truth for precedence and accepted forms.
- **Performance** is deliberately secondary to correctness until the VDBE +
  planner work lands; the iterator executor is `O(n)` in places (some constraint
  and `WITHOUT ROWID` paths rebuild on write) that Track B/C will revisit.

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
  by probing each semantic dimension against the `sqlite3` CLI. *(Note: the corpus
  does NOT exercise cross-database writes — see Track E's E0.)*
- **Fuzzing** — a deterministic corruption-robustness harness
  (`tests/fuzz_corruption.rs`, ~50k malformed-file variants; `tests/fuzz_sql.rs`,
  ~3.3k malformed/deeply-nested SQL) asserts the readers return an error and never
  panic. *(Expand toward a coverage-guided fuzzer when a no-dep path exists.)*
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

### Build-specific oracle behaviors we intentionally do NOT match

The pinned `sqlite3 3.50.4` oracle is a custom (`alt1`) build whose behavior
differs from *documented* / standard SQLite in two places; graphite matches the
standard, not the build:

- **`LIKE` case folding.** The oracle folds full Unicode in `LIKE` (`'É' LIKE
  'é'` → true) via the C library's locale-specific `towlower` — per-codepoint and
  not replicable byte-for-byte (`ẞ`→`ß` not `ss`, `Σ` matches both `σ`/`ς`).
  graphite's `LIKE` is ASCII-only case-insensitive, like documented SQLite, and
  this is pinned (`tests/like_escape.rs`).
- **`utc`/`localtime` date modifiers** are timezone-dependent (the host/build TZ)
  and graphite's TZ support is feature-gated, so their results are environment-,
  not graphite-, specific.
- **Writable `sqlite_dbpage`.** The alt1 build refuses every page write with a
  `read-only` runtime error (no `SQLITE_ENABLE_DBPAGE_VTAB` write path), so there
  is no oracle to diff a writable `sqlite_dbpage` against — `dbpage-2` is parked
  on this, not on implementation effort (read-side `dbpage-1` is done).
- **`PRAGMA function_list` / `collation_list`.** The alt1 oracle enumerates *its
  own* loaded extensions (functions like `sha3`/`geopoly`/`zipfile`/`decimal`,
  collations `decimal`/`uint`), so neither result set is reproducible by a
  zero-dependency engine. graphite reports only its built-in collations
  (`BINARY`/`NOCASE`/`RTRIM`, reverse-registration order) and does not implement
  `function_list`. Result-value semantics that *are* build-independent are pinned
  in `tests/value_semantics_diff.rs`.

---

## 7. Suggested order

The headline features are done (§3), and **Track E is essentially complete**; what
remains (§4) is bigger, multi-session work, each track independently shippable. A
reasonable order:

1. **B5c-1 → B5c-4, then B5b** — VDBE depth (Track B): move `IN (SELECT)`,
   correlated subqueries, compound, and windows onto the VDBE, then the
   per-cursor streaming join. Perf/coverage; parity-gated, low risk.
2. **D2b** — the real FTS5 inverted index (Track D): the one scaling gap in an
   otherwise-complete module.
3. **C7a/C7b + crash-recovery harness** — the SQLite-format journal (Track C):
   durability depth; pairs with the fault-injecting VFS.
4. **C8c, then C9a–C9d** — bounded pcache, then the concurrency story (Track C).
5. **dbpage, D5, D6** — ecosystem surfaces (Track D).

**Deferred / blocked** (documented in §4): **B1b** join reordering and **B4**
`sqlite_stat4` (diverge from / unverifiable against the stat1-only oracle);
**B1c** RIGHT/FULL inner seeks (correct via materialization); **D7** the C-API
shim (needs `unsafe`; a sibling crate); the **A-rn3-edge** ambiguous-ref case
(needs per-column-ref spans); and the FTS5 large-scale encoder sub-cases (need the
fts5 writer source). Build-specific oracle quirks we intentionally do NOT match
are recorded in §6.
