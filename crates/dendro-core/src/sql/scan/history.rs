#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! time travel：AS OF 扫描与时间戳解析。

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

pub(crate) fn time_travel_scan(
    db: &Database,
    branch: &str,
    full_name: &str,
    version: &sqlparser::ast::TableVersion,
    pushdown_limit: Option<usize>,
) -> Result<TableView> {
    let commit = match resolve_as_of(db, branch, version)? {
        Some(c) => c,
        None => {
            return Err(SqlError::new(
                "22023",
                format!(
                    "time travel: no snapshot at or before the given point on branch \"{branch}\""
                ),
            ))
        }
    };
    let short_name = full_name.rsplit(['.', '@']).next().unwrap_or(full_name);
    let (schema, entry) = resolve_table_at(db, Some(&commit.root), short_name)?;
    let root = entry
        .table_root
        .as_ref()
        .and_then(|s| crate::format::hash::Hash::from_base32(s));
    let mut rows = Vec::new();
    if let Some(r) = &root {
        let mut it = crate::prolly::cursor::TreeIter::new(db.store.clone(), r)?;
        while let Some((k, v)) = it.next_item()? {
            let _ = k; // 键 = 主键编码，行内已含列值
            if let Some(cap) = pushdown_limit {
                if rows.len() >= cap {
                    break;
                }
            }
            rows.push(row_from_bytes(&schema, &v)?);
        }
    }
    let names = schema.columns.iter().map(|c| c.name.clone()).collect();
    Ok(TableView { names, rows })
}

/// 解析 `AS OF` 目标：提交哈希（CAS 直接命中）或时间戳（第一父链最近提交）。
/// Ok(None) = 早于历史起点。
pub(crate) fn resolve_as_of(
    db: &Database,
    branch: &str,
    version: &sqlparser::ast::TableVersion,
) -> Result<Option<crate::versioned::commit::Commit>> {
    let sqlparser::ast::TableVersion::ForSystemTimeAsOf(expr) = version else {
        return Err(SqlError::not_supported(format!("AS OF: {version}")));
    };
    let literal = match expr {
        sqlparser::ast::Expr::Value(vws) => match &vws.value {
            sqlparser::ast::Value::SingleQuotedString(s)
            | sqlparser::ast::Value::DoubleQuotedString(s) => s.clone(),
            sqlparser::ast::Value::Number(n, _) => n.clone(),
            other => return Err(SqlError::not_supported(format!("AS OF literal: {other}"))),
        },
        other => {
            return Err(SqlError::not_supported(format!(
                "AS OF: only string/number literals supported, got {other}"
            )))
        }
    };
    // ① 提交哈希：base32 可解码且 CAS 命中 → 精确提交（跨分支快照读允许）。
    // 类型标签非 Commit 的对象（node/schema/哈希猜测命中）→ 22023 而非 500；
    // CAS 未命中 → 落到时间戳解析路径
    if let Some(h) = crate::format::hash::Hash::from_base32(&literal) {
        if let Ok((ty, data)) = db.cas.get(&h) {
            if ty != crate::objstore::cas::ChunkType::Commit {
                return Err(SqlError::new(
                    "22023",
                    format!("AS OF \"{literal}\": object exists but is not a commit"),
                ));
            }
            return crate::versioned::commit::Commit::decode(&data)
                .map(Some)
                .map_err(|e| SqlError::internal(format!("as-of commit: {e}")));
        }
    }
    // ② 时间戳：epoch ms 或 ISO8601
    let target_ms = parse_as_of_ms(&literal)?;
    let b = db.branch(branch)?;
    let mut cur = b.head.load_full();
    let mut guard = 0u32;
    loop {
        match cur.as_ref() {
            None => return Ok(None), // 链尽（早于首个提交）
            Some(c) if c.ts_ms <= target_ms => return Ok(Some(c.clone())),
            Some(c) => match c.parents.first() {
                Some(p) => {
                    let (_ty, data) = db.cas.get(p)?;
                    cur = Arc::new(Some(crate::versioned::commit::Commit::decode(&data)?));
                    guard += 1;
                    if guard > 100_000 {
                        return Err(SqlError::internal("as-of history walk overflow"));
                    }
                }
                None => return Ok(None),
            },
        }
    }
}

/// `AS OF` 时间字面量：epoch 毫秒（纯数字）或 `YYYY-MM-DD[ T]HH:MM[:SS[.mmm]]Z?`
/// （缺省时间分量取 0；一律按 UTC 解释——文档口径）。错误 → 22023。
pub(crate) fn parse_as_of_ms(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    let s = s.strip_suffix('Z').unwrap_or(s);
    let s = s.trim_end_matches("+00:00");
    let bad = || {
        SqlError::new(
            "22023",
            format!("AS OF: cannot parse timestamp \"{s}\" (epoch ms or ISO8601 UTC)"),
        )
    };
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dp = date.split('-');
    let (y, mo, d) = match (dp.next(), dp.next(), dp.next()) {
        (Some(y), Some(mo), Some(d)) if dp.next().is_none() => (y, mo, d),
        _ => return Err(bad()),
    };
    let (y, mo, d) = (
        y.parse::<i64>().map_err(|_| bad())?,
        mo.parse::<u32>().map_err(|_| bad())?,
        d.parse::<u32>().map_err(|_| bad())?,
    );
    // 年份上界（轮次审计 R2）：无界年份在 days*86_400*1000 处 i64 溢出——
    // debug 构建 panic、release 静默回绕。±300_000 年远超任何提交时间线
    // 且算术余量 >30 倍（300000*366*86400*1000 ≈ 9.5e15 < i64::MAX/30）。
    if !(-300_000..=300_000).contains(&y) {
        return Err(bad());
    }
    // 月长/闰年校验（此前 2026-02-30 会滚动到 3 月 2 日）
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][(mo - 1) as usize];
    if !(1..=12).contains(&mo) || !(1..=dim).contains(&d) {
        return Err(bad());
    }
    let mut hh = 0i64;
    let mut mi = 0i64;
    let mut sec = 0i64;
    let mut ms = 0i64;
    if let Some(t) = time {
        let (hms, frac) = match t.split_once('.') {
            Some((h, f)) => (h, Some(f)),
            None => (t, None),
        };
        let parts: Vec<&str> = hms.split(':').collect();
        if parts.is_empty() || parts.len() > 3 {
            return Err(bad());
        }
        let parse = |v: &str| -> Result<i64> { v.parse::<i64>().map_err(|_| bad()) };
        hh = parse(parts[0])?;
        if parts.len() > 1 {
            mi = parse(parts[1])?;
        }
        if parts.len() > 2 {
            sec = parse(parts[2])?;
        }
        if let Some(f) = frac {
            // 任意位宽分数秒：取前 3 位为毫秒，余位须为数字（µs/ns 常见）
            if f.is_empty() || !f.chars().all(|c| c.is_ascii_digit()) {
                return Err(bad());
            }
            let f3: String = f.chars().take(3).collect();
            if !f3.is_empty() {
                ms = f3.parse::<i64>().map_err(|_| bad())? * 10i64.pow(3 - f3.len() as u32);
            }
        }
        if !(0..24).contains(&hh) || !(0..60).contains(&mi) || !(0..61).contains(&sec) {
            return Err(bad());
        }
    }
    // 民用日期 → Unix 天数（Howard Hinnant 算法，proleptic Gregorian）
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = ((mo + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(((days * 86_400 + hh * 3600 + mi * 60 + sec) * 1_000) + ms)
}
