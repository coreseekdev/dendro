//! dendro 嵌入式 API——SQLite 风格进程内使用，零网络开销。
//!
//! # 快速开始
//!
//! ```no_run
//! use dendro_core::embed::Connection;
//! let mut conn = Connection::memory().unwrap();
//! conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
//! conn.execute("INSERT INTO t VALUES (1, 'hello')").unwrap();
//! let r = conn.query("SELECT id, name FROM t").unwrap();
//! for row in r.rows() {
//!     println!("{:?}", row);
//! }
//! ```

use crate::engine::{Database, Session};
use crate::types::{Output, SqlValue};
use crate::Result;
use std::sync::Arc;

// ---- 公共类型 ----

/// 绑定参数值
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Integer(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Integer(v as i64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

/// 查询结果（拥有型）
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
}

impl QueryResult {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }
    pub fn column_name(&self, idx: usize) -> &str {
        self.columns.get(idx).map(|s| s.as_str()).unwrap_or("")
    }
    pub fn row(&self, idx: usize) -> Option<&Vec<SqlValue>> {
        self.rows.get(idx)
    }
    pub fn rows(&self) -> &[Vec<SqlValue>] {
        &self.rows
    }
    pub fn get_i64(&self, row: usize, col: usize) -> Option<i64> {
        match self.rows.get(row)?.get(col)? {
            SqlValue::Int64(v) => Some(*v),
            SqlValue::Int32(v) => Some(*v as i64),
            _ => None,
        }
    }
    pub fn get_string(&self, row: usize, col: usize) -> Option<String> {
        match self.rows.get(row)?.get(col)? {
            SqlValue::Utf8(s) => Some(s.clone()),
            _ => None,
        }
    }
    pub fn get_f64(&self, row: usize, col: usize) -> Option<f64> {
        match self.rows.get(row)?.get(col)? {
            SqlValue::Float64(v) => Some(*v),
            SqlValue::Int64(v) => Some(*v as f64),
            _ => None,
        }
    }
    pub fn is_null(&self, row: usize, col: usize) -> bool {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .is_none_or(|v| matches!(v, SqlValue::Null))
    }
}

/// 嵌入式数据库连接
pub struct Connection {
    db: Arc<Database>,
    sess: Session,
}

impl Connection {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = Database::open(crate::DbOptions {
            store: crate::StoreConfig::LocalDir(path.as_ref().to_path_buf()),
            ..Default::default()
        })?;
        let sess = db.new_session();
        Ok(Self { db, sess })
    }

    pub fn memory() -> Result<Self> {
        let db = Database::open(crate::DbOptions {
            store: crate::StoreConfig::Memory,
            ..Default::default()
        })?;
        let sess = db.new_session();
        Ok(Self { db, sess })
    }

    /// 执行 SQL（DDL/DML；返回受影响行数）
    pub fn execute(&mut self, sql: &str) -> Result<u64> {
        let outputs = self.sess.exec(sql)?;
        let mut affected = 0u64;
        for o in &outputs {
            if let Output::Command { affected: a, .. } = o {
                affected += a;
            }
        }
        Ok(affected)
    }

    /// 查询 SQL（SELECT；返回完整结果集）
    /// 切换会话用户（S-4 测试面/嵌入式多用户；权限门主体；小写折叠）
    pub fn set_user(&mut self, user: &str) {
        self.sess.user = crate::sql::privs::norm_user(user);
    }

    pub fn query(&mut self, sql: &str) -> Result<QueryResult> {
        let outputs = self.sess.exec(sql)?;
        Ok(to_result(outputs))
    }

    /// 执行 SQL 返回首行首列 i64
    pub fn query_scalar_i64(&mut self, sql: &str) -> Result<Option<i64>> {
        Ok(self.query(sql)?.get_i64(0, 0))
    }

    /// C ABI（dendro-sqlite）支持面：按名登记/执行/关闭预编译语句——
    /// 跨 FFI 的所有权形态（Rust 借用版 [`Statement`] 无法过边界）
    pub fn sess_prepare(&mut self, name: &str, sql: &str) -> Result<crate::engine::PrepareMeta> {
        self.sess.prepare(name, sql, &[])
    }
    pub fn sess_exec_prepared(&mut self, name: &str, params: &[Value]) -> Result<QueryResult> {
        let sql_params = to_sql_values(params);
        let outputs = self.sess.exec_prepared(name, &sql_params)?;
        Ok(to_result(vec![outputs]))
    }
    pub fn sess_close_prepared(&mut self, name: &str) {
        self.sess.close_prepared(name)
    }

    /// 预编译语句
    pub fn prepare(&mut self, sql: &str) -> Result<Statement<'_>> {
        let name = format!(
            "__e{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        self.sess.prepare(&name, sql, &[])?;
        Ok(Statement {
            sess: &mut self.sess,
            name,
        })
    }

    /// 开启事务（RAII：Drop 未 commit = 自动 ROLLBACK）
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.sess.exec("BEGIN")?;
        Ok(Transaction {
            sess: &mut self.sess,
            done: false,
        })
    }

    pub fn database(&self) -> Arc<Database> {
        self.db.clone()
    }
    pub fn new_session(&mut self) -> Session {
        self.db.new_session()
    }
    pub fn create_branch(&self, name: &str, from: &str) -> Result<()> {
        self.db.create_branch(name, from)
    }
}

