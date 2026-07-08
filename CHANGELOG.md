# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/KarpelesLab/graphitesql/compare/v0.0.16...v0.1.0) - 2026-07-08

### Added

- *(storage)* store whole-number reals in REAL columns as int serials
- *(exec)* UPDATE … FROM on WITHOUT ROWID tables + fix source-column stripping
- *(exec)* support UPSERT and RETURNING on WITHOUT ROWID tables
- *(cli)* implement the .read shell command
- *(cli)* implement the .databases shell command
- *(cli)* .tables/.indexes use sqlite's columnar list layout

### Fixed

- *(json)* decode backslash escapes in a quoted JSON path key
- *(exec)* keep an AFTER UPDATE trigger's edit to a later row
- *(exec)* reject unknown columns in a FROM-less SELECT at prepare time
- *(exec)* compensated (Kahan) summation for sum/avg/total
- *(func)* quote() truncates a text value at an embedded NUL
- *(exec)* break compound ORDER BY ties on the remaining columns
- *(exec)* auto rowid after a negative explicit rowid is max+1, not 1
- *(exec)* BEFORE INSERT trigger sees rowid -1 for an auto row
- *(exec)* targeted UPSERT updates the named conflict's row
- *(exec)* reject illegal overrides of a base window's clauses
- *(exec)* printf ! flag renders through the sqlite3FpDecode port

## [0.0.16](https://github.com/KarpelesLab/graphitesql/compare/v0.0.15...v0.0.16) - 2026-07-08

### Added

- *(cli)* .schema matches sqlite's printSchemaLine quirks
- *(cli)* implement the .dump shell command
- *(datetime)* accept a bare HH:MM[:SS] time-shift modifier
- *(exec)* live-seek a joined table by a UNIQUE secondary index (B5b-2)
- *(planner)* render USING COVERING INDEX for a covering join seek
- *(exec)* thread the join rowid through the cost-based reorders
- *(exec)* resolve a table-qualified rowid across a join
- *(geopoly)* the geopoly virtual table (R-Tree-backed spatial index)
- *(geopoly)* the geopoly scalar geometry functions and group_bbox
- *(fts5)* accept the full set of maintenance/config commands
- *(exec)* reject an aggregate/windowed recursive CTE term like sqlite
- *(exec)* honour a recursive CTE's ORDER BY as a priority work-queue
- *(fts5)* the rank config command and unrecognized-option rejection
- *(fts5)* direct DML and the 'delete' command for external/contentless tables
- *(fts5)* external-content tables (content=, content_rowid=)
- *(fts5)* support the braced {col ...}: column-set MATCH filter
- *(planner)* cost-based join order for three or more tables
- *(planner)* match sqlite's DISTINCT/GROUP BY/ORDER BY plan nodes over joins
- *(planner)* scan a join table via a covering index
- *(planner)* drive a two-table join from the secondary-index-seek-inner side
- *(planner)* drive a two-table join from the rowid-seek-inner side
- *(planner)* choose the best ORDER BY index, not the first
- *(planner)* pick a covering index for GROUP BY / DISTINCT among several
- *(planner)* elide ORDER BY when the chosen seek index supplies the order
- *(planner)* prefer a covering index for range-leading seeks
- *(planner)* prefer a covering index for secondary-index seeks
- *(btree)* honor DESC in UNIQUE and rowid-table primary-key auto-indexes
- *(btree)* honor DESC primary key on WITHOUT ROWID tables
- *(btree)* honor DESC index column order (byte-compatible)

### Documentation

- record window ORDER BY NULLS fix and windowed-sum type-tag residual
- record math numeric-type/atan fixes and huge-arg trig residual
- record the geopoly extension in ROADMAP Track D
- record the min/max multi-index covering-EQP deferral

### Fixed

- *(exec)* read an integer-serialized REAL-column value back as a real
- *(json)* json_tree root key/path follows sqlite's jsonEachPathLength
- *(exec)* quote() renders a real at round-trip precision (port sqlite3FpDecode)
- *(datetime)* utc/localtime modifier normalizes out-of-range fields
- *(exec)* zeroblob(NULL) and printf %c of an empty string match sqlite
- *(exec)* coerce integer function arguments like sqlite3_value_int64
- *(exec)* COLLATE must not change comparison affinity
- *(exec)* count(*) via an index honors LIMIT and OFFSET
- *(parser)* reject a column after a table constraint and an empty trigger body
- *(fk)* ON UPDATE SET DEFAULT re-checks against the post-update parents
- *(fk)* delete the parent row before enforcing its ON DELETE actions
- *(fk)* resolve a UNIQUE/PK conflict before the INSERT foreign-key check
- *(fk)* ON … SET DEFAULT re-validates the default references a parent
- *(trigger)* a BEFORE UPDATE trigger's changes to the updated row persist
- *(trigger)* RAISE(IGNORE) in an AFTER trigger no longer skips later rows
- *(vdbe)* GROUP BY key follows the single min/max row like other bare columns
- *(datetime)* numeric Julian-Day argument with modifiers and out-of-range guard
- *(func)* printf precision handling for %c, %q/%Q/%w, %#e, and huge floats
- *(func)* replace() with an empty pattern ignores a NULL replacement
- *(eval)* bit shift by a negative amount is an arithmetic shift the other way
- *(window)* honor NULLS FIRST/LAST in a window ORDER BY
- *(float)* atan of a very large argument returns ±π/2, not 0
- *(func)* math functions NULL non-numeric arguments
- *(func)* ceil/floor/trunc preserve integer type and NULL non-numerics
- *(exec)* carry the outer column's affinity into a correlated comparison
- *(exec)* decline an affinity-unsound index join seek (wrong results)
- *(fts5)* name shadow tables with sqlite's single-quoted form
- *(exec)* match sqlite's shadow-table schema (no space after commas)
- *(exec)* give CREATE TABLE AS SELECT columns their inherited types
- *(vdbe)* apply collation to an ordered aggregate's ORDER BY
- *(vdbe)* resolve ORDER BY names to result aliases before base columns
- *(exec)* honour explicit/inherited collation on ORDER BY terms
- *(fts5)* match sqlite's bm25 score exactly (corpus-statistic correction)
- *(window)* apply ORDER BY when a window's base table has an index
- *(vdbe)* return index order for an equality prefix on a composite index

## [0.0.15](https://github.com/KarpelesLab/graphitesql/compare/v0.0.14...v0.0.15) - 2026-07-06

### Added

- *(eqp)* render the FOR IN-OPERATOR node for an indexed IN-subquery
- *(vdbe)* seek an N-table left-deep chain of ipk joins with live cursors (B5b-2d)
- *(vdbe)* seek an INNER/LEFT rowid-join with a compound ON clause (B5b-2c)
- *(vdbe)* seek the inner side of a LEFT rowid-join with a live cursor (B5b-2b)
- *(vdbe)* seek the inner side of an INNER rowid-join with a live cursor (B5b-2a)
- *(alter)* scope-aware RENAME COLUMN in multi-table view/trigger bodies (A-rn3-edge)
- *(eqp)* render the SEARCH + LIST SUBQUERY for a seekable IN (SELECT) (B9a-seek)
- *(eqp)* emit the GROUP BY/DISTINCT temp-b-tree over a rowid range seek (B9d subset)
- *(eqp)* flatten a bare-LIMIT derived/CTE body under a narrower projection or single-term ORDER BY (B9c-flatten)
- *(eqp)* render LIST SUBQUERY + BLOOM FILTER for a non-correlated IN (SELECT) (B9a)
- *(planner)* seek col = (non-correlated scalar subquery) (B9e)

### Documentation

- defer B9b (window-function EQP) by design after investigation
- trim completed B9 items from the roadmap
- re-scope B9i (covering-scan row order) into B9h after investigation

### Fixed

- *(exec)* eagerly validate window function args, FILTER and WHERE too
- *(exec)* eagerly reject a bad column in a window OVER PARTITION BY / ORDER BY
- *(exec)* eagerly reject a bad column on the LHS of an IN (SELECT)
- *(exec)* eagerly reject a bad column in a multi-row VALUES IN-list

### Performance

- *(planner)* read a covering index for a no-seek WHERE scan
- *(planner)* port SQLite's covering-scan width cost model (B9h slice)

## [0.0.14](https://github.com/KarpelesLab/graphitesql/compare/v0.0.13...v0.0.14) - 2026-07-01

### Added

