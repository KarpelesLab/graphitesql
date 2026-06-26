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
collation, column-name spans; the standing **error-parity** sweep (reject what
sqlite rejects, byte-exact messages); and cross-object **ALTER** propagation
(**A-rn1…A-rn4**: RENAME TABLE→views/FKs, RENAME COLUMN→FKs/views/triggers,
text-preserving CREATE-text edits).

**Remaining:**

- **A-rn3-edge — RENAME COLUMN in genuinely multi-table view/trigger bodies.**
  The token rewrite bails (leaves the body unchanged — never corrupts) on a bare
  column ref that is ambiguous across multiple base sources, because the AST has
  no per-column-ref source span.
  - **A-rn3-edge-1** — add a source span (byte range) to `Expr::Column`.
  - **A-rn3-edge-2** — use the span for scope-aware rename (resolve each bare ref
    to its owning table, rewrite only the matching ones).
- **Error-parity (standing).** No known leftovers; the sweep continues against new
  construct families and files any new divergence here.
  - **Eager column resolution (done for the common case).** SQLite resolves every
    column reference at prepare time, so `SELECT nope FROM t` errors `no such
    column: nope` even when the table is empty or `WHERE` filters out every row.
    The tree-walker resolves lazily (per row), so a result that reached no row
    used to swallow the error. `validate_columns_exist` now runs an eager check on
    a top-level, window-free block whose every `FROM` source is a plain base
    table/view joined by `ON`/cross joins (no `NATURAL`/`USING`): it resolves every
    column ref in the projection/`WHERE`/join-`ON`, a `table.*` whose qualifier
    names no source (`no such table: x`), and any *qualified* ref in
    `GROUP BY`/`HAVING`/`ORDER BY` (a `t.col` there is never an alias or ordinal).
    `DELETE`/`UPDATE` resolve their `WHERE` and `SET`-value columns eagerly too
    (`validate_dml_refs`, no-`FROM` updates only). Matches sqlite for that surface
    (`tests/eager_column_resolution.rs`).
  - **INSERT column-resolution messages (done).** A bad `INSERT` now reports
    sqlite's exact wording rather than graphite's old generic text: an unknown
    column in the list → `table T has no column named C`; a value-count mismatch
    with an explicit list → `M values for N columns`; and a bare/`SELECT` insert
    → `table T has N columns but M values were supplied` (where `N` counts the
    non-generated target columns). The `WITHOUT ROWID` path now also catches the
    implicit-list count mismatch it previously let slide. Both rowid and
    `WITHOUT ROWID` paths share `insert_count_mismatch`; verified against sqlite3
    in `tests/eager_column_resolution.rs`.
  - **UPDATE SET tuple-width message (done).** `UPDATE t SET (a,b) = (1)` (a
    parallel-expression tuple narrower/wider than the column list) reported a
    graphite-specific parse error with a byte location; it now raises sqlite's
    semantic `N columns assigned M values` — the same wording the row-value
    `(c1,…) = (SELECT …)` path already used (`tests/update_row_subquery.rs`).
  - **Multiple-PRIMARY-KEY message (done).** `CREATE TABLE` with two primary keys
    now quotes the table name like sqlite — `table "t" has more than one primary
    key` — for both the column-level and table-level forms
    (`tests/create_validation.rs`).
  - **JSON non-text / NULL paths (done).** A path argument to any json1 path
    function is now coerced to text before validation (matching sqlite), so an
    `INTEGER`/`REAL`/`BLOB` path that does not spell a `$`-rooted path raises
    `bad JSON path: '<text>'` instead of silently resolving to NULL —
    `json_extract(j, 1)`, `json_set(j, 5, …)`, etc. And a NULL path in
    `json_extract` and `json_remove` now short-circuit the whole call to NULL,
    scanning left to right (a malformed path *before* the NULL still errors
    first; for `json_remove`, removals already applied are discarded)
    (`tests/json_surface.rs`).
  - **`strftime` defaults & format coercion (done).** `strftime(FORMAT)` with no
    time-value now defaults to 'now' (like `date()`/`time()`/`datetime()`)
    instead of returning NULL, and a non-text format is coerced to text before
    rendering (`strftime(123)` → `123`, `strftime(x'41')` → `A`); only a NULL
    format yields NULL (`tests/strftime_null.rs`).
  - **`likelihood()` probability validation (done).** The second argument to
    `likelihood(X, Y)` must be a floating-point *literal* in `0.0..=1.0`, checked
    against the parsed AST like sqlite's `exprProbability`: an integer literal
    (even `0`/`1`), a negative, a string, a compound expression (`0.5+0.1`), or a
    column reference is now rejected with `second argument to likelihood() must
    be a constant between 0.0 and 1.0`, while `0.5`/`.5`/`1e0`/`(0.5)` are
    accepted. `likelihood` was dropped from the VDBE pure-function list so the
    all-constant case still sees the source expression (`tests/likelihood_probability.rs`).
    *Remaining (both cases):* graphite validates lazily, so an unreached row
    (`SELECT likelihood(a,2) FROM empty`) is not rejected at prepare time as
    sqlite would; a statement-level prepare pass would be needed.
  - **DISTINCT-aggregate arity message (done).** A `DISTINCT` aggregate with a
    number of arguments other than one now reports sqlite's `DISTINCT aggregates
    must have exactly one argument`, checked after the function's upper arity
    bound (so `count(DISTINCT 1,2)`/`sum(DISTINCT 1,2)` still report "wrong number
    of arguments") but before the lower bound (so `count(DISTINCT)`, whose 0-arg
    form is valid as `count(*)`, and `group_concat(DISTINCT a,b)` report the
    DISTINCT message) (`tests/distinct_aggregate_arity.rs`).
  - **`count()` with no arguments behaves as `count(*)` (done).** The bare
    no-argument `count()` now tallies every row of the group (NULLs included),
    matching sqlite, in scalar, `GROUP BY`, and windowed (`count() OVER (…)`)
    positions — previously graphite rejected it with "wrong number of arguments"
    (`tests/count_no_args.rs`).
  - **`string_agg` arity and JSON-group-aggregate recognition (done).**
    `string_agg(X)` now requires its separator (exactly two arguments, the
    one-argument form reporting `wrong number of arguments to function
    string_agg()` — even with `DISTINCT`, since the arity check precedes the
    DISTINCT one), matching sqlite rather than treating it as a `group_concat`
    alias. And the JSON group aggregates (`json_group_array`/`json_group_object`
    and their `jsonb_` variants), which have no scalar form, are now recognized
    as aggregates at *any* arity, so a wrong count reports `wrong number of
    arguments` from the aggregate guard instead of falling through to
    `no such function` (`tests/agg_name_arity.rs`).
  - **Aggregate in `GROUP BY` is rejected (done).** An aggregate function
    anywhere inside a `GROUP BY` term now reports sqlite's dedicated `aggregate
    functions are not allowed in the GROUP BY clause` — including the nested
    (`1 + count(*)`) and output-alias-rewritten (`SELECT count(*) AS c … GROUP BY
    c`) forms, and the `GROUP BY max(a)` over a real table that lazy per-row
    evaluation previously accepted silently. The check runs after alias/positional
    resolution; aggregates remain legal in `HAVING`/`ORDER BY`
    (`tests/group_by_aggregate.rs`).
  - **Window function in `WHERE`/`HAVING` is "misuse" (done).** A call carrying
    an `OVER` clause — including an aggregate such as `sum(x) OVER ()`, not just
    the ranking functions — now reports `misuse of window function NAME()` when
    it appears in `WHERE`/`HAVING`, matching sqlite, instead of falling through
    to the generic "aggregate … used outside an aggregate context". The per-row
    evaluator only ever sees an `OVER` call in a disallowed position, since valid
    window calls are pre-computed and substituted out first
    (`tests/window_misuse.rs`).
  - **Row value in a scalar context is "row value misused" (done).** A row value
    `(a, b, …)` used where a single scalar is expected — bare in the SELECT list,
    as an arithmetic operand, a function argument, or a bare `WHERE` predicate —
    now reports sqlite's `row value misused` instead of graphite's own wording
    ("row value used where a single value is expected"). The legal row contexts
    (`(a,b) = (c,d)`, `(a,b) < (c,d)`, `(a,b) IN (…)`) are unaffected
    (`tests/row_value_misused.rs`).
    *Remaining:* a *multi-column subquery* in a comparison (`(SELECT 1,2) = 1`)
    should likewise report `row value misused`, but graphite still reports
    `sub-select returns 2 columns - expected 1` there — the column-count message
    is correct in the SELECT-list scalar context (`SELECT (SELECT 1,2)`) but the
    comparison operand needs context-aware detection to switch wording.
    *Remaining:* extend it past the conservative scope — derived-table/subquery
    scopes, `NATURAL`/`USING` coalesced names, and *bare* `GROUP BY`/`HAVING`/
    `ORDER BY` refs (need output-alias/ordinal awareness) are still left to lazy
    resolution; and ALTER-time re-validation that *rejects* an ALTER making a
    dependent view/trigger unresolvable needs statement-level DDL rollback (a
    writer savepoint around `exec_alter`, like `run_dml_atomic`).
  - **Duplicate CTE name within one `WITH` clause is rejected (done).** Two CTEs
    sharing a name in the same `WITH` list (`WITH x AS (…), x AS (…) …`) now
    report sqlite's `duplicate WITH table name: NAME` (case-insensitive, naming
    the duplicate occurrence) instead of being silently accepted, across
    `SELECT`/`UPDATE`/`DELETE` and with `RECURSIVE`. A same name re-used in a
    *nested* `WITH` is a separate scope and stays legal
    (`tests/duplicate_cte_name.rs`).
  - **An unrecognized `PRAGMA` name is silently ignored (done).** sqlite raises
    no error for an unknown pragma — it returns no rows and moves on. graphite's
    write path already no-oped unknown setters (`PRAGMA made_up = 1`), but the
    read/function forms (`PRAGMA made_up`, `PRAGMA made_up(1)`) errored with
    "not yet implemented: this PRAGMA". The `run_pragma` catch-all now returns an
    empty result instead, so a probing tool/ORM that reads an unsupported pragma
    sees sqlite's behaviour (`tests/unknown_pragma_ignored.rs`).
  - **Rowid allocation no longer panics at the `i64::MAX` boundary (done).**
    Inserting a row whose auto-assigned rowid would exceed `i64::MAX` used to
    panic with "attempt to add with overflow". It now matches sqlite: an
    `AUTOINCREMENT` table fails with `SQLITE_FULL` ("database or disk is full")
    since AUTOINCREMENT never reuses a rowid, while a plain rowid / `INTEGER
    PRIMARY KEY` table picks a random free rowid and succeeds. `next_rowid`
    saturates and the new `auto_rowid` helper detects the exhausted range; the
    result still passes `PRAGMA integrity_check` (`tests/rowid_overflow.rs`).
  - **`trim`/`ltrim`/`rtrim` with a NULL trim-set return NULL (done).** sqlite
    propagates a NULL second argument like any other NULL (`trim(X, NULL)` is
    NULL); graphite previously treated it as an empty trim-set and returned the
    subject unchanged. The one-arg forms and a NULL subject were already NULL,
    and a non-NULL or empty-string set still trims as before
    (`tests/trim_null_set.rs`).

### Track B — Query planner, statistics & the VDBE

**Done:** `ANALYZE`/`sqlite_stat1` stats-driven planning; the full equality/range/
`IN`/OR-union + inner-join + `WITHOUT ROWID` seek family; hash join + automatic-
index EQP; index-driven `ORDER BY` and covering reads (**B0/B0b**); the VDBE spike
+ its scalar-expression compiler; the **VDBE join family** (**B5a**); **VDBE
routing default-on** (**B7a/B7b**, ~93 % of the corpus, tree-walker is the
fallback oracle); bytecode `EXPLAIN` (**B8**); the router's **non-correlated
scalar/`EXISTS` subquery fold** pre-pass (folds such subqueries — in the
projection, `WHERE`/`HAVING`/`GROUP BY`/`ORDER BY`/join-`ON`, **and now
`LIMIT`/`OFFSET`** — to constants so the VDBE runs the rest; never changes a
result).

**Remaining — move each remaining shape onto the VDBE.** Additive, gated on
VDBE-vs-tree-walker parity; results are already correct via the fallback, so this
is *perf/coverage*, not correctness.

- **B5c-1 — `IN (SELECT …)` on the VDBE — DONE.** A non-correlated `IN (SELECT)`
  now runs on the VDBE for both candidate kinds. *Computed candidate:* folded to
  `IN (list)` (a computed result carries NONE affinity, so it's equivalent).
  *Bare-column candidate (the hard case):* the router carries the candidate
  column's affinity (resolved via `subquery_column_origins`, as a canonical type
  name) into a new `Expr::InList.candidate_affinity` field; the VDBE's IN OR-chain
  feeds it as each `Op::Compare.ra`, so the existing
  `apply_comparison_affinity(left_aff, candidate_aff)` runtime step computes
  SQLite's `combine` — crucially an untyped candidate is `Some(Blob)`, NOT `None`,
  so `lt.x(TEXT) IN (SELECT cn.y)` → `combine(TEXT,Blob)` → no coercion → no match
  (matching sqlite), where a plain literal-list fold wrongly coerced. `NOT IN` and
  NULL-in-set fall out of the existing OR-chain. The candidate column's collation
  is NOT consulted — `IN (SELECT)` uses the LEFT operand's collation (verified vs
  sqlite: a NOCASE candidate never changes the result), so NOCASE/RTRIM candidate
  columns route too. Verified across every left×candidate affinity combo vs
  sqlite3 (`tests/subquery_diff.rs`, `tests/vdbe_in_select.rs`), tree-walker ==
  VDBE == sqlite.
  - *Implementation note (investigated 2026-06-25):* a fold to `IN (list)` CANNOT
    work — list literals report NONE comparison-affinity, but the bare-column
    candidate must contribute its column affinity; and wrapping each value in
    `CAST(v AS coltype)` is wrong because CAST changes the *value* (a non-numeric
    text in an INTEGER-affinity column → `0`), not just the comparison affinity.
    The correct path: carry the candidate affinity into the VDBE's `Op::Compare.ra`
    for the IN OR-chain (the chain is built in `vdbe.rs` `Expr::InList`, affinity
    via `push_compare_coll`→`expr_affinity`). Cleanest minimal change is an
    `InList { …, candidate_affinity: Option<Affinity> }` field the router sets when
    folding a bare-column `IN (SELECT)` (then `expr_affinity` of each element
    yields that). This is **perf-only** (the tree-walker already returns correct
    results, so no differential test distinguishes done from not-done) and touches
    the default VDBE path — do it deliberately in a focused turn, not as a quick
    win.
  - *Confirmed 2026-06-26 (a fold attempt was tried and REVERTED — do NOT retry
    it): bare-column `IN (SELECT col)` cannot be folded to `IN (list)` under ANY
    affinity guard.* SQLite applies the LEFT operand's affinity to an `IN (list)`,
    but for `x IN (SELECT y)` it uses `combine(left_aff, y_aff)` where `y_aff` is
    the **subquery result column's** affinity — and these diverge precisely for a
    bare column. Verified: `lt.x(TEXT) IN (SELECT y+0 FROM cn)` (computed
    candidate, NONE aff) → 1 == folded `IN (list)` (SQLite uses left TEXT aff,
    like the fold), but `lt.x(TEXT) IN (SELECT cn.y)` (bare untyped column) → 0
    (SQLite uses `combine(TEXT, NONE)=BLOB`, no coercion) while a fold to
    `IN (1)` wrongly applies TEXT and yields 1 (`tests/subquery_diff.rs::
    in_select_affinity_matches_sqlite3`). So the existing `is_bare_column_expr`
    bail is **correctness, not conservatism** — the bare-column candidate is
    exactly the divergent case. Only the native candidate-affinity membership op
    (B5c-1a/b above) can route it.
- **B5c-2 — correlated subqueries on the VDBE.**
  - **B5c-2a** — thread the outer row into the subquery's register frame.
  - **B5c-2b** — compile `Subquery`/`Exists`/`InSelect` that read an outer column.
- **B5c-3 — compound `SELECT` (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`) — DONE.**
  `run_compound_vdbe` runs each constituent SELECT through the VDBE
  (`run_select_vdbe`) and folds the row-sets left-associatively. The set-combine,
  the post-dedup sort (rows in sorted order for `UNION`/`INTERSECT`/`EXCEPT`,
  preserved for `UNION ALL`), and the whole-query `ORDER BY`/`LIMIT`/`OFFSET`
  reuse the tree-walker's exact helpers (`apply_compound`, `compound_order_limit`,
  collation-aware via the left SELECT's output collations), so the result is
  byte-identical. A nested-compound arm (e.g. a multi-row `VALUES`, which
  desugars to a `UNION ALL` chain), a CTE-bearing arm, or any arm the VDBE
  cannot run defers the whole query to the tree-walker. Differentially tested
  vs sqlite 3.50.4 (`tests/vdbe_compound.rs`), incl. `1`/`1.0` dedup keeping the
  later representation, three-arm left-associativity, per-arm `WHERE`, and the
  column-count-mismatch error.
  - **B5c-3a** — *(folded into the above: each arm runs on the VDBE.)*
  - **B5c-3b** — *(folded into the above: collation-aware set-combine + sort.)*
