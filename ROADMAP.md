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
verified against `sqlite3` (1658-query corpus plus ~45 focused suites). Detailed
history lives in `CHANGELOG.md`; in summary, graphitesql today:

**Reads & writes real SQLite files.** Opens `sqlite3`-written databases
(including WAL-mode) and **creates** databases whose files `sqlite3` opens with
`PRAGMA integrity_check = ok`. Storage covers rowid and **`WITHOUT ROWID`**
tables, automatic/secondary/`UNIQUE` indexes (incl. `sqlite_autoindex_*`),
overflow pages, the freelist with **page merging on delete**, real **`VACUUM`**,
and the **WAL read *and* write** path (`journal_mode=WAL`, `wal_checkpoint`).

**Runs a broad SQL dialect.** `SELECT` with `WHERE`/`GROUP BY`/`HAVING` (incl.
without `GROUP BY`)/`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT` and SELECT-list
aliases resolved in WHERE/GROUP BY/HAVING; `INNER`/`LEFT`/`RIGHT`/`FULL`/cross/
comma **joins** (nested-loop + a hash join for equi-joins); compound queries
(`UNION`/`INTERSECT`/`EXCEPT`, collation-aware); (recursive) **CTEs** with
`LIMIT`; correlated subqueries, `[NOT] EXISTS`, derived tables; views & CTEs as
sources; **window functions** (`ROWS`/`RANGE`/`GROUPS`, `EXCLUDE`, `FILTER`,
named windows); `INSERT … SELECT`, `UPDATE … FROM`, UPSERT, `RETURNING`, row
values, `STRICT` tables, generated columns; a broad scalar/aggregate function
library incl. **date/time**, `printf`/`format`, **JSON** (`json_*`,
`json_group_array`/`object`, `json_each`/`json_tree`), math (pure-`core`);
**type affinity** and SQLite-exact real formatting; collation
(`BINARY`/`NOCASE`/`RTRIM`) honored across comparisons, `IN`/`BETWEEN`/`CASE`,
`min`/`max`, set ops, `ORDER BY`/`GROUP BY`/`DISTINCT`/`UNIQUE`/index keys;
`EXPLAIN QUERY PLAN`; an index-driven planner (equality/range/`IN`/OR-union
seeks, stats-driven choice via `ANALYZE`/`sqlite_stat1`); constraint enforcement
(`NOT NULL`, `CHECK`, `UNIQUE`/`PK`, **foreign keys** with all actions,
standalone/partial/expression UNIQUE indexes); **triggers**
(`BEFORE`/`AFTER`/`INSTEAD OF`, `UPDATE OF`, `WHEN`, recursive, `NEW`/`OLD`
incl. rowid); `SAVEPOINT`/`RELEASE`/`ROLLBACK TO`; DDL with full CREATE-time and
ALTER validation; the schema catalog queryable as `sqlite_schema`/`sqlite_master`.

What remains is breadth and depth toward full SQLite parity, below.

---

## 4. Forward plan — closing the gap with SQLite

Four tracks. Completed work is summarized; **remaining work is broken into
numbered, independently-shippable pieces** (each one lands with a differential
test and keeps `master` green). Tracks can progress in parallel.

### Track A — SQL language & functions breadth  *(substantially complete)*

Done: outer joins, generated columns, collations, UPSERT, `RETURNING`, row
values, `ORDER BY` modifiers (`NULLS FIRST/LAST`, `IS [NOT] DISTINCT FROM`),
`STRICT` tables, `CREATE TABLE … AS SELECT`, `INSERT … SELECT`,
`UPDATE … SET … FROM`, `*`/`table.*` with aggregates, HAVING without GROUP BY,
SELECT-list aliases in WHERE/GROUP BY/HAVING, the window-function suite, the
math + JSON + `printf` libraries (incl. `json_group_*`, `string_agg`, the `,`
printf flag), partial/expression/UNIQUE-index breadth, and full DDL validation.

**Remaining pieces:**

- **A1 — `randomblob()` / `random()` / `zeroblob` edges.** *(intentionally
  deferred: non-deterministic, untestable differentially without an RNG; revisit
  if a seedable RNG lands.)*
- **A2 — DESC index columns honored in seeks.** Currently a DESC index yields
  correct results by scan/superset; teach the seek paths to walk a DESC b-tree
  in the right direction. *Perf-only; verify via `EXPLAIN QUERY PLAN`.*
