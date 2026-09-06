//! 恢复（SPEC 02 §4）：manifest + WAL 探测回放，重建分支内存态。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::objstore::manifest::{BranchHead, Manifest};
use crate::objstore::ObjStore;
use std::sync::Arc;

/// 打库时校验：全部分支引用的 commit chunk 存在性（抽样 HEAD）
pub fn recover_branches(obj: &Arc<dyn ObjStore>, manifest: &Manifest) -> Result<()> {
    for (name, head) in &manifest.refs {
        if let Some(c) = &head.commit {
            let Some(h) = Hash::from_base32(c) else {
                return Err(SqlError::internal(format!("branch {name}: bad commit addr")));
            };
            let path = crate::objstore::cas::CasStore::chunk_path(&h);
            if obj.head(&path).ok().flatten().is_none() {
                return Err(SqlError::io(format!("branch {name}: commit chunk missing")));
            }
        }
    }
    Ok(())
}

/// 回放一个分支的未物化 WAL（covered_seq 之后）到内存态。
/// 由 Database::branch() 在构造分支运行态时调用。
pub(crate) fn replay_branch(
    db: &crate::engine::Database,
    b: &Arc<crate::engine::Branch>,
    head: &BranchHead,
) -> Result<()> {
    let lo = head.wal_seg;
    let tail = crate::wal::probe_tail(&db.obj, &b.name, lo.max(1));
    if tail < lo.max(1) {
        return Ok(()); // 无段
    }
    let covered = head.covered_seq;
    let mut max_seq = covered;
    let mut pending = b.pending.lock();
    let mut pending_bytes = 0usize;
    for seg in (lo.max(1))..=tail {
        let data = crate::wal::read_segment(&db.obj, &b.name, seg)?;
        let mut it = crate::wal::FrameIter::new(&data);
        while let Some(f) = it.next_frame() {
            let (ty, seq, payload) = f.map_err(SqlError::from)?;
            match ty {
                crate::wal::FrameType::Txn => {
                    if seq <= covered {
                        continue; // 已物化
                    }
                    let recs = crate::wal::decode_txn(payload)?;
                    for r in recs {
                        for (key, val) in r.ops {
                            let m = match &val {
                                Some(v) => crate::prolly::Mutation::Put(v.clone()),
                                None => crate::prolly::Mutation::Delete,
                            };
                            let vb = pending_bytes
                                + key.len()
                                + val.as_ref().map(|v| v.len()).unwrap_or(0);
                            pending.entry(r.table_id).or_default().insert(key.clone(), m);
                            b.mem
                                .table(r.table_id)
                                .install(key, seq, val.map(Arc::new));
                            pending_bytes = vb;
                        }
                    }
                    max_seq = max_seq.max(seq);
                }
                crate::wal::FrameType::Checkpoint => {
                    let ck = crate::wal::decode_checkpoint(payload)?;
                    // checkpoint 帧本身描述完整树状态；其后的 Txn 才需要叠加。
                    // manifest.covered_seq 是权威；这里兜底推进。
                    max_seq = max_seq.max(ck.seq_covered);
                }
                _ => {}
            }
        }
    }
    drop(pending);
    b.pending_bytes.store(pending_bytes as u64, std::sync::atomic::Ordering::Release);
    b.watermark.store(max_seq, std::sync::atomic::Ordering::Release);
    b.restore_seq(max_seq);
    Ok(())
}