- **Positional `GROUP BY <ordinal>` on the VDBE — DONE.** A bare integer
  `GROUP BY N` term names the N-th *output* column, not the constant N (SQLite's
  rule). The router (`run_select_vdbe`) rewrites each such term to the referenced
  result column's expression — mirroring the tree-walker's resolution — before the
  grouped compiler runs, so `GROUP BY 1` / `GROUP BY 1, 2` / a positional term
  mixed with a named one all run on the VDBE. A wildcard projection (`*`) or an
  out-of-range ordinal defers to the tree-walker, which validates and emits the
  exact `… GROUP BY term out of range …` error. Differentially tested
  (`tests/vdbe_grouped.rs`, incl. positional + `HAVING`/`ORDER BY`/`LIMIT`).
- **`DISTINCT` aggregates on the bare aggregate path — DONE.** `count(DISTINCT x)`,
  `sum`/`avg`/`total`/`min`/`max`/`group_concat` with `DISTINCT` now run on the
  single-table aggregate path (no GROUP BY). The interpreter already collects each
  aggregate's non-NULL arguments into a `Vec<Value>`, so `DISTINCT` simply skips a
  value equal (BINARY) to one already collected (`Op::AggStep { distinct }`),
  folding over distinct values only. A non-BINARY column collation still defers to
  the tree-walker (that path already bailed on non-BINARY), so the NOCASE case is
  handled correctly by fallback. Differentially tested vs sqlite 3.50.4
  (`tests/vdbe_distinct_agg.rs`). `FILTER`/`ORDER BY` aggregates still fall back.
  - *Also done — `DISTINCT` aggregates **over `GROUP BY`**.* `AggSpec`/`Op::GroupStep`
    gained the same `distinct` flag, deduping each group's collected values at fold
    time, so `SELECT a, count(DISTINCT b) … GROUP BY a` (and the `HAVING`/`ORDER BY`/
    `LIMIT` general path, mixed with plain aggregates) runs on the VDBE — for both
    the single-table and the nested-loop-join grouped compilers (they share
    `emit_group_fold`). The grouped path already bailed on non-BINARY collations, so
    the dedup is BINARY-safe. Tested in `tests/vdbe_distinct_agg.rs` with
    within-group duplicate values.
  - *Also done — `DISTINCT` aggregates **over a bare two-table join**.* The
    bare-aggregate join compiler (`compile_aggregate_join`, no GROUP BY) now
    threads the same `distinct` flag into its `Op::AggStep` (previously hardcoded
    non-distinct), so `SELECT count(DISTINCT a.v), sum(DISTINCT b.w) FROM a JOIN b
    …` folds-and-dedups over the joined rows on the VDBE. BINARY-safe (the path
    already bails on non-BINARY collation). Tested in `tests/vdbe_distinct_agg.rs`
    with a join that replicates a value so the dedup is observable.