- *(planner)* seek a fixed-prefix GLOB range on a BINARY index (B9f)
- *(eqp)* render a non-flattenable LIMIT/OFFSET derived/CTE body as a CO-ROUTINE (B9c)
- *(planner)* seek a trailing rowid range via a rowid/oid alias too (B9g)
- *(planner)* seek a trailing rowid range after an index equality prefix (B9g)
- *(planner)* seek a parenthesized column like the bare column
- *(eqp)* honor the NOT INDEXED hint in EXPLAIN QUERY PLAN
- *(eqp)* flatten a bare-LIMIT derived/CTE body under a pure-wildcard outer in EXPLAIN QUERY PLAN
- *(planner)* seek col IS <non-null constant> like an equality
- *(eqp)* render a DISTINCT derived/CTE/view body as a CO-ROUTINE in EXPLAIN QUERY PLAN
- *(eqp)* render an aggregate derived/CTE/view body as a CO-ROUTINE in EXPLAIN QUERY PLAN
- *(eqp)* render a FROM-less scalar subquery in EXPLAIN QUERY PLAN
- *(eqp)* render a scalar node for a row-value UPDATE SET subquery in EXPLAIN QUERY PLAN
- *(eqp)* render a scalar subquery node for single-row INSERT VALUES in EXPLAIN QUERY PLAN
- *(eqp)* render a scalar subquery node for UPDATE/DELETE in EXPLAIN QUERY PLAN
- *(eqp)* flatten an aliased derived/CTE projection and validate outer refs in EXPLAIN QUERY PLAN
- *(eqp)* flatten a source-alias-qualified derived/CTE projection in EXPLAIN QUERY PLAN
- *(eqp)* flatten a narrower derived/CTE projection in EXPLAIN QUERY PLAN
- *(eqp)* flatten a derived/CTE source through an outer WHERE
- *(eqp)* serve a redundant NULLS ORDER BY from the index without a sorter
- *(eqp)* render partial-cover ORDER BY on a compound MERGE plan
- *(eqp)* resolve named ORDER BY terms in the compound MERGE plan
- *(eqp)* render MERGE plan for a top-level compound with ORDER BY
- *(eqp)* render a compound CTE/derived body as CO-ROUTINE over COMPOUND QUERY
- *(eqp)* honor a MATERIALIZED CTE hint with a MATERIALIZE node
- *(eqp)* render a multi-row VALUES CTE body as a CO-ROUTINE with SCAN N CONSTANT ROWS
- *(eqp)* render a multi-row VALUES clause in FROM as a SCAN/SEARCH N-ROW VALUES CLAUSE node
- *(eqp)* fold a multi-row VALUES clause into a single SCAN N-ROW VALUES CLAUSE node
- *(eqp)* render outer ORDER BY/GROUP BY/DISTINCT temp-b-tree node for a recursive CTE
- *(eqp)* render CO-ROUTINE plan for a recursive CTE
- *(eqp)* render SCALAR SUBQUERY for a GROUP BY projection subquery
- *(planner)* seek a comma join on an unqualified equi-predicate
- *(eqp)* render SCALAR SUBQUERY for an ORDER BY scalar subquery
- *(eqp)* render SCALAR SUBQUERY N for a scalar subquery in the projection
- *(eqp)* render SCALAR SUBQUERY N for a non-correlated scalar WHERE subquery
- *(planner)* elide the sorter for a WITHOUT ROWID filtered full-scan's order
- *(planner)* elide the sorter for a WITHOUT ROWID PK-seek's ordered output
- *(planner)* elide the sorter for a WITHOUT ROWID PK-ordered scan
- *(eqp)* elide the ORDER BY sorter for an ordinal over SELECT *
- *(eqp)* elide the ORDER BY sorter for an ordinal/alias over an index-ordered scan
- *(eqp)* fold an ORDER BY into the GROUP BY / DISTINCT b-tree by ordinal or alias
- *(eqp)* drop the ORDER BY sorter for a single-row bare aggregate
- *(eqp)* place distinct-aggregate temp-b-trees after the GROUP BY sorter
- *(eqp)* emit USE TEMP B-TREE FOR <f>(DISTINCT) for distinct aggregates
- *(eqp)* generalize the min/max SEARCH optimization

### Documentation

- plan the remaining EQP-fidelity & access-path tracks (B9a–B9j)

### Fixed

- *(planner)* apply the COLLATE-mismatch seek check to range bounds too
- *(planner)* don't index-seek an equality whose COLLATE differs from the column
- *(eqp)* render a view source in EXPLAIN QUERY PLAN instead of crashing
- *(eqp)* render SEARCH for a min/max outer over a recursive CTE
- *(exec)* stop recursive-CTE column-origin resolution from overflowing
- *(eqp)* range-check positional GROUP BY / ORDER BY terms on the plan path

## [0.0.13](https://github.com/KarpelesLab/graphitesql/compare/v0.0.12...v0.0.13) - 2026-06-30

### Added

- *(eqp)* render WITHOUT ROWID min/max as SEARCH USING PRIMARY KEY
- *(eqp)* render the min/max optimization as SEARCH
- *(planner)* elide ORDER BY for single-value-pinned WHERE seeks
- *(planner)* seek a WITHOUT ROWID secondary index for `col IS NULL`
- *(planner)* seek the index for a `col IS NULL` constraint
- *(eqp)* label aggregate-only seeks USING COVERING INDEX
- *(eqp)* name aliased tables by alias and tag LEFT-JOIN inner side
- *(eqp)* elide ORDER BY temp b-tree for a rowid IN-list seek (B-planner slice 12)
- *(eqp)* seek the WITHOUT ROWID PRIMARY KEY for an IN-list / OR-chain
- *(eqp)* collapse a same-column equality OR-chain to one index seek
- *(eqp)* collapse a rowid-equality OR-chain to a single rowid seek
- *(eqp)* plan a rowid-pinning DISTINCT as the no-op it is
- *(eqp)* plain-scan a rowid GROUP BY and skip its redundant sort (B5b-1)
- *(eqp)* elide equality-pinned ORDER BY terms on the seek path (B5b-1)
- *(eqp)* serve a trailing rowid from a WHERE-seek index walk
- *(eqp)* credit a named UNIQUE index for the trailing-rowid ORDER BY walk
- *(eqp)* serve a trailing rowid term from the secondary-index walk
- *(eqp)* skip the sort when ORDER BY leads with the rowid/IPK
- *(eqp)* fold ORDER BY into the GROUP BY/DISTINCT temp b-tree
- *(eqp)* emit USE TEMP B-TREE FOR GROUP BY / DISTINCT on a plain scan
- *(eqp)* walk an index whose columns are a prefix of a longer ORDER BY
- *(eqp)* render the COMPOUND QUERY tree for UNION/INTERSECT/EXCEPT
- *(eqp)* render EXPLAIN QUERY PLAN over a CTE source
- *(eqp)* flatten a wildcard derived table into the base-table scan
- *(vdbe)* run an uncorrelated FROM-less EXISTS subquery inline (B5b-1)
- *(vdbe)* run an uncorrelated FROM-less scalar subquery inline (B5b-1)
- *(vdbe)* run a no-op ORDER BY over a FROM-less SELECT on the VDBE
- *(json)* drive bare json_each/json_tree from WHERE json constraints
- *(pragma)* drive bare pragma TVFs from WHERE arg constraints
- *(vdbe)* run a FROM-less SELECT with LIMIT/OFFSET/DISTINCT on the VDBE
- *(vdbe)* run a FROM-less SELECT with a WHERE clause on the VDBE
- *(vdbe)* run more pure scalar functions on the VDBE
- *(vdbe)* run the date/time scalar functions on the VDBE
- *(alter)* reject DROP COLUMN that breaks a dependent view or trigger
- *(alter)* propagate RENAME COLUMN into same-table qualified self-refs
- *(alter)* detect UPDATE…FROM subquery trigger bodies on RENAME TABLE
- *(alter)* propagate RENAME COLUMN into cross-object trigger bodies (A-rn3)
- *(alter)* propagate RENAME COLUMN into a view's nested cross-source subquery
- *(exec)* reject unknown/wrong-arity functions in expression-position subqueries
- *(exec)* reject unknown columns over a NATURAL/USING join at prepare time
- *(exec)* reject unknown columns over a derived-table join at prepare time
- *(vdbe)* run a window over a NATURAL/USING join on the VDBE (B5c-4)
- *(vdbe)* run a window function over a join containing a derived subquery
- *(vdbe)* run SELECT */window over joins sharing a column or holding a view/TVF
- *(vdbe)* run a window-function SELECT over a TVF source on the VDBE (B5b-1)
- *(vdbe)* run a window-function SELECT over a view source on the VDBE (B5b-1)
- *(vdbe)* run a view named directly as a FROM source on the VDBE (B5b-1)
- *(vdbe)* resolve a view source's column affinity, fixing a derived-over-view divergence (B5b-1)
- *(vdbe)* run a derived table whose body is a same-affinity compound on the VDBE (B5b-1)
- *(vdbe)* run a derived table whose body is a plain join on the VDBE (B5b-1)
- *(vdbe)* run a constant-argument table-valued function in a join on the VDBE (B5b-1)
- *(vdbe)* run json_each/json_tree FROM sources on the VDBE (B5b-1)
- *(vdbe)* run a single table-valued-function FROM source on the VDBE (B5b-1)
- *(vdbe)* run a sibling-CTE FROM source on the VDBE (B5b-1)
- *(vdbe)* resolve a GROUP BY output-alias on the VDBE (B5b-1)
- *(vdbe)* run SELECT DISTINCT over a grouped query on the VDBE (B5b-1)
- *(vdbe)* run a computed (non-column) GROUP BY key on the VDBE (B5b-1)
- *(vdbe)* fold compound-bodied non-correlated subqueries (B5c-1)
- *(vdbe)* fold nested non-correlated subqueries (B5c-1)
- *(vdbe)* run a window over a VALUES derived/CTE source on the VDBE
- *(vdbe)* run a window function over a CTE source on the VDBE
- *(vdbe)* run a window function over a derived subquery on the VDBE
- *(vdbe)* run a nested derived-table source on the VDBE
- *(vdbe)* run a compound SELECT with CTEs on the VDBE
- *(vdbe)* run a CTE FROM-source on the VDBE
- *(vdbe)* run a constant/VALUES subquery source on the VDBE
- *(vdbe)* track the min/max companion row for bare columns
- *(vdbe)* represent bare columns on the general grouped path
- *(vdbe)* emit a first-row representative for a bare column in grouped output
- *(vdbe)* run two-argument group_concat/string_agg on the VDBE
- *(vdbe)* run printf/format on the VDBE
- *(vdbe)* run text LIKE pattern ESCAPE c on the VDBE

### Documentation

- record empirically-mapped name-resolution precedence for A-misc-1
- clear done A-misc-2, sharpen A-misc-1 with clause-order precedence
- collapse completed Track A/B narrative, expand remaining open work
- note ALTER TABLE + cross-object propagation in the README
- bump focused-suite count to 360+ in README status line
- record printf '!' high-precision float-decode gap in ROADMAP

### Fixed

