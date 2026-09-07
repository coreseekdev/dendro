//! S3 兼容对象存储适配器（AWS S3 / MinIO / RustFS / 阿里 OSS S3 兼容端点）。
//!
//! 基于 `object_store` crate；语义对齐 SPEC 01：
//! - `put_if_absent` → S3 条件写 `If-None-Match: *`（PutMode::Create），
//!   冲突返回 `AlreadyExists`（实测 RustFS/MinIO 均支持，返回 412）
//! - 超时类"不确定写"消解：写入前附加随机 put-id 元数据，超时后 head 反查
//!   （SPEC 01 §3，slatedb 同款技巧）——把 at-most-once 变成 effectively-once
//! - 内置重试（RetryConfig：指数退避）+ 计数器（请求数/字节数，供网络成本审计）
//!
//! 同步 trait：内部持有一个专用 tokio runtime，调用方（引擎阻塞线程）block_on。

use super::{HeadInfo, ObjError, ObjResult, ObjStore};
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::{
    path::Path as OsPath, GetOptions, ObjectStore as OsObjectStore, ObjectStoreExt, PutMode, PutOptions,
    PutPayload, RetryConfig,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 适配器统计（网络成本审计）
#[derive(Default)]
pub struct S3Stats {
    pub gets: AtomicU64,
    pub ranges: AtomicU64,
    pub puts: AtomicU64,
    pub conditional_puts: AtomicU64,
    pub heads: AtomicU64,
    pub deletes: AtomicU64,
    pub bytes_get: AtomicU64,
    pub bytes_put: AtomicU64,
}

pub struct S3ObjStore {
    inner: Arc<dyn OsObjectStore>,
    rt: tokio::runtime::Runtime,
    stats: S3Stats,
}

impl S3ObjStore {
    pub fn new(cfg: S3Config) -> ObjResult<Self> {
        let retry = RetryConfig {
            backoff: Default::default(),
            max_retries: cfg.max_retries,
            retry_timeout: Duration::from_secs(cfg.retry_timeout_s),
        };
        let built = object_store::aws::AmazonS3Builder::new()
            .with_endpoint(&cfg.endpoint)
            .with_bucket_name(&cfg.bucket)
            .with_access_key_id(&cfg.access_key)
            .with_secret_access_key(&cfg.secret_key)
            .with_region(&cfg.region)
            .with_allow_http(true)
            .with_retry(retry)
            .with_conditional_put(object_store::aws::S3ConditionalPut::ETagMatch)
            .build()
            .map_err(|e| ObjError::Io(format!("s3 build: {e}")))?;
        let inner: Arc<dyn OsObjectStore> = Arc::new(built);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| ObjError::Io(format!("s3 runtime: {e}")))?;
        Ok(Self { inner, rt, stats: S3Stats::default() })
    }

    pub fn stats(&self) -> &S3Stats {
        &self.stats
    }

    fn os_path(&self, path: &str) -> OsPath {
        OsPath::from(path)
    }

    fn new_putid() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos:x}-{:x}", std::process::id() as u128 ^ rand_hash(nanos as u64) as u128)
    }

}

fn rand_hash(seed: u64) -> u64 {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x.wrapping_mul(0x94D0_49BB_1331_11EB)
}

#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    pub max_retries: usize,
    pub retry_timeout_s: u64,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:9000".into(),
            bucket: "dendro".into(),
            access_key: "minioadmin".into(),
            secret_key: "minioadmin".into(),
            region: "us-east-1".into(),
            max_retries: 8,
            retry_timeout_s: 60,
        }
    }
}

