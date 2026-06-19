# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

- Phase 9: `PRAGMA recursive_triggers` — triggers may fire other triggers when
  enabled (bounded to 1000 levels); off by default, matching SQLite
- Phase 9: broaden the differential corpus to 1658 queries (window functions,
  derived tables, correlated subqueries/`EXISTS`, real-valued expressions) and
  render reals through the `%.15g`-compatible formatter
- Phase 9: explicit window frame clauses — `ROWS`/`RANGE`/`GROUPS BETWEEN … AND …`
  (and the bare-start form), with `UNBOUNDED`/`CURRENT ROW`/`N PRECEDING`/
  `N FOLLOWING` bounds and an accepted-and-ignored `EXCLUDE` clause
- Phase 9: derived tables — `FROM (SELECT …) [AS] alias` as a sole source or a
  join operand
- Phase 9: views (and CTEs) usable as a join source — a view or CTE may now
  appear on either side of a `JOIN`, not just as the sole `FROM` source
- Phase 9: row triggers — `CREATE TRIGGER … [BEFORE|AFTER] {INSERT|UPDATE|DELETE}
  ON t [WHEN …] BEGIN … END` with `OLD`/`NEW` row references and `DROP TRIGGER`;
  fired non-recursively (matching `recursive_triggers = OFF`)
- Phase 9: foreign-key enforcement behind `PRAGMA foreign_keys = ON` (off by
  default, as in SQLite) — child-side parent-existence checks on INSERT/UPDATE,
  and referential actions on the parent (NO ACTION/RESTRICT, CASCADE, SET NULL,
  SET DEFAULT) for DELETE and UPDATE; FK clauses are now modeled in the AST
- Phase 9: window functions — `row_number`, `rank`, `dense_rank`, `ntile`,
  `lag`/`lead`, `first_value`/`last_value`/`nth_value`, and aggregate windows
  (`sum`/`avg`/`count`/`min`/`max`/`total`/`group_concat`) over
  `PARTITION BY`/`ORDER BY` with SQLite's default frame; verified against `sqlite3`
- Phase 9: real-number text formatting now matches SQLite's `%!.15g` exactly
  (15 significant digits, scientific past the `[-4, 15)` exponent window)
- Phase 9: correlated subqueries (scalar `(SELECT …)`, `IN (SELECT …)`) and
  `[NOT] EXISTS (SELECT …)` — the subquery resolves columns from the enclosing
  query's current row via an outer-scope frame stack
- Phase 9: recursive `WITH RECURSIVE` CTEs (anchor + fixed-point recursive term,
  `UNION`/`UNION ALL`), CTEs that reference earlier CTEs, and CTEs usable as a
  join source — backed by a materialized CTE environment
- Phase 9: `EXPLAIN QUERY PLAN` (SCAN/SEARCH plan rows in SQLite's format) and a
  rowid (`INTEGER PRIMARY KEY`) equality fast-path that seeks the table b-tree
  directly instead of scanning
- Phase 9: date/time functions (`date`, `time`, `datetime`, `julianday`,
  `unixepoch`, `strftime`) and `printf`/`format` — a dependency-free port of
  SQLite's `date.c` Julian-day core, verified differentially against `sqlite3`

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
