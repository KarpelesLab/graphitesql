// Web Worker that drives graphitesql over OPFS.
//
// OPFS `FileSystemSyncAccessHandle`s are synchronous but *only* usable inside a
// Worker, and acquiring one is async — so this worker acquires a handle for each
// database file up front (main + `-journal` + `-wal`, the three files
// `Connection::{create,open}_vfs` touches) and hands the set to `Database.openOpfs`.
// From then on the Rust engine does fully synchronous I/O against them.
//
// Build the web package first:  wasm-pack build --target web --out-dir pkg-web
import init, { Database } from "../pkg-web/graphitesql_wasm.js";

let db = null;

async function acquireHandles(dbName) {
  const root = await navigator.storage.getDirectory();
  const files = {};
  for (const suffix of ["", "-journal", "-wal"]) {
    const name = dbName + suffix;
    const fh = await root.getFileHandle(name, { create: true });
    files[name] = await fh.createSyncAccessHandle();
  }
  return files;
}

self.onmessage = async (ev) => {
  const msg = ev.data;
  try {
    switch (msg.type) {
      case "open": {
        await init(); // instantiate the wasm module
        const files = await acquireHandles(msg.name);
        // Auto-detect: an empty main file means this is a fresh database.
        const isNew = files[msg.name].getSize() === 0;
        db = Database.openOpfs(files, msg.name, isNew);
        self.postMessage({ id: msg.id, ok: true, created: isNew });
        break;
      }
      case "exec": {
        const changes = db.exec(msg.sql);
        self.postMessage({ id: msg.id, ok: true, changes });
        break;
      }
      case "query": {
        const result = db.query(msg.sql);
        self.postMessage({ id: msg.id, ok: true, result });
        break;
      }
      default:
        throw new Error(`unknown message type: ${msg.type}`);
    }
  } catch (e) {
    self.postMessage({ id: msg.id, ok: false, error: String((e && e.message) || e) });
  }
};
