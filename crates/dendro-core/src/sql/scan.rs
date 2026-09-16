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
    // S-4：UNION / UNION ALL（v1：两侧子查询独立求值 → 拼接；UNION
    // 额外按全行文本去重；列数须匹配，列名取左侧）
    let select = match set_expr {
        SetExpr::Select(s) => s.as_ref(),
        SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } => {
            use sqlparser::ast::{SetOperator, SetQuantifier};
            // O-2c+ A1：集合操作走计划路径（优化开 + 计划可建 + 节点
            // ⊆ 可执行集 + q 级无 OFFSET）；失败/不覆盖回落下方 AST 路径
            if sess.optimize_enabled {
                if let Ok(mut plan) = crate::ir::plan::build_plan(q) {
                    crate::ir::plan::rewrite_pushdown(&mut plan);
                    crate::sql::optimize::rewrite_join_order(&mut plan, db, sess);
                    let top_ok = matches!(
                        &plan,
                        crate::ir::plan::Plan::Sort { .. }
                            | crate::ir::plan::Plan::SetOp { .. }
                            | crate::ir::plan::Plan::Limit { .. }
                    );
                    if top_ok && plan_nodes_exec_ok(&plan) {
                        let masks = plan_scan_masks(db, sess, &plan);
                        let mut cx = ExecCx {
                            masks: &masks,
                            sort_hint: None,
                            metrics: None,
                            depth: 0,
                        };
                        let (tv, _) = exec_plan(db, sess, &plan, snapshot, &mut cx)?;
                        return Ok(tv);
                    }
                }
            }
            // 三算子统一（S-4 v2）：UNION / EXCEPT / INTERSECT
            let mk_query = |body: Box<SetExpr>| sqlparser::ast::Query {
                with: None,
                body,
                order_by: None,
                limit_clause: None,
                fetch: None,
                locks: Vec::new(),
                for_clause: None,
                settings: None,
                format_clause: None,
                pipe_operators: Vec::new(),
            };
            let lt = eval_query(db, sess, &mk_query(left.clone()), snapshot)?;
            let rt = eval_query(db, sess, &mk_query(right.clone()), snapshot)?;
            if lt.names.len() != rt.names.len() {
                return Err(SqlError::syntax(format!(
                    "set op: column count mismatch {}/{}",
                    lt.names.len(),
                    rt.names.len()
                )));
            }
            let row_key = |r: &Vec<SqlValue>| -> String {
                r.iter()
                    .map(|v| expr::to_text(v.clone()))
                    .collect::<Vec<_>>()
                    .join("\u{1}")
            };
            let right_keys: std::collections::HashSet<String> =
                rt.rows.iter().map(&row_key).collect();
            let all = matches!(
                set_quantifier,
                SetQuantifier::All | SetQuantifier::AllByName
            );
            let mut rows: Vec<Vec<SqlValue>> = match op {
                SetOperator::Union => {
                    let mut combined = lt.rows;
                    combined.extend(rt.rows);
                    if !all {
                        let mut seen = std::collections::HashSet::new();
                        combined.retain(|r| seen.insert(row_key(r)));
                    }
                    combined
                }
                SetOperator::Except => {
                    if all {
                        // EXCEPT ALL：多重集差——右删 min(count_l, count_r) 次
                        let mut right_counts: std::collections::HashMap<String, u64> =
                            std::collections::HashMap::new();
                        for r in &rt.rows {
                            *right_counts.entry(row_key(r)).or_insert(0) += 1;
                        }
                        lt.rows
                            .into_iter()
                            .filter(|r| {
                                let k = row_key(r);
                                match right_counts.get_mut(&k) {
                                    Some(c) if *c > 0 => {
                                        *c -= 1;
                                        false
                                    }
                                    _ => true,
                                }
                            })
                            .collect()
                    } else {
                        // EXCEPT（DISTINCT）：集差 + 去重
                        let mut seen = std::collections::HashSet::new();
                        lt.rows
                            .into_iter()
                            .filter(|r| {
                                let k = row_key(r);
                                !right_keys.contains(&k) && seen.insert(k)
                            })
                            .collect()
                    }
                }
                SetOperator::Intersect => {
                    if all {
                        // INTERSECT ALL：多重集交——每值 min(count_l, count_r) 份
                        // （限制输出计数 ≤ 右侧计数，与 EXCEPT ALL 对称）
                        let mut right_counts: std::collections::HashMap<String, u64> =
                            std::collections::HashMap::new();
                        for r in &rt.rows {
                            *right_counts.entry(row_key(r)).or_insert(0) += 1;
                        }
                        lt.rows
                            .into_iter()
                            .filter(|r| {
                                let k = row_key(r);
                                match right_counts.get_mut(&k) {
                                    Some(c) if *c > 0 => {
                                        *c -= 1;
                                        true
                                    }
                                    _ => false,
                                }
                            })
                            .collect()
                    } else {
                        // INTERSECT（DISTINCT）：集交 + 去重
                        let mut seen = std::collections::HashSet::new();
                        lt.rows
                            .into_iter()
                            .filter(|r| {
                                let k = row_key(r);
                                right_keys.contains(&k) && seen.insert(k)
                            })
                            .collect()
                    }
                }
                other => return Err(SqlError::not_supported(format!("set op: {other:?}"))),
            };
            // 外层 ORDER BY / LIMIT 统一应用到合并结果
            let order_exprs: &[sqlparser::ast::OrderByExpr] =
                match q.order_by.as_ref().map(|o| &o.kind) {
                    Some(sqlparser::ast::OrderByKind::Expressions(exprs)) => exprs,
                    _ => &[],
                };
            if !order_exprs.is_empty() {
                let asc: Vec<bool> = order_exprs
                    .iter()
                    .map(|o| o.options.asc.unwrap_or(true))
                    .collect();
                let cols = cols_lookup(&lt.names);
                let mut keyed: Vec<Vec<SqlValue>> = Vec::with_capacity(rows.len());
                for row in &rows {
                    let mut kr = Vec::with_capacity(asc.len() + row.len());
                    for o in order_exprs {
                        let v = match &o.expr {
                            sqlparser::ast::Expr::Identifier(id) => cols
                                .get(&id.value.to_ascii_lowercase())
                                .and_then(|&i| row.get(i).cloned())
                                .unwrap_or(SqlValue::Null),
                            sqlparser::ast::Expr::Value(vws) => {
                                if let sqlparser::ast::Value::Number(n, _) = &vws.value {
                                    let idx: usize = n.parse().unwrap_or(1);
                                    row.get(idx - 1).cloned().unwrap_or(SqlValue::Null)
                                } else {
                                    SqlValue::Null
                                }
                            }
                            _ => SqlValue::Null,
                        };
                        kr.push(v);
                    }
                    kr.extend(row.iter().cloned());
                    keyed.push(kr);
                }
                // O-5：LIMIT 已知时走 top-N 有界堆（内存上界 n 行；与全量
                // 排序取前缀逐字节一致——含并列稳定序；n = limit + offset，
                // 排序后 OFFSET/LIMIT 段照常跳过/截断）
                let topn: Option<usize> = match &q.limit_clause {
                    Some(sqlparser::ast::LimitClause::LimitOffset { limit, offset, .. }) => {
                        let l = limit
                            .as_ref()
                            .map(|e| eval_const(e).map(|v| v as usize))
                            .transpose()?;
                        let o = offset
                            .as_ref()
                            .map(|off| eval_const(&off.value).map(|v| v as usize))
                            .transpose()?
                            .unwrap_or(0);
                        l.map(|l| l + o)
                    }
                    _ => None,
                };
        let mut sort_op = match topn {
            Some(n) => crate::exec::pipeline::SortOp::with_limit(asc, n),
            None => crate::exec::pipeline::SortOp::new(asc),
        };
                let mut sink = crate::exec::pipeline::CollectSink::new(None);
                let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(keyed)];
                let mut it = src.into_iter();
                let mut pipe_cx = crate::exec::pipeline::PipeCtx::new(
                    vec![],
                    sess.stmt_deadline,
                    sess.cancel_token.clone(),
                );
                crate::exec::pipeline::drive(&mut pipe_cx, &mut it, &mut sort_op, &mut sink)?;
                rows = sink.rows;
            }
            if let Some(sqlparser::ast::LimitClause::LimitOffset { limit, offset, .. }) =
                &q.limit_clause
            {
                if let Some(off) = offset {
                    let n = eval_const(&off.value)? as usize;
                    rows = rows.into_iter().skip(n).collect();
                }
                if let Some(l) = limit {
                    let n = eval_const(l)? as usize;
                    rows.truncate(n);
                }
            }
            return Ok(TableView {
                names: lt.names,
                rows,
            });
        }
        other => {
            return Err(SqlError::not_supported(format!(
                "set op: {}",
                short_str(other)
            )))
        }
    };
    // DISTINCT（第二十一轮 R21-17 曾显式拒绝；现实现：投影后 first-seen
    // 去重、ORDER BY 前——键为类型标签 + 值的 debug 编码（与组键同口径，
    // 防 Int64(1)↔Utf8("1") 跨类型碰撞）
    let distinct = select
        .distinct
        .as_ref()
        .is_some_and(|d| matches!(d, sqlparser::ast::Distinct::Distinct));
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
    // O-2a/O-2c：计划 = 优化与执行的共同基底。build_plan 失败（派生表等）
    // → 不下推/不走计划执行，查询不受影响；优化关闭 = AST 路径（差分轴）
    let mut qplan: Option<crate::ir::plan::Plan> = if sess.optimize_enabled {
        crate::ir::plan::build_plan(q).ok()
    } else {
        None
    };
    let pushed_plan: Vec<(String, Vec<Expr>)> = match qplan.as_mut() {
        Some(p) => {
            let pushed = crate::ir::plan::rewrite_pushdown(p);
            // O-4'：INNER 链贪心重排（估算门控——无统计自动不动）
            crate::sql::optimize::rewrite_join_order(p, db, sess);
            pushed
        }
        None => vec![],
    };
    // O-2c 覆盖判定：无聚合/分组/HAVING/排序/LIMIT/通配投影，且计划节点
    // ⊆ {Scan, Filter, Join, Project} → 计划驱动执行（重写后的计划即
    // 执行序——EXPLAIN 计划块与执行逐节点对应）
    if let Some(p) = qplan.as_ref() {
        if plan_exec_covered(p, select, q) {
            let masks = plan_scan_masks(db, sess, p);
            let mut cx = ExecCx {
                masks: &masks,
                sort_hint: None,
                metrics: None,
                depth: 0,
            };
            let (mut tv, _layout) = exec_plan(db, sess, p, snapshot, &mut cx)?;
            if distinct {
                dedup_rows(&mut tv.rows);
            }
            return Ok(tv);
        }
    }
    let order_exprs: &[sqlparser::ast::OrderByExpr] =
        match q.order_by.as_ref().map(|o| &o.kind) {
            Some(sqlparser::ast::OrderByKind::Expressions(exprs)) => exprs,
            Some(sqlparser::ast::OrderByKind::All(_)) | None => &[],
        };
    let (mut tv, factor_layout) = eval_from(
        db,
        sess,
        select,
        snapshot,
        pushdown_limit,
        &pushed_plan,
        order_exprs,
    )?;
    // WHERE（v1 规则优化：常量折叠/布尔简化先于求值）
    let selection = select
        .selection
        .as_ref()
        .map(crate::sql::optimize::optimize);
    if let Some(w) = selection.as_ref() {
        // 常量短路（Q-1 优化器）：WHERE 表达式不含列引用时单次求值——
        // false/NULL → 跳过扫描直接返回空集（免全表遍历+行解码）；
        // true → 跳过过滤（恒真条件不需逐行判定）
        // 账本 #23：恒假短路仅在**无聚合**时返回空集——带全局聚合的查询
        // （count(*) 等）空输入仍须产出一行（零值聚合）。此前短路跳过了
        // 聚合阶段：WHERE 1=0 返回 0 行、而等价的逐行过滤路径返回 count=0，
        // 两条路径分叉。has_agg 判定前移（原在过滤后计算，现两处共用）。
        let has_agg = projection_aggregates(&select.projection).is_some()
            || select.having.as_ref().map(has_agg_expr).unwrap_or(false);
        if !has_column_ref(w) && !has_agg {
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
        // #28：join 后限定名按因子布局解析（o.id 不再错读右侧同名列）
        let lay = factor_layout.clone();
        let nms = tv.names.clone();
        let qres = move |n: &str| resolve_qualified(&lay, &nms, n);
        tv = apply_predicates_q(tv, w, sess, &qres)?;
    }
    // GROUP BY / 聚合 / HAVING（has_agg 已在常量短路判定前计算——#23）
    let group_exprs: Vec<Expr> = match &select.group_by {
        GroupByExpr::All(_) => return Err(SqlError::not_supported("GROUP BY ALL")),
        GroupByExpr::Expressions(e, _) => e.clone(),
    };
    let out_names: Vec<String>;
    let mut out_rows: Vec<Vec<SqlValue>>;
    if !group_exprs.is_empty() || has_agg {
        let calls = collect_agg_calls(&select.projection, select.having.as_ref(), &group_exprs)?;
        let cols = cols_lookup(&tv.names);
        // v2c-3：聚合执行路径选择——auto = 资格判定（纯列引用键/参数 →
        // AggOp 管线），force_agg 可强制（差分轴；强制不可行 → 报错，
        // 静默回落会让差分失义——force_source 同约定）
        let plan = agg_pipeline_plan(&group_exprs, &calls, &cols);
        let res = match (sess.force_agg, plan) {
            (Some(crate::sql::dispatch::AggPath::Row), _) | (None, None) => {
                agg::group_aggregate(&tv, &group_exprs, &calls, &cols)?
            }
            (Some(crate::sql::dispatch::AggPath::Pipeline), None) => {
                return Err(SqlError::syntax(
                    "cannot force pipeline: group/agg expressions are not plain column refs",
                ));
            }
            (_, Some((gidx, specs))) => {                let mut agg_op = crate::exec::pipeline::AggOp::new(gidx, specs);
                let mut sink = crate::exec::pipeline::CollectSink::new(None);
                let batches: Vec<Result<Vec<Vec<SqlValue>>>> = tv
                    .rows
                    .chunks(crate::exec::pipeline::ROW_BATCH)
                    .map(|ch| Ok(ch.to_vec()))
                    .collect();
                let mut it = batches.into_iter();
                let mut pcx = crate::exec::pipeline::PipeCtx::new(
                    vec![],
                    sess.stmt_deadline,
                    sess.cancel_token.clone(),
                );
                crate::exec::pipeline::drive(&mut pcx, &mut it, &mut agg_op, &mut sink)?;
                // AggOp 吐出 [组键 | 聚合值] 拼接行——拆回 keys/vals，
                // HAVING/投影段与行式路径共用（零分叉）
                let nk = group_exprs.len();
                let keys = sink.rows.iter().map(|r| r[..nk].to_vec()).collect();
                let vals = sink.rows.iter().map(|r| r[nk..].to_vec()).collect();
                agg::AggResult { keys, vals }
            }
        };
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
        // 投影（#28：限定名按因子布局解析）
        let lay = factor_layout.clone();
        let nms = tv.names.clone();
        let qres = move |n: &str| resolve_qualified(&lay, &nms, n);
        let (names, proj_rows) = project(&select.projection, &tv, sess, qres, Some(&factor_layout))?;
        out_names = names;
        out_rows = proj_rows;
    }
    if distinct {
        dedup_rows(&mut out_rows);
    }
    let order_exprs: &[OrderByExpr] = match q.order_by.as_ref().map(|o| &o.kind) {
        Some(sqlparser::ast::OrderByKind::Expressions(exprs)) => exprs,
        Some(sqlparser::ast::OrderByKind::All(_)) | None => &[],
    };
    if !order_exprs.is_empty() {
        let has_input = out_rows.len() == tv.rows.len();
        // v2c-2：SortOp 接线——键提取仍用 apply_order 逻辑（别名/序数/
        // 未投影列回退的复杂度不值得管线化），排序本体走 SortOp
        // （breaker 形态；与手写 sort_by 的差分在 operator_tests）
        let asc: Vec<bool> = order_exprs
            .iter()
            .map(|o| o.options.asc.unwrap_or(true))
            .collect();
        let cols = cols_lookup(&out_names);
        let cols_in = if has_input {
            Some(cols_lookup(&tv.names))
        } else {
            None
        };
        // 键提取（Schwartzian——apply_order 的键提取段原样搬运）
        let mut keyed_rows: Vec<Vec<SqlValue>> = Vec::with_capacity(out_rows.len());
        for (ri, row) in out_rows.iter().enumerate() {
            let mut kr = Vec::with_capacity(asc.len() + row.len());
            for o in order_exprs {
                let v = order_key_value(o, row, ri, &cols, &cols_in, &tv, has_input)?;
                kr.push(v);
            }
            kr.extend(row.iter().cloned());
            keyed_rows.push(kr);
        }
        // O-5：LIMIT 已知时走 top-N 有界堆（内存上界 n 行；结果与全量
        // 排序取前缀逐字节一致——含并列稳定序；n = limit + offset，
        // 排序后 OFFSET/LIMIT 段照常跳过/截断）
        let topn: Option<usize> = match &q.limit_clause {
            Some(sqlparser::ast::LimitClause::LimitOffset { limit, offset, .. }) => {
                let l = limit
                    .as_ref()
                    .map(|e| eval_const(e).map(|v| v as usize))
                    .transpose()?;
                let o = offset
                    .as_ref()
                    .map(|off| eval_const(&off.value).map(|v| v as usize))
                    .transpose()?
                    .unwrap_or(0);
                l.map(|l| l + o)
            }
            _ => None,
        };
        let mut sort_op = match topn {
            Some(n) => crate::exec::pipeline::SortOp::with_limit(asc, n),
            None => crate::exec::pipeline::SortOp::new(asc),
        };
        let mut sink = crate::exec::pipeline::CollectSink::new(None);
        let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(keyed_rows)];
        let mut it = src.into_iter();
        let mut pipe_cx = crate::exec::pipeline::PipeCtx::new(
            vec![],
            sess.stmt_deadline,
            sess.cancel_token.clone(),
        );
        crate::exec::pipeline::drive(&mut pipe_cx, &mut it, &mut sort_op, &mut sink)?;
        // SortOp.push 内部已 drain 键前缀（键-行分离），sink.rows 即纯数据行
        // ——**不得再 drain**（曾双重 drain 把数据列删空 → make_record_set
        // 空行越界 panic，merge_source 测试首跑即抓）
        out_rows = sink.rows;
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

/// 谓词过滤（WHERE 段原体，O-1 函数化供下推复用；求值错误 → 语句失败
/// 不静默吞）。
/// v2b B2：编译优先——谓词编译为 ScalarProgram 逐行步进（ir-spec 03）；
/// 编译失败（Function/TryCast/Substring 等未覆盖形态）整体回落 AST
/// 直评，行为与既有路径逐字节一致（B1 差分 + slt 护航）。
/// v2c-1b：谓词走 push 管线（协议 C3 + eval_chunk v1=C5）。
/// 单批 v1（tv.rows 本已物化，行移动零拷贝）；批粒度随流式 Source
/// 到来。语义与手写循环逐字节一致（eval_row + Qual 丢行 + 错误上抛），
/// 新增：每批 deadline/cancel 检查点（S-3 对齐）。编译失败回落 AST。
fn apply_predicates(
    tv: TableView,
    w: &Expr,
    sess: &Session,
) -> Result<TableView> {
    // 裸名解析（单因子/下推场景；join 后场景用 apply_predicates_q）
    let names = tv.names.clone();
    let cols = col_lookup(&names);
    apply_predicates_q(tv, w, sess, &cols)
}

/// 同 apply_predicates，携带调用方解析器（#28：join 后限定名按因子
/// 布局解析——两侧同名列不错读）
fn apply_predicates_q(
    mut tv: TableView,
    w: &Expr,
    sess: &Session,
    resolve: &dyn Fn(&str) -> Option<usize>,
) -> Result<TableView> {
    // cols 借用收敛在块内（闭包持有生命周期——外提会锁死结尾的 tv 移动）
    let rows: Vec<Vec<SqlValue>> = {
        match crate::sql::scalar::compile_predicate_named(
            w,
            resolve,
            tv.names.len(),
            &tv.names,
        ) {
            Ok(cp) if tv.rows.len() > 64 => {
                let mut cx = crate::exec::pipeline::PipeCtx::new(
                    vec![],
                    sess.stmt_deadline,
                    sess.cancel_token.clone(),
                );
                let all = std::mem::take(&mut tv.rows);
                let mut src = std::iter::once(Ok(all));
                let mut op = crate::exec::pipeline::FilterOp::new(cp.prog);
                let mut sink = crate::exec::pipeline::CollectSink::new(None);
                crate::exec::pipeline::drive(&mut cx, &mut src, &mut op, &mut sink)?;
                sink.rows
            }
            // 小结果集走紧循环（点查 1 行：管线包装的常数开销在 µs 级
            // 路径不可接受——bench 实证 315k→200k；两路径语义一致）
            Ok(cp) => {
                let mut filtered = Vec::with_capacity(tv.rows.len());
                for row in tv.rows.drain(..) {
                    let mut out = SqlValue::Null;
                    crate::sql::scalar::eval_row(&cp.prog, &row, &[], &mut out)?;
                    if matches!(out, SqlValue::Bool(true)) {
                        filtered.push(row);
                    }
                }
                filtered
            }
            Err(_) => {
                let mut filtered = Vec::with_capacity(tv.rows.len());
                for row in tv.rows.drain(..) {
                    if matches!(expr::eval(w, &row, resolve), Ok(SqlValue::Bool(true))) {
                        filtered.push(row);
                    }
                }
                filtered
            }
        }
    };
    tv.rows = rows;
    Ok(tv)
}

pub(crate) fn short_str_pub(s: &impl std::fmt::Display) -> String {
    short_str(s)
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

pub(crate) fn eval_const(v: &Expr) -> Result<i64> {
    let v = expr::eval(v, &[], &|_| None)?;
    expr::as_i64(&v)
}

fn col_lookup(names: &[String]) -> impl Fn(&str) -> Option<usize> + '_ {
    move |name: &str| {
        let low = name.to_ascii_lowercase();
        // 精确匹配（含限定名 e.id / 表别名.列名）
        if let Some(pos) = names.iter().position(|n| n.to_ascii_lowercase() == low) {
            return Some(pos);
        }
        // 限定名回退：`d.id` → 尝试匹配裸列名 `id`（JOIN 输出的 TableView
        // 列名不含表别名前缀——col_lookup 需消解限定名差异，否则 JOIN 后
        // WHERE 过滤列查找失败 → 行被静默丢弃）
        if let Some(dot) = low.rfind('.') {
            let bare = &low[dot + 1..];
            return names.iter().position(|n| {
                let nl = n.to_ascii_lowercase();
                nl == bare || nl.ends_with(bare)
            });
        }
        None
    }
}

fn cols_lookup(names: &[String]) -> HashMap<String, usize> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.to_ascii_lowercase(), i))
        .collect()
}

