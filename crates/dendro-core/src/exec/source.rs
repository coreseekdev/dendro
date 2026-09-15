//! v2c-4（方向2）：Source 流式化——MainPlusDelta 三路惰性归并游标
//!（ir-spec 04 §4；载荷 = 行路径 `Vec<Vec<SqlValue>>`，Chunk 载荷版
//! 随 AP ChunkPlane 全面化落地）。
//!
//! 替换 try_ap_scan 的**全量 BTreeMap 物化**（全表行以 SqlValue 形式
//! 常驻）为批量迭代器：每次产 ≤ ROW_BATCH 行，消费方按批拉取。
//! 语义不变量（与原实现逐条对齐，差分锚点 = dispatch_differential
//! 的 main vs fallback）：
//! - 同 key 优先级：txn > overlay > 新段 > 旧段（尾巴 BTreeMap 预并，
//!   段侧 newest-wins 堆归并）；
//! - 输出恒 pk 字节序（段内 pk 有序由 CBF 写路径保证；段间/尾巴由
//!   堆与键比较归并）；
//! - col_deletes 只抑制**段源**行（overlay/txn 的重插不受影响）；
//! - pushdown_limit 早停（产出计数达 cap 后迭代终止）。
//!
//! 内存形态：段 Arrow 批（CBF 读取路径现状，段级物化）+ 当前段解码批
//! + 尾巴键值（行字节，不解码）。全表 SqlValue 物化消除。

use crate::error::{Result, SqlError};
use crate::exec::pipeline::ROW_BATCH;
use crate::types::SqlValue;
use crate::versioned::TableSchema;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashSet};
use std::sync::Arc;

/// 尾巴条目：行键 → 行字节覆盖（Some）/ 墓碑（None）
type TailEntry = (Vec<u8>, Option<Arc<Vec<u8>>>);

/// 段游标：一段的 Arrow 批序列 + 懒解码（进入新批时整批解码）
struct SegCursor {
    /// 段序（col_segments 索引；大 = 新——newest-wins 的优先级轴）
    idx: usize,
    batches: Vec<arrow::record_batch::RecordBatch>,
    batch_i: usize,
    /// 当前批解码行
    rows: Vec<Vec<SqlValue>>,
    row_i: usize,
    /// 当前行键（encode_key(pk)）——游标有效期内非空
    key: Vec<u8>,
}

impl SegCursor {
    fn new(
        idx: usize,
        batches: Vec<arrow::record_batch::RecordBatch>,
        schema: &TableSchema,
        pkc: usize,
    ) -> Result<Option<Self>> {
        let mut c = SegCursor {
            idx,
            batches,
            batch_i: 0,
            rows: Vec::new(),
            row_i: 0,
            key: Vec::new(),
        };
        // 定位首个有效行（空批跳过）
        if !c.step(schema, pkc)? {
            return Ok(None);
        }
        Ok(Some(c))
    }

    /// 推进到下一行；行耗尽则装载下一批（batch_i = 已装载数），段耗尽
    /// 返回 false。
    fn step(&mut self, schema: &TableSchema, pkc: usize) -> Result<bool> {
        loop {
            if self.row_i < self.rows.len() {
                let r = &self.rows[self.row_i];
                if pkc >= r.len() {
                    self.row_i += 1;
                    continue; // 列缺失守卫（原实现同款）
                }
                self.key = crate::format::row::encode_key(&[r[pkc].clone()]);
                return Ok(true);
            }
            if self.batch_i >= self.batches.len() {
                return Ok(false);
            }
            self.rows =
                crate::sql::scan::rows_from_batches(&self.batches[self.batch_i], schema)?;
            self.batch_i += 1;
            self.row_i = 0;
        }
    }
}

/// 三路惰性归并源（Iterator 协议 = Source::open 的返回形态，04 §4）。
/// next() 产 ≤ ROW_BATCH 行的批；None = 流尽或 limit 早停。
pub struct MainPlusDeltaSource {
    cursors: Vec<Option<SegCursor>>,
    /// 归并堆：Reverse 使 pop 序 = key 升序；同 key 段序大（新）者先出
    heap: BinaryHeap<Reverse<(Vec<u8>, Reverse<usize>)>>,
    /// 尾巴（overlay 预并 txn 写；键序）：Some=行字节覆盖，None=墓碑
    tail: Vec<TailEntry>,
    tail_pos: usize,
    /// col_deletes（行键 hex 解码）——只抑制段源
    deletes: HashSet<Vec<u8>>,
    schema: TableSchema,
    pkc: usize,
    ncols: usize,
    /// 产出行数（pushdown_limit 早停）
    emitted: usize,
    limit: Option<usize>,
    /// 游标推进期的延迟错误（Iterator::next 单次返回）
    pending_err: Option<SqlError>,
    done: bool,
}

