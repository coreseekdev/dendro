//! WAL 帧编解码（SPEC 02）：**不可信输入边界**——本模块是 Kani L1 的
//! 验证对象：verification/kani 经 #[path] 直接编译本文件（真源编译，
//! 非镜像副本）。因此依赖面刻意最小：error（仅构造器）/ format::hash
//! （纯数据）/ crc32c；新增依赖会让 Kani 绑定失效，须同 PR 评估。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;

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
    /// pub(crate)：Kani harness（同 crate 兄弟模块）对全定义域符号化
    pub(crate) fn from_u16(v: u16) -> Option<Self> {
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
    ///
    /// 段尾识别（账本 #19）：旧 guard `remaining <= TRAILER_LEN 即停`
    /// 把"最后一个总长 ≤32B 的小帧"（24B 头 + ≤8B 载荷）当 trailer
    /// 丢弃——封段回放静默丢已确认提交（Kani H2 反例）。改为按魔数
    /// 识别：DRNO（帧）≠ ESAL（段尾），天然可分；垃圾尾（非两魔数）
    /// 报 Err，不再被字节数容差静默吞掉。撕尾/腐坏的容忍分级由回放方
    /// 按段位置处理（recovery.rs：末段一律容忍，非末段严格报错）——
    /// 迭代器不做 trailer crc 校验：ESAL 魔数已截断的撕尾/位腐，
    /// 与"已封但计数腐坏"在追加模式下本就不可区分（尾段是否封口
    /// 不可靠），校验只会把符号执行成本变成 28 字节 CRC 链（H2 爆炸）。
    pub fn next_frame(&mut self) -> Option<Result<(FrameType, u64, &'a [u8])>> {
        if self.off >= self.data.len() {
            return None;
        }
        let d = &self.data[self.off..];
        if d.len() < HEADER_LEN {
            return None; // 半帧头：段尾/撕尾
        }
        let magic = u32::from_le_bytes(d[..4].try_into().unwrap());
        if magic == SEGMENT_MAGIC {
            return None; // 段尾 trailer（ESAL）
        }
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
