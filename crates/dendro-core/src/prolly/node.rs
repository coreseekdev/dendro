//! 节点编码（SPEC 03 §2.2）：keys/values 平行数组 + 地址槽表（GC 用）。
//!
//! 布局（LE）：
//! ```text
//! level u8 | count u32
//! key_lens u16×N | key_bytes concat | key_offs u32×N
//! val_lens u32×N | val_bytes concat | val_offs u32×N
//! addr_slots u32×S   // val_bytes 内嵌 20B 子节点地址的字节偏移（内部节点）
//! crc32c u32         // 覆盖以上全部
//! ```
//! 解析零拷贝：`Node` 持有 Arc<[u8]>，访问器即时解码。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use std::sync::Arc;

pub const ADDR_LEN: usize = 20;

#[derive(Clone)]
pub struct Node {
    data: Arc<Vec<u8>>,
    /// 完整 chunk 地址 = H([type_tag] ++ data)，构建/加载时算一次
    addr: Hash,
}

/// 叶/内部统一的条目值
#[derive(Debug, Clone, PartialEq)]
pub enum EntryVal {
    /// 叶层：行负载字节
    Item(Vec<u8>),
    /// 内部层：子地址 + 子树条目数
    Child(Hash, u64),
}

impl Node {
    pub fn build(level: u8, entries: &[(Vec<u8>, EntryVal)]) -> Node {
        let n = entries.len();
        let mut key_lens = Vec::with_capacity(n * 2);
        let mut key_bytes = Vec::new();
        let mut val_lens = Vec::with_capacity(n * 4);
        let mut val_bytes = Vec::new();
        let mut addr_slots = Vec::new();
        for (k, v) in entries {
            key_lens.extend_from_slice(&(k.len() as u16).to_le_bytes());
            key_bytes.extend_from_slice(k);
            match v {
                EntryVal::Item(items) => {
                    val_lens.extend_from_slice(&(items.len() as u32).to_le_bytes());
                    val_bytes.extend_from_slice(items);
                }
                EntryVal::Child(addr, cnt) => {
                    val_lens.extend_from_slice(&((ADDR_LEN + 8) as u32).to_le_bytes());
                    let off = val_bytes.len() as u32;
                    addr_slots.push(off);
                    val_bytes.extend_from_slice(addr.as_bytes());
                    val_bytes.extend_from_slice(&cnt.to_le_bytes());
                }
            }
        }
        let n_keys: usize = key_bytes.len();
        let mut out = Vec::with_capacity(
            5 + n * 6 + n_keys + n * 4 + val_bytes.len() + n * 4 + addr_slots.len() * 4 + 4,
        );
        out.push(level);
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend_from_slice(&key_lens);
        out.extend_from_slice(&key_bytes);
        {
            let mut off = 0u32;
            for i in 0..n {
                out.extend_from_slice(&off.to_le_bytes());
                off += u16::from_le_bytes(key_lens[i * 2..i * 2 + 2].try_into().unwrap()) as u32;
            }
        }
        out.extend_from_slice(&val_lens);
        out.extend_from_slice(&val_bytes);
        {
            let mut voff = 0u32;
            for i in 0..n {
                out.extend_from_slice(&voff.to_le_bytes());
                voff += u32::from_le_bytes(val_lens[i * 4..i * 4 + 4].try_into().unwrap());
            }
        }
        out.extend_from_slice(&(addr_slots.len() as u32).to_le_bytes());
        for a in &addr_slots {
            out.extend_from_slice(&a.to_le_bytes());
        }
        let crc = crc32c::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Self::from_arc(Arc::new(out))
    }

    pub fn from_arc(data: Arc<Vec<u8>>) -> Node {
        let mut buf = Vec::with_capacity(data.len() + 1);
        buf.push(super::super::objstore::cas::ChunkType::Node as u8);
        buf.extend_from_slice(&data);
        let addr = Hash::of(&buf);
        Node { data, addr }
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn addr(&self) -> Hash {
        self.addr
    }

    fn rd_u32(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.data[off..off + 4].try_into().unwrap())
    }
    fn rd_u16(&self, off: usize) -> u16 {
        u16::from_le_bytes(self.data[off..off + 2].try_into().unwrap())
    }

