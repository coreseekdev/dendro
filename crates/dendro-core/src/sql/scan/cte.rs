#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! 递归 CTE：不动点迭代与 VALUES 注入。

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

pub(crate) fn eval_recursive_cte(
    db: &Database,
    sess: &mut Session,
    q: &Query,
    with: &sqlparser::ast::With,
    snapshot: u64,
) -> Result<Query> {
    let cte = with.cte_tables.first().ok_or_else(|| {
        SqlError::not_supported("WITH RECURSIVE with no CTE")
    })?;
    let r_name = cte.alias.name.value.to_ascii_lowercase();
    if cte.materialized.is_some() || !cte.alias.columns.is_empty() {
        return Err(SqlError::not_supported(
            "WITH RECURSIVE with MATERIALIZED / column aliases",
        ));
    }
    // CTE 体必须是 UNION [ALL]（base 臂 + 递归臂）
    let (base_se, rec_se, _all) = match &*cte.query.body {
        sqlparser::ast::SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Union,
            set_quantifier,
            left,
            right,
        } => (
            left,
            right,
            !matches!(set_quantifier, sqlparser::ast::SetQuantifier::Distinct | sqlparser::ast::SetQuantifier::None),
        ),
        _ => {
            return Err(SqlError::not_supported(
                "WITH RECURSIVE body must be UNION [ALL]",
            ))
        }
    };
    // base 求值（不含 r 引用）
    let base_query = mk_single_query(base_se);
    let base_tv = eval_query(db, sess, &base_query, snapshot)?;
    // 逐轮迭代：递归臂中 r → Derived(VALUES 已积累行)，求值至不动点
    let mut all_rows = base_tv.rows.clone();
    const MAX_ITER: usize = 200;
    const MAX_ROWS: usize = 1000; // 发散防护——到顶必须报错（静默截断 =
    // 不可察觉的错误结果，架构审视 #2），PG 同场景报
    // "recursive query cancelled" 而非给出部分行
    let mut hit_limit = false;
    let mut converged = false;
    for _ in 0..MAX_ITER {
        if all_rows.len() >= MAX_ROWS {
            hit_limit = true;
            break;
        }
        let mut rec_q = mk_single_query(rec_se);
        replace_rec_ref(
            &mut rec_q,
            &r_name,
            rows_to_exprs(&all_rows),
            &base_tv.names,
        );
        let rec_tv = eval_query(db, sess, &rec_q, snapshot)?;
        // UNION ALL 追加；UNION DISTINCT 语义按全行文本去重
        let before = all_rows.len();
        all_rows.extend(rec_tv.rows.iter().cloned());
        let mut seen = std::collections::HashSet::new();
        all_rows.retain(|row| {
            let key: String = row
                .iter()
                .map(|v| expr::to_text(v.clone()))
                .collect::<Vec<_>>()
                .join("\u{1}");
            seen.insert(key)
        });
        // 全量注入（非 PG 的 delta 工作表）下递归臂每轮重生成旧行，
        // 产出恒非空——不动点判定必须看"去重后无新行"
        if all_rows.len() == before {
            converged = true;
            break;
        }
    }
    if hit_limit || !converged {
        return Err(SqlError::not_supported(format!(
            "recursive CTE exceeded iteration({MAX_ITER})/row({MAX_ROWS}) limit — \
             divergent or too large; refusing to return partial rows"
        )));
    }
    // 结果 Query：r 替换为 Derived(VALUES all_rows)，外层查询正常求值
    let mut out = q.clone();
    out.with = None;
    replace_rec_ref(&mut out, &r_name, rows_to_exprs(&all_rows), &base_tv.names);
    Ok(out)
}

/// 行值 → 字面量表达式矩阵（VALUES 注入用）
pub(crate) fn rows_to_exprs(rows: &[Vec<SqlValue>]) -> Vec<Vec<Expr>> {
    rows.iter()
        .map(|row| row.iter().map(value_literal_expr).collect())
        .collect()
}

pub(crate) fn value_literal_expr(v: &SqlValue) -> Expr {
    let value = match v {
        SqlValue::Int64(i) => sqlparser::ast::Value::Number(i.to_string(), false),
        SqlValue::Int32(i) => sqlparser::ast::Value::Number(i.to_string(), false),
        SqlValue::Float64(f) => sqlparser::ast::Value::Number(f.to_string(), false),
        SqlValue::Utf8(s) => sqlparser::ast::Value::SingleQuotedString(s.clone()),
        SqlValue::Bool(b) => sqlparser::ast::Value::Boolean(*b),
        _ => sqlparser::ast::Value::Null,
    };
    Expr::Value(sqlparser::ast::ValueWithSpan {
        value,
        span: sqlparser::tokenizer::Span::empty(),
    })
}

pub(crate) fn mk_single_query(se: &sqlparser::ast::SetExpr) -> Query {
    Query {
        with: None,
        body: Box::new(se.clone()),
        order_by: None,
        limit_clause: None,
        fetch: None,
        locks: Vec::new(),
        for_clause: None,
        settings: None,
        format_clause: None,
        pipe_operators: Vec::new(),
    }
}

/// 递归臂中 r 的表引用 → Derived(VALUES rows)
pub(crate) fn replace_rec_ref(
    q: &mut Query,
    r_name: &str,
    rows: Vec<Vec<Expr>>,
    names: &[String],
) {
    if let sqlparser::ast::SetExpr::Select(sel) = &mut *q.body {
        let derived = mk_values_derived(&rows, names, r_name);
        for twj in sel.from.iter_mut() {
            replace_factor_rec(&mut twj.relation, r_name, &derived);
            for j in twj.joins.iter_mut() {
                replace_factor_rec(&mut j.relation, r_name, &derived);
            }
        }
    }
}

pub(crate) fn mk_values_derived(
    rows: &[Vec<Expr>],
    names: &[String],
    r_name: &str,
) -> TableFactor {
    // AST 直构 `(VALUES ...) AS r(col, ...)`——零 SQL 文本、零重解析。
    // （旧方案：UNION ALL of SELECT 文本每轮重 parse 增长中的链，
    // 迭代上限提到 200 后解析/求值递归栈溢出）
    TableFactor::Derived {
        lateral: false,
        subquery: Box::new(Query {
            with: None,
            body: Box::new(sqlparser::ast::SetExpr::Values(sqlparser::ast::Values {
                explicit_row: false,
                value_keyword: false,
                rows: rows
                    .iter()
                    .map(|r| sqlparser::ast::Parens::with_empty_span(r.clone()))
                    .collect(),
            })),
            order_by: None,
            limit_clause: None,
            fetch: None,
            locks: Vec::new(),
            for_clause: None,
            settings: None,
            format_clause: None,
            pipe_operators: Vec::new(),
        }),
        alias: Some(sqlparser::ast::TableAlias {
            explicit: true,
            name: sqlparser::ast::Ident::new(r_name),
            columns: names
                .iter()
                .map(|n| sqlparser::ast::TableAliasColumnDef::from_name(n.clone()))
                .collect(),
            at: None,
        }),
        sample: None,
    }
}

pub(crate) fn replace_factor_rec(
    tf: &mut TableFactor,
    r_name: &str,
    derived: &TableFactor,
) {
    if let TableFactor::Table { name, .. } = tf {
        let short = name
            .0
            .last()
            .and_then(|p| p.as_ident())
            .map(|i| i.value.to_ascii_lowercase())
            .unwrap_or_default();
        if short == r_name {
            *tf = derived.clone();
        }
    }
}
