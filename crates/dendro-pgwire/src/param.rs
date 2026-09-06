//! Bind 参数解码与结果单元格编码（SPEC 06 §2.3 类型映射）。
//!
//! - 文本（format 0）→ [`parse`](crate::parse) 解析成 SqlValue
//! - 二进制（format 1）→ 按 PG 二进制格式解码：
//!   bool 1B / int4 BE4B / int8 BE8B / float8 BE8B / text 原文 / bytea 原文 /
//!   date BE i32（自 2000-01-01 天数）/ timestamp BE i64（自 2000-01-01 微秒）
//! - 结果单元格：text 直接取引擎文本；binary 由文本值反向编码
//!   （引擎只产出文本行 + 列类型，SPEC 10 §3/§8）

use dendro_core::error::SqlError;
use dendro_core::types::{ColType, SqlValue};

use crate::parse;

/// 1970-01-01 → 2000-01-01 的天数（PG epoch）
pub const PG_EPOCH_DAYS: i64 = 10_957;
/// PG epoch（2000-01-01）的 Unix 毫秒
pub const PG_EPOCH_MS: i64 = PG_EPOCH_DAYS * 86_400_000; // 946_684_800_000

/// 解码一个 Bind 参数。`raw = None` 即 NULL。
pub fn decode_param(format: i16, ty: ColType, raw: Option<&[u8]>) -> Result<SqlValue, SqlError> {
    let Some(raw) = raw else {
        return Ok(SqlValue::Null);
    };
    match format {
        0 => {
            let s = std::str::from_utf8(raw)
                .map_err(|_| SqlError::invalid_text("parameter text is not valid UTF-8"))?;
            parse::parse_text(ty, s)
        }
        1 => decode_binary(ty, raw),
        other => Err(SqlError::new(
            "08P01",
            format!("unsupported parameter format code {other}"),
        )),
    }
}

fn bad_len(ty: ColType, got: usize, want: usize) -> SqlError {
    SqlError::new(
        "08P01",
        format!(
            "malformed binary {} value: {} bytes, expected {}",
            ty.type_name(),
            got,
            want
        ),
    )
}

/// PG 二进制格式 → SqlValue（SPEC 06 §2.3）
pub fn decode_binary(ty: ColType, raw: &[u8]) -> Result<SqlValue, SqlError> {
    Ok(match ty {
        ColType::Bool => {
            if raw.len() != 1 {
                return Err(bad_len(ty, raw.len(), 1));
            }
            SqlValue::Bool(raw[0] != 0)
        }
        ColType::Int32 => {
            if raw.len() != 4 {
                return Err(bad_len(ty, raw.len(), 4));
            }
            SqlValue::Int32(i32::from_be_bytes(raw.try_into().unwrap()))
        }
        ColType::Int64 => {
            if raw.len() != 8 {
                return Err(bad_len(ty, raw.len(), 8));
            }
            SqlValue::Int64(i64::from_be_bytes(raw.try_into().unwrap()))
        }
        ColType::Float64 => {
            if raw.len() != 8 {
                return Err(bad_len(ty, raw.len(), 8));
            }
            SqlValue::Float64(f64::from_be_bytes(raw.try_into().unwrap()))
        }
        ColType::Utf8 => SqlValue::Utf8(
            String::from_utf8(raw.to_vec())
                .map_err(|_| SqlError::invalid_text("invalid UTF-8 in text parameter"))?,
        ),
        ColType::Bytes => SqlValue::Bytes(raw.to_vec()),
        ColType::Date32 => {
            if raw.len() != 4 {
                return Err(bad_len(ty, raw.len(), 4));
            }
            let d = i32::from_be_bytes(raw.try_into().unwrap());
            SqlValue::Date32(d + PG_EPOCH_DAYS as i32)
        }
        ColType::TimestampMs => {
            if raw.len() != 8 {
                return Err(bad_len(ty, raw.len(), 8));
            }
            let us = i64::from_be_bytes(raw.try_into().unwrap());
            SqlValue::TimestampMs(us.div_euclid(1000) + PG_EPOCH_MS)
        }
    })
}

/// 引擎文本单元格 → 协议字节。format 0 = 原文；format 1 = 文本解析后按类型
/// 编码为 PG 二进制。解析失败时兜底回文本（引擎文本与本层解析应自洽）。
pub fn encode_cell(ty: ColType, text: Option<&str>, format: i16) -> Option<Vec<u8>> {
    match (format, text) {
        (_, None) => None,
        (0, Some(s)) => Some(s.as_bytes().to_vec()),
        (1, Some(s)) => match parse::parse_text(ty, s) {
            Ok(v) => Some(encode_binary_value(ty, &v)),
            Err(_) => Some(s.as_bytes().to_vec()),
        },
        _ => Some(text?.as_bytes().to_vec()),
    }
}

