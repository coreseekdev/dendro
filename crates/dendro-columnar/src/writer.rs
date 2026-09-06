//! CBF 写入器（SPEC 05 §2 文件布局、§3 块格式、§4 编码器选择策略）。

use crate::codec::{chunk_of, col_type_of, encode_chunk, layout_of, merge_chunks, CodecId, Layout};
use crate::footer::{pad_align, write_block_header, write_footer, BlockHeader, ChunkMeta, RgMeta};
use crate::stats::{order_minmax, ColStats};
use crate::{Error, Result, ALIGN, BLOCK_HEADER_LEN, SAMPLE_ROWS, ZSTD_LEVEL};
use arrow::array::{ArrayRef, RecordBatch};
use arrow::datatypes::SchemaRef;
use dendro_core::types::ColType;

pub(crate) fn write_cbf(
    batches: &[RecordBatch],
    row_group_rows: usize,
    codec_choice: Option<&dyn Fn(&str, ColType, &ColStats) -> CodecId>,
) -> Result<Vec<u8>> {
    let first = batches
        .first()
        .ok_or_else(|| Error::InvalidInput("no batches given".into()))?;
    // 0 = 使用默认行组行数（SPEC 05 §2：1,048,576，tonbo 同款）
    let rg_rows = if row_group_rows == 0 {
        crate::DEFAULT_ROW_GROUP_ROWS
    } else {
        row_group_rows
    };
    let schema = first.schema();
    for (i, b) in batches.iter().enumerate() {
        if b.schema() != schema {
            return Err(Error::InvalidInput(format!("batch {i} schema mismatch")));
        }
    }
    let ncols = schema.fields().len();
    if ncols == 0 {
        return Err(Error::InvalidInput("schema has no columns".into()));
    }
    let ctypes: Vec<ColType> = schema
        .fields()
        .iter()
        .map(|f| col_type_of(f.data_type()))
        .collect::<Result<_>>()?;
    let layouts: Vec<Layout> = schema
        .fields()
        .iter()
        .map(|f| layout_of(f.data_type()))
        .collect::<Result<_>>()?;

    // ---- codec 决策：首行组/前 64K 行采样（SPEC 05 §4），决策对全文件该列生效 ----
    let codecs = decide_codecs(batches, &schema, &ctypes, &layouts, codec_choice)?;

    // ---- 行组切分（SPEC 05 §2：RG 依 (checkpoint, pk) 有序 ⇒ 主键范围列式有序）----
    let rgs = split_row_groups(batches, ncols, rg_rows)?;

    // ---- 逐 RG 逐列编码 ----
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut rg_metas: Vec<RgMeta> = Vec::with_capacity(rgs.len());
    let mut total_rows = 0u64;
    let mut pk_min = u64::MAX;
    let mut pk_max = 0u64;
    for rg in &rgs {
        let mut col_metas = Vec::with_capacity(ncols);
        for c in 0..ncols {
            let parts = rg.cols[c]
                .iter()
                .map(chunk_of)
                .collect::<Result<Vec<_>>>()?;
            let part = merge_chunks(parts)?;
            let cm = encode_block(&mut buf, &part, &layouts[c], codecs[c])?;
            if c == 0 {
                for b in &cm.blocks {
                    pk_min = pk_min.min(b.min);
                    pk_max = pk_max.max(b.max);
                }
            }
            col_metas.push(cm);
        }
        rg_metas.push(RgMeta {
            first_row: total_rows,
            rows: rg.rows as u32,
            cols: col_metas,
        });
        total_rows += rg.rows as u64;
    }
    if rgs.is_empty() {
        pk_min = 0;
        pk_max = 0;
    }

    write_footer(&mut buf, &schema, &rg_metas, total_rows, pk_min, pk_max)?;
    tracing::debug!(
        row_groups = rg_metas.len(),
        columns = ncols,
        total_rows,
        bytes = buf.len(),
        "write_cbf done"
    );
    Ok(buf)
}

