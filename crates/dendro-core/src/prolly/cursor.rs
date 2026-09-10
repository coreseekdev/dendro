//! 树游标：有序迭代、点查、范围扫描（SPEC 03 §2.4）。
//!
//! 路径不变式：`path[0]`=根 … `path[last]`=叶；
//! 内部节点槽位 idx = 待下探子节点；叶 idx = 下一条目。
//! seek/advance 全程只持有 Arc 节点，读路径无锁。

use super::node::{EntryVal, Node};
use super::NodeStore;
use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use std::sync::Arc;

pub struct TreeIter {
    store: Arc<NodeStore>,
    path: Vec<(Node, usize)>,
    finished: bool,
}

impl TreeIter {
    pub fn new(store: Arc<NodeStore>, root: &Hash) -> Result<Self> {
        let (path, exhausted) = build_path(&store, root, None)?;
        Ok(Self {
            store,
            path,
            finished: exhausted,
        })
    }

    pub fn empty() -> Self {
        Self {
            store: empty_store(),
            path: vec![],
            finished: true,
        }
    }

    /// 定位到第一个 >= key 的条目
    pub fn seek(&mut self, key: &[u8]) -> Result<()> {
        if self.path.is_empty() {
            return Ok(());
        }
        let root = self.path[0].0.addr();
        let (path, exhausted) = build_path(&self.store, &root, Some(key))?;
        self.path = path;
        self.finished = exhausted;
        Ok(())
    }

    /// 下一条目；耗尽返回 None
    pub fn next_item(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            if self.finished {
                return Ok(None);
            }
            // 叶上取当前条目
            {
                let (leaf, idx) = self.path.last().unwrap();
                if *idx < leaf.count() {
                    let item = match leaf.value(*idx) {
                        EntryVal::Item(v) => (leaf.key(*idx), v),
                        EntryVal::Child(..) => return Err(SqlError::internal("child at leaf")),
                    };
                    self.path.last_mut().unwrap().1 += 1;
                    return Ok(Some(item));
                }
            }
            // 叶尽/越过叶尾：上卷直到找到未尽的内部节点，再下探
            loop {
                match self.path.last_mut() {
                    None => {
                        self.finished = true;
                        return Ok(None);
                    }
                    Some(top) if top.0.level() == 0 => {
                        self.path.pop();
                    }
                    Some(top) => {
                        top.1 += 1;
                        if top.1 < top.0.count() {
                            break;
                        }
                        self.path.pop();
                    }
                }
            }
            // 下探到叶
            loop {
                let (n, i) = self.path.last().unwrap();
                let child = match n.value(*i) {
                    EntryVal::Child(c, _) => c,
                    EntryVal::Item(_) => return Err(SqlError::internal("item at internal")),
                };
                let cn = self.store.get_node(&child)?;
                let lvl = cn.level();
                self.path.push((cn, 0));
                if lvl == 0 {
                    break;
                }
            }
        }
    }
}

fn empty_store() -> Arc<NodeStore> {
    // 迭代器在 path 为空时永不触碰 store；这里造一个哑实例保证类型完整
    let mem = std::sync::Arc::new(crate::objstore::memory::MemoryObjStore::new());
    let cas = Arc::new(crate::objstore::cas::CasStore::new(mem));
    Arc::new(NodeStore::new(cas, 1))
}

/// 从根构建到叶的路径；返回 (path, 是否已越过所有条目)
fn build_path(
    store: &NodeStore,
    root: &Hash,
    key_ge: Option<&[u8]>,
) -> Result<(Vec<(Node, usize)>, bool)> {
    let mut path = Vec::new();
    let mut addr = *root;
    loop {
        let node = store.get_node(&addr)?;
        let idx = match key_ge {
            None => 0,
            Some(k) => node.lower_bound(k),
        };
        if idx >= node.count() {
            // key 越过该节点全部条目：该子树耗尽（叶上=无 >= key；内部=无子可下探）
            path.push((node, idx));
            return Ok((path, true));
        }
        if node.level() == 0 {
            path.push((node, idx));
            return Ok((path, false));
        }
        let child = match node.value(idx) {
            EntryVal::Child(c, _) => c,
            EntryVal::Item(_) => return Err(SqlError::internal("item at internal")),
        };
        path.push((node, idx));
        addr = child;
    }
}

/// 点查
pub fn lookup(store: &NodeStore, root: &Hash, key: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut addr = *root;
    loop {
        let node = store.get_node(&addr)?;
        let i = node.lower_bound(key);
        if i >= node.count() {
            return Ok(None);
        }
        match node.value(i) {
            EntryVal::Item(v) => {
                if node.key_slice(i) == key {
                    return Ok(Some(v));
                }
                return Ok(None);
            }
            EntryVal::Child(child, _) => addr = child,
        }
    }
}

/// 范围扫描 [start, end)（小范围便捷版；大扫描走 TreeIter 流式）
pub fn range_scan(
    store: Arc<NodeStore>,
    root: &Hash,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut it = TreeIter::new(store, root)?;
    if let Some(s) = start {
        it.seek(s)?;
    }
    let mut out = Vec::new();
    while let Some((k, v)) = it.next_item()? {
        if let Some(e) = end {
            if k.as_slice() >= e {
                break;
            }
        }
        out.push((k, v));
    }
    Ok(out)
}

/// 子树条目总数
pub fn tree_count(store: &NodeStore, root: &Hash) -> Result<u64> {
    let node = store.get_node(root)?;
    if node.level() == 0 {
        return Ok(node.count() as u64);
    }
    let mut total = 0u64;
    for i in 0..node.count() {
        if let EntryVal::Child(_, c) = node.value(i) {
            total += c;
        }
    }
    Ok(total)
}
