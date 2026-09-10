//! 测试 1：codec × 列型 × null 比例 roundtrip 矩阵（SPEC 05 §3 全 codec）。
//! 解码后与原始 batch 逐值相等。

mod common;

use common::{assert_roundtrip_eq, roundtrip_single_col, Rng};
use dendro_columnar::CodecId;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array,
    RecordBatch, StringArray, TimestampMillisecondArray,
};
use arrow::datatypes::DataType;

const N: usize = 3000; // rg_rows=997 ⇒ 4 行组，覆盖跨 batch/RG 边界
const RG_ROWS: usize = 997;

fn with_nulls<T>(
    rng: &mut Rng,
    n: usize,
    ratio: f64,
    mut gen: impl FnMut(usize, &mut Rng) -> T,
) -> Vec<Option<T>> {
    (0..n)
        .map(|i| {
            if rng.f01() < ratio {
                None
            } else {
                Some(gen(i, rng))
            }
        })
        .collect()
}

fn make_columns(rng: &mut Rng, null_ratio: f64) -> Vec<(&'static str, ArrayRef)> {
    let mut cols: Vec<(&'static str, ArrayRef)> = Vec::new();

    // i32：混合正负的“随机宽域”值
    let v: Vec<Option<i32>> = with_nulls(rng, N, null_ratio, |_, rng| {
        rng.below(u32::MAX as u64) as i32
    });
    cols.push(("i32_rand", Arc::new(Int32Array::from(v))));

    // i64 顺序（DELTA/排序路径）
    let v: Vec<Option<i64>> = with_nulls(rng, N, null_ratio, |i, _| (i as i64) * 7 - 3);
    cols.push(("i64_seq", Arc::new(Int64Array::from(v))));

    // i64 全域随机
    let v: Vec<Option<i64>> = with_nulls(rng, N, null_ratio, |_, rng| rng.next_u64() as i64);
    cols.push(("i64_rand", Arc::new(Int64Array::from(v))));

    // f64
    let v: Vec<Option<f64>> = with_nulls(rng, N, null_ratio, |_, rng| {
        (rng.next_u64() as f64 / u64::MAX as f64 - 0.5) * 1e6
    });
    cols.push(("f64_rand", Arc::new(Float64Array::from(v))));

    // utf8 低基数（5 值）
    let v: Vec<Option<String>> =
        with_nulls(rng, N, null_ratio, |_, rng| format!("k{}", rng.below(5)));
    cols.push(("utf8_low", Arc::new(StringArray::from(v))));

    // utf8 中基数（1e3 值）
    let v: Vec<Option<String>> = with_nulls(rng, N, null_ratio, |_, rng| {
        format!("mid_{}", rng.below(1000))
    });
    cols.push(("utf8_mid", Arc::new(StringArray::from(v))));

    // utf8 高基数（近乎唯一）
    let v: Vec<Option<String>> = with_nulls(rng, N, null_ratio, |i, rng| {
        format!("uniq_{i}_{:016x}", rng.next_u64())
    });
    cols.push(("utf8_high", Arc::new(StringArray::from(v))));

    // binary（变宽字节）
    let v: Vec<Option<Vec<u8>>> = with_nulls(rng, N, null_ratio, |_, rng| {
        let len = rng.below(16) as usize;
        (0..len).map(|_| rng.below(256) as u8).collect()
    });
    let refs: Vec<Option<&[u8]>> = v.iter().map(|o| o.as_deref()).collect();
    cols.push(("bin_var", Arc::new(BinaryArray::from(refs))));

    // date32（窄域整型）
    let v: Vec<Option<i32>> = with_nulls(rng, N, null_ratio, |_, rng| {
        19_000 - (rng.below(2000) as i32)
    });
    cols.push(("date32", Arc::new(Date32Array::from(v))));

    // timestamp ms（近单调）
    let v: Vec<Option<i64>> = with_nulls(rng, N, null_ratio, |i, _| {
        1_700_000_000_000 + (i as i64) * 1000
    });
    cols.push(("ts_ms", Arc::new(TimestampMillisecondArray::from(v))));

    // bool
    let v: Vec<Option<bool>> = with_nulls(rng, N, null_ratio, |_, rng| rng.below(2) == 1);
    cols.push(("bool_rand", Arc::new(BooleanArray::from(v))));

    cols
}

#[test]
fn roundtrip_all_codecs_all_types() {
    use arrow::datatypes::{Field, Schema};
    use dendro_columnar::{write_cbf, Error};
    let codecs = [
        CodecId::Raw,
        CodecId::BitPack,
        CodecId::RleDict,
        CodecId::Zstd,
        CodecId::Fsst,
        CodecId::Delta,
    ];
    for (ti, null_ratio) in [0.0f64, 0.1, 0.5].into_iter().enumerate() {
        let mut rng = Rng::new(0xC0FFEE + ti as u64);
        for (name, col) in make_columns(&mut rng, null_ratio) {
            for codec in codecs {
                let ctx = format!("ratio={null_ratio} col={name} codec={codec:?}");
                // 契约：BITPACK/DELTA 只作用于定宽列，FSST 只作用于变宽列
                //（越界组合 → CodecNotApplicable）
                let is_var = matches!(col.data_type(), DataType::Utf8 | DataType::Binary);
                let inapplicable = matches!(codec, CodecId::BitPack | CodecId::Delta) && is_var
                    || codec == CodecId::Fsst && !is_var;
                if inapplicable {
                    let schema = Arc::new(Schema::new(vec![Field::new(
                        name,
                        col.data_type().clone(),
                        true,
                    )]));
                    let batch = RecordBatch::try_new(schema, vec![col.clone()]).unwrap();
                    let err = write_cbf(&[batch], RG_ROWS, Some(&|_, _, _| codec)).unwrap_err();
                    assert!(
                        matches!(err, Error::CodecNotApplicable(_)),
                        "{ctx}: expected CodecNotApplicable, got {err}"
                    );
                    continue;
                }
                let (_bytes, footer, batches) = roundtrip_single_col(&col, name, RG_ROWS, codec);
                assert_roundtrip_eq(&col, &batches, &ctx);
                // 块级 rows 记账（测试 3 的一部分）
                let total_rows: u32 = footer.rgs.iter().map(|r| r.rows).sum();
                assert_eq!(total_rows as usize, col.len(), "{ctx}: footer rows");
                let total_nulls: u32 = footer
                    .rgs
                    .iter()
                    .map(|r| r.cols[0].blocks[0].null_count)
                    .sum();
                assert_eq!(
                    total_nulls as usize,
                    col.null_count(),
                    "{ctx}: footer null_count"
                );
            }
        }
    }
}

/// BITPACK/RLE_DICT/DELTA 在 Float64 上走比特域（精确往返，字典值 = f64 bits）
#[test]
fn float_bit_domain_exactness() {
    let mut rng = Rng::new(7);
    let vals: Vec<Option<f64>> = with_nulls(&mut rng, 500, 0.2, |i, rng| match i % 5 {
        0 => -0.0,
        1 => f64::MIN,
        2 => 3.14159e-300 * (i as f64),
        3 => 0.0,
        _ => rng.next_u64() as f64,
    });
    let col: ArrayRef = Arc::new(Float64Array::from(vals));
    for codec in [
        CodecId::BitPack,
        CodecId::RleDict,
        CodecId::Delta,
        CodecId::Zstd,
    ] {
        let (_b, _f, batches) = roundtrip_single_col(&col, "f64bits", 128, codec);
        assert_roundtrip_eq(&col, &batches, &format!("f64bits/{codec:?}"));
    }
}

/// 默认自适应策略（codec_choice=None）端到端：多列混合型文件
#[test]
fn default_adaptive_strategy_roundtrip() {
    use arrow::datatypes::{Field, Schema};
    use dendro_columnar::{read_cbf, write_cbf};

    let mut rng = Rng::new(99);
    let cols = make_columns(&mut rng, 0.1);
    let schema = Arc::new(Schema::new(
        cols.iter()
            .map(|(n, c)| Field::new(*n, c.data_type().clone(), true))
            .collect::<Vec<_>>(),
    ));
    let batch = arrow::array::RecordBatch::try_new(
        schema.clone(),
        cols.iter().map(|(_, c)| c.clone()).collect(),
    )
    .unwrap();
    let bytes = write_cbf(&[batch], 1000, None).unwrap();
    let (schema2, batches) = read_cbf(&bytes).unwrap();
    assert_eq!(schema2, schema);
    assert!(batches.len() >= 3);
    let mut row = 0usize;
    for b in &batches {
        for (ci, (_, col)) in cols.iter().enumerate() {
            let exp = col.slice(row, b.num_rows());
            common::assert_array_eq(exp.as_ref(), b.column(ci).as_ref(), "adaptive");
        }
        row += b.num_rows();
    }
    assert_eq!(row, N);
}

/// 默认策略 FSST 规则：高基数（ratio ≥ 0.2）且均长 ≥ 6B 的 Utf8 → FSST；
/// 低基数 → RLE_DICT；短串高基数 / Bytes → RAW（codec_choice 可强制覆盖）
#[test]
fn fsst_policy_selection() {
    use dendro_columnar::{choose_codec, ColStats, ColType};
    let base = ColStats {
        rows: 1000,
        null_count: 0,
        distinct: 0,
        min: 0,
        max: 0,
        monotonic: false,
        avg_len: 0.0,
    };
    let utf8 = |ratio: f64, avg: f64| ColStats {
        distinct: (ratio * 1000.0) as usize,
        avg_len: avg,
        ..base.clone()
    };
    let c = |ty, s: &ColStats| choose_codec("x", ty, s);
    // 高基数长串 → FSST
    assert_eq!(c(ColType::Utf8, &utf8(0.95, 24.0)), CodecId::Fsst);
    // 高基数但过短（摊不平符号表）→ RAW
    assert_eq!(c(ColType::Utf8, &utf8(0.95, 3.0)), CodecId::Raw);
    // 低基数 → RLE_DICT（FSST 不越过 dict 边界）
    assert_eq!(c(ColType::Utf8, &utf8(0.05, 24.0)), CodecId::RleDict);
    // Bytes 不进默认 FSST（blob 常不可压；冷块经 codec_choice 指定）
    assert_eq!(c(ColType::Bytes, &utf8(0.95, 24.0)), CodecId::Raw);
}

/// FSST switch-on 不可压输入回归：4096 行 × 64B 随机 blob = 256KB 块（>32KB）。
/// compress_bulk 对每字节投机写 out[curr+1]，输出缓冲预置不足会越界 panic
///（修复：编码缓冲预置 2×+8，内部按实际缩回）。
#[test]
fn fsst_switch_on_incompressible_roundtrip() {
    let mut rng = Rng::new(0xBADF00D);
    let v: Vec<Option<Vec<u8>>> = (0..4096)
        .map(|_| Some((0..64).map(|_| rng.below(256) as u8).collect::<Vec<u8>>()))
        .collect();
    let refs: Vec<Option<&[u8]>> = v.iter().map(|o| o.as_deref()).collect();
    let col: ArrayRef = Arc::new(BinaryArray::from(refs));
    let (_bytes, footer, batches) = roundtrip_single_col(&col, "bin_fsst", 4096, CodecId::Fsst);
    assert_roundtrip_eq(&col, &batches, "fsst-incompressible");
    assert_eq!(footer.rgs[0].cols[0].blocks[0].codec, CodecId::Fsst);
}

/// 默认策略负收益守卫：高均长高基数但近随机（base62 token）→ choose_codec
/// 初选 FSST，样本试编码收益 <10%（≥90% 原始大小）→ 全文件降级 RAW。
/// 显式 codec_choice 强制 FSST 不受守卫影响（视为契约）。
#[test]
fn fsst_default_policy_downgrades_incompressible() {
    use arrow::datatypes::{Field, Schema};
    use dendro_columnar::{read_cbf, read_footer, write_cbf};
    let mut rng = Rng::new(0xFEED);
    let alpha = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let v: Vec<Option<String>> = (0..20_000)
        .map(|_| {
            Some(
                (0..32)
                    .map(|_| alpha[rng.below(alpha.len() as u64) as usize] as char)
                    .collect(),
            )
        })
        .collect();
    let col: ArrayRef = Arc::new(StringArray::from(v));
    let schema = Arc::new(Schema::new(vec![Field::new("tok", DataType::Utf8, true)]));
    let batch = RecordBatch::try_new(schema, vec![col.clone()]).unwrap();
    let bytes = write_cbf(&[batch], 20_000, None).unwrap();
    let footer = read_footer(&bytes).unwrap();
    assert_eq!(
        footer.rgs[0].cols[0].blocks[0].codec,
        CodecId::Raw,
        "不可压高基数文本应经试编码降级 RAW"
    );
    let (_s, batches) = read_cbf(&bytes).unwrap();
    assert_roundtrip_eq(&col, &batches, "fsst-downgrade");
}

/// 多 batch 输入（RG 边界切分 batch，SPEC 05 §2）
#[test]
fn multi_batch_row_group_split() {
    use arrow::datatypes::{Field, Schema};
    use dendro_columnar::{read_cbf, write_cbf};
    let mut rng = Rng::new(5);
    let v: Vec<Option<i64>> = with_nulls(&mut rng, 1000, 0.1, |i, _| i as i64);
    let col: ArrayRef = Arc::new(Int64Array::from(v));
    let schema = Arc::new(Schema::new(vec![Field::new("pk", DataType::Int64, true)]));
    let b1 = RecordBatch::try_new(schema.clone(), vec![col.slice(0, 400)]).unwrap();
    let b2 = RecordBatch::try_new(schema.clone(), vec![col.slice(400, 250)]).unwrap();
    let b3 = RecordBatch::try_new(schema.clone(), vec![col.slice(650, 350)]).unwrap();
    let bytes = write_cbf(&[b1, b2, b3], 300, None).unwrap();
    let (_s, batches) = read_cbf(&bytes).unwrap();
    assert_eq!(
        batches.iter().map(|b| b.num_rows()).collect::<Vec<_>>(),
        vec![300, 300, 300, 100]
    );
    assert_roundtrip_eq(&col, &batches, "multi-batch");
}
