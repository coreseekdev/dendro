//! 聚合：GROUP BY 分组 + count/sum/avg/min/max（行式）。
#![allow(clippy::type_complexity)]

use super::expr;
use super::scan::TableView;
use crate::error::{Result, SqlError};
use crate::types::SqlValue;
use sqlparser::ast::Expr;
use std::cmp::Ordering;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AggCall {
    pub func: String, // count/sum/avg/min/max
    pub arg: Option<Expr>,
    pub distinct: bool,
    pub is_star: bool,
    pub display: String, // 原文，输出列名 & 求值期匹配
}

#[derive(Clone)]
struct Accum {
    count: u64,
    sum_f: f64,
    sum_i: i64,
    is_float: bool,
    min: Option<SqlValue>,
    max: Option<SqlValue>,
    distinct: Option<std::collections::HashSet<String>>,
}

impl Accum {
    fn new() -> Self {
        Self { count: 0, sum_f: 0.0, sum_i: 0, is_float: false, min: None, max: None, distinct: None }
    }
    fn push(&mut self, v: Option<SqlValue>, distinct: bool) -> Result<()> {
        match v {
            None => {
                // count(*) 路径
                self.count += 1;
                Ok(())
            }
            Some(val) => {
                if val.is_null() {
                    return Ok(());
                }
                if distinct {
                    self.distinct.get_or_insert_with(std::collections::HashSet::new).insert(expr::to_text(val.clone()));
                }
                self.count += 1;
                match &val {
                    SqlValue::Float64(f) => {
                        self.is_float = true;
                        self.sum_f += *f;
                    }
                    v => {
                        let i = expr::as_i64(v)?;
                        self.sum_i += i;
                    }
                }
                // min/max 更新（val 保持所有权，按需克隆）
                match &self.min {
                    None => self.min = Some(val.clone()),
                    Some(m) => {
                        if expr::cmp_values(&val, m)? == Ordering::Less {
                            self.min = Some(val.clone());
                        }
                    }
                }
                match &self.max {
                    None => self.max = Some(val),
                    Some(m) => {
                        if expr::cmp_values(&val, m)? == Ordering::Greater {
                            self.max = Some(val.clone());
                        }
                    }
                }
                Ok(())
            }
        }
    }
    fn finish(&self, func: &str, distinct: bool) -> SqlValue {
        match func {
            "count" => {
                let n = if distinct { self.distinct.as_ref().map(|s| s.len() as u64).unwrap_or(0) } else { self.count };
                SqlValue::Int64(n as i64)
            }
            "sum" => {
                if self.count == 0 {
                    return SqlValue::Null;
                }
                if self.is_float {
                    SqlValue::Float64(self.sum_f)
                } else {
                    SqlValue::Int64(self.sum_i)
                }
            }
            "avg" => {
                if self.count == 0 {
                    return SqlValue::Null;
                }
                let total = if self.is_float { self.sum_f } else { self.sum_i as f64 };
                SqlValue::Float64(total / self.count as f64)
            }
            "min" => self.min.clone().unwrap_or(SqlValue::Null),
            "max" => self.max.clone().unwrap_or(SqlValue::Null),
            _ => SqlValue::Null,
        }
    }
}

/// 聚合结果：每组 = (组键值 SqlValue, 聚合值 SqlValue)；顺序与 group_exprs/calls 对齐
pub struct AggResult {
    pub keys: Vec<Vec<SqlValue>>,   // 每组组键（group_exprs 顺序）
    pub vals: Vec<Vec<SqlValue>>,   // 每组聚合值（calls 顺序）
}

/// 执行分组聚合（HAVING 由调用方在拿到 keys/vals 后求值）
pub fn group_aggregate(
    tv: &TableView,
    group_exprs: &[Expr],
    calls: &[AggCall],
    cols: &HashMap<String, usize>,
) -> Result<AggResult> {
    let colfn = |name: &str| cols.get(&name.to_ascii_lowercase()).copied();
    let mut groups: HashMap<Vec<String>, Vec<Accum>> = HashMap::new();
    let mut order: Vec<(Vec<String>, Vec<SqlValue>)> = Vec::new(); // (hashkey, 组键值)
    for row in &tv.rows {
        let mut hashkey = Vec::with_capacity(group_exprs.len());
        let mut keyvals = Vec::with_capacity(group_exprs.len());
        for g in group_exprs {
            let v = expr::eval(g, row, &colfn)?;
            hashkey.push(expr::to_text(v.clone()));
            keyvals.push(v);
        }
        let g = groups.entry(hashkey.clone()).or_insert_with(|| {
            order.push((hashkey.clone(), keyvals.clone()));
            vec![Accum::new(); calls.len()]
        });
        for (ai, c) in calls.iter().enumerate() {
            if c.is_star {
                g[ai].push(None, c.distinct)?;
            } else if let Some(a) = &c.arg {
                let v = expr::eval(a, row, &colfn)?;
                if v.is_null() {
                    continue;
                }
                g[ai].push(Some(v), c.distinct)?;
            }
        }
    }
    let mut keys = Vec::with_capacity(order.len());
    let mut vals = Vec::with_capacity(order.len());
    for (hk, kv) in &order {
        let accums = &groups[hk];
        keys.push(kv.clone());
        vals.push(accums.iter().zip(calls).map(|(a, c)| a.finish(&c.func, c.distinct)).collect());
    }
    // 无 GROUP BY 的全局聚合对空输入仍产出**一行**（count(*)=0，PG 语义；
    // 第七轮 R7-1 伴生②：此前空输入返回 0 行）
    if order.is_empty() && group_exprs.is_empty() && !calls.is_empty() {
        let accums = vec![Accum::new(); calls.len()];
        let row = accums.iter().zip(calls).map(|(a, c)| a.finish(&c.func, c.distinct)).collect();
        keys.push(Vec::new());
        vals.push(row);
    }
    Ok(AggResult { keys, vals })
}

/// HAVING 求值：聚合调用按 display 映射到值；group expr 按文本映射到键值
pub fn eval_having(
    e: &Expr,
    calls: &[AggCall],
    agg_vals: &[SqlValue],
    group_exprs: &[Expr],
    key_vals: &[SqlValue],
    cols: &HashMap<String, usize>,
) -> Result<SqlValue> {
    let _colfn = |name: &str| cols.get(&name.to_ascii_lowercase()).copied();
    match e {
        Expr::Function(f) => {
            let display = f.to_string();
            if let Some(idx) = calls.iter().position(|c| c.display == display) {
                return Ok(agg_vals[idx].clone());
            }
            Err(SqlError::syntax("unsupported HAVING function"))
        }
        Expr::BinaryOp { left, op, right } => {
            let l = eval_having(left, calls, agg_vals, group_exprs, key_vals, cols)?;
            let r = eval_having(right, calls, agg_vals, group_exprs, key_vals, cols)?;
            expr::binop(op.clone(), l, r)
        }
        Expr::Nested(i) => eval_having(i, calls, agg_vals, group_exprs, key_vals, cols),
        Expr::Identifier(_) => {
            // 列引用：必须是 group 键（tv 列不可用），按 group expr 文本匹配
            for (gi, g) in group_exprs.iter().enumerate() {
                if g.to_string() == e.to_string() {
                    return Ok(key_vals[gi].clone());
                }
            }
            Err(SqlError::syntax("HAVING column must appear in GROUP BY"))
        }
        Expr::Value(vws) => Ok(expr::value_from_parser(vws.value.clone())),
        other => Err(SqlError::not_supported(format!("HAVING: {}", other))),
    }
}
