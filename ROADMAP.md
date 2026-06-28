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
trailing comma now `incomplete input`; and a `CREATE TABLE`/`VIEW`/`INDEX` whose
name is already taken now names the *existing* object's kind — a table/view
collision over a view is `view X already exists` and over an index is `there is
already an index named X` (was uniformly `table X already exists`), while
`CREATE INDEX` over a table or view is `there is already a table named X` (it
says "table" even for a view); this also closes a silent-accept hazard where a
`CREATE TABLE`/`INDEX` over an existing view/index was previously *allowed* to
create a duplicate name (a shared `table_namespace_conflict` helper, with
`IF NOT EXISTS` still a no-op); `iif(...)`/`if(...)` now desugars to a CASE
expression at parse time, so the multi-branch form `iif(c1,v1,c2,v2,…[,else])`
(SQLite 3.48+) works and untaken branches short-circuit (`iif(1,'a',<overflow>)`
returns `a` rather than erroring); and a `LIKE … ESCAPE` whose escape character
is `_` or `%` no longer treats that character as a wildcard — the escape
character is only ever the escape introducer, and a trailing escape matches just
the empty remainder (`'ab' LIKE 'a_' ESCAPE '_'` is now false); and a hex
literal wider than 64 bits is now a *recognized* token rejected with SQLite's
dedicated `hex literal too big: 0x…` (echoing the literal verbatim) rather than a
generic `unrecognized token`; and the `sqlite_source_id()` scalar is now
implemented (it returned `no such function`) — an independent reimplementation
can't carry a C build's id, so it returns graphitesql's own identifier in
SQLite's `YYYY-MM-DD HH:MM:SS <hash>` shape (the build-invariant contract —
non-NULL `text` with a timestamp-shaped leading field, and an arity error for
any argument — is what the differential test checks); and `PRAGMA
table_info`/`table_xinfo` now reports `notnull=1` for the PRIMARY KEY columns of
a **WITHOUT ROWID** table (they are implicitly NOT NULL — a table-level
composite key and an INTEGER PRIMARY KEY included), while a rowid table's PK
columns stay `notnull=0`; the *enforcement* already matched sqlite (a NULL-PK
insert was rejected), only the introspection column was out of sync; and a
strict JSON number whose magnitude overflows `f64` to ±infinity now round-trips
its verbatim source text in JSON *text* output (`json('1e1000')` → `1e1000`,
`json('[1e1000]')` → `[1e1000]`) instead of collapsing to the `9e999` infinity
literal — the serializer checked `is_infinite()` before the text-preserving
arm, so the parsed source text was dropped; the extracted SQL value is still
`f64` infinity, and a *computed* infinity (no source text) still renders as
`9e999`; and a JSON number with a **leading zero** on its integer part
(`00`, `007`, `00.5`, `-01`) is now rejected as `malformed JSON` instead of being
silently accepted as `0`/`7`/… — a lone `0` and a `0` followed by `.`/`e`
(`0.5`, `0e1`) stay valid, and `json_error_position` points at the token's
second character (`00` → 2, `[01]` → 3) to match sqlite; and a **JSON5
leading/trailing-`.` number** now renders in JSON text with just the minimal
`0` inserted to make it valid JSON (`json('1.e5')` → `1.0e5`, `.5e2` → `0.5e2`,
even the overflowing `1.e5000` → `1.0e5000`) rather than as the computed float
(`100000.0`) — the JSONB `FLOAT5` payload already stored the raw text byte-exact;
only the text serializer was canonicalizing; and a **partial-index `WHERE`
predicate** now rejects a subquery (`CREATE INDEX i ON t(a) WHERE b IN (SELECT
1)` → `subqueries prohibited in partial index WHERE clauses`) and a
non-deterministic function, in SQLite's exact precedence: a non-deterministic
*key* expression outranks a WHERE subquery (`t(random()) WHERE b IN (SELECT 1)`
→ `non-deterministic functions prohibited in index expressions`), a WHERE
subquery outranks WHERE non-determinism (even `WHERE b IN (SELECT random())`
reports the subquery), and a bare non-deterministic WHERE uses its own distinct
message (`non-deterministic functions prohibited in partial index WHERE
clauses`, *not* "index expressions"); the subquery check (reusing
`expr_has_subquery`) fires before column resolution so `WHERE zzz IN (SELECT 1)`
still reports the subquery; and a **window frame boundary** now reports SQLite's
two distinct failure classes instead of collapsing both to `near ")"`: a grammar
error for an illegal `UNBOUNDED` direction (`UNBOUNDED FOLLOWING` as a *start*
bound, `UNBOUNDED PRECEDING` as an *end* bound → `near "FOLLOWING"/"PRECEDING":
syntax error` at the direction keyword) and a semantic `unsupported frame
specification` when the start category comes after the end's (`CURRENT ROW AND
1 PRECEDING`, `1 FOLLOWING AND 1 PRECEDING`, `1 FOLLOWING AND CURRENT ROW`) —
the offset is not compared, so `2 FOLLOWING AND 1 FOLLOWING` stays a valid empty
frame; the old code built the right message text but routed it through `err()`,
which discards the string and emits a positional `near` error; and a **scalar
function used as a window function** (`abs(a) OVER ()`, `upper(a) OVER ()`, or a
two-argument `min`/`max` like `max(a,b) OVER ()`, which is scalar) is now
rejected at prepare time with `NAME() may not be used as a window function` —
covering *both* the VDBE window path (a real base table) and the tree-walker
path (a subquery source), in the projection and in `ORDER BY`; only the eleven
built-in window functions and the true aggregates (which double as window
functions, including single-arg `min`/`max`) stay legal there; an **unknown**
name carrying `OVER (…)` (`nope() OVER ()`), though, is now reported as `no such
function: nope` ahead of the window-misuse wording — SQLite resolves the name
before it classifies the `OVER`, so existence wins (a *known* scalar of any
arity keeps the misuse wording); and the `*`
wildcard **argument** is now accepted only for `count(*)` — every other call,
aggregate or scalar (`sum(*)`, `min(*)`, `group_concat(*)`, `abs(*)`,
`length(*)`), is rejected at prepare time with `wrong number of arguments to
function NAME()` in every clause position, while in a non-aggregate query the
`HAVING clause on a non-aggregate query` check still outranks the arity error;
and a **reserved keyword in table-option position** (after the column list) is
now a `near "KW"` syntax error rather than being mis-parsed — most importantly
`CREATE TABLE t(a) AS SELECT …` now errors at `AS` (the CTAS form is illegal
once a column list is present), matching SQLite, instead of pointing at the
later `SELECT`; a non-reserved word there is still `unknown table option: NAME`
and a column-less `CREATE TABLE t AS SELECT …` still parses; and the
`json_set`/`json_insert`/`json_replace` family (plus the `jsonb_*` blob
variants) now follows SQLite's varargs arity exactly — zero arguments yield
`NULL`, a lone document is a no-op that returns the document unchanged, and an
*even* argument count is the hard error `json_NAME() needs an odd number of
arguments` (the message always names the text-output `json_*` form, even for a
`jsonb_*` call), replacing graphite's old over-strict "requires a document and
(path, value) pairs" rejection; and an **unreferenced `WITH` CTE** is no longer
semantically analyzed — SQLite never validates a common table expression that the
consuming query does not reach, so a bad column or missing table inside an unused
CTE (`WITH r AS (SELECT nope) SELECT 1`) is not an error and an otherwise-infinite
unused recursive CTE is simply never run; graphite used to eagerly materialize
*every* CTE in a `WITH` clause. Reachability is computed by an exhaustive
source-name walker over the query (FROM/joins/ON, every projection and predicate
`Expr`, subqueries, `EXISTS`/`IN (SELECT …)`, `FILTER`, window specs, `ORDER
BY`/`LIMIT`) seeded from the outer query and closed transitively (a used CTE pulls
in the siblings it names), and is scope-aware — a derived subquery's own nested
`WITH` shadows an outer CTE of the same name, so `WITH a AS (SELECT bad) SELECT *
FROM (WITH a AS (SELECT 7 x) SELECT x FROM a)` binds the inner `a` and leaves the
outer one unanalyzed; duplicate `WITH` names and syntax errors still fire
regardless of use. The same unused-skip now covers **DML `WITH`** too (`WITH …
UPDATE`/`DELETE`): the reachability mask is seeded from the statement's `SET`
values, `… FROM` sources, `WHERE`/`ORDER BY` and `RETURNING` (the `update_cte_seeds`/
`delete_cte_seeds` collectors feeding the shared `cte_mask_from_seeds`), so `WITH u
AS (SELECT nope) UPDATE t SET a = 2` and an unused infinite recursive CTE on a
`DELETE` are no longer analyzed — while a *used* CTE with a fault still errors and a
duplicate `WITH` name is still rejected. And
**`likelihood(X, prob)`** is now validated at prepare time, not per row: SQLite
checks during analysis that the call has exactly two arguments and that the
probability is a floating-point literal in `0.0..=1.0` (`exprProbability`), so
both `wrong number of arguments to function likelihood()` and `second argument
to likelihood() must be a constant between 0.0 and 1.0` fire even when the query
yields no rows — over an empty or fully-filtered table — where graphite used to
defer the check to the evaluator and silently accept the bad call; the new
`reject_invalid_likelihood` walker runs in every scalar-expression position
(result list / WHERE / GROUP BY / ORDER BY / join `ON`, and nested function
arguments), leaving a `likelihood(…) OVER (…)` to the window-misuse path. And
**nested aggregates** are now rejected at prepare time: SQLite forbids an
aggregate (or window function) inside another aggregate's argument — the argument
is resolved with `NC_InAggFunc` set — so `sum(count(*))` is `misuse of aggregate
function count()` and `sum(row_number() OVER ())` is `misuse of window function
row_number()`, both fired during analysis (even over an empty/fully-filtered
table, where graphite's lazy evaluator used to accept the nesting and even
mis-evaluate it — `count(sum(a))` returned `0`). The `reject_nested_aggregate_arg`
walker scans each plain-aggregate call's arguments, names the last nested
aggregate/window in source order (matching SQLite's resolver), and runs in the
result list, `HAVING`, and `ORDER BY` — plus the VDBE window-dispatch path, which
bypasses `run_core` — while leaving scalar wrappers (`abs(count(*))`,
`max(sum(a), 1)`), an aggregate-as-window (`sum(a) OVER ()`), and subquery-level
aggregates untouched. And **aggregate arity** is now validated during analysis,
ahead of every placement/misuse check and independent of row production: SQLite
reports `wrong number of arguments to function NAME()` for `sum()`, `sum(a, a)`,
`count(1, 2)` and the like even in a clause where the aggregate would otherwise
be a misuse (`WHERE sum()`, `ORDER BY sum(a, a)`, `GROUP BY avg(a, a)`) and even
over an empty / fully-filtered table where graphite's lazy evaluator used to
reach neither check. `reject_aggregate_arity_in_select` walks every clause
(result list / WHERE / HAVING / GROUP BY / ORDER BY / join `ON`) at the top of
`run_core` — before the VDBE fast-path — applying SQLite's bound counts (`count`
0–1; `group_concat`/`string_agg`/`json_group_object` up to 2; the rest exactly
1) while leaving scalar multi-arg `min`/`max` alone. The same guard covers an
aggregate used as a **window function** (`sum(a, b) OVER ()`, `sum() OVER ()`),
which graphite previously ran silently; the eleven built-in window functions
keep their own arity. All byte-exact vs `sqlite3` 3.50.4.

And an **aggregate inside a window function's `OVER` spec** (`PARTITION BY` /
`ORDER BY`) now classifies the query as a single aggregate group, matching
SQLite: `SELECT row_number() OVER (ORDER BY sum(a)) FROM t` computes `sum(a)`
over the whole table first (one group), then runs the window over that single
row — graphite used to take the plain-window path over the raw rows and emit one
result row per input row. Routing is decided by `has_over_spec_aggregate` (a walk
of each result column's window-function nodes, testing their `PARTITION BY` /
`ORDER BY` exprs for an aggregate), which is OR-ed into the `windowed_agg`
condition in `finish_from_rows`, so the over-spec case flows through
`eval_windowed_aggregate` and composes with `GROUP BY`, `DISTINCT`, `ORDER BY`
and `LIMIT`. Crucially the over-spec aggregate is **not** counted by
`has_result_aggregate`, so it does not make a `HAVING` legal: `… OVER (ORDER BY
sum(a)) … HAVING sum(a)>0` still reports `HAVING clause on a non-aggregate query`
(even `HAVING 1`/`HAVING 0`) unless a *real* non-windowed result aggregate is
present, exactly as SQLite. Byte-exact vs `sqlite3` 3.50.4.

And an **unknown or wrong-arity scalar function in a DELETE/UPDATE `SET` value,
`WHERE` predicate, or `RETURNING` clause** is now rejected during analysis
(`no such function: NAME` / `wrong number of arguments to function NAME()`),
where graphite's lazy evaluator used to accept it over an empty or
fully-filtered table — no surviving row ever evaluated the call. The existing
`reject_unresolved_functions` dry-resolver (NULL stand-in arguments, skipping
window/aggregate/`MATCH`/FTS names) now runs in `validate_dml_refs` over the
SET/WHERE expressions and the `RETURNING` list. Column existence is still
resolved first (`RETURNING nope(zzz)` → `no such column: zzz`). The two clauses
order the function check differently, matching SQLite's two resolution regimes:
in `RETURNING` (where, as in a SELECT result, an aggregate passes name
resolution and is only flagged a misuse afterwards) the unknown-name check runs
*ahead of* the aggregate/window misuse checks, so `RETURNING nope(count(*))` is
`no such function: nope` while `RETURNING abs(count(*))` — outer name known — is
`misuse of aggregate function count()`; in `SET`/`WHERE` (aggregates forbidden
at resolve time) the misuse check stays first, so `nope(sum(a))` keeps `misuse
of aggregate function sum()`. The one residual corner is a SET/WHERE expression
that nests an unknown function *inside* an aggregate (`SET a=sum(nope(a))`):
SQLite's true innermost-first post-order reports `no such function: nope`,
whereas graphite reports the outer `misuse of aggregate function sum()` — a
pre-existing divergence left untouched (no global pass order satisfies both that
and `nope(sum(a))`). Byte-exact vs `sqlite3` 3.50.4 otherwise.

And an **`IN (SELECT …)` whose subquery width disagrees with the left-hand side**
is now rejected during analysis (`sub-select returns N columns - expected M`) on
both the SELECT and the UPDATE/DELETE paths, where graphite's per-row evaluator
used to accept the mismatch over an empty or fully-filtered table (the `IN` is
never reached, so the lazy width check never fires). The structural subquery
width comes from `row_column_affinities` (no rows needed; `*` expands to the
FROM width, a literal row counts its elements); the LHS arity is its row-value
width (`(a,b)` → 2, a bare scalar → 1). It runs right after `validate_columns_exist`
at the outermost query and after column resolution in `validate_dml_refs`, so a
missing column still wins. Crucially the arity error fires **only when the
subquery and the LHS are column-clean** — every referenced column resolves
against the subquery's own FROM plus the outer (correlation) scope, with no
compound arm or further-nested subquery the single-arm walk cannot inspect:
SQLite reports a `no such column` *before* the arity mismatch, and graphite
resolves a subquery body's columns lazily, so a dirty subquery (`a IN (SELECT
zzz, zzz)`, `zzz IN (SELECT a, a)`) is left to its existing behaviour rather than
risk reporting an arity error where a `no such column` is due. A correlated
subquery (`a IN (SELECT a, b FROM t WHERE b=t.a)`), a constant-row subquery, and
a `HAVING`/`SET` position are all covered. Residuals: a subquery with an
unresolved column (still silently accepted over an empty table, as before), a
compound (`UNION`) subquery, and a doubly-nested subquery body are conservatively
skipped. Byte-exact vs `sqlite3` 3.50.4 for the column-clean cases.

The companion case — a **multi-column subquery used where a single value is
expected** — is now rejected the same way (`sub-select returns N columns -
expected 1`), again on both the SELECT and the UPDATE/DELETE paths and again over
an empty or fully-filtered table the lazy evaluator never reaches. The walker is
context-aware: it descends into operands of every scalar expression (arithmetic,
concat, `||`, unary `-`/`NOT`, `CAST`, `LIKE`/`GLOB`, `AND`/`OR`, `IS NULL`,
function arguments, `ORDER BY`/`GROUP BY`/`HAVING`/`WHERE`, an `IN` list element)
and flags a bare `(SELECT a, b)` there, but it deliberately stops at a subquery
that is a **direct operand of a comparison operator** (`=`, `<>`, `<`, `<=`, `>`,
`>=`, `IS`, `IS NOT`) or of `BETWEEN`: SQLite reports `row value misused` in those
positions, a separate diagnostic left to its existing behaviour. The same
column-clean gate applies — the arity error fires only when the subquery body
resolves against its own FROM plus the outer scope — so a dirty subquery yields a
`no such column` rather than a wrong arity report. A correlated subquery and the
`SET`/`WHERE` DML positions are covered. Byte-exact vs `sqlite3` 3.50.4 for the
column-clean scalar contexts.

The third member of that family — **`row value misused`** — is now also raised at
prepare time. A row value `(a, b, …)` is legal only as an operand of a row
comparison, `BETWEEN`, or `IN`; used anywhere a single value is expected (a bare
result column, an arithmetic or function operand, a `WHERE`/`ORDER BY`) SQLite
rejects it, and a comparison or `BETWEEN` whose operands disagree in row width
(`a = (1,2)`, `(a,b) = (1,2,3)`, `a = (SELECT 1,2)`) is the same error. graphite
evaluated this per row — the row-value arm of the scalar evaluator and the
`operand_arity` checks on `=`/`IS`/`BETWEEN` — so over an empty or fully-filtered
table it was silently accepted. A prepare-time walker now mirrors that exactly: a
bare row value in a scalar position is rejected, and at each comparison/`BETWEEN`
the structural operand widths (a row value's length, a column-clean subquery's
output width, else 1) must match. Equal-arity row comparisons (`(a,b) = (1,2)`,
`(a,b) BETWEEN (1,2) AND (3,4)`, `(a,b) IN ((1,2),(3,4))`) stay valid, and a
nested misuse inside a valid row element (`(a, (1,2)) = (1,2)`) is still caught.
It runs on the SELECT and the UPDATE/DELETE paths after column resolution, so a
`no such column` still wins; a comparison against a subquery with an unresolved
column is conservatively skipped (left to its lazy behaviour). The closely
related **`IN(…) element has N terms - expected M`** is handled in the same pass:
every element of an `IN` list must share the left-hand side's row width, so
`(a,b) IN ((1,2),(3))`, `(a,b) IN (1,2)`, and `(a,b) IN ((1,2,3))` are rejected at
prepare time (a row element under a *scalar* LHS, `a IN ((1,2))`, is the plain
`row value misused`). This also fixes a latent wrong-answer: over a non-empty
table graphite used to accept `(a,b) IN ((1,2),(3))` and return the row matched by
the well-formed element, silently ignoring the malformed one. The one remaining
residual is a dirty operand where SQLite reports `row value misused` *before* the
missing column. Byte-exact vs `sqlite3` 3.50.4 for the column-clean cases.

A **compound (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`) with arms of unequal
column count** now picks its message exactly as SQLite does. SQLite chooses by the
shape of the *right* operand of the mismatching step: when that arm is a `VALUES`
clause it reports `all VALUES must have the same number of terms` — regardless of
the operator and even when the left arm is an ordinary `SELECT` (`SELECT 1,2 UNION
VALUES (1)`) — and otherwise it names the operator (`SELECTs to the left and right
of UNION do not have the same number of result columns`). graphite previously
emitted the operator-named message for every `VALUES`-on-the-right mismatch except
the all-`UNION ALL` case; it now matches in every operator/arm combination, while
a right-hand `SELECT` and a single multi-row `VALUES` with an internal mismatch
(`VALUES (1,2),(3)`) keep their existing (already-correct) messages. Byte-exact vs
`sqlite3` 3.50.4.

