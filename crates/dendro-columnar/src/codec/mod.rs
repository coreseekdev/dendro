//! codec 层：值域模型 + 编解码分发 + 自适应策略（SPEC 05 §3/§4，SPEC 08 §5）。
//!
//! 值域模型（`ColumnValues`）：
//! - `Fixed`：定宽列统一为**无符号化 u64 值域**（SPEC 05 §3 “整型无符号化”）：
//!   i32/date32 → `v as u32`；i64/ts → `v as u64`；bool → 0/1（width=1 字节）；
//!   f64 → `to_bits()`（比特域精确往返，字典/bitpack/delta 同样适用）。
//! - `Var`：utf8/binary → offsets(u32, rows+1) + bytes。
//!
//! min/max 用的是另一个域（order 域，见 `stats::order_key`）：保序变换，供
//! zone map 剪枝；两域仅在编解码器内部与统计层互转，块头/foooter 永远存 order 域。

mod bitpack;
mod delta;
mod rledict;
mod raw;
mod zstd;

use crate::stats::ColStats;
use crate::{Error, Result, ZSTD_LEVEL};

use arrow::array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array,
    StringArray, TimestampMillisecondArray,
};
use arrow::buffer::{Buffer, BooleanBuffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, TimeUnit};
use dendro_core::types::ColType;

/// 块编码器 id（SPEC 05 §3：0 RAW 1 BITPACK 2 RLE_DICT 3 ZSTD，4 FSST 保留，5 DELTA）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecId {
    Raw = 0,
    BitPack = 1,
    RleDict = 2,
    Zstd = 3,
    Delta = 5,
}

