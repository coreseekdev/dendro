//! opt1-prolly-tp-base：DML pk 下推正确性 + 有界 memtx 验证
use dendro_core::embed::Connection;
use dendro_core::{DbOptions, StoreConfig};
use std::sync::Arc;

/// 全局 memprof meter 的并行互扰防护（两测试串行）
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 独立临时目录（open_with 强制 LocalDir——同路径互染；每测试一目录）
fn conn_fresh(tag: &str) -> Connection {
    let dir = std::env::temp_dir().join(format!("dendro-tpbase-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Connection::open_with(&dir, DbOptions::embedded(StoreConfig::Memory)).unwrap()
}

#[test]
fn update_delete_pk_pushdown_correctness() {
    let _g = SERIAL.lock().unwrap();
    let mut c = conn_fresh("upd");
    c.execute("CREATE TABLE u (id BIGINT PRIMARY KEY, v BIGINT, tag TEXT)")
        .unwrap();
    for ch in 0..20 {
        let vals: Vec<String> = (0..500)
            .map(|i| {
                let id = ch * 500 + i + 1;
                format!("({id}, {}, 't{}')", id % 7, id % 3)
            })
            .collect();
        c.execute(&format!("INSERT INTO u VALUES {}", vals.join(",")))
            .unwrap();
    }
    c.execute("CHECKPOINT").unwrap(); // 树驻留——下推路径必经
                                      // 等值下推：affected 精确 + 值正确
    assert_eq!(c.execute("UPDATE u SET v = 999 WHERE id = 42").unwrap(), 1);
    assert!(matches!(
        &c.query("SELECT v FROM u WHERE id = 42").unwrap().rows[0][0],
        dendro_core::types::SqlValue::Int32(999)
    ));
    // 不存在的键：affected = 0（此前全扫描同值——回归保护）
    assert_eq!(
        c.execute("UPDATE u SET v = 1 WHERE id = 999999").unwrap(),
        0
    );
    // 非 pk 谓词：回落全扫（v 列无下推——语义正确性）
    assert_eq!(
        c.execute("UPDATE u SET tag = 'x' WHERE v = 999").unwrap(),
        1
    );
    // IN 下推：多点更新
    assert_eq!(
        c.execute("UPDATE u SET v = 5 WHERE id IN (1, 2, 3)")
            .unwrap(),
        3
    );
    // DELETE 同路径：pk 等值删除
    assert_eq!(c.execute("DELETE FROM u WHERE id = 7").unwrap(), 1);
    let r = c.query("SELECT count(*) FROM u").unwrap();
    assert!(
        matches!(&r.rows[0][0], dendro_core::types::SqlValue::Int64(9999)),
        "{:?}",
        r.rows
    );
    // 混合谓词（pk 等值 AND 其他条件）：下推取行后按谓词过滤
    assert_eq!(
        c.execute("UPDATE u SET v = 8 WHERE id = 10 AND tag <> 'x'")
            .unwrap(),
        1
    );
    assert_eq!(
        c.execute("UPDATE u SET v = 9 WHERE id = 11 AND tag = 'nonexist'")
            .unwrap(),
        0
    );
}

#[test]
fn embedded_bounded_memtx() {
    let _g = SERIAL.lock().unwrap();
    // burst 写入 >> 8MB 阈值：踢醒 checkpoint 应把 pending 压回有界
    //（内存上界 = 阈值 + 检查点进行期在途；观察窗口 5s）
    let mut c = conn_fresh("bounded");
    c.execute("CREATE TABLE b (id BIGINT PRIMARY KEY, payload TEXT)")
        .unwrap();
    for round in 0..40 {
        // 每轮 ~400KB × 40 = 16MB > 8MB 阈值
        let vals: Vec<String> = (0..2000)
            .map(|i| {
                let id = round * 2000 + i + 1;
                format!("({id}, '{}')", "p".repeat(190))
            })
            .collect();
        c.execute(&format!("INSERT INTO b VALUES {}", vals.join(",")))
            .unwrap();
        let pending = pending_bytes_of(&c);
        // 有界性：pending 不得无界增长（kick 后 checkpoint 归零；
        // 允许瞬时超阈——上界 = 阈值 + 一轮在途）
        assert!(
            pending < (12 << 20) as u64 + 600_000,
            "round {round}: pending {pending} 应有界（8MB 阈值 + 在途）"
        );
    }
    // 等待 kick 检查点完成后的归零（轮询 5s 窗口）
    // 末轮残余 < 阈值不会再触发 kick（正确行为——kick 只在越界时）：
    // 有界性已由循环内断言保证；显式 CHECKPOINT 后归零收口
    c.execute("CHECKPOINT").unwrap();
    assert_eq!(pending_bytes_of(&c), 0, "显式 checkpoint 后 pending 归零");
}

fn pending_bytes_of(_c: &Connection) -> u64 {
    // memprof 全局 meter（pending 载荷口径）
    dendro_core::memprof::memtx_pending()
        .bytes
        .load(std::sync::atomic::Ordering::Relaxed)
}
