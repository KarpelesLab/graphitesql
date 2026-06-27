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
collation, column-name spans; cross-object **ALTER** propagation (RENAME
TABLE→views/FKs, RENAME COLUMN→FKs/views/triggers, text-preserving CREATE-text
edits); and a broad **error-parity** sweep — prepare-time column/aggregate/
window/row-value resolution, DDL/DML/JSON/PRAGMA/`printf` message wording,
lexer/parser error framing (`near "TOKEN"`, `incomplete input`, `unrecognized
token: "X"`), and the `json_each`/`json_tree` `id`/`parent` columns (each row's
JSONB *byte offset*, not a row counter) and `fullkey`/`path` label quoting
(`$."a b"` for any non-simple key); `EXPLAIN QUERY PLAN` of a `FROM`-less
SELECT now renders `SCAN CONSTANT ROW` (also covering a single-row `VALUES`);
and the `sqlite_schema.sql` text is now canonicalised the way SQLite stores it
(regenerated `CREATE <TYPE> ` head — `IF NOT EXISTS`/`TEMP` dropped, prefix
whitespace collapsed — plus the verbatim body with the trailing `;` removed),
including the schema-qualified (`CREATE TABLE aux.t …` → bare `t`) and `TEMP`
forms; and `CREATE TABLE … AS SELECT` now writes its column list with SQLite's
`identPut` quoting (bare when safe, keyword-aware, no spaces after commas, e.g.
`CREATE TABLE t(a,c)`); and `ALTER TABLE … ADD COLUMN` now splices the new column
in after the last column but before any trailing table-level constraints
(`t(a, b, c, CHECK(a>0))`); and a multi-column subquery or row value used as a
*comparison operand* (`=`/`<>`/`<`/`IS`/`BETWEEN`/…) against a different-width
operand now reports `row value misused` (the `sub-select returns N columns`
column-count message is kept for genuinely scalar contexts — bare SELECT list,
`IN`, function arguments), while two equal-arity vectors row-compare
lexicographically (`(SELECT 1,2) = (SELECT 3,4)` → `0`, and likewise for
subquery-vs-subquery `BETWEEN`); and a trailing `ORDER BY`/`LIMIT` after a
`VALUES` query core is now a `near "ORDER"/"LIMIT": syntax error` (SQLite's
grammar attaches them only to the `SELECT` form of a core, so `VALUES (1),(2)
ORDER BY 1` — and the same after a compound whose last core is `VALUES` — is
rejected, while a compound ending in a `SELECT` or an outer `SELECT … FROM
(VALUES …) ORDER BY` still parses); and an unresolved column written as a
*double-quoted* identifier (`SELECT "foo"`) now carries SQLite's
string-literal hint — `no such column: "foo" - should this be a string literal
in single-quotes?` — re-quoting the name, while a bare word, a `[bracket]`/
`` `backtick` `` identifier, and any table-qualified reference keep the plain
`no such column: NAME` wording (the hint reaches the result-list, `WHERE`,
`GROUP BY`/`ORDER BY`, and `UPDATE`/`DELETE` paths via a `quoted` flag on
`Expr::Column` threaded through the resolvers and the eager prepare-time
validators); and an unrecognized table option after the column list
(`CREATE TABLE t(a) FOO`) is now `unknown table option: FOO` — the name echoed
verbatim (a bare word, a `"quoted"` identifier or a `'string'` are all option
names; a number/operator there stays a `near "TOKEN"` syntax error), the
option list parsed as a possibly-empty comma-separated set so the first bad
option wins (`FOO, STRICT` reports `FOO` and never enters STRICT mode) and is
surfaced only *after* a STRICT table's missing-datatype check (`STRICT, FOO`
over an untyped column still reports `missing datatype for t.a`), with a
trailing comma now `incomplete input` — all byte-exact vs `sqlite3` 3.50.4.

**Remaining:**

- **A-rn3-edge — RENAME COLUMN in genuinely multi-table view/trigger bodies.**
  The token rewrite bails (leaves the body unchanged — never corrupts) on a bare
  column ref that is ambiguous across multiple base sources, because the AST has
  no per-column-ref source span.
  - **A-rn3-edge-1** — add a source span (byte range) to `Expr::Column`. This is
    the enabling refactor; it *also* unblocks the 3-part-qualifier check below,
    which wants the same enriched `Expr::Column`. Do them together.
  - **A-rn3-edge-2** — use the span for scope-aware rename: resolve each bare ref
    to its owning table, rewrite only the matching ones.