/// SqlValue → PG 二进制字节（与 [`decode_binary`] 互逆）
pub fn encode_binary_value(ty: ColType, v: &SqlValue) -> Vec<u8> {
    match (ty, v) {
        (ColType::Bool, SqlValue::Bool(b)) => vec![*b as u8],
        (ColType::Int32, SqlValue::Int32(i)) => i.to_be_bytes().to_vec(),
        (ColType::Int64, SqlValue::Int64(i)) => i.to_be_bytes().to_vec(),
        (ColType::Float64, SqlValue::Float64(f)) => f.to_be_bytes().to_vec(),
        (ColType::Utf8, SqlValue::Utf8(s)) => s.as_bytes().to_vec(),
        (ColType::Bytes, SqlValue::Bytes(b)) => b.clone(),
        (ColType::Date32, SqlValue::Date32(d)) => (d - PG_EPOCH_DAYS as i32).to_be_bytes().to_vec(),
        (ColType::TimestampMs, SqlValue::TimestampMs(ms)) => {
            ((ms - PG_EPOCH_MS) * 1000).to_be_bytes().to_vec()
        }
        // 类型不符（理论不可达：值由同类型文本解析而来）— 文本兜底
        _ => value_to_text(v).into_bytes(),
    }
}

fn value_to_text(v: &SqlValue) -> String {
    match v {
        SqlValue::Null => String::new(),
        SqlValue::Bool(b) => b.to_string(),
        SqlValue::Int32(i) => i.to_string(),
        SqlValue::Int64(i) => i.to_string(),
        SqlValue::Float64(f) => dendro_core::types::format_f64(*f),
        SqlValue::Utf8(s) => s.clone(),
        SqlValue::Bytes(b) => format!("\\x{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        SqlValue::Date32(d) => dendro_core::types::format_date(*d),
        SqlValue::TimestampMs(ms) => dendro_core::types::format_ts_ms(*ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_int_roundtrip() {
        let v = decode_binary(ColType::Int64, &7i64.to_be_bytes()).unwrap();
        assert_eq!(v, SqlValue::Int64(7));
        assert_eq!(encode_binary_value(ColType::Int64, &v), 7i64.to_be_bytes());
        assert!(decode_binary(ColType::Int64, &[1, 2, 3]).is_err()); // 08P01
        let v = decode_binary(ColType::Int32, &(-5i32).to_be_bytes()).unwrap();
        assert_eq!(v, SqlValue::Int32(-5));
    }

    #[test]
    fn binary_bool_float() {
        assert_eq!(decode_binary(ColType::Bool, &[1]).unwrap(), SqlValue::Bool(true));
        assert_eq!(decode_binary(ColType::Bool, &[0]).unwrap(), SqlValue::Bool(false));
        let bits = 1.5f64.to_be_bytes();
        assert_eq!(decode_binary(ColType::Float64, &bits).unwrap(), SqlValue::Float64(1.5));
    }

    #[test]
    fn binary_date_and_timestamp_pg_epoch() {
        // PG epoch 本身：2000-01-01 = days 0 / µs 0
        assert_eq!(decode_binary(ColType::Date32, &0i32.to_be_bytes()).unwrap(), SqlValue::Date32(10_957));
        assert_eq!(
            decode_binary(ColType::TimestampMs, &0i64.to_be_bytes()).unwrap(),
            SqlValue::TimestampMs(946_684_800_000)
        );
        // 负值（epoch 之前）：1999-12-31 = -86_400_000_000 µs
        let us = -86_400_000_000i64;
        assert_eq!(
            decode_binary(ColType::TimestampMs, &us.to_be_bytes()).unwrap(),
            SqlValue::TimestampMs(946_684_800_000 - 86_400_000)
        );
        // 逆向
        assert_eq!(
            encode_binary_value(ColType::Date32, &SqlValue::Date32(10_957)),
            0i32.to_be_bytes().to_vec()
        );
        assert_eq!(
            encode_binary_value(ColType::TimestampMs, &SqlValue::TimestampMs(946_684_800_123)),
            123_000i64.to_be_bytes().to_vec()
        );
    }

    #[test]
    fn text_param_decode() {
        assert_eq!(decode_param(0, ColType::Int64, Some(b"9")).unwrap(), SqlValue::Int64(9));
        assert_eq!(decode_param(0, ColType::Utf8, Some(b"hi")).unwrap(), SqlValue::Utf8("hi".into()));
        assert_eq!(decode_param(0, ColType::Bool, Some(b"t")).unwrap(), SqlValue::Bool(true));
        assert!(decode_param(0, ColType::Date32, Some(b"bad")).is_err());
        assert_eq!(decode_param(0, ColType::Int32, None).unwrap(), SqlValue::Null);
        assert!(decode_param(2, ColType::Int32, Some(b"1")).is_err()); // 未知 format
    }

    #[test]
    fn encode_cell_text_and_binary() {
        assert_eq!(encode_cell(ColType::Int64, Some("1"), 0), Some(b"1".to_vec()));
        assert_eq!(encode_cell(ColType::Int64, Some("1"), 1), Some(1i64.to_be_bytes().to_vec()));
        assert_eq!(encode_cell(ColType::Int64, None, 1), None);
        assert_eq!(encode_cell(ColType::Utf8, Some("ab"), 1), Some(b"ab".to_vec()));
        // 兜底：坏文本回退原文
        assert_eq!(encode_cell(ColType::Int64, Some("x!"), 1), Some(b"x!".to_vec()));
    }
}
