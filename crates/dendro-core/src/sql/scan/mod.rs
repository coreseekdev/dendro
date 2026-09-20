#![allow(clippy::type_complexity)]
//! 查询执行：FROM 解析、快照扫描（prolly 树 ∪ memtx overlay）、过滤/投影/聚合/排序。

use super::agg;
use super::expr;
use crate::engine::{Database, Session};
use crate::error::{Result, SqlError};
use crate::types::{ColType, ColumnMeta, Output, RecordSet, SqlValue};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Query, Select, SelectItem, SetExpr, TableFactor,
    Value as PV,
};
use std::collections::HashMap;

// ---------- 阶段0 拆分（架构审视 §3.1）：子模块 + 再导出 ----------
// 纯移动零语义变更；子模块经 use super::* 互见，外部 scan::X 路径不变
mod cte;
mod history;
mod join;
mod plan_exec;
pub(crate) use plan_exec::expr_has_subquery;
mod point;
mod project;
mod pseudo;
mod scan_table;
mod window;
pub(crate) use history::*;
pub(crate) use join::*;
pub(crate) use plan_exec::*;
pub(crate) use point::*;
pub use project::has_column_ref;
pub use project::rows_to_record_set;
pub(crate) use project::*;
pub(crate) use pseudo::*;
pub use scan_table::resolve_table;
pub use scan_table::rows_to_batches;
pub use scan_table::rows_to_batches_typed;
pub(crate) use scan_table::*;
pub(crate) use window::*;

/// 扫描出来的表视图
#[derive(Clone)]
pub struct TableView {
    pub names: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
}

pub(crate) fn exec_query(
    db: &Database,
    sess: &mut Session,
    q: Query,
    snapshot: u64,
) -> Result<Output> {
    let _t_eval = crate::perf::enter(crate::perf::Stage::Eval);
    let view = eval_query(db, sess, &q, snapshot)?;
    crate::perf::exit(crate::perf::Stage::Eval, _t_eval);
    let _t_out = crate::perf::enter(crate::perf::Stage::OutputBuild);
    let colmeta: Vec<ColumnMeta> = view
        .names
        .iter()
        .zip(
            view.rows
                .first()
                .map(|r| r.as_slice())
                .unwrap_or(&[])
                .iter()
                .map(infer_type)
                .collect::<Vec<_>>()
                .into_iter()
                .chain(std::iter::repeat(ColType::Utf8)),
        )
        .take(view.names.len())
        .map(|(n, t)| ColumnMeta {
            name: n.clone(),
            ty: t,
        })
        .collect();
    // 列型按数据推断（空表回退 Utf8）
    let colmeta = fix_colmeta(view.rows.first(), &view.names, colmeta);
    let out = Output::Rows(RecordSet {
        columns: colmeta,
        batches: rows_to_batches(&view.names, &view.rows)?,
    });
    crate::perf::exit(crate::perf::Stage::OutputBuild, _t_out);
    Ok(out)
}

fn fix_colmeta(
    first_row: Option<&Vec<SqlValue>>,
    names: &[String],
    meta: Vec<ColumnMeta>,
) -> Vec<ColumnMeta> {
    if let Some(r) = first_row {
        meta.iter()
            .zip(r.iter())
            .enumerate()
            .map(|(i, (m, v))| ColumnMeta {
                name: names[i].clone(),
                ty: if v.is_null() { m.ty } else { infer_type(v) },
            })
            .collect()
    } else {
        meta
    }
}

fn infer_type(v: &SqlValue) -> ColType {
    match v {
        SqlValue::Null => ColType::Utf8,
        SqlValue::Bool(_) => ColType::Bool,
        SqlValue::Int32(_) => ColType::Int32,
        SqlValue::Int64(_) => ColType::Int64,
        SqlValue::Float64(_) => ColType::Float64,
        SqlValue::Utf8(_) => ColType::Utf8,
        SqlValue::Bytes(_) => ColType::Bytes,
        SqlValue::Date32(_) => ColType::Date32,
        SqlValue::TimestampMs(_) => ColType::TimestampMs,
    }
}

