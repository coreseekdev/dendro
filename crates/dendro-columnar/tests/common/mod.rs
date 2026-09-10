//! 测试共享工具：确定性随机数（不引入 rand 依赖）+ 逐值逻辑比较。

#![allow(dead_code)]

use arrow::array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array,
    RecordBatch, StringArray, TimestampMillisecondArray,
};
use arrow::datatypes::DataType;
use dendro_columnar::{read_cbf, read_footer};

/// xorshift64* — 确定性、无依赖
pub struct Rng(pub u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// [0, n)
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
    /// 0.0..1.0
    pub fn f01(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// 逻辑逐值相等（null 位置与值都一致）；覆盖 CBF 支持的全部列型
pub fn assert_array_eq(expected: &dyn Array, got: &dyn Array, ctx: &str) {
    assert_eq!(
        expected.data_type(),
        got.data_type(),
        "{ctx}: data_type mismatch"
    );
    assert_eq!(expected.len(), got.len(), "{ctx}: len mismatch");
    assert_eq!(
        expected.null_count(),
        got.null_count(),
        "{ctx}: null_count mismatch"
    );
    for i in 0..expected.len() {
        assert_eq!(
            expected.is_null(i),
            got.is_null(i),
            "{ctx}: row {i} null flag"
        );
        if expected.is_null(i) {
            continue;
        }
        let ok = match expected.data_type() {
            DataType::Boolean => {
                expected
                    .as_any()
                    .downcast_ref::<BooleanArray>()
                    .unwrap()
                    .value(i)
                    == got
                        .as_any()
                        .downcast_ref::<BooleanArray>()
                        .unwrap()
                        .value(i)
            }
            DataType::Int32 => {
                expected
                    .as_any()
                    .downcast_ref::<Int32Array>()
                    .unwrap()
                    .value(i)
                    == got.as_any().downcast_ref::<Int32Array>().unwrap().value(i)
            }
            DataType::Int64 => {
                expected
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .value(i)
                    == got.as_any().downcast_ref::<Int64Array>().unwrap().value(i)
            }
            DataType::Float64 => {
                let a = expected
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .unwrap()
                    .value(i);
                let b = got
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .unwrap()
                    .value(i);
                a.to_bits() == b.to_bits()
            }
            DataType::Utf8 => {
                expected
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .value(i)
                    == got.as_any().downcast_ref::<StringArray>().unwrap().value(i)
            }
            DataType::Binary => {
                expected
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .unwrap()
                    .value(i)
                    == got.as_any().downcast_ref::<BinaryArray>().unwrap().value(i)
            }
            DataType::Date32 => {
                expected
                    .as_any()
                    .downcast_ref::<Date32Array>()
                    .unwrap()
                    .value(i)
                    == got.as_any().downcast_ref::<Date32Array>().unwrap().value(i)
            }
            DataType::Timestamp(arrow::datatypes::TimeUnit::Millisecond, _) => {
                expected
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .unwrap()
                    .value(i)
                    == got
                        .as_any()
                        .downcast_ref::<TimestampMillisecondArray>()
                        .unwrap()
                        .value(i)
            }
            other => panic!("{ctx}: unsupported type {other:?}"),
        };
        assert!(ok, "{ctx}: row {i} value mismatch");
    }
}

/// 写 → 读 单列，校验逐值相等；返回 (file bytes, footer, 输出 batches)
pub fn roundtrip_single_col(
    col: &ArrayRef,
    col_name: &str,
    rg_rows: usize,
    codec: dendro_columnar::CodecId,
) -> (Vec<u8>, dendro_columnar::CbfFooter, Vec<RecordBatch>) {
    use arrow::datatypes::{Field, Schema};
    use dendro_columnar::write_cbf;
    let schema = std::sync::Arc::new(Schema::new(vec![Field::new(
        col_name,
        col.data_type().clone(),
        true,
    )]));
    let batch = RecordBatch::try_new(schema.clone(), vec![col.clone()]).unwrap();
    let bytes = write_cbf(&[batch], rg_rows, Some(&move |_, _, _| codec)).unwrap();
    let (schema2, batches) = read_cbf(&bytes).unwrap();
    assert_eq!(schema2.fields().len(), 1);
    assert_eq!(schema2.field(0).name(), col_name);
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, col.len(), "row count mismatch after roundtrip");
    let footer = read_footer(&bytes).unwrap();
    (bytes, footer, batches)
}

/// 跨行组拼接后与原列逐值比较（行组保持行序 ⇒ 对应切片直接比较）
pub fn assert_roundtrip_eq(col: &ArrayRef, batches: &[RecordBatch], ctx: &str) {
    let mut row = 0usize;
    for (ri, b) in batches.iter().enumerate() {
        let got = b.column(0);
        let n = got.len();
        let exp = col.slice(row, n);
        assert_array_eq(exp.as_ref(), got.as_ref(), &format!("{ctx}: rg{ri}"));
        row += n;
    }
    assert_eq!(row, col.len());
}
