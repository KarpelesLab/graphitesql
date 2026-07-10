# graphitesql-capi

A `libsqlite3`-compatible **C ABI** (subset) over [graphitesql](../), a pure-Rust,
byte-compatible reimplementation of SQLite. Link it like `libsqlite3` and drive
graphitesql's engine from C/C++ or any FFI that speaks the SQLite C API.

This is the ROADMAP **D7 (C-API shim)** track. It is a *separate* crate on purpose:
the core `graphitesql` crate is `#![forbid(unsafe_code)]`, `no_std`+alloc and
zero-dependency, while a C ABI needs `extern "C"`, raw pointers and `unsafe`. Same
shape as the `graphitesql-wasm` sibling.

## Implemented surface

| Area | Functions |
|------|-----------|
| Lifecycle | `sqlite3_open`, `sqlite3_open_v2`, `sqlite3_close`, `sqlite3_close_v2` |
| One-shot | `sqlite3_exec` (with row callback) |
| Prepared | `sqlite3_prepare_v2`/`v3`, `sqlite3_step`, `sqlite3_reset`, `sqlite3_clear_bindings`, `sqlite3_finalize`, `sqlite3_sql`, `sqlite3_db_handle` |
| Bind | `sqlite3_bind_int`/`int64`/`double`/`null`/`text`/`blob` |
| Parameters | `sqlite3_bind_parameter_count`/`name`/`index` (named `:x`/`@x`/`$x` + numbered `?N`) |
| Columns | `sqlite3_column_count`/`data_count`/`name`/`type`/`int`/`int64`/`double`/`text`/`blob`/`bytes` |
| Status | `sqlite3_errmsg`, `sqlite3_errcode`/`extended_errcode`, `sqlite3_errstr`, `sqlite3_changes`/`total_changes`, `sqlite3_last_insert_rowid`, `sqlite3_get_autocommit`, `sqlite3_busy_timeout`, `sqlite3_interrupt` |
| UDFs | `sqlite3_create_function` (scalar + aggregate), `sqlite3_user_data`, `sqlite3_aggregate_context`, `sqlite3_value_*`, `sqlite3_result_*` |
| Version | `sqlite3_libversion`, `sqlite3_libversion_number` (reports `3.50.4`) |
| Memory | `sqlite3_free` |

Result codes and datatype constants (`SQLITE_OK`, `SQLITE_ROW`, `SQLITE_DONE`,
`SQLITE_INTEGER`, …) match SQLite. See [`include/sqlite3.h`](include/sqlite3.h).

Prepared statements are emulated over graphitesql's materialized query model: a
`step` walks already-computed rows, which is behaviourally equivalent to SQLite's
incremental VDBE stepping for these entry points. Column metadata
(`column_count`/`column_name`) is available immediately after `prepare` for a
row-producer, as in SQLite.

`INSERT/UPDATE/DELETE … RETURNING` drives the row path (`step` → `SQLITE_ROW`),
detected structurally via the engine's parser so a "returning" inside a string is
not mistaken for the clause.

User-defined functions work, scalar and aggregate. A **scalar** registers with
`xFunc` set (read args with `sqlite3_value_*`, return via `sqlite3_result_*`); an
**aggregate** registers with `xStep`+`xFinal`, keeping per-group state in the
buffer from `sqlite3_aggregate_context` (a fresh accumulator per group). Both are
callable from SQL anywhere, including `WHERE` and `GROUP BY`.

**Not yet covered:** window UDFs, the `_v3` prepare flags, incremental BLOB I/O,
online backup, hooks/authorizer, and the UTF-16 entry points.

## Build & test

```sh
cargo build --release          # -> target/release/libgraphitesql_capi.{so,a}
tests/run.sh                   # builds, compiles tests/ctest.c against it, runs
```

`tests/ctest.c` drives the library exactly as a real libsqlite3 consumer would
(open → create → prepare/bind/step insert → query with a bound filter → column
readout → exec+callback → error path → blob round-trip) and checks every result.

## Use from C

```c
#include "sqlite3.h"   // graphitesql-capi/include/sqlite3.h

sqlite3 *db;
sqlite3_open(":memory:", &db);
sqlite3_exec(db, "CREATE TABLE t(a,b)", 0, 0, 0);

sqlite3_stmt *st;
sqlite3_prepare_v2(db, "INSERT INTO t VALUES(?1, ?2)", -1, &st, 0);
sqlite3_bind_int64(st, 1, 42);
sqlite3_bind_text(st, 2, "hi", -1, SQLITE_TRANSIENT);
sqlite3_step(st);           // SQLITE_DONE
sqlite3_finalize(st);
sqlite3_close(db);
```

```sh
cc myprog.c -Igraphitesql-capi/include \
   -Lgraphitesql-capi/target/release -lgraphitesql_capi -o myprog
```

## License

MIT OR Apache-2.0, matching the workspace.
