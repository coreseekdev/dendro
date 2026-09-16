#![allow(unused_imports)] // 统一复制主模块导入块（阶段0 拆分：纯移动）
//! hash join（内/左）与等值对提取。

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

pub(crate) fn hash_join(
    l: TableView,
    r: TableView,
    on: &Expr,
    deadline: Option<std::time::Instant>,
    build_select: bool,
    llay: Option<&FactorLayout>,
    rkey: Option<&str>,
) -> Result<TableView> {
    // 找等值条件 col_l = col_r（支持 AND 链中提取多个；#22 残留合取回收）
    let (eqs, residual) = extract_equi(on, &l.names, &r.names, llay, rkey)?;
    let mut names = l.names.clone();
    names.extend(r.names.clone());
    // 列名克隆进闭包持有——避免借用 names 阻碍结尾 TableView 移动；
    // #30 同族修：residual 限定名按布局消歧（原裸末段首匹配曾使
    // acc-vs-acc 残留 `a.x = b.y` 同名自比较恒真——join 条件失效）
    let owned = names.clone();
    let lay_owned = llay.cloned().unwrap_or_default();
    let res_cols = move |n: &str| resolve_qualified(&lay_owned, &owned, n);
    // 建右表哈希（文本键：类型内规范）
    let mkkey = |row: &Vec<SqlValue>, idx: &[usize]| -> Option<Vec<String>> {
        let mut k = Vec::with_capacity(idx.len());
        for &i in idx {
            let v = row.get(i)?;
            if v.is_null() {
                return None;
            }
            // 类型 tag + 文本：防 Int64(1) 与 Utf8("1") 碰撞
            k.push(format!(
                "{}\u{0}{}",
                v.type_name(),
                expr::to_text(v.clone())
            ));
        }
        Some(k)
    };
    // O-4（spec 12 §4）：INNER join 构建侧按**实际基数**选择——join 时
    // 两侧均已扫描/下推过滤，行数是精确值；小侧建哈希表（内存 O(min)
    // 替代 O(right)，probe 侧流式）。输出行恒 left++right（列序与
    // 因子布局/#28 解析不变），仅产出序随 probe 侧变化——多重集恒等。
    // build_select=false（optimize off 差分轴）= 原固定建右行为。
    let build_left = build_select && l.rows.len() < r.rows.len();
    let (build_rows, build_idx, probe_rows, probe_idx) = if build_left {
        (&l.rows, &eqs.lidx, &r.rows, &eqs.ridx)
    } else {
        (&r.rows, &eqs.ridx, &l.rows, &eqs.lidx)
    };
    let mut hm: HashMap<Vec<String>, Vec<&Vec<SqlValue>>> = HashMap::new();
    // SOTA P2：Join Filter Pushdown——build 侧键实际 [min,max]（非统计
    // 近似——build 侧已含全部过滤效果的最窄区间）。probe 行先做 O(1)
    // 范围检查，界外跳过（免 format! 键字符串 + 哈希查找——DuckDB
    // blog 2024-11 的 build-side min/max → probe-side filter 同构）
    let nkeys = build_idx.len();
    let mut key_min: Vec<Option<SqlValue>> = vec![None; nkeys];
    let mut key_max: Vec<Option<SqlValue>> = vec![None; nkeys];
    let mut hj_chk = 0usize;
    for br in build_rows {
        hj_chk += 1;
        if hj_chk.is_multiple_of(4096) {
            if let Some(d) = deadline {
                if std::time::Instant::now() >= d {
                    return Err(SqlError::new("57014", "statement timeout"));
                }
            }
        }
        if let Some(key) = mkkey(br, build_idx) {
            // 键 min/max 追踪（非 NULL 行——与 mkkey 的 NULL 跳过同步）
            for (i, &bi) in build_idx.iter().enumerate() {
                if let Some(v) = br.get(bi) {
                    if !v.is_null() {
                        let less = match &key_min[i] {
                            None => true,
                            Some(m) => expr::cmp_values(v, m)
                                .map(|o| o == std::cmp::Ordering::Less)
                                .unwrap_or(false),
                        };
                        if less {
                            key_min[i] = Some(v.clone());
                        }
                        let greater = match &key_max[i] {
                            None => true,
                            Some(m) => expr::cmp_values(v, m)
                                .map(|o| o == std::cmp::Ordering::Greater)
                                .unwrap_or(false),
                        };
                        if greater {
                            key_max[i] = Some(v.clone());
                        }
                    }
                }
            }
            hm.entry(key).or_default().push(br);
        }
    }
    // 范围有效判定：全部键列都有 min/max（空 build 侧或全 NULL → 全
    // None → 跳过过滤——直接进哈希查找得到正确空结果）
    let has_range = key_min.iter().zip(&key_max).all(|(mn, mx)| mn.is_some() && mx.is_some());
    let mut rows = Vec::new();
    for pr in probe_rows {
        // Join Filter Pushdown：O(1) 范围检查先于 O(k) 键构造 + O(1)
        // 哈希查找。比较错误（类型混列）按"在范围内"处理——后续哈希
        // 查找会按完整语义裁决（保守——不提前误丢行）
        if has_range {
            let mut in_range = true;
            for (i, &pi) in probe_idx.iter().enumerate() {
                if let Some(v) = pr.get(pi) {
                    if !v.is_null() {
                        let below = match &key_min[i] {
                            Some(m) => expr::cmp_values(v, m)
                                .map(|o| o == std::cmp::Ordering::Less)
                                .unwrap_or(false),
                            None => false,
                        };
                        let above = match &key_max[i] {
                            Some(m) => expr::cmp_values(v, m)
                                .map(|o| o == std::cmp::Ordering::Greater)
                                .unwrap_or(false),
                            None => false,
                        };
                        if below || above {
                            in_range = false;
                            break;
                        }
                    }
                }
            }
            if !in_range {
                continue; // 界外——不可能匹配（省 format!/哈希）
            }
        }
        let key = match mkkey(pr, probe_idx) {
            Some(k) => k,
            None => continue,
        };
        if let Some(matches) = hm.get(&key) {
            for br in matches {
                // 输出列序恒 left++right（build 侧决定拼装方向）
                let row: Vec<SqlValue> = if build_left {
                    let mut row: Vec<SqlValue> = (**br).clone();
                    row.extend(pr.iter().cloned());
                    row
                } else {
                    let mut row = pr.clone();
                    row.extend(br.iter().cloned());
                    row
                };
                // #22：残留合取逐候选对求值（INNER：不成立即丢弃）
                if !residual_holds(&residual, &row, &res_cols)? {
                    continue;
                }
                rows.push(row);
            }
        }
    }
    Ok(TableView { names, rows })
}