/// join 后的因子列布局（#28 修复）：因子键 → (列区间起点, 区间宽,
/// 因子内列名)。限定名 `o.id` 先按因子内解析，杜绝跨侧同名列错读
/// （原 cols_lookup HashMap 重复键 last-wins：o.id 读到右侧 id 列）。
pub type FactorLayout = Vec<(String, usize, usize, Vec<String>)>;

/// 限定名优先的列解析：`alias.col` / `table.col` → 因子区间内定位；
/// 裸名 → 全名空间首匹配（现状语义）。因子未命中回退全空间（派生表等）。
pub fn resolve_qualified(
    layout: &FactorLayout,
    names: &[String],
    name: &str,
) -> Option<usize> {
    let low = name.to_ascii_lowercase();
    if let Some((prefix, col)) = low.split_once('.') {
        if let Some((_, start, _, local)) = layout.iter().find(|(k, _, _, _)| *k == prefix) {
            // 因子内定位（start 已是该因子的绝对偏移；local 长度即区间宽）
            if let Some(i) = local.iter().position(|n| n.eq_ignore_ascii_case(col)) {
                return Some(start + i);
            }
            return None; // 前缀命中但列不在因子内：不回退（防跨侧误读）
        }
    }
    names.iter().position(|n| n.to_ascii_lowercase() == low)
}


