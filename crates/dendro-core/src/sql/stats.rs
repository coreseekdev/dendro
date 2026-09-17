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

/// 等值选择率：MCV 精确频次 → 1/精确 NDV → footer 1/区间宽
pub fn eq_selectivity(
    cs: &ColStat,
    an: Option<&ColAnalyze>,
    an_rows: u64,
    v: &crate::types::SqlValue,
) -> Option<f64> {
    if let Some(an) = an {
        if an.ndv > 0 {
            if let Some(k) = val_key(v) {
                if let Some((_, c)) = an.mcv.iter().find(|(mk, _)| *mk == k) {
                    return Some(*c as f64 / an_rows.max(1) as f64);
                }
            }
            return Some(1.0 / an.ndv as f64);
        }
        return None;
    }
    if cs.has_data {
        let span = cs.max.saturating_sub(cs.min) + 1;
        return Some(1.0 / (span as f64));
    }
    None
}

/// 范围选择率（分析优先）：等高直方图插值（桶间均匀），无直方图回退
/// footer uniform。直方图空/退化 → 回退
pub fn range_selectivity_ex(
    cs: &ColStat,
    hist: Option<&ColHistogram>,
    an_rows: u64,
    v: &crate::types::SqlValue,
    op: &str,
) -> Option<f64> {
    if let Some(h) = hist {
        if h.counts.len() >= 2 && an_rows > 0 {
            let p = val_order(v)?;
            let edges = &h.edges;
            let counts = &h.counts;
            let n_b = counts.len();
            let (lo, hi) = (edges[0], edges[n_b]);
            if p <= lo {
                return match op {
                    ">" | ">=" => Some(1.0),
                    "<" | "<=" => Some(0.0),
                    _ => None,
                };
            }
            if p >= hi {
                return match op {
                    ">" | ">=" => Some(0.0),
                    "<" | "<=" => Some(1.0),
                    _ => None,
                };
            }
            // 定位桶：最后一个 edge ≤ p 的桶（(edge_i, edge_{i+1}]）
            let idx = edges.partition_point(|&e| e <= p) - 1;
            let idx = idx.min(n_b - 1);
            let (blo, bhi) = (edges[idx], edges[idx + 1].max(edges[idx] + 1));
            let within = (p.saturating_sub(blo)) as f64 / ((bhi - blo) as f64);
            let in_bucket = counts[idx] as f64;
            let above: f64 = counts[idx + 1..].iter().sum::<u64>() as f64;
            let ge_frac = (above + in_bucket * (1.0 - within)) / an_rows as f64;
            return match op {
                ">" | ">=" => Some(ge_frac),
                "<" | "<=" => Some((1.0 - ge_frac).max(0.0)),
                _ => None,
            };
        }
    }
    range_selectivity(cs, v, op)
}

/// 谓词（下推到 scan 的 Filter）估算基数：行数 × 各数值范围合取项
/// 选择率连乘（不可估项忽略——只放大不缩零；非数值/复杂形态保守 1）
pub fn estimate_filter_rows(
    stats: &TableStats,
    names: &[String],
    pred: &sqlparser::ast::Expr,
    total_rows: u64,
) -> u64 {
    estimate_filter_rows_ex(stats, None, names, pred, total_rows)
}

