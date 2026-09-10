//! SimObjStore：故障注入仿真对象存储（故障模型的**单一事实源**）。
//!
//! 与 basalt SimDisk 同方法论：全部依赖 ObjStore 的组件（WAL、manifest、
//! CAS、opfuzz、TLA+ 环境动作）只消费本模块定义的故障语义——
//! 改本文件语义 = 环境模型设计变更，必须同 PR 更新 docs/VERIFICATION.md
//! §3、opfuzz 参数与 TLC 环境动作。
//!
//! 故障面（对齐 basalt SimDisk + OSS 真实语义）：
//! - `put`/`append_at`：字节先入 pending（对客户端不可见——get 只读 committed）
//! - `sync`：pending 原子落为 committed（enoscpc/torn 在此注入）
//! - `crash()`：丢弃全部 pending（模拟进程死亡）——**committed 保留**
//! - `torn_write_prob`：sync 时按概率只落前半块（块粒度截断）
//! - `enospc_after`：第 N 次写起返回 ENOSPC
//! - `fail_writes`：确定性写失败开关（回归测试瞬时故障注入点）
//! - `list`/`head`：只反映 committed（恢复视角 = crash 后视角）
//!
//! 注意：dendro 的写路径语义是"PUT 即持久边界"（LocalObjStore fsync），
//! pending 状态对应"write 调用了但 fsync 未返回"的窗口——crash() 丢弃的
//! 就是这个窗口。这与 WAL 的 Uncertain 语义（SPEC 02 §3.5）同构。

use crate::objstore::{HeadInfo, ObjError, ObjResult, ObjStore};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Default)]
struct SimFile {
    /// 已持久化字节（crash 后存活）
    committed: Vec<u8>,
    /// 已写入未 fsync 字节（crash 丢弃）
    pending: Vec<u8>,
}

#[derive(Default)]
struct SimInner {
    files: Mutex<BTreeMap<String, SimFile>>,
    write_count: AtomicU64,
    torn_write_prob: f64,
    enospc_after: u64,
    pub fail_writes: AtomicBool,
}

/// 故障注入仿真对象存储。Clone 共享同一状态（Arc）。
#[derive(Clone)]
pub struct SimObjStore {
    inner: std::sync::Arc<SimInner>,
}

impl SimObjStore {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(SimInner {
                files: Mutex::new(BTreeMap::new()),
                write_count: AtomicU64::new(0),
                torn_write_prob: 0.0,
                enospc_after: 0,
                fail_writes: AtomicBool::new(false),
            }),
        }
    }

    pub fn with_faults(torn_write_prob: f64, enospc_after: u64) -> Self {
        Self {
            inner: std::sync::Arc::new(SimInner {
                files: Mutex::new(BTreeMap::new()),
                write_count: AtomicU64::new(0),
                torn_write_prob,
                enospc_after,
                fail_writes: AtomicBool::new(false),
            }),
        }
    }

    /// 确定性写失败开关（append/put 一律 ENOSPC）
    pub fn set_fail_writes(&self, on: bool) {
        self.inner.fail_writes.store(on, Ordering::SeqCst);
    }

    /// 模拟进程崩溃：丢弃全部 pending（未 fsync 字节）。
    /// 已 committed 字节保留。文件若 committed 为空且无 pending → 整个消失。
    pub fn crash(&self) {
        let mut files = self.inner.files.lock().unwrap();
        files.retain(|_, f| !f.committed.is_empty());
        for f in files.values_mut() {
            f.pending.clear();
        }
    }

    /// flush 全部 pending（显式 fsync 语义；正常写路径自动触发）
    fn sync_one(&self, f: &mut SimFile) -> ObjResult<()> {
        use rand::Rng;
        // torn write 注入：按概率只落前半块
        if self.inner.torn_write_prob > 0.0
            && !f.pending.is_empty()
            && rand::rng().random_bool(self.inner.torn_write_prob)
        {
            let cut = f.pending.len() / 2;
            f.committed.extend_from_slice(&f.pending[..cut]);
            f.pending.drain(..cut);
            return Err(ObjError::Io("sim: torn write".into()));
        }
        f.committed.append(&mut f.pending);
        Ok(())
    }

    fn should_fail(&self) -> bool {
        if self.inner.fail_writes.load(Ordering::SeqCst) {
            return true;
        }
        let limit = self.inner.enospc_after;
        if limit > 0 && self.inner.write_count.fetch_add(1, Ordering::SeqCst) + 1 > limit {
            return true;
        }
        false
    }
}

