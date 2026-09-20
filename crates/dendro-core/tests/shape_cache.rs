//! 形状缓存（literal 模板化自动 prepare）差分与边界
mod common;
use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};

fn conn(tag: &str) -> Connection {
    let dir = std::env::temp_dir().join(format!("dendro-shape-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Connection::open_with(&dir, DbOptions::embedded(StoreConfig::Memory)).unwrap()
}

#[test]
fn unique_text_queries_equivalent() {
    let mut c = conn("diff");
    c.execute("CREATE TABLE s (id BIGINT PRIMARY KEY, v TEXT, n BIGINT)")
        .unwrap();
    c.execute("INSERT INTO s VALUES (1,'a',10),(2,'b''q',20),(3,NULL,30)")
        .unwrap();
    // 同模板不同字面量：引号转义/数字/NULL 混合——结果与逐条等价
    for (id, expect) in [(1i64, "a"), (2, "b'q")] {
        let r = c
            .query(&format!(
                "SELECT v FROM s WHERE id = {id} AND n >= {}",
                id * 10
            ))
            .unwrap();
        assert!(
            format!("{:?}", r.rows).contains(expect),
            "id={id}: {:?}",
            r.rows
        );
    }
    // 字符串字面量含引号
    let r = c.query("SELECT id FROM s WHERE v = 'b''q'").unwrap();
    assert!(format!("{:?}", r.rows).contains("2"));
    // 数字在标识符内不得模板化破坏（表名列名含数字）
    c.execute("CREATE TABLE t2 (c1 BIGINT PRIMARY KEY)")
        .unwrap();
    c.execute("INSERT INTO t2 VALUES (5)").unwrap();
    let r = c.query("SELECT c1 FROM t2 WHERE c1 = 5").unwrap();
    assert!(format!("{:?}", r.rows).contains("5"), "{:?}", r.rows);
    // 重复执行同模板（缓存命中路径）稳定性
    for i in 0..100 {
        let r = c
            .query(&format!("SELECT c1 FROM t2 WHERE c1 = {}", 5))
            .unwrap();
        let _ = i;
        assert!(format!("{:?}", r.rows).contains("5"));
    }
    // 注释形态 fail-open（透传全量 parse 不炸）
    let r = c
        .query("SELECT c1 FROM t2 WHERE c1 = 5 -- trailing")
        .unwrap();
    assert!(format!("{:?}", r.rows).contains("5"));
}

#[test]
fn shape_cache_populates() {
    let mut c = conn("census");
    c.execute("CREATE TABLE z (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO z VALUES (1)").unwrap();
    let before = dendro_core::sql::shapecache::len();
    for i in 0..50 {
        let _ = c.query(&format!("SELECT id FROM z WHERE id = {i}"));
    }
    assert!(
        dendro_core::sql::shapecache::len() >= before + 1,
        "模板应入缓存：before={before} after={}",
        dendro_core::sql::shapecache::len()
    );
}