/// 全查询求值（不含输出构造）
pub(crate) fn eval_query(
    db: &Database,
    sess: &mut Session,
    q: &Query,
    snapshot: u64,
) -> Result<TableView> {
    // 阶段5（架构审视 §3.1 阶段5）：唯一求值路径 = 计划。
    // AST 只做 parse → lowering（子查询内联）→ build_plan →
    // （optimize 开：6 重写族）→ exec_plan。旧 AST 装配全线退役：
    // eval_from join 链 / 窗口合成列段 / 聚合双路径段 / 投影排序
    // limit 出口 / 集合操作 AST 回落 / 递归 CTE VALUES 注入。
    // optimize 开关语义收窄为"重写族开关"（差分轴 on/off 对拍重写
    // 正确性；求值路径恒为计划）；force_source/force_agg 物理轴不变
    reject_offset_comma(q)?;

    // VALUES 叶子（无 FROM 常量行集——计划不适用，直评）
    if let SetExpr::Values(vals) = &*q.body {
        let mut rows = Vec::new();
        for parens in &vals.rows {
            let mut row = Vec::new();
            for e in &parens.content {
                row.push(expr::eval(e, &[], &|_| None)?);
            }
            rows.push(row);
        }
        let n = rows.first().map(|r| r.len()).unwrap_or(0);
        let names: Vec<String> = (0..n).map(|i| format!("column{}", i + 1)).collect();
        return Ok(TableView { names, rows });
    }

    // lowering：非相关子查询内联（标量→常量 / IN→InList / EXISTS→
    // Bool；相关子查询诚实拒绝）；集合操作分支递归覆盖
    let mut q_owned = q.clone();
    lower_subqueries(db, sess, &mut q_owned.body, snapshot)?;

    // P0 性能批：Arrow 原生全局聚合捷径——SELECT agg(...) FROM table
    // （无 WHERE/GROUP BY/JOIN/ORDER BY/LIMIT/SetOp/CTE）直接在列存
    // Arrow 列上计算，免 rows_from_batches 106M SqlValue 行式转换。
    // 形态不覆盖/引擎未接 → 透传行式路径（差分安全）
    if let Some(tv) = try_arrow_global_agg(db, sess, &q_owned, snapshot)? {
        return Ok(tv);
    }
    // 统一框架内点查早退（perf 拆解定位：exec_plan/扫描机器 ~8µs——
    // pk 等值形态在计划构建前直接走 point 路径）。形态门保守：单表 +
    // 无 GROUP/ORDER/LIMIT/DISTINCT/HAVING/join/子查询 + 投影为裸列
    // 或通配。SET dendro.optimize=off 时关闭（差分轴）
    if sess.optimize_enabled && sess.force_source.is_none() {
        if let Some(tv) = try_point_early(db, sess, &q_owned, snapshot)? {
            return Ok(tv);
        }
    }
    let _t_bp = crate::perf::enter(crate::perf::Stage::BuildPlan);
    let mut plan = crate::ir::plan::build_plan(&q_owned)?;
    crate::perf::exit(crate::perf::Stage::BuildPlan, _t_bp);
    let _t_opt = crate::perf::enter(crate::perf::Stage::Optimize);
    if sess.optimize_enabled {
        crate::sql::optimize::rewrite_in_list(&mut plan);
        crate::ir::plan::rewrite_pushdown(&mut plan);
        crate::sql::optimize::rewrite_stat_prop(&mut plan, db, sess);
        crate::sql::optimize::rewrite_eq_copy(&mut plan);
        crate::sql::optimize::rewrite_eq_copy(&mut plan);
        crate::sql::optimize::rewrite_join_order(&mut plan, db, sess);
        crate::sql::optimize::rewrite_filter_order(&mut plan);
    }
    crate::perf::exit(crate::perf::Stage::Optimize, _t_opt);
    // 结构侧安全网：不可执行节点集 = 诚实报错（无回落可吞）
    if !plan_nodes_exec_ok(&plan) {
        return Err(SqlError::not_supported(format!(
            "query shape not covered by plan path: {}",
            short_str(&q_owned.body)
        )));
    }
    let masks = plan_scan_masks(db, sess, &plan);
    let mut cx = ExecCx {
        masks: &masks,
        sort_hint: None,
        metrics: None,
        depth: 0,
        bindings: Default::default(),
    };
    let (tv, _) = exec_plan(db, sess, &plan, snapshot, &mut cx)?;
    Ok(tv)
}