- **Aggregate `FILTER (WHERE …)` on the VDBE — DONE.** `count(*) FILTER (WHERE
  …)`, `sum`/`avg`/`min`/`max`/`group_concat`/… with a per-aggregate `FILTER`
  clause now run on the VDBE across all aggregate paths (bare single-table, over
  `GROUP BY`, and over a two-table join), composing with `DISTINCT`. `Op::AggStep`
  and `AggSpec` each gained a `filter: Option<usize>` register; the fold evaluates
  the predicate per row and a row that is not *true* contributes to neither the
  count bump nor the collected values for that aggregate (each aggregate's filter
  is independent). `agg_kind_distinct` now binds the `FILTER` predicate instead of
  bailing on it. Differentially tested vs sqlite 3.50.4 in
  `tests/vdbe_filter_agg.rs`.
- **Ordered `group_concat(x ORDER BY …)` on the VDBE — DONE.** A
  `group_concat` with an inner `ORDER BY` now runs on the VDBE across all
  aggregate paths (bare single-table, over `GROUP BY`, and over a two-table
  join). The accumulator (`AggAcc`, now a struct rather than a tuple) records
  each collected value's `ORDER BY` key row alongside the value; finalization
  sorts a stable index permutation under those keys (BINARY collation, SQLite
  NULL placement — NULLs first under `ASC`, last under `DESC`, honoring explicit
  `NULLS FIRST`/`LAST`) before joining. `Op::AggStep`/`AggSpec` carry an
  `order: Vec<AggOrderKey>` of pre-compiled key registers; `agg_kind_distinct`
  binds the `ORDER BY` terms. Tightly scoped: only single-arg `group_concat`
  ordered — `DISTINCT` + `ORDER BY`, and an `ORDER BY` on any other aggregate,
  still fall back (no regression — those bailed before). Differentially tested
  vs sqlite 3.50.4 in `tests/vdbe_ordered_agg.rs`. Windowed (`OVER`) calls still
  fall back.
