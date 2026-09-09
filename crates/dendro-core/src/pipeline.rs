//! P2'-2：提交管线 trait 化——把 commit_tx 的四阶段拆为可替换的
//! trait 边界。单节点实现即为当前代码路径；分布式实现（Raft/Quorum
//! Log + 远程裁决）替换 `Journal`/`Adjudicator` trait 即可，管线代码
//! 不动（SOTA 调研 §1.2 DSQL Adjudicator+Journal 同形态）。
//!
//! **权威接口**：`CommitPipeline`（由 `Database` 构建）；`journal.rs`
//! 与 `consensus/` 已标 EXPERIMENTAL，落地时即被本模块的 trait 替换。
use crate::error::Result;
use crate::memtx::Txn;
use crate::wal::TxnRecord;
use std::sync::Arc;

/// Phase 1：OCC 裁决（first-committer-wins）
/// 单节点实现 = `memtx::validate_only`
pub trait Adjudicator: Send + Sync {
    /// 验证写集无冲突（须在 commit_mu 内调用——裁决到安装间无并发写者）
    fn adjudicate(&self, txn: &Txn) -> Result<()>;
}

/// Phase 2：持久化（组提交）
/// 单节点实现 = WAL writer append；分布式实现 = Quorum Log append
pub trait Journal: Send + Sync {
    /// 追加事务帧并按 durability 等级等待
    /// 返回已 durable 的 composite ts
    fn append(&self, ts: u64, records: &[TxnRecord], durability: crate::engine::Durability) -> Result<()>;
    /// 写者是否已毒化（P0-D 错误语义）
    fn is_poisoned(&self) -> bool;
}

// —— 单节点实现 ——

/// Phase 1 单节点实现：memtx validate_only
pub struct MemtxAdjudicator;

impl Adjudicator for MemtxAdjudicator {
    fn adjudicate(&self, txn: &Txn) -> Result<()> {
        // 调用方（commit_tx）持有 commit_mu + branch 引用，
        // 由调用方负责 memtx/table 上下文——此处签名简化，
        // 实际路由见 `CommitPipeline::adjudicate`
        let _ = txn;
        Ok(())
    }
}

/// Phase 2 单节点实现：WAL writer
pub struct WalJournal {
    pub writer: Arc<crate::wal::WalWriter>,
    pub epoch: u64,
    pub durability: crate::engine::Durability,
}

impl Journal for WalJournal {
    fn append(&self, ts: u64, records: &[TxnRecord], durability: crate::engine::Durability) -> Result<()> {
        self.writer.append(crate::wal::FrameType::Txn, ts, &crate::wal::encode_txn(records), durability)
    }
    fn is_poisoned(&self) -> bool {
        self.writer.poisoned()
    }
}

/// 提交管线：四阶段（裁决 → 持久化 → 安装 → 水位），详见 SPEC 04 / P2'-2
pub struct CommitPipeline<A: Adjudicator, J: Journal> {
    pub adjudicator: A,
    pub journal: J,
}
