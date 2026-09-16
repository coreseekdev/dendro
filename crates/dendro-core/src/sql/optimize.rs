//! 优化器 v1（O-1，spec 12）：表达式级规则 + 计划级合取下推。
//!
//! 表达式级：AST 自底向上常量折叠 + 布尔简化 + NOT 消除（R3）。
//! 计划级：R1 合取拆分 + R2 单源合取下推分类（应用点在 eval_from）。
//! 规则合同（确定性/语义保持/差分可枚举/EXPLAIN 可见）见 spec 12 §1。

use sqlparser::ast::{BinaryOperator as BinOp, Expr, UnaryOperator as UnOp};

/// 优化 SQL 表达式（递归自底向上，语义保持）
pub fn optimize(e: &Expr) -> Expr {
    match e {
        Expr::BinaryOp { left, op, right } => {
            let l = optimize(left);
            let r = optimize(right);
            fold_binary(op, &l, &r).unwrap_or(Expr::BinaryOp {
                left: Box::new(l),
                op: op.clone(),
                right: Box::new(r),
            })
        }
        Expr::UnaryOp { op, expr } => {
            let inner = optimize(expr);
            fold_unary(*op, &inner).unwrap_or(Expr::UnaryOp {
                op: *op,
                expr: Box::new(inner),
            })
        }
        Expr::Nested(inner) => optimize(inner),
        Expr::IsNull(inner) => Expr::IsNull(Box::new(optimize(inner))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(optimize(inner))),
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(optimize(expr)),
            list: list.iter().map(optimize).collect(),
            negated: *negated,
        },
        _ => e.clone(),
    }
}

/// 二元运算常量折叠（整数算术）
fn fold_binary(op: &BinOp, l: &Expr, r: &Expr) -> Option<Expr> {
    match op {
        BinOp::And | BinOp::Or => return fold_bool(op, l, r),
        _ => {}
    }
    let (lv, rv) = match (extract_int(l), extract_int(r)) {
        (Some(a), Some(b)) => (a, b),
        _ => return None,
    };
    let result = match op {
        BinOp::Plus => lv.checked_add(rv),
        BinOp::Minus => lv.checked_sub(rv),
        BinOp::Multiply => lv.checked_mul(rv),
        _ => return None,
    }?;
    Some(int_expr(result))
}

