//! WAL 损坏与 flush 失败回归（评审 P0-1/P0-2 配套测试）：
//! - FrameIter 对坏 len / 截断 payload / 截断帧头：报错或有序终止，绝不 panic
//! - 打开路径对损坏段：返回 Err（fail-fast），不 panic
//! - flush PUT 失败注入：不丢帧、不挂死；恢复后帧随下一次 flush 全部 durable
//!
//! 段格式见 SPEC 01 / wal.rs：24B 帧头（magic/ver/ty/seq/len/crc）+ payload + 32B 段尾。

use bytes::Bytes;
use dendro_core::objstore::memory::MemoryObjStore;
use dendro_core::objstore::{ObjResult, ObjStore};
use dendro_core::wal::{encode_frame, FrameIter, FrameType, HEADER_LEN};
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn opts_store(store: StoreConfig) -> DbOptions {
    DbOptions {
        store,
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 10,
        wal_segment_bytes: 4 << 20,
        checkpoint_threshold_bytes: u64::MAX,
        checkpoint_interval_s: 0,
        cache_budget_bytes: 256 << 20,
        lease_ttl_ms: 60_000,
        read_only: false,
        gc_retention_ms: 24 * 3600 * 1000,
    }
}

fn seed_db(dir: &std::path::Path) {
    let db = Database::open(opts_store(StoreConfig::LocalDir(dir.to_path_buf()))).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    for i in 0..3 {
        s.exec(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"))
            .unwrap();
    }
    drop(s);
    drop(db); // 模拟崩溃：段已落盘
}

/// 把 {dir}/wal/main/e*/ 下第一个段的帧 0 的 len 字段改成 u32::MAX
fn corrupt_first_frame_len(dir: &std::path::Path) -> usize {
    let seg = find_first_segment(dir);
    let mut data = std::fs::read(&seg).unwrap();
    assert!(data.len() > HEADER_LEN);
    data[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&seg, &data).unwrap();
    data.len()
}

fn find_first_segment(dir: &std::path::Path) -> std::path::PathBuf {
    let mut hit = None;
    for e in std::fs::read_dir(dir.join("wal").join("main"))
        .unwrap()
        .flatten()
    {
        for f in std::fs::read_dir(e.path()).unwrap().flatten() {
            if f.path().extension().is_some_and(|x| x == "wal") {
                hit = Some(f.path());
                break;
            }
        }
    }
    hit.expect("no wal segment written")
}

// ── FrameIter 单元级：不可信输入永不 panic ──

#[test]
fn frame_iter_bad_len_is_error_not_panic() {
    let frame = encode_frame(FrameType::Txn, 1, b"hello-wal-payload");
    let mut seg = frame.clone();
    seg[16..20].copy_from_slice(&u32::MAX.to_le_bytes()); // len 不可信
    let mut it = FrameIter::new(&seg);
    match it.next_frame() {
        Some(Err(e)) => assert!(e.message.contains("exceeds"), "{e}"),
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn frame_iter_bad_crc_is_error() {
    let mut frame = encode_frame(FrameType::Txn, 1, b"payload-for-crc-check");
    let last = frame.len() - 1;
    frame[last] ^= 0xFF; // 翻转 payload 尾字节 → CRC 失配
    let mut it = FrameIter::new(&frame);
    assert!(matches!(it.next_frame(), Some(Err(_))), "CRC 损坏必须报错");
}

#[test]
fn frame_iter_truncated_payload_is_error() {
    // 200B payload：截 40B 后仍剩 184B（> 32B 段尾容差）→ 必须报错而非静默
    let payload = vec![b'x'; 200];
    let frame = encode_frame(FrameType::Txn, 1, &payload);
    let cut = &frame[..frame.len() - 40];
    let mut it = FrameIter::new(cut);
    assert!(
        matches!(it.next_frame(), Some(Err(_))),
        "截断 payload 应报错而非 panic/静默"
    );
}

#[test]
fn frame_iter_torn_tail_tolerated() {
    // 尾部 ≤ 32B（不足段尾）视为撕裂写容忍 → None（torn write 语义，SPEC 01 §4）。
    // 引擎真实帧均 > 32B（帧头 24B + 非空 payload），不会被此容差吞掉。
    let frame = encode_frame(FrameType::Txn, 1, b"0123456789");
    let cut = &frame[..frame.len() - 4]; // 剩 30B
    let mut it = FrameIter::new(cut);
    assert!(it.next_frame().is_none(), "≤32B 残尾按撕裂写容忍");
}

#[test]
fn frame_iter_truncated_header_ends_cleanly() {
    let frame = encode_frame(FrameType::Txn, 1, b"xyz");
    let cut = &frame[..10]; // 帧头不完整（< 24B）
    let mut it = FrameIter::new(cut);
    assert!(
        it.next_frame().is_none(),
        "半帧头视为段尾（torn write 容忍）"
    );
}

#[test]
fn frame_iter_trailer_not_decoded_as_frame() {
    // 正常段 = 帧 + 32B 段尾；迭代器必须停在帧边界，不把段尾当帧
    let frame = encode_frame(FrameType::Txn, 7, b"aaa");
    let mut seg = frame;
    seg.extend_from_slice(&[0u8; 32]);
    let mut it = FrameIter::new(&seg);
    let (ty, seq, payload) = it.next_frame().unwrap().unwrap();
    assert_eq!((ty, seq, payload), (FrameType::Txn, 7, &b"aaa"[..]));
    assert!(it.next_frame().is_none());
}

// ── e2e：打库路径对损坏段 fail-fast ──

#[test]
fn open_rejects_corrupted_wal_segment() {
    // **已封段**（优雅关闭 = trailer 落盘）的帧损坏 = 真实腐坏，恢复严格报错。
    // 追加模式合同：未封段（崩溃撕尾）走容忍路径，见下一个测试。
    let dir = std::env::temp_dir().join(format!("dendro-walc1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    seed_db_closed(&dir);
    corrupt_first_frame_len(&dir);
    // len=u32::MAX 曾经会直接越界 panic；现在必须返回 Err。
    // 惰性打开（S-1）后恢复回放发生在分支首次触达——损坏在触达时暴露。
    let db = match Database::open(opts_store(StoreConfig::LocalDir(dir.clone()))) {
        Ok(db) => db,
        Err(e) => {
            assert!(e.message.contains("wal"), "错误应来自 WAL 解析：{e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };
    let err = match db.branch("main") {
        Ok(_) => panic!("已封段的损坏应导致分支恢复失败"),
        Err(e) => e,
    };
    assert!(err.message.contains("wal"), "错误应来自 WAL 解析：{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// seed + **优雅关闭**：close_graceful 封段（trailer 落盘）→ 严格校验合同
fn seed_db_closed(dir: &std::path::Path) {
    let db = Database::open(opts_store(StoreConfig::LocalDir(dir.to_path_buf()))).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    for i in 0..3 {
        s.exec(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"))
            .unwrap();
    }
    drop(s);
    db.shutdown();
    drop(db);
}

#[test]
fn open_tolerates_unclosed_torn_tail() {
    // **未封段**（崩溃模拟：直接 drop，无 shutdown）的撕尾帧 = 追加中途
    // 崩溃 → 容忍，回放已 durable 前缀（追加模式 P2-6e 合同）
    let dir = std::env::temp_dir().join(format!("dendro-walc3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    seed_db(&dir); // 无 shutdown：段未封口
                   // 在段尾追加一个撕碎的帧（len 声称 1KB 但只有 4B payload）
    use dendro_core::wal::{encode_frame, FrameType};
    let seg = find_first_segment(&dir);
    let torn = encode_frame(FrameType::Txn, 99, &vec![b'x'; 1024]);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
        f.write_all(&torn[..24 + 4]).unwrap(); // 帧头 + 撕裂的 len 前缀
    }
    let db = Database::open(opts_store(StoreConfig::LocalDir(dir.clone()))).unwrap();
    let mut s = db.new_session();
    s.exec("USE BRANCH main").unwrap();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(
            rs.text_rows()[0][0].as_deref(),
            Some("3"),
            "已 durable 前缀必须完整回放（撕尾帧被容忍）"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_survives_truncated_wal_segment() {
    let dir = std::env::temp_dir().join(format!("dendro-walc2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    seed_db(&dir);
    let seg = find_first_segment(&dir);
    let len = std::fs::metadata(&seg).unwrap().len();
    let file = std::fs::OpenOptions::new().write(true).open(&seg).unwrap();
    file.set_len(len - 3).unwrap(); // 掉尾 3 字节（撕裂写模拟）
    drop(file);
    // 打开不 panic（panic 即测试失败）；行为：载荷截断 → Err，帧头截断 → 回放可用前缀
    let _opened = Database::open(opts_store(StoreConfig::LocalDir(dir.clone())));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── flush 失败注入：不丢帧、不挂死 ──

/// put 前 N 次失败的包装层（可限定路径前缀；空串 = 全部）
struct FlakyPutStore {
    inner: MemoryObjStore,
    fail_puts_left: AtomicU32,
    fail_prefix: String,
}

impl ObjStore for FlakyPutStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        self.inner.get(path)
    }
    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        self.inner.get_range(path, off, len)
    }
    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        // 仅在剩余预算 >0 且命中前缀时注入失败
        if !path.starts_with(&self.fail_prefix) {
            return self.inner.put(path, data);
        }
        let mut cur = self.fail_puts_left.load(Ordering::SeqCst);
        loop {
            if cur == 0 {
                break;
            }
            match self.fail_puts_left.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Err(dendro_core::objstore::ObjError::Io(
                        "injected put failure".into(),
                    ))
                }
                Err(x) => cur = x,
            }
        }
        self.inner.put(path, data)
    }
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.inner.put_if_absent(path, data)
    }
    fn delete(&self, path: &str) -> ObjResult<()> {
        self.inner.delete(path)
    }
    fn head(&self, path: &str) -> ObjResult<Option<dendro_core::objstore::HeadInfo>> {
        self.inner.head(path)
    }
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        self.inner.list_prefix(prefix)
    }
    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        self.inner.copy(from, to)
    }
}

#[test]
fn wal_failure_poisons_writer_until_reopen() {
    // P0-D 错误语义定案（毒化）：
    // - WAL PUT 失败 ⇒ 写者毒化：后续 append 一律 40003 拒绝（不得在结果
    //   未知的状态上叠加写）；flush 停止 ⇒ 确定性失败的帧绝不持久化，
    //   失败 = 未提交（进程内无痕 + 重启后无幽灵行，两个断言自此一致）；
    // - 唯一恢复路径 = reopen（新 writer + 恢复回放裁决真实状态）。
    let obj = Arc::new(FlakyPutStore {
        inner: MemoryObjStore::new(),
        fail_puts_left: AtomicU32::new(0),
        fail_prefix: String::new(),
    });
    let db = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a')").unwrap(); // 段 1 正常落盘
    }
    obj.fail_puts_left.store(5_000, Ordering::SeqCst);
    let err = {
        let mut s = db.new_session();
        match s.exec("INSERT INTO t VALUES (2, 'b')") {
            Ok(_) => panic!("持续故障期提交应失败"),
            Err(e) => e,
        }
    };
    assert_eq!(
        err.state, "40003",
        "WAL 失败应为 completion_unknown（毒化）：{err}"
    );
    // 进程内：失败的提交无痕（P2' install-after-durable）
    {
        let mut s = db.new_session();
        let o = s.exec("SELECT count(*) FROM t").unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(
                rs.text_rows()[0][0].as_deref(),
                Some("1"),
                "失败提交不得产生可见状态"
            );
        }
    }
    // 毒化后：故障恢复也拒绝新提交——不得叠加在未知状态上
    obj.fail_puts_left.store(0, Ordering::SeqCst);
    {
        let mut s = db.new_session();
        let e = match s.exec("INSERT INTO t VALUES (3, 'c')") {
            Ok(_) => panic!("毒化后提交应被拒"),
            Err(e) => e,
        };
        assert_eq!(e.state, "40003", "毒化写者拒绝一切提交：{e}");
    }
    drop(db);
    // reopen：新 writer 未毒化 → 可写；失败事务的帧从未持久化 → 无幽灵行
    let db2 = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db2.new_session();
        let o = s.exec("SELECT count(*) FROM t").unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            assert_eq!(
                rs.text_rows()[0][0].as_deref(),
                Some("1"),
                "失败事务重启后不得出现（无幽灵提交）"
            );
        }
        s.exec("INSERT INTO t VALUES (3, 'c')").unwrap();
    }
    drop(db2);
    let db3 = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    let mut s = db3.new_session();
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
fn transient_flush_failure_self_heals_via_reopen() {
    // P0-D 定案后无"透明自愈"：单次故障同样毒化（错误语义必须与故障持续
    // 时间无关）。自愈发生在 reopen 层：新 writer 未毒化，可继续写入。
    // fail_prefix="wal/"：keepalive 的 fence/ PUT 不消耗预算（确定性）
    let obj = Arc::new(FlakyPutStore {
        inner: MemoryObjStore::new(),
        fail_puts_left: AtomicU32::new(0),
        fail_prefix: "wal/".into(),
    });
    let db = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    // 注入仅限 wal/ 前缀（keepalive 的 fence/ PUT 不消耗预算 → 确定性）
    obj.fail_puts_left.store(1, Ordering::SeqCst);
    let err = {
        let mut s = db.new_session();
        match s.exec("INSERT INTO t VALUES (1, 'a')") {
            Ok(_) => panic!("故障期提交应失败（Group 必须等 durable）"),
            Err(e) => e,
        }
    };
    assert_eq!(err.state, "40003", "{err}");
    // 毒化即时生效：下一个提交立即被拒（而非等待超时）
    let e = {
        let mut s = db.new_session();
        match s.exec("INSERT INTO t VALUES (2, 'b')") {
            Ok(_) => panic!("毒化后提交应被拒"),
            Err(e) => e,
        }
    };
    assert_eq!(e.state, "40003");
    drop(db);
    // reopen 即自愈
    let db2 = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    let mut s = db2.new_session();
    s.exec("INSERT INTO t VALUES (1, 'a')").unwrap();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(
            rs.text_rows()[0][0].as_deref(),
            Some("1"),
            "reopen 后恢复正常"
        );
    }
}

// ── P0-A：并发 flush 同段号覆盖（第三轮评审探针固化为回归）──

/// 首次 put 指定前缀时阻塞一段时间的包装层（模拟 S3 慢 PUT）
struct SlowFirstPutStore {
    inner: MemoryObjStore,
    slow_prefix: &'static str,
    armed: std::sync::atomic::AtomicBool,
    delay: Duration,
}

impl ObjStore for SlowFirstPutStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        self.inner.get(path)
    }
    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        self.inner.get_range(path, off, len)
    }
    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        if path.starts_with(self.slow_prefix) && self.armed.swap(false, Ordering::SeqCst) {
            std::thread::sleep(self.delay);
        }
        self.inner.put(path, data)
    }
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()> {
        self.inner.put_if_absent(path, data)
    }
    fn delete(&self, path: &str) -> ObjResult<()> {
        self.inner.delete(path)
    }
    fn head(&self, path: &str) -> ObjResult<Option<dendro_core::objstore::HeadInfo>> {
        self.inner.head(path)
    }
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>> {
        self.inner.list_prefix(prefix)
    }
    fn copy(&self, from: &str, to: &str) -> ObjResult<()> {
        self.inner.copy(from, to)
    }
}

