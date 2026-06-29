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
verified against `sqlite3` (a 1,600+ query corpus plus 240+ focused test suites).
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

**Done** — the per-feature write-ups that used to fill this section have been
cleared; each completed item lives in the git history, the release-plz
`CHANGELOG`, and its own `tests/*.rs`. Everything below is differentially
byte-exact vs the pinned `sqlite3` 3.50.4 oracle. Capability summary:

- **Query surface** — SELECT with every join kind (INNER/LEFT/RIGHT/FULL,
  NATURAL, USING-coalesced, cross), correlated / `EXISTS` / `IN (SELECT)` /
  scalar subqueries, compound set-ops (UNION[ ALL]/INTERSECT/EXCEPT), CTEs
  (recursive, mutual, `[NOT] MATERIALIZED`), window functions (ROWS/RANGE/GROUPS
  frames, `EXCLUDE`, value-offsets, `FILTER`, named windows), DISTINCT,
  GROUP BY / HAVING, ORDER BY (`NULLS FIRST/LAST`, `COLLATE`, positional),
  LIMIT/OFFSET (incl. subquery operands).
- **DML** — INSERT (multi-row, `DEFAULT VALUES`, `INSERT … SELECT` with snapshot
  semantics), UPSERT (`ON CONFLICT DO UPDATE/NOTHING`, `excluded.*`, partial-index
  targets), `RETURNING`, UPDATE (simultaneous SET, `UPDATE … FROM`, row-value
  `SET (a,b)=(…)` and `=(SELECT …)`), DELETE, all `OR <conflict>` clauses, and
  `AS`-aliased UPDATE/DELETE targets.
- **DDL** — CREATE/DROP/ALTER TABLE (ADD / DROP / RENAME COLUMN, RENAME TABLE),
  CREATE/DROP VIEW / INDEX / TRIGGER (BEFORE/AFTER/INSTEAD OF, WHEN, `RAISE`),
  STRICT and WITHOUT ROWID, generated columns (VIRTUAL/STORED), AUTOINCREMENT +
  `sqlite_sequence`, partial / expression / collation / DESC indexes,
  constraint-level `ON CONFLICT`, and FK enforcement (CASCADE / SET NULL /
  SET DEFAULT / RESTRICT, composite, self-referential, DEFERRABLE).
- **Cross-object ALTER propagation** — RENAME TABLE → dependent views / FKs /
  triggers; RENAME COLUMN → FKs / views / triggers, including single-source
  nested-subquery bodies via the global-uniqueness provers, with quote- and
  whitespace-preserving edits to the regenerated CREATE text. DROP COLUMN is
  rejected when a view/trigger still references the column in value position.
- **Functions & values** — the full scalar / aggregate / date-time /
  `printf`+`format` / JSON + JSONB libraries; type affinity (comparison and
  storage), collation (BINARY/NOCASE/RTRIM, propagated through
  IN/BETWEEN/CASE/min-max/compound), `random()`/`randomblob()`, blob↔text↔number
  coercion, and verbatim column-name source spans.
- **Schema catalog & introspection** — `sqlite_schema`/`sqlite_master` readable;
  stored `sql` text canonicalised the way SQLite stores it (regenerated
  `CREATE <TYPE>` head, dropped `IF NOT EXISTS`/`TEMP`/trailing `;`, CTAS column
  quoting); the introspection PRAGMAs and the `pragma_*` table-valued-function
  surface; `EXPLAIN QUERY PLAN` shaping (incl. `SCAN CONSTANT ROW`).
- **ATTACH / multi-schema** — `ATTACH`/`DETACH`, schema-qualified read/write/DROP,
  TEMP tables, cross-database joins / views / transactions (see Track E).
- **Error parity** — prepare-time column / aggregate / window / row-value
  resolution and misuse checks; DDL/DML/JSON/PRAGMA/`printf` message wording;
  lexer/parser framing (`near "TOKEN"`, `incomplete input`,
  `unrecognized token: "X"`); the double-quote→string-literal hint; and
  constraint-failure column naming.