// ---------- FROM ----------

fn eval_from(
    db: &Database,
    sess: &mut Session,
    select: &Select,
    snapshot: u64,
    pushdown_limit: Option<usize>,
    pushed: &[(String, Vec<Expr>)],
    order_exprs: &[sqlparser::ast::OrderByExpr],
) -> Result<(TableView, FactorLayout)> {
    let Some(twj) = select.from.first() else {
        // 无 FROM 常量投影（S 缺口，SELECT -3 / SELECT 1+1）：标准语义 =
        // 单行零列输入——投影/聚合（count(*) → 1）在此行上正常求值
        return Ok((
            TableView {
                names: vec![],
                rows: vec![vec![]],
            },
            FactorLayout::new(),
        ));
    };
    // O-3 投影裁剪：单表查询（无 join）计算列需求位图——AP 列存段
    // 对非需求列跳过解码（null 占位；optimize 关闭 = None 全解码）
    let col_mask_storage: Option<Vec<bool>> = if sess.optimize_enabled
        && select.from.len() == 1
        && twj.joins.is_empty()
    {
        if let TableFactor::Table { name, .. } = &twj.relation {
            let full = name
                .0
                .iter()
                .filter_map(|p| p.as_ident().map(|i| i.value.clone()))
                .collect::<Vec<_>>()
                .join(".");
            let short = full.rsplit('.').next().unwrap_or(&full).to_string();
            resolve_table(db, &sess.branch, &short)
                .ok()
                .and_then(|(schema, _)| {
                    crate::sql::optimize::column_mask(select, order_exprs, &schema)
                })
        } else {
            None
        }
    } else {
        None
    };
    let mut tv = table_scan_opt(
        db,
        sess,
        &twj.relation,
        snapshot,
        select.selection.as_ref(),
        pushdown_limit,
        col_mask_storage.as_deref(),
    )?;
    // O-1 R2：首因子的下推合取项（join 前过滤——加性，join 后 WHERE
    // 原样保留，语义合同见 spec 12 §2）
    if let Some(k) = crate::sql::optimize::factor_key(&twj.relation) {
        if let Some((_, cs)) = pushed.iter().find(|(key, _)| *key == k) {
            let combined = crate::sql::optimize::and_all(cs.clone());
            if let Some(w) = combined {
                tv = apply_predicates(tv, &w, sess)?;
            }
        }
    }
    // #28：因子列布局（限定名解析用——首个因子从 0 起）
    let mut layout: FactorLayout = Vec::new();
    if let Some(k) = crate::sql::optimize::factor_key(&twj.relation) {
        layout.push((
            k,
            0,
            tv.names.len(),
            tv.names.iter().map(|n| n.to_ascii_lowercase()).collect(),
        ));
    }
    for j in &twj.joins {
        match &j.join_operator {
            // sqlparser 0.62 区分裸 `JOIN`(Join) 与 `INNER JOIN`(Inner)、
            // 裸 `LEFT JOIN`(Left) 与 `LEFT OUTER JOIN`(LeftOuter)——语义相同
            JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => {
                let mut right =
                    table_scan_opt(db, sess, &j.relation, snapshot, None, None, None)?;
                right = apply_pushed(db_right_key(&j.relation), right, pushed, sess)?;
                push_layout(&mut layout, &j.relation, right.names.clone());
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
                // #30：AST 路径同样消歧（左侧已含多因子——限定名按布局；
                // 差分实证：3 链 `o.cid = c.id` 曾误中左表 r.id）
                tv = hash_join(
                    tv,
                    right,
                    l,
                    sess.stmt_deadline,
                    sess.optimize_enabled,
                    Some(&layout),
                    crate::sql::optimize::factor_key(&j.relation)
                        .as_deref(),
                )?;
            }
            JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
                let mut right =
                    table_scan_opt(db, sess, &j.relation, snapshot, None, None, None)?;
                // LEFT 右侧下推安全：NULL 扩展行仍被 join 后保留的原谓词
                // 过滤（NULL 判非真）——与全量右表 + 事后过滤结果一致
                right = apply_pushed(db_right_key(&j.relation), right, pushed, sess)?;
                push_layout(&mut layout, &j.relation, right.names.clone());
                let e = match constraint {
                    sqlparser::ast::JoinConstraint::On(e) => e,
                    _ => return Err(SqlError::not_supported("LEFT JOIN constraint")),
                };
                tv = hash_join_left(
                    tv,
                    right,
                    e,
                    sess.stmt_deadline,
                    Some(&layout),
                    crate::sql::optimize::factor_key(&j.relation).as_deref(),
                )?;
            }
            _other => return Err(SqlError::not_supported("join type")),
        }
    }
    Ok((tv, layout))
}

/// join 右因子的布局追加：区间起点 = 既有因子列宽和（join 拼接序），
/// 局部名列 = 右因子扫描输出列（join 前克隆——hash_join 按值消费）
fn push_layout(
    layout: &mut FactorLayout,
    tf: &sqlparser::ast::TableFactor,
    local: Vec<String>,
) {
    if let Some(k) = crate::sql::optimize::factor_key(tf) {
        let start: usize = layout.iter().map(|(_, _, l, _)| l).sum();
        layout.push((
            k,
            start,
            local.len(),
            local.iter().map(|n| n.to_ascii_lowercase()).collect(),
        ));
    }
}

/// join 右因子的下推应用（与首因子同构；key 来自因子别名/表名）
fn apply_pushed(
    key: Option<String>,
    tv: TableView,
    pushed: &[(String, Vec<Expr>)],
    sess: &Session,
) -> Result<TableView> {
    if let Some(k) = key {
        if let Some((_, cs)) = pushed.iter().find(|(key, _)| *key == k) {
            let combined = crate::sql::optimize::and_all(cs.clone());
            if let Some(w) = combined {
                return apply_predicates(tv, &w, sess);
            }
        }
    }
    Ok(tv)
}

fn db_right_key(tf: &sqlparser::ast::TableFactor) -> Option<String> {
    crate::sql::optimize::factor_key(tf)
}

// ---------------------------------------------------------------------------
// O-2c：计划驱动执行（覆盖形状 Scan/Filter/Join/Project——聚合/排序/
// LIMIT/集合操作由 eval_select 覆盖判定回落 AST 路径）
// ---------------------------------------------------------------------------

/// 聚合调用 display 串 → AggCall（O-2c+ A3：计划执行期结构化——
/// display 由 collect_agg_text 产生，形态 = 合法聚合表达式文本）
fn parse_plan_agg(display: &str) -> Result<crate::sql::agg::AggCall> {
    let e = crate::ir::plan::parse_expr_text_pub(display)
        .ok_or_else(|| SqlError::internal(format!("plan agg parse: {display}")))?;
    match e {
        Expr::Function(f) => {
            let n = f.name.to_string().to_ascii_lowercase();
            if !matches!(n.as_str(), "count" | "sum" | "avg" | "min" | "max") {
                return Err(SqlError::internal(format!("plan agg fn: {display}")));
            }
            let (arg, is_star) = match fn_args(&f).first() {
                Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => (Some(e.clone()), false),
                Some(FunctionArg::Unnamed(FunctionArgExpr::Wildcard)) => (None, true),
                _ => (None, false),
            };
            Ok(crate::sql::agg::AggCall {
                func: n,
                arg,
                distinct: fn_distinct(&f),
                is_star,
                display: display.to_string(),
            })
        }
        other => Err(SqlError::internal(format!(
            "plan agg not a function: {other}"
        ))),
    }
}

