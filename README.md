# graphitesql

A pure, safe, `no_std`-capable Rust re-implementation of **SQLite**, as a single
crate, aiming for **byte-for-byte compatibility with the SQLite 3 database file
format**.

> **Status: early scaffolding.** The on-disk-format foundations (varints,
> database header, value/serial-type model) are implemented and tested against
> real SQLite output. The full build plan is in **[ROADMAP.md](ROADMAP.md)**.

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

The public API is still taking shape. Today you can use the format primitives:

```rust
use graphitesql::format::DatabaseHeader;

let bytes = std::fs::read("some.db")?;
let header = DatabaseHeader::parse(&bytes)?;
println!("page size = {}", header.page_size);
println!("pages     = {}", header.size_in_pages);
# Ok::<(), graphitesql::Error>(())
```

The target end-state API (subject to change) looks like:

```rust,ignore
use graphitesql::Connection;

let mut db = Connection::open(":memory:")?;
db.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)")?;
db.execute("INSERT INTO users(name) VALUES ('ada'), ('grace')")?;

let mut stmt = db.prepare("SELECT id, name FROM users WHERE id > ?")?;
for row in stmt.query([0])? {
    let (id, name): (i64, String) = row?;
    println!("{id}: {name}");
}
```

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

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option. (SQLite itself is public domain; see `ROADMAP.md` for the open
question of whether to mirror that with a public-domain dedication.)
