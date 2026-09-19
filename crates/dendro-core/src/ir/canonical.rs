//! IR 正式形式（canonical form）。
//!
//! 比较的前提是唯一性：**语义等价的计划必须映射到同一正式形式**
//!（canonical(A) == canonical(B) ⟺ 前端视 A/B 等价）。SQL 表面会
//! 合并/拆分计划节点（派生表包裹、恒等投影、Filter 堆叠），同一查询
//! 的两种写法在原始 Plan 上结构性不同——本模块做保范规范化（自底
//! 向上、确定性），消除这类表面差异。
//!
//! 规则（每条都是语义保持的）：
//! 1. 匿名派生表透明化：`SubqueryScan{key:""}` 是纯包裹 → 透传内部
//! 2. 恒等投影剥除：`Project{wildcard, exprs:[]}`（SELECT * 透传）→ 内部
//! 3. Filter 堆叠折叠：`Filter{p, Filter{q, x}}` → `Filter{q AND p, x}`
//!    （合取序 = 内层在前——确定性）
//! 4. Project∘Project 内联：外层标识符按内层 names → exprs 替换后合并
//!    （SELECT 列表合并的逆；v1 限非通配内层，裸名精确匹配）
//!
//! 不可规范化的差异（不同谓词/不同列序/不同节点类型）保持可区分——
//! 正式形式是判别性等价关系，不是语义包含。

use crate::ir::plan::Plan;
use sqlparser::ast::Expr;

/// 括号剥除：Expr::Nested 是表面语法（((x)) ≡ x）——正式形式消除。
/// 递归至不再是 Nested 为止（内部子表达式的括号按原样保留——
/// 优先级语义由原始解析决定）。
fn nx(e: &Expr) -> Expr {
    match e {
        Expr::Nested(i) => nx(i),
        o => cn_ids(o),
    }
}

/// 标识符大小写归一（解析层处处 eq_ignore_ascii_case——大小写是
/// 表面语法；字符串字面量不受影响）。VisitMut 递归全部子表达式。
/// 合取规范：AND 树展平为合取集、按显示串排序后左折叠重建
/// （合取可交换——项序是表面差异）。
fn conj_canon(e: &Expr) -> Expr {
    fn flatten(x: &Expr, out: &mut Vec<Expr>) {
        if let Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } = x
        {
            flatten(left, out);
            flatten(right, out);
            return;
        }
        out.push(x.clone());
    }
    let mut cs = Vec::new();
    flatten(e, &mut cs);
    if cs.len() < 2 {
        return e.clone();
    }
    let mut keyed: Vec<(String, Expr)> = cs.into_iter().map(|c| (c.to_string(), c)).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed
        .into_iter()
        .map(|(_, c)| c)
        .reduce(|l, r| conjoin(&l, &r))
        .unwrap()
}

fn cn_ids(e: &Expr) -> Expr {
    use sqlparser::ast::{VisitMut, VisitorMut};
    struct Lower;
    impl VisitorMut for Lower {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &mut Expr) -> std::ops::ControlFlow<()> {
            match e {
                Expr::Identifier(id) => {
                    id.value = id.value.to_ascii_lowercase();
                }
                Expr::CompoundIdentifier(parts) => {
                    for p in parts.iter_mut() {
                        p.value = p.value.to_ascii_lowercase();
                    }
                }
                Expr::Function(f) => {
                    f.name = sqlparser::ast::ObjectName(
                        f.name
                            .0
                            .iter()
                            .map(|part| match part.as_ident() {
                                Some(id) => sqlparser::ast::ObjectNamePart::Identifier(
                                    sqlparser::ast::Ident::new(id.value.to_ascii_lowercase()),
                                ),
                                None => part.clone(),
                            })
                            .collect(),
                    );
                }
                _ => {}
            }
            std::ops::ControlFlow::Continue(())
        }
    }
    let mut e2 = e.clone();
    let _ = e2.visit(&mut Lower);
    e2
}

