//! 表达式求值（行式）：sqlparser Expr + 一行 SqlValue → SqlValue。

use crate::error::{Result, SqlError};
use crate::types::SqlValue;
use sqlparser::ast::{
    BinaryOperator as BO, DataType as PD, Expr, Function, FunctionArg, FunctionArgExpr, Value as PV,
};

/// 求值。cols: 列名 → 下标（小写）。
pub fn eval(e: &Expr, row: &[SqlValue], cols: &dyn Fn(&str) -> Option<usize>) -> Result<SqlValue> {
    match e {
        Expr::Value(v) => {
            if let PV::Placeholder(id) = &v.value {
                return Err(SqlError::new("08P01", format!("unbound parameter {id}")));
            }
            Ok(value_from_parser(v.value.clone()))
        }
        Expr::Identifier(id) => {
            let name = id.value.as_str();
            cols(name)
                .map(|i| row[i].clone())
                .ok_or_else(|| SqlError::undefined_column(format!("column \"{name}\" does not exist")))
        }
        Expr::CompoundIdentifier(parts) => {
            let name = parts.last().map(|p| p.value.clone()).unwrap_or_default();
            cols(&name)
                .map(|i| row[i].clone())
                .ok_or_else(|| SqlError::undefined_column(format!("column \"{name}\" does not exist")))
        }
        Expr::Wildcard(_) | Expr::QualifiedWildcard(..) => {
            Err(SqlError::syntax("wildcard not allowed in expression"))
        }
        Expr::BinaryOp { left, op, right } => {
            let l = eval(left, row, cols)?;
            let r = eval(right, row, cols)?;
            binop(op.clone(), l, r)
        }
        Expr::UnaryOp { op, expr } => {
            let v = eval(expr, row, cols)?;
            Ok(match op {
                sqlparser::ast::UnaryOperator::Not => SqlValue::Bool(!as_bool(&v)?),
                sqlparser::ast::UnaryOperator::Minus => match v {
                    SqlValue::Int32(i) => SqlValue::Int32(-i),
                    SqlValue::Int64(i) => SqlValue::Int64(-i),
                    SqlValue::Float64(f) => SqlValue::Float64(-f),
                    SqlValue::Null => SqlValue::Null,
                    _ => return Err(SqlError::datatype_mismatch("cannot negate")),
                },
                sqlparser::ast::UnaryOperator::Plus => v,
                _ => return Err(SqlError::not_supported("unary op")),
            })
        }
        Expr::Nested(inner) => eval(inner, row, cols),
        Expr::Cast { kind, expr, data_type, .. } => {
            // v1 统一按普通 cast 处理（TRY_CAST 失败给 NULL）
            let v = eval(expr, row, cols);
            let out = match kind {
                sqlparser::ast::CastKind::TryCast | sqlparser::ast::CastKind::SafeCast => {
                    v.ok().and_then(|v| cast_value(v, data_type).ok()).unwrap_or(SqlValue::Null)
                }
                _ => cast_value(v?, data_type)?,
            };
            Ok(out)
        }
        Expr::IsNull(inner) => Ok(SqlValue::Bool(eval(inner, row, cols)?.is_null())),
        Expr::IsNotNull(inner) => Ok(SqlValue::Bool(!eval(inner, row, cols)?.is_null())),
        Expr::IsTrue(inner) => Ok(SqlValue::Bool(matches!(as_bool(&eval(inner, row, cols)?), Ok(true)))),
        Expr::IsFalse(inner) => Ok(SqlValue::Bool(matches!(as_bool(&eval(inner, row, cols)?), Ok(false)))),
        Expr::InList { expr, list, negated } => {
            let v = eval(expr, row, cols)?;
            let mut found = false;
            let mut has_null = false;
            for item in list {
                let iv = eval(item, row, cols)?;
                if iv.is_null() || v.is_null() {
                    has_null = true;
                    continue;
                }
                if cmp_values(&v, &iv)? == std::cmp::Ordering::Equal {
                    found = true;
                    break;
                }
            }
            let res = if found {
                true
            } else if has_null {
                return Ok(SqlValue::Null);
            } else {
                false
            };
            Ok(SqlValue::Bool(res != *negated))
        }
        Expr::Between { expr, negated, low, high } => {
            let v = eval(expr, row, cols)?;
            let lo = eval(low, row, cols)?;
            let hi = eval(high, row, cols)?;
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(SqlValue::Null);
            }
            let inside = cmp_values(&v, &lo)? != std::cmp::Ordering::Less
                && cmp_values(&v, &hi)? != std::cmp::Ordering::Greater;
            Ok(SqlValue::Bool(inside != *negated))
        }
        Expr::Case { operand, conditions, else_result, .. } => {
            let opv = match operand {
                Some(o) => Some(eval(o, row, cols)?),
                None => None,
            };
            for cw in conditions {
                let cv = eval(&cw.condition, row, cols)?;
                let hit = match &opv {
                    Some(o) => {
                        !cv.is_null() && !o.is_null() && cmp_values(o, &cv)? == std::cmp::Ordering::Equal
                    }
                    None => as_bool(&cv)?,
                };
                if hit {
                    return eval(&cw.result, row, cols);
                }
            }
            match else_result {
                Some(e) => eval(e, row, cols),
                None => Ok(SqlValue::Null),
            }
        }
        Expr::Function(f) => eval_function(f, row, cols),
        Expr::Substring { expr, substring_from, substring_for, special: _, shorthand: _ } => {
            let s0 = match eval(expr, row, cols)? {
                SqlValue::Utf8(s) => s,
                SqlValue::Null => return Ok(SqlValue::Null),
                v => crate::sql::expr::to_text(v),
            };
            let from_e: &Expr = substring_from.as_deref().ok_or_else(|| SqlError::syntax("substring missing FROM"))?;
            let start = match eval(from_e, row, cols)? {
                SqlValue::Null => return Ok(SqlValue::Null),
                v => as_i64(&v)? - 1,
            };
            let chars: Vec<char> = s0.chars().collect();
            let start = start.clamp(0, chars.len() as i64) as usize;
            match substring_for {
                Some(len_e) => {
                    let len = match eval(len_e, row, cols)? {
                        SqlValue::Null => return Ok(SqlValue::Null),
                        v => as_i64(&v)?.max(0) as usize,
                    };
                    Ok(SqlValue::Utf8(chars[start..(start + len).min(chars.len())].iter().collect()))
                }
                None => Ok(SqlValue::Utf8(chars[start..].iter().collect())),
            }
        }
        Expr::TypedString(ts) => {
            let sv = value_from_parser(ts.value.value.clone());
            cast_value(sv, &ts.data_type)
        }
        other => Err(SqlError::not_supported(format!("expression: {}", short(other)))),
    }
}

