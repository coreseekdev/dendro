//! 内存事务引擎（SPEC 04）：OCC MVCC，分支内单写者提交，无锁快照读。
//!
//! 数据结构（P1 紧凑布局，HANA delta store 思想）：
//! - 每表 64 shard；shard = **排序主区**（`(key, ts)` 升序的不可变紧凑
//!   数组，二分查找）+ **追加增量区**（未排序 Vec，线性扫；满阈值归并
//!   进主区）。此前是 `BTreeMap<Box<[u8]>, Arc<VersionVec>>`——每条
//!   5 次堆分配 + 节点分散，1M 键工作集下点查 cache miss 主导
//!   （实测 3.4µs vs 100k 键 0.35µs，见 memres 探针）；扁平 Cell 数组
//!   连续驻留，二分扫描对 prefetcher 友好。
//! - 归并摊销：`delta_max = max(DELTA_MIN, sorted.len()/DELTA_RATIO)`
//!   ——主区每次归并增长 1+1/RATIO，翻倍仅 RATIO·ln2 次归并，
//!   摊销每写 O(RATIO) Cell 拷贝（对标 Vec 倍增摊销）。
//! - checkpoint 后 ts ≤ ckpt_seq 的历史版本被释放（内存有界 = checkpoint
//!   窗口写入量）。
//!
//! 红线执行：无 Rc/RefCell；VerCell 不可变以 Arc 共享；锁只出现在
//! 写路径（安装/归并）与 checkpoint 截断。**锁序：归并/截断持 delta 写锁
//! 后再拿 sorted 写锁（唯一嵌套）；读路径一律先 clone sorted 的 Arc 并
//! 释放读锁、再拿 delta 读锁——无嵌套，无死锁面**。

use crate::error::{Result, SqlError};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// 紧凑键（P0-C）：≤23B 内联零分配（rowid/短复合键常态）；超长回落
/// Arc。**比较恒为字节序**（手写 Ord——enum derive 会先比 tag，破坏
/// 与树/BTreeMap 键序的全局一致性）。内联动机：sorted 二分每步若解
/// 引用指针（旧 Arc key），64 分片 × log n 次 cache miss 曾使
/// range100 慢 5×（局部性 > 扇出，ADMS'24 实证）。
#[derive(Clone, Debug)]
pub enum SmolKey {
    Inline(u8, [u8; 23]),
    Heap(Arc<[u8]>),
}

impl SmolKey {
    pub fn from_vec(v: Vec<u8>) -> Self {
        if v.len() <= 23 {
            let mut buf = [0u8; 23];
            buf[..v.len()].copy_from_slice(&v);
            Inline(v.len() as u8, buf)
        } else {
            Heap(v.into())
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Inline(n, b) => &b[..*n as usize],
            Heap(b) => b,
        }
    }
}

use SmolKey::{Heap, Inline};

impl PartialEq for SmolKey {
    fn eq(&self, o: &Self) -> bool {
        self.as_bytes() == o.as_bytes()
    }
}
impl Eq for SmolKey {}
impl PartialOrd for SmolKey {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for SmolKey {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.as_bytes().cmp(o.as_bytes())
    }
}
impl std::borrow::Borrow<[u8]> for SmolKey {
    fn borrow(&self) -> &[u8] {
        self.as_bytes()
    }
}
impl std::hash::Hash for SmolKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state)
    }
}

/// 扁平版本条目：(key, ts) 排序键 + 值 Arc（P0-A 共享分配）。
/// 多版本同 key = 相邻多条 Cell；无 per-entry 间接层。
pub struct VerCell {
    pub key: SmolKey,
    pub ts: u64,
    /// None = tombstone（删除）
    pub val: Option<Arc<[u8]>>,
}

