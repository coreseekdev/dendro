//! 前置2（ir-spec 07 §1.1）：**结果等价正式定义**——差分/对拍的唯一比较点。
//! 任何测试需要比对两个执行路径的结果时必须经由此模块，禁止各处自定义。
//!
//! 双模式：
//! - 无 ORDER BY 查询：**多重集**比较（列名 + 类型标签 + 值）。行序是
//!   路径相关现状（AP 段序/树序/首见组序不同），v1 不承诺跨路径一致；
//! - 带 ORDER BY（全键）查询：调用方传 `ordered = true`，严格行序比较。
//!
//! 值等价：SqlValue 相等，Float64 例外——IEEE 位等或 **1 ULP 容差**
//! （浮点 sum 的累加顺序依赖行序，路径不同可能差最后一位；若某用例
//! 需要位精确，用 `float_tol_ulp = 0`）；NaN 位等（不与任何值相等，
//! 包括自身——两侧同为 NaN 视为等价）。

#![allow(dead_code)]

use dendro_core::types::{Output, RecordSet, SqlValue};

/// 把 Output 列表规范成 (列名列表, 类型标签列表, 行集合)。
/// 非 Rows 输出（Command 等）以标签行表示——两侧同为 Command 才等价。
pub enum Canonical {
    Rows {
        cols: Vec<String>,
        tys: Vec<String>,
        rows: Vec<Vec<SqlValue>>,
    },
    Commands(Vec<String>),
}

/// 低层：纯 SqlValue 行的等价断言（embed QueryResult / 手工行用）
pub fn assert_rows_equiv(
    ctx: &str,
    cols_a: &[String],
    rows_a: &[Vec<SqlValue>],
    cols_b: &[String],
    rows_b: &[Vec<SqlValue>],
    ordered: bool,
) {
    let c1: Vec<String> = cols_a.to_vec();
    let c2: Vec<String> = cols_b.to_vec();
    let (r1, r2) = (rows_a.to_vec(), rows_b.to_vec());
    compare_rows(ctx, &c1, &r1, &c2, &r2, ordered, 1);
}

pub fn canonicalize(outs: &[Output]) -> Canonical {
    let mut cmds = Vec::new();
    for o in outs {
        match o {
            Output::Command { tag, affected } => cmds.push(format!("{tag} {affected}")),
            Output::Rows(r) => {
                // 一个语句序列里 Rows 之前不应有 Command（差分语料约束）；
                // 首个 Rows 即取其为结果集
                return Canonical::Rows {
                    cols: r.columns.iter().map(|c| c.name.clone()).collect(),
                    tys: r.columns.iter().map(|c| format!("{:?}", c.ty)).collect(),
                    rows: record_rows(r),
                };
            }
        }
    }
    Canonical::Commands(cmds)
}

/// arrow 批 → SqlValue 行（测试侧转换；与引擎内部 rows_from_batches 无关）
fn record_rows(rs: &RecordSet) -> Vec<Vec<SqlValue>> {
    use arrow::array::{
        Array, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array, StringArray,
        TimestampMillisecondArray,
    };
    fn dn(arr: &dyn Array) -> &dyn std::any::Any {
        arr.as_any()
    }
    let mut out = Vec::new();
    for b in &rs.batches {
        for i in 0..b.num_rows() {
            let mut row = Vec::with_capacity(b.num_columns());
            for (ci, arr) in b.columns().iter().enumerate() {
                let a: &dyn Array = arr.as_ref();
                let v = if a.is_null(i) {
                    SqlValue::Null
                } else {
                    match a.data_type() {
                        arrow::datatypes::DataType::Int32 => {
                            SqlValue::Int32(dn(a).downcast_ref::<Int32Array>().unwrap().value(i))
                        }
                        arrow::datatypes::DataType::Int64 => {
                            SqlValue::Int64(dn(a).downcast_ref::<Int64Array>().unwrap().value(i))
                        }
                        arrow::datatypes::DataType::Float64 => SqlValue::Float64(
                            dn(a).downcast_ref::<Float64Array>().unwrap().value(i),
                        ),
                        arrow::datatypes::DataType::Utf8 => SqlValue::Utf8(
                            dn(a)
                                .downcast_ref::<StringArray>()
                                .unwrap()
                                .value(i)
                                .to_string(),
                        ),
                        arrow::datatypes::DataType::Boolean => {
                            SqlValue::Bool(dn(a).downcast_ref::<BooleanArray>().unwrap().value(i))
                        }
                        arrow::datatypes::DataType::Date32 => {
                            SqlValue::Date32(dn(a).downcast_ref::<Date32Array>().unwrap().value(i))
                        }
                        arrow::datatypes::DataType::Timestamp(_, _) => SqlValue::TimestampMs(
                            dn(a)
                                .downcast_ref::<TimestampMillisecondArray>()
                                .unwrap()
                                .value(i),
                        ),
                        arrow::datatypes::DataType::Binary => SqlValue::Bytes(
                            dn(a)
                                .downcast_ref::<arrow::array::BinaryArray>()
                                .unwrap()
                                .value(i)
                                .to_vec(),
                        ),
                        other => panic!("tests/common: 未覆盖的 arrow 类型 {other:?}（列 {ci}）"),
                    }
                };
                row.push(v);
            }
            out.push(row);
        }
    }
    out
}

