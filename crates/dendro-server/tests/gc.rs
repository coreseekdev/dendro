//! GC 回归（P1-4 定案，docs/design/GC定案.md）：
//! - 墓碑登记与"停止引用"同一 manifest 原子发布；保留窗口内对象**绝不删除**
//!   （P0-3 崩溃窗口保证：旧 manifest 引用的段必须还在）
//! - 窗口过后 gc_sweep 删除：列存旧段 / WAL 旧 epoch 目录 / 当前 epoch 前缀段
//! - 旧 manifest 版本回收（保留最近 16），load_latest 兼容空洞
//! - 回收后重新打开：恢复路径跳过已删前缀/目录，数据完整

use dendro_columnar::integrate::CbfColumnar;
use dendro_core::objstore::memory::MemoryObjStore;
use dendro_core::objstore::ObjStore;
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;
use std::time::Duration;

fn opts(obj: Arc<dyn ObjStore>, retention_ms: i64) -> DbOptions {
    DbOptions {
        store: StoreConfig::Obj(obj),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 10,
        wal_segment_bytes: 4 << 20,
        checkpoint_threshold_bytes: u64::MAX,
        checkpoint_interval_s: 0,
        cache_budget_bytes: 256 << 20,
        lease_ttl_ms: 60_000,
        read_only: false,
        gc_retention_ms: retention_ms,
    }
}

