//! 分支写者租约（fencing，SPEC 02 §6 / 设计文档 P1）。
//!
//! 每个"分支写者世代"一个不可变对象：`fence/{branch}/{epoch:020}.json`，
//! 内容 = {epoch, holder, expires_at_ms}。
//!
//! 规则：
//! - 进程打开分支即**领取新 epoch = 当前最大 epoch + 1**（条件写，唯一性由
//!   对象存储保证；同 epoch 竞争者只有一个能成功）
//! - 持有者周期性重写自己的租约文件（续期）；停止续期 = 租约过期
//! - WAL 段与事务时间戳携带 epoch（ts = epoch<<32 | seq）：恢复时按 epoch
//!   升序重放，高 epoch 天然覆盖低 epoch 的陈旧写入（脑裂安全）
//! - 过期租约的持有者**必须停止写入**（commit 前的本地过期检查 +
//!   后台续期线程）；滞后写出的段落在低 epoch 路径上，重放时被跳过/覆盖
//!
//! 诚实边界：无控制面时，"过期后仍在写的旧节点"其**非冲突键**的已提交
//! 数据会保留（它是真实提交）；与新 epoch 冲突的键由 ts 比较消解（新赢）。

use crate::error::Result;
use crate::objstore::ObjStore;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub epoch: u64,
    pub holder: String,
    pub expires_at_ms: i64,
}

pub struct FenceStore {
    obj: Arc<dyn ObjStore>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

impl FenceStore {
    pub fn new(obj: Arc<dyn ObjStore>) -> Self {
        Self { obj }
    }

    fn lease_path(branch: &str, epoch: u64) -> String {
        format!("fence/{branch}/{epoch:020}.json")
    }

    #[allow(dead_code)]
    fn read_lease(&self, branch: &str, epoch: u64) -> Result<Option<Lease>> {
        match self.obj.get(&Self::lease_path(branch, epoch)) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| crate::error::SqlError::io(format!("fence parse: {e}"))),
            Err(crate::objstore::ObjError::NotFound(_)) => Ok(None),
            Err(e) => Err(crate::error::SqlError::from(e)),
        }
    }

    /// 当前最大 epoch（恢复/接管入口用；此处允许 LIST——频率极低）
    pub fn max_epoch(&self, branch: &str) -> Result<u64> {
        let prefix = format!("fence/{branch}/");
        let mut max = 0u64;
        for p in self.obj.list_prefix(&prefix)? {
            let stem = p
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(".json"))
                .unwrap_or("");
            if let Ok(e) = stem.parse::<u64>() {
                max = max.max(e);
            }
        }
        Ok(max)
    }

    /// 领取新 epoch = max+1（条件写保证唯一；冲突 = 重试更高 epoch）。
    /// `min_epoch` 下界：接管者至少要超过已知的旧 epoch。
    pub fn acquire(&self, branch: &str, holder: &str, ttl_ms: i64, min_epoch: u64) -> Result<u64> {
        let mut epoch = self.max_epoch(branch)?.max(min_epoch) + 1;
        loop {
            let lease = Lease {
                epoch,
                holder: holder.into(),
                expires_at_ms: now_ms() + ttl_ms,
            };
            match self
                .obj
                .put_if_absent(&Self::lease_path(branch, epoch), serde_json::to_vec(&lease).unwrap().into())
            {
                Ok(()) => return Ok(epoch),
                Err(crate::objstore::ObjError::Exists(_)) => epoch += 1, // 同 epoch 竞争：加一重试
                Err(e) => return Err(crate::error::SqlError::from(e)),
            }
        }
    }

    /// 续期（仅 epoch 持有者调用；覆盖写同内容语义，幂等）
    pub fn renew(&self, branch: &str, lease: &Lease) -> Result<()> {
        self.obj
            .put(&Self::lease_path(branch, lease.epoch), serde_json::to_vec(lease).unwrap().into())
            .map_err(crate::error::SqlError::from)
    }

    /// 是否已过期
    #[allow(dead_code)]
    pub fn expired(lease: &Lease) -> bool {
        lease.expires_at_ms <= now_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objstore::memory::MemoryObjStore;
    use std::sync::Arc as A;

    #[test]
    fn acquire_monotonic_and_exclusive() {
        let obj: A<dyn ObjStore> = A::new(MemoryObjStore::new());
        let f = FenceStore::new(obj.clone());
        assert_eq!(f.max_epoch("b").unwrap(), 0);
        let e1 = f.acquire("b", "n1", 60_000, 0).unwrap();
        assert_eq!(e1, 1);
        let e2 = f.acquire("b", "n2", 60_000, 0).unwrap();
        assert_eq!(e2, 2, "同持有多租约 → epoch 单调+1（跨进程打开即新世代）");
        // 过期判定
        let l = f.read_lease("b", e1).unwrap().unwrap();
        assert!(!FenceStore::expired(&l));
        // 两个并发竞争者抢同一 epoch：CAS 保证只有一个成功
        let (tx, rx) = std::sync::mpsc::channel();
        let f1 = FenceStore::new(obj.clone());
        let f2 = FenceStore::new(obj);
        std::thread::scope(|s| {
            let t1 = s.spawn(|| f1.acquire("c", "x", 60_000, 2).unwrap());
            let t2 = s.spawn(|| f2.acquire("c", "y", 60_000, 2).unwrap());
            let a = t1.join().unwrap();
            let b = t2.join().unwrap();
            tx.send((a.min(b), a.max(b))).unwrap();
        });
        let (lo, hi) = rx.recv().unwrap();
        assert_eq!(hi, lo + 1, "并发竞争 → epoch 连续分配，无一物两主");
    }
}
