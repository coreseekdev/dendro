//! Plan → SQL 逆解析（unparse）与不动点 round-trip 报告。
//!
//! 动机（q21 定位链教训）：SQL → 数据的缺陷定位链路太长（装载 70 分钟
//! × 反复复现）。本模块把**前端**（解析 → 降级 → 计划）从执行侧剥离：
//!
//! - `plan_to_sql`：把**未优化**计划重构成 SQL（全括号化派生表风格——
//!   冗余但无歧义，diff 友好）
//! - `roundtrip_report`：sql → P₁ → sql₁ → P₂ → sql₂；**不动点判定**
//!   sql₁ == sql₂：
//!   - 成立 ⇒ 前端无损（解析/降级/建计划稳定）——缺陷在执行侧
//!     （掩码/管线/存储），直接去查对应层
//!   - 不成立 ⇒ 前端缺陷，报告给出首个分歧点
//!   - 不可渲染节点（WITH RECURSIVE 等）⇒ 诚实标注，不误报
//!
//! 口径：raw 计划 = eval_query 同链的降级后、重写前
//!（lower_subqueries → build_plan；rewrite_* 全部跳过）。

use crate::error::{Result, SqlError};
use crate::ir::plan::Plan;
use crate::sql::SqlDialect;

/// 渲染失败节点（诚实边界——不静默降级）
#[derive(Debug)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unparse: {} not supported", self.0)
    }
}

type UResult = std::result::Result<String, Unsupported>;

fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// 聚合调用渲染（count(*)/count(DISTINCT u)/sum(v)）
fn agg_call(f: &crate::sql::agg::AggCall) -> String {
    let d = if f.distinct { "DISTINCT " } else { "" };
    match &f.arg {
        Some(a) if !f.is_star => format!("{}({d}{a})", f.func),
        _ => format!("{}(*)", f.func),
    }
}