**Remaining — genuinely open work.** Ordered roughly easiest → hardest. These are
the residuals left after the differential sweep; the surface is otherwise
exhausted for bounded (single-fix) work.

- **A-misc-1 — structural row-arity error ordering vs name resolution.** When a
  clause holds *both* a structural row-arity error and a missing column, SQLite's
  precedence is **clause-ordered, then pre-order within each clause's expression
  tree** (it raises these during a single name-resolution walk that visits each
  node before its children, in clause order). Concretely:
  - within one clause, a structural mismatch on a node beats a `no such column`
    in that same subtree — `(nope,a) IN ((1,2,3))` → `IN(...) element has 3 terms
    - expected 2`; `(nope,a)=(1,2,3)` and `(nope,a) BETWEEN (1,2,3) AND (4,5,6)`
    → `row value misused`; `nope=(1,2)` (scalar-vs-row) → `row value misused`;
  - but a missing column in an *earlier-resolved clause* wins over a later
    clause's structural error — `SELECT nope FROM t WHERE (a,a)=(1,2,3)` →
    `no such column: nope`;
  - and the `IN` *scalar-LHS + row-element* misuse (`nope IN ((1,2))`) is **not**
    structural — a missing column wins there (`no such column: nope`).

  graphite resolves all columns up front (`validate_columns_exist`) and only then
  runs the structural checks (`reject_row_value_misuse`), so it emits the column
  error in every "structural + missing-column **in the same clause**" case. The
  structural logic itself is already correct and byte-exact for column-clean
  inputs; only the *ordering* against name resolution diverges. A naive
  whole-statement structural pre-pass would regress the cross-clause case above,
  so the real fix is to **interleave** the structural arity check with column
  resolution clause-by-clause in SQLite's clause order (pre-order within each
  tree) — a resolver-ordering change, not a one-line hoist. Low-impact (the
  message body is identical; only which of two errors fires first differs).

- **A-misc-2 — `*` / `t.*` over an unaliased self-join.** `SELECT * FROM t, t`
  is ambiguous on the *database-qualified* expansion (`main.t.a`, `temp.t.a`);
  graphite reports the bare `t.a`. Needs the owning database name threaded onto
  `ColumnInfo` (it currently carries only table/alias). The non-wildcard
  ambiguous-column message already echoes the written qualifier exactly.

- **A-tvf-bare — eponymous TVFs used as a bare table name (no parens).**
  `FROM generate_series`, `FROM json_each`, `FROM pragma_table_info` (without a
  parenthesised argument list) are real eponymous virtual tables in SQLite: their
  hidden arguments can be supplied through the `WHERE` clause
  (`FROM generate_series WHERE start=1 AND stop=3`), and an unconstrained
  reference yields either no rows (`json_each`, the pragma TVFs) or a
  function-specific "first argument … missing or unusable" error
  (`generate_series`). graphite only recognises these *with* parentheses, so a
  bare reference is `no such table: <name>`. Closing this means modelling the
  eponymous TVFs as `FROM` sources with `WHERE`-driven hidden-column binding —
  a feature, not a message tweak.

- **A-printf-bang — `printf`/`format` `!` (alt-form-2) flag at high precision.**
  Without `!` graphite is byte-exact (SQLite caps a `%f`/`%e`/`%g` double at 16
  significant digits). The `!` flag lifts the cap to 20 sig-digits via SQLite's
  bespoke float decoder, where graphite instead emits the exact f64 decimal
  expansion — so `printf('%!.20f', 0.1)` is graphite's `0.10000000000000000555`
  vs SQLite's `0.1000000000000000055`. Matching it requires porting
  `sqlite3FpDecode` + the double-double binary→decimal machinery
  (`sqlite3Fp2Convert10`, `powerOfTen`/`aBase`/`aScale`/`aScaleLo`,
  `sqlite3Multiply128/160`) — a ~300-line table-heavy port for an obscure
  extension flag. Deferred as low-ROI; see [[printf-bang-float-decode]].

