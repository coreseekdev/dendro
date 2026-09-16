//! CBF 读取器：整文件读、footer 剪枝、按需 (RG, 列) chunk 解码
//! （SPEC 05 §2 footer 单次 range GET、§7 剪枝管线）。

use crate::codec::{build_array, decode_chunk, layout_of, merge_chunks, ChunkPart, Layout};
use crate::footer::{parse_block_header, parse_footer, CbfFooter};
use crate::{Error, Result, BLOCK_HEADER_LEN};
use arrow::array::{ArrayRef, RecordBatch};
use dendro_core::objstore::ObjStore;
use std::sync::Arc;
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
    tracing::debug!(row_groups = out.len(), bytes = data.len(), "read_cbf done");
    Ok((footer.schema, out))
}

/// 段的稀疏 footer 抓取（head 定长 → 尾 8B → footer 体；列统计/
/// 掩码剪枝共用——O-3+ 稀疏读路径）
pub fn footer_sparse(
    obj: &Arc<dyn ObjStore>,
    path: &str,
) -> Result<CbfFooter> {
    let len = obj
        .head(path)
        .map_err(|e| crate::Error::InvalidInput(format!("head: {e}")))?
        .ok_or_else(|| crate::Error::InvalidInput("segment missing".into()))?
        .len;
    let tail = obj
        .get_range(path, len - 8, 8)
        .map_err(|e| crate::Error::InvalidInput(format!("tail: {e}")))?;
    let flen = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as u64;
    let fbody = obj
        .get_range(path, len - flen, (flen - 8) as usize)
        .map_err(|e| crate::Error::InvalidInput(format!("footer: {e}")))?;
    crate::footer::parse_footer_from(&tail, &fbody)
}

/// 字节源（整文件切片 / ObjStore get_range 稀疏取数）
pub(crate) type Fetch<'a> = &'a dyn Fn(u64, usize) -> Result<Vec<u8>>;

/// 解码指定行组的指定列（zone map 剪枝后按需 range 读取入口）。
pub(crate) fn read_column_chunk(
    data: &[u8],
    footer: &CbfFooter,
    rg: usize,
    col: usize,
) -> Result<ArrayRef> {
    let fetch = |off: u64, len: usize| -> Result<Vec<u8>> {
        let s = (off as usize).min(data.len());
        let e = (s + len).min(data.len());
        Ok(data[s..e].to_vec())
    };
    read_column_chunk_fetch(&fetch, footer, rg, col)
}

/// 稀疏读入口：字节经 fetch 取（块头/块数据/validity 各自 get_range）
pub(crate) fn read_column_chunk_fetch(
    fetch: Fetch<'_>,
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
        parts.push(decode_block(fetch, b, cm, &layout)?);
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
    fetch: Fetch<'_>,
    meta: &crate::footer::BlockMeta,
    chunk: &crate::footer::ChunkMeta,
    layout: &Layout,
) -> Result<ChunkPart> {
    let head = fetch(meta.offset, BLOCK_HEADER_LEN)?;
    let h = parse_block_header(&head)?;
    // footer 与块头一致性（防指针漂移/半写）
    if h.codec != meta.codec.as_u8() || h.rows != meta.rows || h.null_count != meta.null_count {
        return Err(Error::Corrupt("footer/header metadata mismatch".into()));
    }
    let payload = fetch(meta.offset + BLOCK_HEADER_LEN as u64, h.data_len as usize)?;
    if crc32c::crc32c(&payload) != h.crc {
        return Err(Error::Corrupt("block crc32c mismatch".into()));
    }
    let vals = decode_chunk(
        meta.codec,
        &payload,
        h.raw_len as usize,
        meta.rows as usize,
        layout,
    )?;

    // validity 位图独立区（LSB-first，Arrow 同构）
    let validity = if chunk.validity_len > 0 {
        if chunk.validity_offset == 0 {
            return Err(Error::Corrupt("validity offset 0 with len > 0".into()));
        }
        let vb = fetch(chunk.validity_offset, chunk.validity_len as usize)?;
        // validity 位图 1=valid；padding 位为 0
        let set = vb.iter().map(|b| b.count_ones()).sum::<u32>();
        if meta.rows.saturating_sub(set) != meta.null_count {
            return Err(Error::Corrupt("validity popcount != null_count".into()));
        }
        Some(vb)
    } else {
        if meta.null_count != 0 {
            return Err(Error::Corrupt(
                "null_count > 0 but no validity bitmap".into(),
            ));
        }
        None
    };
    Ok(ChunkPart { vals, validity })
}
