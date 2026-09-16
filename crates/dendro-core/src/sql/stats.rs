//! 列统计（join reorder 前置——spec 12 §4）：CBF footer 聚合 + order 域
//! 选择率。统计源 = 段级 zone map（每块每列 min/max/null_count），经
//! 稀疏 footer 读取得——零列数据解码。
//!
//! 消费面 v1：EXPLAIN ANALYZE 的 est 对 actual（估算可见性 + 正确性
//! 验证）；join reorder 落地时作基数估计输入（uniform 假设——无
//! NDV/直方图，等值谓词不可估，恒 None）。

use crate::engine::{Database, Session};
use crate::sql::expr;

/// 单列聚合统计（order 域）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColStat {
    pub rows: u64,
    pub nulls: u64,
    pub min: u64,
    pub max: u64,
    pub has_data: bool,
}

/// 表级列统计（列序 = schema 列序）
#[derive(Debug, Clone)]
pub struct TableStats {
    pub cols: Vec<ColStat>,
}

/// 稀疏读表统计（无段/解析失败 → None——统计面永不阻塞查询）
pub fn table_stats(
    db: &Database,
    sess: &Session,
    table: &str,
) -> Option<TableStats> {
    let (_, entry) = crate::sql::scan::resolve_table(db, &sess.branch, table).ok()?;
    if entry.col_segments.is_empty() {
        return None;
    }
    let ap = db.columnar()?;
    let cols = ap.col_stats(db.obj_store(), &entry.col_segments)?;
    Some(TableStats {
        cols: cols
            .into_iter()
            .map(|c| ColStat {
                rows: c.rows,
                nulls: c.nulls,
                min: c.min,
                max: c.max,
                has_data: c.has_data,
            })
            .collect(),
    })
}

/// 数值 → order 域（Int 家族与 scan.rs order_domain 同口径）
fn order_of(v: &crate::types::SqlValue) -> Option<u64> {
    Some(match v {
        crate::types::SqlValue::Int32(i) => (*i as i64 ^ i64::MIN) as u64,
        crate::types::SqlValue::Int64(i) => (*i ^ i64::MIN) as u64,
        crate::types::SqlValue::Date32(d) => (*d as i64 ^ i64::MIN) as u64,
        crate::types::SqlValue::TimestampMs(t) => (*t ^ i64::MIN) as u64,
        _ => return None,
    })
}

/// 范围谓词选择率（uniform 假设：谓词点在 [min,max] order 域内外的
/// 区间占比；区间零宽/类型不可比 → None——统计面保守放弃）
pub fn range_selectivity(stat: &ColStat, v: &crate::types::SqlValue, op: &str) -> Option<f64> {
    let p = order_of(v)?;
    if !stat.has_data || stat.max < stat.min {
        return None;
    }
    let (lo, hi) = (stat.min, stat.max);
    if hi == lo {
        // 单点区间：谓词点在区间外的比较可判 0
        return match op {
            ">" | ">=" if p >= hi => Some(0.0),
            "<" | "<=" if p <= lo => Some(0.0),
            "<" if p > hi => Some(1.0),
            ">" if p < lo => Some(1.0),
            _ => Some(1.0), // 单点区间内的比较保守 1
        };
    }
    // 差值整数运算后再转 f64——order 域绝对值（~2^63）超 f64 精度
    //（ulp 2048），先减后除保区间分数精确（stats 差分实证：est=0 假象）
    let below = p.saturating_sub(lo);
    let span = hi - lo;
    let frac = (below.min(span) as f64) / (span as f64);
    match op {
        ">" | ">=" => Some(1.0 - frac),
        "<" | "<=" => Some(frac),
        _ => None,
    }
}

/// 谓词（下推到 scan 的 Filter）估算基数：行数 × 各数值范围合取项
/// 选择率连乘（不可估项忽略——只放大不缩零；非数值/复杂形态保守 1）
pub fn estimate_filter_rows(
    stats: &TableStats,
    names: &[String],
    pred: &sqlparser::ast::Expr,
    total_rows: u64,
) -> u64 {
    // 收集 `col op 数值常量` 合取项
    let mut sels: Vec<f64> = Vec::new();
    fn walk(
        e: &sqlparser::ast::Expr,
        stats: &TableStats,
        names: &[String],
        sels: &mut Vec<f64>,
    ) {
        use sqlparser::ast::Expr;
        match e {
            Expr::BinaryOp { left, op, right } => {
                use sqlparser::ast::BinaryOperator as BO;
                match op {
                    BO::And => {
                        walk(left, stats, names, sels);
                        walk(right, stats, names, sels);
                    }
                    BO::Gt | BO::GtEq | BO::Lt | BO::LtEq => {
                        let op_s = match op {
                            BO::Gt => ">",
                            BO::GtEq => ">=",
                            BO::Lt => "<",
                            _ => "<=",
                        };
                        // 列 op 常量 / 常量 op 列 两种朝向
                        for (a, b, o) in [
                            (left.as_ref(), right.as_ref(), op_s),
                            (right.as_ref(), left.as_ref(), match op_s {
                                ">" => "<",
                                ">=" => "<=",
                                "<" => ">",
                                _ => ">=",
                            }),
                        ] {
                            if let (
                                Expr::Identifier(id),
                                Expr::Value(vws),
                            ) = (a, b)
                            {
                                let v = expr::value_from_parser(vws.value.clone());
                                if let Some(ci) = names
                                    .iter()
                                    .position(|n| n.eq_ignore_ascii_case(&id.value))
                                {
                                    if let Some(f) =
                                        range_selectivity(&stats.cols[ci], &v, o)
                                    {
                                        sels.push(f);
                                    }
                                }
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Expr::Nested(i) => walk(i, stats, names, sels),
            _ => {}
        }
    }
    walk(pred, stats, names, &mut sels);
    let mut est = total_rows as f64;
    for f in sels {
        est *= f;
    }
    est.round() as u64
}