impl MainPlusDeltaSource {
    /// 段批（segment_batches 与 segments 按索引对齐）、overlay（memtx
    /// snapshot_rows）、txn_writes（显式事务写，键序无关——预并时覆盖）
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        segment_batches: Vec<Vec<arrow::record_batch::RecordBatch>>,
        overlay: BTreeMap<Vec<u8>, Option<Arc<Vec<u8>>>>,
        txn_writes: Vec<TailEntry>,
        deletes: HashSet<Vec<u8>>,
        schema: TableSchema,
        limit: Option<usize>,
    ) -> Result<Self> {
        let pkc = schema.pk.first().map(|i| *i as usize).unwrap_or(0);
        let ncols = schema.columns.len();
        // 尾巴预并：overlay 先入，txn 后入（写覆盖优先级最高）
        let mut tail_map = overlay;
        for (k, v) in txn_writes {
            tail_map.insert(k, v);
        }
        let tail: Vec<_> = tail_map.into_iter().collect();
        let mut cursors = Vec::with_capacity(segment_batches.len());
        let mut heap = BinaryHeap::new();
        for (idx, batches) in segment_batches.into_iter().enumerate() {
            if let Some(c) = SegCursor::new(idx, batches, &schema, pkc)? {
                heap.push(Reverse((c.key.clone(), Reverse(c.idx))));
                cursors.push(Some(c));
            } else {
                cursors.push(None);
            }
        }
        Ok(Self {
            cursors,
            heap,
            tail,
            tail_pos: 0,
            deletes,
            schema,
            pkc,
            ncols,
            emitted: 0,
            limit,
            pending_err: None,
            done: false,
        })
    }

    /// 弹出堆顶并推进对应游标（耗尽/出错则游标下线）。返回被弹出条目
    /// 的 (key, 行)。pending_err 置位时返回值仍有效（行已取）。
    fn pop_heap(&mut self) -> Option<(Vec<u8>, Vec<SqlValue>)> {
        let Reverse((key, Reverse(ci))) = self.heap.pop()?;
        let mut row = Vec::new();
        let mut offline = false;
        if let Some(cur) = self.cursors[ci].as_mut() {
            debug_assert_eq!(cur.key, key);
            let mut r = std::mem::take(&mut cur.rows[cur.row_i]);
            r.resize(self.ncols, SqlValue::Null);
            row = r;
            match cur.step(&self.schema, self.pkc) {
                Ok(true) => {
                    let nk = cur.key.clone();
                    self.heap.push(Reverse((nk, Reverse(ci))));
                }
                Ok(false) => offline = true,
                Err(e) => {
                    self.pending_err = Some(e);
                    offline = true;
                }
            }
        }
        if offline {
            self.cursors[ci] = None;
        }
        Some((key, row))
    }

    fn peek_key(&self) -> Option<&Vec<u8>> {
        self.heap.peek().map(|Reverse((k, _))| k)
    }

    /// 丢弃堆中键 = k 的全部条目（同键旧段副本；游标各自推进）
    fn discard_heap_key(&mut self, k: &[u8]) -> Result<()> {
        while self.peek_key().is_some_and(|hk| hk == k) {
            self.pop_heap();
            if let Some(e) = self.pending_err.take() {
                return Err(e);
            }
        }
        Ok(())
    }

    /// 尾巴行解码（字节 → 行；补列宽）
    fn decode_tail(&self, bytes: &[u8]) -> Result<Vec<SqlValue>> {
        let mut r = crate::sql::scan::row_from_bytes(&self.schema, bytes)?;
        r.resize(self.ncols, SqlValue::Null);
        Ok(r)
    }
}

impl Iterator for MainPlusDeltaSource {
    type Item = Result<Vec<Vec<SqlValue>>>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.pending_err.take() {
            self.done = true;
            return Some(Err(e));
        }
        if self.done || self.limit == Some(self.emitted) {
            return None;
        }
        let mut batch: Vec<Vec<SqlValue>> = Vec::with_capacity(ROW_BATCH);
        let cap = self.limit.unwrap_or(usize::MAX) - self.emitted;
        while batch.len() < ROW_BATCH.min(cap) {
            let seg_key = self.peek_key().cloned();
            let tail_key = self.tail.get(self.tail_pos).map(|(k, _)| k.clone());
            match (seg_key, tail_key) {
                (None, None) => break,
                // 尾巴键 ≤ 段键：尾巴胜（同键时段侧版本全部丢弃——
                // overlay/txn 的写覆盖段源；墓碑则该键整体不可见）
                (Some(sk), Some(tk)) if tk <= sk => {
                    let v = self.tail[self.tail_pos].1.clone();
                    self.tail_pos += 1;
                    if tk == sk {
                        if let Err(e) = self.discard_heap_key(&sk) {
                            self.done = true;
                            return Some(Err(e));
                        }
                    }
                    match v {
                        Some(bytes) => match self.decode_tail(&bytes) {
                            Ok(r) => batch.push(r),
                            Err(e) => {
                                self.done = true;
                                return Some(Err(e));
                            }
                        },
                        None => continue, // 墓碑
                    }
                }
                (None, Some(_)) => {
                    let v = self.tail[self.tail_pos].1.clone();
                    self.tail_pos += 1;
                    match v {
                        Some(bytes) => match self.decode_tail(&bytes) {
                            Ok(r) => batch.push(r),
                            Err(e) => {
                                self.done = true;
                                return Some(Err(e));
                            }
                        },
                        None => continue,
                    }
                }
                // 段键严格更小：段侧产出（堆序保证首个弹出即最新段；
                // 其余同键条目 = 旧段副本，丢弃）
                (Some(sk), _) => {
                    // let-else 而非 `?`（评审 P2：`?` 会静默终止迭代并丢弃
                    // 当次已累积批；不可达路径也按防御写法收口）
                    let Some((k, row)) = self.pop_heap() else {
                        break;
                    };
                    if let Some(e) = self.pending_err.take() {
                        self.done = true;
                        return Some(Err(e));
                    }
                    if let Err(e) = self.discard_heap_key(&k) {
                        self.done = true;
                        return Some(Err(e));
                    }
                    if self.deletes.contains(&k) {
                        let _ = sk;
                        continue; // col_deletes 抑制段源行
                    }
                    batch.push(row);
                }
            }
        }
        self.emitted += batch.len();
        if batch.is_empty() {
            self.done = true;
            return None; // 流尽（零行不推——D3）；批空即 done
        }
        Some(Ok(batch))
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;