/// 预编译语句
pub struct Statement<'conn> {
    sess: &'conn mut Session,
    name: String,
}

impl Statement<'_> {
    pub fn query(&mut self, params: &[Value]) -> Result<QueryResult> {
        let sql_params = to_sql_values(params);
        let outputs = self.sess.exec_prepared(&self.name, &sql_params)?;
        Ok(to_result(vec![outputs]))
    }
    pub fn execute(&mut self, params: &[Value]) -> Result<u64> {
        let sql_params = to_sql_values(params);
        let output = self.sess.exec_prepared(&self.name, &sql_params)?;
        Ok(count_affected(std::slice::from_ref(&output)))
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        self.sess.close_prepared(&self.name);
    }
}

/// 事务（RAII）
pub struct Transaction<'conn> {
    sess: &'conn mut Session,
    done: bool,
}

impl Transaction<'_> {
    pub fn execute(&mut self, sql: &str) -> Result<u64> {
        let outputs = self.sess.exec(sql)?;
        Ok(count_affected(&outputs))
    }
    /// 切换会话用户（S-4 测试面/嵌入式多用户；权限门主体；小写折叠）
    pub fn set_user(&mut self, user: &str) {
        self.sess.user = crate::sql::privs::norm_user(user);
    }

    pub fn query(&mut self, sql: &str) -> Result<QueryResult> {
        let outputs = self.sess.exec(sql)?;
        Ok(to_result(outputs))
    }
    pub fn commit(mut self) -> Result<()> {
        self.sess.exec("COMMIT")?;
        self.done = true;
        Ok(())
    }
    pub fn rollback(mut self) -> Result<()> {
        self.sess.exec("ROLLBACK")?;
        self.done = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.sess.exec("ROLLBACK");
        }
    }
}

// ---- 内部 ----

fn to_sql_values(values: &[Value]) -> Vec<SqlValue> {
    values
        .iter()
        .map(|v| match v {
            Value::Null => SqlValue::Null,
            Value::Integer(i) => SqlValue::Int64(*i),
            Value::Real(f) => SqlValue::Float64(*f),
            Value::Text(s) => SqlValue::Utf8(s.clone()),
            Value::Blob(b) => SqlValue::Bytes(b.clone()),
        })
        .collect()
}

fn count_affected(outputs: &[Output]) -> u64 {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::Command { affected, .. } => Some(*affected),
            _ => None,
        })
        .sum()
}

/// 文本单元格 → 按列型定型（无类型信息时退文本）
fn typed_cell(s: &str, ty: Option<crate::types::ColType>) -> SqlValue {
    use crate::types::ColType;
    match ty {
        Some(ColType::Int32) => s
            .parse::<i32>()
            .map(SqlValue::Int32)
            .unwrap_or_else(|_| SqlValue::Utf8(s.to_string())),
        Some(ColType::Int64) => s
            .parse::<i64>()
            .map(SqlValue::Int64)
            .unwrap_or_else(|_| SqlValue::Utf8(s.to_string())),
        Some(ColType::Float64) => s
            .parse::<f64>()
            .map(SqlValue::Float64)
            .unwrap_or_else(|_| SqlValue::Utf8(s.to_string())),
        Some(ColType::Bool) => match s {
            "true" | "t" => SqlValue::Bool(true),
            "false" | "f" => SqlValue::Bool(false),
            _ => SqlValue::Utf8(s.to_string()),
        },
        // Utf8/Bytes/Date/Timestamp 及未知：保持文本（Date/Ts 的文本形
        // 保留原串——embed 消费方按需再转换）
        _ => SqlValue::Utf8(s.to_string()),
    }
}

fn to_result(outputs: Vec<Output>) -> QueryResult {
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    for o in &outputs {
        if let Output::Rows(rs) = o {
            if columns.is_empty() {
                columns = rs.columns.iter().map(|c| c.name.clone()).collect();
            }
            // 账本 #26（c）：按**列元数据类型**定型——此前对文本化结果
            // 重新猜类型（i64→f64→Utf8 启发式），TEXT 列的 '1' 在 embed
            // 视图里变回 Int64（测试视角污染源）
            let tys: Vec<crate::types::ColType> = rs.columns.iter().map(|c| c.ty).collect();
            for row in rs.text_rows() {
                let sql_row: Vec<SqlValue> = row
                    .iter()
                    .enumerate()
                    .map(|(i, c)| match c {
                        Some(s) => typed_cell(s, tys.get(i).copied()),
                        None => SqlValue::Null,
                    })
                    .collect();
                rows.push(sql_row);
            }
        }
    }
    QueryResult { columns, rows }
}
