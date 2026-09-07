//! dendro-core — Dendro 引擎核心
//!
//! 分层（自底向上）：format → objstore → prolly → versioned → wal → memtx
//! → catalog → sql → engine。
//! 性能红线：热路径无 Rc/RefCell；共享仅限不可变 Arc；见 spec/00-overview.md §6。

pub mod error;
pub mod types;
pub mod format;
pub mod objstore;
pub mod prolly;
pub mod versioned;
pub mod wal;

pub mod memtx;
pub mod recovery;
pub mod kv;
pub mod sql;

pub mod engine; // 门面：Database/Session（wire 层唯一入口）

// 顶层 re-export（wire/bench/slt 使用方）
pub use engine::{Database, DbOptions, Durability, PrepareMeta, Session, StoreConfig, WireSession};
pub use error::{Result, SqlError};
pub use types::{ColType, ColumnMeta, Output, RecordSet, SqlValue};
