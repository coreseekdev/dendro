#![allow(clippy::type_complexity)]
//! 查询执行：FROM 解析、快照扫描（prolly 树 ∪ memtx overlay）、过滤/投影/聚合/排序。

use super::agg;
use super::expr;
use crate::engine::{Database, Session};
use crate::error::{Result, SqlError};
use crate::types::{ColType, ColumnMeta, Output, RecordSet, SqlValue};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Query, Select, SelectItem, SetExpr, TableFactor,
    Value as PV,
};
use std::collections::HashMap;

// ---------- 阶段0 拆分（架构审视 §3.1）：子模块 + 再导出 ----------
// 纯移动零语义变更；子模块经 use super::* 互见，外部 scan::X 路径不变
mod cte;
mod history;
mod join;
mod plan_exec;
mod point;
mod project;
mod pseudo;
mod scan_table;
mod window;
pub(crate) use history::*;
pub(crate) use join::*;
pub(crate) use plan_exec::*;
pub(crate) use point::*;
pub use project::has_column_ref;
pub use project::rows_to_record_set;
pub(crate) use project::*;
pub(crate) use pseudo::*;
pub use scan_table::resolve_table;
pub use scan_table::rows_to_batches;
pub use scan_table::rows_to_batches_typed;
pub(crate) use scan_table::*;
pub(crate) use window::*;

/// 扫描出来的表视图
#[derive(Clone)]
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
    // 阶段5（架构审视 §3.1 阶段5）：唯一求值路径 = 计划。
    // AST 只做 parse → lowering（子查询内联）→ build_plan →
    // （optimize 开：6 重写族）→ exec_plan。旧 AST 装配全线退役：
    // eval_from join 链 / 窗口合成列段 / 聚合双路径段 / 投影排序
    // limit 出口 / 集合操作 AST 回落 / 递归 CTE VALUES 注入。
    // optimize 开关语义收窄为"重写族开关"（差分轴 on/off 对拍重写
    // 正确性；求值路径恒为计划）；force_source/force_agg 物理轴不变
    reject_offset_comma(q)?;

    // VALUES 叶子（无 FROM 常量行集——计划不适用，直评）
    if let SetExpr::Values(vals) = &*q.body {
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

    // lowering：非相关子查询内联（标量→常量 / IN→InList / EXISTS→
    // Bool；相关子查询诚实拒绝）；集合操作分支递归覆盖
    let mut q_owned = q.clone();
    lower_subqueries(db, sess, &mut q_owned.body, snapshot)?;

    let mut plan = crate::ir::plan::build_plan(&q_owned)?;
    if sess.optimize_enabled {
        crate::sql::optimize::rewrite_in_list(&mut plan);
        crate::ir::plan::rewrite_pushdown(&mut plan);
        crate::sql::optimize::rewrite_stat_prop(&mut plan, db, sess);
        crate::sql::optimize::rewrite_eq_copy(&mut plan);
        crate::sql::optimize::rewrite_join_order(&mut plan, db, sess);
        crate::sql::optimize::rewrite_filter_order(&mut plan);
    }
    // 结构侧安全网：不可执行节点集 = 诚实报错（无回落可吞）
    if !plan_nodes_exec_ok(&plan) {
        return Err(SqlError::not_supported(format!(
            "query shape not covered by plan path: {}",
            short_str(&q_owned.body)
        )));
    }
    let masks = plan_scan_masks(db, sess, &plan);
    let mut cx = ExecCx {
        masks: &masks,
        sort_hint: None,
        metrics: None,
        depth: 0,
        bindings: Default::default(),
    };
    let (tv, _) = exec_plan(db, sess, &plan, snapshot, &mut cx)?;
    Ok(tv)
}

/// lowering：Select 臂 WHERE/HAVING 子查询内联（SetExpr 递归——
/// 集合操作分支各自覆盖；逗号多因子诚实拒绝与旧 AST 入口同口径）
fn lower_subqueries(
    db: &Database,
    sess: &mut Session,
    se: &mut SetExpr,
    snapshot: u64,
) -> Result<()> {
    match se {
        SetExpr::Select(sel) => {
            reject_multi_from(sel)?;
            if let Some(w) = sel.selection.as_mut() {
                // L1/L2：WHERE 合取位的 InSubquery 保留在谓词——
                // 非相关由 build_select 落 SemiJoin（单次求值 + 哈希
                // 探测，免 InList 字面量物化）；其余合取项与相关
                // 透传（Filter 执行臂迭代求值）交由 inline 处理
                let conjuncts = crate::sql::optimize::flatten_and(w);
                let mut out: Vec<Expr> = Vec::with_capacity(conjuncts.len());
                for c in conjuncts {
                    if crate::sql::optimize::as_semi_candidate(&c).is_some() {
                        out.push(c);
                    } else {
                        let mut c = c;
                        crate::sql::optimize::inline_subqueries(db, sess, &mut c, snapshot, true)?;
                        out.push(c);
                    }
                }
                *w = crate::sql::optimize::fold_and(out)
                    .expect("flatten_and 对 Some 谓词必产出非空序列");
            }
            if let Some(h) = sel.having.as_mut() {
                // HAVING 位：聚合输出层无外层行上下文——相关即拒绝
                crate::sql::optimize::inline_subqueries(db, sess, h, snapshot, false)?;
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            lower_subqueries(db, sess, left, snapshot)?;
            lower_subqueries(db, sess, right, snapshot)?;
        }
        SetExpr::Query(inner) => {
            lower_subqueries(db, sess, &mut inner.body, snapshot)?;
        }
        _ => {}
    }
    Ok(())
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
        match crate::sql::scalar::compile_predicate_cached(w, resolve, tv.names.len(), &tv.names) {
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
pub fn resolve_qualified(layout: &FactorLayout, names: &[String], name: &str) -> Option<usize> {
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
        if matches!(lc, sqlparser::ast::LimitClause::OffsetCommaLimit { .. }) {
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
