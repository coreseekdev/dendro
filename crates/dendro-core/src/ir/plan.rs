//! 逻辑计划 IR（O-2a，spec 12 §4 / spec 02）：AST → Plan 的规范化 +
//! 计划级下推重写 + dendro.ir v1 plan 方言打印。
//!
//! 定位（v1）：Plan 是**优化器决策结构**与**审阅产物**——下推分类从
//! AST 走查迁移到计划走查（行为合同不变，差分测试护航），打印供
//! EXPLAIN（join 形态从 "pending" 升级为真实计划）与 golden（字节级
//! 确定性，P2）。执行仍走 AST 装配（O-3+ 的迁移目标）。
//!
//! **打印方言**（spec 09 §2）：SSA 名（%t 表/%s 扫描/%f 过滤/%j 连接/
//! %a 聚合/%p 投影/%o 排序/%u 集合/%v 值），每行一操作；谓词以
//! JSON 转义的 SQL 文本呈现（sqlparser Display 确定性）；物理派发
//! 注解（ScanAlt）不入计划本体。**v1 打印单向**（parse 随执行层
//! 管线化落地——不承诺未实现的 round-trip）。

use crate::engine::{Database, Session};
use crate::error::Result;
use crate::sql::agg::AggCall;
use sqlparser::ast::{Expr, Query, SetExpr, TableFactor};

/// 窗口调用（阶段1 IR 自足化：从 scan/window.rs 升格——计划与 AST
/// 路径共用同一结构；display 保留原文供打印/匹配）
#[derive(Debug, Clone)]
pub struct WindowCall {
    /// 函数名（row_number / rank / dense_rank / sum / count / min / max / avg）
    pub func: String,
    /// 参数（None = 无参如 row_number()；Some = sum(v) 的 v）
    pub arg: Option<Expr>,
    /// PARTITION BY 表达式列表
    pub partition_by: Vec<Expr>,
    /// ORDER BY 表达式 + ASC 标记
    pub order_by: Vec<(Expr, bool)>,
    /// 合成列名（__w0, __w1...——追加到 tv.names）
    pub synth_col: String,
    /// 原表达式 display（打印/round-trip 用——与 AggCall.display 同型）
    pub display: String,
}

/// 扫描版本子句（阶段1：Option<String> display 文本 → 结构化）
#[derive(Debug, Clone, PartialEq)]
pub enum ScanVersion {
    /// FOR SYSTEM_TIME AS OF <毫秒时间戳>
    AsOfTime(i64),
    /// AS OF <hash 文本>（内容寻址快照）
    AsOfHash(String),
}

impl ScanVersion {
    /// 打印/重建 synthetic_tf 用的 display 文本（往返稳定）
    pub fn display(&self) -> String {
        match self {
            ScanVersion::AsOfTime(ms) => ms.to_string(),
            ScanVersion::AsOfHash(h) => h.clone(),
        }
    }
    /// display 文本 → 结构化（纯数字 = 时间戳；其余 = hash）
    pub fn parse_display(t: &str) -> ScanVersion {
        match t.trim().parse::<i64>() {
            Ok(ms) => ScanVersion::AsOfTime(ms),
            Err(_) => ScanVersion::AsOfHash(t.trim().to_string()),
        }
    }
}

/// 逻辑计划节点（v1 形状集 = eval 支持的形状；select.from 仅首因子
/// 与其 join 链——与 eval_from 现状一致，逗号多因子 not_supported）
#[derive(Debug, Clone)]
pub enum Plan {
    /// 无 FROM 常量输入（单行零列）
    Values,
    Scan {
        table: String,
        alias: Option<String>,
        /// 版本子句（FOR SYSTEM_TIME AS OF ... / AS OF ...）——历史查询
        /// 上计划路径（O-2c+ B）；None = 当前读。阶段1 起结构化
        version: Option<ScanVersion>,
    },
    Filter {
        pred: Expr,
        input: Box<Plan>,
    },
    Join {
        kind: &'static str, // "inner" | "left"
        on: Expr,
        left: Box<Plan>,
        right: Box<Plan>,
    },
    Aggregate {
        keys: Vec<Expr>,
        /// 结构化聚合调用（阶段1 IR 自足化——display 文本进 IR 曾使
        /// sqlparser Display 格式成为语义身份，格式变体即错配）
        aggs: Vec<AggCall>,
        input: Box<Plan>,
    },
    /// SELECT DISTINCT（去重；阶段2 起入计划路径）
    Distinct {
        input: Box<Plan>,
    },
    /// 窗口函数（合成列求值；阶段2 起入计划路径——Project 下、
    /// Filter(谓词) 上）
    Window {
        calls: Vec<WindowCall>,
        input: Box<Plan>,
    },
    /// CTE 绑定（阶段3：CTE 进计划——体求值一次，body 中 Scan{name}
    /// 引用绑定结果；多次引用共享同一求值，克隆展开消失）
    Cte {
        name: String,
        /// CTE 列别名（WITH x(a, b) AS ...）；None = 用体输出名
        names: Option<Vec<String>>,
        plan: Box<Plan>,
        body: Box<Plan>,
    },
    /// 递归 CTE 不动点（阶段3：执行器驱动 delta 工作集迭代——PG 语义
    /// 递归臂只见上一轮新行；无 VALUES 注入、无 O(n²) 全量重建）
    IterativeScan {
        name: String,
        /// UNION DISTINCT 语义（false = UNION ALL）
        distinct: bool,
        /// CTE 列别名（WITH r(n) ...）——迭代绑定视图的列名（递归臂
        /// 引用 r.n 依赖；None = base 输出名）
        names: Option<Vec<String>>,
        base: Box<Plan>,
        recursive: Box<Plan>,
    },
    Project {
        exprs: Vec<Expr>,
        /// 输出列名（与 exprs 平行；别名信息不在 Expr 上——O-2c 执行
        /// 期由计划完整重投影所需）
        names: Vec<String>,
        /// 纯通配投影（SELECT *）：执行 = 输入透传（全列）；exprs/names 空
        wildcard: bool,
        /// 限定通配投影（SELECT o.*, c.*）：按因子布局区间取列——
        /// 阶段2 翻转（全部项为 QualifiedWildcard 时非空；混合形态
        /// build 失败回落 AST）
        prefixes: Vec<String>,
        input: Box<Plan>,
    },
    Sort {
        keys: Vec<(Expr, bool)>, // (expr, asc)
        input: Box<Plan>,
    },
    /// LIMIT/OFFSET 终结（含 OFFSET-only；与 Sort 分离——top-N 有界堆
    /// 经 Limit{Sort} 父子模式在执行期重建：n = limit + offset）
    Limit {
        limit: Option<usize>,
        offset: usize,
        input: Box<Plan>,
    },
    SetOp {
        op: &'static str, // "union" | "except" | "intersect"
        all: bool,
        left: Box<Plan>,
        right: Box<Plan>,
    },
}

impl Plan {
    /// 收集计划内全部扫描因子键（下推分类的目标集合）
    pub fn scan_keys(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_keys(&mut out);
        out
    }
    fn collect_keys(&self, out: &mut Vec<String>) {
        match self {
            Plan::Scan { table, alias, .. } => {
                let k = alias
                    .clone()
                    .unwrap_or_else(|| table.to_ascii_lowercase());
                if !out.contains(&k) {
                    out.push(k);
                }
            }
            Plan::Filter { input, .. }
            | Plan::Aggregate { input, .. }
            | Plan::Project { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input }
            | Plan::Window { input, .. } => input.collect_keys(out),
            Plan::Cte { plan, body, .. } => {
                plan.collect_keys(out);
                body.collect_keys(out);
            }
            Plan::IterativeScan { base, recursive, .. } => {
                base.collect_keys(out);
                recursive.collect_keys(out);
            }
            Plan::Join { left, right, .. } => {
                left.collect_keys(out);
                right.collect_keys(out);
            }
            Plan::SetOp { left, right, .. } => {
                left.collect_keys(out);
                right.collect_keys(out);
            }
            Plan::Values => {}
        }
    }
}