/// 追加增量区的最小容量（低于此不触发归并——小负载全走 delta）
const DELTA_MIN: usize = 4096;
/// 读触发归并阈值：范围/全表扫描前 delta 超过此数先归并（无序 delta
/// 只能线性扫——12 万驻留条曾使 range100.mem 崩到 1.6k，A/B 实证）。
/// 点查不受影响（哈希索引）。归并在写路径摊销之外提供第二收敛点。
const DELTA_READ_MERGE: usize = 1024;
/// 归并摊销比：delta_max = DELTA_MIN + sorted.len()/RATIO——主区每次
/// 归并增长 1+1/RATIO，翻倍仅 ~RATIO·ln2 次归并，摊销每写 O(RATIO) 拷贝
const DELTA_RATIO: usize = 4;

/// 追加增量区：cells 无序追加 + key→下标哈希索引（点查 O(1)，对标
/// HANA delta store 的辅助索引）。索引键与 Cell 共享同一 key Arc。
#[derive(Default)]
struct DeltaArea {
    cells: Vec<VerCell>,
    index: HashMap<SmolKey, Vec<u32>>,
}

impl DeltaArea {
    fn push(&mut self, c: VerCell) {
        let idx = self.cells.len() as u32;
        self.index.entry(c.key.clone()).or_default().push(idx);
        self.cells.push(c);
    }
}

struct Shard {
    /// (key, ts) 升序不可变快照（读者在 delta 读锁保护下 clone Arc，二分）
    sorted: RwLock<Arc<Vec<VerCell>>>,
    /// 追加写增量（哈希索引点查；满阈值归并进 sorted）
    delta: RwLock<DeltaArea>,
}

pub const NSHARD: usize = 64;

fn shard_of(key: &[u8]) -> usize {
    let h = xxhash_rust::xxh3::xxh3_64(key);
    (h as usize) & (NSHARD - 1)
}

/// delta 内该 key 可见性最佳的条目（ts <= snapshot 的最大 ts）
fn delta_best<'a>(d: &'a DeltaArea, key: &[u8], snapshot: u64) -> Option<&'a VerCell> {
    let idxs = d.index.get(key)?;
    idxs.iter()
        .map(|&i| &d.cells[i as usize])
        .filter(|c| c.ts <= snapshot)
        .max_by_key(|c| c.ts)
}

fn delta_latest(d: &DeltaArea, key: &[u8]) -> Option<u64> {
    d.index.get(key).map(|idxs| {
        idxs
            .iter()
            .map(|&i| d.cells[i as usize].ts)
            .max()
            .unwrap_or(0)
    })
}

/// 读触发归并：范围/全表扫描前的第二收敛点（delta 无序只能线性扫）
fn merge_if_large(sh: &Shard) {
    if sh.delta.read().cells.len() > DELTA_READ_MERGE {
        let mut d = sh.delta.write();
        if d.cells.len() > DELTA_READ_MERGE {
            merge_shard(sh, &mut d);
        }
    }
}

/// 归并：delta 全量排序并入 sorted（新快照），重建索引。调用方持 delta 写锁。
fn merge_shard(sh: &Shard, d: &mut DeltaArea) {
    let mut cells = std::mem::take(&mut d.cells);
    d.index.clear();
    cells.sort_by(|a, b| (a.key.as_bytes(), a.ts).cmp(&(b.key.as_bytes(), b.ts)));
    let old = sh.sorted.read().clone();
    let mut out: Vec<VerCell> = Vec::with_capacity(old.len() + cells.len());
    let mut i = 0usize;
    let mut dit = cells.into_iter().peekable();
    while i < old.len() {
        let Some(dc) = dit.peek() else { break };
        if (old[i].key.as_bytes(), old[i].ts) <= (dc.key.as_bytes(), dc.ts) {
            out.push(clone_cell(&old[i]));
            i += 1;
        } else {
            // delta 侧 move（零拷贝）：仅 sorted 侧因不可变共享需 Arc 拷贝
            out.push(dit.next().unwrap());
        }
    }
    out.extend(old[i..].iter().map(clone_cell));
    out.extend(dit);
    *sh.sorted.write() = Arc::new(out);
}