pub(crate) fn hash_join_left(
    l: TableView,
    r: TableView,
    on: &Expr,
    deadline: Option<std::time::Instant>,
    llay: Option<&FactorLayout>,
    rkey: Option<&str>,
) -> Result<TableView> {
    // #30 消歧与 INNER 同款（布局 + 右键——曾只修 INNER，LEFT 的多因子
    // 左限定名仍裸末段误中）
    let (eqs, residual) = extract_equi(on, &l.names, &r.names, llay, rkey)?;
    let mut names = l.names.clone();
    names.extend(r.names.clone());
    // 列名克隆进闭包持有——避免借用 names 阻碍结尾 TableView 移动
    //（LEFT 侧布局未接线——residual 裸名回退，与 AST 路径同口径）
    let owned = names.clone();
    let res_cols = col_lookup(&owned);
    // #21：类型标签键（与 INNER 版同型）——裸 to_text 曾使 NULL↔NULL、
    // Int64(1)↔Utf8("1") 碰撞；NULL 分量 = 永不匹配（SQL 等值 NULL 语义）
    let mkkey = |row: &Vec<SqlValue>, idx: &[usize]| -> Option<Vec<String>> {
        let mut k = Vec::with_capacity(idx.len());
        for &i in idx {
            let v = row.get(i)?;
            if v.is_null() {
                return None;
            }
            k.push(format!(
                "{}{}{}",
                v.type_name(),
                NUL_PLACEHOLDER,
                expr::to_text(v.clone())
            ));
        }
        Some(k)
    };
    let mut hm: HashMap<Vec<String>, Vec<&Vec<SqlValue>>> = HashMap::new();
    let mut hj_chk = 0usize;
    for rr in &r.rows {
        hj_chk += 1;
        if hj_chk.is_multiple_of(4096) {
            if let Some(d) = deadline {
                if std::time::Instant::now() >= d {
                    return Err(SqlError::new("57014", "statement timeout"));
                }
            }
        }
        if let Some(key) = mkkey(rr, &eqs.ridx) {
            hm.entry(key).or_default().push(rr);
        }
    }
    let null_right = vec![SqlValue::Null; r.names.len()];
    let mut rows = Vec::new();
    for lr in &l.rows {
        // 探侧 NULL 键 → 无匹配 → NULL 延展（LEFT 语义）
        let matched = match mkkey(lr, &eqs.lidx) {
            None => None,
            Some(k) => hm.get(&k),
        };
        match matched {
            Some(ms) => {
                let mut any = false;
                for rr in ms {
                    let mut row = lr.clone();
                    row.extend(rr.iter().cloned());
                    // #22：残留合取；全部候选不成立 → 仍按无匹配 NULL 延展
                    if !residual_holds(&residual, &row, &res_cols)? {
                        continue;
                    }
                    any = true;
                    rows.push(row);
                }
                if !any {
                    let mut row = lr.clone();
                    row.extend(null_right.iter().cloned());
                    rows.push(row);
                }
            }
            None => {
                let mut row = lr.clone();
                row.extend(null_right.iter().cloned());
                rows.push(row);
            }
        }
    }
    Ok(TableView { names, rows })
}