- *(vdbe)* walk a no-WHERE covering-index scan in key order
- *(vdbe)* walk a multi-value IN seek in index-key order
- *(vdbe)* walk secondary-index seeks in key order by deferring to the tree-walker
- *(eqp)* never emit a LAST 0 TERMS temp-btree node
- *(eqp)* decline EXPLAIN QUERY PLAN of a derived table joined to another source
- *(eqp)* render CO-ROUTINE for an aliased constant-row derived table
- *(exec)* reject a FROM-less wildcard projection at prepare time
- *(pragma)* empty result for argumentless introspection PRAGMAs
- *(cli)* print rows for the =arg form of query PRAGMAs
- *(exec)* catch unresolved columns in correlated subquery bodies at prepare (A-prepare-correlated)
- *(exec)* keep same-named columns from different databases distinct under *
- *(parser)* accept a postfix COLLATE after a closed IN (…) construct
- *(exec)* error on unrecognized pragma table-valued sources
- *(exec)* reject positional GROUP BY resolving to an aggregate column
- *(exec)* resolve signed/wrapped positional ORDER BY / GROUP BY ordinals
- *(vdbe)* defer SELECT DISTINCT with an explicit COLLATE projection
- *(vdbe)* defer min/max with an explicit COLLATE arg to the tree-walker
- *(vdbe)* defer DISTINCT aggregate with an explicit COLLATE arg
- *(alter)* keep a foreign key's parent column intact on RENAME COLUMN
- *(alter)* rewrite renamed table in trigger WHEN guard and body subqueries
- *(eval)* coerce a blob to a number via its bytes-as-text
- *(value)* compare integers exactly, not through f64 (precision loss above 2^53)
- *(func)* sum() result type follows numeric affinity, not storage class
- *(vdbe)* defer negative/wrapped ORDER BY ordinals to the tree-walker
- *(vdbe)* order GROUP BY output by the grouping keys
- *(alter)* rename INSERT-INTO target in a trigger body on RENAME TABLE
- *(window)* order grouped+windowed rows by the window like SQLite

## [0.0.12](https://github.com/KarpelesLab/graphitesql/compare/v0.0.11...v0.0.12) - 2026-06-28

### Added

- *(exec)* resolve forward CTE references and reject true cycles by entry point
- *(exec)* validate the database qualifier on REINDEX schema.name
- *(exec)* forbid ALTER/DROP/CREATE INDEX on internal sqlite_ tables
- *(exec)* report circular reference for anchorless recursive CTE
- *(cli)* report the result row of a PRAGMA journal_mode setter
- *(exec)* name a *-expansion self-join ambiguity by its source origin
- *(exec)* reject a window function nested in another window's definition
- *(exec)* match sqlite's message for a non-TVF name called in FROM
- *(exec)* match sqlite's VALUES vs operator message on compound arity mismatch
- *(exec)* reject IN-list element arity mismatch at prepare time
- *(exec)* reject row-value misuse at prepare time
- *(exec)* reject a multi-column scalar subquery at prepare time
- *(exec)* reject IN-subquery column-count mismatch at prepare time
- *(exec)* reject unknown/wrong-arity functions in DELETE/UPDATE clauses
- *(exec)* report an unknown windowed function as "no such function"
- *(exec)* validate columns over a single derived-table FROM at prepare time
- *(pragma)* round-trip automatic_index and cell_size_check
- *(pragma)* honor PRAGMA ignore_check_constraints
- *(pragma)* enforce PRAGMA query_only as read-only mode
- *(pragma)* honor PRAGMA case_sensitive_like for LIKE
- *(parser)* accept WITH before any INSERT source, not just INSERT … SELECT
- *(window)* allow any constant expression as a frame offset
- *(exec)* reject CREATE TRIGGER on a system table
- *(exec)* re-apply the non-generated-column rule after DROP COLUMN
- *(exec)* reject aggregate/window functions in CREATE INDEX
- *(exec)* reject a qualified DML target in a trigger body
- *(exec)* skip an unreferenced WITH CTE on UPDATE/DELETE
- *(exec)* support an UPDATE/DELETE target table alias (... AS x)
- *(exec)* detect foreign key mismatch like sqlite
- *(cte)* reject a recursive CTE that self-joins its recursive table
- *(dml)* honor INDEXED BY / NOT INDEXED on UPDATE and DELETE
- *(exec)* validate three-part column qualifiers in UPSERT clauses
- *(exec)* validate three-part column qualifiers in UPDATE/DELETE
- *(exec)* validate three-part schema.table.column column qualifiers
- *(json)* json_each/json_tree accept a JSONB blob document
- *(json)* expose hidden json/root columns on json_each and json_tree
- *(json)* preserve escaped object-key provenance through JSONB and text
- *(json)* preserve JSON5 string escapes via the TEXT5 JSONB tag
- *(json)* preserve standard-JSON string escapes verbatim in text and JSONB
- *(func)* implement sqlite_source_id()
- *(eqp)* render SCAN CONSTANT ROW for a FROM-less SELECT
- *(exec)* accept rowid/_rowid_/oid as an INSERT target column
- *(parser)* report premature end-of-input as "incomplete input"
- *(exec)* reject FILTER on a non-aggregate function at prepare time
- *(exec)* reject aggregate misuse in ORDER BY and join ON at prepare time
- *(exec)* reject window-function misuse at prepare time
- *(exec)* reject aggregate-in-WHERE misuse at prepare time
- *(exec)* validate UPSERT column references against the target table

### Documentation

- note SQLite-compatible prepare-time diagnostics in README
- bump differential test-suite count to 260+
- record JSON string-escape provenance as a remaining Track A item
- prune completed items from ROADMAP and expand what remains

### Fixed

- *(window)* honor key collation in PARTITION BY / ORDER BY
- *(window)* emit rows in window order when no outer ORDER BY
- *(printf)* round %e mantissa half away from zero like SQLite
- *(index)* name the column in WITHOUT ROWID secondary-index UNIQUE errors
- *(index)* reject CREATE UNIQUE INDEX over existing duplicate rows
- *(json)* json_quote(X) renders a JSONB blob as its JSON text
- *(exec)* RENAME COLUMN rewrites trigger refs inside WHEN/body subqueries
- *(exec)* rewrite view subquery refs on RENAME COLUMN
- *(exec)* sequence PRIMARY KEY validation so a duplicate PK outranks a generated-column PK error
- *(exec)* echo the written qualifier in the ambiguous-column-name message
- *(parser)* reject ALL/DISTINCT in operand position, accept ALL in aggregates
- *(exec)* resolve a SELECT-output alias inside an ORDER BY expression
- *(exec)* treat an aggregate in a window OVER spec as a single group
- *(exec)* resolve generated-column forward refs and reject cycles at CREATE
- *(trigger)* resolve a FROM-less SELECT step when the trigger fires
- *(trigger)* police the full trigger-step grammar inside a body
- *(trigger)* defer body ORDER BY/LIMIT error behind target resolution
- *(exec)* resolve LIMIT/OFFSET in an empty column scope at prepare time
- *(exec)* resolve functions and aggregate misuse nested in row values and COLLATE
- *(exec)* resolve unknown and wrong-arity scalar functions at prepare time
- *(exec)* reject an aggregate or window function inside a FILTER predicate
- *(exec)* reject min()/max() with zero arguments at prepare time
- *(exec)* reject aggregate/window function in DELETE/UPDATE RETURNING
- *(window)* reject a window-only function used without OVER at prepare time
- *(parser)* name the clause when ORDER BY/LIMIT precedes a compound op
- *(update)* reject ORDER BY without LIMIT on UPDATE/DELETE
- *(trigger)* schema-qualify a trigger body's missing-table error
- *(ddl)* order CREATE TABLE validation like sqlite's schema-build pass
- *(exec)* arity-check aggregates used as window functions
- *(exec)* validate aggregate arity at prepare time
- *(exec)* reject a nested aggregate or window in an aggregate argument at prepare time
- *(func)* validate likelihood() at prepare time, not per row
- *(cte)* do not analyze an unreferenced WITH CTE
- *(json)* match sqlite arity for json_set/insert/replace family
- *(parser)* reject a reserved keyword in table-option position
- *(exec)* a `*` argument is valid only for count()
- *(exec)* reject a scalar function used as a window function
- *(parser)* report window frame-bound errors with sqlite's two messages
- *(exec)* order partial-index WHERE subquery/non-determinism errors like sqlite
- *(parser)* accept an optional transaction name in BEGIN/COMMIT/END
- *(parser)* reject SQLite's reserved keywords as bare names
- *(dml)* reject ON CONFLICT targets that match no PK/UNIQUE constraint
- *(ddl)* resolve CHECK/generated/index functions at CREATE
- *(ddl)* resolve CHECK/generated-column functions at CREATE
- *(pragma)* report PRAGMA journal_size_limit getter
- *(exec)* reject RAISE() used outside a trigger program
- *(vacuum)* match SQLite's VACUUM INTO existing-file message
- *(parser)* OFFSET is not a reserved keyword outside LIMIT
- *(cli)* run each command-line argument as its own SQL batch
- *(alter)* wrap a RENAME COLUMN name collision like sqlite
- *(exec)* keep the schema qualifier when a known database is missing the object
- *(exec)* report an unknown database qualifier on a table reference as a missing object
- *(json)* render a JSON5 dot-form number with the minimal zero, not the float
- *(json)* reject a JSON number with a leading-zero integer part
- *(json)* preserve verbatim text of an f64-overflowing JSON number
- *(pragma)* report notnull=1 for WITHOUT ROWID primary-key columns
- *(lexer)* report an over-64-bit hex literal as "hex literal too big"
- *(eval)* the LIKE ESCAPE character is never a wildcard
- *(parser)* desugar iif()/if() to CASE for multi-branch + short-circuit
- *(exec)* name the existing object's kind in a CREATE collision
- *(parser)* report unknown table options like SQLite
- *(eval)* hint at a string literal for an unresolved double-quoted column
- *(parser)* reject ORDER BY/LIMIT after a VALUES query core
- *(eval)* report "row value misused" for vector comparison operands
- *(alter)* ADD COLUMN inserts before trailing table constraints
- *(schema)* canonicalise schema-qualified, TEMP and CTAS stored sql
- *(schema)* canonicalise sqlite_schema.sql like sqlite
- *(json)* quote non-simple keys in json_each/json_tree fullkey/path
- *(json)* number json_each/json_tree id by JSONB byte offset
- *(cli)* a ;-truncated statement is a syntax error, not "incomplete input"
- *(parser)* reject AUTOINCREMENT outside a column PRIMARY KEY
- *(pragma)* parse user_version/application_id value as an integer token
- *(pragma)* return empty for index_info/foreign_key_list on an unknown object
- *(exec)* reject a CREATE VIEW column-count mismatch on use
- *(alter)* match sqlite's DROP COLUMN refusal rules and messages
- *(exec)* reject a missing column in GROUP BY/HAVING/ORDER BY clauses
- *(eval)* reject a scalar IN (SELECT …) with the wrong column count
- *(func)* emit a trailing % in printf/format literally
- *(json)* -> and ->> reject a malformed text document
- *(window)* RANGE offset frame requires exactly one ORDER BY expression
- *(json)* json_each/json_tree reject >2 args and accept zero args
- *(tokenizer)* report lexing failures as `unrecognized token: "X"`
- *(parser)* report syntax errors as `near "TOKEN": syntax error`
- *(vdbe)* reject non-integer LIMIT/OFFSET on the table-scan path
- *(exec)* use sqlite's nested-aggregate misuse wording

