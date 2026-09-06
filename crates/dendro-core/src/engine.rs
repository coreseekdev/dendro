//! 引擎门面 — wire 层（pgwire/mywire/slt/bench）唯一入口。
//!
//! 线程模型：同步引擎；Database: Send+Sync（Arc 共享）；Session 单线程使用。
//! 一个 Database = 一个对象存储根上的库；一个 Session = 一条客户端连接。
//! SQL 执行逻辑在 `sql` 模块，本文件是状态与提交路径（SPEC 02/03/04 的交汇点）。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::memtx::{BranchMem, Txn};
use crate::objstore::cas::CasStore;
use crate::objstore::manifest::{Manifest, ManifestStore};
use crate::objstore::{local::LocalObjStore, memory::MemoryObjStore, ObjStore};
use crate::prolly::chunker::Mutation;
use crate::prolly::NodeStore;
use crate::types::{Output, SqlValue};
use crate::versioned::commit::Commit;
use crate::versioned::TableSchema;
use crate::wal::WalWriter;
use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// 对象存储配置（SPEC 01）
#[derive(Debug, Clone)]
pub enum StoreConfig {
    LocalDir(PathBuf),
    Memory,
}

#[derive(Debug, Clone)]
pub struct DbOptions {
    pub store: StoreConfig,
    pub wal_flush_interval_ms: u64,
    pub wal_segment_bytes: u64,
    pub durability: Durability,
    /// checkpoint 触发阈值（pending 字节）
    pub checkpoint_threshold_bytes: u64,
    /// checkpoint 周期（秒）；0 = 只显式触发
    pub checkpoint_interval_s: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    NoWait,
    Group,
    Always,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            store: StoreConfig::LocalDir(PathBuf::from("/tmp/dendro-data")),
            wal_flush_interval_ms: 50,
            wal_segment_bytes: 32 << 20,
            durability: Durability::Group,
            checkpoint_threshold_bytes: 16 << 20,
            checkpoint_interval_s: 30,
        }
    }
}

impl DbOptions {
    pub fn memory() -> Self {
        Self { store: StoreConfig::Memory, ..Default::default() }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// manifest 快照（原子换新）
pub struct DbSnapshot {
    pub manifest: Manifest,
}

/// 分支运行态
pub struct Branch {
    pub name: String,
    /// 最新 checkpoint 的 commit（树状态权威）
    pub head: ArcSwap<Option<Commit>>,
    /// WAL 写入器（分支私有序列）
    pub wal: Arc<WalWriter>,
    /// memtx
    pub mem: BranchMem,
    /// 提交串行化锁（OCC 验证+安装+WAL 序列 的原子域）
    pub commit_mu: Mutex<()>,
    next_seq: AtomicU64,
    /// 已安装（可见）的提交水位
    pub watermark: AtomicU64,
    /// 自上次 checkpoint 的累积 pending 变更：table → (key → mut)
    pub pending: Mutex<HashMap<u32, BTreeMap<Vec<u8>, Mutation>>>,
    pub pending_bytes: AtomicU64,
}

impl Branch {
    pub fn alloc_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::SeqCst) + 1
    }
    pub fn snapshot(&self) -> u64 {
        self.watermark.load(Ordering::Acquire)
    }
    /// 恢复路径：重建 seq 计数器
    pub fn restore_seq(&self, seq: u64) {
        self.next_seq.store(seq, Ordering::Release);
        self.watermark.store(seq, Ordering::Release);
    }
}

/// 已解析的预编译语句元数据
#[derive(Debug, Clone)]
pub struct PrepareMeta {
    pub param_types: Vec<crate::types::ColType>,
    pub result_columns: Vec<crate::types::ColumnMeta>,
}

/// 预编译语句（会话私有缓存）
#[derive(Clone)]
pub struct Prepared {
    pub sql: String,
    pub stmt: sqlparser::ast::Statement,
    pub param_types: Vec<crate::types::ColType>,
    pub result_columns: Vec<crate::types::ColumnMeta>,
}