/// 带 ANALYZE 统计的估算（消费优先级：MCV 等值精确频次 → 1/精确 NDV
/// → footer 1/区间宽；范围：等高直方图插值 → footer uniform——
/// 偏斜数据下两者差距即本批的动机）
pub fn estimate_filter_rows_ex(
    stats: &TableStats,
    an: Option<&TableAnalyze>,
    names: &[String],
    pred: &sqlparser::ast::Expr,
    total_rows: u64,
) -> u64 {
    // 收集 `col op 数值常量` 合取项
    let mut sels: Vec<f64> = Vec::new();
    fn walk(
        e: &sqlparser::ast::Expr,
        stats: &TableStats,
        an: Option<&TableAnalyze>,
        names: &[String],
        sels: &mut Vec<f64>,
    ) {
        use sqlparser::ast::Expr;
        match e {
            Expr::BinaryOp { left, op, right } => {
                use sqlparser::ast::BinaryOperator as BO;
                match op {
                    BO::And => {
                        walk(left, stats, an, names, sels);
                        walk(right, stats, an, names, sels);
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
                            if let (Some(id), Some(v)) = (col_name, val) {
                                if let Some(ci) =
                                    names.iter().position(|n| n.eq_ignore_ascii_case(id))
                                {
                                    let cs = &stats.cols[ci];
                                    let an_col = an.and_then(|a| a.cols.get(ci));
                                    if let Some(f) = eq_selectivity(
                                        cs,
                                        an_col,
                                        an.map(|a| a.rows).unwrap_or(0),
                                        &v,
                                    ) {
                                        sels.push(f);
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
                                if let Some(ci) =
                                    names.iter().position(|n| n.eq_ignore_ascii_case(id))
                                {
                                    let an_col = an.and_then(|a| a.cols.get(ci));
                                    let f = range_selectivity_ex(
                                        &stats.cols[ci],
                                        an_col.and_then(|c| c.hist.as_ref()),
                                        an.map(|a| a.rows).unwrap_or(0),
                                        &v,
                                        o,
                                    );
                                    if let Some(f) = f {
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
            Expr::Nested(i) => walk(i, stats, an, names, sels),
            _ => {}
        }
    }
    walk(pred, stats, an, names, &mut sels);
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

// ---------------------------------------------------------------------------
// ANALYZE：等高直方图 + MCV + 精确 NDV（性能批 P1；Leis 2025"CE 质量
// 是最高杠杆"的直接落地）。存储 = CAS 侧车 chunk（ChunkType::Stats，
// JSON v1），TableEntry.stats_addr 引用——内容寻址不可变，重分析 =
// 新对象新地址；估算消费按 addr 进程级缓存（视图缓存同式）
// ---------------------------------------------------------------------------

/// 等高直方图（order 域）：K 个桶，边界 edges[K+1]（首尾 = min/max），
/// counts[i] = (edges[i], edges[i+1]] 内样本数（末桶含 hi）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ColHistogram {
    pub edges: Vec<u64>,
    pub counts: Vec<u64>,
}

/// 单列分析产物（文本列无直方图，仅 MCV/NDV）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ColAnalyze {
    pub ndv: u64,
    pub nulls: u64,
    /// 最频值（键口径 = 值规范键，同 eq 估算），上限 32
    pub mcv: Vec<(String, u64)>,
    pub hist: Option<ColHistogram>,
}

/// 表级分析产物（列序 = schema 列序）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableAnalyze {
    pub rows: u64,
    pub cols: Vec<ColAnalyze>,
}

const HIST_BUCKETS: usize = 64;
const MCV_MAX: usize = 32;

/// 值 → 规范键（eq 估算/MCV 统一口径；整数宽度归一同 join 键）
fn val_key(v: &crate::types::SqlValue) -> Option<String> {
    Some(match v {
        crate::types::SqlValue::Utf8(t) => format!("s:{t}"),
        crate::types::SqlValue::Bool(b) => format!("b:{b}"),
        other => format!("o:{}", val_order(other)?),
    })
}

/// 值 → order 域（直方图插值口径；数值族同 range_selectivity）
fn val_order(v: &crate::types::SqlValue) -> Option<u64> {
    match v {
        crate::types::SqlValue::Int32(i) => Some((*i as i64 ^ i64::MIN) as u64),
        crate::types::SqlValue::Int64(i) => Some((*i ^ i64::MIN) as u64),
        crate::types::SqlValue::Date32(d) => Some((*d as i64 ^ i64::MIN) as u64),
        crate::types::SqlValue::TimestampMs(t) => Some((*t ^ i64::MIN) as u64),
        crate::types::SqlValue::Float64(f) => Some(f.to_bits()), // 有序浮点位型
        _ => None,
    }
}

/// ANALYZE TABLE：全量扫描当前可见行 → 每列 NDV/MCV/等高直方图 →
/// CAS 侧车 + catalog 引用提交。陈旧性 = 手动重分析（v1 合同）；
/// 估算侧对 row_count 偏差不做衰减——消费面仅 EXPLAIN est 与 reorder
pub fn analyze_impl(
    db: &crate::engine::Database,
    sess: &mut crate::engine::Session,
    table: &str,
) -> crate::error::Result<Option<crate::types::Output>> {
    use crate::types::Output;
    let (schema, entry) = crate::sql::scan::resolve_table(db, &sess.branch, table)?;
    if sess.txn.is_some() {
        return Err(crate::error::SqlError::new(
            "25001",
            "ANALYZE cannot run inside a transaction",
        ));
    }
    let snapshot = sess.implicit_snapshot(db)?;
    let q = format!("SELECT * FROM {table}");
    let stmts = crate::sql::parse_batch(&q, crate::sql::SqlDialect::Pg)?;
    let Some(sqlparser::ast::Statement::Query(q)) = stmts.into_iter().next() else {
        return Err(crate::error::SqlError::internal("analyze parse"));
    };
    let tv = crate::sql::scan::eval_query(db, sess, &q, snapshot)?;
    let ncols = schema.columns.len();
    let rows = tv.rows.len() as u64;
    // 逐列收集：键 → 频次（MCV/NDV）+ order 域样本（直方图）
    let mut cols_out = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let mut freq: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        let mut nulls = 0u64;
        let mut orders: Vec<u64> = Vec::new();
        for row in &tv.rows {
            let v = row.get(c).cloned().unwrap_or(crate::types::SqlValue::Null);
            if v.is_null() {
                nulls += 1;
                continue;
            }
            if let Some(k) = val_key(&v) {
                *freq.entry(k).or_insert(0) += 1;
            }
            if let Some(o) = val_order(&v) {
                orders.push(o);
            }
        }
        let ndv = freq.len() as u64;
        let mut mcv: Vec<(String, u64)> = freq.into_iter().collect();
        mcv.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0))); // 频次降序，键升序破平
        mcv.truncate(MCV_MAX);
        let hist = if orders.len() >= HIST_BUCKETS * 4 {
            orders.sort_unstable();
            let n = orders.len();
            let mut edges = vec![orders[0]];
            for b in 0..HIST_BUCKETS {
                let hi_rank = (n * (b + 1)) / HIST_BUCKETS; // 等高边界秩
                let edge = if b + 1 == HIST_BUCKETS {
                    orders[n - 1]
                } else {
                    orders[hi_rank.min(n - 1)]
                };
                edges.push(edge);
            }
            // 边界（去重的升序边）上按 (prev, cur] 归属计数
            let mut c2 = vec![0u64; HIST_BUCKETS];
            for &o in &orders {
                // 二分找首个 > o 的边界索引
                let idx = edges.partition_point(|&e| e <= o) - 1;
                let idx = idx.min(HIST_BUCKETS - 1);
                c2[idx] += 1;
            }
            Some(ColHistogram { edges, counts: c2 })
        } else {
            None
        };
        cols_out.push(ColAnalyze {
            ndv,
            nulls,
            mcv,
            hist,
        });
    }
    let an = TableAnalyze {
        rows,
        cols: cols_out,
    };
    let data = serde_json::to_vec(&an)
        .map_err(|e| crate::error::SqlError::io(format!("analyze serde: {e}")))?;
    let chunk = crate::objstore::cas::Chunk {
        ty: crate::objstore::cas::ChunkType::Stats,
        data,
    };
    let chunk_addr = chunk.addr();
    let mut seen = std::collections::HashSet::new();
    db.cas
        .put_batch(&[chunk], &mut seen)
        .map_err(crate::error::SqlError::from)?;
    let ne = crate::versioned::TableEntry {
        stats_addr: Some(chunk_addr.to_base32()),
        ..entry
    };
    crate::sql::ddl::catalog_commit(db, sess, vec![(schema.name.clone(), Some(ne))], "ANALYZE")?;
    // 引用切换后清缓存（旧 addr 条目自然失效；新 addr 首读加载）
    analyze_cache().lock().unwrap().clear();
    Ok(Some(Output::Command {
        tag: "ANALYZE".into(),
        affected: 0,
    }))
}

/// addr → 分析产物（进程级缓存；内容寻址不可变 ⇒ 键即身份）
fn analyze_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<TableAnalyze>>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<TableAnalyze>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// 读取表的分析统计（未分析/不可达 → None——估算回退 footer uniform）
pub fn load_analyze(
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
    table: &str,
) -> Option<std::sync::Arc<TableAnalyze>> {
    let (_, entry) = crate::sql::scan::resolve_table(db, &sess.branch, table).ok()?;
    let addr = entry.stats_addr.as_ref()?;
    if let Some(a) = analyze_cache().lock().unwrap().get(addr) {
        return Some(a.clone());
    }
    let hash = crate::format::hash::Hash::from_base32(addr)?;
    let (_ty, data) = db.cas.get(&hash).ok()?;
    let an: TableAnalyze = serde_json::from_slice(&data).ok()?;
    let an = std::sync::Arc::new(an);
    analyze_cache()
        .lock()
        .unwrap()
        .insert(addr.clone(), an.clone());
    Some(an)
}
