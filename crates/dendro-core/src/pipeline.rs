//! P2'-2：提交管线 trait 化——把 commit_tx 的四阶段拆为可替换的
//! trait 边界。单节点实现即为当前代码路径；分布式实现（Raft/Quorum
//! Log + 远程裁决）替换 `Journal`/`Adjudicator` trait 即可，管线代码
//! 不动（SOTA 调研 §1.2 DSQL Adjudicator+Journal 同形态）。
//!
//! **设计决策**：当前 `commit_tx` 直接内联调用 `memtx::validate_only`
//! 和 `wal::append`——因为单节点路径中 trait 调用会引入不必要的间接层
//! （性能最优先红线）。本模块定义的 trait 是**分布式实现就绪**的
//! 编译期接口：P2'-2 分布式落地时，`commit_tx` 改为通过
//! `Box<dyn Adjudicator>` / `Box<dyn Journal>` 调用（或泛型参数化）。
//!
//! 当前状态：**预留接口（未来接线）**。`journal.rs` 与 `consensus/`
//! 已标 EXPERIMENTAL，落地时即被本模块的 trait 替换。
use crate::error::Result;
use crate::memtx::Txn;
use crate::wal::TxnRecord;
use std::sync::Arc;

/// Phase 1：OCC 裁决（first-committer-wins）
/// 单节点实现 = `memtx::validate_only`；分布式实现 = 远程裁决服务
pub trait Adjudicator: Send + Sync {
    /// 验证写集无冲突（须在 commit_mu 内调用——裁决到安装间无并发写者）
    fn adjudicate(&self, txn: &Txn) -> Result<()>;
}

/// Phase 2：持久化（组提交）
/// 单节点实现 = WAL writer append；分布式实现 = Quorum Log append
pub trait Journal: Send + Sync {
    /// 追加事务帧并按 durability 等级等待
    fn append(&self, ts: u64, records: &[TxnRecord], durability: crate::engine::Durability) -> Result<()>;
    /// 写者是否已毒化（P0-D 错误语义）
    fn is_poisoned(&self) -> bool;
}

// —— 单节点参考实现（供测试和文档；commit_tx 当前直接内联调用） ——

/// Phase 1 参考实现：memtx validate_only 的 trait 包装
#[derive(Debug, Default)]
pub struct MemtxAdjudicator;

impl Adjudicator for MemtxAdjudicator {
    fn adjudicate(&self, _txn: &Txn) -> Result<()> {
        // 实际验证需要 BranchMem 上下文（见 commit_tx 中的内联调用）
        // 此处为接口演示；分布式实现时会传入远程裁决上下文
        Ok(())
    }
}

/// Phase 2 参考实现：WAL writer 的 trait 包装
pub struct WalJournal {
    pub writer: Arc<crate::wal::WalWriter>,
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
