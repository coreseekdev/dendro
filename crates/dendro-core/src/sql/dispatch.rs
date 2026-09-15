//! v2c-1（C4）：coverage 派发器（ir-spec 05 号）——扫描路径选择的纯函数化。
//!
//! 原状：table_scan_opt 的三层快路径 if-else（pk 下推 → AP 扫描 → 行扫），
//! 决策标准散落且不可组合。本模块把决策抽为**单点纯函数**：
//! `(表因子, 谓词, 会话) → ScanAlt`——派发可枚举 ⇒ 差分可枚举（ADR-5）。
//!
//! 执行器（try_pk_pushdown / try_ap_scan / table_scan）保持内部守卫：
//! 决策与执行之间若发生竞态（如段被并发 checkpoint 回收），执行器
//! 返回 None → 调用方回落行路径（与原 if-else 行为一致）。
//!
//! **force_source**（05 §6-2 定案：派发器参数形态）：仅调试/测试构建可
//! 经 `SET dendro.force_source` 设置（release 忽略该 SET，S-3 精神——
//! 调试面不进生产二进制的语义面）。强制语义 = "结构性可行即走"：
//! 旁路**性能阈值**（1 万行门槛），但无段/无 pk 键等结构性不可行 → 报错
//! （静默回落会让差分测试失义）。

use crate::engine::{Database, Session};
use crate::error::{Result, SqlError};
use sqlparser::ast::{Expr, TableFactor};

/// 扫描替身（02 §3 的四类；命名按评审 P0-2 修正——CurrentPoint 是
/// "当前读点查"（memtx→墓碑→树→txn 四段合成），非 memtx-only）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanAlt {
    /// 当前读点查（pk 等值/IN）
    CurrentPoint,
    /// CBF 段 + 当前读尾巴归并（AP）
    MainPlusDelta,
    /// prolly 时间旅行（version 子句；由 table_scan 内部路由）
    HistoryScan,
    /// 现行行扫描（prolly+overlay；等价性锚点）
    RowFallback,
}

impl ScanAlt {
    pub fn label(&self) -> &'static str {
        match self {
            ScanAlt::CurrentPoint => "CurrentPoint",
            ScanAlt::MainPlusDelta => "MainPlusDelta",
            ScanAlt::HistoryScan => "HistoryScan",
            ScanAlt::RowFallback => "RowFallback",
        }
    }
}

/// force_source 取值解析（SET 值 → 强制目标；"auto" = 清除）
pub fn parse_force(v: &str) -> Result<Option<ScanAlt>> {
    Ok(match v.to_ascii_lowercase().as_str() {
        "auto" => None,
        "delta" | "point" | "current" => Some(ScanAlt::CurrentPoint),
        "main" | "main+delta" | "ap" => Some(ScanAlt::MainPlusDelta),
        "prolly" | "history" => Some(ScanAlt::HistoryScan),
        "fallback" | "row" => Some(ScanAlt::RowFallback),
        other => {
            return Err(SqlError::syntax(format!(
                "unknown dendro.force_source value: {other} \
                 (auto|delta|main|prolly|fallback)"
            )))
        }
    })
}

/// v2c-3：聚合执行路径强制目标（AggOp 管线 vs group_aggregate 行式）。
/// 缺省（None）= auto：资格判定（组键/聚合参数均为纯列引用 → 管线）；
/// pipeline/row 仅调试构建可强制（差分测试，ADR-5 同源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggPath {
    Pipeline,
    Row,
}

pub fn parse_force_agg(v: &str) -> Result<Option<AggPath>> {
    Ok(match v.to_ascii_lowercase().as_str() {
        "auto" => None,
        "pipeline" => Some(AggPath::Pipeline),
        "row" => Some(AggPath::Row),
        other => {
            return Err(SqlError::syntax(format!(
                "unknown dendro.force_agg value: {other} (auto|pipeline|row)"
            )))
        }
    })
}

/// 派发（05 §1-2：候选生成 + 规则序；无统计代价模型——规则序即优先序）：
/// 1. version 子句 → HistoryScan（R6 P0 门控的 IR 化：从未加入其他候选）
/// 2. force_source 覆盖（调试构建）：结构性检查后旁路阈值
/// 3. pk 等值/IN 可下推 → CurrentPoint
/// 4. AP 资格（段存在 + ≥1 万行）→ MainPlusDelta
/// 5. 其余 → RowFallback
pub fn dispatch_scan(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    selection: Option<&Expr>,
    snapshot: u64,
) -> Result<ScanAlt> {
    let _ = snapshot;
    // 强制覆盖（仅调试构建可设值；release 下恒 None——见 SET 处理）
    if let Some(forced) = sess.force_source {
        return enforce_forced(db, sess, tf, selection, forced);
    }
    if let TableFactor::Table { version, .. } = tf {
        if version.is_some() {
            return Ok(ScanAlt::HistoryScan);
        }
    }
    // pk 点查资格（复用既有资格判定——单点事实源）
    if let Ok(Some(_)) = super::scan::try_pk_pushdown(db, sess, tf, selection, 0) {
        return Ok(ScanAlt::CurrentPoint);
    }
    if super::scan::ap_resolve(db, sess, tf, false).is_some() {
        return Ok(ScanAlt::MainPlusDelta);
    }
    Ok(ScanAlt::RowFallback)
}

/// 强制语义：结构性可行即走（旁路性能阈值）；结构性不可行 = 报错
fn enforce_forced(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    selection: Option<&Expr>,
    forced: ScanAlt,
) -> Result<ScanAlt> {
    match forced {
        ScanAlt::RowFallback => Ok(ScanAlt::RowFallback),
        ScanAlt::HistoryScan => {
            if let TableFactor::Table { version, .. } = tf {
                if version.is_some() {
                    return Ok(ScanAlt::HistoryScan);
                }
            }
            Err(SqlError::syntax(
                "cannot force prolly: no FOR SYSTEM_TIME/AS OF clause on this query",
            ))
        }
        ScanAlt::CurrentPoint => {
            // 点查路径结构性要求 pk 等值/IN 谓词（无键集无法点查）
            let eligible = matches!(
                super::scan::try_pk_pushdown(db, sess, tf, selection, 0),
                Ok(Some(_))
            );
            if eligible {
                Ok(ScanAlt::CurrentPoint)
            } else {
                Err(SqlError::syntax(
                    "cannot force delta: no pk equality/IN predicate on this query",
                ))
            }
        }
        ScanAlt::MainPlusDelta => {
            if super::scan::ap_resolve(db, sess, tf, true).is_some() {
                Ok(ScanAlt::MainPlusDelta)
            } else {
                Err(SqlError::syntax(
                    "cannot force main: table has no columnar segments \
                     (structural requirement, not a threshold)",
                ))
            }
        }
    }
}