A **`FROM name(args)` whose name is not a built-in table-valued function** now
reports what SQLite reports. SQLite resolves the bare name as a table/view first:
if such an object exists, calling it with an argument list is `'<name>' is not a
function` (the schema qualifier, if any, is dropped — `main.t()` over an existing
`t` → `'t' is not a function`); otherwise it is a plain missing table with the
qualifier echoed as written (`bad.t()` → `no such table: bad.t`, never `unknown
database bad`; `nope()` → `no such table: nope`). graphite previously answered
`no such table-valued function: <name>` for every such call. The genuine built-in
TVFs (`generate_series`, `json_each`/`json_tree`, the `pragma_*` forms) are
unaffected. Byte-exact vs `sqlite3` 3.50.4.

A **window function nested inside another window function's definition** is now
rejected at prepare time, matching SQLite. A window call may not appear in another
window's arguments, its `FILTER` predicate, or its `OVER` spec's `PARTITION BY` /
`ORDER BY` — including a named `WINDOW w AS (…)` reached via `OVER w`. SQLite
reports `misuse of window function <inner>()` even over an empty table; graphite
evaluated the spec lazily and silently accepted it. The fix walks each window
definition (and every `WINDOW` clause entry) for a nested `OVER`. A window
function in a *frame-bound offset* is deliberately left alone — that is SQLite's
separate lazy "frame starting offset must be a non-negative integer" path, which
errors only once a row is produced. Byte-exact vs `sqlite3` 3.50.4.

