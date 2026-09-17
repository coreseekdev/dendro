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
        let key: String = r
            .iter()
            .map(|v| format!("{v:?}"))
            .collect::<Vec<_>>()
            .join("\u{1}");
        seen.insert(key)
    });
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
        // 子查询形态：内层列引用与外层行无关，但**不可**被外层常量
        // 短路求值（expr::eval 不支持——按"有列引用"处理交正常过滤）
        Expr::Exists { .. } | Expr::InSubquery { .. } | Expr::Subquery(_) => true,
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

// ---------- ORDER ----------

/// v2c-3：AggOp 管线资格（05 §4 同源的装配层判定）：组键与聚合参数均为
/// 纯列引用 → 可管线化（列偏移预解析）；否则（表达式键/参数）走
/// group_aggregate 行式路径（等价性锚点）。
pub(crate) fn agg_pipeline_plan(
    group_exprs: &[Expr],
    calls: &[AggCall],
    cols: &std::collections::HashMap<String, usize>,
) -> Option<(Vec<usize>, Vec<crate::exec::pipeline::AggSpec>)> {
    fn col_idx(e: &Expr, cols: &std::collections::HashMap<String, usize>) -> Option<usize> {
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

pub(crate) fn fn_args_mut(f: &mut sqlparser::ast::Function) -> &mut [FunctionArg] {
    match &mut f.args {
        sqlparser::ast::FunctionArguments::List(l) => &mut l.args,
        _ => &mut [] as &mut [FunctionArg],
    }
}
