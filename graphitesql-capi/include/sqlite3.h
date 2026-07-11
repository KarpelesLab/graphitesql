/*
** Subset of the sqlite3 C API implemented by graphitesql-capi (ROADMAP D7).
** Declarations match the official sqlite3.h so existing consumers compile
** unchanged for the covered surface. See src/lib.rs for the authoritative list.
*/
#ifndef GRAPHITESQL_SQLITE3_H
#define GRAPHITESQL_SQLITE3_H
#ifdef __cplusplus
extern "C" {
#endif

typedef struct sqlite3 sqlite3;
typedef struct sqlite3_stmt sqlite3_stmt;
typedef long long int sqlite3_int64;

/* Result codes */
#define SQLITE_OK      0
#define SQLITE_ERROR   1
#define SQLITE_NOMEM   7
#define SQLITE_RANGE  25
#define SQLITE_ROW   100
#define SQLITE_DONE  101

/* Fundamental datatypes */
#define SQLITE_INTEGER 1
#define SQLITE_FLOAT   2
#define SQLITE_TEXT    3
#define SQLITE_BLOB    4
#define SQLITE_NULL    5

/* Text encodings */
#define SQLITE_UTF8 1

/* bind_text/blob destructor sentinels */
#define SQLITE_STATIC      ((void(*)(void*))0)
#define SQLITE_TRANSIENT   ((void(*)(void*))-1)

const char *sqlite3_libversion(void);
int sqlite3_libversion_number(void);

int sqlite3_open(const char *filename, sqlite3 **ppDb);
int sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs);
int sqlite3_close(sqlite3 *db);
int sqlite3_close_v2(sqlite3 *db);

int sqlite3_exec(sqlite3 *db, const char *sql,
                 int (*callback)(void *, int, char **, char **),
                 void *arg, char **errmsg);

const char *sqlite3_errmsg(sqlite3 *db);
int sqlite3_errcode(sqlite3 *db);
int sqlite3_extended_errcode(sqlite3 *db);
const char *sqlite3_errstr(int rc);
int sqlite3_changes(sqlite3 *db);
int sqlite3_total_changes(sqlite3 *db);
sqlite3_int64 sqlite3_last_insert_rowid(sqlite3 *db);
int sqlite3_get_autocommit(sqlite3 *db);
int sqlite3_busy_timeout(sqlite3 *db, int ms);
void sqlite3_interrupt(sqlite3 *db);

int sqlite3_prepare_v2(sqlite3 *db, const char *sql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail);
int sqlite3_prepare_v3(sqlite3 *db, const char *sql, int nByte, unsigned int prepFlags,
                       sqlite3_stmt **ppStmt, const char **pzTail);
sqlite3 *sqlite3_db_handle(sqlite3_stmt *stmt);
const char *sqlite3_sql(sqlite3_stmt *stmt);
int sqlite3_step(sqlite3_stmt *stmt);
int sqlite3_reset(sqlite3_stmt *stmt);
int sqlite3_clear_bindings(sqlite3_stmt *stmt);
int sqlite3_finalize(sqlite3_stmt *stmt);

int sqlite3_bind_int(sqlite3_stmt *stmt, int idx, int v);
int sqlite3_bind_int64(sqlite3_stmt *stmt, int idx, sqlite3_int64 v);
int sqlite3_bind_double(sqlite3_stmt *stmt, int idx, double v);
int sqlite3_bind_null(sqlite3_stmt *stmt, int idx);
int sqlite3_bind_text(sqlite3_stmt *stmt, int idx, const char *text, int nByte, void(*d)(void*));
int sqlite3_bind_blob(sqlite3_stmt *stmt, int idx, const void *data, int nByte, void(*d)(void*));

int sqlite3_bind_parameter_count(sqlite3_stmt *stmt);
const char *sqlite3_bind_parameter_name(sqlite3_stmt *stmt, int idx);
int sqlite3_bind_parameter_index(sqlite3_stmt *stmt, const char *name);

int sqlite3_column_count(sqlite3_stmt *stmt);
int sqlite3_data_count(sqlite3_stmt *stmt);
const char *sqlite3_column_name(sqlite3_stmt *stmt, int col);
int sqlite3_column_type(sqlite3_stmt *stmt, int col);
int sqlite3_column_int(sqlite3_stmt *stmt, int col);
sqlite3_int64 sqlite3_column_int64(sqlite3_stmt *stmt, int col);
double sqlite3_column_double(sqlite3_stmt *stmt, int col);
const unsigned char *sqlite3_column_text(sqlite3_stmt *stmt, int col);
const void *sqlite3_column_blob(sqlite3_stmt *stmt, int col);
int sqlite3_column_bytes(sqlite3_stmt *stmt, int col);

