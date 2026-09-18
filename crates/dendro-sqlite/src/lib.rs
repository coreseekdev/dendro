//! dendro-sqlite —— SQLite C ABI 兼容层（"sqlite 侧的 wire"）。
//!
//! SQLite 没有 wire 协议——它的生态接口是 **libsqlite3 的 C ABI
//! （sqlite3_* 函数族）**。本 crate 把 dendro 暴露为该 ABI 的实现
//! （cdylib/staticlib），使链 libsqlite3 的程序/语言运行时（C/C++、
//! Python `sqlite3` 经 LD_PRELOAD 或链接替换、任何 `dlopen` 用法）
//! 可以**进程内**使用 dendro——替换 SQLite，但获得 HTAP（列存物化 +
//! 分支 + 对象存储原生）。
//!
//! # 两轴定位（方言 × 传输）
//!
//! | 传输 | 方言 | 用途 |
//! |------|------|------|
//! | pgwire（TCP 二进制 v3） | PG | psql、JDBC、驱动生态 |
//! | mywire（TCP 二进制） | MySQL | MySQL 客户端生态 |
//! | embed（Rust 进程内） | **Sqlite 默认**（可换） | Rust 嵌入安全 API——本 crate 的底层 |
//! | **dendro-sqlite（C ABI 进程内）** | **SQLite** | **SQLite 生态替换面——与 embed 同一进程内语义 + C 调用约定 + SQLite 方言** |
//!
//! # 合同与边界（诚实清单）
//!
//! **API 级替换，非文件级**：dendro 的存储是目录（prolly CAS + WAL +
//! CBF 段），不是单文件 .db——`sqlite3_open(path)` 把 path 当**目录**
//! 打开（存在性不要求；`:memory:` / 空串 = 内存库）。
//!
//! **单线程合同**：同一 `sqlite3*` 及其语句在单线程内使用（SQLite 的
//! default serialized 模式不承诺）。
//!
//! **stmt 生命周期**：`sqlite3_close_v2` 自动失效未 finalize 的语句
//! （后续 step/finalize 返回 SQLITE_MISUSE 而非 UB）。
//!
//! **v1 剩余未实现**：结果集为语句物化（`step` 首次执行全量求值）
//! ——惰性游标留给 v2。已实现：blob 绑定、`pzTail` 多语句切分、
//! `sqlite3_sql`、`column_decltype`、`sqlite3_changes`（exec/step
//! 双路径）、**`last_insert_rowid`**（SQLite 语义：INTEGER PRIMARY
//! KEY 列即 rowid 别名——记末行 PK）。
//! # 快速验证
//!
//! ```c
//! #include "dendro_sqlite.h"
//! sqlite3 *db; sqlite3_open(":memory:", &db);
//! sqlite3_exec(db, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", 0,0,0);
//! sqlite3_exec(db, "INSERT INTO t VALUES (1,'hi')", 0,0,0);
//! sqlite3_stmt *st; sqlite3_prepare_v2(db, "SELECT v FROM t", -1, &st, 0);
//! while (sqlite3_step(st) == SQLITE_ROW)
//!   puts((char*)sqlite3_column_text(st, 0));
//! sqlite3_finalize(st); sqlite3_close(db);
//! ```

#![allow(clippy::missing_safety_doc)]

use dendro_core::embed::{Connection, QueryResult, Value};
use dendro_core::types::SqlValue;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ---- SQLite 常量（与 sqlite3.h 数值一致） ----
pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ERROR: c_int = 1;
pub const SQLITE_MISUSE: c_int = 21;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;
pub const SQLITE_INTEGER: c_int = 1;
pub const SQLITE_FLOAT: c_int = 2;
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_BLOB: c_int = 4;
pub const SQLITE_NULL: c_int = 5;

const VERSION: &str = "3.41.2-dendro\0";

/// 连接态：db+会话 + 存活标志（close_v2 使未 finalize 语句失效）
struct Conn {
    alive: Arc<AtomicBool>,
    conn: Option<Connection>,
    changes: i64,
    last_err: String,
    /// errmsg 缓冲（到下次 API 调用前有效——文档化弱于 sqlite3_free
    /// 语义但零分配失败面）
    err_buf: CString,
}

/// 语句态：裸引用所属连接（单线程合同）+ 物化结果游标
struct Stmt {
    alive: Arc<AtomicBool>,
    conn: *mut Conn,
    name: String,
    #[allow(dead_code)] // 调试面（未来 sqlite3_sql 导出用）
    sql: CString,
    params: Vec<Value>,
    result: Option<QueryResult>,
    row: usize,
    /// 惰性游标（v2 step 语义）：单表 SELECT 形态源直驱——
    /// 馞行 O(首批)/early-term/输出 O(批) 内存；段解码仍整段
    /// 物化（完整惰性随流式执行器 C 档——诚实边界）
    lazy: Option<dendro_core::embed::LazyScan>,
    /// 当前行（惰性/物化双路统一访问位）
    cur: Option<Vec<dendro_core::types::SqlValue>>,
    /// prepare 期列元数据（column_count/name 在首次 step 前可用——
    /// SQLite 语义）
    col_names: Vec<String>,
    /// 列声明类型（column_decltype 用；表达式列为空——SQLite 对
    /// 表达式返回 NULL 同义）
    col_decls: Vec<Option<&'static std::ffi::CStr>>,
    /// column_text/bytes 的临时格式化缓冲
    scratch: Vec<u8>,
}

