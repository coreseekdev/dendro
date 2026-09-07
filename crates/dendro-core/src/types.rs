//! 协议无关的值/结果类型 — wire 层与执行器的契约（SPEC 06 §1）。

use arrow::datatypes::DataType;

/// 跨协议参数/单元格值。Date/Timestamp 以内部表示存储（天数/毫秒）。
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Utf8(String),
    Bytes(Vec<u8>),
    /// 自 1970-01-01 的天数
    Date32(i32),
    /// 自 Unix epoch 的毫秒数
    TimestampMs(i64),
}

impl SqlValue {
    pub fn is_null(&self) -> bool {
        matches!(self, SqlValue::Null)
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            SqlValue::Null => "unknown",
            SqlValue::Bool(_) => "boolean",
            SqlValue::Int32(_) => "int4",
            SqlValue::Int64(_) => "int8",
            SqlValue::Float64(_) => "float8",
            SqlValue::Utf8(_) => "text",
            SqlValue::Bytes(_) => "bytea",
            SqlValue::Date32(_) => "date",
            SqlValue::TimestampMs(_) => "timestamp",
        }
    }
    /// PG 类型 OID（SPEC 06 §2.3）
    pub fn pg_oid(&self) -> u32 {
        match self {
            SqlValue::Null => 705, // unknown
            SqlValue::Bool(_) => 16,
            SqlValue::Int32(_) => 23,
            SqlValue::Int64(_) => 20,
            SqlValue::Float64(_) => 701,
            SqlValue::Utf8(_) => 25,
            SqlValue::Bytes(_) => 17,
            SqlValue::Date32(_) => 1082,
            SqlValue::TimestampMs(_) => 1114,
        }
    }
}

/// 内部列类型（catalog/执行器共用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ColType {
    Bool,
    Int32,
    Int64,
    Float64,
    Utf8,
    Bytes,
    Date32,
    TimestampMs,
}

impl ColType {
    pub fn type_name(self) -> &'static str {
        match self {
            ColType::Bool => "boolean",
            ColType::Int32 => "integer",
            ColType::Int64 => "bigint",
            ColType::Float64 => "double precision",
            ColType::Utf8 => "text",
            ColType::Bytes => "bytea",
            ColType::Date32 => "date",
            ColType::TimestampMs => "timestamp without time zone",
        }
    }
    pub fn pg_oid(self) -> u32 {
        match self {
            ColType::Bool => 16,
            ColType::Int32 => 23,
            ColType::Int64 => 20,
            ColType::Float64 => 701,
            ColType::Utf8 => 25,
            ColType::Bytes => 17,
            ColType::Date32 => 1082,
            ColType::TimestampMs => 1114,
        }
    }
    pub fn arrow(self) -> DataType {
        match self {
            ColType::Bool => DataType::Boolean,
            ColType::Int32 => DataType::Int32,
            ColType::Int64 => DataType::Int64,
            ColType::Float64 => DataType::Float64,
            ColType::Utf8 => DataType::Utf8,
            ColType::Bytes => DataType::Binary,
            ColType::Date32 => DataType::Date32,
            ColType::TimestampMs => DataType::Timestamp(arrow::datatypes::TimeUnit::Millisecond, None),
        }
    }
    pub fn from_parse(s: &str) -> Option<ColType> {
        Some(match s.to_ascii_lowercase().as_str() {
            "bool" | "boolean" => ColType::Bool,
            "smallint" | "int2" | "int" | "int4" | "integer" | "signed" => ColType::Int32,
            "bigint" | "int8" | "bigserial" | "serial8" => ColType::Int64,
            "real" | "float4" => ColType::Float64,
            "double" | "float8" | "double precision" | "float" | "decimal" | "numeric" => ColType::Float64,
            "text" | "varchar" | "varchar2" | "char" | "bpchar" | "character varying" | "string" => ColType::Utf8,
            "bytea" | "blob" | "binary" | "varbinary" => ColType::Bytes,
            "date" => ColType::Date32,
            "timestamp" | "timestamp without time zone" | "datetime" => ColType::TimestampMs,
            _ => return None,
        })
    }
}

/// 结果集列元数据（wire 层据此构造 RowDescription / ColumnDefinition）
#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub ty: ColType,
}

/// 单条语句执行输出
#[derive(Debug)]
pub enum Output {
    /// DDL/DML：tag 形如 "INSERT 0 3" / "CREATE TABLE"
    Command { tag: String, affected: u64 },
    Rows(RecordSet),
}

/// 行集：Arrow 批（AP/TP 统一货币）+ 列元数据
#[derive(Debug, Clone)]
pub struct RecordSet {
    pub columns: Vec<ColumnMeta>,
    pub batches: Vec<arrow::record_batch::RecordBatch>,
}

