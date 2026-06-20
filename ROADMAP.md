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

## 3. Foundation — Phases 0–9 ✅ *(done)*

The layered foundation and a broad SQL engine are complete and differentially
verified against `sqlite3` (1658-query corpus plus ~25 focused suites). Detailed
history lives in `CHANGELOG.md` and the git log; in summary, graphitesql today:

**Reads & writes real SQLite files.** Opens databases written by `sqlite3`
(including WAL-mode), and **creates** databases — `CREATE TABLE`/`INDEX`/`VIEW`/
`TRIGGER`, `INSERT`/`UPDATE`/`DELETE`, transactions — whose files the real
`sqlite3` opens with `PRAGMA integrity_check = ok`. Storage covers rowid and
**`WITHOUT ROWID`** tables, automatic + secondary + `UNIQUE` indexes (incl. the
implicit `sqlite_autoindex_*`), overflow pages, the freelist with **page merging
on delete**, real **`VACUUM`** compaction, and the **WAL read *and* write** path
(`journal_mode=WAL`, frame append, `wal_checkpoint`).

**Runs a broad SQL dialect.** `SELECT` with `WHERE`/`GROUP BY`/`HAVING`/
`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`; `INNER`/`LEFT`/cross/comma **joins**;
compound queries (`UNION`/`INTERSECT`/`EXCEPT`); (recursive) **CTEs**;
**correlated subqueries**, `[NOT] EXISTS`, and **derived tables**; views & CTEs
as join sources; **window functions** with explicit `ROWS`/`RANGE`/`GROUPS`
frames; a core of scalar + aggregate functions, **date/time** + `printf`;
**type affinity** and SQLite-exact `%!.15g` real formatting; `EXPLAIN QUERY
PLAN`; an index-driven equality planner; **constraint enforcement** (`NOT NULL`,
`CHECK`, `UNIQUE`/`PK`, **foreign keys** with all referential actions); and
**triggers** (`BEFORE`/`AFTER`/`INSTEAD OF`, recursive).

What remains is breadth and depth toward full SQLite parity, below.

---

## 4. Forward plan — closing the gap with SQLite

Four tracks, each independently shippable and individually differential-tested.
Order within a track is roughly by value/effort; tracks can progress in parallel.

### Track A — SQL language & functions breadth

Make the dialect complete. Each item lands with a differential corpus addition.

- ✅ **`CREATE TABLE … AS SELECT`** — columns from the query's labels, populated
  with its rows.
- ✅ **Outer joins** — `LEFT`/`RIGHT`/`FULL [OUTER] JOIN` (nested-loop, with
  unmatched-side NULL fill).
- ✅ **Generated columns** — `… AS (expr) [STORED|VIRTUAL]`: modeled in the AST,
  `VIRTUAL` computed on read, `STORED` materialized on write, writes rejected,
  indexable. *Ref:* `build.c`, expression eval.
- ✅ **Collations** — `COLLATE NOCASE`/`RTRIM`/`BINARY` (and column/index
  `COLLATE`) honored in comparisons, `ORDER BY`, `GROUP BY`, `DISTINCT`, `UNIQUE`
  enforcement, and index key ordering/seek. *Ref:* `vdbeaux.c`, `callback.c`.
- ✅ **UPSERT** — `INSERT … ON CONFLICT [(cols)] DO UPDATE SET … [WHERE …]` and
  `DO NOTHING`, with the `excluded` pseudo-table.
- ✅ **`RETURNING`** — for `INSERT`/`UPDATE`/`DELETE` (`Connection::execute_returning`).
- ✅ **Row values** — `(a, b) = (c, d)`, `(a,b) < (c,d)`, `(a, b) IN ((…),(…))`,
  `(a,b) IN (SELECT …)`, and `VALUES` as a standalone statement / table source.
- ✅ **`ORDER BY` modifiers** — `NULLS FIRST`/`NULLS LAST`; `IS [NOT] DISTINCT FROM`.
- **Aggregate/window extras** — ✅ `FILTER (WHERE …)` on aggregates, ✅ the
  `WINDOW name AS (…)` clause with named-window reuse, ✅ `percent_rank`/
  `cume_dist`, ✅ ordered aggregates (`group_concat(x ORDER BY y)`). *Remaining:*
  frame `EXCLUDE`, `FILTER` on window functions, `count(DISTINCT …)` over windows.