/// 因子键（与 optimize::factor_key 同口径：别名优先，短表名小写）
fn factor_key(
    tf: &TableFactor,
) -> Option<(String, String, Option<String>, Option<ScanVersion>)> {
    match tf {
        TableFactor::Table {
            name, alias, version, ..
        } => {
            let base = name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(|i| i.value.to_ascii_lowercase())?;
            let key = alias
                .as_ref()
                .map(|a| a.name.value.to_ascii_lowercase())
                .unwrap_or_else(|| base.clone());
            let ver = version.as_ref().map(scan_version_of);
            Some((key, base, alias.as_ref().map(|a| a.name.value.clone()), ver))
        }
        _ => None,
    }
}

/// sqlparser TableVersion → ScanVersion（display 形态分类）
fn scan_version_of(v: &sqlparser::ast::TableVersion) -> ScanVersion {
    let d = v.to_string();
    ScanVersion::parse_display(&d)
}

/// Query → Plan（自顶向下：SetOp 递归 / Select 装配链）
pub fn build_plan(q: &Query) -> Result<Plan> {
    let mut p = build_plan_body(q)?;
    // 阶段3：WITH → Cte 节点（rev 包裹——后声明先包，使链式引用
    // c2→c1 的 c2 体落在 c1 绑定作用域内）
    if let Some(with) = &q.with {
        for cte in with.cte_tables.iter().rev() {
            let name = cte.alias.name.value.to_ascii_lowercase();
            let names = if cte.alias.columns.is_empty() {
                None
            } else {
                Some(
                    cte.alias
                        .columns
                        .iter()
                        .map(|c| c.name.value.clone())
                        .collect(),
                )
            };
            let plan = if with.recursive
                && matches!(
                    &*cte.query.body,
                    SetExpr::SetOperation {
                        op: sqlparser::ast::SetOperator::Union,
                        ..
                    }
                )
            {
                let SetExpr::SetOperation {
                    set_quantifier,
                    left,
                    right,
                    ..
                } = &*cte.query.body
                else {
                    unreachable!()
                };
                let distinct = matches!(
                    set_quantifier,
                    sqlparser::ast::SetQuantifier::Distinct | sqlparser::ast::SetQuantifier::None
                );
                Plan::IterativeScan {
                    name: name.clone(),
                    distinct,
                    names: names.clone(),
                    base: Box::new(build_setexpr(left)?),
                    recursive: Box::new(build_setexpr(right)?),
                }
            } else {
                build_plan(&cte.query)?
            };
            p = Plan::Cte {
                name,
                names,
                plan: Box::new(plan),
                body: Box::new(p),
            };
        }
    }
    Ok(p)
}

fn build_plan_body(q: &Query) -> Result<Plan> {
    let mut p = build_setexpr(&q.body)?;
    // ORDER BY / LIMIT（Query 级）
    let order: &[(Expr, bool)] = &[]; // OrderByExpr → (expr, asc) 在下方转换
    let _ = order;
    if let sqlparser::ast::OrderByKind::Expressions(exprs) =
        q.order_by.as_ref().map(|o| &o.kind).unwrap_or(&sqlparser::ast::OrderByKind::Expressions(vec![]))
    {
        if !exprs.is_empty() {
            let keys: Vec<(Expr, bool)> = exprs
                .iter()
                .map(|o| (o.expr.clone(), o.options.asc.unwrap_or(true)))
                .collect();
            p = Plan::Sort { keys, input: Box::new(p) };
        }
    }
    // LIMIT/OFFSET（含 OFFSET-only、LIMIT-无-ORDER）
    if let Some(sqlparser::ast::LimitClause::LimitOffset { limit, offset, .. }) = &q.limit_clause {
        let lim = match limit {
            Some(l) => Some(crate::sql::scan::eval_const(l)? as usize),
            None => None,
        };
        let off = match offset {
            Some(off) => Some(crate::sql::scan::eval_const(&off.value)? as usize),
            None => Some(0),
        }
        .unwrap_or(0);
        if lim.is_some() || off > 0 {
            p = Plan::Limit {
                limit: lim,
                offset: off,
                input: Box::new(p),
            };
        }
    }
    Ok(p)
}

fn build_setexpr(se: &SetExpr) -> Result<Plan> {
    match se {
        SetExpr::Select(sel) => build_select(sel),
        SetExpr::SetOperation { op, set_quantifier, left, right } => {
            use sqlparser::ast::{SetOperator, SetQuantifier};
            let name = match op {
                SetOperator::Union => "union",
                SetOperator::Except => "except",
                SetOperator::Intersect => "intersect",
                SetOperator::Minus => "except",
            };
            let all = !matches!(set_quantifier, SetQuantifier::Distinct | SetQuantifier::None);
            Ok(Plan::SetOp {
                op: name,
                all,
                left: Box::new(build_setexpr(left)?),
                right: Box::new(build_setexpr(right)?),
            })
        }
        SetExpr::Query(q) => build_plan(q),
        other => Err(crate::error::SqlError::not_supported(format!(
            "plan build: {other}"
        ))),
    }
}