impl Default for SimObjStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjStore for SimObjStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        let files = self.inner.files.lock().unwrap();
        match files.get(path) {
            None => Err(ObjError::NotFound(path.to_string())),
            Some(f) => {
                // 读视角 = crash 视角：只可见 committed（附录：读 pending 是
                // R7-①类缺陷的根因形态）
                let mut b = f.committed.clone();
                b.extend_from_slice(&f.pending);
                if b.is_empty() {
                    return Err(ObjError::NotFound(path.to_string()));
                }
                Ok(Bytes::from(b))
            }
        }
    }

    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        let files = self.inner.files.lock().unwrap();
        match files.get(path) {
            None => Err(ObjError::NotFound(path.to_string())),
            Some(f) => {
                let all = [&f.committed[..], &f.pending[..]].concat();
                let start = off as usize;
                if start > all.len() {
                    return Err(ObjError::NotFound(path.to_string()));
                }
                let end = (start + len).min(all.len());
                Ok(Bytes::from(all[start..end].to_vec()))
            }
        }
    }

    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        if self.should_fail() {
            return Err(ObjError::Io("sim: injected write failure".into()));
        }
        let mut files = self.inner.files.lock().unwrap();
        let f = files.entry(path.to_string()).or_default();
        f.committed = data.to_vec();
        f.pending.clear();
        Ok(())
    }

    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        let mut files = self.inner.files.lock().unwrap();
        if files.contains_key(path) {
            return Err(ObjError::Exists(path.to_string()));
        }
        files.insert(
            path.to_string(),
            SimFile {
                committed: data.to_vec(),
                pending: Vec::new(),
            },
        );
        Ok(())
    }

    /// 偏移写（WAL 追加模式）：写 pending + 自动 sync_one（fsync 语义）
    fn append_at(&self, path: &str, offset: u64, data: &[u8]) -> ObjResult<()> {
        if self.should_fail() {
            return Err(ObjError::Io("sim: injected append failure".into()));
        }
        let mut files = self.inner.files.lock().unwrap();
        let f = files.entry(path.to_string()).or_default();
        let end = offset as usize + data.len();
        if f.committed.len() < end {
            f.committed.resize(end, 0);
        }
        f.committed[offset as usize..end].copy_from_slice(data);
        drop(files);
        // fsync 语义由 sync_one 承担（torn/enospc 注入点一致）
        let mut files = self.inner.files.lock().unwrap();
        if let Some(f) = files.get_mut(path) {
            self.sync_one(f)?;
        }
        Ok(())
    }

    fn preallocate(&self, _path: &str, _len: u64) -> ObjResult<()> {
        Ok(()) // 仿真无空间概念
    }

    fn resize(&self, path: &str, len: u64) -> ObjResult<()> {
        let mut files = self.inner.files.lock().unwrap();
        if let Some(f) = files.get_mut(path) {
            f.committed.truncate(len as usize);
            f.pending.clear();
        }
        Ok(())
    }

    fn supports_append(&self) -> bool {
        true // 仿真支持全部能力（opfuzz 全路径覆盖）
    }

    fn delete(&self, path: &str) -> ObjResult<()> {
        let mut files = self.inner.files.lock().unwrap();
        files.remove(path);
        Ok(())
    }

    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>> {
        let files = self.inner.files.lock().unwrap();
        Ok(files.get(path).map(|f| HeadInfo {
            len: (f.committed.len() + f.pending.len()) as u64,
        }))
    }

    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        let files = self.inner.files.lock().unwrap();
        Ok(files
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        let data = self.get(from)?;
        self.put(to, data)
    }
}