/// wire 层面向的会话 trait（协议 crate 依赖此接口而非具体 Session）。
/// PreparedMeta/Output 语义见 spec/10。
pub trait WireSession: Send {
    fn exec(&mut self, sql: &str) -> Result<Vec<Output>>;
    fn prepare(&mut self, name: &str, sql: &str, hint: &[crate::types::ColType]) -> Result<PrepareMeta>;
    fn exec_prepared(&mut self, name: &str, params: &[SqlValue]) -> Result<Output>;
    fn close_prepared(&mut self, name: &str);
    /// ReadyForQuery 事务状态：b'I' 空闲 / b'T' 事务中 / b'E' 失败事务
    fn txn_status(&self) -> u8 {
        b'I'
    }
}

impl WireSession for Session {
    fn exec(&mut self, sql: &str) -> Result<Vec<Output>> {
        Session::exec(self, sql)
    }
    fn prepare(&mut self, name: &str, sql: &str, hint: &[crate::types::ColType]) -> Result<PrepareMeta> {
        Session::prepare(self, name, sql, hint)
    }
    fn exec_prepared(&mut self, name: &str, params: &[SqlValue]) -> Result<Output> {
        Session::exec_prepared(self, name, params)
    }
    fn close_prepared(&mut self, name: &str) {
        Session::close_prepared(self, name)
    }
    fn txn_status(&self) -> u8 {
        if self.failed_txn {
            b'E'
        } else if self.txn.is_some() {
            b'T'
        } else {
            b'I'
        }
    }
}

/// 物化器接口（列存投影由外部 crate 实现以避免依赖环；
/// dendro-server 启动时注入 dendro-columnar 的 CBF 实现，SPEC 05 §1）
pub trait Materializer: Send + Sync {
    /// 把表树（root）物化为列存对象，返回 (对象路径, 行数)。
    fn materialize(&self, obj: &Arc<dyn ObjStore>, store: &Arc<NodeStore>, root: &Hash, schema: &TableSchema) -> Result<(String, u64)>;
}

/// 列式扫描接口（AP 路径；实现在 dendro-columnar，SPEC 05 §7）
pub trait ApScan: Send + Sync {
    /// 扫描列存投影。pk_range = 第 0 列 order 域的开区间 (min_excl, max_incl)，
    /// None 表示不限；返回的行组按 (rows, cols) 对齐 schema。
    fn scan(
        &self,
        obj: &Arc<dyn ObjStore>,
        path: &str,
        schema: &TableSchema,
        pk_range: &Option<(Option<u64>, Option<u64>)>,
    ) -> Result<Vec<arrow::record_batch::RecordBatch>>;
}

/// 数据库门面
pub struct Database {
    pub(crate) opts: DbOptions,
    pub(crate) obj: Arc<dyn ObjStore>,
    pub(crate) cas: Arc<CasStore>,
    pub(crate) store: Arc<NodeStore>,
    pub(crate) manifest_store: Arc<ManifestStore>,
    pub(crate) state: ArcSwap<DbSnapshot>,
    pub(crate) branches: RwLock<HashMap<String, Arc<Branch>>>,
    pub(crate) session_seq: AtomicU64,
    /// 已写入 chunk 的进程内缓存（去重判定）
    pub(crate) chunk_seen: Mutex<HashSet<Hash>>,
    stop_cp: Arc<std::sync::atomic::AtomicBool>,
    materializer: arc_swap::ArcSwap<Option<std::sync::Arc<dyn Materializer>>>,
    ap_scan: arc_swap::ArcSwap<Option<std::sync::Arc<dyn ApScan>>>,
}

impl Database {
    /// 注入 AP 列式扫描器
    pub fn set_ap_scan(self: &Arc<Self>, a: std::sync::Arc<dyn ApScan>) {
        self.ap_scan.store(std::sync::Arc::new(Some(a)));
    }
    pub(crate) fn ap_scanner(&self) -> Option<std::sync::Arc<dyn ApScan>> {
        self.ap_scan.load().as_ref().clone()
    }
}

impl Database {
    /// 注入物化器（必须在打开后、写负载前）
    pub fn set_materializer(self: &Arc<Self>, m: std::sync::Arc<dyn Materializer>) {
        self.materializer.store(std::sync::Arc::new(Some(m)));
    }
    pub fn obj_store(&self) -> &Arc<dyn ObjStore> {
        &self.obj
    }
    pub fn node_store(&self) -> &Arc<NodeStore> {
        &self.store
    }
}

