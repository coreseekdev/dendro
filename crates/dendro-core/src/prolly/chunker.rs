//! chunker：树的构建与增量应用（SPEC 03 §2.3）。
//!
//! v1 工程决策（spec/03 §2.3 注）：批量建树用 weibull 分裂器切节点（与 prolly
//! 布局一致）；增量更新走**路径复制 + 溢出按字节中点重分裂**（CoW B-tree）。
//! 内容寻址/结构共享/diff/merge 语义全部保留；点更新写放大 = O(树高)。
//! 完整"边界重切分 + 实时对齐"的 prolly 增量算法由 prototype/prolly 验证，v2 切换。

use super::node::{EntryVal, Node, ADDR_LEN};
use super::splitter::KeySplitter;
use super::NodeStore;
use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub enum Mutation {
    Put(Vec<u8>),
    Delete,
}

/// 每 entry 的近似字节成本（分列统计与分裂依据）
fn entry_bytes(k: &[u8], v: &EntryVal) -> usize {
    k.len()
        + 12
        + match v {
            EntryVal::Item(b) => b.len(),
            EntryVal::Child(_, _) => ADDR_LEN + 8,
        }
}

/// 树的条目总数（内部节点用累计计数）
fn count_of(v: &EntryVal) -> u64 {
    match v {
        EntryVal::Item(_) => 1,
        EntryVal::Child(_, c) => *c,
    }
}

pub struct Chunker<'a> {
    pub store: &'a NodeStore,
    pub session: &'a mut HashSet<Hash>,
    pub dirty: usize,
}

impl<'a> Chunker<'a> {
    pub fn new(store: &'a NodeStore, session: &'a mut HashSet<Hash>) -> Self {
        Self {
            store,
            session,
            dirty: 0,
        }
    }

    fn put(&mut self, level: u8, entries: Vec<(Vec<u8>, EntryVal)>) -> Result<Node> {
        let node = Node::build(level, &entries);
        self.store.put_node(node.clone(), self.session)?;
        self.dirty += 1;
        Ok(node)
    }

    /// 把有序 entries 按 splitter 切成节点，返回父层条目
    fn chunk_level(
        &mut self,
        level: u8,
        entries: Vec<(Vec<u8>, EntryVal)>,
    ) -> Result<Vec<(Vec<u8>, EntryVal)>> {
        let splitter = KeySplitter::new(level);
        let mut out = Vec::new();
        let mut buf: Vec<(Vec<u8>, EntryVal)> = Vec::new();
        let mut bytes = 0usize;
        for (k, v) in entries {
            let prev = bytes;
            bytes += entry_bytes(&k, &v);
            buf.push((k.clone(), v));
            if splitter.crossed_boundary(prev, bytes, &k) {
                let count: u64 = buf.iter().map(|(_, v)| count_of(v)).sum();
                let last = buf.last().unwrap().0.clone();
                let node = self.put(level, std::mem::take(&mut buf))?;
                out.push((last, EntryVal::Child(node.addr(), count)));
                bytes = 0;
            }
        }
        if !buf.is_empty() {
            let count: u64 = buf.iter().map(|(_, v)| count_of(v)).sum();
            let last = buf.last().unwrap().0.clone();
            let node = self.put(level, buf)?;
            out.push((last, EntryVal::Child(node.addr(), count)));
        }
        Ok(out)
    }

    /// 全量构建（items 必须按 key 有序、无重复）
    pub fn build(&mut self, items: &[(Vec<u8>, Vec<u8>)]) -> Result<Option<Hash>> {
        if items.is_empty() {
            return Ok(None);
        }
        let mut level_entries: Vec<(Vec<u8>, EntryVal)> = items
            .iter()
            .map(|(k, v)| (k.clone(), EntryVal::Item(v.clone())))
            .collect();
        let mut level = 0u8;
        loop {
            let parent = self.chunk_level(level, std::mem::take(&mut level_entries))?;
            if parent.len() == 1 {
                // 根：唯一节点；若其本身是内部节点则已是根
                let (_, v) = &parent[0];
                let addr = match v {
                    EntryVal::Child(a, _) => *a,
                    _ => unreachable!(),
                };
                return Ok(Some(addr));
            }
            level += 1;
            level_entries = parent;
        }
    }

