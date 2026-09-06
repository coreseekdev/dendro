//! CBF footer：自描述二进制元数据（SPEC 05 §2，**不要 JSON**）。
//!
//! ```text
//! [footer body（64B 对齐起点）]
//!   schema:  col_count u32; per col { name_len u32, name, arrow_type u16, nullable u8 }
//!   rg_count u32
//!   total_rows u64
//!   pk_min u64, pk_max u64          // 第 0 列 order 域（文件级 row range，SPEC §2）
//!   per RG { first_row u64, rows u32, col_count u32,
//!            per col { block_count u32,
//!                      per block { offset u64, rows u32, null_count u32,
//!                                  codec u8, flags u8, min u64, max u64 },
//!                      validity_offset u64, validity_len u64 } }
//! [footer_len u32][magic u32]        // 文件尾 8B：footer_len = body + 8
//! ```
//!
//! 说明：SPEC §2 文件级还有 table_id/schema hash/key cols——本库无 catalog 上下文，
//! 以**内嵌完整 schema** 替代（自描述超集）；pk 取第 0 列（“顺序即布局”，SPEC §2）。
//! 每块 codec 序列即 SPEC §3 的 decode_plan 输入（per-col codec + 输出 buffer 形状）。

use crate::codec::{arrow_type_from_id, arrow_type_id, CodecId};
use crate::{Error, Result, ALIGN, BLOCK_HEADER_LEN};
use arrow::datatypes::{Field, Schema, SchemaRef};
use std::sync::Arc;

/// 文件尾 magic u32（与块 magic 0xCB71 呼应的文件级标识）
pub const FILE_MAGIC: u32 = 0xCB71_F001;

/// 每 (行组, 列) 的 chunk 元数据：块列表 + validity 位图区位置
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkMeta {
    pub blocks: Vec<BlockMeta>,
    pub validity_offset: u64,
    pub validity_len: u64,
}

/// 每 Block 的 zone map/记账（SPEC 05 §3 块头镜像 + 文件偏移）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMeta {
    /// 64B 对齐的块头文件偏移
    pub offset: u64,
    pub rows: u32,
    pub null_count: u32,
    pub codec: CodecId,
    /// bit0 all_non_null / bit1 sorted / bits2-7 zstd level
    pub flags: u8,
    /// 定点解释（SPEC 05 §3）
    pub min: u64,
    pub max: u64,
}

/// 每行组元数据
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgMeta {
    /// 行组首行的文件内行号
    pub first_row: u64,
    pub rows: u32,
    pub cols: Vec<ChunkMeta>,
}

/// footer 解析结果（read_footer 的返回）
#[derive(Debug, Clone)]
pub struct CbfFooter {
    pub schema: SchemaRef,
    pub rg_count: usize,
    pub total_rows: u64,
    /// 文件级 row range：第 0 列（pk 口径）order 域 min/max
    pub pk_min: u64,
    pub pk_max: u64,
    pub rgs: Vec<RgMeta>,
    /// footer body 起始文件偏移（writer 保证 64B 对齐）
    pub footer_offset: u64,
}

impl CbfFooter {
    /// 剪枝视角的每行组每列摘要：(rg_idx, rows, min, max, null_count, codec)
    pub fn column_stats(&self, col: usize) -> Vec<(usize, u32, u64, u64, u32, CodecId)> {
        self.rgs
            .iter()
            .enumerate()
            .map(|(ri, rg)| {
                let cm = &rg.cols[col];
                // v1 每 chunk 单块；多块时取聚合（min/max 合并，null/rows 求和）
                let mut rows = 0u32;
                let mut nulls = 0u32;
                let mut mn = u64::MAX;
                let mut mx = 0u64;
                let mut codec = CodecId::Raw;
                for b in &cm.blocks {
                    rows += b.rows;
                    nulls += b.null_count;
                    mn = mn.min(b.min);
                    mx = mx.max(b.max);
                    codec = b.codec;
                }
                (ri, rows, mn, mx, nulls, codec)
            })
            .collect()
    }
}

pub(crate) fn pad_align(buf: &mut Vec<u8>) {
    while buf.len() % ALIGN != 0 {
        buf.push(0);
    }
}

/// 块头 64B 的字段集（ser/de 双方共用，SPEC 05 §3）
pub(crate) struct BlockHeader {
    pub codec: u8,
    pub flags: u8,
    pub rows: u32,
    pub null_count: u32,
    pub data_len: u64,
    pub raw_len: u64,
    pub min: u64,
    pub max: u64,
    pub crc: u32,
}

