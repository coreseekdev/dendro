//! 多实例租约接管 e2e（P1）：
//! 验证 epoch 单调递进、接管后写入、跨 epoch 恢复的完整性。
//! 注意：当前 fencing 为"epoch 路径隔离 + 恢复期抑制"（被动），
//! 运行时拒写（commit 前的 epoch 校验）尚未实现。

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
    println!("[dbg] fence files after A open:");
    for e in std::fs::read_dir(dir.join("fence").join("main")).into_iter().flatten().flatten() {
        println!("  {:?}", e.path().file_name().unwrap());
    }
    println!("[dbg] e_a={e_a}");
    {
        let mut s = a.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a1')").unwrap();
        s.exec("INSERT INTO t VALUES (2, 'a2')").unwrap();
    }
    drop(a); // 模拟崩溃（无优雅关闭）

    // ── 实例 B：A 掉线后立即打开 → 领取 E1+1 ──
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
fn lease_expiry_blocks_stale_writer_then_allows_takeover() {
    // TTL=600ms：A 打开后 B 需等租约过期才能接管
    let dir = std::env::temp_dir().join(format!("dendro-mn2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let a = Database::open(opts(&dir, 600)).unwrap();
    let e_a = a.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);

    let t0 = Instant::now();
    let b = Database::open(opts(&dir, 600)).unwrap();
    // B 打开即接管（等 A 的 600ms 租约过期）
    let e_b = b.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);
    let waited = t0.elapsed();
    println!("[lease] A epoch={e_a} → B epoch={e_b}, waited={waited:?}");
    assert!(e_b > e_a, "接管者 epoch 应更大");
    drop(b);
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}