impl CodecId {
    pub fn from_u8(v: u8) -> Result<CodecId> {
        Ok(match v {
            0 => CodecId::Raw,
            1 => CodecId::BitPack,
            2 => CodecId::RleDict,
            3 => CodecId::Zstd,
            5 => CodecId::Delta,
            other => return Err(Error::Corrupt(format!("unknown codec id {other}"))),
        })
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// 列物理布局（决定值域解释与 RAW 定宽字节数）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    /// 整型域：width 为 RAW 定宽字节数（bool=1, i32/date32=4, i64/ts=8）
    Int { width: usize },
    /// f64：值域 = to_bits()，order 域 = IEEE754 全序变换
    Float64,
    /// utf8 / binary：offsets + bytes
    Var,
}

impl Layout {
    pub(crate) fn width_bytes(&self) -> usize {
        match self {
            Layout::Int { width } => *width,
            Layout::Float64 => 8,
            Layout::Var => 0,
        }
    }
}

pub(crate) fn layout_of(dt: &DataType) -> Result<Layout> {
    Ok(match dt {
        DataType::Boolean => Layout::Int { width: 1 },
        DataType::Int32 | DataType::Date32 => Layout::Int { width: 4 },
        DataType::Int64 => Layout::Int { width: 8 },
        DataType::Float64 => Layout::Float64,
        DataType::Timestamp(TimeUnit::Millisecond, _) => Layout::Int { width: 8 },
        DataType::Utf8 | DataType::Binary => Layout::Var,
        other => return Err(Error::UnsupportedArrowType(format!("{other:?}"))),
    })
}

/// Arrow DataType → ColType（codec 决策回调口径，与 dendro-core::types 一致）
pub fn col_type_of(dt: &DataType) -> Result<ColType> {
    Ok(match dt {
        DataType::Boolean => ColType::Bool,
        DataType::Int32 => ColType::Int32,
        DataType::Int64 => ColType::Int64,
        DataType::Float64 => ColType::Float64,
        DataType::Utf8 => ColType::Utf8,
        DataType::Binary => ColType::Bytes,
        DataType::Date32 => ColType::Date32,
        DataType::Timestamp(TimeUnit::Millisecond, _) => ColType::TimestampMs,
        other => return Err(Error::UnsupportedArrowType(format!("{other:?}"))),
    })
}

/// footer 内 schema 的 Arrow 类型 id（自描述二进制，SPEC 05 §2）
pub(crate) fn arrow_type_id(dt: &DataType) -> Result<u16> {
    Ok(match dt {
        DataType::Boolean => 0,
        DataType::Int32 => 1,
        DataType::Int64 => 2,
        DataType::Float64 => 3,
        DataType::Utf8 => 4,
        DataType::Binary => 5,
        DataType::Date32 => 6,
        DataType::Timestamp(TimeUnit::Millisecond, _) => 7,
        other => return Err(Error::UnsupportedArrowType(format!("{other:?}"))),
    })
}

pub(crate) fn arrow_type_from_id(id: u16) -> Result<DataType> {
    Ok(match id {
        0 => DataType::Boolean,
        1 => DataType::Int32,
        2 => DataType::Int64,
        3 => DataType::Float64,
        4 => DataType::Utf8,
        5 => DataType::Binary,
        6 => DataType::Date32,
        7 => DataType::Timestamp(TimeUnit::Millisecond, None),
        other => return Err(Error::Corrupt(format!("unknown arrow type id {other}"))),
    })
}

/// 一个 chunk（块）解码/编码前的中间表示 + validity 位图（LSB-first、offset 0）
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChunkPart {
    pub vals: ColumnValues,
    /// None = 全非空（flags.bit0）；Some = LSB-first 位图，bit i = row i valid
    pub validity: Option<Vec<u8>>,
}

/// 列值的无符号化/物理域表示（SPEC 05 §3 data 区按 codec 的共同输入）
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ColumnValues {
    Fixed {
        width: usize,
        is_float: bool,
        values: Vec<u64>,
    },
    Var {
        offsets: Vec<u32>,
        bytes: Vec<u8>,
    },
}

impl ColumnValues {
    pub(crate) fn rows(&self) -> usize {
        match self {
            ColumnValues::Fixed { values, .. } => values.len(),
            ColumnValues::Var { offsets, .. } => offsets.len().saturating_sub(1),
        }
    }
    /// RAW 编码后的字节数（= 块头 raw_len 口径：解码后字节数，SPEC 05 §3）
    pub(crate) fn raw_len(&self) -> u64 {
        match self {
            ColumnValues::Fixed { width, values, .. } => (values.len() * width) as u64,
            ColumnValues::Var { offsets, bytes } => (offsets.len() * 4 + bytes.len()) as u64,
        }
    }
}

impl ChunkPart {
    pub(crate) fn rows(&self) -> usize {
        self.vals.rows()
    }
    pub(crate) fn null_count(&self) -> usize {
        match &self.validity {
            // validity 位图 1=valid（Arrow 同构）⇒ null = rows - set bits
            None => 0,
            Some(b) => {
                let set = b.iter().map(|x| x.count_ones()).sum::<u32>() as usize;
                self.rows() - set
            }
        }
    }
    pub(crate) fn is_valid(&self, i: usize) -> bool {
        match &self.validity {
            None => true,
            Some(b) => b[i >> 3] >> (i & 7) & 1 == 1,
        }
    }
}

/// 从 Arrow 数组物化为 ChunkPart（null 槽位值归零/空串，validity 单独成区）。
/// 归零保证解码缓冲确定性，便于 GPU 端批量搬运（SPEC 05 §3）。
pub(crate) fn chunk_of(array: &ArrayRef) -> Result<ChunkPart> {
    use arrow::array::*;
    let n = array.len();
    let vals = match array.data_type() {
        DataType::Boolean => {
            let a = downcast::<BooleanArray>(array)?;
            ColumnValues::Fixed {
                width: 1,
                is_float: false,
                values: (0..n).map(|i| a.value(i) as u64).collect(),
            }
        }
        DataType::Int32 => {
            let a = downcast::<Int32Array>(array)?;
            ColumnValues::Fixed {
                width: 4,
                is_float: false,
                values: (0..n).map(|i| a.value(i) as u32 as u64).collect(),
            }
        }
        DataType::Int64 => {
            let a = downcast::<Int64Array>(array)?;
            ColumnValues::Fixed {
                width: 8,
                is_float: false,
                values: (0..n).map(|i| a.value(i) as u64).collect(),
            }
        }
        DataType::Float64 => {
            let a = downcast::<Float64Array>(array)?;
            ColumnValues::Fixed {
                width: 8,
                is_float: true,
                values: (0..n).map(|i| a.value(i).to_bits()).collect(),
            }
        }
        DataType::Date32 => {
            let a = downcast::<Date32Array>(array)?;
            ColumnValues::Fixed {
                width: 4,
                is_float: false,
                values: (0..n).map(|i| a.value(i) as u32 as u64).collect(),
            }
        }
        DataType::Timestamp(TimeUnit::Millisecond, _) => {
            let a = downcast::<TimestampMillisecondArray>(array)?;
            ColumnValues::Fixed {
                width: 8,
                is_float: false,
                values: (0..n).map(|i| a.value(i) as u64).collect(),
            }
        }
        DataType::Utf8 => {
            let a = downcast::<StringArray>(array)?;
            var_values(n, |i| a.value(i).as_bytes())
        }
        DataType::Binary => {
            let a = downcast::<BinaryArray>(array)?;
            var_values(n, |i| a.value(i))
        }
        other => return Err(Error::UnsupportedArrowType(format!("{other:?}"))),
    };
    let validity = validity_bytes(array.as_ref());
    Ok(ChunkPart { vals, validity })
}

fn downcast<T: arrow::array::Array + 'static>(array: &ArrayRef) -> Result<&T> {
    array
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| Error::Corrupt("array downcast failed".into()))
}