/// 值等价（§1.1：Float 1 ULP 容差默认；NaN 位等=两侧皆 NaN）
pub fn value_equiv(a: &SqlValue, b: &SqlValue, float_tol_ulp: u32) -> bool {
    match (a, b) {
        (SqlValue::Float64(x), SqlValue::Float64(y)) => {
            if x.is_nan() && y.is_nan() {
                return true;
            }
            if x == y {
                return true;
            }
            if float_tol_ulp == 0 {
                return false;
            }
            let bx = x.to_bits();
            let by = y.to_bits();
            // 同符号下的位距（IEEE754 单调性：位差≈ULP 数）
            bx.abs_diff(by) <= float_tol_ulp as u64
        }
        _ => a == b,
    }
}

/// 结果等价断言。`ordered=false`：多重集；`true`：严格行序（调用方
/// 保证查询带全键 ORDER BY）。
pub fn assert_equiv(ctx: &str, a: &[Output], b: &[Output], ordered: bool) {
    assert_equiv_tol(ctx, a, b, ordered, 1);
}

pub fn assert_equiv_tol(ctx: &str, a: &[Output], b: &[Output], ordered: bool, float_tol_ulp: u32) {
    let (ca, cb) = (canonicalize(a), canonicalize(b));
    match (&ca, &cb) {
        (Canonical::Commands(x), Canonical::Commands(y)) => {
            assert_eq!(x, y, "{ctx}: Command 输出不等价");
        }
        (
            Canonical::Rows {
                cols: c1,
                tys: t1,
                rows: r1,
            },
            Canonical::Rows {
                cols: c2,
                tys: t2,
                rows: r2,
            },
        ) => {
            assert_eq!(t1, t2, "{ctx}: 列类型标签不同");
            compare_rows(ctx, c1, r1, c2, r2, ordered, float_tol_ulp);
        }
        _ => panic!("{ctx}: 输出形态不同（Rows vs Command）"),
    }
}

fn compare_rows(
    ctx: &str,
    c1: &[String],
    r1: &[Vec<SqlValue>],
    c2: &[String],
    r2: &[Vec<SqlValue>],
    ordered: bool,
    tol: u32,
) {
    assert_eq!(c1, c2, "{ctx}: 列名不同（行序外的 schema 必须一致）");
    if r1.len() != r2.len() {
        panic!("{ctx}: 行数不同 {} vs {}", r1.len(), r2.len());
    }
    if ordered {
        for (i, (x, y)) in r1.iter().zip(r2.iter()).enumerate() {
            assert_row(ctx, i, x, y, tol);
        }
    } else {
        // 多重集：按规范键排序后逐一比对（键=值的稳定序列化）
        let key = |r: &Vec<SqlValue>| format!("{r:?}");
        let (mut sa, mut sb) = (r1.to_vec(), r2.to_vec());
        sa.sort_by_key(|r| key(r).to_string());
        sb.sort_by_key(|r| key(r).to_string());
        for (i, (x, y)) in sa.iter().zip(sb.iter()).enumerate() {
            assert_row(ctx, i, x, y, tol);
        }
    }
}

fn assert_row(ctx: &str, i: usize, x: &[SqlValue], y: &[SqlValue], tol: u32) {
    assert_eq!(x.len(), y.len(), "{ctx}: 行 {i} 列数不同");
    for (j, (xa, ya)) in x.iter().zip(y.iter()).enumerate() {
        if !value_equiv(xa, ya, tol) {
            panic!("{ctx}: 行 {i} 列 {j} 不等价：{xa:?} vs {ya:?}");
        }
    }
}
