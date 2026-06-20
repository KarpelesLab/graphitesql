# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

- Track D: table-valued functions — `generate_series(start, stop[, step])`,
  `json_each`, and `json_tree` as `FROM` sources (sole source or joined).
  `json_each` yields the direct children, `json_tree` the full depth-first tree,
  each with the `key`/`value`/`type`/`atom`/`id`/`parent`/`fullkey`/`path` columns
  (the `id`/`parent` numbering is graphitesql's own; the rest match SQLite).
  Establishes the TVF mechanism (`TableRef.tvf_args`). Verified against `sqlite3`.
- Track A: `RIGHT [OUTER] JOIN` and `FULL [OUTER] JOIN`. The nested-loop join now
  tracks matched right rows and emits the unmatched ones with NULL left columns
  (and unmatched left rows for `FULL`/`LEFT`). Verified against `sqlite3`.
- Track A: `LIKE … ESCAPE`, the `like(pattern, text[, escape])` function form, and
  the `likely`/`unlikely`/`likelihood` optimizer-hint functions (identity at the
  value level). Verified against `sqlite3`.
- Track A: `CREATE TABLE … AS SELECT …` (CTAS). The new table's columns are the
  query's output labels (untyped), populated with the query's rows via the normal
  insert path. Verified against `sqlite3`.
- Track A: `INDEXED BY name` / `NOT INDEXED` query hints. `NOT INDEXED` forces a
  table scan; `INDEXED BY` restricts the planner to the named index (and errors if
  it does not exist). Results are identical to the unhinted query. Verified.
- Track A: ordered aggregates — `group_concat(x ORDER BY y [DESC])` (and any
  aggregate with an inner `ORDER BY`) sorts the group's rows before folding,
  honoring `DESC`/`NULLS` and collation. Verified against `sqlite3`.
- Track A: `percent_rank()` and `cume_dist()` window functions. Verified against
  `sqlite3`.
- Track A: named windows — `WINDOW w AS (…)` definitions with `OVER w` references
  and `OVER (w ORDER BY …)` extension (a base window supplies `PARTITION BY`; the
  use may add `ORDER BY`/frame). Verified against `sqlite3`.
- Track C: in-engine `PRAGMA integrity_check` / `quick_check`. Walks every table
  and index b-tree and verifies each index holds exactly the entries its table
  implies (honoring partial-index predicates), returning `ok` when consistent or
  one row per problem — no longer delegated to `sqlite3`. Agrees with `sqlite3` on
  valid databases (rowid/WITHOUT ROWID, multi-column/unique/partial/expression
  indexes).
- Track C: introspection PRAGMAs — `index_list`, `index_info`,
  `foreign_key_list`, `foreign_key_check`, `freelist_count`, `application_id`,
  `data_version`. Output matches SQLite's column layout and ordering;
  `foreign_key_check` reports `(table, rowid, parent, fkid)` for each dangling
  reference. Verified against `sqlite3`.
- Track A: expression indexes — `CREATE INDEX … (lower(x))`, `(a + b)`, etc. The
  index key is the per-row evaluation of the term expressions; entries are
  maintained on insert/update/delete and rebuild, so `sqlite3 integrity_check`
  (which recomputes the expressions) passes. The planner scans rather than
  seeking an expression index. (Not supported on WITHOUT ROWID tables yet.)
  Verified against `sqlite3`.
- Track A: partial indexes — `CREATE INDEX … WHERE <predicate>`. The index stores
  only rows satisfying the predicate; entries are added/removed as rows cross the
  boundary on insert/update/delete, so `sqlite3 integrity_check` passes. The
  planner conservatively scans rather than seeking a partial index (always
  correct). Verified against `sqlite3`.
- Track A: `VALUES` as a query — standalone (`VALUES (1,2),(3,4)`) and as a table
  source (`SELECT … FROM (VALUES …)`). Desugared to a `UNION ALL` of single-row
  selects with SQLite's `column1`/`column2`/… naming. Verified against `sqlite3`.
- Track A: aggregate `FILTER (WHERE …)`. `count`/`sum`/`avg`/`total`/
  `group_concat`/… accept a `FILTER (WHERE predicate)` that restricts which rows
  of the group they consume, grouped or ungrouped. Verified against `sqlite3`.
- Track C: `SAVEPOINT` / `RELEASE` / `ROLLBACK TO` nested transactions. The write
  pager snapshots its staged state on `SAVEPOINT`; `ROLLBACK TO` restores it
  (keeping the savepoint open and repeatable), `RELEASE` discards it keeping the
  changes, and releasing the outermost savepoint of an implicit transaction
  commits. Savepoints nest inside `BEGIN`, revert schema changes, and persist to
  disk on release. Verified against `sqlite3` semantics.
- Track A: row-value expressions — `(a,b) = (c,d)`, lexicographic ordering
  (`<`/`<=`/`>`/`>=`), `(a,b) IN ((…),(…))`, and `(a,b) IN (SELECT …)`, with
  SQLite's three-valued NULL semantics (an undecided element yields NULL; a
  decisive earlier element still resolves). Verified against `sqlite3`.
