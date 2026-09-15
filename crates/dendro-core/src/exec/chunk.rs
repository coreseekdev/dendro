//! v2c-1（C1）：ChunkPlane 执行层地基——Chunk 类型（ir-spec 04 §1）。
//!
//! 合同（评审 D3/D4 定死）：
//! - **len = 逻辑行数**：`sel=Some(s)` 时 `s.len()==len` 且
//!   `arrays[i].len() >= len`（物化后相等）——决定全部算子写法；
//! - **零行不变量**：算子不接收也不发送 0 行 Chunk（Filter 全滤空跳过
//!   push；空结果集 schema 由 Sink 出口保证）；
//! - 边界转换（§1）：`to_record_batch` 是唯一出口（连续/无 sel 零拷贝，
//!   sel 非连续必须物化——物化点显式声明，评审 P2）；
//! - v1 实现：包装 arrow ArrayRef（ADR-4：分配优化藏在类型后，池化推迟）。
//!
//! v2c-1a 阶段：类型与转换器先立契约（单测锁定），Source 采纳随后。

use arrow::array::{ArrayRef, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema, SchemaRef};
use std::sync::Arc;

use crate::error::{Result, SqlError};
use crate::types::{ColType, SqlValue};

/// Chunk 的列 schema（名字 + 逻辑类型；跨算子共享的稳定引用）
#[derive(Debug, Clone)]
pub struct ChunkSchema {
    pub cols: Vec<(String, ColType)>,
    arrow: SchemaRef,
}

impl ChunkSchema {
    pub fn new(cols: Vec<(String, ColType)>) -> Result<Self> {
        let fields: Result<Vec<Field>> = cols
            .iter()
            .map(|(n, t)| {
                Ok(Field::new(
                    n.clone(),
                    coltype_to_arrow(t).ok_or_else(|| {
                        SqlError::internal(format!("chunk: 不支持的列类型 {t:?}"))
                    })?,
                    true,
                ))
            })
            .collect();
        Ok(Self {
            cols,
            arrow: Arc::new(ArrowSchema::new(fields?)),
        })
    }
    pub fn arrow(&self) -> SchemaRef {
        self.arrow.clone()
    }
}

pub fn coltype_to_arrow(t: &ColType) -> Option<DataType> {
    Some(match t {
        ColType::Int32 => DataType::Int32,
        ColType::Int64 => DataType::Int64,
        ColType::Float64 => DataType::Float64,
        ColType::Utf8 => DataType::Utf8,
        ColType::Bool => DataType::Boolean,
        ColType::Date32 => DataType::Date32,
        ColType::TimestampMs => DataType::Timestamp(arrow::datatypes::TimeUnit::Millisecond, None),
        ColType::Bytes => DataType::Binary,
    })
}

/// 执行数据单元（04 §1）
#[derive(Debug, Clone)]
pub struct Chunk {
    pub schema: Arc<ChunkSchema>,
    arrays: Vec<ArrayRef>,
    /// 逻辑行数（sel=Some 时 = sel.len()）
    len: usize,
    /// selection vector（过滤不物化；Some(s) ⇒ s.len()==len）
    sel: Option<Box<[u32]>>,
}

impl Chunk {
    /// 全列连续构造（CBF 解码产物 / 行物化出口）
    pub fn from_arrays(schema: Arc<ChunkSchema>, arrays: Vec<ArrayRef>) -> Result<Self> {
        let len = arrays.first().map(|a| a.len()).unwrap_or(0);
        for a in &arrays {
            if a.len() != len {
                return Err(SqlError::internal("chunk: 列长度不一致"));
            }
        }
        if schema.cols.len() != arrays.len() {
            return Err(SqlError::internal("chunk: 列数与 schema 不一致"));
        }
        if len == 0 {
            // 零行不变量：构造即拒绝（调用方跳过发送）
            return Err(SqlError::internal("chunk: 0 行 Chunk 非法"));
        }
        Ok(Self {
            schema,
            arrays,
            len,
            sel: None,
        })
    }

