//! KV 接口层（SPEC 11）：把 memtx + prolly 树统一暴露为**分支化的版本 KV**。
//!
//! 定位（对"KV 层"讨论的落点）：KV 是接口契约，不是实现位置。
//! 实现复用全部既有机器——memtx（增量）、prolly 树（权威）、OCC（提交）、
//! WAL（持久）、分支（键空间隔离）、merge（冲突消解）。
//!
//! "不标准"之处（相对 etcd/Redis 的差异，均为有意设计）：
//! - 值带版本：MVCC 快照读、追加式无原地改写
//! - 键空间 = 分支：可 fork、可三方合并（行级冲突显式化）
//! - 键序 = 字节序：范围扫描按字节比较
//! - 提交 = OCC 验证：写写冲突返回 40001

use std::collections::HashSet;

use crate::engine::{commit_tx, Database};
use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::format::row::{decode_row, encode_key, encode_row};
use crate::memtx::Txn;
use crate::prolly::chunker::Mutation;
use crate::prolly::cursor::TreeIter;
use crate::types::{ColType, SqlValue};
use crate::versioned::{ColumnDef, TableSchema};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const KV_TABLE: &str = "__kv";

pub(crate) fn kv_schema() -> TableSchema {
    TableSchema {
        name: KV_TABLE.into(),
        columns: vec![
            ColumnDef { name: "k".into(), ty: ColType::Bytes, nullable: false },
            ColumnDef { name: "v".into(), ty: ColType::Bytes, nullable: true },
        ],
        pk: vec![0],
    }
}

fn enc_key(key: &[u8]) -> Vec<u8> {
    encode_key(&[SqlValue::Bytes(key.to_vec())])
}

fn enc_row(key: &[u8], val: &[u8]) -> Vec<u8> {
    encode_row(&[SqlValue::Bytes(key.to_vec()), SqlValue::Bytes(val.to_vec())])
}

/// 分支化的版本 KV 会话。
/// 与 SQL 共享同一存储与事务机制（同一个 `__kv` 表）；写经过 OCC 与 WAL。
pub struct Kv {
    db: Arc<Database>,
    branch: String,
    table_id: u32,
    txn: Option<Txn>,
}

impl Kv {
    /// 打开某分支上的 KV 视图；`__kv` 表不存在时自动创建（catalog 提交）。
    pub fn open(db: &Arc<Database>, branch: &str) -> Result<Kv> {
        let mut kv = Kv { db: db.clone(), branch: branch.to_string(), table_id: 0, txn: None };
        kv.ensure_table()?;
        Ok(kv)
    }

    /// 切换分支（键空间隔离；未提交事务会被丢弃）
    pub fn use_branch(&mut self, name: &str) -> Result<()> {
        self.db.branch(name)?;
        self.branch = name.to_string();
        self.txn = None;
        self.ensure_table()?;
        Ok(())
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }
    /// 内部表 id（诊断用）
    pub fn table_id(&self) -> u32 {
        self.table_id
    }