/// lowering：Select 臂 WHERE/HAVING 子查询内联（SetExpr 递归——
/// 集合操作分支各自覆盖；逗号多因子诚实拒绝与旧 AST 入口同口径）
fn lower_subqueries(
    db: &Database,
    sess: &mut Session,
    se: &mut SetExpr,
    snapshot: u64,
) -> Result<()> {
    match se {
        SetExpr::Select(sel) => {
            reject_multi_from(sel)?;
            // rowid 别名（方言档案行为面）：SQLite {rowid,_rowid_,oid} /
            // MySQL {_rowid} → 单列整数 PK 列；PG 空（不映射——
            // 引用未声明列按 undefined column 响亮，PG 忠实）。
            // 真列名优先，多因子裸名歧义不动留给响亮报错
            rewrite_rowid_refs(db, sess, sel);
            if let Some(w) = sel.selection.as_mut() {
                // L1/L2：WHERE 合取位的 InSubquery 保留在谓词——
                // 非相关由 build_select 落 SemiJoin（单次求值 + 哈希
                // 探测，免 InList 字面量物化）；其余合取项与相关
                // 透传（Filter 执行臂迭代求值）交由 inline 处理
                let conjuncts = crate::sql::optimize::flatten_and(w);
                let mut out: Vec<Expr> = Vec::with_capacity(conjuncts.len());
                for c in conjuncts {
                    if crate::sql::optimize::as_semi_candidate(&c).is_some() {
                        out.push(c);
                    } else {
                        let mut c = c;
                        crate::sql::optimize::inline_subqueries(db, sess, &mut c, snapshot, true)?;
                        out.push(c);
                    }
                }
                *w = crate::sql::optimize::fold_and(out)
                    .expect("flatten_and 对 Some 谓词必产出非空序列");
            }
            if let Some(h) = sel.having.as_mut() {
                // HAVING 位：聚合输出层无外层行上下文——相关即拒绝
                crate::sql::optimize::inline_subqueries(db, sess, h, snapshot, false)?;
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            lower_subqueries(db, sess, left, snapshot)?;
            lower_subqueries(db, sess, right, snapshot)?;
        }
        SetExpr::Query(inner) => {
            lower_subqueries(db, sess, &mut inner.body, snapshot)?;
        }
        _ => {}
    }
    Ok(())
}

/// 同 apply_predicates，携带调用方解析器（#28：join 后限定名按因子
/// 布局解析——两侧同名列不错读）
fn apply_predicates_q(
    mut tv: TableView,
    w: &Expr,
    sess: &Session,
    resolve: &dyn Fn(&str) -> Option<usize>,
) -> Result<TableView> {
    // 诊断富化：undefined_column 携带当前布局的可用列名（窄行后列缺席
    // 类缺陷的定位面——10M q21 "URL does not exist" 一次锁定错位层）
    let avail = tv.names.join(", ");
    let enrich = |e: SqlError| match &*e.state {
        "42703" => {
            SqlError::undefined_column(format!("{msg} (available: [{avail}])", msg = e.message,))
        }
        _ => e,
    };
    // cols 借用收敛在块内（闭包持有生命周期——外提会锁死结尾的 tv 移动）
    let rows: Vec<Vec<SqlValue>> = {
        match crate::sql::scalar::compile_predicate_cached(w, resolve, tv.names.len(), &tv.names) {
            Ok(cp) if tv.rows.len() > 64 => {
                let mut cx = crate::exec::pipeline::PipeCtx::new(
                    vec![],
                    sess.stmt_deadline,
                    sess.cancel_token.clone(),
                );
                let all = std::mem::take(&mut tv.rows);
                let mut src = std::iter::once(Ok(all));
                let mut op = crate::exec::pipeline::FilterOp::new(cp.prog);
                let mut sink = crate::exec::pipeline::CollectSink::new(None);
                crate::exec::pipeline::drive(&mut cx, &mut src, &mut op, &mut sink)
                    .map_err(enrich)?;
                sink.rows
            }
            // 小结果集走紧循环（点查 1 行：管线包装的常数开销在 µs 级
            // 路径不可接受——bench 实证 315k→200k；两路径语义一致）
            Ok(cp) => {
                let mut filtered = Vec::with_capacity(tv.rows.len());
                for row in tv.rows.drain(..) {
                    let mut out = SqlValue::Null;
                    crate::sql::scalar::eval_row(&cp.prog, &row, &[], &mut out)?;
                    if matches!(out, SqlValue::Bool(true)) {
                        filtered.push(row);
                    }
                }
                filtered
            }
            Err(_) => {
                // P0-2 修（架构评审）：原 matches! 吞 Err → 静默丢行
                //（count=0 假象——apply_predicates_q 是子查询 bug 的
                // 最后放大器）。改为首个 Err 上抛——"求值错误 → 语句
                // 失败 不静默吞"的文档承诺在回退分支同样成立
                let mut filtered = Vec::with_capacity(tv.rows.len());
                for row in tv.rows.drain(..) {
                    match expr::eval(w, &row, resolve) {
                        Ok(SqlValue::Bool(true)) => filtered.push(row),
                        Ok(_) => {} // NULL/false → 丢行（正确语义）
                        Err(e) => return Err(enrich(e)),
                    }
                }
                filtered
            }
        }
    };
    tv.rows = rows;
    Ok(tv)
}

/// 点查早退（统一框架内——非旁路 API）：pk 等值形态的最窄安全门。
/// 返回 None = 形态不覆盖，透传计划路径（差分安全）。
fn try_point_early(
    db: &Database,
    sess: &mut Session,
    q: &Query,
    snapshot: u64,
) -> Result<Option<TableView>> {
    use sqlparser::ast::{SelectItem, SetExpr, TableFactor};
    let SetExpr::Select(sel) = &*q.body else {
        return Ok(None);
    };
    if q.order_by.is_some()
        || q.limit_clause.is_some()
        || q.fetch.is_some()
        || sel.distinct.is_some()
        || sel.having.is_some()
        || !matches!(&sel.group_by, sqlparser::ast::GroupByExpr::Expressions(es, _) if es.is_empty())
        || sel.from.len() != 1
        || !sel.from[0].joins.is_empty()
    {
        return Ok(None);
    }
    let TableFactor::Table { name, version, .. } = &sel.from[0].relation else {
        return Ok(None);
    };
    if version.is_some() {
        return Ok(None);
    }
    let Some(pred) = &sel.selection else {
        return Ok(None);
    };
    if crate::sql::scan::expr_has_subquery(pred) {
        return Ok(None);
    }
    let table = name
        .0
        .last()
        .and_then(|p| p.as_ident())
        .map(|i| i.value.clone())
        .unwrap_or_default();
    if table.is_empty() {
        return Ok(None);
    }
    // 复用下推资格判定 + 点取（与计划路径同一实现——语义一致由构造保证）
    let tf = crate::sql::scan::synthetic_tf(&table, None);
    let Some((schema, entry)) =
        crate::sql::scan::try_pk_pushdown(db, sess, &tf, Some(pred), snapshot)?
    else {
        return Ok(None);
    };
    // 投影形态先判（裸列/通配才覆盖——聚合/表达式必须回落计划
    // 路径：空集聚合返回单行 0，直出会得 0 行——ap_txn q14 实证）
    let mut out_idx: Vec<usize> = Vec::new();
    let mut wildcard = false;
    for item in &sel.projection {
        match item {
            SelectItem::Wildcard(_) => wildcard = true,
            SelectItem::UnnamedExpr(sqlparser::ast::Expr::Identifier(id)) => {
                let Some(ci) = schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&id.value))
                else {
                    return Ok(None); // 未知名——回落（诚实）
                };
                out_idx.push(ci);
            }
            _ => return Ok(None), // 表达式/聚合/限定通配——回落计划路径
        }
    }
    // 字节级点取（免中转：不解码全行/不建中间 TableView）
    let key = crate::sql::scan::point_key_of(pred, &schema)?;
    let Some(bytes) =
        crate::sql::scan::fetch_row_bytes(db, sess, &entry, &key, snapshot)?
    else {
        // 键不存在：0 行（与计划路径点取语义一致）
        return Ok(Some(TableView {
            names: Vec::new(),
            rows: Vec::new(),
        }));
    };
    let full = row_from_bytes(&schema, &bytes)?;
    // 直出（无中转行拷贝）
    let out_names: Vec<String> = if wildcard {
        schema.columns.iter().map(|c| c.name.clone()).collect()
    } else {
        out_idx
            .iter()
            .map(|&i| schema.columns[i].name.clone())
            .collect()
    };
    let out_row: Vec<SqlValue> = if wildcard {
        full
    } else {
        out_idx.iter().map(|&i| full[i].clone()).collect()
    };
    Ok(Some(TableView {
        names: out_names,
        rows: vec![out_row],
    }))
}