An **ambiguous column surfaced by `*` expansion of an unaliased self-join** is now
named the way SQLite names it — by the source's *origin*. `SELECT * FROM t, t`
reports `ambiguous column name: main.t.a` (a temp table that shadows it →
`temp.t.a`, an attached `aux.t.a`), and a derived table or CTE, which has no
database, is qualified with `*` (`SELECT * FROM x, x` over a CTE `x` →
`ambiguous column name: *.x.a`). graphite previously emitted the bare
`<source>.<col>`. Explicit references (`SELECT a` / `SELECT x.a`) keep their
unprefixed spelling, unchanged. Byte-exact vs `sqlite3` 3.50.4.

**`RENAME COLUMN` now rewrites a dependent view's references that live inside
expression subqueries.** SQLite rewrites every reference to a renamed column
wherever a view names it — including within a scalar `(SELECT …)`, `EXISTS`, or
`x IN (SELECT …)`. graphite used to abandon the *entire* view rewrite the moment
the body held any subquery, leaving stale references so the view became
unqueryable (`no such column: a`). A single-source view whose subqueries
reference only the renamed table is now rewritten in full — bare and
`<alias>.`-qualified references, at every nesting level (the validator walks the
top `SELECT` and every nested expression subquery, accumulating each alias bound
to the table). It still conservatively leaves the view untouched (a known gap,
never a wrong rewrite) when a token rewrite can't be proven safe: a subquery
touching another table, a derived table in a `FROM`, or a result-column alias
colliding with the renamed name. Byte-exact vs `sqlite3` 3.50.4
(`tests/view_rename_column_subquery.rs`).

**`RENAME COLUMN` now rewrites a dependent trigger's references that live inside
`WHEN`/body expression subqueries.** The exact trigger analog of the view fix
above: a trigger attached to the renamed table, whose `WHEN` guard and body
statements (`UPDATE`/`DELETE`/`INSERT … SELECT` over the same table) nest a
scalar `(SELECT …)`, `EXISTS`, or `x IN (SELECT …)` referencing only that table,
used to bail the moment any subquery appeared — leaving stale references so the
trigger broke (`no such column: a`) the next time it fired. It is now rewritten
in full: bare, `NEW.`/`OLD.`, and `<alias>.`-qualified references at every
nesting level (reusing the view validator to accumulate each nested-subquery
`FROM` alias). It still conservatively leaves the trigger untouched (a known gap,
never a wrong rewrite) when a token rewrite can't be proven safe: a body
statement writing another table, a subquery touching another table, or a derived
table in a `FROM`. Byte-exact vs `sqlite3` 3.50.4
(`tests/trigger_rename_column_subquery.rs`).

**`json_quote(X)` now renders a JSONB blob as its JSON text.** SQLite accepts a
BLOB argument to `json_quote` when it decodes as JSONB and emits the
corresponding JSON (so `jsonb_*` results compose: `json_quote(jsonb('[1,2]'))` →
`[1,2]`, and the 1-byte JSONB scalars `x'00'`/`x'01'` → `null`/`true`), raising
`JSON cannot hold BLOB values` only for a blob that is *not* valid JSONB.
graphite rejected every blob unconditionally. It now routes the blob through its
existing JSONB decoder (the one behind `jsonb()`/`jsonb_extract`): valid JSONB
renders, invalid still errors. Byte-exact vs `sqlite3` 3.50.4
(`tests/json_quote_jsonb.rs`).

