//! 行与键编码（SPEC 03 §3）
//!
//! 键：保序字节编码——字节序 = 逻辑序，prolly map 直接服务 ORDER BY/范围扫描。
//! 行：tag+len 紧凑编码，WAL 帧与 prolly 叶值共用。

use crate::error::{SqlError, Result};
use crate::types::{ColType, SqlValue};

// ---- 键编码（保序） ----

const K_NULL: u8 = 0x00;
const K_NUM: u8 = 0x10;
const K_BYTES: u8 = 0x20;
const K_BOOL: u8 = 0x30;

/// 多列主键 → 保序字节。NULL 排最前；数值/时间统一 8B 定宽；
/// 字节串 0x00→0x00 0xFF 转义并以 0x00 0x01 终止（保证前缀序正确）。
pub fn encode_key(vals: &[SqlValue]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * vals.len());
    for v in vals {
        match v {
            SqlValue::Null => out.push(K_NULL),
            SqlValue::Bool(b) => {
                out.push(K_BOOL);
                out.push(if *b { 1 } else { 0 });
            }
            SqlValue::Int32(i) => {
                out.push(K_NUM);
                out.extend_from_slice(&(*i as i64 ^ i64::MIN).to_be_bytes());
            }
            SqlValue::Int64(i) => {
                out.push(K_NUM);
                out.extend_from_slice(&(i ^ i64::MIN).to_be_bytes());
            }
            SqlValue::Date32(d) => {
                out.push(K_NUM);
                out.extend_from_slice(&(*d as i64 ^ i64::MIN).to_be_bytes());
            }
            SqlValue::TimestampMs(t) => {
                out.push(K_NUM);
                out.extend_from_slice(&(t ^ i64::MIN).to_be_bytes());
            }
            SqlValue::Float64(f) => {
                out.push(K_NUM);
                out.extend_from_slice(&f64_to_orderable(*f).to_be_bytes());
            }
            SqlValue::Utf8(s) => {
                out.push(K_BYTES);
                escape_into(&mut out, s.as_bytes());
            }
            SqlValue::Bytes(b) => {
                out.push(K_BYTES);
                escape_into(&mut out, b);
            }
        }
    }
    out
}

/// 从字节解码键（按给定列型；catalog 校验 schema 时使用）
pub fn decode_key(bytes: &[u8], types: &[ColType]) -> Result<Vec<SqlValue>> {
    let mut vals = Vec::with_capacity(types.len());
    let mut r = bytes;
    for t in types {
        if r.is_empty() {
            return Err(SqlError::internal("key truncated"));
        }
        let tag = r[0];
        r = &r[1..];
        match tag {
            K_NULL => vals.push(SqlValue::Null),
            K_BOOL => {
                if r.is_empty() { return Err(SqlError::internal("row truncated: bool")); }
                vals.push(SqlValue::Bool(r[0] == 1));
                r = &r[1..];
            }
            K_NUM => {
                if r.len() < 8 {
                    return Err(SqlError::internal("key num truncated"));
                }
                let raw = i64::from_be_bytes(r[..8].try_into().unwrap());
                r = &r[8..];
                let v = raw ^ i64::MIN;
                vals.push(match t {
                    ColType::Int32 => SqlValue::Int32(v as i32),
                    ColType::Int64 => SqlValue::Int64(v),
                    ColType::Date32 => SqlValue::Date32(v as i32),
                    ColType::TimestampMs => SqlValue::TimestampMs(v),
                    ColType::Float64 => SqlValue::Float64(f64_from_orderable(raw as u64)),
                    _ => return Err(SqlError::internal("key type mismatch")),
                });
            }
            K_BYTES => {
                let (s, rest) = unescape(r)?;
                r = rest;
                vals.push(match t {
                    ColType::Utf8 => SqlValue::Utf8(
                        String::from_utf8(s).map_err(|_| SqlError::invalid_text("bad utf8 in key"))?,
                    ),
                    ColType::Bytes => SqlValue::Bytes(s),
                    _ => return Err(SqlError::internal("key type mismatch")),
                });
            }
            _ => return Err(SqlError::internal("unknown key tag")),
        }
    }
    Ok(vals)
}