- **Reserved keywords accepted as bare identifiers.** ~37 reserved words
  (`SELECT`, `WHERE`, `FROM`, …) are still accepted where SQLite rejects them —
  e.g. `CREATE TABLE t(select)`. The fix is to apply SQLite's exact reserved-
  keyword set in `ident()` (an `is_reserved_keyword` helper already exists in
  `src/sql/parser.rs`, ~line 460, used by `pragma_value`). High regression risk —
  every accidental-keyword identifier in the test corpus must keep working — so it
  wants a dedicated pass with a full differential sweep over the keyword list, not
  a quick edit.
- **3-part `schema.table.column` qualifier not validated.** `after_name` in
  `src/sql/parser.rs` (~line 2090) parses a 3-part ref but *drops* the schema part
  (`table = column; column = self.ident()?`), so a bogus `nope.t.a` is silently
  accepted instead of erroring. Needs a `schema` field on `Expr::Column` (~74 call
  sites across `src/exec/func.rs`, `src/exec/mod.rs`, `src/sql/parser.rs`,
  `src/exec/vdbe.rs`, `src/exec/eval.rs`, `src/sql/print.rs`). Shares the
  `Expr::Column` enrichment with A-rn3-edge-1.
- **Prepare-time validation gaps (lazy where SQLite is eager).** A few constructs
  are still validated per-row, so an unreached row (empty / fully-filtered table)
  is accepted where SQLite rejects at prepare time. All want the same fix — a
  statement-level prepare pass that walks every expression once, independent of
  row production:
  - `likelihood(a, 2)` over an empty table — the out-of-range probability literal
    is not caught until a row is evaluated.
  - a window nested in an aggregate *argument* in a result column
    (`sum(row_number() OVER ())`) over an empty table.
  - bare (unqualified) refs in derived-table/subquery scopes and `NATURAL`/`USING`
    coalesced names — `validate_columns_exist` only covers the top-level
    plain-table / `ON`-join scope.
- **ALTER-time rejection of an ALTER that breaks a dependent.** An `ALTER` that
  makes a dependent view/trigger unresolvable should be rejected and rolled back;
  graphite leaves the now-broken object. Needs statement-level DDL rollback — a
  writer savepoint around `exec_alter`, mirroring `run_dml_atomic`.
- **Two residual parse-path non-issues (not worth chasing):** `UPDATE SET a=1`
  flags `a` where SQLite flags `SET` (reserved-word leniency), and `BEGIN
  TRANSACTION FOO` silently accepts the trailing name.

### Track B — Query planner, statistics & the VDBE

**Done:** `ANALYZE`/`sqlite_stat1` stats-driven planning; the full equality/range/
`IN`/OR-union + inner-join + `WITHOUT ROWID` seek family; hash join + automatic-
index EQP; index-driven `ORDER BY` and covering reads (**B0/B0b**); the VDBE + its
scalar-expression compiler; **VDBE routing default-on** (**B7a/B7b**, tree-walker
is the fallback oracle); bytecode `EXPLAIN` (**B8**). Running on the VDBE now: the
**join family** (**B5a/B5b-1** — N-table inner joins plus 2-table and N-table
`LEFT`/`RIGHT`/`FULL` nested-loop joins, all with projection / WHERE-merged-ON /
`DISTINCT` / `ORDER BY` / `LIMIT` / grouped-and-bare aggregates, no cross-product
materialized); non-correlated scalar/`EXISTS`/`IN (SELECT)` folds (**B5c-1**, incl.
the native candidate-affinity membership op for a bare-column candidate); compound
`UNION`/`INTERSECT`/`EXCEPT` (**B5c-3**); positional `GROUP BY <ordinal>`;
`DISTINCT`, `FILTER`, and ordered `group_concat(x ORDER BY …)` aggregates; and
window functions over a single table or a plain join (**B5c-4**).

