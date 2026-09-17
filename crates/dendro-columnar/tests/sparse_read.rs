//! ObjStore 稀疏读（O-3+）：掩码扫描经 get_range 取 footer 与所需块，
//! 跳列的 IO 面。字节计数断言：掩码扫描读到的字节 < 全列扫描；
//! 结果等价（非需求列 null 占位、需求列值与全读一致）。

use bytes::Bytes;
use dendro_columnar::integrate::CbfColumnar;
use dendro_core::engine::ColumnarStore;
use dendro_core::objstore::{HeadInfo, ObjResult, ObjStore};
use dendro_core::types::{ColType, SqlValue};
use dendro_core::versioned::{ColumnDef, TableSchema};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// 计数字节源：包装内层 store，统计 get/get_range 实际取回的字节
struct Counting {
    inner: Arc<dyn ObjStore>,
    bytes: AtomicU64,
    calls: AtomicU64,
    seen: RwLock<std::collections::HashSet<String>>,
}

impl ObjStore for Counting {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        let b = self.inner.get(path)?;
        self.bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.seen.write().unwrap().insert(path.to_string());
        Ok(b)
    }
    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        let b = self.inner.get_range(path, off, len)?;
        self.bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.seen.write().unwrap().insert(path.to_string());
        Ok(b)
    }
    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.inner.put(path, data)
    }
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.inner.put_if_absent(path, data)
    }
    fn delete(&self, path: &str) -> ObjResult<()> {
        self.inner.delete(path)
    }
    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>> {
        self.inner.head(path)
    }
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        self.inner.list_prefix(prefix)
    }
    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        self.inner.copy(from, to)
    }
}

fn schema5() -> TableSchema {
    TableSchema {
        name: "w".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                ty: ColType::Int64,
                nullable: true,
            },
            ColumnDef {
                name: "a".into(),
                ty: ColType::Int64,
                nullable: true,
            },
            ColumnDef {
                name: "b".into(),
                ty: ColType::Int64,
                nullable: true,
            },
            ColumnDef {
                name: "c".into(),
                ty: ColType::Int64,
                nullable: true,
            },
            ColumnDef {
                name: "note".into(),
                ty: ColType::Utf8,
                nullable: true,
            },
        ],
        pk: vec![0],
    }
}

fn write_segment(
    cbf: &CbfColumnar,
    store: &Arc<dyn ObjStore>,
    schema: &TableSchema,
    nrows: usize,
) -> dendro_core::versioned::ColSegment {
    let rows: Vec<Vec<SqlValue>> = (0..nrows)
        .map(|i| {
            vec![
                SqlValue::Int64(i as i64),
                SqlValue::Int64((i % 7) as i64),
                SqlValue::Int64((i * 2) as i64),
                SqlValue::Int64((i % 13) as i64),
                SqlValue::Utf8(format!("note-{i}-payload-payload-payload")),
            ]
        })
        .collect();
    cbf.write_segment(store, "w", schema, &rows).unwrap()
}

#[test]
fn sparse_masked_scan_reads_fewer_bytes_and_decodes_correctly() {
    let mem: Arc<dyn ObjStore> = Arc::new(dendro_core::objstore::memory::MemoryObjStore::new());
    let cbf = CbfColumnar {
        row_group_rows: 256,
    };
    let schema = schema5();
    let seg = write_segment(&cbf, &mem, &schema, 2048); // 8 个 RG

    // 全列读
    let full_store = Arc::new(Counting {
        inner: mem.clone(),
        bytes: AtomicU64::new(0),
        calls: AtomicU64::new(0),
        seen: Default::default(),
    });
    let full_probe = full_store.clone() as Arc<Counting>;
    let full_store: Arc<dyn ObjStore> = full_store;
    let batches_full = cbf
        .scan(
            &full_store,
            &schema,
            std::slice::from_ref(&seg),
            &None,
            None,
        )
        .unwrap();
    let full_bytes = full_probe.bytes.load(Ordering::Relaxed);
    let full_calls = full_probe.calls.load(Ordering::Relaxed);

    // 掩码读：id + a（b/c/note 裁剪）
    let mask = [true, true, false, false, false];
    let sparse_store = Arc::new(Counting {
        inner: mem.clone(),
        bytes: AtomicU64::new(0),
        calls: AtomicU64::new(0),
        seen: Default::default(),
    });
    let sparse_probe = sparse_store.clone() as Arc<Counting>;
    let sparse_store: Arc<dyn ObjStore> = sparse_store;
    let batches_masked = cbf
        .scan(
            &sparse_store,
            &schema,
            std::slice::from_ref(&seg),
            &None,
            Some(&mask),
        )
        .unwrap();
    let sparse_bytes = sparse_probe.bytes.load(Ordering::Relaxed);
    let sparse_calls = sparse_probe.calls.load(Ordering::Relaxed);

    assert!(full_calls == 1, "全列 = 单次整文件 get：{full_calls}");
    assert!(sparse_calls > 1, "稀疏 = 多次 get_range：{sparse_calls}");
    assert!(
        sparse_bytes < full_bytes / 2,
        "掩码（2/5 列）读字节 {} 应 < 全读 {} 的一半",
        sparse_bytes,
        full_bytes
    );

    // 值等价：需求列逐行一致；裁剪列全 null
    assert_eq!(batches_full.len(), batches_masked.len());
    let mut idx = 0i64;
    for (bf, bm) in batches_full.iter().zip(&batches_masked) {
        assert_eq!(bf.num_rows(), bm.num_rows());
        for r in 0..bf.num_rows() {
            assert_eq!(
                bf.column(0)
                    .as_any()
                    .downcast_ref::<arrow::array::Int64Array>()
                    .unwrap()
                    .value(r),
                bm.column(0)
                    .as_any()
                    .downcast_ref::<arrow::array::Int64Array>()
                    .unwrap()
                    .value(r),
                "id 列（需求）值一致 @ {idx}"
            );
            assert_eq!(
                bf.column(1)
                    .as_any()
                    .downcast_ref::<arrow::array::Int64Array>()
                    .unwrap()
                    .value(r),
                bm.column(1)
                    .as_any()
                    .downcast_ref::<arrow::array::Int64Array>()
                    .unwrap()
                    .value(r),
                "a 列（需求）值一致 @ {idx}"
            );
            assert!(bm.column(4).is_null(r), "note 列（裁剪）应 null @ {idx}");
            idx += 1;
        }
    }
    assert_eq!(idx, 2048);
}
