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
    DbOptions {
        read_only: true,
        ..opts(dir, 800)
    }
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
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
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
            assert_eq!(
                rs.text_rows()[0][0].as_deref(),
                Some("1"),
                "RO 读可见（第 {} 次）",
                i + 1
            );
        }
        // 写被拒：SQLSTATE 25006 read_only_sql_transaction
        let e = {
            let mut s = ro.new_session();
            s.exec("INSERT INTO t VALUES (2, 'x')").unwrap_err()
        };
        assert_eq!(e.state, "25006", "只读分支必须拒写");
        // checkpoint 等写路径同样被拒
        assert!(
            ro.checkpoint_branch("main").is_err(),
            "只读分支拒绝 checkpoint"
        );
        drop(ro);
    }
    assert_eq!(fence_files(&dir), 1, "只读打开不得产生租约对象");

    // 后续写者接管 epoch 仍连续（= 2，未被 RO 打开顶掉）
    let w2 = Database::open(opts(&dir, 800)).unwrap();
    let e2 = w2
        .branch("main")
        .unwrap()
        .lease_epoch
        .load(std::sync::atomic::Ordering::Acquire);
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
    let e_a = a
        .branch("main")
        .unwrap()
        .lease_epoch
        .load(std::sync::atomic::Ordering::Acquire);
    {
        let mut s = a.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a1')").unwrap();
        s.exec("INSERT INTO t VALUES (2, 'a2')").unwrap();
    }
    drop(a); // 模拟崩溃（无优雅关闭）

    // ── 实例 B：A 掉线后打开 → 领取 E1+1 ──
    let t_open = Instant::now();
    let b = Database::open(opts(&dir, 800)).unwrap();
    let e_b = b
        .branch("main")
        .unwrap()
        .lease_epoch
        .load(std::sync::atomic::Ordering::Acquire);
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
    let e_a = a
        .branch("main")
        .unwrap()
        .lease_epoch
        .load(std::sync::atomic::Ordering::Acquire);

    let b = Database::open(opts(&dir, 600)).unwrap();
    let e_b = b
        .branch("main")
        .unwrap()
        .lease_epoch
        .load(std::sync::atomic::Ordering::Acquire);
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
    fn get(&self, p: &str) -> dendro_core::objstore::ObjResult<bytes::Bytes> {
        self.inner.get(p)
    }
    fn get_range(
        &self,
        p: &str,
        o: u64,
        l: usize,
    ) -> dendro_core::objstore::ObjResult<bytes::Bytes> {
        self.inner.get_range(p, o, l)
    }
    fn put(&self, p: &str, d: bytes::Bytes) -> dendro_core::objstore::ObjResult<()> {
        if p.starts_with("fence/") {
            let prev = self
                .ok_left
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if prev <= 0 {
                return Err(dendro_core::objstore::ObjError::Io(
                    "fence unavailable (partitioned)".into(),
                ));
            }
        }
        self.inner.put(p, d)
    }
    fn put_if_absent(&self, p: &str, d: bytes::Bytes) -> dendro_core::objstore::ObjResult<()> {
        if p.starts_with("fence/") {
            let prev = self
                .ok_left
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if prev <= 0 {
                return Err(dendro_core::objstore::ObjError::Io(
                    "fence unavailable (partitioned)".into(),
                ));
            }
        }
        self.inner.put_if_absent(p, d)
    }
    fn delete(&self, p: &str) -> dendro_core::objstore::ObjResult<()> {
        self.inner.delete(p)
    }
    fn head(
        &self,
        p: &str,
    ) -> dendro_core::objstore::ObjResult<Option<dendro_core::objstore::HeadInfo>> {
        self.inner.head(p)
    }
    fn list_prefix(&self, p: &str) -> dendro_core::objstore::ObjResult<Vec<String>> {
        self.inner.list_prefix(p)
    }
    fn copy(&self, f: &str, t: &str) -> dendro_core::objstore::ObjResult<()> {
        self.inner.copy(f, t)
    }
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
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        s.exec("INSERT INTO t VALUES (1, 'ok')").unwrap(); // TTL 内提交成功
    }
    // 超过 TTL：保活续期持续失败（分区）→ 租约过期
    std::thread::sleep(Duration::from_millis(150));
    let err = {
        let mut s = db.new_session();
        s.exec("INSERT INTO t VALUES (2, 'rejected')").unwrap_err()
    };
    assert_eq!(
        err.state, "40001",
        "过期租约的提交应被拒（serialization/fencing）"
    );
    assert!(
        err.message.contains("fencing"),
        "错误信息应说明 fencing 原因：{err}"
    );

    // 旧会话的读路径不受影响（拒写不拒读）
    {
        let mut s = db.new_session();
        let o = s.exec("SELECT count(*) FROM t").unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(
                rs.text_rows()[0][0].as_deref(),
                Some("1"),
                "过期写者的行未提交"
            );
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
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    for i in 0..10 {
        std::thread::sleep(Duration::from_millis(30));
        let mut s = db.new_session();
        s.exec(&format!("INSERT INTO t VALUES ({i}, 'alive')"))
            .unwrap();
    }
    {
        let mut s = db.new_session();
        let o = s.exec("SELECT count(*) FROM t").unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(
                rs.text_rows()[0][0].as_deref(),
                Some("10"),
                "惰性续期下健康写者不被误拒"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn idle_writer_stays_writable() {
    // ⚠ flush_loop 的保活调用曾被误删（第六轮 P0 回归：空闲超过 TTL 即永久
    // 40001，且无任何测试拦截——196 全绿放行了回归）。本测试是它的防线：
    // 空闲窗口（2.5×TTL，期间零提交）后必须仍可写。
    let dir = std::env::temp_dir().join(format!("dendro-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(opts(&dir, 200)).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("INSERT INTO t VALUES (1)").unwrap();
    }
    std::thread::sleep(Duration::from_millis(500)); // 空闲 > TTL：保活必须兜住
    {
        let mut s = db.new_session();
        s.exec("INSERT INTO t VALUES (2)")
            .expect("空闲超过 TTL 的写者必须仍可写（保活职责）");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reopen_branch_sql_recovers_poisoned_writer() {
    // REOPEN BRANCH：毒化写者的 SQL 级恢复入口（第六轮 P1：reopen_branch
    // 曾零调用方）。注入持续 WAL 故障 → 毒化 → 提交 40003 → REOPEN BRANCH
    // → 新 writer 可写。
    use std::sync::atomic::{AtomicU32, Ordering};
    struct WalFailStore {
        inner: dendro_core::objstore::memory::MemoryObjStore,
        fail_wal: AtomicU32,
    }
    impl dendro_core::objstore::ObjStore for WalFailStore {
        fn get(&self, p: &str) -> dendro_core::objstore::ObjResult<bytes::Bytes> {
            self.inner.get(p)
        }
        fn get_range(
            &self,
            p: &str,
            o: u64,
            l: usize,
        ) -> dendro_core::objstore::ObjResult<bytes::Bytes> {
            self.inner.get_range(p, o, l)
        }
        fn put(&self, p: &str, d: bytes::Bytes) -> dendro_core::objstore::ObjResult<()> {
            if p.starts_with("wal/") {
                // CAS 消耗预算（fetch_sub 在 0 上会回绕为 u32::MAX 污染后续 put）
                let mut cur = self.fail_wal.load(Ordering::SeqCst);
                loop {
                    if cur == 0 {
                        break;
                    }
                    match self.fail_wal.compare_exchange_weak(
                        cur,
                        cur - 1,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => {
                            return Err(dendro_core::objstore::ObjError::Io("wal down".into()))
                        }
                        Err(x) => cur = x,
                    }
                }
            }
            self.inner.put(p, d)
        }
        fn put_if_absent(&self, p: &str, d: bytes::Bytes) -> dendro_core::objstore::ObjResult<()> {
            self.inner.put_if_absent(p, d)
        }
        fn delete(&self, p: &str) -> dendro_core::objstore::ObjResult<()> {
            self.inner.delete(p)
        }
        fn head(
            &self,
            p: &str,
        ) -> dendro_core::objstore::ObjResult<Option<dendro_core::objstore::HeadInfo>> {
            self.inner.head(p)
        }
        fn list_prefix(&self, p: &str) -> dendro_core::objstore::ObjResult<Vec<String>> {
            self.inner.list_prefix(p)
        }
        fn copy(&self, f: &str, t: &str) -> dendro_core::objstore::ObjResult<()> {
            self.inner.copy(f, t)
        }
    }
    let store = std::sync::Arc::new(WalFailStore {
        inner: dendro_core::objstore::memory::MemoryObjStore::new(),
        fail_wal: AtomicU32::new(0),
    });
    let obj: std::sync::Arc<dyn dendro_core::objstore::ObjStore> = store.clone();
    let db = Database::open(DbOptions {
        store: StoreConfig::Obj(obj.clone()),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 5,
        ..DbOptions::default()
    })
    .unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("INSERT INTO t VALUES (1)").unwrap();
    }
    // 注入 WAL 故障 → 毒化
    store.fail_wal.store(5_000, Ordering::SeqCst);
    {
        let mut s = db.new_session();
        let e = s.exec("INSERT INTO t VALUES (2)").unwrap_err();
        assert_eq!(e.state, "40003", "{e}");
    }
    // REOPEN BRANCH：SQL 级恢复（故障仍持续也不阻碍 reopen——新 writer 先毒
    // 化前可写第一笔；此处注入保持，reopen 后首笔会再毒化，先清故障验证恢复）
    store.fail_wal.store(0, Ordering::SeqCst);
    {
        let mut s = db.new_session();
        s.exec("REOPEN BRANCH main").unwrap();
        s.exec("INSERT INTO t VALUES (2)").unwrap();
    }
    // 旧会话视角新 writer 可见
    let mut s = db.new_session();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(
            rs.text_rows()[0][0].as_deref(),
            Some("2"),
            "reopen 后恢复写入"
        );
    }
}

#[test]
fn lazy_open_and_drop_branch_gc() {
    // S-1：启动不全量打开分支（每分支一线程+租约+fence 写在万级分支下不可行）
    // S-2：DROP BRANCH 的私有对象（WAL 段/fence）墓碑化 → GC 回收（此前永久泄漏）
    let dir = std::env::temp_dir().join(format!("dendro-lazy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // 两个分支
    {
        let db = Database::open(opts(&dir, 300)).unwrap();
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("CREATE BRANCH b2 FROM main").unwrap();
        s.exec("INSERT INTO t VALUES (1)").unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    // 打开（惰性）：不触达则零驻留分支
    {
        let db = Database::open(opts(&dir, 300)).unwrap();
        assert!(db.active_branches().is_empty(), "启动不得预打开任何分支");
        let _ = db.branch("b2").unwrap(); // 只触达 b2
        assert_eq!(db.active_branches().len(), 1, "仅触达的分支驻留");
    }
    // DROP BRANCH b2：私有对象墓碑化 → 保留窗口后回收
    {
        let db = Database::open(opts(&dir, 300)).unwrap();
        let mut s = db.new_session();
        s.exec("DROP BRANCH b2").unwrap();
        assert!(
            std::fs::read_dir(dir.join("fence").join("b2")).is_ok(),
            "窗口内 fence 对象仍在"
        );
        std::thread::sleep(Duration::from_millis(350));
        let db = Database::open(DbOptions {
            store: StoreConfig::LocalDir(dir.clone()),
            durability: dendro_core::Durability::Group,
            wal_flush_interval_ms: 10,
            wal_segment_bytes: 4 << 20,
            checkpoint_threshold_bytes: u64::MAX,
            checkpoint_interval_s: 0,
            cache_budget_bytes: 256 << 20,
            lease_ttl_ms: 300,
            read_only: false,
            gc_retention_ms: 300,
        })
        .unwrap();
        {
            let man = std::fs::read_dir(dir.join("manifest"))
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .max()
                .unwrap();
            let text = std::fs::read_to_string(&man).unwrap();
            let tombs: Vec<&str> = text.split("\"path\"").skip(1).collect();
            println!("[dbg] tombstones in {}: {:?}", man.display(), tombs.len());
            println!(
                "[dbg] b2 tombs: {}",
                tombs.iter().filter(|t| t.contains("b2")).count()
            );
        }
        std::thread::sleep(Duration::from_millis(350));
        db.gc_sweep().unwrap(); // 幂等：open 时的 sweep 可能已回收
        assert!(
            std::fs::read_dir(dir.join("fence").join("b2"))
                .map(|d| d.count())
                .unwrap_or(0)
                == 0,
            "DROP 后 fence 对象应被回收（不再永久泄漏）"
        );
        assert!(
            std::fs::read_dir(dir.join("wal").join("b2"))
                .map(|d| d.count())
                .unwrap_or(0)
                == 0,
            "DROP 后 WAL 段应被回收"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