### Testing

- drop a build-divergent LIKE-trailing-escape query from the oracle list
- gate UPDATE/DELETE ORDER BY LIMIT diffs on the update/delete-limit extension

## [0.0.11](https://github.com/KarpelesLab/graphitesql/compare/v0.0.10...v0.0.11) - 2026-06-26

### Added

- *(exec)* reserve the sqlite_ object-name prefix for internal use
- *(vdbe)* run N-table LEFT/INNER join chains on the VDBE

### Documentation

- note DELETE/UPDATE eager column resolution in ROADMAP
- record eager-resolution coverage (ON, star qualifier, qualified GROUP/ORDER refs)

### Fixed

- *(func)* substr(X, NULL [, ...]) returns NULL for a NULL start
- *(func)* trim/ltrim/rtrim return NULL for a NULL trim-set
- *(exec)* don't panic on rowid overflow at the i64::MAX boundary
- *(exec)* silently ignore an unrecognized PRAGMA name
- *(exec)* reject duplicate CTE names within one WITH clause
- *(exec)* a row value in a scalar context reports "row value misused"
- *(exec)* window function in WHERE/HAVING reports "misuse of window function"
- *(exec)* reject aggregate functions in the GROUP BY clause
- *(exec)* string_agg requires its separator; recognize JSON group aggregates
- *(exec)* count() with no arguments behaves as count(*)
- *(exec)* report "DISTINCT aggregates must have exactly one argument"
- *(func)* validate the likelihood() probability argument
- *(datetime)* strftime defaults to 'now' and coerces a non-text format
- *(json)* json_remove with a NULL path returns NULL
- *(json)* coerce non-text JSON paths to text and short-circuit NULL paths
- *(exec)* quote the table name in the multiple-primary-key error
- *(parser)* UPDATE SET tuple-width mismatch uses sqlite's wording
- *(exec)* match sqlite's INSERT column-resolution error messages
- *(exec)* resolve DELETE/UPDATE WHERE and SET-value columns eagerly
- *(exec)* eagerly resolve qualified refs in GROUP BY / HAVING / ORDER BY
- *(exec)* reject a star whose table qualifier names no FROM source
- *(exec)* extend eager column resolution to join ON predicates
- *(exec)* resolve column references eagerly so a missing column errors on an empty result

## [0.0.10](https://github.com/KarpelesLab/graphitesql/compare/v0.0.9...v0.0.10) - 2026-06-26

### Added

- *(vdbe)* run window functions over a plain join on the VDBE
- *(vdbe)* run window functions over a single table on the VDBE
- *(vdbe)* run ordered group_concat(x ORDER BY …) on the VDBE
- *(vdbe)* run aggregate FILTER (WHERE …) on the VDBE
- *(vdbe)* run DISTINCT aggregates over a bare two-table join
- *(vdbe)* run DISTINCT aggregates over GROUP BY on the VDBE
- *(vdbe)* run DISTINCT aggregates on the bare aggregate path
- *(vdbe)* resolve positional GROUP BY ordinals on the VDBE
- *(vdbe)* run compound SELECT (UNION/INTERSECT/EXCEPT) on the VDBE (B5c-3)
- *(vdbe)* run DISTINCT over two-table outer joins on the VDBE (B5b-1)
- *(vdbe)* run ORDER BY over two-table outer joins on the VDBE (B5b-1)
- *(vdbe)* run GROUP BY over a join with the full grouped grammar (B5b-1)
- *(vdbe)* fold a plain GROUP BY inner join over the nested loop (B5b-1)
- *(vdbe)* fold a bare-aggregate inner join over the nested loop (B5b-1)
- *(vdbe)* run an ORDER BY inner nested-loop join through the sorter (B5b-1)
- *(vdbe)* support DISTINCT over an inner nested-loop join (B5b-1)
- *(vdbe)* run two-table FULL JOIN on the VDBE (B5b-1)
- *(vdbe)* run two-table RIGHT JOIN on the VDBE (B5b-1)
- *(vdbe)* run two-table LEFT JOIN on the VDBE with null-padding (B5b-1)
- *(vdbe)* generalize the nested-loop join to N tables (B5b-1)
- *(vdbe)* run two-table inner joins as a nested loop over two cursors (B5b-1)
- *(vdbe)* route bare-column IN (SELECT col) on the VDBE via candidate affinity (B5c-1)
- *(vfs)* verify reader SHARED-lock sharing end-to-end (C9a)
- *(fts5)* decode multi-leaf segments — term pagination + doclist spanning (D2b-3)
- *(pager)* write and recover SQLite-format rollback journals (C7a/C7b)
- *(fts5)* decode the %_data segment index for a single-term doclist lookup (D2b-1)
- *(vdbe)* fold a non-correlated IN (SELECT computed) to IN (list) for the VDBE
- *(vdbe)* fold a non-correlated subquery in LIMIT/OFFSET so it runs on the VDBE
- *(vtab)* report main for any-schema-qualified dbstat/sqlite_dbpage
- *(vtab)* resolve + introspect the eponymous dbstat/sqlite_dbpage tables
- *(vtab)* add the read-only sqlite_dbpage virtual table
- *(attach)* resolve unqualified names against attached databases (Track E)

### Documentation

- document the opt-in unicode case-folding feature
- ROADMAP — bare-column IN (SELECT) cannot be folded (attempt reverted)
- ROADMAP — concrete implementation path for B5c-1 bare-column IN (SELECT)
- README — index-driven FTS5 MATCH, SQLite-format journal + crash recovery, bounded cache
- mark ROADMAP Track E essentially complete (E0/E1/E2/E3/E-arch-a done)
- trim ROADMAP to remaining work; condense the README status block

### Fixed

- *(func)* fold ASCII-only by default, add opt-in unicode feature
- *(exec)* schema-qualify a missing table in CREATE INDEX / CREATE TRIGGER
- *(exec)* SQLite-exact compound, window, and WITHOUT ROWID error messages
- *(insert)* reject a non-integer INTEGER PRIMARY KEY value as datatype mismatch
- *(exec)* SQLite-exact "no such column" qualifier and compound ORDER BY error
- *(func)* match SQLite's scalar-function arity and json_extract(<2-arg)
- *(exec)* match SQLite's out-of-range ORDER BY/GROUP BY message
- *(trigger)* SELECT RAISE(...) WHERE cond honors the WHERE clause
- *(insert)* a bare INSERT targets only non-generated columns
- *(eval)* apply comparison affinity to IS / IS NOT and CASE x WHEN y
- *(eval)* IN (list) applies only the left operand's affinity to each element
- *(eval)* apply a scalar subquery's column affinity in comparisons

### Performance

- *(vdbe)* route bare-column IN (SELECT col) regardless of the candidate's collation
- *(fts5)* route phrase + NEAR MATCH over multi-segment indexes (D2b-2)
- *(fts5)* index-route K-term phrases (K>=2), not just two-term (D2b-2)
- *(fts5)* index-route bare-term/boolean/prefix MATCH over multi-segment indexes (D2b-2)
- *(fts5)* index-route a two-term NEAR(a b, n) MATCH (D2b-2)
- *(fts5)* index-route N-operand bare-term boolean trees (D2b-2)
- *(fts5)* index-route a single prefix-term MATCH (D2b-2)
- *(fts5)* index-route two-operand bare-term boolean MATCH (D2b-2)
- *(fts5)* index-route a two-term phrase MATCH (D2b-2)
- *(fts5)* index-route column-scoped single bare-term MATCH (D2b-2)
- *(pager)* bound the page cache with LRU eviction (C8c)
- *(fts5)* route single bare-term MATCH through the segment index (D2b-2)

### Testing

- *(pager)* add a WAL-mode crash-recovery harness (§6)
- *(pager)* add a fault-injecting VFS crash-recovery harness (C7)
- pin build-independent value semantics against sqlite3
- *(attach)* add E0 cross-database write regression oracle (Track E)

## [0.0.9](https://github.com/KarpelesLab/graphitesql/compare/v0.0.8...v0.0.9) - 2026-06-24

### Fixed