impl Database {
    /// 打开（不存在则初始化）一个库
    pub fn open(opts: DbOptions) -> Result<Arc<Database>> {
        let obj: Arc<dyn ObjStore> = match &opts.store {
            StoreConfig::LocalDir(p) => Arc::new(LocalObjStore::open(p)?),
            StoreConfig::Memory => Arc::new(MemoryObjStore::new()),
        };
        let cas = Arc::new(CasStore::new(obj.clone()));
        let store = Arc::new(NodeStore::new(cas.clone(), 4096));
        let manifest_store = Arc::new(ManifestStore::new(obj.clone()));
        // 初始化 / 恢复
        if manifest_store.init(now_ms()).is_err() {
            // 已存在 → 恢复
        }
        let (ver, manifest) = manifest_store.load_latest().map_err(SqlError::from)?;
        crate::recovery::recover_branches(&obj, &manifest)?;
        let db = Arc::new(Database {
            opts,
            obj,
            cas,
            store,
            manifest_store,
            state: ArcSwap::from_pointee(DbSnapshot { manifest }),
            branches: RwLock::new(HashMap::new()),
            session_seq: AtomicU64::new(1),
            chunk_seen: Mutex::new(HashSet::new()),
            stop_cp: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            materializer: arc_swap::ArcSwap::from_pointee(None),
            ap_scan: arc_swap::ArcSwap::from_pointee(None),
        });
        db.load_open_branches(ver)?;
        db.start_checkpoint_thread();
        Ok(db)
    }

    pub fn new_session(self: &Arc<Self>) -> Session {
        Session {
            db: self.clone(),
            branch: "main".to_string(),
            txn: None,
            prepared: HashMap::new(),
            failed_txn: false,
            dialect: crate::sql::SqlDialect::Pg,
        }
    }

    /// 协议层设置方言（mywire → MySql）
    pub fn set_session_dialect(sess: &mut Session, d: crate::sql::SqlDialect) {
        sess.dialect = d;
    }

    pub(crate) fn manifest(&self) -> Arc<DbSnapshot> {
        self.state.load_full()
    }

    /// 读/建分支运行态（恢复路径也走这里：从 manifest 构造）
    pub(crate) fn branch(&self, name: &str) -> Result<Arc<Branch>> {
        {
            let g = self.branches.read();
            if let Some(b) = g.get(name) {
                return Ok(b.clone());
            }
        }
        let snap = self.manifest();
        let head_info = snap
            .manifest
            .refs
            .get(name)
            .ok_or_else(|| SqlError::undefined_branch(format!("branch \"{name}\" does not exist")))?;
        let commit = match &head_info.commit {
            Some(a) => {
                let h = Hash::from_base32(a).ok_or_else(|| SqlError::internal("bad commit addr"))?;
                let (_ty, data) = self.cas.get(&h)?;
                Some(Commit::decode(&data)?)
            }
            None => None,
        };
        let wal = WalWriter::open(
            self.obj.clone(),
            name,
            head_info.wal_seg + 1,
            crate::wal::WalConfig::from(&self.opts),
        );
        let b = Arc::new(Branch {
            name: name.to_string(),
            head: ArcSwap::from_pointee(commit),
            wal,
            mem: BranchMem::default(),
            commit_mu: Mutex::new(()),
            next_seq: AtomicU64::new(0),
            watermark: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            pending_bytes: AtomicU64::new(0),
        });
        // 恢复：回放 WAL 中未物化的事务（covered_seq 之后）
        crate::recovery::replay_branch(self, &b, head_info)?;
        {
            let mut g = self.branches.write();
            g.entry(name.to_string()).or_insert(b.clone());
        }
        Ok(b)
    }

    pub(crate) fn remove_branch_runtime(&self, name: &str) {
        self.branches.write().remove(name);
    }

