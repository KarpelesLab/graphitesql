# graphitesql-wasm

WebAssembly (browser) bindings for [graphitesql](../), a pure-Rust,
byte-compatible reimplementation of SQLite. Run a real SQLite-format database
entirely in the browser — in memory, or persisted to the **Origin-Private File
System (OPFS)**.

This is the ROADMAP **D6 (wasm)** track. It is a *separate* crate on purpose: the
core `graphitesql` crate is `#![forbid(unsafe_code)]`, `no_std`+alloc and
zero-dependency, while these JS bindings use `wasm-bindgen` / `js-sys` / `web-sys`
(which generate `unsafe` glue). The core is consumed with `default-features = false`.

## API

```ts
class Database {
  constructor();                                      // in-memory (:memory:)
  static openOpfs(files: object, path: string,
                  create: boolean): Database;         // persistent (Worker only)
  static deserialize(bytes: Uint8Array): Database;    // load a .sqlite image
  exec(sql: string): number;                          // DDL/DML -> rows changed
  query(sql: string): { columns: string[], rows: any[][] };
  serialize(): Uint8Array;                            // dump to a .sqlite image
}
```

Value mapping (SQLite → JS): `NULL`→`null`, `INTEGER`→`number` (or `bigint` when
`|n| ≥ 2^53`, so no precision is lost), `REAL`→`number`, `TEXT`→`string`,
`BLOB`→`Uint8Array`.

## Build

```sh
# Browser (ES modules), used by examples/index.html:
wasm-pack build --target web --out-dir pkg-web

# Node (for the smoke test):
wasm-pack build --target nodejs --out-dir pkg-node
node tests/node_smoke.mjs
```

## In-memory (works anywhere, including the main thread)

```js
import init, { Database } from "./pkg-web/graphitesql_wasm.js";
await init();
const db = new Database();
db.exec("CREATE TABLE t(a, b)");
db.exec("INSERT INTO t VALUES (1, 'hi')");
console.log(db.query("SELECT * FROM t").rows); // [[1, "hi"]]
```

## Persistent (OPFS)

OPFS `FileSystemSyncAccessHandle`s are synchronous — which is exactly what
graphitesql's synchronous VFS needs — but they are **only usable inside a Web
Worker**, and *acquiring* one is asynchronous. So the pattern is: the worker
acquires a sync-access handle for each of the three files a connection touches
(`name`, `name-journal`, `name-wal`) up front, then hands the set to `openOpfs`.
All subsequent I/O is synchronous Rust calling synchronous JS — no async rework
of the engine.

See [`examples/worker.js`](examples/worker.js) and
[`examples/index.html`](examples/index.html) for a complete, persistent demo
(insert a row, reload the page, the data is still there). Serve over http(s):

```sh
wasm-pack build --target web --out-dir pkg-web
python3 -m http.server   # then open http://localhost:8000/examples/
```

OPFS requires Chrome/Edge 108+ or Safari 17+. There is no OPFS in Node, so the
persistent path can only be exercised in a browser; the in-memory path is
covered by `tests/node_smoke.mjs`.

## License

MIT OR Apache-2.0, matching the workspace.