- **A3 — partial/expression index use in the planner.** The planner currently
  scans for these; let `leading_index_for`/`eqp_access` consider them when the
  query's `WHERE` implies the partial predicate / matches the expression.
- **A4 — literal-left collation fallback.** `'x' IN (SELECT nocase_col)` and a
  literal compared to a collated subquery column should use the *right* side's
  collation when the left has none; and window-frame `min`/`max` collation.

### Track B — Query planner, statistics & the VDBE

Done: `ANALYZE` + `sqlite_stat1` (byte-compatible) with stats-driven index
choice; range/`IN`/OR-union seeks; a hash join for equi-joins; the VDBE spike
(`exec::vdbe`) covering constant projections, single-table scan + `WHERE` +
`ORDER BY` + `DISTINCT` + `LIMIT`/`OFFSET`, whole-table aggregates, and
single-table `GROUP BY`, all matching the tree-walker via `query_vdbe`.

**Remaining pieces:**

- **B1 — Join order.** Reorder `FROM` tables by a simple cost model (smallest
  estimated cardinality / most-selective indexed table first) instead of textual
  order; keep results identical, verify the chosen order via `EXPLAIN QUERY
  PLAN`. *Ref:* `where.c`.
- **B2 — Covering-index detection.** When all referenced columns of a table are
  in a chosen index, read from the index without touching the table b-tree; mark
  it in `EXPLAIN QUERY PLAN` (`USING COVERING INDEX`).
- **B3 — Automatic indexes for unindexed joins.** Build a transient hash/sorted
  index on a join's inner table when no usable index exists (the `auto-index`
  optimization), reported as `USING AUTOMATIC … INDEX`.
- **B4 — `sqlite_stat4` histograms.** Extend `ANALYZE` to gather per-index
  sample histograms (byte-compatible `sqlite_stat4` rows) and use them for range
  selectivity. *Ref:* `analyze.c`.
- **B5 — VDBE: joins.** Add nested-loop join opcodes (`OpenRead`/`Rewind`/
  `Column`/`Next` per cursor with nested loops) so a two-table join runs on the
  register machine alongside the tree-walker.
- **B6 — VDBE: `HAVING` + aggregate `ORDER BY` on the grouped path.**
- **B7 — VDBE: become the execution path.** Migrate `Connection::query` onto the
  VDBE behind a flag, then by default, keeping the differential corpus green.
- **B8 — Real `EXPLAIN` (bytecode).** Emit the `addr|opcode|p1|p2|p3|p4|p5`
  listing from a compiled `Program`; currently `Error::Unsupported`. *Ref:*
  `vdbe.c`, `opcodes.h`.

### Track C — Storage engine, transactions, concurrency & multi-schema

Done: the `Vfs` locking contract (`SHARED`/`RESERVED`/`PENDING`/`EXCLUSIVE`,
process-local), rollback-journal writer serialization, `SAVEPOINT` family,
transaction-state validation, and the introspection PRAGMAs (`index_list`,
`index_info`, `foreign_key_list`/`_check`, `integrity_check`/`quick_check`,
`freelist_count`, `application_id`, `data_version`, the `pragma_*` TVFs).

**Remaining pieces:**

*Multi-schema (`ATTACH`/`DETACH`/`TEMP`):*

- ✅ **C1 — Multi-database registry + `PRAGMA database_list`.** `Connection`
  holds `main` (the existing fields) plus an attached-database list; each
  attached db has its own `Backend` + `Schema`.
- ✅ **C2 — `ATTACH ':memory:' AS x` / `DETACH x`.** In-memory attachments;
  SQLite-exact name validation; `database_list` shows them at seq 2+ (seq 1
  reserved for temp).
- ✅ **C3 — Schema-qualified names `x.table`.** Reads materialize a qualified
  single table through its own backend; writes (`CREATE`/`INSERT`/`UPDATE`/
  `DELETE`/`DROP … aux.t`) temporarily swap the attached db in as the active
  `main` (a single write touches one database). Databases are isolated, matching
  sqlite3. Cross-database **joins** now work too — each join source is
  materialized through its own backend, and 3-part column names
  (`aux.tbl.col`) parse. `WITHOUT ROWID` tables read cross-db too (sole
  source and join source). *Remaining within C3: qualified `ALTER`.*