    fn ensure_table(&mut self) -> Result<()> {
        let db = self.db.clone();
        let branch = self.branch.clone();
        match super::sql::scan::resolve_table(&db, &branch, KV_TABLE) {
            Ok((_, entry)) => self.table_id = entry.id,
            Err(e) => {
                if e.state != "42P01" {
                    return Err(e);
                }
                self.table_id = create_kv_table(&db, &branch)?;
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<u64> {
        if let Some(t) = &self.txn {
            return Ok(t.snapshot);
        }
        Ok(self.db.branch(&self.branch)?.snapshot())
    }

    /// 点查：memtx 快照 ∪ 树
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let key = key.as_ref();
        let b = self.db.branch(&self.branch)?;
        let snapshot = self.snapshot()?;
        let entry = self.kv_entry()?;
        let k = enc_key(key);
        // 写集优先（显式事务内的读己之写）
        if let Some(t) = &self.txn {
            if let Some(v) = t.local_get(entry.id, &k) {
                return Ok(v.map(|x| x.to_vec()));
            }
        }
        if let Some(v) = b.mem.table(entry.id).get(&k, snapshot) {
            let row = decode_row(&v)?;
            return Ok(row.get(1).and_then(val_bytes));
        }
        if let Some(root) = entry
            .table_root
            .as_ref()
            .and_then(|s| Hash::from_base32(s))
        {
            if let Some(v) = crate::prolly::cursor::lookup(self.db.node_store(), &root, &k)? {
                let row = decode_row(&v)?;
                return Ok(row.get(1).and_then(val_bytes));
            }
        }
        Ok(None)
    }

    /// 范围扫描 [start, end)（字节序；None = 不设界）。返回 (key, value) 升序。
    pub fn scan(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let b = self.db.branch(&self.branch)?;
        let snapshot = self.snapshot()?;
        let entry = self.kv_entry()?;
        let mut out: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

        // 1) 树侧（已物化，键序）
        if let Some(root) = entry
            .table_root
            .as_ref()
            .and_then(|s| Hash::from_base32(s))
        {
            let mut it = TreeIter::new(self.db.node_store().clone(), &root)?;
            if let Some(s) = start {
                it.seek(s)?;
            }
            while let Some((k, v)) = it.next_item()? {
                if let Some(e) = end {
                    if k.as_slice() >= e {
                        break;
                    }
                }
                if let Some(row) = decode_row(&v).ok().map(|r| (r, ())) {
                    if let Some(val) = row.0.get(1).and_then(val_bytes) {
                        out.insert(k, val);
                    }
                }
            }
        }
        // 2) memtx overlay：覆盖同键、插入新键、tombstone 删除
        let overlay = b.mem.table(entry.id).snapshot_rows(snapshot);
        for (k, v) in overlay {
            let key = match crate::format::row::decode_key(&k, &[ColType::Bytes]) {
                Ok(vals) => match vals.into_iter().next() {
                    Some(SqlValue::Bytes(b)) => b,
                    _ => continue,
                },
                Err(_) => continue,
            };
            if let (Some(s), Some(e)) = (start, end) {
                if key.as_slice() < s || key.as_slice() >= e {
                    continue;
                }
            }
            match v {
                Some(bytes) => {
                    if let Ok(row) = decode_row(&bytes) {
                        if let Some(val) = row.get(1).and_then(val_bytes) {
                            out.insert(key, val);
                        }
                    } else {
                        out.remove(&key);
                    }
                }
                None => {
                    out.remove(&key);
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// 写入（自动提交模式：立即提交；显式事务中进写集）
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let (k, v) = (key.as_ref().to_vec(), value.as_ref().to_vec());
        let row = enc_row(&k, &v);
        self.apply(&k, Mutation::Put(row))
    }

    /// 删除（自动提交模式：立即提交）
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<()> {
        let k = key.as_ref().to_vec();
        self.apply(&k, Mutation::Delete)
    }

    fn apply(&mut self, raw_key: &[u8], m: Mutation) -> Result<()> {
        let snapshot = self.snapshot()?;
        let entry = self.kv_entry()?;
        let k = enc_key(raw_key);
        let autocommit = self.txn.is_none();
        let txn = self.txn.get_or_insert_with(|| Txn::new(snapshot));
        match &m {
            Mutation::Put(_) => txn.put(entry.id, k, m_val(&m)),
            Mutation::Delete => txn.delete(entry.id, k),
        }
        if autocommit {
            let t = self.txn.take().expect("just inserted");
            commit_tx(&self.db, &self.branch, &t)?;
        }
        Ok(())
    }

    /// 比较-交换（线性化）：当前值等于 expect（None 表示期望不存在）才写入。
    /// 冲突 = 返回 Ok(false)（并发提交者赢）；显式事务中用法相同。
    pub fn cas(
        &mut self,
        key: impl AsRef<[u8]>,
        expect: Option<&[u8]>,
        new: impl AsRef<[u8]>,
    ) -> Result<bool> {
        // CAS = 显式事务内的读-判-写：begin 的快照先于读，
        // OCC 验证保证读→提交之间无并发写（线性化）
        self.begin()?;
        let cur = self.get(key.as_ref())?;
        let matched = match (expect, cur.as_deref()) {
            (Some(e), Some(c)) => e == c,
            (None, None) => true,
            _ => false,
        };
        if matched {
            self.put(key.as_ref(), new.as_ref())?;
            self.commit()?;
        } else {
            self.rollback();
        }
        Ok(matched)
    }

    /// 显式事务（KV 层的 BEGIN）
    pub fn begin(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(SqlError::new("25001", "transaction already active"));
        }
        let snap = self.snapshot()?;
        let mut t = Txn::new(snap);
        t.explicit = true;
        self.txn = Some(t);
        Ok(())
    }
    pub fn commit(&mut self) -> Result<()> {
        let t = self
            .txn
            .take()
            .ok_or_else(|| SqlError::new("25P01", "no transaction"))?;
        if !t.writes.is_empty() {
            commit_tx(&self.db, &self.branch, &t)?;
        }
        Ok(())
    }
    pub fn rollback(&mut self) {
        self.txn = None;
    }

    fn kv_entry(&self) -> Result<crate::versioned::TableEntry> {
        let db = self.db.clone();
        let branch = self.branch.clone();
        let (_, entry) = super::sql::scan::resolve_table(&db, &branch, KV_TABLE)?;
        Ok(entry)
    }
}

fn val_bytes(v: &SqlValue) -> Option<Vec<u8>> {
    match v {
        SqlValue::Bytes(b) => Some(b.clone()),
        _ => None,
    }
}

fn m_val(m: &Mutation) -> Vec<u8> {
    match m {
        Mutation::Put(v) => v.clone(),
        Mutation::Delete => Vec::new(),
    }
}

/// 确保 `__kv` 表存在（首次使用时通过 DDL 路径创建）；返回表 id。
fn create_kv_table(db: &Arc<Database>, branch: &str) -> Result<u32> {
    let b = db.branch(branch)?;
    let _g = b.commit_mu.lock();
    // 双检：可能已被其他会话创建
    {
        let head = b.head.load_full();
        let catalog = crate::versioned::Versioned::new(db.store.clone());
        if let Some(entry) =
            catalog.catalog_lookup(head.as_ref().as_ref().map(|c| c.root).as_ref(), KV_TABLE)?
        {
            return Ok(entry.id);
        }
    }
    let mut session_chunks: HashSet<Hash> = HashSet::new();
    let catalog = crate::versioned::Versioned::new(db.store.clone());
    let head = b.head.load_full();
    let old_catalog = head.as_ref().as_ref().map(|c| c.root);
    let schema = kv_schema();
    let tid = {
        let catalog_probe = crate::versioned::Versioned::new(db.store.clone());
        catalog_probe
            .catalog_entries(old_catalog.as_ref())?
            .iter()
            .map(|(_, e)| e.id)
            .max()
            .unwrap_or(0)
            + 1
    };
    let entry = crate::versioned::TableEntry {
        id: tid,
        name: KV_TABLE.into(),
        schema_addr: schema.addr_of().to_base32(),
        table_root: None,
        row_count: 0,
        col_segments: Vec::new(),
        col_deletes: Vec::new(),
        col_rows: 0,
    };
    db.cas.put_batch(&[schema.to_chunk()], &mut session_chunks).map_err(SqlError::from)?;
    let new_catalog = catalog.apply_catalog(old_catalog.as_ref(), vec![(KV_TABLE.into(), Some(entry))], &mut session_chunks)?;
    let commit = crate::versioned::Commit {
        root: new_catalog.ok_or_else(|| SqlError::internal("empty catalog"))?,
        parents: head.iter().map(|c| c.addr()).collect(),
        height: head.as_ref().as_ref().map(|c| c.height).unwrap_or(0) + 1,
        ts_ms: crate::engine::now_ms(),
        branch: branch.to_string(),
        author: "dendro".into(),
        message: "ensure __kv".into(),
    };
    let cchunk = commit.encode();
    db.cas.put_batch(&[cchunk], &mut session_chunks).map_err(SqlError::from)?;
    b.head.store(Arc::new(Some(commit.clone())));
    let seq = b.alloc_seq();
    let ck = crate::wal::CheckpointRecord {
        catalog_root: commit.root,
        commit_addr: commit.addr(),
        seq_covered: b.watermark.load(std::sync::atomic::Ordering::Acquire),
    };
    b.wal.append(
        crate::wal::FrameType::Checkpoint,
        seq,
        &crate::wal::encode_checkpoint(&ck),
        db.opts.durability,
    )?;
    let seg_now = b.wal.current_seg().saturating_sub(1);
    let covered = ck.seq_covered;
    let bname = branch.to_string();
    db.update_manifest(|m| {
        let h = m.refs.get_mut(&bname).ok_or_else(|| SqlError::internal("branch vanished"))?;
        h.commit = Some(commit.addr().to_base32());
        h.wal_seg = seg_now.max(h.wal_seg);
        h.covered_seq = covered;
        Ok(true)
    })?;
    Ok(tid)
}


#[cfg(test)]
mod dbg_tests {
    use super::*;
    use crate::Database;
    use crate::DbOptions;

    #[test]
    fn dbg_put_get() {
        let db = Database::open(DbOptions::memory()).unwrap();
        let mut kv = Kv::open(&db, "main").unwrap();
        eprintln!("[dbg] table_id={}", kv.table_id);
        kv.put("a", "1").unwrap();
        eprintln!("[dbg] watermark={}", db.branch("main").unwrap().watermark.load(std::sync::atomic::Ordering::Acquire));
        let entry = kv.kv_entry().unwrap();
        eprintln!("[dbg] entry.id={} root={:?}", entry.id, entry.table_root);
        let b = db.branch("main").unwrap();
        let over = b.mem.table(entry.id).snapshot_rows(10);
        eprintln!("[dbg] overlay keys={:?} vals={:?}", over.keys().map(|k| k.to_vec()).collect::<Vec<_>>(), over.values().map(|v| v.clone()).collect::<Vec<_>>());
        let got = kv.get("a").unwrap();
        eprintln!("[dbg] get(a)={got:?}");
    }
}
