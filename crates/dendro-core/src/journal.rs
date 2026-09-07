//! Journal——多副本提交日志（SOTA 调研 §1.2 / DSQL Adjudicator+Journal 同形态）。
//!
//! 底座 = tikv/raft-rs（TiKV 级生产验证的 Rust Raft）。
//! M1（当前）：单节点集群（1 voter），行为等价于现有 WAL 但走 Raft 状态机；
//! M2（多节点）：同代码，多 voter + 传输层。
//!
//! 提交管线：
//! ```text
//! Session.commit
//!   → Kv/SQL 写集
//!   → Journal.propose(batch)    Raft propose → 多数派持久 → committed
//!   → memtx install + pending   （从 committed 批次重放）
//!   → watermark 推进
//! ```

use raft::prelude::*;
use raft::{Config, Storage};
use crate::error::SqlError;
use crate::error::Result;
use slog::Drain;

/// 提交批次：一次事务的全部变更
#[derive(Debug, Clone)]
pub struct CommitBatch {
    pub branch: String,
    pub data: Vec<u8>,
}

/// Journal 的 raft Storage 实现（内存版，M1；M2 可换磁盘/共享存储）
pub struct JournalStorage {
    inner: raft::storage::MemStorage,
}

impl JournalStorage {
    pub fn new() -> Self {
        Self { inner: raft::storage::MemStorage::default() }
    }
}

impl Default for JournalStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for JournalStorage {
    fn initial_state(&self) -> raft::Result<RaftState> {
        self.inner.initial_state()
    }
    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        ctx: raft::GetEntriesContext,
    ) -> raft::Result<Vec<Entry>> {
        self.inner.entries(low, high, max_size, ctx)
    }
    fn term(&self, idx: u64) -> raft::Result<u64> {
        self.inner.term(idx)
    }
    fn first_index(&self) -> raft::Result<u64> {
        self.inner.first_index()
    }
    fn last_index(&self) -> raft::Result<u64> {
        self.inner.last_index()
    }
    fn snapshot(&self, request_index: u64, to: u64) -> raft::Result<Snapshot> {
        self.inner.snapshot(request_index, to)
    }
}

/// Raft Journal 节点：驱动 raft-rs 状态机，对外提供 propose。
pub struct RaftJournal {
    node: raft::RawNode<JournalStorage>,
    ticker: std::time::Instant,
    tick_interval: std::time::Duration,
}

impl RaftJournal {
    pub fn new(node_id: u64) -> crate::error::Result<Self> {
        let config = Config {
            id: node_id,
            election_tick: 20,
            heartbeat_tick: 5,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            check_quorum: false,
            skip_bcast_commit: true,
            ..Default::default()
        };
        config.validate().map_err(|e| SqlError::internal(format!("raft config: {e}")))?;

        let storage = JournalStorage::new();
        let drain = slog::Discard;
        let logger = slog::Logger::root(drain, slog::o!());
        let mut node = raft::RawNode::new(&config, storage, &logger)
            .map_err(|e| SqlError::internal(format!("raft node: {e}")))?;

        // 单节点：自己就是唯一 voter，发起选举成为 leader
        node.campaign().map_err(|e| SqlError::internal(format!("raft campaign: {e}")))?;
        if node.has_ready() {
            let ready = node.ready();
            node.advance(ready);
        }

        Ok(Self {
            node,
            ticker: std::time::Instant::now(),
            tick_interval: std::time::Duration::from_millis(50),
        })
    }

    /// propose 一个提交批次，驱动 ready 直到 committed
    pub fn propose(&mut self, data: Vec<u8>) -> crate::error::Result<()> {
        self.node
            .propose(vec![], data)
            .map_err(|e| SqlError::internal(format!("raft propose: {e}")))?;
        self.process_ready();
        Ok(())
    }

    /// 驱动 raft Ready 循环
    pub fn process_ready(&mut self) {
        while self.node.has_ready() {
            let mut ready = self.node.ready();
            self.node.advance(ready);
        }
    }

    /// 定期 tick（由外部调用方的定时器驱动）
    pub fn tick_if_due(&mut self) {
        if self.ticker.elapsed() >= self.tick_interval {
            self.node.tick();
            self.ticker = std::time::Instant::now();
        }
    }
}