/// 从旧快照拷贝一条 Cell（key/val 均 Arc 共享——仅两次原子递增，零分配）
fn clone_cell(c: &VerCell) -> VerCell {
    VerCell {
        key: c.key.clone(),
        ts: c.ts,
        val: c.val.clone(),
    }
}

/// 一张表的内存态
pub struct TableMem {
    shards: Vec<Shard>,
}

impl Default for TableMem {
    fn default() -> Self {
        Self {
            shards: (0..NSHARD)
                .map(|_| Shard {
                    sorted: RwLock::new(Arc::new(Vec::new())),
                    delta: RwLock::new(DeltaArea::default()),
                })
                .collect(),
        }
    }
}

impl TableMem {
    /// 点查：snapshot 可见的最新版本（None 含墓碑遮蔽与不存在两种）
    pub fn get(&self, key: &[u8], snapshot: u64) -> Option<Arc<[u8]>> {
        let sh = &self.shards[shard_of(key)];
        // 锁序 delta→sorted（与归并同序）：delta 读锁保护两区一致视图
        let d = sh.delta.read();
        let sorted = sh.sorted.read().clone();
        let (mut best_ts, mut best_val, mut have) = (0u64, None, false);
        if let Some(c) = delta_best(&d, key, snapshot) {
            best_ts = c.ts;
            best_val = c.val.clone();
            have = true;
        }
        // sorted 二分：最后一个 (key', ts') <= (key, snapshot)
        let idx = sorted.partition_point(|c| (c.key.as_bytes(), c.ts) <= (key, snapshot));
        if idx > 0 && sorted[idx - 1].key.as_bytes() == key {
            let c = &sorted[idx - 1];
            if !have || c.ts > best_ts {
                best_ts = c.ts;
                best_val = c.val.clone();
                have = true;
            }
        }
        if have {
            best_val
        } else {
            None
        }
    }

    /// 可见版本 ts（存在性/冲突检测用）
    pub fn visible_ts(&self, key: &[u8], snapshot: u64) -> Option<u64> {
        let sh = &self.shards[shard_of(key)];
        let d = sh.delta.read();
        let sorted = sh.sorted.read().clone();
        let mut best = delta_best(&d, key, snapshot).map(|c| c.ts);
        let idx = sorted.partition_point(|c| (c.key.as_bytes(), c.ts) <= (key, snapshot));
        if idx > 0 && sorted[idx - 1].key.as_bytes() == key {
            let ts = sorted[idx - 1].ts;
            if best.is_none_or(|b| ts > b) {
                best = Some(ts);
            }
        }
        best
    }

    /// 最新版本 ts（含未提交窗口外的全部已提交版本；OCC 写写冲突检测）
    pub fn latest_ts(&self, key: &[u8]) -> Option<u64> {
        let sh = &self.shards[shard_of(key)];
        let d = sh.delta.read();
        let sorted = sh.sorted.read().clone();
        let mut lt = delta_latest(&d, key);
        let idx = sorted.partition_point(|c| c.key.as_bytes() <= key);
        if idx > 0 && sorted[idx - 1].key.as_bytes() == key {
            lt = lt.max(Some(sorted[idx - 1].ts));
        }
        lt
    }

    /// 提交安装：key 的新版本（调用方已持有分支 commit 锁，见 engine::commit_tx）。
    /// 追加进 delta 区（O(1)）；delta 越自适应阈值时归并进 sorted 主区。
    /// ts 严格递增由 commit_mu 串行化 + debug_assert 双保险（重复 ts = seq 分配缺陷）。
    pub fn install(&self, key: Vec<u8>, ts: u64, val: Option<Arc<[u8]>>) {
        let sh = &self.shards[shard_of(&key)];
        let mut d = sh.delta.write();
        debug_assert!(delta_latest(&d, &key).is_none_or(|t| t < ts));
        let slen = sh.sorted.read().len();
        d.push(VerCell {
            key: SmolKey::from_vec(key),
            ts,
            val,
        });
        if d.cells.len() > DELTA_MIN + slen / DELTA_RATIO {
            merge_shard(sh, &mut d);
        }
    }

