//! SQL 层：sqlparser 解析（PG/MySQL 双方言）→ 分发执行（SPEC 07）。
//! v1 执行器为行式（SqlValue），列式 AP 扫描在 dendro-columnar。
#![allow(clippy::type_complexity)]

pub mod agg;
pub mod ddl;
pub mod expr;
pub mod scan;

use crate::engine::{commit_tx, Database, Prepared, Session};
use crate::error::{Result, SqlError};
use crate::types::{ColType, ColumnMeta, Output, RecordSet, SqlValue};
use sqlparser::ast::Statement;
use sqlparser::ast::Value as PV;
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;

/// 方言（由 wire 层设置；默认 PG）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    Pg,
    MySql,
}

impl Session {
    pub fn dialect(&self) -> SqlDialect {
        self.dialect
    }
}

// Session 增加字段（通过在 engine.rs 定义处扩展）；这里用独立实现块无法加字段，
// 因此字段直接在 engine.rs 的 Session 定义里（见 engine.rs）。

/// 投影列名兜底（聚合等表达式的紧凑文本）
pub fn agg_display(e: &sqlparser::ast::Expr) -> String {
    e.to_string()
}

pub(crate) fn parse_batch(sql: &str, d: SqlDialect) -> Result<Vec<Statement>> {
    let dialect = match d {
        SqlDialect::Pg => &PostgreSqlDialect {} as &dyn sqlparser::dialect::Dialect,
        SqlDialect::MySql => &MySqlDialect {},
    };
    // 分支族语句先于 sqlparser（其语法非标准）
    if let Some(st) = branch_statement(sql) {
        return Ok(vec![st]);
    }
    Parser::new(dialect)
        .try_with_sql(sql)
        .map_err(|e| SqlError::syntax(short_err(&e)))?
        .parse_statements()
        .map_err(|e| SqlError::syntax(short_err(&e)))
}

fn short_err(e: &sqlparser::parser::ParserError) -> String {
    let s = e.to_string();
    s.lines().next().unwrap_or("parse error").to_string()
}

/// 分支族语句的轻量预解析 → 合成 Statement::Custom? sqlparser 无自定义——
/// 用内部桥接：翻译为特殊的 Statement 分发标记。
/// 这里直接把分支语句编码成 DDL 风格不现实；改为 exec_batch 内先拦截文本。
fn branch_statement(_sql: &str) -> Option<Statement> {
    None
}

/// 多语句批量执行
pub(crate) fn exec_batch(db: &Database, sess: &mut Session, sql: &str) -> Result<Vec<Output>> {
    // PG 语义：失败事务内只允许 ROLLBACK（25P02）
    if sess.failed_txn {
        let up = sql.trim_start().to_ascii_uppercase();
        if !up.starts_with("ROLLBACK") && !up.starts_with("COMMIT") {
            return Err(SqlError::new("25P02",
                "current transaction is aborted, commands ignored until end of transaction block"));
        }
    }
    let mut outs = Vec::new();
    for raw in split_statements(sql) {
        if branch_sql_kind(&raw).is_some() {
            let out = exec_branch_statement(db, sess, &raw)?;
            outs.extend(out);
            continue;
        }
        if let Some(out) = exec_cursor_statement(db, sess, &raw)? {
            outs.extend(out);
            continue;
        }
        let stmts = parse_batch(&raw, sess.dialect)?;
        if stmts.is_empty() {
            continue;
        }
        for stmt in stmts {
            let out = exec_statement(db, sess, stmt)?;
            if let Some(o) = out {
                outs.push(o);
            }
        }
    }
    Ok(outs)
}