fn var_values<'a>(n: usize, at: impl Fn(usize) -> &'a [u8]) -> ColumnValues {
    let mut offsets = Vec::with_capacity(n + 1);
    let mut bytes = Vec::new();
    offsets.push(0u32);
    for i in 0..n {
        bytes.extend_from_slice(at(i));
        offsets.push(bytes.len() as u32);
    }
    ColumnValues::Var { offsets, bytes }
}

/// validity 位图 → LSB-first 字节（offset 0）。全非空 → None（flags.bit0，SPEC 05 §3）。
pub(crate) fn validity_bytes(array: &dyn arrow::array::Array) -> Option<Vec<u8>> {
    let nulls = array.nulls()?;
    if array.null_count() == 0 {
        return None;
    }
    let mut out = vec![0u8; array.len().div_ceil(8)];
    for i in nulls.inner().set_indices() {
        out[i >> 3] |= 1 << (i & 7);
    }
    Some(out)
}

/// 跨块合并（writer 合并 batch 切片 / reader 合并 RG 内多块）。
/// 列内合并是纯拼接：Fixed 直连；Var 调整 offsets 基址；validity 按位补齐。
pub(crate) fn merge_chunks(mut parts: Vec<ChunkPart>) -> Result<ChunkPart> {
    if parts.len() == 1 {
        return Ok(parts.pop().unwrap());
    }
    if parts.is_empty() {
        return Err(Error::Corrupt("merge_chunks: no parts".into()));
    }
    let total_rows: usize = parts.iter().map(|p| p.rows()).sum();
    let validity = if parts.iter().all(|p| p.validity.is_none()) {
        None
    } else {
        let mut out = vec![0u8; total_rows.div_ceil(8)];
        let mut base = 0usize;
        for p in &parts {
            let n = p.rows();
            match &p.validity {
                Some(b) => append_bits(&mut out, base, b, n),
                None => set_bits(&mut out, base, n),
            }
            base += n;
        }
        Some(out)
    };
    let vals = match &parts[0].vals {
        ColumnValues::Fixed { width, is_float, .. } => {
            let mut values = Vec::with_capacity(total_rows);
            for p in &parts {
                match &p.vals {
                    ColumnValues::Fixed { values: v, .. } => values.extend_from_slice(v),
                    _ => return Err(Error::Corrupt("merge type mismatch".into())),
                }
            }
            ColumnValues::Fixed { width: *width, is_float: *is_float, values }
        }
        ColumnValues::Var { .. } => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            let mut bytes = Vec::new();
            offsets.push(0u32);
            for p in &parts {
                match &p.vals {
                    ColumnValues::Var { offsets: o, bytes: b } => {
                        for &off in &o[1..] {
                            offsets.push(off + bytes.len() as u32);
                        }
                        bytes.extend_from_slice(b);
                    }
                    _ => return Err(Error::Corrupt("merge type mismatch".into())),
                }
            }
            ColumnValues::Var { offsets, bytes }
        }
    };
    Ok(ChunkPart { vals, validity })
}