    pub fn level(&self) -> u8 {
        self.data[0]
    }
    pub fn count(&self) -> usize {
        self.rd_u32(1) as usize
    }
    fn key_lens_off(&self) -> usize {
        5
    }
    fn key_bytes_off(&self) -> usize {
        self.key_lens_off() + self.count() * 2
    }
    fn key_offs_off(&self) -> usize {
        self.key_bytes_off() + self.keys_total_len()
    }
    fn keys_total_len(&self) -> usize {
        let mut t = 0;
        for i in 0..self.count() {
            t += self.rd_u16(self.key_lens_off() + i * 2) as usize;
        }
        t
    }
    fn val_lens_off(&self) -> usize {
        self.key_offs_off() + self.count() * 4
    }
    fn val_bytes_off(&self) -> usize {
        self.val_lens_off() + self.count() * 4
    }
    fn vals_total_len(&self) -> usize {
        let mut t = 0;
        for i in 0..self.count() {
            t += self.rd_u32(self.val_lens_off() + i * 4) as usize;
        }
        t
    }
    fn val_offs_off(&self) -> usize {
        self.val_bytes_off() + self.vals_total_len()
    }
    fn addr_slots_off(&self) -> usize {
        self.val_offs_off() + self.count() * 4
    }

    pub fn key(&self, i: usize) -> Vec<u8> {
        self.key_slice(i).to_vec()
    }

    /// 第 i 个键的借用切片（P2-6 热路径去分配：lower_bound/lookup 的逐比较
    /// `key()` 曾每次二分步分配一个 Vec——节点数据被 Node 持有，借用零成本）
    pub fn key_slice(&self, i: usize) -> &[u8] {
        let off = self.rd_u32(self.key_offs_off() + i * 4) as usize;
        let len = self.rd_u16(self.key_lens_off() + i * 2) as usize;
        &self.data[self.key_bytes_off() + off..self.key_bytes_off() + off + len]
    }

    /// 条目值（叶=Item 字节拷贝；内部=Child 解码）
    pub fn value(&self, i: usize) -> EntryVal {
        let off = self.rd_u32(self.val_offs_off() + i * 4) as usize;
        let len = self.rd_u32(self.val_lens_off() + i * 4) as usize;
        let start = self.val_bytes_off() + off;
        let slice = &self.data[start..start + len];
        if self.level() == 0 {
            EntryVal::Item(slice.to_vec())
        } else {
            let mut a = [0u8; ADDR_LEN];
            a.copy_from_slice(&slice[..ADDR_LEN]);
            let cnt = u64::from_le_bytes(slice[ADDR_LEN..ADDR_LEN + 8].try_into().unwrap());
            EntryVal::Child(Hash::from_bytes(a), cnt)
        }
    }

    /// 内部节点子树条目数（供序数定位）
    pub fn subtree_count(&self, i: usize) -> u64 {
        debug_assert!(self.level() > 0);
        match self.value(i) {
            EntryVal::Child(_, c) => c,
            EntryVal::Item(_) => 1,
        }
    }

    /// 节点内二分：返回第一个 >= key 的槽位
    pub fn lower_bound(&self, key: &[u8]) -> usize {
        let mut lo = 0usize;
        let mut hi = self.count();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.key_slice(mid) < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// GC 遍历：内嵌子地址（从 addr_slots 解）
    pub fn child_addrs(&self) -> Vec<Hash> {
        if self.level() == 0 {
            return vec![];
        }
        let n_slots = self.rd_u32(self.addr_slots_off()) as usize;
        let mut out = Vec::with_capacity(n_slots);
        let base = self.val_bytes_off();
        for i in 0..n_slots {
            let off = self.rd_u32(self.addr_slots_off() + 4 + i * 4) as usize;
            let mut a = [0u8; ADDR_LEN];
            a.copy_from_slice(&self.data[base + off..base + off + ADDR_LEN]);
            out.push(Hash::from_bytes(a));
        }
        out
    }

    pub fn first_key(&self) -> Vec<u8> {
        self.key(0)
    }
    pub fn last_key(&self) -> Vec<u8> {
        self.key(self.count() - 1)
    }

    pub fn entries(&self) -> Vec<(Vec<u8>, EntryVal)> {
        (0..self.count())
            .map(|i| (self.key(i), self.value(i)))
            .collect()
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Node(level={}, count={}, addr={})",
            self.level(),
            self.count(),
            self.addr()
        )
    }
}

pub fn parse_err(msg: impl Into<String>) -> SqlError {
    SqlError::internal(format!("prolly node: {}", msg.into()))
}

/// 校验 CRC 与结构（读取时防损坏）
pub fn validate(data: &[u8]) -> Result<()> {
    if data.len() < 9 {
        return Err(parse_err("too short"));
    }
    let body = &data[..data.len() - 4];
    let crc = u32::from_le_bytes(data[data.len() - 4..].try_into().unwrap());
    if crc32c::crc32c(body) != crc {
        return Err(parse_err("crc mismatch"));
    }
    Ok(())
}
