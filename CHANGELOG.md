# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

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