fn escape_into(out: &mut Vec<u8>, b: &[u8]) {
    for &c in b {
        if c == 0 {
            out.extend_from_slice(&[0, 0xFF]);
        } else {
            out.push(c);
        }
    }
    out.extend_from_slice(&[0, 0x01]);
}

fn unescape(mut r: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    let mut out = Vec::new();
    loop {
        if r.len() < 2 {
            return Err(SqlError::internal("key string unterminated"));
        }
        if r[0] == 0 && r[1] == 0x01 {
            return Ok((out, &r[2..]));
        }
        if r[0] == 0 && r[1] == 0xFF {
            out.push(0);
            r = &r[2..];
        } else {
            out.push(r[0]);
            r = &r[1..];
        }
    }
}

/// IEEE754 全序变换（保序）：负数取反位，非负置符号位
pub fn f64_to_orderable(f: f64) -> u64 {
    let bits = f.to_bits();
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    }
}

pub fn f64_from_orderable(u: u64) -> f64 {
    let bits = if u >> 63 == 1 { u & !(1 << 63) } else { !u };
    f64::from_bits(bits)
}

// ---- 行值编码（WAL TXN 帧 / prolly 叶值共用） ----

const V_NULL: u8 = 0;
const V_BOOL: u8 = 1;
const V_I32: u8 = 2;
const V_I64: u8 = 3;
const V_F64: u8 = 4;
const V_UTF8: u8 = 5;
const V_BYTES: u8 = 6;
const V_DATE: u8 = 7;
const V_TS: u8 = 8;

pub fn encode_row(vals: &[SqlValue]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(&(vals.len() as u16).to_le_bytes());
    for v in vals {
        match v {
            SqlValue::Null => out.push(V_NULL),
            SqlValue::Bool(b) => {
                out.push(V_BOOL);
                out.push(*b as u8);
            }
            SqlValue::Int32(i) => {
                out.push(V_I32);
                out.extend_from_slice(&i.to_le_bytes());
            }
            SqlValue::Int64(i) => {
                out.push(V_I64);
                out.extend_from_slice(&i.to_le_bytes());
            }
            SqlValue::Float64(f) => {
                out.push(V_F64);
                out.extend_from_slice(&f.to_le_bytes());
            }
            SqlValue::Utf8(s) => {
                out.push(V_UTF8);
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            SqlValue::Bytes(b) => {
                out.push(V_BYTES);
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b);
            }
            SqlValue::Date32(d) => {
                out.push(V_DATE);
                out.extend_from_slice(&d.to_le_bytes());
            }
            SqlValue::TimestampMs(t) => {
                out.push(V_TS);
                out.extend_from_slice(&t.to_le_bytes());
            }
        }
    }
    out
}