- *(attach)* resolve a VALUES subquery in main for a cross-db INSERT
- *(attach)* resolve INSERT … SELECT source in main, not the target db
- *(pragma)* run bare / (N) incremental_vacuum off the query path
- *(datetime)* render strftime %J at 16 significant digits
- *(datetime)* honor subsec modifier in strftime %s (millisecond epoch)
- *(window)* reject a non-positive ntile / nth_value argument

### Testing

- *(like)* pin LIKE as ASCII-only case-insensitive (vs the alt1 oracle's Unicode fold)

## [0.0.8](https://github.com/KarpelesLab/graphitesql/compare/v0.0.7...v0.0.8) - 2026-06-24

### Added

- *(create)* reject an aggregate function in a CHECK or generated column
- *(create)* reject a foreign key naming an unknown local column
- *(create)* reject invalid constraints on a generated column
- *(select)* reject ambiguous column names that bind to an enclosing FROM
- *(select)* reject ambiguous unqualified column names
- *(subquery)* compare a row value against a row-returning subquery
- *(vdbe)* run NATURAL/USING joins combined with outer joins
- *(vdbe)* run chained RIGHT/FULL outer joins via a unified outer-join path
- *(vdbe)* run INNER NATURAL/USING joins on the VDBE
- *(vdbe)* run single RIGHT/FULL outer joins on the VDBE
- *(vdbe)* run LEFT JOIN queries on the VDBE via nested-loop NULL-extension
- *(window)* support window functions over GROUP BY / aggregates
- *(pragma)* round-trip PRAGMA busy_timeout and implement wal_checkpoint
- *(pragma)* implement PRAGMA analysis_limit and PRAGMA optimize
- *(vdbe)* fold non-correlated scalar and EXISTS subqueries to constants
- *(pragma)* index_xinfo lists a WITHOUT ROWID index's trailing PK columns
- *(pragma)* index_info/index_xinfo report expression-index columns
- *(vdbe)* compile a FROM subquery (derived table) over a BINARY base
- *(vdbe)* resolve rowid/_rowid_/oid on a single-table scan
- *(vdbe)* compile the COLLATE operator with sqlite's collation precedence
- *(vdbe)* fold constant LIMIT/OFFSET expressions, not just literals
- *(vdbe)* compile N-table inner joins (not just two)
- *(sql)* accept a VALUES clause as the IN right-hand side
- *(vdbe)* compile x IS TRUE / IS FALSE truthiness tests
- *(sql)* support WITH on UPDATE and DELETE statements
- *(fts5)* honor unicode61 tokenchars and separators options
- *(rtree)* prune the node tree by query bounds (spatial pushdown)
- *(fts5)* honor unicode61 remove_diacritics 0|1|2 and the ascii tokenizer
- *(fts5)* fold the full Latin diacritic table to match unicode61
- *(vdbe)* run explicit-parameter queries on the VDBE via substitution (B5/B7)
- *(planner)* mixed-direction partial sort over a non-covering index (B0b-i)
- *(planner)* partial-sort EQP for mixed-direction ORDER BY over a covering index (B0b-i)
- *(alter)* RENAME COLUMN propagates into cross-object trigger bodies (A-rn3)
- *(fts5)* store FTS5 in sqlite's shadow tables so sqlite can read it (D2e-M2b)
- *(pragma)* honor PRAGMA secure_delete (round-trip + zero freed pages) (C8a)
- *(alter)* RENAME COLUMN propagates into multi-source view bodies (A-rn3)
- *(fts5)* read SQLite-written FTS5 documents (D2e M1) + accept quoted DDL names
- *(rtree)* write SQLite's byte-compatible R-Tree node format (D3c M2)
- *(rtree)* read SQLite's byte-compatible R-Tree on-disk node format (D3c M1)
- *(pragma)* report SQLite defaults for legacy/no-op boolean pragmas
- *(update)* UPDATE SET (cols) = (SELECT …) row-value-subquery assignment
- *(vacuum)* VACUUM INTO writes a compact copy to a new file
- *(fts5)* fts5vocab vocabulary tables (row/col/instance forms)
- *(vtab)* dbstat eponymous read-only virtual table for per-page stats
- *(json)* json_valid accepts the optional FLAGS argument
- *(autoincrement)* persist the rowid high-water mark in sqlite_sequence
- *(alter)* RENAME COLUMN propagates into single-source triggers (A-rn3)
- *(alter)* RENAME COLUMN propagates into single-source views (A-rn3)
- *(vdbe)* make the VDBE the default SELECT engine (B7b)
- *(vdbe)* collation-aware comparison and ORDER BY (NOCASE/RTRIM columns)
- *(vdbe)* plain EXPLAIN lists the compiled VDBE bytecode (B8)
- *(vdbe)* route per query block so compound-query arms use the VDBE
- *(vdbe)* qualified column resolution; shared-name joins work
- *(vdbe)* route query() onto the VDBE behind an opt-in flag (B7a)
- *(vdbe)* spike compiles two-table inner joins (B5a)
- *(vdbe)* spike compiles table.* projection in a single-table scan
- *(vdbe)* spike compiles pure scalar function calls
- *(vdbe)* spike compiles the -> and ->> JSON extraction operators
- *(vdbe)* spike compiles LIKE/GLOB and IN (list) expressions
- *(vdbe)* spike compiles IS/IS NOT and BETWEEN expressions
- *(vdbe)* spike compiles blob literals, bitwise & unary +/~ ops
- *(constraints)* honor NOT NULL's ON CONFLICT action
- *(constraints)* honor a constraint's ON CONFLICT action
- *(alter)* RENAME COLUMN propagates into other tables' foreign keys
- *(rtree)* add the rtree_i32 integer-coordinate variant
- *(rtree)* support auxiliary (+) columns
- *(fts5)* implement the porter tokenizer (stemming)
- *(fts5)* accept the rebuild/optimize maintenance commands
- *(fts5)* honor UNINDEXED columns
- *(fts5)* snippet() supports the auto-column form snippet(t, -1, …)
- *(fts5)* add snippet() aux function, byte-exact with sqlite
- *(fts5)* gate FTS5 behind a default-on `fts5` feature
- *(fts5)* EXPLAIN QUERY PLAN reports sqlite's MATCH idxNum:idxStr
- *(rtree)* EXPLAIN QUERY PLAN reports sqlite's idxNum:idxStr (D3b)

### Documentation

- mark the aggregate-in-CHECK/generated error-parity item done in ROADMAP
- mark the FK-unknown-local-column error-parity item done in ROADMAP
- refresh ROADMAP — clear completed work, expand the remaining tracks
- VDBE join family complete (NATURAL/USING + outer joins)
- record unified outer-join path / chained RIGHT-FULL on the VDBE
- record INNER NATURAL/USING joins on the VDBE (completes join family)
- record single RIGHT/FULL outer joins on the VDBE in ROADMAP
- record LEFT JOIN on the VDBE (B5a/B5b) in ROADMAP
- note window-over-GROUP BY/aggregate support in ROADMAP
- record VDBE non-correlated scalar/EXISTS subquery folding (B5c)
- note lazy outer-LIMIT bounding of recursive CTEs in ROADMAP
- scope D2e-M2 FTS5 sqlite-compat to unicode61-matching text (tokenizer gap)
- *(roadmap)* audit VDBE (B5c) coverage and note the param-less foundation
- *(roadmap)* B1b — graphite's join planner diverges from sqlite by design
- *(roadmap)* B4 sqlite_stat4 is blocked by the differential oracle
- README — FTS5 files are now byte-compatible with sqlite
- mark D2e-M1 (read sqlite FTS5) done in ROADMAP
- mark D3c (R-Tree on-disk node format) done in ROADMAP and README
- README notes the VDBE is now the default SELECT engine
- record VDBE pure scalar function support in ROADMAP
- record VDBE spike scalar-expression coverage in ROADMAP
- record snippet() auto-column + spaced-colon fix in ROADMAP

### Fixed

- *(view)* a view column inherits its base column's collation and affinity
- *(window)* correct RANGE-offset frames around NULL ORDER BY values
- *(fk)* compare under the parent key column's collation
- *(join)* apply column affinity to NATURAL/USING coalesce-key equality
- *(fk)* apply the parent key column's affinity when matching child values
- *(eval)* apply per-element comparison affinity to row-value IN (SELECT …)
- *(eval)* apply comparison affinity to IN (SELECT …)
- *(select)* reject HAVING on a non-aggregate query in two missed cases
- *(collation)* reject an unknown COLLATE name when it is consumed in a query
- *(window)* reject an invalid frame specification
- *(vacuum)* error on VACUUM of an unknown database
- *(reindex)* error on REINDEX of an unidentifiable object
- *(analyze)* error on ANALYZE of an unknown object instead of no-op
- *(ddl)* reject an unknown COLLATE name at CREATE TABLE / CREATE INDEX
- *(subquery)* a scalar subquery must return exactly one column
- *(json)* -> / ->> treat a bare key as a literal label, not a nested path
- *(ctas)* auto-rename duplicate output columns instead of erroring
- *(json)* the -> / ->> operators error on a malformed explicit path
- *(func)* reject too many arguments to aggregates and window functions
- *(func)* panic on typeof()/hex()/unicode() with no args; validate trim arity
- *(dml)* reject UPDATE with an unknown SET-target column on an empty table
- validate INDEXED BY index exists; table_info(missing) returns empty
- *(select)* name window-function columns after their source text
- *(select)* t.* over a join names only that table's columns
- *(window)* rewrite aggregates in named WINDOW defs for window-over-aggregate
- *(cte)* bound an infinite recursive CTE by the consuming query's LIMIT
- *(subquery)* inherit affinity/collation through nested derived tables
- *(subquery)* derived-table columns inherit affinity and collation
- *(fk)* INSERT OR REPLACE fires the replaced row's ON DELETE actions
- *(schema)* INTEGER PRIMARY KEY DESC is not a rowid alias
- *(trigger)* fire multiple triggers in reverse creation order like sqlite
- *(error)* drop the redundant "constraint failed:" prefix to match sqlite
- *(alter)* RENAME TABLE repoints foreign keys in other tables
- *(fts5)* fold Latin-1 diacritics in the tokenizer to match unicode61
- *(parser)* number anonymous ? parameters by parse position, not eval order
- *(datetime)* unixepoch(X, 'subsec') returns fractional seconds
- *(alter)* RENAME COLUMN propagates into multi-source trigger bodies
- *(json)* propagate JSON subtype through aggregates, multi-path extract, and ->
- *(parser)* allow OFFSET and END as bare column names
- *(pragma)* journal_mode reports "memory" for an in-memory database
- *(create)* reject CHECK/generated expressions referencing unknown columns
- *(drop)* DROP … IF EXISTS still rejects a wrong-type object
- *(alter)* RENAME TABLE rewrites dependent trigger bodies
- *(vdbe)* harden opt-in routing (grouped bare columns, schema, collation, labels)
- *(json)* json_quote returns a JSON-subtyped argument unquoted
- *(pragma)* foreign_key_list lists foreign keys by id ascending
- *(update)* SET subqueries see the pre-update snapshot
- *(json)* json_set/json_insert create intermediate containers
- *(parser)* reject a numeric literal followed by an identifier character
- *(rtree)* reject a duplicate id, the rowid-alias column
- *(vtab)* reject an explicit duplicate rowid on a persistent vtab INSERT
- *(fts5)* accept whitespace around the `col : token` column filter
- *(alter)* RENAME COLUMN preserves CREATE text — A-rn4 complete
- *(alter)* DROP COLUMN preserves CREATE text like sqlite (A-rn4)
- *(alter)* ADD COLUMN appends verbatim text to the schema (A-rn4)
- *(alter)* RENAME TO preserves CREATE text like sqlite (A-rn4)
- *(constraint)* CHECK violation names the constraint, like sqlite
- *(constraint)* UNIQUE message names columns on WITHOUT ROWID too
- *(constraint)* UNIQUE violation names the offending columns

### Performance

- *(planner)* seek by rowid for the rowid/_rowid_/oid keyword aliases

### Testing

- contain fuzz-corruption temp files in a per-PID directory
- *(vdbe)* cover 3-table NATURAL/USING join chains
- *(vdbe)* cover t.* over RIGHT/FULL outer joins
- *(fts5)* harden sqlite-reads-graphite FTS5 — multi-leaf, porter, edits
- *(fts5)* verify byte-exact multi-column poslists vs sqlite (D2e-M2)
- *(fts5)* add varying-rowid pagination coverage; characterize pgsz edge
- *(fts5)* unified streaming segment writer (terms + spanning) (D2e-M2c)
- *(fts5)* verify byte-exact doclist-spanning leaf carry vs sqlite (D2e-M2c)
- *(fts5)* verify byte-exact multi-leaf segment pagination vs sqlite (D2e-M2c)
- *(fts5)* verify byte-exact %_data segment encoding vs sqlite (D2e-M2a)
- *(vdbe)* lock in B7b default-engine parity and rejection regressions

## [0.0.7](https://github.com/KarpelesLab/graphitesql/compare/v0.0.6...v0.0.7) - 2026-06-22

### Added

- *(fts5)* highlight() auxiliary function
- *(fts5)* bm25() per-column weights
- *(fts5)* bm25() relevance ranking and the rank column (D2d)
- *(fts5)* MATCH ^ anchor for column-initial tokens
- *(fts5)* MATCH NEAR(...) proximity groups
- *(fts5)* MATCH boolean operators (AND/OR/NOT) with parentheses
- *(fts5)* MATCH phrase and prefix queries
- *(fts5)* MATCH column-filter syntax (col:token)
- *(fts5)* MATCH full-text queries with a unicode61-style tokenizer
- *(fts5)* built-in fts5 module stores and retrieves documents
- *(parser)* MATCH and REGEXP operators desugar to function calls
- *(vtab)* VTabSchema can declare column types; rtree table_info matches sqlite
- *(api)* register user-defined aggregate functions from Rust (D4)
- *(api)* register user-defined scalar functions from Rust (D4)
- *(vtab)* built-in rtree spatial-index module (D3a)
- *(vtab)* persistent virtual tables via a backing table (W2)
- *(vtab)* honor an explicit rowid in a virtual-table INSERT (W1c)
- *(vtab)* route UPDATE and DELETE to a writable module (W1b)
- *(vtab)* writable virtual tables — INSERT via a module's update (W1a)
- *(pragma)* round-trip cache_size and report mmap_size like sqlite (C8b)
- *(json)* encode JSON5-form numbers under the JSONB INT5/FLOAT5 tags
- *(api)* add Connection::execute_batch for multi-statement scripts

### Documentation

- record FTS5 highlight() done
- record FTS5 bm25 ranking (D2d) done
- README details the complete FTS5 MATCH query language
- FTS5 MATCH now supports the full core query language
- record FTS5 D2a/D2b/D2c progress and built-in modules
- mark D4 aggregate UDFs done (window/collation remain)
- mark D4 scalar UDFs done (aggregate/window/collation remain)
- mark D3a (rtree module, correct results) done
- mark W2 (persistent shadow-table storage) done
- mark W1 (writable-vtab trait + DML routing) done
- mark B0b-iii single-index case done (ORDER BY after a WHERE seek)
- mark C8b done (cache_size round-trip, mmap_size no-rows)
- mark A8 done (JSONB INT5/FLOAT5 for JSON5-form numbers)
- mark A3b fully done (partial/expression index range + IN seeks)
- mark A3b range-seek half done (partial/expression index ranges)
- update planner-leftovers ordering (B0b-i/ii done; B0b-iii needs shared seek-choice helper)
- mark B0b-ii (covered-query covering scan) done in ROADMAP
- mark B0b-i done; note mixed-direction partial-sort + B0b-ii/iii nuances
- mark A-rn1/A-rn2, A4, A7, A2 + composite eq-prefix range seek done in ROADMAP
- refresh README status — WAL writes, WITHOUT ROWID, auto_vacuum, JSONB, vtabs are done

### Fixed

- *(vtab)* VACUUM and foreign_key_list handle virtual tables
- *(pragma)* integrity_check skips virtual tables instead of erroring
- *(drop)* DROP TABLE on a persistent vtab removes its backing table
- *(alter)* handle ALTER and CREATE INDEX on a virtual table
- *(pragma)* table_info over a virtual table lists its columns, not an error
- *(eqp)* EXPLAIN QUERY PLAN over a virtual table no longer errors
- *(json)* json_each/json_tree honor the optional path argument
- *(alter)* rewrite dependent view bodies on RENAME TABLE

### Performance

- *(planner)* report mixed-direction partial sorts like sqlite (B0b-i)
- *(planner)* ORDER BY satisfied by a leading-column range seek (B0b-iii)
- *(planner)* skip the sort when a WHERE-seek already orders by an index suffix
- *(planner)* seek partial and expression indexes for IN-list queries
- *(planner)* seek partial and expression indexes for range queries
- *(planner)* answer a covered query with a covering-index scan
- *(planner)* satisfy a multi-term ORDER BY from a composite index prefix
- *(planner)* seek a composite index's equality prefix plus a trailing range

## [0.0.6](https://github.com/KarpelesLab/graphitesql/compare/v0.0.5...v0.0.6) - 2026-06-21

### Added

- *(planner)* range-seek a secondary index on a WITHOUT ROWID table
- *(planner)* seek a secondary index on a WITHOUT ROWID table
- *(planner)* seek a WITHOUT ROWID PRIMARY KEY in joins
- *(planner)* range-seek a WITHOUT ROWID PRIMARY KEY
- *(planner)* seek a WITHOUT ROWID table's PRIMARY KEY instead of scanning
- *(planner)* report unindexed equi-joins as an automatic covering index
- *(json)* implement the JSONB binary-JSON family
- *(pragma)* table_info over a view; keep type parameters
- *(func)* add unistr(), unistr_quote() and subtype()
- *(func)* implement random() and randomblob(); cap blob-builders at 1e9
- *(pragma)* report SQLite defaults for unexposed tuning getters
- *(cli)* render EXPLAIN QUERY PLAN as SQLite's QUERY PLAN tree
- *(exec)* LIMIT/OFFSET may be a subquery or contain one
- *(parser)* redundant FROM parens, schema-qualified PRAGMA, WITH+INSERT
- *(func)* add the soundex(X) scalar function
- *(exec)* reject sqlite-invalid statements; deferred foreign keys
- *(pager)* PRAGMA incremental_vacuum(N) for auto_vacuum=INCREMENTAL (C6b-4)
- *(parser)* postfix 'expr NOT NULL' operator
- *(parser)* accept CTE 'AS [NOT] MATERIALIZED' hints
- *(pager)* FULL auto_vacuum commit-time truncation (compaction)
- *(parser)* string-literal aliases and empty CAST type name
- *(vtab)* best_index constraint pushdown for virtual tables (D1b)
- *(json)* end-relative paths and strict error semantics for json1
- *(datetime)* support the subsec/subsecond modifier
- *(pager)* write auto_vacuum databases with maintained pointer maps (C6b-2)
- *(vdbe)* grouped HAVING and aggregate ORDER BY on the VDBE path (B6)
- *(json)* accept JSON5 input in the JSON functions
- *(vtab)* CREATE VIRTUAL TABLE + executor integration (D1b)
- *(pager)* create empty auto_vacuum databases (C6b-1)
- *(vtab)* virtual-table module trait + registry (D1a)
- *(planner)* covering-index reads on WHERE-driven index seeks (B2b)

### Documentation

- prune completed work from ROADMAP and split remaining into smaller steps

### Fixed

- *(ddl)* propagate RENAME COLUMN into the table's own expressions
- *(ddl)* DROP TABLE drops the table's triggers
- *(json)* preserve a strict JSON number's source text
- *(parser)* name unaliased result columns after their verbatim source
- *(pragma)* correct table_info pk ordinal and index_list pk origin
- *(printf)* cap float conversions at 16 significant digits like SQLite
- *(shell)* print BLOB/TEXT as raw bytes to first NUL; length() stops at NUL
- *(planner)* EXPLAIN QUERY PLAN reports implicit autoindex seeks (B-track)
- *(cli)* split trigger BEGIN...END bodies correctly; print RETURNING rows
- *(exec)* UPDATE assignments are simultaneous; parse row-value SET
- *(cli)* route EXPLAIN and WITH-prefixed DML to the right method
- *(eval)* empty IN () list is false even for a NULL left operand
- *(func)* blob C-string coercion, unhex ignore-set, char() out-of-range
- *(exec)* grouped output is ordered by the GROUP BY keys
- *(eval)* apply comparison affinity in IN/BETWEEN and to bare rowid
- *(exec)* dedup compound (UNION/INTERSECT/EXCEPT) output is sorted
- *(exec)* json_group_array(DISTINCT x) dedupes its values
- *(exec)* LIMIT/OFFSET require an integer value (OP_MustBeInt)
- *(exec)* statement atomicity, conflict resolution, and RAISE() in triggers
- *(math)* match sqlite3 on scalar math accuracy and overflow edges
- *(exec)* positional GROUP BY resolves to the output column; generate_series step 0
- *(datetime)* strict value parsing and modifier edge cases
- *(cli)* route PRAGMA setters through execute so they take effect
- *(printf)* alt-form flags, integer precision, and edge-case panics
- *(exec)* arity guard for aggregates called with too few arguments
- *(window)* honor the group_concat/string_agg separator argument
- *(eval)* raw-byte blob concatenation, GLOB leading ], and overflow panics
- *(func)* NULLIF collation, substr/min/max panics, and scalar-function edges
- *(planner)* resolve ORDER BY output alias through a COLLATE/paren wrapper
- *(datetime)* NULL on unknown strftime specifier, add %U, bound year at 9999
- *(eval)* honor explicit COLLATE inside IN, CASE, and BETWEEN
- match sqlite for real text rendering and numeric CAST prefixes
- *(json)* json_valid(X) validates strict RFC-8259, not JSON5
- *(reader)* never panic on malformed databases or SQL

