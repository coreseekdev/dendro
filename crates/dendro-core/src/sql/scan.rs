#![allow(clippy::type_complexity)]
//! 查询执行：FROM 解析、快照扫描（prolly 树 ∪ memtx overlay）、过滤/投影/聚合/排序。

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
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

/// 扫描出来的表视图
pub struct TableView {
    pub names: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
}

pub(crate) fn exec_query(
    db: &Database,
    sess: &mut Session,
    q: Query,
    snapshot: u64,
) -> Result<Output> {
    let view = eval_query(db, sess, &q, snapshot)?;
    let colmeta: Vec<ColumnMeta> = view
        .names
        .iter()
        .zip(
            view.rows
                .first()
                .map(|r| r.as_slice())
                .unwrap_or(&[])
                .iter()
                .map(infer_type)
                .collect::<Vec<_>>()
                .into_iter()
                .chain(std::iter::repeat(ColType::Utf8)),
        )
        .take(view.names.len())
        .map(|(n, t)| ColumnMeta {
            name: n.clone(),
            ty: t,
        })
        .collect();
    // 列型按数据推断（空表回退 Utf8）
    let colmeta = fix_colmeta(view.rows.first(), &view.names, colmeta);
    Ok(Output::Rows(RecordSet {
        columns: colmeta,
        batches: rows_to_batches(&view.names, &view.rows)?,
    }))
}

fn fix_colmeta(
    first_row: Option<&Vec<SqlValue>>,
    names: &[String],
    meta: Vec<ColumnMeta>,
) -> Vec<ColumnMeta> {
    if let Some(r) = first_row {
        meta.iter()
            .zip(r.iter())
            .enumerate()
            .map(|(i, (m, v))| ColumnMeta {
                name: names[i].clone(),
                ty: if v.is_null() { m.ty } else { infer_type(v) },
            })
            .collect()
    } else {
        meta
    }
}

fn infer_type(v: &SqlValue) -> ColType {
    match v {
        SqlValue::Null => ColType::Utf8,
        SqlValue::Bool(_) => ColType::Bool,
        SqlValue::Int32(_) => ColType::Int32,
        SqlValue::Int64(_) => ColType::Int64,
        SqlValue::Float64(_) => ColType::Float64,
        SqlValue::Utf8(_) => ColType::Utf8,
        SqlValue::Bytes(_) => ColType::Bytes,
        SqlValue::Date32(_) => ColType::Date32,
        SqlValue::TimestampMs(_) => ColType::TimestampMs,
    }
}

/// 全查询求值（不含输出构造）
pub(crate) fn eval_query(
    db: &Database,
    sess: &mut Session,
    q: &Query,
    snapshot: u64,
) -> Result<TableView> {
    let set_expr = q.body.as_ref();
    let select = match set_expr {
        SetExpr::Select(s) => s.as_ref(),
        other => {
            return Err(SqlError::not_supported(format!(
                "set op: {}",
                short_str(other)
            )))
        }
    };
    // DISTINCT 投影显式拒绝（第二十一轮 R21-17：静默忽略 = 语义黑洞）
    if select.distinct.is_some() {
        return Err(SqlError::not_supported("SELECT DISTINCT"));
    }
    // Q-1 LIMIT 下推：无 ORDER BY **且无 WHERE** 时扫描期早停——
    // WHERE 过滤后行数未知，先截断会静默漏行（第十八轮 R18-1 探针实证：
    // WHERE id>=900 LIMIT 5 曾返回 0 行）；有 ORDER BY 需全量排序，不下推
    let has_join = select.from.iter().any(|twj| !twj.joins.is_empty());
    // R21-1：聚合/GROUP BY/HAVING 存在时 LIMIT 作用于聚合结果集而非扫描行，
    // 下推到扫描层会静默截断（count(*) LIMIT 1 返回 1 而非全表计数）
    let has_agg = projection_aggregates(&select.projection).is_some()
        || select.having.as_ref().map(has_agg_expr).unwrap_or(false);
    let has_group = matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(_, _) if !select.group_by.to_string().is_empty());
    let pushdown_limit: Option<usize> = match (&q.order_by, &q.limit_clause) {
        _ if select.selection.is_some() || has_join || has_agg || has_group => None,
        (None, Some(sqlparser::ast::LimitClause::LimitOffset { limit, offset, .. })) => {
            let off = match offset {
                Some(off) => Some(eval_const(&off.value)? as usize),
                None => None,
            };
            let lim = match limit {
                Some(l) => Some(eval_const(l)? as usize),
                None => None,
            };
            match (lim, off) {
                (Some(l), Some(o)) => Some(l + o),
                (Some(l), None) => Some(l),
                _ => None,
            }
        }
        _ => None,
    };
    let mut tv = eval_from(db, sess, select, snapshot, pushdown_limit)?;
    // WHERE
    if let Some(w) = &select.selection {
        // 常量短路（Q-1 优化器）：WHERE 表达式不含列引用时单次求值——
        // false/NULL → 跳过扫描直接返回空集（免全表遍历+行解码）；
        // true → 跳过过滤（恒真条件不需逐行判定）
        if !has_column_ref(w) {
            match expr::eval(w, &[], &|_| None) {
                Ok(SqlValue::Bool(false)) | Ok(SqlValue::Null) => {
                    return Ok(TableView {
                        names: tv.names.clone(),
                        rows: vec![],
                    });
                }
                Ok(SqlValue::Bool(true)) => return Ok(tv), // 恒真：免过滤
                _ => {}                                    // 非布尔：走正常过滤（行级报错）
            }
        }
        let cols = col_lookup(&tv.names);
        // WHERE 过滤（求值错误 → 语句失败，不静默吞）
        let mut filtered = Vec::with_capacity(tv.rows.len());
        for row in tv.rows.drain(..) {
            match expr::eval(w, &row, &cols) {
                Ok(SqlValue::Bool(true)) => filtered.push(row),
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
        tv.rows = filtered;
    }
    // GROUP BY / 聚合 / HAVING
    let has_agg = projection_aggregates(&select.projection).is_some()
        || select.having.as_ref().map(has_agg_expr).unwrap_or(false);
    let group_exprs: Vec<Expr> = match &select.group_by {
        GroupByExpr::All(_) => return Err(SqlError::not_supported("GROUP BY ALL")),
        GroupByExpr::Expressions(e, _) => e.clone(),
    };
    let out_names: Vec<String>;
    let mut out_rows: Vec<Vec<SqlValue>>;
    if !group_exprs.is_empty() || has_agg {
        let calls = collect_agg_calls(&select.projection, select.having.as_ref(), &group_exprs)?;
        let cols = cols_lookup(&tv.names);
        let res = agg::group_aggregate(&tv, &group_exprs, &calls, &cols)?;
        // HAVING 过滤
        let mut kept: Vec<usize> = Vec::new();
        for i in 0..res.keys.len() {
            if let Some(h) = &select.having {
                let hv =
                    agg::eval_having(h, &calls, &res.vals[i], &group_exprs, &res.keys[i], &cols)?;
                if hv != SqlValue::Bool(true) {
                    continue;
                }
            }
            kept.push(i);
        }
        // 投影：每个 SelectItem expr → 组键位置或聚合值位置
        let mut rows_out = Vec::with_capacity(kept.len());
        for &i in &kept {
            let mut row = Vec::with_capacity(select.projection.len());
            for item in &select.projection {
                let e = match item {
                    SelectItem::UnnamedExpr(e) => Some(e),
                    SelectItem::ExprWithAlias { expr, .. } => Some(expr),
                    _ => None,
                };
                match e {
                    Some(e) => {
                        if let Some(ci) = calls.iter().position(|c| c.display == e.to_string()) {
                            row.push(res.vals[i][ci].clone());
                        } else if let Some(gi) = group_exprs
                            .iter()
                            .position(|g| g.to_string() == e.to_string())
                        {
                            row.push(res.keys[i][gi].clone());
                        } else {
                            return Err(SqlError::syntax(
                                "column must appear in GROUP BY or aggregation",
                            ));
                        }
                    }
                    None => return Err(SqlError::not_supported("SELECT * with GROUP BY")),
                }
            }
            rows_out.push(row);
        }
        out_rows = rows_out;
        out_names = projection_names(&select.projection, &tv.names, &calls)?;
    } else {
        // 投影
        let (names, proj_rows) = project(&select.projection, &tv)?;
        out_names = names;
        out_rows = proj_rows;
    }
    let order_exprs: &[OrderByExpr] = match q.order_by.as_ref().map(|o| &o.kind) {
        Some(sqlparser::ast::OrderByKind::Expressions(exprs)) => exprs,
        Some(sqlparser::ast::OrderByKind::All(_)) | None => &[],
    };
    if !order_exprs.is_empty() {
        let has_input = out_rows.len() == tv.rows.len();
        apply_order(
            &mut out_rows,
            &out_names,
            order_exprs,
            if has_input { Some(&tv) } else { None },
        )?;
    }
    // OFFSET/LIMIT（0.62: limit_clause）
    if let Some(lc) = &q.limit_clause {
        match lc {
            sqlparser::ast::LimitClause::LimitOffset { limit, offset, .. } => {
                if let Some(off) = offset {
                    let n = eval_const(&off.value)? as usize;
                    out_rows = out_rows.into_iter().skip(n).collect();
                }
                if let Some(l) = limit {
                    let n = eval_const(l)? as usize;
                    out_rows.truncate(n);
                }
            }
            other => return Err(SqlError::not_supported(format!("LIMIT form: {other}"))),
        }
    }
    Ok(TableView {
        names: out_names,
        rows: out_rows,
    })
}

fn short_str(s: &impl std::fmt::Display) -> String {
    s.to_string().chars().take(40).collect()
}

pub(crate) fn describe_query(
    db: &Database,
    sess: &mut Session,
    q: &Query,
) -> Result<Vec<ColumnMeta>> {
    // 优先静态推断（schema/聚合规则）；失败回退空跑 LIMIT 0
    if let Some(meta) = describe_static(db, sess, q) {
        return Ok(meta);
    }
    let snapshot = sess.implicit_snapshot(db)?;
    let mut q2 = q.clone();
    q2.limit_clause = Some(sqlparser::ast::LimitClause::LimitOffset {
        limit: Some(Expr::Value(sqlparser::ast::ValueWithSpan {
            value: PV::Number("0".into(), false),
            span: sqlparser::tokenizer::Span::empty(),
        })),
        offset: None,
        limit_by: vec![],
    });
    let view = eval_query(db, sess, &q2, snapshot)?;
    let tys = if view.names.is_empty() {
        vec![]
    } else {
        (0..view.names.len())
            .map(|i| {
                view.rows
                    .first()
                    .map(|r| infer_type(&r[i]))
                    .unwrap_or(ColType::Utf8)
            })
            .collect()
    };
    Ok(view
        .names
        .iter()
        .zip(tys)
        .map(|(n, t)| ColumnMeta {
            name: n.clone(),
            ty: t,
        })
        .collect())
}

/// 静态列型：单表 SELECT 的 schema 直查 + 聚合函数规则
fn describe_static(db: &Database, sess: &mut Session, q: &Query) -> Option<Vec<ColumnMeta>> {
    let select = match q.body.as_ref() {
        SetExpr::Select(s) => s.as_ref(),
        _ => return None,
    };
    // 单表且无 join 才做静态
    let twj = select.from.first()?;
    if !twj.joins.is_empty() {
        return None;
    }
    let full = match &twj.relation {
        TableFactor::Table { name, .. } => name
            .0
            .iter()
            .filter_map(|p| p.as_ident())
            .map(|i| i.value.clone())
            .collect::<Vec<_>>()
            .join("."),
        TableFactor::Derived { .. } => {
            // 子查询：递归 describe 取列
            let inner = eval_query(db, sess, subquery_of(&twj.relation)?, 0).ok();
            return inner.map(|v| {
                v.names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| ColumnMeta {
                        name: n.clone(),
                        ty: v
                            .rows
                            .first()
                            .map(|r| infer_type(&r[i]))
                            .unwrap_or(ColType::Utf8),
                    })
                    .collect()
            });
        }
        _ => return None,
    };
    let (schema, _) = resolve_table(db, &sess.branch, &full).ok()?;
    let col_ty = |c: &str| schema.col_index(c).map(|i| schema.columns[i].ty);
    let agg_ty = |name: &str, arg: Option<&Expr>| -> ColType {
        match name {
            "count" => ColType::Int64,
            "avg" => ColType::Float64,
            "sum" => arg
                .and_then(|e| match e {
                    Expr::Identifier(id) => col_ty(&id.value),
                    _ => None,
                })
                .unwrap_or(ColType::Int64),
            _ => arg
                .and_then(|e| match e {
                    Expr::Identifier(id) => col_ty(&id.value),
                    _ => None,
                })
                .unwrap_or(ColType::Utf8),
        }
    };
    let mut out = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => {
                for c in &schema.columns {
                    out.push(ColumnMeta {
                        name: c.name.clone(),
                        ty: c.ty,
                    });
                }
            }
            SelectItem::QualifiedWildcard(_, _) => return None,
            SelectItem::ExprWithAlias { expr, alias } => {
                let ty = col_ty(&alias.value).unwrap_or_else(|| expr_ty(expr, &col_ty, &agg_ty));
                out.push(ColumnMeta {
                    name: alias.value.clone(),
                    ty,
                });
            }
            SelectItem::UnnamedExpr(e) => {
                let name = match e {
                    Expr::Identifier(id) => id.value.clone(),
                    other => crate::sql::agg_display(other),
                };
                let ty = expr_ty(e, &col_ty, &agg_ty);
                out.push(ColumnMeta { name, ty });
            }
            SelectItem::ExprWithAliases { .. } => return None,
        }
    }
    Some(out)
}

