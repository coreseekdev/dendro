/*
 * dendro_sqlite.h —— dendro 的 SQLite C ABI 兼容层
 *
 * 链接 libdendro_sqlite（cdylib/staticlib）即可把链 libsqlite3 的
 * 程序切换到 dendro（进程内 HTAP：列存物化 + 分支 + 对象存储原生）。
 *
 * 合同与边界（详见 crate 文档）：
 * - API 级替换非文件级：sqlite3_open(path) 的 path 是【目录】
 *   （:memory: / 空串 = 内存库）；
 * - 单线程合同（同一句柄及其语句在单线程使用）；
 * - sqlite3_close_v2 自动失效未 finalize 语句（后续调用返回
 *   SQLITE_MISUSE 而非未定义行为）；
 * - v1 限制：sqlite3_changes/exec 不跟踪受影响行数（用 prepare/step
 *   或 Rust embed API）；last_insert_rowid 返回 0；prepare_v2 的
 *   pzTail 不切分多语句（指向串尾）；无 decltype/blob 绑定。
 */
#ifndef DENDRO_SQLITE_H
#define DENDRO_SQLITE_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct sqlite3 sqlite3;
typedef struct sqlite3_stmt sqlite3_stmt;
typedef long long sqlite3_int64;

/* 结果码 / 列类型（与 sqlite3.h 数值一致） */
#define SQLITE_OK 0
#define SQLITE_ERROR 1
#define SQLITE_MISUSE 21
#define SQLITE_ROW 100
#define SQLITE_DONE 101
#define SQLITE_INTEGER 1
#define SQLITE_FLOAT 2
#define SQLITE_TEXT 3
#define SQLITE_BLOB 4
#define SQLITE_NULL 5
/* open flags（接收并忽略——内存/本地目录恒可写） */
#define SQLITE_OPEN_READONLY 0x00000001
#define SQLITE_OPEN_READWRITE 0x00000002
#define SQLITE_OPEN_CREATE 0x00000004

const char *sqlite3_libversion(void);
int sqlite3_libversion_number(void);

int sqlite3_open(const char *filename, sqlite3 **ppDb);
int sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags,
                    const char *zVfs);
int sqlite3_close(sqlite3 *);
int sqlite3_close_v2(sqlite3 *);

const char *sqlite3_errmsg(sqlite3 *);
int sqlite3_errcode(sqlite3 *);
int sqlite3_changes(sqlite3 *);
sqlite3_int64 sqlite3_last_insert_rowid(sqlite3 *);
int sqlite3_busy_timeout(sqlite3 *, int ms);

typedef int (*sqlite3_callback)(void *arg, int ncols, char **argv,
                                char **azColName);
int sqlite3_exec(sqlite3 *, const char *sql, sqlite3_callback cb, void *arg,
                 char **errmsg);

int sqlite3_prepare_v2(sqlite3 *, const char *zSql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail);
int sqlite3_finalize(sqlite3_stmt *);
int sqlite3_step(sqlite3_stmt *);
int sqlite3_reset(sqlite3_stmt *);
int sqlite3_clear_bindings(sqlite3_stmt *);
int sqlite3_bind_parameter_count(sqlite3_stmt *);

int sqlite3_bind_null(sqlite3_stmt *, int idx);
int sqlite3_bind_int64(sqlite3_stmt *, int idx, sqlite3_int64 value);
int sqlite3_bind_double(sqlite3_stmt *, int idx, double value);
int sqlite3_bind_text(sqlite3_stmt *, int idx, const char *value, int nByte,
                      const void *destructor);

int sqlite3_column_count(sqlite3_stmt *);
const char *sqlite3_column_name(sqlite3_stmt *, int iCol);
int sqlite3_column_type(sqlite3_stmt *, int iCol);
sqlite3_int64 sqlite3_column_int64(sqlite3_stmt *, int iCol);
double sqlite3_column_double(sqlite3_stmt *, int iCol);
const unsigned char *sqlite3_column_text(sqlite3_stmt *, int iCol);
int sqlite3_column_bytes(sqlite3_stmt *, int iCol);
const void *sqlite3_column_blob(sqlite3_stmt *, int iCol);

#ifdef __cplusplus
}
#endif
#endif /* DENDRO_SQLITE_H */
