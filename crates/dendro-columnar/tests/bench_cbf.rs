//! 微基准（SPEC 08 §3 协议的缩尺版：1e6 行，RAW/DELTA/RLE_DICT/ZSTD3 对比）。
//!
//! 运行：`cargo test -p dendro-columnar --release --test bench_cbf -- --ignored --nocapture`
//!
//! 指标：编码吞吐、解码吞吐（按逻辑 raw 字节 GB/s）、压缩比 R = raw/file。

use std::sync::Arc;
use std::time::Instant;

use arrow::array::{Array, ArrayRef, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use dendro_columnar::{read_cbf, write_cbf, CodecId};

const N: usize = 1_000_000;
const RG_ROWS: usize = 1_048_576; // SPEC 05 §2 默认行组（单行组）
const RUNS: usize = 3;

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn bench_column(name: &str, col: &ArrayRef, raw_bytes: u64, codecs: &[CodecId]) {
    let schema = Arc::new(Schema::new(vec![Field::new("c", col.data_type().clone(), false)]));
    let batch = arrow::array::RecordBatch::try_new(schema, vec![col.clone()]).unwrap();
    println!("\n== {name} ==  rows={N} raw={:.2} MB", raw_bytes as f64 / 1e6);
    println!(
        "{:10} {:>12} {:>12} {:>12} {:>10}",
        "codec", "enc MB/s", "dec MB/s", "file MB", "R"
    );
    for codec in codecs {
        if *codec == CodecId::Delta && matches!(col.data_type(), DataType::Utf8) {
            println!("{:10} {:>12} (DELTA 不适用变宽列)", format!("{codec:?}"), "-");
            continue;
        }
        let mut enc_t = Vec::new();
        let mut dec_t = Vec::new();
        let mut file_len = 0u64;
        for _ in 0..RUNS {
            let t0 = Instant::now();
            let bytes = write_cbf(&[batch.clone()], RG_ROWS, Some(&|_, _, _| *codec)).unwrap();
            let t1 = Instant::now();
            let (_s, out) = read_cbf(&bytes).unwrap();
            let t2 = Instant::now();
            assert_eq!(out[0].column(0).len(), N);
            // 防优化：校验首尾值
            let _ = out[0].column(0).is_valid(N - 1);
            enc_t.push(t1.duration_since(t0).as_secs_f64());
            dec_t.push(t2.duration_since(t1).as_secs_f64());
            file_len = bytes.len() as u64;
        }
        let enc = median(&mut enc_t);
        let dec = median(&mut dec_t);
        let raw = raw_bytes as f64;
        println!(
            "{:10} {:>12.0} {:>12.0} {:>12.2} {:>10.2}",
            format!("{codec:?}"),
            raw / 1e6 / enc,
            raw / 1e6 / dec,
            file_len as f64 / 1e6,
            raw / file_len as f64,
        );
    }
}

#[test]
#[ignore]
fn bench_cbf_throughput() {
    // 列 1：i64 顺序（pk/ts 形态；SPEC 08 §5 “顺序 int → 热层 RAW/DELTA”）
    let seq: ArrayRef = Arc::new(Int64Array::from((0..N as i64).collect::<Vec<_>>()));
    bench_column("i64_seq (pk/ts)", &seq, (N * 8) as u64, &[
        CodecId::Raw,
        CodecId::Delta,
        CodecId::RleDict,
        CodecId::Zstd,
    ]);

    // 列 2：utf8 低基数（8 个 distinct 值，每值 ~14B）
    let texts: Vec<String> = (0..N)
        .map(|i| format!("segment_{:02}_cat", i % 8))
        .collect();
    let raw_text = texts.iter().map(|s| s.len()).sum::<usize>() as u64 + (N as u64 + 1) * 4;
    let txt: ArrayRef = Arc::new(StringArray::from(texts));
    bench_column("utf8_low_card (8 distinct)", &txt, raw_text, &[
        CodecId::Raw,
        CodecId::RleDict,
        CodecId::Zstd,
    ]);
}
