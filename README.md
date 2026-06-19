# graphitesql

A pure, safe, `no_std`-capable Rust re-implementation of **SQLite**, as a single
crate, aiming for **byte-for-byte compatibility with the SQLite 3 database file
format**.

> **Status: read engine working.** graphitesql opens real SQLite files and runs
> `SELECT` queries — table/index b-trees, overflow pages, the schema catalog, a
> SQL parser, and an expression/aggregate executor are implemented and tested
> against databases produced by `sqlite3`. Writing (the b-tree writer, journal,
> WAL) is next. The full build plan and status is in **[ROADMAP.md](ROADMAP.md)**.

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

Today you can open a real SQLite database and query it (read-only):

```rust,ignore
use graphitesql::{Connection, Value};

let db = Connection::open("app.db")?; // a file written by sqlite3
let result = db.query("SELECT id, name FROM users WHERE id > 1 ORDER BY name")?;

for row in &result.rows {
    if let (Value::Integer(id), Value::Text(name)) = (&row[0], &row[1]) {
        println!("{id}: {name}");
    }
}

// Aggregates, GROUP BY, expressions, scalar functions all work:
let r = db.query("SELECT count(*), max(id) FROM users")?;
```

The write path (`CREATE`/`INSERT`/`UPDATE`/`DELETE`, `:memory:` creation) is
under construction — see the roadmap. Low-level format primitives are also
public (`graphitesql::format::DatabaseHeader`, `graphitesql::btree`, …).

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
