#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! PK 点查与范围提取：谓词→PK 区间、pushdown、点查视图。

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

/// order 域（定点解释）值：与 CBF footer 的 min/max 同口径
pub(crate) fn order_domain(v: &SqlValue) -> Option<u64> {
    Some(match v {
        SqlValue::Int32(i) => (*i as i64 ^ i64::MIN) as u64,
        SqlValue::Int64(i) => (i ^ i64::MIN) as u64,
        SqlValue::Date32(d) => (*d as i64 ^ i64::MIN) as u64,
        SqlValue::TimestampMs(t) => (t ^ i64::MIN) as u64,
        SqlValue::Float64(f) => crate::format::row::f64_to_orderable(*f),
        SqlValue::Utf8(s) => {
            let mut b = [0u8; 8];
            for (i, byte) in s.as_bytes().iter().take(8).enumerate() {
                b[i] = *byte;
            }
            u64::from_be_bytes(b)
        }
        _ => return None,
    })
}

/// 从 WHERE 提取单列 pk 的范围 (min_excl, max_incl)
pub(crate) fn extract_pk_range(sel: &Expr, pk: &str) -> Option<(Option<u64>, Option<u64>)> {
    fn lit_of(e: &Expr) -> Option<SqlValue> {
        if let Expr::Value(vws) = e {
            if !matches!(vws.value, sqlparser::ast::Value::Placeholder(_)) {
                return Some(super::expr::value_from_parser(vws.value.clone()));
            }
        }
        None
    }
    fn col_is(e: &Expr, pk: &str) -> bool {
        match e {
            Expr::Identifier(id) => id.value.eq_ignore_ascii_case(pk),
            _ => false,
        }
    }
    let mut lo: Option<u64> = None;
    let mut hi: Option<u64> = None;
    fn walk(e: &Expr, pk: &str, lo: &mut Option<u64>, hi: &mut Option<u64>) {
        match e {
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::And,
                right,
            } => {
                walk(left, pk, lo, hi);
                walk(right, pk, lo, hi);
            }
            Expr::BinaryOp { left, op, right } => {
                let (col, val, flip) = if col_is(left, pk) {
                    (true, lit_of(right), false)
                } else if col_is(right, pk) {
                    (true, lit_of(left), true)
                } else {
                    (false, None, false)
                };
                if let (true, Some(v)) = (col, val) {
                    let Some(d) = order_domain(&v) else { return };
                    use sqlparser::ast::BinaryOperator as BO;
                    let eff = match (op, flip) {
                        (BO::Gt, false) | (BO::Lt, true) => Some(("gt", d)),
                        (BO::GtEq, false) | (BO::LtEq, true) => Some(("gte", d)),
                        (BO::Lt, false) | (BO::Gt, true) => Some(("lt", d)),
                        (BO::LtEq, false) | (BO::GtEq, true) => Some(("lte", d)),
                        (BO::Eq, _) => Some(("eq", d)),
                        _ => None,
                    };
                    if let Some((kind, dv)) = eff {
                        match kind {
                            "gt" => {
                                if lo.is_none_or(|l| dv > l) {
                                    *lo = Some(dv);
                                }
                            }
                            "gte" => {
                                if lo.is_none_or(|l| dv.saturating_sub(1) > l) {
                                    *lo = Some(dv.saturating_sub(1));
                                }
                            }
                            "eq" => {
                                *lo = Some(dv.saturating_sub(1));
                                *hi = Some(dv + 1);
                            }
                            "lte" => {
                                if hi.is_none_or(|h| dv < h) {
                                    *hi = Some(dv);
                                }
                            }
                            "lt" if hi.is_none_or(|h| dv.saturating_sub(1) < h) => {
                                *hi = Some(dv.saturating_sub(1));
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    walk(sel, pk, &mut lo, &mut hi);
    if lo.is_none() && hi.is_none() {
        None
    } else {
        Some((lo, hi))
    }
}

/// 提取单列数字 PK 的范围合取（P2-6g）：返回 (lo, hi)，各为 (值, 是否含端点)。
/// 识别 AND 树中的 `pk >/>=/</<= 字面量`（字面量在另一侧亦可，方向自动翻转）；
/// 非 PK/非数字字面量的合取返回 None——由下游常规过滤承担，不影响正确性。
/// 返回 None = 无任何范围界（不做下推）。
pub(crate) fn extract_pk_int_range(
    e: &Expr,
    pk: &str,
) -> Option<(Option<(i64, bool)>, Option<(i64, bool)>)> {
    use sqlparser::ast::BinaryOperator as Op;
    match e {
        Expr::Nested(inner) => extract_pk_int_range(inner, pk),
        Expr::BinaryOp {
            left,
            op: Op::And,
            right,
        } => {
            let a = extract_pk_int_range(left, pk);
            let b = extract_pk_int_range(right, pk);
            match (a, b) {
                (Some((l1, h1)), Some((l2, h2))) => {
                    // 交：lo 取值更大者、同值取排他（更紧）；hi 取值更小者、
                    // 同值取排他（更紧）
                    let lo = match (l1, l2) {
                        (Some(x), Some(y)) => Some(match (x, y) {
                            (x, y) if x.0 > y.0 => x,
                            (x, y) if x.0 < y.0 => y,
                            (x, y) => {
                                if !x.1 {
                                    x
                                } else {
                                    y
                                }
                            }
                        }),
                        (x, None) => x,
                        (None, y) => y,
                    };
                    let hi = match (h1, h2) {
                        (Some(x), Some(y)) => Some(match (x, y) {
                            (x, y) if x.0 < y.0 => x,
                            (x, y) if x.0 > y.0 => y,
                            (x, y) => {
                                if !x.1 {
                                    x
                                } else {
                                    y
                                }
                            }
                        }),
                        (x, None) => x,
                        (None, y) => y,
                    };
                    Some((lo, hi))
                }
                (a, b) => a.or(b),
            }
        }
        Expr::BinaryOp {
            left,
            op: op @ (Op::Gt | Op::GtEq | Op::Lt | Op::LtEq),
            right,
        } => {
            // 主键列在左/右识别 + 字面量提取
            let (col_first, other): (bool, &Expr) = match (left.as_ref(), right.as_ref()) {
                (Expr::Identifier(id), v) if id.value.eq_ignore_ascii_case(pk) => (true, v),
                (v, Expr::Identifier(id)) if id.value.eq_ignore_ascii_case(pk) => (false, v),
                _ => return None,
            };
            let v = match other {
                Expr::Value(vws) => super::expr::value_from_parser(vws.value.clone()),
                Expr::UnaryOp {
                    op: sqlparser::ast::UnaryOperator::Minus,
                    expr: inner,
                } => match inner.as_ref() {
                    // 小数字面量是 Int32（number_or_string 窄化）——两档都要取负
                    Expr::Value(vws) => match super::expr::value_from_parser(vws.value.clone()) {
                        SqlValue::Int64(i) => SqlValue::Int64(-i),
                        SqlValue::Int32(i) => SqlValue::Int64(-(i as i64)),
                        _ => return None,
                    },
                    _ => return None,
                },
                _ => return None,
            };
            let i = match v {
                SqlValue::Int64(i) => i,
                SqlValue::Int32(i) => i as i64,
                _ => {
                    eprintln!("[range-probe] literal bail: v={v:?} other={other:?}");
                    return None;
                }
            };
            let lower = matches!(op, Op::Gt | Op::GtEq);
            let incl = matches!(op, Op::GtEq | Op::LtEq);
            if col_first == lower {
                // col 在左且是下界（>/>=），或 col 在右且是上界（</<=）
                Some((Some((i, incl)), None))
            } else {
                // col 在左且是上界（</<=），或 col 在右且是下界（>/>=）
                Some((None, Some((i, incl))))
            }
        }
        _ => None,
    }
}

/// 范围界 → 树键界（encode_key 对整数是 8B 定宽保序：值域 ±1 即键域 ±1）。
/// hi 一律转排他（range_scan 端点排他）；溢出 = 该侧无界（由下游过滤兜底）。
pub(crate) fn pk_range_keys(
    lo: &Option<(i64, bool)>,
    hi: &Option<(i64, bool)>,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let start = match lo {
        Some((v, true)) => Some(crate::format::row::encode_key(&[SqlValue::Int64(*v)])),
        Some((v, false)) => v
            .checked_add(1)
            .map(|x| crate::format::row::encode_key(&[SqlValue::Int64(x)])),
        None => None,
    };
    let end = match hi {
        Some((v, true)) => v
            .checked_add(1)
            .map(|x| crate::format::row::encode_key(&[SqlValue::Int64(x)])),
        Some((v, false)) => Some(crate::format::row::encode_key(&[SqlValue::Int64(*v)])),
        None => None,
    };
    (start, end)
}

/// 判定 WHERE 是否为 pk 直查形态
pub(crate) fn try_pk_pushdown(
    db: &Database,
    sess: &mut Session,
    tf: &TableFactor,
    selection: Option<&Expr>,
    _snapshot: u64,
) -> Result<Option<(crate::versioned::TableSchema, crate::versioned::TableEntry)>> {
    let name = match tf {
        TableFactor::Table { name, version, .. } => {
            // P1-10：PK 直查读 memtx ∪ 当前树——无历史根概念。带版本子句的
            // 查询必须回落 table_scan 的 time travel 路径（此前 version 被
            // 忽略 → 历史点查静默返回当前数据 + 泄漏在途 memtx 行）
            if version.is_some() {
                return Ok(None);
            }
            name.0
                .iter()
                .filter_map(|p| p.as_ident())
                .map(|i| i.value.clone())
                .collect::<Vec<_>>()
                .join(".")
        }
        _ => return Ok(None),
    };
    let low = name.to_ascii_lowercase();
    if matches!(
        low.as_str(),
        "cambium.branches"
            | "branches"
            | "information_schema.tables"
            | "pg_catalog.pg_tables"
            | "pg_tables"
            | "information_schema.columns"
            | "pg_catalog.pg_columns"
            | "pg_columns"
            | "pg_catalog.pg_settings"
            | "pg_settings"
            | "cambium.commit_log"
            | "commit_log"
    ) {
        return Ok(None);
    }
    let Some(sel) = selection else {
        return Ok(None);
    };
    // 单列主键 + 顶层 Eq/IN 形态才走直查
    // （显式事务：以 BEGIN 冻结的 catalog 根解析，R7-3）
    let frozen = match &sess.txn {
        Some(t) if t.explicit => t.head_root,
        _ => None,
    };
    let (schema, entry) = match frozen {
        Some(r) => resolve_table_at(
            db,
            Some(&r),
            name.rsplit(['.', '@']).next().unwrap_or(&name),
        )?,
        None => match resolve_table(db, &sess.branch, &name) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        },
    };
    if schema.pk.len() != 1 {
        return Ok(None);
    }
    let pk_name = schema.columns[schema.pk[0] as usize].name.clone();
    let ok = match sel {
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::Eq,
            right,
        } => is_col_vs_value(left, right, &pk_name) || is_col_vs_value(right, left, &pk_name),
        // 账本 #20：资格判定必须与取键同拒 negated——NOT IN 在此放行则
        // 取键产出空集，点查静默变 0 行（附录 A 实证）。回落通用谓词路径。
        Expr::InList { expr, negated, .. } if !*negated => match expr.as_ref() {
            Expr::Identifier(id) => id.value.eq_ignore_ascii_case(&pk_name),
            _ => false,
        },
        _ => false,
    };
    Ok(if ok { Some((schema, entry)) } else { None })
}

pub(crate) fn is_col_vs_value(a: &Expr, b: &Expr, pk: &str) -> bool {
    match (a, b) {
        (Expr::Identifier(id), Expr::Value(_)) => id.value.eq_ignore_ascii_case(pk),
        (Expr::Value(_), Expr::Identifier(id)) => id.value.eq_ignore_ascii_case(pk),
        _ => false,
    }
}

/// pk 直查视图：memtx ∪ 树
/// 表达式 → 字面量 SqlValue（S-3 负数审计 R8）：`Value` 与
/// `UnaryOp::Minus(数值)` 两类；其余（列引用/函数/算式）→ None。
/// 此前 Eq/IN 直查只认 `Expr::Value`——负数字面量被静默过滤成
/// 空键集（`id = -3` 恰好有非下推回退路径兜底，`IN (-3, 4)` 则
/// **静默丢行**）。
pub(crate) fn expr_to_literal(e: &Expr) -> Option<SqlValue> {
    match e {
        Expr::Value(vws) => Some(super::expr::value_from_parser(vws.value.clone())),
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr: inner,
        } => match super::expr::value_from_parser(match inner.as_ref() {
            Expr::Value(vws) => vws.value.clone(),
            _ => return None,
        }) {
            SqlValue::Int64(i) => Some(SqlValue::Int64(-i)),
            SqlValue::Int32(i) => Some(SqlValue::Int32(-i)),
            SqlValue::Float64(f) => Some(SqlValue::Float64(-f)),
            _ => None,
        },
        _ => None,
    }
}

/// pk 等值谓词 → 行键（单键形态；IN/非等值由调用方回落）
pub(crate) fn point_key_of(
    pred: &Expr,
    schema: &crate::versioned::TableSchema,
) -> Result<Vec<u8>> {
    use sqlparser::ast::{BinaryOperator, Expr};
    let pk_name = schema.columns[schema.pk[0] as usize].name.clone();
    let v: &Expr = match pred {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            if is_col_vs_value(left, right, &pk_name) {
                right.as_ref()
            } else if is_col_vs_value(right, left, &pk_name) {
                left.as_ref()
            } else {
                return Err(SqlError::internal("point_key_of: not pk eq"));
            }
        }
        // IN(单值) = 等值（optimize 侧 rewrite 前的形态）
        Expr::InList {
            expr,
            list,
            negated: false,
        } if list.len() == 1 => {
            let is_pk = match expr.as_ref() {
                Expr::Identifier(id) => id.value.eq_ignore_ascii_case(&pk_name),
                _ => false,
            };
            if !is_pk {
                return Err(SqlError::internal("point_key_of: not pk in"));
            }
            &list[0]
        }
        _ => return Err(SqlError::internal("point_key_of: not eq")),
    };
    let lit = expr_to_literal(v)
        .ok_or_else(|| SqlError::internal("point_key_of: non-literal"))?;
    Ok(crate::format::row::encode_key(std::slice::from_ref(&lit)))
}

/// 字节级单键点取（try_point_early 的免中转形态：不解码、不建
/// TableView——调用方按需解码投影列）。语义与 build_point_view
/// 的单键路径一致：memtx ∪ 树、墓碑隐藏、事务自身写。
pub(crate) fn fetch_row_bytes(
    db: &Database,
    sess: &Session,
    entry: &crate::versioned::TableEntry,
    key: &[u8],
    snapshot: u64,
) -> Result<Option<std::sync::Arc<Vec<u8>>>> {
    let b = db.branch(&sess.branch)?;
    let tm = b.mem.table(entry.id);
    let mut found: Option<std::sync::Arc<Vec<u8>>> = match tm.get(key, snapshot) {
        Some(v) => Some(v),
        None => {
            let tombstoned = tm.latest_ts(key).is_some_and(|ts| ts <= snapshot);
            if tombstoned {
                None
            } else {
                entry
                    .table_root
                    .as_ref()
                    .and_then(|s| crate::format::hash::Hash::from_base32(s))
                    .map(|r| crate::prolly::cursor::lookup(&db.store, &r, key))
                    .transpose()?
                    .flatten()
                    .map(std::sync::Arc::new)
            }
        }
    };
    if let Some(t) = &sess.txn {
        if t.explicit {
            if let Some(m) = t.writes.get(&(entry.id, key.to_vec())) {
                match m {
                    crate::prolly::Mutation::Put(v) => {
                        found = Some(std::sync::Arc::new(v.clone()))
                    }
                    crate::prolly::Mutation::Delete => found = None,
                }
            }
        }
    }
    Ok(found)
}

pub(crate) fn build_point_view(
    db: &Database,
    sess: &mut Session,
    schema: &crate::versioned::TableSchema,
    entry: &crate::versioned::TableEntry,
    selection: &Expr,
    snapshot: u64,
) -> Result<TableView> {
    use sqlparser::ast::{BinaryOperator, Expr};
    let pk_name = schema.columns[schema.pk[0] as usize].name.clone();
    let keys: Vec<SqlValue> = match selection {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let v: &Expr = if is_col_vs_value(left, right, &pk_name) {
                right.as_ref()
            } else {
                left.as_ref()
            };
            expr_to_literal(v).into_iter().collect()
        }
        Expr::InList {
            expr,
            list,
            negated,
        } if !*negated => {
            let _ = expr;
            list.iter().filter_map(expr_to_literal).collect()
        }
        _ => vec![],
    };
    let b = db.branch(&sess.branch)?;
    let tm = b.mem.table(entry.id);
    let tree_root = entry
        .table_root
        .as_ref()
        .and_then(|s| crate::format::hash::Hash::from_base32(s));
    let mut rows = Vec::with_capacity(keys.len());
    for pkv in keys {
        let key = crate::format::row::encode_key(std::slice::from_ref(&pkv));
        // memtx 优先；**墓碑判定**（第七轮 R7-1 伴生①）：memtx 无值时可能是
        // "可见墓碑"（该 key 在快照前已被删除）——此时不得回退树复活被删行
        let mut found: Option<Arc<Vec<u8>>> = match tm.get(&key, snapshot) {
            Some(v) => Some(v),
            None => {
                let tombstoned = tm.latest_ts(&key).is_some_and(|ts| ts <= snapshot);
                if tombstoned {
                    None
                } else {
                    match &tree_root {
                        Some(r) => crate::prolly::cursor::lookup(&db.store, r, &key)?.map(Arc::new),
                        None => None,
                    }
                }
            }
        };
        // 会话显式事务自身写（R8-1）：点查同样读自己的写
        if let Some(t) = &sess.txn {
            if t.explicit {
                if let Some(m) = t.writes.get(&(entry.id, key.clone())) {
                    match m {
                        crate::prolly::Mutation::Put(v) => found = Some(Arc::new(v.clone())),
                        crate::prolly::Mutation::Delete => found = None,
                    }
                }
            }
        }
        if let Some(v) = found {
            rows.push(row_from_bytes(schema, &v)?);
        }
    }
    let names = schema.columns.iter().map(|c| c.name.clone()).collect();
    Ok(TableView { names, rows })
}

// ---------- time travel（P1-10）----------
//
// 语法：`FROM <table> FOR SYSTEM_TIME AS OF '<ts_ms | ISO8601 | commit_hash>' [AS alias]`
// 语义（Dolt 式提交快照，非逐行系统版本）：
// - 提交哈希：该 commit 的物化树根原样读（commit = checkpoint = 完整态）；
// - 时间戳：沿分支**第一父链**找 ts_ms ≤ 目标的最近提交——链跨分支边界
//   （fork 继承源提交），故可回溯到 fork 之前；早于首个提交 → 22023。
// - 快照是**只读历史树**：无 memtx overlay、无会话写、无 checkpoint 推进
//   （历史树 chunk 不可变，天然稳定）。checkpoint 之后的未物化事务不在任何
//   提交里 ⇒ 时间旅行精度 = 提交粒度（文档口径，SPEC 03 §5）。
