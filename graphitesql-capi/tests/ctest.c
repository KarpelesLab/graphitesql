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

/* Correctly-typed no-op aggregate callbacks (registration is unsupported). */
static void dummy_step(sqlite3_context *c, int n, sqlite3_value **v) { (void)c; (void)n; (void)v; }
static void dummy_final(sqlite3_context *c) { (void)c; }

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

  /* Aggregate registration is not supported yet -> SQLITE_ERROR. */
  CHECK("aggregate create -> ERROR",
        sqlite3_create_function(db, "agg", 1, SQLITE_UTF8, NULL, NULL,
                                dummy_step, dummy_final) == SQLITE_ERROR);

  CHECK("version string", strcmp(sqlite3_libversion(), "3.50.4") == 0);

  sqlite3_close(db);

  printf(failures ? "\n%d FAILURE(S)\n" : "\nALL PASS\n", failures);
  return failures ? 1 : 0;
}