#[test]
fn concurrent_flush_same_seg_never_overwrites() {
    // 修复前（评审探针实证）：flush_loop 持段 N 缓冲慢速 PUT 期间，Always
    // append 以同段号 N 再 PUT → 同路径两个不同内容，最后写者胜 → 已 ack 帧
    // 丢失。修复后：flush_mu 单飞，Always 等待在途上传完成后以新段号落盘。
    use dendro_core::wal::{FrameType, WalConfig, WalWriter};
    let slow = Arc::new(SlowFirstPutStore {
        inner: MemoryObjStore::new(),
        slow_prefix: "wal/",
        armed: std::sync::atomic::AtomicBool::new(true),
        delay: Duration::from_millis(400),
    });
    let obj: Arc<dyn ObjStore> = slow.clone();
    let cfg = WalConfig {
        flush_interval: Duration::from_millis(5),
        segment_bytes: 4 << 20,
        durability: dendro_core::Durability::Group,
        keepalive: None,
    };
    let w = WalWriter::open(obj, "t1", 1, 1, cfg);
    // 帧 1 NoWait：5ms 内由 flush_loop 取走缓冲，进入 400ms 慢 PUT
    w.append(
        FrameType::Txn,
        1,
        b"frame-one",
        dendro_core::Durability::NoWait,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(60)); // 确保 flush_loop 已进入慢 PUT
                                                   // 帧 2 Always：修复前会与在途 PUT 同段号并发；修复后在 flush_mu 上等待
    let t0 = std::time::Instant::now();
    w.append(
        FrameType::Txn,
        2,
        b"frame-two",
        dendro_core::Durability::Always,
    )
    .unwrap();
    let elapsed = t0.elapsed();
    w.close();

    // 两帧都必须 durable 且分属不同段（同段覆盖 = 丢帧）
    let s1 = dendro_core::wal::read_segment(&(slow.clone() as Arc<dyn ObjStore>), "t1", 1, 1)
        .unwrap_or_default();
    let s2 = dendro_core::wal::read_segment(&(slow.clone() as Arc<dyn ObjStore>), "t1", 1, 2)
        .unwrap_or_default();
    let has = |seg: &[u8], pat: &[u8]| {
        seg.len() >= pat.len() && (0..=seg.len() - pat.len()).any(|i| &seg[i..i + pat.len()] == pat)
    };
    assert!(has(&s1, b"frame-one"), "段 1 必须含帧 1（不被覆盖）");
    assert!(has(&s2, b"frame-two"), "段 2 必须含帧 2（Always 已 ack）");
    assert!(
        elapsed >= Duration::from_millis(300),
        "Always 应等待在途上传完成（实际 {elapsed:?}）"
    );
}

