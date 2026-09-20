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
    /// 带完整选项打开（内存预算/持久性/阈值等全部旋钮——嵌入调用方
    /// 的资源治理口；Examples: bulk 装载调大 checkpoint_threshold_bytes
    /// 降低 checkpoint 频率，或调小以压 memtx 高水位）
    pub fn open_with(path: impl AsRef<std::path::Path>, opts: crate::DbOptions) -> Result<Self> {
        let db = Database::open(crate::DbOptions {
            store: crate::StoreConfig::LocalDir(path.as_ref().to_path_buf()),
            ..opts
        })?;
        let mut sess = db.new_session();
        sess.dialect = crate::sql::SqlDialect::Sqlite;
        Ok(Self { db, sess })
    }

    /// 内存占用观测（RSS 近似——memtx/节点缓存/段解码活动集；供
    /// 预算控制回路与诊断）
    pub fn memory_usage_bytes(&self) -> u64 {
        let _ = &self.sess;
        crate::engine::proc_rss_bytes().unwrap_or(0)
    }

    /// 主键直读（嵌入式快路径：绕过 SQL 管线的单行点取）。
    /// 语义与 `SELECT * FROM t WHERE <pk> = ?` 一致：memtx ∪ 当前树、
    /// 墓碑隐藏、显式事务读自身写（build_point_view 同口径）。
    /// stprobe 实测 ~29µs SQL 层开销归零——点查 4.5×（37→8µs 量级）
    pub fn find_by_pk(
        &mut self,
        table: &str,
        pk: i64,
    ) -> Result<Option<Vec<crate::types::SqlValue>>> {
        let snapshot = self.sess.implicit_snapshot(&self.db)?;
        let (schema, entry) = crate::sql::scan::resolve_table(&self.db, &self.sess.branch, table)?;
        let key = crate::format::row::encode_key(&[crate::types::SqlValue::Int64(pk)]);
        let b = self.db.branch(&self.sess.branch)?;
        let tm = b.mem.table(entry.id);
        let mut found: Option<std::sync::Arc<Vec<u8>>> = match tm.get(&key, snapshot) {
            Some(v) => Some(v),
            None => {
                // 墓碑判定（与点查路径同语义）：memtx 可见墓碑不得回退树
                let tombstoned = tm.latest_ts(&key).is_some_and(|ts| ts <= snapshot);
                if tombstoned {
                    None
                } else {
                    entry
                        .table_root
                        .as_ref()
                        .and_then(|s| crate::format::hash::Hash::from_base32(s))
                        .map(|r| crate::prolly::cursor::lookup(&self.db.store, &r, &key))
                        .transpose()?
                        .flatten()
                        .map(std::sync::Arc::new)
                }
            }
        };
        // 显式事务读自身写
        if let Some(t) = &self.sess.txn {
            if t.explicit {
                if let Some(m) = t.writes.get(&(entry.id, key.clone())) {
                    match m {
                        crate::prolly::Mutation::Put(v) => {
                            found = Some(std::sync::Arc::new(v.clone()))
                        }
                        crate::prolly::Mutation::Delete => found = None,
                    }
                }
            }
        }
        Ok(found
            .map(|v| crate::sql::scan::row_from_bytes(&schema, &v))
            .transpose()?)
    }

    /// 库句柄直读（嵌入式高级用法/诊断探针——常规路径走 query/execute）
    pub fn db(&self) -> &std::sync::Arc<Database> {
        &self.db
    }

    /// 内存快照 JSON（memprof：包络 + 分用途 meters + 分配器明细；
    /// 嵌入方预算回路/监控的消费口）
    pub fn memory_snapshot_json(&self) -> String {
        let _ = &self.sess;
        crate::memprof::snapshot_json(&crate::memprof::get().snapshot())
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = Database::open(crate::DbOptions {
            store: crate::StoreConfig::LocalDir(path.as_ref().to_path_buf()),
            ..Default::default()
        })?;
        let mut sess = db.new_session();
        // 进程内产品面默认 **SQLite 方言**（与 SQLite 同义的嵌入式
        // 语义；`?` 位置参数可用，`$1` 亦兼容）。引擎层 Session 默认
        // Pg（网络传输由各自 wire 显式声明）——两层默认不同是有意
        sess.dialect = crate::sql::SqlDialect::Sqlite;
        Ok(Self { db, sess })
    }

    pub fn memory() -> Result<Self> {
        let db = Database::open(crate::DbOptions {
            store: crate::StoreConfig::Memory,
            ..Default::default()
        })?;
        let mut sess = db.new_session();
        sess.dialect = crate::sql::SqlDialect::Sqlite;
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
    /// 切换方言（默认 **Sqlite**——进程内语义；需要 PG 生态语义的
    /// 嵌入调用方设 Pg，如 `?` 之外的 `$N` 命名形态差异面）
    pub fn set_dialect(&mut self, d: crate::sql::SqlDialect) {
        self.sess.dialect = d;
    }

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
        self.sess_exec_prepared_mixed(name, params).map(|(r, _)| r)
    }

    /// C ABI 口：预编译执行返回 (结果, 受影响)——step 路径的
    /// sqlite3_changes 跟踪
    pub fn sess_exec_prepared_mixed(
        &mut self,
        name: &str,
        params: &[Value],
    ) -> Result<(QueryResult, i64)> {
        let sql_params = to_sql_values(params);
        let output = self.sess.exec_prepared(name, &sql_params)?;
        let affected = match &output {
            Output::Command { affected: a, .. } => *a as i64,
            _ => 0,
        };
        Ok((to_result(vec![output]), affected))
    }
    pub fn sess_close_prepared(&mut self, name: &str) {
        self.sess.close_prepared(name)
    }

    /// 惰性扫描（SQLite step 语义）：单表 SELECT 形态源直驱——
    /// 首行 O(首批)/early-term/输出内存 O(批)。非该形态返回 None
    ///（调用方回落 [`Self::query`] 全量物化）。sql 应已代入参数
    ///（见 [`Self::substitute_sql`]）。
    pub fn lazy_scan(&mut self, sql: &str) -> Result<Option<LazyScan>> {
        let stmts = crate::sql::parse_batch(sql, self.sess.dialect)?;
        let Some(sqlparser::ast::Statement::Query(q)) = stmts.into_iter().next() else {
            return Ok(None);
        };
        let Some(shape) = lazy_scan_shape(&q) else {
            return Ok(None);
        };
        LazyScan::build(&self.db.clone(), &mut self.sess, &shape)
    }

    /// 占位符代入（`$N`/`?` → 字面量）——惰性路径的参数绑定
    ///（prepared 机制之外的独立口）
    pub fn substitute_sql(&self, sql: &str, params: &[Value]) -> Result<String> {
        let mut stmts = crate::sql::parse_batch(sql, self.sess.dialect)?;
        let Some(stmt) = stmts.first_mut() else {
            return Ok(sql.to_string());
        };
        let sql_values = to_sql_values(params);
        let substituted = crate::sql::substitute_params(stmt.clone(), &sql_values)?;
        Ok(substituted.to_string())
    }

    /// 最近一次成功 INSERT 的 rowid（单列整数 PK 表的末行 PK；
    /// SQLite last_insert_rowid 语义）
    pub fn last_insert_rowid(&self) -> i64 {
        self.sess.last_rowid
    }

    /// 混合执行（查询结果 + 受影响行数一并返回——C ABI 的
    /// sqlite3_changes 跟踪口；Rust 侧 execute/query 各取一半的合并源）
    pub fn exec_mixed(&mut self, sql: &str) -> Result<(QueryResult, i64)> {
        let outputs = self.sess.exec(sql)?;
        let mut affected = 0i64;
        for o in &outputs {
            if let Output::Command { affected: a, .. } = o {
                affected += *a as i64;
            }
        }
        let r = to_result(outputs);
        Ok((r, affected))
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
    // 切换会话用户（S-4 测试面/嵌入式多用户；权限门主体；小写折叠）
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
        // Bytes：\x 前缀 hex 文本回解（cell_to_text 的镜像——blob
        // 参数/列经文本往返的还原臂；缺失曾使 BLOB 列在 embed/FFI
        // 视图里降级 Utf8）
        Some(ColType::Bytes) => match s.strip_prefix("\\x") {
            Some(h) => {
                let bytes = (0..h.len() / 2)
                    .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16))
                    .collect::<std::result::Result<Vec<u8>, _>>();
                match bytes {
                    Ok(b) => SqlValue::Bytes(b),
                    Err(_) => SqlValue::Utf8(s.to_string()),
                }
            }
            None => SqlValue::Utf8(s.to_string()),
        },
        // Utf8/Date/Timestamp 及未知：保持文本（Date/Ts 的文本形
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