**`CREATE UNIQUE INDEX` over a table that already holds duplicate keys now
fails** with `UNIQUE constraint failed: …`, as SQLite does. graphite built the
index silently — the trailing rowid in each encoded index key made every entry
distinct, so the btree insert never saw the clash — leaving a "unique" index
that did not enforce uniqueness yet still passed `PRAGMA integrity_check`: a
silent-corruption bug. `exec_create_index` now pre-checks the indexed key tuples
before writing the btree (NULLs distinct, collation-aware, the partial-index
`WHERE` predicate applied), on both the rowid and `WITHOUT ROWID` paths, and
raises `t.col[, …]` for a column index or `index '<name>'` for an expression
index. Byte-exact vs `sqlite3` 3.50.4 (`tests/create_unique_index_duplicate.rs`).

**CTEs in one `WITH` clause now see each other — forward references work, and
true cycles are rejected.** SQLite makes every CTE in a `WITH` mutually visible
(it expands them on demand from the outer query), so a CTE may reference one
declared *after* it (`WITH a AS (SELECT * FROM b), b AS (SELECT 9) …`), and a
genuine cycle is `circular reference: <name>`, naming the CTE the outer query
enters through (`… FROM a` over an `a`<->`b` cycle reports `a`; `… FROM b`
reports `b`). graphite exposed only the CTEs declared *before* each one, so a
forward reference fell through to the schema as `no such table: <name>`.
`push_ctes` now takes the consuming statement's source names (the entry order),
builds the sibling dependency graph, detects cycles in entry order, and
materializes dependencies before dependents — declaration order is preserved for
independent CTEs, so every backward-only (pre-existing) query is unaffected. A
direct self-reference is still recursion, not a cycle. Byte-exact vs `sqlite3`
3.50.4 (`tests/cte_forward_reference.rs`).

**A duplicate PRIMARY KEY now outranks a generated-column PK error when the
first PK is non-generated.** SQLite processes `PRIMARY KEY` declarations
sequentially (`sqlite3AddPrimaryKey`), so the *first* declared PK decides which
error a `CREATE TABLE` reports: a generated first PK yields `generated columns
cannot be part of the PRIMARY KEY`, but a non-generated first PK followed by any
second PK yields `table "X" has more than one primary key`. graphite fired the
generated-column error eagerly from its per-column loop, so
`CREATE TABLE t(a PRIMARY KEY, b AS (a) PRIMARY KEY)` reported the generated
error where SQLite says "more than one primary key". The generated-PK check is
now gated on whether the first PK declaration (column-level PKs precede
table-level ones in source order) is itself generated. Byte-exact vs `sqlite3`
3.50.4 (`tests/pk_generated_precedence.rs`).

**`REINDEX schema.name` validates its database qualifier** ahead of the object
lookup. SQLite rejects an unknown database with `unknown database <name>` (as it
does for `VACUUM`/`ATTACH`); graphite parsed the qualifier but threw it away,
reporting the generic `unable to identify the object to be reindexed` for a bad
database. The `Reindex` AST now carries `schema`/`name`, the executor resolves
the qualifier first, and two parity points fall out: a *known* database with an
unidentifiable object still says `unable to identify …` (`REINDEX main.nope`),
and a collation may be reindexed only *unqualified* — `REINDEX nocase` is a
no-op but `REINDEX main.nocase` is `unable to identify …` (a collation is not a
per-database object). Byte-exact vs `sqlite3` 3.50.4.

