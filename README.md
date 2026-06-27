# graphitesql

[![CI](https://github.com/KarpelesLab/graphitesql/actions/workflows/ci.yml/badge.svg)](https://github.com/KarpelesLab/graphitesql/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/graphitesql.svg)](https://crates.io/crates/graphitesql)
[![docs.rs](https://img.shields.io/docsrs/graphitesql)](https://docs.rs/graphitesql)
[![License: blessing](https://img.shields.io/badge/license-blessing%20(public%20domain)-blue.svg)](LICENSE)
![no_std](https://img.shields.io/badge/no__std-yes-success.svg)
![MSRV](https://img.shields.io/badge/rustc-1.88+-blue.svg)

A pure, safe, `no_std`-capable Rust re-implementation of **SQLite**, as a single
crate, aiming for **byte-for-byte compatibility with the SQLite 3 database file
format**.

> **Status: read + write working, with a broad SQL engine**, verified
> differentially against the real `sqlite3` CLI (a 1,600+ query corpus plus 260+
> focused suites). graphitesql opens real SQLite files and **creates databases**
> that `sqlite3` opens with `PRAGMA integrity_check = ok`. `SELECT` executes
> through a register-machine **VDBE engine by default**, falling back to the
> tree-walker for shapes it does not yet compile.

**What works** (full capability list and forward plan in **[ROADMAP.md](ROADMAP.md)**):

- **Storage** — rowid and `WITHOUT ROWID` tables; secondary/`UNIQUE`/partial/
  expression indexes; overflow pages; `VACUUM` (+ `VACUUM INTO`); the full
  **`auto_vacuum`** track (incl. `incremental_vacuum`); **WAL read *and* write**;
  a **SQLite-format rollback journal** so a crash mid-write is recoverable by
  `sqlite3` (and vice-versa) — fault-injection crash-recovery tested across both
  the rollback-journal and WAL paths; and a **bounded LRU page cache**
  (`cache_size`).
- **SQL** — `INNER`/`LEFT`/`RIGHT`/`FULL`/`NATURAL`/`USING` joins; aggregates,
  `GROUP BY`/`HAVING`, compound queries, (recursive) **CTEs**, correlated
  subqueries & `EXISTS`, **window functions**; UPSERT, `RETURNING`, `STRICT`
  tables, generated columns; **triggers**, **foreign keys**, and **ATTACH / TEMP**
  multi-schema.
- **Functions & planning** — date/time, `printf`, math, and **JSON + JSONB**; an
  index-driven planner with **`EXPLAIN QUERY PLAN`** matching sqlite (plain
  `EXPLAIN` lists the compiled bytecode).
- **Virtual tables** — built-in `series`, **`rtree`** (queries prune the node
  tree by coordinate bounds), and **`fts5`** (full-text `MATCH` with phrases/
  prefixes/column filters/`NEAR`/`^` anchors, `bm25()`/`rank` ordering,
  `highlight()`, `fts5vocab`) — common `MATCH` shapes (bare term, column-scoped,
  two-term phrase, two-operand boolean) are now answered from the **inverted
  index** instead of scanning every document; the read-only `dbstat` and
  `sqlite_dbpage` (raw page bytes) tables; and `register_module` /
  `register_function` for your own.
- **Byte-compatible on disk** — R-Tree (`_node`) and FTS5 (sqlite's
  `_content`/`_data`/`_idx`/`_docsize`/`_config` shadow tables) round-trip through
  stock `sqlite3`, which opens, `MATCH`es, and integrity-checks them. FTS5 folds
  diacritics exactly like `unicode61` and honors the full `tokenize=` option set
  (`remove_diacritics 0|1|2`, `porter`, `ascii`, `tokenchars`/`separators`).

## Why

SQLite is the most-deployed database in the world, but it's C. graphitesql brings
the same file format and SQL dialect to places where a **safe, dependency-free,
`no_std` Rust** library shines:

- **WebAssembly** — run a real SQLite-compatible database in the browser or in
  a wasm sandbox with no JS shim and no Emscripten.
- **Embedded / bare-metal** — `no_std` + `alloc`, bring-your-own storage.
- **Sandboxed / capability-based hosts** — no `unsafe`, no FFI, no syscalls
  except through a `Vfs` trait you control.

## Goals

- ✅ **File-format compatible.** Open a database written by `sqlite3`; write one
  `sqlite3` can open. Verified with differential tests against the C library.
- ✅ **Safe.** `#![forbid(unsafe_code)]` across the whole crate.
- ✅ **Portable.** `#![no_std]` + `alloc`. Optional `std` feature for real files.
- ✅ **Single crate.** Storage, B-tree, SQL parser, and VM all live in
  `graphitesql`.
- ✅ **No dependencies.** Only `core` and `alloc`.

## Non-goals (at least initially)

- Being a faster SQLite. Correctness and compatibility first.
- 100% of every SQLite extension (FTS5, R-Tree, sessions, …). These are layered
  in later, behind features. See the roadmap.
- Drop-in C ABI (`libsqlite3.so`). A C-API shim is a possible future crate, not
  the core.

## Usage

Create a database, write to it, and read it back — and `sqlite3` can open it too:

```rust,ignore
use graphitesql::{Connection, Value};

let mut db = Connection::open_memory()?;            // or Connection::create("app.db")?
db.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)")?;
db.execute("INSERT INTO users(name) VALUES ('ada'), ('grace')")?;
db.execute("UPDATE users SET name = 'Ada Lovelace' WHERE id = 1")?;

let result = db.query("SELECT id, name FROM users ORDER BY name")?;
for row in &result.rows {
    if let (Value::Integer(id), Value::Text(name)) = (&row[0], &row[1]) {
        println!("{id}: {name}");
    }
}

// Aggregates, GROUP BY, joins, expressions, scalar functions, transactions:
db.query("SELECT u.name, sum(o.amount) FROM users u JOIN orders o \
          ON u.id = o.user_id GROUP BY u.name")?;
db.execute("BEGIN")?;
db.execute("DELETE FROM users WHERE id = 2")?;
db.execute("COMMIT")?;
```

Open an existing `sqlite3`-written file with `Connection::open("file.db")` (or
`open_readonly`). Low-level format primitives are public too
(`graphitesql::format::DatabaseHeader`, `graphitesql::btree`, …).

## Command-line shell

The crate ships a `graphitesql` binary modeled on the `sqlite3` CLI:

```sh
cargo run --bin graphitesql                 # in-memory, interactive
cargo run --bin graphitesql -- app.db       # open/create app.db, interactive
cargo run --bin graphitesql -- app.db "SELECT * FROM users;"   # one-shot
```

It accepts `;`-terminated SQL (multi-line) and dot-commands: `.tables`,
`.schema [table]`, `.headers on|off`, `.help`, `.quit`. Results print in
SQLite's default `|`-separated list mode.

## Feature flags

| feature | default | effect |
|---------|---------|--------|
| `std`     | on  | std-file `Vfs`, `std::error::Error` impl |
| `fts5`    | on  | built-in FTS5 full-text search (`MATCH`, `bm25()`/`rank`, `highlight()`) |
| `unicode` | off | `upper()`/`lower()` fold the full Unicode range (`café` → `CAFÉ`); the default folds ASCII only, exactly like stock `sqlite3` |

Disable default features for `no_std`. An in-memory VFS (`:memory:`) is always
available, including on wasm. Drop `fts5` (e.g. `--no-default-features --features
std`) to build without full-text search. Enable `unicode` for full-Unicode
case-folding (the default matches stock `sqlite3`'s ASCII-only `upper()`/`lower()`).

## Building & testing

```sh
cargo test                                   # full suite (std)
cargo build --no-default-features            # no_std build
cargo clippy --all-targets                   # lints (unsafe is forbidden)
```

## Reference material & attribution

graphitesql is an independent re-implementation. It uses SQLite's public-domain
source and documentation purely as a **specification reference** — no SQLite code
is compiled into this crate. Fetch the (git-ignored, hash-verified) reference
tree with:

```sh
./reference/fetch.sh
```

Deep gratitude to D. Richard Hipp and the SQLite developers. See
[`NOTICE`](NOTICE) and [`ATTRIBUTION.md`](ATTRIBUTION.md).

## License

**Public domain**, mirroring SQLite. In place of a legal notice, graphitesql
carries a blessing — see [`LICENSE`](LICENSE). The SPDX identifier is
`blessing` (the SQLite Blessing).