- ✅ **C4 — `TEMP` tables.** A lazily-created in-memory `temp` database (seq 1);
  `CREATE TEMP TABLE` targets it (modeled as a `schema = "temp"` qualifier);
  unqualified names resolve `temp`→`main` (a temp table shadows main);
  `sqlite_temp_master`/`sqlite_temp_schema`/`temp.sqlite_master` read the temp
  catalog; TEMP no longer persists to a file database. *Remaining: `CREATE TEMP
  INDEX/VIEW/TRIGGER` still target main.*
- ✅ **C5 — `ATTACH 'file.db' AS x`.** Opens a real file (std file VFS) as an
  attached database — creating an empty one if absent, else opening the existing
  file (rollback-journal mode so commits are immediately sqlite3-readable).
  Cross-engine verified both directions. Also fixed: a qualified `CREATE TABLE
  aux.t` now stores its CREATE bare-named in the target catalog (the `aux.`
  prefix is invalid in that database's namespace). *Remaining: cross-database
  transactions spanning attached files (each commits independently today).*

**The ATTACH/DETACH/TEMP multi-schema track (C1–C5) is complete** for in-memory
and file databases, including cross-database joins. Remaining multi-schema
refinements: qualified `ALTER`/`CREATE INDEX|VIEW|TRIGGER`, and cross-database
transactions.

*Storage:*

- **C6 — `auto_vacuum` (full + incremental) + ptrmap pages.** Implement
  pointer-map pages so the file can shrink: track each page's parent, move pages
  on free, truncate on commit (`auto_vacuum=FULL`) or via `PRAGMA
  incremental_vacuum` (`auto_vacuum=INCREMENTAL`). Header `auto_vacuum` flag set
  at create. Verify `sqlite3` reads the result with `integrity_check = ok`.
  *Ref:* `btree.c` (ptrmap).
- **C7 — SQLite-format rollback journal.** Match the on-disk journal byte layout
  (ours is a private, recoverable format today) so a crashed graphitesql write is
  recoverable by `sqlite3`. Pairs with the crash-recovery test harness (§6).
- **C8 — `secure_delete`; `PRAGMA cache_size`/`mmap_size`; a real `pcache`.**
- **C9 — Reader `SHARED`-lock enforcement; OS-file locks; WAL `-shm` wal-index;
  thread-safe `Connection`.** The remaining concurrency pieces (cross-process and
  multi-reader-with-writer); OS-file locks need `std::fs::File::lock` (MSRV bump)
  or a host VFS.

### Track D — Virtual tables & ecosystem extensions

Done: a table-valued-function mechanism (`generate_series`, `json_each`,
`json_tree` as `FROM` sources).

**Remaining pieces:**

- **D1 — `sqlite3_module` analog + `CREATE VIRTUAL TABLE`.** A safe Rust trait
  for virtual-table modules (`connect`/`best_index`/`filter`/`next`/`column`/
  `rowid`), a module registry, `CREATE VIRTUAL TABLE … USING module(args)`
  persisted in `sqlite_schema`, and the executor treating a vtab as a `FROM`
  source via the trait. Foundation for D2–D3. *Ref:* `vtab.c`, `vdbevtab.c`.
- **D2 — FTS5** full-text search (as a module on D1). *Ref:* `fts5*.c`.
- **D3 — R-Tree** spatial index (as a module on D1). *Ref:* `rtree.c`.
- **D4 — User-defined functions from Rust** — register scalar/aggregate/window
  funcs and custom collations through a safe API.
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

The breadth tracks (A) are substantially complete. The forward focus is the
architectural tracks, in this order:

1. **C1–C5: `ATTACH`/`DETACH`/`TEMP` multi-schema** — start with C1 (the
   multi-database registry), the structural foundation, then layer C2–C5.
2. **C6: `auto_vacuum` + ptrmap** — self-contained in the btree/pager layer.
3. **B1–B3: planner (join order, covering & automatic indexes)** — visible in
   `EXPLAIN QUERY PLAN`, results unchanged.
4. **D1: the `sqlite3_module` virtual-table interface** — unblocks FTS5/R-Tree.
5. **B5–B8: the executor→VDBE migration** — the largest internal refactor;
   unblocks real `EXPLAIN`.