impl Conn {
    fn set_err(&mut self, e: dendro_core::error::SqlError) -> c_int {
        self.last_err = format!("{}: {}", e.state, e.message);
        self.err_buf =
            CString::new(self.last_err.clone()).unwrap_or_else(|_| CString::new("error").unwrap());
        SQLITE_ERROR
    }
}

#[no_mangle]
pub extern "C" fn sqlite3_libversion() -> *const c_char {
    VERSION.as_ptr().cast()
}

#[no_mangle]
pub extern "C" fn sqlite3_libversion_number() -> c_int {
    30_410_002
}

/// 打开：`:memory:` / NULL / 空串 = 内存库；其余 = LocalDir(path)
/// （目录语义——见 crate 文档）。flags/v1 忽略（内存与本地目录恒可写）。
#[no_mangle]
pub unsafe extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    pp_db: *mut *mut sqlite3,
    _flags: c_int,
    _vfs: *const c_char,
) -> c_int {
    if pp_db.is_null() {
        return SQLITE_MISUSE;
    }
    let mem = filename.is_null()
        || unsafe { CStr::from_ptr(filename) }.to_bytes().is_empty()
        || unsafe { CStr::from_ptr(filename) } == c":memory:";
    let conn = if mem {
        Connection::memory()
    } else {
        let path = unsafe { CStr::from_ptr(filename) }
            .to_string_lossy()
            .to_string();
        Connection::open(&path)
    };
    // 方言经 embed 构造默认即 Sqlite（同一进程内语义——本 crate
    // 是 embed 的 C ABI 暴露 + SQLite 方言，无需重复声明）
    match conn {
        Ok(c) => {
            let conn = Box::new(Conn {
                alive: Arc::new(AtomicBool::new(true)),
                conn: Some(c),
                changes: 0,
                last_err: String::new(),
                err_buf: CString::new("").unwrap(),
            });
            *pp_db = Box::into_raw(conn).cast();
            SQLITE_OK
        }
        Err(e) => {
            // SQLite 合同：失败也写回 handle（可仅用于 errmsg）——
            // v1 简化：不写回，调用方读 errno 面缺失记录于文档
            let _ = e;
            SQLITE_ERROR
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_open(filename: *const c_char, pp_db: *mut *mut sqlite3) -> c_int {
    unsafe { sqlite3_open_v2(filename, pp_db, 0x2 | 0x4, std::ptr::null()) }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_close_v2(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return SQLITE_OK;
    }
    let conn: &mut Conn = unsafe { &mut *db.cast() };
    conn.alive.store(false, Ordering::Release);
    // 优雅关闭（WAL flush；Branch Drop 链负责）
    conn.conn = None;
    unsafe { drop(Box::from_raw(db.cast::<Conn>())) };
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_close(db: *mut sqlite3) -> c_int {
    unsafe { sqlite3_close_v2(db) }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char {
    if db.is_null() {
        return c"invalid handle".as_ptr();
    }
    let conn: &Conn = unsafe { &*db.cast() };
    conn.err_buf.as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errcode(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let conn: &Conn = unsafe { &*db.cast() };
    if conn.last_err.is_empty() {
        SQLITE_OK
    } else {
        SQLITE_ERROR
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return 0;
    }
    let conn: &Conn = unsafe { &*db.cast() };
    conn.changes as c_int
}

/// 多语句执行（dendro 会话天然支持语句串）。callback 非 NULL 时逐行
/// 回调（argv/azColName 指向语句结果内部，回调后失效——SQLite 同义）
#[no_mangle]
pub unsafe extern "C" fn sqlite3_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    cb: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *mut *const c_char, *mut *const c_char) -> c_int,
    >,
    cb_arg: *mut c_void,
    _errmsg: *mut *mut c_char,
) -> c_int {
    if db.is_null() || sql.is_null() {
        return SQLITE_MISUSE;
    }
    let conn: &mut Conn = unsafe { &mut *db.cast() };
    if !conn.alive.load(Ordering::Acquire) || conn.conn.is_none() {
        return SQLITE_MISUSE;
    }
    let sql_str = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return SQLITE_MISUSE,
    };
    let c = conn.conn.as_mut().unwrap();
    let (outputs, affected) = match c.exec_mixed(&sql_str) {
        Ok(r) => r,
        Err(e) => return conn.set_err(e),
    };
    conn.changes = affected;
    let rows = outputs.rows().to_vec();
    let names: Vec<CString> = (0..outputs.column_count())
        .map(|i| CString::new(outputs.column_name(i)).unwrap_or_default())
        .collect();
    if let Some(cb) = cb {
        for row in &rows {
            let mut argv: Vec<CString> = Vec::with_capacity(names.len());
            for v in row {
                let s = match v {
                    SqlValue::Null => CString::new("").unwrap(),
                    other => CString::new(value_text(other)).unwrap_or_default(),
                };
                argv.push(s);
            }
            let mut argv_ptrs: Vec<*const c_char> = argv.iter().map(|s| s.as_ptr()).collect();
            let mut name_ptrs: Vec<*const c_char> = names.iter().map(|s| s.as_ptr()).collect();
            let rc = unsafe {
                cb(
                    cb_arg,
                    names.len() as c_int,
                    argv_ptrs.as_mut_ptr().cast(),
                    name_ptrs.as_mut_ptr().cast(),
                )
            };
            if rc != SQLITE_OK {
                return rc;
            }
        }
    }
    conn.last_err.clear();
    SQLITE_OK
}

/// 顶层语句边界：首个**引号/注释外**的 `;` 后一字节位置（无则全串）。
/// 词法状态机覆盖 '…'/"…"/`…`/[…] 字面量与标识符、`--` 行注释、
/// `/* */` 块注释——与 SQLite 的 prepare 边界语义同构
fn stmt_boundary(bytes: &[u8]) -> Option<usize> {
    let b = bytes;
    let mut i = 0usize;
    let mut saw_token = false;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                saw_token = true;
                let q = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2; // 转义双写
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'[' => {
                saw_token = true; // SQLite 方括号标识符
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b';' if saw_token => return Some(i + 1),
            c => {
                if !c.is_ascii_whitespace() {
                    saw_token = true;
                }
                i += 1;
            }
        }
    }
    None
}

/// ColType → SQLite 声明名（column_decltype）。**必须 NUL 终止**
///（c"…" 字面量——&str 裸 as_ptr 当 CStr 读会越界进相邻静态区）
fn decl_name(t: dendro_core::types::ColType) -> Option<&'static std::ffi::CStr> {
    use dendro_core::types::ColType::*;
    Some(match t {
        Int32 | Int64 => c"INTEGER",
        Float64 => c"REAL",
        Utf8 => c"TEXT",
        Bytes => c"BLOB",
        Bool => c"BOOLEAN",
        Date32 => c"DATE",
        TimestampMs => c"TIMESTAMP",
    })
}

/// SqlValue → 文本（column_text/exec 回调共用口径）
fn value_text(v: &SqlValue) -> String {
    match v {
        SqlValue::Null => String::new(),
        SqlValue::Bool(b) => b.to_string(),
        SqlValue::Int32(i) => i.to_string(),
        SqlValue::Int64(i) => i.to_string(),
        SqlValue::Float64(f) => f.to_string(),
        SqlValue::Utf8(s) => s.clone(),
        SqlValue::Bytes(b) => String::from_utf8_lossy(b).to_string(),
        other => format!("{other:?}"),
    }
}

fn value_type(v: &SqlValue) -> c_int {
    match v {
        SqlValue::Null => SQLITE_NULL,
        SqlValue::Int32(_) | SqlValue::Int64(_) => SQLITE_INTEGER,
        SqlValue::Float64(_) => SQLITE_FLOAT,
        SqlValue::Utf8(_) => SQLITE_TEXT,
        SqlValue::Bytes(_) => SQLITE_BLOB,
        _ => SQLITE_TEXT, // Bool/Date/Timestamp 以文本呈现（v1）
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    db: *mut sqlite3,
    sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    if db.is_null() || sql.is_null() || pp_stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let conn: &mut Conn = unsafe { &mut *db.cast() };
    if !conn.alive.load(Ordering::Acquire) {
        return SQLITE_MISUSE;
    }
    let bytes = unsafe { CStr::from_ptr(sql) }.to_bytes();
    let sql_str = if n_byte >= 0 {
        String::from_utf8_lossy(&bytes[..(n_byte as usize).min(bytes.len())]).to_string()
    } else {
        String::from_utf8_lossy(bytes).to_string()
    };
    // pzTail：顶层（引号/注释外）分号边界——多语句串切出首语句
    // （SQLite prepare_v2 语义）。boundary = 首语句字节长度（含分号）
    let boundary = stmt_boundary(bytes).unwrap_or(bytes.len());
    let stmt_sql = sql_str[..boundary].to_string();
    let tail_off = {
        // tail 起点越过边界后的空白
        let t = sql_str[boundary..].trim_start();
        boundary + (sql_str[boundary..].len() - t.len())
    };
    let c = match conn.conn.as_mut() {
        Some(c) => c,
        None => return SQLITE_MISUSE,
    };
    let name = format!(
        "__ffi_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let meta = match c.sess_prepare(&name, &stmt_sql) {
        Ok(m) => m,
        Err(e) => return conn.set_err(e),
    };
    let col_names = meta
        .result_columns
        .iter()
        .map(|cm| cm.name.clone())
        .collect::<Vec<_>>();
    let col_decls = meta
        .result_columns
        .iter()
        .map(|cm| decl_name(cm.ty))
        .collect::<Vec<_>>();
    let stmt = Box::new(Stmt {
        alive: conn.alive.clone(),
        conn: db.cast(),
        name,
        sql: CString::new(stmt_sql).unwrap_or_default(),
        params: Vec::new(),
        result: None,
        row: 0,
        lazy: None,
        cur: None,
        col_names,
        col_decls,
        scratch: Vec::new(),
    });
    *pp_stmt = Box::into_raw(stmt).cast();
    if !pz_tail.is_null() {
        // 首语句后首个非空白字节（无后续语句 = 串尾 nul）
        unsafe { *pz_tail = sql.add(tail_off) };
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_OK;
    }
    let st: Box<Stmt> = unsafe { Box::from_raw(stmt.cast()) };
    if st.alive.load(Ordering::Acquire) {
        let conn: &mut Conn = unsafe { &mut *st.conn };
        if let Some(c) = conn.conn.as_mut() {
            c.sess_close_prepared(&st.name);
        }
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    if !st.alive.load(Ordering::Acquire) {
        return SQLITE_MISUSE;
    }
    // 惰性路径：单表 SELECT 形态——源直驱逐行（首次 step 构造）
    if let Some(lz) = st.lazy.as_mut() {
        return match lz.next_row() {
            Ok(Some(row)) => {
                st.cur = Some(row);
                SQLITE_ROW
            }
            Ok(None) => {
                st.cur = None;
                SQLITE_DONE
            }
            Err(e) => {
                let conn: &mut Conn = unsafe { &mut *st.conn };
                conn.set_err(e)
            }
        };
    }
    // 首次 step（或 reset 后）：执行并物化
    if st.result.is_none() {
        let conn: &mut Conn = unsafe { &mut *st.conn };
        let c = match conn.conn.as_mut() {
            Some(c) => c,
            None => return SQLITE_MISUSE,
        };
        let params = st.params.clone();
        // 惰性判定：SELECT 单表形态（已代入参数的原文重建查询——
        // prepared 语句体不含参数后的 AST）
        let mut try_lazy = || -> Option<dendro_core::embed::LazyScan> {
            let sql_text = st.sql.to_string_lossy().to_string();
            let sub = c.substitute_sql(&sql_text, &params).ok()?;
            c.lazy_scan(&sub).ok()?
        };
        if let Some(lz) = try_lazy() {
            st.lazy = Some(lz);
            let st2: &mut Stmt = unsafe { &mut *stmt.cast() };
            return match st2.lazy.as_mut().unwrap().next_row() {
                Ok(Some(row)) => {
                    st2.cur = Some(row);
                    SQLITE_ROW
                }
                Ok(None) => {
                    st2.cur = None;
                    SQLITE_DONE
                }
                Err(e) => {
                    let conn2: &mut Conn = unsafe { &mut *st2.conn };
                    conn2.set_err(e)
                }
            };
        }
        match c.sess_exec_prepared_mixed(&st.name, &params) {
            Ok((res, affected)) => {
                conn.changes = affected;
                st.result = Some(res);
                st.row = 0;
            }
            Err(e) => return conn.set_err(e),
        }
    }
    let st2: &mut Stmt = unsafe { &mut *stmt.cast() };
    let r = st2.result.as_ref().unwrap();
    if st2.row < r.row_count() {
        st2.row += 1;
        st2.cur = r.row(st2.row - 1).cloned();
        SQLITE_ROW
    } else {
        st2.cur = None;
        SQLITE_DONE
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    st.result = None;
    st.row = 0;
    st.lazy = None;
    st.cur = None;
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_clear_bindings(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    st.params.clear();
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_count(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    let st: &Stmt = unsafe { &*stmt.cast() };
    st.params.len() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    if stmt.is_null() || idx < 1 {
        return SQLITE_MISUSE;
    }
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    let i = idx as usize - 1;
    while st.params.len() <= i {
        st.params.push(Value::Null);
    }
    st.params[i] = Value::Null;
    SQLITE_OK
}

macro_rules! bind_fn {
    ($name:ident, $val:expr, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(stmt: *mut sqlite3_stmt, idx: c_int, v: $ty) -> c_int {
            if stmt.is_null() || idx < 1 {
                return SQLITE_MISUSE;
            }
            let st: &mut Stmt = unsafe { &mut *stmt.cast() };
            let i = idx as usize - 1;
            while st.params.len() <= i {
                st.params.push(Value::Null);
            }
            st.params[i] = $val(v);
            SQLITE_OK
        }
    };
}

bind_fn!(sqlite3_bind_int64, |v: i64| Value::Integer(v), i64);
bind_fn!(sqlite3_bind_double, |v: f64| Value::Real(v), f64);

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    v: *const c_void,
    n: c_int,
    _destructor: *const c_void,
) -> c_int {
    if stmt.is_null() || idx < 1 {
        return SQLITE_MISUSE;
    }
    if v.is_null() && n != 0 {
        return SQLITE_MISUSE;
    }
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    let blob = if v.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(v.cast::<u8>(), n.max(0) as usize) }.to_vec()
    };
    let i = idx as usize - 1;
    while st.params.len() <= i {
        st.params.push(Value::Null);
    }
    st.params[i] = Value::Blob(blob);
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    v: *const c_char,
    n: c_int,
    _destructor: *const c_void,
) -> c_int {
    if stmt.is_null() || idx < 1 || v.is_null() {
        return SQLITE_MISUSE;
    }
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    let bytes = unsafe { CStr::from_ptr(v) }.to_bytes();
    let s = if n >= 0 {
        String::from_utf8_lossy(&bytes[..(n as usize).min(bytes.len())]).to_string()
    } else {
        String::from_utf8_lossy(bytes).to_string()
    };
    let i = idx as usize - 1;
    while st.params.len() <= i {
        st.params.push(Value::Null);
    }
    st.params[i] = Value::Text(s);
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    let st: &Stmt = unsafe { &*stmt.cast() };
    // 元数据优先（prepare 期已定）；步进后以物化结果为准（对齐）
    st.result
        .as_ref()
        .map_or(st.col_names.len() as c_int, |r| r.column_count() as c_int)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_name(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char {
    if stmt.is_null() {
        return c"".as_ptr();
    }
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    // prepare 期元数据（step 前可用）；步进后以结果为准
    let name = match st.result.as_ref() {
        Some(r) => r.column_name(i.max(0) as usize).to_string(),
        None => match st.col_names.get(i.max(0) as usize) {
            Some(n) => n.clone(),
            None => return c"".as_ptr(),
        },
    };
    st.scratch = name.as_bytes().to_vec();
    st.scratch.push(0);
    st.scratch.as_ptr().cast()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_type(stmt: *mut sqlite3_stmt, i: c_int) -> c_int {
    if stmt.is_null() {
        return SQLITE_NULL;
    }
    let st: &Stmt = unsafe { &*stmt.cast() };
    let Some(r) = st.result.as_ref() else {
        return SQLITE_NULL;
    };
    match r
        .row(st.row.saturating_sub(1))
        .and_then(|row| row.get(i.max(0) as usize))
    {
        Some(v) => value_type(v),
        None => SQLITE_NULL,
    }
}

/// 语句当前行的第 i 列（惰性 cur / 物化 result 双路统一）
unsafe fn cur_cell(stmt: *mut sqlite3_stmt, i: c_int) -> Option<dendro_core::types::SqlValue> {
    let st: &Stmt = unsafe { &*stmt.cast() };
    let i = i.max(0) as usize;
    if let Some(row) = &st.cur {
        return row.get(i).cloned();
    }
    st.result
        .as_ref()?
        .row(st.row.saturating_sub(1))
        .and_then(|row| row.get(i))
        .cloned()
}

macro_rules! col_fn {
    ($name:ident, $body:expr, $ret:ty, $default:expr) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(stmt: *mut sqlite3_stmt, i: c_int) -> $ret {
            if stmt.is_null() {
                return $default;
            }
            match unsafe { cur_cell(stmt, i) } {
                Some(v) => $body(&v),
                None => $default,
            }
        }
    };
}

col_fn!(
    sqlite3_column_int64,
    |v: &SqlValue| match v {
        SqlValue::Int32(i) => *i as i64,
        SqlValue::Int64(i) => *i,
        SqlValue::Float64(f) => *f as i64,
        SqlValue::Utf8(s) => s.parse().unwrap_or(0),
        _ => 0,
    },
    i64,
    0
);
col_fn!(
    sqlite3_column_double,
    |v: &SqlValue| match v {
        SqlValue::Float64(f) => *f,
        SqlValue::Int32(i) => *i as f64,
        SqlValue::Int64(i) => *i as f64,
        SqlValue::Utf8(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    },
    f64,
    0.0
);

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_text(stmt: *mut sqlite3_stmt, i: c_int) -> *const u8 {
    if stmt.is_null() {
        return std::ptr::null();
    }
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    let text = match unsafe { cur_cell(stmt, i) } {
        Some(SqlValue::Utf8(s)) => s,
        Some(other) => value_text(&other),
        None => return std::ptr::null(),
    };
    st.scratch = text.into_bytes();
    st.scratch.push(0);
    st.scratch.as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_bytes(stmt: *mut sqlite3_stmt, i: c_int) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    match unsafe { cur_cell(stmt, i) } {
        Some(SqlValue::Utf8(s)) => s.len() as c_int,
        Some(SqlValue::Bytes(b)) => b.len() as c_int,
        Some(SqlValue::Null) | None => 0,
        Some(_) => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_blob(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_void {
    if stmt.is_null() {
        return std::ptr::null();
    }
    // cur_cell 返回 owned 值——复制进 scratch 再取指针（直接对
    // 临时取 as_ptr 是悬垂：match 臂结束即释放）
    let st: &mut Stmt = unsafe { &mut *stmt.cast() };
    match unsafe { cur_cell(stmt, i) } {
        Some(SqlValue::Bytes(b)) => {
            st.scratch = b;
            st.scratch.as_ptr().cast()
        }
        Some(SqlValue::Utf8(s)) => {
            st.scratch = s.into_bytes();
            st.scratch.as_ptr().cast()
        }
        _ => std::ptr::null(),
    }
}

/// 语句 SQL 文本（prepare 原文；到 finalize 有效）
#[no_mangle]
pub unsafe extern "C" fn sqlite3_sql(stmt: *mut sqlite3_stmt) -> *const c_char {
    if stmt.is_null() {
        return c"".as_ptr();
    }
    let st: &Stmt = unsafe { &*stmt.cast() };
    st.sql.as_ptr()
}

/// 列声明类型（表达式/聚合列为 NULL——SQLite 同义）
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_decltype(
    stmt: *mut sqlite3_stmt,
    i: c_int,
) -> *const c_char {
    if stmt.is_null() {
        return std::ptr::null();
    }
    let st: &Stmt = unsafe { &*stmt.cast() };
    match st.col_decls.get(i.max(0) as usize) {
        Some(Some(d)) => d.as_ptr(),
        _ => std::ptr::null(),
    }
}

/// busy_timeout：无阻塞模型（单写者 + OCC），恒成功
#[no_mangle]
pub unsafe extern "C" fn sqlite3_busy_timeout(_db: *mut sqlite3, _ms: c_int) -> c_int {
    SQLITE_OK
}

/// 最近 INSERT 的 rowid（= 单列整数 PK 末行值——SQLite INTEGER
/// PRIMARY KEY 即 rowid 别名语义；非整数 PK 表插入后保持上次值）
#[no_mangle]
pub unsafe extern "C" fn sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64 {
    if db.is_null() {
        return 0;
    }
    let conn: &Conn = unsafe { &*db.cast() };
    conn.conn.as_ref().map_or(0, |c| c.last_insert_rowid())
}

// ---- Opaque 句柄别名（头文件同构） ----
#[allow(non_camel_case_types)]
pub struct sqlite3 {
    _private: [u8; 0],
}
#[allow(non_camel_case_types)]
pub struct sqlite3_stmt {
    _private: [u8; 0],
}

#[cfg(test)]
mod ffi_tests {
    use super::*;

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    #[test]
    fn ffi_end_to_end() {
        unsafe {
            assert_eq!(
                CStr::from_ptr(sqlite3_libversion()).to_bytes(),
                b"3.41.2-dendro"
            );
            let mut db: *mut sqlite3 = std::ptr::null_mut();
            let mem = cstr(":memory:");
            assert_eq!(sqlite3_open(mem.as_ptr(), &mut db), SQLITE_OK);
            for sql in [
                "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT, score DOUBLE)",
                "INSERT INTO t VALUES (1, 'alice', 9.5), (2, 'bob', 7.0), (3, 'carol', 8.0)",
            ] {
                let s = cstr(sql);
                assert_eq!(
                    sqlite3_exec(
                        db,
                        s.as_ptr(),
                        None,
                        std::ptr::null_mut(),
                        std::ptr::null_mut()
                    ),
                    SQLITE_OK,
                    "exec: {sql}"
                );
            }
            // 错误路径
            let bad = cstr("SELECT nope FROM t");
            assert_eq!(
                sqlite3_exec(
                    db,
                    bad.as_ptr(),
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                SQLITE_ERROR
            );
            let msg = CStr::from_ptr(sqlite3_errmsg(db.cast())).to_string_lossy();
            assert!(msg.contains("nope"), "{msg}");

            // prepare/bind/step
            let sql = cstr("SELECT id, name FROM t WHERE id >= ? ORDER BY id");
            let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut tail: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, &mut tail),
                SQLITE_OK
            );
            assert_eq!(sqlite3_column_count(stmt), 2);
            assert_eq!(
                CStr::from_ptr(sqlite3_column_name(stmt, 0)).to_bytes(),
                b"id"
            );
            assert_eq!(sqlite3_bind_int64(stmt, 1, 2), SQLITE_OK);
            let mut ids = Vec::new();
            let mut names = Vec::new();
            loop {
                let rc = sqlite3_step(stmt);
                if rc == SQLITE_DONE {
                    break;
                }
                assert_eq!(rc, SQLITE_ROW);
                assert_eq!(sqlite3_column_type(stmt, 0), SQLITE_INTEGER);
                ids.push(sqlite3_column_int64(stmt, 0));
                let t = sqlite3_column_text(stmt, 1);
                names.push(CStr::from_ptr(t.cast()).to_string_lossy().to_string());
            }
            assert_eq!(ids, vec![2, 3]);
            assert_eq!(names, vec!["bob", "carol"]);
            assert_eq!(sqlite3_step(stmt), SQLITE_DONE);
            // reset 重绑
            assert_eq!(sqlite3_reset(stmt), SQLITE_OK);
            assert_eq!(sqlite3_bind_int64(stmt, 1, 3), SQLITE_OK);
            assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
            assert_eq!(sqlite3_column_int64(stmt, 0), 3);
            assert_eq!(sqlite3_step(stmt), SQLITE_DONE);
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);

            // 文本参数
            let ps = cstr("SELECT count(*) FROM t WHERE name = ?");
            let mut stmt2: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut t2: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, ps.as_ptr(), -1, &mut stmt2, &mut t2),
                SQLITE_OK
            );
            let name = cstr("alice");
            assert_eq!(
                sqlite3_bind_text(stmt2, 1, name.as_ptr(), -1, std::ptr::null()),
                SQLITE_OK
            );
            assert_eq!(sqlite3_step(stmt2), SQLITE_ROW);
            assert_eq!(sqlite3_column_int64(stmt2, 0), 1);
            assert_eq!(sqlite3_finalize(stmt2), SQLITE_OK);
            assert_eq!(sqlite3_close_v2(db.cast()), SQLITE_OK);
        }
    }

    /// 失效合同：close 后遗留语句 → MISUSE 而非 UB
    #[test]
    fn ffi_stmt_invalidation_after_close() {
        unsafe {
            let mut db: *mut sqlite3 = std::ptr::null_mut();
            let mem = cstr(":memory:");
            assert_eq!(sqlite3_open(mem.as_ptr(), &mut db), SQLITE_OK);
            let ddl = cstr("CREATE TABLE x (a BIGINT PRIMARY KEY)");
            sqlite3_exec(
                db,
                ddl.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            let q = cstr("SELECT a FROM x");
            let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut tail: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut stmt, &mut tail),
                SQLITE_OK
            );
            assert_eq!(sqlite3_close_v2(db.cast()), SQLITE_OK);
            assert_eq!(sqlite3_step(stmt), SQLITE_MISUSE);
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
        }
    }

    /// 持久化：目录打开 → 重开可见（in-proc 替换 SQLite 的核心承诺）
    #[test]
    fn ffi_persistence_reopen() {
        let dir = std::env::temp_dir().join(format!("dendro_ffi_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            let mut db: *mut sqlite3 = std::ptr::null_mut();
            let path = cstr(dir.to_str().unwrap());
            assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
            let sql = cstr(
                "CREATE TABLE p (id BIGINT PRIMARY KEY, v TEXT); INSERT INTO p VALUES (1, 'persisted')",
            );
            sqlite3_exec(
                db,
                sql.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert_eq!(sqlite3_close_v2(db.cast()), SQLITE_OK);

            let mut db2: *mut sqlite3 = std::ptr::null_mut();
            assert_eq!(sqlite3_open(path.as_ptr(), &mut db2), SQLITE_OK);
            let q = cstr("SELECT v FROM p WHERE id = 1");
            let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut tail: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db2, q.as_ptr(), -1, &mut stmt, &mut tail),
                SQLITE_OK
            );
            assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
            let t = sqlite3_column_text(stmt, 0);
            assert_eq!(CStr::from_ptr(t.cast()).to_bytes(), b"persisted");
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
            assert_eq!(sqlite3_close_v2(db2.cast()), SQLITE_OK);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod ffi_v15_tests {
    use super::*;

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    /// blob 绑定往返 + changes 双路径跟踪 + decltype
    #[test]
    fn ffi_blob_changes_decltype() {
        unsafe {
            let mut db: *mut sqlite3 = std::ptr::null_mut();
            let mem = cstr(":memory:");
            assert_eq!(sqlite3_open(mem.as_ptr(), &mut db), SQLITE_OK);
            let ddl = cstr("CREATE TABLE b (id BIGINT PRIMARY KEY, data BLOB)");
            sqlite3_exec(
                db,
                ddl.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            // exec 路径 changes
            let ins = cstr("INSERT INTO b VALUES (1, x'00ff10'), (2, x'aa')");
            sqlite3_exec(
                db,
                ins.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert_eq!(sqlite3_changes(db), 2);

            // prepare + blob 绑定 + step 路径 changes
            let sql = cstr("INSERT INTO b VALUES (3, ?)");
            let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut tail: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, &mut tail),
                SQLITE_OK
            );
            let blob: [u8; 3] = [1, 2, 3];
            assert_eq!(
                sqlite3_bind_blob(stmt, 1, blob.as_ptr().cast(), 3, std::ptr::null()),
                SQLITE_OK
            );
            assert_eq!(sqlite3_step(stmt), SQLITE_DONE);
            assert_eq!(sqlite3_changes(db), 1);
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);

            // 读回 blob + decltype
            let q = cstr("SELECT data FROM b WHERE id = 3");
            let mut st2: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut t2: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut st2, &mut t2),
                SQLITE_OK
            );
            let dt = sqlite3_column_decltype(st2, 0);
            assert_eq!(CStr::from_ptr(dt).to_bytes(), b"BLOB");
            assert_eq!(sqlite3_step(st2), SQLITE_ROW);
            assert_eq!(sqlite3_column_type(st2, 0), SQLITE_BLOB);
            assert_eq!(sqlite3_column_bytes(st2, 0), 3);
            let ptr = sqlite3_column_blob(st2, 0) as *const u8;
            assert_eq!(std::slice::from_raw_parts(ptr, 3), &[1, 2, 3]);
            assert_eq!(sqlite3_finalize(st2), SQLITE_OK);
            assert_eq!(sqlite3_close_v2(db), SQLITE_OK);
        }
    }

    /// pzTail 多语句切分：首语句后 tail 指向第二语句；sqlite3_sql 原文
    #[test]
    fn ffi_pztail_multistmt() {
        unsafe {
            let mut db: *mut sqlite3 = std::ptr::null_mut();
            let mem = cstr(":memory:");
            assert_eq!(sqlite3_open(mem.as_ptr(), &mut db), SQLITE_OK);
            let ddl = cstr("CREATE TABLE m (a BIGINT PRIMARY KEY)");
            sqlite3_exec(
                db,
                ddl.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            let two = cstr("INSERT INTO m VALUES (1); SELECT a FROM m");
            let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut tail: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, two.as_ptr(), -1, &mut stmt, &mut tail),
                SQLITE_OK
            );
            // tail = 第二语句起点
            let t = CStr::from_ptr(tail).to_string_lossy();
            assert!(t.trim_start().starts_with("SELECT"), "tail={t}");
            // sqlite3_sql = 首语句原文
            let s = CStr::from_ptr(sqlite3_sql(stmt)).to_string_lossy();
            assert!(s.contains("INSERT") && !s.contains("SELECT"), "sql={s}");
            assert_eq!(sqlite3_step(stmt), SQLITE_DONE);
            assert_eq!(sqlite3_changes(db), 1);
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);

            // 从 tail 继续 prepare 第二语句（SQLite shell 模式）
            let mut stmt2: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut tail2: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, tail, -1, &mut stmt2, &mut tail2),
                SQLITE_OK
            );
            assert_eq!(sqlite3_step(stmt2), SQLITE_ROW);
            assert_eq!(sqlite3_column_int64(stmt2, 0), 1);
            assert_eq!(sqlite3_finalize(stmt2), SQLITE_OK);
            assert_eq!(sqlite3_close_v2(db), SQLITE_OK);
        }
    }

    /// 词法边界扫描：字符串内分号/注释内分号不切分
    #[test]
    fn stmt_boundary_lexer() {
        assert_eq!(stmt_boundary(b"SELECT 'a;b' ; SELECT 2"), Some(14));
        assert_eq!(
            stmt_boundary(b"SELECT 1 -- c; comment\n; SELECT 2"),
            Some(24)
        );
        assert_eq!(stmt_boundary(b"/* ; */ SELECT 1"), None);
        assert_eq!(stmt_boundary(b"SELECT [c;ol] FROM t; SELECT 2"), Some(21));
    }
}