    /// MERGE BRANCH src INTO dst（SPEC 03 §6）
    pub fn merge_branches(&self, src: &str, dst: &str) -> Result<String> {
        if src == dst {
            return Err(SqlError::syntax("cannot merge a branch into itself"));
        }
        // 双方先 checkpoint（树状态冻结）
        self.checkpoint_branch(src)?;
        self.checkpoint_branch(dst)?;
        let (sc, dc) = {
            let snap = self.manifest();
            let s = snap.manifest.refs.get(src).and_then(|h| h.commit.as_ref()).and_then(|a| Hash::from_base32(a))
                .ok_or_else(|| SqlError::internal("source branch has no checkpoint"))?;
            let d = snap.manifest.refs.get(dst).and_then(|h| h.commit.as_ref()).and_then(|a| Hash::from_base32(a))
                .ok_or_else(|| SqlError::internal("target branch has no checkpoint"))?;
            (s, d)
        };
        let sc = self.load_commit(&sc)?;
        let dc = self.load_commit(&dc)?;
        // 公共祖先（按 height 双向收敛）
        let base = self.common_ancestor(&sc, &dc)?;
        // catalog 级合并
        let outcome = crate::versioned::merge::merge_map(&self.store, base.map(|c| c.root), Some(sc.root), Some(dc.root), &mut self.chunk_seen.lock())?;
        let session: HashSet<Hash> = HashSet::new();
        match outcome {
            crate::versioned::merge::MergeOutcome::NoOp => Ok("NOOP".to_string()),
            crate::versioned::merge::MergeOutcome::FastForward(new_root) => {
                self.write_branch_commit(dst, new_root, vec![dc.addr()], "fast-forward merge")?;
                Ok("FAST_FORWARD".to_string())
            }
            crate::versioned::merge::MergeOutcome::Merged { root, .. } => {
                self.write_branch_commit(dst, root, vec![dc.addr(), sc.addr()], "merge")?;
                Ok("MERGED".to_string())
            }
            crate::versioned::merge::MergeOutcome::Conflicts(cs) => {
                Err(SqlError::serialization(format!(
                    "merge conflict: {} conflicting keys, e.g. {:?}",
                    cs.len(),
                    cs.iter().take(5).map(|c| c.key.clone()).collect::<Vec<_>>()
                )))
            }
        }
        .map(|s| {
            let _ = session;
            s
        })
    }

    pub(crate) fn load_commit(&self, addr: &Hash) -> Result<Commit> {
        let (_ty, data) = self.cas.get(addr)?;
        Commit::decode(&data)
    }

    fn common_ancestor(&self, a: &Commit, b: &Commit) -> Result<Option<Commit>> {
        // 按高度对齐后同步上溯（merge commit 多父取第一父近似；完整祖先闭包 v2）
        let (mut x, mut y) = (a.clone(), b.clone());
        let mut guard = 0;
        while x.height > y.height {
            x = self.parent_of(&x)?;
            guard += 1;
            if guard > 100_000 { return Err(SqlError::internal("ancestor walk overflow")); }
        }
        while y.height > x.height {
            y = self.parent_of(&y)?;
        }
        while x.addr() != y.addr() {
            x = self.parent_of(&x)?;
            y = self.parent_of(&y)?;
            guard += 1;
            if guard > 100_000 { return Err(SqlError::internal("ancestor walk overflow")); }
        }
        Ok(Some(x))
    }

    fn parent_of(&self, c: &Commit) -> Result<Commit> {
        c.parents
            .first()
            .map(|p| self.load_commit(p))
            .transpose()?
            .ok_or_else(|| SqlError::internal("no parent"))
    }

