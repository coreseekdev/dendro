//! 对象存储层（SPEC 01）：不可变对象 + 条件写 + 无 LIST 恢复。

pub mod cached;
pub mod cas;
pub mod fence;
pub mod local;
pub mod manifest;
pub mod s3;
pub mod memory;
pub mod throttled;

use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ObjError {
    #[error("object exists: {0}")]
    Exists(String),
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("io: {0}")]
    Io(String),
    #[error("transient: {0}")]
    Transient(String),
    /// 结果不确定：请求可能已成功（超时/连接中断）。调用方按路径语义消解：
    /// manifest → GET 反查 payload 内嵌 putid；内容寻址对象 → 同字节重试幂等。
    #[error("uncertain: {0}")]
    Uncertain(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
}

pub type ObjResult<T> = std::result::Result<T, ObjError>;

impl From<std::io::Error> for ObjError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::AlreadyExists => ObjError::Exists("path".into()),
            std::io::ErrorKind::NotFound => ObjError::NotFound("path".into()),
            _ => ObjError::Io(e.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadInfo {
    pub len: u64,
}

/// 对象存储抽象（SPEC 01 §2）。所有对象写入后不可变；
/// `put_if_absent` 是 fencing/乐观提交的基石。
pub trait ObjStore: Send + Sync + 'static {
    fn get(&self, path: &str) -> ObjResult<Bytes>;
    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes>;
    fn put(&self, path: &str, data: Bytes) -> ObjResult<()>;
    /// 不存在才成功；已存在 → Err(Exists)
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()>;
    fn delete(&self, path: &str) -> ObjResult<()>;
    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>>;
    /// 仅本地/内存实现高效；恢复关键路径不得依赖
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>>;
    fn copy(&self, from: &str, to: &str) -> ObjResult<()>;
}

pub type SharedObjStore = Arc<dyn ObjStore>;

/// 重试包裹：瞬时错误指数退避（SPEC 01 §3）
pub fn retry<T>(mut f: impl FnMut() -> ObjResult<T>, max_retries: u32) -> ObjResult<T> {
    let mut attempt = 0u32;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(ObjError::Transient(msg)) if attempt < max_retries => {
                let delay = Duration::from_millis(100u64.saturating_mul(1 << attempt.min(5)));
                tracing::warn!(msg, delay_ms = delay.as_millis() as u64, "objstore retry");
                std::thread::sleep(delay);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 路径安全检查：只允许 [A-Za-z0-9_\-./] 且无 `..` 段。
/// 允许尾部 `/`（前缀列举场景，如 "wal/"）。
pub fn validate_path(p: &str) -> ObjResult<()> {
    if p.is_empty() || p.starts_with('/') {
        return Err(ObjError::Io(format!("bad path {p}")));
    }
    let trimmed = p.strip_suffix('/').unwrap_or(p);
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == ".." {
            return Err(ObjError::Io(format!("bad path segment in {p}")));
        }
        if !seg.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) {
            return Err(ObjError::Io(format!("bad char in path {p}")));
        }
    }
    Ok(())
}
