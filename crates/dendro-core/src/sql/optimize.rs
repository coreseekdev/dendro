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
        _ => None, // 派生表等 v1 不作下推目标
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
