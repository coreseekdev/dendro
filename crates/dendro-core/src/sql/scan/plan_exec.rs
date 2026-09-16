#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! 计划路径执行：exec_plan 节点解释、聚合组合段、覆盖判定、计划级排序与指标。

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

/// sqlparser Function → AggCall（阶段1 IR 自足化：display 随结构携带，
/// 计划构建/round-trip/执行共用——parse_plan_agg 重解析删除）
pub(crate) fn agg_call_from_fn(
    display: &str,
    f: &sqlparser::ast::Function,
) -> Result<crate::sql::agg::AggCall> {
    let n = f.name.to_string().to_ascii_lowercase();
    if !matches!(n.as_str(), "count" | "sum" | "avg" | "min" | "max") {
        return Err(SqlError::internal(format!("plan agg fn: {display}")));
    }
    let (arg, is_star) = match fn_args(f).first() {
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => (Some(e.clone()), false),
        Some(FunctionArg::Unnamed(FunctionArgExpr::Wildcard)) => (None, true),
        _ => (None, false),
    };
    Ok(crate::sql::agg::AggCall {
        func: n,
        arg,
        distinct: fn_distinct(f),
        is_star,
        display: display.to_string(),
    })
}

/// display 串 → AggCall（计划方言 parser 专用——文本经 sqlparser
/// 回解析为 Function；非热路径）
pub(crate) fn agg_call_from_display(
    display: &str,
) -> Result<crate::sql::agg::AggCall> {
    let e = crate::ir::plan::parse_expr_text_pub(display)
        .ok_or_else(|| SqlError::internal(format!("plan agg parse: {display}")))?;
    match e {
        Expr::Function(f) => agg_call_from_fn(display, &f),
        other => Err(SqlError::internal(format!(
            "plan agg not a function: {other}"
        ))),
    }
}