- **Function library** — ✅ math functions (`sqrt`, `pow`, `ceil`, `floor`,
  `ln`/`log`, trig, …, pure-`core`, no libm) and ✅ **JSON** functions (`json`,
  `json_extract`, `json_array`/`json_object`, `json_type`, `json_array_length`,
  `json_valid`, `json_quote`, the `->`/`->>` operators, and the
  `json_set`/`json_insert`/`json_replace`/`json_remove`/`json_patch` mutators,
  and the `json_each`/`json_tree` table-valued functions), plus `LIKE … ESCAPE`,
  the `like()` function form, and `likely`/`unlikely`/`likelihood`. *Remaining:* a
  few string/blob built-ins. *Ref:* `func.c`, `math.c`, `json.c`.
- **Indexing breadth** — ✅ partial indexes (`CREATE INDEX … WHERE`), ✅
  expression indexes (`CREATE INDEX … (lower(x))`), ✅ `INDEXED BY` / `NOT INDEXED`
  hints. *Remaining:* `DESC` index columns honored in seeks, partial/expression-
  index use in the planner (currently scan-only), UNIQUE expression indexes.

### Track B — Query planner, statistics & the VDBE

Move from "iterator executor + equality index seek" to a cost-based planner, and
introduce a bytecode IR so `EXPLAIN` is real and the planner is testable.

- ✅ **`ANALYZE` + `sqlite_stat1`** — gather and store row/selectivity statistics
  (`nRow avgEqK` format, byte-compatible), read back when planning. *Remaining:*
  `sqlite_stat4` histograms. *Ref:* `analyze.c`.
- **Cost-based planning** — ✅ statistics now drive index *choice*; *remaining:*
  **join order** (today joins run in `FROM` order as nested loops); range scans
  (`<`/`>`/`BETWEEN`) and `IN`-list driven seeks; the **OR-by-union**
  optimization; covering-index detection; auto-indexes for unindexed joins;
  skip-scan. *Ref:* `where.c`, `wherecode.c`, `whereexpr.c`.
- **VDBE bytecode IR** — compile the AST to register-machine bytecode and run it,
  replacing the tree-walker incrementally (same results). This is the enabler for
  **real `EXPLAIN`** (the `addr/opcode/p1…` listing) being byte-comparable to
  SQLite's, and for query flattening / subquery co-routines. *Ref:* `vdbe.c`,
  `vdbeaux.c`, `opcodes.h`.
- **`EXPLAIN` (bytecode)** — currently `Error::Unsupported`; lands with the VDBE.

### Track C — Storage engine, transactions & **concurrency**

- **Concurrency model** *(committed — we will support it).* Progress:
  - ✅ the **`Vfs` locking contract** (`SHARED`/`RESERVED`/`PENDING`/`EXCLUSIVE`)
    as a `LockState` machine, shared per-path across handles in `MemoryVfs`/
    `StdVfs` (process-local), with the write pager taking write-intent → exclusive
    and a second writer getting `Error::Busy`;
  - ✅ **rollback-journal writer serialization** — one writer at a time, readers
    isolated via per-connection buffering;
  - *remaining:* **reader `SHARED`-lock enforcement on the read path**; an
    **OS-file lock** implementation (needs `std::fs::File::lock`, MSRV ≥ 1.89, or
    a host VFS) for true cross-process locking;
  - *remaining:* the **WAL `-shm` wal-index** (shared-memory hash index) and WAL
    locking protocol for multi-reader-with-writer concurrency and safe checkpoint;
  - *remaining:* a **thread-safe `Connection`/shared-pager** with a documented
    threading model.
  *Ref:* `wal.c` (`walIndex*`), `os_unix.c` (locking), `pager.c`.
- **`auto_vacuum`** (full + incremental) — pointer-map (ptrmap) pages so the file
  can shrink on commit; `PRAGMA incremental_vacuum`. *Ref:* `btree.c` (ptrmap).
- **`secure_delete`**, `PRAGMA cache_size`/`mmap_size`, a real page cache
  (`pcache`) for read performance.
- ✅ **`SAVEPOINT` / `RELEASE` / `ROLLBACK TO`** — nested transactions via staged
  overlay snapshots in the write pager. *Ref:* `pager.c` (savepoints).
- **`ATTACH` / `DETACH`** — multiple database schemas in one connection, with
  cross-database queries and the `main`/`temp`/attached namespaces; **TEMP**
  tables/indexes/triggers.
- **SQLite-format rollback journal** — match the on-disk journal byte layout
  (ours is a private, recoverable format today) so a crashed graphitesql write is
  recoverable by `sqlite3` too.
- ✅ introspection PRAGMAs: `index_list`, `index_info`, `foreign_key_list`,
  `foreign_key_check`, `integrity_check`/`quick_check` (in-engine), `freelist_count`,
  `application_id`, `data_version` (plus existing `table_info`, …). *Remaining:*
  the `pragma_*` table-valued functions.