- **B5c-4 — window functions over a single table *or a plain join* on the VDBE —
  DONE.** A window-function `SELECT` (`row_number`/`rank`/`dense_rank`/`ntile`,
  aggregate `OVER` with `PARTITION BY`/`ORDER BY`/explicit frames, `lag`/`lead`/
  `first_value`, named `WINDOW` definitions, and windows *over* `GROUP BY`/
  aggregates) over one plain main-database table — or over a plain N-table join —
  now runs on the VDBE. The window evaluation itself is not bytecode:
  `run_window_vdbe` scans the source on the VDBE (a single table as `SELECT *,
  rowid`, a join as `SELECT *`) with the query's `WHERE` applied, so the
  scan/filter/join run as compiled opcodes, then feeds the rows to
  `finish_from_rows`, the post-`WHERE` tail extracted from `run_core` (windows,
  aggregation, projection, `DISTINCT`, `ORDER BY`, `LIMIT`/`OFFSET`) that the
  tree-walker already used, so the result is identical by construction. A single
  table appends its rowid so a `rowid` reference resolves; a joined row has no
  single rowid, so the join path supplies `None` and *defers whenever the query
  references a rowid* anywhere (projection, `WHERE`, `GROUP BY`, `HAVING`, `ORDER
  BY`, or any window's `PARTITION BY`/`ORDER BY`). A derived/virtual/view source,
  `WITHOUT ROWID` table (its rowid projection bails), `NATURAL`/`USING` join, CTE,
  non-`main` schema, or any join shape the VDBE join engine can't scan (e.g. tables
  sharing a column name) falls back. An outer `ORDER BY` satisfied by the scan also
  defers (the existing guard), so window queries use a non-rowid/multi-term `ORDER
  BY`. Differentially tested vs sqlite 3.50.4 in `tests/vdbe_window.rs`.