fn short(e: &Expr) -> String {
    let s = e.to_string();
    s.chars().take(60).collect()
}

fn eval_function(f: &Function, row: &[SqlValue], cols: &dyn Fn(&str) -> Option<usize>) -> Result<SqlValue> {
    let name = f.name.to_string().to_ascii_lowercase();
    let args: &[FunctionArg] = match &f.args {
        sqlparser::ast::FunctionArguments::List(l) => &l.args,
        _ => &[] as &[FunctionArg],
    };
    let ev = |a: &FunctionArg| -> Result<SqlValue> {
        match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => eval(e, row, cols),
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => Err(SqlError::syntax("wildcard arg")),
            _ => Err(SqlError::syntax("named args unsupported")),
        }
    };
    let a0 = || -> Result<SqlValue> {
        match args.first() {
            Some(a) => ev(a),
            None => Err(SqlError::syntax("missing argument")),
        }
    };
    let a1 = || -> Result<SqlValue> {
        match args.get(1) {
            Some(a) => ev(a),
            None => Err(SqlError::syntax("missing second argument")),
        }
    };
    let f64v = |v: SqlValue| -> Result<f64> { as_f64(&v) };
    match name.as_str() {
        "abs" => {
            let v = a0()?;
            Ok(match v {
                SqlValue::Int32(i) => SqlValue::Int32(i.abs()),
                SqlValue::Int64(i) => SqlValue::Int64(i.abs()),
                SqlValue::Float64(f) => SqlValue::Float64(f.abs()),
                SqlValue::Null => SqlValue::Null,
                _ => return Err(SqlError::datatype_mismatch("abs")),
            })
        }
        "round" => {
            let v = a0()?;
            let d = if args.len() > 1 { as_i64(&a1()?)? as i32 } else { 0 };
            let f = f64v(v)?;
            let m = 10f64.powi(d);
            Ok(SqlValue::Float64((f * m).round() / m))
        }
        "floor" => Ok(SqlValue::Float64(f64v(a0()?)?.floor())),
        "ceil" | "ceiling" => Ok(SqlValue::Float64(f64v(a0()?)?.ceil())),
        "sqrt" | "pow" | "power" => {
            if name == "sqrt" {
                Ok(SqlValue::Float64(f64v(a0()?)?.sqrt()))
            } else {
                let (a, b) = (f64v(a0()?)?, f64v(a1()?)?);
                Ok(SqlValue::Float64(a.powf(b)))
            }
        }
        "length" | "char_length" | "character_length" => match a0()? {
            SqlValue::Utf8(s) => Ok(SqlValue::Int32(s.chars().count() as i32)),
            SqlValue::Null => Ok(SqlValue::Null),
            _ => Err(SqlError::datatype_mismatch("length")),
        },
        "upper" | "lower" => match a0()? {
            SqlValue::Utf8(s) => Ok(SqlValue::Utf8(if name == "upper" { s.to_uppercase() } else { s.to_lowercase() })),
            SqlValue::Null => Ok(SqlValue::Null),
            _ => Err(SqlError::datatype_mismatch(name)),
        },
        "substr" | "substring" => {
            let s = match a0()? {
                SqlValue::Utf8(s) => s,
                SqlValue::Null => return Ok(SqlValue::Null),
                _ => return Err(SqlError::datatype_mismatch("substr")),
            };
            let start = as_i64(&a1()?)? - 1;
            let chars: Vec<char> = s.chars().collect();
            let start = start.clamp(0, chars.len() as i64) as usize;
            if args.len() > 2 {
                let len = as_i64(&ev(args.get(2).unwrap())?)?.max(0) as usize;
                Ok(SqlValue::Utf8(chars[start..(start + len).min(chars.len())].iter().collect()))
            } else {
                Ok(SqlValue::Utf8(chars[start..].iter().collect()))
            }
        }
        "concat" => {
            let mut s = String::new();
            for a in args {
                match ev(a)? {
                    SqlValue::Utf8(v) => s.push_str(&v),
                    SqlValue::Null => {}
                    v => s.push_str(&to_text(v)),
                }
            }
            Ok(SqlValue::Utf8(s))
        }
        "trim" | "ltrim" | "rtrim" => match a0()? {
            SqlValue::Utf8(s) => Ok(SqlValue::Utf8(match name.as_str() {
                "trim" => s.trim().to_string(),
                "ltrim" => s.trim_start().to_string(),
                _ => s.trim_end().to_string(),
            })),
            SqlValue::Null => Ok(SqlValue::Null),
            _ => Err(SqlError::datatype_mismatch(name)),
        },
        "replace" => {
            let (s, from, to) = (a0()?, a1()?, ev(args.get(2).ok_or_else(|| SqlError::syntax("replace arity"))?)?);
            match (s, from, to) {
                (SqlValue::Utf8(s), SqlValue::Utf8(f), SqlValue::Utf8(t)) => {
                    Ok(SqlValue::Utf8(s.replace(&f, &t)))
                }
                _ => Err(SqlError::datatype_mismatch("replace")),
            }
        }
        "coalesce" => {
            for a in args {
                let v = ev(a)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Ok(SqlValue::Null)
        }
        "nullif" => {
            let (x, y) = (a0()?, a1()?);
            if !x.is_null() && !y.is_null() && cmp_values(&x, &y)? == std::cmp::Ordering::Equal {
                Ok(SqlValue::Null)
            } else {
                Ok(x)
            }
        }
        "greatest" | "least" => {
            let mut best: Option<SqlValue> = None;
            for a in args {
                let v = ev(a)?;
                if v.is_null() {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let ord = cmp_values(&v, &b)?;
                        let take_v = if name == "greatest" {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        };
                        if take_v { v } else { b }
                    }
                });
            }
            Ok(best.unwrap_or(SqlValue::Null))
        }
        "mod" => {
            let (x, y) = (as_i64(&a0()?)?, as_i64(&a1()?)?);
            if y == 0 {
                return Err(SqlError::division_by_zero());
            }
            Ok(SqlValue::Int64(x.rem_euclid(y.abs()) * y.signum()))
        }
        _ => Err(SqlError::not_supported(format!("function {name}"))),
    }
}