**Structural DDL on an internal `sqlite_` table is now rejected** rather than
silently performed. SQLite forbids `ALTER`, `DROP TABLE`, and `CREATE INDEX` on
any table whose name begins with `sqlite_` — the schema catalog
(`sqlite_master` / `sqlite_schema`) and the bookkeeping tables (`sqlite_sequence`,
…) — with `table <name> may not be {altered,dropped,indexed}`. graphite reported
`no such table` for the catalog (which it doesn't expose as a droppable table)
and — the real hazard — *actually renamed/dropped/indexed* `sqlite_sequence`.
A new `reject_internal_table_ddl` guards all three DDL entry points: it
normalises the catalog aliases to `sqlite_master` (and the temp catalog to
`sqlite_temp_master`) and otherwise uses the table's stored name, fires only once
the target exists (a missing `sqlite_stat1` is still `no such table`, and
`IF EXISTS` still suppresses a genuinely-absent one), and outranks `IF EXISTS`
for a catalog/internal table that does exist. Direct DML (`INSERT`/`DELETE`) on
`sqlite_sequence` stays allowed, matching SQLite. Byte-exact vs `sqlite3` 3.50.4.

A **self-referential CTE with no leading anchor** now reports SQLite's
`circular reference: <name>` rather than graphite's internal "recursive CTE must
have a non-recursive anchor and a recursive term". When the recursive table
appears already in the *first* arm — a plain self-`FROM` (`WITH c AS (SELECT *
FROM c) …`, with or without `RECURSIVE`), a self-join, a recursive arm placed
before the anchor in a compound, or every arm recursive — there is nothing to
seed the recursion, and SQLite rejects it by name. graphite now splits that case
out from the genuinely-unsupported "no recursive term anywhere" case and emits
the matching message. Valid recursive CTEs (anchor first) are unaffected.
Byte-exact vs `sqlite3` 3.50.4. (The harder *mutual*-recursion forward-reference
case — `WITH a AS (SELECT * FROM b), b AS (SELECT * FROM a) …`, where SQLite says
`circular reference: a` and graphite still says `no such table: b` — is a CTE
scoping issue tracked separately.)

The **`PRAGMA journal_mode = <mode>` setter now reports its result row**, as
SQLite does — it echoes the resulting journal mode (`wal` after a successful
switch, `memory` for an in-memory database that cannot change it, or the
unchanged current mode when the requested one is invalid). The shell had routed
every `PRAGMA … = …` setter through the row-discarding write path, so the setter
form printed nothing; it now reads the mode back through the getter (preserving
any `schema.` qualifier) and prints it. Genuinely silent setters
(`foreign_keys = ON`, `user_version = 5`) stay silent. Byte-exact vs `sqlite3`
3.50.4, on both in-memory and file databases.

A known remaining gap, newly tracked: **eponymous table-valued functions used as
a bare table name** (`FROM generate_series`, `FROM json_each`, `FROM
pragma_table_info`, without a parenthesised argument list). SQLite exposes these
as eponymous virtual tables whose hidden arguments can be supplied through the
`WHERE` clause (`FROM generate_series WHERE start = 1 AND stop = 3` returns rows),
and an unconstrained reference either yields no rows (`json_each`, the pragma
functions) or the function-specific "first argument … missing or unusable" error
(`generate_series`). graphite only recognises these when called with parentheses,
so a bare reference is `no such table: <name>`. Closing this means treating the
eponymous TVFs as real `FROM` sources with `WHERE`-driven hidden-column binding —
a feature, not a message tweak, so it is deferred rather than half-aligned.

And **`CREATE TABLE` validation ordering** now mirrors the order in which SQLite
builds a schema, so a statement with several faults reports the same one SQLite
does. The per-column "add column" checks run first, left to right — a duplicate
name, then the structural generated-column rules (a second `AS`, a `DEFAULT`, or
membership in the PRIMARY KEY), then the `COLLATE` sequence — *ahead of* the
end-of-table checks, including STRICT's missing/unknown-datatype check. The first
end-of-table check is "must have at least one non-generated column", which
therefore outranks an unknown table option, a prohibited subquery/aggregate, and
any `no such column` resolution of a CHECK / generated expression. graphite used
to resolve a generated column's expression before noticing the table was
all-generated (so `CREATE TABLE t(a AS (b))` wrongly said `no such column: b`
instead of `must have at least one non-generated column`) and ran the
duplicate-name / `COLLATE` checks after the STRICT datatype check; both are fixed,
byte-exact vs `sqlite3` 3.50.4 across ~35 multi-fault permutations. The same
"must have at least one non-generated column" rule is now also re-applied after
an **`ALTER TABLE … DROP COLUMN`**: dropping the last ordinary column (leaving
only generated columns) is rejected with `error in table T after drop column:
must have at least one non-generated column`, ahead of the generated-expression
re-resolution — so `DROP COLUMN a` from `t(a, b AS (a+1))` reports that rule
rather than `no such column: a`, and `t(a, b AS (1))` (whose generated expression
no longer mentions the dropped column) is now rejected instead of silently
building an all-generated table.

A **generated column may reference another column declared later in the table**,
including another generated column. SQLite resolves these forward references
topologically (`b AS (c+1), c AS (a+1)` yields the same values whichever order
the two generated columns appear in); graphite used to evaluate generated columns
strictly in declaration order, so a forward-referenced generated column was still
`NULL` when its dependant was computed. Both the read path (VIRTUAL columns) and
the write path (STORED columns, which must be byte-exact on disk for
`integrity_check`) now evaluate generated columns in dependency order via a
post-order DFS. A **cycle among the generated columns** is rejected at CREATE —
before any row is inserted, exactly as SQLite does — with `generated column loop
on "X"`, where `X` is the column whose expression *closes* the cycle (the
generated columns being visited in declaration order); graphite used to accept
the cyclic table and silently emit `NULL`s. Byte-exact vs `sqlite3` 3.50.4 across
self-loops, 2- and 3-cycles in both declaration orders, and cycle references
buried inside larger expressions.

A **trigger body that targets a missing table** is now reported with the same
schema qualifier SQLite uses. SQLite compiles a trigger program in the trigger's
own database, so an unqualified DML target (`INSERT`/`UPDATE`/`DELETE`) that
resolves to nothing is named schema-qualified — a `main` trigger says `no such
table: main.nope` — whereas a *temp* trigger, whose names resolve across all
schemas, keeps the bare `no such table: nope`. graphite previously reported the
bare name in both cases; the fix tags each fired trigger with its origin catalog
(swap-aware, so a temp trigger on a main table is still labelled `temp`) and
qualifies the error only for non-temp triggers. Byte-exact vs `sqlite3` 3.50.4.

A **schema-qualified DML target inside a trigger body** is now rejected at
`CREATE TRIGGER` time. SQLite compiles a trigger program in the trigger's own
database, so a `schema.table` qualifier on an `INSERT`/`UPDATE`/`DELETE` *target*
within the body is forbidden and errors with `qualified table names are not
allowed on INSERT, UPDATE, and DELETE statements within triggers` — for `main`
and `temp` triggers and AFTER and INSTEAD OF alike, whether or not the qualified
schema/table exists. A qualifier on a table in a body *subquery* (`SELECT … FROM
main.u`) stays legal — only the DML target is checked. graphite used to accept
the qualified target silently. The check sits after the trigger's own
missing-target / timing-mismatch / duplicate-name errors so those still win.
Byte-exact vs `sqlite3` 3.50.4.

A **`CREATE TRIGGER` whose target is a system table** is now rejected with
SQLite's `cannot create trigger on system table`. The schema tables
(`sqlite_master` / `sqlite_schema` / `sqlite_temp_master`) always count as system
tables; any other `sqlite_`-prefixed table counts only once it physically exists
(so `sqlite_sequence` is rejectable only after an `AUTOINCREMENT` table brings it
into being, and an absent `sqlite_foo` still falls through to `no such table`).
The check outranks the missing-table, timing-mismatch and body-qualifier errors
but is itself outranked by the duplicate-name error. graphite used to report `no
such table` for the schema tables and even *succeeded* on an existing
`sqlite_sequence`. Byte-exact vs `sqlite3` 3.50.4.

An **aggregate or window function in a `CREATE INDEX`** key expression or
partial-index `WHERE` clause is now rejected at prepare time with SQLite's
`misuse of aggregate function NAME()` / `misuse of window function NAME()`.
graphite used to build the index silently. The whole `CREATE INDEX` validation
was reordered to SQLite's precedence: every key expression is resolved fully,
left to right, *before* the `WHERE` predicate, so a key fault outranks a `WHERE`
fault. Within a key the order is unknown column → unknown function →
non-determinism → aggregate → window → dotted reference → unknown collation;
the `WHERE` clause is then checked as subquery → unknown column → unknown
function → non-determinism → aggregate → window. This also fixes a pre-existing
ordering bug where an unknown *key* column was reported only after a `WHERE`
subquery / non-deterministic function. Byte-exact vs `sqlite3` 3.50.4 across
~25 permutations.

An **`ORDER BY` on an UPDATE/DELETE with no `LIMIT`** is now rejected at prepare
time. SQLite only allows the ordering as a companion to the update/delete-`LIMIT`
extension — the order decides *which* rows the cap keeps — so a bare `ORDER BY`
is meaningless and errors with `ORDER BY without LIMIT on UPDATE` / `... on
DELETE`. graphite used to accept it silently and update/delete nothing. The guard
sits after the target's existence / view / vtab checks but ahead of column
resolution, matching SQLite's diagnostic precedence: a missing table and a
modify-a-view error still win, but a bogus `ORDER BY` or `SET` column reports the
limit error first. Byte-exact vs `sqlite3` 3.50.4 across ~17 permutations.

The companion family — **the whole trigger-step grammar inside a `BEGIN … END`
body** — is now policed to match SQLite. SQLite admits only
`SELECT`/`VALUES`/`INSERT`/`REPLACE`/`UPDATE`/`DELETE`/`WITH`-then-`SELECT|VALUES`
steps and rejects everything else at prepare time; graphite used to parse a wider
grammar and silently accept or no-op several constructs. Now matched byte-for-byte:
a **disallowed leading keyword** (`PRAGMA`, `VACUUM`, `CREATE`, `DROP`, `ALTER`,
`EXPLAIN`, `SAVEPOINT`, `BEGIN`, `COMMIT`, `ATTACH`, …) → `near "KW": syntax
error`; a **`WITH`-prefixed body DML** (`WITH … INSERT|UPDATE|DELETE|REPLACE`) →
`near "<dmlkw>": syntax error` (the DML keyword, not `WITH`); a **schema-qualified
DML target** (`UPDATE main.t …`) → `qualified table names are not allowed on
INSERT, UPDATE, and DELETE statements within triggers`; **`UPDATE`/`DELETE …
RETURNING`** → `near "RETURNING": syntax error`; **`INSERT`/`REPLACE … RETURNING`**
→ `cannot use RETURNING in a trigger`; and the prior **`UPDATE/DELETE … ORDER
BY`/`LIMIT`** → `near "ORDER"`/`near "LIMIT": syntax error` (the row-limit
extension SQLite omits from the trigger grammar entirely).

The mechanism is *record-not-throw*: the parser carries an `in_trigger_body` flag
(set only while consuming a `CREATE TRIGGER … BEGIN … END` body, saved/restored so
a nested CREATE-TRIGGER-in-body is safe) and the body-step parsers record the
would-be `near "…"` / semantic message — **first in source order wins** — onto a
`body_error: Option<String>` field of the `CreateTrigger` node rather than throwing
on the spot. The executor surfaces it only **after** resolving the trigger target:
because SQLite parses the body steps only once the target resolves, a
duplicate-name (`trigger tr already exists`), missing-table (`no such table:
main.nope`), system-table (`cannot create trigger on system table`), or
timing-mismatch (`cannot create BEFORE trigger on view: v`) error outranks every
body error. `WITH`-then-`SELECT|VALUES`, a parenthesised `SELECT`, a qualified
table inside a body *subquery*, and a top-level `RETURNING`/`ORDER BY`/`LIMIT` all
stay legal. A leading-keyword guard in the CLI's `has_returning()` keeps a
`CREATE TRIGGER` whose body contains `RETURNING` from being misrouted to the
RETURNING execution path (so the recorded body message wins). Byte-exact vs
`sqlite3` 3.50.4 across the disallowed-keyword set, the `WITH`-DML / qualified /
RETURNING / row-limit cases, the full target-resolution precedence matrix, and
source-order first-wins in both directions.

A bare **`SELECT` step in a trigger body is now resolved when the trigger fires**.
Such a step is side-effect-free *except* for a `RAISE(…)`, but SQLite still
compiles (resolves) it when the firing statement is prepared — so a FROM-less body
`SELECT` naming a missing column, unknown function, wrong arity, or a bad
`NEW`/`OLD` column raises that error the moment the trigger fires. graphite ran a
RAISE-only path (`run_trigger_select`) that skipped every non-`RAISE` projection,
so it silently no-op'd the `SELECT`. It now evaluates each FROM-less projection
up front (value discarded) to force name/function/arity resolution, *before* the
`WHERE` filter and any sibling `RAISE` — matching SQLite, where a missing column
outranks a `RAISE` in the same step and the resulting error rolls the whole firing
statement back (any earlier body `INSERT` is undone). `RAISE`-bearing projections
keep their dedicated path (`eval` has no `RAISE` support); aggregate / window
calls are skipped so a FROM-less `SELECT count(*)` stays the valid `0` rather than
a spurious `misuse of aggregate`. Byte-exact vs `sqlite3` 3.50.4. *(Residuals, both
inherent to evaluating rather than statically resolving: a **FROM-bearing** body
`SELECT` — `SELECT * FROM nope` — and a name inside an **un-taken `CASE` branch**
are still resolved lazily; a full static resolver over the trigger row scope would
close them.)*

A misplaced **`ORDER BY` / `LIMIT` before a compound operator** now reports the
same message SQLite does. These clauses bind to the *whole* compound, so
`SELECT … ORDER BY … UNION SELECT …` is rejected with `ORDER BY clause should
come after UNION not before` (or `LIMIT clause …` when only a `LIMIT` is
misplaced — `ORDER BY` wins when both are present), with the operator spelled out
(`UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`). graphite previously left the
operator unconsumed and emitted a bare `near "UNION": syntax error`. The check
lives in the compound-select parser, right after the trailing-clause parse, and
leaves the legal trailing form (`… UNION … ORDER BY … LIMIT …`) untouched.
Byte-exact vs `sqlite3` 3.50.4 across ~13 permutations. *(Residual: when compound
arms differ in width and the **right** arm is a `VALUES`, SQLite says `all VALUES
must have the same number of terms` rather than the generic `SELECTs to the left
and right …`; graphite desugars `VALUES` into a `SELECT` with no surviving marker,
so distinguishing it needs an `is_values` flag threaded through the `Select` AST —
deferred as a cosmetic message on a malformed-query edge.)*

A **built-in window-only function used without `OVER`** (`row_number()`,
`rank()`, `lag(a)`, … called as a plain scalar) is now rejected at prepare time
with `misuse of window function NAME()`. graphite's per-row evaluator already
reported this when a row was reached, but over an empty or fully filtered table
the call was never evaluated, so the error was silently skipped (the same
eager-vs-lazy gap closed earlier for column resolution and aggregate misuse). A
wrong argument count is diagnosed first (`ntile()` → `wrong number of arguments
to function ntile()`), matching SQLite's order. The check walks every scalar
position (result columns, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, join `ON`),
stopping at subquery boundaries. In `HAVING` the misuse is reported only once the
clause is legal — a non-aggregate query emits `HAVING clause on a non-aggregate
query` first — which also fixed a pre-existing divergence where an `OVER` window
in a non-aggregate query's `HAVING` reported the window misuse ahead of the
HAVING-context error. Byte-exact vs `sqlite3` 3.50.4 across ~30 permutations.

An **aggregate or window function in a `DELETE`/`UPDATE` `RETURNING` clause** is
now rejected at prepare time with `misuse of aggregate function NAME()` /
`misuse of window function NAME()`. A `RETURNING` clause projects one row per
modified row, so it is never an aggregate query and offers no window context; a
window-only builtin called there without `OVER` is the same misuse. graphite
evaluated `RETURNING` lazily and so silently produced no rows over an empty
(fully deleted/updated) table — the same eager-vs-lazy gap, here closed by
extending the existing `validate_dml_refs` walk (which already rejected these in
`WHERE`/`SET`) to the `RETURNING` exprs. A missing column still wins (`no such
column: zzz`), and `INSERT … RETURNING` is — as in SQLite — *not* subject to this
(it is validated on a separate path). Byte-exact vs `sqlite3` 3.50.4 across ~24
permutations.

**`min()` / `max()` with zero arguments** is now rejected at prepare time with
`wrong number of arguments to function NAME()`. These two functions are an
aggregate at one argument and a scalar at two or more, so a zero-argument call
matches neither signature; graphite's aggregate-arity validator treated them as
aggregates only at one argument and so skipped the bare call, leaving it to be
caught lazily — i.e. never, over an empty or fully filtered table, where it
silently produced no rows. The check now special-cases the zero-arg form ahead of
the aggregate gate, covering result columns, `WHERE`, `GROUP BY`, `HAVING`, and
`ORDER BY`. The windowed form (`max() OVER ()`) is left to its own distinct error
(`max() may not be used as a window function`). Byte-exact vs `sqlite3` 3.50.4
across ~19 permutations.

**An aggregate or window function inside a `FILTER (WHERE …)` predicate** is now
rejected at prepare time. The filter is an ordinary boolean expression that may
not itself aggregate, so `count(*) FILTER (WHERE sum(a)>0)` is `misuse of
aggregate function sum()` and `… FILTER (WHERE rank()>0)` is `misuse of window
function rank()`. graphite evaluated the predicate lazily per row and so silently
returned a value over an empty (or fully filtered) table; a new
`reject_aggregate_in_filter` walk descends into each aggregate's filter from every
clause (result columns, `WHERE`, `HAVING`, `GROUP BY`, `ORDER BY`, join `ON`, and
the window-dispatch path). A missing column inside the filter still wins (`no such
column: zzz`), and the windowed carrier `count(*) FILTER (…) OVER ()` — which
SQLite accepts — is left untouched. Byte-exact vs `sqlite3` 3.50.4 across ~15
permutations.

**Unknown or wrong-arity scalar functions are now resolved at prepare time.**
SQLite rejects an unknown name (`no such function: NAME`) and a wrong argument
count (`wrong number of arguments to function NAME()`) before the query runs;
graphite noticed only at row-evaluation time — i.e. never over an empty (or fully
filtered) table, where it silently returned no rows — and the experimental VDBE
fast path compiled a *known* call without re-checking its arity, so even
`SELECT abs(a,b) FROM t` over an empty table slipped through. A new
`reject_unresolved_functions_in_select` runs the existing
`reject_unresolved_functions` dry-resolve (the same one CHECK/generated-column
expressions already used) across every clause, on both the VDBE-success path
(where a success guarantees the columns resolved, so a function fault is the sole
remaining error) and the tree-walker path. A missing column still wins (`no such
column: c`). Byte-exact vs `sqlite3` 3.50.4 across ~24 permutations; functions
whose arity differs only under a locally ICU-enabled sqlite (`lower`/`upper`/
`substr` take an optional locale argument there) are out of scope, matching the
ASCII-only CI oracle.

**The shared expression walker now descends into row values and `COLLATE`.**
`window::visit` (and its `replace_in` twin) — the walker behind the prepare-time
resolution/misuse checks — stopped at a row value `(a, b, …)` and `expr COLLATE
name`, so anything nested under those was invisible: `(abs(a,b),1)=(1,2)` and
`nope(a) COLLATE nocase` slipped past the function-resolution check, and
`(count(*),1)=(1,1)` / `count(*) COLLATE nocase` / `(row_number() OVER (),1)=…`
escaped the aggregate/window misuse checks. The same two-node blind spot in the
aggregate classifier (`expr_contains_agg`) and substitutor
(`substitute_aggregates`) meant a *valid* aggregate wrapped in `COLLATE` —
`sum(a) COLLATE binary` — was misclassified as a scalar call and wrongly rejected
as a misuse; both now descend through `COLLATE` (the classifier deliberately does
*not* treat a row value as an aggregate context, since an aggregate in a row value
in result/`HAVING` position is `row value misused` in SQLite regardless). Completing
the descent is one general fix that closes all of these. Byte-exact vs `sqlite3`
3.50.4. A bare row value as a scalar result column (`SELECT (sum(a),1) FROM t` →
`row value misused`) is a separate row-value-context check, still open.

**`LIMIT`/`OFFSET` is resolved in an empty column scope.** SQLite evaluates a
`LIMIT`/`OFFSET` expression with *no table columns in scope* — not even a
correlated outer column — and that resolution runs ahead of every other check in
the statement. So any column reference inside one is `no such column: NAME`, and
that wins over an aggregate misuse (`LIMIT sum(a)` → `no such column: a`, not
`misuse of aggregate function sum()`), over an unknown / wrong-arity function
(`LIMIT nope(a)` → `no such column: a`), and over a result-column / `WHERE`
resolution error elsewhere; only a `LIMIT` with no column argument keeps its own
error (`LIMIT count(*)` → `misuse`, `LIMIT nope()` → `no such function`). graphite
resolved the limit lazily during evaluation, so it saw the aggregate's misuse (or
a correlated outer column) before the missing one and silently accepted some
statements SQLite rejects at prepare time. A single early check in `run_core`
(`reject_scopeless_column_ref` over the query level's own `LIMIT`/`OFFSET`, not
descending into a subquery, which has its own scope) closes the top-level *and*
correlated-subquery cases. Byte-exact vs `sqlite3` 3.50.4.

