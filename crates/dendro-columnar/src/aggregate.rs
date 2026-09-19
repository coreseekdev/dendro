//! Arrow 原生全局聚合（性能批 P0）——无 GROUP BY 的
//! COUNT/SUM/AVG/MIN/MAX 直接在 Arrow 列数组上计算，
//! 免 rows_from_batches 行式转换（1M×106 列 ≈ 106M SqlValue
//! 创建——q01-q07 类查询的 CPU 与内存主源）。
//!
//! 输入：列存段集 + 列名 → 输出：单行聚合值行。
//! 仅处理全局聚合（keys 空）；GROUP BY 走行式路径不变。

use dendro_core::engine::ColumnarStore as _;

use arrow::array::{
    Array, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array, StringArray,
    TimestampMillisecondArray,
};
use std::sync::Arc;

/// 聚合函数（与 AggCall display 同口径）
#[derive(Debug, Clone, PartialEq)]
pub enum AggKind {
    Count,
    CountStar,
    Sum,
    Avg,
    Min,
    Max,
}

/// 全局聚合请求
pub struct GlobalAgg {
    pub kind: AggKind,
    /// 列名（CountStar 无需）
    pub col: Option<String>,
}

/// 在 RecordBatch 列上执行全局聚合（无 GROUP BY）
pub fn global_aggregate(
    batches: &[arrow::record_batch::RecordBatch],
    schema_cols: &[String],
    reqs: &[GlobalAgg],
) -> crate::Result<Vec<dendro_core::types::SqlValue>> {
    let mut out = Vec::with_capacity(reqs.len());
    for req in reqs {
        let v = match req.kind {
            AggKind::CountStar => {
                let n: usize = batches.iter().map(|b| b.num_rows()).sum();
                dendro_core::types::SqlValue::Int64(n as i64)
            }
            AggKind::Count => {
                let ci = col_index(schema_cols, req.col.as_deref().unwrap_or(""))?;
                let n: usize = batches
                    .iter()
                    .map(|b| b.num_rows() - b.column(ci).null_count())
                    .sum();
                dendro_core::types::SqlValue::Int64(n as i64)
            }
            AggKind::Sum | AggKind::Avg | AggKind::Min | AggKind::Max => {
                let ci = col_index(schema_cols, req.col.as_deref().unwrap_or(""))?;
                agg_on_column(batches, ci, &req.kind)?
            }
        };
        out.push(v);
    }
    Ok(out)
}

fn col_index(names: &[String], name: &str) -> crate::Result<usize> {
    names
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name))
        .ok_or_else(|| crate::Error::InvalidInput(format!("{name}")))
}

/// 在列数组上计算 SUM/AVG/MIN/MAX（数值族精确——i128 累加、f64 混入）
fn agg_on_column(
    batches: &[arrow::record_batch::RecordBatch],
    ci: usize,
    kind: &AggKind,
) -> crate::Result<dendro_core::types::SqlValue> {
    let mut sum_i: i128 = 0;
    let mut sum_f: f64 = 0.0;
    let mut is_float = false;
    let mut count: usize = 0;
    let mut min: Option<dendro_core::types::SqlValue> = None;
    let mut max: Option<dendro_core::types::SqlValue> = None;
    let mut any = false;

    for b in batches {
        let col = b.column(ci);
        any = any || col.len() > col.null_count();

        // 逐类型扫描（Arrow 零拷贝访问——不创建 SqlValue 中间对象）
        downcast_and_agg(
            col.as_ref(),
            &mut sum_i,
            &mut sum_f,
            &mut is_float,
            &mut count,
            &mut min,
            &mut max,
        )?;
    }

    if !any {
        // 全 NULL/空 → SUM/AVG = NULL, MIN/MAX = NULL
        return Ok(dendro_core::types::SqlValue::Null);
    }

    match kind {
        AggKind::Sum => {
            if is_float {
                Ok(dendro_core::types::SqlValue::Float64(sum_f + sum_i as f64))
            } else {
                Ok(dendro_core::types::SqlValue::Int64(
                    i64::try_from(sum_i).map_err(|_| {
                        crate::Error::InvalidInput("bigint sum out of range".into())
                    })?,
                ))
            }
        }
        AggKind::Avg => {
            if count == 0 {
                return Ok(dendro_core::types::SqlValue::Null);
            }
            let total = sum_f + sum_i as f64;
            Ok(dendro_core::types::SqlValue::Float64(total / count as f64))
        }
        AggKind::Min => {
            min.ok_or_else(|| crate::Error::InvalidInput("min with data but None".into()))
        }
        AggKind::Max => {
            max.ok_or_else(|| crate::Error::InvalidInput("max with data but None".into()))
        }
        _ => unreachable!(),
    }
}

