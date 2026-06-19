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

> **Status: read + write working, with broad SQL.** graphitesql opens real
> SQLite files (incl. WAL-mode reads), **creates databases and runs
> `CREATE TABLE`/`INSERT`/`UPDATE`/`DELETE` with transactions and secondary
> indexes** — and the databases it writes are opened by the real `sqlite3` CLI
> with `PRAGMA integrity_check = ok`. The SQL surface now covers joins,
> aggregates, `GROUP BY`/`HAVING`, compound queries, (recursive) CTEs,
> correlated subqueries & `EXISTS`, **window functions**, **date/time &
> `printf`**, **`EXPLAIN QUERY PLAN`**, **foreign keys** (`PRAGMA foreign_keys`),
> and **triggers** — all verified differentially against `sqlite3`. Still to
> come: WAL writes, `WITHOUT ROWID`, and more (see the roadmap). The full build
> plan and status is in **[ROADMAP.md](ROADMAP.md)**.

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
| `std`   | on      | std-file `Vfs`, `std::error::Error` impl |

Disable default features for `no_std`. An in-memory VFS (`:memory:`) is always
available, including on wasm.

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
