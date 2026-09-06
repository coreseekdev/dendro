//! CBF 读取器：整文件读、footer 剪枝、按需 (RG, 列) chunk 解码
//! （SPEC 05 §2 footer 单次 range GET、§7 剪枝管线）。

use crate::codec::{build_array, decode_chunk, layout_of, merge_chunks, ChunkPart, Layout};
use crate::footer::{parse_block_header, parse_footer, CbfFooter};
use crate::{Error, Result, BLOCK_HEADER_LEN};
use arrow::array::{ArrayRef, RecordBatch};
use arrow::datatypes::SchemaRef;

pub(crate) fn read_footer(data: &[u8]) -> Result<CbfFooter> {
    parse_footer(data)
}

/// v1 整文件解码：footer 校验 → 逐 RG 逐列 chunk 解码 → RecordBatch。
pub(crate) fn read_cbf(data: &[u8]) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    let footer = parse_footer(data)?;
    let ncols = footer.schema.fields().len();
    let mut out = Vec::with_capacity(footer.rg_count);
    for ri in 0..footer.rg_count {
        let mut cols = Vec::with_capacity(ncols);
        for ci in 0..ncols {
            cols.push(read_column_chunk(data, &footer, ri, ci)?);
        }
        out.push(RecordBatch::try_new(footer.schema.clone(), cols)?);
    }
    tracing::debug!(
        row_groups = out.len(),
        bytes = data.len(),
        "read_cbf done"
    );
    Ok((footer.schema, out))
}

/// 解码指定行组的指定列（zone map 剪枝后按需 range 读取入口）。
pub(crate) fn read_column_chunk(
    data: &[u8],
    footer: &CbfFooter,
    rg: usize,
    col: usize,
) -> Result<ArrayRef> {
    let rm = footer
        .rgs
        .get(rg)
        .ok_or_else(|| Error::InvalidInput(format!("row group {rg} out of range")))?;
    let cm = rm
        .cols
        .get(col)
        .ok_or_else(|| Error::InvalidInput(format!("column {col} out of range")))?;
    let dt = footer.schema.field(col).data_type().clone();
    let layout = layout_of(&dt)?;
    let mut parts = Vec::with_capacity(cm.blocks.len());
    for b in &cm.blocks {
        parts.push(decode_block(data, b, cm, &layout)?);
    }
    let part = merge_chunks(parts)?;
    if part.rows() != rm.rows as usize {
        return Err(Error::Corrupt(format!(
            "chunk rows {} != rg rows {}",
            part.rows(),
            rm.rows
        )));
    }
    build_array(part, &dt)
}

fn decode_block(
    data: &[u8],
    meta: &crate::footer::BlockMeta,
    chunk: &crate::footer::ChunkMeta,
    layout: &Layout,
) -> Result<ChunkPart> {
    let off = meta.offset as usize;
    if off.saturating_add(BLOCK_HEADER_LEN) > data.len() {
        return Err(Error::Corrupt("block header out of file".into()));
    }
    let h = parse_block_header(&data[off..off + BLOCK_HEADER_LEN])?;
    // footer 与块头一致性（防指针漂移/半写）
    if h.codec != meta.codec.as_u8() || h.rows != meta.rows || h.null_count != meta.null_count {
        return Err(Error::Corrupt("footer/header metadata mismatch".into()));
    }
    let d0 = off + BLOCK_HEADER_LEN;
    let d1 = d0 + h.data_len as usize;
    if d1 > data.len() {
        return Err(Error::Corrupt("block data out of file".into()));
    }
    let payload = &data[d0..d1];
    if crc32c::crc32c(payload) != h.crc {
        return Err(Error::Corrupt("block crc32c mismatch".into()));
    }
    let vals = decode_chunk(meta.codec, payload, h.raw_len as usize, meta.rows as usize, layout)?;

    // validity 位图独立区（LSB-first，Arrow 同构）
    let validity = if chunk.validity_len > 0 {
        if chunk.validity_offset == 0 {
            return Err(Error::Corrupt("validity offset 0 with len > 0".into()));
        }
        let v0 = chunk.validity_offset as usize;
        let v1 = v0 + chunk.validity_len as usize;
        if v1 > data.len() {
            return Err(Error::Corrupt("validity region out of file".into()));
        }
        let vb = &data[v0..v1];
        // validity 位图 1=valid；padding 位为 0
        let set = vb.iter().map(|b| b.count_ones()).sum::<u32>();
        if meta.rows.saturating_sub(set) != meta.null_count {
            return Err(Error::Corrupt("validity popcount != null_count".into()));
        }
        Some(vb.to_vec())
    } else {
        if meta.null_count != 0 {
            return Err(Error::Corrupt("null_count > 0 but no validity bitmap".into()));
        }
        None
    };
    Ok(ChunkPart { vals, validity })
}