// ---------------------------------------------------------------------------
// 惰性扫描游标（v2 / SQLite step 语义）：单表 SELECT 形态源直驱——
// 首行延迟 O(首批)、early-termination（消费者停 = 不再拉源）、输出
// 内存 O(批)。诚实边界：段解码仍整段物化（完整惰性随流式执行器
// C 档）；非此形态返回 None（调用方回落全量物化 query()）。
// ---------------------------------------------------------------------------

/// 形态判定：单表 + 无 join/group/having/distinct/窗口/排序/limit/
/// 集合操作/CTE/子查询位/聚合投影——`SELECT cols FROM t [WHERE p]`
pub fn lazy_scan_shape(q: &sqlparser::ast::Query) -> Option<LazyShape> {
    let sel = match &*q.body {
        sqlparser::ast::SetExpr::Select(sel) => sel,
        _ => return None,
    };
    if q.with.is_some() || q.order_by.is_some() || q.limit_clause.is_some() || q.fetch.is_some() {
        return None;
    }
    if sel.from.len() != 1 || !sel.from[0].joins.is_empty() {
        return None;
    }
    let sqlparser::ast::TableFactor::Table { name, .. } = &sel.from[0].relation else {
        return None;
    };
    let table = name
        .0
        .last()
        .and_then(|p| p.as_ident())
        .map(|i| i.value.clone())?;
    if !matches!(&sel.group_by, sqlparser::ast::GroupByExpr::Expressions(es, _) if es.is_empty())
        || sel.having.is_some()
        || sel.distinct.is_some()
    {
        return None;
    }
    if crate::sql::scan::projection_aggregates(&sel.projection).is_some() {
        return None;
    }
    if let Some(w) = &sel.selection {
        if crate::sql::scan::expr_has_subquery(w) {
            return None;
        }
    }
    // 投影收集（Unnamed/ExprWithAlias；通配形态不支持——列名不可静态定）
    let mut proj = Vec::new();
    let mut names = Vec::new();
    for item in &sel.projection {
        match item {
            sqlparser::ast::SelectItem::UnnamedExpr(e) => {
                proj.push(e.clone());
                names.push(crate::sql::scan::short_str_pub(e));
            }
            sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                proj.push(expr.clone());
                names.push(alias.value.clone());
            }
            _ => return None,
        }
    }
    Some(LazyShape {
        table,
        pred: sel.selection.clone(),
        proj,
        names,
    })
}

