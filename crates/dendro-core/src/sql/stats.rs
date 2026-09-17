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

/// 表级列统计（列序 = schema 列序——names 来自 schema，stats 来自段）
#[derive(Debug, Clone)]
pub struct TableStats {
    pub names: Vec<String>,
    pub cols: Vec<ColStat>,
}

/// 稀疏读表统计（无段/解析失败 → None——统计面永不阻塞查询）
pub fn table_stats(db: &Database, sess: &Session, table: &str) -> Option<TableStats> {
    let (schema, entry) = crate::sql::scan::resolve_table(db, &sess.branch, table).ok()?;
    if entry.col_segments.is_empty() {
        return None;
    }
    let ap = db.columnar()?;
    let cols = ap.col_stats(db.obj_store(), &entry.col_segments)?;
    Some(TableStats {
        names: schema.columns.iter().map(|c| c.name.clone()).collect(),
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
    //
    // SOTA 调研结论（docs/research/优化器SOTA调研.md）：DuckDB Ebergen
    // 的非等值 d^(2/3) 公式是**无范围信息时**的 ndv-only 回退。我们的
    // CBF footer 提供 min/max 区间（zone map），uniform 区间估计严格
    // 更优——实证：12000 行 b∈[1,11999] 的 `b > 6000`：
    //   uniform = 5999（精确命中线性数据）
    //   d^(2/3) = 11476（保守过度——区间大时 sel ≈ 1-d^(-1/3) ≈ 0.96）
    // 因此保持 uniform 估计为主；d^(2/3) 留作无 min/max 时的回退位。
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
    fn walk(e: &sqlparser::ast::Expr, stats: &TableStats, names: &[String], sels: &mut Vec<f64>) {
        use sqlparser::ast::Expr;
        match e {
            Expr::BinaryOp { left, op, right } => {
                use sqlparser::ast::BinaryOperator as BO;
                match op {
                    BO::And => {
                        walk(left, stats, names, sels);
                        walk(right, stats, names, sels);
                    }
                    BO::Eq => {
                        // 等值：sel ≈ 1/ndv（区间宽 + 1 上界近似——
                        // eq_copy 复制的 `col = N` 谓词需要此分支）
                        for (a, b) in [
                            (left.as_ref(), right.as_ref()),
                            (right.as_ref(), left.as_ref()),
                        ] {
                            let col_name: Option<&String> = match a {
                                Expr::Identifier(id) => Some(&id.value),
                                Expr::CompoundIdentifier(parts) => {
                                    parts.last().as_ref().map(|p| &p.value)
                                }
                                _ => None,
                            };
                            let val = match b {
                                Expr::Value(vws) => {
                                    Some(expr::value_from_parser(vws.value.clone()))
                                }
                                _ => None,
                            };
                            if let (Some(id), Some(_)) = (col_name, val) {
                                if let Some(ci) =
                                    names.iter().position(|n| n.eq_ignore_ascii_case(id))
                                {
                                    let cs = &stats.cols[ci];
                                    if cs.has_data {
                                        let span = cs.max.saturating_sub(cs.min) + 1;
                                        sels.push(1.0 / (span as f64));
                                    }
                                }
                                break;
                            }
                        }
                    }
                    BO::Gt | BO::GtEq | BO::Lt | BO::LtEq => {
                        let op_s = match op {
                            BO::Gt => ">",
                            BO::GtEq => ">=",
                            BO::Lt => "<",
                            _ => "<=",
                        };
                        // 列 op 常量 / 常量 op 列 两种朝向
                        // 朝向两种：col op const / const op col
                        let op_s2 = match op {
                            BO::Gt => "<",
                            BO::GtEq => "<=",
                            BO::Lt => ">",
                            _ => ">=",
                        };
                        for (a, b, o) in [
                            (left.as_ref(), right.as_ref(), op_s),
                            (right.as_ref(), left.as_ref(), op_s2),
                        ] {
                            // 裸名或限定名（末段）——下推合取项必为限定
                            //（conjunct_target 拒裸名），原仅匹配裸 Identifier
                            // 使 scan_est 对重排输入恒全行（P2 修）
                            let col_name: Option<&String> = match a {
                                Expr::Identifier(id) => Some(&id.value),
                                Expr::CompoundIdentifier(parts) => {
                                    parts.last().as_ref().map(|p| &p.value)
                                }
                                _ => None,
                            };
                            let val = match b {
                                Expr::Value(vws) => {
                                    Some(expr::value_from_parser(vws.value.clone()))
                                }
                                _ => None,
                            };
                            if let (Some(id), Some(v)) = (col_name, val) {
                                let _ = o;
                                if let Some(ci) =
                                    names.iter().position(|n| n.eq_ignore_ascii_case(id))
                                {
                                    if let Some(f) = range_selectivity(&stats.cols[ci], &v, o) {
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

// ---------------------------------------------------------------------------
// join reorder 估算（spec 12 §4 前置收口）：
// NDV 近似（无持久化 distinct——footer 只存 min/max/null_count/rows）
// + 等值 join 基数估计 + 扫描估算口
// ---------------------------------------------------------------------------

/// 整数列 NDV（区间宽上界）：min(max−min+1, 非空行数)；防溢出
pub fn ndv_range(stat: &ColStat) -> Option<u64> {
    if !stat.has_data {
        return None;
    }
    let non_null = stat.rows.saturating_sub(stat.nulls);
    if non_null == 0 {
        return None;
    }
    let span = stat.max.saturating_sub(stat.min);
    Some(span.min(non_null - 1) + 1)
}

/// 等值 join 基数估计：|A ⋈ B| ≈ |A|·|B| / max(ndv_l, ndv_r)
///（均匀假设；双侧缺 ndv 回退 sqrt(小侧)——弱区分度经验值）
pub fn join_est_rows(rows_l: u64, rows_r: u64, ndv_l: Option<u64>, ndv_r: Option<u64>) -> u64 {
    let denom = match (ndv_l, ndv_r) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => {
            let s = rows_l.min(rows_r).max(1) as f64;
            return ((rows_l as f64) * (rows_r as f64) / s.sqrt()) as u64;
        }
    };
    (((rows_l as f64) * (rows_r as f64)) / (denom.max(1)) as f64) as u64
}

/// 表扫描估算（reorder 用）：段总行数 × 下推谓词选择率
pub fn scan_est(st: &TableStats, pred: Option<&sqlparser::ast::Expr>) -> u64 {
    let Some(total) = st.cols.first().map(|c| c.rows) else {
        return 0;
    };
    match pred {
        Some(p) => estimate_filter_rows(st, &st.names, p, total),
        None => total,
    }
}

/// 列 NDV 查询口（join 键两侧）：pk 精确 = 行数 / 整数区间界 / None
pub fn col_ndv(st: &TableStats, col: &str, is_pk: bool) -> Option<u64> {
    let idx = st.names.iter().position(|n| n.eq_ignore_ascii_case(col))?;
    let cs = &st.cols.get(idx)?;
    if is_pk {
        Some(cs.rows) // 唯一键精确
    } else {
        ndv_range(cs)
    }
}