    /// 在分支上写一个新 commit（树根 + 父链），推进 manifest；当前分支 memtx pending 保留
    pub(crate) fn write_branch_commit(
        &self,
        branch: &str,
        root: Hash,
        parents: Vec<Hash>,
        message: &str,
    ) -> Result<Hash> {
        let b = self.branch(branch)?;
        let _g = b.commit_mu.lock();
        let _head = b.head.load_full();
        let height = parents.iter().try_fold(0u64, |m, p| -> Result<u64> {
            Ok(m.max(self.load_commit(p)?.height))
        })? + 1;
        let commit = Commit {
            root,
            parents,
            height,
            ts_ms: now_ms(),
            branch: branch.to_string(),
            author: "dendro".into(),
            message: message.into(),
        };
        let cchunk = commit.encode();
        let mut session: HashSet<Hash> = HashSet::new();
        self.cas.put_batch(&[cchunk], &mut session).map_err(SqlError::from)?;
        b.head.store(Arc::new(Some(commit.clone())));
        let seq = b.alloc_seq();
        let ck = crate::wal::CheckpointRecord { catalog_root: root, commit_addr: commit.addr(), seq_covered: b.watermark.load(Ordering::Acquire) };
        b.wal.append(crate::wal::FrameType::Checkpoint, seq, &crate::wal::encode_checkpoint(&ck), self.opts.durability)?;
        let seg_now = b.wal.current_seg().saturating_sub(1);
        let covered = ck.seq_covered;
        let bname = branch.to_string();
        self.update_manifest(|m| {
            let h = m.refs.get_mut(&bname).ok_or_else(|| SqlError::internal("branch vanished"))?;
            h.commit = Some(commit.addr().to_base32());
            h.wal_seg = seg_now.max(h.wal_seg);
            h.covered_seq = covered;
            Ok(true)
        })?;
        Ok(commit.addr())
    }

    pub(crate) fn branch_exists(&self, name: &str) -> bool {
        self.branches.read().contains_key(name)
            || self.manifest().manifest.refs.contains_key(name)
    }

    /// manifest 乐观提交（重试内建）
    pub(crate) fn update_manifest(
        &self,
        f: impl Fn(&mut Manifest) -> Result<bool>,
    ) -> Result<()> {
        for _ in 0..64 {
            let (ver, m) = self.manifest_store.load_latest().map_err(SqlError::from)?;
            let mut m = m;
            match f(&mut m)? {
                false => return Ok(()), // 无需变更
                true => {}
            }
            match self.manifest_store.commit(ver, m) {
                Ok(_) => {
                    let (_, fresh) = self.manifest_store.load_latest().map_err(SqlError::from)?;
                    self.state.store(Arc::new(DbSnapshot { manifest: fresh }));
                    return Ok(());
                }
                Err(crate::objstore::ObjError::Exists(_)) => continue, // 乐观冲突重试
                Err(e) => return Err(SqlError::from(e)),
            }
        }
        Err(SqlError::internal("manifest commit: too many conflicts"))
    }

    /// checkpoint 一个分支：pending 变更物化为树 + commit 对象 + manifest 推进
    pub fn checkpoint_branch(&self, branch_name: &str) -> Result<Option<Hash>> {
        let b = self.branch(branch_name)?;
        let _g = b.commit_mu.lock();
        self.checkpoint_locked(&b)
    }