fn subquery_of(tf: &TableFactor) -> Option<&Query> {
    match tf {
        TableFactor::Derived { subquery, .. } => Some(subquery),
        _ => None,
    }
}

fn expr_ty(
    e: &Expr,
    col_ty: &dyn Fn(&str) -> Option<ColType>,
    agg: &dyn Fn(&str, Option<&Expr>) -> ColType,
) -> ColType {
    match e {
        Expr::Identifier(id) => col_ty(&id.value).unwrap_or(ColType::Utf8),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|p| col_ty(&p.value).unwrap_or(ColType::Utf8))
            .unwrap_or(ColType::Utf8),
        Expr::Function(f) => {
            let n = f.name.to_string().to_ascii_lowercase();
            if matches!(n.as_str(), "count" | "sum" | "avg" | "min" | "max") {
                let arg = crate::sql::scan::fn_args(f).first().and_then(|a| match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                    _ => None,
                });
                agg(&n, arg)
            } else if matches!(n.as_str(), "length" | "char_length") {
                ColType::Int32
            } else if matches!(
                n.as_str(),
                "round" | "floor" | "ceil" | "sqrt" | "pow" | "avg"
            ) {
                ColType::Float64
            } else {
                ColType::Utf8
            }
        }
        Expr::Cast { data_type, .. } => {
            ColType::from_parse(&data_type.to_string()).unwrap_or(ColType::Utf8)
        }
        Expr::BinaryOp { .. } => {
            // 数值运算 → 保守 Float64；比较 → bool
            ColType::Float64
        }
        _ => ColType::Utf8,
    }
}

fn eval_const(v: &Expr) -> Result<i64> {
    let v = expr::eval(v, &[], &|_| None)?;
    expr::as_i64(&v)
}

fn col_lookup(names: &[String]) -> impl Fn(&str) -> Option<usize> + '_ {
    move |name: &str| {
        let low = name.to_ascii_lowercase();
        names.iter().position(|n| n.to_ascii_lowercase() == low)
    }
}

fn cols_lookup(names: &[String]) -> HashMap<String, usize> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.to_ascii_lowercase(), i))
        .collect()
}

// ---------- FROM ----------

