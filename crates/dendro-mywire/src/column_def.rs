//! ColumnDefinition41 构造与内部 ColType → MySQL 类型码映射 — SPEC 06 §3「COM_QUERY」。
//!
//! 结果集列定义（protocol 文档 ColumnDefinition41）：
//! ```text
//! lenenc_str catalog("def") / schema / table / org_table / name / org_name
//! lenenc_int 0x0C   固定字段块长度
//! 2B 字符集  4B 列长  1B 类型码  2B flags  1B decimals  2B filler
//! ```

use crate::codec::write_lenenc_str;
use dendro_core::types::{ColType, ColumnMeta};

/// MySQL 列类型码
pub const MYSQL_TYPE_TINY: u8 = 1;
pub const MYSQL_TYPE_LONG: u8 = 3;
pub const MYSQL_TYPE_DOUBLE: u8 = 5;
pub const MYSQL_TYPE_LONGLONG: u8 = 8;
pub const MYSQL_TYPE_DATE: u8 = 10;
pub const MYSQL_TYPE_DATETIME: u8 = 12;
pub const MYSQL_TYPE_BLOB: u8 = 252;
pub const MYSQL_TYPE_VAR_STRING: u8 = 253;

/// utf8_general_ci（文本列）
pub const UTF8_GENERAL_CI: u16 = 33;
/// binary（数值/时间/二进制列）
pub const BINARY_CHARSET: u16 = 63;

/// 结果集 schema 名（v1 单库，与 pgwire 侧一致的逻辑库名）
pub const SCHEMA: &str = "cambium";

/// 一列的 ColumnDefinition41（SPEC 06 §3：flags 暂不填 PRI_KEY/NOT_NULL = 0）
#[derive(Debug, Clone)]
pub struct ColumnDef41 {
    pub name: String,
    pub org_name: String,
    pub charset: u16,
    pub column_length: u32,
    pub type_code: u8,
    pub flags: u16,
    pub decimals: u8,
}

/// 内部类型 → MySQL 类型元组（类型码, 字符集, 列长, decimals）
///
/// 列长/decimals 口径：整型显示宽 11/20，TINY(Bool)=1（BOOL≡TINYINT(1) 惯例），
/// DOUBLE 列长 22 + decimals 31（MySQL 原生对浮点的做法），text/blob 取上限 65535，
/// DATE=10、DATETIME=19。
pub fn mysql_type(ty: ColType) -> (u8, u16, u32, u8) {
    match ty {
        ColType::Bool => (MYSQL_TYPE_TINY, BINARY_CHARSET, 1, 0),
        ColType::Int32 => (MYSQL_TYPE_LONG, BINARY_CHARSET, 11, 0),
        ColType::Int64 => (MYSQL_TYPE_LONGLONG, BINARY_CHARSET, 20, 0),
        ColType::Float64 => (MYSQL_TYPE_DOUBLE, BINARY_CHARSET, 22, 31),
        ColType::Utf8 => (MYSQL_TYPE_VAR_STRING, UTF8_GENERAL_CI, 65535, 0),
        ColType::Bytes => (MYSQL_TYPE_BLOB, BINARY_CHARSET, 65535, 0),
        ColType::Date32 => (MYSQL_TYPE_DATE, BINARY_CHARSET, 10, 0),
        ColType::TimestampMs => (MYSQL_TYPE_DATETIME, BINARY_CHARSET, 19, 0),
    }
}

pub fn column_def(meta: &ColumnMeta) -> ColumnDef41 {
    let (type_code, charset, column_length, decimals) = mysql_type(meta.ty);
    ColumnDef41 {
        name: meta.name.clone(),
        org_name: meta.name.clone(),
        charset,
        column_length,
        type_code,
        flags: 0,
        decimals,
    }
}

pub fn column_def_bytes(meta: &ColumnMeta) -> Vec<u8> {
    column_def(meta).encode()
}

impl ColumnDef41 {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        write_lenenc_str(&mut b, b"def"); // catalog
        write_lenenc_str(&mut b, SCHEMA.as_bytes()); // schema
        write_lenenc_str(&mut b, b""); // table（v1 不提供物理表名）
        write_lenenc_str(&mut b, b""); // org_table
        write_lenenc_str(&mut b, self.name.as_bytes());
        write_lenenc_str(&mut b, self.org_name.as_bytes());
        b.push(0x0C); // 固定字段块长度
        b.extend_from_slice(&self.charset.to_le_bytes());
        b.extend_from_slice(&self.column_length.to_le_bytes());
        b.push(self.type_code);
        b.extend_from_slice(&self.flags.to_le_bytes());
        b.push(self.decimals);
        b.extend_from_slice(&[0u8, 0]); // filler
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_mapping() {
        // SPEC 06 §3：Bool→TINY, Int32→LONG, Int64→LONGLONG, Float64→DOUBLE,
        // Utf8→VAR_STRING, Bytes→BLOB, Date32→DATE, TimestampMs→DATETIME
        assert_eq!(mysql_type(ColType::Bool).0, 1);
        assert_eq!(mysql_type(ColType::Int32).0, 3);
        assert_eq!(mysql_type(ColType::Int64).0, 8);
        assert_eq!(mysql_type(ColType::Float64).0, 5);
        assert_eq!(mysql_type(ColType::Utf8).0, 253);
        assert_eq!(mysql_type(ColType::Bytes).0, 252);
        assert_eq!(mysql_type(ColType::Date32).0, 10);
        assert_eq!(mysql_type(ColType::TimestampMs).0, 12);
        // 字符集：文本 33 / 其余 63；decimals：浮点 31 / 其他 0
        assert_eq!(mysql_type(ColType::Utf8).1, 33);
        assert_eq!(mysql_type(ColType::Int64).1, 63);
        assert_eq!(mysql_type(ColType::Float64).3, 31);
        assert_eq!(mysql_type(ColType::Int32).2, 11);
        assert_eq!(mysql_type(ColType::Int64).2, 20);
    }

    #[test]
    fn encodes_column_definition_41() {
        let bytes = column_def_bytes(&ColumnMeta { name: "1".into(), ty: ColType::Int64 });
        let mut r = crate::codec::Reader::new(&bytes);
        assert_eq!(r.lenenc_bytes().unwrap(), b"def");
        assert_eq!(r.lenenc_bytes().unwrap(), SCHEMA.as_bytes());
        assert_eq!(r.lenenc_bytes().unwrap(), b""); // table
        assert_eq!(r.lenenc_bytes().unwrap(), b""); // org_table
        assert_eq!(r.lenenc_bytes().unwrap(), b"1"); // name
        assert_eq!(r.lenenc_bytes().unwrap(), b"1"); // org_name
        assert_eq!(r.lenenc_int().unwrap(), 0x0C);
        assert_eq!(r.u16_le().unwrap(), BINARY_CHARSET);
        assert_eq!(r.u32_le().unwrap(), 20);
        assert_eq!(r.u8().unwrap(), MYSQL_TYPE_LONGLONG);
        assert_eq!(r.u16_le().unwrap(), 0); // flags 暂不填
        assert_eq!(r.u8().unwrap(), 0);
        assert_eq!(r.take(2).unwrap(), &[0, 0]); // filler
        assert!(r.is_empty());
    }
}