    /// checkpoint 截断：释放 ts <= ckpt_seq 的历史（若最新也 ≤，则整键移除）
    pub fn truncate_to(&self, ckpt_seq: u64) -> usize {
        let mut freed = 0usize;
        for sh in &self.shards {
            let mut d = sh.delta.write();
            let mut cells: Vec<VerCell> = Vec::with_capacity(sh.sorted.read().len() + d.cells.len());
            {
                let s = sh.sorted.read();
                cells.extend(s.iter().map(clone_cell));
            }
            cells.append(&mut d.cells);
            d.index.clear();
            cells.sort_by(|a, b| (a.key.as_bytes(), a.ts).cmp(&(b.key.as_bytes(), b.ts)));
            let mut out: Vec<VerCell> = Vec::with_capacity(cells.len());
            let mut i = 0usize;
            while i < cells.len() {
                let mut j = i;
                while j < cells.len() && cells[j].key.as_bytes() == cells[i].key.as_bytes() {
                    j += 1;
                }
                if cells[j - 1].ts > ckpt_seq {
                    // 保留 > ckpt 的尾部（ts 升序，前缀 = 截断历史）
                    let keep_from = i + cells[i..j].partition_point(|c| c.ts <= ckpt_seq);
                    if keep_from > i {
                        freed += 1;
                    }
                    out.extend(cells[keep_from..j].iter().map(clone_cell));
                } else {
                    freed += 1; // 整键移除
                }
                i = j;
            }
            *sh.sorted.write() = Arc::new(out);
        }
        freed
    }

    /// 全表可见行快照（TP 扫描 overlay 用）。sorted 连续流扫 + delta
    /// 覆盖（delta 条目 ts 恒大于 sorted 同 key 条目——归并搬迁保序）
    pub fn snapshot_rows(&self, snapshot: u64) -> BTreeMap<Vec<u8>, Option<Arc<[u8]>>> {
        let mut out = BTreeMap::new();
        for sh in &self.shards {
            merge_if_large(sh);
            let d = sh.delta.read();
            let s = sh.sorted.read().clone();
            let mut i = 0usize;
            while i < s.len() {
                let mut j = i;
                while j < s.len() && s[j].key.as_bytes() == s[i].key.as_bytes() {
                    j += 1;
                }
                let idx = i + s[i..j].partition_point(|c| c.ts <= snapshot);
                if idx > i {
                    out.insert(s[idx - 1].key.as_bytes().to_vec(), s[idx - 1].val.clone());
                }
                i = j;
            }
            for c in d.cells.iter() {
                if c.ts <= snapshot {
                    out.insert(c.key.as_bytes().to_vec(), c.val.clone());
                }
            }
        }
        out
    }