/// 计划 → SQL。**链式融合**：Project/Aggregate/Filter/Sort/Limit/
/// Distinct 链折叠为单条 SELECT（WHERE 在聚合下方、HAVING 在聚合
/// 上方、ORDER BY/LIMIT 同层）——与 build_plan 对 SELECT 的分解严格
/// 互逆，保证重构 SQL 再解析得到同构计划（正式形式等价的前提）。
/// 不可融合的形态（窗口/集合操作/CTE/半连接/嵌套 Project）按节点
/// 渲染为派生表。
pub fn plan_to_sql(p: &Plan) -> UResult {
    // ---- 自顶向下收集可融合链 ----
    let mut items: Option<Vec<String>> = None; // SELECT 列表（首个 Project）
    let mut group: Option<(Vec<String>, Vec<String>)> = None; // (keys, aggs)
    let mut wheres: Vec<String> = Vec::new(); // 聚合下方的谓词（= SQL WHERE）
    let mut having_pending: Vec<String> = Vec::new(); // 聚合上方暂记（遇聚合 → HAVING；链尾无聚合 → WHERE）
    let mut order: Option<Vec<String>> = None;
    let (mut limit, mut offset) = (None, 0usize);
    let mut distinct = false;
    let mut cur = p;
    loop {
        match cur {
            Plan::Limit {
                limit: l,
                offset: o,
                input,
            } => {
                if limit.is_none() {
                    limit = *l;
                    offset = *o;
                    cur = input;
                    continue;
                }
                break;
            }
            Plan::Sort { keys, input } => {
                if order.is_none() {
                    order = Some(
                        keys.iter()
                            .map(|(e, asc)| format!("{e} {}", if *asc { "ASC" } else { "DESC" }))
                            .collect(),
                    );
                    cur = input;
                    continue;
                }
                break;
            }
            Plan::Distinct { input } => {
                distinct = true;
                cur = input;
                continue;
            }
            Plan::Filter { pred, input } => {
                // 计划树中 WHERE 位于聚合**下方**（遍历遇 Aggregate 之后）、
                // HAVING 位于上方（之前）——两段式分类
                if items.is_some() && group.is_none() && !having_pending.is_empty() {
                    // 投影上方的谓词不可折进 WHERE（语义位置在投影后）
                    break;
                }
                if group.is_some() {
                    wheres.push(format!("({pred})"));
                } else {
                    having_pending.push(format!("{pred}"));
                }
                cur = input;
                continue;
            }
            Plan::Project {
                exprs,
                names,
                wildcard,
                prefixes,
                input,
            } => {
                if items.is_none() && group.is_none() {
                    let mut its: Vec<String> = Vec::new();
                    if *wildcard {
                        its.push("*".into());
                    }
                    for pfx in prefixes {
                        its.push(format!("{}.*", q(pfx)));
                    }
                    for (e, n) in exprs.iter().zip(names) {
                        its.push(format!("{e} AS {}", q(n)));
                    }
                    items = Some(its);
                    cur = input;
                    continue;
                }
                break;
            }
            Plan::Aggregate { keys, aggs, input } => {
                if group.is_none() {
                    let ks: Vec<String> = keys.iter().map(|k| format!("{k}")).collect();
                    let as_: Vec<String> = aggs.iter().map(agg_call).collect();
                    group = Some((ks, as_));
                    cur = input;
                    continue;
                }
                break;
            }
            _ => break,
        }
    }
    // ---- FROM 侧：剩余子树 ----
    let from = match cur {
        Plan::Values => return Err(Unsupported("Values（无 FROM 常量输入）".into())),
        Plan::Scan {
            table,
            alias,
            version,
        } => {
            let mut f = q(table);
            if let Some(a) = alias {
                f.push_str(&format!(" AS {}", q(a)));
            }
            if let Some(v) = version {
                f.push_str(&format!(" {}", v.display()));
            }
            f
        }
        Plan::SubqueryScan { key, plan } => {
            let inner = plan_to_sql(plan)?;
            if key.is_empty() {
                format!("({inner})")
            } else {
                format!("({inner}) AS {}", q(key))
            }
        }
        // Join 渲染为 FROM 片段（表引用/派生表侧因子）——直出
        j @ Plan::Join { .. } => plan_to_sql_node(j)?,
        other => format!("({})", plan_to_sql_node(other)?),
    };
    // ---- 组装（无 SELECT 列表但有聚合：keys+aggs 即列表）----
    let select_items = match (&items, &group) {
        (Some(its), _) => its.join(", "),
        (None, Some((ks, as_))) => {
            let mut v = ks.clone();
            v.extend(as_.iter().cloned());
            v.join(", ")
        }
        (None, None) => "*".to_string(),
    };
    let mut sql = format!(
        "SELECT {}{select_items} FROM {from}",
        if distinct { "DISTINCT " } else { "" }
    );
    // 链尾无聚合：暂记谓词实为 WHERE（普通过滤）
    let having = if group.is_some() {
        having_pending
    } else {
        wheres.append(&mut having_pending.clone());
        Vec::new()
    };
    // 谓词序：内层在前（canonical 规则 3 的合取序对齐）
    if !wheres.is_empty() {
        let mut w = wheres.clone();
        w.reverse();
        sql.push_str(&format!(" WHERE {}", w.join(" AND ")));
    }
    if let Some((ks, _)) = &group {
        if !ks.is_empty() {
            sql.push_str(&format!(" GROUP BY {}", ks.join(", ")));
        }
    }
    if !having.is_empty() {
        let mut h = having.clone();
        h.reverse();
        sql.push_str(&format!(" HAVING {}", h.join(" AND ")));
    }
    if let Some(ob) = &order {
        sql.push_str(&format!(" ORDER BY {}", ob.join(", ")));
    }
    if let Some(n) = limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    if offset > 0 {
        sql.push_str(&format!(" OFFSET {offset}"));
    }
    Ok(sql)
}