/// 组合聚合段执行（O-2c+ A3）：Aggregate → [HAVING Filter] → Project
/// 一体求值——镜像 eval_select 聚合分支（同 group_aggregate/eval_having/
/// display-文本映射语义）。返回投影结果。
fn exec_aggregate_composite(
    tv_in: &TableView,
    agg: &crate::ir::plan::Plan,
    having: Option<&Expr>,
    proj_exprs: &[Expr],
    proj_names: &[String],
) -> Result<TableView> {
    let crate::ir::plan::Plan::Aggregate { keys, aggs, .. } = agg else {
        return Err(SqlError::internal("aggregate composite: 非 Aggregate 节点"));
    };
    let calls: Vec<crate::sql::agg::AggCall> =
        aggs.iter().map(|d| parse_plan_agg(d)).collect::<Result<_>>()?;
    let cols = cols_lookup(&tv_in.names);
    let res = agg::group_aggregate(tv_in, keys, &calls, &cols)?;
    // HAVING 过滤（保留组索引）
    let mut kept: Vec<usize> = Vec::new();
    for i in 0..res.keys.len() {
        if let Some(h) = having {
            let hv = agg::eval_having(h, &calls, &res.vals[i], keys, &res.keys[i], &cols)?;
            if hv != SqlValue::Bool(true) {
                continue;
            }
        }
        kept.push(i);
    }
    // 投影映射：display 匹配聚合值 / 组键文本匹配（与 eval_select 同口径）
    let mut rows = Vec::with_capacity(kept.len());
    for &i in &kept {
        let mut row = Vec::with_capacity(proj_exprs.len());
        for e in proj_exprs {
            let et = e.to_string();
            if let Some(ci) = calls.iter().position(|c| c.display == et) {
                row.push(res.vals[i][ci].clone());
            } else if let Some(gi) = keys.iter().position(|g| g.to_string() == et) {
                row.push(res.keys[i][gi].clone());
            } else {
                return Err(SqlError::syntax(
                    "column must appear in GROUP BY or aggregation",
                ));
            }
        }
        rows.push(row);
    }
    Ok(TableView {
        names: proj_names.to_vec(),
        rows,
    })
}

/// 行集 first-seen 去重（SELECT DISTINCT / 保序；键 = 类型标签 + 值
/// debug 编码——组键同口径，防跨类型碰撞）
fn dedup_rows(rows: &mut Vec<Vec<SqlValue>>) {
    let mut seen = std::collections::HashSet::new();
    rows.retain(|r| {
        let key: String = r.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("\u{1}");
        seen.insert(key)
    });
}

/// 集合操作应用（O-2c+ 提取：eval_query 与 exec_plan 共享——防漂移）。
/// op ∈ {union, except, intersect}；all = UNION ALL/EXCEPT ALL/... 多重集
pub(crate) fn apply_setop(
    op: &str,
    all: bool,
    lt: TableView,
    rt: &TableView,
) -> Result<(Vec<String>, Vec<Vec<SqlValue>>)> {
    if lt.names.len() != rt.names.len() {
        return Err(SqlError::syntax(format!(
            "set op: column count mismatch {}/{}",
            lt.names.len(),
            rt.names.len()
        )));
    }
    let names = lt.names.clone();
    let row_key = |r: &Vec<SqlValue>| -> String {
        r.iter()
            .map(|v| expr::to_text(v.clone()))
            .collect::<Vec<_>>()
            .join("\u{1}")
    };
    let right_keys: std::collections::HashSet<String> = rt.rows.iter().map(&row_key).collect();
    let rows: Vec<Vec<SqlValue>> = match op {
        "union" => {
            let mut combined = lt.rows;
            combined.extend(rt.rows.iter().cloned());
            if !all {
                let mut seen = std::collections::HashSet::new();
                combined.retain(|r| seen.insert(row_key(r)));
            }
            combined
        }
        "except" => {
            if all {
                let mut right_counts: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                for r in &rt.rows {
                    *right_counts.entry(row_key(r)).or_insert(0) += 1;
                }
                lt.rows
                    .into_iter()
                    .filter(|r| {
                        let k = row_key(r);
                        match right_counts.get_mut(&k) {
                            Some(c) if *c > 0 => {
                                *c -= 1;
                                false
                            }
                            _ => true,
                        }
                    })
                    .collect()
            } else {
                let mut seen = std::collections::HashSet::new();
                lt.rows
                    .into_iter()
                    .filter(|r| {
                        let k = row_key(r);
                        !right_keys.contains(&k) && seen.insert(k)
                    })
                    .collect()
            }
        }
        "intersect" => {
            if all {
                let mut right_counts: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                for r in &rt.rows {
                    *right_counts.entry(row_key(r)).or_insert(0) += 1;
                }
                lt.rows
                    .into_iter()
                    .filter(|r| {
                        let k = row_key(r);
                        match right_counts.get_mut(&k) {
                            Some(c) if *c > 0 => {
                                *c -= 1;
                                true
                            }
                            _ => false,
                        }
                    })
                    .collect()
            } else {
                let mut seen = std::collections::HashSet::new();
                lt.rows
                    .into_iter()
                    .filter(|r| {
                        let k = row_key(r);
                        right_keys.contains(&k) && seen.insert(k)
                    })
                    .collect()
            }
        }
        other => return Err(SqlError::not_supported(format!("set op: {other}"))),
    };
    Ok((names, rows))
}

/// 计划路径的 ORDER BY 键提取（O-2c+ A2）：镜像 order_key_value 语义
/// ——输出列名/序数优先，未投影列回退输入行（投影保行：两侧行对齐）。
fn plan_order_key(
    e: &Expr,
    out_row: &[SqlValue],
    ri: usize,
    out_cols: &std::collections::HashMap<String, usize>,
    in_row: Option<&[SqlValue]>,
    in_cols: &Option<std::collections::HashMap<String, usize>>,
) -> Result<SqlValue> {
    match e {
        Expr::Identifier(id) => {
            let low = id.value.to_ascii_lowercase();
            if let Some(&i) = out_cols.get(&low) {
                return Ok(out_row.get(i).cloned().unwrap_or(SqlValue::Null));
            }
            if let (Some(ir), Some(ic)) = (in_row, in_cols) {
                if let Some(&i) = ic.get(&low) {
                    return Ok(ir.get(i).cloned().unwrap_or(SqlValue::Null));
                }
            }
            Ok(SqlValue::Null)
        }
        // 序数（ORDER BY 1）——输出列位置
        Expr::Value(vws) => match &vws.value {
            sqlparser::ast::Value::Number(n, _) => {
                let idx: usize = n.parse().unwrap_or(1);
                Ok(out_row.get(idx.wrapping_sub(1)).cloned().unwrap_or(SqlValue::Null))
            }
            _ => Ok(SqlValue::Null),
        },
        other => {
            // 一般表达式：输出行求值优先，失败回退输入行
            let oc = |n: &str| out_cols.get(&n.to_ascii_lowercase()).copied();
            if let Ok(v) = expr::eval(other, out_row, &oc) {
                return Ok(v);
            }
            if let (Some(ir), Some(ic)) = (in_row, in_cols) {
                let icf = |n: &str| ic.get(&n.to_ascii_lowercase()).copied();
                return expr::eval(other, ir, &icf);
            }
            let _ = ri;
            Ok(SqlValue::Null)
        }
    }
}

/// 计划路径排序（A2）：键提取 + SortOp（top-N 有界堆——limit 已知时）
fn plan_sort(
    keys: &[(Expr, bool)],
    limit: Option<usize>,
    out_rows: &mut Vec<Vec<SqlValue>>,
    in_rows: Option<&[Vec<SqlValue>]>,
    out_names: &[String],
    in_names: &[String],
    sess: &Session,
) -> Result<()> {
    let asc: Vec<bool> = keys.iter().map(|(_, a)| *a).collect();
    let out_cols = cols_lookup(out_names);
    let in_cols: Option<std::collections::HashMap<String, usize>> =
        in_rows.map(|_| cols_lookup(in_names));
    let mut keyed: Vec<Vec<SqlValue>> = Vec::with_capacity(out_rows.len());
    for (ri, row) in out_rows.iter().enumerate() {
        let ir = in_rows.and_then(|rs| rs.get(ri)).map(|v| v.as_slice());
        let mut kr = Vec::with_capacity(asc.len() + row.len());
        for (e, _) in keys {
            kr.push(plan_order_key(e, row, ri, &out_cols, ir, &in_cols)?);
        }
        kr.extend(row.iter().cloned());
        keyed.push(kr);
    }
    let mut sort_op = match limit {
        Some(n) => crate::exec::pipeline::SortOp::with_limit(asc, n),
        None => crate::exec::pipeline::SortOp::new(asc),
    };
    let mut sink = crate::exec::pipeline::CollectSink::new(None);
    let src: Vec<Result<Vec<Vec<SqlValue>>>> = vec![Ok(keyed)];
    let mut it = src.into_iter();
    let mut pcx = crate::exec::pipeline::PipeCtx::new(
        vec![],
        sess.stmt_deadline,
        sess.cancel_token.clone(),
    );
    crate::exec::pipeline::drive(&mut pcx, &mut it, &mut sort_op, &mut sink)?;
    *out_rows = sink.rows;
    Ok(())
}