- Track A: JSON `->`/`->>` operators and mutators. `->` returns the extracted
  node as JSON, `->>` as a SQL value; a bare-label or integer right operand is
  normalized to `$.label`/`$[n]`. Added `json_set`, `json_insert`,
  `json_replace`, `json_remove`, and RFC-7396 `json_patch`; nested
  `json_array`/`json_object` arguments embed as JSON. Verified against `sqlite3`.
- Track A: `ORDER BY … NULLS FIRST/LAST` and `IS [NOT] DISTINCT FROM`. NULL
  placement in sorts is now controllable (default stays SQLite's: NULLs first
  under `ASC`, last under `DESC`); `IS DISTINCT FROM`/`IS NOT DISTINCT FROM` are
  the null-aware (in)equality operators. Verified against `sqlite3`.
- Track C: VFS advisory-locking contract and writer serialization. A new
  `LockState` encodes SQLite's `SHARED`/`RESERVED`/`PENDING`/`EXCLUSIVE`
  compatibility rules; `MemoryVfs` and `StdVfs` now share one lock state per path
  across all open handles (process-local). The write pager takes the write-intent
  lock when staging a transaction and upgrades to exclusive while flushing, so a
  second connection writing the same database is rejected with `Error::Busy`
  while another holds an open write transaction — and the lock is released on
  commit, rollback, and autocommit. (Reads buffer per-connection so they stay
  isolated from uncommitted writes; cross-process OS locks remain a host-VFS
  concern.)
- Track B: VDBE bytecode IR spike. A new `exec::vdbe` module defines a
  register-machine instruction set (`Op`), a `Program`, a compiler for constant
  `SELECT` projections, and an interpreter — built *alongside* the tree-walking
  executor (not replacing it) so the IR can grow incrementally toward cursors and
  filters. The compiled+interpreted output matches both the tree-walker and
  `sqlite3` for arithmetic, concatenation, comparison, three-valued `AND`/`OR`/
  `NOT`, `IS [NOT] NULL`, `CASE` (via `Goto`/`IfFalse` control flow on a
  program-counter interpreter), and `CAST` projections; unsupported queries
  cleanly report `Unsupported` for fallback. The IR also scans a single plain
  table with an optional `WHERE` filter (`Rewind`/`Column`/`Next` cursor ops with
  an `IfFalse` row skip), wired into the engine via the new
  `Connection::query_vdbe`, matching the tree-walker and `sqlite3` for
  `SELECT <exprs> FROM <table> [WHERE …] [ORDER BY …] [LIMIT n [OFFSET m]]` (a
  `DecrJumpZero` counter caps the row count; an `IfPosDecr` counter skips the
  leading `OFFSET` rows). `ORDER BY` compiles to a sorter: the scan stages each
  projected row plus its key columns (`SorterInsert`), then after the scan the
  rows are sorted (`SorterSort`, honoring `DESC`/`NULLS FIRST`/`LAST`) and a
  second cursor loop (`SorterRewind`/`SorterRow`/`SorterNext`) emits them with
  `OFFSET`/`LIMIT` applied to the sorted output. Output-column ordinals
  (`ORDER BY 2`) and aliases (`ORDER BY d`) resolve to their projection.
  `SELECT DISTINCT` compiles to a `DistinctCheck` gate (NULLs compare equal) that
  drops duplicate output rows before `OFFSET`/`LIMIT`, composing with `ORDER BY`
  (dedup, then sort).
