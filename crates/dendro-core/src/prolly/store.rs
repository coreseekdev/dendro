//! NodeStore：节点(chunk)读写 + 读缓存。不可变节点以 Arc 共享，无锁读。

use super::node::{validate, Node};
use crate::error::Result;
use crate::format::hash::Hash;
use crate::objstore::cas::{Chunk, ChunkType, CasStore};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

pub struct NodeStore {
    cas: Arc<CasStore>,
    cache: Mutex<(HashMap<Hash, Node>, usize)>, // (map, 当前条数)
    cache_cap: usize,
}

impl NodeStore {
    pub fn new(cas: Arc<CasStore>, cache_cap: usize) -> Self {
        Self {
            cas,
            cache: Mutex::new((HashMap::new(), 0)),
            cache_cap,
        }
    }

    pub fn cas(&self) -> &Arc<CasStore> {
        &self.cas
    }

    pub fn put_node(&self, node: Node, session_cache: &mut std::collections::HashSet<Hash>) -> Result<Hash> {
        let h = node.addr();
        if !session_cache.contains(&h) {
            let ty = if node.level() == 0 { ChunkType::Node } else { ChunkType::Node };
            let chunk = Chunk { ty, data: node.data().to_vec() };
            debug_assert_eq!(chunk.addr(), h);
            self.cas.put_batch(&[chunk], session_cache)?;
        }
        self.cache_insert(h, node.clone());
        Ok(h)
    }

    /// 读取节点（带缓存）
    pub fn get_node(&self, h: &Hash) -> Result<Node> {
        if let Some(n) = self.cache.lock().0.get(h) {
            return Ok(n.clone());
        }
        let (ty, data) = self.cas.get(h)?;
        debug_assert_eq!(ty, ChunkType::Node);
        validate(&data)?;
        let node = Node::from_arc(Arc::new(data.to_vec()));
        self.cache_insert(*h, node.clone());
        Ok(node)
    }

    fn cache_insert(&self, h: Hash, n: Node) {
        let mut g = self.cache.lock();
        if g.1 < self.cache_cap || g.0.contains_key(&h) {
            if g.0.insert(h, n).is_none() {
                g.1 += 1;
            }
            if g.1 > self.cache_cap {
                // 简单淘汰：清一半（近似 LRU 的廉价替代；v2 换真 LRU）
                let keys: Vec<Hash> = g.0.keys().take(g.1 / 2).copied().collect();
                for k in keys {
                    g.0.remove(&k);
                    g.1 -= 1;
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