/// 计划 Scan → 合成 TableFactor（视图展开/派发按名工作；布局键取计划）。
/// version（display 文本）经 sqlparser 重建——与表达式 Display→parse
/// 同稳定性合同（O-2c+ B：历史查询上计划路径）
fn synthetic_tf(table: &str, version: Option<&str>) -> TableFactor {
    let ver = version.and_then(|v| {
        let sql = format!("SELECT * FROM x {v}");
        let stmts = crate::sql::parse_batch(&sql, crate::sql::SqlDialect::Pg).ok()?;
        match stmts.into_iter().next()? {
            sqlparser::ast::Statement::Query(q) => match *q.body {
                sqlparser::ast::SetExpr::Select(sel) => {
                    let twj = sel.from.into_iter().next()?;
                    match twj.relation {
                        TableFactor::Table { version, .. } => version,
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        }
    });
    TableFactor::Table {
        name: sqlparser::ast::ObjectName(vec![sqlparser::ast::ObjectNamePart::Identifier(
            sqlparser::ast::Ident::new(table.to_string()),
        )]),
        alias: None,
        args: None,
        version: ver,
        partitions: vec![],
        with_hints: vec![],
        with_ordinality: false,
        sample: None,
        index_hints: vec![],
        json_path: None,
    }
}

/// EXPLAIN ANALYZE 接线口（mod.rs 跨模块）
pub(crate) fn plan_exec_covered_pub(
    plan: &crate::ir::plan::Plan,
    select: &Select,
    q: &Query,
) -> bool {
    plan_exec_covered(plan, select, q)
}

/// 同 plan_nodes_exec_ok（pub 口）
pub(crate) fn plan_nodes_exec_ok_pub(p: &crate::ir::plan::Plan) -> bool {
    plan_nodes_exec_ok(p)
}

/// exec_plan pub 口
pub(crate) fn exec_plan_pub(
    db: &Database,
    sess: &mut Session,
    plan: &crate::ir::plan::Plan,
    snapshot: u64,
    cx: &mut ExecCx<'_>,
) -> Result<(TableView, FactorLayout)> {
    exec_plan(db, sess, plan, snapshot, cx)
}

/// O-2c 覆盖判定：查询形态（AST 侧）+ 计划节点集（结构侧）双检
fn plan_exec_covered(
    plan: &crate::ir::plan::Plan,
    select: &Select,
    q: &Query,
) -> bool {
    use crate::ir::plan::Plan;
    // AST 形态：通配/OFFSET → AST 路径（DISTINCT 入口已拒；
    // Plan::Sort v1 不携带 OFFSET；A3 起聚合/分组/HAVING 走计划路径）
    // DISTINCT：去重在 eval_select 出口做（dedup→sort→limit 语义序），
    // 计划路径的 sort(top-N)/limit 先于出口去重会错序（[a,a,a,b] LIMIT 2
    // → 计划 [a] vs 正确 [a,b]）——含 sort/limit 的 DISTINCT 查询回落
    let has_distinct = select.distinct.is_some();
    let has_sort_or_limit = q.order_by.is_some() || q.limit_clause.is_some();
    if has_distinct && has_sort_or_limit {
        return false;
    }
    // LIMIT/OFFSET 由 Limit 节点承接（含 LIMIT-无-ORDER / OFFSET-only）
    // 集合操作查询：body 非 Select——只约束 q 级（排序/LIMIT 已成节点）
    if !matches!(&*q.body, sqlparser::ast::SetExpr::Select(_)) {
        // SetOp 形态：无 WHERE 级 select 检查——q 级 OFFSET 已排除
        if matches!(
            &q.limit_clause,
            Some(sqlparser::ast::LimitClause::LimitOffset { offset: Some(_), .. })
        ) {
            return false;
        }
    }
    // 通配：纯 Wildcard（全部项）→ 计划 wildcard 形态；
    // QualifiedWildcard（o.*——前缀过滤语义）与混合通配 → AST 路径
    //（曾把 o.* 当纯通配 → join 两列全出而非仅 o 列——评审 P0）
    let n_wild: usize = select
        .projection
        .iter()
        .filter(|item| matches!(item, sqlparser::ast::SelectItem::Wildcard(_)))
        .count();
    let n_qwild: usize = select
        .projection
        .iter()
        .filter(|item| {
            matches!(item, sqlparser::ast::SelectItem::QualifiedWildcard(..))
        })
        .count();
    if n_wild > 0 && n_wild != select.projection.len() {
        return false; // 混合通配
    }
    if n_qwild > 0 {
        return false; // QualifiedWildcard 全部回落（o.* 前缀过滤语义）
    }
    // 版本子句（FOR SYSTEM_TIME/AS OF）：O-2c+ B 起计划路径携带
    // version（synthetic_tf 重建——HistoryScan 路由不变）
    // 计划结构：节点 ⊆ 可执行集；顶层 Project（Select）或 Sort/SetOp
    match plan {
        Plan::Project { exprs, wildcard, .. } if !exprs.is_empty() || *wildcard => {
            plan_nodes_exec_ok(plan)
        }
        Plan::Sort { .. } | Plan::SetOp { .. } | Plan::Limit { .. } => plan_nodes_exec_ok(plan),
        _ => false,
    }
}

/// 计划节点可执行性（exec_plan 覆盖集；Aggregate 待 A3）
fn plan_nodes_exec_ok(p: &crate::ir::plan::Plan) -> bool {
    use crate::ir::plan::Plan;
    match p {
        Plan::Scan { .. } | Plan::Values => true,
        Plan::Filter { input, .. } | Plan::Project { input, .. } | Plan::Sort { input, .. } => {
            plan_nodes_exec_ok(input)
        }
        Plan::Limit { input, .. } => plan_nodes_exec_ok(input),
        Plan::Join { left, right, .. } | Plan::SetOp { left, right, .. } => {
            plan_nodes_exec_ok(left) && plan_nodes_exec_ok(right)
        }
        Plan::Aggregate { input, .. } => plan_nodes_exec_ok(input),
    }
}

/// 计划内全部表达式（Filter 谓词 / Project 投影 / Sort 键——掩码分析面）
fn plan_exprs(plan: &crate::ir::plan::Plan, out: &mut Vec<Expr>) {
    use crate::ir::plan::Plan;
    match plan {
        Plan::Filter { pred, input } => {
            out.push(pred.clone());
            plan_exprs(input, out);
        }
        Plan::Project { exprs, input, .. } => {
            out.extend(exprs.iter().cloned());
            plan_exprs(input, out);
        }
        Plan::Join { on, left, right, .. } => {
            out.push(on.clone());
            plan_exprs(left, out);
            plan_exprs(right, out);
        }
        Plan::Aggregate { keys, input, .. } => {
            out.extend(keys.iter().cloned());
            plan_exprs(input, out);
        }
        Plan::Sort { keys, input } => {
            out.extend(keys.iter().map(|(e, _)| e.clone()));
            plan_exprs(input, out);
        }
        Plan::Limit { input, .. } => plan_exprs(input, out),
        Plan::SetOp { left, right, .. } => {
            plan_exprs(left, out);
            plan_exprs(right, out);
        }
        Plan::Scan { .. } | Plan::Values => {}
    }
}

/// 各 scan 因子的列掩码（plan 版 column_mask：全计划表达式引用面走查）
pub(crate) fn plan_scan_masks(
    db: &Database,
    sess: &Session,
    plan: &crate::ir::plan::Plan,
) -> std::collections::HashMap<String, Vec<bool>> {
    use crate::ir::plan::Plan;
    let mut out = std::collections::HashMap::new();
    // 通配投影 = 全列需求（fail-open：不裁剪——裁掉列在 SELECT * 下
    // 变 null 即刻可见；prune 差分首跑即抓）
    fn has_wildcard(p: &Plan) -> bool {
        match p {
            Plan::Project { wildcard, input, .. } => *wildcard || has_wildcard(input),
            Plan::Filter { input, .. }
            | Plan::Aggregate { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. } => has_wildcard(input),
            Plan::Join { left, right, .. } | Plan::SetOp { left, right, .. } => {
                has_wildcard(left) || has_wildcard(right)
            }
            Plan::Scan { .. } | Plan::Values => false,
        }
    }
    if has_wildcard(plan) {
        return out;
    }
    let mut exprs = Vec::new();
    plan_exprs(plan, &mut exprs);
    let mut idents = Vec::new();
    for e in &exprs {
        crate::sql::optimize::expr_idents_pub(e, &mut idents);
    }
    fn scans_of(p: &Plan, out: &mut Vec<(String, String)>) {
        match p {
            Plan::Scan { table, alias, .. } => {
                out.push((alias.clone().unwrap_or_else(|| table.clone()), table.clone()))
            }
            Plan::Filter { input, .. } | Plan::Project { input, .. }
            | Plan::Aggregate { input, .. } | Plan::Sort { input, .. }
            | Plan::Limit { input, .. } => scans_of(input, out),
            Plan::Join { left, right, .. } | Plan::SetOp { left, right, .. } => {
                scans_of(left, out);
                scans_of(right, out);
            }
            Plan::Values => {}
        }
    }
    let mut scans = Vec::new();
    scans_of(plan, &mut scans);
    for (key, table) in scans {
        if out.contains_key(&key) {
            continue;
        }
        let Ok((schema, _)) = resolve_table(db, &sess.branch, &table) else {
            continue; // 解析失败 → 无掩码（全解码）
        };
        let ncols = schema.columns.len();
        let mut mask = vec![false; ncols];
        let mut any = false;
        for id in &idents {
            let bare = id.rsplit('.').next().unwrap_or(id);
            if let Some(i) = schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(bare))
            {
                mask[i] = true;
                any = true;
            }
        }
        for &pk in &schema.pk {
            if (pk as usize) < ncols {
                mask[pk as usize] = true;
                any = true;
            }
        }
        if any && mask.iter().any(|&b| !b) {
            out.insert(key, mask);
        }
    }
    out
}

/// 执行期节点指标（EXPLAIN ANALYZE——spec 09 §5.5 / 04 §2 D7 预留位）
#[derive(Debug, Clone)]
pub struct NodeMetric {
    /// 节点标签（scan t / filter / join inner / project / aggregate /
    /// sort / limit / setop union）
    pub label: String,
    /// 树深（0 = 顶层）——缩进呈现
    pub depth: usize,
    /// 该节点输出行数（实际）
    pub rows: usize,
    /// 子树墙钟（含子节点）
    pub elapsed_us: u128,
    /// 估算行数（列统计 × 选择率——仅 scan 节点；None = 无段统计）
    pub est: Option<u64>,
}

/// exec_plan 上下文（参数收敛：掩码/top-N 界/指标采集 + 递归深度）
pub struct ExecCx<'a> {
    pub masks: &'a std::collections::HashMap<String, Vec<bool>>,
    pub sort_hint: Option<usize>,
    /// None = 不采集（常规执行零开销）；Some = EXPLAIN ANALYZE
    ///（子节点经 as_deref_mut 线性共享——兄弟顺序复用同一向量）
    pub metrics: Option<&'a mut Vec<NodeMetric>>,
    pub depth: usize,
}

impl<'a> ExecCx<'a> {
    fn child(&mut self, hint: Option<usize>, deeper: bool) -> ExecCx<'_> {
        ExecCx {
            masks: self.masks,
            sort_hint: hint,
            metrics: self.metrics.as_deref_mut(),
            depth: self.depth + usize::from(deeper),
        }
    }
    /// 记录节点指标（采集开启时）
    fn record(&mut self, label: &str, rows: usize, t: std::time::Instant) {
        self.record_est(label, rows, t, None);
    }

    /// 同上，携带估算行数（scan 节点——列统计消费面）
    fn record_est(
        &mut self,
        label: &str,
        rows: usize,
        t: std::time::Instant,
        est: Option<u64>,
    ) {
        if let Some(ms) = self.metrics.as_mut() {
            ms.push(NodeMetric {
                label: label.to_string(),
                depth: self.depth,
                rows,
                elapsed_us: t.elapsed().as_micros(),
                est,
            });
        }
    }
}