- Track B: `ANALYZE` and cost-based index selection. `ANALYZE [name]` gathers
  index selectivity into a `sqlite_stat1(tbl,idx,stat)` table, byte-compatible
  with SQLite's `nRow avgEq1 avgEq2 …` format (`avgEqK = (nRow + dK/2)/dK`);
  no-index tables get a `(tbl, NULL, nRow)` row, empty indexes are skipped, and
  re-analyzing replaces a table's rows. The planner (both execution and
  `EXPLAIN QUERY PLAN`) now prefers the most selective usable index per those
  statistics, falling back to the longest-prefix heuristic when unanalyzed.
  Verified against `sqlite3` incl. `integrity_check`.
- Track A: SQLite JSON functions — `json`, `json_valid`, `json_quote`,
  `json_type`, `json_array_length`, `json_extract`, `json_array`, `json_object`.
  Includes a pure-`core` RFC-8259 parser/serializer and `$`/`.key`/`[n]` path
  navigation; JSON scalars map back to SQL values (`true`/`false`→1/0,
  `null`→NULL), objects/arrays return minified JSON text, and nested
  `json_array`/`json_object` calls embed as JSON (subtype propagation by call
  origin). Verified against `sqlite3`. (Mutators `json_set`/`json_remove`/…, the
  `->`/`->>` operators, and `json_each`/`json_tree` are not yet implemented.)
- Track A: SQLite math functions — `pi`, `sqrt`, `exp`, `ln`, `log`/`log10`/
  `log2`, `pow`/`power`, `mod`, `ceil`/`ceiling`, `floor`, `trunc`, `sin`/`cos`/
  `tan`, `asin`/`acos`/`atan`/`atan2`, `sinh`/`cosh`/`tanh`,
  `asinh`/`acosh`/`atanh`, `degrees`, `radians`. Implemented in pure `core`
  arithmetic (no libm dependency): `sqrt` is correctly rounded; the transcendentals
  are accurate to ~1 ULP. NULL/domain errors return NULL. Verified against `sqlite3`.
- Track A: UPSERT and `RETURNING`. `INSERT … ON CONFLICT [(target)] DO NOTHING`
  skips the conflicting row; `DO UPDATE SET … [WHERE …]` updates the existing
  row, exposing the would-be-inserted values via the `excluded` pseudo-table and
  honoring a vetoing `WHERE`. `INSERT`/`UPDATE`/`DELETE … RETURNING <cols|*>`
  projects the affected rows; drained via the new `Connection::execute_returning`.
  Verified against `sqlite3`. (WITHOUT ROWID upsert/returning not yet supported.)
- Track A: collating sequences — `BINARY`/`NOCASE`/`RTRIM` honored in comparisons,
  `ORDER BY`, `GROUP BY`, `DISTINCT`, `count(DISTINCT …)`, `UNIQUE` enforcement, and
  index b-tree ordering/seek. Resolution follows SQLite: explicit `COLLATE` (left
  precedence) > column collation (left precedence) > `BINARY`. NOCASE/RTRIM indexes
  order their keys by the collation so `sqlite3 integrity_check` passes, and
  index-driven equality lookups find case-variant rows. Verified against `sqlite3`.
- Track A: generated columns — `… AS (expr) [STORED|VIRTUAL]`. VIRTUAL columns
  are computed on read and not stored; STORED ones are materialized on write;
  writes to a generated column are rejected; indexes over generated columns work;
  `table_info` hides them. Verified against `sqlite3` incl. `integrity_check`.
- Phase 9: b-tree page merging on delete — a delete that empties table leaf pages
  now compacts the b-tree in place (root preserved), returning the slack to the
  freelist for reuse so the file no longer grows unboundedly across delete/insert
  cycles; verified valid across heavy/scattered/full deletes by `sqlite3`
  `integrity_check`. This clears the last named Phase 9 deliverable.

