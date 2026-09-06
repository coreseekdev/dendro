//! dendro-core 引擎集成：CBF 物化器 + AP 列式扫描（SPEC 05 §1 §7）。
//!
//! 通过 core 定义的 trait 反转依赖方向（core 不依赖 columnar），
//! 由 dendro-server 启动时注入。

use crate::{read_column_chunk, read_footer, write_cbf};
use arrow::array::ArrayRef;
use arrow::record_batch::RecordBatch;
use dendro_core::engine::{ApScan, Materializer};
use dendro_core::error::{Result, SqlError};
use dendro_core::format::row::decode_row;
use dendro_core::objstore::ObjStore;
use dendro_core::prolly::cursor::TreeIter;
use dendro_core::prolly::NodeStore;
use dendro_core::types::ColType;
use dendro_core::versioned::TableSchema;
use std::sync::Arc;

/// CBF 物化器：行树快照 → Arrow 批 → CBF 对象（col/{table}/{roothash12}.cbf）
pub struct CbfMaterializer {
    pub row_group_rows: usize,
}

impl Materializer for CbfMaterializer {
    fn materialize(
        &self,
        obj: &Arc<dyn ObjStore>,
        store: &Arc<NodeStore>,
        root: &dendro_core::format::hash::Hash,
        schema: &TableSchema,
    ) -> Result<(String, u64)> {
        // 树 → 行（SqlValue）
        let mut it = TreeIter::new(store.clone(), root)?;
        let mut rows: Vec<Vec<dendro_core::SqlValue>> = Vec::with_capacity(1024);
        while let Some((_k, v)) = it.next_item()? {
            let mut vals = decode_row(&v)?;
            vals.resize(schema.columns.len(), dendro_core::SqlValue::Null);
            rows.push(vals);
        }
        let nrows = rows.len() as u64;
        if nrows == 0 {
            // 空表：不产投影
            return Err(SqlError::internal("empty table"));
        }
        let colmeta: Vec<dendro_core::ColumnMeta> = schema
            .columns
            .iter()
            .map(|c| dendro_core::ColumnMeta { name: c.name.clone(), ty: c.ty })
            .collect();
        let mut batches: Vec<RecordBatch> = Vec::new();
        for chunk in rows.chunks(8192) {
            batches.extend(dendro_core::sql::scan::rows_to_batches_typed(&colmeta, chunk));
        }
        let bytes = write_cbf(&batches, self.row_group_rows, None).map_err(|e| SqlError::internal(e.to_string()))?;
        let addr = dendro_core::format::hash::Hash::of(&bytes).to_base32();
        let path = format!("col/{}/{}.cbf", schema.name, &addr[..12]);
        obj.put(&path, bytes.into()).map_err(|e| SqlError::io(format!("cbf put: {e}")))?;
        Ok((path, nrows))
    }
}

/// CBF AP 扫描：footer + 每 RG 的 pk zone map 剪枝 + 按需列解码
pub struct CbfApScan;

impl ApScan for CbfApScan {
    fn scan(
        &self,
        obj: &Arc<dyn ObjStore>,
        path: &str,
        schema: &TableSchema,
        pk_range: &Option<(Option<u64>, Option<u64>)>,
    ) -> Result<Vec<RecordBatch>> {
        let data = obj.get(path).map_err(|e| SqlError::io(format!("cbf get: {e}")))?;
        let footer = read_footer(&data).map_err(|e| SqlError::internal(e.to_string()))?;
        let mut out: Vec<RecordBatch> = Vec::new();
        for rg in 0..footer.rg_count {
            let rgm = &footer.rgs[rg];
            // pk（第 0 列）zone map 剪枝：min/max 是 order 域定点
            if let Some((lo, hi)) = pk_range {
                if footer.rgs[rg].cols.is_empty() {
                    continue;
                }
                let pk = &footer.rgs[rg].cols[0];
                // rg 区间 [rg.min, rg.max]（闭包）；要求区间 (lo,hi) 与其不相交才剪
                if let (Some(l), Some(h)) = (*lo, *hi) {
                    if let (Some(b0), Some(bl)) = (pk.blocks.first(), pk.blocks.last()) {
                        if h <= b0.min || l >= bl.max {
                            continue;
                        }
                    }
                }
            }
            let mut cols: Vec<ArrayRef> = Vec::with_capacity(schema.columns.len());
            for (ci, _c) in schema.columns.iter().enumerate() {
                if ci >= rgm.cols.len() {
                    // schema 演化补 NULL 列
                    use arrow::array::{new_null_array};
                    cols.push(new_null_array(&arrow_type_for(&schema.columns[ci].ty), rgm.rows as usize));
                } else {
                    let arr = read_column_chunk(&data, &footer, rg, ci).map_err(|e| SqlError::internal(e.to_string()))?;
                    cols.push(arr);
                }
            }
            // 批 schema 用 footer 内嵌 schema（列型=物化时的真实类型）
            let batch = RecordBatch::try_new(footer.schema.clone(), cols)
                .map_err(|e| SqlError::internal(format!("cbf batch: {e}")))?;
            out.push(batch);
        }
        Ok(out)
    }
}

fn arrow_type_for(t: &ColType) -> arrow::datatypes::DataType {
    t.arrow()
}