**An `UPDATE`/`DELETE` target table may now carry an `AS` alias** (`UPDATE t AS x
SET b = x.a … WHERE x.a = 1`, `DELETE FROM t AS x WHERE x.a = 1`). SQLite lets a
single-table mutation rename its target with the `AS` keyword; the alias then
becomes the *sole* qualifier for the target's columns in `SET`/`WHERE`/`ORDER BY`
— the real table name no longer resolves there (`UPDATE t AS x SET b = t.a` →
`no such column: t.a`). graphite previously had no `alias` field on the `Update`/
`Delete` AST and silently dropped the alias, so `x.a` failed to resolve. The fix
parses an optional `AS <ident>` after the target name (a bare `UPDATE t x …` is a
syntax error, as in SQLite) and, at exec time, AST-rewrites the alias qualifier to
the real table name across `SET`/`WHERE`/`ORDER BY` and any correlated subqueries,
descending but stopping at a subquery that re-binds the alias name (so `… SET b =
(SELECT max(b) FROM t AS x)` shadows correctly). A column the alias can't resolve
is rejected eagerly at prepare time with the alias qualifier preserved (`x.nope` →
`no such column: x.nope`, over a populated *or* empty table), and the `rowid`
family resolves through the alias. `RETURNING` is the documented quirk: it still
resolves against the **real** table name, not the alias (`… RETURNING t.a` works,
`RETURNING x.a` is `no such column: x.a`). Bundled with it, a `TABLE.*` wildcard in
`RETURNING` is now rejected at parse time (`RETURNING may not use "TABLE.*"
wildcards`) for `INSERT`/`UPDATE`/`DELETE` alike — a bare `*` is still fine —
closing a pre-existing gap where graphite expanded `t.*` that SQLite rejects.
Byte-exact vs `sqlite3` 3.50.4 across ~40 permutations (`tests/update_delete_alias.rs`).
*(Residual: for a **view**/virtual-table target the column set isn't known on the
best-effort path, so a missing aliased column there is reported against the real
name rather than the alias — a minor message-only divergence on an already-erroring
query.)*