## [0.0.4](https://github.com/KarpelesLab/graphitesql/compare/v0.0.3...v0.0.4) - 2026-06-19

### Other

- Phase 9: UNIQUE constraints on WITHOUT ROWID tables
- Phase 9: real VACUUM compaction + empty-page cursor fix
- Phase 8/9: WAL write path (PRAGMA journal_mode=WAL)
- Phase 9: secondary indexes on WITHOUT ROWID tables
- Phase 9: INSTEAD OF triggers (writable views)
- Phase 9: WITHOUT ROWID tables
- correct remaining-deliverables list
- Phase 9: automatic indexes for UNIQUE / PRIMARY KEY
- Phase 9: PRAGMA recursive_triggers
- Phase 9: broaden differential corpus to 1658 (windows, subqueries, reals)
- Phase 9: explicit window frame clauses
- Phase 9: derived tables (FROM (SELECT ...) AS alias)
- Phase 9: views and CTEs as join sources
- refresh README status for expanded SQL surface
- Phase 9: row triggers (CREATE TRIGGER)
- Phase 9: foreign-key enforcement (PRAGMA foreign_keys)
- Phase 9: window functions + %.15g real formatting
- Phase 9: correlated subqueries + EXISTS
- Phase 9: recursive CTEs (WITH RECURSIVE)
- Phase 9: EXPLAIN QUERY PLAN + rowid equality fast-path
- Phase 9: date/time functions + printf/format

## [0.0.3](https://github.com/KarpelesLab/graphitesql/compare/v0.0.2...v0.0.3) - 2026-06-19

### Other

- Phase 9: index-driven query planning (closes the rest of issue #4)
- Phase 9: compound queries (UNION / UNION ALL / INTERSECT / EXCEPT)
- Phase 9: broaden differential corpus to 1633 (joins, group_concat, GLOB)
- Phase 9: fix substr() window semantics; differential at 1618/1618
- Phase 9: type affinity (comparison + storage)
- Phase 9: expand differential corpus + fix CAST/aggregate bugs
- Phase 9: differential test harness (1513/1513 vs sqlite3); MSRV 1.88

## [0.0.2](https://github.com/KarpelesLab/graphitesql/compare/v0.0.1...v0.0.2) - 2026-06-19

### Other

- Phase 9: ALTER TABLE RENAME COLUMN
- Phase 9: UNIQUE/PRIMARY KEY enforcement + INSERT OR IGNORE/REPLACE
- Phase 9: enforce CHECK constraints
- Phase 9: non-recursive CTEs (WITH ... AS (...))
- Phase 9: subqueries — scalar (SELECT ...) and IN (SELECT ...)
- Phase 9: parse the full CREATE TABLE constraint grammar
- Phase 9: accept VACUUM (no-op compaction)
- Phase 9: CREATE VIEW / DROP VIEW and querying views
- Phase 9: more scalar functions (concat, sign, zeroblob, quote, unhex, ...)
- Phase 9: enforce NOT NULL constraints
- Phase 9: ALTER TABLE (ADD COLUMN / RENAME TO) + AST printer
- add status badges to README
- Phase 9: CREATE INDEX + index maintenance + DROP
- Phase 9: freelist reclamation (frees pages; overflow-row DELETE)

## [0.0.1](https://github.com/KarpelesLab/graphitesql/compare/v0.0.0...v0.0.1) - 2026-06-19

### Other

- Fix CI docs build + add test/no_std jobs
- Add graphitesql CLI shell (sqlite3-style)
- Phase 9: queryable PRAGMAs (table_info, page_size, ...)
- Phase 9 (breadth): multi-table INNER/LEFT/cross joins
- Remove stray pipe FIFO accidentally committed
- Phase 8: WAL read support (real-checksum frame overlay)
- Phase 7: writable Connection — CREATE/INSERT/UPDATE/DELETE + transactions
- Phase 6: write side — journaled pager + b-tree insert (sqlite3-compatible)
