#![allow(clippy::all)]
//! 多实例租约接管 e2e（P1）：
//! - epoch 单调递进、接管后写入、跨 epoch 恢复的完整性
//! - fencing 运行时拒写：租约过期的写者提交被拒（40001），
//!   健康写者经 commit 路径惰性续期持续可写（engine::Branch::fence_gate）

use dendro_core::{Database, DbOptions, StoreConfig};
use std::time::{Duration, Instant};

fn opts(dir: &std::path::Path, ttl_ms: i64) -> DbOptions {
    DbOptions {
        store: StoreConfig::LocalDir(dir.to_path_buf()),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 10,
        wal_segment_bytes: 4 << 20,
        checkpoint_threshold_bytes: u64::MAX, // 关闭自动 checkpoint，控制 epoch 边界
        checkpoint_interval_s: 0,
        cache_budget_bytes: 256 << 20,
        lease_ttl_ms: ttl_ms,
    }
}

#[test]
fn multi_instance_takeover() {
    let dir = std::env::temp_dir().join(format!("dendro-mn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // ── 实例 A：建表 + 写 2 行（领取 epoch E1）──
    let a = Database::open(opts(&dir, 800)).unwrap();
    let e_a = a.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);
    {
        let mut s = a.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a1')").unwrap();
        s.exec("INSERT INTO t VALUES (2, 'a2')").unwrap();
    }
    drop(a); // 模拟崩溃（无优雅关闭）

    // ── 实例 B：A 掉线后打开 → 领取 E1+1 ──
    let t_open = Instant::now();
    let b = Database::open(opts(&dir, 800)).unwrap();
    let e_b = b.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);
    assert_eq!(e_b, e_a + 1, "接管者 epoch 应为 E+1");
    println!("[takeover] B opened in {:?}", t_open.elapsed());

    // A 世代已 ACK 的数据完整可见（跨 epoch 恢复）
    {
        let mut s = b.new_session();
        let o = s.exec("SELECT count(*) FROM t").unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(rs.text_rows()[0][0].as_deref(), Some("2"), "A 世代数据恢复");
        }
    }
    // B 写入（落在新 epoch 的 WAL 路径）
    {
        let mut s = b.new_session();
        s.exec("INSERT INTO t VALUES (3, 'b3')").unwrap();
    }
    drop(b);

    // ── 实例 C：验证 A+B 两世代数据完整 ──
    let c = Database::open(opts(&dir, 800)).unwrap();
    {
        let mut s = c.new_session();
        let o = s.exec("SELECT count(*), max(id) FROM t").unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(rs.text_rows()[0][0].as_deref(), Some("3"));
            assert_eq!(rs.text_rows()[0][1].as_deref(), Some("3"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lease_takeover_epoch_monotonic() {
    // 接管不等待旧租约过期（诚实边界）：B 打开即领取更高 epoch；
    // 旧写者的运行时约束由 fence_gate（过期拒写）承担，不在此测试。
    let dir = std::env::temp_dir().join(format!("dendro-mn2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let a = Database::open(opts(&dir, 600)).unwrap();
    let e_a = a.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);

    let b = Database::open(opts(&dir, 600)).unwrap();
    let e_b = b.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);
    assert!(e_b > e_a, "接管者 epoch 应更大");
    drop(b);
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fence_expired_writer_rejected() {
    // 运行时拒写（fence_gate）：TTL=80ms；写者暂停超过 TTL（无提交 → 无续期）
    // 后，下一次提交被拒（SQLSTATE 40001）。健康写者（持续提交）不受影响——
    // 见 fence_renew_keeps_healthy_writer_writing。
    let dir = std::env::temp_dir().join(format!("dendro-mn3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(opts(&dir, 80)).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'ok')").unwrap(); // 提交时惰性续期
    }
    // 暂停超过 TTL：租约过期，无 commit 路径触发续期
    std::thread::sleep(Duration::from_millis(150));
    let err = {
        let mut s = db.new_session();
        s.exec("INSERT INTO t VALUES (2, 'rejected')").unwrap_err()
    };
    assert_eq!(err.state, "40001", "过期租约的提交应被拒（serialization/fencing）");
    assert!(err.message.contains("fencing"), "错误信息应说明 fencing 原因：{err}");

    // 旧会话的读路径不受影响（拒写不拒读）
    {
        let mut s = db.new_session();
        let o = s.exec("SELECT count(*) FROM t").unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(rs.text_rows()[0][0].as_deref(), Some("1"), "过期写者的行未提交");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fence_renew_keeps_healthy_writer_writing() {
    // 健康写者：TTL=80ms，但每 30ms 提交一次（惰性续期跟随 commit），
    // 整个窗口 > 3×TTL 也不应出现 40001。
    let dir = std::env::temp_dir().join(format!("dendro-mn4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(opts(&dir, 80)).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    }
    for i in 0..10 {
        std::thread::sleep(Duration::from_millis(30));
        let mut s = db.new_session();
        s.exec(&format!("INSERT INTO t VALUES ({i}, 'alive')")).unwrap();
    }
    {
        let mut s = db.new_session();
        let o = s.exec("SELECT count(*) FROM t").unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(rs.text_rows()[0][0].as_deref(), Some("10"), "惰性续期下健康写者不被误拒");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