/// 计划 → 正式形式（确定性；输入不可变）
pub fn canonicalize(p: &Plan) -> Plan {
    let c = match p {
        Plan::Values => Plan::Values,
        Plan::Scan {
            table,
            alias,
            version,
        } => Plan::Scan {
            table: table.to_ascii_lowercase(),
            alias: alias.as_deref().map(|a| a.to_ascii_lowercase()),
            version: version.clone(),
        },
        Plan::Filter { pred, input } => {
            let inner = canonicalize(input);
            // 规则 3：堆叠折叠
            match inner {
                Plan::Filter {
                    pred: inner_pred,
                    input: inner_input,
                } => Plan::Filter {
                    pred: conj_canon(&nx(&conjoin(&inner_pred, pred))),
                    input: inner_input,
                },
                other => Plan::Filter {
                    pred: conj_canon(&nx(pred)),
                    input: Box::new(other),
                },
            }
        }
        Plan::Project {
            exprs,
            names,
            wildcard,
            prefixes,
            input,
        } => {
            let inner = canonicalize(input);
            // 规则 2：恒等投影剥除
            if *wildcard && exprs.is_empty() && prefixes.is_empty() {
                return inner;
            }
            // 规则 2b：混合通配投影 × 窗口——`SELECT *, win() OVER ...` 是
            // SQL 表达窗口调用的必然表面形态（build_plan 恒生成），窗口
            // 节点本身透传输入列，混合投影可消解
            if *wildcard && !exprs.is_empty() && prefixes.is_empty() {
                if matches!(inner, Plan::Window { .. }) {
                    return inner;
                }
            }
            // 规则 4：Project∘Project 内联（内层非通配纯表达式）
            if let Plan::Project {
                exprs: inner_exprs,
                names: inner_names,
                wildcard: false,
                prefixes: inner_prefixes,
                input: inner_input,
            } = &inner
            {
                if inner_prefixes.is_empty() {
                    let subst = |e: &Expr| substitute(e, inner_names, inner_exprs);
                    let merged: Vec<Expr> = exprs.iter().map(|e| nx(&subst(e))).collect();
                    // 前缀通配保留在外层（列序：通配位 = 内层输出位）
                    let _ = prefixes;
                    return Plan::Project {
                        exprs: merged,
                        names: names.iter().map(|n| n.to_ascii_lowercase()).collect(),
                        wildcard: *wildcard,
                        prefixes: prefixes.clone(),
                        input: inner_input.clone(),
                    };
                }
            }
            Plan::Project {
                exprs: exprs.iter().map(nx).collect(),
                names: names.iter().map(|n| n.to_ascii_lowercase()).collect(),
                wildcard: *wildcard,
                prefixes: prefixes.clone(),
                input: Box::new(inner),
            }
        }
        Plan::Aggregate { keys, aggs, input } => Plan::Aggregate {
            keys: keys.iter().map(nx).collect(),
            aggs: aggs
                .iter()
                .map(|a| {
                    let mut a2 = a.clone();
                    if let Some(arg) = &a2.arg {
                        a2.arg = Some(nx(arg));
                    }
                    a2.func = a2.func.to_ascii_lowercase();
                    a2.display = a2.display.to_ascii_lowercase();
                    a2
                })
                .collect(),
            input: Box::new(canonicalize(input)),
        },
        Plan::Sort { keys, input } => Plan::Sort {
            keys: keys.iter().map(|(e, a)| (nx(e), *a)).collect(),
            input: Box::new(canonicalize(input)),
        },
        Plan::Limit {
            limit,
            offset,
            input,
        } => Plan::Limit {
            limit: *limit,
            offset: *offset,
            input: Box::new(canonicalize(input)),
        },
        Plan::Join {
            kind,
            on,
            left,
            right,
        } => Plan::Join {
            kind,
            on: on.clone(),
            left: Box::new(canonicalize(left)),
            right: Box::new(canonicalize(right)),
        },
        Plan::SemiJoin {
            key,
            negated,
            sub,
            input,
        } => Plan::SemiJoin {
            key: key.clone(),
            negated: *negated,
            sub: Box::new(canonicalize(sub)),
            input: Box::new(canonicalize(input)),
        },
        Plan::Distinct { input } => Plan::Distinct {
            input: Box::new(canonicalize(input)),
        },
        Plan::SubqueryScan { key, plan } => {
            let inner = canonicalize(plan);
            // 规则 1：匿名派生表透明化
            if key.is_empty() {
                return inner;
            }
            Plan::SubqueryScan {
                key: key.clone(),
                plan: Box::new(inner),
            }
        }
        Plan::Cte {
            name,
            names,
            plan,
            body,
        } => Plan::Cte {
            name: name.clone(),
            names: names.clone(),
            plan: Box::new(canonicalize(plan)),
            body: Box::new(canonicalize(body)),
        },
        Plan::Window { calls, input } => Plan::Window {
            calls: calls
                .iter()
                .map(|w| {
                    let mut c = w.clone();
                    c.func = c.func.to_ascii_lowercase();
                    if let Some(a) = &c.arg {
                        c.arg = Some(nx(a));
                    }
                    c.partition_by = c.partition_by.iter().map(nx).collect();
                    c.order_by = c.order_by.iter().map(|(e, a)| (nx(e), *a)).collect();
                    c
                })
                .collect(),
            input: Box::new(canonicalize(input)),
        },
        Plan::SetOp {
            op,
            left,
            right,
            all,
        } => Plan::SetOp {
            op,
            left: Box::new(canonicalize(left)),
            right: Box::new(canonicalize(right)),
            all: *all,
        },
        Plan::IterativeScan { .. } => p.clone(),
    };
    // 折叠可能使外层再次匹配规则（如 Project 剥除后暴露 Project∘Project）
    // ——一次再入即可覆盖本模块规则系的复合（规则都是收缩性的）
    let c2 = fixup_once(&c);
    if std::mem::discriminant(&c) == std::mem::discriminant(&c2) && &c == &c2 {
        c
    } else {
        canonicalize(&c2)
    }
}

