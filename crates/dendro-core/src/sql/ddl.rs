//! DDL/DML：CREATE/DROP/ALTER/TRUNCATE TABLE、INSERT、UPDATE、DELETE。
//!
//! 提交路径：写集进会话事务 → commit_tx（OCC 验证 + memtx 安装 + WAL）。
//! DDL 直接改 catalog 树并产生新 commit（树状态变更），manifest CAS 推进。

use super::expr;
use super::scan;
use crate::engine::{commit_tx, now_ms, Database, Session};
use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::format::row::{encode_key, encode_row};
use crate::memtx::Txn;
use crate::types::{ColType, ColumnMeta, Output, SqlValue};
use crate::versioned::{ColumnDef, TableEntry, TableSchema};
use sqlparser::ast::{ColumnDef as PColumnDef, Expr, Ident, Insert, ObjectName, TableConstraint};
use std::collections::HashSet;

pub(crate) fn exec_create_table(db: &Database, sess: &mut Session, create: sqlparser::ast::CreateTable) -> Result<Option<Output>> {
    let name = object_name(&create.name);
    let short = name.rsplit('.').next().unwrap_or(&name).to_string();
    let (cols, pk) = translate_columns(&create.columns, &create.constraints)?;
    let schema = TableSchema { name: short.clone(), columns: cols, pk };
    // 已存在检查
    {
        let (ver, _) = scan_catalog(db, sess)?;
        let _ = ver;
    }
    if let Ok((_, entry)) = scan::resolve_table(db, sess, &short) {
        let _ = entry;
        if create.if_not_exists {
            return Ok(Some(Output::Command { tag: "CREATE TABLE".into(), affected: 0 }));
        }
        return Err(SqlError::duplicate_table(format!("relation \"{short}\" already exists")));
    }
    // 目录变更：catalog 树
    let b = db.branch(&sess.branch)?;
    let _g = b.commit_mu.lock();
    // 取表 id：在 catalog 树自身的条目上分配（树随 checkpoint 提交，
    // 不依赖 manifest 计数器；多分支并发建表由 merge 的键冲突消解）
    let tid = {
        let head = b.head.load_full();
        let catalog = crate::versioned::Versioned::new(db.store.clone());
        let entries = catalog.catalog_entries(head.as_ref().as_ref().map(|c| c.root).as_ref())?;
        entries.iter().map(|(_, e)| e.id).max().unwrap_or(0) + 1
    };
    let mut session_chunks: HashSet<Hash> = HashSet::new();
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    let head = b.head.load_full();
    let old_catalog = head.as_ref().as_ref().map(|c| c.root);
    let entry = TableEntry {
        name: short.clone(),
        schema_addr: schema.addr_of().to_base32(),
        table_root: None,
        row_count: 0,
        id: tid,
        col_segments: Vec::new(),
        col_deletes: Vec::new(),
        col_rows: 0,
    };
    // schema chunk 先写
    db.cas.put_batch(&[schema.to_chunk()], &mut session_chunks).map_err(SqlError::from)?;
    let new_catalog = catalog.apply_catalog(old_catalog.as_ref(), vec![(short.clone(), Some(entry))], &mut session_chunks)?;
    let commit = crate::versioned::commit::Commit {
        root: new_catalog.ok_or_else(|| SqlError::internal("empty catalog"))?,
        parents: head.iter().map(|c| c.addr()).collect(),
        height: head.as_ref().as_ref().map(|c| c.height).unwrap_or(0) + 1,
        ts_ms: now_ms(),
        branch: sess.branch.clone(),
        author: "dendro".into(),
        message: format!("CREATE TABLE {short}"),
    };
    let cchunk = commit.encode();
    db.cas.put_batch(&[cchunk], &mut session_chunks).map_err(SqlError::from)?;
    b.head.store(std::sync::Arc::new(Some(commit.clone())));
    let seq = b.alloc_seq();
    let ck = crate::wal::CheckpointRecord {
        catalog_root: commit.root,
        commit_addr: commit.addr(),
        seq_covered: b.watermark.load(std::sync::atomic::Ordering::Acquire),
    };
    b.wal.append(crate::wal::FrameType::Checkpoint, seq, &crate::wal::encode_checkpoint(&ck), db.opts.durability)?;
    let seg_now = b.wal.current_seg().saturating_sub(1);
    let covered = ck.seq_covered;
    let bname = sess.branch.clone();
    db.update_manifest(|m| {
        let h = m.refs.get_mut(&bname).ok_or_else(|| SqlError::internal("branch vanished"))?;
        h.commit = Some(commit.addr().to_base32());
        h.wal_seg = seg_now.max(h.wal_seg);
        h.covered_seq = covered;
        Ok(true)
    })?;
    Ok(Some(Output::Command { tag: "CREATE TABLE".into(), affected: 0 }))
}

