//! dendro-core 引擎集成：CBF 列存存储（增量分段，SPEC 05 §1 §6）。
//!
//! 通过 core 定义的 `ColumnarStore` trait 反转依赖方向（core 不依赖 columnar），
//! 由 dendro-server 启动时注入。
//!
//! OSS 友好要点：
//! - `write_segment`：纯内存输入（memtx 增量），一次 PUT 写一个不可变小段
//! - `write_full`：读整棵行树（经缓存的对象存储）重建单段，替代 N 个小段
//! - `scan`：段级 pk 剪枝 + 行组级 zone map 剪枝 + 按需列解码

use crate::{read_column_chunk, read_footer, write_cbf};
use arrow::array::ArrayRef;
use arrow::record_batch::RecordBatch;
use dendro_core::engine::ColumnarStore;
use dendro_core::error::{Result, SqlError};
use dendro_core::format::hash::Hash;
use dendro_core::format::row::{decode_row, encode_key};
use dendro_core::objstore::ObjStore;
use dendro_core::prolly::cursor::TreeIter;
use dendro_core::prolly::NodeStore;
use dendro_core::types::SqlValue;
use dendro_core::versioned::{ColSegment, TableSchema};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct CbfColumnar {
    pub row_group_rows: usize,
}

impl CbfColumnar {
    fn rows_to_batches(
        &self,
        schema: &TableSchema,
        rows: &BTreeMap<Vec<u8>, Vec<SqlValue>>,
    ) -> (Vec<RecordBatch>, u64) {
        let colmeta: Vec<dendro_core::ColumnMeta> = schema
            .columns
            .iter()
            .map(|c| dendro_core::ColumnMeta {
                name: c.name.clone(),
                ty: c.ty,
            })
            .collect();
        let mut batches = Vec::new();
        let n = rows.len();
        let mut written = 0u64;
        let mut start = 0usize;
        while start < n {
            let end = (start + 8192).min(n);
            let chunk: Vec<Vec<SqlValue>> = rows
                .values()
                .skip(start)
                .take(end - start)
                .cloned()
                .collect();
            written += chunk.len() as u64;
            batches.extend(dendro_core::sql::scan::rows_to_batches_typed(
                &colmeta, &chunk,
            ));
            start = end;
        }
        (batches, written)
    }

    /// pk 键（保序编码）→ order 域定点（与 footer min/max 同口径）
    fn order_domain(key: &[u8]) -> u64 {
        if key.len() >= 9 && key[0] == 0x10 {
            u64::from_be_bytes(key[1..9].try_into().unwrap())
        } else {
            let mut b = [0u8; 8];
            for (i, byte) in key.iter().skip(1).take(8).enumerate() {
                b[i] = *byte;
            }
            u64::from_be_bytes(b)
        }
    }

    fn build_segment(
        &self,
        obj: &Arc<dyn ObjStore>,
        table: &str,
        schema: &TableSchema,
        rows: &mut BTreeMap<Vec<u8>, Vec<SqlValue>>,
    ) -> Result<ColSegment> {
        let (batches, total) = self.rows_to_batches(schema, rows);
        let bytes = write_cbf(&batches, self.row_group_rows, None)
            .map_err(|e| SqlError::internal(e.to_string()))?;
        let (pk_min, pk_max) = match (rows.keys().next(), rows.keys().next_back()) {
            (Some(k1), Some(k2)) => (Self::order_domain(k1), Self::order_domain(k2)),
            _ => (0, u64::MAX),
        };
        let addr = Hash::of(&bytes).to_base32();
        let path = format!("col/{table}/{}.cbf", &addr[..12]);
        obj.put(&path, bytes.into())
            .map_err(|e| SqlError::io(format!("cbf put: {e}")))?;
        Ok(ColSegment {
            path,
            rows: total,
            pk_min,
            pk_max,
        })
    }

    fn key_rows(
        schema: &TableSchema,
        rows: &[Vec<SqlValue>],
    ) -> Result<BTreeMap<Vec<u8>, Vec<SqlValue>>> {
        let pkc = *schema
            .pk
            .first()
            .ok_or_else(|| SqlError::internal("no pk"))? as usize;
        let mut keyed = BTreeMap::new();
        for r in rows {
            let pkv = r
                .get(pkc)
                .cloned()
                .ok_or_else(|| SqlError::internal("row shorter than pk"))?;
            keyed.insert(encode_key(&[pkv]), r.clone());
        }
        Ok(keyed)
    }
}

impl ColumnarStore for CbfColumnar {
    fn write_segment(
        &self,
        obj: &Arc<dyn ObjStore>,
        table: &str,
        schema: &TableSchema,
        rows: &[Vec<SqlValue>],
    ) -> Result<ColSegment> {
        let mut keyed = Self::key_rows(schema, rows)?;
        self.build_segment(obj, table, schema, &mut keyed)
    }

    fn write_full(
        &self,
        obj: &Arc<dyn ObjStore>,
        store: &Arc<NodeStore>,
        root: &Hash,
        schema: &TableSchema,
        existing: &[ColSegment],
    ) -> Result<(ColSegment, Vec<String>)> {
        // 读整棵行树（对象存储读经缓存层）
        let mut it = TreeIter::new(store.clone(), root)?;
        let mut keyed: BTreeMap<Vec<u8>, Vec<SqlValue>> = BTreeMap::new();
        while let Some((k, v)) = it.next_item()? {
            let mut vals = decode_row(&v)?;
            vals.resize(schema.columns.len(), SqlValue::Null);
            keyed.insert(k, vals);
        }
        let seg = self.build_segment(obj, &schema.name, schema, &mut keyed)?;
        let old_paths: Vec<String> = existing.iter().map(|s| s.path.clone()).collect();
        Ok((seg, old_paths))
    }

