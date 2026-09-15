//! SQL 表达式规则优化（v1）：AST 级常量折叠 + 布尔简化 + 恒等消除。
//!
//! 在 `exec` 前对解析后的 AST 做一遍自底向上重写——消除运行时可预算的
//! 常量子表达式。v2 方向：prepared statement 缓存、逻辑计划 IR。

use sqlparser::ast::{BinaryOperator as BinOp, Expr, UnaryOperator as UnOp};

/// 优化 SQL 表达式（递归自底向上，语义保持）
pub fn optimize(e: &Expr) -> Expr {
    match e {
        Expr::BinaryOp { left, op, right } => {
            let l = optimize(left);
            let r = optimize(right);
            fold_binary(op, &l, &r).unwrap_or(Expr::BinaryOp {
                left: Box::new(l),
                op: *op,
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
        _ => None,
    }
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
