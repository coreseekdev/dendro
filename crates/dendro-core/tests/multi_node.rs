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
        read_only: false,
        gc_retention_ms: 24 * 3600 * 1000,
    }
}

fn opts_ro(dir: &std::path::Path) -> DbOptions {
    DbOptions { read_only: true, ..opts(dir, 800) }
}

fn fence_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir.join("fence").join("main"))
        .map(|d| d.flatten().count())
        .unwrap_or(0)
}

#[test]
fn read_only_open_does_not_pollute_epoch_sequence() {
    // 评审 A7：此前任何打开都领新 epoch + 起 WAL writer——读副本每次打开
    // 都制造空 epoch 目录。现在：只读打开零 fence 对象、可读、拒写（25006）。
    let dir = std::env::temp_dir().join(format!("dendro-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // 写者建立数据（领取 epoch 1 → fence 文件 1 个）
    {
        let db = Database::open(opts(&dir, 800)).unwrap();
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a')").unwrap();
    }
    let before = fence_files(&dir);
    assert_eq!(before, 1, "写者打开应恰好一个租约对象");

    // 只读打开 ×2：不新增 fence 对象（epoch 序列不被污染）
    for i in 0..2 {
        let ro = Database::open(opts_ro(&dir)).unwrap();
        let b = ro.branch("main").unwrap();
        assert!(b.read_only);
        let o = {
            let mut s = ro.new_session();
            s.exec("SELECT count(*) FROM t").unwrap()
        };
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(rs.text_rows()[0][0].as_deref(), Some("1"), "RO 读可见（第 {} 次）", i + 1);
        }
        // 写被拒：SQLSTATE 25006 read_only_sql_transaction
        let e = {
            let mut s = ro.new_session();
            s.exec("INSERT INTO t VALUES (2, 'x')").unwrap_err()
        };
        assert_eq!(e.state, "25006", "只读分支必须拒写");
        // checkpoint 等写路径同样被拒
        assert!(ro.checkpoint_branch("main").is_err(), "只读分支拒绝 checkpoint");
        drop(ro);
    }
    assert_eq!(fence_files(&dir), 1, "只读打开不得产生租约对象");

    // 后续写者接管 epoch 仍连续（= 2，未被 RO 打开顶掉）
    let w2 = Database::open(opts(&dir, 800)).unwrap();
    let e2 = w2.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);
    assert_eq!(e2, 2, "epoch 序列不应被只读打开污染");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_only_open_missing_store_errors() {
    // 只读打开绝不创建对象：空存储 → 明确报错
    let dir = std::env::temp_dir().join(format!("dendro-ro2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let e = match Database::open(opts_ro(&dir)) {
        Ok(_) => panic!("只读打开空存储应报错"),
        Err(e) => e,
    };
    assert!(e.message.contains("read-only"), "{e}");
    assert!(!dir.join("manifest").exists(), "不得创建 manifest 对象");
    let _ = std::fs::remove_dir_all(&dir);
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

/// fence/ 前缀只允许前 N 次 put 成功（模拟"租约领取后 OSS 分区，续期全部失败"）
struct FenceFailAfterStore {
    inner: dendro_core::objstore::memory::MemoryObjStore,
    ok_left: std::sync::atomic::AtomicI32,
}
impl dendro_core::objstore::ObjStore for FenceFailAfterStore {
    fn get(&self, p: &str) -> dendro_core::objstore::ObjResult<bytes::Bytes> { self.inner.get(p) }
    fn get_range(&self, p: &str, o: u64, l: usize) -> dendro_core::objstore::ObjResult<bytes::Bytes> { self.inner.get_range(p, o, l) }
    fn put(&self, p: &str, d: bytes::Bytes) -> dendro_core::objstore::ObjResult<()> {
        if p.starts_with("fence/") {
            let prev = self.ok_left.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if prev <= 0 {
                return Err(dendro_core::objstore::ObjError::Io("fence unavailable (partitioned)".into()));
            }
        }
        self.inner.put(p, d)
    }
    fn put_if_absent(&self, p: &str, d: bytes::Bytes) -> dendro_core::objstore::ObjResult<()> {
        if p.starts_with("fence/") {
            let prev = self.ok_left.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if prev <= 0 {
                return Err(dendro_core::objstore::ObjError::Io("fence unavailable (partitioned)".into()));
            }
        }
        self.inner.put_if_absent(p, d)
    }
    fn delete(&self, p: &str) -> dendro_core::objstore::ObjResult<()> { self.inner.delete(p) }
    fn head(&self, p: &str) -> dendro_core::objstore::ObjResult<Option<dendro_core::objstore::HeadInfo>> { self.inner.head(p) }
    fn list_prefix(&self, p: &str) -> dendro_core::objstore::ObjResult<Vec<String>> { self.inner.list_prefix(p) }
    fn copy(&self, f: &str, t: &str) -> dendro_core::objstore::ObjResult<()> { self.inner.copy(f, t) }
}

#[test]
fn fence_expired_writer_rejected() {
    // 运行时拒写（fence_gate）：TTL=80ms。空闲保活（flush_loop 回调）会持续
    // 续期——健康写者不再因空闲而失约（自毒化已修）。真实失约场景 = 续期
    // PUT 持续失败（OSS 分区）：本测试从领取租约后立即切断 fence/ 写入，
    // 保活与惰性续期全部失败 → 租约到期 → 下一次提交被拒（40001）。
    let store = FenceFailAfterStore {
        inner: dendro_core::objstore::memory::MemoryObjStore::new(),
        ok_left: std::sync::atomic::AtomicI32::new(1), // 仅 acquire 成功
    };
    let obj: std::sync::Arc<dyn dendro_core::objstore::ObjStore> = std::sync::Arc::new(store);
    let opts = DbOptions {
        store: StoreConfig::Obj(obj),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 10,
        wal_segment_bytes: 4 << 20,
        checkpoint_threshold_bytes: u64::MAX,
        checkpoint_interval_s: 0,
        cache_budget_bytes: 256 << 20,
        lease_ttl_ms: 80,
        read_only: false,
        gc_retention_ms: 24 * 3600 * 1000,
    };
    let db = Database::open(opts).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'ok')").unwrap(); // TTL 内提交成功
    }
    // 超过 TTL：保活续期持续失败（分区）→ 租约过期
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