    /// 增量应用变更（muts 任意顺序；同 key 取最后一条）。
    /// 返回新根；空树返回 None。
    pub fn apply(
        &mut self,
        root: Option<&Hash>,
        muts: &[(Vec<u8>, Mutation)],
    ) -> Result<Option<Hash>> {
        if muts.is_empty() {
            return Ok(root.copied());
        }
        // 防御性排序 + 同 key 去重（保最后）
        let mut sorted: Vec<(Vec<u8>, Mutation)> = muts.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        sorted.reverse();
        sorted.dedup_by(|a, b| a.0 == b.0);
        sorted.reverse();
        let muts = &sorted[..];
        match root {
            None => {
                // 空树：直接把 Put 条目建树
                let items: Vec<(Vec<u8>, Vec<u8>)> = muts
                    .iter()
                    .filter_map(|(k, m)| match m {
                        Mutation::Put(v) => Some((k.clone(), v.clone())),
                        Mutation::Delete => None,
                    })
                    .collect();
                self.build(&items)
            }
            Some(r) => {
                let level = self.store.get_node(r)?.level();
                let nodes = self.apply_rec(r, level, muts)?;
                match nodes.len() {
                    0 => Ok(None),
                    1 => {
                        let mut node = nodes.into_iter().next().unwrap();
                        // 根收缩：内部根只有单个内部子 → 子上移
                        while node.level() > 0 && node.count() == 1 {
                            let child = match node.value(0) {
                                EntryVal::Child(a, _) => a,
                                _ => break,
                            };
                            let cn = self.store.get_node(&child)?;
                            if cn.level() + 1 == node.level() {
                                node = cn;
                            } else {
                                break;
                            }
                        }
                        Ok(Some(node.addr()))
                    }
                    _ => {
                        // 根分裂：新根节点
                        let level = nodes[0].level() + 1;
                        let entries: Vec<(Vec<u8>, EntryVal)> = nodes
                            .iter()
                            .map(|n| {
                                let count = subtree_count_of(n);
                                (n.last_key(), EntryVal::Child(n.addr(), count))
                            })
                            .collect();
                        let root = self.put(level, entries)?;
                        Ok(Some(root.addr()))
                    }
                }
            }
        }
    }

    /// 递归：应用变更到一个子树，返回替换该子树的 0/1/N 个新节点（N=分裂）。
    /// 未触及子树原地址复用（结构共享）。
    fn apply_rec(
        &mut self,
        addr: &Hash,
        level: u8,
        muts: &[(Vec<u8>, Mutation)],
    ) -> Result<Vec<Node>> {
        let node = self.store.get_node(addr)?;
        debug_assert_eq!(node.level(), level);
        if level == 0 {
            // 叶：merge 条目
            let merged = merge_leaf(&node, muts)?;
            // 按 splitter 重新分块（通常仍为 1 节点；大写入/溢出为多节点）
            let out = self.chunk_level(0, merged)?;
            let nodes = self.materialize_entries(0, &out)?;
            return Ok(nodes);
        }
        // 内部：按子树 key 区间划分 muts。
        // 子树 i 覆盖 (child[i-1].key, child[i].key]；尾部 muts（key > 末子树 max）
        // 归入最后一个子树（其范围内追加新 key）。
        let n_children = node.count();
        let mut bounds = Vec::with_capacity(n_children);
        {
            let mut mi = 0usize;
            for i in 0..n_children {
                let ck = node.key(i);
                while mi < muts.len() && muts[mi].0.as_slice() <= ck.as_slice() {
                    mi += 1;
                }
                bounds.push(mi);
            }
        }
        let mut new_entries: Vec<(Vec<u8>, EntryVal)> = Vec::with_capacity(n_children + 2);
        for i in 0..n_children {
            let ck = node.key(i);
            let (child_addr, child_count) = match node.value(i) {
                EntryVal::Child(a, c) => (a, c),
                _ => return Err(SqlError::internal("item at internal node")),
            };
            let start = if i == 0 { 0 } else { bounds[i - 1] };
            let end = if i == n_children - 1 {
                muts.len()
            } else {
                bounds[i]
            };
            let sub = &muts[start..end];
            if sub.is_empty() {
                new_entries.push((ck, EntryVal::Child(child_addr, child_count)));
            } else {
                let repl = self.apply_rec(&child_addr, level - 1, sub)?;
                // 替换森林按序并入（0 个 = 子树清空，条目丢弃）
                for n in repl {
                    let count = subtree_count_of(&n);
                    new_entries.push((n.last_key(), EntryVal::Child(n.addr(), count)));
                }
            }
        }
        if new_entries.is_empty() {
            return Ok(vec![]);
        }
        // 重建本层节点（若超尺寸则分裂）
        let out = self.chunk_level(level, new_entries)?;
        let nodes = self.materialize_entries(level, &out)?;
        Ok(nodes)
    }

