//! 测试 2（footer/剪枝正确性）+ 测试 3（stats 记账）+ 测试 5（64B 对齐断言）。

mod common;

use common::{assert_array_eq, Rng};
use dendro_columnar::{read_cbf, read_column_chunk, read_footer, write_cbf, CodecId, ColStats};
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};

/// 3 行组带主键范围：[0,1000) [1000,2000) [2000,3000)，第 1 列为低基数 utf8（10% null）
fn build_file() -> (Vec<u8>, ArrayRef, ArrayRef) {
    let mut rng = Rng::new(42);
    let n = 3000usize;
    let pk: Vec<i64> = (0..n as i64).collect();
    let texts: Vec<Option<String>> = (0..n)
        .map(|i| {
            if rng.f01() < 0.1 {
                None
            } else {
                Some(format!("cat_{}", i % 7))
            }
        })
        .collect();
    let pk_col: ArrayRef = Arc::new(Int64Array::from(pk));
    let txt_col: ArrayRef = Arc::new(StringArray::from(texts));
    let schema = Arc::new(Schema::new(vec![
        Field::new("pk", DataType::Int64, false),
        Field::new("txt", DataType::Utf8, true),
    ]));
    let mut batches = Vec::new();
    for r in [0usize, 1000, 2000] {
        batches.push(
            arrow::array::RecordBatch::try_new(
                schema.clone(),
                vec![pk_col.slice(r, 1000), txt_col.slice(r, 1000)],
            )
            .unwrap(),
        );
    }
    let bytes = write_cbf(
        &batches,
        1000,
        Some(&|name, _ty, _s| {
            if name == "pk" {
                CodecId::Delta // pk 顺序列（SPEC 08 §5 发现 3）
            } else {
                CodecId::RleDict // 低基数文本（SPEC 05 §4 distinct<0.2）
            }
        }),
    )
    .unwrap();
    (bytes, pk_col, txt_col)
}

#[test]
fn footer_metadata_and_pruning() {
    let (bytes, pk_col, txt_col) = build_file();
    let f = read_footer(&bytes).unwrap();

    // 文件级：rg_count / 总行数 / pk row range（order 域 = ColStats 口径）
    assert_eq!(f.rg_count, 3);
    assert_eq!(f.total_rows, 3000);
    let pk_stats = ColStats::of(&pk_col).unwrap();
    assert_eq!(f.pk_min, pk_stats.min);
    assert_eq!(f.pk_max, pk_stats.max);
    // 顺序 pk 的 order 域 key 可直接比较大小（符号位翻转保序）
    assert!(f.pk_min < f.pk_max);

    // RG 级 min/max（zone map 剪枝依据，SPEC 05 §7）
    for (ri, rg) in f.rgs.iter().enumerate() {
        assert_eq!(rg.rows, 1000);
        assert_eq!(rg.first_row, (ri * 1000) as u64);
        let b = &rg.cols[0].blocks[0];
        let lo = ri as i64 * 1000;
        let hi = lo + 999;
        assert_eq!(b.min, ((lo as u64) ^ (1 << 63)), "rg{ri} pk min");
        assert_eq!(b.max, ((hi as u64) ^ (1 << 63)), "rg{ri} pk max");
        assert_eq!(b.codec, CodecId::Delta);
        assert_eq!(b.null_count, 0);
        assert_eq!(b.flags & 0b1, 0b1, "pk all_non_null");
        assert_eq!(b.flags & 0b10, 0b10, "pk sorted");
        // 行组主键范围互不重叠 ⇒ 剪枝谓词可跳过
        if ri > 0 {
            let prev = &f.rgs[ri - 1].cols[0].blocks[0];
            assert!(prev.max < b.min, "rg 范围应有序不重叠");
        }
    }

    // txt 列：null 记账 + RLE_DICT
    let txt_stats = ColStats::of(&txt_col).unwrap();
    let sum_nulls: u32 = f.rgs.iter().map(|r| r.cols[1].blocks[0].null_count).sum();
    assert_eq!(sum_nulls as usize, txt_stats.null_count);
    assert!(sum_nulls > 0 && sum_nulls < 3000);
    assert_eq!(f.rgs[0].cols[1].blocks[0].codec, CodecId::RleDict);

    // column_stats 摘要接口（剪枝调用方视角）
    let cs = f.column_stats(0);
    assert_eq!(cs.len(), 3);
    assert_eq!(
        cs[1],
        (
            1,
            1000,
            f.rgs[1].cols[0].blocks[0].min,
            f.rgs[1].cols[0].blocks[0].max,
            0,
            CodecId::Delta
        )
    );

    // 按需单行组单列解码（read_column_chunk）：只动 rg1
    let rg1_pk = read_column_chunk(&bytes, &f, 1, 0).unwrap();
    let expect_pk = pk_col.slice(1000, 1000);
    assert_array_eq(expect_pk.as_ref(), rg1_pk.as_ref(), "rg1 pk");
    let rg1_txt = read_column_chunk(&bytes, &f, 1, 1).unwrap();
    let expect_txt = txt_col.slice(1000, 1000);
    assert_array_eq(expect_txt.as_ref(), rg1_txt.as_ref(), "rg1 txt");

    // 全文件读回一致
    let (_s, batches) = read_cbf(&bytes).unwrap();
    assert_eq!(batches.len(), 3);
    assert_array_eq(
        pk_col.slice(2000, 1000).as_ref(),
        batches[2].column(0).as_ref(),
        "rg2 pk full-read",
    );
    assert_array_eq(
        txt_col.slice(0, 1000).as_ref(),
        batches[0].column(1).as_ref(),
        "rg0 txt full-read",
    );
}

