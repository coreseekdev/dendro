//! 三方合并（SPEC 03 §6）：基于 prolly/CoW 树的 chunk 级 diff，
//! 行级冲突检测（divergent modify / delete-vs-modify）。

use super::commit::Commit;
use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::objstore::cas::{Chunk, ChunkType};
use crate::prolly::chunker::{Chunker, Mutation};
use crate::prolly::diff::diff;
use crate::prolly::NodeStore;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct MergeConflict {
    pub key: String,
    pub base: Option<Vec<u8>>,
    pub left: Option<Vec<u8>>,
    pub right: Option<Vec<u8>>,
}

#[derive(Debug)]
pub enum MergeOutcome {
    /// 已是最新（right 无变更）
    NoOp,
    /// 快进：新根 = right
    FastForward(Hash),
    /// 合并产生新根 + merge commit 地址
    Merged { root: Hash, commit: Hash },
    /// 冲突清单
    Conflicts(Vec<MergeConflict>),
}

/// 三方合并一棵 prolly/CoW map。
/// base/left/right: 各树根（None=空树）。
pub fn merge_map(
    store: &Arc<NodeStore>,
    base: Option<Hash>,
    left: Option<Hash>,
    right: Option<Hash>,
    session: &mut HashSet<Hash>,
) -> Result<MergeOutcome> {
    if left == right {
        return Ok(MergeOutcome::NoOp);
    }
    if base == right {
        return Ok(MergeOutcome::FastForward(left.unwrap()));
    }
    if base == left {
        // right 已领先：快进到 right（对 left 视角）
        return Ok(MergeOutcome::FastForward(right.unwrap()));
    }
    let pl = diff(store.clone(), base.as_ref(), left.as_ref())?;
    let pr = diff(store.clone(), base.as_ref(), right.as_ref())?;

    // 归并两路 patch
    let mut merged: Vec<(Vec<u8>, Mutation)> = Vec::new();
    let mut conflicts: Vec<MergeConflict> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < pl.len() || j < pr.len() {
        let take_left = if j >= pr.len() {
            true
        } else if i >= pl.len() {
            false
        } else {
            pl[i].key <= pr[j].key
        };
        let (l, r) = if take_left {
            let l = Some(&pl[i]);
            let r = if j < pr.len() && pl[i].key == pr[j].key {
                let r = Some(&pr[j]);
                j += 1;
                i += 1;
                r
            } else {
                i += 1;
                None
            };
            (l, r)
        } else {
            let r = Some(&pr[j]);
            j += 1;
            (None, r)
        };
        let key = l.or(r).unwrap().key.clone();
        match (l, r) {
            (Some(l), None) => push_change(&mut merged, &key, &l.new),
            (None, Some(r)) => push_change(&mut merged, &key, &r.new),
            (Some(l), Some(r)) => {
                if l.new == r.new {
                    push_change(&mut merged, &key, &l.new); // 收敛
                } else {
                    conflicts.push(MergeConflict {
                        key: String::from_utf8_lossy(&key).to_string(),
                        base: l.old.clone(),
                        left: l.new.clone(),
                        right: r.new.clone(),
                    });
                }
            }
            (None, None) => unreachable!(),
        }
    }
    if !conflicts.is_empty() {
        return Ok(MergeOutcome::Conflicts(conflicts));
    }
    // 基于 left 应用合并变更
    let mut ck = Chunker::new(store, session);
    let new_root = ck.apply(left.as_ref(), &merged)?;
    Ok(MergeOutcome::Merged { root: new_root.unwrap(), commit: Hash::from_bytes([0u8; 20]) })
}

fn push_change(out: &mut Vec<(Vec<u8>, Mutation)>, key: &[u8], new: &Option<Vec<u8>>) {
    let m = match new {
        Some(v) => Mutation::Put(v.clone()),
        None => Mutation::Delete,
    };
    out.push((key.to_vec(), m));
}

