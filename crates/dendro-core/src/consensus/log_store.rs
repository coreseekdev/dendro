//! 分层日志存储：内存热层（M1）+ OSS 冷层接口（M2 接入）。
//!
//! M1 用 BTreeMap（内存有序）；M2 换 NVMe 段文件 + OSS 上传。
//! 接口不变（`ConsensusLogStore` trait），实现可替换。

use super::{ConsensusEntry, ConsensusLogStore};
use crate::error::Result;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub struct TieredLogStore {
    entries: Mutex<BTreeMap<u64, ConsensusEntry>>,
    first: AtomicU64,
    last: AtomicU64,
}

impl TieredLogStore {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            first: AtomicU64::new(0),
            last: AtomicU64::new(0),
        }
    }
}

impl Default for TieredLogStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsensusLogStore for TieredLogStore {
    fn append(&self, entries: &[ConsensusEntry]) -> Result<()> {
        let mut m = self.entries.lock().unwrap();
        for e in entries {
            if self.first.load(Ordering::Acquire) == 0 {
                self.first.store(e.index, Ordering::Release);
            }
            m.insert(e.index, e.clone());
            self.last.store(e.index, Ordering::Release);
        }
        Ok(())
    }

    fn read(&self, low: u64, high: u64) -> Result<Vec<ConsensusEntry>> {
        let m = self.entries.lock().unwrap();
        Ok(m.range(low..high).map(|(_, v)| v.clone()).collect())
    }

    fn truncate_from(&self, idx: u64) -> Result<()> {
        let mut m = self.entries.lock().unwrap();
        let keys: Vec<u64> = m.range(idx..).map(|(k, _)| *k).collect();
        for k in keys {
            m.remove(&k);
        }
        if let Some(&last) = m.keys().next_back() {
            self.last.store(last, Ordering::Release);
        } else {
            self.last.store(0, Ordering::Release);
        }
        Ok(())
    }

    fn compact_to(&self, idx: u64) -> Result<()> {
        let mut m = self.entries.lock().unwrap();
        let keys: Vec<u64> = m.range(..=idx).map(|(k, _)| *k).collect();
        for k in keys {
            m.remove(&k);
        }
        if let Some(&f) = m.keys().next() {
            self.first.store(f, Ordering::Release);
        }
        Ok(())
    }

    fn first_index(&self) -> Result<u64> {
        Ok(self.first.load(Ordering::Acquire))
    }

    fn last_index(&self) -> Result<u64> {
        Ok(self.last.load(Ordering::Acquire))
    }

    fn tier_to_oss(&self, _idx: u64) -> Result<()> {
        Ok(()) // v2: OSS 冷层上传
    }

    fn restore_from_oss(&self) -> Result<()> {
        Ok(()) // v2: OSS 冷层恢复
    }
}
