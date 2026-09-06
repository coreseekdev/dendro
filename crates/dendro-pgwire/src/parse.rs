//! 参数/单元格 文本解析 — 协议层的类型文本 ↔ 内部 SqlValue。
//!
//! 属于 pgwire 私有模块（SPEC 10 §8：参数编码由 wire 层负责，core 不管）。
//! 引擎侧只会产出规范的文本格式（dendro-core::types 的 format_* 系列），
//! 这里解析的就是这些格式的超集（PG 文本格式）。

use dendro_core::error::SqlError;
use dendro_core::types::{days_from_civil, ColType, SqlValue};

pub(crate) fn parse_text(ty: ColType, s: &str) -> Result<SqlValue, SqlError> {
    let t = s.trim();
    Ok(match ty {
        ColType::Bool => SqlValue::Bool(parse_bool(t)?),
        ColType::Int32 => SqlValue::Int32(t.parse::<i32>().map_err(|_| invalid(ty, s))?),
        ColType::Int64 => SqlValue::Int64(t.parse::<i64>().map_err(|_| invalid(ty, s))?),
        ColType::Float64 => SqlValue::Float64(t.parse::<f64>().map_err(|_| invalid(ty, s))?),
        ColType::Utf8 => SqlValue::Utf8(s.to_string()),
        ColType::Bytes => SqlValue::Bytes(parse_bytea(s)?),
        ColType::Date32 => SqlValue::Date32(parse_date(t)?),
        ColType::TimestampMs => SqlValue::TimestampMs(parse_timestamp(t)?),
    })
}

fn invalid(ty: ColType, s: &str) -> SqlError {
    SqlError::invalid_text(format!(
        "invalid input syntax for type {}: \"{s}\"",
        ty.type_name()
    ))
}

fn parse_bool(s: &str) -> Result<bool, SqlError> {
    match s.to_ascii_lowercase().as_str() {
        "t" | "true" | "y" | "yes" | "on" | "1" => Ok(true),
        "f" | "false" | "n" | "no" | "off" | "0" => Ok(false),
        _ => Err(SqlError::invalid_text(format!("invalid input syntax for type boolean: \"{s}\""))),
    }
}

/// bytea 文本格式：`\x<hex>`（PG 标准）；其他按原始字节（v1 不支持 escape 格式）
fn parse_bytea(s: &str) -> Result<Vec<u8>, SqlError> {
    let hex = match s.strip_prefix("\\x").or_else(|| s.strip_prefix("\\X")) {
        Some(h) => h,
        None => return Ok(s.as_bytes().to_vec()),
    };
    fn hv(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = hex.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(SqlError::invalid_text("invalid hex encoding in bytea"));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = hv(pair[0]).ok_or_else(|| SqlError::invalid_text("invalid hex digit in bytea"))?;
        let lo = hv(pair[1]).ok_or_else(|| SqlError::invalid_text("invalid hex digit in bytea"))?;
        out.push(hi << 4 | lo);
    }
    Ok(out)
}

/// `YYYY-MM-DD` → 自 1970-01-01 的天数（22P02 on error）
pub(crate) fn parse_date(s: &str) -> Result<i32, SqlError> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(SqlError::invalid_text(format!("invalid date syntax: \"{s}\"")));
    }
    let y: i64 = parts[0]
        .parse()
        .map_err(|_| SqlError::invalid_text(format!("invalid date syntax: \"{s}\"")))?;
    let m: u32 = parts[1]
        .parse()
        .map_err(|_| SqlError::invalid_text(format!("invalid date syntax: \"{s}\"")))?;
    let d: u32 = parts[2]
        .parse()
        .map_err(|_| SqlError::invalid_text(format!("invalid date syntax: \"{s}\"")))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(SqlError::invalid_text(format!("date field out of range: \"{s}\"")));
    }
    let days = days_from_civil(y, m, d);
    i32::try_from(days)
        .map_err(|_| SqlError::invalid_text(format!("date out of range: \"{s}\"")))
}

/// `YYYY-MM-DD[ T]HH:MM[:SS[.frac]]` → 自 Unix epoch 的毫秒
pub(crate) fn parse_timestamp(s: &str) -> Result<i64, SqlError> {
    let bad = || SqlError::invalid_text(format!("invalid timestamp syntax: \"{s}\""));
    let s = s.trim();
    let (date_part, time_part) = match s.find([' ', 'T']) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let days = parse_date(date_part)?;
    let tod_ms = if time_part.is_empty() {
        0
    } else {
        parse_time_ms(time_part).map_err(|_| bad())?
    };
    Ok(days as i64 * 86_400_000 + tod_ms)
}