pub fn to_text(v: SqlValue) -> String {
    match v {
        SqlValue::Null => String::new(),
        SqlValue::Bool(b) => b.to_string(),
        SqlValue::Int32(i) => i.to_string(),
        SqlValue::Int64(i) => i.to_string(),
        SqlValue::Float64(f) => crate::types::format_f64(f),
        SqlValue::Utf8(s) => s,
        SqlValue::Bytes(b) => b.iter().map(|x| *x as char).collect(),
        SqlValue::Date32(d) => crate::types::format_date(d),
        SqlValue::TimestampMs(t) => crate::types::format_ts_ms(t),
    }
}

pub fn binop(op: BO, l: SqlValue, r: SqlValue) -> Result<SqlValue> {
    use std::cmp::Ordering::*;
    if l.is_null() || r.is_null() {
        return Ok(SqlValue::Null);
    }
    match op {
        BO::Eq | BO::NotEq | BO::Lt | BO::LtEq | BO::Gt | BO::GtEq => {
            let ord = cmp_values(&l, &r)?;
            let b = match op {
                BO::Eq => ord == Equal,
                BO::NotEq => ord != Equal,
                BO::Lt => ord == Less,
                BO::LtEq => ord != Greater,
                BO::Gt => ord == Greater,
                BO::GtEq => ord != Less,
                _ => unreachable!(),
            };
            Ok(SqlValue::Bool(b))
        }
        BO::And => Ok(SqlValue::Bool(as_bool(&l)? && as_bool(&r)?)),
        BO::Or => Ok(SqlValue::Bool(as_bool(&l)? || as_bool(&r)?)),
        BO::Plus | BO::Minus | BO::Multiply | BO::Divide | BO::Modulo => arith(op, l, r),
        BO::StringConcat => Ok(SqlValue::Utf8(format!("{}{}", to_text(l), to_text(r)))),
        other => Err(SqlError::not_supported(format!("operator {other}"))),
    }
}