pub(crate) fn short_str_pub(s: &impl std::fmt::Display) -> String {
    short_str(s)
}

fn short_str(s: &impl std::fmt::Display) -> String {
    s.to_string().chars().take(40).collect()
}

pub(crate) fn describe_query(
    db: &Database,
    sess: &mut Session,
    q: &Query,
) -> Result<Vec<ColumnMeta>> {
    // 优先静态推断（schema/聚合规则）；失败回退空跑 LIMIT 0
    if let Some(meta) = describe_static(db, sess, q) {
        return Ok(meta);
    }
    let snapshot = sess.implicit_snapshot(db)?;
    let mut q2 = q.clone();
    q2.limit_clause = Some(sqlparser::ast::LimitClause::LimitOffset {
        limit: Some(Expr::Value(sqlparser::ast::ValueWithSpan {
            value: PV::Number("0".into(), false),
            span: sqlparser::tokenizer::Span::empty(),
        })),
        offset: None,
        limit_by: vec![],
    });
    let view = eval_query(db, sess, &q2, snapshot)?;
    let tys = if view.names.is_empty() {
        vec![]
    } else {
        (0..view.names.len())
            .map(|i| {
                view.rows
                    .first()
                    .map(|r| infer_type(&r[i]))
                    .unwrap_or(ColType::Utf8)
            })
            .collect()
    };
    Ok(view
        .names
        .iter()
        .zip(tys)
        .map(|(n, t)| ColumnMeta {
            name: n.clone(),
            ty: t,
        })
        .collect())
}

