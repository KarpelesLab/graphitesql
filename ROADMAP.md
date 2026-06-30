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
  surface; `EXPLAIN QUERY PLAN` shaping (incl. `SCAN CONSTANT ROW`, and an
  aliased constant-row derived table — `FROM (SELECT <consts>) AS s` — rendered as
  `CO-ROUTINE s` / `SCAN CONSTANT ROW` / `SCAN s` byte-exactly; previously any
  derived-table source crashed EQP with a malformed empty `no such table:`; plus a
  pure-wildcard outer over a single base-table body — `SELECT * FROM (SELECT * FROM
  t)` — flattened to the body's own plan the way SQLite does, so an inner
  `WHERE`/`ORDER BY` carries through to `SEARCH`/`TEMP B-TREE`; an *outer* `WHERE`
  over a pass-through wildcard body — `SELECT * FROM (SELECT * FROM t) s WHERE a=5`
  / `SELECT * FROM (SELECT b FROM t WHERE b>0) WHERE b<10` — now also flattens:
  SQLite pushes the predicate into the scan, so graphite ANDs the (unqualified)
  outer predicate into the body and recurses, tightening the `SCAN` into a `SEARCH`
  or adding a range bound (re-deriving the covering-index choice from the body's own
  projection, so a `SELECT b` body keeps `COVERING INDEX` where a `SELECT *` body
  drops to a plain `INDEX`); a *narrower* outer projection of bare unqualified
  columns — `SELECT a FROM (SELECT * FROM t)` / `SELECT x FROM (SELECT x,y FROM u)
  WHERE y>3` — flattens the same way, substituting the outer projection into the body
  so it can pick a `COVERING INDEX` access path the full-row body could not; a column
  qualified by the derived source's own alias / CTE name — `SELECT s.a FROM (SELECT *
  FROM t) s WHERE s.a<5` / `WITH c AS (…) SELECT c.a FROM c` — flattens as well, the
  qualifier being stripped on merge since it names the source itself; an *aliased*
  inner projection — `SELECT aa FROM (SELECT a AS aa FROM t) WHERE aa<5` — flattens
  too, the derived output name `aa` being mapped back to base column `a` on merge so
  the seek lands on the index over `a`; this all holds only when the inner projection
  is bare columns or `*` (a computed `a+1 AS x` has no base column to seek on) and the
  outer references only names the source actually outputs (a reference to a column the
  source doesn't expose — a sqlite `no such column` — declines, as do a foreign outer
  qualifier and an inner join/aggregate/DISTINCT/view/LIMIT);
  a `WITH`-clause CTE referenced as a FROM source renders the same
  way — `WITH c AS (SELECT 1) SELECT * FROM c` → `CO-ROUTINE c`, `WITH c AS (SELECT *
  FROM t) SELECT * FROM c` → `SCAN t` — instead of crashing with `no such table: c`;
  an explicit `WITH c AS MATERIALIZED (…)` hint forces the `MATERIALIZE c` node SQLite
  renders (its child is the body's plan — a table `SCAN`, an index scan, or a
  `SCAN N CONSTANT ROWS` for a multi-row `VALUES` body — followed by the outer
  `SCAN c`/`SEARCH c` plus one optional trailing temp-b-tree), where graphite would
  otherwise flatten the body away; `NOT MATERIALIZED`/absent keeps the flatten, the
  hint never changes the executed rows, and a subquery-bearing `VALUES` body or an
  outer-clause combination decline cleanly (the hint is carried on a new
  `Cte::materialized` field the parser stamps);
  a derived/CTE source combined with a join declines cleanly instead of crashing; a
  compound query — `SELECT … UNION/UNION ALL/INTERSECT/EXCEPT …` — renders the
  `COMPOUND QUERY` / `LEFT-MOST SUBQUERY` / per-arm-operator tree byte-exactly
  (3+ arms, per-arm WHERE seeks, a shared `WITH`, and a bare `LIMIT`/`OFFSET` all
  carry through); a trailing `ORDER BY` on the compound switches SQLite to its
  separate `MERGE (<OP>)` plan, which graphite now renders for positional
  (`ORDER BY 1`) or bare named/aliased (`ORDER BY a`, `ORDER BY b,a`, matched
  case-insensitively against the result-set names and rewritten to positional for
  the arms) terms with default null-ordering — each arm is recursed with the
  `ORDER BY` pushed in and the arms combine left-associatively under nested `MERGE`
  nodes (`LEFT` = accumulated head, `RIGHT` = next arm; every operator including
  `UNION ALL` uses `MERGE` once an `ORDER BY` is present, a whole-compound
  `LIMIT`/`OFFSET` and a per-arm `WHERE` `SEARCH` carry through); a *partial-cover*
  `ORDER BY` also renders — a merge sorts an arm by the whole output row whenever a
  de-duplicating operator (`UNION`/`INTERSECT`/`EXCEPT`) governs it (the set
  operation compares full rows), so SQLite appends the not-yet-covered output
  columns (ascending) to that arm's sort, surfacing as a per-arm `USE TEMP B-TREE
  FOR [LAST [N TERMS] OF] ORDER BY`; an arm is governed by a dedup op iff one
  appears in the operator suffix from that arm onward (in `a UNION b UNION ALL c`
  the trailing `c` keeps a bare sort while `a`/`b` append; a pure `UNION ALL`
  compound appends nothing), reproduced per-arm; a *redundant* explicit `NULLS`
  clause whose placement matches the natural index/PK walk (`ASC NULLS FIRST` /
  `DESC NULLS LAST`, i.e. `nulls_first == !descending`) is now treated exactly like
  a bare term — across the single-table scan, the covering scan, a `WHERE`-seek
  prefix, and each compound `MERGE` arm — so SQLite serves it from the index with no
  sorter; the *opposite* placement (`ASC NULLS LAST` / `DESC NULLS FIRST`) is a
  two-pass index scan graphite does not model, so it keeps its sorter (a known EQP
  residual; rows stay correct), shared via the `redundant_nulls` helper; an explicit
  `COLLATE` (SQLite falls to a CO-ROUTINE+materialize shape), a non-column
  expression term, and a `*` projection all still decline cleanly; a top-level multi-row `VALUES (…),(…),…` clause —
  which graphite desugars to `UNION ALL` arms — folds into the single
  `SCAN N-ROW VALUES CLAUSE` node SQLite renders (a one-row `VALUES` stays
  `SCAN CONSTANT ROW`), and when such a clause is the left-most arm of, or an operand
  within, a *real* compound only its own rows fold while the genuine boundaries keep
  their operator nodes — driven by a new `Select::values_rows` row-count the parser
  stamps (purely informational, distinguishing a real `VALUES` from a hand-written
  `SELECT … column1 UNION ALL …`); a row carrying a subquery (SQLite's plural
  `SCAN N CONSTANT ROWS` + interposed subquery nodes) declines cleanly; the same
  multi-row clause used as a derived `FROM` source — `SELECT * FROM (VALUES(1,2),(3,4))`
  — also folds to a bare `SCAN N-ROW VALUES CLAUSE` (no co-routine wrapper) plus at
  most one trailing outer `USE TEMP B-TREE FOR ORDER BY|GROUP BY|DISTINCT`, with a lone
  `min`/`max` switching it to `SEARCH N-ROW VALUES CLAUSE`; the *single-row* FROM-source
  (still co-routine-wrapped), a subquery-bearing row, a multi-table FROM/JOIN, and a
  combination of outer clauses all decline cleanly; the same multi-row clause used as a
  *CTE body* — `WITH c AS (VALUES(1,2),(3,4)) SELECT * FROM c` — materializes as
  `CO-ROUTINE c` whose child is `SCAN N CONSTANT ROWS` (the plural phrasing, distinct
  from the FROM-source's `SCAN N-ROW VALUES CLAUSE`) followed by the outer
  `SCAN c`/`SEARCH c` plus one optional trailing temp-b-tree (a single-row body is the
  singular `SCAN CONSTANT ROW`; a subquery-bearing row, a join across CTEs, and a
  combination of outer clauses decline); a *compound* body carrying at least one dedup
  set operator — `SELECT * FROM (SELECT … UNION/INTERSECT/EXCEPT …) s` and
  `WITH c AS (SELECT … UNION …) SELECT * FROM c` — likewise can't flatten, so it
  materializes as `CO-ROUTINE {s|c}` whose single child is the body's `COMPOUND QUERY`
  subtree (recursed normally — `LEFT-MOST SUBQUERY` plus one operator node per arm,
  including any interspersed `UNION ALL`) followed by the outer `SCAN`/`SEARCH` plus one
  optional trailing temp-b-tree (with `count(*)` keeping the `SCAN` and a lone `min`/`max`
  switching to `SEARCH`); an unaliased derived table (SQLite numbers it `(subquery-N)`),
  a `UNION ALL`-only body (it streams without a dedup b-tree and flattens to a bare
  `COMPOUND QUERY`), and an outer `WHERE` (the predicate pushes into the arms,
  re-deriving their scans) each decline cleanly; an aggregate
  `GROUP BY a ORDER BY a` answered by
  a covering index on `a` emits a bare `SCAN … USING COVERING INDEX` with **no**
  temp-btree node — the group-by access already yields the ORDER BY term in order, so
  zero terms need sorting and graphite no longer over-emits a nonsensical
  `USE TEMP B-TREE FOR LAST 0 TERMS OF ORDER BY`; and an `ORDER BY` that is *longer*
  than the chosen index — `ORDER BY a, b` over an index on `(a)` — walks the index
  for the `a` prefix and sorts only the trailing terms (`SCAN t USING INDEX it` +
  `USE TEMP B-TREE FOR LAST TERM OF ORDER BY`), where graphite previously rejected any
  index shorter than the ORDER BY and fell back to a full `SCAN t` + full sort; and a
  single-table `GROUP BY` / `DISTINCT` over a plain `SCAN` now emits
  `USE TEMP B-TREE FOR GROUP BY` / `FOR DISTINCT` after the `SCAN` line, exactly like
  sqlite — restricted to the clean bare-scan case (`GROUP BY`/`DISTINCT` on the rowid
  alone needs no node; a secondary index leading with the first key, which sqlite would
  walk instead of plain-scanning, is declined so the plan never desyncs), with the
  covering-index-but-not-leading and `WITHOUT ROWID` shapes still deferred). When an
  `ORDER BY` accompanies that bare-scan grouping/dedup, sqlite reuses the grouping
  b-tree to satisfy the sort — suppressing the separate `USE TEMP B-TREE FOR ORDER BY`
  node exactly when the ORDER BY key list equals the grouping key list (`GROUP BY`
  tolerates any per-column direction; a `DISTINCT` b-tree is ascending-only; the default
  NULL ordering must hold). graphite now mirrors that suppression for plain-column ORDER
  BY terms, declining the whole node for positional / alias / expression / `COLLATE`
  terms (left at their prior single-node rendering, never a new divergence). A
  multi-term `ORDER BY` whose *leading* term is the rowid / INTEGER PRIMARY KEY is
  likewise served entirely by the full scan — because that key is unique no trailing
  term can break a tie — so `ORDER BY id, b` skips the sort and its temp-btree node
  exactly like a lone `ORDER BY id` (previously only the single-term form was
  recognised). The trailing-rowid-*within-a-secondary-index* case is handled too:
  every secondary index on a rowid table is implicitly ordered by `(key columns…,
  rowid)` with the rowid stored *ascending*, so `ORDER BY b, id` over an index on
  `(b)` — or `ORDER BY b, c, id` over `(b, c)` — is fully served by the index walk
  with no temp-btree node, matching sqlite. The credit is withheld whenever the
  ascending rowid would fall out of phase: a `DESC` index column (reversed walk),
  a mixed-direction boundary (`ORDER BY b, id DESC`), or a non-rowid column sitting
  between the matched prefix and the rowid (`ORDER BY b, id` over `(b, c)`); those
  keep their prior `LAST TERM OF ORDER BY` sort. A *named* `UNIQUE` index gets the
  same credit (its entries are still `(key…, rowid)`, with multiple NULLs broken by
  the rowid), since a `CREATE UNIQUE INDEX` carries accurate per-column directions;
  an *automatic* UNIQUE/PK index is excluded (its directions are not reconstructed,
  so a `UNIQUE(b DESC)` constraint must not be mis-credited) — a deferred
  `order_index_scan` gap. The same trailing-rowid credit now also applies on the
  `WHERE`-*seek* path (`seek_order_prefix`): once a seek has walked an index's whole
  key (after any equality-pinned prefix) the walk continues in rowid order, so
  `WHERE b>2 ORDER BY b, id` or `WHERE b=3 ORDER BY id` over an index on `(b)` is
  served by the seek with no temp-btree node, with the same DESC / mixed-direction /
  automatic-index exclusions. An equality-*pinned* `ORDER BY` term is now elided too:
  a column constrained by `WHERE col = <literal>` is constant across the seeked rows,
  so sqlite drops any `ORDER BY` term on it — `WHERE b=3 ORDER BY b, id` skips the
  sort just like `WHERE b=3 ORDER BY id`, even when the pinned term *leads* (the
  effective walk direction then comes from the first non-constant term, so
  `WHERE b=3 ORDER BY b, id DESC` walks the rowid descending). The pin applies to any
  equality column, indexed or not (`WHERE b=3 AND c=5 ORDER BY c, id`). A `GROUP BY`
  on the rowid / INTEGER PRIMARY KEY is recognised as degenerate too: each group is a
  single row emitted in rowid order, so sqlite plain-`SCAN`s the table (never a
  covering index — `group_by_is_rowid` now stands `covering_scan` down) and skips the
  `USE TEMP B-TREE FOR ORDER BY` for a sole `ORDER BY` term on that same key, both
  directions (`SELECT id FROM t GROUP BY id ORDER BY id [DESC]`), including with an
  aggregate (`count(*)`) or an aggregate `HAVING`. It is withheld for a multi-term
  `ORDER BY` (sqlite does not elide trailing terms via key-uniqueness in the GROUP BY
  case) and a non-rowid `GROUP BY`. (A `HAVING` *range* on the group-key rowid — which
  sqlite pushes down to a `SEARCH … (rowid>?)` — is a separate deferred pushdown gap;
  rows stay correct.) A `DISTINCT` whose projection *pins* the rowid / INTEGER PRIMARY
  KEY is likewise recognised as a **no-op** (`distinct_is_noop`): the key is unique per
  row, so de-duplication removes nothing and sqlite plans the query exactly as if
  `DISTINCT` were absent — no `USE TEMP B-TREE FOR DISTINCT` node, a bare `SCAN`
  (`covering_scan` stands down via `rowid_ordered_scan`), and a sole leading `ORDER BY`
  on that key skips the sort too (`SELECT DISTINCT id FROM t ORDER BY id [DESC]`,
  `SELECT DISTINCT id, a FROM t ORDER BY id`, `SELECT DISTINCT * FROM t ORDER BY id`,
  and the implicit-rowid `SELECT DISTINCT rowid FROM u …`). A bare `*` / `t.*` counts
  (the IPK is in its expansion); an *expression* projection (`id+0`) does not (sqlite
  keeps its DISTINCT b-tree) and is left untouched. (Deferred, orthogonal: a no-`ORDER
  BY` `DISTINCT`/covering scan still emits rows in rowid order rather than sqlite's
  index-walk order; and `WITHOUT ROWID` DISTINCT/grouping.) A pure
  `rowid = a OR rowid = b OR …` equality OR-chain — the key being the `rowid` alias
  *or* the explicit INTEGER PRIMARY KEY column — collapses to a single
  `SEARCH … USING INTEGER PRIMARY KEY (rowid=?)` rather than a `MULTI-INDEX OR`, exactly
  as sqlite plans it: `rowid_seek_constraint` now recognises the IPK column (not just
  the alias) and an all-equality OR-chain (`rowid_eq_or_chain`), so the executor's
  existing rowid fast path seeks the unioned candidates (a valid superset — `run_core`
  re-applies the WHERE) and `eqp_or_plan` declines, letting the single SEARCH node
  render (`SELECT * FROM t WHERE id=3 OR id=5 [OR id=2]`, `(id=3 OR id=5) AND a>0`, the
  `rowid`-alias and implicit-rowid spellings). The collapse is scoped to the pure
  all-equality case: an `IN`-list disjunct (`id=3 OR id IN(4,5)`), a non-rowid disjunct
  (`id=3 OR a=5`), or any mix keeps sqlite's `MULTI-INDEX OR` — `rowid_eq_or_chain`
  rejects the non-equality leaf (`tests/eqp_rowid_or_seek.rs`). A `rowid`/INTEGER-PK
  `IN`-list (or the equivalent same-column equality OR-chain) combined with `ORDER BY`
  on that rowid now elides the `USE TEMP B-TREE FOR ORDER BY`, matching sqlite, which
  seeks the listed rowids in *sorted* order: `in_seek_order` is the single shared
  predicate driving both the EQP elision and the executor — when it fires, the rowid
  seek sorts its keys so rows emit in ascending rowid order and `run_core` reverses for
  `DESC` (`id IN(5,1,3) ORDER BY id [ASC|DESC]`, the OR-chain spelling, `ORDER BY rowid`,
  a multi-term `ORDER BY id, b` — `tests/eqp_rowid_in_order_by_seek.rs`). (Deferred,
  with a tripwire test: the same `ORDER BY` elision for a *secondary*-index `IN`
  (`a IN(..) ORDER BY a`, which sqlite serves from the ordered covering index), for a
  `WITHOUT ROWID` PK, and for a bare-`rowid` alias table with no INTEGER PRIMARY KEY —
  graphite still builds the temp b-tree there; rows stay correct.) A same-column equality OR-chain on a
  *secondary* index column (`a = 1 OR a = 2 OR …`, every disjunct a bare equality on
  the *same* column) is likewise the equivalent of `a IN (1, 2, …)` and collapses to a
  single `SEARCH … USING INDEX` seek (or a `SCAN` when the column has no index), exactly
  as sqlite plans the IN-list: `find_in_constraint` now recognises the chain (via
  `flatten_or` + an `eq_col_const` leaf that rejects mixed columns and NULL keys), so
  the executor's existing `try_index_in` seeks the unioned values (a valid superset)
  and `eqp_or_plan` declines, letting the single node render (`a=5 OR a=1 [OR a=8]`,
  `(a=5 OR a=1) AND b>0`, reversed/parenthesised leaves; an unindexed `b=… OR b=…`
  chain stays one `SCAN`; a mixed-column `a=5 OR b=2` keeps `MULTI-INDEX OR`). The
  collapse tracks graphite's own `IN` path exactly, so the only residual vs sqlite is
  the *pre-existing* `IN` covering-index choice (graphite picks the narrow `ia`, sqlite
  the covering `iab`) — identical for `a IN (5,1)` and `a=5 OR a=1`, no new divergence
  (`tests/eqp_secondary_or_seek.rs`). On a `WITHOUT ROWID` table the same `IN`/OR-chain
  on the *leading* PRIMARY KEY column now seeks the clustered b-tree once per value
  (`try_without_rowid_pk_in`) rather than scanning, rendering one
  `SEARCH … USING PRIMARY KEY (k=?)` exactly as sqlite plans it: previously graphite
  recognised only single-equality and range bounds on the WITHOUT ROWID PK, so an
  `IN`/`OR` fell to a full `SCAN`. Each distinct leading-PK value addresses a disjoint
  b-tree slice (composite PK leading column prefix-seeks), so repeated values
  de-duplicate and the concatenation is a valid superset; a non-leading-PK or unindexed
  column still scans (`SELECT * FROM t WHERE k IN ('a','c')`, `k='a' OR k='c'`, the
  INTEGER/TEXT/composite-PK and `NOT INDEXED` cases — `tests/eqp_without_rowid_pk_in_seek.rs`).
  `EXPLAIN QUERY PLAN` now names an *aliased* base table by its **alias alone**
  (`SCAN x`, `SEARCH x USING INDEX ia (a=?)`, `SCAN x VIRTUAL TABLE …`), exactly as
  sqlite does, rather than the `table AS alias` form graphite used to print: `eqp_label`
  returns the alias when present. The single quirk sqlite makes is the bare `count(*)`
  covering-index plan, which it labels with the *table name* even when aliased
  (`SCAN t USING COVERING INDEX ia`) — the `count_covering_index` EQP branch mirrors that
  by using `from.first.name`. Adjacent fix: the inner side of a `LEFT JOIN` now carries
  sqlite's ` LEFT-JOIN` suffix on its `SEARCH` node for every seek kind (rowid IPK,
  secondary index, `WITHOUT ROWID` PK, automatic covering index) — a shared `left_suffix`
  applied across all four join EQP branches; previously the rowid/index join branches
  omitted it (`tests/eqp_alias_label.rs`, and the corrected hardcoded assertions in
  `tests/explain.rs`, `tests/join_seek.rs`, `tests/join_index_seek.rs`).
  A WHERE-equality / range / IN seek whose chosen index holds every referenced
  column — *including through aggregate/function arguments* (`SELECT count(*) …
  WHERE a=?`, `sum(a) … WHERE a=?`) — is now labelled `USING COVERING INDEX`,
  matching sqlite: `seek_index_covers` (which drives BOTH the EQP label and the
  executor's index-only read, so they stay in lockstep) now routes through
  `query_cols_covered` (recurses into function args) rather than the plain-column
  `index_covers_query`, so an aggregate referencing no uncovered column qualifies
  (`tests/eqp_covering_seek_aggregate.rs`). (Residual: when sqlite picks a *wider*
  covering index than graphite — `SELECT b FROM t WHERE a=?` where graphite seeks
  the narrow `ia` and sqlite the covering `iab` — graphite honestly labels its
  narrower index `USING INDEX`; that is the separate cost-model index-*choice* gap,
  unchanged. `min/max … WHERE col=?` SCAN-vs-SEARCH via one-end index read is also
  still open.)
  A `col IS NULL` conjunct is now a *seekable* NULL-key equality: sqlite renders
  `SEARCH … USING [COVERING] INDEX … (col=?)` for it (NULLs sort first in the
  index and `cmp_values` treats `NULL == NULL` as equal, so the prefix seek finds
  exactly the NULL-keyed entries), and graphite now matches — previously it SCANned
  because the eq-constraint collector only recognised `col = const`. A new
  `collect_isnull_cols` feeds the index chooser (and `eqp_access`, in lockstep) a
  `Value::Null` key per `col IS NULL`, covering single-column seeks, covering
  aggregates (`count(*)/sum(a) … WHERE a IS NULL` → `USING COVERING INDEX`),
  composite NULL-or-mixed prefixes (`a IS NULL AND b IS NULL`, `a=1 AND b IS NULL`),
  and a range on the column after a NULL equality prefix (`a IS NULL AND b>?`). The
  constraint is tracked *apart* from value equalities so the rowid / INTEGER PRIMARY
  KEY fast paths never fire for `rowid IS NULL` (sqlite SCANs there — the rowid is
  never NULL) and so `col = NULL` (never true) keeps bailing unchanged
  (`tests/eqp_isnull_seek.rs`). The same NULL-key seek now also extends to a
  `WITHOUT ROWID` table's secondary index (`try_without_rowid_index_seek` + its
  `eqp_access` branch, in lockstep): `col IS NULL` over a secondary index whose
  records carry the trailing PK seeks the index (`SEARCH … USING [COVERING] INDEX
  … (col=?)`) instead of scanning, while `pk IS NULL` (the PK is NOT NULL) keeps
  scanning. (Residuals: the index-*choice* tiebreak above applies when two
  equal-prefix indexes qualify; and a *multi-row composite covering* seek on a
  `WITHOUT ROWID` table emits rows in PK order where sqlite emits index order — a
  separate, pre-existing ordering quirk unrelated to IS NULL.)
  A `WHERE` seek that pins a column to a *single value* now lets graphite elide a
  matching `ORDER BY` (no `USE TEMP B-TREE FOR ORDER BY`), as sqlite does, in two
  more cases feeding `order_satisfied_by_scan`. (1) A bare `rowid` / INTEGER
  PRIMARY KEY equality — `id = 5`, the `rowid` alias, or a one-element `IN` —
  returns at most one row, so *any* `ORDER BY` is already satisfied regardless of
  which columns it names (`rowid_eq_single_row` keys off `rowid_seek_constraint`
  returning a single candidate; a multi-value `IN`/OR-chain stays with
  `in_seek_order`, which checks the leading term). (2) A `col IS NULL` conjunct
  pins `col` to its single NULL key exactly like a value equality, so an
  `ORDER BY` term on it is constant and drops out, while a following term on an
  index-walked column keeps the seek order: `seek_order_prefix` now folds
  `collect_isnull_cols` into both its index choice and its constant-drop loop
  (`a IS NULL ORDER BY a [DESC]`, `a IS NULL ORDER BY a, b` / `ORDER BY b` over an
  `(a,b)` index, both columns pinned via `a=2 AND b IS NULL ORDER BY a, b`) —
  EQP and rows byte-exact vs sqlite (`tests/eqp_orderby_seek_elision.rs`).
  (Deferred, pre-existing and unrelated: an empty `id IS NULL` result sqlite
  orders by *scanning a secondary index* — `SCAN … USING INDEX ib` — and an
  `IS NOT NULL` range seek (`a > NULL`); graphite still scans + sorts there, rows
  correct.)
  A `WHERE`-bearing single-table `SELECT` *without* `ORDER BY` whose seek SQLite
  answers by walking a secondary index now returns rows in *index-key* order on
  the VDBE path too. The VDBE executes such a query as a rowid-order table scan,
  which diverges from index order for any multi-row seek (a range bound, a
  covering `IS NOT NULL`, an equality-prefixed range on a composite index) — a
  latent gap the differential corpus missed because it ORDER-BYs or doesn't
  project bare indexed columns. `run_select_vdbe` now defers exactly those shapes
  to the tree-walker (whose seek paths already walk the index in key order) via a
  `vdbe_seek_returns_index_order` predicate; single-key seeks (`a=?`, `a IS NULL`,
  one-element `IN`) keep rowid order and stay on the VDBE, and any explicit
  `ORDER BY` makes the order access-path-independent so those stay too. This also
  lands a new covering plan, `try_isnotnull_covering`: `SELECT a … WHERE a IS NOT
  NULL` over an index on `a` now reads `USING COVERING INDEX a (a>?)` (NULLs sort
  first), while the near-full-table non-covering `SELECT *`/`SELECT b` stay a
  plain `SCAN` on both sides — EQP and rows byte-exact vs sqlite3 3.50.4
  (`tests/vdbe_secondary_index_order.rs`).
  A follow-up extends the same defer to a **multi-value `IN`** on a secondary
  index: SQLite seeks the list in sorted key order (so the rows arrive in index
  order), so `in_seek_fetch` now sorts its seek keys under the index collations,
  and `vdbe_seek_returns_index_order` defers a `col IN (…)` whose column leads a
  plain, partial (predicate proven), or expression index — covering and
  non-covering, with NULL list entries dropped before the seek (`a IN (5,NULL,2)`
  matches the same rows as `a IN (5,2)`) and duplicate/absent values folded. A
  rowid/IPK `IN` keeps walking the table b-tree in rowid order (= the VDBE scan
  order) so it stays on the VDBE, and a single non-NULL key stays a single-key
  seek. (Deferred, pre-existing and not regressed: a parameterized range bound
  `a > ?`, whose constant isn't known at the defer-decision point, so it stays on
  the VDBE; and the `ORDER BY a DESC` duplicate-key tiebreak — the VDBE's stable
  sort keeps equal-key rows in rowid order where SQLite's reverse index walk
  emits them in reverse-rowid order — an orthogonal DESC-sort quirk.)
  A second follow-up closes the same divergence for a **no-`WHERE` covering scan**:
  `SELECT a FROM t` (or `SELECT DISTINCT a`, composite `SELECT a,b` over `(a,b)`)
  with no `ORDER BY` is served by the tree-walker's `covering_scan` — reading the
  covering secondary index in key order — while the VDBE rowid-scans the table.
  `run_select_vdbe` now defers whenever `vdbe_covering_scan_reorders` (a thin wrapper
  over `covering_scan`'s own applicability check) fires, so the rows arrive in index
  order matching sqlite3 3.50.4; `SELECT *`/non-covered projections keep the rowid
  `SCAN`, and an `ORDER BY` re-sorts independently of the access path
  (`tests/vdbe_covering_scan_order.rs`). (Pre-existing, not regressed: a redundant
  index on the IPK still mislabels its EQP `USING COVERING INDEX` vs sqlite's bare
  `SCAN`, and two identical covering indexes leave both EQP and seek-order ambiguous
  — `covering_scan` declines rather than guess sqlite's cost-model choice. Both need
  separate EQP-side work and arise only from degenerate schemas.) A non-correlated
  scalar `(SELECT …)` in the `WHERE` clause now renders its `SCALAR SUBQUERY N`
  node — a sibling of the outer scan, numbered left-to-right, with the subquery
  body's own plan recursed as its child, placed after the scan and before any
  GROUP BY / ORDER BY sorter — where graphite previously emitted no node at all and
  diverged on every such query. SQLite shares one sequential subquery id across CTE
  materialisations and compound arms, so the numbering is only a clean `1..n` (all
  we render) when the statement has no CTEs, the subqueries live solely in the
  `WHERE` clause, and each is a non-correlated, non-compound scalar `(SELECT …)`
  over base tables with no further nested subquery; everything else is declined,
  leaving the pre-existing bare `SCAN` (a correlated body, `EXISTS`, `IN (SELECT)`,
  a CTE/compound body that bumps the counter to `SCALAR SUBQUERY 2`, or a subquery
  in the projection rather than the `WHERE`). Since sqlite *always* emits a node for
  a scalar `WHERE` subquery, adding the correct one can only converge a plan, never
  regress a passing one (`tests/eqp_where_scalar_subquery.rs`). The same renders for
  a scalar `(SELECT …)` in the **projection** (`SELECT (SELECT …), … FROM t`),
  numbered `1..n` in column order — the only difference being sequencing: a
  projection subquery is evaluated post-grouping, so SQLite places its node *after*
  a `USE TEMP B-TREE FOR GROUP BY` sorter but *before* DISTINCT / ORDER BY. Our
  single insertion point (right after the scan) matches the no-GROUP-BY shapes, so
  GROUP BY / HAVING are declined; `DISTINCT` is declined too because graphite's
  separate distinct sorter node does not fire when a projection column is a subquery
  (emitting the scalar node there would leave the plan still missing the DISTINCT
  node rather than byte-exact — we never render a node into a non-matching plan).
  The two positions are mutually exclusive (each collector declines if the other
  holds a subquery), so a subquery in *both* projection and WHERE — SQLite's
  cross-position `SCALAR SUBQUERY 2` then `1` numbering — declines cleanly
  (`tests/eqp_projection_scalar_subquery.rs`). The same node now also renders for a
  scalar `(SELECT …)` in an **`ORDER BY`** term (`… ORDER BY (SELECT …)`), numbered
  `1..n` in term order; an ORDER BY subquery is sequenced exactly like a WHERE one —
  after the scan, before the `USE TEMP B-TREE FOR ORDER BY` sorter — so the same
  insertion point matches. GROUP BY / HAVING are declined (the node would sequence
  after the grouping sorter) and so is `DISTINCT` (a separate distinct sorter whose
  ORDER BY interplay we do not model); a body join / compound / correlation / `EXISTS`
  and a subquery in another clause decline as in the other two forms
  (`tests/eqp_orderby_scalar_subquery.rs`). The **GROUP BY projection** case the
  un-grouped collector declines now renders via a *second* insertion point: a
  projection `(SELECT …)` in a grouped query is sequenced *after* the
  `USE TEMP B-TREE FOR GROUP BY` sorter (and any distinct-aggregate b-trees) but
  *before* an ORDER BY sorter, so its node is emitted there — numbered `1..n` in
  column order, with a non-subquery `HAVING` and a folded/unfolded `ORDER BY` riding
  along. `DISTINCT` (separate distinct sorter), a `HAVING`/`ORDER BY` subquery (which
  reorders or renumbers the nodes), and a correlated/compound/join body all decline,
  unchanged from before (`tests/eqp_grouped_projection_scalar_subquery.rs`). The same
  `SCALAR SUBQUERY 1` node now also renders for an **UPDATE/DELETE** carrying a single
  non-correlated scalar subquery in a `SET` assignment or the `WHERE` clause
  (`UPDATE t SET c=(SELECT count(*) FROM u) WHERE b=1`, `DELETE FROM t WHERE
  c=(SELECT …)`): the node is a sibling of the access node, its body recursed as the
  child, where graphite previously emitted only the access node. Only the
  single-subquery case is rendered — SQLite emits several `SET` subqueries in source
  order but numbered in *reverse* (codegen-fragile), and a correlated body
  (`CORRELATED SCALAR SUBQUERY`) / `IN (SELECT)` (`LIST SUBQUERY` + bloom) are
  different shapes — so multi-subquery, `UPDATE … FROM`, row-value `SET (…)=(SELECT)`,
  a CTE, a trailing `ORDER BY`/`LIMIT`, and `RETURNING` all decline to the bare access
  node. A **single-row `INSERT … VALUES`** carrying one such scalar subquery
  (`INSERT INTO t(b) VALUES((SELECT count(*) FROM u))`) renders the same node — and,
  having no scan of its own, the node *is* the whole plan (`SCALAR SUBQUERY 1` at the
  root); a multi-row `VALUES` (which adds a `SCAN N CONSTANT ROWS` node), several value
  subqueries, an `INSERT … SELECT`, a CTE, an upsert, or `RETURNING` decline
  (`tests/eqp_dml_scalar_subquery.rs`).
- **EQP recursive CTE** — a `WITH RECURSIVE c(…) AS (<anchor> UNION[ ALL]
  <recursive>) SELECT … FROM c` source now renders SQLite's `CO-ROUTINE c` subtree
  byte-exactly: a `SETUP` child holding the non-recursive anchor arm's plan (recursed
  normally — `SELECT <consts>`/`VALUES(…)` → `SCAN CONSTANT ROW`, `SELECT … FROM t` →
  `SCAN t`), a `RECURSIVE STEP` child whose self-reference reads as a plain `SCAN c`
  of the materialized table, then the outer `SCAN c`. Previously any such query
  errored `EXPLAIN QUERY PLAN for this query shape`. `eqp_select` detects the
  canonical two-arm split with the executor's own `references_name_select` (one
  anchor arm that does not name the CTE, one recursive arm whose `FROM` is a bare
  reference to it) and renders it when the outer query adds no further node (an outer
  `WHERE` and a bare aggregate add none). The outer access over the materialized
  co-routine is normally a `SCAN`, but a lone `min()`/`max()` aggregate seeks one end
  and reads as **`SEARCH c`** (no index detail — a co-routine has none); a second
  aggregate keeps the `SCAN`, and a `min(DISTINCT …)` (which interposes a
  `USE TEMP B-TREE FOR min(DISTINCT)` node) declines. `UNION` and `UNION ALL` are the
  same plan. A *single* outer `ORDER BY`, `GROUP BY`, or `DISTINCT` now also renders,
  appending one root-level `USE TEMP B-TREE FOR ORDER BY`/`GROUP BY`/`DISTINCT` node
  after the outer scan (the min/max `SEARCH` access applies independently, so
  `DISTINCT max(n)` is `SEARCH c` plus the DISTINCT sorter). A join in the recursive
  arm (`FROM c, t` — a second scan child), a *combination* of those outer clauses
  (SQLite folds/reorders the temp-b-tree nodes), and `min(DISTINCT …)` decline
  cleanly, keeping the prior error rather than emitting a wrong plan; the executed
  rows always match (`tests/eqp_recursive_cte.rs`).
- **Recursive-CTE base-anchor execution** — *executing* (not just `EXPLAIN`ing) a
  recursive CTE whose body carried a base-table source — a base-table anchor
  (`SELECT a FROM t UNION ALL …`), a single-row aggregate/filtered anchor, or a
  base-table join in the recursive arm — used to crash with a stack overflow before
  producing any row; the textbook `SELECT <const>` anchor sidestepped it, so the bug
  lurked. Column-origin resolution was the fault: `named_source_origins_in` resolved a
  `FROM c` reference by descending into the CTE body with `c` *still in scope*, so the
  body's own self-reference re-entered the same path without end. A recursive body's
  origins are conservatively `None` anyway, so the fix stops at a self-naming CTE
  (returns `None`) and otherwise resolves the body with that CTE dropped from scope —
  terminating while keeping sibling references resolvable. These cases now run and
  match `sqlite3` 3.50.4 row-for-row (`tests/recursive_cte_base_anchor.rs`).
- **EQP min/max optimization** — a query whose only aggregate is a single
  `min(X)`/`max(X)` (no `GROUP BY`/`HAVING`/`WHERE`/statement-level `DISTINCT`, no
  second aggregate; the call may be scalar-wrapped like `abs(min(a))`/`max(a)+1`)
  reads one end of an ordered scan, so SQLite renders the access as **`SEARCH`**
  rather than `SCAN`. graphite previously labelled it `SCAN` (it still *executes*
  the aggregate over an ordinary covering scan — one output row, so only the label
  differed and the value already matched). `eqp_select` now recognises the shape via
  `minmax_search_detail` / `single_minmax_shape` and emits the right `SEARCH` clause:
  - a full index covering *every* referenced column → `SEARCH t USING COVERING
    INDEX <name>` (the argument may be a bare column, an expression `min(a+1)`, or a
    constant `min(1)`);
  - else a bare column beside the aggregate (`min(a), b`) seeks the sole index that
    *leads* with the aggregated column and reads the rest from the table by rowid →
    `SEARCH t USING INDEX <name>` (non-covering);
  - else a bare `SEARCH t`.
  A **`WITHOUT ROWID`** table is its own clustered primary-key b-tree (it carries
  every column), so a non-covering seek reads `SEARCH t USING PRIMARY KEY` — unless a
  secondary index covers the aggregated column, preferred as `… USING COVERING
  INDEX <name>`. For **`min(DISTINCT col)`** SQLite materializes the distinct values
  in a `USE TEMP B-TREE FOR min(DISTINCT)` node *except* when the call is the sole
  reference and its column **leads** the seek b-tree (a secondary index beginning
  with it, or the first PRIMARY KEY column of a `WITHOUT ROWID` table) — then the
  node is elided and graphite renders the bare covering/PRIMARY-KEY seek; every other
  `DISTINCT` shape (non-leading column, extra reference, expression argument) is
  declined to the ordinary access path. Byte-exact vs sqlite3 3.50.4
  (`tests/eqp_minmax_search.rs`). (Deferred, rendered differently by sqlite and out
  of scope: a `WHERE` clause (sqlite serves the seek from the WHERE index); the
  `DISTINCT` temp-b-tree shapes above; and the ambiguous case of ≥2 equally-covering
  or ≥2 leading indexes, where sqlite's cost model picks one — graphite declines.)
- **EQP `DISTINCT`-aggregate temp-b-tree** — every `f(DISTINCT col)` aggregate other
  than `min`/`max` collects its distinct values through a private sorter, which SQLite
  renders as a **`USE TEMP B-TREE FOR <f>(DISTINCT)`** node — one per *unique* such
  aggregate (the function name lowercased: `count`/`sum`/`avg`/`total`/`group_concat`),
  placed *before* the scan line, in result-column order. `eqp_select` emits these via
  `distinct_agg_btrees` exactly when the access path is a bare full `SCAN t` (no index
  then delivers the distinct values pre-ordered; a `WHERE`/`ORDER BY` that engages no
  index leaves the scan bare and the node stands, while a covering/seeking index moves
  the scan off the bare form and elides the node through the ordinary access path). Two
  rules mirror sqlite's `AggInfo`: identical calls coalesce (`count(DISTINCT b)+count(DISTINCT b)`
  → one node), and when the bare scan already yields the column ordered — the
  rowid-aliasing `INTEGER PRIMARY KEY`, or the leading PK column of a `WITHOUT ROWID`
  table — and the lone distinct aggregate is the *entire* computation (one unique
  distinct aggregate, no other aggregate, no bare column), the node is elided; a second
  aggregate, a bare column, or a non-leading distinct column brings it back. Under
  `GROUP BY` the scan order serves the group key, so nothing is elided: each unique
  distinct aggregate spills through its own b-tree *after* the `USE TEMP B-TREE FOR
  GROUP BY` node, in result-column order (same coalescing); grouping a rowid table by
  its own `INTEGER PRIMARY KEY` skips the GROUP-BY sorter and emits no node. Byte-exact
  vs sqlite3 3.50.4 (`tests/eqp_distinct_agg_btree.rs`). (Deferred: multi-argument
  `DISTINCT`, which sqlite rejects at prepare time; `min`/`max(DISTINCT)` on the SEARCH
  path. The indexed-`GROUP BY` scan line itself — `SCAN t USING INDEX ia` — is a
  pre-existing access-path rendering gap, independent of these nodes.)
- **EQP bare-aggregate `ORDER BY` elision** — an aggregate query with **no `GROUP BY`**
  (`SELECT count(*) FROM t ORDER BY 1`) collapses the table to exactly one row, so the
  `ORDER BY` is a no-op and SQLite plans no sorter. `eqp_select` now suppresses its
  `USE TEMP B-TREE FOR ORDER BY` node for that single-row shape (aggregate present, empty
  `GROUP BY`, no window function), keyed off the shape alone — independent of `WHERE`,
  the result-column count, or which column the irrelevant `ORDER BY` names; the min/max
  SEARCH path elides too. A `GROUP BY` (multi-row) keeps the sorter, and a window function
  is excluded (per-row output). Byte-exact vs sqlite3 3.50.4
  (`tests/eqp_aggregate_order_by.rs`). (Still divergent, separate slice: window-coroutine
  EQP rendering.)
- **EQP `ORDER BY`-into-grouping fold** — when a bare-`SCAN` aggregate spills its key
  through a `USE TEMP B-TREE FOR GROUP BY` / `… FOR DISTINCT` node, that b-tree already
  delivers key order, so SQLite reuses it for the `ORDER BY` and emits no separate sorter.
  `group_distinct_btree` now decides this fold after resolving each `ORDER BY` term to a
  grouping key column — directly, by 1-based position (`ORDER BY 1`), or through an output
  alias (`SELECT a AS x … ORDER BY x`) — and folds iff the resolved term list equals the
  key list with compatible sort options: a `GROUP BY` b-tree honors any per-column (even
  mixed) `ASC`/`DESC`, a `DISTINCT` b-tree is ascending-only, and each term's `NULLS`
  placement must be the default for its direction (`ASC` ⇒ FIRST, `DESC` ⇒ LAST). A term
  that escapes the key (`ORDER BY count(*)`, `ORDER BY 3`), a reordered key list
  (`ORDER BY 2, 1`), or a non-default `NULLS` keeps both nodes. Previously graphite
  *declined the grouping node itself* whenever an `ORDER BY` term was not a plain column,
  emitting a lone wrong sorter; now the grouping node always stands. Byte-exact vs sqlite3
  3.50.4 (`tests/eqp_group_order_fold.rs`). (Out of scope: expression grouping keys
  (`GROUP BY a+0`), and an `ORDER BY` ordinal over an *index-ordered* scan, where the
  access path — not this b-tree — provides the order.)
- **EQP `ORDER BY` ordinal/alias over an index-ordered scan** — the previous fold's
  out-of-scope sibling: an `ORDER BY` term written as a 1-based ordinal (`ORDER BY 1`)
  or a bare output alias (`SELECT b AS x … ORDER BY x`) names the same column as if
  written directly, so when an index-ordered access path (a full covering scan or a
  `WHERE` seek) already yields that column in order, SQLite plans no sorter. graphite's
  order-detection paths (`rowid_ordered_scan`, `order_index_scan`, `scan_order_prefix`,
  `in_seek_order`, `seek_order_prefix`) only matched a directly-written `ORDER BY col`,
  so the ordinal/alias forms spuriously emitted `USE TEMP B-TREE FOR ORDER BY` — and the
  alias form even *missed* the covering index (`SCAN t` instead of `… USING COVERING
  INDEX ib`). A shared `order_key_expr(sel, expr)` resolver now maps an ordinal/alias
  term to its underlying result-column expression (SQLite resolves output names first)
  before each path's column match runs, and the same resolution feeds the covering
  checks (`index_covers_query`, `query_cols_covered`) so the index is recognised as
  covering. The sorter elision, the partial mixed-direction "LAST n TERMS" label, and the
  rowid/IPK ordinal all follow. Byte-exact vs sqlite3 3.50.4, plan and rows
  (`tests/eqp_order_by_ordinal_scan.rs`). (Out of scope, pre-existing: an alias that
  shadows a table column but projects an *expression* — an executor alias-resolution
  corner.)
- **EQP `ORDER BY` ordinal over `SELECT *`** — the prior fold's out-of-scope sibling: a
  positional ordinal over a `*` / `table.*` wildcard (`SELECT * FROM t ORDER BY 1`) names
  the column SQLite resolves it to against the *expanded* output list before planning, so
  an index on that column serves the sort with no sorter. graphite left a wildcard ordinal
  unresolved (there is no `ResultColumn::Expr` to borrow), so it spuriously emitted `USE
  TEMP B-TREE FOR ORDER BY`. A shared `order_projection(columns, table_cols)` now expands a
  `*` / `table.*` in place into one synthetic unqualified column reference per non-hidden
  table column, and the five single-table order-detection paths plus the two covering
  checks resolve the ordinal through that expansion. The sorter elision, the `DESC` walk, a
  `WHERE`-seek serving the ordinal, a mixed `SELECT a, * … ORDER BY n`, the rowid/IPK
  ordinal, and the all-columns-covered `SELECT * FROM s ORDER BY 1` → `USING COVERING
  INDEX` all follow. Byte-exact vs sqlite3 3.50.4, plan and rows
  (`tests/eqp_order_by_ordinal_wildcard.rs`).
- **EQP `WITHOUT ROWID` PK-ordered scan** — the prior fold's out-of-scope sibling: a
  `WITHOUT ROWID` table is stored as a b-tree clustered by its PRIMARY KEY, so a full
  scan yields rows in PK-clustered storage order (PK columns first, then the rest)
  ascending. When the whole `ORDER BY` is a uniform-direction *contiguous prefix* of that
  storage order, SQLite plans a bare `SCAN w`; graphite declined every `WITHOUT ROWID`
  ordered scan (all its order-detection paths bail on `meta.without_rowid`) and spuriously
  emitted `USE TEMP B-TREE FOR ORDER BY`. A new `without_rowid_ordered_scan`, plugged into
  the single `order_satisfied_by_scan` chokepoint (so the executor elides the sort and the
  EQP elides the temp-b-tree node together), matches the `ORDER BY` against a uniform
  prefix of `storage_order` — the ascending walk needs no sorter, the `DESC` walk only the
  executor's existing materialise-then-reverse. Restricted to an all-ascending PK
  (`meta.pk_all_asc`, detected via a standalone-`DESC`-keyword scan of the CREATE text,
  since the parser drops table-level key directions): graphite stores every `WITHOUT
  ROWID` PK ascending regardless of a declared `DESC`, so eliding for a `DESC` PK would
  diverge from SQLite's DESC-clustered storage — those keep the sorter. The `SELECT *` /
  positional-ordinal projections resolve through the same `order_projection`. Byte-exact
  vs sqlite3 3.50.4, plan and rows (`tests/eqp_without_rowid_order.rs`). (Out of scope,
  pre-existing: a non-prefix or mixed-direction `ORDER BY` — `ORDER BY y`, `ORDER BY x, z`
  — which SQLite serves with a *partial* "LAST TERM" sorter; graphite keeps its full
  sorter there, an unchanged divergence.)
- **EQP `WITHOUT ROWID` PK-seek ordered output** — the seek companion to the scan above.
  The executor tries a `WITHOUT ROWID` PK seek/range *before* any secondary index, so an
  equality on a leading-key prefix (or a range on the leading key) walks the matching rows
  in PK-clustered storage order; after dropping the equality-pinned (constant) columns, a
  uniform-direction `ORDER BY` prefix of the remaining walk needs no sorter — SQLite plans
  the bare `SEARCH w USING PRIMARY KEY …`, graphite spuriously kept the temp b-tree. A new
  `without_rowid_seek_order`, plugged into the same `order_satisfied_by_scan` chokepoint,
  confirms a leading-PK equality/range guards the seek (declining a non-leading or
  secondary-index seek, and an `IN` on the leading key), folds the pinned columns out, and
  matches the rest of the PK walk plus trailing payload as a uniform prefix. Same
  all-ascending-PK restriction (`meta.pk_all_asc`) as the scan slice — a `DESC` PK keeps
  its sorter. Byte-exact vs sqlite3 3.50.4, plan and rows
  (`tests/eqp_without_rowid_seek_order.rs`). (Out of scope, pre-existing: a full scan whose
  non-seek `WHERE` filters a non-PK column yet still walks in PK order — SQLite elides the
  sorter, graphite keeps it — and the partial "LAST TERM" sorter divergence above.)
- **EQP `WITHOUT ROWID` filtered full-scan ordered output** — the case the seek slice
  deferred. When a `WITHOUT ROWID` table's `WHERE` constrains *only* non-seekable columns
  the executor still full-scans the PK-clustered b-tree, so the surviving rows keep PK
  storage order; SQLite plans a bare `SCAN w` and elides the sorter for a uniform
  (constant-column-dropped) `ORDER BY` prefix, while graphite kept the temp b-tree. A new
  `without_rowid_scan_filtered_order`, on the same `order_satisfied_by_scan` chokepoint,
  stands down when the leading PK column *or* any secondary index's leading column carries
  an equality/`IN`/range constraint (either would steer the executor onto a seek whose walk
  is not the PK order — and `order_index_scan` never picks a secondary index for *ordering*
  on a `WITHOUT ROWID` table, so an unconstrained index is never walked); otherwise it drops
  the equality-pinned columns and matches the rest as a contiguous uniform prefix of the
  storage order. An *internal* pinned-column skip (`WHERE y=2 ORDER BY x, z`, where the
  later term is functionally determined) is declined so SQLite's *partial* "LAST TERM"
  sorter stays the unchanged pre-existing divergence rather than becoming a new one (graphite
  cannot emit a partial sorter). Verified against sqlite3 3.50.4: across a 39-query
  `WITHOUT ROWID` sweep the change cut EQP divergences 31→16 (the residue is all
  pre-existing: index-vs-scan access choices, secondary-index ordering, partial sorters) and
  introduced none — the detector can only ever *remove* a graphite sorter. Byte-exact, plan
  and rows (`tests/eqp_without_rowid_scan_filtered_order.rs`).
- **EQP positional-term range check** — an out-of-range positional `GROUP BY` / `ORDER BY`
  ordinal (`SELECT a FROM t ORDER BY 2`, one output column) is a prepare-time error in
  SQLite, reported identically whether the statement is executed or `EXPLAIN QUERY
  PLAN`'d. graphite's executed path runs `check_positional_terms` in `run_core`, but the
  plan path (`eqp_select`) skipped it and silently built a tree for the invalid query.
  `eqp_select` now runs the same check for a wildcard-free projection — where the output-
  column count is exactly the projected-column count — so the plan path errors byte-exact
  with the executed path and with sqlite (`Nth <clause> term out of range - should be
  between 1 and M`); ORDER BY is resolved before GROUP BY, so its term wins when both are
  out of range. Verified vs sqlite3 3.50.4 (`tests/eqp_positional_out_of_range.rs`). (A
  `SELECT *` projection still defers the count to the scan; unchanged.)
- **Comma-join unqualified equi-predicate promotion** — `FROM t, u WHERE a = x`
  (the implicit-join spelling with a bare-column equality) now seeks the inner table
  by index just like the explicit `FROM t JOIN u ON a = x`, which already did.
  `promote_comma_join_ons` rewrites a comma join to an `ON` so the join fold can
  seek/hash it; it previously matched only a *qualified* `t.a = u.x` (via the column
  qualifier), so an unqualified equality stayed a full nested-loop `SCAN` of the
  inner table — diverging from SQLite's `SEARCH … USING INDEX`. The promotion now
  resolves each bare column to its owning source by name (`comma_join_table_columns`
  → `resolve_col_table`): the unique table holding a column of that name, declining
  on an ambiguous or unknown name so SQLite's own "ambiguous column" error still
  fires. Shared by `run_core` and `eqp_select`, so the executed access path and the
  plan move together; results are unchanged (the equality stays in `WHERE`). Byte-
  exact vs sqlite3 3.50.4 (plan and rows) for the two-table cases whose written
  `FROM` order already matches SQLite's chosen order — secondary-index and INTEGER
  PRIMARY KEY rowid seeks, reversed and mixed qualified/unqualified spellings,
  aliased tables (`tests/comma_join_unqualified_seek.rs`). Out of scope (separate
  optimizer capabilities, not this promotion, rows already correct): SQLite's cost-
  based *join reordering* for 3+ tables, a *range* (`a > x`) join predicate, and
  covering-index detection on a join's inner seek (a pre-existing divergence the
  explicit-join form shares).
- **ATTACH / multi-schema** — `ATTACH`/`DETACH`, schema-qualified read/write/DROP,
  TEMP tables, cross-database joins / views / transactions (see Track E).
- **Error parity** — prepare-time column / aggregate / window / row-value
  resolution and misuse checks; DDL/DML/JSON/PRAGMA/`printf` message wording;
  lexer/parser framing (`near "TOKEN"`, `incomplete input`,
  `unrecognized token: "X"`); the double-quote→string-literal hint; and
  constraint-failure column naming. A `FROM`-less wildcard projection is rejected
  at prepare time with SQLite's highest precedence — a bare `*` is `no tables
  specified`, a qualified `X.*` is `no such table: X`, ahead of a missing `LIMIT`
  column / wrong-arity aggregate / compound column-count mismatch — recursively
  over compound arms, derived-table and expression-position subqueries, while an
  unreferenced CTE body stays lazily accepted (`tests/fromless_wildcard.rs`).

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

  *Empirically-mapped precedence (2026-06-30, vs sqlite3 3.50.4 — supersedes the
  simple clause model above):* the deciding factor is SQLite's actual name-
  resolution **clause order**, which is `result-set → HAVING → WHERE → GROUP BY /
  ORDER BY` — note **HAVING resolves before WHERE** (`WHERE zzz=1 … HAVING yyy=1`
  → `no such column: yyy`, not `zzz`; `WHERE nope=1 … HAVING (a,a)=(1,2,3)` →
  `row value misused`; but `WHERE (a,a)=(1,2,3) … ORDER BY nope` / `GROUP BY nope`
  → `row value misused`, so WHERE outranks GROUP BY/ORDER BY). Within one clause it
  is a single pre-order walk in which a structural fault (checked at the node,
  before its children — so `(nope,a) IN ((1,2,3))` flags arity before descending to
  `nope`) and a `no such column` at a leaf are detected together, first-fault-wins
  (`nope=1 AND (a,a)=(1,2,3)` → column; `(a,a)=(1,2,3) AND nope=1` → structural).
  The scalar-LHS `IN (row)` case (`nope IN ((1,2))`) is *not* a node-level
  structural fault, so the column wins. Implementing this means one combined
  ordered walk reusing the existing node predicates from both `validate_columns_exist`
  and `reject_row_value_misuse`; deferred as fragile-for-cosmetic-gain (only
  reorders two already-correct messages on doubly-malformed input).

- **A-misc-2 — `*` / `t.*` over a same-named self-join — DONE.** Both facets now
  match SQLite. (1) The database-qualified expansion error names the source
  origin (`main.t.a`, `temp.t.a`, attached `aux.t.a`, or `*.x.a` for a
  derived/CTE source). (2) The dual false-positive is fixed: `SELECT * FROM t,
  aux.t` (same table name, *different* databases) no longer mis-reports
  `ambiguous column name: main.t.a` — it keeps all columns, because the
  ambiguity test now compares the full `(database, table, name)` origin.
  `eval::ColumnInfo` carries a `schema: Option<String>` stamped at base-table
  `FROM`-resolution (`main`/`temp`/attached name), which rides through
  NATURAL/USING coalescing. Differentially verified — `tests/ambiguous_wildcard_origin.rs`.

- **A-tvf-bare — eponymous TVFs used as a bare table name (no parens).**
  `FROM generate_series`, `FROM json_each`, `FROM pragma_table_info` (without a
  parenthesised argument list) are real eponymous virtual tables in SQLite: their
  hidden arguments can be supplied through the `WHERE` clause
  (`FROM generate_series WHERE start=1 AND stop=3`), and an unconstrained
  reference yields either no rows (`json_each`, the pragma TVFs) or a
  function-specific "first argument … missing or unusable" error
  (`generate_series`).
  - **A-tvf-bare-pragma — bare `pragma_*` TVFs driven by `WHERE arg=…`. DONE.**
    A bare `FROM pragma_table_info WHERE arg='t'` (and `pragma_index_list` /
    `index_info` / `index_xinfo` / `foreign_key_list` / `table_xinfo` / … —
    every argument-taking pragma TVF) now binds its pragma argument from a
    top-level equality constraint on the hidden `arg` column, with an optional
    `schema=…` constraint. Every pragma TVF — bare *or* called — now also exposes
    the hidden `arg`/`schema` input columns SQLite does: they echo the
    call/constraint values, are selectable and filterable, and are omitted from
    `*` expansion. Implementation: `push_pragma_tvf_args` rewrites a bare
    pragma source into the call form by lifting `arg`/`schema` literal/parameter
    equalities out of the `WHERE` (descending `AND`/parens) into synthetic
    positional `tvf_args`; the pragma branch of `tvf_rows` appends the two hidden
    columns echoing those values. run_core still re-applies the full `WHERE`
    (the echoed columns satisfy it — a superset, never wrong); a bare reference
    with no `arg=` constraint or only a non-equality (`arg LIKE …`) yields no
    rows, matching the argument-less `PRAGMA`. Differential test in
    `tests/pragma_tvf_bare.rs`. See [[pragma-fidelity]].
  - **A-tvf-bare-json — bare `json_each` / `json_tree` driven by `WHERE json=…`. DONE.**
    A bare `FROM json_each WHERE json='[10,20,30]'` (and `json_tree`, with an
    optional `root='$.a'` constraint) now binds the document and path from
    top-level equality constraints on the hidden `json`/`root` columns — the same
    mechanism as the pragma case, generalized. `is_pragma_tvf`→`is_bare_tvf`
    (recognizes `json_each`/`json_tree` as well as `pragma_*`); `push_bare_tvf_args`
    name-dispatches the hidden-column list (`["json","root"]` vs `["arg","schema"]`)
    and lifts the equalities into synthetic positional `tvf_args`. The existing
    `tvf_rows` json path already echoes the hidden `json`/`root` columns and excludes
    them from `*`. Bare with no `json=` / only a non-equality (`json LIKE …`) yields
    no rows, like an argument-less reference. Because these forms are always
    *bounded* (unlike `generate_series`), the materialise path handles them.
    Differential test in `tests/json_each_hidden_columns.rs`. See [[pragma-fidelity]].
  - **A-tvf-bare-series — bare `generate_series` (remaining).**
    Still `no such table: <name>` for the parenthesis-free form. `generate_series`
    is the hard one: its default `stop` is `0xffffffff`, so a bare
    `WHERE start=1` (no `stop=`) is an effectively unbounded series that SQLite
    streams lazily — graphite's tree-walker *materialises* every TVF source, so it
    cannot represent the unbounded case without hanging. This belongs on the VDBE
    cursor track (lazy row production), not the materialise path; deferred there.

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

- **A-prepare-correlated — prepare-time validation in correlated subquery bodies. DONE.**
  The eager (prepare-time, row-independent) validators cover the common scopes —
  `validate_columns_exist` (top-level plain-table / `ON`-join),
  `validate_derived_columns` (single derived-table `FROM`),
  `validate_join_derived_columns` (a derived source joined or `NATURAL`/`USING`
  coalesced), and `reject_unresolved_functions_in_subqueries` (unknown / wrong-
  arity scalar functions inside expression-position subqueries, gated on
  `subquery_body_columns_clean`). The residual — a column reference inside a
  **correlated subquery body** that binds to neither the body's own `FROM` nor any
  enclosing scope — is now closed by `validate_subquery_body_columns` /
  `check_subquery_body_columns` (mod.rs), run once at the outermost query right
  after `validate_join_derived_columns`. It collects every expression-position
  subquery (scalar, `IN (SELECT)`, `EXISTS`), resolves each body's shallow column
  refs against the body's own `FROM` plus the accumulated correlation scope via the
  schema-aware `column_resolves_scoped` (a three-part `schema.table.column` must
  also match the candidate's database of origin — the `ColumnInfo::schema` added
  in A-misc-2), and raises the first `no such column: <as-written>` SQLite would,
  recursing into deeper bodies with each level's `FROM` added to the scope.
  Conservative throughout (compound bodies, un-buildable `FROM`s, and unknown-origin
  candidates fall back to the lazy path) so no valid SQL is rejected. So
  `SELECT (SELECT bad.t.a) FROM t` now errors `no such column: bad.t.a` over an
  empty table, matching SQLite. Differential test in
  `tests/subquery_body_columns.rs`.
  *(Orthogonal: the tree-walker still cannot* execute *a bare/qualified `rowid`
  over any join — a per-table-rowid-in-join-rows gap, not a validation gap.)*

- **A-cli-pragma-eq — `=arg` form of argument-taking query PRAGMAs in the shell. DONE.**
  The `graphitesql` shell routed any `PRAGMA … = …` to `execute` (a setter that
  discards rows), so the `=arg` form of the *row-returning* argument pragmas
  (`PRAGMA table_info=foo`, same query as `PRAGMA table_info(foo)`) printed
  nothing. The library/parser were already correct — both forms parse to the same
  AST — so the fix is purely in `is_pragma_setter` (`src/bin/graphitesql.rs`): it
  now extracts the pragma name and excludes the eight argument-taking query
  pragmas (`table_info`, `table_xinfo`, `table_list`, `index_list`, `index_info`,
  `index_xinfo`, `foreign_key_list`, `foreign_key_check`) so their `=arg` form
  routes to `query` and prints rows, byte-for-byte with sqlite, while real setters
  (`user_version=42`, `journal_mode=wal`, …) still route to `execute`.
  Differential test in `tests/cli_pragma_setter.rs`.

- **A-pragma-noarg — argumentless / numeric-argument introspection PRAGMAs. DONE.**
  A bare argument-taking query pragma (`PRAGMA table_info`, no `(name)`/`=name`)
  named no object, so graphite raised `PRAGMA … requires a … name` where SQLite
  returns an *empty* result. Likewise a numeric argument: SQLite coerces it to
  text (`PRAGMA index_info(1)` looks up an object literally named `1`, finds none,
  returns empty; `PRAGMA foreign_key_check(1)` → `no such table: 1`). Fixed in
  `run_pragma` (`src/exec/mod.rs`): `pragma_arg_name` now returns
  `Option<String>` (with integer→text coercion), and the row-returning getters
  (`table_info`/`table_xinfo`, `index_list`, `index_info`/`index_xinfo`,
  `foreign_key_list`) treat an absent/non-name argument as a name that matches
  nothing → empty result. `foreign_key_check` keeps its `None` = check-all-tables
  behaviour. Test: `tests/pragma_introspection.rs`.

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
  `EXPLAIN` (**B8**). A `FROM`-less `SELECT … WHERE <pred>` runs here too: the
  predicate is gated before the projection (an `IfFalse` over the rowless row), so
  a false/NULL predicate emits zero rows and never evaluates the SELECT list. A
  constant `LIMIT`/`OFFSET` folds at compile time (the single row is suppressed when
  `LIMIT` is 0 or a positive `OFFSET` skips it; a negative `LIMIT`/`OFFSET` matches
  SQLite's unlimited/no-skip), and `DISTINCT` is a no-op over one row. An `ORDER BY`
  whose terms resolve is likewise a no-op (one row can't reorder) and runs here —
  the key is compiled to force type/column resolution, then discarded; a positional
  ordinal (`ORDER BY 2`, needs range-checking) or an output-alias reference still
  defers to the tree-walker. An uncorrelated `FROM`-less scalar subquery in
  expression position (`(SELECT <e> [WHERE <p>])`) compiles inline: the result
  register defaults to NULL, an optional `WHERE` gates it (`IfFalse` over the
  rowless row), and the projected value overwrites the NULL when the row
  qualifies — so a filtered-out subquery yields NULL. A scalar call buried in such
  a subquery is arity-checked at prepare time even when the outer row never
  executes (the VDBE-success path now runs the subquery function validator too);
  an aggregate / multi-column / `ORDER BY`/`LIMIT`/`OFFSET` or `FROM`-bearing
  subquery body still defers. A `[NOT] EXISTS (SELECT … [WHERE <p>])` over a
  FROM-less body folds the same way: with no predicate (or a true one) the rowless
  row survives so `EXISTS` is 1, a false predicate makes it 0 (inverted for `NOT
  EXISTS`). `EXISTS` ignores the projection — a multi-column inner is fine — but
  each term is still compiled to force resolution, so an unresolved column or a
  `SELECT *` (no tables) defers; an aggregate projection (a row exists even over a
  false-`WHERE` empty input) defers too.
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
- **`PRAGMA table_list` row order.** SQLite emits rows in its internal schema
  hash-table iteration order (e.g. a two-table db created `foo, bar` lists
  `bar, foo`; a view created last can sort ahead of earlier tables), which is
  neither creation nor alphabetical order and depends on SQLite's exact string
  hash and rehash thresholds. Reproducing it byte-for-byte would mean
  reimplementing that hash — high-effort, fragile, and cosmetic — so graphite
  emits a stable per-schema order (its own object order) instead. The row *set*
  and every column value match; only inter-row ordering differs. Same
  non-reproducible category as `collation_list` ordering above.

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
