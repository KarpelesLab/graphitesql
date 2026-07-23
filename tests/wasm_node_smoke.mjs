// Node smoke test for the in-memory path of graphitesql's `wasm` feature.
// Build first: wasm-pack build --target nodejs --features wasm (outputs pkg/).
// (OPFS is browser-only; this exercises Database.new/exec/query/serialize/deserialize.)
import { Database } from "../pkg/graphitesql.js";

let failures = 0;
function check(name, cond) {
  if (cond) {
    console.log(`  ok   ${name}`);
  } else {
    console.log(`  FAIL ${name}`);
    failures++;
  }
}

const db = new Database();
db.exec("CREATE TABLE t(a INTEGER, b TEXT, c REAL, d BLOB)");
db.exec("INSERT INTO t VALUES (1, 'hello', 3.5, x'01ff'), (9007199254740993, NULL, 2.0, NULL)");

const r = db.query("SELECT a, b, c, d FROM t ORDER BY a");
check("two columns labels", JSON.stringify(r.columns) === JSON.stringify(["a", "b", "c", "d"]));
check("two rows", r.rows.length === 2);

// Row 1
check("int as number", r.rows[0][0] === 1);
check("text", r.rows[0][1] === "hello");
check("real", r.rows[0][2] === 3.5);
check("blob is Uint8Array", r.rows[0][3] instanceof Uint8Array);
check("blob bytes", r.rows[0][3][0] === 0x01 && r.rows[0][3][1] === 0xff);

// Row 2: integer beyond 2^53 must come back as a BigInt with no precision loss
check("big int as BigInt", typeof r.rows[1][0] === "bigint");
check("big int value", r.rows[1][0] === 9007199254740993n);
check("null", r.rows[1][1] === null);

// aggregate
const agg = db.query("SELECT count(*) n, sum(c) s FROM t");
check("count", agg.rows[0][0] === 2);
check("sum", agg.rows[0][1] === 5.5);

// serialize -> deserialize round trip
const image = db.serialize();
check("serialize produces bytes", image instanceof Uint8Array && image.length >= 4096);
check("serialize header", new TextDecoder().decode(image.slice(0, 15)) === "SQLite format 3");
const db2 = Database.deserialize(image);
const r2 = db2.query("SELECT count(*) FROM t");
check("deserialize round-trips rows", r2.rows[0][0] === 2);

// error propagation
let threw = false;
try {
  db.query("SELECT * FROM nope");
} catch (e) {
  threw = true;
  check("error message matches sqlite", String(e.message).includes("no such table: nope"));
}
check("bad query throws", threw);

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