fn append_bits(dst: &mut [u8], base: usize, src: &[u8], n: usize) {
    for i in 0..n {
        if src[i >> 3] >> (i & 7) & 1 == 1 {
            let p = base + i;
            dst[p >> 3] |= 1 << (p & 7);
        }
    }
}

fn set_bits(dst: &mut [u8], base: usize, n: usize) {
    for i in 0..n {
        let p = base + i;
        dst[p >> 3] |= 1 << (p & 7);
    }
}

/// ChunkPart → Arrow 数组（offset 0 直接由字节缓冲构造，SPEC 05 §5）。
/// 定宽路径产出连续定宽 buffer + 独立 validity（GPU 显存拷贝无需重排）。
pub(crate) fn build_array(mut part: ChunkPart, dt: &DataType) -> Result<ArrayRef> {
    use std::sync::Arc;
    let n = part.rows();
    let nulls: Option<NullBuffer> = part
        .validity
        .take()
        .map(|vb| NullBuffer::new(BooleanBuffer::new(Buffer::from_vec(vb), 0, n)));
    let arr: ArrayRef = match dt {
        DataType::Boolean => {
            let values = fixed_of(&part, CodecId::Raw)?;
            let mut bits = vec![0u8; n.div_ceil(8)];
            for (i, &v) in values.iter().enumerate() {
                if v != 0 {
                    bits[i >> 3] |= 1 << (i & 7);
                }
            }
            Arc::new(BooleanArray::new(
                BooleanBuffer::new(Buffer::from_vec(bits), 0, n),
                nulls,
            ))
        }
        DataType::Int32 => {
            let values = fixed_of(&part, CodecId::Raw)?;
            let sb: ScalarBuffer<i32> =
                values.iter().map(|&v| v as u32 as i32).collect::<Vec<_>>().into();
            Arc::new(Int32Array::new(sb, nulls))
        }
        DataType::Int64 => {
            let values = fixed_of(&part, CodecId::Raw)?;
            let sb: ScalarBuffer<i64> =
                values.iter().map(|&v| v as i64).collect::<Vec<_>>().into();
            Arc::new(Int64Array::new(sb, nulls))
        }
        DataType::Float64 => {
            let values = fixed_of(&part, CodecId::Raw)?;
            let sb: ScalarBuffer<f64> =
                values.iter().map(|&v| f64::from_bits(v)).collect::<Vec<_>>().into();
            Arc::new(Float64Array::new(sb, nulls))
        }
        DataType::Date32 => {
            let values = fixed_of(&part, CodecId::Raw)?;
            let sb: ScalarBuffer<i32> =
                values.iter().map(|&v| v as u32 as i32).collect::<Vec<_>>().into();
            Arc::new(Date32Array::new(sb, nulls))
        }
        DataType::Timestamp(TimeUnit::Millisecond, _) => {
            let values = fixed_of(&part, CodecId::Raw)?;
            let sb: ScalarBuffer<i64> =
                values.iter().map(|&v| v as i64).collect::<Vec<_>>().into();
            Arc::new(TimestampMillisecondArray::new(sb, nulls))
        }
        DataType::Utf8 | DataType::Binary => {
            let (offsets, bytes) = match &part.vals {
                ColumnValues::Var { offsets, bytes } => (offsets, bytes),
                _ => return Err(Error::Corrupt("var data expected".into())),
            };
            if bytes.len() > i32::MAX as usize {
                return Err(Error::InvalidInput("var chunk exceeds i32 offsets".into()));
            }
            let ob = OffsetBuffer::new(ScalarBuffer::from(
                offsets.iter().map(|&o| o as i32).collect::<Vec<_>>(),
            ));
            let buf = Buffer::from_vec(bytes.clone());
            match dt {
                DataType::Utf8 => Arc::new(StringArray::try_new(ob, buf, nulls)?),
                _ => Arc::new(BinaryArray::try_new(ob, buf, nulls)?),
            }
        }
        other => return Err(Error::UnsupportedArrowType(format!("{other:?}"))),
    };
    Ok(arr)
}