/* User-defined scalar functions */
typedef struct sqlite3_context sqlite3_context;
typedef struct sqlite3_value sqlite3_value;

int sqlite3_create_function(sqlite3 *db, const char *zName, int nArg, int eTextRep,
    void *pApp,
    void (*xFunc)(sqlite3_context *, int, sqlite3_value **),
    void (*xStep)(sqlite3_context *, int, sqlite3_value **),
    void (*xFinal)(sqlite3_context *));

void *sqlite3_user_data(sqlite3_context *ctx);
void *sqlite3_aggregate_context(sqlite3_context *ctx, int nBytes);

int sqlite3_create_window_function(sqlite3 *db, const char *zName, int nArg, int eTextRep,
    void *pApp,
    void (*xStep)(sqlite3_context *, int, sqlite3_value **),
    void (*xFinal)(sqlite3_context *),
    void (*xValue)(sqlite3_context *),
    void (*xInverse)(sqlite3_context *, int, sqlite3_value **),
    void (*xDestroy)(void *));

/* Custom collating sequences */
int sqlite3_create_collation(sqlite3 *db, const char *zName, int eTextRep, void *pArg,
    int (*xCompare)(void *, int, const void *, int, const void *));
int sqlite3_create_collation_v2(sqlite3 *db, const char *zName, int eTextRep, void *pArg,
    int (*xCompare)(void *, int, const void *, int, const void *),
    void (*xDestroy)(void *));

int sqlite3_value_type(sqlite3_value *v);
int sqlite3_value_int(sqlite3_value *v);
sqlite3_int64 sqlite3_value_int64(sqlite3_value *v);
double sqlite3_value_double(sqlite3_value *v);
int sqlite3_value_bytes(sqlite3_value *v);
const unsigned char *sqlite3_value_text(sqlite3_value *v);
const void *sqlite3_value_blob(sqlite3_value *v);

void sqlite3_result_null(sqlite3_context *ctx);
void sqlite3_result_int(sqlite3_context *ctx, int v);
void sqlite3_result_int64(sqlite3_context *ctx, sqlite3_int64 v);
void sqlite3_result_double(sqlite3_context *ctx, double v);
void sqlite3_result_text(sqlite3_context *ctx, const char *text, int nByte, void(*d)(void*));
void sqlite3_result_blob(sqlite3_context *ctx, const void *data, int nByte, void(*d)(void*));
void sqlite3_result_error(sqlite3_context *ctx, const char *msg, int nByte);

/* UTF-16 entry points (native byte order; nByte args are in bytes) */
int sqlite3_open16(const void *zFilename, sqlite3 **ppDb);
int sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte,
                         sqlite3_stmt **ppStmt, const void **pzTail);
int sqlite3_bind_text16(sqlite3_stmt *stmt, int idx, const void *text, int nByte, void(*d)(void*));
const void *sqlite3_column_text16(sqlite3_stmt *stmt, int col);
int sqlite3_column_bytes16(sqlite3_stmt *stmt, int col);
const void *sqlite3_errmsg16(sqlite3 *db);

/* Data-change notification hook */
#define SQLITE_DELETE 9
#define SQLITE_INSERT 18
#define SQLITE_UPDATE 23
void *sqlite3_update_hook(sqlite3 *db,
    void (*xCallback)(void *, int op, char const *zDb, char const *zTable, sqlite3_int64 rowid),
    void *pArg);
void *sqlite3_commit_hook(sqlite3 *db, int (*xCallback)(void *), void *pArg);
void *sqlite3_rollback_hook(sqlite3 *db, void (*xCallback)(void *), void *pArg);

/* Incremental BLOB I/O (buffered) */
typedef struct sqlite3_blob sqlite3_blob;
int sqlite3_blob_open(sqlite3 *db, const char *zDb, const char *zTable, const char *zColumn,
                      sqlite3_int64 iRow, int flags, sqlite3_blob **ppBlob);
int sqlite3_blob_bytes(sqlite3_blob *blob);
int sqlite3_blob_read(sqlite3_blob *blob, void *z, int n, int iOffset);
int sqlite3_blob_write(sqlite3_blob *blob, const void *z, int n, int iOffset);
int sqlite3_blob_reopen(sqlite3_blob *blob, sqlite3_int64 iRow);
int sqlite3_blob_close(sqlite3_blob *blob);

int sqlite3_complete(const char *sql);
int sqlite3_stmt_readonly(sqlite3_stmt *stmt);

void sqlite3_free(void *p);

#ifdef __cplusplus
}
#endif
#endif /* GRAPHITESQL_SQLITE3_H */
