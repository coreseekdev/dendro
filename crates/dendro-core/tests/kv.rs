//! KV 接口层测试（SPEC 11）：基本读写、范围、CAS、事务、分支隔离与合并可见性、持久化。
use dendro_core::kv::Kv;
use dendro_core::{Database, DbOptions, StoreConfig};

#[test]
fn kv_basic_and_scan() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut kv = Kv::open(&db, "main").unwrap();
    assert_eq!(kv.get("missing").unwrap(), None);
    kv.put("a", "1").unwrap();
    kv.put("b", "2").unwrap();
    kv.put("c", "3").unwrap();
    kv.delete("b").unwrap();
    assert_eq!(kv.get("a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(kv.get("b").unwrap(), None);
    let scan = kv.scan(Some(b"a"), Some(b"c")).unwrap();
    assert_eq!(scan.len(), 1);
    assert_eq!(scan[0], (b"a".to_vec(), b"1".to_vec()));
    let all = kv.scan(None, None).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn kv_cas_and_txn() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut kv = Kv::open(&db, "main").unwrap();
    // CAS：期望不存在 → 成功
    assert!(kv.cas("k", None, b"v1".as_slice()).unwrap());
    // CAS：期望旧值 → 成功；期望错误旧值 → false
    assert!(kv.cas("k", Some(b"v1"), b"v2").unwrap());
    assert!(!kv.cas("k", Some(b"v1"), b"x").unwrap());
    assert_eq!(kv.get("k").unwrap().as_deref(), Some(&b"v2"[..]));
    // 显式事务：原子性
    kv.begin().unwrap();
    kv.put("t1", "a").unwrap();
    kv.put("t2", "b").unwrap();
    kv.commit().unwrap();
    assert!(kv.get("t1").unwrap().is_some());
    kv.begin().unwrap();
    kv.put("t3", "c").unwrap();
    kv.rollback();
    assert_eq!(kv.get("t3").unwrap(), None);
}

#[test]
fn kv_branch_isolation_and_merge() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut main = Kv::open(&db, "main").unwrap();
    main.put("k", "base").unwrap();
    main.put("only-main", "m").unwrap();

    db.create_branch("feat", "main").unwrap();
    let mut feat = Kv::open(&db, "feat").unwrap();
    feat.put("feat-key", "fv").unwrap();
    feat.delete("only-main").unwrap();

    // 分支隔离
    assert_eq!(feat.get("feat-key").unwrap().as_deref(), Some(&b"fv"[..]));
    assert_eq!(main.get("feat-key").unwrap(), None);

    // 合并后 main 可见
    main.put("k2", "v2").unwrap();
    db.merge_branches("feat", "main").unwrap();
    assert_eq!(main.get("feat-key").unwrap().as_deref(), Some(&b"fv"[..]));
    assert_eq!(main.get("only-main").unwrap(), None);
}

#[test]
fn kv_persistence() {
    let dir = std::env::temp_dir().join(format!("dendro-kv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Database::open(DbOptions {
            store: StoreConfig::LocalDir(dir.clone()),
            durability: dendro_core::Durability::Group,
            wal_flush_interval_ms: 10,
            ..Default::default()
        })
        .unwrap();
        let mut kv = Kv::open(&db, "main").unwrap();
        kv.put("durable", "yes").unwrap();
    }
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir.clone()),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 10,
        ..Default::default()
    })
    .unwrap();
    let mut kv = Kv::open(&db, "main").unwrap();
    assert_eq!(kv.get("durable").unwrap().as_deref(), Some(&b"yes"[..]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kv_occ_conflict() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let mut k1 = Kv::open(&db, "main").unwrap();
    let mut k2 = Kv::open(&db, "main").unwrap();
    k1.put("hot", "v0").unwrap();
    k2.put("hot", "v0").unwrap();
    // 两个事务基于同一快照写同一 key：后提交者冲突
    k1.begin().unwrap();
    k1.put("hot", "A").unwrap();
    k2.begin().unwrap();
    k2.put("hot", "B").unwrap();
    k1.commit().unwrap();
    let err = k2.commit().unwrap_err();
    assert_eq!(err.state, "40001");
}
