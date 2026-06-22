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
verified against `sqlite3` (a 1,600+ query corpus plus 140+ focused test suites).
Detailed history lives in `CHANGELOG.md`; in summary, graphitesql today:

**Reads & writes real SQLite files.** Opens `sqlite3`-written databases
(including WAL-mode) and **creates** databases whose files `sqlite3` opens with
`PRAGMA integrity_check = ok`. Storage covers rowid and **`WITHOUT ROWID`**
tables, automatic/secondary/`UNIQUE` indexes (incl. `sqlite_autoindex_*`),
overflow pages, the freelist with **page merging on delete**, real **`VACUUM`**,
the full **`auto_vacuum`** track (read, write, FULL auto-truncate, INCREMENTAL
reclaim), and the **WAL read *and* write** path (`journal_mode=WAL`,
`wal_checkpoint`).

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
(+ `CURRENT_DATE`/`TIME`/`TIMESTAMP`, `timediff`), `printf`/`format` (16-sig-digit
float cap like sqlite), `random`/`randomblob`, `unistr`/`unistr_quote`,
`subtype`, **JSON** (`json_*`, `json_group_array`/`object`, `json_pretty`,
`json_each`/`json_tree`, **JSON5 input** with strict `json_valid`, verbatim
number-text preservation) and the **JSONB binary family** (`jsonb`,
`jsonb_array`/`object`/`extract`/`set`/`insert`/`replace`/`remove`/`patch`,
`jsonb_group_array`/`object`, and JSONB-blob input to every `json_*`), **virtual
tables** (`CREATE VIRTUAL TABLE … USING module`), `iif`/`if`,
`sqlite_version`, math (pure-`core`); **type affinity** and SQLite-exact real
formatting; column names matching sqlite's verbatim source spans; collation
(`BINARY`/`NOCASE`/`RTRIM`) honored across comparisons,
`IN`/`BETWEEN`/`CASE`, `min`/`max`, set ops,
`ORDER BY`/`GROUP BY`/`DISTINCT`/`UNIQUE`/index keys; `EXPLAIN QUERY PLAN` with
an index-driven planner (equality/range/`IN`/OR-union seeks, **inner-join seeks**
incl. the full **`WITHOUT ROWID`** PK/secondary-index seek family, **comma-join**
`WHERE`-equality promotion, **automatic-index** reporting for unindexed
equi-joins, **index-driven `ORDER BY`** with **covering-index reads**,
stats-driven choice via `ANALYZE`/`sqlite_stat1`); constraint enforcement
(`NOT NULL`, `CHECK`,
`UNIQUE`/`PK`, standalone/partial/expression UNIQUE indexes; **foreign keys**
enforced at runtime under `PRAGMA foreign_keys=ON` — child INSERT/UPDATE parent
checks and parent DELETE/UPDATE actions NO ACTION/RESTRICT/CASCADE/SET NULL/SET
DEFAULT, composite + self-referential; *deferred:* DEFERRABLE/INITIALLY DEFERRED);
**triggers** (`BEFORE`/`AFTER`/`INSTEAD OF`, `UPDATE OF`,
`WHEN`, recursive, `NEW`/`OLD` incl. rowid); `SAVEPOINT`/`RELEASE`/`ROLLBACK TO`;
**`ATTACH`/`DETACH`/`TEMP`** multi-schema (cross-database reads, writes, joins,
qualified DDL, view reads, transactions & savepoints); DDL with full CREATE-time
and ALTER validation (incl. `DROP TABLE` cascading the table's triggers, and
`RENAME COLUMN` propagating into the table's own CHECK/generated/DEFAULT
expressions); the schema catalog queryable as `sqlite_schema`/`sqlite_master`
(incl. `table_info` over a view and over the catalog itself, the composite-PK
`pk` ordinal, and `index_list` `pk`/`u` origins), with
`table_list`/`collation_list`/`database_list` and bare `pragma_*` table-valued
functions.

What remains is breadth and depth toward full SQLite parity, below.

---

## 4. Forward plan — closing the gap with SQLite

Four tracks. Completed work is summarized; **remaining work is broken into
numbered, independently-shippable pieces** (each one lands with a differential
test and keeps `master` green). Tracks can progress in parallel.

### Track A — SQL language & functions breadth  *(substantially complete)*

Done: outer joins + `NATURAL`/`USING` (coalescing), generated columns,
collations, UPSERT, `UPDATE OR …`, `RETURNING`, row values, `ORDER BY` modifiers,
`STRICT` tables, `CREATE TABLE … AS SELECT`, `INSERT … SELECT`, `UPDATE … FROM`,
HAVING without GROUP BY, SELECT-list aliases in WHERE/GROUP BY/HAVING,
`CURRENT_DATE`/`TIME`/`TIMESTAMP`, `iif`/`if`, `sqlite_version`, the
window-function suite; the math + `printf`/`format` (16-sig-digit float cap) +
JSON libraries incl. JSON5 input, `timediff` (A5), `json_error_position` (A6),
`random`/`randomblob`, `unistr`/`unistr_quote`/`subtype`, the **JSONB binary
family**, and JSON verbatim-number preservation; partial/expression index
*equality* seeks (A3); column names matching sqlite's verbatim source spans;
type names keeping their `(len[,scale])`; and DDL validation incl. `DROP TABLE`
cascading triggers and `RENAME COLUMN` propagating into the table's own
expressions.