    /// 把父层条目 (…, Child(addr,…)) 反查为节点列表
    fn materialize_entries(
        &mut self,
        _level: u8,
        parent_entries: &[(Vec<u8>, EntryVal)],
    ) -> Result<Vec<Node>> {
        let mut nodes = Vec::with_capacity(parent_entries.len());
        for (_, v) in parent_entries {
            if let EntryVal::Child(a, _) = v {
                nodes.push(self.store.get_node(a)?);
            }
        }
        Ok(nodes)
    }
}

fn subtree_count_of(n: &Node) -> u64 {
    if n.level() == 0 {
        return n.count() as u64;
    }
    let mut t = 0u64;
    for i in 0..n.count() {
        if let EntryVal::Child(_, c) = n.value(i) {
            t += c;
        }
    }
    t
}

/// 叶条目与变更的有序归并
fn merge_leaf(node: &Node, muts: &[(Vec<u8>, Mutation)]) -> Result<Vec<(Vec<u8>, EntryVal)>> {
    let mut out = Vec::with_capacity(node.count() + muts.len());
    let mut ei = 0usize;
    let mut mi = 0usize;
    while ei < node.count() || mi < muts.len() {
        if mi >= muts.len()
            || (ei < node.count() && node.key(ei).as_slice() < muts[mi].0.as_slice())
        {
            out.push((node.key(ei), node.value(ei)));
            ei += 1;
        } else if ei >= node.count() || node.key(ei).as_slice() > muts[mi].0.as_slice() {
            match &muts[mi].1 {
                Mutation::Put(v) => out.push((muts[mi].0.clone(), EntryVal::Item(v.clone()))),
                Mutation::Delete => {}
            }
            mi += 1;
        } else {
            // 同 key：变更覆盖
            match &muts[mi].1 {
                Mutation::Put(v) => out.push((muts[mi].0.clone(), EntryVal::Item(v.clone()))),
                Mutation::Delete => {}
            }
            ei += 1;
            mi += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objstore::cas::CasStore;
    use crate::objstore::memory::MemoryObjStore;
    use crate::prolly::cursor;
    use std::sync::Arc;

    fn store() -> Arc<NodeStore> {
        let mem: Arc<dyn crate::objstore::ObjStore> = Arc::new(MemoryObjStore::new());
        Arc::new(NodeStore::new(Arc::new(CasStore::new(mem)), 1024))
    }

    fn items(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n)
            .map(|i| {
                (
                    format!("k{:08}", i).into_bytes(),
                    format!("v{i}").into_bytes(),
                )
            })
            .collect()
    }

    #[test]
    fn build_lookup_scan() {
        let s = store();
        let mut sess = HashSet::new();
        let mut ck = Chunker::new(&s, &mut sess);
        let root = ck.build(&items(5000)).unwrap().unwrap();
        assert_eq!(cursor::tree_count(&s, &root).unwrap(), 5000);
        let v = cursor::lookup(&s, &root, b"k00000042").unwrap().unwrap();
        assert_eq!(v, b"v42");
        assert!(cursor::lookup(&s, &root, b"nope").unwrap().is_none());
        let rng =
            cursor::range_scan(s.clone(), &root, Some(b"k00000100"), Some(b"k00000200")).unwrap();
        assert_eq!(rng.len(), 100);
        assert_eq!(rng[0].0, b"k00000100".to_vec());
        // 树高合理（5000 条目应为 2~3 层）
        let h = s.get_node(&root).unwrap().level();
        assert!((1..=3).contains(&h), "height {h}");
    }

    #[test]
    fn apply_put_delete_structural_sharing() {
        let s = store();
        let mut sess = HashSet::new();
        let mut ck = Chunker::new(&s, &mut sess);
        let root = ck.build(&items(3000)).unwrap().unwrap();

        // 单点更新
        let muts = vec![
            (b"k00000100".to_vec(), Mutation::Put(b"updated".to_vec())),
            (b"k00009999".to_vec(), Mutation::Put(b"new".to_vec())),
            (b"k00000005".to_vec(), Mutation::Delete),
        ];
        let mut ck2 = Chunker::new(&s, &mut sess);
        let root2 = ck2.apply(Some(&root), &muts).unwrap().unwrap();
        assert_ne!(root, root2);
        // 结构共享：新写 chunk 数应远小于总节点数
        assert!(ck2.dirty < 40, "dirty {} too big", ck2.dirty);
        // 语义正确
        assert_eq!(
            cursor::lookup(&s, &root2, b"k00000100").unwrap().unwrap(),
            b"updated"
        );
        assert_eq!(
            cursor::lookup(&s, &root2, b"k00009999").unwrap().unwrap(),
            b"new"
        );
        assert!(cursor::lookup(&s, &root2, b"k00000005").unwrap().is_none());
        assert_eq!(cursor::tree_count(&s, &root2).unwrap(), 3000);
        // 原树不变（不可变）
        assert_eq!(
            cursor::lookup(&s, &root, b"k00000100").unwrap().unwrap(),
            b"v100"
        );
        // 旧树仍可读
        assert_eq!(cursor::tree_count(&s, &root).unwrap(), 3000);
    }

    #[test]
    fn delete_to_empty_and_rebuild() {
        let s = store();
        let mut sess = HashSet::new();
        let mut ck = Chunker::new(&s, &mut sess);
        let root = ck.build(&items(50)).unwrap().unwrap();
        let dels: Vec<(Vec<u8>, Mutation)> = (0..50)
            .map(|i| (format!("k{i:08}").into_bytes(), Mutation::Delete))
            .collect();
        let mut ck2 = Chunker::new(&s, &mut sess);
        let root2 = ck2.apply(Some(&root), &dels).unwrap();
        assert!(root2.is_none(), "全删后应为空树");
        // 重建
        let mut ck3 = Chunker::new(&s, &mut sess);
        let root3 = ck3
            .apply(
                root2.as_ref(),
                &[(b"a".to_vec(), Mutation::Put(b"1".to_vec()))],
            )
            .unwrap()
            .unwrap();
        assert_eq!(cursor::tree_count(&s, &root3).unwrap(), 1);
    }

    #[test]
    fn bulk_insert_on_top() {
        let s = store();
        let mut sess = HashSet::new();
        let mut ck = Chunker::new(&s, &mut sess);
        let root = ck.build(&items(100)).unwrap().unwrap();
        // 追加 5000 个 key（有序追加场景）
        let app: Vec<(Vec<u8>, Mutation)> = (100..5100)
            .map(|i| {
                (
                    format!("k{i:08}").into_bytes(),
                    Mutation::Put(format!("v{i}").into_bytes()),
                )
            })
            .collect();
        let mut ck2 = Chunker::new(&s, &mut sess);
        let root2 = ck2.apply(Some(&root), &app).unwrap().unwrap();
        assert_eq!(cursor::tree_count(&s, &root2).unwrap(), 5100);
        assert_eq!(
            cursor::lookup(&s, &root2, b"k00005099").unwrap().unwrap(),
            b"v5099"
        );
        assert_eq!(
            cursor::lookup(&s, &root2, b"k00000099").unwrap().unwrap(),
            b"v99"
        );
    }
}
