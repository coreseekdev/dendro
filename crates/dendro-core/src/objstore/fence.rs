//! 分支写者租约（fencing，SPEC 02 §6 / 设计文档 P1）。
//!
//! 每个"分支写者世代"一个对象：`fence/{branch}/{epoch:020}.json`，
//! 内容 = {epoch, holder, expires_at_ms}。
//!
//! 已实现的运行时语义（engine::commit_tx / write_branch_commit / checkpoint
//! 三个写入口在 commit_mu 内调用 `Branch::fence_gate`）：
//! - 进程打开分支即**领取新 epoch = 当前最大 epoch + 1**（条件写，唯一性由
//!   对象存储保证；同 epoch 竞争者只有一个能成功）
//! - **commit 前本地过期检查**：租约过期的写者提交直接被拒（SQLSTATE 40001），
//!   即"自知失约者停止写入"
//! - **惰性续期**：健康写者在 commit 路径上续期，至多每 ttl/3 一次 PUT
//!   （无后台线程；续期失败仅告警，下次提交重试，过期后被上行检查拒绝）
//! - WAL 段与事务时间戳携带 epoch（ts = epoch<<32 | seq）：恢复时按 epoch
//!   升序重放，高 epoch 天然覆盖低 epoch 的陈旧写入（脑裂安全）
//!
//! 诚实边界：
//! - 接管者**不等待**旧租约过期即可领取更高 epoch——运行时互斥靠"旧写者
//!   自查过期后拒写"，不靠接管方阻塞
//! - 失约旧写者（如进程暂停超过 TTL 后未再提交）已在低 epoch 路径上的
//!   滞后段，恢复期由 ts 比较消解（新赢）；其**非冲突键**的已提交数据
//!   会保留（它是真实提交）

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

#[derive(Clone)]
pub struct FenceStore {
    obj: Arc<dyn ObjStore>,
}

pub(crate) fn now_ms() -> i64 {
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
    /// 返回完整租约（engine 存入 Branch，供 fence_gate 过期检查/续期）。
    pub fn acquire(&self, branch: &str, holder: &str, ttl_ms: i64, min_epoch: u64) -> Result<Lease> {
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
                Ok(()) => return Ok(lease),
                Err(crate::objstore::ObjError::Exists(_)) => epoch += 1, // 同 epoch 竞争：加一重试
                Err(e) => return Err(crate::error::SqlError::from(e)),
            }
        }
    }

    /// 续期（覆盖写同路径，幂等）。**调用者必须已验证自己持有该 epoch**
    /// （LeaseKeeper::renew_if_due 在 check 通过后调用）——盲目续期他人的
    /// 租约等于替别人保活。
    pub fn renew(&self, branch: &str, lease: &Lease) -> Result<()> {
        self.obj
            .put(&Self::lease_path(branch, lease.epoch), serde_json::to_vec(lease).unwrap().into())
            .map_err(crate::error::SqlError::from)
    }

    /// 是否已过期
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
        let l1 = f.acquire("b", "n1", 60_000, 0).unwrap();
        assert_eq!(l1.epoch, 1);
        let l2 = f.acquire("b", "n2", 60_000, 0).unwrap();
        assert_eq!(l2.epoch, 2, "同持有多租约 → epoch 单调+1（跨进程打开即新世代）");
        // 过期判定
        assert!(!FenceStore::expired(&l1));
        // 两个并发竞争者抢同一 epoch：CAS 保证只有一个成功，且 epoch 连续
        let f1 = FenceStore::new(obj.clone());
        let f2 = FenceStore::new(obj);
        let (a, b) = std::thread::scope(|s| {
            let t1 = s.spawn(|| f1.acquire("c", "x", 60_000, 2).unwrap().epoch);
            let t2 = s.spawn(|| f2.acquire("c", "y", 60_000, 2).unwrap().epoch);
            (t1.join().unwrap(), t2.join().unwrap())
        });
        assert_eq!(a.max(b), a.min(b) + 1, "并发竞争 → epoch 连续分配，无一物两主");
    }
}