/// 写 64B 块头：magic u16, version u16, codec u8, flags u8, rows u32,
/// null_count u32, data_len u64, raw_len u64, min u64, max u64, reserved u16, crc u32
/// + 12B 零填充（字段共 52B）。
pub(crate) fn write_block_header(out: &mut [u8; BLOCK_HEADER_LEN], h: &BlockHeader) {
    for b in out.iter_mut() {
        *b = 0;
    }
    out[0..2].copy_from_slice(&crate::BLOCK_MAGIC.to_le_bytes());
    out[2..4].copy_from_slice(&crate::BLOCK_VERSION.to_le_bytes());
    out[4] = h.codec;
    out[5] = h.flags;
    out[6..10].copy_from_slice(&h.rows.to_le_bytes());
    out[10..14].copy_from_slice(&h.null_count.to_le_bytes());
    out[14..22].copy_from_slice(&h.data_len.to_le_bytes());
    out[22..30].copy_from_slice(&h.raw_len.to_le_bytes());
    out[30..38].copy_from_slice(&h.min.to_le_bytes());
    out[38..46].copy_from_slice(&h.max.to_le_bytes());
    // out[46..48] reserved = 0
    out[48..52].copy_from_slice(&h.crc.to_le_bytes());
    // out[52..64] 零填充
}

pub(crate) fn parse_block_header(b: &[u8]) -> Result<BlockHeader> {
    if b.len() < BLOCK_HEADER_LEN {
        return Err(Error::Corrupt("block header truncated".into()));
    }
    let magic = u16::from_le_bytes([b[0], b[1]]);
    if magic != crate::BLOCK_MAGIC {
        return Err(Error::Corrupt(format!("block magic {magic:#x} != 0xCB71")));
    }
    let version = u16::from_le_bytes([b[2], b[3]]);
    if version != crate::BLOCK_VERSION {
        return Err(Error::Corrupt(format!("block version {version} unsupported")));
    }
    let u32le = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    let u64le = |s: &[u8]| {
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        u64::from_le_bytes(a)
    };
    Ok(BlockHeader {
        codec: b[4],
        flags: b[5],
        rows: u32le(&b[6..10]),
        null_count: u32le(&b[10..14]),
        data_len: u64le(&b[14..22]),
        raw_len: u64le(&b[22..30]),
        min: u64le(&b[30..38]),
        max: u64le(&b[38..46]),
        crc: u32le(&b[48..52]),
    })
}

/// 追加 footer（body + 文件尾 8B），body 起点 64B 对齐。
pub(crate) fn write_footer(
    buf: &mut Vec<u8>,
    schema: &SchemaRef,
    rgs: &[RgMeta],
    total_rows: u64,
    pk_min: u64,
    pk_max: u64,
) -> Result<()> {
    pad_align(buf);
    let body_start = buf.len();
    let fields = schema.fields();
    buf.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    for f in fields.iter() {
        let name = f.name().as_bytes();
        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        buf.extend_from_slice(name);
        buf.extend_from_slice(&arrow_type_id(f.data_type())?.to_le_bytes());
        buf.push(f.is_nullable() as u8);
    }
    buf.extend_from_slice(&(rgs.len() as u32).to_le_bytes());
    buf.extend_from_slice(&total_rows.to_le_bytes());
    buf.extend_from_slice(&pk_min.to_le_bytes());
    buf.extend_from_slice(&pk_max.to_le_bytes());
    for rg in rgs {
        buf.extend_from_slice(&rg.first_row.to_le_bytes());
        buf.extend_from_slice(&rg.rows.to_le_bytes());
        buf.extend_from_slice(&(rg.cols.len() as u32).to_le_bytes());
        for cm in &rg.cols {
            buf.extend_from_slice(&(cm.blocks.len() as u32).to_le_bytes());
            for b in &cm.blocks {
                buf.extend_from_slice(&b.offset.to_le_bytes());
                buf.extend_from_slice(&b.rows.to_le_bytes());
                buf.extend_from_slice(&b.null_count.to_le_bytes());
                buf.push(b.codec.as_u8());
                buf.push(b.flags);
                buf.extend_from_slice(&b.min.to_le_bytes());
                buf.extend_from_slice(&b.max.to_le_bytes());
            }
            buf.extend_from_slice(&cm.validity_offset.to_le_bytes());
            buf.extend_from_slice(&cm.validity_len.to_le_bytes());
        }
    }
    let footer_len = (buf.len() - body_start + 8) as u32;
    buf.extend_from_slice(&footer_len.to_le_bytes());
    buf.extend_from_slice(&FILE_MAGIC.to_le_bytes());
    Ok(())
}