/// 单层再匹配（规则复合的收敛步）
fn fixup_once(p: &Plan) -> Plan {
    match p {
        Plan::Project {
            exprs,
            names,
            wildcard,
            prefixes,
            input,
        } => {
            if *wildcard && exprs.is_empty() && prefixes.is_empty() {
                return (**input).clone();
            }
            if let Plan::Project {
                exprs: inner_exprs,
                names: inner_names,
                wildcard: false,
                prefixes: inner_prefixes,
                input: inner_input,
            } = &**input
            {
                if inner_prefixes.is_empty() {
                    let merged: Vec<Expr> = exprs
                        .iter()
                        .map(|e| nx(&substitute(e, inner_names, inner_exprs)))
                        .collect();
                    return Plan::Project {
                        exprs: merged,
                        names: names.iter().map(|n| n.to_ascii_lowercase()).collect(),
                        wildcard: *wildcard,
                        prefixes: prefixes.clone(),
                        input: inner_input.clone(),
                    };
                }
            }
            p.clone()
        }
        Plan::SubqueryScan { key, plan } if key.is_empty() => (**plan).clone(),
        _ => p.clone(),
    }
}

/// 内层 AND 合取（序 = 内层在前——确定性）
fn conjoin(a: &Expr, b: &Expr) -> Expr {
    use sqlparser::ast::BinaryOperator;
    Expr::BinaryOp {
        left: Box::new(a.clone()),
        op: BinaryOperator::And,
        right: Box::new(b.clone()),
    }
}

/// 标识符替换：names[i] → exprs[i]（裸名或末段精确匹配；未匹配保留
/// 原样——引用更深列时由形状规则兜底）
fn substitute(e: &Expr, names: &[String], exprs: &[Expr]) -> Expr {
    if names.is_empty() {
        return e.clone();
    }
    let lookup = |n: &str| -> Option<Expr> {
        names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(n))
            .and_then(|i| exprs.get(i).cloned())
    };
    match e {
        Expr::Identifier(id) => lookup(&id.value).unwrap_or_else(|| e.clone()),
        Expr::CompoundIdentifier(parts) => {
            let last = parts.last().map(|p| p.value.clone()).unwrap_or_default();
            lookup(&last).unwrap_or_else(|| e.clone())
        }
        // 其余节点递归（结构保持；子查询内不替换——作用域隔离）
        other => other.clone(),
    }
}