fn parse_time_ms(s: &str) -> Result<i64, SqlError> {
    // 丢弃时区后缀（+HH / -HH / Z）——内部一律 UTC
    let s = match s.find(['+', 'Z']) {
        Some(i) => &s[..i],
        None => s,
    };
    let (core, frac_ms) = match s.find('.') {
        Some(i) => {
            let frac = &s[i + 1..];
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return Err(SqlError::invalid_text("invalid fractional seconds"));
            }
            let mut ms = frac.as_bytes();
            ms = &ms[..ms.len().min(3)]; // 截断到毫秒
            let mut v = 0i64;
            for b in ms {
                v = v * 10 + (b - b'0') as i64;
            }
            for _ in ms.len()..3 {
                v *= 10; // 不足 3 位补零
            }
            (&s[..i], v)
        }
        None => (s, 0),
    };
    let parts: Vec<&str> = core.split(':').collect();
    let bad = || SqlError::invalid_text(format!("invalid time syntax: \"{core}\""));
    if parts.len() < 2 || parts.len() > 3 {
        return Err(bad());
    }
    let h: i64 = parts[0].parse().map_err(|_| bad())?;
    let m: i64 = parts[1].parse().map_err(|_| bad())?;
    let sec: i64 = if parts.len() == 3 {
        parts[2].parse().map_err(|_| bad())?
    } else {
        0
    };
    if h > 23 || m > 59 || sec > 60 {
        return Err(bad());
    }
    Ok(h * 3_600_000 + m * 60_000 + sec * 1000 + frac_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_float_bool() {
        assert_eq!(parse_text(ColType::Int64, " 42 ").unwrap(), SqlValue::Int64(42));
        assert_eq!(parse_text(ColType::Int32, "-7").unwrap(), SqlValue::Int32(-7));
        assert!(parse_text(ColType::Int32, "9x").is_err());
        assert_eq!(parse_text(ColType::Float64, "1.25e2").unwrap(), SqlValue::Float64(125.0));
        assert_eq!(parse_text(ColType::Bool, "TRUE").unwrap(), SqlValue::Bool(true));
        assert_eq!(parse_text(ColType::Bool, "f").unwrap(), SqlValue::Bool(false));
        assert!(parse_text(ColType::Bool, "maybe").is_err());
        assert_eq!(parse_text(ColType::Utf8, " hi ").unwrap(), SqlValue::Utf8(" hi ".into()));
    }

    #[test]
    fn parse_bytea_hex() {
        assert_eq!(parse_text(ColType::Bytes, "\\x00ff10").unwrap(), SqlValue::Bytes(vec![0, 255, 16]));
        assert!(parse_text(ColType::Bytes, "\\x0f0").is_err());
        assert_eq!(
            parse_text(ColType::Bytes, "plain").unwrap(),
            SqlValue::Bytes(b"plain".to_vec())
        );
    }

    #[test]
    fn parse_date_values() {
        // 1970-01-01 = 0
        assert_eq!(parse_date("1970-01-01").unwrap(), 0);
        // 2000-01-01 = 10957（PG epoch）
        assert_eq!(parse_date("2000-01-01").unwrap(), 10_957);
        assert_eq!(parse_date("2024-02-29").unwrap(), 19_782);
        assert!(parse_date("2024-13-01").is_err());
        assert!(parse_date("20240101").is_err());
    }

    #[test]
    fn parse_timestamp_values() {
        assert_eq!(parse_timestamp("1970-01-01 00:00:00").unwrap(), 0);
        assert_eq!(parse_timestamp("1970-01-01").unwrap(), 0);
        assert_eq!(parse_timestamp("1970-01-01T00:00:01.5").unwrap(), 1_500);
        assert_eq!(parse_timestamp("2000-01-01 00:00:00").unwrap(), 946_684_800_000);
        assert_eq!(
            parse_timestamp("2024-01-02 03:04:05.123+00").unwrap(),
            (parse_date("2024-01-02").unwrap() as i64) * 86_400_000
                + 3 * 3_600_000
                + 4 * 60_000
                + 5_000
                + 123
        );
        assert!(parse_timestamp("2024-01-02 25:00:00").is_err());
        assert!(parse_timestamp("2024-01-02 03:04:05.x").is_err());
    }
}
