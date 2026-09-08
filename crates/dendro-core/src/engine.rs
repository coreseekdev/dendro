#![allow(clippy::type_complexity)]
//! 引擎门面 — wire 层（pgwire/mywire/slt/bench）唯一入口。
//!
//! 线程模型：同步引擎；Database: Send+Sync（Arc 共享）；Session 单线程使用。
//! 一个 Database = 一个对象存储根上的库；一个 Session = 一条客户端连接。
//! SQL 执行逻辑在 `sql` 模块，本文件是状态与提交路径（SPEC 02/03/04 的交汇点）。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::format::row::decode_row;
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
pub enum StoreConfig {
    LocalDir(PathBuf),
    Memory,
    /// S3 兼容对象存储（AWS S3 / MinIO / RustFS / OSS）
    S3(crate::objstore::s3::S3Config),
    /// 注入预构建的存储栈（测试/自定义包装，如带统计的缓存栈）
    Obj(Arc<dyn ObjStore>),
}

impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreConfig::LocalDir(p) => write!(f, "LocalDir({p:?})"),
            StoreConfig::Memory => write!(f, "Memory"),
            StoreConfig::S3(c) => write!(f, "S3({}/{})", c.endpoint, c.bucket),
            StoreConfig::Obj(_) => write!(f, "Obj(...)"),
        }
    }
}

#[derive(Debug)]
pub struct DbOptions {
    pub store: StoreConfig,
    pub wal_flush_interval_ms: u64,
    pub wal_segment_bytes: u64,
    pub durability: Durability,
    /// checkpoint 触发阈值（pending 字节）
    pub checkpoint_threshold_bytes: u64,
    /// checkpoint 周期（秒）；0 = 只显式触发
    pub checkpoint_interval_s: u64,
    /// 读路径缓存字节预算（S3 后端；0 = 1GiB）
    pub cache_budget_bytes: u64,
    /// 写者租约 TTL（毫秒；P1 fencing，SPEC 02 §6）
    pub lease_ttl_ms: i64,
    /// 只读打开（读副本）：不领 epoch、不起 WAL writer、拒绝一切写。
    /// 打开已存在的库；空存储打开即报错（绝不创建对象）。
    pub read_only: bool,
    /// GC 保留窗口（毫秒）：墓碑对象登记后至少保留这么久，覆盖滞后读者。
    /// 默认 24h；< 0 = 禁用回收。
    pub gc_retention_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// 缓冲即 ack，不等上传。契约：崩溃/上传失败可能丢尾（毒化窗口内
    /// ack 的提交重启即失）——SPEC 02 §3.5；默认应为 Group。
    NoWait,
    /// 等待组提交落盘（本次或同批 flush durable 后返回）。
    Group,
    /// 等待自己的帧所在段上传成功。
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
            cache_budget_bytes: 1 << 30,
            lease_ttl_ms: 30_000,
            read_only: false,
            gc_retention_ms: 24 * 3600 * 1000,
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
    /// 本进程持有的写者世代（P1 fencing；0=未领取；只读分支=ref epoch）
    pub lease_epoch: AtomicU64,
    /// 只读分支（读副本）：不领租约、不起 WAL writer；fence_gate 拒绝一切写
    pub read_only: bool,
    /// 已截断 memtx 历史的最高 covered seq（Q-9：显式事务快照低于此值 =
    /// 冲突检测盲区 → 提交时显式 40001，杜绝静默丢失更新）
    pub covered_min: AtomicU64,
    /// 活跃显式事务快照注册表（R9-1：存在活跃快照即跳过 memtx 截断，
    /// 否则冻结读被 checkpoint 截断击穿）。**引用计数**（第十轮 R10-3：
    /// BTreeSet 去重使同 watermark 双事务共占一槽，先结束者连带摘除他人
    /// 保护）——value = 持有该快照的事务数
    pub active_snaps: Mutex<std::collections::BTreeMap<u64, usize>>,
    /// 写者租约 keep（与 flush_loop 的保活回调共享；P1 运行时拒写 + 空闲保活）
    pub lease: Arc<LeaseKeeper>,
    /// 已安装（可见）的提交水位
    pub watermark: AtomicU64,
    /// 自上次 checkpoint 的累积 pending 变更：table → (key → mut)
    pub pending: Mutex<HashMap<u32, BTreeMap<Vec<u8>, Mutation>>>,
    pub pending_bytes: AtomicU64,
}

/// 租约 + 下次续期时间（都在同一把锁内；next_renew_ms=0 表示立即可续）
pub struct LeaseState {
    pub lease: crate::objstore::fence::Lease,
    pub next_renew_ms: i64,
}

/// 分支写者租约的持有端：fence_gate 的检查/续期 + flush_loop 空闲保活共用。
pub struct LeaseKeeper {
    pub branch: String,
    pub ttl_ms: i64,
    pub fence: crate::objstore::fence::FenceStore,
    pub state: Mutex<LeaseState>,
}

impl LeaseKeeper {
    /// fence_gate 的**拒写检查**：租约过期 → 40001（自知失约者停止写入）
    pub fn check(&self) -> Result<()> {
        let st = self.state.lock();
        if st.lease.expires_at_ms <= crate::objstore::fence::now_ms() {
            return Err(SqlError::serialization(format!(
                "fencing: branch \"{}\" lease epoch {} expired — writer must re-open to acquire a new epoch",
                self.branch, st.lease.epoch
            )));
        }
        Ok(())
    }

