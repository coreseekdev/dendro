//! tikv/raft-rs 适配器：实现 `ConsensusNode` 抽象（与具体库解耦）。
//!
//! 驱动模式（raft-rs 的 Ready 循环）：
//! ```text
//! propose(data) → RawNode.propose()
//! poll():
//!   1. tick（定时）
//!   2. step（收到的消息）
//!   3. ready() → persist entries → send messages → advance
//!   4. 收集 committed entries → 返回给调用方
//! ```

use super::{CommittedBatch, ConsensusEntry, ConsensusNode, ConsensusLogStore};
use crate::error::{Result, SqlError};
use raft::prelude::*;
use raft::{Config, RawNode, Storage};
use slog::Drain;
use std::sync::Arc;

/// raft-rs 适配器
pub struct RaftRsNode {
    node: RawNode<TieredStorageAdapter>,
    /// 待返回的已提交批次
    pending_committed: Vec<CommittedBatch>,
}

/// 把 `ConsensusLogStore` 适配为 raft-rs 的 `Storage` trait
pub struct TieredStorageAdapter {
    log_store: Arc<dyn ConsensusLogStore>,
    state: raft::RaftState,
}

impl TieredStorageAdapter {
    pub fn new(log_store: Arc<dyn ConsensusLogStore>, node_id: u64) -> Self {
        Self {
            log_store,
            state: raft::RaftState {
                hard_state: raft::eraftpb::HardState::default(),
                conf_state: {
                    let mut cs = raft::eraftpb::ConfState::default();
                    cs.voters = vec![node_id].into();
                    cs
                },
            },
        }
    }
}

impl Storage for TieredStorageAdapter {
    fn initial_state(&self) -> raft::Result<raft::RaftState> {
        Ok(self.state.clone())
    }
    fn entries(
        &self,
        low: u64,
        high: u64,
        _max_size: impl Into<Option<u64>>,
        _ctx: raft::GetEntriesContext,
    ) -> raft::Result<Vec<Entry>> {
        let entries = self
            .log_store
            .read(low, high)
            .map_err(|e| raft::Error::Store(raft::StorageError::Other(Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)))))?;
        Ok(entries
            .into_iter()
            .map(|e| {
                let mut entry = Entry::default();
                entry.index = e.index;
                entry.term = e.term;
                entry.data = e.data.into();
                entry
            })
            .collect())
    }
    fn term(&self, idx: u64) -> raft::Result<u64> {
        if idx == 0 {
            return Ok(0);
        }
        self.log_store
            .read(idx, idx + 1)
            .ok()
            .and_then(|v| v.first().map(|e| e.term))
            .ok_or(raft::Error::Store(raft::StorageError::Compacted))
    }
    fn first_index(&self) -> raft::Result<u64> {
        self.log_store
            .first_index()
            .map_err(|_| raft::Error::Store(raft::StorageError::Compacted))
    }
    fn last_index(&self) -> raft::Result<u64> {
        self.log_store
            .last_index()
            .map_err(|_| raft::Error::Store(raft::StorageError::Compacted))
    }
    fn snapshot(&self, _request_index: u64, _to: u64) -> raft::Result<Snapshot> {
        Err(raft::Error::Store(raft::StorageError::SnapshotTemporarilyUnavailable))
    }
}

impl RaftRsNode {
    pub fn new(
        node_id: u64,
        log_store: Arc<dyn ConsensusLogStore>,
    ) -> Result<Self> {
        let config = Config {
            id: node_id,
            election_tick: 20,
            heartbeat_tick: 5,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            check_quorum: true,
            skip_bcast_commit: true,
            ..Default::default()
        };
        config.validate().map_err(|e| SqlError::internal(format!("raft config: {e}")))?;

        let adapter = TieredStorageAdapter::new(log_store, node_id);
        let drain = slog::Discard;
        let logger = slog::Logger::root(drain, slog::o!());
        let mut node = RawNode::new(&config, adapter, &logger)
            .map_err(|e| SqlError::internal(format!("raft node: {e}")))?;

        // 单节点：发起选举成为 leader
        if node_id != 0 {
            node.campaign().map_err(|e| SqlError::internal(format!("raft campaign: {e}")))?;
        }

        Ok(Self { node, pending_committed: Vec::new() })
    }
}

impl ConsensusNode for RaftRsNode {
    fn propose(&mut self, data: Vec<u8>) -> Result<u64> {
        self.node
            .propose(vec![], data)
            .map_err(|e| SqlError::internal(format!("raft propose: {e}")))?;
        self.poll()?;
        Ok(self.node.raft.r.raft_log.committed)
    }

    fn poll(&mut self) -> Result<Vec<CommittedBatch>> {
        if !self.node.has_ready() {
            return Ok(vec![]);
        }
        let mut ready = self.node.ready();
        let mut committed = Vec::new();

        // 持久化 entries（由 ConsensusLogStore 处理）
        if !ready.entries().is_empty() {
            let entries: Vec<ConsensusEntry> = ready
                .entries()
                .iter()
                .map(|e| ConsensusEntry {
                    index: e.index,
                    term: e.term,
                    data: e.data.to_vec(),
                })
                .collect();
            let _ = entries; // ConsensusLogStore::append 在 M2 接入
        }

        // 消费已提交的 entries
        for entry in ready.take_committed_entries().drain(..) {
            if entry.data.is_empty() {
                continue;
            }
            committed.push(CommittedBatch {
                index: entry.index,
                term: entry.term,
                data: entry.data.to_vec(),
            });
        }

        self.node.advance(ready);
        self.pending_committed.extend(committed.clone());
        Ok(committed)
    }

    fn is_leader(&self) -> bool {
        self.node.raft.state == raft::StateRole::Leader
    }

    fn leader_id(&self) -> Option<u64> {
        if self.node.raft.state == raft::StateRole::Leader {
            Some(self.node.raft.r.id)
        } else {
            if self.node.raft.r.leader_id == raft::INVALID_ID { None } else { Some(self.node.raft.r.leader_id) }
        }
    }

    fn term(&self) -> u64 {
        self.node.raft.term
    }

    fn committed_index(&self) -> u64 {
        self.node.raft.raft_log.committed
    }

    fn create_snapshot(&mut self) -> Result<ConsensusEntry> {
        // v1：空快照（日志完整保留，无需快照安装）
        Ok(ConsensusEntry::default())
    }

    fn restore_snapshot(&mut self, _snap: ConsensusEntry) -> Result<()> {
        Ok(())
    }
}

// 需要 raft::Storage 的引用传递（RawNode 持有 storage）
// 但 raft-rs 0.7 的 RawNode 拥有 storage，因此通过内部字段访问
