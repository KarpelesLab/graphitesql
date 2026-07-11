/*
** End-to-end C test for graphitesql-capi: drives the library exactly as a real
** libsqlite3 consumer would (open, exec, prepare/bind/step/column, finalize,
** close) and checks results. Compiled and run by tests/run.sh against both this
** shim and (when present) the real libsqlite3 to confirm behavioural parity.
*/
#include "sqlite3.h"
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <ctype.h>

static int failures = 0;
#define CHECK(name, cond) do { \
  if (cond) { printf("  ok   %s\n", name); } \
  else { printf("  FAIL %s\n", name); failures++; } \
} while (0)

static int count_cb(void *arg, int ncol, char **vals, char **names) {
  (void)ncol; (void)names;
  int *sum = (int *)arg;
  *sum += atoi(vals[0]);
  return 0;
}

/* A user-defined scalar: times_k(x) = x * k, where k is the app pointer. */
static void times_k(sqlite3_context *ctx, int argc, sqlite3_value **argv) {
  (void)argc;
  long long k = *(long long *)sqlite3_user_data(ctx);
  sqlite3_result_int64(ctx, sqlite3_value_int64(argv[0]) * k);
}

/* A UDF that concatenates its two text args. */
static void concat2(sqlite3_context *ctx, int argc, sqlite3_value **argv) {
  (void)argc;
  char buf[128];
  snprintf(buf, sizeof buf, "%s%s",
           (const char *)sqlite3_value_text(argv[0]),
           (const char *)sqlite3_value_text(argv[1]));
  sqlite3_result_text(ctx, buf, -1, SQLITE_TRANSIENT);
}

/* A user-defined aggregate: sum_sq(x) = sum of x*x, accumulated in per-group
   state obtained from sqlite3_aggregate_context. */
struct sumsq { long long total; };
static void sumsq_step(sqlite3_context *ctx, int argc, sqlite3_value **argv) {
  (void)argc;
  struct sumsq *st = (struct sumsq *)sqlite3_aggregate_context(ctx, sizeof(struct sumsq));
  long long x = sqlite3_value_int64(argv[0]);
  st->total += x * x;
}
static void sumsq_final(sqlite3_context *ctx) {
  struct sumsq *st = (struct sumsq *)sqlite3_aggregate_context(ctx, sizeof(struct sumsq));
  sqlite3_result_int64(ctx, st ? st->total : 0);
}

/* A no-op step with a mismatched final signature is an invalid combination. */
static void lone_step(sqlite3_context *c, int n, sqlite3_value **v) { (void)c; (void)n; (void)v; }

/* Custom collation: reverse of BINARY (memcmp negated). */
static int rev_collation(void *arg, int nx, const void *x, int ny, const void *y) {
  (void)arg;
  int n = nx < ny ? nx : ny;
  int c = memcmp(x, y, (size_t)n);
  if (c == 0) c = nx - ny;
  return -c; /* reverse */
}

/* Custom collation equal to NOCASE (ASCII case-insensitive). */
static int nocase_collation(void *arg, int nx, const void *x, int ny, const void *y) {
  (void)arg;
  const unsigned char *a = x, *b = y;
  int n = nx < ny ? nx : ny;
  for (int i = 0; i < n; i++) {
    int ca = toupper(a[i]), cb = toupper(b[i]);
    if (ca != cb) return ca - cb;
  }
  return nx - ny;
}

/* Update-hook accounting for the test below. */
static int uh_inserts = 0, uh_updates = 0, uh_deletes = 0;
static long long uh_last_rowid = 0;
static void update_cb(void *arg, int op, const char *db, const char *tbl, long long rowid) {
  (void)arg; (void)db; (void)tbl;
  if (op == SQLITE_INSERT) uh_inserts++;
  else if (op == SQLITE_UPDATE) uh_updates++;
  else if (op == SQLITE_DELETE) uh_deletes++;
  uh_last_rowid = rowid;
}

static int ch_commits = 0, rh_rollbacks = 0, ch_veto = 0;
static int commit_cb(void *arg) { (void)arg; ch_commits++; return ch_veto; }
static void rollback_cb(void *arg) { (void)arg; rh_rollbacks++; }

/* Authorizer: deny every write action (a read-only sandbox). */
static int deny_writes_cb(void *arg, int action, const char *a1, const char *a2,
                          const char *db, const char *trig) {
  (void)arg; (void)a1; (void)a2; (void)db; (void)trig;
  if (action == SQLITE_INSERT || action == SQLITE_UPDATE ||
      action == SQLITE_DELETE || action == SQLITE_DROP_TABLE ||
      action == SQLITE_CREATE_TABLE)
    return SQLITE_DENY;
  return SQLITE_OK;
}