    fn col_stats(
        &self,
        obj: &Arc<dyn ObjStore>,
        segments: &[ColSegment],
    ) -> Option<Vec<dendro_core::sql::stats::ColStat>> {
        let agg = crate::segment_col_stats(obj, segments).ok()?;
        Some(
            agg.into_iter()
                .map(|c| dendro_core::sql::stats::ColStat {
                    rows: c.rows,
                    nulls: c.nulls,
                    min: c.min,
                    max: c.max,
                    has_data: c.has_data,
                })
                .collect(),
        )
    }

    fn scan(
        &self,
        obj: &Arc<dyn ObjStore>,
        schema: &TableSchema,
        segments: &[ColSegment],
        pk_range: &Option<(Option<u64>, Option<u64>)>,
        col_mask: Option<&[bool]>,
    ) -> Result<Vec<RecordBatch>> {
        let mut out = Vec::new();
        for seg in segments {
            // 段级 pk 剪枝：开区间 (lo,hi) 与 [min,max] 不相交则整段跳过。
            // 账本 #24：None=无界——不得用 Option 序比较（None<=Some 恒真
            // 会把单边开范围整段剪掉）
            if let Some((lo, hi)) = pk_range {
                if hi.is_some_and(|h| h <= seg.pk_min) || lo.is_some_and(|l| l >= seg.pk_max) {
                    continue;
                }
            }
            // O-3+ 稀疏读：有裁剪列时 footer 与所需块经 get_range 取
            //（跳列的 IO 面——不再整文件 get）；无裁剪维持整文件读
            let pruned = col_mask.is_some_and(|m| m.iter().any(|&b| !b));
            let data: bytes::Bytes = if pruned {
                bytes::Bytes::new()
            } else {
                obj.get(&seg.path)
                    .map_err(|e| SqlError::io(format!("cbf get: {e}")))?
            };
            let footer = if pruned {
                let len = obj
                    .head(&seg.path)
                    .map_err(|e| SqlError::io(format!("cbf head: {e}")))?
                    .ok_or_else(|| SqlError::io("cbf segment missing"))?
                    .len;
                let tail = obj
                    .get_range(&seg.path, len - 8, 8)
                    .map_err(|e| SqlError::io(format!("cbf tail: {e}")))?;
                let flen = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as u64;
                let fbody = obj
                    .get_range(&seg.path, len - flen, (flen - 8) as usize)
                    .map_err(|e| SqlError::io(format!("cbf footer: {e}")))?;
                crate::footer::parse_footer_from(&tail, &fbody)
                    .map_err(|e| SqlError::internal(e.to_string()))?
            } else {
                read_footer(&data).map_err(|e| SqlError::internal(e.to_string()))?
            };
            // 稀疏字节源（块头/块数据/validity 各自 get_range）
            let sparse_fetch: Option<Box<dyn Fn(u64, usize) -> crate::Result<Vec<u8>> + '_>> =
                if pruned {
                    let path = seg.path.clone();
                    let obj2 = obj.clone();
                    Some(Box::new(move |off: u64, n: usize| {
                        obj2.get_range(&path, off, n)
                            .map(|b| b.to_vec())
                            .map_err(|e| crate::Error::InvalidInput(format!("range: {e}")))
                    }))
                } else {
                    None
                };
            for rg in 0..footer.rg_count {
                let rgm = &footer.rgs[rg];
                if rgm.cols.is_empty() {
                    continue;
                }
                // 行组级 pk zone map 剪枝
                let pk = &rgm.cols[0];
                if let Some((lo, hi)) = pk_range {
                    if let (Some(b0), Some(bl)) = (pk.blocks.first(), pk.blocks.last()) {
                        // 账本 #24 同修：None=无界（is_some_and 判定）
                        if hi.is_some_and(|h| h <= b0.min) || lo.is_some_and(|l| l >= bl.max) {
                            continue;
                        }
                    }
                }
                let mut cols: Vec<ArrayRef> = Vec::with_capacity(schema.columns.len());
                for ci in 0..schema.columns.len() {
                    // 缺列/裁剪列的 null 占位类型取 footer 字段类型
                    //（物化时真实类型——与批 schema 一致；当前 schema
                    // 可能已分叉，streaming_source 差分实证）
                    let null_ty = || footer.schema.field(ci).data_type().clone();
                    if ci >= rgm.cols.len() {
                        use arrow::array::new_null_array;
                        cols.push(new_null_array(&null_ty(), rgm.rows as usize));
                    } else if col_mask.is_some_and(|m| !m[ci]) {
                        // O-3 投影裁剪：非需求列零成本 null 占位（不解码
                        // 列 chunk；批宽恒 = schema 宽——消费端零映射）
                        use arrow::array::new_null_array;
                        cols.push(new_null_array(&null_ty(), rgm.rows as usize));
                    } else if let Some(f) = &sparse_fetch {
                        let arr = crate::reader::read_column_chunk_fetch(f, &footer, rg, ci)
                            .map_err(|e| SqlError::internal(e.to_string()))?;
                        cols.push(arr);
                    } else {
                        let arr = read_column_chunk(&data, &footer, rg, ci)
                            .map_err(|e| SqlError::internal(e.to_string()))?;
                        cols.push(arr);
                    }
                }
                // footer 内嵌 schema（列型=物化时的真实类型）
                let batch = RecordBatch::try_new(footer.schema.clone(), cols)
                    .map_err(|e| SqlError::internal(format!("cbf batch: {e}")))?;
                out.push(batch);
            }
        }
        Ok(out)
    }
}