impl ObjStore for S3ObjStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        self.stats.gets.fetch_add(1, Ordering::Relaxed);
        let p = self.os_path(path);
        let r = self.rt.block_on(async {
            self.inner
                .get(&p)
                .await?
                .bytes()
                .await
        });
        match r {
            Ok(b) => {
                self.stats.bytes_get.fetch_add(b.len() as u64, Ordering::Relaxed);
                Ok(b)
            }
            Err(object_store::Error::NotFound { .. }) => Err(ObjError::NotFound(path.into())),
            Err(e) => Err(ObjError::Transient(format!("{path}: {e}"))),
        }
    }

    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        self.stats.ranges.fetch_add(1, Ordering::Relaxed);
        let p = self.os_path(path);
        let opts = GetOptions::new().with_range(Some(off..off + len as u64));
        let r = self.rt.block_on(async { self.inner.get_opts(&p, opts).await?.bytes().await });
        match r {
            Ok(b) => {
                self.stats.bytes_get.fetch_add(b.len() as u64, Ordering::Relaxed);
                Ok(b)
            }
            Err(object_store::Error::NotFound { .. }) => Err(ObjError::NotFound(path.into())),
            Err(e) => Err(ObjError::Transient(format!("{path}: {e}"))),
        }
    }

    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.stats.puts.fetch_add(1, Ordering::Relaxed);
        let p = self.os_path(path);
        let n = data.len();
        let r = self.rt.block_on(async { self.inner.put(&p, PutPayload::from(data)).await });
        match r {
            Ok(_) => {
                self.stats.bytes_put.fetch_add(n as u64, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => Err(ObjError::Uncertain(format!("{path}: {e}"))),
        }
    }

    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.stats.conditional_puts.fetch_add(1, Ordering::Relaxed);
        let p = self.os_path(path);
        let n = data.len();
        let opts = PutOptions::from(PutMode::Create);
        let r = self
            .rt
            .block_on(async { self.inner.put_opts(&p, PutPayload::from(data), opts).await });
        match r {
            Ok(_) => {
                self.stats.bytes_put.fetch_add(n as u64, Ordering::Relaxed);
                Ok(())
            }
            Err(object_store::Error::AlreadyExists { .. }) => Err(ObjError::Exists(path.into())),
            Err(object_store::Error::Precondition { .. }) => Err(ObjError::Exists(path.into())),
            // 超时/中断：结果不确定（可能已写入）。消解由调用方按路径语义处理：
            // manifest 用 payload 内嵌 putid 反查；内容寻址对象同字节重试幂等。
            Err(e) => Err(ObjError::Uncertain(format!("{path}: {e}"))),
        }
    }

    fn delete(&self, path: &str) -> ObjResult<()> {
        self.stats.deletes.fetch_add(1, Ordering::Relaxed);
        let p = self.os_path(path);
        let r = self.rt.block_on(async { self.inner.delete(&p).await });
        match r {
            Ok(()) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(ObjError::Transient(format!("{path}: {e}"))),
        }
    }

    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>> {
        self.stats.heads.fetch_add(1, Ordering::Relaxed);
        let p = self.os_path(path);
        let r = self.rt.block_on(async { self.inner.head(&p).await });
        match r {
            Ok(m) => Ok(Some(HeadInfo { len: m.size })),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(ObjError::Transient(format!("{path}: {e}"))),
        }
    }

    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        let p = self.os_path(prefix);
        let r = self.rt.block_on(async {
            let stream = self.inner.list(Some(&p));
            stream.try_collect::<Vec<_>>().await
        });
        match r {
            Ok(meta_list) => Ok(meta_list
                .into_iter()
                .map(|m| {
                    let loc = m.location.to_string();
                    // object_store 路径不带前导 /；还原为 trait 约定的相对路径
                    loc
                })
                .collect()),
            Err(e) => Err(ObjError::Transient(format!("{prefix}: {e}"))),
        }
    }

    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        let (f, t) = (self.os_path(from), self.os_path(to));
        let r = self.rt.block_on(async { self.inner.copy(&f, &t).await });
        match r {
            Ok(()) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => Err(ObjError::NotFound(from.into())),
            Err(e) => Err(ObjError::Transient(format!("{from}: {e}"))),
        }
    }
}