/// 静态列型：单表 SELECT 的 schema 直查 + 聚合函数规则
fn describe_static(db: &Database, sess: &mut Session, q: &Query) -> Option<Vec<ColumnMeta>> {
    let select = match q.body.as_ref() {
        SetExpr::Select(s) => s.as_ref(),
        _ => return None,
    };
    // 单表且无 join 才做静态
    let twj = select.from.first()?;
    if !twj.joins.is_empty() {
        return None;
    }
    let full = match &twj.relation {
        TableFactor::Table { name, .. } => name
            .0
            .iter()
            .filter_map(|p| p.as_ident())
            .map(|i| i.value.clone())
            .collect::<Vec<_>>()
            .join("."),
        TableFactor::Derived { .. } => {
            // 子查询：递归 describe 取列
            let inner = eval_query(db, sess, subquery_of(&twj.relation)?, 0).ok();
            return inner.map(|v| {
                v.names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| ColumnMeta {
                        name: n.clone(),
                        ty: v
                            .rows
                            .first()
                            .map(|r| infer_type(&r[i]))
                            .unwrap_or(ColType::Utf8),
                    })
                    .collect()
            });
        }
        _ => return None,
    };
    let (schema, _) = resolve_table(db, &sess.branch, &full).ok()?;
    let col_ty = |c: &str| schema.col_index(c).map(|i| schema.columns[i].ty);
    let agg_ty = |name: &str, arg: Option<&Expr>| -> ColType {
        match name {
            "count" => ColType::Int64,
            "avg" => ColType::Float64,
            "sum" => arg
                .and_then(|e| match e {
                    Expr::Identifier(id) => col_ty(&id.value),
                    _ => None,
                })
                .unwrap_or(ColType::Int64),
            _ => arg
                .and_then(|e| match e {
                    Expr::Identifier(id) => col_ty(&id.value),
                    _ => None,
                })
                .unwrap_or(ColType::Utf8),
        }
    };
    let mut out = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => {
                for c in &schema.columns {
                    out.push(ColumnMeta {
                        name: c.name.clone(),
                        ty: c.ty,
                    });
                }
            }
            SelectItem::QualifiedWildcard(_, _) => return None,
            SelectItem::ExprWithAlias { expr, alias } => {
                let ty = col_ty(&alias.value).unwrap_or_else(|| expr_ty(expr, &col_ty, &agg_ty));
                out.push(ColumnMeta {
                    name: alias.value.clone(),
                    ty,
                });
            }
            SelectItem::UnnamedExpr(e) => {
                let name = match e {
                    Expr::Identifier(id) => id.value.clone(),
                    other => crate::sql::agg_display(other),
                };
                let ty = expr_ty(e, &col_ty, &agg_ty);
                out.push(ColumnMeta { name, ty });
            }
            SelectItem::ExprWithAliases { .. } => return None,
        }
    }
    Some(out)
}

fn subquery_of(tf: &TableFactor) -> Option<&Query> {
    match tf {
        TableFactor::Derived { subquery, .. } => Some(subquery),
        _ => None,
    }
}

fn expr_ty(
    e: &Expr,
    col_ty: &dyn Fn(&str) -> Option<ColType>,
    agg: &dyn Fn(&str, Option<&Expr>) -> ColType,
) -> ColType {
    match e {
        Expr::Identifier(id) => col_ty(&id.value).unwrap_or(ColType::Utf8),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|p| col_ty(&p.value).unwrap_or(ColType::Utf8))
            .unwrap_or(ColType::Utf8),
        Expr::Function(f) => {
            let n = f.name.to_string().to_ascii_lowercase();
            if matches!(n.as_str(), "count" | "sum" | "avg" | "min" | "max") {
                let arg = crate::sql::scan::fn_args(f).first().and_then(|a| match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                    _ => None,
                });
                agg(&n, arg)
            } else if matches!(n.as_str(), "length" | "char_length") {
                ColType::Int32
            } else if matches!(
                n.as_str(),
                "round" | "floor" | "ceil" | "sqrt" | "pow" | "avg"
            ) {
                ColType::Float64
            } else {
                ColType::Utf8
            }
        }
        Expr::Cast { data_type, .. } => {
            ColType::from_parse(&data_type.to_string()).unwrap_or(ColType::Utf8)
        }
        Expr::BinaryOp { .. } => {
            // 数值运算 → 保守 Float64；比较 → bool
            ColType::Float64
        }
        _ => ColType::Utf8,
    }
}

