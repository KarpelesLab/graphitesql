# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

- Track C: **`ATTACH`/`DETACH`, `TEMP`, and cross-database queries** (C1–C5).
  `ATTACH ':memory:'/'file.db' AS x` and `DETACH x` manage an attached-database
  registry (`PRAGMA database_list`). Schema-qualified names (`aux.t`) work for
  reads and writes (`CREATE`/`INSERT`/`UPDATE`/`DELETE`/`DROP … aux.t`,
  `aux.sqlite_master`), databases isolated, cross-engine verified. `CREATE TEMP
  TABLE` lives in a private in-memory `temp` database (seq 1) that shadows main
  for unqualified names and never persists to a file; `sqlite_temp_master` reads
  it. File attachments are sqlite3-readable/writable both directions.
  **Cross-database joins** now work (`SELECT … FROM main.u JOIN aux.o ON …`),
  each source materialized through its own backend, with 3-part column names
  (`aux.tbl.col`) parsed; `WITHOUT ROWID` tables read cross-db too. Qualified
  `ALTER TABLE aux.t …` (ADD / RENAME COLUMN / RENAME TABLE),
  `CREATE INDEX aux.idx ON t(…)`, `CREATE TRIGGER aux.tr … ON t …`, and
  `CREATE VIEW aux.v AS …` target the attached database (stored bare-named,
  cross-engine verified; trigger bodies' `NEW.col` left intact). Cross-database
  **view reads** resolve the view body's unqualified tables (joins, subqueries,
  nested views) in the view's own database. (Cross-database transactions are
  upcoming.)
- **Transaction & DDL state checks**: nested `BEGIN`, and `COMMIT`/`ROLLBACK`
  with no active transaction, are now rejected; `DROP` of a missing object
  reports lowercase "no such <kind>" with a table↔view hint; `ALTER … RENAME
  COLUMN` onto an existing name and `RENAME TABLE` onto an existing table/index
  are rejected with SQLite's messages.
- **CREATE TABLE validations** matching SQLite: duplicate column name, more than
  one PRIMARY KEY, a PRIMARY KEY/UNIQUE list naming a missing column,
  AUTOINCREMENT only on an INTEGER PRIMARY KEY (and not on WITHOUT ROWID), and a
  table with no non-generated column.
- Fix: the **`%` operator** truncates both operands to integers (`10.5 % 3` → 1.0,
  divisor truncating to 0 → NULL), like SQLite; the `mod()` function stays floating.
- **`string_agg`** added as the standard-SQL alias for `group_concat`.
- **`json_group_array` / `json_group_object` aggregates** — build a JSON array
  or object from a group (NULL-inclusive, `ORDER BY` inside the aggregate, JSON
  subtype propagation for `json(...)` arguments), like SQLite.