/// 编码一个块的数据区（SPEC 05 §3 data 区按 codec）。返回 raw_len（解码后字节数）。
pub(crate) fn encode_chunk(codec: CodecId, part: &ChunkPart, out: &mut Vec<u8>) -> Result<u64> {
    match codec {
        CodecId::Raw => Ok(raw::encode(part, out) as u64),
        CodecId::BitPack => {
            let values = fixed_of(part, codec)?;
            bitpack::encode(values, out);
            Ok(part.vals.raw_len())
        }
        CodecId::RleDict => {
            let l0 = out.len();
            match &part.vals {
                ColumnValues::Fixed { width, values, .. } => {
                    rledict::encode_fixed(*width, values, out)?
                }
                ColumnValues::Var { offsets, bytes } => {
                    rledict::encode_var(offsets, bytes, out)?
                }
            }
            let _ = l0;
            Ok(part.vals.raw_len())
        }
        CodecId::Zstd => {
            // 整块 zstd = 对 RAW 字节流压缩（SPEC 05 §3 ZSTD；级别见 flags 高位）
            let mut tmp = Vec::with_capacity(part.vals.raw_len() as usize);
            let raw_len = raw::encode(part, &mut tmp);
            let packed = zstd::compress(&tmp, ZSTD_LEVEL)?;
            out.extend_from_slice(&packed);
            Ok(raw_len as u64)
        }
        CodecId::Delta => {
            let values = fixed_of(part, codec)?;
            delta::encode(values, out);
            Ok(part.vals.raw_len())
        }
    }
}

/// 解码一个块的数据区 → 中间表示。`raw_len` 来自块头，用于 zstd 容量与一致性校验。
pub(crate) fn decode_chunk(
    codec: CodecId,
    data: &[u8],
    raw_len: usize,
    rows: usize,
    layout: &Layout,
) -> Result<ColumnValues> {
    match codec {
        CodecId::Raw => raw::decode(data, raw_len, rows, layout),
        CodecId::BitPack => {
            let (width, is_float) = fixed_layout(layout, codec)?;
            Ok(ColumnValues::Fixed {
                width,
                is_float,
                values: bitpack::decode(data, rows)?,
            })
        }
        CodecId::RleDict => rledict::decode(data, rows, layout),
        CodecId::Zstd => {
            let raw = zstd::decompress(data, raw_len)?;
            raw::decode(&raw, raw_len, rows, layout)
        }
        CodecId::Delta => {
            let (width, is_float) = fixed_layout(layout, codec)?;
            Ok(ColumnValues::Fixed {
                width,
                is_float,
                values: delta::decode(data, rows)?,
            })
        }
    }
}

