#![allow(clippy::type_complexity)]
//! 查询执行：FROM 解析、快照扫描（prolly 树 ∪ memtx overlay）、过滤/投影/聚合/排序。

use super::agg;
use super::expr;
use crate::engine::{Database, Session};
use crate::error::{Result, SqlError};
use crate::types::{ColType, ColumnMeta, Output, RecordSet, SqlValue};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, GroupByExpr, OrderByExpr, Query,
    Select, SelectItem, SetExpr, TableFactor, Value as PV,
};
use std::collections::HashMap;


// ---------- 阶段0 拆分（架构审视 §3.1）：子模块 + 再导出 ----------
// 纯移动零语义变更；子模块经 use super::* 互见，外部 scan::X 路径不变
mod from;
mod plan_exec;
mod project;
mod scan_table;
mod point;
mod history;
mod join;
mod pseudo;
mod window;
mod cte;
pub use project::rows_to_record_set;
pub use project::has_column_ref;
pub use scan_table::resolve_table;
pub use scan_table::rows_to_batches;
pub use scan_table::rows_to_batches_typed;
pub(crate) use from::*;
pub(crate) use plan_exec::*;
pub(crate) use project::*;
pub(crate) use scan_table::*;
pub(crate) use point::*;
pub(crate) use history::*;
pub(crate) use join::*;
pub(crate) use pseudo::*;
pub(crate) use window::*;
pub(crate) use cte::*;

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
    // 窗口函数在 eval_select 投影段处理（真实现——不再拒绝）

    reject_offset_comma(q)?;
    // P0：CTE 展开——非递归走 expand_ctes（纯函数）；递归走迭代不动点
    //（需本函数的求值上下文 db/sess/snapshot——optimize 纯函数不可达）
    let q_expanded;
    let q: &Query = if let Some(with) = &q.with {
        if with.recursive {
            q_expanded = eval_recursive_cte(db, sess, q, with, snapshot)?;
        } else {
            q_expanded = crate::sql::optimize::expand_ctes(q)?;
        }
        &q_expanded
    } else {
        q
    };
    let set_expr = q.body.as_ref();
    // S-4：UNION / UNION ALL（v1：两侧子查询独立求值 → 拼接；UNION
    // 额外按全行文本去重；列数须匹配，列名取左侧）
    // P0：VALUES 直接求值（递归 CTE 的迭代轮回 Derived 需要）
    if let SetExpr::Values(vals) = set_expr {
        let mut rows = Vec::new();
        for parens in &vals.rows {
            let mut row = Vec::new();
            for e in &parens.content {
                row.push(expr::eval(e, &[], &|_| None)?);
            }
            rows.push(row);
        }
        let n = rows.first().map(|r| r.len()).unwrap_or(0);
        let names: Vec<String> = (0..n).map(|i| format!("column{}", i + 1)).collect();
        return Ok(TableView { names, rows });
    }
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
                    crate::sql::optimize::rewrite_in_list(&mut plan);
                    crate::ir::plan::rewrite_pushdown(&mut plan);
                    crate::sql::optimize::rewrite_stat_prop(&mut plan, db, sess);
                    crate::sql::optimize::rewrite_eq_copy(&mut plan);
                    crate::sql::optimize::rewrite_join_order(&mut plan, db, sess);
                    crate::sql::optimize::rewrite_filter_order(&mut plan);
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
    // P0：子查询内联（非相关 → 常量/InList/Bool）——WHERE / HAVING 中
    // 的标量子查询、IN 子查询、EXISTS 在计划构建前展开（PG SubLink→
    // InitPlan 同构——子查询一次求值后内联替换）。select 不可变 →
    // clone 谓词 → 内联 → 后续路径用内联后版本
    let selection_inlined: Option<Expr> = match &select.selection {
        Some(w) => {
            let mut w2 = w.clone();
            crate::sql::optimize::inline_subqueries(db, sess, &mut w2, snapshot)?;
            Some(w2)
        }
        None => None,
    };
    // 同时 shadow select 和 q——build_plan(q) 与 eval_from 的
    // select.selection 都必须看到内联后版本（原只 shadow select
    // → build_plan 用原始 q 的子查询 WHERE → expr::eval 不支持
    // → Err → 行被过滤 → count=0——差分首跑即抓）
    let mut select_owned: Select;
    let select: &Select = if selection_inlined.is_some() {
        select_owned = select.clone();
        select_owned.selection = selection_inlined.clone();
        &select_owned
    } else {
        select
    };
    let mut q_owned: Query;
    let q: &Query = if selection_inlined.is_some() {
        q_owned = q.clone();
        if let sqlparser::ast::SetExpr::Select(sel) = &mut *q_owned.body {
            sel.selection = selection_inlined;
        }
        &q_owned
    } else {
        q
    };
    // HAVING 子查询内联（独立处理——select 引用重定向后仍需处理）
    // v1：HAVING 子查询走 expr eval not_supported（量少，诚实拒绝）
    // P0-5：逗号多因子 FROM 在计划构建前拒绝（build_plan 只取
    // from.first()——计划路径会静默丢 t2+；AST 路径 eval_from 同病）
    reject_multi_from(select)?;

    // O-2a/O-2c：计划 = 优化与执行的共同基底。build_plan 失败（派生表等）
    // → 不下推/不走计划执行，查询不受影响；优化关闭 = AST 路径（差分轴）
    let mut qplan: Option<crate::ir::plan::Plan> = if sess.optimize_enabled {
        crate::ir::plan::build_plan(q).ok()
    } else {
        None
    };
    let pushed_plan: Vec<(String, Vec<Expr>)> = match qplan.as_mut() {
        Some(p) => {
            // IN 重写在 pushdown 前（单值 IN → = 可触发点查下推）；
            // filter 重排在最后（eq_copy/stat_prop 注入后按代价排序）
            crate::sql::optimize::rewrite_in_list(p);
            let pushed = crate::ir::plan::rewrite_pushdown(p);
            crate::sql::optimize::rewrite_stat_prop(p, db, sess);
            crate::sql::optimize::rewrite_eq_copy(p);
            crate::sql::optimize::rewrite_join_order(p, db, sess);
            crate::sql::optimize::rewrite_filter_order(p);
            pushed
        }
        None => vec![],
    };
    // O-2c 覆盖判定：无聚合/分组/HAVING/排序/LIMIT/通配投影，且计划节点
    // ⊆ {Scan, Filter, Join, Project} → 计划驱动执行（重写后的计划即
    // 执行序——EXPLAIN 计划块与执行逐节点对应）
    if let Some(p) = qplan.as_ref() {
        // 窗口查询不走计划路径（Plan IR 无窗口节点——eval_select 的
        // 合成列路径处理；计划路径的 expr::eval 会报 "function sum"）
        let has_window = select.projection.iter().any(|item| {
            matches!(item,
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias { expr: Expr::Function(f), .. }
                if f.over.is_some())
        });
        if !has_window && plan_exec_covered(p, select, q) {
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
    // P0：窗口函数（全输入行上下文——在聚合/投影前求值）
    // has_agg 判定需排除窗口调用（窗口 sum() 不是全局聚合——每行出值）
    let window_calls = collect_window_calls(select)?;
    if !window_calls.is_empty() {
        // 合成列追加到 tv（tv 在 eval_from 后、聚合前）
        let wcols = cols_lookup(&tv.names);
        eval_windows(&mut tv, &window_calls, &wcols)?;
        // 投影重写：窗口调用 → Identifier(合成列名)
        // + has_agg 排除窗口
        // 重建投影（简单方案：包装 SelectItem 走 Identifier）
        // v1：直接在投影段引用合成列
        let has_window = !window_calls.is_empty();
        // 覆盖判定：含窗口的查询走 AST 路径（计划 IR 未覆盖窗口）
        // → 强制回落（通过设置 has_window 使 plan_exec_covered 返回 false
        //   的效果——此处直接 return AST 路径的后续代码）
        // 最简：设置一个 flag 使下方不走计划路径
        let select_with_window = true;
        // 投影阶段：window call expr 替换为 Identifier(synth_col)
        let mut patched_projection: Vec<SelectItem> = Vec::new();
        let mut wc_idx = 0usize;
        for item in select.projection.iter() {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f)) if f.over.is_some() => {
                    let synth = window_calls
                        .get(wc_idx)
                        .map(|w| w.synth_col.clone())
                        .unwrap_or_default();
                    wc_idx += 1;
                    patched_projection.push(SelectItem::ExprWithAlias {
                        expr: Expr::Identifier(sqlparser::ast::Ident::new(synth)),
                        alias: sqlparser::ast::Ident::new(f.to_string()),
                    });
                }
                _ => patched_projection.push(item.clone()),
            }
        }
        // 使用 patched 投影 + 无聚合路径（窗口已算完，synth 列可当普通列引用）
        let mut select_owned2: Select;
        let select: &Select = {
            select_owned2 = select.clone();
            // 窗口查询不走聚合路径——重置 group/having
            // v1：窗口 + GROUP BY 不支持
            if !matches!(&select_owned2.group_by, GroupByExpr::Expressions(es, _) if es.is_empty())
                || select_owned2.having.is_some()
            {
                return Err(SqlError::not_supported(
                    "window function with GROUP BY / HAVING (v1)",
                ));
            }
            select_owned2.projection = patched_projection;
            select_owned2.group_by = GroupByExpr::Expressions(vec![], vec![]);
            select_owned2.having = None;
            &select_owned2
        };
        let _ = (has_window, select_with_window);
        // 继续走正常投影路径（窗口列作为普通列）
        let wnames = tv.names.clone();
        let wres = move |n: &str| wnames.iter().position(|c| c.eq_ignore_ascii_case(n));
        let (names, proj_rows) = project(&select.projection, &tv, sess, wres, None)?;
        return Ok(TableView { names, rows: proj_rows });
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
                // P0-2 修（架构评审）：原 matches! 吞 Err → 静默丢行
                //（count=0 假象——apply_predicates_q 是子查询 bug 的
                // 最后放大器）。改为首个 Err 上抛——"求值错误 → 语句
                // 失败 不静默吞"的文档承诺在回退分支同样成立
                let mut filtered = Vec::with_capacity(tv.rows.len());
                for row in tv.rows.drain(..) {
                    match expr::eval(w, &row, resolve) {
                        Ok(SqlValue::Bool(true)) => filtered.push(row),
                        Ok(_) => {} // NULL/false → 丢行（正确语义）
                        Err(e) => return Err(e),
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

/// 窗口函数检测（止血——防 `sum() OVER()` 被当普通聚合静默塌缩）。
/// 遍历 Select 的投影/HAVING/ORDER BY，找 `Expr::Function` 带 `over`
/// 逗号多因子 FROM 显式拒绝（P0-5：原静默丢 t2+——`FROM t1,t2` ≡
/// `FROM t1`，评审确认 eval_from/build_select 均只取 first()）
pub(crate) fn reject_multi_from(select: &Select) -> Result<()> {
    if select.from.len() > 1 {
        return Err(SqlError::not_supported(format!(
            "comma-separated FROM ({} factors) — use explicit JOIN",
            select.from.len()
        )));
    }
    Ok(())
}

/// MySQL `LIMIT offset, count` 变体显式拒绝（P0-4：build_plan 只 match
/// LimitOffset，OffsetCommaLimit 静默无 Limit 节点 → MySQL 分页返回全行）
pub(crate) fn reject_offset_comma(q: &Query) -> Result<()> {
    if let Some(lc) = &q.limit_clause {
        if matches!(
            lc,
            sqlparser::ast::LimitClause::OffsetCommaLimit { .. }
        ) {
            return Err(SqlError::not_supported(
                "LIMIT offset, count (MySQL comma syntax) — use LIMIT n OFFSET m",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// P0：窗口函数实现（OVER PARTITION BY / ORDER BY + 排名 + 裸聚合）
// 位置：eval_select 投影前（全输入行上下文——合成列 + 投影重写）
// v1 支持：row_number/rank/dense_rank + sum/count/min/max/avg OVER
// v1 拒绝：named window / window frame / LAG/LEAD（OFFSET 表达式）
// ---------------------------------------------------------------------------