/// 每 (RG, 列) 编码为一个块（v1 单块/chunk；footer 保留 blocks[] 结构）：
/// [64B 块头][data 区][pad→64B][validity 位图区][pad→64B]，全部 64B 对齐（SPEC 05 §2/§3）。
fn encode_block(
    buf: &mut Vec<u8>,
    part: &crate::codec::ChunkPart,
    layout: &Layout,
    codec: CodecId,
) -> Result<ChunkMeta> {
    pad_align(buf);
    let hdr_off = buf.len() as u64;
    buf.resize(buf.len() + BLOCK_HEADER_LEN, 0); // 预留块头

    let mut data = Vec::new();
    let raw_len = encode_chunk(codec, part, &mut data)?;
    let crc = crc32c::crc32c(&data);
    let (min, max, sorted) = order_minmax(part, layout);
    let rows = part.rows() as u32;
    let null_count = part.null_count() as u32;

    buf.extend_from_slice(&data);

    // validity 位图独立区（64B 对齐、LSB-first、Arrow 同构）；flags.bit0=1 时省略
    let (validity_offset, validity_len, all_non_null) = match &part.validity {
        Some(vb) => {
            pad_align(buf);
            let off = buf.len() as u64;
            buf.extend_from_slice(vb);
            (off, vb.len() as u64, false)
        }
        None => (0u64, 0u64, true),
    };

    let mut flags = 0u8;
    if all_non_null {
        flags |= 1;
    }
    if sorted {
        flags |= 2; // 排序检测：前缀有序（SPEC 05 §4）
    }
    if codec == CodecId::Zstd {
        flags |= (ZSTD_LEVEL as u8) << 2; // 级别存 flags 高位（SPEC 05 §3）
    }
    let h = BlockHeader {
        codec: codec.as_u8(),
        flags,
        rows,
        null_count,
        data_len: data.len() as u64,
        raw_len,
        min,
        max,
        crc,
    };
    let mut hb = [0u8; BLOCK_HEADER_LEN];
    write_block_header(&mut hb, &h);
    buf[hdr_off as usize..hdr_off as usize + BLOCK_HEADER_LEN].copy_from_slice(&hb);

    Ok(ChunkMeta {
        blocks: vec![crate::footer::BlockMeta {
            offset: hdr_off,
            rows,
            null_count,
            codec,
            flags,
            min,
            max,
        }],
        validity_offset,
        validity_len,
    })
}

fn decide_codecs(
    batches: &[RecordBatch],
    schema: &SchemaRef,
    ctypes: &[ColType],
    layouts: &[Layout],
    codec_choice: Option<&dyn Fn(&str, ColType, &ColStats) -> CodecId>,
) -> Result<Vec<CodecId>> {
    let ncols = schema.fields().len();
    let mut sample_cols: Vec<Vec<ArrayRef>> = (0..ncols).map(|_| Vec::new()).collect();
    let mut sampled = 0usize;
    'outer: for b in batches {
        let mut off = 0usize;
        while off < b.num_rows() {
            let take = (SAMPLE_ROWS - sampled).min(b.num_rows() - off);
            for c in 0..ncols {
                sample_cols[c].push(b.column(c).slice(off, take).clone());
            }
            sampled += take;
            off += take;
            if sampled >= SAMPLE_ROWS {
                break 'outer;
            }
        }
    }
    (0..ncols)
        .map(|c| {
            let parts = sample_cols[c]
                .iter()
                .map(chunk_of)
                .collect::<Result<Vec<_>>>()?;
            let part = merge_chunks(parts)?;
            let stats = ColStats::from_chunk(&part, &layouts[c]);
            let name = schema.field(c).name();
            Ok(match codec_choice {
                Some(f) => f(name, ctypes[c], &stats),
                None => crate::choose_codec(name, ctypes[c], &stats),
            })
        })
        .collect()
}

struct RgBuild {
    rows: usize,
    cols: Vec<Vec<ArrayRef>>,
}

fn split_row_groups(batches: &[RecordBatch], ncols: usize, rg_rows: usize) -> Result<Vec<RgBuild>> {
    let mut rgs: Vec<RgBuild> = Vec::new();
    for b in batches {
        let mut off = 0usize;
        while off < b.num_rows() {
            if rgs.last().map(|r| r.rows == rg_rows).unwrap_or(true) {
                rgs.push(RgBuild {
                    rows: 0,
                    cols: (0..ncols).map(|_| Vec::new()).collect(),
                });
            }
            let rg = rgs.last_mut().unwrap();
            let take = (rg_rows - rg.rows).min(b.num_rows() - off);
            for c in 0..ncols {
                rg.cols[c].push(b.column(c).slice(off, take).clone());
            }
            rg.rows += take;
            off += take;
        }
    }
    // 空 RG（全输入 0 行）不产出
    rgs.retain(|r| r.rows > 0);
    let _ = ALIGN; // 对齐由 encode_block/pad_align 保证
    Ok(rgs)
}