    fn schema_1pk() -> TableSchema {
        // id BIGINT pk, v BIGINT
        TableSchema {
            name: "t".into(),
            columns: vec![
                crate::versioned::ColumnDef {
                    name: "id".into(),
                    ty: crate::types::ColType::Int64,
                    nullable: true,
                },
                crate::versioned::ColumnDef {
                    name: "v".into(),
                    ty: crate::types::ColType::Int64,
                    nullable: true,
                },
            ],
            pk: vec![0],
        }
    }

    fn i64_batch(pairs: &[(i64, i64)]) -> arrow::record_batch::RecordBatch {
        use arrow::array::{Int64Array, RecordBatch};
        let ids: Int64Array = pairs.iter().map(|(a, _)| *a).collect();
        let vs: Int64Array = pairs.iter().map(|(_, b)| *b).collect();
        RecordBatch::try_from_iter(vec![
            ("id", Arc::new(ids) as Arc<dyn arrow::array::Array>),
            ("v", Arc::new(vs) as Arc<dyn arrow::array::Array>),
        ])
        .unwrap()
    }

    fn key_of(id: i64) -> Vec<u8> {
        crate::format::row::encode_key(&[SqlValue::Int64(id)])
    }

    fn rows_i64(rows: &[Vec<SqlValue>]) -> Vec<(i64, i64)> {
        rows.iter()
            .map(|r| match (&r[0], &r[1]) {
                (SqlValue::Int64(a), SqlValue::Int64(b)) => (*a, *b),
                (SqlValue::Int64(a), _) => (*a, i64::MIN),
                _ => (i64::MIN, i64::MIN),
            })
            .collect()
    }

    /// 三段（含同键跨段覆盖）+ 尾巴（墓碑/覆盖）+ deletes 的全语义矩阵
    #[test]
    fn three_way_merge_priority_and_order() {
        let s1 = i64_batch(&[(1, 10), (3, 30)]); // 旧段
        let s2 = i64_batch(&[(2, 200), (3, 33), (5, 50)]); // 新段：3 覆盖旧段
        let mut overlay = BTreeMap::new();
        overlay.insert(key_of(2), None); // 墓碑：段源 2 整体不可见
        let deletes = HashSet::new();
        let src = MainPlusDeltaSource::new(
            vec![vec![s1], vec![s2]],
            overlay,
            vec![(key_of(5), None)], // txn 删除 5
            deletes,
            schema_1pk(),
            None,
        )
        .unwrap();
        let rows: Result<Vec<Vec<Vec<SqlValue>>>> = src.collect();
        let all: Vec<Vec<SqlValue>> = rows.unwrap().into_iter().flatten().collect();
        // 1(旧段) 2(墓碑→无) 3(新段 33) 5(txn 删→无)
        assert_eq!(rows_i64(&all), vec![(1, 10), (3, 33)]);
    }

    /// 段源被 col_deletes 抑制，但尾巴重插同键行不受影响
    #[test]
    fn deletes_suppress_segment_but_not_tail() {
        let s1 = i64_batch(&[(7, 70), (9, 90)]);
        let mut deletes = HashSet::new();
        deletes.insert(key_of(7)); // 段源 7 删除
        let src = MainPlusDeltaSource::new(
            vec![vec![s1]],
            BTreeMap::new(),
            vec![],
            deletes,
            schema_1pk(),
            None,
        )
        .unwrap();
        let all: Vec<Vec<SqlValue>> = src
            .collect::<Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(rows_i64(&all), vec![(9, 90)]);
    }

    /// limit 早停：只产 cap 行
    #[test]
    fn limit_early_stop() {
        let s1 = i64_batch(&[(1, 1), (2, 2), (3, 3), (4, 4)]);
        let src = MainPlusDeltaSource::new(
            vec![vec![s1]],
            BTreeMap::new(),
            vec![],
            HashSet::new(),
            schema_1pk(),
            Some(2),
        )
        .unwrap();
        let all: Vec<Vec<SqlValue>> = src
            .collect::<Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(rows_i64(&all), vec![(1, 1), (2, 2)]);
    }
}
