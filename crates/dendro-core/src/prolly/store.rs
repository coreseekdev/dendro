//! NodeStore：节点(chunk)读写 + 读缓存。不可变节点以 Arc 共享，无锁读。
#![allow(clippy::type_complexity)]

use super::node::{validate, Node};
use crate::error::Result;
use crate::format::hash::Hash;
use crate::objstore::cas::{CasStore, Chunk, ChunkType};
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// 读缓存分片数（16：并发点查的锁竞争摊薄；每分片独立 LRU）
const SHARDS: usize = 16;

/// 分片 LRU + 字节预算账本（opt1-prolly-tp-base：节点缓存从"条数上限"
/// 升级为**字节预算**——`DbOptions::cache_budget_bytes` 的真接线；条数
/// 上限的历史问题：4096 节点 × 4KB ≈ 16MB 恒定，预算旋钮形同虚设）。
struct Shard {
    lru: lru::LruCache<Hash, Node>,
    /// 当前驻留字节（= Σ node.data().len()；Node 为 Arc 共享，同一节点
    /// 多处引用只计一次——LRU 键即内容地址，天然去重）
    bytes: usize,
    /// 字节预算（每分片 = 总预算/SHARDS；0 = 无限——旧行为兼容）
    budget: usize,
}

/// 进程级 NodeStore 弱引用注册表（census 聚合——命中率/驻留）
static STORES: Mutex<Vec<std::sync::Weak<NodeStore>>> = Mutex::new(Vec::new());

fn register_store(s: &Arc<NodeStore>) {
    let mut g = STORES.lock();
    g.retain(|w| w.upgrade().is_some());
    g.push(Arc::downgrade(s));
}

/// 全进程节点缓存统计聚合：(hits, misses, resident_bytes)
pub fn global_cache_stats() -> (u64, u64, u64) {
    let mut g = STORES.lock();
    g.retain(|w| w.upgrade().is_some());
    g.iter()
        .filter_map(|w| w.upgrade())
        .map(|s| s.cache_stats())
        .fold((0, 0, 0), |(h, m, r), (a, b, c)| (h + a, m + b, r + c))
}

pub struct NodeStore {
    cas: Arc<CasStore>,
    shards: Vec<Mutex<Shard>>,
    /// census：命中/未命中计数（memprof 观测——TP 页缓存命中率）
    hits: AtomicU64,
    misses: AtomicU64,
    /// census：当前缓存驻留字节
    resident: AtomicU64,
}

impl NodeStore {
    /// 条数上限构造（兼容旧调用；cursor.rs 传 1 = 纯透传）
    pub fn new(cas: Arc<CasStore>, cache_cap: usize) -> Self {
        Self::with_byte_budget(cas, 0, cache_cap)
    }

    /// 字节预算构造（0 = 无限；entry_cap 为防御性条数上限——防超小
    /// 节点场景账本精度损失撑爆条数）
    pub fn with_byte_budget(cas: Arc<CasStore>, budget_bytes: usize, entry_cap: usize) -> Self {
        let per_shard_cap = (entry_cap / SHARDS).max(64);
        let per_shard_budget = budget_bytes / SHARDS;
        let out = Self {
            cas,
            shards: (0..SHARDS)
                .map(|_| {
                    Mutex::new(Shard {
                        lru: lru::LruCache::new(
                            NonZeroUsize::new(per_shard_cap.max(per_shard_budget / 4096)).unwrap(),
                        ),
                        bytes: 0,
                        budget: per_shard_budget,
                    })
                })
                .collect(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            resident: AtomicU64::new(0),
        };
        // 无法在此拿 Arc（构造中）——engine 侧包装后注册
        out
    }

    /// 进程级 census 注册（engine 构造 Arc 后调用）
    pub fn register_for_census(self: &Arc<Self>) {
        register_store(self);
    }

    pub fn cas(&self) -> &Arc<CasStore> {
        &self.cas
    }

    /// 缓存命中/未命中（memprof census 口径）
    pub fn cache_stats(&self) -> (u64, u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.resident.load(Ordering::Relaxed),
        )
    }

    pub fn put_node(
        &self,
        node: Node,
        session_cache: &mut std::collections::HashSet<Hash>,
    ) -> Result<Hash> {
        let h = node.addr();
        if !session_cache.contains(&h) {
            let ty = ChunkType::Node;
            let chunk = Chunk {
                ty,
                data: node.data().to_vec(),
            };
            debug_assert_eq!(chunk.addr(), h);
            self.cas.put_batch(&[chunk], session_cache)?;
        }
        self.cache_insert(h, node);
        Ok(h)
    }

    /// 读取节点（分片 LRU 缓存；命中晋升，未命中回填并按字节预算淘汰）。
    /// 未命中路径不再重算节点地址（SHA-512）：地址即查询键，内容寻址的
    /// 完整性由 crc32c（validate）+ 写入端哈希保证——每次 miss 省一次
    /// 全节点 SHA-512（4KB ≈ 1-2µs）。
    pub fn get_node(&self, h: &Hash) -> Result<Node> {
        let shard = self.shard(h);
        {
            let mut g = shard.lock();
            if let Some(n) = g.lru.get(h) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(n.clone());
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let (ty, data) = self.cas.get(h)?;
        debug_assert_eq!(ty, ChunkType::Node);
        validate(&data)?;
        let node = Node::from_arc_with_addr(Arc::new(data.to_vec()), *h);
        self.cache_insert(*h, node.clone());
        Ok(node)
    }

    fn shard(&self, h: &Hash) -> &Mutex<Shard> {
        &self.shards[h.as_u64() as usize % SHARDS]
    }

    fn cache_insert(&self, h: Hash, n: Node) {
        let shard = self.shard(&h);
        let mut g = shard.lock();
        let sz = n.data().len();
        if g.lru.contains(&h) {
            return; // Arc 共享——已驻留不重复计账
        }
        g.lru.put(h, n);
        g.bytes += sz;
        self.resident.fetch_add(sz as u64, Ordering::Relaxed);
        // 预算淘汰：最久未用先行（0 预算 = 无限，不淘汰）
        if g.budget > 0 {
            while g.bytes > g.budget {
                match g.lru.pop_lru() {
                    Some((_, ev)) => {
                        g.bytes -= ev.data().len();
                        self.resident
                            .fetch_sub(ev.data().len() as u64, Ordering::Relaxed);
                    }
                    None => break,
                }
            }
        }
    }

    /// 树根是否有效（空树 = None）
    pub fn validate_root(&self, root: &Option<String>) -> Result<bool> {
        match root {
            None => Ok(true),
            Some(s) => {
                let h = Hash::from_base32(s)
                    .ok_or_else(|| crate::error::SqlError::internal("bad root hash"))?;
                Ok(self.cas.has(&h))
            }
        }
    }
}