fn scan_catalog(db: &Database, sess: &Session) -> Result<(u64, ())> {
    let b = db.branch(&sess.branch)?;
    let head = b.head.load_full();
    let _ = head;
    Ok((0, ()))
}

fn object_name(n: &ObjectName) -> String {
    n.0
        .iter()
        .map(|p| p.as_ident().map(|i| i.value.clone()).unwrap_or_else(|| p.to_string()))
        .collect::<Vec<_>>()
        .join(".")
}

fn translate_columns(cols: &[PColumnDef], constraints: &[TableConstraint]) -> Result<(Vec<ColumnDef>, Vec<u16>)> {
    let mut out = Vec::new();
    let mut pk: Vec<u16> = Vec::new();
    for c in cols {
        let ty = ColType::from_parse(&c.data_type.to_string())
            .ok_or_else(|| SqlError::not_supported(format!("type {}", c.data_type)))?;
        let mut nullable = true;
        let mut inline_pk = false;
        for opt in &c.options {
            match opt.option {
                sqlparser::ast::ColumnOption::NotNull => nullable = false,
                sqlparser::ast::ColumnOption::PrimaryKey(_) => inline_pk = true,
                sqlparser::ast::ColumnOption::Unique(_) => {
                    return Err(SqlError::not_supported("UNIQUE constraint (v1: primary key only)"))
                }
                sqlparser::ast::ColumnOption::Default(_) => { /* 接受但 v1 忽略 */ }
                _ => {}
            }
        }
        out.push(ColumnDef { name: c.name.value.clone(), ty, nullable });
        if inline_pk {
            pk.push((out.len() - 1) as u16);
            out.last_mut().unwrap().nullable = false;
        }
    }
    for con in constraints {
        if let TableConstraint::PrimaryKey(pkcon) = con {
            for ic in &pkcon.columns {
                let name = match &ic.column.expr {
                    Expr::Identifier(id) => id.value.clone(),
                    other => other.to_string(),
                };
                let low = name.to_ascii_lowercase();
                let idx = out
                    .iter()
                    .position(|c| c.name.to_ascii_lowercase() == low)
                    .ok_or_else(|| SqlError::undefined_column(format!("pk column {name}")))?;
                pk.push(idx as u16);
                out[idx].nullable = false;
            }
        } else if matches!(con, TableConstraint::Unique(_)) {
            return Err(SqlError::not_supported("UNIQUE constraint (v1: primary key only)"));
        }
    }
    Ok((out, pk))
}