fn arith(op: BO, l: SqlValue, r: SqlValue) -> Result<SqlValue> {
    // 任一浮点 → 浮点运算；否则整数
    if matches!(l, SqlValue::Float64(_)) || matches!(r, SqlValue::Float64(_)) {
        let (a, b) = (as_f64(&l)?, as_f64(&r)?);
        return Ok(SqlValue::Float64(match op {
            BO::Plus => a + b,
            BO::Minus => a - b,
            BO::Multiply => a * b,
            BO::Divide => {
                if b == 0.0 {
                    return Err(SqlError::division_by_zero());
                }
                a / b
            }
            BO::Modulo => a % b,
            _ => unreachable!(),
        }));
    }
    let (a, b): (i64, i64) = (as_i64(&l)?, as_i64(&r)?);
    let out = match op {
        BO::Plus => a.checked_add(b),
        BO::Minus => a.checked_sub(b),
        BO::Multiply => a.checked_mul(b),
        BO::Divide => {
            if b == 0 {
                return Err(SqlError::division_by_zero());
            }
            a.checked_div(b)
        }
        BO::Modulo => {
            if b == 0 {
                return Err(SqlError::division_by_zero());
            }
            a.checked_rem(b)
        }
        _ => unreachable!(),
    };
    let v = out.ok_or_else(|| SqlError::internal("integer overflow"))?;
    Ok(SqlValue::Int64(v))
}