    /// 条数（Cell 口径 = Σ键×版本；单版本场景 = 键数——census）
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|sh| {
                let d = sh.delta.read();
                sh.sorted.read().len() + d.cells.len()
            })
            .sum()
    }

    /// 快照时点是否存在任何可见条目（Put 或墓碑均算——两者都会改变
    /// 全表聚合结果）。首命中即返。
    pub fn has_visible_rows(&self, snapshot: u64) -> bool {
        for sh in &self.shards {
            merge_if_large(sh);
            let d = sh.delta.read();
            if d.cells.iter().any(|c| c.ts <= snapshot) {
                return true;
            }
            let s = sh.sorted.read().clone();
            let mut i = 0usize;
            while i < s.len() {
                let mut j = i;
                while j < s.len() && s[j].key.as_bytes() == s[i].key.as_bytes() {
                    j += 1;
                }
                if s[i..j].iter().any(|c| c.ts <= snapshot) {
                    return true;
                }
                i = j;
            }
        }
        false
    }

    /// 范围可见性快照（P2-6g）：只物化 `[start, end)` 键区间的 overlay。
    /// 可见性判定与 [`Self::snapshot_rows`] 完全一致。
    pub fn snapshot_rows_in_range(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        snapshot: u64,
    ) -> BTreeMap<Vec<u8>, Option<Arc<[u8]>>> {
        let in_range = |k: &[u8]| {
            (start.is_none_or(|s| k >= s)) && (end.is_none_or(|e| k < e))
        };
        let mut out = BTreeMap::new();
        for sh in &self.shards {
            merge_if_large(sh);
            let d = sh.delta.read();
            let s = sh.sorted.read().clone();
            // sorted 有序：二分定位 start，扫到 end 出界即停（旧 BTreeMap
            // 的 range(a..e) 语义——线性从头扫曾使 range100.mem 全表扫/查询）
            let mut i = match start {
                Some(st) => s.partition_point(|c| c.key.as_bytes() < st),
                None => 0,
            };
            while i < s.len() {
                if let Some(e) = end {
                    if s[i].key.as_bytes() >= e {
                        break;
                    }
                }
                let mut j = i;
                while j < s.len() && s[j].key.as_bytes() == s[i].key.as_bytes() {
                    j += 1;
                }
                let idx = i + s[i..j].partition_point(|c| c.ts <= snapshot);
                if idx > i {
                    out.insert(s[idx - 1].key.as_bytes().to_vec(), s[idx - 1].val.clone());
                }
                i = j;
            }
            for c in d.cells.iter() {
                if c.ts <= snapshot && in_range(c.key.as_bytes()) {
                    out.insert(c.key.as_bytes().to_vec(), c.val.clone());
                }
            }
        }
        out
    }

    pub fn row_count_visible(&self, snapshot: u64) -> usize {
        self.snapshot_rows(snapshot)
            .values()
            .filter(|v| v.is_some())
            .count()
    }
}

/// 分支内存表集合：table_id → TableMem
#[derive(Default)]
pub struct BranchMem {
    tables: RwLock<HashMap<u32, Arc<TableMem>>>,
}

impl BranchMem {
    /// 全表键数（census——memtx.entries 的口径）
    pub fn len(&self) -> usize {
        self.tables.read().values().map(|t| t.len()).sum()
    }

    pub fn table(&self, table_id: u32) -> Arc<TableMem> {
        {
            let g = self.tables.read();
            if let Some(t) = g.get(&table_id) {
                return t.clone();
            }
        }
        let mut g = self.tables.write();
        g.entry(table_id)
            .or_insert_with(|| Arc::new(TableMem::default()))
            .clone()
    }

    /// 恢复不变量校验（debug/test 构建；S3 WAL 调研 P0-2）：
    /// 全部表全部键的已安装版本 ts ≤ bound——"已安装集 ⊆ 写历史前缀"
    /// 的本地可判形式（bound = 回放前沿 watermark）。流式扫描
    /// （O(n) 时间 / O(1) 额外内存）；违例返回首条证据
    pub fn debug_check_ts_bound(&self, bound: u64) -> std::result::Result<(), String> {
        let tables = self.tables.read();
        for (tid, t) in tables.iter() {
            for sh in t.shards.iter() {
                // 全 Cell 流式扫：任一条目超界即违例（sorted+delta 双区）
                let mut check = |cells: &[VerCell]| -> std::result::Result<(), String> {
                    for c in cells {
                        if c.ts > bound {
                            return Err(format!(
                                "table {tid} key {:02x?}..: installed ts {} > bound {}",
                                &c.key.as_bytes()[..c.key.as_bytes().len().min(8)],
                                c.ts,
                                bound
                            ));
                        }
                    }
                    Ok(())
                };
                check(&sh.sorted.read())?;
                let d = sh.delta.read();
                check(&d.cells)?;
            }
        }
        Ok(())
    }