- **B5b — per-cursor nested-loop join + inner seek** (stream the inner side
  instead of materializing the cross-product; *perf-only*).
  - **B5b-1** — *Done (multi-cursor opcodes + nested-loop join over materialized
    per-table rows).* The VDBE gained multi-cursor opcodes `RewindC`/`ColumnC`/
    `NextC` (each carrying a `cursor` id) and a `run_rows_multi` interpreter that
    threads one position per cursor; the single-cursor `Rewind`/`Column`/`Next`
    and the whole default path are byte-for-byte unchanged (cursor 0 backs them).
    A new `compile_join2` emits a nested loop `RewindC 0 [ RewindC 1 <body>
    NextC 1 ] NextC 0` (outer = left, outermost), with a `Compiler.cursor_boundaries`
    map turning each combined column index into a `(cursor, local col)` `ColumnC`.
    The compiler is N-ary (`boundaries` = cumulative per-cursor column counts):
    the router runs a plain **N-table** inner join (projection + WHERE-merged-ON +
    `DISTINCT` (BINARY) + `ORDER BY` (staged through a sorter) + constant
    LIMIT/OFFSET) as an N-deep nested loop — *no `t1 × … × tN` cross-product is
    materialized for any arity*. A **bare-aggregate** join (`count(*)`, `sum`,
    `min`/`max`, `group_concat`, … with no GROUP BY) likewise folds over the nested
    loop via `compile_aggregate_join` (mirroring the single-table
    `compile_aggregate_select`) and emits one finalized row — again with no
    cross-product materialized; the fold order matches the cross-product so
    order-sensitive `group_concat` is byte-identical. A **`GROUP BY`** join folds
    each surviving combined row into its group over the nested loop — *the full
    grouped grammar*: keys + aggregates, optional **HAVING / ORDER BY / LIMIT /
    OFFSET**, no cross-product, same first-seen group order. This came for free by
    factoring the fold loop into `emit_group_fold` (single-cursor `Rewind`/`Column`
    when `cursor_boundaries` is `None`, else the N-deep `RewindC`/`ColumnC`/`NextC`)
    and generalizing `compile_group_select`/`compile_group_emit` over an optional
    `boundaries` slice; `compile_group_join` is now a thin adapter that expands the
    projection and delegates, so a join inherits the single-table compiler's plain
    `GroupEmit` path *and* its general `GroupFinalize`/`GroupNext` emit (with the
    HAVING skip and ORDER-BY sorter) unchanged. Only `GROUP BY DISTINCT` and
    aggregates needing DISTINCT/FILTER/ORDER BY still fall back to the cross-product
    path. An ordered, grouped inner join is now
    feature-complete on the VDBE (parity with the single-table scan compiler). Row
    order matches the cross-product and sqlite. Verified by
    the differential join corpus (2-, 3-, 4-table) + direct unit tests
    (`exec::vdbe::tests`, `tests/vdbe_nested_join.rs`). This is the multi-cursor
    foundation the rest of B8 (storage cursors, correlated subqueries, windows)
    builds on.
    - *Also done — two-table `LEFT`/`RIGHT`/`FULL JOIN` on the VDBE.*
      `compile_left_join2` emits a null-padding nested loop: for each preserved-side
      (cursor 0) row the inner cursor is scanned, a `NullRow` opcode (cleared by
      `RewindC`, makes a cursor's `ColumnC`s read NULL) pads the output when no
      inner row satisfied `ON`. The `ON` is kept SEPARATE from `WHERE` (unlike the
      inner-join merge — they differ for the unmatched row): `ON` gates the match,
      `WHERE` filters every emitted row including the null-padded one. `RIGHT JOIN`
      reuses the same compiler with the operands swapped (cursor 0 = the preserved
      right table; column refs resolve by name regardless of cursor order). `FULL
      JOIN` (`compile_full_join2`) is a two-pass nested loop: pass 1 is the LEFT
      join, additionally recording matched right rows in a per-row bitmap
      (`MarkMatched`); pass 2 scans the right table and emits each row NOT matched
      (`IfMatched` skips the rest) with the left side NULL — yielding SQLite's
      FULL-join order (left-driven rows, then unmatched-right). A single 2-table
      `LEFT`/`RIGHT`/`FULL JOIN` (no `NATURAL`/`USING`) routes here; `NATURAL`/
      `USING`, 3+-table or non-nested-loopable shapes fall back to the tree-walker.
      `ORDER BY` and `DISTINCT` over these outer joins are now supported too, at
      full parity with the inner-join path. Every emission point (matched, left-null,
      and — for FULL — right-null) projects its row, then a `DistinctCheck` gates it
      on uniqueness (when `DISTINCT`, BINARY collation only) and — under `ORDER BY` —
      stages the row + sort keys into one sorter (the null-padded row's keys/columns
      read NULL via the `NullRow`-cleared `ColumnC`s); a single sorted emit loop then
      applies OFFSET/LIMIT to the ordered output. `DISTINCT` collapses duplicate
      matched AND duplicate null-padded rows (two all-NULL right sides compare equal),
      across both FULL passes. Verified vs the differential corpus + unit/integration
      tests (incl. `WHERE …col IS NULL`, both empty sides, compound `ON`, `coalesce`
      over nulls, LIMIT/OFFSET across passes, `ORDER BY` with NULL-first/last keys,
      and `DISTINCT` over duplicate matched/null-padded rows).
    - *Also done — N-table `LEFT`/`INNER` join chains on the VDBE.*
      `compile_left_join_n` generalizes the two-table null-padding loop to a
      left-deep chain of 2–4 joins, all `LEFT` or `INNER` with at least one `LEFT`
      (no `NATURAL`/`USING`). A recursive emitter (`emit_join_level`) nests one
      cursor per table — cursor 0 the always-preserved base — and at each level
      keeps a per-outer-row "matched" flag: a `LEFT` level whose `ON` matched no
      inner row null-pads that cursor (`NullRow`) and *still* recurses into the
      deeper cursors (whose `ON`s may reference the now-NULL columns), while an
      `INNER` level simply yields nothing. Each join's `ON` gates matches at its own
      level (kept separate from `WHERE`); `WHERE` filters the fully-assembled row at
      the innermost leaf, where projection + `DISTINCT` (BINARY) + `ORDER BY` (one
      sorter) + constant `LIMIT`/`OFFSET` reuse the same emit helpers as the
      two-table path. The fold order (outermost cursor slowest, each level's matches
      in scan order then its null-padded row) matches SQLite's left-deep join order.
      A `RIGHT`/`FULL` join anywhere, `NATURAL`/`USING`, a depth > 4, or a
      GROUP-BY/aggregate/HAVING shape falls back. Differentially tested vs sqlite
      3.50.4 in `tests/vdbe_outer_join_chain.rs` (3- and 4-table `LEFT`/`INNER`
      mixes, mid-chain and tail null-padding, fully-null tails, `WHERE …IS NULL`,
      `DISTINCT`, `coalesce`, and `LIMIT`/`OFFSET`).
  - **B5b-2** — seek-driven inner cursor (rowid/PK/index) over real storage
    (`OpenRead` + a b-tree `TableCursor`), mirroring the tree-walker's inner-join
    seeks. Needs the VDBE interpreter to hold live storage cursors (the larger B8
    step); today both cursors are materialized per-table row-sets.
- **B1c — RIGHT/FULL join inner seeks** (INNER/LEFT seek; RIGHT/FULL still
  materialize the inner table).

**Blocked / deferred by design:**
- **B1b — cost-based join reordering.** graphite's per-cursor seek/bloom-filter
  choices diverge from sqlite's cost-reordered plain scans *by design*; matching
  the EQP would mean abandoning often-cheaper access paths. Results already correct.
- **B4 — `sqlite_stat4` histograms.** The pinned `sqlite3 3.50.4` oracle is built
  without `SQLITE_ENABLE_STAT4`, so there is nothing to diff against and a
  stat4-driven planner would *diverge* the EQP corpus. Needs a stat4-enabled oracle.

### Track C — Storage engine, transactions, concurrency

**Done:** the `Vfs` locking contract; rollback-journal writer serialization;
`SAVEPOINT`; the introspection PRAGMAs; the **entire multi-schema track** (C1–C5,
`ATTACH`/`DETACH`/`TEMP`); the **whole `auto_vacuum` track** (C6); `VACUUM …
INTO`; `secure_delete` (C8a); `cache_size`/`mmap_size` reporting (C8b).

**Remaining:**

- **C7 — SQLite-format rollback journal.** *Done (C7a + C7b):* `write_journal`
  now emits SQLite's exact byte layout (8-byte magic `d9 d5 05 f9 20 a1 63 d7`,
  record count, checksum nonce, initial page count, sector/page sizes, header
  padded to one sector; then `(pgno, page, 4-byte sparse checksum)` records), with
  the count published only after the records sync (sqlite's two-flush protocol).
  `recover` detects a hot journal, validates the header, replays each
  checksum-valid record, and truncates to the recorded size — stopping at a torn
  tail and walking multiple sector-aligned segments. Cross-verified **both ways**
  vs `sqlite3` 3.50.4: graphite's hot journal recovered by sqlite, and a
  sqlite-authored hot journal (forced cache-spill + mid-transaction SIGKILL)
  recovered by graphite (`tests/journal_sqlite_format.rs`).
  - **C7-harness** — *Done:* a fault-injecting `FaultVfs` (kills a chosen file at
    the Nth `write`/`truncate`/`sync`, optionally a torn half-write, freezing the
    on-disk bytes like a power loss) drives the §6 crash-recovery suite
    (`tests/crash_recovery_harness.rs`). Recovery held at every injection point —
    before/midway/torn db-page writes, on the db `sync`, on the finalizing
    journal-clear, and a full ordinal×{clean,torn} sweep — each reopen
    `integrity_check = ok`, never torn, cross-checked with `sqlite3`. No recovery
    bug found. graphite's own writer still emits a single journal segment (it
    buffers the whole overlay and never spills mid-transaction); multi-segment
    *writing* would land with the bounded pcache (C8c).
