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
            │  exec         iterator executor + VDBE          │  execution
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
| `exec`             | name resolution, execution, DDL/DML, triggers, functions, VDBE | `select.c`, `where.c`, `insert.c`, `vdbe.c` |
| `planner` *(in `exec`)* | index selection, join order (partly cost-based) | `where.c`, `analyze.c` |
| `func` / `collate` | scalar/aggregate funcs, collations | `func.c`, `date.c`, `callback.c` |
| `schema`           | parse `sqlite_schema`, build the catalog | `build.c`, `prepare.c` |
| `api`              | `Connection`/`Statement` | `main.c`, `vdbeapi.c` |

**Executor vs. bytecode.** The engine grew as an *operational, iterator-style*
executor and now also has a **register VDBE** that most read queries route to
by default (the tree-walker is the parity oracle and the fallback for shapes the
VDBE hasn't taken over). Track B is finishing that migration — live storage
cursors, correlated subqueries, and windows on the VDBE — plus the cost-model
work that a bytecode planner unlocks.

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
verified against the pinned `sqlite3` 3.50.4 oracle (a 1,600+ query corpus plus
260+ focused test suites). Detail lives in `CHANGELOG.md` and git history; in
summary, graphitesql today:

**Reads & writes real SQLite files.** Opens `sqlite3`-written databases
(including WAL-mode) and **creates** databases whose files `sqlite3` opens with
`PRAGMA integrity_check = ok`. Storage covers rowid and **`WITHOUT ROWID`**
tables, automatic/secondary/`UNIQUE` indexes (incl. `sqlite_autoindex_*`),
overflow pages, the freelist with **page merging on delete**, real **`VACUUM`**
(+ `VACUUM … INTO`), the full **`auto_vacuum`** track (read, write, FULL
auto-truncate, INCREMENTAL reclaim), the **SQLite-format rollback journal** with
hot-journal recovery, and the **WAL read *and* write** path (`journal_mode=WAL`,
`wal_checkpoint`). A whole-number real in a `REAL` column is stored with the
compact integer serial type (`MEM_IntReal`), byte-matching sqlite.

**Runs a broad SQL dialect** — all differentially byte-exact vs the oracle:

- **Queries** — `SELECT` with `WHERE`/`GROUP BY`/`HAVING`/`ORDER BY`
  (`NULLS FIRST/LAST`, `COLLATE`, positional)/`LIMIT`/`OFFSET`/`DISTINCT` and
  SELECT-list aliases; every join kind (`INNER`/`LEFT`/`RIGHT`/`FULL`/cross/comma,
  `NATURAL`/`USING` column coalescing, affinity- & collation-aware keys),
  nested-loop + hash join; compound queries
  (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`, collation-aware, dedup-ordered
  tie-breaking); (recursive/mutual) **CTEs** with `[NOT] MATERIALIZED` and the
  recursive term's `ORDER BY`/`LIMIT` driving the work-queue order; correlated /
  `[NOT] EXISTS` / `IN (SELECT)` / scalar subqueries; derived tables & views as
  sources (inheriting base affinity/collation); **window functions**
  (`ROWS`/`RANGE`/`GROUPS`, `EXCLUDE`, value-offsets, `FILTER`, named windows,
  over `GROUP BY`/aggregates, NULL-correct RANGE frames); row-value comparisons
  and multi-column `IN`.
- **DML** — INSERT (multi-row, `DEFAULT VALUES`, `INSERT … SELECT` snapshot
  semantics), UPSERT (`DO UPDATE/NOTHING`, `excluded.*`, targeted/partial-index,
  incl. on `WITHOUT ROWID`), `RETURNING` (INSERT/UPDATE/DELETE, rowid &
  `WITHOUT ROWID`), UPDATE (simultaneous SET, `UPDATE … FROM`, row-value SET),
  DELETE, all `OR <conflict>` clauses, `AS`-aliased targets, and compensated
  (Kahan) `sum`/`avg`/`total`.
- **DDL** — CREATE/DROP/ALTER TABLE (ADD/DROP/RENAME COLUMN, RENAME TABLE, with
  cross-object propagation into views/FKs/triggers), CREATE/DROP
  VIEW/INDEX/TRIGGER (BEFORE/AFTER/INSTEAD OF, `UPDATE OF`, `WHEN`, `RAISE`,
  recursive, `NEW`/`OLD`), STRICT & WITHOUT ROWID, generated columns
  (VIRTUAL/STORED), AUTOINCREMENT + `sqlite_sequence`, partial/expression/
  collation/DESC indexes, constraint-level `ON CONFLICT`, foreign keys
  (CASCADE/SET NULL/SET DEFAULT/RESTRICT, composite, self-referential,
  DEFERRABLE), and full CREATE-time + ALTER validation with byte-exact error
  parity.
- **Functions & values** — the full scalar/aggregate/date-time
  (`strftime` incl. `subsec`/`%J`)/`printf`+`format` (incl. the `!` alt-form-2
  high-precision flag via the ported `sqlite3FpDecode`)/JSON + JSONB libraries;
  type affinity; collation (BINARY/NOCASE/RTRIM) propagated through
  IN/BETWEEN/CASE/min-max/compound/ordering; `random()`/`randomblob()`;
  blob↔text↔number coercion; verbatim column-name source spans; JSON paths with
  quoted-key backslash escapes.
- **Constraints, triggers, transactions** — `NOT NULL`/`CHECK`/`UNIQUE`/`PK`,
  partial/expression UNIQUE indexes, FK enforcement; triggers incl. reentrant
  same-table edits; the `SAVEPOINT` family; `ATTACH`/`DETACH`/`TEMP` multi-schema.
- **Schema catalog & introspection** — `sqlite_schema`/`sqlite_master` readable
  with sqlite-canonicalised `sql` text; the introspection PRAGMAs and the
  `pragma_*` table-valued-function surface (incl. bare `WHERE arg=…`-driven
  forms); `EXPLAIN QUERY PLAN` shaped byte-exactly across the derived/CTE/view
  flatten & CO-ROUTINE taxonomy, scalar/`IN`-subquery nodes, the seek family, and
  the trailing temp-b-tree / ORDER-BY-elision cases (the whole 2026-06/07 B9
  cluster).
- **Virtual tables & extensions** — a writable/persistent vtab layer with the
  built-in **R-Tree**, **FTS5** (read + write, sqlite-readable on disk, full query
  language + `bm25`/`highlight`/config), **geopoly** (scalar library + R-Tree-
  backed vtab), `dbstat`, read-only `sqlite_dbpage`, and Rust scalar/aggregate
  **UDFs**.

The shell (`graphitesql`) supports `.tables`/`.indexes`/`.schema`/`.databases`/
`.dump`/`.read`/`.headers`, byte-compatible where implemented.

What remains is breadth and depth toward full SQLite parity, below.

---

## 4. Forward plan — remaining work

Five tracks. Each keeps only the **open pieces**, in small independently-shippable
chunks; every chunk lands with a differential test and keeps `master` green.
Tracks can progress in parallel. (Completed items — the vast majority of the SQL
surface — are in git history and `CHANGELOG.md`.)

### Track A — SQL language & functions  *(substantially complete)*

RENAME COLUMN dependency propagation is now comprehensive — mixed-scope
views+triggers (per-occurrence `Expr::Column` spans), compound (UNION/INTERSECT/
EXCEPT) incl. `ORDER BY`, and `WITH` CTE bodies incl. output-column provenance
(details in git history / `CHANGELOG.md`). What remains:

- **A-alter-1 — derived-table propagation for RENAME COLUMN. DONE 2026-07-10.** A
  derived-table subquery consumed in a compound arm (`SELECT a FROM t UNION SELECT a
  FROM (SELECT a FROM u)`) now rewrites the base arm and leaves the derived arm,
  byte-exact vs sqlite; a derived table that exposes the renamed column unaliased
  and is consumed (`SELECT a FROM (SELECT a FROM t)`) bails unchanged (sqlite
  rejects). Applied the CTE-provenance pattern to `FROM (subquery)`: a
  `body_exposes_old` helper (factored out of `cte_old_owner`), derived sources
  recursed + skipped as base sources in `collect_select_base_sources_ctx`, keyed by
  alias (synthetic when unaliased) with their provenance in a *select-local*
  visibility list (derived tables don't propagate to sibling arms/subqueries, unlike
  CTEs — a subtle bug found and fixed during the sweep). Verified across 48 shapes.
  With this, RENAME COLUMN propagation is complete for every shape sqlite rewrites.
- **A-alter-2 — ALTER-time rejection of a RENAME that breaks a dependent view.
  DONE 2026-07-10 (views; triggers = A-alter-2b).** sqlite rejects + rolls back a
  rename that leaves a dependent view unresolvable (`USING(col)` column vanishes, a
  derived/CTE that exposes the renamed column and is then consumed); graphite now
  matches. Took the clean path (a): first **closed the last propagation gaps** with
  the `BareRewrite::At` span machinery so the blanket probe can't false-reject — a
  result-column alias equal to `old` (`SELECT b AS a, a FROM t`) and a source table
  named `old` (`SELECT t.a FROM t, a`) now rewrite the bound occurrences span-precisely
  while leaving the alias/table token (`scope_bare_old_decision` always returns
  `At(spans)`, never a blanket `All`; `select_needs_scope_aware` forces the scope pass
  when a result-alias or source name equals `old`). Then re-applied the machinery: a
  `\0graphite_alter` writer savepoint around the rewrite, `Schema::read` the post-rename
  overlay, `first_broken_view_after_rename` probes each dependent view
  (`SELECT * FROM "v" LIMIT 0`) and, on a resolution error, `rollback_to_savepoint` +
  the byte-exact `error in view NAME after rename: <detail>`. A sweep of 12+ shapes
  (aliases, GROUP BY/HAVING/DISTINCT/ORDER BY, self-alias, comma/NATURAL joins) found
  no shape where graphite leaves a view broken that sqlite rewrites. Tests:
  `tests/alter_rename_rollback.rs` (reject-and-rollback + accept), and the reject cases
  folded into the two rename-propagation suites.
- **A-alter-2b — RENAME-that-breaks-a-dependent-TRIGGER rejection. PARTIALLY DONE
  2026-07-10 (INSERT…SELECT + body-SELECT subset).** The trigger analogue of
  A-alter-2. graphite can't query a trigger, so `first_broken_trigger_after_rename`
  resolves the trigger's *real* body `SELECT` ASTs — the source of an `INSERT …
  SELECT` and a body `SELECT` step — against the post-rename schema (via `run_select`),
  with `NEW`/`OLD`/`RAISE` neutralised (`neutralize_new_old_select`, replacing
  `NEW.x`/`OLD.x`/`RAISE(…)` with `NULL`), and rejects with the byte-exact
  `error in trigger NAME after rename: <detail>` when a probe fails with a genuine
  renamed-column resolution error (`trigger_break_detail`: the message is `no such
  column` / `cannot join using column` *and* names the old column as a whole
  identifier). **Key design decision — probe the real AST, never reconstruct.**
  Reconstructing a probe `SELECT` from `UPDATE`/`DELETE`/`VALUES`/`WHEN` fields
  resolves in a different order than sqlite (whose *partial* rewrite dangles a
  different reference than graphite's all-or-nothing), producing a *wrong* rejection
  message; and a `LIMIT 0` on any probe makes graphite skip the scan so a
  `WHERE`/projection subquery never resolves (a missed break). So only the real
  `INSERT…SELECT`/body-`SELECT` ASTs are probed (no `LIMIT`), which graphite resolves
  identically to sqlite → byte-exact. **Residual (documented, sound):** a break
  reachable *only* through an `UPDATE`/`DELETE`/`VALUES`/`WHEN` expression subquery is
  still accepted (as graphite accepted *every* trigger break before this) — a
  false-accept, never a false-reject; same class the DROP COLUMN dependency check
  leaves. Closing it needs either graphite's propagation to become partial like
  sqlite's, or a scope-aware per-subquery probe. Verified 16-shape sweep (6 reject / 10
  accept, incl. correlated-over-target and NEW/OLD/RAISE). Test:
  `tests/alter_rename_trigger_rollback.rs`.
- **A-misc-1 — structural row-arity error *ordering* vs name resolution.** *(niche;
  cosmetic)* On doubly-malformed input (a row-value misuse *and* a missing column
  in one clause) graphite reports the column error where sqlite sometimes reports
  the structural one — message bodies identical, only first-fault order differs.
  Fix = interleave the arity check with column resolution clause-by-clause
  (`result-set → HAVING → WHERE → GROUP BY/ORDER BY`, first-fault-wins). Fragile for
  cosmetic gain; low priority.
- **A-tvf-bare-series — bare `generate_series` (no parens). DONE 2026-07-11.** A
  bare `generate_series` now takes its hidden `start`/`stop`/`step` input columns
  from top-level `WHERE` equalities, exactly like the bare `pragma_*` / `json_each`
  eponymous forms (`is_bare_tvf` + `push_bare_tvf_args` extended with the
  `["start","stop","step"]` column set). `generate_series` gained the three echoed
  hidden columns (constant per row, excluded from `*`), and `is_const_arg` now
  accepts a signed/parenthesized constant so `WHERE step=-2` drives the pushdown.
  Fixed a pre-existing divergence found while probing: a one-argument
  `generate_series(N)` defaulted `stop` to `N` (one row) instead of SQLite's
  `0xFFFFFFFF`; the no-argument error now matches SQLite's text too. The unbounded
  default is not a problem in practice — the tree-walker still materialises, but a
  bare form is always driven by a `WHERE stop=…`. Verified differentially
  (`tests/table_valued.rs::bare_generate_series_driven_from_where`).

### Track B — Query planner, statistics & the VDBE

Every item is gated on VDBE-vs-tree-walker parity (returns the tree-walker's
result or declines to it — never a wrong answer), so this track is
**perf/coverage/EQP-fidelity only**, never a correctness risk.

**Move the last shapes onto the VDBE:**

- **B-limit-fold — constant-expression `LIMIT`/`OFFSET` on the VDBE. DONE
  2026-07-11.** `fold_const_int` now folds a `LIMIT`/`OFFSET` built from
  deterministic, stateless scalar functions (`abs`/`round`/`length`/`coalesce`/…,
  combined with arithmetic), not just an integer literal — so e.g.
  `LIMIT abs(-3)` and `LIMIT (2*2)+coalesce(NULL,1)` run on the VDBE instead of
  bailing. The allowlist deliberately excludes clock (`datetime`/`strftime`),
  random, and connection-state functions, which are folded at *run* time by the
  tree-walker (folding them at *compile* time would diverge); those, and any
  column/subquery/aggregate/window/filtered argument, bail — so the result is
  always identical to the tree-walker (`tests/vdbe_limit_fold.rs`).
- **B-vdbe-swap — two-table rowid-inner swap on the VDBE. DONE 2026-07-11.** A
  two-table inner join the cost model reorders to drive from the *second* table
  (seeking `from.first` by its cheaper rowid) previously *deferred* to the
  tree-walker; it now runs on the VDBE. `compile_join2` gained a `loop_order`
  permutation (empty = identity) that nests the driver cursor outermost (`[1, 0]`),
  which — because a rowid join matches ≤1 inner row — reproduces the tree-walker's
  driven, unordered emission order exactly (verified for multi-driver-row, DISTINCT,
  WHERE, LIMIT, and the comma form). The swap is applied only to the plain-projection
  path and only when the driver is scanned in rowid/declaration order; a driver
  walked via a *reordering covering index* (`SCAN v USING COVERING INDEX iv`, which
  the materialized rowset scan can't reproduce) and aggregate/GROUP BY joins still
  defer. The **single-column-UNIQUE index-inner swap** runs on the VDBE too (same
  `[1, 0]` permutation — a unique index also matches ≤1 inner row); a *composite* or
  *non-unique* index-inner swap can match several inner rows in index-key order and
  still defers. A **bare order-independent aggregate** (`count`/`sum`/`total`/`avg`/
  `min`/`max`, no GROUP BY) is invariant to the join drive order, so its swap — *and
  even the N-table (≥3) reorder* — now runs on the VDBE (the identity-order fold is
  correct); an order-sensitive aggregate (`group_concat`/`string_agg`/the JSON
  aggregates, whitelisted conservatively so an unknown/user aggregate defers), a
  GROUP BY, or a plain-projection N-table reorder still defers.
  `tests/vdbe_join_swap.rs`.
- **B5b-2 — seek-driven inner cursor over real storage** *(the largest remaining
  VDBE piece)*. Inner rowid seeks (INNER + LEFT, single & N-table left-deep chain,
  compound-`ON`) already run over a live `TableCursor`. *Single-table live scan
  done (2026-07-09, 52116a2):* a plain rowid base-table `SELECT` routed through the
  VDBE now streams rows from a live `TableCursor` in the interpreter via a
  `Cursor0Source` trait (`rewind`/`advance`/`column`) instead of materializing —
  additive, result-identical to the tree-walker + sqlite, falls back for
  WITHOUT ROWID / subquery / view / join / hinted sources. Remaining: the
  in-*interpreter* `OpenRead`/`SeekRowid` opcodes over B5b-1's multi-cursor
  foundation (move the *seek* into bytecode — an internal refactor, no behavior
  change); and seek by a **secondary index** / `WITHOUT ROWID` PK, which is
  *affinity-blocked* (`index_seek_rowids` compares raw keys with the index
  collation and skips the comparison-affinity that `o.x = t.k` applies — routing
  it risks a silent false-negative that wouldn't fall back; needs the tree-walker's
  affinity machinery threaded in first).
- **B5c-2 — correlated subqueries on the VDBE. DONE 2026-07-11.** A correlated
  scalar/`EXISTS` subquery on a single-table live scan now runs on the VDBE (a
  `SubqueryEval` callback re-evaluates the body per outer row through the
  tree-walker, so the value matches). The **prepare-time validation gap** that
  forced the 2026-07-09 revert is closed: `run_core`'s post-VDBE-success path now
  runs the same subquery-body/arity/row-value checks the tree-walker runs
  (`validate_subquery_body_columns` → `reject_invalid_in_subquery_arity` →
  `reject_invalid_scalar_subquery_arity` → `reject_row_value_misuse`), over the
  outer FROM scope resolved *without materializing rows* (`window_join_source_columns`)
  — so `a > (SELECT 1,2)` and `(SELECT u.a)` over a zero-row/filtered scan error
  exactly as SQLite, not silently accepted. `row_value_misuse` + `subquery_body_columns`
  green again; results byte-identical to the tree-walker (`tests/vdbe_correlated_subquery.rs`).
  *Extended to ALL joins (same day):* a correlated scalar/`EXISTS` subquery now runs on
  the VDBE over any join. An `INNER`/comma join uses the nested-loop path
  (`compile_join2` + `allow_correlated`), assembling the combined multi-cursor row for
  the callback (`combined_join_row`). A `LEFT`/`RIGHT`/`FULL`/`NATURAL`/`USING` join uses
  the *materialized* path (`compile_table_select_opts(allow_correlated)` over the already-
  combined rows), where cursor 0 is the combined row — the interpreter picks the source by
  cursor count (`< 2` ⇒ cursor 0, else the assembled row). The post-success validation adds
  `validate_nested_ambiguity` so an ambiguous outer reference inside a subquery is still
  rejected. Byte-identical to the tree-walker across all join kinds
  (`tests/vdbe_correlated_subquery.rs`).
  *Extended to grouped projections (2026-07-11):* a correlated scalar/`EXISTS` subquery
  in a plain `GROUP BY` projection now runs on the VDBE when it is correlated **only on
  the group key(s)** — its value is then well-defined per group. `compile_group_emit`
  admits it via `group_correlated_output` (a conservative walker: any non-key outer
  reference, three-part reference, or shape it cannot fully account for declines), emitting
  `GroupOut::Sub`/`SubExists`; the `GroupEmit` interpreter builds a synthetic per-group row
  (the group's key values at their source-column positions, all else NULL) and evaluates the
  subquery against it through the same `SubqueryEval` callback. A reference to a non-key
  column, or a `HAVING`/`ORDER BY`/`DISTINCT` grouped shape (the general path), still defers
  to the tree-walker. Byte-identical to sqlite (`tests/vdbe_correlated_subquery.rs`).
  *Extended to materialized single sources (2026-07-11):* the materialized single-source
  path (a derived table / CTE / view / TVF / `WITHOUT ROWID` table — the shapes the
  live-scan path declines) now compiles with `allow_correlated` and, when the program
  carries a subquery, runs it through a `LiveSubqueryEval` over the source's columns — so a
  correlated scalar/`EXISTS` (per row) or a group-key correlated `GROUP BY` projection (per
  group) over a derived/CTE source runs on the VDBE instead of deferring.
  *Correlated `IN (SELECT …)` (2026-07-11):* a correlated `expr [NOT] IN (SELECT …)` (the
  non-correlated bare-column form is pre-folded to an `IN (list)` by the router) now runs on
  the VDBE — `compile_expr` wraps the whole predicate in a FROM-less scalar `SELECT` routed
  through the existing `CorrelatedScalar` op, so the tree-walker applies the exact NULL-aware
  three-valued `IN` semantics against the outer frame (no new op/trait method). This also
  lifts the former fallback for an unfolded compound-arm `IN (SELECT 'x' UNION SELECT a …)`:
  wrapping preserves the candidate column's comparison affinity (the reason the router
  declined to fold it), so it now runs correctly rather than deferring.
  *Extended to grouped joins (2026-07-11):* a group-key-correlated projection subquery over
  a `GROUP BY` **join** now runs on the VDBE too — `group_cols` index the combined column
  space and the synthetic per-group row is built at combined width, so the same guard and
  `GroupEmit` machinery apply; `compile_group_join` threads `allow_correlated` and the caller
  supplies a `LiveSubqueryEval` over the combined columns. Non-key grouped references (whose
  per-group value is unspecified) still defer to the tree-walker.
  *Extended to the general grouped path — HAVING / ORDER BY (2026-07-11):* a group-key-correlated
  scalar/`EXISTS` subquery in `HAVING`, in an `ORDER BY` key, or in a projection on the *general*
  grouped path (the second pass over finalized groups: `HAVING`/`ORDER BY`/`LIMIT`/`DISTINCT`)
  now runs on the VDBE. New `Op::GroupCorrelatedScalar` / `GroupCorrelatedExists` build the
  synthetic per-group row from the current group's key vector (`emit_groups[gcursor]`) placed at
  their source-column positions; the compiler sets `group_emit_keys` before the emit body (gated
  on all-bare-column keys and no single-min/max representative rule), and `compile_expr`'s
  subquery arms emit the group op after the same group-key-only guard. Non-key references bail.
  Byte-identical to sqlite (`grouped_correlated_in_having_and_order_by`).
- **B1c — RIGHT/FULL join inner seeks. DONE 2026-07-11 (all four join kinds now
  seek-drive).** **FULL (two-table, explicit projection):** a `FULL JOIN` equals the
  compound `(a LEFT JOIN b) UNION ALL (rows of b with no matching a, a-null-padded)`
  — verified row-for-row *including the no-`ORDER BY` order* against sqlite.
  `Connection::try_full_join_seek` builds that compound: arm 1 is `a LEFT JOIN b`
  (seeks b via the LEFT seek path), arm 2 scans b with a correlated
  `NOT EXISTS (SELECT 1 FROM a WHERE on)` (B5c-2 seek-drives the left lookup) and
  projects the left columns as NULL (`null_out_a_columns` rewrites left-column refs,
  incl. inside functions like `coalesce(a.x,…)`, to NULL). So neither table is
  materialized. Deferred (→ materialized FULL path) for a wildcard/non-rewritable
  projection, a grouped/windowed/DISTINCT query, or a non-base table — only *adds*
  seek coverage. Byte-identical to sqlite (`tests/vdbe_right_join_seek.rs`).
  **RIGHT (two-table):** a `RIGHT JOIN` is the mirror of
  a `LEFT JOIN` (the *right* table is preserved), so `a RIGHT JOIN b ON …` is
  rewritten to the identity `b LEFT JOIN a ON …` (`Connection::swap_right_join_to_left`),
  which routes through the existing seek path and drives the now-inner left table by
  rowid / unique index instead of materializing it. An explicit projection resolves
  columns by name (no reorder); a bare `SELECT *` rotates the swapped `(right, left)`
  combined columns back to `(left, right)` (the left column count comes from the
  schema, no materialization). Any non-seekable shape falls through to the existing —
  correct — materialized RIGHT path, so this only *adds* seek coverage. Byte-identical
  to sqlite incl. left-side null-padding and `SELECT *` column order
  (`tests/vdbe_right_join_seek.rs`). A `SELECT *` FULL join and any non-rewritable
  shape still take the (correct) materialized path.
- **VDBE aggregate coverage — `json_group_array` / `jsonb_group_array`. DONE 2026-07-11.**
  Added `AggKind::JsonGroupArray { jsonb }`: the fold keeps NULL arguments for this
  kind (SQLite includes them as JSON `null`) and the finalizer serializes the
  collected values via the same `json::value_to_json` the tree-walker's `arg_to_json`
  uses, so an empty group yields `[]` (not NULL) and the array is byte-identical.
  Admitted only when the argument does not statically carry the JSON subtype
  (`func::produces_json` — a `json(x)` / `->` argument defers, since its text must be
  spliced in unquoted). `DISTINCT` dedups via the existing per-group path. Verified vs
  sqlite3 (`tests/vdbe_json_group_array.rs`).
  *`json_group_object` / `jsonb_group_object` (2026-07-11):* the two-argument object
  aggregate now runs on the VDBE too. `AggSpec`/`Op::AggStep` gained a second value
  register (`arg2`) and `AggAcc` a parallel `vals2`, so the fold collects key/value
  pairs (NULLs kept); the finalizer text-coerces each key and serializes via the same
  `Json::Object` path (empty group → `{}`). Gated on the *value* argument not carrying
  the JSON subtype.

**Cost model & EQP fidelity** *(rows already correct — plan/perf only)*:

- **B9h — cost-model single-table index *choice*.** The purely *structural* costs
  are done (no-`WHERE` covering-scan choice; covering-preferred equality/range/
  GROUP-BY/DISTINCT/ORDER-BY seeks via `choose_seek_index`/`choose_range_index`).
  **ORDER-BY sort-avoidance with a non-seekable `WHERE` — DONE 2026-07-11:** a query
  whose `WHERE` is not served by a seek index but whose `ORDER BY` is now walks the
  ORDER-BY index to avoid the temp-b-tree sort (`SCAN t USING INDEX i_b`), matching
  sqlite. `order_index_scan` no longer bails on any `WHERE`; it admits one exactly
  when `eqp_access` shows a plain `SCAN` (no seek) — so when the `WHERE` *does* seek
  an index, that seek (and the sort) is planned instead, as before. The executor
  reaches this path only after every seek fails and `run_core` re-applies the `WHERE`
  to the ordered rows downstream, so no execution change was needed. Verified
  differentially against the sqlite3 CLI (`tests/eqp_sort_avoidance.rs`).
  **Seek-vs-sort with a single open-ended range — DONE 2026-07-11:** when the
  `WHERE` is a *single open-ended* range (`b>?`, `b<?`, `b>=?`, `b<=?`, `b!=?`) on
  one index and the `ORDER BY` is fully served by *another* index, sqlite walks the
  ORDER-BY index to avoid the sort rather than seek the range (the range's ~1/4
  default selectivity does not pay for losing the ordered walk) — whereas an
  equality / bounded range (`… AND …`) / `IN` stays a seek + sort.
  `order_index_scan` now also admits that single-open-range access (recognised
  structurally from the `eqp_access` render), suppressing the override when the
  chosen ORDER-BY index *is* the seek index (there the seek is already ordered —
  B9j seek-order-credit — so the SEARCH stays); `try_index_range` defers to it so
  execution and EQP agree, and the `COVERING` label now folds in the WHERE columns.
  Gated to the no-ANALYZE case (value-specific selectivity is B4). Verified
  differentially (`single_open_range_prefers_order_index_over_seek`).
  Still open: ORDER BY influencing the index *choice* among indexes (the full
  sort-avoidance cost *term*, beyond the no-seek and single-open-range cases); the tiebreak among several non-covering indexes sharing an equality
  prefix (SQLite's full LogEst row-cost, not reducible to narrower/newest); a
  *partial-prefix* covering index for a multi-column ORDER BY (unify
  `order_index_scan`/`covering_scan`); `min`/`max` over a non-leading covered
  column (one-end-seek-vs-full-covering-scan distinction). These are structural; a
  stat4 oracle is only needed for genuinely data-driven choices (B4).
- **B1b — cost-based join reordering.** The structural slices are done (rowid- and
  index-inner swaps, N-table greedy connected-cheapest order, covering-index join
  scans, trailing-node EQP parity). Still open: **selectivity-driven** ordering (a
  single-table WHERE restriction picking the driver) and projection-sensitive
  equal-cost driver ties — the full `whereLoopAddBtree`/`wherePathSolver` formula.
  Divergent EQP where graphite's per-cursor access paths are often cheaper than
  sqlite's cost-reordered scans is *by design* (results correct). *Probed
  2026-07-11 — a concrete sub-case:* when the join **driver** carries its own
  selective WHERE equality (`… JOIN small ON big.v=small.v WHERE big.k=7`, or a
  rowid `WHERE big.id=7`), sqlite renders the driver as a `SEARCH` seeking that
  constraint (`SEARCH big USING INDEX bk (k=?)`) while graphite `SCAN`s it and
  builds an automatic index on the inner. The *rows are identical* (the driver's
  WHERE is re-applied) — this is EQP/perf only. Closing it needs the driver's
  access path to become a seek (an EQP **and** join-executor change, since the
  driver is currently always scanned), which ripples through the whole join EQP
  corpus; deferred as a genuine `whereLoopAddBtree` driver-cost slice, not a quick
  structural one.
- **B9j — collation-aware index *selection* for a non-default-collation index.**
  An index carrying a non-default collation (`CREATE INDEX ib ON t(b COLLATE
  NOCASE)`) is mis-selected (rows still correct via the WHERE re-apply). The model —
  an index serves a term iff its per-column collation equals the term's *effective*
  collation. **ORDER BY slice DONE 2026-07-11:** `order_index_scan` now resolves each
  `ORDER BY` term's effective collation (an explicit `COLLATE`, else the column's
  declared collation) and matches it against the index's stored collation, so
  `ORDER BY b COLLATE NOCASE` walks the NOCASE index while `ORDER BY b` uses the
  BINARY one — byte-identical to sqlite (`tests/eqp_sort_avoidance.rs`). **WHERE
  equality slice DONE 2026-07-11:** `collect_eq_constraints_coll` records each
  equality's effective collation (un-gated), and `choose_seek_index` matches an
  equality to an index only when their collations agree — so `WHERE b = 'x' COLLATE
  NOCASE` seeks the NOCASE index (`ib`) while a plain `= 'x'` uses the BINARY one.
  Threaded through `choose_seek_index` (+ `stat4_equal_est`), `try_index_lookup`'s
  seek-key build (which keeps the gated `eqs` for the rowid fast path), `eqp_access`,
  and `seek_order_prefix` — all in lockstep, full corpus green. **Range slice DONE
  2026-07-11:** a single `> 'x' COLLATE NOCASE` bound now seeks the NOCASE index too
  (`collect_range_constraints_coll` un-gates single `<`/`>` bounds; `range_collation`
  recovers the bound collation; `choose_range_index` matches it to the index's
  leading-column collation). `BETWEEN`/`GLOB` keep the gated per-bound behaviour (a
  mixed-collation `BETWEEN 'a' AND 'd' COLLATE NOCASE` still uses the BINARY index,
  matching sqlite). **Seek ORDER-BY-collation credit DONE 2026-07-11:**
  `seek_order_prefix` peels an explicit `COLLATE` from each `ORDER BY` term and
  compares the index walk against the term's *effective* collation, so a NOCASE
  equality/range seek walking the NOCASE index earns the order credit —
  `b > 'x' COLLATE NOCASE ORDER BY b COLLATE NOCASE` runs with no temp b-tree,
  matching sqlite. **B9j is now complete** (ORDER-BY, WHERE equality, WHERE range,
  seek order-credit).
- **B9b — window-function EQP.** The co-routine *body* is exactly the B9h index
  choice (SQLite picks the index that covers the input **and** serves the
  `PARTITION BY`/window-`ORDER BY`), so this is **blocked on B9h** (plus a
  deterministic model for the multi-window/nested `(subquery-N)` numbering).
- **B4 — `sqlite_stat4` histograms.** *Generation done (2026-07-09).* `ANALYZE`
  emits byte-compatible `sqlite_stat4` (faithful `analyze.c` accumulator port in
  `src/exec/stat4.rs`), verified 0-diff against a `-DSQLITE_ENABLE_STAT4` oracle
  across 300+ fuzzed schemas; a pre-existing `sqlite_stat1` avg-eq divergence was
  fixed in the same pass. *Planner-use — equality done (2026-07-09):* the index
  chooser now consults stat4 samples for `col = ?` selectivity (ports
  `whereKeyStats`/`whereEqualScanEst`/`initAvgEq`), so a rare-value equality flips
  to the selective index — narrowed (only when the index has stat1+stat4 and the
  matched leading prefix is fully bound) so no ANALYZE-less plan moves and the EQP
  corpus stays green. *Equality scan-vs-search done (2391bf4)* and *range
  selectivity done (3317535):* the index-vs-full-`SCAN` `rRun` comparison
  (`full_scan_beats_seek`/`full_scan_beats_range`, LogEst ports of
  `whereLoopAddBtree`/`whereLoopFindLesser`/`wherePathSolver`) now full-scans an
  unselective non-covering equality **or** leading range at sqlite's exact
  boundary, and `whereRangeScanEst`'s STAT4 branch estimates range rows from the
  samples — all gated to stat4-backed non-covering single-candidate cases so the
  EQP corpus stays byte-identical. *Join-order path solver done (7493118):* a
  bounded LogEst `wherePathSolver`/`whereLoopAddBtree` port (`join_scan_cost`/
  `join_seek_cost`/`two_table_second_drives_cheaper`/`ntable_join_order`) now
  picks the join **driver** by cost — `big JOIN small` drives `small`, 3/4-table
  hubs drive the smallest table — matching the oracle, stats-gated so no-ANALYZE
  plans are byte-identical. **Remaining (small):** the tail-table access *label*
  in 3+-table plans (sqlite renders `BLOOM FILTER`/`AUTOMATIC COVERING INDEX` for
  the tail vs graphite's index seek — a separate rendering track, result/order
  unaffected); and choosing among several *candidate indexes on one table* by
  full WhereLoop cost (only the scan-vs-seek leaf + driver order are ported).

### Track C — Storage engine, transactions, concurrency

In-process multi-connection coordination is done: read-only clean-page cache
(C8c), persistent deferred read locks (C9a), the process-local WAL wal-index
(C9c), and the documented thread-confined `Connection` model (C9d). See git
history / `CHANGELOG.md`. Remaining:

- **C9b — OS-level cross-process file locks.** *C9b-0/1/2 DONE 2026-07-10.* The
  `StdVfs` now drives **one process-wide OS advisory lock** (`std::fs::File`'s 1.89
  `lock`/`try_lock`/`unlock`) off its per-path aggregate `LockState` (`CpLock` in
  `src/vfs/std_file.rs`): OS *shared* while only readers are active, OS *exclusive*
  for any write intent. So two OS processes over one file serialize their writes
  (and explicit read txns coordinate) — the C9b-0 MSRV bump, the C9b-1 primitive,
  and the C9b-2 two-process test (`tests/c9b_cross_process_locks.rs`, unix-guarded)
  all landed. **Whole-file limitation (documented):** the mapping is *pessimistic*
  — a writer holds OS-exclusive for the whole write txn (from `RESERVED`), so it
  blocks cross-process readers during a write, where SQLite's byte-range `RESERVED`
  lock keeps them. std's whole-file locks cannot express that split. Remaining:
  - **C9b-3 — autocommit reads take a cross-process shared lock. DONE 2026-07-10.**
    A bare autocommit `SELECT` now takes a *transient* `Shared` lock for the duration
    of the read (`WritePager::begin_autocommit_read`/`end_autocommit_read`, wired in
    `Connection::query_params` for a `Select` with no open txn over a `Write`
    backend), so a foreign process mid-write (holding the OS-exclusive lock under the
    pessimistic whole-file model) BUSYs the read instead of letting it see a torn
    page. Acquired before the `revalidate_read_cache` change-counter read (so that's
    covered too) and released at statement end; the clean read cache is left intact
    (the next statement's token revalidation handles a foreign commit). In-process
    this adds no contention — the OS lock is process-wide, so a sibling connection's
    write already holds it and a same-process `Shared` acquire is a no-op; only a
    cross-process exclusive holder BUSYs, and a foreign *shared* lock still coexists.
    Test: `tests/c9b_cross_process_locks.rs::foreign_exclusive_lock_blocks_an_autocommit_reader`.
    **With this, Track C's cross-process locking is complete** (the whole-file
    pessimistic-writer limitation remains documented — std locks can't express
    SQLite's byte-range `RESERVED` split).

### Track D — Virtual tables & ecosystem extensions

- **D2b-leftover (perf-only).** A ≥3-phrase `NEAR` still falls back to the
  `_content` scan (results correct). The high-frequency-term case is **no longer a
  fallback** — a spanning term's doclist-index segment is served via the index
  route (pinned by `high_frequency_spanning_term_takes_index_route`).
- **D2e-encoder — byte-identical FTS5 at large scale.** The writer is byte-identical
  vs sqlite for the mainline: doclist-index, multi-term leaf-fill, prefix indexes,
  doclist spill, incremental multi-segment writes + automerge, and incremental
  DELETE/UPDATE tombstones incl. delete-crisis merge (git history / `CHANGELOG.md`).
  **Remaining (thin tails — all fall back to the *correct* bulk rebuild, never
  wrong, just not incremental):**
  - **D2e-1 — delete-crisis with a populated higher level** (needs tombstones
    carried into the merge).
  - **D2e-2 — incremental writes inside explicit `BEGIN`/`SAVEPOINT`** (autocommit
    is incremental; explicit txns rebuild).
  - **D2e-3 — incremental path for prefix-index and spanning-dlidx segments.**
- **D4-leftover — DONE 2026-07-11 (window UDFs + custom collations).**
  *Window UDFs:* a user-registered aggregate (`Connection::register_aggregate_function`
  / C-API `sqlite3_create_function` or `sqlite3_create_window_function`) is now usable
  as a window function — `myagg(x) OVER (…)`. The window executor drives it by
  recomputing over each frame with a fresh accumulator (`fill_window_partition`'s
  aggregate arm falls back to `self.aggregates` when the name isn't a built-in), so no
  `xValue`/`xInverse` inverse protocol is needed; built-in window aggregates keep
  precedence. Verified across running / whole-partition / `PARTITION BY` / explicit
  `ROWS` frames, each matching the built-in `sum` window (`tests/window_udf.rs`, and a
  C `wsumsq` case in ctest.c).
  *Custom collations:* application-registered collating sequences work end-to-end:
  `Collation::Custom(u32)`
  (an id into a process-global, `std`-gated registry, so the public enum keeps
  `Copy`/`Send`/`Sync`/`Eq`), resolved inside `value::cmp_text`/`cmp_values_coll` so
  every comparison site — `ORDER BY`/`GROUP BY`/`DISTINCT`, `WHERE`, `UNIQUE` and
  b-tree **index** keys — uses them without threading a registry through the btree
  layer. `Connection::register_collation` (core) and `sqlite3_create_collation`(`_v2`)
  (C-API) register them; `COLLATE <name>` resolves via `resolve_collation_name` at the
  name sites. Verified: a custom collation drives ORDER BY, matches built-in `NOCASE`
  ordering (the indirect differential, since the CLI oracle can't register one), backs
  a `UNIQUE` index that passes `integrity_check`, and re-registration replaces. Limits
  (documented, never wrong): `std`-only; process-global by name; a schema declaring
  `COLLATE <name>` needs it registered before use (SQLite defers to first use — this
  errors, stricter but not wrong).
- **D5 — `sqlite3_session`. Essentially complete.** Changeset/patchset generation
  + apply (all PK shapes incl. composite/WITHOUT ROWID), `invert`/`concat`, custom
  conflict handlers (`xConflict`), per-table attach, indirect-change flagging
  (trigger/FK), and changeset rebase (`sqlite3_rebaser`) are all byte-verified vs
  the `SQLITE_ENABLE_SESSION` oracles (git history / `CHANGELOG.md`). Only
  **streaming** (`xInput`/`xOutput`) is unimplemented — an API-shape variant with
  no benefit over the `Vec` API in Rust; effectively won't-do.
- **D6 — wasm / browser bindings — DONE 2026-07-11.** Shipped as the
  **`graphitesql-wasm`** sibling crate (its own workspace, so the core stays
  zero-dep + `#![forbid(unsafe_code)]`; the bindings opt out via `wasm-bindgen` /
  `js-sys` / `web-sys`). Chosen model (per the user): **OPFS sync-access handles**
  for persistence + **wasm-bindgen** for the JS surface — no async `Connection`
  rework needed, because OPFS sync handles satisfy graphite's existing synchronous
  `Vfs`/`File` traits directly.
  - **D6-0 — model decided.** OPFS sync-access-handle backend (synchronous, so the
    engine's sync VFS is reused as-is), wasm-bindgen glue in a sibling crate,
    `wasm32-unknown-unknown` target. Persistence lives in a Web Worker (the only
    place OPFS sync handles are available).
  - **D6-1 — wasm build + in-memory bindings (DONE).** The core compiles to
    `wasm32-unknown-unknown`; the sibling exposes `Database` (`new()` in-memory,
    `exec`, `query` → `{columns, rows}`, `serialize`/`deserialize`). Value marshaling
    covers NULL/int(→number or BigInt past 2^53)/real/text/blob(→Uint8Array).
    Verified end-to-end under Node (`tests/node_smoke.mjs`), including a
    serialize→deserialize round-trip and sqlite-exact error propagation.
  - **D6-2 — persistent OPFS VFS (DONE).** An `OpfsVfs`/`OpfsFile` implementing the
    existing `Vfs`/`File` traits over pre-acquired `FileSystemSyncAccessHandle`s
    (`Database.openOpfs(files, path, create)`); the worker acquires a handle per
    file (`name`, `-journal`, `-wal`) up front. Complete runnable browser demo in
    `graphitesql-wasm/examples/` (persists across reloads). OPFS is browser-only,
    so the persistent path is browser-tested rather than in CI; CI builds + clippies
    the crate.
- **dbpage-2 INSERT leftover.** The writable `sqlite_dbpage` **UPDATE** path is
  done (patch a page's raw bytes; byte-identical to the oracle). The **INSERT**
  form is not: writing a page *beyond* EOF (`INSERT(count+1, …)`) grows the file
  while leaving the header `page_count` unchanged — a deliberately inconsistent
  state (file size ≠ header) that graphite's consistency-maintaining pager (which
  truncates the file to `page_count` at commit) prevents by construction.
  Reproducing it byte-for-byte would mean breaking that invariant to write a
  malformed file for an operation that only ever produces one; parked as an
  architectural boundary, not a blocker. (The read-side `WHERE schema='aux'`
  redirect is likewise still open.)

- **D7 — C-API shim — DONE 2026-07-11.** Shipped as the **`graphitesql-capi`**
  sibling crate (its own workspace; opts out of zero-dep + `#![forbid(unsafe_code)]`
  for the `extern "C"` + raw-pointer surface, same shape as `graphitesql-wasm`). A
  `libsqlite3`-compatible C ABI: `open`/`open_v2`/`close`, `exec` (row callback),
  `prepare_v2`/`step`/`reset`/`clear_bindings`/`finalize`, `bind_*`, `column_*`,
  `errmsg`/`errcode`/`changes`/`last_insert_rowid`, `libversion` (reports 3.50.4) —
  32 exported `sqlite3_*` symbols, matching result/type constants. Prepared
  statements are emulated over graphite's materialized query model (a `step` walks
  the computed rows; column metadata is available right after `prepare` for a
  row-producer, as in SQLite). `INSERT/UPDATE/DELETE … RETURNING` drives the row
  path (classified structurally via the engine's parser). Named/numbered bind
  parameters (`sqlite3_bind_parameter_count`/`_name`/`_index`) and **user-defined
  functions — scalar and aggregate** (`sqlite3_create_function` + the
  `sqlite3_value_*` / `sqlite3_result_*` families + `sqlite3_aggregate_context`,
  bridged onto the engine's `register_function`/`register_aggregate_function`) are
  supported — plus `sqlite3_create_window_function`, custom collations
  (`sqlite3_create_collation`), the UTF-16 entry points (`*16`), and the
  `sqlite3_update_hook` data-change notification, and the `sqlite3_commit_hook` /
  `sqlite3_rollback_hook` transaction callbacks, the online backup API
  (`sqlite3_backup_init`/`_step`/`_finish`/`_remaining`/`_pagecount`), and the
  statement-level authorizer (`sqlite3_set_authorizer`) — **80 exported
  `sqlite3_*` symbols**. Verified end-to-end by a C program (`tests/ctest.c`, run in
  CI's `capi` job) that links the cdylib and drives the full lifecycle including a
  scalar UDF in a `WHERE`, an aggregate UDF over a `GROUP BY`, a window UDF, a custom
  collation, a UTF-16 round-trip, update-hook accounting, commit/rollback-hook
  accounting with a commit veto, a whole-database online backup, a read-only
  authorizer sandbox, and buffered incremental BLOB I/O (`sqlite3_blob_*`). The
  backup is a whole-image copy (built on `Connection::restore_from`, the
  destination-side of `serialize`/`deserialize`), non-streaming like the buffered
  BLOB I/O. The authorizer is statement-level (each statement's primary action code
  with its object name, plus a table-level `READ` for a single-table `SELECT`) —
  enough for a read-only or per-table/operation sandbox; per-column `READ`
  granularity and the `FUNCTION` code are not modeled. **Track D C-API is now
  residual-free** for the surface it targets.

### Track E — Cross-database write resolution  *(essentially complete)*

A write to an attached/`temp` database swaps that database in as the active `main`
for the whole statement, while a subquery/source reading the *original* main still
resolves there. `INSERT … SELECT`/`… VALUES ((SELECT …))` are pre-materialised in
the original context before the swap.

A **schema-qualified** subquery reference in a cross-db write's WHERE/SET
(`UPDATE aux.u … WHERE a IN (SELECT a FROM main.t)`, or the target's own `aux.u`)
now resolves correctly: `resolve_db` inverts the swapped pair while the swap is
live (a `swap_active` flag; qualified refs only — unqualified keeps the active
slot).

**Remaining:** an *unqualified* name present in **both** the active db and an
attached one, referenced unqualified inside a cross-db write, binds to the active
db (graphite) rather than `main` (sqlite). Realistic schemas qualify such
references. A full fix would mean dropping the read-side swap entirely (resolve
the write target by qualifier, reads by the global `main → temp → attached` order)
— a larger refactor of the pervasive resolution path.

### CLI shell (`graphitesql`)

The shell covers the common introspection/dump commands, the full `.mode` family
(`list`/`csv`/`column`/`line`/`tabs`/`quote`/`insert`/`json`/`ascii`/`html`/`tcl`/
`markdown`/`box`/`table` — all byte-verified vs `sqlite3` 3.50.4, incl. the
`SHELL_ESC_ASCII` control-char escaping across every display mode), the output/
import layer (`.separator`/`.nullvalue`/`.output`/`.once`/`.import`/`.echo`/
`.changes`/`.print`/`.show`), and `.backup`/`.save` (backed by
`Connection::serialize()`). See git history / `CHANGELOG.md`. **Remaining (all
peripheral — the SQL engine, not the shell, is the project's purpose):**
- **CLI-1 — `.bail`. DONE 2026-07-10.** `.bail on` stops the batch at the first
  error and exits non-zero (`tests/cli_dot_commands.rs::bail_on_stops_after_error`).
  The related non-interactive exit-code part is also DONE: a piped batch now exits
  non-zero if any statement errored even under `.bail off`, matching sqlite
  (`tests/cli_dot_commands.rs::non_interactive_exits_nonzero_on_error`).
- **CLI-2 — sqlite-style error *text*. DONE 2026-07-10.** The one-shot (`-arg`)
  path now renders `Error: in prepare, <msg>` with a `^--- error here` source-line
  caret for compile-time errors and `Error: stepping, <msg> [(<code>)]` for run-time
  errors, byte-exact with the sqlite3 shell across a 34-case corpus
  (`tests/cli_error_format.rs`). Done CLI-only (no library offset threading) via
  `render_cli_error` in `src/bin/graphitesql.rs`: the caret token is located by a
  string/comment-skipping text search of the failed statement, and prepare-vs-step
  is decided by an *inverted* classification (a small stable set of step errors;
  everything else is prepare). **Long-line *windowing* DONE 2026-07-11:** a far-right
  error token (offset > 50) now slides the shown source line forward and caps it at
  78 chars, keeping the caret at a bounded column, exactly as the sqlite shell's
  `shell_error_context` does (shared `caret_block` helper; window-slide + 78-cap +
  the offset-25 caret-direction flip). *Residual (rare):* a repeated-operator token
  (`===`) whose exact fail offset only the parser knows — graphite text-searches the
  first occurrence, so its caret (and any windowing keyed off it) can point at the
  wrong `=`; closing it needs the parser's byte offset on `Error` (a public-API
  change).
- **CLI-2b — script/piped error *text*. DONE 2026-07-11.** The piped/`.read`/
  interactive path now renders the sqlite shell's *script* wording — `Parse error
  near line N: <msg>` (with the whitespace-collapsed statement and a `^--- error
  here` caret) for a prepare error, and `Runtime error near line N: <msg> (<code>)`
  for a step error — instead of the old plain `Error: error: <msg>`. `N` is the
  1-based input line the failing statement begins on: the REPL/`feed_reader` loops
  count input lines and record each group's start, and `run_sql_batch` maps the
  failing statement back to its line by locating it within the group. Reuses the
  same prepare-vs-step classification and caret geometry as CLI-2 (`render_script_
  error` beside `render_cli_error`). Byte-exact vs sqlite3 3.50.4 on stdout *and*
  stderr (compared separately — buffered stdout and unbuffered stderr interleave
  differently when merged) across an 8-script corpus incl. mid-script errors,
  multi-line-statement carets, and the (19)-coded runtime error
  (`tests/cli_error_format.rs::script_mode_error_rendering_matches_sqlite`).
- **CLI-3 — `.echo` per-input-line. DONE 2026-07-10.** `.echo on` now echoes
  dot-command input lines too (the command turning echo on is not itself echoed),
  byte-identical to sqlite3 (`tests/cli_dot_commands.rs::echo_includes_dot_command_lines`).

---

## 5. Cross-cutting concerns

- **Edition** is **Rust 2024** (`let`-chains adopted); **MSRV** moving to **1.89**
  (approved 2026-07-10) to use `std::fs::File::lock` for C9b cross-process locks
  (tracked as C9b-0).
- **Numeric model** — reals are `f64` to match SQLite; no extended decimal/bignum.
- **Text is UTF-8.** `Value::Text` is a Rust `String`, so an operation that would
  produce *non-UTF-8* "text" (e.g. `zeroblob(2)||x'ff'`) falls back to `Value::Blob`
  to preserve the bytes — the bytes match sqlite, but `typeof`/`quote` differ.
  Fixing means a bytes-backed text value, a pervasive Value-model change; deferred.
- **Parser** stays hand-written (no build-time codegen, friendlier errors);
  `parse.y` remains the source of truth for precedence and accepted forms.
- **Performance** is deliberately secondary to correctness until the VDBE + planner
  work lands; the iterator executor is `O(n)` in places (some constraint and
  `WITHOUT ROWID` paths rebuild on write) that Track B/C will revisit.

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
  by probing each semantic dimension against the `sqlite3` CLI. *(The corpus does
  NOT exercise cross-database writes — see Track E.)*
- **Fuzzing** — a deterministic corruption-robustness harness
  (`tests/fuzz_corruption.rs`, ~50k malformed-file variants; `tests/fuzz_sql.rs`,
  ~3.3k malformed/deeply-nested SQL) asserts the readers return an error and never
  panic. *(Expand toward a coverage-guided fuzzer when a no-dep path exists.)*
- **Crash-recovery** *(Done)* — a fault-injecting `FaultVfs` drives rollback-journal
  and **WAL** recovery suites asserting an `integrity_check = ok` state at every
  injection point, cross-checked with `sqlite3`.
- **SQLite's own suite** *(planned)* — run a curated slice of SQLite's `test/` TCL
  assertions (the SQL-level ones) as an additional oracle.

### Known sources of legitimate file divergence

Two SQLite-compatible writers can produce different bytes for the same logical
content; we document and accept these rather than chase them:

- Free-page reuse order and exact balancing splits; `change_counter` /
  `version_valid_for` / the embedded `SQLITE_VERSION_NUMBER`; unused/reserved bytes
  left from deletions. **Compatibility means both engines read each other's files
  and agree on contents**, not byte-identical independently-built databases.
- **Multi-row `WITHOUT ROWID` & secondary-index b-tree page layout** differs from
  sqlite's cell ordering/free-block placement (a WR table with *no* real column
  diffs identically to one with — i.e. it is layout, not the `MEM_IntReal` record
  encoding, which *does* byte-match). A separate, larger b-tree-layout parity
  project; files remain valid & mutually readable.

### Behaviors we intentionally do NOT match

- **Build-specific oracle quirks.** The pinned oracle is a custom `alt1` build:
  ICU-style full-Unicode `LIKE` case folding (graphite is ASCII-only, like
  documented SQLite); `utc`/`localtime` TZ-dependent modifiers; the read-only
  `sqlite_dbpage` write refusal; `PRAGMA function_list`/`collation_list` enumerating
  the build's loaded extensions; `PRAGMA table_list` schema-hash row order. Matching
  any of these would mean differential-testing a compile-time option.
- **Documented-undefined SQLite behavior.** A bare `ON CONFLICT DO UPDATE` on a row
  violating *multiple* uniqueness constraints ("undefined which constraint
  triggers"); a multi-match `UPDATE … FROM` join ("arbitrarily chosen"). graphite
  handles the well-defined single-match cases exactly; the ambiguous ones are
  plan-dependent and not chased.
- **Cosmetic / last-ULP residuals.** Windowed `sum()`'s sticky integer-vs-real
  *type tag* (per-frame recompute vs SQLite's `xStep`/`xInverse` `approx`
  flag — value identical, tag differs; porting window.c scheduling is high effort
  for a display tag); huge-argument `sin`/`cos` last-ULP (would need reimplementing
  glibc's libm); extreme-exponent (`|exp| ≳ 300`) `quote()` real rendering (a
  compiler-FP parser artifact — graphite's shorter form round-trips in graphite);
  a fractional-second + `-N month` date-modifier rounding edge; and the single
  pathological hex literal `-0x8000000000000000` (SQLite folds the unary minus
  into the hex literal and rejects the `INT64_MIN` overflow at parse time;
  graphite treats `-` as a runtime operator that overflows to a real — exactly
  as SQLite itself does for `0 - 0x8000000000000000`. Every other hex literal,
  signed or not, matches).

---

## 7. Suggested order

The headline features are done (§3); Tracks A, C, D & E are essentially complete
(RENAME COLUMN propagation, in-process concurrency, FTS5 mainline, `sqlite3_session`
all landed). What remains (§4) is smaller chunks plus a few bigger threads, each
independently shippable. Recommended next order:

1. **C9b — cross-process OS file locks** *(newly unblocked; MSRV 1.89 approved)*.
   Start with **C9b-0** (the MSRV bump), then the lock primitive (C9b-1) and the
   two-process differential test (C9b-2). Well-scoped, high value.
2. **A-alter-2b residual — trigger breaks via UPDATE/DELETE/VALUES/WHEN subqueries.**
   Views (A-alter-2) and the trigger INSERT…SELECT/body-SELECT subset (A-alter-2b)
   landed 2026-07-10. Remaining: a trigger break reachable only through an
   `UPDATE`/`DELETE`/`VALUES`/`WHEN` expression subquery is still accepted (a
   sound false-accept). Needs partial-rewrite propagation or a scope-aware
   per-subquery probe; low priority (same residual class as DROP COLUMN).
3. **B5b-2 — live storage cursors on the VDBE** *(in progress)*. The largest VDBE
   piece; next sub-steps are the in-interpreter `OpenRead`/`SeekRowid` opcodes and
   the affinity-blocked secondary-index / `WITHOUT ROWID` seeks. Parity-gated, low
   risk. Then **B5c-2** correlated subqueries (compile-time-validated), which also
   unblocks bare `generate_series` (A-tvf-bare-series).
4. **Cost model (Track B)** — B9h index-choice sub-items, then B9b window EQP
   (blocked on B9h); B1b selectivity-driven join order.
5. **D2e FTS5 tails** (D2e-1/2/3) and **D6 wasm** (pending the D6-0 architecture
   decision).

**Blocked by project constraints** (not effort): **D7** C-API shim (needs `unsafe`;
a sibling crate that opts out); **dbpage-2 INSERT-grow** (would write a deliberately
malformed file, breaking the pager consistency invariant); **B9j** collation-aware
index selection and **B1c** RIGHT/FULL inner seeks (correct via materialization —
plan/perf only); the Track E unqualified cross-db name residual.