/// 测试 3：ColStats 与 footer 的 rows/null_count 记账（含全空列、全非空列）
#[test]
fn stats_accounting() {
    use arrow::array::Int32Array;
    let all_valid: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5]));
    let st = ColStats::of(&all_valid).unwrap();
    assert_eq!((st.rows, st.null_count, st.distinct), (5, 0, 5));
    assert!(st.monotonic);

    let with_nulls: ArrayRef = Arc::new(Int32Array::from(vec![
        Some(7),
        None,
        Some(7),
        None,
        Some(3),
        Some(7),
    ]));
    let st = ColStats::of(&with_nulls).unwrap();
    assert_eq!((st.rows, st.null_count, st.distinct), (6, 2, 2));
    // distinct_ratio 用非空行数作分母
    assert!((st.distinct_ratio() - 2.0 / 4.0).abs() < 1e-12);
    assert!(!st.monotonic); // 7,7,3 非递减？7→3 递减 ⇒ false

    let all_null: ArrayRef = Arc::new(Int32Array::from(vec![None::<i32>; 4]));
    let st = ColStats::of(&all_null).unwrap();
    assert_eq!((st.rows, st.null_count, st.distinct), (4, 4, 0));
    assert_eq!(st.distinct_ratio(), 0.0);

    // footer 记账与 ColStats 一致
    let schema = Arc::new(Schema::new(vec![Field::new("c", DataType::Int32, true)]));
    let batch = arrow::array::RecordBatch::try_new(schema, vec![with_nulls.clone()]).unwrap();
    let bytes = write_cbf(&[batch], 4, None).unwrap();
    let f = read_footer(&bytes).unwrap();
    assert_eq!(f.rgs.len(), 2); // 6 行 / rg_rows=4 ⇒ [4,2]
    assert_eq!(f.rgs[0].rows, 4);
    assert_eq!(f.rgs[1].rows, 2);
    let nulls: usize = f
        .rgs
        .iter()
        .map(|r| r.cols[0].blocks[0].null_count as usize)
        .sum();
    assert_eq!(nulls, 2);
}

/// 测试 5：块头 / 数据区 / validity 区 / footer 全部 64B 对齐（SPEC 05 §2/§3）
#[test]
fn alignment_64b() {
    let (bytes, _pk, _txt) = build_file();
    let f = read_footer(&bytes).unwrap();
    assert_eq!(
        (f.footer_offset as usize) % dendro_columnar::ALIGN,
        0,
        "footer 起点 64B 对齐"
    );
    for rg in &f.rgs {
        for cm in &rg.cols {
            for b in &cm.blocks {
                assert_eq!((b.offset as usize) % 64, 0, "block header offset 64B 对齐");
                // 数据区紧跟 64B 块头 ⇒ 同样对齐
                let data_off = b.offset as usize + dendro_columnar::BLOCK_HEADER_LEN;
                assert_eq!(data_off % 64, 0, "data 区 offset 64B 对齐");
            }
            if cm.validity_len > 0 {
                assert_eq!(
                    (cm.validity_offset as usize) % 64,
                    0,
                    "validity 区 64B 对齐"
                );
                assert!(
                    cm.validity_offset > cm.blocks[0].offset,
                    "validity 在数据之后"
                );
            }
        }
    }
    // 文件尾 8B：footer_len + magic（read_footer 成功即隐含校验；显式断言魔数）
    assert_eq!(
        &bytes[bytes.len() - 4..],
        &dendro_columnar::footer::FILE_MAGIC.to_le_bytes()
    );
}

/// 损坏检测：翻转数据区一位 → crc32c 校验失败
#[test]
fn crc_detection() {
    let (mut bytes, _pk, _txt) = build_file();
    let f = read_footer(&bytes).unwrap();
    // rg0/col0 的数据区第 1 字节（块头之后）
    let off = f.rgs[0].cols[0].blocks[0].offset as usize + dendro_columnar::BLOCK_HEADER_LEN;
    bytes[off] ^= 0xFF;
    let err = read_cbf(&bytes).unwrap_err();
    assert!(err.to_string().contains("crc32c"), "got: {err}");
}