- **C8c — bounded `pcache` with LRU eviction.** *Done:* a `PageCache`
  (`src/pager/pcache.rs`, **C8c-1**) — fixed-capacity LRU over clean-page images,
  capacity from `cache_size` (positive = pages, negative = KiB/page_size, default
  `-2000`), evicting the LRU clean page when a new key would exceed it. The read
  `Pager`'s keep-everything map is replaced by it (**C8c-2**); `WritePager` gets a
  bounded clean read-cache that the overlay/WAL shadow, so a **dirty page is
  structurally never evictable**, invalidated on lock acquire/release/commit for
  cross-connection coherence. Eviction is transparent (re-read on miss) — verified
  correct + bounded under tiny caches with writes/rollback, cross-checked vs
  `sqlite3` and `tests/concurrency.rs` (`tests/pcache_bounded.rs`).
  - *Leftover:* the `WritePager` read cache is active only inside a write txn
    (sole-writer); a pure read-only connection over a read-write file falls back to
    direct (already-bounded) disk reads, since coherent caching there needs a
    statement-level change-counter revalidation hook from the exec layer.
- **C9 — concurrency** (each independent):
  - **C9a** — reader `SHARED`-lock sharing. *Lock model done + verified:*
    `LockState` (`src/vfs/mod.rs`) already counts `Shared` holders — many readers
    coexist, `Reserved` is admitted under readers (single write-intent),
    `Exclusive` BUSYs until every *other* reader drains — proven end-to-end over
    both the std (per-path registry) and memory VFS (`tests/concurrency_readers.rs`,
    6 tests). *Remaining (pager-owned):* the pager takes `Shared` only transiently
    on the way to `Reserved` (pure reads hold no persistent read lock), so
    "multiple readers across an open read txn block a writer" isn't yet observable
    at the `Connection` layer — needs a persistent read-lock policy in
    `src/pager/`.
  - **C9b** — OS-level cross-process file locks (`std::fs::File::lock`; wants MSRV
    1.89) behind the std VFS.
  - **C9c** — the WAL `-shm` wal-index for multi-connection WAL readers.
  - **C9d** — a thread-safe `Connection` (`Send`/`Sync`, or a documented
    per-thread model).

