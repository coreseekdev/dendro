//! AP 列存路径集成：CHECKPOINT 物化 → 列式扫描（zone map 剪枝）→ 聚合。
use dendro_core::{Database, DbOptions, StoreConfig};
use std::time::Instant;

#[test]
fn ap_materialize_and_scan() {
    let dir = std::env::temp_dir().join(format!("dendro-ap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir.clone()),
        durability: dendro_core::Durability::NoWait,
        wal_flush_interval_ms: 1,
        ..Default::default()
    })
    .unwrap();
    db.set_materializer(Arc::new(dendro_columnar::integrate::CbfMaterializer { row_group_rows: 4096 }));
    db.set_ap_scan(Arc::new(dendro_columnar::integrate::CbfApScan));
    let mut s = db.new_session();

    s.exec("CREATE TABLE sales (id BIGINT PRIMARY KEY, region TEXT, amount DOUBLE)").unwrap();
    // 30k 行（> AP 阈值 10k），分块插入
    let n = 30_000usize;
    let mut done = 0;
    while done < n {
        let end = (done + 2000).min(n);
        let mut sql = String::from("INSERT INTO sales VALUES ");
        for i in done..end {
            if i > done { sql.push(','); }
            let region = if i % 3 == 0 { "east" } else if i % 3 == 1 { "west" } else { "south" };
            sql.push_str(&format!("({i}, '{region}', {})", (i % 100) as f64 + 0.5));
        }
        s.exec(&sql).unwrap();
        done = end;
    }
    // 行路径聚合（物化前）
    let t0 = Instant::now();
    let out = s.exec("SELECT region, count(*), sum(amount) FROM sales GROUP BY region").unwrap();
    let row_path = t0.elapsed();
    let _ = out;

    // 物化
    s.exec("CHECKPOINT").unwrap();

    // 物化后再插入（WAL 尾部，列存投影之外）→ 查询结果必须仍然完整
    s.exec("INSERT INTO sales VALUES (99999, 'north', 1.0)").unwrap();

    // AP 路径聚合（物化后，行数 > 10k → 走 CBF + WAL overlay）
    let t1 = Instant::now();
    let out2 = s.exec("SELECT region, count(*) FROM sales GROUP BY region ORDER BY region").unwrap();
    let ap_path = t1.elapsed();
    println!("row_path={row_path:?} ap_path={ap_path:?}");
    if let dendro_core::Output::Rows(rs) = &out2[0] {
        for r in rs.text_rows() { println!("  {:?}", r); }
        assert!(rs.total_rows() >= 3, "至少 3 个 region 组");
    } else {
        panic!("expected rows");
    }
    // 总行数校验（CBF 30k + WAL 尾 1 行）
    let out3 = s.exec("SELECT count(*) FROM sales").unwrap();
    if let dendro_core::Output::Rows(rs) = &out3[0] {
        let c = rs.text_rows()[0][0].clone().unwrap();
        assert_eq!(c, "30001", "列存 + WAL 尾部合并正确");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
use std::sync::Arc;