- Fix: **collation is now honored** in `IN (list)`, `IN (SELECT …)`, `BETWEEN`,
  `CASE x WHEN y`, `min()`/`max()`, and compound set ops (UNION/INTERSECT/EXCEPT
  dedup + their ORDER BY) — these used plain BINARY before, so NOCASE columns
  diverged from SQLite. (Literal-left `IN`/comparison falling back to the
  subquery/right column's collation, and window-frame min/max, remain edges.)
- **`printf`/`format` `,` thousands-grouping flag and `l`/`ll` length
  modifiers** (`printf('%,d', 1234567)` → `1,234,567`; `%ld`/`%lld` accepted).
- Fix: **`UPDATE OF <columns>` triggers** fire only when one of the named
  columns is in the UPDATE's SET list (previously fired on any update).
- Fix: **`NEW.rowid` / `OLD.rowid`** (and qualified rowid in correlated
  subqueries) now resolve inside trigger bodies.
- **SELECT-list aliases in WHERE/GROUP BY/HAVING** are now resolved (a real
  column of the same name still takes precedence), matching SQLite — e.g.
  `SELECT a+b AS s FROM t WHERE s>3` and `… GROUP BY m`/`HAVING c>1`.
- Fix: **CTE explicit column list** must match the body column count
  (`table t has N values for M columns`), like SQLite.
- Fix: a **multi-row `VALUES` on the right of a compound operator** (e.g.
  `… UNION VALUES(2),(3)`) now contributes all its rows, not just the first.
- Track A: **`UPDATE … SET … FROM <sources>`** (SQLite's UPDATE-FROM extension)
  — the target table is joined to the FROM tables (incl. multi-table and
  derived-table sources); each matched target row is updated using the joined
  row's columns, firing triggers and enforcing constraints as usual.
- Fix: **LIMIT/OFFSET on a recursive CTE** is honored — it bounds the produced
  rows and terminates the recursion (was stripped, causing "did not terminate").
- Fix: **HAVING without GROUP BY** parses and runs (whole result = one group);
  a HAVING on a non-aggregate query is rejected like SQLite.
- Fix: **`PRAGMA table_info.dflt_value`** is the default expression's SQL text
  (string defaults keep their quotes, `DEFAULT NULL` shows `NULL`).
- Fix: **table-qualified rowid aliases** (`t.rowid` / `t._rowid_` / `t.oid`) now
  resolve (bare forms already did); a real column of that name still wins.
- Fix: **`*` / `table.*` mixed with aggregates** (`SELECT *, count(*) …`) now
  works — wildcards expand to columns following the representative-row rule.
- Track A: **`INSERT … SELECT`** — populate a table from a query (compound
  sources and target column lists included). The query is snapshotted before any
  insert, so `INSERT INTO t SELECT … FROM t` terminates; rows then flow through
  the ordinary insert path (defaults, constraints, triggers, indexes). As part
  of this, a bare `VALUES` row with an implicit column list is now required to
  match the column count, matching SQLite.
- **Schema catalog queryable** as `sqlite_schema` and the historical
  `sqlite_master` (read-only 5-column rowid table at page 1); direct DML against
  it is rejected with "table … may not be modified".
- Fix: **`ALTER TABLE ADD COLUMN` constraint restrictions** — a `UNIQUE` or
  `PRIMARY KEY` column is rejected, and a `NOT NULL` column with a NULL default
  is rejected when the table already has rows, matching SQLite.
- Fix: **subqueries rejected in CHECK constraints and generated columns** at
  `CREATE` time (SQLite forbids them; graphite previously evaluated them).
- Fix: **`sum()`/`abs()` integer overflow is an error** (not a silent real
  promotion), matching SQLite — the `+`/`*` operators still fall back to real.
- Fix: **`-9223372036854775808` parses as `Integer(i64::MIN)`** (the literal
  `2^63` folds under a leading minus) instead of a real; `typeof` and `abs()`
  now agree with SQLite.
- Fix: **text→number ignores `inf`/`infinity`/`nan`** (value 0 / NULL like
  SQLite); numeric overflow such as `1e400` still yields ±Inf.
- Track A: **`STRICT` tables**. `CREATE TABLE … STRICT` (alone or with `WITHOUT
  ROWID`, in either order) restricts column types to `INT`/`INTEGER`/`REAL`/
  `TEXT`/`BLOB`/`ANY` — any other or missing type is rejected at `CREATE` — and
  type-checks every stored value against its column on INSERT/UPDATE/UPSERT
  (`ANY` columns store values with no affinity). The whole type×value matrix,
  the stored `typeof`/`quote`, and the `CREATE`-time rejections all match
  `sqlite3`, which also reads our STRICT files and enforces them identically.
- Fix: **UNIQUE enforcement for standalone indexes**. A `CREATE UNIQUE INDEX`
  (plain, partial, expression, or multi-column) was maintained but never
  *enforced* — duplicate keys were silently accepted. `find_conflicts` (and the
  WITHOUT ROWID write paths) now check these indexes, collation- and NULL-aware,
  covering INSERT/UPDATE/UPSERT/`OR IGNORE`/`OR REPLACE`.
- Track B: **hash join**. A two-table join with an equi-join `left.col = right.col`
  in its `ON` now builds a hash index on the joined table and probes it per left
  row (the full `ON` is still re-evaluated on each candidate, so semantics are
  unchanged), turning the O(n·m) nested loop into a probe. Numeric keys collide
  across `INTEGER`/`REAL` (`5` and `5.0`) and across affinity (`5`/`'5'`) via
  multi-keying; non-`BINARY` collations fall back to the nested loop. Verified
  against `sqlite3` (numeric/text/NOCASE/duplicate-key/NULL/outer/self joins).
- Fix: pre-comparison type affinity no longer text-coerces a typeless (BLOB/NONE)
  column against a TEXT column — `none_col = text_col` now matches SQLite (e.g.
  integer `1` vs `'1'` is false). `expr_affinity` distinguishes a literal's
  absence of affinity from a column's BLOB affinity.

- Track B: **`IN`-list index seeks**. A single-table query with `column IN (c1,
  c2, …)` now seeks each constant through an index on that column (or the rowid
  b-tree for an `INTEGER PRIMARY KEY`), unions the rowids, and fetches the rows,
  instead of scanning. Returns a superset (full `WHERE` re-applied). Verified
  against `sqlite3`.
- Track B: **OR-by-union**. A single-table query whose `WHERE` is a top-level `OR`
  of individually index/rowid-seekable predicates (equality, `IN`, range, or an
  `AND` containing one) now seeks each disjunct, unions the rowids, and fetches the
  rows once, instead of scanning. If any disjunct is not seekable it falls back to
  a scan. Superset semantics keep it correct (full `WHERE` re-applied). Verified
  against `sqlite3`, including ORs spanning two different indexes. `EXPLAIN QUERY
  PLAN` reports these as SQLite's nested `MULTI-INDEX OR` / `INDEX 1` / `SEARCH …`
  structure.
- Track B: `EXPLAIN QUERY PLAN` now reports the index range and `IN`-list seeks as
  `SEARCH … USING INDEX … (a>? AND a<?)` / `(a=?)` (and rowid `IN` as
  `… INTEGER PRIMARY KEY (rowid=?)`), matching SQLite's format and reflecting what
  the executor actually does.
- Track B: index **range scans**. A single-table query whose `WHERE` constrains an
  indexed column by `<`/`<=`/`>`/`>=`/`BETWEEN` now seeks the index between those
  bounds (`btree::index_range_rowids`, an in-order traversal that stops once the
  upper bound is passed) instead of scanning the whole table, then re-applies the
  full `WHERE`. The lookup returns a superset, so correctness is preserved
  regardless of bound edge cases. A range on the `INTEGER PRIMARY KEY` rowid walks
  the table b-tree directly between integer bounds (seeking the lower bound, then
  iterating until the upper). Both verified against `sqlite3`, and reported by
  `EXPLAIN QUERY PLAN` as `SEARCH … (rowid>? AND rowid<?)` etc.
- Track A: `octet_length(X)` (byte length of a value's encoding — blob bytes, else
  the UTF-8 length of its text form) and the `glob(pattern, text)` function form of
  the `GLOB` operator. Both verified against `sqlite3` in the differential corpus.
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
  (dedup, then sort). Whole-table aggregates (`count`/`sum`/`total`/`avg`/`min`/
  `max`/`group_concat`, no `GROUP BY`) compile to `AggStep`/`AggFinal`: the scan
  folds each slot (counting rows for `count(*)`, collecting non-NULL arguments
  otherwise) and a single `ResultRow` emits the finalized values, reproducing the
  tree-walker's exact semantics (integer-`sum` overflow promotes to real, empty
  group yields 0/NULL per function). `GROUP BY <columns>` over a single table
  compiles to `GroupStep`/`GroupEmit`: the scan folds per-group accumulators
  (groups kept in first-seen order, NULLs grouping together, matching the
  tree-walker) and one row per group is emitted, where each output column is
  either a grouping-key value or a finalized aggregate. `HAVING`/`ORDER BY`/
  non-grouped output expressions fall back to the tree-walker.
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