### Performance

- *(planner)* seek/hash comma joins with the equality in WHERE
- *(planner)* use partial and expression indexes for equality seeks (A3)
- *(planner)* seek the inner join table by a secondary index (B1a²)
- *(planner)* seek the inner join table by rowid (B1a)

### Testing

- *(dml)* don't differentially test DELETE/UPDATE...LIMIT (CI sqlite lacks it)
- *(fk)* composite and self-referential foreign-key enforcement cases

## [0.0.5](https://github.com/KarpelesLab/graphitesql/compare/v0.0.4...v0.0.5) - 2026-06-20

### Added

- implement timediff(A, B) scalar function (roadmap A5)

### Fixed

- *(test)* make connection mutable in having_no_group test

### Other

- changelog + roadmap for timediff (A5) and CREATE TEMP VIEW/TRIGGER (C-ms1)
- mkdir the sqlite3 install dir before unzip
- pin sqlite3 to 3.50.4 as the differential oracle
- Merge A5: timediff(A,B)
- changelog + roadmap for json_error_position (A6) and count(*) covering (B2b)
- Merge A6: json_error_position(X)
- Phase A6: implement json_error_position(X) scalar function
- prune completed roadmap items, split/sharpen pending ones
- Add json_pretty(X [, indent])
- Add CURRENT_DATE / CURRENT_TIME / CURRENT_TIMESTAMP keywords
- Support bare pragma_<name> table-valued functions (no parens)
- Add sqlite_version() scalar function
- Add UPDATE OR IGNORE/REPLACE/ABORT conflict clauses
- Add if() as an alias for iif(), and the 2-arg form
- Fix NATURAL JOIN / JOIN USING (silent cross-join bug)
- B2 (planner): covering-index reads for the ordered index scan
- B0 (planner): satisfy ORDER BY from a secondary index
- B0 (planner): skip the sort for ORDER BY rowid / INTEGER PRIMARY KEY
- make Track-B planner gaps concrete + testable
- Add PRAGMA collation_list
- Add PRAGMA table_list
- auto_vacuum awareness — read, report, and refuse-to-corrupt
- Track C: cross-database savepoints
- Track C: cross-database transactions (completes the multi-schema track)
- Track C: cross-database view reads + qualified CREATE VIEW
- Track C: qualified CREATE TRIGGER (aux.tr) + bare-SQL strip helper
- Track C: qualified CREATE INDEX (aux.idx / temp.idx)
- Track C: qualified ALTER TABLE (aux.t / temp.t)
- Track C: WITHOUT ROWID cross-database reads
- Track C: cross-database joins + 3-part column names
- mark C5 done — ATTACH multi-schema track (C1-C5) complete
- ATTACH 'file.db' AS x (cross-engine file databases)
- TEMP tables (separate temp database with unqualified shadowing)
- note C4 (TEMP) design approach in roadmap
- mark C1-C3 (ATTACH multi-schema) done in roadmap + changelog
- C3 (write path): cross-database CREATE/INSERT/UPDATE/DELETE/DROP
- C3 (read path): schema-qualified table reads via materialization
- ATTACH ':memory:' AS x / DETACH x
- multi-database registry + PRAGMA database_list
- restructure ROADMAP — condense done work, expand tracks into shippable pieces
- changelog for transaction/DDL state checks
- Reject RENAME COLUMN onto existing name; broaden RENAME TABLE collision check
- Validate transaction state; fix DROP error messages
- changelog for CREATE validations, % operator, string_agg
- Add CREATE TABLE validations (dup column, multi-PK, bad PK/UNIQUE col, AUTOINCREMENT)
- Reject a table with no non-generated column
- Fix % operator to truncate operands to integers (SQLite semantics)
- Add string_agg as an alias for group_concat
- Add json_group_array / json_group_object aggregates
- changelog for collation propagation fixes
- Apply collation to compound set ops (UNION/INTERSECT/EXCEPT) and their ORDER BY
- Apply left-operand collation to IN (SELECT …)
- Apply collation to BETWEEN and CASE x WHEN y
- Apply collation to IN membership and min()/max()
- changelog for printf comma flag + length modifiers
- support the ',' thousands-grouping flag and l/ll length modifiers
- Resolve NEW.rowid / OLD.rowid in trigger bodies
- Honor UPDATE OF <columns> in trigger firing
- Resolve SELECT-list aliases in WHERE/GROUP BY/HAVING
- Reject CTE explicit column list with mismatched count
- changelog for VALUES compound-operand fix
- Fix multi-row VALUES as a compound-query operand
- Guard UPDATE … FROM on WITHOUT ROWID tables; roadmap note
- Support UPDATE … SET … FROM <sources>
- changelog for recursive-CTE LIMIT, HAVING, dflt_value fixes
- Honor LIMIT/OFFSET on a recursive CTE
- PRAGMA table_info: dflt_value is the default expression's SQL text
- Allow HAVING without GROUP BY
- changelog for qualified rowid and star-with-aggregation
- Resolve table-qualified rowid aliases (t.rowid / t._rowid_ / t.oid)
- Support '*' / table.* mixed with aggregates
- Support INSERT … SELECT
- changelog for schema catalog, ADD COLUMN, CHECK-subquery fixes
- Enforce SQLite's ALTER TABLE ADD COLUMN restrictions
- Make the schema catalog queryable as sqlite_schema / sqlite_master
- Reject subqueries in CHECK constraints and generated columns
- changelog entries for overflow/literal/inf-nan fixes
- Reject inf/nan words in text→number coercion (match SQLite)
- Fold -9223372036854775808 literal to Integer(i64::MIN)
- Match SQLite: sum()/abs() integer overflow is an error
- Add STRICT tables (CREATE TABLE … STRICT)
- Enforce UNIQUE on standalone indexes (plain/partial/expression)
- Support PRAGMA table-valued functions in FROM
- Inf prints as Inf in text output, 9.0e+999 only in quote()
- Print infinities as ±9.0e+999 and map NaN arithmetic to NULL
- Persist writable PRAGMA user_version / application_id
- Add last_insert_rowid(), changes(), and total_changes()
- Add printf '*' width/precision, unhex(x,ignore), and sign() NULL semantics
- Support ORDER BY / LIMIT on DELETE and UPDATE
- Fix table_info notnull for rowid; add table_xinfo and index_xinfo
- Parse REINDEX as a no-op
- Add CREATE TEMP TABLE and ALTER TABLE DROP COLUMN
- Fix `IS TRUE`/`IS FALSE` truthiness and abs() of text
- Fix compound dedup to keep the last occurrence's representation
- Apply FILTER on window aggregates; reject DISTINCT windows
- mark frame EXCLUDE and RANGE value-offset done in ROADMAP
- Implement window frame EXCLUDE clause
- Implement RANGE window frames with value offsets
- Fix strftime %j off-by-one; add ISO week-date %G/%V/%g
- Allow HAVING to reference SELECT-output aliases
- Implement SQLite's bare-column min/max rule
- Parse sized / multi-word type names in CAST
- Fix printf %g notation, %f half-away rounding, and float sign flags
- Fix negative LIMIT to mean "no limit"
- Support `_` digit separators in numeric literals
- Validate and normalize date/time components
- Fix round() to round the true decimal value (half away from zero)
- Add correlated-subquery / EXISTS differential test
- Add distinct-aggregate and window-function differential test
- Fix substr() on blobs to slice bytes and return a blob
- Fix CAST: blob reinterpretation and NUMERIC text reduction
- Fix quote() blob format and CAST AS BLOB
- Add REAL formatting differential test
- Add mixed-affinity differential test
- Fix NUMERIC storage affinity: reduce integral reals to integers
- Phase 9: broaden differential corpus with scalar/date edge cases
- Phase 9: hash join for equi-join ON conditions
- Fix pre-comparison affinity: NONE column vs TEXT column
- Phase 9: EXPLAIN QUERY PLAN emits MULTI-INDEX OR
- Phase 9: broaden differential corpus over planner seek paths
- Phase 9: OR-by-union index optimization
- Phase 9: rowid range scans over the table b-tree
- Phase 9: EXPLAIN QUERY PLAN reports range/IN index seeks
- Phase 9: IN-list driven index seeks
- Phase 9: index range scans (< <= > >= BETWEEN)
- Phase 9: add octet_length() and glob() scalar functions
- Phase 9: VDBE single-table GROUP BY
- Phase 9: VDBE whole-table aggregates
- Phase 9: VDBE SELECT DISTINCT
- Phase 9: VDBE ORDER BY via a sorter
- Phase 9: VDBE OFFSET on single-table scans
- Track B: VDBE LIMIT
- Track B: VDBE WHERE filtering
- Track B: VDBE table scans + Connection::query_vdbe
- Track B: VDBE CAST op; refresh module overview
- Track B: VDBE control flow (Goto/IfFalse) and CASE compilation
- Track B: extend VDBE IR with comparison and boolean ops
- Track B: VDBE bytecode IR spike (exec::vdbe)
- Track A: RIGHT and FULL OUTER JOIN
- Track A: LIKE ... ESCAPE, like() function form, likely/unlikely/likelihood
- Track C: in-engine PRAGMA integrity_check / quick_check
- Track D: json_each / json_tree table-valued functions
- Track D: table-valued functions — generate_series
- Track A: CREATE TABLE ... AS SELECT (CTAS)
- Track A: INDEXED BY / NOT INDEXED query hints
- Track A: ordered aggregates (group_concat(x ORDER BY y))
- Track A: percent_rank() and cume_dist() window functions
- Track A: expression indexes (CREATE INDEX ... (expr))
- Track A: named windows (WINDOW w AS ... / OVER w)
- Track C: PRAGMA foreign_key_check
- Track C: introspection PRAGMAs (index_list/info, foreign_key_list, ...)
- Track A: partial indexes (CREATE INDEX ... WHERE)
- Track A: VALUES as a statement and table source
- Track A: row-value IN (SELECT ...)
- Track A: aggregate FILTER (WHERE ...) clause
- Track C: SAVEPOINT / RELEASE / ROLLBACK TO nested transactions
- Track A: row-value expressions (=, ordering, IN)
- Track A: JSON ->/->> operators and json_set/insert/replace/remove/patch
- Track A: ORDER BY NULLS FIRST/LAST and IS [NOT] DISTINCT FROM
- Track C: VFS advisory-locking contract + writer serialization
- Track B: ANALYZE + sqlite_stat1 + cost-based index selection
- Track A: SQLite JSON functions (pure-core parser/serializer)
- Track A: SQLite math functions (pure-core, no libm)
- Track A: UPSERT (ON CONFLICT DO UPDATE/NOTHING) and RETURNING
- Track A: collating sequences (BINARY/NOCASE/RTRIM)
- Track A: generated columns (STORED / VIRTUAL)
- re-plan ROADMAP toward full SQLite parity
- Phase 9: b-tree page merging on delete (last roadmap item)