/// 节点级渲染（融合链之外的形态：Join/SemiJoin/Window/SetOp/Cte——
/// 被上方 query_of 作为派生表引用）
fn plan_to_sql_node(p: &Plan) -> UResult {
    match p {
        Plan::Join {
            kind,
            on,
            left,
            right,
        } => {
            let kw = match *kind {
                "left" => "LEFT JOIN",
                _ => "JOIN",
            };
            // 侧因子：Scan 直出表引用（别名/版本保留——重解析回 Join{Scan}
            // 同构）；其余按派生表
            let side = |p: &Plan| -> UResult {
                match p {
                    Plan::Scan {
                        table,
                        alias,
                        version,
                    } => {
                        let mut f = q(table);
                        if let Some(a) = alias {
                            f.push_str(&format!(" AS {}", q(a)));
                        }
                        if let Some(v) = version {
                            f.push_str(&format!(" {}", v.display()));
                        }
                        Ok(f)
                    }
                    Plan::SubqueryScan { key, plan } => {
                        Ok(format!("({}) AS {}", plan_to_sql(plan)?, q(key)))
                    }
                    other => Ok(format!("({})", plan_to_sql(other)?)),
                }
            };
            Ok(format!("{} {kw} {} ON {on}", side(left)?, side(right)?))
        }
        Plan::SemiJoin {
            key,
            negated,
            sub,
            input,
        } => Ok(format!(
            "SELECT * FROM ({}) WHERE {key} {}IN (SELECT * FROM ({}))",
            plan_to_sql(input)?,
            if *negated { "NOT " } else { "" },
            plan_to_sql(sub)?
        )),
        Plan::Window { calls, input } => {
            let mut its = vec!["*".to_string()];
            for (i, w) in calls.iter().enumerate() {
                let pb: Vec<String> = w.partition_by.iter().map(|e| e.to_string()).collect();
                let ob: Vec<String> = w
                    .order_by
                    .iter()
                    .map(|(e, asc)| format!("{e} {}", if *asc { "ASC" } else { "DESC" }))
                    .collect();
                let over = if pb.is_empty() && ob.is_empty() {
                    String::new()
                } else {
                    let mut inner = String::new();
                    if !pb.is_empty() {
                        inner.push_str(&format!("PARTITION BY {}", pb.join(", ")));
                    }
                    if !ob.is_empty() {
                        if !inner.is_empty() {
                            inner.push(' ');
                        }
                        inner.push_str(&format!("ORDER BY {}", ob.join(", ")));
                    }
                    format!(" OVER ({inner})")
                };
                let arg = match &w.arg {
                    Some(a) => a.to_string(),
                    None => String::new(),
                };
                its.push(format!(
                    "{}({arg}){over} AS {}",
                    w.func,
                    q(&format!("w{i}"))
                ));
            }
            Ok(format!(
                "SELECT {} FROM ({})",
                its.join(", "),
                plan_to_sql(input)?
            ))
        }
        Plan::SetOp {
            op,
            left,
            right,
            all,
        } => Ok(format!(
            "({}) {} ({})",
            plan_to_sql(left)?,
            match *op {
                "intersect" => "INTERSECT",
                "except" => "EXCEPT",
                _ if *all => "UNION ALL",
                _ => "UNION",
            },
            plan_to_sql(right)?
        )),
        Plan::Cte {
            name,
            names,
            plan,
            body,
        } => {
            let cols = match names {
                Some(ns) => format!(
                    "({})",
                    ns.iter().map(|n| q(n)).collect::<Vec<_>>().join(", ")
                ),
                None => String::new(),
            };
            Ok(format!(
                "WITH {}{cols} AS ({}) {}",
                q(name),
                plan_to_sql(plan)?,
                plan_to_sql(body)?
            ))
        }
        Plan::IterativeScan { .. } => Err(Unsupported("IterativeScan（WITH RECURSIVE）".into())),
        other => plan_to_sql(other),
    }
}

/// 不动点报告（roundtrip = unparse + compare 的组合便捷口）
#[derive(Debug, Clone)]
pub struct RoundtripReport {
    /// 未优化计划文本（dendro.ir v1 打印——EXPLAIN 同源）
    pub raw_ir: String,
    /// 重构 SQL（第一遍）
    pub sql1: String,
    /// 重构 SQL（第二遍——对照）
    pub sql2: String,
    /// **主裁决**：newIR == IR（结构等价）
    pub ir_equal: bool,
    /// 次级诊断：unparse 字符串不动点（unparse 确定性下的必要条件）
    pub fixpoint: bool,
    /// IR 正式形式文本（P₁ 的 canonical——唯一性判定的载体）
    pub canon_ir: String,
    /// P₂ 的正式形式（分歧诊断对照）
    pub canon_ir2: String,
    /// 首个分歧位置描述（失败时）
    pub divergence: Option<String>,
}

impl RoundtripReport {
    /// 快速裁决：true = 前端无损（缺陷在执行侧）
    pub fn parse_faithful(&self) -> bool {
        self.ir_equal
    }
}

/// sql → 未优化 Plan（纯前端：parse → build_plan，无降级无重写）。
/// "最原始 IR"的单一构造口——dump/unparse/compare/roundtrip 共用。
pub fn raw_plan(sql: &str, dialect: SqlDialect) -> Result<Plan> {
    use sqlparser::ast::Statement;
    let stmts = crate::sql::parse_batch(sql, dialect)?;
    let Some(Statement::Query(q)) = stmts.into_iter().next() else {
        return Err(SqlError::not_supported("单条 SELECT"));
    };
    // 纯前端口径：不做 lower_subqueries（它需要 db 上下文做表内联；
    // 子查询谓词原样透传进计划——比 eval_query 链更原始一层）。
    // build_plan 失败本身即前端证据，诚实上抛。
    Ok(crate::ir::plan::build_plan(&q)?)
}

/// dump：SQL → 未优化 IR 文本（`dendro ir` 同源）
pub fn dump_ir(sql: &str, dialect: SqlDialect) -> Result<String> {
    let p = raw_plan(sql, dialect)?;
    let none_lookup = |_t: &str| None;
    Ok(crate::ir::plan::print_plan("q0", &p, &none_lookup))
}