/// 惰性形态描述
pub struct LazyShape {
    pub table: String,
    pub pred: Option<sqlparser::ast::Expr>,
    pub proj: Vec<sqlparser::ast::Expr>,
    pub names: Vec<String>,
}

/// 惰性扫描游标：`next_row` 逐行产出（批拉取 + 行级谓词/投影）
pub struct LazyScan {
    src: crate::exec::source::MainPlusDeltaSource,
    src_names: Vec<String>,
    names: Vec<String>,
    pred: Option<crate::sql::scalar::CompiledPredicate>,
    pred_expr: Option<sqlparser::ast::Expr>,
    proj: Vec<sqlparser::ast::Expr>,
    buf: Vec<Vec<SqlValue>>,
    pos: usize,
    current: Option<Vec<SqlValue>>,
    /// 已产出行数（诊断/测试）
    pub produced: u64,
}

impl LazyScan {
    // 从形态 + 已代入参数的 AST 构造（失败返回 None 回落）
    pub(crate) fn build(
        db: &crate::engine::Database,
        sess: &mut crate::engine::Session,
        shape: &LazyShape,
    ) -> Result<Option<Self>> {
        let snapshot = sess.implicit_snapshot(db)?;
        let Some((src_names, src)) =
            crate::sql::scan::lazy_scan_source(db, sess, &shape.table, snapshot)?
        else {
            return Ok(None);
        };
        // 列名解析器（源行布局）
        let resolver_names = src_names.clone();
        let resolve = move |n: &str| -> Option<usize> {
            resolver_names
                .iter()
                .position(|c| c.eq_ignore_ascii_case(n))
        };
        let pred_cp = shape.pred.as_ref().and_then(|p| {
            // 不支持形态 → None：行级 expr 求值兜底
            crate::sql::scalar::compile_predicate_cached(p, &resolve, src_names.len(), &src_names)
                .ok()
        });
        let names = shape.names.clone();
        Ok(Some(Self {
            src,
            src_names,
            names,
            pred: pred_cp,
            pred_expr: shape.pred.clone(),
            proj: shape.proj.clone(),
            buf: Vec::new(),
            pos: 0,
            current: None,
            produced: 0,
        }))
    }

    pub fn column_count(&self) -> usize {
        self.proj.len()
    }

    pub fn column_name(&self, i: usize) -> Option<&str> {
        self.names.get(i).map(|s| s.as_str())
    }

    /// 下一行：None = 耗尽。批拉取 + 谓词短路 + 投影行级求值
    pub fn next_row(&mut self) -> Result<Option<Vec<SqlValue>>> {
        loop {
            if self.pos < self.buf.len() {
                let raw = std::mem::take(&mut self.buf[self.pos]); // 移出——批消费后即弃
                self.pos += 1;
                // 谓词（程序优先；不支持形态走 expr 行级）
                let keep = if let Some(cp) = &self.pred {
                    let mut out = SqlValue::Null;
                    crate::sql::scalar::eval_row(&cp.prog, &raw, &[], &mut out)?;
                    matches!(out, SqlValue::Bool(true))
                } else if let Some(pe) = &self.pred_expr {
                    let names = self.src_names.clone();
                    let resolve = move |n: &str| -> Option<usize> {
                        names.iter().position(|c| c.eq_ignore_ascii_case(n))
                    };
                    matches!(
                        crate::sql::expr::eval(pe, &raw, &resolve)?,
                        SqlValue::Bool(true)
                    )
                } else {
                    true
                };
                if !keep {
                    continue;
                }
                // 投影
                let names = self.src_names.clone();
                let resolve = move |n: &str| -> Option<usize> {
                    names.iter().position(|c| c.eq_ignore_ascii_case(n))
                };
                let mut row = Vec::with_capacity(self.proj.len());
                for e in &self.proj {
                    row.push(crate::sql::expr::eval(e, &raw, &resolve)?);
                }
                self.produced += 1;
                self.current = Some(row);
                return Ok(self.current.clone());
            }
            // 批尽 → 拉下一批
            match self.src.next() {
                Some(Ok(b)) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Some(Err(e)) => return Err(e),
                None => return Ok(None),
            }
        }
    }
}
