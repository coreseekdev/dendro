
//! M-4：**第二实现解码对拍**——GPU 内核等价性的前置证明。
//!
//! 块数据区的解码按 SPEC 05 §3 的 codec 语义（Raw 8B LE / Delta 首值 +
//! zigzag varint 差分 / BitPack 定宽位流）由本文件的 `ref_decode_block`
//! 独立实现，不经 `dendro_columnar::codec` 的 Rust 编解码栈；
//! 块文件偏移来自官方 `read_footer`（容器解析不在对拍范围）。
//! 断言：独立解码输出与写入源值位级一致。

use arrow::array::{Int64Array, RecordBatch};
use dendro_columnar::{read_cbf, read_footer, write_cbf, CodecId};
use std::sync::Arc;

/// **独立第二实现**：按 codec 语义解码一个块的数据区 → i64 序列。
/// 只依赖块头字段偏移与 codec 位流规范（SPEC 05 §3），零 dendro 依赖。
fn ref_decode_block(codec: CodecId, data: &[u8], rows: usize) -> Vec<i64> {
    match codec {
        CodecId::Raw => (0..rows)
            .map(|i| i64::from_le_bytes(data[i * 8..(i + 1) * 8].try_into().unwrap()))
            .collect(),
        CodecId::Delta => {
            // 首值 8B LE + zigzag LEB128 varint 差分
            let first = i64::from_le_bytes(data[0..8].try_into().unwrap());
            let mut out = vec![first];
            let mut cur = first;
            let mut off = 8usize;
            for _ in 1..rows {
                let mut shift = 0u32;
                let mut acc: u64 = 0;
                loop {
                    let b = data[off];
                    off += 1;
                    acc |= ((b & 0x7f) as u64) << shift;
                    shift += 7;
                    if b & 0x80 == 0 {
                        break;
                    }
                }
                cur += ((acc >> 1) as i64) ^ -((acc & 1) as i64);
                out.push(cur);
            }
            out
        }
        CodecId::BitPack => {
            // LSB-first 位流：`bit_width u8` + 连续位流（值 i 的位 b 在
            // 流偏移 i*width+b）。值已无符号化（order 域），需转回 i64
            let w = data[0] as usize;
            let _mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            (0..rows)
                .map(|i| {
                    let mut v: u64 = 0;
                    for b in 0..w {
                        let p = i * w + b;
                        if (data[1 + (p >> 3)] >> (p & 7)) & 1 == 1 {
                            v |= 1 << b;
                        }
                    }
                    // Int64 写入是 `v as u64` 直转 ⇒ 解码即 `u64 as i64` 回转
                    v as i64
                })
                .collect()
        }
        other => panic!("ref decoder 未覆盖 {other:?}"),
    }
}

fn make_batch(n: i64) -> RecordBatch {
    let vals: Vec<i64> = (1..=n).map(|i| i * 3 - n / 2).collect();
    let arr = Int64Array::from(vals);
    RecordBatch::try_from_iter(vec![("v", Arc::new(arr) as _)]).unwrap()
}

/// 对单个 codec 做"写 → footer 定位块 → 独立解码 → 源值对拍"
fn check_codec(codec: CodecId, n: i64) {
    use dendro_core::types::ColType;
    use dendro_columnar::ColStats;
    let batch = make_batch(n);
    let choice = |_: &str, _: ColType, _: &ColStats| -> CodecId { codec };
    let bytes = write_cbf(std::slice::from_ref(&batch), 4096, Some(&choice)).unwrap();

    let footer = read_footer(&bytes).unwrap();
    assert!(footer.rg_count >= 1);
    let mut ref_decoded: Vec<i64> = Vec::new();
    for rg in &footer.rgs {
        for chunk in &rg.cols {
            for bm in &chunk.blocks {
                assert_eq!(bm.codec, codec, "块 codec 应与声明一致");
                // 块头 64B 内 data_len（偏移 14..22）给出本块数据区实际长度
                let hdr_off = bm.offset as usize;
                let data_len = u64::from_le_bytes(
                    bytes[hdr_off + 14..hdr_off + 22].try_into().unwrap(),
                ) as usize;
                let data_start = hdr_off + 64;
                let data = &bytes[data_start..data_start + data_len];
                ref_decoded.extend(ref_decode_block(codec, data, bm.rows as usize));
            }
        }
    }
    let expected: Vec<i64> = (1..=n).map(|i| i * 3 - n / 2).collect();
    // make_batch 用同样的公式生成源值
    assert_eq!(ref_decoded.len(), expected.len());
    for (i, got) in ref_decoded.iter().enumerate() {
        let src_val = (i as i64 + 1) * 3 - n / 2;
        assert_eq!(*got, src_val, "独立解码值不符");
    }
}

#[test]
fn m4_ref_decode_raw_bit_exact() {
    check_codec(CodecId::Raw, 100);
}

#[test]
fn m4_ref_decode_delta_bit_exact() {
    check_codec(CodecId::Delta, 100);
}

#[test]
fn m4_ref_decode_bitpack_bit_exact() {
    check_codec(CodecId::BitPack, 100);
}

#[test]
fn m4_official_reader_agrees_with_source() {
    // 官方 reader（Rust 编解码栈）输出也与源值一致——三方闭环：
    // 源值 == 独立实现 == 官方 reader
    for codec in [CodecId::Raw, CodecId::Delta, CodecId::BitPack] {
        let batch = make_batch(50);
        let choice = |_: &str, _: dendro_core::types::ColType, _: &dendro_columnar::ColStats| -> CodecId { codec };
        let bytes = write_cbf(&[batch], 4096, Some(&choice)).unwrap();
        let (_schema, batches) = read_cbf(&bytes).unwrap();
        let got: Vec<i64> = batches
            .iter()
            .flat_map(|b| {
                b.column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .values()
                    .iter()
                    .copied()
            })
            .collect();
        let expected: Vec<i64> = (1..=50).map(|i| i * 3 - 25).collect();
        assert_eq!(got, expected, "{codec:?}");
    }
}