impl RecordSet {
    pub fn empty(columns: Vec<ColumnMeta>) -> Self {
        Self { columns, batches: vec![] }
    }
    /// 迭代为文本行（协议 text 格式），None=列内 NULL
    pub fn text_rows(&self) -> Vec<Vec<Option<String>>> {
        let mut rows = Vec::new();
        for batch in &self.batches {
            let n = batch.num_rows();
            for r in 0..n {
                let mut row = Vec::with_capacity(batch.num_columns());
                for col in batch.columns().iter() {
                    row.push(cell_to_text(col, r));
                }
                rows.push(row);
            }
        }
        rows
    }
    pub fn total_rows(&self) -> usize {
        self.batches.iter().map(|b| b.num_rows()).sum()
    }
    /// 迭代为类型化行（协议 binary 编码用）；列型按 self.columns（描述口径）
    pub fn typed_rows(&self) -> Vec<Vec<Option<SqlValue>>> {
        use arrow::array::{Array, Date32Array, Float64Array, Int32Array, Int64Array, StringArray, BinaryArray, BooleanArray, TimestampMillisecondArray};
        let mut rows = Vec::new();
        for batch in &self.batches {
            for r in 0..batch.num_rows() {
                let mut row = Vec::with_capacity(batch.num_columns());
                for (ci, (col, meta)) in batch.columns().iter().zip(&self.columns).enumerate() {
                    let _ = ci;
                    if col.is_null(r) {
                        row.push(None);
                        continue;
                    }
                    let v = match col.data_type() {
                        DataType::Boolean => col.as_any().downcast_ref::<BooleanArray>().map(|a| SqlValue::Bool(a.value(r))),
                        DataType::Int32 => col.as_any().downcast_ref::<Int32Array>().map(|a| SqlValue::Int32(a.value(r))),
                        DataType::Int64 => col.as_any().downcast_ref::<Int64Array>().map(|a| SqlValue::Int64(a.value(r))),
                        DataType::Float64 => col.as_any().downcast_ref::<Float64Array>().map(|a| SqlValue::Float64(a.value(r))),
                        DataType::Utf8 => col.as_any().downcast_ref::<StringArray>().map(|a| SqlValue::Utf8(a.value(r).to_string())),
                        DataType::Binary => col.as_any().downcast_ref::<BinaryArray>().map(|a| SqlValue::Bytes(a.value(r).to_vec())),
                        DataType::Date32 => col.as_any().downcast_ref::<Date32Array>().map(|a| SqlValue::Date32(a.value(r))),
                        DataType::Timestamp(arrow::datatypes::TimeUnit::Millisecond, _) => col.as_any().downcast_ref::<TimestampMillisecondArray>().map(|a| SqlValue::TimestampMs(a.value(r))),
                        _ => None,
                    };
                    // 按描述列型收敛（i32→i64 等宽化）
                    row.push(v.map(|val| coerce_to(val, meta.ty)));
                }
                rows.push(row);
            }
        }
        rows
    }
}

/// 值向声明列型收敛（宽化安全；不丢失）
pub fn coerce_to(v: SqlValue, ty: ColType) -> SqlValue {
    use SqlValue::*;
    match (&v, ty) {
        (Int32(i), ColType::Int64) => Int64(*i as i64),
        (Int32(i), ColType::Float64) => Float64(*i as f64),
        (Int64(i), ColType::Float64) => Float64(*i as f64),
        (Float64(f), ColType::Float64) => Float64(*f),
        _ => v,
    }
}

fn cell_to_text(col: &arrow::array::ArrayRef, row: usize) -> Option<String> {
    use arrow::array::{Array, Date32Array, Float64Array, Int32Array, Int64Array, StringArray, BinaryArray, BooleanArray, TimestampMillisecondArray};
    if col.is_null(row) {
        return None;
    }
    let s = match col.data_type() {
        DataType::Boolean => {
            let a = col.as_any().downcast_ref::<BooleanArray>()?;
            a.value(row).to_string()
        }
        DataType::Int32 => {
            let a = col.as_any().downcast_ref::<Int32Array>()?;
            a.value(row).to_string()
        }
        DataType::Int64 => {
            let a = col.as_any().downcast_ref::<Int64Array>()?;
            a.value(row).to_string()
        }
        DataType::Float64 => {
            let a = col.as_any().downcast_ref::<Float64Array>()?;
            format_f64(a.value(row))
        }
        DataType::Utf8 => {
            let a = col.as_any().downcast_ref::<StringArray>()?;
            a.value(row).to_string()
        }
        DataType::Binary => {
            let a = col.as_any().downcast_ref::<BinaryArray>()?;
            format!("\\x{}", a.value(row).iter().map(|b| format!("{b:02x}")).collect::<String>())
        }
        DataType::Date32 => {
            let a = col.as_any().downcast_ref::<Date32Array>()?;
            crate::types::format_date(a.value(row))
        }
        DataType::Timestamp(arrow::datatypes::TimeUnit::Millisecond, _) => {
            let a = col.as_any().downcast_ref::<TimestampMillisecondArray>()?;
            crate::types::format_ts_ms(a.value(row))
        }
        _ => return Some("<unsupported>".into()),
    };
    Some(s)
}

/// PG 风格 float 输出：整数省小数、保留最短可往返表示
pub fn format_f64(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        let s = format!("{v}");
        s
    }
}

pub fn format_date(days: i32) -> String {
    // days since 1970-01-01
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn format_ts_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let hh = rem / 3_600_000;
    let mm = (rem % 3_600_000) / 60_000;
    let ss = (rem % 60_000) / 1000;
    let ms2 = rem % 1000;
    if ms2 == 0 {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{ms2:03}")
    }
}

/// Howard Hinnant 的 civil_from_days 算法
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