// ── P0-B：checkpoint 中途失败不得丢已提交事务 ──

#[test]
fn checkpoint_failure_preserves_committed_data() {
    // 只对 cas/ 上传注入持续失败：树物化（put_batch）失败 → checkpoint 报错，
    // pending 必须被归还；随后故障恢复，checkpoint 成功，数据完整（进程内 + 重启）。
    let obj = Arc::new(FlakyPutStore {
        inner: MemoryObjStore::new(),
        fail_puts_left: AtomicU32::new(0),
        fail_prefix: "objects/".into(),
    });
    let db = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a')").unwrap(); // 已 ack（Group durable）
    }
    // 注入 cas/ 持续失败 → checkpoint 失败
    obj.fail_puts_left.store(10_000, Ordering::SeqCst);
    {
        let err = match db.checkpoint_branch("main") {
            Ok(_) => panic!("注入期 checkpoint 应失败"),
            Err(e) => e,
        };
        assert!(!err.message.is_empty(), "{err}");
    }
    // 失败后数据仍可读（pending 归还 + memtx 未动）
    let mut s = db.new_session();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(
            rs.text_rows()[0][0].as_deref(),
            Some("1"),
            "失败 checkpoint 不得丢已提交行"
        );
    }
    drop(s);
    // 故障恢复 → checkpoint 成功 → 重启后数据完整（归来的 pending 被物化）
    obj.fail_puts_left.store(0, Ordering::SeqCst);
    db.checkpoint_branch("main").unwrap();
    drop(db);
    let db2 = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    let mut s = db2.new_session();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(rs.text_rows()[0][0].as_deref(), Some("1"), "重启后仍完整");
    }
}