### Track D — Virtual tables & ecosystem extensions

**Done:** the read-only vtab foundation (TVFs, the `VTabModule`/`VTabRegistry`
trait, `best_index`/`filter` pushdown — **D1**); the **writable, persistent** vtab
layer (**W1/W2**); both headline modules — the full **R-Tree** (**D3a–D3c**,
byte-compatible nodes) and the full **FTS5** (**D2a–D2e**, read + write,
sqlite-readable on disk, with the `tokenize=` option chain + diacritic folding);
Rust scalar/aggregate **UDFs** (**D4**); the **`dbstat`** vtab.

**Remaining:**

- **D2b — a real FTS5 inverted index.** Today `MATCH` scans `_content`; the
  on-disk segment *format* is already byte-compatible (written by
  `fts5_rebuild_index`), so this is the *read* path that uses the index for scale.
  - **D2b-1** — *Done:* decode the `%_data` segment index (leaf header, page-index
    footer, prefix-compressed term keys, doclists, position lists) for a
    single-term lookup — `decode_term` in `src/fts5_index.rs`, the byte-inverse of
    the writer. Single-leaf segments only; a spanning/interior/doclist-index
    segment returns `None` so the caller falls back to the `_content` scan.
    Verified by writer→decoder round-trips and against `sqlite3`-written leaves
    (`tests/fts5_decode.rs`).
  - **D2b-2** — *Done (single bare term, table-wide + column-scoped):* a single
    bare-term `MATCH` — both table-wide (`tbl MATCH 'word'`) and column-scoped
    (`tbl MATCH 'col : word'` / `col:word`) — over a fully-indexed single-segment
    table is answered via the index: `lookup_term_rowids` /
    `lookup_term_rowids_in_column` (`src/fts5_index.rs`, filtering the term's
    per-column positions to the named column) → `fts5_try_index_match`
    (`src/exec/mod.rs`, `AnyColumn`/`InColumn` routes) seeks just the matching
    `_content` rows by rowid. Everything else (phrases, `NEAR`, prefixes, boolean,
    `^`, multi-column filters, `UNINDEXED` columns, multi-segment / interior /
    dlidx) stays on the `_content` scan. Results are a guaranteed superset
    re-filtered by `run_core`, so rows/order/`bm25`/`highlight` are identical —
    verified vs `sqlite3` (`tests/fts5_index_match.rs`) + a route counter.
    Also routed: a **two-term phrase** (`tbl MATCH '"a b"'`, table-wide and
    column-scoped) via doclist intersection + per-column position adjacency
    (`lookup_phrase_rowids` — token a at `p`, token b at `p+1` in the same column,
    repeated-word `"a a"` handled), identical to the scan's `fts5_phrase_starts`.
    And an **N-operand bare-term boolean tree** (`a AND b AND c`, `a OR b AND c`,
    `(a OR b) AND NOT c`, …) — `lookup_bool_tree_rowids`/`eval_bool_tree` walk
    graphite's parsed FTS5 AST (the same tree `fts5_eval` evaluates, so `NOT > AND
    > OR` precedence is inherited) and combine the leaves' doclists bottom-up with
    sorted-merge intersect/union/difference; routed-result == scan == sqlite
    (precedence pinned by differential tests). And a **prefix term**
    (`tbl MATCH 'wor*'`, table-wide and column-scoped) — `lookup_prefix_rowids`
    walks the sorted leaf term keys, unions the doclists of every term sharing the
    prefix; matches the scan (prefix tokens are not Porter-stemmed). And a
    **two-term `NEAR(a b, n)`** (default n=10) — `lookup_near_rowids` intersects
    the two terms' doclists and keeps docs with a per-column position pair within
    the span; the distance inequality `|pa-pb| <= n+1` is derived from the scan's
    general `max_end - min_start < n + total_len` rule (two single tokens →
    total_len 2) and pinned vs `sqlite3` at the boundary. **Multi-segment:**
    bare-term / column / boolean / prefix routing now also works over the
    **multiple segments** a real `sqlite3` index accumulates (`all_segments` +
    `merge_segments` union the per-segment doclists), bailing to the scan on a
    tombstone or an overlapping docid (update/delete history) — verified vs
    `sqlite3` on a genuine >1-segment index incl. a deletes-present case
    (`tests/fts5_index_multiseg.rs`). **K-term phrases:** phrase routing now covers
    any length (`"a b c …"`, repeated-word, column-scoped) via a consecutive-run
    check (`phrase_run_matches`: binary-search each term[i] at start+i) + a K-way
    docid sweep (`phrase_intersect_k`) — the index analogue of the scan's
    `fts5_phrase_starts`. **Multi-segment phrase+NEAR** (`decode_terms_multiseg`):
    phrase and NEAR now route over multi-segment indexes too — each doc's
    positions come from its owning segment, with the same tombstone +
    combined-overlap bail-to-scan as the rowid merge (verified vs `sqlite3` incl.
    deletes-present). **MATCH is now comprehensively index-routed: EVERY shape
    (term/column/phrase/boolean/prefix/NEAR) on BOTH single- and multi-segment
    indexes.** *Remaining (all PERF-ONLY — the scan handles them, results already
    correct):* ≥3-phrase `NEAR`, and dlidx/interior segment decode (D2b-3 leftover,
    only for a single term spanning ~16+ leaves).
  - **D2b-3** — *Done (multi-leaf):* `decode_term` now handles **multi-leaf term
    pagination** (terms across leaves, each with its own page-index footer) and
    **doclist spanning** (carried poslist tail + absolute first-rowid on the
    continuation leaf, via `gather_doclist_runs`/`decode_spanning_doclist`),
    including the mixed case where a spill leaf also starts the next term. A
    segment with **doclist-index (dlidx) or interior (`height > 0`) pages** —
    reached only by a single term spanning ~16+ leaves — still returns `None` so
    the caller falls back to the `_content` scan (never a truncated doclist).
    Verified by writer→decoder round-trips and against real `sqlite3` multi-leaf
    indexes at pgsz 64/80/128 (`tests/fts5_decode_multileaf.rs`). Remaining:
    dlidx/interior decode (only for very-high-frequency terms).
- **D2e-encoder — byte-identical FTS5 at large scale** (structural validity holds
  today; these only affect exact-byte parity past a few leaves, and each needs the
  fts5 writer source for the precise split heuristic): the combined
  spanning-doclist-then-paginated-terms leaf-fill boundary; doclist-index (`dli`)
  pages; segment-b-tree interior (`height > 0`) `_data` pages.
- **dbpage — the raw-page vtab** (`sqlite_dbpage`, sibling of `dbstat`).
  Done: **dbpage-1** — read (one row per page: `pgno`, `data`), byte-exact vs
  `sqlite3` on the same file (`tests/dbpage.rs`); both eponymous read-only vtabs
  (`dbstat`, `sqlite_dbpage`) also resolve a `main.`-qualifier, answer
  `PRAGMA table_info`/`table_xinfo` with their fixed column shape (incl. the
  trailing hidden columns). Any schema qualifier (`main.`/`temp.`/`<attached>.`)
  resolves the eponymous tables and — matching SQLite's hidden `schema` column
  default of `main` — reports the **main** database regardless of the qualifier
  (so `aux.dbstat`/`temp.dbstat` report main, not the qualified db). A
  `temp.`-qualified *non-eponymous* read with no temp database now reports the
  name as missing instead of panicking.
  - *Deferred:* a `WHERE schema='aux'` constraint to redirect the report to a
    non-main database (SQLite drives this through the hidden `schema` column;
    graphite has no hidden-column pushdown for it yet). Also unmatched: SQLite
    quirkily reports `main` even for an *unknown* schema qualifier
    (`nope.dbstat`), where graphite errors `unknown database nope`.
  - **dbpage-2** — write (raw page replacement). *Oracle-blocked:* the pinned
    `sqlite3 3.50.4` alt1 build was compiled without the writable-dbpage path —
    every real page write returns `read-only` (deterministically; see §6). With
    no engine to diff against, a writable `sqlite_dbpage` can't satisfy the
    differential-test rule; deferred until a writable-dbpage oracle is available.
- **D4-leftover — window UDFs + custom collations** (the latter needs a user
  variant on the `Collation` enum — invasive).
- **D5 — `sqlite3_session`** changesets/patchsets for replication.
- **D6 — async VFS for wasm** (non-blocking IndexedDB/OPFS I/O).

**Blocked by design:**
- **D7 — C-API shim** (`libsqlite3`-compatible surface). Needs `extern "C"` + raw
  pointers, incompatible with `#![forbid(unsafe_code)]`; would live in a sibling
  crate that opts out.