/// 从文件字节串解析 footer（文件尾 range get 8B 即可定位，SPEC 05 §2）。
pub(crate) fn parse_footer(data: &[u8]) -> Result<CbfFooter> {
    if data.len() < 8 {
        return Err(Error::Corrupt("file shorter than tail".into()));
    }
    let tail = &data[data.len() - 8..];
    if u32::from_le_bytes([tail[4], tail[5], tail[6], tail[7]]) != FILE_MAGIC {
        return Err(Error::Corrupt("file magic mismatch".into()));
    }
    let footer_len = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as usize;
    if footer_len < 8 || footer_len > data.len() {
        return Err(Error::Corrupt(format!("footer_len {footer_len} invalid")));
    }
    let footer_offset = (data.len() - footer_len) as u64;
    let mut c: &[u8] = &data[data.len() - footer_len..data.len() - 8];
    macro_rules! need {
        ($n:expr, $what:expr) => {
            if c.len() < $n {
                return Err(Error::Corrupt(format!("footer {} truncated", $what)));
            }
        };
    }
    need!(4, "col_count");
    let col_count = take_u32(&mut c);
    let mut fields = Vec::with_capacity(col_count as usize);
    for _ in 0..col_count {
        need!(4, "name_len");
        let name_len = take_u32(&mut c) as usize;
        need!(name_len + 3, "column meta");
        let name = String::from_utf8_lossy(&c[..name_len]).into_owned();
        c = &c[name_len..];
        let tid = u16::from_le_bytes([c[0], c[1]]);
        c = &c[2..];
        let nullable = c[0] != 0;
        c = &c[1..];
        let dt = arrow_type_from_id(tid)?;
        fields.push(Field::new(name, dt, nullable));
    }
    need!(4 + 8 * 3, "file level");
    let rg_count = take_u32(&mut c) as usize;
    let total_rows = take_u64(&mut c);
    let pk_min = take_u64(&mut c);
    let pk_max = take_u64(&mut c);
    let mut rgs = Vec::with_capacity(rg_count);
    for _ in 0..rg_count {
        need!(8 + 4 + 4, "rg meta");
        let first_row = take_u64(&mut c);
        let rows = take_u32(&mut c);
        let col_count = take_u32(&mut c) as usize;
        let mut cols = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            need!(4, "block_count");
            let block_count = take_u32(&mut c) as usize;
            let mut blocks = Vec::with_capacity(block_count);
            for _ in 0..block_count {
                need!(8 + 4 + 4 + 1 + 1 + 8 + 8, "block meta");
                let offset = take_u64(&mut c);
                let rows = take_u32(&mut c);
                let null_count = take_u32(&mut c);
                let codec = CodecId::from_u8(take_u8(&mut c))?;
                let flags = take_u8(&mut c);
                let min = take_u64(&mut c);
                let max = take_u64(&mut c);
                blocks.push(BlockMeta { offset, rows, null_count, codec, flags, min, max });
            }
            need!(16, "validity meta");
            let validity_offset = take_u64(&mut c);
            let validity_len = take_u64(&mut c);
            cols.push(ChunkMeta { blocks, validity_offset, validity_len });
        }
        rgs.push(RgMeta { first_row, rows, cols });
    }
    if !c.is_empty() {
        return Err(Error::Corrupt(format!(
            "footer has {} trailing bytes",
            c.len()
        )));
    }
    Ok(CbfFooter {
        schema: Arc::new(Schema::new(fields)),
        rg_count,
        total_rows,
        pk_min,
        pk_max,
        rgs,
        footer_offset,
    })
}

fn take_u32(c: &mut &[u8]) -> u32 {
    let v = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    *c = &c[4..];
    v
}

fn take_u64(c: &mut &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&c[..8]);
    *c = &c[8..];
    u64::from_le_bytes(a)
}

fn take_u8(c: &mut &[u8]) -> u8 {
    let v = c[0];
    *c = &c[1..];
    v
}