    pub fn remove_table(&self, table_id: u32) {
        self.tables.write().remove(&table_id);
    }

    pub fn truncate_all(&self, ckpt_seq: u64) -> usize {
        let g = self.tables.read();
        g.values().map(|t| t.truncate_to(ckpt_seq)).sum()
    }
}

/// 会话本地事务：读快照 + 写集
pub struct Txn {
    pub snapshot: u64,
    /// (table_id, key) → 写
    pub writes: BTreeMap<(u32, Vec<u8>), crate::prolly::Mutation>,
    /// 显式事务标记
    pub explicit: bool,
    /// 显式事务冻结的 catalog 根（BEGIN 时的树状态；读路径专用——写路径仍
    /// 按当前 head 解析，第七轮 R7-3）
    pub head_root: Option<crate::format::hash::Hash>,
}

impl Txn {
    pub fn new(snapshot: u64) -> Self {
        Self {
            snapshot,
            writes: BTreeMap::new(),
            explicit: false,
            head_root: None,
        }
    }
    pub fn put(&mut self, table_id: u32, key: Vec<u8>, val: Vec<u8>) {
        self.writes
            .insert((table_id, key), crate::prolly::Mutation::Put(val.into()));
    }
    pub fn delete(&mut self, table_id: u32, key: Vec<u8>) {
        self.writes
            .insert((table_id, key), crate::prolly::Mutation::Delete);
    }
    /// 同 key 后写覆盖前写：读取时写集优先
    pub fn local_get(&self, table_id: u32, key: &[u8]) -> Option<Option<Arc<[u8]>>> {
        match self.writes.get(&(table_id, key.to_vec())) {
            Some(crate::prolly::Mutation::Put(v)) => Some(Some(v.clone())),
            Some(crate::prolly::Mutation::Delete) => Some(None),
            None => None,
        }
    }
}

/// 提交验证 + 安装（SPEC 04 §3）。串行化由调用方的 commit 锁保证。
/// OCC 裁决：只验证不安装（P2' 管线 Phase 1）。**必须在 commit_mu 内调用**
/// ——验证结果到 install 之间无并发写者，裁决不会被作废。
/// 写写冲突：任一 key 的最新版本 ts > 快照 ⇒ 别的提交已抢跑（first-committer-wins）。
pub fn validate_only(
    mem: &BranchMem,
    tables_in_txn: &[(u32, Arc<TableMem>)],
    txn: &Txn,
) -> Result<()> {
    for (table_id, key) in txn.writes.keys() {
        let tm = tables_in_txn
            .iter()
            .find(|(id, _)| id == table_id)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| mem.table(*table_id));
        if let Some(latest) = tm.latest_ts(key) {
            if latest > txn.snapshot {
                return Err(SqlError::serialization(format!(
                    "tuple updated by concurrent transaction (key {} bytes, their ts={latest}, our snapshot={})",
                    key.len(),
                    txn.snapshot
                )));
            }
        }
    }
    Ok(())
}