### Other

- **`timediff(A, B)`** scalar function — the calendar delta from B to A as
  `(+|-)YYYY-MM-DD HH:MM:SS.SSS`, a faithful port of SQLite's `date.c`
  algorithm (byte-identical across 2000+ randomized pairs incl. leap days,
  month boundaries, and sub-second; NULL/invalid → NULL).
- **`CREATE TEMP VIEW` / `CREATE TEMP TRIGGER`** now live in the `temp` catalog
  (`sqlite_temp_master`), not `main`'s `sqlite_master`, matching sqlite — a temp
  view resolves via the temp catalog (shadowing main) and a temp trigger fires on
  writes to its table (including a `main` table). `CREATE TEMP INDEX` already did.
- **`json_error_position(X)`** scalar function — the 1-based byte position of the
  first JSON syntax error (0 if valid, NULL for NULL). Matches sqlite3 on
  well-formed JSON and the common structural-error shapes; JSON5 inputs sqlite
  accepts (unquoted keys, trailing commas) diverge, since graphite's JSON parser
  is strict RFC-8259.
- **Planner (B2b): `SELECT count(*)` via a covering index.** A bare `count(*)`
  over a single rowid table with exactly one full (non-partial) secondary index
  now counts the index's entries instead of scanning the table, and `EXPLAIN
  QUERY PLAN` reports `SCAN t USING COVERING INDEX <name>` (matching sqlite).
  Conservative: any WHERE/GROUP BY/HAVING/DISTINCT/join, a WITHOUT ROWID table,
  or zero/multiple candidate indexes falls back to the plain table scan.

- **`json_pretty(X [, indent])`** -- reformats JSON with indentation (default 4
  spaces; empty arrays/objects and scalars stay compact), byte-compatible with
  sqlite3.
- **`CURRENT_DATE` / `CURRENT_TIME` / `CURRENT_TIMESTAMP`** keywords now evaluate
  (UTC), equivalent to `date`/`time`/`datetime('now')` -- in expressions and as
  column `DEFAULT`s. A quoted `"current_date"` stays an identifier.
- **Bare `pragma_<name>` table-valued functions** (no parentheses) now work as
  a FROM source (e.g. `SELECT name FROM pragma_database_list`), the zero-argument
  form, matching sqlite; a real table/view/CTE of the same name still shadows it.
- **`sqlite_version()`** scalar function -- returns the SQLite release graphitesql
  tracks and writes into new file headers (`3.53.2`).
- **`UPDATE OR IGNORE/REPLACE/ABORT/ROLLBACK/FAIL`** conflict clauses are now
  parsed and honored: `OR IGNORE` skips a row whose update would violate a
  UNIQUE/NOT NULL/CHECK constraint, `OR REPLACE` deletes the conflicting rows
  first, and `OR ABORT`/`ROLLBACK`/`FAIL` (and the default) fail the statement --
  matching sqlite.
- **`if(...)`** is now accepted as SQLite's alias for `iif(...)`, and both
  accept the 2-argument form (`if(cond, x)` -> `x` or NULL).
- Fix: **`NATURAL JOIN` and `JOIN … USING (…)`** now work. `NATURAL` was parsed
  as a table alias (`FROM t AS natural JOIN …`), silently turning a natural join
  into a cross join with wrong results; both forms now join on equality of the
  common / named columns and coalesce each into a single output column
  (`COALESCE(left, right)` for outer joins), keeping it in its left position and
  referenceable unqualified — matching sqlite. A `NATURAL` join with no common
  column degrades to a cross join, and a `USING` column absent from either side
  is an error, both as in sqlite.
- **Planner (B0): index-driven `ORDER BY`.** A single-table full scan whose sole
  `ORDER BY` term is satisfied by a scan order now skips the sort: the rowid /
  INTEGER PRIMARY KEY case uses the table b-tree (`SCAN t`), and a column that is
  the leading column of a full (non-partial) index whose collation matches scans
  that index in key order (`SCAN t USING INDEX …`). `DESC` reverses (index
  NULLs-first → NULLs-last, matching `ORDER BY … DESC`). `EXPLAIN QUERY PLAN`,
  the scan, and the skipped sort share one planner decision so they stay in
  lockstep; verified against sqlite incl. NULLs, ties, `DESC`, `LIMIT`, and a
  NOCASE index. Conservative: any `WHERE`/grouping/aggregate/window/`DISTINCT`,
  `COLLATE`, extra ORDER BY terms, partial/expression index, or a shadowing
  column falls back to the in-memory sort.
- **Planner (B2): covering-index reads.** When the index satisfying an ordered
  scan also holds every column the query references (an indexed column or the
  rowid), rows are built directly from the index records and the table b-tree is
  not touched; `EXPLAIN QUERY PLAN` reports `USING COVERING INDEX` (else plain
  `USING INDEX`), matching sqlite. Coverage detection is conservative — a
  wildcard over a non-covered column, an expression/function/subquery result
  column, or any generated column on the table disables it.
- **`PRAGMA collation_list`** lists the three built-in collating sequences (BINARY/NOCASE/RTRIM).
- **`PRAGMA table_list [(name)]`**: one row per table/view across main + temp +
  attached databases — `(schema, name, type, ncol, wr, strict)` — plus each
  database's synthetic schema table (`temp` always listed, matching sqlite).
  View `ncol` is the output-column count; `wr`/`strict` flag WITHOUT ROWID /
  STRICT tables. Row set verified equal to the sqlite3 CLI.
- Track C6a: **`auto_vacuum` awareness.** `PRAGMA auto_vacuum` now reports the
  database's mode (0 = NONE, 1 = FULL, 2 = INCREMENTAL). graphite reads
  `auto_vacuum` databases created by sqlite3 (pointer-map pages are skipped
  naturally; `integrity_check` ok), but — because it does not yet maintain
  ptrmap pages — refuses to *write* one (`Unsupported`) rather than corrupt its
  pointer map, and rejects `PRAGMA auto_vacuum=FULL|INCREMENTAL`. Ordinary
  (`auto_vacuum=NONE`) databases are unaffected.
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
  nested views) in the view's own database. **Cross-database transactions**:
  `BEGIN … COMMIT/ROLLBACK` spanning main + temp + attached databases
  commits/rolls back them together (data and DDL); file attachments persist on
  commit and leave no trace on rollback. The multi-schema track is complete.
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