pub(crate) fn eval_const(v: &Expr) -> Result<i64> {
    let v = expr::eval(v, &[], &|_| None)?;
    expr::as_i64(&v)
}

fn col_lookup(names: &[String]) -> impl Fn(&str) -> Option<usize> + '_ {
    move |name: &str| {
        let low = name.to_ascii_lowercase();
        // 精确匹配（含限定名 e.id / 表别名.列名）
        if let Some(pos) = names.iter().position(|n| n.to_ascii_lowercase() == low) {
            return Some(pos);
        }
        // 限定名回退：`d.id` → 尝试匹配裸列名 `id`（JOIN 输出的 TableView
        // 列名不含表别名前缀——col_lookup 需消解限定名差异，否则 JOIN 后
        // WHERE 过滤列查找失败 → 行被静默丢弃）
        if let Some(dot) = low.rfind('.') {
            let bare = &low[dot + 1..];
            return names.iter().position(|n| {
                let nl = n.to_ascii_lowercase();
                nl == bare || nl.ends_with(bare)
            });
        }
        None
    }
}

fn cols_lookup(names: &[String]) -> HashMap<String, usize> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.to_ascii_lowercase(), i))
        .collect()
}

/// join 后的因子列布局（#28 修复）：因子键 → (列区间起点, 区间宽,
/// 因子内列名)。限定名 `o.id` 先按因子内解析，杜绝跨侧同名列错读
/// （原 cols_lookup HashMap 重复键 last-wins：o.id 读到右侧 id 列）。
pub type FactorLayout = Vec<(String, usize, usize, Vec<String>)>;

/// 限定名优先的列解析：`alias.col` / `table.col` → 因子区间内定位；
/// 裸名 → 全名空间首匹配（现状语义）。因子未命中回退全空间（派生表等）。
pub fn resolve_qualified(layout: &FactorLayout, names: &[String], name: &str) -> Option<usize> {
    let low = name.to_ascii_lowercase();
    if let Some((prefix, col)) = low.split_once('.') {
        if let Some((_, start, _, local)) = layout.iter().find(|(k, _, _, _)| *k == prefix) {
            // 因子内定位（start 已是该因子的绝对偏移；local 长度即区间宽）
            if let Some(i) = local.iter().position(|n| n.eq_ignore_ascii_case(col)) {
                return Some(start + i);
            }
            return None; // 前缀命中但列不在因子内：不回退（防跨侧误读）
        }
    }
    names.iter().position(|n| n.to_ascii_lowercase() == low)
}

// ---------- FROM ----------

/// 窗口函数检测（止血——防 `sum() OVER()` 被当普通聚合静默塌缩）。
/// 遍历 Select 的投影/HAVING/ORDER BY，找 `Expr::Function` 带 `over`
/// 逗号多因子 FROM 显式拒绝（P0-5：原静默丢 t2+——`FROM t1,t2` ≡
/// `FROM t1`，评审确认 eval_from/build_select 均只取 first()）
pub(crate) fn reject_multi_from(select: &Select) -> Result<()> {
    if select.from.len() > 1 {
        return Err(SqlError::not_supported(format!(
            "comma-separated FROM ({} factors) — use explicit JOIN",
            select.from.len()
        )));
    }
    Ok(())
}

