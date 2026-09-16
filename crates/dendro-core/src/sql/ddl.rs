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
use sqlparser::ast::{FunctionArg, FunctionArgExpr};
use crate::versioned::{ColumnDef, TableEntry, TableSchema};
use sqlparser::ast::{ColumnDef as PColumnDef, Expr, Ident, Insert, ObjectName, TableConstraint};
use std::collections::HashSet;

pub(crate) fn exec_create_table(
    db: &Database,
    sess: &mut Session,
    create: sqlparser::ast::CreateTable,
) -> Result<Option<Output>> {
    let name = object_name(&create.name);
    let short = name.rsplit('.').next().unwrap_or(&name).to_string();
    let (cols, pk, fk_defs, unique_sets, check_exprs) = translate_columns(&create.columns, &create.constraints)?;
    let schema = TableSchema {
        name: short.clone(),
        columns: cols,
        pk,
    };
    // 已存在检查
    {
        let (ver, _) = scan_catalog(db, sess)?;
        let _ = ver;
    }
    if let Ok((_, entry)) = scan::resolve_table(db, &sess.branch, &short) {
        let _ = entry;
        if create.if_not_exists {
            return Ok(Some(Output::Command {
                tag: "CREATE TABLE".into(),
                affected: 0,
            }));
        }
        return Err(SqlError::duplicate_table(format!(
            "relation \"{short}\" already exists"
        )));
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
        owner: crate::sql::privs::norm_user(&sess.user),
        acl: std::collections::HashMap::new(),
        foreign_keys: resolve_fk_refs(db, sess, fk_defs)?,
        unique_sets,
        check_exprs,
    };
    // schema chunk 先写
    db.cas
        .put_batch(&[schema.to_chunk()], &mut session_chunks)
        .map_err(SqlError::from)?;
    let new_catalog = catalog.apply_catalog(
        old_catalog.as_ref(),
        vec![(short.clone(), Some(entry))],
        &mut session_chunks,
    )?;
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
    db.cas
        .put_batch(&[cchunk], &mut session_chunks)
        .map_err(SqlError::from)?;
    b.head.store(std::sync::Arc::new(Some(commit.clone())));
    let seq = b.alloc_seq();
    let ck = crate::wal::CheckpointRecord {
        catalog_root: commit.root,
        commit_addr: commit.addr(),
        seq_covered: b.watermark.load(std::sync::atomic::Ordering::Acquire),
    };
    b.wal.append(
        crate::wal::FrameType::Checkpoint,
        seq,
        &crate::wal::encode_checkpoint(&ck),
        db.opts.durability,
    )?;
    let seg_now = b.wal.current_seg().saturating_sub(1);
    let covered = ck.seq_covered;
    let bname = sess.branch.clone();
    db.update_manifest(|m| {
        m.schema_version += 1; // DDL 白名单（B3）：catalog 变更
        let h = m
            .refs
            .get_mut(&bname)
            .ok_or_else(|| SqlError::internal("branch vanished"))?;
        h.commit = Some(commit.addr().to_base32());
        h.wal_seg = seg_now.max(h.wal_seg);
        h.covered_seq = covered;
        h.epoch = b.lease_epoch.load(std::sync::atomic::Ordering::Acquire);
        Ok(true)
    })?;
    Ok(Some(Output::Command {
        tag: "CREATE TABLE".into(),
        affected: 0,
    }))
}

/// FK 的 ref_columns 延迟解析（建表时才可查父表 schema）。
/// v1 限定：REFERENCES 必须指向父表 PK 列——列索引 = 父表 pk 序
fn resolve_fk_refs(
    db: &Database,
    sess: &Session,
    mut fks: Vec<crate::versioned::ForeignKeyDef>,
) -> Result<Vec<crate::versioned::ForeignKeyDef>> {
    for fk in fks.iter_mut() {
        let (parent_schema, _entry) =
            scan::resolve_table(db, &sess.branch, &fk.ref_table).map_err(|_| {
                SqlError::undefined_table(format!(
                    "FOREIGN KEY references non-existent table {}",
                    fk.ref_table
                ))
            })?;
        // v1：ref_columns 空 = 按父表 PK 序填充
        if fk.ref_columns.is_empty() {
            if parent_schema.pk.len() != fk.columns.len() {
                return Err(SqlError::not_supported(
                    "FOREIGN KEY must reference parent PRIMARY KEY (v1)",
                ));
            }
            fk.ref_columns = parent_schema.pk.clone();
        }
    }
    Ok(fks)
}

fn scan_catalog(db: &Database, sess: &Session) -> Result<(u64, ())> {
    let b = db.branch(&sess.branch)?;
    let head = b.head.load_full();
    let _ = head;
    Ok((0, ()))
}