/// catalog 变更 + 新 commit + manifest 推进的公共路径
pub(crate) fn catalog_commit(
    db: &Database,
    sess: &Session,
    changes: Vec<(String, Option<TableEntry>)>,
    message: &str,
) -> Result<()> {
    let b = db.branch(&sess.branch)?;
    let _g = b.commit_mu.lock();
    let mut session_chunks: HashSet<Hash> = HashSet::new();
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    let head = b.head.load_full();
    let old_catalog = head.as_ref().as_ref().map(|c| c.root);
    // schema chunk 写盘
    let mut schema_chunks: Vec<crate::objstore::cas::Chunk> = Vec::new();
    for (_, e) in &changes {
        if let Some(e) = e {
            let h = Hash::from_base32(&e.schema_addr).ok_or_else(|| SqlError::internal("bad schema addr"))?;
            if !db.cas.has(&h) {
                // schema chunk 缺失（新表/改表）：由调用方提前放入 —— 这里从 catalog 反查无需写
            }
        }
    }
    let _ = &mut schema_chunks;
    let new_catalog = catalog.apply_catalog(old_catalog.as_ref(), changes, &mut session_chunks)?;
    let commit = crate::versioned::commit::Commit {
        root: new_catalog.ok_or_else(|| SqlError::internal("empty catalog"))?,
        parents: head.iter().map(|c| c.addr()).collect(),
        height: head.as_ref().as_ref().map(|c| c.height).unwrap_or(0) + 1,
        ts_ms: now_ms(),
        branch: sess.branch.clone(),
        author: "dendro".into(),
        message: message.into(),
    };
    let cchunk = commit.encode();
    db.cas.put_batch(&[cchunk], &mut session_chunks).map_err(SqlError::from)?;
    b.head.store(std::sync::Arc::new(Some(commit.clone())));
    let seq = b.alloc_seq();
    let ck = crate::wal::CheckpointRecord {
        catalog_root: commit.root,
        commit_addr: commit.addr(),
        seq_covered: b.watermark.load(std::sync::atomic::Ordering::Acquire),
    };
    b.wal.append(crate::wal::FrameType::Checkpoint, seq, &crate::wal::encode_checkpoint(&ck), db.opts.durability)?;
    let seg_now = b.wal.current_seg().saturating_sub(1);
    let covered = ck.seq_covered;
    let bname = sess.branch.clone();
    db.update_manifest(|m| {
        let h = m.refs.get_mut(&bname).ok_or_else(|| SqlError::internal("branch vanished"))?;
        h.commit = Some(commit.addr().to_base32());
        h.wal_seg = seg_now.max(h.wal_seg);
        h.covered_seq = covered;
        Ok(true)
    })?;
    Ok(())
}

pub(crate) fn drop_table_impl(
    db: &Database,
    sess: &mut Session,
    names: Vec<ObjectName>,
    if_exists: bool,
) -> Result<Option<Output>> {
    let mut changes = Vec::new();
    for n in names {
        let full = object_name(&n);
        let short = full.rsplit('.').next().unwrap_or(&full).to_string();
        match scan::resolve_table(db, sess, &short) {
            Ok((_, entry)) => {
                let b = db.branch(&sess.branch)?;
                b.mem.remove_table(entry.id);
                changes.push((short.clone(), None));
            }
            Err(e) => {
                if !if_exists {
                    return Err(e);
                }
            }
        }
    }
    if changes.is_empty() {
        return Ok(Some(Output::Command { tag: "DROP TABLE".into(), affected: 0 }));
    }
    catalog_commit(db, sess, changes, "DROP TABLE")?;
    Ok(Some(Output::Command { tag: "DROP TABLE".into(), affected: 0 }))
}

pub(crate) fn alter_table_impl(
    db: &Database,
    sess: &mut Session,
    name: ObjectName,
    op: sqlparser::ast::AlterTableOperation,
) -> Result<Option<Output>> {
    let full = object_name(&name);
    let short = full.rsplit('.').next().unwrap_or(&full).to_string();
    match op {
        sqlparser::ast::AlterTableOperation::AddColumn { column_def, .. } => {
            let (mut cols, pk) = translate_columns(&[column_def], &[])?;
            let (_, entry) = scan::resolve_table(db, sess, &short)?;
            let catalog = crate::versioned::Versioned::new(db.store.clone());
            let mut schema = catalog.load_schema(&entry.schema_addr)?;
            schema.columns.append(&mut cols);
            schema.pk = pk; // AddColumn 无 pk
            db.cas
                .put_batch(&[schema.to_chunk()], &mut HashSet::new())
                .map_err(SqlError::from)?;
            let ne = TableEntry { schema_addr: schema.addr_of().to_base32(), ..entry };
            catalog_commit(db, sess, vec![(short.clone(), Some(ne))], "ALTER TABLE ADD COLUMN")?;
            Ok(Some(Output::Command { tag: "ALTER TABLE".into(), affected: 0 }))
        }
        other => Err(SqlError::not_supported(format!("ALTER TABLE: {other}"))),
    }
}