/// MySQL `LIMIT offset, count` 变体显式拒绝（P0-4：build_plan 只 match
/// LimitOffset，OffsetCommaLimit 静默无 Limit 节点 → MySQL 分页返回全行）
pub(crate) fn reject_offset_comma(q: &Query) -> Result<()> {
    if let Some(lc) = &q.limit_clause {
        if matches!(lc, sqlparser::ast::LimitClause::OffsetCommaLimit { .. }) {
            return Err(SqlError::not_supported(
                "LIMIT offset, count (MySQL comma syntax) — use LIMIT n OFFSET m",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// P0：窗口函数实现（OVER PARTITION BY / ORDER BY + 排名 + 裸聚合）
// 位置：eval_select 投影前（全输入行上下文——合成列 + 投影重写）
// v1 支持：row_number/rank/dense_rank + sum/count/min/max/avg OVER
// v1 拒绝：named window / window frame / LAG/LEAD（OFFSET 表达式）
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// SQLite rowid 别名（INTEGER PRIMARY KEY 即 rowid——SQLite 语义）。
// 查询期 AST 改写：rowid/_rowid_/oid（裸名或限定名）→ PK 列引用。
// 真列名优先（表已声明同名列则不动）；多因子下裸名歧义不动（执行期
// undefined column 响亮）；非整数 PK 表不动（同左）
// ---------------------------------------------------------------------------

fn rewrite_rowid_refs(db: &Database, sess: &Session, sel: &mut Select) {
    use sqlparser::ast::VisitMut;

    // 别名集来自方言档案（PG 空集 = 直接返回，零开销）
    let aliases = sess.dialect.profile().rowid_aliases();
    if aliases.is_empty() {
        return;
    }
    let is_rowid_name = |n: &str| {
        let ln = n.to_ascii_lowercase();
        aliases.iter().any(|a| *a == ln)
    };

    // 因子收集：(因子键, 表名)——from 首因子 + join 链（仅物理表；
    // 派生表/CTE 无 rowid 概念）
    let mut factors: Vec<(String, String)> = Vec::new();
    fn table_of(tf: &sqlparser::ast::TableFactor) -> Option<(String, String)> {
        if let sqlparser::ast::TableFactor::Table { name, alias, .. } = tf {
            let base = name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(|i| i.value.clone())?;
            let key = alias
                .as_ref()
                .map(|a| a.name.value.to_ascii_lowercase())
                .unwrap_or_else(|| base.to_ascii_lowercase());
            return Some((key, base));
        }
        None
    }
    for twj in sel.from.iter() {
        if let Some(f) = table_of(&twj.relation) {
            factors.push(f);
        }
    }
    for j in sel.from.iter().flat_map(|f| f.joins.iter()) {
        if let Some(f) = table_of(&j.relation) {
            factors.push(f);
        }
    }
    if factors.is_empty() {
        return;
    }
    // 因子键 → PK 列名（可别名解析的才登记）
    let mut pk_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (k, t) in &factors {
        if let Ok((schema, _)) = crate::sql::scan::resolve_table(db, &sess.branch, t) {
            if schema.pk.len() == 1 {
                let pi = schema.pk[0] as usize;
                let int_pk = matches!(
                    schema.columns.get(pi).map(|c| c.ty),
                    Some(crate::types::ColType::Int32 | crate::types::ColType::Int64)
                );
                if int_pk {
                    // 真列名优先：表已声明 rowid/_rowid_/oid 列则不可别名
                    let shadowed = schema.columns.iter().any(|c| is_rowid_name(&c.name));
                    if !shadowed {
                        if let Some(pc) = schema.columns.get(pi) {
                            pk_of.insert(k.clone(), pc.name.clone());
                        }
                    }
                }
            }
        }
    }
    if pk_of.is_empty() {
        return;
    }
    let single = factors.len() == 1;
    let _ = sel.visit(&mut RowidRewriter {
        pk_of,
        single,
        aliases,
    });
}

struct RowidRewriter {
    pk_of: std::collections::HashMap<String, String>,
    single: bool,
    aliases: &'static [&'static str],
}
impl RowidRewriter {
    fn is_rowid_name(&self, n: &str) -> bool {
        let ln = n.to_ascii_lowercase();
        self.aliases.iter().any(|a| *a == ln)
    }
}
impl sqlparser::ast::VisitorMut for RowidRewriter {
    type Break = ();
    fn pre_visit_expr(&mut self, e: &mut Expr) -> std::ops::ControlFlow<Self::Break> {
        match e {
            // 裸 rowid：仅单因子无歧义
            Expr::Identifier(id) if self.single && self.is_rowid_name(&id.value) => {
                let pk = self.pk_of.values().next().unwrap().clone();
                *e = Expr::Identifier(sqlparser::ast::Ident::new(pk));
            }
            // 限定 t.rowid：按因子键解析
            Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                let p = parts[0].value.clone();
                let c = parts[1].value.clone();
                if self.is_rowid_name(&c) {
                    if let Some(pk) = self.pk_of.get(&p.to_ascii_lowercase()) {
                        parts[1].value = pk.clone();
                    }
                }
            }
            _ => {}
        }
        std::ops::ControlFlow::Continue(())
    }
}

// ---------------------------------------------------------------------------
// P0 性能批：Arrow 原生全局聚合捷径。形态判定：单表 + 仅全局聚合 +
// 无 WHERE/HAVING/GROUP BY/ORDER BY/LIMIT/DISTINCT/SetOp/CTE/窗口。
// 覆盖 ClickBench q01-q07 类查询（COUNT(*)/SUM/AVG/MIN/MAX）。
// 返回 Some(tv) = 捷径产出；None = 回落行式（差分安全）
// ---------------------------------------------------------------------------
fn try_arrow_global_agg(
    db: &Database,
    sess: &mut Session,
    q: &Query,
    snapshot: u64,
) -> Result<Option<TableView>> {
    let sel = match &*q.body {
        SetExpr::Select(sel) => sel,
        _ => return Ok(None),
    };
    // SET dendro.optimize=off 时关闭（差分测试的行式对照入口）
    if !sess.optimize_enabled {
        return Ok(None);
    }
    if q.with.is_some() || q.order_by.is_some() || q.limit_clause.is_some() || q.fetch.is_some() {
        return Ok(None);
    }
    if sel.from.len() != 1
        || !sel.from[0].joins.is_empty()
        || sel.selection.is_some()
        || sel.having.is_some()
        || sel.distinct.is_some()
    {
        return Ok(None);
    }
    if !matches!(&sel.group_by, sqlparser::ast::GroupByExpr::Expressions(es, _) if es.is_empty()) {
        return Ok(None);
    }
    // 单表
    let sqlparser::ast::TableFactor::Table { name, .. } = &sel.from[0].relation else {
        return Ok(None);
    };
    let Some(table) = name
        .0
        .last()
        .and_then(|p| p.as_ident())
        .map(|i| i.value.clone())
    else {
        return Ok(None);
    };
    // 投影全部是聚合函数（无裸列引用）
    let mut reqs = Vec::new();
    for item in &sel.projection {
        let e = match item {
            SelectItem::UnnamedExpr(e) => e,
            SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => return Ok(None), // 通配不支持
        };
        let Expr::Function(f) = e else {
            return Ok(None);
        };
        if f.over.is_some() {
            return Ok(None);
        }
        let fname = f.name.to_string().to_ascii_lowercase();
        // DISTINCT 形态（count/sum 去重）捷径不覆盖——回落行式
        if crate::sql::scan::fn_distinct(f) {
            return Ok(None);
        }
        let (kind, col) = match fname.as_str() {
            "count" | "count_star" => {
                // count(*)：无参或通配符；count(col)：单裸列
                let args = crate::sql::scan::fn_args(f);
                let is_star = args.is_empty()
                    || matches!(
                        &args[0],
                        sqlparser::ast::FunctionArg::Unnamed(
                            sqlparser::ast::FunctionArgExpr::Wildcard
                        )
                    );
                if is_star {
                    ("count_star".to_string(), None)
                } else if args.len() == 1 {
                    let Some(col) = ident_of(&args[0]) else {
                        return Ok(None);
                    };
                    ("count".to_string(), Some(col))
                } else {
                    return Ok(None);
                }
            }
            "sum" | "avg" | "min" | "max" => {
                let args = crate::sql::scan::fn_args(f);
                if args.len() != 1 {
                    return Ok(None);
                }
                let Some(col) = ident_of(&args[0]) else {
                    return Ok(None);
                };
                (fname.clone(), Some(col))
            }
            _ => return Ok(None), // 非全局聚合函数——回落
        };
        reqs.push(crate::versioned::GlobalAggReq { kind, col });
    }
    if reqs.is_empty() {
        return Ok(None);
    }
    // 引擎层执行
    let Some(ap) = db.columnar() else {
        return Ok(None);
    };
    let Ok((schema, entry)) = resolve_table(db, &sess.branch, &table) else {
        return Ok(None);
    };
    if entry.col_segments.is_empty() {
        return Ok(None);
    }
    // 未物化增量门（差分安全的硬前提）：捷径只读不可变段，
    // memtable overlay / 显式事务写 / col_deletes 任一非空都会让
    // 段单独求值丢行或多数——必须回落三路归并的行式路径
    if !entry.col_deletes.is_empty() {
        return Ok(None);
    }
    if db
        .branch(&sess.branch)?
        .mem
        .table(entry.id)
        .has_visible_rows(snapshot)
    {
        return Ok(None);
    }
    if let Some(t) = &sess.txn {
        if t.explicit && t.writes.keys().any(|(tid, _)| *tid == entry.id) {
            return Ok(None);
        }
    }
    match ap.global_agg(db.obj_store(), &schema, &entry.col_segments, &reqs) {
        Some(Ok(vals)) => {
            let names: Vec<String> = sel
                .projection
                .iter()
                .map(|i| match i {
                    SelectItem::UnnamedExpr(e) => short_str_pub(e),
                    SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                    _ => String::new(),
                })
                .collect();
            Ok(Some(TableView {
                names,
                rows: vec![vals],
            }))
        }
        Some(Err(_)) => Ok(None), // 执行错误——回落行式（行式会给出具体报错）
        None => Ok(None),         // 引擎不覆盖——回落
    }
}

/// 裸列引用 → 列名（聚合参数形态）
fn ident_of(a: &sqlparser::ast::FunctionArg) -> Option<String> {
    use sqlparser::ast::FunctionArgExpr;
    match a {
        sqlparser::ast::FunctionArg::Unnamed(FunctionArgExpr::Expr(
            sqlparser::ast::Expr::Identifier(id),
        )) => Some(id.value.clone()),
        _ => None,
    }
}