/// 组合聚合段执行（O-2c+ A3）：Aggregate → [HAVING Filter] → Project
/// 一体求值——镜像 eval_select 聚合分支（同 group_aggregate/eval_having/
/// display-文本映射语义）。返回投影结果。
pub(crate) fn exec_aggregate_composite(
    tv_in: &TableView,
    agg: &crate::ir::plan::Plan,
    having: Option<&Expr>,
    proj_exprs: &[Expr],
    proj_names: &[String],
) -> Result<TableView> {
    let crate::ir::plan::Plan::Aggregate { keys, aggs, .. } = agg else {
        return Err(SqlError::internal("aggregate composite: 非 Aggregate 节点"));
    };
    // 阶段1：aggs 已结构化（AggCall）——重解析消失
    let calls: Vec<crate::sql::agg::AggCall> = aggs.clone();
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
pub(crate) fn plan_order_key(
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
pub(crate) fn plan_sort(
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
pub(crate) fn synthetic_tf(table: &str, version: Option<&str>) -> TableFactor {
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
pub(crate) fn plan_exec_covered(
    plan: &crate::ir::plan::Plan,
    select: &Select,
    q: &Query,
) -> bool {
    use crate::ir::plan::Plan;
    // 阶段2 翻转①：DISTINCT 与 sort/limit 共存不再回落——Plan::Distinct
    // 在 Project 之上、Sort/Limit 之下（dedup→sort→limit 语义序由节点
    // 层次直接表达；旧回落原因是计划无去重节点、去重在 eval 出口做）
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
    // 阶段2 翻转③：QualifiedWildcard（o.*）→ 计划 prefixes 形态；
    // 混合限定通配 build 失败（build_plan Err → 计划路径整体不适用，
    // AST 路径处理——此处无需形态检查）
    if n_wild > 0 && n_wild != select.projection.len() {
        return false; // 混合通配（*, x）
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
pub(crate) fn plan_nodes_exec_ok(p: &crate::ir::plan::Plan) -> bool {
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
        // 阶段2 翻转：Distinct/Window 入可执行集（实现 = AST 路径
        // 同款共享助手；差分轴对拍护航）
        Plan::Distinct { input } | Plan::Window { input, .. } => {
            plan_nodes_exec_ok(input)
        }
    }
}

/// 计划内全部表达式（Filter 谓词 / Project 投影 / Sort 键——掩码分析面）
pub(crate) fn plan_exprs(plan: &crate::ir::plan::Plan, out: &mut Vec<Expr>) {
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
        Plan::Distinct { input } => plan_exprs(input, out),
        Plan::Window {
            calls, input, ..
        } => {
            for c in calls {
                if let Some(a) = &c.arg {
                    out.push(a.clone());
                }
                out.extend(c.partition_by.iter().cloned());
                out.extend(c.order_by.iter().map(|(e, _)| e.clone()));
            }
            plan_exprs(input, out);
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
            Plan::Project {
                wildcard,
                prefixes,
                input,
                ..
            } => *wildcard || !prefixes.is_empty() || has_wildcard(input),
            Plan::Filter { input, .. }
            | Plan::Aggregate { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input }
            | Plan::Window { input, .. } => has_wildcard(input),
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
            | Plan::Limit { input, .. } | Plan::Distinct { input }
            | Plan::Window { input, .. } => scans_of(input, out),
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
pub(crate) fn node_label(p: &crate::ir::plan::Plan) -> String {
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
        Plan::Distinct { .. } => "distinct".into(),
        Plan::Window { .. } => "window".into(),
        Plan::SetOp { op, .. } => format!("setop {op}"),
    }
}

/// 计划树求值（O-2c）：(结果, 因子布局)。Filter/投影的限定名按布局解析。
/// 包装层统一记录节点指标（采集开启时——子树墙钟 + 实际输出行数）
pub(crate) fn exec_plan(
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

pub(crate) fn exec_plan_inner(
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
            let ver_s = version.as_ref().map(|v| v.display());
            let tf = synthetic_tf(table, ver_s.as_deref());
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
                let ver_s = version.as_ref().map(|v| v.display());
            let tf = synthetic_tf(table, ver_s.as_deref());
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
            prefixes,
            input,
        } => {
            if *wildcard {
                // 纯通配：输入透传（全列原名原行）
                let (tv, _) = exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
                return Ok((tv, FactorLayout::new()));
            }
            if !prefixes.is_empty() {
                // 限定通配（o.*, c.*）：按因子布局区间取列（阶段2 翻转③
                // ——前缀 = 因子键；区间外列丢弃）
                let (tv, layout) =
                    exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
                let mut names_out: Vec<String> = Vec::new();
                let mut idx: Vec<usize> = Vec::new();
                for pre in prefixes {
                    if let Some((_, start, len, _)) =
                        layout.iter().find(|(k, _, _, _)| *k == *pre)
                    {
                        for i in *start..(*start + *len) {
                            idx.push(i);
                            names_out.push(tv.names[i].clone());
                        }
                    }
                }
                let rows = tv
                    .rows
                    .into_iter()
                    .map(|r| idx.iter().map(|&i| r[i].clone()).collect())
                    .collect();
                return Ok((
                    TableView {
                        names: names_out,
                        rows,
                    },
                    FactorLayout::new(),
                ));
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
                ..
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
        Plan::Distinct { input } => {
            // 阶段1 接线（阶段2 翻转覆盖）：实现体 = AST 路径同款
            // dedup_rows——共享助手，零新语义
            let (mut tv, layout) =
                exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
            dedup_rows(&mut tv.rows);
            Ok((tv, layout))
        }
        Plan::Window { calls, input } => {
            // 阶段1 接线（阶段2 翻转覆盖）：合成列求值 = AST 路径同款
            // eval_windows——共享助手，零新语义
            let (mut tv, layout) =
                exec_plan(db, sess, input, snapshot, &mut cx.child(None, true))?;
            let cols = cols_lookup(&tv.names);
            super::window::eval_windows(&mut tv, calls, &cols)?;
            Ok((tv, layout))
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
pub(crate) fn right_scan_key(p: &crate::ir::plan::Plan) -> Option<String> {
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
pub(crate) fn project_exprs<R>(
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