    /// 需已持 commit_mu
    pub(crate) fn checkpoint_locked(&self, b: &Branch) -> Result<Option<Hash>> {
        let pending: HashMap<u32, BTreeMap<Vec<u8>, Mutation>> =
            std::mem::take(&mut *b.pending.lock());
        b.pending_bytes.store(0, Ordering::Release);
        let old_head = b.head.load_full();
        let old_catalog = old_head.as_ref().as_ref().map(|c| c.root);
        let mut session: HashSet<Hash> = HashSet::new();
        let catalog = crate::versioned::Versioned::new(self.store.clone());
        let mut changes: Vec<(String, Option<crate::versioned::TableEntry>)> = Vec::new();
        // pending 按表应用
        let _snap = self.manifest();
        for (tid, muts) in &pending {
            // 找表名/当前 root
            let entries = catalog.catalog_entries(old_catalog.as_ref())?;
            let (tname, entry) = entries
                .iter()
                .find(|(_, e)| e.id == *tid)
                .map(|(n, e)| (n.clone(), e.clone()))
                .ok_or_else(|| SqlError::internal("pending for unknown table"))?;
            let old_root = entry
                .table_root
                .as_ref()
                .and_then(|s| Hash::from_base32(s))
                .or(None);
            let root_ref = old_root.as_ref();
            let (new_root, _dirty) = catalog.apply_table_mutations(
                root_ref,
                muts.iter().map(|(k, m)| (k.clone(), m.clone())).collect(),
                &mut session,
            )?;
            let mut ne = entry.clone();
            ne.table_root = new_root.map(|h| h.to_base32());
            ne.row_count = count_rows(self.store.as_ref(), new_root.as_ref());
            // 列存物化（有物化器且表非空时重建投影；增量投影 v2）
            if let Some(root) = &new_root {
                if let Ok(schema) = catalog.load_schema_with_entry(&ne) {
                    if let Some(m) = self.materializer.load().as_ref() {
                        match m.materialize(&self.obj, &self.store, root, &schema) {
                            Ok((path, nrows)) => {
                                ne.col_path = Some(path);
                                ne.col_rows = nrows;
                            }
                            Err(e) => tracing::warn!("materialize {}: {e}", ne.name),
                        }
                    }
                }
            }
            changes.push((tname, Some(ne)));
        }
        let new_catalog = catalog.apply_catalog(old_catalog.as_ref(), changes, &mut session)?;
        // commit 对象
        let height = old_head.as_ref().as_ref().map(|c| c.height).unwrap_or(0) + 1;
        let commit = Commit {
            root: new_catalog.ok_or_else(|| SqlError::internal("empty catalog"))?,
            parents: old_head.iter().map(|c| c.addr()).collect(),
            height,
            ts_ms: now_ms(),
            branch: b.name.clone(),
            author: "dendro".into(),
            message: "checkpoint".into(),
        };
        let cchunk = commit.encode();
        self.cas.put_batch(&[cchunk], &mut session).map_err(SqlError::from)?;
        b.head.store(Arc::new(Some(commit.clone())));
        // WAL CHECKPOINT 帧 + 立即 flush
        let seq = b.alloc_seq();
        let ck = crate::wal::CheckpointRecord {
            catalog_root: commit.root,
            commit_addr: commit.addr(),
            seq_covered: b.watermark.load(Ordering::Acquire),
        };
        b.wal
            .append(crate::wal::FrameType::Checkpoint, seq, &crate::wal::encode_checkpoint(&ck), Durability::Always)?;
        // manifest：commit + 当前已 flush 段 + covered_seq
        let seg_now = b.wal.current_seg().saturating_sub(1);
        let covered = ck.seq_covered;
        let bname = b.name.clone();
        self.update_manifest(|m| {
            let h = m.refs.get_mut(&bname).ok_or_else(|| SqlError::internal("branch vanished"))?;
            h.commit = Some(commit.addr().to_base32());
            h.wal_seg = seg_now.max(h.wal_seg);
            h.covered_seq = covered;
            Ok(true)
        })?;
        // 释放 memtx 历史版本
        b.mem.truncate_all(covered);
        Ok(Some(commit.addr()))
    }

    fn load_open_branches(&self, _ver: u64) -> Result<()> {
        let names: Vec<String> = self.manifest().manifest.refs.keys().cloned().collect();
        for n in names {
            let _ = self.branch(&n)?; // 失败（如坏 commit）则跳过该分支
        }
        Ok(())
    }

    fn start_checkpoint_thread(self: &Arc<Self>) {
        let db = self.clone();
        let interval = self.opts.checkpoint_interval_s.max(1);
        let threshold = self.opts.checkpoint_threshold_bytes;
        std::thread::Builder::new()
            .name("dendro-checkpoint".into())
            .spawn(move || loop {
                if db.stop_cp.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(interval));
                let names: Vec<String> = {
                    let g = db.branches.read();
                    g.keys().cloned().collect()
                };
                for n in names {
                    let Ok(b) = db.branch(&n) else { continue };
                    if db.opts.checkpoint_interval_s == 0 {
                        continue;
                    }
                    if b.pending_bytes.load(Ordering::Relaxed) >= threshold {
                        let _ = db.checkpoint_branch(&n);
                    }
                }
            })
            .expect("spawn checkpoint thread");
    }

}

impl From<&DbOptions> for crate::wal::WalConfig {
    fn from(o: &DbOptions) -> Self {
        Self {
            flush_interval: std::time::Duration::from_millis(o.wal_flush_interval_ms.max(1)),
            segment_bytes: o.wal_segment_bytes,
            durability: o.durability,
        }
    }
}

fn count_rows(store: &NodeStore, root: Option<&Hash>) -> u64 {
    root.and_then(|r| crate::prolly::cursor::tree_count(store, r).ok()).unwrap_or(0)
}

