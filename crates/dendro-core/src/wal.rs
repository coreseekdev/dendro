//! WAL：只写日志直写对象存储（SPEC 02）。
//! 段 = 一个不可变对象；组提交摊薄 RTT；durable 回调推进水位。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::objstore::ObjStore;
use bytes::Bytes;
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// M-5：进程级 WAL flush PUT 延迟累计（微秒 / 次数）
pub static FLUSH_LATENCY_US: AtomicU64 = AtomicU64::new(0);
pub static FLUSH_LATENCY_CNT: AtomicU64 = AtomicU64::new(0);
use std::time::Duration;

pub const FRAME_MAGIC: u32 = 0x4F524E44; // "DRNO" LE 视觉可辨
pub const FRAME_VERSION: u16 = 1;
pub const SEGMENT_MAGIC: u32 = 0x4C415345; // "ESAL"
pub const HEADER_LEN: usize = 24;
pub const TRAILER_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Txn = 1,
    Checkpoint = 2,
    Fence = 3,
    Seal = 4,
}

impl FrameType {
    fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => FrameType::Txn,
            2 => FrameType::Checkpoint,
            3 => FrameType::Fence,
            4 => FrameType::Seal,
            _ => return None,
        })
    }
}

/// TXN 帧 payload：一个事务的全部行变更
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnRecord {
    pub table_id: u32,
    /// (key, val)；val=None = 删除
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

pub fn encode_txn(records: &[TxnRecord]) -> Vec<u8> {
    let mut d = Vec::with_capacity(256);
    d.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for r in records {
        d.extend_from_slice(&r.table_id.to_le_bytes());
        d.extend_from_slice(&(r.ops.len() as u32).to_le_bytes());
        for (k, v) in &r.ops {
            d.extend_from_slice(&(k.len() as u32).to_le_bytes());
            d.extend_from_slice(k);
            match v {
                Some(val) => {
                    d.extend_from_slice(&(val.len() as u32).to_le_bytes());
                    d.extend_from_slice(val);
                }
                None => d.extend_from_slice(&u32::MAX.to_le_bytes()),
            }
        }
    }
    d
}

pub fn decode_txn(payload: &[u8]) -> Result<Vec<TxnRecord>> {
    let mut r = payload;
    let need = |r: &[u8], n: usize| -> Result<()> {
        if r.len() < n {
            Err(SqlError::internal("wal txn truncated"))
        } else {
            Ok(())
        }
    };
    need(r, 4)?;
    let ntables = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
    r = &r[4..];
    let mut out = Vec::with_capacity(ntables);
    for _ in 0..ntables {
        need(r, 8)?;
        let table_id = u32::from_le_bytes(r[..4].try_into().unwrap());
        let nops = u32::from_le_bytes(r[4..8].try_into().unwrap()) as usize;
        r = &r[8..];
        let mut ops = Vec::with_capacity(nops);
        for _ in 0..nops {
            need(r, 4)?;
            let klen = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
            r = &r[4..];
            need(r, klen)?;
            let key = r[..klen].to_vec();
            r = &r[klen..];
            need(r, 4)?;
            let vlen = u32::from_le_bytes(r[..4].try_into().unwrap());
            r = &r[4..];
            if vlen == u32::MAX {
                ops.push((key, None));
            } else {
                need(r, vlen as usize)?;
                ops.push((key, Some(r[..vlen as usize].to_vec())));
                r = &r[vlen as usize..];
            }
        }
        out.push(TxnRecord { table_id, ops });
    }
    Ok(out)
}

/// CHECKPOINT 帧 payload
#[derive(Debug, Clone)]
pub struct CheckpointRecord {
    pub catalog_root: Hash,
    pub commit_addr: Hash,
    pub seq_covered: u64,
}

pub fn encode_checkpoint(c: &CheckpointRecord) -> Vec<u8> {
    let mut d = Vec::with_capacity(44);
    d.extend_from_slice(c.catalog_root.as_bytes());
    d.extend_from_slice(c.commit_addr.as_bytes());
    d.extend_from_slice(&c.seq_covered.to_le_bytes());
    d
}

pub fn decode_checkpoint(payload: &[u8]) -> Result<CheckpointRecord> {
    if payload.len() < 44 {
        return Err(SqlError::internal("wal ckpt truncated"));
    }
    let mut a = [0u8; 20];
    a.copy_from_slice(&payload[..20]);
    let catalog_root = Hash::from_bytes(a);
    a.copy_from_slice(&payload[20..40]);
    let commit_addr = Hash::from_bytes(a);
    let seq_covered = u64::from_le_bytes(payload[40..48].try_into().unwrap());
    Ok(CheckpointRecord {
        catalog_root,
        commit_addr,
        seq_covered,
    })
}

pub fn encode_frame(ty: FrameType, seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(HEADER_LEN + payload.len());
    f.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    f.extend_from_slice(&FRAME_VERSION.to_le_bytes());
    f.extend_from_slice(&(ty as u16).to_le_bytes());
    f.extend_from_slice(&seq.to_le_bytes());
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(&crc32c::crc32c(payload).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

/// 段尾：magic + frame_count + min/max seq + crc(覆盖段尾前 28B)
pub fn encode_trailer(frame_count: u64, min_seq: u64, max_seq: u64) -> [u8; TRAILER_LEN] {
    let mut t = [0u8; TRAILER_LEN];
    t[..4].copy_from_slice(&SEGMENT_MAGIC.to_le_bytes());
    t[4..12].copy_from_slice(&frame_count.to_le_bytes());
    t[12..20].copy_from_slice(&min_seq.to_le_bytes());
    t[20..28].copy_from_slice(&max_seq.to_le_bytes());
    let crc = crc32c::crc32c(&t[..28]);
    t[28..32].copy_from_slice(&crc.to_le_bytes());
    t
}

// ---------------------------------------------------------------------------
// 段迭代/回放
// ---------------------------------------------------------------------------

pub struct FrameIter<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> FrameIter<'a> {
    pub fn new(seg: &'a [u8]) -> Self {
        Self { data: seg, off: 0 }
    }
    /// 返回 (type, seq, payload)；段尾 32B trailer 不作为帧产出。
    /// 对不可信输入（截断/坏 len）返回 Err 而非 panic（P0-2 修复）。
    pub fn next_frame(&mut self) -> Option<Result<(FrameType, u64, &'a [u8])>> {
        if self.off >= self.data.len() || self.data.len() - self.off <= TRAILER_LEN {
            return None;
        }
        if self.off + HEADER_LEN > self.data.len() {
            return None;
        }
        let d = &self.data[self.off..];
        if d.len() < HEADER_LEN {
            return None;
        }
        let magic = u32::from_le_bytes(d[..4].try_into().unwrap());
        if magic != FRAME_MAGIC {
            return Some(Err(SqlError::internal("wal frame magic")));
        }
        let ver = u16::from_le_bytes(d[4..6].try_into().unwrap());
        if ver != FRAME_VERSION {
            return Some(Err(SqlError::internal("wal frame version")));
        }
        let ty = match FrameType::from_u16(u16::from_le_bytes(d[6..8].try_into().unwrap())) {
            Some(t) => t,
            None => return Some(Err(SqlError::internal("wal frame type"))),
        };
        let seq = u64::from_le_bytes(d[8..16].try_into().unwrap());
        let len = u32::from_le_bytes(d[16..20].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(d[20..24].try_into().unwrap());
        // P0-2 修复：len 不可信——切片前校验
        if HEADER_LEN + len > d.len() {
            return Some(Err(SqlError::internal("wal frame payload exceeds segment")));
        }
        let payload = &d[HEADER_LEN..HEADER_LEN + len];
        if crc32c::crc32c(payload) != crc {
            return Some(Err(SqlError::internal("wal frame crc")));
        }
        self.off += HEADER_LEN + len;
        Some(Ok((ty, seq, payload)))
    }
}

// ---------------------------------------------------------------------------
// 写入器：每分支一个；后台 flush 线程组提交
// ---------------------------------------------------------------------------

pub struct WalConfig {
    pub flush_interval: Duration,
    pub segment_bytes: u64,
    pub durability: crate::engine::Durability,
    /// 租约保活回调（engine 注入；flush_loop 每次醒来调用一次，回调自限频）。
    /// 解决"空闲写者自毒化"：惰性续期只挂在 commit 路径上，无流量的分支
    /// TTL 到期后所有提交被 40001 拒绝且无自愈（第三轮评审 §3.1）。
    pub keepalive: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

struct WalShared {
    buf: Vec<u8>, // 当前段缓冲（帧序列）
    frames: u64,
    min_seq: u64,
    max_seq: u64,
    cur_seg: u64,        // 正在写的段号
    flushed_seg: u64,    // 已成功上传的最高段
    durable_seq: u64,    // 已 durable 的最高 seq
    pending_frames: u64, // 当前缓冲帧数（唤醒用）
    /// 追加模式（P2-6e，仅 supports_append 存储）：当前打开段累计字节数
    /// （0 = 未打开/不支持）。段按 segment_bytes 封口（追加 trailer），
    /// 段数从"每 flush 一个"降到"每 segment_bytes 一个"
    seg_appended: u64,
    seg_frames_total: u64, // 当前打开段累计帧数（封段 trailer 用）
    seg_min_seq_open: u64, // 当前打开段最小 seq（封段 trailer 用）
    /// **写者毒化（P0-D 错误语义定案）**：任何 flush PUT 失败后置位。
    /// 置位后：append 一律拒绝（SQLSTATE 40003 completion_unknown），
    /// flush_loop 停止上传（确定失败的帧绝不持久化——错误 = 未提交）；
    /// 唯一恢复路径是 reopen（新 writer + 恢复回放裁决真实状态）。
    /// Uncertain（PUT 可能已成功）：帧保留在缓冲但不上传，对象若已落盘
    /// 则 reopen 后回放可见——该事务结果**未知**，客户端须对账（SPEC 02 §3.5）。
    poisoned: bool,
}

pub struct WalWriter {
    obj: Arc<dyn ObjStore>,
    branch: String,
    /// 只读 writer（读副本）：append 一律 25006（第七轮 R7-2——DDL 的 WAL
    /// 帧先于 update_manifest 到达，需在此给出正确错误码而非 58030）
    read_only: bool,
    epoch: u64,
    cfg: WalConfig,
    shared: Mutex<WalShared>,
    cv: Condvar,
    /// 上传单飞互斥（P0-A）：同一时刻至多一个 flush_now 在途
    flush_mu: Mutex<()>,
    /// GC 起始段号：之前的段已登记墓碑且 covered（GC 定案）
    first_seg: AtomicU64,
    /// 每-段最大 seq（P2-6 段退休安全界）：seg → 该段内最大帧 seq。
    /// 两段式提交下 durable-but-in-flight 的帧可落在 checkpoint 帧之前的段里
    /// ——段退休（wal_first_seg 推进）必须以"段内全部帧已安装"为界
    /// （retire_bound：max seq 的 ts ≤ covered 才可退休），否则重启回放跳过
    /// 含在途帧的段 = 已 ack 提交丢失（审计 R3-P0 实证）。
    seg_max_seq: Mutex<std::collections::BTreeMap<u64, u64>>,
    /// 首个未上传段号（恢复起点提示；进程内缓存）
    stop: Mutex<bool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl WalWriter {
    pub fn open(
        obj: Arc<dyn ObjStore>,
        branch: &str,
        epoch: u64,
        start_seg: u64,
        cfg: WalConfig,
    ) -> Arc<WalWriter> {
        let w = Arc::new(Self {
            obj,
            branch: branch.to_string(),
            read_only: false,
            epoch,
            cfg,
            shared: Mutex::new(WalShared {
                buf: Vec::new(),
                frames: 0,
                min_seq: 0,
                max_seq: 0,
                cur_seg: start_seg,
                flushed_seg: start_seg.saturating_sub(1),
                durable_seq: 0,
                pending_frames: 0,
                seg_appended: 0,
                seg_frames_total: 0,
                seg_min_seq_open: 0,
                poisoned: false,
            }),
            cv: Condvar::new(),
            flush_mu: Mutex::new(()),
            seg_max_seq: Mutex::new(std::collections::BTreeMap::new()),
            first_seg: AtomicU64::new(start_seg),
            stop: Mutex::new(false),
            handle: Mutex::new(None),
        });
        // 后台 flush 线程：**持 Weak**（第六轮发现——线程持 Arc 自环 ⇒ Drop
        // 永不触发，遗弃分支（drop/reopen 后）的线程+租约保活永生，会在 GC
        // 删除后"复活"已删分支的 fence 对象）。外部 Arc 全部释放 ⇒ upgrade
        // 失败 ⇒ 线程退出。
        let w2 = Arc::downgrade(&w);
        let h = std::thread::Builder::new()
            .name(format!("wal-{branch}-e{epoch}"))
            .spawn(move || {
                while let Some(w) = w2.upgrade() {
                    if w.tick_once() {
                        return;
                    }
                    // 事件驱动（P2-6b）：仍有帧（PUT 在途期间新到的）→ 立即
                    // 续刷；空闲则挂在条件变量上（enqueue/stop/durable 推进
                    // 都会 notify），flush_interval 仅作保活节拍上限——
                    // 租约续期回调靠它周期执行（见 tick_once）。
                    // ⚠ 毒化时帧永久滞留缓冲（pending_frames > 0 恒真），
                    // 必须一并挂起——否则 100% CPU 热旋到 reopen（审计 R4-F1）。
                    let mut g = w.shared.lock();
                    if !g.poisoned && g.pending_frames > 0 {
                        drop(g);
                        continue;
                    }
                    let _timeout = w.cv.wait_for(&mut g, w.cfg.flush_interval);
                    drop(g);
                }
            })
            .expect("spawn wal thread");
        *w.handle.lock() = Some(h);
        w
    }

    /// 只读构造（读副本）：不占段号、不起 flush 线程（stop=true 且无 handle）。
    /// append 永不应到达此处——上游 fence_gate 已拒绝只读写；即使到达，
    /// NoWait 只进缓冲且永不 flush（stop=true），不会向存储发出任何 PUT。
    pub fn open_read_only(
        obj: Arc<dyn ObjStore>,
        branch: &str,
        epoch: u64,
        start_seg: u64,
        cfg: WalConfig,
    ) -> Arc<WalWriter> {
        Arc::new(Self {
            obj,
            branch: branch.to_string(),
            read_only: true,
            epoch,
            cfg,
            shared: Mutex::new(WalShared {
                buf: Vec::new(),
                frames: 0,
                min_seq: 0,
                max_seq: 0,
                cur_seg: start_seg,
                flushed_seg: start_seg.saturating_sub(1),
                durable_seq: 0,
                pending_frames: 0,
                seg_appended: 0,
                seg_frames_total: 0,
                seg_min_seq_open: 0,
                poisoned: false,
            }),
            cv: Condvar::new(),
            flush_mu: Mutex::new(()),
            seg_max_seq: Mutex::new(std::collections::BTreeMap::new()),
            first_seg: AtomicU64::new(start_seg),
            stop: Mutex::new(true),
            handle: Mutex::new(None),
        })
    }

    /// 段路径：wal/{branch}/e{epoch:020}/{seg:020}.wal
    /// epoch 进路径 ⇒ 陈旧写者的段落在低 epoch 目录，恢复按 epoch 升序重放、
    /// 高 epoch 覆盖（脑裂安全，P1 设计文档 §3.2）
    pub fn seg_path(branch: &str, epoch: u64, seg: u64) -> String {
        format!("wal/{branch}/e{epoch:020}/{seg:020}.wal")
    }

    /// 追加一帧并按 durability 语义等待。返回 durable（或缓冲后）seq。
    /// 毒化后一律拒绝（P0-D：不得在结果未知的状态上叠加写）。
    pub fn append(
        &self,
        ty: FrameType,
        seq: u64,
        payload: &[u8],
        durability: crate::engine::Durability,
    ) -> Result<()> {
        // NoWait 帧不唤醒刷盘（搭车：下一组持久刷盘/段满/空闲节拍兜底）；
        // Group/Always 立即唤醒（事件驱动，延迟 ≈ PUT 而非定时器间隔）
        self.enqueue_only(
            ty,
            seq,
            payload,
            durability != crate::engine::Durability::NoWait,
        )?;
        match durability {
            crate::engine::Durability::NoWait => Ok(()),
            crate::engine::Durability::Always => {
                self.flush_now()?;
                self.await_durable(seq)
            }
            crate::engine::Durability::Group => self.await_durable(seq),
        }
    }

    /// 仅入队（P2-6 组提交解耦）：缓冲追加，**不等待 durable**。
    /// `notify`：调用方是否需要 durable 尽快达成（Group/Always = true）——
    /// true 即刻唤醒刷盘线程（事件驱动）；false（NoWait）不唤醒，帧搭车到
    /// 下一组持久刷盘/段满/空闲节拍（防每帧一段的对象爆炸与无谓唤醒）。
    /// 供两段式提交使用——validate+enqueue 持 commit_mu，durable 等待在锁外
    /// 并发进行（多提交并入同一组刷盘），install 在 durable 之后二次持锁。
    /// 与 [`Self::append`] 相同的毒化/只读守卫。
    pub fn enqueue_only(
        &self,
        ty: FrameType,
        seq: u64,
        payload: &[u8],
        notify: bool,
    ) -> Result<()> {
        if self.read_only {
            return Err(SqlError::new("25006", "read-only branch: cannot write"));
        }
        if self.shared.lock().poisoned {
            return Err(SqlError::new("40003",
                "wal writer poisoned by an earlier upload failure; reopen the branch to recover (transaction outcome may be unknown)"));
        }
        let frame = encode_frame(ty, seq, payload);
        let _size = frame.len();
        let full;
        {
            let mut g = self.shared.lock();
            if g.buf.is_empty() {
                g.min_seq = seq;
            }
            g.max_seq = seq;
            g.frames += 1;
            g.pending_frames += 1;
            g.buf.extend_from_slice(&frame);
            // 段满：立即触发 flush（软阈值）
            full = g.buf.len() as u64 >= self.cfg.segment_bytes;
        }
        // 事件驱动组提交（P2-6b）：需要 durable 的帧入队即刻唤醒刷盘线程——
        // PUT 在途时到达的帧自然并入下一组（慢存储自动成批摊薄 RTT，
        // 快存储零定时器等待）。此前仅段满才 notify，常规帧要等
        // flush_interval 定时器：内存存储的单提交延迟被人为抬到 50ms 级。
        if notify || full {
            self.cv.notify_all();
        }
        Ok(())
    }

    /// 两段式提交的 durable 等待（P2-6）：按持久性等级等待 seq 落盘。
    /// Always 立即触发一次 flush（单飞）；Group 依赖 flush_loop 节拍或
    /// 等待者超时自触发——并发等待者的帧天然并入同一组。
    pub(crate) fn wait_durable(
        &self,
        seq: u64,
        durability: crate::engine::Durability,
    ) -> Result<()> {
        match durability {
            crate::engine::Durability::NoWait => Ok(()),
            crate::engine::Durability::Always => {
                self.flush_now()?;
                self.await_durable(seq)
            }
            crate::engine::Durability::Group => self.await_durable(seq),
        }
    }

    fn await_durable(&self, seq: u64) -> Result<()> {
        let mut g = self.shared.lock();
        while g.durable_seq < seq {
            if *self.stop.lock() {
                return Err(SqlError::io("wal writer stopped"));
            }
            if g.poisoned {
                return Err(SqlError::new("40003",
                    "wal writer poisoned by an earlier upload failure; reopen the branch to recover (transaction outcome may be unknown)"));
            }
            let t = self.cv.wait_for(&mut g, self.cfg.flush_interval);
            let durabled = g.durable_seq >= seq;
            if !durabled && t.timed_out() {
                drop(g);
                self.flush_now()?;
                g = self.shared.lock();
            }
        }
        Ok(())
    }

    /// 立即把当前缓冲上传为段对象。
    /// **单飞互斥（P0-A 修复）**：同一时刻只允许一个上传在途。此前的并发窗口：
    /// flush_loop 取走段 N 缓冲正在慢速 PUT，期间 Always 提交以同段号 N 再次
    /// PUT——两个不同内容写同一路径，最后写者胜，先落盘的已 ack 帧被静默覆盖
    /// （第三轮评审探针实证）。`flush_mu` 串行化后，段号推进（成功后 +1）在
    /// 下一个 flush 开始前完成，同段号并发从机制上不可能。
    /// PUT 失败时数据放回缓冲头部，等下次重试（不丢帧）；段号仅在成功后推进
    /// （失败重试复用同段号，不留空洞——空洞会使 probe_tail 丢失其后所有段）。
    /// Uncertain 语义：同路径重试 PUT 为超集覆盖，最后写者胜。
    /// 计数器在锁内、缓冲被取走时清零；失败路径把本段计数归还。
    pub fn flush_now(&self) -> Result<u64> {
        let t0 = std::time::Instant::now();
        let _single = self.flush_mu.lock();
        // ── 追加模式（P2-6e，supports_append 存储）──
        // 帧字节就地续写当前打开段（无 trailer），按 segment_bytes 封段
        // （追加 32B trailer + 推进 cur_seg）。段数从"每 flush 一个"降到
        // "每 segment_bytes 一个"；durable 提交从"建写整段+fsync+rename"
        // 降为"尾部追加 fsync"。
        // 撕尾容忍：追加非原子，崩溃留部分帧——FrameIter 对 epoch 最后
        // 一段的帧错误按 torn tail 处理（recovery 端，SPEC 01 §4）。
        if self.obj.supports_append() {
            // 锁内取批 + 封段判定（**含 cur_seg 推进**）；I/O 必须在锁外——
            // append 持锁做 fsync 会阻塞全部提交者入队（且 advance_durable
            // 再取同锁自死锁，首版实证）。
            // will_close 时同步推进 cur_seg：失败路径已毒化（不再写、无段洞
            // ——P0 不变量"失败不产生段号空洞"由毒化保证），成功路径段已封口；
            // 窗口期新帧入队自然落到下一段，不会写进已封 trailer 的段。
            let (seg, payload, batch_max, _will_close) = {
                let mut g = self.shared.lock();
                if g.poisoned || g.buf.is_empty() {
                    return Ok(g.flushed_seg);
                }
                let seg = g.cur_seg;
                let batch_frames = g.frames;
                let batch_min = g.min_seq;
                let batch_max = g.max_seq;
                let min_open = if g.seg_min_seq_open == 0 {
                    batch_min
                } else {
                    g.seg_min_seq_open
                };
                let mut payload = std::mem::take(&mut g.buf);
                g.frames = 0;
                g.pending_frames = 0;
                g.min_seq = 0;
                g.max_seq = 0;
                let will_close = g.seg_appended + payload.len() as u64 + TRAILER_LEN as u64
                    >= self.cfg.segment_bytes;
                if will_close {
                    let seg_frames_total = g.seg_frames_total + batch_frames;
                    payload.extend_from_slice(&encode_trailer(
                        seg_frames_total,
                        min_open,
                        batch_max,
                    ));
                    g.seg_appended = 0;
                    g.seg_frames_total = 0;
                    g.seg_min_seq_open = 0;
                    g.cur_seg = g.cur_seg.max(seg + 1);
                } else {
                    g.seg_appended += payload.len() as u64;
                    g.seg_frames_total += batch_frames;
                    g.seg_min_seq_open = min_open;
                }
                (seg, payload, batch_max, will_close)
            };
            let path = Self::seg_path(&self.branch, self.epoch, seg);
            if let Err(e) = self.obj.append(&path, &payload) {
                // 失败：毒化且**不回滚缓冲**——append 可能已写入部分字节，
                // 回滚会造成 reopen 后重放重复帧。已落盘前缀由帧 CRC 守护，
                // 撕尾由恢复端容忍；本批事务按 Uncertain 对账（SPEC 02 §3.5）。
                self.shared.lock().poisoned = true;
                return Err(SqlError::new(
                    "40003",
                    format!("wal append failed, writer poisoned; reopen required: {e}"),
                ));
            }
            self.advance_durable(seg, batch_max);
            let us = t0.elapsed().as_micros() as u64;
            FLUSH_LATENCY_US.fetch_add(us, Ordering::Relaxed);
            FLUSH_LATENCY_CNT.fetch_add(1, Ordering::Relaxed);
            return Ok(seg);
        }
        // ── 整段模式（对象存储 / 不支持追加的包装层）──
        let (seg, bytes, max_seq, seg_frames, seg_min) = {
            let mut g = self.shared.lock();
            // 毒化后不再上传（P0-D）：确定失败的帧绝不持久化；
            // 调用方经 await_durable 的毒化检查得到 40003
            if g.poisoned {
                return Ok(g.flushed_seg);
            }
            if g.buf.is_empty() {
                return Ok(g.flushed_seg);
            }
            let seg = g.cur_seg;
            let max_seq = g.max_seq;
            let seg_frames = g.frames;
            let seg_min = g.min_seq;
            let mut data = std::mem::take(&mut g.buf);
            data.extend_from_slice(&encode_trailer(g.frames, g.min_seq, g.max_seq));
            g.frames = 0;
            g.min_seq = 0;
            g.max_seq = 0;
            g.pending_frames = 0;
            (seg, Bytes::from(data), max_seq, seg_frames, seg_min)
        };
        let path = Self::seg_path(&self.branch, self.epoch, seg);
        if let Err(e) = self.obj.put(&path, bytes.clone()) {
            // PUT 失败：**毒化写者**（P0-D 定案）。帧留缓冲但不再上传——
            // 确定性失败 ⇒ 这些事务未提交，恢复回放永不可见，错误如实。
            // （Uncertain 场景：对象可能已落盘，reopen 后回放裁决真实状态，
            // 客户端须对账——见 WalShared.poisoned 注释与 SPEC 02 §3.5。）
            // cur_seg 不动；reopen 前不再有任何上传（flush_loop 停）。
            let body_len = bytes.len() - TRAILER_LEN;
            let mut g = self.shared.lock();
            g.poisoned = true;
            let mut restored = bytes[..body_len].to_vec();
            restored.extend_from_slice(&g.buf);
            g.buf = restored;
            g.frames += seg_frames;
            g.pending_frames += seg_frames;
            g.min_seq = if g.min_seq == 0 {
                seg_min
            } else {
                seg_min.min(g.min_seq)
            };
            g.max_seq = g.max_seq.max(max_seq);
            return Err(SqlError::new(
                "40003",
                format!("wal put failed, writer poisoned; reopen required: {e}"),
            ));
        }
        self.advance_durable(seg, max_seq);
        let mut g = self.shared.lock();
        g.cur_seg = g.cur_seg.max(seg + 1);
        let us = t0.elapsed().as_micros() as u64;
        drop(g);
        // M-5：flush PUT 延迟（含上传）— 优雅关闭/毒化时不上报
        FLUSH_LATENCY_US.fetch_add(us, Ordering::Relaxed);
        FLUSH_LATENCY_CNT.fetch_add(1, Ordering::Relaxed);
        drop(_single);
        Ok(seg)
    }

    /// flush 线程收到上传完成后的 durable 推进（带段内最大 seq）
    pub fn advance_durable(&self, seg: u64, max_seq: u64) {
        {
            let mut g = self.shared.lock();
            if seg > g.flushed_seg {
                g.flushed_seg = seg;
            }
            g.durable_seq = g.durable_seq.max(max_seq);
        }
        // 段-最大 seq 记账（成功上传后；段退休安全界的依据）
        if max_seq > 0 {
            self.seg_max_seq.lock().insert(seg, max_seq);
        }
        self.cv.notify_all();
    }

    /// 段退休安全界（P2-6）：**最高**的"段内最大帧 ts ≤ covered_ts"段号。
    /// 返回 0 = 无可退休段（含 in-flight 帧的段 ts 超 covered，一律不退）。
    /// 段号与 max_seq 单调对应 ⇒ 满足条件的段构成前缀。
    pub fn retire_bound(&self, covered_ts: u64) -> u64 {
        let g = self.seg_max_seq.lock();
        let mut best = 0u64;
        for (&seg, &max_seq) in g.iter() {
            if crate::recovery::composite_ts(self.epoch, max_seq) <= covered_ts && seg > best {
                best = seg;
            }
        }
        best
    }

    /// 单次 tick（保活/上传）；返回 true = 应退出（stop 或外部 Arc 全释放）。
    /// 由持 Weak 的后台线程周期调用（Weak 打破自环：线程若持 Arc，Drop 永不
    /// 触发，遗弃分支（drop/reopen 后）的线程 + 租约保活永生，会在 GC 删除后
    /// "复活"已删分支的 fence 对象——第六轮发现）。
    fn tick_once(&self) -> bool {
        if *self.stop.lock() {
            return true;
        }
        let (pending, poisoned) = {
            let g = self.shared.lock();
            (g.pending_frames, g.poisoned)
        };
        if poisoned {
            // 毒化后停止一切上传（确定失败的帧绝不持久化，错误 = 未提交）；
            // **保活也停止**：毒化写者已不可用，继续续租会占住租约——
            // 让它自然过期，接管者 reopen 恢复（第五轮 P1）
            return false;
        }
        // 租约保活（必须在毒化检查**之后**）：空闲分支靠它免于 TTL 失约。
        // ⚠ 曾被误删（第六轮 P0 回归：空闲 30s 即永久 40001）——
        // tests/multi_node.rs::idle_writer_stays_writable 是其回归防线。
        if let Some(ka) = &self.cfg.keepalive {
            ka(); // 自限频：内部比较 next_renew_ms，未到期即返回
        }
        if pending > 0 {
            if let Err(e) = self.flush_now() {
                tracing::error!("wal flush: {e}");
            }
        }
        false
    }

    pub fn durable_watermark(&self) -> u64 {
        self.shared.lock().durable_seq
    }

    /// 写者是否已毒化（P0-D；engine::reopen_branch 与监控用）
    pub fn poisoned(&self) -> bool {
        self.shared.lock().poisoned
    }

    pub fn current_seg(&self) -> u64 {
        self.shared.lock().cur_seg
    }

    /// GC 起始段号（之前的段已被墓碑覆盖；恢复从这段开始探测）
    pub fn first_seg(&self) -> u64 {
        self.first_seg.load(Ordering::Acquire)
    }

    /// GC 推进起始段号（checkpoint 发布后调用）
    pub fn set_first_seg(&self, seg: u64) {
        self.first_seg.fetch_max(seg, Ordering::AcqRel);
    }

    pub fn close(&self) {
        *self.stop.lock() = true;
        self.cv.notify_all();
        if let Some(h) = self.handle.lock().take() {
            let _ = h.join();
        }
    }

    /// **优雅关闭**（Q-12）：先上传剩余缓冲（毒化时跳过——错误语义保留：
    /// 毒化帧永不上传），再停线程。Group/Always 已 ack 的数据不受影响；
    /// NoWait 缓冲尾因此得以持久（比进程死亡多保住一段）。
    pub fn close_graceful(&self) {
        if !self.shared.lock().poisoned {
            let _ = self.flush_now();
            // 追加模式：把打开段封口（追加 trailer + 推进 cur_seg）——
            // 优雅关闭的段不依赖撕尾容忍 reopen
            if self.obj.supports_append() {
                let g = self.shared.lock();
                if g.seg_appended > 0 && !g.poisoned {
                    drop(g);
                    let _ = self.close_open_segment();
                }
            }
        }
        self.close();
    }

    /// 封口当前打开段（追加模式）：trailer 落盘 + 推进段号。缓冲应已空
    /// （close_graceful 先行 flush）；失败仅告警——reopen 撕尾容忍兜底。
    fn close_open_segment(&self) -> Result<()> {
        let (path, trailer, seg) = {
            let mut g = self.shared.lock();
            if g.seg_appended == 0 {
                return Ok(());
            }
            let trailer = encode_trailer(
                g.seg_frames_total,
                g.seg_min_seq_open,
                g.durable_seq & 0xFFFF_FFFF,
            );
            g.seg_appended = 0;
            g.seg_frames_total = 0;
            g.seg_min_seq_open = 0;
            let seg = g.cur_seg;
            g.cur_seg += 1;
            (Self::seg_path(&self.branch, self.epoch, seg), trailer, seg)
        };
        self.obj.append(&path, &trailer)?;
        self.advance_durable(seg, 0);
        Ok(())
    }
}

/// 外部最后一个 Arc 释放 ⇒ 后台线程不再 upgrade 成功 ⇒ 自行退出；
/// 此处置 stop 双保险（若线程尚在 upgrade 窗口内）
impl Drop for WalWriter {
    fn drop(&mut self) {
        *self.stop.lock() = true;
    }
}

// ---------------------------------------------------------------------------
// 恢复：HEAD 探测尾部（SPEC 01 §4）
// ---------------------------------------------------------------------------

/// 找到分支 WAL 的最高已存在段号（指数探测+二分）
pub fn probe_tail(obj: &Arc<dyn ObjStore>, branch: &str, epoch: u64, lo_seg: u64) -> u64 {
    let exists = |seg: u64| -> bool {
        obj.head(&WalWriter::seg_path(branch, epoch, seg))
            .ok()
            .flatten()
            .is_some()
    };
    if !exists(lo_seg) {
        return lo_seg.saturating_sub(1);
    }
    let mut lo = lo_seg;
    let mut hi = lo + 1;
    while exists(hi) {
        lo = hi;
        hi *= 2;
        if hi > 1 << 40 {
            break;
        }
    }
    // (lo, hi] 二分
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if exists(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// 读取并解码一个段
/// 段尾是否为有效 trailer（封段完成标记）。区分两类合同：
/// 已封段（优雅关闭/按阈值封口）帧损坏 = 真实腐坏，恢复严格报错；
/// 未封段（追加中崩溃）帧错误 = 撕尾容忍，回放已 durable 前缀。
pub fn segment_closed(data: &[u8]) -> bool {
    if data.len() < TRAILER_LEN {
        return false;
    }
    let t = &data[data.len() - TRAILER_LEN..];
    let magic = u32::from_le_bytes(t[..4].try_into().unwrap());
    let crc = u32::from_le_bytes(t[28..32].try_into().unwrap());
    magic == SEGMENT_MAGIC && crc32c::crc32c(&t[..28]) == crc
}

pub fn read_segment(
    obj: &Arc<dyn ObjStore>,
    branch: &str,
    epoch: u64,
    seg: u64,
) -> Result<Vec<u8>> {
    let path = WalWriter::seg_path(branch, epoch, seg);
    let data = obj
        .get(&path)
        .map_err(|e| SqlError::io(format!("wal read: {e}")))?;
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Durability;
    use crate::objstore::memory::MemoryObjStore;

    fn cfg() -> WalConfig {
        WalConfig {
            flush_interval: Duration::from_millis(20),
            segment_bytes: 1 << 20,
            durability: Durability::Group,
            keepalive: None,
        }
    }

    #[test]
    fn frame_roundtrip() {
        let rec = TxnRecord {
            table_id: 7,
            ops: vec![
                (b"k1".to_vec(), Some(b"v1".to_vec())),
                (b"k2".to_vec(), None),
            ],
        };
        let payload = encode_txn(std::slice::from_ref(&rec));
        let frame = encode_frame(FrameType::Txn, 42, &payload);
        let mut it = FrameIter::new(&frame);
        let (ty, seq, p) = it.next_frame().unwrap().unwrap();
        assert_eq!(ty, FrameType::Txn);
        assert_eq!(seq, 42);
        assert_eq!(decode_txn(p).unwrap(), vec![rec]);
        assert!(it.next_frame().is_none());
    }

    #[test]
    fn group_commit_durability() {
        let mem: Arc<dyn ObjStore> = Arc::new(MemoryObjStore::new());
        let w = WalWriter::open(mem.clone(), "main", 1, 1, cfg());
        for i in 0..10u64 {
            let rec = TxnRecord {
                table_id: 1,
                ops: vec![(format!("k{i}").into_bytes(), Some(b"v".to_vec()))],
            };
            w.append(
                FrameType::Txn,
                i + 1,
                &encode_txn(&[rec]),
                Durability::Group,
            )
            .unwrap();
        }
        // 等 flush 周期
        std::thread::sleep(Duration::from_millis(80));
        assert!(w.durable_watermark() >= 10);
        let segs = mem.list_prefix("wal/main/").unwrap();
        assert!(!segs.is_empty());
        // 回放校验（帧可能按段拆分：遍历全部段）
        let segs = mem.list_prefix("wal/main/").unwrap();
        let mut seen = 0;
        for sp in &segs {
            let data = mem.get(sp).unwrap();
            let mut it = FrameIter::new(&data);
            while let Some(f) = it.next_frame() {
                let (ty, _seq, p) = f.unwrap();
                assert_eq!(ty, FrameType::Txn);
                seen += decode_txn(p).unwrap().len();
            }
        }
        assert_eq!(seen, 10, "segments: {segs:?}");
        w.close();
    }

    #[test]
    fn probe_tail_finds_segments() {
        let mem: Arc<dyn ObjStore> = Arc::new(MemoryObjStore::new());
        for seg in [1u64, 2, 3] {
            mem.put(
                &WalWriter::seg_path("main", 1, seg),
                Bytes::from_static(b"x"),
            )
            .unwrap();
        }
        assert_eq!(probe_tail(&mem, "main", 1, 1), 3);
        assert_eq!(probe_tail(&mem, "main", 1, 4), 3); // lo 不存在
        assert_eq!(probe_tail(&mem, "nope", 1, 1), 0);
    }
}
