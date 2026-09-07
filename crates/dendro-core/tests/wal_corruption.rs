#![allow(clippy::all)]
//! WAL 损坏与 flush 失败回归（评审 P0-1/P0-2 配套测试）：
//! - FrameIter 对坏 len / 截断 payload / 截断帧头：报错或有序终止，绝不 panic
//! - 打开路径对损坏段：返回 Err（fail-fast），不 panic
//! - flush PUT 失败注入：不丢帧、不挂死；恢复后帧随下一次 flush 全部 durable
//! 段格式见 SPEC 01 / wal.rs：24B 帧头（magic/ver/ty/seq/len/crc）+ payload + 32B 段尾。

use dendro_core::objstore::memory::MemoryObjStore;
use dendro_core::objstore::{ObjResult, ObjStore};
use dendro_core::wal::{encode_frame, FrameIter, FrameType, HEADER_LEN};
use dendro_core::{Database, DbOptions, StoreConfig};
use bytes::Bytes;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

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
    }
}

fn seed_db(dir: &std::path::Path) {
    let db = Database::open(opts_store(StoreConfig::LocalDir(dir.to_path_buf()))).unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    for i in 0..3 {
        s.exec(&format!("INSERT INTO t VALUES ({i}, 'v{i}')")).unwrap();
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
    for e in std::fs::read_dir(dir.join("wal").join("main")).unwrap().flatten() {
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
    assert!(matches!(it.next_frame(), Some(Err(_))), "截断 payload 应报错而非 panic/静默");
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
    assert!(it.next_frame().is_none(), "半帧头视为段尾（torn write 容忍）");
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
    let dir = std::env::temp_dir().join(format!("dendro-walc1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    seed_db(&dir);
    corrupt_first_frame_len(&dir);
    // len=u32::MAX 曾经会直接越界 panic；现在必须返回 Err
    let err = match Database::open(opts_store(StoreConfig::LocalDir(dir.clone()))) {
        Ok(_) => panic!("损坏段应导致 open 失败"),
        Err(e) => e,
    };
    assert!(err.message.contains("wal"), "错误应来自 WAL 解析：{err}");
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

/// put 前 N 次失败的包装层
struct FlakyPutStore {
    inner: MemoryObjStore,
    fail_puts_left: AtomicU32,
}

impl ObjStore for FlakyPutStore {
    fn get(&self, path: &str) -> ObjResult<Bytes> {
        self.inner.get(path)
    }
    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes> {
        self.inner.get_range(path, off, len)
    }
    fn put(&self, path: &str, data: Bytes) -> ObjResult<()> {
        // 仅在剩余预算 >0 时注入失败（fetch_sub 在 0 上会回绕污染后续所有 put）
        let mut cur = self.fail_puts_left.load(Ordering::SeqCst);
        loop {
            if cur == 0 {
                break;
            }
            match self.fail_puts_left.compare_exchange_weak(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Err(dendro_core::objstore::ObjError::Io("injected put failure".into())),
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
fn flush_put_failure_no_frame_loss_no_hang() {
    let obj = Arc::new(FlakyPutStore { inner: MemoryObjStore::new(), fail_puts_left: AtomicU32::new(0) });
    let db = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a')").unwrap(); // 段 1 正常落盘
    }
    // 持续注入 put 失败：Durability::Group 下 await_durable 一次超时后错误上抛
    // （有界延迟，不挂死——本测试完成本身即证明）；帧留在缓冲等待重试
    obj.fail_puts_left.store(5_000, Ordering::SeqCst);
    let err = {
        let mut s = db.new_session();
        match s.exec("INSERT INTO t VALUES (2, 'b')") {
            Ok(_) => panic!("持续故障期提交应失败"),
            Err(e) => e,
        }
    };
    assert!(err.message.contains("wal put"), "失败应来自 WAL put：{err}");
    // 故障恢复：失败帧仍在缓冲；下一次成功 flush 把它和新帧一并带 durable
    obj.fail_puts_left.store(0, Ordering::SeqCst);
    {
        let mut s = db.new_session();
        s.exec("INSERT INTO t VALUES (3, 'c')").unwrap();
    }
    drop(db);
    // 重开：故障期"提交失败"的行 2 与恢复后的行 3 都必须 durable（不丢帧）
    let db2 = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    let mut s = db2.new_session();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(rs.text_rows()[0][0].as_deref(), Some("3"), "失败帧必须在恢复后 durable（不丢帧）");
    }
}

#[test]
fn transient_flush_failure_self_heals() {
    // 单次瞬时故障：后台 flush 线程重试即可恢复，语句可透明成功（P0-1 设计语义）
    let obj = Arc::new(FlakyPutStore { inner: MemoryObjStore::new(), fail_puts_left: AtomicU32::new(0) });
    let db = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    }
    obj.fail_puts_left.store(1, Ordering::SeqCst);
    let b = db.branch("main").unwrap();
    let target;
    {
        let mut s = db.new_session();
        // 不论语句本身报错与否（后台重试可能先行消化故障），最终必须可读
        let _ = s.exec("INSERT INTO t VALUES (1, 'a')");
        target = b.snapshot(); // 该提交的 ts
    }
    // 等 durable 水位追上该提交（后台 flush 线程消化注入的故障，≤2s）
    let mut durable = false;
    for _ in 0..200 {
        if b.wal.durable_watermark() >= target {
            durable = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(durable, "瞬时故障必须被后台 flush 重试消化（durable 水位应推进）");
    drop(db);
    let db2 = Database::open(opts_store(StoreConfig::Obj(obj.clone()))).unwrap();
    let mut s = db2.new_session();
    let o = s.exec("SELECT count(*) FROM t").unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] {
        assert_eq!(rs.text_rows()[0][0].as_deref(), Some("1"), "重启后仍在");
    }
}