fn object_name(n: &ObjectName) -> String {
    n.0.iter()
        .map(|p| {
            p.as_ident()
                .map(|i| i.value.clone())
                .unwrap_or_else(|| p.to_string())
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn translate_columns(
    cols: &[PColumnDef],
    constraints: &[TableConstraint],
) -> Result<(Vec<ColumnDef>, Vec<u16>, Vec<crate::versioned::ForeignKeyDef>, Vec<Vec<u16>>, Vec<String>)> {
    let mut out = Vec::new();
    let mut pk: Vec<u16> = Vec::new();
    let mut fks: Vec<crate::versioned::ForeignKeyDef> = Vec::new();
    let mut unique_sets: Vec<Vec<u16>> = Vec::new();
    let mut check_exprs: Vec<String> = Vec::new();
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
                    unique_sets.push(vec![(out.len()) as u16]);
                }
                sqlparser::ast::ColumnOption::Default(_) => { /* 接受但 v1 忽略 */ }
                sqlparser::ast::ColumnOption::Check(ref ck) => {
                    // P0：列级 CHECK（原静默丢弃）
                    check_exprs.push(ck.expr.to_string());
                }
                sqlparser::ast::ColumnOption::ForeignKey(ref fk) => {
                    // 列级 REFERENCES t(c) → 单列 FK（P0——原静默丢弃）
                    let ref_table = fk
                        .foreign_table
                        .0
                        .last()
                        .and_then(|p| p.as_ident())
                        .map(|i| i.value.to_ascii_lowercase())
                        .unwrap_or_default();
                    let ref_col = fk
                        .referred_columns
                        .first()
                        .map(|i| i.value.clone())
                        .unwrap_or_default();
                    if ref_table.is_empty() || ref_col.is_empty() {
                        return Err(SqlError::not_supported(
                            "REFERENCES without table(column)",
                        ));
                    }
                    fks.push(crate::versioned::ForeignKeyDef {
                        columns: vec![(out.len()) as u16], // 当前列（push 前索引）
                        ref_table,
                        ref_columns: vec![], // 延迟到建表后解析（需父表 schema）
                        on_delete_cascade: matches!(
                            fk.on_delete,
                            Some(sqlparser::ast::ReferentialAction::Cascade)
                        ),
                    });
                    // 记 ref_col 名——建表后解析为索引
                    let _ = ref_col;
                }
                _ => {}
            }
        }
        out.push(ColumnDef {
            name: c.name.value.clone(),
            ty,
            nullable,
        });
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
        } else if let TableConstraint::Unique(u) = con {
            // P0：UNIQUE 约束——多列 UNIQUE(a,b) 是一个组合集（原误拆
            // 为单列集：(10,30) 与 (10,20) 的 a=10 冲突——差分实证）
            let mut cols_idx = Vec::new();
            for ic in &u.columns {
                let name = match &ic.column.expr {
                    Expr::Identifier(id) => id.value.clone(),
                    other => other.to_string(),
                };
                let low = name.to_ascii_lowercase();
                let idx = out
                    .iter()
                    .position(|c| c.name.to_ascii_lowercase() == low)
                    .ok_or_else(|| SqlError::undefined_column(format!("unique column {name}")))?;
                cols_idx.push(idx as u16);
            }
            unique_sets.push(cols_idx);
        } else if let TableConstraint::Check(check) = con {
            // P0：CHECK 约束（原静默丢弃）→ 文本形态存储 + INSERT 求值
            check_exprs.push(check.expr.to_string());
        } else if let TableConstraint::ForeignKey(fk) = con {
            // 表级 FOREIGN KEY (col) REFERENCES t(col)（P0——原静默丢弃）
            let ref_table = fk
                .foreign_table
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(|i| i.value.to_ascii_lowercase())
                .unwrap_or_default();
            if ref_table.is_empty() || fk.referred_columns.len() != fk.columns.len() {
                return Err(SqlError::not_supported(
                    "FOREIGN KEY column count mismatch",
                ));
            }
            let mut cols_idx = Vec::new();
            for c in &fk.columns {
                let low = c.value.to_ascii_lowercase();
                let idx = out
                    .iter()
                    .position(|cd| cd.name.to_ascii_lowercase() == low)
                    .ok_or_else(|| SqlError::undefined_column(format!("FK column {low}")))?;
                cols_idx.push(idx as u16);
            }
            fks.push(crate::versioned::ForeignKeyDef {
                columns: cols_idx,
                ref_table,
                ref_columns: vec![], // 延迟到建表后解析
                on_delete_cascade: matches!(
                    fk.on_delete,
                    Some(sqlparser::ast::ReferentialAction::Cascade)
                ),
            });
            // 存 referred 列名→索引延迟——v1 简化：建表后用父表 PK 序
        }
    }
    Ok((out, pk, fks, unique_sets, check_exprs))
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
    catalog_commit_locked(db, sess, changes, message, &_g)
}