/// 限定名消歧列定位（#30）：前缀必须命中本侧因子（左 = 布局区间
/// 精确解析；右 = 单因子键），命中即定位、列不在因子内不回退；前缀
/// 不属本侧 → None（**不得**裸末段回退——曾使 `c.id` 误中右表 o.id
/// / 左表 r.id，join 键错位——重排差分实证）
pub(crate) fn col_pos_lay(
    e: &Expr,
    names: &[String],
    layout: Option<&FactorLayout>,
    rkey: Option<&str>,
) -> Option<usize> {
    match e {
        Expr::Identifier(id) => {
            let low = id.value.to_ascii_lowercase();
            names.iter().position(|n| n.to_ascii_lowercase() == low)
        }
        Expr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                let prefix = parts[0].value.to_ascii_lowercase();
                let col = parts[1].value.to_ascii_lowercase();
                if let Some(lay) = layout {
                    if let Some((_, start, len, local)) =
                        lay.iter().find(|(k, _, _, _)| *k == prefix)
                    {
                        return local
                            .iter()
                            .position(|n| *n == col)
                            .map(|i| start + i)
                            .filter(|abs| *abs < start + len);
                    }
                }
                if let Some(rk) = rkey {
                    if prefix == rk {
                        return names
                            .iter()
                            .position(|n| n.to_ascii_lowercase() == col);
                    }
                    return None; // 前缀不属右侧因子
                }
            }
            // 无布局无键（历史调用面）——末段回退
            let last = parts.last()?.value.to_ascii_lowercase();
            names.iter().position(|n| n.to_ascii_lowercase() == last)
        }
        _ => None,
    }
}

pub(crate) struct EquiIdx {
    lidx: Vec<usize>,
    ridx: Vec<usize>,
}

/// 从 ON 条件提取 `l.c = r.c` 等值对（AND 链）
pub(crate) fn extract_equi(
    e: &Expr,
    ln: &[String],
    rn: &[String],
    llay: Option<&FactorLayout>,
    rkey: Option<&str>,
) -> Result<(EquiIdx, Vec<Expr>)> {
    let mut lidx = Vec::new();
    let mut ridx = Vec::new();
    let mut residual: Vec<Expr> = Vec::new();
    // 账本 #22：AND 链中非等值合取曾**静默丢弃**（子节点返回值被无视）。
    // 改为收集残留、逐候选对求值（LEFT：残留不成立=该对不匹配→NULL 延展）
    #[allow(clippy::too_many_arguments)] // 消歧上下文五元组——内聚于
    // 递归闭包不可拆（拆参结构反而增加跨闭包状态）
    fn walk(
        e: &Expr,
        ln: &[String],
        rn: &[String],
        li: &mut Vec<usize>,
        ri: &mut Vec<usize>,
        residual: &mut Vec<Expr>,
        llay: Option<&FactorLayout>,
        rkey: Option<&str>,
    ) -> Result<bool> {
        match e {
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::And,
                right,
            } => {
                // #22：不可提取的子式由**父节点**收集（子式自身只在叶子报
                // false）；AND 恒真当且仅当全部子式可提取
                let a = walk(left, ln, rn, li, ri, residual, llay, rkey)?;
                if !a {
                    residual.push((**left).clone());
                }
                let b = walk(right, ln, rn, li, ri, residual, llay, rkey)?;
                if !b {
                    residual.push((**right).clone());
                }
                Ok(a && b)
            }
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::Eq,
                right,
            } => {
                // 两边各解析出一列：一属左表一属右表（限定名按侧消歧：
                // 左侧布局区间 / 右侧单因子键——#30）
                let le = col_pos_lay(left, ln, llay, None);
                let re = col_pos_lay(right, rn, None, rkey);
                let lo = col_pos_lay(left, rn, None, rkey);
                let ro = col_pos_lay(right, ln, llay, None);
                if let (Some(a), Some(b)) = (le, re) {
                    li.push(a);
                    ri.push(b);
                    Ok(true)
                } else if let (Some(a), Some(b)) = (lo, ro) {
                    li.push(b);
                    ri.push(a);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }
    let _ = walk(e, ln, rn, &mut lidx, &mut ridx, &mut residual, llay, rkey)?;
    // 顶层整体不可提取且无任何等值对 → 原错误语义（顶层非等值条件）
    if lidx.is_empty() {
        return Err(SqlError::not_supported(
            "JOIN ON: only equi-conditions supported",
        ));
    }
    Ok((EquiIdx { lidx, ridx }, residual))
}

/// 残留合取求值（#22）：组合行上求值，全部 Bool(true) 才匹配（终结
/// 语义与 WHERE 一致：仅 true 放行）
/// 键内分隔符（NUL 字节，类型名与文本之间——附录 A 键编码）
pub(crate) const NUL_PLACEHOLDER: &str = "\u{0}";

pub(crate) fn residual_holds(
    residual: &[Expr],
    combined: &[SqlValue],
    cols: &dyn Fn(&str) -> Option<usize>,
) -> Result<bool> {
    for e in residual {
        if !matches!(expr::eval(e, combined, cols), Ok(SqlValue::Bool(true))) {
            return Ok(false);
        }
    }
    Ok(true)
}

// ---------- 投影/聚合 ----------