pub fn decode_row(bytes: &[u8]) -> Result<Vec<SqlValue>> {
    if bytes.len() < 2 {
        return Err(SqlError::internal("row truncated"));
    }
    let n = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    let mut vals = Vec::with_capacity(n);
    let mut r = &bytes[2..];
    for _ in 0..n {
        if r.is_empty() {
            return Err(SqlError::internal("row truncated at tag"));
        }
        let tag = r[0];
        r = &r[1..];
        match tag {
            V_NULL => vals.push(SqlValue::Null),
            V_BOOL => {
                if r.is_empty() { return Err(SqlError::internal("row truncated: bool")); }
                vals.push(SqlValue::Bool(r[0] == 1));
                r = &r[1..];
            }
            V_I32 => {
                if r.len() < 4 { return Err(SqlError::internal("row truncated: i32")); }
                if r.len() < 4 { return Err(SqlError::internal("row truncated: i32")); }
                vals.push(SqlValue::Int32(i32::from_le_bytes(r[..4].try_into().unwrap())));
                r = &r[4..];
            }
            V_I64 => {
                if r.len() < 8 { return Err(SqlError::internal("row truncated: i64")); }
                if r.len() < 8 { return Err(SqlError::internal("row truncated: i64")); }
                vals.push(SqlValue::Int64(i64::from_le_bytes(r[..8].try_into().unwrap())));
                r = &r[8..];
            }
            V_F64 => {
                if r.len() < 8 { return Err(SqlError::internal("row truncated: f64")); }
                if r.len() < 8 { return Err(SqlError::internal("row truncated: f64")); }
                vals.push(SqlValue::Float64(f64::from_le_bytes(r[..8].try_into().unwrap())));
                r = &r[8..];
            }
            V_UTF8 => {
                let (s, rest) = take_bytes(r)?;
                vals.push(SqlValue::Utf8(
                    String::from_utf8(s).map_err(|_| SqlError::invalid_text("bad utf8 in row"))?,
                ));
                r = rest;
            }
            V_BYTES => {
                let (b, rest) = take_bytes(r)?;
                vals.push(SqlValue::Bytes(b));
                r = rest;
            }
            V_DATE => {
                if r.len() < 4 { return Err(SqlError::internal("row truncated: date")); }
                vals.push(SqlValue::Date32(i32::from_le_bytes(r[..4].try_into().unwrap())));
                r = &r[4..];
            }
            V_TS => {
                if r.len() < 8 { return Err(SqlError::internal("row truncated: ts")); }
                vals.push(SqlValue::TimestampMs(i64::from_le_bytes(r[..8].try_into().unwrap())));
                r = &r[8..];
            }
            _ => return Err(SqlError::internal("unknown row tag")),
        }
    }
    Ok(vals)
}

fn take_bytes(r: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    if r.len() < 4 {
        return Err(SqlError::internal("row len truncated"));
    }
    let len = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
    if r.len() < 4 + len {
        return Err(SqlError::internal("row bytes truncated"));
    }
    Ok((r[4..4 + len].to_vec(), &r[4 + len..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_order() {
        let k = |v: SqlValue| encode_key(&[v]);
        // int 序
        assert!(k(SqlValue::Int64(-2)) < k(SqlValue::Int64(-1)));
        assert!(k(SqlValue::Int64(-1)) < k(SqlValue::Int64(0)));
        assert!(k(SqlValue::Int64(i64::MIN)) < k(SqlValue::Int64(i64::MAX)));
        // float 序（含负数）
        assert!(k(SqlValue::Float64(-1.5)) < k(SqlValue::Float64(-0.5)));
        assert!(k(SqlValue::Float64(0.25)) < k(SqlValue::Float64(1e9)));
        // 字符串前缀序
        assert!(k(SqlValue::Utf8("ab".into())) < k(SqlValue::Utf8("abc".into())));
        assert!(k(SqlValue::Utf8("a\u{0}b".into())) < k(SqlValue::Utf8("a\u{0}c".into())));
        // null 最小
        assert!(k(SqlValue::Null) < k(SqlValue::Int64(i64::MIN)));
        // roundtrip
        let key = encode_key(&[SqlValue::Int64(42), SqlValue::Utf8("x\u{0}y".into())]);
        let dec = decode_key(&key, &[ColType::Int64, ColType::Utf8]).unwrap();
        assert_eq!(dec, vec![SqlValue::Int64(42), SqlValue::Utf8("x\u{0}y".into())]);
    }

    #[test]
    fn row_roundtrip() {
        let row = vec![
            SqlValue::Null,
            SqlValue::Bool(true),
            SqlValue::Int32(-7),
            SqlValue::Int64(1 << 40),
            SqlValue::Float64(3.25),
            SqlValue::Utf8("héllo".into()),
            SqlValue::Bytes(vec![1, 2, 3]),
            SqlValue::Date32(19000),
            SqlValue::TimestampMs(1_700_000_000_000),
        ];
        assert_eq!(decode_row(&encode_row(&row)).unwrap(), row);
    }
}