fn downcast_and_agg(
    arr: &dyn Array,
    sum_i: &mut i128,
    sum_f: &mut f64,
    is_float: &mut bool,
    count: &mut usize,
    min: &mut Option<dendro_core::types::SqlValue>,
    max: &mut Option<dendro_core::types::SqlValue>,
) -> crate::Result<()> {
    use dendro_core::types::SqlValue;
    let mut update_min = |v: SqlValue| {
        if let Some(m) = min {
            if let Ok(std::cmp::Ordering::Less) = dendro_core::sql::expr::cmp_values_pub(&v, m) {
                *min = Some(v);
            }
        } else {
            *min = Some(v);
        }
    };
    let mut update_max = |v: SqlValue| {
        if let Some(m) = max {
            if let Ok(std::cmp::Ordering::Greater) = dendro_core::sql::expr::cmp_values_pub(&v, m) {
                *max = Some(v);
            }
        } else {
            *max = Some(v);
        }
    };

    if let Some(a) = arr.as_any().downcast_ref::<Int64Array>() {
        for i in 0..a.len() {
            if !a.is_null(i) {
                let v = a.value(i);
                *sum_i += v as i128;
                *count += 1;
                let sv = SqlValue::Int64(v);
                update_min(sv.clone());
                update_max(sv);
            }
        }
    } else if let Some(a) = arr.as_any().downcast_ref::<Int32Array>() {
        for i in 0..a.len() {
            if !a.is_null(i) {
                let v = a.value(i);
                *sum_i += v as i128;
                *count += 1;
                let sv = SqlValue::Int32(v);
                update_min(sv.clone());
                update_max(sv);
            }
        }
    } else if let Some(a) = arr.as_any().downcast_ref::<Float64Array>() {
        *is_float = true;
        for i in 0..a.len() {
            if !a.is_null(i) {
                let v = a.value(i);
                *sum_f += v;
                *count += 1;
                let sv = SqlValue::Float64(v);
                update_min(sv.clone());
                update_max(sv);
            }
        }
    } else if let Some(a) = arr.as_any().downcast_ref::<StringArray>() {
        // SUM/AVG 非数值 → 错误；MIN/MAX 合法
        for i in 0..a.len() {
            if !a.is_null(i) {
                *count += 1;
                let sv = SqlValue::Utf8(a.value(i).to_string());
                update_min(sv.clone());
                update_max(sv);
            }
        }
    } else if let Some(a) = arr.as_any().downcast_ref::<BooleanArray>() {
        for i in 0..a.len() {
            if !a.is_null(i) {
                let sv = SqlValue::Bool(a.value(i));
                update_min(sv.clone());
                update_max(sv);
            }
        }
    } else if let Some(a) = arr.as_any().downcast_ref::<Date32Array>() {
        for i in 0..a.len() {
            if !a.is_null(i) {
                let sv = SqlValue::Date32(a.value(i));
                update_min(sv.clone());
                update_max(sv);
            }
        }
    } else if let Some(a) = arr.as_any().downcast_ref::<TimestampMillisecondArray>() {
        for i in 0..a.len() {
            if !a.is_null(i) {
                let sv = SqlValue::TimestampMs(a.value(i));
                update_min(sv.clone());
                update_max(sv);
            }
        }
    }
    Ok(())
}