int main(void) {
  sqlite3 *db = NULL;
  int rc = sqlite3_open(":memory:", &db);
  CHECK("open :memory:", rc == SQLITE_OK && db != NULL);

  rc = sqlite3_exec(db,
      "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB)",
      NULL, NULL, NULL);
  CHECK("create table", rc == SQLITE_OK);

  /* Parameterized inserts via prepare/bind/step. */
  sqlite3_stmt *ins = NULL;
  rc = sqlite3_prepare_v2(db, "INSERT INTO t(name, score) VALUES(?1, ?2)", -1, &ins, NULL);
  CHECK("prepare insert", rc == SQLITE_OK && ins != NULL);

  const char *names[] = {"alice", "bob", "carol"};
  double scores[]     = {1.5,     2.25,  3.0};
  for (int i = 0; i < 3; i++) {
    sqlite3_reset(ins);
    sqlite3_clear_bindings(ins);
    sqlite3_bind_text(ins, 1, names[i], -1, SQLITE_TRANSIENT);
    sqlite3_bind_double(ins, 2, scores[i]);
    rc = sqlite3_step(ins);
    CHECK("insert step -> DONE", rc == SQLITE_DONE);
  }
  CHECK("last_insert_rowid == 3", sqlite3_last_insert_rowid(db) == 3);
  sqlite3_finalize(ins);

  /* Query back with a bound filter. */
  sqlite3_stmt *sel = NULL;
  rc = sqlite3_prepare_v2(db,
      "SELECT id, name, score FROM t WHERE score >= ?1 ORDER BY id", -1, &sel, NULL);
  CHECK("prepare select", rc == SQLITE_OK);
  sqlite3_bind_double(sel, 1, 2.0);

  CHECK("column_count == 3", sqlite3_column_count(sel) == 3);

  int rows = 0;
  long long ids = 0;
  char last_name[64] = {0};
  double last_score = 0;
  while ((rc = sqlite3_step(sel)) == SQLITE_ROW) {
    ids += sqlite3_column_int64(sel, 0);
    strncpy(last_name, (const char *)sqlite3_column_text(sel, 1), sizeof last_name - 1);
    last_score = sqlite3_column_double(sel, 2);
    CHECK("col0 type INTEGER", sqlite3_column_type(sel, 0) == SQLITE_INTEGER);
    CHECK("col1 type TEXT", sqlite3_column_type(sel, 1) == SQLITE_TEXT);
    CHECK("col2 type FLOAT", sqlite3_column_type(sel, 2) == SQLITE_FLOAT);
    rows++;
  }
  CHECK("select returned DONE", rc == SQLITE_DONE);
  CHECK("two rows matched (bob, carol)", rows == 2);
  CHECK("id sum 2+3", ids == 5);
  CHECK("last row name carol", strcmp(last_name, "carol") == 0);
  CHECK("last row score 3.0", last_score == 3.0);

  /* Column name introspection. */
  CHECK("column_name(1) == name", strcmp(sqlite3_column_name(sel, 1), "name") == 0);
  sqlite3_finalize(sel);

  /* UPDATE reports changes. */
  rc = sqlite3_exec(db, "UPDATE t SET score = score + 1", NULL, NULL, NULL);
  CHECK("update ok", rc == SQLITE_OK);
  CHECK("changes == 3", sqlite3_changes(db) == 3);

  /* exec + callback aggregation. */
  int total = 0;
  rc = sqlite3_exec(db, "SELECT count(*) FROM t", count_cb, &total, NULL);
  CHECK("exec callback ok", rc == SQLITE_OK);
  CHECK("callback saw count 3", total == 3);

  /* Error reporting. */
  char *emsg = NULL;
  rc = sqlite3_exec(db, "SELECT * FROM nope", NULL, NULL, &emsg);
  CHECK("bad query -> error", rc == SQLITE_ERROR);
  CHECK("errmsg mentions table", emsg && strstr(emsg, "no such table: nope"));
  sqlite3_free(emsg);

  /* sqlite3_error_offset: a syntax error reports the offending token's byte
   * offset (the repeated `=` in `===`); a resolution error reports -1. The shim
   * parses lazily, so the error surfaces at step. */
  const char *serr_sql = "SELECT 1 FROM t WHERE score === 1";
  sqlite3_stmt *serr = NULL;
  rc = sqlite3_prepare_v2(db, serr_sql, -1, &serr, NULL);
  CHECK("syntax error prepares (lazy)", rc == SQLITE_OK && serr != NULL);
  CHECK("syntax error -> ERROR at step", sqlite3_step(serr) == SQLITE_ERROR);
  int erroff = sqlite3_error_offset(db);
  CHECK("error_offset points at a '=' token", erroff >= 0 && serr_sql[erroff] == '=');
  sqlite3_finalize(serr);
  sqlite3_stmt *rerr = NULL;
  rc = sqlite3_prepare_v2(db, "SELECT nope FROM t", -1, &rerr, NULL);
  CHECK("resolution error prepares (lazy)", rc == SQLITE_OK && rerr != NULL);
  CHECK("no-such-column -> ERROR at step", sqlite3_step(rerr) == SQLITE_ERROR);
  CHECK("error_offset -1 for a resolution error", sqlite3_error_offset(db) == -1);
  sqlite3_finalize(rerr);

  /* Blob round-trip. */
  sqlite3_stmt *bstmt = NULL;
  sqlite3_prepare_v2(db, "SELECT ?1", -1, &bstmt, NULL);
  unsigned char raw[] = {0x00, 0x01, 0xff, 0x7f};
  sqlite3_bind_blob(bstmt, 1, raw, sizeof raw, SQLITE_TRANSIENT);
  CHECK("blob step -> ROW", sqlite3_step(bstmt) == SQLITE_ROW);
  CHECK("blob type", sqlite3_column_type(bstmt, 0) == SQLITE_BLOB);
  CHECK("blob length 4", sqlite3_column_bytes(bstmt, 0) == 4);
  const unsigned char *got = (const unsigned char *)sqlite3_column_blob(bstmt, 0);
  CHECK("blob bytes match", got && memcmp(got, raw, 4) == 0);
  sqlite3_finalize(bstmt);

  /* INSERT ... RETURNING drives the row path (step -> ROW), not just DONE. */
  sqlite3_stmt *ret = NULL;
  rc = sqlite3_prepare_v2(db,
      "INSERT INTO t(name, score) VALUES('dave', 9.0) RETURNING id, name", -1, &ret, NULL);
  CHECK("prepare insert-returning", rc == SQLITE_OK);
  CHECK("returning step -> ROW", sqlite3_step(ret) == SQLITE_ROW);
  CHECK("returning col count 2", sqlite3_column_count(ret) == 2);
  CHECK("returning id == 4", sqlite3_column_int64(ret, 0) == 4);
  CHECK("returning name dave", strcmp((const char *)sqlite3_column_text(ret, 1), "dave") == 0);
  CHECK("returning then DONE", sqlite3_step(ret) == SQLITE_DONE);
  CHECK("returning changed 1 row", sqlite3_changes(db) == 1);
  sqlite3_finalize(ret);

  /* The word "returning" inside a string literal must not trigger the row path
     (structural detection, not a text scan). */
  rc = sqlite3_exec(db, "INSERT INTO t(name) VALUES('returning home')", NULL, NULL, NULL);
  CHECK("'returning' in a string is a plain insert", rc == SQLITE_OK && sqlite3_changes(db) == 1);

  /* Named parameters: count, name<->index, and value binding by looked-up index. */
  sqlite3_stmt *np = NULL;
  sqlite3_prepare_v2(db, "SELECT :who, ?2, @n", -1, &np, NULL);
  CHECK("param count 3", sqlite3_bind_parameter_count(np) == 3);
  CHECK("param 1 name :who", strcmp(sqlite3_bind_parameter_name(np, 1), ":who") == 0);
  CHECK("param 2 anonymous", sqlite3_bind_parameter_name(np, 2) == NULL);
  CHECK("param 3 name @n", strcmp(sqlite3_bind_parameter_name(np, 3), "@n") == 0);
  CHECK("index of :who is 1", sqlite3_bind_parameter_index(np, ":who") == 1);
  CHECK("index of @n is 3", sqlite3_bind_parameter_index(np, "@n") == 3);
  CHECK("index of missing is 0", sqlite3_bind_parameter_index(np, ":nope") == 0);
  sqlite3_bind_text(np, sqlite3_bind_parameter_index(np, ":who"), "eve", -1, SQLITE_TRANSIENT);
  sqlite3_bind_int64(np, 2, 77);
  sqlite3_bind_int64(np, sqlite3_bind_parameter_index(np, "@n"), 88);
  CHECK("named step -> ROW", sqlite3_step(np) == SQLITE_ROW);
  CHECK("data_count 3", sqlite3_data_count(np) == 3);
  CHECK("named who=eve", strcmp((const char *)sqlite3_column_text(np, 0), "eve") == 0);
  CHECK("named ?2=77", sqlite3_column_int64(np, 1) == 77);
  CHECK("named @n=88", sqlite3_column_int64(np, 2) == 88);
  CHECK("out-of-range bind -> RANGE", sqlite3_bind_int64(np, 4, 0) == SQLITE_RANGE);
  sqlite3_finalize(np);

  /* User-defined scalar functions callable from SQL. */
  static long long k = 10;
  rc = sqlite3_create_function(db, "times_k", 1, SQLITE_UTF8, &k, times_k, NULL, NULL);
  CHECK("create_function times_k", rc == SQLITE_OK);
  rc = sqlite3_create_function(db, "concat2", 2, SQLITE_UTF8, NULL, concat2, NULL, NULL);
  CHECK("create_function concat2", rc == SQLITE_OK);

  sqlite3_stmt *ufn = NULL;
  sqlite3_prepare_v2(db, "SELECT times_k(5), concat2('foo','bar')", -1, &ufn, NULL);
  CHECK("udf step -> ROW", sqlite3_step(ufn) == SQLITE_ROW);
  CHECK("times_k(5) == 50", sqlite3_column_int64(ufn, 0) == 50);
  CHECK("concat2 == foobar", strcmp((const char *)sqlite3_column_text(ufn, 1), "foobar") == 0);
  sqlite3_finalize(ufn);

  /* A UDF used inside a WHERE clause over table rows. */
  sqlite3_stmt *ufw = NULL;
  sqlite3_prepare_v2(db, "SELECT count(*) FROM t WHERE times_k(id) <= 20", -1, &ufw, NULL);
  CHECK("udf-in-where step", sqlite3_step(ufw) == SQLITE_ROW);
  CHECK("times_k(id)<=20 matches id 1,2", sqlite3_column_int64(ufw, 0) == 2);
  sqlite3_finalize(ufw);

  /* User-defined aggregate over table rows: sum of id*id. `t` holds ids 1..5
     (alice/bob/carol, dave via RETURNING, 'returning home'), so 1+4+9+16+25=55.
     Cross-check against the builtin so the test is robust to earlier inserts. */
  rc = sqlite3_create_function(db, "sum_sq", 1, SQLITE_UTF8, NULL, NULL,
                               sumsq_step, sumsq_final);
  CHECK("create aggregate sum_sq", rc == SQLITE_OK);
  sqlite3_stmt *ag = NULL;
  sqlite3_prepare_v2(db, "SELECT sum_sq(id), sum(id*id) FROM t", -1, &ag, NULL);
  CHECK("aggregate step", sqlite3_step(ag) == SQLITE_ROW);
  CHECK("sum_sq matches builtin sum(id*id)",
        sqlite3_column_int64(ag, 0) == sqlite3_column_int64(ag, 1));
  sqlite3_finalize(ag);

  /* A GROUP BY exercises fresh per-group accumulator state. */
  sqlite3_exec(db, "CREATE TABLE g(k INT, v INT)", NULL, NULL, NULL);
  sqlite3_exec(db, "INSERT INTO g VALUES(1,2),(1,3),(2,5)", NULL, NULL, NULL);
  sqlite3_stmt *ag2 = NULL;
  sqlite3_prepare_v2(db, "SELECT k, sum_sq(v) FROM g GROUP BY k ORDER BY k", -1, &ag2, NULL);
  CHECK("group row 1", sqlite3_step(ag2) == SQLITE_ROW && sqlite3_column_int64(ag2, 1) == 13); /* 4+9 */
  CHECK("group row 2", sqlite3_step(ag2) == SQLITE_ROW && sqlite3_column_int64(ag2, 1) == 25); /* 25 */
  sqlite3_finalize(ag2);

  /* An invalid callback combination (step without final) -> SQLITE_ERROR. */
  CHECK("step-without-final -> ERROR",
        sqlite3_create_function(db, "bad", 1, SQLITE_UTF8, NULL, NULL,
                                lone_step, NULL) == SQLITE_ERROR);

  /* Connection/statement introspection + prepare_v3. */
  CHECK("errstr(SQLITE_RANGE)", strcmp(sqlite3_errstr(SQLITE_RANGE), "column index out of range") == 0);
  CHECK("busy_timeout no-op OK", sqlite3_busy_timeout(db, 1000) == SQLITE_OK);
  CHECK("autocommit on by default", sqlite3_get_autocommit(db) == 1);
  sqlite3_exec(db, "BEGIN", NULL, NULL, NULL);
  CHECK("autocommit off in transaction", sqlite3_get_autocommit(db) == 0);
  sqlite3_exec(db, "COMMIT", NULL, NULL, NULL);
  CHECK("autocommit on after commit", sqlite3_get_autocommit(db) == 1);
  CHECK("total_changes accumulates", sqlite3_total_changes(db) > 0);

  sqlite3_stmt *v3 = NULL;
  int prc = sqlite3_prepare_v3(db, "SELECT 42", -1, 0, &v3, NULL);
  CHECK("prepare_v3 ok", prc == SQLITE_OK);
  CHECK("sql() echoes text", strcmp(sqlite3_sql(v3), "SELECT 42") == 0);
  CHECK("db_handle round-trips", sqlite3_db_handle(v3) == db);
  CHECK("v3 step", sqlite3_step(v3) == SQLITE_ROW && sqlite3_column_int64(v3, 0) == 42);
  sqlite3_finalize(v3);

  /* Custom collating sequences via sqlite3_create_collation. */
  rc = sqlite3_create_collation(db, "REV", SQLITE_UTF8, NULL, rev_collation);
  CHECK("create_collation REV", rc == SQLITE_OK);
  rc = sqlite3_create_collation(db, "MYNOCASE", SQLITE_UTF8, NULL, nocase_collation);
  CHECK("create_collation MYNOCASE", rc == SQLITE_OK);

  sqlite3_exec(db, "CREATE TABLE c(s TEXT)", NULL, NULL, NULL);
  sqlite3_exec(db, "INSERT INTO c VALUES('apple'),('Cherry'),('banana')", NULL, NULL, NULL);

  /* ORDER BY ... COLLATE REV: reverse-binary order -> banana, apple, Cherry
     (lowercase 'a'/'b' > uppercase 'C' in BINARY, reversed). */
  sqlite3_stmt *cr = NULL;
  sqlite3_prepare_v2(db, "SELECT s FROM c ORDER BY s COLLATE REV", -1, &cr, NULL);
  sqlite3_step(cr);
  CHECK("REV row0 banana", strcmp((const char *)sqlite3_column_text(cr, 0), "banana") == 0);
  sqlite3_step(cr);
  CHECK("REV row1 apple", strcmp((const char *)sqlite3_column_text(cr, 0), "apple") == 0);
  sqlite3_step(cr);
  CHECK("REV row2 Cherry", strcmp((const char *)sqlite3_column_text(cr, 0), "Cherry") == 0);
  sqlite3_finalize(cr);

  /* A custom collation defined to equal NOCASE must order like built-in NOCASE:
     apple, banana, Cherry. */
  const char *want[] = {"apple", "banana", "Cherry"};
  sqlite3_stmt *cn = NULL, *bn = NULL;
  sqlite3_prepare_v2(db, "SELECT s FROM c ORDER BY s COLLATE MYNOCASE", -1, &cn, NULL);
  sqlite3_prepare_v2(db, "SELECT s FROM c ORDER BY s COLLATE NOCASE", -1, &bn, NULL);
  int match = 1;
  for (int i = 0; i < 3; i++) {
    sqlite3_step(cn); sqlite3_step(bn);
    const char *a = (const char *)sqlite3_column_text(cn, 0);
    const char *b = (const char *)sqlite3_column_text(bn, 0);
    if (strcmp(a, want[i]) != 0 || strcmp(a, b) != 0) match = 0;
  }
  CHECK("MYNOCASE matches built-in NOCASE ordering", match);
  sqlite3_finalize(cn); sqlite3_finalize(bn);

  /* A custom collation works in a UNIQUE index (case-insensitive dedup). */
  rc = sqlite3_exec(db, "CREATE TABLE ci(k TEXT COLLATE MYNOCASE)", NULL, NULL, NULL);
  CHECK("create table with custom collation column", rc == SQLITE_OK);
  sqlite3_exec(db, "CREATE UNIQUE INDEX ci_k ON ci(k)", NULL, NULL, NULL);
  sqlite3_exec(db, "INSERT INTO ci VALUES('Hello')", NULL, NULL, NULL);
  rc = sqlite3_exec(db, "INSERT INTO ci VALUES('HELLO')", NULL, NULL, NULL);
  CHECK("custom-collation UNIQUE rejects case-variant dup", rc != SQLITE_OK);

  /* An unknown collation name still errors. */
  rc = sqlite3_exec(db, "SELECT 1 FROM c ORDER BY s COLLATE nope", NULL, NULL, NULL);
  CHECK("unknown collation still errors", rc == SQLITE_ERROR);

  /* A user-defined aggregate registered as a window function: running sum of
     squares over an ORDER BY frame. Reuses sumsq_step/sumsq_final. */
  rc = sqlite3_create_window_function(db, "wsumsq", 1, SQLITE_UTF8, NULL,
                                      sumsq_step, sumsq_final, NULL, NULL, NULL);
  CHECK("create_window_function wsumsq", rc == SQLITE_OK);
  sqlite3_exec(db, "CREATE TABLE w(i INTEGER)", NULL, NULL, NULL);
  sqlite3_exec(db, "INSERT INTO w VALUES(1),(2),(3)", NULL, NULL, NULL);
  sqlite3_stmt *ws = NULL;
  sqlite3_prepare_v2(db, "SELECT wsumsq(i) OVER (ORDER BY i) FROM w ORDER BY i", -1, &ws, NULL);
  long long wexp[] = {1, 5, 14}; /* 1, 1+4, 1+4+9 */
  int wok = 1;
  for (int i = 0; i < 3; i++) {
    if (sqlite3_step(ws) != SQLITE_ROW || sqlite3_column_int64(ws, 0) != wexp[i]) wok = 0;
  }
  CHECK("window UDF running sum-of-squares 1,5,14", wok);
  sqlite3_finalize(ws);

  /* UTF-16 path: prepare16 + bind_text16 + column_text16 round-trip. Build
     UTF-16 (native-endian, ASCII so 1 unit/char) buffers by hand. */
  {
    /* "SELECT ?1" */
    const char *q = "SELECT ?1";
    unsigned short q16[16];
    int qi = 0;
    for (const char *p = q; *p; p++) q16[qi++] = (unsigned char)*p;
    q16[qi] = 0;
    /* bind value "wide" */
    const char *val = "wide";
    unsigned short v16[8];
    int vi = 0;
    for (const char *p = val; *p; p++) v16[vi++] = (unsigned char)*p;
    v16[vi] = 0;

    sqlite3_stmt *u16 = NULL;
    rc = sqlite3_prepare16_v2(db, q16, -1, &u16, NULL);
    CHECK("prepare16_v2 ok", rc == SQLITE_OK && u16 != NULL);
    sqlite3_bind_text16(u16, 1, v16, -1, SQLITE_TRANSIENT);
    CHECK("utf16 step -> ROW", sqlite3_step(u16) == SQLITE_ROW);
    CHECK("column_bytes16 == 8", sqlite3_column_bytes16(u16, 0) == 8); /* 4 chars * 2 */
    const unsigned short *out = (const unsigned short *)sqlite3_column_text16(u16, 0);
    int u16ok = out && out[0] == 'w' && out[1] == 'i' && out[2] == 'd' && out[3] == 'e' && out[4] == 0;
    CHECK("column_text16 round-trips 'wide'", u16ok);
    /* And the UTF-8 view agrees. */
    CHECK("utf8 view agrees", strcmp((const char *)sqlite3_column_text(u16, 0), "wide") == 0);
    sqlite3_finalize(u16);
  }

  /* Data-change notification via sqlite3_update_hook. */
  sqlite3_update_hook(db, update_cb, NULL);
  sqlite3_exec(db, "CREATE TABLE h(a)", NULL, NULL, NULL);
  sqlite3_exec(db, "INSERT INTO h VALUES (1),(2),(3)", NULL, NULL, NULL);
  sqlite3_exec(db, "UPDATE h SET a=a+1 WHERE a=1", NULL, NULL, NULL);
  sqlite3_exec(db, "DELETE FROM h WHERE a=3", NULL, NULL, NULL);
  CHECK("update hook saw 3 inserts", uh_inserts == 3);
  CHECK("update hook saw 1 update", uh_updates == 1);
  CHECK("update hook saw 1 delete", uh_deletes == 1);
  CHECK("update hook last rowid (delete of rowid 3)", uh_last_rowid == 3);
  /* Removing the hook stops notifications. */
  sqlite3_update_hook(db, NULL, NULL);
  sqlite3_exec(db, "INSERT INTO h VALUES (9)", NULL, NULL, NULL);
  CHECK("removed hook: still 3 inserts", uh_inserts == 3);

  /* Commit / rollback hooks (sqlite3_commit_hook / sqlite3_rollback_hook). */
  sqlite3_commit_hook(db, commit_cb, NULL);
  sqlite3_rollback_hook(db, rollback_cb, NULL);
  ch_commits = 0; rh_rollbacks = 0; ch_veto = 0;
  sqlite3_exec(db, "CREATE TABLE ct(x)", NULL, NULL, NULL);   /* autocommit write */
  sqlite3_exec(db, "INSERT INTO ct VALUES(1)", NULL, NULL, NULL);
  CHECK("commit hook fired on autocommit writes", ch_commits == 2);
  sqlite3_exec(db, "BEGIN", NULL, NULL, NULL);
  sqlite3_exec(db, "INSERT INTO ct VALUES(2)", NULL, NULL, NULL);
  sqlite3_exec(db, "ROLLBACK", NULL, NULL, NULL);
  CHECK("rollback hook fired on ROLLBACK", rh_rollbacks == 1);
  /* Veto: a non-zero commit hook converts the commit into a rollback. */
  ch_veto = 1;
  sqlite3_exec(db, "INSERT INTO ct VALUES(3)", NULL, NULL, NULL);
  {
    sqlite3_stmt *cs = NULL;
    sqlite3_prepare_v2(db, "SELECT count(*) FROM ct", -1, &cs, NULL);
    sqlite3_step(cs);
    CHECK("commit veto rolled the write back", sqlite3_column_int(cs, 0) == 1);
    sqlite3_finalize(cs);
  }
  CHECK("commit veto fired the rollback hook", rh_rollbacks == 2);
  sqlite3_commit_hook(db, NULL, NULL);
  sqlite3_rollback_hook(db, NULL, NULL);

  /* Online backup: copy a populated source into a fresh destination. */
  {
    sqlite3 *src = NULL, *dst = NULL;
    sqlite3_open(":memory:", &src);
    sqlite3_exec(src, "CREATE TABLE bak(a); INSERT INTO bak VALUES(10),(20),(30)", NULL, NULL, NULL);
    sqlite3_open(":memory:", &dst);
    sqlite3_backup *bk = sqlite3_backup_init(dst, "main", src, "main");
    CHECK("backup_init non-NULL", bk != NULL);
    CHECK("backup_pagecount positive", sqlite3_backup_pagecount(bk) > 0);
    int rc = sqlite3_backup_step(bk, -1);
    CHECK("backup_step -> DONE", rc == SQLITE_DONE);
    CHECK("backup_remaining 0 when done", sqlite3_backup_remaining(bk) == 0);
    CHECK("backup_finish OK", sqlite3_backup_finish(bk) == SQLITE_OK);
    /* The destination now holds the source's table. */
    sqlite3_stmt *bs = NULL;
    sqlite3_prepare_v2(dst, "SELECT count(*), sum(a) FROM bak", -1, &bs, NULL);
    sqlite3_step(bs);
    CHECK("backup copied 3 rows", sqlite3_column_int(bs, 0) == 3);
    CHECK("backup copied values (sum 60)", sqlite3_column_int(bs, 1) == 60);
    sqlite3_finalize(bs);
    sqlite3_close(src);
    sqlite3_close(dst);
  }

  /* Authorizer: a read-only sandbox denies writes but allows reads. */
  {
    sqlite3 *az = NULL;
    sqlite3_open(":memory:", &az);
    sqlite3_exec(az, "CREATE TABLE s(a); INSERT INTO s VALUES(1),(2)", NULL, NULL, NULL);
    sqlite3_set_authorizer(az, deny_writes_cb, NULL);
    int rd = sqlite3_exec(az, "SELECT * FROM s", NULL, NULL, NULL);
    CHECK("authorizer allows SELECT", rd == SQLITE_OK);
    int wr = sqlite3_exec(az, "INSERT INTO s VALUES(3)", NULL, NULL, NULL);
    CHECK("authorizer denies INSERT", wr != SQLITE_OK);
    int dr = sqlite3_exec(az, "DROP TABLE s", NULL, NULL, NULL);
    CHECK("authorizer denies DROP", dr != SQLITE_OK);
    /* Clearing the authorizer re-allows writes. */
    sqlite3_set_authorizer(az, NULL, NULL);
    int wr2 = sqlite3_exec(az, "INSERT INTO s VALUES(3)", NULL, NULL, NULL);
    CHECK("cleared authorizer allows INSERT", wr2 == SQLITE_OK);
    sqlite3_stmt *cs = NULL;
    sqlite3_prepare_v2(az, "SELECT count(*) FROM s", -1, &cs, NULL);
    sqlite3_step(cs);
    CHECK("only the allowed insert landed (3 rows)", sqlite3_column_int(cs, 0) == 3);
    sqlite3_finalize(cs);
    sqlite3_close(az);
  }

  /* Incremental BLOB I/O: open a cell, read it, overwrite a byte, verify. */
  sqlite3_exec(db, "CREATE TABLE blobs(id INTEGER PRIMARY KEY, data BLOB)", NULL, NULL, NULL);
  {
    sqlite3_stmt *bi = NULL;
    sqlite3_prepare_v2(db, "INSERT INTO blobs(id,data) VALUES(1, ?1)", -1, &bi, NULL);
    unsigned char init[] = {0xaa, 0xbb, 0xcc, 0xdd};
    sqlite3_bind_blob(bi, 1, init, 4, SQLITE_TRANSIENT);
    sqlite3_step(bi);
    sqlite3_finalize(bi);
  }
  sqlite3_blob *blob = NULL;
  rc = sqlite3_blob_open(db, "main", "blobs", "data", 1, 1 /* rw */, &blob);
  CHECK("blob_open ok", rc == SQLITE_OK && blob != NULL);
  CHECK("blob_bytes == 4", sqlite3_blob_bytes(blob) == 4);
  unsigned char rb[4] = {0};
  CHECK("blob_read ok", sqlite3_blob_read(blob, rb, 4, 0) == SQLITE_OK);
  CHECK("blob_read bytes", rb[0] == 0xaa && rb[3] == 0xdd);
  /* out-of-range read fails */
  CHECK("blob_read oob -> ERROR", sqlite3_blob_read(blob, rb, 4, 2) == SQLITE_ERROR);
  /* overwrite byte 1 */
  unsigned char nb = 0x55;
  CHECK("blob_write ok", sqlite3_blob_write(blob, &nb, 1, 1) == SQLITE_OK);
  sqlite3_blob_close(blob); /* flushes */

  /* Read back via SQL to confirm the write persisted. */
  {
    sqlite3_stmt *chk = NULL;
    sqlite3_prepare_v2(db, "SELECT data FROM blobs WHERE id=1", -1, &chk, NULL);
    sqlite3_step(chk);
    const unsigned char *got = (const unsigned char *)sqlite3_column_blob(chk, 0);
    CHECK("blob write persisted", sqlite3_column_bytes(chk, 0) == 4 && got[1] == 0x55 && got[0] == 0xaa);
    sqlite3_finalize(chk);
  }

  /* sqlite3_complete: statement-completeness for REPL-style consumers. */
  CHECK("complete: 'SELECT 1;'", sqlite3_complete("SELECT 1;") == 1);
  CHECK("complete: trailing ws/comment", sqlite3_complete("SELECT 1;  -- done\n") == 1);
  CHECK("incomplete: no semicolon", sqlite3_complete("SELECT 1") == 0);
  CHECK("incomplete: semicolon in string only", sqlite3_complete("SELECT ';'") == 0);
  CHECK("complete: two statements", sqlite3_complete("SELECT 1; SELECT 2;") == 1);

  /* sqlite3_stmt_readonly. */
  {
    sqlite3_stmt *ro = NULL, *rw = NULL;
    sqlite3_prepare_v2(db, "SELECT 1", -1, &ro, NULL);
    sqlite3_prepare_v2(db, "INSERT INTO t(name) VALUES('x')", -1, &rw, NULL);
    CHECK("SELECT is readonly", sqlite3_stmt_readonly(ro) != 0);
    CHECK("INSERT is not readonly", sqlite3_stmt_readonly(rw) == 0);
    sqlite3_finalize(ro);
    sqlite3_finalize(rw);
  }

  CHECK("version string", strcmp(sqlite3_libversion(), "3.50.4") == 0);

  sqlite3_close(db);

  printf(failures ? "\n%d FAILURE(S)\n" : "\nALL PASS\n", failures);
  return failures ? 1 : 0;
}
