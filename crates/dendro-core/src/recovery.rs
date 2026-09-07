//! 恢复（SPEC 02 §4）：manifest + WAL 探测回放，重建分支内存态。
//! P1 多 epoch：按 epoch 升序回放（复合时间戳 ts = epoch<<32 | seq），
//! 高 epoch 事务自然覆盖低 epoch 陈旧写（脑裂安全）。

use crate::error::{Result, SqlError};
use crate::format::hash::Hash;
use crate::objstore::manifest::{BranchHead, Manifest};
use crate::objstore::ObjStore;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// 复合事务时间戳：ts = (epoch << 32) | (seq & 0xFFFF_FFFF)
pub fn composite_ts(epoch: u64, seq: u64) -> u64 {
    (epoch << 32) | (seq & 0xFFFF_FFFF)
}

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

/// 回放一个分支的未物化 WAL：epoch 1..=max 升序，每 epoch 内段号/seq 升序。
/// 跳过 ts ≤ covered_seq（已物化）；陈旧写（同 key 更高 ts 已存在）被抑制。
pub(crate) fn replay_branch(
    db: &crate::engine::Database,
    b: &Arc<crate::engine::Branch>,
    head: &BranchHead,
    lease_epoch: u64,
) -> Result<()> {
    // 回放所有旧 epoch（1..=本进程租约 epoch-1）；本 epoch 目录打开时为空
    let max_epoch = lease_epoch.saturating_sub(1).max(head.epoch.max(1)).max(1) - 1;
    let max_epoch = max_epoch + 1;
    let covered = head.covered_seq;
    let mut max_ts = covered;
    let mut pend = b.pending.lock();
    let mut pending_bytes = 0u64;

    for epoch in 1..=max_epoch {
        // 当前 epoch 的 WAL 前缀可能已被 GC 回收（GC 定案）：从 manifest
        // 记录的 first_seg 起探测；旧 epoch 目录可能整体已回收（探测返回 0 → 零帧）
        let lo = if epoch == head.epoch {
            head.wal_first_seg.max(1)
        } else {
            1
        };
        let tail = crate::wal::probe_tail(&db.obj, &b.name, epoch, lo);
        for seg in lo..=tail {
            let data = crate::wal::read_segment(&db.obj, &b.name, epoch, seg)?;
            let mut it = crate::wal::FrameIter::new(&data);
            while let Some(f) = it.next_frame() {
                let (ty, seq, payload) = f?;
                let ts = composite_ts(epoch, seq);
                match ty {
                    crate::wal::FrameType::Txn => {
                        if ts <= covered {
                            continue; // 已物化进树
                        }
                        let recs = crate::wal::decode_txn(payload)?;
                        for r in recs {
                            let tm = b.mem.table(r.table_id);
                            for (key, val) in r.ops {
                                let m = match &val {
                                    Some(v) => crate::prolly::Mutation::Put(v.clone()),
                                    None => crate::prolly::Mutation::Delete,
                                };
                                // 陈旧写抑制：同 key 已有更高 ts（新 epoch 写过）→ 跳过
                                if tm.latest_ts(&key).is_some_and(|t| t >= ts) {
                                    continue;
                                }
                                pend.entry(r.table_id).or_default().insert(key.clone(), m);
                                tm.install(key.clone(), ts, val.clone().map(Arc::new));
                                pending_bytes +=
                                    (key.len() + val.as_ref().map(|v| v.len()).unwrap_or(0)) as u64;
                            }
                        }
                        max_ts = max_ts.max(ts);
                    }
                    crate::wal::FrameType::Checkpoint => {
                        let ck = crate::wal::decode_checkpoint(payload)?;
                        max_ts = max_ts.max(ck.seq_covered);
                    }
                    _ => {}
                }
            }
        }
    }
    drop(pend);
    b.pending_bytes.store(pending_bytes, Ordering::Release);
    b.watermark.store(max_ts, Ordering::Release);
    b.restore_seq(max_ts);
    Ok(())
}