### Track D — Phase 10: ecosystem & extensions *(post-1.0, behind features)*

Each is opt-in and outside the core compatibility promise; several build on the
VDBE (Track B) and virtual tables.

- **C-API shim** — a `libsqlite3`-compatible surface (`sqlite3_open`/`prepare`/
  `step`/`column_*`/`bind_*`/…) as a separate crate, so existing C/FFI consumers
  link against graphitesql. *Ref:* `main.c`, `legacy.c`, `vdbeapi.c`.
- **Virtual tables & table-valued functions** — ✅ a TVF mechanism with
  `generate_series`, `json_each`, and `json_tree` as `FROM` sources. *Remaining:*
  the `sqlite3_module` analog (`xBestIndex`/`xFilter`/…) and `CREATE VIRTUAL
  TABLE`. Foundation for FTS5/R-Tree. *Ref:* `vtab.c`, `vdbevtab.c`.
- **FTS5** full-text search; **R-Tree** spatial index. *Ref:* `fts5*.c`, `rtree.c`.
- **User-defined functions from Rust** — register scalar/aggregate/window funcs
  and custom collations through a safe API.
- **`sqlite3_session`** — changesets/patchsets for replication.
- **Async VFS for wasm** — non-blocking I/O so the engine runs in the browser
  over IndexedDB/OPFS.

---

## 5. Cross-cutting concerns

- **MSRV** is pinned at **1.88** (`Cargo.toml`); revisit before 1.0.
- **Numeric model** — reals are `f64` to match SQLite; no extended decimal/bignum.
- **Parser** stays hand-written (no build-time codegen, friendlier errors);
  `parse.y` remains the source of truth for precedence and accepted forms.
- **Performance** is deliberately secondary to correctness until the VDBE +
  planner land; the iterator executor is `O(n)` in places (e.g. some constraint
  and `WITHOUT ROWID` paths rebuild on write) that the planner work will revisit.

---

## 6. File-format compatibility & testing strategy

This is the project's whole reason to exist, so it gets first-class testing.

- **Differential tests.** Run the same SQL through both `sqlite3` and graphitesql
  and diff results; a large generated corpus (`tests/differential.rs`, 1658
  queries) plus a per-feature suite. Every new feature adds to one of these.
- **`integrity_check` as a gate.** Any database graphitesql writes must pass
  `sqlite3`'s `PRAGMA integrity_check` (and, with FKs on, `foreign_key_check`).
- **Round-trip & cross-engine.** graphitesql reads what `sqlite3` writes and vice
  versa, for every storage feature (rowid, `WITHOUT ROWID`, WAL, post-VACUUM).
- **Fuzzing** *(planned, expand)* — fuzz the readers with malformed pages (must
  return `Error::Corrupt`, never panic) and fuzz SQL parsing.
- **Crash-recovery** *(planned)* — a fault-injecting `Vfs` that truncates / fails
  at chosen fsync points, asserting recovery to a consistent state; pairs with
  the SQLite-format journal and concurrency work in Track C.
- **SQLite's own suite** *(planned)* — run a curated slice of SQLite's `test/`
  TCL assertions as an additional oracle.

### Known sources of legitimate file divergence

Two SQLite-compatible writers can produce different bytes for the same logical
content; we document and accept these rather than chase them: free-page reuse
order and exact balancing splits, `change_counter`/`version_valid_for` values,
the embedded `SQLITE_VERSION_NUMBER`, and unused/reserved bytes left from
deletions. **Compatibility means both engines read each other's files and agree
on contents**, not byte-identical independently-built databases.

---

## 7. Immediate next steps

Done so far: ✅ generated columns, ✅ collations, ✅ UPSERT + `RETURNING`,
✅ math + core JSON functions, ✅ `ANALYZE`/`sqlite_stat1` + stats-driven index
choice. A suggested order for what remains:

1. **The `Vfs` locking contract + rollback-journal concurrency** (Track C) — the
   first concrete step of the committed concurrency model, before the WAL `-shm`.
2. **VDBE IR spike** (Track B) — prototype compiling a simple `SELECT` to bytecode
   and running it, to de-risk the executor→VDBE migration and unlock real
   `EXPLAIN`.
3. **Range/`IN` driven index seeks + join order** (Track B) — extend the new
   cost-based chooser beyond equality prefixes.
4. **Row values & `ORDER BY` modifiers** (Track A) — `(a,b) IN (…)`,
   `NULLS FIRST/LAST`, `IS [NOT] DISTINCT FROM`.