fn fixed_of(part: &ChunkPart, codec: CodecId) -> Result<&[u64]> {
    match &part.vals {
        ColumnValues::Fixed { values, .. } => Ok(values),
        ColumnValues::Var { .. } => Err(Error::CodecNotApplicable(codec)),
    }
}

fn fixed_layout(layout: &Layout, codec: CodecId) -> Result<(usize, bool)> {
    match layout {
        Layout::Int { width } => Ok((*width, false)),
        Layout::Float64 => Ok((8, true)),
        Layout::Var => Err(Error::CodecNotApplicable(codec)),
    }
}

/// 内置自适应 codec 策略（SPEC 05 §4 规则 + SPEC 08 §5 实证表）。
///
/// - 顺序/单调整型（pk/ts）→ DELTA（实证：zstd 独占 delta 熵 3.9-10.8R，
///   内置 DELTA 摆脱对 zstd 的隐性依赖；SPEC 08 §5 发现 3）
/// - 低基数（distinct_ratio < 0.1 整型 / < 0.2 文本）→ RLE_DICT
///   （实证：mid-card dict 13.8-15R vs raw+zstd 2.6R，10 倍差距）
/// - 值域窄 → BITPACK（id 流/窄整数位搬运，T 接近 RAW）
/// - 随机整型 / f64 → RAW（实证：zstd 无益甚至负收益；mantissa 低位近随机）
/// - ZSTD 不进默认策略（热层永不过熵解码器，SPEC 05 §3；冷块经 codec_choice 指定）
pub fn choose_codec(_name: &str, ty: ColType, s: &ColStats) -> CodecId {
    let ratio = s.distinct_ratio();
    match ty {
        ColType::Float64 => CodecId::Raw,
        ColType::Utf8 | ColType::Bytes => {
            if ratio < 0.2 {
                CodecId::RleDict
            } else {
                CodecId::Raw
            }
        }
        ColType::Bool | ColType::Int32 | ColType::Int64 | ColType::Date32 | ColType::TimestampMs => {
            if s.monotonic {
                CodecId::Delta
            } else if ratio < 0.1 {
                CodecId::RleDict
            } else if narrow_range(ty, s) {
                CodecId::BitPack
            } else {
                CodecId::Raw
            }
        }
    }
}

/// 值域窄判定（order 域 span 的有效位数；SPEC 05 §4 “值域窄 → BITPACK”）：
/// 4 字节型 ≤24bit、8 字节型 ≤48bit、bool 恒为 1bit。
fn narrow_range(ty: ColType, s: &ColStats) -> bool {
    if s.rows == 0 || s.rows == s.null_count {
        return false;
    }
    let span = s.max.wrapping_sub(s.min);
    let bits = 64 - span.leading_zeros();
    match ty {
        ColType::Bool => true,
        ColType::Int32 | ColType::Date32 => bits <= 24,
        ColType::Int64 | ColType::TimestampMs => bits <= 48,
        _ => false,
    }
}

/// zigzag（DELTA 一阶差分用）：把有符号差分映射为无符号 varint。
/// 输入是差分的二补码位型；负数需要**算术**右移语义（先转 i64 再移位）。
pub(crate) fn zigzag_encode(d: u64) -> u64 {
    let n = d as i64;
    ((n << 1) ^ (n >> 63)) as u64
}

pub(crate) fn zigzag_decode(z: u64) -> u64 {
    ((z >> 1) as i64 ^ -((z & 1) as i64)) as u64
}

/// LEB128 varint 写入
pub(crate) fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// LEB128 varint 读取（带截断防护）
pub(crate) fn read_varint(cur: &mut &[u8]) -> Result<u64> {
    let mut r = 0u64;
    let mut shift = 0u32;
    loop {
        if cur.is_empty() {
            return Err(Error::Corrupt("varint truncated".into()));
        }
        let b = cur[0];
        *cur = &cur[1..];
        r |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(r);
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::Corrupt("varint too long".into()));
        }
    }
}