**Remaining — move the last shapes onto the VDBE.** Additive and *perf/coverage
only* (the tree-walker fallback already returns correct results; each step is
gated on VDBE-vs-tree-walker parity, so it can't regress correctness):

- **B5c-2 — correlated subqueries on the VDBE.** Today any subquery reading an
  outer column defers to the tree-walker.
  - **B5c-2a** — thread the outer row into the subquery's register frame.
  - **B5c-2b** — compile `Subquery`/`Exists`/`InSelect` that read an outer column.
- **B5b-2 — seek-driven inner cursor over real storage (the big B8 step).** The
  nested-loop join currently materializes each table's rows into in-memory
  row-sets. This step gives the VDBE interpreter **live storage cursors**
  (`OpenRead` + a b-tree `TableCursor`) so the inner side is *seeked* by
  rowid/PK/index instead of materialized — mirroring the tree-walker's inner-join
  seeks. B5b-1's multi-cursor opcodes (`RewindC`/`ColumnC`/`NextC`) are the
  foundation. This is the largest remaining VDBE piece and the prerequisite that
  makes correlated-subquery (B5c-2) and window streaming worthwhile.
- **B1c — RIGHT/FULL join inner seeks.** INNER/LEFT already seek; RIGHT/FULL still
  materialize the inner table.

**Blocked / deferred by design:**
- **B1b — cost-based join reordering.** graphite's per-cursor seek/bloom-filter
  choices diverge from sqlite's cost-reordered plain scans *by design*; matching
  the EQP would mean abandoning often-cheaper access paths. Results already correct.
- **B4 — `sqlite_stat4` histograms.** The pinned `sqlite3 3.50.4` oracle is built
  without `SQLITE_ENABLE_STAT4`, so there is nothing to diff against and a
  stat4-driven planner would *diverge* the EQP corpus. Needs a stat4-enabled oracle.

### Track C — Storage engine, transactions, concurrency

**Done:** the `Vfs` locking contract; rollback-journal writer serialization;
`SAVEPOINT`; the introspection PRAGMAs; the **multi-schema track** (C1–C5,
`ATTACH`/`DETACH`/`TEMP`); the **`auto_vacuum` track** (C6); `VACUUM … INTO`;
`secure_delete` (C8a); `cache_size`/`mmap_size` reporting (C8b); the **SQLite-
format rollback journal** (C7a/C7b — exact byte layout, two-flush commit protocol,
hot-journal recovery, cross-recovered both ways vs `sqlite3`) with its fault-
injecting crash-recovery harness; and the **bounded LRU `pcache`** (C8c,
`src/pager/pcache.rs`, dirty pages structurally never evictable).

**Remaining:**

- **C8c-leftover — read-cache for read-only connections.** The `WritePager` read
  cache is active only inside a write txn (sole-writer); a pure read-only
  connection over a read-write file falls back to direct (already-bounded) disk
  reads. Coherent caching there needs a statement-level change-counter
  revalidation hook from the exec layer.
- **C9a — persistent read locks.** The lock model is done and verified
  (`LockState` in `src/vfs/mod.rs` counts `Shared` holders; readers coexist,
  `Reserved` admits under readers, `Exclusive` BUSYs until readers drain —
  `tests/concurrency_readers.rs`). What's missing is pager-owned: the pager takes
  `Shared` only transiently on the way to `Reserved` (pure reads hold no
  persistent read lock), so "multiple readers across an open read txn block a
  writer" isn't yet observable at the `Connection` layer. Needs a persistent
  read-lock policy in `src/pager/`.
- **C9b — OS-level cross-process file locks** (`std::fs::File::lock`; wants MSRV
  1.89) behind the std VFS.
- **C9c — the WAL `-shm` wal-index** for multi-connection WAL readers.
- **C9d — a thread-safe `Connection`** (`Send`/`Sync`, or a documented per-thread
  model).

### Track D — Virtual tables & ecosystem extensions

**Done:** the read-only vtab foundation (TVFs, the `VTabModule`/`VTabRegistry`
trait, `best_index`/`filter` pushdown — **D1**); the **writable, persistent** vtab
layer (**W1/W2**); the full **R-Tree** (**D3a–D3c**, byte-compatible nodes) and
the full **FTS5** (**D2a–D2e**, read + write, sqlite-readable on disk, with the
`tokenize=` option chain + diacritic folding); Rust scalar/aggregate **UDFs**
(**D4**); the **`dbstat`** and read-only **`sqlite_dbpage`** (dbpage-1) vtabs. The
FTS5 **inverted-index read path** (**D2b-1/2/3**) is comprehensive: *every* `MATCH`
shape (term / column-scoped / phrase / boolean / prefix / `NEAR`) is index-routed
over single- *and* multi-segment indexes, including multi-leaf term pagination and
doclist spanning.

**Remaining:**

- **D2b-leftover (perf-only).** Two `MATCH` shapes still fall back to the
  `_content` scan — results already correct: a ≥3-phrase `NEAR`, and a single
  very-high-frequency term whose segment uses doclist-index (dlidx) or interior
  (`height > 0`) pages (only reached by a term spanning ~16+ leaves).
- **D2e-encoder — byte-identical FTS5 at large scale.** Structural validity holds
  today; exact-byte parity past a few leaves needs the fts5 writer source for the
  precise split heuristics: the combined spanning-doclist-then-paginated-terms
  leaf-fill boundary; doclist-index (`dli`) pages; segment-b-tree interior
  (`height > 0`) `_data` pages.
- **dbpage-2 — writable `sqlite_dbpage`.** *Oracle-blocked:* the pinned `sqlite3
  3.50.4` alt1 build was compiled without the writable-dbpage path — every raw
  page write returns `read-only`, so there is no engine to diff against. Deferred
  until a writable-dbpage oracle is available. (Also deferred on the read side: a
  `WHERE schema='aux'` hidden-column constraint to redirect the report off `main`,
  which SQLite drives through a hidden `schema` column graphite doesn't push down.)
- **D4-leftover — window UDFs + custom collations.** The latter needs a user
  variant on the `Collation` enum (invasive).
- **D5 — `sqlite3_session`** changesets/patchsets for replication.
- **D6 — async VFS for wasm** (non-blocking IndexedDB/OPFS I/O).

**Blocked by design:**
- **D7 — C-API shim** (`libsqlite3`-compatible surface). Needs `extern "C"` + raw
  pointers, incompatible with `#![forbid(unsafe_code)]`; would live in a sibling
  crate that opts out.

### Track E — Cross-database write resolution  *(essentially complete)*

**Done:** a write to an attached/`temp` database swaps that database in as the
active `main` for the whole statement, while a subquery/source reading the
*original* main still resolves there (**E0–E3 + E-arch-a**). `unqualified_db`
resolves an unqualified name `main`-first then attached (SQLite's `main → temp →
attached` order); `INSERT … SELECT` and `INSERT … VALUES ((SELECT …))` are
pre-materialized in the original context before the swap.

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
- **Crash-recovery** *(Done)* — a fault-injecting `FaultVfs` that kills a chosen
  file at a chosen write/truncate/sync (optionally a torn half-write) drives two
  suites asserting recovery to a consistent, `integrity_check = ok` state at every
  injection point: rollback-journal mode (`tests/crash_recovery_harness.rs`) and
  **WAL mode** (`tests/wal_crash_recovery_harness.rs`, 12 tests — crashes during
  frame append, post-commit-frame, mid-checkpoint db writeback, WAL reset, and
  torn writes; recovery held everywhere, cross-checked with `sqlite3`).
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

1. **B5b-2 — live storage cursors on the VDBE.** The largest remaining VDBE piece;
   it turns the materialized inner join into a seek-driven one and is the
   prerequisite for streaming correlated subqueries and windows. Perf/coverage,
   parity-gated, low risk.
2. **B5c-2 — correlated subqueries on the VDBE**, once B5b-2 lands the live-cursor
   machinery.
3. **C9a → C9d — the concurrency story** (persistent read locks in `src/pager/`,
   then OS file locks, the WAL `-shm` index, and a thread-safe `Connection`).
4. **D2e-encoder / dbpage-2 / D5 / D6** — ecosystem surfaces; pick the unblocked
   ones (dbpage-2 is oracle-blocked, D2e-encoder needs the fts5 writer source).
5. **Track A leftovers** — the `Expr::Column` enrichment (source span + schema
   field) that unblocks both **A-rn3-edge** and the 3-part-qualifier check, plus
   the statement-level prepare pass for the lazy-validation gaps.

**Deferred / blocked** (documented in §4): **B1b** join reordering and **B4**
`sqlite_stat4` (diverge from / unverifiable against the stat1-only oracle);
**B1c** RIGHT/FULL inner seeks (correct via materialization); **D7** the C-API
shim (needs `unsafe`; a sibling crate); **dbpage-2** (oracle-blocked); the
**A-rn3-edge** ambiguous-ref case (needs per-column-ref spans); and the FTS5
large-scale encoder sub-cases (need the fts5 writer source). Build-specific oracle
quirks we intentionally do NOT match are recorded in §6.