fn build_select(sel: &sqlparser::ast::Select) -> Result<Plan> {
    // FROM：首因子 + join 链（eval_from 同构）
    let mut plan: Plan = match sel.from.first() {
        None => Plan::Values,
        Some(twj) => {
            let (_key, table, alias, version) = factor_key(&twj.relation)
                .ok_or_else(|| crate::error::SqlError::not_supported("plan: derived table"))?;
            let mut p = Plan::Scan {
                table,
                alias,
                version,
            };
            for j in &twj.joins {
                let kind = match j.join_operator {
                    sqlparser::ast::JoinOperator::Join(_)
                    | sqlparser::ast::JoinOperator::Inner(_) => "inner",
                    sqlparser::ast::JoinOperator::Left(_)
                    | sqlparser::ast::JoinOperator::LeftOuter(_) => "left",
                    _ => {
                        return Err(crate::error::SqlError::not_supported(
                            "plan: join type",
                        ))
                    }
                };
                let on = match &j.join_operator {
                    sqlparser::ast::JoinOperator::Join(c)
                    | sqlparser::ast::JoinOperator::Inner(c)
                    | sqlparser::ast::JoinOperator::Left(c)
                    | sqlparser::ast::JoinOperator::LeftOuter(c) => match c {
                        sqlparser::ast::JoinConstraint::On(e) => e.clone(),
                        _ => {
                            return Err(crate::error::SqlError::not_supported(
                                "plan: join constraint",
                            ))
                        }
                    },
                    _ => unreachable!(),
                };
                let (_, rtable, ralias, rversion) = factor_key(&j.relation)
                    .ok_or_else(|| crate::error::SqlError::not_supported("plan: derived table"))?;
                p = Plan::Join {
                    kind,
                    on,
                    left: Box::new(p),
                    right: Box::new(Plan::Scan {
                        table: rtable,
                        alias: ralias,
                        version: rversion,
                    }),
                };
            }
            p
        }
    };
    // WHERE
    if let Some(w) = &sel.selection {
        plan = Plan::Filter { pred: w.clone(), input: Box::new(plan) };
    }
    // 窗口（合成列——Project 之下求值；与 eval_select 同收集口径）
    let window_calls = crate::sql::scan::collect_window_calls(sel)?;
    if !window_calls.is_empty() {
        // 与 AST 路径同拒绝（单一语义面）：窗口 + GROUP BY/HAVING v1 不支持
        if !matches!(&sel.group_by, sqlparser::ast::GroupByExpr::Expressions(es, _) if es.is_empty())
            || sel.having.is_some()
        {
            return Err(crate::error::SqlError::not_supported(
                "window function with GROUP BY / HAVING (v1)",
            ));
        }
        plan = Plan::Window {
            calls: window_calls.clone(),
            input: Box::new(plan),
        };
    }
    // GROUP BY / 聚合
    let has_agg = crate::sql::scan::projection_aggregates(&sel.projection).is_some()
        || sel.having.as_ref().map(crate::sql::scan::has_agg_expr).unwrap_or(false);
    let keys: Vec<Expr> = match &sel.group_by {
        sqlparser::ast::GroupByExpr::Expressions(es, _) => es.clone(),
        sqlparser::ast::GroupByExpr::All(_) => {
            return Err(crate::error::SqlError::not_supported("GROUP BY ALL"))
        }
    };
    if !keys.is_empty() || has_agg {
        let mut aggs: Vec<AggCall> = Vec::new();
        for item in &sel.projection {
            if let sqlparser::ast::SelectItem::UnnamedExpr(e)
            | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } = item
            {
                collect_agg_calls_plan(e, &mut aggs)?;
            }
        }
        if let Some(h) = &sel.having {
            collect_agg_calls_plan(h, &mut aggs)?;
        }
        plan = Plan::Aggregate { keys, aggs, input: Box::new(plan) };
        if let Some(h) = &sel.having {
            plan = Plan::Filter { pred: h.clone(), input: Box::new(plan) };
        }
    }
    // 投影（表达式 + 输出列名——与 project() 命名口径一致：
    // Unnamed = expr 文本前 40 字符；Alias = 别名）
    // 阶段2：窗口调用在构建期重写为合成列引用（与 AST 路径同遍历序）；
    // 限定通配（o.*）收集为 prefixes（全部项为 QualifiedWildcard 才走
    // 计划——混合形态 build 失败，AST 路径处理）
    let mut exprs: Vec<Expr> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut wilds = 0usize;
    let mut qwilds = 0usize;
    let mut prefixes: Vec<String> = Vec::new();
    let mut wc_idx = 0usize;
    for item in &sel.projection {
        match item {
            sqlparser::ast::SelectItem::UnnamedExpr(e) => {
                // 窗口项输出名 = 完整 display（与 AST 路径 patched 投影
                // 同口径——40 字符截断在两路径间产生命名分叉）；普通
                // 表达式维持 short_str
                let display = if crate::sql::scan::expr_has_window(e) {
                    e.to_string()
                } else {
                    crate::sql::scan::short_str_pub(e)
                };
                let mut ne = e.clone();
                if crate::sql::scan::expr_has_window(e) {
                    crate::sql::scan::rewrite_window_calls(&mut ne, &window_calls, &mut wc_idx);
                }
                exprs.push(ne);
                names.push(display);
            }
            sqlparser::ast::SelectItem::ExprWithAlias { expr, alias, .. } => {
                let mut ne = expr.clone();
                if crate::sql::scan::expr_has_window(expr) {
                    crate::sql::scan::rewrite_window_calls(&mut ne, &window_calls, &mut wc_idx);
                }
                exprs.push(ne);
                names.push(alias.value.clone());
            }
            sqlparser::ast::SelectItem::Wildcard(_) => wilds += 1,
            sqlparser::ast::SelectItem::QualifiedWildcard(kind, _) => {
                // 因子键 = 限定名末段小写（与 factor_key 同口径）
                if let Some(sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(o)) =
                    Some(kind)
                {
                    if let Some(k) = o
                        .0
                        .last()
                        .and_then(|p| p.as_ident())
                        .map(|i| i.value.to_ascii_lowercase())
                    {
                        prefixes.push(k);
                    }
                }
                qwilds += 1;
            }
            _ => {}
        }
    }
    // 混合限定通配（o.* 与表达式/通配并存）→ build 失败（AST 路径）
    if qwilds > 0 && qwilds != sel.projection.len() {
        return Err(crate::error::SqlError::not_supported(
            "plan: mixed qualified wildcard",
        ));
    }
    // 纯通配（全部项为通配）：wildcard 透传形态；混合通配计划不覆盖
    let wildcard = wilds > 0 && wilds == sel.projection.len();
    if wildcard || !prefixes.is_empty() {
        exprs.clear();
        names.clear();
    }
    plan = Plan::Project {
        exprs,
        names,
        wildcard,
        prefixes,
        input: Box::new(plan),
    };
    // SELECT DISTINCT（Project 之上——投影后按输出行去重）
    if sel.distinct.is_some() {
        plan = Plan::Distinct { input: Box::new(plan) };
    }
    Ok(plan)
}

/// 计划侧聚合调用收集（结构化 AggCall；display 去重口径与原文本版一致）
fn collect_agg_calls_plan(e: &Expr, out: &mut Vec<AggCall>) -> Result<()> {
    match e {
        Expr::Function(f) => {
            let n = f.name.to_string().to_ascii_lowercase();
            if matches!(n.as_str(), "count" | "sum" | "avg" | "min" | "max") {
                let d = e.to_string();
                if !out.iter().any(|c| c.display == d) {
                    out.push(crate::sql::scan::agg_call_from_fn(&d, f)?);
                }
            }
            if let sqlparser::ast::FunctionArguments::List(l) = &f.args {
                for a in &l.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(inner),
                    ) = a
                    {
                        collect_agg_calls_plan(inner, out)?;
                    }
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_agg_calls_plan(left, out)?;
            collect_agg_calls_plan(right, out)?;
        }
        Expr::Nested(i) => collect_agg_calls_plan(i, out)?,
        Expr::Cast { expr, .. } => collect_agg_calls_plan(expr, out)?,
        _ => {}
    }
    Ok(())
}

/// 下推结果的 EXPLAIN 注记（O-1 的 desc 合同不变）
pub fn pushdown_desc(pushed: &[(String, Vec<Expr>)]) -> String {
    if pushed.is_empty() {
        return String::new();
    }
    let total: usize = pushed.iter().map(|(_, cs)| cs.len()).sum();
    let parts: Vec<String> = pushed
        .iter()
        .map(|(k, cs)| format!("{k}({})", cs.len()))
        .collect();
    format!("optimizer: pushdown {total} conjunct(s) → {}", parts.join(", "))
}

// ---------------------------------------------------------------------------
// 计划级下推重写（O-2a：决策结构从 AST 走查迁到计划走查）
// ---------------------------------------------------------------------------

/// 就地下推：Filter(Join(...)) 的单源合取项下沉为各 Scan 上的
/// Filter 节点（打印即"优化后计划"）。返回 (因子键 → 合取项) 供
/// eval_from 应用（合同与 optimize::pushdown_plan 输出一致）。
pub fn rewrite_pushdown(plan: &mut Plan) -> Vec<(String, Vec<Expr>)> {
    let mut out = Vec::new();
    rewrite_walk(plan, &mut out);
    out
}