### Track E — Cross-database write resolution  *(essentially complete)*

**Done:** a write to an attached/`temp` database swaps that database in as the
active `main` for the whole statement, so a subquery/source reading the *original*
main must still resolve there. **E0** built the regression oracle
(`tests/cross_db_writes.rs`, deterministic in-memory ATTACH). **`INSERT … SELECT`**
and **`INSERT … VALUES ((SELECT …))`** are materialized in the original context
before the swap (`prematerialize_insert_source`). **E1/E2/E3 + E-arch-a:**
`unqualified_db` now resolves an unqualified name `main`-first and then falls back
to attached databases (SQLite's `main → temp → attached` order) — which also lets
a cross-db `UPDATE/DELETE aux.t …` subquery resolve a `main` table (the original
`main` is in the target's swapped-out attached slot), and fixes a top-level
`SELECT … FROM s` where `s` lives only in an attached database.

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

1. **B5c-1 → B5c-4, then B5b** — VDBE depth (Track B): move `IN (SELECT)`,
   correlated subqueries, compound, and windows onto the VDBE, then the
   per-cursor streaming join. Perf/coverage; parity-gated, low risk.
2. **D2b** — the real FTS5 inverted index (Track D): the one scaling gap in an
   otherwise-complete module.
3. **C7a/C7b + crash-recovery harness** — the SQLite-format journal (Track C):
   durability depth; pairs with the fault-injecting VFS.
4. **C8c, then C9a–C9d** — bounded pcache, then the concurrency story (Track C).
5. **dbpage, D5, D6** — ecosystem surfaces (Track D).

**Deferred / blocked** (documented in §4): **B1b** join reordering and **B4**
`sqlite_stat4` (diverge from / unverifiable against the stat1-only oracle);
**B1c** RIGHT/FULL inner seeks (correct via materialization); **D7** the C-API
shim (needs `unsafe`; a sibling crate); the **A-rn3-edge** ambiguous-ref case
(needs per-column-ref spans); and the FTS5 large-scale encoder sub-cases (need the
fts5 writer source). Build-specific oracle quirks we intentionally do NOT match
are recorded in §6.
