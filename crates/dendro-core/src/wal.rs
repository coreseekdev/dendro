//! WAL：只写日志直写对象存储（SPEC 02）。
//! 段 = 一个不可变对象；组提交摊薄 RTT；durable 回调推进水位。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::objstore::ObjStore;
use bytes::Bytes;
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
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
    Ok(CheckpointRecord { catalog_root, commit_addr, seq_covered })
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
}

struct WalShared {
    buf: Vec<u8>,            // 当前段缓冲（帧序列）
    frames: u64,
    min_seq: u64,
    max_seq: u64,
    cur_seg: u64,            // 正在写的段号
    flushed_seg: u64,        // 已成功上传的最高段
    durable_seq: u64,        // 已 durable 的最高 seq
    pending_frames: u64,     // 当前缓冲帧数（唤醒用）
}

pub struct WalWriter {
    obj: Arc<dyn ObjStore>,
    branch: String,
    epoch: u64,
    cfg: WalConfig,
    shared: Mutex<WalShared>,
    cv: Condvar,
    /// 首个未上传段号（恢复起点提示；进程内缓存）
    stop: Mutex<bool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl WalWriter {
    pub fn open(obj: Arc<dyn ObjStore>, branch: &str, epoch: u64, start_seg: u64, cfg: WalConfig) -> Arc<WalWriter> {
        let w = Arc::new(Self {
            obj,
            branch: branch.to_string(),
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
            }),
            cv: Condvar::new(),
            stop: Mutex::new(false),
            handle: Mutex::new(None),
        });
        // 后台 flush 线程
        let w2 = w.clone();
        let h = std::thread::Builder::new()
            .name(format!("wal-{branch}-e{epoch}"))
            .spawn(move || w2.flush_loop())
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
            }),
            cv: Condvar::new(),
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
    pub fn append(&self, ty: FrameType, seq: u64, payload: &[u8], durability: crate::engine::Durability) -> Result<()> {
        let frame = encode_frame(ty, seq, payload);
        let _size = frame.len();
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
            if g.buf.len() as u64 >= self.cfg.segment_bytes {
                self.cv.notify_all();
            }
        }
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
    /// PUT 失败时数据放回缓冲头部，等下次重试（P0-1 修复：不丢帧）。
    /// 段号仅在 PUT **成功后**推进：失败重试复用同一段号。若失败也推进，
    /// 会留下段号空洞，probe_tail 的连续性假设会把空洞后的段全部判为
    /// 不存在 → 恢复丢数据（回归测试 wal_corruption 捕获）。
    /// Uncertain 语义：同路径重试 PUT 覆盖写，内容为超集，最后写者胜。
    /// 计数器在锁内、缓冲被取走时清零——此刻计数只含本段，之后的 append 属于下一段；
    /// 失败路径把本段计数归还（恢复的帧仍占缓冲），上传期间的新 append 不受影响。
    /// 段体以 Bytes 持有：失败回拷从 Bytes 切片拷回，成功路径零额外 memcpy。
    pub fn flush_now(&self) -> Result<u64> {
        let (seg, bytes, max_seq, seg_frames, seg_min) = {
            let mut g = self.shared.lock();
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
            // PUT 失败：段体放回缓冲头部（旧帧在前，上传期间的新 append 在后）；
            // cur_seg 不动，重试仍写同一段号（不留空洞）
            let body_len = bytes.len() - TRAILER_LEN;
            let mut g = self.shared.lock();
            let mut restored = bytes[..body_len].to_vec();
            restored.extend_from_slice(&g.buf);
            g.buf = restored;
            g.frames += seg_frames;
            g.pending_frames += seg_frames;
            g.min_seq = if g.min_seq == 0 { seg_min } else { seg_min.min(g.min_seq) };
            g.max_seq = g.max_seq.max(max_seq);
            return Err(SqlError::io(format!("wal put: {e}")));
        }
        self.advance_durable(seg, max_seq);
        let mut g = self.shared.lock();
        g.cur_seg = g.cur_seg.max(seg + 1);
        drop(g);
        Ok(seg)
    }

    /// flush 线程收到上传完成后的 durable 推进（带段内最大 seq）
    pub fn advance_durable(&self, seg: u64, max_seq: u64) {
        let mut g = self.shared.lock();
        if seg > g.flushed_seg {
            g.flushed_seg = seg;
        }
        g.durable_seq = g.durable_seq.max(max_seq);
        drop(g);
        self.cv.notify_all();
    }

    fn flush_loop(self: Arc<Self>) {
        loop {
            if *self.stop.lock() {
                return;
            }
            std::thread::sleep(self.cfg.flush_interval);
            let pending = { let g = self.shared.lock(); g.pending_frames };
            if pending > 0 {
                if let Err(e) = self.flush_now() {
                    tracing::error!("wal flush: {e}");
                }
            }
        }
    }

    pub fn durable_watermark(&self) -> u64 {
        self.shared.lock().durable_seq
    }

    pub fn current_seg(&self) -> u64 {
        self.shared.lock().cur_seg
    }

    pub fn close(&self) {
        *self.stop.lock() = true;
        self.cv.notify_all();
        if let Some(h) = self.handle.lock().take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// 恢复：HEAD 探测尾部（SPEC 01 §4）
// ---------------------------------------------------------------------------

/// 找到分支 WAL 的最高已存在段号（指数探测+二分）
pub fn probe_tail(obj: &Arc<dyn ObjStore>, branch: &str, epoch: u64, lo_seg: u64) -> u64 {
    let exists = |seg: u64| -> bool {
        obj.head(&WalWriter::seg_path(branch, epoch, seg)).ok().flatten().is_some()
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
pub fn read_segment(obj: &Arc<dyn ObjStore>, branch: &str, epoch: u64, seg: u64) -> Result<Vec<u8>> {
    let path = WalWriter::seg_path(branch, epoch, seg);
    let data = obj.get(&path).map_err(|e| SqlError::io(format!("wal read: {e}")))?;
    Ok(data.to_vec())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::objstore::memory::MemoryObjStore;
    use crate::engine::Durability;

    fn cfg() -> WalConfig {
        WalConfig {
            flush_interval: Duration::from_millis(20),
            segment_bytes: 1 << 20,
            durability: Durability::Group,
        }
    }

    #[test]
    fn frame_roundtrip() {
        let rec = TxnRecord {
            table_id: 7,
            ops: vec![(b"k1".to_vec(), Some(b"v1".to_vec())), (b"k2".to_vec(), None)],
        };
        let payload = encode_txn(&[rec.clone()]);
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
            let rec = TxnRecord { table_id: 1, ops: vec![(format!("k{i}").into_bytes(), Some(b"v".to_vec()))] };
            w.append(FrameType::Txn, i + 1, &encode_txn(&[rec]), Durability::Group).unwrap();
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
            mem.put(&WalWriter::seg_path("main", 1, seg), Bytes::from_static(b"x")).unwrap();
        }
        assert_eq!(probe_tail(&mem, "main", 1, 1), 3);
        assert_eq!(probe_tail(&mem, "main", 1, 4), 3); // lo 不存在
        assert_eq!(probe_tail(&mem, "nope", 1, 1), 0);
    }
}
