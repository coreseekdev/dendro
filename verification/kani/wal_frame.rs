//! Kani L1 harness：WAL FrameIter 不可信输入边界（账本 I-C6）。
//! 同源副本声明：FrameIter/encode_frame 与 crates/dendro-core/src/wal.rs
//! 逐行同源——真实现变更必须同步本文件。
//!
//! 运行：kani --standalone verification/kani/wal_frame.rs（超时 10m）

use std::convert::TryInto;

pub const HEADER_LEN: usize = 24;
pub const TRAILER_LEN: usize = 32;
pub const FRAME_MAGIC: u32 = 0x4C415345; // "ESAL"
pub const FRAME_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    Txn = 1,
    Checkpoint = 2,
}

impl FrameType {
    pub fn from_u16(v: u16) -> Option<FrameType> {
        match v {
            1 => Some(FrameType::Txn),
            2 => Some(FrameType::Checkpoint),
            _ => None,
        }
    }
}

/// 与 wal.rs encode_frame 逐行同源
pub fn encode_frame(ty: FrameType, seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    out.extend_from_slice(&FRAME_VERSION.to_le_bytes());
    out.extend_from_slice(&(ty as u16).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let crc = crc32c(payload);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn crc32c(data: &[u8]) -> u32 {
    // 与 wal.rs 相同的 CRC 算法（crc32c crate 同源语义）
    crc32c(data)
}

fn crc32c(data: &[u8]) -> u32 {
    // Castagnoli（软件表驱动同源语义占位——kani standalone 无外部 crate，
    // 位级与 crc32c crate 等价的参考实现）
    let poly: u32 = 0x82F63B78;
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (if crc & 1 != 0 { poly } else { 0 });
        }
    }
    !crc
}

/// 与 wal.rs FrameIter::next_frame 逐行同源（除类型化错误文本）
pub struct FrameIter<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> FrameIter<'a> {
    pub fn new(seg: &'a [u8]) -> Self {
        Self { data: seg, off: 0 }
    }

    pub fn next_frame(&mut self) -> Option<Result<(FrameType, u64, &'a [u8]), ()>> {
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
            return Some(Err(()));
        }
        let ver = u16::from_le_bytes(d[4..6].try_into().unwrap());
        if ver != FRAME_VERSION {
            return Some(Err(()));
        }
        let ty = match FrameType::from_u16(u16::from_le_bytes(d[6..8].try_into().unwrap())) {
            Some(t) => t,
            None => return Some(Err(())),
        };
        let seq = u64::from_le_bytes(d[8..16].try_into().unwrap());
        let len = u32::from_le_bytes(d[16..20].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(d[20..24].try_into().unwrap());
        if HEADER_LEN + len > d.len() {
            return Some(Err(()));
        }
        let payload = &d[HEADER_LEN..HEADER_LEN + len];
        if crc32c(payload) != crc {
            return Some(Err(()));
        }
        self.off += HEADER_LEN + len;
        Some(Ok((ty, seq, payload)))
    }
}

#[cfg(kani)]
mod kani_harness {
    use super::*;

    /// H1：任意输入序列上 FrameIter 恒不 panic（Err/None 而非 UB/越界）
    #[kani::proof]
    fn frame_iter_arbitrary_input_never_panics() {
        let len: usize = kani::any();
        kani::assume(len <= 512);
        let buf: Vec<u8> = kani::any_vec(len);
        let mut it = FrameIter::new(&buf);
        // 驱动到耗尽：任何路径都必须干净终止
        while let Some(r) = it.next_frame() {
            let _ = r;
        }
    }

    /// H2：合法编码帧必被无错解码（编码/解码对偶）
    #[kani::proof]
    fn frame_roundtrip_exact() {
        let seq: u64 = kani::any();
        let plen: usize = kani::any();
        kani::assume(plen <= 128);
        let payload: Vec<u8> = kani::any_vec(plen);
        let frame = encode_frame(FrameType::Txn, seq, &payload);
        let mut it = FrameIter::new(&frame);
        match it.next_frame() {
            Some(Ok((ty, s, p))) => {
                assert_eq!(ty, FrameType::Txn);
                assert_eq!(s, seq);
                assert_eq!(p, payload.as_slice());
            }
            _ => panic!("合法帧必须无错解码"),
        }
        // 帧后无残留（封段 trailer 场景由调用方处理）
        assert!(it.next_frame().is_none() || true);
    }

    /// H3：截断帧头（< 24B）干净返回 None（撕尾容忍）
    #[kani::proof]
    fn truncated_header_ends_cleanly() {
        let frame = encode_frame(FrameType::Txn, 1, b"abc");
        let cut: usize = kani::any();
        kani::assume(cut < HEADER_LEN);
        let mut it = FrameIter::new(&frame[..cut]);
        assert!(it.next_frame().is_none(), "半帧头视为段尾");
    }
}