/// 写 merge commit（两父），需要调用方先拿 merge_map 的 root
pub fn write_merge_commit(
    store: &Arc<NodeStore>,
    root: Hash,
    left_commit: &Commit,
    right_commit: &Commit,
    branch: &str,
    author: &str,
    now_ms: i64,
    session: &mut HashSet<Hash>,
) -> Result<Hash> {
    let c = Commit {
        root,
        parents: vec![left_commit.addr(), right_commit.addr()],
        height: left_commit.height.max(right_commit.height) + 1,
        ts_ms: now_ms,
        branch: branch.to_string(),
        author: author.to_string(),
        message: format!("merge {} into {}", right_commit.branch, branch),
    };
    let chunk = c.encode();
    store
        .cas()
        .put_batch(&[Chunk { ty: ChunkType::Commit, data: chunk.data.clone() }], session)
        .map_err(|e| SqlError::from(e))?;
    Ok(chunk.addr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objstore::memory::MemoryObjStore;
    use crate::objstore::cas::CasStore;
    use crate::prolly::cursor;

    fn store() -> Arc<NodeStore> {
        let mem: Arc<dyn crate::objstore::ObjStore> = Arc::new(MemoryObjStore::new());
        Arc::new(NodeStore::new(Arc::new(CasStore::new(mem)), 1024))
    }

    fn build(store: &Arc<NodeStore>, session: &mut HashSet<Hash>, kvs: Vec<(Vec<u8>, Vec<u8>)>) -> Option<Hash> {
        let mut ck = Chunker::new(store, session);
        ck.build(&kvs).unwrap()
    }

    fn k(i: u32) -> Vec<u8> {
        format!("k{i:04}").into_bytes()
    }

    #[test]
    fn disjoint_merge_and_conflicts() {
        let s = store();
        let mut sess = HashSet::new();
        // base: k0..k99
        let base = build(&s, &mut sess, (0..100).map(|i| (k(i), format!("v{i}").into_bytes())).collect());
        // left: 改 k10，删 k20，加 k200
        let left = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(
                base.as_ref(),
                &[
                    (k(10), Mutation::Put(b"left10".to_vec())),
                    (k(20), Mutation::Delete),
                    (k(200), Mutation::Put(b"left200".to_vec())),
                ],
            )
            .unwrap()
        };
        // right: 改 k30，改 k40，加 k300
        let right = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(
                base.as_ref(),
                &[
                    (k(30), Mutation::Put(b"right30".to_vec())),
                    (k(40), Mutation::Put(b"right40".to_vec())),
                    (k(300), Mutation::Put(b"right300".to_vec())),
                ],
            )
            .unwrap()
        };
        // 无冲突合并
        let out = merge_map(&s, base, left, right, &mut sess).unwrap();
        match out {
            MergeOutcome::Merged { root, .. } => {
                assert_eq!(cursor::lookup(&s, &root, &k(10)).unwrap().unwrap(), b"left10");
                assert!(cursor::lookup(&s, &root, &k(20)).unwrap().is_none());
                assert_eq!(cursor::lookup(&s, &root, &k(30)).unwrap().unwrap(), b"right30");
                assert_eq!(cursor::lookup(&s, &root, &k(200)).unwrap().unwrap(), b"left200");
                assert_eq!(cursor::lookup(&s, &root, &k(300)).unwrap().unwrap(), b"right300");
                assert_eq!(cursor::tree_count(&s, &root).unwrap(), 101);
            }
            _ => panic!("expected merged"),
        }
        // 冲突：两边改同一 key 不同值
        let l2 = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(base.as_ref(), &[(k(50), Mutation::Put(b"L".to_vec()))]).unwrap()
        };
        let r2 = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(base.as_ref(), &[(k(50), Mutation::Put(b"R".to_vec()))]).unwrap()
        };
        match merge_map(&s, base, l2, r2, &mut sess).unwrap() {
            MergeOutcome::Conflicts(cs) => {
                assert_eq!(cs.len(), 1);
                assert_eq!(cs[0].key, "k0050");
                assert_eq!(cs[0].left.as_deref(), Some(b"L".as_slice()));
            }
            _ => panic!("expected conflicts"),
        }
        // 收敛：两边改同 key 同值 → 无冲突（left 额外改 k10 使两树不同）
        let l3 = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(base.as_ref(), &[(k(51), Mutation::Put(b"SAME".to_vec())), (k(10), Mutation::Put(b"only-left".to_vec()))]).unwrap()
        };
        let r3 = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(base.as_ref(), &[(k(51), Mutation::Put(b"SAME".to_vec()))]).unwrap()
        };
        match merge_map(&s, base, l3, r3, &mut sess).unwrap() {
            MergeOutcome::Merged { root, .. } => {
                assert_eq!(cursor::lookup(&s, &root, &k(51)).unwrap().unwrap(), b"SAME");
                assert_eq!(cursor::lookup(&s, &root, &k(10)).unwrap().unwrap(), b"only-left");
            }
            other => panic!("expected merged, got {other:?}"),
        }
        // 两树完全相同（同改同值且无其他差异）：内容寻址 ⇒ NoOp
        let l4 = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(base.as_ref(), &[(k(52), Mutation::Put(b"S".to_vec()))]).unwrap()
        };
        let r4 = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(base.as_ref(), &[(k(52), Mutation::Put(b"S".to_vec()))]).unwrap()
        };
        assert!(matches!(merge_map(&s, base, l4, r4, &mut sess).unwrap(), MergeOutcome::NoOp));
    }

    #[test]
    fn fast_forward_and_noop() {
        let s = store();
        let mut sess = HashSet::new();
        let base = build(&s, &mut sess, (0..10).map(|i| (k(i), b"x".to_vec())).collect());
        let right = {
            let mut ck = Chunker::new(&s, &mut sess);
            ck.apply(base.as_ref(), &[(k(99), Mutation::Put(b"y".to_vec()))]).unwrap()
        };
        match merge_map(&s, base, base, right, &mut sess).unwrap() {
            MergeOutcome::FastForward(f) => assert_eq!(f, right.unwrap()),
            _ => panic!("expected ff"),
        }
        match merge_map(&s, base, right, right, &mut sess).unwrap() {
            MergeOutcome::NoOp => {}
            _ => panic!("expected noop"),
        }
    }
}