fn rewrite_walk(plan: &mut Plan, out: &mut Vec<(String, Vec<Expr>)>) {
    match plan {
        Plan::Filter { pred, input } => {
            rewrite_walk(input, out);
            // Filter 直接覆 Join：可下推分类（其他位置不动——投影上方
            // 的谓词可能引用计算列，v1 只处理 join 上方）
            if let Plan::Join { kind, right, .. } = &**input {
                let keys = input.scan_keys();
                if keys.len() >= 2 {
                    // LEFT join 右侧因子的单源合取项**不得移除**——AST
                    // 路径的下推是加性（join 后保留原谓词过滤 NULL 延展
                    // 行）；计划重写曾整体搬走 → LEFT 语义破坏（plan-exec
                    // 差分暴露：6 行 vs 2 行）。修：右侧项下推副本进
                    // residual 保留（hoist 仅为预过滤）。
                    let right_keys = right.scan_keys();
                    let keep_right = *kind == "left";
                    let conjuncts = crate::sql::optimize::split_conjuncts(pred);
                    let mut residual: Vec<Expr> = Vec::new();
                    let mut pushed: Vec<(String, Vec<Expr>)> = Vec::new();
                    for c in conjuncts {
                        match crate::sql::optimize::conjunct_target(&c, &keys) {
                            Some(k) => {
                                if keep_right && right_keys.contains(&k) {
                                    residual.push(c.clone());
                                }
                                match pushed.iter_mut().find(|(key, _)| *key == k) {
                                    Some(slot) => slot.1.push(c),
                                    None => pushed.push((k, vec![c])),
                                }
                            }
                            None => residual.push(c),
                        }
                    }
                    if !pushed.is_empty() {
                        hoist_filters(input, &pushed);
                        out.extend(pushed);
                        if residual.is_empty() {
                            // 整体下推完成——Filter 节点消解（LEFT 右侧项
                            // 已进 residual，不会走到这里）
                            *plan = std::mem::replace(input, Plan::Values);
                        } else {
                            *pred = crate::sql::optimize::and_all(residual)
                                .expect("residual 非空必有合取重组");
                        }
                    }
                }
            }
        }
        Plan::Join { left, right, .. } => {
            rewrite_walk(left, out);
            rewrite_walk(right, out);
        }
        Plan::Aggregate { input, .. }
        | Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input }
        | Plan::Window { input, .. } => rewrite_walk(input, out),
        Plan::SetOp { left, right, .. } => {
            rewrite_walk(left, out);
            rewrite_walk(right, out);
        }
        Plan::Cte { plan, body, .. } => {
            rewrite_walk(plan, out);
            rewrite_walk(body, out);
        }
        Plan::IterativeScan { base, recursive, .. } => {
            rewrite_walk(base, out);
            rewrite_walk(recursive, out);
        }
        Plan::Scan { .. } | Plan::Values => {}
    }
}

/// 把下推项挂到对应 Scan 上方（Filter(Scan)）；因子键按 join 树匹配
fn hoist_filters(join: &mut Plan, pushed: &[(String, Vec<Expr>)]) {
    match join {
        Plan::Join { left, right, .. } => {
            hoist_filters(left, pushed);
            hoist_filters(right, pushed);
        }
        Plan::Scan { .. } => {
            let key = match join {
                Plan::Scan { table, alias, .. } => alias
                    .clone()
                    .unwrap_or_else(|| table.to_ascii_lowercase()),
                _ => unreachable!(),
            };
            if let Some((_, cs)) = pushed.iter().find(|(k, _)| *k == key) {
                if let Some(w) = crate::sql::optimize::and_all(cs.clone()) {
                    *join = Plan::Filter { pred: w, input: Box::new(std::mem::replace(join, Plan::Values)) };
                }
            }
        }
        Plan::Filter { input, .. } => hoist_filters(input, pushed),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 打印（dendro.ir v1 plan 方言；P2 字节确定性）
// ---------------------------------------------------------------------------

/// 表 schema 查询口（打印时的列/pk 呈现；未解析 = 省略 cols 属性）
pub type SchemaLookup<'a> = &'a dyn Fn(&str) -> Option<(Vec<String>, Vec<u16>)>;

/// 从 Database 解析的打印 lookup（EXPLAIN 用）
pub fn db_schema_lookup<'a>(
    db: &'a Database,
    sess: &'a Session,
) -> impl Fn(&str) -> Option<(Vec<String>, Vec<u16>)> + 'a {
    move |t: &str| {
        crate::sql::scan::resolve_table(db, &sess.branch, t)
            .ok()
            .map(|(schema, _)| {
                (
                    schema.columns.iter().map(|c| c.name.clone()).collect(),
                    schema.pk.clone(),
                )
            })
    }
}

struct Printer<'a> {
    out: String,
    counters: [usize; 13], // v,t,s,f,j,a,p,o,u,d,w,c,i
    lookup: SchemaLookup<'a>,
    scan_ids: std::collections::HashMap<String, String>, // 因子键 → %sN
}

pub fn print_plan(name: &str, plan: &Plan, lookup: SchemaLookup<'_>) -> String {
    let mut p = Printer {
        out: String::new(),
        counters: [0; 13],
        lookup,
        scan_ids: std::collections::HashMap::new(),
    };
    p.out.push_str("dendro.ir v1\n");
    p.out.push_str(&format!("plan @{name} {{\n"));
    let top = p.emit(plan);
    p.out.push_str(&format!("  yield {top}\n"));
    p.out.push_str("}\n");
    p.out
}

const KINDS: [&str; 13] = ["v", "t", "s", "f", "j", "a", "p", "o", "u", "d", "w", "c", "i"];

impl<'a> Printer<'a> {
    fn next_id(&mut self, k: usize) -> String {
        let id = format!("%{}{}", KINDS[k], self.counters[k]);
        self.counters[k] += 1;
        id
    }

