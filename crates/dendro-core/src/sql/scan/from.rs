#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! FROM 子句求值：首因子扫描 + join 链装配 + 下推合取项回灌。

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

pub(crate) fn eval_from(
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
pub(crate) fn push_layout(
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
pub(crate) fn apply_pushed(
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

pub(crate) fn db_right_key(tf: &sqlparser::ast::TableFactor) -> Option<String> {
    crate::sql::optimize::factor_key(tf)
}

// ---------------------------------------------------------------------------
// O-2c：计划驱动执行（覆盖形状 Scan/Filter/Join/Project——聚合/排序/
// LIMIT/集合操作由 eval_select 覆盖判定回落 AST 路径）
// ---------------------------------------------------------------------------