pub(crate) fn truncate_impl(
    db: &Database,
    sess: &mut Session,
    tables: Vec<sqlparser::ast::TruncateTableTarget>,
) -> Result<Option<Output>> {
    let snapshot = sess.implicit_snapshot(db)?;
    let mut txn = sess.txn.take().unwrap_or_else(|| Txn::new(snapshot));
    let mut changes = Vec::new();
    for t in tables {
        let full = object_name(&t.name);
        let short = full.rsplit('.').next().unwrap_or(&full).to_string();
        let (_, entry) = scan::resolve_table(db, sess, &short)?;
        // 全表删除：扫描可见行全部写 tombstone
        let tv = table_scan_pub(db, sess, &short, snapshot)?;
        for row in &tv.rows {
            let pk_vals: Vec<SqlValue> = entry_pk(&schema_of(db, sess, &entry)?, row);
            txn.delete(entry.id, encode_key(&pk_vals));
        }
        let mut ne = entry.clone();
        ne.row_count = 0;
        changes.push((short, Some(ne)));
    }
    if !txn.explicit {
        commit_tx(db, &sess.branch, &txn)?;
    } else {
        sess.txn = Some(txn);
    }
    catalog_commit(db, sess, changes, "TRUNCATE")?;
    Ok(Some(Output::Command { tag: "TRUNCATE TABLE".into(), affected: 0 }))
}

fn schema_of(db: &Database, _sess: &Session, entry: &TableEntry) -> Result<TableSchema> {
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    catalog.load_schema(&entry.schema_addr)
}

fn entry_pk(schema: &TableSchema, row: &[SqlValue]) -> Vec<SqlValue> {
    schema.pk.iter().map(|&i| row[i as usize].clone()).collect()
}