/// 同上，调用方已持 commit_mu（GRANT/REVOKE 的读改写原子段——
/// resolve → mutate → commit 必须同锁，否则并发授权互相覆盖）
pub(crate) fn catalog_commit_locked(
    db: &Database,
    sess: &Session,
    changes: Vec<(String, Option<TableEntry>)>,
    message: &str,
    _g: &parking_lot::MutexGuard<'_, ()>,
) -> Result<()> {
    let b = db.branch(&sess.branch)?;
    let mut session_chunks: HashSet<Hash> = HashSet::new();
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    let head = b.head.load_full();
    let old_catalog = head.as_ref().as_ref().map(|c| c.root);
    // schema chunk 写盘
    let mut schema_chunks: Vec<crate::objstore::cas::Chunk> = Vec::new();
    for (_, e) in &changes {
        if let Some(e) = e {
            let h = Hash::from_base32(&e.schema_addr)
                .ok_or_else(|| SqlError::internal("bad schema addr"))?;
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
    db.cas
        .put_batch(&[cchunk], &mut session_chunks)
        .map_err(SqlError::from)?;
    b.head.store(std::sync::Arc::new(Some(commit.clone())));
    let seq = b.alloc_seq();
    let ck = crate::wal::CheckpointRecord {
        catalog_root: commit.root,
        commit_addr: commit.addr(),
        seq_covered: b.watermark.load(std::sync::atomic::Ordering::Acquire),
    };
    b.wal.append(
        crate::wal::FrameType::Checkpoint,
        seq,
        &crate::wal::encode_checkpoint(&ck),
        db.opts.durability,
    )?;
    let seg_now = b.wal.current_seg().saturating_sub(1);
    let covered = ck.seq_covered;
    let bname = sess.branch.clone();
    db.update_manifest(|m| {
        m.schema_version += 1; // DDL 白名单（B3）：catalog 变更
        let h = m
            .refs
            .get_mut(&bname)
            .ok_or_else(|| SqlError::internal("branch vanished"))?;
        h.commit = Some(commit.addr().to_base32());
        h.wal_seg = seg_now.max(h.wal_seg);
        h.covered_seq = covered;
        h.epoch = b.lease_epoch.load(std::sync::atomic::Ordering::Acquire);
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
        match scan::resolve_table(db, &sess.branch, &short) {
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
        return Ok(Some(Output::Command {
            tag: "DROP TABLE".into(),
            affected: 0,
        }));
    }
    catalog_commit(db, sess, changes, "DROP TABLE")?;
    Ok(Some(Output::Command {
        tag: "DROP TABLE".into(),
        affected: 0,
    }))
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
            let (mut cols, new_pk, _, _, _) = translate_columns(&[column_def], &[])?;
            if !new_pk.is_empty() {
                return Err(SqlError::not_supported(
                    "ALTER TABLE ADD COLUMN ... PRIMARY KEY（改用建表约束或重建表）",
                ));
            }
            let (_, entry) = scan::resolve_table(db, &sess.branch, &short)?;
            let catalog = crate::versioned::Versioned::new(db.store.clone());
            let mut schema = catalog.load_schema(&entry.schema_addr)?;
            schema.columns.append(&mut cols);
            // 原主键保持不变：AddColumn 只追加列。曾在此被新列的空 pk
            // 整体覆盖，表随即永久失去主键（后续 INSERT 全部报
            // "has no primary key"）。
            let _ = new_pk;
            db.cas
                .put_batch(&[schema.to_chunk()], &mut HashSet::new())
                .map_err(SqlError::from)?;
            let ne = TableEntry {
                schema_addr: schema.addr_of().to_base32(),
                ..entry
            };
            catalog_commit(
                db,
                sess,
                vec![(short.clone(), Some(ne))],
                "ALTER TABLE ADD COLUMN",
            )?;
            Ok(Some(Output::Command {
                tag: "ALTER TABLE".into(),
                affected: 0,
            }))
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
        let (_, entry) = scan::resolve_table(db, &sess.branch, &short)?;
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
    Ok(Some(Output::Command {
        tag: "TRUNCATE TABLE".into(),
        affected: 0,
    }))
}

fn schema_of(db: &Database, _sess: &Session, entry: &TableEntry) -> Result<TableSchema> {
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    catalog.load_schema(&entry.schema_addr)
}

fn entry_pk(schema: &TableSchema, row: &[SqlValue]) -> Vec<SqlValue> {
    schema.pk.iter().map(|&i| row[i as usize].clone()).collect()
}

/// INSERT INTO t [(cols)] VALUES (...), (...) | DEFAULT VALUES
pub(crate) fn exec_insert(
    db: &Database,
    sess: &mut Session,
    insert: Insert,
) -> Result<Option<Output>> {
    let table = match &insert.table {
        sqlparser::ast::TableObject::TableName(n) => object_name(n),
        other => return Err(SqlError::not_supported(format!("INSERT target: {other}"))),
    };
    let short = table.rsplit('.').next().unwrap_or(&table).to_string();
    let (schema, entry) = scan::resolve_table(db, &sess.branch, &short)?;
    if schema.pk.is_empty() {
        return Err(SqlError::not_supported(format!(
            "table \"{short}\" has no primary key"
        )));
    }
    // P0：UPSERT（ON CONFLICT DO NOTHING / DO UPDATE）——冲突时跳过/更新
    // PG 的 ON CONFLICT 冲突面 = PK 或指定 UNIQUE 约束；v1 限定 PK
    //（insert_row 的 duplicate_key 在 on_conflict 模式下改走此分支）
    let upsert: Option<&sqlparser::ast::OnConflict> = match &insert.on {
        Some(sqlparser::ast::OnInsert::OnConflict(oc)) => Some(oc),
        Some(other) => {
            return Err(SqlError::not_supported(format!(
                "INSERT ON: {other:?}"
            )))
        }
        None => None,
    };
    let source = insert
        .source
        .ok_or_else(|| SqlError::syntax("INSERT requires source"))?;
    let snapshot = sess.implicit_snapshot(db)?;
    let mut txn = sess.txn.take().unwrap_or_else(|| Txn::new(snapshot));
    let mut count = 0u64;
    let mut seen_keys: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

    /// 冲突时行为：None=报错；DO NOTHING=跳过；DO UPDATE=按 SET 更新
    /// 返回 (是否处理, count 增量)
    #[allow(clippy::too_many_arguments)] // UPSERT 上下文——内聚不拆
    fn handle_conflict(
        db: &Database,
        sess: &Session,
        schema: &crate::versioned::TableSchema,
        entry: &crate::versioned::TableEntry,
        txn: &mut Txn,
        upsert: Option<&sqlparser::ast::OnConflict>,
        pk_key: &[u8],
        row: &mut Vec<SqlValue>,
        pk_vals: &[SqlValue],
    ) -> Result<(bool, u64)> {
        let Some(oc) = upsert else {
            return Ok((false, 0)); // 无 UPSERT——调用方走正常报错路径
        };
        // 检查冲突（三段：txn 写集 / memtx / 树）
        let b = db.branch(&sess.branch)?;
        let tm = b.mem.table(entry.id);
        // memtx（含墓碑感知）
        let memtx_hit = tm.get(pk_key, txn.snapshot).is_some();
        // 树
        let tree_hit = entry
            .table_root
            .as_ref()
            .and_then(|r| crate::format::hash::Hash::from_base32(r))
            .is_some_and(|root| {
                crate::prolly::cursor::lookup(&db.store, &root, pk_key)
                    .map(|v| v.is_some())
                    .unwrap_or(false)
            });
        // txn 写集（同事务先插后冲）
        let txn_hit = txn.writes.contains_key(&(entry.id, pk_key.to_vec()));
        if !memtx_hit && !tree_hit && !txn_hit {
            return Ok((false, 0)); // 无冲突——正常 INSERT 路径
        }
        // 冲突！按 ON CONFLICT action 处理
        match &oc.action {
            sqlparser::ast::OnConflictAction::DoNothing => {
                Ok((true, 0)) // 跳过（不报错不计数）
            }
            sqlparser::ast::OnConflictAction::DoUpdate(du) => {
                // DO UPDATE SET col = expr（excluded.col = INSERT 尝试的新值）
                // 读取现有行 → 应用 SET → 写回
                let existing = read_existing_row(
                    db, sess, schema, entry, txn, pk_key, pk_vals,
                )?;
                let Some(existing_row) = existing else {
                    return Ok((false, 0)); // 墓碑——无现有行，走 INSERT
                };
                let mut updated = existing_row.clone();
                let _ = &row;
                for a in &du.assignments {
                    let target = match &a.target {
                        sqlparser::ast::AssignmentTarget::ColumnName(cn) => {
                            cn.to_string().to_ascii_lowercase()
                        }
                        other => {
                            return Err(SqlError::not_supported(format!(
                                "ON CONFLICT SET target: {other:?}"
                            )))
                        }
                    };
                    let ci = schema.col_index(&target).ok_or_else(|| {
                        SqlError::undefined_column(format!(
                            "ON CONFLICT SET column {target}"
                        ))
                    })?;
                    // SET 值求值：excluded(col) → INSERT 尝试的新值；
                    // 其他表达式对 existing 行求值
                    let val = match &a.value {
                        Expr::Function(f)
                            if {
                                let n = f.name.to_string().to_ascii_lowercase();
                                n == "excluded" || n == "values"
                            } =>
                        {
                            if let Some(
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)),
                            ) = crate::sql::scan::fn_args(f).first()
                            {
                                let arg_name = match arg {
                                    Expr::Identifier(id) => id.value.clone(),
                                    Expr::CompoundIdentifier(ps) => {
                                        ps.last().map(|p| p.value.clone()).unwrap_or_default()
                                    }
                                    _ => String::new(),
                                };
                                let aci = schema.col_index(&arg_name).ok_or_else(|| {
                                    SqlError::undefined_column(format!(
                                        "excluded.{arg_name}"
                                    ))
                                })?;
                                row[aci].clone()
                            } else {
                                return Err(SqlError::not_supported(
                                    "excluded() requires column argument",
                                ));
                            }
                        }
                        e => {
                            let colfn = |name: &str| schema.col_index(name);
                            expr::eval(e, &existing_row, &colfn)?
                        }
                    };
                    updated[ci] =
                        coerce_for_column(val, &schema.columns[ci].ty)?;
                }
                // 写回（PUT 覆盖）
                let new_key = encode_key(pk_vals);
                txn.writes.insert(
                    (entry.id, new_key),
                    crate::prolly::Mutation::Put(
                        crate::format::row::encode_row(&updated),
                    ),
                );
                Ok((true, 1))
            }
        }
    }

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
                        let name =
                            i.0.first()
                                .and_then(|p| p.as_ident())
                                .map(|x| x.value.clone())
                                .ok_or_else(|| SqlError::undefined_column(i.to_string()))?;
                        schema
                            .col_index(&name)
                            .ok_or_else(|| SqlError::undefined_column(name))
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            for vr in value_rows {
                if vr.len() != col_idx.len() {
                    return Err(SqlError::syntax(format!(
                        "insert arity {}/{}",
                        vr.len(),
                        col_idx.len()
                    )));
                }
                let mut row = vec![SqlValue::Null; schema.columns.len()];
                for (v, &ci) in vr.iter().zip(&col_idx) {
                    let lit = expr::eval(v, &[], &|_| None)?;
                    row[ci] = coerce_for_column(lit, &schema.columns[ci].ty)?;
                }
                let pk_vals: Vec<SqlValue> =
                    schema.pk.iter().map(|&i| row[i as usize].clone()).collect();
                let key = encode_key(&pk_vals);
                if !seen_keys.insert(key.clone()) {
                    return Err(SqlError::duplicate_key(
                        "duplicate key value violates primary key constraint (key in same INSERT)"
                            .to_string(),
                    ));
                }
                // P0 UPSERT：冲突时 DO NOTHING / DO UPDATE（非报错路径）
                let (handled, delta) = handle_conflict(
                    db, sess, &schema, &entry, &mut txn, upsert, &key, &mut row, &pk_vals,
                )?;
                if handled {
                    count += delta;
                    continue;
                }
                insert_row(db, sess, &schema, entry.id, &mut txn, row, &entry.foreign_keys, &entry.unique_sets, &entry.check_exprs)?;
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
                        let name =
                            i.0.first()
                                .and_then(|p| p.as_ident())
                                .map(|x| x.value.clone())
                                .ok_or_else(|| SqlError::undefined_column(i.to_string()))?;
                        schema
                            .col_index(&name)
                            .ok_or_else(|| SqlError::undefined_column(name))
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            for src_row in &view.rows {
                let mut row = vec![SqlValue::Null; schema.columns.len()];
                for (si, &ci) in col_idx.iter().enumerate() {
                    row[ci] = src_row.get(si).cloned().unwrap_or(SqlValue::Null);
                }
                insert_row(db, sess, &schema, entry.id, &mut txn, row, &entry.foreign_keys, &entry.unique_sets, &entry.check_exprs)?;
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
    Ok(Some(Output::Command {
        tag: format!("INSERT 0 {count}"),
        affected: count,
    }))
}

/// 账本 #26（b）：INSERT 字面量按**目标列型**采纳（PG unknown-literal
/// 语义）。Utf8 字面量 → 数值/日期/时间戳列按输入语法解析（非法 →
/// 22P02）；数值/布尔 → TEXT 列文本化；同族直通；Null 直通。
pub(crate) fn coerce_for_column(v: SqlValue, ty: &crate::types::ColType) -> Result<SqlValue> {
    use crate::types::ColType;
    let bad = |want: &str, shown: String| {
        SqlError::new(
            "22P02",
            format!("invalid input syntax for type {want}: {shown}"),
        )
    };
    Ok(match (v, ty) {
        (SqlValue::Null, _) => SqlValue::Null,
        (SqlValue::Utf8(s), ColType::Int32) => {
            SqlValue::Int32(s.parse().map_err(|_| bad("integer", s))?)
        }
        (SqlValue::Utf8(s), ColType::Int64) => {
            SqlValue::Int64(s.parse().map_err(|_| bad("bigint", s))?)
        }
        (SqlValue::Utf8(s), ColType::Float64) => {
            SqlValue::Float64(s.parse().map_err(|_| bad("double", s))?)
        }
        (SqlValue::Utf8(s), ColType::Bool) => {
            let low = s.to_ascii_lowercase();
            match low.as_str() {
                "true" | "t" | "1" => SqlValue::Bool(true),
                "false" | "f" | "0" => SqlValue::Bool(false),
                _ => return Err(bad("boolean", s)),
            }
        }
        (SqlValue::Utf8(s), ColType::Date32) => SqlValue::Date32(
            crate::sql::expr::parse_date(&s).ok_or_else(|| bad("date", s.clone()))?,
        ),
        (SqlValue::Utf8(s), ColType::TimestampMs) => SqlValue::TimestampMs(
            crate::sql::expr::parse_ts(&s).ok_or_else(|| bad("timestamp", s.clone()))?,
        ),
        // 数值/布尔 → TEXT：文本化（PG：INSERT 1 INTO text → '1'）
        (SqlValue::Int32(i), ColType::Utf8) => SqlValue::Utf8(i.to_string()),
        (SqlValue::Int64(i), ColType::Utf8) => SqlValue::Utf8(i.to_string()),
        (SqlValue::Float64(f), ColType::Utf8) => SqlValue::Utf8(crate::types::format_f64(f)),
        (SqlValue::Bool(b), ColType::Utf8) => SqlValue::Utf8(b.to_string()),
        (SqlValue::Date32(d), ColType::Utf8) => SqlValue::Utf8(crate::types::format_date(d)),
        (SqlValue::TimestampMs(t), ColType::Utf8) => SqlValue::Utf8(crate::types::format_ts_ms(t)),
        // 同族宽/窄化（PG 数值字面量按列采纳：小整数 Number 定型 Int32，
        // 入 BIGINT 列须宽化——此前 embed 的 i64 启发式掩盖了存储宽度）
        (SqlValue::Int32(i), ColType::Int64) => SqlValue::Int64(i as i64),
        (SqlValue::Int64(i), ColType::Int32) => i32::try_from(i)
            .map(SqlValue::Int32)
            .map_err(|_| SqlError::new("22003", format!("integer out of range: {i}")))?,
        (SqlValue::Int32(i), ColType::Float64) => SqlValue::Float64(i as f64),
        (SqlValue::Int64(i), ColType::Float64) => SqlValue::Float64(i as f64),
        (SqlValue::Float64(f), ColType::Int32) if f.fract() == 0.0 => SqlValue::Int32(f as i32),
        (SqlValue::Float64(f), ColType::Int64) if f.fract() == 0.0 => SqlValue::Int64(f as i64),
        // 其余（浮点带小数入整列 / 布尔入数值等）：直通由读取侧类型不匹配
        // 暴露（v1 不做全矩阵错误面）
        (v, _) => v,
    })
}

#[allow(clippy::too_many_arguments)] // 约束执法上下文——FkUnique 结构化不合算
fn insert_row(
    db: &Database,
    sess: &Session,
    schema: &TableSchema,
    table_id: u32,
    txn: &mut Txn,
    row: Vec<SqlValue>,
    fks: &[crate::versioned::ForeignKeyDef],
    unique_sets: &[Vec<u16>],
    check_exprs: &[String],
) -> Result<()> {
    let pk_vals: Vec<SqlValue> = schema.pk.iter().map(|&i| row[i as usize].clone()).collect();
    if pk_vals.iter().any(|v| v.is_null()) {
        return Err(SqlError::new("23502", "null value in primary key column"));
    }
    // P0：CHECK 约束执法（表达式对行求值——false → 23514）
    if !check_exprs.is_empty() {
        let colfn = |name: &str| schema.col_index(name);
        for ce in check_exprs {
            // CHECK 存裸表达式文本——包装为 SELECT 重 parse 求值；
            // parse 失败 = 建表时已接受的约束此刻不可解析，约束静默
            // 失效不可接受（架构审视 #1），必须报错拒绝写入
            let ss = crate::sql::parse_batch(&format!("SELECT {}", ce), crate::sql::SqlDialect::Pg)
                .map_err(|e| SqlError::new("23514", format!("check constraint unparsable: {ce}: {e}")))?;
            let mut ss = ss;
            match ss.pop() {
                Some(sqlparser::ast::Statement::Query(q)) => {
                    if let sqlparser::ast::SetExpr::Select(sel) = *q.body {
                        if let Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) = sel.projection.first().cloned() {
                            let v = expr::eval(&e, &row, &colfn)?;
                            if matches!(v, SqlValue::Bool(false)) {
                                return Err(SqlError::new(
                                    "23514",
                                    format!("new row violates check constraint: {ce}"),
                                ));
                            }
                        }
                    }
                }
                _ => {
                    return Err(SqlError::new(
                        "23514",
                        format!("check constraint unparsable: {ce}"),
                    ))
                }
            }
        }
    }
    // P0：NOT NULL 执法（nullable 字段此前全库无读点——静默入库）
    for (i, cd) in schema.columns.iter().enumerate() {
        if !cd.nullable && row.get(i).is_some_and(|v| v.is_null()) {
            return Err(SqlError::new(
                "23502",
                format!("null value in column \"{}\" violates not-null constraint", cd.name),
            ));
        }
    }
    // P0：FK 父行检查（FK 列非 NULL → 父表 PK 点查存在性）
    if !fks.is_empty() {
        check_fk_parents(db, sess, schema, fks, &row, txn)?;
    }
    // P0：UNIQUE 约束检查（v1 全扫树——值组合不得与现有行重复；
    // NULL 不参与唯一性——SQL 语义；v2 建 unique 索引点查）
    for us in unique_sets {
        let uvals: Vec<SqlValue> = us
            .iter()
            .map(|&i| row.get(i as usize).cloned().unwrap_or(SqlValue::Null))
            .collect();
        if uvals.iter().any(|v| v.is_null()) {
            continue;
        }
        // P0 UNIQUE：同表扫描（memtx overlay + prolly 树——两侧都查；
        // v1 全扫，v2 建 unique 索引点查）
        let b = db.branch(&sess.branch)?;
        // 1. memtx overlay（insert 后未 checkpoint 的数据在此）
        let tm = b.mem.table(table_id);
        for (_, v) in tm.snapshot_rows(txn.snapshot) {
            if let Some(v) = v {
                if let Ok(existing) = crate::sql::scan::row_from_bytes(schema, &v) {
                    let dup = us
                        .iter()
                        .zip(&uvals)
                        .all(|(&ci, uv)| existing.get(ci as usize).is_some_and(|x| x == uv));
                    if dup {
                        let col_names: Vec<String> = us
                            .iter()
                            .filter_map(|&ci| {
                                schema.columns.get(ci as usize).map(|c| c.name.clone())
                            })
                            .collect();
                        return Err(SqlError::duplicate_key(format!(
                            "duplicate key value violates unique constraint on \"{}\"",
                            col_names.join(", ")
                        )));
                    }
                }
            }
        }
        // 2. prolly 树（checkpoint 后数据）
        let (_, this_entry) = scan::resolve_table(db, &sess.branch, &schema.name)?;
        let root = this_entry
            .table_root
            .as_ref()
            .and_then(|r| crate::format::hash::Hash::from_base32(r));
        if let Some(root) = root {
            for (_, v) in crate::prolly::cursor::range_scan(
                db.store.clone(),
                &root,
                None,
                None,
            )
            .map_err(|e| SqlError::internal(e.to_string()))?
            {
                if let Ok(existing) = crate::sql::scan::row_from_bytes(schema, &v) {
                    let dup = us
                        .iter()
                        .zip(&uvals)
                        .all(|(&ci, uv)| existing.get(ci as usize).is_some_and(|x| x == uv));
                    if dup {
                        let col_names: Vec<String> = us
                            .iter()
                            .filter_map(|&ci| {
                                schema.columns.get(ci as usize).map(|c| c.name.clone())
                            })
                            .collect();
                        return Err(SqlError::duplicate_key(format!(
                            "duplicate key value violates unique constraint on \"{}\"",
                            col_names.join(", ")
                        )));
                    }
                }
            }
        }
    }
    let key = encode_key(&pk_vals);
    // Q-16：同一显式事务内两次 INSERT 同键，第二次必须 23505
    //（此前写集不可见 → 静默覆盖）
    // 按 mutation 类型判别（第二十一轮 R21-2）：Delete + INSERT = 合法替换
    if let Some(crate::prolly::Mutation::Put(_)) = txn.writes.get(&(table_id, key.clone())) {
        return Err(SqlError::duplicate_key(
            "duplicate key value violates primary key constraint (key already inserted in this transaction)".to_string(),
        ));
    }
    // 主键冲突检查（快照内已存在 + 事务写集）。
    // **墓碑感知**（第七轮 R7-1 伴生）：DELETE 是 overlay 墓碑（树旧行在下次
    // checkpoint 前仍物理存在）——可见墓碑（latest_ts <= 快照）下重插同键
    // 必须允许，否则删除后重插报 23505（PG 语义为允许）。
    let b = db.branch(&sess.branch)?;
    let tm = b.mem.table(table_id);
    let tombstoned = tm.latest_ts(&key).is_some_and(|ts| ts <= txn.snapshot);
    // 事务自身 Delete → memtx/tree 旧行视为已删（不参与 dup 检查）
    let txn_deleted = matches!(
        txn.writes.get(&(table_id, key.clone())),
        Some(crate::prolly::Mutation::Delete)
    );
    let exists_mem = if txn_deleted {
        false
    } else {
        tm.get(&key, txn.snapshot).is_some()
    };
    let exists_tree = if tombstoned || txn_deleted {
        false
    } else {
        match &entry_root(db, sess, table_id)? {
            Some(r) => crate::prolly::cursor::lookup(&db.store, r, &key)?.is_some(),
            None => false,
        }
    };
    if exists_mem || exists_tree {
        return Err(SqlError::duplicate_key(
            "duplicate key value violates primary key constraint".to_string(),
        ));
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

pub(crate) fn exec_delete(
    db: &Database,
    sess: &mut Session,
    delete: sqlparser::ast::Delete,
) -> Result<Option<Output>> {
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
    let (schema, entry) = scan::resolve_table(db, &sess.branch, &short)?;
    let snapshot = sess.implicit_snapshot(db)?;
    // 找目标行：全扫 + WHERE（v1；pk 等值优化同 SELECT）
    let tv = table_scan_pub(db, sess, &short, snapshot)?;
    let cols: std::collections::HashMap<String, usize> = std::collections::HashMap::from_iter(
        tv.names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.to_ascii_lowercase(), i)),
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
    Ok(Some(Output::Command {
        tag: format!("DELETE {count}"),
        affected: count,
    }))
}

/// 暴露给 ddl 的表扫描（复用 scan 内部实现）
pub(crate) use scan::table_scan_by_name as table_scan_pub;

/// UPDATE 的实际实现（exec_statement 里拆解 AST 后调用）
pub(crate) fn update_impl(
    db: &Database,
    sess: &mut Session,
    table: &str,
    assignments: Vec<(Ident, Expr)>,
    selection: Option<Expr>,
) -> Result<Option<Output>> {
    let short = table.rsplit('.').next().unwrap_or(table).to_string();
    let (schema, entry) = scan::resolve_table(db, &sess.branch, &short)?;
    let snapshot = sess.implicit_snapshot(db)?;
    let tv = scan::table_scan_by_name(db, sess, &short, snapshot)?;
    let cols: std::collections::HashMap<String, usize> = std::collections::HashMap::from_iter(
        tv.names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.to_ascii_lowercase(), i)),
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
        let new_pk: Vec<SqlValue> = schema
            .pk
            .iter()
            .map(|&i| new_row[i as usize].clone())
            .collect();
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
    Ok(Some(Output::Command {
        tag: format!("UPDATE {count}"),
        affected: count,
    }))
}

/// 列元数据辅助（prepare describe 用）
#[allow(dead_code)]
pub(crate) fn colmeta(names: &[String], ty: ColType) -> Vec<ColumnMeta> {
    names
        .iter()
        .map(|n| ColumnMeta {
            name: n.clone(),
            ty,
        })
        .collect()
}

#[allow(dead_code)]
fn unused(_: &Database, _: &mut Session, _: Option<&Expr>) {}

// ---------------------------------------------------------------------------
// P0：外键执法（INSERT/UPDATE 检查父行存在；DELETE RESTRICT 检查子引用）
// v1 限定：REFERENCES 指向父表 PK（点查效率）；ON DELETE CASCADE 支持
// ---------------------------------------------------------------------------

/// INSERT/UPDATE 时的 FK 父行检查：行中 FK 列值非 NULL 时按父表 PK 点查
pub(crate) fn check_fk_parents(
    db: &Database,
    sess: &Session,
    schema: &TableSchema,
    fks: &[crate::versioned::ForeignKeyDef],
    row: &[SqlValue],
    txn: &Txn,
) -> Result<()> {
    for fk in fks {
        // FK 列值（任一 NULL → 跳过——SQL 外键语义）
        let fk_vals: Vec<SqlValue> = fk
            .columns
            .iter()
            .map(|&i| row.get(i as usize).cloned().unwrap_or(SqlValue::Null))
            .collect();
        if fk_vals.iter().any(|v| v.is_null()) {
            continue;
        }
        let key = encode_key(&fk_vals);
        // 查父表存在性（事务写集 + memtx + 树——三段合成）
        let (_, parent_entry) = scan::resolve_table(db, &sess.branch, &fk.ref_table)?;
        let parent_id = parent_entry.id;
        // 1. 事务写集
        if let Some(crate::prolly::Mutation::Put(_)) = txn.writes.get(&(parent_id, key.clone())) {
            continue; // 本事务已插入父行
        }
        if txn.writes.contains_key(&(parent_id, key.clone())) {
            // 本事务删过父行（Delete mutation）→ 23503
            return Err(SqlError::new(
                "23503",
                format!(
                    "insert or update on table \"{}\" violates foreign key constraint",
                    schema.name
                ),
            ));
        }
        // 2. memtx overlay
        let b = db.branch(&sess.branch)?;
        let tm = b.mem.table(parent_id);
        if tm.get(&key, txn.snapshot).is_some() {
            continue; // memtx 可见
        }
        // 3. prolly 树
        let root = parent_entry.table_root.as_ref().and_then(|r| {
            crate::format::hash::Hash::from_base32(r)
        });
        if let Some(root) = root {
            let found = crate::prolly::cursor::lookup(
                &db.store,
                &root,
                &key,
            )
            .map_err(|e| SqlError::internal(e.to_string()))?;
            if found.is_some() {
                continue; // 树中存在
            }
        }
        // 三段都未命中 → 父行不存在
        return Err(SqlError::new(
            "23503",
            format!(
                "insert or update on table \"{}\" violates foreign key constraint \"{} → {}\"",
                schema.name,
                fk.columns
                    .iter()
                    .map(|&i| schema.columns.get(i as usize).map(|c| c.name.clone()).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(", "),
                fk.ref_table
            ),
        ));
    }
    Ok(())
}


/// 读取现有行（UPSERT DO UPDATE 用）：txn 写集 → memtx → 树 三段
fn read_existing_row(
    db: &Database,
    sess: &Session,
    schema: &crate::versioned::TableSchema,
    entry: &crate::versioned::TableEntry,
    txn: &Txn,
    key: &[u8],
    pk_vals: &[SqlValue],
) -> Result<Option<Vec<SqlValue>>> {
    let _ = pk_vals;
    // 1. txn 写集（本事务先插的行）
    if let Some(m) = txn.writes.get(&(entry.id, key.to_vec())) {
        match m {
            crate::prolly::Mutation::Put(bytes) => {
                return Ok(Some(crate::sql::scan::row_from_bytes(schema, bytes)?));
            }
            crate::prolly::Mutation::Delete => return Ok(None),
        }
    }
    // 2. memtx
    let b = db.branch(&sess.branch)?;
    let tm = b.mem.table(entry.id);
    if let Some(v) = tm.get(key, txn.snapshot) {
        return Ok(Some(crate::sql::scan::row_from_bytes(schema, &v)?));
    }
    // 3. 树
    if let Some(root_str) = &entry.table_root {
        if let Some(root) = crate::format::hash::Hash::from_base32(root_str) {
            if let Ok(Some(v)) = crate::prolly::cursor::lookup(&db.store, &root, key) {
                return Ok(Some(crate::sql::scan::row_from_bytes(schema, &v)?));
            }
        }
    }
    Ok(None)
}