A **window frame offset may be any constant expression**, not just an integer
literal. SQLite accepts `ROWS (1+1) PRECEDING`, `RANGE (2.5-0.5) FOLLOWING`,
`CAST(2 AS INT)`, `2 COLLATE NOCASE`, `1<<1`, and so on — anything built from
literals and operators (including `CAST`/`COLLATE`/parentheses) — and validates
the offset at *run time*, once the partition has a row: it folds the expression,
applies **numeric affinity** (so a text `'2'`/`'2.0'`/`' 2 '` coerces and works,
but `'2x'`, a blob, or `NULL` fail), then requires a non-negative integer for
`ROWS`/`GROUPS` or a non-negative number for `RANGE`. A row-dependent offset (a
column, a function call like `abs(2)`, or a subquery) is rejected with the same
`frame {starting,ending} offset must be a non-negative {integer,number}` message,
and the whole check is **deferred over an empty input** (no rows ⇒ no
evaluation ⇒ no error), exactly as SQLite's stepping-time validation. graphite
used to reject every non-integer-literal offset at *parse* time with a bogus
`near "PRECEDING": syntax error`. The parser now keeps the offset as a full
`Expr` (`FrameBound::Preceding/Following(Box<Expr>)`); `resolve_frame` evaluates
and validates each offset once per non-empty partition into a numeric
`ResolvedFrame`, which the `row_bound`/`group_bound`/`range_value_bound` helpers
consume. Byte-exact vs `sqlite3` 3.50.4 (`tests/window_frame_offset_expr.rs`),
covering the value cases, the affinity edges, the run-time rejections, and the
empty-input deferral.

An **`ORDER BY` expression may reference a SELECT-output alias**, not just a bare
alias term. SQLite resolves `SELECT a AS x FROM t ORDER BY x+0` (and `-x`,
`abs(x)`, `x+y`, `x||c`, `CASE WHEN x>1 …`) by binding the name to the *computed
output value*, with a real input column of the same name taking precedence
(`SELECT a AS b … ORDER BY b+0` still orders by the column `b`). The alias is in
scope for aggregate (`SELECT count(*) AS n … ORDER BY n+0`), window
(`ORDER BY row_number() OVER (…)+0`), `DISTINCT`, and grouped
(`ORDER BY n*10+x`) queries alike. The VDBE path already handled this; the
fix is in the tree-walker fallback (`eval_simple`/`eval_aggregated`), which used
to resolve only a *whole-term* alias/ordinal (`resolve_order_index`) and raised
`no such column` the moment the alias appeared inside a larger expression. Each
now evaluates a general `ORDER BY` term against a context augmented with the
output columns (base columns first, so they win ties), mirroring the existing
`HAVING` augmentation. Byte-exact vs `sqlite3` 3.50.4
(`tests/order_by_alias_expr.rs`).

The **`ALL`/`DISTINCT` quantifier keywords** are now parsed exactly where SQLite
allows them and rejected exactly where it doesn't. They are valid only directly
after `SELECT` or as the first token inside an aggregate call; `ALL` is the
default, so `count(ALL a)` counts every non-null `a` (graphite previously failed
to accept it — `count(ALL a)` errored `near "a"`) and now parses like
`count(a)`. In any other expression-operand position the keyword is a reserved
syntax error pointing at *itself* (`1 > ALL (SELECT 1)` → `near "ALL"`,
`1 > DISTINCT (…)` → `near "DISTINCT"`); graphite used to mis-parse it as a
column/function so the caret landed on the following `SELECT`/operand. Only one
quantifier is allowed, so a second one falls through to the same operand-position
rejection (`count(ALL DISTINCT a)` → `near "DISTINCT"`,
`count(DISTINCT ALL a)` → `near "ALL"`). The fix is two targeted parser arms: the
aggregate-call path eats an optional `ALL` after the `DISTINCT` check, and
`primary()` rejects a bare `all`/`distinct` operand. Byte-exact vs `sqlite3`
3.50.4 (`tests/all_distinct_operand_syntax.rs`).

The **"ambiguous column name" message now echoes the reference as written.**
SQLite names the offending column with the exact qualifier the user typed — a
bare `column`, a `table.column`, or a three-part `schema.table.column` — so
`SELECT t.a FROM t, t` reports `ambiguous column name: t.a` and
`SELECT main.t.a FROM t, t` reports `main.t.a`. graphite used to strip the
qualifier and always print the bare `a`. The fix reconstructs the written form
from the `Expr::Column { schema, table, column }` fields in
`validate_unambiguous_columns`. Byte-exact vs `sqlite3` 3.50.4
(`tests/ambiguous_column_qualifier.rs`). One sub-case is deferred: a `*`/`t.*`
wildcard over an *unaliased self-join* is ambiguous on the database-qualified
expansion (`SELECT * FROM t, t` → `main.t.a`, `temp.t.a` for a temp table),
which needs the owning database name threaded onto `ColumnInfo` (it currently
carries only the table/alias); graphite reports `t.a` there for now.

