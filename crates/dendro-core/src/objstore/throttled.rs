//! 延迟注入对象存储（基准用，SPEC 09 §4）：包装任意实现，按操作类型注入延迟。
//! RTT 分布 = 均值 mean_ms + 均匀抖动 ±jitter_pct，并发上限 semaphore。

use super::{HeadInfo, ObjResult, ObjStore};
use bytes::Bytes;
use parking_lot::Mutex;
use rand::Rng;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct LatencySpec {
    pub mean_ms: f64,
    pub jitter_pct: f64,
}

impl LatencySpec {
    pub fn none() -> Self {
        Self {
            mean_ms: 0.0,
            jitter_pct: 0.0,
        }
    }
}

pub struct ThrottledObjStore {
    inner: Arc<dyn ObjStore>,
    pub get_lat: LatencySpec,
    pub put_lat: LatencySpec,
    pub head_lat: LatencySpec,
    max_concurrency: usize,
    inflight: Mutex<u64>,
    // 统计
    pub stats_gets: AtomicU64,
    pub stats_puts: AtomicU64,
    pub stats_heads: AtomicU64,
    pub stats_bytes_get: AtomicU64,
    pub stats_bytes_put: AtomicU64,
}

impl ThrottledObjStore {
    pub fn new(inner: Arc<dyn ObjStore>, rtt: LatencySpec, max_concurrency: usize) -> Self {
        Self {
            inner,
            get_lat: rtt.clone(),
            put_lat: rtt,
            head_lat: LatencySpec::none(),
            max_concurrency,
            inflight: Mutex::new(0),
            stats_gets: AtomicU64::new(0),
            stats_puts: AtomicU64::new(0),
            stats_heads: AtomicU64::new(0),
            stats_bytes_get: AtomicU64::new(0),
            stats_bytes_put: AtomicU64::new(0),
        }
    }

    fn gate(&self, spec: &LatencySpec) -> InFlightGuard<'_> {
        while *self.inflight.lock() >= self.max_concurrency as u64 {
            std::thread::sleep(Duration::from_micros(200));
        }
        *self.inflight.lock() += 1;
        if spec.mean_ms > 0.0 {
            let jitter = 1.0 + rand::rng().random_range(-spec.jitter_pct..spec.jitter_pct);
            let ms = (spec.mean_ms * jitter).max(0.0);
            if ms > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(ms / 1000.0));
            }
        }
        InFlightGuard { store: self }
    }
}

struct InFlightGuard<'a> {
    store: &'a ThrottledObjStore,
}
impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        *self.store.inflight.lock() -= 1;
    }
}

/// 并发受限下的模拟：并发数 ≤ max_concurrency，每次操作延迟独立采样。
impl ObjStore for ThrottledObjStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        let _g = self.gate(&self.get_lat);
        self.stats_gets.fetch_add(1, Ordering::Relaxed);
        let t = Instant::now();
        let r = self.inner.get(path);
        self.stats_bytes_get.fetch_add(
            r.as_ref().map(|b| b.len()).unwrap_or(0) as u64,
            Ordering::Relaxed,
        );
        let _ = t.elapsed();
        r
    }
    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        let _g = self.gate(&self.get_lat);
        self.stats_gets.fetch_add(1, Ordering::Relaxed);
        let r = self.inner.get_range(path, off, len);
        self.stats_bytes_get.fetch_add(
            r.as_ref().map(|b| b.len()).unwrap_or(0) as u64,
            Ordering::Relaxed,
        );
        r
    }
    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        let _g = self.gate(&self.put_lat);
        self.stats_puts.fetch_add(1, Ordering::Relaxed);
        self.stats_bytes_put
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.inner.put(path, data)
    }
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        let _g = self.gate(&self.put_lat);
        self.stats_puts.fetch_add(1, Ordering::Relaxed);
        self.stats_bytes_put
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.inner.put_if_absent(path, data)
    }
    fn delete(&self, path: &str) -> ObjResult<()> {
        let _g = self.gate(&self.get_lat);
        self.inner.delete(path)
    }
    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>> {
        let _g = self.gate(&self.head_lat);
        self.stats_heads.fetch_add(1, Ordering::Relaxed);
        self.inner.head(path)
    }
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        let _g = self.gate(&self.get_lat);
        self.inner.list_prefix(prefix)
    }
    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        let _g = self.gate(&self.get_lat);
        self.inner.copy(from, to)
    }
    fn append(&self, path: &str, data: &[u8]) -> ObjResult<()> {
        // 追加按单次写计延迟（WAL 段 = 写密集路径，注入即代表性采样）
        let _g = self.gate(&self.put_lat);
        self.inner.append(path, data)
    }
    fn supports_append(&self) -> bool {
        self.inner.supports_append()
    }
}
