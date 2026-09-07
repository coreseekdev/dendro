//! 读路径磁盘缓存（SPEC 01 §7）：LRU 字节配额，缓存整对象与 byte-range。
//!
//! 对真实 OSS（1–10ms RTT）这是可用性的硬前提：一次点查 = 树高 × 1 RTT，
//! 命中缓存后降为本地 NVMe 读取。写路径不经过缓存（对象存储写直传）。
//!
//! 实现：缓存文件布局 `{cache_dir}/{sha256(path64)}-{off}-{len}`，
//! 索引在内存（路径 → {off, len, size, atime}），启动时扫描目录恢复；
//! 超配额按 atime 近似 LRU 淘汰。原子写入（tmp+rename），崩溃可丢。

use super::{HeadInfo, ObjResult, ObjStore};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

struct CacheIndex {
    // cache_key → (文件路径, atime)
    entries: HashMap<u64, (PathBuf, u64)>,
    bytes: u64,
    tick: u64,
}

pub struct CachedObjStore {
    inner: Arc<dyn ObjStore>,
    dir: PathBuf,
    budget: u64,
    idx: Mutex<CacheIndex>,
    // 命中统计
    pub hits: AtomicU64,
    pub misses: AtomicU64,
}

/// 公共缓存键（S3 后端缓存目录命名用）
pub fn cache_key_public(s: &str) -> u64 {
    cache_key(s)
}

