//! NodeStore：节点(chunk)读写 + 读缓存。不可变节点以 Arc 共享，无锁读。
#![allow(clippy::type_complexity)]

use super::node::{validate, Node};
use crate::error::Result;
use crate::format::hash::Hash;
use crate::objstore::cas::{CasStore, Chunk, ChunkType};
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::sync::Arc;

/// 读缓存分片数（16：并发点查的锁竞争摊薄；每分片独立 LRU）
const SHARDS: usize = 16;

pub struct NodeStore {
    cas: Arc<CasStore>,
    /// 分片真 LRU（P2-6c）：此前是"满即冻结"的 HashMap——cache_insert 在
    /// 满 + miss 时直接跳过插入，淘汰分支永远不可达 ⇒ 超过 cache_cap 个
    /// 节点的表，点查每次都打存储层（30 万行表实测 34k q/s vs 热表 189k）。
    shards: Vec<Mutex<lru::LruCache<Hash, Node>>>,
}

impl NodeStore {
    pub fn new(cas: Arc<CasStore>, cache_cap: usize) -> Self {
        let per_shard = (cache_cap / SHARDS).max(64);
        Self {
            cas,
            shards: (0..SHARDS)
                .map(|_| Mutex::new(lru::LruCache::new(NonZeroUsize::new(per_shard).unwrap())))
                .collect(),
        }
    }

    pub fn cas(&self) -> &Arc<CasStore> {
        &self.cas
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

    /// 读取节点（分片 LRU 缓存；命中晋升，未命中回填并淘汰最久未用）。
    /// 未命中路径不再重算节点地址（SHA-512）：地址即查询键，内容寻址的
    /// 完整性由 crc32c（validate）+ 写入端哈希保证——每次 miss 省一次
    /// 全节点 SHA-512（4KB ≈ 1-2µs）。
    pub fn get_node(&self, h: &Hash) -> Result<Node> {
        let shard = self.shard(h);
        let mut g = shard.lock();
        if let Some(n) = g.get(h) {
            return Ok(n.clone());
        }
        drop(g);
        let (ty, data) = self.cas.get(h)?;
        debug_assert_eq!(ty, ChunkType::Node);
        validate(&data)?;
        let node = Node::from_arc_with_addr(Arc::new(data.to_vec()), *h);
        self.shard(h).lock().put(*h, node.clone());
        Ok(node)
    }

    fn shard(&self, h: &Hash) -> &Mutex<lru::LruCache<Hash, Node>> {
        &self.shards[h.as_u64() as usize % SHARDS]
    }

    fn cache_insert(&self, h: Hash, n: Node) {
        self.shard(&h).lock().put(h, n);
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