fn eval_from(
    db: &Database,
    sess: &mut Session,
    select: &Select,
    snapshot: u64,
    pushdown_limit: Option<usize>,
) -> Result<TableView> {
    let Some(twj) = select.from.first() else {
        // 无 FROM 常量投影（S 缺口，SELECT -3 / SELECT 1+1）：标准语义 =
        // 单行零列输入——投影/聚合（count(*) → 1）在此行上正常求值
        return Ok(TableView {
            names: vec![],
            rows: vec![vec![]],
        });
    };
    let mut tv = table_scan_opt(
        db,
        sess,
        &twj.relation,
        snapshot,
        select.selection.as_ref(),
        pushdown_limit,
    )?;
    for j in &twj.joins {
        match &j.join_operator {
            // sqlparser 0.62 区分裸 `JOIN`(Join) 与 `INNER JOIN`(Inner)、
            // 裸 `LEFT JOIN`(Left) 与 `LEFT OUTER JOIN`(LeftOuter)——语义相同
            JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => {
                let right = table_scan_opt(db, sess, &j.relation, snapshot, None, None)?;
                let (l, _r) = match constraint {
                    sqlparser::ast::JoinConstraint::On(e) => (e, None::<&Expr>),
                    sqlparser::ast::JoinConstraint::Natural => {
                        return Err(SqlError::not_supported("NATURAL JOIN"))
                    }
                    sqlparser::ast::JoinConstraint::Using(_) => {
                        return Err(SqlError::not_supported("USING"))
                    }
                    sqlparser::ast::JoinConstraint::None => {
                        return Err(SqlError::syntax("join requires ON"))
                    }
                };
                tv = hash_join(tv, right, l, sess.stmt_deadline)?;
            }
            JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
                let right = table_scan_opt(db, sess, &j.relation, snapshot, None, None)?;
                let e = match constraint {
                    sqlparser::ast::JoinConstraint::On(e) => e,
                    _ => return Err(SqlError::not_supported("LEFT JOIN constraint")),
                };
                tv = hash_join_left(tv, right, e, sess.stmt_deadline)?;
            }
            _other => return Err(SqlError::not_supported("join type")),
        }
    }
    Ok(tv)
}

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
fn table_scan_opt(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    snapshot: u64,
    selection: Option<&Expr>,
    pushdown_limit: Option<usize>,
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
    // 快路径 1：单表 + pk 等值/IN 且无其他复杂谓词 → 直查
    if let Some((schema, entry)) = try_pk_pushdown(db, sess, tf, selection, snapshot)? {
        let sel = selection.expect("pushdown implies selection");
        return build_point_view(db, sess, &schema, &entry, sel, snapshot);
    }
    // 快路径 2：列存投影可用（AP 路径，>= 1 万行）→ CBF 扫描 + zone map 剪枝
    if let Some(tv) = try_ap_scan(db, sess, tf, selection, snapshot, pushdown_limit)? {
        return Ok(tv);
    }
    table_scan(db, sess, tf, snapshot, pushdown_limit, selection)
}

/// AP 路径：表有列存投影且行数达标时走 CBF 扫描
fn try_ap_scan(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    selection: Option<&Expr>,
    snapshot: u64,
    pushdown_limit: Option<usize>,
) -> Result<Option<TableView>> {
    let name = match tf {
        TableFactor::Table { name, version, .. } => {
            // P1-10：AP 列存只有当前物化段——带版本子句的查询走 time travel
            // 路径（列存历史快照 v2；此前 version 被忽略 → 历史查询读当前段）
            if version.is_some() {
                return Ok(None);
            }
            name.0
                .iter()
                .filter_map(|p| p.as_ident())
                .map(|i| i.value.clone())
                .collect::<Vec<_>>()
                .join(".")
        }
        _ => return Ok(None),
    };
    let Some(ap) = db.columnar() else {
        return Ok(None);
    };
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
    let Ok((schema, entry)) = resolved else {
        return Ok(None);
    };
    if entry.col_segments.is_empty() || entry.col_rows < 10_000 {
        return Ok(None);
    }
    // pk 范围提取（order 域，开区间语义收集）
    let mut pk_range: Option<(Option<u64>, Option<u64>)> = None;
    if schema.pk.len() == 1 {
        let pk_name = schema.columns[schema.pk[0] as usize].name.clone();
        if let Some(sel) = selection {
            pk_range = extract_pk_range(sel, &pk_name);
        }
    }
    // 多段扫描：新段优先（段列表末尾=最新），pk 去重 + delete 抑制
    let mut rows = Vec::with_capacity(entry.col_rows as usize);
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let pkc = schema.pk[0] as usize;
    let _deletes: std::collections::HashSet<Vec<u8>> = entry
        .col_deletes
        .iter()
        .filter_map(|k| crate::format::hash::Hash::from_base32(k))
        .map(|h| h.as_bytes().to_vec())
        .collect();
    // col_deletes 存"行键的 hex"（与 encode_key 输出同一编码）
    let deletes: std::collections::HashSet<Vec<u8>> = entry
        .col_deletes
        .iter()
        .filter_map(|h| {
            (0..h.len() / 2)
                .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16))
                .collect::<std::result::Result<Vec<u8>, _>>()
                .ok()
        })
        .collect();
    for seg in entry.col_segments.iter().rev() {
        // 段级 pk 剪枝
        if let Some((lo, hi)) = pk_range {
            if hi <= Some(seg.pk_min) || lo >= Some(seg.pk_max) {
                continue;
            }
        }
        let batches = ap.scan(
            db.obj_store(),
            &schema,
            std::slice::from_ref(seg),
            &pk_range,
        )?;
        for b in &batches {
            for mut r in rows_from_batches(b, &schema)? {
                if pkc >= r.len() {
                    continue;
                }
                let key = crate::format::row::encode_key(&[r[pkc].clone()]);
                if seen.contains(&key) || deletes.contains(&key) {
                    continue;
                }
                seen.insert(key);
                r.resize(schema.columns.len(), SqlValue::Null);
                rows.push(r);
            }
        }
    }
    // 可见性归并（与 table_scan 同一抽象）：CBF 行 → memtx overlay →
    // 会话显式事务自身写（Q-14：此前 AP 路径不读事务的写——≥1 万行的
    // 显式事务内查询与行路径不一致）
    let b = db.branch(&sess.branch)?;
    let overlay = b.mem.table(entry.id).snapshot_rows(snapshot);
    let txn_has_writes = sess
        .txn
        .as_ref()
        .map(|t| t.explicit && t.writes.keys().any(|(tid, _)| *tid == entry.id))
        .unwrap_or(false);
    if !overlay.is_empty() || txn_has_writes {
        let mut keyed: std::collections::BTreeMap<Vec<u8>, Vec<SqlValue>> =
            std::collections::BTreeMap::new();
        for r in rows {
            let k = crate::format::row::encode_key(&[r[pkc].clone()]);
            keyed.insert(k, r);
        }
        for (k, ov) in overlay {
            match ov {
                Some(v) => {
                    if let Ok(r) = row_from_bytes(&schema, &v) {
                        keyed.insert(k, r);
                    }
                }
                None => {
                    keyed.remove(&k);
                }
            }
        }
        // ③ 会话显式事务自身写（最后覆盖）
        if let Some(t) = &sess.txn {
            if t.explicit {
                for ((tid, k), m) in &t.writes {
                    if *tid != entry.id {
                        continue;
                    }
                    match m {
                        crate::prolly::Mutation::Put(v) => {
                            if let Ok(r) = row_from_bytes(&schema, v) {
                                keyed.insert(k.clone(), r);
                            }
                        }
                        crate::prolly::Mutation::Delete => {
                            keyed.remove(k);
                        }
                    }
                }
            }
        }
        // Q-1：AP 路径同口径早停——BTreeMap 键序前 cap 行
        if let Some(cap) = pushdown_limit {
            rows = keyed.values().take(cap).cloned().collect();
        } else {
            rows = keyed.into_values().collect();
        }
    }
    let names = schema.columns.iter().map(|c| c.name.clone()).collect();
    Ok(Some(TableView { names, rows }))
}

/// order 域（定点解释）值：与 CBF footer 的 min/max 同口径
fn order_domain(v: &SqlValue) -> Option<u64> {
    Some(match v {
        SqlValue::Int32(i) => (*i as i64 ^ i64::MIN) as u64,
        SqlValue::Int64(i) => (i ^ i64::MIN) as u64,
        SqlValue::Date32(d) => (*d as i64 ^ i64::MIN) as u64,
        SqlValue::TimestampMs(t) => (t ^ i64::MIN) as u64,
        SqlValue::Float64(f) => crate::format::row::f64_to_orderable(*f),
        SqlValue::Utf8(s) => {
            let mut b = [0u8; 8];
            for (i, byte) in s.as_bytes().iter().take(8).enumerate() {
                b[i] = *byte;
            }
            u64::from_be_bytes(b)
        }
        _ => return None,
    })
}