/// 粗分割（sqlparser 也可做，但分支语句要先拦截；sqlparser 分割对自定义语法报错）
fn split_statements(sql: &str) -> Vec<String> {
    // 逐字符扫描，感知单引号/双引号/行注释/块注释/$$ dollar quote
    let mut out = Vec::new();
    let mut cur = String::new();
    let bytes: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    let n = bytes.len();
    while i < n {
        let c = bytes[i];
        if c == '\'' {
            cur.push(c);
            i += 1;
            while i < n {
                cur.push(bytes[i]);
                if bytes[i] == '\'' {
                    if i + 1 < n && bytes[i + 1] == '\'' {
                        cur.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == '"' || c == '`' {
            let q = c;
            cur.push(c);
            i += 1;
            while i < n {
                cur.push(bytes[i]);
                if bytes[i] == q {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == '-' && i + 1 < n && bytes[i + 1] == '-' {
            while i < n && bytes[i] != '\n' {
                cur.push(bytes[i]);
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && bytes[i + 1] == '*' {
            while i < n {
                cur.push(bytes[i]);
                if bytes[i] == '*' && i + 1 < n && bytes[i + 1] == '/' {
                    cur.push('/');
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == '$' {
            // dollar-quoted $$...$$ 或 $tag$...$tag$
            let mut tag = String::from("$");
            let mut j = i + 1;
            while j < n && (bytes[j].is_alphanumeric() || bytes[j] == '_') {
                tag.push(bytes[j]);
                j += 1;
            }
            if j < n && bytes[j] == '$' {
                tag.push('$');
                let open: String = tag.clone();
                // 找闭合
                let rest: String = bytes[j + 1..].iter().collect();
                if let Some(pos) = rest.find(&open) {
                    for &ch in &bytes[i..=j] {
                        cur.push(ch);
                    }
                    let inner: Vec<char> = rest[..pos].chars().collect();
                    cur.extend(inner);
                    cur.push_str(&open);
                    i = j + 1 + pos + open.chars().count();
                    continue;
                }
            }
        }
        if c == ';' {
            out.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BranchKind {
    Create,
    Drop,
    Use,
    Show,
    Merge,
    Checkpoint,
    /// REOPEN BRANCH <name>——毒化写者的 SQL 级恢复入口（第六轮 P1：
    /// reopen_branch 此前零调用方，40003 指引的操作对 SQL 客户端不存在）
    Reopen,
}

pub(crate) fn kind_name(k: BranchKind) -> &'static str {
    match k {
        BranchKind::Create => "CREATE BRANCH",
        BranchKind::Drop => "DROP BRANCH",
        BranchKind::Use => "USE BRANCH",
        BranchKind::Show => "SHOW BRANCHES",
        BranchKind::Merge => "MERGE BRANCH",
        BranchKind::Checkpoint => "CHECKPOINT",
        BranchKind::Reopen => "REOPEN BRANCH",
    }
}

fn branch_sql_kind(sql: &str) -> Option<BranchKind> {
    let up = top_keyword(sql);
    match up.as_str() {
        "CREATE BRANCH" => Some(BranchKind::Create),
        "DROP BRANCH" => Some(BranchKind::Drop),
        "USE BRANCH" => Some(BranchKind::Use),
        "SHOW BRANCHES" => Some(BranchKind::Show),
        "MERGE BRANCH" => Some(BranchKind::Merge),
        "CHECKPOINT" => Some(BranchKind::Checkpoint),
        "REOPEN BRANCH" => Some(BranchKind::Reopen),
        _ => None,
    }
}

/// 提取语句前两个大写关键词（忽略注释/空白）
fn top_keyword(sql: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for tok in sql.split(|c: char| c.is_whitespace()) {
        let t = tok.trim_matches(|c: char| c == '(' || c == ')' || c == ';');
        if t.is_empty() {
            continue;
        }
        words.push(t.to_ascii_uppercase());
        if words.len() == 2 {
            break;
        }
    }
    words.join(" ")
}

fn exec_branch_statement(db: &Database, sess: &mut Session, sql: &str) -> Result<Vec<Output>> {
    // 事务内拒绝会自伤/破坏隔离的分支语句（第九轮 R9-2）：CHECKPOINT 推进
    // covered_min → 本事务提交撞 Q-9 40001；CREATE/DROP/MERGE/REOPEN 是
    // catalog 写且不可回滚（Q-10 事务化前的保守口径）
    if sess.txn.is_some() {
        if let Some(kind) = branch_sql_kind(sql) {
            // SHOW BRANCHES 只读，事务内放行（第十一轮收口：第十轮声称已做
            // 实际未落地——回归 sql_semantics::r10_show_branches_allowed_and_snapshot_lifecycle）
            if !matches!(kind, BranchKind::Show) {
                return Err(SqlError::new(
                    "25001",
                    format!("cannot execute {} inside a transaction", kind_name(kind)),
                ));
            }
        }
    }
    let kind = branch_sql_kind(sql).unwrap();
    let ddl = crate::versioned::Versioned::new(db.store.clone());
    match kind {
        BranchKind::Use => {
            if sess.txn.is_some() {
                return Err(SqlError::new("25001", "cannot switch branch inside a transaction"));
            }
            let name = parse_ident_after(sql, "USE BRANCH")?;
            // 确认存在
            db.branch(&name)?;
            sess.branch = name;
            Ok(vec![Output::Command { tag: "USE".into(), affected: 0 }])
        }
        BranchKind::Create => {
            // CREATE BRANCH [IF NOT EXISTS] name [FROM src]（SPEC 03 §5）
            let text = skip_keyword(sql, "CREATE BRANCH");
            let upper = text.trim_start().to_ascii_uppercase();
            let if_not_exists = upper.starts_with("IF NOT EXISTS");
            let text = if if_not_exists { text.trim_start()[13..].to_string() } else { text };
            let (name, rest) = split_ident(&text);
            let src = match rest.trim_start().strip_prefix("FROM") {
                Some(after) => split_ident(after).0,
                None => "main".to_string(),
            };
            if db.branch_exists(&name) {
                if if_not_exists {
                    return Ok(vec![Output::Command { tag: "CREATE BRANCH".into(), affected: 0 }]);
                }
                return Err(SqlError::duplicate_table(format!("branch \"{name}\" already exists")));
            }
            db.create_branch(&name, &src)?;
            Ok(vec![Output::Command { tag: "CREATE BRANCH".into(), affected: 0 }])
        }
        BranchKind::Drop => {
            let text = skip_keyword(sql, "DROP BRANCH");
            let if_exists = text.trim_start().to_ascii_uppercase().starts_with("IF EXISTS");
            let text = if if_exists { text.trim_start()[9..].to_string() } else { text };
            let (name, _) = split_ident(&text);
            if name == "main" {
                return Err(SqlError::not_supported("cannot drop branch \"main\""));
            }
            let exists = db.manifest().manifest.refs.contains_key(&name);
            if !exists {
                if if_exists {
                    return Ok(vec![Output::Command { tag: "DROP BRANCH".into(), affected: 0 }]);
                }
                return Err(SqlError::undefined_branch(format!("branch \"{name}\" does not exist")));
            }
            let b = db.branch(&name)?;
            b.wal.close();
            // 分支私有对象墓碑化（S-2：此前 DROP 永久泄漏 WAL 段与 fence 对象）。
            // 树 chunk 为跨分支共享内容寻址，不可删（GC 定案 §4.4）。
            db.update_manifest(|m| {
                m.refs.remove(&name);
                let now = crate::engine::now_ms();
                // WAL：全部 epoch 的现存段（低频路径，允许 LIST）
                let mut paths = db.obj.list_prefix(&format!("wal/{name}/")).unwrap_or_default();
                paths.extend(db.obj.list_prefix(&format!("fence/{name}/")).unwrap_or_default());
                for p in paths {
                    if !m.tombstones.iter().any(|t| t.path == p) {
                        m.tombstones.push(crate::objstore::manifest::Tombstone { path: p, at_ms: now });
                    }
                }
                Ok(true)
            })?;
            db.remove_branch_runtime(&name);
            db.gc_sweep().ok();
            Ok(vec![Output::Command { tag: "DROP BRANCH".into(), affected: 0 }])
        }
        BranchKind::Show => {
            let names: Vec<String> = db.manifest().manifest.refs.keys().cloned().collect();
            let mut rows = Vec::new();
            for n in names {
                let h = db.manifest().manifest.refs[&n].clone();
                rows.push(vec![
                    SqlValue::Utf8(n),
                    SqlValue::Utf8(h.commit.clone().unwrap_or_default()),
                    h.parent.map(SqlValue::Utf8).unwrap_or(SqlValue::Null),
                ]);
            }
            Ok(vec![Output::Rows(make_record_set(
                &["branch", "commit", "parent"],
                &[ColType::Utf8, ColType::Utf8, ColType::Utf8],
                rows,
            ))])
        }
        BranchKind::Merge => {
            // MERGE BRANCH src INTO dst
            let text = skip_keyword(sql, "MERGE BRANCH");
            let (src, rest) = split_ident(&text);
            let after = match rest.trim_start().strip_prefix("INTO") {
                Some(a) => a,
                None => return Err(SqlError::syntax("expected INTO in MERGE BRANCH")),
            };
            let (dst, _) = split_ident(after);
            let summary = db.merge_branches(&src, &dst)?;
            Ok(vec![Output::Command { tag: format!("MERGE {summary}"), affected: 0 }])
        }
        BranchKind::Checkpoint => {
            let name = sess.branch.clone();
            db.checkpoint_branch(&name)?;
            Ok(vec![Output::Command { tag: "CHECKPOINT".into(), affected: 0 }])
        }
        BranchKind::Reopen => {
            // REOPEN BRANCH [name]：缺省 = 当前分支
            let text = skip_keyword(sql, "REOPEN BRANCH");
            let name = text
                .split(|c: char| c.is_whitespace() || c == ';')
                .find(|t| !t.is_empty())
                .map(|t| t.to_string())
                .unwrap_or_else(|| sess.branch.clone());
            db.reopen_branch(&name)?;
            Ok(vec![Output::Command { tag: "REOPEN".into(), affected: 0 }])
        }
    }
    .inspect(|_outs| {
        let _ = &ddl;
    })
}

fn skip_keyword(sql: &str, kw: &str) -> String {
    let idx = sql.to_ascii_uppercase().find(kw).map(|i| i + kw.len()).unwrap_or(0);
    sql[idx..].to_string()
}

fn parse_ident_after(sql: &str, kw: &str) -> Result<String> {
    let text = skip_keyword(sql, kw);
    Ok(split_ident(&text).0)
}

/// 取第一个标识符（引号感知），返回 (标识符, 余下文本)
fn split_ident(s: &str) -> (String, String) {
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (rest[..end].to_string(), rest[end + 1..].to_string());
        }
    }
    if let Some(rest) = t.strip_prefix('`') {
        if let Some(end) = rest.find('`') {
            return (rest[..end].to_string(), rest[end + 1..].to_string());
        }
    }
    let end = t
        .find(|c: char| c.is_whitespace() || c == ';' || c == '(')
        .unwrap_or(t.len());
    (t[..end].to_string(), t[end..].to_string())
}

/// 统一结果集构造
pub(crate) fn make_record_set(names: &[&str], tys: &[ColType], rows: Vec<Vec<SqlValue>>) -> RecordSet {
    let columns: Vec<ColumnMeta> =
        names.iter().zip(tys).map(|(n, t)| ColumnMeta { name: n.to_string(), ty: *t }).collect();
    scan::rows_to_record_set(&columns, rows)
}

/// DDL 判定（Q-10：显式事务内拒绝的 catalog 写集合）
fn is_ddl(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::CreateTable { .. }
            | Statement::CreateIndex { .. }
            | Statement::AlterTable { .. }
            | Statement::Drop { .. }
            | Statement::Truncate { .. } // 第二十轮 R20-1：truncate_impl 同样 catalog_commit
    )
}

/// 单语句执行；None = 无输出（如空事务语句内部处理）
pub(crate) fn exec_statement(db: &Database, sess: &mut Session, stmt: Statement) -> Result<Option<Output>> {
    // Q-10（保守口径）：显式事务内拒绝 DDL——catalog 写不经事务写集，
    // 立即生效且 ROLLBACK 不可撤销（第十八轮 R18-2 实证可见性漂移）。
    // v2 事务化 catalog 后放开。
    if sess.txn.is_some() && is_ddl(&stmt) {
        return Err(SqlError::new(
            "25001",
            "transactional DDL not supported: run DDL outside explicit transactions",
        ));
    }
    match stmt {
        Statement::StartTransaction { .. } => {
            if sess.txn.is_some() {
                return Err(SqlError::new("25001", "transaction already active"));
            }
            let b = db.branch(&sess.branch)?;
            // 持 commit_mu 完成"取快照 + 注册"（第十轮 P10-3 竞态修复）：
            // 否则 checkpoint 可能在两步之间判空并截断，冻结读被击穿
            let _g = b.commit_mu.lock();
            let snapshot = b.snapshot();
            // 注册活跃快照（第九轮 R9-1）：checkpoint 的截断水位尊重本事务
            b.active_snaps
                .lock()
                .entry(snapshot)
                .and_modify(|c| *c += 1)
                .or_insert(1);
            let mut txn = crate::memtx::Txn::new(snapshot);
            // 冻结 catalog 根（第七轮 R7-3）：显式事务内的树读以 BEGIN 时的
            // 根为准——否则 overlay 按 BEGIN 快照、树按当前 head，同一 count(*)
            // 会随 checkpoint 推进在事务内翻转
            txn.head_root = b.head.load_full().as_ref().as_ref().map(|c| c.root);
            txn.explicit = true;
            sess.txn = Some(txn);
            drop(_g);
            Ok(Some(Output::Command { tag: "BEGIN".into(), affected: 0 }))
        }
        Statement::Commit { .. } => {
            // PG 语义：aborted 事务上的 COMMIT = 丢弃并回报 ROLLBACK
            //（否则 25P02 门可被绕过：失败后 COMMIT 私运半截写集）
            if sess.failed_txn {
                if let Some(t) = sess.txn.take() {
                    sess.unregister_snapshot(&t.snapshot);
                }
                sess.failed_txn = false;
                return Ok(Some(Output::Command { tag: "ROLLBACK".into(), affected: 0 }));
            }
            let t = sess.txn.take().ok_or_else(|| SqlError::new("25P01", "no transaction"))?;
            sess.unregister_snapshot(&t.snapshot); // COMMIT 必须注销（第十轮 R10-1：此前缺失 → 截断永久跳过/memtx 无界）
            if !t.writes.is_empty() {
                commit_tx(db, &sess.branch, &t)?;
            }
            sess.failed_txn = false;
            Ok(Some(Output::Command { tag: "COMMIT".into(), affected: 0 }))
        }
        Statement::Rollback { .. } => {
            if let Some(t) = sess.txn.take() {
                sess.unregister_snapshot(&t.snapshot); // 第十轮 R10-1：此前缺失
            }
            sess.failed_txn = false;
            Ok(Some(Output::Command { tag: "ROLLBACK".into(), affected: 0 }))
        }
        Statement::Query(q) => {
            if q.with.is_some() {
                return Err(SqlError::not_supported("WITH (CTE)"));
            }
            let snap_tx = sess.implicit_snapshot(db)?;
            let out = scan::exec_query(db, sess, *q, snap_tx)?;
            Ok(Some(out))
        }
        Statement::Insert(insert) => ddl::exec_insert(db, sess, insert),
        Statement::Update(upd) => {
            let name = match &upd.table.relation {
                sqlparser::ast::TableFactor::Table { name, .. } => {
                    name.0.iter().map(|p| p.as_ident().map(|i| i.value.clone()).unwrap_or_default()).collect::<Vec<_>>().join(".")
                }
                other => other.to_string(),
            };
            let assignments: Vec<(sqlparser::ast::Ident, sqlparser::ast::Expr)> = upd
                .assignments
                .iter()
                .map(|a| match a {
                    sqlparser::ast::Assignment {
                        target: sqlparser::ast::AssignmentTarget::ColumnName(col),
                        value,
                        ..
                    } => {
                        let id = col
                            .0
                            .last()
                            .and_then(|p| p.as_ident())
                            .cloned()
                            .unwrap_or_else(|| sqlparser::ast::Ident::new(String::new()));
                        Ok((id, value.clone()))
                    }
                    _ => Err(SqlError::not_supported("tuple assignment")),
                })
                .collect::<Result<Vec<_>>>()?;
            ddl::update_impl(db, sess, &name, assignments, upd.selection.clone())
        }
        Statement::Delete(delete) => ddl::exec_delete(db, sess, delete),
        Statement::CreateTable(create) => ddl::exec_create_table(db, sess, create),
        Statement::Drop { object_type: sqlparser::ast::ObjectType::Table, names, if_exists, .. } => {
            ddl::drop_table_impl(db, sess, names, if_exists)
        }
        Statement::AlterTable(alt) => {
            if let [op] = alt.operations.as_slice() {
                ddl::alter_table_impl(db, sess, alt.name.clone(), op.clone())
            } else {
                Err(SqlError::not_supported("multiple ALTER TABLE operations"))
            }
        }
        Statement::Truncate(tr) => ddl::truncate_impl(db, sess, tr.table_names),
        Statement::Explain { statement, .. } => {
            let inner = *statement;
            let _ = inner;
            Ok(Some(Output::Rows(make_record_set(
                &["QUERY PLAN"],
                &[ColType::Utf8],
                vec![vec![SqlValue::Utf8("Seq Scan (v1: detailed plans pending)".into())]],
            ))))
        }
        Statement::Set(_) => Ok(Some(Output::Command { tag: "SET".into(), affected: 0 })),
        Statement::ShowVariable { variable } => {
            let name = variable
                .iter()
                .map(|i| i.value.clone())
                .collect::<Vec<_>>()
                .join(".")
                .to_ascii_lowercase();
            let v = match name.as_str() {
                "server_version" => "17.2 (dendro 0.1)".to_string(),
                "transaction_isolation" | "transaction isolation level" => "read committed".to_string(),
                "search_path" => "public".to_string(),
                "databasename" | "database" => "cambium".to_string(),
                "max_rows" => "0".to_string(),
                _ => "unset".to_string(),
            };
            Ok(Some(Output::Rows(make_record_set(
                &[&name],
                &[ColType::Utf8],
                vec![vec![SqlValue::Utf8(v)]],
            ))))
        }
        Statement::ShowTables { .. } => {
            let b = db.branch(&sess.branch)?;
            let head = b.head.load_full();
            let catalog = crate::versioned::Versioned::new(db.store.clone());
            let entries = catalog.catalog_entries(head.as_ref().as_ref().map(|c| c.root).as_ref())?;
            let rows = entries
                .into_iter()
                .map(|(n, _)| vec![SqlValue::Utf8(n)])
                .collect();
            Ok(Some(Output::Rows(make_record_set(&["Tables_in_cambium"], &[ColType::Utf8], rows))))
        }
        Statement::ShowDatabases { .. } | Statement::ShowSchemas { .. } => Ok(Some(Output::Rows(
            make_record_set(&["Name"], &[ColType::Utf8], vec![vec![SqlValue::Utf8("cambium".into())]]),
        ))),
        Statement::Flush { .. } => Ok(Some(Output::Command { tag: "OK".into(), affected: 0 })),
        other => Err(SqlError::syntax(format!(
            "unsupported statement: {}",
            stmt_kind(&other)
        ))),
    }
}

fn stmt_kind(s: &Statement) -> String {
    let full = s.to_string();
    full.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

impl Session {
    /// 隐式快照：无显式事务时每语句新快照（READ COMMITTED）
    pub(crate) fn implicit_snapshot(&self, db: &Database) -> Result<u64> {
        if let Some(t) = &self.txn {
            return Ok(t.snapshot);
        }
        let b = db.branch(&self.branch)?;
        Ok(b.snapshot())
    }
}

// —— 预编译（PG extended / MySQL 二进制 v2）——
pub(crate) fn prepare(
    db: &Database,
    sess: &mut Session,
    name: &str,
    sql: &str,
    hint: &[ColType],
) -> Result<crate::engine::PrepareMeta> {
    // 分支语句也可 prepared：包装为文本透传
    let (is_branch, branch_sql) = {
        let k = branch_sql_kind(sql);
        (k.is_some(), sql.to_string())
    };
    let (stmt, params): (Statement, usize) = if is_branch {
        (dummy_branch_statement(), 0)
    } else {
        let mut stmts = parse_batch(sql, sess.dialect)?;
        if stmts.len() != 1 {
            return Err(SqlError::syntax("prepared statement must be a single statement"));
        }
        let params = count_placeholders(&mut stmts[0]);
        (stmts.into_iter().next().unwrap(), params)
    };
    // 参数类型：wire hint 优先；否则按语句上下文推断（INSERT 列/UPDATE SET/WHERE 比较）
    let param_types: Vec<ColType> = {
        let inferred = infer_param_types(db, sess, &stmt, params);
        match (hint.len() >= params && !hint.is_empty(), inferred) {
            (true, _) => hint[..params].to_vec(),
            (_, Some(v)) => v,
            (_, None) => vec![ColType::Utf8; params],
        }
    };
    // 结果列推断：SELECT 尝试执行 describe（用空结果路径）
    let result_columns = describe_result(db, sess, &stmt)?;
    let meta = crate::engine::PrepareMeta { param_types, result_columns };
    sess.prepared.insert(name.to_string(), Prepared { sql: branch_sql, stmt, param_types: meta.param_types.clone(), result_columns: meta.result_columns.clone() });
    Ok(meta)
}

fn dummy_branch_statement() -> Statement {
    // 分支语句不进 sqlparser；prepared 时以 Flush 占位，执行时按原文走 branch 路径
    sqlparser::ast::Statement::Flush {
        object_type: sqlparser::ast::FlushType::BinaryLogs,
        location: None,
        channel: None,
        read_lock: false,
        export: false,
        tables: vec![],
    }
}

fn count_placeholders(stmt: &mut Statement) -> usize {
    use sqlparser::ast::visit_expressions_mut;
    use sqlparser::ast::Expr;
    use std::ops::ControlFlow;
    let mut max = 0usize;
    let _ = visit_expressions_mut(stmt, |e: &mut Expr| {
        if let Expr::Value(vws) = e {
            if let PV::Placeholder(id) = &vws.value {
                let n = id.trim_start_matches('$').trim_start_matches('?').parse::<usize>().unwrap_or(0);
                max = max.max(n);
            }
        }
        ControlFlow::<()>::Continue(())
    });
    max
}

/// 按语句上下文推断 $n 的类型（真实 PG 的 unknown 参数解析行为）
fn infer_param_types(db: &Database, sess: &Session, stmt: &Statement, n: usize) -> Option<Vec<ColType>> {
    use sqlparser::ast::Expr;
    let mut out: Vec<Option<ColType>> = vec![None; n];

    let mut set = |idx: usize, ty: ColType| {
        if idx >= 1 && idx <= n {
            out[idx - 1] = Some(ty);
        }
    };
    // 表达式侧：col <op> $n 或 $n <op> col
    fn from_binary(e: &Expr, col_of: &dyn Fn(&str) -> Option<ColType>, set: &mut dyn FnMut(usize, ColType)) {
        match e {
            Expr::BinaryOp { left, op: sqlparser::ast::BinaryOperator::Or, right } => {
                from_binary(left, col_of, set);
                from_binary(right, col_of, set);
            }
            Expr::BinaryOp { left, right, .. } => {
                let ph = |x: &Expr| -> Option<usize> {
                    if let Expr::Value(vws) = x {
                        if let sqlparser::ast::Value::Placeholder(id) = &vws.value {
                            return id.trim_start_matches('$').trim_start_matches('?').parse::<usize>().ok();
                        }
                    }
                    None
                };
                if let Some(i) = ph(left) {
                    if let Expr::Identifier(id) = right.as_ref() {
                        if let Some(t) = col_of(&id.value) {
                            set(i, t);
                        }
                    }
                }
                if let Some(i) = ph(right) {
                    if let Expr::Identifier(id) = left.as_ref() {
                        if let Some(t) = col_of(&id.value) {
                            set(i, t);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let _ = &mut set;
    match stmt {
        Statement::Insert(ins) => {
            let name = match &ins.table {
                sqlparser::ast::TableObject::TableName(n) => n
                    .0
                    .iter()
                    .filter_map(|p| p.as_ident())
                    .map(|i| i.value.clone())
                    .collect::<Vec<_>>()
                    .join("."),
                _ => return None,
            };
            let (schema, _) = scan::resolve_table(db, &sess.branch, &name).ok()?;
            let col_idx: Vec<usize> = if ins.columns.is_empty() {
                (0..schema.columns.len()).collect()
            } else {
                ins.columns
                    .iter()
                    .filter_map(|i| i.0.first().and_then(|p| p.as_ident()))
                    .filter_map(|i| schema.col_index(&i.value))
                    .collect()
            };
            if let Some(src) = ins.source.as_ref() {
                if let sqlparser::ast::SetExpr::Values(values) = src.body.as_ref() {
                    if let Some(first) = values.rows.first() {
                        for (vi, item) in first.iter().enumerate() {
                            if let Expr::Value(vws) = item {
                                if let sqlparser::ast::Value::Placeholder(id) = &vws.value {
                                    if let Ok(i) = id.trim_start_matches('$').trim_start_matches('?').parse::<usize>() {
                                        if let Some(&ci) = col_idx.get(vi) {
                                            set(i, schema.columns[ci].ty);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Statement::Update(upd) => {
            let name = match &upd.table.relation {
                sqlparser::ast::TableFactor::Table { name, .. } => name
                    .0
                    .iter()
                    .filter_map(|p| p.as_ident())
                    .map(|i| i.value.clone())
                    .collect::<Vec<_>>()
                    .join("."),
                _ => return None,
            };
            let (schema, _) = scan::resolve_table(db, &sess.branch, &name).ok()?;
            let col_of = |c: &str| schema.col_index(c).map(|i| schema.columns[i].ty);
            for a in &upd.assignments {
                if let sqlparser::ast::AssignmentTarget::ColumnName(col) = &a.target {
                    let cname = col.0.last().and_then(|p| p.as_ident()).map(|i| i.value.clone()).unwrap_or_default();
                    from_binary(&a.value, &col_of, &mut set);
                    let _ = cname;
                }
            }
            if let Some(sel) = &upd.selection {
                from_binary(sel, &col_of, &mut set);
            }
        }
        Statement::Query(q) => {
            if let sqlparser::ast::SetExpr::Select(sel) = q.body.as_ref() {
                let tables = &sel.from;
                if let Some(twj) = tables.first() {
                    if let sqlparser::ast::TableFactor::Table { name, .. } = &twj.relation {
                        let full = name
                            .0
                            .iter()
                            .filter_map(|p| p.as_ident())
                            .map(|i| i.value.clone())
                            .collect::<Vec<_>>()
                            .join(".");
                        if let Ok((schema, _)) = scan::resolve_table(db, &sess.branch, &full) {
                            let col_of = |c: &str| schema.col_index(c).map(|i| schema.columns[i].ty);
                            if let Some(w) = &sel.selection {
                                from_binary(w, &col_of, &mut set);
                            }
                        }
                    }
                }
            }
        }
        _ => return None,
    }
    if out.iter().all(|o| o.is_none()) {
        None
    } else {
        Some(out.into_iter().map(|o| o.unwrap_or(ColType::Utf8)).collect())
    }
}

fn describe_result(_db: &Database, _sess: &mut Session, stmt: &Statement) -> Result<Vec<ColumnMeta>> {
    match stmt {
        Statement::Query(q) => scan::describe_query(_db, _sess, q.as_ref()),
        _ => Ok(vec![]),
    }
}

pub(crate) fn exec_prepared(db: &Database, sess: &mut Session, name: &str, params: &[SqlValue]) -> Result<Output> {
    let p = sess
        .prepared
        .get(name)
        .ok_or_else(|| SqlError::new("26000", "prepared statement does not exist"))?
        .clone();
    if params.len() != p.param_types.len() {
        return Err(SqlError::new(
            "08P01",
            format!("bind parameter mismatch: {}/{}", params.len(), p.param_types.len()),
        ));
    }
    if branch_sql_kind(&p.sql).is_some() {
        let outs = exec_branch_statement(db, sess, &p.sql)?;
        return outs.into_iter().next().ok_or_else(|| SqlError::internal("empty branch output"));
    }
    let stmt = substitute_params(p.stmt, params)?;
    match exec_statement(db, sess, stmt)? {
        Some(o) => Ok(o),
        None => Ok(Output::Command { tag: "OK".into(), affected: 0 }),
    }
}

/// Placeholder("$1"/"?" ) → 参数字面量（copy 语句级 AST）
fn substitute_params(mut stmt: Statement, params: &[SqlValue]) -> Result<Statement> {
    use sqlparser::ast::visit_expressions_mut;
    use sqlparser::ast::Expr;
    use std::ops::ControlFlow;
    let _ = visit_expressions_mut(&mut stmt, |e: &mut Expr| {
        if let Expr::Value(vws) = e {
            if let PV::Placeholder(id) = &vws.value {
                let n = id.trim_start_matches('$').trim_start_matches('?').parse::<usize>().unwrap_or(1);
                let v = params.get(n - 1).cloned().unwrap_or(SqlValue::Null);
                let lit = expr::value_to_value_expr(&v);
                vws.value = lit;
                vws.span = sqlparser::tokenizer::Span::empty();
            }
        }
        ControlFlow::<()>::Continue(())
    });
    Ok(stmt)
}


// ---------------------------------------------------------------------------
// 游标（Q-1b v1：INSENSITIVE / READ ONLY / 会话级——DECLARE 时物化结果集）
// ---------------------------------------------------------------------------

/// 在原文中定位第 `n` 个 token（0-based，按空白切分与 toks 一致）的
/// 字节起始处；越界返回空串
fn locate_token_slice<'a>(sql: &'a str, toks: &[String], n: usize) -> &'a str {
    // 与 toks 相同的切分方式前进，记第 n 个 token 的字节起点
    let mut offset = 0usize;
    for (idx, tok) in sql.split_whitespace().enumerate() {
        let start = offset + sql[offset..].find(tok).unwrap_or(0);
        if idx == n {
            return &sql[start..];
        }
        offset = start + tok.len();
    }
    let _ = toks;
    ""
}

/// 识别游标语句；返回 None = 非游标语句
fn cursor_sql_kind(sql: &str) -> Option<CursorStmt> {
    let toks: Vec<String> = sql
        .split_whitespace()
        .map(|t| t.trim_matches(';').to_string())
        .collect();
    if toks.is_empty() {
        return None;
    }
    if toks[0].eq_ignore_ascii_case("DECLARE") && toks.len() >= 4 && toks[2].eq_ignore_ascii_case("CURSOR") {
        let name = toks[1].clone();
        // DECLARE name CURSOR FOR <query>（"FOR" 可选，PG 兼容）。
        // query 提取按 token 起始字节定位（第十/十八轮：splitn 逐字符切分
        // 在连续空白/多空格下错位）
        let rest_start = if toks[3].eq_ignore_ascii_case("FOR") { 4 } else { 3 };
        let query = locate_token_slice(sql, &toks, rest_start)
            .trim()
            .trim_end_matches(';')
            .to_string();
        return Some(CursorStmt::Declare(name, query));
    }
    if toks[0].eq_ignore_ascii_case("FETCH") && toks.len() >= 2 {
        // FETCH ALL FROM name | FETCH n [FROM] name | FETCH NEXT [FROM] name
        let mut i = 1;
        let mut n: Option<usize> = None;
        if let Ok(v) = toks[i].parse::<usize>() {
            n = Some(v);
            i += 1;
        } else if toks[i].eq_ignore_ascii_case("NEXT") {
            n = Some(1);
            i += 1;
        } else if toks[i].eq_ignore_ascii_case("ALL") {
            n = None;
            i += 1;
        }
        if toks.get(i).map(|t| t.eq_ignore_ascii_case("FROM")).unwrap_or(false) {
            i += 1;
        }
        let name = toks.get(i)?.clone();
        return Some(CursorStmt::Fetch(name, n));
    }
    if toks[0].eq_ignore_ascii_case("CLOSE") && toks.len() >= 2 {
        return Some(CursorStmt::Close(toks[1].clone()));
    }
    None
}

pub(crate) enum CursorStmt {
    /// DECLARE name CURSOR [FOR] query
    Declare(String, String),
    /// FETCH [n|ALL|NEXT] [FROM] name
    Fetch(String, Option<usize>),
    /// CLOSE name
    Close(String),
}

/// 执行游标语句；None = 非游标语句
pub(crate) fn exec_cursor_statement(
    db: &Database,
    sess: &mut Session,
    sql: &str,
) -> Result<Option<Vec<Output>>> {
    let Some(kind) = cursor_sql_kind(sql) else {
        return Ok(None);
    };
    match kind {
        CursorStmt::Declare(name, query) => {
            // 只接受查询（第十八轮 R18-4：先执行后报错会有 DML 副作用）
            if !query.trim_start().to_ascii_uppercase().starts_with("SELECT") {
                return Err(SqlError::not_supported(
                    "DECLARE CURSOR requires a SELECT query",
                ));
            }
            let outs = exec_batch(db, sess, &query)?;
            let mut record_set = None;
            for o in outs {
                if let Output::Rows(rs) = o {
                    record_set = Some(rs);
                }
            }
            match record_set {
                Some(rs) => {
                    // Q-1 上界护栏：游标数上限 255（PG 风格），防会话级
                    // 物化结果集无界累积
                    if sess.cursors.len() >= 255 && !sess.cursors.contains_key(&name.to_ascii_lowercase()) {
                        return Err(SqlError::new(
                            "53310",
                            "too many cursors (max 255 per session); close one first",
                        ));
                    }
                    sess.cursors.insert(name.to_ascii_lowercase(), (rs, 0));
                    Ok(Some(vec![Output::Command { tag: "DECLARE CURSOR".into(), affected: 0 }]))
                }
                None => Err(SqlError::not_supported("DECLARE CURSOR requires a query returning rows")),
            }
        }
        CursorStmt::Fetch(name, count) => {
            let key = name.to_ascii_lowercase();
            let Some((rs, pos)) = sess.cursors.get_mut(&key) else {
                return Err(SqlError::new("34000", format!("cursor \"{name}\" does not exist")));
            };
            let total = rs.total_rows();
            let take = count.unwrap_or(total);
            let take = take.min(total - (*pos).min(total));
            let start = (*pos).min(total);
            // 抽取 [start, start+take) 行（text 行再转 SqlValue）
            let text_rows = rs.text_rows();
            let rows: Vec<Vec<SqlValue>> = text_rows[start..start + take]
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| match c {
                            Some(s) => SqlValue::Utf8(s.clone()),
                            None => SqlValue::Null,
                        })
                        .collect()
                })
                .collect();
            let out = crate::sql::scan::rows_to_record_set(&rs.columns, rows);
            *pos += take;
            if *pos >= total {
                // 读完：PG 语义 cursor 仍存在直到 CLOSE，但为免悬挂状态这里保留
            }
            Ok(Some(vec![Output::Rows(out)]))
        }
        CursorStmt::Close(name) => {
            let key = name.to_ascii_lowercase();
            if sess.cursors.remove(&key).is_none() {
                return Err(SqlError::new("34000", format!("cursor \"{name}\" does not exist")));
            }
            Ok(Some(vec![Output::Command { tag: "CLOSE CURSOR".into(), affected: 0 }]))
        }
    }
}