- **A-rn3-edge — RENAME COLUMN in genuinely multi-table view/trigger bodies.**
  The token rewrite already handles single-source-with-subqueries bodies and
  nested-subquery / cross-object reaches via the global-uniqueness provers
  (`view_global_unique_quals` / `trigger_global_unique_quals`): a bare `old` is
  rewritten when that column name is unique across *every* base source at any
  nesting level. What remains is the genuinely **ambiguous** case — a bare ref to
  a column that *several* in-scope base tables own, where only the occurrence in
  the renamed table's scope should change. The rewrite currently bails (leaves the
  body unchanged — never corrupts) because the AST has no per-column-ref span.
  Two steps:
  - **A-rn3-edge-1** *(enabling refactor)* — add a source span (byte range) to
    `Expr::Column`. The sibling `schema` field it once shared with the now-landed
    3-part qualifier check is already in place, so this is just the span.
  - **A-rn3-edge-2** — use the span for scope-aware rename: resolve each bare ref
    to its owning table and rewrite only the matching occurrences.

- **A-prepare-correlated — prepare-time validation in correlated subquery bodies.**
  The eager (prepare-time, row-independent) validators now cover the common
  scopes — `validate_columns_exist` (top-level plain-table / `ON`-join),
  `validate_derived_columns` (single derived-table `FROM`),
  `validate_join_derived_columns` (a derived source joined or `NATURAL`/`USING`
  coalesced), and `reject_unresolved_functions_in_subqueries` (unknown / wrong-
  arity scalar functions inside expression-position subqueries, gated on
  `subquery_body_columns_clean`). The single residual is a three-part
  `schema.table.column` reference inside a **correlated subquery body** that binds
  to an enclosing `FROM`: `SELECT (SELECT bad.t.a) FROM t` is accepted where
  SQLite reports `no such column: bad.t.a`, and a subquery body that is itself
  correlated-with-a-missing-column is likewise left to the lazy path. Both want
  the same missing piece — correlated-outer-scope resolution for a three-part ref.
  *(Orthogonal: the tree-walker still cannot* execute *a bare/qualified `rowid`
  over any join — a per-table-rowid-in-join-rows gap, not a validation gap.)*

- **A-alter-rollback — ALTER-time rejection of a RENAME that breaks a dependent.**
  `DROP COLUMN` already rejects pre-mutation when a dependent view/trigger would
  break (it computes the post-drop shape before touching storage, so no rollback
  infra is needed). A `RENAME TABLE`/`RENAME COLUMN` whose propagation *cannot be
  proven* (so the rewrite is skipped) can still leave a dependent that no longer
  resolves; SQLite rejects and rolls back. graphite mutates before the breakage is
  observable, so this needs **statement-level DDL rollback** — a writer savepoint
  around `exec_alter`, mirroring `run_dml_atomic` — which is the one piece of
  infrastructure the whole "reject an ALTER that breaks a dependent" story is
  waiting on.

### Track B — Query planner, statistics & the VDBE

**Done.** The per-shape write-ups have been cleared (git history + `tests/*.rs`
keep the detail); each item below is gated on VDBE-vs-tree-walker parity, so it
returns the tree-walker's results or declines to it — never a wrong answer.

- **Planner / access paths** — `ANALYZE`/`sqlite_stat1` stats-driven planning;
  the full equality / range / `IN` / OR-union + inner-join + `WITHOUT ROWID` seek
  family; hash join + automatic-index EQP; index-driven `ORDER BY` and covering
  reads (**B0/B0b**).
- **VDBE core** — the register VM + its scalar-expression compiler; **routing
  default-on** (**B7a/B7b**, tree-walker is the fallback oracle); bytecode
  `EXPLAIN` (**B8**).