/// 会话
pub struct Session {
    pub(crate) db: Arc<Database>,
    pub(crate) branch: String,
    pub(crate) txn: Option<Txn>,
    pub(crate) prepared: HashMap<String, Prepared>,
    pub(crate) failed_txn: bool,
    pub(crate) dialect: crate::sql::SqlDialect,
}

impl Session {
    /// 执行一段 SQL（可含多语句，`;` 分隔）；空/纯注释 → 空 Vec
    pub fn exec(&mut self, sql: &str) -> Result<Vec<Output>> {
        let db = self.db.clone();
        crate::sql::exec_batch(&db, self, sql)
    }
    /// PG extended：Parse
    pub fn prepare(
        &mut self,
        name: &str,
        sql: &str,
        hint: &[crate::types::ColType],
    ) -> Result<PrepareMeta> {
        let db = self.db.clone();
        crate::sql::prepare(&db, self, name, sql, hint)
    }
    /// PG extended：Execute
    pub fn exec_prepared(&mut self, name: &str, params: &[SqlValue]) -> Result<Output> {
        let db = self.db.clone();
        crate::sql::exec_prepared(&db, self, name, params)
    }
    pub fn close_prepared(&mut self, name: &str) {
        self.prepared.remove(name);
    }
}

/// 提交一个事务的写集（engine 内部路径，sql 模块调用）
pub(crate) fn commit_tx(db: &Database, sess_branch: &str, txn: &Txn) -> Result<u64> {
    let b = db.branch(sess_branch)?;
    let _g = b.commit_mu.lock();
    let seq = b.alloc_seq();
    // OCC：写写冲突检测（SPEC 04 §3 first-committer-wins）
    // 收集涉及的表
    let mut tables: Vec<u32> = txn.writes.keys().map(|(t, _)| *t).collect();
    tables.sort_unstable();
    tables.dedup();
    crate::memtx::validate_and_install(&b.mem, &[], txn, seq)?;
    // pending 登记 + WAL 帧
    let mut recs: Vec<crate::wal::TxnRecord> = Vec::new();
    {
        let mut pend = b.pending.lock();
        for (tid, key, m) in iter_writes(txn) {
            let val = match m {
                Mutation::Put(v) => Some(v.clone()),
                Mutation::Delete => None,
            };
            pend.entry(tid).or_default().insert(key.clone(), m.clone());
            recs.push(crate::wal::TxnRecord { table_id: tid, ops: vec![(key.clone(), val)] });
        }
    }
    b.pending_bytes.fetch_add(payload_len(&recs) as u64, Ordering::Release);
    b.wal.append(crate::wal::FrameType::Txn, seq, &crate::wal::encode_txn(&recs), db.opts.durability)?;
    b.watermark.store(seq, Ordering::Release);
    Ok(seq)
}

fn iter_writes(txn: &Txn) -> impl Iterator<Item = (u32, &Vec<u8>, &Mutation)> {
    txn.writes.iter().map(|((t, k), m)| (*t, k, m))
}

fn payload_len(recs: &[crate::wal::TxnRecord]) -> usize {
    recs.iter().map(|r| r.ops.iter().map(|(k, v)| k.len() + v.as_ref().map(|v| v.len()).unwrap_or(0)).sum::<usize>()).sum()
}

// —— wire 层 API（真身）——
impl Database {
    pub(crate) fn open_impl(opts: DbOptions) -> Result<Arc<Database>> {
        Database::open(opts)
    }
    pub(crate) fn exec_impl(&self, sess: &mut Session, sql: &str) -> Result<Vec<Output>> {
        crate::sql::exec_batch(self, sess, sql)
    }
    pub(crate) fn prepare_impl(
        &self,
        sess: &mut Session,
        name: &str,
        sql: &str,
        hint: &[crate::types::ColType],
    ) -> Result<crate::engine::PrepareMeta> {
        crate::sql::prepare(self, sess, name, sql, hint)
    }
    pub(crate) fn exec_prepared_impl(&self, sess: &mut Session, name: &str, params: &[SqlValue]) -> Result<Output> {
        crate::sql::exec_prepared(self, sess, name, params)
    }
    pub(crate) fn close_prepared_impl(&self, sess: &mut Session, name: &str) {
        sess.prepared.remove(name);
    }
}
