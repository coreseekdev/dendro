#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! 窗口函数求值：分区聚合 + 逐行排名，合成列。

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

/// 收集 Select 投影中的窗口函数调用
pub(crate) struct WindowCall {
    /// 函数名（row_number / rank / dense_rank / sum / count / min / max / avg）
    pub(crate) func: String,
    /// 参数（None = 无参如 row_number()；Some = sum(v) 的 v）
    pub(crate) arg: Option<Expr>,
    /// PARTITION BY 表达式列表
    pub(crate) partition_by: Vec<Expr>,
    /// ORDER BY 表达式 + ASC 标记
    pub(crate) order_by: Vec<(Expr, bool)>,
    /// 合成列名（__w0, __w1...——追加到 tv.names）
    pub(crate) synth_col: String,
}

/// 提取 Select 投影中的窗口函数（v1：每项最多 1 个窗口调用）
pub(crate) fn collect_window_calls(select: &Select) -> Result<Vec<WindowCall>> {
    let mut out = Vec::new();
    for item in &select.projection {
        let e = match item {
            SelectItem::UnnamedExpr(e) => e,
            SelectItem::ExprWithAlias { expr, .. } => expr,
            _ => continue,
        };
        if let Expr::Function(f) = e {
            if let Some(over) = &f.over {
                let sqlparser::ast::WindowType::WindowSpec(spec) = over else {
                    return Err(SqlError::not_supported("named window (WINDOW clause)"));
                };
                if spec.window_frame.is_some() {
                    return Err(SqlError::not_supported("window frame (ROWS/RANGE)"));
                }
                let name = f.name.to_string().to_ascii_lowercase();
                if !matches!(
                    name.as_str(),
                    "row_number" | "rank" | "dense_rank" | "sum" | "count" | "min" | "max" | "avg"
                ) {
                    return Err(SqlError::not_supported(format!("window function {name}")));
                }
                let arg = match fn_args(f).first() {
                    Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => Some(e.clone()),
                    Some(FunctionArg::Unnamed(FunctionArgExpr::Wildcard)) => None,
                    None => None,
                    _ => return Err(SqlError::not_supported("window function arg form")),
                };
                // 排名函数无参；聚合必须有参数
                if matches!(name.as_str(), "row_number" | "rank" | "dense_rank") && arg.is_some() {
                    return Err(SqlError::syntax("ranking function takes no argument"));
                }
                if matches!(name.as_str(), "sum" | "min" | "max" | "avg") && arg.is_none() {
                    return Err(SqlError::syntax("aggregate window function requires argument"));
                }
                let order_by: Vec<(Expr, bool)> = spec
                    .order_by
                    .iter()
                    .map(|o| (o.expr.clone(), o.options.asc.unwrap_or(true)))
                    .collect();
                let synth_col = format!("__w{}", out.len());
                out.push(WindowCall {
                    func: name,
                    arg,
                    partition_by: spec.partition_by.clone(),
                    order_by,
                    synth_col,
                });
            }
        }
    }
    Ok(out)
}