/// 三态比较（数值跨型比较；字符串字节序；null 已由调用方过滤）
pub fn cmp_values(l: &SqlValue, r: &SqlValue) -> Result<std::cmp::Ordering> {
    use std::cmp::Ordering;
    use std::cmp::Ordering::*;
    let ln = matches!(l, SqlValue::Null);
    let rn = matches!(r, SqlValue::Null);
    if ln || rn {
        // SQL 三值逻辑由调用方处理；这里给确定序
        return Ok(if ln && rn { Equal } else if ln { Less } else { Greater });
    }
    // 数值族
    let numeric = |v: &SqlValue| {
        matches!(v, SqlValue::Int32(_) | SqlValue::Int64(_) | SqlValue::Float64(_) | SqlValue::Date32(_) | SqlValue::TimestampMs(_))
    };
    if numeric(l) && numeric(r) {
        // 浮点优先，否则 i64
        if matches!(l, SqlValue::Float64(_)) || matches!(r, SqlValue::Float64(_)) {
            let (a, b) = (as_f64(l)?, as_f64(r)?);
            return Ok(a.partial_cmp(&b).unwrap_or(Ordering::Equal));
        }
        let (a, b) = (as_i64(l)?, as_i64(r)?);
        return Ok(a.cmp(&b));
    }
    match (l, r) {
        (SqlValue::Bool(a), SqlValue::Bool(b)) => Ok(a.cmp(b)),
        (SqlValue::Utf8(a), SqlValue::Utf8(b)) => Ok(a.as_bytes().cmp(b.as_bytes())),
        (SqlValue::Bytes(a), SqlValue::Bytes(b)) => Ok(a.cmp(b)),
        _ => Err(SqlError::datatype_mismatch(format!("cannot compare {} with {}", l.type_name(), r.type_name()))),
    }
}

pub fn as_bool(v: &SqlValue) -> Result<bool> {
    match v {
        SqlValue::Bool(b) => Ok(*b),
        SqlValue::Int32(i) => Ok(*i != 0),
        SqlValue::Int64(i) => Ok(*i != 0),
        SqlValue::Null => Err(SqlError::internal("NULL bool")), // 调用方通常先查 null
        _ => Err(SqlError::datatype_mismatch("expected boolean")),
    }
}

pub fn as_i64(v: &SqlValue) -> Result<i64> {
    match v {
        SqlValue::Int32(i) => Ok(*i as i64),
        SqlValue::Int64(i) => Ok(*i),
        SqlValue::Bool(b) => Ok(*b as i64),
        SqlValue::Date32(d) => Ok(*d as i64),
        SqlValue::TimestampMs(t) => Ok(*t),
        _ => Err(SqlError::datatype_mismatch(format!("expected number, got {}", v.type_name()))),
    }
}

pub fn as_f64(v: &SqlValue) -> Result<f64> {
    match v {
        SqlValue::Float64(f) => Ok(*f),
        _ => Ok(as_i64(v)? as f64),
    }
}

/// 类型转换（CAST）
pub fn cast_value(v: SqlValue, ty: &PD) -> Result<SqlValue> {
    if v.is_null() {
        return Ok(SqlValue::Null);
    }
    Ok(match ty {
        PD::Boolean | PD::Bool => SqlValue::Bool(as_bool(&v)?),
        PD::Int(_) | PD::Integer(_) | PD::SmallInt(_) | PD::Int2(_) => SqlValue::Int32(as_i64(&v)? as i32),
        PD::BigInt(_) | PD::Int8(_) => SqlValue::Int64(as_i64(&v)?),
        PD::Real | PD::Float(_) | PD::Float4 | PD::Float8 | PD::Double(_) | PD::DoublePrecision | PD::Decimal(_) | PD::Numeric { .. } => {
            SqlValue::Float64(as_f64(&v)?)
        }
        PD::Text | PD::Varchar(_) | PD::Char(_) | PD::String(_) | PD::CharacterVarying(_) => {
            SqlValue::Utf8(to_text(v))
        }
        PD::Bytea | PD::Blob(_) | PD::Binary(_) | PD::Varbinary(_) => match v {
            SqlValue::Bytes(b) => SqlValue::Bytes(b),
            SqlValue::Utf8(s) => SqlValue::Bytes(s.into_bytes()),
            _other => return Err(SqlError::datatype_mismatch("cast to bytea")),
        },
        PD::Date => match v {
            SqlValue::Date32(d) => SqlValue::Date32(d),
            SqlValue::Utf8(s) => SqlValue::Date32(parse_date(&s).ok_or_else(|| SqlError::invalid_text(format!("bad date: {s}")))?),
            _ => return Err(SqlError::datatype_mismatch("cast to date")),
        },
        PD::Timestamp(..) | PD::Datetime(_) => match v {
            SqlValue::TimestampMs(t) => SqlValue::TimestampMs(t),
            SqlValue::Utf8(s) => SqlValue::TimestampMs(parse_ts(&s).ok_or_else(|| SqlError::invalid_text(format!("bad timestamp: {s}")))?),
            _ => return Err(SqlError::datatype_mismatch("cast to timestamp")),
        },
        other => return Err(SqlError::not_supported(format!("cast to {other}"))),
    })
}