/// 安装写集到 memtx（P2' 管线 Phase 3，**必须在 durable 之后调用**——
/// 此前 install 先于 WAL，WAL 失败时未提交数据可见 + 重启后幽灵行）。
pub fn install(
    mem: &BranchMem,
    tables_in_txn: &[(u32, Arc<TableMem>)],
    txn: &Txn,
    commit_seq: u64,
) {
    for ((table_id, key), m) in &txn.writes {
        let tm = tables_in_txn
            .iter()
            .find(|(id, _)| id == table_id)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| mem.table(*table_id));
        match m {
            // P0-A：值 Arc 共享——install 与写集持同一分配（此前整份克隆）
            crate::prolly::Mutation::Put(v) => {
                tm.install(key.clone(), commit_seq, Some(v.clone()))
            }
            crate::prolly::Mutation::Delete => tm.install(key.clone(), commit_seq, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_visibility_and_truncate() {
        let t = TableMem::default();
        t.install(b"k".to_vec(), 5, Some(b"a".to_vec().into()));
        t.install(b"k".to_vec(), 8, Some(b"b".to_vec().into()));
        t.install(b"k".to_vec(), 12, None);
        assert!(t.get(b"k", 4).is_none());
        assert_eq!(&*t.get(b"k", 5).unwrap(), b"a");
        assert_eq!(&*t.get(b"k", 8).unwrap(), b"b");
        assert_eq!(t.get(b"k", 12), None); // tombstone
        assert_eq!(t.get(b"k", 100), None);
        assert_eq!(t.latest_ts(b"k"), Some(12));
        // 截断到 10：保留 tombstone(ts=12)
        let freed = t.truncate_to(10);
        assert_eq!(freed, 1);
        assert_eq!(t.latest_ts(b"k"), Some(12));
        assert_eq!(t.get(b"k", 8), None, "截断后老快照应不可见");
        // 再截到 12：整键移除
        t.truncate_to(12);
        assert_eq!(t.latest_ts(b"k"), None);
    }

    #[test]
    fn delta_merge_visibility() {
        // 跨过 DELTA_MIN 触发归并：多版本可读性、墓碑遮蔽、latest_ts 全保持
        let t = TableMem::default();
        let n = DELTA_MIN + 100;
        for i in 0..n as u64 {
            t.install(
                format!("k{i:06}").into_bytes(),
                i + 1,
                Some(format!("v{i}").into_bytes().into()),
            );
        }
        assert_eq!(t.len(), n);
        // 全部可见（部分在 sorted、部分在 delta）
        for i in 0..n as u64 {
            let want = format!("v{i}");
            assert_eq!(
                t.get(format!("k{i:06}").as_bytes(), u64::MAX - 1).as_deref(),
                Some(want.as_bytes()),
                "key {i}"
            );
        }
        // 老快照只见前缀（ts = i+1：快照 s 只见 key < s）
        for s in [1u64, 10, (n / 2) as u64, n as u64] {
            for i in 0..n as u64 {
                let got = t.visible_ts(format!("k{i:06}").as_bytes(), s);
                if i < s {
                    assert_eq!(got, Some(i + 1), "snapshot {s} key {i}");
                } else {
                    assert_eq!(got, None, "snapshot {s} key {i}");
                }
            }
        }
    }

    #[test]
    fn delta_overwrite_and_tombstone_across_merge() {
        let t = TableMem::default();
        let n = DELTA_MIN + 10;
        // 同 key 多版本：k000000 写 n 次（ts 1..=n），最终墓碑
        for i in 1..=n as u64 {
            let val = if i == n as u64 { None } else { Some(format!("v{i}").into_bytes().into()) };
            t.install(b"k000000".to_vec(), i, val);
        }
        // 墓碑遮蔽（含 delta 索引路径与归并后 sorted 路径）
        assert_eq!(t.get(b"k000000", n as u64), None);
        let want: Arc<[u8]> = format!("v{}", n - 1).into_bytes().into();
        assert_eq!(t.get(b"k000000", n as u64 - 1), Some(want));
        assert_eq!(t.latest_ts(b"k000000"), Some(n as u64));
        // snapshot_rows：墓碑计入 overlay
        let rows = t.snapshot_rows(u64::MAX - 1);
        assert_eq!(rows.get(b"k000000".as_ref()), Some(&None));
    }

    #[test]
    fn shard_distribution() {
        let t = TableMem::default();
        for i in 0..1000u32 {
            t.install(
                format!("k{i}").into_bytes(),
                1,
                Some(b"x".to_vec().into()),
            );
        }
        assert_eq!(t.row_count_visible(1), 1000);
    }
}
