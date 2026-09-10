//! FSST 文本压缩（SPEC 05 §3 预留槽位 codec_id=4，VLDB'20 Boncz et al.）。
//!
//! 高基数文本（distinct_ratio ≥ 0.2，RLE_DICT 不适用）的默认 codec：符号级
//! 字节对齐压缩，无熵解码依赖（区别于 zstd），解码 ≈ 查表展开，热层可用。
//! 压缩器每次编码训练符号表（≤16KB 采样，5 轮贪心），<32KB 输入自动退化为
//! 原样拷贝（switch=0），小块无收益也无损失。
//!
//! 块数据区布局（自描述，crc32c 覆盖全部三段）：
//!
//! ```text
//! [symbol_table: 2312B 定长]        ← 符号表序列化（8B 头: MAGIC|switch|suffix_lim|terminator|n_symbols）
//! [comp_offsets: (rows+1) × u32 LE] ← 每行码流偏移（FSST 码空间）
//! [comp_bytes]                      ← FSST 码流；switch=0 时为原样拷贝
//! ```
//!
//! 实现采用 [lance 的 `fsst` crate]（参考 C 实现的纯 Rust 移植，解码端对
//! 不可信输入做过硬化：符号长度 1..=8 校验、offset 单调校验、escape 完整性）。
//! 符号表按块内联——块保持自描述单元（zone map 剪枝读单块即可解码），代价
//! 为每块 2312B（1M 行行组下 ≈0.02%，可忽略）。
//!
//! [lance 的 `fsst` crate]: https://crates.io/crates/fsst

use crate::codec::{ColumnValues, Layout};
use crate::{Error, Result};

use fsst::fsst::{compress, decompress, FSST_SYMBOL_TABLE_SIZE};

/// 符号表定长（lance fsst：8B 头 + 256×8B 符号值 + 256B 符号长度）
const SYMBOL_TABLE_LEN: usize = FSST_SYMBOL_TABLE_SIZE;