#[test]
fn close_graceful_flushes_no_wait_tail() {
    // Q-12：close_graceful 先上传剩余缓冲再停线程。
    // **NoWait 持久级**（第十六轮 R16-1：Group 语义下 exec 返回前已
    // await_durable，close 时缓冲必空——用 Group 测是空转回归）。
    // NoWait 的缓冲尾在优雅关闭下得以持久（进程死亡的丢失语义只属于
    // NoWait+崩溃；Group/Always 不受影响）。
    let obj = Arc::new(FlakyPutStore {
        inner: MemoryObjStore::new(),
        fail_puts_left: AtomicU32::new(0),
        fail_prefix: String::new(),
    });
    let mut o = opts_store(StoreConfig::Obj(obj.clone()));
    o.durability = dendro_core::Durability::NoWait;
    let db = Database::open(o).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        s.exec("INSERT INTO t VALUES (1, 'tail')").unwrap(); // NoWait 语义下假设缓冲未及上传
    }
    db.branch("main").unwrap().wal.close_graceful();
    drop(db);
    let db2 = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    let mut s = db2.new_session();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(
            rs.text_rows()[0][0].as_deref(),
            Some("1"),
            "优雅关闭必须持久化 NoWait 缓冲尾"
        );
    }
}

#[test]
fn concurrent_inflight_commits_fail_cleanly_on_poison() {
    // P2-6 两段式提交 × 毒化：durable 等待移出 commit_mu 后，多个并发提交
    // 同帧入组。注入 PUT 失败 → 写者毒化 → 所有等待者 40003（Uncertain
    // 口径），in-flight 注册表被 InflightGuard 清空（watermark 不停滞），
    // 毒化前已成功提交全部可见；reopen 后无幽灵行。
    let obj = Arc::new(FlakyPutStore {
        inner: MemoryObjStore::new(),
        fail_puts_left: AtomicU32::new(0),
        fail_prefix: String::new(),
    });
    let db = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        s.exec("INSERT INTO t VALUES (0)").unwrap(); // 段 1 正常
    }
    // 毒化 + 8 并发不同键提交：等待者要么成功（毒化前已落盘），要么 40003
    obj.fail_puts_left.store(1, Ordering::SeqCst);
    let results: Vec<Vec<Result<(), String>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (1..=8)
            .map(|i| {
                let db = db.clone();
                scope.spawn(move || {
                    let mut s = db.new_session();
                    match s.exec(&format!("INSERT INTO t VALUES ({i})")) {
                        Ok(_) => Ok(()),
                        Err(e) => Err(e.state.to_string()),
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| vec![h.join().unwrap()])
            .collect()
    });
    let flat: Vec<&Result<(), String>> = results.iter().flatten().collect();
    let oks = flat.iter().filter(|r| r.is_ok()).count();
    let errs = flat.iter().filter(|r| r.is_err()).count();
    assert_eq!(oks + errs, 8, "每线程恰一结果");
    // 成功者恰一次可见；失败者（40003）不得可见
    let visible: usize = {
        let mut s = db.new_session();
        match s.exec("SELECT count(*) FROM t").unwrap().last().unwrap() {
            dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap().parse().unwrap(),
            _ => panic!("expected rows"),
        }
    };
    assert_eq!(
        visible,
        1 + oks,
        "可见行 = 成功提交数（毒化提交无进程内痕迹）"
    );
    // reopen：重新领 epoch 后提交继续工作（毒化自愈路径不回归）
    drop(db);
    let db2 = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db2.new_session();
        s.exec("INSERT INTO t VALUES (100)").unwrap();
        let n = match s.exec("SELECT count(*) FROM t").unwrap().last().unwrap() {
            dendro_core::Output::Rows(rs) => rs.text_rows()[0][0]
                .clone()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            _ => panic!("expected rows"),
        };
        // 不确定域 = **失败 PUT 的整个段**（帧级而非行级）：FlakyPutStore 确定性
        // 失败 ⇒ 该段 0 帧落盘 ⇒ n = 1 + oks 精确成立；真实 S3 超时下该段 k 帧
        // 可能全部已落盘（Uncertain），n 上界 = 1 + oks + k ≤ 9（尝试总数）。
        // 断言口径：n ∈ [1+oks, 9]（SPEC 02 §3.5 按段 Uncertain）
        assert!(
            (1 + oks..=9).contains(&n),
            "reopen 后可见行数越界：n={n} oks={oks}"
        );
        assert!(n > oks, "成功提交必须在 reopen 后可见：n={n} oks={oks}");
    }
}

