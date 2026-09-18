//! 对象存储层（SPEC 01）：不可变对象 + 条件写 + 无 LIST 恢复。

pub mod cached;
pub mod cas;
pub mod fence;
pub mod local;
pub mod manifest;
pub mod memory;
pub mod s3;
pub mod sim;
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
    /// 批量写路径的单对象写入（跳过逐文件 fsync——批末由
    /// [`Self::sync_batch`] 一次性同步；默认退化为 put 保持语义）
    fn put_no_sync(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.put(path, data)
    }
    /// 批量写收尾：一次文件系统级同步（覆盖本批全部写入 + 目录项）。
    /// 默认 no-op（S3/Memory：写 ACK 即持久）
    fn sync_batch(&self) -> ObjResult<()> {
        Ok(())
    }
    /// 不存在才成功；已存在 → Err(Exists)
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()>;
    fn delete(&self, path: &str) -> ObjResult<()>;
    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>>;
    /// 仅本地/内存实现高效；恢复关键路径不得依赖
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>>;
    fn copy(&self, from: &str, to: &str) -> ObjResult<()>;
    /// 偏移就地写（P2-6e WAL 段优化）：在 `offset` 处覆写 `data` 并
    /// **fdatasync**。配合 [`Self::preallocate`] 使用（文件尺寸固定 ⇒
    /// fdatasync 免元数据日志，durable 提交延迟 ≈2×）。
    /// 语义契约：单写者（调用方 = 分支 flush 单飞）；写入非原子——读到
    /// 中途字节的读者按"撕尾容忍"处理（WAL 段专用，恢复端已容忍）。
    /// 默认不支持（对象存储无偏移持久写）——`supports_append()` = false，
    /// WAL 写入端自动退化为整段 put。
    fn append_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> ObjResult<()> {
        Err(ObjError::Io("append unsupported by this store".into()))
    }
    /// 预分配（fallocate）：空间+尺寸一步到位，后续 append_at 不改尺寸。
    /// 默认 no-op（不支持 = 退化为按需扩展，fdatasync 含元数据、较慢但正确）
    fn preallocate(&self, _path: &str, _len: u64) -> ObjResult<()> {
        Ok(())
    }
    /// 缩到实际使用尺寸（封段时回收预分配空间；默认 no-op，失败仅浪费空间）
    fn resize(&self, _path: &str, _len: u64) -> ObjResult<()> {
        Ok(())
    }
    fn supports_append(&self) -> bool {
        false
    }
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
        if !seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            return Err(ObjError::Io(format!("bad char in path {p}")));
        }
    }
    Ok(())
}

/// 条件写能力自检（Lance 调研 P0-2 落地 / UnsafeCommitHandler 教训）：
/// 目标存储若**静默忽略** If-None-Match（重复 create 返回无条件成功），
/// put_if_absent 将无声覆盖——fence 租约唯一性与 manifest CAS 的安全
/// 模型整体失效。开工即探测：二次 create 必须被拒（Exists）。
/// Uncertain 放行（与现有消解语义一致——报不确定错误的存储不是
/// "忽略条件写"的证据；真忽略者返回 Ok）。探测对象即测即删。
pub fn verify_conditional_put(obj: &dyn ObjStore) -> ObjResult<()> {
    let path = format!(".dendro-selftest/condput-{}", rand_path());
    let r = (|| -> ObjResult<()> {
        obj.put_if_absent(&path, Bytes::from_static(b"probe"))?;
        match obj.put_if_absent(&path, Bytes::from_static(b"probe2")) {
            Err(ObjError::Exists(_)) => Ok(()),
            Ok(()) => Err(ObjError::Io(
                "conditional-put self-test failed: store accepted a duplicate create \
                 (If-None-Match ignored) — this store cannot safely back dendro: \
                 fence lease uniqueness and manifest CAS would be silently unsafe"
                    .into(),
            )),
            Err(ObjError::Uncertain(_)) => Ok(()),
            Err(e) => Err(e),
        }
    })();
    let _ = obj.delete(&path);
    r
}

fn rand_path() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (std::process::id() as u64).rotate_left(32)
}

#[cfg(test)]
mod condput_tests {
    use super::*;
    use crate::objstore::memory::MemoryObjStore;
    use std::sync::Arc;

    /// 正确实现条件写的存储：自检通过
    #[test]
    fn condput_ok_on_conforming_store() {
        let obj: Arc<dyn ObjStore> = Arc::new(MemoryObjStore::new());
        assert!(verify_conditional_put(obj.as_ref()).is_ok());
    }

    /// 静默忽略条件写的存储（重复 create 返回 Ok）：必须被响亮拒绝
    /// ——UnsafeCommitHandler 教训的开工面防线
    #[test]
    fn condput_rejects_ignoring_store() {
        struct IgnoringStore;
        impl ObjStore for IgnoringStore {
            fn get(&self, _p: &str) -> ObjResult<Bytes> {
                Err(ObjError::NotFound(_p.into()))
            }
            fn put(&self, _p: &str, _d: Bytes) -> ObjResult<()> {
                Ok(())
            }
            fn put_if_absent(&self, _p: &str, _d: Bytes) -> ObjResult<()> {
                Ok(()) // 无条件成功——模拟忽略 If-None-Match 的存储
            }
            fn delete(&self, _p: &str) -> ObjResult<()> {
                Ok(())
            }
            fn head(&self, _p: &str) -> ObjResult<Option<HeadInfo>> {
                Ok(None)
            }
            fn list_prefix(&self, _p: &str) -> ObjResult<Vec<String>> {
                Ok(vec![])
            }
            fn get_range(&self, _p: &str, _o: u64, _l: usize) -> ObjResult<Bytes> {
                Err(ObjError::NotFound(_p.into()))
            }
            fn copy(&self, _a: &str, _b: &str) -> ObjResult<()> {
                Ok(())
            }
        }
        let e = verify_conditional_put(&IgnoringStore).unwrap_err();
        assert!(
            e.to_string().contains("self-test failed"),
            "坏存储必须被拒：{e}"
        );
    }
}
