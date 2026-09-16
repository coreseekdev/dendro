#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! 物理表扫描统一入口：table_scan_opt/table_scan、catalog 解析、AP 列存适配、行↔batch。

use super::*;


use super::agg::{self, AggCall};
use super::expr;
use crate::engine::{Database, Session};
use crate::error::{Result, SqlError};
use crate::format::row::decode_row;
use crate::types::{ColType, ColumnMeta, Output, RecordSet, SqlValue};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, GroupByExpr, JoinOperator, ObjectName, OrderByExpr, Query,
    Select, SelectItem, SetExpr, TableFactor, Value as PV,
};
use std::collections::HashMap;
use std::sync::Arc;

/// 按表名扫描（ddl 的 DELETE/UPDATE 复用）
pub(crate) fn table_scan_by_name(
    db: &Database,
    sess: &mut Session,
    name: &str,
    snapshot: u64,
) -> Result<TableView> {
    let tf = TableFactor::Table {
        name: ObjectName::from(vec![sqlparser::ast::Ident::new(name)]),
        alias: None,
        args: None,
        with_hints: vec![],
        version: None,
        partitions: vec![],
        index_hints: vec![],
        json_path: None,
        sample: None,
        with_ordinality: false,
    };
    table_scan(db, sess, &tf, snapshot, None, None)
}