A **leading `WITH` now rides on every `INSERT` source, not just
`INSERT … SELECT`.** SQLite extends `WITH` to all DML forms, so
`WITH c AS (…) INSERT INTO t VALUES(…)` / `DEFAULT VALUES` are accepted, with the
CTEs in scope for any subquery inside the `VALUES` list
(`WITH c(n) AS (VALUES(5)) INSERT INTO t VALUES((SELECT n FROM c))` inserts `5`).
graphite previously parsed only `WITH … INSERT … SELECT` and rejected the
`VALUES` forms with a spurious `near ";": syntax error` (and answered
`incomplete input` instead of `no such table` when the CTE name collided with the
insert target). The CTEs ride on a new `Insert::ctes` field (parsed in
`with_prefixed`, pushed/popped around the insert by an `exec_insert` wrapper that
mirrors `exec_delete`, and also pushed inside `prematerialize_insert_source` so
the cross-database source materializes in the pre-swap context). A CTE never
shadows the insert *target* — `INSERT INTO c …` stays `no such table: c`.
Byte-exact vs `sqlite3` 3.50.4 (`tests/with_insert.rs`, plus the parse-acceptance
guard in `tests/parser_surface.rs`).

**`PRAGMA case_sensitive_like` is now honored**, not silently ignored. With the
pragma `ON`, the `LIKE` operator and the two-argument `like()` function compare
ASCII letters case-sensitively (`'A' LIKE 'a'` → `0`), while the `_`/`%`
wildcards, `ESCAPE`, and non-ASCII letters behave as before — only ASCII folding
is switched off, matching SQLite's built-in `LIKE`. `GLOB` stays case-sensitive
regardless, and the get form (`PRAGMA case_sensitive_like`) returns no rows (a
write-only toggle). graphite previously parsed the pragma but never wired it into
comparison, so the flag was a no-op. The setting rides on a `Connection`
`case_sensitive_like` field, surfaced to pure eval through a new
`Subqueries::case_sensitive_like()` hook so `eval_binary`'s `LIKE` arm and
`func.rs`'s `like()` thread it without touching the ~100 `EvalCtx` build sites;
the VDBE's `Like` op always folds case, so `run_select_vdbe` defers to the
tree-walker whenever the flag is set (off by default, so the common path is
unchanged). Byte-exact vs `sqlite3` 3.50.4 (`tests/case_sensitive_like.rs`).

**`PRAGMA query_only` is now enforced**, not silently ignored. With it `ON` the
connection is read-only: any statement that opens a write transaction —
INSERT/UPDATE/DELETE, every CREATE/DROP/ALTER (including on TEMP tables), VACUUM,
and ANALYZE (which writes `sqlite_stat1`) — fails with `attempt to write a
readonly database`, while SELECT, PRAGMA, ATTACH/DETACH, and read-only
transaction/savepoint control pass through; turning it back off restores writes,
and the get form reads the live flag. graphite previously parsed the pragma but
never gated writes on it. The flag rides on a `Connection` `query_only` field
(set in the `exec_pragma` setter, surfaced on the read path); the gate is a
single chokepoint at the top of `exec_parsed` (`statement_writes_db`), which
every write reaches — DML descends to `run_dml_atomic` from there, and both the
main-target and the swapped temp/attached paths route through it. Byte-exact vs
`sqlite3` 3.50.4 (`tests/query_only.rs`). One documented residual: `REINDEX` of
an existing index is not blocked — graphite models REINDEX as a no-op (indexes
stay current on every write), so it never opens a write transaction.

**`PRAGMA ignore_check_constraints` is now honored**, not silently ignored. With
it `ON`, INSERT and UPDATE skip CHECK enforcement — column-level, table-level,
and named `CONSTRAINT … CHECK` alike — so a row that would violate a CHECK is
stored unchanged; NOT NULL, UNIQUE, and foreign keys are unaffected, since those
are enforced on separate paths. Turning it back off re-enforces CHECK, and the
get form reads the live flag. graphite previously parsed the pragma but always
enforced CHECK. The flag rides on a `Connection` `ignore_check_constraints` field
(set in the `exec_pragma` setter, surfaced on the read path); the gate is a
single early-return at the top of `check_constraints`, the one function both
INSERT and UPDATE call to validate CHECK exprs. Byte-exact vs `sqlite3` 3.50.4
(`tests/ignore_check_constraints.rs`).

**`PRAGMA automatic_index` and `PRAGMA cell_size_check` now round-trip** instead
of being pinned to their defaults. Both are inert in graphite — it builds no
transient automatic indexes, and it validates btree cells on every read
regardless — but, like sqlite, the stored value is observable: setting one and
reading it back returns what was set (previously the get form hard-coded `1` /
`0` and dropped any assignment). Each rides on a `Connection` `Cell` (set in both
the `exec_pragma` setter and the read path, so the assignment persists whether it
arrives via `execute()` or `query()`). Byte-exact vs `sqlite3` 3.50.4
(`tests/pragma_index_check_roundtrip.rs`). One related residual, left as a
documented CLI concern: the *set-form echo* of the value-returning pragmas
(`PRAGMA secure_delete=1`, `busy_timeout`, `analysis_limit`, `journal_size_limit`)
— sqlite prints the resulting value as a row on the assignment itself, while
graphite's one-shot CLI routes `=` setters through `execute()` (which discards
rows); the value is stored correctly and reads back via the plain form. Matching
the echo needs per-pragma knowledge of which setters return a row, since
`foreign_keys=1`/`cache_size=N` do not.

**Remaining.** The long run of completed error-parity / DDL / JSON / qualifier
items that used to sit here has been cleared — each lives in the git history, the
release-plz `CHANGELOG`, and its own `tests/*.rs`. What is left is the genuinely
open work:

- **A-rn3-edge — RENAME COLUMN in genuinely multi-table view/trigger bodies.**
  The token rewrite bails (leaves the body unchanged — never corrupts) on a bare
  column ref that is ambiguous across multiple base sources, because the AST has
  no per-column-ref source span. The *single*-source-with-subqueries cases are
  now handled for both views (`tests/view_rename_column_subquery.rs`) and
  triggers (`tests/trigger_rename_column_subquery.rs`); what remains is the truly
  multi-table body (a join, a subquery over another table, a body statement
  writing a different table).
  - **A-rn3-edge-1** — add a source span (byte range) to `Expr::Column`. This is
    the enabling refactor. (The sibling `schema` field it once shared with the
    now-landed 3-part `schema.table.column` qualifier check is already in place,
    so this is now just the span.)
  - **A-rn3-edge-2** — use the span for scope-aware rename: resolve each bare ref
    to its owning table, rewrite only the matching ones.
- **Prepare-time validation gaps (lazy where SQLite is eager).** A few constructs
  are still validated per-row, so an unreached row (empty / fully-filtered table)
  is accepted where SQLite rejects at prepare time. All want the same fix — a
  statement-level prepare pass that walks every expression once, independent of
  row production:
  - bare (unqualified) refs in *multi-source* derived/subquery scopes and
    `NATURAL`/`USING` coalesced names. `validate_columns_exist` covers the
    top-level plain-table / `ON`-join scope, and `validate_derived_columns` now
    covers the *single* derived-table (subquery) `FROM` — its top-level result /
    `WHERE` / `GROUP BY` / `HAVING` / `ORDER BY` refs are resolved against the
    derived output at prepare time, so `SELECT a FROM (SELECT a FROM t) WHERE
    zzz = 1` errors over an empty derived table, and (since a subquery has no
    rowid) so does `SELECT rowid FROM (SELECT a FROM t)`. Still open: a derived
    table *joined* to another source, and a three-part `schema.table.column` ref
    inside a *correlated subquery body* (binding to an enclosing FROM) —
    `SELECT (SELECT bad.t.a) FROM t` is accepted where sqlite reports
    `no such column: bad.t.a`.
  - unknown / wrong-arity *scalar functions* inside an expression-position
    subquery (`Subquery` / `EXISTS` / `IN (SELECT …)`) — e.g.
    `SELECT (SELECT nope(a)) FROM t` or `… WHERE a IN (SELECT nope(b) FROM t)`.
    `reject_unresolved_functions_in_select` does not recurse into nested SELECTs;
    doing so safely needs the column-vs-function precedence preserved (a bad column
    in the subquery must still win, `no such column` over `no such function`), which
    depends on the same correlated-scope column resolution above. Derived tables in
    `FROM`, CTE bodies, and compound (`UNION`) arms are already covered (they
    materialize, which resolves them).
- **ALTER-time rejection of an ALTER that breaks a dependent.** An `ALTER` that
  makes a dependent view/trigger unresolvable should be rejected and rolled back;
  graphite leaves the now-broken object. Needs statement-level DDL rollback — a
  writer savepoint around `exec_alter`, mirroring `run_dml_atomic`.

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
