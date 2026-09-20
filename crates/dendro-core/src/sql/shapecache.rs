//! 形状缓存（literal 模板化自动 prepare）——解析层优化。
//!
//! 动机（perf 实测）：唯一文本查询（拼接字面量——ClickBench/真实
//! 应用普遍形态）必 miss 计划缓存，parse 4.6µs/条 = 点查路径的
//! 最大单项。机制：把 SQL 文本中的**字面量**（引号串/数字）替换为
//! 占位符得模板；模板哈希 → 缓存的模板 AST（含 Placeholder）；
//! 命中即 克隆 + `substitute_params` 代入字面量——**与显式 prepare
//! 同一机制**，自动获得。
//!
//! 正确性（fail-open 设计）：
//! - 只替换**边界完整**的字面量：引号串（'' 转义感知）、纯数字
//!   （前邻为分隔符——`col1`/`t2` 等标识符内的数字不动）
//! - 注释/美元引号/$$ 等任何异常 → 返回 None（透传全量 parse）
//! - 代入后 AST 与直接 parse 原文**逐节点等价**（字面量即 Value）
//!
//! 与计划缓存分层：文本 hash（全同命中，零代入）→ 形状 hash
//! （模板命中，代入）→ parse（miss，入两层缓存）。

use crate::error::Result;
use crate::types::SqlValue;
use sqlparser::ast::Statement;
use std::sync::Arc;

/// 字面量提取 + 模板化。None = 形态复杂，fail-open 透传。
pub fn literal_template(sql: &str) -> Option<(String, Vec<SqlValue>)> {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut lits = Vec::new();
    let mut i = 0usize;
    let mut prev_delim = true; // 行首视作分隔边界
    while i < b.len() {
        let c = b[i] as char;
        match c {
            '\'' => {
                // 引号串（'' 转义）——收集到闭合
                let mut j = i + 1;
                let mut val = String::new();
                loop {
                    if j >= b.len() {
                        return None; // 未闭合——透传（parse 会报错）
                    }
                    if b[j] == b'\'' {
                        if j + 1 < b.len() && b[j + 1] == b'\'' {
                            val.push('\'');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    val.push(b[j] as char);
                    j += 1;
                }
                lits.push(SqlValue::Utf8(val));
                out.push_str(&format!("${}", lits.len()));
                i = j + 1;
                prev_delim = false;
            }
            '-' if i + 1 < b.len() && b[i + 1] == b'-' => return None, // 行注释——透传
            '/' if i + 1 < b.len() && b[i + 1] == b'*' => return None, // 块注释——透传
            '"' | '`' => return None,                                  // 引用标识符——保守透传
            c if c.is_ascii_digit() && prev_delim => {
                // 数字字面量（前邻为分隔边界）：[0-9]+(\.[0-9]+)?
                let mut j = i;
                let mut is_float = false;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j < b.len() && b[j] == b'.' && j + 1 < b.len() && b[j + 1].is_ascii_digit() {
                    is_float = true;
                    j += 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                // 后邻不得是标识符字符（1e5/0x1F 等形态透传）
                if j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    return None;
                }
                let txt = &sql[i..j];
                let v = if is_float {
                    SqlValue::Float64(txt.parse().ok()?)
                } else {
                    match txt.parse::<i64>() {
                        Ok(n) => SqlValue::Int64(n),
                        Err(_) => SqlValue::Float64(txt.parse().ok()?),
                    }
                };
                lits.push(v);
                out.push_str(&format!("${}", lits.len()));
                i = j;
                prev_delim = false;
            }
            c => {
                out.push(c);
                i += 1;
                prev_delim = c.is_ascii_whitespace()
                    || matches!(
                        c,
                        '(' | ')' | ',' | '=' | '<' | '>' | '+' | '-' | '*' | '/' | '%'
                    );
            }
        }
    }
    if lits.is_empty() {
        return None; // 无字面量——计划缓存已覆盖（避免双重查找）
    }
    Some((out, lits))
}

/// AST 中 Placeholder 值的个数（模板校验——方言把 $1 解析成标识符
/// 时计数不匹配，fail-open）
fn count_placeholders(stmt: &Statement) -> usize {
    use sqlparser::ast::{visit_expressions, Expr};
    use std::ops::ControlFlow;
    let mut n = 0;
    let _ = visit_expressions(stmt, |e: &Expr| {
        if let Expr::Value(vws) = e {
            if matches!(vws.value, sqlparser::ast::Value::Placeholder(_)) {
                n += 1;
            }
        }
        ControlFlow::<()>::Continue(())
    });
    n
}

/// 模板缓存（进程级；界 4096，满即清——计划缓存同策略）
type ShapeCache = std::sync::Mutex<std::collections::HashMap<u64, Arc<Statement>>>;
static CACHE: std::sync::OnceLock<ShapeCache> = std::sync::OnceLock::new();
const CAP: usize = 4096;

fn cache() -> &'static ShapeCache {
    CACHE.get_or_init(ShapeCache::default)
}

/// 形状缓存条数（census/观测）
pub fn len() -> usize {
    cache().lock().unwrap().len()
}

/// 解析或取模板（dialect 参与 key——占位符语法方言差异）。
/// 返回 (模板 AST 克隆, 提取的字面量)——克隆后归调用方代入。
pub fn parse_or_get(
    sql: &str,
    dialect: crate::sql::SqlDialect,
) -> Result<Option<(Statement, Vec<SqlValue>)>> {
    let Some((template, lits)) = literal_template(sql) else {
        return Ok(None);
    };
    let key = xxhash_rust::xxh3::xxh3_64(template.as_bytes())
        ^ (dialect as usize as u64).rotate_left(32)
        ^ 0x5a_a1e;
    let hit = cache().lock().unwrap().get(&key).cloned();
    let stmt = match hit {
        Some(ast) => (*ast).clone(),
        None => {
            // 模板含 $N 占位符——按会话方言 parse（PG $1 天然合法；
            // SQLite/MySQL 的 ? 占位符形态在 literal_template 产物
            // 中统一为 $N，方言 parser 对未绑定参数报错时 fail-open：
            // 解析失败即放弃缓存该模板，透传原文由调用方全量 parse）
            let parsed = match crate::sql::parse_batch(&template, dialect) {
                Ok(mut v) if v.len() == 1 => v.remove(0),
                _ => return Ok(None),
            };
            // 占位符校验（embed_api rowid 用例实证）：MySQL/SQLite 方言
            // 把 $1 解析成**标识符**而非 Placeholder（静默变形不报错）——
            // 计数不匹配即 fail-open，不入缓存
            if count_placeholders(&parsed) != lits.len() {
                return Ok(None);
            }
            let mut g = cache().lock().unwrap();
            if g.len() >= CAP {
                g.clear();
            }
            g.insert(key, Arc::new(parsed.clone()));
            parsed
        }
    };
    Ok(Some((stmt, lits)))
}