/// 节点指标标签（EXPLAIN ANALYZE 呈现）
fn node_label(p: &crate::ir::plan::Plan) -> String {
    use crate::ir::plan::Plan;
    match p {
        Plan::Values => "values".into(),
        Plan::Scan { table, .. } => format!("scan {table}"),
        Plan::Filter { .. } => "filter".into(),
        Plan::Join { kind, .. } => format!("join {kind}"),
        Plan::Aggregate { .. } => "aggregate".into(),
        Plan::Project { wildcard, .. } => {
            if *wildcard {
                "project *".into()
            } else {
                "project".into()
            }
        }
        Plan::Sort { .. } => "sort".into(),
        Plan::Limit { .. } => "limit".into(),
        Plan::SetOp { op, .. } => format!("setop {op}"),
    }
}

/// 计划树求值（O-2c）：(结果, 因子布局)。Filter/投影的限定名按布局解析。
/// 包装层统一记录节点指标（采集开启时——子树墙钟 + 实际输出行数）
fn exec_plan(
    db: &Database,
    sess: &mut Session,
    plan: &crate::ir::plan::Plan,
    snapshot: u64,
    cx: &mut ExecCx<'_>,
) -> Result<(TableView, FactorLayout)> {
    let t0 = std::time::Instant::now();
    let label = node_label(plan);
    let r = exec_plan_inner(db, sess, plan, snapshot, cx)?;
    cx.record(&label, r.0.rows.len(), t0);
    Ok(r)
}

fn exec_plan_inner(
    db: &Database,
    sess: &mut Session,
    plan: &crate::ir::plan::Plan,
    snapshot: u64,
    cx: &mut ExecCx<'_>,
) -> Result<(TableView, FactorLayout)> {
    use crate::ir::plan::Plan;
    match plan {
        Plan::Values => Ok((
            TableView { names: vec![], rows: vec![vec![]] },
            FactorLayout::new(),
        )),
        Plan::Scan {
            table,
            alias,
            version,
        } => {
            let key = alias.clone().unwrap_or_else(|| table.to_ascii_lowercase());
            let tf = synthetic_tf(table, version.as_deref());
            let mask = cx.masks.get(&key);
            let tv = table_scan_opt(db, sess, &tf, snapshot, None, None, mask.map(|v| v.as_slice()))?;
            // est = 段统计总行数（无过滤；无段 → None）
            let est = cx.metrics.as_ref().and_then(|_| {
                crate::sql::stats::table_stats(db, sess, table)
                    .and_then(|st| st.cols.first().map(|c| c.rows))
            });
            if let Some(ms) = cx.metrics.as_mut() {
                ms.push(NodeMetric {
                    label: format!("scan {table}"),
                    depth: cx.depth,
                    rows: tv.rows.len(),
                    elapsed_us: 0,
                    est,
                });
            }
            let layout = FactorLayout::from([(
                key,
                0usize,
                tv.names.len(),
                tv.names.iter().map(|n| n.to_ascii_lowercase()).collect(),
            )]);
            Ok((tv, layout))
        }
        Plan::Filter { pred, input } => {
            // Filter 直接覆 Scan：谓词下传为 selection 提示（点查/派发
            // 判定恢复——下推后的计划把 pk 谓词留在了 Scan 紧上方）；
            // 提示不过滤行，apply_predicates_q 仍执行实际过滤
            if let Plan::Scan {
                table,
                alias,
                version,
            } = &**input
            {
                let key = alias.clone().unwrap_or_else(|| table.to_ascii_lowercase());
                let tf = synthetic_tf(table, version.as_deref());
                let mask = cx.masks.get(&key);
                let t_scan = std::time::Instant::now();
                let mut tv = table_scan_opt(
                    db,
                    sess,
                    &tf,
                    snapshot,
                    Some(pred),
                    None,
                    mask.map(|v| v.as_slice()),
                )?;
                // 捷径绕过 exec_plan(Scan) 包装——手动记 scan 指标
                //（行数 = 过滤前扫描输出；est = 列统计 × 范围选择率）
                let est = cx.metrics.as_ref().and_then(|_| {
                    let st = crate::sql::stats::table_stats(db, sess, table)?;
                    let total = st.cols.first()?.rows;
                    Some(crate::sql::stats::estimate_filter_rows(
                        &st, &tv.names, pred, total,
                    ))
                });
                cx.record_est(&format!("scan {table}"), tv.rows.len(), t_scan, est);
                let names: Vec<String> = tv.names.iter().map(|n| n.to_ascii_lowercase()).collect();
                let layout = FactorLayout::from([(key, 0usize, tv.names.len(), names)]);
                let lay = layout.clone();
                let nms = tv.names.clone();
                let qres = move |n: &str| resolve_qualified(&lay, &nms, n);
                tv = apply_predicates_q(tv, pred, sess, &qres)?;
                return Ok((tv, layout));
            }
            let (mut tv, layout) = exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
            let lay = layout.clone();
            let nms = tv.names.clone();
            let qres = move |n: &str| resolve_qualified(&lay, &nms, n);
            tv = apply_predicates_q(tv, pred, sess, &qres)?;
            Ok((tv, layout))
        }
        Plan::Join { kind, on, left, right } => {
            let (l, mut llayout) = exec_plan(db, sess, left, snapshot, &mut cx.child(None, true))?;
            let (r, rlayout) = exec_plan(db, sess, right, snapshot, &mut cx.child(None, true))?;
            let rstart = l.names.len();
            let tv = if *kind == "left" {
                let lkey = right_scan_key(right);
                hash_join_left(
                    l,
                    r,
                    on,
                    sess.stmt_deadline,
                    Some(&llayout),
                    lkey.as_deref(),
                )?
            } else {
                let rkey = right_scan_key(right);
                hash_join(
                    l,
                    r,
                    on,
                    sess.stmt_deadline,
                    sess.optimize_enabled,
                    Some(&llayout),
                    rkey.as_deref(),
                )?
            };
            // 布局拼接：右因子区间起点 = 左侧列宽（join 拼接序）
            for (k, _, len, local) in rlayout {
                llayout.push((k, rstart, len, local));
            }
            Ok((tv, llayout))
        }
        Plan::Project {
            exprs,
            names,
            wildcard,
            input,
        } => {
            if *wildcard {
                // 纯通配：输入透传（全列原名原行）
                let (tv, _) = exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
                return Ok((tv, FactorLayout::new()));
            }
            // A3 组合模式：Project{[Filter(HAVING)] Aggregate input}
            // ——聚合中间态（keys/vals/calls）不出组合段
            match &**input {
                Plan::Aggregate { input: a_in, .. } => {
                    // 聚合的**内层**输入（组合段吃掉 Aggregate 节点自身）
                    let t_agg = std::time::Instant::now();
                    let (tv_in, _) =
                        exec_plan(db, sess, a_in, snapshot, &mut cx.child(None, true))?;
                    let out = exec_aggregate_composite(&tv_in, input, None, exprs, names)?;
                    // 组合内联绕过 exec_plan(Aggregate) 包装——手动记
                    //（rows = HAVING 后组行；project 同值由包装层记）
                    cx.record("aggregate", out.rows.len(), t_agg);
                    return Ok((out, FactorLayout::new()));
                }
                Plan::Filter {
                    pred,
                    input: inner,
                } if matches!(&**inner, Plan::Aggregate { .. }) => {
                    let Plan::Aggregate { input: a_in, .. } = &**inner else {
                        unreachable!()
                    };
                    let t_agg = std::time::Instant::now();
                    let (tv_in, _) =
                        exec_plan(db, sess, a_in, snapshot, &mut cx.child(None, true))?;
                    let out =
                        exec_aggregate_composite(&tv_in, inner, Some(pred), exprs, names)?;
                    cx.record("aggregate", out.rows.len(), t_agg);
                    return Ok((out, FactorLayout::new()));
                }
                _ => {}
            }
            let (tv, layout) = exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
            let lay = layout.clone();
            let nms = tv.names.clone();
            let qres = move |n: &str| resolve_qualified(&lay, &nms, n);
            let out = project_exprs(exprs, names, &tv, sess, qres)?;
            Ok((out, FactorLayout::new()))
        }
        Plan::Sort { keys, input } => {
            // A2/A3：排序在投影之上。普通投影保留输入作键回退（投影保行
            // ——未投影列的 ORDER BY 键经输入行求值）；聚合组合形态的
            // 行不与输入对齐（组行）——经 Project 臂组合执行后无回退排序
            if let Plan::Project {
                exprs,
                names,
                wildcard,
                input: pin,
            } = &**input
            {
                let agg_composite = matches!(&**pin, Plan::Aggregate { .. })
                    || matches!(
                        &**pin,
                        Plan::Filter { input: fi, .. } if matches!(&**fi, Plan::Aggregate { .. })
                    );
                if agg_composite {
                    let (mut out, _) = exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
                    plan_sort(keys, cx.sort_hint, &mut out.rows, None, &out.names, &[], sess)?;
                    return Ok((out, FactorLayout::new()));
                }
                let (tv_in, layout) = exec_plan(db, sess, pin, snapshot, &mut cx.child(None, true))?;
                // wildcard 透传（键直接对输入列解析——无重投影层）
                if *wildcard {
                    let mut out = tv_in;
                    plan_sort(
                        keys,
                        cx.sort_hint,
                        &mut out.rows,
                        None,
                        &out.names,
                        &[],
                        sess,
                    )?;
                    return Ok((out, FactorLayout::new()));
                }
                let lay = layout.clone();
                let nms = tv_in.names.clone();
                let qres = move |n: &str| resolve_qualified(&lay, &nms, n);
                let mut out = project_exprs(exprs, names, &tv_in, sess, qres)?;
                plan_sort(
                    keys,
                    cx.sort_hint,
                    &mut out.rows,
                    Some(&tv_in.rows),
                    &out.names,
                    &tv_in.names,
                    sess,
                )?;
                return Ok((out, FactorLayout::new()));
            }
            // 非 Project 输入（集合操作顶等）：无输入回退
            let (mut tv, _layout) = exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
            plan_sort(keys, cx.sort_hint, &mut tv.rows, None, &tv.names, &[], sess)?;
            Ok((tv, FactorLayout::new()))
        }
        Plan::SetOp { op, all, left, right } => {
            // A1：两侧子计划求值 → 共享 apply_setop（与 eval_query 逐字节
            // 同语义）
            let (lt, _) = exec_plan(db, sess, left, snapshot, &mut cx.child(None, true))?;
            let (rt, _) = exec_plan(db, sess, right, snapshot, &mut cx.child(None, true))?;
            let (names, rows) = apply_setop(op, *all, lt, &rt)?;
            Ok((
                TableView { names, rows },
                FactorLayout::new(),
            ))
        }
        Plan::Limit {
            limit,
            offset,
            input,
        } => {
            // LIMIT/OFFSET 终结。input 为 Sort 时以 n = limit + offset 作
            // top-N 界下传（有界堆——与 AST 路径 topn 同口径）
            let hint = match (&**input, limit) {
                (Plan::Sort { .. }, Some(l)) => Some(l + offset),
                _ => None,
            };
            let t0 = std::time::Instant::now();
            let _ = t0;
            let (mut tv, _layout) = exec_plan(db, sess, input, snapshot, &mut cx.child(hint, false))?;
            if *offset > 0 {
                tv.rows = tv.rows.into_iter().skip(*offset).collect();
            }
            if let Some(n) = limit {
                tv.rows.truncate(*n);
            }
            Ok((tv, FactorLayout::new()))
        }
        // 聚合：组合模式之外的裸 Aggregate——不可达（防御性回落错误）
        other => Err(SqlError::internal(format!(
            "exec_plan: uncovered plan node {other:?}"
        ))),
    }
}