fn extract_int(e: &Expr) -> Option<i64> {
    match e {
        Expr::Value(vws) => match &vws.value {
            sqlparser::ast::Value::Number(n, _) => n.parse::<i64>().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn int_expr(v: i64) -> Expr {
    Expr::Value(sqlparser::ast::ValueWithSpan {
        value: sqlparser::ast::Value::Number(v.to_string(), false),
        span: sqlparser::tokenizer::Span::empty(),
    })
}

/// 布尔简化（短路恒等）
fn fold_bool(op: &BinOp, l: &Expr, r: &Expr) -> Option<Expr> {
    match op {
        BinOp::And => {
            if is_true(l) {
                Some(r.clone())
            } else if is_true(r) {
                Some(l.clone())
            } else if is_false(l) || is_false(r) {
                Some(false_expr())
            } else {
                None
            }
        }
        BinOp::Or => {
            if is_false(l) {
                Some(r.clone())
            } else if is_false(r) {
                Some(l.clone())
            } else if is_true(l) || is_true(r) {
                Some(true_expr())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn is_true(e: &Expr) -> bool {
    matches!(e, Expr::Value(vws) if matches!(vws.value, sqlparser::ast::Value::Boolean(true)))
}
fn is_false(e: &Expr) -> bool {
    matches!(e, Expr::Value(vws) if matches!(vws.value, sqlparser::ast::Value::Boolean(false)))
}
fn true_expr() -> Expr {
    Expr::Value(sqlparser::ast::ValueWithSpan {
        value: sqlparser::ast::Value::Boolean(true),
        span: sqlparser::tokenizer::Span::empty(),
    })
}
fn false_expr() -> Expr {
    Expr::Value(sqlparser::ast::ValueWithSpan {
        value: sqlparser::ast::Value::Boolean(false),
        span: sqlparser::tokenizer::Span::empty(),
    })
}

/// 一元运算折叠
fn fold_unary(op: UnOp, inner: &Expr) -> Option<Expr> {
    match op {
        UnOp::Minus => match inner {
            Expr::Value(vws) => match &vws.value {
                sqlparser::ast::Value::Number(n, _) => {
                    let v: i64 = n.parse().ok()?;
                    Some(int_expr(-v))
                }
                _ => None,
            },
            Expr::UnaryOp {
                op: UnOp::Minus,
                expr: inner2,
            } => Some((**inner2).clone()),
            _ => None,
        },
        UnOp::Plus => Some(inner.clone()),
        // R3 NOT 消除：NOT NOT x → x；NOT 字面量折叠（原 fold_bool
        // 只覆盖 AND/OR 侧）
        UnOp::Not => match inner {
            Expr::UnaryOp {
                op: UnOp::Not,
                expr: inner2,
            } => Some((**inner2).clone()),
            Expr::Value(vws) => match &vws.value {
                sqlparser::ast::Value::Boolean(b) => Some(bool_expr(!b)),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn bool_expr(b: bool) -> Expr {
    Expr::Value(sqlparser::ast::ValueWithSpan {
        value: sqlparser::ast::Value::Boolean(b),
        span: sqlparser::tokenizer::Span::empty(),
    })
}

// ---------------------------------------------------------------------------
// R1/R2：合取拆分 + 单源下推分类（spec 12 §2）
// ---------------------------------------------------------------------------

/// R1：WHERE 谓词拆成合取项（穿透 Nested；OR 不拆——析取保持原子）
pub fn split_conjuncts(e: &Expr) -> Vec<Expr> {
    match e {
        Expr::BinaryOp {
            op: BinOp::And,
            left,
            right,
        } => {
            let mut out = split_conjuncts(left);
            out.extend(split_conjuncts(right));
            out
        }
        Expr::Nested(inner) => split_conjuncts(inner),
        other => vec![other.clone()],
    }
}

/// 表因子键（下推目标的标识）：别名优先，无别名用表短名（小写）
pub fn factor_key(tf: &sqlparser::ast::TableFactor) -> Option<String> {
    match tf {
        sqlparser::ast::TableFactor::Table {
            name, alias, ..
        } => {
            let base = name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(|i| i.value.to_ascii_lowercase())?;
            let key = alias
                .as_ref()
                .map(|a| a.name.value.to_ascii_lowercase())
                .unwrap_or(base);
            Some(key)
        }
        sqlparser::ast::TableFactor::Derived { alias, .. } => {
            // 派生表以 alias 名入布局（限定名 `d.col` 解析用；递归 CTE
            // 注入的 `(VALUES ...) AS r(col,...)` 依赖此键）
            alias.as_ref().map(|a| a.name.value.to_ascii_lowercase())
        }
        _ => None, // 其余因子 v1 不作下推目标
    }
}

/// 收集表达式内的标识符（限定名保前缀；CompoundIdentifier 取全路径）
pub(crate) fn expr_idents_pub(e: &Expr, out: &mut Vec<String>) {
    expr_idents(e, out)
}

fn expr_idents(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Identifier(id) => out.push(id.value.to_ascii_lowercase()),
        Expr::CompoundIdentifier(parts) => {
            let path = parts
                .iter()
                .map(|i| i.value.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(".");
            if !path.is_empty() {
                out.push(path);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_idents(left, out);
            expr_idents(right, out);
        }
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) | Expr::IsTrue(expr) | Expr::IsFalse(expr) => {
            expr_idents(expr, out)
        }
        Expr::InList { expr, list, .. } => {
            expr_idents(expr, out);
            for i in list {
                expr_idents(i, out);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_idents(expr, out);
            expr_idents(low, out);
            expr_idents(high, out);
        }
        Expr::Cast { expr, .. } => expr_idents(expr, out),
        // CASE：条件/结果均可能引用列（原缺失——裁剪掉 CASE 引用列会
        // 静默错值，O-3 差分补齐）
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                expr_idents(o, out);
            }
            for cw in conditions {
                expr_idents(&cw.condition, out);
                expr_idents(&cw.result, out);
            }
            if let Some(e) = else_result {
                expr_idents(e, out);
            }
        }
        Expr::Function(f) => {
            if let sqlparser::ast::FunctionArguments::List(l) = &f.args {
                for a in &l.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(e),
                    ) = a
                    {
                        expr_idents(e, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// R2：合取项的下推归属。Some(key) = 全部标识符限定且前缀同因子；
/// None = 留在 join 后（裸列名歧义 / 跨表 / 含非表因子引用）。
pub fn conjunct_target(conjunct: &Expr, factor_keys: &[String]) -> Option<String> {
    let mut idents = Vec::new();
    expr_idents(conjunct, &mut idents);
    if idents.is_empty() {
        return None; // 纯常量项：Q-1 短路已处理，不下推
    }
    let mut target: Option<String> = None;
    for id in &idents {
        let Some((prefix, _col)) = id.split_once('.') else {
            return None; // 裸列名：v1 保守不消解（spec 12 §2）
        };
        if !factor_keys.iter().any(|k| k == prefix) {
            return None; // 前缀不是本查询的因子（列别名等）——不推
        }
        match &target {
            None => target = Some(prefix.to_string()),
            Some(t) if t == prefix => {}
            _ => return None, // 跨表
        }
    }
    target
}

/// 合取项重组（单元素直返；空 = None）
pub fn and_all(mut cs: Vec<Expr>) -> Option<Expr> {
    if cs.is_empty() {
        return None;
    }
    let mut acc = cs.remove(0);
    for c in cs {
        acc = Expr::BinaryOp {
            left: Box::new(acc),
            op: BinOp::And,
            right: Box::new(c),
        };
    }
    Some(acc)
}

// ---------------------------------------------------------------------------
// O-3：投影裁剪——列需求位图（AP 列存段跳列解码的依据）
// ---------------------------------------------------------------------------

/// 单表查询的列需求位图（true = 需要）。**fail-open**：任何不确定
/// （通配投影 / 未知名 / 不可解析形态）→ None = 全解码——裁剪只在
/// 确定无损时发生。pk 列恒保留（归并键/点查下推依赖）。
pub fn column_mask(
    select: &sqlparser::ast::Select,
    order_exprs: &[sqlparser::ast::OrderByExpr],
    schema: &crate::versioned::TableSchema,
) -> Option<Vec<bool>> {
    let ncols = schema.columns.len();
    // 通配投影 = 全列
    for item in &select.projection {
        if matches!(
            item,
            sqlparser::ast::SelectItem::Wildcard(_) | sqlparser::ast::SelectItem::QualifiedWildcard(..)
        ) {
            return None;
        }
    }
    let mut idents = Vec::new();
    for item in &select.projection {
        match item {
            sqlparser::ast::SelectItem::UnnamedExpr(e) => expr_idents(e, &mut idents),
            sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } => {
                expr_idents(expr, &mut idents)
            }
            _ => return None,
        }
    }
    if let Some(w) = &select.selection {
        expr_idents(w, &mut idents);
    }
    if let sqlparser::ast::GroupByExpr::Expressions(es, _) = &select.group_by {
        for e in es {
            expr_idents(e, &mut idents);
        }
    }
    if let Some(h) = &select.having {
        expr_idents(h, &mut idents);
    }
    for o in order_exprs {
        expr_idents(&o.expr, &mut idents);
    }
    let mut mask = vec![false; ncols];
    let mut any = false;
    for id in &idents {
        // 限定名取末段（单表：限定前缀必为本表别名/表名）
        let bare = id.rsplit('.').next().unwrap_or(id);
        // 未知名（别名引用/序数等）——保守放弃（`?` 直返 None）
        let i = schema
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(bare))?;
        mask[i] = true;
        any = true;
    }
    for &pk in &schema.pk {
        if let (i, true) = (pk as usize, (pk as usize) < ncols) {
            mask[i] = true;
            any = true;
        }
    }
    if !any || mask.iter().all(|&b| b) {
        return None; // 无列需求（异常）或全需求——不裁剪
    }
    Some(mask)
}

// ---------------------------------------------------------------------------
// SOTA P2：Statistics Propagation（DuckDB blog 2024-11）
// 等值 join a.x = b.y → 两列 [min,max] 区间交集 → 较窄侧的区间
// 作为对侧扫描过滤器注入（Zone Map 剪枝在 CBF 段级已有——这是
// 计划层的等价物，在无段统计的场景也生效）
// ---------------------------------------------------------------------------

/// 等值 join 键的区间传播：对每对 (acc_col, new_col)：
/// 1. 读两侧 ColStat [min, max]（order 域）
/// 2. 交集 = [max(lo_l, lo_r), min(hi_l, hi_r)]
/// 3. 若交集严格窄于任一侧 → 较宽侧的 Scan 上方注入范围过滤
pub fn rewrite_stat_prop(
    plan: &mut Plan2,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
) {
    rewrite_stat_prop_walk(plan, db, sess);
}

fn rewrite_stat_prop_walk(
    plan: &mut Plan2,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
) {
    use crate::ir::plan::Plan;
    match plan {
        Plan::Join { kind, left, right, on } => {
            rewrite_stat_prop_walk(left, db, sess);
            rewrite_stat_prop_walk(right, db, sess);
            if kind == &"inner" {
                try_propagate_ranges(left, right, on, db, sess);
            }
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::Aggregate { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input }
        | Plan::Window { input, .. } => rewrite_stat_prop_walk(input, db, sess),
        Plan::SubqueryScan { plan, .. } => rewrite_stat_prop_walk(plan, db, sess),
        Plan::Cte { plan, body, .. } => {
            rewrite_stat_prop_walk(plan, db, sess);
            rewrite_stat_prop_walk(body, db, sess);
        }
        Plan::IterativeScan { base, recursive, .. } => {
            rewrite_stat_prop_walk(base, db, sess);
            rewrite_stat_prop_walk(recursive, db, sess);
        }
        Plan::SetOp { left, right, .. } => {
            rewrite_stat_prop_walk(left, db, sess);
            rewrite_stat_prop_walk(right, db, sess);
        }
        Plan::Scan { .. } | Plan::Values => {}
    }
}

/// 对一个 join 的等值键对做区间传播
fn try_propagate_ranges(
    left: &mut Plan2,
    right: &mut Plan2,
    on: &Expr,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
) {
    // 提取 ON 的等值对（复用 join_estimate 的解析形态）
    let pairs = extract_eq_pairs(on);
    if pairs.is_empty() {
        return;
    }
    for (l_id, r_id) in pairs {
        // 定位两侧（限定名格式 "alias.col"）
        let l_parts: Vec<&str> = l_id.split('.').collect();
        let r_parts: Vec<&str> = r_id.split('.').collect();
        if l_parts.len() != 2 || r_parts.len() != 2 {
            continue;
        }
        let (l_fk, l_col) = (l_parts[0], l_parts[1]);
        let (r_fk, r_col) = (r_parts[0], r_parts[1]);

        // 读两侧统计
        let Some((l_st, _)) = stats_of_node(left, l_fk, db, sess) else {
            continue;
        };
        let Some((r_st, _)) = stats_of_node(right, r_fk, db, sess) else {
            continue;
        };
        let Some(l_cs) = col_stat(&l_st, l_col) else { continue };
        let Some(r_cs) = col_stat(&r_st, r_col) else { continue };

        // 交集（order 域——保序映射下区间交 = 值域交）
        let lo = l_cs.min.max(r_cs.min);
        let hi = l_cs.max.min(r_cs.max);
        if lo > hi {
            continue; // 区间不相交——不可能有匹配（谓词传播将产生空集，
                      // 但生成空过滤器语义危险——v1 跳过）
        }

        // 对较宽的一侧注入范围过滤（若交集严格窄于该侧原区间）
        inject_range_filter(left, l_fk, l_col, &l_cs, lo, hi);
        inject_range_filter(right, r_fk, r_col, &r_cs, lo, hi);
    }
}

/// 提取 ON 的等值对（`a.x = b.y` AND 链）
fn extract_eq_pairs(on: &Expr) -> Vec<(String, String)> {
    let mut out = Vec::new();
    fn walk(e: &Expr, out: &mut Vec<(String, String)>) {
        match e {
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::And,
                right,
            } => {
                walk(left, out);
                walk(right, out);
            }
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::Eq,
                right,
            } => {
                let mut il = Vec::new();
                let mut ir = Vec::new();
                expr_idents_pub(left, &mut il);
                expr_idents_pub(right, &mut ir);
                if let (Some(l), Some(r)) = (il.first(), ir.first()) {
                    if l.contains('.') && r.contains('.') {
                        out.push((l.clone(), r.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    walk(on, &mut out);
    out
}

/// 子树内因子键 → 该因子的 TableStats 与表名
fn stats_of_node(
    p: &Plan2,
    factor_key: &str,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
) -> Option<(crate::sql::stats::TableStats, String)> {
    use crate::ir::plan::Plan;
    fn find_scan<'a>(
        p: &'a Plan2,
        key: &str,
    ) -> Option<&'a Plan2> {
        match p {
            Plan::Scan { table, alias, .. } => {
                let k = alias.clone().unwrap_or_else(|| table.clone());
                if k == key { Some(p) } else { None }
            }
            Plan::Filter { input, .. } => find_scan(input, key),
            Plan::Join { left, right, .. } => {
                find_scan(left, key).or_else(|| find_scan(right, key))
            }
            _ => None,
        }
    }
    let scan = find_scan(p, factor_key)?;
    let Plan::Scan { table, .. } = scan else { unreachable!() };
    let st = crate::sql::stats::table_stats(db, sess, table)?;
    Some((st, table.clone()))
}

fn col_stat(st: &crate::sql::stats::TableStats, col: &str) -> Option<crate::sql::stats::ColStat> {
    let idx = st
        .names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(col))?;
    st.cols.get(idx).copied()
}

/// 在因子上方注入范围过滤（交集窄于原区间时）——Filter{Scan} 的
/// pred 追加 AND col >= lo AND col <= hi
fn inject_range_filter(
    node: &mut Plan2,
    _fk: &str,
    col: &str,
    cs: &crate::sql::stats::ColStat,
    lo: u64,
    hi: u64,
) {
    use crate::ir::plan::Plan;
    // 交集严格窄于原区间才注入
    if lo <= cs.min && hi >= cs.max {
        return; // 无收紧
    }
    // 定位 Filter{Scan}（有则追加，无则新建）
    match node {
        Plan::Filter { pred, input }
            if matches!(&**input, Plan::Scan { .. }) =>
        {
            // 追加（AND 链）
            let range = range_expr(col, lo, hi);
            *pred = Expr::BinaryOp {
                left: Box::new(pred.clone()),
                op: sqlparser::ast::BinaryOperator::And,
                right: Box::new(range),
            };
        }
        Plan::Scan { .. } => {
            // 新建 Filter{Scan}
            let range = range_expr(col, lo, hi);
            *node = Plan::Filter {
                pred: range,
                input: Box::new(std::mem::replace(node, Plan::Values)),
            };
        }
        _ => {} // 非叶子形态——v1 不注入（reorder 保证叶子，但防御）
    }
}

/// order 域区间 → SQL 范围谓词（lo/hi → i64 值域——定点解码）
fn range_expr(col: &str, lo: u64, hi: u64) -> Expr {
    // order 域 → i64（v ^ (1<<63) 的逆）
    let lo_v = (lo ^ (1u64 << 63)) as i64;
    let hi_v = (hi ^ (1u64 << 63)) as i64;
    let id = |s: &str| Expr::Identifier(sqlparser::ast::Ident::new(s));
    let num = |v: i64| {
        Expr::Value(sqlparser::ast::ValueWithSpan {
            value: sqlparser::ast::Value::Number(v.to_string(), false),
            span: sqlparser::tokenizer::Span::empty(),
        })
    };
    // col >= lo AND col <= hi（使用裸列名——Filter{Scan} 内单因子无歧义）
    Expr::BinaryOp {
        left: Box::new(Expr::BinaryOp {
            left: Box::new(id(col)),
            op: sqlparser::ast::BinaryOperator::GtEq,
            right: Box::new(num(lo_v)),
        }),
        op: sqlparser::ast::BinaryOperator::And,
        right: Box::new(Expr::BinaryOp {
            left: Box::new(id(col)),
            op: sqlparser::ast::BinaryOperator::LtEq,
            right: Box::new(num(hi_v)),
        }),
    }
}

// ---------------------------------------------------------------------------
// O-4'：join reorder（INNER 链贪心重排——spec 12 §4 前置收口）
// ---------------------------------------------------------------------------

/// 因子槽（重排分析单元：叶子 Scan 或下推后的 Filter{Scan}）
struct JoinFactor {
    key: String,
    node: Plan2,
    /// 该因子**到达时**的 ON（左深链第 k 步的 ON 归属第 k 个到达因子）
    on: Expr,
    est: u64,
}

/// Plan 局部别名（避免与顶层 Plan 名冲突的占位——实际用 ir::plan::Plan）
type Plan2 = crate::ir::plan::Plan;

/// ON 的因子引用集（限定名前缀；裸名 → 无法判定归属 → None 即保守放弃）
fn on_factor_refs(on: &Expr, keys: &[String]) -> Option<Vec<String>> {
    let mut ids = Vec::new();
    crate::sql::optimize::expr_idents_pub(on, &mut ids);
    let mut out = Vec::new();
    for id in &ids {
        let Some((prefix, _)) = id.split_once('.') else {
            return None; // 裸名：归属不明——保守不重排
        };
        if !keys.iter().any(|k| k == prefix) {
            return None; // 引用链外标识（列别名等）——放弃
        }
        if !out.iter().any(|k| k == prefix) {
            out.push(prefix.to_string());
        }
    }
    Some(out)
}

/// INNER join 链贪心重排（left-deep 保持）：
/// 1. 链上全部 INNER 且 ≥3 因子；叶子 = Scan / Filter{Scan}；
///    任一 LEFT / 嵌套非链形态 → 不动
/// 2. 连通性：每步选「ON 引用 ⊆ 已选集 ∪ 自身」的因子中估算最小者
/// 3. 估算：scan_est（下推谓词后）× join_est（等值键 NDV：pk 精确/
///    整数区间/缺省回退）——无统计（est 全 0）不重排
///
/// 语义安全性：INNER ⋈ 可交换结合（输出**行序**随序变化——无序多重集
/// 等价，common §1.1 口径；有 ORDER BY 的查询排序在最终输出前）
pub fn rewrite_join_order(
    plan: &mut Plan2,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
) {
    if has_wildcard_projection(plan) {
        return; // 纯通配投影：输出列序 = join 序——重排改变客户端可见
                // schema（SELECT * / INSERT…SELECT * 位置契约）。AST 路径
                // 输出 FROM 序；计划路径须保持一致（评审 P0）
    }
    rewrite_join_order_walk(plan, db, sess);
}

/// 计划任意 Project 节点带 wildcard 标记（含 SetOp 分支内）
fn has_wildcard_projection(p: &Plan2) -> bool {
    use crate::ir::plan::Plan;
    match p {
        Plan::Project { wildcard, input, .. } => *wildcard || has_wildcard_projection(input),
        Plan::Filter { input, .. }
        | Plan::Aggregate { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input }
        | Plan::Window { input, .. } => has_wildcard_projection(input),
        Plan::SubqueryScan { plan, .. } => has_wildcard_projection(plan),
        Plan::Cte { plan, body, .. } => {
            has_wildcard_projection(plan) || has_wildcard_projection(body)
        }
        Plan::IterativeScan { base, recursive, .. } => {
            has_wildcard_projection(base) || has_wildcard_projection(recursive)
        }
        Plan::Join { left, right, .. } | Plan::SetOp { left, right, .. } => {
            has_wildcard_projection(left) || has_wildcard_projection(right)
        }
        Plan::Scan { .. } | Plan::Values => false,
    }
}

fn rewrite_join_order_walk(
    plan: &mut Plan2,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
) {
    use crate::ir::plan::Plan;
    match plan {
        Plan::Join { kind, left, right, .. } if kind == &"inner" => {
            rewrite_join_order_walk(left, db, sess);
            rewrite_join_order_walk(right, db, sess);
            try_reorder_chain(plan, db, sess);
        }
        Plan::Join { left, right, .. } => {
            // LEFT：子链内部仍可重排（外层结构不动）
            rewrite_join_order_walk(left, db, sess);
            rewrite_join_order_walk(right, db, sess);
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::Aggregate { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input }
        | Plan::Window { input, .. } => rewrite_join_order_walk(input, db, sess),
        Plan::SubqueryScan { plan, .. } => rewrite_join_order_walk(plan, db, sess),
        Plan::Cte { plan, body, .. } => {
            rewrite_join_order_walk(plan, db, sess);
            rewrite_join_order_walk(body, db, sess);
        }
        Plan::IterativeScan { base, recursive, .. } => {
            rewrite_join_order_walk(base, db, sess);
            rewrite_join_order_walk(recursive, db, sess);
        }
        Plan::SetOp { left, right, .. } => {
            rewrite_join_order_walk(left, db, sess);
            rewrite_join_order_walk(right, db, sess);
        }
        Plan::Scan { .. } | Plan::Values => {}
    }
}

/// 链提取：Join{Join{...}, leaf_k, on_k}（左深）——叶子带各自到达 ON
fn try_reorder_chain(
    plan: &mut Plan2,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
) {
    use crate::ir::plan::Plan;
    // 收集（沿左脊——克隆快照遍历，重排是小树、克隆成本可忽略；
    // 直接 &mut 左脊遍历与右侧不可变读互斥借用，得不偿失）
    let mut factors: Vec<JoinFactor> = Vec::new();
    let mut cursor: &Plan2 = plan;
    loop {
        match cursor {
            Plan::Join { kind, left, right, on } if kind == &"inner" => {
                // 右因子必须是叶子（Scan / Filter{Scan}）
                let (key, node) = match &**right {
                    Plan::Scan { table, alias, .. } => (
                        alias.clone().unwrap_or_else(|| table.clone()),
                        (**right).clone(),
                    ),
                    Plan::Filter { .. } if matches!(&**right, Plan::Filter { .. }) => {
                        let Plan::Filter { input, .. } = &**right else {
                            unreachable!()
                        };
                        let Plan::Scan { table, alias, .. } = &**input else {
                            return // Filter 下非 Scan——非叶子形态
                        };
                        (
                            alias.clone().unwrap_or_else(|| table.clone()),
                            (**right).clone(),
                        )
                    }
                    _ => return, // 右侧子树非叶子（bushy）——不动
                };
                let on = on.clone();
                factors.push(JoinFactor { key, node, on, est: 0 });
                cursor = left;
            }
            _ => break,
        }
    }
    // 链底（最左因子）
    let bottom = cursor.clone();
    let (first_key, first_node) = match &bottom {
        Plan::Scan { table, alias, .. } => (
            alias.clone().unwrap_or_else(|| table.clone()),
            bottom.clone(),
        ),
        Plan::Filter { input, .. } if matches!(&**input, Plan::Scan { .. }) => {
            let Plan::Scan { table, alias, .. } = &**input else { unreachable!() };
            (
                alias.clone().unwrap_or_else(|| table.clone()),
                bottom.clone(),
            )
        }
        _ => return, // 链底非简单叶子——不动
    };
    factors.push(JoinFactor {
        key: first_key,
        node: first_node,
        on: Expr::Value(sqlparser::ast::ValueWithSpan {
            value: sqlparser::ast::Value::Boolean(true),
            span: sqlparser::tokenizer::Span::empty(),
        }), // 首因子无到达 ON
        est: 0,
    });
    if factors.len() < 3 {
        return; // <3 因子：O-4 构建侧已覆盖——不重排（避免无谓行序扰动）
    }
    // 估算（无统计 → 全 0 → 不重排）
    let keys: Vec<String> = factors.iter().map(|f| f.key.clone()).collect();
    for f in factors.iter_mut() {
        let (table, _alias, pushed_pred) = factor_parts(&f.node);
        let st = crate::sql::stats::table_stats(db, sess, &table);
        let est = match &st {
            Some(st) => {
                // 下推谓词（Filter{Scan} 形态的 pred）
                crate::sql::stats::scan_est(st, pushed_pred.as_ref())
            }
            None => 0,
        };
        if est == 0 && st.is_some() {
            return; // 有表无段统计（表名解析失败/无段）——保守不动
        }
        f.est = est;
    }
    if factors.iter().any(|f| f.est == 0) {
        return; // 任一无统计 → 放弃（半估半猜的重排比不排危险）
    }
    // 待决边模型：ON 不是"因子的到达条件"而是"因子集上的边"——
    // 消费于其引用集全部就位的那一步（AND 合并为该步 join ON）；
    // 首放置不消费边（哑 ON 丢弃）；候选必须至少连通一条待决边
    //（防笛卡尔积）。首因子取最小 est（真实语义：无 ON 约束首步）。
    let mut pending: Vec<Expr> = factors
        .iter()
        .filter(|f| !is_true_expr(&f.on))
        .map(|f| f.on.clone())
        .collect();
    let mut remaining: Vec<JoinFactor> = factors;
    remaining.sort_by_key(|f| f.est);
    let first = remaining.remove(0);
    let mut acc_keys = vec![first.key.clone()];
    let mut acc_est = first.est;
    let mut acc_node = first.node;
    // 已放置因子（键→表名）——join_estimate acc 侧定位用（P1 修：
    // 原查 remaining 未放置集——属主必不在其中）
    let first_table = factor_parts(&acc_node).0;
    let mut acc_placed: Vec<(String, String)> = vec![(first.key.clone(), first_table)];
    let total = acc_keys.len() + remaining.len();
    while !remaining.is_empty() {
        let mut best: Option<(usize, Vec<usize>, u64)> = None; // (因子, 消费边序号, est)
        for (i, f) in remaining.iter().enumerate() {
            // 连通性：某条待决边引用 ⊆ acc ∪ {f}
            let mut consumable: Vec<usize> = Vec::new();
            for (pi, on) in pending.iter().enumerate() {
                let Some(refs) = on_factor_refs(on, &keys) else {
                    continue; // 裸名/外引用边——不可判定（保守：不消费）
                };
                if refs.iter().all(|r| r == &f.key || acc_keys.contains(r)) {
                    consumable.push(pi);
                }
            }
            if consumable.is_empty() {
                continue; // 不连通——防笛卡尔积
            }
            // 估算：消费边中首个可解等值；否则扫描估算回退
            let est = consumable
                .iter()
                .filter_map(|&pi| {
                    join_estimate(
                        &pending[pi],
                        &acc_keys,
                        &acc_placed,
                        &remaining,
                        i,
                        db,
                        sess,
                        acc_est,
                    )
                })
                .next()
                .unwrap_or(f.est);
            if best.as_ref().is_none_or(|(_, _, e)| est < *e) {
                best = Some((i, consumable, est));
            }
        }
        let Some((bi, consumable, best_est)) = best else {
            return; // 无候选（连通性锁死）——还原不重排
        };
        let f = remaining.remove(bi);
        // 消费边 AND 合并为该步 ON（保序：pending 序）
        let mut step_on: Option<Expr> = None;
        for &pi in consumable.iter().rev() {
            let on = pending.remove(pi);
            step_on = Some(match step_on {
                None => on,
                Some(acc) => Expr::BinaryOp {
                    left: Box::new(on),
                    op: sqlparser::ast::BinaryOperator::And,
                    right: Box::new(acc),
                },
            });
        }
        let step_on = step_on.expect("consumable 非空必有 ON");
        let f_table = factor_parts(&f.node).0;
        acc_node = Plan::Join {
            kind: "inner",
            on: step_on,
            left: Box::new(acc_node),
            right: Box::new(f.node),
        };
        acc_keys.push(f.key.clone());
        acc_placed.push((f.key.clone(), f_table));
        acc_est = best_est;
    }
    if pending.is_empty() && acc_keys.len() == total {
        *plan = acc_node;
    }
    // 残留 pending（不可判定边）→ 不重排（保守）
}

fn is_true_expr(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Value(vws) if matches!(vws.value, sqlparser::ast::Value::Boolean(true))
    )
}


/// 叶子因子的 (表名, 别名, 下推谓词)
fn factor_parts(node: &Plan2) -> (String, Option<String>, Option<Expr>) {
    use crate::ir::plan::Plan;
    match node {
        Plan::Scan { table, alias, .. } => (table.clone(), alias.clone(), None),
        Plan::Filter { pred, input } => {
            let Plan::Scan { table, alias, .. } = &**input else {
                unreachable!()
            };
            (table.clone(), alias.clone(), Some(pred.clone()))
        }
        _ => unreachable!("factor_parts 只接受叶子"),
    }
}

/// 等值 join 估算：ON 的 `a.x = b.y`（一侧在 acc、一侧在新）→
/// max(ndv) 分数；ON 不可解（非等值/键不可解）→ None（回退扫描估算）
#[allow(clippy::too_many_arguments)] // 贪心估算上下文——内聚闭包不可拆
fn join_estimate(
    on: &Expr,
    acc_keys: &[String],
    acc_placed: &[(String, String)],
    remaining: &[JoinFactor],
    pick: usize,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
    acc_est: u64,
) -> Option<u64> {
    // 等值对提取（单等值；AND 链取首个可解等值——v1）
    let Expr::BinaryOp {
        left,
        op: sqlparser::ast::BinaryOperator::Eq,
        right,
    } = on
    else {
        return None;
    };
    let mut ids_l = Vec::new();
    let mut ids_r = Vec::new();
    expr_idents_pub(left, &mut ids_l);
    expr_idents_pub(right, &mut ids_r);
    let (id_l, id_r) = (ids_l.first()?, ids_r.first()?);
    // 归属：一侧 acc 一侧新
    let new_key = &remaining[pick].key;
    let (acc_id, new_id) = if id_l.starts_with(&format!("{new_key}.")) {
        (id_r.clone(), id_l.clone())
    } else if id_r.starts_with(&format!("{new_key}.")) {
        (id_l.clone(), id_r.clone())
    } else {
        return None; // 双侧同域（acc 内自关联式 ON）——不估
    };
    let acc_col = acc_id.split('.').next_back()?.to_string();
    let acc_fk = acc_id.split('.').next()?.to_string();
    if !acc_keys.contains(&acc_fk) {
        return None;
    }
    let new_col = new_id.split('.').next_back()?.to_string();
    // 新因子 NDV
    let (n_table, _, n_pred) = factor_parts(&remaining[pick].node);
    let n_st = crate::sql::stats::table_stats(db, sess, &n_table)?;
    let n_est = crate::sql::stats::scan_est(&n_st, n_pred.as_ref());
    let n_is_pk = is_pk_col(&n_table, &new_col, db, sess);
    let n_ndv = crate::sql::stats::col_ndv(&n_st, &new_col, n_is_pk);
    // acc 侧：因子表定位（列名→表——acc 内哪张表含该键；v1 遍历 acc 因子
    // 的表名，col_ndv 命中者）——保守：任一 acc 表含列即可
    // acc 侧定位（P1 修）：**已放置集**按因子键精确匹配——原在
    // remaining（未放置）中查，属主必不在其中（恒 None 或误取他表 NDV）
    let a_table = acc_placed
        .iter()
        .find(|(k, _)| *k == acc_fk)
        .map(|(_, t)| t.clone())?;
    let a_st = crate::sql::stats::table_stats(db, sess, &a_table)?;
    if !a_st.names.iter().any(|n| n.eq_ignore_ascii_case(&acc_col)) {
        return None;
    }
    let a_is_pk = is_pk_col(&a_table, &acc_col, db, sess);
    let a_ndv = crate::sql::stats::col_ndv(&a_st, &acc_col, a_is_pk);
    Some(crate::sql::stats::join_est_rows(acc_est, n_est, a_ndv, n_ndv))
}

/// 列是否该表 pk（schema 查询）
fn is_pk_col(
    table: &str,
    col: &str,
    db: &crate::engine::Database,
    sess: &crate::engine::Session,
) -> bool {
    crate::sql::scan::resolve_table(db, &sess.branch, table)
        .map(|(schema, _)| {
            schema
                .pk
                .iter()
                .any(|&p| schema.columns.get(p as usize).is_some_and(|c| c.name.eq_ignore_ascii_case(col)))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parse_batch;
    use sqlparser::ast::Statement;

    fn optimize_where(sql: &str) -> String {
        let stmts = parse_batch(sql, crate::sql::SqlDialect::Pg).unwrap();
        match &stmts[0] {
            Statement::Query(q) => {
                if let sqlparser::ast::SetExpr::Select(sel) = q.body.as_ref() {
                    let optimized = optimize(sel.selection.as_ref().unwrap());
                    format!("{optimized}")
                } else {
                    panic!("expected select")
                }
            }
            _ => panic!("expected query"),
        }
    }

    #[test]
    fn constant_folding_arithmetic() {
        assert_eq!(optimize_where("SELECT * FROM t WHERE 1 + 2 = 3"), "3 = 3");
        assert_eq!(
            optimize_where("SELECT * FROM t WHERE 10 * 3 = 30"),
            "30 = 30"
        );
    }

    #[test]
    fn bool_simplification_and_true() {
        let r = optimize_where("SELECT * FROM t WHERE TRUE AND id = 1");
        assert!(!r.contains("TRUE"), "TRUE AND 未被消除: {r}");
    }

    #[test]
    fn complex_expr_preserved() {
        let r = optimize_where("SELECT * FROM t WHERE id + 1 = 2");
        assert!(r.contains("id"), "列引用不应被消除: {r}");
    }
}

// ---------------------------------------------------------------------------
// SOTA P3：等值谓词复制下推（DuckDB Filter Pushdown 的等值复制规则）
// `WHERE a.x = 5 AND a.x = b.y` → pushdown 后 a 侧 Filter{a.x = 5}，
// ON 等值对 a.x = b.y → 可复制为 b.y = 5（INNER join 语义：
// 结果只含 a.x = b.y 的对，a.x = 5 的行 join 的对侧必有 b.y = 5）
// 范围谓词同理（a.x > 10 → b.y > 10：join 结果中 b.y = a.x > 10）
// ---------------------------------------------------------------------------

/// 等值谓词复制：join ON 等值对 (l.col, r.col)，若 l 侧 Filter 有
/// 关于 l.col 的常量比较，复制到 r.col 并注入 r 侧 Filter{Scan}
pub fn rewrite_eq_copy(plan: &mut Plan2) {
    rewrite_eq_copy_walk(plan);
}

fn rewrite_eq_copy_walk(plan: &mut Plan2) {
    use crate::ir::plan::Plan;
    match plan {
        Plan::Join { kind, left, right, on } => {
            rewrite_eq_copy_walk(left);
            rewrite_eq_copy_walk(right);
            if kind == &"inner" {
                try_eq_copy(left, right, on);
            }
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::Aggregate { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input }
        | Plan::Window { input, .. } => rewrite_eq_copy_walk(input),
        Plan::SubqueryScan { plan, .. } => rewrite_eq_copy_walk(plan),
        Plan::Cte { plan, body, .. } => {
            rewrite_eq_copy_walk(plan);
            rewrite_eq_copy_walk(body);
        }
        Plan::IterativeScan { base, recursive, .. } => {
            rewrite_eq_copy_walk(base);
            rewrite_eq_copy_walk(recursive);
        }
        Plan::SetOp { left, right, .. } => {
            rewrite_eq_copy_walk(left);
            rewrite_eq_copy_walk(right);
        }
        Plan::Scan { .. } | Plan::Values => {}
    }
}

/// 一对 (col_id, op, 常量值) ——从 Filter 谓词中提取的可复制比较
#[derive(Debug, Clone)]
struct CopyablePred {
    col: String,  // 限定名 "a.x"
    op: &'static str, // ">=" | "<=" | "=" (只复制这三类)
    val: i64,     // 数值常量（order 域无关——直接用 i64 值域）
}

fn try_eq_copy(left: &mut Plan2, right: &mut Plan2, on: &Expr) {
    let pairs = extract_eq_pairs(on);
    if pairs.is_empty() {
        return;
    }
    // 从两侧 Filter 提取可复制谓词
    let l_preds = extract_copyable(left);
    let r_preds = extract_copyable(right);
    for (l_id, r_id) in &pairs {
        // l → r 方向：l 侧谓词中引用 l_id 的，改列名为 r_id 注入 r 侧
        for p in &l_preds {
            if p.col == *l_id {
                inject_copy(right, r_id, p);
            }
        }
        // r → l 方向
        for p in &r_preds {
            if p.col == *r_id {
                inject_copy(left, l_id, p);
            }
        }
    }
}

/// 从子树中提取可复制的常量比较（Filter{Scan} 内的合取项）
fn extract_copyable(node: &Plan2) -> Vec<CopyablePred> {
    use crate::ir::plan::Plan;
    let mut out = Vec::new();
    // 只提取 Filter{Scan} 的谓词（叶子层——可安全改写）
    if let Plan::Filter { pred, input } = node {
        if matches!(&**input, Plan::Scan { .. }) {
            collect_copyable(pred, &mut out);
        }
    }
    out
}

fn collect_copyable(e: &Expr, out: &mut Vec<CopyablePred>) {
    match e {
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } => {
            collect_copyable(left, out);
            collect_copyable(right, out);
        }
        Expr::BinaryOp { left, op, right } => {
            use sqlparser::ast::BinaryOperator as BO;
            let op_s = match op {
                BO::GtEq => ">=",
                BO::LtEq => "<=",
                BO::Eq => "=",
                _ => return,
            };
            // 限定名 op 数值常量（或反向）
            for (a, b, o) in [
                (left.as_ref(), right.as_ref(), op_s),
                (
                    right.as_ref(),
                    left.as_ref(),
                    match op_s {
                        ">=" => "<=",
                        "<=" => ">=",
                        _ => "=",
                    },
                ),
            ] {
                if let (
                    Expr::CompoundIdentifier(parts),
                    Expr::Value(vws),
                ) = (a, b)
                {
                    if parts.len() == 2 {
                        let col = format!(
                            "{}.{}",
                            parts[0].value.to_ascii_lowercase(),
                            parts[1].value.to_ascii_lowercase()
                        );
                        if let sqlparser::ast::Value::Number(n, _) = &vws.value {
                            if let Ok(v) = n.parse::<i64>() {
                                out.push(CopyablePred {
                                    col,
                                    op: o,
                                    val: v,
                                });
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// 将可复制谓词注入对侧（改列名后 AND 到 Filter 或新建 Filter）
fn inject_copy(node: &mut Plan2, target_id: &str, pred: &CopyablePred) {
    use crate::ir::plan::Plan;
    let id = |s: &str| Expr::Identifier(sqlparser::ast::Ident::new(s));
    let num = |v: i64| {
        Expr::Value(sqlparser::ast::ValueWithSpan {
            value: sqlparser::ast::Value::Number(v.to_string(), false),
            span: sqlparser::tokenizer::Span::empty(),
        })
    };
    let target_col = target_id.split('.').next_back().unwrap_or(target_id);
    let new_pred = Expr::BinaryOp {
        left: Box::new(id(target_col)),
        op: match pred.op {
            ">=" => sqlparser::ast::BinaryOperator::GtEq,
            "<=" => sqlparser::ast::BinaryOperator::LtEq,
            _ => sqlparser::ast::BinaryOperator::Eq,
        },
        right: Box::new(num(pred.val)),
    };
    match node {
        Plan::Filter {
            pred: existing,
            input,
        } if matches!(&**input, Plan::Scan { .. }) => {
            *existing = Expr::BinaryOp {
                left: Box::new(existing.clone()),
                op: sqlparser::ast::BinaryOperator::And,
                right: Box::new(new_pred),
            };
        }
        Plan::Scan { .. } => {
            *node = Plan::Filter {
                pred: new_pred,
                input: Box::new(std::mem::replace(node, Plan::Values)),
            };
        }
        _ => {} // 非叶子——防御
    }
}

// ---------------------------------------------------------------------------
// SOTA 收口：IN Clause Rewriter（DuckDB blog 2024-11 同构）
// 单值 IN (x) → x = v（等值——可触发 eq_copy / 点查下推）
// 小范围连续整数 IN (1,2,3) → x >= 1 AND x <= 3（范围——可触发
// stat_prop / 段级 zone map 剪枝）
// 其余保持 IN 列表（步列表求值——原有路径）
// ---------------------------------------------------------------------------

/// IN 列表重写：遍历计划表达式树，找到 `col IN (v1, v2, ...)` 合取项
pub fn rewrite_in_list(plan: &mut Plan2) {
    rewrite_in_list_walk(plan);
}

fn rewrite_in_list_walk(plan: &mut Plan2) {
    use crate::ir::plan::Plan;
    match plan {
        Plan::Filter { pred, input } => {
            rewrite_in_list_walk(input);
            *pred = rewrite_in_expr(pred);
        }
        Plan::Project { exprs, input, .. } => {
            rewrite_in_list_walk(input);
            for e in exprs.iter_mut() {
                *e = rewrite_in_expr(e);
            }
        }
        Plan::Join { on, left, right, .. } => {
            rewrite_in_list_walk(left);
            rewrite_in_list_walk(right);
            *on = rewrite_in_expr(on);
        }
        Plan::Aggregate { keys, input, .. } => {
            rewrite_in_list_walk(input);
            for e in keys.iter_mut() {
                *e = rewrite_in_expr(e);
            }
        }
        Plan::Sort { keys, input, .. } => {
            rewrite_in_list_walk(input);
            for (e, _) in keys.iter_mut() {
                *e = rewrite_in_expr(e);
            }
        }
        Plan::Limit { input, .. }
        | Plan::Distinct { input }
        | Plan::Window { input, .. } => rewrite_in_list_walk(input),
        Plan::SubqueryScan { plan, .. } => rewrite_in_list_walk(plan),
        Plan::Cte { plan, body, .. } => {
            rewrite_in_list_walk(plan);
            rewrite_in_list_walk(body);
        }
        Plan::IterativeScan { base, recursive, .. } => {
            rewrite_in_list_walk(base);
            rewrite_in_list_walk(recursive);
        }
        Plan::SetOp { left, right, .. } => {
            rewrite_in_list_walk(left);
            rewrite_in_list_walk(right);
        }
        Plan::Scan { .. } | Plan::Values => {}
    }
}

/// 表达式内 IN 列表重写（递归）
fn rewrite_in_expr(e: &Expr) -> Expr {
    match e {
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } => Expr::BinaryOp {
            left: Box::new(rewrite_in_expr(left)),
            op: sqlparser::ast::BinaryOperator::And,
            right: Box::new(rewrite_in_expr(right)),
        },
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(rewrite_in_expr(left)),
            op: op.clone(),
            right: Box::new(rewrite_in_expr(right)),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(rewrite_in_expr(inner))),
        // IN 列表重写核心
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            // 提取全部数值常量列表项
            let consts: Vec<i64> = list
                .iter()
                .filter_map(|item| match item {
                    Expr::Value(vws) => match &vws.value {
                        sqlparser::ast::Value::Number(n, _) => n.parse::<i64>().ok(),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            if consts.len() != list.len() || consts.is_empty() {
                return e.clone(); // 非全数值或空——不改
            }
            if consts.len() == 1 {
                // 单值 → 等值
                return Expr::BinaryOp {
                    left: expr.clone(),
                    op: sqlparser::ast::BinaryOperator::Eq,
                    right: Box::new(Expr::Value(sqlparser::ast::ValueWithSpan {
                        value: sqlparser::ast::Value::Number(
                            consts[0].to_string(),
                            false,
                        ),
                        span: sqlparser::tokenizer::Span::empty(),
                    })),
                };
            }
            // 连续整数序列 → 范围
            let sorted = {
                let mut s = consts.clone();
                s.sort();
                s
            };
            let consecutive = sorted.windows(2).all(|w| w[1] == w[0] + 1);
            if consecutive && sorted.len() >= 2 {
                let lo = sorted[0];
                let hi = sorted[sorted.len() - 1];
                let num = |v: i64| {
                    Box::new(Expr::Value(sqlparser::ast::ValueWithSpan {
                        value: sqlparser::ast::Value::Number(v.to_string(), false),
                        span: sqlparser::tokenizer::Span::empty(),
                    }))
                };
                return Expr::BinaryOp {
                    left: Box::new(Expr::BinaryOp {
                        left: expr.clone(),
                        op: sqlparser::ast::BinaryOperator::GtEq,
                        right: num(lo),
                    }),
                    op: sqlparser::ast::BinaryOperator::And,
                    right: Box::new(Expr::BinaryOp {
                        left: expr.clone(),
                        op: sqlparser::ast::BinaryOperator::LtEq,
                        right: num(hi),
                    }),
                };
            }
            e.clone() // 非连续——保持 IN 列表
        }
        _ => e.clone(),
    }
}

// ---------------------------------------------------------------------------
// SOTA 收口：Reorder Filters（廉价谓词先执行——DuckDB blog 2024-11）
// Filter{Scan} 的合取链重排：等值 → 范围 → 其余（等值最廉价——
// O(1) 比较；范围次之——cmp_values；表达式求值最贵——函数/算术）
// 语义安全：AND 交换律（eager 求值下错误行为不变——两侧已求值）
// ---------------------------------------------------------------------------

pub fn rewrite_filter_order(plan: &mut Plan2) {
    rewrite_filter_order_walk(plan);
}

fn rewrite_filter_order_walk(plan: &mut Plan2) {
    use crate::ir::plan::Plan;
    match plan {
        Plan::Filter { pred, input } => {
            rewrite_filter_order_walk(input);
            reorder_conjuncts(pred);
        }
        Plan::Project { input, .. }
        | Plan::Aggregate { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input }
        | Plan::Window { input, .. } => rewrite_filter_order_walk(input),
        Plan::SubqueryScan { plan, .. } => rewrite_filter_order_walk(plan),
        Plan::Cte { plan, body, .. } => {
            rewrite_filter_order_walk(plan);
            rewrite_filter_order_walk(body);
        }
        Plan::IterativeScan { base, recursive, .. } => {
            rewrite_filter_order_walk(base);
            rewrite_filter_order_walk(recursive);
        }
        Plan::Join { left, right, .. } => {
            rewrite_filter_order_walk(left);
            rewrite_filter_order_walk(right);
        }
        Plan::SetOp { left, right, .. } => {
            rewrite_filter_order_walk(left);
            rewrite_filter_order_walk(right);
        }
        Plan::Scan { .. } | Plan::Values => {}
    }
}

/// 合取链按代价重排（等值 < 范围 < 表达式）
fn reorder_conjuncts(pred: &mut Expr) {
    let conjuncts = split_conjuncts(pred);
    if conjuncts.len() < 2 {
        return;
    }
    // 代价分级：0 = 等值（col = const）；1 = 范围（col >=/<=/>/< const）；
    // 2 = IsNull 族（O(1) 判空）；3 = 其他（函数/算术/嵌套）
    let rank = |e: &Expr| -> u8 {
        match e {
            Expr::BinaryOp {
                op: sqlparser::ast::BinaryOperator::Eq,
                ..
            } => 0,
            Expr::BinaryOp {
                op: sqlparser::ast::BinaryOperator::GtEq
                | sqlparser::ast::BinaryOperator::LtEq
                | sqlparser::ast::BinaryOperator::Gt
                | sqlparser::ast::BinaryOperator::Lt,
                ..
            } => 1,
            Expr::IsNull(_) | Expr::IsNotNull(_) => 2,
            _ => 3,
        }
    };
    let mut indexed: Vec<(u8, usize, Expr)> = conjuncts
        .into_iter()
        .enumerate()
        .map(|(i, e)| (rank(&e), i, e))
        .collect();
    // 稳定排序：rank 相同按原序（语义等价 + 确定性）
    indexed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let reordered: Vec<Expr> = indexed.into_iter().map(|(_, _, e)| e).collect();
    *pred = and_all(reordered).expect("非空合取列表");
}

// ---------------------------------------------------------------------------
// P0：子查询内联（非相关子查询 → 常量/值列表——PG SubLink→InitPlan 同构）
// 在 eval_select 的 WHERE 求值前，遍历谓词：
// - Expr::Subquery(q) → 求值（单行单列 → 标量常量）
// - Expr::InSubquery(e, q, neg) → 求值 → InList（值列表）
// - Expr::Exists(q, neg) → 求值（行数>0 → Bool）
// 相关子查询（引用外层列）→ not_supported（诚实拒绝——v2 迭代求值）
// ---------------------------------------------------------------------------

/// 谓词中的非相关子查询内联（WHERE / HAVING / JOIN ON 均经此）
pub fn inline_subqueries(
    db: &crate::engine::Database,
    sess: &mut crate::engine::Session,
    e: &mut Expr,
    snapshot: u64,
) -> Result<(), crate::error::SqlError> {
    inline_walk(db, sess, e, snapshot)
}

fn inline_walk(
    db: &crate::engine::Database,
    sess: &mut crate::engine::Session,
    e: &mut Expr,
    snapshot: u64,
) -> Result<(), crate::error::SqlError> {
    use crate::types::SqlValue;
    // 相关性检测：收集子查询 FROM 表的全部列名（内域），然后检查
    // WHERE/投影中的限定名前缀是否引用内域外的表（真相关）。
    // 非限定名视为内域引用（子查询自身列——保守不标相关）。
    //（v1 的"尝试求值"法：域外列解析 Err 被 apply_predicates 静默
    // 吞掉 → 空集 → avg=NULL → 恒假——子查询"成功"返回错误结果）
    match e {
        // 标量子查询：(SELECT max(v) FROM t) → 常量
        Expr::Subquery(q) => {
            if is_correlated(q) {
                return Err(crate::error::SqlError::not_supported(
                    "correlated scalar subquery (v2: iterative evaluation)",
                ));
            }
            let tv = match crate::sql::scan::eval_query(db, sess, q, snapshot) {
                Ok(tv) => tv,
                Err(e) if e.state == "42703" => {
                    // 列不存在——外层列引用（相关子查询）
                    return Err(crate::error::SqlError::not_supported(
                        "correlated scalar subquery (v2: iterative evaluation)",
                    ));
                }
                Err(e) => return Err(e),
            };
            if tv.rows.len() == 1 && tv.names.len() == 1 {
                let v = tv.rows[0][0].clone();
                *e = sql_value_to_expr(&v);
            } else if tv.rows.is_empty() {
                *e = sql_value_to_expr(&SqlValue::Null);
            } else {
                return Err(crate::error::SqlError::not_supported(
                    "scalar subquery returned multiple rows",
                ));
            }
            Ok(())
        }
        // IN 子查询：v IN (SELECT id FROM t) → v IN (v1, v2, ...)
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            if is_correlated(subquery) {
                return Err(crate::error::SqlError::not_supported(
                    "correlated IN subquery (v2: iterative evaluation)",
                ));
            }
            let tv = match crate::sql::scan::eval_query(db, sess, subquery, snapshot) {
                Ok(tv) => tv,
                Err(e) if e.state == "42703" => {
                    return Err(crate::error::SqlError::not_supported(
                        "correlated IN subquery (v2: iterative evaluation)",
                    ));
                }
                Err(e) => return Err(e),
            };
            let list: Vec<Expr> = tv
                .rows
                .iter()
                .map(|r| sql_value_to_expr(&r[0]))
                .collect();
            *e = Expr::InList {
                expr: expr.clone(),
                list,
                negated: *negated,
            };
            Ok(())
        }
        // EXISTS：EXISTS (SELECT ...) → Bool
        Expr::Exists {
            subquery,
            negated,
        } => {
            if is_correlated(subquery) {
                return Err(crate::error::SqlError::not_supported(
                    "correlated IN subquery (v2: iterative evaluation)",
                ));
            }
            let tv = match crate::sql::scan::eval_query(db, sess, subquery, snapshot) {
                Ok(tv) => tv,
                Err(e) if e.state == "42703" => {
                    return Err(crate::error::SqlError::not_supported(
                        "correlated EXISTS (v2: iterative evaluation)",
                    ));
                }
                Err(e) => return Err(e),
            };
            let exists = !tv.rows.is_empty();
            *e = sql_value_to_expr(&SqlValue::Bool(if *negated {
                !exists
            } else {
                exists
            }));
            Ok(())
        }
        // 递归：二元/嵌套/InList/Case 等容器
        Expr::BinaryOp {
            left,
            right,
            op: _,
        } => {
            inline_walk(db, sess, left, snapshot)?;
            inline_walk(db, sess, right, snapshot)
        }
        Expr::Nested(inner) => inline_walk(db, sess, inner, snapshot),
        Expr::UnaryOp {
            expr: inner,
            op: _,
        } => inline_walk(db, sess, inner, snapshot),
        Expr::IsNull(inner) | Expr::IsNotNull(inner) | Expr::IsTrue(inner)
        | Expr::IsFalse(inner) => inline_walk(db, sess, inner, snapshot),
        Expr::InList {
            expr,
            list,
            negated: _,
        } => {
            inline_walk(db, sess, expr, snapshot)?;
            for item in list.iter_mut() {
                inline_walk(db, sess, item, snapshot)?;
            }
            Ok(())
        }
        Expr::Between {
            expr,
            low,
            high,
            negated: _,
        } => {
            inline_walk(db, sess, expr, snapshot)?;
            inline_walk(db, sess, low, snapshot)?;
            inline_walk(db, sess, high, snapshot)
        }
        Expr::Cast {
            expr: inner, ..
        } => inline_walk(db, sess, inner, snapshot),
        _ => Ok(()),
    }
}


/// 子查询相关性检测：收集 FROM 表因子键（内域），检查 WHERE/投影中
/// 的限定名前缀是否引用内域外的表——真相关（需外层列迭代求值）
fn is_correlated(q: &sqlparser::ast::Query) -> bool {
    // 收集 FROM 因子键
    let mut inner_factors: Vec<String> = Vec::new();
    if let sqlparser::ast::SetExpr::Select(sel) = &*q.body {
        for twj in &sel.from {
            if let Some(k) = crate::sql::optimize::factor_key(&twj.relation) {
                inner_factors.push(k);
            }
        }
    }
    // 收集 WHERE/投影中的限定名前缀
    let mut prefixes: Vec<String> = Vec::new();
    if let sqlparser::ast::SetExpr::Select(sel) = &*q.body {
        for item in &sel.projection {
            if let sqlparser::ast::SelectItem::UnnamedExpr(e)
            | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } = item
            {
                let mut ids = Vec::new();
                expr_idents_pub(e, &mut ids);
                for id in ids {
                    if let Some((prefix, _)) = id.split_once('.') {
                        let p = prefix.to_string();
                        if !prefixes.contains(&p) {
                            prefixes.push(p);
                        }
                    }
                }
            }
        }
        if let Some(w) = &sel.selection {
            let mut ids = Vec::new();
            expr_idents_pub(w, &mut ids);
            for id in ids {
                if let Some((prefix, _)) = id.split_once('.') {
                    let p = prefix.to_string();
                    if !prefixes.contains(&p) {
                        prefixes.push(p);
                    }
                }
            }
        }
    }
    // 限定名前缀引用了内域外的表 → 相关
    prefixes.iter().any(|p| !inner_factors.contains(p))
}

/// SqlValue → 常量 Expr
fn sql_value_to_expr(v: &crate::types::SqlValue) -> Expr {
    use crate::types::SqlValue;
    let vws = |val: sqlparser::ast::Value| {
        Expr::Value(sqlparser::ast::ValueWithSpan {
            value: val,
            span: sqlparser::tokenizer::Span::empty(),
        })
    };
    match v {
        SqlValue::Null => Expr::Value(sqlparser::ast::ValueWithSpan {
            value: sqlparser::ast::Value::Null,
            span: sqlparser::tokenizer::Span::empty(),
        }),
        SqlValue::Bool(b) => vws(sqlparser::ast::Value::Boolean(*b)),
        SqlValue::Int64(i) => vws(sqlparser::ast::Value::Number(i.to_string(), false)),
        SqlValue::Int32(i) => vws(sqlparser::ast::Value::Number(i.to_string(), false)),
        SqlValue::Float64(f) => vws(sqlparser::ast::Value::Number(f.to_string(), false)),
        SqlValue::Utf8(s) => vws(sqlparser::ast::Value::SingleQuotedString(s.clone())),
        SqlValue::Date32(d) => vws(sqlparser::ast::Value::Number(d.to_string(), false)),
        SqlValue::TimestampMs(t) => vws(sqlparser::ast::Value::Number(t.to_string(), false)),
        SqlValue::Bytes(b) => {
            vws(sqlparser::ast::Value::SingleQuotedString(
                String::from_utf8_lossy(b).to_string(),
            ))
        }
    }
}


// ---------------------------------------------------------------------------
// P0：CTE / WITH 非递归内联展开（AST 重写方案 A——评审文档 §3）
// `WITH c AS (SELECT ...) SELECT * FROM c JOIN t ON ...`
//   → 把 FROM 中 Table{name="c"} 替换为 Derived{subquery: c.query,
//     alias: c.alias}（内联展开——非递归 CTE 的语义 = 派生表）
// 递归 CTE / MATERIALIZED / 列别名 → not_supported（诚实拒绝）
// ---------------------------------------------------------------------------

/// CTE 内联展开：返回新 Query（原 q 不变——纯函数）
pub fn expand_ctes(q: &sqlparser::ast::Query) -> crate::error::Result<sqlparser::ast::Query> {
    let Some(with) = &q.with else {
        return Ok(q.clone()); // 无 WITH——原样返回
    };
    if with.recursive {
        return Err(crate::error::SqlError::not_supported(
            "WITH RECURSIVE (v2: iterative fixpoint needs eval context in expand)",
        ));
    }
    // 收集 CTE 名 → 查询体（按声明序——后声明可引用先声明）
    // 链式 CTE：c2 体内 `FROM c1` 也需替换——对每个 CTE 体先应用
    // 已收集的先前 CTE 替换，再入列
    let mut ctes: Vec<(String, sqlparser::ast::Query)> = Vec::new();
    for cte in &with.cte_tables {
        if cte.materialized.is_some() {
            return Err(crate::error::SqlError::not_supported(
                "AS MATERIALIZED / NOT MATERIALIZED",
            ));
        }
        if !cte.alias.columns.is_empty() {
            return Err(crate::error::SqlError::not_supported(
                "CTE column aliases (col1, col2)",
            ));
        }
        let name = cte.alias.name.value.to_ascii_lowercase();
        // 1. 嵌套 WITH 展开（CTE 体内的 WITH）
        let mut body = expand_ctes(&cte.query)?;
        // 2. 先前 CTE 引用替换（c2 体内的 FROM c1 → Derived(c1.query)）
        replace_cte_refs(&mut body, &ctes);
        ctes.push((name, body));
    }
    let mut out = q.clone();
    out.with = None; // 已消费
    // 在 body 的 FROM 中替换 CTE 引用
    replace_cte_refs(&mut out, &ctes);
    Ok(out)
}

/// 递归替换 Query body 中所有 CTE 表引用为 Derived 子查询
fn replace_cte_refs(q: &mut sqlparser::ast::Query, ctes: &[(String, sqlparser::ast::Query)]) {
    replace_setexpr(&mut q.body, ctes);
}

fn replace_setexpr(
    se: &mut Box<sqlparser::ast::SetExpr>,
    ctes: &[(String, sqlparser::ast::Query)],
) {
    match &mut **se {
        sqlparser::ast::SetExpr::Select(sel) => {
            replace_in_from(&mut sel.from, ctes);
        }
        sqlparser::ast::SetExpr::SetOperation { left, right, .. } => {
            replace_setexpr(left, ctes);
            replace_setexpr(right, ctes);
        }
        sqlparser::ast::SetExpr::Query(q) => {
            replace_cte_refs(q, ctes);
        }
        _ => {}
    }
}

fn replace_in_from(
    from: &mut [sqlparser::ast::TableWithJoins],
    ctes: &[(String, sqlparser::ast::Query)],
) {
    for twj in from.iter_mut() {
        replace_in_factor(&mut twj.relation, ctes);
        for j in twj.joins.iter_mut() {
            replace_in_factor(&mut j.relation, ctes);
        }
    }
}

fn replace_in_factor(
    tf: &mut sqlparser::ast::TableFactor,
    ctes: &[(String, sqlparser::ast::Query)],
) {
    match tf {
        sqlparser::ast::TableFactor::Table { name, alias, .. } => {
            let short = name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(|i| i.value.to_ascii_lowercase())
                .unwrap_or_default();
            // 命中 CTE 名 → 替换为 Derived
            if let Some((_, cte_q)) = ctes.iter().find(|(n, _)| *n == short) {
                let alias_str = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| short.clone());
                *tf = sqlparser::ast::TableFactor::Derived {
                    lateral: false,
                    subquery: Box::new(cte_q.clone()),
                    alias: Some(sqlparser::ast::TableAlias {
                        explicit: false,
                        name: sqlparser::ast::Ident::new(alias_str),
                        columns: vec![],
                        at: None,
                    }),
                    sample: None,
                };
            }
        }
        sqlparser::ast::TableFactor::Derived { subquery, .. } => {
            // 嵌套派生表内的 CTE 引用也替换
            replace_cte_refs(subquery, ctes);
        }
        _ => {}
    }
}