fn rows(db: &Arc<Database>, sql: &str) -> Vec<Vec<String>> {
    let mut s = db.new_session();
    let out = s.exec(sql).unwrap();
    out.iter()
        .filter_map(|o| match o {
            dendro_core::Output::Rows(rs) => Some(
                rs.text_rows()
                    .iter()
                    .map(|r| r.iter().map(|c| c.clone().unwrap_or_default()).collect::<Vec<String>>())
                    .collect::<Vec<Vec<String>>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn gc_columnar_segments_after_retention_window() {
    let mem = Arc::new(MemoryObjStore::new());
    let obj: Arc<dyn ObjStore> = mem.clone();
    let db = Database::open(opts(obj.clone(), 300)).unwrap();
    db.set_columnar(Arc::new(CbfColumnar { row_group_rows: 1_048_576 }));
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    }
    // 9 次 checkpoint（各 1 行增量）→ 第 9 次触发全量重建，替换前 8 个段
    for i in 0..9 {
        let mut s = db.new_session();
        s.exec(&format!("INSERT INTO t VALUES ({i}, 'v{i}')")).unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    let col_objs = obj.list_prefix("col/").unwrap();
    assert!(col_objs.len() >= 9, "至少 8 旧段 + 1 新段（实际 {}）", col_objs.len());
    let before = col_objs.len();

    // 墓碑已登记但窗口（300ms）未过：旧段必须原样存在（P0-3 崩溃窗口保证）
    let n = db.gc_sweep().unwrap();
    let after = obj.list_prefix("col/").unwrap().len();
    assert_eq!(after, before, "保留窗口内不得删除任何对象");

    // 窗口过后：到期墓碑被回收
    std::thread::sleep(Duration::from_millis(350));
    let n2 = db.gc_sweep().unwrap();
    assert!(n + n2 > 0, "到期墓碑应被删除");
    let final_objs = obj.list_prefix("col/").unwrap();
    assert_eq!(final_objs.len(), 1, "全量重建后应只剩 1 个段对象");

    // 回收后数据完整（列存投影与行存都可读）
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], "9");
}

#[test]
fn gc_wal_epochs_prefix_and_recovery() {
    let mem = Arc::new(MemoryObjStore::new());
    let obj: Arc<dyn ObjStore> = mem.clone();
    let wal_of = |epoch: u64| format!("wal/main/e{epoch:020}/");

    // 写者 A（epoch 1）
    {
        let db = Database::open(opts(obj.clone(), 300)).unwrap();
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a')").unwrap();
        db.checkpoint_branch("main").unwrap();
    } // drop = 崩溃模拟
    assert!(!obj.list_prefix(&wal_of(1)).unwrap().is_empty(), "epoch1 段应存在");

    // 写者 B（epoch 2）：首个 checkpoint 后，旧 epoch 目录全部 covered → 登记墓碑
    {
        let db = Database::open(opts(obj.clone(), 300)).unwrap();
        let e = db.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);
        assert_eq!(e, 2);
        let mut s = db.new_session();
        s.exec("INSERT INTO t VALUES (2, 'b')").unwrap();
        db.checkpoint_branch("main").unwrap(); // 登记旧 epoch 墓碑
        // 窗口内不删
        db.gc_sweep().unwrap();
        assert!(!obj.list_prefix(&wal_of(1)).unwrap().is_empty(), "窗口内旧 epoch 段不得删除");
        s.exec("INSERT INTO t VALUES (3, 'c')").unwrap();
    } // B 未 checkpoint 的 txn3 在 WAL 段中 durable（Group）
    std::thread::sleep(Duration::from_millis(350));

    // C 打开：open pass 删除到期墓碑；恢复必须跳过已删的 epoch1 目录且不丢 B 的数据
    let c = Database::open(opts(obj.clone(), 300)).unwrap();
    assert!(obj.list_prefix(&wal_of(1)).unwrap().is_empty(), "窗口后旧 epoch 目录应被回收");
    assert_eq!(rows(&c, "SELECT count(*) FROM t")[0][0], "3", "GC 后恢复数据完整");
    assert_eq!(rows(&c, "SELECT max(id) FROM t")[0][0], "3");

    // 当前 epoch 前缀段：INSERT 4（Group → 独立段1），checkpoint（ck 帧 → 段2）
    // → 前缀段1 登记墓碑、wal_first_seg 推进到 2；到期删除后恢复仍从段2 起
    {
        let mut s = c.new_session();
        s.exec("INSERT INTO t VALUES (4, 'd')").unwrap();
        c.checkpoint_branch("main").unwrap();
    }
    let e3 = c.branch("main").unwrap().lease_epoch.load(std::sync::atomic::Ordering::Acquire);
    let seg1_path = format!("wal/main/e{e3:020}/00000000000000000001.wal");
    assert!(obj.get(&seg1_path).is_ok(), "前缀段1 此时应存在");
    std::thread::sleep(Duration::from_millis(350));
    let n = c.gc_sweep().unwrap();
    assert!(n > 0, "前缀段应被回收");
    assert!(obj.get(&seg1_path).is_err(), "前缀段1 应已删除");
    drop(c);
    // 前缀空洞后的恢复：replay 从 wal_first_seg=2 起探测，数据必须完整
    let d = Database::open(opts(obj.clone(), 300)).unwrap();
    assert_eq!(rows(&d, "SELECT count(*), max(id) FROM t")[0][0], "4", "前缀回收后恢复完整");
}

#[test]
fn gc_manifest_versions_keep_recent() {
    let mem = Arc::new(MemoryObjStore::new());
    let obj: Arc<dyn ObjStore> = mem.clone();
    let db = Database::open(opts(obj.clone(), 300)).unwrap();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    }
    // 25 次 checkpoint → manifest 版本一路推进；每次 checkpoint 内嵌 gc_sweep
    // 将版本数压在"保留最近 16"的水位附近
    for i in 0..25 {
        let mut s = db.new_session();
        s.exec(&format!("INSERT INTO t VALUES ({i})")).unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    let count = |obj: &Arc<dyn ObjStore>| obj.list_prefix("manifest/").unwrap().len();
    let total = count(&obj);
    assert!(total <= 17, "manifest 版本应被持续回收至 ≤17（实际 {total}）");
    assert!(obj.get("manifest/00000000000000000001.json").is_err(), "最老版本应已删除");
    // 最新版本可读、数据完整
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], "25");
}
