//! 共识抽象层：与具体 Raft 库解耦 + cloud-native 分层日志存储（SOTA 调研 §1.1/§1.3）。
//!
//! 两层抽象：
//! 1. `ConsensusNode` — 提交/选举/心跳 的共识行为接口（适配 raft-rs / openraft / 自研）
//! 2. `ConsensusLogStore` — Raft 日志的存储抽象（NVMe 热层 + OSS 冷层，cloud-native）
//!
//! 设计动机（用户修正）：
//! - 直接耦合 raft-rs 的 `Storage` trait 会导致 Raft 日志存储绑定本地盘语义
//! - OSS 没有原地改写、RTT 高 ⇒ Raft 日志需要"NVMe 热 + OSS 冷"的分层
//! - 抽象层使得切换 Raft 库（raft-rs → openraft）或存储（Local → S3）不触动协议代码

pub mod log_store;
pub mod raft_rs;

use crate::error::Result;

/// 共识日志条目
#[derive(Debug, Clone, Default)]
pub struct ConsensusEntry {
    pub index: u64,
    pub term: u64,
    pub data: Vec<u8>,
}

/// 已提交批次的消费回调载荷
#[derive(Debug, Clone)]
pub struct CommittedBatch {
    pub index: u64,
    pub term: u64,
    pub data: Vec<u8>,
}

/// 共识节点抽象——与具体 Raft 库解耦。
///
/// 实现者负责：
/// - 驱动内部状态机（tick/消息收发/Ready 消费）
/// - 通过 `ConsensusLogStore` 持久化日志条目
/// - 自动选主（Raft）或外部授序（fencing/PacificA）
pub trait ConsensusNode: Send {
    /// 追加一个提交批次。返回全局 ts（多数派持久后）。
    /// 非 Leader 返回 Err（客户端应重路由到 Leader）。
    fn propose(&mut self, data: Vec<u8>) -> Result<u64>;

    /// 驱动内部状态机：处理 tick、消费消息、产出 committed batches。
    /// 返回自上次 poll 以来已提交的批次。
    fn poll(&mut self) -> Result<Vec<CommittedBatch>>;

    /// 是否 leader
    fn is_leader(&self) -> bool;

    /// 当前已知 leader（None = 选举中）
    fn leader_id(&self) -> Option<u64>;

    /// 当前任期
    fn term(&self) -> u64;

    /// 已提交的最高日志 index
    fn committed_index(&self) -> u64;

    /// 创建快照（用于慢 follower 追平）
    fn create_snapshot(&mut self) -> Result<ConsensusEntry>;

    /// 从快照恢复（日志截断到快照点）
    fn restore_snapshot(&mut self, snap: ConsensusEntry) -> Result<()>;
}

/// 共识日志存储抽象——cloud-native 分层（NVMe 热 + OSS 冷）。
pub trait ConsensusLogStore: Send + Sync {
    /// 追加条目（写入热层 NVMe）
    fn append(&self, entries: &[ConsensusEntry]) -> Result<()>;
    /// 读取 [low, high) 范围内的条目（热层命中 → NVMe；否则 → OSS 冷层）
    fn read(&self, low: u64, high: u64) -> Result<Vec<ConsensusEntry>>;
    /// 截断 ≥ idx 的条目（raft 不稳定日志回退时调用）
    fn truncate_from(&self, idx: u64) -> Result<()>;
    /// 压缩 ≤ idx 的条目（已应用到快照的旧日志可删）
    fn compact_to(&self, idx: u64) -> Result<()>;
    fn first_index(&self) -> Result<u64>;
    fn last_index(&self) -> Result<u64>;
    /// 将 ≤ idx 的条目上传到 OSS 冷层（异步触发，批量化）
    fn tier_to_oss(&self, idx: u64) -> Result<()>;
    /// 从 OSS 冷层恢复（节点重启后热层为空）
    fn restore_from_oss(&self) -> Result<()>;
}