/// 从 WHERE 提取单列 pk 的范围 (min_excl, max_incl)
fn extract_pk_range(sel: &Expr, pk: &str) -> Option<(Option<u64>, Option<u64>)> {
    fn lit_of(e: &Expr) -> Option<SqlValue> {
        if let Expr::Value(vws) = e {
            if !matches!(vws.value, sqlparser::ast::Value::Placeholder(_)) {
                return Some(super::expr::value_from_parser(vws.value.clone()));
            }
        }
        None
    }
    fn col_is(e: &Expr, pk: &str) -> bool {
        match e {
            Expr::Identifier(id) => id.value.eq_ignore_ascii_case(pk),
            _ => false,
        }
    }
    let mut lo: Option<u64> = None;
    let mut hi: Option<u64> = None;
    fn walk(e: &Expr, pk: &str, lo: &mut Option<u64>, hi: &mut Option<u64>) {
        match e {
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::And,
                right,
            } => {
                walk(left, pk, lo, hi);
                walk(right, pk, lo, hi);
            }
            Expr::BinaryOp { left, op, right } => {
                let (col, val, flip) = if col_is(left, pk) {
                    (true, lit_of(right), false)
                } else if col_is(right, pk) {
                    (true, lit_of(left), true)
                } else {
                    (false, None, false)
                };
                if let (true, Some(v)) = (col, val) {
                    let Some(d) = order_domain(&v) else { return };
                    use sqlparser::ast::BinaryOperator as BO;
                    let eff = match (op, flip) {
                        (BO::Gt, false) | (BO::Lt, true) => Some(("gt", d)),
                        (BO::GtEq, false) | (BO::LtEq, true) => Some(("gte", d)),
                        (BO::Lt, false) | (BO::Gt, true) => Some(("lt", d)),
                        (BO::LtEq, false) | (BO::GtEq, true) => Some(("lte", d)),
                        (BO::Eq, _) => Some(("eq", d)),
                        _ => None,
                    };
                    if let Some((kind, dv)) = eff {
                        match kind {
                            "gt" => {
                                if lo.is_none_or(|l| dv > l) {
                                    *lo = Some(dv);
                                }
                            }
                            "gte" => {
                                if lo.is_none_or(|l| dv.saturating_sub(1) > l) {
                                    *lo = Some(dv.saturating_sub(1));
                                }
                            }
                            "eq" => {
                                *lo = Some(dv.saturating_sub(1));
                                *hi = Some(dv + 1);
                            }
                            "lte" => {
                                if hi.is_none_or(|h| dv < h) {
                                    *hi = Some(dv);
                                }
                            }
                            "lt" if hi.is_none_or(|h| dv.saturating_sub(1) < h) => {
                                *hi = Some(dv.saturating_sub(1));
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    walk(sel, pk, &mut lo, &mut hi);
    if lo.is_none() && hi.is_none() {
        None
    } else {
        Some((lo, hi))
    }
}

/// Arrow 批 → 行（SqlValue），列型按 schema 收敛
fn rows_from_batches(
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

/// 提取单列数字 PK 的范围合取（P2-6g）：返回 (lo, hi)，各为 (值, 是否含端点)。
/// 识别 AND 树中的 `pk >/>=/</<= 字面量`（字面量在另一侧亦可，方向自动翻转）；
/// 非 PK/非数字字面量的合取返回 None——由下游常规过滤承担，不影响正确性。
/// 返回 None = 无任何范围界（不做下推）。
fn extract_pk_int_range(e: &Expr, pk: &str) -> Option<(Option<(i64, bool)>, Option<(i64, bool)>)> {
    use sqlparser::ast::BinaryOperator as Op;
    match e {
        Expr::Nested(inner) => extract_pk_int_range(inner, pk),
        Expr::BinaryOp {
            left,
            op: Op::And,
            right,
        } => {
            let a = extract_pk_int_range(left, pk);
            let b = extract_pk_int_range(right, pk);
            match (a, b) {
                (Some((l1, h1)), Some((l2, h2))) => {
                    // 交：lo 取值更大者、同值取排他（更紧）；hi 取值更小者、
                    // 同值取排他（更紧）
                    let lo = match (l1, l2) {
                        (Some(x), Some(y)) => Some(match (x, y) {
                            (x, y) if x.0 > y.0 => x,
                            (x, y) if x.0 < y.0 => y,
                            (x, y) => {
                                if !x.1 {
                                    x
                                } else {
                                    y
                                }
                            }
                        }),
                        (x, None) => x,
                        (None, y) => y,
                    };
                    let hi = match (h1, h2) {
                        (Some(x), Some(y)) => Some(match (x, y) {
                            (x, y) if x.0 < y.0 => x,
                            (x, y) if x.0 > y.0 => y,
                            (x, y) => {
                                if !x.1 {
                                    x
                                } else {
                                    y
                                }
                            }
                        }),
                        (x, None) => x,
                        (None, y) => y,
                    };
                    Some((lo, hi))
                }
                (a, b) => a.or(b),
            }
        }
        Expr::BinaryOp {
            left,
            op: op @ (Op::Gt | Op::GtEq | Op::Lt | Op::LtEq),
            right,
        } => {
            // 主键列在左/右识别 + 字面量提取
            let (col_first, other): (bool, &Expr) = match (left.as_ref(), right.as_ref()) {
                (Expr::Identifier(id), v) if id.value.eq_ignore_ascii_case(pk) => (true, v),
                (v, Expr::Identifier(id)) if id.value.eq_ignore_ascii_case(pk) => (false, v),
                _ => return None,
            };
            let v = match other {
                Expr::Value(vws) => super::expr::value_from_parser(vws.value.clone()),
                Expr::UnaryOp {
                    op: sqlparser::ast::UnaryOperator::Minus,
                    expr: inner,
                } => match inner.as_ref() {
                    // 小数字面量是 Int32（number_or_string 窄化）——两档都要取负
                    Expr::Value(vws) => match super::expr::value_from_parser(vws.value.clone()) {
                        SqlValue::Int64(i) => SqlValue::Int64(-i),
                        SqlValue::Int32(i) => SqlValue::Int64(-(i as i64)),
                        _ => return None,
                    },
                    _ => return None,
                },
                _ => return None,
            };
            let i = match v {
                SqlValue::Int64(i) => i,
                SqlValue::Int32(i) => i as i64,
                _ => {
                    eprintln!("[range-probe] literal bail: v={v:?} other={other:?}");
                    return None;
                }
            };
            let lower = matches!(op, Op::Gt | Op::GtEq);
            let incl = matches!(op, Op::GtEq | Op::LtEq);
            if col_first == lower {
                // col 在左且是下界（>/>=），或 col 在右且是上界（</<=）
                Some((Some((i, incl)), None))
            } else {
                // col 在左且是上界（</<=），或 col 在右且是下界（>/>=）
                Some((None, Some((i, incl))))
            }
        }
        _ => None,
    }
}

/// 范围界 → 树键界（encode_key 对整数是 8B 定宽保序：值域 ±1 即键域 ±1）。
/// hi 一律转排他（range_scan 端点排他）；溢出 = 该侧无界（由下游过滤兜底）。
fn pk_range_keys(
    lo: &Option<(i64, bool)>,
    hi: &Option<(i64, bool)>,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let start = match lo {
        Some((v, true)) => Some(crate::format::row::encode_key(&[SqlValue::Int64(*v)])),
        Some((v, false)) => v
            .checked_add(1)
            .map(|x| crate::format::row::encode_key(&[SqlValue::Int64(x)])),
        None => None,
    };
    let end = match hi {
        Some((v, true)) => v
            .checked_add(1)
            .map(|x| crate::format::row::encode_key(&[SqlValue::Int64(x)])),
        Some((v, false)) => Some(crate::format::row::encode_key(&[SqlValue::Int64(*v)])),
        None => None,
    };
    (start, end)
}

/// 判定 WHERE 是否为 pk 直查形态
fn try_pk_pushdown(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    selection: Option<&Expr>,
    _snapshot: u64,
) -> Result<Option<(crate::versioned::TableSchema, crate::versioned::TableEntry)>> {
    let name = match tf {
        TableFactor::Table { name, version, .. } => {
            // P1-10：PK 直查读 memtx ∪ 当前树——无历史根概念。带版本子句的
            // 查询必须回落 table_scan 的 time travel 路径（此前 version 被
            // 忽略 → 历史点查静默返回当前数据 + 泄漏在途 memtx 行）
            if version.is_some() {
                return Ok(None);
            }
            name.0
                .iter()
                .filter_map(|p| p.as_ident())
                .map(|i| i.value.clone())
                .collect::<Vec<_>>()
                .join(".")
        }
        _ => return Ok(None),
    };
    let low = name.to_ascii_lowercase();
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
        return Ok(None);
    }
    let Some(sel) = selection else {
        return Ok(None);
    };
    // 单列主键 + 顶层 Eq/IN 形态才走直查
    // （显式事务：以 BEGIN 冻结的 catalog 根解析，R7-3）
    let frozen = match &sess.txn {
        Some(t) if t.explicit => t.head_root,
        _ => None,
    };
    let (schema, entry) = match frozen {
        Some(r) => resolve_table_at(
            db,
            Some(&r),
            name.rsplit(['.', '@']).next().unwrap_or(&name),
        )?,
        None => match resolve_table(db, &sess.branch, &name) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        },
    };
    if schema.pk.len() != 1 {
        return Ok(None);
    }
    let pk_name = schema.columns[schema.pk[0] as usize].name.clone();
    let ok = match sel {
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::Eq,
            right,
        } => is_col_vs_value(left, right, &pk_name) || is_col_vs_value(right, left, &pk_name),
        Expr::InList { expr, .. } => match expr.as_ref() {
            Expr::Identifier(id) => id.value.eq_ignore_ascii_case(&pk_name),
            _ => false,
        },
        _ => false,
    };
    Ok(if ok { Some((schema, entry)) } else { None })
}

fn is_col_vs_value(a: &Expr, b: &Expr, pk: &str) -> bool {
    match (a, b) {
        (Expr::Identifier(id), Expr::Value(_)) => id.value.eq_ignore_ascii_case(pk),
        (Expr::Value(_), Expr::Identifier(id)) => id.value.eq_ignore_ascii_case(pk),
        _ => false,
    }
}

/// pk 直查视图：memtx ∪ 树
/// 表达式 → 字面量 SqlValue（S-3 负数审计 R8）：`Value` 与
/// `UnaryOp::Minus(数值)` 两类；其余（列引用/函数/算式）→ None。
/// 此前 Eq/IN 直查只认 `Expr::Value`——负数字面量被静默过滤成
/// 空键集（`id = -3` 恰好有非下推回退路径兜底，`IN (-3, 4)` 则
/// **静默丢行**）。
fn expr_to_literal(e: &Expr) -> Option<SqlValue> {
    match e {
        Expr::Value(vws) => Some(super::expr::value_from_parser(vws.value.clone())),
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr: inner,
        } => match super::expr::value_from_parser(match inner.as_ref() {
            Expr::Value(vws) => vws.value.clone(),
            _ => return None,
        }) {
            SqlValue::Int64(i) => Some(SqlValue::Int64(-i)),
            SqlValue::Int32(i) => Some(SqlValue::Int32(-i)),
            SqlValue::Float64(f) => Some(SqlValue::Float64(-f)),
            _ => None,
        },
        _ => None,
    }
}

fn build_point_view(
    db: &Database,
    sess: &mut Session,
    schema: &crate::versioned::TableSchema,
    entry: &crate::versioned::TableEntry,
    selection: &Expr,
    snapshot: u64,
) -> Result<TableView> {
    use sqlparser::ast::{BinaryOperator, Expr};
    let pk_name = schema.columns[schema.pk[0] as usize].name.clone();
    let keys: Vec<SqlValue> = match selection {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let v: &Expr = if is_col_vs_value(left, right, &pk_name) {
                right.as_ref()
            } else {
                left.as_ref()
            };
            expr_to_literal(v).into_iter().collect()
        }
        Expr::InList {
            expr,
            list,
            negated,
        } if !*negated => {
            let _ = expr;
            list.iter().filter_map(expr_to_literal).collect()
        }
        _ => vec![],
    };
    let b = db.branch(&sess.branch)?;
    let tm = b.mem.table(entry.id);
    let tree_root = entry
        .table_root
        .as_ref()
        .and_then(|s| crate::format::hash::Hash::from_base32(s));
    let mut rows = Vec::with_capacity(keys.len());
    for pkv in keys {
        let key = crate::format::row::encode_key(std::slice::from_ref(&pkv));
        // memtx 优先；**墓碑判定**（第七轮 R7-1 伴生①）：memtx 无值时可能是
        // "可见墓碑"（该 key 在快照前已被删除）——此时不得回退树复活被删行
        let mut found: Option<Arc<Vec<u8>>> = match tm.get(&key, snapshot) {
            Some(v) => Some(v),
            None => {
                let tombstoned = tm.latest_ts(&key).is_some_and(|ts| ts <= snapshot);
                if tombstoned {
                    None
                } else {
                    match &tree_root {
                        Some(r) => crate::prolly::cursor::lookup(&db.store, r, &key)?.map(Arc::new),
                        None => None,
                    }
                }
            }
        };
        // 会话显式事务自身写（R8-1）：点查同样读自己的写
        if let Some(t) = &sess.txn {
            if t.explicit {
                if let Some(m) = t.writes.get(&(entry.id, key.clone())) {
                    match m {
                        crate::prolly::Mutation::Put(v) => found = Some(Arc::new(v.clone())),
                        crate::prolly::Mutation::Delete => found = None,
                    }
                }
            }
        }
        if let Some(v) = found {
            rows.push(row_from_bytes(schema, &v)?);
        }
    }
    let names = schema.columns.iter().map(|c| c.name.clone()).collect();
    Ok(TableView { names, rows })
}

// ---------- time travel（P1-10）----------
//
// 语法：`FROM <table> FOR SYSTEM_TIME AS OF '<ts_ms | ISO8601 | commit_hash>' [AS alias]`
// 语义（Dolt 式提交快照，非逐行系统版本）：
// - 提交哈希：该 commit 的物化树根原样读（commit = checkpoint = 完整态）；
// - 时间戳：沿分支**第一父链**找 ts_ms ≤ 目标的最近提交——链跨分支边界
//   （fork 继承源提交），故可回溯到 fork 之前；早于首个提交 → 22023。
// - 快照是**只读历史树**：无 memtx overlay、无会话写、无 checkpoint 推进
//   （历史树 chunk 不可变，天然稳定）。checkpoint 之后的未物化事务不在任何
//   提交里 ⇒ 时间旅行精度 = 提交粒度（文档口径，SPEC 03 §5）。

fn time_travel_scan(
    db: &Database,
    branch: &str,
    full_name: &str,
    version: &sqlparser::ast::TableVersion,
    pushdown_limit: Option<usize>,
) -> Result<TableView> {
    let commit = match resolve_as_of(db, branch, version)? {
        Some(c) => c,
        None => {
            return Err(SqlError::new(
                "22023",
                format!(
                    "time travel: no snapshot at or before the given point on branch \"{branch}\""
                ),
            ))
        }
    };
    let short_name = full_name.rsplit(['.', '@']).next().unwrap_or(full_name);
    let (schema, entry) = resolve_table_at(db, Some(&commit.root), short_name)?;
    let root = entry
        .table_root
        .as_ref()
        .and_then(|s| crate::format::hash::Hash::from_base32(s));
    let mut rows = Vec::new();
    if let Some(r) = &root {
        let mut it = crate::prolly::cursor::TreeIter::new(db.store.clone(), r)?;
        while let Some((k, v)) = it.next_item()? {
            let _ = k; // 键 = 主键编码，行内已含列值
            if let Some(cap) = pushdown_limit {
                if rows.len() >= cap {
                    break;
                }
            }
            rows.push(row_from_bytes(&schema, &v)?);
        }
    }
    let names = schema.columns.iter().map(|c| c.name.clone()).collect();
    Ok(TableView { names, rows })
}

/// 解析 `AS OF` 目标：提交哈希（CAS 直接命中）或时间戳（第一父链最近提交）。
/// Ok(None) = 早于历史起点。
fn resolve_as_of(
    db: &Database,
    branch: &str,
    version: &sqlparser::ast::TableVersion,
) -> Result<Option<crate::versioned::commit::Commit>> {
    let sqlparser::ast::TableVersion::ForSystemTimeAsOf(expr) = version else {
        return Err(SqlError::not_supported(format!("AS OF: {version}")));
    };
    let literal = match expr {
        sqlparser::ast::Expr::Value(vws) => match &vws.value {
            sqlparser::ast::Value::SingleQuotedString(s)
            | sqlparser::ast::Value::DoubleQuotedString(s) => s.clone(),
            sqlparser::ast::Value::Number(n, _) => n.clone(),
            other => return Err(SqlError::not_supported(format!("AS OF literal: {other}"))),
        },
        other => {
            return Err(SqlError::not_supported(format!(
                "AS OF: only string/number literals supported, got {other}"
            )))
        }
    };
    // ① 提交哈希：base32 可解码且 CAS 命中 → 精确提交（跨分支快照读允许）。
    // 类型标签非 Commit 的对象（node/schema/哈希猜测命中）→ 22023 而非 500；
    // CAS 未命中 → 落到时间戳解析路径
    if let Some(h) = crate::format::hash::Hash::from_base32(&literal) {
        if let Ok((ty, data)) = db.cas.get(&h) {
            if ty != crate::objstore::cas::ChunkType::Commit {
                return Err(SqlError::new(
                    "22023",
                    format!("AS OF \"{literal}\": object exists but is not a commit"),
                ));
            }
            return crate::versioned::commit::Commit::decode(&data)
                .map(Some)
                .map_err(|e| SqlError::internal(format!("as-of commit: {e}")));
        }
    }
    // ② 时间戳：epoch ms 或 ISO8601
    let target_ms = parse_as_of_ms(&literal)?;
    let b = db.branch(branch)?;
    let mut cur = b.head.load_full();
    let mut guard = 0u32;
    loop {
        match cur.as_ref() {
            None => return Ok(None), // 链尽（早于首个提交）
            Some(c) if c.ts_ms <= target_ms => return Ok(Some(c.clone())),
            Some(c) => match c.parents.first() {
                Some(p) => {
                    let (_ty, data) = db.cas.get(p)?;
                    cur = Arc::new(Some(crate::versioned::commit::Commit::decode(&data)?));
                    guard += 1;
                    if guard > 100_000 {
                        return Err(SqlError::internal("as-of history walk overflow"));
                    }
                }
                None => return Ok(None),
            },
        }
    }
}

/// `AS OF` 时间字面量：epoch 毫秒（纯数字）或 `YYYY-MM-DD[ T]HH:MM[:SS[.mmm]]Z?`
/// （缺省时间分量取 0；一律按 UTC 解释——文档口径）。错误 → 22023。
fn parse_as_of_ms(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    let s = s.strip_suffix('Z').unwrap_or(s);
    let s = s.trim_end_matches("+00:00");
    let bad = || {
        SqlError::new(
            "22023",
            format!("AS OF: cannot parse timestamp \"{s}\" (epoch ms or ISO8601 UTC)"),
        )
    };
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dp = date.split('-');
    let (y, mo, d) = match (dp.next(), dp.next(), dp.next()) {
        (Some(y), Some(mo), Some(d)) if dp.next().is_none() => (y, mo, d),
        _ => return Err(bad()),
    };
    let (y, mo, d) = (
        y.parse::<i64>().map_err(|_| bad())?,
        mo.parse::<u32>().map_err(|_| bad())?,
        d.parse::<u32>().map_err(|_| bad())?,
    );
    // 年份上界（轮次审计 R2）：无界年份在 days*86_400*1000 处 i64 溢出——
    // debug 构建 panic、release 静默回绕。±300_000 年远超任何提交时间线
    // 且算术余量 >30 倍（300000*366*86400*1000 ≈ 9.5e15 < i64::MAX/30）。
    if !(-300_000..=300_000).contains(&y) {
        return Err(bad());
    }
    // 月长/闰年校验（此前 2026-02-30 会滚动到 3 月 2 日）
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][(mo - 1) as usize];
    if !(1..=12).contains(&mo) || !(1..=dim).contains(&d) {
        return Err(bad());
    }
    let mut hh = 0i64;
    let mut mi = 0i64;
    let mut sec = 0i64;
    let mut ms = 0i64;
    if let Some(t) = time {
        let (hms, frac) = match t.split_once('.') {
            Some((h, f)) => (h, Some(f)),
            None => (t, None),
        };
        let parts: Vec<&str> = hms.split(':').collect();
        if parts.is_empty() || parts.len() > 3 {
            return Err(bad());
        }
        let parse = |v: &str| -> Result<i64> { v.parse::<i64>().map_err(|_| bad()) };
        hh = parse(parts[0])?;
        if parts.len() > 1 {
            mi = parse(parts[1])?;
        }
        if parts.len() > 2 {
            sec = parse(parts[2])?;
        }
        if let Some(f) = frac {
            // 任意位宽分数秒：取前 3 位为毫秒，余位须为数字（µs/ns 常见）
            if f.is_empty() || !f.chars().all(|c| c.is_ascii_digit()) {
                return Err(bad());
            }
            let f3: String = f.chars().take(3).collect();
            if !f3.is_empty() {
                ms = f3.parse::<i64>().map_err(|_| bad())? * 10i64.pow(3 - f3.len() as u32);
            }
        }
        if !(0..24).contains(&hh) || !(0..60).contains(&mi) || !(0..61).contains(&sec) {
            return Err(bad());
        }
    }
    // 民用日期 → Unix 天数（Howard Hinnant 算法，proleptic Gregorian）
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = ((mo + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(((days * 86_400 + hh * 3600 + mi * 60 + sec) * 1_000) + ms)
}

/// 单表扫描（含系统视图）；pk 等值条件下推走直查
fn table_scan(
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
        TableFactor::Derived { subquery, .. } => {
            let view = eval_query(db, sess, subquery.as_ref(), snapshot)?;
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
fn resolve_table_at(
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

fn row_from_bytes(schema: &crate::versioned::TableSchema, bytes: &[u8]) -> Result<Vec<SqlValue>> {
    let mut vals = decode_row(bytes)?;
    // schema 演化：补 NULL / 截断
    vals.resize(schema.columns.len(), SqlValue::Null);
    Ok(vals)
}

// ---------- JOIN ----------

fn hash_join(
    l: TableView,
    r: TableView,
    on: &Expr,
    deadline: Option<std::time::Instant>,
) -> Result<TableView> {
    // 找等值条件 col_l = col_r（支持 AND 链中提取多个）
    let eqs = extract_equi(on, &l.names, &r.names)?;
    let mut names = l.names.clone();
    names.extend(r.names.clone());
    // 建右表哈希（文本键：类型内规范）
    let mkkey = |row: &Vec<SqlValue>, idx: &[usize]| -> Option<Vec<String>> {
        let mut k = Vec::with_capacity(idx.len());
        for &i in idx {
            let v = row.get(i)?;
            if v.is_null() {
                return None;
            }
            // 类型 tag + 文本：防 Int64(1) 与 Utf8("1") 碰撞
            k.push(format!(
                "{}\u{0}{}",
                v.type_name(),
                expr::to_text(v.clone())
            ));
        }
        Some(k)
    };
    let mut hm: HashMap<Vec<String>, Vec<&Vec<SqlValue>>> = HashMap::new();
    let mut hj_chk = 0usize;
    for rr in &r.rows {
        hj_chk += 1;
        if hj_chk.is_multiple_of(4096) {
            if let Some(d) = deadline {
                if std::time::Instant::now() >= d {
                    return Err(SqlError::new("57014", "statement timeout"));
                }
            }
        }
        if let Some(key) = mkkey(rr, &eqs.ridx) {
            hm.entry(key).or_default().push(rr);
        }
    }
    let mut rows = Vec::new();
    for lr in &l.rows {
        let key = match mkkey(lr, &eqs.lidx) {
            Some(k) => k,
            None => continue,
        };
        if let Some(matches) = hm.get(&key) {
            for rr in matches {
                let mut row = lr.clone();
                row.extend(rr.iter().cloned());
                rows.push(row);
            }
        }
    }
    Ok(TableView { names, rows })
}

fn hash_join_left(
    l: TableView,
    r: TableView,
    on: &Expr,
    deadline: Option<std::time::Instant>,
) -> Result<TableView> {
    let eqs = extract_equi(on, &l.names, &r.names)?;
    let mut names = l.names.clone();
    names.extend(r.names.clone());
    let mut hm: HashMap<Vec<String>, Vec<&Vec<SqlValue>>> = HashMap::new();
    let mut hj_chk = 0usize;
    for rr in &r.rows {
        hj_chk += 1;
        if hj_chk.is_multiple_of(4096) {
            if let Some(d) = deadline {
                if std::time::Instant::now() >= d {
                    return Err(SqlError::new("57014", "statement timeout"));
                }
            }
        }
        let key: Vec<String> = eqs
            .ridx
            .iter()
            .map(|&i| expr::to_text(rr[i].clone()))
            .collect();
        if key.iter().any(|k| k == "\x00NULL") {
            continue;
        }
        hm.entry(key).or_default().push(rr);
    }
    let null_right = vec![SqlValue::Null; r.names.len()];
    let mut rows = Vec::new();
    for lr in &l.rows {
        let key: Vec<String> = eqs
            .lidx
            .iter()
            .map(|&i| expr::to_text(lr[i].clone()))
            .collect();
        let matched = if key.iter().any(|k| k == "\x00NULL") {
            None
        } else {
            hm.get(&key)
        };
        match matched {
            Some(ms) => {
                for rr in ms {
                    let mut row = lr.clone();
                    row.extend(rr.iter().cloned());
                    rows.push(row);
                }
            }
            None => {
                let mut row = lr.clone();
                row.extend(null_right.iter().cloned());
                rows.push(row);
            }
        }
    }
    Ok(TableView { names, rows })
}

struct EquiIdx {
    lidx: Vec<usize>,
    ridx: Vec<usize>,
}

/// 从 ON 条件提取 `l.c = r.c` 等值对（AND 链）
fn extract_equi(e: &Expr, ln: &[String], rn: &[String]) -> Result<EquiIdx> {
    let mut lidx = Vec::new();
    let mut ridx = Vec::new();
    fn walk(
        e: &Expr,
        ln: &[String],
        rn: &[String],
        li: &mut Vec<usize>,
        ri: &mut Vec<usize>,
    ) -> Result<bool> {
        match e {
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::And,
                right,
            } => {
                walk(left, ln, rn, li, ri)?;
                walk(right, ln, rn, li, ri)?;
                Ok(true)
            }
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::Eq,
                right,
            } => {
                // 两边各解析出一列：一属左表一属右表
                let le = col_pos(left, ln);
                let re = col_pos(right, rn);
                let lo = col_pos(left, rn);
                let ro = col_pos(right, ln);
                if let (Some(a), Some(b)) = (le, re) {
                    li.push(a);
                    ri.push(b);
                    Ok(true)
                } else if let (Some(a), Some(b)) = (lo, ro) {
                    li.push(b);
                    ri.push(a);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }
    if !walk(e, ln, rn, &mut lidx, &mut ridx)? {
        return Err(SqlError::not_supported(
            "JOIN ON: only equi-conditions supported",
        ));
    }
    if lidx.is_empty() {
        return Err(SqlError::not_supported("JOIN ON: no equi-condition found"));
    }
    Ok(EquiIdx { lidx, ridx })
}

fn col_pos(e: &Expr, names: &[String]) -> Option<usize> {
    match e {
        Expr::Identifier(id) => {
            let low = id.value.to_ascii_lowercase();
            names.iter().position(|n| n.to_ascii_lowercase() == low)
        }
        Expr::CompoundIdentifier(parts) => {
            let last = parts.last()?.value.to_ascii_lowercase();
            names.iter().position(|n| n.to_ascii_lowercase() == last)
        }
        _ => None,
    }
}

// ---------- 投影/聚合 ----------

fn project(p: &[SelectItem], tv: &TableView) -> Result<(Vec<String>, Vec<Vec<SqlValue>>)> {
    let cols = cols_lookup(&tv.names);
    let colfn = |name: &str| cols.get(&name.to_ascii_lowercase()).copied();
    let mut names = Vec::new();
    let mut items: Vec<(Expr, Option<String>)> = Vec::new();
    for item in p {
        match item {
            SelectItem::Wildcard(_) | SelectItem::ExprWithAliases { .. } => {
                for n in &tv.names {
                    names.push(n.clone());
                    items.push((
                        Expr::Identifier(sqlparser::ast::Ident::new(n.clone())),
                        None,
                    ));
                }
            }
            SelectItem::QualifiedWildcard(prefix, _) => {
                let pre = prefix.to_string().to_ascii_lowercase();
                for n in &tv.names {
                    if n.to_ascii_lowercase().starts_with(&pre) {
                        names.push(n.clone());
                        items.push((
                            Expr::Identifier(sqlparser::ast::Ident::new(n.clone())),
                            None,
                        ));
                    }
                }
            }
            SelectItem::ExprWithAlias { expr, alias, .. } => {
                names.push(alias.value.clone());
                items.push((expr.clone(), None));
            }
            SelectItem::UnnamedExpr(e) => {
                names.push(short_str(e));
                items.push((e.clone(), None));
            }
        }
    }
    let mut rows = Vec::with_capacity(tv.rows.len());
    for row in &tv.rows {
        let mut out = Vec::with_capacity(items.len());
        for (e, _) in &items {
            out.push(expr::eval(e, row, &colfn)?);
        }
        rows.push(out);
    }
    Ok((names, rows))
}

fn projection_names(p: &[SelectItem], tv: &[String], calls: &[AggCall]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for item in p {
        match item {
            SelectItem::Wildcard(_) | SelectItem::ExprWithAliases { .. } => {
                names.extend(tv.iter().cloned())
            }
            SelectItem::QualifiedWildcard(prefix, _) => {
                let pre = prefix.to_string().to_ascii_lowercase();
                names.extend(
                    tv.iter()
                        .filter(|n| n.to_ascii_lowercase().starts_with(&pre))
                        .cloned(),
                );
            }
            SelectItem::ExprWithAlias { alias, .. } => names.push(alias.value.clone()),
            SelectItem::UnnamedExpr(e) => {
                match calls.iter().find(|c| c.display == e.to_string()) {
                    // 命中聚合列：用聚合显示名
                    Some(c) => names.push(c.display.clone()),
                    // 其余（分组表达式或普通表达式）：按文本短形
                    None => names.push(short_str(e)),
                }
            }
        }
    }
    Ok(names)
}

fn projection_aggregates(p: &[SelectItem]) -> Option<()> {
    for item in p {
        let e = match item {
            SelectItem::UnnamedExpr(e) => e,
            SelectItem::ExprWithAlias { expr, .. } => expr,
            _ => continue,
        };
        if has_agg_expr(e) {
            return Some(());
        }
    }
    None
}

/// 递归检查表达式是否引用了任何列（Identifier/CompoundIdentifier）。
/// 用于 WHERE 常量短路：无列引用的表达式可在扫描前单次求值（Q-1 优化器）。
pub fn has_column_ref(e: &sqlparser::ast::Expr) -> bool {
    use sqlparser::ast::Expr;
    match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => true,
        Expr::BinaryOp { left, right, .. } => has_column_ref(left) || has_column_ref(right),
        Expr::UnaryOp { expr, .. } => has_column_ref(expr),
        Expr::Nested(inner) => has_column_ref(inner),
        Expr::Function(f) => fn_args(f).iter().any(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => has_column_ref(e),
            FunctionArg::Named {
                arg: FunctionArgExpr::Expr(e),
                ..
            } => has_column_ref(e),
            _ => false,
        }),
        Expr::Cast { expr, .. } => has_column_ref(expr),
        Expr::IsTrue(e)
        | Expr::IsFalse(e)
        | Expr::IsNotTrue(e)
        | Expr::IsNotFalse(e)
        | Expr::IsNull(e)
        | Expr::IsNotNull(e) => has_column_ref(e),
        Expr::InList { expr, list, .. } => has_column_ref(expr) || list.iter().any(has_column_ref),
        Expr::Between {
            expr, low, high, ..
        } => has_column_ref(expr) || has_column_ref(low) || has_column_ref(high),
        _ => false,
    }
}

pub(crate) fn fn_args(f: &sqlparser::ast::Function) -> &[FunctionArg] {
    match &f.args {
        sqlparser::ast::FunctionArguments::List(l) => &l.args,
        _ => &[] as &[FunctionArg],
    }
}

pub(crate) fn fn_distinct(f: &sqlparser::ast::Function) -> bool {
    match &f.args {
        sqlparser::ast::FunctionArguments::List(l) => {
            matches!(
                l.duplicate_treatment,
                Some(sqlparser::ast::DuplicateTreatment::Distinct)
            )
        }
        _ => false,
    }
}

fn has_agg_expr(e: &Expr) -> bool {
    // 深度优先找聚合函数名
    match e {
        Expr::Function(f) => {
            let n = f.name.to_string().to_ascii_lowercase();
            if matches!(n.as_str(), "count" | "sum" | "avg" | "min" | "max") {
                return true;
            }
            fn_args(f).iter().any(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => has_agg_expr(e),
                _ => false,
            })
        }
        Expr::BinaryOp { left, right, .. } => has_agg_expr(left) || has_agg_expr(right),
        Expr::Nested(i) => has_agg_expr(i),
        // 与 collect_agg_calls 的 Cast 分支对称：count(*)::text 曾被当普通
        // 表达式而报错（第七轮 R7-8）
        Expr::Cast { expr, .. } => has_agg_expr(expr),
        _ => false,
    }
}

fn collect_agg_calls(
    p: &[SelectItem],
    having: Option<&Expr>,
    _groups: &[Expr],
) -> Result<Vec<AggCall>> {
    let mut calls: Vec<AggCall> = Vec::new();
    fn visit(e: &Expr, calls: &mut Vec<AggCall>) {
        match e {
            Expr::Function(f) => {
                let n = f.name.to_string().to_ascii_lowercase();
                if matches!(n.as_str(), "count" | "sum" | "avg" | "min" | "max") {
                    let (arg_expr, distinct, is_star) = match fn_args(f).first() {
                        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => {
                            (Some(e.clone()), fn_distinct(f), false)
                        }
                        Some(FunctionArg::Unnamed(FunctionArgExpr::Wildcard)) => {
                            (None, false, true)
                        }
                        _ => (None, false, false),
                    };
                    let display = e.to_string();
                    if !calls.iter().any(|c| c.display == display) {
                        calls.push(AggCall {
                            func: n,
                            arg: arg_expr,
                            distinct,
                            is_star,
                            display,
                        });
                    }
                    return;
                }
                for a in fn_args(f) {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a {
                        visit(e, calls);
                    }
                }
            }
            Expr::BinaryOp { left, right, .. } => {
                visit(left, calls);
                visit(right, calls);
            }
            Expr::Nested(i) => visit(i, calls),
            Expr::Cast { expr, .. } => visit(expr, calls),
            _ => {}
        }
    }
    for item in p {
        match item {
            SelectItem::UnnamedExpr(e) => visit(e, &mut calls),
            SelectItem::ExprWithAlias { expr, .. } => visit(expr, &mut calls),
            _ => {}
        }
    }
    if let Some(h) = having {
        visit(h, &mut calls);
    }
    Ok(calls)
}

// ---------- ORDER ----------

fn apply_order(
    rows: &mut Vec<Vec<SqlValue>>,
    names: &[String],
    orders: &[OrderByExpr],
    input: Option<&TableView>,
) -> Result<()> {
    let cols = cols_lookup(names);
    let cols_in = input.map(|tv| cols_lookup(&tv.names));
    let mut keys: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
    for (ri, row) in rows.iter().enumerate() {
        let mut ks = Vec::with_capacity(orders.len());
        for o in orders {
            // 别名/列名/位置/表达式
            let v = match &o.expr {
                Expr::Identifier(id) => {
                    let low = id.value.to_ascii_lowercase();
                    match cols.get(&low) {
                        Some(&i) => row[i].clone(),
                        // 未投影列：回退到输入行（FROM 表原始行）
                        None => match (cols_in.as_ref(), input) {
                            (Some(ic), Some(tv)) => {
                                let in_row = &tv.rows[ri];
                                match ic.get(&low) {
                                    Some(&j) => in_row[j].clone(),
                                    None => expr::eval(&o.expr, in_row, &|n| {
                                        ic.get(&n.to_ascii_lowercase()).copied()
                                    })?,
                                }
                            }
                            _ => expr::eval(&o.expr, row, &|n| {
                                cols.get(&n.to_ascii_lowercase()).copied()
                            })?,
                        },
                    }
                }
                Expr::Value(vws) => {
                    if let PV::Number(n, _) = &vws.value {
                        let idx: usize = n
                            .parse()
                            .map_err(|_| SqlError::syntax("bad ORDER BY ordinal"))?;
                        row.get(idx - 1)
                            .cloned()
                            .ok_or_else(|| SqlError::syntax("ORDER BY out of range"))?
                    } else {
                        expr::eval(&o.expr, row, &|n| {
                            cols.get(&n.to_ascii_lowercase()).copied()
                        })?
                    }
                }
                e => {
                    if let (Some(ic), Some(tv)) = (cols_in.as_ref(), input) {
                        let in_row = &tv.rows[ri];
                        match expr::eval(e, in_row, &|n| ic.get(&n.to_ascii_lowercase()).copied()) {
                            Ok(v) => v,
                            Err(_) => {
                                expr::eval(e, row, &|n| cols.get(&n.to_ascii_lowercase()).copied())?
                            }
                        }
                    } else {
                        expr::eval(e, row, &|n| cols.get(&n.to_ascii_lowercase()).copied())?
                    }
                }
            };
            ks.push(v);
        }
        keys.push(ks);
    }
    // 排序（ Schwartzian：索引排序后重排）
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by(|&a, &b| {
        for (i, o) in orders.iter().enumerate() {
            let (x, y) = (&keys[a][i], &keys[b][i]);
            let ord = if x.is_null() && y.is_null() {
                Ordering::Equal
            } else if x.is_null() {
                Ordering::Greater // null 最后
            } else if y.is_null() {
                Ordering::Less
            } else {
                expr::cmp_values(x, y).unwrap_or(Ordering::Equal)
            };
            let ord = if o.options.asc.unwrap_or(true) {
                ord
            } else {
                ord.reverse()
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    let sorted: Vec<Vec<SqlValue>> = idx.into_iter().map(|i| rows[i].clone()).collect();
    *rows = sorted;
    Ok(())
}

// ---------- 伪表 ----------

fn pseudo_branches(db: &Database) -> Result<TableView> {
    let names = vec!["branch".into(), "commit".into(), "parent".into()];
    let mut rows = Vec::new();
    for (n, h) in db.manifest().manifest.refs.clone() {
        rows.push(vec![
            SqlValue::Utf8(n),
            SqlValue::Utf8(h.commit.unwrap_or_default()),
            h.parent.map(SqlValue::Utf8).unwrap_or(SqlValue::Null),
        ]);
    }
    Ok(TableView { names, rows })
}

fn pseudo_tables(db: &Database) -> Result<TableView> {
    let names = vec!["table_schema".into(), "table_name".into()];
    let mut rows = Vec::new();
    let branch = db.branch("main")?;
    let _ = branch;
    for sess_name in ["main"] {
        let b = db.branch(sess_name)?;
        let head = b.head.load_full();
        let catalog = crate::versioned::Versioned::new(db.store.clone());
        for (n, _) in catalog.catalog_entries(head.as_ref().as_ref().map(|c| c.root).as_ref())? {
            rows.push(vec![SqlValue::Utf8("public".into()), SqlValue::Utf8(n)]);
        }
    }
    Ok(TableView { names, rows })
}

fn pseudo_columns(db: &Database, sess: &mut Session) -> Result<TableView> {
    let names = vec![
        "table_schema".into(),
        "table_name".into(),
        "column_name".into(),
        "data_type".into(),
    ];
    let mut rows = Vec::new();
    let b = db.branch("main")?;
    let head = b.head.load_full();
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    for (n, _) in catalog.catalog_entries(head.as_ref().as_ref().map(|c| c.root).as_ref())? {
        let (schema, _) = resolve_table(db, &sess.branch, &n)?;
        for c in schema.columns {
            rows.push(vec![
                SqlValue::Utf8("public".into()),
                SqlValue::Utf8(n.clone()),
                SqlValue::Utf8(c.name),
                SqlValue::Utf8(c.ty.type_name().into()),
            ]);
        }
    }
    Ok(TableView { names, rows })
}

fn pseudo_commit_log(db: &Database, sess: &mut Session) -> Result<TableView> {
    let names = vec![
        "commit".into(),
        "height".into(),
        "ts_ms".into(),
        "message".into(),
    ];
    let mut rows = Vec::new();
    let b = db.branch(&sess.branch)?;
    let mut cur = b.head.load_full();
    let mut guard = 0;
    while let Some(c) = cur.as_ref() {
        rows.push(vec![
            SqlValue::Utf8(c.addr().to_base32()),
            SqlValue::Int64(c.height as i64),
            SqlValue::TimestampMs(c.ts_ms),
            SqlValue::Utf8(c.message.clone()),
        ]);
        guard += 1;
        if guard > 1000 || c.parents.is_empty() {
            break;
        }
        let (_ty, data) = db.cas.get(&c.parents[0])?;
        cur = Arc::new(Some(crate::versioned::commit::Commit::decode(&data)?));
    }
    Ok(TableView { names, rows })
}

// ---------- 输出 ----------

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

/// RecordSet 便捷构造
pub fn rows_to_record_set(columns: &[ColumnMeta], rows: Vec<Vec<SqlValue>>) -> RecordSet {
    RecordSet {
        columns: columns.to_vec(),
        batches: rows_to_batches_typed(columns, &rows),
    }
}