fn cache_key(path: &str) -> u64 {
    // FNV-1a 64：缓存键冲突概率可忽略（路径空间有限且内容另行校验 CRC 语义）
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

impl CachedObjStore {
    pub fn new(inner: Arc<dyn ObjStore>, dir: impl Into<PathBuf>, budget_bytes: u64) -> ObjResult<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        // 启动恢复：清点现存缓存文件（文件名 = {key:016x}-{off}-{len}）
        let mut entries = HashMap::new();
        let mut bytes = 0u64;
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let stem = name.strip_suffix(".c").unwrap_or(&name);
                if let Some(key) = stem.split('-').next().and_then(|k| u64::from_str_radix(k, 16).ok()) {
                    let sz = e.metadata().map(|m| m.len()).unwrap_or(0);
                    entries.insert(key, (e.path(), 0));
                    bytes += sz;
                } else {
                    let _ = fs::remove_file(e.path()); // 残片清理
                }
            }
        }
        Ok(Self {
            inner,
            dir,
            budget: budget_bytes,
            idx: Mutex::new(CacheIndex { entries, bytes, tick: 0 }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    fn evict_if_needed(&self, idx: &mut CacheIndex) {
        while idx.bytes > self.budget && !idx.entries.is_empty() {
            // 近似 LRU：淘汰 atime 最小
            let victim = idx
                .entries
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, (p, _))| (*k, p.clone()));
            match victim {
                Some((k, p)) => {
                    if let Ok(m) = fs::metadata(&p) {
                        idx.bytes = idx.bytes.saturating_sub(m.len());
                    }
                    let _ = fs::remove_file(&p);
                    idx.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    fn cache_file_path(&self, key: u64, off: u64, len: usize) -> PathBuf {
        self.dir.join(format!("{key:016x}-{off}-{len}.c"))
    }

    fn read_cached(&self, p: &Path, off: u64, len: usize) -> Option<Bytes> {
        let mut f = fs::File::open(p).ok()?;
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(off)).ok()?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).ok()?;
        Some(Bytes::from(buf))
    }

    fn store_cached(&self, key: u64, off: u64, data: &[u8]) -> PathBuf {
        let p = self.cache_file_path(key, off, data.len());
        let tmp = p.with_extension("tmp");
        if let Ok(mut f) = fs::File::create(&tmp) {
            if f.write_all(data).is_ok() && f.sync_all().is_ok() {
                drop(f);
                if fs::rename(&tmp, &p).is_ok() {
                    let mut idx = self.idx.lock();
                    idx.tick += 1;
                    let t = idx.tick;
                    idx.entries.insert(key, (p.clone(), t));
                    idx.bytes += data.len() as u64;
                    drop(idx);
                    self.evict_loop();
                }
            }
        }
        p
    }

    fn evict_loop(&self) {
        let mut idx = self.idx.lock();
        self.evict_if_needed(&mut idx);
    }

    fn touch(&self, key: u64) {
        let mut idx = self.idx.lock();
        idx.tick += 1;
        let t = idx.tick;
        if let Some(e) = idx.entries.get_mut(&key) {
            e.1 = t;
        }
    }
}

impl ObjStore for CachedObjStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        let key = cache_key(path);
        let cached_path = {
            let idx = self.idx.lock();
            idx.entries.get(&key).cloned().map(|(p, _)| p)
        };
        if let Some(p) = cached_path {
            let size = self.inner.head(path)?.map(|h| h.len as usize).unwrap_or(0);
            if size > 0 {
                if let Some(b) = self.read_cached(&p, 0, size) {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    self.touch(key);
                    return Ok(b);
                }
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let b = self.inner.get(path)?;
        self.store_cached(key, 0, &b);
        Ok(b)
    }

    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        let key = cache_key(&format!("{path}#{off}#{len}"));
        let cached_path = {
            let idx = self.idx.lock();
            idx.entries.get(&key).cloned().map(|(p, _)| p)
        };
        if let Some(p) = cached_path {
            if let Some(b) = self.read_cached(&p, 0, len) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                self.touch(cache_key(path));
                return Ok(b);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let b = self.inner.get_range(path, off, len)?;
        if !b.is_empty() {
            self.store_cached(key, 0, &b);
        }
        Ok(b)
    }

    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.inner.put(path, data)
    }
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.inner.put_if_absent(path, data)
    }
    fn delete(&self, path: &str) -> ObjResult<()> {
        // 内容失效：删缓存条目（对象不可变，删除仅发生在 GC）
        let key = cache_key(path);
        let p = {
            let mut idx = self.idx.lock();
            idx.entries.remove(&key).map(|(p, _)| p)
        };
        if let Some(p) = p {
            let _ = fs::remove_file(p);
        }
        self.inner.delete(path)
    }
    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>> {
        // head 走远端（便宜且需新鲜度）
        self.inner.head(path)
    }
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        self.inner.list_prefix(prefix)
    }
    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        self.inner.copy(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objstore::memory::MemoryObjStore;

    #[test]
    fn cache_hits_and_eviction() {
        let mem: Arc<dyn ObjStore> = Arc::new(MemoryObjStore::new());
        let dir = std::env::temp_dir().join(format!("dendro-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cached = CachedObjStore::new(mem.clone(), &dir, 1024).unwrap();
        mem.put("a/b", Bytes::from(vec![7u8; 100])).unwrap();
        // 第一次：miss
        let _ = cached.get("a/b").unwrap();
        assert_eq!(cached.misses.load(Ordering::Relaxed), 1);
        // 第二次：hit
        let b = cached.get("a/b").unwrap();
        assert_eq!(cached.hits.load(Ordering::Relaxed), 1);
        assert_eq!(b.len(), 100);
        // range 缓存
        let r = cached.get_range("a/b", 10, 5).unwrap();
        assert_eq!(r.len(), 5);
        assert_eq!(cached.get_range("a/b", 10, 5).unwrap().as_ref(), r.as_ref());
        // 超配额淘汰：放入 2000B（> 1024 预算）→ 旧条目被淘汰，总量有界
        mem.put("c/d", Bytes::from(vec![9u8; 2000])).unwrap();
        let _ = cached.get("c/d").unwrap();
        {
            let idx = cached.idx.lock();
            assert!(idx.bytes <= 2000 + 64, "eviction should bound bytes, got {}", idx.bytes);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
