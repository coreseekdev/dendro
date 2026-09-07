#![allow(clippy::all)]
//! 真实 S3（RustFS/MinIO）端到端：存储主体在对象存储 + 杀进程恢复 + 网络字节审计。
//!
//! 运行（需先起容器并建桶）：
//! ```bash
//! docker run -d --name dendro-rustfs -p 19000:9000 \
//!   -e RUSTFS_ACCESS_KEY=dendrokey -e RUSTFS_SECRET_KEY=dendrosecret rustfs/rustfs:latest
//! aws --endpoint-url http://127.0.0.1:19000 s3 mb s3://dendro-test
//! DENDRO_S3=1 cargo test -p dendro-server --test s3_cloud -- --nocapture
//! ```

use dendro_core::objstore::{cached::CachedObjStore, ObjStore};
use dendro_core::objstore::s3::{S3Config, S3ObjStore};
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn s3_stack(prefix_tag: &str) -> (Arc<CachedObjStore>, Arc<S3ObjStore>, String) {
    let endpoint = std::env::var("DENDRO_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:19000".into());
    let bucket = std::env::var("DENDRO_S3_BUCKET").unwrap_or_else(|_| "dendro-test".into());
    let s3 = Arc::new(
        S3ObjStore::new(S3Config {
            endpoint,
            bucket: bucket.clone(),
            access_key: std::env::var("DENDRO_S3_KEY").unwrap_or_else(|_| "dendrokey".into()),
            secret_key: std::env::var("DENDRO_S3_SECRET").unwrap_or_else(|_| "dendrosecret".into()),
            region: "us-east-1".into(),
            max_retries: 4,
            retry_timeout_s: 30,
        })
        .unwrap(),
    );
    let cache_dir = std::env::temp_dir().join(format!("dendro-s3cache-{prefix_tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache_dir);
    let cached = Arc::new(CachedObjStore::new(s3.clone(), cache_dir, 512 << 20).unwrap());
    (cached, s3, bucket)
}

fn wipe(db: &dyn dendro_core::objstore::ObjStore) {
    for p in db.list_prefix("").unwrap_or_default() {
        if p.ends_with(".cbf") || p.ends_with(".wal") || p.ends_with(".json") || p.ends_with(".chunk") {
            let _ = db.delete(&p);
        }
    }
}

#[test]
fn s3_lifecycle_and_crash_recovery() {
    if std::env::var("DENDRO_S3").is_err() {
        eprintln!("SKIPPED (cloud test did NOT run): set DENDRO_S3=1 with a live S3 endpoint; exiting without executing");
        return;
    }
    let (cached, s3, _bucket) = s3_stack("lifecycle");
    wipe(cached.as_ref());

    let tag = format!("lc{}", std::process::id() % 100000);
    let opts = DbOptions {
        store: StoreConfig::Obj(cached.clone()),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 20,
        wal_segment_bytes: 8 << 20,
        checkpoint_threshold_bytes: u64::MAX,
        checkpoint_interval_s: 0,
        cache_budget_bytes: 512 << 20,
        lease_ttl_ms: 1500,
        read_only: false,
        gc_retention_ms: 24 * 3600 * 1000,
    };
    let db = Database::open(opts).unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar { row_group_rows: 4096 }));
    let mut s = db.new_session();

    // 1) DDL + 批量插入（跨 AP 阈值）
    s.exec(&format!("CREATE TABLE t_{tag} (id BIGINT PRIMARY KEY, region TEXT, amount DOUBLE)"))
        .unwrap();
    let n = 12_000usize;
    let mut done = 0;
    while done < n {
        let end = (done + 2000).min(n);
        let mut sql = format!("INSERT INTO t_{tag} VALUES ");
        for i in done..end {
            if i > done { sql.push(','); }
            let region = ["east", "west", "south"][i % 3];
            sql.push_str(&format!("({i}, '{region}', {})", (i % 500) as f64));
        }
        s.exec(&sql).unwrap();
        done = end;
    }
    // 2) CHECKPOINT：树物化 + 列存增量段上传到 S3
    s.exec("CHECKPOINT").unwrap();
    let col_objs = s3.list_prefix("col/").unwrap_or_default();
    assert!(!col_objs.is_empty(), "列存段应已上传到 S3");
    let bytes_after_ckpt = s3.stats().bytes_put.load(std::sync::atomic::Ordering::Relaxed);
    println!("[bytes] after checkpoint: put={}B across {} objects", bytes_after_ckpt, col_objs.len());

    // 3) checkpoint 后再写（仅 WAL）
    s.exec(&format!("INSERT INTO t_{tag} VALUES (99999, 'north', 1.5)")).unwrap();

    // 4) 模拟崩溃：直接 drop（无优雅关闭）
    drop(db);

    // 5) 从 S3 重新打开（新"计算节点"）
    let (cached2, _s3b, _) = s3_stack("lifecycle");
    let db2 = Database::open(DbOptions {
        store: StoreConfig::Obj(cached2.clone()),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: 20,
        wal_segment_bytes: 8 << 20,
        checkpoint_threshold_bytes: u64::MAX,
        checkpoint_interval_s: 0,
        cache_budget_bytes: 512 << 20,
        lease_ttl_ms: 1500,
        read_only: false,
        gc_retention_ms: 24 * 3600 * 1000,
    })
    .unwrap();
    db2.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar { row_group_rows: 4096 }));
    let mut s2 = db2.new_session();

    // 6) 全量校验：列存段 + WAL 尾部 = 12001 行（AP 路径，含 overlay 合并）
    let out = s2.exec(&format!("SELECT count(*) FROM t_{tag}")).unwrap();
    if let dendro_core::Output::Rows(rs) = &out[0] {
        let c = rs.text_rows()[0][0].clone().unwrap();
        assert_eq!(c, "12001", "列存段 + WAL 尾部应完整恢复");
    }
    // 7) AP 聚合
    let out = s2.exec(&format!("SELECT region, count(*) FROM t_{tag} GROUP BY region ORDER BY region")).unwrap();
    if let dendro_core::Output::Rows(rs) = &out[0] {
        assert!(rs.total_rows() >= 4);
    }
    // 8) 分支 + 合并（全部状态在 S3）
    s2.exec("CREATE BRANCH feat FROM main").unwrap();
    s2.exec("USE BRANCH feat").unwrap();
    s2.exec(&format!("INSERT INTO t_{tag} VALUES (88888, 'feat', 9.9)")).unwrap();
    s2.exec("USE BRANCH main").unwrap();
    for br in ["feat", "main"] {
        let o = s2.exec(&format!("SELECT commit, height FROM cambium.commit_log('{br}')")).unwrap();
        if let dendro_core::Output::Rows(rs) = &o[0] {
            println!("[diag] {br}: {:?}", rs.text_rows());
        }
    }
    let m = s2.exec(&format!("MERGE BRANCH feat INTO main"));
    match &m {
        Ok(o) => println!("[merge] ok: {:?}", o.iter().map(|x| match x { dendro_core::Output::Command{tag,..} => tag.clone(), _ => "rows".into() }).collect::<Vec<_>>()),
        Err(e) => println!("[merge] ERR: {} {}", e.state, e.message),
    }
    let _ = m.unwrap();
    let o = s2.exec(&format!("SELECT commit, height FROM cambium.commit_log('main')")).unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] { println!("[post-merge] main: {:?}", rs.text_rows()); }
    let o = s2.exec(&format!("SELECT count(*) FROM t_{tag} WHERE id = 88888")).unwrap();
    if let dendro_core::Output::Rows(rs) = &o[0] { println!("[post-merge] row88888: {:?}", rs.text_rows()); }
    let out = s2.exec(&format!("SELECT count(*) FROM t_{tag}")).unwrap();
    if let dendro_core::Output::Rows(rs) = &out[0] {
        assert_eq!(rs.text_rows()[0][0].clone().unwrap(), "12002");
    }
    // 9) DELETE → checkpoint → deletes 抑制
    s2.exec(&format!("DELETE FROM t_{tag} WHERE id = 99999")).unwrap();
    s2.exec("CHECKPOINT").unwrap();
    let out = s2.exec(&format!("SELECT count(*) FROM t_{tag}")).unwrap();
    if let dendro_core::Output::Rows(rs) = &out[0] {
        assert_eq!(rs.text_rows()[0][0].clone().unwrap(), "12001");
    }

    let gets = s3.stats().gets.load(std::sync::atomic::Ordering::Relaxed);
    let ranges = s3.stats().ranges.load(std::sync::atomic::Ordering::Relaxed);
    let puts = s3.stats().puts.load(std::sync::atomic::Ordering::Relaxed);
    let cond = s3.stats().conditional_puts.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "[net-audit] s3 gets={gets} ranges={ranges} puts={puts} cond_puts={cond} bytes_get={} bytes_put={}",
        s3.stats().bytes_get.load(std::sync::atomic::Ordering::Relaxed),
        s3.stats().bytes_put.load(std::sync::atomic::Ordering::Relaxed),
    );
}