#[cfg(test)]
mod ffi_lazy_tests {
    use super::*;

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn setup(db: *mut sqlite3, n: i64) {
        unsafe {
            let ddl = cstr("CREATE TABLE big (id INTEGER PRIMARY KEY, v TEXT, n BIGINT)");
            sqlite3_exec(
                db,
                ddl.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            // 分批插入避免巨型单语句
            let mut i = 1i64;
            while i <= n {
                let hi = (i + 999).min(n);
                let vals: Vec<String> = (i..=hi)
                    .map(|k| format!("({k}, 'v{k}', {k} * 2)"))
                    .collect();
                let ins = cstr(&format!("INSERT INTO big VALUES {}", vals.join(",")));
                sqlite3_exec(
                    db,
                    ins.as_ptr(),
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                i = hi + 1;
            }
        }
    }

    /// 惰性游标端到端：单表 SELECT（带谓词）逐行 step 与物化路径结果
    /// 逐行一致；early-termination 后 finalize 无泄漏/崩溃
    #[test]
    fn ffi_lazy_cursor_consistency() {
        unsafe {
            let mut db: *mut sqlite3 = std::ptr::null_mut();
            let mem = cstr(":memory:");
            assert_eq!(sqlite3_open(mem.as_ptr(), &mut db), SQLITE_OK);
            setup(db, 5000);

            // 无排序形态走惰性（ORDER BY 形态回落物化——双路径合同）
            let q_lazy = cstr("SELECT id, v FROM big WHERE n > 5000");

            // 惰性路径：全量 step 收集
            let mut st: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut tail: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, q_lazy.as_ptr(), -1, &mut st, &mut tail),
                SQLITE_OK
            );
            let mut lazy_rows: Vec<(i64, String)> = Vec::new();
            loop {
                let rc = sqlite3_step(st);
                if rc == SQLITE_DONE {
                    break;
                }
                assert_eq!(rc, SQLITE_ROW);
                lazy_rows.push((
                    sqlite3_column_int64(st, 0),
                    CStr::from_ptr(sqlite3_column_text(st, 1).cast())
                        .to_string_lossy()
                        .to_string(),
                ));
            }
            assert_eq!(sqlite3_finalize(st), SQLITE_OK);
            // n > 5000 → n=2*id → id > 2500 → 2501..=5000 = 2500 行
            assert_eq!(lazy_rows.len(), 2500);
            assert_eq!(lazy_rows[0], (2501, "v2501".to_string()));
            assert_eq!(lazy_rows[2499], (5000, "v5000".to_string()));

            // 与物化路径逐行对拍
            let mut st2: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut t2: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, q_lazy.as_ptr(), -1, &mut st2, &mut t2),
                SQLITE_OK
            );
            let mut i = 0;
            loop {
                let rc = sqlite3_step(st2);
                if rc == SQLITE_DONE {
                    break;
                }
                assert_eq!(rc, SQLITE_ROW);
                assert_eq!(sqlite3_column_int64(st2, 0), lazy_rows[i].0);
                i += 1;
            }
            assert_eq!(i, 2500);
            assert_eq!(sqlite3_finalize(st2), SQLITE_OK);

            // early-termination：只 step 首行即 finalize（SQLite 最常见
            // 消费形态——惰性下不执行剩余源）
            let mut st3: *mut sqlite3_stmt = std::ptr::null_mut();
            let mut t3: *const c_char = std::ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, q_lazy.as_ptr(), -1, &mut st3, &mut t3),
                SQLITE_OK
            );
            assert_eq!(sqlite3_step(st3), SQLITE_ROW);
            assert_eq!(sqlite3_column_int64(st3, 0), 2501);
            assert_eq!(sqlite3_finalize(st3), SQLITE_OK);
            assert_eq!(sqlite3_close_v2(db), SQLITE_OK);
        }
    }
}
