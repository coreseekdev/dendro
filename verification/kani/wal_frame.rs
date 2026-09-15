//! Kani L1 harness：WAL FrameIter 不可信输入边界（账本 I-C6）。
//! 同源副本声明：FrameIter/encode_frame 与 crates/dendro-core/src/wal.rs
//! 逐行同源——真实现变更必须同步本文件。
//!
//! 运行：kani verification/kani/wal_frame.rs

use std::convert::TryInto;

pub const HEADER_LEN: usize = 24;
pub const TRAILER_LEN: usize = 32;
/// 与真实现同步（账本 #19 附带发现：本副本 FRAME_MAGIC 曾漂移为
/// 0x4C41_5345 而真实现是 0x4F524E44——"逐行同源"声明无机制保障，
/// 已加 constants-sync 测试防再犯）
pub const FRAME_MAGIC: u32 = 0x4F524E44; // "DRNO" LE
pub const SEGMENT_MAGIC: u32 = 0x4C415345; // "ESAL"
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

fn crc32c(data: &[u8]) -> u32 {
    let poly: u32 = 0x82F6_3B78;
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (if crc & 1 != 0 { poly } else { 0 });
        }
    }
    !crc
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
    /// 界 40 = 帧头 24 + 16B 载荷：旧界 24 使迭代器永远停在段尾 guard，
    /// 解码体从未被探索（账本 #19 连带发现）。
    /// 编码注意（实证调参）：整块符号化（kani::any 数组，无填充循环）
    /// + 固定 3 次调用（解析/推进/再解析）——原 while-let 无界迭代 +
    /// 符号长度 Vec 填充使展开空间乘法爆炸（40B/ unwind 80 实测
    /// >9min 不收敛）。unwind 17 覆盖 crc32c（内循环恰 8、外循环
    /// ≤16B 载荷）。
    #[kani::proof]
    #[kani::unwind(17)]
    fn frame_iter_arbitrary_input_never_panics() {
        let buf: [u8; 40] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= 40);
        let mut it = FrameIter::new(&buf[..len]);
        let _ = it.next_frame();
        let _ = it.next_frame();
        let _ = it.next_frame();
    }

    /// H4（账本 #19 回归）：总长 ≤ TRAILER_LEN 的小帧在段尾必须被解码
    /// （旧 guard `remaining <= TRAILER_LEN 即停` 会把它当 trailer 丢弃
    /// ——封段回放静默丢已确认提交）。
    #[kani::proof]
    #[kani::unwind(9)]
    fn tiny_frame_at_segment_end_decodes() {
        let seq: u64 = kani::any();
        let payload: [u8; 2] = [kani::any(), kani::any()]; // 26B 帧 < 32B trailer
        let frame = encode_frame(FrameType::Txn, seq, &payload);
        assert!(frame.len() <= TRAILER_LEN, "前置：确为小帧");
        let mut it = FrameIter::new(&frame);
        match it.next_frame() {
            Some(Ok((ty, s, p))) => {
                assert_eq!(ty, FrameType::Txn);
                assert_eq!(s, seq);
                assert_eq!(p, payload.as_slice());
            }
            _ => panic!("小帧必须解码，不得当 trailer 丢弃"),
        }
    }

    /// H2：合法编码帧必被无错解码（编码/解码对偶）
    /// 注：payload 用栈上数组而非 vec![kani::any(),..]——符号字节进
    /// 堆分配（box [..] → into_raw_with_allocator）会引入分配器建模
    /// 噪声，曾在本工具链上产生虚假反例（0.67 + nightly-2025-11-21）。
    /// unwind 9：crc32c 内循环恰 8 次 + 外循环 = 载荷长（此处 2）。
    /// 无显式界时 Kani 对符号长度循环逐轮自动加界、每轮完整求解，
    /// 实测 >150s 不收敛（CaDiCaL/kissat 同）。
    #[kani::proof]
    #[kani::unwind(9)]
    fn frame_roundtrip_exact() {
        let seq: u64 = kani::any();
        let payload: [u8; 2] = [kani::any(), kani::any()];
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