/// "YYYY-MM-DD" → days since epoch
pub fn parse_date(s: &str) -> Option<i32> {
    let parts: Vec<&str> = s.trim().splitn(3, '-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    Some(crate::types::days_from_civil(y, m, d) as i32)
}

/// "YYYY-MM-DD[ HH:MM:SS[.mmm]]" → ms since epoch（UTC）
pub fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    let (d, t) = match s.split_once([' ', 'T']) {
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    let days = parse_date(d)?;
    let ms = if t.is_empty() {
        0
    } else {
        let t = t.trim_end_matches('Z');
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() < 2 {
            return None;
        }
        let hh: i64 = parts[0].parse().ok()?;
        let mm: i64 = parts[1].parse().ok()?;
        let ss: f64 = parts.get(2).map(|x| x.parse().unwrap_or(0.0)).unwrap_or(0.0);
        hh * 3_600_000 + mm * 60_000 + (ss * 1000.0) as i64
    };
    Some(days as i64 * 86_400_000 + ms)
}

/// sqlparser Value → SqlValue 字面量
pub fn value_from_parser(v: PV) -> SqlValue {
    match v {
        PV::Number(n, _) | PV::SingleQuotedString(n) | PV::DoubleQuotedString(n) => number_or_string(n),
        PV::Boolean(b) => SqlValue::Bool(b),
        PV::Null => SqlValue::Null,
        PV::HexStringLiteral(h) => SqlValue::Bytes(
            (0..h.len() / 2)
                .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16))
                .collect::<std::result::Result<Vec<u8>, _>>()
                .unwrap_or_default(),
        ),
        PV::EscapedStringLiteral(s) => SqlValue::Utf8(s),
        other => SqlValue::Utf8(other.to_string()),
    }
}

fn number_or_string(n: String) -> SqlValue {
    if let Ok(i) = n.parse::<i64>() {
        if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
            return SqlValue::Int32(i as i32);
        }
        return SqlValue::Int64(i);
    }
    if let Ok(f) = n.parse::<f64>() {
        return SqlValue::Float64(f);
    }
    SqlValue::Utf8(n)
}

/// SqlValue → sqlparser Expr::Value（prepared 参数替换用）
pub fn value_to_value_expr(v: &SqlValue) -> PV {
    match v {
        SqlValue::Null => PV::Null,
        SqlValue::Bool(b) => PV::Boolean(*b),
        SqlValue::Int32(i) => PV::Number(i.to_string(), false),
        SqlValue::Int64(i) => PV::Number(i.to_string(), false),
        SqlValue::Float64(f) => PV::Number(f.to_string(), false),
        SqlValue::Utf8(s) => PV::SingleQuotedString(s.clone()),
        SqlValue::Bytes(b) => PV::HexStringLiteral(b.iter().map(|x| format!("{x:02X}")).collect()),
        SqlValue::Date32(d) => PV::SingleQuotedString(crate::types::format_date(*d)),
        SqlValue::TimestampMs(t) => PV::SingleQuotedString(crate::types::format_ts_ms(*t)),
    }
}