/// unparse：SQL → 未优化 IR → 重构 SQL'（`dendro unparse` 同源）
pub fn unparse_sql(sql: &str, dialect: SqlDialect) -> Result<String> {
    let p = raw_plan(sql, dialect)?;
    plan_to_sql(&p).map_err(|u| SqlError::not_supported(u.to_string()))
}

/// compare：两段 SQL → IR 结构等价裁决（`dendro compare` 同源）。
/// 不限于 roundtrip 自检——任意两段 SQL 的语义等价判定。
#[derive(Debug, Clone)]
pub struct CompareReport {
    pub equal: bool,
    /// 两侧 IR 文本（分歧诊断）
    pub ir_a: String,
    pub ir_b: String,
    pub divergence: Option<String>,
}

pub fn compare_sql(a: &str, b: &str, dialect: SqlDialect) -> Result<CompareReport> {
    let pa = raw_plan(a, dialect)?;
    let pb = raw_plan(b, dialect)?;
    let none_lookup = |_t: &str| None;
    // 正式形式比较（等价 ⟺ 唯一形式）；诊断打印与裁决同口径
    let (ca, cb) = (
        crate::ir::canonical::canonicalize(&pa),
        crate::ir::canonical::canonicalize(&pb),
    );
    let equal = ca == cb;
    let (ia, ib) = (
        crate::ir::plan::print_plan("q0", &ca, &none_lookup),
        crate::ir::plan::print_plan("q0", &cb, &none_lookup),
    );
    let divergence = if equal {
        None
    } else {
        Some(first_divergence(&ia, &ib))
    };
    Ok(CompareReport {
        equal,
        ir_a: ia,
        ir_b: ib,
        divergence,
    })
}

/// roundtrip：SQL → IR₁ → unparse → SQL' → IR₂，主裁决 IR₂ == IR₁
/// （字符串不动点与分歧点为诊断副产物）。
pub fn roundtrip_report(sql: &str, dialect: SqlDialect) -> Result<RoundtripReport> {
    let p1 = raw_plan(sql, dialect)?;
    let none_lookup = |_t: &str| None;
    let raw_ir = crate::ir::plan::print_plan("q0", &p1, &none_lookup);
    let sql1 = plan_to_sql(&p1).map_err(|u| SqlError::not_supported(u.to_string()))?;
    // 第二遍：重构 SQL 必须可再解析（解析失败本身就是前端缺陷证据）
    let p2 = raw_plan(&sql1, dialect)?;
    let sql2 = plan_to_sql(&p2).map_err(|u| SqlError::not_supported(u.to_string()))?;
    // 正式形式等价（canonical 唯一性）——主裁决
    let (c1, c2) = (
        crate::ir::canonical::canonicalize(&p1),
        crate::ir::canonical::canonicalize(&p2),
    );
    let ir_equal = c1 == c2;
    let canon_ir = crate::ir::plan::print_plan("q0", &c1, &none_lookup);
    let _ = &c2;
    let fixpoint = sql1 == sql2;
    let divergence = if ir_equal && fixpoint {
        None
    } else if !ir_equal {
        // 主裁决失败：正式形式对照优先
        Some(format!(
            "正式形式分歧：\n--- canon P1 ---\n{}\n--- canon P2 ---\n{}",
            crate::ir::plan::print_plan("q0", &c1, &none_lookup),
            crate::ir::plan::print_plan("q0", &c2, &none_lookup)
        ))
    } else {
        Some(format!(
            "字符串不动点失败（正式形式等价——unparse 非规范形态）：{}",
            first_divergence(&sql1, &sql2)
        ))
    };
    Ok(RoundtripReport {
        raw_ir,
        sql1,
        sql2,
        ir_equal,
        fixpoint,
        canon_ir,
        canon_ir2: crate::ir::plan::print_plan("q0", &c2, &none_lookup),
        divergence,
    })
}

/// 首个分歧（行:列 + 两侧上下文）
fn first_divergence(a: &str, b: &str) -> String {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let n = ab.len().min(bb.len());
    for i in 0..n {
        if ab[i] != bb[i] {
            fn ctx(s: &str, i: usize) -> &str {
                let lo = i.saturating_sub(30);
                s.get(lo..(i + 30).min(s.len())).unwrap_or("?")
            }
            return format!("byte {i}: sql1 …{}… vs sql2 …{}…", ctx(a, i), ctx(b, i));
        }
    }
    if ab.len() != bb.len() {
        return format!("prefix equal; len {} vs {}", ab.len(), bb.len());
    }
    "identical".into()
}