    /// 递归发射节点；返回节点 SSA 名
    fn emit(&mut self, plan: &Plan) -> String {
        match plan {
            Plan::Values => {
                let id = self.next_id(0);
                self.out.push_str(&format!("  {id} = values {{rows = 1}}\n"));
                id
            }
            Plan::Scan {
                table,
                alias,
                version,
            } => {
                let t_id = self.next_id(1);
                let alias_part = alias
                    .as_ref()
                    .map(|a| format!(" as \"{a}\""))
                    .unwrap_or_default();
                let ver_part = version
                    .as_ref()
                    .map(|v| {
                        format!(
                            ", version = {}",
                            crate::ir::text::escape_sql_text(&v.display())
                        )
                    })
                    .unwrap_or_default();
                match (self.lookup)(table) {
                    Some((cols, pk)) => {
                        let pk_str: Vec<String> =
                            pk.iter().map(|i| i.to_string()).collect();
                        self.out.push_str(&format!(
                            "  {t_id} = table \"{table}\"{alias_part} {{cols = [{cols_str}], pk = [{pk_str}]{ver_part}}}\n",
                            cols_str = cols.join(", "),
                            pk_str = pk_str.join(", "),
                        ));
                    }
                    None => self.out.push_str(&format!(
                        "  {t_id} = table \"{table}\"{alias_part}{ver_part} ! unresolved\n",
                    )),
                }
                let s_id = self.next_id(2);
                self.out.push_str(&format!("  {s_id} = scan {t_id}\n"));
                let key = alias
                    .clone()
                    .unwrap_or_else(|| table.to_ascii_lowercase());
                self.scan_ids.insert(key, s_id.clone());
                s_id
            }
            Plan::Filter { pred, input } => {
                let i = self.emit(input);
                let id = self.next_id(3);
                self.out.push_str(&format!(
                    "  {id} = filter {i}, pred {}\n",
                    crate::ir::text::escape_sql_text(&pred.to_string())
                ));
                id
            }
            Plan::Join { kind, on, left, right } => {
                let l = self.emit(left);
                let r = self.emit(right);
                let id = self.next_id(4);
                self.out.push_str(&format!(
                    "  {id} = join {kind} {l}, {r} on {}\n",
                    crate::ir::text::escape_sql_text(&on.to_string())
                ));
                id
            }
            Plan::Aggregate { keys, aggs, input } => {
                let i = self.emit(input);
                let id = self.next_id(5);
                let ks: Vec<String> = keys.iter().map(|e| e.to_string()).collect();
                let aggs_s: Vec<String> =
                    aggs.iter().map(|a| a.display.clone()).collect();
                self.out.push_str(&format!(
                    "  {id} = aggregate {i} {{keys = [{}], aggs = [{}]}}\n",
                    ks.join(", "),
                    aggs_s.join(", ")
                ));
                id
            }
            Plan::Project {
                exprs,
                names,
                wildcard,
                prefixes,
                input,
            } => {
                let i = self.emit(input);
                let id = self.next_id(6);
                if *wildcard {
                    self.out.push_str(&format!("  {id} = project {i} {{wildcard}}\n"));
                    return id;
                }
                if !prefixes.is_empty() {
                    self.out.push_str(&format!(
                        "  {id} = project {i} {{prefixes = [{}]}}\n",
                        prefixes.join(", ")
                    ));
                    return id;
                }
                let es: Vec<String> = exprs.iter().map(|e| e.to_string()).collect();
                let ns: Vec<String> = names
                    .iter()
                    .map(|n| crate::ir::text::escape_sql_text(n))
                    .collect();
                self.out.push_str(&format!(
                    "  {id} = project {i} {{exprs = [{}], names = [{}]}}\n",
                    es.join(", "),
                    ns.join(", ")
                ));
                id
            }
            Plan::Sort { keys, input } => {
                let i = self.emit(input);
                let id = self.next_id(7);
                let ks: Vec<String> = keys
                    .iter()
                    .map(|(e, asc)| format!("{e} {}", if *asc { "ASC" } else { "DESC" }))
                    .collect();
                self.out.push_str(&format!(
                    "  {id} = sort {i} {{keys = [{}]}}\n",
                    ks.join(", ")
                ));
                id
            }
            Plan::Limit {
                limit,
                offset,
                input,
            } => {
                let i = self.emit(input);
                let id = self.next_id(3); // f 池复用（filter 同前缀族）
                let attrs = match (limit, offset) {
                    (Some(l), o) => format!("{{limit = {l}, offset = {o}}}"),
                    (None, o) => format!("{{offset = {o}}}"),
                };
                self.out.push_str(&format!("  {id} = limit {i} {attrs}\n"));
                id
            }
            Plan::SetOp { op, all, left, right } => {
                let l = self.emit(left);
                let r = self.emit(right);
                let id = self.next_id(8);
                let q = if *all { "all" } else { "distinct" };
                self.out.push_str(&format!("  {id} = {op} {q} {l}, {r}\n"));
                id
            }
            Plan::Distinct { input } => {
                let i = self.emit(input);
                let id = self.next_id(9);
                self.out.push_str(&format!("  {id} = distinct {i}\n"));
                id
            }
            Plan::Window { calls, input } => {
                let i = self.emit(input);
                let id = self.next_id(10);
                let cs: Vec<String> = calls
                    .iter()
                    .map(|c| format!("{} AS {}", c.display, c.synth_col))
                    .collect();
                self.out.push_str(&format!(
                    "  {id} = window {i} {{calls = [{}]}}\n",
                    cs.join(", ")
                ));
                id
            }
            Plan::Cte {
                name,
                names,
                plan,
                body,
            } => {
                let p_id = self.emit(plan);
                let b_id = self.emit(body);
                let id = self.next_id(11);
                let cols_part = names
                    .as_ref()
                    .map(|ns| format!(" {{cols = [{}]}}", ns.join(", ")))
                    .unwrap_or_default();
                self.out.push_str(&format!(
                    "  {id} = cte \"{name}\" ({p_id}){cols_part} body {b_id}\n"
                ));
                id
            }
            Plan::IterativeScan {
                name,
                distinct,
                names,
                base,
                recursive,
            } => {
                let b = self.emit(base);
                let r = self.emit(recursive);
                let id = self.next_id(12);
                let d = if *distinct { " distinct" } else { "" };
                let cols_part = names
                    .as_ref()
                    .map(|ns| format!(" {{cols = [{}]}}", ns.join(", ")))
                    .unwrap_or_default();
                self.out.push_str(&format!(
                    "  {id} = iterate \"{name}\" base {b} step {r}{d}{cols_part}\n"
                ));
                id
            }
        }
    }
}

// ---------------------------------------------------------------------------
// O-2b：plan 方言 parser（fail-closed；P1 = parse(print(p)) 再 print 字节恒等）
// ---------------------------------------------------------------------------

/// SQL 表达式文本 → Expr（借道 parse_batch；谓词/投影两种包装）
pub(crate) fn parse_expr_text_pub(t: &str) -> Option<Expr> {
    parse_expr_text(t, false)
}

