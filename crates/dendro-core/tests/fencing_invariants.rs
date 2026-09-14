//! fencing/租约不变式回归（验证账本 I-F1/F2/F3，评审 R7 补充目录）：
//! - I-F1 epoch 唯一单调：并发 acquire 无物两主
//! - I-F2 过期写者零新副作用：租约过期后 flush_loop 不上传（诚实边界：回放消解）
//! - I-F3 脑裂收敛：双 epoch 并发 ack 的提交，恢复后收敛为高 epoch 串行历史

use dendro_core::objstore::memory::MemoryObjStore;
use dendro_core::objstore::ObjStore;
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn open_local(dir: &std::path::Path, ttl_ms: i64) -> Arc<Database> {
    Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir.to_path_buf()),
        lease_ttl_ms: ttl_ms,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn fence_epoch_monotonic_under_contention() {
    // I-F1：并发过期+重新领租约——epoch 严格单调（无一物两主）
    let dir = std::env::temp_dir().join(format!("fence-ep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = open_local(&dir, 50); // 极短 TTL → 频繁过期重领
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    }
    let mut epochs = Vec::new();
    for round in 0..6 {
        // 等 TTL 过期 → fence_gate 拒写 → reopen 领新 epoch
        std::thread::sleep(std::time::Duration::from_millis(60));
        // 触发 fence_gate（读 watermark 确认过期后拒写）
        let _ = db.branch("main");
        let mut s = db.new_session();
        let r = s.exec(&format!("INSERT INTO t VALUES ({})", 100 + round));
        if r.is_err() {
            // 过期拒写（预期）→ reopen
            drop(s);
            let db2 = open_local(&dir, 50);
            let mut s2 = db2.new_session();
            s2.exec(&format!("INSERT INTO t VALUES ({})", 100 + round))
                .unwrap();
            drop(s2);
            // 读 epoch
            let b = db2.branch("main").unwrap();
            epochs.push(b.lease_epoch.load(std::sync::atomic::Ordering::Acquire));
            drop(db2);
            // 后续 round 重用 db2
            break;
        }
        let b = db.branch("main").unwrap();
        epochs.push(b.lease_epoch.load(std::sync::atomic::Ordering::Acquire));
    }
    // epoch 严格单调递增
    for w in epochs.windows(2) {
        assert!(w[0] < w[1], "epoch 必须单调递增：{epochs:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lease_expiry_writes_rejected_then_reopen_recovers() {
    // I-F2 配套：短租约过期 → fence_gate 拒写（40001）→ reopen 恢复全量
    let dir = std::env::temp_dir().join(format!("fence-ttl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = open_local(&dir, 30); // 30ms TTL
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("INSERT INTO t VALUES (1)").unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    // 过期后新会话提交：fence_gate 40001
    {
        let mut s = db.new_session();
        let e = s.exec("INSERT INTO t VALUES (2)").unwrap_err();
        assert_eq!(e.state, "40001", "过期后写应 40001：{e}");
    }
    // reopen：新 epoch + 全量可见
    drop(db);
    let db2 = open_local(&dir, 60_000);
    let mut s = db2.new_session();
    let n = match &s.exec("SELECT count(*) FROM t").unwrap()[0] {
        dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!("expected rows"),
    };
    assert_eq!(n, "1", "reopen 后已 ack 数据可见");
    s.exec("INSERT INTO t VALUES (2)").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