#[test]
fn segment_retirement_bounded_by_covered_frontier() {
    // 审计 R3-P0 回归：两段式提交下 durable-but-in-flight 帧可落在
    // checkpoint 帧之前的段里——段退休界必须按"段内最大帧 ts ≤ covered"，
    // 否则重启回放从 wal_first_seg 起跳过含在途帧的段 = 已 ack 提交丢失。
    // 本测试直接钉住 retire_bound 语义：含未覆盖帧的段不可退休。
    use dendro_core::recovery::composite_ts;
    use dendro_core::wal::{FrameType, WalConfig, WalWriter};

    let dir = std::env::temp_dir().join(format!("dendro-retire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let obj: Arc<dyn ObjStore> = Arc::new(MemoryObjStore::new());
    let epoch = 1u64;
    let cfg = WalConfig {
        flush_interval: std::time::Duration::from_millis(1),
        segment_bytes: 4 << 20,
        durability: dendro_core::Durability::NoWait,
        keepalive: None,
    };
    let w = WalWriter::open(obj, "retire", epoch, 1, cfg);
    // 帧 seq 1..=10 入队并刷盘 → 段 1 的 max_seq = 10
    for seq in 1..=10u64 {
        w.enqueue_only(FrameType::Txn, seq, b"x", false).unwrap();
    }
    w.flush_now().unwrap();
    // 全部覆盖（watermark = ts(10)）→ 段 1 可退休
    assert_eq!(w.retire_bound(composite_ts(epoch, 10)), 1);
    // 仅覆盖到 seq 3（= 段内含在途/未安装帧 4..10，模拟两段式 in-flight 窗口）
    // → 段 1 的 max_seq=10 超 covered ⇒ **不可退休**（返回 0）
    assert_eq!(w.retire_bound(composite_ts(epoch, 3)), 0);
    // 空记账（无已刷段）→ 0
    let w2 = WalWriter::open(
        Arc::new(MemoryObjStore::new()),
        "retire2",
        epoch,
        1,
        WalConfig {
            flush_interval: std::time::Duration::from_millis(1),
            segment_bytes: 4 << 20,
            durability: dendro_core::Durability::NoWait,
            keepalive: None,
        },
    );
    assert_eq!(w2.retire_bound(composite_ts(epoch, 100)), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn poisoned_writer_flush_thread_does_not_hot_spin() {
    // 审计 R4-F1 回归：毒化后帧永久滞留缓冲（pending_frames > 0 恒真），
    // 事件驱动刷盘线程若不挂起即 100% CPU 热旋到 reopen。断言：毒化后的
    // 300ms 闲置窗口内进程 CPU 时间增量 < 100ms（热旋 ≈ 300ms；挂起 ≈ 0）。
    let obj = Arc::new(FlakyPutStore {
        inner: MemoryObjStore::new(),
        fail_puts_left: AtomicU32::new(0),
        fail_prefix: String::new(),
    });
    let db = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    }
    obj.fail_puts_left.store(u32::MAX, Ordering::SeqCst);
    {
        let mut s = db.new_session();
        let e = s.exec("INSERT INTO t VALUES (1)").unwrap_err();
        assert_eq!(e.state, "40003", "{e}");
    }
    // 停止注入但**不 reopen**：写者仍毒化（挂起路径），无任何提交活动
    obj.fail_puts_left.store(0, Ordering::SeqCst);
    let cpu = || -> u64 {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let after = stat.rsplit(')').next().unwrap().trim_start();
        let f: Vec<&str> = after.split_whitespace().collect();
        // after 去掉 comm 后：state 是字段 3 ⇒ utime/stime 是第 12/13 个（0-based）
        let utime: u64 = f[11].parse().unwrap();
        let stime: u64 = f[12].parse().unwrap();
        utime + stime
    };
    let c0 = cpu();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let c1 = cpu();
    // 时钟 tick 通常 100Hz：300ms 窗口内热旋 ≈ 30 tick；挂起 ≈ 0-3 tick
    assert!(
        c1.saturating_sub(c0) < 10,
        "毒化写者刷盘线程热旋：300ms 窗口消耗 {} tick",
        c1 - c0
    );
}

#[test]
fn append_mode_batches_segments_by_size() {
    // P2-6e 对象数收益：追加模式按 segment_bytes 封段——50 次 Group 提交
    // （每帧 ~100B）只应产生极少的段对象；旧整段模式下 = 每 flush 一段
    // （事件驱动下 ≈ 50 段）。
    let dir = std::env::temp_dir().join(format!("dendro-append-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir.clone()),
        durability: dendro_core::Durability::Group,
        ..opts_store(StoreConfig::LocalDir(dir.clone()))
    })
    .unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        for i in 0..50 {
            s.exec(&format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
    }
    db.shutdown();
    let wal_dir = {
        let mut best = None;
        for e in std::fs::read_dir(dir.join("wal")).unwrap().flatten() {
            best = Some(e.path());
        }
        best.unwrap()
    };
    // 递归数段对象
    let mut segs = 0usize;
    for e in walk(&wal_dir) {
        if e.extension().map(|x| x == "wal").unwrap_or(false) {
            segs += 1;
        }
    }
    assert!(
        segs <= 4,
        "追加模式段数应远小于 flush 数（50）：实际 {segs}"
    );
    // 全部提交可见
    drop(db);
    let db = Database::open(opts_store(StoreConfig::LocalDir(dir.clone()))).unwrap();
    let mut s = db.new_session();
    s.exec("USE BRANCH main").unwrap();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(rs.text_rows()[0][0].as_deref(), Some("50"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn walk(p: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if p.is_dir() {
        for e in std::fs::read_dir(p).unwrap().flatten() {
            let ep = e.path();
            if ep.is_dir() {
                out.extend(walk(&ep));
            } else {
                out.push(ep);
            }
        }
    }
    out
}