- **Joins on the VDBE** (**B5a/B5b-1**) — N-table inner joins plus 2-table and
  N-table `LEFT`/`RIGHT`/`FULL` nested-loop joins, with projection /
  WHERE-merged-ON / `DISTINCT` / `ORDER BY` / `LIMIT` / grouped-and-bare
  aggregates, no cross-product materialized. `SELECT *` / `tbl.*` over a
  same-named-column join self-qualifies each column to its source. An aggregate /
  window function misused in an `ON`/`WHERE` predicate is detected up front and
  deferred so the tree-walker raises the proper error.
- **Subquery folds** (**B5c-1**) — non-correlated scalar / `EXISTS` / `IN (SELECT)`
  evaluated by the tree-walker with affinity/collation preserved; the fold threads
  scope as it descends, so further-nested self-contained subqueries and
  compound-bodied (`UNION`/`INTERSECT`/`EXCEPT`) subqueries fold too (value folds
  require every arm to project a computed column so the result carries NONE
  affinity, matching a literal list).
- **Grouping & aggregates** — positional and computed `GROUP BY` keys; bare
  (non-grouped) columns emitted from the group's first row (or a lone
  `min`/`max`'s companion row); `SELECT DISTINCT` over a grouped query (dedup via
  a `DistinctCheck` placed after `HAVING`, before `LIMIT`/sorter); `DISTINCT` /
  `FILTER` / ordered + two-arg `group_concat`/`string_agg` aggregates; grouped
  output key-ordered before emission. An explicit non-BINARY `COLLATE` on a
  `DISTINCT` / `min`/`max` / aggregate argument correctly defers to the
  tree-walker rather than folding under BINARY.
- **Compound set-ops** (**B5c-3**) — `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`,
  including a whole-query `WITH` referenced by one or more arms (arms that can't
  run — recursive / join / deep-sibling CTE bodies — decline the whole compound).
- **Derived / view / CTE / TVF sources** — `FROM (VALUES …)` and `FROM (SELECT
  consts)`; an in-scope CTE source (recursive / sibling-reading / shadowing);
  derived bodies that are a plain join, a same-affinity compound, a view, or
  nested to any depth, with each output column's `(affinity, collation)` resolved
  across the body (`subquery_column_origins` / `arm_column_origins` /
  `named_source_origins`) so affinity-sensitive outer predicates coerce exactly;
  a view named directly as a `FROM` source; a single table-valued-function source
  (`generate_series`, `json_each`/`json_tree`, `pragma_<name>(arg)`) and a
  constant-argument TVF in a join. Non-BINARY-collation columns, `rowid` over a
  rowid-less source, and bodies the origin resolver can't crack still defer.
- **Window functions** (**B5c-4**) over a table, a plain/NATURAL/USING join, a
  view, a TVF, a derived subquery, or a CTE — `window_source_columns` /
  `window_join_source_columns` resolve each source's columns exactly as the base
  scan exposes them, then the shared `finish_from_rows` tail evaluates the frames.
- **Misc operators** — three-argument `LIKE … ESCAPE`, `printf`/`format`, the
  date/time library (`date`/`time`/`datetime`/`julianday`/`unixepoch`/`strftime`/
  `timediff`, all via `Op::Func` over reconstructed argument values — `'now'`
  reads the wall clock identically on both paths, never the `ctx`), and the rest
  of the pure-value scalar library on the same `Op::Func` path: the
  inverse-hyperbolic math (`asinh`/`acosh`/`atanh`), the Unicode-escape helpers
  (`unistr`/`unistr_quote`), the JSON syntax probe (`json_error_position`, which
  reads only its argument's text, not a JSON subtype), and the build-constant
  identifiers (`sqlite_version`/`sqlite_source_id`); positional
  `ORDER BY`/`GROUP BY` ordinals resolved
  through `positional_int` (a signed/parenthesized/`COLLATE`-wrapped in-range
  ordinal names its output column on every path; an out-of-range or non-ordinal
  form defers so the tree-walker raises SQLite's exact range error; an ordinal
  resolving to an aggregate output column is rejected at prepare time).

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