/// 窗口函数求值：全输入行 → 合成列追加到 tv
/// 语义：row_number = 分组内排序序号（1 起，同键稳定序）；
/// rank = 跳跃排名（同值同名次，下一个跳）；dense_rank = 连续排名；
/// sum/count/min/max/avg = 分组聚合（窗口 = 每行都出——非塌缩）
pub(crate) fn eval_windows(
    tv: &mut TableView,
    calls: &[WindowCall],
    cols: &std::collections::HashMap<String, usize>,
) -> Result<()> {
    let n = tv.rows.len();
    // 对每个窗口调用：计算每行的窗口值
    for wc in calls {
        // 1. PARTITION BY 键提取（每行）
        let colfn = |name: &str| cols.get(&name.to_ascii_lowercase()).copied();
        let mut part_keys: Vec<String> = Vec::with_capacity(n);
        let mut order_keys: Vec<Vec<SqlValue>> = Vec::with_capacity(n);
        for row in &tv.rows {
            let mut pk = String::new();
            for pe in &wc.partition_by {
                let v = expr::eval(pe, row, &colfn)?;
                pk.push_str(&expr::to_text(v));
                pk.push('\u{1}');
            }
            part_keys.push(pk);
            let mut ok = Vec::with_capacity(wc.order_by.len());
            for (oe, _) in &wc.order_by {
                let v = expr::eval(oe, row, &colfn)?;
                ok.push(v);
            }
            order_keys.push(ok);
        }
        // 2. 行索引按 (partition, order) 排序（稳定——保原序 tie-break）
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| {
            part_keys[a].cmp(&part_keys[b]).then_with(|| {
                for (ka, kb) in order_keys[a].iter().zip(&order_keys[b]) {
                    let ord = if ka.is_null() && kb.is_null() {
                        std::cmp::Ordering::Equal
                    } else if ka.is_null() {
                        std::cmp::Ordering::Greater // null-last
                    } else if kb.is_null() {
                        std::cmp::Ordering::Less
                    } else {
                        expr::cmp_values(ka, kb).unwrap_or(std::cmp::Ordering::Equal)
                    };
                    // ASC 默认（DESC 每键翻——v1 全 ASC 语义 + 调用侧翻转）
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            })
        });
        // 3. 窗口值计算（v1：分区聚合——无 frame 时整分区同值）
        // 排名函数（row_number/rank/dense_rank）需要行序 → 逐行
        // 聚合函数（sum/count/min/max/avg）→ 分区总计（同一分区每行同值）
        let is_ranking = matches!(
            wc.func.as_str(),
            "row_number" | "rank" | "dense_rank"
        );
        if is_ranking {
            // 排名：逐行（按排序后序遍历——依赖行序）
            let mut values: Vec<SqlValue> = vec![SqlValue::Null; n];
            let mut prev_part: Option<&String> = None;
            let mut rank = 0u64;
            let mut dense_rank = 0u64;
            let mut row_num = 0u64;
            for &ri in &idx {
                let is_new_part = prev_part != Some(&part_keys[ri]);
                if is_new_part {
                    rank = 0;
                    dense_rank = 0;
                    row_num = 0;
                    prev_part = Some(&part_keys[ri]);
                }
                row_num += 1;
                let same_order = row_num > 1 && {
                    let prev_ri = idx[row_num as usize - 2];
                    part_keys[prev_ri] == part_keys[ri]
                        && order_keys[prev_ri] == order_keys[ri]
                };
                if !same_order {
                    rank = row_num;
                    dense_rank += 1;
                }
                values[ri] = match wc.func.as_str() {
                    "row_number" => SqlValue::Int64(row_num as i64),
                    "rank" => SqlValue::Int64(rank as i64),
                    _ => SqlValue::Int64(dense_rank as i64),
                };
            }
            // 合成列追加
            tv.names.push(wc.synth_col.clone());
            for (ri, row) in tv.rows.iter_mut().enumerate() {
                row.push(values[ri].clone());
            }
        } else {
            // 聚合：分区总计（无 frame → 整分区同值——PG 默认语义）
            let mut values: Vec<SqlValue> = vec![SqlValue::Null; n];
            let mut processed: Vec<bool> = vec![false; n];
            for &ri in &idx {
                if processed[ri] {
                    continue;
                }
                let pk = &part_keys[ri];
                // 同分区全部行
                let mut agg_count = 0u64;
                let mut agg_sum = 0f64;
                let mut agg_is_float = false;
                let mut agg_min: Option<SqlValue> = None;
                let mut agg_max: Option<SqlValue> = None;
                for &rj in &idx {
                    if &part_keys[rj] != pk {
                        continue;
                    }
                    processed[rj] = true;
                    // 累加
                    if let Some(arg_e) = &wc.arg {
                        let v = expr::eval(arg_e, &tv.rows[rj], &colfn)?;
                        if !v.is_null() {
                            agg_count += 1;
                            match &v {
                                SqlValue::Int64(i) => agg_sum += *i as f64,
                                SqlValue::Int32(i) => agg_sum += *i as f64,
                                SqlValue::Float64(f) => {
                                    agg_is_float = true;
                                    agg_sum += *f;
                                }
                                _ => {}
                            }
                            let less = agg_min.as_ref().is_none_or(|m| {
                                expr::cmp_values(&v, m)
                                    .map(|o| o == std::cmp::Ordering::Less)
                                    .unwrap_or(false)
                            });
                            if less {
                                agg_min = Some(v.clone());
                            }
                            let greater = agg_max.as_ref().is_none_or(|m| {
                                expr::cmp_values(&v, m)
                                    .map(|o| o == std::cmp::Ordering::Greater)
                                    .unwrap_or(false)
                            });
                            if greater {
                                agg_max = Some(v);
                            }
                        }
                    } else {
                        agg_count += 1; // count(*)
                    }
                }
                // 分配分区聚合值
                let val = match wc.func.as_str() {
                    "count" => SqlValue::Int64(agg_count as i64),
                    "sum" => {
                        if agg_count == 0 {
                            SqlValue::Null
                        } else if agg_is_float {
                            SqlValue::Float64(agg_sum)
                        } else {
                            SqlValue::Int64(agg_sum as i64)
                        }
                    }
                    "avg" => {
                        if agg_count == 0 {
                            SqlValue::Null
                        } else {
                            SqlValue::Float64(agg_sum / agg_count as f64)
                        }
                    }
                    "min" => agg_min.clone().unwrap_or(SqlValue::Null),
                    "max" => agg_max.clone().unwrap_or(SqlValue::Null),
                    _ => SqlValue::Null,
                };
                for &rj in &idx {
                    if &part_keys[rj] == pk {
                        values[rj] = val.clone();
                    }
                }
            }
            // 合成列追加
            tv.names.push(wc.synth_col.clone());
            for (ri, row) in tv.rows.iter_mut().enumerate() {
                row.push(values[ri].clone());
            }
        }
    }
    Ok(())
}


// ---------------------------------------------------------------------------
// P0：递归 CTE（WITH RECURSIVE 迭代不动点）
// r AS (base UNION [ALL] recursive)
// → R0 = eval(base)；R(k+1) = eval(recursive, r ← Rk as Derived)
// → R(k+1) 为空或上限 → 终止；结果 = R0 ∪ R1 ∪ ...
// 返回的 Query 将 r 替换为 Derived{总结果}（后续走正常查询路径）
// ---------------------------------------------------------------------------