fn parse_expr_text(t: &str, as_pred: bool) -> Option<Expr> {
    let sql = if as_pred {
        format!("SELECT 1 WHERE {t}")
    } else {
        format!("SELECT {t}")
    };
    let stmts = crate::sql::parse_batch(&sql, crate::sql::SqlDialect::Pg).ok()?;
    let q = stmts.into_iter().next()?;
    match q {
        sqlparser::ast::Statement::Query(q) => match *q.body {
            SetExpr::Select(sel) => {
                if as_pred {
                    sel.selection
                } else {
                    match sel.projection.into_iter().next()? {
                        sqlparser::ast::SelectItem::UnnamedExpr(e) => Some(e),
                        sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } => Some(expr),
                        _ => None,
                    }
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// 顶层逗号切分（括号/引号深度感知；aggs/keys/exprs 列表解析用）
fn split_top_level(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\\' if in_str => i += 1,
            // 开/闭引号都翻转（原守卫 !in_str 使闭引号被忽略——
            // CASE 'lit' 形态整串判未闭合 → None，round-trip 探针暴露）
            b'"' | b'\'' => in_str = !in_str,
            b'(' | b'[' if !in_str => depth += 1,
            b')' | b']' if !in_str => depth -= 1,
            b',' if !in_str && depth == 0 => {
                out.push(s[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if in_str || depth != 0 {
        return None;
    }
    let last = s[start..].trim();
    if !last.is_empty() || !out.is_empty() {
        out.push(last.to_string());
    }
    Some(out)
}

/// 引号串解包："…"（JSON 转义）→ 文本
fn unquote(t: &str) -> Option<String> {
    let t = t.trim();
    let inner = t.strip_prefix('"')?;
    let end = crate::ir::text::find_str_end(inner)?;
    crate::ir::text::json_unescape(&inner[..end])
}

/// 解析 dendro.ir v1 plan 块（fail-closed：任何未知行/坏引用 → None）。
/// 谓词/键/投影文本经 sqlparser 回解析为 Expr；aggs 保持展示串。
/// 操作分派长链——question_mark 改写破坏标签一览性（parse_const 同例）
#[allow(clippy::question_mark)]
pub fn parse_plan(text: &str) -> Option<Plan> {
    let mut lines = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty());
    if lines.next()? != "dendro.ir v1" {
        return None;
    }
    let sig = lines.next()?;
    if !sig.starts_with("plan @") || !sig.ends_with('{') {
        return None;
    }
    // SSA 表：id → 已构建节点（树按定义序装配；子节点必先于父节点）
    let mut nodes: Vec<(String, Plan)> = Vec::new();
    let lookup_node = |nodes: &Vec<(String, Plan)>, id: &str| -> Option<Plan> {
        let id = id.trim();
        let owned;
        let key: &str = if id.starts_with('%') {
            id
        } else {
            owned = format!("%{id}");
            &owned
        };
        nodes
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, p)| p.clone())
    };
    let mut yielded: Option<Plan> = None;
    for line in lines {
        if line == "}" {
            break;
        }
        if let Some(rest) = line.strip_prefix("yield %") {
            if yielded.is_some() || !rest.starts_with(|c: char| c.is_ascii_alphanumeric()) {
                return None;
            }
            yielded = lookup_node(&nodes, &format!("%{rest}"));
            continue;
        }
        let (id, body) = line.split_once(" = ")?;
        if !id.starts_with('%') {
            return None;
        }
        let node = if let Some(t) = body.strip_prefix("values ") {
            if t != "{rows = 1}" {
                return None;
            }
            Plan::Values
        } else if body.starts_with("table ") {
            // table "name" [as "alias"] {cols = [...], pk = [...]} / ! unresolved
            // cols/pk 不入 Plan（打印时经 lookup 复查）；alias 可选
            let rest = body.strip_prefix("table ")?;
            fn ver_of(tail: &str) -> Option<Option<ScanVersion>> {
                match tail.split_once(", version = ") {
                    Some((_, v)) => {
                        let inner = v.strip_prefix('"')?; // 跳开引号
                        let end = crate::ir::text::find_str_end(inner)?;
                        Some(Some(ScanVersion::parse_display(&crate::ir::text::json_unescape(
                            &inner[..end],
                        )?)))
                    }
                    None => Some(None),
                }
            }
            if let Some((n, tail)) = rest.split_once(" as ") {
                let name = unquote(n)?;
                let alias_s = if let Some((a, _)) = tail.split_once(" !") {
                    a
                } else {
                    tail.split_once("}")?.0
                };
                let version = ver_of(tail)?;
                Plan::Scan {
                    table: name,
                    alias: Some(unquote(alias_s.trim())?),
                    version,
                }
            } else {
                // 无别名：name 后是 {attrs} 或 ! unresolved
                let (n, tail) = rest.split_once(' ')?;
                let name = unquote(n)?;
                if !tail.starts_with('{') && !tail.starts_with('!') {
                    return None;
                }
                let version = ver_of(tail)?;
                Plan::Scan {
                    table: name,
                    alias: None,
                    version,
                }
            }
        } else if let Some(t) = body.strip_prefix("scan %") {
            let child = lookup_node(&nodes, t)?;
            // scan 引用的必为 table 节点——结构校验
            match child {
                p @ Plan::Scan { .. } => p,
                _ => return None,
            }
        } else if let Some(t) = body.strip_prefix("filter %") {
            let (src, pred) = t.split_once(", pred ")?;
            let input = Box::new(lookup_node(&nodes, src.trim())?);
            let pred = parse_expr_text(&unquote(pred)?, true)?;
            Plan::Filter { pred, input }
        } else if let Some(t) = body.strip_prefix("join ") {
            let (kind, rest) = t.split_once(' ')?;
            let kind = match kind {
                "inner" => "inner",
                "left" => "left",
                _ => return None,
            };
            let (lr, on) = rest.split_once(" on ")?;
            let (l, r) = lr.split_once(", ")?;
            let on = parse_expr_text(&unquote(on)?, true)?;
            Plan::Join {
                kind,
                on,
                left: Box::new(lookup_node(&nodes, l)?),
                right: Box::new(lookup_node(&nodes, r)?),
            }
        } else if let Some(t) = body.strip_prefix("aggregate %") {
            let (src, attrs) = t.split_once(" {keys = [")?;
            let input = Box::new(lookup_node(&nodes, src.trim())?);
            let (keys_s, rest) = attrs.split_once("], aggs = [")?;
            let aggs_s = rest.strip_suffix("]}")?;
            let keys = split_top_level(keys_s)?
                .into_iter()
                .map(|k| parse_expr_text(&k, false))
                .collect::<Option<Vec<_>>>()?;
            let aggs = split_top_level(aggs_s)?
                .into_iter()
                .map(|d| crate::sql::scan::agg_call_from_display(&d).ok())
                .collect::<Option<Vec<_>>>()?;
            Plan::Aggregate { keys, aggs, input }
        } else if let Some(src) = body.strip_prefix("project %").filter(|_| body.ends_with("{wildcard}")) {
            let input = Box::new(lookup_node(&nodes, src.trim())?);
            Plan::Project {
                exprs: vec![],
                names: vec![],
                wildcard: true,
                prefixes: vec![],
                input,
            }
        } else if let Some((src, pfx)) = body
            .strip_prefix("project %")
            .and_then(|t| t.split_once(" {prefixes = ["))
            .and_then(|(s, rest)| rest.strip_suffix("]}").map(|p| (s, p)))
            .map(|(s, p)| (s.trim().to_string(), p.to_string()))
        {
            let input = Box::new(lookup_node(&nodes, &src)?);
            let prefixes = split_top_level(&pfx)?;
            Plan::Project {
                exprs: vec![],
                names: vec![],
                wildcard: false,
                prefixes,
                input,
            }
        } else if let Some(t) = body.strip_prefix("project %") {
            let (src, attrs) = t.split_once(" {exprs = [")?;
            let input = Box::new(lookup_node(&nodes, src.trim())?);
            let (exprs_s, rest) = attrs.split_once("], names = [")?;
            let names_s = rest.strip_suffix("]}")?;
            let exprs = split_top_level(exprs_s)?
                .into_iter()
                .map(|e| parse_expr_text(&e, false))
                .collect::<Option<Vec<_>>>()?;
            let names = split_top_level(names_s)?
                .into_iter()
                .map(|n| unquote(&n))
                .collect::<Option<Vec<_>>>()?;
            Plan::Project {
                exprs,
                names,
                wildcard: false,
                prefixes: vec![],
                input,
            }
        } else if let Some(t) = body.strip_prefix("sort %") {
            let (src, attrs) = t.split_once(" {keys = [")?;
            let input = Box::new(lookup_node(&nodes, src.trim())?);
            let keys_s = attrs.strip_suffix("]}")?;
            let _ = attrs;
            let keys = split_top_level(keys_s)?
                .into_iter()
                .map(|entry| {
                    if let Some(e) = entry.strip_suffix(" ASC") {
                        parse_expr_text(e, false).map(|x| (x, true))
                    } else if let Some(e) = entry.strip_suffix(" DESC") {
                        parse_expr_text(e, false).map(|x| (x, false))
                    } else {
                        None
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            Plan::Sort { keys, input }
        } else if let Some(t) = body.strip_prefix("limit %") {
            let (src, attrs) = t.split_once(" {")?;
            let input = Box::new(lookup_node(&nodes, src.trim())?);
            let attrs = attrs.strip_suffix('}')?;
            // {limit = L, offset = O} | {offset = O}
            let (limit, offset) = if let Some(rest) = attrs.strip_prefix("limit = ") {
                let (l, rest) = rest.split_once(", offset = ")?;
                (Some(l.parse().ok()?), rest.parse().ok()?)
            } else {
                let o = attrs.strip_prefix("offset = ")?;
                (None, o.parse().ok()?)
            };
            Plan::Limit {
                limit,
                offset,
                input,
            }
        } else if let Some(t) = ["union ", "except ", "intersect "]
            .iter()
            .find_map(|p| body.strip_prefix(p))
        {
            let op = match body.split(' ').next()? {
                "union" => "union",
                "except" => "except",
                _ => "intersect",
            };
            let (q, rest) = t.split_once(' ')?;
            let (l, r) = rest.split_once(", ")?;
            Plan::SetOp {
                op,
                all: q == "all",
                left: Box::new(lookup_node(&nodes, l)?),
                right: Box::new(lookup_node(&nodes, r)?),
            }
        } else if let Some(t) = body.strip_prefix("distinct %") {
            let input = Box::new(lookup_node(&nodes, t.trim())?);
            Plan::Distinct { input }
        } else if let Some(t) = body.strip_prefix("window %") {
            let (src, attrs) = t.split_once(" {calls = [")?;
            let input = Box::new(lookup_node(&nodes, src.trim())?);
            let calls_s = attrs.strip_suffix("]}")?;
            let calls = split_top_level(calls_s)?
                .into_iter()
                .map(|entry| {
                    // "<display> AS __wN"：display 回解析为 Function →
                    // WindowCall（synth 列名保留）
                    let (d, synth) = entry.rsplit_once(" AS __w")?;
                    let synth = format!("__w{synth}");
                    let e = parse_expr_text(d, false)?;
                    let f = match e {
                        Expr::Function(f) if f.over.is_some() => f,
                        _ => return None,
                    };
                    crate::sql::scan::window_call_from_fn(&f.to_string(), &f, synth).ok()
                })
                .collect::<Option<Vec<_>>>()?;
            Plan::Window { calls, input }
        } else if let Some(t) = body.strip_prefix("cte \"") {
            // cte "name" (%plan)[ {cols = [...]}] body %body
            let (name, rest) = t.split_once("\" (")?;
            let (p_id, rest) = rest.split_once(')')?;
            let plan = Box::new(lookup_node(&nodes, p_id)?);
            let (names, rest) = match rest.strip_prefix(" {cols = [") {
                Some(r) => {
                    let (ns, r2) = r.split_once("]}")?;
                    (Some(split_top_level(ns)?), r2)
                }
                None => (None, rest),
            };
            let b_id = rest.strip_prefix(" body %")?;
            let body = Box::new(lookup_node(&nodes, b_id)?);
            Plan::Cte {
                name: name.to_string(),
                names,
                plan,
                body,
            }
        } else if let Some(t) = body.strip_prefix("iterate \"") {
            // iterate "name" base %b step %r[ distinct]
            let (name, rest) = t.split_once("\" base ")?;
            let (b, rest) = rest.split_once(" step ")?;
            // step 引用后缀序：[ {cols = [...]}][ distinct]（打印序
            // distinct 在前 cols 在后——剥除从最外层后缀起）
            let (rest, names) = match rest.find(" {cols = [") {
                Some(pos) => {
                    let cs = &rest[pos + " {cols = [".len()..];
                    let (ns, _) = cs.split_once("]}")?;
                    (&rest[..pos], Some(split_top_level(ns)?))
                }
                None => (rest, None),
            };
            let (r, d) = match rest.strip_suffix(" distinct") {
                Some(rr) => (rr, true),
                None => (rest, false),
            };
            Plan::IterativeScan {
                name: name.to_string(),
                distinct: d,
                names,
                base: Box::new(lookup_node(&nodes, b)?),
                recursive: Box::new(lookup_node(&nodes, r)?),
            }
        } else {
            return None; // 未知操作 fail-closed
        };
        nodes.push((id.to_string(), node));
    }
    yielded
}

/// 计划结构校验（§4 共用）：join/集合操作子节点非空、scan 有表名、
/// sort 键非空
pub fn verify_plan(p: &Plan) -> bool {
    match p {
        Plan::Values => true,
        Plan::Scan { table, .. } => !table.is_empty(),
        Plan::Filter { input, .. } => verify_plan(input),
        Plan::Join { left, right, .. } => verify_plan(left) && verify_plan(right),
        Plan::Aggregate { keys, input, .. } => keys.len() <= 64 && verify_plan(input),
        Plan::Project { input, .. } => verify_plan(input),
        Plan::Sort { keys, input } => !keys.is_empty() && verify_plan(input),
        Plan::Limit { input, .. } => verify_plan(input),
        Plan::SetOp { left, right, .. } => verify_plan(left) && verify_plan(right),
        Plan::Distinct { input } => verify_plan(input),
        Plan::Window { calls, input } => !calls.is_empty() && verify_plan(input),
        Plan::Cte { name, plan, body, .. } => {
            !name.is_empty() && verify_plan(plan) && verify_plan(body)
        }
        Plan::IterativeScan { name, base, recursive, .. } => {
            !name.is_empty() && verify_plan(base) && verify_plan(recursive)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假 schema：orders(id,cid,total) / customers(id,region)
    fn fake_lookup() -> impl Fn(&str) -> Option<(Vec<String>, Vec<u16>)> {
        |t: &str| match t {
            "orders" => Some((
                vec!["id".into(), "cid".into(), "total".into()],
                vec![0],
            )),
            "customers" => Some((vec!["id".into(), "region".into()], vec![0])),
            _ => None,
        }
    }

    fn plan_of(sql: &str) -> Plan {
        let stmts =
            crate::sql::parse_batch(sql, crate::sql::SqlDialect::Pg).unwrap();
        match &stmts[0] {
            sqlparser::ast::Statement::Query(q) => build_plan(q).unwrap(),
            _ => panic!("expected query"),
        }
    }

    /// P2 确定性：同计划打印两次字节全等 + SSA 序稳定
    #[test]
    fn print_deterministic() {
        let p = plan_of(
            "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id \
             WHERE o.total > 60 AND c.region = 'EU' ORDER BY o.id LIMIT 5",
        );
        let t1 = print_plan("q0", &p, &fake_lookup());
        let t2 = print_plan("q0", &p, &fake_lookup());
        assert_eq!(t1, t2);
        assert!(t1.contains("dendro.ir v1\nplan @q0 {"), "{t1}");
        assert!(t1.contains("yield %"), "{t1}");
    }

    /// 下推重写：Filter 上移到各 Scan 上方；Filter 节点消解
    #[test]
    fn rewrite_hoists_filters_below_join() {
        let mut p = plan_of(
            "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id \
             WHERE o.total > 60 AND c.region = 'EU'",
        );
        let pushed = rewrite_pushdown(&mut p);
        assert_eq!(pushed.len(), 2, "{:?}", pushed);
        let text = print_plan("q0", &p, &fake_lookup());
        // 两个 filter 节点都在 scan 之上、join 之下
        assert!(text.contains("filter %s0"), "{text}");
        assert!(text.contains("filter %s1"), "{text}");
        // join 的输入是 filter 节点
        let jline = text.lines().find(|l| l.contains("join inner")).unwrap();
        assert!(jline.contains("%f0") && jline.contains("%f1"), "{jline}");
        // 顶层 Filter 已消解（无 join 上方残留 filter）
        assert!(!text.contains("filter %j0"), "{text}");
    }

    /// 跨表谓词残留：Filter 部分下推、部分留 join 上方
    #[test]
    fn rewrite_keeps_cross_table_residual() {
        let mut p = plan_of(
            "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id \
             WHERE o.total > 60 AND o.cid > c.id",
        );
        let pushed = rewrite_pushdown(&mut p);
        assert_eq!(pushed.len(), 1, "仅 o.total 下推：{:?}", pushed);
        let text = print_plan("q0", &p, &fake_lookup());
        let jline = text.lines().find(|l| l.contains("join inner")).unwrap();
        assert!(jline.contains("%f0"), "左输入带下推 filter：{jline}");
        assert!(jline.contains("%s1"), "右输入无 filter：{jline}");
        assert!(
            text.contains("filter %j0"),
            "跨表残留留在 join 上方：{text}"
        );
    }

    /// 集合操作/聚合/排序形状的计划结构
    #[test]
    fn setop_agg_sort_shapes() {
        let p = plan_of(
            "SELECT region, count(*) FROM customers GROUP BY region \
             UNION ALL SELECT 'x', 0 ORDER BY 1 LIMIT 3",
        );
        let text = print_plan("q0", &p, &fake_lookup());
        assert!(text.contains("aggregate %s0"), "{text}");
        assert!(text.contains("union all"), "{text}");
        assert!(text.contains("sort"), "{text}");
        assert!(text.contains("limit = 3"), "{text}");
    }

    /// golden（tests/golden/plans.ir）：计划文本字节级锁定——优化器
    /// 语义改动的 PR 中 diff 即计划变更审阅面。
    /// 再生成：`UPDATE_GOLDEN=1 cargo test -p dendro-core --lib ir::plan`
    #[test]
    fn golden_plans_ir() {
        let corpus = [
            "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id WHERE o.total > 60 AND c.region = 'EU'",
            "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id WHERE o.total > 60 AND o.cid > c.id",
            "SELECT o.id FROM orders o LEFT JOIN customers c ON o.cid = c.id WHERE c.region = 'EU' ORDER BY o.id LIMIT 5",
            "SELECT c.region, count(*), sum(o.total) FROM orders o JOIN customers c ON o.cid = c.id WHERE o.total >= 50 GROUP BY c.region HAVING sum(o.total) > 10 ORDER BY c.region",
            "SELECT id FROM orders UNION SELECT id FROM customers",
            "SELECT id FROM orders EXCEPT ALL SELECT id FROM customers",
            "SELECT nope.x FROM no_table nope",
            // 阶段1：Distinct / Window 节点打印（结构化 IR）
            "SELECT DISTINCT cid FROM orders",
            "SELECT DISTINCT cid FROM orders ORDER BY cid LIMIT 2",
            "SELECT id, row_number() OVER (ORDER BY total DESC) FROM orders",
            "SELECT id, sum(total) OVER (PARTITION BY cid ORDER BY id) FROM orders",
            // 阶段3：Cte / IterativeScan 打印
            "WITH RECURSIVE fib(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM fib WHERE n < 5) SELECT sum(n) FROM fib",
            "WITH big AS (SELECT id FROM orders WHERE total > 100) SELECT count(*) FROM big",
        ];
        let mut cur = String::new();
        cur.push_str("; dendro.ir v1 plans golden（生成见 ir/plan.rs tests；人工审阅后提交）\n");
        for sql in corpus {
            let mut p = plan_of(sql);
            let pushed = rewrite_pushdown(&mut p);
            let desc = pushdown_desc(&pushed);
            if !desc.is_empty() {
                cur.push_str(&format!("; corpus: {sql}\n; {desc}\n"));
            } else {
                cur.push_str(&format!("; corpus: {sql}\n"));
            }
            cur.push_str(&print_plan("q0", &p, &fake_lookup()));
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/plans.ir");
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(path, &cur).unwrap();
            return;
        }
        let want = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("golden 缺失（UPDATE_GOLDEN=1 再生成）：{e}"));
        assert_eq!(want, cur, "golden 偏移——优化器计划变更需重生成并人工审阅");
    }

    /// O-2b P1：parse(print(p)) → verify → 再 print **字节恒等**
    ///（谓词/键/投影经 sqlparser Display→parse→Display 稳定性由此锁定）
    #[test]
    fn plan_roundtrip_byte_equal() {
        let corpus = [
            "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id WHERE o.total > 60 AND c.region = 'EU'",
            "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id WHERE o.total > 60 AND o.cid > c.id",
            "SELECT o.id FROM orders o LEFT JOIN customers c ON o.cid = c.id WHERE c.region = 'EU' ORDER BY o.id DESC LIMIT 5",
            "SELECT c.region, count(*), sum(o.total) FROM orders o JOIN customers c ON o.cid = c.id WHERE o.total >= 50 GROUP BY c.region HAVING sum(o.total) > 10 ORDER BY c.region",
            "SELECT id FROM orders UNION SELECT id FROM customers",
            "SELECT id FROM orders EXCEPT ALL SELECT id FROM customers",
            "SELECT nope.x FROM no_table nope",
            "SELECT 1",
            // 复杂表达式形态（CASE / IN / BETWEEN / 算术）穿透 expr 回解析
            "SELECT CASE WHEN o.total > 100 THEN 'big' ELSE note END FROM orders o WHERE o.id IN (1, 2, 3) OR o.total BETWEEN 50 AND 60",
            // O-2c+ B：version 属性（display 文本）round-trip
            "SELECT id FROM orders FOR SYSTEM_TIME AS OF 1726400000000",
            // 阶段1：Distinct / Window / 结构化 AggCall round-trip
            "SELECT DISTINCT cid FROM orders ORDER BY cid LIMIT 2",
            "SELECT id, row_number() OVER (ORDER BY total DESC) FROM orders",
            "SELECT id, sum(total) OVER (PARTITION BY cid ORDER BY id) FROM orders WHERE total > 10",
            "SELECT c.region, count(*), sum(o.total) FROM orders o JOIN customers c ON o.cid = c.id GROUP BY c.region UNION ALL SELECT region, count(*), sum(id) FROM customers GROUP BY region",
            // 阶段3：Cte / IterativeScan round-trip
            "WITH RECURSIVE fib(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM fib WHERE n < 5) SELECT sum(n) FROM fib",
            "WITH big AS (SELECT id FROM orders WHERE total > 100) SELECT count(*) FROM big",
        ];
        for sql in corpus {
            let mut p = plan_of(sql);
            let _ = rewrite_pushdown(&mut p);
            let t1 = print_plan("q0", &p, &fake_lookup());
            let p2 = parse_plan(&t1)
                .unwrap_or_else(|| panic!("parse 失败：{sql}
{t1}"));
            assert!(verify_plan(&p2), "verify 失败：{sql}");
            let t2 = print_plan("q0", &p2, &fake_lookup());
            assert_eq!(t1, t2, "round-trip 字节不恒等：{sql}");
        }
    }

    /// fail-closed：未知操作 / 坏 SSA 引用 / 未闭合 → None
    #[test]
    fn plan_parse_fail_closed() {
        let good = print_plan(
            "q0",
            &plan_of("SELECT o.id FROM orders o WHERE o.total > 60"),
            &fake_lookup(),
        );
        assert!(parse_plan(&good).is_some());
        assert!(parse_plan(&good.replace("scan %t0", "frob %t0")).is_none());
        assert!(parse_plan(&good.replace("scan %t0", "scan %t9")).is_none());
        assert!(parse_plan(&good.replace("}\n", "")).is_none());
        assert!(parse_plan(&good.replace("dendro.ir v1", "dendro.ir v2")).is_none());
    }
}