#[test]
fn s3_manifest_conditional_put_conflict() {
    if std::env::var("DENDRO_S3").is_err() {
        eprintln!("SKIPPED (cloud test did NOT run): set DENDRO_S3=1");
        return;
    }
    // 独立桶：与 lifecycle 测试隔离（共享桶会互相污染 manifest 链）
    let endpoint = std::env::var("DENDRO_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:19000".into());
    let s3 = Arc::new(
        S3ObjStore::new(S3Config {
            bucket: "dendro-cond".into(),
            endpoint,
            access_key: std::env::var("DENDRO_S3_KEY").unwrap_or_else(|_| "dendrokey".into()),
            secret_key: std::env::var("DENDRO_S3_SECRET").unwrap_or_else(|_| "dendrosecret".into()),
            region: "us-east-1".into(),
            ..Default::default()
        })
        .unwrap(),
    );
    let cache_dir = std::env::temp_dir().join(format!("dendro-s3cache-cond-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache_dir);
    let cached = Arc::new(CachedObjStore::new(s3.clone(), cache_dir, 32 << 20).unwrap());
    // 直接对同一 manifest 版本双写：第二个必须 Exists（乐观提交基石）
    let ver = 10_000u64 + (std::process::id() as u64 % 500_000);
    let path = format!("manifest/{ver:020}.json");
    let m1 = format!("{{\"version\":{ver},\"writer_putid\":\"a\"}}").into_bytes();
    cached.put_if_absent(&path, m1.into()).unwrap();
    let m2 = format!("{{\"version\":{ver},\"writer_putid\":\"b\"}}").into_bytes();
    let r = cached.put_if_absent(&path, m2.into());
    assert!(matches!(r, Err(dendro_core::objstore::ObjError::Exists(_))));
}