    /// **惰性续期**：每 ttl/3 至多一次 PUT；失败仅告警（下次提交/保活重试，
    /// 过期后被 check 拒绝）。两个调用方：commit 路径的 fence_gate、
    /// flush_loop 的空闲保活（解决"30 秒无提交即永久 40001"的自毒化）。
    /// **PUT 在锁外执行**（P2-6）：锁内 clone 后释放锁再发 PUT——否则续期
    /// RTT 会阻塞 fence_gate（提交路径）、/readyz、/metrics 的 state 锁。
    /// 并发双触发（gate+保活同窗）无害：覆盖写幂等，写回取 max 防倒退。
    pub fn renew_if_due(&self) {
        let now = crate::objstore::fence::now_ms();
        let fresh = {
            let st = self.state.lock();
            if now < st.next_renew_ms || st.lease.expires_at_ms <= now {
                return; // 未到续期点 / 已过期（保活无权救活失约者，重开才能重获写权）
            }
            let mut f = st.lease.clone();
            f.expires_at_ms = now + self.ttl_ms;
            f
        }; // 锁已释放
        match self.fence.renew(&self.branch, &fresh) {
            Ok(()) => {
                let mut st = self.state.lock();
                if st.lease.epoch == fresh.epoch {
                    st.lease = fresh;
                    st.next_renew_ms = st.next_renew_ms.max(now + self.ttl_ms / 3);
                }
            }
            Err(e) => {
                tracing::warn!(branch = %self.branch, error = %e, "fence renew failed; retry on next commit/keepalive");
            }
        }
    }
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
    /// P1 fencing 运行时拒写（需已持 commit_mu）：过期 → 40001；健康路径
    /// 惰性续期。空闲流量下的保活由 flush_loop 回调承担（LeaseKeeper::renew_if_due）。
    pub(crate) fn fence_gate(&self) -> Result<()> {
        if self.read_only {
            return Err(SqlError::new("25006", "read-only branch: cannot write (open without read_only to acquire a lease)"));
        }
        if self.wal.poisoned() {
            // 毒化分支：快速失败且**不再续租**（第六轮委托点 b——否则 commit
            // 路径的 gate 会替不可用写者续命）。租约自然过期，接管者可接管。
            return Err(SqlError::new("40003",
                "wal writer poisoned by an earlier upload failure; reopen the branch to recover (transaction outcome may be unknown)"));
        }
        self.lease.check()?;
        self.lease.renew_if_due();
        Ok(())
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

/// 列存存储接口（由外部 crate 实现以避免依赖环；SPEC 05 §1 §6）
///
/// OSS 友好设计：**增量分段**——checkpoint 只把 memtx 增量写成新的不可变 CBF 段
/// （纯内存读，零树扫描、零远端读），扫描端按 pk 去重（新段优先）；
/// 段数超阈值时才全量重建（低频、一次写放大换长期读放大）。
pub trait ColumnarStore: Send + Sync {
    /// 写一个增量段。rows 为该 checkpoint 的可见增量行（纯内存输入）。
    fn write_segment(
        &self,
        obj: &Arc<dyn ObjStore>,
        table: &str,
        schema: &TableSchema,
        rows: &[Vec<crate::types::SqlValue>],
    ) -> Result<crate::versioned::ColSegment>;

    /// 全量重建：读整棵行树 + 与现有段合并去重，写单个段；返回 (新段, 待删旧段路径)。
    fn write_full(
        &self,
        obj: &Arc<dyn ObjStore>,
        store: &Arc<NodeStore>,
        root: &Hash,
        schema: &TableSchema,
        existing: &[crate::versioned::ColSegment],
    ) -> Result<(crate::versioned::ColSegment, Vec<String>)>;

    /// 扫描段集合（含段级 pk 剪枝）。调用方（core）负责 pk 去重与 delete 抑制。
    fn scan(
        &self,
        obj: &Arc<dyn ObjStore>,
        schema: &TableSchema,
        segments: &[crate::versioned::ColSegment],
        pk_range: &Option<(Option<u64>, Option<u64>)>,
    ) -> Result<Vec<arrow::record_batch::RecordBatch>>;
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
    /// 每-名字打开互斥（branch 创建 / reopen 驱逐串行化）
    open_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub(crate) session_seq: AtomicU64,
    /// 已写入 chunk 的进程内缓存（去重判定）
    #[allow(dead_code)]
    pub(crate) chunk_seen: Mutex<HashSet<Hash>>,
    stop_cp: Arc<std::sync::atomic::AtomicBool>,
    columnar: arc_swap::ArcSwap<Option<std::sync::Arc<dyn ColumnarStore>>>,
}

impl Database {
    /// 注入列存引擎（必须在打开后、写负载前）
    pub fn set_columnar(self: &Arc<Self>, c: std::sync::Arc<dyn ColumnarStore>) {
        self.columnar.store(std::sync::Arc::new(Some(c)));
    }
    pub(crate) fn columnar(&self) -> Option<std::sync::Arc<dyn ColumnarStore>> {
        self.columnar.load().as_ref().clone()
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
            StoreConfig::S3(cfg) => {
                let mut s3: Arc<dyn ObjStore> =
                    Arc::new(crate::objstore::s3::S3ObjStore::new(cfg.clone())?);
                // 慢网络模拟旋钮（公网/跨机房 OSS）：注入每次请求的 RTT
                if let Ok(ms) = std::env::var("DENDRO_S3_RTT_MS") {
                    if let Ok(mean) = ms.parse::<f64>() {
                        eprintln!("[s3] latency injection: {mean}ms RTT");
                        s3 = Arc::new(crate::objstore::throttled::ThrottledObjStore::new(
                            s3,
                            crate::objstore::throttled::LatencySpec {
                                mean_ms: mean / 2.0, // 单程
                                jitter_pct: 0.2,
                            },
                            16,
                        ));
                    }
                }
                // 读路径缓存（真 OSS 延迟下的可用性前提，SPEC 01 §7）
                let cache_dir = std::env::temp_dir().join(format!(
                    "dendro-cache-{}",
                    crate::objstore::cached::cache_key_public(&format!(
                        "{}{}",
                        cfg.endpoint, cfg.bucket
                    ))
                ));
                let cached = crate::objstore::cached::CachedObjStore::new(
                    s3,
                    cache_dir,
                    opts.cache_budget_bytes,
                )?;
                Arc::new(cached)
            }
            StoreConfig::Obj(a) => a.clone(),
        };
        let cas = Arc::new(CasStore::new(obj.clone()));
        let store = Arc::new(NodeStore::new(cas.clone(), 4096));
        let manifest_store = Arc::new(ManifestStore::new(obj.clone()));
        // 初始化 / 恢复
        if opts.read_only {
            // 只读打开绝不创建任何对象：库不存在（无 manifest）→ 明确报错
            if manifest_store.load_latest().is_err() {
                return Err(SqlError::io("read-only open: no database found at store root"));
            }
        } else if manifest_store.init(now_ms()).is_err() {
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
            open_locks: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(1),
            chunk_seen: Mutex::new(HashSet::new()),
            stop_cp: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            columnar: arc_swap::ArcSwap::from_pointee(None),
        });
        // 惰性打开（S-1）：不再启动即打开全部分支——每分支一线程 + 一租约
        // + 一次 fence 写，万级分支场景下弹性叙事不成立。分支在首次会话
        // 触达时按需创建（branch() 自带恢复回放）。
        let _ = ver;
        db.start_checkpoint_thread();
        // 打库回收 pass：清理上次运行遗留的到期墓碑（失败不阻塞打开）
        if let Err(e) = db.gc_sweep() {
            tracing::warn!("gc sweep on open: {e}");
        }
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

    /// 当前 manifest 快照（分支列表/提交历史等系统视图用）
    pub fn manifest(&self) -> Arc<DbSnapshot> {
        self.state.load_full()
    }

    /// 已驻留内存的分支快照（监控/负载自感知用）。
    /// 注意：与 `branch()` 不同，这里**绝不**懒加载——不会为仅存在于
    /// manifest 的分支领 epoch/起 WAL writer（否则读监控会产生写副作用）。
    pub fn active_branches(&self) -> Vec<Arc<Branch>> {
        self.branches.read().values().cloned().collect()
    }

    /// **重开分支**（毒化写者的进程内恢复入口，第五轮 P1）：
    /// 把驻留分支从 writer 注册表驱逐（旧 Arc 上的在途会话继续用旧 writer
    /// 并按毒化语义失败），随后按正常打开路径重新领取 epoch + 恢复回放——
    /// 恢复以 manifest + WAL 为准，裁决毒化期间的真实状态（WAL 失败 =
    /// 未提交；Uncertain 落盘者此时可见，客户端须对账，见 SPEC 02 §3.5）。
    pub fn reopen_branch(&self, name: &str) -> Result<Arc<Branch>> {
        // 每-名字串行化（第六轮 P1 并发边界）：并发 reopen / 并发 open 同名
        // 分支在此排队，不会出现双 epoch + 孤儿写者
        let _guard = self.open_lock(name);
        let old = self.branches.read().get(name).cloned();
        let Some(old) = old else {
            return self.branch_locked(name); // 未驻留：正常打开
        };
        // 守卫：只允许驱逐**毒化**写者——健康写者不可被 reopen 静默替换
        //（poisoned() 访问器的第二个读者，第六轮 P1）
        if !old.wal.poisoned() {
            return Ok(old);
        }
        // 清空在途提交后驱逐（commit_mu：在途会话要么完成要么已失败）
        let _g = old.commit_mu.lock();
        self.branches.write().remove(name);
        old.wal.close();
        self.branch_locked(name)
    }

    /// 每-名字打开互斥（创建/驱逐串行化；读快照路径不受影响）
    fn open_lock(&self, name: &str) -> Arc<Mutex<()>> {
        let mut g = self.open_locks.lock();
        g.entry(name.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    /// 读/建分支运行态（恢复路径也走这里：从 manifest 构造）。
    /// 创建段持每-名字互斥——此前两个线程同时首次打开同名分支会各自
    /// 领 epoch/起 WAL writer，败者仅靠 or_insert 丢弃（副作用已发生，
    /// 第六轮 P1 并发边界）。
    pub fn branch(&self, name: &str) -> Result<Arc<Branch>> {
        {
            let g = self.branches.read();
            if let Some(b) = g.get(name) {
                return Ok(b.clone());
            }
        }
        let _guard = self.open_lock(name);
        {
            // 双检：等锁期间可能已被其他线程创建
            let g = self.branches.read();
            if let Some(b) = g.get(name) {
                return Ok(b.clone());
            }
        }
        self.branch_locked(name)
    }

    /// 需已持 open_lock(name)
    fn branch_locked(&self, name: &str) -> Result<Arc<Branch>> {
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
        // P1：进程打开分支即领取新 epoch（世代化；fence 条件写保证唯一）。
        // 只读模式（读副本）：不领租约（不产生 fence 对象、不推进 epoch 序列）、
        // 不起 WAL flush 线程——评审 A7：此前读打开也制造新写者世代。
        let fence = crate::objstore::fence::FenceStore::new(self.obj.clone());
        let (lease_epoch, keeper, cfg) = if self.opts.read_only {
            let e = head_info.epoch.max(1);
            let cfg = crate::wal::WalConfig::from(&self.opts);
            (e, None, cfg)
        } else {
            let holder = format!("{}-{}", std::process::id(), self.session_seq.load(Ordering::Relaxed));
            let lease = fence.acquire(name, &holder, self.opts.lease_ttl_ms, head_info.epoch)?;
            let e = lease.epoch;
            // 租约 keep 与 WAL flush 线程共享：flush_loop 每次醒来调用保活回调
            // （自限频），空闲分支不再因 TTL 过期而永久 40001（评审 §3.1）
            let keeper = Arc::new(LeaseKeeper {
                branch: name.to_string(),
                ttl_ms: self.opts.lease_ttl_ms,
                fence: fence.clone(),
                state: Mutex::new(LeaseState { lease, next_renew_ms: 0 }),
            });
            let mut cfg = crate::wal::WalConfig::from(&self.opts);
            let k = keeper.clone();
            cfg.keepalive = Some(Arc::new(move || k.renew_if_due()));
            (e, Some(keeper), cfg)
        };
        let keeper = keeper.unwrap_or_else(|| {
            // 只读分支占位租约（expires_at_ms=0）；fence_gate 的 read_only 检查先于
            // check 拒绝一切写，metrics 跳过其 TTL 输出
            Arc::new(LeaseKeeper {
                branch: name.to_string(),
                ttl_ms: self.opts.lease_ttl_ms,
                fence: fence.clone(),
                state: Mutex::new(LeaseState {
                    lease: crate::objstore::fence::Lease {
                        epoch: lease_epoch,
                        holder: "read-only".into(),
                        expires_at_ms: 0,
                    },
                    next_renew_ms: 0,
                }),
            })
        });
        // 回放所有旧 epoch（1..=lease_epoch-1）；新 epoch 目录为空，随后写入
        let wal = if self.opts.read_only {
            WalWriter::open_read_only(self.obj.clone(), name, lease_epoch, 1, cfg)
        } else {
            WalWriter::open(self.obj.clone(), name, lease_epoch, 1, cfg)
        };
        let b = Arc::new(Branch {
            name: name.to_string(),
            head: ArcSwap::from_pointee(commit),
            wal,
            mem: BranchMem::default(),
            commit_mu: Mutex::new(()),
            next_seq: AtomicU64::new(0),
            lease_epoch: AtomicU64::new(lease_epoch),
            read_only: self.opts.read_only,
            covered_min: AtomicU64::new(0),
            active_snaps: Mutex::new(std::collections::BTreeMap::new()),
            lease: keeper,
            watermark: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            pending_bytes: AtomicU64::new(0),
        });
        // 恢复：回放 WAL 中未物化的事务（covered_seq 之后）
        crate::recovery::replay_branch(self, &b, head_info, lease_epoch)?;
        {
            let mut g = self.branches.write();
            g.entry(name.to_string()).or_insert(b.clone());
        }
        Ok(b)
    }

    /// 创建分支（O(1)：源分支 checkpoint 后写一条 ref；零数据复制）
    pub fn create_branch(&self, name: &str, from: &str) -> Result<()> {
        if self.branch_exists(name) {
            return Err(SqlError::duplicate_table(format!("branch \"{name}\" already exists")));
        }
        self.checkpoint_branch(from)?;
        let (src_commit, src_seg) = {
            let snap = self.manifest();
            let h = snap
                .manifest
                .refs
                .get(from)
                .ok_or_else(|| SqlError::undefined_branch(format!("branch \"{from}\" does not exist")))?;
            (h.commit.clone(), h.wal_seg)
        };
        self.update_manifest(|m| {
            m.refs.insert(
                name.to_string(),
                crate::objstore::manifest::BranchHead {
                    commit: src_commit.clone(),
                    wal_seg: 0,
                    parent: Some(from.to_string()),
                    fork_commit: src_commit.clone(),
                    fork_wal_seg: src_seg,
                    epoch: 0,
                    covered_seq: 0,
                    wal_first_seg: 0,
                },
            );
            Ok(true)
        })?;
        self.branch(name)?;
        Ok(())
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
        let base = self.common_ancestor(&sc, &dc)?;

        // 结构化目录三方合并：目录级冲突时逐表下推行级合并（SPEC 08 §6）
        let cat = crate::versioned::Versioned::new(self.store.clone());
        let be = cat.catalog_entries(base.as_ref().map(|c| c.root).as_ref())?;
        let le = cat.catalog_entries(Some(&sc.root))?;
        let re = cat.catalog_entries(Some(&dc.root))?;
        let mut session: HashSet<Hash> = HashSet::new();
        let cm = crate::versioned::merge::merge_catalog(&self.store, &be, &le, &re, &mut session)?;
        if !cm.conflicts.is_empty() {
            return Err(SqlError::serialization(format!(
                "merge conflict: {} tables conflicted ({})",
                cm.conflicts.len(),
                cm.conflicts.join("; ")
            )));
        }
        // 合并后的目录应用到 left 树（结构共享）→ 新目录根
        let new_root = {
            let mut ck = crate::prolly::Chunker::new(&self.store, &mut session);
            let muts: Vec<(Vec<u8>, crate::prolly::Mutation)> = cm
                .entries
                .iter()
                .map(|(n, e)| {
                    (
                        n.as_bytes().to_vec(),
                        crate::prolly::Mutation::Put(crate::versioned::encode_table_entry(e)),
                    )
                })
                .collect();
            ck.apply(Some(&sc.root), &muts)?
        };
        let root = new_root.expect("structured merge yields catalog root");
        self.write_branch_commit(dst, root, vec![dc.addr(), sc.addr()], "merge")?;
        Ok("MERGED".to_string())
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
        b.fence_gate()?;
        // 注（in-doubt 家族，低配版）：此处顺序为 head.store → WAL ck 帧 →
        // manifest。WAL 失败时 head 已推进而 manifest 未动——进程内后续 DDL
        // 可见"报错了的"新树，重启后以 manifest + 回放为准（收敛），客户端
        // 收到错误。与 commit_tx 的管线重排（P2'-1）不同，本路径的失败窗口
        // 由下次成功 checkpoint 收敛，v1 如实记录；随 P2'-2 统一重排。
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
            h.epoch = b.lease_epoch.load(Ordering::Acquire);
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
        // 只读库单一咽喉守卫（第七轮 R7-2）：此前只读副本可执行 DROP BRANCH
        // 等 manifest 写（CAS 在副本上成功 → 持久删除分支 + 墓碑化其对象）。
        // commit_tx / checkpoint 已由 fence_gate 各自拒绝；本函数覆盖其余
        // 全部 catalog 写（DROP/CREATE/MERGE/catalog_commit/…）。
        if self.opts.read_only {
            return Err(SqlError::new("25006", "read-only database: cannot execute statements that modify the catalog"));
        }
        for _ in 0..64 {
            let (ver, m) = self.manifest_store.load_latest().map_err(SqlError::from)?;
            let mut m = m;
            let changed = f(&mut m)?;
            if !changed {
                // 无需变更：快照仍刷新为刚读到的版本
                self.state.store(Arc::new(DbSnapshot { manifest: m }));
                return Ok(());
            }
            match self.manifest_store.commit(ver, m.clone()) {
                Ok(_new_ver) => {
                    // 自发布：commit 的就是我们刚构造的 m（版本号 new_ver），
                    // 直接作为本进程快照，无需再 LIST 刷新（P1-F：每次发布省 1 LIST）
                    self.state.store(Arc::new(DbSnapshot { manifest: m }));
                    return Ok(());
                }
                Err(crate::objstore::ObjError::Exists(_)) => continue, // 乐观冲突重试
                Err(e) => return Err(SqlError::from(e)),
            }
        }
        Err(SqlError::internal("manifest commit: too many conflicts"))
    }

    /// checkpoint 一个分支：pending 变更物化为树 + commit 对象 + manifest 推进。
    /// GC sweep 在 commit_mu **释放后**执行（含最多 256 次远端 DELETE，
    /// 锁内执行会阻塞该分支全部提交，评审 §3.2）
    pub fn checkpoint_branch(&self, branch_name: &str) -> Result<Option<Hash>> {
        let b = self.branch(branch_name)?;
        let out = {
            let _g = b.commit_mu.lock();
            b.fence_gate()?;
            self.checkpoint_locked(&b)
        };
        if let Err(e) = self.gc_sweep() {
            tracing::warn!("gc sweep: {e}");
        }
        out
    }

    /// 增量列存物化（需已持 commit_mu）：SPEC 05 §6。
    /// 返回被全量重建替换的旧段路径（调用方在同一 manifest 发布中登记墓碑）。
    fn materialize_delta(
        &self,
        col: &Arc<dyn ColumnarStore>,
        b: &Branch,
        ne: &mut crate::versioned::TableEntry,
        new_root: &Option<Hash>,
        schema: &TableSchema,
    ) -> Result<Vec<String>> {
        const COMPACT_SEGMENTS: usize = 8;
        const DELETE_CAP: usize = 10_000;
        let mut retired: Vec<String> = Vec::new();
        let snapshot = b.watermark.load(Ordering::Acquire);
        let overlay = b.mem.table(ne.id).snapshot_rows(snapshot);
        let delta_deletes: Vec<String> = overlay
            .iter()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.iter().map(|b| format!("{b:02x}")).collect::<String>())
            .collect();
        let delta_rows: Vec<Vec<SqlValue>> = overlay
            .values()
            .filter_map(|v| v.as_ref())
            .filter_map(|bytes| decode_row(bytes).ok())
            .collect();

        // 全量重建条件：无段（首次物化且表非空）/ 段过多 / 删除过多
        let need_full = (!ne.col_segments.is_empty() && ne.col_segments.len() >= COMPACT_SEGMENTS)
            || (ne.col_deletes.len() + delta_deletes.len() > DELETE_CAP);

        if need_full {
            if let Some(root) = new_root {
                let (seg, old_paths) =
                    col.write_full(&self.obj, &self.store, root, schema, &ne.col_segments)?;
                ne.col_segments = vec![seg];
                ne.col_deletes.clear();
                // 旧段不再被新 manifest 引用——墓碑随本次发布登记（GC 定案）
                retired.extend(old_paths);
            }
        } else if !delta_rows.is_empty() {
            let seg = col.write_segment(&self.obj, &ne.name, schema, &delta_rows)?;
            // 重新插入的 key：从 deletes 集合移除（删除不再抑制新值）
            let reinserted: std::collections::HashSet<String> = overlay
                .iter()
                .filter(|(_, v)| v.is_some())
                .map(|(k, _)| k.iter().map(|b| format!("{b:02x}")).collect::<String>())
                .collect();
            ne.col_deletes.retain(|k| !reinserted.contains(k));
            ne.col_segments.push(seg);
        }
        // 纯删除（无新行）：只累积 deletes
        if !delta_deletes.is_empty() && delta_rows.is_empty() {
            for d in delta_deletes {
                if !ne.col_deletes.contains(&d) {
                    ne.col_deletes.push(d);
                }
            }
        }
        ne.col_rows = ne.col_segments.iter().map(|s| s.rows).sum();
        Ok(retired)
    }

    /// 需已持 commit_mu。
    /// **失败安全（P0-B 修复）**：pending 被 take 后任何 OSS 错误都把它**合并
    /// 归还**（键覆盖合并，commit_mu 保证无并发写者）——否则下次成功 checkpoint
    /// 会推进 covered_seq，越过这些已 ack 事务的 WAL 帧，重启回放跳过 = 数据
    /// 永久丢失。归还不改 memtx/head（checkpoint 的全部远端写失败即无效果）；
    /// 若错误发生在 head.store 之后（WAL ck 帧失败），head 已前进而 manifest
    /// 未动——归还的 pending 会在下次 checkpoint 以新 head 为父重新发布，收敛。
    pub(crate) fn checkpoint_locked(&self, b: &Branch) -> Result<Option<Hash>> {
        let pending: HashMap<u32, BTreeMap<Vec<u8>, Mutation>> =
            std::mem::take(&mut *b.pending.lock());
        b.pending_bytes.store(0, Ordering::Release);
        let out = self.checkpoint_locked_inner(b, &pending);
        if out.is_err() {
            let mut bytes = 0usize;
            {
                let mut pend = b.pending.lock();
                for (tid, muts) in pending {
                    let slot = pend.entry(tid).or_default();
                    for (k, v) in muts {
                        bytes += k.len() + match &v {
                            crate::prolly::Mutation::Put(val) => val.len(),
                            crate::prolly::Mutation::Delete => 0,
                        };
                        slot.insert(k.clone(), v);
                    }
                }
            }
            b.pending_bytes.fetch_add(bytes as u64, Ordering::Release);
        }
        out
    }

    fn checkpoint_locked_inner(
        &self,
        b: &Branch,
        pending: &HashMap<u32, BTreeMap<Vec<u8>, Mutation>>,
    ) -> Result<Option<Hash>> {
        let old_head = b.head.load_full();
        let old_catalog = old_head.as_ref().as_ref().map(|c| c.root);
        let mut session: HashSet<Hash> = HashSet::new();
        let catalog = crate::versioned::Versioned::new(self.store.clone());
        let mut changes: Vec<(String, Option<crate::versioned::TableEntry>)> = Vec::new();
        let mut gc_retired: Vec<String> = Vec::new(); // 本次替换的列存段（墓碑登记）
        // pending 按表应用
        let _snap = self.manifest();
        for (tid, muts) in pending {
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
            // 列存增量物化（OSS 友好：纯内存输入，零树扫描、零远端读）
            // delta = 本 checkpoint 的 memtx overlay（上次 covered_seq 之后的全部可见行）
            ne.col_rows = ne.col_segments.iter().map(|s| s.rows).sum();
            if let Some(col) = self.columnar() {
                if let Ok(schema) = catalog.load_schema_with_entry(&ne) {
                    match self.materialize_delta(&col, b, &mut ne, &new_root, &schema) {
                        Ok(mut old) => gc_retired.append(&mut old),
                        Err(e) => tracing::warn!("materialize {}: {e}", ne.name),
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
        let seq = crate::recovery::composite_ts(b.lease_epoch.load(Ordering::Acquire), b.alloc_seq());
        let ck = crate::wal::CheckpointRecord {
            catalog_root: commit.root,
            commit_addr: commit.addr(),
            seq_covered: b.watermark.load(Ordering::Acquire),
        };
        b.wal
            .append(crate::wal::FrameType::Checkpoint, seq, &crate::wal::encode_checkpoint(&ck), Durability::Always)?;
        // manifest：commit + 当前已 flush 段 + covered_seq
        // GC 墓碑（与"停止引用"同一原子发布，GC 定案）：
        // ① 全量重建替换的列存段；② WAL 当前 epoch 前缀段（checkpoint 帧之前的
        //    段全部 covered）；③ 旧 epoch 整目录（本分支首个 checkpoint 后全部
        //    covered——回放水位含旧 epoch 的全部 ts）
        let seg_now = b.wal.current_seg().saturating_sub(1);
        let covered = ck.seq_covered;
        let cur_epoch = b.lease_epoch.load(Ordering::Acquire);
        let mut tombstone: Vec<String> = std::mem::take(&mut gc_retired);
        let first = b.wal.first_seg();
        for seg in first..seg_now {
            tombstone.push(crate::wal::WalWriter::seg_path(&b.name, cur_epoch, seg));
        }
        if cur_epoch > 1 {
            // 旧 epoch 目录已全部 covered，逐段登记墓碑。**每次 checkpoint 重
            // 新 LIST**（而非仅本 epoch 首次）：失约写者可能在接管者的首次
            // 快照之后仍向旧 epoch 追加滞后段，重 LIST 保证最终覆盖（评审
            // "僵尸 epoch 段"）。低频路径，允许 LIST；墓碑按 path 去重。
            for epoch in 1..cur_epoch {
                let prefix = format!("wal/{}/e{epoch:020}/", b.name);
                if let Ok(paths) = self.obj.list_prefix(&prefix) {
                    tombstone.extend(paths);
                }
            }
        }
        let t_now = now_ms();
        let bname = b.name.clone();
        let epoch_for_head = cur_epoch;
        self.update_manifest(|m| {
            let h = m.refs.get_mut(&bname).ok_or_else(|| SqlError::internal("branch vanished"))?;
            h.commit = Some(commit.addr().to_base32());
            h.wal_seg = seg_now.max(h.wal_seg);
            h.covered_seq = covered;
            h.epoch = epoch_for_head;
            h.wal_first_seg = seg_now.max(h.wal_first_seg);
            for p in &tombstone {
                if !m.tombstones.iter().any(|t| t.path == *p) {
                    m.tombstones.push(crate::objstore::manifest::Tombstone { path: p.clone(), at_ms: t_now });
                }
            }
            Ok(true)
        })?;
        b.wal.set_first_seg(seg_now);
        // 释放 memtx 历史版本。**截断水位尊重最老活跃快照**（第九轮 R9-1）：
        // 显式事务冻结读依赖 memtx 保留其快照可见的版本——无脑截断到 covered
        // 会把"BEGIN 前已提交、BEGIN 后首次物化"的行从事务内静默抹掉。
        // （Q-9 的写侧 40001 不变：写冲突检测的盲区与读保留是两回事。）
        // 活跃显式事务存在 ⇒ 本轮不截断：其冻结读依赖 memtx 保留快照可见
        // 版本（v1 保守口径——内存随最老事务生命周期增长，已知权衡，精细化
        // 按版本保留列 TASK Q-13）。无活跃事务时正常截断。
        // covered_min **只在真截断时推进**（第十轮 P10-5）：截断被跳过时冲突
        // 检测并未致盲，推进会使保留窗口内的提交被误拒 40001。
        let has_active = !b.active_snaps.lock().is_empty();
        if !has_active {
            b.mem.truncate_all(covered);
            b.covered_min.store(covered, Ordering::Release);
        }
        Ok(Some(commit.addr()))
    }

    /// **优雅关闭**（Q-12）：停自动 checkpoint 线程 → 对全部驻留分支
    /// close_graceful（上传 WAL 剩余缓冲后停线程）。调用后本进程不再
    /// 接受新会话；调用方（serve 收到 SIGTERM/SIGINT）随后退出。
    pub fn shutdown(self: &Arc<Self>) {
        self.stop_cp.store(true, Ordering::Relaxed);
        let branches: Vec<Arc<Branch>> = self
            .branches
            .write()
            .drain()
            .map(|(_, b)| b)
            .collect();
        for b in branches {
            b.wal.close_graceful();
        }
    }

    /// GC 回收 pass（GC 定案，docs/design/GC定案.md）：
    /// ① 删除保留窗口已过的墓碑对象（单批 ≤256 个，有界）；② 压缩墓碑清单；
    /// ③ 回收旧 manifest 版本（保留最近 16 个）。
    /// 只读库/禁用（gc_retention_ms < 0）时为空操作。
    pub fn gc_sweep(&self) -> Result<usize> {
        if self.opts.read_only || self.opts.gc_retention_ms < 0 {
            return Ok(0);
        }
        let deadline = now_ms() - self.opts.gc_retention_ms;
        let (latest, m) = self.manifest_store.load_latest().map_err(SqlError::from)?;
        let mut deleted = 0usize;
        let due: Vec<String> = m
            .tombstones
            .iter()
            .filter(|t| t.at_ms <= deadline)
            .map(|t| t.path.clone())
            .take(256)
            .collect();
        if !due.is_empty() {
            let mut removed: Vec<String> = Vec::new();
            for p in &due {
                if self.obj.delete(p).is_ok() {
                    deleted += 1;
                    removed.push(p.clone());
                }
                // 删除失败的对象**保留墓碑**（at_ms 不变），下轮重试；
                // 剔除失败者会造成永久孤儿（评审 §3.2）
            }
            if !removed.is_empty() {
                self.update_manifest(|m2| {
                    m2.tombstones.retain(|t| !removed.contains(&t.path));
                    m2.gc_last_sweep_ver = latest; // 水位随压缩顺带发布（P2-B）
                    Ok(true)
                })?;
            }
        }
        // 旧 manifest 版本：保留最近 16（滞后读者兜底；JSON 极小、LIST 权威路径兼容空洞）
        let vers = self.manifest_store.retained(latest, 16);
        deleted += self.manifest_store.delete_versions(&vers);
        // gc_last_sweep_ver 已在上方墓碑压缩的发布里顺带更新（P2-B：不独立
        // 发布——独立递增会使每 checkpoint 的 manifest 版本推进 ×2，keep-16
        // 的停滞写者安全垫减半）
        Ok(deleted)
    }


    fn start_checkpoint_thread(self: &Arc<Self>) {
        // **持 Weak**（第六轮发现）：线程持 Arc<Database> 自环 ⇒ Database 永不
        // Drop ⇒ 遗弃分支的 Branch/WalWriter/租约保活全部永生——GC 删除已删
        // 分支的 fence 对象后会被"复活"（回归 lazy_open_and_drop_branch_gc）。
        let db = Arc::downgrade(self);
        let interval = self.opts.checkpoint_interval_s.max(1);
        let threshold = self.opts.checkpoint_threshold_bytes;
        std::thread::Builder::new()
            .name("dendro-checkpoint".into())
            .spawn(move || loop {
                // **先 sleep 再 upgrade**（第六轮发现）：若在 upgrade 持有期
                // sleep，Database 在整个 sleep 期间存活（30s/轮）——遗弃分支
                // 的 Branch/WalWriter/租约保活随之"永生"，GC 删除已删分支的
                // fence 对象后会被复活（回归 lazy_open_and_drop_branch_gc）。
                std::thread::sleep(std::time::Duration::from_secs(interval));
                let Some(db) = db.upgrade() else {
                    return; // 外部 Arc 全释放：Database 已死，检查点线程随之退出
                };
                if db.stop_cp.load(Ordering::Relaxed) {
                    return;
                }
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
                        if let Err(e) = db.checkpoint_branch(&n) {
                            tracing::error!(branch = %n, error = %e, "auto checkpoint failed; pending retained");
                        }
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
            keepalive: None,
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

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(t) = self.txn.take() {
            self.unregister_snapshot(&t.snapshot);
        }
    }
}

impl Session {
    /// 注销活跃快照（事务结束或会话放弃；分支可能已被驱逐——查无则忽略）。
    /// **引用计数递减**（R10-3）：同 watermark 的并发事务各自持一槽
    pub(crate) fn unregister_snapshot(&self, snapshot: &u64) {
        if let Ok(b) = self.db.branch(&self.branch) {
            let mut g = b.active_snaps.lock();
            if let Some(c) = g.get_mut(snapshot) {
                *c -= 1;
                if *c == 0 {
                    g.remove(snapshot);
                }
            }
        }
    }
    /// 执行一段 SQL（可含多语句，`;` 分隔）；空/纯注释 → 空 Vec
    pub fn exec(&mut self, sql: &str) -> Result<Vec<Output>> {
        let db = self.db.clone();
        let result = crate::sql::exec_batch(&db, self, sql);
        if result.is_err() && self.txn.is_some() {
            self.failed_txn = true;
        }
        result
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

/// 提交一个事务的写集（engine 内部路径，sql 模块调用）。
/// **管线顺序 = 裁决 → 持久化 → 安装 → 水位**（P2' 提交管线重构，
/// docs/design/提交管线重构.md；第三轮评审 §5.5 验收项："install 必须在
/// durable 之后"）。旧顺序先安装 memtx 再写 WAL，WAL 失败时 memtx 已带
/// 数据而客户端收到错误（in-doubt：未提交数据可见 + 重启后幽灵行）；
/// 新顺序下 WAL 失败 ⇒ 无任何可见状态，错误如实。
/// 安全性：各阶段都在 commit_mu 内，validate 与 install 之间无并发写者
/// （裁决结果不会被并发提交作废）。
pub(crate) fn commit_tx(db: &Database, sess_branch: &str, txn: &Txn) -> Result<u64> {
    let b = db.branch(sess_branch)?;
    let _g = b.commit_mu.lock();
    b.fence_gate()?;
    // Q-9：显式事务快照早于最近一次 checkpoint 的截断水位 ⇒ 该事务的冲突
    // 检测存在盲区（memtx 历史已截断、树只有最新态）——静默 last-writer-wins
    // 会造成丢失更新，改为显式 40001（客户端重试即获得完整视图）
    if txn.explicit && txn.snapshot < b.covered_min.load(Ordering::Acquire) {
        return Err(SqlError::serialization(
            "transaction spans a checkpoint; conflict detection unavailable — retry",
        ));
    }
    let epoch = b.lease_epoch.load(Ordering::Acquire);
    let seq = b.alloc_seq();
    let ts = crate::recovery::composite_ts(epoch, seq);
    // Phase 1: OCC 裁决（只验证不安装；SPEC 04 §3 first-committer-wins）
    crate::memtx::validate_only(&b.mem, &[], txn)?;
    // Phase 2: 持久化（组提交；失败 → 无可见状态，错误上抛）
    let mut recs: Vec<crate::wal::TxnRecord> = Vec::new();
    for (tid, key, m) in iter_writes(txn) {
        let val = match m {
            Mutation::Put(v) => Some(v.clone()),
            Mutation::Delete => None,
        };
        recs.push(crate::wal::TxnRecord { table_id: tid, ops: vec![(key.clone(), val)] });
    }
    b.wal.append(crate::wal::FrameType::Txn, ts, &crate::wal::encode_txn(&recs), db.opts.durability)?;
    // Phase 3: 安装（durable 之后才产生可见状态）+ pending 登记（checkpoint 积压）
    crate::memtx::install(&b.mem, &[], txn, ts);
    let mut plen = 0usize;
    {
        let mut pend = b.pending.lock();
        for (tid, key, m) in iter_writes(txn) {
            plen += key.len()
                + match m {
                    Mutation::Put(v) => v.len(),
                    Mutation::Delete => 0,
                };
            pend.entry(tid).or_default().insert(key.clone(), m.clone());
        }
    }
    b.pending_bytes.fetch_add(plen as u64, Ordering::Release);
    // Phase 4: 可见性推进
    b.watermark.store(ts, Ordering::Release);
    Ok(ts)
}

fn iter_writes(txn: &Txn) -> impl Iterator<Item = (u32, &Vec<u8>, &Mutation)> {
    txn.writes.iter().map(|((t, k), m)| (*t, k, m))
}

// —— wire 层 API（真身）——