/// 右子树首个 scan 的因子键（rkey 消歧用；非 Scan/Filter{Scan} 叶
/// → None——历史末段回退行为）
fn right_scan_key(p: &crate::ir::plan::Plan) -> Option<String> {
    use crate::ir::plan::Plan;
    match p {
        Plan::Scan { table, alias, .. } => {
            Some(alias.clone().unwrap_or_else(|| table.to_ascii_lowercase()))
        }
        Plan::Filter { input, .. } => right_scan_key(input),
        _ => None,
    }
}

/// 表达式集投影（O-2c 计划路径；与 project() 同机制——ProjectOp 管线）
fn project_exprs<R>(
    exprs: &[Expr],
    names: &[String],
    tv: &TableView,
    sess: &Session,
    resolve: R,
) -> Result<TableView>
where
    R: Fn(&str) -> Option<usize> + Send + 'static,
{
    if exprs.is_empty() {
        return Ok(TableView {
            names: names.to_vec(),
            rows: vec![],
        });
    }
    let mut pop = crate::exec::pipeline::ProjectOp {
        exprs: exprs.to_vec(),
        cols: Box::new(resolve),
    };
    let mut sink = crate::exec::pipeline::CollectSink::new(None);
    let batches: Vec<Result<Vec<Vec<SqlValue>>>> = tv
        .rows
        .chunks(crate::exec::pipeline::ROW_BATCH)
        .map(|ch| Ok(ch.to_vec()))
        .collect();
    let mut it = batches.into_iter();
    let mut pcx = crate::exec::pipeline::PipeCtx::new(
        vec![],
        sess.stmt_deadline,
        sess.cancel_token.clone(),
    );
    crate::exec::pipeline::drive(&mut pcx, &mut it, &mut pop, &mut sink)?;
    Ok(TableView {
        names: names.to_vec(),
        rows: sink.rows,
    })
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

fn try_ap_scan(
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
pub(crate) fn try_pk_pushdown(
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
        // 账本 #20：资格判定必须与取键同拒 negated——NOT IN 在此放行则
        // 取键产出空集，点查静默变 0 行（附录 A 实证）。回落通用谓词路径。
        Expr::InList { expr, negated, .. } if !*negated => match expr.as_ref() {
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

pub(crate) fn row_from_bytes(schema: &crate::versioned::TableSchema, bytes: &[u8]) -> Result<Vec<SqlValue>> {
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
    build_select: bool,
    llay: Option<&FactorLayout>,
    rkey: Option<&str>,
) -> Result<TableView> {
    // 找等值条件 col_l = col_r（支持 AND 链中提取多个；#22 残留合取回收）
    let (eqs, residual) = extract_equi(on, &l.names, &r.names, llay, rkey)?;
    let mut names = l.names.clone();
    names.extend(r.names.clone());
    // 列名克隆进闭包持有——避免借用 names 阻碍结尾 TableView 移动；
    // #30 同族修：residual 限定名按布局消歧（原裸末段首匹配曾使
    // acc-vs-acc 残留 `a.x = b.y` 同名自比较恒真——join 条件失效）
    let owned = names.clone();
    let lay_owned = llay.cloned().unwrap_or_default();
    let res_cols = move |n: &str| resolve_qualified(&lay_owned, &owned, n);
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
    // O-4（spec 12 §4）：INNER join 构建侧按**实际基数**选择——join 时
    // 两侧均已扫描/下推过滤，行数是精确值；小侧建哈希表（内存 O(min)
    // 替代 O(right)，probe 侧流式）。输出行恒 left++right（列序与
    // 因子布局/#28 解析不变），仅产出序随 probe 侧变化——多重集恒等。
    // build_select=false（optimize off 差分轴）= 原固定建右行为。
    let build_left = build_select && l.rows.len() < r.rows.len();
    let (build_rows, build_idx, probe_rows, probe_idx) = if build_left {
        (&l.rows, &eqs.lidx, &r.rows, &eqs.ridx)
    } else {
        (&r.rows, &eqs.ridx, &l.rows, &eqs.lidx)
    };
    let mut hm: HashMap<Vec<String>, Vec<&Vec<SqlValue>>> = HashMap::new();
    let mut hj_chk = 0usize;
    for br in build_rows {
        hj_chk += 1;
        if hj_chk.is_multiple_of(4096) {
            if let Some(d) = deadline {
                if std::time::Instant::now() >= d {
                    return Err(SqlError::new("57014", "statement timeout"));
                }
            }
        }
        if let Some(key) = mkkey(br, build_idx) {
            hm.entry(key).or_default().push(br);
        }
    }
    let mut rows = Vec::new();
    for pr in probe_rows {
        let key = match mkkey(pr, probe_idx) {
            Some(k) => k,
            None => continue,
        };
        if let Some(matches) = hm.get(&key) {
            for br in matches {
                // 输出列序恒 left++right（build 侧决定拼装方向）
                let row: Vec<SqlValue> = if build_left {
                    let mut row: Vec<SqlValue> = (**br).clone();
                    row.extend(pr.iter().cloned());
                    row
                } else {
                    let mut row = pr.clone();
                    row.extend(br.iter().cloned());
                    row
                };
                // #22：残留合取逐候选对求值（INNER：不成立即丢弃）
                if !residual_holds(&residual, &row, &res_cols)? {
                    continue;
                }
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
    llay: Option<&FactorLayout>,
    rkey: Option<&str>,
) -> Result<TableView> {
    // #30 消歧与 INNER 同款（布局 + 右键——曾只修 INNER，LEFT 的多因子
    // 左限定名仍裸末段误中）
    let (eqs, residual) = extract_equi(on, &l.names, &r.names, llay, rkey)?;
    let mut names = l.names.clone();
    names.extend(r.names.clone());
    // 列名克隆进闭包持有——避免借用 names 阻碍结尾 TableView 移动
    //（LEFT 侧布局未接线——residual 裸名回退，与 AST 路径同口径）
    let owned = names.clone();
    let res_cols = col_lookup(&owned);
    // #21：类型标签键（与 INNER 版同型）——裸 to_text 曾使 NULL↔NULL、
    // Int64(1)↔Utf8("1") 碰撞；NULL 分量 = 永不匹配（SQL 等值 NULL 语义）
    let mkkey = |row: &Vec<SqlValue>, idx: &[usize]| -> Option<Vec<String>> {
        let mut k = Vec::with_capacity(idx.len());
        for &i in idx {
            let v = row.get(i)?;
            if v.is_null() {
                return None;
            }
            k.push(format!(
                "{}{}{}",
                v.type_name(),
                NUL_PLACEHOLDER,
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
    let null_right = vec![SqlValue::Null; r.names.len()];
    let mut rows = Vec::new();
    for lr in &l.rows {
        // 探侧 NULL 键 → 无匹配 → NULL 延展（LEFT 语义）
        let matched = match mkkey(lr, &eqs.lidx) {
            None => None,
            Some(k) => hm.get(&k),
        };
        match matched {
            Some(ms) => {
                let mut any = false;
                for rr in ms {
                    let mut row = lr.clone();
                    row.extend(rr.iter().cloned());
                    // #22：残留合取；全部候选不成立 → 仍按无匹配 NULL 延展
                    if !residual_holds(&residual, &row, &res_cols)? {
                        continue;
                    }
                    any = true;
                    rows.push(row);
                }
                if !any {
                    let mut row = lr.clone();
                    row.extend(null_right.iter().cloned());
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

/// 限定名消歧列定位（#30）：前缀必须命中本侧因子（左 = 布局区间
/// 精确解析；右 = 单因子键），命中即定位、列不在因子内不回退；前缀
/// 不属本侧 → None（**不得**裸末段回退——曾使 `c.id` 误中右表 o.id
/// / 左表 r.id，join 键错位——重排差分实证）
fn col_pos_lay(
    e: &Expr,
    names: &[String],
    layout: Option<&FactorLayout>,
    rkey: Option<&str>,
) -> Option<usize> {
    match e {
        Expr::Identifier(id) => {
            let low = id.value.to_ascii_lowercase();
            names.iter().position(|n| n.to_ascii_lowercase() == low)
        }
        Expr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                let prefix = parts[0].value.to_ascii_lowercase();
                let col = parts[1].value.to_ascii_lowercase();
                if let Some(lay) = layout {
                    if let Some((_, start, len, local)) =
                        lay.iter().find(|(k, _, _, _)| *k == prefix)
                    {
                        return local
                            .iter()
                            .position(|n| *n == col)
                            .map(|i| start + i)
                            .filter(|abs| *abs < start + len);
                    }
                }
                if let Some(rk) = rkey {
                    if prefix == rk {
                        return names
                            .iter()
                            .position(|n| n.to_ascii_lowercase() == col);
                    }
                    return None; // 前缀不属右侧因子
                }
            }
            // 无布局无键（历史调用面）——末段回退
            let last = parts.last()?.value.to_ascii_lowercase();
            names.iter().position(|n| n.to_ascii_lowercase() == last)
        }
        _ => None,
    }
}

struct EquiIdx {
    lidx: Vec<usize>,
    ridx: Vec<usize>,
}

/// 从 ON 条件提取 `l.c = r.c` 等值对（AND 链）
fn extract_equi(
    e: &Expr,
    ln: &[String],
    rn: &[String],
    llay: Option<&FactorLayout>,
    rkey: Option<&str>,
) -> Result<(EquiIdx, Vec<Expr>)> {
    let mut lidx = Vec::new();
    let mut ridx = Vec::new();
    let mut residual: Vec<Expr> = Vec::new();
    // 账本 #22：AND 链中非等值合取曾**静默丢弃**（子节点返回值被无视）。
    // 改为收集残留、逐候选对求值（LEFT：残留不成立=该对不匹配→NULL 延展）
    #[allow(clippy::too_many_arguments)] // 消歧上下文五元组——内聚于
    // 递归闭包不可拆（拆参结构反而增加跨闭包状态）
    fn walk(
        e: &Expr,
        ln: &[String],
        rn: &[String],
        li: &mut Vec<usize>,
        ri: &mut Vec<usize>,
        residual: &mut Vec<Expr>,
        llay: Option<&FactorLayout>,
        rkey: Option<&str>,
    ) -> Result<bool> {
        match e {
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::And,
                right,
            } => {
                // #22：不可提取的子式由**父节点**收集（子式自身只在叶子报
                // false）；AND 恒真当且仅当全部子式可提取
                let a = walk(left, ln, rn, li, ri, residual, llay, rkey)?;
                if !a {
                    residual.push((**left).clone());
                }
                let b = walk(right, ln, rn, li, ri, residual, llay, rkey)?;
                if !b {
                    residual.push((**right).clone());
                }
                Ok(a && b)
            }
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::Eq,
                right,
            } => {
                // 两边各解析出一列：一属左表一属右表（限定名按侧消歧：
                // 左侧布局区间 / 右侧单因子键——#30）
                let le = col_pos_lay(left, ln, llay, None);
                let re = col_pos_lay(right, rn, None, rkey);
                let lo = col_pos_lay(left, rn, None, rkey);
                let ro = col_pos_lay(right, ln, llay, None);
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
    let _ = walk(e, ln, rn, &mut lidx, &mut ridx, &mut residual, llay, rkey)?;
    // 顶层整体不可提取且无任何等值对 → 原错误语义（顶层非等值条件）
    if lidx.is_empty() {
        return Err(SqlError::not_supported(
            "JOIN ON: only equi-conditions supported",
        ));
    }
    Ok((EquiIdx { lidx, ridx }, residual))
}

/// 残留合取求值（#22）：组合行上求值，全部 Bool(true) 才匹配（终结
/// 语义与 WHERE 一致：仅 true 放行）
/// 键内分隔符（NUL 字节，类型名与文本之间——附录 A 键编码）
const NUL_PLACEHOLDER: &str = "\u{0}";

fn residual_holds(
    residual: &[Expr],
    combined: &[SqlValue],
    cols: &dyn Fn(&str) -> Option<usize>,
) -> Result<bool> {
    for e in residual {
        if !matches!(expr::eval(e, combined, cols), Ok(SqlValue::Bool(true))) {
            return Ok(false);
        }
    }
    Ok(true)
}

// ---------- 投影/聚合 ----------

fn project<R>(
    p: &[SelectItem],
    tv: &TableView,
    sess: &Session,
    resolve: R,
    layout: Option<&FactorLayout>,
) -> Result<(Vec<String>, Vec<Vec<SqlValue>>)>
where
    R: Fn(&str) -> Option<usize> + Send + 'static,
{
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
                // 布局过滤：前缀 = 因子键 → 该因子区间列（原 starts_with
                // 对裸列名恒不匹配 → 0 列——o.* 在 join 与单表下全坏）
                // sqlparser 0.62 的 QualifiedWildcard prefix Display 含
                // `.*`（"o.*"）——剥通配尾后匹配因子键
                let pre = prefix
                    .to_string()
                    .to_ascii_lowercase()
                    .trim_end_matches(".*")
                    .to_string();
                match layout {
                    Some(lay) => {
                        if let Some((_, start, len, _)) =
                            lay.iter().find(|(k, _, _, _)| *k == pre)
                        {
                            for i in *start..(*start + *len) {
                                if let Some(n) = tv.names.get(i) {
                                    names.push(n.clone());
                                    items.push((
                                        Expr::Identifier(sqlparser::ast::Ident::new(
                                            n.clone(),
                                        )),
                                        None,
                                    ));
                                }
                            }
                        }
                    }
                    None => {
                        // 无布局（单表）：前缀须匹配表键（alias 或表名）
                        // —— tv.names 全列即该表
                        // 保守：不做前缀匹配检查，直接全列（单表 o.* 与
                        // SELECT * 同义——符合直觉且零破坏）
                        for n in &tv.names {
                            names.push(n.clone());
                            items.push((
                                Expr::Identifier(sqlparser::ast::Ident::new(n.clone())),
                                None,
                            ));
                        }
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
    // v2c-3：ProjectOp 接线——装配（名字/表达式展开）在上，行变换走管线
    // （分批；原手写双层循环删除，与 ProjectOp 的差分见 agg_pipeline 测试）
    if items.is_empty() {
        return Ok((names, vec![]));
    }
    let exprs: Vec<Expr> = items.iter().map(|(e, _)| e.clone()).collect();
    let mut pop = crate::exec::pipeline::ProjectOp {
        exprs,
        // #28：限定名按因子布局解析（调用方注入；裸名回退全空间首匹配）
        cols: Box::new(move |n: &str| resolve(n)),
    };
    let mut sink = crate::exec::pipeline::CollectSink::new(None);
    let batches: Vec<Result<Vec<Vec<SqlValue>>>> = tv
        .rows
        .chunks(crate::exec::pipeline::ROW_BATCH)
        .map(|ch| Ok(ch.to_vec()))
        .collect();
    let mut it = batches.into_iter();
    let mut pcx = crate::exec::pipeline::PipeCtx::new(
        vec![],
        sess.stmt_deadline,
        sess.cancel_token.clone(),
    );
    crate::exec::pipeline::drive(&mut pcx, &mut it, &mut pop, &mut sink)?;
    Ok((names, sink.rows))
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

pub(crate) fn projection_aggregates(p: &[SelectItem]) -> Option<()> {
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

pub(crate) fn has_agg_expr(e: &Expr) -> bool {
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

/// v2c-3：AggOp 管线资格（05 §4 同源的装配层判定）：组键与聚合参数均为
/// 纯列引用 → 可管线化（列偏移预解析）；否则（表达式键/参数）走
/// group_aggregate 行式路径（等价性锚点）。
fn agg_pipeline_plan(
    group_exprs: &[Expr],
    calls: &[AggCall],
    cols: &std::collections::HashMap<String, usize>,
) -> Option<(
    Vec<usize>,
    Vec<crate::exec::pipeline::AggSpec>,
)> {
    fn col_idx(
        e: &Expr,
        cols: &std::collections::HashMap<String, usize>,
    ) -> Option<usize> {
        match e {
            Expr::Identifier(id) => cols.get(&id.value.to_ascii_lowercase()).copied(),
            Expr::Nested(i) => col_idx(i, cols),
            _ => None,
        }
    }
    let mut gidx = Vec::with_capacity(group_exprs.len());
    for g in group_exprs {
        gidx.push(col_idx(g, cols)?);
    }
    let mut specs = Vec::with_capacity(calls.len());
    for c in calls {
        use crate::exec::pipeline::{AggFunc, AggSpec};
        let func = match c.func.as_str() {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "avg" => AggFunc::Avg,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            _ => return None,
        };
        let arg_col = if c.is_star {
            None
        } else {
            Some(col_idx(c.arg.as_ref()?, cols)?)
        };
        specs.push(AggSpec {
            func,
            arg_col,
            distinct: c.distinct,
        });
    }
    Some((gidx, specs))
}

/// ORDER BY 键提取（apply_order 的键提取段函数化——逻辑零改动）：
/// 别名→投影列 / 序数 / 未投影列回退输入行 / 任意表达式
fn order_key_value(
    o: &OrderByExpr,
    row: &[SqlValue],
    ri: usize,
    cols: &std::collections::HashMap<String, usize>,
    cols_in: &Option<std::collections::HashMap<String, usize>>,
    tv: &TableView,
    has_input: bool,
) -> Result<SqlValue> {
    match &o.expr {
        Expr::Identifier(id) => {
            let low = id.value.to_ascii_lowercase();
            match cols.get(&low) {
                Some(&i) => Ok(row[i].clone()),
                None => match (cols_in.as_ref(), has_input) {
                    (Some(ic), true) => {
                        let in_row = &tv.rows[ri];
                        match ic.get(&low) {
                            Some(&j) => Ok(in_row[j].clone()),
                            None => expr::eval(&o.expr, in_row, &|n| {
                                ic.get(&n.to_ascii_lowercase()).copied()
                            }),
                        }
                    }
                    _ => expr::eval(&o.expr, row, &|n| {
                        cols.get(&n.to_ascii_lowercase()).copied()
                    }),
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
                    .ok_or_else(|| SqlError::syntax("ORDER BY out of range"))
            } else {
                expr::eval(&o.expr, row, &|n| {
                    cols.get(&n.to_ascii_lowercase()).copied()
                })
            }
        }
        e => {
            if let (Some(ic), true) = (cols_in.as_ref(), has_input) {
                let in_row = &tv.rows[ri];
                match expr::eval(e, in_row, &|n| ic.get(&n.to_ascii_lowercase()).copied()) {
                    Ok(v) => Ok(v),
                    Err(_) => expr::eval(e, row, &|n| cols.get(&n.to_ascii_lowercase()).copied()),
                }
            } else {
                expr::eval(e, row, &|n| cols.get(&n.to_ascii_lowercase()).copied())
            }
        }
    }
}

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