/// 编码一个变宽块（FSST 只适用 Var 布局）。返回 raw_len 口径与 [`ColumnValues::raw_len`] 一致。
pub(crate) fn encode(offsets: &[u32], bytes: &[u8], out: &mut Vec<u8>) -> Result<()> {
    // crate 的 offset 语义是 i32（arrow OffsetSizeTrait）；与 build_array 的 i32 上限一致
    if bytes.len() > i32::MAX as usize {
        return Err(Error::InvalidInput("var chunk exceeds i32 offsets".into()));
    }
    let mut sym = vec![0u8; SYMBOL_TABLE_LEN];
    let in_offs: Vec<i32> = offsets.iter().map(|&o| o as i32).collect();
    // crate 契约：调用前 out 缓冲 len ≥ 输入（内部按实际缩回）
    let mut comp = vec![0u8; bytes.len()];
    let mut comp_offs: Vec<i32> = vec![0; in_offs.len()];
    compress::<i32>(&mut sym, bytes, &in_offs, &mut comp, &mut comp_offs)
        .map_err(|e| Error::Fsst(format!("encode: {e}")))?;
    out.extend_from_slice(&sym);
    for &o in &comp_offs {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out.extend_from_slice(&comp);
    Ok(())
}

/// 解码一个变宽块。`raw_len` 来自块头（crc32c 已校验），用于解压后一致性对账。
pub(crate) fn decode(
    data: &[u8],
    raw_len: usize,
    rows: usize,
    layout: &Layout,
) -> Result<ColumnValues> {
    if *layout != Layout::Var {
        return Err(Error::CodecNotApplicable(crate::codec::CodecId::Fsst));
    }
    if data.len() < SYMBOL_TABLE_LEN {
        return Err(Error::Corrupt(
            "fsst block shorter than symbol table".into(),
        ));
    }
    let sym = &data[..SYMBOL_TABLE_LEN];
    let offs_len = (rows + 1)
        .checked_mul(4)
        .ok_or_else(|| corrupt("fsst offsets overflow"))?;
    if data.len() < SYMBOL_TABLE_LEN + offs_len {
        return Err(corrupt("fsst block shorter than offsets table"));
    }
    let offs_bytes = &data[SYMBOL_TABLE_LEN..SYMBOL_TABLE_LEN + offs_len];
    let comp = &data[SYMBOL_TABLE_LEN + offs_len..];

    let mut in_offs = Vec::with_capacity(rows + 1);
    for i in 0..=rows {
        let b = &offs_bytes[i * 4..i * 4 + 4];
        in_offs.push(u32::from_le_bytes(b.try_into().unwrap()) as i32);
    }
    // 码流偏移必须单调且终点对齐码流长度（防构造解码越界）
    if in_offs.first().copied() != Some(0) {
        return Err(corrupt("fsst first code offset != 0"));
    }
    if in_offs.windows(2).any(|w| w[0] > w[1]) || in_offs[rows] as usize != comp.len() {
        return Err(corrupt("fsst code offsets not monotonic / end mismatch"));
    }

    // crate 契约：解码前 out 缓冲 len ≥ 8×码流（1B 码最多展开 8B 符号）；按实际缩回
    let cap = comp.len().saturating_mul(8).max(raw_len);
    let mut out_bytes = vec![0u8; cap];
    let mut out_offs: Vec<i32> = vec![0; rows + 1];
    decompress::<i32>(sym, comp, &in_offs, &mut out_bytes, &mut out_offs)
        .map_err(|e| Error::Corrupt(format!("fsst decode: {e}")))?;

    // 与块头 raw_len 对账：(rows+1)×4 + 解压字节
    if (rows + 1) * 4 + out_bytes.len() != raw_len {
        return Err(corrupt("fsst decoded size != block raw_len"));
    }
    let offsets = out_offs.into_iter().map(|o| o as u32).collect();
    Ok(ColumnValues::Var {
        offsets,
        bytes: out_bytes,
    })
}

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(format!("fsst: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{chunk_of, decode_chunk, encode_chunk, CodecId};
    use arrow::array::{ArrayRef, StringArray};
    use std::sync::Arc;

    fn roundtrip(strings: Vec<Option<&str>>) -> Vec<Option<String>> {
        let arr: ArrayRef = Arc::new(StringArray::from(strings));
        let part = chunk_of(&arr).unwrap();
        let mut data = Vec::new();
        let raw_len = encode_chunk(CodecId::Fsst, &part, &mut data).unwrap();
        let layout = crate::codec::layout_of(arr.data_type()).unwrap();
        let back =
            decode_chunk(CodecId::Fsst, &data, raw_len as usize, part.rows(), &layout).unwrap();
        let (offsets, bytes) = match &back {
            ColumnValues::Var { offsets, bytes } => (offsets, bytes),
            other => panic!("expected Var, got {other:?}"),
        };
        (0..back.rows())
            .map(|i| {
                if !part.is_valid(i) {
                    return None;
                }
                Some(
                    String::from_utf8(bytes[offsets[i] as usize..offsets[i + 1] as usize].to_vec())
                        .unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn fsst_roundtrip_natural_text() {
        // 自然文本（重复词表）——FSST 的主场
        let words = [
            "SELECT",
            "FROM",
            "WHERE",
            "INSERT",
            "UPDATE",
            "user@example.com",
        ];
        let strings: Vec<Option<&str>> = (0..5000).map(|i| Some(words[i % words.len()])).collect();
        let expect: Vec<Option<String>> = (0..5000)
            .map(|i| Some(words[i % words.len()].to_string()))
            .collect();
        assert_eq!(roundtrip(strings), expect);
    }

    #[test]
    fn fsst_roundtrip_edge_cases() {
        let long = "y".repeat(1000);
        let strings = vec![
            Some(""),
            Some("a"),
            Some("hello world"),
            Some("héllo wörld — multibyte ünïcode ✓"),
            Some("\u{0}\u{1}控制字符\t\n"),
            Some("xxxxxxxx"),    // 恰好最大符号长
            Some(long.as_str()), // 超长串
            None,                // null 槽位（值已归零 = 空串）
            Some(""),            // 尾部空串
        ];
        let got = roundtrip(strings.clone());
        // null 槽位值域归零（空串），validity 位图单独保存——值域层面全等
        for (i, s) in strings.iter().enumerate() {
            if let Some(s) = s {
                assert_eq!(got[i], Some(s.to_string()), "row {i}");
            }
        }
    }

    #[test]
    fn fsst_roundtrip_all_null_and_empty() {
        assert!(roundtrip(vec![None, None]).iter().all(|s| s.is_none()));
        assert!(roundtrip(vec![Some(""), Some("")])
            .iter()
            .all(|s| s == &Some(String::new())));
    }

    #[test]
    fn fsst_compresses_natural_text() {
        // 实际压缩收益（switch-on 路径，>32KB）
        let text = "The quick brown fox jumps over the lazy dog. ";
        let strings: Vec<Option<&str>> = std::iter::repeat_n(Some(text), 20_000).collect();
        let arr: ArrayRef = Arc::new(StringArray::from(strings));
        let part = chunk_of(&arr).unwrap();
        let mut data = Vec::new();
        let _ = encode_chunk(CodecId::Fsst, &part, &mut data).unwrap();
        let raw = part.vals.raw_len() as usize;
        assert!(
            data.len() * 3 < raw * 2,
            "fsst should beat 2/3 of raw on repeated text: {} vs {raw}",
            data.len()
        );
    }

    #[test]
    fn fsst_decode_rejects_corrupt_offsets() {
        let owned: Vec<String> = (0..40_000).map(|i| format!("value-{i}-payload")).collect();
        let strings: Vec<Option<&str>> = owned.iter().map(|s| Some(s.as_str())).collect();
        let arr: ArrayRef = Arc::new(StringArray::from(strings));
        let part = chunk_of(&arr).unwrap();
        let mut data = Vec::new();
        let raw_len = encode_chunk(CodecId::Fsst, &part, &mut data).unwrap();
        let layout = crate::codec::layout_of(arr.data_type()).unwrap();

        // 篡改符号表 magic
        let mut bad = data.clone();
        bad[0] ^= 0xFF;
        assert!(decode_chunk(CodecId::Fsst, &bad, raw_len as usize, part.rows(), &layout).is_err());

        // 截断 offsets 表
        let mut bad = data.clone();
        bad.truncate(SYMBOL_TABLE_LEN + 4);
        assert!(decode_chunk(CodecId::Fsst, &bad, raw_len as usize, part.rows(), &layout).is_err());

        // raw_len 对不上
        assert!(decode_chunk(
            CodecId::Fsst,
            &data,
            raw_len as usize + 7,
            part.rows(),
            &layout
        )
        .is_err());
    }

    #[test]
    fn fsst_rejects_fixed_layout() {
        use arrow::array::Int64Array;
        let arr: ArrayRef = Arc::new(Int64Array::from(vec![1i64, 2, 3]));
        let part = chunk_of(&arr).unwrap();
        let mut data = Vec::new();
        assert!(encode_chunk(CodecId::Fsst, &part, &mut data).is_err());
        // 解码端同样拒绝
        let layout = crate::codec::layout_of(arr.data_type()).unwrap();
        assert!(decode(&[0u8; SYMBOL_TABLE_LEN + 4], 0, 0, &layout).is_err());
    }
}