/// INSERT INTO t [(cols)] VALUES (...), (...) | DEFAULT VALUES
pub(crate) fn exec_insert(db: &Database, sess: &mut Session, insert: Insert) -> Result<Option<Output>> {
    let table = match &insert.table {
        sqlparser::ast::TableObject::TableName(n) => object_name(n),
        other => return Err(SqlError::not_supported(format!("INSERT target: {other}"))),
    };
    let short = table.rsplit('.').next().unwrap_or(&table).to_string();
    let (schema, entry) = scan::resolve_table(db, sess, &short)?;
    if schema.pk.is_empty() {
        return Err(SqlError::not_supported(format!("table \"{short}\" has no primary key")));
    }
    let source = insert.source.ok_or_else(|| SqlError::syntax("INSERT requires source"))?;
    let snapshot = sess.implicit_snapshot(db)?;
    let mut txn = sess.txn.take().unwrap_or_else(|| Txn::new(snapshot));
    let mut count = 0u64;
    match *source.body {
        sqlparser::ast::SetExpr::Values(values) => {
            let value_rows = values.rows;
            // 列投影（INSERT INTO t (a, b) VALUES ...）
            let col_idx: Vec<usize> = if insert.columns.is_empty() {
                (0..schema.columns.len()).collect()
            } else {
                insert
                    .columns
                    .iter()
                    .map(|i| {
                        let name = i.0.first().and_then(|p| p.as_ident()).map(|x| x.value.clone())
                            .ok_or_else(|| SqlError::undefined_column(i.to_string()))?;
                        schema.col_index(&name).ok_or_else(|| SqlError::undefined_column(name))
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            for vr in value_rows {
                if vr.len() != col_idx.len() {
                    return Err(SqlError::syntax(format!("insert arity {}/{}", vr.len(), col_idx.len())));
                }
                let mut row = vec![SqlValue::Null; schema.columns.len()];
                for (v, &ci) in vr.iter().zip(&col_idx) {
                    row[ci] = expr::eval(v, &[], &|_| None)?;
                }
                insert_row(db, sess, &schema, entry.id, &mut txn, row)?;
                count += 1;
            }
        }
        sqlparser::ast::SetExpr::Query(q) => {
            // INSERT INTO t SELECT ...
            let view = scan::eval_query(db, sess, q.as_ref(), snapshot)?;
            let col_idx: Vec<usize> = if insert.columns.is_empty() {
                (0..schema.columns.len()).collect()
            } else {
                insert
                    .columns
                    .iter()
                    .map(|i| {
                        let name = i.0.first().and_then(|p| p.as_ident()).map(|x| x.value.clone())
                            .ok_or_else(|| SqlError::undefined_column(i.to_string()))?;
                        schema.col_index(&name).ok_or_else(|| SqlError::undefined_column(name))
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            for src_row in &view.rows {
                let mut row = vec![SqlValue::Null; schema.columns.len()];
                for (si, &ci) in col_idx.iter().enumerate() {
                    row[ci] = src_row.get(si).cloned().unwrap_or(SqlValue::Null);
                }
                insert_row(db, sess, &schema, entry.id, &mut txn, row)?;
                count += 1;
            }
        }
        other => return Err(SqlError::not_supported(format!("INSERT source: {}", other))),
    }
    if !txn.explicit {
        commit_tx(db, &sess.branch, &txn)?;
    } else {
        sess.txn = Some(txn);
    }
    Ok(Some(Output::Command { tag: format!("INSERT 0 {count}"), affected: count }))
}

fn insert_row(
    db: &Database,
    sess: &Session,
    schema: &TableSchema,
    table_id: u32,
    txn: &mut Txn,
    row: Vec<SqlValue>,
) -> Result<()> {
    let pk_vals: Vec<SqlValue> = schema.pk.iter().map(|&i| row[i as usize].clone()).collect();
    if pk_vals.iter().any(|v| v.is_null()) {
        return Err(SqlError::new("23502", "null value in primary key column"));
    }
    let key = encode_key(&pk_vals);
    // 主键冲突检查（快照内已存在 + 事务写集）
    let b = db.branch(&sess.branch)?;
    let tm = b.mem.table(table_id);
    let exists_mem = tm.get(&key, txn.snapshot).is_some();
    let exists_tree = match &entry_root(db, sess, table_id)? {
        Some(r) => crate::prolly::cursor::lookup(&db.store, r, &key)?.is_some(),
        None => false,
    };
    if exists_mem || exists_tree {
        return Err(SqlError::duplicate_key(format!("duplicate key value violates primary key constraint")));
    }
    let val = encode_row(&row);
    txn.put(table_id, key, val);
    Ok(())
}

fn entry_root(db: &Database, sess: &Session, table_id: u32) -> Result<Option<Hash>> {
    let b = db.branch(&sess.branch)?;
    let head = b.head.load_full();
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    let entries = catalog.catalog_entries(head.as_ref().as_ref().map(|c| c.root).as_ref())?;
    for (_, e) in entries {
        if e.id == table_id {
            return Ok(e.table_root.as_ref().and_then(|s| Hash::from_base32(s)));
        }
    }
    Ok(None)
}

pub(crate) fn exec_delete(db: &Database, sess: &mut Session, delete: sqlparser::ast::Delete) -> Result<Option<Output>> {
    let twjs: &Vec<sqlparser::ast::TableWithJoins> = match &delete.from {
        sqlparser::ast::FromTable::WithFromKeyword(v) => v,
        sqlparser::ast::FromTable::WithoutKeyword(v) => v,
    };
    let name = twjs
        .first()
        .map(|t| match &t.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => object_name(name),
            other => other.to_string(),
        })
        .ok_or_else(|| SqlError::syntax("DELETE requires table"))?;
    let short = name.rsplit('.').next().unwrap_or(&name).to_string();
    let (schema, entry) = scan::resolve_table(db, sess, &short)?;
    let snapshot = sess.implicit_snapshot(db)?;
    // 找目标行：全扫 + WHERE（v1；pk 等值优化同 SELECT）
    let tv = table_scan_pub(db, sess, &short, snapshot)?;
    let cols: std::collections::HashMap<String, usize> = std::collections::HashMap::from_iter(
        tv.names.iter().enumerate().map(|(i, n)| (n.to_ascii_lowercase(), i)),
    );
    let colfn = |n: &str| cols.get(&n.to_ascii_lowercase()).copied();
    let mut txn = sess.txn.take().unwrap_or_else(|| Txn::new(snapshot));
    let mut count = 0u64;
    for row in &tv.rows {
        if let Some(w) = &delete.selection {
            if expr::eval(w, row, &colfn)? != SqlValue::Bool(true) {
                continue;
            }
        }
        let pk_vals: Vec<SqlValue> = schema.pk.iter().map(|&i| row[i as usize].clone()).collect();
        let key = encode_key(&pk_vals);
        txn.delete(entry.id, key);
        count += 1;
    }
    if !txn.explicit {
        commit_tx(db, &sess.branch, &txn)?;
    } else {
        sess.txn = Some(txn);
    }
    Ok(Some(Output::Command { tag: format!("DELETE {count}"), affected: count }))
}

/// 暴露给 ddl 的表扫描（复用 scan 内部实现）
pub(crate) use scan::table_scan_by_name as table_scan_pub;

pub(crate) fn exec_update(_db: &Database, _sess: &mut Session) -> Result<Option<Output>> {
    Err(SqlError::internal("update handled in exec_statement"))
}

/// UPDATE 的实际实现（exec_statement 里拆解 AST 后调用）
pub(crate) fn update_impl(
    db: &Database,
    sess: &mut Session,
    table: &str,
    assignments: Vec<(Ident, Expr)>,
    selection: Option<Expr>,
) -> Result<Option<Output>> {
    let short = table.rsplit('.').next().unwrap_or(table).to_string();
    let (schema, entry) = scan::resolve_table(db, sess, &short)?;
    let snapshot = sess.implicit_snapshot(db)?;
    let tv = scan::table_scan_by_name(db, sess, &short, snapshot)?;
    let cols: std::collections::HashMap<String, usize> = std::collections::HashMap::from_iter(
        tv.names.iter().enumerate().map(|(i, n)| (n.to_ascii_lowercase(), i)),
    );
    let colfn = |n: &str| cols.get(&n.to_ascii_lowercase()).copied();
    let mut txn = sess.txn.take().unwrap_or_else(|| Txn::new(snapshot));
    let mut count = 0u64;
    for row in &tv.rows {
        if let Some(w) = &selection {
            if expr::eval(w, row, &colfn)? != SqlValue::Bool(true) {
                continue;
            }
        }
        let mut new_row = row.clone();
        for (id, e) in &assignments {
            let ci = schema
                .col_index(&id.value)
                .ok_or_else(|| SqlError::undefined_column(id.value.clone()))?;
            new_row[ci] = expr::eval(e, row, &colfn)?;
        }
        let old_pk: Vec<SqlValue> = schema.pk.iter().map(|&i| row[i as usize].clone()).collect();
        let new_pk: Vec<SqlValue> = schema.pk.iter().map(|&i| new_row[i as usize].clone()).collect();
        if encode_key(&old_pk) != encode_key(&new_pk) {
            return Err(SqlError::not_supported("UPDATE of primary key columns"));
        }
        txn.put(entry.id, encode_key(&old_pk), encode_row(&new_row));
        count += 1;
    }
    if !txn.explicit {
        commit_tx(db, &sess.branch, &txn)?;
    } else {
        sess.txn = Some(txn);
    }
    Ok(Some(Output::Command { tag: format!("UPDATE {count}"), affected: count }))
}

/// 列元数据辅助（prepare describe 用）
pub(crate) fn colmeta(names: &[String], ty: ColType) -> Vec<ColumnMeta> {
    names.iter().map(|n| ColumnMeta { name: n.clone(), ty }).collect()
}

#[allow(dead_code)]
fn unused(_: &Database, _: &mut Session, _: Option<&Expr>) {}