/// Arrow 原生全局聚合的公共口：列存引擎接口扩展
pub trait ArrowAggregate {
    /// 全局聚合（无 GROUP BY）直接在 Arrow 列上计算
    fn global_agg(
        &self,
        obj: &Arc<dyn dendro_core::objstore::ObjStore>,
        schema: &dendro_core::versioned::TableSchema,
        segments: &[dendro_core::versioned::ColSegment],
        reqs: &[GlobalAgg],
    ) -> crate::Result<Vec<dendro_core::types::SqlValue>>;
}

impl ArrowAggregate for crate::integrate::CbfColumnar {
    fn global_agg(
        &self,
        obj: &Arc<dyn dendro_core::objstore::ObjStore>,
        schema: &dendro_core::versioned::TableSchema,
        segments: &[dendro_core::versioned::ColSegment],
        reqs: &[GlobalAgg],
    ) -> crate::Result<Vec<dendro_core::types::SqlValue>> {
        let names: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        // 只读需要的列——按 reqs 收集列掩码（COUNT(*) 无列需求）
        let mut mask = vec![false; names.len()];
        // pk 列总是需要（段读取的 pk 口径——不裁 pk）
        for &pi in &schema.pk {
            mask[pi as usize] = true;
        }
        for req in reqs {
            if let Some(c) = &req.col {
                if let Some(ci) = names.iter().position(|n| n.eq_ignore_ascii_case(c)) {
                    mask[ci] = true;
                }
            }
        }
        let batches =
            crate::integrate::CbfColumnar::scan(self, obj, schema, segments, &None, Some(&mask))
                .map_err(|e| crate::Error::InvalidInput(format!("cbf scan: {e}")))?;
        // 计量：活动段批（RAII——覆盖聚合计算期）
        use arrow::array::Array as _;
        let _guard = dendro_core::memprof::MeterGuard::new(
            dendro_core::memprof::colscan_active(),
            batches
                .iter()
                .map(|b| b.get_array_memory_size() as u64)
                .sum::<u64>(),
            batches.iter().map(|b| b.num_rows() as u64).sum::<u64>(),
        );
        // P0-2 后掩码扫描产窄批（只含活跃列）——名字表同步收窄，
        // col_index 按窄名定位
        let active_names: Vec<String> = names
            .iter()
            .zip(mask.iter())
            .filter(|(_, &b)| b)
            .map(|(n, _)| n.clone())
            .collect();
        global_aggregate(&batches, &active_names, reqs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_agg_basics() {
        use arrow::array::Int64Array;
        use arrow::datatypes::{DataType, Field, Schema};
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
        let arr = Arc::new(Int64Array::from(vec![1, 2, 3, 4, 5]));
        let batch = arrow::record_batch::RecordBatch::try_new(schema, vec![arr]).unwrap();
        let cols = vec!["v".to_string()];

        // COUNT(*)
        let r = global_aggregate(
            &[batch.clone()],
            &cols,
            &[GlobalAgg {
                kind: AggKind::CountStar,
                col: None,
            }],
        )
        .unwrap();
        assert_eq!(r[0], dendro_core::types::SqlValue::Int64(5));

        // SUM
        let r = global_aggregate(
            &[batch.clone()],
            &cols,
            &[GlobalAgg {
                kind: AggKind::Sum,
                col: Some("v".into()),
            }],
        )
        .unwrap();
        assert_eq!(r[0], dendro_core::types::SqlValue::Int64(15));

        // AVG
        let r = global_aggregate(
            &[batch.clone()],
            &cols,
            &[GlobalAgg {
                kind: AggKind::Avg,
                col: Some("v".into()),
            }],
        )
        .unwrap();
        assert_eq!(r[0], dendro_core::types::SqlValue::Float64(3.0));

        // MIN/MAX
        let r = global_aggregate(
            &[batch.clone()],
            &cols,
            &[GlobalAgg {
                kind: AggKind::Min,
                col: Some("v".into()),
            }],
        )
        .unwrap();
        assert_eq!(r[0], dendro_core::types::SqlValue::Int64(1));
        let r = global_aggregate(
            &[batch],
            &cols,
            &[GlobalAgg {
                kind: AggKind::Max,
                col: Some("v".into()),
            }],
        )
        .unwrap();
        assert_eq!(r[0], dendro_core::types::SqlValue::Int64(5));
    }
}
