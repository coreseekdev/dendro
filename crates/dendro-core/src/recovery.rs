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
                return Err(SqlError::internal(format!(
                    "branch {name}: bad commit addr"
                )));
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
            // 追加模式（P2-6e）：epoch **最后一段**的帧错误一律按撕裂写
            // 容忍（SQLite/PG 同语义）——追加+fsync 中途崩溃、或掉电时
            // ext4 数据块乱序持久化（trailer 已在而前部帧丢失）都无法与
            // 位腐区分，尾段"是否封口"不可靠。非最后一段其后的段存在而
            // 本段帧损坏 = 真实腐坏，严格报错。截断风险已文档化（SPEC 02）。
            let last = seg == tail;
            while let Some(f) = it.next_frame() {
                let (ty, seq, payload) = match f {
                    Ok(x) => x,
                    Err(e) if last => {
                        tracing::warn!(branch = %b.name, epoch, seg, error = %e,
                            "torn tail on last wal segment; replaying durable prefix");
                        break;
                    }
                    Err(e) => return Err(e),
                };
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
    crate::memprof::memtx_add(pending_bytes);
    b.watermark.store(max_ts, Ordering::Release);
    b.restore_seq(max_ts);
    // P0-2 恢复不变量断言（debug/test 构建）：已安装集 ⊆ 回放历史
    // 前缀——全部已安装版本 ts ≤ watermark（watermark = 本进程自 WAL
    // 探测到的最大 ts）。越界 = 回放遗漏 / 安装路径缺陷 / memtx 损坏，
    // 响亮失败而非静默错果
    #[cfg(any(test, debug_assertions))]
    {
        let wm = b.watermark.load(Ordering::Acquire);
        b.mem.debug_check_ts_bound(wm).map_err(SqlError::internal)?;
    }
    Ok(())
}

#[cfg(test)]
mod invariant_tests {
    use super::*;
    use crate::engine::{Database, DbOptions, StoreConfig};
    use crate::objstore::memory::MemoryObjStore;

    /// P0-2 可重建性：同一对象存储上重开新 Database——
    /// manifest + 存活 WAL 段 + prolly 提交 重建 memtx，
    /// 可见行集逐行等价（含 UPDATE 覆盖与 DELETE 墓碑）。
    /// 重开路径内联触发 P0-2 前缀性断言（debug_check_ts_bound）
    #[test]
    fn reopen_rebuilds_identical_state() {
        let obj: Arc<dyn ObjStore> = Arc::new(MemoryObjStore::new());
        {
            let db = Database::open(DbOptions {
                store: StoreConfig::Obj(obj.clone()),
                ..Default::default()
            })
            .unwrap();
            let mut s = db.new_session();
            s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)")
                .unwrap();
            s.exec("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
                .unwrap();
            s.exec("UPDATE t SET v = 25 WHERE id = 2").unwrap();
            s.exec("DELETE FROM t WHERE id = 3").unwrap();
        } // Database drop → WAL 优雅关闭 → 全帧持久
        let db2 = Database::open(DbOptions {
            store: StoreConfig::Obj(obj),
            ..Default::default()
        })
        .unwrap();
        let mut s2 = db2.new_session();
        let out = s2.exec("SELECT id, v FROM t ORDER BY id").unwrap();
        let rows = match &out[0] {
            crate::types::Output::Rows(rs) => rs
                .text_rows()
                .iter()
                .map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap()))
                .collect::<Vec<_>>(),
            _ => panic!(),
        };
        assert_eq!(
            rows,
            vec![("1".into(), "10".into()), ("2".into(), "25".into())],
            "重开必须逐行重建可见集（含 UPDATE 覆盖与 DELETE 墓碑）"
        );
    }

    /// P0-2 前缀性（负例构造面）：断言器本身——越界安装必须被抓到。
    /// 直接对 BranchMem 做越界 install 验证 debug_check_ts_bound 的
    /// 检出能力（防"断言器恒真"的假护栏）
    #[test]
    fn ts_bound_checker_catches_violation() {
        let mem = crate::memtx::BranchMem::default();
        let t = mem.table(7);
        t.install(b"k1".to_vec(), 100, Some(Arc::new(vec![1u8])));
        assert!(mem.debug_check_ts_bound(100).is_ok());
        assert!(mem.debug_check_ts_bound(99).is_err(), "越界安装必须被检出");
    }
}