/// 带谓词的单表扫描：pk 等值/IN 下推走直查（TP 点查路径）
pub(crate) fn table_scan_opt(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    snapshot: u64,
    selection: Option<&Expr>,
    pushdown_limit: Option<usize>,
    col_mask: Option<&[bool]>,
) -> Result<TableView> {
    // 视图展开（Q-1 扩展）：FROM 引用视图名 → 执行存储的 SQL 并返回结果。
    // **深度上限 8**（第二十一轮 R21-13）：自引用视图 → 递归展开 → 栈溢出
    // SIGABRT 进程崩溃；深度上限将无限递归转为有界错误。
    // **基表优先**（R21-14）：如果表存在（非视图），跳过视图展开——
    // 防止视图遮蔽同名基表使基表永久不可达。
    // 深度计数使用 thread-local（R21-13 修复：无函数签名变更）。
    thread_local! {
        static VIEW_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    let cur_depth = VIEW_DEPTH.with(|d| d.get());
    if cur_depth >= 8 {
        return Err(SqlError::not_supported(
            "view expansion exceeds max depth (8); circular view definition?",
        ));
    }
    if let TableFactor::Table { name, version, .. } = tf {
        // P1-10：视图没有自己的提交链（视图体是查询文本，不是物化树）。
        // 带版本子句的视图引用显式拒绝——此前 version 被忽略，历史查询
        // 会对视图按**当前时间**求值（静默错误）。
        if version.is_some() {
            let vname = name
                .0
                .iter()
                .filter_map(|p| p.as_ident())
                .map(|i| i.value.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(".");
            if db.manifest().manifest.views.contains_key(&vname) {
                return Err(SqlError::not_supported(format!(
                    "time travel on view \"{vname}\" (views have no commit chain); AS OF the underlying base tables instead"
                )));
            }
        }
        let vname = name
            .0
            .iter()
            .filter_map(|p| p.as_ident())
            .map(|i| i.value.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(".");
        if let Some(query_text) = db.manifest().manifest.views.get(&vname) {
            let query_text = query_text.clone();
            let stmts = crate::sql::parse_batch(&query_text, sess.dialect)?;
            if stmts.len() == 1 {
                if let sqlparser::ast::Statement::Query(sub_query) =
                    stmts.into_iter().next().unwrap()
                {
                    VIEW_DEPTH.with(|d| d.set(cur_depth + 1));
                    let result = eval_query(db, sess, sub_query.as_ref(), snapshot);
                    VIEW_DEPTH.with(|d| d.set(cur_depth));
                    return result;
                }
            }
        }
    }
    // v2c-1：派发器决策（ir-spec 05——原快路径 if-else 链的纯函数化）。
    // 执行器保持内部守卫（决策与执行间的竞态由执行器兜底回落）。
    match crate::sql::dispatch::dispatch_scan(db, sess, tf, selection, snapshot)? {
        crate::sql::dispatch::ScanAlt::CurrentPoint => {
            if let Some((schema, entry)) = try_pk_pushdown(db, sess, tf, selection, snapshot)? {
                let sel = selection.expect("pushdown implies selection");
                return build_point_view(db, sess, &schema, &entry, sel, snapshot);
            }
            table_scan(db, sess, tf, snapshot, pushdown_limit, selection)
        }
        crate::sql::dispatch::ScanAlt::MainPlusDelta => {
            if let Some(tv) =
                try_ap_scan(db, sess, tf, selection, snapshot, pushdown_limit, col_mask)?
            {
                return Ok(tv);
            }
            table_scan(db, sess, tf, snapshot, pushdown_limit, selection)
        }
        // RowFallback 即现行 table_scan（prolly+overlay 行路径）；
        // HistoryScan 由 table_scan 内部按 version 子句路由（time_travel_scan）
        crate::sql::dispatch::ScanAlt::HistoryScan | crate::sql::dispatch::ScanAlt::RowFallback => {
            table_scan(db, sess, tf, snapshot, pushdown_limit, selection)
        }
    }
}

/// AP 路径：表有列存投影且行数达标时走 CBF 扫描
/// AP 路径资格判定（v2c-1 与 dispatch 共用的单一事实源）：
/// 带版本子句 / 非表 / 无列存 / 解析失败 / 段空或行数 < 1 万 → 不适用。
/// `bypass_threshold`：force_source 调试旁路行数阈值（结构性条件
/// （段存在）仍强制——无段无法执行）。
pub(crate) fn ap_resolve(
    db: &Database,
    sess: &Session,
    tf: &TableFactor,
    bypass_threshold: bool,
) -> Option<(crate::versioned::TableSchema, crate::versioned::TableEntry)> {
    let name = match tf {
        TableFactor::Table { name, version, .. } => {
            // P1-10：AP 列存只有当前物化段——带版本子句的查询走 time travel
            // 路径（列存历史快照 v2；此前 version 被忽略 → 历史查询读当前段）
            if version.is_some() {
                return None;
            }
            name.0
                .iter()
                .filter_map(|p| p.as_ident())
                .map(|i| i.value.clone())
                .collect::<Vec<_>>()
                .join(".")
        }
        _ => return None,
    };
    db.columnar()?;
    let short_name = name.rsplit('.').next().unwrap_or(&name).to_string();
    // 显式事务：以 BEGIN 冻结的 catalog 根解析（Q-14——此前 AP 路径用当前
    // head，事务内树的可见性与行路径不一致）
    let frozen = match &sess.txn {
        Some(t) if t.explicit => t.head_root,
        _ => None,
    };
    let resolved = match frozen {
        Some(r) => resolve_table_at(db, Some(&r), &short_name),
        None => resolve_table(db, &sess.branch, &short_name),
    };
    let (schema, entry) = resolved.ok()?;
    if entry.col_segments.is_empty() {
        return None;
    }
    if !bypass_threshold && entry.col_rows < 10_000 {
        return None;
    }
    Some((schema, entry))
}

pub(crate) fn try_ap_scan(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    selection: Option<&Expr>,
    snapshot: u64,
    pushdown_limit: Option<usize>,
    col_mask: Option<&[bool]>,
) -> Result<Option<TableView>> {
    let Some((schema, entry)) = ap_resolve(db, sess, tf, false) else {
        return Ok(None);
    };
    let Some(ap) = db.columnar() else {
        return Ok(None);
    };
    // pk 范围提取（order 域，开区间语义收集）
    let mut pk_range: Option<(Option<u64>, Option<u64>)> = None;
    if schema.pk.len() == 1 {
        let pk_name = schema.columns[schema.pk[0] as usize].name.clone();
        if let Some(sel) = selection {
            pk_range = extract_pk_range(sel, &pk_name);
        }
    }
    // 归并源（v2c-4，ir-spec 04 §4 MainPlusDelta）：段（旧→新，新者覆盖）
    // + memtx overlay + 显式事务写，三路按 pk 惰性归并（exec::source 流式
    // 化——替换 v2c-2 的全量 BTreeMap 物化；语义不变量逐条搬运）：
    // - 同 key 优先级 txn > overlay > 新段 > 旧段；
    // - col_deletes 只抑制**段源**行（overlay/txn 的重插不受影响）；
    // - 输出恒 pk 有序（AP 与行路径行序收敛）；
    // - pushdown_limit 早停（源侧产出计数）。
    let deletes: std::collections::HashSet<Vec<u8>> = entry
        .col_deletes
        .iter()
        .filter_map(|h| {
            // col_deletes 存"行键的 hex"（与 encode_key 输出同一编码）
            (0..h.len() / 2)
                .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16))
                .collect::<std::result::Result<Vec<u8>, _>>()
                .ok()
        })
        .collect();
    // 段批（ap.scan 内含段级 pk 剪枝——账本 #24 的 None=无界语义）
    let mut segment_batches = Vec::with_capacity(entry.col_segments.len());
    for seg in entry.col_segments.iter() {
        segment_batches.push(ap.scan(
            db.obj_store(),
            &schema,
            std::slice::from_ref(seg),
            &pk_range,
            col_mask,
        )?);
    }
    // memtx overlay（覆盖段源；None=墓碑删除）
    let b = db.branch(&sess.branch)?;
    let overlay = b.mem.table(entry.id).snapshot_rows(snapshot);
    // 会话显式事务自身写（最后覆盖；Q-14：AP 路径读事务的写）
    let mut txn_writes: Vec<(Vec<u8>, Option<std::sync::Arc<Vec<u8>>>)> = Vec::new();
    if let Some(t) = &sess.txn {
        if t.explicit {
            for ((tid, k), m) in &t.writes {
                if *tid != entry.id {
                    continue;
                }
                match m {
                    crate::prolly::Mutation::Put(v) => {
                        txn_writes.push((k.clone(), Some(std::sync::Arc::new(v.clone()))));
                    }
                    crate::prolly::Mutation::Delete => {
                        txn_writes.push((k.clone(), None));
                    }
                }
            }
        }
    }
    let src = crate::exec::source::MainPlusDeltaSource::new(
        segment_batches,
        overlay,
        txn_writes,
        deletes,
        schema.clone(),
        pushdown_limit,
    )?;
    // v1 消费形态：整流收集进 TableView（后续 Source 直推管线——方向 B）；
    // 批拉取语义已就位（ROW_BATCH 粒度），物化发生在消费侧而非源侧
    let mut rows: Vec<Vec<SqlValue>> = Vec::new();
    for item in src {
        rows.extend(item?);
    }
    let names = schema.columns.iter().map(|c| c.name.clone()).collect();
    Ok(Some(TableView { names, rows }))
}

/// Arrow 批 → 行（SqlValue），列型按 schema 收敛
pub(crate) fn rows_from_batches(
    batch: &arrow::record_batch::RecordBatch,
    schema: &crate::versioned::TableSchema,
) -> Result<Vec<Vec<SqlValue>>> {
    use arrow::array::{
        Array, BinaryArray, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array,
        StringArray, TimestampMillisecondArray,
    };
    let mut rows = Vec::with_capacity(batch.num_rows());
    for r in 0..batch.num_rows() {
        let mut row = Vec::with_capacity(batch.num_columns());
        for ci in 0..batch.num_columns() {
            let col = batch.column(ci);
            if col.is_null(r) {
                row.push(SqlValue::Null);
                continue;
            }
            let v = match col.data_type() {
                arrow::datatypes::DataType::Boolean => col
                    .as_any()
                    .downcast_ref::<BooleanArray>()
                    .map(|a| SqlValue::Bool(a.value(r))),
                arrow::datatypes::DataType::Int32 => col
                    .as_any()
                    .downcast_ref::<Int32Array>()
                    .map(|a| SqlValue::Int32(a.value(r))),
                arrow::datatypes::DataType::Int64 => col
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .map(|a| SqlValue::Int64(a.value(r))),
                arrow::datatypes::DataType::Float64 => col
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .map(|a| SqlValue::Float64(a.value(r))),
                arrow::datatypes::DataType::Utf8 => col
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .map(|a| SqlValue::Utf8(a.value(r).to_string())),
                arrow::datatypes::DataType::Binary => col
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .map(|a| SqlValue::Bytes(a.value(r).to_vec())),
                arrow::datatypes::DataType::Date32 => col
                    .as_any()
                    .downcast_ref::<Date32Array>()
                    .map(|a| SqlValue::Date32(a.value(r))),
                arrow::datatypes::DataType::Timestamp(
                    arrow::datatypes::TimeUnit::Millisecond,
                    _,
                ) => col
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .map(|a| SqlValue::TimestampMs(a.value(r))),
                _ => None,
            };
            row.push(v.unwrap_or(SqlValue::Null));
        }
        // schema 演化：补 NULL
        row.resize(schema.columns.len(), SqlValue::Null);
        rows.push(row);
    }
    Ok(rows)
}

/// 单表扫描（含系统视图）；pk 等值条件下推走直查
pub(crate) fn table_scan(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    snapshot: u64,
    pushdown_limit: Option<usize>,
    selection: Option<&Expr>,
) -> Result<TableView> {
    match tf {
        TableFactor::Table { name, version, .. } => {
            let full = name
                .0
                .iter()
                .map(|p| {
                    p.as_ident()
                        .map(|i| i.value.clone())
                        .unwrap_or_else(|| p.to_string())
                })
                .collect::<Vec<_>>()
                .join(".");
            let low = full.to_ascii_lowercase();
            if version.is_some() {
                // 伪表不支持 time travel（无历史提交链）
                if matches!(
                    low.as_str(),
                    "cambium.branches"
                        | "branches"
                        | "information_schema.tables"
                        | "pg_catalog.pg_tables"
                        | "pg_tables"
                        | "information_schema.columns"
                        | "pg_catalog.pg_columns"
                        | "pg_columns"
                        | "pg_catalog.pg_settings"
                        | "pg_settings"
                        | "cambium.commit_log"
                        | "commit_log"
                ) {
                    return Err(SqlError::not_supported(format!(
                        "time travel on pseudo relation \"{low}\""
                    )));
                }
                return time_travel_scan(
                    db,
                    &sess.branch,
                    &full,
                    version.as_ref().unwrap(),
                    pushdown_limit,
                );
            }
            match low.as_str() {
                "cambium.branches" | "branches" => return pseudo_branches(db),
                "information_schema.tables" | "pg_catalog.pg_tables" | "pg_tables" => {
                    return pseudo_tables(db)
                }
                "information_schema.columns" | "pg_catalog.pg_columns" | "pg_columns" => {
                    return pseudo_columns(db, sess)
                }
                "pg_catalog.pg_settings" | "pg_settings" => {
                    return Ok(TableView {
                        names: vec!["name".into(), "setting".into()],
                        rows: vec![],
                    })
                }
                "cambium.commit_log" | "commit_log" => {
                    // commit_log('branch') 不带参数时用当前分支
                    return pseudo_commit_log(db, sess);
                }
                _ => {}
            }
            // 显式事务：读以 BEGIN 冻结的 catalog 根为准（R7-3）
            let frozen = match &sess.txn {
                Some(t) if t.explicit => t.head_root,
                _ => None,
            };
            let (schema, entry) = match frozen {
                Some(r) => resolve_table_at(
                    db,
                    Some(&r),
                    full.rsplit(['.', '@']).next().unwrap_or(&full),
                )?,
                None => resolve_table(db, &sess.branch, &full)?,
            };
            let b = db.branch(&sess.branch)?;
            let _head = b.head.load_full();
            let root = entry
                .table_root
                .as_ref()
                .and_then(|s| crate::format::hash::Hash::from_base32(s));
            // PK 范围下推（P2-6g）：数字型单列主键 + WHERE 中的 >/>=/</<= 合取
            // → 树走 range_scan、overlay 走区间物化（此前选择性范围查询与
            // 全表扫描同价：30 万行树 + 10 万 overlay 全量物化 ≈ 180ms）。
            // 余下非范围谓词由下游常规过滤承担——区间只是超集收窄，语义不变。
            let pk_range = if schema.pk.len() == 1 {
                let pk_col = &schema.columns[schema.pk[0] as usize];
                if matches!(
                    pk_col.ty,
                    ColType::Int64 | ColType::Int32 | ColType::Date32 | ColType::TimestampMs
                ) {
                    selection.and_then(|sel| extract_pk_int_range(sel, &pk_col.name))
                } else {
                    None
                }
            } else {
                None
            };
            let (range_keys, overlay) = match &pk_range {
                Some((lo, hi)) => {
                    let tm = b.mem.table(entry.id);
                    let (start_key, end_key) = pk_range_keys(lo, hi);
                    // 空区间判定必须**先于**任何 range 调用——BTreeMap 对
                    // start > end 直接 panic（WHERE id > 5 AND id < 5 实证，
                    // 审计 R6-1 P0）
                    if let (Some(a), Some(b2)) = (&start_key, &end_key) {
                        if a >= b2 {
                            return Ok(TableView {
                                names: schema.columns.iter().map(|c| c.name.clone()).collect(),
                                rows: vec![],
                            });
                        }
                    }
                    let overlay = tm.snapshot_rows_in_range(
                        start_key.as_deref(),
                        end_key.as_deref(),
                        snapshot,
                    );
                    let rk = match &root {
                        Some(r) => crate::prolly::cursor::range_scan(
                            db.store.clone(),
                            r,
                            start_key.as_deref(),
                            end_key.as_deref(),
                        )?,
                        None => vec![],
                    };
                    (Some(rk), overlay)
                }
                None => (None, b.mem.table(entry.id).snapshot_rows(snapshot)),
            };
            if std::env::var("DENDRO_SCAN_DEBUG").is_ok() {
                eprintln!(
                    "[scan] table={full} root={root:?} overlay={overlay:?} schema={:?}",
                    schema
                        .columns
                        .iter()
                        .map(|c| (c.name.clone(), c.ty))
                        .collect::<Vec<_>>()
                );
            }
            // 可见性归并（**单一抽象**，第七轮评审建议）：树 → checkpointed
            // overlay → 会话显式事务自身写，三层按序覆盖。
            // ⚠ 此处曾是键序双指针归并（`<=` vs `<` 之差产生过 R7-1 P0：
            // checkpoint 后 UPDATE 双行/DELETE 复活）——收敛为 map 覆盖语义
            // 后，键序错误在结构上无处可写。
            let mut visible: std::collections::BTreeMap<Vec<u8>, Arc<Vec<u8>>> =
                std::collections::BTreeMap::new();
            // ① 树（checkpoint 物化态）；范围下推时只取 [start, end)
            match &range_keys {
                Some(rk) => {
                    for (k, v) in rk {
                        visible.insert(k.clone(), Arc::new(v.clone()));
                    }
                }
                None => {
                    if let Some(r) = &root {
                        let mut it = crate::prolly::cursor::TreeIter::new(db.store.clone(), r)?;
                        let mut n = 0usize;
                        while let Some((k, v)) = it.next_item()? {
                            n += 1;
                            if n.is_multiple_of(4096) {
                                sess.deadline_check()?;
                            }
                            visible.insert(k, Arc::new(v));
                        }
                    }
                }
            }
            // ② memtx overlay（checkpoint 之后的已提交变更）；None = 墓碑
            for (k, ov) in overlay {
                match ov {
                    Some(v) => {
                        visible.insert(k, v);
                    }
                    None => {
                        visible.remove(&k);
                    }
                }
            }
            // ③ 会话显式事务自身写（R8-1：读自己的写——此前 SQL 读路径从不
            // 合并 sess.txn.writes，BEGIN;INSERT 后 SELECT 看不到、
            // BEGIN;DELETE 后 UPDATE 空转且 COMMIT 后幽灵行）
            if let Some(t) = &sess.txn {
                if t.explicit {
                    for ((tid, k), m) in &t.writes {
                        if *tid != entry.id {
                            continue;
                        }
                        match m {
                            crate::prolly::Mutation::Put(v) => {
                                visible.insert(k.clone(), Arc::new(v.clone()));
                            }
                            crate::prolly::Mutation::Delete => {
                                visible.remove(k);
                            }
                        }
                    }
                }
            }
            // Q-1 LIMIT 下推：无 ORDER BY 时解码在 cap 行后停止
            // （BTreeMap 键序确定，前 cap 行即 LIMIT/OFFSET 语义的正确前缀）
            let mut rows = Vec::with_capacity(
                pushdown_limit
                    .unwrap_or(visible.len())
                    .min(visible.len())
                    .max(64),
            );
            let mut n = 0usize;
            for (i, v) in visible.values().enumerate() {
                if let Some(cap) = pushdown_limit {
                    if i >= cap {
                        break;
                    }
                }
                n += 1;
                if n.is_multiple_of(4096) {
                    sess.deadline_check()?;
                }
                rows.push(row_from_bytes(&schema, v)?);
            }
            let names = schema.columns.iter().map(|c| c.name.clone()).collect();
            Ok(TableView { names, rows })
        }
        TableFactor::Derived { subquery, alias, .. } => {
            let mut view = eval_query(db, sess, subquery.as_ref(), snapshot)?;
            // `(VALUES ...) AS r(n, ...)`：alias 列名覆盖子查询输出名
            // （VALUES 求值产出 column1/column2——不覆盖则外层 WHERE/投影
            // 按真实列名解析失败；递归 CTE 注入依赖此路径）
            if let Some(a) = alias {
                if !a.columns.is_empty() && a.columns.len() == view.names.len() {
                    view.names = a
                        .columns
                        .iter()
                        .map(|c| c.name.value.clone())
                        .collect();
                }
            }
            Ok(view)
        }
        other => Err(SqlError::not_supported(format!(
            "FROM: {}",
            short_str(other)
        ))),
    }
}

/// 解析表（catalog 查找；可能带 schema 前缀 public.t / t）
pub fn resolve_table(
    db: &Database,
    branch_name: &str,
    name: &str,
) -> Result<(crate::versioned::TableSchema, crate::versioned::TableEntry)> {
    let short_name = name.rsplit(['.', '@']).next().unwrap_or(name);
    let branch = db.branch(branch_name)?;
    let head = branch.head.load_full();
    resolve_table_at(
        db,
        head.as_ref().as_ref().map(|c| c.root).as_ref(),
        short_name,
    )
}

/// 以**给定 catalog 根**解析（显式事务冻结读，第七轮 R7-3：事务内树的
/// 可见性以 BEGIN 时的根为准，不随 checkpoint 推进翻转）
pub(crate) fn resolve_table_at(
    db: &Database,
    root: Option<&crate::format::hash::Hash>,
    short_name: &str,
) -> Result<(crate::versioned::TableSchema, crate::versioned::TableEntry)> {
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    let found = catalog
        .catalog_lookup(root, short_name)?
        .map(
            |e| -> Result<(crate::versioned::TableSchema, crate::versioned::TableEntry)> {
                let schema = catalog.load_schema(&e.schema_addr)?;
                Ok((schema, e))
            },
        )
        .transpose()?;
    found.ok_or_else(|| {
        SqlError::undefined_table(format!("relation \"{short_name}\" does not exist"))
    })
}

pub(crate) fn row_from_bytes(schema: &crate::versioned::TableSchema, bytes: &[u8]) -> Result<Vec<SqlValue>> {
    let mut vals = decode_row(bytes)?;
    // schema 演化：补 NULL / 截断
    vals.resize(schema.columns.len(), SqlValue::Null);
    Ok(vals)
}

// ---------- JOIN ----------

/// 行 → Arrow 批（8192 行/批）
pub fn rows_to_batches(
    names: &[String],
    rows: &[Vec<SqlValue>],
) -> Result<Vec<arrow::record_batch::RecordBatch>> {
    let columns: Vec<ColumnMeta> = names
        .iter()
        .enumerate()
        .map(|(i, n)| ColumnMeta {
            name: n.clone(),
            ty: rows
                .first()
                .map(|r| infer_type(&r[i]))
                .unwrap_or(ColType::Utf8),
        })
        .collect();
    Ok(rows_to_batches_typed(&columns, rows))
}

pub fn rows_to_batches_typed(
    columns: &[ColumnMeta],
    rows: &[Vec<SqlValue>],
) -> Vec<arrow::record_batch::RecordBatch> {
    // 列型：描述口径优先；否则按该列首个非空值推断（首行可能是 NULL，导致整列值丢失）
    let mut col_types: Vec<ColType> = Vec::with_capacity(columns.len());
    for (ci, col) in columns.iter().enumerate() {
        let first_non_null = rows
            .iter()
            .find_map(|r| r.get(ci).filter(|v| !v.is_null()).cloned());
        col_types.push(match first_non_null {
            Some(v) => infer_type(&v),
            None => col.ty,
        });
    }
    use arrow::array::{
        ArrayRef, BinaryArray, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array,
        StringArray, TimestampMillisecondArray,
    };
    use arrow::datatypes::{Field, Schema};
    use std::sync::Arc;
    let _schema = Arc::new(Schema::new(
        columns
            .iter()
            .map(|c| Field::new(c.name.clone(), c.ty.arrow(), true))
            .collect::<Vec<_>>(),
    ));
    let mut batches = Vec::new();
    for chunk in rows.chunks(8192) {
        let cols: Vec<ArrayRef> = columns
            .iter()
            .enumerate()
            .map(|(ci, _c)| -> ArrayRef {
                let mut vs: Vec<Option<SqlValue>> = Vec::with_capacity(chunk.len());
                for r in chunk {
                    // 按稳定列型收敛（Int32→Int64 宽化；类型不符置 NULL 不崩）
                    vs.push(
                        r.get(ci)
                            .cloned()
                            .map(|v| crate::types::coerce_to(v, col_types[ci])),
                    );
                }
                match col_types[ci] {
                    ColType::Bool => Arc::new(BooleanArray::from(
                        vs.iter()
                            .map(|v| {
                                v.as_ref().and_then(|x| {
                                    if let SqlValue::Bool(b) = x {
                                        Some(*b)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>(),
                    )),
                    ColType::Int32 => Arc::new(Int32Array::from(
                        vs.iter()
                            .map(|v| {
                                v.as_ref().and_then(|x| {
                                    if let SqlValue::Int32(i) = x {
                                        Some(*i)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>(),
                    )),
                    ColType::Int64 => Arc::new(Int64Array::from(
                        vs.iter()
                            .map(|v| {
                                v.as_ref().and_then(|x| {
                                    if let SqlValue::Int64(i) = x {
                                        Some(*i)
                                    } else if let SqlValue::Int32(i) = x {
                                        Some(*i as i64)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>(),
                    )),
                    ColType::Float64 => Arc::new(Float64Array::from(
                        vs.iter()
                            .map(|v| {
                                v.as_ref().and_then(|x| {
                                    if let SqlValue::Float64(f) = x {
                                        Some(*f)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>(),
                    )),
                    ColType::Utf8 => Arc::new(StringArray::from(
                        vs.iter()
                            .map(|v| {
                                v.as_ref().and_then(|x| {
                                    if let SqlValue::Utf8(s) = x {
                                        Some(s.as_str())
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>(),
                    )),
                    ColType::Bytes => Arc::new(BinaryArray::from(
                        vs.iter()
                            .map(|v| {
                                v.as_ref().and_then(|x| {
                                    if let SqlValue::Bytes(b) = x {
                                        Some(b.as_slice())
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>(),
                    )),
                    ColType::Date32 => Arc::new(Date32Array::from(
                        vs.iter()
                            .map(|v| {
                                v.as_ref().and_then(|x| {
                                    if let SqlValue::Date32(d) = x {
                                        Some(*d)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>(),
                    )),
                    ColType::TimestampMs => Arc::new(TimestampMillisecondArray::from(
                        vs.iter()
                            .map(|v| {
                                v.as_ref().and_then(|x| {
                                    if let SqlValue::TimestampMs(t) = x {
                                        Some(*t)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>(),
                    )),
                }
            })
            .collect();
        // 以稳定列型重建 schema
        let real_schema = Arc::new(Schema::new(
            columns
                .iter()
                .enumerate()
                .map(|(i, c)| Field::new(c.name.clone(), col_types[i].arrow(), true))
                .collect::<Vec<_>>(),
        ));
        let batch = arrow::record_batch::RecordBatch::try_new(real_schema, cols)
            .expect("record batch build");
        batches.push(batch);
    }
    batches
}
