#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! 投影与表达式辅助：project、投影列名/聚合识别、窗口调用收集（结构）、行→RecordSet。

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

/// 行集 first-seen 去重（SELECT DISTINCT / 保序；键 = 类型标签 + 值
/// debug 编码——组键同口径，防跨类型碰撞）
pub(crate) fn dedup_rows(rows: &mut Vec<Vec<SqlValue>>) {
    let mut seen = std::collections::HashSet::new();
    rows.retain(|r| {
        let key: String = r.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("\u{1}");
        seen.insert(key)
    });
}

pub(crate) fn project<R>(
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

pub(crate) fn projection_names(p: &[SelectItem], tv: &[String], calls: &[AggCall]) -> Result<Vec<String>> {
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
            // 窗口调用（sum() OVER(...)）不是全局聚合——每行出值，
            // 不触发聚合路径（P0 修：原被当聚合 → 全局塌缩 1 行）
            if f.over.is_some() {
                return false;
            }
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

pub(crate) fn collect_agg_calls(
    p: &[SelectItem],
    having: Option<&Expr>,
    _groups: &[Expr],
) -> Result<Vec<AggCall>> {
    let mut calls: Vec<AggCall> = Vec::new();
    fn visit(e: &Expr, calls: &mut Vec<AggCall>) {
        match e {
            Expr::Function(f) => {
                if f.over.is_some() {
                    return; // 窗口调用不是聚合（P0）
                }
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
pub(crate) fn agg_pipeline_plan(
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
pub(crate) fn order_key_value(
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

/// RecordSet 便捷构造
pub fn rows_to_record_set(columns: &[ColumnMeta], rows: Vec<Vec<SqlValue>>) -> RecordSet {
    RecordSet {
        columns: columns.to_vec(),
        batches: rows_to_batches_typed(columns, &rows),
    }
}

// ---------------------------------------------------------------------------
// 架构评审止血：窗口函数诚实拒绝 + FROM/LIMIT 显式拒绝
// ---------------------------------------------------------------------------
