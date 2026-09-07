//! 内存事务引擎（SPEC 04）：OCC MVCC，分支内单写者提交，无锁快照读。
//!
//! 数据结构：
//! - 每表 64 shard 的 HashMap<Key, Arc<VersionVec>>（写短锁；读锁粒度=单 key 查找）
//! - VersionVec: 按提交序排列的不可变 VerCell（ts 升序），读者二分找可见版本
//! - checkpoint 后 ts ≤ ckpt_seq 的历史版本被释放（内存有界 = checkpoint 窗口写入量）
//!
//! 红线执行：无 Rc/RefCell；VerCell 不可变以 Arc 共享；锁只出现在
//! 写路径（安装）与 checkpoint 截断，读路径每次查找一次读锁。

use crate::error::{Result, SqlError};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// 单个版本（不可变）
pub struct VerCell {
    pub ts: u64,
    /// None = tombstone（删除）
    pub val: Option<Arc<Vec<u8>>>,
}

type VersionVec = Vec<Arc<VerCell>>;

struct Shard {
    map: RwLock<HashMap<Box<[u8]>, Arc<VersionVec>>>,
}

pub const NSHARD: usize = 64;

fn shard_of(key: &[u8]) -> usize {
    let h = xxhash_rust::xxh3::xxh3_64(key);
    (h as usize) & (NSHARD - 1)
}

/// 一张表的内存态
pub struct TableMem {
    shards: Vec<Shard>,
}

impl Default for TableMem {
    fn default() -> Self {
        Self {
            shards: (0..NSHARD).map(|_| Shard { map: RwLock::new(HashMap::new()) }).collect(),
        }
    }
}

impl TableMem {
    /// 点查：snapshot 可见的最新版本
    pub fn get(&self, key: &[u8], snapshot: u64) -> Option<Arc<Vec<u8>>> {
        let sh = &self.shards[shard_of(key)];
        let g = sh.map.read();
        let vv = g.get(key)?;
        // 尾向二分：ts <= snapshot 的最新版本
        let idx = vv.partition_point(|c| c.ts <= snapshot);
        if idx == 0 {
            None
        } else {
            vv[idx - 1].val.clone()
        }
    }

    /// 可见版本 ts（存在性/冲突检测用）
    pub fn visible_ts(&self, key: &[u8], snapshot: u64) -> Option<u64> {
        let sh = &self.shards[shard_of(key)];
        let g = sh.map.read();
        let vv = g.get(key)?;
        let idx = vv.partition_point(|c| c.ts <= snapshot);
        if idx == 0 {
            None
        } else {
            Some(vv[idx - 1].ts)
        }
    }

    /// 最新版本 ts（含未提交窗口外的全部已提交版本；OCC 写写冲突检测）
    pub fn latest_ts(&self, key: &[u8]) -> Option<u64> {
        let sh = &self.shards[shard_of(key)];
        let g = sh.map.read();
        let vv = g.get(key)?;
        vv.last().map(|c| c.ts)
    }

    /// 提交安装：key 的新版本（调用方已持有分支 commit 锁，见 engine::commit_tx）
    pub fn install(&self, key: Vec<u8>, ts: u64, val: Option<Arc<Vec<u8>>>) {
        let sh = &self.shards[shard_of(&key)];
        let mut g = sh.map.write();
        let vv = g.entry(key.into()).or_default();
        // commit 锁保证 ts 单调
        debug_assert!(vv.last().map(|c| c.ts) <= Some(ts));
        Arc::make_mut(vv).push(Arc::new(VerCell { ts, val }));
    }

    /// checkpoint 截断：释放 ts <= ckpt_seq 的历史（若最新也 ≤，则整键移除）
    pub fn truncate_to(&self, ckpt_seq: u64) -> usize {
        let mut freed = 0usize;
        for sh in &self.shards {
            let mut g = sh.map.write();
            let keys: Vec<Box<[u8]>> = g.keys().cloned().collect();
            for k in keys {
                let vv = g.get_mut(&k).unwrap();
                if vv.last().map(|c| c.ts).unwrap_or(0) <= ckpt_seq {
                    g.remove(&k);
                    freed += 1;
                } else {
                    // 保留尾部最新（> ckpt），丢弃历史
                    let idx = vv.partition_point(|c| c.ts <= ckpt_seq);
                    if idx > 0 {
                        let keep: VersionVec = vv[idx..].to_vec();
                        *vv = Arc::new(keep);
                        freed += 1;
                    }
                }
            }
        }
        freed
    }

    /// 全表可见行快照（TP 扫描 overlay 用；表小/窗口短，物化成本可接受）
    pub fn snapshot_rows(&self, snapshot: u64) -> BTreeMap<Vec<u8>, Option<Arc<Vec<u8>>>> {
        let mut out = BTreeMap::new();
        for sh in &self.shards {
            let g = sh.map.read();
            for (k, vv) in g.iter() {
                let idx = vv.partition_point(|c| c.ts <= snapshot);
                if idx > 0 {
                    out.insert(k.to_vec(), vv[idx - 1].val.clone());
                }
            }
        }
        out
    }

    pub fn row_count_visible(&self, snapshot: u64) -> usize {
        self.snapshot_rows(snapshot).values().filter(|v| v.is_some()).count()
    }
}

/// 分支内存表集合：table_id → TableMem
#[derive(Default)]
pub struct BranchMem {
    tables: RwLock<HashMap<u32, Arc<TableMem>>>,
}

impl BranchMem {
    pub fn table(&self, table_id: u32) -> Arc<TableMem> {
        {
            let g = self.tables.read();
            if let Some(t) = g.get(&table_id) {
                return t.clone();
            }
        }
        let mut g = self.tables.write();
        g.entry(table_id).or_insert_with(|| Arc::new(TableMem::default())).clone()
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
        Self { snapshot, writes: BTreeMap::new(), explicit: false, head_root: None }
    }
    pub fn put(&mut self, table_id: u32, key: Vec<u8>, val: Vec<u8>) {
        self.writes.insert((table_id, key), crate::prolly::Mutation::Put(val));
    }
    pub fn delete(&mut self, table_id: u32, key: Vec<u8>) {
        self.writes.insert((table_id, key), crate::prolly::Mutation::Delete);
    }
    /// 同 key 后写覆盖前写：读取时写集优先
    pub fn local_get(&self, table_id: u32, key: &[u8]) -> Option<Option<Arc<Vec<u8>>>> {
        match self.writes.get(&(table_id, key.to_vec())) {
            Some(crate::prolly::Mutation::Put(v)) => Some(Some(Arc::new(v.clone()))),
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
            crate::prolly::Mutation::Put(v) => tm.install(key.clone(), commit_seq, Some(Arc::new(v.clone()))),
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
        t.install(b"k".to_vec(), 5, Some(Arc::new(b"a".to_vec())));
        t.install(b"k".to_vec(), 8, Some(Arc::new(b"b".to_vec())));
        t.install(b"k".to_vec(), 12, None);
        assert_eq!(t.get(b"k", 4).is_none(), true);
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
    fn shard_distribution() {
        let t = TableMem::default();
        for i in 0..1000u32 {
            t.install(format!("k{i}").into_bytes(), 1, Some(Arc::new(b"x".to_vec())));
        }
        assert_eq!(t.row_count_visible(1), 1000);
    }
}