    /// 带 selection 构造（sel 长度 = 逻辑行数；arrays 物理长度 ≥ sel 各项）
    pub fn with_sel(
        schema: Arc<ChunkSchema>,
        arrays: Vec<ArrayRef>,
        sel: Box<[u32]>,
    ) -> Result<Self> {
        if sel.is_empty() {
            return Err(SqlError::internal("chunk: 空 selection 非法（跳过发送）"));
        }
        if schema.cols.len() != arrays.len() {
            return Err(SqlError::internal("chunk: 列数与 schema 不一致"));
        }
        let max = sel.iter().copied().max().unwrap() as usize;
        for a in &arrays {
            if max >= a.len() {
                return Err(SqlError::internal("chunk: selection 越界"));
            }
        }
        let len = sel.len();
        Ok(Self {
            schema,
            arrays,
            len,
            sel: Some(sel),
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// 物理列（sel 场景返回原数组——消费方须走 row_at/take 语义）
    pub fn column(&self, i: usize) -> &ArrayRef {
        &self.arrays[i]
    }
    pub fn ncols(&self) -> usize {
        self.arrays.len()
    }

    /// 逻辑行 → SqlValue（sel 感知）。行物化解码（unwind 边界的原子操作）。
    pub fn row_at(&self, logical: usize, out: &mut Vec<SqlValue>) -> Result<()> {
        if logical >= self.len {
            return Err(SqlError::internal("chunk: row_at 越界"));
        }
        out.clear();
        let phys = match &self.sel {
            Some(s) => s[logical] as usize,
            None => logical,
        };
        for (i, arr) in self.arrays.iter().enumerate() {
            out.push(array_value_at(arr, phys, &self.schema.cols[i].1)?);
        }
        Ok(())
    }

    /// 行构造（Current/Fallback Source 的物化入口；04 §4）
    pub fn from_rows(schema: Arc<ChunkSchema>, rows: &[Vec<SqlValue>]) -> Result<Self> {
        if rows.is_empty() {
            return Err(SqlError::internal("chunk: 0 行 Chunk 非法"));
        }
        let ncols = schema.cols.len();
        let mut builders: Vec<Box<dyn arrow::array::ArrayBuilder>> = Vec::with_capacity(ncols);
        for (_, t) in &schema.cols {
            builders.push(make_builder(t, rows.len())?);
        }
        for r in rows {
            if r.len() != ncols {
                return Err(SqlError::internal("chunk: 行列数不匹配"));
            }
            for (i, v) in r.iter().enumerate() {
                append_value(builders[i].as_mut(), v)?;
            }
        }
        let arrays = builders
            .into_iter()
            .map(|mut b| arrow::array::ArrayBuilder::finish(&mut *b))
            .collect();
        Self::from_arrays(schema, arrays)
    }

    /// 唯一出口：→ RecordBatch（结果集边界）。
    /// 无 sel：零拷贝；有 sel：**物化**（arrow take——物化点显式，04 §6-4）。
    pub fn to_record_batch(&self) -> Result<RecordBatch> {
        match &self.sel {
            None => Ok(
                RecordBatch::try_new(self.schema.arrow(), self.arrays.clone())
                    .map_err(|e| SqlError::internal(format!("chunk: {e}")))?,
            ),
            Some(sel) => {
                let indices = arrow::array::UInt32Array::from(sel.to_vec());
                let taken: Result<Vec<ArrayRef>> = self
                    .arrays
                    .iter()
                    .map(|a| {
                        arrow::compute::take(a, &indices, None)
                            .map_err(|e| SqlError::internal(format!("chunk take: {e}")))
                    })
                    .collect();
                RecordBatch::try_new(self.schema.arrow(), taken?)
                    .map_err(|e| SqlError::internal(format!("chunk: {e}")))
            }
        }
    }
}

fn array_value_at(arr: &ArrayRef, i: usize, t: &ColType) -> Result<SqlValue> {
    use arrow::array::*;
    let a = arr.as_ref();
    Ok(if a.is_null(i) {
        SqlValue::Null
    } else {
        match t {
            ColType::Int32 => {
                SqlValue::Int32(a.as_any().downcast_ref::<Int32Array>().unwrap().value(i))
            }
            ColType::Int64 => {
                SqlValue::Int64(a.as_any().downcast_ref::<Int64Array>().unwrap().value(i))
            }
            ColType::Float64 => {
                SqlValue::Float64(a.as_any().downcast_ref::<Float64Array>().unwrap().value(i))
            }
            ColType::Utf8 => SqlValue::Utf8(
                a.as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .value(i)
                    .to_string(),
            ),
            ColType::Bool => {
                SqlValue::Bool(a.as_any().downcast_ref::<BooleanArray>().unwrap().value(i))
            }
            ColType::Date32 => {
                SqlValue::Date32(a.as_any().downcast_ref::<Date32Array>().unwrap().value(i))
            }
            ColType::TimestampMs => SqlValue::TimestampMs(
                a.as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .unwrap()
                    .value(i),
            ),
            ColType::Bytes => SqlValue::Bytes(
                a.as_any()
                    .downcast_ref::<BinaryArray>()
                    .unwrap()
                    .value(i)
                    .to_vec(),
            ),
        }
    })
}

fn make_builder(t: &ColType, cap: usize) -> Result<Box<dyn arrow::array::ArrayBuilder>> {
    use arrow::array::*;
    Ok(match t {
        ColType::Int32 => Box::new(Int32Builder::with_capacity(cap)),
        ColType::Int64 => Box::new(Int64Builder::with_capacity(cap)),
        ColType::Float64 => Box::new(Float64Builder::with_capacity(cap)),
        ColType::Utf8 => Box::new(StringBuilder::with_capacity(cap, cap * 8)),
        ColType::Bool => Box::new(BooleanBuilder::with_capacity(cap)),
        ColType::Date32 => Box::new(Date32Builder::with_capacity(cap)),
        ColType::TimestampMs => Box::new(TimestampMillisecondBuilder::with_capacity(cap)),
        ColType::Bytes => Box::new(BinaryBuilder::with_capacity(cap, cap * 8)),
    })
}

/// NULL 追加：按 builder 具体类型走 append_option(None)——经由
/// Any 下转后按类型分派（与 append_value 的非空路径对称）
fn append_null_dyn(b: &mut dyn arrow::array::ArrayBuilder) -> Result<()> {
    use arrow::array::*;
    let any = b.as_any_mut();
    macro_rules! nul {
        ($t:ty, $vt:ty) => {
            if let Some(x) = any.downcast_mut::<$t>() {
                x.append_option(None::<$vt>);
                return Ok(());
            }
        };
    }
    nul!(Int32Builder, i32);
    nul!(Int64Builder, i64);
    nul!(Float64Builder, f64);
    nul!(StringBuilder, &str);
    nul!(BooleanBuilder, bool);
    nul!(Date32Builder, i32);
    nul!(TimestampMillisecondBuilder, i64);
    nul!(BinaryBuilder, &[u8]);
    Err(SqlError::internal("chunk: 未知 builder 类型（NULL 追加）"))
}

fn append_value(b: &mut dyn arrow::array::ArrayBuilder, v: &SqlValue) -> Result<()> {
    use arrow::array::*;
    macro_rules! down {
        ($t:ty) => {
            b.as_any_mut().downcast_mut::<$t>().unwrap()
        };
    }
    if matches!(v, SqlValue::Null) {
        append_null_dyn(b)?;
        return Ok(());
    }
    // builder append_value 返回 ()（溢出由 capacity 增长兜底）
    match v {
        SqlValue::Null => unreachable!(),
        SqlValue::Int32(x) => down!(Int32Builder).append_value(*x),
        SqlValue::Int64(x) => down!(Int64Builder).append_value(*x),
        SqlValue::Float64(x) => down!(Float64Builder).append_value(*x),
        SqlValue::Utf8(s) => down!(StringBuilder).append_value(s),
        SqlValue::Bool(x) => down!(BooleanBuilder).append_value(*x),
        SqlValue::Date32(d) => down!(Date32Builder).append_value(*d),
        SqlValue::TimestampMs(t) => down!(TimestampMillisecondBuilder).append_value(*t),
        SqlValue::Bytes(b) => down!(BinaryBuilder).append_value(b),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sch() -> Arc<ChunkSchema> {
        Arc::new(
            ChunkSchema::new(vec![
                ("id".into(), ColType::Int64),
                ("v".into(), ColType::Utf8),
            ])
            .unwrap(),
        )
    }

    /// D4：len=逻辑行数；D3：零行拒绝；出口零拷贝/物化语义
    #[test]
    fn chunk_contracts() {
        let c = Chunk::from_rows(
            sch(),
            &[
                vec![SqlValue::Int64(1), SqlValue::Utf8("a".into())],
                vec![SqlValue::Int64(2), SqlValue::Utf8("b".into())],
                vec![SqlValue::Int64(3), SqlValue::Utf8("c".into())],
            ],
        )
        .unwrap();
        assert_eq!(c.len(), 3);
        let mut row = Vec::new();
        c.row_at(1, &mut row).unwrap();
        assert_eq!(row, vec![SqlValue::Int64(2), SqlValue::Utf8("b".into())]);
        // 出口（无 sel）：3 行
        let rb = c.to_record_batch().unwrap();
        assert_eq!(rb.num_rows(), 3);

        // sel：逻辑行数 = sel.len；row_at 走物理下标
        // 列数不符 → 错误
        let sel: Box<[u32]> = vec![2u32, 0].into_boxed_slice();
        let c2 = Chunk::with_sel(sch(), vec![c.column(0).clone()], sel);
        assert!(c2.is_err());
        // 构造完整 sel chunk（借 c 的两列）
        let sel: Box<[u32]> = vec![2u32, 0].into_boxed_slice();
        let c3 =
            Chunk::with_sel(sch(), vec![c.column(0).clone(), c.column(1).clone()], sel).unwrap();
        assert_eq!(c3.len(), 2, "len = sel.len（逻辑行数）");
        c3.row_at(0, &mut row).unwrap();
        assert_eq!(
            row,
            vec![SqlValue::Int64(3), SqlValue::Utf8("c".into())],
            "row_at 走物理下标"
        );
        // 出口（有 sel）：物化为 2 行
        let rb3 = c3.to_record_batch().unwrap();
        assert_eq!(rb3.num_rows(), 2);

        // 零行拒绝（D3）
        assert!(Chunk::from_rows(sch(), &[]).is_err());
        // round-trip：rows → chunk → rows 全等
        let mut back = Vec::new();
        for i in 0..c.len() {
            let mut r = Vec::new();
            c.row_at(i, &mut r).unwrap();
            back.push(r);
        }
        let src = [
            vec![SqlValue::Int64(1), SqlValue::Utf8("a".into())],
            vec![SqlValue::Int64(2), SqlValue::Utf8("b".into())],
            vec![SqlValue::Int64(3), SqlValue::Utf8("c".into())],
        ];
        assert_eq!(back, src);
    }

    /// NULL 与各类型经 Chunk 不失真
    #[test]
    fn chunk_null_and_types() {
        let s = Arc::new(
            ChunkSchema::new(vec![
                ("a".into(), ColType::Int64),
                ("b".into(), ColType::Float64),
                ("c".into(), ColType::Bool),
            ])
            .unwrap(),
        );
        let c = Chunk::from_rows(
            s,
            &[
                vec![SqlValue::Null, SqlValue::Float64(1.5), SqlValue::Bool(true)],
                vec![SqlValue::Int64(7), SqlValue::Null, SqlValue::Bool(false)],
            ],
        )
        .unwrap();
        let mut r = Vec::new();
        c.row_at(0, &mut r).unwrap();
        assert_eq!(r[0], SqlValue::Null);
        c.row_at(1, &mut r).unwrap();
        assert_eq!(r[1], SqlValue::Null);
        assert_eq!(r[0], SqlValue::Int64(7));
    }
}