**Remaining pieces** (small, each function/clause-scoped):

- **A2 — DESC index columns honored in seeks. ✅ DONE / verified.** A `DESC`
  single-column index seeks equality / range / `IN` with correct results and the
  `SEARCH … USING INDEX` plan; a `DESC` second column in a composite eq-prefix +
  range seek is handled too. Differentially identical to sqlite.
- **A3b — partial/expression index for range/IN seeks. ✅ DONE.** Both
  `try_index_range` (via `partial_expr_range`) and `try_index_in` (via
  `partial_pred_guaranteed` + `find_expr_in_values`) now seek a partial index's
  leading column (predicate proven by the WHERE) or an expression index's keyed
  expression, for `<`/`<=`/`>`/`>=` and `IN (…)`. `eqp_access` in lockstep —
  `SEARCH … USING INDEX i (b>?)`, `(<expr>>?)`, `(b=?)`, `(<expr>=?)`.
- **A4 — `NULLIF` collation in `func.rs`. ✅ DONE / verified.** `nullif(x, y)`
  already resolves the comparison collation via `resolve_collation` (explicit
  `COLLATE` on either operand, then a column's declared collation, then BINARY);
  differentially identical to sqlite, including column-declared collations.
- **A7 — multi-statement `execute_batch(sql)` API. ✅ DONE.** `Connection::
  execute_batch` runs a `;`-separated script like `sqlite3_exec` via a
  tokenizer-based `split_sql_script` (string/comment/`BEGIN…END`/`CASE…END`
  aware), each slice through `execute_params` (a `SELECT` runs and is discarded);
  stops at the first error. `execute()` stays single-statement.
- **A8 — JSONB of JSON5-form numbers. ✅ DONE.** `jsonb('0xFF')` →
  `INT5 "0xFF"`, `jsonb('.5')`/`jsonb('5.')` → `FLOAT5 ".5"/"5."`, byte-identical
  to sqlite (`Json::Int` carries the hex raw text; `Json::Real` keeps the
  leading/trailing-`.` text, dropping only a leading-`+` form which normalizes).
  `json()` still renders them canonically.

*ALTER rename — cross-object propagation* (a column/table rename must reach
*other* schema objects, not just the table's own definition; today those break
with "no such table/column" after a rename). Build bottom-up:

- **A-rn1 — table-rename AST walker. ✅ DONE.** `rename_table_in_select` walks a
  `Select` renaming every `FROM`/`JOIN` `TableRef.name == old`, `old.*`, and
  qualified `old.col`, recursing subqueries / CTEs / windows / compound parts and
  respecting a same-level CTE that shadows the name.
- **A-rn2 — RENAME TABLE rewrites dependent view bodies. ✅ DONE.** On
  `ALTER TABLE t RENAME TO t2`, every view whose body references `t` is rewritten
  with a text-preserving token rewrite (only genuine table references — a
  like-named column tail or function call is spared; the substituted name is
  double-quoted, matching sqlite byte-for-byte). `SELECT … FROM v` works after the
  rename; unrelated views are untouched. (Triggers already fire via the repointed
  `tbl_name`.)
- **A-rn3 — RENAME COLUMN reaches dependent objects.** Extend `rename_column_ref`
  use to dependent view/trigger bodies and to foreign keys in *other* tables that
  name the renamed parent column. *Harder than A-rn2: column renames are
  scope-aware — a bare `oldcol` token can belong to another table, so the
  token-rewrite trick used for A-rn2 is unsafe here; this needs real name
  resolution mapping resolved column refs back to source spans.*
- **A-rn4 — text-preserving schema edits. ✅ DONE.** Every `ALTER TABLE` now edits
  the stored CREATE text in place rather than reprinting the (quoted/canonical) AST,
  so `SELECT sql FROM sqlite_master` is byte-identical to sqlite: **`RENAME TO`**
  (`rename_table_token_after` edits just the table-name token in the table's own
  CREATE and each dependent index's `ON` clause); **`ADD COLUMN`**
  (`append_column_to_create` splices the column's verbatim source before the
  closing paren); **`DROP COLUMN`** (`drop_column_from_create` removes the column
  segment + one adjacent comma); **`RENAME COLUMN`** (`rewrite_ident_tokens`
  repoints the column identifier in the CREATE — inside CHECK/generated exprs and
  dependent index defs too — reproducing the new name bare or double-quoted exactly
  as the user wrote it, captured as `AlterAction::RenameColumn::new_text`; no
  keyword list needed since SQLite preserves the user's quoting, not a re-decision).

### Track B — Query planner, statistics & the VDBE

Done: `ANALYZE` + `sqlite_stat1` (byte-compatible) with stats-driven index
choice; equality/range/`IN`/OR-union seeks (including a **composite equality
prefix + a trailing range** on the next index column, `x=? AND y>?`, seeked as one
bounded range and rendered the same way in EQP); **inner-join seeks** — rowid/IPK
(**B1a**), secondary-index (**B1a²**), and the **complete `WITHOUT ROWID` seek
family** (PK equality + range, PK joins, secondary-index equality + range, with
the named-index-vs-autoindex covering rule); a **hash join** for unindexed
equi-joins with **B3 automatic-index** EQP (`BLOOM FILTER` + `SEARCH … USING
AUTOMATIC COVERING INDEX`); **comma-join `WHERE`-equality promotion** (`FROM a, b
WHERE a.x=b.y` planned like an explicit `JOIN … ON`); **B0** index-driven
`ORDER BY` (rowid + secondary, ASC+DESC); **B2/B2b covering reads** (ordered scan,
`count(*)` via index, and equality/range/`IN` seeks reading straight from a
covering index); and the VDBE spike (`exec::vdbe`) covering constant projections,
single-table scan + `WHERE`/`ORDER BY`/`DISTINCT`/`LIMIT`, whole-table aggregates,
single-table `GROUP BY`, and grouped `HAVING` + aggregate `ORDER BY` (**B6**) —
all matching the tree-walker via `query_vdbe`. The spike's scalar-expression
compiler now spans literals (incl. blobs), arithmetic, concatenation,
comparison + three-valued boolean logic, `IS`/`IS NOT`, the bitwise operators
(`& | << >> ~`), unary `+`/`-`/`NOT`, `IS NULL`, `CASE`, `CAST`, `BETWEEN`,
`LIKE`/`GLOB`, `IN (list)`, the `->`/`->>` JSON operators (its binary-op match
is now exhaustive), and **pure scalar function calls** (`Op::Func` defers to
`eval_scalar` over literal-reconstructed argument values, whitelisted to
context-free functions so `random`/`last_insert_rowid`/date-time/FTS5/UDFs fall
back). Remaining VDBE expression gaps are the context-dependent shapes
(`Collate` threading, `Parameter` binding, correlated `Subquery`/`Exists`/
`InSelect`) — these belong with the executor-integration steps (**B5c**/**B7**),
not the standalone spike.

**Remaining optimizer pieces** *(perf-only — results already correct; acceptance:
the plan matches sqlite3's `EXPLAIN QUERY PLAN` and execution stays in lockstep):*

- **B0b-i — multi-term `ORDER BY` via a multi-column index prefix. ✅ DONE.**
  `order_index_scan` now matches an `ORDER BY (c1, c2, …)` against an index whose
  leading columns are those columns (uniform direction, matching collations,
  default NULLs) and walks the index instead of sorting. The **mixed-direction
  partial sort** over a `WHERE` seek is now reported like sqlite too: when the
  seek walks a leading prefix of the `ORDER BY` in order but a later term breaks
  (e.g. `WHERE a>? ORDER BY a, b DESC`), EXPLAIN reads `USE TEMP B-TREE FOR LAST
  n TERM[S] OF ORDER BY` (via `seek_order_prefix`, shared with B0b-iii). *(A
  mixed-direction partial sort over a no-`WHERE` full-index scan still full-sorts;
  results are correct in every case — only that EQP label differs.)*
- **B0b-ii — covered query over an index. ✅ DONE (EQP/read side).** A no-`WHERE`
  query whose every referenced column is held by exactly one full index now reads
  from that index (`covering_scan` + `query_cols_covered`), reporting `SCAN …
  USING COVERING INDEX` like sqlite — covering plain projections, `DISTINCT`, and
  `GROUP BY`/aggregates over covered columns. *(Still hash-grouped, not stream-
  grouped in index order; results identical.)*
- **B0b-iii — `ORDER BY` from a `WHERE`-chosen index. ✅ DONE.** A `WHERE` seek
  walks its index in key order, so the rows already arrive ordered and the sort is
  skipped. Two cases, both recognized by `order_satisfied_by_scan` (so EQP and
  execution stay in lockstep): an **equality** seek satisfies an `ORDER BY` on the
  index columns following the equality prefix (`WHERE a=? ORDER BY b`, via
  `order_satisfied_by_seek`); a leading-column **range** seek satisfies an `ORDER
  BY` over the index columns themselves (`WHERE a>? ORDER BY a, b`, via
  `order_satisfied_by_range_seek`). Both are conservative — they fire only when
  exactly one plain secondary index can seek (no equality, no partial/expression
  index, no rowid range for the range case), so the executor's choice is
  unambiguous; otherwise the always-correct sort stands. Byte-identical EQP and
  row order vs sqlite3 (`tests/order_by_after_seek.rs`). *Remaining sub-case: the
  **mixed-direction** `ORDER BY a, b DESC` partial sort (B0b-i), still a full
  sort.*
- **B1b — Join reordering.** Beyond the comma-join promotion (done), reorder
  `FROM` tables by a simple cost model (most-selective indexed table inner)
  instead of textual order; results identical, order verified via EQP. Preserve
  LEFT/RIGHT/FULL semantics. *Ref:* `where.c`.
- **B1c — RIGHT/FULL join inner seeks.** B1a/B1a²/WITHOUT-ROWID seeks cover
  INNER/LEFT; RIGHT/FULL joins still materialize the inner table.
- **B4 — `sqlite_stat4` histograms.** Extend `ANALYZE` to gather per-index sample
  histograms (byte-compatible `sqlite_stat4` rows) and use them for range
  selectivity. Split: **B4a** write/read the `sqlite_stat4` rows; **B4b** consult
  them in the seek-cost chooser. *Ref:* `analyze.c`.

*VDBE migration* (the largest internal refactor — changes representation, not
results; keep the differential corpus green at every step). Done so far: the
spike covers single-table scans, aggregates, `GROUP BY`, and grouped `HAVING` +
aggregate `ORDER BY` (**B6**), all parity-checked via `query_vdbe`. Remaining,
each additive behind `query_vdbe` until B7:

- **B5a — VDBE two-table inner join. ✅ DONE (cross-product form).** A single
  INNER/CROSS/comma join materializes `t1 × t2` as one combined row set with the
  `ON`/join predicate folded into the `WHERE`, then reuses the single-cursor
  scan compiler + interpreter unchanged (`query_vdbe`). Bails (→ tree-walker) on
  outer joins, `NATURAL`/`USING`, `t.*` over a join, 3+ tables, and ambiguous
  shared column names. *Remaining for B5b:* a true per-cursor nested loop
  (`OpenRead`/`Rewind`/`Column`/`Next`) so the inner side isn't fully
  materialized, plus index/PK inner seeks.
- **B5b — VDBE join: per-cursor nested loop, index/PK inner seek + outer-join
  NULL-extend.** Replace the materialized cross-product with cursor opcodes; add
  the seek-driven inner cursor and LEFT NULL-extension.
- **B5c — VDBE: subqueries / compound / window** shapes still on the tree-walker.
- **B7a — route `query()` onto the VDBE behind a flag. ✅ DONE.**
  `Connection::set_use_vdbe(true)` makes `query()` try the VDBE engine first and
  fall back transparently (on any unsupported shape *or* error) to the
  tree-walker, which stays the source of truth. Off by default. A corpus-parity
  test (`vdbe_routing_matches_tree_walker_on_corpus`) runs the full differential
  corpus with routing on and asserts byte-identical output: **1773 / 1898
  queries (93 %) are answered by the VDBE**, the rest fall back. Validating this
  flushed out and fixed several real VDBE bugs — comparison **affinity**
  (`g <= '1'`), `IS TRUE`/`IS FALSE` truthiness, `LIMIT 0`/negative, and unsafe
  routing of correlated-subquery inner scans (gated off via `outer_scope`).
- **B7b — flip the default to the VDBE. ✅ DONE.** `use_vdbe` now defaults to
  **on**: `query()` runs `SELECT` through the VDBE first and falls back
  transparently to the tree-walker for any shape it does not handle (every
  unsupported shape, and every error, defers — the tree-walker stays the source
  of truth). Validated by running the **entire test suite** with the default on
  (not just the differential corpus): byte-identical throughout. Getting there
  flushed out and fixed a series of real VDBE gaps — comparison/ORDER BY
  **collation**, `IS TRUE`/`LIMIT 0`, qualified-column resolution, the
  grouped-bare-column / attached-schema / column-label cases, plus (this step)
  an ORDER-BY-satisfied-by-index-scan bail (tie/NULL order follows the scan),
  `HAVING`-without-GROUP-BY and index-hint bails, JSON-subtype-aware functions
  excluded from the scalar whitelist, blob `||` concatenation, and `-(i64::MIN)`
  overflow promotion. The compiler also does qualified `(table, column)`
  resolution, and routing is per query block (`run_core`) so compound-query arms
  are accelerated while the tree-walker combines.
- **B8 — `EXPLAIN` (bytecode). ✅ DONE.** Plain `EXPLAIN <select>` compiles the
  query to graphite's VDBE program and returns the listing as `(addr, opcode,
  detail)` rows (`Program::explain_rows` + `Connection::explain_bytecode`, via a
  no-row `compile_select_program`). This is graphite's own register-machine IR;
  it intentionally does **not** mirror SQLite's `vdbe.c` opcode set (documented
  as unstable, and never compared in the differential corpus). Covers the
  constant and single-table cases; joins/other shapes report `Unsupported`
  (EXPLAIN QUERY PLAN remains the full-coverage path). *Ref:* `vdbe.c`.

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
transactions + savepoints. **`auto_vacuum`** is fully read/write now (C6a read +
C6b-1 empty-db header + C6b-2 ptrmap maintenance): graphite reads and **writes**
auto_vacuum databases that sqlite3 reads with `integrity_check = ok` (only the
optional space-reclaim — FULL truncate C6b-3, `incremental_vacuum` C6b-4 —
remains).

**`VACUUM … INTO <file>` ✅ DONE** — writes a compact copy of the database to a
new file (the in-place `VACUUM` reuses the same compact-rebuild; `vacuum_write_into`
evaluates the target path, refuses an existing file like sqlite, and stamps the
preserved `user_version`). `std`-only (needs the OS VFS). The output is read by
`sqlite3` with `integrity_check = ok` and matches sqlite's own `VACUUM INTO`
logical content (`tests/vacuum_into.rs`).

The **multi-schema track is complete** (C1–C5 + C-ms1 `CREATE TEMP
VIEW/TRIGGER` catalog placement), and the **whole `auto_vacuum` track is complete**
(C6a read; C6b-1 empty-db header; C6b-2 commit-time `rebuild_ptrmap`; C6b-3 FULL
auto-truncate; C6b-4 `incremental_vacuum(N)` on-demand reclaim — all cross-checked
with sqlite3 `integrity_check = ok`). Tuning PRAGMAs (`cache_size`, `synchronous`,
`busy_timeout`, `locking_mode`, …) now *report* sqlite's defaults; honoring them
is C8b/C8c.

**Remaining pieces** *(storage / durability / concurrency — each independent):*

- **C7 — SQLite-format rollback journal.** Match the on-disk journal byte layout
  (ours is a private, recoverable format today) so a crash mid-write is
  recoverable by `sqlite3`. Pairs with the crash-recovery harness (§6). Split:
  **C7a** write the sqlite journal header + page records; **C7b** recover from a
  sqlite-format journal on open.
- **C8a — `secure_delete`.** Zero freed cell/page content (`PRAGMA
  secure_delete=ON`).
- **C8b — honor `PRAGMA cache_size` / `mmap_size`. ✅ DONE.** `cache_size` round-
  trips its set value on the connection (default −2000); `mmap_size` returns no
  rows (the reference build disables mmap) instead of erroring. Both verified vs
  sqlite. The set `cache_size` is stored but does not yet bound a real cache —
  that is C8c.
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

Done: the **read-only** virtual-table foundation — a table-valued-function
mechanism (`generate_series`, `json_each`, `json_tree`); the `VTabModule` trait +
`VTabRegistry` (**D1a**); `CREATE VIRTUAL TABLE … USING module[(args)]` parsing,
persistence, and `FROM`-source integration incl. joins and `DROP` (**D1b**); and
`best_index`/`filter` constraint pushdown (**D1b²**). Today a vtab is read-only
(INSERT/UPDATE/DELETE rejected) and stateless (modules get no storage).

**The blocker for FTS5/R-Tree is a *writable, persistent* vtab.** Both store data
that must survive in the file, byte-compatibly. SQLite backs them with **shadow
tables** (ordinary b-trees) — which graphite already writes byte-compatibly — so
the path is: give modules writable shadow storage, then build the two modules on
top. Build bottom-up (each step lands testable on `:memory:` first):

- **W1 — writable-vtab trait + DML routing. ✅ DONE.** `VTabModule::update`
  (the `xUpdate` analog) takes a `VTabChange::Insert`/`Delete`/`Update`; the
  default keeps a table read-only. `Connection::register_module` registers a
  custom module. The executor routes all three DML verbs to it
  (`exec_vtab_insert`/`exec_vtab_delete`/`exec_vtab_update`): INSERT maps the
  column list and evaluates values; UPDATE/DELETE scan via the cursor, filter by
  `WHERE`, and call `update` per matching row. Explicit-rowid INSERT (W1c) too.
  Tests: `tests/writable_vtab.rs`. *(Remaining: hidden columns, RETURNING /
  `UPDATE … FROM` on a vtab.)*
- **W2 — shadow-table storage for modules. ✅ DONE.** A `persistent()` module
  keeps its rows in a real `<vtab>_data` regular table (created at CREATE VIRTUAL
  TABLE, reusing the normal table machinery → transactional + integrity-checked).
  Reads scan it; `update` gets a `VTabStore` (rows/put/delete) over it. The
  re-entrancy (executor `&mut self` while the module is registry-borrowed) is
  solved by `with_vtab_store`, which takes the module out of the registry for the
  write phase. Data survives reopening the file (tested). *(W2a: the backing table
  holds the vtab's own columns; FTS5/R-Tree will declare module-specific shadow
  schemas on top of the same `VTabStore` mechanism.)*
- **D3 — R-Tree** (smaller of the two; build on W1/W2). *Ref:* `rtree.c`.
  - **D3a — module + correct results. ✅ DONE.** Built-in `rtree` module
    (`vtab.rs`, registered on every connection) on top of W1/W2: parses
    `rtree(id, minX, maxX[, …])`, persists rows in `<name>_data`, answers spatial
    queries by scan + the re-applied WHERE. Coordinates use sqlite's exact f32
    directional rounding (`rtreeValueDown`/`Up`'s `1 ∓ 2⁻²³` nudge — min↓, max↑);
    id is the integer rowid; rejects `min>max` and bad arity. Differentially clean
    vs sqlite3 (`tests/rtree.rs`). *(The shadow table is W2a's `<name>_data`, not
    sqlite's `_node`/`_rowid`/`_parent` — that byte-compat layout is D3c.)*
  - **D3b — `best_index` spatial pushdown (EQP). ✅ DONE (plan reporting).**
    `RTreeModule::best_index` mirrors sqlite's rtree `xBestIndex` so EXPLAIN QUERY
    PLAN reads identically: an `id =` lookup is `INDEX 1:`, a coordinate range is
    `INDEX 2:<op><col>…` (`A`=`=`,`B`=`<=`,`C`=`<`,`D`=`>=`,`E`=`>`; column digit
    0-based among coords — e.g. `minX>=? AND maxX<=?` → `INDEX 2:D0B1`), a bare
    scan `INDEX 2:`. `eqp_vtab_detail` now routes persistent modules through
    `best_index` for the reported plan (fts5 keeps `INDEX 0:`). *(Execution still
    scans the backing table + re-applies WHERE; narrowing the scan via the node
    tree — true pushdown — would build on D3c's node format.)*
  - **D3c — byte-compatible node format. ✅ DONE.** R-Trees (no aux columns) use
    sqlite's on-disk node format — the `<name>_node` b-tree of nodes + `_rowid` /
    `_parent` maps — so a graphite-written R-Tree round-trips through sqlite3
    (`rtreecheck` + `integrity_check` ok, queries identical), and graphite reads a
    sqlite-written one. A node blob is a 2-byte BE depth (root) + 2-byte BE cell
    count + cells (8-byte BE rowid/child + n_coord 4-byte BE f32/i32 coords),
    padded to `min(page_size-64, 4+51*cell_size)`. Each write reads the current
    entries, applies the change, and BULK-REBUILDS a balanced tree (sqlite reads
    any valid tree, so the incremental quadratic-split shape need not be
    reproduced). Wired through CREATE/INSERT/DELETE/UPDATE/DROP/RENAME/VACUUM;
    conservative f32 rounding + min>max rejection preserved. `tests/rtree.rs`.
    *(A write rebuilds the whole tree — O(n)/statement; incremental insert/split
    is a later perf optimization. Aux-column R-Trees keep `_data`.)*
- **D2 — FTS5** full-text search (build on W1/W2; the larger module). *Ref:*
  `fts5*.c`. Break out: **D2a** tokenizer (unicode61/ascii); **D2b** inverted
  index in shadow tables + `INSERT`; **D2c** `MATCH` query; **D2d** `bm25()`
  ranking; **D2e** byte-compatible on-disk segment format.
  - **D2a — tokenizer. ✅ DONE.** `fts5_tokenize` in `vtab.rs`: maximal
    alphanumeric runs, case-folded — a faithful unicode61 approximation for
    ASCII/basic text (diacritic folding + full Unicode category tables deferred).
  - **D2b — document store. ✅ DONE (correct-results; inverted index deferred).**
    The built-in `fts5` module (registered on every connection alongside
    `series`/`rtree`) declares one untyped column per `USING fts5(col, …)` name
    (ignoring `key = value` options and `col UNINDEXED` modifiers) and stores
    documents in the persistent `<name>_data` backing table keyed by an implicit
    rowid. CREATE/INSERT/UPDATE/DELETE/SELECT and `PRAGMA table_info` are
    byte-identical to sqlite3. *(A real inverted index in shadow tables — for
    scaling beyond a scan — is the remaining D2b work.)*
  - **D2c — `MATCH` query. ✅ DONE (correct-results, full core query language).**
    `t MATCH …` searches all columns, `col MATCH …` one column (scan + the
    re-applied WHERE, like rtree D3a). A recursive-descent parser in `vtab.rs`
    handles the full FTS5 query grammar: bare tokens, `token*` prefixes,
    `"quoted phrases"` (consecutive/ordered), `col:…` column filters, and the
    boolean operators `AND` (explicit or implicit), `OR`, `NOT` with SQLite's
    precedence (`NOT`>`AND`>`OR`) and parentheses, and the `NEAR(p1 p2 …, n)`
    proximity group. Byte-identical to sqlite3 across all these forms — the core
    FTS5 query language is complete.
  - **D2d — `bm25()` ranking + the `rank` column. ✅ DONE (correct-results).**
    `ORDER BY rank` / `ORDER BY bm25(t)` sorts most-relevant-first and `bm25(t)`
    / `rank` expose the score, byte-for-byte sqlite's Okapi BM25 (`k1=1.2`,
    `b=0.75`, idf clamped up to `1e-6`, sum negated). `fts5_bm25_scores`
    (`vtab.rs`) scores the corpus honoring `col:` filters and a `col MATCH …`
    scope; `run_core` computes the per-rowid scores for a single-`fts5`-table
    MATCH query into a connection-scoped `Fts5QueryCtx` cell that the `bm25()`
    special form and the `rank` column read during projection / `ORDER BY`.
    Outside an fts5 MATCH, `rank`/`bm25()` stay ordinary unknown names.
  - **D2 EXPLAIN QUERY PLAN. ✅ DONE.** `EXPLAIN QUERY PLAN` over an `fts5` table
    reports sqlite's `idxNum:idxStr`: a table-wide `MATCH` is `INDEX 0:M<ncols>`,
    a column `MATCH` is `M<colidx>`, a rowid lookup is `=`, and `ORDER BY rank` /
    `ORDER BY rowid` set the order-consumed bit (32 / 64) — byte-identical to
    sqlite3 (`eqp_vtab_detail`'s fts5 branch).
  - **D2-aux — `highlight()`. ✅ DONE (correct-results).** `highlight(t, col,
    open, close)` wraps each matched-phrase token in the markers (one pair per
    phrase instance; original inter-token text preserved; case-insensitive,
    case-preserving), byte-identical to sqlite3. Reuses the `Fts5QueryCtx` cell
    (the MATCH query; no corpus needed) via a position-aware tokenizer
    (`fts5_tokenize_spans`).
  - **D2-aux — `snippet()`. ✅ DONE (correct-results).** `snippet(t, col, open,
    close, ellipsis, n)` reproduces `fts5SnippetFunction` byte-for-byte: per
    phrase instance it scores the anchor window (`1000`/distinct phrase + `1`/
    repeat) and considers two starts — the centered `iAdj`, and the enclosing
    sentence boundary (token 0 or after a `.`/`:` + space) with a `+120`/`+100`
    bonus; best score wins, earliest start on ties; a window reaching the column
    end appends the trailing text (so a final `.` survives) instead of an
    ellipsis. The auto-column form `snippet(t, -1, …)` scores every column and
    renders the best (ties → first column). Validated at 0 mismatches over ~17k
    random differential cases (terms/OR/phrase, 1–3 columns, explicit and `-1`
    columns, every `n`, punctuated sentences, spaced/unspaced `col:token`).
  - **D2-fts5-feature.** The whole module is behind a default-on `fts5` Cargo
    feature; `--no-default-features` (or any build without `fts5`) drops it and
    `USING fts5` then reports `no such module: fts5`, as SQLite does uncompiled.
  - **D2-vocab — `fts5vocab`. ✅ DONE (correct-results).** The `fts5vocab`
    module (`Fts5VocabModule` in `vtab.rs`, registered when `fts5` is enabled)
    presents another FTS5 table's vocabulary in all three forms: `row` (term,
    doc, cnt), `col` (term, col, doc, cnt), and `instance` (term, doc, col,
    offset). `connect` validates `(table, type)` and declares the form's columns;
    the executor's `scan_fts5vocab` tokenizes the referenced table's documents
    (same `fts5_tokenize` as indexing) and aggregates, byte-identical to sqlite3
    (`tests/fts5vocab.rs`). *(The `instance` form's `offset` column must be
    quoted in graphite — `offset` is a reserved word in its parser, an orthogonal
    gap.)*
  - **D2e — byte-compatible on-disk format. READ ✅ DONE (M1); WRITE remains
    (M2).** **M1:** graphite reads a sqlite-written FTS5 — its parser now accepts
    sqlite's single-quoted shadow-table names (`CREATE TABLE 'ft_data'(…)`), and
    `try_virtual_table` reads the documents from `<name>_content`, answering
    queries (incl. `MATCH`) with the scan-based matcher (the segment index need
    not be decoded). `tests/fts5.rs`. **M2 (in progress):** for graphite's *own*
    FTS5 files to be sqlite-readable, write the `%_data`/`%_idx` inverted-index
    segment layout (doclists, varint postings, segment b-tree) — sqlite's `MATCH`
    + FTS5 integrity-check need a valid index, so (unlike the R-Tree) a scan-based
    store is not enough. **M2a — segment-byte encoder verified ✅:** the structure
    record, averages record, and single-leaf page (term prefix-compression with
    the `FTS5_MAIN_PREFIX` '0', doclists with rowid deltas, position lists with
    collist deltas) are reproduced byte-for-byte vs sqlite 3.50.4 across every
    doclist shape — `tests/fts5_segment.rs` (a differential reference encoder).
    **M2c — multi-leaf pagination encoder verified ✅:** by lowering FTS5's
    logical `pgsz`, the differential test also reproduces multi-leaf segments
    byte-for-byte — the leaf-packing split threshold (flush before a term would
    take the encoded leaf `>= pgsz`), each leaf re-stating its first term in full
    form, the structure record's config cookie + write counter (= leaf count),
    and the `%_idx` separator rows (each the shortest prefix of a leaf's first
    term that exceeds the previous leaf's last term). `tests/fts5_segment.rs`.
    *Remaining encoder sub-cases:* the "first rowid before first term" carry when
    a single term's doclist spans leaf boundaries, doclist-index pages (dli flag),
    and segment b-tree interior `_data` pages (height > 0). **M2b — storage wiring
    (remaining):** swap graphite's generic `<name>_data` backing table for
    sqlite's five shadow tables (`_content`/`_data`/`_idx`/`_docsize`/`_config`),
    maintaining them on insert/delete/update and porting the verified encoder.
    The larger remaining file-compat piece.
- **D4 — User-defined functions from Rust.** Scalar ✅ DONE
  (`register_function`, via `Subqueries::call_udf`) and aggregate ✅ DONE
  (`register_aggregate_function` + an `AggregateFunction` step/finalize trait;
  detection via a predicate-parameterized `expr_contains_agg`, honoring GROUP
  BY/HAVING/DISTINCT). Built-ins win. *Remaining: window UDFs and custom
  collations (the `Collation` enum would need a user variant — invasive).* Pairs
  with `register_module`.
- **D-introspect — `dbstat`. ✅ DONE.** The eponymous read-only `dbstat` virtual
  table reports per-page b-tree storage stats (`name, path, pageno, pagetype,
  ncell, payload, unused, mx_payload, pgoffset, pgsize`), one row per page plus
  one per overflow page. `scan_dbstat` (`exec/mod.rs`) DFS-walks every b-tree
  (`sqlite_schema` at page 1, then each table/index root) and computes, byte-for-
  byte like SQLite's dbstat.c: `unused` from the page header's cell-content
  pointer, fragmented-bytes count, and freeblock chain; `payload` as the summed
  local cell bytes; `mx_payload` as the largest total cell payload; SQLite's
  `/<hex-child>/` + `+<hex-overflow>` path strings; and even reproduces dbstat's
  off-by-one overflow `pgoffset` (the lagged previous-page offset). A real user
  table named `dbstat` shadows it. Differentially clean vs sqlite3 over small,
  indexed, multi-level, and overflow-chain databases (`tests/dbstat.rs`).
  *(Sibling `dbpage` — writable raw page access — not yet built.)*
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
- **Fuzzing** — a deterministic corruption-robustness harness
  (`tests/fuzz_corruption.rs`, ~50k malformed-file variants;
  `tests/fuzz_sql.rs`, ~3.3k malformed/deeply-nested SQL) asserts the readers
  return an error and never panic. It already caught real reader panics
  (`btree/page.rs` assert/bounds/arithmetic, `sql/parser.rs` recursion depth),
  now fixed. *(Expand toward a coverage-guided fuzzer when a no-dep path exists.)*
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

The bounded SQL-language, function, and planner correctness items are essentially
closed (the whole `WITHOUT ROWID` seek family, comma-join promotion, automatic-
index EQP, JSONB, JSON number-provenance, `random`/`randomblob`, `unistr`/
`subtype`, view `table_info`, the `DROP`/`RENAME COLUMN` DDL fixes, etc.). What's
left is **bigger, multi-step work** — each track above is now broken into the
smaller pieces to ship it. Suggested order:

1. **W1 + W2 — writable, persistent vtabs.** The single unlock for the two
   headline modules. Small, self-contained, and testable on its own (a trivial
   writable module proves it before D2/D3).
2. **D3 — R-Tree** on top of W1/W2 (D3a correct results → D3b pushdown → D3c
   byte-compatible nodes). Smaller and more bounded than FTS5; do it first.
3. **A-rn3 — cross-object RENAME COLUMN** (view/trigger/FK propagation), the one
   remaining *functional* ALTER gap (A-rn1 + A-rn2 RENAME TABLE→views are done);
   needs scope-aware column resolution, not the token rewrite A-rn2 used.
4. **Planner leftovers** (perf-only, EQP-gated) — the mixed-direction partial
   sort, **B1b** join reordering, **B4** `sqlite_stat4`, and **B0b-iii**'s
   *multi-index* case (the single-seekable-index case is done; the ambiguous
   multi-index case still needs a shared seek-index-choice helper to match
   sqlite). (**A2** DESC seeks, the composite eq-prefix + trailing-range seek,
   **A3b** partial/expr range·IN seeks, **B0b-i** multi-term ORDER BY, **B0b-ii**
   covered-query covering scan, and **B0b-iii** single-index ORDER-BY-after-seek
   are done.)
5. **D2 — FTS5** (D2a–D2e) — the larger module, once W1/W2 and R-Tree have
   exercised the writable-vtab path.
6. **B5/B7/B8 — the executor→VDBE migration** — the largest internal refactor;
   unblocks real bytecode `EXPLAIN`.
7. **Smaller gaps** — **C8a/b/c** (secure_delete, cache honoring). (**A4**
   `nullif` collation, **A7** `execute_batch`, and **A8** JSONB JSON5 numbers are
   done.)

Deferred / blocked: **C7/C9** (SQLite-format journal + cross-process
locks/concurrency — durability depth), **D5/D6** (sessions, async wasm VFS),
**D7** (C-API — blocked by `#![forbid(unsafe_code)]`), **A-rn4** (cosmetic
text-preserving schema edits). The **SQLite TCL suite** (§6) isn't runnable
against a Rust crate — the differential corpus + `integrity_check` remain the
green proxy.
